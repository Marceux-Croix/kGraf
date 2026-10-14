//! The orchestrator approves connections, not messages (decision 0067, point
//! 6).
//!
//! - **Rule-covered pairs** ([`RunLoop::refresh_rule_connections`]): sibling
//!   (two attempts of one contract), dependency (`OpenContract::depends_on`)
//!   and shared read (`coordination.subscriptions` overlap), with no model
//!   call. Parent and child is opened by the subagent spawn.
//! - **Judged pairs**: `request_connection`, and a
//!   `send_message` to a pair with no connection, queue a request. Once per
//!   loop tick, every queued request is judged in one reserved, ledgered call
//!   issued off the loop ([`RunLoop::start_judging`]); its verdicts are matched
//!   to the requests by pair, and a pair with no verdict is refused with a
//!   reason. A refusal is recorded on the pair, so the run never
//!   pays to judge it again.
//! - **Delivery**: a message over an open connection is persisted to the
//!   recipient's inbox (raw), then injected quoted as data with its sender
//!   named (`sessions::model_text`): another model's words never reach the
//!   recipient as instructions. The sender is told it is queued, never
//!   delivered. It is never refused for volume, only
//!   paced: past the soft rate of 12 per minute per sender
//!   per connection, measured over a true sliding 60 s window, delivery waits
//!   `5 s * excess` (at most 5 minutes) through `not_before_ms`, and never
//!   before a message the same sender queued earlier on that connection, so
//!   order holds. The recipient's contract shows "delivery
//!   paced" until the paced rows are delivered.
//! - **The re-check** ([`RunLoop::recheck_inbox`]) delivers what could not be
//!   injected when it was persisted: a row that came due after pacing, and a
//!   row for a session whose injector did not exist yet (the gap between
//!   dispatch and `spawn_worker`). It touches the store only for
//!   sessions marked since the last pass and for paced rows that came due,
//!   so an idle loop pass costs no query.

use super::sessions::{
    ConnectionRequest, ConnectionVerdict, JudgeOutput, JudgeRequest, injectable, sessions_of,
};
use super::*;
use kgraf_core::session::{
    ConnectionBasis, ConnectionFacts, ConnectionState, Decider, InboxItem, SessionId, connection_id,
};
use kgraf_runtime::InjectError;

/// A sender's messages in the last minute on one connection before pacing
/// starts (design: "a soft message rate of 12 per minute per sender per
/// connection").
pub(super) const SOFT_MESSAGES_PER_MINUTE: usize = 12;
/// The sliding window the soft rate is counted over.
const WINDOW_MS: i64 = 60_000;
/// Delay per message over the soft rate.
const DELAY_PER_EXCESS_MS: i64 = 5_000;
/// Longest pacing delay.
const MAX_DELAY_MS: i64 = 5 * 60_000;

/// Pacing over a sliding window, per sender and connection.
#[derive(Default)]
pub(super) struct Pacer {
    sent: BTreeMap<(String, String), VecDeque<i64>>,
}

/// One message's pacing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Paced {
    /// Messages from this sender on this connection in the window, this one
    /// included.
    pub(super) recent: usize,
    /// When the message may be delivered.
    pub(super) not_before_ms: i64,
}

impl Pacer {
    /// Count a message from `sender` on connection `conn` at `now`, and give
    /// its delivery time: `now`, delayed past the soft rate, and never before
    /// `last_pending_ms` (the latest undelivered message this sender queued
    /// on the connection), so delivery keeps send order.
    pub(super) fn pace(
        &mut self,
        sender: &str,
        conn: &str,
        now: i64,
        last_pending_ms: Option<i64>,
    ) -> Paced {
        let window = self
            .sent
            .entry((sender.to_owned(), conn.to_owned()))
            .or_default();
        while window.front().is_some_and(|t| *t <= now - WINDOW_MS) {
            window.pop_front();
        }
        window.push_back(now);
        let recent = window.len();
        let excess = recent.saturating_sub(SOFT_MESSAGES_PER_MINUTE) as i64;
        let delayed = now + (DELAY_PER_EXCESS_MS * excess).min(MAX_DELAY_MS);
        Paced {
            recent,
            not_before_ms: last_pending_ms.map_or(delayed, |last| delayed.max(last)),
        }
    }
}

/// Which sessions the re-check must look at: those marked since
/// the last pass, and those whose earliest paced row is due.
#[derive(Default)]
pub(super) struct Recheck {
    marked: BTreeSet<SessionId>,
    paced: BTreeMap<SessionId, i64>,
}

impl Recheck {
    /// `session` has a row to inject as soon as its injector exists.
    pub(super) fn mark(&mut self, session: &SessionId) {
        self.marked.insert(session.clone());
    }

    /// `session` has a row that is due at `at`.
    pub(super) fn pace(&mut self, session: &SessionId, at: i64) {
        let e = self.paced.entry(session.clone()).or_insert(at);
        *e = (*e).min(at);
    }

    /// Whether a paced row is waiting for `session`.
    pub(super) fn is_paced(&self, session: &SessionId) -> bool {
        self.paced.contains_key(session)
    }

    /// The sessions to look at now, taken off the lists. Nothing to do costs
    /// two empty-collection checks.
    pub(super) fn due(&mut self, now: i64) -> Vec<SessionId> {
        if self.marked.is_empty() && self.paced.values().all(|t| *t > now) {
            return Vec::new();
        }
        let mut out: BTreeSet<SessionId> = std::mem::take(&mut self.marked);
        let due: Vec<SessionId> = self
            .paced
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(s, _)| s.clone())
            .collect();
        for s in due {
            self.paced.remove(&s);
            out.insert(s);
        }
        out.into_iter().collect()
    }
}

/// What happens once a queued pair is judged.
enum AfterJudge {
    /// `request_connection`: the verdict is the reply.
    Reply(oneshot::Sender<Result<(), SessionError>>),
    /// `send_message`: an open verdict delivers the message.
    Deliver {
        text: String,
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
}

/// One request waiting for a judgment.
struct Waiter {
    from: SessionId,
    to: SessionId,
    reason: UntrustedText,
    then: AfterJudge,
}

/// The batch being judged: its hold, its waiters and its task.
struct Judging {
    reservation: Option<Reservation>,
    waiters: Vec<Waiter>,
    task: tokio::task::JoinHandle<()>,
}

/// Requests waiting for the next batch, and the one batch in flight.
#[derive(Default)]
pub(super) struct JudgeQueue {
    queued: Vec<Waiter>,
    /// When the oldest queued request arrived: a batch starts one tick later,
    /// so requests that arrive together are judged together.
    since_ms: Option<i64>,
    judging: Option<Judging>,
}

impl JudgeQueue {
    /// The batch in flight's hold and task, for the run's end. Queued
    /// waiters are dropped with it (their calls end as stopped).
    pub(super) fn abort(&mut self) -> Option<(Option<Reservation>, tokio::task::JoinHandle<()>)> {
        self.queued.clear();
        self.since_ms = None;
        self.judging.take().map(|j| (j.reservation, j.task))
    }
}

impl RunLoop {
    fn connection_state(&self, a: &SessionId, b: &SessionId) -> Option<ConnectionState> {
        let id = connection_id(a, b);
        sessions_of(&self.s)
            .connections_of(a)
            .ok()?
            .into_iter()
            .find(|c| c.id == id)
            .map(|c| c.state)
    }

    /// Recompute every rule-covered connection from the run's own state.
    pub(super) fn refresh_rule_connections(&mut self) {
        let contracts = self.record.open_contracts.clone();
        // `agent_connections.a`/`.b` are foreign keys: every dispatched worker
        // needs a row before a rule can connect it.
        for c in &contracts {
            for w in &c.workers {
                self.ensure_session_row(w);
            }
        }
        let mut pairs: Vec<(SessionId, SessionId, ConnectionBasis)> = Vec::new();
        for c in &contracts {
            for i in 0..c.workers.len() {
                for j in (i + 1)..c.workers.len() {
                    pairs.push((
                        c.workers[i].clone(),
                        c.workers[j].clone(),
                        ConnectionBasis::Sibling,
                    ));
                }
            }
            for dep in &c.depends_on {
                let Some(d) = contracts.iter().find(|d| &d.contract == dep) else {
                    continue;
                };
                for a in &c.workers {
                    for b in &d.workers {
                        pairs.push((
                            a.clone(),
                            b.clone(),
                            ConnectionBasis::Dependency { on: dep.clone() },
                        ));
                    }
                }
            }
        }
        // Siblings under one parent (decision 0067, point 6); parent and
        // child are connected at the spawn (`subagents.rs`).
        for (a, b) in self.child_sibling_pairs() {
            pairs.push((a, b, ConnectionBasis::Sibling));
        }
        let subs = self.record.coordination.subscriptions.clone();
        let ids: Vec<&String> = subs.keys().collect();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let (ca, cb) = (ids[i], ids[j]);
                let Some(path) = subs[ca].intersection(&subs[cb]).next().cloned() else {
                    continue;
                };
                let workers = |id: &String| {
                    contracts
                        .iter()
                        .find(|c| &c.contract == id)
                        .map(|c| c.workers.clone())
                        .unwrap_or_default()
                };
                for a in &workers(ca) {
                    for b in &workers(cb) {
                        pairs.push((
                            a.clone(),
                            b.clone(),
                            ConnectionBasis::SharedRead { path: path.clone() },
                        ));
                    }
                }
            }
        }
        for (a, b, basis) in pairs {
            if let Err(e) = self.open_by_rule(&a, &b, basis) {
                eprintln!("kgraf-orchestrator: a rule connection {a}--{b}: {e}");
            }
        }
    }

    /// `request_connection` (decision 0067, point 6): open at once if a rule
    /// covers the pair or it is already open, refused at once if the pair was
    /// refused before in this run, and queued for the next judgment batch
    /// otherwise.
    pub(super) async fn hook_request_connection(
        &mut self,
        from: AgentId,
        to: AgentId,
        reason: String,
        reply: oneshot::Sender<Result<(), SessionError>>,
    ) -> Result<(), OrchestratorError> {
        if reason.trim().is_empty() {
            let _ = reply.send(Err(SessionError::Empty));
            return Ok(());
        }
        if let Some(refusal) = self.refusal_of(&to) {
            let _ = reply.send(Err(refusal));
            return Ok(());
        }
        if from == to {
            let _ = reply.send(Ok(()));
            return Ok(());
        }
        self.ensure_session_row(&from);
        self.ensure_session_row(&to);
        self.refresh_rule_connections();
        match self.connection_state(&from, &to) {
            Some(ConnectionState::Open) => {
                let _ = reply.send(Ok(()));
            }
            Some(ConnectionState::Refused { reason }) => {
                let _ = reply.send(Err(SessionError::Refused(reason)));
            }
            Some(ConnectionState::Closed) => {
                let _ = reply.send(Err(SessionError::Refused(
                    "the connection is closed".into(),
                )));
            }
            Some(ConnectionState::Pending) | None => self.queue_judgment(Waiter {
                from,
                to,
                reason: UntrustedText::new(reason).for_storage(),
                then: AfterJudge::Reply(reply),
            }),
        }
        Ok(())
    }

    /// `send_message` (decision 0067, point 6). The recipient's liveness is
    /// checked before anything is judged or persisted: a finished
    /// agent refuses with the offer to send it to the orchestrator, an
    /// unknown id refuses as unknown. Over an open connection the message is
    /// delivered at once; a pair with no connection is held until the next
    /// judgment batch decides it.
    pub(super) async fn hook_send_message(
        &mut self,
        from: AgentId,
        to: AgentId,
        text: String,
        reply: oneshot::Sender<Result<(), SessionError>>,
    ) -> Result<(), OrchestratorError> {
        if text.trim().is_empty() {
            let _ = reply.send(Err(SessionError::Empty));
            return Ok(());
        }
        if let Some(refusal) = self.refusal_of(&to) {
            let _ = reply.send(Err(refusal));
            return Ok(());
        }
        if from == to {
            let _ = reply.send(Err(SessionError::Refused(
                "an agent cannot message itself".into(),
            )));
            return Ok(());
        }
        self.ensure_session_row(&from);
        self.ensure_session_row(&to);
        self.refresh_rule_connections();
        match self.connection_state(&from, &to) {
            Some(ConnectionState::Open) => {
                let _ = reply.send(self.deliver_message(&from, &to, text));
            }
            Some(ConnectionState::Refused { reason }) => {
                let _ = reply.send(Err(SessionError::Refused(reason)));
            }
            Some(ConnectionState::Closed) => {
                let _ = reply.send(Err(SessionError::Refused(
                    "the connection is closed".into(),
                )));
            }
            Some(ConnectionState::Pending) | None => self.queue_judgment(Waiter {
                from,
                to,
                reason: UntrustedText::new("a message was sent with no open connection"),
                then: AfterJudge::Deliver { text, reply },
            }),
        }
        Ok(())
    }

    /// Hold a request for the next batch; the pair shows as pending.
    fn queue_judgment(&mut self, w: Waiter) {
        if self.connection_state(&w.from, &w.to).is_none() {
            let facts = ConnectionFacts {
                id: connection_id(&w.from, &w.to),
                run: kgraf_core::RunId::new(self.record.run.to_string()),
                a: w.from.clone(),
                b: w.to.clone(),
                basis: ConnectionBasis::Requested {
                    reason: w.reason.to_description(240),
                },
                state: ConnectionState::Pending,
                decided_by: None,
                decided_ms: None,
                messages: 0,
                last_ms: None,
            };
            if let Err(e) = sessions_of(&self.s).upsert_connection(&facts) {
                eprintln!("kgraf-orchestrator: a pending connection could not be recorded: {e}");
            }
        }
        let now = self.now();
        let q = &mut self.sessions_rt.judge;
        q.since_ms.get_or_insert(now);
        q.queued.push(w);
    }

    /// Start one judgment batch over every queued request, once the oldest
    /// has waited a tick and no batch is in flight. The call runs
    /// in a spawned task; its result comes back as [`super::SessionWork`].
    pub(super) fn start_judging(&mut self) {
        let now = self.now();
        let tick = i64::try_from(self.config.tick.as_millis()).unwrap_or(i64::MAX);
        let q = &self.sessions_rt.judge;
        if q.judging.is_some()
            || q.queued.is_empty()
            || q.since_ms.is_some_and(|t| now < t.saturating_add(tick))
        {
            return;
        }
        let queued = std::mem::take(&mut self.sessions_rt.judge.queued);
        self.sessions_rt.judge.since_ms = None;
        // A pair decided since its request was queued (by the batch that was
        // in flight then) is answered from that decision, not paid for again.
        let mut waiters: Vec<Waiter> = Vec::new();
        for w in queued {
            match self.connection_state(&w.from, &w.to) {
                Some(ConnectionState::Open) => self.resolve_waiter(w, true, String::new()),
                Some(ConnectionState::Refused { reason }) => self.resolve_waiter(w, false, reason),
                Some(ConnectionState::Closed) => {
                    self.resolve_waiter(w, false, "the connection is closed".into())
                }
                Some(ConnectionState::Pending) | None => waiters.push(w),
            }
        }
        if waiters.is_empty() {
            return;
        }
        // One request per pair, whichever direction and however many waiters.
        let mut requests: Vec<ConnectionRequest> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for w in &waiters {
            if seen.insert(connection_id(&w.from, &w.to)) {
                requests.push(ConnectionRequest {
                    pair: (w.from.clone(), w.to.clone()),
                    // The model's copy (decision 0071).
                    reason: w.reason.for_model(),
                });
            }
        }
        let Some(answerer) = self.question_answerer.clone() else {
            let reason = "no connection judge is configured for this run".to_owned();
            self.session_notice(
                "",
                "",
                format!(
                    "{} connection request(s) were refused unjudged: {reason}",
                    requests.len()
                ),
            );
            self.resolve_all(waiters, &reason);
            return;
        };
        let input = requests
            .iter()
            .map(|r| (r.reason.len() as u64).div_ceil(4) + 30)
            .sum::<u64>()
            + 200;
        let reservation = self.reserve_call(&answerer.cost_basis(), input);
        let mut agents: Vec<(SessionId, String)> = Vec::new();
        for r in &requests {
            for s in [&r.pair.0, &r.pair.1] {
                if agents.iter().any(|(a, _)| a == s) {
                    continue;
                }
                let brief = self
                    .contract_of(s)
                    .and_then(|c| {
                        self.record
                            .contract(&c)
                            .map(|oc| kgraf_secrets::model_copy(&oc.instruction).into_owned())
                    })
                    .map(|i| i.chars().take(300).collect())
                    .unwrap_or_default();
                agents.push((s.clone(), brief));
            }
        }
        let request = JudgeRequest {
            original_instruction: kgraf_secrets::model_copy(&crate::delta::instruction_text(
                &self.record,
            ))
            .into_owned(),
            requests,
            agents,
        };
        let wait_from = super::sessions::WaitFrom {
            user: self.sessions_rt.answer_wait,
            answerer: answerer.clone(),
        };
        let task = super::sessions::spawn_session_call(
            self.tx.clone(),
            wait_from,
            "connection judgment",
            async move { answerer.judge_connections(&request).await },
            super::sessions::SessionWork::Judged,
            None,
        );
        self.sessions_rt.judge.judging = Some(Judging {
            reservation: Some(reservation),
            waiters,
            task,
        });
    }

    /// Refuse every waiter with `reason`, recording nothing on the pairs: an
    /// unjudged request is not a decision, so a later one is judged afresh.
    fn resolve_all(&mut self, waiters: Vec<Waiter>, reason: &str) {
        for w in waiters {
            let reply = match w.then {
                AfterJudge::Reply(r) | AfterJudge::Deliver { reply: r, .. } => r,
            };
            let _ = reply.send(Err(SessionError::Refused(reason.to_owned())));
        }
    }

    /// A judgment batch came back: settle its hold, record each pair's
    /// verdict, and answer every waiter. A pair the judge gave no verdict for
    /// is refused, with that reason.
    pub(super) fn on_judged(
        &mut self,
        result: Result<JudgeOutput, PlanError>,
    ) -> Result<(), OrchestratorError> {
        let Some(j) = self.sessions_rt.judge.judging.take() else {
            return Ok(());
        };
        let verdicts = match result {
            Ok(out) => {
                self.note_malformed_retries("connection judgment", out.retried_calls.len());
                if let Some(r) = j.reservation {
                    let calls: Vec<CallId> =
                        out.retried_calls.into_iter().chain(out.call).collect();
                    self.settle_call(r, Some(&calls))?;
                }
                out.verdicts
            }
            Err(e) => {
                if let Some(r) = j.reservation {
                    self.settle_call(r, e.calls())?;
                }
                let reason = format!("the connection judgment failed: {e}");
                self.session_notice("", "", reason.clone());
                self.resolve_all(j.waiters, &reason);
                return self.save();
            }
        };
        let mut decided: BTreeMap<String, ConnectionVerdict> = BTreeMap::new();
        for w in &j.waiters {
            let pair = connection_id(&w.from, &w.to);
            if decided.contains_key(&pair) {
                continue;
            }
            let v = verdicts
                .iter()
                .find(|v| connection_id(&v.pair.0, &v.pair.1) == pair)
                .cloned()
                .unwrap_or_else(|| ConnectionVerdict {
                    pair: (w.from.clone(), w.to.clone()),
                    open: false,
                    reason: "the orchestrator's judgment gave no verdict for this pair".into(),
                });
            self.record_verdict(&w.from, &w.to, &v);
            decided.insert(pair, v);
        }
        for w in j.waiters {
            let Some(v) = decided.get(&connection_id(&w.from, &w.to)).cloned() else {
                continue;
            };
            self.resolve_waiter(w, v.open, v.reason);
        }
        self.save()
    }

    /// Answer one waiter with its pair's decision: the reply of a
    /// `request_connection`, or the delivery (or refusal) of a held
    /// `send_message`. A recipient that ended while its pair was decided
    /// refuses as retired.
    fn resolve_waiter(&mut self, w: Waiter, open: bool, reason: String) {
        match w.then {
            AfterJudge::Reply(reply) => {
                let _ = reply.send(if open {
                    Ok(())
                } else {
                    Err(SessionError::Refused(reason))
                });
            }
            AfterJudge::Deliver { text, reply } => {
                let result = if !open {
                    Err(SessionError::Refused(reason))
                } else if let Some(refusal) = self.refusal_of(&w.to) {
                    Err(refusal)
                } else {
                    self.deliver_message(&w.from, &w.to, text)
                };
                let _ = reply.send(result);
            }
        }
    }

    fn record_verdict(&mut self, from: &AgentId, to: &AgentId, v: &ConnectionVerdict) {
        // The judge's reason is model text, stored and shown: persist-scrubbed
        // (decision 0071).
        let reason = kgraf_secrets::scrub_persist(&v.reason).into_owned();
        let now = self.now();
        let facts = ConnectionFacts {
            id: connection_id(from, to),
            run: kgraf_core::RunId::new(self.record.run.to_string()),
            a: from.clone(),
            b: to.clone(),
            basis: ConnectionBasis::Requested {
                reason: reason.clone(),
            },
            state: if v.open {
                ConnectionState::Open
            } else {
                ConnectionState::Refused {
                    reason: reason.clone(),
                }
            },
            decided_by: Some(Decider::Orchestrator),
            decided_ms: Some(now),
            messages: 0,
            last_ms: None,
        };
        if let Err(e) = sessions_of(&self.s).upsert_connection(&facts) {
            eprintln!("kgraf-orchestrator: a connection verdict could not be recorded: {e}");
        }
    }

    /// Deliver `text` from `from` to `to` over an open connection: paced
    /// (never refused for volume), persisted, then injected now or by the
    /// re-check. A store failure is reported to the sender, never dropped.
    fn deliver_message(
        &mut self,
        from: &AgentId,
        to: &AgentId,
        text: String,
    ) -> Result<(), SessionError> {
        let store = sessions_of(&self.s);
        let conn = connection_id(from, to);
        let now = self.now();
        let source = InboxSource::Agent { id: from.clone() };
        let pending = store
            .undelivered(to)
            .map_err(|e| SessionError::NotPersisted(e.to_string()))?;
        let last_pending = pending
            .iter()
            .filter(|i| i.source == source)
            .map(|i| i.not_before_ms)
            .max();
        // An earlier row that is due but not delivered yet (its injector was
        // not ready, or its paced time passed since the last re-check): this
        // message must not overtake it.
        let earlier_due = pending
            .iter()
            .any(|i| injectable(i.kind) && i.not_before_ms <= now);
        let paced = self
            .sessions_rt
            .pacer
            .pace(from.as_str(), &conn, now, last_pending);
        let n = self.sessions_rt.next_id();
        let item = InboxItem {
            id: format!("msg-{from}-{to}-{now}-{n}"),
            session: to.clone(),
            source,
            kind: InboxKind::Message,
            mode: InjectMode::AtBoundary,
            // Raw: the inbox store takes it in (decision 0071); every
            // injection quotes it as data (`sessions::model_text`).
            text,
            received_ms: now,
            not_before_ms: paced.not_before_ms,
            delivered: None,
        };
        store
            .push_inbox(&item)
            .map_err(|e| SessionError::NotPersisted(e.to_string()))?;
        if let Ok(Some(mut c)) = store
            .connections_of(from)
            .map(|v| v.into_iter().find(|c| c.id == conn))
        {
            c.messages += 1;
            c.last_ms = Some(now);
            if let Err(e) = store.upsert_connection(&c) {
                eprintln!("kgraf-orchestrator: a connection's count could not be updated: {e}");
            }
        }
        if paced.not_before_ms > now {
            if let Some(contract) = self.contract_of(to) {
                self.note_wait(
                    &contract,
                    crate::waits::WaitState::PacedDelivery {
                        detail: format!(
                            "{} messages/min from {from} to {to}; delivery paced to {} ms from \
                             now",
                            paced.recent,
                            paced.not_before_ms - now
                        ),
                    },
                );
            }
            self.sessions_rt.recheck.pace(to, paced.not_before_ms);
            // The notice is visible at once, not at the loop's next
            // unrelated save. The message itself is already persisted, so a
            // failed record save is said, not returned as its failure.
            if let Err(e) = self.save() {
                eprintln!("kgraf-orchestrator: a pacing notice could not be saved: {e}");
            }
        } else if earlier_due {
            // Delivered with the rows before it, in inbox order.
            self.sessions_rt.recheck.mark(to);
            if let Err(e) = self.recheck_inbox() {
                eprintln!("kgraf-orchestrator: the inbox of {to} could not be delivered: {e}");
            }
        } else {
            self.inject_now_or_later(to, &item);
        }
        Ok(())
    }

    /// Deliver the inbox rows of every session the [`Recheck`] names: each
    /// due, injectable row of a session whose injector exists. A session not
    /// running any more is dropped from the lists: its resume delivers them.
    pub(super) fn recheck_inbox(&mut self) -> Result<(), OrchestratorError> {
        let now = self.now();
        let due = self.sessions_rt.recheck.due(now);
        if due.is_empty() {
            return Ok(());
        }
        let mut cleared = false;
        let store = sessions_of(&self.s);
        for session in due {
            let Some(injector) = self.injector_of(&session) else {
                if self.is_running(&session) {
                    // Dispatched, but its worker is not spawned yet.
                    self.sessions_rt.recheck.mark(&session);
                }
                continue;
            };
            let rows = match store.undelivered(&session) {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("kgraf-orchestrator: the inbox of {session} could not be read: {e}");
                    self.sessions_rt.recheck.mark(&session);
                    continue;
                }
            };
            for item in rows.into_iter().filter(|i| injectable(i.kind)) {
                if item.not_before_ms > now {
                    self.sessions_rt.recheck.pace(&session, item.not_before_ms);
                    continue;
                }
                match injector.inject(super::sessions::injection_of(
                    &item,
                    &kgraf_secrets::model_copy(&item.text),
                )) {
                    Ok(()) | Err(InjectError::Duplicate { .. }) => {}
                    Err(InjectError::Finished { .. }) => break,
                    // Not built yet: the rows stay undelivered, so the
                    // session is checked again on a later pass.
                    Err(InjectError::NotBuilt) => {
                        self.sessions_rt.recheck.mark(&session);
                        break;
                    }
                }
            }
            if !self.sessions_rt.recheck.is_paced(&session)
                && let Some(contract) = self.contract_of(&session)
                && self.in_flight.contains_key(&contract)
                && matches!(
                    self.record
                        .coordination
                        .waits
                        .get(&contract)
                        .map(|w| w.kind()),
                    Some(crate::waits::WaitKind::PacedDelivery)
                )
            {
                self.note_wait(&contract, crate::waits::WaitState::Running);
                cleared = true;
            }
        }
        if cleared { self.save() } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_twelve_in_a_minute_are_not_paced_and_the_thirteenth_is() {
        let mut p = Pacer::default();
        for i in 0..12 {
            let paced = p.pace("a", "a--b", 1_000 + i, None);
            assert_eq!(paced.not_before_ms, 1_000 + i, "message {i}");
        }
        let paced = p.pace("a", "a--b", 1_012, None);
        assert_eq!(paced.recent, 13);
        assert_eq!(paced.not_before_ms, 1_012 + DELAY_PER_EXCESS_MS);
    }

    /// A burst then a gap: 25 fast messages, then a 61 s gap. The window
    /// has emptied, so the 26th is not paced for volume, but it is never
    /// scheduled before the 25th, which is still pending.
    #[test]
    fn after_a_burst_and_a_gap_a_message_is_never_scheduled_before_a_pending_one() {
        let mut p = Pacer::default();
        let mut last = 0;
        for i in 0..25 {
            let paced = p.pace("a", "a--b", i, Some(last));
            assert!(paced.not_before_ms >= last, "message {i} jumped the queue");
            last = paced.not_before_ms;
        }
        assert_eq!(last, 24 + 13 * DELAY_PER_EXCESS_MS, "the 25th is 13 over");
        let after_gap = 24 + 61_000;
        let paced = p.pace("a", "a--b", after_gap, Some(last));
        assert_eq!(paced.recent, 1, "the sliding window emptied during the gap");
        assert!(
            paced.not_before_ms >= last,
            "the 26th ({}) must not be delivered before the 25th ({last})",
            paced.not_before_ms
        );
    }

    /// One message a minute, for an hour, is never paced.
    #[test]
    fn one_message_a_minute_is_never_paced() {
        let mut p = Pacer::default();
        for i in 0..60 {
            let now = i * 60_000;
            let paced = p.pace("a", "a--b", now, None);
            assert_eq!(paced.not_before_ms, now, "minute {i}");
            assert_eq!(paced.recent, 1, "minute {i}");
        }
    }

    /// The window slides: twelve messages spread over the last 59 s still
    /// count, so the next one is paced even though the first came long ago.
    #[test]
    fn the_window_slides_rather_than_resetting() {
        let mut p = Pacer::default();
        p.pace("a", "a--b", 0, None);
        for i in 0..11 {
            p.pace("a", "a--b", 50_000 + i * 1_000, None);
        }
        // At 60_500 the message at 0 has left the window; eleven remain.
        let paced = p.pace("a", "a--b", 60_500, None);
        assert_eq!(paced.recent, 12);
        assert_eq!(paced.not_before_ms, 60_500);
        let paced = p.pace("a", "a--b", 60_600, None);
        assert_eq!(paced.recent, 13);
        assert_eq!(paced.not_before_ms, 60_600 + DELAY_PER_EXCESS_MS);
    }

    #[test]
    fn senders_and_connections_are_paced_apart() {
        let mut p = Pacer::default();
        for i in 0..12 {
            p.pace("a", "a--b", i, None);
        }
        assert_eq!(p.pace("b", "a--b", 20, None).not_before_ms, 20);
        assert_eq!(p.pace("a", "a--c", 20, None).not_before_ms, 20);
    }

    #[test]
    fn the_delay_is_capped_at_five_minutes() {
        let mut p = Pacer::default();
        let mut last = 0;
        for i in 0..200 {
            last = p.pace("a", "a--b", i, None).not_before_ms;
        }
        assert_eq!(last, 199 + MAX_DELAY_MS);
    }

    /// An idle re-check names nothing; a marked session is named once; a
    /// paced one only once it is due.
    #[test]
    fn the_recheck_names_only_marked_and_due_sessions() {
        let mut r = Recheck::default();
        assert!(r.due(0).is_empty());
        let a = SessionId::new("a");
        let b = SessionId::new("b");
        r.mark(&a);
        r.pace(&b, 100);
        assert_eq!(r.due(50), vec![a.clone()]);
        assert!(r.due(50).is_empty(), "a marked session is named once");
        assert!(r.is_paced(&b));
        r.pace(&b, 80);
        assert_eq!(r.due(90), vec![b.clone()], "the earliest paced time counts");
        assert!(!r.is_paced(&b));
    }
}
