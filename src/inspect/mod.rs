use anyhow::Result;
use colored::Colorize;
use rusqlite::Connection;

use crate::archive::ArchiveManager;
use crate::journal::{get_mutations, summarize_mutations};
use crate::transaction::{load_all_transactions, load_transaction};

/// Inspect a specific transaction and print a detailed human-readable report
pub fn inspect_transaction(conn: &Connection, txid: i64) -> Result<()> {
    let tx = load_transaction(conn, txid)?;
    let mutations = get_mutations(conn, txid)?;
    let summary = summarize_mutations(&mutations);
    let archives = ArchiveManager::list_archives(conn, txid)?;

    println!();
    println!("{}", "╔══════════════════════════════════════════════╗".cyan());
    println!(
        "{} Transaction #{} — {}",
        "║".cyan(),
        tx.txid.to_string().yellow(),
        tx.command.white().bold()
    );
    println!("{}", "╚══════════════════════════════════════════════╝".cyan());
    println!();

    // Basic info
    println!("{}", "  ┌─ Metadata ──────────────────────────────────".blue());
    println!("  │  Status:          {}", format_status(&tx.status.as_str()));
    println!("  │  Package manager: {}", tx.package_manager.as_str().yellow());
    println!("  │  Started:         {}", tx.start_time.format("%Y-%m-%d %H:%M:%S UTC"));
    if let Some(end) = tx.end_time {
        let duration = end - tx.start_time;
        println!(
            "  │  Finished:        {} ({}s)",
            end.format("%Y-%m-%d %H:%M:%S UTC"),
            duration.num_seconds()
        );
    }
    if let Some(pid) = tx.pid_root {
        println!("  │  Root PID:        {}", pid);
    }
    println!("  └────────────────────────────────────────────────");
    println!();

    // Mutation summary
    println!("{}", "  ┌─ Mutation Summary ─────────────────────────".blue());
    println!("  │  Total mutations: {}", summary.total.to_string().yellow());
    println!("  │  Created:         {}", summary.created);
    println!("  │  Modified:        {}", summary.modified);
    println!("  │  Deleted:         {}", summary.deleted);
    println!("  │  Renamed:         {}", summary.renamed);
    println!("  └────────────────────────────────────────────────");
    println!();

    // File breakdown by category
    if !mutations.is_empty() {
        println!("{}", "  ┌─ Files by Category ────────────────────────".blue());
        let mut by_cat: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
        for m in &mutations {
            by_cat.entry(&m.file_category).or_default().push(&m.path);
        }
        for (cat, paths) in &by_cat {
            println!("  │  {} ({}):", cat.cyan(), paths.len());
            for path in paths.iter().take(5) {
                println!("  │    • {}", path);
            }
            if paths.len() > 5 {
                println!("  │    … and {} more", paths.len() - 5);
            }
        }
        println!("  └────────────────────────────────────────────────");
        println!();
    }

    // Archives
    if !archives.is_empty() {
        println!("{}", "  ┌─ Archived Files ───────────────────────────".blue());
        for a in &archives {
            let modified_tag = if a.modified_after_install {
                " [user-modified]".red().to_string()
            } else {
                String::new()
            };
            println!("  │  • {}{}", a.original_path, modified_tag);
            println!("  │      → {}", a.archive_path.dimmed());
            if let Ok(ts) = a.archived_at.parse::<chrono::DateTime<chrono::Utc>>() {
                println!(
                    "  │      archived: {}",
                    ts.format("%Y-%m-%d %H:%M:%S UTC").to_string().dimmed()
                );
            }
        }
        println!("  └────────────────────────────────────────────────");
        println!();
        println!(
            "  {} Recover with: {}",
            "💡".yellow(),
            format!("pkgundo recover {}", txid).white().bold()
        );
    }

    Ok(())
}

/// Show a chronological timeline of all transactions
pub fn show_timeline(conn: &Connection) -> Result<()> {
    let txs = load_all_transactions(conn)?;

    println!();
    println!("{}", "  pkgundo Transaction Timeline".white().bold());
    println!("{}", "  ════════════════════════════════════════════".cyan());

    if txs.is_empty() {
        println!("  No transactions recorded yet.");
        println!("  Run: {}", "sudo pkgundo run <command>".yellow());
        return Ok(());
    }

    for tx in &txs {
        let status_str = format_status(tx.status.as_str());
        let ts = tx.start_time.format("%Y-%m-%d %H:%M").to_string();
        println!(
            "  [{:>4}] {} │ {} │ {} │ {}",
            tx.txid.to_string().yellow(),
            ts.dimmed(),
            tx.package_manager.as_str().cyan(),
            status_str,
            tx.command.white()
        );
    }

    println!();
    println!(
        "  {} Inspect any transaction: {}",
        "→".green(),
        "pkgundo inspect <txid>".yellow()
    );
    Ok(())
}

/// Show current status: recent transactions + quick stats
pub fn show_status(conn: &Connection) -> Result<()> {
    let txs = load_all_transactions(conn)?;

    let total = txs.len();
    let running = txs.iter().filter(|t| t.status.as_str() == "running").count();
    let completed = txs.iter().filter(|t| t.status.as_str() == "completed").count();
    let rolled_back = txs.iter().filter(|t| t.status.as_str() == "rolled_back").count();
    let failed = txs.iter().filter(|t| t.status.as_str() == "failed").count();

    println!();
    println!("{}", "  pkgundo Status".white().bold());
    println!("{}", "  ══════════════════════════════".cyan());
    println!("  Total transactions:  {}", total.to_string().yellow());
    if running > 0 {
        println!("  Running:             {}", running.to_string().green());
    }
    println!("  Completed:           {}", completed);
    println!("  Rolled back:         {}", rolled_back.to_string().cyan());
    if failed > 0 {
        println!("  Failed:              {}", failed.to_string().red());
    }

    println!();
    println!("{}", "  Recent Transactions:".blue());
    for tx in txs.iter().take(5) {
        println!(
            "  [{:>4}] {} {}",
            tx.txid.to_string().yellow(),
            format_status(tx.status.as_str()),
            tx.command.dimmed()
        );
    }

    if total > 5 {
        println!("  … and {} more. Run {} to see all.", total - 5, "pkgundo timeline".yellow());
    }

    println!();
    println!("  Storage: {}", crate::db::PKGUNDO_DB_PATH.dimmed());
    Ok(())
}

fn format_status(status: &str) -> String {
    match status {
        "running" => "🔄 running    ".green().to_string(),
        "completed" => "✓ completed  ".green().to_string(),
        "rolled_back" => "↩ rolled_back".cyan().to_string(),
        "failed" => "✗ failed     ".red().to_string(),
        _ => status.to_string(),
    }
}
