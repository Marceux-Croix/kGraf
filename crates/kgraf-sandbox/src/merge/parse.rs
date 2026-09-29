//! Pure parsers and path rules for merge-back. No git processes here.

use std::collections::BTreeMap;

use unicode_normalization::UnicodeNormalization;

use super::state::{ConflictMessage, ConflictSide, ConflictedPath};

/// `git merge-tree --write-tree` arrived in 2.38, `--attr-source` in 2.42.
pub(super) const MIN_GIT: (u32, u32) = (2, 42);

/// Mode git records for a gitlink (a commit of another repository).
pub(super) const GITLINK_MODE: &str = "160000";

/// Mode git records for a symbolic link (its blob is the target).
pub(super) const SYMLINK_MODE: &str = "120000";

/// `(major, minor)` from `git version 2.54.0` or `git version 2.39.3 (Apple Git-146)`.
pub(super) fn parse_version(raw: &str) -> Option<(u32, u32)> {
    let mut it = raw
        .strip_prefix("git version ")?
        .split(|c: char| !c.is_ascii_digit());
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

pub(super) struct MergeTreeOutput {
    pub tree: String,
    pub paths: Vec<ConflictedPath>,
    pub messages: Vec<ConflictMessage>,
}

/// Parse `git merge-tree --write-tree -z` output:
/// `<tree> NUL`, then per conflicted stage `<mode> SP <oid> SP <stage> TAB
/// <path> NUL`, an empty field, then messages as `<n> NUL <path>{n} NUL
/// <kind> NUL <message> NUL`. A clean merge prints only the tree.
pub(super) fn parse_merge_tree_z(out: &[u8]) -> Result<MergeTreeOutput, String> {
    let text = std::str::from_utf8(out).map_err(|e| format!("non-UTF-8 output: {e}"))?;
    let mut fields = text.split('\0');
    let tree = fields
        .next()
        .filter(|t| !t.is_empty())
        .ok_or("no tree id")?
        .to_string();
    let mut paths: Vec<ConflictedPath> = Vec::new();
    for field in fields.by_ref() {
        if field.is_empty() {
            break;
        }
        let (meta, path) = field
            .split_once('\t')
            .ok_or_else(|| format!("conflict entry without a path: {field:?}"))?;
        let mut meta = meta.split(' ');
        let (Some(mode), Some(blob), Some(stage), None) =
            (meta.next(), meta.next(), meta.next(), meta.next())
        else {
            return Err(format!("malformed conflict entry {field:?}"));
        };
        let side = Some(ConflictSide {
            mode: mode.to_string(),
            blob: blob.to_string(),
        });
        let index = match paths.iter().position(|p| p.path == path) {
            Some(i) => i,
            None => {
                paths.push(ConflictedPath {
                    path: path.to_string(),
                    base: None,
                    integration: None,
                    attempt: None,
                });
                paths.len() - 1
            }
        };
        let entry = &mut paths[index];
        match stage {
            "1" => entry.base = side,
            "2" => entry.integration = side,
            "3" => entry.attempt = side,
            other => return Err(format!("unknown stage {other:?} for {path}")),
        }
    }
    let mut messages = Vec::new();
    while let Some(count) = fields.next() {
        if count.is_empty() {
            continue;
        }
        let n: usize = count
            .parse()
            .map_err(|_| format!("bad message path count {count:?}"))?;
        let mut msg_paths = Vec::with_capacity(n);
        for _ in 0..n {
            msg_paths.push(fields.next().ok_or("message path missing")?.to_string());
        }
        let kind = fields.next().ok_or("message kind missing")?.to_string();
        let message = fields
            .next()
            .ok_or("message text missing")?
            .trim_end()
            .to_string();
        messages.push(ConflictMessage {
            paths: msg_paths,
            kind,
            message,
        });
    }
    Ok(MergeTreeOutput {
        tree,
        paths,
        messages,
    })
}

/// One line of `git diff-index --cached --raw -z --no-renames <tree>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RawChange {
    pub old_mode: String,
    pub new_mode: String,
    /// Blob the path holds after the change (for a link, its target).
    pub new_oid: String,
    pub path: String,
}

/// Parse `:<old mode> <new mode> <old oid> <new oid> <status> NUL <path> NUL`.
pub(super) fn parse_raw_diff_z(out: &[u8]) -> Result<Vec<RawChange>, String> {
    let text = std::str::from_utf8(out).map_err(|e| format!("non-UTF-8 output: {e}"))?;
    let mut fields = text.split('\0');
    let mut changes = Vec::new();
    while let Some(meta) = fields.next() {
        if meta.is_empty() {
            continue;
        }
        let meta = meta
            .strip_prefix(':')
            .ok_or_else(|| format!("raw diff record without ':': {meta:?}"))?;
        let parts: Vec<&str> = meta.split(' ').collect();
        let [old_mode, new_mode, _, new_oid, _status] = parts[..] else {
            return Err(format!("malformed raw diff record {meta:?}"));
        };
        let path = fields
            .next()
            .filter(|p| !p.is_empty())
            .ok_or("raw diff record without a path")?;
        changes.push(RawChange {
            old_mode: old_mode.to_string(),
            new_mode: new_mode.to_string(),
            new_oid: new_oid.to_string(),
            path: path.to_string(),
        });
    }
    Ok(changes)
}

/// Paths that steer git itself rather than hold project content: attributes
/// pick filter and merge drivers, `.gitmodules` points at other repositories,
/// and a `.git*` directory can be a hooks path (this project's own hooks live
/// in `.githooks`). An attempt that touches one is refused, never merged.
/// `.gitignore`, `.gitkeep` and `.github/` are ordinary project files.
///
/// The state directories `.kgraf` and `.haustoria` (its former name, decision
/// 0062) are refused too: a worker that landed either in the checkout would
/// plant state (or make the "both state directories exist" check refuse the
/// project). Each component goes through `kgraf_core::paths::is_protected_name`,
/// the runtime's own rule, so case, Unicode, trailing-dot and NTFS short-name
/// spellings are caught the same way.
pub(super) fn is_protected(path: &str) -> bool {
    path.split('/').any(kgraf_core::paths::is_protected_name)
}

/// Case-folded, NFC-normalized form. Two distinct paths with the same key
/// are one file on a case-insensitive or normalization-insensitive file
/// system (macOS and Windows defaults), so one would overwrite the other at
/// checkout.
fn collision_key(path: &str) -> String {
    path.nfc().collect::<String>().to_lowercase()
}

/// Groups of distinct paths that share a collision key.
pub(super) fn collision_groups<'a>(
    paths: impl IntoIterator<Item = &'a str>,
) -> BTreeMap<String, Vec<String>> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in paths {
        let group = groups.entry(collision_key(path)).or_default();
        if !group.iter().any(|p| p == path) {
            group.push(path.to_string());
        }
    }
    groups.retain(|_, g| g.len() > 1);
    for g in groups.values_mut() {
        g.sort();
    }
    groups
}

/// Collision groups in `merged` that `before` did not already have. A
/// collision already on the integration branch is not this attempt's doing.
pub(super) fn new_collisions(before: &[String], merged: &[String]) -> Vec<Vec<String>> {
    let old = collision_groups(before.iter().map(String::as_str));
    collision_groups(merged.iter().map(String::as_str))
        .into_iter()
        .filter(|(key, group)| old.get(key) != Some(group))
        .map(|(_, group)| group)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_conflict_output_from_git_2_54() {
        // Captured from `git merge-tree --write-tree -z` (content conflict on
        // `f`, clean add of `h`).
        let raw = b"27d620571c41abe45be7ad2693fae2c71f7d5816\0\
100644 78981922613b2afb6025042ff6bd878ac1994e85 1\tf\0\
100644 b19a1e93bec1317dc609722e12affafbfa74dc2 2\tf\0\
100644 950b81b7eee953d050aa05a641f8e056c85dd1bd 3\tf\0\0\
1\0f\0Auto-merging\0Auto-merging f\n\0\
1\0f\0CONFLICT (contents)\0CONFLICT (content): Merge conflict in f\n\0";
        let parsed = parse_merge_tree_z(raw).unwrap();
        assert_eq!(parsed.tree, "27d620571c41abe45be7ad2693fae2c71f7d5816");
        assert_eq!(parsed.paths.len(), 1);
        let f = &parsed.paths[0];
        assert_eq!(f.path, "f");
        assert!(f.base.is_some() && f.integration.is_some() && f.attempt.is_some());
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[1].kind, "CONFLICT (contents)");
        assert_eq!(
            parsed.messages[1].message,
            "CONFLICT (content): Merge conflict in f"
        );
    }

    #[test]
    fn parses_clean_output_and_rejects_garbage() {
        let clean = parse_merge_tree_z(b"c74c5219eb8696489a35cd7fc9fc88e3488150d2\0").unwrap();
        assert!(clean.paths.is_empty() && clean.messages.is_empty());
        assert!(parse_merge_tree_z(b"").is_err());
        assert!(parse_merge_tree_z(b"t\x00100644 x 9\tf\x00\x00").is_err());
        assert!(parse_merge_tree_z(b"t\0nonsense\0\0").is_err());
    }

    #[test]
    fn parses_raw_diff_records() {
        let raw = b":000000 160000 0000 abcd A\0sub\0:100644 100644 aa bb M\0a b.txt\0";
        let changes = parse_raw_diff_z(raw).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].new_mode, GITLINK_MODE);
        assert_eq!(changes[0].path, "sub");
        assert_eq!(changes[1].path, "a b.txt");
        assert!(parse_raw_diff_z(b"100644 x\0p\0").is_err());
        assert!(parse_raw_diff_z(b":1 2 3 4 A\0").is_err());
    }

    #[test]
    fn version_gate() {
        assert!(parse_version("git version 2.54.0").is_some_and(|v| v >= MIN_GIT));
        assert!(parse_version("git version 2.42.0 (Apple Git-150)").is_some_and(|v| v >= MIN_GIT));
        assert!(parse_version("git version 2.39.3 (Apple Git-146)").is_some_and(|v| v < MIN_GIT));
        assert_eq!(parse_version("not git"), None);
    }

    #[test]
    fn protected_paths() {
        for p in [
            ".gitattributes",
            "sub/.gitattributes",
            ".gitmodules",
            ".GitAttributes",
            ".githooks/pre-commit",
            "a/.gitconfig",
            // Decision 0062: the state directory under both names, however spelled.
            ".kgraf/ledger.db",
            ".haustoria/x",
            ".Haustoria/x",
            "sub/.HAUSTORIA./x",
            "HAUSTO~1/x",
            "KGRAF~1/x",
            "GIT~1/config",
        ] {
            assert!(is_protected(p), "{p}");
        }
        for p in [
            ".gitignore",
            "a/.gitkeep",
            ".github/workflows/ci.yml",
            "git.rs",
            "x.git",
        ] {
            assert!(!is_protected(p), "{p}");
        }
    }

    #[test]
    fn collisions_fold_case_and_normalization() {
        let nfc = "caf\u{e9}.txt";
        let nfd = "cafe\u{301}.txt";
        let groups = collision_groups(["Foo.txt", "foo.txt", "bar", nfc, nfd, "Dir", "dir"]);
        assert_eq!(groups.len(), 3);
        assert!(groups.values().any(|g| g == &["Foo.txt", "foo.txt"]));
        let before = vec!["Dir".to_string(), "dir".to_string()];
        let merged = vec![
            "Dir".to_string(),
            "dir".to_string(),
            "A".to_string(),
            "a".to_string(),
        ];
        assert_eq!(new_collisions(&before, &merged), vec![vec!["A", "a"]]);
    }
}
