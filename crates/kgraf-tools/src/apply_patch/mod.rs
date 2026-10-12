//! The file-edit tool: codex-rs `apply_patch`'s patch format, ported (0055,
//! third_party/codex/UPSTREAM).
//!
//! **What was ported and what was not.** codex-rs's own patch application walks
//! an `ExecutorFileSystem` trait object through a sandbox context and resolves
//! paths as `PathUri`s, both built for codex's own executor process. kGraf
//! already has a filesystem seam of its own: [`crate::Access`] for lexical
//! escape checks before approval, and [`crate::fs_safety`] for symlink-resistant
//! reads and writes at execution time. Vendoring codex's I/O layer on top would
//! mean two path-safety mechanisms disagreeing about what "the workspace" means,
//! so only the pure, filesystem-free parts were ported:
//!
//! - [`parser`] and [`streaming_parser`]: the `*** Begin Patch` grammar, turning
//!   patch text into [`parser::Hunk`]s.
//! - [`seek_sequence`] and [`replace`]: the fuzzy context-matching and
//!   line-replacement logic that turns an [`parser::UpdateFileChunk`] plus a
//!   file's current contents into its new contents.
//!
//! Applying a [`parser::Hunk`] to the filesystem, and declaring the paths it
//! touches, is kGraf's own, built on the seams above.
//!
//! **Validate-then-commit.** A patch's hunks are
//! all planned, parsed, path-checked, and (for an update) matched against the
//! file's current content, before any hunk is written. A patch whose third
//! hunk fails to match no longer leaves the first two hunks' writes on disk
//! with no way to tell the caller only part of the patch applied.
//!
//! **Off the async executor.** Parsing and fuzzy-matching a large or
//! adversarial patch is synchronous CPU work with no natural yield point; it
//! runs in [`tokio::task::spawn_blocking`] rather than on the worker's async
//! task, and the call returns as soon as the worker is cancelled rather than
//! waiting for that blocking work to finish (it cannot be interrupted
//! mid-computation, only raced: the call returns, the spawned work finishes on
//! its own thread and its result is discarded).

#[cfg(test)]
mod git_files_tests;
pub(crate) mod parser;
mod protected;
mod reindent;
mod replace;
mod replace_tool;
#[cfg(test)]
mod replace_tool_tests;
mod seek_sequence;
mod streaming_parser;
mod syntax;

pub use parser::parse_patch;
pub use parser::{Hunk, ParseError, UpdateFileChunk};
pub use protected::ProtectedApplyPatch;
pub use replace_tool::{REPLACE_IN_FILE, ReplaceInFileTool};

use crate::fs_safety::{self, MAX_READ_BYTES};
use crate::{Access, CallPermit, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use cap_std::fs::Dir;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// The `apply_patch` tool. Stateless: every call is self-contained.
#[derive(Debug, Default, Clone, Copy)]
pub struct ApplyPatchTool;

/// [`ApplyPatchTool::definition`]'s `description`, pulled out to a constant so
/// [`tests::the_description_s_own_examples_parse`] can hold it to its word: the
/// worked examples below must themselves be valid patches, or the tool would be
/// teaching a model a format it then refuses.
///
/// **Root-cause fix (Haiku failures).** Before
/// this, the description was one line -- "Edit, add, delete or move files with
/// a `*** Begin Patch` document (codex apply_patch format)" -- and named no
/// rule of the grammar at all, banking on every model already knowing OpenAI's
/// Codex CLI patch dialect cold. `gpt-6-luna` (OpenAI's own lineage) and
/// `kimi-k3` (evidently trained on the same widely-copied convention) do;
/// `claude-haiku-4-5-20251001` measurably does not: without the grammar it
/// writes a unified-diff-style `--- a/file`/`+++ b/file` patch, then a
/// `*** Update File:` header with unprefixed body lines, each rejected by
/// [`parser::parse_patch`]'s own error text one rule at a time, spending the
/// whole step budget on trial and error with nothing committed. This is a
/// tool-description gap, not a wire-protocol or worker-loop defect: every
/// route sends the same `description` string verbatim, so teaching the grammar
/// once here reaches every model through its own tool definitions rather than
/// through a model-specific branch.
// Built with `concat!` rather than a single `\`-continued literal: the worked
// example below needs meaningful leading spaces (a context line's ` ` prefix)
// on some lines, and Rust's `\`-newline continuation eats *all* leading
// whitespace on the next line, indentation and meaning alike -- exactly the
// off-by-invisible-whitespace bug this fix exists to stop a model from making.
// One string literal per line, joined by explicit `"\n"`s, sidesteps it.
const APPLY_PATCH_DESCRIPTION: &str = concat!(
    "Edit, add, delete or move files with a `*** Begin Patch` / `*** End Patch` ",
    "document. One or more hunks, each starting its own `*** Add File: <path>`, ",
    "`*** Delete File: <path>` or `*** Update File: <path>` line (paths are ",
    "relative to the workspace root); an Update hunk may add a `*** Move to: ",
    "<new path>` line right after it to rename while editing. Exact rules, since ",
    "getting this wrong burns your whole step budget on rejected patches:\n",
    "- An Add File hunk's body lines are the new file's contents, each prefixed ",
    "with a literal `+` (even a blank line: just `+`).\n",
    "- An Update File hunk's body lines are prefixed with one of three characters: ",
    "a literal space for an unchanged context line copied from the file, `-` for a ",
    "line the file currently has that is being removed, or `+` for a new line ",
    "being added. Give every body line one of these three prefixes; a bare line ",
    "with no prefix is read as a context line and must match the file. Include a few ",
    "unchanged context lines around each change so the matcher can place it.\n",
    "- A Delete File hunk has no body lines: just the header.\n",
    "- Write the `*** Begin Patch` and `*** End Patch` lines. A patch that starts ",
    "with a file header and has no `*** Begin Patch` line is applied as if it had ",
    "one, and the result says so; any other malformed patch is refused.\n",
    "- Never use unified-diff syntax (`--- a/file`, `+++ b/file`, `@@ -1,3 +1,3 @@` ",
    "with line numbers, `/dev/null`); this is not that format.\n",
    "\n",
    "Worked example, updating one file and adding another in the same patch:\n",
    "```\n",
    "*** Begin Patch\n",
    "*** Update File: src/lib.rs\n",
    "@@ fn greet\n",
    " pub fn greet(name: &str) -> String {\n",
    "-    format!(\"hi {name}\")\n",
    "+    format!(\"hello, {name}!\")\n",
    " }\n",
    "*** Add File: NOTES.md\n",
    "+First line of the new file.\n",
    "+Second line.\n",
    "*** End Patch\n",
    "```",
);

/// The file a rejected `apply_patch` call aimed at, for [`rejection_help`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchTarget {
    pub path: String,
    /// Whether the call's first file header was `*** Add File:`.
    pub adds: bool,
}

/// The first file a call's patch names (`*** Add File:`, `*** Update File:`
/// or `*** Delete File:`), read leniently from the text: the call may be the
/// malformed one, so this does not parse it. `None` when it names none.
pub fn patch_target(input: &Value) -> Option<PatchTarget> {
    let patch = input
        .get("patch")
        .or_else(|| input.get("input"))
        .and_then(Value::as_str)?;
    patch.lines().map(str::trim).find_map(|line| {
        [
            ("*** Add File:", true),
            ("*** Update File:", false),
            ("*** Delete File:", false),
        ]
        .iter()
        .find_map(|(head, adds)| {
            let path = line.strip_prefix(head)?.trim();
            (!path.is_empty()).then(|| PatchTarget {
                path: path.to_owned(),
                adds: *adds,
            })
        })
    })
}

/// What to tell a model whose `apply_patch` call was rejected more than once:
/// the one rule the `error` shows it broke, and a minimal valid patch for the
/// file the call targeted. `None` when the error is not one of the format or
/// matching errors (a protected path, an escaping path), where an example
/// would only mislead.
pub fn rejection_help(error: &str, target: Option<&PatchTarget>) -> Option<String> {
    let rule = if error.contains("Begin Patch") {
        "The first line of the patch must be exactly `*** Begin Patch`."
    } else if error.contains("End Patch") {
        "The last line of the patch must be exactly `*** End Patch`."
    } else if error.contains("invalid hunk") {
        "Each file section starts with `*** Add File: <path>`, `*** Update File: <path>` or \
         `*** Delete File: <path>`. In an Add File section every line starts with `+`. In an \
         Update File section every line starts with a space (an unchanged line), `-` (a line \
         to remove) or `+` (a line to add)."
    } else if error.contains("Failed to find") {
        "A context or `-` line is not in the file as it is now. Read the file again and copy \
         those lines exactly, including indentation."
    } else if error.contains("invalid patch") {
        "A patch is `*** Begin Patch`, one or more file sections, then `*** End Patch`."
    } else {
        return None;
    };
    let path = target.map_or("path/to/file", |t| t.path.as_str());
    let example = if target.is_some_and(|t| t.adds) {
        format!("*** Begin Patch\n*** Add File: {path}\n+first line of the new file\n*** End Patch")
    } else {
        format!(
            "*** Begin Patch\n*** Update File: {path}\n@@\n an unchanged line\n-the line to \
             remove\n+the line to add\n*** End Patch"
        )
    };
    Some(format!(
        "\n{rule} A minimal valid patch for this file:\n{example}"
    ))
}

/// Consecutive failed `apply_patch` calls on one file after which an error
/// for that file offers the whole-file route ([`whole_file_hint`]).
pub const WHOLE_FILE_HINT_AFTER: u32 = 3;

/// One sentence for a model whose patches on `path` keep failing: replacing
/// the whole file needs no context or `-` line to match. The tool applies a
/// Delete File and an Add File of one path as a replace. With
/// `replace_in_file` (the worker has that tool), a small edit is pointed at it
/// first: it takes the old and the new text, with no line prefixes.
pub fn whole_file_hint(path: &str, replace_in_file: bool) -> String {
    let small = if replace_in_file {
        format!(
            "\nFor a small edit, call replace_in_file with `path` {path:?}, `old` (the exact \
             text now in the file) and `new`; it needs no line prefixes or context lines."
        )
    } else {
        String::new()
    };
    format!(
        "\nThis file has failed to patch repeatedly. You can replace the whole file in one \
         patch: `*** Delete File: {path}` followed by `*** Add File: {path}` with the full new \
         content, every line starting with `+` (a body with no `+` at all is read as plain text):\n{}{small}",
        whole_file_skeleton(path, true)
    )
}

/// A patch that writes `path` as a whole file: a Delete File first when the
/// file exists (the tool applies the pair as a replace), then an Add File.
pub fn whole_file_skeleton(path: &str, replace: bool) -> String {
    let delete = if replace {
        format!("*** Delete File: {path}\n")
    } else {
        String::new()
    };
    format!("*** Begin Patch\n{delete}*** Add File: {path}\n+full new content\n*** End Patch")
}

/// A patch longer than this is refused before it is even parsed (cap
/// patch size). Generous for a real multi-file change; a patch this large is
/// already an unusual model output worth refusing rather than absorbing.
const MAX_PATCH_BYTES: usize = 2 * 1024 * 1024;
/// A patch naming more hunks than this is refused: a circuit breaker against
/// an adversarial patch built from many small hunks, each cheap to parse but
/// expensive in aggregate to match.
const MAX_HUNKS: usize = 2_000;

fn patch_text(input: &Value) -> Result<&str, ToolError> {
    let patch =
        input
            .get("patch")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput {
                tool: "apply_patch".into(),
                reason: "expected a string `patch` field".into(),
            })?;
    if patch.len() > MAX_PATCH_BYTES {
        return Err(ToolError::InvalidInput {
            tool: "apply_patch".into(),
            reason: format!(
                "patch is {} bytes, over the {MAX_PATCH_BYTES}-byte cap",
                patch.len()
            ),
        });
    }
    Ok(patch)
}

const BEGIN_NOTE: &str =
    "note: the patch had no '*** Begin Patch' line; it was applied as if it had";
const END_NOTE: &str = "note: the patch had no '*** End Patch' line (or a malformed one); it was \
     applied as if it ended there, so check that no lines were cut off";
const UNIFIED_NOTE: &str = "note: an `@@ -a,b +c,d @@` header is a unified-diff header, which this \
     format does not use; its line numbers were ignored and the text after it, if any, was taken as \
     the `@@` context line";

const GLUED_NOTE: &str = "note: an `@@` marker written with no space before its text (`@@text`) \
     was read as `@@ text`, the `@@` context line";

const RAW_ADD_NOTE: &str = "note: an Add File section had no line starting with '+', so every \
     line of it was read as content of the new file, as written; write a '+' before each line \
     of the new file";

const NO_NEWLINE_NOTE: &str = "note: a `\\ No newline at end of file` line is a git diff marker, \
     not part of this format; it was ignored unless it follows the last '+' or '-' line of a \
     hunk, where it sets the file's final-newline state";

const BLANK_ADD_NOTE: &str = "note: an Add File section had a blank line with no '+' between \
     lines that start with '+'; it was read as an empty line of the new file";

const BARE_CONTEXT_NOTE: &str = "note: an Update File section had a line with no ' ', '+' or '-' \
     prefix; it was read as an unchanged context line, so it must match the file exactly";

/// The git diff marker for a last line with no newline.
const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file";

/// A patch repaired into the form the parser takes, and a note per repair.
/// Models send the hunks without the envelope, drop or mangle the end line,
/// or head a chunk with a unified-diff `@@ -a,b +c,d @@` line. Each repair is
/// one whose intent is certain, and each is stated in the call's result.
/// Anything else goes to the parser unchanged and is refused there.
///
/// - A first non-blank line that is a file header gets `*** Begin Patch`.
/// - A patch that begins with `*** Begin Patch` (or was given one) and whose
///   last line is not `*** End Patch` gets one; a last line that is only a
///   mangled end marker (`***/ End Patch`) is replaced by it.
/// - `@@text` (no space; the text does not start with `@`) in an Update File
///   section becomes `@@ text`. A body line never starts with `@`, so this
///   reads nothing else; `@@@ x` and other `@@@` forms are left for the parser
///   to refuse. A leading `-` or `+` on the text is dealt with when the
///   context is matched against the file, where the file can decide.
/// - `@@ -a,b +c,d @@ text` in an Update File section becomes `@@ text`
///   (`@@` when there is no text).
/// - An Add File section with no body line starting with `+` holds raw text:
///   every line up to the next line starting with `***` is content. Blank
///   lines inside are blank lines of the file; blank lines just before the
///   next marker are separators and are dropped. A body line that itself
///   starts with `***` cannot be raw content: it ends the section, and the
///   parser refuses it as a bad header. A section with any `+` line is left
///   alone and refused if it also has a bare line.
/// - A line that is exactly git's `\ No newline at end of file` is read, not
///   kept as content. After the last `+` line of an Add File section it makes
///   the file end with no newline. After the last `+` line of an Update File
///   chunk it makes the result end with no newline; after the last `-` line,
///   with no such marker after the last `+` line, it makes the result end with
///   one. Either holds only when the chunk ends at the end of the file. Any
///   other marker is dropped with a note. A body line that carries the text as
///   content starts with `+`, `-` or a space, so no real line is lost.
/// - In an Add File section whose non-blank lines all start with `+`, a blank
///   line between the first and the last `+` line is an empty line of the
///   file. A blank line outside that span, and any non-blank line without a
///   `+`, stay refused.
/// - In an Update File chunk (the lines between `@@` markers) that has no
///   line starting with a space, a line with no ` `, `+` or `-` prefix is a
///   context line, written with its prefix left out. It is matched against
///   the file as any context line is, so a wrong reading fails there. A chunk
///   that mixes spaced and bare lines is left alone: a bare line there is
///   likely a lost `-` or `+`, and the parser refuses it. A line
///   starting with `@@` or `***` is not read this way: those are markers, and
///   a malformed one is refused by the parser.
fn repaired(patch: &str) -> (String, Vec<&'static str>, Vec<FinalNewline>) {
    let mut notes = Vec::new();
    let first = patch.lines().map(str::trim).find(|l| !l.is_empty());
    let headed = first.is_some_and(|l| {
        ["*** Add File:", "*** Update File:", "*** Delete File:"]
            .iter()
            .any(|h| l.starts_with(h))
    });
    let begun = first == Some(parser::BEGIN_PATCH_MARKER);
    if !headed && !begun {
        return (patch.to_string(), notes, Vec::new());
    }
    // Blank lines are dropped from both ends, but no line is trimmed: with no
    // end marker the last line is a body line, and trimming the whole patch
    // cut the trailing whitespace off that line's text.
    let mut lines: Vec<String> = patch.lines().map(str::to_string).collect();
    let keep_from = lines
        .iter()
        .position(|l| !l.trim().is_empty())
        .unwrap_or(lines.len());
    lines.drain(..keep_from);
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    // The first line is a marker or a file header (checked above), so its
    // leading whitespace is not content.
    if let Some(head) = lines.first_mut() {
        *head = head.trim_start().to_string();
    }
    if headed {
        lines.insert(0, parser::BEGIN_PATCH_MARKER.to_string());
        notes.push(BEGIN_NOTE);
    }
    let last = lines.last().map(|l| l.trim().to_string());
    match last.as_deref() {
        Some(parser::END_PATCH_MARKER) if lines.len() > 1 => {}
        Some(l) if l.starts_with('*') && l.ends_with("End Patch") && lines.len() > 1 => {
            if let Some(slot) = lines.last_mut() {
                *slot = parser::END_PATCH_MARKER.to_string();
            }
            notes.push(END_NOTE);
        }
        _ => {
            lines.push(parser::END_PATCH_MARKER.to_string());
            notes.push(END_NOTE);
        }
    }
    let marked = scan_sections(&lines);
    lines.retain(|l| l.trim_end() != NO_NEWLINE_MARKER);
    if marked.stray_marker {
        notes.push(NO_NEWLINE_NOTE);
    }
    if prefix_raw_add_bodies(&mut lines) {
        notes.push(RAW_ADD_NOTE);
    }
    if plus_blank_add_lines(&mut lines) {
        notes.push(BLANK_ADD_NOTE);
    }
    // Chunk membership is read again here: the repairs above add and drop
    // lines, and the marker lines are gone.
    let sections = scan_sections(&lines);
    let mut in_update = false;
    let mut bare_context = false;
    for (i, line) in lines.iter_mut().enumerate() {
        let t = line.trim_end();
        if t.starts_with(parser::MOVE_TO_MARKER) || t == parser::EOF_MARKER {
            // Both stand inside an Update File section.
        } else if t.starts_with("*** ") {
            in_update = t.starts_with(parser::UPDATE_FILE_MARKER);
        } else if in_update
            && is_bare_body_line(line)
            && sections.chunk_of[i].is_some_and(|c| !sections.chunks[c].spaced)
        {
            line.insert(0, ' ');
            bare_context = true;
        } else if in_update && let Some(glued) = glued_marker_text(t) {
            let spaced = format!("@@ {glued}");
            *line = match unified_header_context(&spaced) {
                Some("") => "@@".to_string(),
                Some(context) => format!("@@ {context}"),
                None => spaced,
            };
            if !notes.contains(&GLUED_NOTE) {
                notes.push(GLUED_NOTE);
            }
        } else if in_update && let Some(context) = unified_header_context(t) {
            *line = if context.is_empty() {
                "@@".to_string()
            } else {
                format!("@@ {context}")
            };
            if !notes.contains(&UNIFIED_NOTE) {
                notes.push(UNIFIED_NOTE);
            }
        }
    }
    if bare_context {
        notes.push(BARE_CONTEXT_NOTE);
    }
    (lines.join("\n"), notes, marked.requests)
}

/// A final-newline state asked for by a `\ No newline at end of file` line,
/// for the hunk at index `hunk` of the patch (and its chunk `chunk`, for an
/// Update File).
#[derive(Debug, PartialEq)]
struct FinalNewline {
    hunk: usize,
    chunk: Option<usize>,
    newline: bool,
}

/// One chunk of an Update File section, as [`scan_sections`] reads it.
#[derive(Default)]
struct ChunkScan {
    hunk: usize,
    index: usize,
    /// Whether a body line starts with a space.
    spaced: bool,
    /// A marker line follows the chunk's last `+` line.
    after_plus: bool,
    /// A marker line follows the chunk's last `-` line.
    after_minus: bool,
}

#[derive(Default)]
struct Scan {
    /// For each line, the index in `chunks` of the Update chunk it belongs to.
    chunk_of: Vec<Option<usize>>,
    chunks: Vec<ChunkScan>,
    requests: Vec<FinalNewline>,
    /// A marker line that asks for nothing: it follows no `+` line of an Add
    /// File and no `+` or `-` line of an Update File chunk.
    stray_marker: bool,
}

/// Reads the sections of a patch the way the parser will: which chunk each
/// Update line is in (a chunk starts at an `@@` line, or at the first body
/// line when none came before), and where a `\ No newline at end of file`
/// line asks for a final-newline state.
fn scan_sections(lines: &[String]) -> Scan {
    #[derive(PartialEq)]
    enum Section {
        Other,
        Add,
        Update,
    }
    let mut scan = Scan::default();
    let mut section = Section::Other;
    let mut hunks = 0usize;
    let mut add_after_plus = false;
    let mut chunk: Option<usize> = None;
    let mut chunks_in_hunk = 0usize;
    let close_add = |scan: &mut Scan, hunk: usize, after_plus: &mut bool| {
        if std::mem::take(after_plus) {
            scan.requests.push(FinalNewline {
                hunk,
                chunk: None,
                newline: false,
            });
        }
    };
    for (i, line) in lines.iter().enumerate() {
        scan.chunk_of.push(None);
        let t = line.trim_end();
        if t.starts_with(parser::MOVE_TO_MARKER) || t == parser::EOF_MARKER {
            continue;
        }
        if t.starts_with("*** ") {
            if section == Section::Add {
                close_add(&mut scan, hunks - 1, &mut add_after_plus);
            }
            section = if t.starts_with(parser::ADD_FILE_MARKER) {
                Section::Add
            } else if t.starts_with(parser::UPDATE_FILE_MARKER) {
                Section::Update
            } else {
                Section::Other
            };
            if section != Section::Other || t.starts_with(parser::DELETE_FILE_MARKER) {
                hunks += 1;
            }
            chunk = None;
            chunks_in_hunk = 0;
            continue;
        }
        let prev = i.checked_sub(1).map(|p| lines[p].as_str());
        if t == NO_NEWLINE_MARKER {
            match (&section, chunk, prev) {
                (Section::Add, _, Some(p)) if p.starts_with('+') => add_after_plus = true,
                (Section::Update, Some(c), Some(p)) if p.starts_with('+') => {
                    scan.chunks[c].after_plus = true;
                }
                (Section::Update, Some(c), Some(p)) if p.starts_with('-') => {
                    scan.chunks[c].after_minus = true;
                }
                _ => scan.stray_marker = true,
            }
            continue;
        }
        match section {
            Section::Add => {
                if line.starts_with('+') {
                    add_after_plus = false;
                }
            }
            Section::Update => {
                if chunk.is_none() || line.starts_with("@@") {
                    scan.chunks.push(ChunkScan {
                        hunk: hunks - 1,
                        index: chunks_in_hunk,
                        ..ChunkScan::default()
                    });
                    chunk = Some(scan.chunks.len() - 1);
                    chunks_in_hunk += 1;
                }
                let Some(c) = chunk else { continue };
                scan.chunk_of[i] = Some(c);
                let c = &mut scan.chunks[c];
                match line.chars().next() {
                    Some(' ') => c.spaced = true,
                    Some('+') => c.after_plus = false,
                    Some('-') => c.after_minus = false,
                    _ => {}
                }
            }
            Section::Other => {}
        }
    }
    if section == Section::Add {
        close_add(&mut scan, hunks - 1, &mut add_after_plus);
    }
    for c in &scan.chunks {
        if c.after_plus || c.after_minus {
            scan.requests.push(FinalNewline {
                hunk: c.hunk,
                chunk: Some(c.index),
                newline: !c.after_plus,
            });
        }
    }
    scan
}

/// Sets the final-newline state `requests` ask for on the parsed `hunks`.
fn apply_final_newlines(hunks: &mut [Hunk], requests: &[FinalNewline]) {
    for request in requests {
        match (hunks.get_mut(request.hunk), request.chunk) {
            (Some(Hunk::AddFile { contents, .. }), None) if !request.newline => {
                if contents.ends_with('\n') {
                    contents.pop();
                }
            }
            (Some(Hunk::UpdateFile { chunks, .. }), Some(c)) => {
                if let Some(chunk) = chunks.get_mut(c) {
                    chunk.final_newline = Some(request.newline);
                }
            }
            _ => {}
        }
    }
}

/// Whether `line`, inside an Update File section, has no prefix at all: it is
/// not blank, does not start with ` `, `+` or `-`, and is not a marker.
fn is_bare_body_line(line: &str) -> bool {
    let Some(first) = line.chars().next() else {
        return false;
    };
    !matches!(first, ' ' | '+' | '-') && !line.starts_with("@@") && !line.starts_with("***")
}

/// Turns a blank line inside an Add File body into `+` (see [`repaired`]).
/// Only a section whose non-blank lines all start with `+` is touched, and
/// only between its first and last `+` line. True when a line was changed.
fn plus_blank_add_lines(lines: &mut [String]) -> bool {
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        if !lines[i]
            .trim_start()
            .starts_with(parser::ADD_FILE_MARKER.trim_end())
        {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < lines.len() && !lines[end].trim_start().starts_with("***") {
            end += 1;
        }
        let body = &mut lines[start..end];
        let first = body.iter().position(|l| l.starts_with('+'));
        let last = body.iter().rposition(|l| l.starts_with('+'));
        if let (Some(first), Some(last)) = (first, last)
            && body
                .iter()
                .all(|l| l.starts_with('+') || l.trim().is_empty())
        {
            for l in &mut body[first..last] {
                if l.trim().is_empty() {
                    *l = "+".to_string();
                    changed = true;
                }
            }
        }
        i = end.max(i + 1);
    }
    changed
}

/// Gives every line of an all-raw Add File body a `+` (see [`repaired`]).
/// True when a section was changed.
fn prefix_raw_add_bodies(lines: &mut Vec<String>) -> bool {
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        if !lines[i]
            .trim_start()
            .starts_with(parser::ADD_FILE_MARKER.trim_end())
        {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < lines.len() && !lines[end].trim_start().starts_with("***") {
            end += 1;
        }
        // Blank lines before the next marker separate sections; they are not
        // a final blank line of the file.
        let mut body_end = end;
        while body_end > start && lines[body_end - 1].trim().is_empty() {
            body_end -= 1;
        }
        if body_end > start && lines[start..body_end].iter().all(|l| !l.starts_with('+')) {
            for l in &mut lines[start..body_end] {
                l.insert(0, '+');
            }
            lines.drain(body_end..end);
            end = body_end;
            changed = true;
        }
        i = end.max(i + 1);
    }
    changed
}

/// The text of an `@@` line written with no space after the marker
/// (`@@text`), or `None`. Text starting with `@` is not read (`@@@ x`).
fn glued_marker_text(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("@@")?;
    let first = rest.chars().next()?;
    (!first.is_whitespace() && first != '@').then_some(rest)
}

/// The text after a unified-diff hunk header `@@ -a[,b] +c[,d] @@`, trimmed
/// (empty when the header stands alone), or `None` if `line` is not one.
fn unified_header_context(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, rest) = rest.split_once(" @@")?;
    let range = |r: &str| {
        r.split(',').count() <= 2
            && r.split(',')
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    (range(old) && range(new) && (rest.is_empty() || rest.starts_with(' '))).then(|| rest.trim())
}

/// The patch as parsed, and the notes for everything lenient about it.
struct Parsed {
    hunks: Vec<Hunk>,
    notes: Vec<String>,
}

/// A Delete File and an Add File of one path in one patch is a replace: the
/// delete is dropped (the add overwrites the file) and a note says so. Both
/// orders count. This is the evident intent, and the two hunks would
/// otherwise be refused as one path named twice.
fn merge_delete_add(hunks: Vec<Hunk>, notes: &mut Vec<String>) -> Vec<Hunk> {
    let added: Vec<PathBuf> = hunks
        .iter()
        .filter_map(|h| match h {
            Hunk::AddFile { path, .. } => normalize_hunk_path(path).ok(),
            _ => None,
        })
        .collect();
    hunks
        .into_iter()
        .filter(|h| {
            let Hunk::DeleteFile { path } = h else {
                return true;
            };
            let Ok(norm) = normalize_hunk_path(path) else {
                return true;
            };
            let replaced = added
                .iter()
                .any(|a| kgraf_core::paths::fold_path(a) == kgraf_core::paths::fold_path(&norm));
            if replaced {
                notes.push(format!(
                    "note: the patch deleted and added {}; it was applied as a replace of the \
                     file (the Add File content is now the whole file)",
                    norm.display()
                ));
            }
            !replaced
        })
        .collect()
}

fn parse(input: &Value) -> Result<Parsed, ToolError> {
    let (text, repairs, final_newlines) = repaired(patch_text(input)?);
    let mut notes: Vec<String> = repairs.iter().map(ToString::to_string).collect();
    let mut hunks = parse_patch(&text).map_err(|e| ToolError::InvalidInput {
        tool: "apply_patch".into(),
        reason: e.to_string(),
    })?;
    apply_final_newlines(&mut hunks, &final_newlines);
    if hunks.len() > MAX_HUNKS {
        return Err(ToolError::InvalidInput {
            tool: "apply_patch".into(),
            reason: format!(
                "patch names {} hunks, over the {MAX_HUNKS} cap",
                hunks.len()
            ),
        });
    }
    let hunks = merge_delete_add(hunks, &mut notes);
    Ok(Parsed { hunks, notes })
}

/// `result` with each of `notes` added to a completed call as a text part.
fn with_notes(
    result: Result<ToolOutput, ToolError>,
    notes: Vec<String>,
) -> Result<ToolOutput, ToolError> {
    match result {
        Ok(mut out) => {
            for note in notes {
                out.parts.push(crate::OutputPart::Text {
                    text: crate::UntrustedText::new(note),
                });
            }
            Ok(out)
        }
        other => other,
    }
}

fn internal(reason: impl Into<String>) -> ToolError {
    ToolError::Internal {
        tool: "apply_patch".into(),
        reason: reason.into(),
    }
}

/// Lexically normalize a hunk's path, refusing an absolute path, a `..`
/// escape, and a path that normalizes to the workspace
/// root itself (`x/..`): `apply_patch` edits files, not "the workspace".
fn normalize_hunk_path(path: &Path) -> Result<PathBuf, ToolError> {
    let normalized = crate::normalize_relative(path).map_err(|source| ToolError::Access {
        tool: "apply_patch".into(),
        source,
    })?;
    if normalized.as_os_str().is_empty() {
        return Err(ToolError::InvalidInput {
            tool: "apply_patch".into(),
            reason: format!(
                "{} normalizes to the workspace root; apply_patch edits files, not the workspace itself",
                path.display()
            ),
        });
    }
    Ok(normalized)
}

/// Every path a hunk writes, normalized: the hunk's own path, plus a move
/// destination.
fn written_paths(hunk: &Hunk) -> Result<Vec<PathBuf>, ToolError> {
    match hunk {
        Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } => {
            Ok(vec![normalize_hunk_path(path)?])
        }
        Hunk::UpdateFile {
            path, move_path, ..
        } => {
            let path = normalize_hunk_path(path)?;
            match move_path {
                Some(dest) => Ok(vec![path, normalize_hunk_path(dest)?]),
                None => Ok(vec![path]),
            }
        }
    }
}

/// Refuses a patch that names one path in more than one hunk, a move
/// destination included. Every hunk is planned against the file as it is on
/// disk, so a second hunk on the same path would plan from the old content and
/// its write would replace the first hunk's (an Update then an Update, a
/// Delete then an Add, a Move onto a path another hunk edits). Paths are
/// compared as the file system resolves them (`fold_path`), so a case or
/// normalization twin counts as the same path. A hunk that moves a file onto
/// its own path names it once.
///
/// `resolve` maps each path to the file it reaches: the identity before the
/// workspace is open, and the path with its in-workspace links followed once
/// it is, so two hunks on `a` and on a link `b -> a` are one file too.
fn refuse_repeated_paths(
    hunks: &[Hunk],
    resolve: impl Fn(&Path) -> Result<PathBuf, ToolError>,
) -> Result<(), ToolError> {
    let mut seen = std::collections::HashSet::new();
    for hunk in hunks {
        let mut own = std::collections::HashSet::new();
        for path in written_paths(hunk)? {
            let resolved = resolve(&path)?;
            let key = kgraf_core::paths::fold_path(&resolved);
            if own.insert(key.clone()) && !seen.insert(key) {
                let named = if resolved == path {
                    path.display().to_string()
                } else {
                    format!(
                        "{} (a link that reaches {})",
                        path.display(),
                        resolved.display()
                    )
                };
                return Err(ToolError::InvalidInput {
                    tool: "apply_patch".into(),
                    reason: format!(
                        "{named} is named in more than one hunk. Each hunk is matched against \
                         the file as it is now, so a second hunk on one file would overwrite \
                         the first. Put every change to one file in a single Update File hunk \
                         (several @@ chunks are allowed) and send a separate patch for a \
                         delete followed by an add."
                    ),
                });
            }
        }
    }
    Ok(())
}

/// The lexical form of [`refuse_repeated_paths`], for use before the
/// workspace is open.
fn lexical(path: &Path) -> Result<PathBuf, ToolError> {
    Ok(path.to_path_buf())
}

#[async_trait::async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".into(),
            description: APPLY_PATCH_DESCRIPTION.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "A patch beginning with '*** Begin Patch' and \
                            ending with '*** End Patch'. Every Add File line and every \
                            Update File body line must start with '+', '-' or a literal \
                            space; see this tool's own description for the full grammar \
                            and a worked example.",
                    }
                },
                "required": ["patch"],
                "additionalProperties": false,
            }),
        }
    }

    /// No argument-scoped permission pattern applies to `apply_patch`: its
    /// subject would be the whole patch body, which is not a command line a
    /// prefix rule could meaningfully name.
    /// `access()`'s per-file declarations, above, are the real gate.
    fn permission_subject(&self, _input: &Value) -> Option<String> {
        None
    }

    /// Parses the patch's file headers (`kgraf-tools`
    /// already refuses an escaping path here for every built-in tool) so the
    /// runtime knows every file this call will touch before it is approved.
    /// This is a *lexical* check only; the symlink-resistant check that
    /// actually prevents an escape runs in [`fs_safety`] at [`Tool::call`]
    /// time, since `access()` has no filesystem to resolve a symlink against.
    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        let hunks = parse(input)?.hunks;
        refuse_repeated_paths(&hunks, lexical)?;
        let mut access = Access::none();
        for hunk in &hunks {
            for path in written_paths(hunk)? {
                fs_safety::refuses_git_or_kgraf("apply_patch", &path)?;
                access = access
                    .write_path(&path)
                    .map_err(|source| ToolError::Access {
                        tool: "apply_patch".into(),
                        source,
                    })?;
            }
        }
        Ok(access)
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled {
                tool: "apply_patch".into(),
            });
        }
        let Parsed { hunks, notes } = parse(&input)?;
        if hunks.is_empty() {
            return Ok(ToolOutput::error("the patch made no changes"));
        }
        let root = ctx.workspace_root.clone();
        let cancel = ctx.cancel.clone();
        let task = tokio::task::spawn_blocking(move || apply_all(&hunks, &root, &cancel));
        let result = tokio::select! {
            _ = ctx.cancel.cancelled() => Err(ToolError::Cancelled { tool: "apply_patch".into() }),
            result = task => result.map_err(|e| internal(format!("apply task failed: {e}")))?,
        };
        with_notes(result, notes)
    }
}

/// One hunk's effect, fully validated (paths checked, an update's context
/// matched against the file's current content) but not yet written to disk.
enum Planned {
    Add {
        path: PathBuf,
        contents: String,
    },
    Delete {
        path: PathBuf,
    },
    Write {
        path: PathBuf,
        contents: String,
    },
    Move {
        from: PathBuf,
        to: PathBuf,
        contents: String,
    },
}

/// Plan and then commit every hunk (nothing is written until every hunk
/// has been validated). Runs on a blocking-pool thread; `cancel` is
/// checked between hunks so a multi-hunk patch can still bail out promptly,
/// even though the matching work inside one hunk cannot be interrupted.
fn apply_all(
    hunks: &[Hunk],
    root: &Path,
    cancel: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let applied = apply_all_names("apply_patch", hunks, root, cancel, fs_safety::Names::Plain)?;
    with_notes(Ok(ToolOutput::text(applied.summary)), applied.notes)
}

/// What a completed apply reports: the summary of files changed, and a note
/// for each place a line matched only under a tolerant rule.
struct Applied {
    summary: String,
    notes: Vec<String>,
}

/// [`apply_all`] under the write guard's names policy `names`: the protected
/// apply admits `.gitattributes`, `.gitmodules` and `.githooks` in Bypass.
/// `tool` is the name its errors carry: `replace_in_file` writes through here too.
fn apply_all_names(
    tool: &str,
    hunks: &[Hunk],
    root: &Path,
    cancel: &CancellationToken,
    names: fs_safety::Names,
) -> Result<Applied, ToolError> {
    refuse_repeated_paths(hunks, lexical)?;
    let dir = fs_safety::open_workspace(tool, root)?;
    // Again with links followed: a hunk on a link and a hunk on its target
    // name one file, and both would plan from its old content.
    refuse_repeated_paths(hunks, |path| fs_safety::resolve_links(tool, &dir, path))?;
    // The write guard runs on every written path before any hunk is planned:
    // planning reads through the read guard, which lets `.gitmodules` through,
    // so without this a protected path late in the patch would be refused only
    // at commit, after earlier hunks had written.
    for hunk in hunks {
        for path in written_paths(hunk)? {
            fs_safety::guard_write_names(tool, &dir, &path, names)?;
        }
    }

    let mut planned = Vec::with_capacity(hunks.len());
    let mut notes = Vec::new();
    for hunk in hunks {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled { tool: tool.into() });
        }
        planned.push(plan_one(tool, hunk, &dir, &mut notes)?);
    }

    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    for change in planned {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled { tool: tool.into() });
        }
        commit_one(
            tool,
            change,
            &dir,
            names,
            &mut added,
            &mut modified,
            &mut deleted,
        )?;
    }
    Ok(Applied {
        summary: summary(&added, &modified, &deleted),
        notes,
    })
}

/// Whether the `apply_patch` call `input` would apply cleanly to the tree at
/// `root` right now, decided by the same parser, path guards and matcher the
/// call uses, stopping before the commit pass: nothing is written. `Err` is
/// the reason the call would be refused.
pub fn check_applies(input: &Value, root: &Path) -> Result<(), String> {
    let check = || -> Result<(), ToolError> {
        let Parsed { hunks, .. } = parse(input)?;
        if hunks.is_empty() {
            return Err(internal("the patch made no changes"));
        }
        refuse_repeated_paths(&hunks, lexical)?;
        let dir = fs_safety::open_workspace("apply_patch", root)?;
        refuse_repeated_paths(&hunks, |path| {
            fs_safety::resolve_links("apply_patch", &dir, path)
        })?;
        for hunk in &hunks {
            for path in written_paths(hunk)? {
                fs_safety::guard_write_names("apply_patch", &dir, &path, fs_safety::Names::Plain)?;
            }
        }
        let mut notes = Vec::new();
        for hunk in &hunks {
            plan_one("apply_patch", hunk, &dir, &mut notes)?;
        }
        Ok(())
    };
    check().map_err(|e| e.to_string())
}

fn plan_one(
    tool: &str,
    hunk: &Hunk,
    dir: &Dir,
    notes: &mut Vec<String>,
) -> Result<Planned, ToolError> {
    match hunk {
        Hunk::AddFile { path, contents } => {
            let path = normalize_hunk_path(path)?;
            // An add over an existing hard-linked file would be refused at
            // commit, after earlier hunks had written; refuse it here.
            fs_safety::refuse_if_hard_linked(tool, dir, &path)?;
            Ok(Planned::Add {
                path,
                contents: contents.clone(),
            })
        }
        Hunk::DeleteFile { path } => {
            let path = normalize_hunk_path(path)?;
            // Validated here, not deferred to the commit pass: a delete
            // of a missing file or a directory must fail before any other
            // hunk in this patch has written anything.
            match fs_safety::metadata_if_exists("apply_patch", dir, &path)? {
                Some(meta) if meta.is_file() => {
                    fs_safety::refuse_if_hard_linked("apply_patch", dir, &path)?;
                }
                Some(_) => {
                    return Err(internal(format!(
                        "{} is a directory; apply_patch deletes files only",
                        path.display()
                    )));
                }
                None => {
                    return Err(internal(format!("{} does not exist", path.display())));
                }
            }
            Ok(Planned::Delete { path })
        }
        Hunk::UpdateFile {
            path,
            move_path,
            chunks,
        } => {
            let path = normalize_hunk_path(path)?;
            let original =
                fs_safety::read_capped_to_string("apply_patch", dir, &path, MAX_READ_BYTES, false)?;
            let (new_contents, matched) =
                replace::derive_new_contents(&original, &path.display().to_string(), chunks)
                    .map_err(internal)?;
            notes.extend(matched);
            match move_path {
                Some(dest) => {
                    let dest = normalize_hunk_path(dest)?;
                    // A move overwrites `dest`; a hard-linked `dest` is refused
                    // now, before any hunk writes (the source was checked by
                    // the read above).
                    fs_safety::refuse_if_hard_linked("apply_patch", dir, &dest)?;
                    // The lexically identical case (`dest == path`) is caught above; the
                    // same bug
                    // is reached two other ways a string comparison cannot see:
                    // a case-only alias on a case-insensitive filesystem
                    // (`Readme.md` -> `README.md`) and a symlink alias
                    // (`b.txt -> a.txt`). Both name one real file on disk, so
                    // "write the new content to `dest`, then delete `path`"
                    // would write it and then delete that same file out from
                    // under itself. Comparing `(dev, ino)` after resolution
                    // catches both: the destination's *current* on-disk
                    // identity is what matters, checked before any write.
                    let same_file = dest == path || paths_are_same_file(dir, &path, &dest)?;
                    if same_file {
                        Ok(Planned::Write {
                            path,
                            contents: new_contents,
                        })
                    } else {
                        Ok(Planned::Move {
                            from: path,
                            to: dest,
                            contents: new_contents,
                        })
                    }
                }
                None => Ok(Planned::Write {
                    path,
                    contents: new_contents,
                }),
            }
        }
    }
}

fn commit_one(
    tool: &str,
    change: Planned,
    dir: &Dir,
    names: fs_safety::Names,
    added: &mut Vec<String>,
    modified: &mut Vec<String>,
    deleted: &mut Vec<String>,
) -> Result<(), ToolError> {
    match change {
        Planned::Add { path, contents } => {
            fs_safety::write_confined_names(tool, dir, &path, contents.as_bytes(), names)?;
            added.push(display(&path));
        }
        Planned::Delete { path } => {
            fs_safety::remove_file_confined_names(tool, dir, &path, names)?;
            deleted.push(display(&path));
        }
        Planned::Write { path, contents } => {
            fs_safety::write_confined_names(tool, dir, &path, contents.as_bytes(), names)?;
            modified.push(display(&path));
        }
        Planned::Move { from, to, contents } => {
            fs_safety::write_confined_names(tool, dir, &to, contents.as_bytes(), names)?;
            fs_safety::remove_file_confined_names(tool, dir, &from, names)?;
            modified.push(display(&to));
        }
    }
    Ok(())
}

/// Whether `a` and `dest` currently name the same file on disk: a
/// case-only alias on a case-insensitive filesystem, or a symlink alias. `a`
/// is expected to exist (its content was just read); `dest` may not.
#[cfg(unix)]
fn paths_are_same_file(dir: &Dir, a: &Path, dest: &Path) -> Result<bool, ToolError> {
    let a_id = fs_safety::file_identity("apply_patch", dir, a)?;
    let dest_id = fs_safety::file_identity("apply_patch", dir, dest)?;
    Ok(matches!((a_id, dest_id), (Some(a), Some(b)) if a == b))
}

/// Off Unix there is no file identity in `cap_std`'s portable metadata. A
/// move between two spellings the file system may resolve to one file (a
/// case-only rename on a case-insensitive volume, the Windows default) is
/// refused rather than guessed: written and then deleted, it would lose the
/// file. A symbolic-link alias is not detected here (a known gap; creating a
/// link on Windows needs a privilege a worker does not normally hold).
#[cfg(not(unix))]
fn paths_are_same_file(_dir: &Dir, a: &Path, dest: &Path) -> Result<bool, ToolError> {
    if kgraf_core::paths::same_path(a, dest) {
        return Err(internal(format!(
            "moving {} to {} changes only how the name is spelled; that cannot be checked \
             on this platform, so rename it through a temporary name in two patches",
            a.display(),
            dest.display()
        )));
    }
    Ok(false)
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn summary(added: &[String], modified: &[String], deleted: &[String]) -> String {
    let mut lines = vec!["Success. Updated the following files:".to_string()];
    for p in added {
        lines.push(format!("A {p}"));
    }
    for p in modified {
        lines.push(format!("M {p}"));
    }
    for p in deleted {
        lines.push(format!("D {p}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancellationToken as CT;

    fn ctx(root: std::path::PathBuf) -> ToolContext {
        ToolContext {
            worker: "w1".into(),
            task: "t1".into(),
            workspace_root: root,
            cancel: CT::new(),
            progress: crate::ProgressSink::default(),
        }
    }

    fn wrap(body: &str) -> Value {
        json!({ "patch": format!("*** Begin Patch\n{body}\n*** End Patch") })
    }

    /// The example a rejection carries is itself a valid patch, in both forms,
    /// and each known error names the one rule it broke; an error that is not
    /// about the patch's format gets no example.
    #[test]
    fn a_rejection_help_example_is_a_valid_patch_for_the_targeted_file() {
        let add = patch_target(&json!({"patch": "*** Begin Patch\n*** Add File: src/new.rs\n+x"}))
            .expect("target");
        assert_eq!(
            add,
            PatchTarget {
                path: "src/new.rs".into(),
                adds: true
            }
        );
        let update = patch_target(&json!({"patch": "junk\n*** Update File: src/lib.rs\n x"}))
            .expect("target");
        assert!(!update.adds);
        assert_eq!(patch_target(&json!({"patch": "--- a/x\n+++ b/x"})), None);
        for (error, target, rule, header) in [
            (
                "invalid patch: The last line of the patch must be '*** End Patch'",
                Some(&add),
                "must be exactly `*** End Patch`",
                "*** Add File: src/new.rs",
            ),
            (
                "invalid patch: The first line of the patch must be '*** Begin Patch'",
                Some(&update),
                "must be exactly `*** Begin Patch`",
                "*** Update File: src/lib.rs",
            ),
            (
                "invalid hunk at line 3, 'x' is not a valid hunk header",
                None,
                "every line starts with `+`",
                "*** Update File: path/to/file",
            ),
            (
                "Failed to find context 'fn a' in src/lib.rs",
                Some(&update),
                "copy those lines exactly",
                "*** Update File: src/lib.rs",
            ),
        ] {
            let help = rejection_help(error, target).expect("help");
            assert!(help.contains(rule), "{help}");
            assert!(help.contains(header), "{help}");
            let example = help
                .split_once("for this file:\n")
                .expect("example")
                .1
                .to_owned();
            let hunks = parse_patch(&example)
                .unwrap_or_else(|e| panic!("the example must parse: {e}\n{example}"));
            assert_eq!(hunks.len(), 1, "{example}");
        }
        // A protected path or an escaping path is not a format error.
        assert_eq!(rejection_help("path is protected", Some(&add)), None);
    }

    /// A patch that starts with a file header and has no envelope is applied as
    /// if it had one, and the result says so; one that starts any other way
    /// stays refused.
    #[tokio::test]
    async fn a_patch_without_its_envelope_is_applied_and_the_result_says_so() {
        let tool = ApplyPatchTool;
        for (patch, why) in [
            (
                "*** Add File: a.txt\n+one\n+two",
                "no begin and no end line",
            ),
            (
                "\n  *** Add File: a.txt\n+one\n+two\n*** End Patch\n",
                "no begin line, leading blank lines",
            ),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let input = json!({ "patch": patch });
            tool.access(&input).expect("access");
            let out = tool
                .call(
                    input,
                    &ctx(dir.path().to_path_buf()),
                    &crate::CallPermit::new(),
                )
                .await
                .expect(why);
            assert!(!out.is_error, "{why}");
            let text = out.render();
            assert!(
                text.contains("note: the patch had no '*** Begin Patch' line"),
                "{why}: {text}"
            );
            let written = std::fs::read_to_string(dir.path().join("a.txt")).expect("written");
            assert_eq!(written, "one\ntwo\n", "{why}");
        }
        // A patch with its envelope carries no note.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = tool
            .call(
                wrap("*** Add File: a.txt\n+one"),
                &ctx(dir.path().to_path_buf()),
                &crate::CallPermit::new(),
            )
            .await
            .expect("call");
        assert!(!out.render().contains("note:"));
        // Malformed beyond the missing envelope stays an error.
        for bad in [
            "just some text",
            "--- a/x\n+++ b/x\n*** Add File: a.txt\n+one",
        ] {
            let err = tool.access(&json!({ "patch": bad })).expect_err("refused");
            assert!(
                matches!(err, ToolError::InvalidInput { .. }),
                "{bad}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn a_patch_applied_to_a_temp_workspace_changes_the_file_as_expected() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("hello.txt"), "hi\nold\n").expect("seed");
        let input = wrap("*** Update File: hello.txt\n@@\n hi\n-old\n+new");
        let tool = ApplyPatchTool;
        let access = tool.access(&input).expect("access");
        assert!(
            access
                .writes_paths()
                .contains(std::path::Path::new("hello.txt"))
        );
        let permit = crate::CallPermit::new();
        let out = tool
            .call(input, &ctx(dir.path().to_path_buf()), &permit)
            .await
            .expect("call");
        assert!(!out.is_error);
        let contents = std::fs::read_to_string(dir.path().join("hello.txt")).expect("read back");
        assert_eq!(contents, "hi\nnew\n");
    }

    /// A path named in two hunks used to lose the first edit: both planned
    /// from the old content and the second write replaced the first.
    #[tokio::test]
    async fn one_path_in_two_hunks_is_refused_and_writes_nothing() {
        let tool = ApplyPatchTool;
        for (label, body) in [
            (
                "update twice",
                "*** Update File: a.txt\n@@\n-one\n+ONE\n*** Update File: a.txt\n@@\n-two\n+TWO",
            ),
            (
                "move onto an edited path",
                "*** Update File: a.txt\n*** Move to: b.txt\n@@\n-one\n+ONE\n*** Update File: b.txt\n@@\n-x\n+y",
            ),
            (
                "case twin",
                "*** Update File: a.txt\n@@\n-one\n+ONE\n*** Update File: A.TXT\n@@\n-two\n+TWO",
            ),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").expect("seed");
            std::fs::write(dir.path().join("b.txt"), "x\n").expect("seed");
            let input = wrap(body);
            let err = tool.access(&input).expect_err(label);
            assert!(
                err.to_string().contains("more than one hunk"),
                "{label}: {err}"
            );
            let err = tool
                .call(
                    input,
                    &ctx(dir.path().to_path_buf()),
                    &crate::CallPermit::new(),
                )
                .await
                .expect_err(label);
            assert!(
                err.to_string().contains("more than one hunk"),
                "{label}: {err}"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("a.txt")).expect("read"),
                "one\ntwo\n",
                "{label}: nothing may be written"
            );
        }
    }

    /// A hunk on a link and a hunk on its target name one file: both would
    /// plan from its old content and the second write would drop the first.
    /// A link to a directory counts the same way.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_and_its_target_in_two_hunks_are_refused() {
        for (label, body) in [
            (
                "file link",
                "*** Update File: a.txt\n@@\n-one\n+ONE\n*** Update File: b.txt\n@@\n-two\n+TWO",
            ),
            (
                "directory link",
                "*** Update File: src/a.txt\n@@\n-one\n+ONE\n*** Update File: lnk/a.txt\n@@\n-two\n+TWO",
            ),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir(dir.path().join("src")).expect("src");
            std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").expect("seed");
            std::fs::write(dir.path().join("src/a.txt"), "one\ntwo\n").expect("seed");
            std::os::unix::fs::symlink("a.txt", dir.path().join("b.txt")).expect("link");
            std::os::unix::fs::symlink("src", dir.path().join("lnk")).expect("link");
            let err = ApplyPatchTool
                .call(
                    wrap(body),
                    &ctx(dir.path().to_path_buf()),
                    &crate::CallPermit::new(),
                )
                .await
                .expect_err(label);
            assert!(
                err.to_string().contains("more than one hunk")
                    && err.to_string().contains("a link that reaches"),
                "{label}: {err}"
            );
            for file in ["a.txt", "src/a.txt"] {
                assert_eq!(
                    std::fs::read_to_string(dir.path().join(file)).expect("read"),
                    "one\ntwo\n",
                    "{label}: nothing may be written"
                );
            }
        }
    }

    /// Two chunks in one hunk on one path is the supported way to edit a file
    /// in two places, and a move onto its own path names the path once.
    #[tokio::test]
    async fn two_chunks_in_one_hunk_still_apply() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "one\nmid\ntwo\n").expect("seed");
        let input = wrap("*** Update File: a.txt\n@@\n-one\n+ONE\n@@\n-two\n+TWO");
        let out = ApplyPatchTool
            .call(
                input,
                &ctx(dir.path().to_path_buf()),
                &crate::CallPermit::new(),
            )
            .await
            .expect("call");
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).expect("read"),
            "ONE\nmid\nTWO\n"
        );
    }

    #[tokio::test]
    async fn adding_and_deleting_files_works() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("gone.txt"), "bye\n").expect("seed");
        let input = wrap("*** Add File: new/nested.txt\n+created\n*** Delete File: gone.txt");
        let tool = ApplyPatchTool;
        let permit = crate::CallPermit::new();
        let out = tool
            .call(input, &ctx(dir.path().to_path_buf()), &permit)
            .await
            .expect("call");
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new/nested.txt")).expect("read"),
            "created\n"
        );
        assert!(!dir.path().join("gone.txt").exists());
    }

    #[test]
    fn a_patch_that_escapes_the_workspace_is_refused() {
        let input = wrap("*** Add File: ../outside.txt\n+x");
        let err = ApplyPatchTool.access(&input).expect_err("must refuse");
        assert!(matches!(err, ToolError::Access { .. }));
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let input = wrap("*** Delete File: /etc/passwd");
        let err = ApplyPatchTool.access(&input).expect_err("must refuse");
        assert!(matches!(err, ToolError::Access { .. }));
    }

    /// A blank line inside an Add File reaches the model named as blank, with
    /// how to write one, through the tool's own refusal (a Delete File and an
    /// Add File of one path, a blank line before the end line).
    #[test]
    fn a_blank_add_file_line_is_refused_naming_the_blank_line() {
        let input = json!({"patch": "*** Begin Patch\n*** Delete File: src/lib.rs\n*** Add File: \
                                     src/lib.rs\n+pub fn a() {}\n\n*** End Patch\n"});
        let err = ApplyPatchTool.access(&input).expect_err("must refuse");
        let ToolError::InvalidInput { reason, .. } = &err else {
            panic!("{err:?}")
        };
        assert!(
            reason.contains("invalid hunk at line 5, the line is blank."),
            "{reason}"
        );
        assert!(reason.contains("a line holding only '+'"), "{reason}");
        assert!(
            !reason.contains("'' is not a valid hunk header"),
            "{reason}"
        );
    }

    /// `x/..` normalizes to the empty path, i.e. the
    /// workspace root itself. `apply_patch` edits files, not "the workspace".
    #[test]
    fn a_path_normalizing_to_the_workspace_root_is_refused() {
        let input = wrap("*** Add File: x/..\n+x");
        let err = ApplyPatchTool.access(&input).expect_err("must refuse");
        assert!(matches!(err, ToolError::InvalidInput { .. }));
    }

    #[tokio::test]
    async fn call_independently_refuses_an_escape_even_if_access_was_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = wrap("*** Add File: ../outside.txt\n+x");
        let tool = ApplyPatchTool;
        let permit = crate::CallPermit::new();
        let err = tool
            .call(input, &ctx(dir.path().to_path_buf()), &permit)
            .await
            .expect_err("must refuse");
        assert!(matches!(err, ToolError::Access { .. }));
    }

    /// Regression: writing through a symlinked
    /// intermediate directory used to reach outside the workspace.
    #[cfg(unix)]
    #[tokio::test]
    async fn adding_a_file_through_a_symlinked_directory_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("d")).expect("symlink");
        let input = wrap("*** Add File: d/authorized_keys\n+pwned");
        let tool = ApplyPatchTool;
        let permit = crate::CallPermit::new();
        let err = tool
            .call(input, &ctx(dir.path().to_path_buf()), &permit)
            .await
            .expect_err("must refuse the escape");
        assert!(matches!(
            err,
            ToolError::Access { .. } | ToolError::Internal { .. }
        ));
        assert!(!outside.path().join("authorized_keys").exists());
    }

    /// Regression: updating a path that is a symlink
    /// to an outside file used to overwrite the outside target.
    #[cfg(unix)]
    #[tokio::test]
    async fn updating_a_symlinked_file_pointing_outside_the_workspace_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join(".zshrc");
        std::fs::write(&target, "original\n").expect("seed");
        std::os::unix::fs::symlink(&target, dir.path().join(".zshrc")).expect("symlink");
        let input = wrap("*** Update File: .zshrc\n@@\n-original\n+pwned");
        let tool = ApplyPatchTool;
        let permit = crate::CallPermit::new();
        let err = tool
            .call(input, &ctx(dir.path().to_path_buf()), &permit)
            .await
            .expect_err("must refuse the escape");
        assert!(matches!(
            err,
            ToolError::Access { .. } | ToolError::Internal { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(&target).expect("unchanged"),
            "original\n"
        );
    }

    /// Regression: deleting through a symlinked
    /// intermediate directory used to delete the outside file.
    #[cfg(unix)]
    #[tokio::test]
    async fn deleting_through_a_symlinked_directory_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("keep.txt");
        std::fs::write(&target, "do not delete").expect("seed");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("d")).expect("symlink");
        let input = wrap("*** Delete File: d/keep.txt");
        let tool = ApplyPatchTool;
        let permit = crate::CallPermit::new();
        let err = tool
            .call(input, &ctx(dir.path().to_path_buf()), &permit)
            .await
            .expect_err("must refuse the escape");
        assert!(matches!(
            err,
            ToolError::Access { .. } | ToolError::Internal { .. }
        ));
        assert!(target.exists());
    }

    /// A move whose destination normalizes to the same
    /// path as the source used to write the new content and then delete that
    /// same file out from under itself, losing it entirely.
    #[tokio::test]
    async fn a_move_to_the_same_path_is_treated_as_a_plain_update() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("same.txt"), "old\n").expect("seed");
        let input = wrap("*** Update File: same.txt\n*** Move to: ./same.txt\n@@\n-old\n+new");
        let tool = ApplyPatchTool;
        let permit = crate::CallPermit::new();
        let out = tool
            .call(input, &ctx(dir.path().to_path_buf()), &permit)
            .await
            .expect("call");
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("same.txt")).expect("must still exist"),
            "new\n"
        );
    }

    /// A patch whose second hunk cannot be matched
    /// must leave the first hunk's file untouched, not partially applied.
    #[tokio::test]
    async fn a_later_hunk_that_fails_to_match_leaves_earlier_hunks_uncommitted() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "a-old\n").expect("seed");
        std::fs::write(dir.path().join("b.txt"), "b-old\n").expect("seed");
        let input = wrap(
            "*** Update File: a.txt\n@@\n-a-old\n+a-new\n\
             *** Update File: b.txt\n@@\n-this-context-does-not-exist\n+b-new",
        );
        let tool = ApplyPatchTool;
        let permit = crate::CallPermit::new();
        let err = tool
            .call(input, &ctx(dir.path().to_path_buf()), &permit)
            .await
            .expect_err("the unmatched hunk must fail the whole call");
        assert!(matches!(err, ToolError::Internal { .. }));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).expect("read"),
            "a-old\n",
            "the first hunk must not have been committed"
        );
    }

    #[tokio::test]
    async fn an_already_cancelled_call_is_refused_before_doing_any_work() {
        let dir = tempfile::tempdir().expect("tempdir");
        let c = ctx(dir.path().to_path_buf());
        c.cancel.cancel();
        let input = wrap("*** Add File: x.txt\n+x");
        let permit = crate::CallPermit::new();
        let err = ApplyPatchTool
            .call(input, &c, &permit)
            .await
            .expect_err("must observe cancellation");
        assert!(matches!(err, ToolError::Cancelled { .. }));
        assert!(!dir.path().join("x.txt").exists());
    }

    #[test]
    fn a_patch_over_the_byte_cap_is_refused() {
        let huge = "x".repeat(MAX_PATCH_BYTES + 1);
        let input = json!({ "patch": huge });
        let err = ApplyPatchTool.access(&input).expect_err("must refuse");
        assert!(matches!(err, ToolError::InvalidInput { .. }));
    }

    /// On a case-insensitive filesystem (macOS's
    /// APFS default), `Readme.md` and `README.md` are the same directory
    /// entry. Before this fix, writing the new content to `README.md` and
    /// then deleting `Readme.md` deleted that same entry, losing the file.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_case_only_move_on_a_case_insensitive_filesystem_keeps_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("Readme.md"), "keep\n").expect("seed");
        let input = wrap("*** Update File: Readme.md\n*** Move to: README.md\n@@\n-keep\n+kept");
        let permit = crate::CallPermit::new();
        let out = ApplyPatchTool
            .call(input, &ctx(dir.path().to_path_buf()), &permit)
            .await
            .expect("call");
        assert!(!out.is_error, "{out:?}");
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "the file must not have been lost: {entries:?}"
        );
        let content = std::fs::read_to_string(dir.path().join("README.md")).expect("read back");
        assert_eq!(content, "kept\n");
    }

    /// Second case: `b.txt` is a symlink alias of `a.txt`, so "move `a.txt` to
    /// `b.txt`" writes and deletes the same underlying file.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_move_to_a_symlink_alias_of_the_source_keeps_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "data\n").expect("seed");
        std::os::unix::fs::symlink("a.txt", dir.path().join("b.txt")).expect("symlink");
        let input = wrap("*** Update File: a.txt\n*** Move to: b.txt\n@@\n-data\n+data2");
        let permit = crate::CallPermit::new();
        let out = ApplyPatchTool
            .call(input, &ctx(dir.path().to_path_buf()), &permit)
            .await
            .expect("call");
        assert!(!out.is_error, "{out:?}");
        let content = std::fs::read_to_string(dir.path().join("b.txt")).expect("read via symlink");
        assert_eq!(
            content, "data2\n",
            "the aliased file must keep its new content"
        );
    }

    /// The shared write path names the tool it was called for in its errors:
    /// `replace_in_file` writes through it, and its refusals must not say
    /// `apply_patch`.
    #[cfg(unix)]
    #[test]
    fn the_shared_write_path_errors_name_the_calling_tool() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("authorized_keys");
        std::fs::write(&target, "k\n").expect("seed");
        std::fs::hard_link(&target, dir.path().join("keys")).expect("hard link");
        let hunk = Hunk::AddFile {
            path: PathBuf::from("keys"),
            contents: "x\n".into(),
        };
        for tool in ["replace_in_file", "apply_patch"] {
            let err = apply_all_names(
                tool,
                std::slice::from_ref(&hunk),
                dir.path(),
                &CancellationToken::new(),
                fs_safety::Names::Plain,
            )
            .err()
            .expect("refused")
            .to_string();
            assert!(err.contains(tool), "{tool}: {err}");
            let other = if tool == "apply_patch" {
                "replace_in_file"
            } else {
                "apply_patch"
            };
            assert!(!err.contains(other), "{tool}: {err}");
        }
        assert_eq!(std::fs::read_to_string(&target).expect("outside"), "k\n");
    }

    /// A pre-existing hard link inside the workspace names a file outside it.
    /// Every hunk kind that would write, delete or move it is refused while
    /// planning, so an earlier hunk in the same patch writes nothing either
    /// and the refusal names the path.
    #[cfg(unix)]
    #[tokio::test]
    async fn every_hunk_touching_a_hard_linked_file_is_refused_before_anything_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("authorized_keys");
        std::fs::write(&target, "ssh-ed25519 AAAA\n").expect("seed");
        std::fs::hard_link(&target, dir.path().join("keys")).expect("hard link");
        std::fs::write(dir.path().join("plain.txt"), "p\n").expect("seed");
        let permit = crate::CallPermit::new();
        for body in [
            "*** Add File: first.txt\n+x\n*** Update File: keys\n@@\n-ssh-ed25519 AAAA\n+pwned",
            "*** Add File: first.txt\n+x\n*** Delete File: keys",
            "*** Add File: first.txt\n+x\n*** Add File: keys\n+pwned",
            "*** Add File: first.txt\n+x\n*** Update File: plain.txt\n*** Move to: keys\n@@\n-p\n+q",
            "*** Add File: first.txt\n+x\n*** Update File: keys\n*** Move to: moved.txt\n@@\n-ssh-ed25519 AAAA\n+x",
        ] {
            let err = ApplyPatchTool
                .call(wrap(body), &ctx(dir.path().to_path_buf()), &permit)
                .await
                .expect_err(body);
            let message = err.to_string();
            assert!(
                message.contains("`keys`") && message.contains("hard links"),
                "{body}: {message}"
            );
            assert!(!dir.path().join("first.txt").exists(), "{body}");
            assert!(!dir.path().join("moved.txt").exists(), "{body}");
        }
        assert_eq!(
            std::fs::read_to_string(&target).expect("outside"),
            "ssh-ed25519 AAAA\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("plain.txt")).expect("plain"),
            "p\n"
        );
    }

    /// Regression: the tool
    /// description teaches the grammar with a worked example; this test pulls
    /// that exact example back out of [`APPLY_PATCH_DESCRIPTION`] and parses
    /// it, so a future edit that breaks the example (a missing `+`, a stray
    /// unified-diff line) fails here instead of only failing silently against
    /// whichever model happens to copy it verbatim. Tests the mechanism
    /// (the example is real, parseable input), not a fixed string.
    #[test]
    fn the_description_s_own_worked_example_parses() {
        let fenced: Vec<&str> = APPLY_PATCH_DESCRIPTION.split("```").collect();
        assert_eq!(
            fenced.len(),
            3,
            "expected exactly one fenced example in the description; got {} pieces",
            fenced.len()
        );
        let example = fenced[1].trim_start_matches('\n');
        let hunks = parse_patch(example).expect("the description's own example must parse");
        // The example is written to exercise all three hunk shapes at once
        // (context/add/remove in an Update, plus an Add) so this test would
        // catch a description edit that quietly dropped one of them.
        assert_eq!(hunks.len(), 2, "expected an Update and an Add hunk");
        assert!(matches!(hunks[0], Hunk::UpdateFile { .. }));
        assert!(matches!(hunks[1], Hunk::AddFile { .. }));
    }

    /// Regression, 2026-09-28: the description must actually teach the three
    /// line-prefix rule and warn off unified-diff syntax -- the two things
    /// every captured Haiku failure got wrong. Written as substring checks on the
    /// real, shipped constant (not a copy) so a future rewrite that drops the
    /// teaching content, even while keeping the tool working, is caught here.
    #[test]
    fn the_description_teaches_the_line_prefix_rule_and_warns_off_unified_diff() {
        let d = APPLY_PATCH_DESCRIPTION;
        assert!(d.contains("literal space"), "must name the context prefix");
        assert!(
            d.contains('-') && d.contains('+'),
            "must name both edit prefixes"
        );
        assert!(
            d.to_lowercase().contains("unified-diff") || d.to_lowercase().contains("unified diff"),
            "must warn a model away from unified-diff syntax by name"
        );
        assert!(d.contains("*** Begin Patch"), "must name the outer markers");
    }

    /// A protected path in a later hunk is refused before the first hunk
    /// writes. Planning reads through the read guard, which lets
    /// `.gitmodules` through, so the write guard must run up front.
    #[tokio::test]
    async fn a_protected_path_in_a_later_hunk_writes_nothing() {
        let tool = ApplyPatchTool;
        let first = "*** Update File: a.txt\n@@\n-one\n+ONE\n";
        for (label, second) in [
            ("add", "*** Add File: .gitmodules\n+[submodule \"x\"]"),
            ("update", "*** Update File: .gitmodules\n@@\n-old\n+new"),
            ("delete", "*** Delete File: .gitmodules"),
            (
                "move destination",
                "*** Update File: b.txt\n*** Move to: .gitattributes\n@@\n-x\n+y",
            ),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(dir.path().join("a.txt"), "one\n").expect("seed");
            std::fs::write(dir.path().join("b.txt"), "x\n").expect("seed");
            std::fs::write(dir.path().join(".gitmodules"), "old\n").expect("seed");
            let input = wrap(&format!("{first}{second}"));
            assert!(
                matches!(tool.access(&input), Err(ToolError::Access { .. })),
                "{label}: access must refuse"
            );
            let err = tool
                .call(
                    input,
                    &ctx(dir.path().to_path_buf()),
                    &crate::CallPermit::new(),
                )
                .await
                .expect_err(label);
            assert!(matches!(err, ToolError::Access { .. }), "{label}: {err}");
            assert_eq!(
                std::fs::read_to_string(dir.path().join("a.txt")).expect("read"),
                "one\n",
                "{label}: the first hunk must not have written"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join(".gitmodules")).expect("read"),
                "old\n",
                "{label}"
            );
            assert!(dir.path().join("b.txt").exists(), "{label}");
            assert!(!dir.path().join(".gitattributes").exists(), "{label}");
        }
    }

    /// A link inside the workspace to `.git` or to `.gitmodules` passes every
    /// lexical check; the guard follows it and refuses the write.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_write_through_an_in_workspace_link_to_git_metadata_is_refused() {
        let tool = ApplyPatchTool;
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).expect("git dir");
        std::fs::write(root.join(".git/config"), "[core]\n").expect("seed");
        std::fs::write(root.join(".gitmodules"), "old\n").expect("seed");
        std::fs::create_dir(root.join("sub")).expect("sub");
        std::os::unix::fs::symlink(".git", root.join("g")).expect("link");
        std::os::unix::fs::symlink("../.gitmodules", root.join("sub/m")).expect("link");
        std::os::unix::fs::symlink("g", root.join("chain")).expect("link");
        for body in [
            "*** Update File: g/config\n@@\n-[core]\n+[core]\n+\tfsmonitor = evil",
            "*** Add File: g/hooks/pre-commit\n+evil",
            "*** Add File: chain/config\n+evil",
            "*** Update File: sub/m\n@@\n-old\n+new",
            "*** Delete File: sub/m",
        ] {
            let err = tool
                .call(
                    wrap(body),
                    &ctx(root.to_path_buf()),
                    &crate::CallPermit::new(),
                )
                .await
                .expect_err(body);
            assert!(matches!(err, ToolError::Access { .. }), "{body}: {err}");
        }
        assert_eq!(
            std::fs::read_to_string(root.join(".git/config")).expect("read"),
            "[core]\n"
        );
        assert!(!root.join(".git/hooks").exists());
        assert_eq!(
            std::fs::read_to_string(root.join(".gitmodules")).expect("read"),
            "old\n"
        );
        assert!(root.join("sub/m").exists(), "the link itself must survive");
    }

    /// Runs `patch` against a temp workspace seeded with `seed` files and
    /// returns the rendered result and the directory.
    async fn run_patch(
        seed: &[(&str, &str)],
        patch: &str,
    ) -> (Result<String, String>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, text) in seed {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("seed dir");
            }
            std::fs::write(path, text).expect("seed");
        }
        let out = ApplyPatchTool
            .call(
                json!({ "patch": patch }),
                &ctx(dir.path().to_path_buf()),
                &crate::CallPermit::new(),
            )
            .await;
        (out.map(|o| o.render()).map_err(|e| e.to_string()), dir)
    }

    /// Round evidence: a worker ended its patch with `***/ End Patch`, and a
    /// patch with no end line at all was refused with "last line must be".
    #[tokio::test]
    async fn a_missing_or_mangled_end_line_is_applied_and_the_result_says_so() {
        let body = "*** Begin Patch\n*** Update File: a.txt\n@@\n one\n-two\n+TWO\n";
        for (label, patch) in [
            ("no end line", body.to_string()),
            ("mangled end line", format!("{body}***/ End Patch")),
        ] {
            let (out, dir) = run_patch(&[("a.txt", "one\ntwo\n")], &patch).await;
            let text = out.unwrap_or_else(|e| panic!("{label}: {e}"));
            assert!(text.contains("no '*** End Patch' line"), "{label}: {text}");
            let written = std::fs::read_to_string(dir.path().join("a.txt")).expect("read");
            assert_eq!(written, "one\nTWO\n", "{label}");
        }
    }

    /// With no end line, the patch's last line is a body line: the repair
    /// that supplies the end line keeps that line's trailing whitespace (a
    /// Markdown hard line break here) instead of trimming it away.
    #[tokio::test]
    async fn a_supplied_end_line_keeps_the_last_line_s_trailing_whitespace() {
        let patch = "\n*** Begin Patch\n*** Update File: a.md\n@@\n one\n-two\n+two  \n+three  ";
        let (out, dir) = run_patch(&[("a.md", "one\ntwo\n")], patch).await;
        let text = out.expect("applies");
        assert!(text.contains("no '*** End Patch' line"), "{text}");
        assert_eq!(read(&dir, "a.md"), "one\ntwo  \nthree  \n");
    }

    /// Round evidence (conflicts-tier2, greenfield): `*** Delete File: x`
    /// then `*** Add File: x` was refused 24 times as one path in two hunks.
    #[tokio::test]
    async fn a_delete_and_an_add_of_one_path_is_a_replace_with_a_note() {
        for order in [
            "*** Delete File: a.txt\n*** Add File: a.txt\n+fresh",
            "*** Add File: a.txt\n+fresh\n*** Delete File: a.txt",
        ] {
            let (out, dir) = run_patch(
                &[("a.txt", "one\ntwo\n")],
                wrap(order)["patch"].as_str().expect("patch"),
            )
            .await;
            let text = out.unwrap_or_else(|e| panic!("{order}: {e}"));
            assert!(text.contains("deleted and added a.txt"), "{text}");
            assert!(text.contains("A a.txt"), "{text}");
            assert!(!text.contains("D a.txt"), "{text}");
            let written = std::fs::read_to_string(dir.path().join("a.txt")).expect("read");
            assert_eq!(written, "fresh\n");
        }
        // The same path in a delete and an update is still refused.
        let (out, _) = run_patch(
            &[("a.txt", "one\n")],
            wrap("*** Delete File: a.txt\n*** Update File: a.txt\n@@\n-one\n+x")["patch"]
                .as_str()
                .expect("patch"),
        )
        .await;
        assert!(out.expect_err("refused").contains("more than one hunk"));
    }

    /// Round evidence (round20, loyalty): `@@-//! Contract: x` and `@@! Contract: x`
    /// were refused as unexpected lines. The marker with no space reads as
    /// `@@ text`, and one stray `-` or `+` on the text is dropped only when the
    /// file has the text without it.
    #[tokio::test]
    async fn an_at_marker_glued_to_its_text_is_read_as_a_context_line() {
        let file = "//! Contract: x
//! more
fn a() {
    old();
}
";
        for (marker, expect_sign_note) in [
            ("@@-//! Contract: x", true),
            ("@@+//! Contract: x", true),
            ("@@//! Contract: x", false),
            ("@@ //! Contract: x", false),
        ] {
            let patch = format!(
                "*** Begin Patch\n*** Update File: l.rs\n{marker}\n //! more\n-fn a() {{\n+fn b() {{\n*** End Patch"
            );
            let (out, dir) = run_patch(&[("l.rs", file)], &patch).await;
            let text = out.unwrap_or_else(|e| panic!("{marker}: {e}"));
            assert_eq!(
                std::fs::read_to_string(dir.path().join("l.rs")).expect("read"),
                "//! Contract: x\n//! more\nfn b() {\n    old();\n}\n",
                "{marker}"
            );
            assert_eq!(
                text.contains("no space before its text"),
                !marker.contains("@@ "),
                "{marker}: {text}"
            );
            assert_eq!(
                text.contains("stray diff sign"),
                expect_sign_note,
                "{marker}: {text}"
            );
        }
        let file = "! Contract: y\nfn a() {}\n";
        let (out, dir) = run_patch(
            &[("l.rs", file)],
            "*** Begin Patch\n*** Update File: l.rs\n@@! Contract: y\n-fn a() {}\n+fn b() {}\n*** End Patch",
        )
        .await;
        out.expect("applies");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("l.rs")).expect("read"),
            "! Contract: y\nfn b() {}\n"
        );
    }

    /// The text as written wins over the sign-stripped text.
    #[tokio::test]
    async fn a_context_with_a_leading_sign_that_is_in_the_file_is_not_stripped() {
        let file = "x\n-- sql comment\ny\n-- sql comment\nz\n";
        let (out, dir) = run_patch(
            &[("q.sql", file)],
            "*** Begin Patch\n*** Update File: q.sql\n@@ -- sql comment\n y\n--- sql comment\n+-- other\n*** End Patch",
        )
        .await;
        let text = out.expect("applies");
        assert!(!text.contains("stray diff sign"), "{text}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("q.sql")).expect("read"),
            "x\n-- sql comment\ny\n-- other\nz\n"
        );
    }

    #[tokio::test]
    async fn an_ambiguous_at_marker_is_still_refused() {
        let file = "a\nb\n";
        // `@@@` is not a context marker; a signed context absent from the file
        // under either reading is refused; so is a glued unified-diff header
        // whose text is not in the file.
        for (marker, want) in [
            ("@@@ a", "Unexpected line found"),
            ("@@-zzz", "Failed to find context '-zzz'"),
            ("@@-1,2 +1,2 @@ zzz", "Failed to find context 'zzz'"),
        ] {
            let patch = format!(
                "*** Begin Patch\n*** Update File: f.txt\n{marker}\n a\n-b\n+c\n*** End Patch"
            );
            let (out, dir) = run_patch(&[("f.txt", file)], &patch).await;
            let err = out.expect_err(marker);
            assert!(err.contains(want), "{marker}: {err}");
            assert_eq!(
                std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
                file
            );
        }
    }

    /// The whole-file route the repeated-failure hint names must itself apply.
    #[tokio::test]
    async fn the_whole_file_hint_skeleton_replaces_the_file() {
        let hint = whole_file_hint("src/lib.rs", false);
        assert!(!hint.contains("replace_in_file"), "{hint}");
        let with = whole_file_hint("src/lib.rs", true);
        assert!(with.contains("call replace_in_file"), "{with}");
        assert!(with.starts_with(&hint[..hint.find("*** Begin Patch").expect("skeleton")]));
        let skeleton = &hint[hint.find("*** Begin Patch").expect("skeleton")..];
        assert_eq!(
            skeleton,
            "*** Begin Patch\n*** Delete File: src/lib.rs\n*** Add File: src/lib.rs\n+full new content\n*** End Patch"
        );
        let (out, dir) = run_patch(&[("src/lib.rs", "old\nlines\n")], skeleton).await;
        out.expect("applies");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read"),
            "full new content\n"
        );
    }

    /// Round evidence (pricing_engine): the whole-file route was followed, but
    /// the Add File body was written without `+`. The body is read as content.
    #[tokio::test]
    async fn an_unprefixed_add_file_body_after_a_delete_replaces_the_file() {
        let patch = "*** Begin Patch\n*** Delete File: src/lib.rs\n*** Add File: src/lib.rs\n\
                     //! Contract 5 of 6\nuse a::B;\n\nfn price() {\n    1\n}\n*** End Patch";
        let (out, dir) = run_patch(&[("src/lib.rs", "old\nlines\n")], patch).await;
        let text = out.expect("applies");
        assert!(text.contains("no line starting with '+'"), "{text}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read"),
            "//! Contract 5 of 6\nuse a::B;\n\nfn price() {\n    1\n}\n"
        );
    }

    #[tokio::test]
    async fn an_unprefixed_add_file_alone_creates_the_file_and_drops_trailing_blanks() {
        let patch = "*** Begin Patch\n*** Add File: n.txt\none\n\ntwo\n\n*** Add File: m.txt\n+x\n*** End Patch";
        let (out, dir) = run_patch(&[], patch).await;
        out.expect("applies");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("n.txt")).expect("read"),
            "one\n\ntwo\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("m.txt")).expect("read"),
            "x\n"
        );
    }

    #[tokio::test]
    async fn a_mixed_add_file_body_is_still_refused() {
        let patch = "*** Begin Patch\n*** Add File: n.txt\n+one\ntwo\n*** End Patch";
        let (out, dir) = run_patch(&[], patch).await;
        let err = out.expect_err("refused");
        assert!(err.contains("does not start with '+'"), "{err}");
        assert!(!dir.path().join("n.txt").exists());
    }

    /// Git's `\ No newline at end of file` line sets the final-newline state
    /// when it follows the last `+` or `-` line of a hunk that ends the file,
    /// and is dropped with a note anywhere else.
    #[tokio::test]
    async fn a_git_no_newline_line_sets_the_final_newline_state_or_is_noted() {
        let marker = "\\ No newline at end of file";
        // After the last `+` line: the result and the new file have no final
        // newline. The marker after `-b` is mid-hunk and changes nothing.
        let patch = format!(
            "*** Begin Patch\n*** Update File: f.txt\n@@\n a\n-b\n{marker}\n+c\n{marker}\n\
             *** Add File: n.txt\n+one\n{marker}\n*** End Patch"
        );
        let (out, dir) = run_patch(&[("f.txt", "a\nb\n")], &patch).await;
        let text = out.expect("applies");
        assert!(!text.contains("is a git diff marker"), "{text}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
            "a\nc"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("n.txt")).expect("read"),
            "one"
        );
        // After the last `-` line, with none after the last `+` line: the
        // result gains a final newline.
        let patch = format!(
            "*** Begin Patch\n*** Update File: f.txt\n@@\n a\n-b\n{marker}\n+c\n*** End Patch"
        );
        let (out, dir) = run_patch(&[("f.txt", "a\nb")], &patch).await;
        out.expect("applies");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
            "a\nc\n"
        );
        // A marker that follows a context line is dropped with a note.
        let patch = format!(
            "*** Begin Patch\n*** Update File: f.txt\n@@\n a\n{marker}\n-b\n+c\n*** End Patch"
        );
        let (out, dir) = run_patch(&[("f.txt", "a\nb\n")], &patch).await;
        let text = out.expect("applies");
        assert!(text.contains("is a git diff marker"), "{text}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
            "a\nc\n"
        );
        // A hunk that does not reach the end of the file keeps the final
        // newline state, and the result says so.
        let patch = format!(
            "*** Begin Patch\n*** Update File: f.txt\n@@\n-a\n+z\n{marker}\n b\n*** End Patch"
        );
        let (out, dir) = run_patch(&[("f.txt", "a\nb\nc\n")], &patch).await;
        let text = out.expect("applies");
        assert!(
            text.contains("does not end at the end of the file"),
            "{text}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
            "z\nb\nc\n"
        );
        // A content line that carries the text keeps it.
        let patch = format!("*** Begin Patch\n*** Add File: m.txt\n+{marker}\n*** End Patch");
        let (out, dir) = run_patch(&[], &patch).await;
        let text = out.expect("applies");
        assert!(!text.contains("git diff marker"), "{text}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("m.txt")).expect("read"),
            format!("{marker}\n")
        );
    }

    /// A blank line without `+` between `+` lines is an empty line of the new
    /// file. A truly mixed body, or a blank line outside the `+` lines, stays
    /// refused.
    #[tokio::test]
    async fn a_blank_line_between_plus_lines_of_an_add_file_is_an_empty_line() {
        let patch = "*** Begin Patch\n*** Add File: n.txt\n+one\n\n+two\n*** End Patch";
        let (out, dir) = run_patch(&[], patch).await;
        let text = out.expect("applies");
        assert!(text.contains("blank line with no '+'"), "{text}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("n.txt")).expect("read"),
            "one\n\ntwo\n"
        );
        for refused in [
            "*** Add File: n.txt\n+one\n\nbare\n+two",
            "*** Add File: n.txt\n+one\n+two\n",
            "*** Add File: n.txt\n\n+one\n+two",
        ] {
            let patch = format!("*** Begin Patch\n{refused}\n*** End Patch");
            let (out, dir) = run_patch(&[], &patch).await;
            out.expect_err(refused);
            assert!(!dir.path().join("n.txt").exists(), "{refused}");
        }
    }

    /// A line with no prefix in an Update hunk is a context line, and must
    /// match the file; a marker-like line is still refused.
    #[tokio::test]
    async fn a_bare_line_in_an_update_hunk_is_a_context_line() {
        let file = "fn a() {\none();\ntwo();\n}\n";
        let patch = "*** Begin Patch\n*** Update File: f.rs\n@@\nfn a() {\none();\n-two();\n+three();\n}\n*** End Patch";
        let (out, dir) = run_patch(&[("f.rs", file)], patch).await;
        let text = out.expect("applies");
        assert!(text.contains("no ' ', '+' or '-' prefix"), "{text}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.rs")).expect("read"),
            "fn a() {\none();\nthree();\n}\n"
        );
        // A bare line that is not in the file fails at the match, file intact.
        let patch = "*** Begin Patch\n*** Update File: f.rs\n@@\nfn zzz() {\n-two();\n+three();\n*** End Patch";
        let (out, dir) = run_patch(&[("f.rs", file)], patch).await;
        let err = out.expect_err("no match");
        assert!(err.contains("Failed to find"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.rs")).expect("read"),
            file
        );
        // Marker-like lines are not context.
        for bad in ["@@@ x", "*** Bogus: x"] {
            let patch = format!(
                "*** Begin Patch\n*** Update File: f.rs\n@@\n one();\n{bad}\n-two();\n*** End Patch"
            );
            let (out, _dir) = run_patch(&[("f.rs", file)], &patch).await;
            out.expect_err(bad);
        }
    }

    /// A chunk that mixes ` `-prefixed and bare lines is refused: a bare line
    /// there is likely a lost `-`, and reading it as context would keep a line
    /// the patch meant to delete. A later all-bare chunk is still repaired.
    #[tokio::test]
    async fn a_bare_line_in_a_chunk_with_spaced_context_is_refused() {
        let file = "a\nb\nd\n";
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@\n a\nb\n+c\n d\n*** End Patch";
        let (out, dir) = run_patch(&[("f.txt", file)], patch).await;
        out.expect_err("refused");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
            file
        );
        let file = "a\nb\nd\ne\nf\ng\n";
        let patch =
            "*** Begin Patch\n*** Update File: f.txt\n@@\n a\n+x\n@@\ne\n-f\n+F\ng\n*** End Patch";
        let (out, dir) = run_patch(&[("f.txt", file)], patch).await;
        out.expect("second chunk is all bare");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
            "a\nx\nb\nd\ne\nF\ng\n"
        );
    }

    /// A `*** Move to:` line does not end the repairs of its Update section.
    #[tokio::test]
    async fn a_bare_line_after_a_move_to_line_is_still_context() {
        let file = "one\ntwo\nthree\n";
        let patch = "*** Begin Patch\n*** Update File: f.txt\n*** Move to: g.txt\n@@\none\n-two\n+2\nthree\n*** End Patch";
        let (out, dir) = run_patch(&[("f.txt", file)], patch).await;
        let text = out.expect("applies");
        assert!(text.contains("no ' ', '+' or '-' prefix"), "{text}");
        assert!(!dir.path().join("f.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("g.txt")).expect("read"),
            "one\n2\nthree\n"
        );
    }

    #[tokio::test]
    async fn a_prefixed_add_file_body_is_unchanged_and_carries_no_raw_note() {
        let patch = "*** Begin Patch\n*** Add File: n.txt\n+one\n+\n+++two\n*** End Patch";
        let (out, dir) = run_patch(&[], patch).await;
        let text = out.expect("applies");
        assert!(!text.contains("no line starting with '+'"), "{text}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("n.txt")).expect("read"),
            "one\n\n++two\n"
        );
    }

    #[tokio::test]
    async fn a_raw_add_file_line_starting_with_stars_ends_the_section_and_is_refused() {
        let patch = "*** Begin Patch\n*** Add File: n.txt\none\n*** two\n*** End Patch";
        let (out, dir) = run_patch(&[], patch).await;
        out.expect_err("refused");
        assert!(!dir.path().join("n.txt").exists());
    }

    /// Round evidence (pricing_engine): `@@ -1,4 +1,4 @@` and
    /// `@@ -14,28 +14,4 @@ use tax_us::TaxUs;` headed update chunks.
    #[tokio::test]
    async fn a_unified_diff_hunk_header_is_stripped_and_the_result_says_so() {
        let file = "use iface::{Order, Receipt};\nuse discount_flat::FlatDiscount;\nuse tax_us::TaxUs;\n\nfn price() {\n    old();\n}\n";
        let alone = wrap(
            "*** Update File: p.rs\n@@ -1,4 +1,4 @@\n-use iface::{Order, Receipt};\n+use iface::{Order, Receipt, Discount};\n use discount_flat::FlatDiscount;",
        );
        let (out, dir) =
            run_patch(&[("p.rs", file)], alone["patch"].as_str().expect("patch")).await;
        let text = out.expect("applies");
        assert!(text.contains("unified-diff header"), "{text}");
        let written = std::fs::read_to_string(dir.path().join("p.rs")).expect("read");
        assert!(
            written.starts_with("use iface::{Order, Receipt, Discount};\n"),
            "{written}"
        );
        // The text after the header is the context line.
        let sectioned = wrap(
            "*** Update File: p.rs\n@@ -14,28 +14,4 @@ use tax_us::TaxUs;\n \n fn price() {\n-    old();\n+    new();",
        );
        let (out, dir) = run_patch(
            &[("p.rs", file)],
            sectioned["patch"].as_str().expect("patch"),
        )
        .await;
        let text = out.expect("applies");
        assert!(text.contains("unified-diff header"), "{text}");
        let written = std::fs::read_to_string(dir.path().join("p.rs")).expect("read");
        assert!(written.contains("    new();"), "{written}");
        // A plain `@@ text` line carries no note.
        let plain = wrap("*** Update File: p.rs\n@@ fn price() {\n-    old();\n+    new();");
        let (out, _) = run_patch(&[("p.rs", file)], plain["patch"].as_str().expect("patch")).await;
        assert!(!out.expect("applies").contains("unified-diff"));
    }

    #[test]
    fn unified_header_detection_is_exact() {
        assert_eq!(unified_header_context("@@ -1 +1 @@"), Some(""));
        assert_eq!(
            unified_header_context("@@ -1,4 +1,5 @@ fn x()"),
            Some("fn x()")
        );
        for not in [
            "@@ fn x()",
            "@@ -a,b +c,d @@",
            "@@ -1,4 +1,4 @@fn x()",
            "@@ -1,2,3 +1 @@",
            "@@ - +1 @@",
            "@@",
        ] {
            assert_eq!(unified_header_context(not), None, "{not}");
        }
    }

    /// Round evidence (greenfield): an Update File of only `+` lines was
    /// applied as an append, three times, and each said "Success".
    #[tokio::test]
    async fn an_anchorless_addition_through_the_tool_is_refused_and_writes_nothing() {
        let seed = "[workspace]\nmembers = [\"a\"]\n";
        let (out, dir) = run_patch(
            &[("Cargo.toml", seed)],
            wrap("*** Update File: Cargo.toml\n+[workspace]\n+members = [\"a\"]")["patch"]
                .as_str()
                .expect("patch"),
        )
        .await;
        let err = out.expect_err("refused");
        assert!(err.contains("only `+` lines"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Cargo.toml")).expect("read"),
            seed
        );
    }

    #[tokio::test]
    async fn a_tolerant_match_is_stated_in_the_result() {
        let (out, dir) = run_patch(
            &[("a.rs", "\tfn a() {\n\t\told();\n\t}\n")],
            wrap("*** Update File: a.rs\n@@\n fn a() {\n-\told();\n+\tnew();\n }")["patch"]
                .as_str()
                .expect("patch"),
        )
        .await;
        let text = out.expect("applies");
        assert!(text.contains("only after ignoring whitespace"), "{text}");
        assert!(
            text.contains("1 tab less indentation than the file"),
            "{text}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).expect("read"),
            "\tfn a() {\n\t\tnew();\n\t}\n"
        );
    }

    fn read(dir: &tempfile::TempDir, name: &str) -> String {
        std::fs::read_to_string(dir.path().join(name)).expect("read")
    }

    /// Recorded evidence: a worker's patch to a Python stub module wrote
    /// every unchanged line with one extra space after its ` ` prefix, while
    /// its `-` and `+` lines were exact. It matched only after ignoring
    /// whitespace and was written back as the patch's text, so line 1 of the
    /// module gained a leading space (`IndentationError: unexpected indent`)
    /// and the call reported success. The matched lines show two offsets
    /// (one extra space on the context lines, none on the `-` lines), so the
    /// patch is now refused, the file is unchanged, and the refusal names the
    /// slip.
    #[tokio::test]
    async fn the_recorded_extra_space_patch_is_refused_and_the_module_is_unchanged() {
        let module = include_str!("testdata/containers.py");
        let patch = include_str!("testdata/containers_extra_space.patch");
        let (out, dir) = run_patch(&[("shapes/containers.py", module)], patch).await;
        let err = out.expect_err("refused");
        assert!(
            err.contains("not shifted from the file by one amount"),
            "{err}"
        );
        assert!(err.contains("not applied"), "{err}");
        assert!(
            err.contains("Every unchanged line has 1 space more indentation"),
            "{err}"
        );
        assert_eq!(read(&dir, "shapes/containers.py"), module);
    }

    /// A Python method body sent at the wrong depth (the patch has the method
    /// at column 0, the file at column 4): context lines stay as the file has
    /// them, added lines move 4 spaces right, and the module still parses.
    #[tokio::test]
    async fn a_python_patch_at_a_different_indentation_is_written_at_the_file_s() {
        let module = "class Stack:\n    def __init__(self):\n        raise NotImplementedError\n\n    def size(self):\n        return 0\n";
        let patch = "*** Begin Patch\n*** Update File: m.py\n@@\n def __init__(self):\n-    raise NotImplementedError\n+    self._items = []\n+\n+def push(self, item):\n+    self._items.append(item)\n \n def size(self):\n*** End Patch";
        let (out, dir) = run_patch(&[("m.py", module)], patch).await;
        let text = out.expect("applies");
        assert!(
            text.contains("4 spaces less indentation than the file"),
            "{text}"
        );
        assert!(text.contains("3 added lines were re-indented"), "{text}");
        let written = read(&dir, "m.py");
        assert_eq!(
            written,
            "class Stack:\n    def __init__(self):\n        self._items = []\n\n    def push(self, item):\n        self._items.append(item)\n\n    def size(self):\n        return 0\n"
        );
        assert_eq!(syntax::python_indentation_error(&written), None);
    }

    /// gofmt indents Go with tabs, and a model writes the patch with four
    /// spaces per level. Before, the offset grew with each level and the
    /// patch was refused, so every nested Go edit needed exact tabs; a
    /// Makefile recipe must start with a tab or make refuses it.
    #[tokio::test]
    async fn a_go_or_makefile_patch_written_with_spaces_gets_the_file_s_tabs() {
        let go = "package v\n\nfunc Dot(a, b []int) int {\n\tsum := 0\n\tfor i := range a {\n\t\tsum += a[i]\n\t}\n\treturn sum\n}\n";
        let patch = "*** Begin Patch\n*** Update File: v.go\n@@\n func Dot(a, b []int) int {\n     sum := 0\n     for i := range a {\n-        sum += a[i]\n+        sum += a[i] * b[i]\n     }\n     return sum\n*** End Patch";
        let (out, dir) = run_patch(&[("v.go", go)], patch).await;
        let text = out.expect("applies");
        assert!(text.contains("4 spaces for each tab"), "{text}");
        assert_eq!(
            read(&dir, "v.go"),
            "package v\n\nfunc Dot(a, b []int) int {\n\tsum := 0\n\tfor i := range a {\n\t\tsum += a[i] * b[i]\n\t}\n\treturn sum\n}\n"
        );
        let make = "test:\n\tgo vet ./...\n\tgo test ./...\n";
        let patch = "*** Begin Patch\n*** Update File: Makefile\n@@\n test:\n     go vet ./...\n+    gofmt -l .\n     go test ./...\n*** End Patch";
        let (out, dir) = run_patch(&[("Makefile", make)], patch).await;
        out.expect("applies");
        assert_eq!(
            read(&dir, "Makefile"),
            "test:\n\tgo vet ./...\n\tgofmt -l .\n\tgo test ./...\n"
        );
    }

    /// YAML: a nested mapping edited with the patch's lines 2 spaces
    /// shallower than the file's; the new key lands at the file's depth.
    #[tokio::test]
    async fn a_yaml_patch_at_a_different_indentation_is_written_at_the_file_s() {
        let seed = "jobs:\n  build:\n    steps:\n      - run: make\n    env:\n      A: 1\n";
        let patch = "*** Begin Patch\n*** Update File: ci.yml\n@@\n   env:\n     A: 1\n+    B: 2\n*** End Patch";
        let (out, dir) = run_patch(&[("ci.yml", seed)], patch).await;
        let text = out.expect("applies");
        assert!(text.contains("re-indented"), "{text}");
        assert_eq!(
            read(&dir, "ci.yml"),
            "jobs:\n  build:\n    steps:\n      - run: make\n    env:\n      A: 1\n      B: 2\n"
        );
    }

    /// One context line at the file's indentation and another shifted: no
    /// one offset, so the `+` lines' indentation cannot be known. Refused,
    /// nothing written.
    #[tokio::test]
    async fn an_inconsistent_indentation_offset_is_refused_and_writes_nothing() {
        let seed = "a:\n  b: 1\n  c: 2\n";
        let patch = "*** Begin Patch\n*** Update File: x.yaml\n@@\n a:\n-    b: 1\n+    b: 3\n*** End Patch";
        let (out, dir) = run_patch(&[("x.yaml", seed)], patch).await;
        let err = out.expect_err("refused");
        assert!(
            err.contains("not shifted from the file by one amount"),
            "{err}"
        );
        assert_eq!(read(&dir, "x.yaml"), seed);
    }

    /// The parse check behind the re-indent: one offset is measured, but an
    /// added line is itself at a depth the file has no level for, so the
    /// moved result is not valid Python. Refused before it is written.
    #[tokio::test]
    async fn a_tolerant_python_edit_that_no_longer_parses_is_refused() {
        let seed = "def f():\n    return 1\n";
        // Patch is 2 spaces deeper than the file throughout; the added line
        // has 3 spaces, so 1 space after the move: an unmatched unindent.
        let patch = "*** Begin Patch\n*** Update File: f.py\n@@\n   def f():\n-      return 1\n+      x = 1\n+   y = 2\n*** End Patch";
        let (out, dir) = run_patch(&[("f.py", seed)], patch).await;
        let err = out.expect_err("refused");
        assert!(err.contains("no longer valid Python"), "{err}");
        assert!(err.contains("line 3"), "{err}");
        assert_eq!(read(&dir, "f.py"), seed);
    }
}
