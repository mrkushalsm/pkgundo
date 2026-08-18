use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::fingerprint::{capture_fingerprint, compute_sha256, store_fingerprint};

/// Maximum file size to store as a blob (1 MB)
pub const MAX_BLOB_SIZE: u64 = 1 * 1024 * 1024;

/// Store the content of a file as a blob in the DB for future restore.
/// Skips files larger than MAX_BLOB_SIZE or that don't exist.
pub fn store_file_blob(conn: &Connection, txid: i64, path: &Path, phase: &str) -> Result<bool> {
    if !path.exists() || path.is_symlink() || path.is_dir() {
        return Ok(false);
    }

    let meta = fs::metadata(path)?;
    if meta.size() > MAX_BLOB_SIZE {
        log::debug!(
            "BlobStore: skipping {} (size {} > limit {})",
            path.display(), meta.size(), MAX_BLOB_SIZE
        );
        return Ok(false);
    }

    let content = fs::read(path).context(format!("BlobStore: failed to read {}", path.display()))?;
    let sha256 = compute_sha256(path)?;
    let path_str = path.to_string_lossy().to_string();
    let now = Utc::now();

    conn.execute(
        "INSERT OR IGNORE INTO file_blobs
         (txid, path, phase, content, sha256, size, uid, gid, mode, captured_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            txid,
            path_str,
            phase,
            content,
            sha256,
            meta.size() as i64,
            meta.uid() as i64,
            meta.gid() as i64,
            meta.mode() as i64,
            now.to_rfc3339(),
        ],
    )
    .context("BlobStore: failed to insert blob")?;

    log::debug!("BlobStore: stored {} phase={} ({} bytes)", path_str, phase, content.len());
    Ok(true)
}

/// Retrieve a stored blob's content for a path+phase
pub fn get_blob_content(conn: &Connection, txid: i64, path: &str, phase: &str) -> Result<Option<Vec<u8>>> {
    let mut stmt = conn.prepare(
        "SELECT content FROM file_blobs WHERE txid = ?1 AND path = ?2 AND phase = ?3 LIMIT 1",
    )?;

    let mut rows = stmt.query(rusqlite::params![txid, path, phase])?;
    if let Some(row) = rows.next()? {
        let content: Vec<u8> = row.get(0)?;
        Ok(Some(content))
    } else {
        Ok(None)
    }
}

/// Restore a file from its pre-install blob snapshot.
/// Returns true if restoration succeeded.
pub fn restore_from_blob(conn: &Connection, txid: i64, path_str: &str) -> Result<bool> {
    let content = match get_blob_content(conn, txid, path_str, "pre")? {
        Some(c) => c,
        None => return Ok(false),
    };

    let path = Path::new(path_str);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(path, &content)
        .context(format!("BlobStore: failed to restore {}", path_str))?;

    // Restore permissions from DB
    restore_metadata(conn, txid, path_str, path)?;

    log::info!("BlobStore: restored {} from pre-install blob ({} bytes)", path_str, content.len());
    Ok(true)
}

fn restore_metadata(conn: &Connection, txid: i64, path_str: &str, path: &Path) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT uid, gid, mode FROM file_blobs WHERE txid = ?1 AND path = ?2 AND phase = 'pre' LIMIT 1",
    )?;

    let mut rows = stmt.query(rusqlite::params![txid, path_str])?;
    if let Some(row) = rows.next()? {
        let uid: u32 = row.get::<_, i64>(0)? as u32;
        let gid: u32 = row.get::<_, i64>(1)? as u32;
        let mode: u32 = row.get::<_, i64>(2)? as u32;

        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));

        // chown via libc
        let c_path = std::ffi::CString::new(path_str).unwrap_or_default();
        unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
    }
    Ok(())
}

/// Pre-scan /etc and /usr/lib/systemd, storing blobs for small config files.
/// Called before the monitored command is launched.
pub fn pre_scan_configs(conn: &Connection, txid: i64) -> Result<usize> {
    let scan_roots = ["/etc", "/usr/lib/systemd/system"];
    let mut count = 0usize;

    for root in &scan_roots {
        let root_path = Path::new(root);
        if !root_path.exists() {
            continue;
        }

        for entry in walkdir::WalkDir::new(root_path)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            match store_file_blob(conn, txid, path, "pre") {
                Ok(true) => count += 1,
                Ok(false) => {}
                Err(e) => log::debug!("BlobStore pre-scan skip {}: {}", path.display(), e),
            }

            // Record a "pre" fingerprint for every visited file (not just blob-eligible
            // ones) so rollback can later tell whether a config was modified after install,
            // even for files too large to store as a blob.
            match capture_fingerprint(txid, path, "pre") {
                Ok(fp) => {
                    if let Err(e) = store_fingerprint(conn, &fp) {
                        log::debug!("Fingerprint pre-scan skip {}: {}", path.display(), e);
                    }
                }
                Err(e) => log::debug!("Fingerprint capture skip {}: {}", path.display(), e),
            }
        }
    }

    log::info!("BlobStore: pre-scanned {} files for txid={}", count, txid);
    Ok(count)
}

