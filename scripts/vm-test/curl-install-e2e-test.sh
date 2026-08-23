#!/usr/bin/env bash
# End-to-end test of the REAL production install path: on a freshly
# reverted, source-free VM, run the exact one-liner a real user would
# (`curl -fsSL .../install.sh | sh`, fetching from the actual published
# GitHub repo — not a local rsync/file:// override like install-sh-test.sh
# uses), then dogfood a real package-manager install/launch/remove cycle
# through the resulting installed binary, to prove the whole pipeline
# (curl -> git clone -> cargo build -> pkgundo setup -> PM hooks -> real
# mutation capture) works end-to-end on each supported distro.
#
# Usage:
#   ./curl-install-e2e-test.sh arch
#   ./curl-install-e2e-test.sh debian
#   ./curl-install-e2e-test.sh fedora
#
# Run the matching ./setup-vm*.sh once before using this.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

DISTRO="${1:-}"
case "$DISTRO" in
    arch)
        export VM_NAME="${VM_NAME:-pkgundo-test}"
        export BASE_IMAGE_FILENAME="${BASE_IMAGE_FILENAME:-arch-base.qcow2}"
        ;;
    debian)
        export VM_NAME="${VM_NAME:-pkgundo-test-debian}"
        export BASE_IMAGE_FILENAME="${BASE_IMAGE_FILENAME:-debian-base.qcow2}"
        ;;
    fedora)
        export VM_NAME="${VM_NAME:-pkgundo-test-fedora}"
        export BASE_IMAGE_FILENAME="${BASE_IMAGE_FILENAME:-fedora-base.qcow2}"
        ;;
    *)
        echo "Usage: $0 <arch|debian|fedora>" >&2
        exit 1
        ;;
esac

source ./lib.sh
require_tools virsh ssh

INSTALL_URL="https://raw.githubusercontent.com/mrkushalsm/pkgundo/main/install.sh"
BIN="/usr/local/bin/pkgundo"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

if ! virsh dominfo "$VM_NAME" >/dev/null 2>&1; then
    echo "VM '$VM_NAME' doesn't exist yet. Run the matching setup-vm*.sh first." >&2
    exit 1
fi

echo "== [$DISTRO] Reverting VM to clean snapshot (genuinely source-free — proves this is the real curl path, not a local rsync) =="
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running

IP="$(vm_ip)"
for _ in $(seq 1 20); do
    [ -n "$IP" ] && break
    sleep 3
    IP="$(vm_ip)"
done
echo "VM IP: $IP"
wait_for_ssh "$IP"

if [ "$DISTRO" = fedora ]; then
    # dnf5's actions plugin isn't installed by default (a separate,
    # optional package — see README's "Supported today" section);
    # `pkgundo setup`/`install-hook` deliberately bail rather than
    # silently install it for the user, so a real Fedora user following
    # install.sh needs this one prerequisite first.
    echo "== [fedora] Installing the dnf5 actions-plugin prerequisite (documented, not auto-installed by pkgundo) =="
    ssh_vm "$IP" "sudo dnf install -y libdnf5-plugin-actions >/dev/null 2>&1; echo DONE"
fi

echo
echo "== [$DISTRO] [1] Running the real production one-liner: curl -fsSL $INSTALL_URL | sh =="
INSTALL_OUT="$(ssh_vm "$IP" "curl -fsSL '$INSTALL_URL' | sh" 2>&1)"
echo "$INSTALL_OUT" | tail -20
echo "$INSTALL_OUT" | grep -q "Done. 'pkgundo track <app>' to start watching something." \
    || fail "install.sh (via curl, from the real GitHub repo) did not report success"
echo "PASS: install.sh fetched from GitHub, built, and installed successfully via the real one-liner."

echo
echo "== [$DISTRO] [2] Binary, daemon, and PM hooks are all in place =="
ssh_vm "$IP" "test -x $BIN" || fail "expected $BIN to exist and be executable"
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon is not active after curl-install"
case "$DISTRO" in
    arch)   HOOK_CHECK="test -f /etc/pacman.d/hooks/99-pkgundo-tracked.hook" ;;
    debian) HOOK_CHECK="test -f /etc/apt/apt.conf.d/98pkgundo.conf" ;;
    fedora) HOOK_CHECK="test -f /etc/dnf/libdnf5-plugins/actions.d/98pkgundo.actions" ;;
esac
ssh_vm "$IP" "$HOOK_CHECK" || fail "expected the $DISTRO package-manager hook to be installed"
echo "PASS: binary installed, daemon active, $DISTRO hook installed — all via the curl-installed binary, no manual steps."

echo
echo "== [$DISTRO] [3] Real dev workflow through the curl-installed binary: install+use+remove npm, confirm auto-track + real mutation capture + removal reminder =="
if [ "$DISTRO" = arch ]; then
    # The base image's glibc predates whatever nodejs is current in the
    # repos at test time — a real Arch partial-upgrade pitfall (`pacman -S`
    # a single package without a full `-Syu` first), already discovered and
    # fixed the same way in pacman-hook-test.sh: node refuses to even start
    # ("GLIBC_2.44 not found"). A real Arch user keeps their system fully
    # upgraded, never does a partial one, so bringing the VM fully current
    # (and rebooting into the new kernel/glibc) first is the realistic fix.
    echo "  (bringing the VM fully current first — pacman's partial-upgrade pitfall, see pacman-hook-test.sh)"
    ssh_vm "$IP" "sudo pacman -Syu --noconfirm >/dev/null 2>&1; echo DONE"
    ssh_vm "$IP" "sudo reboot" || true
    sleep 15
    wait_for_ssh "$IP"
    ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active \
        || fail "daemon (systemd-enabled by the curl-installed setup) failed to come back up after the pacman -Syu reboot"
fi
case "$DISTRO" in
    arch)
        INSTALL_CMD="sudo pacman -S --noconfirm --needed nodejs npm"
        TRACK_GREP="^npm"
        REMOVE_CMD="sudo pacman -Rs --noconfirm nodejs npm"
        REMOVE_GREP="tracked apps were just removed"
        ;;
    debian)
        INSTALL_CMD="sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y nodejs npm"
        TRACK_GREP="^npm"
        REMOVE_CMD="sudo env DEBIAN_FRONTEND=noninteractive apt-get remove -y nodejs npm"
        REMOVE_GREP="tracked apps were just removed"
        ;;
    fedora)
        # No standalone npm package on Fedora — npm ships bundled as
        # nodejs-npm, a dependency-reason install, so nodejs itself is
        # the tracked app (see dnf-hook-test.sh's step 12 for the full
        # explanation of the /usr/bin/npm -> /usr/bin/node shebang
        # attribution this relies on).
        INSTALL_CMD="sudo dnf install -y nodejs"
        TRACK_GREP="^nodejs "
        REMOVE_CMD="sudo dnf remove -y nodejs"
        REMOVE_GREP="pkgundo was tracking removed package 'nodejs'"
        ;;
esac

ssh_vm "$IP" "$INSTALL_CMD >/dev/null 2>&1; echo DONE"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "$TRACK_GREP" || fail "expected npm/nodejs to be auto-tracked on explicit install via the curl-installed binary"
TXID="$(ssh_vm "$IP" "$BIN tracked" | grep "$TRACK_GREP" | grep -oP 'txid=\K[0-9]+' | head -1)"
[ -n "$TXID" ] || fail "could not resolve a txid for the tracked npm/nodejs app"
echo "PASS: auto-tracked on install (txid=$TXID)."

ssh_vm "$IP" "mkdir -p ~/.npm-global ~/myproj && npm config set prefix ~/.npm-global && npm install -g cowsay >/dev/null 2>&1 && cd ~/myproj && npm init -y >/dev/null 2>&1 && npm install lodash >/dev/null 2>&1; echo DONE"
sleep 2
MUTATIONS="$(ssh_vm "$IP" "$BIN inspect $TXID" | grep -oP 'Total mutations:\s*\K[0-9]+')"
[ "$MUTATIONS" -gt 0 ] || fail "expected non-zero mutations captured via the curl-installed daemon, got $MUTATIONS"
echo "PASS: real npm usage produced $MUTATIONS mutation(s), captured by the curl-installed daemon."

REMOVE_OUT="$(ssh_vm "$IP" "$REMOVE_CMD 2>&1")"
echo "$REMOVE_OUT" | grep -q "$REMOVE_GREP" || fail "expected a removal reminder from the curl-installed hook"
echo "PASS: removal reminder correctly printed by the curl-installed $DISTRO hook."

ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon health was affected by this test — it never should be"
echo "PASS: daemon health unaffected throughout."

echo
echo "== [$DISTRO] [4] pkgundo setup --remove cleanly tears everything down =="
ssh_vm "$IP" "sudo $BIN setup --remove"
ssh_vm "$IP" "! systemctl is-enabled pkgundo-daemon >/dev/null 2>&1" || fail "expected the daemon unit to be disabled after setup --remove"
ssh_vm "$IP" "! $HOOK_CHECK" || fail "expected the $DISTRO hook file to be gone after setup --remove"
echo "PASS: setup --remove cleanly disabled the daemon and removed the hook."

echo
echo "== [$DISTRO] Reverting VM back to clean snapshot (leaving it ready for next run) =="
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running >/dev/null

echo
echo "CURL-INSTALL E2E TEST ($DISTRO) PASSED"
