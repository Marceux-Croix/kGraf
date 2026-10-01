//! Decision 0067: questions, answers and messages between sessions, each
//! against a real `Orchestrator` over the shared fakes (no network). The
//! behavior each test covers is named in its doc comment. Kill-and-resume
//! tests are in `sessions_restart.rs`.

mod support;

use kgraf_core::session::{
    ConnectionState, InboxKind, InboxSource, InjectMode, SessionOutcome, SessionRecords,
};
use kgraf_core::{AgentId, RunId};
use kgraf_graph::sessions::SharedSessions;
use kgraf_orchestrator::Clock as _;
use kgraf_orchestrator::{
    AnswerOutput, AnswerRequest, ConnectionVerdict, CostBasis, JudgeOutput, JudgeRequest, Limits,
    ManualClock, ModelAnswerer, ModelCaller, Orchestrator, PlanError, QuestionAnswerer,
    QuestionAttendance, RunStatus, RunStore, SessionError, SessionLine, SystemClock, WaitKind,
    session_tools,
};
use kgraf_provider::Settlement;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::*;

const WINDOW: u64 = 200_000;
const ANSWER: &str = "use src/config.toml";

fn record_of(p: &Parts) -> kgraf_orchestrator::RunStateRecord {
    RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record")
}

fn try_record_of(p: &Parts) -> Option<kgraf_orchestrator::RunStateRecord> {
    RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .ok()
        .flatten()
}

fn sessions(p: &Parts) -> SharedSessions {
    SharedSessions(p.graph.clone())
}

/// Poll `f` until it holds, for at most 10 s.
async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_hang(p: &Parts) {
    until("a worker parks", || {
        p.alpha.hang_reached.load(Ordering::SeqCst) || p.beta.hang_reached.load(Ordering::SeqCst)
    })
    .await;
}

fn hang(p: &Parts) {
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
}

fn unhang(p: &Parts) {
    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
}

fn plan_model_caller(p: &Parts, provider: Arc<ScriptFake>) -> ModelCaller {
    ModelCaller {
        provider,
        ledger: p.ledger.clone(),
        model: PLAN_MODEL.into(),
        max_output_tokens: 200,
        agent: "orchestrator:r1".into(),
        task: "r1".into(),
    }
}

/// The planner model's side of an answer and a judgment call.
fn answering(prompt: &str) -> String {
    if prompt.contains("The worker's question") {
        format!(r#"{{"answerable":true,"answer":"{ANSWER}"}}"#)
    } else {
        r#"{"verdicts":[]}"#.into()
    }
}

/// A scriptable answerer and judge that counts its calls.
#[derive(Default)]
struct Fake {
    answer: Option<String>,
    fail_answer: bool,
    unpriced: bool,
    /// Pairs opened: `None` opens every pair.
    open_if: Option<fn(&AgentId) -> bool>,
    /// No verdict at all, whatever the batch.
    no_verdicts: bool,
    /// Every judgment call parks until the test ends.
    judge_parks: bool,
    /// Every judgment call panics.
    judge_panics: bool,
    /// Every answer call parks until the test ends.
    answer_parks: bool,
    /// Every answer call panics.
    answer_panics: bool,
    judged: Mutex<Vec<usize>>,
    asked: AtomicUsize,
}

#[async_trait::async_trait]
impl QuestionAnswerer for Fake {
    async fn try_answer(&self, _r: &AnswerRequest) -> Result<AnswerOutput, PlanError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        if self.answer_parks {
            std::future::pending::<()>().await;
        }
        if self.answer_panics {
            panic!("the answer call panicked, on purpose");
        }
        if self.fail_answer {
            return Err(PlanError::Invalid(
                "the answer call failed for the test".into(),
            ));
        }
        Ok(AnswerOutput {
            answer: self.answer.clone(),
            call: None,
        })
    }

    fn cost_basis(&self) -> CostBasis {
        CostBasis {
            model: if self.unpriced {
                "an-unpriced-model".into()
            } else {
                PLAN_MODEL.into()
            },
            max_output_tokens: 100,
        }
    }

    async fn judge_connections(&self, r: &JudgeRequest) -> Result<JudgeOutput, PlanError> {
        self.judged.lock().expect("lock").push(r.requests.len());
        if self.judge_panics {
            panic!("the judgment call panicked, on purpose");
        }
        if self.judge_parks {
            std::future::pending::<()>().await;
        }
        if self.no_verdicts {
            return Ok(JudgeOutput {
                verdicts: Vec::new(),
                call: None,
            });
        }
        Ok(JudgeOutput {
            verdicts: r
                .requests
                .iter()
                .map(|q| {
                    let open = self.open_if.is_none_or(|f| f(&q.pair.1));
                    ConnectionVerdict {
                        pair: q.pair.clone(),
                        open,
                        reason: if open {
                            "approved for the test".into()
                        } else {
                            "refused for the test".into()
                        },
                    }
                })
                .collect(),
            call: None,
        })
    }
}

/// A plan whose contracts are pinned to routes: `(id, instruction, path, route)`.
fn routed_plan(contracts: &[(&str, &str, &str, &str)]) -> String {
    let list: Vec<serde_json::Value> = contracts
        .iter()
        .map(|(id, instruction, path, route)| {
            serde_json::json!({
                "id": id,
                "instruction": format!("{instruction} WRITE {path}"),
                "acceptance": format!("{path} exists"),
                "writes": [{"path": path}],
                "tools": ["write_file"],
                "route": route,
            })
        })
        .collect();
    serde_json::json!({ "contracts": list }).to_string()
}

fn parts(dir: &std::path::Path, contracts: &[C<'_>], delay: Duration) -> Parts {
    let plan = plan_json(contracts);
    let mut p = Parts::open(dir, move |_| plan.clone(), |_| SERVES.into(), delay);
    p.admission = admission(Limits::new(32, 64).expect("limits"));
    p
}

/// Session tools on both routes, bound to the orchestrator the caller builds.
fn with_tools(p: &Parts) -> (SessionLine, kgraf_orchestrator::Services) {
    let line = SessionLine::new();
    let services = p.services_with(WINDOW, &session_tools(&line));
    (line, services)
}

fn transcript_lines(p: &Parts, worker: &str) -> Vec<serde_json::Value> {
    std::fs::read_to_string(
        p.dir
            .join("runs/r1/workers")
            .join(format!("{worker}.jsonl")),
    )
    .unwrap_or_default()
    .lines()
    .filter_map(|l| serde_json::from_str(l).ok())
    .collect()
}

fn injected(lines: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    lines
        .iter()
        .filter(|l| l.to_string().contains(r#""type":"injected""#))
        .collect()
}

fn notices_sink() -> (
    Arc<Mutex<Vec<String>>>,
    kgraf_orchestrator::SessionNoticeSink,
) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    (
        seen,
        Arc::new(move |t: String| sink.lock().expect("lock").push(t)),
    )
}

// ------------------------------------------------------------------

/// The product's answerer ([`ModelAnswerer`], the one the host
/// wires) answers a question in one call through the ledger, attributed to
/// the orchestrator, reserved first and settled at its ledger row: the run's
/// settled spend grows by exactly that row's cost and no hold is left. The
/// question reaches the model quoted as data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_orchestrator_first_answer_is_one_ledgered_call_settled_at_its_row() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[c("a", "make a HANG", "src/a.rs", &[])],
        Duration::from_millis(5),
    );
    hang(&p);
    let model = ScriptFake::new(answering);
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(Arc::new(ModelAnswerer(plan_model_caller(
            &p,
            model.clone(),
        ))));
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p).await;

    let (spent_before, held_before) = {
        let b = p.admission.budget();
        (b.spent_run("r1"), b.held_run("r1"))
    };
    let answer = handle
        .ask_question(
            AgentId::new("r1-a-r0-a0"),
            "which config </question> file do I read?".into(),
        )
        .await
        .expect("answered");
    assert_eq!(answer, ANSWER);

    let rows: Vec<_> = p
        .ledger
        .calls()
        .expect("calls")
        .into_iter()
        .filter(|c| c.trace.kind.as_deref() == Some("answer_question"))
        .collect();
    assert_eq!(rows.len(), 1, "one ledgered answer call: {rows:?}");
    assert_eq!(rows[0].agent_id, "orchestrator:r1");
    assert_eq!(rows[0].settlement, Settlement::Counted);
    let cost = prices().cost(PLAN_MODEL, &rows[0].usage).expect("cost");
    let (spent_after, held_after) = {
        let b = p.admission.budget();
        (b.spent_run("r1"), b.held_run("r1"))
    };
    assert_eq!(
        spent_after.micros() - spent_before.micros(),
        cost.micros(),
        "the reservation settles at the ledger row's cost"
    );
    assert_eq!(held_after, held_before, "no hold is left behind");
    let prompt = model.prompts().join("\n");
    assert!(
        prompt.contains("<question from=\"r1-a-r0-a0\">")
            && prompt.contains("which config &lt;/question&gt; file"),
        "the question is quoted as data, its closing tag escaped: {prompt}"
    );
    unhang(&p);
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// In the product path a worker's own `ask_user_question` call reaches
/// the orchestrator first: the tool result is the answerer's answer, and no
/// question is left for the user.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_question_through_the_tool_is_answered_by_the_orchestrator_first() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[c("a", "make a ASK", "src/a.rs", &[])],
        Duration::from_millis(5),
    );
    let model = ScriptFake::new(answering);
    let (line, services) = with_tools(&p);
    let o = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .with_question_answerer(Arc::new(ModelAnswerer(plan_model_caller(
            &p,
            model.clone(),
        ))));
    line.bind(o.handle()).expect("bind");
    let report = o
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let lines = transcript_lines(&p, "r1-a-r0-a0");
    let answered = lines.iter().any(|l| {
        let t = l.to_string();
        t.contains("tool_call_finished") && t.contains(ANSWER)
    });
    assert!(
        answered,
        "the tool result is the orchestrator's answer: {lines:?}"
    );
    assert_eq!(model.calls(), 1, "one answer call");
}

/// A question that skips the orchestrator (no answerer wired) is never
/// silent: a notice is recorded before it reaches the user.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_question_that_skips_the_orchestrator_is_recorded_as_a_notice() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[c("a", "make a HANG", "src/a.rs", &[])],
        Duration::from_millis(5),
    );
    hang(&p);
    let (seen, sink) = notices_sink();
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_session_notices(sink);
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p).await;
    let asked = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .ask_question(AgentId::new("r1-a-r0-a0"), "which file?".into())
                .await
        }
    });
    until("the question waits", || {
        !seen.lock().expect("lock").is_empty()
    })
    .await;
    let pending = handle.pending_questions().await;
    assert_eq!(pending.len(), 1, "{pending:?}");
    handle
        .answer_question(pending[0].id, "src/a.rs".into())
        .await
        .expect("answer");
    assert_eq!(asked.await.expect("join").expect("answered"), "src/a.rs");
    unhang(&p);
    handle.stop().await;
    let report = run.await.expect("join").expect("run");
    let skipped = |t: &String| t.contains("skipped the orchestrator") && t.contains("r1-a-r0-a0");
    assert!(
        seen.lock().expect("lock").iter().any(skipped),
        "{:?}",
        seen.lock().expect("lock")
    );
    assert!(report.notices.iter().any(skipped), "{:?}", report.notices);
}

/// A failed answer call records a fallback notice, and the question
/// reaches the user.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_answer_call_is_a_notice_and_the_user_is_asked() {
    for (case, fake) in [(
        "call failed",
        Fake {
            fail_answer: true,
            ..Fake::default()
        },
    )] {
        let dir = tempfile::tempdir().expect("dir");
        let p = parts(
            dir.path(),
            &[c("a", "make a HANG", "src/a.rs", &[])],
            Duration::from_millis(5),
        );
        hang(&p);
        let (seen, sink) = notices_sink();
        let fake = Arc::new(fake);
        let o = Orchestrator::new(config(), p.services(WINDOW))
            .expect("orchestrator")
            .with_question_answerer(fake.clone())
            .with_session_notices(sink);
        let handle = o.handle();
        let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
        wait_hang(&p).await;
        let _asked = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .ask_question(AgentId::new("r1-a-r0-a0"), "which file?".into())
                    .await
            }
        });
        let mut pending = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while pending.is_empty() {
            assert!(
                Instant::now() < deadline,
                "{case}: no question reached the user"
            );
            pending = handle.pending_questions().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let seen = seen.lock().expect("lock").clone();
        assert!(
            seen.iter()
                .any(|t| t.contains("skipped the orchestrator") && t.contains(case)),
            "{case}: {seen:?}"
        );
        unhang(&p);
        handle.stop().await;
        let _ = run.await.expect("join");
    }
}

/// Decision 0075: an answer model with no price entry is asked like any
/// other. Its hold is zero, the call is made, and the question does not
/// go to the user for want of a price.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unpriced_answer_model_is_still_asked() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[c("a", "make a HANG", "src/a.rs", &[])],
        Duration::from_millis(5),
    );
    hang(&p);
    let (seen, sink) = notices_sink();
    let fake = Arc::new(Fake {
        unpriced: true,
        answer: Some(ANSWER.into()),
        ..Fake::default()
    });
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(fake.clone())
        .with_session_notices(sink);
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p).await;
    let _asked = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .ask_question(AgentId::new("r1-a-r0-a0"), "which file?".into())
                .await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while fake.asked.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the answerer was never asked");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !seen
            .lock()
            .expect("lock")
            .iter()
            .any(|t| t.contains("skipped the orchestrator")),
        "an unpriced model is not a reason to skip the orchestrator"
    );
    unhang(&p);
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// An answer call that panics, or never returns, does not leave the asking
/// worker blocked: the supervisor turns it into a failed call, a notice says
/// so, and the question goes to the user.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answer_call_that_panics_or_hangs_still_sends_the_question_to_the_user() {
    for (case, fake) in [
        (
            "panicked",
            Fake {
                answer_panics: true,
                ..Fake::default()
            },
        ),
        (
            "did not finish within",
            Fake {
                answer_parks: true,
                ..Fake::default()
            },
        ),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let p = parts(
            dir.path(),
            &[c("a", "make a HANG", "src/a.rs", &[])],
            Duration::from_millis(5),
        );
        hang(&p);
        let (seen, sink) = notices_sink();
        let o = Orchestrator::new(config(), p.services(WINDOW))
            .expect("orchestrator")
            .with_question_answerer(Arc::new(fake))
            .with_session_call_timeout(Duration::from_millis(200))
            .with_session_notices(sink);
        let handle = o.handle();
        let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
        wait_hang(&p).await;
        let _asked = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .ask_question(AgentId::new("r1-a-r0-a0"), "which file?".into())
                    .await
            }
        });
        let mut pending = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while pending.is_empty() {
            assert!(
                Instant::now() < deadline,
                "{case}: no question reached the user"
            );
            pending = handle.pending_questions().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let seen = seen.lock().expect("lock").clone();
        assert!(
            seen.iter()
                .any(|t| t.contains("skipped the orchestrator") && t.contains(case)),
            "{case}: {seen:?}"
        );
        unhang(&p);
        handle.stop().await;
        let _ = run.await.expect("join");
    }
}

/// A judgment call that panics, or never returns, refuses its waiters with
/// the reason and clears: the next request is judged again rather than
/// queued behind a judgment that will never come back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_judgment_call_that_panics_or_hangs_refuses_and_clears() {
    for (case, fake) in [
        (
            "panicked",
            Fake {
                judge_panics: true,
                ..Fake::default()
            },
        ),
        (
            "did not finish within",
            Fake {
                judge_parks: true,
                ..Fake::default()
            },
        ),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let p = parts(
            dir.path(),
            &[
                c("a", "make a HANG", "src/a.rs", &[]),
                c("b", "make b HANG", "src/b.rs", &[]),
            ],
            Duration::from_millis(5),
        );
        hang(&p);
        let judge = Arc::new(fake);
        let o = Orchestrator::new(config(), p.services(WINDOW))
            .expect("orchestrator")
            .with_question_answerer(judge.clone())
            .with_session_call_timeout(Duration::from_millis(200));
        let handle = o.handle();
        let run = tokio::spawn(o.start(RunId::new("r1"), "make a and b".into()));
        until("both park", || {
            p.stats.requests().iter().filter(|r| r.2 == 1).count() >= 2
        })
        .await;
        let (a0, b0) = (AgentId::new("r1-a-r0-a0"), AgentId::new("r1-b-r0-a0"));
        for attempt in 1..=2 {
            let err = tokio::time::timeout(
                Duration::from_secs(10),
                handle.send_message(a0.clone(), b0.clone(), "hi".into()),
            )
            .await
            .unwrap_or_else(|_| panic!("{case}: judgment {attempt} never came back"))
            .expect_err("a failed judgment is a refusal");
            assert!(
                matches!(&err, SessionError::Refused(r) if r.contains(case)),
                "{case}: {err:?}"
            );
        }
        assert_eq!(
            judge.judged.lock().expect("lock").len(),
            2,
            "{case}: the second request was judged again"
        );
        unhang(&p);
        handle.stop().await;
        let _ = run.await.expect("join");
    }
}

// ------------------------------------------------------------------

/// A message to a finished agent is refused as retired, with the offer to
/// send it to the orchestrator, before anything is judged
/// or persisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_to_a_finished_agent_is_refused_and_nothing_is_persisted() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[
            c("a", "make a", "src/a.rs", &[]),
            c("b", "make b HANG", "src/b.rs", &[]),
        ],
        Duration::from_millis(5),
    );
    hang(&p);
    let judge = Arc::new(Fake::default());
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(judge.clone());
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a and b".into()));
    until("a verifies", || {
        try_record_of(&p).is_some_and(|r| r.is_verified("a"))
    })
    .await;
    let a0 = AgentId::new("r1-a-r0-a0");
    let err = handle
        .send_message(AgentId::new("r1-b-r0-a0"), a0.clone(), "too late".into())
        .await
        .expect_err("a finished agent takes no message");
    assert!(matches!(err, SessionError::Retired { .. }), "{err:?}");
    assert!(err.to_string().contains("orchestrator"), "{err}");
    let inbox = sessions(&p).undelivered(&a0).expect("undelivered");
    assert!(inbox.is_empty(), "nothing is persisted for it: {inbox:?}");
    assert!(
        judge.judged.lock().expect("lock").is_empty(),
        "nothing is judged for it"
    );
    unhang(&p);
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// An unknown recipient is refused as unknown, and no session row is
/// invented for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_to_an_unknown_agent_is_refused_as_unknown() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[c("a", "make a HANG", "src/a.rs", &[])],
        Duration::from_millis(5),
    );
    hang(&p);
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(Arc::new(Fake::default()));
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p).await;
    let ghost = AgentId::new("no-such-agent");
    let err = handle
        .send_message(AgentId::new("r1-a-r0-a0"), ghost.clone(), "hello".into())
        .await
        .expect_err("an unknown agent takes no message");
    assert_eq!(err, SessionError::Unknown(ghost.to_string()));
    assert_eq!(sessions(&p).session(&ghost).expect("read"), None);
    let err = handle
        .request_connection(AgentId::new("r1-a-r0-a0"), ghost.clone(), "why".into())
        .await
        .expect_err("nor a connection");
    assert_eq!(err, SessionError::Unknown(ghost.to_string()));
    unhang(&p);
    handle.stop().await;
    let _ = run.await.expect("join");
}

// ------------------------------------------------------------------

/// A judgment with no verdict for a pair refuses that pair with a reason,
/// and the loop keeps running (it used to panic on `expect`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_judge_that_returns_no_verdict_refuses_the_pair_and_the_loop_lives() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[
            c("a", "make a HANG", "src/a.rs", &[]),
            c("b", "make b HANG", "src/b.rs", &[]),
        ],
        Duration::from_millis(5),
    );
    hang(&p);
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(Arc::new(Fake {
            no_verdicts: true,
            ..Fake::default()
        }));
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a and b".into()));
    until("both park", || {
        p.stats.requests().iter().filter(|r| r.2 == 1).count() >= 2
    })
    .await;
    let (a0, b0) = (AgentId::new("r1-a-r0-a0"), AgentId::new("r1-b-r0-a0"));
    let err = handle
        .send_message(a0.clone(), b0.clone(), "hi".into())
        .await
        .expect_err("no verdict is a refusal");
    assert!(
        matches!(&err, SessionError::Refused(r) if r.contains("no verdict")),
        "{err:?}"
    );
    assert!(handle.status().await.is_some(), "the loop is alive");
    let state = sessions(&p)
        .connections_of(&a0)
        .expect("connections")
        .into_iter()
        .find(|c| c.b == b0 || c.a == b0)
        .map(|c| c.state);
    assert!(
        matches!(state, Some(ConnectionState::Refused { .. })),
        "{state:?}"
    );
    unhang(&p);
    handle.stop().await;
    let report = run.await.expect("join").expect("the run ends normally");
    assert_eq!(report.status, RunStatus::Stopped);
}

// ------------------------------------------------------------------

/// An answer returned as the `ask_user_question` tool result is not
/// injected again when the session resumes: ask, stop, resume, and the
/// answer is in the transcript once, as the tool result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answer_returned_as_a_tool_result_is_not_injected_again_on_resume() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[c("a", "make a ASK HANG", "src/a.rs", &[])],
        Duration::from_millis(5),
    );
    hang(&p);
    let (line, services) = with_tools(&p);
    let o = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .with_question_answerer(Arc::new(Fake {
            answer: Some(ANSWER.into()),
            ..Fake::default()
        }));
    line.bind(o.handle()).expect("bind");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p).await;
    let worker = AgentId::new("r1-a-r0-a0");
    handle.stop_session(worker.clone()).await.expect("stop");
    until("the stopped attempt's parked call is dropped", || {
        p.alpha.live.lock().expect("lock").is_empty()
            && p.beta.live.lock().expect("lock").is_empty()
    })
    .await;
    unhang(&p);
    handle.resume_session(worker.clone()).await.expect("resume");
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let lines = transcript_lines(&p, worker.as_str());
    let with_answer: Vec<_> = lines
        .iter()
        .filter(|l| l.to_string().contains(ANSWER))
        .collect();
    assert_eq!(
        with_answer.len(),
        1,
        "the answer is in the transcript once, as the tool result: {with_answer:?}"
    );
    assert!(
        injected(&lines).is_empty(),
        "nothing was injected: {:?}",
        injected(&lines)
    );
}

// ------------------------------------------------------------------

/// An unanswered question times out with the recorded no-answer text,
/// and its contract shows "awaiting an answer" while it waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unanswered_question_times_out_with_the_recorded_no_answer_text() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[c("a", "make a HANG", "src/a.rs", &[])],
        Duration::from_millis(5),
    );
    hang(&p);
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(Arc::new(Fake::default()))
        .with_question_timeout(Duration::from_millis(400));
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p).await;
    let worker = AgentId::new("r1-a-r0-a0");
    let asked = tokio::spawn({
        let handle = handle.clone();
        let worker = worker.clone();
        async move { handle.ask_question(worker, "which file?".into()).await }
    });
    let awaiting =
        |p: &Parts| try_record_of(p).and_then(|r| r.coordination.waits.get("a").map(|w| w.kind()));
    until("the contract shows awaiting an answer", || {
        awaiting(&p) == Some(WaitKind::AwaitingAnswer)
    })
    .await;
    let text = asked.await.expect("join").expect("a result, not an error");
    assert!(text.contains("State the assumption"), "{text}");
    let recorded = sessions(&p).undelivered(&worker).expect("inbox");
    assert!(
        recorded
            .iter()
            .any(|i| i.kind == InboxKind::Answer && i.text == text),
        "the no-answer result is recorded: {recorded:?}"
    );
    until("the wait returns to running", || {
        awaiting(&p) == Some(WaitKind::Running)
    })
    .await;
    assert!(handle.pending_questions().await.is_empty());
    unhang(&p);
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// A run known to be unattended answers a question the orchestrator cannot
/// answer at once, not after the 60 s unattended timeout, and records the
/// answer; no question stays on the desk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_known_unattended_run_answers_a_question_at_once() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[c("a", "make a HANG", "src/a.rs", &[])],
        Duration::from_millis(5),
    );
    hang(&p);
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(Arc::new(Fake::default()))
        .with_question_attendance(QuestionAttendance::Unattended);
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p).await;
    let worker = AgentId::new("r1-a-r0-a0");
    let started = Instant::now();
    let text = tokio::time::timeout(
        Duration::from_secs(20),
        handle.ask_question(worker.clone(), "which file?".into()),
    )
    .await
    .expect("answered well before the 60 s timeout")
    .expect("a result, not an error");
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(text.contains("Nobody is attached"), "{text}");
    assert!(text.contains("State the assumption"), "{text}");
    let recorded = sessions(&p).undelivered(&worker).expect("inbox");
    assert!(
        recorded
            .iter()
            .any(|i| i.kind == InboxKind::Answer && i.text == text),
        "the answer is recorded: {recorded:?}"
    );
    assert!(handle.pending_questions().await.is_empty());
    unhang(&p);
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// The attendance rule never waits forever.
#[test]
fn a_box_run_waits_the_box_timeout_and_any_other_the_scheduled_one() {
    assert_eq!(
        QuestionAttendance::Box.timeout(),
        kgraf_orchestrator::DEFAULT_ASK_TIMEOUT
    );
    assert_eq!(
        QuestionAttendance::Unattended.timeout(),
        kgraf_orchestrator::SCHEDULED_ASK_TIMEOUT
    );
}

// ------------------------------------------------------------------

/// A paced message is delivered by the loop's re-check once it
/// comes due, not only when it was sent; the recipient's contract shows
/// "delivery paced" meanwhile and goes back to running after. The clock is
/// moved by hand; the recipient is slow enough to reach a boundary after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paced_message_is_delivered_by_the_recheck_when_it_comes_due() {
    let dir = tempfile::tempdir().expect("dir");
    let mut p = parts(
        dir.path(),
        &[C {
            id: "a",
            instruction: "make a HANG",
            path: "src/a.rs",
            deps: &[],
            attempts: 2,
        }],
        Duration::from_millis(5),
    );
    // The attempt on alpha parks and sends; the one on beta receives and
    // reaches a boundary after a slow round 0.
    p.beta = WorkerFake::new("beta", p.stats.clone(), Duration::from_millis(1_500));
    p.alpha.hang.store(true, Ordering::SeqCst);
    let clock = Arc::new(ManualClock::new(SystemClock.now_ms()));
    let mut services = p.services(WINDOW);
    services.clock = clock.clone();
    let o = Orchestrator::new(config(), services).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until("alpha parks", || {
        p.alpha.hang_reached.load(Ordering::SeqCst)
    })
    .await;
    let contract = record_of(&p)
        .open_contracts
        .into_iter()
        .find(|c| c.contract == "a")
        .expect("a");
    let on = |route: &str| {
        let i = contract
            .worker_routes
            .iter()
            .position(|r| r == route)
            .expect("route");
        contract.workers[i].clone()
    };
    let (sender, receiver) = (on("alpha"), on("beta"));
    for i in 0..13 {
        handle
            .send_message(sender.clone(), receiver.clone(), format!("message {i}"))
            .await
            .expect("never refused for volume");
    }
    let wait_of =
        |p: &Parts| try_record_of(p).and_then(|r| r.coordination.waits.get("a").map(|w| w.kind()));
    until("the contract shows delivery paced", || {
        wait_of(&p) == Some(WaitKind::PacedDelivery)
    })
    .await;
    clock.advance(6_000);
    until("the wait clears once the paced row is injected", || {
        wait_of(&p) == Some(WaitKind::Running)
    })
    .await;
    // Parallel attempts run to their end: the parked sender fails, and the
    // receiver's attempt is selected.
    p.alpha.fail_parked.store(true, Ordering::SeqCst);
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let delivered = injected(&transcript_lines(&p, receiver.as_str())).len();
    assert_eq!(
        delivered, 13,
        "all 13 reached the receiver, the paced one too"
    );
    assert!(
        sessions(&p)
            .undelivered(&receiver)
            .expect("inbox")
            .iter()
            .all(|i| i.kind != InboxKind::Message),
        "every message is marked delivered"
    );
}

/// A message sent after an earlier paced one came due, and before the
/// loop's re-check delivered it, does not overtake it: both reach the
/// recipient in the order they were sent. The loop's tick is long, so the
/// only wake between the two is the second message itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_sent_after_a_paced_one_came_due_does_not_overtake_it() {
    let dir = tempfile::tempdir().expect("dir");
    let mut p = parts(
        dir.path(),
        &[C {
            id: "a",
            instruction: "make a HANG",
            path: "src/a.rs",
            deps: &[],
            attempts: 2,
        }],
        Duration::from_millis(5),
    );
    p.beta = WorkerFake::new("beta", p.stats.clone(), Duration::from_millis(1_500));
    p.alpha.hang.store(true, Ordering::SeqCst);
    let clock = Arc::new(ManualClock::new(SystemClock.now_ms()));
    let mut services = p.services(WINDOW);
    services.clock = clock.clone();
    let mut cfg = config();
    cfg.tick = Duration::from_secs(30);
    let o = Orchestrator::new(cfg, services).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until("alpha parks", || {
        p.alpha.hang_reached.load(Ordering::SeqCst)
    })
    .await;
    let contract = record_of(&p)
        .open_contracts
        .into_iter()
        .find(|c| c.contract == "a")
        .expect("a");
    let on = |route: &str| {
        let i = contract
            .worker_routes
            .iter()
            .position(|r| r == route)
            .expect("route");
        contract.workers[i].clone()
    };
    let (sender, receiver) = (on("alpha"), on("beta"));
    for i in 0..13 {
        handle
            .send_message(sender.clone(), receiver.clone(), format!("message {i}"))
            .await
            .expect("never refused for volume");
    }
    // The last send's reply comes before the loop's re-check of that
    // iteration: let the loop finish it and sleep on its long tick, so the
    // clock moves while nothing re-checks.
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Past the pacing window: "message 12" is due, and the next one is not
    // paced at all.
    clock.advance(61_000);
    handle
        .send_message(sender.clone(), receiver.clone(), "message 13".into())
        .await
        .expect("sent");
    p.alpha.fail_parked.store(true, Ordering::SeqCst);
    let report = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("the run ended in time")
        .expect("join")
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let lines = transcript_lines(&p, receiver.as_str());
    let order: Vec<String> = injected(&lines)
        .iter()
        .map(|l| l.to_string())
        .filter_map(|l| {
            (0..14)
                .rev()
                .find(|i| l.contains(&format!("message {i}")))
                .map(|i| i.to_string())
        })
        .collect();
    let at = |n: &str| order.iter().position(|x| x == n);
    assert!(
        at("12").is_some() && at("13").is_some() && at("12") < at("13"),
        "sent order kept: {order:?}"
    );
}

// ------------------------------------------------------------------

/// The run stays alive on a user stop, and a stop
/// never ends as `Blocked`. The restart case is in `sessions_restart.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_user_stop_keeps_the_loop_alive_for_resume_session() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[c("a", "make a HANG", "src/a.rs", &[])],
        Duration::from_millis(5),
    );
    hang(&p);
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p).await;
    handle
        .stop_session(AgentId::new("r1-a-r0-a0"))
        .await
        .expect("stop");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!run.is_finished(), "the run waits for resume_session");
    assert!(handle.status().await.is_some());
    handle.stop().await;
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Stopped);
}

// ------------------------------------------------------------------

/// A message to an agent that retires is reported to the sending
/// agent by injection: the report is a `Notice` row, and the re-check (or the
/// direct injection) delivers it into the sender's transcript.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_to_an_agent_that_retires_is_reported_to_the_sender_by_injection() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = routed_plan(&[
        ("x", "make x HANG", "src/x.rs", "alpha"),
        ("y", "make y", "src/y.rs", "beta"),
    ]);
    let mut p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.admission = admission(Limits::new(32, 64).expect("limits"));
    // x parks on alpha; y runs on beta with a slow round 0, so it is between
    // boundaries while x fails and the report reaches it.
    p.beta = WorkerFake::new("beta", p.stats.clone(), Duration::from_millis(1_500));
    p.alpha.hang.store(true, Ordering::SeqCst);
    let mut cfg = config();
    cfg.max_retries = 0;
    let o = Orchestrator::new(cfg, p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(Arc::new(Fake::default()));
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make x and y".into()));
    until("x parks", || p.alpha.hang_reached.load(Ordering::SeqCst)).await;
    let record = record_of(&p);
    let worker_of = |id: &str| {
        record
            .open_contracts
            .iter()
            .find(|c| c.contract == id)
            .and_then(|c| c.workers.first().cloned())
            .expect("dispatched")
    };
    let (x, y) = (worker_of("x"), worker_of("y"));
    handle
        .send_message(y.clone(), x.clone(), "are you using Config?".into())
        .await
        .expect("judged open, persisted");
    // x's parked call fails: its session retires Failed with the message
    // still undelivered.
    p.alpha.fail_parked.store(true, Ordering::SeqCst);
    let report = run.await.expect("join").expect("run");
    assert!(
        matches!(report.status, RunStatus::Blocked { .. }),
        "x is held after its one attempt: {report:?}"
    );
    let (ended, _) = sessions(&p).lifecycle(&x).expect("read").expect("x");
    assert_eq!(ended.map(|(o, _)| o), Some(SessionOutcome::Failed));
    let reports: Vec<_> = injected(&transcript_lines(&p, y.as_str()))
        .into_iter()
        .filter(|l| l.to_string().contains("not delivered"))
        .cloned()
        .collect();
    assert_eq!(
        reports.len(),
        1,
        "the sender got the report once, in its transcript: {reports:?}"
    );
    assert!(reports[0].to_string().contains(r#""kind":"notice""#));
}

/// A session that can resume is never reported undelivered; its note
/// waits, and the resume delivers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_to_a_stopped_session_is_not_reported_and_is_delivered_on_resume() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[c("a", "make a HANG", "src/a.rs", &[])],
        Duration::from_millis(5),
    );
    hang(&p);
    let (seen, sink) = notices_sink();
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_session_notices(sink);
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_hang(&p).await;
    let worker = AgentId::new("r1-a-r0-a0");
    handle.stop_session(worker.clone()).await.expect("stop");
    until("the stopped attempt ends", || {
        p.alpha.live.lock().expect("lock").is_empty()
            && p.beta.live.lock().expect("lock").is_empty()
    })
    .await;
    let id = handle
        .send_to_session(
            worker.clone(),
            InboxKind::Note,
            InjectMode::AtBoundary,
            "for after the resume".into(),
        )
        .await
        .expect("a stopped session takes a note");
    unhang(&p);
    handle.resume_session(worker.clone()).await.expect("resume");
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(
        !seen
            .lock()
            .expect("lock")
            .iter()
            .any(|t| t.contains("not delivered")),
        "{:?}",
        seen.lock().expect("lock")
    );
    let lines = transcript_lines(&p, worker.as_str());
    let delivered: Vec<_> = injected(&lines)
        .into_iter()
        .filter(|l| l.to_string().contains(&id))
        .collect();
    assert_eq!(
        delivered.len(),
        1,
        "delivered once on resume: {delivered:?}"
    );
}

// ------------------------------------------------------------------

/// Connection requests that arrive together are judged in one call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_that_arrive_together_are_judged_in_one_call() {
    let dir = tempfile::tempdir().expect("dir");
    let ids = ["s", "b", "c", "d", "e", "f"];
    let paths: Vec<String> = ids.iter().map(|i| format!("src/{i}.rs")).collect();
    let contracts: Vec<C<'_>> = ids
        .iter()
        .zip(&paths)
        .map(|(id, path)| c(id, "make it HANG", path, &[]))
        .collect();
    let p = parts(dir.path(), &contracts, Duration::from_millis(5));
    hang(&p);
    let judge = Arc::new(Fake::default());
    let mut cfg = config();
    // One tick is the batching window; a long one makes "together" certain.
    cfg.tick = Duration::from_millis(300);
    let o = Orchestrator::new(cfg, p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(judge.clone());
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make them".into()));
    until("all park", || {
        p.stats.requests().iter().filter(|r| r.2 == 1).count() >= ids.len()
    })
    .await;
    let from = AgentId::new("r1-s-r0-a0");
    let sends: Vec<_> = ids[1..]
        .iter()
        .map(|id| {
            let handle = handle.clone();
            let from = from.clone();
            let to = AgentId::new(format!("r1-{id}-r0-a0"));
            tokio::spawn(async move {
                handle
                    .request_connection(from, to, "shared type".into())
                    .await
            })
        })
        .collect();
    for s in sends {
        s.await.expect("join").expect("approved");
    }
    assert_eq!(
        *judge.judged.lock().expect("lock"),
        vec![5],
        "five requests, one call"
    );
    unhang(&p);
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// While a judgment call is pending, the loop still answers a stop and a
/// status read at once, and the run ends; the waiting sender's call ends too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_is_handled_while_a_judgment_call_is_pending() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[
            c("a", "make a HANG", "src/a.rs", &[]),
            c("b", "make b HANG", "src/b.rs", &[]),
        ],
        Duration::from_millis(5),
    );
    hang(&p);
    let judge = Arc::new(Fake {
        judge_parks: true,
        ..Fake::default()
    });
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(judge.clone());
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a and b".into()));
    until("both park", || {
        p.stats.requests().iter().filter(|r| r.2 == 1).count() >= 2
    })
    .await;
    let pending = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .send_message(
                    AgentId::new("r1-a-r0-a0"),
                    AgentId::new("r1-b-r0-a0"),
                    "hi".into(),
                )
                .await
        }
    });
    until("the judgment call is in flight", || {
        !judge.judged.lock().expect("lock").is_empty()
    })
    .await;
    let quick = Duration::from_secs(2);
    tokio::time::timeout(quick, handle.status())
        .await
        .expect("a status read is not held behind the judgment")
        .expect("status");
    tokio::time::timeout(quick, handle.stop_session(AgentId::new("r1-b-r0-a0")))
        .await
        .expect("a session stop is not held behind the judgment")
        .expect("stopped");
    handle.stop().await;
    let report = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("the run ends while the judgment is still pending")
        .expect("join")
        .expect("run");
    assert_eq!(report.status, RunStatus::Stopped);
    let sent = pending.await.expect("join");
    assert_eq!(sent, Err(SessionError::Stopped), "the sender's call ends");
}

// ------------------------------------------------------------------

/// A refused `request_connection` tells the sender why, records the
/// refusal on the pair, and a second request is refused from that record
/// without a second paid call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_connection_request_tells_the_sender_and_is_not_judged_again() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[
            c("a", "make a HANG", "src/a.rs", &[]),
            c("b", "make b HANG", "src/b.rs", &[]),
        ],
        Duration::from_millis(5),
    );
    hang(&p);
    let judge = Arc::new(Fake {
        open_if: Some(|_| false),
        ..Fake::default()
    });
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(judge.clone());
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a and b".into()));
    until("both park", || {
        p.stats.requests().iter().filter(|r| r.2 == 1).count() >= 2
    })
    .await;
    let (a0, b0) = (AgentId::new("r1-a-r0-a0"), AgentId::new("r1-b-r0-a0"));
    for _ in 0..2 {
        let err = handle
            .request_connection(a0.clone(), b0.clone(), "we share a type".into())
            .await
            .expect_err("refused");
        assert_eq!(err, SessionError::Refused("refused for the test".into()));
    }
    let err = handle
        .send_message(a0.clone(), b0.clone(), "hi".into())
        .await
        .expect_err("a refused pair carries no message");
    assert_eq!(err, SessionError::Refused("refused for the test".into()));
    assert_eq!(
        judge.judged.lock().expect("lock").len(),
        1,
        "the refusal is judged once for the run"
    );
    let conn = sessions(&p)
        .connections_of(&a0)
        .expect("connections")
        .into_iter()
        .find(|c| c.a == b0 || c.b == b0)
        .expect("the pair");
    assert_eq!(
        conn.state,
        ConnectionState::Refused {
            reason: "refused for the test".into()
        }
    );
    let inbox = sessions(&p).undelivered(&b0).expect("inbox");
    assert!(
        inbox
            .iter()
            .all(|i| i.source != InboxSource::Agent { id: a0.clone() })
    );
    unhang(&p);
    handle.stop().await;
    let _ = run.await.expect("join");
}

// ------------------------------------------------------------------

/// Stopping one of two attempts frees exactly that attempt's agent slot
/// (`AdmissionGate::release_one`) and its budget reservation once its
/// `Finished` is processed, while the sibling keeps its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_one_attempt_frees_its_reservation() {
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[C {
            id: "a",
            instruction: "make a HANG",
            path: "src/a.rs",
            deps: &[],
            attempts: 2,
        }],
        Duration::from_millis(5),
    );
    hang(&p);
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until("both park", || {
        p.alpha.hang_reached.load(Ordering::SeqCst) && p.beta.hang_reached.load(Ordering::SeqCst)
    })
    .await;
    let held_both = p.admission.budget().held_run("r1");
    assert!(held_both.micros() > 0, "two attempts, two holds");
    let (items, agents) = p.admission.gate().in_flight();
    assert_eq!(agents, 2, "two attempts, two agent slots");
    handle
        .stop_session(AgentId::new("r1-a-r0-a0"))
        .await
        .expect("stop");
    until("the stopped attempt's hold and slot are freed", || {
        p.admission.budget().held_run("r1") < held_both && p.admission.gate().in_flight().1 < 2
    })
    .await;
    let held_one = p.admission.budget().held_run("r1");
    assert_eq!(
        held_one.micros() * 2,
        held_both.micros(),
        "exactly one of the two equal holds is freed; the sibling keeps its own"
    );
    assert_eq!(
        p.admission.gate().in_flight(),
        (items, 1),
        "exactly one agent slot is freed; the work item stays with the sibling"
    );
    unhang(&p);
    handle.stop().await;
    let _ = run.await.expect("join");
}

// ------------------------------------------------------------------ 0071

/// Decision 0071's two-copy rule on the session paths: a pattern-shaped
/// token (not registered, so the model may see it) in a worker's question, a
/// direct note, a message and the orchestrator's answer is scrubbed in every
/// stored inbox row and in the transcript, while the model (the answerer's
/// prompt, the worker's tool result, the recipient's injection) reads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_texts_are_stored_scrubbed_and_read_by_the_model_whole() {
    let token = format!("ghp_{}", "Sq1Wr2Et3Yu4Io5Pa6Sd7Fg8Hj9Kl0Zx1Cv2");
    let secret = token.as_str();
    let dir = tempfile::tempdir().expect("dir");
    let p = parts(
        dir.path(),
        &[C {
            id: "a",
            instruction: "make a",
            path: "src/a.rs",
            deps: &[],
            attempts: 2,
        }],
        Duration::from_millis(400),
    );
    let in_reply = token.clone();
    let model = ScriptFake::new(move |prompt: &str| {
        let secret = in_reply.as_str();
        if prompt.contains("The worker's question") {
            format!(r#"{{"answerable":true,"answer":"use {secret}"}}"#)
        } else {
            r#"{"verdicts":[]}"#.into()
        }
    });
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(Arc::new(ModelAnswerer(plan_model_caller(
            &p,
            model.clone(),
        ))));
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until("round 0 runs", || p.stats.requests().len() >= 2).await;
    let (a0, a1) = (AgentId::new("r1-a-r0-a0"), AgentId::new("r1-a-r0-a1"));
    let answer = handle
        .ask_question(a0.clone(), format!("is the key {secret}?"))
        .await
        .expect("answered");
    assert_eq!(
        answer,
        format!("use {secret}"),
        "the worker reads the whole answer"
    );
    assert!(
        model.prompts().join("\n").contains(secret),
        "the model reads the whole question"
    );
    handle
        .send_to_session(
            a1.clone(),
            InboxKind::Note,
            InjectMode::AtBoundary,
            format!("note {secret}"),
        )
        .await
        .expect("note");
    handle
        .send_message(a0.clone(), a1.clone(), format!("message {secret}"))
        .await
        .expect("siblings");
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let db = rusqlite::Connection::open(dir.path().join("graph.db")).expect("db");
    let texts: Vec<String> = db
        .prepare("SELECT text FROM session_inbox")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert!(texts.len() >= 3, "{texts:?}");
    assert!(
        texts.iter().all(|t| !t.contains(secret)),
        "every stored row is scrubbed: {texts:?}"
    );
    let transcript = transcript_lines(&p, a1.as_str());
    let reached = injected(&transcript);
    assert_eq!(
        reached.len(),
        2,
        "the note and the message reached the recipient: {reached:?}"
    );
    assert!(
        reached.iter().all(|l| !l.to_string().contains(secret)),
        "its transcript is scrubbed: {reached:?}"
    );
}

/// A rule that comes to cover a pair later (here a
/// shared read, observed once `z` verifies) never reverses the
/// orchestrator's paid refusal of it: the refusal, its reason and its
/// decider survive `refresh_rule_connections`, a message is still refused,
/// and the collision is one notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rule_that_covers_a_refused_pair_later_does_not_reverse_the_refusal() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = routed_plan(&[
        ("x", "make x READ src/shared.rs HANG", "src/x.rs", "alpha"),
        ("y", "make y READ src/shared.rs HANG", "src/y.rs", "alpha"),
        ("z", "make z", "src/z.rs", "beta"),
    ]);
    let mut p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    p.admission = admission(Limits::new(32, 64).expect("limits"));
    p.beta = WorkerFake::new("beta", p.stats.clone(), Duration::from_millis(1_000));
    p.alpha.hang.store(true, Ordering::SeqCst);
    let (seen, sink) = notices_sink();
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_question_answerer(Arc::new(Fake {
            open_if: Some(|_| false),
            ..Fake::default()
        }))
        .with_session_notices(sink);
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make x, y, z".into()));
    until("x and y park", || {
        p.stats
            .requests()
            .iter()
            .filter(|r| r.0 == "alpha" && r.2 == 1)
            .count()
            >= 2
    })
    .await;
    let (x0, y0) = (AgentId::new("r1-x-r0-a0"), AgentId::new("r1-y-r0-a0"));
    let err = handle
        .request_connection(x0.clone(), y0.clone(), "we read one file".into())
        .await
        .expect_err("the judge refuses");
    assert_eq!(err, SessionError::Refused("refused for the test".into()));
    until("z verifies, so x and y now share a read", || {
        try_record_of(&p).is_some_and(|r| {
            r.is_verified("z")
                && r.coordination.subscriptions.contains_key("x")
                && r.coordination.subscriptions.contains_key("y")
        })
    })
    .await;
    for _ in 0..2 {
        let err = handle
            .send_message(x0.clone(), y0.clone(), "hi".into())
            .await
            .expect_err("the refusal stands");
        assert_eq!(err, SessionError::Refused("refused for the test".into()));
    }
    let conn = sessions(&p)
        .connections_of(&x0)
        .expect("connections")
        .into_iter()
        .find(|c| c.a == y0 || c.b == y0)
        .expect("the pair");
    assert_eq!(
        conn.state,
        ConnectionState::Refused {
            reason: "refused for the test".into()
        }
    );
    assert_eq!(
        conn.decided_by,
        Some(kgraf_core::session::Decider::Orchestrator)
    );
    let collisions = seen
        .lock()
        .expect("lock")
        .iter()
        .filter(|t| t.contains("earlier decision stands"))
        .count();
    assert_eq!(collisions, 1, "{:?}", seen.lock().expect("lock"));
    handle.stop().await;
    let _ = run.await.expect("join");
}
