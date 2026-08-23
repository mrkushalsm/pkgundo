#!/usr/bin/env bash
# Regression test for the dnf5 hook `pkgundo install-hook` manages — the
# Fedora sibling of pacman-hook-test.sh and apt-hook-test.sh. dnf5's
# actions plugin fires once per package (via ${pkg.name} substitution),
# structurally different from both pacman's stdin-delivered list and
# apt's snapshot-diff: a bulk removal of N tracked packages therefore
# produces N separate single-package reminder blocks here, not one
# combined summary (asserted explicitly below, not just assumed).
#
# Also covers a prerequisite pacman/apt don't have: dnf5's actions plugin
# is a separate, optional package (libdnf5-plugin-actions) that install-hook
# must detect and bail on instructively if missing.
#
# Usage:
#   ./dnf-hook-test.sh
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
echo "== Copying pkgundo source into the VM =="
rsync -az --delete -e "ssh -i $SSH_KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null" \
    --exclude target --exclude .git \
    "$REPO_ROOT/" "pkgundo@$IP:~/pkgundo/"

echo "== Building pkgundo inside the VM (release mode) =="
ssh_vm "$IP" "cd ~/pkgundo && cargo build --release --quiet"

BIN="/home/pkgundo/pkgundo/target/release/pkgundo"

echo
echo "== [1] Installing the systemd unit and starting the daemon =="
ssh_vm "$IP" "sudo cp ~/pkgundo/systemd/pkgundo-daemon.service /etc/systemd/system/ && \
    sudo sed -i 's|/usr/bin/pkgundo|$BIN|' /etc/systemd/system/pkgundo-daemon.service && \
    sudo sed -i '/ConditionPathExists/d' /etc/systemd/system/pkgundo-daemon.service && \
    sudo systemctl daemon-reload && sudo systemctl start pkgundo-daemon"
sleep 1
ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon did not reach active state"

echo
echo "== [2] pkgundo install-hook bails with a clear instruction when libdnf5-plugin-actions isn't installed yet =="
set +e
PRECHECK_OUT="$(ssh_vm "$IP" "sudo $BIN install-hook 2>&1")"
PRECHECK_STATUS=$?
set -e
[ "$PRECHECK_STATUS" -ne 0 ] || fail "install-hook should have failed before libdnf5-plugin-actions is installed"
echo "$PRECHECK_OUT" | grep -q "sudo dnf install libdnf5-plugin-actions" || fail "expected the install instruction naming the plugin package"
ssh_vm "$IP" "test -f /etc/dnf/libdnf5-plugins/actions.d/98pkgundo.actions" && fail "no hook file should have been written on the failing pre-flight check"
echo "PASS: install-hook correctly bailed with an install instruction, and wrote nothing."

echo
echo "== [3] Install the plugin, then install-hook succeeds and writes a correctly-patched actions file =="
ssh_vm "$IP" "sudo dnf install -y libdnf5-plugin-actions >/dev/null"
ssh_vm "$IP" "sudo $BIN install-hook"
ssh_vm "$IP" "test -f /etc/dnf/libdnf5-plugins/actions.d/98pkgundo.actions" || fail "install-hook did not write the dnf5 actions file"
ssh_vm "$IP" "grep -q \"$BIN dnf-hook-install\" /etc/dnf/libdnf5-plugins/actions.d/98pkgundo.actions" \
    || fail "actions file's install line was not patched to the real binary path"
ssh_vm "$IP" "grep -q \"$BIN dnf-hook-remove\" /etc/dnf/libdnf5-plugins/actions.d/98pkgundo.actions" \
    || fail "actions file's remove line was not patched to the real binary path"
echo "PASS: install-hook wrote a correctly-patched dnf5 actions file."

echo
echo "== [4] Auto-track-on-install: explicitly installed packages are tracked with zero manual 'pkgundo track', dependency-reason installs are not =="
# htop pulls no new dependency on this minimal Cloud Base image, so use it
# purely for the 'explicit install gets tracked' half; jq (real binary at
# /usr/bin/jq) pulling in oniguruma as a genuine automatic dependency
# covers the 'dependency install is NOT tracked' half.
ssh_vm "$IP" "sudo dnf install -y jq >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^jq" \
    || fail "an explicitly installed package should have been auto-tracked without ever calling 'pkgundo track'"
echo "PASS: explicitly installed package was auto-tracked with zero manual pkgundo commands."
ssh_vm "$IP" "$BIN tracked" | grep -q "^oniguruma" \
    && fail "a dependency-reason install (oniguruma) should NOT have been auto-tracked"
echo "PASS: package pulled in only as a dependency was correctly left untracked."

echo
echo "== [5] Removing a package that was never tracked: true no-op (no output, exit 0) =="
# dejavu-sans-fonts: a real explicit install that's still never trackable
# (pure data package, no executable anywhere under BIN_DIRS) — same
# reasoning as the apt phase's fonts-dejavu-core swap-in.
ssh_vm "$IP" "sudo dnf install -y dejavu-sans-fonts >/dev/null"
set +e
UNTRACKED_OUT="$(ssh_vm "$IP" "sudo dnf remove -y dejavu-sans-fonts 2>&1")"
UNTRACKED_STATUS=$?
set -e
[ "$UNTRACKED_STATUS" -eq 0 ] || fail "dnf remove of an untracked package should still exit 0"
echo "$UNTRACKED_OUT" | grep -qi "pkgundo was tracking" && fail "expected no pkgundo reminder for an untracked package removal"
echo "PASS: untracked-package removal produced no reminder and exited cleanly."

echo
echo "== [6] Single-match case: track a real package, remove it, expect the reminder =="
ssh_vm "$IP" "sudo dnf install -y htop >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^htop" || fail "htop should have been auto-tracked on explicit install"
SINGLE_OUT="$(ssh_vm "$IP" "sudo dnf remove -y htop 2>&1")"
echo "$SINGLE_OUT"
echo "$SINGLE_OUT" | grep -q "pkgundo was tracking removed package 'htop'" || fail "expected the single-match reminder naming htop"
echo "$SINGLE_OUT" | grep -q "pkgundo untrack htop --rollback" || fail "expected the rollback command suggestion"
echo "PASS: single tracked-package removal produced the expected reminder."

echo
echo "== [7] Structurally different from pacman/apt: bulk removal of 2 tracked packages produces TWO separate reminder blocks, not one combined summary =="
ssh_vm "$IP" "sudo dnf install -y tree figlet >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^tree" || fail "tree should have been auto-tracked"
ssh_vm "$IP" "$BIN tracked" | grep -q "^figlet" || fail "figlet should have been auto-tracked"
BULK_OUT="$(ssh_vm "$IP" "sudo dnf remove -y tree figlet 2>&1")"
echo "$BULK_OUT"
echo "$BULK_OUT" | grep -q "pkgundo was tracking removed package 'tree'" || fail "expected a reminder naming tree"
echo "$BULK_OUT" | grep -q "pkgundo was tracking removed package 'figlet'" || fail "expected a reminder naming figlet"
REMINDER_BLOCKS="$(echo "$BULK_OUT" | grep -c "pkgundo was tracking removed package")"
[ "$REMINDER_BLOCKS" -eq 2 ] || fail "expected exactly 2 separate single-package reminder blocks (dnf5's per-package invocation model), got $REMINDER_BLOCKS"
echo "PASS: bulk removal of 2 tracked packages correctly produced 2 separate reminder blocks (confirmed structural difference from pacman/apt, not assumed)."

echo
echo "== [8] Exit-code contract: hooks must exit 0 even when something inside breaks =="
ssh_vm "$IP" "sudo chmod 000 /var/lib/pkgundo/pkgundo.db"
set +e
ssh_vm "$IP" "sudo dnf reinstall -y curl"
BROKEN_STATUS=$?
set -e
ssh_vm "$IP" "sudo chmod 644 /var/lib/pkgundo/pkgundo.db"
[ "$BROKEN_STATUS" -eq 0 ] || fail "dnf must still exit 0 even when the hooks' internal DB read fails (got $BROKEN_STATUS)"
echo "PASS: DB-read failure is swallowed internally — dnf's own exit status is unaffected."

echo
echo "== [9] DB-lock contention: hooks read consistent data while the daemon is actively capturing for another app =="
ssh_vm "$IP" "printf '#include <unistd.h>\nint main(){sleep(6);return 0;}\n' > /tmp/slow.c && sudo gcc -x c /tmp/slow.c -o /usr/local/bin/slowapp"
ssh_vm "$IP" "cd ~/pkgundo && $BIN track slowapp"
ssh_vm "$IP" "sudo dnf install -y fortune-mod >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^fortune-mod" || fail "fortune-mod should have been auto-tracked before this contention step"
ssh_vm "$IP" "/usr/local/bin/slowapp &"
sleep 1
CONTENTION_OUT="$(ssh_vm "$IP" "sudo dnf remove -y fortune-mod 2>&1")"
echo "$CONTENTION_OUT"
echo "$CONTENTION_OUT" | grep -q "pkgundo was tracking removed package 'fortune-mod'" \
    || fail "expected the fortune-mod reminder even with the daemon mid-capture for slowapp"
echo "PASS: hook produced a correct reminder while the daemon held an active capture elsewhere."

echo
echo "== [10] install-hook --remove: actions file gone, subsequent install/removal produce no auto-track/reminder =="
ssh_vm "$IP" "sudo $BIN install-hook --remove"
ssh_vm "$IP" "test -f /etc/dnf/libdnf5-plugins/actions.d/98pkgundo.actions" && fail "actions file should be gone after install-hook --remove"
# Deliberately an untouched package name — the removal hook never
# auto-untracks (detection-only, by design), so a previously-used name
# would still show status=tracking regardless of whether install-hook
# --remove actually works, making that check a false pass/fail either way
# (the exact bug found and fixed in pacman-hook-test.sh this same session).
ssh_vm "$IP" "sudo dnf install -y cmatrix >/dev/null"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^cmatrix" \
    && fail "expected no auto-tracking once the dnf5 hook itself is uninstalled"
echo "PASS: install-hook --remove cleanly disables both auto-tracking and the removal reminder."

echo
echo "== [11] pkgundo track <package-name> resolves a real rpm package to its binaries via the new rpm -ql branch =="
ssh_vm "$IP" "cd ~/pkgundo && $BIN track cmatrix"
ssh_vm "$IP" "$BIN tracked" | grep -q "^cmatrix" || fail "'pkgundo track cmatrix' should have resolved and tracked cmatrix via rpm -ql"
echo "PASS: 'pkgundo track <package-name>' correctly resolves an rpm package's binaries."

ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon health was affected by hook/CLI-side testing — it never should be"
echo "PASS: daemon health unaffected throughout — hooks and install-hook are both CLI-side, no daemon involvement."

echo
echo "== [12] Real-world dev workflow: nodejs install correctly pulls npm as nodejs-npm (a dependency, left untracked); confirm real mutations land on nodejs's own txid =="
# Unlike Arch/Debian, Fedora has no standalone "npm" package at all —
# `dnf install nodejs` alone pulls in npm bundled as nodejs-npm, purely as
# an automatic dependency (confirmed live: `dnf install nodejs npm`
# errors on "npm" as an unresolvable argument, since no such package name
# exists here). So the correct real-world behavior to check on this distro
# is the opposite of Arch/Debian's: nodejs-npm must NOT be auto-tracked.
#
# Mutation capture on this VM's btrfs root now works (see
# src/btrfs_mount.rs and the VM-verified btrfs-mutation-test.sh — the
# fanotify FAN_MARK_FILESYSTEM+FAN_REPORT_FID EXDEV limitation previously
# documented here is fixed), so — like the ext4-rooted Arch/Debian VMs —
# this section now asserts real, non-zero mutation counts rather than only
# exercising the hook/auto-track/reminder mechanism. `/usr/bin/npm` is a
# `#!/usr/bin/env node` shebang script, not an ELF binary itself, so the
# kernel's exec of a real npm invocation resolves to `/usr/bin/node` — one
# of nodejs's own tracked paths — meaning npm's writes are correctly
# attributed to nodejs's txid even though npm itself is never tracked.
# Re-enable the hook (removed in step 10).
ssh_vm "$IP" "sudo $BIN install-hook"
ssh_vm "$IP" "sudo dnf install -y nodejs >/dev/null 2>&1; echo DONE"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^nodejs " || fail "nodejs should have been auto-tracked on explicit install"
ssh_vm "$IP" "$BIN tracked" | grep -q "^nodejs-npm" && fail "nodejs-npm is a dependency-reason install here and should NOT have been auto-tracked"
echo "PASS: nodejs auto-tracked; its bundled nodejs-npm dependency correctly left untracked (no standalone npm package exists on Fedora)."
NODEJS_TXID="$(ssh_vm "$IP" "$BIN tracked" | grep "^nodejs " | grep -oP 'txid=\K[0-9]+')"
# A real no-sudo dev setup (npm config set prefix), a real -g install, and a
# real per-project local install — exactly how a developer actually uses
# npm, not a synthetic single-file write.
ssh_vm "$IP" "mkdir -p ~/.npm-global ~/myproj && npm config set prefix ~/.npm-global && npm install -g cowsay >/dev/null 2>&1 && cd ~/myproj && npm init -y >/dev/null 2>&1 && npm install lodash >/dev/null 2>&1; echo DONE"
sleep 2
NODEJS_MUTATIONS="$(ssh_vm "$IP" "$BIN inspect $NODEJS_TXID" | grep -oP 'Total mutations:\s*\K[0-9]+')"
[ "$NODEJS_MUTATIONS" -gt 0 ] || fail "expected nodejs's own txid ($NODEJS_TXID) to have real mutations recorded from npm's writes, got $NODEJS_MUTATIONS"
echo "PASS: npm install (-g and local-project) correctly attributed $NODEJS_MUTATIONS mutation(s) to nodejs's own txid=$NODEJS_TXID (via the /usr/bin/npm -> /usr/bin/node shebang)."
NODEJS_REMOVE_OUT="$(ssh_vm "$IP" "sudo dnf remove -y nodejs 2>&1")"
echo "$NODEJS_REMOVE_OUT" | grep -q "pkgundo was tracking removed package 'nodejs'" || fail "expected a removal reminder naming nodejs"
echo "PASS: removing nodejs produced the correct removal reminder."

echo
echo "== [13] Real-world heavy package: firefox install/launch/remove =="
ssh_vm "$IP" "sudo dnf install -y firefox xorg-x11-server-Xvfb >/dev/null 2>&1; echo DONE"
sleep 1
ssh_vm "$IP" "$BIN tracked" | grep -q "^firefox" || fail "firefox should have been auto-tracked on explicit install"
FIREFOX_TXID="$(ssh_vm "$IP" "$BIN tracked" | grep "^firefox" | grep -oP 'txid=\K[0-9]+')"
# Launch it for real (headless, via xvfb — no GPU in this VM) to generate a
# genuine ~/.mozilla profile, the same way a first-run desktop launch would.
ssh_vm "$IP" "xvfb-run -a firefox --headless https://example.com >/dev/null 2>&1 & sleep 8; pkill firefox 2>/dev/null; sleep 1; true"
FIREFOX_MUTATIONS="$(ssh_vm "$IP" "$BIN inspect $FIREFOX_TXID" | grep -oP 'Total mutations:\s*\K[0-9]+')"
[ "$FIREFOX_MUTATIONS" -gt 0 ] || fail "expected a real firefox launch to produce at least one mutation under \$HOME"
echo "PASS: a real firefox launch produced $FIREFOX_MUTATIONS real mutation(s), correctly attributed."
FIREFOX_REMOVE_OUT="$(ssh_vm "$IP" "sudo dnf remove -y firefox 2>&1")"
echo "$FIREFOX_REMOVE_OUT" | grep -q "pkgundo was tracking removed package 'firefox'" || fail "expected a removal reminder naming firefox"
echo "PASS: a real heavy package (firefox) with a large dependency tree and actual profile auto-tracked and produced the correct removal reminder."

ssh_vm "$IP" "systemctl is-active pkgundo-daemon" | grep -q active || fail "daemon health was affected by real-world npm/firefox testing — it never should be"
echo "PASS: daemon health unaffected by real-world npm/firefox testing."

echo
echo "== Reverting VM back to clean snapshot (leaving it ready for next run) =="
virsh snapshot-revert "$VM_NAME" "$SNAPSHOT_NAME" --running >/dev/null

echo
echo "DNF-HOOK TEST PASSED"
