use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// The execution status of a transaction
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TransactionStatus {
    Running,
    Completed,
    Failed,
    RolledBack,
    /// A long-lived accumulation bucket for a tracked app's mutations across
    /// many separate launches, left open (end_time NULL) until untracked.
    Tracking,
    /// Terminal state for a tracked app's bucket transaction once untracked.
    /// Distinct from RolledBack: untracking doesn't necessarily mean any
    /// mutation was actually reversed yet.
    Untracked,
}

impl TransactionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransactionStatus::Running => "running",
            TransactionStatus::Completed => "completed",
            TransactionStatus::Failed => "failed",
            TransactionStatus::RolledBack => "rolled_back",
            TransactionStatus::Tracking => "tracking",
            TransactionStatus::Untracked => "untracked",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "running" => TransactionStatus::Running,
            "completed" => TransactionStatus::Completed,
            "failed" => TransactionStatus::Failed,
            "rolled_back" => TransactionStatus::RolledBack,
            "tracking" => TransactionStatus::Tracking,
            "untracked" => TransactionStatus::Untracked,
            _ => TransactionStatus::Failed,
        }
    }
}

/// The detected package manager for a transaction
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PackageManager {
    Pacman,
    Apt,
    Dnf,
    Rpm,
    Dpkg,
    Pip,
    Script,  // make install, ./installer.sh, etc.
    Unknown,
}

impl PackageManager {
    pub fn as_str(&self) -> &'static str {
        match self {
            PackageManager::Pacman => "pacman",
            PackageManager::Apt => "apt",
            PackageManager::Dnf => "dnf",
            PackageManager::Rpm => "rpm",
            PackageManager::Dpkg => "dpkg",
            PackageManager::Pip => "pip",
            PackageManager::Script => "script",
            PackageManager::Unknown => "unknown",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "pacman" => PackageManager::Pacman,
            "apt" | "apt-get" => PackageManager::Apt,
            "dnf" | "yum" => PackageManager::Dnf,
            "rpm" => PackageManager::Rpm,
            "dpkg" => PackageManager::Dpkg,
            "pip" | "pip3" => PackageManager::Pip,
            "script" => PackageManager::Script,
            _ => PackageManager::Unknown,
        }
    }

    /// Detect package manager from command arguments
    pub fn detect_from_command(args: &[String]) -> Self {
        if args.is_empty() {
            return PackageManager::Unknown;
        }
        let bin = std::path::Path::new(&args[0])
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&args[0]);

        match bin {
            "pacman" => PackageManager::Pacman,
            "apt" | "apt-get" => PackageManager::Apt,
            "dnf" | "yum" => PackageManager::Dnf,
            "rpm" => PackageManager::Rpm,
            "dpkg" => PackageManager::Dpkg,
            "pip" | "pip3" | "pip2" => PackageManager::Pip,
            _ => {
                // Script-type installer
                if args[0].ends_with(".sh") || args[0].starts_with("./") || bin == "make" {
                    PackageManager::Script
                } else {
                    PackageManager::Unknown
                }
            }
        }
    }
}

/// Core transaction record that represents a monitored command execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub txid: i64,
    pub command: String,
    pub package_manager: PackageManager,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub status: TransactionStatus,
    pub pid_root: Option<i32>,
    pub rollback_mode: String,
    pub notes: Option<String>,
}

impl Transaction {
    /// Determine the package name(s) from the command arguments, best-effort
    pub fn extract_package_name(&self) -> Option<String> {
        let parts: Vec<&str> = self.command.split_whitespace().collect();
        match self.package_manager {
            PackageManager::Pacman => {
                // pacman -S <pkg> or pacman -Syu <pkg>
                parts.iter().skip(1)
                    .find(|p| !p.starts_with('-'))
                    .map(|s| s.to_string())
            }
            PackageManager::Apt => {
                // apt install <pkg>
                if let Some(idx) = parts.iter().position(|p| *p == "install") {
                    parts.get(idx + 1).map(|s| s.to_string())
                } else {
                    None
                }
            }
            PackageManager::Dnf => {
                // dnf install <pkg>
                if let Some(idx) = parts.iter().position(|p| *p == "install") {
                    parts.get(idx + 1).map(|s| s.to_string())
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// Create a new transaction in the database, returning its ID
pub fn create_transaction(conn: &Connection, command: &str, args: &[String]) -> Result<i64> {
    let pm = PackageManager::detect_from_command(args);
    let now = Utc::now();

    conn.execute(
        "INSERT INTO transactions (command, package_manager, start_time, status, rollback_mode)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            command,
            pm.as_str(),
            now.to_rfc3339(),
            TransactionStatus::Running.as_str(),
            "conservative"
        ],
    )
    .context("Failed to insert transaction")?;

    let txid = conn.last_insert_rowid();
    log::info!("Created transaction txid={} for command: {}", txid, command);
    Ok(txid)
}

/// Load a transaction by ID from the database
pub fn load_transaction(conn: &Connection, txid: i64) -> Result<Transaction> {
    let mut stmt = conn.prepare(
        "SELECT txid, command, package_manager, start_time, end_time, status, pid_root, rollback_mode, notes
         FROM transactions WHERE txid = ?1",
    )?;

    let tx = stmt.query_row(rusqlite::params![txid], |row| {
        let start_str: String = row.get(3)?;
        let end_str: Option<String> = row.get(4)?;
        let status_str: String = row.get(5)?;
        let pm_str: String = row.get(2)?;

        Ok(Transaction {
            txid: row.get(0)?,
            command: row.get(1)?,
            package_manager: PackageManager::from_str(&pm_str),
            start_time: start_str.parse::<DateTime<Utc>>().unwrap_or(Utc::now()),
            end_time: end_str.and_then(|s| s.parse::<DateTime<Utc>>().ok()),
            status: TransactionStatus::from_str(&status_str),
            pid_root: row.get(6)?,
            rollback_mode: row.get(7).unwrap_or_else(|_| "conservative".to_string()),
            notes: row.get(8)?,
        })
    })
    .context(format!("Transaction {} not found", txid))?;

    Ok(tx)
}

/// Load all transactions ordered by start time
pub fn load_all_transactions(conn: &Connection) -> Result<Vec<Transaction>> {
    let mut stmt = conn.prepare(
        "SELECT txid, command, package_manager, start_time, end_time, status, pid_root, rollback_mode, notes
         FROM transactions ORDER BY start_time DESC",
    )?;

    let txs = stmt.query_map([], |row| {
        let start_str: String = row.get(3)?;
        let end_str: Option<String> = row.get(4)?;
        let status_str: String = row.get(5)?;
        let pm_str: String = row.get(2)?;

        Ok(Transaction {
            txid: row.get(0)?,
            command: row.get(1)?,
            package_manager: PackageManager::from_str(&pm_str),
            start_time: start_str.parse::<DateTime<Utc>>().unwrap_or(Utc::now()),
            end_time: end_str.and_then(|s| s.parse::<DateTime<Utc>>().ok()),
            status: TransactionStatus::from_str(&status_str),
            pid_root: row.get(6)?,
            rollback_mode: row.get(7).unwrap_or_else(|_| "conservative".to_string()),
            notes: row.get(8)?,
        })
    })?
    .collect::<rusqlite::Result<Vec<_>>>()
    .context("Failed to load transactions")?;

    Ok(txs)
}

/// Update the status of a transaction
pub fn update_transaction_status(
    conn: &Connection,
    txid: i64,
    status: TransactionStatus,
    pid_root: Option<i32>,
) -> Result<()> {
    let now = Utc::now();
    conn.execute(
        "UPDATE transactions SET status = ?1, end_time = ?2, pid_root = COALESCE(?3, pid_root)
         WHERE txid = ?4",
        rusqlite::params![
            status.as_str(),
            now.to_rfc3339(),
            pid_root,
            txid
        ],
    )?;
    Ok(())
}
