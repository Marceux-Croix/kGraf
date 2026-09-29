//! Time to first useful action (measured budget): from `prepare` on a
//! repository to a validated, reconciled plan naming its first dispatchable item,
//! with the model answering at once (the live call's latency is the provider's and
//! is measured by the live run, not here). Each median is asserted against its
//! budget at the end, so a regression fails `cargo bench`.
//!
//! - `cold`: a fresh graph each iteration: full index, ingest, rank, brief.
//! - `warm`: the index current and the answer cached: what a re-read or a restart
//!   costs.
//!
//! `this_repo` is this workspace. `large_repo` is any local repository named by
//! `KGRAF_BENCH_LARGE_REPO` (read only; the graph lives in memory); without
//! it those benchmarks are skipped and say so. Budgets and the measurements they
//! came from are in `BUDGETS`.
//!
//! Run: `KGRAF_BENCH_LARGE_REPO=/path/to/repo cargo bench -p kgraf-interpret --bench first_action`.

#[path = "../tests/support/mod.rs"]
mod support;

use criterion::{Criterion, criterion_group, criterion_main};
use kgraf_graph::GraphStore;
use kgraf_interpret::{NoTestRuns, prepare};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use support::*;

/// Set from the measurements in
/// a recorded release-build run (release,
/// Apple silicon, load average 14 to 28 from other builds: not a quiet machine, so
/// these are loose; a quiet re-measurement is still owed). About 2x the
/// worst median seen, rounded up. `large_repo/*` is enforced only when
/// `KGRAF_BENCH_LARGE_REPO` is set; without it those budgets are reported as
/// skipped.
const BUDGETS: &[(&str, Duration)] = &[
    ("this_repo/cold", Duration::from_millis(4_000)),
    ("this_repo/warm", Duration::from_millis(1_500)),
    ("large_repo/cold", Duration::from_millis(28_000)),
    ("large_repo/warm", Duration::from_millis(12_000)),
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
                println!("budget {id}: skipped (no samples; set KGRAF_BENCH_LARGE_REPO)");
                if id.starts_with("this_repo") {
                    failures.push(format!("{id}: no samples"));
                }
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
            "first-action budgets missed:\n{}",
            failures.join("\n")
        );
    }
}

fn model() -> FakeModel {
    FakeModel::new(|_| {
        answer(
            vec![item(
                "first",
                &[],
                json!({"symbols": ["main"]}),
                "unstated",
                json!([{"text": "it runs", "check": {"kind": "symbol", "name": "main"}}]),
            )],
            json!([]),
            &[],
        )
    })
}

/// One pass to a named first action.
fn one(rt: &tokio::runtime::Runtime, store: &GraphStore, model: &FakeModel) -> Duration {
    let start = Instant::now();
    let i = rt.block_on(async {
        prepare(store, request())
            .expect("prepare")
            .ask(model)
            .await
            .expect("ask")
            .finish(store, &NoTestRuns)
            .expect("finish")
    });
    let d = start.elapsed();
    assert!(i.first_action().is_some());
    d
}

fn repos() -> Vec<(&'static str, PathBuf)> {
    let this = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let mut out = vec![("this_repo", this)];
    if let Some(large) = kgraf_core::legacy::env_var("KGRAF_BENCH_LARGE_REPO") {
        out.push(("large_repo", PathBuf::from(large)));
    }
    out
}

fn benches(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let s = Samples::default();
    let mut g = c.benchmark_group("first_action");
    g.sample_size(10);
    for (name, root) in repos() {
        g.bench_function(format!("{name}/cold"), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let store = GraphStore::open_in_memory(&root).expect("store");
                    let d = one(&rt, &store, &model());
                    s.add(&format!("{name}/cold"), d);
                    total += d;
                }
                total
            })
        });
        let store = GraphStore::open_in_memory(&root).expect("store");
        let m = model();
        one(&rt, &store, &m);
        g.bench_function(format!("{name}/warm"), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let d = one(&rt, &store, &m);
                    s.add(&format!("{name}/warm"), d);
                    total += d;
                }
                total
            })
        });
        assert_eq!(m.calls(), 1, "warm iterations must not ask again");
    }
    g.finish();
    s.assert_budgets();
}

criterion_group!(first_action, benches);
criterion_main!(first_action);
