use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fs;
use std::process::Command;

/// The systemctl action detected in a monitored process
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ServiceAction {
    Enable,
    Disable,
    Start,
    Stop,
    Restart,
    Reload,
    DaemonReload,
}

impl ServiceAction {
    pub fn as_str(&self) -> &str {
        match self {
            ServiceAction::Enable => "enable",
            ServiceAction::Disable => "disable",
            ServiceAction::Start => "start",
            ServiceAction::Stop => "stop",
            ServiceAction::Restart => "restart",
            ServiceAction::Reload => "reload",
            ServiceAction::DaemonReload => "daemon-reload",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "enable" => Some(ServiceAction::Enable),
            "disable" => Some(ServiceAction::Disable),
            "start" => Some(ServiceAction::Start),
            "stop" => Some(ServiceAction::Stop),
            "restart" => Some(ServiceAction::Restart),
            "reload" => Some(ServiceAction::Reload),
            "daemon-reload" => Some(ServiceAction::DaemonReload),
            _ => None,
        }
    }

    /// The reverse action to apply during rollback
    pub fn inverse(&self) -> Option<ServiceAction> {
        match self {
            ServiceAction::Enable => Some(ServiceAction::Disable),
            ServiceAction::Disable => Some(ServiceAction::Enable),
            ServiceAction::Start => Some(ServiceAction::Stop),
            ServiceAction::Stop => None, // don't restart a stopped service
            ServiceAction::Restart => None,
            ServiceAction::Reload => None,
            ServiceAction::DaemonReload => None,
        }
    }
}

/// A recorded service state change event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceEvent {
    pub id: Option<i64>,
    pub txid: i64,
    pub service_name: String,
    pub action: ServiceAction,
    pub pre_state: Option<String>,
    pub timestamp: DateTime<Utc>,
}

/// Parse a null-delimited cmdline from /proc/<pid>/cmdline and detect systemctl calls
pub fn parse_cmdline_for_systemctl(cmdline_raw: &[u8]) -> Option<(ServiceAction, Vec<String>)> {
    let args: Vec<String> = cmdline_raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();

    if args.is_empty() {
        return None;
    }

    let bin = std::path::Path::new(&args[0])
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&args[0])
        .to_string();

    if bin != "systemctl" {
        return None;
    }

    // Find the action verb
    let action_str = args.iter().skip(1).find(|a| !a.starts_with('-'))?;
    let action = ServiceAction::from_str(action_str)?;

    // Collect service names (non-flag args after the action verb)
    let services: Vec<String> = args
        .iter()
        .skip(1)
        .skip_while(|a| **a != *action_str)
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .cloned()
        .collect();

    Some((action, services))
}

/// Read /proc/<pid>/cmdline as raw bytes
pub fn read_cmdline(pid: i32) -> Option<Vec<u8>> {
    fs::read(format!("/proc/{}/cmdline", pid)).ok()
}

/// Record a service event to the DB
pub fn record_service_event(conn: &Connection, event: &ServiceEvent) -> Result<i64> {
    conn.execute(
        "INSERT OR IGNORE INTO service_events
         (txid, service_name, action, pre_state, timestamp)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            event.txid,
            event.service_name,
            event.action.as_str(),
            event.pre_state,
            event.timestamp.to_rfc3339(),
        ],
    )
    .context("Failed to record service event")?;
    Ok(conn.last_insert_rowid())
}

/// Load all service events for a transaction
pub fn get_service_events(conn: &Connection, txid: i64) -> Result<Vec<ServiceEvent>> {
    let mut stmt = conn.prepare(
        "SELECT id, txid, service_name, action, pre_state, timestamp
         FROM service_events WHERE txid = ?1 ORDER BY timestamp ASC",
    )?;

    let events = stmt
        .query_map(rusqlite::params![txid], |row| {
            let ts_str: String = row.get(5)?;
            let action_str: String = row.get(3)?;
            Ok(ServiceEvent {
                id: Some(row.get(0)?),
                txid: row.get(1)?,
                service_name: row.get(2)?,
                action: ServiceAction::from_str(&action_str).unwrap_or(ServiceAction::Reload),
                pre_state: row.get(4)?,
                timestamp: ts_str.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now()),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("Failed to load service events")?;

    Ok(events)
}

/// Detect systemctl calls from the process tree for a transaction.
/// Called after the transaction completes, scans process_tree cmdlines.
pub fn detect_service_changes_from_pids(conn: &Connection, txid: i64, pids: &[i32]) -> Result<()> {
    for &pid in pids {
        if let Some(raw) = read_cmdline(pid) {
            if let Some((action, services)) = parse_cmdline_for_systemctl(&raw) {
                for service in services {
                    // Query pre-state before the action was taken (best-effort)
                    let pre_state = match &action {
                        ServiceAction::Enable => Some("disabled".to_string()),
                        ServiceAction::Disable => Some("enabled".to_string()),
                        ServiceAction::Start => Some("inactive".to_string()),
                        ServiceAction::Stop => Some("active".to_string()),
                        _ => None,
                    };

                    let event = ServiceEvent {
                        id: None,
                        txid,
                        service_name: service.clone(),
                        action: action.clone(),
                        pre_state,
                        timestamp: Utc::now(),
                    };

                    record_service_event(conn, &event).ok();
                    log::info!("ServiceTracker: detected systemctl {} {}", action.as_str(), service);
                }
            }
        }
    }
    Ok(())
}

/// Rollback Step F: reverse all service operations from a transaction
pub fn rollback_service_events(conn: &Connection, txid: i64, dry_run: bool) -> Result<Vec<String>> {
    let events = get_service_events(conn, txid)?;
    let mut reversed = Vec::new();

    // Process in reverse order (last action first)
    for event in events.iter().rev() {
        if let Some(inverse_action) = event.action.inverse() {
            let service = &event.service_name;
            log::info!(
                "ServiceTracker rollback: systemctl {} {}",
                inverse_action.as_str(),
                service
            );

            if !dry_run {
                let result = Command::new("systemctl")
                    .args([inverse_action.as_str(), service])
                    .status();

                match result {
                    Ok(status) if status.success() => {
                        reversed.push(format!("systemctl {} {}", inverse_action.as_str(), service));
                        log::info!("ServiceTracker: reverted {} → {}", service, inverse_action.as_str());
                    }
                    Ok(s) => {
                        log::warn!(
                            "ServiceTracker: systemctl {} {} failed (exit {:?})",
                            inverse_action.as_str(), service, s.code()
                        );
                    }
                    Err(e) => {
                        log::warn!("ServiceTracker: could not run systemctl: {}", e);
                    }
                }
            } else {
                reversed.push(format!(
                    "[dry-run] systemctl {} {}",
                    inverse_action.as_str(), service
                ));
            }
        }
    }

    // Always run daemon-reload after service changes
    if !reversed.is_empty() && !dry_run {
        let _ = Command::new("systemctl").arg("daemon-reload").status();
    }

    Ok(reversed)
}
