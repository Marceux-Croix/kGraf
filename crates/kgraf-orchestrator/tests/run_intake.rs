//! Input taken in outside the run loop (decision 0100, section 4), through the
//! real loop with fake providers and no network: a note is acknowledged and a
//! status question answered while the loop waits on a planner call; the
//! question cards never wait for the loop; a row is folded into the record
//! exactly once across a kill; a row for an ended run says it waits; a stop
//! during a planner call leaves no unreconciled ledger row.
//!
//! Order is asserted, never wall time: "acknowledged while the call is still
//! held" is checked with the call held by a semaphore the test releases.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::run_intake::{NOTED, NOTED_COMPLETED, NOTED_NOT_RUNNING};
use kgraf_orchestrator::{
    InboundKind, Orchestrator, OutboundReply, PortClient, RunStatus, RunStore, TraceEvent,
    TraceLog, channel_port,
};
use kgraf_provider::Settlement;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;

const WINDOW: u64 = 200_000;

/// Longest wait for a reply: a guard against a hang, not a budget.
const HANG_GUARD: Duration = Duration::from_secs(20);

async fn reply_to(client: &PortClient, id: &str) -> OutboundReply {
    let started = Instant::now();
    loop {
        if let Some(r) = client
            .drain_replies()
            .into_iter()
            .find(|r| r.in_reply_to == id)
        {
            return r;
        }
        assert!(started.elapsed() < HANG_GUARD, "no reply to {id}");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn record_of(p: &Parts) -> kgraf_orchestrator::RunStateRecord {
    RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record")
}

async fn trace_of(p: &Parts) -> Vec<TraceEvent> {
    let path = p.dir.join("runs").join("r1").join("trace.jsonl");
    let started = Instant::now();
    loop {
        match TraceLog::open(&path).await {
            Ok(log) => return log.load().await.expect("load"),
            Err(_) if started.elapsed() < HANG_GUARD => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("trace: {e:?}"),
        }
    }
}

fn intake_rows(p: &Parts) -> Vec<kgraf_graph::IntakeRow> {
    p.graph
        .lock()
        .expect("graph")
        .intake_after("r1", 0)
        .expect("rows")
}

/// What a dead process's leases come to for a resume: this test process is
/// alive, so the killed run's leases are released the way a takeover of a
/// dead owner releases them.
fn release_killed_leases(p: &Parts) {
    let g = p.graph.lock().expect("lock");
    let reg = kgraf_graph::LeaseRegistry::new(&g);
    let holders: std::collections::BTreeSet<String> = reg
        .all()
        .expect("leases")
        .into_iter()
        .map(|l| l.agent_id)
        .collect();
    for h in holders {
        reg.release_all(&h).expect("release");
    }
}

/// B1 holds whatever the loop does: with the `decompose` call held, a note is
/// acknowledged (its row committed) and a status question answered, the
/// question cards read, and the note is folded once the loop is free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_and_a_status_question_are_answered_while_decompose_is_held() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(20),
    );
    let release = p.planner.hold("make a");
    let (port, client) = channel_port("box", Arc::new(|| {}));
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    o.attach_port(port);
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until("the decompose call is held", || p.planner.calls() == 1).await;

    let mut client = client;
    let n = client
        .send(InboundKind::Note, "prefer small functions", 1)
        .expect("send");
    let ack = reply_to(&client, &n).await;
    assert_eq!(ack.text, NOTED);
    let rows = intake_rows(&p);
    assert_eq!(
        rows.len(),
        1,
        "the row is committed before the acknowledgment"
    );
    assert_eq!(rows[0].kind, "note");
    assert_eq!(rows[0].target, "run");
    let q = client
        .send(InboundKind::StatusQuestion, "how far along?", 2)
        .expect("send");
    let status = reply_to(&client, &q).await;
    assert!(status.text.starts_with("run r1:"), "{}", status.text);
    assert!(
        status
            .text
            .contains("planning: the decompose call is running"),
        "{}",
        status.text
    );
    assert!(handle.published_questions().is_empty());
    // Every answer came while the call was still held.
    assert!(!run.is_finished());
    assert_eq!(p.planner.calls(), 1);
    assert_eq!(p.planner.prompts().len(), 1);
    release.add_permits(1);

    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let record = record_of(&p);
    assert_eq!(
        record.notes.iter().filter(|note| note.id == n).count(),
        1,
        "the note is folded once"
    );
    assert_eq!(record.intake_folded, Some(rows[0].id));
    let trace = trace_of(&p).await;
    assert!(trace.iter().any(|e| matches!(
        e,
        TraceEvent::IntakeFolded { row, ack_us: Some(_), fold_us: Some(_), .. } if *row == rows[0].id
    )));
}

/// A stop during the `decompose` call ends the run at once, and the cut call
/// settles cancelled, so no row of the run is left unreconciled; the cut is
/// traced with its stop latency.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_during_decompose_leaves_no_unreconciled_row() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(20),
    );
    let _held = p.planner.hold("make a");
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    until("the decompose call is held", || p.planner.calls() == 1).await;
    handle.stop().await;
    let report = tokio::time::timeout(HANG_GUARD, run)
        .await
        .expect("the stop ended the run")
        .expect("join")
        .expect("run");
    assert_eq!(report.status, RunStatus::Stopped, "{report:?}");
    let calls = p.ledger.calls().expect("calls");
    let decompose: Vec<_> = calls
        .iter()
        .filter(|c| c.trace.kind.as_deref() == Some("decompose"))
        .collect();
    assert_eq!(decompose.len(), 1);
    assert_eq!(decompose[0].settlement, Settlement::Cancelled);
    assert!(
        calls
            .iter()
            .all(|c| c.settlement != Settlement::Unreconciled),
        "{calls:?}"
    );
    let trace = trace_of(&p).await;
    assert!(trace.iter().any(|e| matches!(
        e,
        TraceEvent::PlannerTask { kind, outcome, stop_us: Some(_), .. }
            if kind == "decompose" && outcome == "cancelled"
    )));
}

/// A note for a completed run is kept in the table and says it waits for a
/// reopen; nothing folds it into the finished record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_for_a_completed_run_waits_for_a_reopen() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(20),
    );
    let (port, mut client) = channel_port("box", Arc::new(|| {}));
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    o.attach_port(port);
    let report = o
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed);
    let before = record_of(&p);
    let n = client
        .send(InboundKind::Note, "and one more thing", 1)
        .expect("send");
    let ack = reply_to(&client, &n).await;
    assert_eq!(ack.text, NOTED_COMPLETED);
    let rows = intake_rows(&p);
    assert_eq!(rows.len(), 1, "kept in the table");
    let after = record_of(&p);
    assert_eq!(after.notes.len(), before.notes.len(), "not folded");
    assert!(after.intake_folded.is_none_or(|f| f < rows[0].id));
}

/// A note for a stopped run (no loop) says it waits for a resume; the resume
/// folds it before its first orchestrator call, which carries it, and once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_for_a_stopped_run_is_folded_by_the_resume_once() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(20),
    );
    let held = p.hold_workers("make a");
    let (port, mut client) = channel_port("box", Arc::new(|| {}));
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    o.attach_port(port);
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    handle
        .published(|s| s.status.as_ref().is_some_and(|s| !s.in_flight.is_empty()))
        .await;
    handle.stop().await;
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Stopped);

    let n = client
        .send(InboundKind::Note, "use the vendored parser", 1)
        .expect("send");
    assert_eq!(reply_to(&client, &n).await.text, NOTED_NOT_RUNNING);
    let rows = intake_rows(&p);
    assert_eq!(rows.len(), 1);
    assert!(record_of(&p).notes.iter().all(|note| note.id != n));

    held.add_permits(100);
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let report = o.resume(RunId::new("r1")).await.expect("resume");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let record = record_of(&p);
    assert_eq!(record.notes.iter().filter(|note| note.id == n).count(), 1);
    assert_eq!(record.intake_folded, Some(rows[0].id));
    // Folded as the resume opened: the resume's re-grounding carried it.
    assert!(
        p.grounder
            .prompts()
            .iter()
            .any(|prompt| prompt.contains("use the vendored parser")),
        "the note reached the resume's first orchestrator call"
    );
}

/// A kill after the fold: the record saved the folded row id with the note,
/// so the next resume folds nothing again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_after_the_fold_folds_the_row_once() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(20),
    );
    let held = p.hold_workers("make a");
    let (port, mut client) = channel_port("box", Arc::new(|| {}));
    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    o.attach_port(port);
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    handle
        .published(|s| s.status.as_ref().is_some_and(|s| !s.in_flight.is_empty()))
        .await;
    let n = client
        .send(InboundKind::Note, "keep the old flag", 1)
        .expect("send");
    assert_eq!(reply_to(&client, &n).await.text, NOTED);
    until("the note is folded and saved", || {
        record_of(&p).notes.iter().any(|note| note.id == n)
    })
    .await;
    // The kill: the loop's future is dropped where it stands.
    run.abort();
    let _ = run.await;
    release_killed_leases(&p);
    held.add_permits(100);

    let o = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let report = o.resume(RunId::new("r1")).await.expect("resume");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let record = record_of(&p);
    assert_eq!(
        record.notes.iter().filter(|note| note.id == n).count(),
        1,
        "{:?}",
        record.notes
    );
}

fn concurrent() -> kgraf_orchestrator::OrchestratorConfig {
    kgraf_orchestrator::OrchestratorConfig {
        reground_every: 1,
        concurrent_planner: true,
        ..config()
    }
}

/// With the concurrent planner, a held re-grounding is one call, however many
/// loop steps pass while it is held, and another contract verifies and
/// merges meanwhile; the contract it is for waits, then dispatches on its
/// answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_reground_is_one_call_and_another_contract_merges_meanwhile() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &[]),
    ]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(20),
    );
    let release = p.grounder.hold("Next step `b`");
    let o = Orchestrator::new(concurrent(), p.services(WINDOW)).expect("orchestrator");
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a and b".into()));
    until("b's re-grounding is held", || p.grounder.calls() == 1).await;
    let started = Instant::now();
    while !record_of(&p).is_verified("a") {
        assert!(!run.is_finished(), "the run ended");
        assert!(started.elapsed() < HANG_GUARD, "a never merged");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Many loop steps (the tick is 10 ms) pass with the call held.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reground_rows = |p: &Parts| {
        p.ledger
            .calls()
            .expect("calls")
            .into_iter()
            .filter(|c| c.trace.kind.as_deref() == Some("reground"))
            .count()
    };
    assert_eq!(reground_rows(&p), 1, "one call in flight per key");
    assert_eq!(p.grounder.calls(), 1);
    assert!(!record_of(&p).is_verified("b"), "b waits for its answer");
    release.add_permits(1);
    let report = tokio::time::timeout(HANG_GUARD, run)
        .await
        .expect("the run ends")
        .expect("join")
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let record = record_of(&p);
    assert_eq!(record.groundings.len(), 1);
    assert_eq!(record.groundings[0].contract, "b");
    assert!(record.groundings[0].serves);
    assert_eq!(reground_rows(&p), 1);
    let trace = trace_of(&p).await;
    assert!(trace.iter().any(|e| matches!(
        e,
        TraceEvent::PlannerTask { kind, subject, outcome, .. }
            if kind == "reground" && subject == "b" && outcome == "answered"
    )));
}

/// A kill after a planner answer arrived and before the loop took it in:
/// the resume takes the answer from the intake table and makes no call for
/// it, and its ledger call is charged once (the resume's ledger seeding
/// charged it already).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_planner_answer_left_by_a_kill_is_used_once_and_charged_once() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let planner = {
        let plan = plan.clone();
        move |_: &str| plan.clone()
    };
    // The killed process: planned, `a` in flight, then stopped.
    {
        let p = Parts::open_with(
            dir.path(),
            planner.clone(),
            |_| SERVES.into(),
            Duration::from_millis(20),
            true,
        );
        let _held = p.hold_workers("make a");
        let o = Orchestrator::new(concurrent(), p.services(WINDOW)).expect("orchestrator");
        let handle = o.handle();
        let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
        handle
            .published(|s| s.status.as_ref().is_some_and(|s| !s.in_flight.is_empty()))
            .await;
        handle.stop().await;
        let report = run.await.expect("join").expect("run");
        assert_eq!(report.status, RunStatus::Stopped);
        // The resume's re-grounding of `a`, answered before the kill: its
        // ledger row, and the answer the task appended to the intake table.
        let ledger = p.ledger.clone();
        let call = tokio::task::spawn_blocking(move || {
            let token = ledger
                .begin(
                    "fake-plan",
                    kgraf_core::Protocol::Additive,
                    PLAN_MODEL,
                    "orchestrator:r1",
                    "r1",
                )
                .expect("begin");
            let id = token.id().clone();
            token
                .settle(
                    &kgraf_core::TokenUsage {
                        // Dear on purpose: a second charge cannot hide among
                        // the run's other spend.
                        uncached_input: 200_000_000,
                        text_output: 50,
                        ..Default::default()
                    },
                    None,
                )
                .expect("settle");
            id
        })
        .await
        .expect("join");
        let body = serde_json::json!({
            "key": {"kind": "reground", "subject": "a", "generation": 0},
            "answer": {"Reground": {
                "verdict": {"Ok": [true, "on track", call, []]},
                "calls": [call],
                "note_ids": [],
                "trigger": "resume"
            }}
        })
        .to_string();
        p.graph
            .lock()
            .expect("graph")
            .append_intake(&kgraf_graph::NewIntakeRow {
                run: "r1",
                kind: "planner_answer",
                target: "reground:a",
                channel: "planner",
                message_id: "",
                received_ms: now_ms(),
                body: &body,
            })
            .expect("append");
    }
    // The next process.
    let p = Parts::open_with(
        dir.path(),
        planner,
        |_| SERVES.into(),
        Duration::from_millis(20),
        true,
    );
    let held = p.hold_workers("make a");
    let o = Orchestrator::new(concurrent(), p.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.resume(RunId::new("r1")));
    handle
        .published(|s| s.status.as_ref().is_some_and(|s| !s.in_flight.is_empty()))
        .await;
    assert_eq!(
        p.grounder.calls(),
        0,
        "the answer was used, not asked again"
    );
    let dear = p
        .ledger
        .calls()
        .expect("calls")
        .into_iter()
        .filter(|c| c.usage.uncached_input == 200_000_000)
        .map(|c| c.id)
        .collect::<Vec<_>>();
    let cost = p.ledger.spend_of(&dear, &prices()).expect("spend").total;
    let spent = p.admission.budget().spent_run("r1");
    assert!(
        spent >= cost && spent.micros() < 2 * cost.micros(),
        "charged once: spent {spent}, the answer's call {cost}"
    );
    held.add_permits(100);
    let report = tokio::time::timeout(HANG_GUARD, run)
        .await
        .expect("the run ends")
        .expect("join")
        .expect("resume");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let record = record_of(&p);
    assert_eq!(record.groundings.len(), 1, "{:?}", record.groundings);
    assert_eq!(p.grounder.calls(), 0);
}

/// The box's question cards never wait for the loop: with the loop inside a
/// held `decompose` call, a read from the UI thread returns.
#[test]
fn the_box_reads_its_question_cards_while_the_loop_is_in_a_planner_call() {
    use kgraf_orchestrator::InteractiveRun;
    struct Factory(Parts);
    #[async_trait::async_trait]
    impl kgraf_orchestrator::OrchestratorFactory for Factory {
        async fn configured(&self) -> Result<(), kgraf_orchestrator::ReadinessFailure> {
            Ok(())
        }
        fn build(
            &self,
            _run: &RunId,
        ) -> Result<Orchestrator, kgraf_orchestrator::OrchestratorError> {
            Orchestrator::new(config(), self.0.services(WINDOW))
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let parts = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let release = parts.planner.hold("make a");
    let planner = parts.planner.clone();
    let factory = Arc::new(Factory(parts));
    let run = runtime
        .block_on(InteractiveRun::start(
            runtime.handle().clone(),
            factory,
            RunId::new("r1"),
            "make a".into(),
            Arc::new(|| {}),
        ))
        .expect("started");
    let started = Instant::now();
    while planner.calls() == 0 {
        assert!(started.elapsed() < HANG_GUARD, "decompose never started");
        std::thread::sleep(Duration::from_millis(2));
    }
    // A frame: the read runs on a thread of its own, so a read that waited
    // for the loop shows as no answer rather than a hung test.
    let (tx, rx) = std::sync::mpsc::channel();
    let ui = std::thread::spawn(move || {
        let _ = tx.send(run.pending_questions());
        run
    });
    let read = rx.recv_timeout(Duration::from_secs(5));
    // Released either way, so a read that waited can return.
    release.add_permits(1);
    let mut run = ui.join().expect("ui thread");
    assert!(
        read.is_ok_and(|q| q.is_empty()),
        "the read waited for the loop"
    );
    assert!(run.snapshot().run.is_some());
    run.stop();
    let started = Instant::now();
    while !run.is_finished() {
        assert!(started.elapsed() < HANG_GUARD, "the run did not end");
        run.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The record as a reader that holds no lock of the run sees it.
fn record_on_disk(dir: &std::path::Path) -> Option<kgraf_orchestrator::RunStateRecord> {
    let g = kgraf_graph::GraphStore::open_read_only(&dir.join("graph.db"), dir).ok()?;
    RunStore::new(Arc::new(std::sync::Mutex::new(g)))
        .load(&RunId::new("r1"))
        .ok()
        .flatten()
}

fn wait_std(what: &str, f: impl Fn() -> bool) {
    let started = Instant::now();
    while !f() {
        assert!(started.elapsed() < HANG_GUARD, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

fn planner_rows(dir: &std::path::Path) -> Vec<kgraf_graph::IntakeRow> {
    kgraf_graph::IntakeWriter::open(&dir.join("graph.db"))
        .expect("reader")
        .after("r1", 0)
        .expect("rows")
        .into_iter()
        .filter(|r| r.kind == "planner_answer")
        .collect()
}

/// The first process of the kill tests: `a` merged, `b`'s re-grounding
/// held, with the planner calls as tasks.
fn first_process(
    dir: &std::path::Path,
) -> (tokio::runtime::Runtime, Parts, Arc<tokio::sync::Semaphore>) {
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &[]),
    ]);
    let rt = runtime();
    let p = Parts::open_with(
        dir,
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(20),
        true,
    );
    let release = p.grounder.hold("Next step `b`");
    let o = Orchestrator::new(concurrent(), p.services(WINDOW)).expect("orchestrator");
    rt.spawn(o.start(RunId::new("r1"), "make a and b".into()));
    wait_std("b's re-grounding is held", || p.grounder.calls() == 1);
    wait_std("a merged and saved", || {
        record_on_disk(dir).is_some_and(|r| r.is_verified("a"))
    });
    (rt, p, release)
}

/// What a killed process leaves on disk: a copy of its directory taken while
/// it no longer writes (its loop is stopped for good, or idle and dropped).
/// The next process resumes in the copy, as one would on the same disk.
fn snapshot(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for entry in std::fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            snapshot(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// The next process: resumes the run.
fn next_process(dir: &std::path::Path) -> (Parts, kgraf_orchestrator::RunReport) {
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &[]),
    ]);
    let p = Parts::open_with(
        dir,
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(20),
        true,
    );
    release_killed_leases(&p);
    let o = Orchestrator::new(concurrent(), p.services(WINDOW)).expect("orchestrator");
    let rt = runtime();
    let report = rt
        .block_on(async { tokio::time::timeout(HANG_GUARD, o.resume(RunId::new("r1"))).await })
        .expect("the resume ends")
        .expect("resume");
    rt.shutdown_background();
    (p, report)
}

/// A kill after the answer arrived through its task and before what it
/// changed was saved (the loop is stopped for good at its next save, as a
/// killed process is): the resume uses the answer from the intake table, with
/// no new call, and records it once.
#[test]
fn a_kill_before_the_answer_is_saved_uses_it_on_resume_without_a_new_call() {
    let dir = tempfile::tempdir().expect("dir");
    let (rt, p, release) = first_process(dir.path());
    // Every save of the first process waits from here, for good.
    let guard = p.graph.lock().expect("graph");
    release.add_permits(1);
    wait_std("the answer is in the intake table", || {
        !planner_rows(dir.path()).is_empty()
    });
    std::thread::sleep(Duration::from_millis(300));
    let copy = tempfile::tempdir().expect("copy");
    snapshot(dir.path(), copy.path());
    rt.shutdown_background();
    std::mem::forget(guard);
    let dir = copy;
    assert!(
        record_on_disk(dir.path()).is_some_and(|r| r.groundings.is_empty()),
        "nothing of the answer was saved before the kill"
    );
    let (p2, report) = next_process(dir.path());
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(
        p2.grounder.calls(),
        0,
        "the answer was used, not asked again"
    );
    let record = record_on_disk(dir.path()).expect("record");
    let b: Vec<_> = record
        .groundings
        .iter()
        .filter(|g| g.contract == "b")
        .collect();
    assert_eq!(b.len(), 1, "{:?}", record.groundings);
    drop(p);
}

/// A kill after the answer was used and saved: the resume does not use it a
/// second time (its own re-grounding is a new call, with a call of its own).
#[test]
fn a_kill_after_the_answer_is_used_does_not_use_it_again() {
    let dir = tempfile::tempdir().expect("dir");
    let (rt, p, release) = first_process(dir.path());
    let held_b = p.hold_workers("make b");
    release.add_permits(1);
    wait_std("the answer is used and saved", || {
        let rows = planner_rows(dir.path());
        record_on_disk(dir.path()).is_some_and(|r| {
            r.groundings.iter().any(|g| g.contract == "b")
                && rows.iter().all(|row| {
                    r.intake_above.contains(&row.id) || r.intake_folded.is_some_and(|f| f >= row.id)
                })
                && !rows.is_empty()
        })
    });
    let copy = tempfile::tempdir().expect("copy");
    snapshot(dir.path(), copy.path());
    rt.shutdown_background();
    held_b.add_permits(1_000);
    let dir = copy;
    let (p2, report) = next_process(dir.path());
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let record = record_on_disk(dir.path()).expect("record");
    let calls: Vec<_> = record
        .groundings
        .iter()
        .filter(|g| g.contract == "b")
        .map(|g| g.call.clone())
        .collect();
    let mut unique = calls.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        calls.len(),
        "an answer used twice: {:?}",
        record.groundings
    );
    assert_eq!(
        p2.grounder.calls(),
        calls.len() - 1,
        "the resume's own re-grounding only"
    );
    drop(p);
}

/// A re-grounding whose task dies (the model call panics) gives back the
/// cadence it took: its contract asks again and is dispatched only after an
/// answer, never ungrounded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reground_that_dies_is_asked_again_before_its_contract_dispatches() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b", "src/b.rs", &[]),
    ]);
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n = asked.clone();
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        move |_| {
            if n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                panic!("the grounding model panicked");
            }
            SERVES.into()
        },
        Duration::from_millis(20),
    );
    let o = Orchestrator::new(concurrent(), p.services(WINDOW)).expect("orchestrator");
    let report = tokio::time::timeout(HANG_GUARD, o.start(RunId::new("r1"), "make a and b".into()))
        .await
        .expect("the run ends")
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "asked again"
    );
    let record = record_of(&p);
    assert!(
        record
            .groundings
            .iter()
            .any(|g| g.contract == "b" && g.serves),
        "b was grounded before it dispatched: {:?}",
        record.groundings
    );
}
