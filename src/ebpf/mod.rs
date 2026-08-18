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
use std::fs;
use std::io;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
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

/// fanotify-based filesystem monitor that provides PID attribution per event.
/// Falls back to a no-op if fanotify is not available.
pub struct FanotifyMonitor {
    pub txid: i64,
    fanotify_fd: RawFd,
    pub pid_attribution: bool,
}

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
        })
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
                    path = Self::resolve_dfid_name(mount_fd, &buf, info_offset, info_len);
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
    /// f_handle[handle_bytes]} + NUL-terminated name.
    fn resolve_dfid_name(mount_fd: RawFd, buf: &[u8], info_offset: usize, info_len: usize) -> Option<PathBuf> {
        let handle_off = info_offset + 12;
        if handle_off + 8 > buf.len() {
            log::warn!("fanotify: resolve_dfid_name: handle_off out of bounds");
            return None;
        }
        let handle_bytes =
            u32::from_ne_bytes(buf.get(handle_off..handle_off + 4)?.try_into().ok()?) as usize;
        let name_off = handle_off + 8 + handle_bytes;
        let info_end = info_offset + info_len;
        if name_off > info_end || info_end > buf.len() {
            log::warn!(
                "fanotify: resolve_dfid_name: name_off {} out of bounds (info_end={}, buf.len={}, handle_bytes={})",
                name_off, info_end, buf.len(), handle_bytes
            );
            return None;
        }

        // The kernel-provided bytes at handle_off are already laid out exactly
        // like `struct file_handle` (handle_bytes, handle_type, f_handle[]),
        // so we can pass a pointer straight into the read buffer.
        let handle_ptr = buf[handle_off..].as_ptr() as *mut libc::c_void;
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

        let name_bytes = &buf[name_off..info_end];
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
