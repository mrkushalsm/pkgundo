#!/usr/bin/env bash
# Regression test for both pacman hooks `pkgundo install-hook` manages:
# install-time auto-tracking (explicitly installed packages get tracked with
# zero manual `pkgundo track`, dependency-reason installs are correctly left
# alone) and the removal-time reminder (single-match and bulk-match cases,
# removing an untracked package is a true no-op). Also covers the hard
# safety contract shared by both hooks — their exit code is always 0 even
# when something inside breaks, so neither can ever fail or hang someone's
# package install/removal.
#
# Usage:
#   ./pacman-hook-test.sh
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
echo "== [2] pkgundo install-hook: writes both real hooks with Exec pointed at this binary =="
ssh_vm "$IP" "sudo $BIN install-hook"
ssh_vm "$IP" "test -f /etc/pacman.d/hooks/99-pkgundo-tracked.hook" || fail "install-hook did not write the removal hook file"
ssh_vm "$IP" "grep -q \"Exec = $BIN pacman-hook\" /etc/pacman.d/hooks/99-pkgundo-tracked.hook" \
    || fail "removal hook's Exec line was not patched to the real binary path"
ssh_vm "$IP" "test -f /etc/pacman.d/hooks/98-pkgundo-track-on-install.hook" \
    || fail "install-hook did not write the install-time auto-track hook file"
ssh_vm "$IP" "grep -q \"Exec = $BIN pacman-hook-install\" /etc/pacman.d/hooks/98-pkgundo-track-on-install.hook" \
    || fail "install hook's Exec line was not patched to the real binary path"
echo "PASS: install-hook wrote both correctly-patched hook files."

echo
echo "== [3] Auto-track-on-install: explicitly installed packages are tracked with zero manual 'pkgundo track', dependency-reason installs are not =="
# --asdeps forces pacman's own install-reason bookkeeping to 'dependency'
# regardless of whether anything actually depends on the package — this is
# the same field the hook itself checks via 'pacman -Qi', so it's a
# deterministic way to exercise the skip path without relying on a real
# dependency graph.
ssh_vm "$IP" "sudo pacman -S --noconfirm --needed --asdeps fastfetch >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^fastfetch" \
    && fail "a dependency-reason install should NOT have been auto-tracked"
echo "PASS: package installed as a dependency was correctly left untracked."

ssh_vm "$IP" "sudo pacman -S --noconfirm --needed newsboat >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^newsboat" \
    || fail "an explicitly installed package should have been auto-tracked without ever calling 'pkgundo track'"
echo "PASS: explicitly installed package was auto-tracked with zero manual pkgundo commands."

echo
echo "== [4] Removing a package that was never tracked: true no-op (no output, exit 0) =="
ssh_vm "$IP" "sudo pacman -S --noconfirm --needed fastfetch >/dev/null"
set +e
UNTRACKED_OUT="$(ssh_vm "$IP" "sudo pacman -R --noconfirm fastfetch 2>&1")"
UNTRACKED_STATUS=$?
set -e
[ "$UNTRACKED_STATUS" -eq 0 ] || fail "pacman -R of an untracked package should still exit 0"
echo "$UNTRACKED_OUT" | grep -qi "pkgundo was tracking\|tracked apps were just removed" \
    && fail "expected no pkgundo reminder for an untracked package removal"
echo "PASS: untracked-package removal produced no reminder and exited cleanly."

echo
echo "== [5] Single-match case: track a real package, remove it, expect the reminder =="
# The install-hook auto-tracks htop the moment it's installed below (it's
# an explicit install, same as [3]); the manual '$BIN track htop' call is
# therefore redundant in practice, kept here only to prove the manual path
# still works fine layered on top of an already-auto-tracked package.
ssh_vm "$IP" "sudo pacman -S --noconfirm --needed htop >/dev/null"
ssh_vm "$IP" "cd ~/pkgundo && $BIN track htop"
SINGLE_OUT="$(ssh_vm "$IP" "sudo pacman -R --noconfirm htop 2>&1")"
echo "$SINGLE_OUT"
echo "$SINGLE_OUT" | grep -q "pkgundo was tracking removed package 'htop'" || fail "expected the single-match reminder naming htop"
echo "$SINGLE_OUT" | grep -q "pkgundo untrack htop --rollback" || fail "expected the rollback command suggestion"
echo "$SINGLE_OUT" | grep -q "pkgundo untrack htop --rollback --dry-run" || fail "expected the dry-run preview command suggestion"
echo "PASS: single tracked-package removal produced the expected reminder."

echo
echo "== [6] Bulk-removal case: two tracked packages removed in one transaction, one combined summary =="
ssh_vm "$IP" "sudo pacman -S --noconfirm --needed fastfetch fortune-mod >/dev/null"
ssh_vm "$IP" "cd ~/pkgundo && $BIN track fastfetch && $BIN track fortune-mod"
BULK_OUT="$(ssh_vm "$IP" "sudo pacman -R --noconfirm fastfetch fortune-mod 2>&1")"
echo "$BULK_OUT"
echo "$BULK_OUT" | grep -q "2 tracked apps were just removed" || fail "expected a single combined summary block for 2 tracked apps"
echo "$BULK_OUT" | grep -q "fastfetch" || fail "expected fastfetch named in the bulk summary"
echo "$BULK_OUT" | grep -q "fortune-mod" || fail "expected fortune-mod named in the bulk summary"
REMINDER_BLOCKS="$(echo "$BULK_OUT" | grep -c "tracked apps were just removed\|pkgundo was tracking removed package")"
[ "$REMINDER_BLOCKS" -eq 1 ] || fail "expected exactly one summary block, not one reminder per package"
echo "PASS: bulk removal of 2 tracked packages produced a single combined summary, not a wall of separate reminders."

echo
echo "== [7] Exit-code contract: hook must exit 0 even when something inside it breaks =="
ssh_vm "$IP" "sudo pacman -S --noconfirm --needed newsboat >/dev/null"
ssh_vm "$IP" "cd ~/pkgundo && $BIN track newsboat"
# Simulate an internal failure by making the DB temporarily unreadable to
# the hook's own readonly open — the transaction's exit status is what's
# under test here, not the (expected, harmless) missing reminder.
ssh_vm "$IP" "sudo chmod 000 /var/lib/pkgundo/pkgundo.db"
set +e
ssh_vm "$IP" "sudo pacman -R --noconfirm newsboat"
BROKEN_STATUS=$?
set -e
ssh_vm "$IP" "sudo chmod 644 /var/lib/pkgundo/pkgundo.db"
[ "$BROKEN_STATUS" -eq 0 ] || fail "pacman -R must still exit 0 even when the hook's internal DB read fails (got $BROKEN_STATUS)"
echo "PASS: hook failure is swallowed internally — pacman's own exit status is unaffected."

echo
echo "== [8] DB-lock contention: hook reads consistent data while the daemon is actively capturing for another app =="
ssh_vm "$IP" "printf '#include <unistd.h>\nint main(){sleep(6);return 0;}\n' > /tmp/slow.c && sudo gcc -x c /tmp/slow.c -o /usr/local/bin/slowapp"
ssh_vm "$IP" "cd ~/pkgundo && $BIN track slowapp"
ssh_vm "$IP" "sudo pacman -S --noconfirm --needed tree >/dev/null"
ssh_vm "$IP" "cd ~/pkgundo && $BIN track tree"
ssh_vm "$IP" "/usr/local/bin/slowapp &" # long-lived launch keeps a mutation-capture mark armed
sleep 1
CONTENTION_OUT="$(ssh_vm "$IP" "sudo pacman -R --noconfirm tree 2>&1")"
echo "$CONTENTION_OUT"
echo "$CONTENTION_OUT" | grep -q "pkgundo was tracking removed package 'tree'" \
    || fail "expected the tree reminder even with the daemon mid-capture for slowapp"
echo "PASS: hook produced a correct reminder while the daemon held an active capture elsewhere."

echo
echo "== [9] install-hook --remove: both hook files gone, subsequent install/removal produce no auto-track/reminder =="
ssh_vm "$IP" "sudo pacman -S --noconfirm --needed htop >/dev/null"
ssh_vm "$IP" "cd ~/pkgundo && $BIN track htop"
ssh_vm "$IP" "sudo $BIN install-hook --remove"
ssh_vm "$IP" "test -f /etc/pacman.d/hooks/99-pkgundo-tracked.hook" && fail "removal hook file should be gone after install-hook --remove"
ssh_vm "$IP" "test -f /etc/pacman.d/hooks/98-pkgundo-track-on-install.hook" && fail "install hook file should be gone after install-hook --remove"
POSTREMOVE_OUT="$(ssh_vm "$IP" "sudo pacman -R --noconfirm htop 2>&1")"
echo "$POSTREMOVE_OUT" | grep -qi "pkgundo was tracking" && fail "expected no reminder once the hook itself is uninstalled"
# Deliberately a package name never touched anywhere earlier in this script:
# fastfetch/fortune-mod/newsboat/tree/htop were all tracked in earlier steps
# and the removal hook never auto-untracks (detection-only, by design), so
# any of those would still show status=tracking from a *previous* step
# regardless of whether install-hook --remove actually works, making this
# check a false pass/fail either way. cowsay is fresh, so a tracked-apps hit
# here can only come from the install hook actually re-firing.
ssh_vm "$IP" "sudo pacman -S --noconfirm --needed cowsay >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^cowsay" \
    && fail "expected no auto-tracking once the install hook itself is uninstalled"
echo "PASS: install-hook --remove cleanly disables both auto-tracking and the removal reminder."

ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon health was affected by hook/CLI-side testing — it never should be"
echo "PASS: daemon health unaffected throughout — hook and install-hook are both CLI-side, no daemon involvement."

echo
echo "== Reverting VM back to clean snapshot (leaving it ready for next run) =="
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running >/dev/null

echo
echo "PACMAN-HOOK TEST PASSED"
