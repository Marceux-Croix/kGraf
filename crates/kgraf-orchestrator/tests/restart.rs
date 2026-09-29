//! Restart acceptance (0056): kill the process mid-run, restart,
//! and the run resumes from its record with **no completed round repeated**. The
//! round the kill interrupted never completed; it is issued again and charged, and
//! its interrupted first attempt stays charged at its worst case until reconciled.
//!
//! The kill is a tokio runtime shut down in the background with work in flight:
//! the orchestrator loop, both forwarders and a worker blocked inside a provider
//! call are dropped where they stand, as a killed process would leave them. The
//! restart opens the same graph store, ledger and run directory with fresh
//! providers, so any request the resumed run makes is visible on its own.
//!
//! MAST FM-1.3 (step repetition): no model round that completed before the kill is
//! issued again, and the plan is not re-derived.

mod support;

use kgraf_core::{Money, RunId, TokenUsage};
use kgraf_orchestrator::{GroundingTrigger, Orchestrator, OrchestratorConfig, RunStatus, RunStore};
use kgraf_provider::Settlement;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use support::*;

const WINDOW: u64 = 200_000;
/// Short leases, so the killed process's run lease lapses in the test's time.
const LEASE_TTL_MS: i64 = 300;

fn cfg() -> OrchestratorConfig {
    let mut c = config();
    c.lease_ttl_ms = LEASE_TTL_MS;
    c
}

/// Wait out the killed process's leases.
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

fn model_calls_in(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .map(|t| t.matches(r#""type":"model_call""#).count())
        .unwrap_or(0)
}

/// Run until `a` is verified and `b` is blocked inside its second model call,
/// then kill the process. Returns the ledger rows as they stood at the kill.
fn run_and_kill(dir: &std::path::Path) -> Vec<kgraf_provider::CallRecord> {
    let plan = plan_json(&[
        c("a", "make a", "src/a.rs", &[]),
        c("b", "make b HANG", "src/b.rs", &[]),
    ]);

    // ---- first process: a finishes, b is killed inside its second round.
    let first = runtime();
    let p1 = Parts::open_with(
        dir,
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    p1.alpha.hang.store(true, Ordering::SeqCst);
    p1.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(cfg(), p1.services(WINDOW)).expect("orchestrator");
    first.spawn(o.start(RunId::new("r1"), "make a and b".into()));

    let b_transcript = dir.join("runs/r1/workers").join("r1-b-r0-a0.jsonl");
    let store = RunStore::new(p1.graph.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let record = store.load(&RunId::new("r1")).expect("load");
        let a_verified = record.as_ref().is_some_and(|r| r.is_verified("a"));
        let hung = p1.alpha.hang_reached.load(Ordering::SeqCst)
            || p1.beta.hang_reached.load(Ordering::SeqCst);
        if a_verified && hung && model_calls_in(&b_transcript) == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the first run did not reach the kill point"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let before = p1.ledger.calls().expect("calls");
    let counted_before = |task: &str| {
        before
            .iter()
            .filter(|c| c.task_id == task && c.settlement == Settlement::Counted)
            .count()
    };
    assert_eq!(counted_before("r1/a"), 2);
    assert_eq!(
        counted_before("r1/b"),
        1,
        "b's first round completed before the kill"
    );
    assert_eq!(p1.planner.calls(), 1);

    // ---- kill.
    first.shutdown_background();
    drop(store);
    drop(p1);
    after_kill();
    before
}

#[test]
fn a_killed_run_resumes_from_its_record_without_repeating_a_paid_call() {
    let dir = tempfile::tempdir().expect("dir");
    let before = run_and_kill(dir.path());

    // ---- second process: fresh providers over the same files.
    let second = runtime();
    let p2 = Parts::open_with(
        dir.path(),
        |_| panic!("the resumed run must not decompose again"),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    let started = Instant::now();
    let report = second
        .block_on(async {
            Orchestrator::new(cfg(), p2.services(WINDOW))
                .expect("orchestrator")
                .resume(RunId::new("r1"))
                .await
        })
        .expect("resume");
    println!("restart to completion: {:?}", started.elapsed());

    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["a".to_owned(), "b".to_owned()]);
    assert_eq!(p2.planner.calls(), 0, "no second decomposition");
    let requests = p2.stats.requests();
    assert!(
        requests.iter().all(|r| !r.1.contains("make a")),
        "the verified contract was not run again: {requests:?}"
    );
    assert!(
        requests.iter().all(|r| r.2 >= 1),
        "b's completed round 0 was not asked again: {requests:?}"
    );
    assert_eq!(
        requests.len(),
        1,
        "only b's interrupted round was issued again"
    );

    // Re-grounded on resume, against the verbatim instruction.
    assert_eq!(p2.grounder.calls(), 1);
    assert!(p2.grounder.prompts()[0].contains("make a and b"));
    let record = RunStore::new(p2.graph.clone())
        .load(&RunId::new("r1"))
        .expect("load")
        .expect("record");
    assert!(
        record
            .groundings
            .iter()
            .any(|g| g.trigger == GroundingTrigger::Resume && g.serves)
    );

    // The ledger: every call counted before the kill is still counted once, and
    // after the resume each task has exactly the rounds it needed.
    let after = p2.ledger.calls().expect("calls");
    for c in before
        .iter()
        .filter(|c| c.settlement == Settlement::Counted)
    {
        assert_eq!(
            after.iter().filter(|x| x.id == c.id).count(),
            1,
            "call {} is recorded once",
            c.id
        );
    }
    let counted = |task: &str| {
        after
            .iter()
            .filter(|c| c.task_id == task && c.settlement == Settlement::Counted)
            .count()
    };
    assert_eq!(counted("r1/a"), 2);
    assert_eq!(counted("r1/b"), 2);
    // The round the kill interrupted is on record, unreconciled, and charged at
    // its worst case (the route's stop point and a full output): the resumed
    // run's spend is every counted call plus that worst case, not less.
    let interrupted: Vec<_> = after
        .iter()
        .filter(|c| c.task_id == "r1/b" && c.settlement == Settlement::Unreconciled)
        .collect();
    assert_eq!(
        interrupted.len(),
        1,
        "the interrupted call is kept, not dropped"
    );
    let prices = prices();
    let worst = prices
        .cost(
            &interrupted[0].model,
            &TokenUsage {
                // The stop point at the dearer input rate (cache writes, here).
                cache_write: 80_000,
                text_output: 4_096,
                ..TokenUsage::default()
            },
        )
        .expect("priced");
    let counted_spend: Money = after
        .iter()
        .filter(|c| {
            (c.task_id.starts_with("r1/") || c.agent_id == "orchestrator:r1")
                && c.settlement == Settlement::Counted
        })
        .map(|c| prices.cost(&c.model, &c.usage).expect("priced"))
        .sum();
    assert_eq!(report.spent, counted_spend + worst, "{report:?}");
    assert!(
        std::fs::read_to_string(dir.path().join("worktrees/r1-b-r0-a0/src/b.rs"))
            .expect("b's change survived the kill in its worktree")
            .contains("alpha")
            || std::fs::read_to_string(dir.path().join("worktrees/r1-b-r0-a0/src/b.rs"))
                .expect("read")
                .contains("beta")
    );
}

#[test]
fn a_worker_that_finished_before_the_kill_is_not_run_again() {
    use kgraf_core::{CallId, TokenUsage};
    use kgraf_provider::StopReason;
    use kgraf_runtime::WorkerEvent;
    use std::io::Write;

    let dir = tempfile::tempdir().expect("dir");
    run_and_kill(dir.path());
    // The worker's answer reached its transcript, and the process died before
    // the orchestrator recorded the result.
    let path = dir.path().join("runs/r1/workers/r1-b-r0-a0.jsonl");
    let text = std::fs::read_to_string(&path).expect("transcript");
    let next = text.lines().count();
    let call = text
        .lines()
        .rev()
        .find(|l| l.contains(r#""type":"model_call""#))
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .and_then(|v| v["event"]["call"].as_str().map(CallId::new))
        .expect("the interrupted call");
    let tail = [
        WorkerEvent::TextDelta {
            text: "done".into(),
        },
        WorkerEvent::Usage {
            call,
            usage: TokenUsage {
                uncached_input: 100,
                text_output: 10,
                ..TokenUsage::default()
            },
        },
        WorkerEvent::Stop {
            reason: StopReason::EndTurn,
        },
    ];
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open");
    for (i, event) in tail.into_iter().enumerate() {
        let line = serde_json::json!({"seq": next + i, "event": event});
        writeln!(file, "{line}").expect("append");
    }
    drop(file);

    let second = runtime();
    let p2 = Parts::open_with(
        dir.path(),
        |_| panic!("no second decomposition"),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    let report = second
        .block_on(async {
            Orchestrator::new(cfg(), p2.services(WINDOW))
                .expect("orchestrator")
                .resume(RunId::new("r1"))
                .await
        })
        .expect("resume");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(report.verified, vec!["a".to_owned(), "b".to_owned()]);
    assert!(
        p2.stats.requests().is_empty(),
        "the finished worker's result was rebuilt from its transcript, not re-run"
    );
    // Its calls were charged once, at open (counted at cost, the unreconciled one
    // at its worst case), and not again when the rebuilt result settled.
    let prices = prices();
    let after = p2.ledger.calls().expect("calls");
    let mut expected = Money::ZERO;
    for c in after
        .iter()
        .filter(|c| c.task_id.starts_with("r1/") || c.agent_id == "orchestrator:r1")
    {
        expected += match c.settlement {
            Settlement::Counted => prices.cost(&c.model, &c.usage).expect("priced"),
            Settlement::Unreconciled => prices
                .cost(
                    &c.model,
                    &TokenUsage {
                        // The stop point at the dearer input rate (cache writes, here).
                        cache_write: 80_000,
                        text_output: 4_096,
                        ..TokenUsage::default()
                    },
                )
                .expect("priced"),
            _ => Money::ZERO,
        };
    }
    assert_eq!(report.spent, expected, "{report:?}");
}

#[test]
fn a_kill_mid_decomposition_does_not_repeat_the_calls_that_answered() {
    let dir = tempfile::tempdir().expect("dir");
    // 150,000 characters is about 50,000 tokens, over a 40,000-token target, so
    // the plan needs one split call after the first decomposition.
    let huge = format!("huge {}", "x ".repeat(75_000));
    let top = serde_json::json!({"contracts":[
        {"id":"h","instruction": huge,"acceptance":"ok","writes":[{"path":"src/h.rs"}]},
        {"id":"z","instruction":"z WRITE src/z.rs","acceptance":"ok",
         "writes":[{"path":"src/z.rs"}],"depends_on":["h"]}
    ]})
    .to_string();
    let split = plan_json(&[
        c("p1", "part one", "src/h1.rs", &[]),
        c("p2", "part two", "src/h2.rs", &["p1"]),
    ]);

    // First process: the decomposition answers; the split call never does.
    let first = runtime();
    let top_for_run = top.clone();
    let p1 = Parts::open_with(
        dir.path(),
        move |_| top_for_run.clone(),
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    *p1.planner.hang_on.lock().expect("lock") = Some("Split this contract".into());
    let o = Orchestrator::new(cfg(), p1.services(WINDOW)).expect("orchestrator");
    first.spawn(o.start(RunId::new("r1"), "one huge part and one small".into()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while p1.planner.calls() < 2 {
        assert!(Instant::now() < deadline, "the split call was not reached");
        std::thread::sleep(Duration::from_millis(5));
    }
    first.shutdown_background();
    drop(p1);
    after_kill();

    // Second process: only the split that never answered is asked again.
    let second = runtime();
    let p2 = Parts::open_with(
        dir.path(),
        move |prompt| {
            assert!(
                prompt.contains("Split this contract"),
                "the answered decomposition was asked again"
            );
            split.clone()
        },
        |_| SERVES.into(),
        Duration::from_millis(5),
        true,
    );
    let report = second
        .block_on(async {
            Orchestrator::new(cfg(), p2.services(WINDOW))
                .expect("orchestrator")
                .resume(RunId::new("r1"))
                .await
        })
        .expect("resume");
    assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    assert_eq!(p2.planner.calls(), 1);
    assert_eq!(
        report.verified,
        vec!["h.p1".to_owned(), "h.p2".to_owned(), "z".to_owned()]
    );
}
