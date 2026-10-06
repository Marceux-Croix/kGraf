//! The worker loop. It builds context from graph slices (decision 0043),
//! calls a provider through the accounting path (every token is attributed), dispatches tool calls, and
//! returns a structured result.
//!
//! - [`spawn_worker`]: check a [`WorkerBrief`] against a [`WorkerEnv`] and run the
//!   loop as a task; the state machine is documented in `worker.rs`.
//! - [`WorkerEvent`] and [`EventSequence`]: the neutral event sequence, approval
//!   decisions included, and the order it must follow. [`TranscriptBuilder`] and
//!   [`ResumeState`] rebuild the model's messages from it; the loop builds its own
//!   messages the same way, so a resume sees what the loop would have sent.
//! - [`WorkerBrief`] in, [`WorkerResult`] out, with a structured [`Handoff`].
//! - [`WorkerHandle`] / [`WorkerEndpoint`]: cancel mid-stream, read events, await
//!   the result; bulk events are backpressured, control events never wait.
//! - [`stream_bridge`]: the provider's synchronous stream sink to the async loop.
//! - [`ContextConfig`]: the context thresholds of decision 0057 addendum 2.
//! - [`StopPointPolicy`], [`CompactionNotice`], [`RecallHistoryTool`]: the
//!   runtime-enforced compaction of decision 0095 and the recall tool.
//! - [`SharedPrefix`], [`PrefixSnapshot`], [`PrefixRegistry`]: the byte-identical
//!   prefix, checked on every request and admitted once per vendor and model.
//! - [`ObservationMasker`]: the masking seam; [`SessionMasker`], the default, is
//!   `kgraf-session`'s `reduction::reduce` (masking only, never truncation).
//! - [`StepBudget`], [`StallDetector`], [`dependencies`]: the step budget, stall
//!   detection (0056) and the tool-call ordering.
//!
//! **Ported code.** Each block carries its provenance comment:
//!
//! - Masking: SWE-agent `LastNObservations`, ported in `kgraf-session`'s
//!   `reduction` module (its provenance lives there); `masking.rs` adapts it.
//! - `step.rs`: mini-swe-agent `src/minisweagent/agents/default.py`
//!   (`DefaultAgent.query` step limit), commit
//!   04d809ceab9df28f9adaed044884180159172930, MIT. The reserved final round and
//!   the three-rounds-left notice are not in upstream and are written here.
//! - `inject.rs`: LibreChat's steering queue and drain (`InMemoryJobStore.ts`
//!   steer methods, `agents/steering/runtime.ts`), commit
//!   7b2362d7a7c6148b84850924dc7fa5fc43307923, MIT; see
//!   `third_party/librechat/UPSTREAM`.
//!
//! Written new, with the search recorded: stall detection (0056 found no permissive
//! library), the tool-call dependency order, and the loop itself (0055: no agent
//! framework allows the accounting path and the shared-prefix layout).

mod bridge;
mod brief;
mod compaction;
mod context;
mod dispatch;
mod event;
mod fence;
mod handle;
mod inject;
mod masking;
mod paths;
mod permission_resolver;
mod prefix;
mod progress;
mod rebuild;
mod settle;
mod shortfall;
mod stall;
mod step;
mod thresholds;
mod worker;
mod worktree;

pub use bridge::{StreamPump, StreamSink, stream_bridge};
pub use brief::{
    BriefError, Handoff, HandoffCause, MIN_STEP_BUDGET, RATIONALE_WORD_CAP, Rationale, WorkerBrief,
    WorkerOutcome, WorkerResult, parse_rationale,
};
pub use compaction::{
    COMPACTION_CALL_KIND, COMPACTION_NOTICE_HEADER, CompactionNotice, CompactionSummary,
    CompactionTrigger, DEFAULT_READ_COUNT, HistoryArchive, MAX_READ_COUNT, MAX_RECALL_CHARS,
    MAX_RECALL_HITS, NOTHING_TO_RECALL, RECALL_TOOL, REMAINDER_HEAD, RecallHistoryTool,
    SUMMARY_REQUEST, StopPointPolicy, SummarySource, handback_remainder, original_instruction,
    parse_summary, render_notice,
};
pub use context::{
    GraphSlices, NeutralPrefixEncoder, NoSlices, PrefixCheckError, PrefixInvalidation,
    PrefixRegistry, PrefixSnapshot, RenderedSlice, SliceError, SliceLeftOut, SliceRendering,
    SliceSource, left_out_notice, opening_messages, opening_messages_with, render_system,
};
pub use dispatch::dependencies;
pub use event::{CancelPhase, EventSequence, NoticeKind, SequenceError, WorkerEvent};
pub use fence::{FenceGuard, ScopeFence};
pub use handle::{RuntimeError, WorkerEndpoint, WorkerHandle, worker_channel};
pub use inject::{CompactRequestError, InjectError, Injection, Injector};
pub use masking::{DEFAULT_KEEP_RECENT, NoMasking, ObservationMasker, SessionMasker};
pub use paths::{
    fold_name, fold_path, forbidden_path, is_build_config_path, is_protected_name, same_path,
};
pub use permission_resolver::ModeResolver;
pub use prefix::{
    McpInstructions, PrefixDigest, PrefixEncoder, PrefixError, PrefixSlice, SharedPrefix,
};
pub use progress::{
    CheckKind, CheckScore, Checkpoint, CheckpointBaseline, CheckpointVerdict,
    DEFAULT_MAX_CHECKPOINTS, FAILED_WRITE_LIMIT, ProgressMeasure, REPEATED_CALL_LIMIT,
    REPEATED_ERROR_LIMIT, RoundBlocks, SloppyPattern, check_score, command_of,
};
pub use rebuild::{INTERRUPTED_RESULT, ResumeError, ResumeState, TranscriptBuilder};
pub use settle::{TranscriptSettler, settle_for_disk};
pub use shortfall::{
    RefusalDirectory, RefusalRecord, RefusedCall, SHORTFALL_TOOL, capability_of, refusal_hint,
    refused_call,
};
pub use stall::{DEFAULT_STALL_REPEATS, Stall, StallDetector, call_digest};
pub use step::{RoundPlan, StepBudget, WARN_AT_ROUNDS_LEFT};
pub use thresholds::{
    ContextConfig, DEFAULT_CEILING_TOKENS, DEFAULT_COMPACTION_POINT_PERCENT,
    MAX_COMPACTION_POINT_PERCENT, MIN_COMPACTION_POINT_PERCENT, PointSource, ThresholdError,
};
pub use worker::{
    AskResolver, CANCEL_BEFORE_SEND_REASON, CallMode, EMPTY_TURN_ABANDON_REASON, EMPTY_TURN_NOTICE,
    LoopSettings, OUTPUT_LIMIT_ABANDON_REASON, OUTPUT_LIMIT_NOTICE,
    STEER_ABANDON_BEFORE_SEND_REASON, STEER_ABANDON_REASON, STEER_INTERRUPTED, STEER_NOT_RUN,
    SpawnOptions, WorkerEnv, WorkerError, blocking, spawn_worker,
};
