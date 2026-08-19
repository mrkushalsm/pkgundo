use anyhow::{Context, Result};
use colored::Colorize;

use crate::daemon::client::send_request;
use crate::daemon::ipc::{Request, Response};
use crate::journal;
use crate::rollback::review::{group_mutations, review_groups_interactive};
use crate::rollback::{RollbackEngine, RollbackMode};

use super::is_root;

/// Handle `pkgundo track <app>`
pub async fn handle_track(app: &str) -> Result<()> {
    match send_request(Request::Track { name: app.to_string() }).await? {
        Response::Ok { message } => println!("{} {}", "✓".green(), message),
        Response::Error { message } => println!("{} {}", "✗".red(), message),
        other => println!("{} unexpected daemon response: {:?}", "✗".red(), other),
    }
    Ok(())
}

/// Handle `pkgundo untrack <app> [--rollback [--mode ...] [--dry-run]]`
///
/// `--dry-run` means nothing happens at all — same as every other dry-run
/// flag in this codebase — so it must skip the real `Untrack` IPC call too,
/// not just the file removal: that call has a real, permanent side effect
/// (flips the app to untracked), and running it during a "preview" would
/// leave the app silently untracked even though no files were touched.
pub async fn handle_untrack(
    app: &str,
    rollback: bool,
    mode: &str,
    dry_run: bool,
    db_path: &str,
) -> Result<()> {
    if rollback && !is_root() {
        eprintln!("{} pkgundo untrack --rollback requires root privileges.", "Error:".red());
        std::process::exit(1);
    }

    if rollback && dry_run {
        return preview_rollback(app, mode, db_path);
    }

    match send_request(Request::Untrack { name: app.to_string() }).await? {
        Response::Ok { message } => println!("{} {}", "✓".green(), message),
        Response::Error { message } => {
            println!("{} {}", "✗".red(), message);
            return Ok(());
        }
        other => {
            println!("{} unexpected daemon response: {:?}", "✗".red(), other);
            return Ok(());
        }
    }

    if !rollback {
        return Ok(());
    }

    // The daemon only owns the IPC/tracking side; rollback executes
    // CLI-side against the DB and filesystem directly, exactly like the
    // standalone `pkgundo rollback <txid>` command.
    let tracked = load_tracked(app, db_path)?;
    println!(
        "{} Rolling back accumulated $HOME mutations for '{}' (txid {})",
        "→".yellow(),
        app,
        tracked.txid.to_string().cyan()
    );

    let selected_groups = {
        let conn = crate::db::open_db_readonly(db_path).context("Failed to open pkgundo database")?;
        let mutations = journal::get_mutations(&conn, tracked.txid)?;
        let groups = group_mutations(&mutations);
        if groups.is_empty() {
            None
        } else {
            println!(
                "  {} {} group(s) of recorded mutations to review:",
                "→".yellow(),
                groups.len()
            );
            let selected = review_groups_interactive(&groups)?;
            Some(selected)
        }
    };

    let engine = RollbackEngine::new(tracked.txid, RollbackMode::from_str(mode), false, db_path)
        .with_home_cleanup(true)
        .with_selected_groups(selected_groups);
    let report = engine.execute()?;
    report.print_summary();

    Ok(())
}

/// `--rollback --dry-run`: preview only, no IPC call, no DB/filesystem
/// writes at all — the app stays tracked exactly as it was before.
fn preview_rollback(app: &str, mode: &str, db_path: &str) -> Result<()> {
    let tracked = load_tracked(app, db_path)?;
    println!(
        "{} Previewing rollback for '{}' (txid {})",
        "→".yellow(),
        app,
        tracked.txid.to_string().cyan()
    );
    println!("  {}", "[DRY RUN — no changes will be made, still tracked]".yellow().bold());

    let engine = RollbackEngine::new(tracked.txid, RollbackMode::from_str(mode), true, db_path)
        .with_home_cleanup(true);
    let report = engine.execute()?;
    report.print_summary();

    Ok(())
}

fn load_tracked(app: &str, db_path: &str) -> Result<crate::tracked_apps::TrackedApp> {
    let conn = crate::db::open_db_readonly(db_path).context("Failed to open pkgundo database")?;
    crate::tracked_apps::load_tracked_app(&conn, app)?
        .with_context(|| format!("'{}' has no recorded tracking history to roll back", app))
}

/// Handle `pkgundo tracked [--all]`
pub async fn handle_tracked(all: bool) -> Result<()> {
    match send_request(Request::ListTracked { all }).await? {
        Response::TrackedList { apps } => {
            if apps.is_empty() {
                println!("{} No tracked apps.", "→".yellow());
                return Ok(());
            }
            for app in apps {
                println!(
                    "{}  {} kind={} status={} package={} paths={:?}",
                    app.name.bold(),
                    format!("txid={}", app.txid).dimmed(),
                    app.kind,
                    app.status,
                    app.package_name.as_deref().unwrap_or("-"),
                    app.resolved_paths
                );
            }
        }
        Response::Error { message } => println!("{} {}", "✗".red(), message),
        other => println!("{} unexpected daemon response: {:?}", "✗".red(), other),
    }
    Ok(())
}
