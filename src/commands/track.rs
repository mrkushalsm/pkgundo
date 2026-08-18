use anyhow::{Context, Result};
use colored::Colorize;

use crate::daemon::client::send_request;
use crate::daemon::ipc::{Request, Response};
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
    let conn = crate::db::open_db_readonly(db_path).context("Failed to open pkgundo database")?;
    let tracked = crate::tracked_apps::load_tracked_app(&conn, app)?
        .with_context(|| format!("'{}' has no recorded tracking history to roll back", app))?;
    drop(conn);

    println!(
        "{} Rolling back accumulated $HOME mutations for '{}' (txid {})",
        "→".yellow(),
        app,
        tracked.txid.to_string().cyan()
    );
    if dry_run {
        println!("  {}", "[DRY RUN — no changes will be made]".yellow().bold());
    }

    let engine =
        RollbackEngine::new(tracked.txid, RollbackMode::from_str(mode), dry_run, db_path)
            .with_home_cleanup(true);
    let report = engine.execute()?;
    report.print_summary();

    Ok(())
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
