//! Grouping, tagging, and interactive review of a tracked app's recorded
//! `$HOME` mutations before `untrack --rollback` acts on them. Pure
//! grouping/tagging logic lives here alongside a thin, testable interactive
//! prompt loop — see the plan for why this replaces `file_category` for
//! this purpose and why grouping is capped at 2 path components.

use anyhow::Result;
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::journal::MutationRecord;
use crate::rollback::home_root_of;

/// Coarse category a mutation group gets tagged with, driving its suggested
/// default action. See `tag_for_home_relative` for the matching rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupTag {
    Cache,
    Log,
    State,
    Tmp,
    Data,
}

impl GroupTag {
    fn label(&self) -> &'static str {
        match self {
            GroupTag::Cache => "Cache",
            GroupTag::Log => "Log",
            GroupTag::State => "State",
            GroupTag::Tmp => "Tmp",
            GroupTag::Data => "Data",
        }
    }
}

/// What a group's tag suggests doing, by default — always overridable by
/// the user in the review loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestedAction {
    Keep,
    Remove,
}

fn suggested_action_for_tag(tag: GroupTag) -> SuggestedAction {
    match tag {
        GroupTag::Data => SuggestedAction::Keep,
        GroupTag::Cache | GroupTag::Log | GroupTag::State | GroupTag::Tmp => SuggestedAction::Remove,
    }
}

/// One reviewable bucket of mutations, keyed by a home-relative path capped
/// at 2 components.
#[derive(Debug, Clone)]
pub struct MutationGroup {
    pub key: PathBuf,
    pub tag: GroupTag,
    pub suggested: SuggestedAction,
    pub paths: Vec<String>,
}

impl MutationGroup {
    pub fn file_count(&self) -> usize {
        self.paths.len()
    }
}

/// A mutation `process_mutation` would skip regardless of any review
/// decision — mirrors the static (filesystem-independent) subset of its
/// skip logic, so the review UI never wastes a prompt on a group whose
/// answer can't actually change anything.
fn always_skipped(operation: &str) -> bool {
    matches!(operation, "rename_from" | "rename")
}

/// Home-relative path, capped at 2 components below `$HOME` (or `/root`).
/// `None` if the path doesn't resolve under any known home — such
/// mutations are never grouped, and callers must treat that as "always
/// process normally", never as "silently excluded".
pub fn group_key_for_path(path: &Path) -> Option<PathBuf> {
    let home = home_root_of(path)?;
    let rel = path.strip_prefix(&home).ok()?;
    let capped: PathBuf = rel.components().take(2).collect();
    if capped.as_os_str().is_empty() {
        return None;
    }
    Some(home.join(capped))
}

/// Match whole path *components* of the home-relative suffix, not a raw
/// substring search — see the plan for why (`cachetools`/`catalog`/`login`
/// false-positive avoidance).
fn tag_for_home_relative(home: &Path, path: &Path) -> GroupTag {
    let rel = path.strip_prefix(home).unwrap_or(path);
    for comp in rel.components() {
        // XDG dirs are dotfiles (~/.cache, ~/.config, ...) — strip a
        // leading dot before comparing, or ".cache" would never match
        // "cache" and every real-world cache/log dir would silently fall
        // through to the Data default instead of being tagged correctly.
        let raw = comp.as_os_str().to_string_lossy().to_lowercase();
        let s = raw.trim_start_matches('.');
        match s {
            "cache" => return GroupTag::Cache,
            "log" | "logs" => return GroupTag::Log,
            "state" => return GroupTag::State,
            "tmp" | "temp" => return GroupTag::Tmp,
            _ => {}
        }
    }
    GroupTag::Data
}

/// Group a transaction's mutations into reviewable buckets, deterministically
/// sorted by key. Mutations that are always skipped regardless of review
/// (see `always_skipped`) or that don't resolve to any home are excluded
/// from grouping entirely — the former because reviewing them would be
/// misleading busywork, the latter because they must always fall through to
/// normal processing (see `group_key_for_path`'s doc).
pub fn group_mutations(mutations: &[MutationRecord]) -> Vec<MutationGroup> {
    use std::collections::BTreeMap;

    let mut buckets: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for m in mutations {
        if always_skipped(&m.operation) {
            continue;
        }
        let path = Path::new(&m.path);
        let Some(key) = group_key_for_path(path) else {
            continue;
        };
        buckets.entry(key).or_default().push(m.path.clone());
    }

    buckets
        .into_iter()
        .map(|(key, paths)| {
            let home = home_root_of(&key).unwrap_or_else(|| key.clone());
            let tag = tag_for_home_relative(&home, &key);
            let suggested = suggested_action_for_tag(tag);
            MutationGroup { key, tag, suggested, paths }
        })
        .collect()
}

/// Per-group parsed response.
enum Answer {
    UseDefault,
    Remove,
    Keep,
    RemoveAllRemaining,
    KeepAllRemaining,
    ListFiles,
    Invalid,
}

fn parse_answer(line: &str) -> Answer {
    match line.trim().to_lowercase().as_str() {
        "" => Answer::UseDefault,
        "r" => Answer::Remove,
        "k" => Answer::Keep,
        "a" => Answer::RemoveAllRemaining,
        "s" => Answer::KeepAllRemaining,
        "l" => Answer::ListFiles,
        _ => Answer::Invalid,
    }
}

/// Drive the interactive per-group review loop over generic `BufRead`/`Write`
/// so the branching logic (defaults, bulk shortcuts, `l`, EOF) is fully
/// unit-testable via `Cursor<&[u8]>` fixtures, independent of real stdin.
/// Returns the set of group keys the caller selected to remove.
pub fn review_groups<R: BufRead, W: Write>(
    groups: &[MutationGroup],
    input: &mut R,
    output: &mut W,
) -> Result<HashSet<PathBuf>> {
    let mut selected: HashSet<PathBuf> = HashSet::new();
    let mut force_remove_rest = false;
    let mut force_keep_rest = false;

    for group in groups {
        if force_remove_rest {
            selected.insert(group.key.clone());
            continue;
        }
        if force_keep_rest {
            continue;
        }

        loop {
            writeln!(
                output,
                "  {} [{}] {} files — suggested: {} (Enter=accept, r=remove, k=keep, a=remove all remaining, s=keep all remaining, l=list files)",
                group.key.display(),
                group.tag.label(),
                group.file_count(),
                match group.suggested {
                    SuggestedAction::Remove => "remove",
                    SuggestedAction::Keep => "keep",
                }
            )?;
            write!(output, "  > ")?;
            output.flush()?;

            let mut line = String::new();
            let n = input.read_line(&mut line)?;
            if n == 0 {
                // EOF mid-review: default this and every remaining group,
                // then stop — never hang or error on exhausted/closed stdin.
                writeln!(
                    output,
                    "  (input ended — using suggested defaults for this and all remaining groups)"
                )?;
                if group.suggested == SuggestedAction::Remove {
                    selected.insert(group.key.clone());
                }
                for remaining in groups.iter().skip_while(|g| g.key != group.key).skip(1) {
                    if remaining.suggested == SuggestedAction::Remove {
                        selected.insert(remaining.key.clone());
                    }
                }
                return Ok(selected);
            }

            match parse_answer(&line) {
                Answer::UseDefault => {
                    if group.suggested == SuggestedAction::Remove {
                        selected.insert(group.key.clone());
                    }
                    break;
                }
                Answer::Remove => {
                    selected.insert(group.key.clone());
                    break;
                }
                Answer::Keep => {
                    break;
                }
                Answer::RemoveAllRemaining => {
                    selected.insert(group.key.clone());
                    force_remove_rest = true;
                    break;
                }
                Answer::KeepAllRemaining => {
                    force_keep_rest = true;
                    break;
                }
                Answer::ListFiles => {
                    for p in &group.paths {
                        writeln!(output, "    {}", p)?;
                    }
                    continue;
                }
                Answer::Invalid => {
                    writeln!(output, "  (unrecognized input, try again)")?;
                    continue;
                }
            }
        }
    }

    Ok(selected)
}

/// Real-stdin/stdout wrapper around `review_groups`.
pub fn review_groups_interactive(groups: &[MutationGroup]) -> Result<HashSet<PathBuf>> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stdout();
    review_groups(groups, &mut input, &mut output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn mutation(path: &str, op: &str) -> MutationRecord {
        MutationRecord {
            id: None,
            txid: 1,
            pid: None,
            operation: op.to_string(),
            path: path.to_string(),
            timestamp: chrono::Utc::now(),
            file_category: "UserData".to_string(),
            pre_hash: None,
            post_hash: None,
        }
    }

    #[test]
    fn group_key_caps_deep_nesting_to_one_bucket() {
        let key = group_key_for_path(Path::new(
            "/home/alice/testproj/node_modules/lodash/fp/get.js",
        ));
        assert_eq!(key, Some(PathBuf::from("/home/alice/testproj/node_modules")));
    }

    #[test]
    fn group_key_collapses_config_nesting() {
        let key = group_key_for_path(Path::new("/home/alice/.config/weechat/weechat.conf"));
        assert_eq!(key, Some(PathBuf::from("/home/alice/.config/weechat")));
    }

    #[test]
    fn group_key_none_for_unresolvable_home() {
        assert_eq!(group_key_for_path(Path::new("/usr/bin/foo")), None);
    }

    #[test]
    fn tag_priority_picks_first_match() {
        // "cache" appears before "log" in priority order.
        let home = Path::new("/home/alice");
        let tag = tag_for_home_relative(home, Path::new("/home/alice/cache/log"));
        assert_eq!(tag, GroupTag::Cache);
    }

    #[test]
    fn tag_matching_handles_real_dotfile_xdg_dirs() {
        // Real XDG dirs are dotfiles (~/.cache, ~/.local/state, ...) — a
        // path component keeps its leading dot (".cache", not "cache"),
        // which a naive exact-match against "cache" would miss entirely.
        let home = Path::new("/home/alice");
        assert_eq!(tag_for_home_relative(home, Path::new("/home/alice/.cache/app/f")), GroupTag::Cache);
        assert_eq!(
            tag_for_home_relative(home, Path::new("/home/alice/.local/state/app/f")),
            GroupTag::State
        );
        assert_eq!(tag_for_home_relative(home, Path::new("/home/alice/.config/app/f")), GroupTag::Data);
    }

    #[test]
    fn tag_matching_does_not_mistag_embedded_substrings() {
        let home = Path::new("/home/alice");
        assert_eq!(
            tag_for_home_relative(home, Path::new("/home/alice/.local/share/cachetools")),
            GroupTag::Data
        );
        assert_eq!(
            tag_for_home_relative(home, Path::new("/home/alice/.local/share/catalog")),
            GroupTag::Data
        );
        assert_eq!(
            tag_for_home_relative(home, Path::new("/home/alice/.config/login")),
            GroupTag::Data
        );
    }

    #[test]
    fn group_mutations_excludes_always_skipped_and_ungroupable() {
        let mutations = vec![
            mutation("/home/alice/.config/app/a.conf", "create"),
            mutation("/home/alice/.config/app/old.conf", "rename_from"),
            mutation("/usr/bin/whatever", "create"),
        ];
        let groups = group_mutations(&mutations);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].paths, vec!["/home/alice/.config/app/a.conf".to_string()]);
    }

    #[test]
    fn review_groups_empty_input_uses_suggested_defaults() {
        let groups = vec![
            MutationGroup {
                key: PathBuf::from("/home/alice/.cache/app"),
                tag: GroupTag::Cache,
                suggested: SuggestedAction::Remove,
                paths: vec!["/home/alice/.cache/app/f".to_string()],
            },
            MutationGroup {
                key: PathBuf::from("/home/alice/.config/app"),
                tag: GroupTag::Data,
                suggested: SuggestedAction::Keep,
                paths: vec!["/home/alice/.config/app/f".to_string()],
            },
        ];
        let mut input = Cursor::new(b"\n\n".to_vec());
        let mut output = Vec::new();
        let selected = review_groups(&groups, &mut input, &mut output).unwrap();
        assert!(selected.contains(&PathBuf::from("/home/alice/.cache/app")));
        assert!(!selected.contains(&PathBuf::from("/home/alice/.config/app")));
    }

    #[test]
    fn review_groups_a_stops_consuming_further_input() {
        let groups = vec![
            MutationGroup {
                key: PathBuf::from("/home/alice/g1"),
                tag: GroupTag::Data,
                suggested: SuggestedAction::Keep,
                paths: vec!["/home/alice/g1/f".to_string()],
            },
            MutationGroup {
                key: PathBuf::from("/home/alice/g2"),
                tag: GroupTag::Data,
                suggested: SuggestedAction::Keep,
                paths: vec!["/home/alice/g2/f".to_string()],
            },
        ];
        // "a" on the first group should remove both, without consuming the
        // bogus leftover line queued for the second group.
        let mut input = Cursor::new(b"a\nk\n".to_vec());
        let mut output = Vec::new();
        let selected = review_groups(&groups, &mut input, &mut output).unwrap();
        assert!(selected.contains(&PathBuf::from("/home/alice/g1")));
        assert!(selected.contains(&PathBuf::from("/home/alice/g2")));
    }

    #[test]
    fn review_groups_s_keeps_all_remaining() {
        let groups = vec![
            MutationGroup {
                key: PathBuf::from("/home/alice/g1"),
                tag: GroupTag::Cache,
                suggested: SuggestedAction::Remove,
                paths: vec!["/home/alice/g1/f".to_string()],
            },
            MutationGroup {
                key: PathBuf::from("/home/alice/g2"),
                tag: GroupTag::Cache,
                suggested: SuggestedAction::Remove,
                paths: vec!["/home/alice/g2/f".to_string()],
            },
        ];
        let mut input = Cursor::new(b"s\n".to_vec());
        let mut output = Vec::new();
        let selected = review_groups(&groups, &mut input, &mut output).unwrap();
        assert!(selected.is_empty());
    }

    #[test]
    fn review_groups_l_lists_then_reprompts_same_group() {
        let groups = vec![MutationGroup {
            key: PathBuf::from("/home/alice/g1"),
            tag: GroupTag::Data,
            suggested: SuggestedAction::Keep,
            paths: vec!["/home/alice/g1/a".to_string(), "/home/alice/g1/b".to_string()],
        }];
        let mut input = Cursor::new(b"l\nr\n".to_vec());
        let mut output = Vec::new();
        let selected = review_groups(&groups, &mut input, &mut output).unwrap();
        assert!(selected.contains(&PathBuf::from("/home/alice/g1")));
        let printed = String::from_utf8(output).unwrap();
        assert!(printed.contains("/home/alice/g1/a"));
        assert!(printed.contains("/home/alice/g1/b"));
    }

    #[test]
    fn review_groups_eof_mid_review_defaults_remaining_and_stops() {
        let groups = vec![
            MutationGroup {
                key: PathBuf::from("/home/alice/g1"),
                tag: GroupTag::Data,
                suggested: SuggestedAction::Keep,
                paths: vec!["/home/alice/g1/f".to_string()],
            },
            MutationGroup {
                key: PathBuf::from("/home/alice/g2"),
                tag: GroupTag::Cache,
                suggested: SuggestedAction::Remove,
                paths: vec!["/home/alice/g2/f".to_string()],
            },
        ];
        // Empty input: read_line immediately returns Ok(0) (EOF) on the very
        // first group.
        let mut input = Cursor::new(Vec::new());
        let mut output = Vec::new();
        let selected = review_groups(&groups, &mut input, &mut output).unwrap();
        assert!(!selected.contains(&PathBuf::from("/home/alice/g1"))); // Keep default
        assert!(selected.contains(&PathBuf::from("/home/alice/g2"))); // Remove default
    }

    #[test]
    fn review_groups_invalid_input_reprompts() {
        let groups = vec![MutationGroup {
            key: PathBuf::from("/home/alice/g1"),
            tag: GroupTag::Data,
            suggested: SuggestedAction::Keep,
            paths: vec!["/home/alice/g1/f".to_string()],
        }];
        let mut input = Cursor::new(b"bogus\nr\n".to_vec());
        let mut output = Vec::new();
        let selected = review_groups(&groups, &mut input, &mut output).unwrap();
        assert!(selected.contains(&PathBuf::from("/home/alice/g1")));
    }
}
