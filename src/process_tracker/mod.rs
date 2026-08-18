use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashSet;
use std::fs;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;

/// A single entry in the process attribution table
#[derive(Debug, Clone)]
pub struct ProcessEntry {
    pub pid: i32,
    pub ppid: i32,
    pub name: String,
    pub txid: i64,
}

/// ProcessTracker watches a root PID's entire descendant tree.
/// It periodically polls /proc to discover child processes and record them.
pub struct ProcessTracker {
    pub root_pid: i32,
    pub txid: i64,
    pub known_pids: HashSet<i32>,
}

impl ProcessTracker {
    pub fn new(root_pid: i32, txid: i64) -> Self {
        let mut known_pids = HashSet::new();
        known_pids.insert(root_pid);
        Self {
            root_pid,
            txid,
            known_pids,
        }
    }

    /// Get the parent PID of a given PID by reading /proc/<pid>/status
    pub fn get_ppid(pid: i32) -> Option<i32> {
        let status_path = format!("/proc/{}/status", pid);
        let content = fs::read_to_string(&status_path).ok()?;
        for line in content.lines() {
            if line.starts_with("PPid:") {
                let ppid_str = line.split_whitespace().nth(1)?;
                return ppid_str.parse::<i32>().ok();
            }
        }
        None
    }

    /// Get the process name from /proc/<pid>/comm
    pub fn get_process_name(pid: i32) -> String {
        let comm_path = format!("/proc/{}/comm", pid);
        fs::read_to_string(&comm_path)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| format!("pid_{}", pid))
    }

    /// Check if a given PID is a descendant of our root_pid
    pub fn is_descendant(&self, pid: i32) -> bool {
        let mut current = pid;
        let mut visited = HashSet::new();
        loop {
            if current == self.root_pid {
                return true;
            }
            if visited.contains(&current) || current <= 1 {
                return false;
            }
            visited.insert(current);
            match Self::get_ppid(current) {
                Some(ppid) => current = ppid,
                None => return false,
            }
        }
    }

    /// Scan /proc for all PIDs that are descendants of root_pid
    pub fn scan_descendants(&self) -> Vec<i32> {
        let mut descendants = Vec::new();
        if let Ok(proc_entries) = fs::read_dir("/proc") {
            for entry in proc_entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if let Ok(pid) = name_str.parse::<i32>() {
                    if pid != self.root_pid && self.is_descendant(pid) {
                        descendants.push(pid);
                    }
                }
            }
        }
        descendants
    }

    /// Record a PID entry to the database
    pub fn record_pid(conn: &Connection, entry: &ProcessEntry) -> Result<()> {
        conn.execute(
            "INSERT OR IGNORE INTO process_tree (pid, ppid, name, txid) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![entry.pid, entry.ppid, entry.name, entry.txid],
        )
        .context("Failed to record process entry")?;
        Ok(())
    }

}

/// Monitor a process tree for a given root PID, recording all new descendants to the DB.
/// Runs in a background tokio task until the root process exits.
/// Sends discovered PIDs to the provided channel.
pub async fn watch_process_tree(
    root_pid: i32,
    txid: i64,
    db_path: String,
    pid_tx: mpsc::Sender<i32>,
) {
    let mut tracker = ProcessTracker::new(root_pid, txid);
    let poll_interval = Duration::from_millis(50);

    // Record root PID first
    let root_name = ProcessTracker::get_process_name(root_pid);
    log::debug!(
        "ProcessTracker: monitoring root PID {} ({}) for txid={}",
        root_pid,
        root_name,
        txid
    );

    let _ = pid_tx.send(root_pid).await;

    loop {
        // Check if root is still alive
        let root_alive = fs::metadata(format!("/proc/{}", root_pid)).is_ok();
        if !root_alive {
            log::debug!("ProcessTracker: root PID {} exited", root_pid);
            break;
        }

        // Scan for new descendants
        let descendants = tracker.scan_descendants();
        for pid in descendants {
            if tracker.known_pids.insert(pid) {
                // New PID discovered
                let ppid = ProcessTracker::get_ppid(pid).unwrap_or(root_pid);
                let name = ProcessTracker::get_process_name(pid);
                log::debug!(
                    "ProcessTracker: discovered child PID {} ({}) under txid={}",
                    pid,
                    name,
                    txid
                );

                // Record to DB
                if let Ok(conn) = Connection::open(&db_path) {
                    let entry = ProcessEntry {
                        pid,
                        ppid,
                        name,
                        txid,
                    };
                    let _ = ProcessTracker::record_pid(&conn, &entry);
                }

                let _ = pid_tx.send(pid).await;
            }
        }

        sleep(poll_interval).await;
    }
}
