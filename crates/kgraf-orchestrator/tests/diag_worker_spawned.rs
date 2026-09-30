//! `WorkerSpawned` is recorded only once a worker attempt actually starts:
//! `try_dispatch`'s admission, cost estimate,
//! lease acquisition and budget reservation, and `start_attempt`'s own
//! workspace preparation and route lookup, can all still fail or defer a
//! contract before any worker runs. An earlier version of this recorded the
//! event right after admission, before any of `start_attempt`'s own checks;
//! this test drives the workspace-preparation failure specifically, since it
//! is the one of the two `start_attempt`-level checks a fake `Workspaces` can
//! trigger without a contrived, hand-edited resume record (the route-lookup
//! check a few lines later in `start_attempt` returns the exact same way --
//! `send_failed` and an early return, with no diagnostics call reached
//! either side of it -- so proving this one is not recorded past is a direct
//! proxy for both).

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{OpenContract, Orchestrator, PrepareFailure};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// A `Workspaces` that always fails to prepare, standing in for a corrupted
/// or unreachable worktree: exactly the branch in `start_attempt` that
/// returns before a worker (and so before `WorkerSpawned`) ever exists.
struct AlwaysFailsToPrepare;

#[async_trait::async_trait]
impl kgraf_orchestrator::Workspaces for AlwaysFailsToPrepare {
    async fn prepare(
        &self,
        _contract: &OpenContract,
        _worker: &kgraf_core::AgentId,
    ) -> Result<PathBuf, PrepareFailure> {
        Err(PrepareFailure::Unavailable(
            "simulated: no worktree for this test".into(),
        ))
    }

    async fn release(&self, _workspace: &Path, _keep: bool) {}

    async fn integrate(
        &self,
        _contract: &OpenContract,
        _workspace: &Path,
    ) -> Result<kgraf_orchestrator::Integration, String> {
        unreachable!("a contract that never gets a workspace never reaches integration")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workspace_that_never_prepares_records_no_worker_spawned() {
    let dir = tempfile::tempdir().expect("dir");
    let diag_dir = tempfile::tempdir().expect("diag dir");
    let diag_path = diag_dir.path().join("diagnostics.jsonl");
    kgraf_diag::init(diag_path.clone(), kgraf_diag::DEFAULT_CAP_BYTES);

    let plan = plan_json(&[c("a", "make a WRITE src/a.rs", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    let mut services = p.services(WINDOW);
    services.workspaces = Arc::new(AlwaysFailsToPrepare);
    let report = Orchestrator::new(config(), services)
        .expect("orchestrator")
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("the run itself completes; the one contract in it never verifies");

    // Sanity: dispatch was actually attempted (otherwise the absence of
    // `WorkerSpawned` below would be vacuous, not a proof of anything).
    assert!(
        report.counters.dispatches >= 1,
        "the contract must have reached dispatch for this test to mean anything: {report:?}"
    );

    // Fix, follow-up to decision 0069: a fixed timeout raced the writer
    // thread under load. `flush_for_test` is deterministic instead -- it
    // round-trips through the writer's own FIFO channel, so it cannot return
    // before every event recorded above is already on disk -- and, unlike
    // `shutdown`, it does not tear the process-wide writer down, which would
    // be one more thing for a parallel test run sharing this process to trip
    // over.
    assert!(
        kgraf_diag::flush_for_test(),
        "the writer this test's own `init` started must still be the process-wide one"
    );
    let records = kgraf_diag::store::read_all(&diag_path).expect("read diagnostics");
    let spawned: Vec<_> = records
        .iter()
        .filter(|r| matches!(r.event, kgraf_diag::DiagEvent::WorkerSpawned { .. }))
        .collect();
    assert!(
        spawned.is_empty(),
        "a workspace that never prepares must record no WorkerSpawned; got {spawned:?}"
    );
}
