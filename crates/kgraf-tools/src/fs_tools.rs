//! Read-only worker tools: read a file, list a directory, search file contents.
//!
//! Novel code (nothing in the evidence directory or the reuse
//! candidates covers a plain read/list/search triad; these are a few dozen
//! lines each over [`crate::fs_safety`], not worth a separate dependency).
//! Every path goes through [`crate::fs_safety`], not `std::fs`: a lexical
//! escape check alone (what this module used at first)
//! cannot see a symlink planted inside the workspace pointing outside it.

use crate::fs_safety::{self, AllowedHardlinkReads, MAX_READ_BYTES};
use crate::{
    Access, CallPermit, OutputPart, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput,
};
use serde_json::{Value, json};
use std::path::Path;

/// An absent `path`, or an empty or blank one, is `default`: models send
/// `{"path": ""}` for the workspace root, which the descriptions call the
/// default, and opening the empty path failed with "cannot open :".
fn path_field<'a>(input: &'a Value, default: &'a str) -> Result<&'a Path, ToolError> {
    match input.get("path") {
        None => Ok(Path::new(default)),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(Path::new(default)),
        Some(v) => v
            .as_str()
            .map(Path::new)
            .ok_or_else(|| ToolError::InvalidInput {
                tool: "path".into(),
                reason: "expected a string `path` field".into(),
            }),
    }
}

/// Reads one file's contents.
///
/// A hard-linked file is refused unless its path is in the tool's
/// [`AllowedHardlinkReads`] (empty by default; see `fs_safety`).
#[derive(Debug, Default, Clone)]
pub struct ReadFileTool {
    hard_links: AllowedHardlinkReads,
}

impl ReadFileTool {
    /// A read tool that may read hard-linked files under `allowed`.
    pub fn with_hardlink_reads(allowed: AllowedHardlinkReads) -> Self {
        Self {
            hard_links: allowed,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".into(),
            description: "Read one file's contents, as a path relative to the workspace root."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false,
            }),
        }
    }

    /// No argument-scoped permission pattern applies to a read-only path
    /// tool: its `access()` declaration is already the precise gate, and a
    /// path is not a command line a prefix rule could meaningfully match
    /// against.
    fn permission_subject(&self, _input: &Value) -> Option<String> {
        None
    }

    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        let path = path_field(input, ".")?;
        Access::none()
            .read_path(path)
            .map_err(|source| ToolError::Access {
                tool: "read_file".into(),
                source,
            })
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let path = path_field(&input, ".")?;
        let dir = fs_safety::open_workspace("read_file", &ctx.workspace_root)?;
        let bytes = fs_safety::read_capped(
            "read_file",
            &dir,
            path,
            MAX_READ_BYTES,
            self.hard_links.permits(path),
        )?;
        register_if_secrets_file(path, &bytes);
        let mime = mime_guess_from_extension(path);
        Ok(ToolOutput {
            parts: vec![OutputPart::from_bytes(&bytes, mime.as_deref())],
            is_error: false,
        })
    }
}

/// Decision 0071: a worker read a file named like a secrets file (`.env`,
/// `.env.*`, `*.env`, `secrets.json`, ...; `kgraf_secrets::is_secrets_file_name`),
/// so its credential values join the record scrubber before the result is
/// taken in: transcripts, traces and run records lose them. The model-facing
/// path is unchanged: the worker reads the file it asked for, values
/// included. Called before the bytes become a tool result.
pub(crate) fn register_if_secrets_file(path: &Path, bytes: &[u8]) {
    let named = path
        .file_name()
        .is_some_and(|n| kgraf_secrets::is_secrets_file_name(&n.to_string_lossy()));
    if named && let Ok(text) = std::str::from_utf8(bytes) {
        kgraf_secrets::register_records_only(text);
    }
}

/// A handful of extensions worth naming in a binary summary. Not a general MIME
/// database (no dependency added for a label the model never acts on).
fn mime_guess_from_extension(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        _ => return None,
    };
    Some(mime.to_string())
}

/// Lists the direct entries of one directory, sorted, one per line, with a
/// trailing `/` on directories.
#[derive(Debug, Default, Clone, Copy)]
pub struct ListDirectoryTool;

#[async_trait::async_trait]
impl Tool for ListDirectoryTool {
    fn name(&self) -> &str {
        "list_directory"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_directory".into(),
            description: "List one directory's direct entries, relative to the workspace root. \
                Defaults to the workspace root."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "additionalProperties": false,
            }),
        }
    }

    /// No argument-scoped permission pattern applies to a read-only path
    /// tool: its `access()` declaration is already the precise gate, and a
    /// path is not a command line a prefix rule could meaningfully match
    /// against.
    fn permission_subject(&self, _input: &Value) -> Option<String> {
        None
    }

    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        let path = path_field(input, ".")?;
        Access::none()
            .read_path(path)
            .map_err(|source| ToolError::Access {
                tool: "list_directory".into(),
                source,
            })
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let path = path_field(&input, ".")?;
        let dir = fs_safety::open_workspace("list_directory", &ctx.workspace_root)?;
        // `open_dir` resolves `path` confined to the workspace (an escaping or
        // outside-pointing symlink is refused by the same syscall that opens
        // it), then lists that directory's own entries.
        let sub = fs_safety::open_dir_confined("list_directory", &dir, path)?;
        let mut entries: Vec<String> = sub
            .entries()
            .map_err(|e| ToolError::Internal {
                tool: "list_directory".into(),
                reason: format!("cannot list {}: {e}", path.display()),
            })?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let is_dir = entry.file_type().ok()?.is_dir();
                let name = entry.file_name().to_string_lossy().into_owned();
                Some(if is_dir { format!("{name}/") } else { name })
            })
            .collect();
        entries.sort();
        Ok(ToolOutput::text(entries.join("\n")))
    }
}

const MAX_SEARCH_MATCHES: usize = 200;
/// A file above this size is skipped rather than read in full: search is a
/// scan over many files, not a targeted read, so its per-file budget is
/// smaller than [`MAX_READ_BYTES`] (bound the walk's total cost).
const MAX_SEARCH_FILE_BYTES: u64 = 1024 * 1024;

/// Finds a literal substring across text files under one directory, depth
/// first, alphabetical within a directory. No regex dependency: a literal scan
/// covers the common case, and a fuzzier search is a deliberate later addition
/// (a dependency not added needs a stated reason just as much as one that was).
///
/// A hard-linked file is skipped unless its path is in the tool's
/// [`AllowedHardlinkReads`], and the output says how many were skipped, so a
/// search that found nothing is not mistaken for one that read everything
/// (fail loud).
#[derive(Debug, Default, Clone)]
pub struct SearchFilesTool {
    hard_links: AllowedHardlinkReads,
}

impl SearchFilesTool {
    /// A search tool that may read hard-linked files under `allowed`.
    pub fn with_hardlink_reads(allowed: AllowedHardlinkReads) -> Self {
        Self {
            hard_links: allowed,
        }
    }
}

#[async_trait::async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "search_files".into(),
            description: "Search text files under a directory for a literal substring, \
                returning up to 200 `path:line: text` matches."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "path": {"type": "string"},
                },
                "required": ["query"],
                "additionalProperties": false,
            }),
        }
    }

    /// No argument-scoped permission pattern applies to a read-only path
    /// tool: its `access()` declaration is already the precise gate, and a
    /// path is not a command line a prefix rule could meaningfully match
    /// against.
    fn permission_subject(&self, _input: &Value) -> Option<String> {
        None
    }

    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        let path = path_field(input, ".")?;
        Access::none()
            .read_path(path)
            .map_err(|source| ToolError::Access {
                tool: "search_files".into(),
                source,
            })
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput {
                tool: "search_files".into(),
                reason: "expected a string `query` field".into(),
            })?
            .to_owned();
        let path = path_field(&input, ".")?.to_path_buf();
        let root = ctx.workspace_root.clone();
        let cancel = ctx.cancel.clone();
        let hard_links = self.hard_links.clone();

        // An adversarial tree (many files, or a few enormous ones) makes
        // this a slow, synchronous walk; run it off the async executor thread
        // and give up promptly if the worker is cancelled meanwhile, rather
        // than blocking either on it.
        let walk = tokio::task::spawn_blocking(move || -> Result<Walk, ToolError> {
            let dir = fs_safety::open_workspace("search_files", &root)?;
            let sub = fs_safety::open_dir_confined("search_files", &dir, &path)?;
            let mut walk = Walk {
                query,
                cancel,
                hard_links,
                base: path,
                matches: Vec::new(),
                skipped_hard_links: 0,
                cancelled: false,
            };
            search_dir(&sub, Path::new("."), &mut walk);
            Ok(walk)
        });

        let walk = async {
            walk.await.map_err(|e| ToolError::Internal {
                tool: "search_files".into(),
                reason: format!("search task failed: {e}"),
            })?
        };
        let Walk {
            mut matches,
            skipped_hard_links,
            ..
        } = await_walk(&ctx.cancel, walk).await?;
        matches.truncate(MAX_SEARCH_MATCHES);
        let mut text = if matches.is_empty() {
            "no matches".to_owned()
        } else {
            matches.join("\n")
        };
        if skipped_hard_links > 0 {
            text.push_str(&format!(
                "\n({skipped_hard_links} file(s) not searched: more than one hard link, \
                 which can name a file outside the workspace)"
            ));
        }
        Ok(ToolOutput::text(text))
    }
}

/// The walk's result, or `Cancelled`. `biased`: a cancel already set wins over a
/// walk that has also finished (an unbiased select picked either at random and
/// returned partial matches as a success), and a walk that stopped on the cancel
/// (`Walk::cancelled`) is reported as cancelled, not as its partial matches.
async fn await_walk(
    cancel: &tokio_util::sync::CancellationToken,
    walk: impl std::future::Future<Output = Result<Walk, ToolError>>,
) -> Result<Walk, ToolError> {
    let cancelled = || ToolError::Cancelled {
        tool: "search_files".into(),
    };
    let walk = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(cancelled()),
        result = walk => result?,
    };
    if walk.cancelled || cancel.is_cancelled() {
        return Err(cancelled());
    }
    Ok(walk)
}

/// One search walk's inputs and what it found.
struct Walk {
    query: String,
    cancel: tokio_util::sync::CancellationToken,
    hard_links: AllowedHardlinkReads,
    /// The searched directory, relative to the workspace root, so a file's
    /// workspace-relative path can be checked against `hard_links`.
    base: std::path::PathBuf,
    matches: Vec<String>,
    skipped_hard_links: usize,
    /// Set when the walk stopped on the cancellation token: its matches are
    /// partial and must not be returned as a result.
    cancelled: bool,
}

/// Walks `dir` (a confined `cap_std` directory capability) depth-first,
/// appending `relative-path:line: text` for every line containing the query in
/// a UTF-8 text file, until [`MAX_SEARCH_MATCHES`] are collected or the
/// cancellation token fires. `label` is the path shown to the caller, kept
/// separate from the confinement path (which is always relative to `dir`)
/// so a symlink cannot forge a label naming a path outside the workspace.
fn search_dir(dir: &cap_std::fs::Dir, label: &Path, walk: &mut Walk) {
    if walk.cancel.is_cancelled() {
        walk.cancelled = true;
        return;
    }
    if walk.matches.len() >= MAX_SEARCH_MATCHES {
        return;
    }
    let Ok(entries) = dir.entries() else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(cap_std::fs::DirEntry::file_name);
    for entry in entries {
        if walk.cancel.is_cancelled() {
            walk.cancelled = true;
            return;
        }
        if walk.matches.len() >= MAX_SEARCH_MATCHES {
            return;
        }
        let name = entry.file_name();
        let child_label = label.join(&name);
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            // Every spelling of `.git` and kGraf's state directory, not only
            // the ASCII `.git`: files inside are read relative to the child
            // directory, so the read guard never sees this component.
            if kgraf_core::paths::is_unreadable_name(&name.to_string_lossy()) {
                continue;
            }
            // `open_dir` re-resolves `name` from `dir` with the same confined,
            // symlink-checked syscalls, so a directory entry that is actually
            // a symlink escaping the workspace is refused here rather than
            // walked into.
            if let Ok(child) = dir.open_dir(&name) {
                search_dir(&child, &child_label, walk);
            }
            continue;
        }
        let Ok(meta) = dir.metadata(&name) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_SEARCH_FILE_BYTES {
            continue;
        }
        let allow_hard_link = walk.hard_links.permits(&walk.base.join(&child_label));
        let text = match fs_safety::read_capped_to_string(
            "search_files",
            dir,
            Path::new(&name),
            MAX_SEARCH_FILE_BYTES,
            allow_hard_link,
        ) {
            Ok(text) => text,
            Err(ToolError::Access {
                source: crate::AccessError::HardLinked { .. },
                ..
            }) => {
                walk.skipped_hard_links += 1;
                continue;
            }
            Err(_) => continue,
        };
        // A match line can quote a secrets file's value (decision 0071).
        register_if_secrets_file(Path::new(&name), text.as_bytes());
        let relative = child_label.display().to_string();
        for (index, line) in text.lines().enumerate() {
            if line.contains(&walk.query) {
                walk.matches
                    .push(format!("{relative}:{}: {line}", index + 1));
                if walk.matches.len() >= MAX_SEARCH_MATCHES {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancellationToken;

    fn ctx(root: std::path::PathBuf) -> ToolContext {
        ToolContext {
            worker: "w1".into(),
            task: "t1".into(),
            workspace_root: root,
            cancel: CancellationToken::new(),
            progress: crate::ProgressSink::default(),
        }
    }

    fn permit() -> CallPermit {
        CallPermit::new()
    }

    /// Decision 0071: a search that reads a secrets file adds its values to
    /// the record scrubber; the match the model reads keeps the value.
    #[tokio::test]
    async fn a_search_through_a_secrets_file_scrubs_its_values_from_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = "kGrafMarkerSearch3Vd8Mx1Qh";
        std::fs::create_dir_all(dir.path().join("deploy")).expect("dir");
        std::fs::write(
            dir.path().join("deploy/ci.env"),
            format!("SERVICE_TOKEN={marker}\n"),
        )
        .expect("seed");
        let out = SearchFilesTool::default()
            .call(
                json!({"query": "SERVICE_TOKEN"}),
                &ctx(dir.path().to_path_buf()),
                &permit(),
            )
            .await
            .expect("call");
        let text = out.render();
        assert!(text.contains(marker), "{text}");
        assert!(!kgraf_secrets::scrub_persist(&text).contains(marker));
    }

    #[tokio::test]
    async fn read_file_returns_the_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "hello").expect("seed");
        let input = json!({"path": "a.txt"});
        let access = ReadFileTool::default().access(&input).expect("access");
        assert!(access.reads_paths().contains(Path::new("a.txt")));
        let out = ReadFileTool::default()
            .call(input, &ctx(dir.path().to_path_buf()), &permit())
            .await
            .expect("call");
        assert_eq!(out.render(), "hello");
    }

    #[tokio::test]
    async fn read_file_summarizes_binary_instead_of_encoding_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.png"), [0x89u8, b'P', b'N', b'G', 0]).expect("seed");
        let out = ReadFileTool::default()
            .call(
                json!({"path": "a.png"}),
                &ctx(dir.path().to_path_buf()),
                &permit(),
            )
            .await
            .expect("call");
        let rendered = out.render();
        assert!(rendered.contains("image/png"));
        assert!(!rendered.contains('\u{FFFD}'));
    }

    #[tokio::test]
    async fn read_file_refuses_an_escaping_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = ReadFileTool::default()
            .access(&json!({"path": "../outside.txt"}))
            .expect_err("must refuse");
        assert!(matches!(err, ToolError::Access { .. }));
        let err = ReadFileTool::default()
            .call(
                json!({"path": "../outside.txt"}),
                &ctx(dir.path().to_path_buf()),
                &permit(),
            )
            .await
            .expect_err("must refuse");
        assert!(matches!(err, ToolError::Access { .. }));
    }

    /// Regression: `read_file` used to
    /// follow a symlink straight through `std::fs::read`, so a link inside the
    /// workspace pointing outside it read whatever the link pointed at. This
    /// fails without `fs_safety`'s confined open.
    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_refuses_to_follow_a_symlink_out_of_the_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.txt"), "top secret").expect("seed");
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .expect("symlink");
        let err = ReadFileTool::default()
            .call(
                json!({"path": "link.txt"}),
                &ctx(dir.path().to_path_buf()),
                &permit(),
            )
            .await
            .expect_err("must refuse the escape, not read the planted secret");
        assert!(matches!(
            err,
            ToolError::Access { .. } | ToolError::Internal { .. }
        ));
    }

    #[tokio::test]
    async fn list_directory_lists_sorted_entries_with_a_trailing_slash_on_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("b.txt"), "").expect("seed");
        std::fs::create_dir(dir.path().join("a_dir")).expect("seed");
        let out = ListDirectoryTool
            .call(json!({}), &ctx(dir.path().to_path_buf()), &permit())
            .await
            .expect("call");
        assert_eq!(out.render(), "a_dir/\nb.txt");
    }

    /// An empty or blank `path` lists the workspace root, the default the
    /// description names; it failed with "cannot open :" before.
    #[tokio::test]
    async fn list_directory_reads_an_empty_path_as_the_workspace_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("b.txt"), "").expect("seed");
        for path in ["", "  "] {
            let input = json!({ "path": path });
            ListDirectoryTool.access(&input).expect("access");
            let out = ListDirectoryTool
                .call(input, &ctx(dir.path().to_path_buf()), &permit())
                .await
                .expect("call");
            assert_eq!(out.render(), "b.txt", "path {path:?}");
        }
    }

    /// Regression: listing a symlinked directory used
    /// to walk straight into whatever it pointed at.
    #[cfg(unix)]
    #[tokio::test]
    async fn list_directory_refuses_a_symlinked_directory_escaping_the_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("private.txt"), "").expect("seed");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("d")).expect("symlink");
        let err = ListDirectoryTool
            .call(
                json!({"path": "d"}),
                &ctx(dir.path().to_path_buf()),
                &permit(),
            )
            .await
            .expect_err("must refuse the escape");
        assert!(matches!(
            err,
            ToolError::Access { .. } | ToolError::Internal { .. }
        ));
    }

    #[tokio::test]
    async fn search_files_finds_a_literal_substring_across_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("one.txt"), "alpha\nneedle here\n").expect("seed");
        std::fs::create_dir(dir.path().join("sub")).expect("seed");
        std::fs::write(dir.path().join("sub/two.txt"), "another needle\n").expect("seed");
        let out = SearchFilesTool::default()
            .call(
                json!({"query": "needle"}),
                &ctx(dir.path().to_path_buf()),
                &permit(),
            )
            .await
            .expect("call");
        let rendered = out.render();
        assert!(rendered.contains("one.txt:2: needle here"));
        assert!(rendered.contains("two.txt:1: another needle"));
    }

    /// Files inside `.git` or kGraf's state directory are read relative to
    /// that directory, so the read guard never sees the parent name; the walk
    /// itself must not enter them, whatever their spelling.
    #[tokio::test]
    async fn search_files_never_enters_git_or_kgraf_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        for sub in [".git", ".kgraf", "nested/.haustoria"] {
            std::fs::create_dir_all(dir.path().join(sub)).expect("seed");
            std::fs::write(dir.path().join(sub).join("x.txt"), "needle hidden").expect("seed");
        }
        std::fs::write(dir.path().join("ok.txt"), "needle shown").expect("seed");
        let out = SearchFilesTool::default()
            .call(
                json!({"query": "needle"}),
                &ctx(dir.path().to_path_buf()),
                &permit(),
            )
            .await
            .expect("call");
        let rendered = out.render();
        assert!(rendered.contains("ok.txt:1: needle shown"), "{rendered}");
        assert!(!rendered.contains("hidden"), "{rendered}");
    }

    #[tokio::test]
    async fn search_files_refuses_an_escaping_path() {
        let err = SearchFilesTool::default()
            .access(&json!({"query": "x", "path": "/etc"}))
            .expect_err("must refuse");
        assert!(matches!(err, ToolError::Access { .. }));
    }

    /// Regression: the search walk used to follow a
    /// symlinked subdirectory and read whatever it pointed at.
    #[cfg(unix)]
    #[tokio::test]
    async fn search_files_does_not_walk_through_a_symlinked_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("leak.txt"), "needle outside").expect("seed");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("d")).expect("symlink");
        let out = SearchFilesTool::default()
            .call(
                json!({"query": "needle"}),
                &ctx(dir.path().to_path_buf()),
                &permit(),
            )
            .await
            .expect("call runs, just finds nothing through the link");
        assert_eq!(out.render(), "no matches");
    }

    fn finished_walk(cancelled: bool) -> Walk {
        Walk {
            query: "x".into(),
            cancel: tokio_util::sync::CancellationToken::new(),
            hard_links: AllowedHardlinkReads::default(),
            base: ".".into(),
            matches: vec!["partial.txt:1: x".into()],
            skipped_hard_links: 0,
            cancelled,
        }
    }

    /// The race behind the flaky `search_files_is_cancelled_promptly`: under load
    /// the walk finishes before the select first polls, so both branches are ready
    /// and an unbiased select returned the partial walk as a success about half the
    /// time. Stressed here with both branches ready, many times, on parallel tasks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancelled_search_never_returns_partial_results_as_success() {
        let mut handles = Vec::new();
        for _ in 0..8 {
            handles.push(tokio::spawn(async {
                for _ in 0..200 {
                    let cancel = tokio_util::sync::CancellationToken::new();
                    cancel.cancel();
                    let out = await_walk(&cancel, async { Ok(finished_walk(false)) }).await;
                    assert!(matches!(out, Err(ToolError::Cancelled { .. })));
                }
            }));
        }
        for h in handles {
            h.await.expect("task");
        }
        // A walk that stopped on the cancel is cancelled even if the token is
        // observed first by the walk alone.
        let fresh = tokio_util::sync::CancellationToken::new();
        let out = await_walk(&fresh, async { Ok(finished_walk(true)) }).await;
        assert!(matches!(out, Err(ToolError::Cancelled { .. })));
        let out = await_walk(&fresh, async { Ok(finished_walk(false)) }).await;
        assert!(out.is_ok(), "an uncancelled walk is a result");
    }

    #[tokio::test]
    async fn search_files_is_cancelled_promptly() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..20 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "x".repeat(10)).expect("seed");
        }
        let c = ctx(dir.path().to_path_buf());
        c.cancel.cancel();
        let err = SearchFilesTool::default()
            .call(json!({"query": "x"}), &c, &permit())
            .await
            .expect_err("cancellation must be observed");
        assert!(matches!(err, ToolError::Cancelled { .. }));
    }

    #[cfg(unix)]
    fn hard_linked_workspace() -> (tempfile::TempDir, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret"), "needle secret").expect("seed");
        std::fs::create_dir(dir.path().join("vendor")).expect("dir");
        std::fs::hard_link(
            outside.path().join("secret"),
            dir.path().join("vendor/linked.txt"),
        )
        .expect("hard link");
        std::fs::write(dir.path().join("plain.txt"), "needle plain").expect("seed");
        (dir, outside)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_refuses_a_hard_linked_file_by_name_unless_its_path_is_allowed() {
        let (dir, _outside) = hard_linked_workspace();
        let input = json!({"path": "vendor/linked.txt"});
        let err = ReadFileTool::default()
            .call(input.clone(), &ctx(dir.path().to_path_buf()), &permit())
            .await
            .expect_err("refused by default");
        let message = err.to_string();
        assert!(
            message.contains("`vendor/linked.txt`") && message.contains("2 hard links"),
            "{message}"
        );
        let allowed =
            AllowedHardlinkReads::new([std::path::PathBuf::from("vendor")]).expect("valid");
        let out = ReadFileTool::with_hardlink_reads(allowed)
            .call(input, &ctx(dir.path().to_path_buf()), &permit())
            .await
            .expect("allowed read");
        assert_eq!(out.render(), "needle secret");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn search_files_skips_a_hard_linked_file_and_says_so() {
        let (dir, _outside) = hard_linked_workspace();
        let out = SearchFilesTool::default()
            .call(
                json!({"query": "needle"}),
                &ctx(dir.path().to_path_buf()),
                &permit(),
            )
            .await
            .expect("call");
        let rendered = out.render();
        assert!(rendered.contains("plain.txt:1: needle plain"), "{rendered}");
        assert!(!rendered.contains("needle secret"), "{rendered}");
        assert!(
            rendered.contains("1 file(s) not searched: more than one hard link"),
            "{rendered}"
        );
        let allowed = AllowedHardlinkReads::new([std::path::PathBuf::from("vendor/linked.txt")])
            .expect("valid");
        let rendered = SearchFilesTool::with_hardlink_reads(allowed)
            .call(
                json!({"query": "needle"}),
                &ctx(dir.path().to_path_buf()),
                &permit(),
            )
            .await
            .expect("call")
            .render();
        assert!(
            rendered.contains("linked.txt:1: needle secret"),
            "{rendered}"
        );
        assert!(!rendered.contains("not searched"), "{rendered}");
    }
}
