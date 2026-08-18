use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fs;

/// Type of user/group change
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UserEventKind {
    UserAdded,
    UserRemoved,
    UserModified,
    GroupAdded,
    GroupRemoved,
    GroupModified,
}

impl UserEventKind {
    pub fn as_str(&self) -> &str {
        match self {
            UserEventKind::UserAdded => "user_added",
            UserEventKind::UserRemoved => "user_removed",
            UserEventKind::UserModified => "user_modified",
            UserEventKind::GroupAdded => "group_added",
            UserEventKind::GroupRemoved => "group_removed",
            UserEventKind::GroupModified => "group_modified",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "user_added" => Some(UserEventKind::UserAdded),
            "user_removed" => Some(UserEventKind::UserRemoved),
            "user_modified" => Some(UserEventKind::UserModified),
            "group_added" => Some(UserEventKind::GroupAdded),
            "group_removed" => Some(UserEventKind::GroupRemoved),
            "group_modified" => Some(UserEventKind::GroupModified),
            _ => None,
        }
    }
}

/// A user or group change event detected during a transaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserEvent {
    pub id: Option<i64>,
    pub txid: i64,
    pub kind: UserEventKind,
    pub name: String,
    pub pre_state: Option<String>,
    pub timestamp: DateTime<Utc>,
}

/// Snapshot of /etc/passwd and /etc/group line sets
#[derive(Debug, Clone)]
pub struct UserGroupSnapshot {
    pub passwd_lines: Vec<String>,
    pub group_lines: Vec<String>,
}

impl UserGroupSnapshot {
    /// Take a snapshot of current /etc/passwd and /etc/group
    pub fn capture() -> Result<Self> {
        let passwd = fs::read_to_string("/etc/passwd")
            .context("Failed to read /etc/passwd")?;
        let group = fs::read_to_string("/etc/group")
            .context("Failed to read /etc/group")?;

        Ok(Self {
            passwd_lines: passwd.lines().map(String::from).collect(),
            group_lines: group.lines().map(String::from).collect(),
        })
    }

    /// Serialize snapshot to JSON for DB storage
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "passwd": self.passwd_lines,
            "group": self.group_lines,
        })
        .to_string()
    }

}

/// Compare two snapshots and return detected change events
pub fn diff_snapshots(
    txid: i64,
    before: &UserGroupSnapshot,
    after: &UserGroupSnapshot,
) -> Vec<UserEvent> {
    let mut events = Vec::new();

    // Users added (lines in after but not before)
    for line in &after.passwd_lines {
        if !before.passwd_lines.contains(line) {
            let name = line.split(':').next().unwrap_or("unknown").to_string();
            if !name.is_empty() && !name.starts_with('#') {
                events.push(UserEvent {
                    id: None,
                    txid,
                    kind: UserEventKind::UserAdded,
                    name,
                    pre_state: None,
                    timestamp: Utc::now(),
                });
            }
        }
    }

    // Users removed
    for line in &before.passwd_lines {
        if !after.passwd_lines.contains(line) {
            let name = line.split(':').next().unwrap_or("unknown").to_string();
            if !name.is_empty() && !name.starts_with('#') {
                events.push(UserEvent {
                    id: None,
                    txid,
                    kind: UserEventKind::UserRemoved,
                    name,
                    pre_state: Some(line.clone()),
                    timestamp: Utc::now(),
                });
            }
        }
    }

    // Groups added
    for line in &after.group_lines {
        if !before.group_lines.contains(line) {
            let name = line.split(':').next().unwrap_or("unknown").to_string();
            if !name.is_empty() && !name.starts_with('#') {
                events.push(UserEvent {
                    id: None,
                    txid,
                    kind: UserEventKind::GroupAdded,
                    name,
                    pre_state: None,
                    timestamp: Utc::now(),
                });
            }
        }
    }

    // Groups removed
    for line in &before.group_lines {
        if !after.group_lines.contains(line) {
            let name = line.split(':').next().unwrap_or("unknown").to_string();
            if !name.is_empty() && !name.starts_with('#') {
                events.push(UserEvent {
                    id: None,
                    txid,
                    kind: UserEventKind::GroupRemoved,
                    name,
                    pre_state: Some(line.clone()),
                    timestamp: Utc::now(),
                });
            }
        }
    }

    events
}

/// Store a snapshot in the DB keyed by txid + phase
pub fn store_snapshot(conn: &Connection, txid: i64, phase: &str, snapshot: &UserGroupSnapshot) -> Result<()> {
    let json = snapshot.to_json();
    conn.execute(
        "INSERT OR REPLACE INTO user_snapshots (txid, phase, snapshot_json, captured_at)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![txid, phase, json, Utc::now().to_rfc3339()],
    )
    .context("Failed to store user/group snapshot")?;
    Ok(())
}

/// Record user events to DB
pub fn record_user_events(conn: &Connection, events: &[UserEvent]) -> Result<()> {
    for event in events {
        conn.execute(
            "INSERT OR IGNORE INTO user_events
             (txid, kind, name, pre_state, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                event.txid,
                event.kind.as_str(),
                event.name,
                event.pre_state,
                event.timestamp.to_rfc3339(),
            ],
        )?;
    }
    Ok(())
}

/// Get all user events for a transaction
pub fn get_user_events(conn: &Connection, txid: i64) -> Result<Vec<UserEvent>> {
    let mut stmt = conn.prepare(
        "SELECT id, txid, kind, name, pre_state, timestamp
         FROM user_events WHERE txid = ?1 ORDER BY timestamp ASC",
    )?;

    let events = stmt
        .query_map(rusqlite::params![txid], |row| {
            let ts: String = row.get(5)?;
            let kind_str: String = row.get(2)?;
            Ok(UserEvent {
                id: Some(row.get(0)?),
                txid: row.get(1)?,
                kind: UserEventKind::from_str(&kind_str).unwrap_or(UserEventKind::UserModified),
                name: row.get(3)?,
                pre_state: row.get(4)?,
                timestamp: ts.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now()),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("Failed to load user events")?;

    Ok(events)
}

/// Rollback user/group changes: remove added users/groups with strong warnings.
pub fn rollback_user_events(conn: &Connection, txid: i64, dry_run: bool) -> Result<Vec<String>> {
    let events = get_user_events(conn, txid)?;
    let mut reversed = Vec::new();

    for event in &events {
        match &event.kind {
            UserEventKind::UserAdded => {
                log::warn!(
                    "UserTracker: user '{}' was added during txid={}. Rolling back.",
                    event.name, txid
                );
                if !dry_run {
                    let result = std::process::Command::new("userdel")
                        .args(["--remove", &event.name])
                        .status();
                    match result {
                        Ok(s) if s.success() => {
                            reversed.push(format!("userdel --remove {}", event.name));
                        }
                        _ => {
                            log::warn!("UserTracker: could not remove user '{}'", event.name);
                        }
                    }
                } else {
                    reversed.push(format!("[dry-run] userdel --remove {}", event.name));
                }
            }
            UserEventKind::GroupAdded => {
                log::warn!(
                    "UserTracker: group '{}' was added during txid={}. Rolling back.",
                    event.name, txid
                );
                if !dry_run {
                    let result = std::process::Command::new("groupdel")
                        .arg(&event.name)
                        .status();
                    match result {
                        Ok(s) if s.success() => {
                            reversed.push(format!("groupdel {}", event.name));
                        }
                        _ => {
                            log::warn!("UserTracker: could not remove group '{}'", event.name);
                        }
                    }
                } else {
                    reversed.push(format!("[dry-run] groupdel {}", event.name));
                }
            }
            _ => {
                // UserRemoved/GroupRemoved/Modified — skip (cannot reliably reverse)
                log::debug!("UserTracker: skipping non-reversible event {:?}", event.kind);
            }
        }
    }

    Ok(reversed)
}
