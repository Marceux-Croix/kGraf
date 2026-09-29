//! A panic inside an attempt's, a verifier's, or a gate's own spawned task
//! must still reach the loop as an ordinary (failed) outcome: dropping the
//! `JoinHandle` and never catching `is_panic()` leaves `LoopMsg::Finished` (or
//! `Verified`, or `GateFinished`) unsent, so `in_flight` never empties, the
//! run (and `stop`) hangs, and the admission slot the loop's flight held is
//! never released. Each test drives a real panic through a real call stack --
//! not a mocked `JoinError` -- in code that runs directly inside the spawned
//! closure, not inside the runtime's own worker loop (which already isolates
//! a provider's panic in its channel before it ever reaches these sites).

mod support;

use kgraf_core::{AgentId, RunId, TaskId};
use kgraf_orchestrator::{
    AttemptEnv, AttemptEnvs, GateRequest, GateRunner, GateVerdict, OpenContract, Orchestrator,
    Route, RunStatus, Verdict, Verifier,
};
use kgraf_runtime::WorkerResult;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// [`AttemptEnvs`] whose `build` always panics: a real panic in the code an
/// attempt's spawned task runs directly (workspace/environment setup, before
/// `run_attempt`), not inside the runtime's own worker channel.
struct PanicEnvs;

#[async_trait::async_trait]
impl AttemptEnvs for PanicEnvs {
    async fn build(
        &self,
        _route: &Route,
        _workspace: &Path,
        _worker: &AgentId,
        _task: &TaskId,
    ) -> Result<AttemptEnv, String> {
        panic!("the attempt's environment build panicked, on purpose");
    }
}

/// Attempt spawn (orchestrator.rs): a panic there must not hang the run, and
/// must give back the admission slot the loop's flight held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attempt_whose_environment_build_panics_ends_blocked_and_releases_admission() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut cfg = config();
    cfg.max_retries = 0;
    let report = tokio::time::timeout(
        Duration::from_secs(30),
        Orchestrator::new(cfg, p.services(WINDOW))
            .expect("orchestrator")
            .with_attempt_envs(Arc::new(PanicEnvs))
            .start(RunId::new("r1"), "make a".into()),
    )
    .await
    .expect("the run ended in time, not hung on the panicked attempt")
    .expect("run");
    assert!(
        matches!(report.status, RunStatus::Blocked { .. }),
        "{report:?}"
    );
    assert_eq!(
        p.admission.gate().in_flight(),
        (0, 0),
        "the panicked attempt's admission slot was released"
    );
}

/// A [`Verifier`] whose `verify` always panics: a real panic in code the
/// verifier's spawned task runs directly.
struct PanicVerifier;

#[async_trait::async_trait]
impl Verifier for PanicVerifier {
    async fn verify(
        &self,
        _contract: &OpenContract,
        _workspace: &Path,
        _result: &WorkerResult,
    ) -> Result<Verdict, String> {
        panic!("the verifier panicked, on purpose");
    }

    async fn recheck(&self, _commit: &kgraf_orchestrator::VerifiedCommit) -> Result<bool, String> {
        Ok(true)
    }
}

/// Verifier spawn (orchestrator.rs): a panic there must not hang the run, and
/// must give back the admission slot the loop's flight held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verifier_that_panics_ends_blocked_and_releases_admission() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut services = p.services(WINDOW);
    services.verifier = Arc::new(PanicVerifier);
    let mut cfg = config();
    cfg.max_retries = 0;
    let report = tokio::time::timeout(
        Duration::from_secs(30),
        Orchestrator::new(cfg, services)
            .expect("orchestrator")
            .start(RunId::new("r1"), "make a".into()),
    )
    .await
    .expect("the run ended in time, not hung on the panicked verifier")
    .expect("run");
    assert!(
        matches!(report.status, RunStatus::Blocked { .. }),
        "{report:?}"
    );
    assert_eq!(
        p.admission.gate().in_flight(),
        (0, 0),
        "the panicked verification's admission slot was released"
    );
}

/// A [`GateRunner`] whose `run` always panics: a real panic in code the
/// gate's spawned task runs directly.
struct PanicGate {
    commands: Vec<String>,
}

#[async_trait::async_trait]
impl GateRunner for PanicGate {
    fn commands(&self) -> Vec<String> {
        self.commands.clone()
    }

    async fn run(&self, _request: GateRequest) -> Result<GateVerdict, String> {
        panic!("the gate panicked, on purpose");
    }
}

/// Gate spawn (coord.rs): a panic there must not hang the run, and must give
/// back the admission slot the loop's flight held for the gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gate_that_panics_ends_blocked_and_releases_admission() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let gate = Arc::new(PanicGate {
        commands: vec!["cargo test --workspace".into()],
    });
    let report = tokio::time::timeout(
        Duration::from_secs(30),
        Orchestrator::new(config(), p.services(WINDOW))
            .expect("orchestrator")
            .with_gate_runner(gate)
            .start(RunId::new("r1"), "make a".into()),
    )
    .await
    .expect("the run ended in time, not hung on the panicked gate")
    .expect("run");
    assert!(
        matches!(report.status, RunStatus::Blocked { .. }),
        "{report:?}"
    );
    assert_eq!(
        p.admission.gate().in_flight(),
        (0, 0),
        "the panicked gate's admission slot was released"
    );
}
