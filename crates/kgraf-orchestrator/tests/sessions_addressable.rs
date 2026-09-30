//! Decision 0067: addressable sessions, `ask_user_question` and
//! connections in the orchestrator, against a real `Orchestrator` (the same
//! harness `orchestrator.rs`'s own tests use; no network). Further fixes
//! have their own tests in `session_question_flow.rs` and `sessions_restart.rs`.
//!
//! `WorkerFake`'s `HANG` instruction parks a round-1 model call, so a hung
//! worker reaches no further boundary: these tests check persistence (the
//! inbox row, the run record's `direct:<session>` note) for a hung worker,
//! and delivery (the transcript) only where a worker reaches a boundary.

mod support;

use kgraf_core::session::{
    ConnectionState, InboxKind, InboxSource, InjectMode, SessionFacts, SessionKind, SessionOutcome,
    SessionRecords,
};
use kgraf_core::{AgentId, RunId};
use kgraf_graph::sessions::SharedSessions;
use kgraf_orchestrator::{
    AnswerOutput, AnswerRequest, ConnectionVerdict, CostBasis, JudgeOutput, JudgeRequest, Limits,
    Orchestrator, PlanError, QuestionAnswerer, RunStatus, RunStore, SessionError,
};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

fn record_of(p: &Parts) -> kgraf_orchestrator::RunStateRecord {
    RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record")
}

/// `record_of`, tolerant of the moment before the run's first save: a test
/// that spawns `o.start(..)` and polls right away must not race the record's
/// own creation.
fn try_record_of(p: &Parts) -> Option<kgraf_orchestrator::RunStateRecord> {
    RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .ok()
        .flatten()
}

fn sessions(p: &Parts) -> SharedSessions {
    SharedSessions(p.graph.clone())
}

/// Waits until a worker's session row exists (this file's own opportunistic
/// `ensure_session_row`), so a test can address it by id right after issuing
/// its first hook call.
async fn wait_hang(a: &Arc<WorkerFake>, b: &Arc<WorkerFake>) {
    while !(a.hang_reached.load(Ordering::SeqCst) || b.hang_reached.load(Ordering::SeqCst)) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------- send_to_session

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_to_a_running_worker_is_persisted_before_the_reply_and_recorded_on_the_run() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a HANG", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p.alpha, &p.beta).await;

    let worker = AgentId::new("r1-a-r0-a0");
    let id = handle
        .send_to_session(
            worker.clone(),
            InboxKind::Note,
            InjectMode::AtBoundary,
            "prefer small functions".into(),
        )
        .await
        .expect("send_to_session");

    // Persisted to the session's inbox before this call returned: a
    // reader outside the loop already sees it.
    let undelivered = sessions(&p).undelivered(&worker).expect("undelivered");
    assert!(
        undelivered.iter().any(|i| i.id == id),
        "the note must be in the session's inbox: {undelivered:?}"
    );
    // Also on the run record's `direct:<session>` channel (design point 2).
    let record = record_of(&p);
    assert!(
        record
            .notes
            .iter()
            .any(|n| n.channel == format!("direct:{worker}")),
        "the note must also land on the run record's direct channel: {:?}",
        record.notes
    );

    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    handle.stop_session(worker.clone()).await.ok();
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// **Mutation target 1**: the empty-text guard in `hook_send_to_session`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_note_is_refused_before_anything_is_persisted() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(100),
    );
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    tokio::time::sleep(Duration::from_millis(20)).await;
    let err = handle
        .send_to_session(
            AgentId::new("r1-a-r0-a0"),
            InboxKind::Note,
            InjectMode::AtBoundary,
            "   ".into(),
        )
        .await
        .expect_err("an empty (or whitespace-only) note is refused");
    assert_eq!(err, SessionError::Empty);
    handle.stop().await;
    let _ = run.await.expect("join");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_note_to_an_unknown_session_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(100),
    );
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    tokio::time::sleep(Duration::from_millis(20)).await;
    let err = handle
        .send_to_session(
            AgentId::new("no-such-session"),
            InboxKind::Note,
            InjectMode::AtBoundary,
            "hello".into(),
        )
        .await
        .expect_err("an unknown session is refused, not silently queued");
    assert!(matches!(err, SessionError::Unknown(_)), "{err:?}");
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// A note to a session whose contract already verified: `Retired`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_to_a_finished_session_is_refused_with_retired() {
    let dir = tempfile::tempdir().expect("dir");
    // `a` finishes quickly; `b` hangs, so the orchestrator (and its handle)
    // are still live once `a`'s session has retired.
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b HANG", "src/b.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a and b".into()));
    while !try_record_of(&p).is_some_and(|r| r.verified_contracts.iter().any(|c| c == "a")) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let err = handle
        .send_to_session(
            AgentId::new("r1-a-r0-a0"),
            InboxKind::Note,
            InjectMode::AtBoundary,
            "too late".into(),
        )
        .await
        .expect_err("a finished session refuses a note");
    assert!(matches!(err, SessionError::Retired { .. }), "{err:?}");

    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// **Mutation target 2**: `is_addressable`'s refusal, against a hand-written
/// `addressable: false` row rather than a real dispatched resolver (a real
/// one now exists, decision 0073: `kgraf-orchestrator`'s own `resolver.rs`
/// exercises it end to end, including this same refusal, live, through the
/// real dispatch path).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_to_a_non_addressable_session_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a HANG", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p.alpha, &p.beta).await;
    let worker = AgentId::new("r1-a-r0-a0");
    sessions(&p)
        .upsert_session(&SessionFacts {
            id: worker.clone(),
            run: RunId::new("r1"),
            kind: SessionKind::Worker,
            parent: None,
            contract: Some("a".into()),
            route: None,
            model: None,
            depth: 1,
            transcript: None,
            worktree: None,
            write_scope: Vec::new(),
            reasoning: kgraf_core::session::ReasoningVisibility::Unknown,
            started_ms: 0,
            addressable: false,
        })
        .expect("mark non-addressable");
    let err = handle
        .send_to_session(
            worker.clone(),
            InboxKind::Note,
            InjectMode::AtBoundary,
            "hello".into(),
        )
        .await
        .expect_err("a non-addressable session refuses a note (decision 0073)");
    assert!(matches!(err, SessionError::Refused(_)), "{err:?}");
    let stop_err = handle
        .stop_session(worker.clone())
        .await
        .expect_err("a non-addressable session also refuses a direct stop");
    assert!(matches!(stop_err, SessionError::Refused(_)), "{stop_err:?}");

    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    handle.stop().await;
    let _ = run.await.expect("join");
}

// -------------------------------------------------------------- ask_user_question

struct FakeAnswerer {
    /// `None`: never answers (falls through to the user every time).
    answer: Option<&'static str>,
    calls: Mutex<u32>,
}

#[async_trait::async_trait]
impl QuestionAnswerer for FakeAnswerer {
    async fn try_answer(&self, _request: &AnswerRequest) -> Result<AnswerOutput, PlanError> {
        *self.calls.lock().expect("lock") += 1;
        Ok(AnswerOutput {
            answer: self.answer.map(str::to_owned),
            call: None,
        })
    }

    fn cost_basis(&self) -> CostBasis {
        CostBasis {
            model: PLAN_MODEL.into(),
            max_output_tokens: 200,
        }
    }

    async fn judge_connections(&self, _r: &JudgeRequest) -> Result<JudgeOutput, PlanError> {
        Ok(JudgeOutput {
            verdicts: Vec::new(),
            call: None,
        })
    }
}

/// **Mutation target 3: the orchestrator-first branch.**
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_question_the_orchestrator_can_answer_never_reaches_the_user() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a HANG", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let answerer = Arc::new(FakeAnswerer {
        answer: Some("use src/config.toml"),
        calls: Mutex::new(0),
    });
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(answerer.clone());
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p.alpha, &p.beta).await;

    let admitted_before = record_of(&p).counters.budget_admitted;
    let answer = handle
        .ask_question(
            AgentId::new("r1-a-r0-a0"),
            "which config file do I read?".into(),
        )
        .await
        .expect("answered");
    assert_eq!(answer, "use src/config.toml");
    assert_eq!(
        *answerer.calls.lock().expect("lock"),
        1,
        "tried exactly once"
    );
    // The call was reserved, the same counter every other
    // orchestrator call (re-grounding, decomposition) is admitted through:
    // not a side channel, not free.
    assert!(
        record_of(&p).counters.budget_admitted > admitted_before,
        "the orchestrator-first answer must be a real reserved, ledgered call"
    );
    // Recorded and visible in the worker's own session (design point 4).
    let inbox = sessions(&p)
        .undelivered(&AgentId::new("r1-a-r0-a0"))
        .expect("undelivered");
    assert!(
        inbox
            .iter()
            .any(|i| i.kind == InboxKind::Answer && i.source == InboxSource::Orchestrator),
        "the answer must be visible in the session's inbox: {inbox:?}"
    );

    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// The other branch of the same mutation target: without an answerer (or one
/// that cannot answer), the question reaches the user, and the user's own
/// `answer_question` resolves it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_question_the_orchestrator_cannot_answer_waits_for_the_user() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a HANG", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    // No `with_question_answerer`: every question falls through to the user.
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p.alpha, &p.beta).await;

    let worker = AgentId::new("r1-a-r0-a0");
    let asked = tokio::spawn({
        let handle = handle.clone();
        let worker = worker.clone();
        async move {
            handle
                .ask_question(worker, "which config file do I read?".into())
                .await
        }
    });
    // The loop is free to keep running while the question waits (it is not
    // blocked inside `on_control`): the user answers question id 1.
    tokio::time::sleep(Duration::from_millis(30)).await;
    handle
        .answer_question(1, "src/config.toml".into())
        .await
        .expect("answer");
    let answer = asked.await.expect("join").expect("answered");
    assert_eq!(answer, "src/config.toml");
    let inbox = sessions(&p).undelivered(&worker).expect("undelivered");
    assert!(
        inbox
            .iter()
            .any(|i| i.kind == InboxKind::Answer && i.source == InboxSource::User),
        "the user's answer must also be recorded to the session: {inbox:?}"
    );

    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    handle.stop().await;
    let _ = run.await.expect("join");
}

// ---------------------------------------------------------------- connections

/// Two attempts of one contract are siblings (a rule-covered connection, no
/// model call), so a message between them delivers at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sibling_attempts_are_connected_by_rule_and_a_message_between_them_delivers() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[C {
        id: "a",
        instruction: "make a HANG",
        path: "src/a.rs",
        deps: &[],
        attempts: 2,
    }]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p.alpha, &p.beta).await;

    let a0 = AgentId::new("r1-a-r0-a0");
    let a1 = AgentId::new("r1-a-r0-a1");
    // Neither hook has touched these sessions yet: `send_message` itself
    // recomputes rule connections first.
    handle
        .send_message(
            a0.clone(),
            a1.clone(),
            "watch out for the shared file".into(),
        )
        .await
        .expect("sibling connection is open by rule; the message delivers");
    let state = sessions(&p)
        .connections_of(&a0)
        .expect("connections")
        .into_iter()
        .next()
        .expect("one connection")
        .state;
    assert_eq!(state, ConnectionState::Open);
    let inbox = sessions(&p).undelivered(&a1).expect("undelivered");
    assert!(
        inbox.iter().any(|i| i.kind == InboxKind::Message),
        "{inbox:?}"
    );

    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// **Mutation target 4**: a message with no connection and no configured
/// judge is held (refused, never delivered silently) and names why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_to_an_unconnected_agent_is_refused_with_a_reason_not_dropped() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a HANG", "src/a.rs", &[]),
        c("b", "make b HANG", "src/b.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a and b".into()));
    // Two independent contracts, not siblings: no rule covers this pair.
    while p.stats.requests().len() < 2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let a0 = AgentId::new("r1-a-r0-a0");
    let b0 = AgentId::new("r1-b-r0-a0");
    let err = handle
        .send_message(a0.clone(), b0.clone(), "hi".into())
        .await
        .expect_err("no rule and no judge: refused, not delivered");
    assert!(matches!(err, SessionError::Refused(_)), "{err:?}");
    let inbox = sessions(&p).undelivered(&b0).expect("undelivered");
    assert!(
        !inbox.iter().any(|i| i.kind == InboxKind::Message),
        "a refused message must never be delivered anyway: {inbox:?}"
    );

    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// `request_connection` judged and opened by a real (fake) judge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_requested_connection_a_judge_approves_opens_and_then_carries_messages() {
    struct OpenAll;
    #[async_trait::async_trait]
    impl QuestionAnswerer for OpenAll {
        async fn try_answer(&self, _r: &AnswerRequest) -> Result<AnswerOutput, PlanError> {
            Ok(AnswerOutput {
                answer: None,
                call: None,
            })
        }
        fn cost_basis(&self) -> CostBasis {
            CostBasis {
                model: PLAN_MODEL.into(),
                max_output_tokens: 50,
            }
        }
        async fn judge_connections(&self, r: &JudgeRequest) -> Result<JudgeOutput, PlanError> {
            Ok(JudgeOutput {
                verdicts: r
                    .requests
                    .iter()
                    .map(|q| ConnectionVerdict {
                        pair: q.pair.clone(),
                        open: true,
                        reason: "approved for the test".into(),
                    })
                    .collect(),
                call: None,
            })
        }
    }
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a HANG", "src/a.rs", &[]),
        c("b", "make b HANG", "src/b.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(Arc::new(OpenAll));
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a and b".into()));
    while p.stats.requests().len() < 2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let a0 = AgentId::new("r1-a-r0-a0");
    let b0 = AgentId::new("r1-b-r0-a0");
    handle
        .request_connection(
            a0.clone(),
            b0.clone(),
            "coordinating on a shared type".into(),
        )
        .await
        .expect("approved");
    handle
        .send_message(a0.clone(), b0.clone(), "use the Config struct".into())
        .await
        .expect("now open, so this delivers");

    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    handle.stop().await;
    let _ = run.await.expect("join");
}

// ------------------------------------------------------------------ stop/resume

/// **Mutation target 5**: stopping one attempt cancels only that attempt; a
/// sibling of the same contract keeps running (tried by exceeding the
/// claim, not by reading a flag).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_one_attempt_leaves_its_sibling_running() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[C {
        id: "a",
        instruction: "make a HANG",
        path: "src/a.rs",
        deps: &[],
        attempts: 2,
    }]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p.alpha, &p.beta).await;

    // `alpha` and `beta` each run one of the two attempts (round-robin
    // routing), each with its own `live` set. Stop whichever attempt is on
    // `alpha`; the other (on `beta`) is the sibling this test claims stays
    // untouched.
    let contract = record_of(&p)
        .open_contracts
        .into_iter()
        .find(|c| c.contract == "a")
        .expect("contract a");
    let alpha_index = contract
        .worker_routes
        .iter()
        .position(|r| r == "alpha")
        .expect("one attempt runs on alpha");
    let stopped_worker = contract.workers[alpha_index].clone();
    let (stopped_fake, sibling_fake) = (&p.alpha, &p.beta);
    handle
        .stop_session(stopped_worker.clone())
        .await
        .expect("stop one");
    // The contract is held at once (this hook's own write; see its doc
    // comment for the later race with `finalize`, reported).
    let held_now = record_of(&p)
        .open_contracts
        .iter()
        .any(|c| matches!(&c.status, kgraf_orchestrator::ContractStatus::Held { reason } if reason.contains("stopped by the user")));
    assert!(held_now, "{:?}", record_of(&p).open_contracts);
    tokio::time::sleep(Duration::from_millis(30)).await;
    // The sibling attempt is untouched: the contract is still in flight (it
    // alone would already have ended it if it, too, had been cancelled).
    let status = handle.status().await.expect("status");
    assert_eq!(status.in_flight, vec!["a".to_owned()]);

    // **Mutation target 5**, mechanism-level: the *stopped* attempt's own
    // `HANG` call was dropped by the runtime's cancellation (its instruction
    // left `live`); the sibling's own call is still parked (still in
    // `live`).
    assert!(
        stopped_fake.live.lock().expect("lock").is_empty(),
        "the stopped attempt's own call must have been dropped by its cancel"
    );
    assert!(
        !sibling_fake.live.lock().expect("lock").is_empty(),
        "the sibling's own call must still be parked: stopping one attempt must not \
         touch another"
    );

    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    // The sibling's own model call is already parked inside
    // `std::future::pending` (`WorkerFake`'s `HANG`), which never resolves on
    // its own -- unhanging the flag only changes a *future* round, not this
    // one already in flight. Only its own cancellation (a full stop) can end
    // it, so the property this test claims (stopping one attempt never
    // touched the other) is checked above, at the moment of the stop; ending
    // the run here is cleanup, not a second claim.
    handle.stop().await;
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resuming_an_unknown_session_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(100),
    );
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    tokio::time::sleep(Duration::from_millis(10)).await;
    let err = handle
        .resume_session(AgentId::new("no-such-session"))
        .await
        .expect_err("resuming a session this run never heard of is refused");
    assert!(matches!(err, SessionError::Unknown(_)), "{err:?}");
    handle.stop().await;
    let _ = run.await.expect("join");
}

// ------------------------------------------------------ session end records

/// A normal run leaves one session row per attempt, with the right
/// parent and outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_normal_run_leaves_one_session_row_per_attempt_with_parent_and_outcome() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed);

    let worker = AgentId::new("r1-a-r0-a0");
    let store = sessions(&p);
    let facts = store
        .session(&worker)
        .expect("read")
        .expect("a session row for the one attempt");
    assert_eq!(facts.kind, SessionKind::Worker);
    assert_eq!(facts.parent, Some(AgentId::new("orchestrator:r1")));
    assert_eq!(facts.contract.as_deref(), Some("a"));
    let (ended, _paused) = store
        .lifecycle(&worker)
        .expect("lifecycle")
        .expect("a known session");
    assert_eq!(
        ended.map(|(o, _)| o),
        Some(SessionOutcome::Completed),
        "the winning (only) attempt's session ends Completed"
    );
    // The orchestrator's own row exists too (the parent above resolves).
    assert!(
        store
            .session(&AgentId::new("orchestrator:r1"))
            .expect("read")
            .is_some()
    );
    assert_eq!(
        store
            .sessions_of_run(&RunId::new("r1"))
            .expect("of run")
            .len(),
        2,
        "one worker row plus the orchestrator's own row"
    );
}

/// A stopped attempt stays held on the live loop (the
/// run does not end on a user stop), its session is paused, not retired, and
/// `resume_session` continues its transcript with no paid round repeated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_attempt_stays_held_and_resume_repeats_no_paid_round() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a HANG", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p.alpha, &p.beta).await;

    let worker = AgentId::new("r1-a-r0-a0");
    handle.stop_session(worker.clone()).await.expect("stop");
    // The stopped attempt's `Finished` reaches `finalize`; the loop lives on.
    while !handle
        .status()
        .await
        .expect("the loop answers")
        .in_flight
        .is_empty()
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!run.is_finished(), "the run must not end on a user stop");
    let record = record_of(&p);
    let held = record
        .open_contracts
        .iter()
        .find(|c| c.contract == "a")
        .expect("contract a is still open");
    assert!(
        matches!(&held.status, kgraf_orchestrator::ContractStatus::Held { reason } if reason.contains("stopped by the user")),
        "{:?}",
        held.status
    );
    assert_eq!(held.stopped_by_user, vec![worker.clone()]);
    // The stopped attempt's session is paused, not retired (it resumes under
    // the same id below).
    let (ended, paused) = sessions(&p)
        .lifecycle(&worker)
        .expect("lifecycle")
        .expect("known");
    assert_eq!(ended, None, "a stopped session is paused, not ended");
    assert!(paused.is_some());
    let round0_before = p.stats.requests().iter().filter(|r| r.2 == 0).count();
    assert_eq!(round0_before, 1, "round 0 ran exactly once before the stop");

    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    handle.resume_session(worker.clone()).await.expect("resume");
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let round0_after = p.stats.requests().iter().filter(|r| r.2 == 0).count();
    assert_eq!(
        round0_after, 1,
        "resume continued the same worker's transcript: round 0 was not asked again"
    );
    let (ended, paused) = sessions(&p)
        .lifecycle(&worker)
        .expect("lifecycle")
        .expect("known");
    assert_eq!(ended.map(|(o, _)| o), Some(SessionOutcome::Completed));
    assert_eq!(paused, None, "resume clears the paused fact");
}

/// A worker attempt stopped by the user is charged for its calls when it
/// finishes. Resumed in the same process, its next result lists those calls
/// again (a resumed worker carries its earlier calls): only the calls made
/// after the resume may add to the run's spend.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_attempt_is_not_charged_again_for_its_earlier_calls() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a HANG", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p.alpha, &p.beta).await;
    let worker = AgentId::new("r1-a-r0-a0");
    handle.stop_session(worker.clone()).await.expect("stop");
    while !handle
        .status()
        .await
        .expect("the loop answers")
        .in_flight
        .is_empty()
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let spent_at_stop = p.admission.budget().spent_run("r1");
    let before = p.ledger.calls().expect("calls");
    assert!(
        before.iter().any(|c| c.agent_id == worker.as_str()
            && c.settlement == kgraf_provider::Settlement::Counted),
        "the worker made a counted call before the stop"
    );
    let seen: std::collections::BTreeSet<_> = before.into_iter().map(|c| c.id).collect();

    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    handle.resume_session(worker.clone()).await.expect("resume");
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let prices = prices();
    let added: kgraf_core::Money = p
        .ledger
        .calls()
        .expect("calls")
        .iter()
        .filter(|c| !seen.contains(&c.id) && c.settlement == kgraf_provider::Settlement::Counted)
        .map(|c| prices.cost(&c.model, &c.usage).expect("priced"))
        .sum();
    assert_eq!(
        p.admission.budget().spent_run("r1"),
        spent_at_stop + added,
        "only the calls made after the resume are added to the spend"
    );
}

// -------------------------------------------------------- undelivered notes

/// A note sent before the worker's opening round reaches a boundary at once
/// (design point 3), and is marked delivered from the forwarded
/// `WorkerEvent::Injected` (`run_attempt`'s event loop, not a fake).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivered_note_is_marked_delivered_from_the_forwarded_event() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(60),
    );
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    let worker = AgentId::new("r1-a-r0-a0");
    // Wait for the worker's `Injector` to exist (`run_attempt` fills the slot
    // right after `spawn_worker` returns), but well before round 0's 60 ms
    // delay elapses: sent while no round has completed yet, so it lands in
    // the opening user message at once (design point 3).
    while p.stats.requests().is_empty() {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let id = handle
        .send_to_session(
            worker.clone(),
            InboxKind::Note,
            InjectMode::AtBoundary,
            "prefer small functions".into(),
        )
        .await
        .expect("send_to_session");
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed);
    let undelivered = sessions(&p).undelivered(&worker).expect("undelivered");
    assert!(
        !undelivered.iter().any(|i| i.id == id),
        "the note reached a boundary and must be marked delivered, not left \
         undelivered: {undelivered:?}"
    );
}

/// **Mutation target 6.** A note that never reached a boundary is
/// reported to its sender when its session retires: a user's note to the
/// losing attempt of a two-attempt contract, retired when its sibling wins,
/// reaches the user as a session notice (the box's `RunEvent::SessionNotice`)
/// and the run report.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_undelivered_note_is_reported_to_its_sender() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[C {
        id: "a",
        instruction: "make a HANG",
        path: "src/a.rs",
        deps: &[],
        attempts: 2,
    }]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    // The attempt on alpha parks in round 1; the one on beta completes.
    // Parallel attempts all run to their end, so the parked one is failed
    // once the note is sent: the verifier then selects beta's, and alpha's
    // session retires with the note undelivered.
    p.alpha.hang.store(true, Ordering::SeqCst);
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_session_notices(Arc::new(move |t: String| {
            sink.lock().expect("lock").push(t);
        }));
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    while !p.alpha.hang_reached.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let contract = record_of(&p)
        .open_contracts
        .into_iter()
        .find(|c| c.contract == "a")
        .expect("contract a");
    let on_alpha = contract
        .worker_routes
        .iter()
        .position(|r| r == "alpha")
        .expect("one attempt runs on alpha");
    let parked = contract.workers[on_alpha].clone();
    handle
        .send_to_session(
            parked.clone(),
            InboxKind::Note,
            InjectMode::AtBoundary,
            "this will never reach a boundary".into(),
        )
        .await
        .expect("send_to_session");
    p.alpha.fail_parked.store(true, Ordering::SeqCst);
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let seen = seen.lock().expect("lock").clone();
    let reported = |t: &String| t.contains(parked.as_str()) && t.contains("not delivered");
    assert!(
        seen.iter().any(reported),
        "the never-delivered note must be reported to the user: {seen:?}"
    );
    assert!(report.notices.iter().any(reported), "{:?}", report.notices);
}

// -------------------------------------------------------- paced delivery

/// **Mutation target 7.** Twenty messages in a minute on one connection: the
/// first twelve (the soft rate) go through undelayed, the rest carry a
/// growing `not_before_ms`, arrive in queued order, and raise exactly one
/// `PacedDelivery` wait record (a repeated kind is not re-traced;
/// `waits.rs`'s own `only_a_change_of_kind_is_a_new_trace_record`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_messages_in_a_minute_are_paced_with_growing_delay_and_one_notice() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[C {
        id: "a",
        instruction: "make a HANG",
        path: "src/a.rs",
        deps: &[],
        attempts: 2,
    }]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p.alpha, &p.beta).await;

    let a0 = AgentId::new("r1-a-r0-a0");
    let a1 = AgentId::new("r1-a-r0-a1");
    for i in 0..20 {
        handle
            .send_message(a0.clone(), a1.clone(), format!("message {i}"))
            .await
            .expect("sibling connection is open by rule; never refused for volume");
    }
    let inbox = sessions(&p).undelivered(&a1).expect("undelivered");
    assert_eq!(inbox.len(), 20, "{inbox:?}");
    // Queued (and so injected) order matches send order.
    for (i, item) in inbox.iter().enumerate() {
        assert_eq!(item.text, format!("message {i}"), "{inbox:?}");
    }
    for item in &inbox[..12] {
        assert_eq!(
            item.not_before_ms, item.received_ms,
            "the first 12 (the soft rate) are not paced: {item:?}"
        );
    }
    let mut last_delay = 0;
    for item in &inbox[12..] {
        let delay = item.not_before_ms - item.received_ms;
        assert!(
            delay > last_delay,
            "each further message's delay must grow: {inbox:?}"
        );
        last_delay = delay;
    }
    handle.stop().await;
    let _ = run.await.expect("join");

    let log =
        kgraf_orchestrator::TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
            .await
            .expect("trace");
    let events = log.load().await.expect("load");
    let notices = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                kgraf_orchestrator::TraceEvent::Wait { contract, wait, .. }
                    if contract == "a"
                        && *wait == kgraf_orchestrator::WaitKind::PacedDelivery
            )
        })
        .count();
    assert_eq!(
        notices, 1,
        "one notice for the kind change, keyed by the recipient's contract, not \
         one per paced message: {events:?}"
    );
}

/// A re-split child's session parent is the
/// handed-back worker's own session (`workers.last()`, and the *second*
/// entry from the end of `lineage`, whose own last entry is the child's own
/// id -- both were the bug), not `orchestrator:<run>`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handoffs_child_session_parents_the_handed_back_worker() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = serde_json::json!({"contracts":[
        {"id":"big","instruction":"BIG part WRITE src/big.rs","acceptance":"src/big.rs exists",
         "writes":[{"path":"src/big.rs"}],"tools":["write_file"]}
    ]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut cfg = config();
    cfg.max_split_depth = 0;
    let o = Orchestrator::new(cfg, p.services(10_000)).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "a big part".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    // `write_path`'s `WRITE `-marker convention means a re-split child's own
    // instruction does not itself carry `BIG` forward (`WorkerFake`'s own
    // vocabulary), so this harness's `BIG` trigger does not recurse a second
    // hop; `report.verified` names one child. What is tested here, and is
    // the whole point of the correction, is that the child's own parent
    // names the handed-back *worker*, not `orchestrator:<run>`.
    assert_eq!(report.verified, vec!["big.h1".to_owned()], "{report:?}");

    let store = sessions(&p);
    let parent_of = |worker: &str| {
        store
            .session(&AgentId::new(worker))
            .expect("read")
            .and_then(|f| f.parent)
    };
    assert_eq!(
        parent_of("r1-big.h1-r0-a0").expect("big.h1 has a parent"),
        AgentId::new("r1-big-r0-a0"),
        "a re-split child's session parent is the handed-back worker, not the orchestrator's \
         own session"
    );
}

// -------------------------------------------------------- notes in the next prompt

/// A `send_to_session` note appears in the *next* orchestrator call's prompt
/// as quoted data (`direct:<session>`, the same mechanism the box's own
/// `/steer` uses; design point 2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direct_note_reaches_the_next_orchestrator_call_as_quoted_data() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &["a"]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(200),
    );
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let worker = AgentId::new("r1-a-r0-a0");
    handle
        .send_to_session(
            worker.clone(),
            InboxKind::Note,
            InjectMode::AtBoundary,
            "prefer small functions".into(),
        )
        .await
        .expect("send_to_session");
    // `b` depends on `a`; once `a` verifies, `b` becomes eligible and
    // re-grounds (0057) before its own dispatch, reading the pending note.
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");

    let record = record_of(&p);
    assert!(
        record
            .notes
            .iter()
            .any(|n| n.channel == format!("direct:{worker}") && n.delivered),
        "the note must reach an orchestrator call and be marked delivered: {:?}",
        record.notes
    );
    let prompt = p.grounder.prompts().join("\n");
    assert!(
        prompt.contains(&format!(
            "<inbound_message channel=\"direct:{worker}\" kind=\"note\""
        )),
        "the note must appear in the prompt as a quoted data block: {prompt}"
    );
}

/// A message to 20 recipients is not refused for volume (design point 6: the
/// hard `MAX_FANOUT` of 8 is removed). Every recipient is a real, running
/// session (an unknown id is refused as unknown).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_to_twenty_recipients_is_not_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let ids: Vec<String> = (0..20).map(|i| format!("r{i}")).collect();
    let mut contracts = vec![c("s", "make s HANG", "src/s.rs", &[])];
    let paths: Vec<String> = ids.iter().map(|i| format!("src/{i}.rs")).collect();
    for (id, path) in ids.iter().zip(&paths) {
        contracts.push(c(id, "make it HANG", path, &[]));
    }
    let plan = plan_json(&contracts);
    let mut p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.admission = admission(Limits::new(32, 64).expect("limits"));
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(Arc::new(OpenAllJudge));
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make them".into()));
    while p.stats.requests().iter().filter(|r| r.2 == 1).count() < 21 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let sender = AgentId::new("r1-s-r0-a0");
    for id in &ids {
        let to = AgentId::new(format!("r1-{id}-r0-a0"));
        handle
            .send_message(sender.clone(), to.clone(), format!("hello {id}"))
            .await
            .expect("a message to any of 20 recipients must never be refused for volume");
        let inbox = sessions(&p).undelivered(&to).expect("undelivered");
        assert!(
            inbox.iter().any(|m| m.text == format!("hello {id}")),
            "{to}: {inbox:?}"
        );
    }
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// Opens every pair it is asked to judge.
struct OpenAllJudge;

#[async_trait::async_trait]
impl QuestionAnswerer for OpenAllJudge {
    async fn try_answer(&self, _r: &AnswerRequest) -> Result<AnswerOutput, PlanError> {
        Ok(AnswerOutput {
            answer: None,
            call: None,
        })
    }
    fn cost_basis(&self) -> CostBasis {
        CostBasis {
            model: PLAN_MODEL.into(),
            max_output_tokens: 50,
        }
    }
    async fn judge_connections(&self, r: &JudgeRequest) -> Result<JudgeOutput, PlanError> {
        Ok(JudgeOutput {
            verdicts: r
                .requests
                .iter()
                .map(|q| ConnectionVerdict {
                    pair: q.pair.clone(),
                    open: true,
                    reason: "approved for the test".into(),
                })
                .collect(),
            call: None,
        })
    }
}

/// Property: every posted message ends either delivered (in its recipient's
/// inbox, so a reader can find it whether or not the runtime has drained it
/// yet) or reported refused to its sender -- never neither, whatever mix of
/// judge decisions it meets. Every recipient is a real, running session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_posted_message_ends_delivered_or_refused_never_lost() {
    /// Opens a pair whose recipient's contract number is even.
    struct MixedJudge;
    #[async_trait::async_trait]
    impl QuestionAnswerer for MixedJudge {
        async fn try_answer(&self, _r: &AnswerRequest) -> Result<AnswerOutput, PlanError> {
            Ok(AnswerOutput {
                answer: None,
                call: None,
            })
        }
        fn cost_basis(&self) -> CostBasis {
            CostBasis {
                model: PLAN_MODEL.into(),
                max_output_tokens: 50,
            }
        }
        async fn judge_connections(&self, r: &JudgeRequest) -> Result<JudgeOutput, PlanError> {
            let even = |id: &AgentId| {
                id.as_str()
                    .strip_prefix("r1-c")
                    .and_then(|rest| rest.chars().next())
                    .and_then(|c| c.to_digit(10))
                    .is_some_and(|d| d % 2 == 0)
            };
            Ok(JudgeOutput {
                verdicts: r
                    .requests
                    .iter()
                    .map(|q| ConnectionVerdict {
                        pair: q.pair.clone(),
                        open: even(&q.pair.1),
                        reason: if even(&q.pair.1) {
                            "approved for the test".into()
                        } else {
                            "refused for the test".into()
                        },
                    })
                    .collect(),
                call: None,
            })
        }
    }
    let dir = tempfile::tempdir().expect("dir");
    let ids: Vec<String> = (0..10).map(|i| format!("c{i}")).collect();
    let paths: Vec<String> = ids.iter().map(|i| format!("src/{i}.rs")).collect();
    let mut contracts = vec![c("s", "make s HANG", "src/s.rs", &[])];
    for (id, path) in ids.iter().zip(&paths) {
        contracts.push(c(id, "make it HANG", path, &[]));
    }
    let plan = plan_json(&contracts);
    let mut p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.admission = admission(Limits::new(32, 64).expect("limits"));
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(Arc::new(MixedJudge));
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make them".into()));
    while p.stats.requests().iter().filter(|r| r.2 == 1).count() < 11 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let sender = AgentId::new("r1-s-r0-a0");
    let mut sent = 0u32;
    let mut delivered_or_queued = 0u32;
    let mut refused = 0u32;
    for id in &ids {
        let to = AgentId::new(format!("r1-{id}-r0-a0"));
        sent += 1;
        match handle
            .send_message(sender.clone(), to.clone(), format!("hello {id}"))
            .await
        {
            Ok(()) => {
                // Accepted: it must actually be findable in the recipient's
                // inbox, not just an `Ok` with nothing behind it.
                let inbox = sessions(&p).undelivered(&to).expect("undelivered");
                assert!(
                    inbox.iter().any(|m| m.text == format!("hello {id}")),
                    "{id} was accepted but is not in {to}'s inbox"
                );
                delivered_or_queued += 1;
            }
            Err(SessionError::Refused(_)) => {
                let inbox = sessions(&p).undelivered(&to).expect("undelivered");
                assert!(
                    !inbox.iter().any(|m| m.text == format!("hello {id}")),
                    "{id} was refused but delivered anyway"
                );
                refused += 1;
            }
            Err(other) => panic!("{id} neither delivered nor refused: {other:?}"),
        }
    }
    assert_eq!(
        sent,
        delivered_or_queued + refused,
        "no message is unaccounted for"
    );
    assert_eq!((delivered_or_queued, refused), (5, 5), "the mix is real");

    handle.stop().await;
    let _ = run.await.expect("join");
}
