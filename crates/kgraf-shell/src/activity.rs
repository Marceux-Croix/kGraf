//! The live activity card's rows (decision 0063): the graph change log's newest
//! events, newest first, each routing back to the node it touched.
//!
//! The card follows 0063 exactly:
//! - Its rows are change-log events and nothing else
//!   ([`kgraf_graph::EventTail::newest_of`] reads the log's own rows, of the tables
//!   in [`SHOWN`]). It adds no event source and stores no state.
//! - A row is a time, who (the agent, or the indexer), and one line of text.
//! - There are no counts, no status filters, no grouping and no actions. Rows are in
//!   time order only.
//! - Selecting a row selects its node ([`route`]). A row whose event names no node
//!   the graph draws is shown without a link; that is a gap in the trigger's
//!   payload, not something the card papers over.
//!
//! A thread per shown project tails the log ([`ActivityWatch`]): it reads the
//! watermark every [`POLL`] and the newest [`ROWS`] rows only when it moved, so an
//! idle project costs one indexed `MAX(seq)` read per poll and no UI frame.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use kgraf_galaxy::ProjectModel;
use kgraf_graph::snapshot::{ProjectIndex, open_existing};
use kgraf_graph::{Change, ChangeOp, EventTail};

/// How often the watch reads the log's watermark.
pub const POLL: Duration = Duration::from_millis(500);
/// Rows the card holds: bounded like the text surface, so a first index's tens of
/// thousands of rows cost the card what a hundred do.
pub const ROWS: usize = 100;

/// What an event touched, for the route back to the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A code node, by graph id.
    Node(String),
    /// An agent (a lease holder).
    Agent(String),
    /// A work node or a plan key.
    Work(String),
    /// A file, by repository path.
    Path(String),
    /// Nothing the graph draws.
    None,
}

/// One row of the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    pub seq: i64,
    /// Milliseconds since the epoch (the change log's own time).
    pub ts_ms: i64,
    /// The agent, or `indexer` or `plan` for events no agent made.
    pub who: String,
    /// One line.
    pub text: String,
    pub target: Target,
}

fn field<'a>(c: &'a Change, key: &str) -> Option<&'a str> {
    c.payload.as_ref()?.get(key)?.as_str()
}

fn verb(
    op: ChangeOp,
    insert: &'static str,
    update: &'static str,
    delete: &'static str,
) -> &'static str {
    match op {
        ChangeOp::Insert => insert,
        ChangeOp::Update => update,
        ChangeOp::Delete => delete,
    }
}

/// One change as a row. Pure.
pub fn describe(c: &Change) -> ActivityRow {
    let (who, text, target) = match c.table.as_str() {
        "files" => {
            let path = field(c, "path").unwrap_or(&c.row_id).to_owned();
            let v = verb(c.op, "indexed", "reindexed", "removed");
            (
                "indexer".to_owned(),
                format!("{v} {path}"),
                Target::Path(path),
            )
        }
        "nodes" => {
            let kind = field(c, "kind").unwrap_or("node");
            let name = field(c, "qualified_name").unwrap_or(&c.row_id);
            let v = verb(c.op, "added", "changed", "removed");
            let target = if c.op == ChangeOp::Delete {
                Target::None
            } else {
                Target::Node(c.row_id.clone())
            };
            ("indexer".to_owned(), format!("{v} {kind} {name}"), target)
        }
        "layout" => (
            "indexer".to_owned(),
            "moved in the layout".to_owned(),
            Target::Node(c.row_id.clone()),
        ),
        "edges" => {
            let kind = field(c, "kind").unwrap_or("an");
            let v = verb(c.op, "added", "changed", "removed");
            let target = field(c, "src").map_or(Target::None, |s| Target::Node(s.to_owned()));
            ("indexer".to_owned(), format!("{v} {kind} edge"), target)
        }
        "leases" => {
            let agent = field(c, "agent_id").unwrap_or("an agent").to_owned();
            let v = verb(c.op, "took a lease on", "renewed its lease on", "released");
            (
                agent.clone(),
                format!("{v} {}", c.row_id),
                Target::Agent(agent),
            )
        }
        "node_leases" => {
            let agent = field(c, "agent_id").unwrap_or("an agent").to_owned();
            let v = verb(c.op, "took a lease on", "renewed its lease on", "released");
            (agent, format!("{v} a node"), Target::Node(c.row_id.clone()))
        }
        "work_nodes" => {
            let kind = field(c, "kind").unwrap_or("work");
            let title = field(c, "title").unwrap_or(&c.row_id);
            let v = verb(c.op, "planned", "changed", "removed");
            (
                "plan".to_owned(),
                format!("{v} {kind} {title}"),
                Target::Work(c.row_id.clone()),
            )
        }
        "work_deps" => {
            let src = field(c, "src").unwrap_or("?");
            let dst = field(c, "dst").unwrap_or("?");
            (
                "plan".to_owned(),
                format!("{src} waits on {dst}"),
                Target::Work(src.to_owned()),
            )
        }
        "work_schedules" => {
            let node = field(c, "node_id").unwrap_or("?").to_owned();
            let v = verb(c.op, "scheduled", "rescheduled", "unscheduled");
            ("plan".to_owned(), format!("{v} {node}"), Target::Work(node))
        }
        "work_schedule_history" => {
            let node = field(c, "node_id").unwrap_or("?").to_owned();
            let event = field(c, "event").unwrap_or("event");
            let run = field(c, "run_id")
                .map(|r| format!(" (run {r})"))
                .unwrap_or_default();
            (
                "scheduler".to_owned(),
                format!("{event} on {node}{run}"),
                Target::Work(node),
            )
        }
        "work_presumed" => {
            // Decision 0064: a node presumed from the roadmap; an update records
            // that its source stopped listing it or that real work adopted it.
            let node = c.row_id.clone();
            let text = match c.op {
                ChangeOp::Insert => {
                    let section = field(c, "section").unwrap_or("the roadmap");
                    format!("presumed {node} from {section}")
                }
                ChangeOp::Update => match field(c, "adopted_by") {
                    Some(by) => format!("{node} adopted by {by}"),
                    None => format!("{node} no longer listed in its source"),
                },
                ChangeOp::Delete => format!("removed presumed {node}"),
            };
            ("roadmap".to_owned(), text, Target::Work(node))
        }
        other => (
            "graph".to_owned(),
            format!("{} {other} {}", c.op.as_str(), c.row_id),
            Target::None,
        ),
    };
    ActivityRow {
        seq: c.seq,
        ts_ms: c.ts,
        who,
        text,
        target,
    }
}

/// The events the card shows: what agents, the plan and the
/// scheduler did, and files coming and going. Index churn (layout moves, edges,
/// node inserts and deletes, file updates) and lease renewals are left out, in
/// SQL, before the row limit, so they never fill the tail.
pub const SHOWN: [(&str, &[ChangeOp]); 8] = [
    ("leases", &[ChangeOp::Insert, ChangeOp::Delete]),
    ("node_leases", &[ChangeOp::Insert, ChangeOp::Delete]),
    ("work_nodes", &[]),
    ("work_deps", &[]),
    ("work_schedules", &[]),
    ("work_schedule_history", &[]),
    ("work_presumed", &[]),
    ("files", &[ChangeOp::Insert, ChangeOp::Delete]),
];

/// Each row's slot on the graph, computed once per set of rows and model
/// generation (not per frame: a file row would otherwise scan every code node).
pub fn routes(rows: &[ActivityRow], model: &ProjectModel) -> Vec<Option<u32>> {
    let mut by_path: Option<std::collections::HashMap<&str, u32>> = None;
    rows.iter()
        .map(|r| match &r.target {
            Target::Path(path) => by_path
                .get_or_insert_with(|| {
                    let mut m = std::collections::HashMap::new();
                    for (slot, _, p, _, _) in model.code_nodes() {
                        m.entry(p).or_insert(slot);
                    }
                    m
                })
                .get(path.as_str())
                .copied(),
            _ => route(r, model),
        })
        .collect()
}

/// The slot a row selects on the graph, if the graph draws what it touched.
pub fn route(row: &ActivityRow, model: &ProjectModel) -> Option<u32> {
    match &row.target {
        Target::Node(id) => model.slot_of(id),
        Target::Agent(agent) => model.agent_slot(agent),
        Target::Work(key) => model
            .task_slot(key)
            .or_else(|| {
                model
                    .plan_nodes()
                    .find(|(n, _)| n.key == *key || n.key.strip_prefix("work:") == Some(key))
                    .map(|(_, slot)| slot)
            })
            .or_else(|| model.slot_of(key)),
        // The file's first code node: a file is drawn through its symbols.
        Target::Path(path) => model
            .code_nodes()
            .find(|(_, _, p, _, _)| *p == path)
            .map(|(slot, ..)| slot),
        Target::None => None,
    }
}

/// What the watch reports.
#[derive(Debug, Clone)]
pub enum ActivityUpdate {
    /// The newest rows, newest first.
    Rows(Arc<Vec<ActivityRow>>),
    /// The log cannot be read (no index yet, another schema, a read error),
    /// said until it can.
    Unavailable(String),
}

/// A thread tailing one project's change log for the card. Dropping it stops and
/// joins the thread.
pub struct ActivityWatch {
    rx: Receiver<ActivityUpdate>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ActivityWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityWatch").finish_non_exhaustive()
    }
}

impl Drop for ActivityWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

impl ActivityWatch {
    /// Tails `root`'s change log every `poll`; `wake` runs after each update.
    pub fn spawn(
        root: PathBuf,
        poll: Duration,
        wake: impl Fn() + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("activity-watch".into())
            .spawn(move || {
                // Two keys: the log's watermark gates the query (an idle log
                // costs one `MAX(seq)` read), and the newest shown row gates the
                // update (a reindex moves the watermark but shows nothing new, and
                // wakes no frame).
                let mut last: Option<Result<i64, String>> = None;
                let mut watermark: Option<i64> = None;
                let mut store = None;
                while !flag.load(Ordering::SeqCst) {
                    let now = read(&root, &mut store, &mut watermark);
                    let update = match (&last, now) {
                        (_, Ok(None)) => None,
                        (Some(Ok(a)), Ok(Some((b, _)))) if *a == b => None,
                        (Some(Err(a)), Err(b)) if *a == b => None,
                        (_, Ok(Some((k, rows)))) => {
                            Some((Ok(k), ActivityUpdate::Rows(Arc::new(rows))))
                        }
                        (_, Err(e)) => Some((Err(e.clone()), ActivityUpdate::Unavailable(e))),
                    };
                    if let Some((key, update)) = update {
                        last = Some(key);
                        if tx.send(update).is_err() {
                            return;
                        }
                        wake();
                    }
                    std::thread::park_timeout(poll);
                }
            })?;
        Ok(Self {
            rx,
            stop,
            thread: Some(thread),
        })
    }

    /// The newest update, if one arrived; older ones are superseded.
    pub fn latest(&self) -> Option<ActivityUpdate> {
        self.rx.try_iter().last()
    }
}

/// The log's watermark and, when asked, its newest rows. The store is kept open
/// between polls and reopened after an error.
fn read(
    root: &Path,
    store: &mut Option<kgraf_graph::GraphStore>,
    watermark: &mut Option<i64>,
) -> Result<Option<(i64, Vec<ActivityRow>)>, String> {
    if store.is_none() {
        *watermark = None;
        *store = match open_existing(root).map_err(|e| e.to_string())? {
            ProjectIndex::Ready(s) => Some(s),
            ProjectIndex::Missing { db_path } => {
                return Err(format!("no index at {}", db_path.display()));
            }
            ProjectIndex::OtherSchema {
                found, expected, ..
            } => {
                return Err(format!(
                    "the index has schema {found:?}; this build reads {expected}"
                ));
            }
        };
    }
    let Some(s) = store.as_ref() else {
        return Err("the index is not open".to_owned());
    };
    let result = s
        .change_watermark()
        .and_then(|w| {
            if *watermark == Some(w) {
                return Ok(None);
            }
            *watermark = Some(w);
            let rows = EventTail::newest_of(s, ROWS, &SHOWN)?;
            let key = rows.first().map_or(0, |c| c.seq);
            Ok(Some((key, rows.iter().map(describe).collect())))
        })
        .map_err(|e| e.to_string());
    if result.is_err() {
        *store = None;
    }
    result
}

/// `HH:MM:SS` in local time.
pub fn clock(ts_ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ts_ms)
        .single()
        .map_or_else(
            || "--:--:--".to_owned(),
            |t| t.format("%H:%M:%S").to_string(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(table: &str, op: ChangeOp, row: &str, payload: serde_json::Value) -> Change {
        Change {
            seq: 1,
            ts: 0,
            table: table.into(),
            op,
            row_id: row.into(),
            payload: Some(payload),
        }
    }

    #[test]
    fn a_lease_row_names_its_agent_and_routes_to_it() {
        let r = describe(&change(
            "leases",
            ChangeOp::Insert,
            "src/a.rs",
            serde_json::json!({"agent_id": "run/w1@1", "task_id": "run/a"}),
        ));
        assert_eq!(r.who, "run/w1@1");
        assert_eq!(r.text, "took a lease on src/a.rs");
        assert_eq!(r.target, Target::Agent("run/w1@1".into()));
    }

    #[test]
    fn a_removed_node_has_no_route() {
        let r = describe(&change(
            "nodes",
            ChangeOp::Delete,
            "n1",
            serde_json::json!({"kind": "function", "qualified_name": "a::b"}),
        ));
        assert_eq!(r.text, "removed function a::b");
        assert_eq!(r.target, Target::None);
    }
}
