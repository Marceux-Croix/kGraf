//! The seam that routes one run's `shell` tool calls to an external executor
//! (for example a task container driven by another harness) instead of
//! [`crate::shell::run_sandboxed`] on the host.
//!
//! **Why this seam and not a bigger one.** [`crate::shell::ShellTool::call`]
//! already separates "decide whether this command may run at all" (the
//! `ExecPolicy` checks, the `Prompt`/`Forbidden` gate, the read-only path
//! confinement) from "actually run it" (`sandbox_policy` +
//! `run_sandboxed`). Every safety check stays exactly where it is and runs
//! unconditionally; only the final execution step is swapped. A `ShellTool`
//! built without [`crate::shell::ShellTool::with_external_executor`] never
//! has an executor and its `call()` path is what it was before this module
//! existed. When an executor is set, a failure of the executor is an error
//! result: the command is never run on the host as a fallback.
//!
//! **File writes.** [`crate::ApplyPatchTool`] keeps writing to the host
//! worktree, which stays the copy the model reads. An executor keeps the
//! remote side equal to it: [`ExternalExecutor::exec`] brings the remote in
//! line with the host before a command and brings the host in line with the
//! remote after it, and [`SyncedWrites`] calls
//! [`ExternalExecutor::sync_to_remote`] after every write tool call, so the
//! remote never lags a host write, even when no command follows it.
use crate::execpolicy::{Decision, ExecPolicy};
use crate::{Access, CallPermit, Tool, ToolContext, ToolError, ToolOutput};
use kgraf_provider::message::ToolDefinition;
use std::sync::Arc;
use std::time::Duration;

/// What running one command produced, in the same shape
/// [`crate::shell::run_sandboxed`] already reports: split streams, an exit
/// code (`None` for "killed by a signal" or "still unknown"), and whether the
/// executor itself gave up on a timeout it enforces (so [`crate::shell::ShellTool`]
/// does not have to guess from an exit code alone).
#[derive(Debug, Clone)]
pub struct ExternalOutcome {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
    /// The executor stopped the command itself because it ran past the
    /// timeout it was given. When `true`, `exit_code` describes whatever
    /// state the command was in at that point, not a real exit.
    pub timed_out: bool,
}

/// One command, run somewhere that is not this process's own host: a task
/// container reached through another harness, or any other out-of-process
/// runner. The trait carries no assumption about *how* the command reaches
/// its target: a subprocess talking a line protocol to a bridge process, an
/// in-process HTTP client, or (in tests) a fake that never leaves memory are
/// all valid implementations.
///
/// **Security note for every implementation.** Nothing about this trait
/// object bounds what it is allowed to run. That job stays with
/// [`crate::shell::ShellTool::access`] and [`crate::shell::ShellApprovalHook`],
/// which run identically whether the command ends up here or in
/// `run_sandboxed`. An `ExternalExecutor` must never be handed a command
/// [`crate::shell::ShellTool::access`] would have refused; the tool's
/// `call()` enforces that ordering, not this trait.
#[async_trait::async_trait]
pub trait ExternalExecutor: Send + Sync {
    /// Run `cmd` (argv, never a shell-joined string; the same contract
    /// [`crate::shell::ShellTool`] already holds its local path to) and wait
    /// up to `timeout` for it to finish. Before it runs, the remote tree
    /// equals the host tree. After it ends, the host tree equals the remote
    /// tree. An implementation that cannot honor `timeout` itself should still
    /// return promptly on cancellation; there is no cancellation token here
    /// because every existing call site already races this future against
    /// `ctx.cancel.cancelled()`.
    async fn exec(&self, cmd: &[String], timeout: Duration) -> Result<ExternalOutcome, String>;

    /// The host stopped waiting for an [`Self::exec`] and dropped its future:
    /// the call was cancelled, or the executor did not report within the
    /// host's own bound. Stop the command if the executor can, and make sure
    /// no later call reads that command's result as its own. Called once,
    /// right after the drop, and bounded by the host; the default does
    /// nothing, for an executor whose `exec` holds no state past a drop.
    async fn abandon(&self) {}

    /// Make the remote tree equal to the host tree, byte for byte, and return
    /// only when it is. Called after a host-side file write, so that the next
    /// remote command, and whatever inspects the remote afterward, sees the
    /// write. An error means the two trees may differ.
    async fn sync_to_remote(&self) -> Result<(), String>;
}

/// Wraps a tool that writes to the host worktree so that every call, whether
/// it succeeds or fails, ends with [`ExternalExecutor::sync_to_remote`]. A
/// failed call can still have written some files, so it syncs too. A sync
/// failure is an error the model sees, never a silent divergence.
pub struct SyncedWrites<T> {
    inner: T,
    executor: Arc<dyn ExternalExecutor>,
}

impl<T: Tool> SyncedWrites<T> {
    pub fn new(inner: T, executor: Arc<dyn ExternalExecutor>) -> Self {
        Self { inner, executor }
    }
}

#[async_trait::async_trait]
impl<T: Tool> Tool for SyncedWrites<T> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    fn access(&self, input: &serde_json::Value) -> Result<Access, ToolError> {
        self.inner.access(input)
    }

    fn permission_subject(&self, input: &serde_json::Value) -> Option<String> {
        self.inner.permission_subject(input)
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let result = self.inner.call(input, ctx, permit).await;
        match self.executor.sync_to_remote().await {
            Ok(()) => result,
            Err(e) => Err(ToolError::Internal {
                tool: self.inner.name().to_owned(),
                reason: format!("the write reached the worktree but not the remote tree: {e}"),
            }),
        }
    }
}

/// The command policy for a run whose commands execute in a disposable remote
/// environment. The environment is the isolation boundary, and nobody is at a
/// terminal to answer an approval prompt, so a command no rule covers runs
/// instead of being refused. The exact shapes the default policy forbids stay
/// forbidden, and the build allowlist, if the caller adds one, still applies.
/// No rule here marks a command read-only: read-only confinement checks host
/// paths, and a routed command does not run on the host.
pub fn remote_policy() -> ExecPolicy {
    ExecPolicy::new(Decision::Allow)
        .forbid(&["rm", "-rf", "/"], "refuses to remove the filesystem root")
        .forbid(
            &["git", "push", "--force"],
            "a force push can discard remote history",
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolContext, ToolError};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingExecutor {
        syncs: AtomicUsize,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl ExternalExecutor for CountingExecutor {
        async fn exec(
            &self,
            _cmd: &[String],
            _timeout: Duration,
        ) -> Result<ExternalOutcome, String> {
            unreachable!("these tests never run a command")
        }

        async fn sync_to_remote(&self) -> Result<(), String> {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err("upload failed".into())
            } else {
                Ok(())
            }
        }
    }

    /// A tool that records its calls and can be told to fail.
    struct Inner {
        calls: Mutex<usize>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl Tool for Inner {
        fn name(&self) -> &str {
            "inner"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "inner".into(),
                description: "test".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolContext,
            _permit: &CallPermit,
        ) -> Result<ToolOutput, ToolError> {
            *self.calls.lock().expect("lock") += 1;
            if self.fail {
                Err(ToolError::Internal {
                    tool: "inner".into(),
                    reason: "wrote half".into(),
                })
            } else {
                Ok(ToolOutput::text("written"))
            }
        }
    }

    fn ctx() -> ToolContext {
        ToolContext {
            worker: "w".into(),
            task: "t".into(),
            workspace_root: std::env::temp_dir(),
            cancel: tokio_util::sync::CancellationToken::new(),
            progress: crate::ProgressSink::silent(),
        }
    }

    async fn run(inner_fails: bool, sync_fails: bool) -> (Result<ToolOutput, ToolError>, usize) {
        let executor = Arc::new(CountingExecutor {
            syncs: AtomicUsize::new(0),
            fail: sync_fails,
        });
        let tool = SyncedWrites::new(
            Inner {
                calls: Mutex::new(0),
                fail: inner_fails,
            },
            executor.clone(),
        );
        let result = tool
            .call(serde_json::json!({}), &ctx(), &CallPermit::new())
            .await;
        (result, executor.syncs.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn a_successful_write_is_synced_once_and_its_result_is_kept() {
        let (result, syncs) = run(false, false).await;
        assert_eq!(syncs, 1);
        assert!(!result.expect("ok").is_error);
    }

    #[tokio::test]
    async fn a_failed_write_is_synced_too_and_keeps_its_own_error() {
        let (result, syncs) = run(true, false).await;
        assert_eq!(syncs, 1, "a failed call can leave files behind");
        assert!(
            result
                .expect_err("inner error")
                .to_string()
                .contains("wrote half")
        );
    }

    #[tokio::test]
    async fn a_failed_sync_is_an_error_the_model_sees() {
        let (result, syncs) = run(false, true).await;
        assert_eq!(syncs, 1);
        let err = result
            .expect_err("a sync failure must not be hidden")
            .to_string();
        assert!(err.contains("upload failed"), "{err}");
    }

    #[test]
    fn the_remote_policy_runs_unlisted_commands_but_keeps_the_forbidden_shapes() {
        let policy = remote_policy();
        let argv = |w: &[&str]| w.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let unlisted = policy.evaluate(&argv(&["python3", "solve.py"]));
        assert_eq!(unlisted.decision, Decision::Allow);
        assert!(
            !unlisted.read_only,
            "no host path confinement for a remote command"
        );
        assert_eq!(
            policy.evaluate(&argv(&["rm", "-rf", "/"])).decision,
            Decision::Forbidden
        );
        // The policy a local run uses would have asked, which no one answers.
        assert_eq!(
            ExecPolicy::default_kgraf_policy()
                .evaluate(&argv(&["python3", "solve.py"]))
                .decision,
            Decision::Prompt
        );
    }
}
