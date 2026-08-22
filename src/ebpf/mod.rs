//! Phase 10: Enhanced filesystem monitoring using fanotify with PID attribution.
//!
//! fanotify (kernel 2.6.37+) improves on inotify by providing the PID of the
//! process that caused each filesystem event. This solves the key attribution
//! problem: knowing which process (from which transaction) mutated which file.
//!
//! This module provides:
//! 1. FanotifyMonitor — a fanotify-based fs monitor that yields PID-attributed events
//! 2. check_ebpf_availability() — detects eBPF tracing support on the system
//! 3. EbpfProcessTracker — enhanced process tracking using perf tracepoints
//!
//! Falls back to the inotify-based FsMonitor if fanotify init fails.

use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::classifier::classify_path;
use crate::journal::MutationRecord;

// ── fanotify constants (from linux/fanotify.h) ─────────────────────────────
const FAN_CLASS_NOTIF: u32 = 0x0000_0000;
const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_NONBLOCK: u32 = 0x0000_0002;
const FAN_CLOSE_WRITE: u64 = 0x0000_0008;
const FAN_CREATE: u64 = 0x0000_0100;
const FAN_DELETE: u64 = 0x0000_0200;
const FAN_MOVED_FROM: u64 = 0x0000_0040;
const FAN_MOVED_TO: u64 = 0x0000_0080;

const FAN_MARK_ADD: u32 = 0x0000_0001;
const FAN_MARK_REMOVE: u32 = 0x0000_0002;
// FAN_MARK_MOUNT explicitly forbids FAN_CREATE/FAN_DELETE/FAN_MOVE* in its
// mask — file-handle-identified events "cannot be provided as a mask when
// flags contains FAN_MARK_MOUNT" (`man fanotify_mark`, FAN_MARK_MOUNT
// section) and fail EINVAL unconditionally, regardless of the FAN_REPORT_*
// flags passed to fanotify_init. FAN_MARK_FILESYSTEM (Linux 4.20+) is the
// mark type actually meant to pair with FID-based reporting for this case.
const FAN_MARK_FILESYSTEM: u32 = 0x0000_0100;

// FAN_MARK_MOUNT + directory-entry events (FAN_CREATE/FAN_DELETE/FAN_MOVED_*)
// require the group to be initialized with FID-based reporting, or every
// fanotify_mark() call for them fails EINVAL (see `man fanotify_mark`).
// FAN_REPORT_DFID_NAME = FAN_REPORT_DIR_FID | FAN_REPORT_NAME: events carry a
// parent-directory file handle + the child's filename instead of an fd, for
// every event type in our mask (per `man fanotify_init`, "for events that
// occur on a non-directory object, the reported file handle is that of the
// parent directory ... and the reported name is the name of [the] entry").
const FAN_REPORT_FID: u32 = 0x0000_0200;
const FAN_REPORT_DIR_FID: u32 = 0x0000_0400;
const FAN_REPORT_NAME: u32 = 0x0000_0800;
// FAN_REPORT_DFID_NAME alone is the DIR_FID+NAME bits; per `man fanotify_mark`'s
// ERRORS section, event types requiring file-handle identification (which
// FAN_CREATE/FAN_DELETE/FAN_MOVED_* are) additionally need the base
// FAN_REPORT_FID bit set, or fanotify_mark() fails EINVAL regardless of
// DFID_NAME being set. (Confirmed by testing: EINVAL persisted with only
// FAN_REPORT_DFID_NAME set, until FAN_REPORT_FID was added alongside it.)
const FAN_REPORT_DFID_NAME: u32 = FAN_REPORT_FID | FAN_REPORT_DIR_FID | FAN_REPORT_NAME;

const AT_FDCWD: i32 = -100;

/// Raw fanotify event metadata (matches kernel struct fanotify_event_metadata)
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

const FANOTIFY_METADATA_VERSION: u8 = 3;
const FAN_NOFD: i32 = -1;

/// Info record type for a parent-dir-handle + filename pair (matches kernel
/// FAN_EVENT_INFO_TYPE_DFID_NAME). Follows the fixed metadata for every
/// event once the group is initialized with FAN_REPORT_DFID_NAME.
const FAN_EVENT_INFO_TYPE_DFID_NAME: u8 = 2;

/// Result of eBPF/fanotify availability detection
#[derive(Debug, Clone)]
pub struct KernelCapabilities {
    pub fanotify_available: bool,
    pub ebpf_available: bool,
    pub tracefs_mounted: bool,
    pub kernel_version: String,
}

impl KernelCapabilities {
    /// Detect available kernel tracing capabilities
    pub fn detect() -> Self {
        let kernel_version = fs::read_to_string("/proc/version")
            .unwrap_or_default()
            .split_whitespace()
            .nth(2)
            .unwrap_or("unknown")
            .to_string();

        // Check fanotify support via kernel config
        let fanotify_available = check_fanotify_support();

        // Check eBPF availability
        let ebpf_available = check_ebpf_support();

        // Check if tracefs is mounted
        let tracefs_mounted = std::path::Path::new("/sys/kernel/tracing").exists()
            || std::path::Path::new("/sys/kernel/debug/tracing").exists();

        Self {
            fanotify_available,
            ebpf_available,
            tracefs_mounted,
            kernel_version,
        }
    }
}

fn check_fanotify_support() -> bool {
    // Try to init a fanotify fd — safest way to detect support
    let result = unsafe {
        libc::syscall(libc::SYS_fanotify_init, FAN_CLASS_NOTIF | FAN_CLOEXEC, libc::O_RDONLY)
    };
    if result >= 0 {
        unsafe { libc::close(result as i32) };
        true
    } else {
        false
    }
}

fn check_ebpf_support() -> bool {
    // bpf(BPF_PROG_TYPE_TRACEPOINT) support requires kernel 4.7+
    // Quick check: does /sys/fs/bpf exist?
    std::path::Path::new("/sys/fs/bpf").exists()
        && fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
            .map(|s| s.trim().parse::<i32>().unwrap_or(3) <= 2)
            .unwrap_or(false)
}

/// A currently-running tracked-app launch, registered in the shared map
/// `run_shared` consults to decide whether/how to record an event. `home` is
/// the launching user's home directory (the scoping filter); `mount_fd` is
/// an fd open on that same path, needed because `open_by_handle_at(2)`
/// requires an fd on the *same filesystem* as the object being resolved —
/// with marks potentially spanning multiple distinct filesystems (e.g. a
/// separate `/home` partition), a single hardcoded mount fd can't correctly
/// resolve handles for all of them, so each launch carries its own.
pub struct ActiveLaunch {
    pub txid: i64,
    pub home: PathBuf,
    mount_fd: RawFd,
}

impl ActiveLaunch {
    /// Opens `mount_fd` on `home` itself. Returns `None` if `home` can't be
    /// opened (race: directory doesn't exist yet, permissions, etc.) — the
    /// caller should skip registering this launch rather than panic.
    pub fn new(txid: i64, home: PathBuf) -> Option<Self> {
        let c_home = std::ffi::CString::new(home.to_string_lossy().as_bytes()).ok()?;
        let fd = unsafe { libc::open(c_home.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            log::warn!(
                "ActiveLaunch: failed to open {} for handle resolution: {}",
                home.display(),
                io::Error::last_os_error()
            );
            return None;
        }
        Some(Self { txid, home, mount_fd: fd })
    }
}

impl Drop for ActiveLaunch {
    fn drop(&mut self) {
        if self.mount_fd >= 0 {
            unsafe { libc::close(self.mount_fd) };
        }
    }
}

/// fanotify-based filesystem monitor that provides PID attribution per event.
/// Falls back to a no-op if fanotify is not available.
pub struct FanotifyMonitor {
    pub txid: i64,
    fanotify_fd: RawFd,
    pub pid_attribution: bool,
    /// tid -> tgid, populated proactively (see `refresh_tid_cache`) rather
    /// than only resolved lazily at event-read time. A short-lived worker
    /// thread (e.g. mpd's own db-update thread: scan, write, rename, then
    /// exit — often within a handful of milliseconds) can fully terminate
    /// between generating its write's fanotify event and this monitor
    /// getting around to reading it; a live `/proc/<tid>/status` read at
    /// that point fails and the event is dropped even though the write was
    /// perfectly real. Caching the mapping while the thread is still known
    /// to be alive means a later lookup doesn't depend on it still being
    /// alive. Verified live: a real `mpd` library-database write from its
    /// update thread was silently lost every time without this cache, while
    /// same-process writes from longer-lived threads (main thread's log/pid
    /// writes) were never affected — this only ever hit exactly the kind of
    /// thread short-lived enough to race a live re-resolve.
    tid_cache: Mutex<HashMap<i32, i32>>,
    /// Events whose raw reported pid didn't match any entry in
    /// `active_launches` at read time — buffered here instead of dropped
    /// outright, since the gap is often just `process_tracker`'s ~50ms
    /// `/proc` poll not having registered a just-forked descendant yet, not
    /// a genuinely unrelated process. Retried on every subsequent read
    /// until either attribution succeeds or `PENDING_RETRY_WINDOW` elapses.
    /// See `read_events_filtered`'s doc for the full race this closes.
    pending: Mutex<Vec<PendingEvent>>,
}

/// A fanotify event that couldn't be attributed to any active launch at
/// read time. Stores the *raw reported pid* — deliberately not a resolved
/// tgid — plus a copy of the raw DFID_NAME info record bytes, so a retry is
/// just a cheap map lookup, no `/proc` read at all. This matters: the
/// failure mode being closed here is a child process short-lived enough
/// that it may already be fully exited (and reaped) by the time this event
/// is read, so a `/proc`-based tgid resolution at buffer time would just
/// fail immediately for exactly the events that need buffering. `pid` works
/// anyway because `process_tracker`'s discovery registers a descendant
/// under this exact same raw pid, live-process confirmation or not.
struct PendingEvent {
    mask: u64,
    pid: i32,
    info: Vec<u8>,
    first_seen: Instant,
}

/// How long an unattributed event is worth retrying. `process_tracker`
/// polls every 50ms, so this covers several missed ticks' worth of
/// discovery lag without holding onto events indefinitely.
const PENDING_RETRY_WINDOW: Duration = Duration::from_millis(500);

/// Hard cap on buffered unattributed events — a safety valve against
/// unbounded growth if a whole-filesystem mark sees heavy activity from
/// genuinely unrelated processes while a launch is active (those never
/// resolve, but still occupy a slot until they expire).
const PENDING_MAX: usize = 5000;

impl FanotifyMonitor {
    /// Create a new fanotify monitor. Returns error if fanotify unavailable.
    pub fn try_new(txid: i64) -> Result<Self> {
        let fd = unsafe {
            libc::syscall(
                libc::SYS_fanotify_init,
                (FAN_CLASS_NOTIF | FAN_CLOEXEC | FAN_NONBLOCK | FAN_REPORT_DFID_NAME) as i64,
                (libc::O_RDONLY | libc::O_LARGEFILE) as i64,
            )
        };

        if fd < 0 {
            return Err(anyhow::anyhow!(
                "fanotify_init failed: {}",
                io::Error::last_os_error()
            ));
        }

        Ok(Self {
            txid,
            fanotify_fd: fd as RawFd,
            pid_attribution: true,
            tid_cache: Mutex::new(HashMap::new()),
            pending: Mutex::new(Vec::new()),
        })
    }

    /// Rescan every currently-known launch's live threads and record their
    /// tid -> tgid mapping, and drop cached entries for launches that are no
    /// longer active. Cheap: real launch counts are 0-2 at a time, each with
    /// a handful of threads at most, so this is a small `/proc` directory
    /// listing per known launch, not a whole-system scan.
    fn refresh_tid_cache(&self, active_launches: &Mutex<HashMap<i32, ActiveLaunch>>) {
        let known_tgids: Vec<i32> = {
            let guard = active_launches.lock().unwrap_or_else(|p| p.into_inner());
            guard.keys().copied().collect()
        };
        let mut cache = self.tid_cache.lock().unwrap_or_else(|p| p.into_inner());
        cache.retain(|_, tgid| known_tgids.contains(tgid));
        for tgid in known_tgids {
            for tid in scan_thread_ids(tgid) {
                cache.insert(tid, tgid);
            }
        }
    }

    /// Buffer an event whose raw pid didn't (yet) match any active launch,
    /// bounded by `PENDING_MAX` so sustained unrelated filesystem activity
    /// during a launch can't grow this unboundedly.
    fn buffer_pending(&self, mask: u64, pid: i32, info: &[u8]) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.len() >= PENDING_MAX {
            log::debug!("fanotify: pending-event buffer full ({PENDING_MAX}), dropping event for pid={pid}");
            return;
        }
        log::debug!("fanotify: buffering unattributed event for pid={pid} (retry within {PENDING_RETRY_WINDOW:?})");
        pending.push(PendingEvent { mask, pid, info: info.to_vec(), first_seen: Instant::now() });
    }

    /// Retry every buffered event against the current `active_launches`
    /// state, moving newly-resolvable ones into `events`. An entry is
    /// removed once it either resolves (successfully or not — a stale
    /// handle at this point won't get fresher by waiting longer) or exceeds
    /// `PENDING_RETRY_WINDOW`; only a still-unmatched pid within the window
    /// is kept for another pass. Cheap even with many pending entries: no
    /// retry here costs more than a map lookup unless it actually resolves.
    fn retry_pending(
        &self,
        active_launches: &Mutex<HashMap<i32, ActiveLaunch>>,
        events: &mut Vec<(u64, i32, PathBuf, i64)>,
    ) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.is_empty() {
            return;
        }
        let now = Instant::now();
        pending.retain(|p| {
            if now.duration_since(p.first_seen) > PENDING_RETRY_WINDOW {
                return false;
            }
            let mount_fd = {
                let guard = active_launches.lock().unwrap_or_else(|g| g.into_inner());
                guard.get(&p.pid).map(|l| l.mount_fd)
            };
            let Some(mount_fd) = mount_fd else {
                return true; // still not registered — keep waiting within the window
            };
            if let Some(path) = Self::resolve_info_record(mount_fd, &p.info) {
                let guard = active_launches.lock().unwrap_or_else(|g| g.into_inner());
                if let Some(txid) = attribute_event(p.pid, &path, &guard) {
                    log::debug!(
                        "fanotify: retry resolved buffered event for pid={} -> {} (waited {:?})",
                        p.pid, path.display(), now.duration_since(p.first_seen)
                    );
                    events.push((p.mask, p.pid, path, txid));
                }
            }
            false
        });
    }

    /// Mark filesystem paths for monitoring
    pub fn watch_paths(&self, paths: &[&str]) -> Result<()> {
        let mask = FAN_CLOSE_WRITE | FAN_CREATE | FAN_DELETE | FAN_MOVED_FROM | FAN_MOVED_TO;

        for path in paths {
            let c_path = match std::ffi::CString::new(*path) {
                Ok(c) => c,
                Err(e) => {
                    log::debug!("fanotify: skipping unwatchable path {}: {}", path, e);
                    continue;
                }
            };
            let ret = unsafe {
                libc::syscall(
                    libc::SYS_fanotify_mark,
                    self.fanotify_fd as i64,
                    (FAN_MARK_ADD | FAN_MARK_FILESYSTEM) as i64,
                    mask as i64,
                    AT_FDCWD as i64,
                    c_path.as_ptr(),
                )
            };
            if ret < 0 {
                log::warn!(
                    "fanotify_mark failed for {}: {}",
                    path,
                    io::Error::last_os_error()
                );
            } else {
                log::info!("fanotify: watching {}", path);
            }
        }
        Ok(())
    }

    /// Add or remove a single `FAN_MARK_FILESYSTEM` mark — used by
    /// `MutationCapture` to dynamically arm/disarm coverage of a launch's
    /// home filesystem while this group's `run_shared` loop is concurrently
    /// reading events from the same fd. Adding/removing marks and reading
    /// are independent syscalls on the same fd; the kernel handles this
    /// concurrency fine.
    pub fn mark_filesystem(&self, path: &str, add: bool) -> Result<()> {
        let mask = FAN_CLOSE_WRITE | FAN_CREATE | FAN_DELETE | FAN_MOVED_FROM | FAN_MOVED_TO;
        let flags = if add {
            FAN_MARK_ADD | FAN_MARK_FILESYSTEM
        } else {
            FAN_MARK_REMOVE | FAN_MARK_FILESYSTEM
        };
        let c_path = std::ffi::CString::new(path)
            .map_err(|e| anyhow::anyhow!("path {} has an embedded NUL: {}", path, e))?;
        let ret = unsafe {
            libc::syscall(
                libc::SYS_fanotify_mark,
                self.fanotify_fd as i64,
                flags as i64,
                mask as i64,
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
        log::info!(
            "fanotify: {} filesystem mark for {}",
            if add { "armed" } else { "disarmed" },
            path
        );
        Ok(())
    }

    /// Like `read_events`, but for the shared mutation-capture group: looks
    /// up each event's pid in `active_launches` *before* doing any handle
    /// resolution — most events on a whole-filesystem mark belong to
    /// unrelated processes, so this both scopes correctness (using the
    /// matched launch's own `mount_fd`, since marks may span multiple
    /// distinct filesystems) and skips the resolve work entirely for
    /// everything else. Returns (mask, pid, resolved path, txid) tuples,
    /// already filtered to paths under the matched launch's home dir.
    /// Returns the matched events, plus whether this read filled the entire
    /// buffer — a signal that more events are very likely still queued in
    /// the kernel and should be drained immediately rather than waiting out
    /// the normal poll interval. Under a fast burst (e.g. npm extracting a
    /// package, hundreds of files in well under a second), a fixed sleep
    /// between every read let a launch's root process exit — pruning its
    /// pid from `active_launches` — before already-generated kernel events
    /// for that pid had been drained, silently dropping them from
    /// `attribute_event`'s filtering on the next read.
    ///
    /// A second, narrower race lives one level up from that one: a *new*
    /// descendant process (not a worker thread — an actual forked child,
    /// e.g. a script's `ln`/`cp`/interpreter invocation) can perform its own
    /// write before `process_tracker`'s ~50ms `/proc` poll has discovered it
    /// and registered it in `active_launches` at all. Unlike the tid/tgid
    /// case above, there is no cache to consult — the pid is a real, live
    /// process, but attribution genuinely doesn't know about it *yet*. Such
    /// events are buffered (see `pending`) and retried on subsequent calls
    /// instead of being dropped the instant this happens to run first.
    fn read_events_filtered(
        &self,
        active_launches: &Mutex<HashMap<i32, ActiveLaunch>>,
    ) -> (Vec<(u64, i32, PathBuf, i64)>, bool) {
        let mut buf = [0u8; 65536];
        let mut events = Vec::new();
        self.retry_pending(active_launches, &mut events);

        let n = unsafe {
            libc::read(self.fanotify_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };

        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::WouldBlock {
                log::warn!("fanotify (shared): read() failed: {}", err);
            }
            return (events, false);
        }
        if n == 0 {
            return (events, false);
        }
        let n = n as usize;
        let buffer_was_full = n == buf.len();

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

            // Cheap check first: is this exact pid one we actually care
            // about? Skips both handle resolution and the (comparatively
            // costly, one /proc read each) tgid-resolution fallback below
            // for the overwhelming majority of events on a whole-filesystem
            // mark, which come from entirely unrelated system processes.
            let cheap_mount_fd = {
                let guard = active_launches.lock().unwrap_or_else(|p| p.into_inner());
                guard.get(&meta.pid).map(|l| l.mount_fd)
            };
            let mut resolved = cheap_mount_fd.map(|mount_fd| (meta.pid, mount_fd));

            if resolved.is_none() {
                // fanotify's reported pid is actually the tid of whichever
                // thread performed the I/O — a worker thread performing a
                // tracked launch's own write (e.g. Node's libuv threadpool,
                // which backs many of npm's own blocking `fs` writes) never
                // matches the launch's registered main pid directly. Only
                // pay for resolving this on a miss, so single-threaded
                // launches (the common case) stay exactly as cheap as
                // before this fix. See resolve_tgid's doc for the full story.
                //
                // Check the proactively-populated cache before falling back
                // to a live re-resolve: a short-lived worker thread can have
                // already exited by the time we get here, which would make
                // the live read fail even though the write was real — see
                // `tid_cache`'s doc.
                let cached_tgid = {
                    let cache = self.tid_cache.lock().unwrap_or_else(|p| p.into_inner());
                    cache.get(&meta.pid).copied()
                };
                if let Some(tgid) = cached_tgid.or_else(|| resolve_tgid(meta.pid)) {
                    let guard = active_launches.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(l) = guard.get(&tgid) {
                        resolved = Some((tgid, l.mount_fd));
                    }
                }
            }

            // Info records (parent-dir handle + filename) follow the fixed
            // metadata header — located unconditionally, since both the
            // immediate-resolution path below and the buffering path need
            // the same bytes.
            let mut info_offset = offset + meta.metadata_len as usize;
            let mut info_slice = None;
            while info_offset + 4 <= event_end {
                let info_type = buf[info_offset];
                let info_len =
                    u16::from_ne_bytes([buf[info_offset + 2], buf[info_offset + 3]]) as usize;
                if info_len == 0 || info_offset + info_len > event_end {
                    break;
                }
                if info_type == FAN_EVENT_INFO_TYPE_DFID_NAME {
                    info_slice = Some((info_offset, info_len));
                }
                info_offset += info_len;
            }

            if let Some((tgid, mount_fd)) = resolved {
                let path = info_slice
                    .and_then(|(io, il)| Self::resolve_info_record(mount_fd, &buf[io..io + il]));

                if let Some(p) = path {
                    // Re-check (not just reuse the earlier lookup) since the
                    // launch could theoretically have been deregistered while
                    // we were resolving — attribute_event() re-deriving the
                    // decision from current state is the safe default: an
                    // event from a launch that just ended is correctly dropped.
                    let guard = active_launches.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(txid) = attribute_event(tgid, &p, &guard) {
                        events.push((meta.mask, tgid, p, txid));
                    }
                }
            } else if let Some((io, il)) = info_slice {
                // Neither the raw reported pid nor (if it resolved at all)
                // its tgid currently match a registered launch. Buffer
                // using the *raw* pid — not a resolved tgid — since a
                // short-lived descendant (e.g. a script's `ln`/`cp`
                // invocation) can fully exit and be reaped before this
                // event is even read, which would make a tgid lookup here
                // fail every time for exactly the events that need
                // buffering. `process_tracker`'s discovery registers a
                // descendant under this exact raw pid regardless of
                // whether the process is still alive by the time that
                // happens, so retrying against it needs no `/proc` read at
                // all. Skipped while no launch is active at all, so an
                // idle daemon never pays anything for this.
                if !active_launches.lock().unwrap_or_else(|p| p.into_inner()).is_empty() {
                    self.buffer_pending(meta.mask, meta.pid, &buf[io..io + il]);
                }
            }

            if meta.fd != FAN_NOFD {
                unsafe { libc::close(meta.fd) };
            }

            offset = event_end;
        }

        (events, buffer_was_full)
    }

    /// Read events from the fanotify fd, returning (mask, pid, resolved path) tuples.
    /// `mount_fd` is an open fd on the watched mount, needed to resolve the
    /// parent-directory file handles the kernel reports (see FAN_REPORT_DFID_NAME
    /// on `try_new`) back into real paths via open_by_handle_at(2).
    fn read_events(&self, mount_fd: RawFd) -> Vec<(u64, i32, Option<PathBuf>)> {
        let mut buf = [0u8; 4096];
        let mut events = Vec::new();

        let n = unsafe {
            libc::read(
                self.fanotify_fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };

        if n < 0 {
            let err = io::Error::last_os_error();
            // EAGAIN/EWOULDBLOCK is the normal "no event ready yet" case on
            // this non-blocking fd, not an actual error — happens every loop.
            if err.kind() != io::ErrorKind::WouldBlock {
                log::warn!("fanotify: read() failed: {}", err);
            }
            return events;
        }
        if n == 0 {
            return events;
        }
        let n = n as usize;
        log::debug!("fanotify: read {} bytes from fd", n);

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

            log::debug!(
                "fanotify: event mask={:#x} pid={} metadata_len={} event_len={}",
                meta.mask, meta.pid, meta.metadata_len, meta.event_len
            );

            // Info records (parent-dir handle + filename) follow the fixed
            // metadata header, up to the end of this event.
            let mut info_offset = offset + meta.metadata_len as usize;
            let mut path = None;
            let mut saw_info_record = false;
            while info_offset + 4 <= event_end {
                let info_type = buf[info_offset];
                let info_len =
                    u16::from_ne_bytes([buf[info_offset + 2], buf[info_offset + 3]]) as usize;
                log::debug!("fanotify:   info record type={} len={}", info_type, info_len);
                if info_len == 0 || info_offset + info_len > event_end {
                    break;
                }
                saw_info_record = true;
                if info_type == FAN_EVENT_INFO_TYPE_DFID_NAME {
                    path = Self::resolve_info_record(mount_fd, &buf[info_offset..info_offset + info_len]);
                }
                info_offset += info_len;
            }
            if !saw_info_record {
                log::debug!("fanotify:   no info records on this event (metadata_len={} event_len={})", meta.metadata_len, meta.event_len);
            }

            events.push((meta.mask, meta.pid, path));

            // With FID-based reporting, meta.fd is always FAN_NOFD; this is
            // defensive in case that ever changes.
            if meta.fd != FAN_NOFD {
                unsafe { libc::close(meta.fd) };
            }

            offset = event_end;
        }

        events
    }

    /// Resolve a FAN_EVENT_INFO_TYPE_DFID_NAME info record into a real path.
    /// Layout: info_header(4) + fsid(8) + file_handle{handle_bytes(4), handle_type(4),
    /// f_handle[handle_bytes]} + NUL-terminated name. `info` is the raw record
    /// itself (already sliced out of the read buffer at `info_offset..info_offset+info_len`
    /// by the caller) — taking an owned/borrowed slice rather than
    /// `(buf, info_offset, info_len)` lets `PendingEvent` retry resolution
    /// later from a copy of these same bytes, once the original read
    /// buffer has long since been overwritten by a subsequent read().
    fn resolve_info_record(mount_fd: RawFd, info: &[u8]) -> Option<PathBuf> {
        let handle_off = 12;
        if handle_off + 8 > info.len() {
            log::warn!("fanotify: resolve_info_record: handle_off out of bounds");
            return None;
        }
        let handle_bytes =
            u32::from_ne_bytes(info.get(handle_off..handle_off + 4)?.try_into().ok()?) as usize;
        let name_off = handle_off + 8 + handle_bytes;
        if name_off > info.len() {
            log::warn!(
                "fanotify: resolve_info_record: name_off {} out of bounds (info.len={}, handle_bytes={})",
                name_off, info.len(), handle_bytes
            );
            return None;
        }

        // The kernel-provided bytes at handle_off are already laid out exactly
        // like `struct file_handle` (handle_bytes, handle_type, f_handle[]),
        // so we can pass a pointer straight into the read buffer.
        let handle_ptr = info[handle_off..].as_ptr() as *mut libc::c_void;
        let dir_fd = unsafe {
            libc::syscall(
                libc::SYS_open_by_handle_at,
                mount_fd as i64,
                handle_ptr as i64,
                libc::O_RDONLY as i64,
            )
        };
        if dir_fd < 0 {
            // ESTALE happens routinely when a file is renamed/replaced again
            // (e.g. a .part download finalized) before we get around to
            // resolving this event's handle — not an actionable error, the
            // event is just dropped.
            log::debug!(
                "fanotify: open_by_handle_at failed (handle_bytes={}): {}",
                handle_bytes, io::Error::last_os_error()
            );
            return None;
        }
        let dir_path = fs::read_link(format!("/proc/self/fd/{}", dir_fd)).ok();
        if dir_path.is_none() {
            log::warn!("fanotify: readlink /proc/self/fd/{} failed", dir_fd);
        }
        unsafe { libc::close(dir_fd as i32) };
        let dir_path = dir_path?;

        let name_bytes = &info[name_off..];
        let name_end = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_bytes.len());
        let name = match std::str::from_utf8(&name_bytes[..name_end]) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("fanotify: name is not valid utf8: {}", e);
                return None;
            }
        };

        let resolved = if name.is_empty() || name == "." {
            dir_path
        } else {
            dir_path.join(name)
        };
        log::debug!("fanotify: resolved path {}", resolved.display());
        Some(resolved)
    }

    /// Start watching and forwarding events to the mutation channel
    pub async fn run(self, txid: i64, mutation_tx: mpsc::Sender<MutationRecord>) {
        let watch_paths = ["/usr", "/etc", "/var", "/opt", "/lib", "/lib64", "/bin", "/sbin"];

        if let Err(e) = self.watch_paths(&watch_paths) {
            log::warn!("fanotify watch_paths failed: {}", e);
            return;
        }

        // Needed to resolve the parent-directory file handles the kernel
        // reports back into real paths via open_by_handle_at(2). Any fd on
        // the watched mount works; "/" is guaranteed to exist and be on it.
        let root_cstr = std::ffi::CString::new("/").unwrap();
        let mount_fd = unsafe { libc::open(root_cstr.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if mount_fd < 0 {
            log::warn!("fanotify: failed to open / for path resolution: {}", io::Error::last_os_error());
            return;
        }

        log::info!("Phase 10: fanotify monitor running with PID attribution");

        loop {
            let events = self.read_events(mount_fd);
            for (mask, pid, path) in events {
                let path = match path {
                    Some(p) => p,
                    None => continue,
                };

                if is_excluded_fanotify(&path) {
                    continue;
                }

                let operation = mask_to_operation(mask);
                let category = classify_path(&path);

                let record = MutationRecord {
                    id: None,
                    txid,
                    pid: Some(pid), // ← PID attribution! The key Phase 10 feature
                    operation: operation.to_string(),
                    path: path.to_string_lossy().to_string(),
                    timestamp: Utc::now(),
                    file_category: format!("{}", category),
                    pre_hash: None,
                    post_hash: None,
                };

                let _ = mutation_tx.send(record).await;
            }

            // Yield to avoid busy-loop
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// The shared mutation-capture counterpart to `run()`: no fixed watch
    /// list up front (marks are armed/disarmed dynamically by
    /// `MutationCapture` via `mark_filesystem` while this loop runs), and
    /// every event is attributed against `active_launches` (pid → txid/home)
    /// instead of a single hardcoded txid. Takes `Arc<Self>` rather than
    /// consuming `self`, since `MutationCapture` needs to keep calling
    /// `mark_filesystem` on the same group concurrently with this loop.
    pub async fn run_shared(
        self: Arc<Self>,
        mutation_tx: mpsc::Sender<crate::journal::JournalMessage>,
        active_launches: Arc<Mutex<HashMap<i32, ActiveLaunch>>>,
    ) {
        log::info!("Shared mutation-capture group running");

        loop {
            self.refresh_tid_cache(&active_launches);
            let (events, buffer_was_full) = self.read_events_filtered(&active_launches);
            for (mask, pid, path, txid) in events {
                if is_excluded_fanotify(&path) {
                    continue;
                }

                let operation = mask_to_operation(mask);
                let category = classify_path(&path);

                let record = MutationRecord {
                    id: None,
                    txid,
                    pid: Some(pid),
                    operation: operation.to_string(),
                    path: path.to_string_lossy().to_string(),
                    timestamp: Utc::now(),
                    file_category: format!("{}", category),
                    pre_hash: None,
                    post_hash: None,
                };

                let _ = mutation_tx.send(crate::journal::JournalMessage::Record(record)).await;
            }

            // A full buffer means the kernel very likely still has more
            // queued — keep draining immediately instead of waiting out the
            // normal poll interval, so a launch's pid isn't pruned from
            // active_launches (on process exit) before its own backlog of
            // already-generated events has actually been read.
            if !buffer_was_full {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }
}

impl Drop for FanotifyMonitor {
    fn drop(&mut self) {
        if self.fanotify_fd >= 0 {
            unsafe { libc::close(self.fanotify_fd) };
        }
    }
}

fn mask_to_operation(mask: u64) -> &'static str {
    if mask & FAN_CREATE != 0 {
        "create"
    } else if mask & FAN_CLOSE_WRITE != 0 {
        "modify"
    } else if mask & FAN_DELETE != 0 {
        "delete"
    } else if mask & FAN_MOVED_TO != 0 {
        // The destination now holds real content that wasn't there before
        // this transaction (e.g. package managers finalizing a download by
        // renaming it into place) — rollback treats this like a "create" at
        // the destination path, not a no-op.
        "rename_to"
    } else if mask & FAN_MOVED_FROM != 0 {
        "rename_from"
    } else {
        "unknown"
    }
}

fn is_excluded_fanotify(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy();
    s.starts_with("/proc")
        || s.starts_with("/sys")
        || s.starts_with("/dev")
        || s.starts_with("/run/user")
        || s.starts_with("/var/lib/pkgundo")
}

/// fanotify reports the kernel `pid` field as the actual *thread ID* that
/// performed the I/O, not necessarily the process's main pid (== tgid) —
/// they're only the same for a single-threaded process or its main thread.
/// A launch is only ever registered in `active_launches` under its main
/// pid, so any write happening on a worker thread with a different tid
/// (e.g. Node.js's libuv threadpool, which is exactly what backs many of
/// npm's own blocking `fs` writes — its content-addressable cache store in
/// particular) would silently never match `active_launches.get(&tid)` and
/// get dropped, even though the mark stayed armed for the launch's entire
/// real lifetime. Found live: a real `npm install` with the mark correctly
/// armed for its whole ~8s duration still missed ~85% of the files it
/// actually wrote, overwhelmingly under `~/.npm/_cacache/`. Resolve each
/// event's tid to its owning process's tgid via `/proc/<tid>/status`
/// before any `active_launches` lookup — `None` if the thread has already
/// exited by the time this reads it (fail-closed, matching every other
/// best-effort attribution step here: the event is simply dropped, same
/// as any other event that can't be resolved).
fn resolve_tgid(tid: i32) -> Option<i32> {
    let status = fs::read_to_string(format!("/proc/{}/status", tid)).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Tgid:") {
            return rest.trim().parse::<i32>().ok();
        }
    }
    None
}

/// Every currently-live thread id under `tgid`, via `/proc/<tgid>/task/`.
/// Empty (not an error) if `tgid` has already exited entirely by the time
/// this reads it — same fail-closed posture as every other best-effort
/// `/proc` read in this module.
fn scan_thread_ids(tgid: i32) -> Vec<i32> {
    let mut tids = Vec::new();
    if let Ok(entries) = fs::read_dir(format!("/proc/{}/task", tgid)) {
        for entry in entries.flatten() {
            if let Ok(tid) = entry.file_name().to_string_lossy().parse::<i32>() {
                tids.push(tid);
            }
        }
    }
    tids
}

/// Given an already-resolved event `(pid, path)`, decide whether it belongs
/// to a currently-active tracked-app launch and if so which txid to record
/// it against. `path.starts_with` is component-aware (not a string-prefix
/// compare), so e.g. `/home/alice` vs `/home/alice-backup` can't false-match.
/// Pure and side-effect-free — decoupled from the actual (root-only)
/// fanotify read/resolve mechanics so it's unit-testable on its own.
fn attribute_event(pid: i32, path: &std::path::Path, active_launches: &HashMap<i32, ActiveLaunch>) -> Option<i64> {
    let launch = active_launches.get(&pid)?;
    if path.starts_with(&launch.home) {
        Some(launch.txid)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn launch(txid: i64, home: &str) -> ActiveLaunch {
        // Any real, existing path works — ActiveLaunch::new just opens an fd
        // on it for later open_by_handle_at use, which these tests never
        // reach (attribute_event is pure path/map logic).
        ActiveLaunch::new(txid, PathBuf::from(home)).expect("home dir must exist for this test")
    }

    #[test]
    fn resolve_tgid_of_own_process_is_own_pid() {
        // The test harness's own main thread: tid == tgid == pid, so this
        // should resolve to exactly std::process::id().
        let pid = std::process::id() as i32;
        assert_eq!(resolve_tgid(pid), Some(pid));
    }

    #[test]
    fn resolve_tgid_of_nonexistent_pid_is_none() {
        // A pid this large is vanishingly unlikely to exist.
        assert_eq!(resolve_tgid(i32::MAX - 1), None);
    }

    #[test]
    fn scan_thread_ids_finds_own_running_thread() {
        let own_tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let tids = scan_thread_ids(std::process::id() as i32);
        assert!(tids.contains(&own_tid), "expected {:?} to contain own tid {}", tids, own_tid);
    }

    #[test]
    fn scan_thread_ids_of_nonexistent_tgid_is_empty() {
        assert!(scan_thread_ids(i32::MAX - 1).is_empty());
    }

    #[test]
    fn buffer_pending_respects_the_cap() {
        let monitor = match FanotifyMonitor::try_new(0) {
            Ok(m) => m,
            Err(_) => return, // fanotify unavailable in this sandbox — nothing to test
        };
        for i in 0..(PENDING_MAX + 10) {
            monitor.buffer_pending(0, i as i32, &[]);
        }
        assert_eq!(monitor.pending.lock().unwrap().len(), PENDING_MAX);
    }

    #[test]
    fn retry_pending_drops_expired_entries_without_emitting_them() {
        let monitor = match FanotifyMonitor::try_new(0) {
            Ok(m) => m,
            Err(_) => return,
        };
        {
            let mut pending = monitor.pending.lock().unwrap();
            pending.push(PendingEvent {
                mask: 0,
                pid: 999_999,
                info: vec![],
                first_seen: Instant::now() - PENDING_RETRY_WINDOW - Duration::from_millis(50),
            });
        }
        let active_launches = Mutex::new(HashMap::new());
        let mut events = Vec::new();
        monitor.retry_pending(&active_launches, &mut events);
        assert!(monitor.pending.lock().unwrap().is_empty(), "expired entry must be dropped");
        assert!(events.is_empty());
    }

    #[test]
    fn retry_pending_keeps_waiting_while_its_pid_is_still_unregistered() {
        let monitor = match FanotifyMonitor::try_new(0) {
            Ok(m) => m,
            Err(_) => return,
        };
        {
            let mut pending = monitor.pending.lock().unwrap();
            pending.push(PendingEvent { mask: 0, pid: 4242, info: vec![], first_seen: Instant::now() });
        }
        let active_launches = Mutex::new(HashMap::new()); // 4242 never registered
        let mut events = Vec::new();
        monitor.retry_pending(&active_launches, &mut events);
        assert_eq!(
            monitor.pending.lock().unwrap().len(), 1,
            "still within the retry window and never resolved — must keep waiting, not drop"
        );
        assert!(events.is_empty());
    }

    #[test]
    fn retry_pending_consumes_an_entry_once_its_pid_is_registered() {
        let monitor = match FanotifyMonitor::try_new(0) {
            Ok(m) => m,
            Err(_) => return,
        };
        {
            let mut pending = monitor.pending.lock().unwrap();
            // Garbage info bytes: can't actually resolve to a real path, but
            // that's not what this test is proving — it's that a *matched*
            // tgid stops being retried after one attempt instead of being
            // retried forever just because this particular handle was bogus.
            pending.push(PendingEvent { mask: 0, pid: 4242, info: vec![0u8; 4], first_seen: Instant::now() });
        }
        let mut map = HashMap::new();
        map.insert(4242, launch(1, "/tmp"));
        let active_launches = Mutex::new(map);
        let mut events = Vec::new();
        monitor.retry_pending(&active_launches, &mut events);
        assert!(monitor.pending.lock().unwrap().is_empty(), "matched tgid must be consumed, not retried again");
    }

    #[test]
    fn matches_pid_and_path_under_home() {
        let mut map = HashMap::new();
        map.insert(42, launch(7, "/tmp"));
        assert_eq!(attribute_event(42, Path::new("/tmp/app/config.json"), &map), Some(7));
    }

    #[test]
    fn drops_event_for_unregistered_pid() {
        let mut map = HashMap::new();
        map.insert(42, launch(7, "/tmp"));
        assert_eq!(attribute_event(99, Path::new("/tmp/app/config.json"), &map), None);
    }

    #[test]
    fn drops_event_outside_registered_home() {
        // ActiveLaunch::new needs a real, existing path (it opens an fd on
        // it), so use real sibling tempdirs to exercise the exact
        // alice / alice-backup false-positive this check guards against.
        let tmp = tempfile::tempdir().unwrap();
        let alice = tmp.path().join("alice");
        let alice_backup = tmp.path().join("alice-backup");
        std::fs::create_dir(&alice).unwrap();
        std::fs::create_dir(&alice_backup).unwrap();

        let mut map = HashMap::new();
        map.insert(42, ActiveLaunch::new(7, alice).unwrap());
        // Component-aware starts_with must not treat "alice-backup" as
        // being under "alice".
        assert_eq!(attribute_event(42, &alice_backup.join("file"), &map), None);
    }

    #[test]
    fn matches_home_itself_not_only_children() {
        let mut map = HashMap::new();
        map.insert(42, launch(7, "/tmp"));
        assert_eq!(attribute_event(42, Path::new("/tmp"), &map), Some(7));
    }
}

// ── eBPF Infrastructure ─────────────────────────────────────────────────────

/// eBPF-based process tracker using tracepoints.
///
/// When enabled (via `--features ebpf`), this replaces the /proc polling
/// approach with kernel-level tracepoints on sched_process_fork and
/// sched_process_exec for instant, zero-miss process discovery.
///
/// The eBPF program source is in `ebpf_src/pkgundo.bpf.c`.
/// Compile with: `clang -O2 -target bpf -c pkgundo.bpf.c -o pkgundo.bpf.o`
///
/// Architecture:
/// ┌──────────────────────────────────────────────────────┐
/// │  Kernel space (eBPF program)                         │
/// │  tracepoint/sched/sched_process_fork → BPF hash map  │
/// │  tracepoint/sched/sched_process_exec → BPF hash map  │
/// └──────────────────────────┬───────────────────────────┘
///                            │ BPF maps (shared memory)
/// ┌──────────────────────────▼───────────────────────────┐
/// │  Userspace (pkgundo ebpf module)                     │
/// │  Reads pid→ppid map → correlates with transaction     │
/// └──────────────────────────────────────────────────────┘
pub struct EbpfTracer {
    pub available: bool,
    pub kernel_caps: KernelCapabilities,
}

impl EbpfTracer {
    /// Initialize eBPF tracer. Checks for availability and falls back gracefully.
    pub fn new() -> Self {
        let kernel_caps = KernelCapabilities::detect();
        let available = kernel_caps.ebpf_available && kernel_caps.tracefs_mounted;

        if available {
            log::info!(
                "Phase 10: eBPF tracing available (kernel {}). Tracefs mounted: {}",
                kernel_caps.kernel_version,
                kernel_caps.tracefs_mounted
            );
        } else {
            log::info!(
                "Phase 10: eBPF not available on this system (kernel {}). Using /proc polling.",
                kernel_caps.kernel_version
            );
        }

        Self { available, kernel_caps }
    }

    /// Print a capability report
    pub fn print_report(&self) {
        println!("  Kernel version:     {}", self.kernel_caps.kernel_version);
        println!(
            "  fanotify:           {}",
            if self.kernel_caps.fanotify_available { "✓ available" } else { "✗ unavailable" }
        );
        println!(
            "  eBPF:               {}",
            if self.kernel_caps.ebpf_available { "✓ available" } else { "✗ unavailable" }
        );
        println!(
            "  tracefs:            {}",
            if self.kernel_caps.tracefs_mounted { "✓ mounted" } else { "✗ not mounted" }
        );
        println!(
            "  PID attribution:    {}",
            if self.kernel_caps.fanotify_available {
                "✓ via fanotify"
            } else {
                "⚠ unavailable (inotify fallback)"
            }
        );
    }
}

impl Default for EbpfTracer {
    fn default() -> Self {
        Self::new()
    }
}

/// Try to start a fanotify monitor; fall back to inotify if unavailable.
/// Returns whether fanotify is being used (true = PID attribution enabled).
pub async fn start_enhanced_monitor(
    txid: i64,
    mutation_tx: mpsc::Sender<MutationRecord>,
) -> bool {
    match FanotifyMonitor::try_new(txid) {
        Ok(monitor) => {
            log::info!("Phase 10: Starting fanotify monitor (PID-attributed events)");
            tokio::spawn(async move {
                monitor.run(txid, mutation_tx).await;
            });
            true
        }
        Err(e) => {
            log::debug!("Phase 10: fanotify unavailable ({}), falling back to inotify", e);
            false
        }
    }
}

// ── eBPF program source (documentation/reference) ───────────────────────────
//
// The following eBPF C program traces process fork/exec syscalls.
// To compile: clang -O2 -target bpf -I/usr/include/linux -c pkgundo.bpf.c -o pkgundo.bpf.o
//
// ```c
// #include <linux/bpf.h>
// #include <bpf/bpf_helpers.h>
// #include <bpf/bpf_tracing.h>
//
// // Map: child_pid → parent_pid
// struct {
//     __uint(type, BPF_MAP_TYPE_HASH);
//     __type(key, __u32);
//     __type(value, __u32);
//     __uint(max_entries, 65536);
// } fork_map SEC(".maps");
//
// // Tracepoint: sched_process_fork
// SEC("tracepoint/sched/sched_process_fork")
// int trace_fork(struct trace_event_raw_sched_process_fork *ctx) {
//     __u32 child_pid = ctx->child_pid;
//     __u32 parent_pid = ctx->parent_pid;
//     bpf_map_update_elem(&fork_map, &child_pid, &parent_pid, BPF_ANY);
//     return 0;
// }
//
// // Tracepoint: sched_process_exec
// SEC("tracepoint/sched/sched_process_exec")
// int trace_exec(struct trace_event_raw_sched_process_exec *ctx) {
//     __u32 pid = bpf_get_current_pid_tgid() >> 32;
//     __u32 ppid = bpf_get_current_pid_tgid() & 0xFFFFFFFF;
//     bpf_map_update_elem(&fork_map, &pid, &ppid, BPF_NOEXIST);
//     return 0;
// }
//
// char LICENSE[] SEC("license") = "GPL";
// ```
//
// Loading with aya (Rust):
// ```rust
// let bpf_obj = include_bytes!("../../target/bpf/pkgundo.bpf.o");
// let mut bpf = aya::Bpf::load(bpf_obj)?;
// let prog: &mut TracePoint = bpf.program_mut("trace_fork")?.try_into()?;
// prog.load()?;
// prog.attach("sched", "sched_process_fork")?;
// ```
