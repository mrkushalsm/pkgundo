#!/usr/bin/env bash
# Regression test for the st_dev-based per-filesystem refcounting fix in
# MutationCapture: a hardcoded "/" mark would silently capture nothing under
# a separate /home partition, which is a common real-world layout. This
# needs a genuinely separate filesystem to prove, which the plain
# exec-watch-test.sh VM doesn't have (its /home lives on the same partition
# as / by default) — hence a sibling script rather than a step folded into
# the main one: it needs a throwaway system user + a loop-mounted image,
# more invasive than anything else in that suite, so it's kept separate and
# run less frequently rather than on every regression pass.
#
# Usage:
#   ./exec-watch-multifs-test.sh
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

BIN="./target/release/pkgundo"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

echo
echo "== [1] Creating a second user whose \$HOME is a genuinely separate filesystem =="
# A loop-mounted ext4 image mounted directly onto the new user's home
# directory — a different st_dev from wherever /home/pkgundo lives (btrfs
# root, on the base VM image), which is exactly the distinction the fix is
# for: fanotify_mark(FAN_MARK_FILESYSTEM, <path>) resolves to and marks
# whichever filesystem <path> actually lives on.
ssh_vm "$IP" "sudo useradd -m alice"
ssh_vm "$IP" "sudo dd if=/dev/zero of=/home/alice.img bs=1M count=64 status=none"
ssh_vm "$IP" "sudo mkfs.ext4 -q /home/alice.img"
ssh_vm "$IP" "sudo mount -o loop /home/alice.img /home/alice && sudo chown alice:alice /home/alice"
# Compare st_dev directly (via `stat -c %d`) rather than a filesystem-type
# name: that's precisely what MetadataExt::dev() in the Rust code checks,
# and stat -f's %T can report ambiguous names for ext4 (often "ext2/ext3",
# a shared statfs magic number) — st_dev is the actual, unambiguous thing
# that matters here. findmnt isn't used because it only resolves a path
# that IS itself a mountpoint, and /home/pkgundo is just a plain directory
# on the root filesystem, not a mount root.
ROOT_DEV="$(ssh_vm "$IP" "stat -c %d /home/pkgundo")"
ALICE_DEV="$(ssh_vm "$IP" "stat -c %d /home/alice")"
echo "/home/pkgundo st_dev=$ROOT_DEV, /home/alice st_dev=$ALICE_DEV"
[ "$ROOT_DEV" != "$ALICE_DEV" ] || fail "expected /home/alice to be a genuinely separate filesystem (different st_dev) from /home/pkgundo"

echo
echo "== [2] Building the test binary, installing the daemon, tracking it =="
ssh_vm "$IP" "printf '#include <stdio.h>\n#include <stdlib.h>\n#include <unistd.h>\nint main(){sleep(1);char p[512];snprintf(p,sizeof(p),\"%%s/.testapp-marker\",getenv(\"HOME\"));FILE*f=fopen(p,\"w\");if(f)fclose(f);sleep(1);return 0;}\n' > /tmp/t.c && sudo gcc -x c /tmp/t.c -o /usr/local/bin/testapp"
ssh_vm "$IP" "sudo cp ~/pkgundo/systemd/pkgundo-daemon.service /etc/systemd/system/ && \
    sudo sed -i 's|/usr/bin/pkgundo|/home/pkgundo/pkgundo/target/release/pkgundo|' /etc/systemd/system/pkgundo-daemon.service && \
    sudo sed -i '/ConditionPathExists/d' /etc/systemd/system/pkgundo-daemon.service && \
    sudo systemctl daemon-reload && sudo systemctl start pkgundo-daemon"
sleep 1
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon did not reach active state"
ssh_vm "$IP" "cd ~/pkgundo && $BIN track testapp"

echo
echo "== [3] Run as alice (separate filesystem): confirm the mark is armed for /home/alice, not / or /home/pkgundo =="
ssh_vm "$IP" "sudo -u alice /usr/local/bin/testapp"
sleep 2
ssh_vm "$IP" "sudo journalctl -u pkgundo-daemon --no-pager --since '10 seconds ago' | grep -q 'armed filesystem mark for /home/alice'" \
    || fail "expected a mark armed specifically for /home/alice"
ssh_vm "$IP" "sudo journalctl -u pkgundo-daemon --no-pager --since '10 seconds ago' | grep -qE 'armed filesystem mark for /$'" \
    && fail "a mark was armed for bare '/' — the exact bug a hardcoded '/' mark would cause"

TXID="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT txid FROM tracked_apps WHERE name='testapp'\"")"
[ -n "$TXID" ] || fail "could not find testapp's bucket txid"
ALICE_MUT="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT COUNT(*) FROM mutations WHERE txid=$TXID AND path='/home/alice/.testapp-marker'\"")"
[ "$ALICE_MUT" -ge 1 ] || fail "expected a mutation recorded against /home/alice/.testapp-marker specifically"
echo "PASS: mark correctly scoped to /home/alice's actual filesystem, mutation correctly attributed."

echo
echo "== [4] Concurrent launches on two different filesystems: independent arm/disarm, no cross-attribution =="
ssh_vm "$IP" "rm -f ~/.testapp-marker; sudo rm -f /home/alice/.testapp-marker"
ssh_vm "$IP" "/usr/local/bin/testapp & sudo -u alice /usr/local/bin/testapp & wait"
sleep 2
LOG="$(ssh_vm "$IP" "sudo journalctl -u pkgundo-daemon --no-pager --since '10 seconds ago'")"
echo "$LOG" | grep -q "armed filesystem mark for /home/pkgundo" || fail "expected an independent mark for /home/pkgundo"
echo "$LOG" | grep -q "armed filesystem mark for /home/alice" || fail "expected an independent mark for /home/alice"
echo "$LOG" | grep -q "disarmed filesystem mark for /home/pkgundo" || fail "expected /home/pkgundo's mark to be disarmed independently"
echo "$LOG" | grep -q "disarmed filesystem mark for /home/alice" || fail "expected /home/alice's mark to be disarmed independently"
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon crashed under concurrent multi-filesystem launches"

PKGUNDO_MUT="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT COUNT(*) FROM mutations WHERE txid=$TXID AND path='/home/pkgundo/.testapp-marker'\"")"
ALICE_MUT2="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT COUNT(*) FROM mutations WHERE txid=$TXID AND path='/home/alice/.testapp-marker'\"")"
[ "$PKGUNDO_MUT" -ge 1 ] || fail "expected pkgundo's own mutation to still be captured"
[ "$ALICE_MUT2" -ge 1 ] || fail "expected alice's mutation to still be captured on the second run"
echo "PASS: two filesystems' marks coexisted in the one shared fanotify group without cross-attribution or crashing."

echo
echo "== Cleaning up (unmount loop device) and reverting VM to clean snapshot =="
ssh_vm "$IP" "sudo umount /home/alice && sudo rm -f /home/alice.img" || true
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running >/dev/null

echo
echo "EXEC-WATCH MULTI-FILESYSTEM TEST PASSED"
