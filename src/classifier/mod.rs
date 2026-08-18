use std::path::Path;

/// Semantic category for a file — drives rollback decisions
#[derive(Debug, Clone, PartialEq)]
pub enum FileCategory {
    /// Executable binaries: /usr/bin, /usr/sbin, /bin, /sbin
    Binary,
    /// Config files: /etc — archive carefully on rollback
    Config,
    /// Package caches: /var/cache — safe to delete
    Cache,
    /// Runtime state: /run, /var/run
    RuntimeState,
    /// Temp files: /tmp, /var/tmp — safe to delete
    TempFile,
    /// Log files: /var/log — skip on rollback
    Log,
    /// Symbolic links (cross-category)
    Symlink,
    /// User home directories — NEVER TOUCH
    UserData,
    /// Shared libraries: /usr/lib, /lib, /lib64
    Library,
    /// Desktop/icon/MIME/font caches: generated, safe to delete
    SystemCache,
    /// Package manager databases: /var/lib/pacman, /var/lib/dpkg etc.
    PackageDb,
    /// Service units: /usr/lib/systemd, /etc/systemd
    ServiceUnit,
    /// Unknown/unclassified
    Unknown,
}

/// Classify a path into a FileCategory based on path prefix rules.
/// This is the semantic classification layer that drives rollback decisions.
pub fn classify_path(path: &Path) -> FileCategory {
    let s = path.to_string_lossy();

    // NEVER TOUCH: user home directories
    if s.starts_with("/home/") || s == "/home" {
        return FileCategory::UserData;
    }
    if s.starts_with("/root/") || s == "/root" {
        return FileCategory::UserData;
    }

    // Package manager databases — treat with extreme care
    if s.starts_with("/var/lib/pacman")
        || s.starts_with("/var/lib/dpkg")
        || s.starts_with("/var/lib/rpm")
        || s.starts_with("/var/lib/dnf")
    {
        return FileCategory::PackageDb;
    }

    // Generated system caches — safe to delete
    if s.starts_with("/var/cache/")
        || s.contains("/icons/hicolor/icon-theme.cache")
        || s.contains("font")
        || s.starts_with("/usr/share/info/dir")
        || s.contains("mime")
        || s.ends_with(".cache")
    {
        return FileCategory::Cache;
    }

    if s.starts_with("/var/cache") {
        return FileCategory::Cache;
    }

    // Temp files — safe to remove
    if s.starts_with("/tmp/") || s.starts_with("/var/tmp/") {
        return FileCategory::TempFile;
    }

    // Runtime state — ephemeral, not worth restoring
    if s.starts_with("/run/") || s.starts_with("/var/run/") {
        return FileCategory::RuntimeState;
    }

    // Logs — skip on rollback
    if s.starts_with("/var/log/") {
        return FileCategory::Log;
    }

    // Service units
    if s.starts_with("/usr/lib/systemd/") || s.starts_with("/etc/systemd/") {
        return FileCategory::ServiceUnit;
    }

    // Config files — archive carefully
    if s.starts_with("/etc/") {
        return FileCategory::Config;
    }

    // Shared libraries
    if s.starts_with("/usr/lib/")
        || s.starts_with("/lib/")
        || s.starts_with("/lib64/")
        || s.starts_with("/usr/lib64/")
    {
        // Could be a library or a cache within lib
        if s.contains(".cache") || s.contains("/locale/") {
            return FileCategory::SystemCache;
        }
        return FileCategory::Library;
    }

    // Binaries
    if s.starts_with("/usr/bin/")
        || s.starts_with("/usr/sbin/")
        || s.starts_with("/bin/")
        || s.starts_with("/sbin/")
    {
        return FileCategory::Binary;
    }

    // Share directory — can include various things
    if s.starts_with("/usr/share/") {
        if s.contains("icons")
            || s.contains("pixmaps")
            || s.contains("applications")
            || s.contains("mime")
            || s.contains("fonts")
        {
            return FileCategory::SystemCache;
        }
        return FileCategory::Binary; // treat as package-owned content
    }

    // Check if it's a symlink on disk right now
    if path.is_symlink() {
        return FileCategory::Symlink;
    }

    FileCategory::Unknown
}

/// Determine the rollback action for a file in this category.
/// Returns a human-readable action description and whether it's safe to auto-act.
pub enum RollbackAction {
    /// Remove the file if unchanged since install
    RemoveSafe,
    /// Archive and restore original
    ArchiveAndRestore,
    /// Just remove (generated cache)
    RemoveCache,
    /// Skip — don't touch
    Skip,
    /// Ask the user before doing anything
    AskUser,
    /// Never touch — absolute safety rule
    NeverTouch,
}

pub fn rollback_action_for_category(cat: &FileCategory) -> RollbackAction {
    match cat {
        FileCategory::UserData => RollbackAction::NeverTouch,
        FileCategory::PackageDb => RollbackAction::Skip, // Package manager handles this
        FileCategory::Cache | FileCategory::SystemCache | FileCategory::TempFile => {
            RollbackAction::RemoveCache
        }
        FileCategory::RuntimeState | FileCategory::Log => RollbackAction::Skip,
        FileCategory::Config => RollbackAction::ArchiveAndRestore,
        FileCategory::Binary | FileCategory::Library => RollbackAction::RemoveSafe,
        FileCategory::ServiceUnit => RollbackAction::ArchiveAndRestore,
        FileCategory::Symlink => RollbackAction::RemoveSafe,
        FileCategory::Unknown => RollbackAction::AskUser,
    }
}

/// Human-readable description of a FileCategory
impl std::fmt::Display for FileCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            FileCategory::Binary => "Binary",
            FileCategory::Config => "Config",
            FileCategory::Cache => "Cache",
            FileCategory::RuntimeState => "RuntimeState",
            FileCategory::TempFile => "TempFile",
            FileCategory::Log => "Log",
            FileCategory::Symlink => "Symlink",
            FileCategory::UserData => "UserData",
            FileCategory::Library => "Library",
            FileCategory::SystemCache => "SystemCache",
            FileCategory::PackageDb => "PackageDb",
            FileCategory::ServiceUnit => "ServiceUnit",
            FileCategory::Unknown => "Unknown",
        };
        write!(f, "{}", name)
    }
}
