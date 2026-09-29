//! Budgets: append cost for the durable transcript, and
//! reduction cost for decision 0057's context reduction, both measured over a
//! 10,000-event transcript (not yet a measured budget, so no
//! threshold is asserted here; the numbers are reported only).

use criterion::{Criterion, criterion_group, criterion_main};
use kgraf_session::reduction::{MaskConfig, Reducible, TokenBudget, reduce};
use kgraf_session::transcript::{JsonlTranscript, Transcript};
use std::collections::BTreeSet;
use std::hint::black_box;
use tempfile::tempdir;

const N: usize = 10_000;

#[derive(Clone)]
struct BenchEvent {
    text: String,
    is_observation: bool,
}

impl Reducible for BenchEvent {
    fn approx_tokens(&self) -> usize {
        self.text.len() / 4
    }
    fn as_observation(&self) -> Option<&str> {
        self.is_observation.then_some(self.text.as_str())
    }
    fn masked(&self, placeholder: &str) -> Self {
        BenchEvent {
            text: placeholder.to_owned(),
            is_observation: true,
        }
    }
    fn is_pinned(&self) -> bool {
        false
    }
}

fn transcript_of(n: usize) -> Vec<BenchEvent> {
    (0..n)
        .map(|i| BenchEvent {
            text: format!(
                "tool output #{i}: {}",
                "x".repeat(40) // a representative-sized observation body
            ),
            is_observation: i % 3 != 0,
        })
        .collect()
}

/// The durability claim ("what survives power loss") must be
/// backed by a measured number, not an assertion. This benchmarks both
/// policies so the doc comment on [`JsonlTranscript`]'s durability envelope
/// can cite a real cost for choosing `fsync_every_append(true)`.
fn bench_append(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let mut group = c.benchmark_group("jsonl_transcript_append");
    group.sample_size(10);
    group.bench_function(format!("append_{N}_events_page_cache_only"), |b| {
        b.iter_batched(
            || {
                let dir = tempdir().expect("tempdir");
                let path = dir.path().join("transcript.jsonl");
                (dir, path)
            },
            |(dir, path)| {
                rt.block_on(async {
                    let mut transcript: JsonlTranscript<String> =
                        JsonlTranscript::open(&path).await.expect("open");
                    for i in 0..N {
                        transcript
                            .append(format!("event {i}"))
                            .await
                            .expect("append");
                    }
                });
                drop(dir); // keep the tempdir alive for the whole batch
            },
            criterion::BatchSize::PerIteration,
        );
    });

    // fsync on every append is much slower per event, so this uses a smaller
    // N and the closeout reports the per-event cost, not the raw batch time,
    // for a comparable number.
    const N_FSYNC: usize = 500;
    group.bench_function(format!("append_{N_FSYNC}_events_fsync_every_append"), |b| {
        b.iter_batched(
            || {
                let dir = tempdir().expect("tempdir");
                let path = dir.path().join("transcript.jsonl");
                (dir, path)
            },
            |(dir, path)| {
                rt.block_on(async {
                    let mut transcript: JsonlTranscript<String> = JsonlTranscript::open(&path)
                        .await
                        .expect("open")
                        .fsync_every_append(true);
                    for i in 0..N_FSYNC {
                        transcript
                            .append(format!("event {i}"))
                            .await
                            .expect("append");
                    }
                });
                drop(dir);
            },
            criterion::BatchSize::PerIteration,
        );
    });
    group.finish();
}

fn bench_reduce(c: &mut Criterion) {
    let original = transcript_of(N);
    let pinned = BTreeSet::new();
    let config = MaskConfig::default();
    let tokens: usize = original.iter().map(Reducible::approx_tokens).sum();
    let budget = TokenBudget(tokens / 2);

    let mut group = c.benchmark_group("reduction");
    group.bench_function(format!("reduce_{N}_events"), |b| {
        b.iter(|| {
            let view = reduce(black_box(&original), &pinned, budget, &config);
            black_box(view);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_append, bench_reduce);
criterion_main!(benches);
