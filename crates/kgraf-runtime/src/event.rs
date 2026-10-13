//! The neutral worker event sequence.
//!
//! Streaming is layered as request adapter, stream demuxer, protocol parser, and
//! a **neutral event sequence the transcript is rebuilt from**. [`WorkerEvent`] is
//! that sequence. It names no vendor, and it is what a worker's transcript stores.
//!
//! **Order within a run** (a state machine; [`EventSequence`] enforces it):
//!
//! ```text
//! model round:  (TextDelta | ReasoningDelta)* Usage* AssistantMessage? Stop
//! tool round:   (ToolCallStarted ... ToolCallFinished)*   after Stop{ToolUse}
//! next round:   another model round follows directly       after Stop{PauseTurn}
//! end:          Stop{reason != ToolUse, PauseTurn} with no open tool call, or
//!               Cancelled
//! ```
//!
//! `AssistantMessage` (decision 0071) is emitted once,
//! right before `Stop`, on every round that reaches one naturally -- never on
//! `Cancelled`, whose partial round is dropped rather than turned into a
//! message (see `rebuild.rs`). It carries the round's settled, whole text and
//! reasoning from the assembled response, scrubbed once as one string, so a
//! secret split across two deltas is caught where scrubbing each delta on its
//! own cannot catch it.
//!
//! `Cancelled` may arrive at any point, including mid-stream and with tool calls
//! open. The open calls are then reported by [`EventSequence::open_tool_calls`] so a
//! resume knows which calls never finished.
//!
//! **Usage after a cancel is accepted** (every token is attributed). A stream cancelled mid-response
//! still billed for what it produced, and its final usage may only be known after
//! the cancel lands, so `Usage` is the one event accepted after `Cancelled`.
//!
//! **Approval decisions are events** ([`WorkerEvent::Approval`]), so the approval log
//! is part of the transcript rather than a second record beside it.

use kgraf_core::session::{InboxKind, InboxSource};
use kgraf_core::{CallId, NodeId, TokenUsage};
use kgraf_provider::{Content, StopReason, StreamEvent};
use kgraf_tools::approval::{ApprovalRecord, RecordedDecision};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Where a cancellation landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelPhase {
    /// While the model's response was streaming.
    ModelStream,
    /// While a tool was running.
    ToolCall,
    /// Between rounds.
    BetweenRounds,
}

/// One neutral event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    /// Visible text from the model.
    TextDelta { text: String },
    /// Reasoning from the model, kept apart from text because it is accounted apart.
    ReasoningDelta { text: String },
    /// The round's settled, whole assistant message (decision
    /// 0071): the full text and reasoning the model produced this
    /// round, from the assembled [`kgraf_provider::Response`], not
    /// concatenated from streamed deltas. Emitted once, right before
    /// [`WorkerEvent::Stop`], on every round that reaches one naturally
    /// (never on [`WorkerEvent::Cancelled`], whose partial round is already
    /// dropped, not turned into a message -- see `rebuild.rs`).
    ///
    /// This is the record a resume rebuilds the model's context from, and the
    /// one the shell renders once a round settles: individual `TextDelta`/
    /// `ReasoningDelta` events keep streaming live exactly as before (holding
    /// them back to reunite a split secret was tried and reverted -- it broke
    /// live delivery), but a secret split across two of them is only ever
    /// caught here, once the whole round's text is scrubbed as one string.
    /// `reasoning` is `""` when the round produced none.
    AssistantMessage {
        text: String,
        reasoning: String,
        /// The provider's token on the round's last text part (see
        /// `kgraf_provider::Content::Text`), kept so a resumed run sends it
        /// back. Never scrubbed: a changed byte makes the provider refuse it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<kgraf_provider::Signature>,
    },
    /// A tool call began.
    ToolCallStarted {
        call_id: String,
        /// The tool's wire name.
        tool: String,
        input: serde_json::Value,
        /// The provider's opaque token for this call (see
        /// `kgraf_provider::Content::ToolUse`), kept so a resumed run sends it
        /// back. Never scrubbed: a changed byte makes the provider refuse it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<kgraf_provider::Signature>,
    },
    /// A tool call ended.
    ToolCallFinished {
        call_id: String,
        /// The result as the model sees it.
        output: String,
        is_error: bool,
        /// Measured wall time of the tool's execution alone, in
        /// milliseconds. Absent for a call that never ran (a refusal, a
        /// denial, a synthetic result) and in a transcript written before the
        /// field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// Usage for one accounted provider call, with its ledger id (every token is attributed).
    Usage { call: CallId, usage: TokenUsage },
    /// The model ended a round.
    Stop {
        reason: StopReason,
        /// The provider's own finish string behind `reason`, where the
        /// adapter reads one (`stop`, `tool_calls`, `length`, `end_turn`).
        /// Absent in a transcript written before the field existed, and for
        /// a provider that sends none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
    },
    /// The worker was cancelled.
    Cancelled { phase: CancelPhase },
    /// An approval decision on a tool call.
    Approval { record: ApprovalRecord },
    /// What the worker's context was assembled from: the shared prefix, by digest,
    /// and the private slices, by node id (a trace records "what the call was given,
    /// as slice ids plus the prefix hash", not as text). Emitted once per run
    /// segment, before the first model call.
    Context {
        prefix_digest: String,
        slices: Vec<NodeId>,
        /// The slices of `slices` the worker was not given, each with its file
        /// and the reason (not in the worker's tree, another version there,
        /// unreadable, gone from the graph). `slices` stays the brief's list,
        /// so a resume compares the same list. Absent when every slice was
        /// given, and in a transcript written before the field.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        slices_left_out: Vec<crate::context::SliceLeftOut>,
        /// The gitlink at the worktree root as read when the worker first
        /// started, before any of its calls ran; a resumed segment carries the
        /// first segment's. A resume reads the worktree status only against
        /// this record, never against a `.git` found on disk at resume, which
        /// an earlier command could have planted. `None` with `no_gitlink`: no
        /// gitlink was at the root, and the status is not read.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gitlink: Option<String>,
        /// Set when the first segment found no gitlink it could read at the
        /// root, so a `None` above is a record, not a gap. A transcript written
        /// before the gitlink was recorded has neither field: its resume reads
        /// the entry at the root once the host's reopen has verified the
        /// worktree's gitlink against its own record, and records that.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        no_gitlink: bool,
    },
    /// A ledger row was opened for one model round, before the request left.
    /// Counts the round against the step budget on resume even when the call
    /// failed and produced no usage.
    ModelCall { call: CallId },
    /// Text the loop added to the next user turn (a step-budget warning, the
    /// final-round notice). Recorded so a rebuilt transcript carries it.
    Notice { kind: NoticeKind, text: String },
    /// Whether the round after a [`NoticeKind::NoChange`] notice asks the
    /// provider to require a tool call (`kgraf_provider::ToolChoice`), and
    /// why. Recorded once per notice, before that round's
    /// [`WorkerEvent::ModelCall`]. Built by the loop alone from the route's
    /// recorded support and the adapter; never model input (the rebuild's
    /// arm for it is a no-op).
    ToolChoice { forced: bool, detail: String },
    /// A message from the inbox reached this worker's transcript (decision
    /// 0067, point 3): joins the user turn the way [`WorkerEvent::Notice`]
    /// does. Refused by [`EventSequence`] mid-stream or mid-tool-round: an
    /// injection lands only between rounds (at-boundary mode), or after
    /// [`WorkerEvent::RoundAbandoned`] has closed the round (interrupt mode).
    Injected {
        id: String,
        source: InboxSource,
        kind: InboxKind,
        text: String,
    },
    /// An interrupt-mode steer abandoned the round in flight (decision 0067,
    /// point 3): its partial text is dropped and its usage, if any, is settled
    /// separately, the way a [`WorkerEvent::Cancelled`] round is. Not
    /// terminal: the worker keeps running, and an [`WorkerEvent::Injected`]
    /// follows.
    RoundAbandoned { phase: CancelPhase, reason: String },
    /// A running tool call is waiting on a shared resource before it can
    /// proceed (decision 0067), reported through
    /// [`kgraf_tools::ProgressSink`]. Purely informational: the rebuild's
    /// own arm for this is a no-op, so it never reaches the model (it is
    /// not part of the private transcript a request is built from), only
    /// the transcript file and the live view. `call_id` matches the
    /// `ToolCallStarted`/`ToolCallFinished` pair it falls between. Its text
    /// is kGraf-written (never a vendor's, never user input), so the
    /// model-facing scrub passes it through unexamined and the persist scrub
    /// still runs on it like any other event (decision 0071).
    Waiting { call_id: String, reason: String },
    /// The wait a [`WorkerEvent::Waiting`] reported for `call_id` has ended.
    Resumed { call_id: String },
    /// What the persist scrub redacted from the event just before this one
    /// (decision 0071): kind to count, never any of the
    /// text. Persist-only: the transcript builder ignores it, so it never
    /// reaches a model. Streamed deltas are not counted one by one: the
    /// round's [`WorkerEvent::AssistantMessage`] counts their text once,
    /// and a cancelled or abandoned round's deltas are counted just before
    /// its [`WorkerEvent::Cancelled`] or [`WorkerEvent::RoundAbandoned`].
    Redacted { counts: BTreeMap<String, u32> },
    /// The runtime compacted this worker's context (decision 0095). Built by
    /// the loop alone, between rounds; no model output becomes one. A worker
    /// that cannot hand back may compact more than once, each time on measured
    /// progress (`crate::compaction::recompaction`). Every event before it is
    /// the pre-compaction history, kept as written. The rebuild replaces the
    /// private remainder with the opening message plus the rendered notice and
    /// summary ([`crate::compaction::render_notice`]).
    Compacted {
        notice: Box<crate::compaction::CompactionNotice>,
        summary: Box<crate::compaction::CompactionSummary>,
        /// The tree and check scores at the compaction, which the progress
        /// since it is measured against (`crate::progress::SegmentGate`), so
        /// a resume measures the same segment. Never model input. Absent in a
        /// record from before it; such a worker is not compacted again.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        baseline: Option<Box<crate::progress::CheckpointBaseline>>,
    },
    /// A progress checkpoint (decision 0096): what the loop measured over the
    /// block since the previous one, its verdict, and the baseline the next
    /// one compares against. Built by the loop alone, between rounds, from the
    /// worktree and the worker's own tool results; no model output becomes
    /// one. Never model input: a resume reads it to restore the rounds the
    /// checkpoints granted.
    Checkpoint {
        checkpoint: Box<crate::progress::Checkpoint>,
        baseline: Box<crate::progress::CheckpointBaseline>,
    },
}

/// Why the loop added text to a user turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    /// Three rounds are left, counting the final answer round.
    StepsLeft,
    /// This is the final round; tool calls in it will not run.
    FinalRound,
    /// The last response reached the output ceiling and was discarded; this is
    /// the one continuation round, asking the model to split large patches.
    OutputLimit,
    /// A checkpoint measured no progress and granted one more block (decision
    /// 0096); the next checkpoint without progress ends the attempt.
    NoProgress,
    /// The last turn had no tool call and no visible answer (a model that
    /// planned only in its reasoning); it was discarded and the model was told
    /// to continue. A second empty turn in a row fails the attempt.
    EmptyTurn,
    /// A turn had reasoning and nothing else, and the route can turn reasoning
    /// off: the rest of the attempt's rounds run with reasoning off. Sent once
    /// an attempt.
    ReasoningOff,
    /// The route answered the worker's own reasoning-off request with
    /// reasoning anyway. The fallback is unavailable on this route for the
    /// rest of the run, and the attempt ends as a second empty turn.
    ReasoningOffIgnored,
    /// The model ended the attempt as done with no tool call in the whole
    /// attempt although its contract writes files; it was told once to
    /// continue or call `report_shortfall`. A second such end fails the attempt.
    NoChange,
    /// A turn ended with no tool call and its text held exactly one complete
    /// `*** Begin Patch ... *** End Patch` block; kGraf ran the block as an
    /// `apply_patch` call. Sent once for that turn, after the call's result.
    PatchAsText,
    /// The server could not parse the model's own tool-call output and
    /// answered with a 5xx; the round was dropped and sent again. Sent on each
    /// of the bounded retries.
    MalformedOutput,
    /// The provider counted far more output for the last reply than the reply
    /// carried, and the reply had no tool call: the server dropped part of it
    /// (usually a tool call it could not parse) and still answered as if the
    /// turn had ended. The round was dropped and the model was told what was
    /// missing. Sent on each of the bounded retries of an attempt.
    LostOutput,
    /// The worker was refused the same command program twice and did not call
    /// `report_shortfall`; kGraf filed the permission report for it. Sent once,
    /// when the report is filed.
    KgrafShortfall,
    /// What the orchestrator answered to the report kGraf filed. Counted
    /// apart from [`NoticeKind::KgrafShortfall`] so one filing is one count.
    KgrafShortfallAnswer,
    /// The context estimate reached the stop point before any usage report
    /// measured it, and the round ran anyway (the estimate overcounts by
    /// design); the text carries both numbers. Loop-only: never model input
    /// (the transcript builder skips it).
    EstimateOverStopPoint,
    /// The worker's deadline stopped it (`deadline.rs`); the text carries the
    /// time left, the last round and the expected round. Loop-only: never
    /// model input (a resumed worker runs under its own deadline).
    TimeLimit,
}

impl WorkerEvent {
    /// The model-facing scrub (decision 0071): only
    /// kGraf's own exact-secret registry, never pattern rules. Applied to
    /// what [`crate::worker::WorkerLoop::emit`] feeds the transcript builder
    /// (`TranscriptBuilder::apply`), which is what the *next* model request
    /// is built from. See `kgraf_secrets`'s crate docs for why pattern rules
    /// have no place on this path: a false positive here does not just leak a
    /// marker into a log, it corrupts the file content or command output a
    /// worker is actively reasoning over and may write back to disk.
    ///
    /// Every variant that can carry a tool's, a model's or a sender's text is
    /// scrubbed. The match is exhaustive on purpose: a new variant does not
    /// compile until someone decides how each path treats it.
    pub fn scrubbed_for_model(self) -> WorkerEvent {
        let mut none = kgraf_secrets::Redactions::default();
        self.scrub_with(&mut |t, _| scrub_model_owned(t), &mut none)
    }

    /// The persist/display scrub (decision 0071): the exact-secret registry
    /// *plus* pattern rules. Applied to what [`crate::worker::WorkerLoop::emit`]
    /// and [`crate::worker::WorkerLoop::emit_after_cancel`] send to the
    /// persisted `JsonlTranscript` and the shell, never to what feeds the
    /// transcript builder (see [`WorkerEvent::scrubbed_for_model`]).
    ///
    /// [`WorkerEvent::Approval`]'s decision text (a denial's reason, an ask's
    /// question, a rewrite's before/after JSON) and [`WorkerEvent::Notice`]
    /// are scrubbed here too, though neither is model input.
    pub fn scrubbed_for_persist(self) -> WorkerEvent {
        self.scrubbed_for_persist_counted().0
    }

    /// [`WorkerEvent::scrubbed_for_persist`], with what it redacted by kind
    /// (decision 0071).
    pub fn scrubbed_for_persist_counted(self) -> (WorkerEvent, kgraf_secrets::Redactions) {
        let mut counts = kgraf_secrets::Redactions::default();
        let event = self.scrub_with(
            &mut |t, counts| {
                let (out, found) = kgraf_secrets::scrub_persist_counted(&t);
                counts.merge(&found);
                match out {
                    std::borrow::Cow::Borrowed(_) => t,
                    std::borrow::Cow::Owned(scrubbed) => scrubbed,
                }
            },
            &mut counts,
        );
        (event, counts)
    }

    /// Apply `scrub` to every free-text field, by variant. `Notice`,
    /// `Approval` and `RoundAbandoned` go through it on both paths: on the
    /// model path the registry alone cannot corrupt them, and none of them is
    /// model input except `Notice`, which the loop writes itself.
    fn scrub_with(
        self,
        scrub: &mut Scrub<'_>,
        counts: &mut kgraf_secrets::Redactions,
    ) -> WorkerEvent {
        match self {
            WorkerEvent::TextDelta { text } => WorkerEvent::TextDelta {
                text: scrub(text, counts),
            },
            WorkerEvent::ReasoningDelta { text } => WorkerEvent::ReasoningDelta {
                text: scrub(text, counts),
            },
            WorkerEvent::AssistantMessage {
                text,
                reasoning,
                signature,
            } => WorkerEvent::AssistantMessage {
                text: scrub(text, counts),
                reasoning: scrub(reasoning, counts),
                signature,
            },
            WorkerEvent::ToolCallStarted {
                call_id,
                tool,
                input,
                signature,
            } => WorkerEvent::ToolCallStarted {
                call_id,
                tool,
                input: scrub_json(input, scrub, counts),
                signature,
            },
            WorkerEvent::ToolCallFinished {
                call_id,
                output,
                is_error,
                duration_ms,
            } => WorkerEvent::ToolCallFinished {
                call_id,
                output: scrub(output, counts),
                is_error,
                duration_ms,
            },
            WorkerEvent::Notice { kind, text } => WorkerEvent::Notice {
                kind,
                text: scrub(text, counts),
            },
            // kGraf-written: a route's support and a measurement record.
            // Scrubbed on both paths anyway, as a notice is.
            WorkerEvent::ToolChoice { forced, detail } => WorkerEvent::ToolChoice {
                forced,
                detail: scrub(detail, counts),
            },
            WorkerEvent::Approval { record } => WorkerEvent::Approval {
                record: scrub_approval_record(record, scrub, counts),
            },
            // A sender's message (decision 0067): model input and persisted.
            WorkerEvent::Injected {
                id,
                source,
                kind,
                text,
            } => WorkerEvent::Injected {
                id,
                source,
                kind,
                text: scrub(text, counts),
            },
            WorkerEvent::RoundAbandoned { phase, reason } => WorkerEvent::RoundAbandoned {
                phase,
                reason: scrub(reason, counts),
            },
            // `reason` is kGraf-written (a fixed string like "waiting for a
            // machine slot"), never a vendor's or a model's text, so the
            // model-facing scrub (the exact-secret registry alone) matches
            // nothing in it and this is effectively a pass-through on that
            // path; the persist path's pattern rules still get a chance at
            // it, for defense in depth (decision 0071).
            WorkerEvent::Waiting { call_id, reason } => WorkerEvent::Waiting {
                call_id,
                reason: scrub(reason, counts),
            },
            // No free text: `call_id` is an id, matched against
            // `ToolCallStarted`'s own (also never scrubbed).
            other @ WorkerEvent::Resumed { .. } => other,
            // The summary is model output: every text field is scrubbed. The
            // notice is the loop's own structured record (ids, counts, a
            // path the caller chose) and passes unchanged.
            WorkerEvent::Compacted {
                notice,
                summary,
                baseline,
            } => {
                let mut list = |items: Vec<String>, counts: &mut kgraf_secrets::Redactions| {
                    items.into_iter().map(|t| scrub(t, counts)).collect()
                };
                let decisions = list(summary.decisions, counts);
                let files_touched = list(summary.files_touched, counts);
                let open_items = list(summary.open_items, counts);
                let constraints = list(summary.constraints, counts);
                // Paths from tool inputs, which a model wrote.
                let files_read = summary
                    .files_read
                    .into_iter()
                    .map(|read| crate::compaction::FileRead {
                        path: scrub(read.path, counts),
                        entry: read.entry,
                    })
                    .collect();
                WorkerEvent::Compacted {
                    notice,
                    summary: Box::new(crate::compaction::CompactionSummary {
                        goal: scrub(summary.goal, counts),
                        decisions,
                        files_touched,
                        open_items,
                        constraints,
                        constraints_dropped: summary.constraints_dropped,
                        files_read,
                        files_read_unlisted: summary.files_read_unlisted,
                    }),
                    baseline,
                }
            }
            // The loop's own measurement. Its one text that came from a
            // model is a command's first two words, scrubbed when the loop
            // recorded it; the improvement lines quote that label.
            WorkerEvent::Checkpoint {
                mut checkpoint,
                baseline,
            } => {
                checkpoint.measure.checks_improved =
                    std::mem::take(&mut checkpoint.measure.checks_improved)
                        .into_iter()
                        .map(|t| scrub(t, counts))
                        .collect();
                WorkerEvent::Checkpoint {
                    checkpoint,
                    baseline,
                }
            }
            // Structured data only: ids, usage counts, a stop reason, a
            // phase, a digest, node ids, kind names from this crate's own
            // rules and registry, and the gitlink git wrote when the host
            // made the worktree (read before any model output could reach
            // it; a scrub that changed it would only stop the status read).
            other @ (WorkerEvent::Usage { .. }
            | WorkerEvent::Stop { .. }
            | WorkerEvent::Cancelled { .. }
            | WorkerEvent::Context { .. }
            | WorkerEvent::ModelCall { .. }
            | WorkerEvent::Redacted { .. }) => other,
        }
    }

    /// Whether this event ends the run on its own.
    ///
    /// `Stop { ToolUse }` does not: tools run next. Ending the run on it would be a
    /// turn that ends on a tool call, which produces no answer.
    /// `Stop { PauseTurn }` does not either: the provider paused mid-turn for a
    /// server-side tool it is still running, not for a tool call this build has
    /// to make, and the turn continues by sending the conversation back for
    /// another round. The loop no longer records one (it fails the run instead,
    /// since server tool blocks are not kept); the rule stays so an older
    /// transcript still reads the same way.
    pub fn is_terminal(&self) -> bool {
        match self {
            WorkerEvent::Cancelled { .. } => true,
            WorkerEvent::Stop { reason, .. } => {
                !matches!(reason, StopReason::ToolUse | StopReason::PauseTurn)
            }
            _ => false,
        }
    }

    /// Whether the event carries model or tool output whose volume the reader
    /// controls through backpressure. Everything else is control: small, bounded by
    /// the number of calls, and never delayed (see `WorkerEndpoint::emit`).
    pub fn is_bulk(&self) -> bool {
        matches!(
            self,
            WorkerEvent::TextDelta { .. }
                | WorkerEvent::ReasoningDelta { .. }
                | WorkerEvent::ToolCallFinished { .. }
                | WorkerEvent::AssistantMessage { .. }
        )
    }

    /// The neutral event for one provider stream event, for the ledger call `call`.
    ///
    /// `Started` has no neutral form (the model echo is asserted by the adapter), and
    /// tool-use deltas are not forwarded: tool calls are taken from the assembled
    /// response, whole, as `ToolCallStarted`.
    pub fn from_stream(event: StreamEvent, call: &CallId) -> Option<WorkerEvent> {
        match event {
            StreamEvent::Started { .. } => None,
            StreamEvent::Delta { content } => match content {
                Content::Text { text, .. } => Some(WorkerEvent::TextDelta { text }),
                Content::Reasoning { text } => Some(WorkerEvent::ReasoningDelta { text }),
                Content::ToolUse { .. } | Content::ToolResult { .. } => None,
            },
            StreamEvent::Usage { usage } => Some(WorkerEvent::Usage {
                call: call.clone(),
                usage,
            }),
            StreamEvent::Finished { stop_reason } => Some(WorkerEvent::Stop {
                finish_reason: None,
                reason: stop_reason,
            }),
        }
    }
}

/// `text` with every secret [`kgraf_secrets::scrub_model_facing`] finds
/// replaced by a typed marker (the exact-secret registry only), or `text`
/// unchanged when nothing matched (the common case, and the reason this
/// takes and returns an owned `String` rather than always allocating a new
/// one).
fn scrub_model_owned(text: String) -> String {
    match kgraf_secrets::scrub_model_facing(&text) {
        std::borrow::Cow::Borrowed(_) => text,
        std::borrow::Cow::Owned(scrubbed) => scrubbed,
    }
}

/// One text scrub, adding what it redacted to the counts.
type Scrub<'a> = dyn FnMut(String, &mut kgraf_secrets::Redactions) -> String + 'a;

/// `scrub`, recursively, over every string *value* in a tool call's JSON
/// input -- **never a key** (an object key is a field name a schema
/// chose, such as `"api_key"`; scrubbing it would rename the field a worker
/// or a later reader keys off, for no security benefit, since the key never
/// holds the secret) -- since a tool call's arguments are exactly as
/// free-form as its output: a worker can be told to `curl` a URL with a
/// credential embedded in it, and that credential is now in
/// `ToolCallStarted.input` before the call ever runs.
fn scrub_json(
    value: serde_json::Value,
    scrub: &mut Scrub<'_>,
    counts: &mut kgraf_secrets::Redactions,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(scrub(s, counts)),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .into_iter()
                .map(|v| scrub_json(v, scrub, counts))
                .collect(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, scrub_json(v, scrub, counts)))
                .collect(),
        ),
        other => other,
    }
}

/// An approval decision's text: a denial's or a rewrite's `reason`, an ask's
/// `question`, and a rewrite's `original`/`rewritten` JSON (the same walk as
/// a tool call's input, keys left alone -- see [`scrub_json`]).
fn scrub_approval_record(
    record: ApprovalRecord,
    scrub: &mut Scrub<'_>,
    counts: &mut kgraf_secrets::Redactions,
) -> ApprovalRecord {
    let decision = match record.decision {
        RecordedDecision::Asked {
            question,
            deadline_ms,
        } => RecordedDecision::Asked {
            question: scrub(question, counts),
            deadline_ms,
        },
        RecordedDecision::Denied { reason } => RecordedDecision::Denied {
            reason: scrub(reason, counts),
        },
        RecordedDecision::Rewritten {
            original,
            rewritten,
            reason,
        } => RecordedDecision::Rewritten {
            original: scrub_json(original, scrub, counts),
            rewritten: scrub_json(rewritten, scrub, counts),
            reason: scrub(reason, counts),
        },
        RecordedDecision::UserDenied { reason } => RecordedDecision::UserDenied {
            reason: scrub(reason, counts),
        },
        other @ (RecordedDecision::Allowed
        | RecordedDecision::AllowedNoHooks
        | RecordedDecision::UserAllowed
        | RecordedDecision::AskExpired) => other,
    };
    ApprovalRecord { decision, ..record }
}

/// An event out of order.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SequenceError {
    #[error("an event arrived after the run ended")]
    AfterTerminal,
    #[error("tool call `{call_id}` started twice")]
    DuplicateStart { call_id: String },
    #[error("tool call `{call_id}` finished without starting")]
    FinishedWithoutStart { call_id: String },
    #[error("the run ended with tool calls still open: {open:?}")]
    EndedWithOpenCalls { open: Vec<String> },
    /// An injection or a compaction arrived mid-stream or mid-tool-round
    /// (decisions 0067, 0095): it must wait for the boundary, or for a
    /// `RoundAbandoned` that closes the round first (interrupt mode).
    #[error("an injection or a compaction arrived mid-round, not at a tool-batch boundary")]
    NotBetweenRounds,
}

/// Checks a stream of events against the order above.
///
/// Used by the loop to refuse emitting an impossible sequence, and on resume to
/// validate a loaded transcript before rebuilding from it.
#[derive(Clone, Debug, Default)]
pub struct EventSequence {
    /// Open tool calls: call id to tool name.
    open: BTreeMap<String, String>,
    terminal: bool,
    cancelled: bool,
    /// A model round has started (a delta seen) and not yet reached `Stop`.
    /// Tracked so [`WorkerEvent::Injected`] can be refused mid-stream, not
    /// only mid-tool-round (`open`).
    streaming: bool,
}

impl EventSequence {
    /// A fresh run.
    pub fn new() -> Self {
        Self::default()
    }

    /// Accept one event, or say why it is out of order. A refused event leaves the
    /// state unchanged.
    pub fn accept(&mut self, event: &WorkerEvent) -> Result<(), SequenceError> {
        if self.terminal {
            // A cancelled call's final usage still has to be counted (every token is attributed).
            if self.cancelled && matches!(event, WorkerEvent::Usage { .. }) {
                return Ok(());
            }
            return Err(SequenceError::AfterTerminal);
        }
        match event {
            WorkerEvent::TextDelta { .. } | WorkerEvent::ReasoningDelta { .. } => {
                self.streaming = true;
            }
            WorkerEvent::ToolCallStarted { call_id, tool, .. } => {
                if self.open.contains_key(call_id) {
                    return Err(SequenceError::DuplicateStart {
                        call_id: call_id.clone(),
                    });
                }
                self.open.insert(call_id.clone(), tool.clone());
            }
            WorkerEvent::ToolCallFinished { call_id, .. } => {
                self.open
                    .remove(call_id)
                    .ok_or_else(|| SequenceError::FinishedWithoutStart {
                        call_id: call_id.clone(),
                    })?;
            }
            WorkerEvent::Stop { reason, .. }
                if !matches!(reason, StopReason::ToolUse | StopReason::PauseTurn) =>
            {
                if !self.open.is_empty() {
                    return Err(SequenceError::EndedWithOpenCalls {
                        open: self.open.keys().cloned().collect(),
                    });
                }
                self.streaming = false;
                self.terminal = true;
            }
            WorkerEvent::Stop { .. } => {
                // A tool-use stop ends the model round; the tool round that
                // follows is tracked through `open`, not `streaming`. A
                // pause-turn stop ends the model round the same way, with no
                // tool round to track: the next round follows directly.
                self.streaming = false;
            }
            WorkerEvent::Cancelled { .. } => {
                self.streaming = false;
                self.terminal = true;
                self.cancelled = true;
            }
            WorkerEvent::Injected { .. } if self.streaming || !self.open.is_empty() => {
                return Err(SequenceError::NotBetweenRounds);
            }
            WorkerEvent::Injected { .. } => {}
            // Decision 0095: compaction replaces the private remainder, so it
            // lands only between rounds, like an injection.
            WorkerEvent::Compacted { .. } if self.streaming || !self.open.is_empty() => {
                return Err(SequenceError::NotBetweenRounds);
            }
            // Decision 0096: a checkpoint is measured between rounds.
            WorkerEvent::Checkpoint { .. } if self.streaming || !self.open.is_empty() => {
                return Err(SequenceError::NotBetweenRounds);
            }
            // Decided for the next request, so between rounds.
            WorkerEvent::ToolChoice { .. } if self.streaming || !self.open.is_empty() => {
                return Err(SequenceError::NotBetweenRounds);
            }
            WorkerEvent::RoundAbandoned { .. } => {
                // Closes the round in flight, the way `Cancelled` does
                // (`open` cleared, so the calls it held are exactly what the
                // caller must read as interrupted before this call, the way
                // `ResumeState::from_events` already does for `Cancelled`),
                // but is not terminal: the worker keeps running.
                self.streaming = false;
                self.open.clear();
            }
            _ => {}
        }
        Ok(())
    }

    /// Whether the run has ended.
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Tool calls started and not finished: (call id, tool). After a cancel these are
    /// the calls a resume must treat as interrupted.
    pub fn open_tool_calls(&self) -> impl Iterator<Item = (&str, &str)> {
        self.open.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn started(id: &str) -> WorkerEvent {
        WorkerEvent::ToolCallStarted {
            call_id: id.into(),
            tool: "edit".into(),
            input: json!({}),
            signature: None,
        }
    }

    fn finished(id: &str) -> WorkerEvent {
        WorkerEvent::ToolCallFinished {
            call_id: id.into(),
            output: "ok".into(),
            is_error: false,
            duration_ms: None,
        }
    }

    fn stop(reason: StopReason) -> WorkerEvent {
        WorkerEvent::Stop {
            finish_reason: None,
            reason,
        }
    }

    #[test]
    fn a_full_run_is_accepted() {
        let mut seq = EventSequence::new();
        let run = [
            WorkerEvent::TextDelta {
                text: "I will".into(),
            },
            WorkerEvent::Usage {
                call: CallId::new("c1"),
                usage: TokenUsage::default(),
            },
            stop(StopReason::ToolUse),
            started("t1"),
            started("t2"),
            finished("t2"),
            finished("t1"),
            WorkerEvent::TextDelta {
                text: "done".into(),
            },
            stop(StopReason::EndTurn),
        ];
        for event in &run {
            seq.accept(event).expect("in order");
        }
        assert!(seq.is_terminal());
    }

    #[test]
    fn a_paused_turn_is_not_terminal_and_another_round_follows_directly() {
        // Unlike `Stop { ToolUse }`, a paused turn has no tool round: the next
        // model round follows at once, with no `ToolCallStarted` in between.
        let mut seq = EventSequence::new();
        let run = [
            WorkerEvent::TextDelta {
                text: "still searching".into(),
            },
            stop(StopReason::PauseTurn),
            WorkerEvent::TextDelta {
                text: "found it".into(),
            },
            stop(StopReason::EndTurn),
        ];
        for event in &run {
            seq.accept(event).expect("in order");
        }
        assert!(seq.is_terminal());
        assert!(!stop(StopReason::PauseTurn).is_terminal());
    }

    #[test]
    fn nothing_is_accepted_after_the_end() {
        let mut seq = EventSequence::new();
        seq.accept(&stop(StopReason::EndTurn)).expect("stop");
        assert_eq!(
            seq.accept(&WorkerEvent::TextDelta { text: "x".into() }),
            Err(SequenceError::AfterTerminal)
        );
    }

    #[test]
    fn a_tool_use_stop_does_not_end_the_run() {
        let mut seq = EventSequence::new();
        seq.accept(&stop(StopReason::ToolUse)).expect("stop");
        assert!(!seq.is_terminal());
        assert!(!stop(StopReason::ToolUse).is_terminal());
        assert!(stop(StopReason::MaxTokens).is_terminal());
    }

    #[test]
    fn tool_call_pairing_is_enforced() {
        let mut seq = EventSequence::new();
        assert!(matches!(
            seq.accept(&finished("t1")),
            Err(SequenceError::FinishedWithoutStart { .. })
        ));
        seq.accept(&started("t1")).expect("start");
        assert!(matches!(
            seq.accept(&started("t1")),
            Err(SequenceError::DuplicateStart { .. })
        ));
        assert!(matches!(
            seq.accept(&stop(StopReason::EndTurn)),
            Err(SequenceError::EndedWithOpenCalls { .. })
        ));
        assert!(!seq.is_terminal(), "a refused event leaves state unchanged");
    }

    #[test]
    fn usage_after_a_cancel_is_accepted_and_nothing_else_is() {
        let mut seq = EventSequence::new();
        seq.accept(&WorkerEvent::TextDelta { text: "par".into() })
            .expect("delta");
        seq.accept(&WorkerEvent::Cancelled {
            phase: CancelPhase::ModelStream,
        })
        .expect("cancel");
        seq.accept(&WorkerEvent::Usage {
            call: CallId::new("c1"),
            usage: TokenUsage::default(),
        })
        .expect("the partial usage must be recordable");
        assert_eq!(
            seq.accept(&WorkerEvent::TextDelta { text: "x".into() }),
            Err(SequenceError::AfterTerminal)
        );
        // After a normal end, a late usage is still out of order.
        let mut ended = EventSequence::new();
        ended.accept(&stop(StopReason::EndTurn)).expect("stop");
        assert!(
            ended
                .accept(&WorkerEvent::Usage {
                    call: CallId::new("c1"),
                    usage: TokenUsage::default(),
                })
                .is_err()
        );
    }

    #[test]
    fn stream_events_map_to_neutral_events() {
        let call = CallId::new("c9");
        assert_eq!(
            WorkerEvent::from_stream(
                StreamEvent::Delta {
                    content: Content::text("hi")
                },
                &call
            ),
            Some(WorkerEvent::TextDelta { text: "hi".into() })
        );
        assert_eq!(
            WorkerEvent::from_stream(
                StreamEvent::Usage {
                    usage: TokenUsage::default()
                },
                &call
            ),
            Some(WorkerEvent::Usage {
                call: call.clone(),
                usage: TokenUsage::default()
            })
        );
        assert_eq!(
            WorkerEvent::from_stream(
                StreamEvent::Started {
                    model: "m".into(),
                    request_id: None
                },
                &call
            ),
            None
        );
    }

    #[test]
    fn a_cancel_mid_tool_leaves_the_open_calls_visible() {
        let mut seq = EventSequence::new();
        seq.accept(&started("t1")).expect("start");
        seq.accept(&WorkerEvent::Cancelled {
            phase: CancelPhase::ToolCall,
        })
        .expect("cancel is always accepted");
        assert!(seq.is_terminal());
        assert_eq!(
            seq.open_tool_calls().collect::<Vec<_>>(),
            vec![("t1", "edit")]
        );
    }

    fn injected(text: &str) -> WorkerEvent {
        WorkerEvent::Injected {
            id: "msg-1".into(),
            source: InboxSource::User,
            kind: InboxKind::Note,
            text: text.into(),
        }
    }

    #[test]
    fn injected_is_refused_inside_an_open_tool_round_and_mid_stream() {
        // Mid-stream: a delta has started the round and `Stop` has not landed.
        let mut seq = EventSequence::new();
        seq.accept(&WorkerEvent::TextDelta {
            text: "partial".into(),
        })
        .expect("delta");
        assert_eq!(
            seq.accept(&injected("hi")),
            Err(SequenceError::NotBetweenRounds)
        );

        // Mid-tool-round: a tool call started and has not finished.
        let mut seq = EventSequence::new();
        seq.accept(&stop(StopReason::ToolUse)).expect("stop");
        seq.accept(&started("t1")).expect("start");
        assert_eq!(
            seq.accept(&injected("hi")),
            Err(SequenceError::NotBetweenRounds)
        );

        // Between rounds (after the tool results, before the next model
        // call): accepted.
        let mut seq = EventSequence::new();
        seq.accept(&stop(StopReason::ToolUse)).expect("stop");
        seq.accept(&started("t1")).expect("start");
        seq.accept(&finished("t1")).expect("finish");
        seq.accept(&injected("hi")).expect("between rounds");
    }

    #[test]
    fn round_abandoned_is_not_terminal() {
        let mut seq = EventSequence::new();
        seq.accept(&WorkerEvent::TextDelta {
            text: "partial".into(),
        })
        .expect("delta");
        seq.accept(&WorkerEvent::RoundAbandoned {
            phase: CancelPhase::ModelStream,
            reason: "the user redirected the worker".into(),
        })
        .expect("round abandoned");
        assert!(!seq.is_terminal());
        seq.accept(&injected("hi")).expect("injected after abandon");
    }

    #[test]
    fn round_abandoned_mid_tool_closes_the_open_calls_so_injected_is_accepted_next() {
        let mut seq = EventSequence::new();
        seq.accept(&stop(StopReason::ToolUse)).expect("stop");
        seq.accept(&started("t1")).expect("start");
        // Without closing `open`, `Injected` right after would be refused as
        // mid-tool-round.
        seq.accept(&WorkerEvent::RoundAbandoned {
            phase: CancelPhase::ToolCall,
            reason: "the user redirected the worker".into(),
        })
        .expect("round abandoned mid-tool");
        assert!(!seq.is_terminal());
        assert_eq!(seq.open_tool_calls().collect::<Vec<_>>(), Vec::new());
        seq.accept(&injected("hi"))
            .expect("injected after a mid-tool abandon");
    }

    #[test]
    fn events_round_trip_through_json_with_a_type_tag() {
        let event = WorkerEvent::Cancelled {
            phase: CancelPhase::ModelStream,
        };
        let text = serde_json::to_string(&event).expect("ser");
        assert_eq!(text, r#"{"type":"cancelled","phase":"model_stream"}"#);
        let back: WorkerEvent = serde_json::from_str(&text).expect("de");
        assert_eq!(back, event);
    }
}
