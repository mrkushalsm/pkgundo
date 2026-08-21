//! Pacman removal-hook detection (`pkgundo pacman-hook`) and the CLI helper
//! that installs/removes the hook file itself (`pkgundo install-hook`).
//!
//! The hook is pure detection: read the removed package names pacman feeds
//! it on stdin, look each one up against currently-tracked apps, print a
//! reminder. It never calls into rollback and has zero side effects on the
//! system beyond stdout — see the plan for why this stays decoupled from
//! the review UI in `rollback::review`.

use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use crate::daemon;
use crate::daemon::client::send_request;
use crate::daemon::ipc::{Request, Response};
use crate::db;
use crate::journal;
use crate::tracked_apps;

const HOOK_REMOVE_PATH: &str = "/etc/pacman.d/hooks/99-pkgundo-tracked.hook";
const HOOK_REMOVE_TEMPLATE: &str = include_str!("../../hooks/99-pkgundo-tracked.hook");
const HOOK_INSTALL_PATH: &str = "/etc/pacman.d/hooks/98-pkgundo-track-on-install.hook";
const HOOK_INSTALL_TEMPLATE: &str = include_str!("../../hooks/98-pkgundo-track-on-install.hook");

/// Entry point called from `main`. Every internal failure is caught and
/// logged rather than propagated — this must never make `pacman -R` itself
/// report a failure or hang, since it's a reminder-only side channel on
/// someone else's package removal.
pub fn handle_pacman_hook(db_path: &str) {
    if let Err(e) = run_pacman_hook(db_path) {
        log::warn!("pkgundo pacman-hook: {}", e);
    }
}

fn run_pacman_hook(db_path: &str) -> Result<()> {
    // Fires on every single `pacman -R`, tracked or not — so the common
    // case (pkgundo never run here, or nothing removed was tracked) must
    // stay cheap. No DB file at all means nothing was ever tracked; exit
    // before even trying to open anything.
    if !Path::new(db_path).exists() {
        return Ok(());
    }

    let mut names = String::new();
    std::io::stdin().read_to_string(&mut names)?;
    let package_names: Vec<&str> = names.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    if package_names.is_empty() {
        return Ok(());
    }

    let conn = db::open_db_readonly(db_path)?;

    let mut hits: Vec<(String, String, usize)> = Vec::new(); // (package, tracked name, mutation count)
    for pkg in package_names {
        if let Some(app) = tracked_apps::load_tracked_app_by_package(&conn, pkg)? {
            let count = journal::get_mutations(&conn, app.txid)?.len();
            hits.push((pkg.to_string(), app.name.clone(), count));
        }
    }

    if hits.is_empty() {
        return Ok(());
    }

    // One summary block covering every match in this transaction, not one
    // reminder per package — a bulk `pacman -Rs` removing several tracked
    // apps at once shouldn't wall-of-text the user's terminal.
    println!();
    if hits.len() == 1 {
        let (pkg, name, count) = &hits[0];
        println!(
            "{} pkgundo was tracking removed package '{}' ({} mutation(s) recorded under $HOME).",
            "→".yellow(),
            pkg,
            count
        );
        println!("  Review and roll back: {}", format!("pkgundo untrack {} --rollback", name).cyan());
        println!(
            "  Preview first:         {}",
            format!("pkgundo untrack {} --rollback --dry-run", name).cyan()
        );
    } else {
        println!("{} {} tracked apps were just removed:", "→".yellow(), hits.len());
        for (pkg, name, count) in &hits {
            println!("    {} ({} mutation(s) recorded under $HOME)", pkg, count);
            let _ = name;
        }
        println!("  Review and roll back each: {}", "pkgundo untrack <name> --rollback".cyan());
        println!("  Preview first:              {}", "pkgundo untrack <name> --rollback --dry-run".cyan());
    }
    println!();

    Ok(())
}

/// `pkgundo install-hook` / `pkgundo install-hook --remove`.
///
/// Manages both pacman hooks as one unit — auto-track-on-install and
/// remind-on-removal are two halves of the same "uninstall-aware cleanup"
/// feature, so there's one on/off switch for it, not two to remember.
pub fn handle_install_hook(remove: bool) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("pkgundo install-hook requires root privileges.");
    }

    if remove {
        for path in [HOOK_INSTALL_PATH, HOOK_REMOVE_PATH] {
            if Path::new(path).exists() {
                std::fs::remove_file(path).with_context(|| format!("Failed to remove {}", path))?;
                println!("{} Removed {}", "✓".green(), path);
            } else {
                println!("{} No hook installed at {}", "→".yellow(), path);
            }
        }
        return Ok(());
    }

    let exe = std::env::current_exe().context("Failed to resolve pkgundo's own executable path")?;
    let exe_str = exe.to_string_lossy();

    for (path, template) in [
        (HOOK_INSTALL_PATH, HOOK_INSTALL_TEMPLATE),
        (HOOK_REMOVE_PATH, HOOK_REMOVE_TEMPLATE),
    ] {
        let contents = template.replace("/usr/bin/pkgundo", &exe_str);
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create hook directory {}", parent.display()))?;
        }
        std::fs::write(path, contents).with_context(|| format!("Failed to write {}", path))?;
        println!("{} Installed pacman hook at {} (Exec = {})", "✓".green(), path, exe_str);
    }
    println!(
        "  From now on, {} will auto-track newly, explicitly installed packages, and\n  {} will print a reminder after removing one that's tracked.",
        "pacman -S".yellow(),
        "pacman -R".yellow()
    );

    Ok(())
}

/// Entry point called from `main` for `pkgundo pacman-hook-install`. Same
/// always-exit-0 contract as `handle_pacman_hook`: this fires on every
/// `pacman -S`, so an internal failure must never make an install itself
/// look like it failed.
pub async fn handle_pacman_hook_install(db_path: &str) {
    if let Err(e) = run_pacman_install_hook(db_path).await {
        log::warn!("pkgundo pacman-hook-install: {}", e);
    }
}

async fn run_pacman_install_hook(db_path: &str) -> Result<()> {
    // Fires on every single `pacman -S`, so the common case (pkgundo not
    // in use, or its daemon not running) must stay cheap. Tracking needs
    // the daemon's live exec-watch state, not just a DB write, so "daemon
    // not running" is a hard requirement here, not just an optimization —
    // check its socket before touching stdin/pacman/the DB at all.
    if !Path::new(daemon::SOCKET_PATH).exists() {
        return Ok(());
    }

    let mut names = String::new();
    std::io::stdin().read_to_string(&mut names)?;
    let package_names: Vec<&str> = names.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    if package_names.is_empty() {
        return Ok(());
    }

    // Only auto-track packages the user explicitly asked for — not every
    // transitive dependency pulled in alongside them, which would flood
    // `pkgundo tracked` and put a live exec-watch mark on things nobody
    // asked to have watched.
    let already_tracked = db::open_db_readonly(db_path).ok();

    for pkg in package_names {
        if !is_explicitly_installed(pkg) {
            continue;
        }
        if let Some(conn) = &already_tracked {
            if tracked_apps::load_tracked_app_by_package(conn, pkg)?.is_some() {
                continue;
            }
        }

        match send_request(Request::Track { name: pkg.to_string() }).await {
            Ok(Response::Ok { .. }) => {
                println!("{} pkgundo is now auto-tracking newly installed '{}'.", "→".yellow(), pkg);
            }
            Ok(Response::Error { message }) => {
                log::warn!("pkgundo pacman-hook-install: track '{}' failed: {}", pkg, message);
            }
            Ok(other) => {
                log::warn!("pkgundo pacman-hook-install: unexpected response tracking '{}': {:?}", pkg, other);
            }
            Err(e) => log::warn!("pkgundo pacman-hook-install: {}", e),
        }
    }

    Ok(())
}

/// True only for "Explicitly installed" — false for "Installed as a
/// dependency for another package" and for any package `pacman -Qi` can't
/// find (e.g. it was removed again before this hook got scheduled to run).
fn is_explicitly_installed(pkg: &str) -> bool {
    let output = match Command::new("pacman").args(["-Qi", pkg]).output() {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    parse_install_reason_explicit(&String::from_utf8_lossy(&output.stdout))
}

fn parse_install_reason_explicit(output: &str) -> bool {
    output
        .lines()
        .find(|l| l.trim_start().starts_with("Install Reason"))
        .map(|l| l.to_lowercase().contains("explicitly"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_reason_explicit_is_detected() {
        let output = "Name            : mpd\nInstall Reason  : Explicitly installed\nInstall Script  : No\n";
        assert!(parse_install_reason_explicit(output));
    }

    #[test]
    fn install_reason_dependency_is_not_explicit() {
        let output =
            "Name            : libmpdclient\nInstall Reason  : Installed as a dependency for another package\n";
        assert!(!parse_install_reason_explicit(output));
    }

    #[test]
    fn install_reason_missing_field_defaults_to_not_explicit() {
        assert!(!parse_install_reason_explicit("Name : foo\n"));
    }
}
