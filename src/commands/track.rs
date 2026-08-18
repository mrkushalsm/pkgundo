use anyhow::Result;
use colored::Colorize;

use crate::daemon::client::send_request;
use crate::daemon::ipc::{Request, Response};

/// Handle `pkgundo track <app>`
pub async fn handle_track(app: &str) -> Result<()> {
    match send_request(Request::Track { name: app.to_string() }).await? {
        Response::Ok { message } => println!("{} {}", "✓".green(), message),
        Response::Error { message } => println!("{} {}", "✗".red(), message),
        other => println!("{} unexpected daemon response: {:?}", "✗".red(), other),
    }
    Ok(())
}

/// Handle `pkgundo untrack <app>`
pub async fn handle_untrack(app: &str) -> Result<()> {
    match send_request(Request::Untrack { name: app.to_string() }).await? {
        Response::Ok { message } => println!("{} {}", "✓".green(), message),
        Response::Error { message } => println!("{} {}", "✗".red(), message),
        other => println!("{} unexpected daemon response: {:?}", "✗".red(), other),
    }
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
