//! Schedules on work nodes, and their run history (decision 0052 section 3).
//!
//! A schedule is a property of a work node, never a job list. Each schedule row names
//! the work node it belongs to, and each scheduled run appends history rows against
//! that node: claimed, started, the outcome, and the spend by run id. The galaxy reads
//! these rows to put the schedule, its last run and a disable reason on the node's
//! label (no new panel).
//!
//! **Facts, like `work.rs`.** The graph stores the definition as opaque JSON; the
//! scheduling crate owns its shape (`kgraf_schedule::ScheduleDefinition`). History
//! rows are append-only: a row records what happened at an instant and is never
//! updated or deleted. Current state (disabled or not, the last run) is derived at
//! read time from the newest rows, never stored as a status column.
//!
//! **Not derived data.** A schedule came from the user, and its history records paid
//! runs, so neither can be rebuilt from source. Like the work tables, both tables are
//! absent from the schema-change drop list, and a schema rebuild keeps them.
//!
//! **What is removed.** `kgraf schedule remove` removes the schedule row (the
//! property). The node and every history row stay: the node's past runs remain
//! readable after the schedule that fired them is gone.
//!
//! **What stays elsewhere.** The claim table, its fencing and its disable state live
//! in `.kgraf/schedule.db` (`kgraf_schedule::ScheduleStore`), unchanged. The
//! history here is the record the graph shows; the claim table is what decides.

use crate::error::{GraphError, Result};
use crate::store::GraphStore;
use rusqlite::{OptionalExtension, params};

/// The plan id of the work nodes `kgraf schedule add` creates when no `--node`
/// is given: one plan root, [`SCHEDULE_PLAN_ID`] itself, with one item per schedule
/// under it. It is not an interpretation of a corpus, so an interpretation pass
/// leaves it out of its prior plans.
pub const SCHEDULE_PLAN_ID: &str = "schedules";

/// One schedule, attached to one work node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkSchedule {
    /// The schedule id: the claim table's key, unique across the project.
    pub id: String,
    /// The work node this schedule is a property of.
    pub node_id: String,
    /// The definition as JSON. The graph stores it; it does not interpret it.
    pub definition: String,
    /// Milliseconds since the epoch.
    pub created_ms: i64,
}

/// One history row: something that happened to a schedule's run, on its node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleEvent {
    /// Append order, increasing.
    pub seq: i64,
    pub schedule_id: String,
    pub node_id: String,
    /// What happened (`kgraf_schedule::node_store::EventKind` names these).
    pub event: String,
    /// The run this row is about, when there is one.
    pub run_id: Option<String>,
    /// The occurrence's due instant, in milliseconds since the epoch.
    pub due_ms: Option<i64>,
    /// When this row was recorded, in milliseconds since the epoch.
    pub at_ms: i64,
    /// A reason or an error text, verbatim. Empty when there is none.
    pub detail: String,
    /// The run's spend as the recorder read it from the ledger by run id.
    pub spend: Option<RunSpend>,
}

/// A run's spend as recorded on its node: numbers a caller can sum,
/// and the text the label shows. Read from the ledger by run id, so an occurrence
/// that was reclaimed after an abandoned attempt carries the spend of both attempts:
/// they share the run id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSpend {
    /// Settled, priced spend in micro-dollars.
    pub micros: i64,
    /// Calls attributed to the run.
    pub calls: i64,
    /// Calls whose spend is not known (in flight, unreconciled or unpriced), not in
    /// `micros`.
    pub unpriced: i64,
    /// As shown on the label.
    pub text: String,
}

/// A history row to append.
#[derive(Clone, Debug)]
pub struct NewScheduleEvent<'a> {
    pub schedule_id: &'a str,
    pub node_id: &'a str,
    pub event: &'a str,
    pub run_id: Option<&'a str>,
    pub due_ms: Option<i64>,
    pub at_ms: i64,
    pub detail: &'a str,
    pub spend: Option<&'a RunSpend>,
}

fn schedule_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkSchedule> {
    Ok(WorkSchedule {
        id: row.get(0)?,
        node_id: row.get(1)?,
        definition: row.get(2)?,
        created_ms: row.get(3)?,
    })
}

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduleEvent> {
    Ok(ScheduleEvent {
        seq: row.get(0)?,
        schedule_id: row.get(1)?,
        node_id: row.get(2)?,
        event: row.get(3)?,
        run_id: row.get(4)?,
        due_ms: row.get(5)?,
        at_ms: row.get(6)?,
        detail: row.get(7)?,
        spend: match (row.get(8)?, row.get(9)?, row.get(10)?, row.get(11)?) {
            (Some(micros), Some(calls), Some(unpriced), Some(text)) => Some(RunSpend {
                micros,
                calls,
                unpriced,
                text,
            }),
            _ => None,
        },
    })
}

const SCHEDULE_COLUMNS: &str = "id, node_id, definition, created_ms";
const EVENT_COLUMNS: &str = "seq, schedule_id, node_id, event, run_id, due_ms, at_ms, detail, \
     spend_micros, spend_calls, spend_unpriced, spend_text";

impl GraphStore {
    /// Attach a new schedule to its work node. Refused when the id is empty, when
    /// the node is not stored, or when a schedule with this id already exists (a
    /// schedule is replaced by removing it first, never overwritten in place).
    pub fn add_schedule(&self, schedule: &WorkSchedule) -> Result<()> {
        let invalid = |reason: String| GraphError::InvalidWork { reason };
        if schedule.id.trim().is_empty() {
            return Err(invalid("a schedule has an empty id".into()));
        }
        let tx = self.conn().unchecked_transaction()?;
        let node: Option<String> = tx
            .query_row(
                "SELECT id FROM work_nodes WHERE id = ?1",
                params![schedule.node_id],
                |r| r.get(0),
            )
            .optional()?;
        if node.is_none() {
            return Err(invalid(format!(
                "schedule `{}` names work node `{}`, which is not stored",
                schedule.id, schedule.node_id
            )));
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT node_id FROM work_schedules WHERE id = ?1",
                params![schedule.id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(on) = existing {
            return Err(invalid(format!(
                "a schedule named `{}` already exists, on work node `{on}`",
                schedule.id
            )));
        }
        tx.execute(
            "INSERT INTO work_schedules(id, node_id, definition, created_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                schedule.id,
                schedule.node_id,
                schedule.definition,
                schedule.created_ms
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// One schedule by id.
    pub fn schedule(&self, id: &str) -> Result<Option<WorkSchedule>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {SCHEDULE_COLUMNS} FROM work_schedules WHERE id = ?1"),
                params![id],
                schedule_from_row,
            )
            .optional()?)
    }

    /// Every schedule, by id.
    pub fn schedules(&self) -> Result<Vec<WorkSchedule>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {SCHEDULE_COLUMNS} FROM work_schedules ORDER BY id"
        ))?;
        let rows = stmt.query_map([], schedule_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Detach a schedule from its node. Returns whether one was stored. The node and
    /// the schedule's history rows stay.
    pub fn remove_schedule(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM work_schedules WHERE id = ?1", params![id])?
            > 0)
    }

    /// Append one history row. Refused when the node is not stored (the foreign
    /// key); the schedule row itself need not exist any more, so a run that ends
    /// after its schedule was removed is still recorded on its node.
    ///
    /// A row with a run id is unique per `(schedule_id, event, run_id)` (a partial
    /// unique index): a second writer of the same run event, such as a second app
    /// instance, is ignored inside SQLite, not after a check another process could
    /// race. Returns the new row's `seq`, or `None` when such a row already exists.
    pub fn append_schedule_event(&self, event: &NewScheduleEvent<'_>) -> Result<Option<i64>> {
        let node: Option<String> = self
            .conn()
            .query_row(
                "SELECT id FROM work_nodes WHERE id = ?1",
                params![event.node_id],
                |r| r.get(0),
            )
            .optional()?;
        if node.is_none() {
            return Err(GraphError::InvalidWork {
                reason: format!(
                    "history for schedule `{}` names work node `{}`, which is not stored",
                    event.schedule_id, event.node_id
                ),
            });
        }
        let inserted = self.conn().execute(
            "INSERT OR IGNORE INTO work_schedule_history
                (schedule_id, node_id, event, run_id, due_ms, at_ms, detail,
                 spend_micros, spend_calls, spend_unpriced, spend_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                event.schedule_id,
                event.node_id,
                event.event,
                event.run_id,
                event.due_ms,
                event.at_ms,
                event.detail,
                event.spend.map(|s| s.micros),
                event.spend.map(|s| s.calls),
                event.spend.map(|s| s.unpriced),
                event.spend.map(|s| s.text.as_str())
            ],
        )?;
        Ok((inserted > 0).then(|| self.conn().last_insert_rowid()))
    }

    /// A schedule's newest history rows, newest first, at most `limit`.
    pub fn schedule_events(&self, schedule_id: &str, limit: usize) -> Result<Vec<ScheduleEvent>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM work_schedule_history
             WHERE schedule_id = ?1 ORDER BY seq DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![schedule_id, limit as i64], event_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A work node's newest history rows across every schedule it has had, newest
    /// first, at most `limit`.
    pub fn node_schedule_events(&self, node_id: &str, limit: usize) -> Result<Vec<ScheduleEvent>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM work_schedule_history
             WHERE node_id = ?1 ORDER BY seq DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![node_id, limit as i64], event_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether a row with this schedule, event and run id exists: a recorder uses
    /// it to write each run event once, however often it is retried or rebuilt.
    pub fn has_schedule_event(&self, schedule_id: &str, event: &str, run_id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM work_schedule_history
                 WHERE schedule_id = ?1 AND event = ?2 AND run_id = ?3 LIMIT 1",
                params![schedule_id, event, run_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// The run ids with a row of one of `events` for this schedule.
    pub fn schedule_run_ids_with(
        &self,
        schedule_id: &str,
        events: &[&str],
    ) -> Result<std::collections::BTreeSet<String>> {
        let wanted = serde_json::to_string(events).map_err(|e| GraphError::InvalidWork {
            reason: format!("cannot encode the event names: {e}"),
        })?;
        let mut stmt = self.conn().prepare(
            "SELECT DISTINCT run_id FROM work_schedule_history
             WHERE schedule_id = ?1 AND run_id IS NOT NULL
               AND event IN (SELECT value FROM json_each(?2))",
        )?;
        let rows = stmt.query_map(params![schedule_id, wanted], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether any schedule's history was ever recorded on this node.
    pub fn node_has_schedule_history(&self, node_id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM work_schedule_history WHERE node_id = ?1 LIMIT 1",
                params![node_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// A schedule's newest run row whose event is one of `events`, picked by run
    /// time: the occurrence's due instant (`due_ms`, or `at_ms` for a manual run,
    /// which has none), then the time the row records (so an outcome follows its
    /// start), then append order last. A row rebuilt later from the claim table, or
    /// written late from the retry queue, therefore sorts as the run it describes,
    /// never as newer than a run that came after it.
    pub fn last_schedule_run(
        &self,
        schedule_id: &str,
        events: &[&str],
    ) -> Result<Option<ScheduleEvent>> {
        if events.is_empty() {
            return Ok(None);
        }
        let wanted = serde_json::to_string(events).map_err(|e| GraphError::InvalidWork {
            reason: format!("cannot encode the event names: {e}"),
        })?;
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {EVENT_COLUMNS} FROM work_schedule_history
                     WHERE schedule_id = ?1
                       AND event IN (SELECT value FROM json_each(?2))
                     ORDER BY COALESCE(due_ms, at_ms) DESC, at_ms DESC, seq DESC LIMIT 1"
                ),
                params![schedule_id, wanted],
                event_from_row,
            )
            .optional()?)
    }

    /// A schedule's newest history row whose event is one of `events`, by the time
    /// the row records (`at_ms`), then append order. For state rows (disabled,
    /// enabled); a run's row is picked with [`GraphStore::last_schedule_run`].
    pub fn last_schedule_event(
        &self,
        schedule_id: &str,
        events: &[&str],
    ) -> Result<Option<ScheduleEvent>> {
        if events.is_empty() {
            return Ok(None);
        }
        let wanted = serde_json::to_string(events).map_err(|e| GraphError::InvalidWork {
            reason: format!("cannot encode the event names: {e}"),
        })?;
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {EVENT_COLUMNS} FROM work_schedule_history
                     WHERE schedule_id = ?1
                       AND event IN (SELECT value FROM json_each(?2))
                     ORDER BY at_ms DESC, seq DESC LIMIT 1"
                ),
                params![schedule_id, wanted],
                event_from_row,
            )
            .optional()?)
    }

    /// One work node by id, from any plan.
    pub fn work_node(&self, id: &str) -> Result<Option<crate::work::WorkNode>> {
        let found = self
            .conn()
            .query_row(
                "SELECT id, plan_id, kind, parent_id, title, body, created_ms FROM work_nodes
                 WHERE id = ?1",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, i64>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, plan_id, kind, parent_id, title, body, created_ms)) = found else {
            return Ok(None);
        };
        let kind = crate::work::WorkKind::parse(&kind).ok_or(GraphError::UnknownKind {
            what: "work",
            value: kind,
        })?;
        Ok(Some(crate::work::WorkNode {
            id,
            plan_id,
            kind,
            parent_id,
            title,
            body,
            created_ms,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventTail;
    use crate::work::{WorkKind, WorkNode};

    fn nodes() -> Vec<WorkNode> {
        vec![
            WorkNode {
                id: "p1".into(),
                plan_id: "p1".into(),
                kind: WorkKind::Plan,
                parent_id: None,
                title: "plan".into(),
                body: "{}".into(),
                created_ms: 1,
            },
            WorkNode {
                id: "p1/w1".into(),
                plan_id: "p1".into(),
                kind: WorkKind::Item,
                parent_id: Some("p1".into()),
                title: "nightly build".into(),
                body: "{}".into(),
                created_ms: 1,
            },
        ]
    }

    fn schedule(id: &str, node: &str) -> WorkSchedule {
        WorkSchedule {
            id: id.into(),
            node_id: node.into(),
            definition: format!(r#"{{"id":"{id}"}}"#),
            created_ms: 5,
        }
    }

    fn event<'a>(kind: &'a str, run: Option<&'a str>, detail: &'a str) -> NewScheduleEvent<'a> {
        NewScheduleEvent {
            schedule_id: "nightly",
            node_id: "p1/w1",
            event: kind,
            run_id: run,
            due_ms: Some(10),
            at_ms: 11,
            detail,
            spend: None,
        }
    }

    #[test]
    fn a_schedule_attaches_to_a_stored_node_only_and_is_heard() {
        let dir = tempfile::tempdir().expect("dir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        assert!(matches!(
            store.add_schedule(&schedule("nightly", "p1/w1")),
            Err(GraphError::InvalidWork { .. })
        ));
        store.put_work(&nodes(), &[]).expect("put");
        let mut tail = EventTail::from_now(&store).expect("tail");
        store
            .add_schedule(&schedule("nightly", "p1/w1"))
            .expect("add");
        assert_eq!(
            store.schedule("nightly").expect("read"),
            Some(schedule("nightly", "p1/w1"))
        );
        assert!(
            store.add_schedule(&schedule("nightly", "p1")).is_err(),
            "an existing id is refused, never overwritten"
        );
        let changes = tail.next_batch(&store, 100).expect("batch");
        assert!(
            changes.iter().any(|c| c.table == "work_schedules"),
            "{changes:?}"
        );
    }

    #[test]
    fn history_is_appended_on_the_node_and_outlives_the_schedule() {
        let dir = tempfile::tempdir().expect("dir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        store.put_work(&nodes(), &[]).expect("put");
        store
            .add_schedule(&schedule("nightly", "p1/w1"))
            .expect("add");
        store
            .append_schedule_event(&event("started", Some("sched-nightly-10"), ""))
            .expect("started");
        store
            .append_schedule_event(&NewScheduleEvent {
                spend: Some(&RunSpend {
                    micros: 100_000,
                    calls: 3,
                    unpriced: 1,
                    text: "$0.10".into(),
                }),
                ..event("completed", Some("sched-nightly-10"), "")
            })
            .expect("completed");
        store
            .append_schedule_event(&event("disabled", None, "missing credential"))
            .expect("disabled");
        let newest = store.schedule_events("nightly", 10).expect("events");
        let kinds: Vec<&str> = newest.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(kinds, ["disabled", "completed", "started"]);
        let spend = newest[1].spend.as_ref().expect("spend");
        assert_eq!((spend.micros, spend.calls, spend.unpriced), (100_000, 3, 1));
        assert_eq!(spend.text, "$0.10");
        assert!(
            store
                .has_schedule_event("nightly", "completed", "sched-nightly-10")
                .expect("has")
        );
        assert!(store.node_has_schedule_history("p1/w1").expect("has"));
        assert!(!store.node_has_schedule_history("p1").expect("has"));
        let last_run = store
            .last_schedule_event("nightly", &["started", "completed", "failed"])
            .expect("last")
            .expect("some");
        assert_eq!(last_run.event, "completed");

        assert!(store.remove_schedule("nightly").expect("remove"));
        assert!(store.schedule("nightly").expect("read").is_none());
        assert_eq!(
            store.node_schedule_events("p1/w1", 10).expect("node").len(),
            3,
            "the node keeps its history after the schedule is removed"
        );
        let mut orphan = event("started", None, "");
        orphan.node_id = "p1/ghost";
        assert!(store.append_schedule_event(&orphan).is_err());
    }

    /// The shell's scheduler records history through `open_existing_for_facts`: it
    /// must never create a file, and never rebuild (empty) an index whose schema
    /// version is not this build's.
    #[test]
    fn opening_for_facts_never_creates_or_rebuilds() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("graph.db");
        assert!(GraphStore::open_existing_for_facts(&db, dir.path()).is_err());
        assert!(!db.exists(), "no file is created");
        {
            let store = GraphStore::open(&db, dir.path()).expect("store");
            store.put_work(&nodes(), &[]).expect("put");
        }
        let store = GraphStore::open_existing_for_facts(&db, dir.path()).expect("same version");
        store
            .add_schedule(&schedule("nightly", "p1/w1"))
            .expect("writes");
        store.set_meta("schema_version", "0").expect("age it");
        drop(store);
        assert!(matches!(
            GraphStore::open_existing_for_facts(&db, dir.path()),
            Err(GraphError::SchemaMismatch { found: Some(0), .. })
        ));
        let raw = rusqlite::Connection::open(&db).expect("raw");
        let version: String = raw
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .expect("version");
        assert_eq!(version, "0", "the refused open did not rebuild");
    }

    /// Two app instances (two connections) writing the same run
    /// event produce one row; the second write is ignored inside SQLite.
    #[test]
    fn a_run_event_is_written_once_across_two_connections() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("graph.db");
        let a = GraphStore::open(&db, dir.path()).expect("a");
        a.put_work(&nodes(), &[]).expect("put");
        a.add_schedule(&schedule("nightly", "p1/w1")).expect("add");
        let b = GraphStore::open(&db, dir.path()).expect("b");
        let row = event("completed", Some("r1"), "");
        assert!(a.append_schedule_event(&row).expect("a").is_some());
        assert!(b.append_schedule_event(&row).expect("b").is_none());
        // Rows without a run id are not limited by the index.
        let state = event("disabled", None, "why");
        assert!(a.append_schedule_event(&state).expect("a").is_some());
        assert!(b.append_schedule_event(&state).expect("b").is_some());
        assert_eq!(a.schedule_events("nightly", 10).expect("h").len(), 3);
    }

    /// The last run is picked by run time. A row for an older run
    /// appended later (rebuilt, or written late from the retry queue) does not win.
    #[test]
    fn the_last_run_is_picked_by_run_time_not_append_order() {
        let dir = tempfile::tempdir().expect("dir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        store.put_work(&nodes(), &[]).expect("put");
        store
            .add_schedule(&schedule("nightly", "p1/w1"))
            .expect("add");
        let run = |id: &'static str, due: i64, at: i64| NewScheduleEvent {
            due_ms: Some(due),
            at_ms: at,
            ..event("completed", Some(id), "")
        };
        store
            .append_schedule_event(&run("newer", 2_000, 2_100))
            .expect("newer");
        store
            .append_schedule_event(&run("older", 1_000, 5_000))
            .expect("older, written late");
        let last = store
            .last_schedule_run("nightly", &["completed"])
            .expect("read")
            .expect("some");
        assert_eq!(last.run_id.as_deref(), Some("newer"));
    }

    #[test]
    fn schedules_and_history_survive_a_schema_rebuild() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("graph.db");
        {
            let store = GraphStore::open(&db, dir.path()).expect("store");
            store.put_work(&nodes(), &[]).expect("put");
            store
                .add_schedule(&schedule("nightly", "p1/w1"))
                .expect("add");
            store
                .append_schedule_event(&event("started", Some("r1"), ""))
                .expect("event");
            store
                .set_meta("schema_version", "0")
                .expect("force a rebuild");
        }
        let store = GraphStore::open(&db, dir.path()).expect("reopen");
        assert_eq!(store.schedules().expect("schedules").len(), 1);
        assert_eq!(store.schedule_events("nightly", 10).expect("h").len(), 1);
        assert_eq!(
            store.work_node("p1/w1").expect("node").map(|n| n.title),
            Some("nightly build".to_owned())
        );
    }
}
