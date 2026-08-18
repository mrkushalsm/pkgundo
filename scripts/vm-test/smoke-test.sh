#!/usr/bin/env bash
# Reverts the test VM to its clean snapshot, builds pkgundo inside it, then
# drives a real run -> inspect -> rollback (dry-run, then real) cycle against
# a real package install, and checks the package is actually gone afterward.
#
# Usage:
#   ./smoke-test.sh                          # default: pacman -S htop
#   ./smoke-test.sh pacman -S fastfetch       # test a different package
#
# Run ./setup-vm.sh once before using this.

cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

require_tools virsh ssh rsync

PKG_CMD=("$@")
if [ "${#PKG_CMD[@]}" -eq 0 ]; then
    PKG_CMD=(pacman -S htop --noconfirm)
fi
# extract the package name (last non-flag arg) for the post-rollback check
PKG_NAME=""
for arg in "${PKG_CMD[@]}"; do
    [[ "$arg" == -* ]] && continue
    PKG_NAME="$arg"
done

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
CMD_STR="${PKG_CMD[*]}"

echo
echo "== [1/5] sudo $BIN run $CMD_STR =="
RUN_OUT="$(ssh_vm "$IP" "cd ~/pkgundo && sudo $BIN run $CMD_STR" 2>&1)"
echo "$RUN_OUT"
# Strip ANSI color codes (colored crate emits them even over a non-tty pipe)
# before parsing, so "Transaction ID: <esc>[36m1<esc>[0m" still matches.
TXID="$(echo "$RUN_OUT" | sed -r 's/\x1b\[[0-9;]*m//g' | grep -oP 'Transaction ID: \K[0-9]+' | head -1)"
if [ -z "$TXID" ]; then
    echo "FAIL: could not find a Transaction ID in the run output." >&2
    exit 1
fi
echo "-> txid=$TXID"

echo
echo "== [2/5] sudo $BIN inspect $TXID =="
ssh_vm "$IP" "cd ~/pkgundo && sudo $BIN inspect $TXID"

echo
echo "== [3/5] sudo $BIN rollback $TXID --dry-run =="
ssh_vm "$IP" "cd ~/pkgundo && sudo $BIN rollback $TXID --dry-run"

echo
echo "== [4/5] sudo $BIN rollback $TXID (for real) =="
ssh_vm "$IP" "cd ~/pkgundo && sudo $BIN rollback $TXID"

echo
echo "== [5/5] Verifying $PKG_NAME is actually gone =="
if ssh_vm "$IP" "pacman -Qi '$PKG_NAME'" >/dev/null 2>&1; then
    echo "FAIL: $PKG_NAME is still installed after rollback." >&2
    exit 1
fi
echo "PASS: $PKG_NAME is not installed. Rollback verified end-to-end."

echo
echo "== Reverting VM back to clean snapshot (leaving it ready for next run) =="
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running >/dev/null

echo
echo "SMOKE TEST PASSED for: $CMD_STR"
