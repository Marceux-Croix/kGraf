//! Where an interaction's time went (decision 0100, section 1).
//!
//! Every interaction the user waits on has two budgets: kGraf's own overhead,
//! and the model work it needs. This module splits an interaction's wall time
//! into three parts that always sum to it:
//!
//! - `model_us`: the time a model call attributed to the interaction was in
//!   flight (the ledger's `started` to `finished`), as a union of intervals;
//! - `tools_us`: the time the project's checks and builds ran, outside any
//!   model call;
//! - `kgraf_us`: everything else.
//!
//! **Attribution rule.** For an interaction over `[t0, t1]`, `M` is the union of
//! the ledger intervals of the calls attributed to it (by task id, call kind
//! and parent call), clipped to `[t0, t1]`. `T` is the union of the check and
//! tool intervals, clipped, minus `M`. Then `model = |M|`, `tools = |T|` and
//! `kgraf = wall - model - tools`. Model wins where the two overlap, so the
//! fields add up to the wall time by construction.
//!
//! **Clocks.** Offsets within an interaction come from [`std::time::Instant`],
//! in microseconds. One wall-clock millisecond (`at_ms`) anchors the event so
//! ledger rows, which carry wall-clock milliseconds, can be joined to it. Wall
//! time is never subtracted from an `Instant`; a ledger interval is placed on
//! the interaction's timeline by its offset from the wall anchor. Every
//! function here takes its times as arguments, so tests inject them.
//!
//! The eighteen interaction kinds are listed in [`LatencyKind`] with their
//! budget ids. A kind is measured only where [`LatencyKind::is_emitted`] says
//! so; the others are defined so their events have one schema when they are
//! added.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Instant;

/// The interactions with a budget (decision 0100, section 1, B1 to B15).
///
/// Each variant names the interval it measures:
///
/// | Budget | Kind | Starts | Ends |
/// |---|---|---|---|
/// | B1 | `note_ack` | the loop takes a note or box line | the note is persisted and its reply sent |
/// | B2 | `small_request_dispatch` | the last planner call ends | the worker's first ledger row starts |
/// | B3 | `triage_dispatch` | the request is received | the worker's first ledger row starts |
/// | B4 | `delta_applied` | a change is received | it is applied to the plan and written |
/// | B4a | `streamed_delta_dispatch` | an added contract's object parses | it is dispatched |
/// | B5 | `queued_rewrite` | a rewriting op parses | the queued contract is rewritten |
/// | B6 | `running_redirect` | a redirecting change is applied | the running worker sees it |
/// | B6a | `node_files_redirect` | a change names a running worker's files | that worker sees it |
/// | B7 | `watch_poll` | a run watch poll starts | its update is ready to send |
/// | B8 | `land` | a land is requested | the merge commit is built |
/// | B9 | `reopen` | a stopped run is reopened with a change | it is running again |
/// | B10 | `contract_merged` | the contract's dispatch starts | its verified work is merged |
/// | B10a | `root_gate` | the gate is ready to start | its result is in |
/// | B11 | `edit_applied` | an earlier instruction is opened | the edit is applied |
/// | B12 | `streamed_plan_dispatch` | an independent contract's object parses | it is dispatched |
/// | B13 | `land_suggestion` | the trigger fires | the suggestion is raised |
/// | B14 | `node_note_ack` | a message to a work node arrives | it is acknowledged |
/// | B15 | `node_note_delivered` | that message is acknowledged | the worker holding the node sees it |
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyKind {
    NoteAck,
    SmallRequestDispatch,
    TriageDispatch,
    DeltaApplied,
    StreamedDeltaDispatch,
    QueuedRewrite,
    RunningRedirect,
    NodeFilesRedirect,
    WatchPoll,
    Land,
    Reopen,
    ContractMerged,
    RootGate,
    EditApplied,
    StreamedPlanDispatch,
    LandSuggestion,
    NodeNoteAck,
    NodeNoteDelivered,
}

impl LatencyKind {
    /// Every kind, in budget order.
    pub const ALL: [LatencyKind; 18] = [
        Self::NoteAck,
        Self::SmallRequestDispatch,
        Self::TriageDispatch,
        Self::DeltaApplied,
        Self::StreamedDeltaDispatch,
        Self::QueuedRewrite,
        Self::RunningRedirect,
        Self::NodeFilesRedirect,
        Self::WatchPoll,
        Self::Land,
        Self::Reopen,
        Self::ContractMerged,
        Self::RootGate,
        Self::EditApplied,
        Self::StreamedPlanDispatch,
        Self::LandSuggestion,
        Self::NodeNoteAck,
        Self::NodeNoteDelivered,
    ];

    /// The budget id in decision 0100's table.
    pub fn budget(self) -> &'static str {
        match self {
            Self::NoteAck => "B1",
            Self::SmallRequestDispatch => "B2",
            Self::TriageDispatch => "B3",
            Self::DeltaApplied => "B4",
            Self::StreamedDeltaDispatch => "B4a",
            Self::QueuedRewrite => "B5",
            Self::RunningRedirect => "B6",
            Self::NodeFilesRedirect => "B6a",
            Self::WatchPoll => "B7",
            Self::Land => "B8",
            Self::Reopen => "B9",
            Self::ContractMerged => "B10",
            Self::RootGate => "B10a",
            Self::EditApplied => "B11",
            Self::StreamedPlanDispatch => "B12",
            Self::LandSuggestion => "B13",
            Self::NodeNoteAck => "B14",
            Self::NodeNoteDelivered => "B15",
        }
    }

    /// Whether the build measures this interaction now. A kind that is not
    /// emitted has its schema and no events.
    pub fn is_emitted(self) -> bool {
        matches!(
            self,
            Self::NoteAck
                | Self::SmallRequestDispatch
                | Self::WatchPoll
                | Self::ContractMerged
                | Self::RootGate
        )
    }
}

/// A half-open span on one interaction's timeline, in microseconds from its
/// start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    pub start_us: i64,
    pub end_us: i64,
}

impl Interval {
    pub fn new(start_us: i64, end_us: i64) -> Self {
        Self { start_us, end_us }
    }
}

/// Sorted, disjoint spans covering the same time as `spans`, each clipped to
/// `[0, wall_us]`. Empty and inverted spans drop out.
fn union_clipped(spans: &[Interval], wall_us: i64) -> Vec<Interval> {
    let mut clipped: Vec<Interval> = spans
        .iter()
        .map(|s| Interval::new(s.start_us.max(0), s.end_us.min(wall_us)))
        .filter(|s| s.end_us > s.start_us)
        .collect();
    clipped.sort_by_key(|s| (s.start_us, s.end_us));
    let mut out: Vec<Interval> = Vec::with_capacity(clipped.len());
    for s in clipped {
        match out.last_mut() {
            Some(last) if s.start_us <= last.end_us => last.end_us = last.end_us.max(s.end_us),
            _ => out.push(s),
        }
    }
    out
}

fn total(spans: &[Interval]) -> i64 {
    spans.iter().map(|s| s.end_us - s.start_us).sum()
}

/// `a` with every span of `b` cut out. Both sorted and disjoint.
fn subtract(a: &[Interval], b: &[Interval]) -> Vec<Interval> {
    let mut out = Vec::new();
    for s in a {
        let mut from = s.start_us;
        for cut in b {
            if cut.end_us <= from || cut.start_us >= s.end_us {
                continue;
            }
            if cut.start_us > from {
                out.push(Interval::new(from, cut.start_us));
            }
            from = from.max(cut.end_us);
        }
        if from < s.end_us {
            out.push(Interval::new(from, s.end_us));
        }
    }
    out
}

/// How many spans (after clipping) overlap at least one other span.
fn overlapped_count(spans: &[Interval], wall_us: i64) -> u32 {
    let clipped: Vec<Interval> = spans
        .iter()
        .map(|s| Interval::new(s.start_us.max(0), s.end_us.min(wall_us)))
        .filter(|s| s.end_us > s.start_us)
        .collect();
    let mut n = 0;
    for (i, a) in clipped.iter().enumerate() {
        let hit = clipped
            .iter()
            .enumerate()
            .any(|(j, b)| i != j && a.start_us < b.end_us && b.start_us < a.end_us);
        if hit {
            n += 1;
        }
    }
    n
}

/// One interaction's wall time, split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Split {
    pub wall_us: i64,
    pub kgraf_us: i64,
    pub model_us: i64,
    pub tools_us: i64,
    /// Attributed model calls that ran at the same time as another attributed
    /// call: where more than one model call was in flight, their time counts
    /// once in `model_us`.
    pub model_calls_overlapped: u32,
}

/// Split `[0, wall_us]` by the attribution rule. `model` is the attributed
/// calls' intervals and `tools` the check and tool intervals, both on the
/// interaction's timeline. Model time wins where they overlap, and the three
/// parts always sum to `wall_us`. A negative `wall_us` is read as zero.
pub fn attribute(wall_us: i64, model: &[Interval], tools: &[Interval]) -> Split {
    let wall_us = wall_us.max(0);
    let m = union_clipped(model, wall_us);
    let t = subtract(&union_clipped(tools, wall_us), &m);
    let model_us = total(&m);
    let tools_us = total(&t);
    Split {
        wall_us,
        kgraf_us: wall_us - model_us - tools_us,
        model_us,
        tools_us,
        model_calls_overlapped: overlapped_count(model, wall_us),
    }
}

/// What picks the ledger calls an interaction owns.
#[derive(Clone, Debug, Default)]
pub struct Attribution {
    /// The task id: a call is attributed when its task is this id or lies
    /// beneath it (`task/...`).
    pub task: String,
    /// Call kinds that count. `None` counts every kind.
    pub kinds: Option<Vec<String>>,
}

impl Attribution {
    fn direct(&self, c: &kgraf_provider::CallRecord) -> bool {
        let task_ok = c.task_id == self.task || c.task_id.starts_with(&format!("{}/", self.task));
        let kind_ok = match (&self.kinds, &c.trace.kind) {
            (None, _) => true,
            (Some(kinds), Some(k)) => kinds.iter().any(|x| x == k),
            (Some(_), None) => false,
        };
        task_ok && kind_ok
    }

    /// The calls of `records` attributed to the interaction: those it owns
    /// directly, and those made in service of one (their `parent_call` is an
    /// attributed call), to any depth.
    pub fn select<'a>(
        &self,
        records: &'a [kgraf_provider::CallRecord],
    ) -> Vec<&'a kgraf_provider::CallRecord> {
        let mut owned: BTreeMap<&str, &kgraf_provider::CallRecord> = records
            .iter()
            .filter(|c| self.direct(c))
            .map(|c| (c.id.as_str(), c))
            .collect();
        loop {
            let before = owned.len();
            for c in records {
                if let Some(parent) = &c.trace.parent_call
                    && owned.contains_key(parent.as_str())
                {
                    owned.entry(c.id.as_str()).or_insert(c);
                }
            }
            if owned.len() == before {
                break;
            }
        }
        // Back in the ledger's order.
        records
            .iter()
            .filter(|c| owned.contains_key(c.id.as_str()))
            .collect()
    }
}

/// Ledger calls as intervals on an interaction's timeline. `anchor_ms` is the
/// wall clock when the interaction started; a call's `started_ms` and
/// `finished_ms` become offsets from it. A call with no finish time is in
/// flight until `now_ms`.
pub fn call_intervals(
    calls: &[&kgraf_provider::CallRecord],
    anchor_ms: i64,
    now_ms: i64,
) -> Vec<Interval> {
    calls
        .iter()
        .map(|c| {
            Interval::new(
                (c.started_ms - anchor_ms) * 1_000,
                (c.finished_ms.unwrap_or(now_ms) - anchor_ms) * 1_000,
            )
        })
        .collect()
}

/// One interaction's measurement, as written to the trace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyEvent {
    pub kind: LatencyKind,
    /// The wall clock, in milliseconds since the epoch, when the interaction
    /// ended: the anchor for joining ledger rows.
    pub at_ms: i64,
    pub wall_us: i64,
    pub kgraf_us: i64,
    pub model_us: i64,
    pub tools_us: i64,
    pub model_calls_overlapped: u32,
    /// The contract, when the interaction is one contract's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
}

impl LatencyEvent {
    pub fn new(kind: LatencyKind, at_ms: i64, split: Split, contract: Option<String>) -> Self {
        Self {
            kind,
            at_ms,
            wall_us: split.wall_us,
            kgraf_us: split.kgraf_us,
            model_us: split.model_us,
            tools_us: split.tools_us,
            model_calls_overlapped: split.model_calls_overlapped,
            contract,
        }
    }

    /// Whether the three parts add up to the wall time.
    pub fn sums(&self) -> bool {
        self.kgraf_us + self.model_us + self.tools_us == self.wall_us
    }

    /// One short line: the kind, its budget id, the wall time and the split.
    pub fn line(&self) -> String {
        let ms = |us: i64| format!("{:.1}", us as f64 / 1_000.0);
        format!(
            "latency {} ({}): {} ms = kgraf {} + model {} + tools {}",
            self.kind_name(),
            self.kind.budget(),
            ms(self.wall_us),
            ms(self.kgraf_us),
            ms(self.model_us),
            ms(self.tools_us)
        )
    }

    fn kind_name(&self) -> String {
        serde_json::to_value(self.kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
    }
}

/// The newest split of each interaction kind.
#[derive(Clone, Debug, Default)]
pub struct LatencyBook {
    newest: BTreeMap<LatencyKind, LatencyEvent>,
}

impl LatencyBook {
    pub fn record(&mut self, event: &LatencyEvent) {
        self.newest.insert(event.kind, event.clone());
    }

    /// One line per kind that has an event, in budget order.
    pub fn lines(&self) -> Vec<String> {
        LatencyKind::ALL
            .iter()
            .filter_map(|k| self.newest.get(k))
            .map(LatencyEvent::line)
            .collect()
    }

    pub fn newest(&self, kind: LatencyKind) -> Option<&LatencyEvent> {
        self.newest.get(&kind)
    }
}

/// One step of a dispatch: its name, when it ended (offset from the attempt's
/// start) and how long it took.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchStep {
    pub step: String,
    pub offset_us: i64,
    pub us: i64,
}

/// The time of one `try_dispatch` attempt, step by step.
///
/// `steps` are in the order they ended. Each carries its own duration (from
/// the previous step's end), so the durations sum to the last offset, and
/// `residual_us` is the time after the last step: `total_us` equals the sum of
/// `us` plus `residual_us`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchLatency {
    /// The wall clock, in milliseconds since the epoch, when the attempt
    /// started.
    pub at_ms: i64,
    pub contract: String,
    /// `dispatched`, `deferred:<reason>`, `held`, `skipped` (returned with no
    /// decision to dispatch, hold or defer: a refused time limit, a contract
    /// that is gone) or `error`.
    pub outcome: String,
    pub total_us: i64,
    pub steps: Vec<DispatchStep>,
    pub residual_us: i64,
}

impl DispatchLatency {
    /// Whether the steps and the residual add up to the total.
    pub fn sums(&self) -> bool {
        self.steps.iter().map(|s| s.us).sum::<i64>() + self.residual_us == self.total_us
    }
}

/// Collects a dispatch attempt's steps. Times are passed in.
#[derive(Clone, Debug)]
pub struct DispatchTimer {
    contract: String,
    at_ms: i64,
    start: Instant,
    last: Instant,
    last_offset_us: i64,
    steps: Vec<DispatchStep>,
}

fn micros(from: Instant, to: Instant) -> i64 {
    to.saturating_duration_since(from)
        .as_micros()
        .min(i64::MAX as u128) as i64
}

impl DispatchTimer {
    pub fn new(contract: &str, at_ms: i64, start: Instant) -> Self {
        Self {
            contract: contract.to_owned(),
            at_ms,
            start,
            last: start,
            last_offset_us: 0,
            steps: Vec::new(),
        }
    }

    /// A step ended at `at`. Its duration is the difference of two offsets
    /// from the start, so the durations add up to the last offset exactly,
    /// whatever the clock's rounding.
    pub fn mark(&mut self, step: &str, at: Instant) {
        let at = at.max(self.last);
        let offset_us = micros(self.start, at);
        self.steps.push(DispatchStep {
            step: step.to_owned(),
            offset_us,
            us: offset_us - self.last_offset_us,
        });
        self.last = at;
        self.last_offset_us = offset_us;
    }

    /// The attempt ended at `end` with `outcome`.
    pub fn finish(self, end: Instant, outcome: impl Into<String>) -> DispatchLatency {
        let end = end.max(self.last);
        let total_us = micros(self.start, end);
        DispatchLatency {
            at_ms: self.at_ms,
            contract: self.contract,
            outcome: outcome.into(),
            total_us,
            residual_us: total_us - self.last_offset_us,
            steps: self.steps,
        }
    }
}

/// The outcome of one dispatch attempt: `error` when it failed, `dispatched`
/// when a flight is now running, `held` only when this attempt held the
/// contract (one already held that is looked at again is `skipped`),
/// `deferred:<reason>` when the attempt deferred it, else `skipped`.
pub fn dispatch_outcome(
    errored: bool,
    in_flight: bool,
    held_now: bool,
    held_before: bool,
    deferred: Option<String>,
) -> String {
    if errored {
        "error".to_owned()
    } else if in_flight {
        "dispatched".to_owned()
    } else if held_now && !held_before {
        "held".to_owned()
    } else if let Some(reason) = deferred {
        format!("deferred:{reason}")
    } else {
        "skipped".to_owned()
    }
}

/// When a contract's dispatch began and, if it has run, its verification.
#[derive(Clone, Copy, Debug)]
pub struct ContractClock {
    pub dispatched: Instant,
    pub dispatched_wall_ms: i64,
    pub verify_started: Option<Instant>,
    pub verify_ended: Option<Instant>,
}

/// A gate's readiness and start.
#[derive(Clone, Copy, Debug)]
pub struct GateClock {
    pub ready: Instant,
    pub ready_wall_ms: i64,
    pub started: Instant,
}

/// The orchestrator's latency state: the newest split per kind, the clocks of
/// the interactions in progress, and the dispatch attempt being timed.
#[derive(Debug)]
pub struct LatencyState {
    /// Whether the run measures at all. Measuring adds trace events and
    /// `/status` text; it changes no dispatch order, admission or ledger call
    /// count, which a test shows by running with this off.
    pub enabled: bool,
    pub book: LatencyBook,
    pub timer: Option<DispatchTimer>,
    /// Why the newest `defer` was raised, read by the dispatch wrapper.
    pub last_defer: Option<String>,
    pub contracts: BTreeMap<String, ContractClock>,
    pub gates: BTreeMap<String, GateClock>,
    /// Contracts whose dispatch gap is already written.
    pub gap_written: std::collections::BTreeSet<String>,
    /// The outcome of the newest `dispatch_latency` event written for a
    /// contract that did not dispatch: a deferral is written again only when
    /// its outcome changes, not on every pass.
    pub last_written: BTreeMap<String, String>,
}

impl LatencyState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            book: LatencyBook::default(),
            timer: None,
            last_defer: None,
            contracts: BTreeMap::new(),
            gates: BTreeMap::new(),
            gap_written: Default::default(),
            last_written: BTreeMap::new(),
        }
    }

    /// Whether a `dispatch_latency` event with `outcome` is written for
    /// `contract`: a dispatch, a hold and an error always; a deferral or a
    /// skip only when it differs from the one written last. Records it.
    pub fn should_write_dispatch(&mut self, contract: &str, outcome: &str) -> bool {
        if matches!(outcome, "dispatched" | "held" | "error") {
            self.last_written.remove(contract);
            return true;
        }
        if self.last_written.get(contract).map(String::as_str) == Some(outcome) {
            return false;
        }
        self.last_written
            .insert(contract.to_owned(), outcome.to_owned());
        true
    }

    /// Drop the clocks and marks of contracts that are no longer open and of
    /// gates that are no longer running, so a contract that ends any way (merged,
    /// failed, abandoned, cancelled) leaves nothing behind.
    pub fn retain_open(
        &mut self,
        open: impl Fn(&str) -> bool,
        gate_running: impl Fn(&str) -> bool,
    ) {
        self.contracts.retain(|c, _| open(c));
        self.gap_written.retain(|c| open(c));
        self.last_written.retain(|c, _| open(c));
        self.gates.retain(|g, _| gate_running(g));
    }

    /// Contract and gate clocks still held.
    pub fn clocks_held(&self) -> usize {
        self.contracts.len() + self.gates.len()
    }

    /// A step of the dispatch attempt in progress ended now. Nothing when no
    /// attempt is being timed.
    pub fn mark(&mut self, step: &str) {
        if let Some(t) = self.timer.as_mut() {
            t.mark(step, Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn iv(a: i64, b: i64) -> Interval {
        Interval::new(a, b)
    }

    fn sums(s: &Split) -> bool {
        s.kgraf_us + s.model_us + s.tools_us == s.wall_us
    }

    #[test]
    fn no_calls_and_no_checks_is_all_kgraf() {
        let s = attribute(1_000, &[], &[]);
        assert_eq!((s.kgraf_us, s.model_us, s.tools_us), (1_000, 0, 0));
        assert_eq!(s.model_calls_overlapped, 0);
    }

    #[test]
    fn disjoint_model_and_tool_spans_leave_the_gaps_to_kgraf() {
        let s = attribute(1_000, &[iv(100, 300)], &[iv(500, 700), iv(800, 850)]);
        assert_eq!(s.model_us, 200);
        assert_eq!(s.tools_us, 250);
        assert_eq!(s.kgraf_us, 550);
        assert!(sums(&s));
    }

    #[test]
    fn overlapping_model_calls_count_once() {
        let s = attribute(1_000, &[iv(0, 400), iv(200, 600), iv(900, 950)], &[]);
        assert_eq!(s.model_us, 650, "0..600 plus 900..950");
        assert_eq!(
            s.model_calls_overlapped, 2,
            "the first two overlap each other"
        );
        assert!(sums(&s));
    }

    #[test]
    fn model_wins_where_a_tool_span_overlaps_it() {
        let s = attribute(1_000, &[iv(200, 600)], &[iv(100, 400), iv(500, 800)]);
        // Tools: 100..200 and 600..800 remain once 200..600 belongs to the model.
        assert_eq!(s.model_us, 400);
        assert_eq!(s.tools_us, 100 + 200);
        assert_eq!(s.kgraf_us, 300);
        assert!(sums(&s));
    }

    #[test]
    fn spans_are_clipped_to_the_interaction() {
        let s = attribute(
            500,
            &[iv(-300, 100), iv(450, 900)],
            &[iv(-50, 20), iv(480, 2_000)],
        );
        // Model: 0..100 and 450..500. Tools: 0..20 is inside the first model
        // span; 480..500 is inside the second.
        assert_eq!(s.model_us, 150);
        assert_eq!(s.tools_us, 0);
        assert_eq!(s.kgraf_us, 350);
        assert!(sums(&s));
        // Entirely outside: no effect.
        let out = attribute(500, &[iv(600, 700), iv(-9, -1)], &[iv(900, 1_000)]);
        assert_eq!((out.model_us, out.tools_us, out.kgraf_us), (0, 0, 500));
    }

    #[test]
    fn inverted_and_empty_spans_and_a_negative_wall_are_harmless() {
        let s = attribute(100, &[iv(50, 50), iv(70, 60)], &[iv(10, 5)]);
        assert_eq!((s.model_us, s.tools_us, s.kgraf_us), (0, 0, 100));
        let z = attribute(-5, &[iv(0, 10)], &[]);
        assert_eq!((z.wall_us, z.model_us, z.kgraf_us), (0, 0, 0));
    }

    #[test]
    fn the_parts_sum_to_the_wall_for_scattered_spans() {
        // A fixed pseudo-random sweep: the identity holds for any input.
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |m: i64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % m as u64) as i64
        };
        for _ in 0..300 {
            let wall = next(5_000);
            let spans = |n: usize, next: &mut dyn FnMut(i64) -> i64| -> Vec<Interval> {
                (0..n)
                    .map(|_| {
                        let a = next(7_000) - 1_000;
                        iv(a, a + next(1_500) - 200)
                    })
                    .collect()
            };
            let m = spans(4, &mut next);
            let t = spans(4, &mut next);
            let s = attribute(wall, &m, &t);
            assert!(sums(&s), "{wall} {m:?} {t:?} {s:?}");
            assert!(s.kgraf_us >= 0 && s.model_us >= 0 && s.tools_us >= 0);
        }
    }

    fn rec(
        id: &str,
        task: &str,
        kind: Option<&str>,
        parent: Option<&str>,
        started: i64,
        finished: Option<i64>,
    ) -> kgraf_provider::CallRecord {
        kgraf_provider::CallRecord {
            id: kgraf_core::CallId::new(id),
            adapter: "a".into(),
            protocol: kgraf_core::Protocol::Additive,
            model: "m".into(),
            agent_id: "agent".into(),
            task_id: task.into(),
            settlement: kgraf_provider::Settlement::Counted,
            usage: Default::default(),
            request_id: None,
            started_ms: started,
            finished_ms: finished,
            trace: kgraf_provider::CallTrace {
                kind: kind.map(str::to_owned),
                parent_call: parent.map(kgraf_core::CallId::new),
                ..Default::default()
            },
            redacted: false,
            route: None,
            queued_ms: None,
            http_status: None,
            retry_after_ms: None,
        }
    }

    #[test]
    fn attribution_picks_calls_by_task_kind_and_parent() {
        let records = vec![
            rec(
                "c1",
                "run/a",
                Some("worker_round"),
                None,
                1_000,
                Some(1_200),
            ),
            rec(
                "c2",
                "run/a/sub",
                Some("worker_round"),
                None,
                1_100,
                Some(1_300),
            ),
            rec(
                "c3",
                "run/b",
                Some("worker_round"),
                None,
                1_000,
                Some(9_000),
            ),
            rec(
                "c4",
                "run/other-task",
                Some("embed"),
                Some("c1"),
                1_050,
                Some(1_060),
            ),
            rec(
                "c5",
                "run/a",
                Some("answer_question"),
                None,
                1_400,
                Some(1_500),
            ),
            rec(
                "c6",
                "run/ab",
                Some("worker_round"),
                None,
                1_000,
                Some(2_000),
            ),
        ];
        let all = Attribution {
            task: "run/a".into(),
            kinds: None,
        };
        let ids: Vec<&str> = all.select(&records).iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["c1", "c2", "c4", "c5"], "run/ab is not beneath run/a");
        let rounds = Attribution {
            task: "run/a".into(),
            kinds: Some(vec!["worker_round".into()]),
        };
        let ids: Vec<&str> = rounds
            .select(&records)
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, ["c1", "c2", "c4"], "c4 rides on its parent c1");
    }

    #[test]
    fn ledger_calls_land_on_the_timeline_by_offset_from_the_wall_anchor() {
        let records = [
            rec("c1", "t", None, None, 10_100, Some(10_300)),
            rec("c2", "t", None, None, 10_250, None),
        ];
        let refs: Vec<&kgraf_provider::CallRecord> = records.iter().collect();
        let spans = call_intervals(&refs, 10_000, 10_400);
        assert_eq!(spans, vec![iv(100_000, 300_000), iv(250_000, 400_000)]);
        let s = attribute(500_000, &spans, &[]);
        assert_eq!(s.model_us, 300_000, "100..400 ms once");
        assert_eq!(s.kgraf_us, 200_000);
        assert_eq!(s.model_calls_overlapped, 2);
    }

    #[test]
    fn the_event_sums_and_reads_as_one_short_line() {
        let s = attribute(2_000, &[iv(500, 1_500)], &[]);
        let e = LatencyEvent::new(LatencyKind::ContractMerged, 7, s, Some("c1".into()));
        assert!(e.sums());
        assert_eq!(
            e.line(),
            "latency contract_merged (B10): 2.0 ms = kgraf 1.0 + model 1.0 + tools 0.0"
        );
        let json = serde_json::to_string(&e).expect("json");
        assert!(json.contains("\"kind\":\"contract_merged\""), "{json}");
        let back: LatencyEvent = serde_json::from_str(&json).expect("round trip");
        assert_eq!(back, e);
    }

    #[test]
    fn every_kind_has_a_budget_id_and_a_snake_case_name() {
        let mut ids: Vec<&str> = LatencyKind::ALL.iter().map(|k| k.budget()).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "budget ids are unique");
        for k in LatencyKind::ALL {
            let name = serde_json::to_value(k).expect("json");
            assert!(
                name.as_str()
                    .is_some_and(|s| s.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
            );
        }
        let emitted: Vec<&str> = LatencyKind::ALL
            .iter()
            .filter(|k| k.is_emitted())
            .map(|k| k.budget())
            .collect();
        assert_eq!(emitted, ["B1", "B2", "B7", "B10", "B10a"]);
    }

    #[test]
    fn the_book_keeps_the_newest_split_per_kind_in_budget_order() {
        let mut book = LatencyBook::default();
        let at = |ms| {
            LatencyEvent::new(
                LatencyKind::NoteAck,
                ms,
                attribute(ms * 1_000, &[], &[]),
                None,
            )
        };
        book.record(&LatencyEvent::new(
            LatencyKind::RootGate,
            1,
            attribute(5_000, &[], &[iv(0, 5_000)]),
            None,
        ));
        book.record(&at(1));
        book.record(&at(9));
        let lines = book.lines();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with("latency note_ack (B1): 9.0 ms"),
            "{lines:?}"
        );
        assert!(
            lines[1].starts_with("latency root_gate (B10a): 5.0 ms"),
            "{lines:?}"
        );
    }

    #[test]
    fn held_is_the_outcome_only_of_the_attempt_that_held_the_contract() {
        let o =
            |e, f, hn, hb, d: Option<&str>| dispatch_outcome(e, f, hn, hb, d.map(str::to_owned));
        assert_eq!(
            o(false, false, true, false, None),
            "held",
            "this attempt held it"
        );
        assert_eq!(
            o(false, false, true, true, None),
            "skipped",
            "already held, looked at again"
        );
        assert_eq!(o(false, true, false, false, None), "dispatched");
        assert_eq!(o(true, false, true, false, None), "error");
        assert_eq!(
            o(false, false, false, false, Some("lease")),
            "deferred:lease"
        );
        assert_eq!(o(false, false, false, false, None), "skipped");
        assert_eq!(o(false, false, true, false, Some("lease")), "held");
        assert_eq!(
            o(false, false, true, true, Some("capacity")),
            "deferred:capacity"
        );
    }

    #[test]
    fn a_deferral_is_written_once_until_its_outcome_changes() {
        let mut s = LatencyState::new(true);
        assert!(s.should_write_dispatch("c", "deferred:capacity"));
        assert!(!s.should_write_dispatch("c", "deferred:capacity"));
        assert!(!s.should_write_dispatch("c", "deferred:capacity"));
        assert!(
            s.should_write_dispatch("c", "deferred:lease"),
            "the reason changed"
        );
        assert!(!s.should_write_dispatch("c", "deferred:lease"));
        assert!(
            s.should_write_dispatch("c", "deferred:capacity"),
            "changed back"
        );
        assert!(s.should_write_dispatch("c", "dispatched"));
        assert!(
            s.should_write_dispatch("c", "dispatched"),
            "a dispatch is always written"
        );
        assert!(s.should_write_dispatch("c", "held"));
        assert!(s.should_write_dispatch("c", "error"));
        // After a dispatch the next deferral is news again, and another
        // contract's marks are its own.
        assert!(s.should_write_dispatch("c", "deferred:capacity"));
        assert!(s.should_write_dispatch("d", "deferred:capacity"));
        assert!(s.should_write_dispatch("c", "skipped"));
        assert!(!s.should_write_dispatch("c", "skipped"));
    }

    #[test]
    fn a_contract_that_ends_leaves_no_clock_or_mark_behind() {
        let now = Instant::now();
        let clock = ContractClock {
            dispatched: now,
            dispatched_wall_ms: 1,
            verify_started: None,
            verify_ended: None,
        };
        let mut s = LatencyState::new(true);
        for c in ["merged", "failed", "abandoned", "still-open"] {
            s.contracts.insert(c.to_owned(), clock);
            s.gap_written.insert(c.to_owned());
            s.last_written.insert(c.to_owned(), "deferred:lease".into());
        }
        s.gates.insert(
            "g1".into(),
            GateClock {
                ready: now,
                ready_wall_ms: 1,
                started: now,
            },
        );
        assert_eq!(s.clocks_held(), 5);
        s.retain_open(|c| c == "still-open", |_| false);
        assert_eq!(s.clocks_held(), 1);
        assert!(s.contracts.contains_key("still-open"));
        assert_eq!(s.gap_written.len(), 1);
        assert_eq!(s.last_written.len(), 1);
        s.retain_open(|_| false, |_| false);
        assert_eq!(s.clocks_held(), 0);
        assert!(s.gap_written.is_empty() && s.last_written.is_empty());
    }

    #[test]
    fn dispatch_steps_are_ordered_and_sum_to_the_total_with_the_residual() {
        let t0 = Instant::now();
        let at = |us: u64| t0 + Duration::from_micros(us);
        let mut timer = DispatchTimer::new("c1", 123, t0);
        timer.mark("time_limit_gate", at(10));
        timer.mark("contract_gate", at(110));
        timer.mark("reground", at(110));
        timer.mark("worktree_prepare", at(5_110));
        let e = timer.finish(at(5_400), "dispatched");
        assert_eq!(e.contract, "c1");
        assert_eq!(e.outcome, "dispatched");
        assert_eq!(e.total_us, 5_400);
        assert_eq!(e.residual_us, 290);
        let names: Vec<&str> = e.steps.iter().map(|s| s.step.as_str()).collect();
        assert_eq!(
            names,
            [
                "time_limit_gate",
                "contract_gate",
                "reground",
                "worktree_prepare"
            ]
        );
        let offsets: Vec<i64> = e.steps.iter().map(|s| s.offset_us).collect();
        assert_eq!(offsets, [10, 110, 110, 5_110]);
        let us: Vec<i64> = e.steps.iter().map(|s| s.us).collect();
        assert_eq!(us, [10, 100, 0, 5_000]);
        assert!(e.sums());
    }

    /// Instants carry nanoseconds; each step's microseconds are truncated. The
    /// durations are differences of offsets, so they still add up exactly.
    #[test]
    fn steps_sum_exactly_when_the_clock_has_sub_microsecond_remainders() {
        let t0 = Instant::now();
        let mut timer = DispatchTimer::new("c", 0, t0);
        let mut at = 0u64;
        for i in 0..50 {
            at += 1_999 + (i % 3);
            timer.mark("s", t0 + Duration::from_nanos(at));
        }
        let e = timer.finish(t0 + Duration::from_nanos(at + 1_999), "dispatched");
        assert_eq!(e.steps.len(), 50);
        assert!(e.sums(), "{e:?}");
        assert_eq!(
            e.steps.last().map(|s| s.offset_us),
            Some(e.total_us - e.residual_us)
        );
    }

    #[test]
    fn a_step_that_ends_before_the_last_is_clamped_not_negative() {
        let t0 = Instant::now();
        let mut timer = DispatchTimer::new("c", 0, t0);
        timer.mark("a", t0 + Duration::from_micros(500));
        timer.mark("b", t0 + Duration::from_micros(100));
        let e = timer.finish(t0 + Duration::from_micros(50), "held");
        assert!(e.steps.iter().all(|s| s.us >= 0));
        assert!(e.residual_us >= 0);
        assert!(e.sums());
    }
}
