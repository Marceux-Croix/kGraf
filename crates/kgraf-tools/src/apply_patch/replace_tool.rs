//! The `replace_in_file` tool: replace one exact piece of text in a file.
//!
//! `apply_patch` needs every line prefixed (` `, `+`, `-`) and its context
//! lines copied exactly, and a model that gets either wrong fails and tries
//! again. This tool takes the text to replace and the text to put there, with
//! no line prefixes and no context lines.
//!
//! **One write path.** The new file contents are computed here and written
//! through [`apply_all_names`], the function `apply_patch` writes with, as an
//! Add File of the whole new contents. So the workspace confinement, the
//! protected-name guard, the hard-link refusal and the write itself are the
//! same code, not a copy. The same declarations back it: `access()` declares
//! the one written path, which the worker's write scope, its fences and the
//! permission rules judge as they judge a patch's headers.
//!
//! **Protected paths.** `.git`, kGraf's own state and the files the Bypass mode
//! admits to `apply_patch` are refused here, with a pointer to `apply_patch`:
//! that tool snapshots and checks such a write, and one place for it is
//! enough.
//!
//! **Syntax check.** The check `apply_patch` runs after a whitespace-tolerant
//! match ([`super::syntax`]) runs here on every edit. This tool matches text
//! exactly, so there is no tolerant match to trigger it, and a piece of text
//! replaced without its context is the edit most likely to break indentation.

use super::{Hunk, MAX_PATCH_BYTES, apply_all_names, seek_sequence, summary, syntax};
use crate::fs_safety::{self, MAX_READ_BYTES};
use crate::protected::ProtectedWrites;
use crate::{Access, CallPermit, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The tool's wire name.
pub const REPLACE_IN_FILE: &str = "replace_in_file";

/// Longest list of line numbers named for a text that occurs more than once.
const MAX_LINES_NAMED: usize = 8;

const DESCRIPTION: &str = concat!(
    "Replace one exact piece of text in a file. Give `path`, `old` (the text now in the ",
    "file, copied exactly, with its indentation and line breaks) and `new` (what to put ",
    "there). `old` must occur exactly once in the file; if the tool says it occurs more ",
    "than once, add a neighboring line to `old` and to `new`. There are no line prefixes ",
    "and no context lines. Use it for a small edit. Use apply_patch for several edits to ",
    "one file at once, to delete or move a file, or to add a file with many lines. An ",
    "empty `old` creates the file `path` with `new` as its contents, if it does not ",
    "exist.",
);

/// The `replace_in_file` tool. Stateless; the optional guard only decides which
/// paths are refused with a pointer to `apply_patch`.
#[derive(Debug, Default, Clone)]
pub struct ReplaceInFileTool {
    guard: Option<Arc<ProtectedWrites>>,
}

impl ReplaceInFileTool {
    /// This tool, knowing the attempt's protected-write guard, so a path the
    /// guard would admit to `apply_patch` is refused here with a pointer to it.
    pub fn with_protected_writes(guard: Arc<ProtectedWrites>) -> Self {
        Self { guard: Some(guard) }
    }

    /// Refuses a path that is protected, in every mode.
    fn check_path(&self, path: &Path) -> Result<(), ToolError> {
        let refuse = |why: &str| ToolError::Access {
            tool: REPLACE_IN_FILE.into(),
            source: crate::AccessError::Escapes {
                path: format!("{}: {why}", path.display()),
            },
        };
        match self.guard.as_ref().and_then(|g| g.admit(path)) {
            None => fs_safety::refuses_git_or_kgraf(REPLACE_IN_FILE, path),
            Some(Err(why)) => Err(refuse(&why)),
            Some(Ok(_)) => Err(refuse(
                "a protected path is written with apply_patch, which snapshots and checks it",
            )),
        }
    }
}

/// A call's input, with the path normalized.
struct Input {
    path: PathBuf,
    old: String,
    new: String,
}

fn invalid(reason: impl Into<String>) -> ToolError {
    ToolError::InvalidInput {
        tool: REPLACE_IN_FILE.into(),
        reason: reason.into(),
    }
}

fn internal(reason: impl Into<String>) -> ToolError {
    ToolError::Internal {
        tool: REPLACE_IN_FILE.into(),
        reason: reason.into(),
    }
}

fn parse_input(input: &Value) -> Result<Input, ToolError> {
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(format!("expected a string `{key}` field")))
    };
    let (path, old, new) = (text("path")?, text("old")?, text("new")?);
    for (key, value) in [("old", old), ("new", new)] {
        if value.len() > MAX_PATCH_BYTES {
            return Err(invalid(format!(
                "`{key}` is {} bytes, over the {MAX_PATCH_BYTES}-byte cap",
                value.len()
            )));
        }
    }
    let normalized =
        crate::normalize_relative(Path::new(path)).map_err(|source| ToolError::Access {
            tool: REPLACE_IN_FILE.into(),
            source,
        })?;
    if normalized.as_os_str().is_empty() {
        return Err(invalid(format!(
            "`path` {path:?} names the workspace root; give a file"
        )));
    }
    Ok(Input {
        path: normalized,
        old: old.to_owned(),
        new: new.to_owned(),
    })
}

/// Where `needle` occurs in `text`, overlapping occurrences included: the
/// count and the byte offset of each of the first few.
fn occurrences(text: &str, needle: &str) -> (usize, Vec<usize>) {
    let mut count = 0;
    let mut first = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(needle) {
        let at = from + at;
        count += 1;
        if first.len() < MAX_LINES_NAMED {
            first.push(at);
        }
        // Step one character, so a text that overlaps itself counts twice.
        from = at + text[at..].chars().next().map_or(1, char::len_utf8);
    }
    (count, first)
}

/// 1-based line of byte offset `at` in `text`.
fn line_of(text: &str, at: usize) -> usize {
    text[..at].matches('\n').count() + 1
}

/// The file with its one occurrence of `old` replaced by `new`, and a note when
/// the match needed line endings changed. `Err` is the text the model reads.
fn replaced(
    original: &str,
    path: &str,
    old: &str,
    new: &str,
) -> Result<(String, Option<&'static str>), String> {
    if old == new {
        return Err("`old` and `new` are the same text, so nothing would change".to_owned());
    }
    // A model writes line breaks as `\n`. In a file with CRLF line endings
    // the same lines are found by writing them with CRLF.
    let crlf = |t: &str| t.replace('\n', "\r\n");
    let mut tries = vec![(old.to_owned(), new.to_owned(), None)];
    if original.contains("\r\n") && old.contains('\n') && !old.contains('\r') {
        tries.push((
            crlf(old),
            crlf(new),
            Some(
                "note: the file has CRLF line endings; `old` and `new` were matched and written \
                 with CRLF",
            ),
        ));
    }
    for (old, new, note) in tries {
        let (count, at) = occurrences(original, &old);
        match count {
            0 => continue,
            1 => {
                let start = at[0];
                let mut out = String::with_capacity(original.len() - old.len() + new.len());
                out.push_str(&original[..start]);
                out.push_str(&new);
                out.push_str(&original[start + old.len()..]);
                return Ok((out, note));
            }
            n => {
                let lines: Vec<String> = at
                    .iter()
                    .map(|&a| line_of(original, a).to_string())
                    .collect();
                let more = if n > at.len() { ", ..." } else { "" };
                return Err(format!(
                    "`old` occurs {n} times in {path} (at lines {}{more}). It must occur exactly \
                     once. Add the neighboring lines to `old` (and to `new`) so that it names \
                     one place.",
                    lines.join(", ")
                ));
            }
        }
    }
    Err(format!(
        "`old` occurs 0 times in {path}; nothing was changed. {}",
        near_match(original, old)
    ))
}

/// What in the file is nearest to `old`, for an error that says it was not
/// found. Built on the window scoring `apply_patch` uses for its own hints.
fn near_match(original: &str, old: &str) -> String {
    let lines: Vec<String> = original.lines().map(str::to_owned).collect();
    let pattern: Vec<String> = old.lines().map(str::to_owned).collect();
    if pattern.is_empty() || lines.is_empty() {
        return String::new();
    }
    let clip = seek_sequence::clip;
    let Some((start, score)) = seek_sequence::best_candidate_window(&lines, &pattern, 0) else {
        return format!(
            "`old` has {} lines and the file has {}, so it cannot fit.",
            pattern.len(),
            lines.len()
        );
    };
    if score == pattern.len() {
        return format!(
            "Line {} of the file matches `old` except for whitespace (indentation or trailing \
             spaces). Copy the file's text exactly: '{}'.",
            start + 1,
            clip(&lines[start]),
        );
    }
    if score == 0 {
        if pattern.len() == 1 {
            let needle = pattern[0].trim();
            if let Some((i, line)) = lines
                .iter()
                .enumerate()
                .find(|(_, l)| !needle.is_empty() && l.contains(needle))
            {
                return format!(
                    "Line {} holds the text of `old` without the whitespace around it that \
                     `old` has: '{}'.",
                    i + 1,
                    clip(line)
                );
            }
        }
        return format!(
            "No {}-line region of the file resembles `old`. Read the file again.",
            pattern.len()
        );
    }
    let k = (0..pattern.len())
        .find(|&k| lines[start + k].trim() != pattern[k].trim())
        .unwrap_or(0);
    format!(
        "Nearest candidate is line {} ({score}/{} lines of `old` match there, ignoring \
         whitespace); first disagreement is line {} of `old`: expected '{}', found '{}'.",
        start + 1,
        pattern.len(),
        k + 1,
        clip(&pattern[k]),
        clip(&lines[start + k]),
    )
}

/// What a call changed: the file's new contents, whether the file is new, and
/// a note.
struct Change {
    contents: String,
    created: bool,
    notes: Vec<String>,
}

/// Read the file, compute its new contents, run the syntax check.
fn plan(dir: &cap_std::fs::Dir, path: &Path, old: &str, new: &str) -> Result<Change, ToolError> {
    let shown = path.display().to_string();
    let exists = fs_safety::metadata_if_exists(REPLACE_IN_FILE, dir, path)?.is_some();
    if old.is_empty() {
        if exists {
            return Err(invalid(format!(
                "`old` is empty but {shown} exists. Give the text to replace as `old`. To \
                 replace the whole file, use apply_patch with a Delete File and an Add File \
                 of the same path."
            )));
        }
        return Ok(Change {
            contents: new.to_owned(),
            created: true,
            notes: Vec::new(),
        });
    }
    if !exists {
        return Err(invalid(format!(
            "{shown} does not exist. To create it, call this tool with an empty `old`; or use \
             apply_patch with an Add File."
        )));
    }
    let original =
        fs_safety::read_capped_to_string(REPLACE_IN_FILE, dir, path, MAX_READ_BYTES, false)?;
    let (contents, note) = replaced(&original, &shown, old, new).map_err(internal)?;
    if let Some((kind, line)) =
        syntax::newly_broken(&shown, &original, &contents).map_err(internal)?
    {
        return Err(internal(format!(
            "In {shown}, the edit would leave the file as invalid {kind} (first error at line \
             {line}), so it was not written. Check the indentation of `new`, and make the whole \
             change in one edit."
        )));
    }
    Ok(Change {
        contents,
        created: false,
        notes: note.map(str::to_owned).into_iter().collect(),
    })
}

#[async_trait::async_trait]
impl Tool for ReplaceInFileTool {
    fn name(&self) -> &str {
        REPLACE_IN_FILE
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "replace_in_file".into(),
            description: DESCRIPTION.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The file, relative to the workspace root.",
                    },
                    "old": {
                        "type": "string",
                        "description": "The text now in the file, copied exactly. It must \
                            occur once. Empty to create the file.",
                    },
                    "new": {
                        "type": "string",
                        "description": "The text that replaces `old`.",
                    },
                },
                "required": ["path", "old", "new"],
                "additionalProperties": false,
            }),
        }
    }

    /// As for `apply_patch`: the declared path is the gate, and the call has no
    /// command line a prefix rule could name.
    fn permission_subject(&self, _input: &Value) -> Option<String> {
        None
    }

    /// Declares the one path written, as `apply_patch` declares a hunk's path.
    /// The check is lexical; the symlink-resistant one runs when the call writes.
    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        let Input { path, .. } = parse_input(input)?;
        self.check_path(&path)?;
        Access::none()
            .write_path(&path)
            .map_err(|source| ToolError::Access {
                tool: REPLACE_IN_FILE.into(),
                source,
            })
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled {
                tool: REPLACE_IN_FILE.into(),
            });
        }
        let Input { path, old, new } = parse_input(&input)?;
        self.check_path(&path)?;
        let root = ctx.workspace_root.clone();
        let cancel = ctx.cancel.clone();
        let task = tokio::task::spawn_blocking(move || {
            let dir = fs_safety::open_workspace(REPLACE_IN_FILE, &root)?;
            // The write guard runs before the read, as it does for a patch.
            fs_safety::guard_write_names(REPLACE_IN_FILE, &dir, &path, fs_safety::Names::Plain)?;
            let change = plan(&dir, &path, &old, &new)?;
            let hunk = Hunk::AddFile {
                path: path.clone(),
                contents: change.contents,
            };
            let applied = apply_all_names(&[hunk], &root, &cancel, fs_safety::Names::Plain)?;
            let shown = [path.to_string_lossy().into_owned()];
            let text = if change.created {
                summary(&shown, &[], &[])
            } else {
                summary(&[], &shown, &[])
            };
            let mut out = ToolOutput::text(text);
            for note in change.notes.into_iter().chain(applied.notes) {
                out.parts.push(crate::OutputPart::Text {
                    text: crate::UntrustedText::new(note),
                });
            }
            Ok(out)
        });
        tokio::select! {
            _ = ctx.cancel.cancelled() => Err(ToolError::Cancelled { tool: REPLACE_IN_FILE.into() }),
            result = task => result.map_err(|e| internal(format!("edit task failed: {e}")))?,
        }
    }
}
