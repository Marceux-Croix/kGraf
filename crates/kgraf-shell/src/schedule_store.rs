//! Schedules on work nodes, as the desktop shell reads and records them (decision
//! 0052 section 3).
//!
//! The storage is `kgraf_schedule::node_store`, shared with `kgraf
//! schedule`. This module adds what only the shell does:
//!
//! - [`open`] opens the project's existing `graph.db` for writing facts
//!   (`GraphStore::open_existing_for_facts`: never creates it, and never
//!   rebuilds an index of another schema version, so a background tick cannot empty
//!   the code index), and moves any schedule still in the old graph-meta layout onto
//!   a work node;
//! - [`HistoryRecorder`] turns the scheduler loop's `TickEvent`s into history rows
//!   on each schedule's node (claimed and started, the outcome, the spend by run id,
//!   a disable with its reason), keeps any row it could not write and retries it on
//!   the next tick, and rebuilds lost outcome rows from the claim table;
//! - [`plan_nodes`] turns the schedules' views into work nodes for the galaxy's plan
//!   overlay, so the schedule, its last run and a disable reason are on the node's
//!   label (no new panel; the tray keeps its one summary line). Whether a
//!   schedule is disabled comes from the claim table, not from history.

use std::collections::BTreeSet;
use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use kgraf_galaxy::{PlanNode, WorkState};
use kgraf_graph::{GraphStore, RunSpend};
use kgraf_orchestrator::{ScheduledRuns, TickEvent};
use kgraf_schedule::ScheduleStore;
use kgraf_schedule::node_store::{self, EventKind, Migration, OwnedRunRecord, ScheduleView};

use crate::error::ShellError;

/// `context`, then the error and every source under it: a store error's own text
/// is often only "graph database error", and the cause is its source.
fn chain(context: &str, e: &(dyn std::error::Error + 'static)) -> String {
    let mut text = format!("{context}: {e}");
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(&format!(": {s}"));
        source = s.source();
    }
    text
}

fn config_err(context: &str, e: &(dyn std::error::Error + 'static)) -> ShellError {
    ShellError::Config(chain(context, e))
}

/// The project's graph, opened for writing facts, with old meta-stored schedules
/// moved onto work nodes. `None` when the project has no `graph.db`: this never
/// creates one, and no graph means no schedules.
pub fn open(project: &Path) -> Result<Option<(GraphStore, Vec<Migration>)>, ShellError> {
    let path = project
        .join(kgraf_graph::snapshot::STATE_DIR)
        .join("graph.db");
    if !path.exists() {
        return Ok(None);
    }
    let graph = GraphStore::open_existing_for_facts(&path, project)
        .map_err(|e| config_err(&format!("cannot open {}", path.display()), &e))?;
    let moved = node_store::migrate_legacy(&graph, Utc::now())
        .map_err(|e| config_err("cannot move old schedules onto work nodes", &e))?;
    Ok(Some((graph, moved)))
}

/// Every schedule's definition, with its node.
pub fn load_all(graph: &GraphStore) -> Result<Vec<node_store::NodeSchedule>, ShellError> {
    node_store::load_all(graph).map_err(|e| config_err("cannot read the schedules", &e))
}

/// The history row one tick event makes, if any: kind, schedule, due instant and
/// detail. A process-wide condition (`NotRunnable`) and a clock anomaly are not run
/// history; the tray line and the log carry those. A deferral is recorded once per
/// occurrence (its first), not once per tick while it waits; a deferred occurrence
/// that starts is a start.
fn run_record(event: &TickEvent) -> Option<(EventKind, &str, Option<DateTime<Utc>>, String)> {
    Some(match event {
        TickEvent::Started {
            schedule_id,
            due_at,
        } => (
            EventKind::Started,
            schedule_id.as_str(),
            Some(*due_at),
            "claimed and started".to_owned(),
        ),
        TickEvent::StartedAfterDeferral {
            schedule_id,
            due_at,
            deferred_count,
        } => (
            EventKind::Started,
            schedule_id,
            Some(*due_at),
            format!("claimed and started after {deferred_count} deferral(s)"),
        ),
        TickEvent::Reclaimed {
            schedule_id,
            due_at,
        } => (
            EventKind::Reclaimed,
            schedule_id,
            Some(*due_at),
            "an abandoned claim was reclaimed".to_owned(),
        ),
        TickEvent::Completed {
            schedule_id,
            due_at,
        } => (
            EventKind::Completed,
            schedule_id,
            Some(*due_at),
            String::new(),
        ),
        TickEvent::Failed {
            schedule_id,
            due_at,
            detail,
            ..
        } => (
            EventKind::Failed,
            schedule_id,
            Some(*due_at),
            detail.clone(),
        ),
        TickEvent::Disabled {
            schedule_id,
            reason,
        } => (EventKind::Disabled, schedule_id, None, reason.clone()),
        TickEvent::Deferred {
            schedule_id,
            due_at,
            detail,
            deferred_count,
        } if *deferred_count <= 1 => (
            EventKind::Deferred,
            schedule_id,
            Some(*due_at),
            detail.clone(),
        ),
        TickEvent::Dropped {
            schedule_id,
            due_at,
            detail,
        } => (
            EventKind::Dropped,
            schedule_id,
            Some(*due_at),
            detail.clone(),
        ),
        TickEvent::LeaseLost {
            schedule_id,
            due_at,
        } => (
            EventKind::LeaseLost,
            schedule_id,
            Some(*due_at),
            String::new(),
        ),
        TickEvent::RunIdNotRecorded {
            schedule_id,
            due_at,
            detail,
        } => (
            EventKind::RunIdNotRecorded,
            schedule_id,
            Some(*due_at),
            detail.clone(),
        ),
        TickEvent::Deferred { .. }
        | TickEvent::NotRunnable { .. }
        | TickEvent::ClockAnomaly { .. } => return None,
    })
}

/// The rows one tick's events make. `spend` reads a finished run's spend by run id.
/// A failure that disabled the schedule also makes a disable row with its reason.
pub fn records_of(
    events: &[TickEvent],
    spend: &dyn Fn(&str) -> Option<RunSpend>,
    now: DateTime<Utc>,
) -> Vec<OwnedRunRecord> {
    let mut out = Vec::new();
    for event in events {
        let Some((kind, schedule_id, due_at, detail)) = run_record(event) else {
            continue;
        };
        let run = due_at.map(|due| ScheduledRuns::run_id(schedule_id, due).to_string());
        let run_spend = match (kind, &run) {
            (EventKind::Completed | EventKind::Failed, Some(run)) => spend(run),
            _ => None,
        };
        if let TickEvent::Failed { disabled: true, .. } = event {
            out.push(OwnedRunRecord {
                schedule_id: schedule_id.to_owned(),
                kind,
                run_id: run.clone(),
                due_at,
                detail: detail.clone(),
                spend: run_spend,
                at: now,
            });
            out.push(OwnedRunRecord {
                schedule_id: schedule_id.to_owned(),
                kind: EventKind::Disabled,
                run_id: None,
                due_at: None,
                detail,
                spend: None,
                at: now,
            });
            continue;
        }
        out.push(OwnedRunRecord {
            schedule_id: schedule_id.to_owned(),
            kind,
            run_id: run,
            due_at,
            detail,
            spend: run_spend,
            at: now,
        });
    }
    out
}

/// Most unwritten rows kept for retry. Past this the oldest are dropped, logged at
/// error level with the count; [`HistoryRecorder`]'s claim-table rebuild still
/// restores their outcomes.
pub const MAX_PENDING: usize = 10_000;

/// What one [`HistoryRecorder::record_tick`] did, for the tray line (counts only) and the log (detail).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickRecord {
    /// Rows written this tick, retried ones included.
    pub written: usize,
    /// Rows still waiting to be written, kept for the next tick.
    pub pending: usize,
    /// Outcome rows rebuilt from the claim table.
    pub rebuilt: usize,
    /// Rows given up on (their schedule has no node to record on), and why.
    pub dropped: Vec<String>,
    /// Why the writes or the rebuild failed this tick.
    pub errors: Vec<String>,
    /// A write pass stopped at a busy graph; the rest wait for the next tick.
    pub stopped_busy: bool,
}

impl TickRecord {
    /// Adds the report of writing rebuilt rows: its written rows count as rebuilt.
    pub fn absorb_rebuilt(&mut self, other: TickRecord) {
        self.rebuilt += other.written;
        self.pending = other.pending;
        self.dropped.extend(other.dropped);
        self.errors.extend(other.errors);
        self.stopped_busy |= other.stopped_busy;
    }
}

/// Records run history on each schedule's node across ticks. A row
/// whose write fails (the graph busy past its timeout, say) is kept in memory and
/// retried first on the next tick. Each tick then compares the claim table's
/// settled occurrences with the history and rebuilds any lost outcome row, which
/// also covers a crash between a tick and its write. Rows are written once per run
/// id (a unique index), so a retry, a rebuild or a second app instance never
/// duplicate one.
///
/// **Bounded waits.** A write pass stops at the first busy
/// graph: the rest of the queue waits for the next tick, so a tick waits at most
/// one busy timeout, not one per queued row. The claim table is read in
/// [`HistoryRecorder::find_lost`], which only reads; the writes happen in
/// [`HistoryRecorder::write`], with no claim-table lock held, so the scheduler's
/// lease renewals are never kept waiting behind a busy graph.
#[derive(Debug)]
pub struct HistoryRecorder {
    pending: Vec<OwnedRunRecord>,
    /// Most rows kept for retry ([`MAX_PENDING`]).
    max_pending: usize,
    reconciler: node_store::Reconciler,
    /// Old meta rows already reported as not movable, so each is said once per
    /// process, not once per tick.
    refused_reported: BTreeSet<String>,
}

impl Default for HistoryRecorder {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            max_pending: MAX_PENDING,
            reconciler: node_store::Reconciler::default(),
            refused_reported: BTreeSet::new(),
        }
    }
}

impl HistoryRecorder {
    /// Rows waiting to be written.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Writes the waiting rows, then `new`, stopping at the first busy graph (the
    /// rest wait for the next tick). A row whose schedule has no node is dropped
    /// and said; any other failure keeps the row for retry. Past
    /// [`MAX_PENDING`] waiting rows the oldest are dropped, with the count in
    /// `errors` (the loop logs `errors` at error level).
    pub fn write(&mut self, graph: &GraphStore, new: Vec<OwnedRunRecord>) -> TickRecord {
        let mut report = TickRecord::default();
        let mut queue = std::mem::take(&mut self.pending)
            .into_iter()
            .chain(new)
            .collect::<std::collections::VecDeque<_>>();
        while let Some(rec) = queue.pop_front() {
            match node_store::record(graph, &rec.as_record(), rec.at) {
                Ok(true) => report.written += 1,
                Ok(false) => {}
                // No node to record on: the schedule never had one. Retrying cannot
                // help, so it is dropped and said.
                Err(e @ kgraf_schedule::ScheduleError::UnknownSchedule(_)) => {
                    report.dropped.push(chain(
                        &format!(
                            "schedule `{}`: `{}` has no work node to be recorded on",
                            rec.schedule_id,
                            rec.kind.as_str()
                        ),
                        &e,
                    ));
                }
                Err(e) => {
                    let busy = e.is_busy();
                    report.errors.push(chain(
                        &format!(
                            "schedule `{}`: `{}` not recorded on its work node yet",
                            rec.schedule_id,
                            rec.kind.as_str()
                        ),
                        &e,
                    ));
                    self.pending.push(rec);
                    if busy {
                        report.stopped_busy = true;
                        self.pending.extend(queue.drain(..));
                        break;
                    }
                }
            }
        }
        if self.pending.len() > self.max_pending {
            let over = self.pending.len() - self.max_pending;
            self.pending.drain(..over);
            report.errors.push(format!(
                "{over} unwritten history row(s) dropped past {} waiting; \
                 their outcomes are rebuilt from the claim table",
                self.max_pending
            ));
        }
        report.pending = self.pending.len();
        report
    }

    /// Reads the claim table for settled outcomes the history lacks, past this
    /// recorder's watermarks, skipping run ids still waiting in its queue (their
    /// real rows win). Only reads.
    pub fn find_lost(
        &mut self,
        graph: &GraphStore,
        claims: &ScheduleStore,
        spend: &dyn Fn(&str) -> Option<RunSpend>,
    ) -> Result<Vec<OwnedRunRecord>, String> {
        let queued: BTreeSet<String> = self
            .pending
            .iter()
            .filter(|r| matches!(r.kind, EventKind::Completed | EventKind::Failed))
            .filter_map(|r| r.run_id.clone())
            .collect();
        self.reconciler
            .missing_outcomes(graph, claims, &queued, spend)
            .map_err(|e| chain("cannot rebuild run history from the claim table", &e))
    }

    /// One tick's recording: [`Self::write`] the new rows, then, with a claim
    /// table, [`Self::find_lost`] and write what it found.
    pub fn record_tick(
        &mut self,
        graph: &GraphStore,
        new: Vec<OwnedRunRecord>,
        claims: Option<&ScheduleStore>,
        spend: &dyn Fn(&str) -> Option<RunSpend>,
    ) -> TickRecord {
        let mut report = self.write(graph, new);
        if let Some(claims) = claims {
            match self.find_lost(graph, claims, spend) {
                Ok(lost) => report.absorb_rebuilt(self.write(graph, lost)),
                Err(e) => report.errors.push(e),
            }
        }
        report
    }

    /// The migrations to say: every move, and each refused row once per process.
    pub fn migrations_to_report<'a>(&mut self, moved: &'a [Migration]) -> Vec<&'a Migration> {
        moved
            .iter()
            .filter(|m| match m {
                Migration::Refused { id, .. } => self.refused_reported.insert(id.clone()),
                _ => true,
            })
            .collect()
    }
}

/// The run events that decide whether a schedule's last run failed: a start or a
/// completion after a failure supersedes it; a deferral or a drop of a later
/// occurrence does not (no new run happened).
const OUTCOME_EVENTS: [&str; 4] = ["started", "reclaimed", "completed", "failed"];

/// The failure card of one history row, when it is a failure: (key, words). The
/// key names the row (`scheduled:<schedule>#<seq>`): each attempt, including a
/// retry of the same occurrence, is its own row, so a dismissed failure never
/// hides the next one (final check item 1).
pub fn failure_of(event: &kgraf_graph::ScheduleEvent) -> Option<(String, String)> {
    if event.event != "failed" {
        return None;
    }
    let id = &event.schedule_id;
    let run = event
        .run_id
        .as_deref()
        .map_or_else(String::new, |r| format!(" ({r})"));
    let detail = if event.detail.is_empty() {
        "no reason recorded".to_owned()
    } else {
        event.detail.clone()
    };
    Some((
        format!("scheduled:{id}#{}", event.seq),
        format!("scheduled run {id}{run} failed: {detail}"),
    ))
}

/// Every schedule whose newest run outcome in the graph's durable history is a
/// failure, one card each (final check item 5: a failure recorded while the app
/// was closed shows on the next launch).
pub fn failed_runs(graph: &GraphStore) -> Result<Vec<(String, String)>, ShellError> {
    let err = |e: kgraf_graph::GraphError| config_err("cannot read the schedules' history", &e);
    let mut out = Vec::new();
    for s in graph.schedules().map_err(err)? {
        if let Some(last) = graph
            .last_schedule_run(&s.id, &OUTCOME_EVENTS)
            .map_err(err)?
            && let Some(f) = failure_of(&last)
        {
            out.push(f);
        }
    }
    Ok(out)
}

/// Every schedule's view, for [`plan_nodes`].
pub fn views(graph: &GraphStore) -> Result<Vec<ScheduleView>, ShellError> {
    node_store::views(graph).map_err(|e| config_err("cannot read the schedules' history", &e))
}

/// The plan-overlay key of a scheduled work node. Distinct from a run's contract
/// keys (`<run>/<contract>`), so a schedule's node never merges with a contract.
pub fn node_key(node_id: &str) -> String {
    format!("work:{node_id}")
}

/// Longest label part, in characters; a longer one is cut with `...`.
pub const MAX_LABEL_PART: usize = 120;

pub(crate) fn cut(text: &str) -> String {
    if text.chars().count() <= MAX_LABEL_PART {
        text.to_owned()
    } else {
        format!(
            "{}...",
            text.chars().take(MAX_LABEL_PART - 3).collect::<String>()
        )
    }
}

fn at_text(ms: i64) -> String {
    match Utc.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M UTC").to_string(),
        None => format!("at {ms} ms"),
    }
}

/// One schedule's state on its node: the claim table's disable first, then the
/// user's switch, then a run in flight.
fn state_of(v: &ScheduleView, disabled: Option<&str>) -> WorkState {
    let running = v
        .last_run
        .as_ref()
        .is_some_and(|r| r.event == "started" || r.event == "reclaimed");
    if disabled.is_some() {
        WorkState::ScheduleDisabled
    } else if v.turned_off {
        WorkState::ScheduleOff
    } else if running {
        WorkState::InFlight
    } else {
        WorkState::Waiting
    }
}

/// One schedule's part of its node's label. A failed run's detail is left out when
/// the schedule is disabled: the reason already leads the label.
fn schedule_part(v: &ScheduleView, disabled: Option<&str>) -> String {
    let mut text = format!("schedule {}: {}", v.schedule_id, v.when);
    match &v.last_run {
        Some(run) => {
            let word = EventKind::parse(&run.event).map_or("recorded", EventKind::word);
            text.push_str(&format!("; last run {word} {}", at_text(run.at_ms)));
            if let Some(id) = &run.run_id {
                text.push_str(&format!(" ({id})"));
            }
            if let Some(spend) = &run.spend {
                text.push_str(&format!(", spend {}", spend.text));
            }
            if run.event == "failed" && !run.detail.is_empty() && disabled.is_none() {
                text.push_str(&format!(": {}", run.detail));
            }
        }
        None => text.push_str("; no run yet"),
    }
    cut(&text)
}

/// How urgent a state is on a node with several schedules: the node shows the most
/// urgent one.
fn rank(state: WorkState) -> u8 {
    match state {
        WorkState::ScheduleDisabled => 3,
        WorkState::InFlight => 2,
        WorkState::ScheduleOff => 1,
        _ => 0,
    }
}

/// The scheduled work nodes for the plan overlay: one per node, however many
/// schedules it has. `disabled` answers, from the claim table, why a schedule is
/// disabled now; history is only the timeline. The label leads with the
/// node's state and any reason, so a cut label still says why (decision 0052:
/// "disables the schedule and says why", on its node).
pub fn plan_nodes(
    views: &[ScheduleView],
    disabled: &dyn Fn(&str) -> Option<String>,
) -> Vec<PlanNode> {
    let mut by_node: std::collections::BTreeMap<&str, Vec<&ScheduleView>> =
        std::collections::BTreeMap::new();
    for v in views {
        by_node.entry(v.node_id.as_str()).or_default().push(v);
    }
    by_node
        .into_iter()
        .map(|(node, schedules)| {
            let reasons: Vec<Option<String>> =
                schedules.iter().map(|v| disabled(&v.schedule_id)).collect();
            let state = schedules
                .iter()
                .zip(&reasons)
                .map(|(v, r)| state_of(v, r.as_deref()))
                .max_by_key(|s| rank(*s))
                .unwrap_or(WorkState::Waiting);
            let heads: BTreeSet<String> = schedules
                .iter()
                .zip(&reasons)
                .filter_map(|(v, r)| match r {
                    Some(r) => Some(cut(&format!("disabled: {r}"))),
                    None => v.turned_off.then(|| "turned off".to_owned()),
                })
                .collect();
            let title = cut(&schedules[0].node_title);
            let mut label = format!("{title}: {}", state.word());
            if !heads.is_empty() {
                label.push_str(&format!(
                    " ({})",
                    heads.into_iter().collect::<Vec<_>>().join("; ")
                ));
            }
            for (v, r) in schedules.iter().zip(&reasons) {
                label.push_str(&format!(" | {}", schedule_part(v, r.as_deref())));
            }
            PlanNode {
                key: node_key(node),
                state,
                label,
                blocked_by: Vec::new(),
                asks: 0,
                parent: None,
            }
        })
        .collect()
}

/// The claim table's disable reason for each schedule, as [`plan_nodes`] asks for
/// it. With no claim table yet, nothing is disabled.
pub fn disabled_reasons(
    claims: Option<&ScheduleStore>,
    views: &[ScheduleView],
) -> Result<std::collections::BTreeMap<String, String>, ShellError> {
    let mut out = std::collections::BTreeMap::new();
    let Some(claims) = claims else {
        return Ok(out);
    };
    for v in views {
        // `peek_status` writes nothing, so a read-only claim table serves.
        let status = claims
            .peek_status(&v.schedule_id)
            .map_err(|e| config_err("cannot read a schedule's status from the claim table", &e))?;
        if let Some(reason) = status.and_then(|s| s.disabled_reason) {
            out.insert(v.schedule_id.clone(), reason);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_schedule::node_store::NodeChoice;
    use kgraf_schedule::{ClaimOutcome, RunOutcome, ScheduleDefinition};

    fn project() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().expect("dir");
        let state = dir.path().join(kgraf_graph::snapshot::STATE_DIR);
        std::fs::create_dir_all(&state).expect("mkdir");
        let graph = GraphStore::open(&state.join("graph.db"), dir.path()).expect("graph");
        (dir, graph)
    }

    fn due() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 27, 3, 0, 0).unwrap()
    }

    fn nightly(graph: &GraphStore) {
        let def = ScheduleDefinition::recurring("nightly", "Nightly build", "0 0 3 * * *", "UTC")
            .expect("valid");
        node_store::add(graph, &def, NodeChoice::FromInstruction, due()).expect("add");
    }

    fn no_spend(_: &str) -> Option<RunSpend> {
        None
    }

    fn none_disabled(_: &str) -> Option<String> {
        None
    }

    fn record(graph: &GraphStore, events: &[TickEvent]) -> TickRecord {
        HistoryRecorder::default().record_tick(
            graph,
            records_of(events, &no_spend, due()),
            None,
            &no_spend,
        )
    }

    /// A trigger that refuses every history insert: a failed write, on demand.
    fn refuse_history_writes(dir: &Path, on: bool) {
        let raw = rusqlite::Connection::open(dir.join(".kgraf/graph.db")).expect("raw");
        if on {
            raw.execute_batch(
                "CREATE TRIGGER refuse BEFORE INSERT ON work_schedule_history
                 BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
            )
            .expect("trigger");
        } else {
            raw.execute_batch("DROP TRIGGER refuse;").expect("drop");
        }
    }

    #[test]
    fn a_project_without_a_graph_is_not_given_one() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(open(dir.path()).expect("open").is_none());
        assert!(
            !dir.path().join(".kgraf").exists(),
            "the shell never creates a project's graph"
        );
    }

    #[test]
    fn an_old_meta_schedule_is_moved_onto_a_node_when_the_shell_opens_the_graph() {
        let (dir, graph) = project();
        let def = ScheduleDefinition::one_shot("nightly", "do it", due(), "UTC").expect("valid");
        graph
            .set_meta(
                &kgraf_schedule::schedule_key(&def.id),
                &serde_json::to_string(&def).unwrap(),
            )
            .expect("write row");
        graph
            .set_meta(&kgraf_schedule::schedule_key("broken"), "{not json")
            .expect("write row");
        drop(graph);
        let (graph, moved) = open(dir.path()).expect("open").expect("graph");
        assert_eq!(moved.len(), 2, "{moved:?}");
        let loaded = load_all(&graph).expect("load");
        assert_eq!(loaded[0].definition, def);

        // The refused row is reported once per process, not every tick.
        let mut recorder = HistoryRecorder::default();
        assert_eq!(recorder.migrations_to_report(&moved).len(), 2);
        let (_graph, again) = open(dir.path()).expect("open").expect("graph");
        assert_eq!(again.len(), 1, "only the refused row is found again");
        assert!(recorder.migrations_to_report(&again).is_empty());
    }

    /// Acceptance: a scheduled run's start, outcome and spend land on its node,
    /// and show on the node's label.
    #[test]
    fn a_runs_history_lands_on_the_node_and_its_label() {
        let (_dir, graph) = project();
        nightly(&graph);
        let started = [TickEvent::Started {
            schedule_id: "nightly".into(),
            due_at: due(),
        }];
        assert_eq!(record(&graph, &started).written, 1);
        let nodes = plan_nodes(&views(&graph).expect("views"), &none_disabled);
        assert_eq!(nodes[0].state, WorkState::InFlight);

        let done = [TickEvent::Completed {
            schedule_id: "nightly".into(),
            due_at: due(),
        }];
        let spend = |run: &str| {
            Some(RunSpend {
                micros: 50_000,
                calls: 3,
                unpriced: 0,
                text: format!("$0.05 for {run}"),
            })
        };
        let report = HistoryRecorder::default().record_tick(
            &graph,
            records_of(&done, &spend, due()),
            None,
            &no_spend,
        );
        assert_eq!(report.written, 1);
        let history = graph
            .node_schedule_events("schedules/nightly", 10)
            .expect("history");
        let run = ScheduledRuns::run_id("nightly", due());
        assert_eq!(history[0].event, "completed");
        assert_eq!(history[0].run_id.as_deref(), Some(run.as_str()));
        assert_eq!(history[0].spend.as_ref().map(|s| s.micros), Some(50_000));

        let nodes = plan_nodes(&views(&graph).expect("views"), &none_disabled);
        let node = &nodes[0];
        assert_eq!(node.key, "work:schedules/nightly");
        assert_eq!(node.state, WorkState::Waiting);
        assert!(
            node.label.starts_with("Nightly build: waiting"),
            "{}",
            node.label
        );
        assert!(
            node.label.contains("cron `0 0 3 * * *` in UTC"),
            "{}",
            node.label
        );
        assert!(
            node.label
                .contains("last run completed 2026-09-27 03:00 UTC"),
            "{}",
            node.label
        );
    }

    /// Acceptance item 3, with the claim table deciding: the reason
    /// leads the label once, the state is the distinct disabled state, and the
    /// failed run's detail (the same reason) is not repeated.
    #[test]
    fn a_disable_reason_from_the_claim_table_leads_the_node_label_once() {
        let (_dir, graph) = project();
        nightly(&graph);
        let reason = "missing credential: KIMI_API_KEY is not set";
        let failed = [TickEvent::Failed {
            schedule_id: "nightly".into(),
            due_at: due(),
            detail: reason.into(),
            disabled: true,
        }];
        record(&graph, &failed);
        for _ in 0..3 {
            record(
                &graph,
                &[TickEvent::Disabled {
                    schedule_id: "nightly".into(),
                    reason: reason.into(),
                }],
            );
        }
        let disables = graph
            .schedule_events("nightly", 50)
            .expect("history")
            .into_iter()
            .filter(|e| e.event == "disabled")
            .count();
        assert_eq!(disables, 1, "one row per reason, not one per tick");

        let is_disabled = |_: &str| Some(reason.to_owned());
        let node = &plan_nodes(&views(&graph).expect("views"), &is_disabled)[0];
        assert_eq!(node.state, WorkState::ScheduleDisabled);
        assert!(
            node.label.starts_with(&format!(
                "Nightly build: schedule disabled (disabled: {reason})"
            )),
            "{}",
            node.label
        );
        assert_eq!(node.label.matches(reason).count(), 1, "{}", node.label);
    }

    /// `schedule enable` cleared the claim table, and the `enabled`
    /// history row failed to write. The claim table decides: the node is enabled.
    #[test]
    fn an_enabled_schedule_shows_enabled_even_when_its_enabled_row_was_never_written() {
        let (dir, graph) = project();
        nightly(&graph);
        record(
            &graph,
            &[TickEvent::Disabled {
                schedule_id: "nightly".into(),
                reason: "missing credential".into(),
            }],
        );
        let claims = ScheduleStore::open_in_memory().expect("claims");
        claims.register_schedule("nightly").expect("register");
        claims.enable("nightly").expect("enable");
        refuse_history_writes(dir.path(), true);
        let enabled = OwnedRunRecord {
            schedule_id: "nightly".into(),
            kind: EventKind::Enabled,
            run_id: None,
            due_at: None,
            detail: String::new(),
            spend: None,
            at: due(),
        };
        let report = HistoryRecorder::default().record_tick(&graph, vec![enabled], None, &no_spend);
        assert_eq!(report.errors.len(), 1, "{report:?}");
        let v = views(&graph).expect("views");
        let reasons = disabled_reasons(Some(&claims), &v).expect("reasons");
        let node = &plan_nodes(&v, &|id: &str| reasons.get(id).cloned())[0];
        assert_eq!(node.state, WorkState::Waiting, "{}", node.label);
        assert!(!node.label.contains("disabled"), "{}", node.label);
    }

    /// A row whose write fails is kept and written on a later tick; an
    /// outcome lost outright (a crash) is rebuilt from the claim table.
    #[test]
    fn a_failed_history_write_is_retried_and_a_lost_outcome_is_rebuilt() {
        let (dir, graph) = project();
        nightly(&graph);
        let claims = ScheduleStore::open_in_memory().expect("claims");
        let mut recorder = HistoryRecorder::default();

        refuse_history_writes(dir.path(), true);
        let started = [TickEvent::Started {
            schedule_id: "nightly".into(),
            due_at: due(),
        }];
        let report = recorder.record_tick(
            &graph,
            records_of(&started, &no_spend, due()),
            Some(&claims),
            &no_spend,
        );
        assert_eq!((report.written, report.pending), (0, 1), "{report:?}");
        assert!(!report.errors.is_empty());

        // The occurrence settles in the claim table while the graph still refuses,
        // and the tick that saw it end is lost (a crash).
        let generation = match claims
            .claim_occurrence(
                "nightly",
                due(),
                "a",
                due(),
                std::time::Duration::from_secs(900),
            )
            .expect("claim")
        {
            ClaimOutcome::Claimed { generation, .. } => generation,
            ClaimOutcome::NotClaimed => panic!("claim"),
        };
        let run = ScheduledRuns::run_id("nightly", due()).to_string();
        claims
            .set_run_id("nightly", due(), "a", generation, &run)
            .expect("run id");
        claims
            .record_outcome("nightly", due(), "a", generation, &RunOutcome::Success)
            .expect("outcome");

        refuse_history_writes(dir.path(), false);
        let report = recorder.record_tick(&graph, Vec::new(), Some(&claims), &no_spend);
        assert_eq!(
            (report.written, report.pending, report.rebuilt),
            (1, 0, 1),
            "{report:?}"
        );
        let kinds: Vec<String> = graph
            .schedule_events("nightly", 10)
            .expect("h")
            .into_iter()
            .map(|e| e.event)
            .collect();
        assert_eq!(kinds, ["completed", "started", "added"]);
        let report = recorder.record_tick(&graph, Vec::new(), Some(&claims), &no_spend);
        assert_eq!((report.written, report.rebuilt), (0, 0), "written once");
    }

    fn claim(claims: &ScheduleStore, due: DateTime<Utc>) -> i64 {
        match claims
            .claim_occurrence(
                "nightly",
                due,
                "a",
                due,
                std::time::Duration::from_secs(900),
            )
            .expect("claim")
        {
            ClaimOutcome::Claimed { generation, .. } => generation,
            ClaimOutcome::NotClaimed => panic!("claim"),
        }
    }

    /// Final check items 1 and 5: scheduled failures are read from the graph's
    /// history, so they survive a restart (a fresh connection to the same
    /// file); each failed attempt has its own key; a later start supersedes a
    /// failure and a deferral of another occurrence does not.
    #[test]
    fn scheduled_failures_come_from_the_durable_history() {
        let (dir, graph) = project();
        nightly(&graph);
        let mut recorder = HistoryRecorder::default();
        let no_spend = |_: &str| None;
        let at = |m: i64| due() + chrono::Duration::minutes(m);
        let rec = |kind, run: Option<&str>, detail: &str, m: i64| OwnedRunRecord {
            schedule_id: "nightly".into(),
            kind,
            run_id: run.map(str::to_owned),
            due_at: Some(at(m)),
            detail: detail.into(),
            spend: None,
            at: at(m),
        };
        assert!(failed_runs(&graph).expect("read").is_empty());
        recorder.record_tick(
            &graph,
            vec![
                rec(EventKind::Started, Some("r1"), "", 0),
                rec(EventKind::Failed, Some("r1"), "no route", 0),
            ],
            None,
            &no_spend,
        );
        drop(graph);
        // A new process: the failure is still there.
        let (graph, _) = open(dir.path()).expect("reopen").expect("a graph");
        let first = failed_runs(&graph).expect("read");
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(first[0].0.starts_with("scheduled:nightly#"), "{first:?}");
        assert!(first[0].1.contains("no route") && first[0].1.contains("r1"));
        // The next occurrence is deferred: no run happened, the failure stays.
        recorder.record_tick(
            &graph,
            vec![rec(EventKind::Deferred, None, "at the cap", 60)],
            None,
            &no_spend,
        );
        assert_eq!(failed_runs(&graph).expect("read"), first);
        // It starts: superseded.
        recorder.record_tick(
            &graph,
            vec![rec(EventKind::Started, Some("r2"), "", 60)],
            None,
            &no_spend,
        );
        assert!(failed_runs(&graph).expect("read").is_empty());
        // It fails too: a new card with a new key, so acknowledging the first
        // does not hide it.
        recorder.record_tick(
            &graph,
            vec![rec(EventKind::Failed, Some("r2"), "tests red", 60)],
            None,
            &no_spend,
        );
        let second = failed_runs(&graph).expect("read");
        assert_eq!(second.len(), 1);
        assert_ne!(second[0].0, first[0].0);
    }

    fn row(kind: EventKind, run: Option<&str>, detail: &str) -> OwnedRunRecord {
        OwnedRunRecord {
            schedule_id: "nightly".into(),
            kind,
            run_id: run.map(str::to_owned),
            due_at: Some(due()),
            detail: detail.into(),
            spend: None,
            at: due(),
        }
    }

    /// Verifier item 1: the claim table disabled the schedule and the history has
    /// no disable row at all. The node shows the disabled state with the claim
    /// table's reason; with `disabled_reasons` ignoring the claim table this fails.
    #[test]
    fn a_disable_only_the_claim_table_knows_shows_on_the_node() {
        let (_dir, graph) = project();
        nightly(&graph);
        let claims = ScheduleStore::open_in_memory().expect("claims");
        let generation = claim(&claims, due());
        let reason = kgraf_schedule::DisableReason::MissingCredential {
            detail: "KIMI_API_KEY is not set".into(),
        };
        claims
            .record_outcome(
                "nightly",
                due(),
                "a",
                generation,
                &RunOutcome::Failed(kgraf_schedule::FailureClass::Disabling(reason.clone())),
            )
            .expect("outcome");
        assert!(
            graph
                .schedule_events("nightly", 50)
                .expect("h")
                .iter()
                .all(|e| e.event != "disabled"),
            "no disable row in the history"
        );
        let v = views(&graph).expect("views");
        let reasons = disabled_reasons(Some(&claims), &v).expect("reasons");
        let node = &plan_nodes(&v, &|id: &str| reasons.get(id).cloned())[0];
        assert_eq!(node.state, WorkState::ScheduleDisabled, "{}", node.label);
        assert!(node.label.contains(&reason.to_string()), "{}", node.label);
    }

    /// Verifier item 3: an outcome still waiting in the retry queue is written
    /// from there, with its own error text, not rebuilt from the claim table.
    #[test]
    fn a_queued_outcome_wins_over_a_rebuilt_one() {
        let (dir, graph) = project();
        nightly(&graph);
        let claims = ScheduleStore::open_in_memory().expect("claims");
        let run = ScheduledRuns::run_id("nightly", due()).to_string();
        let generation = claim(&claims, due());
        claims
            .set_run_id("nightly", due(), "a", generation, &run)
            .expect("run id");
        claims
            .record_outcome(
                "nightly",
                due(),
                "a",
                generation,
                &RunOutcome::Failed(kgraf_schedule::FailureClass::Transient {
                    detail: "the gate failed".into(),
                }),
            )
            .expect("outcome");
        let mut recorder = HistoryRecorder::default();
        refuse_history_writes(dir.path(), true);
        let failed = row(EventKind::Failed, Some(&run), "the gate failed: tests red");
        let report = recorder.record_tick(&graph, vec![failed], Some(&claims), &no_spend);
        assert_eq!(report.pending, 1, "{report:?}");
        refuse_history_writes(dir.path(), false);
        let report = recorder.record_tick(&graph, Vec::new(), Some(&claims), &no_spend);
        assert_eq!((report.written, report.rebuilt), (1, 0), "{report:?}");
        let failed: Vec<String> = graph
            .schedule_events("nightly", 10)
            .expect("h")
            .into_iter()
            .filter(|e| e.event == "failed")
            .map(|e| e.detail)
            .collect();
        assert_eq!(failed, ["the gate failed: tests red"]);
    }

    /// Verifier item 4: past the cap, the oldest waiting rows are dropped and the
    /// count is reported as an error (the loop logs errors at error level).
    #[test]
    fn the_retry_queue_is_capped_and_says_how_many_it_dropped() {
        let (dir, graph) = project();
        nightly(&graph);
        refuse_history_writes(dir.path(), true);
        let rows: Vec<OwnedRunRecord> = (0..MAX_PENDING + 5)
            .map(|n| row(EventKind::Started, Some(&format!("r{n}")), ""))
            .collect();
        let mut recorder = HistoryRecorder::default();
        let report = recorder.write(&graph, rows);
        assert_eq!(report.pending, MAX_PENDING);
        assert_eq!(recorder.pending(), MAX_PENDING);
        assert!(
            report.errors.iter().any(|e| e.starts_with(&format!(
                "5 unwritten history row(s) dropped past {MAX_PENDING} waiting"
            ))),
            "{:?}",
            report.errors.last()
        );
        // The oldest were dropped: the first kept row is r5.
        refuse_history_writes(dir.path(), false);
        let report = recorder.write(&graph, Vec::new());
        assert_eq!(report.written, MAX_PENDING);
        assert!(
            !graph
                .has_schedule_event("nightly", "started", "r4")
                .expect("has")
        );
        assert!(
            graph
                .has_schedule_event("nightly", "started", "r5")
                .expect("has")
        );
    }

    /// Verifier item 5: a write pass stops at the first busy graph. Three waiting
    /// rows cost one busy timeout, not three, and all stay queued.
    #[test]
    fn a_busy_graph_stops_the_write_pass_at_the_first_row() {
        let (dir, graph) = project();
        nightly(&graph);
        let holder =
            rusqlite::Connection::open(dir.path().join(".kgraf/graph.db")).expect("holder");
        holder
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("hold the write lock");
        let rows = (0..3)
            .map(|n| row(EventKind::Started, Some(&format!("r{n}")), ""))
            .collect();
        let mut recorder = HistoryRecorder::default();
        let started = std::time::Instant::now();
        let report = recorder.write(&graph, rows);
        let waited = started.elapsed();
        holder.execute_batch("ROLLBACK;").expect("release");
        assert!(report.stopped_busy, "{report:?}");
        assert_eq!((report.written, report.pending), (0, 3), "{report:?}");
        assert_eq!(report.errors.len(), 1, "one attempt, then stop: {report:?}");
        assert!(
            waited < std::time::Duration::from_secs(9),
            "one busy timeout (5 s), not three: {waited:?}"
        );
        let report = recorder.write(&graph, Vec::new());
        assert_eq!((report.written, report.pending), (3, 0), "{report:?}");
    }

    #[test]
    fn a_waiting_deferral_is_recorded_once_and_process_wide_events_not_at_all() {
        let (_dir, graph) = project();
        nightly(&graph);
        let events: Vec<TickEvent> = (1..=3)
            .map(|n| TickEvent::Deferred {
                schedule_id: "nightly".into(),
                due_at: due(),
                detail: "over the cap".into(),
                deferred_count: n,
            })
            .chain([TickEvent::NotRunnable {
                schedule_id: "nightly".into(),
                detail: "no run.json".into(),
            }])
            .collect();
        record(&graph, &events);
        let kinds: Vec<String> = graph
            .schedule_events("nightly", 50)
            .expect("h")
            .into_iter()
            .map(|e| e.event)
            .collect();
        assert_eq!(kinds, ["deferred", "added"]);
    }

    #[test]
    fn an_event_for_an_unknown_schedule_is_dropped_and_said_not_retried() {
        let (_dir, graph) = project();
        let mut recorder = HistoryRecorder::default();
        let events = [TickEvent::Started {
            schedule_id: "ghost".into(),
            due_at: due(),
        }];
        let report = recorder.record_tick(
            &graph,
            records_of(&events, &no_spend, due()),
            None,
            &no_spend,
        );
        assert_eq!(report.dropped.len(), 1, "{report:?}");
        assert_eq!(recorder.pending(), 0);
    }

    /// Each label part is cut; a long title or reason cannot push the
    /// schedule out of the label.
    #[test]
    fn long_label_parts_are_cut() {
        let (_dir, graph) = project();
        nightly(&graph);
        let long = "x".repeat(500);
        let node = &plan_nodes(&views(&graph).expect("views"), &|_: &str| {
            Some(long.clone())
        })[0];
        let head = node.label.split(" | ").next().expect("head");
        assert!(head.chars().count() < 2 * MAX_LABEL_PART, "{}", head.len());
        assert!(node.label.contains("| schedule nightly:"), "{}", node.label);
    }
}
