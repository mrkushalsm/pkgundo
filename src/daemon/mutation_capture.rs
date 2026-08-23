//! Per-filesystem refcounted, lazily-started/stopped wrapper around one
//! shared `FanotifyMonitor`/`run_shared` mutation-capture group. See the
//! plan's "Key design decisions" for why this is per-filesystem (not a
//! single hardcoded `/` mark, and not one group per launch/user).

use anyhow::Result;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::btrfs_mount::BtrfsRootMounts;
use crate::ebpf::{ActiveLaunch, FanotifyMonitor};
use crate::journal::JournalMessage;

struct RunningGroup {
    monitor: Arc<FanotifyMonitor>,
    task: JoinHandle<()>,
}

struct Inner {
    /// Filesystem `st_dev` -> number of active launches currently on it.
    refcounts: HashMap<u64, usize>,
    group: Option<RunningGroup>,
}

pub struct MutationCapture {
    mutation_tx: mpsc::Sender<JournalMessage>,
    active_launches: Arc<Mutex<HashMap<i32, ActiveLaunch>>>,
    inner: Mutex<Inner>,
    btrfs_mounts: BtrfsRootMounts,
}

impl MutationCapture {
    pub fn new(
        mutation_tx: mpsc::Sender<JournalMessage>,
        active_launches: Arc<Mutex<HashMap<i32, ActiveLaunch>>>,
    ) -> Self {
        Self {
            mutation_tx,
            active_launches,
            inner: Mutex::new(Inner { refcounts: HashMap::new(), group: None }),
            btrfs_mounts: BtrfsRootMounts::new(),
        }
    }

    /// Best-effort teardown of any btrfs proxy mounts created this run —
    /// called once at daemon shutdown.
    pub fn btrfs_mounts_cleanup(&self) {
        self.btrfs_mounts.cleanup();
    }

    /// Block until every mutation already captured *as of this call* has
    /// been appended to the database — see `JournalMessage::Flush`'s doc.
    /// Works regardless of whether a capture group is currently running:
    /// the barrier travels through the same long-lived channel the
    /// daemon's one journal-writing task drains for its whole life, not
    /// through any particular filesystem's group.
    pub async fn flush(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.mutation_tx.send(JournalMessage::Flush(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    /// Arm `home`'s filesystem for mutation capture if it's the first active
    /// launch on it (creating the shared group's fanotify fd + read-loop
    /// task on the very first `start()` ever, across all filesystems).
    pub fn start(&self, home: &Path) -> Result<()> {
        let watch_path = self.btrfs_mounts.resolve(home)?;
        let dev = std::fs::metadata(&watch_path)?.dev();
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());

        if inner.group.is_none() {
            let monitor = Arc::new(FanotifyMonitor::try_new(0)?);
            let task = tokio::spawn(
                Arc::clone(&monitor).run_shared(self.mutation_tx.clone(), Arc::clone(&self.active_launches)),
            );
            inner.group = Some(RunningGroup { monitor, task });
        }

        if bump(&mut inner.refcounts, dev) {
            // Safe to unwrap: group was just ensured to exist above.
            inner.group.as_ref().unwrap().monitor.mark_filesystem(&watch_path.to_string_lossy(), true)?;
        }
        Ok(())
    }

    /// Release `home`'s filesystem reference; disarm its mark once nothing
    /// else references it, and tear down the whole shared group once no
    /// filesystem has any remaining reference (idle machines pay nothing).
    pub fn stop(&self, home: &Path) {
        let watch_path = match self.btrfs_mounts.resolve(home) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("MutationCapture::stop: couldn't resolve {}: {}", home.display(), e);
                return;
            }
        };
        let dev = match std::fs::metadata(&watch_path) {
            Ok(m) => m.dev(),
            Err(e) => {
                log::warn!("MutationCapture::stop: couldn't stat {}: {}", watch_path.display(), e);
                return;
            }
        };
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());

        if release(&mut inner.refcounts, dev) {
            if let Some(running) = &inner.group {
                if let Err(e) = running.monitor.mark_filesystem(&watch_path.to_string_lossy(), false) {
                    log::warn!("MutationCapture::stop: failed to disarm mark: {}", e);
                }
            }
        }

        if inner.refcounts.is_empty() {
            if let Some(running) = inner.group.take() {
                running.task.abort();
                log::info!("MutationCapture: no active launches remain, shared group stopped");
            }
        }
    }
}

/// Increment `dev`'s refcount, returning `true` if this was its first
/// reference (0→1 transition — caller should arm the mark). Pure bookkeeping,
/// decoupled from the actual (root-only) fanotify syscalls so it's
/// unit-testable without a real fanotify group.
fn bump(refcounts: &mut HashMap<u64, usize>, dev: u64) -> bool {
    let is_first = *refcounts.get(&dev).unwrap_or(&0) == 0;
    *refcounts.entry(dev).or_insert(0) += 1;
    is_first
}

/// Decrement `dev`'s refcount, returning `true` if it just hit zero (caller
/// should disarm the mark) — and removes the entry so `refcounts.is_empty()`
/// reliably reflects "no filesystem has any active launch."
fn release(refcounts: &mut HashMap<u64, usize>, dev: u64) -> bool {
    if let Some(count) = refcounts.get_mut(&dev) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            refcounts.remove(&dev);
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_arms_only_on_first_reference() {
        let mut refcounts = HashMap::new();
        assert!(bump(&mut refcounts, 42)); // 0 -> 1, first reference
        assert!(!bump(&mut refcounts, 42)); // 1 -> 2, not first
        assert!(!bump(&mut refcounts, 42)); // 2 -> 3, not first
        assert_eq!(refcounts.get(&42), Some(&3));
    }

    #[test]
    fn release_disarms_only_on_last_reference() {
        let mut refcounts = HashMap::new();
        bump(&mut refcounts, 42);
        bump(&mut refcounts, 42);
        assert!(!release(&mut refcounts, 42)); // 2 -> 1, not last
        assert!(release(&mut refcounts, 42)); // 1 -> 0, last -> disarm
        assert!(!refcounts.contains_key(&42)); // entry removed once at 0
    }

    #[test]
    fn distinct_filesystems_refcounted_independently() {
        let mut refcounts = HashMap::new();
        assert!(bump(&mut refcounts, 1)); // root fs, first
        assert!(bump(&mut refcounts, 2)); // separate /home fs, also first
        assert!(release(&mut refcounts, 1)); // root fs now empty
        assert!(refcounts.contains_key(&2)); // /home fs untouched
    }

    #[test]
    fn release_on_untracked_dev_is_a_safe_noop() {
        let mut refcounts = HashMap::new();
        assert!(!release(&mut refcounts, 999));
    }
}
