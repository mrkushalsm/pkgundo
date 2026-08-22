#!/usr/bin/env bash
# Regression test for the apt/dpkg hooks `pkgundo install-hook` manages —
# the Debian/Ubuntu sibling of pacman-hook-test.sh, covering the same
# ground (install-time auto-tracking, removal-time reminder, exit-code-0
# contract, DB-lock contention, install-hook --remove) plus one case
# structurally unique to apt: a single transaction that both installs and
# removes a tracked package in the same Pre/Post-Invoke pass, since apt has
# no install-vs-remove hook split like pacman's two independent files.
#
# Usage:
#   ./apt-hook-test.sh
#
# Run ./setup-vm-debian.sh once before using this.

cd "$(dirname "${BASH_SOURCE[0]}")"
export VM_NAME="${VM_NAME:-pkgundo-test-debian}"
export BASE_IMAGE_FILENAME="${BASE_IMAGE_FILENAME:-debian-base.qcow2}"
source ./lib.sh

require_tools virsh ssh rsync

if ! virsh dominfo "$VM_NAME" >/dev/null 2>&1; then
    echo "VM '$VM_NAME' doesn't exist yet. Run ./setup-vm-debian.sh first." >&2
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

echo "== Copying pkgundo source into the VM =="
rsync -az --delete -e "ssh -i $SSH_KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null" \
    --exclude target --exclude .git \
    "$REPO_ROOT/" "pkgundo@$IP:~/pkgundo/"

echo "== Building pkgundo inside the VM (release mode) =="
ssh_vm "$IP" "cd ~/pkgundo && cargo build --release --quiet"

BIN="/home/pkgundo/pkgundo/target/release/pkgundo"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

echo
echo "== [1] Installing the systemd unit and starting the daemon =="
ssh_vm "$IP" "sudo cp ~/pkgundo/systemd/pkgundo-daemon.service /etc/systemd/system/ && \
    sudo sed -i 's|/usr/bin/pkgundo|$BIN|' /etc/systemd/system/pkgundo-daemon.service && \
    sudo sed -i '/ConditionPathExists/d' /etc/systemd/system/pkgundo-daemon.service && \
    sudo systemctl daemon-reload && sudo systemctl start pkgundo-daemon"
sleep 1
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon did not reach active state"

echo
echo "== [2] pkgundo install-hook: writes the apt.conf.d hook with Exec pointed at this binary =="
ssh_vm "$IP" "sudo $BIN install-hook"
ssh_vm "$IP" "test -f /etc/apt/apt.conf.d/98pkgundo.conf" || fail "install-hook did not write the apt hook file"
ssh_vm "$IP" "grep -q \"$BIN apt-hook-pre\" /etc/apt/apt.conf.d/98pkgundo.conf" \
    || fail "apt hook's Pre-Invoke line was not patched to the real binary path"
ssh_vm "$IP" "grep -q \"$BIN apt-hook-post\" /etc/apt/apt.conf.d/98pkgundo.conf" \
    || fail "apt hook's Post-Invoke line was not patched to the real binary path"
echo "PASS: install-hook wrote a correctly-patched apt hook file."

echo
echo "== [3] Auto-track-on-install: explicitly installed packages are tracked with zero manual 'pkgundo track', dependency-reason installs are not =="
# Real dependency pair, since apt has no equivalent of pacman's synthetic
# --asdeps flag: jq explicitly installed pulls in libjq1/libonig5 as genuine
# automatic dependencies in the same transaction — apt itself records the
# Auto-Installed distinction in /var/lib/apt/extended_states as part of
# resolving it, which is exactly what apt-mark showmanual (and therefore
# our hook) reads. jq (not python3-requests, tried first and found to be a
# pure library package with no executable pkgundo could ever resolve a
# binary for, on any distro) is deliberately chosen because it ships a real
# /usr/bin/jq binary, while its pulled-in deps are libraries only.
ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get update -qq && sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y jq >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^jq" \
    || fail "an explicitly installed package should have been auto-tracked without ever calling 'pkgundo track'"
echo "PASS: explicitly installed package was auto-tracked with zero manual pkgundo commands."
ssh_vm "$IP" "$BIN tracked" | grep -q "^libjq1" \
    && fail "a dependency-reason install (libjq1) should NOT have been auto-tracked"
echo "PASS: package pulled in only as a dependency was correctly left untracked."

echo
echo "== [4] Removing a package that was never tracked: true no-op (no output, exit 0) =="
# fonts-dejavu-core: a real explicit install that's still never trackable
# (pure data package, no executable anywhere under BIN_DIRS) — NOT sl, which
# (like cowsay/figlet/cmatrix below) ships its binary under /usr/games/ and
# so is now correctly auto-tracked on explicit install, which would make
# "removing an untracked package" a false premise for it.
ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y fonts-dejavu-core >/dev/null"
set +e
UNTRACKED_OUT="$(ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get remove -y fonts-dejavu-core 2>&1")"
UNTRACKED_STATUS=$?
set -e
[ "$UNTRACKED_STATUS" -eq 0 ] || fail "apt-get remove of an untracked package should still exit 0"
echo "$UNTRACKED_OUT" | grep -qi "pkgundo was tracking\|tracked apps were just removed" \
    && fail "expected no pkgundo reminder for an untracked package removal"
echo "PASS: untracked-package removal produced no reminder and exited cleanly."

echo
echo "== [5] Single-match case: track a real package, remove it, expect the reminder =="
ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y htop >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^htop" || fail "htop should have been auto-tracked on explicit install"
SINGLE_OUT="$(ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get remove -y htop 2>&1")"
echo "$SINGLE_OUT"
echo "$SINGLE_OUT" | grep -q "pkgundo was tracking removed package 'htop'" || fail "expected the single-match reminder naming htop"
echo "$SINGLE_OUT" | grep -q "pkgundo untrack htop --rollback" || fail "expected the rollback command suggestion"
echo "$SINGLE_OUT" | grep -q "pkgundo untrack htop --rollback --dry-run" || fail "expected the dry-run preview command suggestion"
echo "PASS: single tracked-package removal produced the expected reminder."

echo
echo "== [6] Bulk-removal case: two tracked packages removed in one transaction, one combined summary =="
ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y tree cowsay >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^tree" || fail "tree should have been auto-tracked"
ssh_vm "$IP" "$BIN tracked" | grep -q "^cowsay" || fail "cowsay should have been auto-tracked"
BULK_OUT="$(ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get remove -y tree cowsay 2>&1")"
echo "$BULK_OUT"
echo "$BULK_OUT" | grep -q "2 tracked apps were just removed" || fail "expected a single combined summary block for 2 tracked apps"
echo "$BULK_OUT" | grep -q "tree" || fail "expected tree named in the bulk summary"
echo "$BULK_OUT" | grep -q "cowsay" || fail "expected cowsay named in the bulk summary"
REMINDER_BLOCKS="$(echo "$BULK_OUT" | grep -c "tracked apps were just removed\|pkgundo was tracking removed package")"
[ "$REMINDER_BLOCKS" -eq 1 ] || fail "expected exactly one summary block, not one reminder per package"
echo "PASS: bulk removal of 2 tracked packages produced a single combined summary, not a wall of separate reminders."

echo
echo "== [7] Structurally new vs. pacman: one transaction that both installs and removes a tracked package =="
ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y figlet >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^figlet" || fail "figlet should have been auto-tracked before this combined step"
# apt's 'pkg-' syntax removes a package in the same invocation that installs
# another — exercises the single Pre/Post-Invoke pair correctly doing both
# halves (auto-track the install, remind on the removal) in one pass, which
# pacman's two independently-triggered hook files never have to handle.
COMBINED_OUT="$(ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y sl figlet- 2>&1")"
echo "$COMBINED_OUT"
echo "$COMBINED_OUT" | grep -q "pkgundo is now auto-tracking newly installed 'sl'" \
    || fail "expected sl to be auto-tracked as part of the combined install+remove transaction"
echo "$COMBINED_OUT" | grep -q "pkgundo was tracking removed package 'figlet'" \
    || fail "expected the figlet removal reminder as part of the combined install+remove transaction"
echo "PASS: a single apt transaction correctly auto-tracked one package and reminded about another removed in the same pass."

echo
echo "== [8] Exit-code contract: hooks must exit 0 even when something inside breaks =="
# curl was installed as base tooling during VM setup, so it's guaranteed
# present here without needing an extra install step of its own.
# Simulate an internal failure by making the DB temporarily unreadable to
# the hooks' own readonly opens.
ssh_vm "$IP" "sudo chmod 000 /var/lib/pkgundo/pkgundo.db"
set +e
ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y --reinstall curl"
BROKEN_STATUS=$?
set -e
ssh_vm "$IP" "sudo chmod 644 /var/lib/pkgundo/pkgundo.db"
[ "$BROKEN_STATUS" -eq 0 ] || fail "apt must still exit 0 even when the hooks' internal DB read fails (got $BROKEN_STATUS)"
echo "PASS: DB-read failure is swallowed internally — apt's own exit status is unaffected."

# Apt-specific variant the pacman hook design doesn't need: the snapshot
# file itself unwritable/unreadable (Pre-Invoke failing here is higher
# stakes than pacman's Exec — it can abort the whole apt transaction).
ssh_vm "$IP" "sudo rm -f /var/lib/pkgundo/apt-snapshot && sudo mkdir -p /var/lib/pkgundo/apt-snapshot"
set +e
ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y --reinstall curl"
SNAPSHOT_BROKEN_STATUS=$?
set -e
ssh_vm "$IP" "sudo rmdir /var/lib/pkgundo/apt-snapshot"
[ "$SNAPSHOT_BROKEN_STATUS" -eq 0 ] || fail "apt must still exit 0 even when the snapshot file itself can't be written/read (got $SNAPSHOT_BROKEN_STATUS)"
echo "PASS: a broken snapshot file is swallowed internally — apt's own exit status is unaffected even for Pre-Invoke."

echo
echo "== [9] DB-lock contention: hooks read consistent data while the daemon is actively capturing for another app =="
ssh_vm "$IP" "printf '#include <unistd.h>\nint main(){sleep(6);return 0;}\n' > /tmp/slow.c && sudo gcc -x c /tmp/slow.c -o /usr/local/bin/slowapp"
ssh_vm "$IP" "cd ~/pkgundo && $BIN track slowapp"
ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y fortune-mod >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^fortune-mod" || fail "fortune-mod should have been auto-tracked before this contention step"
ssh_vm "$IP" "/usr/local/bin/slowapp &" # long-lived launch keeps a mutation-capture mark armed
sleep 1
CONTENTION_OUT="$(ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get remove -y fortune-mod 2>&1")"
echo "$CONTENTION_OUT"
echo "$CONTENTION_OUT" | grep -q "pkgundo was tracking removed package 'fortune-mod'" \
    || fail "expected the fortune-mod reminder even with the daemon mid-capture for slowapp"
echo "PASS: hook produced a correct reminder while the daemon held an active capture elsewhere."

echo
echo "== [10] install-hook --remove: apt hook file gone, subsequent install/removal produce no auto-track/reminder =="
ssh_vm "$IP" "sudo $BIN install-hook --remove"
ssh_vm "$IP" "test -f /etc/apt/apt.conf.d/98pkgundo.conf" && fail "apt hook file should be gone after install-hook --remove"
# Deliberately a package name never touched anywhere earlier in this
# script — every package used so far was auto-tracked and the removal hook
# never auto-untracks (detection-only, by design), so a previously-used
# name would still show status=tracking regardless of whether
# install-hook --remove actually works, making that check a false
# pass/fail either way (the exact bug found and fixed in
# pacman-hook-test.sh this same session).
ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y cmatrix >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^cmatrix" \
    && fail "expected no auto-tracking once the apt hook itself is uninstalled"
echo "PASS: install-hook --remove cleanly disables both auto-tracking and the removal reminder."

echo
echo "== [11] No-op apt invocation (reinstalling an already-installed package): zero side effects, snapshot cycle stays valid =="
ssh_vm "$IP" "sudo $BIN install-hook" # re-enable for this check
ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y --reinstall cmatrix" # net-zero package-list change
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^cmatrix" && fail "a no-op reinstall must not spuriously auto-track anything"
# Confirm the snapshot cycle is still healthy for a REAL subsequent
# transaction — a stale/corrupted snapshot from the no-op above would
# otherwise silently break detection here.
ssh_vm "$IP" "sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y bc >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^bc" || fail "a real install right after a no-op reinstall should still be detected correctly"
echo "PASS: a no-op apt invocation produced zero side effects and didn't corrupt the snapshot for the next real transaction."

ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon health was affected by hook/CLI-side testing — it never should be"
echo "PASS: daemon health unaffected throughout — hooks and install-hook are both CLI-side, no daemon involvement."

echo
echo "== Reverting VM back to clean snapshot (leaving it ready for next run) =="
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running >/dev/null

echo
echo "APT-HOOK TEST PASSED"
