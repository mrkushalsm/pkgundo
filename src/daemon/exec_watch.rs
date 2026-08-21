//! Detects every execution of a tracked app's binary via a dedicated
//! `FAN_OPEN_EXEC` fanotify group (per-file marks, not filesystem-wide —
//! `FAN_OPEN_EXEC` fires on the marked *inode* specifically, so this stays
//! narrow regardless of how many apps are tracked). On a match, spawns a
//! process-tree tracker for that launch and registers it with the shared
//! `MutationCapture` group so its `$HOME` writes get captured into the
//! tracked app's bucket transaction.

use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::ebpf::ActiveLaunch;
use crate::daemon::mutation_capture::MutationCapture;

const FAN_CLASS_NOTIF: u32 = 0x0000_0000;
const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_NONBLOCK: u32 = 0x0000_0002;
const FAN_OPEN_EXEC: u64 = 0x0000_1000;
const FAN_MARK_ADD: u32 = 0x0000_0001;
const FAN_MARK_REMOVE: u32 = 0x0000_0002;
const AT_FDCWD: i32 = -100;

const FANOTIFY_METADATA_VERSION: u8 = 3;
const FAN_NOFD: i32 = -1;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FanotifyEventMetadata {
    event_len: u32,
    vers: u8,
    reserved: u8,
    metadata_len: u16,
    mask: u64,
    fd: i32,
    pid: i32,
}

struct WatchedApp {
    name: String,
    txid: i64,
}

/// Owns the `FAN_OPEN_EXEC` fanotify group and the set of currently-marked
/// binary paths. `try_new`/`load_from_db` can fail (e.g. pre-5.0 kernel) —
/// the daemon treats that as graceful degradation, not a startup failure.
pub struct ExecWatch {
    fanotify_fd: RawFd,
    watched: Mutex<HashMap<PathBuf, WatchedApp>>,
}

impl ExecWatch {
    pub fn try_new() -> Result<Self> {
        let fd = unsafe {
            libc::syscall(
                libc::SYS_fanotify_init,
                (FAN_CLASS_NOTIF | FAN_CLOEXEC | FAN_NONBLOCK) as i64,
                (libc::O_RDONLY | libc::O_LARGEFILE) as i64,
            )
        };
        if fd < 0 {
            return Err(anyhow::anyhow!(
                "fanotify_init (exec-watch) failed: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(Self { fanotify_fd: fd as RawFd, watched: Mutex::new(HashMap::new()) })
    }

    /// Build a fresh `ExecWatch` and re-arm marks for every currently
    /// `tracking` app. Called once at daemon startup — kernel-side marks
    /// don't survive the old process's fd closing, so this is what makes
    /// tracking durable across a daemon restart.
    pub fn load_from_db(conn: &Connection) -> Result<Self> {
        let ew = Self::try_new()?;
        let apps = crate::tracked_apps::list_tracked_apps(conn, false)?;
        for app in apps {
            if let Err(e) = ew.watch_app(&app.name, app.txid, &app.resolved_paths) {
                log::warn!("ExecWatch: failed to arm marks for '{}': {}", app.name, e);
            }
        }
        Ok(ew)
    }

    /// Arm `FAN_OPEN_EXEC` marks for every one of `paths` and record which
    /// app/txid they belong to. Called after `Track`'s DB write succeeds.
    pub fn watch_app(&self, name: &str, txid: i64, paths: &[String]) -> Result<()> {
        let mut watched = self.watched.lock().unwrap_or_else(|p| p.into_inner());
        for path in paths {
            self.mark(path, true)?;
            watched.insert(PathBuf::from(path), WatchedApp { name: name.to_string(), txid });
        }
        Ok(())
    }

    /// Disarm every mark currently associated with `name`. Called after
    /// `Untrack`'s DB write succeeds. Best-effort: a failed unmark is logged,
    /// not propagated, since the app is already untracked in the DB either way.
    pub fn unwatch_app(&self, name: &str) {
        let mut watched = self.watched.lock().unwrap_or_else(|p| p.into_inner());
        let stale: Vec<PathBuf> =
            watched.iter().filter(|(_, w)| w.name == name).map(|(p, _)| p.clone()).collect();
        for path in stale {
            if let Err(e) = self.mark(&path.to_string_lossy(), false) {
                log::warn!("ExecWatch: failed to remove mark for {}: {}", path.display(), e);
            }
            watched.remove(&path);
        }
    }

    fn mark(&self, path: &str, add: bool) -> Result<()> {
        let flags = if add { FAN_MARK_ADD } else { FAN_MARK_REMOVE };
        let c_path = CString::new(path)
            .map_err(|e| anyhow::anyhow!("path {} has an embedded NUL: {}", path, e))?;
        let ret = unsafe {
            libc::syscall(
                libc::SYS_fanotify_mark,
                self.fanotify_fd as i64,
                flags as i64,
                FAN_OPEN_EXEC as i64,
                AT_FDCWD as i64,
                c_path.as_ptr(),
            )
        };
        if ret < 0 {
            return Err(anyhow::anyhow!(
                "fanotify_mark ({}) failed for {}: {}",
                if add { "add" } else { "remove" },
                path,
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    /// Read pending `FAN_OPEN_EXEC` events. Unlike the install-time
    /// monitor's FID-based reporting (always `FAN_NOFD`), this group uses
    /// plain fd-based reporting — `meta.fd` is a real, valid fd on every
    /// event and must be closed right after resolving it, or the daemon
    /// leaks one fd per app launch indefinitely.
    fn read_exec_events(&self) -> Vec<(i32, PathBuf)> {
        let mut buf = [0u8; 4096];
        let mut events = Vec::new();

        let n = unsafe {
            libc::read(self.fanotify_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::WouldBlock {
                log::warn!("exec-watch: read() failed: {}", err);
            }
            return events;
        }
        if n == 0 {
            return events;
        }
        let n = n as usize;

        let mut offset = 0usize;
        while offset + std::mem::size_of::<FanotifyEventMetadata>() <= n {
            let meta = unsafe {
                std::ptr::read(buf.as_ptr().add(offset) as *const FanotifyEventMetadata)
            };
            if meta.vers != FANOTIFY_METADATA_VERSION || meta.event_len == 0 {
                break;
            }
            let event_end = offset + meta.event_len as usize;
            if event_end > n {
                break;
            }

            if meta.fd != FAN_NOFD {
                if let Ok(path) = std::fs::read_link(format!("/proc/self/fd/{}", meta.fd)) {
                    events.push((meta.pid, path));
                }
                unsafe { libc::close(meta.fd) };
            }

            offset = event_end;
        }
        events
    }

    /// The event loop: on a match, spawns a per-launch task that resolves
    /// the launching user's home, registers the pid (and its discovered
    /// descendants) with `active_launches`, arms `mutation_capture` for
    /// that home's filesystem, and tears both down once the launch's root
    /// process exits.
    pub async fn run(
        self: Arc<Self>,
        db_path: String,
        active_launches: Arc<Mutex<HashMap<i32, ActiveLaunch>>>,
        mutation_capture: Arc<MutationCapture>,
    ) {
        log::info!("exec-watch: running");
        loop {
            let events = self.read_exec_events();
            for (pid, exec_path) in events {
                let matched = {
                    let watched = self.watched.lock().unwrap_or_else(|p| p.into_inner());
                    watched.get(&exec_path).map(|w| (w.name.clone(), w.txid))
                };
                if let Some((name, txid)) = matched {
                    log::info!(
                        "exec-watch: detected launch of tracked app '{}' (pid={}, path={})",
                        name, pid, exec_path.display()
                    );
                    // Resolved synchronously, right here, rather than inside
                    // the spawned task below: a process that forks and lets
                    // its parent exit immediately (a common daemonize
                    // pattern) can vanish from /proc within microseconds —
                    // faster than a tokio::spawn scheduling hop takes. Doing
                    // this read in the same synchronous loop iteration as
                    // the event itself closes nearly all of that window.
                    let uid = match read_real_uid(pid) {
                        Some(u) => u,
                        None => {
                            log::warn!(
                                "exec-watch: couldn't resolve uid for pid {} (already exited?), skipping launch",
                                pid
                            );
                            continue;
                        }
                    };
                    let db_path = db_path.clone();
                    let active_launches = Arc::clone(&active_launches);
                    let mutation_capture = Arc::clone(&mutation_capture);
                    tokio::spawn(async move {
                        spawn_launch(pid, uid, txid, db_path, active_launches, mutation_capture).await;
                    });
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

impl Drop for ExecWatch {
    fn drop(&mut self) {
        if self.fanotify_fd >= 0 {
            unsafe { libc::close(self.fanotify_fd) };
        }
    }
}

/// System accounts (e.g. `nobody`) commonly have a passwd home of `/` — a
/// resolved, valid-looking answer, not a lookup failure, but arming a
/// filesystem-wide mark scoped to "/" would defeat the entire point of
/// per-launch $HOME scoping (every path on the filesystem satisfies
/// `path.starts_with("/")`). Reject it the same as an unresolvable home.
fn is_usable_home(path: &std::path::Path) -> bool {
    path != std::path::Path::new("/")
}

fn read_real_uid(pid: i32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

async fn spawn_launch(
    pid: i32,
    uid: u32,
    txid: i64,
    db_path: String,
    active_launches: Arc<Mutex<HashMap<i32, ActiveLaunch>>>,
    mutation_capture: Arc<MutationCapture>,
) {
    let home = match crate::scan_leftovers::home_dir_for_uid(uid).filter(|h| is_usable_home(h)) {
        Some(h) => h,
        None => {
            log::warn!("exec-watch: uid {} has no usable home dir (got '/' or none), skipping launch", uid);
            return;
        }
    };

    let root_launch = match ActiveLaunch::new(txid, home.clone()) {
        Some(l) => l,
        None => return, // ActiveLaunch::new already logs the reason
    };

    let launch_pids = Arc::new(Mutex::new(HashSet::new()));
    launch_pids.lock().unwrap_or_else(|p| p.into_inner()).insert(pid);
    active_launches.lock().unwrap_or_else(|p| p.into_inner()).insert(pid, root_launch);

    if let Err(e) = mutation_capture.start(&home) {
        log::warn!("exec-watch: MutationCapture::start failed for {}: {}", home.display(), e);
    }

    let (pid_tx, mut pid_rx) = mpsc::channel::<i32>(64);
    let active_launches_for_pids = Arc::clone(&active_launches);
    let launch_pids_for_pids = Arc::clone(&launch_pids);
    let home_for_pids = home.clone();
    let collector = tokio::spawn(async move {
        while let Some(new_pid) = pid_rx.recv().await {
            let is_new = launch_pids_for_pids.lock().unwrap_or_else(|p| p.into_inner()).insert(new_pid);
            if is_new {
                if let Some(l) = ActiveLaunch::new(txid, home_for_pids.clone()) {
                    active_launches_for_pids.lock().unwrap_or_else(|p| p.into_inner()).insert(new_pid, l);
                }
            }
        }
    });

    crate::process_tracker::watch_process_tree(pid, txid, db_path, pid_tx).await;
    collector.abort();

    // The process tree is confirmed dead, but a very common shutdown
    // pattern (save state, then exit — exactly what mpd, weechat, and most
    // real daemons do) means its very last write can still be sitting
    // unread in the kernel's fanotify queue at this exact instant:
    // `watch_process_tree`'s exit check runs on its own ~50ms poll, fully
    // uncoordinated with the shared mutation-capture loop's own ~10ms read
    // cycle. Pruning `active_launches` immediately would drop that final
    // write on arrival — attribution would already fail by the time it's
    // read. A short grace period gives the capture loop several full
    // cycles to catch up before this launch's attribution disappears.
    // Verified against a real mpd shutdown: without this, its `database`
    // and `state` files (both written synchronously right before exit)
    // were lost every time, while earlier writes in the same process's
    // life (its log file, its pidfile) were captured fine.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let pids: Vec<i32> = launch_pids.lock().unwrap_or_else(|p| p.into_inner()).iter().copied().collect();
    {
        let mut guard = active_launches.lock().unwrap_or_else(|p| p.into_inner());
        for p in pids {
            guard.remove(&p);
        }
    }
    mutation_capture.stop(&home);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn rejects_root_as_a_home_dir() {
        assert!(!is_usable_home(Path::new("/")));
    }

    #[test]
    fn accepts_a_real_home_dir() {
        assert!(is_usable_home(Path::new("/home/alice")));
        assert!(is_usable_home(Path::new("/root")));
    }
}
