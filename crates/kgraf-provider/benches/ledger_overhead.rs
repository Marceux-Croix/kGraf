//! Ledger overhead per provider call: `Ledger::begin` followed by `CallToken::settle`.
//!
//! Every provider call pays this on the accounting path (every call is attributed to the ledger), outside the model
//! call itself, so it is part of the per-turn loop overhead budget.
//!
//! Two stores are measured. The on-disk ledger is what the application uses; the
//! in-memory one isolates SQLite statement cost from file I/O. The table grows over
//! the run, as it does in use; `begin` reads `MAX(rowid)`, which is an index lookup
//! and does not scale with row count.
//!
//! Run: `cargo bench -p kgraf-provider --bench ledger_overhead`.
//!
//! **Decision 0059 (hash-chained ledger) baselines, recorded here rather than
//! only in the PR that made them, per the test-the-mechanism rule ("test the mechanism, not the
//! setting" applies equally to numbers: a benchmark nobody reruns is a
//! setting).** Three configurations, not two, because a two-point comparison
//! would conflate the durability trade with the chain's own cost:
//!
//! | configuration | in-memory | on disk | what it isolates |
//! |---|---|---|---|
//! | **A: original** (per-statement fsync journal, no chain) | ~19.4-20.6 us | ~640-726 us | the pre-0059 baseline |
//! | **B: WAL + `synchronous = FULL`, chain disabled** | ~28.9-29.6 us | ~120-160 us | the honest cost of this file's own durability pragmas alone against the same `begin`+`settle` shape as A, with `wal_autocheckpoint` and `checkpoint_fullfsync` tuned (see `Ledger::from_connection`) |
//! | **C: B, chain and anchor enabled** (what ships) | ~78-80 us | ~550-700 us | B plus one `begin` event, one `settle` event, and the anchor writer publishing both (see below) |
//!
//! **Config C's on-disk number went through two revisions, both measured, not
//! guessed.** The chain's own SQL and hashing cost, alone, is small (config B
//! vs. C's *in-memory* column shows the whole delta: ~50 us). The first
//! on-disk version of C fsynced the `.tip` anchor file inline on the settle
//! path and measured ~5 ms per call, enough to break the
//! per-turn loop budget. Moving that write to
//! [`AnchorWriter`]'s own thread (see its doc comment) dropped that to ~1.3 ms,
//! not the low hundreds of microseconds config B alone would suggest;
//! profiling (temporarily stubbing pieces of `Ledger::append_chain` one at a
//! time and re-running this bench) found the remaining cost was **concurrent
//! fsyncs from two threads to the same filesystem contending with each
//! other**, the anchor writer's own fsync-and-rename, running on its own
//! thread but against the same disk as the connection's commit fsync, not
//! CPU-bound work. `AnchorWriter` now waits `ANCHOR_COALESCE_WINDOW` after
//! each write before looking for the next one, which, under this bench's
//! back-to-back settle load, reduces how often the two threads' fsyncs land
//! at the same moment. That is what got config C's on-disk number under 1 ms.
//!
//! **This is the accepted cost of accounting durability** (a `begin` row surviving
//! a crash) **and of an anchor an attacker cannot silently roll back**
//! (decision 0059's own requirement), now paid off the settle path rather than
//! on it. `Ledger::flush_anchor` and `Drop` exist for a caller that needs the
//! anchor caught up to a specific point regardless of this cost.
//!
//! **D: one writer thread with group commit (2026-09-26).** On disk every write
//! goes through the writer thread, so a single caller pays two thread handoffs
//! per operation; in memory operations run inline. Same run, same load (load
//! average about 30, so both are slow in absolute terms): in memory 68.9 us
//! after against 138 us before; on disk 507 us after against 1.97 ms before.
//! The on-disk gain is mostly the anchor file now flushed with plain `fsync`
//! (as SQLite flushes the ledger) instead of `F_FULLFSYNC`, which stalled the
//! database's own commits. Throughput under concurrency is in
//! `benches/ledger_concurrent.rs`.

use criterion::{Criterion, criterion_group, criterion_main};
use futures_util::FutureExt;
use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::{
    CallToken, Content, Ledger, Provider, ProviderError, RateLimit, RateLimited, Request, Response,
    RouteLimiter, StopReason,
};
use std::hint::black_box;
use std::sync::Arc;

fn usage() -> TokenUsage {
    TokenUsage {
        uncached_input: 1_200,
        cache_read: 38_000,
        cache_write: 0,
        text_output: 420,
        reasoning_output: 180,
        raw: Some(serde_json::json!({
            "input_tokens": 1200,
            "cache_read_input_tokens": 38000,
            "cache_creation_input_tokens": 0,
            "output_tokens": 600
        })),
    }
}

fn record_and_settle(ledger: &Ledger, usage: &TokenUsage) {
    let token = ledger
        .begin("bench", Protocol::Additive, "model-a", "agent-1", "task-1")
        .expect("begin");
    token.settle(usage, Some("req-bench")).expect("settle");
    black_box(token.id());
}

fn bench(c: &mut Criterion) {
    let usage = usage();
    let mut group = c.benchmark_group("ledger_per_call");

    let memory = Ledger::open_in_memory().expect("in-memory ledger");
    group.bench_function("in_memory_begin_settle", |b| {
        b.iter(|| record_and_settle(&memory, &usage));
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let disk = Ledger::open(&dir.path().join("ledger.sqlite")).expect("disk ledger");
    group.bench_function("on_disk_begin_settle", |b| {
        b.iter(|| record_and_settle(&disk, &usage));
    });

    group.finish();
    rate_limited(c, &usage);
}

/// A provider that settles at once, so the bench measures the accounting path and
/// the limiter, not a model.
struct Settles(TokenUsage);

#[async_trait::async_trait]
impl Provider for Settles {
    fn adapter(&self) -> &'static str {
        "bench"
    }
    fn protocol(&self) -> Protocol {
        Protocol::Additive
    }
    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        token.settle(&self.0, Some("req-bench"))?;
        Ok(Response {
            finish_reason: None,
            model: request.model.clone(),
            content: vec![Content::text("ok")],
            stop_reason: StopReason::EndTurn,
            usage: self.0.clone(),
            request_id: Some("req-bench".into()),
        })
    }
}

fn call(ledger: &Ledger, provider: &dyn Provider, request: &Request) {
    let token = ledger
        .begin("bench", Protocol::Additive, "model-a", "agent-1", "task-1")
        .expect("begin");
    // The limit is never reached in this bench, so the future is ready at once.
    let response = provider
        .complete(request, &token)
        .now_or_never()
        .expect("never waits")
        .expect("complete");
    black_box(response);
}

/// (decision 0056): per-call accounting overhead with a route's rate
/// limiter on, against the same path with it off. The limit is far above the
/// bench's rate, so this is the cost of the check itself, never of a wait.
///
/// Measured 2026-09-26 on a loaded machine: the limiter check alone is about
/// 22 ns; in memory, begin plus settle is 60.8 us off and 61.8 us on, a
/// difference inside the noise. On disk (about 0.6 to 0.8 ms either way) the
/// fsyncs dominate and the two cases are not distinguishable.
fn rate_limited(c: &mut Criterion, usage: &TokenUsage) {
    let mut group = c.benchmark_group("ledger_per_call_rate_limited");
    let request = Request::new("model-a", "hi", 16);
    let plain: Arc<dyn Provider> = Arc::new(Settles(usage.clone()));
    let limit = RateLimit {
        requests_per_minute: u32::MAX,
        burst: u32::MAX,
        max_wait_ms: 0,
    };
    let limited = RateLimited::new(
        plain.clone(),
        RouteLimiter::new("bench", limit).expect("limit"),
    );
    // The limiter's own check, alone: what "on" adds to "off" below.
    let limiter = limited.limiter().clone();
    group.bench_function("limiter_acquire_only", |b| {
        b.iter(|| {
            black_box(
                limiter
                    .acquire("bench")
                    .now_or_never()
                    .expect("never waits")
                    .expect("admitted"),
            )
        });
    });
    // A fresh ledger per case: the table grows during a case, and a shared one
    // would bill the later case for the earlier case's rows.
    let memory_off = Ledger::open_in_memory().expect("in-memory ledger");
    group.bench_function("in_memory_limiter_off", |b| {
        b.iter(|| call(&memory_off, plain.as_ref(), &request));
    });
    let memory_on = Ledger::open_in_memory().expect("in-memory ledger");
    group.bench_function("in_memory_limiter_on", |b| {
        b.iter(|| call(&memory_on, &limited, &request));
    });
    let dir = tempfile::tempdir().expect("tempdir");
    let disk_off = Ledger::open(&dir.path().join("off.sqlite")).expect("disk ledger");
    group.bench_function("on_disk_limiter_off", |b| {
        b.iter(|| call(&disk_off, plain.as_ref(), &request));
    });
    let disk_on = Ledger::open(&dir.path().join("on.sqlite")).expect("disk ledger");
    group.bench_function("on_disk_limiter_on", |b| {
        b.iter(|| call(&disk_on, &limited, &request));
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
