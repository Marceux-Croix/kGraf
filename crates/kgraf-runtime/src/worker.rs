//! The worker loop: build context, call the model through the ledger,
//! dispatch tool calls, iterate, return a structured result.
//!
//! **One round**, as a state machine:
//!
//! ```text
//!   BetweenRounds --cancelled--------------------------------> Cancelled
//!        | context >= stop point ----------------------------> HandedBack
//!        | progress checkpoint due (decision 0096, `progress.rs`):
//!        |   progressing, or grace: the budget grows a block
//!        |   idle twice after a productive one ---------------> BudgetExhausted
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
//!        | a call repeated to the stall threshold, counted
//!        |   since the last successful write or compaction --> Stalled
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
//! **Context (0057 addendum 2, 0095).** Old tool outputs are masked on every
//! request through [`ObservationMasker`], always from the original transcript.
//! When the context reaches the configured stop point the worker stops between
//! rounds. With [`StopPointPolicy::HandBack`] (the default) it returns
//! `HandedBack` with a structured `Handoff`, built from what the loop observed.
//! With [`StopPointPolicy::Compact`], set by a caller that cannot split the
//! remainder, the runtime compacts instead (`compaction.rs`): one accounted
//! summary call, a `Compacted` event the model cannot forge or suppress, and a
//! fresh remainder of the verbatim opening plus the summary. At a later stop
//! point it compacts again while `compaction::recompaction` allows it (room
//! under the stop point, measured progress since the last compaction, below
//! the checkpoint ceiling) and hands back with the reason otherwise. A user's
//! request (`Injector::request_compaction`) compacts at the next boundary the
//! same way, once. A compaction clears the state that describes the replaced
//! context and carries the state that describes the work
//! (`Worker::reset_context_state`).
//!
//! ```text
//!   BetweenRounds, context >= stop point
//!        | never compacted, policy HandBack, wrote something -> HandedBack
//!        | never compacted, policy Compact or nothing written -> compact
//!        | compacted, nothing written, under the window ------> next round
//!        | compacted, policy Compact:
//!        |     room, progress since, below the ceiling -------> compact
//!        |     otherwise ---------------------------> HandedBack (CompactionStop)
//!        | compacted, policy HandBack --------------------------> HandedBack
//!   compact: summary call, Compacted -------------------------> BetweenRounds
//! ```

use crate::brief::{BriefError, Handoff, WorkerBrief, WorkerOutcome, WorkerResult};
use crate::compaction::{
    CompactionNotice, CompactionSummary, CompactionTrigger, HistoryArchive, NOTHING_TO_RECALL,
    RECALL_TOOL, SUMMARY_REQUEST, StopPointPolicy, SummarySource,
};
use crate::context::{
    PrefixCheckError, PrefixRegistry, PrefixSnapshot, SliceError, SliceSource,
    opening_messages_with,
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
use kgraf_session::skills::{SKILL_TOOL, SkillRestriction, SkillSession, WorkerSkills};
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
    /// The most progress checkpoints a worker may reach, so at most this many
    /// blocks of the brief's round budget (decision 0096). 1 is the old wall:
    /// the first checkpoint ends the extension whatever it measures.
    pub max_checkpoints: u32,
    /// The run's reasoning setting (decision 0098): each round carries the
    /// level `worker_round` resolves to, each compaction summary the level
    /// of `worker_compaction`. Empty by default: no reasoning field.
    pub reasoning: kgraf_provider::ReasoningLevels,
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
            max_checkpoints: crate::progress::DEFAULT_MAX_CHECKPOINTS,
            reasoning: kgraf_provider::ReasoningLevels::new(),
        }
    }
}

/// What the progress gate needs to know of a call (decision 0096): the paths
/// its tool declares it writes, and whether the masker hides its result.
struct EnvFacts<'a>(&'a WorkerEnv);

impl crate::progress::CallFacts for EnvFacts<'_> {
    fn written_paths(&self, tool: &str, input: &serde_json::Value) -> Vec<PathBuf> {
        self.0
            .tools
            .get(tool)
            .and_then(|registered| registered.tool().access(input).ok())
            .map(|access| access.writes_paths().iter().cloned().collect())
            .unwrap_or_default()
    }

    fn hides(&self, output: &str, is_error: bool) -> bool {
        self.0.masker.hides_result(output, is_error)
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
    /// The environment's skills (decision 0053): the catalog the prefix lists
    /// and the `skill` tool's state. Each worker spawned here gets its own
    /// `allowed-tools` policy on its approval chain. `None`: no skills, and
    /// the prefix must list none.
    pub skills: Option<Arc<SkillSession>>,
    /// The attempt's protected-write guard (decision 0094): in Bypass, a
    /// write to `.git` or kGraf's settings that it admits passes the scope
    /// check, and a failed integrity check fails the worker. `None`: every
    /// protected path is refused, in every mode.
    pub protected_writes: Option<Arc<kgraf_tools::ProtectedWrites>>,
    /// Where each worker spawned here mirrors its policy refusals while it
    /// runs, for the run's `report_shortfall` (decision 0093). `None`: no
    /// tool reads them.
    pub refusals: Option<Arc<crate::shortfall::RefusalDirectory>>,
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
            skills: None,
            protected_writes: None,
            refusals: None,
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
        "the brief activates skills {skills:?}; a brief cannot activate skills (a worker activates one with the `skill` tool), refused rather than ignored"
    )]
    SkillsUnsupported { skills: Vec<String> },
    /// The prefix's skill catalog is not the environment's, or the environment
    /// has skills and no `skill` tool: the model would be offered skills that
    /// cannot be activated, or activate skills whose restrictions no policy
    /// enforces.
    #[error("the environment's skills do not match its prefix: {0}")]
    SkillsMismatch(String),
    /// A skill an earlier segment activated cannot be activated again on
    /// resume, as the transcript recorded it: it is gone, the model may no
    /// longer invoke it, or it changed. Refused rather than resumed without
    /// its restriction, or with a different one.
    #[error(
        "cannot re-activate skill `{name}` that the transcript activated: {reason}. Restore the \
         skill as it was when the worker activated it, or start the work again as a new run"
    )]
    SkillReplay { name: String, reason: String },
    /// The restrictions a subagent inherits from its parent cannot be read.
    /// Refused rather than started with none.
    #[error("cannot apply the skill restrictions inherited from the parent: {0}")]
    InheritedSkills(String),
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
    /// The prefix and the opening messages alone reach the context stop point.
    /// The worker would hand back before its first round with nothing done, and
    /// the same brief split again would do so forever.
    #[error(
        "the worker's pinned context of {floor} tokens (prefix, slices, instruction) reaches its \
         stop point of {stop}; it cannot run a round. Raise the context ceiling or shrink the \
         slices and the instruction"
    )]
    PinnedContextTooLarge { floor: u64, stop: u64 },
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
    /// The `allowed-tools` restrictions of the agent that spawned this worker
    /// (decision 0053): a subagent is restricted as its parent was when it
    /// spawned it, before any skill of its own. Applied whether or not this
    /// environment has skills. Empty for a worker nothing spawned.
    pub inherited_skills: Vec<SkillRestriction>,
    /// What the worker does at its stop point (decision 0095): hand back (the
    /// default) or, for a caller that cannot split the remainder, compact, and
    /// compact again at a later stop point on measured progress.
    pub stop_point: StopPointPolicy,
    /// The worker's transcript file, named in a compaction notice so the
    /// orchestrator and the user can find the full pre-compaction history.
    pub transcript_path: Option<PathBuf>,
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
    check_skills(env)?;
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
    // A node whose file cannot be read for this worker is left out and named
    // (in the opening, the `Context` event and the result), never the end of
    // the spawn: only the graph itself failing is an error here.
    let rendering = tokio::task::spawn_blocking(move || source.render(&nodes))
        .await
        .map_err(|e| WorkerError::SliceTask(e.to_string()))??;
    let opening = opening_messages_with(&brief.instruction, &rendering);
    let slices_left_out = rendering.left_out;
    // On a resume, the tool calls of the earlier segment's last round are what
    // the next request carries results of: its tool round (see Trace).
    let resumed_tools = options
        .resume_from
        .as_deref()
        .map(last_round_tools)
        .unwrap_or_default();
    let fresh = options.resume_from.is_none();
    // The `.git` entry every status read checks against. A fresh worker reads
    // it now, before any call runs: the host has just made or reopened the
    // worktree. A resume takes the one its first segment recorded, since a
    // `.git` on disk now may have been planted by an earlier command.
    let git_at_start = match options.resume_from.as_deref() {
        None => crate::worktree::git_entry(&brief.workspace_root),
        Some(events) => resumed_git_entry(&brief.workspace_root, events),
    };
    // Decision 0096: the checkpoints an earlier segment reached, and what its
    // open block did, so the rounds they granted and the next measurement
    // carry over.
    let mut progress =
        crate::progress::ProgressGate::new(brief.step_budget, env.settings.max_checkpoints);
    if let Some(events) = options.resume_from.as_deref() {
        progress.resume(events, &EnvFacts(env));
    }
    let (builder, rounds_used, delivered, prior) = match options.resume_from {
        Some(events) => {
            let state = ResumeState::from_events_with_base(
                opening,
                env.prefix.estimated_tokens(),
                &events,
            )?;
            let used = state.rounds_used();
            let delivered = state.injected_ids().clone();
            (state.builder, used, delivered, events)
        }
        None => (
            TranscriptBuilder::new(opening).with_base_tokens(env.prefix.estimated_tokens()),
            0,
            BTreeSet::new(),
            Vec::new(),
        ),
    };
    // Decision 0095: a resumed worker keeps its history so far for a later
    // compaction; one that compacted also keeps its archive (the events before
    // its last notice) and the progress measured since that notice, against
    // the baseline the notice's event stored.
    // Decision 0057: the estimate counts what the next request carries, so it
    // learns whether this route resends reasoning.
    let builder = builder.with_reasoning_resent(env.provider.resends_reasoning());
    let compactions = builder.compactions().to_vec();
    let last_compacted = prior
        .iter()
        .rposition(|e| matches!(e, WorkerEvent::Compacted { .. }));
    let archive = last_compacted.map(|at| HistoryArchive::from_events(&prior[..at]));
    let segment = last_compacted.and_then(|at| {
        let baseline = builder.compaction_baseline()?.clone();
        let rounds = prior[..at]
            .iter()
            .filter(|e| matches!(e, WorkerEvent::ModelCall { .. }))
            .count() as u32;
        let mut segment = crate::progress::SegmentGate::start(baseline, rounds);
        for event in &prior[at + 1..] {
            segment.observe(event, &EnvFacts(env));
        }
        Some(segment)
    });
    let history: Vec<WorkerEvent> = prior
        .into_iter()
        .filter(crate::compaction::archived_kind)
        .collect();
    // A fresh worker that starts at the stop point can only hand back at once with the
    // same remainder: refuse it, rather than let the caller split it forever.
    if fresh && brief.context.reached_point(builder.context_tokens()) {
        return Err(WorkerError::PinnedContextTooLarge {
            floor: builder.context_tokens(),
            stop: brief.context.worker_stop_tokens(),
        });
    }
    // Decision 0053: this worker's own `allowed-tools` policy, on its own
    // chain, starting from what it inherits from the agent that spawned it. A
    // resumed worker re-activates, in order, every skill its earlier segments
    // activated, at the version each activation recorded, so a restart neither
    // lifts a restriction nor swaps it for an edited one.
    let mut approval = options.approval.unwrap_or_else(|| env.approval.clone());
    let inherited = &options.inherited_skills;
    let worker_skills = match &env.skills {
        Some(skills) => {
            let policy = skills
                .attach_inheriting(brief.worker.as_str(), inherited)
                .map_err(|e| WorkerError::InheritedSkills(e.to_string()))?;
            replay_skills(Some(skills), brief.worker.as_str(), &builder)?;
            Some(policy)
        }
        // No skill tool here, so only the inherited restrictions apply, and a
        // transcript that activated a skill cannot be resumed.
        None => {
            replay_skills(None, brief.worker.as_str(), &builder)?;
            if inherited.is_empty() {
                None
            } else {
                Some(
                    WorkerSkills::inheriting(brief.worker.as_str(), inherited)
                        .map_err(|e| WorkerError::InheritedSkills(e.to_string()))?,
                )
            }
        }
    };
    if let Some(policy) = &worker_skills {
        approval = approval.with_policy(Arc::new(policy.clone()));
    }
    // Checked after the skills: a changed skill also changes the prefix, and the
    // skill check names the skill and what to do.
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
    let mut stall =
        StallDetector::new(env.settings.stall_repeats).with_window(env.masker.observation_window());
    let mut saw_build = false;
    let mut trace_failures = Vec::new();
    // The live loop counts a round's approved calls once, before they run, and
    // resets after a write that succeeded. The replay does the same per round,
    // on the input each call ran with (after any approval rewrite).
    let compacted_at = builder.replay_compaction_round();
    for (index, round) in builder.replay_rounds().iter().enumerate() {
        // The live loop forgot every count at the compaction
        // (`Worker::reset_context_state`).
        if compacted_at == Some(index) {
            stall.reset();
        }
        // Denied and refused calls did nothing and are not counted. A replay
        // that reaches the threshold would have ended the earlier run, so the
        // result is not acted on.
        let _ = stall.observe(
            round
                .iter()
                .filter(|call| call.counted)
                .map(|call| (call.tool.as_str(), &call.input)),
        );
        let mut wrote = false;
        for call in round
            .iter()
            .filter(|call| call.succeeded || call.interrupted_by_steer())
        {
            let Some(registered) = env.tools.get(&call.tool) else {
                continue;
            };
            // The stored input is pattern-scrubbed: a marker where the call had
            // text. A write path with a marker in it is not the path the call
            // wrote, so it is reported, not recorded.
            let access = match registered.tool().access(&call.input) {
                Ok(access) => access,
                Err(e) => {
                    trace_failures.push(format!(
                        "call {}: the writes of `{}` cannot be replayed from its stored input: {e}",
                        call.id, call.tool
                    ));
                    continue;
                }
            };
            saw_build |= access.is_sandboxed_build();
            for path in access.writes_paths() {
                if crate::rebuild::has_redaction_marker(&path.to_string_lossy()) {
                    trace_failures.push(format!(
                        "call {}: `{}` wrote a path that its stored input redacted",
                        call.id, call.tool
                    ));
                    continue;
                }
                changed.insert(path.clone());
                // The live loop's own wording for each case.
                let verb = if call.succeeded {
                    "wrote"
                } else {
                    "may have written"
                };
                done.push(format!("{} {verb} {}", call.tool, path.to_string_lossy()));
                wrote = true;
            }
        }
        // Tell the detector which results the masker hid, as the live loop did.
        for call in round.iter().filter(|call| call.counted) {
            stall.note_result(
                &crate::stall::call_digest(&call.tool, &call.input),
                env.masker.hides_result(&call.output, call.is_error),
            );
        }
        if wrote {
            stall.reset();
        }
    }
    // A sandboxed build declares no paths, so its writes are not in the replay
    // above. What the worktree shows dirty now includes them.
    if saw_build {
        let root = brief.workspace_root.clone();
        let scope = brief.write_scope.clone();
        // Checked against the gitlink the first segment recorded, not the
        // `.git` on disk now (`resumed_git_entry`).
        let at_start = git_at_start.clone();
        match tokio::task::spawn_blocking(move || {
            crate::worktree::dirty_paths(&root, &at_start?).map(|status| {
                without_generated(&root, status, scope.as_deref(), "an earlier build")
            })
        })
        .await
        {
            Ok(Ok((status, note))) => {
                note_nested_repositories(&mut trace_failures, &status);
                done.extend(note);
                for path in status.paths {
                    if changed.insert(path.clone()) {
                        done.push(format!(
                            "an earlier build changed {}",
                            path.to_string_lossy()
                        ));
                    }
                }
            }
            Ok(Err(e)) => trace_failures.push(format!("worktree status on resume not read: {e}")),
            Err(e) => trace_failures.push(format!("worktree status on resume not read: {e}")),
        }
    }
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
    if !compactions.is_empty() {
        steering.mark_compacted();
    }
    handle.set_spawn_duplicates(already_delivered, listed_twice);
    handle.set_skills(worker_skills);
    let refused =
        crate::shortfall::RefusedCalls::mirrored(env.refusals.clone(), brief.worker.as_str());
    let worker = Worker {
        git_at_start,
        env: env.clone(),
        approval,
        budget: StepBudget::new(
            brief.step_budget.saturating_add(progress.rounds_granted()),
            rounds_used,
        ),
        progress,
        brief,
        seq: EventSequence::new(),
        builder,
        peak_context: 0,
        stall,
        notified_round: None,
        output_limit_retried: false,
        empty_turn_retried: false,
        malformed_retries: 0,
        lost_output_retries: 0,
        no_change_retried: false,
        phase: CancelPhase::BetweenRounds,
        calls,
        last_stop: None,
        tool_calls: 0,
        last_call,
        given_tools: resumed_tools,
        trace_failures,
        route_fault: None,
        cancel_unsent: false,
        round_redactions: kgraf_secrets::Redactions::default(),
        changed,
        done,
        denied: 0,
        refused,
        capabilities: std::collections::HashMap::new(),
        patch_rejections: 0,
        fence: options.fence,
        steering,
        stop_point: options.stop_point,
        transcript_path: options.transcript_path,
        history,
        archive,
        compactions,
        segment,
        slices_left_out,
    };
    Ok((worker, endpoint, handle))
}

/// Re-activate, in order, every skill the transcript's earlier segments
/// activated, each at the version its activation result recorded. With no
/// skills in this environment, any activation refuses the resume.
fn replay_skills(
    skills: Option<&SkillSession>,
    worker: &str,
    builder: &TranscriptBuilder,
) -> Result<(), WorkerError> {
    // The `skill` tool's access is a barrier, so no two activations overlap and
    // the start order is the activation order.
    let activations = builder
        .replay_rounds()
        .iter()
        .flatten()
        .filter(|call| call.tool == SKILL_TOOL && call.succeeded);
    for call in activations {
        let name = call
            .input
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let Some(skills) = skills else {
            return Err(WorkerError::SkillReplay {
                name: name.to_owned(),
                reason: "this run has no skills the model may invoke".into(),
            });
        };
        let recorded = kgraf_session::skills::recorded_hash(&call.output);
        skills
            .reactivate(worker, name, recorded)
            .map_err(|e| WorkerError::SkillReplay {
                name: name.to_owned(),
                reason: e.to_string(),
            })?;
    }
    Ok(())
}

/// The environment's skills agree with its prefix and registry: the prefix
/// lists exactly the skills the model may activate, and an environment with
/// skills registers the `skill` tool.
fn check_skills(env: &WorkerEnv) -> Result<(), WorkerError> {
    let listed = &env.prefix.prefix().skill_catalog;
    match &env.skills {
        None if listed.is_empty() => Ok(()),
        None => Err(WorkerError::SkillsMismatch(
            "the prefix lists skills and the environment has none to activate".into(),
        )),
        Some(skills) => {
            if *listed != skills.model_catalog() {
                return Err(WorkerError::SkillsMismatch(
                    "the prefix's skill catalog is not the environment's".into(),
                ));
            }
            if env.tools.get(SKILL_TOOL).is_none() {
                return Err(WorkerError::SkillsMismatch(format!(
                    "the environment has skills and no `{SKILL_TOOL}` tool"
                )));
            }
            Ok(())
        }
    }
}

/// Why a run stops early.
enum Halt {
    Cancelled {
        phase: CancelPhase,
        partial: Option<(CallId, TokenUsage)>,
    },
    Failed(String),
    /// The server could not parse the model's own output
    /// ([`kgraf_provider::ProviderError::ModelOutputMalformed`]). `drive` sends
    /// the round again, a bounded number of times, before it fails the attempt.
    ModelOutputMalformed(String),
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

/// The start of the result of a call the approval chain denied. A stored
/// transcript is told from a failed run by it: a denied call did nothing.
pub(crate) const DENIED_PREFIX: &str = "ERROR: denied by ";

/// The start of the result of a call refused before it ran (outside the write
/// scope or fenced).
pub(crate) const REFUSED_PREFIX: &str = "ERROR: refused: ";

/// The start of the result of a call the approval chain turned away because
/// the tool could not read its input ([`kgraf_tools::INVALID_INPUT`]). Not
/// "denied": no policy refused it, and a worker told "denied" reads a typo as
/// a permission it lacks. Like a denial, the call did nothing.
pub(crate) const INVALID_CALL_PREFIX: &str = "ERROR: invalid call: ";

/// What follows an invalid call's reason.
pub(crate) const INVALID_CALL_ADVICE: &str =
    "; fix the call and retry. This is not a permission refusal.";

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

/// The `RoundAbandoned` reason for a round whose request never left the
/// process. The round bought no model turn, so a rebuild does not count it
/// (the loop refunds it from the step budget the same way).
pub const STEER_ABANDON_BEFORE_SEND_REASON: &str =
    "an interrupt-mode message arrived before the request was sent";

/// The `RoundAbandoned` reason recorded just before `Cancelled` when a cancel
/// ended the run before the round's request left the process. The loop spends no
/// round on a resume for it, as it refunds one abandoned by a steer.
pub const CANCEL_BEFORE_SEND_REASON: &str = "the worker was cancelled before the request was sent";

/// Why the final round's tool calls do not run.
const FINAL_ROUND_WHY: &str = "the step budget is spent";

/// A repository path as text, with `/` on every platform: a refusal record
/// names the path the way the repository does, not with a Windows `\`.
fn repo_path_text(path: &std::path::Path) -> String {
    if std::path::MAIN_SEPARATOR == '/' {
        return path.to_string_lossy().into_owned();
    }
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// The result a call gets when the final round issues it. The call never reached
/// the stall check, so a replay must not count it.
pub(crate) fn final_round_refusal() -> String {
    format!("ERROR: not run: {FINAL_ROUND_WHY}")
}

/// The `RoundAbandoned` reason for a round that stopped at the output ceiling
/// with no tool call, given one continuation round.
pub const OUTPUT_LIMIT_ABANDON_REASON: &str = "the output ceiling was reached";

/// Say, once per directory, that a directory holding its own repository is
/// listed as one changed path: git does not list the files inside it, so the
/// changed-path list is not complete there.
fn note_nested_repositories(
    trace_failures: &mut Vec<String>,
    status: &crate::worktree::TreeStatus,
) {
    for dir in &status.nested_repositories {
        let note = format!(
            "{} holds its own git repository; files changed inside it are not listed",
            dir.to_string_lossy()
        );
        if !trace_failures.contains(&note) {
            trace_failures.push(note);
        }
    }
}

/// `status` without the files a build or install generated outside `scope`
/// ([`crate::worktree::TreeStatus::without_generated`]), and the line that
/// says how many were left out, when any were. With no scope, every
/// generated file is left out. Blocking: reads the tree.
fn without_generated(
    root: &std::path::Path,
    status: crate::worktree::TreeStatus,
    scope: Option<&[PathBuf]>,
    tool: &str,
) -> (crate::worktree::TreeStatus, Option<String>) {
    let in_scope = |p: &std::path::Path| {
        scope.is_some_and(|s| s.iter().any(|w| crate::paths::same_path(w, p)))
    };
    let (status, left_out) = status.without_generated(root, in_scope);
    let note = (left_out > 0).then(|| {
        format!(
            "{tool} generated {left_out} file(s) (an install, build output, a lockfile or a \
             cache) that the commit leaves out; they are not listed"
        )
    });
    (status, note)
}

/// The notice that follows an output-ceiling stop: one continuation round.
pub const OUTPUT_LIMIT_NOTICE: &str = "Your last response reached the output limit and was \
     discarded. If you were writing a large patch, split it into several smaller apply_patch \
     calls, one file or a few hunks at a time, and send them in separate calls. This is your \
     only continuation: if the limit is reached again the run fails.";

/// The `RoundAbandoned` reason for a turn with no tool call and no visible
/// answer, given one continuation round.
pub const EMPTY_TURN_ABANDON_REASON: &str = "the turn had no tool call and no answer";

/// The notice that follows an empty turn: one continuation round.
pub const EMPTY_TURN_NOTICE: &str = "You ended your turn without calling a tool or giving an \
     answer; continue the task with a tool call, or answer.";

/// How many times a round is sent again after the server failed to parse the
/// model's own output. Beyond it the attempt fails, naming the cause. The
/// provider crate's bound, shared with the orchestrator's JSON-asked calls.
pub const MAX_MALFORMED_OUTPUT_RETRIES: u32 = kgraf_provider::MAX_MALFORMED_OUTPUT_RETRIES;

/// The `RoundAbandoned` reason for a round the server failed on because it
/// could not parse the model's own tool call.
pub const MALFORMED_OUTPUT_ABANDON_REASON: &str =
    "the server could not parse the model's tool call";

/// The `RoundAbandoned` reason for a malformed round that is sent again:
/// [`MALFORMED_OUTPUT_ABANDON_REASON`], then the provider's error (its HTTP
/// status and the server's body), cut to `kgraf_provider::ERROR_KEEP_BYTES`
/// with the cut marked.
///
/// The error is kept because the round's ledger row settles unreconciled
/// with no usage and no request id, and the transcript is the only record
/// of why: without the status, a reader cannot tell a server fault from a
/// call that was dropped mid-flight. The reason is never sent to the model
/// (the rebuild only compares it), and it is scrubbed on both paths like
/// every `RoundAbandoned` reason.
pub fn malformed_output_abandon_reason(error: &str) -> String {
    format!(
        "{MALFORMED_OUTPUT_ABANDON_REASON}: {}",
        kgraf_provider::bounded_error_text(error)
    )
}

/// The notice that follows such a failure. It is appended at the end of the
/// transcript, after every earlier message, so the request prefix the
/// provider caches is unchanged and the retry still hits it.
pub fn malformed_output_notice(retry: u32) -> String {
    let again = if retry > 1 { " again" } else { "" };
    format!(
        "Your previous reply contained a malformed tool call, which the server could not \
         parse{again}. Send one well-formed tool call."
    )
}

/// The `RoundAbandoned` reason for an end as done with no tool call and no
/// change, given one continuation round.
const NO_CHANGE_ABANDON_REASON: &str =
    "the turn ended with no tool call although the contract writes files";

/// The smallest floor of undelivered output tokens
/// ([`kgraf_provider::Provider::undelivered_output_tokens`]) that makes a reply
/// with no tool call a lost one. A dropped tool call is a whole call (a patch
/// is hundreds of tokens); a few stray markers are not.
pub const LOST_OUTPUT_MIN_TOKENS: u64 = 256;

/// How many replies of one attempt may be sent again because the server
/// dropped part of them. Beyond it the reply is judged as it arrived.
pub const MAX_LOST_OUTPUT_RETRIES: u32 = 2;

/// The `RoundAbandoned` reason for a reply the server delivered only in part.
pub const LOST_OUTPUT_ABANDON_REASON: &str =
    "the server counted more output than the reply carried, and the reply had no tool call";

/// The notice that follows a reply the server delivered only in part. The
/// model wrote more than arrived; telling it that it called no tool would be
/// false, and it would plan again from the wrong premise.
pub fn lost_output_notice(counted: u64, missing: u64) -> String {
    format!(
        "Your last reply did not arrive complete: the server counted {counted} output tokens \
         for it, and at least {missing} of them are not in the reply. The missing part is \
         usually a tool call that the server could not read, so it was not run. If you called \
         a tool, send that call again as one well-formed tool call; for a large change, use \
         several smaller apply_patch calls."
    )
}

/// The notice that follows an end with no change: one continuation round.
fn no_change_notice(paths: String) -> String {
    format!(
        "You ended without making any change; this contract writes {paths}. Continue with tool \
         calls, or call report_shortfall if the work cannot be done."
    )
}

struct Worker {
    /// `<root>/.git` as it was when this worker first started
    /// ([`crate::worktree::GitEntry`]; recorded in the transcript's
    /// [`WorkerEvent::Context`] for a resume), or why there is none; every
    /// status read checks against it.
    git_at_start: Result<crate::worktree::GitEntry, String>,
    env: Arc<WorkerEnv>,
    approval: ApprovalChain,
    brief: WorkerBrief,
    seq: EventSequence,
    builder: TranscriptBuilder,
    /// The largest context measure any stop check of this segment read: the
    /// value `reached_point` was given, so the trace reports what decided.
    peak_context: u64,
    budget: StepBudget,
    stall: StallDetector,
    /// The round number a step notice was last emitted for. A round refunded
    /// after a pre-send steer is planned again under the same number, and its
    /// notice is already in the transcript.
    notified_round: Option<u32>,
    /// Whether the round just before was an output-ceiling stop that got its
    /// one continuation. Cleared by any round that ends another way.
    output_limit_retried: bool,
    /// Whether the round just before was an empty turn that got its one
    /// continuation. Cleared by any round that has a tool call or an answer.
    empty_turn_retried: bool,
    /// Consecutive rounds the server failed on the model's own malformed
    /// output and that were sent again. Cleared by any round that gets a
    /// response; capped at [`MAX_MALFORMED_OUTPUT_RETRIES`].
    malformed_retries: u32,
    /// Replies of this attempt sent again because the server delivered only
    /// part of them; capped at [`MAX_LOST_OUTPUT_RETRIES`]. Never cleared: it
    /// bounds the extra rounds a server fault can cost one attempt.
    lost_output_retries: u32,
    /// Whether the attempt already got its one notice for ending as done with
    /// no tool call although its contract writes files.
    no_change_retried: bool,
    phase: CancelPhase,
    calls: Vec<CallId>,
    /// The stop reason of the last round that ended naturally, this segment.
    last_stop: Option<StopReason>,
    /// Tool calls started this segment (counted from the events the loop
    /// sends, as `calls` counts this segment's model calls).
    tool_calls: u32,
    /// The parent of the next round's ledger row (see the module docs, Trace).
    last_call: Option<CallId>,
    /// The previous round's tool calls, in issue order: the tool round the next
    /// request is given, written to its ledger row.
    given_tools: Vec<String>,
    /// Trace writes the ledger refused, reported on the result.
    trace_failures: Vec<String>,
    /// The brief's slices this worker was not given, with the reason each
    /// was left out; recorded in the `Context` event and on the result.
    slices_left_out: Vec<crate::context::SliceLeftOut>,
    /// The provider error that ended the run when it says the route is
    /// unreachable (`ProviderError::is_route_fault`), reported on the result.
    route_fault: Option<String>,
    /// Persist-scrub redactions in this round's streamed deltas, not yet
    /// reported (decision 0071). Dropped when the round's
    /// `AssistantMessage` reports its whole text instead; reported before a
    /// `Cancelled` or `RoundAbandoned` that ends the round unsettled.
    round_redactions: kgraf_secrets::Redactions,
    /// Set when a cancel ends the run before the round's request left the
    /// process: `run` records `RoundAbandoned` first, so a resume does not count
    /// the round.
    cancel_unsent: bool,
    changed: BTreeSet<PathBuf>,
    done: Vec<String>,
    denied: u32,
    /// Calls a policy refused, for the result (decision 0093).
    refused: crate::shortfall::RefusedCalls,
    /// Each call of the current round by id: what it exercises, to name what
    /// a refusal refused and to match a refusal to a retry.
    capabilities: std::collections::HashMap<String, crate::shortfall::CallCapability>,
    /// `apply_patch` calls this worker made that the tool rejected: once there
    /// are two, a rejection carries a worked example for the file it targeted.
    patch_rejections: u32,
    /// This worker's write fence, if it has one (decision 0067, point 5).
    fence: Option<ScopeFence>,
    /// This worker's steering queue (decision 0067, point 3).
    steering: Arc<SteerQueue>,
    /// What the worker does at its stop point (decision 0095).
    stop_point: StopPointPolicy,
    /// The transcript file, for the compaction notice.
    transcript_path: Option<PathBuf>,
    /// The persisted events a compaction archives: every archived kind of
    /// the attempt so far, so a later compaction's archive holds all of it.
    history: Vec<WorkerEvent>,
    /// The pre-compaction history `recall_history` reads, once compacted:
    /// everything before the last compaction.
    archive: Option<HistoryArchive>,
    /// The compaction notices, in order. More than one only for a worker that
    /// cannot hand back and made measured progress between them.
    compactions: Vec<CompactionNotice>,
    /// The progress since the last compaction, for a worker that may compact
    /// again (decision 0095, amendment of 2026-10-06).
    segment: Option<crate::progress::SegmentGate>,
    /// The progress gate that turns the round budget into checkpoints
    /// (decision 0096).
    progress: crate::progress::ProgressGate,
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
    /// Why the call is refused before it runs, and the path that refused it
    /// when one path did.
    fn scope_violation(&self, access: &kgraf_tools::Access) -> Option<(String, Option<String>)> {
        /// The rest of the refusal of a write by a contract that declares no
        /// writes, after the path.
        const WRITELESS_SCOPE: &str = " is outside the declared write scope: this contract \
             declares no writes, so the plan made it a check-only contract (it may run checks \
             and read files, and changes no file). A file the work needs belongs to the \
             contract of the plan that lists it in its writes; if no contract lists it, the \
             plan is missing that write";
        if access.is_exclusive()
            && !access.is_sandboxed_build()
            && let Some(scope) = &self.brief.write_scope
        {
            let mut why = String::from(
                "the call may write anywhere (it declares no paths) and this worker has a declared \
                 write scope; with a write scope, the shell runs only the build and test \
                 allowlist named in its description, and files change only through apply_patch",
            );
            if scope.is_empty() {
                why.push_str(
                    "; this contract declares no writes, so the plan made it a check-only \
                     contract that changes no file",
                );
            }
            return Some((why, None));
        }
        for path in access.writes_paths() {
            if let Some(why) = kgraf_core::paths::forbidden_reason(path) {
                // Decision 0094: in Bypass the guard admits `.git` and
                // kGraf's settings. They lie outside the worktree's files, so
                // the declared write scope (repository files) does not apply.
                // It also admits `.gitattributes`, `.gitmodules` and
                // `.githooks`, which are repository files: the scope check
                // below applies to them like any file.
                let admitted = match &self.env.protected_writes {
                    Some(guard) => guard.admit(path).map(|r| r.map(|t| t.area)),
                    None => Some(Err(why)),
                };
                match admitted {
                    Some(Ok(kgraf_tools::protected::Area::Tracked)) => {}
                    Some(Ok(_)) => continue,
                    Some(Err(why)) => {
                        return Some((
                            format!("{} is not writable by a worker: {why}", path.display()),
                            Some(repo_path_text(path)),
                        ));
                    }
                    None => {}
                }
            }
            if let Some(scope) = &self.brief.write_scope {
                let inside = scope.iter().any(|s| crate::paths::same_path(s, path));
                if !inside {
                    // An empty scope is the plan's choice, not a missing
                    // path: plan intake accepts a contract with no writes
                    // next to contracts that write (one that only installs
                    // and verifies). Said so, so the worker does not ask for
                    // a scope the plan never meant to give, and a contract
                    // whose instruction needs a file names the plan shape.
                    let why = if scope.is_empty() {
                        format!("{}{WRITELESS_SCOPE}", path.display())
                    } else {
                        format!("{} is outside the declared write scope", path.display())
                    };
                    return Some((why, Some(repo_path_text(path))));
                }
            }
        }
        self.fence
            .as_ref()
            .and_then(|f| fence_violation(f, access))
            .map(|why| (why, None))
    }

    async fn run(mut self, endpoint: WorkerEndpoint) {
        let (outcome, answer) = match self.drive(&endpoint).await {
            Ok(end) => end,
            Err(Halt::Cancelled { phase, partial }) => {
                // Control events: never refused by the cancel, never stuck behind a
                // full channel (see handle.rs). A reader that has gone away cannot
                // see them, and the result below is then dropped with the handle.
                if self.cancel_unsent {
                    let _ = self
                        .emit(
                            &endpoint,
                            WorkerEvent::RoundAbandoned {
                                phase,
                                reason: CANCEL_BEFORE_SEND_REASON.to_owned(),
                            },
                        )
                        .await;
                }
                let _ = self.emit(&endpoint, WorkerEvent::Cancelled { phase }).await;
                if let Some((call, usage)) = partial {
                    let _ = self
                        .emit(&endpoint, WorkerEvent::Usage { call, usage })
                        .await;
                }
                (WorkerOutcome::Cancelled, String::new())
            }
            Err(Halt::Failed(error) | Halt::ModelOutputMalformed(error)) => {
                (WorkerOutcome::Failed { error }, String::new())
            }
            // `drive` handles every abandon itself; one reaching here is a
            // loop defect, reported rather than read as success (fail loud).
            Err(Halt::Abandoned { .. }) => (
                WorkerOutcome::Failed {
                    error: "an abandoned round escaped the loop".into(),
                },
                String::new(),
            ),
        };
        // Decision 0094: the attempt's protected writes get their full check
        // (fsck over the shared repository) before the result leaves, so a
        // failed check fails the worker and nothing of it is merged.
        let (outcome, answer) = match self.env.protected_writes.clone() {
            None => (outcome, answer),
            Some(guard) => match tokio::task::spawn_blocking(move || guard.finish()).await {
                Ok(Ok(())) => (outcome, answer),
                Ok(Err(check)) => {
                    let error = match outcome {
                        WorkerOutcome::Failed { error } => format!("{error}\n{check}"),
                        _ => check,
                    };
                    (WorkerOutcome::Failed { error }, String::new())
                }
                Err(e) => (
                    WorkerOutcome::Failed {
                        error: format!("the protected-write check did not finish: {e}"),
                    },
                    String::new(),
                ),
            },
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
            changed_paths: self.worktree_changes(),
            commit: None,
            calls: std::mem::take(&mut self.calls),
            last_stop: self.last_stop,
            tool_calls: self.tool_calls,
            steps: self.budget.used(),
            prefix_digest: self.env.prefix.digest().as_str().to_owned(),
            denied: self.denied,
            trace_failures: std::mem::take(&mut self.trace_failures),
            slices_left_out: self.slices_left_out.clone(),
            route_fault: self.route_fault.take(),
            rationale,
            refused: self.refused.take(),
            compaction: self.compactions.first().cloned(),
            later_compactions: self.compactions.iter().skip(1).cloned().collect(),
            round_blocks: self.progress.report(),
            peak_context_tokens: self.peak_context.max(self.builder.context_tokens()),
        };
        // `Err` means the handle is gone: nobody is waiting for the result, and the
        // ledger already holds every call it names.
        let _ = endpoint.finish(result);
    }

    /// The halt for a model call that failed with `e`. A failure that says
    /// the route is unreachable is also kept for the result
    /// (`WorkerResult::route_fault`), so the orchestrator can take the route
    /// out of rotation instead of reading it as the model's failure.
    fn provider_failed(&mut self, e: &kgraf_provider::ProviderError) -> Halt {
        let text = e.to_string();
        if e.is_route_fault() {
            self.route_fault = Some(text.clone());
        }
        if e.is_malformed_model_output() {
            return Halt::ModelOutputMalformed(text);
        }
        Halt::Failed(text)
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
        match &event {
            WorkerEvent::Stop { reason, .. } => self.last_stop = Some(*reason),
            WorkerEvent::ToolCallStarted { .. } => self.tool_calls += 1,
            _ => {}
        }
        self.archive_event(&event);
        // Observed in its persisted form, as a resume reads it back.
        self.progress.observe(&event, &EnvFacts(&self.env));
        if let Some(segment) = self.segment.as_mut() {
            segment.observe(&event, &EnvFacts(&self.env));
        }
        if let Some(model_event) = model_event {
            self.builder.apply(model_event);
        }
        // The builder has taken a finished tool result above, so the persisted
        // transcript must carry it too. A bulk emit refused because of a cancel
        // never sent the event; without this copy the work a call finished
        // would be missing on resume (the resume would mark the call
        // interrupted, though its writes are on disk).
        let finished_result =
            matches!(event, WorkerEvent::ToolCallFinished { .. }).then(|| event.clone());
        // A requested cancel outranks everything: the run was cancelled. A reader
        // that has gone away without a cancel is `WorkerHandle::result` discarding
        // the events it did not read, which its contract allows: the run goes on,
        // and its result still arrives. The loop keeps its own transcript
        // (`builder`), so nothing it needs is lost.
        match ep.emit(event).await {
            Ok(()) => Ok(()),
            Err(_) if ep.is_cancelled() => {
                if let Some(result) = finished_result {
                    let _ = ep.emit_unbounded(result);
                }
                Err(Halt::Cancelled {
                    phase: self.phase,
                    partial: None,
                })
            }
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
                // The last text part's token: the one a provider attaches at the
                // end of a reply.
                signature: response.content.iter().rev().find_map(|block| match block {
                    Content::Text { signature, .. } => signature.clone(),
                    _ => None,
                }),
            },
        )
        .await?;
        self.emit(
            ep,
            WorkerEvent::Stop {
                reason,
                finish_reason: response.finish_reason.clone(),
            },
        )
        .await
    }

    async fn drive(&mut self, ep: &WorkerEndpoint) -> Result<(WorkerOutcome, String), Halt> {
        let context = WorkerEvent::Context {
            prefix_digest: self.env.prefix.digest().as_str().to_owned(),
            slices: self.brief.slices.clone(),
            slices_left_out: self.slices_left_out.clone(),
            gitlink: self
                .git_at_start
                .as_ref()
                .ok()
                .map(|entry| entry.gitlink().to_owned()),
            no_gitlink: self.git_at_start.is_err(),
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
            self.peak_context = self.peak_context.max(context_tokens);
            // Decision 0095: a user's request compacts at this boundary.
            if self.compactions.is_empty() && self.steering.take_compaction_request() {
                self.compact(ep, CompactionTrigger::UserRequest, context_tokens)
                    .await?;
                continue;
            }
            if self.brief.context.reached_point(context_tokens) {
                // Compaction only where the caller cannot split the
                // remainder. A later overflow compacts again only on measured
                // progress (below); otherwise it hands back with everything
                // the loop observed and the reason.
                // Nothing written yet: a hand-back would give the child this
                // same whole task, so it would only move the work down a level
                // and meet the same stop point (a long first reply plus one
                // large read does it). Compact instead, once; after that, go
                // on while the window holds.
                // Only the transcript's own size counts here: queued messages
                // that cross the point alone hand back with the messages
                // undelivered (decision 0095), whatever was written.
                let no_progress = !self.made_progress()
                    && self
                        .brief
                        .context
                        .reached_point(self.builder.context_tokens());
                if self.compactions.is_empty()
                    && (self.stop_point == StopPointPolicy::Compact || no_progress)
                {
                    self.compact(ep, CompactionTrigger::StopPoint, context_tokens)
                        .await?;
                    continue;
                }
                if !(no_progress && context_tokens < self.brief.context.effective_window()) {
                    // Decision 0095, amendment of 2026-10-06: a worker that
                    // cannot hand back compacts again while there is room
                    // under the stop point and it made measured progress
                    // since its last compaction, up to the checkpoint
                    // ceiling. Holding it would stop work that is moving;
                    // compacting a worker that is not moving would only
                    // spend summary calls, so that one hands back and says
                    // why.
                    let stop = if !self.compactions.is_empty()
                        && self.stop_point == StopPointPolicy::Compact
                    {
                        match self.may_compact_again().await {
                            Ok(()) => {
                                self.compact(ep, CompactionTrigger::StopPoint, context_tokens)
                                    .await?;
                                continue;
                            }
                            Err(stop) => Some(stop),
                        }
                    } else {
                        None
                    };
                    return Ok((self.hand_back(context_tokens, queued, stop), String::new()));
                }
            }
            // Decision 0096: the round budget is a checkpoint. Measured where
            // the budget would warn that three rounds remain, after the
            // context check, so a full context still hands back first.
            if self.progress.midpoint_due(self.budget.used()) {
                let (digests, _) = self.read_tree_state().await;
                self.progress.capture_midpoint(digests);
            }
            if self.progress.due(self.budget.used())
                && let Some(outcome) = self.checkpoint(ep).await?
            {
                return Ok((outcome, String::new()));
            }
            let final_round = match self.budget.next_round() {
                RoundPlan::Exhausted => return Ok((WorkerOutcome::BudgetExhausted, String::new())),
                RoundPlan::Run {
                    round,
                    final_round,
                    notice,
                } => {
                    if let Some((kind, text)) = notice
                        && self.notified_round != Some(round)
                    {
                        self.notified_round = Some(round);
                        self.emit(ep, WorkerEvent::Notice { kind, text }).await?;
                    }
                    final_round
                }
            };
            self.deliver_injections(ep)?;
            let messages = self.env.masker.mask(self.builder.messages());
            let mut request = self.env.prefix.request(
                messages,
                self.env.settings.max_output_tokens,
                self.env.settings.temperature,
            );
            request.reasoning_effort = self
                .env
                .settings
                .reasoning
                .effort(kgraf_provider::ReasoningProcess::WorkerRound);
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
                    let reason = if sent {
                        STEER_ABANDON_REASON
                    } else {
                        STEER_ABANDON_BEFORE_SEND_REASON
                    };
                    // A round that never left still records its zero usage (the
                    // call is settled as a known zero); the rebuild does not let
                    // that zero replace the context anchor.
                    self.abandon(ep, phase, partial, reason).await?;
                    continue;
                }
                Err(Halt::ModelOutputMalformed(text)) => {
                    // A server failure on the model's own generation, not on the
                    // request: another sample usually parses. Each try is its own
                    // ledger call (the failed row stays open as for any 5xx). The
                    // retry costs a round, so a checkpoint sees it as one, and it
                    // is never started on the last round.
                    if final_round
                        || self.malformed_retries >= MAX_MALFORMED_OUTPUT_RETRIES
                        || self.budget.rounds_left() < 2
                    {
                        let why = if self.malformed_retries >= MAX_MALFORMED_OUTPUT_RETRIES {
                            format!(
                                "; the round was sent again {} times and failed the same way \
                                 each time",
                                self.malformed_retries
                            )
                        } else {
                            "; no round is left to send it again".to_owned()
                        };
                        return Err(Halt::Failed(format!("{text}{why}")));
                    }
                    self.malformed_retries += 1;
                    self.abandon(
                        ep,
                        CancelPhase::ModelStream,
                        None,
                        &malformed_output_abandon_reason(&text),
                    )
                    .await?;
                    self.emit(
                        ep,
                        WorkerEvent::Notice {
                            kind: crate::event::NoticeKind::MalformedOutput,
                            text: malformed_output_notice(self.malformed_retries),
                        },
                    )
                    .await?;
                    continue;
                }
                other => other?,
            };
            self.malformed_retries = 0;
            let uses: Vec<(String, String, serde_json::Value)> = response
                .tool_uses()
                .into_iter()
                .map(|(id, name, input)| (id.to_owned(), name.to_owned(), input.clone()))
                .collect();
            let reason = response.stop_reason;
            if reason != StopReason::MaxTokens {
                // A round that ended another way ends the run of ceiling stops.
                self.output_limit_retried = false;
            }
            if !uses.is_empty() || !response.text().trim().is_empty() {
                self.empty_turn_retried = false;
            }
            match (reason, uses.is_empty()) {
                (StopReason::ToolUse, true) => {
                    return Err(Halt::Failed(
                        "the model stopped for a tool call and the response carries none".into(),
                    ));
                }
                (StopReason::ToolUse, false) => {}
                (StopReason::MaxTokens, _)
                    if !final_round
                        && !self.output_limit_retried
                        // The continuation is a round of its own. When it would
                        // be the final round, it would carry two contradictory
                        // notices (split your work, and no tool calls run), so
                        // the ceiling stop fails instead.
                        && self.budget.rounds_left() >= 2 =>
                {
                    // A large apply_patch is the usual cause: the call is cut
                    // off (the adapter drops it and reports `MaxTokens`). Any
                    // complete calls beside a cut-off one are dropped with the
                    // round: a turn that hit the ceiling is not run. The round's spend
                    // stays counted; its truncated text is dropped (the round is
                    // abandoned, which is not a terminal `Stop`), and the model
                    // gets one continuation round, counted in the budget, with
                    // a request to split the work. A second stop in a row fails
                    // below.
                    self.output_limit_retried = true;
                    self.abandon(
                        ep,
                        CancelPhase::ModelStream,
                        None,
                        OUTPUT_LIMIT_ABANDON_REASON,
                    )
                    .await?;
                    self.emit(
                        ep,
                        WorkerEvent::Notice {
                            kind: crate::event::NoticeKind::OutputLimit,
                            text: OUTPUT_LIMIT_NOTICE.to_owned(),
                        },
                    )
                    .await?;
                    continue;
                }
                (StopReason::MaxTokens, _) => {
                    self.emit_stop(ep, reason, &response).await?;
                    return Ok((
                        WorkerOutcome::Failed {
                            error: "the output ceiling was reached before the answer finished"
                                .into(),
                        },
                        response.text(),
                    ));
                }
                (other, false) => {
                    return Err(Halt::Failed(format!(
                        "the response carries tool calls but stopped with {other:?}"
                    )));
                }
                // The provider paused the turn for a server-side tool. The
                // rebuilt transcript keeps only text and client tool calls, so
                // the server tool's blocks would be dropped and the paused turn
                // resent damaged. No route uses server tools, so this fails
                // loud. No `Stop` is recorded: a resume then sees a round that
                // never finished, not a paused turn to continue.
                (StopReason::PauseTurn, true) => {
                    return Err(Halt::Failed(
                        "the provider paused the turn for a server tool; server tool turns are \
                         not supported"
                            .into(),
                    ));
                }
                // The reply ended with no tool call, but the provider counted
                // far more output than it carries: a server that could not
                // parse the model's tool call dropped it and answered 200 with
                // a plain stop. Judged as it arrived, the turn would read as
                // an empty turn, an end with no change, or a completed attempt
                // with its last write missing. The round is dropped and the
                // model is told what happened, at most
                // `MAX_LOST_OUTPUT_RETRIES` times an attempt; after that the
                // reply is judged as it arrived, by the arms below.
                (StopReason::EndTurn | StopReason::StopSequence, true)
                    if !final_round
                        && self.lost_output_retries < MAX_LOST_OUTPUT_RETRIES
                        && self.budget.rounds_left() >= 2
                        && self
                            .env
                            .provider
                            .undelivered_output_tokens(&response)
                            .is_some_and(|n| n >= LOST_OUTPUT_MIN_TOKENS) =>
                {
                    let missing = self
                        .env
                        .provider
                        .undelivered_output_tokens(&response)
                        .unwrap_or_default();
                    self.lost_output_retries += 1;
                    self.abandon(
                        ep,
                        CancelPhase::ModelStream,
                        None,
                        LOST_OUTPUT_ABANDON_REASON,
                    )
                    .await?;
                    self.emit(
                        ep,
                        WorkerEvent::Notice {
                            kind: crate::event::NoticeKind::LostOutput,
                            text: lost_output_notice(response.usage.text_output, missing),
                        },
                    )
                    .await?;
                    continue;
                }
                // No tool call and no visible text: the model's plan sits in its
                // reasoning, or there is nothing at all. That is not an answer,
                // so it is not a completed attempt. The turn is dropped and the
                // model gets one counted continuation round (a notice, as the
                // output ceiling does); a second empty turn in a row fails,
                // naming the stop reason the provider gave. The round is
                // already counted, so a checkpoint (decision 0096) sees these
                // rounds as rounds without progress.
                (StopReason::EndTurn | StopReason::StopSequence, true)
                    if response.text().trim().is_empty() =>
                {
                    if !final_round && !self.empty_turn_retried && self.budget.rounds_left() >= 2 {
                        self.empty_turn_retried = true;
                        self.abandon(
                            ep,
                            CancelPhase::ModelStream,
                            None,
                            EMPTY_TURN_ABANDON_REASON,
                        )
                        .await?;
                        self.emit(
                            ep,
                            WorkerEvent::Notice {
                                kind: crate::event::NoticeKind::EmptyTurn,
                                text: EMPTY_TURN_NOTICE.to_owned(),
                            },
                        )
                        .await?;
                        continue;
                    }
                    self.emit_stop(ep, reason, &response).await?;
                    let why = if self.empty_turn_retried {
                        "twice in a row, after being told to continue"
                    } else {
                        "on its last round"
                    };
                    return Ok((
                        WorkerOutcome::Failed {
                            error: format!(
                                "the model ended its turn with no tool call and no answer {why} \
                                 (stop reason {reason:?}, reasoning only: {})",
                                !response.reasoning().trim().is_empty()
                            ),
                        },
                        String::new(),
                    ));
                }
                // The model announced work and ended the turn: no successful
                // write and no mutating command in the whole attempt (reads
                // may have happened), and its contract writes files. That is not
                // a completion. The text stays in the transcript; the model
                // gets one notice (counted in the budget) and a second such
                // end fails the attempt with a named reason. The round is
                // already counted, so a checkpoint (decision 0096) sees it as a
                // round without progress.
                (StopReason::EndTurn | StopReason::StopSequence, true)
                    if self.ended_without_change() =>
                {
                    if !final_round && !self.no_change_retried && self.budget.rounds_left() >= 2 {
                        self.no_change_retried = true;
                        // The turn is dropped, as an empty turn is: a `Stop`
                        // would end the run's event sequence.
                        self.abandon(ep, CancelPhase::ModelStream, None, NO_CHANGE_ABANDON_REASON)
                            .await?;
                        let text = no_change_notice(self.write_scope_paths());
                        self.emit(
                            ep,
                            WorkerEvent::Notice {
                                kind: crate::event::NoticeKind::NoChange,
                                text,
                            },
                        )
                        .await?;
                        continue;
                    }
                    self.emit_stop(ep, reason, &response).await?;
                    let why = if self.no_change_retried {
                        "again after being told to continue"
                    } else {
                        "on its last round"
                    };
                    return Ok((
                        WorkerOutcome::Failed {
                            error: format!(
                                "the model ended the attempt as done {why} with no write and no \
                                 mutating command, although its contract writes {}",
                                self.write_scope_paths()
                            ),
                        },
                        response.text(),
                    ));
                }
                (StopReason::EndTurn | StopReason::StopSequence, true) => {
                    self.emit_stop(ep, reason, &response).await?;
                    return Ok((WorkerOutcome::Completed, response.text()));
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
                    signature: response.content.iter().find_map(|block| match block {
                        Content::ToolUse {
                            id: block_id,
                            signature,
                            ..
                        } if block_id == id => signature.clone(),
                        _ => None,
                    }),
                };
                self.emit(ep, started).await?;
            }
            if final_round {
                // A turn that ends on a tool call produces no answer.
                let ids: Vec<String> = uses.iter().map(|u| u.0.clone()).collect();
                self.refuse_all(ep, ids, FINAL_ROUND_WHY).await?;
                return Ok((WorkerOutcome::BudgetExhausted, String::new()));
            }
            match self.dispatch(ep, uses).await {
                Ok(Some(stall)) => {
                    return Ok((
                        WorkerOutcome::Stalled {
                            call_digest: stall.call_digest,
                            repeats: stall.repeats,
                            call: stall.call,
                        },
                        String::new(),
                    ));
                }
                Ok(None) => {}
                Err(Halt::Abandoned { phase, partial, .. }) => {
                    // Reached only from `dispatch`'s own `Halt::Abandoned` (below),
                    // which always fires after the round's model call already ran;
                    // the round stays spent, so no refund here.
                    self.abandon(ep, phase, partial, STEER_ABANDON_REASON)
                        .await?;
                }
                Err(halt) => return Err(halt),
            }
        }
    }

    /// Digests of every path the progress gate tracks (dirty ones included), and
    /// what git lists as changed when that could be read. File reads and git
    /// run off the executor thread; a failure is on the result's trace, and
    /// never read as "unchanged".
    async fn read_tree_state(
        &mut self,
    ) -> (
        std::collections::BTreeMap<PathBuf, Option<String>>,
        Option<BTreeSet<PathBuf>>,
    ) {
        let tracked = self.progress.tracked(&self.changed);
        self.read_tree_paths(tracked).await
    }

    /// The digests of `tracked` plus every path git lists as dirty, and the
    /// dirty set when git could be read.
    async fn read_tree_paths(
        &mut self,
        tracked: BTreeSet<PathBuf>,
    ) -> (
        std::collections::BTreeMap<PathBuf, Option<String>>,
        Option<BTreeSet<PathBuf>>,
    ) {
        let root = self.brief.workspace_root.clone();
        let scope = self.brief.write_scope.clone();
        let at_start = self.git_at_start.clone();
        let read = tokio::task::spawn_blocking(move || {
            // Generated files (an install, build output) are no progress and
            // are not digested: an install of thousands of files would cost
            // a hash each at every checkpoint.
            let status = at_start.ok().map(|entry| {
                crate::worktree::dirty_paths(&root, &entry).map(|status| {
                    without_generated(&root, status, scope.as_deref(), "checkpoint").0
                })
            });
            let mut paths = tracked;
            let mut failure = None;
            let dirty = match status {
                Some(Ok(status)) => {
                    paths.extend(status.paths.iter().cloned());
                    Some(status.paths.into_iter().collect::<BTreeSet<PathBuf>>())
                }
                Some(Err(e)) => {
                    failure = Some(e);
                    None
                }
                None => None,
            };
            (crate::progress::digests(&root, &paths), dirty, failure)
        })
        .await;
        match read {
            Ok((now, dirty, failure)) => {
                if let Some(e) = failure {
                    self.trace_failures
                        .push(format!("worktree status at checkpoint not read: {e}"));
                }
                (now, dirty)
            }
            Err(e) => {
                // No digests: every tracked file reads as absent, which the
                // measurement reports as changed or unchanged honestly
                // against its record; the failure is on the result.
                self.trace_failures
                    .push(format!("checkpoint measurement did not finish: {e}"));
                (Default::default(), None)
            }
        }
    }

    /// Measure a progress checkpoint (decision 0096), record it, and apply
    /// its verdict: another block of rounds, the grace notice, or the end.
    /// Returns the outcome when the verdict ends the attempt at once.
    async fn checkpoint(&mut self, ep: &WorkerEndpoint) -> Result<Option<WorkerOutcome>, Halt> {
        use crate::progress::CheckpointVerdict;
        let used = self.budget.used();
        let (now, dirty) = self.read_tree_state().await;
        let (measure, baseline) = self.progress.measure(used, &now, dirty.as_ref());
        let (checkpoint, baseline) = self.progress.decide(used, measure, baseline);
        let verdict = checkpoint.verdict;
        self.budget.extend(checkpoint.rounds_granted);
        self.emit(
            ep,
            WorkerEvent::Checkpoint {
                checkpoint: Box::new(checkpoint),
                baseline: Box::new(baseline),
            },
        )
        .await?;
        match verdict {
            CheckpointVerdict::Grace => {
                self.emit(
                    ep,
                    WorkerEvent::Notice {
                        kind: crate::event::NoticeKind::NoProgress,
                        text: crate::progress::grace_notice(self.progress.block()),
                    },
                )
                .await?;
                Ok(None)
            }
            CheckpointVerdict::EndStuck => Ok(Some(WorkerOutcome::BudgetExhausted)),
            CheckpointVerdict::Continue
            | CheckpointVerdict::EndNoProgress
            | CheckpointVerdict::Ceiling => Ok(None),
        }
    }

    /// Record, for the progress gate, the content of each path a call is
    /// about to write that the gate has no record of (decision 0096), so a
    /// write later put back is seen as no change.
    fn capture_preimages(&mut self, accesses: &[kgraf_tools::Access]) {
        let root = &self.brief.workspace_root;
        for path in accesses.iter().flat_map(|a| a.writes_paths()) {
            let gate = self.progress.needs_preimage(path);
            let segment = self
                .segment
                .as_ref()
                .is_some_and(|s| s.needs_preimage(path));
            if gate || segment {
                let digest = blocking(|| crate::progress::file_digest(&root.join(path)));
                if gate {
                    self.progress.capture_preimage(path.clone(), digest.clone());
                }
                if let Some(s) = self.segment.as_mut().filter(|_| segment) {
                    s.capture_preimage(path.clone(), digest);
                }
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
        reason: &str,
    ) -> Result<(), Halt> {
        let abandoned = WorkerEvent::RoundAbandoned {
            phase,
            reason: reason.to_owned(),
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

    /// Settle a call whose request never left the process as a known zero. A
    /// ledger error is recorded on the result, not dropped.
    async fn settle_unsent(&mut self, call: &CallId, token: &kgraf_provider::CallToken<'_>) {
        let zero = TokenUsage::default();
        match token.cancel_async(Some(&zero)).await {
            Ok(()) | Err(kgraf_provider::LedgerError::AlreadySettled { .. }) => {}
            Err(e) => self
                .trace_failures
                .push(ledger_failure(call, "zero settle", &e)),
        }
    }

    /// The changed paths that are worktree files: a protected write
    /// (decision 0094) lands in the git or state directory, not in the
    /// worktree, so it is left out.
    fn worktree_changes(&self) -> Vec<PathBuf> {
        self.changed
            .iter()
            .filter(|p| {
                !self
                    .env
                    .protected_writes
                    .as_ref()
                    .is_some_and(|g| g.wrote(p))
            })
            .cloned()
            .collect()
    }

    /// Whether the attempt changed anything or started a tool call that may have.
    fn made_progress(&self) -> bool {
        !self.done.is_empty() || !self.changed.is_empty()
    }

    /// The contract's declared write paths, comma separated; empty when it
    /// declares none.
    fn write_scope_paths(&self) -> String {
        self.brief
            .write_scope
            .as_ref()
            .map(|paths| {
                paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    }

    /// An end as done with no successful write and no mutating command in the
    /// attempt, for a contract that declares writes. Reads do not count: an
    /// attempt that read two files and then said it was done changed nothing
    /// (`made_progress` holds only successful writes and the paths a shell
    /// command changed, so a failed write or a read leaves it empty).
    ///
    /// A compaction does not lift the guard. `done` and `changed` carry across
    /// it (and across a resume, from the replay of every round), so a worker
    /// that wrote before its compaction and then ends as done is not caught,
    /// while one that has written nothing at all still is: a compaction with
    /// nothing written is exactly the case where the model has lost sight of
    /// its earlier reads and may announce work it never does.
    fn ended_without_change(&self) -> bool {
        self.brief
            .write_scope
            .as_ref()
            .is_some_and(|paths| !paths.is_empty())
            && !self.made_progress()
    }

    fn hand_back(
        &self,
        context_tokens: u64,
        queued: u64,
        compaction_stop: Option<crate::compaction::CompactionStop>,
    ) -> WorkerOutcome {
        let stop = self.brief.context.worker_stop_tokens();
        // The transcript alone is below the point when only the queued
        // messages crossed it.
        let cause = if self
            .brief
            .context
            .reached_point(self.builder.context_tokens())
            || queued == 0
        {
            crate::brief::HandoffCause::MeasuredContext
        } else {
            crate::brief::HandoffCause::QueuedMessages
        };
        WorkerOutcome::HandedBack {
            handoff: Handoff {
                done: self.done.clone(),
                remaining: vec![crate::compaction::handback_remainder(
                    &self.brief.instruction,
                    context_tokens,
                    stop,
                )],
                files_touched: self.worktree_changes(),
                context_tokens,
                cause,
                compaction_stop,
            },
        }
    }

    /// Whether this worker, which cannot hand back and has compacted, may
    /// compact again now (`compaction::recompaction`): the floor its last
    /// compaction left, the progress since it, and the checkpoint ceiling.
    async fn may_compact_again(&mut self) -> Result<(), crate::compaction::CompactionStop> {
        let compactions = self.compactions.len() as u32;
        let floor = self.builder.compaction_floor().unwrap_or(0);
        let stop = self.brief.context.worker_stop_tokens();
        let measure = match self.segment.as_ref() {
            Some(segment) => {
                let paths = segment.tracked(&self.changed);
                let (now, dirty) = self.read_tree_paths(paths).await;
                self.segment
                    .as_ref()
                    .map(|s| s.measure(self.budget.used(), &now, dirty.as_ref()))
            }
            None => None,
        };
        crate::compaction::recompaction(
            compactions,
            self.progress.ceiling(),
            floor,
            stop,
            measure.as_ref(),
        )
    }

    /// Clear the state that describes the context a compaction replaced.
    ///
    /// - The stall counts: the earlier results are no longer in the model's
    ///   context, so a re-read after the compaction is legitimate. Counting
    ///   starts again at the compaction, so a call repeated after it still
    ///   stalls at the threshold. A resume does the same from the replay
    ///   (`TranscriptBuilder::replay_compaction_round`).
    /// - The one-continuation flags for an empty turn and an output-ceiling
    ///   stop: the notice or the partial output they pair with is not in the
    ///   remainder. A resume starts them clear as well.
    ///
    /// Carried across on purpose: `done` and `changed` (what the worktree
    /// holds), the no-change notice (one per attempt; a second end with
    /// nothing written fails whether or not a compaction came between),
    /// `patch_rejections` (a later rejection gets the worked example sooner),
    /// the step budget, the progress gate, the refusal and denial counts, and
    /// the malformed-output bound (it counts server faults, not context).
    fn reset_context_state(&mut self) {
        self.stall.reset();
        self.empty_turn_retried = false;
        self.output_limit_retried = false;
    }

    /// Keep `event` (persisted form) for the recall archive of a later
    /// compaction.
    fn archive_event(&mut self, event: &WorkerEvent) {
        if crate::compaction::archived_kind(event) {
            self.history.push(event.clone());
        }
    }

    /// Compact this worker's context (decision 0095): ask the worker's
    /// own route for a summary in the fixed schema through the ledger, then
    /// emit `Compacted`, which rebuilds the remainder from the verbatim
    /// opening plus the notice and summary. A summary call that fails or
    /// answers out of schema gives a summary built from what the loop
    /// observed; the worker goes on either way. Only a cancel ends the run
    /// here.
    async fn compact(
        &mut self,
        ep: &WorkerEndpoint,
        trigger: CompactionTrigger,
        at_tokens: u64,
    ) -> Result<(), Halt> {
        // The worker's own context as the next request would carry it, with
        // the fixed request joined to the last user turn when there is one,
        // so the roles alternate on every wire.
        let mut messages = self.env.masker.mask(self.builder.messages());
        match messages.last_mut() {
            Some(last) if last.role == kgraf_provider::Role::User => {
                last.content.push(Content::text(SUMMARY_REQUEST))
            }
            _ => messages.push(kgraf_provider::Message::user(SUMMARY_REQUEST)),
        }
        let mut request = self.env.prefix.request(
            messages,
            self.env.settings.max_output_tokens,
            self.env.settings.temperature,
        );
        request.reasoning_effort = self
            .env
            .settings
            .reasoning
            .effort(kgraf_provider::ReasoningProcess::WorkerCompaction);
        let (reply, summary_call) = self.summary_call(ep, &request).await?;
        let parsed =
            reply.and_then(|text| crate::compaction::parse_summary(&text, &self.brief.instruction));
        let changes = self.worktree_changes();
        let (mut summary, summary_source) = match parsed {
            Ok(summary) => (summary, SummarySource::Model),
            Err(reason) => (
                CompactionSummary::default(),
                SummarySource::Runtime { reason },
            ),
        };
        // kGraf's own state fills what the summary lacks: the goal from the
        // instruction, the writes that succeeded (when the summary lists no
        // decisions), the changed files, and every declared write path not
        // yet changed as an open item. A fallback summary is all of this; a
        // model summary keeps its own wording and gains only what it left
        // out, so a summary that drops the goal never loses the task.
        // A later compaction keeps the earlier summary's decisions and quoted
        // constraints word for word: this summary was written from a context
        // that held them, and a restatement of a restatement drifts.
        if let Some(earlier) = self.builder.last_summary() {
            crate::compaction::carry_forward(&mut summary, earlier);
        }
        crate::compaction::complete_from_state(
            &mut summary,
            &self.brief.instruction,
            self.brief.write_scope.as_deref(),
            &self.done,
            &changes,
        );
        let archive = HistoryArchive::from_events(&self.history);
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        let mut notice = CompactionNotice {
            worker: self.brief.worker.to_string(),
            task: self.brief.task.to_string(),
            trigger,
            at_ms,
            at_tokens,
            stop_tokens: self.brief.context.worker_stop_tokens(),
            window_tokens: self.brief.context.effective_window(),
            summary_tokens: 0,
            summary_call,
            summary_source,
            transcript_path: self.transcript_path.clone(),
            archived_entries: archive.len() as u64,
            recall: self.env.tools.get(RECALL_TOOL).is_some(),
            items_done: self.done.len() as u64,
            items_remaining: summary.open_items.len() as u64,
            number: self.compactions.len() as u32 + 1,
            may_compact_again: self.stop_point == StopPointPolicy::Compact
                && (self.compactions.len() as u32 + 1) < self.progress.ceiling(),
        };
        notice.summary_tokens = kgraf_core::budget::estimate_tokens(
            &crate::compaction::render_notice(&notice, &summary),
        );
        // The baseline the progress since this compaction is measured
        // against, for a worker that cannot hand back: the tree and the
        // check scores now. Stored on the event, so a resume measures the
        // same segment. Taken at the ceiling too, so the hand-back there says
        // whether the worker was still moving.
        let baseline = if self.stop_point == StopPointPolicy::Compact {
            let paths = self.progress.tracked(&self.changed);
            let (files, _) = self.read_tree_paths(paths).await;
            let baseline = crate::progress::CheckpointBaseline {
                files,
                checks: self.progress.latest_checks(),
            };
            self.segment = Some(crate::progress::SegmentGate::start(
                baseline.clone(),
                self.budget.used(),
            ));
            Some(Box::new(baseline))
        } else {
            self.segment = None;
            None
        };
        // Set before the event is sent: recall reads everything before this
        // notice, and a user's request is refused from here on.
        self.archive = Some(archive);
        self.compactions.push(notice.clone());
        self.steering.mark_compacted();
        self.reset_context_state();
        self.emit(
            ep,
            WorkerEvent::Compacted {
                notice: Box::new(notice),
                summary: Box::new(summary),
                baseline,
            },
        )
        .await
    }

    /// The summary call: one accounted, non-streamed call on the worker's own
    /// route, raced against cancellation. `Ok((Err(why), _))` is a call that
    /// did not give a usable reply; the caller falls back. No ledger row, no
    /// call (every token is attributed).
    async fn summary_call(
        &mut self,
        ep: &WorkerEndpoint,
        request: &Request,
    ) -> Result<(Result<String, String>, Option<CallId>), Halt> {
        if let Err(e) = self.env.prefix.check(request) {
            return Err(Halt::Failed(e.to_string()));
        }
        let provider = self.env.provider.clone();
        let trace = CallTrace {
            parent_call: self.last_call.clone(),
            kind: Some(crate::compaction::COMPACTION_CALL_KIND.to_owned()),
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
        let ledger = self.env.ledger.clone();
        let token = match ledger
            .begin_traced_async(
                provider.adapter(),
                provider.protocol(),
                &request.model,
                self.brief.worker.as_str(),
                self.brief.task.as_str(),
                trace,
            )
            .await
        {
            Ok(token) => token,
            Err(e) => {
                return Ok((
                    Err(format!(
                        "no ledger row could be opened for the summary call: {e}"
                    )),
                    None,
                ));
            }
        };
        let call = token.id().clone();
        self.calls.push(call.clone());
        self.last_call = Some(call.clone());
        self.phase = CancelPhase::ModelStream;
        // The summary call is not a round: no `ModelCall` or `Usage` event
        // names it, so a resume never counts it against the step budget. A
        // cancel before the request leaves settles it as a known zero in the
        // ledger, and neither `RoundAbandoned` nor a usage event is written
        // (either would make a rebuild miscount the rounds).
        // The rounds it summarizes are stored before it leaves, as before a
        // round's request (`call_model`).
        Self::await_stored(ep).await;
        if ep.is_cancelled() {
            self.settle_unsent(&call, &token).await;
            return Err(Halt::Cancelled {
                phase: CancelPhase::ModelStream,
                partial: None,
            });
        }
        let outcome = tokio::select! {
            biased;
            () = ep.cancelled() => None,
            r = provider.complete(request, &token) => Some(r),
        };
        self.phase = CancelPhase::BetweenRounds;
        match outcome {
            None => {
                if let Some(failure) = settle_cancelled(&call, &token, &StreamSeen::default()).await
                {
                    self.trace_failures.push(failure);
                }
                Err(Halt::Cancelled {
                    phase: CancelPhase::ModelStream,
                    partial: None,
                })
            }
            Some(Err(e)) => Ok((Err(format!("the summary call failed: {e}")), Some(call))),
            Some(Ok(response)) => Ok((Ok(response.text()), Some(call))),
        }
    }

    /// Answer one `recall_history` call from this worker's own archive.
    fn recall(&self, input: &serde_json::Value) -> (String, bool) {
        let Some(archive) = &self.archive else {
            return (ToolOutput::error(NOTHING_TO_RECALL).render(), true);
        };
        match archive.recall(input) {
            Ok(text) => (ToolOutput::text(text).render(), false),
            Err(why) => (ToolOutput::error(why).render(), true),
        }
    }

    /// Wait until the reader has handled every event emitted so far
    /// ([`WorkerEndpoint::stored`]), before a paid request leaves. Its two
    /// errors need nothing here: a cancel is read by the caller's own cancel
    /// check right after, and a reader that left stores nothing more, which
    /// `send` already treats as delivered.
    async fn await_stored(ep: &WorkerEndpoint) {
        let _ = ep.stored().await;
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
        if let Err(halt) = self
            .emit(ep, WorkerEvent::ModelCall { call: call.clone() })
            .await
        {
            // The row is open and no request was sent. Whatever ends the worker
            // here settles the row as a known zero; returning with the token
            // dropped would leave it unreconciled, as unknown spend. (A reader
            // that left is not a halt: `emit` treats it as delivered.)
            let zero = TokenUsage::default();
            self.settle_unsent(&call, &token).await;
            if matches!(halt, Halt::Cancelled { .. }) {
                self.cancel_unsent = true;
                return Err(Halt::Cancelled {
                    phase: CancelPhase::ModelStream,
                    partial: Some((call, zero)),
                });
            }
            return Err(halt);
        }
        // Every earlier round, and this round's `ModelCall`, is in the
        // transcript before the request leaves: a process killed while it is
        // in flight repeats this request on resume, never a round already
        // paid and answered.
        Self::await_stored(ep).await;

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
            self.settle_unsent(&call, &token).await;
            return Err(Halt::Abandoned {
                phase: CancelPhase::ModelStream,
                partial: Some((call, zero)),
                // The request never left this function: no provider was called.
                sent: false,
            });
        }
        // A cancel that arrived before the request left is the same case: the
        // provider future below is never polled when the cancel is already
        // pending (the select is biased), so no request was sent and the call
        // settles as a known zero rather than an unreconciled row of unknown
        // spend.
        if ep.is_cancelled() {
            let zero = TokenUsage::default();
            self.settle_unsent(&call, &token).await;
            self.cancel_unsent = true;
            return Err(Halt::Cancelled {
                phase: CancelPhase::ModelStream,
                partial: Some((call, zero)),
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
                        if let Some(failure) =
                            settle_cancelled(&call, &token, &StreamSeen::default()).await
                        {
                            self.trace_failures.push(failure);
                        }
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
                    Ok(Err(e)) => return Err(self.provider_failed(&e)),
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
                    if let Err(e) = token.close_on_async(&error, None).await {
                        self.trace_failures.push(ledger_failure(&call, "close", &e));
                    }
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
                        let response = result.map_err(|e| self.provider_failed(&e))?;
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
                        if let Some(failure) = settle_cancelled(&call, &token, &seen).await {
                            self.trace_failures.push(failure);
                        }
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
        // Decision 0095: `recall_history` is answered here, from this
        // worker's own archive, after approval like every call.
        let mut recalls: Vec<ApprovedCall> = Vec::new();
        self.capabilities.clear();
        // The worktree the call's programs read their recipes, scripts and
        // configuration from, so a refusal names the inner program a rule
        // matched (decision 0094, amended 2026-10-06).
        let read_ctx = kgraf_tools::permissions::ReadContext::in_worktree(
            &self.brief.workspace_root,
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .as_deref(),
        );
        for (call_id, tool, input) in uses {
            self.capabilities.insert(
                call_id.clone(),
                crate::shortfall::CallCapability::of_in(&tool, &input, &read_ctx),
            );
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
                        if let Some((why, path)) = self.scope_violation(call.access()) {
                            self.denied += 1;
                            let mut finished = WorkerEvent::ToolCallFinished {
                                call_id: call_id.clone(),
                                output: format!("{REFUSED_PREFIX}{why}"),
                                is_error: true,
                            };
                            self.note_refusal(&mut finished, path);
                            self.emit(ep, finished).await?;
                            break;
                        }
                        if call.tool() == RECALL_TOOL {
                            recalls.push(call);
                            break;
                        }
                        approved.push(call);
                        break;
                    }
                    Verdict::Denied { by, reason } => {
                        self.denied += 1;
                        let output = if by == kgraf_tools::INVALID_INPUT {
                            format!("{INVALID_CALL_PREFIX}{reason}{INVALID_CALL_ADVICE}")
                        } else {
                            format!("{DENIED_PREFIX}{by}: {reason}")
                        };
                        let mut finished = WorkerEvent::ToolCallFinished {
                            call_id: call_id.clone(),
                            output,
                            is_error: true,
                        };
                        self.note_refusal(&mut finished, None);
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
        if let Some(stall) = self.stall.observe(
            approved
                .iter()
                .chain(recalls.iter())
                .map(|a| (a.tool(), a.input())),
        ) {
            let ids: Vec<String> = approved
                .iter()
                .chain(recalls.iter())
                .map(|a| a.call_id().to_owned())
                .collect();
            self.refuse_all(ep, ids, "the same call repeated; stall detected")
                .await?;
            return Ok(Some(stall));
        }
        for call in recalls {
            let (output, is_error) = self.recall(call.input());
            let finished = WorkerEvent::ToolCallFinished {
                call_id: call.call_id().to_owned(),
                output,
                is_error,
            };
            self.emit(ep, finished).await?;
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
        // Before any call of the round runs (decision 0096).
        self.capture_preimages(&accesses);
        let ids: Vec<String> = approved.iter().map(|a| a.call_id().to_owned()).collect();
        let tools: Vec<String> = approved.iter().map(|a| a.tool().to_owned()).collect();
        // The stall detector's key for each call, to tell it whether the call's
        // result is one the masker hides (`record_result`).
        let digests: Vec<String> = approved
            .iter()
            .map(|a| crate::stall::call_digest(a.tool(), a.input()))
            .collect();
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
            self.denied += start_ready(
                &mut calls,
                &waits,
                &done,
                &writes,
                &mut running,
                registry,
                &contexts,
                self.fence.as_ref(),
                &self.approval,
                &progress_tx,
            );
        }
        let why = 'round: loop {
            let next = tokio::select! {
                biased;
                () = ep.cancelled() => break Interruption::Cancel,
                () = &mut interrupt => break Interruption::Steer,
                Some((call_id, reason)) = progress_rx.recv() => {
                    let event = match reason {
                        Some(reason) => WorkerEvent::Waiting { call_id, reason },
                        None => WorkerEvent::Resumed { call_id },
                    };
                    match self.emit(ep, event).await {
                        Ok(()) => continue,
                        // A cancel during the emit still runs the grace loop
                        // below, so a sibling that finishes is recorded.
                        Err(Halt::Cancelled { .. }) => break Interruption::Cancel,
                        Err(other) => return Err(other),
                    }
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
            //
            // The finished call is recorded before this drain, so a cancel
            // during it cannot lose the call's writes or mark a finished call
            // interrupted.
            done[j] = true;
            let mut finished =
                self.record_result(j, &accesses, call_id, &tool, &digests[j], result);
            self.note_refusal(&mut finished, None);
            // A sandboxed build declares no paths; ask git what it changed. Before
            // any emit: a cancel during an emit must not lose the paths.
            if accesses[j].is_sandboxed_build() {
                self.record_tree_changes(&tool).await;
            }
            let mut cancelled_in_drain = false;
            while let Ok((report_id, reason)) = progress_rx.try_recv() {
                let event = match reason {
                    Some(reason) => WorkerEvent::Waiting {
                        call_id: report_id,
                        reason,
                    },
                    None => WorkerEvent::Resumed { call_id: report_id },
                };
                match self.emit(ep, event).await {
                    Ok(()) => {}
                    Err(Halt::Cancelled { .. }) => {
                        cancelled_in_drain = true;
                        break;
                    }
                    Err(other) => return Err(other),
                }
            }
            if cancelled_in_drain {
                // The call finished: its result reaches the transcript (an
                // unbounded emit, since a bulk emit is refused after a cancel),
                // then the grace loop records the siblings still running.
                self.emit_after_cancel(ep, finished)?;
                break 'round Interruption::Cancel;
            }
            match self.emit(ep, finished).await {
                Ok(()) => {}
                // The result was persisted unbounded by `send`; the grace loop
                // records the siblings still running.
                Err(Halt::Cancelled { .. }) => break Interruption::Cancel,
                Err(other) => return Err(other),
            }
            // The emit above can wait for a bulk slot, which is exactly when
            // an interrupt can arrive: checked after it, not before.
            if !self.stopping(ep) {
                self.denied += start_ready(
                    &mut calls,
                    &waits,
                    &done,
                    &writes,
                    &mut running,
                    registry,
                    &contexts,
                    self.fence.as_ref(),
                    &self.approval,
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
            let finished = self.record_result(j, &accesses, call_id, &tool, &digests[j], result);
            if accesses[j].is_sandboxed_build() {
                self.record_tree_changes(&tool).await;
            }
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
        // A sandboxed build or test that outlived the grace declares no paths,
        // and may have written part of its work already: ask git, as for one
        // that returned. What it writes after this scan is not seen.
        if let Some(&j) = outlived.iter().find(|&&j| accesses[j].is_sandboxed_build()) {
            self.record_tree_changes(&tools[j]).await;
        }
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
                        self.stall.reset();
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
        digest: &str,
        result: Result<ToolOutput, ToolError>,
    ) -> WorkerEvent {
        let (output, is_error) = match result {
            Ok(output) => {
                let is_error = output.is_error;
                (output.render(), is_error)
            }
            Err(e) => (format!("ERROR: {e}"), true),
        };
        // A repeat of this call after its result leaves the masker's window is a
        // re-read only if the masker hid the result.
        self.stall
            .note_result(digest, self.env.masker.hides_result(&output, is_error));
        if !is_error {
            for path in accesses[j].writes_paths() {
                self.changed.insert(path.clone());
                self.done
                    .push(format!("{tool} wrote {}", path.to_string_lossy()));
                // The workspace changed: identical calls (a test run, a re-read)
                // may now differ, so they start a new count. The stall check for
                // this round already ran, before execution.
                self.stall.reset();
            }
        }
        WorkerEvent::ToolCallFinished {
            call_id,
            output,
            is_error,
        }
    }

    /// Decision 0093: note a finished call for the refusal record. A policy
    /// refusal (a scope or fence refusal, or a denial a policy made) is
    /// recorded and its text gets the reporting hint; a call that ran without
    /// error marks earlier refusals of its capability as retried. A denial
    /// for malformed input or a misplaced path is neither; the newest
    /// malformed-input error is kept for a report's answer. `path`: the path a
    /// write-scope refusal named.
    fn note_refusal(&mut self, event: &mut WorkerEvent, path: Option<String>) {
        self.note_refusal_record(event, path);
        self.add_patch_help(event);
    }

    /// After the second rejected `apply_patch` call of this worker, a
    /// rejection ends with the rule it broke and a minimal valid patch for the
    /// file the call targeted ([`kgraf_tools::rejection_help`]). A policy
    /// refusal is not a rejected patch: the call's format was not the problem.
    fn add_patch_help(&mut self, event: &mut WorkerEvent) {
        let WorkerEvent::ToolCallFinished {
            call_id,
            output,
            is_error,
        } = event
        else {
            return;
        };
        if !*is_error
            || output.starts_with(DENIED_PREFIX)
            || output.starts_with(REFUSED_PREFIX)
            || self
                .capabilities
                .get(call_id.as_str())
                .is_none_or(|c| c.tool != "apply_patch")
        {
            return;
        }
        self.patch_rejections += 1;
        if self.patch_rejections < 2 {
            return;
        }
        let target = self
            .capabilities
            .get(call_id.as_str())
            .and_then(|c| c.patch_target.as_ref());
        if let Some(help) = kgraf_tools::rejection_help(output, target) {
            output.push_str(&help);
        }
    }

    fn note_refusal_record(&mut self, event: &mut WorkerEvent, path: Option<String>) {
        let WorkerEvent::ToolCallFinished {
            call_id,
            output,
            is_error,
        } = event
        else {
            return;
        };
        let Some(call) = self.capabilities.get(call_id.as_str()).cloned() else {
            return;
        };
        let tool = call.tool.clone();
        if !*is_error {
            self.refused.ran_ok(&tool, call.programs());
            return;
        }
        if output.starts_with(INVALID_CALL_PREFIX)
            || (output.starts_with("ERROR: invalid input")
                && tool != crate::shortfall::SHORTFALL_TOOL)
        {
            self.refused.input_error(&tool, output.clone());
            return;
        }
        let policy = if let Some(rest) = output.strip_prefix(DENIED_PREFIX) {
            let (by, reason) = rest.split_once(": ").unwrap_or((rest, ""));
            crate::shortfall::is_policy_denial(by, reason).then(|| by.to_owned())
        } else {
            output.starts_with(REFUSED_PREFIX).then(String::new)
        };
        let Some(by) = policy else {
            return;
        };
        self.refused.refused(call.refusal(&by, output, path));
        let can_report = self
            .env
            .tools
            .get(crate::shortfall::SHORTFALL_TOOL)
            .is_some();
        *output = crate::shortfall::with_refusal_hint(output, can_report);
    }

    /// Record the worktree paths git reports as changed after a sandboxed build
    /// or test command, which declares none. A status that cannot be read is
    /// reported on the result, not skipped: the handoff would otherwise omit
    /// files the command wrote without saying so.
    async fn record_tree_changes(&mut self, tool: &str) {
        let root = self.brief.workspace_root.clone();
        let scope = self.brief.write_scope.clone();
        let at_start = self.git_at_start.clone();
        let label = tool.to_owned();
        let read = tokio::task::spawn_blocking(move || {
            crate::worktree::dirty_paths(&root, &at_start?)
                .map(|status| without_generated(&root, status, scope.as_deref(), &label))
        })
        .await;
        match read {
            Ok(Ok((status, note))) => {
                note_nested_repositories(&mut self.trace_failures, &status);
                if let Some(note) = note
                    && !self.done.contains(&note)
                {
                    self.done.push(note);
                }
                for path in status.paths {
                    if self.changed.insert(path.clone()) {
                        self.done
                            .push(format!("{tool} changed {}", path.to_string_lossy()));
                    }
                }
            }
            Ok(Err(e)) => self
                .trace_failures
                .push(format!("worktree status after {tool} not read: {e}")),
            Err(e) => self
                .trace_failures
                .push(format!("worktree status after {tool} not read: {e}")),
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
            self.archive_event(&event);
            self.progress.observe(&event, &EnvFacts(&self.env));
            if let Some(segment) = self.segment.as_mut() {
                segment.observe(&event, &EnvFacts(&self.env));
            }
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

/// Start every call whose earlier conflicting calls are done. Returns how
/// many calls the approval chain's policies denied as they started.
///
/// 10 arguments: the round's own per-call state (`calls`/`waits`/`done`/`writes`,
/// one slice per call), plus `running`/`registry`/`contexts`/`progress_tx`, the
/// subagent scope `fence` (decision 0067), and the worker's approval `chain`,
/// whose policies are checked again as each call starts (decision 0053). A
/// bundling struct was considered and rejected: every field here is borrowed
/// with its own lifetime or mutability from `execute`'s own locals, so a struct
/// would only rename the same borrows one level removed, not reduce them.
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
    chain: &ApprovalChain,
    progress_tx: &tokio::sync::mpsc::UnboundedSender<(String, Option<String>)>,
) -> u32 {
    let mut denied = 0;
    for (j, slot) in calls.iter_mut().enumerate() {
        if !waits[j].iter().all(|&i| done[i]) {
            continue;
        }
        if let Some(call) = slot.take() {
            // Decision 0053: the approval chain's policies are checked again
            // as the call starts. A skill activated by an earlier call of this
            // round narrowed the worker's `allowed-tools` after this call was
            // approved; the `skill` tool's access is a barrier, so this call
            // started only after the activation finished. Recorded as the
            // approval chain records a denial: counted, and with its prefix.
            if let Err(refusal) = chain.recheck(registry, &call) {
                denied += 1;
                let id = call.call_id().to_owned();
                let tool = call.tool().to_owned();
                let output = format!("{DENIED_PREFIX}{}: {}", refusal.by, refusal.reason);
                running.push(Box::pin(async move {
                    (j, id, tool, Ok(ToolOutput::error(output)))
                }));
                continue;
            }
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
    denied
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
/// The `.git` entry a resumed worker's status reads check against: the one
/// the transcript's first recorded `Context` holds. A recorded "no gitlink"
/// stays that way: a `.git` found at the root now could have been planted
/// by an earlier command. A transcript written before the gitlink was
/// recorded has a `Context` with neither field; for it the entry is read at
/// the root now. The worker is resumed only in a worktree the host has just
/// reopened, and reopen refuses one whose gitlink or admin directory is not
/// what the host recorded when it made it, so that read is the host's
/// verified entry. This segment's `Context` then records it, and a later
/// resume uses the record.
fn resumed_git_entry(
    root: &std::path::Path,
    events: &[WorkerEvent],
) -> Result<crate::worktree::GitEntry, String> {
    let none_recorded = || {
        Err(format!(
            "no gitlink at {} was recorded when this worker first started",
            root.display()
        ))
    };
    let mut old_format = false;
    for event in events {
        if let WorkerEvent::Context {
            gitlink,
            no_gitlink,
            ..
        } = event
        {
            match (gitlink, no_gitlink) {
                (Some(gitlink), _) => {
                    return Ok(crate::worktree::GitEntry::recorded(root, gitlink.clone()));
                }
                // A record of none, even one a segment after an old-format
                // one wrote: never read what is on disk now.
                (None, true) => return none_recorded(),
                (None, false) => old_format = true,
            }
        }
    }
    if old_format {
        crate::worktree::git_entry(root)
    } else {
        none_recorded()
    }
}

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
///
/// A settle that the ledger refuses leaves the row in flight. It is returned as a
/// failure line for the run's `trace_failures`, never dropped.
async fn settle_cancelled(
    call: &CallId,
    token: &kgraf_provider::CallToken<'_>,
    seen: &StreamSeen,
) -> Option<String> {
    let ack = match &seen.usage {
        Some(usage) => token.cancel_async(Some(usage)),
        None => token.leave_unreconciled_async(seen.request_id.as_deref()),
    };
    match ack.await {
        Ok(()) => None,
        // The adapter finished the call as the cancel landed; that stands.
        Err(kgraf_provider::LedgerError::AlreadySettled { .. }) => None,
        Err(e) => Some(ledger_failure(call, "cancel settle", &e)),
    }
}

/// The failure line for a ledger write the loop could not make.
fn ledger_failure(call: &CallId, what: &str, error: &kgraf_provider::LedgerError) -> String {
    format!("call {call}: ledger {what} failed, the row stays open: {error:?}")
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

#[cfg(test)]
mod resumed_git_entry_tests {
    use super::{WorkerEvent, resumed_git_entry};
    use std::path::Path;

    fn context(gitlink: Option<&str>, no_gitlink: bool) -> WorkerEvent {
        WorkerEvent::Context {
            prefix_digest: "d".into(),
            slices: vec![],
            slices_left_out: Vec::new(),
            gitlink: gitlink.map(str::to_owned),
            no_gitlink,
        }
    }

    /// A recorded "no gitlink" survives the store: it is written, and read
    /// back as a record, not as the old format's gap.
    #[test]
    fn a_recorded_none_round_trips() {
        let text = serde_json::to_string(&context(None, true)).expect("json");
        assert!(text.contains("\"no_gitlink\":true"), "{text}");
        let back: WorkerEvent = serde_json::from_str(&text).expect("back");
        assert!(matches!(
            back,
            WorkerEvent::Context {
                no_gitlink: true,
                ..
            }
        ));
        let with = serde_json::to_string(&context(Some("gitdir: x"), false)).expect("json");
        assert!(!with.contains("no_gitlink"), "{with}");
    }

    /// Nowhere to read from: the root has no `.git` at all. A recorded none,
    /// even after an old-format segment, is final; an old-format transcript
    /// alone reads the root (and here finds nothing).
    #[test]
    fn a_record_wins_and_only_the_old_format_reads_the_root() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        let recorded = resumed_git_entry(root, &[context(Some("gitdir: /x"), false)]);
        assert_eq!(recorded.expect("recorded").gitlink(), "gitdir: /x");
        let none = resumed_git_entry(root, &[context(None, false), context(None, true)])
            .expect_err("a recorded none");
        assert!(none.contains("was recorded"), "{none}");
        let old = resumed_git_entry(root, &[context(None, false)]).expect_err("nothing on disk");
        assert!(!old.contains("was recorded"), "the root was read: {old}");
        let empty = resumed_git_entry(Path::new("/nonexistent"), &[]).expect_err("no context");
        assert!(empty.contains("was recorded"), "{empty}");
    }
}
