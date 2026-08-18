#!/usr/bin/env bash
# Reverts the test VM to its clean snapshot, builds pkgundo inside it, then
# drives the tracked-apps daemon end to end: start the daemon, ping it,
# track a real package and a plain binary, list/untrack/re-track, restart
# the daemon to prove persistence, and confirm a clean error when the
# daemon isn't running. Also exercises scan-leftovers (dry-run, guess-tier
# XDG scoping, never-touch list) since it needs no daemon.
#
# Usage:
#   ./track-test.sh
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

run() {
    echo
    echo "== $* =="
    ssh_vm "$IP" "cd ~/pkgundo && $*"
}

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

echo
echo "== [1] tracked with no daemon running -> clean error, not a panic =="
OUT="$(ssh_vm "$IP" "cd ~/pkgundo && $BIN tracked" 2>&1)" || true
echo "$OUT"
echo "$OUT" | grep -q "daemon is not running" || fail "expected a clean 'daemon is not running' error"

echo
echo "== [2] Installing the systemd unit and starting the daemon =="
ssh_vm "$IP" "sudo cp ~/pkgundo/systemd/pkgundo-daemon.service /etc/systemd/system/ && \
    sudo sed -i 's|/usr/bin/pkgundo|/home/pkgundo/pkgundo/target/release/pkgundo|' /etc/systemd/system/pkgundo-daemon.service && \
    sudo sed -i '/ConditionPathExists/d' /etc/systemd/system/pkgundo-daemon.service && \
    sudo systemctl daemon-reload && sudo systemctl start pkgundo-daemon"
sleep 1
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon did not reach active state"
ssh_vm "$IP" "test -S /run/pkgundo/daemon.sock" || fail "daemon socket was not created"
echo "PASS: daemon active, socket present."

echo
echo "== [3] track a real pacman package (htop, already on the base image or installed now) =="
ssh_vm "$IP" "sudo pacman -S --noconfirm htop >/dev/null"
run "$BIN track htop"
OUT="$(ssh_vm "$IP" "cd ~/pkgundo && $BIN tracked")"
echo "$OUT"
echo "$OUT" | grep -q "htop" || fail "htop not present in tracked list"
echo "$OUT" | grep -q "kind=package" || fail "htop should have resolved as kind=package"

echo
echo "== [4] track a plain binary (/usr/bin/ls) =="
run "$BIN track /usr/bin/ls"
OUT="$(ssh_vm "$IP" "cd ~/pkgundo && $BIN tracked")"
echo "$OUT" | grep -q "kind=binary" || fail "/usr/bin/ls should have resolved as kind=binary"

echo
echo "== [5] untrack htop, confirm it drops out of default listing but stays under --all =="
run "$BIN untrack htop"
OUT="$(ssh_vm "$IP" "cd ~/pkgundo && $BIN tracked")"
echo "$OUT" | grep -q "htop" && fail "htop should not appear in default tracked listing after untrack"
OUT_ALL="$(ssh_vm "$IP" "cd ~/pkgundo && $BIN tracked --all")"
echo "$OUT_ALL" | grep -q "htop" || fail "htop should still appear under --all after untrack"
echo "PASS: untrack semantics correct."

echo
echo "== [6] re-track htop (revive after untrack) =="
run "$BIN track htop"
OUT="$(ssh_vm "$IP" "cd ~/pkgundo && $BIN tracked")"
echo "$OUT" | grep -q "htop" || fail "re-tracked htop should be back in default listing"
echo "PASS: re-track revives the row."

echo
echo "== [7] restart the daemon, confirm tracked apps persisted (DB-backed, not memory-only) =="
ssh_vm "$IP" "sudo systemctl restart pkgundo-daemon"
sleep 1
OUT="$(ssh_vm "$IP" "cd ~/pkgundo && $BIN tracked")"
echo "$OUT" | grep -q "htop" || fail "tracked apps did not survive a daemon restart"
echo "PASS: tracked state persists across daemon restart."

echo
echo "== [8] scan-leftovers dry-run smoke test (no daemon dependency) =="
ssh_vm "$IP" "mkdir -p ~/.config/mytestapp123 && mkdir -p ~/.mytestapp123 && mkdir -p ~/.ssh && touch ~/.ssh/id_test"
OUT="$(ssh_vm "$IP" "cd ~/pkgundo && $BIN scan-leftovers mytestapp123 --dry-run")"
echo "$OUT"
echo "$OUT" | grep -q "mytestapp123" || fail "expected the XDG-scoped guess-tier candidate to be found"
OUT_SSH="$(ssh_vm "$IP" "cd ~/pkgundo && $BIN scan-leftovers ssh --dry-run")"
echo "$OUT_SSH" | grep -q '\.ssh$' && fail "never-touch list should have excluded ~/.ssh"
echo "PASS: scan-leftovers dry-run behaves as designed."

echo
echo "== Reverting VM back to clean snapshot (leaving it ready for next run) =="
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running >/dev/null

echo
echo "TRACK-TEST PASSED"
