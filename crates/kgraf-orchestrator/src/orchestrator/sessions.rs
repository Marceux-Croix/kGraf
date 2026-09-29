//! Every agent is a session (decision 0067): the session-control hooks
//! (`send_to_session`, `stop_session`, `resume_session`, `answer_question`),
//! the worker's `ask_user_question` path, and `record_session`,
//! `retire_session` and `open_by_rule` for the dispatch, finalize and subagent
//! sites.
//!
//! **Source of truth split.** `self.in_flight` and `self.record` (the loop's
//! own state) answer "is this session running, and what does it belong to";
//! the session row's ended fact answers "has it retired". Facts, inbox rows
//! and connections are persisted in [`kgraf_graph::sessions::SharedSessions`]
//! over `self.s.graph` for a reader outside the loop (the session view).
//!
//! **Liveness first.** Every hook that addresses a session checks it before it
//! persists anything: an unknown id is refused as unknown, a retired one as
//! retired with the offer to send the text to the orchestrator instead (owner
//! answer Q6). A session that is not running but can resume (stopped by the
//! user, or between a process stop and its resume) takes the note; it is
//! delivered when the session resumes (`SpawnOptions::injections`).
//!
//! **Paid calls run off the loop.** The orchestrator-first answer
//! to a question and the batched connection judgment (owner
//! answer Q2) are each reserved on the loop, issued in a spawned task, and
//! settled when the task's result comes back through the loop's own channel
//! ([`SessionWork`]). A stop, a steer or a worker's `Finished` is never held
//! behind a model call. Every question that does not get an orchestrator
//! answer attempt (no answerer, a failed call) is
//! recorded as a notice before it reaches the user.
//!
//! **Supervised, and bounded in time.** Each paid call runs in an inner
//! task under a supervisor ([`spawn_session_call`]): a panic, or a call
//! that does not finish within [`SESSION_CALL_TIMEOUT`], comes back as a
//! failed result, so the asking worker is never left blocked and a pending
//! judgment always clears.
//!
//! **Nothing waits forever.** A question the orchestrator could
//! not answer waits for the user until the run's attendance deadline
//! ([`crate::asks::QuestionAttendance`]); the contract shows "awaiting an
//! answer" meanwhile, and an expired question returns the recorded no-answer
//! text, which tells the worker to state its assumption.

use super::*;
use kgraf_core::session::{
    ConnectionBasis, ConnectionFacts, ConnectionState, Decider, InboxItem, SessionFacts, SessionId,
    SessionKind, connection_id,
};
use kgraf_graph::sessions::SharedSessions;
use kgraf_runtime::InjectError;

/// Tries to answer a worker's `ask_user_question` from the run record, and
/// judges connection requests no rule covers. Each method is one model call
/// the orchestrator reserves before it is made and settles at its ledger cost
/// after; the implementation must issue that call through the ledger
/// and name its row in the output. [`crate::ModelAnswerer`] is the product's.
#[async_trait::async_trait]
pub trait QuestionAnswerer: Send + Sync {
    /// `answer: None`: the record does not cover the question, so the user is
    /// asked. Never guesses: a wrong answer standing in for "I don't
    /// know" is worse than the extra round trip to the user.
    async fn try_answer(&self, request: &AnswerRequest) -> Result<AnswerOutput, PlanError>;
    /// What one call costs at most, so the orchestrator can reserve it first
    /// (mirrors `Grounder::cost_basis`, `Decomposer::cost_basis`).
    fn cost_basis(&self) -> CostBasis;
    /// Judge every pair in `request` in one call (decision 0067, point 6). A pair with no verdict in the output is refused.
    async fn judge_connections(&self, request: &JudgeRequest) -> Result<JudgeOutput, PlanError>;
}

/// One batched connection judgment: every queued pair, with what the judge
/// needs to decide them.
#[derive(Clone, Debug)]
pub struct JudgeRequest {
    pub original_instruction: String,
    pub requests: Vec<ConnectionRequest>,
    /// Each session a request names, with the start of its contract's
    /// instruction (orchestrator-written; empty when unknown).
    pub agents: Vec<(SessionId, String)>,
}

/// What a [`QuestionAnswerer`] is given to answer from. The question is the
/// worker's own words: untrusted, quoted as data.
pub struct AnswerRequest {
    /// The asking session.
    pub session: SessionId,
    pub original_instruction: String,
    pub question: UntrustedText,
    pub verified: Vec<String>,
    /// The run's notes, each already quoted as a data block.
    pub notes: Vec<String>,
}

/// One pair a connection judgment covers. The reason is an agent's own words:
/// untrusted, quoted as data.
#[derive(Clone, Debug)]
pub struct ConnectionRequest {
    pub pair: (SessionId, SessionId),
    pub reason: UntrustedText,
}

/// A verdict for one pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionVerdict {
    pub pair: (SessionId, SessionId),
    pub open: bool,
    pub reason: String,
}

/// One judgment call's verdicts, and the ledger row of the call.
pub struct JudgeOutput {
    pub verdicts: Vec<ConnectionVerdict>,
    pub call: Option<CallId>,
}

/// A [`QuestionAnswerer`]'s answer.
pub struct AnswerOutput {
    /// `None`: the record does not cover the question.
    pub answer: Option<String>,
    pub call: Option<CallId>,
}

/// A paid session call's result, back on the loop.
pub(super) enum SessionWork {
    Answered {
        id: u64,
        result: Result<AnswerOutput, PlanError>,
    },
    Judged(Result<JudgeOutput, PlanError>),
}

/// An orchestrator-first answer call in flight.
struct Asking {
    reservation: Reservation,
    session: AgentId,
    question: UntrustedText,
    reply: oneshot::Sender<Result<String, SessionError>>,
    task: tokio::task::JoinHandle<()>,
}

/// Loop-only session state: nothing here needs to survive a restart (the
/// inbox rows and connection facts that do are in the graph store).
pub(super) struct SessionRuntime {
    /// How long a question waits for the user.
    pub(super) question_timeout: Duration,
    /// How long one paid session call may run ([`SESSION_CALL_TIMEOUT`]).
    pub(super) call_timeout: Duration,
    /// Where session notices go besides the trace and the report: the box's
    /// `RunEvent::SessionNotice`.
    pub(super) notice_sink: Option<SessionNoticeSink>,
    asking: BTreeMap<u64, Asking>,
    next_id: u64,
    pub(super) judge: connections::JudgeQueue,
    pub(super) pacer: connections::Pacer,
    pub(super) recheck: connections::Recheck,
    /// Pairs whose refusal or close a rule collided with, already noticed.
    rule_collisions: BTreeSet<String>,
}

/// A callback that shows a session notice to the user.
pub type SessionNoticeSink = Arc<dyn Fn(String) + Send + Sync>;

/// How long one paid session call (an orchestrator-first answer, a
/// connection judgment) may run before it counts as failed. Generous: one
/// model call of a few hundred output tokens; a call that has not answered in
/// this time is stuck, and the worker asking waits on it.
pub const SESSION_CALL_TIMEOUT: Duration = Duration::from_secs(300);

/// Aborts the call task when its supervisor is dropped: a run shutdown
/// aborts the supervisor, and the call must not outlive it.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Run one paid session call off the loop, supervised: `call` runs in its own
/// task under `limit`; a panic or the limit becomes a failed result, and
/// `wrap` of the result always reaches the loop. The error is a transport
/// failure, since the request may have reached the provider: its hold is
/// settled whole.
pub(super) fn spawn_session_call<T, F>(
    tx: mpsc::Sender<LoopMsg>,
    limit: Duration,
    what: &'static str,
    call: F,
    wrap: impl FnOnce(Result<T, PlanError>) -> SessionWork + Send + 'static,
) -> tokio::task::JoinHandle<()>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, PlanError>> + Send + 'static,
{
    let inner = tokio::spawn(tokio::time::timeout(limit, call));
    tokio::spawn(async move {
        let _abort = AbortOnDrop(inner.abort_handle());
        let failed = |reason: String| {
            PlanError::Provider(kgraf_provider::ProviderError::Transport {
                adapter: "kgraf-orchestrator",
                reason,
            })
        };
        let result = match inner.await {
            Ok(Ok(result)) => result,
            Ok(Err(_elapsed)) => Err(failed(format!(
                "the {what} call did not finish within {limit:?}"
            ))),
            Err(e) if e.is_panic() => Err(failed(format!("the {what} call panicked: {e}"))),
            Err(e) => Err(failed(format!("the {what} call ended abnormally: {e}"))),
        };
        let _ = tx.send(LoopMsg::Session(Box::new(wrap(result)))).await;
    })
}

impl SessionRuntime {
    pub(super) fn new(
        question_timeout: Duration,
        notice_sink: Option<SessionNoticeSink>,
        call_timeout: Duration,
    ) -> Self {
        Self {
            question_timeout,
            call_timeout,
            notice_sink,
            asking: BTreeMap::new(),
            next_id: 0,
            judge: connections::JudgeQueue::default(),
            pacer: connections::Pacer::default(),
            recheck: connections::Recheck::default(),
            rule_collisions: BTreeSet::new(),
        }
    }

    pub(super) fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

/// Inbox kinds a running session is given through its injector. An `Answer`
/// row is not: it already reached the worker as its `ask_user_question` tool
/// result, and the row is the visible record of it. Injecting it
/// again would put the same answer in the transcript twice.
pub(super) fn injectable(kind: InboxKind) -> bool {
    !matches!(kind, InboxKind::Answer)
}

/// The recorded result of a question nobody answered in time (decision 0067,
/// point 4): never silent, and it tells the worker what to do instead.
pub(super) fn no_answer_text(after: Duration) -> String {
    format!(
        "No answer came within {} s: the orchestrator could not answer this from the run record, \
         and nobody answered it. State the assumption you make instead, in your answer, and \
         continue.",
        after.as_secs()
    )
}

/// `SessionRecords` over this run's own graph store.
pub(super) fn sessions_of(s: &Services) -> SharedSessions {
    SharedSessions(s.graph.clone())
}

fn store_error(e: impl std::fmt::Display) -> OrchestratorError {
    OrchestratorError::Store(StoreError::Decision(format!("session store: {e}")))
}

/// Whether a session can take a message, and why not.
pub(super) enum Liveness {
    /// Its worker runs in this process now.
    Running,
    /// Not running, but it resumes: stopped by the user, or dispatched by a
    /// process that stopped. A note waits in its inbox until then.
    Resumable,
    /// It ended; nothing reaches it again.
    Retired { outcome: String },
    /// This run never dispatched it.
    Unknown,
}

impl RunLoop {
    pub(super) fn liveness_of(&self, id: &AgentId) -> Liveness {
        if self.in_flight.values().any(|f| f.cancels.contains_key(id)) {
            return Liveness::Running;
        }
        // A subagent (`subagents.rs`): running, or live and not running
        // (paused by the user, adopted and waiting for a slot).
        if let Some(running) = self.child_running(id) {
            return if running {
                Liveness::Running
            } else {
                Liveness::Resumable
            };
        }
        if let Ok(Some((Some((outcome, _)), _))) = sessions_of(&self.s).lifecycle(id) {
            return Liveness::Retired {
                outcome: format!("{outcome:?}").to_lowercase(),
            };
        }
        if self
            .record
            .coordination
            .closed
            .iter()
            .any(|c| c.workers.iter().any(|w| w == id.as_str()))
        {
            return Liveness::Retired {
                outcome: "its contract closed".into(),
            };
        }
        if self
            .record
            .open_contracts
            .iter()
            .any(|c| c.workers.contains(id))
        {
            return Liveness::Resumable;
        }
        // A subagent a stop or a kill left open: adopted again when its
        // parent is dispatched.
        if let Ok(Some(f)) = sessions_of(&self.s).session(id)
            && f.kind == SessionKind::Subagent
        {
            return Liveness::Resumable;
        }
        Liveness::Unknown
    }

    /// Whether `id`'s worker runs in this process (a worker or a subagent).
    pub(super) fn is_running(&self, id: &AgentId) -> bool {
        self.in_flight.values().any(|f| f.cancels.contains_key(id))
            || self.child_running(id) == Some(true)
    }

    /// The refusal for a session that cannot take a message, or `None`.
    pub(super) fn refusal_of(&self, id: &AgentId) -> Option<SessionError> {
        match self.liveness_of(id) {
            Liveness::Running | Liveness::Resumable => None,
            Liveness::Unknown => Some(SessionError::Unknown(id.to_string())),
            Liveness::Retired { outcome } => Some(SessionError::Retired {
                session: id.to_string(),
                outcome,
            }),
        }
    }

    /// The session's `injector`, if its worker is spawned (an `OnceLock`
    /// filled once `run_attempt` has the real `WorkerHandle`).
    pub(super) fn injector_of(&self, id: &AgentId) -> Option<Injector> {
        self.in_flight
            .values()
            .find_map(|f| f.injectors.get(id).and_then(|slot| slot.get().cloned()))
            .or_else(|| self.child_injector(id))
    }

    fn cancel_of(&self, id: &AgentId) -> Option<Arc<Notify>> {
        self.in_flight
            .values()
            .find_map(|f| f.cancels.get(id).cloned())
    }

    /// The contract `id` works under: its own for a worker, its root
    /// worker's for a subagent.
    pub(super) fn contract_of(&self, id: &AgentId) -> Option<String> {
        self.record
            .open_contracts
            .iter()
            .find(|c| c.workers.contains(id))
            .map(|c| c.contract.clone())
            .or_else(|| self.child_contract(id))
    }

    /// Whether `id` takes direct notes (decision 0073):
    /// a session row with `addressable: false` (a merge-conflict resolver)
    /// does not. An absent row reads as addressable, the field's default.
    fn is_addressable(&self, id: &AgentId) -> bool {
        sessions_of(&self.s)
            .session(id)
            .ok()
            .flatten()
            .map(|f| f.addressable)
            .unwrap_or(true)
    }

    /// Record one session's facts (dispatch, handoff, subagent spawn).
    /// `addressable` is `true` for every session but a resolver.
    pub(crate) fn record_session(&mut self, facts: SessionFacts) -> Result<(), OrchestratorError> {
        sessions_of(&self.s)
            .upsert_session(&facts)
            .map_err(store_error)
    }

    /// End one session with its outcome (finalize, handoff, subagent end).
    /// Every note, redirect or message still undelivered in its inbox can no
    /// longer reach it: each is reported to its sender here. A
    /// session that can resume is never retired, so nothing is reported for
    /// it: its inbox is delivered when it resumes.
    pub(crate) fn retire_session(
        &mut self,
        id: &SessionId,
        outcome: SessionOutcome,
    ) -> Result<(), OrchestratorError> {
        let now = self.now();
        let store = sessions_of(&self.s);
        store.end_session(id, outcome, now).map_err(store_error)?;
        let undelivered = store.undelivered(id).map_err(store_error)?;
        let contract = self.contract_of(id).unwrap_or_default();
        for item in undelivered {
            if !matches!(
                item.kind,
                InboxKind::Note | InboxKind::Redirect | InboxKind::Message
            ) {
                continue;
            }
            let what = match item.kind {
                InboxKind::Redirect => "redirect",
                InboxKind::Message => "message",
                _ => "note",
            };
            let text = format!(
                "your {what} to {id} was not delivered: the session ended ({}) before it \
                 reached a round boundary",
                format!("{outcome:?}").to_lowercase()
            );
            match &item.source {
                InboxSource::Agent { id: sender }
                    if matches!(
                        self.liveness_of(sender),
                        Liveness::Retired { .. } | Liveness::Unknown
                    ) =>
                {
                    // The sender ended too: nothing can reach it (a
                    // subagent's report to a parent that already answered,
                    // say). The row stays in the inbox, visible.
                }
                InboxSource::Agent { id: sender } => {
                    let report = InboxItem {
                        id: format!("undelivered-{}", item.id),
                        session: sender.clone(),
                        source: InboxSource::Orchestrator,
                        kind: InboxKind::Notice,
                        mode: InjectMode::AtBoundary,
                        text,
                        received_ms: now,
                        not_before_ms: now,
                        delivered: None,
                    };
                    match store.push_inbox(&report) {
                        Ok(()) => self.inject_now_or_later(sender, &report),
                        Err(e) => eprintln!(
                            "kgraf-orchestrator: an undelivered report could not be recorded: \
                             id {}: {e}",
                            item.id
                        ),
                    }
                }
                InboxSource::User | InboxSource::Orchestrator => {
                    self.session_notice(&contract, id.as_str(), text);
                }
            }
        }
        Ok(())
    }

    /// A row for a session this run knows (running or resumable) that has
    /// none: one dispatched by a process that predates session rows. Called
    /// only after a liveness check, never for an unknown id.
    pub(super) fn ensure_session_row(&mut self, id: &AgentId) {
        if sessions_of(&self.s).session(id).ok().flatten().is_some() {
            return;
        }
        self.ensure_orch_session_row();
        let parent = self.orchestrator_session();
        let now = self.now();
        let facts = SessionFacts {
            id: id.clone(),
            run: kgraf_core::RunId::new(self.record.run.to_string()),
            kind: SessionKind::Worker,
            parent: Some(parent),
            contract: self.contract_of(id),
            route: None,
            model: None,
            depth: 1,
            transcript: None,
            worktree: None,
            write_scope: Vec::new(),
            reasoning: kgraf_core::session::ReasoningVisibility::Unknown,
            started_ms: now,
            addressable: true,
        };
        if let Err(e) = self.record_session(facts) {
            eprintln!("kgraf-orchestrator: a session row for {id}: {e}");
        }
    }

    /// Open a connection a rule covers (decision 0067, point 6): no model
    /// call. Idempotent: an already open connection keeps its counts.
    pub(crate) fn open_by_rule(
        &mut self,
        a: &SessionId,
        b: &SessionId,
        basis: ConnectionBasis,
    ) -> Result<(), OrchestratorError> {
        if a == b {
            return Ok(());
        }
        let store = sessions_of(&self.s);
        let id = connection_id(a, b);
        let existing = store
            .connections_of(a)
            .map_err(store_error)?
            .into_iter()
            .find(|c| c.id == id);
        match existing.map(|c| c.state) {
            Some(ConnectionState::Open) => return Ok(()),
            // A decision already made (a paid orchestrator refusal, or a
            // close) is never reversed by a rule that comes to cover the
            // pair later: the connection keeps its state,
            // reason and counts, and the collision is said once.
            Some(ConnectionState::Refused { reason }) => {
                self.rule_collision(&id, basis, &format!("refused ({reason})"));
                return Ok(());
            }
            Some(ConnectionState::Closed) => {
                self.rule_collision(&id, basis, "closed");
                return Ok(());
            }
            Some(ConnectionState::Pending) | None => {}
        }
        let now = self.now();
        let facts = ConnectionFacts {
            id: connection_id(a, b),
            run: kgraf_core::RunId::new(self.record.run.to_string()),
            a: a.clone(),
            b: b.clone(),
            basis,
            state: ConnectionState::Open,
            decided_by: Some(Decider::Rule),
            decided_ms: Some(now),
            messages: 0,
            last_ms: None,
        };
        store.upsert_connection(&facts).map_err(store_error)
    }

    /// A rule covers a pair the orchestrator already refused or closed: the
    /// decision stands, and the collision is a notice, once per pair.
    fn rule_collision(&mut self, id: &str, basis: ConnectionBasis, state: &str) {
        if !self.sessions_rt.rule_collisions.insert(id.to_owned()) {
            return;
        }
        self.session_notice(
            "",
            "",
            format!(
                "connection {id} is {state}; a rule now covers it ({basis:?}), and the earlier \
                 decision stands"
            ),
        );
    }

    /// A notice about a session the user must be able to see: in the
    /// trace, in the run report's notices, and in the box when a sink is set.
    pub(super) fn session_notice(&mut self, contract: &str, worker: &str, detail: String) {
        // Shown and stored: persist-scrubbed (decision 0071); a detail can
        // quote a provider's error.
        let detail = kgraf_secrets::scrub_persist_owned(detail);
        let at = self.now();
        self.queue(TraceEvent::Notice {
            at_ms: at,
            contract: contract.to_owned(),
            worker: worker.to_owned(),
            detail: detail.clone(),
        });
        if let Some(sink) = &self.sessions_rt.notice_sink {
            sink(detail.clone());
        }
        self.notices.insert(detail);
    }

    // ------------------------------------------------------------- questions

    /// A worker's `ask_user_question` (decision 0067, point 4): tried against the run record first, with one reserved, ledgered
    /// call issued off the loop; the user is asked only when that cannot
    /// answer it. The reply is sent later, from [`Self::on_session_work`] or
    /// the user's answer, never by blocking this loop.
    pub(super) async fn hook_ask_question(
        &mut self,
        session: AgentId,
        question: String,
        reply: oneshot::Sender<Result<String, SessionError>>,
    ) -> Result<(), OrchestratorError> {
        if question.trim().is_empty() {
            let _ = reply.send(Err(SessionError::Empty));
            return Ok(());
        }
        if let Some(refusal) = self.refusal_of(&session) {
            let _ = reply.send(Err(refusal));
            return Ok(());
        }
        self.ensure_session_row(&session);
        let contract = self.contract_of(&session).unwrap_or_default();
        // Taken in (decision 0071): the stored and shown copy is scrubbed; a
        // model reads `for_model` of it.
        let question = UntrustedText::new(question).for_storage();
        let Some(answerer) = self.question_answerer.clone() else {
            self.session_notice(
                &contract,
                session.as_str(),
                format!(
                    "{session}'s question skipped the orchestrator: no answerer is configured \
                     for this run, so it went to the user"
                ),
            );
            self.ask_the_user(session, question, reply);
            return self.save();
        };
        let request = AnswerRequest {
            session: session.clone(),
            original_instruction: kgraf_secrets::model_copy(&self.record.original_instruction)
                .into_owned(),
            question: question.for_model(),
            verified: self.record.verified_contracts.clone(),
            // Untrusted (the user's or another agent's words): quoted as
            // data, never spliced in as if the orchestrator wrote it.
            notes: self
                .record
                .notes
                .iter()
                .map(|n| {
                    n.text
                        .for_model()
                        .quote_as_data("note", &[("channel", &n.channel)])
                })
                .collect(),
        };
        let input = estimate_tokens(&request.original_instruction)
            + (question.len() as u64).div_ceil(4)
            + request
                .notes
                .iter()
                .map(|n| estimate_tokens(n))
                .sum::<u64>()
            + 200;
        let reservation = self.reserve_call(&answerer.cost_basis(), input);
        // The reservation's counters reach the record now, not at the next
        // unrelated save.
        self.save()?;
        let id = self.sessions_rt.next_id();
        let task = spawn_session_call(
            self.tx.clone(),
            self.sessions_rt.call_timeout,
            "answer",
            async move { answerer.try_answer(&request).await },
            move |result| SessionWork::Answered { id, result },
        );
        self.sessions_rt.asking.insert(
            id,
            Asking {
                reservation,
                session,
                question,
                reply,
                task,
            },
        );
        Ok(())
    }

    /// A paid session call's result.
    pub(super) async fn on_session_work(
        &mut self,
        w: SessionWork,
    ) -> Result<(), OrchestratorError> {
        match w {
            SessionWork::Answered { id, result } => self.on_answered(id, result),
            SessionWork::Judged(result) => self.on_judged(result),
        }
    }

    fn on_answered(
        &mut self,
        id: u64,
        result: Result<AnswerOutput, PlanError>,
    ) -> Result<(), OrchestratorError> {
        let Some(a) = self.sessions_rt.asking.remove(&id) else {
            return Ok(());
        };
        let contract = self.contract_of(&a.session).unwrap_or_default();
        match result {
            Ok(out) => {
                let calls: Vec<CallId> = out.call.into_iter().collect();
                self.settle_call(a.reservation, Some(&calls))?;
                match out.answer.filter(|t| !t.trim().is_empty()) {
                    Some(answer) => {
                        // A worker that stopped waiting (cancelled) never saw
                        // it: not recorded as an answer it was given.
                        if !a.reply.is_closed() {
                            self.record_answer(&a.session, InboxSource::Orchestrator, &answer);
                            let _ = a
                                .reply
                                .send(Ok(kgraf_secrets::scrub_model_facing(&answer).into_owned()));
                        }
                    }
                    None => self.ask_the_user(a.session, a.question, a.reply),
                }
            }
            Err(e) => {
                self.settle_call(a.reservation, e.calls())?;
                self.session_notice(
                    &contract,
                    a.session.as_str(),
                    format!(
                        "{}'s question skipped the orchestrator: its answer call failed ({e}), \
                         so it went to the user",
                        a.session
                    ),
                );
                self.ask_the_user(a.session, a.question, a.reply);
            }
        }
        self.save()
    }

    /// Queue a question for the user until the run's attendance deadline, and
    /// show its contract as awaiting an answer.
    fn ask_the_user(
        &mut self,
        session: AgentId,
        question: UntrustedText,
        reply: oneshot::Sender<Result<String, SessionError>>,
    ) {
        if reply.is_closed() {
            return;
        }
        let deadline = std::time::Instant::now() + self.sessions_rt.question_timeout;
        self.question_desk.wait_for(
            session.clone(),
            question.to_description(4_000),
            deadline,
            reply,
        );
        if let Some(c) = self.contract_of(&session) {
            self.note_wait(&c, crate::waits::WaitState::AwaitingAnswer);
        }
    }

    /// The contract goes back to running once none of its sessions has a
    /// question waiting.
    fn question_settled(&mut self, session: &AgentId) {
        let Some(c) = self.contract_of(session) else {
            return;
        };
        let workers = self
            .record
            .contract(&c)
            .map(|oc| oc.workers.clone())
            .unwrap_or_default();
        if workers.iter().any(|w| self.question_desk.is_waiting(w)) {
            return;
        }
        if self.in_flight.contains_key(&c) {
            self.note_wait(&c, crate::waits::WaitState::Running);
        }
    }

    /// Answer every question whose deadline passed with the recorded
    /// no-answer text, and drop those whose worker stopped waiting.
    fn expire_questions(&mut self) -> Result<(), OrchestratorError> {
        let ended = self.question_desk.take_ended(std::time::Instant::now());
        if ended.is_empty() {
            return Ok(());
        }
        for q in ended {
            if q.expired {
                let after = self.sessions_rt.question_timeout;
                let text = no_answer_text(after);
                self.record_answer(&q.session, InboxSource::Orchestrator, &text);
                let contract = self.contract_of(&q.session).unwrap_or_default();
                self.session_notice(
                    &contract,
                    q.session.as_str(),
                    format!(
                        "{}'s question had no answer within {} s; the worker was told to state \
                         its assumption",
                        q.session,
                        after.as_secs()
                    ),
                );
                let _ = q.reply.send(Ok(text));
            }
            self.question_settled(&q.session);
        }
        self.save()
    }

    /// An answer's visible record in the asking session's inbox. It already
    /// reached the worker as its tool result, so it is never injected
    /// ([`injectable`]).
    /// The row takes the raw text: the inbox store is where it is taken in
    /// (persist-scrubbed, decision 0071). The worker is given the
    /// model-facing scrub of the same raw text as its tool result.
    fn record_answer(&mut self, session: &AgentId, source: InboxSource, answer: &str) {
        let now = self.now();
        let n = self.sessions_rt.next_id();
        let item = InboxItem {
            id: format!("answer-{session}-{now}-{n}"),
            session: session.clone(),
            source,
            kind: InboxKind::Answer,
            mode: InjectMode::AtBoundary,
            text: answer.to_owned(),
            received_ms: now,
            not_before_ms: now,
            delivered: None,
        };
        if let Err(e) = sessions_of(&self.s).push_inbox(&item) {
            eprintln!(
                "kgraf-orchestrator: an answer could not be recorded to the session's inbox: \
                 session {session}: {e}"
            );
        }
    }

    /// The user's answer to a question the orchestrator could not answer.
    pub(super) async fn hook_answer_question(
        &mut self,
        question: u64,
        answer: String,
        reply: oneshot::Sender<Result<(), SessionError>>,
    ) -> Result<(), OrchestratorError> {
        if answer.trim().is_empty() {
            let _ = reply.send(Err(SessionError::Empty));
            return Ok(());
        }
        // Decision 0071: the worker reads the model-facing copy as its tool
        // result; the inbox store takes the raw text in, scrubbed.
        let for_model = kgraf_secrets::scrub_model_facing(&answer).into_owned();
        match self.question_desk.answer(question, for_model) {
            Some(session) => {
                self.record_answer(&session, InboxSource::User, &answer);
                self.question_settled(&session);
                let _ = reply.send(Ok(()));
            }
            None => {
                let _ = reply.send(Err(SessionError::Unknown(question.to_string())));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------- direct notes

    /// Send a note, redirect, answer or message to one session directly
    /// (decision 0067, point 2). The session's liveness is checked first;
    /// then the text is persisted to its inbox and to the run record's
    /// `direct:<session>` channel (the next orchestrator call reads it as
    /// data) before this replies; then it is injected, or queued for the
    /// re-check if the worker's injector is not there yet, or left for the
    /// resume of a resumable session.
    pub(super) async fn hook_send_to_session(
        &mut self,
        session: AgentId,
        kind: InboxKind,
        mode: InjectMode,
        text: String,
        reply: oneshot::Sender<Result<String, SessionError>>,
    ) -> Result<(), OrchestratorError> {
        if text.trim().is_empty() {
            let _ = reply.send(Err(SessionError::Empty));
            return Ok(());
        }
        if !self.is_addressable(&session) {
            let _ = reply.send(Err(SessionError::Refused(format!(
                "session `{session}` is not addressable: it never takes a direct note"
            ))));
            return Ok(());
        }
        if let Some(refusal) = self.refusal_of(&session) {
            let _ = reply.send(Err(refusal));
            return Ok(());
        }
        self.ensure_session_row(&session);
        let now = self.now();
        let n = self.sessions_rt.next_id();
        let id = format!("direct-{session}-{now}-{n}");
        let item = InboxItem {
            id: id.clone(),
            session: session.clone(),
            source: InboxSource::User,
            kind,
            mode,
            // Raw: the inbox store takes it in (decision 0071), and the
            // injection reads its model-facing copy.
            text: text.clone(),
            received_ms: now,
            not_before_ms: now,
            delivered: None,
        };
        if let Err(e) = sessions_of(&self.s).push_inbox(&item) {
            let _ = reply.send(Err(SessionError::NotPersisted(e.to_string())));
            return Ok(());
        }
        if let Err(e) = self.persist_note(
            &format!("direct:{session}"),
            InboundKind::Note,
            &text,
            Some(&id),
        ) {
            let _ = reply.send(Err(e_to_session(e)));
            return Ok(());
        }
        self.inject_now_or_later(&session, &item);
        let _ = reply.send(Ok(id));
        Ok(())
    }

    /// Inject a persisted inbox row into its session now if the session's
    /// injector exists, and otherwise mark the session for the loop's
    /// re-check, which delivers it once the injector exists. A
    /// session that is not running gets it at its resume
    /// (`SpawnOptions::injections`).
    pub(super) fn inject_now_or_later(&mut self, session: &AgentId, item: &InboxItem) {
        if !injectable(item.kind) {
            return;
        }
        let Some(injector) = self.injector_of(session) else {
            self.sessions_rt.recheck.mark(session);
            return;
        };
        match injector.inject(Injection {
            id: item.id.clone(),
            source: item.source.clone(),
            kind: item.kind,
            mode: item.mode,
            // `item` holds the raw text (the store scrubs its own copy).
            text: kgraf_secrets::scrub_model_facing(&item.text).into_owned(),
        }) {
            Ok(()) | Err(InjectError::Duplicate { .. }) => {}
            // The worker ended between the liveness check and here: the row
            // stays in the inbox, delivered at its resume or reported to the
            // sender when the session retires. Never lost.
            Err(InjectError::Finished { .. }) | Err(InjectError::NotBuilt) => {}
        }
    }

    // ------------------------------------------------------------- stop, resume

    /// Stop one session's attempt (decision 0067, point 2). Cancels it (its
    /// own `Notify`; a sibling attempt is untouched), marks it paused, and
    /// holds its contract for the user with the typed `stopped_by_user`
    /// fact, which `finalize`, a process resume and the run's end all
    /// respect; only `resume_session` releases it.
    pub(super) async fn hook_stop_session(
        &mut self,
        session: AgentId,
        reply: oneshot::Sender<Result<(), SessionError>>,
    ) -> Result<(), OrchestratorError> {
        if !self.is_addressable(&session) {
            let _ = reply.send(Err(SessionError::Refused(format!(
                "session `{session}` is not addressable: it cannot be stopped directly"
            ))));
            return Ok(());
        }
        // A subagent pauses on its own: its parent's contract is not held.
        if let Some(stopped) = self.stop_child(&session) {
            if stopped.is_ok() {
                let now = self.now();
                if let Err(e) = sessions_of(&self.s).set_paused(&session, Some(now)) {
                    eprintln!(
                        "kgraf-orchestrator: a stop could not be recorded to the session's \
                         paused fact: session {session}: {e}"
                    );
                }
            }
            let _ = reply.send(stopped);
            return Ok(());
        }
        let Some(notify) = self.cancel_of(&session) else {
            let refusal = self
                .refusal_of(&session)
                .unwrap_or_else(|| SessionError::Refused(format!("`{session}` is not running")));
            let _ = reply.send(Err(refusal));
            return Ok(());
        };
        self.ensure_session_row(&session);
        if let Some(id) = self.contract_of(&session) {
            self.hold(&id, format!("stopped by the user ({session})"));
            if let Some(c) = self.record.contract_mut(&id)
                && !c.stopped_by_user.contains(&session)
            {
                c.stopped_by_user.push(session.clone());
            }
            self.save()?;
        }
        notify.notify_one();
        let now = self.now();
        if let Err(e) = sessions_of(&self.s).set_paused(&session, Some(now)) {
            // The cancel already went out: reported, not treated as "the
            // stop did not happen".
            eprintln!(
                "kgraf-orchestrator: a stop could not be recorded to the session's paused \
                 fact: session {session}: {e}"
            );
        }
        let _ = reply.send(Ok(()));
        Ok(())
    }

    /// Resume a stopped session (decision 0067, point 2): removes it from its
    /// contract's `stopped_by_user`, and returns the contract to `Planned`
    /// once none is left, so normal admission dispatches it and it continues
    /// from its transcript.
    pub(super) async fn hook_resume_session(
        &mut self,
        session: AgentId,
        reply: oneshot::Sender<Result<(), SessionError>>,
    ) -> Result<(), OrchestratorError> {
        if let Some(refusal) = self.refusal_of(&session) {
            let _ = reply.send(Err(refusal));
            return Ok(());
        }
        if self.resume_child(&session).is_none()
            && let Some(id) = self.contract_of(&session)
            && let Some(c) = self.record.contract_mut(&id)
            && c.stopped_by_user.contains(&session)
        {
            c.stopped_by_user.retain(|s| s != &session);
            let release =
                c.stopped_by_user.is_empty() && matches!(c.status, ContractStatus::Held { .. });
            if release {
                c.status = ContractStatus::Planned;
                self.note_wait(&id, crate::waits::WaitState::Unchecked);
            }
        }
        self.save()?;
        if let Err(e) = sessions_of(&self.s).set_paused(&session, None) {
            eprintln!(
                "kgraf-orchestrator: a resume could not clear the session's paused fact: \
                 session {session}: {e}"
            );
        }
        let _ = reply.send(Ok(()));
        Ok(())
    }

    // ------------------------------------------------------------- the loop

    /// Once per loop pass: expire questions, start the next connection
    /// judgment batch, and deliver inbox rows that came due (no
    /// store query unless a session was marked or a paced row came due).
    pub(super) fn pump_sessions(&mut self) -> Result<(), OrchestratorError> {
        self.expire_questions()?;
        self.start_judging();
        self.recheck_inbox()
    }

    /// The run is ending: every paid session call still in flight is aborted
    /// and settled at its whole hold (the worst case, as a failed call that
    /// named no ledger row is), and every waiter's call ends as stopped.
    pub(super) fn shutdown_sessions(&mut self) {
        for (_, a) in std::mem::take(&mut self.sessions_rt.asking) {
            a.task.abort();
            if let Err(e) = self.settle_call(a.reservation, None) {
                eprintln!("kgraf-orchestrator: an answer call's hold could not be settled: {e}");
            }
        }
        if let Some((reservation, task)) = self.sessions_rt.judge.abort() {
            task.abort();
            if let Some(r) = reservation
                && let Err(e) = self.settle_call(r, None)
            {
                eprintln!("kgraf-orchestrator: a judgment call's hold could not be settled: {e}");
            }
        }
        self.question_desk.clear();
    }
}

/// [`SteerError`]-shaped store failures, translated to [`SessionError`] for
/// `hook_send_to_session`'s one `persist_note` call.
fn e_to_session(e: SteerError) -> SessionError {
    match e {
        SteerError::Empty => SessionError::Empty,
        SteerError::Stopped => SessionError::Stopped,
        SteerError::NotPersisted(s) => SessionError::NotPersisted(s),
        SteerError::RuntimeCannotInject(w) => {
            SessionError::Refused(format!("runtime cannot inject for {w}"))
        }
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn connection_id_is_stable_regardless_of_argument_order() {
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        assert_eq!(connection_id(&a, &b), connection_id(&b, &a));
    }

    #[test]
    fn an_answer_row_is_never_injected_again() {
        assert!(!injectable(InboxKind::Answer));
        for k in [
            InboxKind::Note,
            InboxKind::Redirect,
            InboxKind::Message,
            InboxKind::Notice,
        ] {
            assert!(injectable(k), "{k:?}");
        }
    }

    #[test]
    fn the_no_answer_text_tells_the_worker_to_state_its_assumption() {
        let t = no_answer_text(Duration::from_secs(60));
        assert!(
            t.contains("60 s") && t.contains("State the assumption"),
            "{t}"
        );
    }
}
