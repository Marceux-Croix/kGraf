//! Editing the rule lists of the user-level `permissions.toml` in place
//! (decision 0094), for Settings > Permissions.
//!
//! The file's text is edited, not rebuilt from [`PermissionSettings`]: a
//! rebuild would drop the user's comments and the order of their keys, and
//! would erase a line the loader could not read. Only the one list an edit
//! names changes; every other key, comment and blank line stays as written.
//!
//! Validation is not written here. [`validate`] calls [`validate_entry`], the
//! one check the file loader uses for every entry, so an entry is refused with
//! the loader's own message, and an entry that loads but matches no call
//! carries the loader's notice.

use crate::permissions::{Entry, EntryList, validate_entry};
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

    /// The loader's name for this list.
    pub fn list(self) -> EntryList {
        match self {
            Self::Deny => EntryList::Deny,
            Self::Ask => EntryList::Ask,
            Self::AllowProgram => EntryList::AllowPrograms,
            Self::AllowTool => EntryList::AllowTools,
            Self::AllowPath => EntryList::AllowPaths,
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

/// One entry [`validate`] accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// The entry as it will be written: trimmed, and a rule in the form the
    /// loader reads back.
    pub text: String,
    /// The loader's notice for an entry that loads but matches no call (a
    /// Claude Code tool kGraf has no tool for), as [`validate_entry`] gives it.
    pub notice: Option<String>,
}

/// Check one entry the way the file loader does ([`validate_entry`]), and give
/// the entry as it will be written (trimmed) with the loader's notice, if any.
/// A refused entry gets the loader's message. An empty entry is refused: it
/// would add nothing.
pub fn validate(kind: RuleKind, text: &str) -> Result<Checked, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err(format!("the {} entry is empty", kind.list().key()));
    }
    let valid = validate_entry(kind.list(), t)?;
    let text = match valid.entry {
        Entry::Deny(rule) | Entry::Ask(rule) => rule.text(),
        Entry::Tool(_) | Entry::Program(_) | Entry::Path(_) => t.to_owned(),
    };
    Ok(Checked {
        text,
        notice: valid.notice,
    })
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
    let entry = validate(kind, entry)?.text;
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

    /// The editor's check is the loader's: the same refusal message, the
    /// same notice for a Claude Code tool kGraf lacks, and the text written
    /// is what the loader reads back.
    #[test]
    fn an_entry_is_checked_with_the_loaders_messages() {
        for (kind, text) in [
            (RuleKind::Deny, "WebFech"),
            (RuleKind::Ask, "Bah(rm*)"),
            (RuleKind::AllowProgram, "two words"),
            (RuleKind::AllowTool, "fetch"),
        ] {
            assert_eq!(
                validate(kind, text).unwrap_err(),
                validate_entry(kind.list(), text).unwrap_err(),
                "{kind:?} {text}"
            );
        }
        let web = validate(RuleKind::Deny, "WebFetch").expect("loads");
        assert_eq!(web.text, "WebFetch");
        let from_loader = validate_entry(EntryList::Deny, "WebFetch")
            .expect("loads")
            .notice
            .expect("a notice");
        assert_eq!(web.notice.as_deref(), Some(from_loader.as_str()));
        assert!(
            from_loader.contains("kGraf has no WebFetch tool"),
            "{from_loader}"
        );
        let ask = validate(RuleKind::Ask, "  Bash(rm *) ").unwrap();
        assert_eq!(ask.text, "Bash(rm *)");
        assert_eq!(ask.notice, None);
        assert_eq!(
            validate(RuleKind::AllowProgram, " make ").unwrap().text,
            "make"
        );
        assert_eq!(validate(RuleKind::AllowTool, "Bash").unwrap().text, "Bash");
        assert_eq!(
            validate(RuleKind::AllowPath, " docs/* ").unwrap().text,
            "docs/*"
        );
        assert!(validate(RuleKind::AllowPath, " ").is_err());
        assert!(validate(RuleKind::Deny, "").is_err());
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
