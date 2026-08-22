mod archive;
mod blob_store;
mod classifier;
mod cli;
mod commands;
mod daemon;
mod db;
mod ebpf;
mod fingerprint;
mod fs_monitor;
mod inspect;
mod journal;
mod process_tracker;
mod rollback;
mod scan_leftovers;
mod service_tracker;
mod tracked_apps;
mod transaction;
mod user_tracker;

use anyhow::Result;
use clap::Parser;
use colored::Colorize;

use cli::{Cli, Commands};
use db::PKGUNDO_DB_PATH;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging. RUST_LOG=debug for verbose output.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    // Print banner — except for the pacman hook, whose output lands inline
    // in someone's `pacman -R` terminal and has no business showing an
    // ASCII banner there.
    if !matches!(
        cli.command,
        Commands::PacmanHook | Commands::PacmanHookInstall | Commands::AptHookPre | Commands::AptHookPost
    ) {
        print_banner();
    }

    // Initialize DB (creates schema if not exists)
    // For non-run commands, we use a read-write connection too (for status updates etc.)
    let db_path = PKGUNDO_DB_PATH;

    match &cli.command {
        Commands::Run { args, mode } => {
            commands::run::handle_run(args, mode, db_path).await?;
        }
        Commands::Rollback { txid, mode, dry_run } => {
            commands::rollback::handle_rollback(*txid, mode, *dry_run, db_path)?;
        }
        Commands::Inspect { txid } => {
            commands::query::handle_inspect(*txid, db_path)?;
        }
        Commands::Timeline => {
            commands::query::handle_timeline(db_path)?;
        }
        Commands::Status => {
            commands::query::handle_status(db_path)?;
        }
        Commands::Recover { txid } => {
            commands::recover::handle_recover(*txid, db_path)?;
        }
        Commands::Simulate { args } => {
            commands::simulate::handle_simulate(args)?;
        }
        Commands::Track { app } => {
            commands::track::handle_track(app).await?;
        }
        Commands::Untrack { app, rollback, mode, dry_run } => {
            commands::track::handle_untrack(app, *rollback, mode, *dry_run, db_path).await?;
        }
        Commands::Tracked { all } => {
            commands::track::handle_tracked(*all).await?;
        }
        Commands::ScanLeftovers { app, dry_run } => {
            commands::scan_leftovers::handle_scan_leftovers(app, *dry_run, db_path)?;
        }
        Commands::Daemon => {
            daemon::run_daemon(db_path).await?;
        }
        Commands::PacmanHook => {
            // Contract: this command's exit code is always 0 — see
            // handle_pacman_hook's doc. Any internal error is caught and
            // logged there, never propagated up through this `?`.
            commands::hook::handle_pacman_hook(db_path);
        }
        Commands::PacmanHookInstall => {
            // Same always-exit-0 contract as PacmanHook — this fires on
            // every `pacman -S`, so it must never make an install look
            // like it failed.
            commands::hook::handle_pacman_hook_install(db_path).await;
        }
        Commands::AptHookPre => {
            // Same always-exit-0 contract as PacmanHook — arguably more
            // load-bearing here, since a failing DPkg::Pre-Invoke can
            // abort apt's whole transaction (unlike pacman's Exec).
            commands::apt_hook::handle_apt_hook_pre(db_path);
        }
        Commands::AptHookPost => {
            commands::apt_hook::handle_apt_hook_post(db_path).await;
        }
        Commands::InstallHook { remove } => {
            commands::hook::handle_install_hook(*remove)?;
        }
    }

    Ok(())
}

fn print_banner() {
    println!();
    println!("{}", "  ██████╗ ██╗  ██╗ ██████╗ ██╗   ██╗███╗   ██╗██████╗  ██████╗".cyan());
    println!("{}", "  ██╔══██╗██║ ██╔╝██╔════╝ ██║   ██║████╗  ██║██╔══██╗██╔═══██╗".cyan());
    println!("{}", "  ██████╔╝█████╔╝ ██║  ███╗██║   ██║██╔██╗ ██║██║  ██║██║   ██║".cyan());
    println!("{}", "  ██╔═══╝ ██╔═██╗ ██║   ██║██║   ██║██║╚██╗██║██║  ██║██║   ██║".cyan());
    println!("{}", "  ██║     ██║  ██╗╚██████╔╝╚██████╔╝██║ ╚████║██████╔╝╚██████╔╝".cyan());
    println!("{}", "  ╚═╝     ╚═╝  ╚═╝ ╚═════╝  ╚═════╝ ╚═╝  ╚═══╝╚═════╝  ╚═════╝".cyan());
    println!(
        "  {}  v0.1.0  {}",
        "Universal Linux Transaction Monitor".white().bold(),
        "& Intelligent Rollback System".dimmed()
    );
    println!();
}
