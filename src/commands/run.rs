use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::{blob_store, db, ebpf, fs_monitor, journal, process_tracker, service_tracker, transaction, user_tracker};

use super::is_root;

/// Handle `pkgundo run <command>` — the main monitoring path
pub async fn handle_run(args: &[String], _mode: &str, db_path: &str) -> Result<()> {
    if args.is_empty() {
        bail!("No command specified. Usage: pkgundo run <command> [args...]");
    }

    if !is_root() {
        eprintln!("{} pkgundo run requires root privileges. Please use sudo.", "Error:".red());
        std::process::exit(1);
    }

    let conn = db::init_db(db_path)?;
    let command_str = args.join(" ");

    println!("{} Monitoring: {}", "→".green(), command_str.yellow());

    // ── Phase 10: Detect kernel capabilities ─────────────────────────────────
    let caps = ebpf::KernelCapabilities::detect();
    if caps.fanotify_available {
        println!("  {} Phase 10: fanotify available — PID attribution enabled", "ℹ".cyan());
    }

    // ── Create transaction ────────────────────────────────────────────────────
    let txid = transaction::create_transaction(&conn, &command_str, args)?;
    println!("  Transaction ID: {}", txid.to_string().cyan());

    // ── Phase 9: Pre-scan configs for blob storage ────────────────────────────
    println!("  Pre-scanning config files for snapshot...");
    match blob_store::pre_scan_configs(&conn, txid) {
        Ok(n) => println!("  Snapshotted {} config files", n.to_string().green()),
        Err(e) => log::warn!("Pre-scan error: {}", e),
    }

    // ── Phase 9: Snapshot user/group state ────────────────────────────────────
    let user_snapshot_pre = user_tracker::UserGroupSnapshot::capture().ok();
    if let Some(ref snap) = user_snapshot_pre {
        let _ = user_tracker::store_snapshot(&conn, txid, "pre", snap);
    }

    // ── Setup mutation channel ────────────────────────────────────────────────
    let (mutation_tx, mut mutation_rx) = mpsc::channel::<journal::MutationRecord>(4096);

    // ── Phase 10: Try fanotify monitor; fall back to inotify ─────────────────
    let using_fanotify = ebpf::start_enhanced_monitor(txid, mutation_tx.clone()).await;
    let _watcher = if !using_fanotify {
        let monitor = fs_monitor::FsMonitor::new(txid);
        let active_pids = Arc::clone(&monitor.active_pids);
        let w = monitor.start_watching(mutation_tx.clone())?;
        Some((w, active_pids))
    } else {
        None
    };

    // Keep a separate inotify watcher alive in the non-fanotify path
    let monitor_for_pids = if _watcher.is_none() {
        None
    } else {
        _watcher.as_ref().map(|(_, pids)| Arc::clone(pids))
    };

    // ── Setup process tracker channel ─────────────────────────────────────────
    let (pid_tx, mut pid_rx) = mpsc::channel::<i32>(256);

    // ── Launch the target command ─────────────────────────────────────────────
    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .spawn()
        .context(format!("Failed to launch command: {}", args[0]))?;

    let root_pid = child.id() as i32;
    println!("  Root PID: {}", root_pid.to_string().cyan());

    let conn_for_update = db::open_db(db_path)?;
    transaction::update_transaction_status(
        &conn_for_update, txid, transaction::TransactionStatus::Running, Some(root_pid),
    )?;

    if let Some(ref active_pids) = monitor_for_pids {
        active_pids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(root_pid);
    }

    // Start process tree watcher
    let db_path_clone = db_path.to_string();
    let pid_tx_clone = pid_tx.clone();
    let pid_task = tokio::spawn(async move {
        process_tracker::watch_process_tree(root_pid, txid, db_path_clone, pid_tx_clone).await;
    });

    // Collect new PIDs and add to monitor
    let active_pids_opt = monitor_for_pids.clone();
    let pid_collector = tokio::spawn(async move {
        while let Some(pid) = pid_rx.recv().await {
            if let Some(ref ap) = active_pids_opt {
                ap.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(pid);
            }
        }
    });

    // Collect mutation events and write to journal
    let db_path_for_journal = db_path.to_string();
    let journal_task = tokio::spawn(async move {
        let conn = match db::open_db(&db_path_for_journal) {
            Ok(c) => c,
            Err(e) => { log::error!("DB open failed: {}", e); return; }
        };
        while let Some(record) = mutation_rx.recv().await {
            if let Err(e) = journal::append_mutation(&conn, &record) {
                log::debug!("Journal dedup: {}", e);
            }
        }
    });

    // ── Wait for child process to complete ────────────────────────────────────
    let status = child.wait().context("Failed to wait for child process")?;

    pid_task.abort();
    pid_collector.abort();
    drop(mutation_tx);

    tokio::time::timeout(std::time::Duration::from_secs(2), journal_task).await.ok();

    // ── Phase 9: Post-transaction analysis ───────────────────────────────────
    // Compare user/group state to detect additions
    if let Some(pre_snap) = user_snapshot_pre {
        if let Ok(post_snap) = user_tracker::UserGroupSnapshot::capture() {
            let _ = user_tracker::store_snapshot(&conn, txid, "post", &post_snap);
            let events = user_tracker::diff_snapshots(txid, &pre_snap, &post_snap);
            if !events.is_empty() {
                log::info!("UserTracker: detected {} user/group changes", events.len());
                let _ = user_tracker::record_user_events(&conn, &events);
            }
        }
    }

    // Detect service changes from process tree cmdlines
    if let Ok(mut stmt) = conn.prepare("SELECT pid FROM process_tree WHERE txid = ?1") {
        let pids: Vec<i32> = match stmt.query_map(rusqlite::params![txid], |r| r.get::<_, i32>(0)) {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => Vec::new(),
        };
        let _ = service_tracker::detect_service_changes_from_pids(&conn, txid, &pids);
    }

    // ── Finalize transaction ──────────────────────────────────────────────────
    let final_status = if status.success() {
        transaction::TransactionStatus::Completed
    } else {
        transaction::TransactionStatus::Failed
    };

    let conn_final = db::open_db(db_path)?;
    transaction::update_transaction_status(&conn_final, txid, final_status.clone(), None)?;

    println!();
    match final_status {
        transaction::TransactionStatus::Completed => {
            println!("{} Command completed successfully.", "✓".green());
        }
        transaction::TransactionStatus::Failed => {
            println!("{} Command exited with error code {:?}", "✗".red(), status.code());
        }
        _ => {}
    }

    println!(
        "  Transaction {} recorded. Use {} to inspect.",
        txid.to_string().cyan(),
        format!("pkgundo inspect {}", txid).yellow()
    );
    println!("  To roll back: {}", format!("sudo pkgundo rollback {}", txid).yellow());

    Ok(())
}
