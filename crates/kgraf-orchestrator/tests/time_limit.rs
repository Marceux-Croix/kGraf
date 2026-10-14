//! The run's time limit applied to its workers (`run_deadline.rs`).
//!
//! A worker under a run with a time limit stops before the limit, and its
//! tree is verified and merged inside it; no attempt starts when the time
//! left cannot hold one round; a run without a limit is unchanged.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{
    OpenContract, Orchestrator, OrchestratorConfig, RunReport, RunStatus, TraceEvent, TraceLog,
    Verdict, VerifiedCommit, Verifier,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;

fn plan(instruction: &str) -> String {
    serde_json::json!({"contracts":[
        {"id":"long","instruction":instruction,
         "acceptance":"src/long.rs exists",
         "writes":[{"path":"src/long.rs"}],"tools":["write_file"]}
    ]})
    .to_string()
}

fn limited(seconds: Option<u64>) -> OrchestratorConfig {
    OrchestratorConfig {
        time_limit: seconds.map(Duration::from_secs),
        ..config()
    }
}

async fn run(p: &Parts, config: OrchestratorConfig) -> (RunReport, Duration, Vec<TraceEvent>) {
    run_with(p, config, None).await
}

/// [`run`] with `verifier` in place of the harness's when set.
async fn run_with(
    p: &Parts,
    config: OrchestratorConfig,
    verifier: Option<Arc<dyn Verifier>>,
) -> (RunReport, Duration, Vec<TraceEvent>) {
    let mut services = p.services(200_000);
    if let Some(v) = verifier {
        services.verifier = v;
    }
    let o = Orchestrator::new(config, services).expect("orchestrator");
    let started = Instant::now();
    let report = tokio::time::timeout(
        Duration::from_secs(30),
        o.start(RunId::new("r1"), "a long part".into()),
    )
    .await
    .expect("the run ends")
    .expect("run");
    let took = started.elapsed();
    let trace = TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    (report, took, trace)
}

fn time_limits(trace: &[TraceEvent]) -> Vec<(String, bool)> {
    trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::TimeLimit { action, cut, .. } => Some((action.clone(), *cut)),
            _ => None,
        })
        .collect()
}

fn finished(trace: &[TraceEvent]) -> Vec<(String, u32)> {
    trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::WorkerFinished { outcome, steps, .. } => Some((outcome.clone(), *steps)),
            _ => None,
        })
        .collect()
}

fn dispatches(trace: &[TraceEvent]) -> usize {
    trace
        .iter()
        .filter(|e| matches!(e, TraceEvent::Dispatch { .. }))
        .count()
}

/// The diagnosed failure, scaled down: a worker whose rounds take 400 ms needs
/// eleven of them (4.4 s) and the run has 4 s. It stops before the worker
/// deadline (3.6 s: the limit less its tenth held back), its tree is verified
/// as it stands and merged, and the run completes inside the limit. Without
/// the deadline it runs all eleven rounds, past the limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_worker_under_a_time_limit_stops_before_it_and_its_tree_is_verified() {
    let dir = tempfile::tempdir().expect("dir");
    let instruction = "LONGRUN part WRITE src/long.rs";
    let p = Parts::open(
        dir.path(),
        move |_| plan(instruction),
        |_| SERVES.into(),
        Duration::from_millis(400),
    );
    let (report, took, trace) = run(&p, limited(Some(4))).await;
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["long".to_owned()]);
    assert!(took < Duration::from_secs(4), "the run took {took:?}");

    let ended = finished(&trace);
    assert_eq!(ended.len(), 1, "one attempt: {ended:?}");
    assert_eq!(ended[0].0, "budget_exhausted");
    assert!(
        ended[0].1 < 11,
        "stopped before its eleven rounds: {ended:?}"
    );
    let stops = time_limits(&trace);
    assert_eq!(
        stops,
        vec![
            ("worker_stopped".to_owned(), false),
            ("tree_verified".to_owned(), false)
        ]
    );
    let stopped = trace
        .iter()
        .find_map(|e| match e {
            TraceEvent::TimeLimit {
                limit_ms,
                deadline_ms,
                elapsed_ms,
                remaining_ms,
                ..
            } => Some((*limit_ms, *deadline_ms, *elapsed_ms, *remaining_ms)),
            _ => None,
        })
        .expect("stop");
    assert_eq!((stopped.0, stopped.1), (4_000, 3_600));
    assert!(stopped.2 <= 3_700, "stopped {} ms in", stopped.2);
    assert!(stopped.3 <= 3_600);
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("stopped at its time limit") && n.contains("verified as it stands")),
        "{:?}",
        report.notices
    );
}

/// A worker cut at the deadline with nothing written leaves the contract to
/// retry, and no time is left for a round: the retry is not started, a notice
/// and a trace record say so with the numbers, and the run ends with what it
/// has. Without the gate a second attempt is dispatched past the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_attempt_starts_when_the_time_left_is_below_the_floor() {
    let dir = tempfile::tempdir().expect("dir");
    let instruction = "HOLDME part WRITE src/long.rs";
    let p = Parts::open(
        dir.path(),
        move |_| plan(instruction),
        |_| SERVES.into(),
        Duration::from_millis(1),
    );
    // Never released: the first round runs until the deadline cuts it.
    let _held = p.hold_workers("HOLDME");
    let (report, took, trace) = run(&p, limited(Some(2))).await;
    assert!(took < Duration::from_secs(5), "the run took {took:?}");
    assert_eq!(dispatches(&trace), 1, "no second attempt");
    assert_eq!(
        time_limits(&trace),
        vec![
            ("worker_stopped".to_owned(), true),
            ("attempt_refused".to_owned(), false)
        ]
    );
    match &report.status {
        RunStatus::Blocked { reason } => {
            assert!(
                reason.contains("no attempt could start at the time limit"),
                "{reason}"
            );
        }
        other => panic!("expected blocked, got {other:?}"),
    }
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("no attempt was started at the time limit")),
        "{:?}",
        report.notices
    );
    assert!(report.verified.is_empty());
}

/// Without a time limit the same slow worker is not stopped: it runs all
/// eleven rounds and completes, and nothing about time is traced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_time_limit_a_slow_worker_runs_to_its_answer() {
    let dir = tempfile::tempdir().expect("dir");
    let instruction = "LONGRUN part WRITE src/long.rs";
    let p = Parts::open(
        dir.path(),
        move |_| plan(instruction),
        |_| SERVES.into(),
        Duration::from_millis(50),
    );
    let (report, _, trace) = run(&p, limited(None)).await;
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(finished(&trace), vec![("completed".to_owned(), 11)]);
    assert!(time_limits(&trace).is_empty());
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

fn slow_part(dir: &std::path::Path) -> Parts {
    Parts::open(
        dir,
        move |_| plan("LONGRUN part WRITE src/long.rs"),
        |_| SERVES.into(),
        Duration::from_millis(400),
    )
}

fn verdicts(trace: &[TraceEvent]) -> Vec<(bool, String)> {
    trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Verdict { passed, detail, .. } => Some((*passed, detail.clone())),
            _ => None,
        })
        .collect()
}

/// The diagnosed failure: the contract's check is `true`, so the stopped
/// worker's tree was refused as unjudgeable and the run delivered nothing.
/// With no attempt able to start, the tree is accepted on that check, and a
/// trace record and a notice say why. Without the change the verdict is a
/// failure ("checks nothing") and the run is blocked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tree_stopped_at_the_limit_is_accepted_by_a_check_that_checks_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let p = slow_part(dir.path());
    let (report, took, trace) = run_with(&p, limited(Some(4)), Some(Arc::new(Vacuous))).await;
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["long".to_owned()]);
    assert!(took < Duration::from_secs(4), "the run took {took:?}");
    assert!(
        time_limits(&trace).contains(&("tree_verified".to_owned(), false)),
        "{:?}",
        time_limits(&trace)
    );
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("no further attempt could run") && n.contains("checks nothing")),
        "{:?}",
        report.notices
    );
}

/// A stopped worker's tree that fails its check ends the run held, with the
/// verification's own reason, as today. Nothing is merged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tree_stopped_at_the_limit_that_fails_its_check_ends_blocked_with_the_reason() {
    let dir = tempfile::tempdir().expect("dir");
    let p = slow_part(dir.path());
    *p.verifier.require.lock().expect("lock") = Some("NEVER PRESENT".into());
    let (report, _, trace) = run(&p, limited(Some(4))).await;
    assert!(
        matches!(report.status, RunStatus::Blocked { .. }),
        "{:?}",
        report.status
    );
    assert!(report.verified.is_empty());
    let v = verdicts(&trace);
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(!v[0].0 && v[0].1.contains("missing or wrong"), "{v:?}");
}

/// The verification of a limit-stopped tree runs inside the time left to the
/// limit: a check that would outlast it is stopped and named, and the run
/// ends before its limit instead of after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_verification_of_a_tree_stopped_at_the_limit_never_runs_past_the_limit() {
    let dir = tempfile::tempdir().expect("dir");
    let p = slow_part(dir.path());
    *p.verifier.delay.lock().expect("lock") = Duration::from_secs(20);
    let (report, took, trace) = run(&p, limited(Some(4))).await;
    assert!(took < Duration::from_secs(5), "the run took {took:?}");
    assert!(
        matches!(report.status, RunStatus::Blocked { .. }),
        "{:?}",
        report.status
    );
    let v = verdicts(&trace);
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(
        !v[0].0 && v[0].1.contains("before the run's time limit"),
        "{v:?}"
    );
}
