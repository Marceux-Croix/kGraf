//! Schedules on work nodes (decision 0052 section 3).
//!
//! A schedule is a property of a work node, never a list of jobs. This module is
//! the one implementation both binaries share: `kgraf schedule` writes through
//! it, and the desktop shell's background loop reads definitions, records run
//! history and derives the node labels through it. The tables are
//! `kgraf_graph::work_schedule`'s; this module adds the typed layer on top
//! (the definition's shape, the event names, and the rules below).
//!
//! **Attaching.** [`add`] attaches a definition to an existing work node
//! ([`NodeChoice::Existing`], `schedule add --node <id>`), or creates one from the
//! instruction ([`NodeChoice::FromInstruction`]): an item `schedules/<id>` under the
//! plan root [`kgraf_graph::SCHEDULE_PLAN_ID`], with the instruction verbatim in
//! its body (decisions 0056, 0057). A created node that already carries history
//! (an earlier schedule with the same id) is never reused: the new one gets
//! `schedules/<id>.2`, `.3`, and so on, so two schedules' histories never mix
//!. A created node with no history (left by a crash before its schedule
//! row was written) is reused.
//!
//! **History.** [`record`] appends one row per run event on the schedule's node:
//! claimed and started, the outcome, and the spend read from the ledger by run id.
//! A run event with a run id is written once, however often it is retried or
//! rebuilt ([`reconcile`]). A disable is recorded once per distinct reason, not once
//! per tick. History is the timeline only: whether a schedule is disabled now is the
//! claim table's answer (`ScheduleStore::schedule_status`), never a history row's
//!.
//!
//! **Rebuilding lost rows.** A history write can fail (a busy graph) or be lost (a
//! crash between the tick and the write). [`Reconciler`] compares the claim table's
//! settled occurrences, past a per-schedule watermark, with the node's outcome rows
//! and returns each missing one. It skips run ids the caller still
//! queues, and runs claimed before the schedule was on its node. A partial unique
//! index makes each run event one row, whoever writes it.
//!
//! **Migration.** Schedules used to be one graph-meta row each (`schedule:<id>`,
//! `crate::storage`). [`migrate_legacy`] moves each such row onto a created work
//! node and removes the meta row, and returns what it did so the caller can say so.
//! It is idempotent, and two processes migrating at once both succeed: one moves the
//! row, the other finds it already moved.

use chrono::{DateTime, Utc};
use kgraf_graph::{
    GraphStore, NewScheduleEvent, RunSpend, SCHEDULE_PLAN_ID, ScheduleEvent, WorkKind, WorkNode,
    WorkSchedule,
};

use crate::definition::ScheduleDefinition;
use crate::error::ScheduleError;
use crate::storage::{INDEX_KEY, SCHEDULE_PREFIX, schedule_ids_from_meta_keys, schedule_key};
use crate::store::ScheduleStore;

/// What happened to a schedule, as stored in a history row's `event` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// The schedule was attached to its node.
    Added,
    /// The schedule was detached from its node (`schedule remove`).
    Removed,
    /// The schedule was moved from the old graph-meta row onto its node.
    Migrated,
    /// An occurrence was claimed and its run started (also after deferrals; the
    /// detail says how many).
    Started,
    /// An abandoned claim was reclaimed and its run started again.
    Reclaimed,
    /// An occurrence was claimed, then refused by the concurrency cap and deferred.
    Deferred,
    /// A deferred occurrence was dropped past its bound.
    Dropped,
    /// This instance's claim lapsed and another instance holds it now.
    LeaseLost,
    /// The run finished successfully.
    Completed,
    /// The run, or its readiness, failed.
    Failed,
    /// The schedule was disabled; the row's detail is the reason.
    Disabled,
    /// A disable was cleared (`schedule enable`).
    Enabled,
    /// The run id could not be recorded in the claim table; spend attribution is at
    /// risk.
    RunIdNotRecorded,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Added => "added",
            EventKind::Removed => "removed",
            EventKind::Migrated => "migrated",
            EventKind::Started => "started",
            EventKind::Reclaimed => "reclaimed",
            EventKind::Deferred => "deferred",
            EventKind::Dropped => "dropped",
            EventKind::LeaseLost => "lease_lost",
            EventKind::Completed => "completed",
            EventKind::Failed => "failed",
            EventKind::Disabled => "disabled",
            EventKind::Enabled => "enabled",
            EventKind::RunIdNotRecorded => "run_id_not_recorded",
        }
    }

    /// The words a label uses for this event.
    pub fn word(self) -> &'static str {
        match self {
            EventKind::Added => "added",
            EventKind::Removed => "removed",
            EventKind::Migrated => "moved onto this node",
            EventKind::Started => "started",
            EventKind::Reclaimed => "reclaimed and restarted",
            EventKind::Deferred => "claimed, deferred by the cap",
            EventKind::Dropped => "dropped",
            EventKind::LeaseLost => "claim lost to another instance",
            EventKind::Completed => "completed",
            EventKind::Failed => "failed",
            EventKind::Disabled => "disabled",
            EventKind::Enabled => "enabled",
            EventKind::RunIdNotRecorded => "run id not recorded",
        }
    }

    pub fn parse(name: &str) -> Option<EventKind> {
        ALL.iter().copied().find(|k| k.as_str() == name)
    }

    /// A run event with a run id is written at most once per run id.
    fn once_per_run(self) -> bool {
        matches!(
            self,
            EventKind::Started
                | EventKind::Reclaimed
                | EventKind::Completed
                | EventKind::Failed
                | EventKind::Dropped
                | EventKind::LeaseLost
                | EventKind::RunIdNotRecorded
        )
    }
}

const ALL: [EventKind; 13] = [
    EventKind::Added,
    EventKind::Removed,
    EventKind::Migrated,
    EventKind::Started,
    EventKind::Reclaimed,
    EventKind::Deferred,
    EventKind::Dropped,
    EventKind::LeaseLost,
    EventKind::Completed,
    EventKind::Failed,
    EventKind::Disabled,
    EventKind::Enabled,
    EventKind::RunIdNotRecorded,
];

/// The events that describe a run. The label's "last run" is the newest of them
/// by run time (`GraphStore::last_schedule_run`), not by append order.
const RUN_EVENTS: [&str; 7] = [
    "started",
    "reclaimed",
    "deferred",
    "dropped",
    "lease_lost",
    "completed",
    "failed",
];

/// The outcome events [`reconcile`] rebuilds.
const OUTCOME_EVENTS: [&str; 2] = ["completed", "failed"];

/// The events a disable is compared with, to write it once per reason.
const STATE_EVENTS: [&str; 4] = ["disabled", "enabled", "added", "migrated"];

/// Most `.N` suffixes tried for a created node id before refusing.
const MAX_NODE_SUFFIX: u32 = 1000;

/// Where a new schedule goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeChoice<'a> {
    /// An existing work node, by id (`schedule add --node <id>`).
    Existing(&'a str),
    /// A new work node made from the instruction.
    FromInstruction,
}

/// A schedule with the node it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSchedule {
    pub node_id: String,
    pub definition: ScheduleDefinition,
}

/// The first id [`NodeChoice::FromInstruction`] tries for `schedule_id`'s node.
pub fn created_node_id(schedule_id: &str) -> String {
    format!("{SCHEDULE_PLAN_ID}/{schedule_id}")
}

fn millis(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

/// The first non-empty line of an instruction, cut to 60 characters: the created
/// node's title. The body keeps the instruction verbatim.
fn title_of(instruction: &str, fallback: &str) -> String {
    const MAX: usize = 60;
    let line = instruction
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(fallback);
    if line.chars().count() <= MAX {
        line.to_owned()
    } else {
        format!("{}...", line.chars().take(MAX).collect::<String>())
    }
}

/// The node id a new schedule's created node takes: the first of `schedules/<id>`,
/// `schedules/<id>.2`, ... that is either not stored or stored with no schedule
/// history.
fn free_node_id(graph: &GraphStore, schedule_id: &str) -> Result<String, ScheduleError> {
    let base = created_node_id(schedule_id);
    for n in 1..=MAX_NODE_SUFFIX {
        let id = if n == 1 {
            base.clone()
        } else {
            format!("{base}.{n}")
        };
        if graph.work_node(&id)?.is_none() || !graph.node_has_schedule_history(&id)? {
            return Ok(id);
        }
    }
    Err(ScheduleError::Graph(kgraf_graph::GraphError::InvalidWork {
        reason: format!(
            "no free node id for schedule `{schedule_id}` after {MAX_NODE_SUFFIX} tries"
        ),
    }))
}

fn create_node(
    graph: &GraphStore,
    def: &ScheduleDefinition,
    now: DateTime<Utc>,
) -> Result<String, ScheduleError> {
    let json = |context, value: serde_json::Value| {
        serde_json::to_string(&value).map_err(|source| ScheduleError::Json {
            id: def.id.clone(),
            context,
            source,
        })
    };
    let root = WorkNode {
        id: SCHEDULE_PLAN_ID.to_owned(),
        plan_id: SCHEDULE_PLAN_ID.to_owned(),
        kind: WorkKind::Plan,
        parent_id: None,
        title: "Scheduled work".to_owned(),
        body: json(
            "cannot encode the plan root",
            serde_json::json!({ "source": "kgraf schedule" }),
        )?,
        created_ms: millis(now),
    };
    let id = free_node_id(graph, &def.id)?;
    let item = WorkNode {
        id: id.clone(),
        plan_id: SCHEDULE_PLAN_ID.to_owned(),
        kind: WorkKind::Item,
        parent_id: Some(SCHEDULE_PLAN_ID.to_owned()),
        title: title_of(&def.instruction, &def.id),
        body: json(
            "cannot encode the node body",
            serde_json::json!({
                "instruction": def.instruction,
                "schedule": def.id,
                "source": "kgraf schedule add",
            }),
        )?,
        created_ms: millis(now),
    };
    graph.put_work(&[root, item], &[])?;
    Ok(id)
}

fn encode(def: &ScheduleDefinition) -> Result<String, ScheduleError> {
    serde_json::to_string(def).map_err(|source| ScheduleError::Json {
        id: def.id.clone(),
        context: "cannot encode the definition",
        source,
    })
}

fn decode(id: &str, text: &str) -> Result<ScheduleDefinition, ScheduleError> {
    serde_json::from_str(text).map_err(|source| ScheduleError::Json {
        id: id.to_owned(),
        context: "the stored definition is not valid JSON",
        source,
    })
}

fn append(
    graph: &GraphStore,
    schedule_id: &str,
    node_id: &str,
    kind: EventKind,
    detail: &str,
    at: DateTime<Utc>,
) -> Result<(), ScheduleError> {
    graph.append_schedule_event(&NewScheduleEvent {
        schedule_id,
        node_id,
        event: kind.as_str(),
        run_id: None,
        due_ms: None,
        at_ms: millis(at),
        detail,
        spend: None,
    })?;
    Ok(())
}

/// Attaches `def` to a work node and records it on the node's history. Returns the
/// node id. Refused, with nothing written, when the id is taken, when the definition
/// does not build (an unknown zone, a bad pattern: decision 0052 refuses these at
/// creation), or when `--node` names a node that is not stored.
pub fn add(
    graph: &GraphStore,
    def: &ScheduleDefinition,
    node: NodeChoice<'_>,
    now: DateTime<Utc>,
) -> Result<String, ScheduleError> {
    crate::storage::validate_schedule_id(&def.id)?;
    def.to_schedule()?;
    if let Some(existing) = graph.schedule(&def.id)? {
        return Err(ScheduleError::ScheduleExists {
            id: def.id.clone(),
            node: existing.node_id,
        });
    }
    let node_id = match node {
        NodeChoice::Existing(id) => {
            if graph.work_node(id)?.is_none() {
                return Err(ScheduleError::UnknownWorkNode(id.to_owned()));
            }
            id.to_owned()
        }
        NodeChoice::FromInstruction => create_node(graph, def, now)?,
    };
    graph.add_schedule(&WorkSchedule {
        id: def.id.clone(),
        node_id: node_id.clone(),
        definition: encode(def)?,
        created_ms: millis(now),
    })?;
    append(graph, &def.id, &node_id, EventKind::Added, &def.when(), now)?;
    Ok(node_id)
}

/// Every schedule with its node. A definition that is not valid JSON fails the
/// whole read: running the others while one silently vanishes is the
/// defect this refuses.
pub fn load_all(graph: &GraphStore) -> Result<Vec<NodeSchedule>, ScheduleError> {
    graph
        .schedules()?
        .into_iter()
        .map(|s| {
            Ok(NodeSchedule {
                definition: decode(&s.id, &s.definition)?,
                node_id: s.node_id,
            })
        })
        .collect()
}

/// Detaches a schedule from its node and records that on the node. Returns the node
/// id, or `None` when no schedule has this id. The node and its history stay.
pub fn remove(
    graph: &GraphStore,
    id: &str,
    now: DateTime<Utc>,
) -> Result<Option<String>, ScheduleError> {
    let Some(existing) = graph.schedule(id)? else {
        return Ok(None);
    };
    graph.remove_schedule(id)?;
    append(graph, id, &existing.node_id, EventKind::Removed, "", now)?;
    Ok(Some(existing.node_id))
}

/// Deletes an old graph-meta schedule row that [`migrate_legacy`] refused to move
///. Returns whether one was stored.
pub fn remove_legacy(graph: &GraphStore, id: &str) -> Result<bool, ScheduleError> {
    let key = schedule_key(id);
    if graph.meta(&key)?.is_none() {
        return Ok(false);
    }
    graph.clear_meta(&key)?;
    Ok(true)
}

/// One run event to record on a schedule's node.
#[derive(Debug, Clone)]
pub struct RunRecord<'a> {
    pub schedule_id: &'a str,
    pub kind: EventKind,
    pub run_id: Option<&'a str>,
    pub due_at: Option<DateTime<Utc>>,
    /// A reason or error text, verbatim; empty when there is none.
    pub detail: &'a str,
    /// The run's spend as read from the ledger by run id.
    pub spend: Option<&'a RunSpend>,
}

/// An owned [`RunRecord`], for a recorder that keeps rows it could not write and
/// retries them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedRunRecord {
    pub schedule_id: String,
    pub kind: EventKind,
    pub run_id: Option<String>,
    pub due_at: Option<DateTime<Utc>>,
    pub detail: String,
    pub spend: Option<RunSpend>,
    /// When the event happened; kept, so a late write records the right time.
    pub at: DateTime<Utc>,
}

impl OwnedRunRecord {
    pub fn as_record(&self) -> RunRecord<'_> {
        RunRecord {
            schedule_id: &self.schedule_id,
            kind: self.kind,
            run_id: self.run_id.as_deref(),
            due_at: self.due_at,
            detail: &self.detail,
            spend: self.spend.as_ref(),
        }
    }
}

/// Appends `rec` to the history of the schedule's node. A schedule removed while
/// its run was in flight is still recorded, on the node its last history row
/// names. Returns `false` when nothing was written because the row is already there:
/// a run event with this run id, or a disable with the same reason (see the module
/// doc).
pub fn record(
    graph: &GraphStore,
    rec: &RunRecord<'_>,
    at: DateTime<Utc>,
) -> Result<bool, ScheduleError> {
    let node_id = match graph.schedule(rec.schedule_id)? {
        Some(s) => s.node_id,
        None => graph
            .schedule_events(rec.schedule_id, 1)?
            .into_iter()
            .next()
            .map(|e| e.node_id)
            .ok_or_else(|| ScheduleError::UnknownSchedule(rec.schedule_id.to_owned()))?,
    };
    if let Some(run_id) = rec.run_id
        && rec.kind.once_per_run()
        && graph.has_schedule_event(rec.schedule_id, rec.kind.as_str(), run_id)?
    {
        return Ok(false);
    }
    if rec.kind == EventKind::Disabled
        && graph
            .last_schedule_event(rec.schedule_id, &STATE_EVENTS)?
            .is_some_and(|e| e.event == EventKind::Disabled.as_str() && e.detail == rec.detail)
    {
        return Ok(false);
    }
    // The unique index decides a race with another writer of the same run event.
    Ok(graph
        .append_schedule_event(&NewScheduleEvent {
            schedule_id: rec.schedule_id,
            node_id: &node_id,
            event: rec.kind.as_str(),
            run_id: rec.run_id,
            due_ms: rec.due_at.map(millis),
            at_ms: millis(at),
            detail: rec.detail,
            spend: rec.spend,
        })?
        .is_some())
}

/// The detail a rebuilt outcome row carries. The claim table keeps the outcome,
/// not its error text, and a rebuilt row exists because the tick that saw the run
/// settle did not record it.
pub const REBUILT_DETAIL: &str =
    "rebuilt from the claim table: its outcome was not recorded when it settled";

/// Finds settled outcomes the claim table has and the node's history lacks, reading only
/// occurrences due after a per-schedule watermark. It only reads; the caller writes what it
/// returns, through the same path as any other row, so a busy graph is handled once.
///
/// Skipped:
/// - a run id the caller still holds in its retry queue (`queued`): the real row,
///   with its error text and time, is written from there;
/// - a run claimed before its schedule was attached to its node (a migrated
///   schedule's earlier runs): no row was ever due for it, so none is rebuilt;
/// - a dropped occurrence (it never ran) and an occurrence with no run id.
///
/// The watermark for a schedule advances to the last occurrence of the leading run
/// of settled ones, so an occurrence still pending or claimed is read again until
/// it settles. It starts empty in each process: the first call reads everything,
/// which is the rebuild at thread start.
#[derive(Debug, Default)]
pub struct Reconciler {
    watermarks: std::collections::HashMap<String, DateTime<Utc>>,
}

impl Reconciler {
    /// The rows to write. Each carries the occurrence's run id, its claim time as
    /// `at` (so it sorts as the run it describes, not as the rebuild),
    /// [`REBUILT_DETAIL`], and the spend `spend` reads by run id.
    pub fn missing_outcomes(
        &mut self,
        graph: &GraphStore,
        claims: &ScheduleStore,
        queued: &std::collections::BTreeSet<String>,
        spend: &dyn Fn(&str) -> Option<RunSpend>,
    ) -> Result<Vec<OwnedRunRecord>, ScheduleError> {
        let mut out = Vec::new();
        for s in graph.schedules()? {
            let after = self.watermarks.get(&s.id).copied();
            let occs = claims.occurrences_since(&s.id, after)?;
            if occs.is_empty() {
                continue;
            }
            let attached = DateTime::<Utc>::from_timestamp_millis(s.created_ms);
            let have = graph.schedule_run_ids_with(&s.id, &OUTCOME_EVENTS)?;
            let mut watermark = after;
            let mut leading = true;
            for occ in &occs {
                let Some(succeeded) = occ.settled else {
                    leading = false;
                    continue;
                };
                if leading {
                    watermark = Some(occ.due_at);
                }
                let Some(run_id) = &occ.run_id else {
                    continue;
                };
                let claimed = occ.claimed_at.unwrap_or(occ.due_at);
                let before_attach = attached.is_some_and(|a| claimed < a);
                if occ.dropped || before_attach || have.contains(run_id) || queued.contains(run_id)
                {
                    continue;
                }
                out.push(OwnedRunRecord {
                    schedule_id: s.id.clone(),
                    kind: if succeeded {
                        EventKind::Completed
                    } else {
                        EventKind::Failed
                    },
                    run_id: Some(run_id.clone()),
                    due_at: Some(occ.due_at),
                    detail: REBUILT_DETAIL.to_owned(),
                    spend: spend(run_id),
                    at: claimed,
                });
            }
            if let Some(w) = watermark {
                self.watermarks.insert(s.id.clone(), w);
            }
        }
        Ok(out)
    }
}

/// Finds and writes the missing outcomes in one call (see [`Reconciler`]), with
/// nothing queued. Returns how many rows were written.
pub fn reconcile(
    graph: &GraphStore,
    claims: &ScheduleStore,
    spend: &dyn Fn(&str) -> Option<RunSpend>,
) -> Result<usize, ScheduleError> {
    let rows = Reconciler::default().missing_outcomes(
        graph,
        claims,
        &std::collections::BTreeSet::new(),
        spend,
    )?;
    let mut written = 0;
    for rec in &rows {
        if record(graph, &rec.as_record(), rec.at)? {
            written += 1;
        }
    }
    Ok(written)
}

/// What a schedule's work node shows from the graph (decision 0052 section 3;
/// what the graph now carries). Derived from the schedule row and the
/// newest history rows at read time. Whether the schedule is disabled is not here:
/// the claim table decides that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleView {
    pub schedule_id: String,
    pub node_id: String,
    /// The node's title, or its id when the node row cannot be read.
    pub node_title: String,
    /// When it fires ([`ScheduleDefinition::when`]), or why the definition cannot
    /// be read.
    pub when: String,
    /// The user turned the schedule off in its definition.
    pub turned_off: bool,
    /// The newest run event.
    pub last_run: Option<ScheduleEvent>,
}

/// Every schedule's view, by schedule id.
pub fn views(graph: &GraphStore) -> Result<Vec<ScheduleView>, ScheduleError> {
    let mut out = Vec::new();
    for s in graph.schedules()? {
        let (when, turned_off) = match decode(&s.id, &s.definition) {
            Ok(def) => (def.when(), !def.enabled),
            Err(e) => (format!("definition unreadable: {e}"), false),
        };
        let node_title = graph
            .work_node(&s.node_id)?
            .map_or_else(|| s.node_id.clone(), |n| n.title);
        let last_run = graph.last_schedule_run(&s.id, &RUN_EVENTS)?;
        out.push(ScheduleView {
            schedule_id: s.id,
            node_id: s.node_id,
            node_title,
            when,
            turned_off,
            last_run,
        });
    }
    Ok(out)
}

/// What [`migrate_legacy`] did with one old meta row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Migration {
    /// Moved onto a created work node.
    Moved { id: String, node_id: String },
    /// Already on a node (a crash after the move, or another process moved it
    /// first); the stale meta row was removed.
    AlreadyOnNode { id: String, node_id: String },
    /// The row could not be moved; it is left in place and the reason given.
    /// `kgraf schedule remove <id>` deletes it.
    Refused { id: String, reason: String },
}

impl Migration {
    pub fn id(&self) -> &str {
        match self {
            Migration::Moved { id, .. }
            | Migration::AlreadyOnNode { id, .. }
            | Migration::Refused { id, .. } => id,
        }
    }
}

impl std::fmt::Display for Migration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Migration::Moved { id, node_id } => write!(
                f,
                "schedule `{id}` moved from the old graph-meta store onto work node `{node_id}`"
            ),
            Migration::AlreadyOnNode { id, node_id } => write!(
                f,
                "schedule `{id}` was already on work node `{node_id}`; its old graph-meta row was removed"
            ),
            Migration::Refused { id, reason } => write!(
                f,
                "schedule `{id}` was NOT moved onto a work node and does not run: {reason}; \
                 `kgraf schedule remove {id}` deletes the old row"
            ),
        }
    }
}

/// One old row moved onto a node. A concurrent migration by another process can
/// win any step; each step checks for that and reports [`Migration::AlreadyOnNode`].
fn migrate_one(
    graph: &GraphStore,
    id: String,
    key: &str,
    text: &str,
    now: DateTime<Utc>,
) -> Result<Migration, ScheduleError> {
    // Both sides must decode to compare them; a row that does not decode is never
    // cleared (two failed decodes used to compare equal).
    let def = match decode(&id, text) {
        Ok(def) if def.id == id => def,
        Ok(def) => {
            return Ok(Migration::Refused {
                reason: format!("the row's definition names id `{}`", def.id),
                id,
            });
        }
        Err(e) => {
            return Ok(Migration::Refused {
                id,
                reason: e.to_string(),
            });
        }
    };
    let already = |graph: &GraphStore| -> Result<Option<Migration>, ScheduleError> {
        let Some(existing) = graph.schedule(&id)? else {
            return Ok(None);
        };
        Ok(Some(match decode(&id, &existing.definition) {
            Ok(on_node) if on_node == def => {
                graph.clear_meta(key)?;
                Migration::AlreadyOnNode {
                    id: id.clone(),
                    node_id: existing.node_id,
                }
            }
            Ok(_) => Migration::Refused {
                id: id.clone(),
                reason: format!(
                    "a different schedule with this id is already on work node `{}`",
                    existing.node_id
                ),
            },
            Err(e) => Migration::Refused {
                id: id.clone(),
                reason: format!(
                    "a schedule with this id is on work node `{}` and its definition does not read: {e}",
                    existing.node_id
                ),
            },
        }))
    };
    if let Some(done) = already(graph)? {
        return Ok(done);
    }
    let node_id = create_node(graph, &def, now)?;
    let added = graph.add_schedule(&WorkSchedule {
        id: id.clone(),
        node_id: node_id.clone(),
        definition: encode(&def)?,
        created_ms: millis(now),
    });
    if let Err(e) = added {
        // Another process added it between the check and the insert.
        return match already(graph)? {
            Some(done) => Ok(done),
            None => Err(e.into()),
        };
    }
    append(
        graph,
        &id,
        &node_id,
        EventKind::Migrated,
        "moved from the graph-meta store",
        now,
    )?;
    graph.clear_meta(key)?;
    Ok(Migration::Moved { id, node_id })
}

/// Moves every schedule still stored as an old graph-meta row onto a work node
/// (see the module doc). Returns one entry per row it found; empty when there was
/// nothing to move. A row that cannot be moved is left in place and reported as
/// [`Migration::Refused`], never dropped.
pub fn migrate_legacy(
    graph: &GraphStore,
    now: DateTime<Utc>,
) -> Result<Vec<Migration>, ScheduleError> {
    let keys = graph.meta_keys(SCHEDULE_PREFIX)?;
    let has_index = keys.iter().any(|k| k == INDEX_KEY);
    let mut out = Vec::new();
    for id in schedule_ids_from_meta_keys(keys) {
        let key = schedule_key(&id);
        let Some(text) = graph.meta(&key)? else {
            continue;
        };
        out.push(migrate_one(graph, id, &key, &text, now)?);
    }
    // The superseded index row names nothing that still runs.
    if has_index {
        graph.clear_meta(INDEX_KEY)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn graph() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().expect("dir");
        let g = GraphStore::open_in_memory(dir.path()).expect("graph");
        (dir, g)
    }

    fn def(id: &str) -> ScheduleDefinition {
        ScheduleDefinition::recurring(
            id,
            "Run the nightly build\nthen report",
            "0 0 3 * * *",
            "UTC",
        )
        .expect("valid")
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 27, 3, 0, 0).unwrap()
    }

    fn spend(micros: i64) -> RunSpend {
        RunSpend {
            micros,
            calls: 2,
            unpriced: 0,
            text: format!("{} micro-dollars", micros),
        }
    }

    fn plan_item(g: &GraphStore) {
        g.put_work(
            &[
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
                    title: "keep the build green".into(),
                    body: "{}".into(),
                    created_ms: 1,
                },
            ],
            &[],
        )
        .expect("put");
    }

    fn legacy(g: &GraphStore, d: &ScheduleDefinition) {
        g.set_meta(&schedule_key(&d.id), &serde_json::to_string(d).unwrap())
            .unwrap();
    }

    #[test]
    fn add_with_a_node_attaches_there_and_refuses_an_unknown_node() {
        let (_d, g) = graph();
        let err = add(&g, &def("nightly"), NodeChoice::Existing("p1/w1"), now())
            .expect_err("no such node");
        assert!(matches!(err, ScheduleError::UnknownWorkNode(_)));
        assert!(g.schedules().expect("read").is_empty(), "nothing written");

        plan_item(&g);
        let node = add(&g, &def("nightly"), NodeChoice::Existing("p1/w1"), now()).expect("add");
        assert_eq!(node, "p1/w1");
        let all = load_all(&g).expect("load");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].node_id, "p1/w1");
        let history = g.node_schedule_events("p1/w1", 10).expect("history");
        assert_eq!(history[0].event, "added");

        let again = add(&g, &def("nightly"), NodeChoice::FromInstruction, now());
        assert!(matches!(again, Err(ScheduleError::ScheduleExists { .. })));
    }

    #[test]
    fn add_without_a_node_creates_one_from_the_instruction() {
        let (_d, g) = graph();
        let node = add(&g, &def("nightly"), NodeChoice::FromInstruction, now()).expect("add");
        assert_eq!(node, "schedules/nightly");
        let created = g.work_node(&node).expect("read").expect("created");
        assert_eq!(created.title, "Run the nightly build");
        assert_eq!(created.parent_id.as_deref(), Some(SCHEDULE_PLAN_ID));
        let body: serde_json::Value = serde_json::from_str(&created.body).expect("json");
        assert_eq!(body["instruction"], "Run the nightly build\nthen report");
    }

    /// Re-adding an id after `remove` must not reuse (and overwrite) the
    /// old schedule's node, whose history belongs to the old schedule.
    #[test]
    fn re_adding_an_id_gets_a_new_node_and_the_old_history_stays_apart() {
        let (_d, g) = graph();
        add(&g, &def("nightly"), NodeChoice::FromInstruction, now()).expect("add");
        remove(&g, "nightly", now()).expect("remove");
        let mut other = def("nightly");
        other.instruction = "A different task".into();
        let node = add(&g, &other, NodeChoice::FromInstruction, now()).expect("re-add");
        assert_eq!(node, "schedules/nightly.2");
        let old = g
            .work_node("schedules/nightly")
            .expect("read")
            .expect("old");
        assert_eq!(
            old.title, "Run the nightly build",
            "the old node is untouched"
        );
        assert_eq!(
            g.node_schedule_events("schedules/nightly.2", 10)
                .expect("h")
                .len(),
            1,
            "only the new schedule's `added` row"
        );
    }

    #[test]
    fn run_history_lands_on_the_node_once_per_run_and_a_disable_once_per_reason() {
        let (_d, g) = graph();
        plan_item(&g);
        add(&g, &def("nightly"), NodeChoice::Existing("p1/w1"), now()).expect("add");
        let paid = spend(20_000);
        let rec = |kind, run, detail, spend| RunRecord {
            schedule_id: "nightly",
            kind,
            run_id: run,
            due_at: Some(now()),
            detail,
            spend,
        };
        assert!(record(&g, &rec(EventKind::Started, Some("r1"), "", None), now()).expect("r"));
        assert!(
            record(
                &g,
                &rec(EventKind::Completed, Some("r1"), "", Some(&paid)),
                now()
            )
            .expect("r")
        );
        assert!(
            !record(
                &g,
                &rec(EventKind::Completed, Some("r1"), "", Some(&paid)),
                now()
            )
            .expect("r"),
            "a retried outcome is not a second row"
        );
        let v = &views(&g).expect("views")[0];
        let last = v.last_run.as_ref().expect("a run");
        assert_eq!(last.event, "completed");
        assert_eq!(last.spend.as_ref().map(|s| s.micros), Some(20_000));

        let reason = "missing credential: ANTHROPIC_API_KEY is not set";
        assert!(record(&g, &rec(EventKind::Disabled, None, reason, None), now()).expect("r"));
        assert!(!record(&g, &rec(EventKind::Disabled, None, reason, None), now()).expect("r"));
        assert_eq!(g.schedule_events("nightly", 50).expect("h").len(), 4);
    }

    #[test]
    fn a_run_ending_after_its_schedule_was_removed_is_still_recorded_on_the_node() {
        let (_d, g) = graph();
        add(&g, &def("nightly"), NodeChoice::FromInstruction, now()).expect("add");
        remove(&g, "nightly", now())
            .expect("remove")
            .expect("was there");
        let rec = RunRecord {
            schedule_id: "nightly",
            kind: EventKind::Completed,
            run_id: Some("sched-nightly-1"),
            due_at: None,
            detail: "",
            spend: None,
        };
        assert!(record(&g, &rec, now()).expect("recorded"));
        let events = g.node_schedule_events("schedules/nightly", 10).expect("h");
        assert_eq!(events[0].event, "completed");
        let never = RunRecord {
            schedule_id: "ghost",
            ..rec
        };
        assert!(matches!(
            record(&g, &never, now()),
            Err(ScheduleError::UnknownSchedule(_))
        ));
    }

    /// An outcome the claim table settled but the history never got (a
    /// failed write, a crash) is rebuilt from the claim table, once.
    #[test]
    fn reconcile_rebuilds_a_lost_outcome_from_the_claim_table_once() {
        let (_d, g) = graph();
        add(&g, &def("nightly"), NodeChoice::FromInstruction, now()).expect("add");
        let claims = ScheduleStore::open_in_memory().expect("claims");
        let due = now();
        let generation = match claims
            .claim_occurrence(
                "nightly",
                due,
                "a",
                due,
                std::time::Duration::from_secs(900),
            )
            .expect("claim")
        {
            crate::ClaimOutcome::Claimed { generation, .. } => generation,
            crate::ClaimOutcome::NotClaimed => panic!("claim"),
        };
        claims
            .set_run_id("nightly", due, "a", generation, "sched-nightly-1")
            .expect("run id");
        claims
            .record_outcome("nightly", due, "a", generation, &crate::RunOutcome::Success)
            .expect("outcome");

        let paid = spend(5);
        let read = |run: &str| (run == "sched-nightly-1").then(|| paid.clone());
        assert_eq!(reconcile(&g, &claims, &read).expect("reconcile"), 1);
        assert_eq!(reconcile(&g, &claims, &read).expect("again"), 0, "once");
        let last = views(&g).expect("views")[0].last_run.clone().expect("run");
        assert_eq!(last.event, "completed");
        assert_eq!(last.run_id.as_deref(), Some("sched-nightly-1"));
        assert_eq!(last.detail, REBUILT_DETAIL);
        assert_eq!(last.spend, Some(paid));
    }

    fn settle(claims: &ScheduleStore, id: &str, due: DateTime<Utc>, run: &str, ok: bool) {
        let generation = match claims
            .claim_occurrence(id, due, "a", due, std::time::Duration::from_secs(900))
            .expect("claim")
        {
            crate::ClaimOutcome::Claimed { generation, .. } => generation,
            crate::ClaimOutcome::NotClaimed => panic!("claim"),
        };
        claims
            .set_run_id(id, due, "a", generation, run)
            .expect("run id");
        let outcome = if ok {
            crate::RunOutcome::Success
        } else {
            crate::RunOutcome::Failed(crate::FailureClass::Transient {
                detail: "boom".into(),
            })
        };
        claims
            .record_outcome(id, due, "a", generation, &outcome)
            .expect("outcome");
    }

    /// Verifier item 7: runs a migrated schedule made before it was on a node get
    /// no rebuilt rows; a run after the move that lost its row does.
    #[test]
    fn runs_from_before_a_migration_are_not_rebuilt() {
        let (_d, g) = graph();
        let claims = ScheduleStore::open_in_memory().expect("claims");
        let before = now() - chrono::TimeDelta::days(2);
        settle(&claims, "nightly", before, "sched-nightly-old", true);
        legacy(&g, &def("nightly"));
        migrate_legacy(&g, now()).expect("migrate");
        let after = now() + chrono::TimeDelta::days(1);
        settle(&claims, "nightly", after, "sched-nightly-new", true);
        assert_eq!(reconcile(&g, &claims, &|_: &str| None).expect("r"), 1);
        let rebuilt: Vec<Option<String>> = g
            .schedule_events("nightly", 10)
            .expect("h")
            .into_iter()
            .filter(|e| e.event == "completed")
            .map(|e| e.run_id)
            .collect();
        assert_eq!(rebuilt, vec![Some("sched-nightly-new".to_owned())]);
    }

    /// Verifier items 3 and 8: a run id in the retry queue is not rebuilt, and the
    /// watermark keeps a settled prefix from being read again, while an occurrence
    /// still claimed is read again until it settles.
    #[test]
    fn the_reconciler_skips_queued_runs_and_reads_only_past_its_watermark() {
        let (_d, g) = graph();
        add(&g, &def("nightly"), NodeChoice::FromInstruction, now()).expect("add");
        let claims = ScheduleStore::open_in_memory().expect("claims");
        let d1 = now() + chrono::TimeDelta::hours(1);
        let d2 = now() + chrono::TimeDelta::hours(2);
        let d3 = now() + chrono::TimeDelta::hours(3);
        settle(&claims, "nightly", d1, "r1", true);
        settle(&claims, "nightly", d2, "r2", false);
        // d3 is claimed and still running.
        let g3 = match claims
            .claim_occurrence("nightly", d3, "a", d3, std::time::Duration::from_secs(900))
            .expect("claim")
        {
            crate::ClaimOutcome::Claimed { generation, .. } => generation,
            crate::ClaimOutcome::NotClaimed => panic!("claim"),
        };
        claims.set_run_id("nightly", d3, "a", g3, "r3").expect("id");

        let mut rec = Reconciler::default();
        let queued: std::collections::BTreeSet<String> = ["r2".to_owned()].into();
        let rows = rec
            .missing_outcomes(&g, &claims, &queued, &|_: &str| None)
            .expect("rows");
        let ids: Vec<&str> = rows.iter().filter_map(|r| r.run_id.as_deref()).collect();
        assert_eq!(
            ids,
            ["r1"],
            "r2 is queued with its real row; r3 has not settled"
        );
        assert_eq!(rec.watermarks.get("nightly"), Some(&d2));

        claims
            .record_outcome("nightly", d3, "a", g3, &crate::RunOutcome::Success)
            .expect("outcome");
        let rows = rec
            .missing_outcomes(
                &g,
                &claims,
                &std::collections::BTreeSet::new(),
                &|_: &str| None,
            )
            .expect("rows");
        let ids: Vec<&str> = rows.iter().filter_map(|r| r.run_id.as_deref()).collect();
        assert_eq!(
            ids,
            ["r3"],
            "only past the watermark: r1 and r2 are not read again"
        );
        assert_eq!(rec.watermarks.get("nightly"), Some(&d3));
    }

    #[test]
    fn an_old_meta_schedule_moves_onto_a_created_node_once() {
        let (_d, g) = graph();
        let old = def("nightly");
        legacy(&g, &old);
        g.set_meta(&schedule_key("broken"), "{not json").unwrap();
        g.set_meta(INDEX_KEY, r#"["nightly"]"#).unwrap();

        let moved = migrate_legacy(&g, now()).expect("migrate");
        assert!(moved.contains(&Migration::Moved {
            id: "nightly".into(),
            node_id: "schedules/nightly".into()
        }));
        assert!(
            moved
                .iter()
                .any(|m| matches!(m, Migration::Refused { id, .. } if id == "broken")),
            "{moved:?}"
        );
        let all = load_all(&g).expect("load");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].definition, old);
        assert!(g.meta(&schedule_key("nightly")).unwrap().is_none());
        assert!(g.meta(INDEX_KEY).unwrap().is_none());
        assert!(g.meta(&schedule_key("broken")).unwrap().is_some());

        // A refused row can be deleted.
        assert!(remove_legacy(&g, "broken").expect("remove"));
        assert!(g.meta(&schedule_key("broken")).unwrap().is_none());
        assert!(!remove_legacy(&g, "broken").expect("gone"));
    }

    /// Every crash point of the move leaves a state the next migration
    /// finishes, with one schedule and no second node.
    #[test]
    fn every_crash_point_of_a_migration_is_finished_by_the_next() {
        let old = def("nightly");
        // 1. After the node was created, before the schedule row.
        let (_d, g) = graph();
        legacy(&g, &old);
        create_node(&g, &old, now()).expect("node");
        let m = migrate_legacy(&g, now()).expect("migrate");
        assert_eq!(
            m,
            vec![Migration::Moved {
                id: "nightly".into(),
                node_id: "schedules/nightly".into()
            }],
            "the empty node left by the crash is reused"
        );
        // 2. After the schedule row, before the history row and the meta clear.
        let (_d, g) = graph();
        legacy(&g, &old);
        let node = create_node(&g, &old, now()).expect("node");
        g.add_schedule(&WorkSchedule {
            id: "nightly".into(),
            node_id: node,
            definition: encode(&old).unwrap(),
            created_ms: 1,
        })
        .expect("add");
        let m = migrate_legacy(&g, now()).expect("migrate");
        assert!(matches!(m[0], Migration::AlreadyOnNode { .. }), "{m:?}");
        assert!(g.meta(&schedule_key("nightly")).unwrap().is_none());
        // 3. After the history row, before the meta clear.
        let (_d, g) = graph();
        legacy(&g, &old);
        migrate_legacy(&g, now()).expect("migrate");
        legacy(&g, &old);
        let m = migrate_legacy(&g, now()).expect("migrate");
        assert!(matches!(m[0], Migration::AlreadyOnNode { .. }), "{m:?}");
        assert_eq!(load_all(&g).expect("load").len(), 1);
        assert!(g.work_node("schedules/nightly.2").unwrap().is_none());
    }

    /// A meta row that does not decode is never cleared, even when the
    /// schedule on the node does not decode either.
    #[test]
    fn a_row_that_does_not_decode_is_never_cleared() {
        let (_d, g) = graph();
        add(&g, &def("nightly"), NodeChoice::FromInstruction, now()).expect("add");
        g.set_meta(&schedule_key("nightly"), "{not json").unwrap();
        let m = migrate_legacy(&g, now()).expect("migrate");
        assert!(matches!(m[0], Migration::Refused { .. }), "{m:?}");
        assert!(g.meta(&schedule_key("nightly")).unwrap().is_some());
    }

    /// The CLI and the shell migrating one file at once both succeed.
    #[test]
    fn two_processes_migrating_at_once_both_succeed_with_one_move() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("graph.db");
        {
            let g = GraphStore::open(&db, dir.path()).expect("graph");
            legacy(&g, &def("nightly"));
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let db = db.clone();
                let root = dir.path().to_path_buf();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let g = GraphStore::open(&db, &root).expect("graph");
                    barrier.wait();
                    migrate_legacy(&g, now()).expect("both succeed")
                })
            })
            .collect();
        let results: Vec<Vec<Migration>> = handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect();
        let moved = results
            .iter()
            .flatten()
            .filter(|m| matches!(m, Migration::Moved { .. }))
            .count();
        assert_eq!(moved, 1, "{results:?}");
        assert!(
            results
                .iter()
                .flatten()
                .all(|m| !matches!(m, Migration::Refused { .. })),
            "{results:?}"
        );
        let g = GraphStore::open(&db, dir.path()).expect("graph");
        assert_eq!(load_all(&g).expect("load").len(), 1);
        let migrated = g
            .schedule_events("nightly", 10)
            .expect("h")
            .into_iter()
            .filter(|e| e.event == "migrated")
            .count();
        assert_eq!(migrated, 1);
    }

    #[test]
    fn schedules_survive_a_schema_rebuild() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("graph.db");
        {
            let g = GraphStore::open(&db, dir.path()).expect("graph");
            add(&g, &def("nightly"), NodeChoice::FromInstruction, now()).expect("add");
            g.set_meta("schema_version", "0").expect("force a rebuild");
        }
        let g = GraphStore::open(&db, dir.path()).expect("reopen");
        let all = load_all(&g).expect("load");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].node_id, "schedules/nightly");
        assert_eq!(
            views(&g).expect("views")[0].node_title,
            "Run the nightly build"
        );
    }

    #[test]
    fn every_event_name_round_trips() {
        for k in ALL {
            assert_eq!(EventKind::parse(k.as_str()), Some(k));
        }
    }
}
