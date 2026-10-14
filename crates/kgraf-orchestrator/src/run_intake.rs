//! Input taken in outside the run loop (decision 0100, section 4).
//!
//! A line from the box or stdin is appended to the graph's `run_intake` table
//! by the port's reader task, on a connection of its own, and acknowledged as
//! soon as the row is committed: the acknowledgment never waits for the loop,
//! whatever the loop is doing (a planner call, a merge, a save). The loop folds
//! the rows past the last one it folded into the run record at its next step,
//! and keeps that row id in the record, so the fold and the record are one
//! save: a kill folds a row once or not at all. A status question and the
//! questions waiting for the user are read from the snapshot the loop
//! publishes ([`LoopSnapshot`]), never asked of the loop.
//!
//! ```text
//!   line --reader: append, commit--> acknowledged --wake--> loop folds at its next step
//!                                                           (row id saved with the record)
//!   no loop (ended, killed) --> acknowledged as waiting --> folded by the next resume or reopen
//! ```

use crate::asks::PendingQuestionView;
use crate::orchestrator::StatusReport;
use crate::run_state::RunStatus;
use crate::store::SharedGraph;
use kgraf_core::RunId;
use kgraf_graph::{IntakeRow, IntakeWriter, NewIntakeRow};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::watch;

/// What an intake row is. Routed when it is appended, so a later reader can
/// address it without reading its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntakeKind {
    /// Information for the run (a box or stdin line).
    Note,
    /// A change to the plan.
    Change,
    /// A message to one work node.
    NodeMessage,
    /// The user's answer to a question or card.
    Answer,
    /// A planner call's answer, taken in by the loop.
    PlannerAnswer,
}

impl IntakeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Change => "change",
            Self::NodeMessage => "node_message",
            Self::Answer => "answer",
            Self::PlannerAnswer => "planner_answer",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "note" => Self::Note,
            "change" => Self::Change,
            "node_message" => Self::NodeMessage,
            "answer" => Self::Answer,
            "planner_answer" => Self::PlannerAnswer,
            _ => return None,
        })
    }
}

/// The target of a row that addresses the whole run.
pub const RUN_TARGET: &str = "run";

/// The acknowledgment of a note the live loop will fold (unchanged from the
/// reply the loop itself gave before intake existed).
pub const NOTED: &str = "noted, as information for the run";
/// The acknowledgment of a note for a completed run.
pub const NOTED_COMPLETED: &str =
    "noted; the run is complete, so this waits until the run is reopened";
/// The acknowledgment of a note for a run no loop is running.
pub const NOTED_NOT_RUNNING: &str =
    "noted; the run is not running, so this waits until the run is resumed";

/// What the loop last published: read by a status question, the box and the
/// shell without asking the loop.
#[derive(Clone, Debug, Default)]
pub struct LoopSnapshot {
    /// The run the loop serves, set as the run is started or resumed.
    pub run: Option<RunId>,
    /// A loop is running this run now.
    pub live: bool,
    /// The run's status at the last publish.
    pub status: Option<StatusReport>,
    /// Questions waiting for the user at the last publish.
    pub questions: Vec<PendingQuestionView>,
    /// The planner call the loop is waiting on, if any (`decompose`,
    /// `interfaces`, a change's delta).
    pub planning: Option<String>,
    /// When it was published, Unix ms.
    pub at_ms: i64,
}

/// How the reader acknowledges a row, from the snapshot.
pub(crate) fn ack_text(snapshot: &LoopSnapshot, loop_gone: bool) -> &'static str {
    let completed = snapshot
        .status
        .as_ref()
        .is_some_and(|s| s.status == RunStatus::Completed);
    if completed {
        NOTED_COMPLETED
    } else if loop_gone || !snapshot.live {
        NOTED_NOT_RUNNING
    } else {
        NOTED
    }
}

/// A status answer from the snapshot.
pub(crate) fn status_text(snapshot: &LoopSnapshot) -> String {
    let mut text = match (&snapshot.status, &snapshot.run) {
        (Some(s), _) => s.text.clone(),
        (None, Some(run)) => format!("run {run}: starting"),
        (None, None) => "no run has started yet".to_owned(),
    };
    if let Some(what) = &snapshot.planning {
        text.push_str(&format!("\nplanning: the {what} call is running"));
    }
    if !snapshot.live && snapshot.run.is_some() {
        text.push_str("\nthe run is not running now");
    }
    text
}

/// One acknowledgment the reader measured, for the loop's `latency` event.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AckTiming {
    pub row: i64,
    /// Receipt by the reader to the acknowledgment sent (B1).
    pub ack_us: i64,
    /// When the row was committed, for the fold latency.
    pub committed: std::time::Instant,
}

/// Where rows are appended: a connection of its own to the graph's file, or
/// the shared store when the graph has no file (in memory: tests and a dry
/// run), which no second connection can open.
enum Sink {
    File(Mutex<IntakeWriter>),
    Shared(SharedGraph),
}

/// An owned row to append.
#[derive(Clone, Debug)]
pub(crate) struct Row {
    pub run: String,
    pub kind: IntakeKind,
    pub target: String,
    pub channel: String,
    pub message_id: String,
    pub received_ms: i64,
    pub body: String,
}

/// Shared by the orchestrator, its handle, the port readers, the planner
/// tasks and the loop.
pub(crate) struct Intake {
    graph: SharedGraph,
    /// The graph's file (`None` in memory), read when the intake is made.
    db_path: Result<Option<std::path::PathBuf>, String>,
    sink: OnceLock<Result<Sink, String>>,
    snapshot: watch::Sender<LoopSnapshot>,
    acks: Mutex<Vec<AckTiming>>,
    /// When the run's stop was asked for, for the stop latency of a call it
    /// cut.
    stop_at: Mutex<Option<std::time::Instant>>,
}

impl std::fmt::Debug for Intake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Intake").finish_non_exhaustive()
    }
}

impl Intake {
    pub(crate) fn new(graph: SharedGraph) -> Arc<Self> {
        // The file is read once, here, so an append never takes the shared
        // store's lock, which the loop holds while it saves.
        let db_path = graph
            .lock()
            .map(|g| g.db_path())
            .map_err(|_| "the graph lock is poisoned".to_owned());
        Arc::new(Self {
            graph,
            db_path,
            sink: OnceLock::new(),
            snapshot: watch::Sender::new(LoopSnapshot::default()),
            acks: Mutex::new(Vec::new()),
            stop_at: Mutex::new(None),
        })
    }

    fn sink(&self) -> Result<&Sink, String> {
        self.sink
            .get_or_init(|| match self.db_path.clone()? {
                Some(p) => IntakeWriter::open(&p)
                    .map(|w| Sink::File(Mutex::new(w)))
                    .map_err(|e| format!("the intake table could not be opened: {e}")),
                None => Ok(Sink::Shared(self.graph.clone())),
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Append `row`; blocking (SQLite), so called off the async workers.
    /// Returns once the row is committed.
    pub(crate) fn append_blocking(&self, row: &Row) -> Result<i64, String> {
        let new = NewIntakeRow {
            run: &row.run,
            kind: row.kind.as_str(),
            target: &row.target,
            channel: &row.channel,
            message_id: &row.message_id,
            received_ms: row.received_ms,
            body: &row.body,
        };
        match self.sink()? {
            Sink::File(w) => w
                .lock()
                .map_err(|_| "the intake writer lock is poisoned".to_owned())?
                .append(&new),
            Sink::Shared(g) => g
                .lock()
                .map_err(|_| "the graph lock is poisoned".to_owned())?
                .append_intake(&new),
        }
        .map_err(|e| format!("the intake row could not be written: {e}"))
    }

    /// [`Self::append_blocking`] on a blocking thread.
    pub(crate) async fn append(self: &Arc<Self>, row: Row) -> Result<i64, String> {
        let me = self.clone();
        tokio::task::spawn_blocking(move || me.append_blocking(&row))
            .await
            .map_err(|e| format!("the intake append panicked: {e}"))?
    }

    /// `run`'s rows past `after`, read through the shared store (the loop's
    /// own connection: it is the one writer of the record they fold into).
    pub(crate) fn rows_after(&self, run: &str, after: i64) -> Result<Vec<IntakeRow>, String> {
        self.graph
            .lock()
            .map_err(|_| "the graph lock is poisoned".to_owned())?
            .intake_after(run, after)
            .map_err(|e| format!("the intake table could not be read: {e}"))
    }

    pub(crate) fn snapshot(&self) -> LoopSnapshot {
        self.snapshot.borrow().clone()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<LoopSnapshot> {
        self.snapshot.subscribe()
    }

    pub(crate) fn publish(&self, f: impl FnOnce(&mut LoopSnapshot)) {
        self.snapshot.send_modify(f);
    }

    pub(crate) fn note_ack(&self, timing: AckTiming) {
        if let Ok(mut acks) = self.acks.lock() {
            acks.push(timing);
        }
    }

    /// The run's stop was asked for now (the first time counts).
    pub(crate) fn note_stop(&self) {
        if let Ok(mut at) = self.stop_at.lock() {
            at.get_or_insert_with(std::time::Instant::now);
        }
    }

    /// When the run's stop was first asked for.
    pub(crate) fn stopped_at(&self) -> Option<std::time::Instant> {
        self.stop_at.lock().ok().and_then(|a| *a)
    }

    pub(crate) fn has_acks(&self) -> bool {
        self.acks.lock().map(|a| !a.is_empty()).unwrap_or(false)
    }

    pub(crate) fn take_acks(&self) -> Vec<AckTiming> {
        self.acks
            .lock()
            .map(|mut a| std::mem::take(&mut *a))
            .unwrap_or_default()
    }
}
