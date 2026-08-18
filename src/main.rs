mod archive;
mod blob_store;
mod classifier;
mod cli;
mod commands;
mod db;
mod ebpf;
mod fingerprint;
mod fs_monitor;
mod inspect;
mod journal;
mod process_tracker;
mod rollback;
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

    // Print banner
    print_banner();

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
