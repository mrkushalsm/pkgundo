use anyhow::Result;
use colored::Colorize;

use crate::{archive, db};

use super::is_root;

/// Handle `pkgundo recover <txid>`
pub fn handle_recover(txid: i64, db_path: &str) -> Result<()> {
    if !is_root() {
        eprintln!("{} pkgundo recover requires root privileges.", "Error:".red());
        std::process::exit(1);
    }

    let conn = db::open_db(db_path)?;
    let archive_mgr = archive::ArchiveManager::new();

    println!("{} Recovering archives for transaction {}", "→".cyan(), txid.to_string().yellow());

    let recovered = archive_mgr.recover_archive(&conn, txid)?;

    if recovered.is_empty() {
        println!("  No archived files found for transaction {}.", txid);
    } else {
        println!("  {} files recovered:", recovered.len().to_string().green());
        for path in &recovered {
            println!("    {} {}", "✓".green(), path);
        }
    }

    Ok(())
}
