//! Decision 0067, across a process restart: "a kill
//! between persisting and injecting delivers exactly once after resume", a
//! kill between the transcript append and the inbox mark sends nothing
//! twice, and a stopped session stays held across a restart until
//! `resume_session`. The kill is a tokio runtime shut down in the background
//! with work in flight, as in `restart.rs`.

mod support;

use kgraf_core::session::{InboxKind, InjectMode, SessionRecords};
use kgraf_core::{AgentId, RunId};
use kgraf_graph::sessions::SharedSessions;
use kgraf_orchestrator::{ContractStatus, Orchestrator, OrchestratorConfig, RunStatus, RunStore};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use support::*;

const WINDOW: u64 = 200_000;
/// Short leases, so the killed process's run lease lapses in the test's time.
const LEASE_TTL_MS: i64 = 300;
const NOTE: &str = "prefer small functions";

fn cfg() -> OrchestratorConfig {
    let mut c = config();
    c.lease_ttl_ms = LEASE_TTL_MS;
    c
}

fn after_kill() {
    std::thread::sleep(Duration::from_millis(LEASE_TTL_MS as u64 + 150));
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn transcript(dir: &std::path::Path, worker: &str) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("runs/r1/workers").join(format!("{worker}.jsonl")))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The transcript sequence numbers of the `Injected` events carrying `id`.
fn injected_seqs(dir: &std::path::Path, worker: &str, id: &str) -> Vec<u64> {
    transcript(dir, worker)
        .iter()
        .filter(|l| {
            let t = l.to_string();
            t.contains(r#""type":"injected""#) && t.contains(id)
        })
        .filter_map(|l| l.get("seq").and_then(serde_json::Value::as_u64))
        .collect()
}

fn delivered_seq(dir: &std::path::Path, id: &str) -> Option<i64> {
    let db = rusqlite::Connection::open(dir.join("graph.db")).expect("db");
    db.query_row(
        "SELECT delivered_seq FROM session_inbox WHERE id = ?1",
        [id],
        |r| r.get::<_, Option<i64>>(0),
    )
    .expect("the inbox row")
}

/// The second process: resume the run over the same files and wait for it.
fn resume(dir: &std::path::Path) -> kgraf_orchestrator::RunReport {
    let second = runtime();
    let p2 = Parts::open_with(
        dir,
        |_| panic!("the resumed run must not decompose again"),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    second
        .block_on(async {
            Orchestrator::new(cfg(), p2.services(WINDOW))
                .expect("orchestrator")
                .resume(RunId::new("r1"))
                .await
        })
        .expect("resume")
}

/// The spec test: the note is persisted and acknowledged while the worker
/// is parked mid-round (it reaches no boundary), then the process is killed.
/// The resumed worker gets it at its first boundary: exactly one `Injected`
/// in the transcript, and the inbox row marked at that sequence.
#[test]
fn a_kill_between_persisting_and_injecting_delivers_exactly_once_after_resume() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a HANG", "src/a.rs", &[])]);
    let first = runtime();
    let p1 = Parts::open_with(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    p1.alpha.hang.store(true, Ordering::SeqCst);
    p1.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(cfg(), p1.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    first.spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_for("the worker parks in round 1", || {
        p1.alpha.hang_reached.load(Ordering::SeqCst) || p1.beta.hang_reached.load(Ordering::SeqCst)
    });
    let worker = AgentId::new("r1-a-r0-a0");
    let id = first
        .block_on(handle.send_to_session(
            worker.clone(),
            InboxKind::Note,
            InjectMode::AtBoundary,
            NOTE.into(),
        ))
        .expect("persisted and acknowledged");
    assert_eq!(delivered_seq(dir.path(), &id), None, "not delivered yet");
    assert!(injected_seqs(dir.path(), worker.as_str(), &id).is_empty());

    first.shutdown_background();
    drop(p1);
    after_kill();

    let report = resume(dir.path());
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let seqs = injected_seqs(dir.path(), worker.as_str(), &id);
    assert_eq!(seqs.len(), 1, "delivered exactly once: {seqs:?}");
    assert_eq!(
        delivered_seq(dir.path(), &id),
        Some(seqs[0] as i64),
        "the inbox row names the transcript sequence that carried it"
    );
}

/// A kill after the `Injected` event reached the transcript but before
/// its inbox row was marked (simulated by clearing the mark, which is exactly
/// the state such a kill leaves). The resume reads
/// `WorkerHandle::already_delivered()` and marks the row at the recorded
/// sequence; nothing is injected a second time.
#[test]
fn a_kill_after_the_transcript_append_marks_the_row_and_sends_nothing_twice() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a HANG", "src/a.rs", &[])]);
    let first = runtime();
    // Round 0 is slow, so the note sent during it is injected at the boundary
    // that follows; round 1 then parks.
    let p1 = Parts::open_with(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(400),
        true,
    );
    p1.alpha.hang.store(true, Ordering::SeqCst);
    p1.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(cfg(), p1.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    first.spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_for("round 0 is in flight", || !p1.stats.requests().is_empty());
    let worker = AgentId::new("r1-a-r0-a0");
    let id = first
        .block_on(handle.send_to_session(
            worker.clone(),
            InboxKind::Note,
            InjectMode::AtBoundary,
            NOTE.into(),
        ))
        .expect("persisted");
    wait_for("the note is in the transcript and round 1 parks", || {
        !injected_seqs(dir.path(), worker.as_str(), &id).is_empty()
            && (p1.alpha.hang_reached.load(Ordering::SeqCst)
                || p1.beta.hang_reached.load(Ordering::SeqCst))
    });
    let seq = injected_seqs(dir.path(), worker.as_str(), &id)[0];
    wait_for("the row is marked", || {
        delivered_seq(dir.path(), &id).is_some()
    });

    first.shutdown_background();
    drop(p1);
    after_kill();
    // The state a kill between the transcript append and the inbox mark
    // leaves behind.
    let db = rusqlite::Connection::open(dir.path().join("graph.db")).expect("db");
    db.execute(
        "UPDATE session_inbox SET delivered_seq = NULL, delivered_ms = NULL WHERE id = ?1",
        [&id],
    )
    .expect("clear the mark");
    drop(db);
    assert!(
        SharedSessions(std::sync::Arc::new(std::sync::Mutex::new(
            kgraf_graph::GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph")
        )))
        .undelivered(&worker)
        .expect("inbox")
        .iter()
        .any(|i| i.id == id),
        "the row reads as undelivered, as after such a kill"
    );

    let report = resume(dir.path());
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(
        injected_seqs(dir.path(), worker.as_str(), &id),
        vec![seq],
        "nothing was injected a second time"
    );
    assert_eq!(
        delivered_seq(dir.path(), &id),
        Some(seq as i64),
        "marked at the sequence the transcript already carried"
    );
}

/// Stop a session, stop the process, start it again and press play: the
/// stopped session stays held, the run waits on its live loop, and only
/// `resume_session` continues it (from its transcript, no round repeated).
#[test]
fn a_stopped_session_stays_held_across_a_restart_until_resume_session() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a HANG", "src/a.rs", &[])]);
    let first = runtime();
    let p1 = Parts::open_with(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    p1.alpha.hang.store(true, Ordering::SeqCst);
    p1.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(cfg(), p1.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = first.spawn(o.start(RunId::new("r1"), "make a".into()));
    wait_for("the worker parks", || {
        p1.alpha.hang_reached.load(Ordering::SeqCst) || p1.beta.hang_reached.load(Ordering::SeqCst)
    });
    let worker = AgentId::new("r1-a-r0-a0");
    first
        .block_on(handle.stop_session(worker.clone()))
        .expect("stop the session");
    let store = RunStore::new(p1.graph.clone());
    wait_for("the stopped attempt ends", || {
        p1.alpha.live.lock().expect("lock").is_empty()
            && p1.beta.live.lock().expect("lock").is_empty()
    });
    first.block_on(handle.stop());
    let report = first
        .block_on(run)
        .expect("join")
        .expect("the first process stops");
    assert_eq!(report.status, RunStatus::Stopped);
    drop(store);
    drop(first);
    drop(p1);
    after_kill();

    // ---- the second process: press play.
    let second = runtime();
    let p2 = Parts::open_with(
        dir.path(),
        |_| panic!("the resumed run must not decompose again"),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    let o = Orchestrator::new(cfg(), p2.services(WINDOW)).expect("orchestrator");
    let handle = o.handle();
    let run = second.spawn(o.resume(RunId::new("r1")));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!run.is_finished(), "the run waits for the stopped session");
    assert!(
        p2.stats.requests().is_empty(),
        "no worker was dispatched: {:?}",
        p2.stats.requests()
    );
    let record = RunStore::new(p2.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    let a = record
        .open_contracts
        .iter()
        .find(|c| c.contract == "a")
        .expect("a");
    assert!(
        matches!(&a.status, ContractStatus::Held { reason } if reason.contains("stopped by the user")),
        "{:?}",
        a.status
    );
    assert_eq!(a.stopped_by_user, vec![worker.clone()]);

    second
        .block_on(handle.resume_session(worker.clone()))
        .expect("resume the session");
    let report = second.block_on(run).expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert!(
        p2.stats.requests().iter().all(|r| r.2 >= 1),
        "the session continued its transcript: round 0 was not asked again: {:?}",
        p2.stats.requests()
    );
}
