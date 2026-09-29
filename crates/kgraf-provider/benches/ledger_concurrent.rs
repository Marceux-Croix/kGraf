//! Ledger throughput under many concurrent workers, on disk.
//!
//! `ledger_overhead` measures one caller in a loop. This measures what a fleet
//! does: many tokio tasks on a multi-thread runtime each opening and settling
//! calls at once against one on-disk ledger. Criterion reports a mean per
//! iteration and has no view of per-operation latency under concurrency, so this
//! bench has its own harness and prints throughput plus p50/p99 settle latency.
//!
//! Run: `cargo bench -p kgraf-provider --bench ledger_concurrent`. Set
//! `LEDGER_BENCH_MEMORY=1` for an in-memory ledger (no fsync, no anchor file).
//!
//! **Measured 2026-09-26**, 16 tasks on disk, medians of five alternating
//! before/after runs on one shared machine (load average 6 to 11). "Before" is
//! the ledger with one mutex-guarded connection; "after" is the writer thread
//! with group commit, SQL aggregates and the anchor flushed with plain `fsync`.
//!
//! | ledger | calls/s | settle p50 | settle p99 | call p50 | call p99 |
//! |---|---|---|---|---|---|
//! | before, blocking API | 1,583 | 104 us | 6.2 ms | 224 us | 121 ms |
//! | after, blocking API | 6,646 | 384 us | 3.4 ms | 819 us | 5.6 ms |
//! | after, async API | 9,280 | 535 us | 2.8 ms | 1.15 ms | 7.7 ms |
//!
//! A settle now waits for the batch in flight, so its p50 rises; the old p50 was
//! low because the mutex is not fair, and the waiting showed up in the whole
//! call's p99 instead (a few callers starved for over 100 ms). One task alone:
//! 1,985 calls/s before, 2,917 after (blocking API).

use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::Ledger;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CALLS_PER_TASK: usize = 200;

fn usage() -> TokenUsage {
    TokenUsage {
        uncached_input: 1_200,
        cache_read: 38_000,
        cache_write: 0,
        text_output: 420,
        reasoning_output: 180,
        raw: Some(serde_json::json!({"input_tokens": 1200, "output_tokens": 600})),
    }
}

#[derive(Clone, Copy, Debug)]
enum Api {
    /// The blocking API called from an async task, as adapters did before the
    /// writer thread.
    Sync,
    /// The async API: the task awaits the acknowledgement.
    Async,
}

/// Runs `tasks` tasks, each opening and settling `CALLS_PER_TASK` calls. Returns
/// the wall time and, per call, the settle latency and the whole call's
/// (`begin` plus settle) latency.
fn run(tasks: usize, api: Api) -> (Duration, Vec<(Duration, Duration)>) {
    let dir = tempfile::tempdir().expect("tempdir");
    // `LEDGER_BENCH_MEMORY=1` runs the same load in memory: no fsync and no
    // anchor file, so what remains is SQL, hashing and thread handoff.
    let ledger = Arc::new(if std::env::var_os("LEDGER_BENCH_MEMORY").is_some() {
        Ledger::open_in_memory().expect("ledger")
    } else {
        Ledger::open(&dir.path().join("ledger.sqlite")).expect("ledger")
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .expect("runtime");
    let started = Instant::now();
    let latencies = runtime.block_on(async {
        let mut handles = Vec::new();
        for t in 0..tasks {
            let ledger = ledger.clone();
            handles.push(tokio::spawn(async move {
                let usage = usage();
                let agent = format!("agent-{t}");
                let mut lat = Vec::with_capacity(CALLS_PER_TASK);
                for _ in 0..CALLS_PER_TASK {
                    let call_at = Instant::now();
                    match api {
                        Api::Sync => {
                            let token = ledger
                                .begin("bench", Protocol::Additive, "model-a", &agent, "task")
                                .expect("begin");
                            let at = Instant::now();
                            token.settle(&usage, Some("req")).expect("settle");
                            lat.push((at.elapsed(), call_at.elapsed()));
                        }
                        Api::Async => {
                            let token = ledger
                                .begin_async("bench", Protocol::Additive, "model-a", &agent, "task")
                                .await
                                .expect("begin");
                            let at = Instant::now();
                            token
                                .settle_async(&usage, Some("req"))
                                .await
                                .expect("settle");
                            lat.push((at.elapsed(), call_at.elapsed()));
                        }
                    }
                }
                lat
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.await.expect("task"));
        }
        all
    });
    let wall = started.elapsed();
    ledger.flush_anchor();
    assert!(ledger.verify().expect("verify").ok, "the chain verifies");
    (wall, latencies)
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let idx = ((sorted.len() as f64) * p).ceil() as usize;
    sorted[idx.saturating_sub(1).min(sorted.len() - 1)]
}

fn main() {
    // `cargo test --benches` runs this binary with `--bench` absent; keep it cheap.
    let quick = !std::env::args().any(|a| a == "--bench");
    let apis: &[Api] = &[Api::Sync, Api::Async];
    for &api in apis {
        for tasks in [1usize, 16] {
            if quick && tasks > 1 {
                continue;
            }
            let (wall, lat) = run(tasks, api);
            let mut settle: Vec<Duration> = lat.iter().map(|l| l.0).collect();
            let mut whole: Vec<Duration> = lat.iter().map(|l| l.1).collect();
            settle.sort();
            whole.sort();
            let calls = lat.len();
            let us = |d: Duration| d.as_secs_f64() * 1e6;
            println!(
                "ledger_concurrent/{api:?}/{tasks}_tasks: {calls} calls in {:.1} ms = {:.0} calls/s; \
                 settle p50 {:.0} us, p99 {:.0} us; call p50 {:.0} us, p99 {:.0} us",
                wall.as_secs_f64() * 1e3,
                calls as f64 / wall.as_secs_f64(),
                us(percentile(&settle, 0.50)),
                us(percentile(&settle, 0.99)),
                us(percentile(&whole, 0.50)),
                us(percentile(&whole, 0.99)),
            );
        }
    }
}
