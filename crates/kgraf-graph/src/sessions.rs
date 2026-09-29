//! Session records in the graph store (decision 0067).
//!
//! [`kgraf_core::session::SessionRecords`] is a seam `kgraf-runtime` and
//! `kgraf-orchestrator` depend on so neither depends on this crate directly (see
//! the trait's doc comment). [`SharedSessions`] is the durable implementation: a
//! thin wrapper over [`GraphStore`] so several owners (the host, the shell's
//! read-only viewer) can share one connection behind a `Mutex`, the same shape the
//! design's interface names.
//!
//! **Facts, not status.** As with `work_schedule.rs`, ending and pausing a session
//! are later UPDATEs to the one row an agent's dispatch created, not a new table:
//! 0067's data model reserves derived display state (thinking, tooling, retired
//! with an outcome, ...) for a reader that also consults the live event stream,
//! which is not this module's job.
//!
//! **Not derived data.** `agent_sessions`, `session_inbox` and `agent_connections`
//! are absent from `DROP_DERIVED_ON_SCHEMA_CHANGE` in `store.rs`: a session came
//! from a paid run and cannot be rebuilt from source, so a schema-version bump
//! that rebuilds the code index must leave these tables alone.

use crate::error::{GraphError, Result};
use crate::store::GraphStore;
use kgraf_core::RunId;
use kgraf_core::session::{
    ConnectionFacts, Decider, InboxItem, InboxKind, InboxSource, InjectMode, ReasoningVisibility,
    SessionFacts, SessionId, SessionKind, SessionLifecycle, SessionOutcome, SessionRecords,
    SessionStoreError,
};
use rusqlite::{OptionalExtension, params};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// --- Plain-enum <-> TEXT column mapping, in the style of `EdgeKind`/`WorkKind` in
// `store.rs`/`work.rs`: a stable, readable string, not the JSON quoting
// `serde_json` would add if these were run through it like the data-carrying
// enums below are. ---

fn kind_str(k: SessionKind) -> &'static str {
    match k {
        SessionKind::Orchestrator => "orchestrator",
        SessionKind::Worker => "worker",
        SessionKind::Subagent => "subagent",
        SessionKind::Gate => "gate",
        SessionKind::Resolver => "resolver",
    }
}

fn parse_kind(s: &str) -> Option<SessionKind> {
    Some(match s {
        "orchestrator" => SessionKind::Orchestrator,
        "worker" => SessionKind::Worker,
        "subagent" => SessionKind::Subagent,
        "gate" => SessionKind::Gate,
        "resolver" => SessionKind::Resolver,
        _ => return None,
    })
}

fn reasoning_str(v: ReasoningVisibility) -> &'static str {
    match v {
        ReasoningVisibility::Full => "full",
        ReasoningVisibility::Summary => "summary",
        ReasoningVisibility::None => "none",
        ReasoningVisibility::Unknown => "unknown",
    }
}

fn parse_reasoning(s: &str) -> Option<ReasoningVisibility> {
    Some(match s {
        "full" => ReasoningVisibility::Full,
        "summary" => ReasoningVisibility::Summary,
        "none" => ReasoningVisibility::None,
        "unknown" => ReasoningVisibility::Unknown,
        _ => return None,
    })
}

fn outcome_str(o: SessionOutcome) -> &'static str {
    match o {
        SessionOutcome::Completed => "completed",
        SessionOutcome::HandedBack => "handed_back",
        SessionOutcome::Failed => "failed",
        SessionOutcome::Cancelled => "cancelled",
        SessionOutcome::StoppedByUser => "stopped_by_user",
        SessionOutcome::BudgetExhausted => "budget_exhausted",
        SessionOutcome::Stalled => "stalled",
    }
}

fn parse_outcome(s: &str) -> Option<SessionOutcome> {
    Some(match s {
        "completed" => SessionOutcome::Completed,
        "handed_back" => SessionOutcome::HandedBack,
        "failed" => SessionOutcome::Failed,
        "cancelled" => SessionOutcome::Cancelled,
        "stopped_by_user" => SessionOutcome::StoppedByUser,
        "budget_exhausted" => SessionOutcome::BudgetExhausted,
        "stalled" => SessionOutcome::Stalled,
        _ => return None,
    })
}

fn inbox_kind_str(k: InboxKind) -> &'static str {
    match k {
        InboxKind::Note => "note",
        InboxKind::Redirect => "redirect",
        InboxKind::Answer => "answer",
        InboxKind::Message => "message",
        InboxKind::Notice => "notice",
    }
}

fn parse_inbox_kind(s: &str) -> Option<InboxKind> {
    Some(match s {
        "note" => InboxKind::Note,
        "redirect" => InboxKind::Redirect,
        "answer" => InboxKind::Answer,
        "message" => InboxKind::Message,
        "notice" => InboxKind::Notice,
        _ => return None,
    })
}

fn inject_mode_str(m: InjectMode) -> &'static str {
    match m {
        InjectMode::AtBoundary => "at_boundary",
        InjectMode::Interrupt => "interrupt",
    }
}

fn parse_inject_mode(s: &str) -> Option<InjectMode> {
    Some(match s {
        "at_boundary" => InjectMode::AtBoundary,
        "interrupt" => InjectMode::Interrupt,
        _ => return None,
    })
}

fn decider_str(d: Decider) -> &'static str {
    match d {
        Decider::Rule => "rule",
        Decider::Orchestrator => "orchestrator",
        Decider::User => "user",
    }
}

fn parse_decider(s: &str) -> Option<Decider> {
    Some(match s {
        "rule" => Decider::Rule,
        "orchestrator" => Decider::Orchestrator,
        "user" => Decider::User,
        _ => return None,
    })
}

/// A store failure, wrapped as [`GraphError`], turned into the string a
/// [`SessionStoreError::Store`] carries: the seam's error type does not itself
/// borrow SQLite's or serde's error type, so this crate's failures cross it as
/// text, exactly as `kgraf-runtime` and `kgraf-orchestrator` already expect
/// (they depend on `kgraf-core::session`, not on `rusqlite`).
fn store_err(e: impl std::fmt::Display) -> SessionStoreError {
    SessionStoreError::Store(e.to_string())
}

fn encode_write_scope(scope: &[String]) -> Result<String> {
    serde_json::to_string(scope).map_err(|e| GraphError::InvalidWork {
        reason: format!("cannot encode a session's write scope: {e}"),
    })
}

fn decode_write_scope(raw: &str) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn path_to_text(p: &Option<PathBuf>) -> Option<String> {
    p.as_ref().map(|p| p.to_string_lossy().into_owned())
}

/// Whether `agent_sessions` has the `addressable` column (a table created
/// before decision 0067's request 17 addendum does not).
pub(crate) fn has_addressable_column(conn: &rusqlite::Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('agent_sessions') WHERE name = 'addressable'",
        [],
        |row| row.get::<_, i64>(0).map(|n| n > 0),
    )?)
}

const SESSION_FACT_COLUMNS: &str = "id, run_id, kind, parent_id, contract, route, model, depth, \
     transcript, worktree, write_scope, reasoning, started_ms, addressable";

fn session_facts_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionFacts> {
    let kind: String = row.get(2)?;
    let reasoning: String = row.get(11)?;
    let write_scope: String = row.get(10)?;
    Ok(SessionFacts {
        id: SessionId::new(row.get::<_, String>(0)?),
        run: RunId::new(row.get::<_, String>(1)?),
        kind: parse_kind(&kind).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                format!("unknown session kind `{kind}`").into(),
            )
        })?,
        parent: row.get::<_, Option<String>>(3)?.map(SessionId::new),
        contract: row.get(4)?,
        route: row.get(5)?,
        model: row.get(6)?,
        depth: row.get::<_, i64>(7)? as u32,
        transcript: row.get::<_, Option<String>>(8)?.map(PathBuf::from),
        worktree: row.get::<_, Option<String>>(9)?.map(PathBuf::from),
        write_scope: decode_write_scope(&write_scope)?,
        reasoning: parse_reasoning(&reasoning).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                11,
                rusqlite::types::Type::Text,
                format!("unknown reasoning visibility `{reasoning}`").into(),
            )
        })?,
        started_ms: row.get(12)?,
        addressable: row.get::<_, i64>(13)? != 0,
    })
}

fn inbox_item_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InboxItem> {
    let source: String = row.get(2)?;
    let kind: String = row.get(3)?;
    let mode: String = row.get(4)?;
    let delivered_seq: Option<i64> = row.get(8)?;
    let delivered_ms: Option<i64> = row.get(9)?;
    Ok(InboxItem {
        id: row.get(0)?,
        session: SessionId::new(row.get::<_, String>(1)?),
        source: serde_json::from_str(&source).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })?,
        kind: parse_inbox_kind(&kind).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                format!("unknown inbox kind `{kind}`").into(),
            )
        })?,
        mode: parse_inject_mode(&mode).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                format!("unknown inject mode `{mode}`").into(),
            )
        })?,
        text: row.get(5)?,
        received_ms: row.get(6)?,
        not_before_ms: row.get(7)?,
        delivered: match (delivered_seq, delivered_ms) {
            (Some(seq), Some(ms)) => Some((seq as u64, ms)),
            _ => None,
        },
    })
}

fn connection_facts_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConnectionFacts> {
    let basis: String = row.get(4)?;
    let state: String = row.get(5)?;
    let decided_by: Option<String> = row.get(6)?;
    Ok(ConnectionFacts {
        id: row.get(0)?,
        run: RunId::new(row.get::<_, String>(1)?),
        a: SessionId::new(row.get::<_, String>(2)?),
        b: SessionId::new(row.get::<_, String>(3)?),
        basis: serde_json::from_str(&basis).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })?,
        state: serde_json::from_str(&state).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
        })?,
        decided_by: decided_by
            .map(|s| {
                parse_decider(&s).ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        format!("unknown decider `{s}`").into(),
                    )
                })
            })
            .transpose()?,
        decided_ms: row.get(7)?,
        messages: row.get::<_, i64>(8)? as u64,
        last_ms: row.get(9)?,
    })
}

impl GraphStore {
    /// Write a session's facts, keyed by id: a first call inserts, a later call
    /// (a route or model learned after dispatch, for example) overwrites only the
    /// fact columns, never the lifecycle columns [`GraphStore::session_end`] and
    /// [`GraphStore::session_set_paused`] write, so a repeated upsert cannot erase
    /// an ending or a pause that already happened. `started_ms` is the first
    /// write's: a session recorded again (a subagent's row written at spawn and
    /// again at dispatch, say) keeps the time it started.
    pub fn session_upsert(&self, s: &SessionFacts) -> Result<()> {
        let write_scope = encode_write_scope(&s.write_scope)?;
        self.conn().execute(
            "INSERT INTO agent_sessions
                (id, run_id, kind, parent_id, contract, route, model, depth,
                 transcript, worktree, write_scope, reasoning, started_ms, addressable)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(id) DO UPDATE SET
                run_id = excluded.run_id, kind = excluded.kind,
                parent_id = excluded.parent_id, contract = excluded.contract,
                route = excluded.route, model = excluded.model, depth = excluded.depth,
                transcript = excluded.transcript, worktree = excluded.worktree,
                write_scope = excluded.write_scope, reasoning = excluded.reasoning,
                addressable = excluded.addressable",
            params![
                s.id.as_str(),
                s.run.as_str(),
                kind_str(s.kind),
                s.parent.as_ref().map(|p| p.as_str()),
                s.contract,
                s.route,
                s.model,
                s.depth,
                path_to_text(&s.transcript),
                path_to_text(&s.worktree),
                write_scope,
                reasoning_str(s.reasoning),
                s.started_ms,
                s.addressable,
            ],
        )?;
        Ok(())
    }

    /// Mark a session ended. Returns whether it was stored (the caller turns
    /// `false` into [`SessionStoreError::Unknown`]).
    pub fn session_end(&self, id: &SessionId, outcome: SessionOutcome, at_ms: i64) -> Result<bool> {
        let updated = self.conn().execute(
            "UPDATE agent_sessions SET ended_outcome = ?2, ended_ms = ?3 WHERE id = ?1",
            params![id.as_str(), outcome_str(outcome), at_ms],
        )?;
        Ok(updated > 0)
    }

    /// Set (or clear, with `None`) a session's paused time. Returns whether it was
    /// stored.
    pub fn session_set_paused(&self, id: &SessionId, paused_ms: Option<i64>) -> Result<bool> {
        let updated = self.conn().execute(
            "UPDATE agent_sessions SET paused_ms = ?2 WHERE id = ?1",
            params![id.as_str(), paused_ms],
        )?;
        Ok(updated > 0)
    }

    /// The lifecycle facts recorded for a session: `None` if it is not stored at
    /// all, matching [`SessionRecords::lifecycle`]'s contract.
    pub fn session_lifecycle(&self, id: &SessionId) -> Result<Option<SessionLifecycle>> {
        let row: Option<(Option<String>, Option<i64>, Option<i64>)> = self
            .conn()
            .query_row(
                "SELECT ended_outcome, ended_ms, paused_ms FROM agent_sessions WHERE id = ?1",
                params![id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((ended_outcome, ended_ms, paused_ms)) = row else {
            return Ok(None);
        };
        let ended = match (ended_outcome, ended_ms) {
            (Some(o), Some(ms)) => Some((
                parse_outcome(&o).ok_or_else(|| GraphError::InvalidWork {
                    reason: format!("unknown session outcome `{o}` stored for `{id}`"),
                })?,
                ms,
            )),
            _ => None,
        };
        Ok(Some((ended, paused_ms)))
    }

    /// One session's facts by id.
    pub fn session_by_id(&self, id: &SessionId) -> Result<Option<SessionFacts>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {SESSION_FACT_COLUMNS} FROM agent_sessions WHERE id = ?1"),
                params![id.as_str()],
                session_facts_from_row,
            )
            .optional()?)
    }

    /// Every session's facts belonging to one run.
    pub fn sessions_of_run(&self, run: &RunId) -> Result<Vec<SessionFacts>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {SESSION_FACT_COLUMNS} FROM agent_sessions WHERE run_id = ?1 ORDER BY started_ms, id"
        ))?;
        let rows = stmt.query_map(params![run.as_str()], session_facts_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every session, across every run: used by [`read_session_graph`] when no run
    /// is named.
    pub fn all_sessions(&self) -> Result<Vec<SessionFacts>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {SESSION_FACT_COLUMNS} FROM agent_sessions ORDER BY started_ms, id"
        ))?;
        let rows = stmt.query_map([], session_facts_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A session's direct children.
    pub fn session_children(&self, id: &SessionId) -> Result<Vec<SessionFacts>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {SESSION_FACT_COLUMNS} FROM agent_sessions WHERE parent_id = ?1 ORDER BY started_ms, id"
        ))?;
        let rows = stmt.query_map(params![id.as_str()], session_facts_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Append one message to a session's inbox. The text is stored
    /// persist-scrubbed (decision 0071): graph.db is read by the shell
    /// and search. The reader that injects it into a worker gives the model
    /// `kgraf_secrets::model_copy` of the stored text, which is the
    /// registry-only copy while this process holds it.
    pub fn inbox_push(&self, item: &InboxItem) -> Result<()> {
        let source = serde_json::to_string(&item.source).map_err(|e| GraphError::InvalidWork {
            reason: format!("cannot encode an inbox message's source: {e}"),
        })?;
        self.conn().execute(
            "INSERT INTO session_inbox
                (id, session_id, source, kind, mode, text, received_ms, not_before_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                item.id,
                item.session.as_str(),
                source,
                inbox_kind_str(item.kind),
                inject_mode_str(item.mode),
                kgraf_secrets::intake(&item.text),
                item.received_ms,
                item.not_before_ms,
            ],
        )?;
        Ok(())
    }

    /// Mark an inbox item delivered. `Ok(None)` means no such item is stored;
    /// `Ok(Some(false))` means it was already marked delivered (refused, not
    /// silently re-accepted, per [`SessionRecords::mark_delivered`]'s contract);
    /// `Ok(Some(true))` means this call marked it.
    pub fn inbox_mark_delivered(&self, item: &str, seq: u64, at_ms: i64) -> Result<Option<bool>> {
        let existing: Option<Option<i64>> = self
            .conn()
            .query_row(
                "SELECT delivered_seq FROM session_inbox WHERE id = ?1",
                params![item],
                |r| r.get(0),
            )
            .optional()?;
        let Some(delivered_seq) = existing else {
            return Ok(None);
        };
        if delivered_seq.is_some() {
            return Ok(Some(false));
        }
        self.conn().execute(
            "UPDATE session_inbox SET delivered_seq = ?2, delivered_ms = ?3 WHERE id = ?1",
            params![item, seq as i64, at_ms],
        )?;
        Ok(Some(true))
    }

    /// A session's undelivered messages, oldest first (the order they were
    /// pushed).
    pub fn inbox_undelivered(&self, session: &SessionId) -> Result<Vec<InboxItem>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, session_id, source, kind, mode, text, received_ms, not_before_ms, \
                    delivered_seq, delivered_ms
             FROM session_inbox WHERE session_id = ?1 AND delivered_seq IS NULL ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![session.as_str()], inbox_item_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// One inbox item by id: so a caller that only
    /// has an id (the runtime's `Injector::undelivered`) can find who to
    /// report a never-delivered note back to (`InboxItem::source`).
    pub fn inbox_item(&self, id: &str) -> Result<Option<InboxItem>> {
        self.conn()
            .query_row(
                "SELECT id, session_id, source, kind, mode, text, received_ms, not_before_ms, \
                        delivered_seq, delivered_ms
                 FROM session_inbox WHERE id = ?1",
                params![id],
                inbox_item_from_row,
            )
            .optional()
            .map_err(GraphError::from)
    }

    /// Write a connection's facts, keyed by its canonical pair id
    /// ([`kgraf_core::session::connection_id`]): a first call inserts, a later
    /// call (state moving from pending to open, a message count climbing)
    /// overwrites the row in place, matching `ConnectionFacts` being read back
    /// whole rather than as separate lifecycle facts.
    pub fn connection_upsert(&self, c: &ConnectionFacts) -> Result<()> {
        let basis = serde_json::to_string(&c.basis).map_err(|e| GraphError::InvalidWork {
            reason: format!("cannot encode a connection's basis: {e}"),
        })?;
        let state = serde_json::to_string(&c.state).map_err(|e| GraphError::InvalidWork {
            reason: format!("cannot encode a connection's state: {e}"),
        })?;
        self.conn().execute(
            "INSERT INTO agent_connections
                (id, run_id, a, b, basis, state, decided_by, decided_ms, messages, last_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                run_id = excluded.run_id, a = excluded.a, b = excluded.b,
                basis = excluded.basis, state = excluded.state,
                decided_by = excluded.decided_by, decided_ms = excluded.decided_ms,
                messages = excluded.messages, last_ms = excluded.last_ms",
            params![
                c.id,
                c.run.as_str(),
                c.a.as_str(),
                c.b.as_str(),
                basis,
                state,
                c.decided_by.map(decider_str),
                c.decided_ms,
                c.messages as i64,
                c.last_ms,
            ],
        )?;
        Ok(())
    }

    /// Every connection naming `session`, by either side.
    pub fn connections_of(&self, session: &SessionId) -> Result<Vec<ConnectionFacts>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, run_id, a, b, basis, state, decided_by, decided_ms, messages, last_ms
             FROM agent_connections WHERE a = ?1 OR b = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![session.as_str()], connection_facts_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every connection in one run.
    pub fn connections_of_run(&self, run: &RunId) -> Result<Vec<ConnectionFacts>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, run_id, a, b, basis, state, decided_by, decided_ms, messages, last_ms
             FROM agent_connections WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![run.as_str()], connection_facts_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// A run's sessions and their edges, read for the galaxy to draw: every
/// session that has no lease still gets a node this way, because a session row
/// exists at dispatch whether or not the agent holds a write lease.
pub struct SessionGraph {
    pub nodes: Vec<SessionFacts>,
    /// Parent-to-child pairs, directed. Per 0067's data model, this excludes the
    /// ordinary orchestrator-to-worker dispatch edge ("there is no edge from the
    /// orchestrator to each worker... N edges to one hub would add clutter and no
    /// information"): an edge is included only when the parent is not itself the
    /// orchestrator, which is exactly the subagent and handoff-child case the
    /// design calls out for the `spawned` edge.
    pub spawned: Vec<(SessionId, SessionId)>,
    pub connections: Vec<ConnectionFacts>,
    /// Sessions with an inter-agent message paced past its arrival: `connections.rs`'s
    /// `Pacer` sets an inbox row's `not_before_ms` ahead of `received_ms` once a
    /// sender's rate on a connection crosses the soft rate (an
    /// unusual number of connections or message volume raises a visible notice).
    /// This is that notice itself, read back from the fact the pacer already wrote, not
    /// a second, independently-tuned rule: a caller (the galaxy's badge) that wants to
    /// know a session is over the soft limit reads this set, never a message count of
    /// its own.
    pub paced: std::collections::BTreeSet<SessionId>,
}

/// Read one run's session graph (every run's, if `run` is `None`) for the galaxy.
pub fn read_session_graph(store: &GraphStore, run: Option<&RunId>) -> Result<SessionGraph> {
    let nodes = match run {
        Some(run) => store.sessions_of_run(run)?,
        None => store.all_sessions()?,
    };
    let mut paced = std::collections::BTreeSet::new();
    for s in &nodes {
        let over = store.inbox_undelivered(&s.id)?.iter().any(|i| {
            i.not_before_ms > i.received_ms && matches!(i.source, InboxSource::Agent { .. })
        });
        if over {
            paced.insert(s.id.clone());
        }
    }
    let kind_of: std::collections::BTreeMap<&SessionId, SessionKind> =
        nodes.iter().map(|s| (&s.id, s.kind)).collect();
    let spawned = nodes
        .iter()
        .filter_map(|s| {
            let parent = s.parent.as_ref()?;
            let parent_is_orchestrator = kind_of.get(parent) == Some(&SessionKind::Orchestrator);
            (!parent_is_orchestrator).then(|| (parent.clone(), s.id.clone()))
        })
        .collect();
    let connections = match run {
        Some(run) => store.connections_of_run(run)?,
        None => {
            let mut all = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            for s in &nodes {
                for c in store.connections_of(&s.id)? {
                    if seen.insert(c.id.clone()) {
                        all.push(c);
                    }
                }
            }
            all
        }
    };
    Ok(SessionGraph {
        nodes,
        spawned,
        connections,
        paced,
    })
}

/// The durable [`SessionRecords`]: a [`GraphStore`] behind a mutex, so several
/// owners (the host writing, the shell reading facts) can share one connection.
/// The mutex, not `GraphStore` itself, is what makes this `Sync`: `rusqlite`'s
/// `Connection` is `Send` but not `Sync`.
pub struct SharedSessions(pub Arc<Mutex<GraphStore>>);

impl SharedSessions {
    fn lock(
        &self,
    ) -> std::result::Result<std::sync::MutexGuard<'_, GraphStore>, SessionStoreError> {
        self.0
            .lock()
            .map_err(|_| SessionStoreError::Store("the shared graph store is poisoned".into()))
    }
}

impl SessionRecords for SharedSessions {
    fn upsert_session(&self, s: &SessionFacts) -> std::result::Result<(), SessionStoreError> {
        self.lock()?.session_upsert(s).map_err(store_err)
    }

    fn end_session(
        &self,
        id: &SessionId,
        outcome: SessionOutcome,
        at_ms: i64,
    ) -> std::result::Result<(), SessionStoreError> {
        let existed = self
            .lock()?
            .session_end(id, outcome, at_ms)
            .map_err(store_err)?;
        if existed {
            Ok(())
        } else {
            Err(SessionStoreError::Unknown(id.to_string()))
        }
    }

    fn set_paused(
        &self,
        id: &SessionId,
        paused_ms: Option<i64>,
    ) -> std::result::Result<(), SessionStoreError> {
        let existed = self
            .lock()?
            .session_set_paused(id, paused_ms)
            .map_err(store_err)?;
        if existed {
            Ok(())
        } else {
            Err(SessionStoreError::Unknown(id.to_string()))
        }
    }

    fn lifecycle(
        &self,
        id: &SessionId,
    ) -> std::result::Result<Option<SessionLifecycle>, SessionStoreError> {
        self.lock()?.session_lifecycle(id).map_err(store_err)
    }

    fn session(
        &self,
        id: &SessionId,
    ) -> std::result::Result<Option<SessionFacts>, SessionStoreError> {
        self.lock()?.session_by_id(id).map_err(store_err)
    }

    fn sessions_of_run(
        &self,
        run: &RunId,
    ) -> std::result::Result<Vec<SessionFacts>, SessionStoreError> {
        self.lock()?.sessions_of_run(run).map_err(store_err)
    }

    fn children(
        &self,
        id: &SessionId,
    ) -> std::result::Result<Vec<SessionFacts>, SessionStoreError> {
        self.lock()?.session_children(id).map_err(store_err)
    }

    fn push_inbox(&self, item: &InboxItem) -> std::result::Result<(), SessionStoreError> {
        self.lock()?.inbox_push(item).map_err(store_err)
    }

    fn mark_delivered(
        &self,
        item: &str,
        seq: u64,
        at_ms: i64,
    ) -> std::result::Result<(), SessionStoreError> {
        match self
            .lock()?
            .inbox_mark_delivered(item, seq, at_ms)
            .map_err(store_err)?
        {
            None => Err(SessionStoreError::Unknown(item.to_owned())),
            Some(false) => Err(SessionStoreError::Store(format!(
                "inbox item `{item}` was already marked delivered"
            ))),
            Some(true) => Ok(()),
        }
    }

    fn undelivered(
        &self,
        session: &SessionId,
    ) -> std::result::Result<Vec<InboxItem>, SessionStoreError> {
        self.lock()?.inbox_undelivered(session).map_err(store_err)
    }

    fn inbox_item(&self, id: &str) -> std::result::Result<Option<InboxItem>, SessionStoreError> {
        self.lock()?.inbox_item(id).map_err(store_err)
    }

    fn upsert_connection(&self, c: &ConnectionFacts) -> std::result::Result<(), SessionStoreError> {
        self.lock()?.connection_upsert(c).map_err(store_err)
    }

    fn connections_of(
        &self,
        session: &SessionId,
    ) -> std::result::Result<Vec<ConnectionFacts>, SessionStoreError> {
        self.lock()?.connections_of(session).map_err(store_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ChangeOp, EventTail};
    use kgraf_core::session::{
        ConnectionBasis, ConnectionState, InboxSource, check_session_records,
    };

    /// An in-memory store never touches disk, so `root` need not be a real,
    /// writable directory (`facts.rs`'s own in-memory tests use the same `"."`):
    /// no test here resolves or creates a path outside its own temp directories.
    fn store() -> GraphStore {
        GraphStore::open_in_memory(std::path::Path::new(".")).expect("store")
    }

    /// An index written before `addressable` existed opens, gains
    /// the column, and reads its rows as addressable (the field's default).
    #[test]
    fn an_index_from_before_addressable_opens_and_reads_its_sessions_as_addressable() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("graph.db");
        let run1 = facts("orchestrator:r1", "r1", None, SessionKind::Orchestrator);
        {
            let store = GraphStore::open(&db, dir.path()).expect("open");
            store.session_upsert(&run1).expect("upsert");
        }
        {
            // The table as a build before the addressable column wrote it.
            let conn = rusqlite::Connection::open(&db).expect("raw");
            conn.execute_batch("ALTER TABLE agent_sessions DROP COLUMN addressable;")
                .expect("drop the column");
            assert!(!has_addressable_column(&conn).expect("read"));
        }
        let store = GraphStore::open(&db, dir.path()).expect("an old index opens");
        assert!(has_addressable_column(store.conn()).expect("read"));
        let read = store
            .session_by_id(&run1.id)
            .expect("read")
            .expect("the old row");
        assert!(read.addressable, "an old row reads as addressable");
        drop(store);
        // Opening again is a no-op: the migration does not run twice.
        GraphStore::open(&db, dir.path()).expect("open again");
    }

    /// A second upsert of one session keeps its
    /// `started_ms`.
    #[test]
    fn a_repeated_upsert_keeps_the_first_start_time() {
        let store = store();
        let mut s = facts("orchestrator:r1", "r1", None, SessionKind::Orchestrator);
        s.started_ms = 10;
        store.session_upsert(&s).expect("first");
        s.started_ms = 99;
        s.model = Some("m".into());
        store.session_upsert(&s).expect("second");
        let read = store.session_by_id(&s.id).expect("read").expect("row");
        assert_eq!(read.started_ms, 10, "the first start time stands");
        assert_eq!(read.model.as_deref(), Some("m"), "other facts update");
    }

    #[test]
    fn shared_sessions_passes_the_session_records_contract() {
        let shared = SharedSessions(Arc::new(Mutex::new(store())));
        check_session_records(&shared);
    }

    fn facts(id: &str, run: &str, parent: Option<&str>, kind: SessionKind) -> SessionFacts {
        SessionFacts {
            id: SessionId::new(id),
            run: RunId::new(run),
            kind,
            parent: parent.map(SessionId::new),
            contract: None,
            route: None,
            model: None,
            depth: 0,
            transcript: None,
            worktree: None,
            write_scope: Vec::new(),
            reasoning: ReasoningVisibility::Unknown,
            started_ms: 1,
            addressable: true,
        }
    }

    /// Decision 0071: an inbox message is stored persist-scrubbed in
    /// graph.db, and the model copy maps back from the stored text.
    #[test]
    fn an_inbox_message_is_stored_scrubbed() {
        let store = store();
        let worker = facts("run-1-c-r0-a0", "run-1", None, SessionKind::Worker);
        store.session_upsert(&worker).expect("upsert");
        let token = format!("ghp_{}", "Mn6Bv8Cx0Za2Sd4Fg6Hj8Kl0Po2Iu4Yt6Re8W");
        let item = InboxItem {
            id: "msg-1".into(),
            session: worker.id.clone(),
            source: InboxSource::User,
            kind: InboxKind::Note,
            mode: InjectMode::AtBoundary,
            text: format!("inbox message uses {token}"),
            received_ms: 3,
            not_before_ms: 3,
            delivered: None,
        };
        store.inbox_push(&item).expect("push");
        let back = store.inbox_undelivered(&worker.id).expect("read");
        assert!(!back[0].text.contains(&token), "{}", back[0].text);
        assert!(back[0].text.contains("[redacted:github-pat@"));
        assert!(kgraf_secrets::model_copy(&back[0].text).contains(&token));
    }

    /// An insert and an update on each of the three tables write a
    /// change-log row whose node is `agent:<id>`, so the activity card can route
    /// (0063).
    #[test]
    fn session_events_route_back_to_the_agent_node() {
        let store = store();
        let mut tail = EventTail::from_now(&store).expect("tail");

        let orchestrator = facts(
            "orchestrator:run-1",
            "run-1",
            None,
            SessionKind::Orchestrator,
        );
        let worker = facts(
            "run-1-contract-r0-a0",
            "run-1",
            Some("orchestrator:run-1"),
            SessionKind::Worker,
        );
        store.session_upsert(&orchestrator).expect("upsert parent");
        store.session_upsert(&worker).expect("upsert child");
        store
            .session_end(&worker.id, SessionOutcome::Completed, 2)
            .expect("end");

        let item = InboxItem {
            id: "msg-1".into(),
            session: worker.id.clone(),
            source: InboxSource::User,
            kind: InboxKind::Note,
            mode: InjectMode::AtBoundary,
            text: "hello".into(),
            received_ms: 3,
            not_before_ms: 3,
            delivered: None,
        };
        store.inbox_push(&item).expect("push");
        store.inbox_mark_delivered("msg-1", 7, 8).expect("deliver");

        let conn = ConnectionFacts {
            id: kgraf_core::session::connection_id(&orchestrator.id, &worker.id),
            run: RunId::new("run-1"),
            a: orchestrator.id.clone(),
            b: worker.id.clone(),
            basis: ConnectionBasis::ParentChild,
            state: ConnectionState::Open,
            decided_by: Some(Decider::Rule),
            decided_ms: Some(1),
            messages: 0,
            last_ms: None,
        };
        store.connection_upsert(&conn).expect("connection insert");
        store
            .connection_upsert(&ConnectionFacts {
                messages: 1,
                ..conn.clone()
            })
            .expect("connection update");

        let batch = tail.next_batch(&store, 100).expect("batch");
        // Checking only `table` and `row_id` cannot tell an `AFTER INSERT` trigger
        // from an `AFTER UPDATE` one wired to the wrong event: both would produce a
        // row with the same table and the same `agent:<id>` prefix. `op` and an
        // exact count per (table, op, id) close that gap.
        let count = |table: &str, op: ChangeOp, id: &str| {
            batch
                .iter()
                .filter(|c| c.table == table && c.op == op && c.row_id == format!("agent:{id}"))
                .count()
        };
        // agent_sessions: two inserts (the orchestrator and the worker), one update
        // (ending the worker).
        assert_eq!(
            count("agent_sessions", ChangeOp::Insert, "orchestrator:run-1"),
            1
        );
        assert_eq!(
            count("agent_sessions", ChangeOp::Insert, "run-1-contract-r0-a0"),
            1
        );
        assert_eq!(
            count("agent_sessions", ChangeOp::Update, "run-1-contract-r0-a0"),
            1
        );
        assert_eq!(
            count("agent_sessions", ChangeOp::Update, "orchestrator:run-1"),
            0,
            "only the worker's row was updated, not the orchestrator's"
        );
        // session_inbox: one insert (the push), one update (the delivery), both
        // named on the recipient (the worker), not on the sender.
        assert_eq!(
            count("session_inbox", ChangeOp::Insert, "run-1-contract-r0-a0"),
            1
        );
        assert_eq!(
            count("session_inbox", ChangeOp::Update, "run-1-contract-r0-a0"),
            1
        );
        assert_eq!(
            count("session_inbox", ChangeOp::Insert, "orchestrator:run-1"),
            0,
            "an inbox row is heard on its recipient, not on every session"
        );
        // agent_connections: one insert and one update, each heard on both sides.
        for id in ["orchestrator:run-1", "run-1-contract-r0-a0"] {
            assert_eq!(count("agent_connections", ChangeOp::Insert, id), 1);
            assert_eq!(count("agent_connections", ChangeOp::Update, id), 1);
        }
    }

    /// A session's inbox row must be heard as one `insert`, never
    /// as an `update` mislabeled as an insert (or the reverse). This is the test
    /// the trigger-swap mutation below is run against.
    #[test]
    fn a_pushed_inbox_message_writes_exactly_one_insert_row_before_delivery() {
        let store = store();
        let mut tail = EventTail::from_now(&store).expect("tail");
        let worker = facts("run-1-contract-r0-a0", "run-1", None, SessionKind::Worker);
        store.session_upsert(&worker).expect("upsert");
        let mut before = tail
            .next_batch(&store, 100)
            .expect("drain the session insert");
        before.clear();

        let item = InboxItem {
            id: "msg-1".into(),
            session: worker.id.clone(),
            source: InboxSource::User,
            kind: InboxKind::Note,
            mode: InjectMode::AtBoundary,
            text: "hello".into(),
            received_ms: 1,
            not_before_ms: 1,
            delivered: None,
        };
        store.inbox_push(&item).expect("push");

        let batch = tail.next_batch(&store, 100).expect("batch");
        let session_inbox_rows: Vec<_> = batch
            .iter()
            .filter(|c| c.table == "session_inbox")
            .collect();
        assert_eq!(
            session_inbox_rows.len(),
            1,
            "exactly one session_inbox row, {batch:?}"
        );
        assert_eq!(
            session_inbox_rows[0].op,
            ChangeOp::Insert,
            "a push with no delivery yet must be recorded as an insert, not an update"
        );
    }

    /// The parent tree walks from a subagent up to the orchestrator.
    #[test]
    fn the_parent_tree_walks_from_a_subagent_to_the_orchestrator() {
        let store = store();
        let orchestrator = facts(
            "orchestrator:run-1",
            "run-1",
            None,
            SessionKind::Orchestrator,
        );
        let worker = facts(
            "run-1-contract-r0-a0",
            "run-1",
            Some("orchestrator:run-1"),
            SessionKind::Worker,
        );
        let subagent = facts(
            "run-1-contract-r0-a0.s0",
            "run-1",
            Some("run-1-contract-r0-a0"),
            SessionKind::Subagent,
        );
        store.session_upsert(&orchestrator).expect("orchestrator");
        store.session_upsert(&worker).expect("worker");
        store.session_upsert(&subagent).expect("subagent");

        let mut node = store
            .session_by_id(&subagent.id)
            .expect("read")
            .expect("some");
        let mut chain = vec![node.id.clone()];
        while let Some(parent) = node.parent.clone() {
            chain.push(parent.clone());
            node = store.session_by_id(&parent).expect("read").expect("some");
        }
        assert_eq!(
            chain,
            vec![
                subagent.id.clone(),
                worker.id.clone(),
                orchestrator.id.clone()
            ]
        );

        let graph = read_session_graph(&store, Some(&RunId::new("run-1"))).expect("graph");
        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(
            graph.spawned,
            vec![(worker.id.clone(), subagent.id.clone())],
            "the orchestrator-to-worker edge is excluded; only the subagent edge remains"
        );
    }

    /// Decision 0067: `paced` reads back exactly what
    /// `connections.rs`'s `Pacer` already wrote (`not_before_ms` ahead of
    /// `received_ms`), not a re-derived message count. A note from the user or
    /// orchestrator, and a message delivered on time, name no session.
    #[test]
    fn a_session_reads_as_paced_only_from_a_delayed_agent_message() {
        let store = store();
        let a = facts("run-1-c-r0-a0", "run-1", None, SessionKind::Worker);
        let b = facts("run-1-c-r0-a1", "run-1", None, SessionKind::Worker);
        let c = facts("run-1-c-r0-a2", "run-1", None, SessionKind::Worker);
        for s in [&a, &b, &c] {
            store.session_upsert(s).expect("upsert");
        }
        let mut n = 0;
        let mut msg = |session: &SessionId, source: InboxSource, received: i64, not_before: i64| {
            n += 1;
            InboxItem {
                id: format!("msg-{session}-{n}"),
                session: session.clone(),
                source,
                kind: InboxKind::Message,
                mode: InjectMode::AtBoundary,
                text: "hello".into(),
                received_ms: received,
                not_before_ms: not_before,
                delivered: None,
            }
        };
        // b: an agent message paced past its arrival -- over the soft limit.
        store
            .inbox_push(&msg(
                &b.id,
                InboxSource::Agent { id: a.id.clone() },
                100,
                5_000,
            ))
            .expect("push");
        // c: an agent message delivered on time, and a user note paced (a note is
        // never rate-limited; only agent-to-agent delivery is), so c is not paced.
        store
            .inbox_push(&msg(
                &c.id,
                InboxSource::Agent { id: a.id.clone() },
                100,
                100,
            ))
            .expect("push");
        store
            .inbox_push(&msg(&c.id, InboxSource::User, 100, 9_000))
            .expect("push");

        let graph = read_session_graph(&store, Some(&RunId::new("run-1"))).expect("graph");
        assert_eq!(
            graph.paced,
            std::collections::BTreeSet::from([b.id.clone()])
        );
        assert!(!graph.paced.contains(&a.id));
        assert!(!graph.paced.contains(&c.id));
    }

    /// Opening an existing schema-4 `graph.db` for facts adds the new
    /// tables without a rebuild, following the pattern `work_schedule.rs`'s own
    /// `opening_for_facts_never_creates_or_rebuilds` test set for that unit.
    #[test]
    fn opening_an_existing_index_for_facts_adds_the_session_tables() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("graph.db");
        {
            // A build before sessions existed: create the index with no session rows at
            // all, as any schema-4 index on disk today would have.
            let store = GraphStore::open(&db, dir.path()).expect("store");
            drop(store);
        }
        let store = GraphStore::open_existing_for_facts(&db, dir.path()).expect("same version");
        let s = facts(
            "orchestrator:run-1",
            "run-1",
            None,
            SessionKind::Orchestrator,
        );
        store
            .session_upsert(&s)
            .expect("the table exists and is writable");
        assert_eq!(store.session_by_id(&s.id).expect("read").as_ref(), Some(&s));

        store.set_meta("schema_version", "0").expect("age it");
        drop(store);
        assert!(
            matches!(
                GraphStore::open_existing_for_facts(&db, dir.path()),
                Err(GraphError::SchemaMismatch { found: Some(0), .. })
            ),
            "a mismatched version is refused, not silently rebuilt"
        );
        let raw = rusqlite::Connection::open(&db).expect("raw");
        let session_count: i64 = raw
            .query_row("SELECT COUNT(*) FROM agent_sessions", [], |r| r.get(0))
            .expect("the table survives the refused open");
        assert_eq!(session_count, 1, "the refused open did not rebuild");
    }

    /// Load-bearing for the "not derived
    /// data" claim this module's doc comment makes: a schema-version bump
    /// rebuilds the code index (`GraphStore::open`'s `Rebuild::Allowed` branch,
    /// `DROP_DERIVED_ON_SCHEMA_CHANGE`), and session facts must survive it exactly
    /// as `work_schedule.rs`'s own `schedules_and_history_survive_a_schema_rebuild`
    /// requires for schedules.
    #[test]
    fn sessions_survive_a_schema_rebuild() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("graph.db");
        {
            let store = GraphStore::open(&db, dir.path()).expect("store");
            let s = facts(
                "orchestrator:run-1",
                "run-1",
                None,
                SessionKind::Orchestrator,
            );
            store.session_upsert(&s).expect("write");
            store
                .set_meta("schema_version", "0")
                .expect("force a rebuild on next open");
        }
        let store = GraphStore::open(&db, dir.path()).expect("reopen, rebuilding derived data");
        assert_eq!(
            store
                .session_by_id(&SessionId::new("orchestrator:run-1"))
                .expect("read")
                .map(|s| s.kind),
            Some(SessionKind::Orchestrator),
            "a session row must survive a schema-version rebuild, unlike derived index data"
        );
    }

    /// A facts-only open (the shell's) can read the session tables, not
    /// only write them.
    #[test]
    fn a_facts_only_open_can_read_sessions() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("graph.db");
        {
            let store = GraphStore::open(&db, dir.path()).expect("store");
            let s = facts(
                "orchestrator:run-1",
                "run-1",
                None,
                SessionKind::Orchestrator,
            );
            store.session_upsert(&s).expect("write");
        }
        let store = GraphStore::open_existing_for_facts(&db, dir.path()).expect("open for facts");
        let read = store
            .session_by_id(&SessionId::new("orchestrator:run-1"))
            .expect("read")
            .expect("some");
        assert_eq!(read.kind, SessionKind::Orchestrator);
    }

    /// `connections_of_run` filters by `run_id` alone, so without
    /// `agent_connections_run` it would be a full scan of `agent_connections`
    /// (fine empty in a test, but not at the scale a long-running project's index
    /// reaches). `EXPLAIN QUERY PLAN` on the exact query `connections_of_run` runs
    /// must show the index, not a table scan.
    #[test]
    fn connections_of_run_uses_its_index() {
        let store = store();
        let mut stmt = store
            .conn()
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT id, run_id, a, b, basis, state, decided_by, decided_ms, messages, last_ms
                 FROM agent_connections WHERE run_id = ?1 ORDER BY id",
            )
            .expect("prepare");
        let plan: Vec<String> = stmt
            .query_map(params!["run-1"], |r| r.get::<_, String>(3))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows");
        let plan_text = plan.join(" | ");
        assert!(
            plan_text.contains("agent_connections_run"),
            "expected the plan to use `agent_connections_run`: {plan_text}"
        );
        assert!(
            !plan_text.contains("SCAN agent_connections"),
            "expected no full scan of agent_connections: {plan_text}"
        );
    }
}
