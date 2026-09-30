//! Scheduled runs start through the orchestrator (0052): readiness first, typed
//! failures for the claim table, a cap on concurrent runs, and every outcome kept
//! until it is collected, failures included.

mod support;

use chrono::{TimeZone, Utc};
use kgraf_orchestrator::{
    Orchestrator, OrchestratorError, OrchestratorFactory, ReadinessFailure, RunStatus,
    ScheduledRuns,
};
use kgraf_schedule::{DisableReason, FailureClass, RunStartOutcome, RunStarter};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use support::*;

struct Factory {
    parts: Parts,
    ready: Mutex<Result<(), ReadinessFailure>>,
    /// When set, the next `build` fails with this error (taken once).
    build_fails: Mutex<Option<OrchestratorError>>,
}

#[async_trait::async_trait]
impl OrchestratorFactory for Factory {
    async fn configured(&self) -> Result<(), ReadinessFailure> {
        Ok(())
    }
    async fn ready(&self, _schedule_id: &str) -> Result<(), ReadinessFailure> {
        self.ready.lock().expect("lock").clone()
    }
    fn build(&self, _run: &kgraf_core::RunId) -> Result<Orchestrator, OrchestratorError> {
        if let Some(e) = self.build_fails.lock().expect("lock").take() {
            return Err(e);
        }
        Orchestrator::new(config(), self.parts.services(200_000))
    }
}

fn factory(dir: &std::path::Path, ready: Result<(), ReadinessFailure>) -> Arc<Factory> {
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    Arc::new(Factory {
        parts: Parts::open(
            dir,
            move |prompt| {
                if prompt.contains("BROKEN") {
                    "no plan today".into()
                } else if prompt.contains("PANIC") {
                    panic!("the planner model panicked, on purpose");
                } else {
                    plan.clone()
                }
            },
            |_| SERVES.into(),
            Duration::from_millis(100),
        ),
        ready: Mutex::new(ready),
        build_fails: Mutex::new(None),
    })
}

/// A build the same configuration can get past later counts toward the pause
/// threshold; any other build failure disables the schedule, as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transient_build_failure_counts_and_does_not_disable() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), Ok(()));
    let mut instructions = BTreeMap::new();
    instructions.insert("nightly".to_owned(), "make a".to_owned());
    let starter = ScheduledRuns::new(factory.clone(), instructions, 1);
    let due = Utc
        .with_ymd_and_hms(2026, 9, 26, 3, 0, 0)
        .single()
        .expect("time");
    *factory.build_fails.lock().expect("lock") = Some(OrchestratorError::Build {
        detail: "MCP server `gone` did not answer".into(),
        transient: true,
    });
    let out = starter.start_run("nightly", due).await.expect("start");
    assert!(
        matches!(&out, RunStartOutcome::Failed(FailureClass::Transient { detail }) if detail.contains("gone")),
        "{out:?}"
    );
    assert_eq!(starter.active(), 0, "the slot is given back");
    *factory.build_fails.lock().expect("lock") = Some(OrchestratorError::Build {
        detail: "route `x` is unknown".into(),
        transient: false,
    });
    let out = starter.start_run("nightly", due).await.expect("start");
    assert!(
        matches!(
            out,
            RunStartOutcome::Failed(FailureClass::Disabling(
                DisableReason::ConfigurationMissing { .. }
            ))
        ),
        "{out:?}"
    );
    // A credential gone between the readiness check and the build disables
    // with a missing-credential reason, not a configuration one.
    *factory.build_fails.lock().expect("lock") = Some(OrchestratorError::MissingCredential(
        "no credential for `kimi`: set KGRAF_KIMI_API_KEY".into(),
    ));
    let out = starter.start_run("nightly", due).await.expect("start");
    assert!(
        matches!(
            &out,
            RunStartOutcome::Failed(FailureClass::Disabling(
                DisableReason::MissingCredential { detail }
            )) if detail.contains("KGRAF_KIMI_API_KEY")
        ),
        "{out:?}"
    );
    assert_eq!(starter.active(), 0);
}

/// A panic inside a scheduled run still records an outcome and gives the slot
/// back: the claim is settled and the cap does not shrink.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_panicking_run_still_ends_with_an_outcome_and_frees_its_slot() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), Ok(()));
    let mut instructions = BTreeMap::new();
    instructions.insert("boom".to_owned(), "PANIC make a".to_owned());
    let starter = ScheduledRuns::new(factory, instructions, 1);
    let due = Utc
        .with_ymd_and_hms(2026, 9, 26, 3, 0, 0)
        .single()
        .expect("time");
    assert_eq!(
        starter.start_run("boom", due).await.expect("start"),
        RunStartOutcome::Started
    );
    tokio::time::timeout(Duration::from_secs(30), drain(&starter))
        .await
        .expect("the slot is given back");
    let finished = starter.take_finished();
    assert_eq!(finished.len(), 1, "the outcome is recorded");
    let err = finished[0]
        .1
        .as_ref()
        .expect_err("a panicked run is a failure");
    assert!(err.contains("panicked"), "{err}");
}

async fn drain(starter: &ScheduledRuns) {
    while starter.active() > 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scheduled_run_checks_readiness_first_and_then_runs_to_completion() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(
        dir.path(),
        Err(ReadinessFailure::MissingCredential(
            "KIMI_API_KEY is not set".into(),
        )),
    );
    let mut instructions = BTreeMap::new();
    instructions.insert("nightly".to_owned(), "make a".to_owned());
    let starter = ScheduledRuns::new(factory.clone(), instructions, 2);
    let due = Utc
        .with_ymd_and_hms(2026, 9, 26, 3, 0, 0)
        .single()
        .expect("time");

    // A missing credential disables the schedule, and no run starts.
    let out = starter.start_run("nightly", due).await.expect("start");
    assert!(matches!(
        out,
        RunStartOutcome::Failed(FailureClass::Disabling(
            DisableReason::MissingCredential { .. }
        ))
    ));
    assert_eq!(starter.active(), 0);
    assert!(factory.parts.stats.requests().is_empty());

    // A transient failure counts, it does not disable.
    *factory.ready.lock().expect("lock") =
        Err(ReadinessFailure::Transient("keychain locked".into()));
    let out = starter.start_run("nightly", due).await.expect("start");
    assert!(matches!(
        out,
        RunStartOutcome::Failed(FailureClass::Transient { .. })
    ));

    // An unknown schedule is a configuration failure.
    let out = starter.start_run("weekly", due).await.expect("start");
    assert!(matches!(
        out,
        RunStartOutcome::Failed(FailureClass::Disabling(
            DisableReason::ConfigurationMissing { .. }
        ))
    ));

    *factory.ready.lock().expect("lock") = Ok(());
    assert_eq!(
        starter.start_run("nightly", due).await.expect("start"),
        RunStartOutcome::Started
    );
    drain(&starter).await;
    let finished = starter.take_finished();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].0, ScheduledRuns::run_id("nightly", due));
    let report = finished[0].1.as_ref().expect("the run's report");
    assert_eq!(report.status, RunStatus::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_scheduled_runs_are_capped_and_a_failed_run_is_surfaced() {
    let dir = tempfile::tempdir().expect("dir");
    let factory = factory(dir.path(), Ok(()));
    let mut instructions = BTreeMap::new();
    instructions.insert("nightly".to_owned(), "make a".to_owned());
    instructions.insert("broken".to_owned(), "BROKEN make a".to_owned());
    let starter = ScheduledRuns::new(factory, instructions, 1);
    let t = |h| {
        Utc.with_ymd_and_hms(2026, 9, 26, h, 0, 0)
            .single()
            .expect("time")
    };

    assert_eq!(
        starter.start_run("nightly", t(1)).await.expect("start"),
        RunStartOutcome::Started
    );
    // The cap is one run: a second start is refused while the first runs. A
    // capacity refusal is `Deferred`, not `Transient` -
    // it says nothing about this schedule's own configuration, so it must never
    // count toward the consecutive-failure pause threshold the way a real run
    // failure does.
    assert!(matches!(
        starter.start_run("nightly", t(2)).await.expect("start"),
        RunStartOutcome::Failed(FailureClass::Deferred { .. })
    ));
    drain(&starter).await;

    // A run that fails is kept for collection, not lost with its task.
    assert_eq!(
        starter.start_run("broken", t(3)).await.expect("start"),
        RunStartOutcome::Started
    );
    drain(&starter).await;
    let finished = starter.take_finished();
    assert_eq!(finished.len(), 2);
    assert!(finished[0].1.is_ok());
    let err = finished[1].1.as_ref().expect_err("the broken run failed");
    assert!(err.contains("decomposition failed"), "{err}");
    assert!(starter.take_finished().is_empty());
}
