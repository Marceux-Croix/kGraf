//! Runtime-enforced worker compaction (decision 0095).
//!
//! A worker is sized to finish under its plan target, and at its stop point it
//! hands its remainder back for the orchestrator to split (0057). Compaction is
//! the fallback for a worker that cannot hand back: the orchestrator says so at
//! spawn ([`StopPointPolicy::Compact`]) when a re-split child would be refused
//! (the delegation depth cap, the task length bound), and a user may ask for it
//! on one running worker. The model never triggers it, never performs it and
//! cannot suppress it.
//!
//! **What the runtime does at each compaction:**
//!
//! 1. Asks the worker's own route for a summary in a fixed JSON schema
//!    ([`CompactionSummary`]): goal, decisions, files touched, open items, and
//!    constraints quoted from the instruction. The call goes through the ledger
//!    (kind `worker_compaction`), so its tokens are attributed. A constraint the
//!    model did not quote word for word is dropped and counted, not kept as a
//!    paraphrase. A reply that does not parse, or a failed call, gives a summary
//!    built from what the loop observed ([`SummarySource::Runtime`]); the worker
//!    goes on either way.
//! 2. Appends [`crate::WorkerEvent::Compacted`], which carries the fixed notice
//!    ([`CompactionNotice`]). The event is built by the loop alone: no model
//!    output becomes a `Compacted` event, and text that claims a compaction is
//!    only text.
//! 3. Rebuilds the private remainder: the opening message (the instruction and
//!    slices, verbatim, never summarized), then the notice and the summary.
//!    The shared prefix does not change (0043).
//! 4. Keeps every event before the notice. The transcript file is append-only,
//!    so the pre-compaction history stays on disk, in order, before the
//!    `compacted` line; the loop keeps the same events in memory as the
//!    [`HistoryArchive`] that [`RECALL_TOOL`] searches.
//!
//! **Again, only on measured progress.** A worker spawned with
//! [`StopPointPolicy::Compact`] that reaches its stop point again is compacted
//! again only when [`recompaction`] allows it: the context right after its last
//! compaction was below the stop point (else no compaction can make room), the
//! rounds since that compaction made measured progress (the progress gate's
//! measure, decision 0096, over [`crate::progress::SegmentGate`]), and it has
//! compacted fewer times than the checkpoint ceiling. Otherwise it hands back
//! with full information and the reason ([`CompactionStop`] on the handoff);
//! it never truncates. A later summary keeps every earlier decision and quoted
//! constraint word for word ([`carry_forward`]), so a summary is never a
//! paraphrase of a paraphrase. A worker spawned with
//! [`StopPointPolicy::HandBack`] compacts at most once (when it has written
//! nothing), and a user's request compacts at most once.
//!
//! **Recall.** `recall_history` is registered in every worker's prefix, so the
//! tool list stays one per vendor and model; before a compaction it answers that
//! there is nothing to recall. It searches and reads only the calling worker's
//! own pre-compaction events, as the transcript file stores them (persist
//! scrubbed, decision 0071), with a bounded output and a cursor to continue.
//!
//! Written new. Searched: codex-rs `compact.rs` (summarizes with a free-form
//! prompt, no fixed schema, no recall), goose's summarize-on-threshold, and
//! OpenHands' `LLMSummarizingCondenser`; none keeps the history searchable by
//! the agent or quotes the instruction verbatim, and the ledger path is
//! kGraf's own.

use crate::event::WorkerEvent;
use kgraf_core::CallId;
use kgraf_provider::message::ToolDefinition;
use kgraf_tools::{Access, CallPermit, Tool, ToolContext, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The worker tool that searches the worker's own pre-compaction history.
pub const RECALL_TOOL: &str = "recall_history";

/// The first line of the text the runtime puts in front of a compacted worker.
pub const COMPACTION_NOTICE_HEADER: &str = "[kGraf runtime notice: context compacted]";

/// The ledger kind of the summary call.
pub const COMPACTION_CALL_KIND: &str = "worker_compaction";

/// The most characters one recall result carries. A longer answer stops at an
/// entry boundary or inside an entry, and says where to continue.
pub const MAX_RECALL_CHARS: usize = 8_000;
/// The most search hits one recall result lists.
pub const MAX_RECALL_HITS: usize = 8;
/// Entries a read returns when `count` is not given.
pub const DEFAULT_READ_COUNT: usize = 5;
/// The most entries one read returns.
pub const MAX_READ_COUNT: usize = 20;
/// Characters of context shown around a search hit.
const EXCERPT_CHARS: usize = 240;

/// The request appended to the worker's own context for the summary call.
pub const SUMMARY_REQUEST: &str = "kGraf is compacting your context because it reached its \
     limit. Do not call a tool. Reply with one JSON object and nothing else, in this schema: \
     {\"goal\": string, \"decisions\": [string], \"files_touched\": [string], \"open_items\": \
     [string], \"constraints\": [string]}. goal: what your instruction asks, in one sentence. \
     decisions: what you decided so far and why. files_touched: the paths you changed. \
     open_items: what is left to do, one small step each. constraints: requirements copied \
     word for word from your instruction; copy them exactly, never paraphrase. Your \
     instruction is shown to you again unchanged after compaction, and your full earlier \
     history stays searchable.";

/// What a worker does when its context reaches the stop point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopPointPolicy {
    /// Hand the remainder back for the orchestrator to split (0057). The
    /// default: a caller that can split the remainder wants it back.
    #[default]
    HandBack,
    /// Compact and go on: the caller cannot split the remainder (a re-split
    /// child would pass the delegation depth cap, or could not start with room
    /// to work). At a later stop point the worker compacts again while
    /// [`recompaction`] allows it, and hands back with the reason otherwise.
    Compact,
}

/// Why the runtime compacted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    /// The stop point, on a worker whose remainder cannot be handed back.
    StopPoint,
    /// A user asked for it on this worker (`/compact <worker>`).
    UserRequest,
}

/// Where the summary came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum SummarySource {
    /// The worker's route answered in the schema.
    Model,
    /// The route's answer could not be used; the runtime built the summary
    /// from what the loop observed.
    Runtime { reason: String },
}

/// The fixed summary schema.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionSummary {
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub files_touched: Vec<String>,
    #[serde(default)]
    pub open_items: Vec<String>,
    /// Requirements quoted from the instruction. Only quotes found word for
    /// word in the instruction (whitespace aside) are kept.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Quoted constraints dropped because the instruction does not hold them.
    #[serde(default)]
    pub constraints_dropped: u32,
}

/// The structured notice the runtime emits when it compacts a worker. Built by
/// the loop alone; no model output becomes one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionNotice {
    pub worker: String,
    pub task: String,
    pub trigger: CompactionTrigger,
    /// Wall-clock time of the compaction, in milliseconds since the epoch.
    pub at_ms: i64,
    /// Context tokens in use when the runtime compacted.
    pub at_tokens: u64,
    /// The worker's stop point.
    pub stop_tokens: u64,
    /// The route's effective window.
    pub window_tokens: u64,
    /// Estimated tokens of the notice and summary the worker continues from.
    pub summary_tokens: u64,
    /// The ledger row of the summary call, when one was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_call: Option<CallId>,
    pub summary_source: SummarySource,
    /// The worker's transcript file, when the caller named it. Every line
    /// before the `compacted` line is the pre-compaction history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<PathBuf>,
    /// Entries of pre-compaction history `recall_history` can reach.
    pub archived_entries: u64,
    /// Whether this worker's tools include `recall_history`. A worker without
    /// it is told that its history is in the transcript file only.
    pub recall: bool,
    /// Items the loop observed done (writes that succeeded).
    pub items_done: u64,
    /// Open items the summary lists.
    pub items_remaining: u64,
    /// Which compaction of this worker this is, from 1. Serde-defaulted so an
    /// older record (one compaction at most) still loads.
    #[serde(default = "first_compaction")]
    pub number: u32,
    /// Whether the worker may be compacted again at its next stop point, on
    /// measured progress ([`StopPointPolicy::Compact`]). The notice tells the
    /// model what happens at the limit. Serde-defaulted to the earlier rule.
    #[serde(default)]
    pub may_compact_again: bool,
}

fn first_compaction() -> u32 {
    1
}

/// Why a worker that cannot hand back was not compacted again at its stop
/// point and handed back instead (decision 0095, amendment of 2026-10-06).
/// Carried on the handoff, so the orchestrator's held reason says which.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stop", rename_all = "snake_case")]
pub enum CompactionStop {
    /// The context right after the last compaction (the opening message, the
    /// notice and the summary) already reached the stop point: no compaction
    /// can make room. The contract can never fit at this stop point.
    NoRoom {
        compactions: u32,
        floor_tokens: u64,
        stop_tokens: u64,
    },
    /// The rounds since the last compaction made no measured progress.
    NoProgress {
        compactions: u32,
        measure: crate::progress::ProgressMeasure,
    },
    /// The worker made progress but has compacted as many times as the
    /// checkpoint ceiling allows.
    Ceiling { compactions: u32, ceiling: u32 },
}

impl CompactionStop {
    /// One clause for a held reason.
    pub fn describe(&self) -> String {
        match self {
            Self::NoRoom {
                compactions,
                floor_tokens,
                stop_tokens,
            } => format!(
                "compaction cannot make room: after compaction {compactions} the context still \
                 took {floor_tokens} tokens, at or over the {stop_tokens}-token stop point, so \
                 no round could run; raise the context window or the stop point, or shorten the \
                 instruction"
            ),
            Self::NoProgress {
                compactions,
                measure,
            } => {
                let mut why = Vec::new();
                why.push(format!("{} file(s) changed", measure.files_changed.len()));
                if measure.checks_improved.is_empty() {
                    why.push("no check improved".to_owned());
                }
                for pattern in &measure.sloppy {
                    why.push(match pattern {
                        crate::progress::SloppyPattern::FailedWrites { failed, succeeded } => {
                            format!("{failed} failed write(s) against {succeeded} that succeeded")
                        }
                        crate::progress::SloppyPattern::RepeatedError { tool, times } => {
                            format!("one `{tool}` error {times} times")
                        }
                        crate::progress::SloppyPattern::RepeatedCalls { tool, times } => {
                            format!("one `{tool}` call with one result {times} times")
                        }
                        crate::progress::SloppyPattern::WriteThenRevert { paths } => {
                            format!("{} file(s) written and put back", paths.len())
                        }
                    });
                }
                format!(
                    "no measured progress since compaction {compactions} ({} over {} round(s)), \
                     so it was not compacted again",
                    why.join(", "),
                    measure.rounds
                )
            }
            Self::Ceiling {
                compactions,
                ceiling,
            } => format!(
                "it made progress but reached the ceiling of {ceiling} compaction(s) (the \
                 progress checkpoint ceiling) after {compactions}"
            ),
        }
    }
}

/// Whether a worker that cannot hand back may be compacted again at its stop
/// point. `compactions` is how many times it has compacted (at least 1),
/// `ceiling` the progress checkpoint ceiling, `floor_tokens` the context right
/// after its last compaction, and `progress` what the rounds since then
/// measured (`None`: not measurable, as for a transcript from before the
/// segment baseline was stored, which keeps the earlier rule: no second
/// compaction).
///
/// The order is the order of certainty: a context that cannot get under the
/// stop point can never fit, whatever the model does; a worker with no
/// progress stops whatever the ceiling; the ceiling bounds the rest.
pub fn recompaction(
    compactions: u32,
    ceiling: u32,
    floor_tokens: u64,
    stop_tokens: u64,
    progress: Option<&crate::progress::ProgressMeasure>,
) -> Result<(), CompactionStop> {
    if floor_tokens >= stop_tokens {
        return Err(CompactionStop::NoRoom {
            compactions,
            floor_tokens,
            stop_tokens,
        });
    }
    match progress {
        Some(measure) if measure.progressing() => {}
        other => {
            return Err(CompactionStop::NoProgress {
                compactions,
                measure: other.cloned().unwrap_or_default(),
            });
        }
    }
    if compactions >= ceiling.max(1) {
        return Err(CompactionStop::Ceiling {
            compactions,
            ceiling: ceiling.max(1),
        });
    }
    Ok(())
}

/// Keep what an earlier summary of this worker recorded: each of its decisions
/// and quoted constraints that `summary` does not hold word for word is put
/// back, in front and in the earlier order. A later summary is written from a
/// context that holds the earlier one, so without this its decisions would be
/// a restatement of a restatement (the compounding error decision 0057 warns
/// of); with it, every decision is the model's own first wording.
pub fn carry_forward(summary: &mut CompactionSummary, earlier: &CompactionSummary) {
    fn keep(now: &mut Vec<String>, before: &[String]) {
        let squashed: Vec<String> = now.iter().map(|d| squash(d)).collect();
        let mut kept: Vec<String> = before
            .iter()
            .filter(|d| !squashed.contains(&squash(d)))
            .cloned()
            .collect();
        kept.append(now);
        *now = kept;
    }
    keep(&mut summary.decisions, &earlier.decisions);
    keep(&mut summary.constraints, &earlier.constraints);
}

/// Parse a summary reply: the first `{` to the last `}`, in the fixed schema.
/// Constraints are checked against `instruction`; a quote it does not hold is
/// dropped and counted.
pub fn parse_summary(reply: &str, instruction: &str) -> Result<CompactionSummary, String> {
    let start = reply
        .find('{')
        .ok_or_else(|| "the reply holds no JSON object".to_owned())?;
    let end = reply
        .rfind('}')
        .filter(|end| *end > start)
        .ok_or_else(|| "the reply holds no complete JSON object".to_owned())?;
    let mut summary: CompactionSummary = serde_json::from_str(&reply[start..=end])
        .map_err(|e| format!("the reply is not in the summary schema: {e}"))?;
    if summary.goal.trim().is_empty()
        && summary.decisions.is_empty()
        && summary.open_items.is_empty()
        && summary.files_touched.is_empty()
    {
        return Err("the reply's summary is empty".into());
    }
    let haystack = squash(instruction);
    let quoted = std::mem::take(&mut summary.constraints);
    summary.constraints_dropped = 0;
    for quote in quoted {
        let needle = squash(&quote);
        if !needle.is_empty() && haystack.contains(&needle) {
            summary.constraints.push(quote);
        } else {
            summary.constraints_dropped += 1;
        }
    }
    Ok(summary)
}

/// The longest goal, in characters, that [`complete_from_state`] takes from
/// the instruction. The instruction itself is in the remainder whole; the goal
/// only names the task.
pub const RUNTIME_GOAL_CHARS: usize = 300;

/// Fill what `summary` lacks from kGraf's own state, never from the model:
///
/// - `goal`, when empty: the first non-blank line of the original instruction
///   (through every hand-back wrapper), cut at [`RUNTIME_GOAL_CHARS`].
/// - `decisions`, when empty: the observed writes (`done`).
/// - `files_touched`: each path the worktree shows changed (`changed`) not
///   already listed; the worktree outranks the model's recall.
/// - `open_items`: each declared write path (`write_scope`) with no recorded
///   change and no open item naming it.
///
/// A runtime fallback summary starts empty and is exactly this; a model summary
/// keeps its own entries and gains the rest.
pub fn complete_from_state(
    summary: &mut CompactionSummary,
    instruction: &str,
    write_scope: Option<&[PathBuf]>,
    done: &[String],
    changed: &[PathBuf],
) {
    if summary.goal.trim().is_empty() {
        summary.goal = runtime_goal(instruction);
    }
    // A model's decisions usually name its writes in its own words; the
    // observed writes stand in only where it listed none.
    if summary.decisions.is_empty() {
        summary.decisions = done.to_vec();
    }
    for path in changed {
        let path = path.to_string_lossy().into_owned();
        if !summary.files_touched.contains(&path) {
            summary.files_touched.push(path);
        }
    }
    for path in write_scope.unwrap_or_default() {
        if changed.contains(path) {
            continue;
        }
        let path = path.to_string_lossy().into_owned();
        if summary.open_items.iter().any(|item| item.contains(&path)) {
            continue;
        }
        summary
            .open_items
            .push(format!("write {path}: no change to it is recorded yet"));
    }
}

/// The first non-blank line of the original instruction, cut at
/// [`RUNTIME_GOAL_CHARS`].
fn runtime_goal(instruction: &str) -> String {
    let line = original_instruction(instruction)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.chars().count() > RUNTIME_GOAL_CHARS {
        let head: String = line.chars().take(RUNTIME_GOAL_CHARS).collect();
        format!("{head}...")
    } else {
        line.to_owned()
    }
}

/// Whitespace collapsed to single spaces, so a quote that reflowed lines still
/// matches.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The text a compacted worker continues from, after its opening message. Fixed
/// wording; only the numbers and the summary fields vary.
pub fn render_notice(notice: &CompactionNotice, summary: &CompactionSummary) -> String {
    let mut out = String::new();
    out.push_str(COMPACTION_NOTICE_HEADER);
    out.push('\n');
    let why = match notice.trigger {
        CompactionTrigger::StopPoint => "it reached its limit",
        CompactionTrigger::UserRequest => "the user asked for it",
    };
    out.push_str(&format!(
        "The kGraf runtime compacted your context because {why}: {} tokens in use, stop point \
         {} of a {}-token window. The instruction above is unchanged and complete. Below is a \
         summary of your earlier work; it can miss details. ",
        notice.at_tokens, notice.stop_tokens, notice.window_tokens,
    ));
    if notice.recall {
        out.push_str(&format!(
            "Your full earlier history ({} entries) is kept, and the {RECALL_TOOL} tool searches \
             it (\"query\": keywords) or reads it (\"from\", \"count\"). Use it before you redo \
             work or when a detail is missing. ",
            notice.archived_entries
        ));
    } else {
        out.push_str(
            "Your full earlier history is kept in your transcript file; you have no tool to read \
             it, so re-read files before you edit them. ",
        );
    }
    if notice.may_compact_again {
        out.push_str(
            "The orchestrator was told that this task was larger than planned. If you reach the \
             limit again, your context is compacted again only if your files or checks show \
             progress since this compaction; otherwise you stop and what is left is handed \
             back.\n",
        );
    } else {
        out.push_str(
            "The orchestrator was told that this task was larger than planned. If you reach the \
             limit again you stop and what is left is handed back.\n",
        );
    }
    if let SummarySource::Runtime { reason } = &notice.summary_source {
        out.push_str(&format!(
            "The summary was built by the runtime from your recorded actions ({reason}).\n"
        ));
    }
    out.push_str("\nSummary\nGoal: ");
    out.push_str(if summary.goal.trim().is_empty() {
        "(see the instruction above)"
    } else {
        summary.goal.trim()
    });
    out.push('\n');
    let mut section = |title: &str, items: &[String]| {
        out.push_str(title);
        out.push_str(":\n");
        if items.is_empty() {
            out.push_str("- (none)\n");
        }
        for item in items {
            out.push_str("- ");
            out.push_str(item.trim());
            out.push('\n');
        }
    };
    section("Decisions", &summary.decisions);
    section("Files touched", &summary.files_touched);
    section("Open items", &summary.open_items);
    section(
        "Constraints quoted from the instruction",
        &summary.constraints,
    );
    if summary.constraints_dropped > 0 {
        out.push_str(&format!(
            "({} quoted constraint(s) were not found word for word in the instruction and were \
             dropped; read the instruction above.)\n",
            summary.constraints_dropped
        ));
    }
    out
}

/// Whether `event` is a kind the recall archive keeps.
pub(crate) fn archived_kind(event: &WorkerEvent) -> bool {
    matches!(
        event,
        WorkerEvent::AssistantMessage { .. }
            | WorkerEvent::ToolCallStarted { .. }
            | WorkerEvent::ToolCallFinished { .. }
            | WorkerEvent::Notice { .. }
            | WorkerEvent::Injected { .. }
    )
}

/// The remainder a worker hands back at its stop point: the original
/// instruction, verbatim, with where it stopped. The orchestrator words a
/// would-be child the same way to ask the delegation guards, at dispatch,
/// whether a hand-back could be split at all.
pub fn handback_remainder(instruction: &str, context_tokens: u64, stop_tokens: u64) -> String {
    format!("{REMAINDER_HEAD}{context_tokens} of {stop_tokens}{REMAINDER_TAIL}{instruction}")
}

/// Where a hand-back remainder starts, up to its token counts.
pub const REMAINDER_HEAD: &str =
    "Continue the original instruction; the worker stopped at the context stop point (";
/// Where a hand-back remainder's token counts end and the instruction begins.
const REMAINDER_TAIL: &str = " tokens). Instruction: ";

/// `text` with every hand-back wrapper taken off its front: the instruction a
/// chain of hand-backs started from. A child's instruction is a remainder
/// that quotes its parent's, so each level of a chain wraps the one before;
/// this reads through all of them. Text that is no remainder comes back as is.
pub fn original_instruction(mut text: &str) -> &str {
    while let Some(rest) = text.strip_prefix(REMAINDER_HEAD) {
        match rest.split_once(REMAINDER_TAIL) {
            Some((_, inner)) => text = inner,
            None => break,
        }
    }
    text
}

/// The pre-compaction history of one worker, as recall entries: one per
/// assistant message, tool call, tool result, notice and delivered message, in
/// transcript order. Built from the persisted (persist-scrubbed) events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryArchive {
    entries: Vec<String>,
}

impl HistoryArchive {
    /// The archive of `events`, in order.
    pub fn from_events<'a>(events: impl IntoIterator<Item = &'a WorkerEvent>) -> Self {
        let mut entries = Vec::new();
        for event in events {
            let entry = match event {
                WorkerEvent::AssistantMessage { text, .. } if !text.trim().is_empty() => {
                    format!("assistant: {text}")
                }
                WorkerEvent::ToolCallStarted {
                    call_id,
                    tool,
                    input,
                    ..
                } => format!("call {call_id} {tool}: {input}"),
                WorkerEvent::ToolCallFinished {
                    call_id,
                    output,
                    is_error,
                    ..
                } => {
                    let kind = if *is_error { "error result" } else { "result" };
                    format!("{kind} {call_id}: {output}")
                }
                WorkerEvent::Notice { text, .. } => format!("notice: {text}"),
                WorkerEvent::Injected { kind, text, .. } => format!("message ({kind:?}): {text}"),
                _ => continue,
            };
            entries.push(entry);
        }
        Self { entries }
    }

    /// Entries in the archive.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the archive holds nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Answer one `recall_history` call. `Err` is a usage error the model reads.
    pub fn recall(&self, input: &serde_json::Value) -> Result<String, String> {
        let query = input
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty());
        let index = |key: &str| -> Result<Option<usize>, String> {
            match input.get(key) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(v) => v
                    .as_u64()
                    .map(|n| Some(usize::try_from(n).unwrap_or(usize::MAX)))
                    .ok_or_else(|| format!("`{key}` must be a whole number of zero or more")),
            }
        };
        let from = index("from")?;
        let count = index("count")?;
        let offset = index("offset")?;
        match (query, from) {
            (Some(q), None) => Ok(self.search(q)),
            (None, Some(from)) => self.read(from, count.unwrap_or(DEFAULT_READ_COUNT), offset),
            (Some(_), Some(_)) => Err("give either `query` or `from`, not both".into()),
            (None, None) => Err(format!(
                "give `query` (keywords to search for) or `from` (an entry number, 0 to {}) to read",
                self.entries.len().saturating_sub(1)
            )),
        }
    }

    /// Ranked keyword search: entries holding the whole query first, then by the
    /// number of distinct query words they hold, then in transcript order.
    fn search(&self, query: &str) -> String {
        let phrase = query.to_lowercase();
        let mut words: Vec<String> = phrase.split_whitespace().map(str::to_owned).collect();
        words.sort();
        words.dedup();
        let mut hits: Vec<(bool, usize, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let lower = entry.to_lowercase();
                let whole = lower.contains(&phrase);
                let matched = words.iter().filter(|w| lower.contains(w.as_str())).count();
                (whole || matched > 0).then_some((whole, matched, i))
            })
            .collect();
        if hits.is_empty() {
            return format!(
                "No entry of your earlier history ({} entries) holds any of: {query}",
                self.entries.len()
            );
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        let total = hits.len();
        let mut out = format!(
            "{total} matching entr{} of {} in your earlier history; best first. Read one whole with \
             {{\"from\": N}}.\n",
            if total == 1 { "y" } else { "ies" },
            self.entries.len()
        );
        for (_, _, i) in hits.iter().take(MAX_RECALL_HITS) {
            let entry = &self.entries[*i];
            let lower = entry.to_lowercase();
            let at = lower
                .find(&phrase)
                .or_else(|| words.iter().filter_map(|w| lower.find(w.as_str())).min())
                .unwrap_or(0);
            out.push_str(&format!("[{i}] {}\n", excerpt(entry, at)));
            if out.len() > MAX_RECALL_CHARS {
                break;
            }
        }
        if total > MAX_RECALL_HITS {
            out.push_str(&format!(
                "({} more matches not listed; use more specific words.)\n",
                total - MAX_RECALL_HITS
            ));
        }
        out
    }

    /// Entries `from..from + count`, the first starting at character `offset`,
    /// within [`MAX_RECALL_CHARS`]; a cut says where to continue.
    fn read(&self, from: usize, count: usize, offset: Option<usize>) -> Result<String, String> {
        if from >= self.entries.len() {
            return Err(format!(
                "entry {from} does not exist; your earlier history has entries 0 to {}",
                self.entries.len().saturating_sub(1)
            ));
        }
        let count = count.clamp(1, MAX_READ_COUNT);
        let end = from.saturating_add(count).min(self.entries.len());
        let mut out = String::new();
        let mut budget = MAX_RECALL_CHARS;
        for i in from..end {
            let skip = if i == from { offset.unwrap_or(0) } else { 0 };
            let entry: Vec<char> = self.entries[i].chars().collect();
            let skip = skip.min(entry.len());
            let header = if skip > 0 {
                format!("[{i}, from character {skip}] ")
            } else {
                format!("[{i}] ")
            };
            let rest = entry.len() - skip;
            if header.len() + rest <= budget {
                out.push_str(&header);
                out.extend(&entry[skip..]);
                out.push('\n');
                budget -= header.len() + rest + 1;
                continue;
            }
            // Cut inside this entry: show what fits and give the cursor.
            let room = budget.saturating_sub(header.len());
            out.push_str(&header);
            out.extend(&entry[skip..skip + room.min(rest)]);
            out.push_str(&format!(
                "\n[output limit of {MAX_RECALL_CHARS} characters reached; continue with \
                 {{\"from\": {i}, \"offset\": {}}}]\n",
                skip + room.min(rest)
            ));
            return Ok(out);
        }
        if end < self.entries.len() {
            out.push_str(&format!(
                "[entries {end} to {} follow; read on with {{\"from\": {end}}}]\n",
                self.entries.len() - 1
            ));
        }
        Ok(out)
    }
}

/// Up to [`EXCERPT_CHARS`] characters of `entry` around byte position `at`.
fn excerpt(entry: &str, at: usize) -> String {
    let chars: Vec<(usize, char)> = entry.char_indices().collect();
    let pos = chars.iter().position(|(b, _)| *b >= at).unwrap_or(0);
    let start = pos.saturating_sub(EXCERPT_CHARS / 3);
    let end = (start + EXCERPT_CHARS).min(chars.len());
    let mut text: String = chars[start..end].iter().map(|(_, c)| *c).collect();
    text = text.replace('\n', " ");
    let lead = if start > 0 { "..." } else { "" };
    let tail = if end < chars.len() { "..." } else { "" };
    format!("{lead}{text}{tail}")
}

/// What a worker that has not compacted gets from `recall_history`.
pub const NOTHING_TO_RECALL: &str = "Nothing to recall: your context has not been compacted, so your whole history is already \
     in your context.";

/// The `recall_history` tool's definition, for the prefix. The worker loop
/// answers every call itself from the calling worker's own archive, so the
/// tool's own `call` is reached only outside a worker loop, where it refuses.
pub struct RecallHistoryTool;

#[async_trait::async_trait]
impl Tool for RecallHistoryTool {
    fn name(&self) -> &str {
        RECALL_TOOL
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: RECALL_TOOL.into(),
            description: "Search or read your own earlier history after the kGraf runtime \
                          compacted your context. Give `query` (keywords) to list the best \
                          matching entries, or `from` (an entry number) with an optional \
                          `count` and `offset` to read entries whole. Read-only; reaches only \
                          your own history. Before a compaction there is nothing to recall."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Keywords to search for."},
                    "from": {"type": "integer", "minimum": 0, "description": "First entry to read."},
                    "count": {"type": "integer", "minimum": 1, "maximum": MAX_READ_COUNT, "description": "Entries to read."},
                    "offset": {"type": "integer", "minimum": 0, "description": "Character to start the first entry at."}
                },
                "additionalProperties": false
            }),
        }
    }

    fn access(&self, _input: &serde_json::Value) -> Result<Access, ToolError> {
        Ok(Access::none())
    }

    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        Err(ToolError::InvalidInput {
            tool: RECALL_TOOL.into(),
            reason: "recall_history is answered by the worker loop; it has no history here".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const INSTRUCTION: &str = "Rename `foo` to `bar` in src/lib.rs.\nKeep the public API \
                               stable.\nDo not touch tests/.";

    fn moving() -> crate::progress::ProgressMeasure {
        crate::progress::ProgressMeasure {
            rounds: 6,
            files_changed: vec![PathBuf::from("src/lib.rs")],
            ..Default::default()
        }
    }

    #[test]
    fn recompaction_needs_room_then_progress_then_a_count_under_the_ceiling() {
        // Room, progress, under the ceiling: compact again.
        assert_eq!(recompaction(1, 10, 7_000, 10_485, Some(&moving())), Ok(()));
        assert_eq!(recompaction(9, 10, 7_000, 10_485, Some(&moving())), Ok(()));
        // No room is reported first, whatever the progress: it can never fit.
        assert!(matches!(
            recompaction(1, 10, 10_485, 10_485, Some(&moving())),
            Err(CompactionStop::NoRoom {
                floor_tokens: 10_485,
                ..
            })
        ));
        // No progress next, whatever the ceiling.
        let stalled = crate::progress::ProgressMeasure {
            rounds: 5,
            sloppy: vec![crate::progress::SloppyPattern::FailedWrites {
                failed: 4,
                succeeded: 0,
            }],
            ..moving()
        };
        let stop = recompaction(10, 10, 7_000, 10_485, Some(&stalled)).expect_err("stalled");
        assert!(matches!(
            stop,
            CompactionStop::NoProgress {
                compactions: 10,
                ..
            }
        ));
        assert!(
            stop.describe().contains("4 failed write(s) against 0"),
            "{}",
            stop.describe()
        );
        // Not measurable (a record from before the baseline was stored): the
        // earlier rule, no second compaction.
        assert!(matches!(
            recompaction(1, 10, 7_000, 10_485, None),
            Err(CompactionStop::NoProgress { .. })
        ));
        // Progress at the ceiling: the ceiling. A ceiling of 1 (or 0, read as
        // 1) keeps the single compaction of before.
        assert_eq!(
            recompaction(10, 10, 7_000, 10_485, Some(&moving())),
            Err(CompactionStop::Ceiling {
                compactions: 10,
                ceiling: 10
            })
        );
        assert!(matches!(
            recompaction(1, 0, 7_000, 10_485, Some(&moving())),
            Err(CompactionStop::Ceiling { ceiling: 1, .. })
        ));
    }

    #[test]
    fn a_later_summary_keeps_every_earlier_decision_and_constraint_word_for_word() {
        let earlier = CompactionSummary {
            decisions: vec!["Stack uses a list".into(), "Queue uses a deque".into()],
            constraints: vec!["Do not touch tests/.".into()],
            ..Default::default()
        };
        let mut later = CompactionSummary {
            // A restatement of the first decision, the second kept as is
            // (whitespace aside), and one new decision.
            decisions: vec![
                "the stack is list-based".into(),
                "Queue  uses a deque".into(),
                "RingBuffer wraps at capacity".into(),
            ],
            ..Default::default()
        };
        carry_forward(&mut later, &earlier);
        assert_eq!(
            later.decisions,
            vec![
                "Stack uses a list",
                "the stack is list-based",
                "Queue  uses a deque",
                "RingBuffer wraps at capacity"
            ]
        );
        assert_eq!(later.constraints, vec!["Do not touch tests/."]);
    }

    #[test]
    fn original_instruction_reads_through_every_hand_back_wrapper() {
        let once = handback_remainder(INSTRUCTION, 11_000, 9_830);
        let thrice = handback_remainder(&handback_remainder(&once, 10_500, 9_830), 10_900, 9_830);
        assert_eq!(original_instruction(&once), INSTRUCTION);
        assert_eq!(original_instruction(&thrice), INSTRUCTION);
        // Text that is no remainder, and a header with no instruction, come back as they are.
        assert_eq!(original_instruction(INSTRUCTION), INSTRUCTION);
        let headless = format!("{REMAINDER_HEAD}1 of 2 tokens)");
        assert_eq!(original_instruction(&headless), headless);
        assert_eq!(original_instruction(""), "");
    }

    #[test]
    fn state_fills_an_empty_summary_and_only_the_gaps_of_a_model_summary() {
        let scope = [PathBuf::from("src/lib.rs"), PathBuf::from("src/api.rs")];
        let done = vec!["write_file wrote src/lib.rs".to_owned()];
        let changed = [PathBuf::from("src/lib.rs")];
        // A fallback: everything from state, the goal through a hand-back wrapper.
        let mut fallback = CompactionSummary::default();
        let wrapped = handback_remainder(INSTRUCTION, 11_000, 9_830);
        complete_from_state(&mut fallback, &wrapped, Some(&scope), &done, &changed);
        assert_eq!(fallback.goal, "Rename `foo` to `bar` in src/lib.rs.");
        assert_eq!(fallback.decisions, done);
        assert_eq!(fallback.files_touched, vec!["src/lib.rs".to_owned()]);
        assert_eq!(
            fallback.open_items,
            vec!["write src/api.rs: no change to it is recorded yet".to_owned()]
        );
        // A model summary keeps its own entries; an open item that names the
        // path already stands for it, and its own decisions are not padded.
        let mut model = CompactionSummary {
            goal: "rename foo".into(),
            decisions: vec!["use sed".into()],
            open_items: vec!["update callers in src/api.rs".into()],
            ..CompactionSummary::default()
        };
        complete_from_state(&mut model, INSTRUCTION, Some(&scope), &done, &changed);
        assert_eq!(model.goal, "rename foo");
        assert_eq!(model.decisions, vec!["use sed".to_owned()]);
        assert_eq!(
            model.open_items,
            vec!["update callers in src/api.rs".to_owned()]
        );
        // No write scope: no open item is made up.
        let mut none = CompactionSummary::default();
        complete_from_state(&mut none, INSTRUCTION, None, &[], &[]);
        assert!(none.open_items.is_empty());
        // A long first line is cut.
        let long = "x".repeat(RUNTIME_GOAL_CHARS + 10);
        let mut cut = CompactionSummary::default();
        complete_from_state(&mut cut, &long, None, &[], &[]);
        assert_eq!(cut.goal.chars().count(), RUNTIME_GOAL_CHARS + 3);
    }

    #[test]
    fn a_reply_in_the_schema_parses_and_keeps_only_verbatim_constraints() {
        let reply = "Here it is:\n{\"goal\": \"rename foo\", \"decisions\": [\"use sed\"], \
                     \"files_touched\": [\"src/lib.rs\"], \"open_items\": [\"update docs\"], \
                     \"constraints\": [\"Keep the public   API\\nstable.\", \"Never touch tests\"]}";
        let s = parse_summary(reply, INSTRUCTION).expect("parses");
        assert_eq!(s.goal, "rename foo");
        assert_eq!(
            s.constraints,
            vec!["Keep the public   API\nstable.".to_owned()]
        );
        assert_eq!(
            s.constraints_dropped, 1,
            "a paraphrase is dropped, not kept"
        );
    }

    #[test]
    fn a_reply_out_of_schema_or_empty_is_refused() {
        assert!(parse_summary("no json here", INSTRUCTION).is_err());
        assert!(parse_summary("{\"goal\": 3}", INSTRUCTION).is_err());
        assert!(parse_summary("{}", INSTRUCTION).is_err());
    }

    fn archive() -> HistoryArchive {
        let events = vec![
            WorkerEvent::AssistantMessage {
                text: "I will look for the config loader".into(),
                reasoning: String::new(),
                signature: None,
            },
            WorkerEvent::ToolCallStarted {
                call_id: "t1".into(),
                tool: "read_file".into(),
                input: json!({"path": "src/config.rs"}),
                signature: None,
            },
            WorkerEvent::ToolCallFinished {
                call_id: "t1".into(),
                output: "fn load() { let port = 8443; }".into(),
                is_error: false,
                duration_ms: None,
            },
            WorkerEvent::TextDelta {
                text: "ignored".into(),
            },
        ];
        HistoryArchive::from_events(&events)
    }

    #[test]
    fn search_ranks_the_entry_holding_the_whole_phrase_first() {
        let a = archive();
        assert_eq!(a.len(), 3, "deltas are not entries");
        let out = a.recall(&json!({"query": "port 8443"})).expect("search");
        let first = out.lines().nth(1).expect("a hit");
        assert!(first.starts_with("[2]"), "{out}");
        assert!(first.contains("8443"), "{out}");
    }

    #[test]
    fn a_read_is_bounded_and_says_where_to_continue() {
        let long = "x".repeat(MAX_RECALL_CHARS * 2);
        let a = HistoryArchive::from_events(&[WorkerEvent::Notice {
            kind: crate::event::NoticeKind::StepsLeft,
            text: long,
        }]);
        let out = a.recall(&json!({"from": 0})).expect("read");
        assert!(out.len() <= MAX_RECALL_CHARS + 200, "{}", out.len());
        assert!(out.contains("\"offset\""), "the cut gives a cursor");
        let next = a
            .recall(&json!({"from": 0, "offset": MAX_RECALL_CHARS}))
            .expect("read on");
        assert!(next.contains("from character"), "{next}");
    }

    #[test]
    fn a_usage_error_names_what_to_give() {
        let a = archive();
        assert!(a.recall(&json!({})).is_err());
        assert!(a.recall(&json!({"from": 99})).is_err());
        assert!(a.recall(&json!({"query": "x", "from": 0})).is_err());
        assert!(a.recall(&json!({"from": -1})).is_err());
    }
}
