//! Harness budgets. Each benchmark's median is asserted against a
//! budget at the end of the run, so a regression fails `cargo bench`.
//!
//! - `first_dispatch/*`: from `Orchestrator::start` to the first worker request
//!   reaching a provider, with a decomposer that answers at once. Covers the run
//!   record write, the plan call through the ledger, plan validation and sizing,
//!   readiness, admission, lease acquisition, budget reservation, the dispatch
//!   record write, the trace, the transcript file and `spawn_worker`.
//! - `dispatch_throughput/*`: 48 independent contracts dispatched while 256
//!   unrelated leases are held by another holder, reported per contract: the
//!   time from start until every contract's worker has sent its first request,
//!   divided by 48. Workers stay in flight, so each dispatch also runs against
//!   the leases of every contract dispatched before it.
//!
//! - `restart_to_resume/disk_ledger`: a stopped run with one worker mid-contract,
//!   from `Orchestrator::resume` in a fresh process state (new graph, ledger and
//!   provider handles over the same files) to the resumed worker's next request.
//!   Covers reading and validating the record, seeding the budget from the ledger,
//!   the verifier recheck, the resume re-grounding call, re-acquiring the leases
//!   and rebuilding the worker from its transcript.
//!
//! - `sessions/*` (decision 0067): with 48 live sessions,
//!   `status_48_live` is one loop pass as the box sees it (a status read
//!   waits behind the pass's inbox re-check, question expiry and judgment
//!   batch), and `note_48_live` is a direct note from the call to its
//!   acknowledgment (liveness, the inbox row, the run record's note, the
//!   injection). `old_recheck_48_live` times, for reference and unbudgeted,
//!   the store reads the re-check did on every pass before L3 (one
//!   `sessions_of_run` and one `undelivered` per session).
//!
//! `memory_ledger` isolates the orchestrator's own cost; `disk_ledger` is the
//! shipped configuration, whose `synchronous = FULL` commits dominate and vary by
//! about 2x run to run. Budgets are the medians measured on 2026-09-26 (Apple
//! silicon, release profile) with headroom of about 2.5x.
//! Run: `cargo bench -p kgraf-orchestrator --bench dispatch`.

#[path = "../tests/support/mod.rs"]
mod support;

use criterion::{Criterion, criterion_group, criterion_main};
use kgraf_core::AgentId;
use kgraf_core::RunId;
use kgraf_core::session::{InboxKind, InjectMode, SessionRecords};
use kgraf_graph::LeaseRegistry;
use kgraf_orchestrator::{Limits, Orchestrator, OrchestratorHandle};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::*;

const CONTRACTS: usize = 48;
const HELD: usize = 256;

/// Measured medians on 2026-09-26: first dispatch 816 us (memory ledger) and
/// 1.70 ms (disk ledger); throughput 401 us and 752 us per contract with 256
/// leases held; restart to resume 4.67 ms. Budgets are those times 2.5,
/// rounded up.
const BUDGETS: &[(&str, Duration)] = &[
    ("first_dispatch/memory_ledger", Duration::from_micros(2_100)),
    ("first_dispatch/disk_ledger", Duration::from_micros(4_300)),
    (
        "dispatch_throughput/memory_ledger",
        Duration::from_micros(1_000),
    ),
    (
        "dispatch_throughput/disk_ledger",
        Duration::from_micros(1_900),
    ),
    (
        "restart_to_resume/disk_ledger",
        Duration::from_micros(12_000),
    ),
    // Measured 2026-09-28 (Apple silicon, release): one loop pass 19.5 us and
    // a direct note 4.24 ms with 48 live sessions; the pre-L3 re-check's
    // store reads alone took 6.06 ms per pass. Budgets are 2.5x, rounded up.
    ("sessions/status_48_live", Duration::from_micros(50)),
    ("sessions/note_48_live", Duration::from_micros(11_000)),
];

#[derive(Default)]
struct Samples(Mutex<BTreeMap<String, Vec<Duration>>>);

impl Samples {
    fn add(&self, id: &str, d: Duration) {
        self.0
            .lock()
            .expect("lock")
            .entry(id.to_owned())
            .or_default()
            .push(d);
    }

    fn assert_budgets(&self) {
        let all = self.0.lock().expect("lock");
        let mut failures = Vec::new();
        for (id, budget) in BUDGETS {
            let Some(v) = all.get(*id) else {
                failures.push(format!("{id}: no samples"));
                continue;
            };
            let mut v = v.clone();
            v.sort();
            let median = v[v.len() / 2];
            println!("budget {id}: median {median:?} of {budget:?}");
            if median > *budget {
                failures.push(format!("{id}: median {median:?} exceeds budget {budget:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "Dispatch budgets missed:\n{}",
            failures.join("\n")
        );
    }
}

/// A run with `contracts` live, parked sessions, for the session benches.
struct LiveRun {
    _dir: tempfile::TempDir,
    parts: Parts,
    handle: OrchestratorHandle,
    run: tokio::task::JoinHandle<
        Result<kgraf_orchestrator::RunReport, kgraf_orchestrator::OrchestratorError>,
    >,
}

fn live_run(rt: &tokio::runtime::Runtime, contracts: usize) -> LiveRun {
    let dir = tempfile::tempdir().expect("dir");
    let specs: Vec<(String, String, String)> = (0..contracts)
        .map(|i| {
            (
                format!("c{i}"),
                format!("part {i} HANG"),
                format!("src/p{i}.rs"),
            )
        })
        .collect();
    let list: Vec<C<'_>> = specs
        .iter()
        .map(|(id, ins, path)| c(id, ins, path, &[]))
        .collect();
    let plan = plan_json(&list);
    let mut p = Parts::open(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    p.admission = admission(Limits::new(64, 64).expect("limits"));
    let mut cfg = config();
    cfg.reground_every = 0;
    cfg.max_contracts_per_plan = 64;
    let o = Orchestrator::new(cfg, p.services(200_000)).expect("orchestrator");
    let handle = o.handle();
    let stats = p.stats.clone();
    let run = rt.spawn(o.start(RunId::new("r1"), "bench".into()));
    rt.block_on(async {
        while stats
            .requests
            .lock()
            .expect("lock")
            .iter()
            .filter(|r| r.2 == 1)
            .count()
            < contracts
        {
            assert!(
                !run.is_finished(),
                "the run ended before every worker parked"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });
    LiveRun {
        _dir: dir,
        parts: p,
        handle,
        run,
    }
}

/// A stopped run, then a resume timed to the resumed worker's first request.
fn one_restart(rt: &tokio::runtime::Runtime) -> Duration {
    let dir = tempfile::tempdir().expect("dir");
    let plan = plan_json(&[c("a", "part a HANG", "src/a.rs", &[])]);
    let p = Parts::open_with(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
        true,
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    let o = Orchestrator::new(config(), p.services(200_000)).expect("orchestrator");
    let handle = o.handle();
    let (alpha, beta) = (p.alpha.clone(), p.beta.clone());
    rt.block_on(async move {
        let run = tokio::spawn(o.start(RunId::new("r1"), "bench".into()));
        while !(alpha.hang_reached.load(Ordering::SeqCst)
            || beta.hang_reached.load(Ordering::SeqCst))
        {
            tokio::task::yield_now().await;
        }
        handle.stop().await;
        let _ = run.await;
    });
    drop(p);
    let p2 = Parts::open_with(
        dir.path(),
        |_| String::new(),
        |_| SERVES.into(),
        Duration::ZERO,
        true,
    );
    p2.alpha.hang.store(true, Ordering::SeqCst);
    p2.beta.hang.store(true, Ordering::SeqCst);
    let stats = p2.stats.clone();
    let start = Instant::now();
    let o = Orchestrator::new(config(), p2.services(200_000)).expect("orchestrator");
    let handle = o.handle();
    rt.block_on(async move {
        let run = tokio::spawn(o.resume(RunId::new("r1")));
        loop {
            if !stats.requests.lock().expect("lock").is_empty() {
                break;
            }
            assert!(!run.is_finished(), "the resume ended before its worker ran");
            tokio::task::yield_now().await;
        }
        let d = start.elapsed();
        handle.stop().await;
        let _ = run.await;
        d
    })
}

/// One run: start, wait until `wanted` workers have sent their first request,
/// return that time, then stop the run.
fn one_run(
    rt: &tokio::runtime::Runtime,
    disk_ledger: bool,
    contracts: usize,
    held: usize,
) -> Duration {
    let dir = tempfile::tempdir().expect("dir");
    let specs: Vec<(String, String, String)> = (0..contracts)
        .map(|i| {
            (
                format!("c{i}"),
                format!("part {i} HANG"),
                format!("src/p{i}.rs"),
            )
        })
        .collect();
    let list: Vec<C<'_>> = specs
        .iter()
        .map(|(id, ins, path)| c(id, ins, path, &[]))
        .collect();
    let plan = plan_json(&list);
    let mut p = Parts::open_with(
        dir.path(),
        move |_| plan.clone(),
        |_| SERVES.into(),
        Duration::ZERO,
        disk_ledger,
    );
    p.alpha.hang.store(true, Ordering::SeqCst);
    p.beta.hang.store(true, Ordering::SeqCst);
    if held > 0 {
        let g = p.graph.lock().expect("lock");
        let paths: Vec<String> = (0..held).map(|i| format!("other/f{i}.rs")).collect();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis() as i64;
        let _ = LeaseRegistry::new(&g)
            .acquire_all("someone-else", "t", &paths, &[], now)
            .expect("hold");
    }
    p.admission = admission(Limits::new(64, 64).expect("limits"));
    let mut cfg = config();
    cfg.reground_every = 0;
    // 48 contracts in one plan: over the default per-plan cap of 32.
    cfg.max_contracts_per_plan = 64;
    let o = Orchestrator::new(cfg, p.services(200_000)).expect("orchestrator");
    let handle = o.handle();
    let stats = p.stats.clone();
    rt.block_on(async move {
        let start = Instant::now();
        let run = tokio::spawn(o.start(RunId::new("r1"), "bench".into()));
        let reached = loop {
            let t = stats.first_requests.lock().expect("lock").clone();
            if t.len() >= contracts {
                break t.into_iter().max().expect("times");
            }
            // A run that ended early (a refused plan) would otherwise spin here.
            assert!(
                !run.is_finished(),
                "the run ended before every contract dispatched"
            );
            tokio::task::yield_now().await;
        };
        handle.stop().await;
        let _ = run.await;
        reached - start
    })
}

fn benches(c: &mut Criterion) {
    let samples = Arc::new(Samples::default());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");
    let mut group = c.benchmark_group("first_dispatch");
    group.sample_size(30);
    for (name, disk) in [("memory_ledger", false), ("disk_ledger", true)] {
        let s = samples.clone();
        group.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let d = one_run(&rt, disk, 1, 0);
                    s.add(&format!("first_dispatch/{name}"), d);
                    total += d;
                }
                total
            })
        });
    }
    group.finish();

    let mut group = c.benchmark_group("dispatch_throughput");
    group.sample_size(10);
    for (name, disk) in [("memory_ledger", false), ("disk_ledger", true)] {
        let s = samples.clone();
        group.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let d = one_run(&rt, disk, CONTRACTS, HELD) / CONTRACTS as u32;
                    s.add(&format!("dispatch_throughput/{name}"), d);
                    total += d;
                }
                total
            })
        });
    }
    group.finish();
    let mut group = c.benchmark_group("restart_to_resume");
    group.sample_size(20);
    {
        let s = samples.clone();
        group.bench_function("disk_ledger", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let d = one_restart(&rt);
                    s.add("restart_to_resume/disk_ledger", d);
                    total += d;
                }
                total
            })
        });
    }
    group.finish();

    let mut group = c.benchmark_group("sessions");
    group.sample_size(50);
    let live = live_run(&rt, CONTRACTS);
    {
        let s = samples.clone();
        let handle = live.handle.clone();
        group.bench_function("status_48_live", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    rt.block_on(handle.status()).expect("status");
                    let d = start.elapsed();
                    s.add("sessions/status_48_live", d);
                    total += d;
                }
                total
            })
        });
    }
    {
        let s = samples.clone();
        let handle = live.handle.clone();
        let mut n = 0usize;
        group.bench_function("note_48_live", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    n += 1;
                    let to = AgentId::new(format!("r1-c{}-r0-a0", n % CONTRACTS));
                    let start = Instant::now();
                    rt.block_on(handle.send_to_session(
                        to,
                        InboxKind::Note,
                        InjectMode::AtBoundary,
                        format!("note {n}"),
                    ))
                    .expect("sent");
                    let d = start.elapsed();
                    s.add("sessions/note_48_live", d);
                    total += d;
                }
                total
            })
        });
    }
    {
        let store = kgraf_graph::sessions::SharedSessions(live.parts.graph.clone());
        group.bench_function("old_recheck_48_live", |b| {
            b.iter(|| {
                let all = store
                    .sessions_of_run(&kgraf_core::RunId::new("r1"))
                    .expect("sessions");
                for session in all {
                    let _ = store.undelivered(&session.id).expect("undelivered");
                }
            })
        });
    }
    group.finish();
    rt.block_on(async {
        live.handle.stop().await;
        let _ = live.run.await;
    });
    drop(live.parts);
    samples.assert_budgets();
}

criterion_group!(dispatch, benches);
criterion_main!(dispatch);
