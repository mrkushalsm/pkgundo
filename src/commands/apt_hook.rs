//! Apt/dpkg auto-track + removal-reminder hook (`pkgundo apt-hook-pre` /
//! `pkgundo apt-hook-post`), the Debian/Ubuntu equivalent of the pacman
//! hooks in `commands::hook`.
//!
//! Unlike pacman's `NeedsTargets` (which pipes exactly the affected package
//! names to the hook's stdin), apt/dpkg has no equivalent mechanism —
//! `DPkg::Pre-Invoke`/`DPkg::Post-Invoke` just run an arbitrary command
//! before/after any dpkg-invoking transaction, with no package list passed
//! as args or stdin, and no install-vs-remove split. So this uses the
//! standard Debian-ecosystem snapshot-diff idiom instead: Pre-Invoke dumps
//! the full currently-installed package list to a state file; Post-Invoke
//! re-dumps it and diffs — this is why ONE hook file with a Pre+Post pair
//! (not two independently-triggered files like pacman's) covers both the
//! install-detection and removal-detection halves in a single Post-Invoke
//! pass.
//!
//! Same always-exit-0 contract as the pacman hooks, arguably higher-stakes
//! here: a `DPkg::Pre-Invoke` that exits non-zero can abort apt's entire
//! transaction, unlike pacman's `Exec`, which just warns and continues.

use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::commands::hook::{auto_track_new_installs, find_tracked_removed, print_removal_reminders};
use crate::db;

fn snapshot_path() -> PathBuf {
    Path::new(db::PKGUNDO_ROOT).join("apt-snapshot")
}

/// Every currently *installed* package name, one per line. Returns `None`
/// on any failure (e.g. `dpkg-query` not present) rather than propagating —
/// same fail-closed posture as the rest of this hook.
///
/// Deliberately filters on `${db:Status-Abbrev}` rather than just listing
/// `${Package}` — `dpkg-query -W` by default lists every package dpkg has
/// ever heard of, including ones in `rc` state (removed, config files still
/// present). Without this filter, a removed package never disappears from
/// the snapshot (dpkg still reports its name), so the diff would never see
/// it as removed and the removal-reminder half of this hook would silently
/// never fire.
fn current_package_list() -> Option<String> {
    let output = Command::new("dpkg-query")
        .args(["-W", "-f=${db:Status-Abbrev}|${Package}\n"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let installed: String = text
        .lines()
        .filter_map(|line| {
            let (status, pkg) = line.split_once('|')?;
            status.trim().starts_with("ii").then(|| pkg.to_string())
        })
        .fold(String::new(), |mut acc, pkg| {
            acc.push_str(&pkg);
            acc.push('\n');
            acc
        });
    Some(installed)
}

/// Entry point called from `main` for `pkgundo apt-hook-pre`. Every
/// internal failure is caught and logged rather than propagated — see the
/// module doc for why this is even more load-bearing than the pacman
/// hooks' own always-exit-0 contract.
pub fn handle_apt_hook_pre(db_path: &str) {
    if let Err(e) = run_apt_hook_pre(db_path) {
        log::warn!("pkgundo apt-hook-pre: {}", e);
    }
}

fn run_apt_hook_pre(db_path: &str) -> Result<()> {
    // Fires on every dpkg-invoking apt transaction — so the common case
    // (pkgundo never run here) must stay cheap, mirroring the pacman
    // hook's own skip check.
    if !Path::new(db_path).exists() {
        return Ok(());
    }
    let Some(listing) = current_package_list() else {
        return Ok(());
    };
    std::fs::write(snapshot_path(), listing).context("Failed to write apt package snapshot")?;
    Ok(())
}

/// Entry point called from `main` for `pkgundo apt-hook-post`. Same
/// always-exit-0 contract as `handle_apt_hook_pre`.
pub async fn handle_apt_hook_post(db_path: &str) {
    if let Err(e) = run_apt_hook_post(db_path).await {
        log::warn!("pkgundo apt-hook-post: {}", e);
    }
}

async fn run_apt_hook_post(db_path: &str) -> Result<()> {
    if !Path::new(db_path).exists() {
        return Ok(());
    }

    let snapshot = snapshot_path();
    // No snapshot means Pre-Invoke never ran for this transaction (e.g. the
    // hook file was only just installed) — nothing to diff against, not an
    // error.
    let before = match std::fs::read_to_string(&snapshot) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let Some(after) = current_package_list() else {
        return Ok(());
    };

    let (installed, removed) = diff_package_snapshots(&before, &after);

    // Always re-prime the snapshot for the *next* Pre/Post pair, whether or
    // not this pass found anything — self-heals a missed cycle instead of
    // leaving the file stale.
    let _ = std::fs::write(&snapshot, &after);

    if !removed.is_empty() {
        let conn = db::open_db_readonly(db_path)?;
        let removed_refs: Vec<&str> = removed.iter().map(String::as_str).collect();
        let hits = find_tracked_removed(&conn, &removed_refs)?;
        print_removal_reminders(&hits);
    }

    if !installed.is_empty() {
        // Only auto-track packages the user explicitly asked for — not
        // every transitive dependency pulled in alongside them. Computed
        // once per Post-Invoke (not once per package like pacman's
        // per-package `pacman -Qi`), since `apt-mark showmanual` already
        // returns the full list in one call.
        let explicit = explicitly_installed_packages();
        let already_tracked = db::open_db_readonly(db_path).ok();
        let installed_refs: Vec<&str> = installed.iter().map(String::as_str).collect();
        auto_track_new_installs(&installed_refs, already_tracked.as_ref(), |pkg| explicit.contains(pkg)).await?;
    }

    Ok(())
}

/// Parse two `dpkg-query -W -f='${Package}\n'`-style snapshots (one package
/// name per line) and return (newly_installed, removed), sorted, dedup'd.
/// Deliberately doesn't trust `dpkg-query`'s output order — parses into
/// `BTreeSet`s and takes the symmetric difference in each direction. Shaped
/// generically enough (any newline-per-package-name snapshot pair) that a
/// future dnf equivalent could reuse it unchanged.
fn diff_package_snapshots(before: &str, after: &str) -> (Vec<String>, Vec<String>) {
    let before: BTreeSet<&str> = before.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let after: BTreeSet<&str> = after.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let installed = after.difference(&before).map(|s| s.to_string()).collect();
    let removed = before.difference(&after).map(|s| s.to_string()).collect();
    (installed, removed)
}

/// Full `apt-mark showmanual` output → a lookup set of explicitly-installed
/// package names. Returns an empty set (never an error) on any failure —
/// same "fail closed, never track anything spuriously" posture as
/// `hook::is_explicitly_installed`'s own default-false fallback.
fn explicitly_installed_packages() -> HashSet<String> {
    Command::new("apt-mark")
        .arg("showmanual")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_showmanual_output(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

fn parse_showmanual_output(output: &str) -> HashSet<String> {
    output.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_detects_pure_installs() {
        let (installed, removed) = diff_package_snapshots("a\nb\n", "a\nb\nc\n");
        assert_eq!(installed, vec!["c".to_string()]);
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_detects_pure_removals() {
        let (installed, removed) = diff_package_snapshots("a\nb\nc\n", "a\nc\n");
        assert!(installed.is_empty());
        assert_eq!(removed, vec!["b".to_string()]);
    }

    #[test]
    fn diff_detects_simultaneous_install_and_removal() {
        // A real apt case pacman's two-file split never has to handle: one
        // transaction (e.g. dependency resolution swapping packages) both
        // installing and removing in the same Pre/Post pair.
        let (installed, removed) = diff_package_snapshots("a\nb\n", "a\nc\n");
        assert_eq!(installed, vec!["c".to_string()]);
        assert_eq!(removed, vec!["b".to_string()]);
    }

    #[test]
    fn diff_of_identical_snapshots_is_empty() {
        let (installed, removed) = diff_package_snapshots("a\nb\n", "a\nb\n");
        assert!(installed.is_empty());
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_handles_empty_before_snapshot() {
        // First-ever run: no prior snapshot content.
        let (installed, removed) = diff_package_snapshots("", "a\nb\n");
        assert_eq!(installed, vec!["a".to_string(), "b".to_string()]);
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_dedupes_repeated_lines() {
        let (installed, removed) = diff_package_snapshots("a\na\n", "a\na\nb\nb\n");
        assert_eq!(installed, vec!["b".to_string()]);
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_ignores_ordering_between_snapshots() {
        // dpkg-query's output order isn't relied on — a shuffled "after"
        // snapshot must diff identically to a sorted one.
        let (installed, removed) = diff_package_snapshots("b\na\nc\n", "c\nb\na\n");
        assert!(installed.is_empty());
        assert!(removed.is_empty());
    }

    #[test]
    fn showmanual_parsing_trims_and_skips_blank_lines() {
        let set = parse_showmanual_output("foo\n  bar  \n\nbaz\n");
        assert!(set.contains("foo"));
        assert!(set.contains("bar"));
        assert!(set.contains("baz"));
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn showmanual_parsing_of_empty_output_is_empty_set() {
        assert!(parse_showmanual_output("").is_empty());
    }
}
