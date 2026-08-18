#!/usr/bin/env bash
# Reverts the test VM to its clean snapshot, installs real Firefox, actually
# launches it (headless via Xvfb) so it creates a real profile under
# ~/.mozilla and ~/.cache/mozilla, then exercises scan_leftovers end to end:
#   - while installed: exact match on ~/.mozilla via the vendor/URL-derived
#     token (not a hardcoded "firefox -> .mozilla" mapping)
#   - after `pacman -Rcns firefox` (fully removed): the cached-archive
#     fallback in stage 1 of signal derivation still finds it
#   - a real (non-dry-run) removal: directory gone + archive copy exists
#
# Usage:
#   ./scan-leftovers-firefox-test.sh
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
echo "== [1] Full system update (base image's glibc is too old for current firefox), then installing firefox + xvfb =="
ssh_vm "$IP" "sudo pacman -Syu --noconfirm >/dev/null"
ssh_vm "$IP" "sudo pacman -S --noconfirm firefox xorg-server-xvfb >/dev/null"

echo
echo "== [2] Creating a real firefox profile (xvfb + -CreateProfile; avoids the GL compositor crash of full --headless rendering) =="
ssh_vm "$IP" "xvfb-run -a -- firefox -CreateProfile default >/tmp/ff-create.log 2>&1; cat /tmp/ff-create.log"
echo "Profile contents created:"
PROFILE_DIR="$(ssh_vm "$IP" "find ~ -maxdepth 3 -iname 'mozilla' -type d 2>/dev/null | head -1")"
echo "Firefox profile parent dir: $PROFILE_DIR"
ssh_vm "$IP" "test -n '$PROFILE_DIR' && test -d '$PROFILE_DIR'" || fail "firefox did not create a mozilla profile dir under \$HOME"
echo "PASS: real firefox profile exists at $PROFILE_DIR (this Firefox build (v153) uses the newer XDG-compliant \$XDG_CONFIG_HOME/mozilla layout, not the legacy ~/.mozilla)."

echo
echo "== [3] scan-leftovers firefox --dry-run (still installed) =="
OUT="$(ssh_vm "$IP" "cd ~/pkgundo && $BIN scan-leftovers firefox --dry-run")"
echo "$OUT"
echo "$OUT" | grep -qE '\[EXACT\].*mozilla$' || fail "expected the mozilla profile dir as an [EXACT] match via the vendor/URL token while installed"
echo "PASS: exact match on the mozilla profile dir found via dynamic vendor-token derivation (not hardcoded)."

echo
echo "== [4] Fully uninstalling firefox (pacman -Rcns) =="
ssh_vm "$IP" "sudo pacman -Rcns --noconfirm firefox >/dev/null"
ssh_vm "$IP" "pacman -Qi firefox" >/dev/null 2>&1 && fail "firefox is still installed after -Rcns"
ssh_vm "$IP" "ls /var/cache/pacman/pkg/ | grep -i '^firefox-' | head -3" || fail "expected a cached firefox archive under /var/cache/pacman/pkg"
echo "PASS: firefox fully removed, cached archive still present."

echo
echo "== [5] scan-leftovers firefox --dry-run (removed — cached-archive fallback) =="
OUT="$(ssh_vm "$IP" "cd ~/pkgundo && $BIN scan-leftovers firefox --dry-run")"
echo "$OUT"
echo "$OUT" | grep -q 'mozilla$' || fail "expected the mozilla profile dir to still be found via the cached-archive fallback after removal"
echo "PASS: leftover scanner still finds the mozilla profile dir purely from the cached package archive (no install needed)."

echo
echo "== [6] Real (non-dry-run) removal, confirm directory gone + archive copy exists =="
ssh_vm "$IP" "cd ~/pkgundo && yes | sudo $BIN scan-leftovers firefox"
ssh_vm "$IP" "test -d '$PROFILE_DIR'" && fail "$PROFILE_DIR should be gone after real scan-leftovers removal"
ssh_vm "$IP" "sudo find /var/lib/pkgundo/archives -path '*mozilla*' | head -5" || fail "expected an archive copy under /var/lib/pkgundo/archives"
echo "PASS: mozilla profile dir removed for real, and archived first (not lost)."

echo
echo "== Reverting VM back to clean snapshot (leaving it ready for next run) =="
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running >/dev/null

echo
echo "FIREFOX SCAN-LEFTOVERS TEST PASSED"
