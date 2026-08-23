#!/usr/bin/env bash
# Verifies the fanotify FAN_MARK_FILESYSTEM + FAN_REPORT_FID EXDEV
# workaround (src/btrfs_mount.rs): on this VM's btrfs root, mutation
# capture previously always failed silently (EXDEV), reporting zero
# mutations for any tracked app regardless of what it actually wrote. The
# fix mounts the owning device's subvolume id 5 read-only under
# /run/pkgundo/btrfs-root/ and marks that instead.
#
# Also covers the ext4 regression case on this same VM (a loop-mounted
# ext4 image for a second user, same pattern as
# exec-watch-multifs-test.sh): proves is_btrfs()'s short-circuit means
# non-btrfs paths are completely unaffected by this change.
#
# Usage:
#   ./btrfs-mutation-test.sh
#
# Run ./setup-vm-fedora.sh once before using this.

cd "$(dirname "${BASH_SOURCE[0]}")"
export VM_NAME="${VM_NAME:-pkgundo-test-fedora}"
export BASE_IMAGE_FILENAME="${BASE_IMAGE_FILENAME:-fedora-base.qcow2}"
source ./lib.sh

require_tools virsh ssh rsync

if ! virsh dominfo "$VM_NAME" >/dev/null 2>&1; then
    echo "VM '$VM_NAME' doesn't exist yet. Run ./setup-vm-fedora.sh first." >&2
    exit 1
fi

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

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

echo "== Installing the daemon and a synthetic test binary that writes under \$HOME =="
ssh_vm "$IP" "printf '#include <stdio.h>\n#include <stdlib.h>\n#include <unistd.h>\nint main(){sleep(1);char p[512];snprintf(p,sizeof(p),\"%%s/.testapp-marker\",getenv(\"HOME\"));FILE*f=fopen(p,\"w\");if(f)fclose(f);sleep(1);return 0;}\n' > /tmp/t.c && sudo gcc -x c /tmp/t.c -o /usr/local/bin/testapp"
ssh_vm "$IP" "sudo cp ~/pkgundo/systemd/pkgundo-daemon.service /etc/systemd/system/ && \
    sudo sed -i 's|/usr/bin/pkgundo|/home/pkgundo/pkgundo/target/release/pkgundo|' /etc/systemd/system/pkgundo-daemon.service && \
    sudo sed -i '/ConditionPathExists/d' /etc/systemd/system/pkgundo-daemon.service && \
    sudo systemctl daemon-reload && sudo systemctl start pkgundo-daemon"
sleep 1
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon did not reach active state"

echo
echo "== [1] Confirm the VM's root is genuinely btrfs (the premise of this whole test) =="
FSTYPE="$(ssh_vm "$IP" "stat -f -c %T /home/pkgundo")"
echo "  /home/pkgundo fstype: $FSTYPE"
[ "$FSTYPE" = "btrfs" ] || fail "expected /home/pkgundo to be on btrfs — this VM's premise no longer holds"

echo
echo "== [2] Track + launch the test binary; confirm NON-ZERO mutations are now captured on btrfs =="
ssh_vm "$IP" "cd ~/pkgundo && $BIN track testapp"
ssh_vm "$IP" "/usr/local/bin/testapp"
sleep 2
TXID="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT txid FROM tracked_apps WHERE name='testapp'\"")"
[ -n "$TXID" ] || fail "could not find testapp's bucket txid"
MUT_COUNT="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT COUNT(*) FROM mutations WHERE txid=$TXID\"")"
echo "  mutations captured: $MUT_COUNT"
[ "$MUT_COUNT" -gt 0 ] || fail "expected non-zero mutations on btrfs — this is the core fix, previously always zero"
MARKER_COUNT="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT COUNT(*) FROM mutations WHERE txid=$TXID AND path='/home/pkgundo/.testapp-marker'\"")"
[ "$MARKER_COUNT" -ge 1 ] || fail "expected the marker file itself to be captured"
echo "PASS: btrfs mutation capture now works — $MUT_COUNT mutation(s) recorded, including the marker file."

echo
echo "== [3] Confirm exactly one proxy mount was created for the device, not per-launch =="
ssh_vm "$IP" "rm -f ~/.testapp-marker"
ssh_vm "$IP" "/usr/local/bin/testapp"
sleep 2
MOUNT_LOG_COUNT="$(ssh_vm "$IP" "sudo journalctl -u pkgundo-daemon --no-pager | grep -c 'btrfs-root' || true")"
echo "  btrfs-root mentions in daemon log: $MOUNT_LOG_COUNT"
PROXY_MOUNTS="$(ssh_vm "$IP" "findmnt -rn -o TARGET | grep -c '/run/pkgundo/btrfs-root/' || true")"
[ "$PROXY_MOUNTS" -eq 1 ] || fail "expected exactly 1 proxy mount under /run/pkgundo/btrfs-root, found $PROXY_MOUNTS"
echo "PASS: exactly one proxy mount exists after two separate launches on the same device."

echo
echo "== [4] Confirm the proxy mount's options: subvolid=5 and ro =="
MOUNT_OPTS="$(ssh_vm "$IP" "findmnt -rn -o OPTIONS \$(findmnt -rn -o TARGET | grep '/run/pkgundo/btrfs-root/')")"
echo "  mount options: $MOUNT_OPTS"
echo "$MOUNT_OPTS" | grep -q "subvolid=5" || fail "expected subvolid=5 in the proxy mount's options"
echo "$MOUNT_OPTS" | grep -qE '(^|,)ro(,|$)' || fail "expected ro in the proxy mount's options"
echo "PASS: proxy mount is subvolid=5 and read-only, as designed."

echo
echo "== [5] Restart the daemon: confirm the proxy mount is cleanly torn down and re-created =="
ssh_vm "$IP" "sudo systemctl restart pkgundo-daemon"
sleep 1
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon did not come back up after restart"
POST_RESTART_MOUNTS="$(ssh_vm "$IP" "findmnt -rn -o TARGET | grep -c '/run/pkgundo/btrfs-root/' || true")"
[ "$POST_RESTART_MOUNTS" -eq 0 ] || fail "expected the proxy mount to be gone immediately after a daemon restart, found $POST_RESTART_MOUNTS"
ssh_vm "$IP" "rm -f ~/.testapp-marker"
ssh_vm "$IP" "/usr/local/bin/testapp"
sleep 2
RECREATED_MOUNTS="$(ssh_vm "$IP" "findmnt -rn -o TARGET | grep -c '/run/pkgundo/btrfs-root/' || true")"
[ "$RECREATED_MOUNTS" -eq 1 ] || fail "expected the proxy mount to be lazily recreated on the next launch after a restart"
TXID2="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT txid FROM tracked_apps WHERE name='testapp'\"")"
MUT_COUNT2="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT COUNT(*) FROM mutations WHERE txid=$TXID2\"")"
[ "$MUT_COUNT2" -gt "$MUT_COUNT" ] || fail "expected more mutations captured after the restart+relaunch (no error/duplicate-mount issue)"
echo "PASS: daemon restart cleanly tears down and lazily re-creates the proxy mount; capture keeps working."

echo
echo "== [6] Ext4 regression: a loop-mounted ext4 \$HOME on this same VM is completely unaffected =="
# Same pattern as exec-watch-multifs-test.sh's Arch-VM check — proves
# is_btrfs()'s short-circuit means non-btrfs paths never touch any of the
# new proxy-mount logic at all.
ssh_vm "$IP" "sudo useradd -m bob"
ssh_vm "$IP" "sudo dd if=/dev/zero of=/home/bob.img bs=1M count=64 status=none"
ssh_vm "$IP" "sudo mkfs.ext4 -q /home/bob.img"
ssh_vm "$IP" "sudo mount -o loop /home/bob.img /home/bob && sudo chown bob:bob /home/bob"
BOB_FSTYPE="$(ssh_vm "$IP" "stat -f -c %T /home/bob")"
[ "$BOB_FSTYPE" = "ext2/ext3" ] || echo "  (note: stat -f reports '$BOB_FSTYPE' for ext4 — ambiguous magic number, expected)"

ssh_vm "$IP" "sudo -u bob /usr/local/bin/testapp"
sleep 2
BOB_MUT="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT COUNT(*) FROM mutations WHERE txid=$TXID2 AND path='/home/bob/.testapp-marker'\"")"
[ "$BOB_MUT" -ge 1 ] || fail "expected bob's ext4-hosted mutation to be captured normally, no btrfs detour involved"
PROXY_MOUNTS_AFTER_BOB="$(ssh_vm "$IP" "findmnt -rn -o TARGET | grep -c '/run/pkgundo/btrfs-root/' || true")"
[ "$PROXY_MOUNTS_AFTER_BOB" -eq 1 ] || fail "expected still exactly 1 proxy mount (bob's ext4 launch should not have created/needed one)"
echo "PASS: ext4-hosted \$HOME captured normally with zero btrfs proxy-mount involvement — is_btrfs() short-circuit confirmed."

ssh_vm "$IP" "sudo umount /home/bob && sudo rm -f /home/bob.img" || true

ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon health was affected by this test — it never should be"
echo "PASS: daemon health unaffected throughout."

echo
echo "== Reverting VM back to clean snapshot (leaving it ready for next run) =="
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running >/dev/null

echo
echo "BTRFS MUTATION-CAPTURE TEST PASSED"
