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

use crate::db;
use crate::journal;
use crate::tracked_apps;

const HOOK_PATH: &str = "/etc/pacman.d/hooks/99-pkgundo-tracked.hook";
const HOOK_TEMPLATE: &str = include_str!("../../hooks/99-pkgundo-tracked.hook");

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
pub fn handle_install_hook(remove: bool) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("pkgundo install-hook requires root privileges.");
    }

    if remove {
        if Path::new(HOOK_PATH).exists() {
            std::fs::remove_file(HOOK_PATH)
                .with_context(|| format!("Failed to remove {}", HOOK_PATH))?;
            println!("{} Removed {}", "✓".green(), HOOK_PATH);
        } else {
            println!("{} No hook installed at {}", "→".yellow(), HOOK_PATH);
        }
        return Ok(());
    }

    let exe = std::env::current_exe().context("Failed to resolve pkgundo's own executable path")?;
    let exe_str = exe.to_string_lossy();
    let contents = HOOK_TEMPLATE.replace("/usr/bin/pkgundo", &exe_str);

    if let Some(parent) = Path::new(HOOK_PATH).parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create hook directory {}", parent.display()))?;
    }
    std::fs::write(HOOK_PATH, contents).with_context(|| format!("Failed to write {}", HOOK_PATH))?;
    println!("{} Installed pacman hook at {} (Exec = {})", "✓".green(), HOOK_PATH, exe_str);
    println!(
        "  From now on, {} will print a reminder after removing a tracked package.",
        "pacman -R".yellow()
    );

    Ok(())
}
