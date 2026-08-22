use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const PKGUNDO_ARCHIVE_ROOT: &str = "/var/lib/pkgundo/archives";

/// Metadata stored alongside each archived file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveMetadata {
    pub txid: i64,
    pub original_path: String,
    pub archived_at: String,
    pub sha256_at_archive: Option<String>,
    pub modified_after_install: bool,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: Option<u32>,
    /// Set only when the original path was a symlink — its link target,
    /// so `recover_archive` can recreate the symlink itself. A symlink has
    /// no file content for `fs::copy` to preserve; without this the entire
    /// symlink (and whatever it pointed to knowing) is unrecoverable the
    /// moment the original is removed.
    #[serde(default)]
    pub symlink_target: Option<String>,
}

/// Archive manager: handles preserving user-modified files before rollback,
/// and restoring them on request.
pub struct ArchiveManager {
    pub root: String,
}

impl ArchiveManager {
    pub fn new() -> Self {
        Self {
            root: PKGUNDO_ARCHIVE_ROOT.to_string(),
        }
    }

    /// Create an ArchiveManager rooted somewhere other than the default
    /// system location (e.g. a temp dir in tests).
    pub fn with_root(root: String) -> Self {
        Self { root }
    }

    /// Compute the archive path for a file given txid and original path
    /// e.g. /var/lib/pkgundo/archives/42/etc/nginx/nginx.conf
    pub fn archive_path_for(&self, txid: i64, original_path: &str) -> std::path::PathBuf {
        // Strip leading slash from original_path
        let rel = original_path.trim_start_matches('/');
        Path::new(&self.root).join(txid.to_string()).join(rel)
    }

    /// The metadata sidecar path for an archived file at `dest`. Appends a
    /// suffix rather than using `Path::with_extension`, which replaces
    /// whatever follows the *last* `.` in the filename — for a name like
    /// `<hash>.cache-9`, that silently collides with `<hash>.cache-10`,
    /// `<hash>.cache-11`, etc. (all of them replace to `<hash>.pkgundo-
    /// meta.json`), each archive_file call overwriting the previous file's
    /// metadata. Appending instead guarantees a distinct sidecar for every
    /// distinct `dest`, regardless of how many dots its own filename has.
    fn meta_path_for(dest: &Path) -> std::path::PathBuf {
        let mut file_name = dest.file_name().unwrap_or_default().to_os_string();
        file_name.push(".pkgundo-meta.json");
        dest.with_file_name(file_name)
    }

    /// Archive a file: copy it to the archive location and record metadata
    pub fn archive_file(
        &self,
        conn: &Connection,
        txid: i64,
        original_path: &str,
        modified_after_install: bool,
    ) -> Result<()> {
        let src = Path::new(original_path);
        if !src.exists() && !src.is_symlink() {
            log::debug!(
                "ArchiveManager: skipping non-existent file: {}",
                original_path
            );
            return Ok(());
        }

        let dest = self.archive_path_for(txid, original_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .context(format!("Failed to create archive dir: {}", parent.display()))?;
        }

        // A symlink has no file content for fs::copy to preserve — its
        // target string *is* its content. Store that in metadata instead
        // (recover_archive recreates the symlink from it); there is
        // nothing to write at `dest` itself.
        let symlink_target = if src.is_symlink() {
            let target = fs::read_link(src)?;
            log::debug!(
                "ArchiveManager: archiving symlink {} -> {}",
                original_path,
                target.display()
            );
            Some(target.to_string_lossy().to_string())
        } else {
            fs::copy(src, &dest).context(format!(
                "Failed to copy {} to archive {}",
                original_path,
                dest.display()
            ))?;
            None
        };

        // Capture metadata
        let meta = fs::symlink_metadata(src).ok();
        let sha256 = crate::fingerprint::compute_sha256(src).ok();

        let archive_meta = ArchiveMetadata {
            txid,
            original_path: original_path.to_string(),
            archived_at: chrono::Utc::now().to_rfc3339(),
            sha256_at_archive: sha256,
            modified_after_install,
            uid: meta.as_ref().map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.uid()
            }),
            gid: meta.as_ref().map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.gid()
            }),
            mode: meta.as_ref().map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.mode()
            }),
            symlink_target,
        };

        // Write JSON metadata alongside archive
        let meta_path = Self::meta_path_for(&dest);
        let json = serde_json::to_string_pretty(&archive_meta)?;
        fs::write(&meta_path, json).context("Failed to write archive metadata")?;

        // Record in database
        conn.execute(
            "INSERT OR REPLACE INTO archives
             (txid, original_path, archive_path, modified_after_install, archived_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                txid,
                original_path,
                dest.to_string_lossy().as_ref(),
                modified_after_install as i32,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;

        log::info!(
            "ArchiveManager: archived {} to {}",
            original_path,
            dest.display()
        );
        Ok(())
    }

    /// Recover archived files: restore them from the archive to their original paths
    pub fn recover_archive(&self, conn: &Connection, txid: i64) -> Result<Vec<String>> {
        let mut stmt = conn.prepare(
            "SELECT original_path, archive_path FROM archives WHERE txid = ?1",
        )?;

        let entries: Vec<(String, String)> = stmt
            .query_map(rusqlite::params![txid], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to load archive entries")?;

        let mut recovered = Vec::new();

        for (original_path, archive_path) in &entries {
            let src = Path::new(archive_path);
            let dest = Path::new(original_path);

            // A symlink was never copied as file content in the first
            // place (see archive_file) — its target lives only in the
            // metadata sidecar. Check for that before the plain-file
            // `src.exists()` path below, which would otherwise always
            // report a symlink entry as "missing" (nothing was ever
            // written at `src` for one) and silently skip recovering it.
            if let Some(meta) = Self::read_metadata(src) {
                if let Some(target) = meta.symlink_target {
                    if let Some(parent) = dest.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::remove_file(dest); // dest may already exist (e.g. re-recovering)
                    match std::os::unix::fs::symlink(&target, dest) {
                        Ok(()) => {
                            log::info!(
                                "ArchiveManager: recovered symlink {} -> {} from archive",
                                original_path,
                                target
                            );
                            recovered.push(original_path.clone());
                        }
                        Err(e) => {
                            log::warn!(
                                "ArchiveManager: failed to recreate symlink {}: {}",
                                original_path,
                                e
                            );
                        }
                    }
                    continue;
                }
            }

            if !src.exists() {
                log::warn!(
                    "ArchiveManager: archive file missing: {}",
                    archive_path
                );
                continue;
            }

            if let Some(parent) = dest.parent() {
                let _ = fs::create_dir_all(parent);
            }

            match fs::copy(src, dest) {
                Ok(_) => {
                    Self::restore_ownership(src, dest);
                    log::info!(
                        "ArchiveManager: recovered {} from archive",
                        original_path
                    );
                    recovered.push(original_path.clone());
                }
                Err(e) => {
                    log::warn!(
                        "ArchiveManager: failed to recover {}: {}",
                        original_path,
                        e
                    );
                }
            }
        }

        Ok(recovered)
    }

    /// Re-apply the original owner/group/mode captured at archive time
    /// (stored in the `.pkgundo-meta.json` sidecar next to `archive_src`)
    /// onto `dest` after `recover_archive` copies its content back.
    /// `fs::copy` alone leaves a freshly-recovered file owned by whichever
    /// user ran `pkgundo recover` (root, since the command requires it) —
    /// without this, a recovered file in someone's home directory would
    /// silently belong to root instead of them. Best-effort: a missing or
    /// unparseable sidecar (e.g. an archive made before this metadata was
    /// added) just means recovery falls back to root ownership, logged, not
    /// a failure of the recovery itself.
    /// Read and parse `archive_src`'s metadata sidecar, if it exists.
    fn read_metadata(archive_src: &Path) -> Option<ArchiveMetadata> {
        let meta_path = Self::meta_path_for(archive_src);
        fs::read_to_string(&meta_path).ok().and_then(|s| serde_json::from_str(&s).ok())
    }

    fn restore_ownership(archive_src: &Path, dest: &Path) {
        let meta = match Self::read_metadata(archive_src) {
            Some(m) => m,
            None => {
                log::debug!(
                    "ArchiveManager: no archive metadata at {}, leaving recovered file's ownership as-is",
                    Self::meta_path_for(archive_src).display()
                );
                return;
            }
        };

        if let Some(mode) = meta.mode {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = fs::set_permissions(dest, std::fs::Permissions::from_mode(mode & 0o7777)) {
                log::warn!("ArchiveManager: failed to restore mode on {}: {}", dest.display(), e);
            }
        }

        if let (Some(uid), Some(gid)) = (meta.uid, meta.gid) {
            let c_path = match std::ffi::CString::new(dest.to_string_lossy().as_bytes()) {
                Ok(p) => p,
                Err(_) => return,
            };
            let ret = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
            if ret != 0 {
                log::warn!(
                    "ArchiveManager: failed to restore ownership ({}:{}) on {}: {}",
                    uid,
                    gid,
                    dest.display(),
                    std::io::Error::last_os_error()
                );
            }
        }
    }

    /// List all archives for a transaction
    pub fn list_archives(conn: &Connection, txid: i64) -> Result<Vec<ArchiveEntry>> {
        let mut stmt = conn.prepare(
            "SELECT id, txid, original_path, archive_path, modified_after_install, archived_at
             FROM archives WHERE txid = ?1",
        )?;

        let entries = stmt
            .query_map(rusqlite::params![txid], |row| {
                Ok(ArchiveEntry {
                    id: row.get(0)?,
                    txid: row.get(1)?,
                    original_path: row.get(2)?,
                    archive_path: row.get(3)?,
                    modified_after_install: row.get::<_, i32>(4)? != 0,
                    archived_at: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(entries)
    }
}

impl Default for ArchiveManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    pub id: i64,
    pub txid: i64,
    pub original_path: String,
    pub archive_path: String,
    pub modified_after_install: bool,
    pub archived_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE archives (
                id INTEGER PRIMARY KEY,
                txid INTEGER NOT NULL,
                original_path TEXT NOT NULL,
                archive_path TEXT NOT NULL,
                modified_after_install INTEGER NOT NULL,
                archived_at TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn recover_restores_the_mode_captured_at_archive_time() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = open_test_db();
        let mgr = ArchiveManager::with_root(tmp.path().join("archives").to_string_lossy().to_string());

        let original = tmp.path().join("original.conf");
        fs::write(&original, b"hello").unwrap();
        fs::set_permissions(&original, fs::Permissions::from_mode(0o640)).unwrap();

        mgr.archive_file(&conn, 1, &original.to_string_lossy(), false).unwrap();

        // Simulate the file having been removed, then recovered with
        // whatever default mode `fs::copy` happens to produce, rather than
        // the original 0o640 — this is the gap `restore_ownership` closes.
        fs::remove_file(&original).unwrap();

        let recovered = mgr.recover_archive(&conn, 1).unwrap();
        assert_eq!(recovered, vec![original.to_string_lossy().to_string()]);

        let restored_mode = fs::metadata(&original).unwrap().permissions().mode() & 0o7777;
        assert_eq!(restored_mode, 0o640, "recover_archive should re-apply the original mode, not whatever fs::copy defaulted to");
    }

    #[test]
    fn restore_ownership_is_a_safe_noop_without_a_metadata_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let archive_src = tmp.path().join("some_file"); // no matching .pkgundo-meta.json
        fs::write(&archive_src, b"x").unwrap();
        let dest = tmp.path().join("dest_file");
        fs::write(&dest, b"x").unwrap();

        // Must not panic or error when the sidecar is missing.
        ArchiveManager::restore_ownership(&archive_src, &dest);
    }

    #[test]
    fn distinct_files_sharing_everything_before_the_last_dot_get_distinct_metadata() {
        // A real case this hit: fontconfig's own cache files, named
        // `<hash>.cache-9`, `<hash>.cache-10`, `<hash>.cache-11` — these all
        // share the same `Path::with_extension` result, which is exactly
        // the bug `meta_path_for` fixes by appending instead of replacing.
        let tmp = tempfile::tempdir().unwrap();
        let conn = open_test_db();
        let mgr = ArchiveManager::with_root(tmp.path().join("archives").to_string_lossy().to_string());

        let src_dir = tmp.path().join("home");
        fs::create_dir_all(&src_dir).unwrap();
        let file_a = src_dir.join("hash.cache-9");
        let file_b = src_dir.join("hash.cache-10");
        fs::write(&file_a, b"nine").unwrap();
        fs::write(&file_b, b"ten").unwrap();
        fs::set_permissions(&file_a, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&file_b, fs::Permissions::from_mode(0o640)).unwrap();

        mgr.archive_file(&conn, 1, &file_a.to_string_lossy(), false).unwrap();
        mgr.archive_file(&conn, 1, &file_b.to_string_lossy(), false).unwrap();

        // Both archived copies and both metadata sidecars must exist
        // independently — none of archive_file's second call for file_b
        // should have overwritten file_a's copy or its metadata.
        let archived_a = mgr.archive_path_for(1, &file_a.to_string_lossy());
        let archived_b = mgr.archive_path_for(1, &file_b.to_string_lossy());
        assert_eq!(fs::read(&archived_a).unwrap(), b"nine");
        assert_eq!(fs::read(&archived_b).unwrap(), b"ten");

        fs::remove_file(&file_a).unwrap();
        fs::remove_file(&file_b).unwrap();
        mgr.recover_archive(&conn, 1).unwrap();

        let mode_a = fs::metadata(&file_a).unwrap().permissions().mode() & 0o7777;
        let mode_b = fs::metadata(&file_b).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode_a, 0o600, "file_a's own mode must survive, not file_b's");
        assert_eq!(mode_b, 0o640, "file_b's own mode must survive, not file_a's");
    }

    /// Reproduces the real bug found on a live VM: fontconfig's numbered
    /// cache candidate files (`<hash>.cache-9`, etc.) turned out to be
    /// symlinks, not regular files. `archive_file`'s symlink branch used to
    /// `fs::read_link` the target and then just discard it — no content
    /// ever got written to the archive destination, yet the function still
    /// logged (and reported) success. Once the original was removed, the
    /// symlink was gone with no way back.
    #[test]
    fn symlinks_are_actually_recoverable_not_silently_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = open_test_db();
        let mgr = ArchiveManager::with_root(tmp.path().join("archives").to_string_lossy().to_string());

        let real_target = tmp.path().join("real-cache-blob");
        fs::write(&real_target, b"shared cache content").unwrap();
        let link = tmp.path().join("hash.cache-9");
        std::os::unix::fs::symlink(&real_target, &link).unwrap();

        mgr.archive_file(&conn, 1, &link.to_string_lossy(), false).unwrap();

        fs::remove_file(&link).unwrap();
        assert!(!link.exists(), "sanity: original symlink is really gone");

        let recovered = mgr.recover_archive(&conn, 1).unwrap();
        assert_eq!(recovered, vec![link.to_string_lossy().to_string()]);

        let meta = fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink(), "recovered entry must be a real symlink, not a copied file");
        assert_eq!(fs::read_link(&link).unwrap(), real_target, "recovered symlink must point at the original target");
    }
}
