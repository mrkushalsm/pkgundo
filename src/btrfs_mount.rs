//! Workaround for the fanotify `FAN_MARK_FILESYSTEM` + `FAN_REPORT_FID`
//! `EXDEV` failure on btrfs subvolumes that aren't subvolume id 5 (see the
//! plan's "Context"/"Key design decisions" for the full background — this
//! is a documented, unresolved kernel/btrfs limitation, not a pkgundo bug).
//!
//! The fix: for any watch path that lives on btrfs, resolve it to a
//! dedicated read-only mount of that device's subvolume id 5 instead, and
//! `fanotify_mark` *that* — the mark attaches to the shared underlying
//! superblock, so it still covers every other subvolume on that device.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;

/// Fails closed to `false` on any `statfs` error — consistent with this
/// codebase's existing "unknown/unresolvable -> treat as the safe default"
/// posture.
pub fn is_btrfs(path: &Path) -> bool {
    let c_path = match CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };
    unsafe {
        let mut buf: libc::statfs = std::mem::zeroed();
        if libc::statfs(c_path.as_ptr(), &mut buf) != 0 {
            return false;
        }
        buf.f_type as i64 == BTRFS_SUPER_MAGIC
    }
}

/// Parse raw `/proc/self/mountinfo` text (NOT `findmnt` output — its
/// display enriches the source field with a `device[/subvol-path]` bracket
/// suffix that isn't present in the kernel-provided file, and would need
/// stripping before being reusable as a fresh `mount(2)` source argument).
/// Returns the longest-prefix-matching mount's `(source, fstype)`.
pub fn find_mount_source(mountinfo: &str, path: &Path) -> Option<(String, String)> {
    let mut best: Option<(usize, String, String)> = None;

    for line in mountinfo.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 5 {
            continue;
        }
        let sep = match tokens.iter().position(|&t| t == "-") {
            Some(s) => s,
            None => continue,
        };
        if sep + 2 >= tokens.len() {
            continue;
        }
        let mount_point = tokens[4];
        let fstype = tokens[sep + 1];
        let source = tokens[sep + 2];

        if path.starts_with(mount_point) && mount_point.len() > best.as_ref().map_or(0, |(l, _, _)| *l) {
            best = Some((mount_point.len(), source.to_string(), fstype.to_string()));
        }
    }

    best.map(|(_, source, fstype)| (source, fstype))
}

pub struct BtrfsRootMounts {
    mounts: Mutex<HashMap<String, PathBuf>>,
}

impl Default for BtrfsRootMounts {
    fn default() -> Self {
        Self::new()
    }
}

impl BtrfsRootMounts {
    pub fn new() -> Self {
        Self { mounts: Mutex::new(HashMap::new()) }
    }

    /// Resolve `path` to the watch path that should actually be passed to
    /// `fanotify_mark`. Non-btrfs paths are returned unchanged (zero
    /// behavior change on ext4/xfs/etc). For btrfs paths, lazily creates
    /// (or reuses) a read-only proxy mount of the owning device's
    /// subvolume id 5, keyed by device so two subvolumes of the same
    /// device share one proxy mount and one refcount entry.
    pub fn resolve(&self, path: &Path) -> Result<PathBuf> {
        if !is_btrfs(path) {
            return Ok(path.to_path_buf());
        }

        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
            .context("failed to read /proc/self/mountinfo")?;
        let (source, _fstype) = find_mount_source(&mountinfo, path)
            .with_context(|| format!("no mountinfo entry covers {}", path.display()))?;

        // Held across the whole check-then-create-then-insert sequence to
        // avoid a TOCTOU double-mount race between two concurrent
        // first-time launches on the same never-before-seen device.
        let mut mounts = self.mounts.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(existing) = mounts.get(&source) {
            return Ok(existing.clone());
        }

        let target = mount_point_for(&source);
        std::fs::create_dir_all(&target)
            .with_context(|| format!("failed to create {}", target.display()))?;
        mount_subvol5(&source, &target)?;

        mounts.insert(source, target.clone());
        Ok(target)
    }

    /// Best-effort unmount of every proxy mount created this run. Called
    /// once at daemon shutdown, mirroring `run_daemon`'s existing
    /// best-effort socket/pid-file cleanup.
    pub fn cleanup(&self) {
        let mut mounts = self.mounts.lock().unwrap_or_else(|p| p.into_inner());
        for (source, target) in mounts.drain() {
            if let Err(e) = umount(&target) {
                log::warn!("BtrfsRootMounts::cleanup: failed to unmount {} ({}): {}", target.display(), source, e);
            }
        }
    }
}

fn mount_point_for(source: &str) -> PathBuf {
    let sanitized: String = source
        .trim_start_matches('/')
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    PathBuf::from(crate::daemon::RUN_DIR).join("btrfs-root").join(sanitized)
}

fn mount_subvol5(source: &str, target: &Path) -> Result<()> {
    let c_source = CString::new(source)?;
    let c_target = CString::new(target.as_os_str().as_bytes())?;
    let c_fstype = CString::new("btrfs")?;
    let c_data = CString::new("subvolid=5")?;

    let ret = unsafe {
        libc::mount(
            c_source.as_ptr(),
            c_target.as_ptr(),
            c_fstype.as_ptr(),
            libc::MS_RDONLY,
            c_data.as_ptr() as *const libc::c_void,
        )
    };
    if ret != 0 {
        bail!("mount({} -> {}, subvolid=5) failed: {}", source, target.display(), std::io::Error::last_os_error());
    }
    Ok(())
}

fn umount(target: &Path) -> Result<()> {
    let c_target = CString::new(target.as_os_str().as_bytes())?;
    let ret = unsafe { libc::umount2(c_target.as_ptr(), 0) };
    if ret != 0 {
        bail!("umount2({}) failed: {}", target.display(), std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_mount_source_single_mount() {
        let mountinfo = "36 35 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n";
        let (source, fstype) = find_mount_source(mountinfo, Path::new("/home/pkgundo/foo")).unwrap();
        assert_eq!(source, "/dev/sda1");
        assert_eq!(fstype, "ext4");
    }

    #[test]
    fn find_mount_source_picks_longest_prefix() {
        let mountinfo = "\
36 35 0:30 /root / rw,relatime shared:1 - btrfs /dev/vda4 rw,subvolid=256,subvol=/root\n\
37 36 0:30 /home /home rw,relatime shared:2 - btrfs /dev/vda4 rw,subvolid=257,subvol=/home\n";
        let (source, _) = find_mount_source(mountinfo, Path::new("/home/pkgundo/sub/dir")).unwrap();
        assert_eq!(source, "/dev/vda4");

        let (source, _) = find_mount_source(mountinfo, Path::new("/etc/foo")).unwrap();
        assert_eq!(source, "/dev/vda4");
    }

    #[test]
    fn find_mount_source_not_found_on_malformed_input() {
        assert!(find_mount_source("", Path::new("/home/pkgundo")).is_none());
        assert!(find_mount_source("garbage line with no separator\n", Path::new("/home/pkgundo")).is_none());
    }

    #[test]
    fn mount_point_for_sanitizes_device_path() {
        let p = mount_point_for("/dev/vda4");
        assert!(p.starts_with(crate::daemon::RUN_DIR));
        assert!(p.to_string_lossy().contains("dev-vda4"));
    }
}
