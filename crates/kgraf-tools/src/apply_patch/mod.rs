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
mod replace;
mod seek_sequence;
mod streaming_parser;

pub use parser::parse_patch;
pub use parser::{Hunk, ParseError, UpdateFileChunk};
pub use protected::ProtectedApplyPatch;

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
    "being added. Every body line needs one of these three prefixes -- a bare line ",
    "with no prefix is invalid, even if it matches the file exactly. Include a few ",
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

/// The leniency stated in the result of a call whose patch had no envelope.
const ENVELOPE_NOTE: &str =
    "note: the patch had no '*** Begin Patch' line; it was applied as if it had";

/// `patch` with a missing `*** Begin Patch` / `*** End Patch` envelope
/// supplied, and whether it was changed. Models often send the hunks alone.
/// Only a patch whose first non-blank line is a file header
/// (`*** Add File:`, `*** Update File:`, `*** Delete File:`) is repaired, so
/// the intent is certain; a missing end marker is added with it. Anything else
/// (a patch that starts some other way, or one that has its begin line and
/// lacks its end line) goes to the parser unchanged and is refused there.
fn with_envelope(patch: &str) -> (std::borrow::Cow<'_, str>, bool) {
    let first = patch.lines().map(str::trim).find(|l| !l.is_empty());
    let headed = first.is_some_and(|l| {
        ["*** Add File:", "*** Update File:", "*** Delete File:"]
            .iter()
            .any(|h| l.starts_with(h))
    });
    if !headed {
        return (std::borrow::Cow::Borrowed(patch), false);
    }
    let body = patch.trim();
    let end = if body.lines().last().map(str::trim) == Some(parser::END_PATCH_MARKER) {
        String::new()
    } else {
        format!("\n{}", parser::END_PATCH_MARKER)
    };
    (
        std::borrow::Cow::Owned(format!("{}\n{body}{end}", parser::BEGIN_PATCH_MARKER)),
        true,
    )
}

/// Whether `input`'s patch is one [`with_envelope`] repairs.
fn envelope_supplied(input: &Value) -> bool {
    input
        .get("patch")
        .and_then(Value::as_str)
        .is_some_and(|p| with_envelope(p).1)
}

/// `result` with [`ENVELOPE_NOTE`] added to a completed call, when `supplied`.
fn noting_envelope(
    result: Result<ToolOutput, ToolError>,
    supplied: bool,
) -> Result<ToolOutput, ToolError> {
    match result {
        Ok(mut out) if supplied => {
            out.parts.push(crate::OutputPart::Text {
                text: crate::UntrustedText::new(ENVELOPE_NOTE),
            });
            Ok(out)
        }
        other => other,
    }
}

fn parse(input: &Value) -> Result<Vec<Hunk>, ToolError> {
    let (text, _) = with_envelope(patch_text(input)?);
    let hunks = parse_patch(&text).map_err(|e| ToolError::InvalidInput {
        tool: "apply_patch".into(),
        reason: e.to_string(),
    })?;
    if hunks.len() > MAX_HUNKS {
        return Err(ToolError::InvalidInput {
            tool: "apply_patch".into(),
            reason: format!(
                "patch names {} hunks, over the {MAX_HUNKS} cap",
                hunks.len()
            ),
        });
    }
    Ok(hunks)
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
        let hunks = parse(input)?;
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
        let hunks = parse(&input)?;
        if hunks.is_empty() {
            return Ok(ToolOutput::error("the patch made no changes"));
        }
        let supplied = envelope_supplied(&input);
        let root = ctx.workspace_root.clone();
        let cancel = ctx.cancel.clone();
        let task = tokio::task::spawn_blocking(move || apply_all(&hunks, &root, &cancel));
        let result = tokio::select! {
            _ = ctx.cancel.cancelled() => Err(ToolError::Cancelled { tool: "apply_patch".into() }),
            result = task => result.map_err(|e| internal(format!("apply task failed: {e}")))?,
        };
        noting_envelope(result, supplied)
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
    apply_all_names(hunks, root, cancel, fs_safety::Names::Plain)
}

/// [`apply_all`] under the write guard's names policy `names`: the protected
/// apply admits `.gitattributes`, `.gitmodules` and `.githooks` in Bypass.
fn apply_all_names(
    hunks: &[Hunk],
    root: &Path,
    cancel: &CancellationToken,
    names: fs_safety::Names,
) -> Result<ToolOutput, ToolError> {
    refuse_repeated_paths(hunks, lexical)?;
    let dir = fs_safety::open_workspace("apply_patch", root)?;
    // Again with links followed: a hunk on a link and a hunk on its target
    // name one file, and both would plan from its old content.
    refuse_repeated_paths(hunks, |path| {
        fs_safety::resolve_links("apply_patch", &dir, path)
    })?;
    // The write guard runs on every written path before any hunk is planned:
    // planning reads through the read guard, which lets `.gitmodules` through,
    // so without this a protected path late in the patch would be refused only
    // at commit, after earlier hunks had written.
    for hunk in hunks {
        for path in written_paths(hunk)? {
            fs_safety::guard_write_names("apply_patch", &dir, &path, names)?;
        }
    }

    let mut planned = Vec::with_capacity(hunks.len());
    for hunk in hunks {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled {
                tool: "apply_patch".into(),
            });
        }
        planned.push(plan_one(hunk, &dir)?);
    }

    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    for change in planned {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled {
                tool: "apply_patch".into(),
            });
        }
        commit_one(change, &dir, names, &mut added, &mut modified, &mut deleted)?;
    }
    Ok(ToolOutput::text(summary(&added, &modified, &deleted)))
}

fn plan_one(hunk: &Hunk, dir: &Dir) -> Result<Planned, ToolError> {
    match hunk {
        Hunk::AddFile { path, contents } => {
            let path = normalize_hunk_path(path)?;
            // An add over an existing hard-linked file would be refused at
            // commit, after earlier hunks had written; refuse it here.
            fs_safety::refuse_if_hard_linked("apply_patch", dir, &path)?;
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
            let new_contents =
                replace::derive_new_contents(&original, &path.display().to_string(), chunks)
                    .map_err(internal)?;
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
    change: Planned,
    dir: &Dir,
    names: fs_safety::Names,
    added: &mut Vec<String>,
    modified: &mut Vec<String>,
    deleted: &mut Vec<String>,
) -> Result<(), ToolError> {
    match change {
        Planned::Add { path, contents } => {
            fs_safety::write_confined_names("apply_patch", dir, &path, contents.as_bytes(), names)?;
            added.push(display(&path));
        }
        Planned::Delete { path } => {
            fs_safety::remove_file_confined_names("apply_patch", dir, &path, names)?;
            deleted.push(display(&path));
        }
        Planned::Write { path, contents } => {
            fs_safety::write_confined_names("apply_patch", dir, &path, contents.as_bytes(), names)?;
            modified.push(display(&path));
        }
        Planned::Move { from, to, contents } => {
            fs_safety::write_confined_names("apply_patch", dir, &to, contents.as_bytes(), names)?;
            fs_safety::remove_file_confined_names("apply_patch", dir, &from, names)?;
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
            "*** Begin Patch\n*** Add File: a.txt\n+one",
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
                "delete then add",
                "*** Delete File: a.txt\n*** Add File: a.txt\n+fresh",
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
}
