use anyhow::Result;
use colored::Colorize;
use rusqlite::Connection;

use crate::{db, inspect};

/// Open the pkgundo DB read-only, running `action` if it exists or printing
/// `not_found_msg` if pkgundo has never been run yet.
fn with_db_readonly(
    db_path: &str,
    not_found_msg: &str,
    action: impl FnOnce(&Connection) -> Result<()>,
) -> Result<()> {
    match db::open_db_readonly(db_path) {
        Ok(conn) => action(&conn),
        Err(_) => {
            println!("{}", not_found_msg);
            Ok(())
        }
    }
}

/// Handle `pkgundo inspect <txid>`
pub fn handle_inspect(txid: i64, db_path: &str) -> Result<()> {
    with_db_readonly(
        db_path,
        &format!(
            "{} No pkgundo database found. Run a command first: {}",
            "→".yellow(),
            "sudo pkgundo run <command>".yellow()
        ),
        |conn| inspect::inspect_transaction(conn, txid),
    )
}

/// Handle `pkgundo timeline`
pub fn handle_timeline(db_path: &str) -> Result<()> {
    with_db_readonly(
        db_path,
        &format!(
            "{} No transactions recorded yet. Run: {}",
            "→".yellow(),
            "sudo pkgundo run <command>".yellow()
        ),
        inspect::show_timeline,
    )
}

/// Handle `pkgundo status`
pub fn handle_status(db_path: &str) -> Result<()> {
    with_db_readonly(
        db_path,
        &format!(
            "{} pkgundo has not been used yet.\n  Start tracking: {}",
            "→".yellow(),
            "sudo pkgundo run <command>".yellow()
        ),
        inspect::show_status,
    )
}
