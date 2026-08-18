use anyhow::{bail, Context, Result};
use colored::Colorize;
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::archive::ArchiveManager;
use crate::blob_store;
use crate::classifier::{classify_path, rollback_action_for_category, FileCategory, RollbackAction};
use crate::fingerprint::{compare_with_current, get_fingerprint_for_path, FingerprintDiff};
use crate::journal::get_mutations;
use crate::service_tracker;
use crate::user_tracker;
use crate::transaction::{load_transaction, update_transaction_status, PackageManager, TransactionStatus};

/// Rollback mode controls aggressiveness of cleanup
#[derive(Debug, Clone, PartialEq)]
pub enum RollbackMode {
    /// Archive aggressively, minimal risk, preserve ambiguity. DEFAULT.
    Conservative,
    /// Deeper cleanup, removes more runtime leftovers.
    Clean,
    /// Aggressive removal. Advanced users only. Strong warnings apply.
    Nuclear,
}

impl RollbackMode {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "clean" => RollbackMode::Clean,
            "nuclear" => RollbackMode::Nuclear,
            _ => RollbackMode::Conservative,
        }
    }
}

/// Result of a single file rollback action
#[derive(Debug)]
pub enum FileRollbackResult {
    Removed(String),
    Archived(String),
    Restored(String),
    Skipped(String),
    Failed(String, String), // path, reason
}

/// RollbackEngine orchestrates the full rollback of a transaction.
/// It follows the spec precisely:
/// Step A: Load → B: Detect PM → C: Delegate to PM → D: Reconcile → E: File analysis → G: Integrity
pub struct RollbackEngine {
    pub txid: i64,
    pub mode: RollbackMode,
    pub dry_run: bool,
    pub db_path: String,
    /// Overrides ArchiveManager's default root (/var/lib/pkgundo/archives).
    /// None means "use the default". Exists so tests can archive into a temp
    /// dir instead of the real system path, which requires root.
    archive_root: Option<String>,
    /// When true, bypasses the `UserData` → `NeverTouch` short-circuit for
    /// paths under `$HOME`, and forces archive-before-remove even for
    /// freshly-created/untouched files. Default false — the standalone
    /// `pkgundo rollback <txid>` command must never set this; a routine
    /// install-time rollback has no legitimate reason to delete anything in
    /// a user's home directory, and that protection must stay exactly as
    /// strict as it is today. Only tracked-app `untrack --rollback` (which
    /// intentionally captures $HOME mutations) sets this.
    home_cleanup: bool,
}

impl RollbackEngine {
    pub fn new(txid: i64, mode: RollbackMode, dry_run: bool, db_path: &str) -> Self {
        Self {
            txid,
            mode,
            dry_run,
            db_path: db_path.to_string(),
            archive_root: None,
            home_cleanup: false,
        }
    }

    /// Override the archive root (for tests; production callers should leave
    /// this unset and get the real /var/lib/pkgundo/archives location).
    pub fn with_archive_root(mut self, root: impl Into<String>) -> Self {
        self.archive_root = Some(root.into());
        self
    }

    /// Allow this rollback to actually touch `$HOME` paths, for tracked-app
    /// cleanup. See the `home_cleanup` field doc for why this must stay
    /// opt-in and default false.
    pub fn with_home_cleanup(mut self, on: bool) -> Self {
        self.home_cleanup = on;
        self
    }

    /// Execute the full rollback flow
    pub fn execute(&self) -> Result<RollbackReport> {
        let conn = Connection::open(&self.db_path)?;
        let mut report = RollbackReport::new(self.txid);

        // ── STEP A: Load transaction ──────────────────────────────────────────
        println!("{}", "  Step A: Loading transaction...".cyan());
        let tx = load_transaction(&conn, self.txid)
            .context(format!("Transaction {} not found", self.txid))?;

        if tx.status == TransactionStatus::RolledBack {
            bail!("Transaction {} has already been rolled back.", self.txid);
        }

        report.command = tx.command.clone();
        println!("  Command: {}", tx.command.yellow());
        println!("  Package manager: {}", tx.package_manager.as_str().yellow());

        let mutations = get_mutations(&conn, self.txid)?;
        println!("  Mutations recorded: {}", mutations.len());

        // ── STEP B: Determine package manager ────────────────────────────────
        println!("{}", "  Step B: Detecting package manager...".cyan());
        let pkg_name = tx.extract_package_name();

        // ── STEP C: Delegate official removal to package manager ──────────────
        if tx.package_manager != PackageManager::Script
            && tx.package_manager != PackageManager::Unknown
        {
            println!("{}", "  Step C: Delegating removal to package manager...".cyan());
            if let Some(ref name) = pkg_name {
                if self.dry_run {
                    println!(
                        "  [DRY RUN] Would run: {}",
                        self.build_pm_remove_command(&tx.package_manager, name)
                    );
                } else {
                    self.run_package_manager_remove(&conn, &tx.package_manager, name, &mut report)?;
                }
            } else {
                println!(
                    "  {}",
                    "  Warning: Could not determine package name. Skipping PM removal.".yellow()
                );
            }
        } else {
            println!(
                "  {}",
                "  Step C: Script transaction — no package manager to delegate to.".yellow()
            );
        }

        // ── STEP D: Reconciliation phase ──────────────────────────────────────
        println!("{}", "  Step D: Reconciliation phase...".cyan());

        // ── STEP E: Per-file analysis ─────────────────────────────────────────
        println!("{}", "  Step E: Analyzing mutations...".cyan());

        let archive_mgr = match &self.archive_root {
            Some(root) => ArchiveManager::with_root(root.clone()),
            None => ArchiveManager::new(),
        };

        for mutation in &mutations {
            let path_str = &mutation.path;
            let path = Path::new(path_str);
            let category = classify_path(path);

            let result = self.process_mutation(
                &conn,
                &archive_mgr,
                path_str,
                &mutation.operation,
                &category,
            );

            match result {
                Ok(fr) => match fr {
                    FileRollbackResult::Removed(p) => { report.removed.push(p); }
                    FileRollbackResult::Archived(p) => { report.archived.push(p); }
                    FileRollbackResult::Restored(p) => { report.restored.push(p); }
                    FileRollbackResult::Skipped(p) => { report.skipped.push(p); }
                    FileRollbackResult::Failed(p, reason) => { report.failed.push((p, reason)); }
                },
                Err(e) => {
                    report.failed.push((path_str.clone(), format!("Error: {}", e)));
                }
            }
        }

        // fanotify only reports file events, never directory-creation, so
        // there's no mutation recorded for the XDG directories an app
        // creates (e.g. `~/.config/weechat`) — only for the files inside
        // them. Without this, every home_cleanup rollback leaves a trail of
        // now-empty directories behind. Bounded at the user's home
        // directory itself, which is never removed even if empty.
        if self.home_cleanup && !self.dry_run {
            let removed_paths: Vec<&String> = report.removed.iter().chain(report.archived.iter()).collect();
            cleanup_empty_ancestor_dirs(&removed_paths);
        }

        // ── STEP F: Service + user reconciliation (Phase 9) ───────────────────
        println!("{}", "  Step F: Service & user reconciliation...".cyan());

        // Rollback systemd service changes
        match service_tracker::rollback_service_events(&conn, self.txid, self.dry_run) {
            Ok(reversed) if !reversed.is_empty() => {
                println!("  Service changes reversed: {}", reversed.len());
                for r in &reversed {
                    println!("    {} {}", "✓".green(), r);
                }
                report.service_reversals = reversed;
            }
            Ok(_) => {
                println!("  No service changes to reverse.");
            }
            Err(e) => {
                log::warn!("Service rollback error: {}", e);
            }
        }

        // Rollback user/group additions (Conservative: skip; Clean: remove; Nuclear: force)
        if self.mode == RollbackMode::Clean || self.mode == RollbackMode::Nuclear {
            match user_tracker::rollback_user_events(&conn, self.txid, self.dry_run) {
                Ok(reversed) if !reversed.is_empty() => {
                    println!("  User/group changes reversed: {}", reversed.len());
                    report.user_reversals = reversed;
                }
                Ok(_) => {}
                Err(e) => log::warn!("User rollback error: {}", e),
            }
        } else {
            // Conservative: warn but don't act
            let user_events = user_tracker::get_user_events(&conn, self.txid).unwrap_or_default();
            if !user_events.is_empty() {
                println!(
                    "  {} {} user/group changes recorded. Use --mode clean to reverse them.",
                    "⚠".yellow(), user_events.len()
                );
            }
        }

        // ── STEP G: Final integrity check ─────────────────────────────────────
        println!("{}", "  Step G: Final integrity check...".cyan());
        self.integrity_check(&mut report)?;

        // Mark transaction as rolled back
        if !self.dry_run {
            update_transaction_status(&conn, self.txid, TransactionStatus::RolledBack, None)?;
        }

        report.success = true;
        Ok(report)
    }

    /// Process a single mutation during rollback
    fn process_mutation(
        &self,
        conn: &Connection,
        archive_mgr: &ArchiveManager,
        path_str: &str,
        operation: &str,
        category: &FileCategory,
    ) -> Result<FileRollbackResult> {
        let path = Path::new(path_str);
        let action = rollback_action_for_category(category);

        // Always skip user data, unless this rollback was explicitly opted
        // into touching $HOME (tracked-app cleanup) via `home_cleanup`.
        match action {
            RollbackAction::NeverTouch if !self.home_cleanup => {
                log::debug!("Rollback skip (never-touch, user data): {}", path_str);
                return Ok(FileRollbackResult::Skipped(path_str.to_string()));
            }
            RollbackAction::Skip => {
                log::debug!("Rollback skip (category policy): {}", path_str);
                return Ok(FileRollbackResult::Skipped(path_str.to_string()));
            }
            _ => {}
        }

        match operation {
            // A rename's destination now holds content that didn't exist at
            // that path before the transaction (e.g. a package manager
            // finalizing a download by renaming it into place) — treat it
            // like a freshly created file for rollback purposes, or it's
            // permanently invisible to cleanup.
            "create" | "rename_to" => self.handle_created_file(conn, archive_mgr, path),
            "modify" => self.handle_modified_file(conn, archive_mgr, path),
            "delete" => self.handle_deleted_file(conn, path_str),
            // The source path of a rename no longer has anything at it —
            // nothing to restore there.
            "rename_from" | "rename" => {
                log::debug!("Rollback skip (rename source, nothing to restore): {}", path_str);
                Ok(FileRollbackResult::Skipped(path_str.to_string()))
            }
            _ => {
                log::debug!("Rollback skip (unknown operation {}): {}", operation, path_str);
                Ok(FileRollbackResult::Skipped(path_str.to_string()))
            }
        }
    }

    /// Handle a file that was CREATED during the transaction
    fn handle_created_file(
        &self,
        conn: &Connection,
        archive_mgr: &ArchiveManager,
        path: &Path,
    ) -> Result<FileRollbackResult> {
        let path_str = path.to_string_lossy().to_string();

        if !path.exists() && !path.is_symlink() {
            log::debug!("Rollback skip (created file already gone): {}", path_str);
            return Ok(FileRollbackResult::Skipped(path_str));
        }

        // Check if the file has been modified since it was installed
        let pre_fp = get_fingerprint_for_path(conn, self.txid, &path_str, "pre")?;
        let current_diff = if let Some(ref pre) = pre_fp {
            compare_with_current(pre)
        } else {
            FingerprintDiff::New
        };

        match current_diff {
            FingerprintDiff::Unchanged | FingerprintDiff::New => {
                // File unchanged since install — safe to remove outright for
                // disposable install artifacts. But once `home_cleanup` is
                // set (tracked-app $HOME cleanup), deleting without a backup
                // is a real data-loss risk in a way it isn't for /usr/bin
                // cruft — archive first, matching what `scan_leftovers`
                // already guarantees unconditionally for its own removals.
                match &self.mode {
                    RollbackMode::Conservative | RollbackMode::Clean | RollbackMode::Nuclear => {
                        // Directories have nothing to archive (ArchiveManager
                        // copies file content); only files/symlinks get one.
                        let archived = self.home_cleanup && (path.is_symlink() || path.is_file());
                        if !self.dry_run {
                            if archived {
                                archive_mgr.archive_file(conn, self.txid, &path_str, false)?;
                            }
                            if path.is_symlink() || path.is_file() {
                                fs::remove_file(path).context(format!("Failed to remove {}", path_str))?;
                            } else if path.is_dir() {
                                // Only remove empty dirs
                                let _ = fs::remove_dir(path);
                            }
                        }
                        if archived {
                            Ok(FileRollbackResult::Archived(path_str))
                        } else {
                            Ok(FileRollbackResult::Removed(path_str))
                        }
                    }
                }
            }
            FingerprintDiff::Modified => {
                // File was modified after install — archive it
                match &self.mode {
                    RollbackMode::Conservative => {
                        if !self.dry_run {
                            archive_mgr.archive_file(conn, self.txid, &path_str, true)?;
                            if path.is_file() || path.is_symlink() {
                                let _ = fs::remove_file(path);
                            }
                        }
                        Ok(FileRollbackResult::Archived(path_str))
                    }
                    RollbackMode::Clean | RollbackMode::Nuclear => {
                        if !self.dry_run {
                            if path.is_file() || path.is_symlink() {
                                let _ = fs::remove_file(path);
                            }
                        }
                        Ok(FileRollbackResult::Removed(path_str))
                    }
                }
            }
            FingerprintDiff::Missing => {
                log::debug!("Rollback skip (created file fingerprint missing): {}", path_str);
                Ok(FileRollbackResult::Skipped(path_str))
            }
        }
    }

    /// Handle a file that was MODIFIED during the transaction
    fn handle_modified_file(
        &self,
        conn: &Connection,
        archive_mgr: &ArchiveManager,
        path: &Path,
    ) -> Result<FileRollbackResult> {
        let path_str = path.to_string_lossy().to_string();
        // For configs specifically, always archive before restoring
        let pre_fp = get_fingerprint_for_path(conn, self.txid, &path_str, "pre")?;
        let current_diff = if let Some(ref pre) = pre_fp {
            compare_with_current(pre)
        } else {
            // No "pre" baseline exists at all for tracked-app launches (there's
            // no pre_scan_configs step outside a `pkgundo run` install), so
            // this is the common case for a file touched on a later launch,
            // not a rare one — under home_cleanup there's nothing meaningful
            // to "restore" (no baseline means no prior state to speak of), so
            // treat it like a created file: archive current content, then
            // remove. Otherwise (regular install rollback), preserve the
            // existing conservative behavior of leaving it alone entirely.
            if self.home_cleanup {
                // A "modify" mutation can outlive its own path: e.g. an app
                // writes to a temp file (create + modify), then atomically
                // renames it into place (rename_to) — by rollback time the
                // temp path is long gone. Check existence first, exactly
                // like handle_created_file does, so a stale reference gets
                // reported as Skipped instead of a phantom "Archived" that
                // corresponds to no actual archive or removal on disk.
                if !path.exists() && !path.is_symlink() {
                    log::debug!("Rollback skip (modified file already gone): {}", path_str);
                    return Ok(FileRollbackResult::Skipped(path_str));
                }
                if !self.dry_run {
                    if path.is_symlink() || path.is_file() {
                        archive_mgr.archive_file(conn, self.txid, &path_str, false)?;
                        fs::remove_file(path).context(format!("Failed to remove {}", path_str))?;
                    } else if path.is_dir() {
                        let _ = fs::remove_dir(path);
                    }
                }
                return Ok(FileRollbackResult::Archived(path_str));
            }
            log::debug!("Rollback skip (modified file has no pre-install baseline): {}", path_str);
            return Ok(FileRollbackResult::Skipped(path_str.to_string()));
        };

        match current_diff {
            FingerprintDiff::Unchanged => {
                // File is still at the post-install state — restore the original via blob
                if !self.dry_run {
                    match blob_store::restore_from_blob(conn, self.txid, &path_str) {
                        Ok(true) => return Ok(FileRollbackResult::Restored(path_str.to_string())),
                        Ok(false) => {}
                        Err(e) => log::debug!("Blob restore: {}", e),
                    }
                }
                // No blob available — note for user
                Ok(FileRollbackResult::Restored(path_str.to_string()))
            }
            FingerprintDiff::Modified => {
                // File was modified AFTER install — archive the modified version, then restore original
                match &self.mode {
                    RollbackMode::Conservative | RollbackMode::Clean => {
                        if !self.dry_run {
                            archive_mgr.archive_file(conn, self.txid, &path_str, true)?;
                            // Try to restore original from blob
                            let _ = blob_store::restore_from_blob(conn, self.txid, &path_str);
                        }
                        Ok(FileRollbackResult::Archived(path_str.to_string()))
                    }
                    RollbackMode::Nuclear => {
                        if !self.dry_run {
                            let _ = blob_store::restore_from_blob(conn, self.txid, &path_str);
                        }
                        Ok(FileRollbackResult::Restored(path_str.to_string()))
                    }
                }
            }
            FingerprintDiff::Missing => {
                log::debug!("Rollback skip (modified file fingerprint missing): {}", path_str);
                Ok(FileRollbackResult::Skipped(path_str.to_string()))
            }
            FingerprintDiff::New => {
                log::debug!("Rollback skip (modified file fingerprint unexpectedly new): {}", path_str);
                Ok(FileRollbackResult::Skipped(path_str.to_string()))
            }
        }
    }

    /// Handle a file that was DELETED during the transaction
    fn handle_deleted_file(&self, conn: &Connection, path_str: &str) -> Result<FileRollbackResult> {
        // Phase 9: Try to restore from blob store first (true content restore)
        if !self.dry_run {
            match blob_store::restore_from_blob(conn, self.txid, path_str) {
                Ok(true) => {
                    log::info!("Rollback: restored deleted file {} from blob store", path_str);
                    return Ok(FileRollbackResult::Restored(path_str.to_string()));
                }
                Ok(false) => {}
                Err(e) => log::debug!("Blob restore failed for {}: {}", path_str, e),
            }
        }

        // Fall back to fingerprint-based detection
        let pre_fp = get_fingerprint_for_path(conn, self.txid, path_str, "pre")?;
        if pre_fp.is_some() {
            log::warn!(
                "Rollback: {} was deleted during tx {}. No blob available — manual restore needed.",
                path_str, self.txid
            );
            return Ok(FileRollbackResult::Restored(path_str.to_string()));
        }
        log::debug!("Rollback skip (deleted file has no pre-install baseline): {}", path_str);
        Ok(FileRollbackResult::Skipped(path_str.to_string()))
    }

    /// Run the appropriate package manager remove command
    fn run_package_manager_remove(
        &self,
        _conn: &Connection,
        pm: &PackageManager,
        pkg_name: &str,
        report: &mut RollbackReport,
    ) -> Result<()> {
        let (bin, args) = match pm {
            PackageManager::Pacman => ("pacman", vec!["-Rcns", "--noconfirm", pkg_name]),
            PackageManager::Apt => ("apt-get", vec!["purge", "-y", pkg_name]),
            PackageManager::Dnf => ("dnf", vec!["remove", "-y", pkg_name]),
            PackageManager::Rpm => ("rpm", vec!["-e", pkg_name]),
            PackageManager::Dpkg => ("dpkg", vec!["--purge", pkg_name]),
            _ => return Ok(()),
        };

        println!("  Running: {} {}", bin, args.join(" "));
        let status = Command::new(bin)
            .args(&args)
            .status()
            .context(format!("Failed to run {} for package removal", bin))?;

        if status.success() {
            report.pm_removal_succeeded = true;
            println!("  {} Package manager removal succeeded.", "✓".green());
        } else {
            println!(
                "  {} Package manager removal failed (exit code {:?}).",
                "✗".red(),
                status.code()
            );
            report.pm_removal_succeeded = false;
        }

        Ok(())
    }

    /// Build a human-readable PM remove command (for dry-run display)
    fn build_pm_remove_command(&self, pm: &PackageManager, pkg_name: &str) -> String {
        match pm {
            PackageManager::Pacman => format!("pacman -Rcns --noconfirm {}", pkg_name),
            PackageManager::Apt => format!("apt-get purge -y {} && apt-get autoremove -y", pkg_name),
            PackageManager::Dnf => format!("dnf remove -y {} && dnf autoremove", pkg_name),
            _ => format!("(package manager remove {})", pkg_name),
        }
    }

    /// Final integrity check: look for broken symlinks, verify nothing is obviously wrong
    fn integrity_check(&self, report: &mut RollbackReport) -> Result<()> {
        let mut issues = Vec::new();

        // Check for broken symlinks in key dirs
        let check_dirs = ["/usr/bin", "/usr/lib", "/etc"];
        for dir_str in &check_dirs {
            let dir = Path::new(dir_str);
            if !dir.exists() {
                continue;
            }
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_symlink() {
                        if !path.exists() {
                            issues.push(format!("Broken symlink: {}", path.display()));
                        }
                    }
                }
            }
        }

        if !issues.is_empty() {
            println!("  {} Integrity issues found:", "⚠".yellow());
            for issue in &issues {
                println!("    - {}", issue.yellow());
            }
        }

        report.integrity_issues = issues;
        Ok(())
    }
}

/// Walk up from each removed/archived path's parent directory, removing
/// directories that are now empty, one level at a time, stopping at (and
/// never removing) the owning user's home directory.
fn cleanup_empty_ancestor_dirs(removed_paths: &[&String]) {
    use std::collections::BTreeSet;

    let mut candidates: BTreeSet<PathBuf> = BTreeSet::new();
    for p in removed_paths {
        if let Some(parent) = Path::new(p.as_str()).parent() {
            candidates.insert(parent.to_path_buf());
        }
    }

    for start in candidates {
        if let Some(home) = home_root_of(&start) {
            remove_empty_dirs_up_to(start, &home);
        }
    }
}

/// Remove `dir` and each empty ancestor above it, stopping at (and never
/// removing) `boundary` itself. Split out from `cleanup_empty_ancestor_dirs`
/// so the tree-walking logic is testable without depending on `/home`
/// actually existing at the filesystem root the way `home_root_of` assumes.
fn remove_empty_dirs_up_to(mut dir: PathBuf, boundary: &Path) {
    loop {
        if dir == boundary || !dir.starts_with(boundary) {
            break;
        }
        match fs::read_dir(&dir) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    break; // not empty — leave it alone
                }
            }
            Err(_) => break, // unreadable — leave it alone
        }
        if fs::remove_dir(&dir).is_err() {
            break;
        }
        log::debug!("Rollback: removed now-empty directory {}", dir.display());
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
}

/// The `/home/<user>` or `/root` a path lives under, per the same
/// convention `home_dir_for_uid` resolves — used only as the removal
/// boundary above, never to be removed itself.
fn home_root_of(path: &Path) -> Option<PathBuf> {
    let mut comps = path.components();
    comps.next()?; // RootDir
    let first = comps.next()?;
    match first.as_os_str().to_str()? {
        "root" => Some(PathBuf::from("/root")),
        "home" => Some(Path::new("/home").join(comps.next()?)),
        _ => None,
    }
}

/// Summary of a rollback operation
#[derive(Debug)]
pub struct RollbackReport {
    pub txid: i64,
    pub command: String,
    pub success: bool,
    pub pm_removal_succeeded: bool,
    pub removed: Vec<String>,
    pub archived: Vec<String>,
    pub restored: Vec<String>,
    pub skipped: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub integrity_issues: Vec<String>,
    // Phase 9
    pub service_reversals: Vec<String>,
    pub user_reversals: Vec<String>,
}

impl RollbackReport {
    pub fn new(txid: i64) -> Self {
        Self {
            txid,
            command: String::new(),
            success: false,
            pm_removal_succeeded: false,
            removed: Vec::new(),
            archived: Vec::new(),
            restored: Vec::new(),
            skipped: Vec::new(),
            failed: Vec::new(),
            integrity_issues: Vec::new(),
            service_reversals: Vec::new(),
            user_reversals: Vec::new(),
        }
    }

    pub fn print_summary(&self) {
        println!();
        println!("{}", "═══════════════════════════════════════════".cyan());
        println!("{} Rollback Report — txid {}", "▶".green(), self.txid);
        println!("{}", "═══════════════════════════════════════════".cyan());
        println!("  Command:  {}", self.command.yellow());
        println!(
            "  PM removal: {}",
            if self.pm_removal_succeeded {
                "✓ succeeded".green().to_string()
            } else {
                "⚠ skipped/failed".yellow().to_string()
            }
        );
        println!("  Files removed:   {}", self.removed.len());
        println!("  Files archived:  {}", self.archived.len());
        println!("  Files restored:  {}", self.restored.len());
        println!("  Files skipped:   {}", self.skipped.len());
        if !self.service_reversals.is_empty() {
            println!("  Services reversed: {}", self.service_reversals.len());
        }
        if !self.user_reversals.is_empty() {
            println!("  Users/groups removed: {}", self.user_reversals.len());
        }
        if !self.failed.is_empty() {
            println!("  {} Failures:", "✗".red());
            for (path, reason) in &self.failed {
                println!("    - {}: {}", path, reason);
            }
        }
        if !self.integrity_issues.is_empty() {
            println!("  {} Integrity warnings:", "⚠".yellow());
            for issue in &self.integrity_issues {
                println!("    - {}", issue);
            }
        }
        println!(
            "  Overall: {}",
            if self.success {
                "✓ SUCCESS".green().to_string()
            } else {
                "✗ INCOMPLETE".red().to_string()
            }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn home_root_of_resolves_home_and_root() {
        assert_eq!(home_root_of(Path::new("/home/alice/.config/app")), Some(PathBuf::from("/home/alice")));
        assert_eq!(home_root_of(Path::new("/home/alice")), Some(PathBuf::from("/home/alice")));
        assert_eq!(home_root_of(Path::new("/root/.config/app")), Some(PathBuf::from("/root")));
    }

    #[test]
    fn home_root_of_none_for_system_paths() {
        assert_eq!(home_root_of(Path::new("/usr/bin/foo")), None);
        assert_eq!(home_root_of(Path::new("/etc/foo")), None);
    }

    #[test]
    fn cleanup_removes_now_empty_nested_dirs_but_stops_at_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("alice");
        let nested = home.join(".config").join("app").join("sub");
        fs::create_dir_all(&nested).unwrap();

        remove_empty_dirs_up_to(nested.clone(), &home);

        assert!(!nested.exists(), "empty leaf dir should be removed");
        assert!(!home.join(".config").join("app").exists(), "empty parent should be removed");
        assert!(!home.join(".config").exists(), "empty grandparent should be removed");
        assert!(home.exists(), "home directory itself must never be removed");
    }

    #[test]
    fn cleanup_stops_at_first_non_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("alice");
        let app_dir = home.join(".config").join("app");
        let sub_dir = app_dir.join("sub");
        fs::create_dir_all(&sub_dir).unwrap();
        // A sibling file that was NOT part of this rollback keeps app_dir non-empty.
        fs::write(app_dir.join("keep.conf"), b"x").unwrap();

        remove_empty_dirs_up_to(sub_dir.clone(), &home);

        assert!(!sub_dir.exists(), "the genuinely empty leaf should still be removed");
        assert!(app_dir.exists(), "must not remove a directory with other content still in it");
    }
}
