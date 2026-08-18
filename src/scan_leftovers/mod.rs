//! Heuristic leftover scanner: fuzzy-matches an app's likely `$HOME` footprint
//! (config/cache/data dirs) against signals derived dynamically from package
//! metadata. No hardcoded per-app table — see the tracked-apps daemon for the
//! precise, non-heuristic counterpart used for apps tracked from install time.

use anyhow::Result;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Directories under which fuzzy `$HOME` matches are never surfaced at all,
/// regardless of confidence tier. A safety floor, not a matching heuristic.
const NEVER_TOUCH: &[&str] = &[".ssh", ".gnupg", ".password-store"];

/// XDG base dirs searched for candidates, relative to `$HOME`.
const XDG_DIRS: &[&str] = &[".config", ".local/share", ".cache", ".local/state"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Guess,
    Likely,
    Exact,
}

impl Confidence {
    pub fn label(&self) -> &'static str {
        match self {
            Confidence::Exact => "exact",
            Confidence::Likely => "likely",
            Confidence::Guess => "guess",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LeftoverCandidate {
    pub path: PathBuf,
    pub confidence: Confidence,
    pub reason: String,
}

/// Signals derived about an app, used to match candidate directories.
#[derive(Debug, Default)]
struct Signals {
    /// Structural tokens: desktop/AppStream IDs, vendor token derived from
    /// the package's URL. A candidate matching one of these exactly is
    /// trusted even outside XDG dirs (e.g. `~/.mozilla` for firefox).
    structural: HashSet<String>,
    /// Package name + resolved binary basenames. Substring match only.
    likely: HashSet<String>,
    /// The raw string the user typed, always present. Weakest signal.
    raw: String,
}

/// Package metadata lookup, abstracted so tests can supply canned output
/// instead of shelling out to a real `pacman`/cache dir.
pub trait PackageMetadataSource {
    /// `pacman -Qi <app>` output, if the package is currently installed.
    fn query_info(&self, app: &str) -> Option<String>;
    /// `pacman -Ql <app>` output, if the package is currently installed.
    fn query_files(&self, app: &str) -> Option<String>;
    /// Path to a cached package archive for `app` under
    /// `/var/cache/pacman/pkg/`, if one exists (most recent mtime wins).
    fn cached_archive(&self, app: &str) -> Option<PathBuf>;
    /// `.PKGINFO` contents extracted from a cached archive.
    fn archive_pkginfo(&self, archive: &Path) -> Option<String>;
    /// File list extracted from a cached archive (one path per line).
    fn archive_file_list(&self, archive: &Path) -> Option<String>;
    /// Read a file's contents (used for `.desktop`/`.metainfo.xml` files
    /// referenced by a package's file list). Real impl just reads from disk;
    /// tests can supply canned content instead.
    fn read_file(&self, path: &str) -> Option<String>;
}

pub struct PacmanMetadataSource;

impl PackageMetadataSource for PacmanMetadataSource {
    fn query_info(&self, app: &str) -> Option<String> {
        let out = Command::new("pacman").args(["-Qi", app]).output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            None
        }
    }

    fn query_files(&self, app: &str) -> Option<String> {
        let out = Command::new("pacman").args(["-Ql", app]).output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            None
        }
    }

    fn cached_archive(&self, app: &str) -> Option<PathBuf> {
        let cache_dir = Path::new("/var/cache/pacman/pkg");
        let entries = fs::read_dir(cache_dir).ok()?;
        let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
        for entry in entries.flatten() {
            let path = entry.path();
            let fname = path.file_name()?.to_str()?;
            if parse_pkg_archive_name(fname).as_deref() != Some(app) {
                continue;
            }
            let mtime = entry.metadata().and_then(|m| m.modified()).ok()?;
            if best.as_ref().map(|(_, t)| mtime > *t).unwrap_or(true) {
                best = Some((path, mtime));
            }
        }
        best.map(|(p, _)| p)
    }

    fn archive_pkginfo(&self, archive: &Path) -> Option<String> {
        let out = Command::new("bsdtar")
            .args(["-xOf", archive.to_str()?, ".PKGINFO"])
            .output()
            .ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            None
        }
    }

    fn archive_file_list(&self, archive: &Path) -> Option<String> {
        let out = Command::new("bsdtar").args(["-tf", archive.to_str()?]).output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            None
        }
    }

    fn read_file(&self, path: &str) -> Option<String> {
        fs::read_to_string(path).ok()
    }
}

/// Parse a pacman cache filename (`<name>-<version>-<pkgrel>-<arch>.pkg.tar.<ext>`)
/// from the right, per the actual naming convention, and return just `<name>`.
/// A plain prefix glob would wrongly match unrelated packages sharing a prefix.
fn parse_pkg_archive_name(fname: &str) -> Option<String> {
    let no_ext = fname
        .strip_suffix(".pkg.tar.zst")
        .or_else(|| fname.strip_suffix(".pkg.tar.xz"))
        .or_else(|| fname.strip_suffix(".pkg.tar.gz"))
        .or_else(|| fname.strip_suffix(".pkg.tar.lz4"))
        .or_else(|| fname.strip_suffix(".pkg.tar"))?;
    let mut parts: Vec<&str> = no_ext.split('-').collect();
    if parts.len() < 4 {
        return None;
    }
    parts.pop(); // arch
    parts.pop(); // pkgrel
    parts.pop(); // version
    Some(parts.join("-"))
}

/// Extract a field like `Name` or `URL` from `pacman -Qi`/`.PKGINFO`-style output.
/// Handles both `Name            : firefox` (Qi) and `pkgname = firefox` (.PKGINFO).
fn extract_field<'a>(text: &'a str, qi_key: &str, pkginfo_key: &str) -> Option<&'a str> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(qi_key) {
            if let Some(v) = rest.trim_start().strip_prefix(':') {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        if let Some(rest) = line.strip_prefix(pkginfo_key) {
            if let Some(v) = rest.trim_start().strip_prefix('=') {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Derive a vendor/organization token from a package's URL, e.g.
/// `https://www.mozilla.org/firefox/` -> `mozilla`. This is what resolves
/// naming mismatches like Firefox -> `.mozilla` without hardcoding it.
fn vendor_token_from_url(url: &str) -> Option<String> {
    let stripped = url.trim().trim_start_matches("https://").trim_start_matches("http://");
    let host = stripped.split('/').next()?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    let labels: Vec<&str> = host.split('.').collect();
    // For a normal `label.tld` or `label.co.uk`-shaped host, the organization
    // token is the first label.
    labels.first().map(|s| s.to_lowercase())
}

/// `file_list` is expected to already be bare absolute paths, one per line
/// (no leading `<pkg> ` prefix as `pacman -Ql` uses) — call sites normalize.
fn collect_desktop_and_appstream_tokens(
    source: &dyn PackageMetadataSource,
    file_list: &str,
    tokens: &mut HashSet<String>,
    binaries: &mut HashSet<String>,
) {
    for line in file_list.lines() {
        let path = line.trim();
        if path.ends_with('/') || path.is_empty() {
            continue;
        }
        if path.ends_with(".desktop") {
            if let Some(basename) = Path::new(path).file_stem().and_then(|s| s.to_str()) {
                tokens.insert(basename.to_lowercase());
            }
            if let Some(content) = source.read_file(path) {
                for line in content.lines() {
                    if let Some(v) = line.strip_prefix("StartupWMClass=") {
                        tokens.insert(v.trim().to_lowercase());
                    }
                    if let Some(v) = line.strip_prefix("Exec=") {
                        if let Some(bin) = v.split_whitespace().next() {
                            if let Some(b) = Path::new(bin).file_name().and_then(|s| s.to_str()) {
                                binaries.insert(b.to_lowercase());
                            }
                        }
                    }
                }
            }
        } else if path.ends_with(".metainfo.xml") || path.ends_with(".appdata.xml") {
            if let Some(content) = source.read_file(path) {
                if let Some(start) = content.find("<id>") {
                    if let Some(end) = content[start..].find("</id>") {
                        let id = &content[start + 4..start + end];
                        // AppStream IDs are often reverse-DNS (org.mozilla.firefox);
                        // take the last dotted segment as the meaningful token.
                        if let Some(last) = id.trim().split('.').next_back() {
                            tokens.insert(last.to_lowercase());
                        }
                    }
                }
            }
        }
    }
}

fn derive_signals(app: &str, source: &dyn PackageMetadataSource) -> Signals {
    let mut signals = Signals {
        raw: app.to_lowercase(),
        ..Default::default()
    };

    // Stage 1: still installed. `pacman -Ql` lines are `<pkg> <path>`.
    // Only treat the app name itself as a "likely" (structural) signal once
    // metadata actually confirms it names a real package — otherwise it's
    // indistinguishable from the raw/guess-tier fallback below.
    if let Some(info) = source.query_info(app) {
        signals.likely.insert(app.to_lowercase());
        apply_metadata(&mut signals, &info);
        if let Some(files) = source.query_files(app) {
            let binaries = crate::tracked_apps::executable_binaries_from_listing(&files);
            for b in &binaries {
                if let Some(name) = Path::new(b).file_name().and_then(|s| s.to_str()) {
                    signals.likely.insert(name.to_lowercase());
                }
            }
            let bare_paths: Vec<&str> = files
                .lines()
                .filter_map(|line| line.split_once(' ').map(|(_, p)| p.trim()))
                .collect();
            let mut extra_bins = HashSet::new();
            collect_desktop_and_appstream_tokens(
                source,
                &bare_paths.join("\n"),
                &mut signals.structural,
                &mut extra_bins,
            );
            signals.likely.extend(extra_bins);
        }
        return signals;
    }

    // Stage 2: not installed, but a cached archive may still exist.
    // `bsdtar -tf` lines are already bare (relative) paths, no `<pkg> ` prefix.
    if let Some(archive) = source.cached_archive(app) {
        signals.likely.insert(app.to_lowercase());
        if let Some(pkginfo) = source.archive_pkginfo(&archive) {
            apply_metadata(&mut signals, &pkginfo);
        }
        if let Some(files) = source.archive_file_list(&archive) {
            let abs_paths: Vec<String> = files
                .lines()
                .map(|l| format!("/{}", l.trim_start_matches("./").trim_start_matches('/')))
                .collect();
            let listing_for_binaries = abs_paths.iter().map(|p| format!("{} {}", app, p)).collect::<Vec<_>>().join("\n");
            let binaries = crate::tracked_apps::executable_binaries_from_listing(&listing_for_binaries);
            for b in &binaries {
                if let Some(name) = Path::new(b).file_name().and_then(|s| s.to_str()) {
                    signals.likely.insert(name.to_lowercase());
                }
            }
            let mut extra_bins = HashSet::new();
            collect_desktop_and_appstream_tokens(
                source,
                &abs_paths.join("\n"),
                &mut signals.structural,
                &mut extra_bins,
            );
            signals.likely.extend(extra_bins);
        }
        return signals;
    }

    // Stage 3: nothing available — raw string only, weakest signal.
    signals
}

fn apply_metadata(signals: &mut Signals, text: &str) {
    if let Some(name) = extract_field(text, "Name", "pkgname") {
        signals.likely.insert(name.to_lowercase());
    }
    let url = extract_field(text, "URL", "url");
    let desc = extract_field(text, "Description", "pkgdesc");
    for candidate_url in [url, desc.filter(|d| d.contains("://"))].into_iter().flatten() {
        if let Some(token) = vendor_token_from_url(candidate_url) {
            signals.structural.insert(token);
        }
    }
}

fn is_never_touch(name: &str) -> bool {
    NEVER_TOUCH.iter().any(|n| n.eq_ignore_ascii_case(name))
}

fn classify(name: &str, signals: &Signals, in_xdg: bool) -> Option<Confidence> {
    let stripped = name.trim_start_matches('.').to_lowercase();
    let lname = name.to_lowercase();

    if signals.structural.contains(&stripped) || signals.structural.contains(&lname) {
        return Some(Confidence::Exact);
    }
    if signals
        .likely
        .iter()
        .any(|t| !t.is_empty() && (lname.contains(t.as_str()) || t.contains(lname.as_str())))
    {
        return Some(Confidence::Likely);
    }
    if in_xdg && !signals.raw.is_empty() && lname.contains(&signals.raw) {
        return Some(Confidence::Guess);
    }
    None
}

fn scan_dir_children(dir: &Path, signals: &Signals, in_xdg: bool, out: &mut Vec<LeftoverCandidate>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        if is_never_touch(name) {
            continue;
        }
        if let Some(confidence) = classify(name, signals, in_xdg) {
            out.push(LeftoverCandidate {
                path: entry.path(),
                confidence,
                reason: format!("[{}] matched under {}", confidence.label(), dir.display()),
            });
        }
    }
}

/// Scan for `app`'s likely leftover files under `home` (or `$HOME` if `None`),
/// using `source` for package metadata. Injectable home root and metadata
/// source exist purely for testability — see `RollbackEngine::with_archive_root`
/// for the established pattern this mirrors.
pub fn scan_with_home(
    app: &str,
    home_override: Option<&Path>,
    source: &dyn PackageMetadataSource,
) -> Result<Vec<LeftoverCandidate>> {
    let home = match home_override {
        Some(h) => h.to_path_buf(),
        None => dirs_home()?,
    };
    let signals = derive_signals(app, source);

    let mut candidates = Vec::new();
    for xdg in XDG_DIRS {
        scan_dir_children(&home.join(xdg), &signals, true, &mut candidates);
    }
    scan_dir_children(&home, &signals, false, &mut candidates);
    // Top-level $HOME scan above only yields dotdirs since classify() needs a
    // real name match; filter out non-dotdirs it might otherwise pick up.
    candidates.retain(|c| {
        let in_home_top = c.path.parent() == Some(home.as_path());
        !in_home_top || c.path.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with('.')).unwrap_or(false)
    });

    Ok(candidates)
}

pub fn scan_leftovers(app: &str) -> Result<Vec<LeftoverCandidate>> {
    scan_with_home(app, None, &PacmanMetadataSource)
}

fn dirs_home() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("HOME environment variable is not set"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeSource {
        info: HashMap<String, String>,
        files: HashMap<String, String>,
        file_contents: HashMap<String, String>,
    }

    impl PackageMetadataSource for FakeSource {
        fn query_info(&self, app: &str) -> Option<String> {
            self.info.get(app).cloned()
        }
        fn query_files(&self, app: &str) -> Option<String> {
            self.files.get(app).cloned()
        }
        fn cached_archive(&self, _app: &str) -> Option<PathBuf> {
            None
        }
        fn archive_pkginfo(&self, _archive: &Path) -> Option<String> {
            None
        }
        fn archive_file_list(&self, _archive: &Path) -> Option<String> {
            None
        }
        fn read_file(&self, path: &str) -> Option<String> {
            self.file_contents.get(path).cloned()
        }
    }

    fn firefox_source() -> FakeSource {
        let mut info = HashMap::new();
        info.insert(
            "firefox".to_string(),
            "Name            : firefox\nURL             : https://www.mozilla.org/firefox/\n".to_string(),
        );
        let mut files = HashMap::new();
        files.insert(
            "firefox".to_string(),
            "firefox /usr/lib/firefox/firefox\nfirefox /usr/bin/firefox\nfirefox /usr/share/applications/firefox.desktop\n"
                .to_string(),
        );
        FakeSource { info, files, file_contents: HashMap::new() }
    }

    #[test]
    fn exact_match_via_vendor_token() {
        let source = firefox_source();
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".mozilla")).unwrap();

        let candidates = scan_with_home("firefox", Some(tmp.path()), &source).unwrap();
        assert!(candidates.iter().any(|c| c.path.ends_with(".mozilla") && c.confidence == Confidence::Exact));
    }

    #[test]
    fn cached_fallback_used_when_not_installed() {
        struct CachedOnly;
        impl PackageMetadataSource for CachedOnly {
            fn query_info(&self, _app: &str) -> Option<String> {
                None
            }
            fn query_files(&self, _app: &str) -> Option<String> {
                None
            }
            fn cached_archive(&self, app: &str) -> Option<PathBuf> {
                Some(PathBuf::from(format!("/fake/{}-1.0-1-x86_64.pkg.tar.zst", app)))
            }
            fn archive_pkginfo(&self, _archive: &Path) -> Option<String> {
                Some("pkgname = firefox\nurl = https://www.mozilla.org/firefox/\n".to_string())
            }
            fn archive_file_list(&self, _archive: &Path) -> Option<String> {
                Some("usr/lib/firefox/firefox\nusr/bin/firefox\n".to_string())
            }
            fn read_file(&self, _path: &str) -> Option<String> {
                None
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".mozilla")).unwrap();
        let candidates = scan_with_home("firefox", Some(tmp.path()), &CachedOnly).unwrap();
        assert!(candidates.iter().any(|c| c.path.ends_with(".mozilla")));
    }

    #[test]
    fn guess_tier_only_surfaced_inside_xdg_dirs() {
        let source = FakeSource { info: HashMap::new(), files: HashMap::new(), file_contents: HashMap::new() };
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".config/mytestapp123")).unwrap();
        fs::create_dir(tmp.path().join(".mytestapp123")).unwrap();

        let candidates = scan_with_home("mytestapp123", Some(tmp.path()), &source).unwrap();
        assert!(candidates.iter().any(|c| c.path.ends_with("mytestapp123") && c.path.starts_with(tmp.path().join(".config"))));
        assert!(!candidates.iter().any(|c| c.path == tmp.path().join(".mytestapp123")));
    }

    #[test]
    fn never_touch_list_overrides_any_match() {
        let mut info = HashMap::new();
        info.insert("ssh".to_string(), "Name            : ssh\n".to_string());
        let source = FakeSource { info, files: HashMap::new(), file_contents: HashMap::new() };
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".ssh")).unwrap();

        let candidates = scan_with_home("ssh", Some(tmp.path()), &source).unwrap();
        assert!(!candidates.iter().any(|c| c.path.ends_with(".ssh")));
    }

    #[test]
    fn parses_pkg_archive_name_right_to_left() {
        assert_eq!(parse_pkg_archive_name("firefox-128.0-1-x86_64.pkg.tar.zst").as_deref(), Some("firefox"));
        assert_eq!(parse_pkg_archive_name("code-oss-1.90.0-1-x86_64.pkg.tar.zst").as_deref(), Some("code-oss"));
    }
}
