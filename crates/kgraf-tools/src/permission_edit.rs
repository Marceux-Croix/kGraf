//! Editing the rule lists of the user-level `permissions.toml` in place
//! (decision 0094), for Settings > Permissions.
//!
//! The file's text is edited, not rebuilt from [`PermissionSettings`]: a
//! rebuild would drop the user's comments and the order of their keys, and
//! would erase a line the loader could not read. Only the one list an edit
//! names changes; every other key, comment and blank line stays as written.
//!
//! Validation is not written here. [`validate`] calls the readers the file
//! loader uses ([`Rule::parse`], [`AllowSet::with_programs`],
//! [`AllowSet::with_tools`]), so a rule is refused with the loader's own
//! message.

use crate::permissions::{AllowSet, Rule};
use std::path::{Path, PathBuf};
use toml_edit::{Array, DocumentMut, Item, Table, Value};

/// One list in the file. Deny and ask are rules (`tool(pattern)`); the allow
/// set has three lists, each a different kind of entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuleKind {
    /// `deny = [...]`: blocks in every mode.
    Deny,
    /// `ask = [...]`: asks in every mode.
    Ask,
    /// `[allow] programs = [...]`: shell programs Auto grants.
    AllowProgram,
    /// `[allow] tools = [...]`: tools Auto grants.
    AllowTool,
    /// `[allow] paths = [...]`: paths a contract's write scope may widen to.
    AllowPath,
}

impl RuleKind {
    pub const ALL: [RuleKind; 5] = [
        Self::Deny,
        Self::Ask,
        Self::AllowProgram,
        Self::AllowTool,
        Self::AllowPath,
    ];

    /// The heading a list shows under.
    pub fn label(self) -> &'static str {
        match self {
            Self::Deny => "Deny rules",
            Self::Ask => "Ask rules",
            Self::AllowProgram => "Allowed programs",
            Self::AllowTool => "Allowed tools",
            Self::AllowPath => "Allowed paths",
        }
    }

    /// What the list does, in one sentence.
    pub fn explanation(self) -> &'static str {
        match self {
            Self::Deny => "Blocked in every mode, Bypass included.",
            Self::Ask => "Asked about in every mode.",
            Self::AllowProgram => "Shell programs Auto grants without asking.",
            Self::AllowTool => "Tools Auto grants without asking.",
            Self::AllowPath => {
                "Paths a contract's write scope may widen to; end with * for a prefix."
            }
        }
    }

    /// An example of one entry, for the empty field.
    pub fn example(self) -> &'static str {
        match self {
            Self::Deny => "shell(curl*)",
            Self::Ask => "shell(git push*)",
            Self::AllowProgram => "make",
            Self::AllowTool => "apply_patch",
            Self::AllowPath => "docs/*",
        }
    }

    /// The key in the file: a top-level key, or a key of `[allow]`.
    fn key(self) -> (Option<&'static str>, &'static str) {
        match self {
            Self::Deny => (None, "deny"),
            Self::Ask => (None, "ask"),
            Self::AllowProgram => (Some("allow"), "programs"),
            Self::AllowTool => (Some("allow"), "tools"),
            Self::AllowPath => (Some("allow"), "paths"),
        }
    }
}

/// Check one entry the way the file loader does, and give the entry as it
/// will be written (trimmed). The message is the loader's.
pub fn validate(kind: RuleKind, text: &str) -> Result<String, String> {
    let t = text.trim();
    match kind {
        RuleKind::Deny | RuleKind::Ask => Rule::parse(t).map(|r| r.text()),
        RuleKind::AllowProgram => AllowSet::default().with_programs([t]).map(|_| t.to_owned()),
        RuleKind::AllowTool => AllowSet::default().with_tools([t]).map(|_| t.to_owned()),
        // The loader reads a path as written; only an empty one is no entry.
        RuleKind::AllowPath if t.is_empty() => Err("an allowed path is empty".to_owned()),
        RuleKind::AllowPath => Ok(t.to_owned()),
    }
}

fn parse(text: &str) -> Result<DocumentMut, String> {
    text.parse::<DocumentMut>().map_err(|e| e.to_string())
}

fn list(doc: &DocumentMut, kind: RuleKind) -> Option<&Item> {
    let (table, key) = kind.key();
    match table {
        None => doc.get(key),
        Some(t) => doc.get(t)?.as_table_like()?.get(key),
    }
}

/// The entries of one list, as written (an absent list is empty). A value
/// that is not a string is left out; the loader refuses such a file.
pub fn entries(text: &str, kind: RuleKind) -> Result<Vec<String>, String> {
    let doc = parse(text)?;
    let Some(item) = list(&doc, kind) else {
        return Ok(Vec::new());
    };
    let array = item
        .as_array()
        .ok_or_else(|| format!("`{}` in the file is not a list", kind.key().1))?;
    Ok(array
        .iter()
        .filter_map(Value::as_str)
        .map(|s| s.trim().to_owned())
        .collect())
}

/// The list to edit, created when `create` and absent.
fn list_mut(
    doc: &mut DocumentMut,
    kind: RuleKind,
    create: bool,
) -> Result<Option<&mut Array>, String> {
    let (table, key) = kind.key();
    let not_list = || format!("`{key}` in the file is not a list");
    let parent: &mut dyn toml_edit::TableLike = match table {
        None => doc.as_table_mut(),
        Some(t) => {
            if doc.get(t).is_none() {
                if !create {
                    return Ok(None);
                }
                doc.insert(t, Item::Table(Table::new()));
            }
            doc.get_mut(t)
                .and_then(Item::as_table_like_mut)
                .ok_or_else(|| format!("`{t}` in the file is not a table"))?
        }
    };
    if parent.get(key).is_none() {
        if !create {
            return Ok(None);
        }
        parent.insert(key, Item::Value(Value::Array(Array::new())));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_array_mut)
        .map(Some)
        .ok_or_else(not_list)
}

/// `text` with `entry` added to the list of `kind`. The entry is checked
/// first ([`validate`]). An entry already listed is an error, not a copy.
pub fn with_entry_added(text: &str, kind: RuleKind, entry: &str) -> Result<String, String> {
    let entry = validate(kind, entry)?;
    let mut doc = parse(text)?;
    let array = list_mut(&mut doc, kind, true)?.ok_or("the list could not be created")?;
    if array
        .iter()
        .filter_map(Value::as_str)
        .any(|s| s.trim() == entry)
    {
        return Err(format!("`{entry}` is already listed"));
    }
    array.push(entry);
    Ok(doc.to_string())
}

/// `text` without `entry` in the list of `kind`. Comparing trims both sides,
/// as [`entries`] does. An entry not listed is an error.
pub fn with_entry_removed(text: &str, kind: RuleKind, entry: &str) -> Result<String, String> {
    let entry = entry.trim();
    let mut doc = parse(text)?;
    let array = list_mut(&mut doc, kind, false)?;
    let at = array.as_deref().and_then(|a| {
        a.iter()
            .position(|v| v.as_str().is_some_and(|s| s.trim() == entry))
    });
    match (array, at) {
        (Some(a), Some(i)) => {
            a.remove(i);
            Ok(doc.to_string())
        }
        _ => Err(format!("`{entry}` is not in the file")),
    }
}

/// The file's text; a missing file is empty.
pub fn read_text(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Write `text` to `path` atomically: a temporary file in the same
/// directory, then a rename, so a reader sees the old file or the new one.
pub fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("cannot write {}: {e}", path.display());
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).map_err(fail)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        fail(e)
    })
}

/// Add `entry` to the list of `kind` in the file at `path`.
pub fn add_to_file(path: &Path, kind: RuleKind, entry: &str) -> Result<(), String> {
    let next = with_entry_added(&read_text(path)?, kind, entry)?;
    write_atomic(path, &next)
}

/// Remove `entry` from the list of `kind` in the file at `path`.
pub fn remove_from_file(path: &Path, kind: RuleKind, entry: &str) -> Result<(), String> {
    let next = with_entry_removed(&read_text(path)?, kind, entry)?;
    write_atomic(path, &next)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "# my rules\nmode = \"auto\" # keep\n\ndeny = [\n    \"shell(curl*)\", # no network\n]\n\n[allow]\n# programs I trust\nprograms = [\"make\"]\n";

    #[test]
    fn an_entry_is_checked_with_the_loaders_messages() {
        let from_loader = Rule::parse("WebFetch").unwrap_err();
        assert_eq!(
            validate(RuleKind::Deny, "WebFetch").unwrap_err(),
            from_loader
        );
        assert_eq!(
            validate(RuleKind::Ask, "  Bash(rm *) ").unwrap(),
            "Bash(rm *)"
        );
        assert!(validate(RuleKind::AllowProgram, "two words").is_err());
        assert!(validate(RuleKind::AllowTool, "WebSearch").is_err());
        assert!(validate(RuleKind::AllowPath, " ").is_err());
    }

    #[test]
    fn an_edit_keeps_comments_and_other_keys() {
        let added = with_entry_added(FILE, RuleKind::Ask, "shell(git push*)").expect("add");
        for kept in [
            "# my rules",
            "mode = \"auto\" # keep",
            "# no network",
            "# programs I trust",
        ] {
            assert!(added.contains(kept), "{kept} lost:\n{added}");
        }
        assert_eq!(
            entries(&added, RuleKind::Ask).unwrap(),
            ["shell(git push*)"]
        );
        assert_eq!(entries(&added, RuleKind::Deny).unwrap(), ["shell(curl*)"]);
        let removed = with_entry_removed(&added, RuleKind::AllowProgram, "make").expect("rm");
        assert!(removed.contains("# programs I trust") && removed.contains("# my rules"));
        assert!(
            entries(&removed, RuleKind::AllowProgram)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn lists_are_created_and_a_duplicate_or_a_missing_entry_is_refused() {
        let t = with_entry_added("", RuleKind::AllowTool, "Bash").expect("add");
        assert_eq!(entries(&t, RuleKind::AllowTool).unwrap(), ["Bash"]);
        assert!(with_entry_added(&t, RuleKind::AllowTool, "Bash").is_err());
        assert!(with_entry_removed(&t, RuleKind::Deny, "x").is_err());
    }
}
