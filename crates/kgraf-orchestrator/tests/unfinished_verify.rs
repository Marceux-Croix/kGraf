//! A worker stopped before it finished (stalled, or out of rounds) may still
//! have delivered: its tree goes to the contract's verification as it stands
//! (`verify::verifies_unfinished`). The worker is a real worker loop over a
//! scripted model that writes the contract's file in its first round and
//! then re-reads it, or reads other files, until the runtime ends it.

mod support;

use kgraf_core::model_profile::{ProfileStore, Source, TaskKind};
use kgraf_core::{Protocol, RunId, TokenUsage};
use kgraf_orchestrator::{
    DirWorkspaces, ModelCaller, ModelDecomposer, ModelGrounder, OpenContract, Orchestrator,
    RunStatus, Services, SystemClock, TraceEvent, TraceLog, Verdict, VerifiedCommit, Verifier,
};
use kgraf_provider::{CallToken, Provider, ProviderError, Request, Response, Role, StopReason};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// What the scripted worker does after its first-round write.
#[derive(Clone, Copy)]
enum After {
    /// Re-read the file it wrote, identically, every round: a stall.
    RereadSame,
    /// Read a different file every round: no repeat, so the step budget ends it.
    ReadOthers,
}

/// Writes `src/b.rs` with `content` in round one, then loops per `after`.
struct DeliverThenLoop {
    content: &'static str,
    after: After,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for DeliverThenLoop {
    fn adapter(&self) -> &'static str {
        "fake-deliver-then-loop"
    }

    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let round = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        let usage = TokenUsage {
            uncached_input: 100,
            text_output: 10,
            ..TokenUsage::default()
        };
        token.settle(&usage, None)?;
        let (name, input) = match (round, self.after) {
            (0, _) => (
                "write_file",
                json!({"path": "src/b.rs", "content": self.content}),
            ),
            (_, After::RereadSame) => ("read_file", json!({"path": "src/b.rs"})),
            (n, After::ReadOthers) => ("read_file", json!({"path": format!("src/other{n}.rs")})),
        };
        Ok(Response {
            finish_reason: None,
            model: request.model.clone(),
            content: vec![kgraf_provider::Content::ToolUse {
                id: format!("t{round}"),
                name: name.into(),
                input,
                signature: None,
            }],
            stop_reason: StopReason::ToolUse,
            usage,
            request_id: None,
        })
    }
}

struct Outcome {
    status: RunStatus,
    notices: Vec<String>,
    trace: Vec<TraceEvent>,
    verifications: usize,
    tally: Option<(u64, u64)>,
}

/// One run of a one-contract plan whose worker is `worker`, with the
/// verifier requiring `require` in the file when set.
async fn run(worker: Arc<DeliverThenLoop>, require: Option<&str>) -> Outcome {
    run_with(worker, require, None).await
}

/// A verifier whose check is `true`: it passes any tree.
struct Vacuous;

#[async_trait::async_trait]
impl Verifier for Vacuous {
    async fn verify(
        &self,
        _contract: &OpenContract,
        _workspace: &std::path::Path,
        _result: &kgraf_runtime::WorkerResult,
    ) -> Result<Verdict, String> {
        Ok(Verdict::Pass {
            by: "true".into(),
            commit: None,
            notices: Vec::new(),
        })
    }

    async fn recheck(&self, _commit: &VerifiedCommit) -> Result<bool, String> {
        Ok(true)
    }
}

/// A verifier that cannot run.
struct Down;

#[async_trait::async_trait]
impl Verifier for Down {
    async fn verify(
        &self,
        _contract: &OpenContract,
        _workspace: &std::path::Path,
        _result: &kgraf_runtime::WorkerResult,
    ) -> Result<Verdict, String> {
        Err("the sandbox is gone".into())
    }

    async fn recheck(&self, _commit: &VerifiedCommit) -> Result<bool, String> {
        Ok(false)
    }
}

/// [`run`] with `verifier` in place of the harness's file verifier when set.
async fn run_with(
    worker: Arc<DeliverThenLoop>,
    require: Option<&str>,
    verifier: Option<Arc<dyn Verifier>>,
) -> Outcome {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("profiles")).expect("profiles dir");
    let plan = json!({"contracts":[
        {"id":"b","instruction":"write the file WRITE src/b.rs","acceptance":"src/b.rs exists",
         "writes":[{"path":"src/b.rs"}],"tools":["write_file","read_file"],"route":"alpha",
         "attempts":1}
    ]})
    .to_string();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    *p.verifier.require.lock().expect("lock") = require.map(str::to_owned);
    let caller = |provider: Arc<ScriptFake>| ModelCaller {
        reasoning: Default::default(),
        provider,
        ledger: p.ledger.clone(),
        model: PLAN_MODEL.into(),
        max_output_tokens: 2_000,
        agent: "orchestrator:r1".into(),
        task: "r1".into(),
    };
    let services = Services {
        graph: p.graph.clone(),
        ledger: p.ledger.clone(),
        prices: prices(),
        routes: vec![
            route_with(
                "alpha",
                "vendor-a",
                ALPHA_MODEL,
                worker.clone(),
                p.ledger.clone(),
                WINDOW,
                &[],
            ),
            route_with(
                "beta",
                "vendor-b",
                BETA_MODEL,
                worker.clone(),
                p.ledger.clone(),
                WINDOW,
                &[],
            ),
        ],
        decomposer: Arc::new(ModelDecomposer(caller(p.planner.clone()))),
        grounder: Arc::new(ModelGrounder(caller(p.grounder.clone()))),
        verifier: verifier.unwrap_or_else(|| p.verifier.clone()),
        workspaces: Arc::new(DirWorkspaces {
            base: p.dir.join("worktrees"),
        }),
        clock: Arc::new(SystemClock),
        admission: p.admission.clone(),
        runs_dir: p.dir.join("runs"),
        profiles: Arc::new(Mutex::new(ProfileStore::default())),
        profiles_dir: p.dir.join("profiles"),
    };
    let mut cfg = config();
    cfg.max_retries = 0;
    let report = Orchestrator::new(cfg, services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "write the file".into())
        .await
        .expect("run");
    let trace = TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let tally = ProfileStore::load(&p.dir.join("profiles"))
        .expect("load")
        .get(ALPHA_MODEL)
        .and_then(|m| m.tally(Source::Live, TaskKind::Implementation))
        .map(|t| (t.attempts, t.satisfied));
    Outcome {
        status: report.status,
        notices: report.notices,
        trace,
        verifications: p.verifier.started.load(Ordering::SeqCst),
        tally,
    }
}

fn outcomes(trace: &[TraceEvent]) -> Vec<String> {
    trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::WorkerFinished { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .collect()
}

fn verdicts(trace: &[TraceEvent]) -> Vec<bool> {
    trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Verdict { passed, .. } => Some(*passed),
            _ => None,
        })
        .collect()
}

/// The shape that was discarded before the rule: a passing delivery, then a loop of
/// identical reads until the stall check ends the worker. The tree passes,
/// so the contract is verified, with a notice, and the route is credited.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_stalled_after_a_passing_delivery_is_verified_and_accepted() {
    let worker = Arc::new(DeliverThenLoop {
        content: "pub fn b() {}",
        after: After::RereadSame,
        calls: AtomicUsize::new(0),
    });
    let out = run(worker.clone(), None).await;
    assert_eq!(outcomes(&out.trace), vec!["stalled".to_owned()]);
    assert_eq!(out.status, RunStatus::Completed, "{:?}", out.notices);
    assert_eq!(verdicts(&out.trace), vec![true]);
    assert_eq!(out.verifications, 1, "one verify run for the one attempt");
    // Write, then three identical reads: the third is refused as the stall.
    assert_eq!(worker.calls.load(Ordering::SeqCst), 4);
    assert!(
        out.notices
            .iter()
            .any(|n| n.contains("stopped before it finished")
                && n.contains("stalled")
                && n.contains("accepted as verified")),
        "{:?}",
        out.notices
    );
    assert_eq!(out.tally, Some((1, 1)));
}

/// The same rule for a worker the step budget ended after it delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_out_of_rounds_after_a_passing_delivery_is_verified_and_accepted() {
    let worker = Arc::new(DeliverThenLoop {
        content: "pub fn b() {}",
        after: After::ReadOthers,
        calls: AtomicUsize::new(0),
    });
    let out = run(worker, None).await;
    assert_eq!(outcomes(&out.trace), vec!["budget_exhausted".to_owned()]);
    assert_eq!(out.status, RunStatus::Completed, "{:?}", out.notices);
    assert_eq!(verdicts(&out.trace), vec![true]);
    assert!(
        out.notices
            .iter()
            .any(|n| n.contains("ran out of its step budget") && n.contains("accepted as verified")),
        "{:?}",
        out.notices
    );
}

/// A stalled tree that fails its check fails the attempt as the stall alone
/// did: blocked, a failed verdict, nothing satisfied.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_stalled_after_a_failing_delivery_still_fails() {
    let worker = Arc::new(DeliverThenLoop {
        content: "pub fn wrong() {}",
        after: After::RereadSame,
        calls: AtomicUsize::new(0),
    });
    let out = run(worker, Some("pub fn b")).await;
    assert_eq!(outcomes(&out.trace), vec!["stalled".to_owned()]);
    assert!(
        matches!(out.status, RunStatus::Blocked { .. }),
        "{:?}",
        out.status
    );
    assert_eq!(verdicts(&out.trace), vec![false]);
    assert!(
        !out.notices
            .iter()
            .any(|n| n.contains("accepted as verified")),
        "{:?}",
        out.notices
    );
    assert_eq!(out.tally, Some((1, 0)));
}

/// A check that checks nothing cannot accept a tree its worker never claimed
/// was done: the stalled attempt fails, where a completed one would be
/// accepted as unverified.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_tree_is_not_accepted_by_a_check_that_checks_nothing() {
    let worker = Arc::new(DeliverThenLoop {
        content: "pub fn b() {}",
        after: After::RereadSame,
        calls: AtomicUsize::new(0),
    });
    let out = run_with(worker, None, Some(Arc::new(Vacuous))).await;
    assert_eq!(outcomes(&out.trace), vec!["stalled".to_owned()]);
    assert!(
        matches!(out.status, RunStatus::Blocked { .. }),
        "{:?}",
        out.status
    );
    let reasons: Vec<String> = out
        .trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Verdict {
                passed: false,
                detail,
                ..
            } => Some(detail.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(reasons[0].contains("checks nothing"), "{reasons:?}");
}

/// A stopped worker's tree the verifier could not judge fails on the stop
/// alone, as before: counted against the attempt (blocked with no retry
/// left, nothing satisfied), not retried as a verifier outage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_tree_the_verifier_cannot_judge_fails_on_the_stall() {
    let worker = Arc::new(DeliverThenLoop {
        content: "pub fn b() {}",
        after: After::RereadSame,
        calls: AtomicUsize::new(0),
    });
    let out = run_with(worker, None, Some(Arc::new(Down))).await;
    assert_eq!(outcomes(&out.trace), vec!["stalled".to_owned()]);
    match &out.status {
        RunStatus::Blocked { reason } => assert!(
            !reason.contains("verifier could not run"),
            "held as a verifier outage: {reason}"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(out.tally, Some((1, 0)));
}
