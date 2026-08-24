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
/// instead of shelling out to a real package manager/cache dir. One impl
/// per supported package manager (pacman/dpkg/rpm) — see `PacmanMetadataSource`,
/// `DpkgMetadataSource`, `RpmMetadataSource` below.
pub trait PackageMetadataSource {
    /// Metadata for `app` if currently installed, normalized to the same
    /// `Key            : value` shape `pacman -Qi` uses regardless of which
    /// package manager this source wraps (`extract_field` only ever looks
    /// for that one shape plus `.PKGINFO`'s `key = value` shape) — e.g. a
    /// dpkg-backed source still emits `Name            : <pkg>` even though
    /// `dpkg -s` itself would say `Package:`. This keeps `derive_signals`'s
    /// shared parsing logic below completely package-manager-agnostic.
    fn query_info(&self, app: &str) -> Option<String>;
    /// Every file `app` owns, as bare absolute paths (no leading package-name
    /// token — pacman's own `-Ql` output is the one exception, so
    /// `PacmanMetadataSource` strips that prefix itself before returning).
    fn query_files(&self, app: &str) -> Option<Vec<String>>;
    /// Path to a cached package archive for `app` (pacman: `/var/cache/pacman/pkg/`,
    /// dpkg: `/var/cache/apt/archives/`, rpm: best-effort under dnf's cache
    /// dirs, which aren't guaranteed to be populated), if one exists (most
    /// recent mtime wins).
    fn cached_archive(&self, app: &str) -> Option<PathBuf>;
    /// Metadata extracted from a cached archive, same normalized shape as
    /// `query_info`.
    fn archive_pkginfo(&self, archive: &Path) -> Option<String>;
    /// File list extracted from a cached archive, same normalized bare-path
    /// shape as `query_files`.
    fn archive_file_list(&self, archive: &Path) -> Option<Vec<String>>;
    /// Read a file's contents (used for `.desktop`/`.metainfo.xml` files
    /// referenced by a package's file list). Real impl just reads from disk;
    /// tests can supply canned content instead.
    fn read_file(&self, path: &str) -> Option<String>;
    /// Read a file's contents straight out of a cached archive, for the
    /// already-uninstalled case where the live path no longer exists on
    /// disk. `rel_path` is archive-relative (no leading `/`).
    fn read_file_in_archive(&self, archive: &Path, rel_path: &str) -> Option<String>;
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

    fn query_files(&self, app: &str) -> Option<Vec<String>> {
        let out = Command::new("pacman").args(["-Ql", app]).output().ok()?;
        if out.status.success() {
            // `pacman -Ql` lines are `<pkg> <path>` — strip the leading
            // package-name token so callers see the same bare-path shape
            // every other source returns.
            Some(
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter_map(|l| l.split_once(' ').map(|(_, p)| p.trim().to_string()))
                    .collect(),
            )
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

    fn archive_file_list(&self, archive: &Path) -> Option<Vec<String>> {
        let out = Command::new("bsdtar").args(["-tf", archive.to_str()?]).output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect())
        } else {
            None
        }
    }

    fn read_file(&self, path: &str) -> Option<String> {
        fs::read_to_string(path).ok()
    }

    fn read_file_in_archive(&self, archive: &Path, rel_path: &str) -> Option<String> {
        let out = Command::new("bsdtar").args(["-xOf", archive.to_str()?, rel_path]).output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            None
        }
    }
}

/// Best-effort: newest matching cache entry under `cache_dir` whose filename
/// (via `parse_name`) resolves to `app`. Shared by dpkg's and rpm's
/// `cached_archive` impls, which only differ in `cache_dir` and their
/// filename-parsing convention.
fn newest_cached_archive(cache_dir: &Path, app: &str, parse_name: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let entries = fs::read_dir(cache_dir).ok()?;
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let fname = match path.file_name().and_then(|f| f.to_str()) {
            Some(f) => f,
            None => continue,
        };
        if parse_name(fname).as_deref() != Some(app) {
            continue;
        }
        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if best.as_ref().map(|(_, t)| mtime > *t).unwrap_or(true) {
            best = Some((path, mtime));
        }
    }
    best.map(|(p, _)| p)
}

/// Parse a `.deb` cache filename (`<name>_<version>_<arch>.deb`) and return
/// just `<name>`.
fn parse_deb_archive_name(fname: &str) -> Option<String> {
    let no_ext = fname.strip_suffix(".deb")?;
    no_ext.split('_').next().map(str::to_string)
}

pub struct DpkgMetadataSource;

impl PackageMetadataSource for DpkgMetadataSource {
    fn query_info(&self, app: &str) -> Option<String> {
        // dpkg-query's own field names (Package/Homepage) don't match
        // pacman-Qi's (Name/URL) — request the normalized shape directly via
        // a custom format string so the shared `extract_field` parsing in
        // this module needs no dpkg-specific knowledge at all.
        let out = Command::new("dpkg-query")
            .args(["-W", "-f=Name            : ${Package}\nURL             : ${Homepage}\n", app])
            .output()
            .ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            None
        }
    }

    fn query_files(&self, app: &str) -> Option<Vec<String>> {
        let out = Command::new("dpkg").args(["-L", app]).output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).lines().map(str::trim).map(String::from).collect())
        } else {
            None
        }
    }

    fn cached_archive(&self, app: &str) -> Option<PathBuf> {
        newest_cached_archive(Path::new("/var/cache/apt/archives"), app, parse_deb_archive_name)
    }

    fn archive_pkginfo(&self, archive: &Path) -> Option<String> {
        // `dpkg-deb -I` dumps the .deb's control file verbatim, which is
        // already `Key: Value` per line — but with dpkg's own field names
        // (Package/Homepage), so still needs remapping into the shared
        // Name/URL shape (unlike `-f` with an explicit format string, `-I`
        // has no per-field templating option).
        let out = Command::new("dpkg-deb").args(["-I", archive.to_str()?]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let control = String::from_utf8_lossy(&out.stdout);
        let name = extract_field(&control, "Package", "Package")?;
        let url = extract_field(&control, "Homepage", "Homepage").unwrap_or("");
        Some(format!("Name            : {name}\nURL             : {url}\n"))
    }

    fn archive_file_list(&self, archive: &Path) -> Option<Vec<String>> {
        // `dpkg-deb -c` lines look like:
        //   -rwxr-xr-x root/root  12345 2024-01-01 12:00 ./usr/bin/foo
        // the path is always the last whitespace-separated field.
        let out = Command::new("dpkg-deb").args(["-c", archive.to_str()?]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.split_whitespace().last())
                .map(|p| format!("/{}", p.trim_start_matches("./")))
                .collect(),
        )
    }

    fn read_file(&self, path: &str) -> Option<String> {
        fs::read_to_string(path).ok()
    }

    fn read_file_in_archive(&self, archive: &Path, rel_path: &str) -> Option<String> {
        // .deb has no single-tool "extract one file to stdout" (unlike
        // bsdtar -xOf) — pipe dpkg-deb's own tarball extraction into tar,
        // via real process piping rather than a shell string (rel_path
        // comes from a package's own file listing, not sanitized for
        // shell-safety).
        use std::process::Stdio;
        let mut fsys = Command::new("dpkg-deb")
            .args(["--fsys-tarfile", archive.to_str()?])
            .stdout(Stdio::piped())
            .spawn()
            .ok()?;
        let fsys_stdout = fsys.stdout.take()?;
        let out = Command::new("tar")
            .args(["-xO", &format!("./{}", rel_path.trim_start_matches('/'))])
            .stdin(fsys_stdout)
            .output()
            .ok()?;
        let _ = fsys.wait();
        if out.status.success() && !out.stdout.is_empty() {
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            None
        }
    }
}

pub struct RpmMetadataSource;

impl PackageMetadataSource for RpmMetadataSource {
    fn query_info(&self, app: &str) -> Option<String> {
        // rpm -qi's own field names (Name/URL) already match pacman -Qi's
        // shape exactly — no remapping needed.
        let out = Command::new("rpm").args(["-qi", app]).output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            None
        }
    }

    fn query_files(&self, app: &str) -> Option<Vec<String>> {
        let out = Command::new("rpm").args(["-ql", app]).output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).lines().map(str::trim).map(String::from).collect())
        } else {
            None
        }
    }

    fn cached_archive(&self, app: &str) -> Option<PathBuf> {
        // Best-effort only: unlike apt, dnf's `keepcache` setting defaults
        // to off on many modern installs (Fedora's dnf5 included), so a
        // downloaded .rpm is often not still around post-install at all —
        // this just checks the couple of directories where one might be.
        for dir in ["/var/cache/libdnf5/system/packages", "/var/cache/dnf"] {
            if let Some(found) = newest_cached_rpm_recursive(Path::new(dir), app) {
                return Some(found);
            }
        }
        None
    }

    fn archive_pkginfo(&self, archive: &Path) -> Option<String> {
        // rpm -qip's output is the exact same "Name        : x" shape as
        // -qi's — no remapping needed here either.
        let out = Command::new("rpm").args(["-qip", archive.to_str()?]).output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            None
        }
    }

    fn archive_file_list(&self, archive: &Path) -> Option<Vec<String>> {
        let out = Command::new("rpm").args(["-qlp", archive.to_str()?]).output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).lines().map(str::trim).map(String::from).collect())
        } else {
            None
        }
    }

    fn read_file(&self, path: &str) -> Option<String> {
        fs::read_to_string(path).ok()
    }

    fn read_file_in_archive(&self, archive: &Path, rel_path: &str) -> Option<String> {
        use std::process::Stdio;
        let mut rpm2cpio = Command::new("rpm2cpio").arg(archive).stdout(Stdio::piped()).spawn().ok()?;
        let rpm2cpio_stdout = rpm2cpio.stdout.take()?;
        let out = Command::new("cpio")
            .args(["--extract", "--to-stdout", &format!("./{}", rel_path.trim_start_matches('/'))])
            .stdin(rpm2cpio_stdout)
            .output()
            .ok()?;
        let _ = rpm2cpio.wait();
        if out.status.success() && !out.stdout.is_empty() {
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            None
        }
    }
}

/// Recursively searches under `dir` for an rpm file resolving (via
/// `rpm -qp --qf`) to `app`'s package name — dnf's own cache layout nests
/// packages under a per-repo subdirectory, so a shallow single-directory
/// scan (like `newest_cached_archive` does for apt/pacman) isn't enough.
fn newest_cached_rpm_recursive(dir: &Path, app: &str) -> Option<PathBuf> {
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match fs::read_dir(&d) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rpm") {
                continue;
            }
            let path_str = match path.to_str() {
                Some(s) => s,
                None => continue,
            };
            let out = match Command::new("rpm").args(["-qp", "--qf", "%{NAME}", path_str]).output() {
                Ok(o) if o.status.success() => o,
                _ => continue,
            };
            if String::from_utf8_lossy(&out.stdout) != app {
                continue;
            }
            let mtime = match entry.metadata().and_then(|m| m.modified()) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if best.as_ref().map(|(_, t)| mtime > *t).unwrap_or(true) {
                best = Some((path, mtime));
            }
        }
    }
    best.map(|(p, _)| p)
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

/// `file_list` is already-normalized bare absolute paths (every
/// `PackageMetadataSource` impl returns this shape — see the trait doc).
/// `archive` is `Some` when the package is already uninstalled: the live
/// paths in `file_list` no longer exist on disk, so referenced files
/// (`.desktop`/`.metainfo.xml` content) must be read straight out of the
/// cached archive instead.
fn collect_desktop_and_appstream_tokens(
    source: &dyn PackageMetadataSource,
    file_list: &[String],
    archive: Option<&Path>,
    tokens: &mut HashSet<String>,
    binaries: &mut HashSet<String>,
) {
    let read = |path: &str| -> Option<String> {
        match archive {
            Some(a) => source.read_file_in_archive(a, path.trim_start_matches('/')),
            None => source.read_file(path),
        }
    };
    for line in file_list {
        let path = line.trim();
        if path.ends_with('/') || path.is_empty() {
            continue;
        }
        if path.ends_with(".desktop") {
            if let Some(basename) = Path::new(path).file_stem().and_then(|s| s.to_str()) {
                tokens.insert(basename.to_lowercase());
            }
            if let Some(content) = read(path) {
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
            if let Some(content) = read(path) {
                if let Some(start) = content.find("<id>") {
                    if let Some(end) = content[start..].find("</id>") {
                        let id = &content[start + 4..start + end];
                        // AppStream IDs are typically reverse-DNS
                        // (org.mozilla.firefox). The *vendor* token that
                        // resolves naming mismatches (firefox -> mozilla)
                        // usually sits in a middle segment, not the last
                        // one (which is often just the product name again,
                        // already covered by the package-name signal) — so
                        // every segment goes in, not just the last.
                        for part in id.trim().split('.') {
                            if !part.is_empty() {
                                tokens.insert(part.to_lowercase());
                            }
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

    // Stage 1: still installed. Every source normalizes to bare absolute
    // paths (see the trait doc), so the same bare-path executable filter
    // applies regardless of which package manager `source` wraps.
    // Only treat the app name itself as a "likely" (structural) signal once
    // metadata actually confirms it names a real package — otherwise it's
    // indistinguishable from the raw/guess-tier fallback below.
    if let Some(info) = source.query_info(app) {
        signals.likely.insert(app.to_lowercase());
        apply_metadata(&mut signals, &info);
        if let Some(files) = source.query_files(app) {
            let binaries = crate::tracked_apps::executable_binaries_from_dpkg_listing(&files.join("\n"));
            for b in &binaries {
                if let Some(name) = Path::new(b).file_name().and_then(|s| s.to_str()) {
                    signals.likely.insert(name.to_lowercase());
                }
            }
            let mut extra_bins = HashSet::new();
            collect_desktop_and_appstream_tokens(source, &files, None, &mut signals.structural, &mut extra_bins);
            signals.likely.extend(extra_bins);
        }
        return signals;
    }

    // Stage 2: not installed, but a cached archive may still exist.
    if let Some(archive) = source.cached_archive(app) {
        signals.likely.insert(app.to_lowercase());
        if let Some(pkginfo) = source.archive_pkginfo(&archive) {
            apply_metadata(&mut signals, &pkginfo);
        }
        if let Some(files) = source.archive_file_list(&archive) {
            let abs_paths: Vec<String> =
                files.iter().map(|l| format!("/{}", l.trim_start_matches("./").trim_start_matches('/'))).collect();
            let binaries = crate::tracked_apps::executable_binaries_from_dpkg_listing(&abs_paths.join("\n"));
            for b in &binaries {
                if let Some(name) = Path::new(b).file_name().and_then(|s| s.to_str()) {
                    signals.likely.insert(name.to_lowercase());
                }
            }
            let mut extra_bins = HashSet::new();
            collect_desktop_and_appstream_tokens(
                source,
                &abs_paths,
                Some(&archive),
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

fn which_ok(bin: &str) -> bool {
    Command::new("which").arg(bin).output().map(|o| o.status.success()).unwrap_or(false)
}

/// Auto-detects which package manager is present (same pacman -> dpkg -> rpm
/// precedence `tracked_apps::resolve_app_targets` already uses) and scans
/// using the matching `PackageMetadataSource`. On a system with none of the
/// three, `PacmanMetadataSource` is used anyway — every one of its methods
/// already degrades to `None` when the underlying binary is missing, which
/// `derive_signals` already treats as "stage 3: raw string only", so this
/// stays a safe, already-tested fallback rather than a new code path.
pub fn scan_leftovers(app: &str) -> Result<Vec<LeftoverCandidate>> {
    if which_ok("pacman") {
        scan_with_home(app, None, &PacmanMetadataSource)
    } else if which_ok("dpkg") {
        scan_with_home(app, None, &DpkgMetadataSource)
    } else if which_ok("rpm") {
        scan_with_home(app, None, &RpmMetadataSource)
    } else {
        scan_with_home(app, None, &PacmanMetadataSource)
    }
}

/// Resolve the real invoking user's home directory `getent`-style, even
/// when run under `sudo`. Real (non-dry-run) removal needs root — it writes
/// to root-owned `/var/lib/pkgundo` — but `sudo` resets `$HOME` to root's
/// (`/root`), which would silently scan the wrong home entirely. When
/// `SUDO_USER` is set, resolve *that* user's home instead of trusting `$HOME`.
fn dirs_home() -> Result<PathBuf> {
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        if let Some(home) = home_dir_for_user(&sudo_user) {
            return Ok(home);
        }
    }
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("HOME environment variable is not set"))
}

fn home_dir_for_user(user: &str) -> Option<PathBuf> {
    let out = Command::new("getent").args(["passwd", user]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    line.trim().split(':').nth(5).map(PathBuf::from)
}

/// Resolve a uid to its home directory, for the daemon's exec-watch path (a
/// launching process's real uid, not a username). Deliberately an in-process
/// `getpwuid_r` lookup rather than shelling out to `getent` like
/// `home_dir_for_user` does: this runs on the race-sensitive path between
/// detecting an exec and arming that filesystem's mutation-capture mark
/// (see `daemon::exec_watch`), where a fork+exec of a whole subprocess was
/// measurably widening the window in which a fast-starting app's first
/// writes could be missed.
pub(crate) fn home_dir_for_uid(uid: u32) -> Option<PathBuf> {
    let mut buf = vec![0u8; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let ret = unsafe {
        libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut result)
    };
    if ret != 0 || result.is_null() || pwd.pw_dir.is_null() {
        return None;
    }
    let home = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) }.to_str().ok()?;
    Some(PathBuf::from(home))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn home_dir_for_uid_resolves_root() {
        // uid 0 (root) is universal on any Linux system this test runs on,
        // same reasoning as this module's other tests that lean on real
        // system state (`getent`/`pacman`) rather than mocking everything.
        assert_eq!(home_dir_for_uid(0), Some(PathBuf::from("/root")));
    }

    #[test]
    fn home_dir_for_uid_none_for_nonexistent() {
        assert_eq!(home_dir_for_uid(u32::MAX), None);
    }

    struct FakeSource {
        info: HashMap<String, String>,
        files: HashMap<String, Vec<String>>,
        file_contents: HashMap<String, String>,
    }

    impl PackageMetadataSource for FakeSource {
        fn query_info(&self, app: &str) -> Option<String> {
            self.info.get(app).cloned()
        }
        fn query_files(&self, app: &str) -> Option<Vec<String>> {
            self.files.get(app).cloned()
        }
        fn cached_archive(&self, _app: &str) -> Option<PathBuf> {
            None
        }
        fn archive_pkginfo(&self, _archive: &Path) -> Option<String> {
            None
        }
        fn archive_file_list(&self, _archive: &Path) -> Option<Vec<String>> {
            None
        }
        fn read_file(&self, path: &str) -> Option<String> {
            self.file_contents.get(path).cloned()
        }
        fn read_file_in_archive(&self, _archive: &Path, rel_path: &str) -> Option<String> {
            self.file_contents.get(&format!("/{}", rel_path)).cloned()
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
            vec![
                "/usr/lib/firefox/firefox".to_string(),
                "/usr/bin/firefox".to_string(),
                "/usr/share/applications/firefox.desktop".to_string(),
            ],
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
            fn query_files(&self, _app: &str) -> Option<Vec<String>> {
                None
            }
            fn cached_archive(&self, app: &str) -> Option<PathBuf> {
                Some(PathBuf::from(format!("/fake/{}-1.0-1-x86_64.pkg.tar.zst", app)))
            }
            fn archive_pkginfo(&self, _archive: &Path) -> Option<String> {
                Some("pkgname = firefox\nurl = https://www.mozilla.org/firefox/\n".to_string())
            }
            fn archive_file_list(&self, _archive: &Path) -> Option<Vec<String>> {
                Some(vec!["usr/lib/firefox/firefox".to_string(), "usr/bin/firefox".to_string()])
            }
            fn read_file(&self, _path: &str) -> Option<String> {
                None
            }
            fn read_file_in_archive(&self, _archive: &Path, _rel_path: &str) -> Option<String> {
                None
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".mozilla")).unwrap();
        let candidates = scan_with_home("firefox", Some(tmp.path()), &CachedOnly).unwrap();
        assert!(candidates.iter().any(|c| c.path.ends_with(".mozilla")));
    }

    /// Reproduces a real-world case found via live VM testing: current
    /// Firefox packaging's `URL` field points at firefox.com (post-rebrand),
    /// so the vendor-token-from-URL heuristic finds nothing linking
    /// "firefox" to "mozilla" — but the package's AppStream metainfo ID
    /// (org.mozilla.firefox) still does, and once uninstalled that file
    /// must be read out of the cached archive, not off live disk.
    #[test]
    fn appstream_mid_segment_resolves_vendor_when_url_has_no_hint() {
        struct RebrandedUrlCachedSource;
        impl PackageMetadataSource for RebrandedUrlCachedSource {
            fn query_info(&self, _app: &str) -> Option<String> {
                None
            }
            fn query_files(&self, _app: &str) -> Option<Vec<String>> {
                None
            }
            fn cached_archive(&self, app: &str) -> Option<PathBuf> {
                Some(PathBuf::from(format!("/fake/{}-153.0.4-1-x86_64.pkg.tar.zst", app)))
            }
            fn archive_pkginfo(&self, _archive: &Path) -> Option<String> {
                Some("pkgname = firefox\nurl = https://www.firefox.com/\n".to_string())
            }
            fn archive_file_list(&self, _archive: &Path) -> Option<Vec<String>> {
                Some(vec!["usr/share/metainfo/org.mozilla.firefox.metainfo.xml".to_string()])
            }
            fn read_file(&self, _path: &str) -> Option<String> {
                None // live disk copy is gone; must come from the archive
            }
            fn read_file_in_archive(&self, _archive: &Path, rel_path: &str) -> Option<String> {
                if rel_path == "usr/share/metainfo/org.mozilla.firefox.metainfo.xml" {
                    Some("<id>org.mozilla.firefox</id>".to_string())
                } else {
                    None
                }
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".mozilla")).unwrap();
        let candidates = scan_with_home("firefox", Some(tmp.path()), &RebrandedUrlCachedSource).unwrap();
        assert!(
            candidates.iter().any(|c| c.path.ends_with(".mozilla") && c.confidence == Confidence::Exact),
            "expected the AppStream ID's mid segment ('mozilla') to resolve the naming mismatch even though \
             the URL field no longer hints at it, got: {:?}",
            candidates
        );
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

    #[test]
    fn parses_deb_archive_name() {
        assert_eq!(parse_deb_archive_name("firefox-esr_128.0-1_amd64.deb").as_deref(), Some("firefox-esr"));
        assert_eq!(parse_deb_archive_name("cowsay_3.03+dfsg2-8_all.deb").as_deref(), Some("cowsay"));
        assert_eq!(parse_deb_archive_name("not-a-deb.txt"), None);
    }
}
