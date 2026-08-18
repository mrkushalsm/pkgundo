use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::fs;

pub const PKGUNDO_DB_PATH: &str = "/var/lib/pkgundo/pkgundo.db";
pub const PKGUNDO_ROOT: &str = "/var/lib/pkgundo";

/// Initialize the SQLite database and create all required tables.
/// This is idempotent — safe to call on every startup.
pub fn init_db(db_path: &str) -> Result<Connection> {
    // Ensure the directory exists
    let db_dir = std::path::Path::new(db_path)
        .parent()
        .unwrap_or(std::path::Path::new(PKGUNDO_ROOT));
    fs::create_dir_all(db_dir)
        .context(format!("Failed to create pkgundo storage dir: {}", db_dir.display()))?;

    let conn = Connection::open(db_path)
        .context(format!("Failed to open database at {}", db_path))?;

    // Enable WAL mode for better concurrent access
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

    // Make the directory world-readable so non-root can run inspect/timeline/status
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(db_dir, fs::Permissions::from_mode(0o755));
    }

    // ── Transactions table ──────────────────────────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS transactions (
            txid            INTEGER PRIMARY KEY AUTOINCREMENT,
            command         TEXT NOT NULL,
            package_manager TEXT NOT NULL DEFAULT 'unknown',
            start_time      TEXT NOT NULL,
            end_time        TEXT,
            status          TEXT NOT NULL DEFAULT 'running',
            pid_root        INTEGER,
            rollback_mode   TEXT NOT NULL DEFAULT 'conservative',
            notes           TEXT
        );",
    )
    .context("Failed to create transactions table")?;

    // ── Mutations table (append-only journal) ───────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS mutations (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            txid            INTEGER NOT NULL REFERENCES transactions(txid),
            pid             INTEGER,
            operation       TEXT NOT NULL,  -- create|modify|delete|rename|chmod
            path            TEXT NOT NULL,
            timestamp       TEXT NOT NULL,
            file_category   TEXT NOT NULL DEFAULT 'Unknown',
            pre_hash        TEXT,           -- SHA256 before modification
            post_hash       TEXT,           -- SHA256 after modification
            UNIQUE(txid, operation, path)   -- dedup identical events
        );",
    )
    .context("Failed to create mutations table")?;

    // ── Fingerprints table ──────────────────────────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS fingerprints (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            txid            INTEGER NOT NULL REFERENCES transactions(txid),
            path            TEXT NOT NULL,
            phase           TEXT NOT NULL,  -- 'pre' or 'post'
            sha256          TEXT,
            size            INTEGER,
            uid             INTEGER,
            gid             INTEGER,
            mode            INTEGER,
            mtime           INTEGER,
            captured_at     TEXT NOT NULL,
            is_symlink      INTEGER NOT NULL DEFAULT 0,
            symlink_target  TEXT,
            UNIQUE(txid, path, phase)
        );",
    )
    .context("Failed to create fingerprints table")?;

    // ── Archives table ──────────────────────────────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS archives (
            id                      INTEGER PRIMARY KEY AUTOINCREMENT,
            txid                    INTEGER NOT NULL REFERENCES transactions(txid),
            original_path           TEXT NOT NULL,
            archive_path            TEXT NOT NULL,
            modified_after_install  INTEGER NOT NULL DEFAULT 0,
            archived_at             TEXT NOT NULL,
            UNIQUE(txid, original_path)
        );",
    )
    .context("Failed to create archives table")?;

    // ── Process tree table ──────────────────────────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS process_tree (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            txid    INTEGER NOT NULL REFERENCES transactions(txid),
            pid     INTEGER NOT NULL,
            ppid    INTEGER,
            name    TEXT,
            UNIQUE(txid, pid)
        );",
    )
    .context("Failed to create process_tree table")?;

    // ── Indexes for common queries ──────────────────────────────────────────
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_mutations_txid ON mutations(txid);
         CREATE INDEX IF NOT EXISTS idx_fingerprints_txid_path ON fingerprints(txid, path);
         CREATE INDEX IF NOT EXISTS idx_archives_txid ON archives(txid);
         CREATE INDEX IF NOT EXISTS idx_process_tree_txid ON process_tree(txid);",
    )?;

    // ── Phase 9: Service events table ──────────────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS service_events (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            txid            INTEGER NOT NULL REFERENCES transactions(txid),
            service_name    TEXT NOT NULL,
            action          TEXT NOT NULL,  -- enable|disable|start|stop|restart|reload
            pre_state       TEXT,           -- enabled|disabled|active|inactive (before action)
            timestamp       TEXT NOT NULL,
            UNIQUE(txid, service_name, action)
        );
        CREATE INDEX IF NOT EXISTS idx_service_events_txid ON service_events(txid);",
    )
    .context("Failed to create service_events table")?;

    // ── Phase 9: File blobs table (pre-install content snapshots) ───────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_blobs (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            txid            INTEGER NOT NULL REFERENCES transactions(txid),
            path            TEXT NOT NULL,
            phase           TEXT NOT NULL,  -- 'pre' or 'post'
            content         BLOB NOT NULL,
            sha256          TEXT NOT NULL,
            size            INTEGER NOT NULL,
            uid             INTEGER,
            gid             INTEGER,
            mode            INTEGER,
            captured_at     TEXT NOT NULL,
            UNIQUE(txid, path, phase)
        );
        CREATE INDEX IF NOT EXISTS idx_file_blobs_txid_path ON file_blobs(txid, path);",
    )
    .context("Failed to create file_blobs table")?;

    // ── Phase 9: User/group events table ───────────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS user_events (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            txid        INTEGER NOT NULL REFERENCES transactions(txid),
            kind        TEXT NOT NULL,  -- user_added|user_removed|group_added|group_removed|...
            name        TEXT NOT NULL,
            pre_state   TEXT,
            timestamp   TEXT NOT NULL,
            UNIQUE(txid, kind, name)
        );
        CREATE INDEX IF NOT EXISTS idx_user_events_txid ON user_events(txid);",
    )
    .context("Failed to create user_events table")?;

    // ── Phase 9: User/group snapshots (pre/post) ────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS user_snapshots (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            txid            INTEGER NOT NULL REFERENCES transactions(txid),
            phase           TEXT NOT NULL,  -- 'pre' or 'post'
            snapshot_json   TEXT NOT NULL,
            captured_at     TEXT NOT NULL,
            UNIQUE(txid, phase)
        );",
    )
    .context("Failed to create user_snapshots table")?;

    // ── Tracked apps table (persistent, cross-session; NOT scoped to one
    // invocation — represents an app being watched across its whole
    // install→usage→uninstall life, independent of any single command) ─────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tracked_apps (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            name            TEXT NOT NULL,                      -- user-given identifier, e.g. \"firefox\"
            kind            TEXT NOT NULL,                       -- 'package' | 'binary'
            package_name    TEXT,                                -- resolved pacman package, if kind='package'
            resolved_paths  TEXT NOT NULL,                       -- JSON array of absolute binary paths
            status          TEXT NOT NULL DEFAULT 'tracking',    -- 'tracking' | 'untracked'
            txid            INTEGER REFERENCES transactions(txid),
            created_at      TEXT NOT NULL,
            untracked_at    TEXT,
            UNIQUE(name)
        );
        CREATE INDEX IF NOT EXISTS idx_tracked_apps_status ON tracked_apps(status);",
    )
    .context("Failed to create tracked_apps table")?;

    log::debug!("Database initialized at {}", db_path);
    Ok(conn)
}

/// Open the pkgundo database in read-write mode (requires appropriate permissions).
pub fn open_db(db_path: &str) -> Result<Connection> {
    Connection::open(db_path).context(format!("Failed to open database at {}", db_path))
}

/// Open the pkgundo database in read-only mode.
/// Used for inspect/timeline/status. Works without root as long as
/// the file has world-read permission (chmod 644).
///
/// Uses immutable=1 URI flag so SQLite skips WAL -shm coordination.
/// Without this, SQLite tries to write a -shm file to the directory
/// even in read-only mode, causing SQLITE_READONLY_DIRECTORY.
pub fn open_db_readonly(db_path: &str) -> Result<Connection> {
    let uri = format!("file:{}?immutable=1", db_path);
    Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context(format!(
        "Cannot open database at {}. Run sudo pkgundo run first.",
        db_path
    ))
}
