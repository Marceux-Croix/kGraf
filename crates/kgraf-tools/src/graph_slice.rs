//! The `graph_slice` tool: a symbol and its neighborhood from the code graph
//! (the graph is the context source; whole-file reads are the fallback).
//!
//! Moved here from the CLI's interim worker tools so every worker built-in lives in one crate and passes through the
//! same [`crate::fs_safety`] rules. The slice text comes from the graph
//! database, not from the file, but it is the file's content all the same, so
//! the node's file is held to the rules a `read_file` of it would meet: the path
//! is plain and relative, never under `.git` or `.kgraf`, it resolves inside
//! the worker's workspace through the confined open (a symlink out is refused),
//! it is a regular file, and a hard-linked file is refused unless the user
//! allowed its path ([`AllowedHardlinkReads`]).
//!
//! **The graph is indexed before the run.** It is not refreshed as the worker
//! writes, so a slice of a file the worker changed can be stale; the tool's
//! description says so.
//!
//! **Off the executor.** Graph queries are SQLite reads. `call` runs them on the
//! blocking pool; `access`, which is synchronous, runs its one symbol lookup
//! through `block_in_place` on a multi-thread runtime.

use crate::fs_safety::{self, AllowedHardlinkReads};
use crate::{Access, CallPermit, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use kgraf_graph::{GraphStore, NodeRecord};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};

const TOOL: &str = "graph_slice";
/// The estimated-token ceiling when the call names none.
pub const DEFAULT_SLICE_BUDGET: u64 = 2_000;

/// Reads a graph slice around one symbol.
pub struct GraphSliceTool {
    store: Arc<Mutex<GraphStore>>,
    hard_links: AllowedHardlinkReads,
}

impl GraphSliceTool {
    /// A slice tool over `store`, refusing hard-linked files.
    pub fn new(store: Arc<Mutex<GraphStore>>) -> Self {
        Self {
            store,
            hard_links: AllowedHardlinkReads::default(),
        }
    }

    /// Allow slices of hard-linked files under `allowed` (as `read_file` would).
    pub fn with_hardlink_reads(mut self, allowed: AllowedHardlinkReads) -> Self {
        self.hard_links = allowed;
        self
    }
}

fn invalid(reason: impl Into<String>) -> ToolError {
    ToolError::InvalidInput {
        tool: TOOL.into(),
        reason: reason.into(),
    }
}

fn internal(reason: impl std::fmt::Display) -> ToolError {
    ToolError::Internal {
        tool: TOOL.into(),
        reason: reason.to_string(),
    }
}

fn symbol_arg(input: &Value) -> Result<&str, ToolError> {
    input
        .get("symbol")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("`symbol` is required"))
}

fn resolve(store: &Mutex<GraphStore>, symbol: &str) -> Result<NodeRecord, ToolError> {
    // A poisoned lock means another query panicked; the store itself is a
    // read-only SQLite handle here, so its state is still usable.
    let store = store.lock().unwrap_or_else(|p| p.into_inner());
    // No requesting file: a crate-relative path found in several crates is
    // refused with its candidates rather than resolved to the first.
    let found = match store.resolve_symbol_from(symbol, None) {
        Ok(found) => found,
        // `path::Name` and a bare file stem: one node, or the candidates.
        Err(kgraf_graph::GraphError::UnknownSymbol { name }) => {
            let mut found = store.resolve_file_form(symbol).map_err(|e| match e {
                kgraf_graph::GraphError::UnknownSymbol { .. } => {
                    invalid(format!("no symbol `{name}` in the graph"))
                }
                other => internal(other),
            })?;
            if found.len() > 1 {
                let list: Vec<String> = found
                    .iter()
                    .map(|n| format!("{} ({}:{})", n.qualified_name, n.path, n.start_line))
                    .collect();
                return Err(invalid(format!(
                    "`{symbol}` is ambiguous; name one of: {}",
                    list.join(", ")
                )));
            }
            found.truncate(1);
            found
        }
        // A crate-relative path several crates hold: its input to fix.
        Err(e @ kgraf_graph::GraphError::AmbiguousSymbol { .. }) => {
            return Err(invalid(e.to_string()));
        }
        Err(other) => return Err(internal(other)),
    };
    found
        .into_iter()
        .next()
        .ok_or_else(|| invalid(format!("no symbol `{symbol}` in the graph")))
}

/// The node's path as a checked, workspace-relative path.
fn node_path(node: &NodeRecord) -> Result<std::path::PathBuf, ToolError> {
    let normalized =
        crate::normalize_relative(Path::new(&node.path)).map_err(|source| ToolError::Access {
            tool: TOOL.into(),
            source,
        })?;
    fs_safety::refuses_git_dir_or_kgraf(TOOL, &normalized)?;
    Ok(normalized)
}

/// `block_in_place` on a multi-thread runtime; inline elsewhere.
fn off_executor<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

#[async_trait::async_trait]
impl Tool for GraphSliceTool {
    fn name(&self) -> &str {
        TOOL
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: TOOL.into(),
            description: "Read a symbol and its neighborhood (enclosing element, callees, \
                callers, siblings) from the code graph. Names that work: `name`, `Type.method` or \
                `Type::method`, `crate::name`, `path/to/file.rs::name`. The graph is indexed \
                before the run; slices of files you have changed may be stale."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "symbol": {"type": "string", "description": "A symbol name or qualified name"},
                    "budget": {"type": "integer", "description": "Estimated-token ceiling, default 2000"}
                },
                "required": ["symbol"],
                "additionalProperties": false,
            }),
        }
    }

    /// Reads the node and its file: two calls on the same symbol overlap, and a
    /// write to the file is ordered against the slice.
    fn access(&self, input: &Value) -> Result<Access, ToolError> {
        let symbol = symbol_arg(input)?;
        let node = off_executor(|| resolve(&self.store, symbol))?;
        let path = node_path(&node)?;
        Access::none()
            .read_node(node.id.as_str())
            .read_path(&path)
            .map_err(|source| ToolError::Access {
                tool: TOOL.into(),
                source,
            })
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let symbol = symbol_arg(&input)?.to_owned();
        let budget = match input.get("budget") {
            None => DEFAULT_SLICE_BUDGET,
            Some(v) => v
                .as_u64()
                .filter(|b| *b > 0)
                .ok_or_else(|| invalid("`budget` must be a positive integer"))?,
        };
        let store = self.store.clone();
        let root = ctx.workspace_root.clone();
        let hard_links = self.hard_links.clone();
        tokio::task::spawn_blocking(move || {
            let node = resolve(&store, &symbol)?;
            let path = node_path(&node)?;
            let dir = fs_safety::open_workspace(TOOL, &root)?;
            fs_safety::check_readable(TOOL, &dir, &path, hard_links.permits(&path))?;
            let store = store.lock().unwrap_or_else(|p| p.into_inner());
            // The worker's own tree first: the store's root is the user's
            // checkout, which can lack a file the worker's base holds. A file
            // the worker has since edited no longer matches the index there,
            // so the indexed copy is read instead (stale, as the description
            // says) rather than no slice at all.
            let slice =
                match kgraf_graph::slice::slice_for_node_in(&store, &node, budget, Some(&root)) {
                    Ok(slice) => slice,
                    Err(
                        kgraf_graph::GraphError::StaleFile { .. }
                        | kgraf_graph::GraphError::StaleSpan { .. }
                        | kgraf_graph::GraphError::Io { .. },
                    ) => kgraf_graph::slice::slice_for_node(&store, &node, budget)
                        .map_err(internal)?,
                    Err(e) => return Err(internal(e)),
                };
            Ok(ToolOutput::text(slice.render()))
        })
        .await
        .map_err(internal)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancellationToken;

    /// A worktree with one indexed Rust file, and the tool over its graph.
    fn indexed(files: &[(&str, &str)]) -> (tempfile::TempDir, GraphSliceTool) {
        let dir = tempfile::tempdir().expect("dir");
        for (path, text) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, text).expect("write");
        }
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        kgraf_graph::Indexer::new(&store).index().expect("index");
        (dir, GraphSliceTool::new(Arc::new(Mutex::new(store))))
    }

    fn ctx(root: &Path) -> ToolContext {
        ToolContext {
            worker: "w".into(),
            task: "t".into(),
            workspace_root: root.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: crate::ProgressSink::default(),
        }
    }

    #[tokio::test]
    async fn a_slice_names_the_symbol_and_declares_its_node_and_file() {
        let (dir, tool) = indexed(&[("src/lib.rs", "pub fn alpha() -> u8 {\n    7\n}\n")]);
        let input = json!({"symbol": "alpha"});
        let access = tool.access(&input).expect("access");
        assert!(access.reads_paths().contains(Path::new("src/lib.rs")));
        let out = tool
            .call(input, &ctx(dir.path()), &crate::CallPermit::new())
            .await
            .expect("slice");
        let text = out.render();
        assert!(text.contains("alpha"), "{text}");
    }

    #[tokio::test]
    async fn an_unknown_symbol_is_invalid_input() {
        let (_dir, tool) = indexed(&[("src/lib.rs", "pub fn alpha() {}\n")]);
        let result = tool.access(&json!({"symbol": "nope"}));
        assert!(
            matches!(result, Err(ToolError::InvalidInput { .. })),
            "{result:?}"
        );
    }

    /// `crate::alpha` held by two crates names neither: refused as input to
    /// fix, with both candidates, never resolved to the first.
    #[tokio::test]
    async fn a_crate_relative_symbol_in_two_crates_is_invalid_input_naming_both() {
        let (_dir, tool) = indexed(&[
            ("a/Cargo.toml", "[package]\nname = \"a\"\n"),
            ("a/src/lib.rs", "pub fn alpha() {}\n"),
            ("b/Cargo.toml", "[package]\nname = \"b\"\n"),
            ("b/src/lib.rs", "pub fn alpha() {}\n"),
        ]);
        match tool.access(&json!({"symbol": "crate::alpha"})) {
            Err(ToolError::InvalidInput { reason, .. }) => {
                assert!(
                    reason.contains("a/src/lib.rs") && reason.contains("b/src/lib.rs"),
                    "{reason}"
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(tool.access(&json!({"symbol": "a::alpha"})).is_ok());
    }

    /// `path::name` resolves to the one symbol in that file; a bare stem held
    /// by two files, or a path part shared by two, is refused with both.
    #[tokio::test]
    async fn a_path_qualified_name_resolves_and_an_ambiguous_one_lists_candidates() {
        let (_dir, tool) = indexed(&[
            ("a/src/lib.rs", "pub fn alpha() {}\n"),
            ("b/src/lib.rs", "pub fn alpha() {}\n"),
            ("b/src/util.rs", "pub fn beta() {}\n"),
        ]);
        let access = tool
            .access(&json!({"symbol": "a/src/lib.rs::alpha"}))
            .expect("one file");
        assert!(access.reads_paths().contains(Path::new("a/src/lib.rs")));
        assert!(tool.access(&json!({"symbol": "util"})).is_ok());
        for name in ["src/lib.rs::alpha", "lib"] {
            match tool.access(&json!({"symbol": name})) {
                Err(ToolError::InvalidInput { reason, .. }) => assert!(
                    reason.contains("a/src/lib.rs") && reason.contains("b/src/lib.rs"),
                    "{name}: {reason}"
                ),
                other => panic!("{name}: {other:?}"),
            }
        }
    }

    /// The slice text comes from the graph, but a hard-linked source file is
    /// refused exactly as `read_file` would refuse it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_slice_of_a_hard_linked_file_is_refused_unless_allowed() {
        let (dir, tool) = indexed(&[("src/lib.rs", "pub fn alpha() {}\n")]);
        let outside = tempfile::tempdir().expect("outside");
        std::fs::hard_link(dir.path().join("src/lib.rs"), outside.path().join("alias"))
            .expect("hard link");
        let err = tool
            .call(
                json!({"symbol": "alpha"}),
                &ctx(dir.path()),
                &crate::CallPermit::new(),
            )
            .await
            .expect_err("refused");
        assert!(err.to_string().contains("`src/lib.rs`"), "{err}");
        let tool = tool.with_hardlink_reads(
            AllowedHardlinkReads::new([std::path::PathBuf::from("src")]).expect("valid"),
        );
        tool.call(
            json!({"symbol": "alpha"}),
            &ctx(dir.path()),
            &crate::CallPermit::new(),
        )
        .await
        .expect("allowed");
    }

    /// The graph was indexed from another tree; the worker's workspace has a
    /// symlink where the file should be, pointing outside. Refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_node_whose_file_is_a_symlink_out_of_the_workspace_is_refused() {
        let (_indexed_dir, tool) = indexed(&[("src/lib.rs", "pub fn alpha() {}\n")]);
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("lib.rs"), "secret").expect("seed");
        std::fs::create_dir(workspace.path().join("src")).expect("src");
        std::os::unix::fs::symlink(
            outside.path().join("lib.rs"),
            workspace.path().join("src/lib.rs"),
        )
        .expect("symlink");
        let err = tool
            .call(
                json!({"symbol": "alpha"}),
                &ctx(workspace.path()),
                &crate::CallPermit::new(),
            )
            .await
            .expect_err("refused");
        assert!(
            matches!(err, ToolError::Access { .. } | ToolError::Internal { .. }),
            "{err}"
        );
    }
}
