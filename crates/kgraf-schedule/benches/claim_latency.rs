//! Claim latency on the atomic claim table.
//!
//! The scheduled-run budget is due-to-start latency; this benchmark measures the
//! table's own contribution to that latency (one `claim_occurrence` call), on an
//! on-disk (not in-memory) database, since the on-disk path is what a real claimant
//! uses and is what the WAL/`busy_timeout` settings are tuned for. It does not measure
//! the orchestrator's readiness or dispatch cost, which does not exist yet (scheduled runs
//! depend on the orchestrator).

use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use criterion::{Criterion, criterion_group, criterion_main};
use kgraf_schedule::ScheduleStore;

fn due(n: u64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(n as i64)
}

fn bench_claim_new_occurrence(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("bench.sqlite3");
    let store = ScheduleStore::open(&db_path).expect("open store");
    let now = due(0);

    // Each iteration claims a fresh occurrence (a new `due_at`), which exercises the
    // full path: the `INSERT OR IGNORE` that creates the row plus the claiming
    // `UPDATE`. Claiming the *same* occurrence repeatedly would only measure the
    // `UPDATE`'s no-op branch after the first iteration, which is not the common case
    // a real scheduler tick hits.
    let mut n: u64 = 0;
    c.bench_function("claim_occurrence (new occurrence, on-disk WAL)", |b| {
        b.iter(|| {
            n += 1;
            store
                .claim_occurrence(
                    "bench-schedule",
                    due(n),
                    "bench-worker",
                    now,
                    Duration::from_secs(60),
                )
                .expect("claim")
        });
    });
}

fn bench_claim_contended(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("bench-contended.sqlite3");
    let store = ScheduleStore::open(&db_path).expect("open store");
    let now = due(0);
    // Pre-claim once so every measured call hits the "already claimed, not expired"
    // rejection path: the `UPDATE`'s `WHERE` clause matches zero rows. This is the
    // losing side of a real race, and it is a cheap point query, but it is worth
    // reporting separately from the winning path above.
    store
        .claim_occurrence(
            "bench-schedule",
            due(0),
            "holder",
            now,
            Duration::from_secs(3600),
        )
        .expect("initial claim");

    c.bench_function("claim_occurrence (already held, rejected)", |b| {
        b.iter(|| {
            store
                .claim_occurrence(
                    "bench-schedule",
                    due(0),
                    "bench-worker",
                    now,
                    Duration::from_secs(60),
                )
                .expect("claim")
        });
    });
}

criterion_group!(benches, bench_claim_new_occurrence, bench_claim_contended);
criterion_main!(benches);
