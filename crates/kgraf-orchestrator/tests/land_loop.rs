//! Landing through a live run loop, and the run lease a land takes with no
//! loop (decision 0100, section 5), against the real orchestrator loop with
//! fake providers and an in-memory repository side.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::land_flow::{
    Built, FoldResult, LandError, LandGit, LandOutcome, LandRequest, TargetInfo, TargetLocation,
    UntestedBinding,
};
use kgraf_orchestrator::land_record::{LandGate, LandingEntry, ProgramSnapshot, StartBranch};
use kgraf_orchestrator::{
    GateRequest, GateRunner, GateVerdict, InboundKind, Orchestrator, OrchestratorError,
    RunStateRecord, RunStatus, RunStore, channel_port,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// A repository where `main` is at `t0` and the run branch at `TIP`; `X` is
/// on the run branch, and every merge onto `t0` has a tree of its own.
#[derive(Default)]
struct FakeRepo {
    moved: Mutex<Vec<String>>,
    folded: Mutex<Vec<String>>,
    /// Hold the move (a slow `post-merge` hook) until set.
    hold_move: Option<Arc<std::sync::atomic::AtomicBool>>,
    move_started: Arc<std::sync::atomic::AtomicBool>,
}

impl LandGit for FakeRepo {
    fn run_branch(&self) -> &str {
        "kgraf/r1"
    }
    fn run_tip(&self) -> Result<String, LandError> {
        Ok("TIP".into())
    }
    fn checked_out_branch(&self) -> Result<Option<String>, LandError> {
        Ok(Some("main".into()))
    }
    fn resolve_target(&self, name: &str) -> Result<TargetInfo, LandError> {
        let moved = self.moved.lock().expect("lock");
        Ok(TargetInfo {
            name: name.into(),
            tip: moved.last().cloned().unwrap_or_else(|| "t0".into()),
            location: TargetLocation::NotCheckedOut,
        })
    }
    fn target_busy(&self, _t: &str) -> Result<Option<String>, LandError> {
        Ok(None)
    }
    fn is_ancestor(&self, a: &str, d: &str) -> Result<bool, LandError> {
        Ok(a == d || a == "t0" || d == "TIP")
    }
    fn tree_of(&self, c: &str) -> Result<String, LandError> {
        Ok(format!("tree-{c}"))
    }
    fn count_commits(&self, _b: &str, _t: &str) -> Result<usize, LandError> {
        Ok(1)
    }
    fn programs(&self, _w: Option<&std::path::Path>) -> Result<ProgramSnapshot, LandError> {
        Ok(ProgramSnapshot::default())
    }
    fn post_land_config_changes(
        &self,
        _t: &TargetInfo,
        _m: &str,
    ) -> Result<Vec<String>, LandError> {
        Ok(Vec::new())
    }
    fn worktree_clean(&self, _w: &std::path::Path) -> Result<bool, LandError> {
        Ok(true)
    }
    fn checkout_recovery(&self, _w: &std::path::Path, _e: &LandingEntry) -> String {
        String::new()
    }
    fn signing_on(&self) -> Result<bool, LandError> {
        Ok(false)
    }
    fn build(&self, _t: &str, _c: &str, _m: &str, _s: bool) -> Result<Built, LandError> {
        Ok(Built::Merge {
            commit: "m1".into(),
            tree: "tree-merged".into(),
        })
    }
    fn move_target(
        &self,
        e: &LandingEntry,
        _l: &TargetLocation,
        _p: Option<&ProgramSnapshot>,
    ) -> Result<(), LandError> {
        self.move_started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(release) = &self.hold_move {
            // Bounded, so a loop blocked by the move fails the test, not hangs it.
            let started = std::time::Instant::now();
            while !release.load(std::sync::atomic::Ordering::SeqCst)
                && started.elapsed() < Duration::from_secs(10)
            {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        self.moved
            .lock()
            .expect("lock")
            .push(e.merge_commit.clone());
        Ok(())
    }
    fn fold(&self, m: &str) -> Result<FoldResult, LandError> {
        self.folded.lock().expect("lock").push(m.to_owned());
        Ok(FoldResult::FastForwarded)
    }
}

async fn until_in_flight(handle: &kgraf_orchestrator::OrchestratorHandle) {
    for _ in 0..500 {
        if handle
            .status()
            .await
            .is_some_and(|s| !s.in_flight.is_empty())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("never in flight");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_land_runs_through_the_live_loop_and_the_loop_records_it() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(300),
    );
    let repo = Arc::new(FakeRepo::default());
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_lander(repo.clone())
        .with_start_branch("main", "t0")
        .with_program_snapshot(ProgramSnapshot::default());
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until_in_flight(&handle).await;

    // While the loop holds the run, a land with no loop is refused.
    let busy = kgraf_orchestrator::take_run_lease_for_land(&p.graph, &RunId::new("r1"), 1);
    assert!(
        matches!(busy, Err(OrchestratorError::RunBusy { .. })),
        "the live loop holds the run lease"
    );

    let asked = handle
        .land(LandRequest {
            attended: true,
            ..LandRequest::default()
        })
        .await
        .expect("land");
    let LandOutcome::NeedsUntestedYes {
        candidate,
        target,
        target_tip,
        ..
    } = asked
    else {
        panic!("no gate passed: the warning first, nothing moved: {asked:?}");
    };
    assert!(repo.moved.lock().expect("lock").is_empty());

    let landed = handle
        .land(LandRequest {
            attended: true,
            candidate: Some(candidate.clone()),
            into: Some(target.clone()),
            untested_confirmed: true,
            untested_for: Some(UntestedBinding {
                candidate,
                target,
                target_tip,
            }),
            ..LandRequest::default()
        })
        .await
        .expect("land");
    let LandOutcome::Landed { entry, .. } = landed else {
        panic!("not landed: {landed:?}");
    };
    assert!(matches!(entry.gate, LandGate::Untested { .. }));
    assert_eq!(*repo.moved.lock().expect("lock"), ["m1"]);
    assert_eq!(*repo.folded.lock().expect("lock"), ["m1"]);

    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed);
    let record = RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert_eq!(
        record.land.start_branch.as_ref().map(|b| b.name.as_str()),
        Some("main"),
        "a fresh run records where it started"
    );
    assert!(record.land.programs.is_some(), "and its programs");
    assert_eq!(record.land.landed.len(), 1, "the loop saved the land");
    assert!(record.land.landing.is_none());

    // The loop has ended: its lease is free for a land with no loop.
    let lease = kgraf_orchestrator::take_run_lease_for_land(&p.graph, &RunId::new("r1"), 2)
        .expect("free once the loop ended");
    drop(lease);
    kgraf_orchestrator::take_run_lease_for_land(&p.graph, &RunId::new("r1"), 3)
        .expect("released on drop");
}

/// Passes the run's own gates at once; holds a land's merge-result gate
/// until released.
struct HeldGate {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl GateRunner for HeldGate {
    fn commands(&self) -> Vec<String> {
        vec!["check".into()]
    }
    async fn run(&self, request: GateRequest) -> Result<GateVerdict, String> {
        let land = request
            .pinned
            .as_ref()
            .is_some_and(|p| p.verified_by.starts_with("land"));
        if land {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(GateVerdict {
            passed: true,
            by: "check".into(),
            output: kgraf_tools::UntrustedText::new(""),
        })
    }
}

/// The merge-result gate runs in the background: a note sent while it is
/// held is acknowledged before it ends, a second land is refused by name,
/// and the land completes when the gate passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_loop_takes_notes_while_a_land_gate_runs() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_secs(5),
    );
    // A run whose root gate passed on `X` (on the run branch), resumed.
    let mut record = RunStateRecord::new(RunId::new("r1"), "make a", 1);
    record.land.start_branch = Some(StartBranch {
        name: "main".into(),
        sha: "t0".into(),
    });
    record.land.programs = Some(ProgramSnapshot::default());
    record.land.note_root_gate_pass("X", 1);
    let store = RunStore::new(p.graph.clone());
    store.save(&record).expect("save");
    store.set_active(&RunId::new("r1")).expect("active");

    let release_move = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let repo = Arc::new(FakeRepo {
        hold_move: Some(release_move.clone()),
        ..FakeRepo::default()
    });
    let gate = Arc::new(HeldGate {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let (port, mut client) = channel_port("box", Arc::new(|| {}));
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_lander(repo.clone())
        .with_gate_runner(gate.clone());
    o.attach_port(port);
    let handle = o.handle();
    let run = tokio::spawn(async move { o.resume(RunId::new("r1")).await });
    until_in_flight(&handle).await;

    let h = handle.clone();
    let land = tokio::spawn(async move {
        h.land(LandRequest {
            attended: true,
            ..LandRequest::default()
        })
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), gate.started.notified())
        .await
        .expect("the land's gate started");

    let id = client
        .send(InboundKind::Note, "prefer small functions", 1)
        .expect("send");
    let mut replied = false;
    for _ in 0..400 {
        if client.drain_replies().iter().any(|r| r.in_reply_to == id) {
            replied = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(replied, "the note was acknowledged while the gate was held");
    assert!(!land.is_finished(), "the land still waits for its gate");
    let second = handle
        .land(LandRequest {
            attended: true,
            ..LandRequest::default()
        })
        .await;
    assert!(
        matches!(second, Err(LandError::LandPending { .. })),
        "{second:?}"
    );
    assert!(repo.moved.lock().expect("lock").is_empty());

    gate.release.notify_one();
    // The move (the user's git and its post-merge hook) is slow: the loop
    // still answers notes meanwhile.
    for _ in 0..1000 {
        if repo.move_started.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(repo.move_started.load(std::sync::atomic::Ordering::SeqCst));
    let id = client
        .send(InboundKind::Note, "and keep them short", 2)
        .expect("send");
    let mut replied = false;
    for _ in 0..400 {
        if client.drain_replies().iter().any(|r| r.in_reply_to == id) {
            replied = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(replied, "the note was acknowledged while the move was held");
    assert!(!land.is_finished(), "the land still waits for its move");
    release_move.store(true, std::sync::atomic::Ordering::SeqCst);
    let outcome = tokio::time::timeout(Duration::from_secs(10), land)
        .await
        .expect("finished")
        .expect("join")
        .expect("land");
    let LandOutcome::Landed { entry, .. } = outcome else {
        panic!("not landed: {outcome:?}");
    };
    assert_eq!(
        entry.gate,
        LandGate::Passed {
            candidate: "X".into(),
            tree: "tree-merged".into()
        }
    );
    assert_eq!(*repo.moved.lock().expect("lock"), ["m1"]);
    handle.stop().await;
    let _ = run.await;
}

/// The run stops while a land's gate runs: the gate is stopped and the land
/// answered that nothing moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_stopped_during_a_land_gate_answers_that_nothing_moved() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_secs(5),
    );
    let mut record = RunStateRecord::new(RunId::new("r1"), "make a", 1);
    record.land.start_branch = Some(StartBranch {
        name: "main".into(),
        sha: "t0".into(),
    });
    record.land.programs = Some(ProgramSnapshot::default());
    record.land.note_root_gate_pass("X", 1);
    let store = RunStore::new(p.graph.clone());
    store.save(&record).expect("save");
    store.set_active(&RunId::new("r1")).expect("active");
    let repo = Arc::new(FakeRepo::default());
    let gate = Arc::new(HeldGate {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let o = Orchestrator::new(config(), p.services(WINDOW))
        .expect("orchestrator")
        .with_lander(repo.clone())
        .with_gate_runner(gate.clone());
    let handle = o.handle();
    let run = tokio::spawn(async move { o.resume(RunId::new("r1")).await });
    until_in_flight(&handle).await;
    let h = handle.clone();
    let land = tokio::spawn(async move {
        h.land(LandRequest {
            attended: true,
            ..LandRequest::default()
        })
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), gate.started.notified())
        .await
        .expect("the land's gate started");
    handle.stop().await;
    let answer = tokio::time::timeout(Duration::from_secs(20), land)
        .await
        .expect("answered")
        .expect("join");
    let err = answer.expect_err("not landed");
    assert!(
        err.to_string()
            .contains("stopped before the land finished; nothing moved"),
        "{err}"
    );
    assert!(repo.moved.lock().expect("lock").is_empty());
    let _ = run.await;
}
