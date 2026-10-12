//! Worker tools: the tool trait every tool implements, and the file-edit,
//! shell and command-policy tools, vendored from codex-rs under `third_party/` (0055).
//!
//! This crate holds the **seams** the other crates build against:
//!
//! - [`Tool`]: one callable tool. It declares what it touches through
//!   [`Tool::access`] **before** it runs, so the runtime can serialize two calls that
//!   touch the same graph node or path and overlap independent ones.
//! - [`approval`]: pre-tool hooks (allow, ask, deny, rewrite) and the only way to
//!   run a tool. `kgraf-session` re-exports it as its public approval API.
//! - [`ToolContext`]: who is calling, where, and the cancellation token.
//! - [`ToolOutput`]: what goes back to the model, held as [`UntrustedText`].
//! - [`ToolRegistry`]: the tool set under model-facing names, with a deferred
//!   partition. A duplicate name is refused (fail loud).
//!
//! **Approval is bound to execution.** [`Tool::call`] takes a [`CallPermit`], and a
//! permit has no public constructor: the only code that makes one is
//! [`ToolRegistry::execute`], which takes an [`approval::ApprovedCall`], which only
//! [`approval::ApprovalChain`] makes, after recording every decision. So a tool
//! cannot be run on a worker's behalf without passing approval, the same shape as
//! the ledger's `CallToken` (every token is attributed). `tests/compile_fail/` asserts it.
//!
//! The concrete tools:
//!
//! - [`apply_patch`]: the file-edit tool, ported from codex-rs `apply-patch` (parser,
//!   streaming parser, fuzzy line matching). Its `access()` parses the patch header
//!   before approval and declares every file it writes.
//! - [`fs_tools`]: read-only tools (read a file, list a directory, search file
//!   contents), each declaring a precise `access()` and, through [`fs_safety`],
//!   refusing an escaping path even through a symlink
//!   and a hard-linked file (a write always; a read unless allowed).
//! - [`GraphSliceTool`]: a code-graph slice around one symbol, with the node's
//!   file held to the same `fs_safety` rules as a read.
//! - [`execpolicy`]: command-policy rules (allow, prompt, forbidden) matched by
//!   argv prefix, the matching core ported from codex-rs `execpolicy`.
//! - [`shell`]: the shell tool, running through `kgraf-sandbox`'s runner inside
//!   the worker's workspace, with a timeout and capped output.

mod access;
pub mod apply_patch;
pub mod approval;
pub mod dependency_refetch;
mod error;
pub mod execpolicy;
pub mod external;
mod file_write;
mod fs_safety;
mod fs_tools;
mod graph_slice;
pub mod model_tools;
mod name;
mod output;
pub mod permission_edit;
pub mod permissions;
pub mod protected;
mod registry;
mod schema_lint;
pub mod shell;
mod terminal;
mod untrusted;

pub(crate) use access::normalize_relative;
pub use access::{Access, PathCase, detect_path_case};
pub use apply_patch::{
    ApplyPatchTool, PatchTarget, ProtectedApplyPatch, WHOLE_FILE_HINT_AFTER, patch_target,
    rejection_help, whole_file_hint, whole_file_skeleton,
};
pub use approval::{INVALID_INPUT, SKILLS_POLICY};
pub use dependency_refetch::{DependencyRefetch, RefetchOutcome};
pub use error::{AccessError, ApprovalError, RegistryError, ToolError};
pub use external::{ExternalExecutor, ExternalOutcome, SyncedWrites, remote_policy};
pub use file_write::{FileWrite, file_write_hint, pure_file_write};
pub use fs_safety::AllowedHardlinkReads;
pub use fs_tools::{ListDirectoryTool, ReadFileTool, SearchFilesTool};
pub use graph_slice::{DEFAULT_SLICE_BUDGET, GraphSliceTool};
pub use model_tools::{
    Attribution, CallLog, Called, DECIDE_TOOL, EMBED_TOOL, MAX_EMBED_RESULT_BYTES, MAX_EMBED_TEXTS,
    MAX_RERANK_DOCUMENTS, MODEL_TOOLS, ModelCallError, ModelCalls, ModelToolFactory, Opened,
    RERANK_TOOL, model_tools, run_model_tool,
};
pub use name::{MAX_WIRE_NAME_CHARS, MCP_PREFIX, QualifiedName};
pub use output::{BinarySummary, OutputPart, ToolOutput};
pub use permissions::{
    ModeSource, PermissionControl, PermissionEvent, PermissionMode, PermissionRulesHook,
    PermissionSettings, RefusedSubject, programs_run, refused_subject, refused_subject_of,
};
pub use protected::ProtectedWrites;
pub use registry::{DiscoveredTools, Exposure, RegisteredTool, ToolRegistry, ToolView};
pub use schema_lint::untyped_schema_nodes;
pub use shell::{ExecGate, SHELL_POLICY_HOOK, ShellApprovalHook, ShellTool, command_argv};
pub use terminal::{quote_argv, terminal_safe};
pub use untrusted::UntrustedText;

use kgraf_core::{AgentId, TaskId};
// Re-exported so a tool implementation needs no direct provider dependency.
pub use kgraf_provider::message::ToolDefinition;
use std::path::PathBuf;
// Re-exported so callers build a `ToolContext` against the same tokio-util version.
pub use tokio_util::sync::CancellationToken;

/// A tool call reports a transient wait through this (`ShellTool` awaiting an
/// execution slot must give a visible signal: "waiting for a machine slot").
/// `ShellTool` calls
/// [`ProgressSink::waiting`] before it awaits a scarce resource and
/// [`ProgressSink::resumed`] once it has one; the worker loop
/// (`kgraf-runtime`, which depends on this crate, not the reverse, so this
/// crate cannot emit a `WorkerEvent` itself) wires this to its own event
/// emission, so the transcript and the live view show the wait. Cheap to
/// clone (one `Arc`); [`ProgressSink::silent`] (this field's default on a
/// `ToolContext` no caller customizes) makes both calls no-ops, so no
/// existing construction site's behavior changes just from this field
/// existing.
/// The callback a [`ProgressSink`] wraps: `Some(reason)` on a wait,
/// `None` once it ends. A named alias only to satisfy clippy's
/// `type_complexity` lint on the field below; the type itself is exactly
/// what it was before this alias existed.
type ProgressCallback = dyn Fn(Option<&str>) + Send + Sync;

#[derive(Clone)]
pub struct ProgressSink(std::sync::Arc<ProgressCallback>);

impl ProgressSink {
    pub fn new(f: impl Fn(Option<&str>) + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(f))
    }

    /// A sink that does nothing: the default for a `ToolContext` no caller
    /// has wired a real one into.
    pub fn silent() -> Self {
        Self(std::sync::Arc::new(|_| {}))
    }

    /// Reports a wait has started, with a short, human-readable reason (for
    /// example "waiting for a machine slot").
    pub fn waiting(&self, reason: &str) {
        (self.0)(Some(reason));
    }

    /// Reports the wait this sink last reported has ended.
    pub fn resumed(&self) {
        (self.0)(None);
    }
}

impl Default for ProgressSink {
    fn default() -> Self {
        Self::silent()
    }
}

impl std::fmt::Debug for ProgressSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProgressSink(..)")
    }
}

/// Everything a tool call may know about its caller.
///
/// The worker and task ids are carried so a tool that reaches the network (a remote
/// MCP server) can attribute the call to its agent and task (decision 0051).
#[derive(Clone, Debug)]
pub struct ToolContext {
    /// The worker making the call.
    pub worker: AgentId,
    /// The task the worker is on.
    pub task: TaskId,
    /// The root of the worker's own git worktree (disjoint writes). Paths in [`Access`] are
    /// relative to it.
    pub workspace_root: PathBuf,
    /// Cancelled when the worker is cancelled. A long-running tool must observe it;
    /// a tool that ignores it holds a cancelled worker open.
    pub cancel: CancellationToken,
    /// Reports a transient wait a tool call hits (decision 0067). See [`ProgressSink`]'s own doc comment.
    pub progress: ProgressSink,
}

/// Proof that the call being made passed approval.
///
/// No public constructor, no `Default`, no `Clone`: only
/// [`ToolRegistry::execute`] makes one, for the duration of one call.
#[derive(Debug)]
pub struct CallPermit {
    _private: (),
}

impl CallPermit {
    pub(crate) fn new() -> Self {
        Self { _private: () }
    }
}

/// One tool a worker can call.
///
/// Two distinct error channels exist on purpose:
///
/// - `Ok(ToolOutput { is_error: true, .. })`: the tool ran and failed in a way the
///   model should see and may correct (file not found, test failed).
/// - `Err(ToolError)`: the harness could not run the tool (bad input, cancelled,
///   internal fault). The runtime records it and reports it to the model as an
///   error result; it is never dropped.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// The tool's own name, unqualified. For an MCP tool this is the server's name
    /// for it; the registry adds the server qualifier.
    fn name(&self) -> &str;

    /// The definition sent to the model. Its `name` must equal [`Tool::name`]; the
    /// registry rewrites it to the qualified wire name and refuses a mismatch.
    fn definition(&self) -> ToolDefinition;

    /// What this call reads and writes, derived from its input before it runs.
    ///
    /// **The default is [`Access::exclusive`]**: a tool that does not declare its
    /// footprint is serialized against every other call. That over-serializes
    /// rather than races (disjoint writes). Built-in tools override it with a precise
    /// declaration; the approval chain computes it from the final, possibly
    /// rewritten, input.
    fn access(&self, _input: &serde_json::Value) -> Result<Access, ToolError> {
        Ok(Access::exclusive())
    }

    /// The string an argument-scoped permission pattern matches against, such as
    /// the command line for a shell tool (`Bash(git:*)` in a skill's
    /// `allowed-tools`). `None` means the tool has no such subject, and a scoped
    /// pattern then refuses the call rather than allowing it.
    fn permission_subject(&self, _input: &serde_json::Value) -> Option<String> {
        None
    }

    /// Run the call. The permit proves the call was approved; see the crate docs.
    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError>;
}
