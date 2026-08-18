use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Comparison result between two fingerprints
#[derive(Debug, Clone, PartialEq)]
pub enum FingerprintDiff {
    /// File is byte-for-byte identical
    Unchanged,
    /// File content or metadata has changed
    Modified,
    /// File no longer exists
    Missing,
    /// No baseline fingerprint to compare against
    New,
}

/// A complete file fingerprint: hash + metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub id: Option<i64>,
    pub txid: i64,
    pub path: String,
    pub phase: String, // "pre" or "post"
    pub sha256: Option<String>,
    pub size: Option<u64>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: Option<u32>,
    pub mtime: Option<i64>,
    pub captured_at: DateTime<Utc>,
    pub is_symlink: bool,
    pub symlink_target: Option<String>,
}

/// Compute SHA256 hash of a file's contents
pub fn compute_sha256(path: &Path) -> Result<String> {
    let data = fs::read(path).context(format!("Failed to read file for hashing: {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let result = hasher.finalize();
    Ok(hex::encode(result))
}

/// Capture a complete fingerprint of a file (or symlink) at the current moment
pub fn capture_fingerprint(txid: i64, path: &Path, phase: &str) -> Result<FileFingerprint> {
    let captured_at = Utc::now();

    // Handle symlinks specially
    if path.is_symlink() {
        let target = fs::read_link(path)
            .map(|t| t.to_string_lossy().to_string())
            .ok();
        let meta = fs::symlink_metadata(path)?;
        return Ok(FileFingerprint {
            id: None,
            txid,
            path: path.to_string_lossy().to_string(),
            phase: phase.to_string(),
            sha256: None, // symlinks don't have content to hash
            size: Some(meta.size()),
            uid: Some(meta.uid()),
            gid: Some(meta.gid()),
            mode: Some(meta.mode()),
            mtime: Some(meta.mtime()),
            captured_at,
            is_symlink: true,
            symlink_target: target,
        });
    }

    if !path.exists() {
        return Ok(FileFingerprint {
            id: None,
            txid,
            path: path.to_string_lossy().to_string(),
            phase: phase.to_string(),
            sha256: None,
            size: None,
            uid: None,
            gid: None,
            mode: None,
            mtime: None,
            captured_at,
            is_symlink: false,
            symlink_target: None,
        });
    }

    let hash = compute_sha256(path).ok();
    let meta = fs::metadata(path)?;

    Ok(FileFingerprint {
        id: None,
        txid,
        path: path.to_string_lossy().to_string(),
        phase: phase.to_string(),
        sha256: hash,
        size: Some(meta.size()),
        uid: Some(meta.uid()),
        gid: Some(meta.gid()),
        mode: Some(meta.mode()),
        mtime: Some(meta.mtime()),
        captured_at,
        is_symlink: false,
        symlink_target: None,
    })
}

/// Store a fingerprint in the database
pub fn store_fingerprint(conn: &Connection, fp: &FileFingerprint) -> Result<i64> {
    conn.execute(
        "INSERT INTO fingerprints
         (txid, path, phase, sha256, size, uid, gid, mode, mtime, captured_at, is_symlink, symlink_target)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            fp.txid,
            fp.path,
            fp.phase,
            fp.sha256,
            fp.size.map(|s| s as i64),
            fp.uid.map(|u| u as i64),
            fp.gid.map(|g| g as i64),
            fp.mode.map(|m| m as i64),
            fp.mtime,
            fp.captured_at.to_rfc3339(),
            fp.is_symlink as i32,
            fp.symlink_target,
        ],
    )
    .context("Failed to store fingerprint")?;
    Ok(conn.last_insert_rowid())
}

/// Load a specific fingerprint by path and phase
pub fn get_fingerprint_for_path(
    conn: &Connection,
    txid: i64,
    path: &str,
    phase: &str,
) -> Result<Option<FileFingerprint>> {
    let mut stmt = conn.prepare(
        "SELECT id, txid, path, phase, sha256, size, uid, gid, mode, mtime, captured_at, is_symlink, symlink_target
         FROM fingerprints WHERE txid = ?1 AND path = ?2 AND phase = ?3 LIMIT 1",
    )?;

    let mut rows = stmt.query_map(rusqlite::params![txid, path, phase], |row| {
        let ts_str: String = row.get(10)?;
        Ok(FileFingerprint {
            id: Some(row.get(0)?),
            txid: row.get(1)?,
            path: row.get(2)?,
            phase: row.get(3)?,
            sha256: row.get(4)?,
            size: row.get::<_, Option<i64>>(5)?.map(|s| s as u64),
            uid: row.get::<_, Option<i64>>(6)?.map(|u| u as u32),
            gid: row.get::<_, Option<i64>>(7)?.map(|g| g as u32),
            mode: row.get::<_, Option<i64>>(8)?.map(|m| m as u32),
            mtime: row.get(9)?,
            captured_at: ts_str.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now()),
            is_symlink: row.get::<_, i32>(11)? != 0,
            symlink_target: row.get(12)?,
        })
    })?;

    if let Some(row) = rows.next() {
        Ok(Some(row?))
    } else {
        Ok(None)
    }
}

/// Compare a pre-install fingerprint against the current state of the file on disk.
/// Returns a FingerprintDiff indicating what changed.
pub fn compare_with_current(baseline: &FileFingerprint) -> FingerprintDiff {
    let path = Path::new(&baseline.path);

    if !path.exists() && !path.is_symlink() {
        return FingerprintDiff::Missing;
    }

    if baseline.sha256.is_none() {
        // Baseline didn't exist when captured → this is a new file
        return FingerprintDiff::New;
    }

    // Compute current hash
    if path.is_symlink() || path.is_dir() {
        // For symlinks/dirs, compare mtime
        if let Ok(meta) = fs::symlink_metadata(path) {
            let current_mtime = meta.mtime();
            if Some(current_mtime) == baseline.mtime {
                return FingerprintDiff::Unchanged;
            } else {
                return FingerprintDiff::Modified;
            }
        }
        return FingerprintDiff::Modified;
    }

    match compute_sha256(path) {
        Ok(current_hash) => {
            if Some(&current_hash) == baseline.sha256.as_ref() {
                FingerprintDiff::Unchanged
            } else {
                FingerprintDiff::Modified
            }
        }
        Err(_) => FingerprintDiff::Missing,
    }
}
