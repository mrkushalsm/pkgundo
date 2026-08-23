//! dnf5 auto-track + removal-reminder hook (`pkgundo dnf-hook-install <pkg>`
//! / `pkgundo dnf-hook-remove <pkg>`), the Fedora equivalent of the pacman
//! hooks in `commands::hook` and the apt/dpkg hooks in `commands::apt_hook`.
//!
//! dnf5's `libdnf5-plugin-actions` plugin is the mechanism here — a
//! declarative `.actions` file (see `hooks/98pkgundo.actions`) tells it to
//! run a command once per package that enters (`in`) or leaves (`out`) the
//! system in a transaction, substituting the package name directly via
//! `${pkg.name}`. Unlike pacman's stdin-delivered list or apt's
//! snapshot-diff, this hands us one package name per invocation — no state
//! file needed, but a bulk transaction affecting several tracked packages
//! means this hook fires (and prints its own reminder) once per package,
//! not once with a combined summary.
//!
//! dnf5's actions plugin runs after the transaction has already committed
//! (`post_transaction`), so — unlike apt's `Pre-Invoke` — a failing command
//! here can't abort or roll back anything. The same catch-and-log,
//! never-propagate contract is still followed for consistency with the
//! other two hooks.
//!
//! Discovered live via `/var/log/dnf5.log` (the reminder text never
//! appeared in `dnf remove`'s own terminal output, with no other visible
//! error): the actions plugin's default "plain" execution mode treats a
//! command's **stdout** as its own control channel — each line is parsed
//! as a `key=value` directive (`tmp.<var>=...`, `conf.<opt>=...`, etc.),
//! not forwarded to the user. Our ordinary `println!`-based reminder text
//! doesn't match that syntax, so every line was logged as a "Missing equal
//! sign" parse error and silently dropped rather than ever reaching the
//! terminal. stderr, by contrast, passes through untouched. Since
//! `find_tracked_removed`/`print_removal_reminders`/`auto_track_new_installs`
//! are shared with pacman/apt (which have no such stdout interception,
//! and must keep printing to real stdout), the fix lives here instead:
//! both entry points below redirect this process's own stdout to stderr
//! before calling into the shared functions, so the exact same `println!`
//! calls end up somewhere the actions plugin won't swallow.

use anyhow::{Context, Result};
use std::process::Command;

use crate::commands::hook::{auto_track_new_installs, find_tracked_removed, print_removal_reminders};
use crate::db;

/// Redirects this process's stdout to stderr (fd 1 -> fd 2) so that
/// `println!` output — otherwise misparsed and dropped by dnf5's actions
/// plugin — reaches the user's terminal via stderr instead. See the module
/// doc for why. Safe: called once, early, in a short-lived process that
/// does nothing else with fd 1 afterward.
fn redirect_stdout_to_stderr() {
    unsafe {
        libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO);
    }
}

/// Entry point called from `main` for `pkgundo dnf-hook-install <pkg>`.
pub async fn handle_dnf_hook_install(db_path: &str, package: &str) {
    redirect_stdout_to_stderr();
    if let Err(e) = run_dnf_hook_install(db_path, package).await {
        log::warn!("pkgundo dnf-hook-install: {}", e);
    }
}

async fn run_dnf_hook_install(db_path: &str, package: &str) -> Result<()> {
    if !std::path::Path::new(db_path).exists() {
        return Ok(());
    }
    if !is_explicitly_installed_dnf(package) {
        return Ok(());
    }
    let already_tracked = db::open_db_readonly(db_path).ok();
    // The explicit-install check already happened above, so the shared
    // `is_explicit` closure here is trivially true — same pattern apt/pacman
    // use, just fed a one-element slice instead of a batch.
    auto_track_new_installs(&[package], already_tracked.as_ref(), |_| true).await
}

/// Entry point called from `main` for `pkgundo dnf-hook-remove <pkg>`.
pub fn handle_dnf_hook_remove(db_path: &str, package: &str) {
    redirect_stdout_to_stderr();
    if let Err(e) = run_dnf_hook_remove(db_path, package) {
        log::warn!("pkgundo dnf-hook-remove: {}", e);
    }
}

fn run_dnf_hook_remove(db_path: &str, package: &str) -> Result<()> {
    if !std::path::Path::new(db_path).exists() {
        return Ok(());
    }
    let conn = db::open_db_readonly(db_path).context("Failed to open pkgundo db")?;
    let hits = find_tracked_removed(&conn, &[package])?;
    print_removal_reminders(&hits);
    Ok(())
}

/// True only for a package `dnf5 repoquery` reports as installed for
/// reason `user` (i.e. explicitly requested, not pulled in as a
/// dependency). Fails closed to `false` on any error — same posture as
/// `hook::is_explicitly_installed`'s own default-false fallback. A
/// per-package on-demand query, not a batch — natural given the hook
/// itself already fires once per package.
fn is_explicitly_installed_dnf(pkg: &str) -> bool {
    let output = match Command::new("dnf5")
        .args(["repoquery", "--installed", "--qf", "%{name} %{reason}\n", pkg])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    parse_repoquery_reason(&String::from_utf8_lossy(&output.stdout), pkg)
}

/// Parses `dnf5 repoquery --qf '%{name} %{reason}\n'`-style output and
/// returns whether the named package's reason is `user` (case-insensitive —
/// the exact set of valid reason strings isn't authoritatively documented,
/// so this checks defensively rather than assuming exact casing).
fn parse_repoquery_reason(output: &str, pkg: &str) -> bool {
    output
        .lines()
        .find_map(|line| {
            let (name, reason) = line.trim().split_once(' ')?;
            (name == pkg).then(|| reason.trim().eq_ignore_ascii_case("user"))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repoquery_reason_user_is_explicit() {
        assert!(parse_repoquery_reason("htop user\n", "htop"));
    }

    #[test]
    fn repoquery_reason_dependency_is_not_explicit() {
        assert!(!parse_repoquery_reason("libjq1 dependency\n", "libjq1"));
    }

    #[test]
    fn repoquery_reason_is_case_insensitive() {
        assert!(parse_repoquery_reason("htop User\n", "htop"));
        assert!(parse_repoquery_reason("htop USER\n", "htop"));
    }

    #[test]
    fn repoquery_reason_missing_package_defaults_to_not_explicit() {
        assert!(!parse_repoquery_reason("", "htop"));
        assert!(!parse_repoquery_reason("otherpkg user\n", "htop"));
    }

    #[test]
    fn repoquery_reason_group_and_weak_are_not_explicit() {
        assert!(!parse_repoquery_reason("foo group\n", "foo"));
        assert!(!parse_repoquery_reason("foo weak\n", "foo"));
    }
}
