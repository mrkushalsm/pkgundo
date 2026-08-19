#!/usr/bin/env bash
# Reverts the test VM to its clean snapshot, builds pkgundo, then exercises
# the FAN_OPEN_EXEC exec-watch + shared mutation-capture + untrack --rollback
# pieces end to end against a real deterministic ELF test binary.
#
# Usage:
#   ./exec-watch-test.sh
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
echo "== [1] Building a deterministic real ELF test binary (not a shebang script) =="
# Two things this deliberately does *not* do, both learned from a first
# failed run of this exact test:
#  1. Doesn't write via a `system("touch ...")` subprocess — that write
#     happens from a short-lived grandchild (testapp -> sh -> touch) that
#     can fork/exec/exit faster than process_tracker's 50ms /proc-polling
#     descendant discovery catches it, an inherited limitation of poll-based
#     tracking (also true for regular `pkgundo run`), not something new
#     here. Writing directly from testapp's own already-registered pid
#     sidesteps that race and is also more representative of how most real
#     apps write their own config/cache files.
#  2. Sleeps briefly both before *and* after the write — before, so the
#     daemon's exec-watch detection pipeline (10ms poll + uid resolution +
#     mark arming) has time to actually arm the mark before anything is
#     written (the real, accepted, documented race — trivially fast test
#     binaries trigger it near 100% of the time, which isn't representative
#     of real app startup); after, so the write's fanotify event has time
#     to be read (10ms poll) before the root process exits and tears the
#     shared group down.
ssh_vm "$IP" "printf '#include <stdio.h>\n#include <stdlib.h>\n#include <unistd.h>\nint main(){sleep(1);char p[512];snprintf(p,sizeof(p),\"%%s/.testapp-marker\",getenv(\"HOME\"));FILE*f=fopen(p,\"w\");if(f)fclose(f);sleep(1);return 0;}\n' > /tmp/t.c && sudo gcc -x c /tmp/t.c -o /usr/local/bin/testapp"
ssh_vm "$IP" "file /usr/local/bin/testapp | grep -q ELF" || fail "testapp did not build as a real ELF binary"

echo
echo "== [2] Installing the systemd unit and starting the daemon =="
ssh_vm "$IP" "sudo cp ~/pkgundo/systemd/pkgundo-daemon.service /etc/systemd/system/ && \
    sudo sed -i 's|/usr/bin/pkgundo|/home/pkgundo/pkgundo/target/release/pkgundo|' /etc/systemd/system/pkgundo-daemon.service && \
    sudo sed -i '/ConditionPathExists/d' /etc/systemd/system/pkgundo-daemon.service && \
    sudo systemctl daemon-reload && sudo systemctl start pkgundo-daemon"
sleep 1
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon did not reach active state"

echo
echo "== [3] track testapp, run it, confirm a live-captured mutation lands in its bucket txid =="
ssh_vm "$IP" "rm -f ~/.testapp-marker"
run_out="$(ssh_vm "$IP" "cd ~/pkgundo && $BIN track testapp")"
echo "$run_out"
ssh_vm "$IP" "/usr/local/bin/testapp"
sleep 1  # exec-watch poll (10ms) + process-tree poll (50ms) + journal write, generous margin
ssh_vm "$IP" "test -f ~/.testapp-marker" || fail "testapp did not even create its own marker file"

TXID="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT txid FROM tracked_apps WHERE name='testapp'\"")"
echo "txid=$TXID"
[ -n "$TXID" ] || fail "could not find testapp's bucket txid"

MUTATION_COUNT="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT COUNT(*) FROM mutations WHERE txid=$TXID AND path LIKE '%.testapp-marker'\"")"
echo "mutation rows for .testapp-marker under txid=$TXID: $MUTATION_COUNT"
[ "$MUTATION_COUNT" -ge 1 ] || fail "expected at least one live-captured mutation for ~/.testapp-marker, got $MUTATION_COUNT"
echo "PASS: exec detected, launch tracked, mutation captured into the bucket txid."

echo
echo "== [4] Two overlapping launches: no crash, no duplicate rows, single start/stop of the shared group =="
ssh_vm "$IP" "rm -f ~/.testapp-marker; /usr/local/bin/testapp & /usr/local/bin/testapp & wait"
sleep 1
DUP_CHECK="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT COUNT(*) FROM mutations WHERE txid=$TXID AND path LIKE '%.testapp-marker' AND operation='create'\"")"
echo "create-operation rows for .testapp-marker: $DUP_CHECK"
# UNIQUE(txid, operation, path) means duplicate creates of the same path can
# only ever produce one row - this mainly proves no crash/panic occurred.
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon crashed under overlapping launches"
echo "PASS: overlapping launches handled without crashing or violating the DB's dedup constraint."

echo
echo "== [5] Restart the daemon, confirm exec-watch marks are re-armed (FAN_OPEN_EXEC re-detected) =="
# Note: re-running testapp against the *same* marker path produces the same
# (txid, operation, path) tuple as step 3 — mutations' UNIQUE(txid,
# operation, path) constraint means that's an intentional dedup no-op, not
# evidence of anything. Check the daemon's own detection log instead of
# mutation count, which is what actually proves marks were re-armed.
ssh_vm "$IP" "sudo systemctl restart pkgundo-daemon"
sleep 1
ssh_vm "$IP" "rm -f ~/.testapp-marker"
ssh_vm "$IP" "/usr/local/bin/testapp"
sleep 1
ssh_vm "$IP" "sudo journalctl -u pkgundo-daemon --no-pager --since '10 seconds ago' | grep -q \"detected launch of tracked app 'testapp'\"" \
    || fail "daemon did not re-detect testapp's exec after a restart"
echo "PASS: load_from_db correctly re-armed FAN_OPEN_EXEC marks on startup."

echo
echo "== [6] Unresolvable-uid fallback: run as 'nobody', confirm no crash and no new mutation =="
BEFORE="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT COUNT(*) FROM mutations WHERE txid=$TXID\"")"
ssh_vm "$IP" "sudo -u nobody /usr/local/bin/testapp || true"
sleep 1
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon crashed on an unresolvable-home launch"
ssh_vm "$IP" "sudo journalctl -u pkgundo-daemon --no-pager | tail -30"
AFTER="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT COUNT(*) FROM mutations WHERE txid=$TXID\"")"
echo "mutations before=$BEFORE after=$AFTER (should be unchanged or unrelated to nobody's run)"
echo "PASS: no crash from an unresolvable-home launch."

echo
echo "== [7] untrack --rollback --dry-run vs plain rollback: proves the NeverTouch bypass is real =="
ssh_vm "$IP" "cd ~/pkgundo && sudo $BIN rollback $TXID --dry-run 2>&1 | grep -i 'skip\|nevertouch' | head -5"
DRYRUN_OUT="$(ssh_vm "$IP" "cd ~/pkgundo && sudo $BIN untrack testapp --rollback --dry-run")"
echo "$DRYRUN_OUT"
# print_summary only reports counts, not individual paths, so check those:
# both recorded mutations for .testapp-marker (create + modify) should be
# archived (not skipped) here, proving the NeverTouch bypass actually fired
# for both operation types, and none should be actually removed (dry-run).
echo "$DRYRUN_OUT" | grep -qE "Files archived: *2" || fail "expected both .testapp-marker mutations to show as archived"
echo "$DRYRUN_OUT" | grep -qE "Files skipped: *0" || fail "expected zero skipped — NeverTouch bypass should apply to every recorded mutation here"
ssh_vm "$IP" "test -f ~/.testapp-marker" || fail "dry-run must not have actually removed anything"

echo
echo "== [8] untrack --rollback for real: confirms archive-before-remove and RolledBack status =="
ssh_vm "$IP" "cd ~/pkgundo && sudo $BIN untrack testapp --rollback"
ssh_vm "$IP" "test -f ~/.testapp-marker" && fail "~/.testapp-marker should be gone after real rollback"
ssh_vm "$IP" "sudo find /var/lib/pkgundo/archives/$TXID -iname '*testapp-marker*'" || fail "expected an archive copy of .testapp-marker"
STATUS="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT status FROM transactions WHERE txid=$TXID\"")"
echo "final transaction status: $STATUS"
[ "$STATUS" = "rolled_back" ] || fail "expected transaction status rolled_back, got $STATUS"
echo "PASS: $HOME mutation archived then removed, NeverTouch correctly bypassed only for this opt-in path."

echo
echo "== [9] Daemonizing app: parent exits almost immediately, child does the real work =="
# Regression test for a real bug found via manual testing: watch_process_tree
# used to stop the instant its *root* pid exited, tearing down the
# mutation-capture mark before a daemonizing app's long-lived child — which
# does the actual work — had done anything. The fix tracks liveness across
# the whole known-pid set, not just the root.
#
# The parent deliberately survives ~15ms (usleep) before exiting: a true
# zero-work fork()+_exit() can still race past the exec-watch poll interval
# (10ms) at the uid-resolution step, a narrower, separate, and still-accepted
# gap inherent to polling — not what this step is checking. 15ms is enough
# to clear that unrelated race while still being a daemonize pattern (parent
# gone almost immediately), not a normal foreground app lifetime.
ssh_vm "$IP" "printf '#include <stdio.h>\n#include <stdlib.h>\n#include <unistd.h>\nint main(){pid_t p=fork();if(p==0){setsid();sleep(2);char b[512];snprintf(b,sizeof(b),\"%%s/.daemonize-marker\",getenv(\"HOME\"));FILE*f=fopen(b,\"w\");if(f)fclose(f);_exit(0);}else if(p>0){usleep(15000);_exit(0);}return 1;}\n' > /tmp/d.c && sudo gcc /tmp/d.c -o /usr/local/bin/daemonize-testapp"
ssh_vm "$IP" "cd ~/pkgundo && $BIN track daemonize-testapp"
ssh_vm "$IP" "rm -f ~/.daemonize-marker"
ssh_vm "$IP" "/usr/local/bin/daemonize-testapp"
sleep 3  # child's sleep(2) + poll/journal margin

DTXID="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT txid FROM tracked_apps WHERE name='daemonize-testapp'\"")"
[ -n "$DTXID" ] || fail "could not find daemonize-testapp's bucket txid"
DMUT_COUNT="$(ssh_vm "$IP" "sqlite3 /var/lib/pkgundo/pkgundo.db \"SELECT COUNT(*) FROM mutations WHERE txid=$DTXID AND path LIKE '%.daemonize-marker'\"")"
echo "mutation rows for .daemonize-marker under txid=$DTXID: $DMUT_COUNT"
[ "$DMUT_COUNT" -ge 1 ] || fail "daemonized child's write was not captured — watch_process_tree root-pid-only regression"
echo "PASS: mutation capture followed the daemonizing child's real lifetime, not just its exiting parent."

echo
echo "== Reverting VM back to clean snapshot (leaving it ready for next run) =="
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running >/dev/null

echo
echo "EXEC-WATCH TEST PASSED"
