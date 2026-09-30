//! Every agent is an addressable session (decision 0067): shared types for a
//! session, its inbox and its connections to other sessions, plus the
//! [`SessionRecords`] seam a graph store implements and every other crate
//! reads and writes through, so `kgraf-runtime` and `kgraf-orchestrator` never
//! depend on `kgraf-graph` directly for this state.
//!
//! **Facts, derived state.** [`SessionFacts`] is what is written once, at
//! dispatch: it names no lifecycle. Ending, pausing and resuming are separate
//! facts ([`SessionRecords::end_session`], [`SessionRecords::set_paused`]); a
//! session's live state (thinking, tooling, awaiting an answer, retired with an
//! outcome, ...) is derived at read time from those rows together with the live
//! event stream (0007), and that derivation is not this crate's job.
//!
//! The implementation is `kgraf-graph`'s `SharedSessions`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A session id is the existing worker [`crate::AgentId`] (decision 0067,
/// point 1): the orchestrator, each worker attempt, each handoff child and
/// each subagent is already identified by one, so no second id is minted.
pub type SessionId = crate::AgentId;

/// What kind of session a row names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Orchestrator,
    Worker,
    Subagent,
    Gate,
    /// A merge-conflict resolver (decision 0073): a
    /// short-lived worker the orchestrator dispatches itself, never a user or
    /// another agent's choice. Always `addressable: false`.
    Resolver,
}

/// What reasoning a session's model returns, as a model limit rather than a
/// bug (decision 0067, point 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningVisibility {
    Full,
    Summary,
    None,
    Unknown,
}

/// One session's facts, written once at dispatch. See the module docs for what
/// is deliberately not here (lifecycle, which is a separate fact).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFacts {
    pub id: SessionId,
    pub run: crate::RunId,
    pub kind: SessionKind,
    pub parent: Option<SessionId>,
    pub contract: Option<String>,
    pub route: Option<String>,
    pub model: Option<String>,
    pub depth: u32,
    pub transcript: Option<PathBuf>,
    pub worktree: Option<PathBuf>,
    pub write_scope: Vec<String>,
    pub reasoning: ReasoningVisibility,
    pub started_ms: i64,
    /// Decision 0067: `false` for a session that never takes a user note
    /// (a future merge-conflict resolver). `true` for every ordinary worker,
    /// subagent and the orchestrator itself. Defaulted on read so a row a
    /// pre-addendum writer left carries neither key.
    #[serde(default = "default_addressable")]
    pub addressable: bool,
}

fn default_addressable() -> bool {
    true
}

/// How a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOutcome {
    Completed,
    HandedBack,
    Failed,
    Cancelled,
    StoppedByUser,
    BudgetExhausted,
    Stalled,
}

/// Who sent an inbox message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboxSource {
    User,
    Orchestrator,
    Agent { id: SessionId },
}

impl std::fmt::Display for InboxSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InboxSource::User => f.write_str("user"),
            InboxSource::Orchestrator => f.write_str("orchestrator"),
            InboxSource::Agent { id } => write!(f, "agent {id}"),
        }
    }
}

/// What kind of message an inbox row carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    Note,
    Redirect,
    Answer,
    Message,
    Notice,
}

/// When a message is injected into the recipient's transcript (decision 0067,
/// point 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectMode {
    AtBoundary,
    Interrupt,
}

/// One message to a session, and its delivery record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: String,
    pub session: SessionId,
    pub source: InboxSource,
    pub kind: InboxKind,
    pub mode: InjectMode,
    pub text: String,
    pub received_ms: i64,
    pub not_before_ms: i64,
    /// The transcript sequence number and time it was delivered at, once it has
    /// been.
    pub delivered: Option<(u64, i64)>,
}

/// Why a connection between two sessions exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectionBasis {
    ParentChild,
    Dependency { on: String },
    SharedRead { path: String },
    Sibling,
    Requested { reason: String },
}

/// A connection's state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectionState {
    Pending,
    Open,
    Refused { reason: String },
    Closed,
}

/// Who decided a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decider {
    Rule,
    Orchestrator,
    User,
}

/// One connection between two sessions in one run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionFacts {
    pub id: String,
    pub run: crate::RunId,
    pub a: SessionId,
    pub b: SessionId,
    pub basis: ConnectionBasis,
    pub state: ConnectionState,
    pub decided_by: Option<Decider>,
    pub decided_ms: Option<i64>,
    pub messages: u64,
    pub last_ms: Option<i64>,
}

/// The canonical id of the connection between `a` and `b`: the two ids in
/// ascending order, in the same order the worker-to-worker pair files use
/// (0026), so a pair always resolves to one row whichever order it is named
/// in.
///
/// **Length-prefixed** ([`crate::stable_id`]'s own technique, applied here to
/// a readable string instead of a hash): a session id may itself contain
/// `--` (an `AgentId` is not restricted the way `plan::sanitize_id` restricts
/// a contract id), so joining two ids on a bare separator can collide --
/// `("a", "b--c")` and `("a--b", "c")` would both join to `"a--b--c"`. Each
/// id's byte length goes before it, so the two cases above join to
/// `"1:a--4:b--c"` and `"4:a--b--1:c"`: different strings, and either one is
/// unambiguous to split back apart (a test below asserts exactly this pair).
pub fn connection_id(a: &SessionId, b: &SessionId) -> String {
    let (a, b) = (a.as_str(), b.as_str());
    let (x, y) = if a <= b { (a, b) } else { (b, a) };
    format!("{}:{x}--{}:{y}", x.len(), y.len())
}

/// Why a session store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionStoreError {
    #[error("{0}")]
    Store(String),
    #[error("unknown session `{0}`")]
    Unknown(String),
}

/// [`SessionRecords::lifecycle`]'s reading: `(ended, paused_ms)`. See that
/// method's doc comment for what each half means.
pub type SessionLifecycle = (Option<(SessionOutcome, i64)>, Option<i64>);

/// Where session, inbox and connection facts are read and written. Implemented
/// by `kgraf-graph`'s `SharedSessions`, so `kgraf-runtime` and `kgraf-orchestrator` depend on this trait, not
/// on a graph store.
pub trait SessionRecords: Send + Sync {
    fn upsert_session(&self, s: &SessionFacts) -> Result<(), SessionStoreError>;
    fn end_session(
        &self,
        id: &SessionId,
        outcome: SessionOutcome,
        at_ms: i64,
    ) -> Result<(), SessionStoreError>;
    fn set_paused(&self, id: &SessionId, paused_ms: Option<i64>) -> Result<(), SessionStoreError>;
    /// The lifecycle facts [`SessionRecords::end_session`] and
    /// [`SessionRecords::set_paused`] recorded, for a reader that derives
    /// display state from facts (the module docs): `Some((ended, paused_ms))`
    /// for a known session, `None` for an unknown one. `ended` is `None`
    /// before `end_session`. `paused_ms` is the last value `set_paused` was
    /// given (so `None` means either never paused or resumed; that
    /// distinction is display state, out of scope for this seam).
    fn lifecycle(&self, id: &SessionId) -> Result<Option<SessionLifecycle>, SessionStoreError>;
    fn session(&self, id: &SessionId) -> Result<Option<SessionFacts>, SessionStoreError>;
    fn sessions_of_run(&self, run: &crate::RunId) -> Result<Vec<SessionFacts>, SessionStoreError>;
    fn children(&self, id: &SessionId) -> Result<Vec<SessionFacts>, SessionStoreError>;
    fn push_inbox(&self, item: &InboxItem) -> Result<(), SessionStoreError>;
    /// Mark `item` delivered, exactly once: a second call on the same item is
    /// refused with [`SessionStoreError::Store`], not silently accepted, so
    /// "exactly once across a crash" (the design's steering section) is a
    /// property this seam itself holds, not just a caller's convention.
    fn mark_delivered(&self, item: &str, seq: u64, at_ms: i64) -> Result<(), SessionStoreError>;
    fn undelivered(&self, session: &SessionId) -> Result<Vec<InboxItem>, SessionStoreError>;
    /// One inbox item by id, whichever session it was sent to. `None` for an
    /// unknown id. A note that ends undelivered names
    /// only its id (the runtime's own `Injector::undelivered`); this is how
    /// the sender it must be reported to (`InboxItem::source`) is found.
    fn inbox_item(&self, id: &str) -> Result<Option<InboxItem>, SessionStoreError>;
    fn upsert_connection(&self, c: &ConnectionFacts) -> Result<(), SessionStoreError>;
    fn connections_of(
        &self,
        session: &SessionId,
    ) -> Result<Vec<ConnectionFacts>, SessionStoreError>;
}

/// Exercises every [`SessionRecords`] method against `r`, so an implementation
/// (`SharedSessions`) is checked against the contract.
#[cfg(any(test, feature = "test-support"))]
pub fn check_session_records(r: &dyn SessionRecords) {
    let run = crate::RunId::new("run-1");
    let parent = SessionFacts {
        id: SessionId::new("orchestrator:run-1"),
        run: run.clone(),
        kind: SessionKind::Orchestrator,
        parent: None,
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
    };
    r.upsert_session(&parent).expect("upsert parent");
    let child = SessionFacts {
        id: SessionId::new("run-contract-r0-a0"),
        parent: Some(parent.id.clone()),
        kind: SessionKind::Worker,
        depth: 1,
        ..parent.clone()
    };
    r.upsert_session(&child).expect("upsert child");

    assert_eq!(
        r.session(&parent.id).expect("read parent"),
        Some(parent.clone())
    );
    assert_eq!(
        r.session(&SessionId::new("no-such-session"))
            .expect("read absent"),
        None
    );
    assert!(parent.addressable, "the field's own default is addressable");
    let resolver = SessionFacts {
        id: SessionId::new("run-1-resolver"),
        kind: SessionKind::Worker,
        addressable: false,
        ..parent.clone()
    };
    r.upsert_session(&resolver)
        .expect("upsert a non-addressable session");
    assert!(
        !r.session(&resolver.id)
            .expect("read resolver")
            .expect("resolver row")
            .addressable,
        "addressable is a real fact, not always true"
    );
    assert_eq!(r.sessions_of_run(&run).expect("of run").len(), 3);
    assert_eq!(
        r.children(&parent.id).expect("children"),
        vec![child.clone()]
    );
    assert!(r.children(&child.id).expect("no grandchildren").is_empty());

    assert_eq!(
        r.lifecycle(&child.id).expect("lifecycle before end"),
        Some((None, None)),
        "a known session with no lifecycle facts yet reads as empty, not unknown"
    );
    assert_eq!(
        r.lifecycle(&SessionId::new("no-such-session"))
            .expect("lifecycle of an absent session"),
        None
    );

    r.end_session(&child.id, SessionOutcome::Completed, 2)
        .expect("end an existing session");
    assert!(matches!(
        r.end_session(
            &SessionId::new("no-such-session"),
            SessionOutcome::Completed,
            0
        ),
        Err(SessionStoreError::Unknown(_))
    ));
    r.set_paused(&parent.id, Some(3)).expect("pause");
    assert_eq!(
        r.lifecycle(&parent.id).expect("lifecycle while paused"),
        Some((None, Some(3))),
        "set_paused must be readable back, not only writable"
    );
    r.set_paused(&parent.id, None).expect("resume");
    assert!(matches!(
        r.set_paused(&SessionId::new("no-such-session"), Some(1)),
        Err(SessionStoreError::Unknown(_))
    ));
    assert_eq!(
        r.lifecycle(&child.id).expect("lifecycle after end"),
        Some((Some((SessionOutcome::Completed, 2)), None)),
        "end_session must be readable back, not only writable"
    );

    let item = |id: &str, received_ms: i64| InboxItem {
        id: id.to_owned(),
        session: child.id.clone(),
        source: InboxSource::User,
        kind: InboxKind::Note,
        mode: InjectMode::AtBoundary,
        text: format!("hello from {id}"),
        received_ms,
        not_before_ms: received_ms,
        delivered: None,
    };
    let first = item("msg-1", 4);
    let second = item("msg-2", 5);
    r.push_inbox(&first).expect("push first");
    r.push_inbox(&second).expect("push second");
    assert_eq!(
        r.undelivered(&child.id).expect("undelivered"),
        vec![first.clone(), second.clone()],
        "undelivered messages come back in the order they were pushed (FIFO)"
    );
    r.mark_delivered(&first.id, 7, 8).expect("mark delivered");
    assert_eq!(
        r.undelivered(&child.id).expect("one delivered, one left"),
        vec![second.clone()]
    );
    assert!(matches!(
        r.mark_delivered(&first.id, 9, 10),
        Err(SessionStoreError::Store(_))
    ));
    assert!(matches!(
        r.mark_delivered("no-such-message", 0, 0),
        Err(SessionStoreError::Unknown(_))
    ));
    assert_eq!(
        r.inbox_item(&second.id).expect("read"),
        Some(second.clone()),
        "an inbox item is readable by id whether or not it was delivered"
    );
    assert_eq!(r.inbox_item("no-such-message").expect("read absent"), None);

    let conn = ConnectionFacts {
        id: connection_id(&parent.id, &child.id),
        run: run.clone(),
        a: parent.id.clone(),
        b: child.id.clone(),
        basis: ConnectionBasis::ParentChild,
        state: ConnectionState::Open,
        decided_by: Some(Decider::Rule),
        decided_ms: Some(1),
        messages: 0,
        last_ms: None,
    };
    r.upsert_connection(&conn).expect("upsert connection");
    assert_eq!(
        r.connections_of(&parent.id).expect("connections of a"),
        vec![conn.clone()]
    );
    assert_eq!(
        r.connections_of(&child.id).expect("connections of b"),
        vec![conn]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_id_is_canonical_whichever_order_it_is_named_in() {
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        assert_eq!(connection_id(&a, &b), connection_id(&b, &a));
        assert_eq!(connection_id(&a, &b), "1:a--1:b");
    }

    #[test]
    fn connection_id_does_not_collide_when_an_id_contains_the_separator() {
        let a = connection_id(&SessionId::new("a"), &SessionId::new("b--c"));
        let b = connection_id(&SessionId::new("a--b"), &SessionId::new("c"));
        assert_ne!(
            a, b,
            "two different pairs must not join to the same string just \
             because one id contains the separator"
        );
        assert_eq!(a, "1:a--4:b--c");
        assert_eq!(b, "4:a--b--1:c");
    }
}
