//! Rebuild the model-facing transcript from neutral events.
//!
//! The transcript is rebuilt from a neutral event sequence. The
//! loop itself builds its messages through [`TranscriptBuilder`] from the events it
//! emits, so a resume, which rebuilds from the stored events, sees exactly the
//! messages the loop would have sent. There is one construction, not two that could
//! drift.
//!
//! **Rules.**
//!
//! - Text and reasoning deltas accumulate; `Stop` turns them into one assistant
//!   message. `AssistantMessage`, emitted once right before `Stop` (decision 0071),
//!   replaces whatever accumulated from deltas with
//!   the round's settled, correctly-whole-scrubbed text -- see
//!   `TranscriptBuilder::apply`. `ToolCallStarted` adds its tool use to that
//!   message.
//! - `ToolCallFinished` results are held until the next round begins (the next
//!   `Stop`, `Notice`, `Cancelled`, or a read of the messages), then written as one
//!   user message in the order the calls started.
//! - `Notice` text joins the user turn that precedes the next model call, on
//!   the same condition as `Injected` below.
//! - `Injected` text (decision 0067) joins the last user turn **only if no
//!   request has carried that turn yet**; otherwise it starts a new user turn.
//!   After a tool round that is the turn of `tool_result` blocks, so the note
//!   follows the results (and any `Notice`); before the first model call it is
//!   the opening message. After an interrupt abandoned a model stream, the last
//!   user turn was already sent, so the note gets its own turn: nothing a
//!   request carried is ever rewritten, and the opening instruction is never
//!   extended once the first call has left. `ModelCall` marks the sent point.
//! - `Cancelled` drops the unfinished round's partial text (it is not a complete
//!   assistant turn), and every started call without a result gets an explicit
//!   interrupted result, so the rebuilt transcript pairs every tool use with a
//!   result and says which ones never finished.
//! - `Usage` counts rounds (one ledger call per round, by call id) and anchors the
//!   context-size estimate. A report of zero (a local model, or a call that never
//!   left) anchors nothing; a round no report covered counts its own output.
//! - `Compacted` (decision 0095) replaces the private remainder with the
//!   opening messages (instruction and slices, verbatim) followed by the
//!   rendered notice and summary, and resets the context estimate to them. The
//!   events before it stay in the transcript; only the messages change.
//! - A rebuild from stored events adds a note to a tool result whose text or call
//!   input holds a `[redacted:...]` marker: the pattern scrub that stored it may
//!   have replaced text the live model saw whole, so the model re-reads before it
//!   edits from that result. Each round's calls are kept as the live loop saw
//!   them (after any approval rewrite) for replaying stall counts and writes.

use crate::event::{EventSequence, SequenceError, WorkerEvent};
use kgraf_core::budget::estimate_tokens;
use kgraf_core::{CallId, NodeId};
use kgraf_provider::{Content, Message, Role, StopReason};
use std::collections::{BTreeSet, HashMap};

/// The note added to a stored tool result that held a redaction marker, or
/// whose call input did.
pub const REDACTED_HISTORY_NOTE: &str = "\n[note: this transcript was read back from storage, \
     where text that looked like a secret is replaced by a `[redacted:...]` marker. Re-read \
     the file or rerun the command before editing from this result, and never copy a marker \
     into a file or a patch.]";

/// Whether `text` holds a redaction marker.
pub(crate) fn has_redaction_marker(text: &str) -> bool {
    text.contains("[redacted:")
}

/// One tool call of a stored round, as the live loop saw it.
#[derive(Clone, Debug)]
pub struct ReplayCall {
    pub id: String,
    pub tool: String,
    /// The input the call ran with: after any approval rewrite.
    pub input: serde_json::Value,
    /// Whether the live loop counted it toward a stall: every call that was not
    /// denied or refused before it ran.
    pub counted: bool,
    /// Whether it finished without error.
    pub succeeded: bool,
    /// The result it finished with (empty until it finished).
    pub output: String,
    /// Whether that result was an error.
    pub is_error: bool,
}

impl ReplayCall {
    /// Whether an interrupt-mode message cut the call off while it ran: it
    /// started and may have written, and the loop treated its declared writes
    /// as changes.
    pub fn interrupted_by_steer(&self) -> bool {
        self.is_error && self.output.starts_with(crate::worker::STEER_INTERRUPTED)
    }
}

/// The result recorded for a call that started and never finished.
pub const INTERRUPTED_RESULT: &str =
    "ERROR: interrupted: the worker was cancelled before this call returned a result";

#[derive(Clone, Debug, Default)]
struct PendingResults {
    order: Vec<String>,
    results: HashMap<String, (String, bool)>,
}

/// Bytes per token above which a deduction of unreported reasoning would be a
/// guess rather than a bound; see [`TranscriptBuilder::context_tokens`].
const REASONING_BYTES_PER_TOKEN_CEILING: u64 = 5;

/// A usage report reduced to what sizes the next prompt.
#[derive(Clone, Copy, Debug)]
struct Anchor {
    input: u64,
    text_output: u64,
    /// Reasoning tokens the provider reported apart from text; zero when it
    /// reports none or does not split them.
    reasoning_reported: u64,
    /// Bytes of reasoning text of the round the report covers.
    reasoning_bytes: usize,
}

/// Builds the messages sent to the model from the events of a run.
#[derive(Clone, Debug)]
pub struct TranscriptBuilder {
    messages: Vec<Message>,
    text: String,
    reasoning: String,
    /// The token on the round's last text part, from its `AssistantMessage`.
    text_signature: Option<kgraf_provider::Signature>,
    pending: Option<PendingResults>,
    last_call: Option<CallId>,
    rounds: u32,
    /// The last usage report that anchored the estimate, as reported.
    anchor: Option<Anchor>,
    tokens_since_usage: u64,
    /// Estimated reasoning tokens of rounds no usage report covered, apart
    /// from `tokens_since_usage` so the estimate counts them only when the
    /// route resends reasoning (`reasoning_resent`).
    since_usage_reasoning: u64,
    /// Whether the next request carries the reasoning this transcript holds.
    /// `true` until the loop says otherwise: counting what is not sent only
    /// raises the estimate, never lowers it.
    reasoning_resent: bool,
    /// Bytes of reasoning text the latest `Stop` ended, reset at each call.
    stop_reasoning_bytes: usize,
    tool_calls: Vec<(String, serde_json::Value)>,
    started: HashMap<String, (String, serde_json::Value)>,
    succeeded: Vec<(String, serde_json::Value)>,
    /// The tool calls of each round, as the live loop saw them: the input after
    /// any approval rewrite, and whether the call was approved and succeeded.
    replay: Vec<Vec<ReplayCall>>,
    /// Whether the last `ToolCallStarted` belongs to the group being filled.
    group_open: bool,
    /// Whether the events being applied were read back from storage, where
    /// text passed the pattern scrub that the live model copy does not.
    from_storage: bool,
    /// Calls (from storage) whose input carried a redaction marker.
    marked_inputs: BTreeSet<String>,
    /// Whether a usage report above zero anchored the round in progress.
    round_anchored: bool,
    calls: Vec<CallId>,
    prefix_digest: Option<String>,
    slices: Option<Vec<NodeId>>,
    /// How many leading messages some request has already carried: set at
    /// each `ModelCall`. An injection never joins a message below this index.
    sent: usize,
    /// The opening messages, kept for a compaction to rebuild from (0095).
    opening: Vec<Message>,
    /// The estimate of what precedes the messages (the shared prefix), for
    /// the context estimate after a compaction.
    base_tokens: u64,
    /// The compaction this transcript records, if any (at most one).
    compaction: Option<crate::compaction::CompactionNotice>,
}

impl TranscriptBuilder {
    /// Start from the run's opening messages (the pinned instruction and slices).
    pub fn new(opening: Vec<Message>) -> Self {
        let tokens_since_usage = opening.iter().map(message_tokens).sum();
        Self {
            opening: opening.clone(),
            base_tokens: 0,
            compaction: None,
            messages: opening,
            text: String::new(),
            reasoning: String::new(),
            text_signature: None,
            pending: None,
            last_call: None,
            rounds: 0,
            anchor: None,
            tokens_since_usage,
            since_usage_reasoning: 0,
            reasoning_resent: true,
            stop_reasoning_bytes: 0,
            tool_calls: Vec::new(),
            started: HashMap::new(),
            succeeded: Vec::new(),
            replay: Vec::new(),
            group_open: false,
            from_storage: false,
            marked_inputs: BTreeSet::new(),
            round_anchored: false,
            calls: Vec::new(),
            prefix_digest: None,
            slices: None,
            sent: 0,
        }
    }

    /// Add an estimate of what precedes the messages (the shared prefix) to the
    /// context estimate, until the first usage report replaces it.
    pub fn with_base_tokens(mut self, tokens: u64) -> Self {
        self.tokens_since_usage += tokens;
        self.base_tokens = tokens;
        self
    }

    /// Say whether the route resends the model's reasoning in the next
    /// request (`Provider::resends_reasoning`). When it does not, reasoning
    /// output is not part of the next prompt and the estimate leaves it out.
    /// Takes effect on the next read, so a transcript replayed from storage
    /// can be told after the replay.
    pub fn with_reasoning_resent(mut self, resent: bool) -> Self {
        self.reasoning_resent = resent;
        self
    }

    /// The compaction this transcript records, if any.
    pub fn compaction(&self) -> Option<&crate::compaction::CompactionNotice> {
        self.compaction.as_ref()
    }

    /// Apply one event.
    pub fn apply(&mut self, event: &WorkerEvent) {
        match event {
            WorkerEvent::TextDelta { text } => self.text.push_str(text),
            WorkerEvent::ReasoningDelta { text } => self.reasoning.push_str(text),
            // Decision 0071: the round's settled, whole
            // text/reasoning, from the assembled response rather than
            // concatenated deltas, always emitted right before `Stop`.
            // *Replaces* whatever deltas accumulated so far this round rather
            // than appending: on a live run these are the same content
            // (`Response::text()`/`reasoning()` is exactly what the deltas
            // that produced it concatenate to, model-facing-scrubbed either
            // way); on resume, replaying the *persisted* (pattern-scrubbed)
            // events, this is the authoritative, correctly-whole-scrubbed
            // text, where the individual persisted deltas might each carry an
            // un-redacted half of a secret that only matches a pattern rule
            // once whole.
            WorkerEvent::AssistantMessage {
                text,
                reasoning,
                signature,
            } => {
                self.text = text.clone();
                self.reasoning = reasoning.clone();
                self.text_signature = signature.clone();
            }
            WorkerEvent::ModelCall { call } => {
                self.count_round(call);
                self.round_anchored = false;
                self.group_open = false;
                self.stop_reasoning_bytes = 0;
                // The loop reads `messages()` (which flushes) before it opens
                // the call, so flushing here changes nothing the loop sent; it
                // makes a rebuild from stored events agree on where the sent
                // messages end.
                self.flush_results();
                self.sent = self.messages.len();
            }
            WorkerEvent::Usage { call, usage } => {
                self.count_round(call);
                let total = usage.total_input() + usage.total_output();
                // A zero report is a round that never reached the provider (a
                // call cancelled before it was sent settles at zero), or a
                // provider that reports no usage (a local model). Either way it
                // says nothing about the context size, so it does not replace
                // the anchor: it would collapse the estimate to nothing and let
                // an oversized request past the stop point. A round no report
                // anchored counts its own output instead (see `Stop`).
                if total > 0 {
                    self.anchor = Some(Anchor {
                        input: usage.total_input(),
                        text_output: usage.text_output,
                        reasoning_reported: usage.reasoning_output,
                        // The usage report may come before or after the
                        // round's `Stop`: the reasoning is in one of the two.
                        reasoning_bytes: self.reasoning.len().max(self.stop_reasoning_bytes),
                    });
                    self.tokens_since_usage = 0;
                    self.since_usage_reasoning = 0;
                    self.round_anchored = true;
                }
            }
            WorkerEvent::Stop { reason, .. } => {
                self.flush_results();
                self.group_open = false;
                if self.round_anchored {
                    if let Some(anchor) = self.anchor.as_mut() {
                        anchor.reasoning_bytes = self.reasoning.len();
                    }
                } else {
                    // No usage report covered this round, so its output is in
                    // no anchor: estimate it.
                    self.tokens_since_usage += estimate_tokens(&self.text);
                    self.since_usage_reasoning += estimate_tokens(&self.reasoning);
                }
                self.stop_reasoning_bytes = self.reasoning.len();
                let mut content = Vec::new();
                if !self.reasoning.is_empty() {
                    content.push(Content::Reasoning {
                        text: std::mem::take(&mut self.reasoning),
                    });
                }
                // A signed part is kept even when its text is empty: the token
                // is what the part carries.
                let signature = self.text_signature.take();
                if !self.text.is_empty() || signature.is_some() {
                    content.push(Content::Text {
                        text: std::mem::take(&mut self.text),
                        cache_breakpoint: false,
                        signature,
                    });
                }
                if *reason == StopReason::ToolUse {
                    self.pending = Some(PendingResults::default());
                }
                self.messages.push(Message {
                    role: Role::Assistant,
                    content,
                });
            }
            WorkerEvent::ToolCallStarted {
                call_id,
                tool,
                input,
                signature,
            } => {
                self.tool_calls.push((tool.clone(), input.clone()));
                if !self.round_anchored {
                    self.tokens_since_usage +=
                        estimate_tokens(tool) + estimate_tokens(&input.to_string());
                }
                if self.from_storage && has_redaction_marker(&input.to_string()) {
                    self.marked_inputs.insert(call_id.clone());
                }
                if !self.group_open {
                    self.replay.push(Vec::new());
                    self.group_open = true;
                }
                if let Some(group) = self.replay.last_mut() {
                    group.push(ReplayCall {
                        id: call_id.clone(),
                        tool: tool.clone(),
                        input: input.clone(),
                        counted: true,
                        succeeded: false,
                        output: String::new(),
                        is_error: false,
                    });
                }
                self.started
                    .insert(call_id.clone(), (tool.clone(), input.clone()));
                if let Some(last) = self.messages.last_mut()
                    && last.role == Role::Assistant
                {
                    last.content.push(Content::ToolUse {
                        id: call_id.clone(),
                        name: tool.clone(),
                        input: input.clone(),
                        signature: signature.clone(),
                    });
                }
                self.pending
                    .get_or_insert_with(PendingResults::default)
                    .order
                    .push(call_id.clone());
            }
            WorkerEvent::ToolCallFinished {
                call_id,
                output,
                is_error,
            } => {
                self.tokens_since_usage += estimate_tokens(output);
                if let Some(call) = self.started.remove(call_id)
                    && !*is_error
                {
                    self.succeeded.push(call);
                }
                if let Some(call) = self
                    .replay
                    .iter_mut()
                    .rev()
                    .flat_map(|group| group.iter_mut())
                    .find(|call| call.id == *call_id)
                {
                    call.succeeded = !*is_error;
                    call.is_error = *is_error;
                    call.output = output.clone();
                    // A call the approval chain or the write scope turned away
                    // (denied, refused, or invalid input) did nothing, and a call the final round refused never
                    // reached the stall check; the live loop counted neither.
                    // Only an error result can be one of these: a successful
                    // result that happens to start with the same words is not.
                    call.counted = !(*is_error
                        && (output.starts_with(crate::worker::DENIED_PREFIX)
                            || output.starts_with(crate::worker::REFUSED_PREFIX)
                            || output.starts_with(crate::worker::INVALID_CALL_PREFIX)
                            || *output == crate::worker::final_round_refusal()));
                }
                // A result read back from storage may hold a span the pattern
                // scrub redacted that the live model saw whole, and the call's
                // own input may too. Say so where the model reads the result:
                // a marker must not be copied into a file or a patch.
                let mut output = output.clone();
                if self.from_storage
                    && (has_redaction_marker(&output) || self.marked_inputs.contains(call_id))
                {
                    output.push_str(REDACTED_HISTORY_NOTE);
                }
                self.pending
                    .get_or_insert_with(PendingResults::default)
                    .results
                    .insert(call_id.clone(), (output, *is_error));
            }
            WorkerEvent::Notice { text, .. } => {
                // The same append-only rule as `Injected`. After an
                // abandoned (or, on resume, cancelled) round, the last user
                // turn was already sent; a notice then starts its own turn
                // instead of rewriting it.
                self.flush_results();
                self.tokens_since_usage += estimate_tokens(text);
                let unsent = self.messages.len() > self.sent;
                match self.messages.last_mut() {
                    Some(last) if last.role == Role::User && unsent => {
                        last.content.push(Content::text(text.clone()))
                    }
                    _ => self.messages.push(Message::user(text.clone())),
                }
            }
            WorkerEvent::Cancelled { .. } => {
                self.group_open = false;
                self.text.clear();
                self.reasoning.clear();
                self.text_signature = None;
                self.flush_results();
            }
            WorkerEvent::Injected { text, .. } => {
                // Decision 0067: append only. Join the last user turn only
                // while no request has carried it (see the module docs).
                self.flush_results();
                self.tokens_since_usage += estimate_tokens(text);
                let unsent = self.messages.len() > self.sent;
                match self.messages.last_mut() {
                    Some(last) if last.role == Role::User && unsent => {
                        last.content.push(Content::text(text.clone()))
                    }
                    _ => self.messages.push(Message::user(text.clone())),
                }
            }
            WorkerEvent::RoundAbandoned { reason, .. } => {
                // A round abandoned before its request left the process bought
                // no model turn: the loop refunded it from the step budget, so
                // a rebuild does not count it either. Its ledger call stays
                // listed in `calls`, and a later report for the same call does
                // not count it again (`count_round`).
                if reason == crate::worker::STEER_ABANDON_BEFORE_SEND_REASON
                    || reason == crate::worker::CANCEL_BEFORE_SEND_REASON
                {
                    self.rounds = self.rounds.saturating_sub(1);
                }
                // Clears the partial turn the way `Cancelled` does, but is not
                // terminal: the worker keeps running past it.
                self.group_open = false;
                self.text.clear();
                self.reasoning.clear();
                self.text_signature = None;
                self.flush_results();
            }
            WorkerEvent::Context {
                prefix_digest,
                slices,
                ..
            } => {
                self.prefix_digest = Some(prefix_digest.clone());
                self.slices = Some(slices.clone());
            }
            WorkerEvent::Approval { record } => {
                // A rewrite replaces the arguments the call runs with. The
                // model's own turn keeps the original; the replay of what the
                // call did follows the rewritten input.
                if let kgraf_tools::approval::RecordedDecision::Rewritten { rewritten, .. } =
                    &record.decision
                {
                    if let Some((_, input)) = self.started.get_mut(&record.call_id) {
                        *input = rewritten.clone();
                    }
                    if self.from_storage && has_redaction_marker(&rewritten.to_string()) {
                        self.marked_inputs.insert(record.call_id.clone());
                    }
                    if let Some(call) = self
                        .replay
                        .iter_mut()
                        .rev()
                        .flat_map(|group| group.iter_mut())
                        .find(|call| call.id == record.call_id)
                    {
                        call.input = rewritten.clone();
                    }
                }
            }
            // Purely informational (decision 0067):
            // never joins a message, never touches what the model sees. The
            // transcript file and the live view are the only readers
            // (`transcripts.rs`'s own arm for these).
            WorkerEvent::Waiting { .. } | WorkerEvent::Resumed { .. } => {}
            // Persist-only redaction counts (decision 0071): never model input.
            WorkerEvent::Redacted { .. } => {}
            // Decision 0096: the loop's measurement, never model input. The
            // worker reads the checkpoints from the stored events on resume
            // (`ProgressGate::resume`); the grace notice, when there is one,
            // is its own `Notice` event.
            WorkerEvent::Checkpoint { .. } => {}
            WorkerEvent::Compacted { notice, summary } => {
                // Results held for the next turn belong to the history the
                // summary replaced; the loop compacts only between rounds,
                // so none is pending there.
                self.pending = None;
                self.group_open = false;
                self.text.clear();
                self.reasoning.clear();
                self.text_signature = None;
                let note = crate::compaction::render_notice(notice, summary);
                let mut messages = self.opening.clone();
                match messages.last_mut() {
                    Some(last) if last.role == Role::User => {
                        last.content.push(Content::text(note.clone()))
                    }
                    _ => messages.push(Message::user(note.clone())),
                }
                // A fresh remainder: nothing of it was sent, and the last
                // usage report described the context it replaced.
                self.tokens_since_usage =
                    self.base_tokens + messages.iter().map(message_tokens).sum::<u64>();
                self.messages = messages;
                self.sent = 0;
                self.anchor = None;
                self.since_usage_reasoning = 0;
                self.round_anchored = false;
                // The summary call is a ledger call of this worker, not a round.
                if let Some(call) = &notice.summary_call
                    && !self.calls.contains(call)
                {
                    self.calls.push(call.clone());
                }
                self.compaction = Some((**notice).clone());
            }
        }
    }

    /// Count a model round once per ledger call, whichever event names it first.
    fn count_round(&mut self, call: &CallId) {
        if self.last_call.as_ref() != Some(call) {
            self.last_call = Some(call.clone());
            self.rounds += 1;
            self.calls.push(call.clone());
        }
    }

    /// Write held tool results as one user message. A call without a result gets
    /// [`INTERRUPTED_RESULT`]: after a cancel that is the truth, and otherwise an
    /// unpaired tool use would be refused by every provider (the loop never leaves
    /// one on purpose).
    fn flush_results(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if pending.order.is_empty() {
            // A tool-use stop whose calls were never recorded: drop an assistant
            // turn left empty, which no provider accepts.
            if let Some(last) = self.messages.last()
                && last.role == Role::Assistant
                && last.content.is_empty()
            {
                self.messages.pop();
            }
            return;
        }
        let content = pending
            .order
            .iter()
            .map(|id| {
                let (output, is_error) = pending
                    .results
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| (INTERRUPTED_RESULT.to_owned(), true));
                Content::ToolResult {
                    tool_use_id: id.clone(),
                    content: output,
                    is_error,
                }
            })
            .collect();
        self.messages.push(Message {
            role: Role::User,
            content,
        });
    }

    /// The transcript as of now, with held results written out.
    pub fn messages(&mut self) -> &[Message] {
        self.flush_results();
        &self.messages
    }

    /// Model rounds spent (ledger calls seen).
    pub fn rounds(&self) -> u32 {
        self.rounds
    }

    /// Estimated context tokens for the next call: the provider's last reported
    /// input plus the output the next request carries, plus an estimate of what
    /// was appended since. Before any usage, an estimate of the whole opening.
    ///
    /// The output the next request carries is the text and tool calls. Reasoning
    /// is counted only when the route resends it (`with_reasoning_resent`); no
    /// adapter does today, so a thinking model's reasoning is billed on the
    /// round that produced it and absent from every later prompt.
    pub fn context_tokens(&self) -> u64 {
        self.anchored_tokens() + self.tokens_since_usage + self.unanchored_reasoning()
    }

    /// [`Self::context_tokens`] with the unreported part bounded from above
    /// rather than estimated: at two bytes per token instead of three. The bound
    /// holds for text that averages at least two UTF-8 bytes per token, which
    /// prose, code and CJK text do; text denser than that (long runs of random
    /// punctuation or base64) can exceed it. Used where an underestimate would
    /// let spend cross a ceiling.
    pub fn conservative_context_tokens(&self) -> u64 {
        self.anchored_tokens()
            + self.tokens_since_usage.saturating_mul(3).div_ceil(2)
            + self.unanchored_reasoning().saturating_mul(3).div_ceil(2)
    }

    /// What the last usage report says the next prompt holds.
    fn anchored_tokens(&self) -> u64 {
        let Some(anchor) = &self.anchor else {
            return 0;
        };
        let output = anchor.text_output + anchor.reasoning_reported;
        let unsent = if self.reasoning_resent {
            0
        } else if anchor.reasoning_reported > 0 {
            anchor.reasoning_reported
        } else {
            // The route did not split reasoning from text, so the reasoning
            // sits inside `text_output`. Take out a lower bound of it: the
            // reasoning text kGraf holds, at an assumed five bytes per token.
            // Real text runs three to four bytes per token, so this leaves
            // some reasoning counted rather than risk an underestimate.
            (anchor.reasoning_bytes as u64 / REASONING_BYTES_PER_TOKEN_CEILING)
                .min(anchor.text_output)
        };
        anchor.input + output - unsent
    }

    fn unanchored_reasoning(&self) -> u64 {
        if self.reasoning_resent {
            self.since_usage_reasoning
        } else {
            0
        }
    }

    /// Every tool call started, in order, for stall detection on resume.
    pub fn tool_calls(&self) -> &[(String, serde_json::Value)] {
        &self.tool_calls
    }

    /// The prefix digest the last `Context` event recorded.
    pub fn prefix_digest(&self) -> Option<&str> {
        self.prefix_digest.as_deref()
    }

    /// The private slices the last `Context` event recorded.
    pub fn slices(&self) -> Option<&[NodeId]> {
        self.slices.as_deref()
    }

    /// Every ledger call of the run, in order.
    pub fn calls(&self) -> &[CallId] {
        &self.calls
    }

    /// The tool calls of each round, in issue order, for replaying what the
    /// earlier segments did.
    pub fn replay_rounds(&self) -> &[Vec<ReplayCall>] {
        &self.replay
    }

    /// Tool calls that finished without error, (tool, input), in finish order.
    pub fn succeeded(&self) -> &[(String, serde_json::Value)] {
        &self.succeeded
    }
}

fn message_tokens(message: &Message) -> u64 {
    message
        .content
        .iter()
        .map(|c| match c {
            Content::ToolUse { name, input, .. } => {
                estimate_tokens(name) + estimate_tokens(&input.to_string())
            }
            other => other.as_text().map_or(0, estimate_tokens),
        })
        .sum()
}

/// A stored transcript that cannot be resumed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResumeError {
    /// An event is out of order within its run segment.
    #[error("the transcript is out of order at event {index}: {source}")]
    OutOfOrder {
        index: usize,
        #[source]
        source: SequenceError,
    },
    /// The run already ended with an answer; there is nothing to resume.
    #[error("the transcript ends in a completed run; there is nothing to resume")]
    AlreadyCompleted,
}

/// A run rebuilt from its stored events, ready to continue.
#[derive(Clone, Debug)]
pub struct ResumeState {
    pub(crate) builder: TranscriptBuilder,
    /// Calls that started and never finished before a cancel, (call id, tool).
    pub interrupted: Vec<(String, String)>,
    /// The ids of every `Injected` event in the transcript.
    injected: BTreeSet<String>,
}

impl ResumeState {
    /// Validate and rebuild. `opening` is the run's opening messages, rendered
    /// again from the brief and the graph (the graph is the source).
    ///
    /// The events may span several run segments: a segment ends cleanly at its
    /// `Cancelled` event (plus any late `Usage`), or is abandoned mid-round by
    /// an interrupt-mode steer (`RoundAbandoned`; the worker keeps running, so
    /// this does not end the segment); either way, the calls left open are
    /// interrupted. A segment can also end without either event: a hard kill
    /// writes nothing more. [`WorkerEvent::Context`] is emitted exactly once
    /// per segment, before its first round (see its doc comment), so it is the
    /// one reliable segment boundary: whatever the previous segment left open
    /// when it was not already cleanly ended is interrupted too, and the
    /// sequence (including mid-stream state) never carries into the next
    /// segment.
    pub fn from_events(opening: Vec<Message>, events: &[WorkerEvent]) -> Result<Self, ResumeError> {
        Self::from_events_with_base(opening, 0, events)
    }

    /// [`Self::from_events`], with the shared prefix's estimated size kept for
    /// the context estimate after a compaction (decision 0095). It is not
    /// added to the estimate before then: the usage reports anchor that.
    pub fn from_events_with_base(
        opening: Vec<Message>,
        base_tokens: u64,
        events: &[WorkerEvent],
    ) -> Result<Self, ResumeError> {
        let mut builder = TranscriptBuilder::new(opening);
        builder.base_tokens = base_tokens;
        builder.from_storage = true;
        let mut seq = EventSequence::new();
        let mut interrupted = Vec::new();
        let mut injected = BTreeSet::new();
        for (index, event) in events.iter().enumerate() {
            if matches!(event, WorkerEvent::Context { .. }) {
                if !seq.is_terminal() {
                    // No `Cancelled` closed the previous segment (a hard kill
                    // writes nothing more): whatever it left open never
                    // finished.
                    interrupted.extend(
                        seq.open_tool_calls()
                            .map(|(id, tool)| (id.to_owned(), tool.to_owned())),
                    );
                }
                seq = EventSequence::new();
            } else if seq.is_terminal() && !matches!(event, WorkerEvent::Usage { .. }) {
                seq = EventSequence::new();
            }
            if matches!(
                event,
                WorkerEvent::Cancelled { .. } | WorkerEvent::RoundAbandoned { .. }
            ) {
                interrupted.extend(
                    seq.open_tool_calls()
                        .map(|(id, tool)| (id.to_owned(), tool.to_owned())),
                );
            }
            seq.accept(event)
                .map_err(|source| ResumeError::OutOfOrder { index, source })?;
            if let WorkerEvent::Injected { id, .. } = event {
                injected.insert(id.clone());
            }
            builder.apply(event);
        }
        // A round that never reached `Stop` (a run that failed mid-stream) left
        // partial text behind. It is not a turn, and the next round must not
        // inherit it.
        builder.text.clear();
        builder.reasoning.clear();
        // What follows is live again: its text is not marked.
        builder.from_storage = false;
        // Only a `Stop` counts as a completed run here (a `Cancelled` last
        // event is exactly the resumable case this function exists for). Of
        // the `Stop` reasons, `ToolUse` and `PauseTurn` do not end the run
        // either: a tool call and a paused turn both continue with another
        // round, so neither makes the transcript "already completed".
        let ended_complete = matches!(
            events
                .iter()
                .rev()
                .find(|e| !matches!(e, WorkerEvent::Usage { .. })),
            Some(WorkerEvent::Stop { reason, .. })
                if !matches!(reason, StopReason::ToolUse | StopReason::PauseTurn)
        );
        if ended_complete {
            return Err(ResumeError::AlreadyCompleted);
        }
        Ok(Self {
            builder,
            interrupted,
            injected,
        })
    }

    /// The inbox ids already in the transcript (decision 0067). The transcript
    /// is the truth: an id here is delivered, whatever the inbox row says, and
    /// a resumed worker refuses to inject it again.
    pub fn injected_ids(&self) -> &BTreeSet<String> {
        &self.injected
    }

    /// Model rounds already spent.
    pub fn rounds_used(&self) -> u32 {
        self.builder.rounds()
    }

    /// The rebuilt messages.
    pub fn messages(&mut self) -> &[Message] {
        self.builder.messages()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::CancelPhase;
    use kgraf_core::TokenUsage;
    use serde_json::json;

    fn usage(call: &str, input: u64) -> WorkerEvent {
        WorkerEvent::Usage {
            call: CallId::new(call),
            usage: TokenUsage {
                uncached_input: input,
                text_output: 10,
                ..TokenUsage::default()
            },
        }
    }

    fn started(id: &str) -> WorkerEvent {
        WorkerEvent::ToolCallStarted {
            call_id: id.into(),
            tool: "read".into(),
            input: json!({"path": id}),
            signature: None,
        }
    }

    #[test]
    fn a_tool_call_signature_survives_the_event_log_into_the_rebuilt_history() {
        // The events as a resumed run reads them: written to JSON and back, with
        // the signature the provider attached to the call.
        let mut events = tool_round("c1", &["t1"]);
        for event in &mut events {
            if let WorkerEvent::ToolCallStarted { signature, .. } = event {
                *signature = Some(kgraf_provider::Signature::new("c2ln", "adapter:model"));
            }
        }
        let stored: Vec<WorkerEvent> = events
            .iter()
            .map(|e| {
                serde_json::from_str(&serde_json::to_string(e).expect("encode")).expect("decode")
            })
            .collect();
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]);
        for event in &stored {
            b.apply(event);
        }
        let messages = b.messages().to_vec();
        let signature = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .find_map(|c| match c {
                Content::ToolUse { signature, .. } => signature.clone(),
                _ => None,
            });
        assert_eq!(
            signature,
            Some(kgraf_provider::Signature::new("c2ln", "adapter:model")),
            "the value and the producer both survive"
        );
    }

    #[test]
    fn a_bare_string_signature_in_an_old_event_log_still_reads() {
        // History written before the producer was recorded holds a string.
        let old = r#"{"type":"tool_call_started","call_id":"t1","tool":"read","input":{},"signature":"c2ln"}"#;
        let event: WorkerEvent = serde_json::from_str(old).expect("old form");
        assert!(matches!(
            event,
            WorkerEvent::ToolCallStarted { signature: Some(ref s), .. }
                if s.value == "c2ln" && s.producer.is_none()
        ));
    }

    fn signed_round(text: &str, signature: Option<&str>) -> Vec<WorkerEvent> {
        vec![
            WorkerEvent::TextDelta { text: text.into() },
            WorkerEvent::AssistantMessage {
                text: text.into(),
                reasoning: String::new(),
                signature: signature.map(|s| kgraf_provider::Signature::new(s, "adapter:model")),
            },
            WorkerEvent::Stop {
                finish_reason: None,
                reason: StopReason::EndTurn,
            },
        ]
    }

    fn assistant_text_blocks(b: &mut TranscriptBuilder) -> Vec<(String, Option<String>)> {
        b.messages()
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .flat_map(|m| m.content.iter())
            .filter_map(|c| match c {
                Content::Text {
                    text, signature, ..
                } => Some((text.clone(), signature.as_ref().map(|s| s.value.clone()))),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_text_signature_survives_the_event_log_into_the_rebuilt_history() {
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]);
        for event in signed_round("the answer", Some("dGV4dA==")) {
            // Through JSON, as a resumed run reads the log.
            let stored: WorkerEvent =
                serde_json::from_str(&serde_json::to_string(&event).expect("encode"))
                    .expect("decode");
            b.apply(&stored);
        }
        assert_eq!(
            assistant_text_blocks(&mut b),
            vec![("the answer".to_owned(), Some("dGV4dA==".to_owned()))]
        );
    }

    #[test]
    fn a_signature_on_an_empty_reply_keeps_the_empty_signed_block() {
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]);
        for event in signed_round("", Some("b25seQ==")) {
            b.apply(&event);
        }
        assert_eq!(
            assistant_text_blocks(&mut b),
            vec![(String::new(), Some("b25seQ==".to_owned()))]
        );
        // An unsigned empty reply still leaves no text block.
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]);
        for event in signed_round("", None) {
            b.apply(&event);
        }
        assert!(assistant_text_blocks(&mut b).is_empty());
    }

    #[test]
    fn a_signature_does_not_leak_into_the_next_round() {
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]);
        for event in signed_round("one", Some("czE=")) {
            b.apply(&event);
        }
        for event in signed_round("two", None) {
            b.apply(&event);
        }
        assert_eq!(
            assistant_text_blocks(&mut b),
            vec![
                ("one".to_owned(), Some("czE=".to_owned())),
                ("two".to_owned(), None)
            ]
        );
    }

    #[test]
    fn an_assistant_message_without_a_signature_field_still_reads_and_writes_none() {
        let old = r#"{"type":"assistant_message","text":"hi","reasoning":""}"#;
        let event: WorkerEvent = serde_json::from_str(old).expect("old form");
        assert!(matches!(
            event,
            WorkerEvent::AssistantMessage {
                signature: None,
                ..
            }
        ));
        assert!(
            !serde_json::to_string(&event)
                .expect("encode")
                .contains("signature")
        );
    }

    #[test]
    fn an_event_log_written_before_signatures_still_reads() {
        let old = r#"{"type":"tool_call_started","call_id":"t1","tool":"read","input":{}}"#;
        let event: WorkerEvent = serde_json::from_str(old).expect("old form");
        assert!(matches!(
            event,
            WorkerEvent::ToolCallStarted {
                signature: None,
                ..
            }
        ));
        // And a call with none writes no field.
        assert!(
            !serde_json::to_string(&event)
                .expect("encode")
                .contains("signature")
        );
    }

    fn finished(id: &str) -> WorkerEvent {
        WorkerEvent::ToolCallFinished {
            call_id: id.into(),
            output: format!("contents of {id}"),
            is_error: false,
        }
    }

    fn tool_round(call: &str, ids: &[&str]) -> Vec<WorkerEvent> {
        let mut out = vec![
            WorkerEvent::TextDelta {
                text: "reading".into(),
            },
            usage(call, 100),
            WorkerEvent::Stop {
                finish_reason: None,
                reason: StopReason::ToolUse,
            },
        ];
        out.extend(ids.iter().map(|id| started(id)));
        out.extend(ids.iter().map(|id| finished(id)));
        out
    }

    #[test]
    fn a_tool_round_rebuilds_as_assistant_tool_uses_then_user_results_in_start_order() {
        let mut b = TranscriptBuilder::new(vec![Message::user("do it")]);
        let mut events = tool_round("c1", &["a", "b"]);
        // Results finish out of order; they are written in start order.
        events.swap(4, 5);
        let events = [
            events,
            vec![
                WorkerEvent::TextDelta {
                    text: "done".into(),
                },
                usage("c2", 150),
                WorkerEvent::Stop {
                    finish_reason: None,
                    reason: StopReason::EndTurn,
                },
            ],
        ]
        .concat();
        for e in &events {
            b.apply(e);
        }
        assert_eq!(b.rounds(), 2);
        let m = b.messages();
        assert_eq!(m.len(), 4);
        assert_eq!(m[1].role, Role::Assistant);
        assert_eq!(m[1].content.len(), 3, "text plus two tool uses");
        let ids: Vec<&str> = m[2]
            .content
            .iter()
            .filter_map(|c| match c {
                Content::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(m[3], Message::assistant("done"));
    }

    #[test]
    fn a_notice_joins_the_user_turn_after_the_tool_results() {
        let mut b = TranscriptBuilder::new(vec![Message::user("do it")]);
        for e in tool_round("c1", &["a"]) {
            b.apply(&e);
        }
        b.apply(&WorkerEvent::Notice {
            kind: crate::event::NoticeKind::FinalRound,
            text: "final".into(),
        });
        let m = b.messages();
        assert_eq!(m.len(), 3);
        assert!(matches!(m[2].content[0], Content::ToolResult { .. }));
        assert_eq!(m[2].content[1], Content::text("final"));
    }

    #[test]
    fn an_injected_message_joins_the_user_turn_after_the_tool_results() {
        let mut b = TranscriptBuilder::new(vec![Message::user("do it")]);
        for e in tool_round("c1", &["a"]) {
            b.apply(&e);
        }
        b.apply(&WorkerEvent::Injected {
            id: "msg-1".into(),
            source: kgraf_core::session::InboxSource::User,
            kind: kgraf_core::session::InboxKind::Redirect,
            text: "focus on the tests instead".into(),
        });
        let m = b.messages();
        assert_eq!(m.len(), 3);
        assert!(matches!(m[2].content[0], Content::ToolResult { .. }));
        assert_eq!(m[2].content[1], Content::text("focus on the tests instead"));
    }

    fn injected(id: &str, text: &str) -> WorkerEvent {
        WorkerEvent::Injected {
            id: id.into(),
            source: kgraf_core::session::InboxSource::User,
            kind: kgraf_core::session::InboxKind::Note,
            text: text.into(),
        }
    }

    #[test]
    fn before_the_first_call_an_injection_joins_the_opening_message() {
        let mut b = TranscriptBuilder::new(vec![Message::user("do it")]);
        b.apply(&injected("m1", "and be brief"));
        let m = b.messages();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].content[1], Content::text("and be brief"));
    }

    #[test]
    fn after_a_sent_turn_is_abandoned_the_injection_starts_a_new_user_turn() {
        // Round 1 is sent (ModelCall) with only the opening message, then an
        // interrupt abandons its stream. The opening was sent, so the note
        // must not extend it: it gets its own user turn.
        let mut b = TranscriptBuilder::new(vec![Message::user("do it")]);
        let events = [
            WorkerEvent::ModelCall {
                call: CallId::new("c1"),
            },
            WorkerEvent::TextDelta {
                text: "partial".into(),
            },
            WorkerEvent::RoundAbandoned {
                phase: CancelPhase::ModelStream,
                reason: "steer".into(),
            },
            injected("m1", "stop and do X"),
        ];
        for e in &events {
            b.apply(e);
        }
        let m = b.messages();
        assert_eq!(m.len(), 2);
        assert_eq!(
            m[0],
            Message::user("do it"),
            "the sent opening is unchanged"
        );
        assert_eq!(m[1], Message::user("stop and do X"));

        // The same after a later round: the tool-result turn round 2 carried
        // stays as it was sent.
        let mut b = TranscriptBuilder::new(vec![Message::user("do it")]);
        let mut events = vec![WorkerEvent::ModelCall {
            call: CallId::new("c1"),
        }];
        events.extend(tool_round("c1", &["a"]));
        events.push(WorkerEvent::ModelCall {
            call: CallId::new("c2"),
        });
        events.push(WorkerEvent::RoundAbandoned {
            phase: CancelPhase::ModelStream,
            reason: "steer".into(),
        });
        events.push(injected("m1", "stop"));
        for e in &events {
            b.apply(e);
        }
        let m = b.messages();
        assert_eq!(m.len(), 4);
        assert_eq!(
            m[2].content.len(),
            1,
            "the sent result turn is not extended"
        );
        assert_eq!(m[3], Message::user("stop"));
    }

    #[test]
    fn resume_reports_every_injected_id() {
        let mut events = tool_round("c1", &["a"]);
        events.push(injected("m1", "one"));
        events.push(injected("m2", "two"));
        let state = ResumeState::from_events(vec![Message::user("x")], &events).expect("resumable");
        let ids: Vec<&str> = state.injected_ids().iter().map(String::as_str).collect();
        assert_eq!(ids, ["m1", "m2"]);
    }

    #[test]
    fn round_abandoned_clears_the_partial_turn_like_cancelled_but_is_not_terminal() {
        let mut events = tool_round("c1", &["a", "b"]);
        events.pop(); // b never finishes
        events.push(WorkerEvent::RoundAbandoned {
            phase: CancelPhase::ToolCall,
            reason: "the user redirected the worker".into(),
        });
        // Unlike `Cancelled`, this does not end the sequence: the worker keeps
        // running, so a resumed segment is not what is being modeled here --
        // `ResumeState` only ever sees `Cancelled` at a segment boundary. The
        // builder alone is exercised: the abandoned round's partial text is
        // gone and its unfinished tool call gets the interrupted result, the
        // same as a cancel would leave it.
        let mut b = TranscriptBuilder::new(vec![Message::user("do it")]);
        for e in &events {
            b.apply(e);
        }
        let m = b.messages();
        let Content::ToolResult {
            content, is_error, ..
        } = &m[2].content[1]
        else {
            panic!("expected a result");
        };
        assert!(*is_error);
        assert_eq!(content, INTERRUPTED_RESULT);
    }

    /// The two new event variants (decision 0067) add no arm that could change
    /// how an existing transcript, recorded before they existed, rebuilds: a
    /// fixed set of pre-existing events must still produce byte-identical
    /// messages.
    #[test]
    fn an_existing_transcript_without_the_new_variants_still_loads_and_rebuilds_unchanged() {
        let mut events = tool_round("c1", &["a", "b"]);
        events.extend([
            WorkerEvent::Notice {
                kind: crate::event::NoticeKind::StepsLeft,
                text: "3 rounds left".into(),
            },
            WorkerEvent::TextDelta {
                text: "done".into(),
            },
            usage("c2", 150),
            WorkerEvent::Stop {
                finish_reason: None,
                reason: StopReason::EndTurn,
            },
        ]);
        let mut once = TranscriptBuilder::new(vec![Message::user("do it")]);
        for e in &events {
            once.apply(e);
        }
        let expected = serde_json::to_string(once.messages()).expect("serialize once");
        let mut again = TranscriptBuilder::new(vec![Message::user("do it")]);
        for e in &events {
            again.apply(e);
        }
        let actual = serde_json::to_string(again.messages()).expect("serialize again");
        assert_eq!(
            actual, expected,
            "rebuilding the same events is deterministic"
        );
        assert_eq!(
            expected,
            concat!(
                r#"[{"role":"user","content":[{"type":"text","text":"do it"}]},"#,
                r#"{"role":"assistant","content":[{"type":"text","text":"reading"},"#,
                r#"{"type":"tool_use","id":"a","name":"read","input":{"path":"a"}},"#,
                r#"{"type":"tool_use","id":"b","name":"read","input":{"path":"b"}}]},"#,
                r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"contents of a"},"#,
                r#"{"type":"tool_result","tool_use_id":"b","content":"contents of b"},"#,
                r#"{"type":"text","text":"3 rounds left"}]},"#,
                r#"{"role":"assistant","content":[{"type":"text","text":"done"}]}]"#
            ),
            "byte-for-byte: the rebuild of an event stream with no new variant is unchanged"
        );
    }

    #[test]
    fn a_cancel_mid_stream_drops_the_partial_turn_and_a_cancel_mid_tool_marks_open_calls() {
        let mut events = tool_round("c1", &["a", "b"]);
        events.pop(); // b never finishes
        events.push(WorkerEvent::Cancelled {
            phase: CancelPhase::ToolCall,
        });
        let mut state =
            ResumeState::from_events(vec![Message::user("do it")], &events).expect("resumable");
        assert_eq!(state.interrupted, vec![("b".to_owned(), "read".to_owned())]);
        assert_eq!(state.rounds_used(), 1);
        let m = state.messages();
        let Content::ToolResult {
            content, is_error, ..
        } = &m[2].content[1]
        else {
            panic!("expected a result");
        };
        assert!(*is_error);
        assert_eq!(content, INTERRUPTED_RESULT);

        let events = vec![
            WorkerEvent::TextDelta {
                text: "partial answ".into(),
            },
            WorkerEvent::Cancelled {
                phase: CancelPhase::ModelStream,
            },
            usage("c1", 90),
        ];
        let mut state =
            ResumeState::from_events(vec![Message::user("do it")], &events).expect("resumable");
        assert_eq!(state.rounds_used(), 1, "the cancelled round billed");
        assert_eq!(state.messages(), &[Message::user("do it")]);
    }

    #[test]
    fn round_abandoned_mid_tool_is_collected_as_interrupted_like_a_cancel() {
        let mut events = tool_round("c1", &["a", "b"]);
        events.pop(); // b never finishes
        events.push(WorkerEvent::RoundAbandoned {
            phase: CancelPhase::ToolCall,
            reason: "the user redirected the worker".into(),
        });
        let state =
            ResumeState::from_events(vec![Message::user("do it")], &events).expect("resumable");
        assert_eq!(state.interrupted, vec![("b".to_owned(), "read".to_owned())]);
        // Unlike `Cancelled`, this is not terminal: more events (the
        // injection, then a fresh round) may follow in the same segment.
        events.push(WorkerEvent::Injected {
            id: "msg-1".into(),
            source: kgraf_core::session::InboxSource::User,
            kind: kgraf_core::session::InboxKind::Redirect,
            text: "focus elsewhere".into(),
        });
        assert!(ResumeState::from_events(vec![Message::user("do it")], &events).is_ok());
    }

    #[test]
    fn a_hard_kill_mid_stream_does_not_leak_streaming_state_into_the_next_segment() {
        // No `Cancelled` at all: a hard kill writes nothing more once the
        // process dies, so this segment simply stops mid-stream.
        let killed_segment = vec![
            WorkerEvent::Context {
                prefix_digest: "d1".into(),
                slices: vec![],
                gitlink: None,
                no_gitlink: false,
            },
            WorkerEvent::TextDelta {
                text: "half an answ".into(),
            },
        ];
        // The resumed segment starts with its own `Context` (worker.rs's
        // `drive` emits exactly one per run), then a tool round. Without a
        // reset at the boundary, `streaming` from the killed segment would
        // still be `true` here, and `EventSequence` has no way to refuse a
        // `ToolCallStarted` for that, but a resume relying on `Injected`
        // right after `Context` would wrongly see `NotBetweenRounds`.
        let mut events = killed_segment;
        events.push(WorkerEvent::Context {
            prefix_digest: "d1".into(),
            slices: vec![],
            gitlink: None,
            no_gitlink: false,
        });
        events.push(WorkerEvent::Injected {
            id: "msg-1".into(),
            source: kgraf_core::session::InboxSource::User,
            kind: kgraf_core::session::InboxKind::Note,
            text: "still there?".into(),
        });
        let state =
            ResumeState::from_events(vec![Message::user("do it")], &events).expect("resumable");
        // The killed segment's partial text is not inherited.
        assert_eq!(state.builder.calls(), &Vec::<CallId>::new());
    }

    /// A three-segment history: killed mid-tool (no `Cancelled` at all), then
    /// resumed and run to a normal completion, then the *orchestrator*
    /// crashed before it recorded that result -- the transcript itself is
    /// complete, nothing else was appended. Before the `Context`-boundary fix,
    /// the first segment's dangling open tool call leaked into the second
    /// segment's `Stop`, which found `open` non-empty and refused with
    /// `EndedWithOpenCalls`; a resume then saw an out-of-order transcript
    /// instead of a completed one. With the fix, the second segment's own
    /// `Context` clears that state first, and the whole history reads as
    /// `AlreadyCompleted`, which is the truth: there is nothing to resume.
    #[test]
    fn a_hard_kill_mid_tool_then_a_completed_but_unrecorded_segment_is_already_completed() {
        let mut events = vec![WorkerEvent::Context {
            prefix_digest: "d1".into(),
            slices: vec![],
            gitlink: None,
            no_gitlink: false,
        }];
        let mut killed = tool_round("c1", &["a"]);
        killed.pop(); // "a" never finishes: the kill lands mid-tool.
        events.extend(killed);

        events.push(WorkerEvent::Context {
            prefix_digest: "d2".into(),
            slices: vec![],
            gitlink: None,
            no_gitlink: false,
        });
        events.push(WorkerEvent::TextDelta {
            text: "done".into(),
        });
        events.push(usage("c2", 50));
        events.push(WorkerEvent::Stop {
            finish_reason: None,
            reason: StopReason::EndTurn,
        });

        assert_eq!(
            ResumeState::from_events(vec![Message::user("do it")], &events).err(),
            Some(ResumeError::AlreadyCompleted)
        );
    }

    #[test]
    fn segments_after_a_cancel_are_accepted_and_a_completed_run_is_not_resumable() {
        let mut events = vec![
            WorkerEvent::Cancelled {
                phase: CancelPhase::BetweenRounds,
            },
            usage("c0", 1),
        ];
        events.extend(tool_round("c1", &["a"]));
        assert!(ResumeState::from_events(vec![Message::user("x")], &events).is_ok());
        events.push(WorkerEvent::Stop {
            finish_reason: None,
            reason: StopReason::EndTurn,
        });
        assert_eq!(
            ResumeState::from_events(vec![Message::user("x")], &events).err(),
            Some(ResumeError::AlreadyCompleted)
        );
        let bad = vec![finished("zzz")];
        assert!(matches!(
            ResumeState::from_events(vec![Message::user("x")], &bad),
            Err(ResumeError::OutOfOrder { index: 0, .. })
        ));
    }

    #[test]
    fn a_paused_turn_is_resumable_like_a_tool_round_not_a_completed_run() {
        // A `pause_turn` stop is not a finished run: a resume must be able to
        // continue it, the same way it continues a run that ended on a tool
        // call. Only a genuine terminal `Stop` (here `EndTurn`) makes it
        // `AlreadyCompleted`.
        let events = vec![
            WorkerEvent::TextDelta {
                text: "still searching".into(),
            },
            usage("c1", 100),
            WorkerEvent::Stop {
                finish_reason: None,
                reason: StopReason::PauseTurn,
            },
        ];
        assert!(ResumeState::from_events(vec![Message::user("x")], &events).is_ok());

        let mut completed = events;
        completed.push(WorkerEvent::Stop {
            finish_reason: None,
            reason: StopReason::EndTurn,
        });
        assert_eq!(
            ResumeState::from_events(vec![Message::user("x")], &completed).err(),
            Some(ResumeError::AlreadyCompleted)
        );
    }

    #[test]
    fn a_round_that_failed_before_any_usage_still_counts_and_calls_are_restored() {
        let events = vec![
            WorkerEvent::ModelCall {
                call: CallId::new("c1"),
            },
            WorkerEvent::ModelCall {
                call: CallId::new("c2"),
            },
            usage("c2", 10),
        ];
        let state = ResumeState::from_events(vec![Message::user("x")], &events).expect("ok");
        assert_eq!(state.rounds_used(), 2);
        assert_eq!(
            state.builder.calls(),
            &[CallId::new("c1"), CallId::new("c2")]
        );
    }

    #[test]
    fn only_calls_that_finished_without_error_count_as_succeeded() {
        let mut b = TranscriptBuilder::new(vec![Message::user("x")]);
        for e in tool_round("c1", &["a", "b"]) {
            b.apply(&e);
        }
        assert_eq!(b.succeeded().len(), 2);
        b.apply(&WorkerEvent::Stop {
            finish_reason: None,
            reason: StopReason::ToolUse,
        });
        b.apply(&started("c"));
        b.apply(&WorkerEvent::ToolCallFinished {
            call_id: "c".into(),
            output: "ERROR: no".into(),
            is_error: true,
        });
        assert_eq!(b.succeeded().len(), 2);
    }

    #[test]
    fn a_round_left_unfinished_by_a_failed_run_is_not_inherited_on_resume() {
        let events = vec![
            WorkerEvent::TextDelta {
                text: "half an ans".into(),
            },
            usage("c1", 50),
        ];
        let mut state =
            ResumeState::from_events(vec![Message::user("do it")], &events).expect("resumable");
        state.builder.apply(&WorkerEvent::TextDelta {
            text: "fresh".into(),
        });
        state.builder.apply(&WorkerEvent::Stop {
            finish_reason: None,
            reason: StopReason::EndTurn,
        });
        assert_eq!(state.messages()[1], Message::assistant("fresh"));
    }

    #[test]
    fn the_context_estimate_is_anchored_on_usage_and_grows_with_appended_results() {
        let mut b = TranscriptBuilder::new(vec![Message::user("x".repeat(300))]);
        assert!(b.context_tokens() >= 100);
        b.apply(&usage("c1", 1_000));
        assert_eq!(b.context_tokens(), 1_010);
        b.apply(&WorkerEvent::Stop {
            finish_reason: None,
            reason: StopReason::ToolUse,
        });
        b.apply(&started("a"));
        b.apply(&WorkerEvent::ToolCallFinished {
            call_id: "a".into(),
            output: "y".repeat(3_000),
            is_error: false,
        });
        assert!(b.context_tokens() > 2_000);
    }

    fn zero_usage(call: &str) -> WorkerEvent {
        WorkerEvent::Usage {
            call: CallId::new(call),
            usage: TokenUsage::default(),
        }
    }

    fn model_call(call: &str) -> WorkerEvent {
        WorkerEvent::ModelCall {
            call: CallId::new(call),
        }
    }

    #[test]
    fn a_zero_usage_report_does_not_reset_the_context_anchor() {
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]);
        b.apply(&model_call("c1"));
        b.apply(&usage("c1", 5_000));
        let anchored = b.context_tokens();
        assert!(anchored >= 5_000);
        // A call cancelled before its request left settles at zero.
        b.apply(&model_call("c2"));
        b.apply(&zero_usage("c2"));
        assert_eq!(b.context_tokens(), anchored);
    }

    #[test]
    fn a_round_abandoned_before_send_is_not_counted_but_one_abandoned_after_is() {
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]);
        b.apply(&model_call("c1"));
        b.apply(&WorkerEvent::RoundAbandoned {
            phase: crate::event::CancelPhase::ModelStream,
            reason: crate::worker::STEER_ABANDON_BEFORE_SEND_REASON.into(),
        });
        b.apply(&zero_usage("c1"));
        assert_eq!(b.rounds(), 0, "the refunded round bought no model turn");
        // The retry counts its own round.
        b.apply(&model_call("c2"));
        assert_eq!(b.rounds(), 1);
        // An abandon after the request left keeps its round spent.
        b.apply(&WorkerEvent::RoundAbandoned {
            phase: crate::event::CancelPhase::ModelStream,
            reason: crate::worker::STEER_ABANDON_REASON.into(),
        });
        assert_eq!(b.rounds(), 1);
    }

    fn assistant_text(text: &str) -> Vec<WorkerEvent> {
        vec![
            WorkerEvent::TextDelta { text: text.into() },
            WorkerEvent::AssistantMessage {
                text: text.into(),
                reasoning: String::new(),
                signature: None,
            },
            WorkerEvent::Stop {
                finish_reason: None,
                reason: StopReason::EndTurn,
            },
        ]
    }

    #[test]
    fn a_provider_that_reports_no_usage_still_has_its_output_counted() {
        // A local model reports zero usage on every round. The estimate must
        // still grow with what the model wrote and with the tool calls it made.
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]);
        let start = b.context_tokens();
        b.apply(&model_call("c1"));
        b.apply(&zero_usage("c1"));
        for event in assistant_text(&"answer word ".repeat(400)) {
            b.apply(&event);
        }
        let after_text = b.context_tokens();
        assert!(after_text > start + 1_000, "{start} -> {after_text}");

        b.apply(&model_call("c2"));
        b.apply(&zero_usage("c2"));
        b.apply(&WorkerEvent::Stop {
            finish_reason: None,
            reason: StopReason::ToolUse,
        });
        b.apply(&WorkerEvent::ToolCallStarted {
            call_id: "t1".into(),
            tool: "write_file".into(),
            input: json!({"path": "a.txt", "content": "x".repeat(3_000)}),
            signature: None,
        });
        assert!(b.context_tokens() > after_text + 500);
    }

    #[test]
    fn output_covered_by_a_usage_report_is_not_counted_twice() {
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]);
        b.apply(&model_call("c1"));
        b.apply(&usage("c1", 5_000));
        let anchored = b.context_tokens();
        for event in assistant_text(&"answer word ".repeat(400)) {
            b.apply(&event);
        }
        // The report's total already holds the round's output.
        assert_eq!(b.context_tokens(), anchored);
    }

    #[test]
    fn a_result_read_back_from_storage_that_holds_a_marker_says_to_re_read() {
        let marker = "[redacted:generic-api-key; re-read the source for exact text]";
        let events = vec![
            WorkerEvent::Stop {
                finish_reason: None,
                reason: StopReason::ToolUse,
            },
            WorkerEvent::ToolCallStarted {
                call_id: "t1".into(),
                tool: "read".into(),
                input: json!({"path": "a.rs"}),
                signature: None,
            },
            WorkerEvent::ToolCallFinished {
                call_id: "t1".into(),
                output: format!("let key = \"{marker}\";"),
                is_error: false,
            },
            WorkerEvent::ToolCallStarted {
                call_id: "t2".into(),
                tool: "apply_patch".into(),
                input: json!({"patch": format!("+ {marker}")}),
                signature: None,
            },
            WorkerEvent::ToolCallFinished {
                call_id: "t2".into(),
                output: "patched".into(),
                is_error: false,
            },
            WorkerEvent::ToolCallStarted {
                call_id: "t3".into(),
                tool: "read".into(),
                input: json!({"path": "b.rs"}),
                signature: None,
            },
            WorkerEvent::ToolCallFinished {
                call_id: "t3".into(),
                output: "plain".into(),
                is_error: false,
            },
        ];
        let result_text = |messages: &[Message], id: &str| -> String {
            messages
                .iter()
                .flat_map(|m| m.content.iter())
                .find_map(|c| match c {
                    Content::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } if tool_use_id == id => Some(content.clone()),
                    _ => None,
                })
                .expect("result")
        };
        // Stored: every call whose text or input carried a marker is noted.
        let mut stored = TranscriptBuilder::new(vec![Message::user("go")]);
        stored.from_storage = true;
        for event in &events {
            stored.apply(event);
        }
        let messages = stored.messages().to_vec();
        assert!(result_text(&messages, "t1").ends_with(REDACTED_HISTORY_NOTE));
        assert!(result_text(&messages, "t2").ends_with(REDACTED_HISTORY_NOTE));
        assert_eq!(result_text(&messages, "t3"), "plain");
        // Live: the model saw the text whole, so nothing is added.
        let mut live = TranscriptBuilder::new(vec![Message::user("go")]);
        for event in &events {
            live.apply(event);
        }
        let messages = live.messages().to_vec();
        assert!(!result_text(&messages, "t1").contains("note:"));
    }

    #[test]
    fn a_replayed_call_uses_the_rewritten_input_and_skips_denied_calls() {
        use kgraf_tools::approval::{ApprovalRecord, RecordedDecision};
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]);
        b.apply(&WorkerEvent::Stop {
            finish_reason: None,
            reason: StopReason::ToolUse,
        });
        b.apply(&WorkerEvent::ToolCallStarted {
            call_id: "t1".into(),
            tool: "write_file".into(),
            input: json!({"path": "original.txt"}),
            signature: None,
        });
        b.apply(&WorkerEvent::ToolCallStarted {
            call_id: "t2".into(),
            tool: "write_file".into(),
            input: json!({"path": "denied.txt"}),
            signature: None,
        });
        b.apply(&WorkerEvent::Approval {
            record: ApprovalRecord {
                call_id: "t1".into(),
                tool: "write_file".into(),
                decided_by: "hook".into(),
                decision: RecordedDecision::Rewritten {
                    original: json!({"path": "original.txt"}),
                    rewritten: json!({"path": "rewritten.txt"}),
                    reason: "test".into(),
                },
            },
        });
        b.apply(&finished("t1"));
        b.apply(&WorkerEvent::ToolCallFinished {
            call_id: "t2".into(),
            output: format!("{}hook: no", crate::worker::DENIED_PREFIX),
            is_error: true,
        });
        let rounds = b.replay_rounds();
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0][0].input, json!({"path": "rewritten.txt"}));
        assert!(rounds[0][0].succeeded && rounds[0][0].counted);
        assert!(!rounds[0][1].counted, "a denied call is not counted");
        // The model's own turn keeps the input it issued.
        let sent = b.messages().to_vec();
        assert!(sent.iter().flat_map(|m| m.content.iter()).any(|c| matches!(
            c,
            Content::ToolUse { input, .. } if input == &json!({"path": "original.txt"})
        )));
    }
    #[test]
    fn a_cancel_before_the_request_leaves_costs_no_round_on_resume() {
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]);
        b.apply(&model_call("c1"));
        b.apply(&usage("c1", 100));
        assert_eq!(b.rounds(), 1);
        // The cancel landed before round 2's request was sent.
        b.apply(&model_call("c2"));
        b.apply(&WorkerEvent::RoundAbandoned {
            phase: crate::event::CancelPhase::ModelStream,
            reason: crate::worker::CANCEL_BEFORE_SEND_REASON.into(),
        });
        b.apply(&WorkerEvent::Cancelled {
            phase: crate::event::CancelPhase::ModelStream,
        });
        b.apply(&zero_usage("c2"));
        assert_eq!(b.rounds(), 1, "the unsent round bought no model turn");
        // A cancel after the request left keeps its round spent.
        b.apply(&model_call("c3"));
        b.apply(&WorkerEvent::Cancelled {
            phase: crate::event::CancelPhase::ModelStream,
        });
        assert_eq!(b.rounds(), 2);
    }

    #[test]
    fn replay_counts_only_calls_the_live_stall_check_saw() {
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]);
        b.apply(&WorkerEvent::Stop {
            finish_reason: None,
            reason: StopReason::ToolUse,
        });
        for id in ["t1", "t2", "t3", "t4"] {
            b.apply(&WorkerEvent::ToolCallStarted {
                call_id: id.into(),
                tool: "read_file".into(),
                input: json!({"path": id}),
                signature: None,
            });
        }
        let result = |id: &str, output: String, is_error: bool| WorkerEvent::ToolCallFinished {
            call_id: id.into(),
            output,
            is_error,
        };
        // A successful result that starts like a denial is still a run call.
        b.apply(&result(
            "t1",
            format!("{}text a file holds", crate::worker::DENIED_PREFIX),
            false,
        ));
        // The final round refuses its calls before the stall check.
        b.apply(&result("t2", crate::worker::final_round_refusal(), true));
        b.apply(&result(
            "t3",
            format!("{}hook: no", crate::worker::DENIED_PREFIX),
            true,
        ));
        b.apply(&result(
            "t4",
            format!(
                "{} It may have written: a.txt.",
                crate::worker::STEER_INTERRUPTED
            ),
            true,
        ));
        let round = &b.replay_rounds()[0];
        assert!(round[0].counted, "a success is never a denial");
        assert!(
            !round[1].counted,
            "a final-round refusal never reached the check"
        );
        assert!(!round[2].counted);
        assert!(round[3].counted && round[3].interrupted_by_steer());
        assert!(!round[0].interrupted_by_steer());
    }

    fn thinking_round(b: &mut TranscriptBuilder, usage: TokenUsage, reasoning_bytes: usize) {
        b.apply(&WorkerEvent::ModelCall {
            call: CallId::new("c1"),
        });
        b.apply(&WorkerEvent::ReasoningDelta {
            text: "a".repeat(reasoning_bytes),
        });
        b.apply(&WorkerEvent::TextDelta { text: "ok".into() });
        b.apply(&WorkerEvent::Usage {
            call: CallId::new("c1"),
            usage,
        });
        b.apply(&WorkerEvent::Stop {
            reason: StopReason::EndTurn,
            finish_reason: None,
        });
    }

    fn usage_of(input: u64, text: u64, reasoning: u64) -> TokenUsage {
        TokenUsage {
            uncached_input: input,
            text_output: text,
            reasoning_output: reasoning,
            ..TokenUsage::default()
        }
    }

    #[test]
    fn reasoning_a_route_does_not_resend_leaves_the_estimate() {
        // Reported apart: exactly the reported share comes out.
        let mut split =
            TranscriptBuilder::new(vec![Message::user("go")]).with_reasoning_resent(false);
        thinking_round(&mut split, usage_of(6_000, 100, 3_000), 12_000);
        assert_eq!(split.context_tokens(), 6_100);
        // Not split (the reasoning sits in text output): a lower bound of it
        // comes out, 12,000 bytes at five bytes per token.
        let mut merged =
            TranscriptBuilder::new(vec![Message::user("go")]).with_reasoning_resent(false);
        thinking_round(&mut merged, usage_of(6_000, 3_100, 0), 12_000);
        assert_eq!(merged.context_tokens(), 6_000 + 3_100 - 2_400);
        // Never more than the output the report names.
        let mut small =
            TranscriptBuilder::new(vec![Message::user("go")]).with_reasoning_resent(false);
        thinking_round(&mut small, usage_of(6_000, 50, 0), 12_000);
        assert_eq!(small.context_tokens(), 6_000);
    }

    #[test]
    fn reasoning_a_route_resends_stays_in_the_estimate() {
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]).with_reasoning_resent(true);
        thinking_round(&mut b, usage_of(6_000, 100, 3_000), 12_000);
        assert_eq!(b.context_tokens(), 9_100);
        // The default, before the loop says otherwise, is the conservative one.
        let mut unknown = TranscriptBuilder::new(vec![Message::user("go")]);
        thinking_round(&mut unknown, usage_of(6_000, 3_100, 0), 12_000);
        assert_eq!(unknown.context_tokens(), 9_100);
    }

    #[test]
    fn the_setting_applies_to_a_transcript_already_replayed() {
        // A resume replays stored events before the loop knows the route.
        let mut b = TranscriptBuilder::new(vec![Message::user("go")]);
        thinking_round(&mut b, usage_of(6_000, 3_100, 0), 12_000);
        assert_eq!(b.context_tokens(), 9_100);
        assert_eq!(b.with_reasoning_resent(false).context_tokens(), 6_700);
    }

    #[test]
    fn an_unreported_round_counts_its_reasoning_only_where_it_is_resent() {
        let run = |resent| {
            let mut b =
                TranscriptBuilder::new(vec![Message::user("go")]).with_reasoning_resent(resent);
            b.apply(&WorkerEvent::ModelCall {
                call: CallId::new("c1"),
            });
            b.apply(&WorkerEvent::ReasoningDelta {
                text: "a".repeat(3_000),
            });
            b.apply(&WorkerEvent::Stop {
                reason: StopReason::EndTurn,
                finish_reason: None,
            });
            b.context_tokens()
        };
        assert_eq!(run(true) - run(false), estimate_tokens(&"a".repeat(3_000)));
    }
}
