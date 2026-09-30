//! Waits at dispatch that had no end, or no voice: a route that stays
//! transiently unready, and a flight wider than the agent ceiling.

mod support;

use kgraf_core::{AgentId, ParallelismLimits, RunId};
use kgraf_orchestrator::{
    Admission, DeferReason, DirWorkspaces, FnProbe, Limits, MachineGate, OpenContract,
    Orchestrator, PrepareFailure, ReadinessFailure, RunStateRecord, RunStatus, RunStore,
    TraceEvent, TraceLog, Workspaces, WriteTarget,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// A contract with two parallel attempts, under a machine ceiling of one
/// agent (the slider set below the project's own limit): it runs at the
/// ceiling, one attempt at a time, and says so, instead of waiting for
/// capacity it can never get.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flight_wider_than_the_machine_ceiling_runs_at_the_ceiling() {
    let dir = tempfile::tempdir().expect("dir");
    let mut wide = c("a", "make a", "src/a.rs", &[]);
    wide.attempts = 2;
    let plan = plan_json(&[wide]);
    let mut p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let machine = MachineGate::new(ParallelismLimits::derive(1, 1, 1));
    p.admission = Admission::with_machine(Limits::new(4, 8).expect("limits"), machine);
    let mut cfg = config();
    cfg.max_parallel_attempts = 2;
    let report = tokio::time::timeout(
        Duration::from_secs(30),
        Orchestrator::new(cfg, p.services(WINDOW))
            .expect("orchestrator")
            .start(RunId::new("r1"), "make a".into()),
    )
    .await
    .expect("the run ended: the wide flight was not deferred for ever")
    .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("exceed the ceiling of 1 agents")),
        "{:?}",
        report.notices
    );
}

/// A route that stays transiently unready is not waited for in silence: the
/// wait is traced again, and noticed, every twenty checks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_readiness_wait_is_traced_and_noticed_periodically() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut s = p.services(WINDOW);
    let checks = Arc::new(AtomicUsize::new(0));
    let seen = checks.clone();
    let not_ready_for = 25;
    let probe = Arc::new(FnProbe(Arc::new(move || {
        if seen.fetch_add(1, Ordering::SeqCst) < not_ready_for {
            Err(ReadinessFailure::Transient("warming up".into()))
        } else {
            Ok(())
        }
    })));
    // Both routes: whichever the rotation picks is the one that waits.
    for r in &mut s.routes {
        r.readiness = probe.clone();
    }
    let report = tokio::time::timeout(
        Duration::from_secs(30),
        Orchestrator::new(config(), s)
            .expect("orchestrator")
            .start(RunId::new("r1"), "make a".into()),
    )
    .await
    .expect("the run ended")
    .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    // One notice however long the wait: the count is in the trace, not in
    // a notice text that would differ each time.
    assert_eq!(
        report
            .notices
            .iter()
            .filter(|n| n.contains("contract a: still not ready"))
            .count(),
        1,
        "{:?}",
        report.notices
    );
    assert!(
        report
            .notices
            .iter()
            .any(|n| n.contains("contract a: still not ready")),
        "{:?}",
        report.notices
    );
    let trace = TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load");
    let deferrals: Vec<&String> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Deferred {
                reason: DeferReason::Readiness,
                detail,
                ..
            } => Some(detail),
            _ => None,
        })
        .collect();
    assert!(
        deferrals
            .iter()
            .any(|d| d.contains("still not ready after 20 checks")),
        "{deferrals:?}"
    );
    assert!(
        deferrals.len() < 5,
        "the wait is reported sparsely, not every check: {deferrals:?}"
    );
}

/// Plain directories that record every prepare and release.
struct Recording {
    dirs: DirWorkspaces,
    prepared: Mutex<Vec<String>>,
    released: Mutex<Vec<(PathBuf, bool)>>,
}

#[async_trait::async_trait]
impl Workspaces for Recording {
    async fn prepare(
        &self,
        contract: &OpenContract,
        worker: &AgentId,
    ) -> Result<PathBuf, PrepareFailure> {
        self.prepared.lock().expect("lock").push(worker.to_string());
        self.dirs.prepare(contract, worker).await
    }

    async fn release(&self, workspace: &Path, keep: bool) {
        self.released
            .lock()
            .expect("lock")
            .push((workspace.to_path_buf(), keep));
    }
}

/// A stopped run with three recorded workers resumes under a ceiling of one.
/// The two workers past the ceiling are dropped: their worktrees are
/// released for discard and their attempts queue again under new ids, so a
/// queued attempt never reopens a dropped worker's worktree or transcript.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_under_a_lower_ceiling_releases_dropped_workers_and_never_reuses_their_ids() {
    let dir = tempfile::tempdir().expect("dir");
    let p = Parts::open(
        dir.path(),
        |_| "{}".into(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let ids: Vec<String> = (0..3).map(|i| format!("r1-a-r0-a{i}")).collect();
    let mut record = RunStateRecord::new(RunId::new("r1"), "crafted", 1);
    record.planned = true;
    let mut open = OpenContract::planned("a", "r1/a".into(), "make a WRITE src/a.rs", "ok");
    open.writes = vec![WriteTarget {
        path: "src/a.rs".into(),
        node: None,
    }];
    open.tools = vec!["write_file".into()];
    open.attempts = 3;
    open.workers = ids.iter().map(|i| AgentId::new(i.as_str())).collect();
    open.worker_routes = vec!["alpha".into(); 3];
    record.open_contracts = vec![open];
    let store = RunStore::new(p.graph.clone());
    store.save(&record).expect("save");
    store.set_active(&RunId::new("r1")).expect("active");

    let ws = Arc::new(Recording {
        dirs: DirWorkspaces {
            base: dir.path().join("worktrees"),
        },
        prepared: Mutex::new(Vec::new()),
        released: Mutex::new(Vec::new()),
    });
    let mut services = p.services(WINDOW);
    services.workspaces = ws.clone();
    let machine = MachineGate::new(ParallelismLimits::derive(1, 1, 1));
    services.admission = Admission::with_machine(Limits::new(4, 8).expect("limits"), machine);
    let mut cfg = config();
    cfg.max_parallel_attempts = 3;
    let report = tokio::time::timeout(
        Duration::from_secs(30),
        Orchestrator::new(cfg, services)
            .expect("orchestrator")
            .resume(RunId::new("r1")),
    )
    .await
    .expect("the run ended")
    .expect("resume");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");

    let prepared = ws.prepared.lock().expect("lock").clone();
    for dropped in &ids[1..] {
        // Reopened once, to discard it; a queued attempt never reopens it.
        assert_eq!(
            prepared.iter().filter(|w| *w == dropped).count(),
            1,
            "a queued attempt reopened dropped worker {dropped}: {prepared:?}"
        );
        let path = dir.path().join("worktrees").join(dropped);
        assert!(
            ws.released.lock().expect("lock").contains(&(path, false)),
            "dropped worker {dropped} was never released for discard"
        );
    }
    // The kept worker, two discards, and two queued attempts under new ids.
    let fresh: Vec<&String> = prepared.iter().filter(|w| !ids.contains(w)).collect();
    assert_eq!(fresh.len(), 2, "{prepared:?}");
    assert_ne!(fresh[0], fresh[1], "{prepared:?}");
}

fn always_transient(text: &'static str) -> Arc<FnProbe> {
    Arc::new(FnProbe(Arc::new(move || {
        Err(ReadinessFailure::Transient(text.into()))
    })))
}

/// One of two routes stays down. A contract with two parallel attempts must
/// not wait for it: the attempts run on the route that is ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_attempt_is_not_held_for_a_route_that_is_down() {
    let dir = tempfile::tempdir().expect("dir");
    let mut wide = c("a", "make a", "src/a.rs", &[]);
    wide.attempts = 2;
    let plan = plan_json(&[wide]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let mut s = p.services(WINDOW);
    s.routes[0].readiness = always_transient("down");
    let mut cfg = config();
    cfg.max_parallel_attempts = 2;
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        Orchestrator::new(cfg, s)
            .expect("orchestrator")
            .start(RunId::new("r1"), "make a".into()),
    )
    .await
    .expect("the attempts ran on the ready route, not held for the down one")
    .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
}

/// Every route stays down. The contract is held after the give-up time, which
/// is counted from the first failed check in time (ticks), and the notice
/// and the hold carry no secret from the failure text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_on_a_route_that_never_recovers_ends_in_a_hold_without_the_secret() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(1),
    );
    let mut s = p.services(WINDOW);
    let secret = "ghp_a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2";
    let probe = always_transient(secret);
    for r in &mut s.routes {
        r.readiness = probe.clone();
    }
    let mut cfg = config();
    cfg.tick = Duration::from_millis(1);
    let report = tokio::time::timeout(
        Duration::from_secs(60),
        Orchestrator::new(cfg, s)
            .expect("orchestrator")
            .start(RunId::new("r1"), "make a".into()),
    )
    .await
    .expect("the run ended: the wait had an end")
    .expect("run");
    let reason = report
        .held
        .iter()
        .find(|(c, _)| c == "a")
        .map(|(_, r)| r.clone())
        .unwrap_or_default();
    assert!(reason.contains("not ready after"), "{report:?}");
    assert!(!reason.contains(secret), "{reason}");
    assert!(
        report.notices.iter().all(|n| !n.contains(secret)),
        "{:?}",
        report.notices
    );
    assert!(
        report.notices.iter().any(|n| n.contains("still not ready")),
        "{:?}",
        report.notices
    );
}
