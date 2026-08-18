use anyhow::Result;
use colored::Colorize;

use crate::rollback::{RollbackEngine, RollbackMode};

use super::is_root;

/// Handle `pkgundo rollback <txid>`
pub fn handle_rollback(txid: i64, mode: &str, dry_run: bool, db_path: &str) -> Result<()> {
    if !is_root() {
        eprintln!("{} pkgundo rollback requires root privileges.", "Error:".red());
        std::process::exit(1);
    }

    let rollback_mode = RollbackMode::from_str(mode);

    println!("{} Rolling back transaction {}", "→".yellow(), txid.to_string().cyan());
    println!("  Mode: {}", mode.yellow());
    if dry_run {
        println!("  {}", "[DRY RUN — no changes will be made]".yellow().bold());
    }

    if rollback_mode == RollbackMode::Nuclear {
        println!(
            "{}",
            "  ⚠ WARNING: Nuclear mode is aggressive. Proceed with caution.".red().bold()
        );
    }

    let engine = RollbackEngine::new(txid, rollback_mode, dry_run, db_path);
    let report = engine.execute()?;
    report.print_summary();

    Ok(())
}
