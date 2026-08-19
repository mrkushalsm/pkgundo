#!/usr/bin/env bash
# Regression test for the interactive group-review step added to
# `untrack --rollback`: mutations across two distinguishable groups (a
# cache-tagged dir, defaulting to remove, and a config-tagged dir,
# defaulting to keep) should each be filtered per the user's per-group
# answer, driven non-interactively via piped stdin — proving the
# RollbackEngine.with_selected_groups() wiring actually takes effect on a
# real system, not just in the unit tests.
#
# Deliberately uses a synthetic two-directory test binary rather than a
# real package (e.g. weechat): this test is about proving the *review
# mechanism* (grouping, tagging, piped-answer parsing, selective
# archive/remove) works end to end, which a deterministic binary
# demonstrates just as well as a real app while staying fast and
# network-independent, matching this suite's existing exec-watch-test.sh
# precedent.
#
# Usage:
#   ./untrack-review-test.sh
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
echo "== [1] Building a test binary that writes into two distinguishable groups =="
# ~/.cache/testapp/... tags as Cache (default: remove); ~/.config/testapp/...
# tags as Data (default: keep) — see review.rs's tagging heuristic.
#
# Creates each parent XDG dir explicitly (~/.cache, then ~/.cache/testapp,
# same for ~/.config) rather than a single non-recursive mkdir() straight
# to the leaf: this minimal Arch cloud image has neither ~/.cache nor
# ~/.config pre-created for a fresh user, and a single mkdir() to a leaf
# whose parent doesn't exist fails with ENOENT — silently, since the return
# value went unchecked — meaning the fopen() calls after it silently failed
# too and nothing was ever written. Caught via a live VM run showing 0
# mutations captured despite the mark correctly arming/disarming.
ssh_vm "$IP" "printf '#include <stdio.h>\n#include <stdlib.h>\n#include <unistd.h>\n#include <sys/stat.h>\nint main(){sleep(1);char home[256];snprintf(home,sizeof(home),\"%%s\",getenv(\"HOME\"));char c0[512],d1[512],c1[512],d2[512],p1[512],p2[512];snprintf(c0,sizeof(c0),\"%%s/.cache\",home);snprintf(d1,sizeof(d1),\"%%s/.cache/testapp\",home);snprintf(c1,sizeof(c1),\"%%s/.config\",home);snprintf(d2,sizeof(d2),\"%%s/.config/testapp\",home);mkdir(c0,0755);mkdir(d1,0755);mkdir(c1,0755);mkdir(d2,0755);snprintf(p1,sizeof(p1),\"%%s/data.tmp\",d1);snprintf(p2,sizeof(p2),\"%%s/settings.conf\",d2);FILE*f1=fopen(p1,\"w\");if(f1)fclose(f1);FILE*f2=fopen(p2,\"w\");if(f2)fclose(f2);sleep(1);return 0;}\n' > /tmp/rt.c && sudo gcc -x c /tmp/rt.c -o /usr/local/bin/reviewtestapp"
ssh_vm "$IP" "file /usr/local/bin/reviewtestapp | grep -q ELF" || fail "reviewtestapp did not build as a real ELF binary"

echo
echo "== [2] Installing the systemd unit and starting the daemon =="
ssh_vm "$IP" "sudo cp ~/pkgundo/systemd/pkgundo-daemon.service /etc/systemd/system/ && \
    sudo sed -i 's|/usr/bin/pkgundo|/home/pkgundo/pkgundo/target/release/pkgundo|' /etc/systemd/system/pkgundo-daemon.service && \
    sudo sed -i '/ConditionPathExists/d' /etc/systemd/system/pkgundo-daemon.service && \
    sudo systemctl daemon-reload && sudo systemctl start pkgundo-daemon"
sleep 1
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon did not reach active state"

echo
echo "== [3] track reviewtestapp, run it, wait for both mutations to land =="
ssh_vm "$IP" "cd ~/pkgundo && $BIN track reviewtestapp"
ssh_vm "$IP" "rm -rf ~/.cache/testapp ~/.config/testapp"
ssh_vm "$IP" "/usr/local/bin/reviewtestapp"
sleep 2

TXID="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT txid FROM tracked_apps WHERE name='reviewtestapp'\"")"
[ -n "$TXID" ] || fail "could not find reviewtestapp's bucket txid"
MUT_COUNT="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT COUNT(*) FROM mutations WHERE txid=$TXID\"")"
echo "mutation rows under txid=$TXID: $MUT_COUNT"
[ "$MUT_COUNT" -ge 2 ] || fail "expected at least 2 mutations (cache + config), got $MUT_COUNT"
echo "PASS: both groups' files captured into the bucket txid."

echo
echo "== [4] untrack --rollback --dry-run: must still be fully unattended, zero prompts =="
DRYRUN_OUT="$(ssh_vm "$IP" "cd ~/pkgundo && timeout 10 sudo $BIN untrack reviewtestapp --rollback --dry-run < /dev/null")" \
    || fail "dry-run hung or errored — it must never prompt (this feature must not touch --dry-run at all)"
echo "$DRYRUN_OUT"
ssh_vm "$IP" "test -f ~/.cache/testapp/data.tmp" || fail "dry-run must not have actually removed anything (cache file)"
ssh_vm "$IP" "test -f ~/.config/testapp/settings.conf" || fail "dry-run must not have actually removed anything (config file)"
echo "PASS: --dry-run unaffected by the review feature — unattended, no filesystem changes."

echo
echo "== [5] untrack --rollback for real, driven non-interactively: remove cache group, keep config group =="
# Groups are sorted by key; '.cache/testapp' sorts before '.config/testapp'
# ('a' < 'o'), so the first prompt is the cache group (answer: r = remove)
# and the second is the config group (answer: k = keep).
REVIEW_OUT="$(ssh_vm "$IP" "cd ~/pkgundo && printf 'r\nk\n' | sudo $BIN untrack reviewtestapp --rollback")"
echo "$REVIEW_OUT"
echo "$REVIEW_OUT" | grep -qi "2 group" || fail "expected the review UI to report 2 groups to review"

ssh_vm "$IP" "test -f ~/.cache/testapp/data.tmp" && fail "cache group should have been removed (answered 'r')"
ssh_vm "$IP" "test -f ~/.config/testapp/settings.conf" || fail "config group should have been kept (answered 'k')"
ssh_vm "$IP" "sudo find /var/lib/pkgundo/archives/$TXID -iname 'data.tmp'" || fail "expected an archive copy of the removed cache file (archive-before-remove even for reviewed removals)"
echo "PASS: review-driven selective rollback — removed group's file gone (and archived), kept group's file untouched."

echo
echo "== Reverting VM back to clean snapshot (leaving it ready for next run) =="
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running >/dev/null

echo
echo "UNTRACK-REVIEW TEST PASSED"
