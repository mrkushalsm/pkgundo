use anyhow::Result;
use colored::Colorize;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::archive::ArchiveManager;
use crate::db;
use crate::scan_leftovers::{self, Confidence};
use crate::transaction;

/// Handle `pkgundo scan-leftovers <app> [--dry-run]`
pub fn handle_scan_leftovers(app: &str, dry_run: bool, db_path: &str) -> Result<()> {
    let mut candidates = scan_leftovers::scan_leftovers(app)?;
    candidates.sort_by(|a, b| b.confidence.cmp(&a.confidence));

    if candidates.is_empty() {
        println!("{} No leftover candidates found for '{}'.", "→".yellow(), app);
        return Ok(());
    }

    println!("Leftover candidates for '{}':", app.bold());
    for c in &candidates {
        println!("  [{}] {}", c.confidence.label().to_uppercase(), c.path.display());
    }

    if dry_run {
        println!("\n{} Dry run: nothing removed.", "→".yellow());
        return Ok(());
    }

    let conn = db::init_db(db_path)?;
    let txid = transaction::create_transaction(&conn, &format!("[scan-leftovers] {}", app), &[])?;
    let archive_mgr = ArchiveManager::new();

    for c in &candidates {
        print!(
            "Remove {} ({})? [y/N] ",
            c.path.display(),
            c.confidence.label()
        );
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            continue;
        }

        if let Err(e) = archive_and_remove(&archive_mgr, &conn, txid, &c.path) {
            println!("{} failed to remove {}: {}", "✗".red(), c.path.display(), e);
        } else {
            println!("{} removed {}", "✓".green(), c.path.display());
        }
    }

    Ok(())
}

fn archive_and_remove(
    archive_mgr: &ArchiveManager,
    conn: &rusqlite::Connection,
    txid: i64,
    path: &Path,
) -> Result<()> {
    if path.is_dir() {
        for entry in walkdir::WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                let p = entry.path().to_string_lossy().to_string();
                archive_mgr.archive_file(conn, txid, &p, false)?;
            }
        }
        fs::remove_dir_all(path)?;
    } else {
        archive_mgr.archive_file(conn, txid, &path.to_string_lossy(), false)?;
        fs::remove_file(path)?;
    }
    Ok(())
}
