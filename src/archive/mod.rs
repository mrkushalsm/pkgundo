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

        // Copy file (not symlink target) to archive
        if src.is_symlink() {
            let target = fs::read_link(src)?;
            // Store symlink info in metadata, don't copy symlink itself
            log::debug!(
                "ArchiveManager: archiving symlink {} -> {}",
                original_path,
                target.display()
            );
        } else {
            fs::copy(src, &dest).context(format!(
                "Failed to copy {} to archive {}",
                original_path,
                dest.display()
            ))?;
        }

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
        };

        // Write JSON metadata alongside archive
        let meta_path = dest.with_extension("pkgundo-meta.json");
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
