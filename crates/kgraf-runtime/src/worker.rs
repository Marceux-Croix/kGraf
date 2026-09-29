//! The worker loop: build context, call the model through the ledger,
//! dispatch tool calls, iterate, return a structured result.
//!
//! **One round**, as a state machine:
//!
//! ```text
//!   BetweenRounds --cancelled--------------------------------> Cancelled
//!        | context >= stop point ----------------------------> HandedBack
//!        | budget spent -------------------------------------> BudgetExhausted
//!        | (budget notice, then the steering drain: every
//!        |  queued message becomes an `Injected` event)
//!        v
//!   ModelStream (ledger row open; stream raced against cancel and
//!                against an interrupt-mode message)
//!        | cancelled: drop the stream, settle partial usage -> Cancelled
//!        | interrupt: drop the stream, settle partial usage,
//!        |   append RoundAbandoned ---------------------------> BetweenRounds
//!        | provider error / overflow / bad stop -------------> Failed
//!        | stop without tool calls --------------------------> Completed
//!        | tool calls in the final round --------------------> BudgetExhausted
//!        v
//!   ToolCall (approval in issue order, stall check on approved calls,
//!             then conflict-ordered execution, raced against cancel and
//!             against an interrupt-mode message)
//!        | a call repeated to the stall threshold -----------> Stalled
//!        | cancelled: record calls that finish within the
//!        |   grace, mark the rest ----------------------------> Cancelled
//!        | interrupt: record calls that finish within the
//!        |   grace, give the rest a synthetic result,
//!        |   append RoundAbandoned ---------------------------> BetweenRounds
//!        v
//!   BetweenRounds
//! ```
//!
//! **Steering (decision 0067, point 3).** Messages reach the loop through the
//! worker's queue (`inject.rs`). The drain runs once per round, after the
//! context, spend and budget checks and after the budget notice, immediately
//! before the request is built. Two reasons for that place, where the design
//! sketch put it before the checks: a round the checks refuse is never sent,
//! so a note drained before them would be in the transcript and never seen by
//! the model (left undrained, it stays listed by `Injector::undelivered` for
//! the orchestrator to route); and the note then follows the notice in the
//! user turn, which is the order the model should read them in. The checks
//! count the queued notes' size without draining them, so a note that would
//! push the round past the stop point makes the worker hand back with the
//! note undelivered, rather than send an oversized request.
//!
//! An interrupt waits out an approval or question ask (that tool call must end
//! first); no further call of the round starts once it is armed, and it then
//! abandons the round at the next interruptible phase. On a steer, read-only
//! calls are cancelled; a call that declares writes is not (cancelling would
//! not stop work already on a blocking thread), and gets the grace to finish.
//! One that outlives the grace is then stopped through its own cancel path
//! (so a shell command's whole process group is killed), and is reported as
//! "may have written" its declared paths, which count as changed. The final round is never abandoned: its
//! answer finishes, and the message is a leftover. A message that arrives as
//! the model finishes its answer is not delivered either: the worker retires,
//! and the item is a leftover (`Injector::undelivered`) the caller routes.
//!
//! **Delivery: exactly once in the transcript, at least once to the
//! provider.** The transcript is the truth for "delivered": the loop records
//! `Injected` before it builds the request, and whoever writes the transcript
//! marks the inbox row delivered after appending that event. A crash before
//! the append loses the event and the round with it, so the resumed segment
//! injects the row once more; the provider may by then have received the lost
//! request that carried it, so it has seen the note twice, but the
//! conversation the model continues from holds it once. A crash after the
//! append finds the id in `ResumeState::injected_ids` and refuses the second
//! copy.
//!
//! **Accounting (every token is attributed).** The only provider calls are `Provider::complete` and
//! `Streaming::stream`, both of which need the `CallToken` minted by
//! `Ledger::begin`. The adapter settles a call it finishes; the loop settles a call
//! it cancels: `Cancelled` with the last cumulative usage the stream delivered, or,
//! when no usage arrived, left unreconciled with its request id, because an unknown
//! spend is not a zero spend. A `ModelCall` event is emitted before each request
//! leaves, so a round counts against the step budget even when it fails.
//!
//! **Shared prefix (0043).** Every request is built from the environment's
//! `PrefixSnapshot` and checked against it before the ledger row is opened; the
//! private remainder (instruction, slices, transcript) follows in `messages`.
//!
//! **Trace.** Every round's ledger row is opened with
//! `Ledger::begin_traced`, carrying:
//!
//! - **parent call**: the call this one continues from, which is the worker's
//!   previous round (on the first round, the call that dispatched the worker, when
//!   [`SpawnOptions::parent_call`] names one; on a resume, the last call of the
//!   earlier segment);
//! - **kind** `worker_round` and **role** `worker`;
//! - **what the call was given**: the brief's private slice ids and the shared
//!   prefix digest, never text;
//! - **the tool round**: the ordered tool calls whose results this request carries,
//!   that is, the previous round's calls. The ledger refuses trace writes once a
//!   row settles and adapters settle before they return, so a round's own calls,
//!   known only from its response, cannot be written to its own row; they are
//!   written to the next row, which is the call that was given them, while it is
//!   still in flight. A run that ends on a tool round (cancelled, stalled, out of
//!   budget) has that last round in its transcript, and a resume of it writes
//!   that round to its first row;
//! - **time to first token**: measured in stream mode, at the first output delta
//!   the adapter pushes, and written then, before the adapter settles. A
//!   non-streamed call has no first token; its row carries none.
//!
//! A trace write the ledger refuses does not stop the run (a trace field is not
//! spend) and is not swallowed: it lands in `WorkerResult::trace_failures`.
//!
//! **Context (0057 addendum 2).** Workers never compact. Old tool outputs are
//! masked on every request through [`ObservationMasker`], always from the original
//! transcript. When the context reaches the configured stop point the worker stops
//! between rounds and returns `HandedBack` with a structured `Handoff`, built from
//! what the loop observed rather than from a summarizing model call, which would be
//! hidden spend.

use crate::brief::{BriefError, Handoff, WorkerBrief, WorkerOutcome, WorkerResult};
use crate::context::{
    PrefixCheckError, PrefixRegistry, PrefixSnapshot, SliceError, SliceSource, opening_messages,
};
use crate::dispatch::dependencies;
use crate::event::{CancelPhase, EventSequence, WorkerEvent};
use crate::fence::ScopeFence;
use crate::handle::{RuntimeError, WorkerEndpoint, WorkerHandle, worker_channel};
use crate::inject::{InjectError, Injection, SteerQueue};
use crate::masking::{ObservationMasker, SessionMasker};
use crate::rebuild::{ResumeError, ResumeState, TranscriptBuilder};
use crate::stall::{DEFAULT_STALL_REPEATS, StallDetector};
use crate::step::{RoundPlan, StepBudget};
use crate::stream_bridge;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::FuturesUnordered;
use kgraf_core::{CallId, TokenUsage};
use kgraf_provider::accounting::CallTrace;
use kgraf_provider::{Content, Ledger, Provider, Request, Response, StopReason, StreamEvent};
use kgraf_tools::approval::{
    ApprovalChain, ApprovalLog, ApprovalRecord, ApprovedCall, PausedCall, PendingCall, UserAnswer,
    Verdict,
};
use kgraf_tools::{
    ApprovalError, DiscoveredTools, PathCase, ProgressSink, ToolContext, ToolError, ToolOutput,
    ToolRegistry, ToolView,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

/// How the loop calls the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallMode {
    /// `Streaming::stream`. The provider must offer streaming; one that does not
    /// is refused at spawn, never silently called another way (fail loud).
    Stream,
    /// `Provider::complete`. Cancelling drops the in-flight request; no partial
    /// usage is known, so the ledger row is settled cancelled with none.
    Complete,
}

/// Loop settings shared by the workers of one environment.
#[derive(Clone, Debug)]
pub struct LoopSettings {
    pub max_output_tokens: u32,
    pub temperature: Option<f32>,
    pub call_mode: CallMode,
    /// Unread stream output the bridge holds before the run fails (bytes).
    pub bridge_limit_bytes: usize,
    /// Bulk events queued for the reader before the worker waits.
    pub bulk_capacity: usize,
    /// Identical tool calls that mark a stall.
    pub stall_repeats: u32,
    /// How the workspace filesystem compares path names.
    pub path_case: PathCase,
    /// After a cancel, how long running tool calls get to finish, so what they
    /// actually did is recorded instead of being guessed at.
    pub tool_cancel_grace: std::time::Duration,
}

impl Default for LoopSettings {
    fn default() -> Self {
        Self {
            max_output_tokens: 4_096,
            temperature: None,
            call_mode: CallMode::Stream,
            bridge_limit_bytes: 8 * 1024 * 1024,
            bulk_capacity: 256,
            stall_repeats: DEFAULT_STALL_REPEATS,
            // The conservative choice: a case-insensitive comparison serializes
            // more, never less. Detect with `kgraf_tools::detect_path_case`.
            path_case: PathCase::Insensitive,
            tool_cancel_grace: std::time::Duration::from_secs(2),
        }
    }
}

/// Answers an approval ask while a worker waits.
#[async_trait::async_trait]
pub trait AskResolver: Send + Sync {
    async fn answer(&self, paused: &PausedCall) -> UserAnswer;
}

/// Everything shared by the workers dispatched to one vendor and model.
pub struct WorkerEnv {
    pub ledger: Arc<Ledger>,
    pub provider: Arc<dyn Provider>,
    /// One registry snapshot per vendor and model (0043).
    pub tools: Arc<ToolRegistry>,
    pub approval: ApprovalChain,
    /// Who answers an ask. `None` fails a run that meets one, loudly.
    pub asks: Option<Arc<dyn AskResolver>>,
    pub prefix: PrefixSnapshot,
    pub prefixes: Arc<PrefixRegistry>,
    pub slices: Arc<dyn SliceSource>,
    pub masker: Arc<dyn ObservationMasker>,
    pub settings: LoopSettings,
}

impl WorkerEnv {
    /// An environment using the process-wide prefix registry, with an empty approval chain (every call recorded as
    /// allowed with no hooks), no ask resolver, no graph, `kgraf-session`'s
    /// masking at SWE-agent's window, and default settings.
    pub fn new(
        ledger: Arc<Ledger>,
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        prefix: PrefixSnapshot,
    ) -> Self {
        Self {
            ledger,
            provider,
            tools,
            approval: ApprovalChain::new(Vec::new()),
            asks: None,
            prefix,
            prefixes: PrefixRegistry::global(),
            slices: Arc::new(crate::context::NoSlices),
            masker: Arc::new(SessionMasker::default()),
            settings: LoopSettings::default(),
        }
    }
}

/// A worker that cannot be started.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Brief(#[from] BriefError),
    #[error(transparent)]
    Prefix(#[from] PrefixCheckError),
    #[error("the brief names model `{brief}` but the prefix snapshot is for `{prefix}`")]
    ModelMismatch { brief: String, prefix: String },
    #[error(
        "the prefix snapshot's tool definitions are not the registry's prefix definitions; one snapshot per registry"
    )]
    ToolsNotInPrefix,
    #[error("adapter `{adapter}` does not stream, and the loop is set to stream")]
    StreamingUnavailable { adapter: &'static str },
    #[error(
        "the brief activates skills {skills:?}, and skills are not wired into the loop yet; refused rather than ignored"
    )]
    SkillsUnsupported { skills: Vec<String> },
    #[error(transparent)]
    Slice(#[from] SliceError),
    #[error(transparent)]
    Resume(#[from] ResumeError),
    #[error(
        "the transcript was recorded with {what} `{stored}` and this worker has `{current}`; refused rather than resumed on different context"
    )]
    ResumeMismatch {
        what: &'static str,
        stored: String,
        current: String,
    },
    /// The transcript was recorded before Haustoria was renamed kGraf: the
    /// worker's system prompt named the product, so its prefix differs.
    #[error(
        "the transcript was recorded before Haustoria was renamed kGraf (decision 0062): its \
         prefix digest `{stored}` is this worker's prefix with the former name, and the \
         worker's prompt now names kGraf; refused rather than resumed on different context. \
         Start the work again as a fresh worker."
    )]
    ResumeFromBeforeRename { stored: String },
    #[error("cannot render the worker's slices: the rendering task failed: {0}")]
    SliceTask(String),
    /// An item of `SpawnOptions::injections` could not be queued.
    #[error("cannot queue a spawn injection: {0}")]
    Injection(InjectError),
}

/// Options for one spawn.
#[derive(Default)]
pub struct SpawnOptions {
    /// This worker's approval chain, replacing the environment's (a skill's
    /// `allowed-tools` policy is per worker).
    pub approval: Option<ApprovalChain>,
    /// Events of an earlier run of this brief to resume from.
    pub resume_from: Option<Vec<WorkerEvent>>,
    /// The call that dispatched this worker (an orchestrator's), recorded as the
    /// parent of the worker's first round. Later rounds name the previous round.
    pub parent_call: Option<CallId>,
    /// Inbox messages undelivered when this worker (a resumed segment) was
    /// killed, or queued before it started, to inject at the first boundary
    /// (decision 0067, point 3: "exactly once across a crash"). An id the
    /// resumed transcript already holds is not queued again; the handle lists
    /// it in `WorkerHandle::duplicate_injections`. Always delivered at the
    /// boundary, whatever its mode: nothing is running yet to interrupt.
    pub injections: Vec<Injection>,
    /// This worker's write fence (decision 0067, point 5): a parent worker's
    /// own attempt is given one at dispatch (`kgraf-orchestrator`'s
    /// `start_attempt`/`run_attempt`), and a live writer subagent fences paths
    /// on that same `ScopeFence` (`ScopeFence` clones share state), so the
    /// parent's writes to those paths are refused while the child holds them.
    /// A read-only subagent is given `ScopeFence::read_only()` instead, which
    /// fences every path. `None`: no fence at all (tests, and `kgraf worker`'s
    /// unorchestrated single attempt).
    pub fence: Option<ScopeFence>,
}

/// Check a brief against its environment and start its worker on the current
/// tokio runtime. Async because rendering the worker's graph slices queries
/// SQLite, which runs on the blocking pool, not on an executor thread.
pub async fn spawn_worker(
    env: Arc<WorkerEnv>,
    brief: WorkerBrief,
    options: SpawnOptions,
) -> Result<WorkerHandle, WorkerError> {
    let (worker, endpoint, handle) = prepare(&env, brief, options).await?;
    tokio::spawn(worker.run(endpoint));
    Ok(handle)
}

/// Run blocking work (a SQLite write, a graph query) without stalling other tasks
/// on this executor thread: `block_in_place` on a multi-thread runtime. A
/// current-thread runtime has no other thread to hand its tasks to, so there the
/// work runs inline; that runtime is for tests and single-worker tools.
pub fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// The checks and setup `spawn_worker` does, without spawning. Returns the loop
/// and its two channel ends.
async fn prepare(
    env: &Arc<WorkerEnv>,
    brief: WorkerBrief,
    options: SpawnOptions,
) -> Result<(Worker, WorkerEndpoint, WorkerHandle), WorkerError> {
    brief.validate()?;
    if !brief.skills.is_empty() {
        return Err(WorkerError::SkillsUnsupported {
            skills: brief.skills.clone(),
        });
    }
    if brief.model != env.prefix.prefix().model {
        return Err(WorkerError::ModelMismatch {
            brief: brief.model.clone(),
            prefix: env.prefix.prefix().model.clone(),
        });
    }
    if env.prefix.tools() != env.tools.prefix_definitions().as_slice() {
        return Err(WorkerError::ToolsNotInPrefix);
    }
    if env.settings.call_mode == CallMode::Stream && env.provider.capabilities().streaming.is_none()
    {
        return Err(WorkerError::StreamingUnavailable {
            adapter: env.provider.adapter(),
        });
    }
    env.prefix
        .refuse_worker_specific(brief.worker.as_str(), brief.task.as_str())?;
    env.prefixes.admit(&env.prefix)?;
    let source = env.slices.clone();
    let nodes = brief.slices.clone();
    let slices = tokio::task::spawn_blocking(move || source.render(&nodes))
        .await
        .map_err(|e| WorkerError::SliceTask(e.to_string()))??;
    let opening = opening_messages(&brief.instruction, &slices);
    // On a resume, the tool calls of the earlier segment's last round are what
    // the next request carries results of: its tool round (see Trace).
    let resumed_tools = options
        .resume_from
        .as_deref()
        .map(last_round_tools)
        .unwrap_or_default();
    let (builder, rounds_used, delivered) = match options.resume_from {
        Some(events) => {
            let state = ResumeState::from_events(opening, &events)?;
            let used = state.rounds_used();
            let delivered = state.injected_ids().clone();
            (state.builder, used, delivered)
        }
        None => (
            TranscriptBuilder::new(opening).with_base_tokens(env.prefix.estimated_tokens()),
            0,
            BTreeSet::new(),
        ),
    };
    // Fail loud: a transcript resumed on a different prefix or different slices would
    // continue a conversation the model never had.
    if let Some(stored) = builder.prefix_digest()
        && stored != env.prefix.digest().as_str()
    {
        // Say so when the difference is the rename (decision 0062).
        if env
            .prefix
            .former_digest()
            .is_some_and(|former| former.as_str() == stored)
        {
            return Err(WorkerError::ResumeFromBeforeRename {
                stored: stored.to_owned(),
            });
        }
        return Err(WorkerError::ResumeMismatch {
            what: "prefix digest",
            stored: stored.to_owned(),
            current: env.prefix.digest().as_str().to_owned(),
        });
    }
    if let Some(stored) = builder.slices()
        && stored != brief.slices.as_slice()
    {
        return Err(WorkerError::ResumeMismatch {
            what: "slices",
            stored: format!("{stored:?}"),
            current: format!("{:?}", brief.slices),
        });
    }
    // What the earlier segments did: their ledger calls, and the writes of the tool
    // calls that succeeded, from the tools' own access declarations.
    let calls = builder.calls().to_vec();
    // A resumed worker continues from its own last call.
    let last_call = calls.last().cloned().or(options.parent_call);
    let mut changed = BTreeSet::new();
    let mut done = Vec::new();
    for (tool, input) in builder.succeeded() {
        let Some(registered) = env.tools.get(tool) else {
            continue;
        };
        if let Ok(access) = registered.tool().access(input) {
            for path in access.writes_paths() {
                changed.insert(path.clone());
                done.push(format!("{tool} wrote {}", path.to_string_lossy()));
            }
        }
    }
    let mut stall = StallDetector::new(env.settings.stall_repeats);
    // Replayed calls count toward the stall threshold; a replay that already
    // reached it would have ended the earlier run, so the result is not acted on.
    let _ = stall.observe(builder.tool_calls().iter().map(|(t, i)| (t.as_str(), i)));
    let (mut handle, endpoint) = worker_channel(brief.worker.clone(), env.settings.bulk_capacity);
    // Decision 0067: the transcript's ids first, so a copy of one of them in
    // `injections` is refused rather than delivered a second time.
    let steering = endpoint.steering();
    // The two kinds of duplicate are reported apart. An id in the
    // transcript is delivered (the caller marks its row delivered); an id
    // listed twice is a caller defect, and one copy is queued.
    let mut already_delivered = Vec::new();
    let mut listed_twice = Vec::new();
    for injection in options.injections {
        if delivered.contains(&injection.id) {
            already_delivered.push(injection.id);
            continue;
        }
        match steering.push_at_spawn(&brief.worker, injection) {
            Ok(()) => {}
            Err(InjectError::Duplicate { id }) => listed_twice.push(id),
            // The queue is fresh and open; nothing else can refuse here.
            Err(other) => return Err(WorkerError::Injection(other)),
        }
    }
    // Seeded after the spawn items, so a later `inject` of a transcript id is
    // refused as a duplicate too.
    steering.seed_delivered(delivered);
    handle.set_spawn_duplicates(already_delivered, listed_twice);
    let worker = Worker {
        env: env.clone(),
        approval: options.approval.unwrap_or_else(|| env.approval.clone()),
        budget: StepBudget::new(brief.step_budget, rounds_used),
        brief,
        seq: EventSequence::new(),
        builder,
        stall,
        phase: CancelPhase::BetweenRounds,
        calls,
        last_call,
        given_tools: resumed_tools,
        trace_failures: Vec::new(),
        round_redactions: kgraf_secrets::Redactions::default(),
        changed,
        done,
        denied: 0,
        fence: options.fence,
        steering,
    };
    Ok((worker, endpoint, handle))
}

/// Why a run stops early.
enum Halt {
    Cancelled {
        phase: CancelPhase,
        partial: Option<(CallId, TokenUsage)>,
    },
    Failed(String),
    /// An interrupt-mode message abandoned the round in flight. Not an end:
    /// `drive` appends `RoundAbandoned` and continues to the boundary.
    Abandoned {
        phase: CancelPhase,
        partial: Option<(CallId, TokenUsage)>,
        /// Whether the request reached the provider. `false` only for the
        /// steer that arrives after `StepBudget::next_round` already counted
        /// the round but before the request left the process: that round
        /// bought no model turn, so `drive` refunds it before the retry
        /// counts its own. `true` everywhere else (mid-stream, mid-dispatch),
        /// where the round already ran and stays spent.
        sent: bool,
    },
}

/// Why a round's tool execution stopped before every call returned.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Interruption {
    /// The worker was cancelled.
    Cancel,
    /// An interrupt-mode message arrived.
    Steer,
}

/// The result a call gets when an interrupt-mode message abandons its round
/// before the call started (decision 0067, point 3).
pub const STEER_NOT_RUN: &str = "ERROR: not run: the worker was redirected by a new message";

/// The result a call gets when an interrupt-mode message abandons its round
/// while the call runs and the call does not return within the cancel grace.
/// Not "not run": the call started, and may have done part of its work.
pub const STEER_INTERRUPTED: &str = "ERROR: interrupted: the worker was redirected by a new \
     message before this call returned a result";

/// Added to [`STEER_INTERRUPTED`] for a call that outlived the grace and may
/// write anywhere (it is exclusive and declares no paths).
pub const STEER_MAY_HAVE_WRITTEN_ANYWHERE: &str =
    "It may write anywhere and may have changed files.";

/// After a steer's grace expires, how long a writing call still running gets
/// to finish its own cancel path before its future is dropped.
/// `ShellTool`'s path is a `killpg` and a reap, which takes milliseconds; the
/// bound only matters for a tool that ignores its token.
pub const STEER_STOP_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

/// The `RoundAbandoned` reason.
pub const STEER_ABANDON_REASON: &str = "an interrupt-mode message arrived";

struct Worker {
    env: Arc<WorkerEnv>,
    approval: ApprovalChain,
    brief: WorkerBrief,
    seq: EventSequence,
    builder: TranscriptBuilder,
    budget: StepBudget,
    stall: StallDetector,
    phase: CancelPhase,
    calls: Vec<CallId>,
    /// The parent of the next round's ledger row (see the module docs, Trace).
    last_call: Option<CallId>,
    /// The previous round's tool calls, in issue order: the tool round the next
    /// request is given, written to its ledger row.
    given_tools: Vec<String>,
    /// Trace writes the ledger refused, reported on the result.
    trace_failures: Vec<String>,
    /// Persist-scrub redactions in this round's streamed deltas, not yet
    /// reported (decision 0071). Dropped when the round's
    /// `AssistantMessage` reports its whole text instead; reported before a
    /// `Cancelled` or `RoundAbandoned` that ends the round unsettled.
    round_redactions: kgraf_secrets::Redactions,
    changed: BTreeSet<PathBuf>,
    done: Vec<String>,
    denied: u32,
    /// This worker's write fence, if it has one (decision 0067, point 5).
    fence: Option<ScopeFence>,
    /// This worker's steering queue (decision 0067, point 3).
    steering: Arc<SteerQueue>,
}

impl Drop for Worker {
    /// A worker that ends by any path, a panic included, refuses further
    /// injections: an `Ok` from `inject` must never mean "queued for nobody".
    fn drop(&mut self) {
        self.steering.close();
    }
}

impl Worker {
    /// Why an approved call's declared writes are refused, if they are.
    ///
    /// Paths compare as a case- and normalization-insensitive volume resolves
    /// them, whatever `path_case` says: a scope check that is stricter on one
    /// volume than another is not a scope check. A call whose access is
    /// exclusive (a shell command, an MCP tool) declares no paths and may write
    /// anywhere, so under a declared scope it is refused.
    ///
    /// **Except a sandboxed build (decision 0084).** An allowlisted build or
    /// test command from a workspace-bound shell tool
    /// ([`kgraf_tools::Access::sandboxed_build`]) runs under a scope: the OS
    /// sandbox confines its writes to the worktree and its scratch directory,
    /// and the verifier's scope check on the committed attempt refuses any
    /// change outside the declared writes. A subagent's fence still refuses it
    /// ([`fence_violation`]): a fence guards paths inside the worktree, which
    /// the sandbox does not.
    fn scope_violation(&self, access: &kgraf_tools::Access) -> Option<String> {
        if access.is_exclusive() && !access.is_sandboxed_build() && self.brief.write_scope.is_some()
        {
            return Some(
                "the call may write anywhere (it declares no paths) and this worker has a declared \
                 write scope; with a write scope, the shell runs only the build and test \
                 allowlist named in its description, and files change only through apply_patch"
                    .into(),
            );
        }
        for path in access.writes_paths() {
            if crate::paths::forbidden_path(path) {
                return Some(format!("{} is not writable by a worker", path.display()));
            }
            if let Some(scope) = &self.brief.write_scope {
                let inside = scope.iter().any(|s| crate::paths::same_path(s, path));
                if !inside {
                    return Some(format!(
                        "{} is outside the declared write scope",
                        path.display()
                    ));
                }
            }
        }
        self.fence.as_ref().and_then(|f| fence_violation(f, access))
    }

    async fn run(mut self, endpoint: WorkerEndpoint) {
        let (outcome, answer) = match self.drive(&endpoint).await {
            Ok(end) => end,
            Err(Halt::Cancelled { phase, partial }) => {
                // Control events: never refused by the cancel, never stuck behind a
                // full channel (see handle.rs). A reader that has gone away cannot
                // see them, and the result below is then dropped with the handle.
                let _ = self.emit(&endpoint, WorkerEvent::Cancelled { phase }).await;
                if let Some((call, usage)) = partial {
                    let _ = self
                        .emit(&endpoint, WorkerEvent::Usage { call, usage })
                        .await;
                }
                (WorkerOutcome::Cancelled, String::new())
            }
            Err(Halt::Failed(error)) => (WorkerOutcome::Failed { error }, String::new()),
            // `drive` handles every abandon itself; one reaching here is a
            // loop defect, reported rather than read as success (fail loud).
            Err(Halt::Abandoned { .. }) => (
                WorkerOutcome::Failed {
                    error: "an abandoned round escaped the loop".into(),
                },
                String::new(),
            ),
        };
        // Persist-scrubbed: the answer is stored
        // in the run record and shown in the shell, the same sinks as the
        // transcript, and reaches this point through no path
        // `WorkerEvent::scrubbed_for_persist` already covers (it is
        // `response.text()`, assembled directly from the model's own stream,
        // not emitted as a `WorkerEvent`).
        let answer = match kgraf_secrets::scrub_persist(&answer) {
            std::borrow::Cow::Borrowed(_) => answer,
            std::borrow::Cow::Owned(scrubbed) => scrubbed,
        };
        // Before the result is delivered: a caller that has seen the result
        // must get `Finished` from `inject`, never a queued `Ok`.
        self.steering.close();
        // Parsed from the already persist-scrubbed answer (decision 0071): a secret inside the rationale block is caught by the
        // same `scrub_persist` call above, never a second, separate scrub. A
        // worker that gave no block (or an empty one) is recorded as `None`,
        // never fabricated (fail loud).
        let rationale = crate::brief::parse_rationale(&answer);
        let result = WorkerResult {
            task: self.brief.task.clone(),
            worker: self.brief.worker.clone(),
            outcome,
            answer,
            changed_paths: self.changed.iter().cloned().collect(),
            commit: None,
            calls: std::mem::take(&mut self.calls),
            steps: self.budget.used(),
            prefix_digest: self.env.prefix.digest().as_str().to_owned(),
            denied: self.denied,
            trace_failures: std::mem::take(&mut self.trace_failures),
            rationale,
        };
        // `Err` means the handle is gone: nobody is waiting for the result, and the
        // ledger already holds every call it names.
        let _ = endpoint.finish(result);
    }

    /// Accept, apply and send one event.
    ///
    /// Scrubbed on two independent paths (decision 0071),
    /// both derived from the same original event so neither can drift:
    /// the transcript builder (`self.builder`, what the next request shows
    /// the model) gets the model-facing scrub (registry only, no pattern
    /// rules -- see `WorkerEvent::scrubbed_for_model`), and `ep.emit` (the
    /// persisted transcript and the shell) gets the persist scrub (registry
    /// plus pattern rules -- see `WorkerEvent::scrubbed_for_persist`). A
    /// secret a tool printed therefore never reaches a provider through a
    /// transcript echo, and a pattern rule's occasional false positive can
    /// only ever land in a stored log or a display, never in text a worker
    /// reasons over or writes back to disk. The sequence checker runs against
    /// the persisted version, which is what is actually appended in order;
    /// scrubbing changes text, never a structural field (`call_id`, `tool`,
    /// a stop reason), so either version would satisfy it identically.
    ///
    /// **A secret split across two streamed deltas** is whole in neither, so
    /// each delta's own scrub misses it. The live stream still carries the
    /// deltas as they arrive (holding them back broke live delivery and hung
    /// `a_worker_cancelled_mid_stream_resumes_from_its_durable_transcript`).
    /// What is written never carries them: the round's `AssistantMessage` is
    /// scrubbed whole, and the transcript writers hold deltas and write that
    /// message instead (`TranscriptSettler`, decision 0071).
    ///
    /// Redaction counts are sent as a `Redacted` event after the event
    /// they describe; see [`Worker::scrub_both`].
    async fn emit(&mut self, ep: &WorkerEndpoint, event: WorkerEvent) -> Result<(), Halt> {
        let (model_event, event, report) = self.scrub_both(event);
        if let Some(pending) = self.unsettled_redactions(&event) {
            self.send(ep, pending, None).await?;
        }
        self.send(ep, event, Some(&model_event)).await?;
        match report {
            Some(counts) => self.send(ep, counts, None).await,
            None => Ok(()),
        }
    }

    /// Scrub `event` for both paths (see [`Worker::emit`]) and decide what
    /// its redaction count becomes (decision 0071): a delta's count
    /// waits in `round_redactions`; an `AssistantMessage` replaces the
    /// round's delta counts with its own; anything else is reported at once.
    fn scrub_both(
        &mut self,
        event: WorkerEvent,
    ) -> (WorkerEvent, WorkerEvent, Option<WorkerEvent>) {
        let model_event = event.clone().scrubbed_for_model();
        let (event, counts) = event.scrubbed_for_persist_counted();
        let report = match &event {
            WorkerEvent::TextDelta { .. } | WorkerEvent::ReasoningDelta { .. } => {
                self.round_redactions.merge(&counts);
                None
            }
            WorkerEvent::AssistantMessage { .. } => {
                self.round_redactions = kgraf_secrets::Redactions::default();
                redacted_event(counts)
            }
            _ => redacted_event(counts),
        };
        (model_event, event, report)
    }

    /// The round's held delta counts, when `event` ends the round unsettled.
    fn unsettled_redactions(&mut self, event: &WorkerEvent) -> Option<WorkerEvent> {
        match event {
            WorkerEvent::Cancelled { .. } | WorkerEvent::RoundAbandoned { .. } => {
                redacted_event(std::mem::take(&mut self.round_redactions))
            }
            _ => None,
        }
    }

    /// Accept one already-scrubbed event, apply its model copy (when it has
    /// one) and send it.
    async fn send(
        &mut self,
        ep: &WorkerEndpoint,
        event: WorkerEvent,
        model_event: Option<&WorkerEvent>,
    ) -> Result<(), Halt> {
        self.seq
            .accept(&event)
            .map_err(|e| Halt::Failed(format!("the loop produced an out-of-order event: {e}")))?;
        if let Some(model_event) = model_event {
            self.builder.apply(model_event);
        }
        // A requested cancel outranks everything: the run was cancelled. A reader
        // that has gone away without a cancel is `WorkerHandle::result` discarding
        // the events it did not read, which its contract allows: the run goes on,
        // and its result still arrives. The loop keeps its own transcript
        // (`builder`), so nothing it needs is lost.
        match ep.emit(event).await {
            Ok(()) => Ok(()),
            Err(_) if ep.is_cancelled() => Err(Halt::Cancelled {
                phase: self.phase,
                partial: None,
            }),
            Err(RuntimeError::HandleDropped { .. }) => Ok(()),
            Err(other) => Err(Halt::Failed(other.to_string())),
        }
    }

    /// Emit the round's settled [`WorkerEvent::AssistantMessage`]
    /// (decision 0071), then `Stop`, in that order, at every one of
    /// this function's four call sites (every branch that ends a model round
    /// naturally). Built from the assembled `response`, not from the deltas
    /// that streamed it, so it is scrubbed as one whole string on the persist
    /// path -- catching a secret split across two deltas, which the persist
    /// scrub of each individual delta cannot.
    async fn emit_stop(
        &mut self,
        ep: &WorkerEndpoint,
        reason: StopReason,
        response: &Response,
    ) -> Result<(), Halt> {
        self.emit(
            ep,
            WorkerEvent::AssistantMessage {
                text: response.text(),
                reasoning: response.reasoning(),
            },
        )
        .await?;
        self.emit(ep, WorkerEvent::Stop { reason }).await
    }

    async fn drive(&mut self, ep: &WorkerEndpoint) -> Result<(WorkerOutcome, String), Halt> {
        let context = WorkerEvent::Context {
            prefix_digest: self.env.prefix.digest().as_str().to_owned(),
            slices: self.brief.slices.clone(),
        };
        self.emit(ep, context).await?;
        loop {
            self.phase = CancelPhase::BetweenRounds;
            if ep.is_cancelled() {
                return Err(Halt::Cancelled {
                    phase: CancelPhase::BetweenRounds,
                    partial: None,
                });
            }
            // The queued messages are drained after these checks,
            // so their size is counted here without draining them. A round
            // they would push past the stop point is not run; the worker hands
            // back and the messages stay undelivered (`Injector::undelivered`).
            let queued = self.steering.queued_tokens();
            // The queued share gets the margin of
            // `conservative_context_tokens` (two bytes per token, not three).
            // Dense text (CJK, base64) tokenizes above the three-byte
            // estimate, and a note is not yet anchored by any usage report.
            let context_tokens =
                self.builder.context_tokens() + queued.saturating_mul(3).div_ceil(2);
            if self.brief.context.reached_point(context_tokens) {
                return Ok((self.hand_back(context_tokens), String::new()));
            }
            let final_round = match self.budget.next_round() {
                RoundPlan::Exhausted => return Ok((WorkerOutcome::BudgetExhausted, String::new())),
                RoundPlan::Run {
                    final_round,
                    notice,
                    ..
                } => {
                    if let Some((kind, text)) = notice {
                        self.emit(ep, WorkerEvent::Notice { kind, text }).await?;
                    }
                    final_round
                }
            };
            self.deliver_injections(ep)?;
            let messages = self.env.masker.mask(self.builder.messages());
            let request = self.env.prefix.request(
                messages,
                self.env.settings.max_output_tokens,
                self.env.settings.temperature,
            );
            self.env
                .prefix
                .check(&request)
                .map_err(|e| Halt::Failed(e.to_string()))?;
            let response = match self.call_model(ep, &request, final_round).await {
                Err(Halt::Abandoned {
                    phase,
                    partial,
                    sent,
                }) => {
                    if !sent {
                        // The round `next_round` counted above never reached the
                        // provider, so it bought no model turn. Refund it before
                        // the retry at the top of the loop counts its own, or a
                        // steer arriving every round would spend two rounds of
                        // budget per model turn and could end the run
                        // `BudgetExhausted` with the model never having acted.
                        self.budget.refund_round();
                    }
                    self.abandon(ep, phase, partial).await?;
                    continue;
                }
                other => other?,
            };
            let uses: Vec<(String, String, serde_json::Value)> = response
                .tool_uses()
                .into_iter()
                .map(|(id, name, input)| (id.to_owned(), name.to_owned(), input.clone()))
                .collect();
            let reason = response.stop_reason;
            match (reason, uses.is_empty()) {
                (StopReason::ToolUse, true) => {
                    return Err(Halt::Failed(
                        "the model stopped for a tool call and the response carries none".into(),
                    ));
                }
                (StopReason::ToolUse, false) => {}
                (other, false) => {
                    return Err(Halt::Failed(format!(
                        "the response carries tool calls but stopped with {other:?}"
                    )));
                }
                // The provider paused the turn for a server-side tool it is still
                // running, not for one this build has to call. There is nothing to
                // dispatch, so this falls through the same way `ToolUse` with no
                // calls of its own would: the round's assistant text is recorded
                // below and the loop goes on to the next round, which sends the
                // conversation (now carrying the paused response) back to the
                // provider. `final_round` still applies: a turn paused with no
                // rounds left is handled exactly like a tool call with no rounds
                // left, further down.
                (StopReason::PauseTurn, true) => {}
                (StopReason::EndTurn | StopReason::StopSequence, true) => {
                    self.emit_stop(ep, reason, &response).await?;
                    return Ok((WorkerOutcome::Completed, response.text()));
                }
                (StopReason::MaxTokens, true) => {
                    self.emit_stop(ep, reason, &response).await?;
                    return Ok((
                        WorkerOutcome::Failed {
                            error: "the output ceiling was reached before the answer finished"
                                .into(),
                        },
                        response.text(),
                    ));
                }
                (StopReason::Other, true) => {
                    self.emit_stop(ep, reason, &response).await?;
                    return Ok((
                        WorkerOutcome::Failed {
                            error: "the provider ended the turn with an unmodeled stop reason"
                                .into(),
                        },
                        response.text(),
                    ));
                }
            }
            self.emit_stop(ep, reason, &response).await?;
            // Every call of the round gets a result (run, denied or refused), so
            // every one is in the tool round the next request is given.
            self.given_tools = uses.iter().map(|(_, name, _)| name.clone()).collect();
            self.phase = CancelPhase::ToolCall;
            for (id, name, input) in &uses {
                let started = WorkerEvent::ToolCallStarted {
                    call_id: id.clone(),
                    tool: name.clone(),
                    input: input.clone(),
                };
                self.emit(ep, started).await?;
            }
            if final_round {
                // A turn that ends on a tool call produces no answer.
                let ids: Vec<String> = uses.iter().map(|u| u.0.clone()).collect();
                self.refuse_all(ep, ids, "the step budget is spent").await?;
                return Ok((WorkerOutcome::BudgetExhausted, String::new()));
            }
            match self.dispatch(ep, uses).await {
                Ok(Some(stall)) => {
                    return Ok((
                        WorkerOutcome::Stalled {
                            call_digest: stall.call_digest,
                            repeats: stall.repeats,
                        },
                        String::new(),
                    ));
                }
                Ok(None) => {}
                Err(Halt::Abandoned { phase, partial, .. }) => {
                    // Reached only from `dispatch`'s own `Halt::Abandoned` (below),
                    // which always fires after the round's model call already ran;
                    // the round stays spent, so no refund here.
                    self.abandon(ep, phase, partial).await?;
                }
                Err(halt) => return Err(halt),
            }
        }
    }

    /// Drain the steering queue into the transcript, in order
    /// (`SteerQueue::deliver`: an item that could not be emitted goes back to
    /// the queue front with every item after it, so it stays listed as
    /// undelivered and nothing overtakes it).
    ///
    /// Synchronous: `Injected` is a control event, which never waits for the
    /// reader, so nothing can run between taking the items and recording them.
    fn deliver_injections(&mut self, ep: &WorkerEndpoint) -> Result<(), Halt> {
        let steering = self.steering.clone();
        steering.deliver(|item| {
            let event = WorkerEvent::Injected {
                id: item.id.clone(),
                source: item.source.clone(),
                kind: item.kind,
                text: item.text.clone(),
            };
            self.emit_after_cancel(ep, event)
        })
    }

    /// Record an abandoned round: `RoundAbandoned` closes it (the rebuild drops
    /// its partial text and pairs any open tool use), then the partial usage of
    /// its cancelled call, if the stream reported any (every token is attributed).
    async fn abandon(
        &mut self,
        ep: &WorkerEndpoint,
        phase: CancelPhase,
        partial: Option<(CallId, TokenUsage)>,
    ) -> Result<(), Halt> {
        let abandoned = WorkerEvent::RoundAbandoned {
            phase,
            reason: STEER_ABANDON_REASON.to_owned(),
        };
        self.emit(ep, abandoned).await?;
        if let Some((call, usage)) = partial {
            self.emit(ep, WorkerEvent::Usage { call, usage }).await?;
        }
        Ok(())
    }

    /// Finish every started call of the round with a refusal, so the transcript
    /// pairs each tool use with a result.
    async fn refuse_all(
        &mut self,
        ep: &WorkerEndpoint,
        ids: Vec<String>,
        why: &str,
    ) -> Result<(), Halt> {
        for id in ids {
            let finished = WorkerEvent::ToolCallFinished {
                call_id: id.clone(),
                output: format!("ERROR: not run: {why}"),
                is_error: true,
            };
            self.emit(ep, finished).await?;
        }
        Ok(())
    }

    fn hand_back(&self, context_tokens: u64) -> WorkerOutcome {
        let stop = self.brief.context.worker_stop_tokens();
        WorkerOutcome::HandedBack {
            handoff: Handoff {
                done: self.done.clone(),
                remaining: vec![format!(
                    "Continue the original instruction; the worker stopped at the context \
                     stop point ({context_tokens} of {stop} tokens). Instruction: {}",
                    self.brief.instruction
                )],
                files_touched: self.changed.iter().cloned().collect(),
                context_tokens,
            },
        }
    }

    /// One accounted model call, raced against cancellation.
    async fn call_model(
        &mut self,
        ep: &WorkerEndpoint,
        request: &Request,
        final_round: bool,
    ) -> Result<Response, Halt> {
        self.phase = CancelPhase::ModelStream;
        let provider = self.env.provider.clone();
        let ledger = self.env.ledger.clone();
        let trace = CallTrace {
            parent_call: self.last_call.clone(),
            kind: Some("worker_round".to_owned()),
            role: Some("worker".to_owned()),
            given_slice_ids: self
                .brief
                .slices
                .iter()
                .map(|n| n.as_str().to_owned())
                .collect(),
            given_prefix_hash: Some(self.env.prefix.digest().as_str().to_owned()),
            ..CallTrace::default()
        };
        let given_tools = std::mem::take(&mut self.given_tools);
        // Awaited, not run on this worker thread: the ledger's writer thread
        // commits it, so the runtime thread stays free for other workers.
        let token = ledger
            .begin_traced_async(
                provider.adapter(),
                provider.protocol(),
                &request.model,
                self.brief.worker.as_str(),
                self.brief.task.as_str(),
                trace,
            )
            .await
            .map_err(|e| Halt::Failed(format!("cannot open a ledger row: {e}")))?;
        // In flight, so accepted unless the ledger fails; see Trace.
        let round = if given_tools.is_empty() {
            Ok(())
        } else {
            token.record_tool_round_async(&given_tools).await
        };
        if let Err(e) = round {
            self.trace_failures
                .push(format!("call {}: tool round not recorded: {e}", token.id()));
        }
        let call = token.id().clone();
        self.calls.push(call.clone());
        self.last_call = Some(call.clone());
        // Before the request leaves: a round that fails without usage still counts.
        self.emit(ep, WorkerEvent::ModelCall { call: call.clone() })
            .await?;

        // Decision 0067: an interrupt that arrived after the drain and before
        // this point (the ledger write above is an await) finds a request that
        // has not left the process. Settled cancelled at zero, a known spend,
        // rather than raced below, where the biased select would abandon the
        // never-polled call and leave its row unreconciled as unknown spend.
        // An arm that lands between this check and the select's first poll
        // still takes the conservative path below. The zero is returned as
        // the partial usage, so the ledger settles it as known.
        //
        // Never in the final round. Abandoning it would spend the
        // last round, end the run `BudgetExhausted` with no answer, and still
        // leave the message undelivered. The final answer is let finish, and
        // the message is a leftover the caller reads from
        // `Injector::undelivered` and routes to the orchestrator.
        if !final_round && self.steering.is_armed() {
            let zero = TokenUsage::default();
            let _ = token.cancel_async(Some(&zero)).await;
            return Err(Halt::Abandoned {
                phase: CancelPhase::ModelStream,
                partial: Some((call, zero)),
                // The request never left this function: no provider was called.
                sent: false,
            });
        }
        // An interrupt-mode message abandons the call in flight. Raced after
        // cancel, which outranks it. Not raced in the final round (above).
        let steering = self.steering.clone();
        let interrupt = async move {
            if final_round {
                std::future::pending::<()>().await;
            }
            steering.interrupt_armed().await;
        };
        tokio::pin!(interrupt);
        let response = match self.env.settings.call_mode {
            CallMode::Complete => {
                let outcome = tokio::select! {
                    biased;
                    () = ep.cancelled() => Err(Interruption::Cancel),
                    () = &mut interrupt => Err(Interruption::Steer),
                    r = provider.complete(request, &token) => Ok(r),
                };
                match outcome {
                    Err(why) => {
                        // No usage was received, so the spend is unknown, not zero.
                        settle_cancelled(&token, &StreamSeen::default()).await;
                        return Err(match why {
                            Interruption::Cancel => Halt::Cancelled {
                                phase: CancelPhase::ModelStream,
                                partial: None,
                            },
                            Interruption::Steer => Halt::Abandoned {
                                phase: CancelPhase::ModelStream,
                                partial: None,
                                // Raced against `provider.complete`, already polled:
                                // the request is in flight.
                                sent: true,
                            },
                        });
                    }
                    Ok(Err(e)) => return Err(Halt::Failed(e.to_string())),
                    Ok(Ok(response)) => {
                        for block in &response.content {
                            let event = match block {
                                Content::Text { text, .. } if !text.is_empty() => {
                                    WorkerEvent::TextDelta { text: text.clone() }
                                }
                                Content::Reasoning { text } if !text.is_empty() => {
                                    WorkerEvent::ReasoningDelta { text: text.clone() }
                                }
                                _ => continue,
                            };
                            self.emit(ep, event).await?;
                        }
                        response
                    }
                }
            }
            CallMode::Stream => {
                let Some(streaming) = provider.capabilities().streaming else {
                    // Checked at spawn; a provider whose capabilities changed since
                    // is refused, never called another way.
                    let error = kgraf_provider::ProviderError::CapabilityNotHonored {
                        adapter: provider.adapter(),
                        capability: "streaming",
                    };
                    let _ = token.close_on_async(&error, None).await;
                    return Err(Halt::Failed(error.to_string()));
                };
                let (sink, mut pump) = stream_bridge(self.env.settings.bridge_limit_bytes);
                // The time-to-first-token write, submitted from inside the sink
                // (which cannot await) and awaited once the stream has ended. The
                // ledger applies it before the adapter's settlement, which is
                // submitted after it.
                let ttft_ack: std::sync::Mutex<Option<kgraf_provider::LedgerAck<()>>> =
                    std::sync::Mutex::new(None);
                let mut seen = StreamSeen::default();
                let mut overflow: Option<RuntimeError> = None;
                enum End {
                    Finished(Result<Response, kgraf_provider::ProviderError>),
                    Cancelled,
                    Interrupted,
                    Halted(Halt),
                }
                let end = {
                    let mut sink = sink;
                    // Time to first token, taken where the adapter delivers the
                    // first output delta and written while the row is still in
                    // flight: the adapter settles before its stream returns, and
                    // the ledger refuses trace writes after settlement.
                    let started = std::time::Instant::now();
                    let mut first_token = false;
                    let ttft_token = &token;
                    let ttft_ack = &ttft_ack;
                    let mut push = move |event: StreamEvent| {
                        if !first_token && matches!(event, StreamEvent::Delta { .. }) {
                            first_token = true;
                            let ms =
                                i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
                            *ttft_ack.lock().unwrap_or_else(|p| p.into_inner()) =
                                Some(ttft_token.record_first_token_async(ms));
                        }
                        sink.push(event)
                    };
                    let stream = streaming.stream(request, &token, &mut push);
                    tokio::pin!(stream);
                    loop {
                        tokio::select! {
                            biased;
                            () = ep.cancelled() => break End::Cancelled,
                            () = &mut interrupt => break End::Interrupted,
                            r = &mut stream => break End::Finished(r),
                            item = pump.next() => match item {
                                Ok(Some(event)) => {
                                    if let Err(halt) =
                                        self.forward(ep, event, &call, &mut seen).await
                                    {
                                        break End::Halted(halt);
                                    }
                                }
                                // The sink lives in this block, so the pump cannot
                                // close here; nothing to do if it somehow does.
                                Ok(None) => {}
                                Err(e) => overflow = Some(e),
                            },
                        }
                    }
                    // The stream future, the closure and the sink drop here, which
                    // closes the bridge so the pump drains to its end below.
                };
                let ttft = ttft_ack.into_inner().unwrap_or_else(|p| p.into_inner());
                if let Some(ack) = ttft
                    && let Err(e) = ack.await
                {
                    self.trace_failures.push(format!(
                        "call {call}: time to first token not recorded: {e}"
                    ));
                }
                match end {
                    End::Finished(result) => {
                        // Everything the stream pushed is still in the bridge.
                        loop {
                            match pump.next().await {
                                Ok(Some(event)) => {
                                    self.forward(ep, event, &call, &mut seen).await?
                                }
                                Ok(None) => break,
                                Err(e) => overflow = Some(e),
                            }
                        }
                        let response = result.map_err(|e| Halt::Failed(e.to_string()))?;
                        if let Some(e) = overflow {
                            return Err(Halt::Failed(e.to_string()));
                        }
                        response
                    }
                    End::Cancelled | End::Interrupted | End::Halted(_) => {
                        // Take the last cumulative usage the stream delivered, without
                        // forwarding more output. A dropped overflow here is
                        // an operator-visible defect, not silently swallowed: a full
                        // bridge lost events the ledger and the transcript never saw,
                        // so it is folded into `trace_failures` and surfaces on the
                        // worker's result the same way a ledger write failure does.
                        let cause = match &end {
                            End::Cancelled => "cancel",
                            End::Interrupted => "steer",
                            _ => "halt",
                        };
                        // An overflow the select loop above saw before it broke out
                        // (bridge full while still forwarding) is folded in here too;
                        // it must not be dropped just because the break happened on
                        // the same tick.
                        if let Some(e) = overflow.take() {
                            self.trace_failures.push(format!(
                                "call {call}: stream drain after {cause} overflowed: {e}"
                            ));
                        }
                        loop {
                            match pump.next().await {
                                Ok(Some(event)) => seen.observe(&event),
                                Ok(None) => break,
                                Err(e) => self.trace_failures.push(format!(
                                    "call {call}: stream drain after {cause} overflowed: {e}"
                                )),
                            }
                        }
                        settle_cancelled(&token, &seen).await;
                        let partial = seen.usage.map(|usage| (call, usage));
                        return Err(match end {
                            End::Halted(Halt::Failed(e)) => Halt::Failed(e),
                            End::Interrupted => Halt::Abandoned {
                                phase: CancelPhase::ModelStream,
                                partial,
                                // The stream already ran, so the request left the
                                // process; the round it bought stays spent.
                                sent: true,
                            },
                            _ => Halt::Cancelled {
                                phase: CancelPhase::ModelStream,
                                partial,
                            },
                        });
                    }
                }
            }
        };
        // One usage event per round, carrying the settled figure: it counts the
        // round on resume and anchors the context estimate.
        let usage = WorkerEvent::Usage {
            call,
            usage: response.usage.clone(),
        };
        self.emit(ep, usage).await?;
        Ok(response)
    }

    /// Forward one stream event as a neutral event. `Finished` is not forwarded:
    /// the loop emits `Stop` itself from the assembled response, once.
    async fn forward(
        &mut self,
        ep: &WorkerEndpoint,
        event: StreamEvent,
        call: &CallId,
        seen: &mut StreamSeen,
    ) -> Result<(), Halt> {
        if matches!(event, StreamEvent::Finished { .. }) {
            return Ok(());
        }
        seen.observe(&event);
        match WorkerEvent::from_stream(event, call) {
            Some(neutral) => self.emit(ep, neutral).await,
            None => Ok(()),
        }
    }

    /// Approve the round's calls in issue order, then run the approved ones:
    /// conflicting calls in issue order, independent calls concurrently.
    async fn dispatch(
        &mut self,
        ep: &WorkerEndpoint,
        uses: Vec<(String, String, serde_json::Value)>,
    ) -> Result<Option<crate::stall::Stall>, Halt> {
        let registry = self.env.tools.clone();
        // Deferred-tool discovery (tool_search) is another stream's; until it lands
        // a worker can call only prefix tools, and a deferred one is refused by the
        // view, recorded as a denial.
        let discovered = DiscoveredTools::new();
        let view = ToolView {
            registry: &registry,
            discovered: &discovered,
        };
        let chain = self.approval.clone();
        let asks = self.env.asks.clone();
        let mut approved = Vec::new();
        for (call_id, tool, input) in uses {
            let pending = PendingCall {
                call_id: call_id.clone(),
                tool,
                input,
                worker: self.brief.worker.clone(),
                task: self.brief.task.clone(),
            };
            let mut verdict = {
                let mut log = RecordingLog {
                    ep,
                    seq: &mut self.seq,
                };
                chain
                    .review(view, pending, &mut log)
                    .await
                    .map_err(approval_halt)?
            };
            loop {
                match verdict {
                    Verdict::Run(call) => {
                        if let Some(why) = self.scope_violation(call.access()) {
                            self.denied += 1;
                            let finished = WorkerEvent::ToolCallFinished {
                                call_id: call_id.clone(),
                                output: format!("ERROR: refused: {why}"),
                                is_error: true,
                            };
                            self.emit(ep, finished).await?;
                            break;
                        }
                        approved.push(call);
                        break;
                    }
                    Verdict::Denied { by, reason } => {
                        self.denied += 1;
                        let finished = WorkerEvent::ToolCallFinished {
                            call_id: call_id.clone(),
                            output: format!("ERROR: denied by {by}: {reason}"),
                            is_error: true,
                        };
                        self.emit(ep, finished).await?;
                        break;
                    }
                    Verdict::Ask(paused) => {
                        let Some(resolver) = asks.as_ref() else {
                            return Err(Halt::Failed(format!(
                                "tool call `{call_id}` needs approval ({}) and this worker has no one to ask",
                                paused.question()
                            )));
                        };
                        let answer = tokio::select! {
                            biased;
                            () = ep.cancelled() => {
                                return Err(Halt::Cancelled { phase: CancelPhase::ToolCall, partial: None });
                            }
                            answer = resolver.answer(&paused) => answer,
                        };
                        let mut log = RecordingLog {
                            ep,
                            seq: &mut self.seq,
                        };
                        verdict = chain
                            .answer(view, paused, answer, &mut log)
                            .await
                            .map_err(approval_halt)?;
                    }
                }
            }
        }
        // Only approved calls count toward a stall: a denied call did nothing.
        if let Some(stall) = self
            .stall
            .observe(approved.iter().map(|a| (a.tool(), a.input())))
        {
            let ids: Vec<String> = approved.iter().map(|a| a.call_id().to_owned()).collect();
            self.refuse_all(ep, ids, "the same call repeated; stall detected")
                .await?;
            return Ok(Some(stall));
        }
        self.execute(ep, &registry, approved).await?;
        Ok(None)
    }

    async fn execute(
        &mut self,
        ep: &WorkerEndpoint,
        registry: &ToolRegistry,
        approved: Vec<ApprovedCall>,
    ) -> Result<(), Halt> {
        if approved.is_empty() {
            return Ok(());
        }
        let accesses: Vec<kgraf_tools::Access> =
            approved.iter().map(|a| a.access().clone()).collect();
        let waits = dependencies(&accesses, self.env.settings.path_case);
        let ids: Vec<String> = approved.iter().map(|a| a.call_id().to_owned()).collect();
        let tools: Vec<String> = approved.iter().map(|a| a.tool().to_owned()).collect();
        // Decision 0067: a call that declares writes is never
        // cancelled by a steer. Cancelling it would not stop work already
        // handed to a blocking thread (apply_patch), so the transcript would
        // say "cancelled" while the file changed. It gets the grace instead,
        // and keeps its real result if it returns in time.
        let writes: Vec<bool> = accesses.iter().map(declares_writes).collect();
        // Both are children of the worker's token, so a worker cancel still
        // reaches every tool. A steer cancels only `steer_cancel`.
        let steer_cancel = ep.cancellation_token().child_token();
        let contexts = RoundContexts {
            read_only: ToolContext {
                worker: self.brief.worker.clone(),
                task: self.brief.task.clone(),
                workspace_root: self.brief.workspace_root.clone(),
                cancel: steer_cancel.clone(),
                // Each call below gets its own, call-id-attributed sink
                // (`start_ready`); this template's own sink is never read.
                progress: ProgressSink::default(),
            },
            writing: ToolContext {
                worker: self.brief.worker.clone(),
                task: self.brief.task.clone(),
                workspace_root: self.brief.workspace_root.clone(),
                cancel: ep.cancellation_token().child_token(),
                progress: ProgressSink::default(),
            },
        };
        // Decision 0067: a tool call's
        // `ProgressSink` reports here, attributed by call id, so a wait
        // (`ShellTool` finding no free execution slot) becomes a
        // `WorkerEvent::Waiting`/`Resumed` pair the transcript and the live
        // view show, without needing `&mut self` reachable from inside a
        // concurrently running tool call: the report only ever crosses this
        // channel, and this loop is the sole place that calls `self.emit`.
        let (progress_tx, mut progress_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, Option<String>)>();
        let interrupt = self.steering.interrupt_armed();
        tokio::pin!(interrupt);
        let mut calls: Vec<Option<ApprovedCall>> = approved.into_iter().map(Some).collect();
        let mut done = vec![false; calls.len()];
        let mut running: FuturesUnordered<Running<'_>> = FuturesUnordered::new();
        // No call starts once a cancel or an interrupt is pending.
        // The select below sees either only when it next runs, and a call
        // started in between would run after the user asked the round to stop.
        if !self.stopping(ep) {
            start_ready(
                &mut calls,
                &waits,
                &done,
                &writes,
                &mut running,
                registry,
                &contexts,
                self.fence.as_ref(),
                &progress_tx,
            );
        }
        let why = loop {
            let next = tokio::select! {
                biased;
                () = ep.cancelled() => break Interruption::Cancel,
                () = &mut interrupt => break Interruption::Steer,
                Some((call_id, reason)) = progress_rx.recv() => {
                    let event = match reason {
                        Some(reason) => WorkerEvent::Waiting { call_id, reason },
                        None => WorkerEvent::Resumed { call_id },
                    };
                    self.emit(ep, event).await?;
                    continue;
                }
                next = running.next() => next,
            };
            let Some((j, call_id, tool, result)) = next else {
                return Ok(());
            };
            // A call's own `resumed()` report (if it made one) is sent,
            // synchronously, strictly before its future can return
            // `Poll::Ready` (they are sequential statements in the tool's
            // own `call`): by the time `running.next()` above reports this
            // call finished, any progress report it made is already sitting
            // in the channel, so draining here (non-blocking, no `.await`
            // race against the `select!` above) guarantees a `Resumed`
            // reaches the transcript before the call's own
            // `ToolCallFinished`, deterministically, not by scheduling luck.
            while let Ok((report_id, reason)) = progress_rx.try_recv() {
                let event = match reason {
                    Some(reason) => WorkerEvent::Waiting {
                        call_id: report_id,
                        reason,
                    },
                    None => WorkerEvent::Resumed { call_id: report_id },
                };
                self.emit(ep, event).await?;
            }
            done[j] = true;
            let finished = self.record_result(j, &accesses, call_id, &tool, result);
            self.emit(ep, finished).await?;
            // The emit above can wait for a bulk slot, which is exactly when
            // an interrupt can arrive: checked after it, not before.
            if !self.stopping(ep) {
                start_ready(
                    &mut calls,
                    &waits,
                    &done,
                    &writes,
                    &mut running,
                    registry,
                    &contexts,
                    self.fence.as_ref(),
                    &progress_tx,
                );
            }
        };
        if why == Interruption::Steer {
            steer_cancel.cancel();
        }

        // Stopped with calls running. Read-only calls see a cancelled token
        // (a worker cancel reaches both contexts as their parent; a steer
        // reaches only the read-only one). Give every running call a bounded
        // time to return, and record what each actually did: a write that
        // completed is a change in the worktree, and reporting it as
        // interrupted would hide it. After a cancel, results go out without
        // waiting for a bulk slot (a bulk emit is refused after a cancel).
        let grace = tokio::time::sleep(self.env.settings.tool_cancel_grace);
        tokio::pin!(grace);
        loop {
            let next = tokio::select! {
                biased;
                next = running.next() => next,
                () = &mut grace => break,
            };
            let Some((j, call_id, tool, result)) = next else {
                break;
            };
            done[j] = true;
            let finished = self.record_result(j, &accesses, call_id, &tool, result);
            self.emit_round_end(ep, finished).await?;
        }
        // On a steer, writing calls were let run through the grace.
        // One still running now is stopped through its own cancel path
        // first: dropping the future alone would kill at most a direct child
        // (`kill_on_drop`), and a shell command's process group (a build's
        // workers, a backgrounded grandchild) would run on without bound.
        // `ShellTool` kills the whole group when its token is cancelled. What
        // each such call returns is kept and shown after the synthetic
        // result, since it is what the call itself said as it stopped.
        let mut after_stop: std::collections::BTreeMap<usize, String> =
            std::collections::BTreeMap::new();
        if why == Interruption::Steer && !running.is_empty() {
            contexts.writing.cancel.cancel();
            let reap = tokio::time::sleep(STEER_STOP_WINDOW);
            tokio::pin!(reap);
            loop {
                let next = tokio::select! {
                    biased;
                    next = running.next() => next,
                    () = &mut reap => break,
                };
                let Some((j, _call_id, _tool, result)) = next else {
                    break;
                };
                let own = match result {
                    Ok(output) => output.render(),
                    Err(e) => format!("ERROR: {e}"),
                };
                after_stop.insert(j, own);
            }
        }
        // Stop what is still running: the futures run inside this task, so
        // dropping them ends them at their next await. Work already handed
        // to a blocking thread is not stopped by this.
        drop(running);
        // Calls started and not returned within the grace: `calls[j]` was
        // taken when the call started.
        let outlived: Vec<usize> = (0..ids.len())
            .filter(|&j| !done[j] && calls[j].is_none())
            .collect();
        // Calls still waiting on a conflicting call, or held back because the
        // round was stopping, never started.
        let unstarted: Vec<String> = calls
            .iter_mut()
            .filter_map(Option::take)
            .map(|call| call.call_id().to_owned())
            .collect();
        match why {
            Interruption::Cancel => {
                for call_id in unstarted {
                    let finished = WorkerEvent::ToolCallFinished {
                        call_id,
                        output: "ERROR: not run: the worker was cancelled before this call started"
                            .into(),
                        is_error: true,
                    };
                    self.emit_after_cancel(ep, finished)?;
                }
                // Calls that outlived the grace stay open; the transcript marks
                // them interrupted (INTERRUPTED_RESULT), which is then the truth.
                Err(Halt::Cancelled {
                    phase: CancelPhase::ToolCall,
                    partial: None,
                })
            }
            Interruption::Steer => {
                // Decision 0067: no tool use is left without a result, because
                // the worker goes on to another request.
                let mut synthetic: Vec<(String, String)> = Vec::new();
                for j in outlived {
                    let declared: Vec<String> = accesses[j]
                        .writes_paths()
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect();
                    if declared.is_empty() {
                        let nodes: Vec<&str> = accesses[j]
                            .writes_nodes()
                            .iter()
                            .map(|n| n.as_str())
                            .collect();
                        let output = if accesses[j].is_exclusive() {
                            format!("{STEER_INTERRUPTED} {STEER_MAY_HAVE_WRITTEN_ANYWHERE}")
                        } else if !nodes.is_empty() {
                            format!(
                                "{STEER_INTERRUPTED} It may have written graph nodes: {}.",
                                nodes.join(", ")
                            )
                        } else {
                            STEER_INTERRUPTED.to_owned()
                        };
                        synthetic.push((ids[j].clone(), output));
                        continue;
                    }
                    // The call may still be writing. Its declared
                    // paths count as changed, and the model is told so.
                    for path in accesses[j].writes_paths() {
                        self.changed.insert(path.clone());
                        self.done.push(format!(
                            "{} may have written {}",
                            tools[j],
                            path.to_string_lossy()
                        ));
                    }
                    synthetic.push((
                        ids[j].clone(),
                        format!(
                            "{STEER_INTERRUPTED} It may have written: {}.",
                            declared.join(", ")
                        ),
                    ));
                }
                synthetic.extend(
                    unstarted
                        .into_iter()
                        .map(|id| (id, STEER_NOT_RUN.to_owned())),
                );
                for (call_id, mut output) in synthetic {
                    if let Some(j) = ids.iter().position(|id| *id == call_id)
                        && let Some(own) = after_stop.get(&j)
                    {
                        output.push_str(&format!(" Its own result when stopped: {own}"));
                    }
                    let finished = WorkerEvent::ToolCallFinished {
                        call_id,
                        output,
                        is_error: true,
                    };
                    self.emit_round_end(ep, finished).await?;
                }
                Err(Halt::Abandoned {
                    phase: CancelPhase::ToolCall,
                    partial: None,
                    // The round's model call already ran (this steer arrived
                    // during tool dispatch); the round stays spent.
                    sent: true,
                })
            }
        }
    }

    /// Whether no further tool call may start: the worker is cancelled or an
    /// interrupt-mode message is pending.
    fn stopping(&self, ep: &WorkerEndpoint) -> bool {
        ep.is_cancelled() || self.steering.is_armed()
    }

    /// Emit a result at the end of a stopped round. Once the
    /// worker is cancelled a bulk emit is refused, so after a cancel (even one
    /// that arrives during a steer's grace) the result goes out unbounded and
    /// the rest of the round's results still reach the transcript.
    async fn emit_round_end(
        &mut self,
        ep: &WorkerEndpoint,
        event: WorkerEvent,
    ) -> Result<(), Halt> {
        if ep.is_cancelled() {
            self.emit_after_cancel(ep, event)
        } else {
            self.emit(ep, event).await
        }
    }

    /// Turn one tool result into its transcript event, recording its writes.
    fn record_result(
        &mut self,
        j: usize,
        accesses: &[kgraf_tools::Access],
        call_id: String,
        tool: &str,
        result: Result<ToolOutput, ToolError>,
    ) -> WorkerEvent {
        let (output, is_error) = match result {
            Ok(output) => {
                let is_error = output.is_error;
                (output.render(), is_error)
            }
            Err(e) => (format!("ERROR: {e}"), true),
        };
        if !is_error {
            for path in accesses[j].writes_paths() {
                self.changed.insert(path.clone());
                self.done
                    .push(format!("{tool} wrote {}", path.to_string_lossy()));
            }
        }
        WorkerEvent::ToolCallFinished {
            call_id,
            output,
            is_error,
        }
    }

    /// Accept, apply and send an event after a cancel, without backpressure.
    fn emit_after_cancel(&mut self, ep: &WorkerEndpoint, event: WorkerEvent) -> Result<(), Halt> {
        // Scrubbed on both paths, same as `emit` (decision 0071): a call
        // recorded as interrupted or refused after a cancel can still carry a
        // tool's partial output.
        let (model_event, event, report) = self.scrub_both(event);
        let pending = self.unsettled_redactions(&event);
        for (event, model_event) in pending
            .into_iter()
            .map(|e| (e, None))
            .chain(std::iter::once((event, Some(model_event))))
            .chain(report.into_iter().map(|e| (e, None)))
        {
            self.seq.accept(&event).map_err(|e| {
                Halt::Failed(format!("the loop produced an out-of-order event: {e}"))
            })?;
            if let Some(model_event) = &model_event {
                self.builder.apply(model_event);
            }
            // A reader that has gone away discards the event, as in `emit`.
            let _ = ep.emit_unbounded(event);
        }
        Ok(())
    }
}

/// The transcript event for `counts`, when anything was redacted.
fn redacted_event(counts: kgraf_secrets::Redactions) -> Option<WorkerEvent> {
    (!counts.is_empty()).then(|| WorkerEvent::Redacted {
        counts: counts.into_counts(),
    })
}

type Running<'a> = BoxFuture<'a, (usize, String, String, Result<ToolOutput, ToolError>)>;

/// The two tool contexts of one round: writing calls get a token
/// a steer does not cancel.
struct RoundContexts {
    read_only: ToolContext,
    writing: ToolContext,
}

/// Why a fence refuses a call's declared writes, if it does (decision 0067,
/// point 5). Shared by the approval-time check (`Worker::scope_violation`)
/// and the start-time check (`start_ready`), so the two can never disagree.
///
/// A write is never refused by removing a tool, only by this fence, so a
/// read-only subagent keeps its parent's full tool definitions and the same
/// prefix digest. An exclusive call declares no paths, so
/// `ScopeFence::is_fenced` is never asked about one: without the first check
/// a fenced worker with no declared write scope (a read-only subagent) could
/// still write anywhere through a shell command or an MCP tool, and the fence
/// would fail open on exactly the calls it exists to catch.
fn fence_violation(fence: &ScopeFence, access: &kgraf_tools::Access) -> Option<String> {
    if access.is_exclusive() && fence.is_any_fenced() {
        return Some(
            "the call may write anywhere (it declares no paths) and a subagent currently holds a fence"
                .into(),
        );
    }
    access
        .writes_paths()
        .iter()
        .find(|path| fence.is_fenced(&path.to_string_lossy()))
        .map(|path| format!("{} is fenced while a subagent holds it", path.display()))
}

/// Whether a call may change something: it declares a written path or node,
/// or it is exclusive (a shell command, an MCP tool), which may write anywhere.
fn declares_writes(access: &kgraf_tools::Access) -> bool {
    access.is_exclusive() || !access.writes_paths().is_empty() || !access.writes_nodes().is_empty()
}

/// Start every call whose earlier conflicting calls are done.
///
/// 9 arguments: the round's own per-call state (`calls`/`waits`/`done`/`writes`,
/// one slice per call), plus `running`/`registry`/`contexts`/`progress_tx` and
/// the subagent scope `fence` (decision 0067). A bundling struct was considered and
/// rejected: every field here is borrowed with its own lifetime or mutability
/// from `execute`'s own locals, so a struct would only rename the same borrows
/// one level removed, not reduce them.
#[allow(clippy::too_many_arguments)]
fn start_ready<'a>(
    calls: &mut [Option<ApprovedCall>],
    waits: &[Vec<usize>],
    done: &[bool],
    writes: &[bool],
    running: &mut FuturesUnordered<Running<'a>>,
    registry: &'a ToolRegistry,
    contexts: &'a RoundContexts,
    fence: Option<&ScopeFence>,
    progress_tx: &tokio::sync::mpsc::UnboundedSender<(String, Option<String>)>,
) {
    for (j, slot) in calls.iter_mut().enumerate() {
        if !waits[j].iter().all(|&i| done[i]) {
            continue;
        }
        if let Some(call) = slot.take() {
            // Decision 0067: the fence is checked again at the moment the
            // call starts, not only when the round was approved. Every call of
            // a round is approved before any runs, so a round of
            // `[spawn_subagent(writes: P), write(P)]` approves the write while
            // P is still free; the write then waits for the spawn (their
            // accesses conflict) and would start after the spawn fenced P,
            // while the child is live. Checked here, synchronously, after
            // every conflicting earlier call has finished, nothing that could
            // fence P runs between this check and the write (disjoint writes).
            if let Some(why) = fence.and_then(|f| fence_violation(f, call.access())) {
                let id = call.call_id().to_owned();
                let tool = call.tool().to_owned();
                running.push(Box::pin(async move {
                    (
                        j,
                        id,
                        tool,
                        Ok(ToolOutput::error(format!("refused: {why}"))),
                    )
                }));
                continue;
            }
            let ctx = if writes[j] {
                &contexts.writing
            } else {
                &contexts.read_only
            };
            let id = call.call_id().to_owned();
            let tool = call.tool().to_owned();
            // A per-call clone, its own `ProgressSink` attributed to this
            // call's id (decision 0067): several
            // calls may be running at once (`FuturesUnordered`), so one
            // shared sink could not say which one is waiting.
            let mut call_ctx = ctx.clone();
            let report_id = id.clone();
            let tx = progress_tx.clone();
            call_ctx.progress = ProgressSink::new(move |status| {
                let _ = tx.send((report_id.clone(), status.map(str::to_owned)));
            });
            running.push(Box::pin(async move {
                let result = registry.execute(call, &call_ctx).await;
                (j, id, tool, result)
            }));
        }
    }
}

/// What a stream delivered before it ended or was cancelled.
#[derive(Default)]
struct StreamSeen {
    /// The last cumulative usage, if any usage event arrived.
    usage: Option<TokenUsage>,
    request_id: Option<String>,
}

impl StreamSeen {
    fn observe(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::Usage { usage } => self.usage = Some(usage.clone()),
            StreamEvent::Started {
                request_id: Some(id),
                ..
            } => self.request_id = Some(id.clone()),
            _ => {}
        }
    }
}

/// The tool calls of the last round in `events`, in issue order: those started
/// after the last model call. Empty when the last round made none (it answered,
/// or was cancelled before its response).
fn last_round_tools(events: &[WorkerEvent]) -> Vec<String> {
    let mut tools = Vec::new();
    for event in events {
        match event {
            WorkerEvent::ModelCall { .. } => tools.clear(),
            WorkerEvent::ToolCallStarted { tool, .. } => tools.push(tool.clone()),
            _ => {}
        }
    }
    tools
}

/// Settle a call the loop cancelled (every token is attributed).
///
/// With usage received, the call is `Cancelled` with that partial usage. With
/// none, the spend is unknown rather than zero, so the row is left unreconciled
/// with the request id, where `Ledger::unreconciled` reports it until the
/// provider's own record resolves it. `AlreadySettled` means the adapter finished
/// the call as the cancel landed, and its settlement stands.
async fn settle_cancelled(token: &kgraf_provider::CallToken<'_>, seen: &StreamSeen) {
    // A ledger write that fails leaves the row in flight, which `unreconciled`
    // also reports; there is no better place to surface it from a cancel.
    let ack = match &seen.usage {
        Some(usage) => token.cancel_async(Some(usage)),
        None => token.leave_unreconciled_async(seen.request_id.as_deref()),
    };
    let _ = ack.await;
}

fn approval_halt(e: ApprovalError) -> Halt {
    Halt::Failed(format!("approval could not complete: {e}"))
}

/// Approval decisions go through the loop's event order check, then out as
/// transcript events.
struct RecordingLog<'a> {
    ep: &'a WorkerEndpoint,
    seq: &'a mut EventSequence,
}

#[async_trait::async_trait]
impl ApprovalLog for RecordingLog<'_> {
    async fn record(&mut self, record: ApprovalRecord) -> Result<(), ApprovalError> {
        // Persist-scrubbed: an `ApprovalRecord`
        // never reaches `WorkerEvent::scrubbed_for_model` (approvals are not
        // replayed into model context, only shown -- see `rebuild.rs`), so
        // this is the only scrub it gets. The shell shows a tool call's full
        // argv when it asks for approval, so an unscrubbed `curl -H "token
        // ghp_..."` would otherwise be visible there and in the transcript.
        let (event, counts) = WorkerEvent::Approval { record }.scrubbed_for_persist_counted();
        for event in std::iter::once(event).chain(redacted_event(counts)) {
            self.seq
                .accept(&event)
                .map_err(|e| ApprovalError::Log(e.to_string()))?;
            // As in `Worker::emit`: a reader that has gone away discards the
            // event; anything else withholds the approval.
            match self.ep.emit(event).await {
                Ok(()) | Err(RuntimeError::HandleDropped { .. }) => {}
                Err(e) => return Err(ApprovalError::Log(e.to_string())),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod known_limits {
    /// A secret split across two streamed deltas is not caught by scrubbing
    /// either delta on its own. This is why the live stream can carry both
    /// halves, and why nothing written is built from deltas: the settled
    /// `AssistantMessage` is scrubbed whole, and `TranscriptSettler` writes it
    /// (or a cancelled round's joined partial text) in their place. See
    /// decision 0071.
    #[test]
    fn a_secret_split_across_two_deltas_is_not_caught_by_either_chunks_scrub() {
        let secret = format!("ghp_{}", "a".repeat(36));
        let (part1, part2) = secret.split_at(20);
        assert_eq!(kgraf_secrets::scrub_persist(part1), part1);
        assert_eq!(kgraf_secrets::scrub_persist(part2), part2);
        // Confirms the gap is real, not merely each half looking harmless:
        // scrubbed and reassembled the naive way, the secret still leaks.
        let reassembled = format!(
            "{}{}",
            kgraf_secrets::scrub_persist(part1),
            kgraf_secrets::scrub_persist(part2)
        );
        assert!(reassembled.contains(&secret));
    }
}
