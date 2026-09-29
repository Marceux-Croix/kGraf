//! Decision 0073: a merge conflict gets a resolver worker
//! before a re-run.
//!
//! `kgraf-host`'s `tests.rs`
//! (`a_real_git_conflicts_attempt_is_kept_until_a_resolver_settles_it`)
//! exercises the real git side (a genuine `git merge` conflict, a kept
//! attempt ref, a resolver worktree at the new tip whose merge lands on the
//! integration branch). These tests exercise the orchestrator's own dispatch,
//! resume and session bookkeeping against the fakes `coordination.rs` already
//! uses for `Integration::Conflict`.

mod support;

use kgraf_core::RunId;
use kgraf_core::session::{InboxKind, InjectMode, SessionKind, SessionRecords};
use kgraf_graph::sessions::SharedSessions;
use kgraf_orchestrator::{
    ContractStatus, Integration, Orchestrator, RunStatus, RunStore, SessionError, TraceEvent,
    TraceLog, WaitKind,
};
use std::sync::atomic::Ordering;
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

fn record_of(p: &Parts) -> kgraf_orchestrator::RunStateRecord {
    RunStore::new(p.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record")
}

fn sessions(p: &Parts) -> SharedSessions {
    SharedSessions(p.graph.clone())
}

async fn trace_of(p: &Parts) -> Vec<TraceEvent> {
    TraceLog::open(&p.dir.join("runs").join("r1").join("trace.jsonl"))
        .await
        .expect("trace")
        .load()
        .await
        .expect("load")
}

/// **Mutation target**: a conflict must dispatch a resolver, not a bare
/// re-run of the same contract. A mutant that deletes the resolver branch (or
/// that re-dispatches `a` directly, as 0018 did) still passes the run to
/// completion -- the fakes always merge on a second try -- so this asserts
/// the *shape* of what happened, not only the end state: exactly one round-0
/// attempt of `a` itself, a second round-0 attempt under a different
/// (resolver) contract id, that resolver's session is `Resolver`-kind and
/// non-addressable, its merge is the one that reaches the integration
/// branch, and `a` is never dispatched a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_merge_conflict_dispatches_a_resolver_worker_not_a_rerun() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let integration = FakeIntegration::new(dir.path().join("worktrees"));
    integration
        .conflict_once
        .lock()
        .expect("lock")
        .insert("a".into());
    let mut cfg = config();
    cfg.max_retries = 2;
    let mut services = p.services(WINDOW);
    services.workspaces = integration.clone();
    let o = Orchestrator::new(cfg, services).expect("orchestrator");
    let handle = o.handle();
    let report = o
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");

    // `a` itself was attempted exactly once: the conflict never re-dispatched
    // it directly (the pre-0073 behavior this test would catch a regression
    // to).
    let a_attempts = p
        .stats
        .requests()
        .iter()
        .filter(|r| r.1.starts_with("make a") && r.2 == 0)
        .count();
    assert_eq!(a_attempts, 1, "a is dispatched once, not re-run");

    let record = record_of(&p);
    // A resolver contract ran, scoped to the conflicted path, and closed --
    // it never lingers as its own open or verified contract.
    assert!(
        record.verified_contracts.iter().any(|c| c == "a"),
        "the original contract is verified: {:?}",
        record.verified_contracts
    );
    assert!(
        !record
            .verified_contracts
            .iter()
            .any(|c| c.contains("resolver")),
        "the resolver never counts as its own verified contract: {:?}",
        record.verified_contracts
    );
    assert!(
        record
            .open_contracts
            .iter()
            .all(|c| c.resolver_for.is_none()),
        "no resolver contract is left open"
    );
    // `a`'s own integration entry is the resolver's merge, not a second plain
    // attempt of `a`.
    assert!(matches!(
        record.coordination.integration.get("a"),
        Some(Integration::Merged { .. })
    ));
    assert_eq!(
        record.coordination.integration_rejections, 1,
        "one conflict, recorded once"
    );

    // The resolver's own merge landed on the (fake) integration branch: two
    // merges total, `a`'s round never repeated verbatim.
    let merges = integration.merges.lock().expect("lock").clone();
    assert_eq!(merges.len(), 1, "one merge reaches the branch: {merges:?}");

    // The conflicted attempt's kept workspace was discarded once settled.
    assert_eq!(
        integration.conflicts_discarded.lock().expect("lock").len(),
        1,
        "the superseded attempt is discarded exactly once"
    );
    assert!(integration.conflicted.lock().expect("lock").is_empty());

    // The trace shows the conflict and the resolution, never a second
    // conflict.
    let trace = trace_of(&p).await;
    let conflicts = trace
        .iter()
        .filter(|e| matches!(e, TraceEvent::Integration { outcome, .. } if outcome == "conflict"))
        .count();
    assert_eq!(conflicts, 1, "{trace:?}");
    assert!(
        trace
            .iter()
            .any(|e| matches!(e, TraceEvent::Integration { outcome, .. } if outcome == "resolved")),
        "{trace:?}"
    );

    // The resolver's own session was recorded as `Resolver` kind and
    // non-addressable, its contract id under the original's (`a_note_to_a_
    // resolver_is_refused_while_it_runs`, below, exercises the live refusal).
    let facts = sessions(&p)
        .sessions_of_run(&RunId::new("r1"))
        .expect("sessions");
    let resolver = facts
        .iter()
        .find(|f| f.kind == SessionKind::Resolver)
        .expect("a resolver session was recorded");
    assert!(
        !resolver.addressable,
        "a resolver session is never addressable"
    );
    assert!(
        record.coordination.closed.iter().any(|c| c.contract == "a"),
        "the original contract is closed as verified: {:?}",
        record.coordination.closed
    );
    assert!(
        resolver
            .contract
            .as_deref()
            .is_some_and(|c| c.starts_with("a.resolver")),
        "{:?}",
        resolver.contract
    );
    // The handle can no longer be exercised once the run has fully ended
    // (`SessionError::Stopped`, an unrelated fact): the live refusal is
    // exercised below while a resolver is still running.
    drop(handle);
}

/// A resolver's session view has no message panel
/// and no input, and a note sent to it directly is refused while it runs
/// (not merely after it ends, when every session refuses one). The resolver
/// is parked mid-round (`WorkerFake`'s `HANG`, embedded in the conflict's own
/// detail text, which only a resolver's brief ever quotes) so there is a real
/// window to address it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_to_a_resolver_is_refused_while_it_runs() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let integration = FakeIntegration::new(dir.path().join("worktrees"));
    integration
        .conflict_once
        .lock()
        .expect("lock")
        .insert("a".into());
    *integration.conflict_detail.lock().expect("lock") = "CONFLICT (content) HANG".into();
    p.alpha
        .hang
        .store(true, std::sync::atomic::Ordering::SeqCst);
    p.beta.hang.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut cfg = config();
    cfg.max_retries = 2;
    let mut services = p.services(WINDOW);
    services.workspaces = integration.clone();
    let o = Orchestrator::new(cfg, services).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    while !(p
        .alpha
        .hang_reached
        .load(std::sync::atomic::Ordering::SeqCst)
        || p.beta
            .hang_reached
            .load(std::sync::atomic::Ordering::SeqCst))
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The resolver is now parked at round 1, after its round-0 write: its
    // session row exists and its instruction carries the conflict's detail
    // (proof this is the resolver's own call, not `a`'s: `a`'s instruction
    // never contains `HANG`).
    let resolver = loop {
        let facts = sessions(&p)
            .sessions_of_run(&RunId::new("r1"))
            .expect("sessions");
        if let Some(f) = facts.into_iter().find(|f| f.kind == SessionKind::Resolver) {
            break f;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert!(!resolver.addressable, "a resolver is never addressable");
    let err = handle
        .send_to_session(
            resolver.id.clone(),
            InboxKind::Note,
            InjectMode::AtBoundary,
            "hello".into(),
        )
        .await
        .expect_err("a note to a running resolver is refused (request 17 addendum)");
    assert!(matches!(err, SessionError::Refused(_)), "{err:?}");
    let stop_err = handle
        .stop_session(resolver.id.clone())
        .await
        .expect_err("a resolver also refuses a direct stop");
    assert!(matches!(stop_err, SessionError::Refused(_)), "{stop_err:?}");

    // The parked call only returns once `stop()` ends the run (the same
    // shutdown `sessions_addressable.rs`'s own `HANG` tests use): a resolver refuses a
    // direct stop (just asserted), so a per-session stop cannot be used to
    // unwind it, and this test's job -- the refusal while it runs -- is
    // already done.
    p.alpha
        .hang
        .store(false, std::sync::atomic::Ordering::SeqCst);
    p.beta
        .hang
        .store(false, std::sync::atomic::Ordering::SeqCst);
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// Only when the resolver itself fails (here: its own merge conflicts again)
/// does the original contract fall back to a plain re-dispatch from the new
/// tip -- 0018's original behavior, now the fallback rather than the rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_resolver_falls_back_to_a_plain_re_dispatch() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let integration = FakeIntegration::new(dir.path().join("worktrees"));
    integration
        .conflict_once
        .lock()
        .expect("lock")
        .insert("a".into());
    // The resolver's own deterministic id is `a.resolver1` (the run's first
    // conflict): make its merge conflict too, so it fails and the fallback
    // fires.
    integration
        .conflict_once
        .lock()
        .expect("lock")
        .insert("a.resolver1".into());
    let mut cfg = config();
    cfg.max_retries = 3;
    let mut services = p.services(WINDOW);
    services.workspaces = integration.clone();
    let o = Orchestrator::new(cfg, services).expect("orchestrator");
    let report = o
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("run");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");

    let record = record_of(&p);
    assert!(record.verified_contracts.iter().any(|c| c == "a"));
    // `a` ran again in the end (the fallback), so its own instruction was
    // attempted more than once.
    let a_attempts = p
        .stats
        .requests()
        .iter()
        .filter(|r| r.1.starts_with("make a") && r.2 == 0)
        .count();
    assert!(
        a_attempts >= 2,
        "the fallback re-dispatches a: {a_attempts}"
    );
    assert_eq!(
        record.coordination.integration_rejections, 2,
        "two conflicts: the original attempt's, and the failed resolver's"
    );
    let trace = trace_of(&p).await;
    assert!(
        trace.iter().any(
            |e| matches!(e, TraceEvent::Integration { outcome, .. } if outcome == "resolver_failed")
        ),
        "{trace:?}"
    );
    let waits_saw_resolver = trace
        .iter()
        .any(|e| matches!(e, TraceEvent::Wait { wait, .. } if *wait == WaitKind::Resolver));
    assert!(waits_saw_resolver, "{trace:?}");
}

/// **Mutation target**: while a resolver is open, `on_resume` must not flip
/// the contract it resolves back to `Planned` . Before the fix, `on_resume` flipped every
/// `Dispatched` contract unconditionally; a stop-and-resume mid-resolver then re-dispatched `a`
/// under the same lease holder its own resolver was still using (`eligible()` has no shared-holder
/// check, and lease acquire treats the same holder as a renewal, so nothing refused the second
/// worker): two live workers writing `src/a.rs` at once, a disjoint-writes violation. This test
/// parks the resolver mid-round (the same `HANG` technique as
/// `a_note_to_a_resolver_is_refused_while_it_runs`), stops the run, resumes it in a fresh process's
/// `Orchestrator`, and asserts `a` is still not dispatched while the (now doubly-resumed) resolver
/// is still the only worker running for that holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_and_resume_mid_resolver_never_re_dispatches_the_original() {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);
    // In-memory ledger (one process, like `a_stopped_run_keeps_its_workers_
    // and_resumes_to_completion`): a real cross-process resume is the kill
    // test below, which needs the disk ledger for that reason alone.
    let p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let integration = FakeIntegration::new(dir.path().join("worktrees"));
    integration
        .conflict_once
        .lock()
        .expect("lock")
        .insert("a".into());
    *integration.conflict_detail.lock().expect("lock") = "CONFLICT (content) HANG".into();
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let mut services = p.services(WINDOW);
    services.workspaces = integration.clone();
    let o = Orchestrator::new(config(), services).expect("orchestrator");
    let handle = o.handle();
    let run = tokio::spawn(o.start(RunId::new("r1"), "make a".into()));
    while !(p.alpha.hang_reached.load(Ordering::SeqCst)
        || p.beta.hang_reached.load(Ordering::SeqCst))
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    handle.stop().await;
    let report = run.await.expect("join").expect("run");
    assert_eq!(report.status, RunStatus::Stopped, "{report:?}");

    let holder_workers = |r: &kgraf_orchestrator::RunStateRecord| -> Vec<String> {
        r.open_contracts
            .iter()
            .filter(|c| c.lineage.first().map(String::as_str) == Some("a"))
            .flat_map(|c| c.workers.iter().map(ToString::to_string))
            .collect()
    };
    let before = record_of(&p);
    let a = before.open_contracts.iter().find(|c| c.contract == "a");
    assert_eq!(
        a.map(|c| &c.status),
        Some(&ContractStatus::Dispatched),
        "the conflicted contract stays Dispatched, not re-eligible, while its \
         resolver is open"
    );
    assert!(
        a.is_some_and(|c| c.workers.is_empty()),
        "its own worker never comes back until the resolver settles"
    );
    assert_eq!(
        holder_workers(&before).len(),
        1,
        "one worker (the resolver's) for the shared holder before resume: {:?}",
        holder_workers(&before)
    );

    // Resume in a fresh `Orchestrator`, without unhanging first: the
    // resolver's transcript replays its completed round 0 and parks again at
    // round 1 (still `hang: true`), giving a second window to inspect `a`
    // mid-resume before this second run is itself stopped (the parked call
    // only ever returns once its run is stopped, never on its own -- the same
    // reason `a_note_to_a_resolver_is_refused_while_it_runs` never just lets
    // its own hung run finish).
    p.alpha.hang_reached.store(false, Ordering::SeqCst);
    p.beta.hang_reached.store(false, Ordering::SeqCst);
    let o2 = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let handle2 = o2.handle();
    let resumed = tokio::spawn(o2.resume(RunId::new("r1")));
    while !(p.alpha.hang_reached.load(Ordering::SeqCst)
        || p.beta.hang_reached.load(Ordering::SeqCst))
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mid_resume = record_of(&p);
    let a = mid_resume.open_contracts.iter().find(|c| c.contract == "a");
    assert_eq!(
        a.map(|c| &c.status),
        Some(&ContractStatus::Dispatched),
        "still not re-dispatched partway through resume"
    );
    assert!(a.is_some_and(|c| c.workers.is_empty()));
    assert_eq!(
        holder_workers(&mid_resume).len(),
        1,
        "exactly one live worker for the shared holder mid-resume (the \
         resumed resolver's): {:?}",
        holder_workers(&mid_resume)
    );
    handle2.stop().await;
    let report = resumed.await.expect("join").expect("resume");
    assert_eq!(report.status, RunStatus::Stopped, "{report:?}");

    // A third, unhanged resume actually finishes the resolver's round.
    p.alpha.hang.store(false, Ordering::SeqCst);
    p.beta.hang.store(false, Ordering::SeqCst);
    let o3 = Orchestrator::new(config(), p.services(WINDOW)).expect("orchestrator");
    let report = o3.resume(RunId::new("r1")).await.expect("resume");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let after = record_of(&p);
    assert!(after.verified_contracts.iter().any(|c| c == "a"));
    let a_attempts = p
        .stats
        .requests()
        .iter()
        .filter(|r| r.1.starts_with("make a") && r.2 == 0)
        .count();
    assert_eq!(
        a_attempts, 1,
        "a is never dispatched a second time across the stop and resume"
    );
}

/// As the test above, but a real process death: a separate tokio runtime is
/// shut down with the resolver parked mid-round, and a brand new process
/// (fresh `Parts`, same on-disk ledger and graph) resumes the run. Real
/// restart mechanics (`restart.rs`'s own technique) exercise the same
/// `on_resume` fix a graceful stop does not distinguish from a kill: a killed
/// process leaves `a` and its resolver exactly as a stopped one does.
#[test]
fn a_kill_and_resume_mid_resolver_never_re_dispatches_the_original() {
    // A short run-lease TTL (`restart.rs`'s own technique), so the killed
    // process's lease has genuinely lapsed by the time the second process
    // resumes, rather than being refused as still held.
    const LEASE_TTL_MS: i64 = 300;
    let mut cfg = config();
    cfg.lease_ttl_ms = LEASE_TTL_MS;
    let dir = tempfile::tempdir().expect("dir");
    let runtime = |threads: usize| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .enable_all()
            .build()
            .expect("runtime")
    };
    let plan = plan_json(&[c("a", "make a", "src/a.rs", &[])]);

    let first = runtime(2);
    let p1 = Parts::open_with(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    let integration = FakeIntegration::new(dir.path().join("worktrees"));
    integration
        .conflict_once
        .lock()
        .expect("lock")
        .insert("a".into());
    *integration.conflict_detail.lock().expect("lock") = "CONFLICT (content) HANG".into();
    p1.alpha.hang.store(true, Ordering::SeqCst);
    p1.beta.hang.store(true, Ordering::SeqCst);
    let mut services = p1.services(WINDOW);
    services.workspaces = integration;
    let o = Orchestrator::new(cfg.clone(), services).expect("orchestrator");
    first.spawn(o.start(RunId::new("r1"), "make a".into()));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !(p1.alpha.hang_reached.load(Ordering::SeqCst)
        || p1.beta.hang_reached.load(Ordering::SeqCst))
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the resolver never reached the kill point"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let store = RunStore::new(p1.graph.clone());
    let before = store
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    let a = before.open_contracts.iter().find(|c| c.contract == "a");
    assert_eq!(a.map(|c| &c.status), Some(&ContractStatus::Dispatched));
    assert!(a.is_some_and(|c| c.workers.is_empty()));

    // ---- kill: drop the whole runtime with the resolver's call in flight.
    first.shutdown_background();
    drop(store);
    drop(p1);
    std::thread::sleep(Duration::from_millis(LEASE_TTL_MS as u64 + 150));

    // ---- second process.
    let second = runtime(2);
    let p2 = Parts::open_with(
        dir.path(),
        |_| panic!("a resumed run must not decompose again"),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    let report = second
        .block_on(
            Orchestrator::new(cfg, p2.services(WINDOW))
                .expect("orchestrator")
                .resume(RunId::new("r1")),
        )
        .expect("resume");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    let record = RunStore::new(p2.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert!(record.verified_contracts.iter().any(|c| c == "a"));
    let a_attempts = p2
        .stats
        .requests()
        .iter()
        .filter(|r| r.1.starts_with("make a") && r.2 == 0)
        .count();
    assert_eq!(
        a_attempts,
        0,
        "a's own round never runs again after the kill: only the resolver's \
         interrupted round is asked again: {:?}",
        p2.stats.requests()
    );
}
