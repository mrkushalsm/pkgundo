use anyhow::Result;
use chrono::Utc;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::classifier;
use crate::journal::MutationRecord;

/// The type of filesystem operation observed
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MutationOperation {
    Create,
    Modify,
    Delete,
    Rename,
    Chmod,
    Unknown,
}

impl MutationOperation {
    pub fn as_str(&self) -> &str {
        match self {
            MutationOperation::Create => "create",
            MutationOperation::Modify => "modify",
            MutationOperation::Delete => "delete",
            MutationOperation::Rename => "rename",
            MutationOperation::Chmod => "chmod",
            MutationOperation::Unknown => "unknown",
        }
    }
}

/// Paths that should never be monitored (pkgundo's own storage, proc, sys, etc.)
fn is_excluded_path(path: &std::path::Path) -> bool {
    let excluded_prefixes = [
        "/proc",
        "/sys",
        "/dev",
        "/run/user",
        "/var/lib/pkgundo",
        "/tmp/.pkgundo",
    ];
    let path_str = path.to_string_lossy();
    excluded_prefixes.iter().any(|prefix| path_str.starts_with(prefix))
}

/// FsMonitor uses the `notify` crate (inotify backend on Linux) to watch the filesystem
/// for mutations during an active transaction.
pub struct FsMonitor {
    pub txid: i64,
    pub active_pids: Arc<Mutex<HashSet<i32>>>,
}

impl FsMonitor {
    pub fn new(txid: i64) -> Self {
        Self {
            txid,
            active_pids: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Start filesystem watching. Returns a watcher handle (must be kept alive) and
    /// a receiver channel that yields MutationRecord entries.
    pub fn start_watching(
        &self,
        mutation_tx: mpsc::Sender<MutationRecord>,
    ) -> Result<RecommendedWatcher> {
        let txid = self.txid;

        let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();

        let mut watcher = RecommendedWatcher::new(
            tx,
            Config::default().with_poll_interval(Duration::from_millis(100)),
        )?;

        // Watch the entire filesystem root — inotify events include path info
        // We selectively filter interesting paths
        let interesting_roots = vec![
            "/usr",
            "/etc",
            "/var",
            "/opt",
            "/home",
            "/lib",
            "/lib64",
            "/bin",
            "/sbin",
        ];

        for root in &interesting_roots {
            let path = std::path::Path::new(root);
            if path.exists() {
                let _ = watcher.watch(path, RecursiveMode::Recursive);
            }
        }

        let mutation_tx_clone = mutation_tx.clone();
        // Spawn a thread to process events from the sync channel
        std::thread::spawn(move || {
            for res in rx {
                match res {
                    Ok(event) => {
                        let operation = map_event_kind_to_operation(&event.kind);
                        if operation == MutationOperation::Unknown {
                            continue;
                        }

                        for path in &event.paths {
                            if is_excluded_path(path) {
                                continue;
                            }

                            let category = classifier::classify_path(path);
                            let now = Utc::now();

                            let record = MutationRecord {
                                id: None,
                                txid,
                                pid: None, // inotify does not give us PID info directly
                                operation: operation.as_str().to_string(),
                                path: path.to_string_lossy().to_string(),
                                timestamp: now,
                                file_category: format!("{:?}", category),
                                pre_hash: None,
                                post_hash: None,
                            };

                            let _ = mutation_tx_clone.blocking_send(record);
                        }
                    }
                    Err(e) => {
                        log::warn!("FsMonitor watch error: {}", e);
                    }
                }
            }
        });

        Ok(watcher)
    }
}

fn map_event_kind_to_operation(kind: &EventKind) -> MutationOperation {
    match kind {
        EventKind::Create(_) => MutationOperation::Create,
        EventKind::Modify(_) => MutationOperation::Modify,
        EventKind::Remove(_) => MutationOperation::Delete,
        _ => MutationOperation::Unknown,
    }
}
