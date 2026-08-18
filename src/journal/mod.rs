use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// A single mutation event recorded in the journal
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationRecord {
    pub id: Option<i64>,
    pub txid: i64,
    pub pid: Option<i32>,
    pub operation: String, // create, modify, delete, rename, chmod
    pub path: String,
    pub timestamp: DateTime<Utc>,
    pub file_category: String, // Binary, Config, Cache, etc.
    pub pre_hash: Option<String>,  // SHA256 before modification
    pub post_hash: Option<String>, // SHA256 after modification
}

/// Append a mutation record to the journal (SQLite mutations table).
/// This is the core append-only journaling operation.
pub fn append_mutation(conn: &Connection, record: &MutationRecord) -> Result<i64> {
    conn.execute(
        "INSERT INTO mutations
         (txid, pid, operation, path, timestamp, file_category, pre_hash, post_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            record.txid,
            record.pid,
            record.operation,
            record.path,
            record.timestamp.to_rfc3339(),
            record.file_category,
            record.pre_hash,
            record.post_hash,
        ],
    )
    .context("Failed to append mutation record")?;

    Ok(conn.last_insert_rowid())
}

/// Retrieve all mutations for a given transaction, ordered by timestamp
pub fn get_mutations(conn: &Connection, txid: i64) -> Result<Vec<MutationRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, txid, pid, operation, path, timestamp, file_category, pre_hash, post_hash
         FROM mutations WHERE txid = ?1 ORDER BY timestamp ASC",
    )?;

    let records = stmt
        .query_map(rusqlite::params![txid], |row| {
            let ts_str: String = row.get(5)?;
            Ok(MutationRecord {
                id: Some(row.get(0)?),
                txid: row.get(1)?,
                pid: row.get(2)?,
                operation: row.get(3)?,
                path: row.get(4)?,
                timestamp: ts_str
                    .parse::<DateTime<Utc>>()
                    .unwrap_or_else(|_| Utc::now()),
                file_category: row.get(6)?,
                pre_hash: row.get(7)?,
                post_hash: row.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("Failed to load mutations")?;

    Ok(records)
}

/// Count mutations by type for a transaction (summary stats)
pub struct MutationSummary {
    pub created: usize,
    pub modified: usize,
    pub deleted: usize,
    pub renamed: usize,
    pub total: usize,
}

pub fn summarize_mutations(mutations: &[MutationRecord]) -> MutationSummary {
    let created = mutations.iter().filter(|m| m.operation == "create").count();
    let modified = mutations.iter().filter(|m| m.operation == "modify").count();
    let deleted = mutations.iter().filter(|m| m.operation == "delete").count();
    let renamed = mutations
        .iter()
        .filter(|m| m.operation == "rename_to" || m.operation == "rename_from" || m.operation == "rename")
        .count();
    MutationSummary {
        created,
        modified,
        deleted,
        renamed,
        total: mutations.len(),
    }
}
