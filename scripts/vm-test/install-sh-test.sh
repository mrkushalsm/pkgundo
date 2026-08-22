#!/usr/bin/env bash
# Regression test for the daily-user one-shot install path: install.sh
# (fetch + build + install the binary) and `pkgundo setup`/`setup --remove`
# (install/enable/start the daemon's systemd unit, wire up the PM hooks via
# the existing install-hook logic, and reverse all of it).
#
# install.sh normally `git clone`s a real published repo; since this repo
# has no remote configured yet, this test rsyncs the working tree (WITH
# .git this time, unlike the other VM scripts) into the VM and points
# PKGUNDO_REPO_URL at that local path — a `git clone file://...` behaves
# identically to cloning a real remote for install.sh's purposes.
#
# Usage:
#   ./install-sh-test.sh
#
# Run ./setup-vm.sh once before using this.

cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

require_tools virsh ssh rsync

if ! virsh dominfo "$VM_NAME" >/dev/null 2>&1; then
    echo "VM '$VM_NAME' doesn't exist yet. Run ./setup-vm.sh first." >&2
    exit 1
fi

echo "== Reverting VM to clean snapshot =="
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running

IP="$(vm_ip)"
for _ in $(seq 1 20); do
    [ -n "$IP" ] && break
    sleep 3
    IP="$(vm_ip)"
done
echo "VM IP: $IP"
wait_for_ssh "$IP"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

echo
echo "== Copying pkgundo source (including .git, as a local clone source for install.sh) into the VM =="
rsync -az --delete -e "ssh -i $SSH_KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null" \
    --exclude target \
    "$REPO_ROOT/" "pkgundo@$IP:~/pkgundo-src/"

echo
echo "== [1] Running install.sh against the local clone source =="
INSTALL_OUT="$(ssh_vm "$IP" "PKGUNDO_REPO_URL=file:///home/pkgundo/pkgundo-src PKGUNDO_INSTALL_DIR=/usr/local/bin sh ~/pkgundo-src/install.sh 2>&1")"
echo "$INSTALL_OUT"
echo "$INSTALL_OUT" | grep -q "pkgundo is set up" || fail "install.sh did not report a completed setup"

echo
echo "== [2] Binary was installed to the expected location =="
ssh_vm "$IP" "test -x /usr/local/bin/pkgundo" || fail "binary was not installed to /usr/local/bin/pkgundo"
echo "PASS: binary installed at /usr/local/bin/pkgundo."

echo
echo "== [3] Daemon unit installed, enabled, and running with the correct ExecStart =="
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon did not reach active state"
ssh_vm "$IP" "systemctl is-enabled pkgundo-daemon" | grep -q enabled || fail "daemon unit was not enabled for boot"
ssh_vm "$IP" "grep -q 'ExecStart=/usr/local/bin/pkgundo daemon' /etc/systemd/system/pkgundo-daemon.service" \
    || fail "daemon unit's ExecStart was not patched to the installed binary path"
echo "PASS: daemon unit installed, enabled, running, correctly patched."

echo
echo "== [4] Package-manager hooks were installed as part of setup (pacman on this VM) =="
ssh_vm "$IP" "test -f /etc/pacman.d/hooks/98-pkgundo-track-on-install.hook" || fail "install hook missing after setup"
ssh_vm "$IP" "test -f /etc/pacman.d/hooks/99-pkgundo-tracked.hook" || fail "removal hook missing after setup"
echo "PASS: pacman hooks installed as part of setup."

echo
echo "== [5] End-to-end: an explicit install is auto-tracked using the installed binary =="
ssh_vm "$IP" "sudo pacman -S --noconfirm htop >/dev/null"
sleep 1
ssh_vm "$IP" "/usr/local/bin/pkgundo tracked" | grep -q "^htop" || fail "htop was not auto-tracked via the setup-installed binary/hooks"
echo "PASS: auto-tracking works end-to-end through the installed binary."

echo
echo "== [6] pkgundo setup --remove: daemon stopped/disabled/unit removed, hooks removed =="
ssh_vm "$IP" "sudo /usr/local/bin/pkgundo setup --remove"
set +e
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active
STILL_ACTIVE=$?
set -e
[ "$STILL_ACTIVE" -ne 0 ] || fail "daemon should not still be active after setup --remove"
ssh_vm "$IP" "test -f /etc/systemd/system/pkgundo-daemon.service" && fail "daemon unit file should be gone after setup --remove"
ssh_vm "$IP" "test -f /etc/pacman.d/hooks/98-pkgundo-track-on-install.hook" && fail "install hook should be gone after setup --remove"
ssh_vm "$IP" "test -f /etc/pacman.d/hooks/99-pkgundo-tracked.hook" && fail "removal hook should be gone after setup --remove"
echo "PASS: setup --remove cleanly tore down the daemon unit and both hooks."

echo
echo "== Reverting VM back to clean snapshot (leaving it ready for next run) =="
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running >/dev/null

echo
echo "INSTALL.SH / SETUP TEST PASSED"
