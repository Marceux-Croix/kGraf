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
//!   context-size estimate.

use crate::event::{EventSequence, SequenceError, WorkerEvent};
use kgraf_core::budget::estimate_tokens;
use kgraf_core::{CallId, NodeId};
use kgraf_provider::{Content, Message, Role, StopReason};
use std::collections::{BTreeSet, HashMap};

/// The result recorded for a call that started and never finished.
pub const INTERRUPTED_RESULT: &str =
    "ERROR: interrupted: the worker was cancelled before this call returned a result";

#[derive(Clone, Debug, Default)]
struct PendingResults {
    order: Vec<String>,
    results: HashMap<String, (String, bool)>,
}

/// Builds the messages sent to the model from the events of a run.
#[derive(Clone, Debug)]
pub struct TranscriptBuilder {
    messages: Vec<Message>,
    text: String,
    reasoning: String,
    pending: Option<PendingResults>,
    last_call: Option<CallId>,
    rounds: u32,
    last_usage_tokens: Option<u64>,
    tokens_since_usage: u64,
    tool_calls: Vec<(String, serde_json::Value)>,
    started: HashMap<String, (String, serde_json::Value)>,
    succeeded: Vec<(String, serde_json::Value)>,
    calls: Vec<CallId>,
    prefix_digest: Option<String>,
    slices: Option<Vec<NodeId>>,
    /// How many leading messages some request has already carried: set at
    /// each `ModelCall`. An injection never joins a message below this index.
    sent: usize,
}

impl TranscriptBuilder {
    /// Start from the run's opening messages (the pinned instruction and slices).
    pub fn new(opening: Vec<Message>) -> Self {
        let tokens_since_usage = opening.iter().map(message_tokens).sum();
        Self {
            messages: opening,
            text: String::new(),
            reasoning: String::new(),
            pending: None,
            last_call: None,
            rounds: 0,
            last_usage_tokens: None,
            tokens_since_usage,
            tool_calls: Vec::new(),
            started: HashMap::new(),
            succeeded: Vec::new(),
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
        self
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
            WorkerEvent::AssistantMessage { text, reasoning } => {
                self.text = text.clone();
                self.reasoning = reasoning.clone();
            }
            WorkerEvent::ModelCall { call } => {
                self.count_round(call);
                // The loop reads `messages()` (which flushes) before it opens
                // the call, so flushing here changes nothing the loop sent; it
                // makes a rebuild from stored events agree on where the sent
                // messages end.
                self.flush_results();
                self.sent = self.messages.len();
            }
            WorkerEvent::Usage { call, usage } => {
                self.count_round(call);
                self.last_usage_tokens = Some(usage.total_input() + usage.total_output());
                self.tokens_since_usage = 0;
            }
            WorkerEvent::Stop { reason } => {
                self.flush_results();
                let mut content = Vec::new();
                if !self.reasoning.is_empty() {
                    content.push(Content::Reasoning {
                        text: std::mem::take(&mut self.reasoning),
                    });
                }
                if !self.text.is_empty() {
                    content.push(Content::text(std::mem::take(&mut self.text)));
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
            } => {
                self.tool_calls.push((tool.clone(), input.clone()));
                self.started
                    .insert(call_id.clone(), (tool.clone(), input.clone()));
                if let Some(last) = self.messages.last_mut()
                    && last.role == Role::Assistant
                {
                    last.content.push(Content::ToolUse {
                        id: call_id.clone(),
                        name: tool.clone(),
                        input: input.clone(),
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
                self.pending
                    .get_or_insert_with(PendingResults::default)
                    .results
                    .insert(call_id.clone(), (output.clone(), *is_error));
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
                self.text.clear();
                self.reasoning.clear();
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
            WorkerEvent::RoundAbandoned { .. } => {
                // Clears the partial turn the way `Cancelled` does, but is not
                // terminal: the worker keeps running past it.
                self.text.clear();
                self.reasoning.clear();
                self.flush_results();
            }
            WorkerEvent::Context {
                prefix_digest,
                slices,
            } => {
                self.prefix_digest = Some(prefix_digest.clone());
                self.slices = Some(slices.clone());
            }
            WorkerEvent::Approval { .. } => {}
            // Purely informational (decision 0067):
            // never joins a message, never touches what the model sees. The
            // transcript file and the live view are the only readers
            // (`transcripts.rs`'s own arm for these).
            WorkerEvent::Waiting { .. } | WorkerEvent::Resumed { .. } => {}
            // Persist-only redaction counts (decision 0071): never model input.
            WorkerEvent::Redacted { .. } => {}
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
    /// input plus output, plus an estimate of what was appended since. Before any
    /// usage, an estimate of the whole opening.
    pub fn context_tokens(&self) -> u64 {
        self.last_usage_tokens.unwrap_or(0) + self.tokens_since_usage
    }

    /// [`Self::context_tokens`] with the unreported part bounded from above
    /// rather than estimated: at two bytes per token instead of three. The bound
    /// holds for text that averages at least two UTF-8 bytes per token, which
    /// prose, code and CJK text do; text denser than that (long runs of random
    /// punctuation or base64) can exceed it. Used where an underestimate would
    /// let spend cross a ceiling.
    pub fn conservative_context_tokens(&self) -> u64 {
        self.last_usage_tokens.unwrap_or(0) + self.tokens_since_usage.saturating_mul(3).div_ceil(2)
    }

    /// Whether a usage figure has anchored the estimate yet.
    pub fn has_usage(&self) -> bool {
        self.last_usage_tokens.is_some()
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
        let mut builder = TranscriptBuilder::new(opening);
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
            Some(WorkerEvent::Stop { reason })
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
        }
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
        }];
        let mut killed = tool_round("c1", &["a"]);
        killed.pop(); // "a" never finishes: the kill lands mid-tool.
        events.extend(killed);

        events.push(WorkerEvent::Context {
            prefix_digest: "d2".into(),
            slices: vec![],
        });
        events.push(WorkerEvent::TextDelta {
            text: "done".into(),
        });
        events.push(usage("c2", 50));
        events.push(WorkerEvent::Stop {
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
                reason: StopReason::PauseTurn,
            },
        ];
        assert!(ResumeState::from_events(vec![Message::user("x")], &events).is_ok());

        let mut completed = events;
        completed.push(WorkerEvent::Stop {
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
            reason: StopReason::EndTurn,
        });
        assert_eq!(state.messages()[1], Message::assistant("fresh"));
    }

    #[test]
    fn the_context_estimate_is_anchored_on_usage_and_grows_with_appended_results() {
        let mut b = TranscriptBuilder::new(vec![Message::user("x".repeat(300))]);
        assert!(!b.has_usage());
        assert!(b.context_tokens() >= 100);
        b.apply(&usage("c1", 1_000));
        assert_eq!(b.context_tokens(), 1_010);
        b.apply(&WorkerEvent::Stop {
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
}
