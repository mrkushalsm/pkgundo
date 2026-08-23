//! Tracked apps: a persistent, cross-session record of an app being watched
//! across its whole install→usage→uninstall life, independent of any single
//! `pkgundo run` invocation. Owned exclusively by the daemon (`src/daemon`);
//! the CLI never touches this module or the DB directly for writes.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use crate::transaction::{self, TransactionStatus};

/// Where a tracked app's watched binaries came from.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedTarget {
    Package { package_name: String, binaries: Vec<String> },
    Binary { path: String },
}

/// A persistent tracked-app record, as stored in `tracked_apps`.
#[derive(Debug, Clone)]
pub struct TrackedApp {
    pub id: i64,
    pub name: String,
    pub kind: String, // "package" | "binary"
    pub package_name: Option<String>,
    pub resolved_paths: Vec<String>,
    pub status: String, // "tracking" | "untracked"
    pub txid: i64,
    pub created_at: String,
    pub untracked_at: Option<String>,
}

/// Directories pacman's and dpkg's file listings put real binaries under.
/// `/usr/lib/` is included because many apps (Firefox included) ship their
/// real binary there, with only a wrapper/symlink under `/usr/bin`.
/// `/usr/games/` is Debian/Ubuntu's standard location for game-ish packages
/// (cowsay, sl, cmatrix, fortune-mod, figlet, ...) — a real dpkg-only
/// convention, not present on Arch.
const BIN_DIRS: &[&str] =
    &["/usr/bin/", "/usr/sbin/", "/usr/local/bin/", "/bin/", "/sbin/", "/usr/lib/", "/usr/games/"];

fn is_executable_candidate(path: &str) -> bool {
    if path.ends_with('/') || !BIN_DIRS.iter().any(|d| path.starts_with(d)) {
        return false;
    }
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Parse a `pacman -Ql <pkg>` style listing (`<pkg> <path>` per line) into
/// the subset of paths that look like real executable binaries. Shared with
/// `scan_leftovers`, which needs the same filtering for both live `pacman
/// -Ql` output and a cached archive's extracted file list.
pub fn executable_binaries_from_listing(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| line.split_once(' ').map(|(_, p)| p.trim().to_string()))
        .filter(|p| is_executable_candidate(p))
        .collect()
}

/// Parse a `dpkg -L <pkg>` listing (one absolute path per line, no leading
/// package-name token — unlike pacman's `-Ql`) into the executable subset.
/// A new sibling to `executable_binaries_from_listing`, not a change to it:
/// that function's two-column parsing is also relied on by `scan_leftovers`
/// for pacman's own listing format and must keep working unchanged.
pub fn executable_binaries_from_dpkg_listing(listing: &str) -> Vec<String> {
    listing.lines().map(str::trim).filter(|p| is_executable_candidate(p)).map(String::from).collect()
}

/// Resolve `app` to either a package's owned binaries (pacman first, then
/// dpkg), or a literal binary path/name. Falls back to treating `app` as a
/// binary if neither package manager resolves it.
pub fn resolve_app_targets(app: &str) -> Result<ResolvedTarget> {
    if Command::new("which").arg("pacman").output().map(|o| o.status.success()).unwrap_or(false) {
        if let Ok(output) = Command::new("pacman").args(["-Ql", app]).output() {
            if output.status.success() {
                let listing = String::from_utf8_lossy(&output.stdout);
                let binaries = executable_binaries_from_listing(&listing);
                if !binaries.is_empty() {
                    return Ok(ResolvedTarget::Package { package_name: app.to_string(), binaries });
                }
            }
        }
    }

    if Command::new("which").arg("dpkg").output().map(|o| o.status.success()).unwrap_or(false) {
        if let Ok(output) = Command::new("dpkg").args(["-L", app]).output() {
            if output.status.success() {
                let listing = String::from_utf8_lossy(&output.stdout);
                let binaries = executable_binaries_from_dpkg_listing(&listing);
                if !binaries.is_empty() {
                    return Ok(ResolvedTarget::Package { package_name: app.to_string(), binaries });
                }
            }
        }
    }

    if Command::new("which").arg("rpm").output().map(|o| o.status.success()).unwrap_or(false) {
        if let Ok(output) = Command::new("rpm").args(["-ql", app]).output() {
            if output.status.success() {
                let listing = String::from_utf8_lossy(&output.stdout);
                // rpm -ql's output shape (one absolute path per line, no
                // leading package-name token) is identical to dpkg -L's, so
                // the same listing parser applies unchanged.
                let binaries = executable_binaries_from_dpkg_listing(&listing);
                if !binaries.is_empty() {
                    return Ok(ResolvedTarget::Package { package_name: app.to_string(), binaries });
                }
            }
        }
    }

    // Fall back: literal path, or a PATH-resolved binary name.
    let path = if app.starts_with('/') {
        app.to_string()
    } else if let Ok(output) = Command::new("which").arg(app).output() {
        if output.status.success() {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else {
            app.to_string()
        }
    } else {
        app.to_string()
    };

    if !Path::new(&path).exists() {
        bail!(
            "'{}' is not a known pacman/dpkg/rpm package and no binary was found at or named '{}'",
            app, path
        );
    }
    Ok(ResolvedTarget::Binary { path })
}

fn resolved_paths_json(target: &ResolvedTarget) -> String {
    let paths: Vec<&str> = match target {
        ResolvedTarget::Package { binaries, .. } => binaries.iter().map(|s| s.as_str()).collect(),
        ResolvedTarget::Binary { path } => vec![path.as_str()],
    };
    serde_json::to_string(&paths).unwrap_or_else(|_| "[]".to_string())
}

/// Start (or resume) tracking `name`. Resolves the app's targets, creates a
/// fresh accumulation transaction, and either inserts a new `tracked_apps`
/// row or revives a previously-untracked one in place (never reuses an old
/// txid on revival — see the plan's "re-tracking" note).
pub fn track_app(conn: &Connection, name: &str) -> Result<TrackedApp> {
    let target = resolve_app_targets(name)?;
    let (kind, package_name) = match &target {
        ResolvedTarget::Package { package_name, .. } => ("package", Some(package_name.clone())),
        ResolvedTarget::Binary { .. } => ("binary", None),
    };
    let resolved_paths = resolved_paths_json(&target);

    let txid = transaction::create_transaction(conn, &format!("[tracked] {}", name), &[])
        .context("Failed to create accumulation transaction for tracked app")?;
    conn.execute(
        "UPDATE transactions SET status = ?1 WHERE txid = ?2",
        rusqlite::params![TransactionStatus::Tracking.as_str(), txid],
    )?;

    let now = Utc::now().to_rfc3339();
    let existing: Option<i64> = conn
        .query_row("SELECT id FROM tracked_apps WHERE name = ?1", [name], |r| r.get(0))
        .optional()?;

    if existing.is_some() {
        conn.execute(
            "UPDATE tracked_apps SET kind = ?1, package_name = ?2, resolved_paths = ?3,
             status = 'tracking', txid = ?4, untracked_at = NULL WHERE name = ?5",
            rusqlite::params![kind, package_name, resolved_paths, txid, name],
        )?;
    } else {
        conn.execute(
            "INSERT INTO tracked_apps (name, kind, package_name, resolved_paths, status, txid, created_at)
             VALUES (?1, ?2, ?3, ?4, 'tracking', ?5, ?6)",
            rusqlite::params![name, kind, package_name, resolved_paths, txid, now],
        )?;
    }

    load_tracked_app(conn, name)?.context("tracked app vanished immediately after insert")
}

/// Stop tracking `name`: closes its accumulation transaction and marks the
/// row untracked. Does not touch any mutations already recorded against it.
pub fn untrack_app(conn: &Connection, name: &str) -> Result<()> {
    let app = load_tracked_app(conn, name)?
        .with_context(|| format!("'{}' is not currently tracked", name))?;
    if app.status != "tracking" {
        bail!("'{}' is not currently tracked", name);
    }

    transaction::update_transaction_status(conn, app.txid, TransactionStatus::Untracked, None)?;
    conn.execute(
        "UPDATE tracked_apps SET status = 'untracked', untracked_at = ?1 WHERE name = ?2",
        rusqlite::params![Utc::now().to_rfc3339(), name],
    )?;
    Ok(())
}

fn row_to_tracked_app(row: &rusqlite::Row) -> rusqlite::Result<TrackedApp> {
    let resolved_paths_str: String = row.get(4)?;
    let resolved_paths: Vec<String> = serde_json::from_str(&resolved_paths_str).unwrap_or_default();
    Ok(TrackedApp {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        package_name: row.get(3)?,
        resolved_paths,
        status: row.get(5)?,
        txid: row.get(6)?,
        created_at: row.get(7)?,
        untracked_at: row.get(8)?,
    })
}

const SELECT_COLS: &str =
    "id, name, kind, package_name, resolved_paths, status, txid, created_at, untracked_at";

pub fn load_tracked_app(conn: &Connection, name: &str) -> Result<Option<TrackedApp>> {
    conn.query_row(
        &format!("SELECT {} FROM tracked_apps WHERE name = ?1", SELECT_COLS),
        [name],
        row_to_tracked_app,
    )
    .optional()
    .context("Failed to load tracked app")
}

/// Look up a currently-tracked app by its pacman package name, as opposed
/// to `load_tracked_app`'s lookup by the name it was tracked under (the two
/// aren't guaranteed to match, and only `kind='package'` rows have
/// `package_name` set at all). Used by the pacman removal hook, which only
/// ever has a package name from stdin, never the tracked-app's own name.
pub fn load_tracked_app_by_package(conn: &Connection, package_name: &str) -> Result<Option<TrackedApp>> {
    conn.query_row(
        &format!(
            "SELECT {} FROM tracked_apps WHERE package_name = ?1 AND status = 'tracking'",
            SELECT_COLS
        ),
        [package_name],
        row_to_tracked_app,
    )
    .optional()
    .context("Failed to load tracked app by package name")
}

/// List tracked apps. `include_all` also returns previously-untracked (historical) rows.
pub fn list_tracked_apps(conn: &Connection, include_all: bool) -> Result<Vec<TrackedApp>> {
    let sql = if include_all {
        format!("SELECT {} FROM tracked_apps ORDER BY created_at DESC", SELECT_COLS)
    } else {
        format!(
            "SELECT {} FROM tracked_apps WHERE status = 'tracking' ORDER BY created_at DESC",
            SELECT_COLS
        )
    };
    let mut stmt = conn.prepare(&sql)?;
    let apps = stmt
        .query_map([], row_to_tracked_app)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("Failed to list tracked apps")?;
    Ok(apps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpkg_listing_keeps_only_executables_under_bin_dirs() {
        // /usr/bin/env is virtually guaranteed present+executable on any
        // Linux box (including this test sandbox); /etc/hostname is a real,
        // existing, non-executable file outside BIN_DIRS — both are stable
        // fixtures for asserting the filter without needing tempfiles under
        // /usr/bin, which a test can't create.
        let listing = "/usr/bin/env\n/etc/hostname\n/usr/share/doc/foo/README\n";
        let binaries = executable_binaries_from_dpkg_listing(listing);
        assert_eq!(binaries, vec!["/usr/bin/env".to_string()]);
    }

    #[test]
    fn dpkg_listing_trims_whitespace_and_ignores_directory_entries() {
        // dpkg -L lists owned directories too, one per line with a
        // trailing slash — `is_executable_candidate` already rejects those.
        let listing = "  /usr/bin/env  \n/usr/bin/\n";
        let binaries = executable_binaries_from_dpkg_listing(listing);
        assert_eq!(binaries, vec!["/usr/bin/env".to_string()]);
    }

    #[test]
    fn dpkg_listing_of_empty_input_is_empty() {
        assert!(executable_binaries_from_dpkg_listing("").is_empty());
    }
}
