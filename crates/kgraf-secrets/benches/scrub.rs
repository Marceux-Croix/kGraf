//! Decision 0071: benchmark both
//! scrub paths on a 1 MB tool output. `scrub_model_facing` (registry only) is
//! the path every tool result crosses on its way to the model; `scrub_persist`
//! (registry plus pattern rules) is the path to the transcript, the shell and
//! agent-to-agent messages. Both are measured with no secret present (the
//! common case, and the one that must stay linear: a single `RegexSet` pass
//! for the persist path, a handful of `str::find` calls for the model-facing
//! one) and with one secret buried in the middle (the path that also has to
//! locate and replace a span). A handful of registered exact secrets are
//! added in every case, since a production `Scrubber` always has at least the
//! running provider's own API key registered.

use criterion::{Criterion, criterion_group, criterion_main};
use kgraf_secrets::Scrubber;
use std::hint::black_box;

const ONE_MB: usize = 1024 * 1024;

fn scrubber_with_exact_secrets() -> Scrubber {
    let s = Scrubber::new();
    // Representative of what a running worker actually registers: one
    // provider key, one MCP header value.
    s.register_exact_secret("provider-api-key", "kgraf-bench-provider-Yc8Mr3Ud6Kp1Zs9V");
    s.register_exact_secret("mcp-header-env", "kgraf-bench-mcp-header-Tf5Nx9Ge2Jw7Lb4Q");
    s
}

/// 1 MB of text with no secret shape anywhere in it: ordinary shell output.
fn clean_1mb() -> String {
    let line = "drwxr-xr-x  2 user user 4096 Jan  1 00:00 some-ordinary-directory-name\n";
    line.repeat(ONE_MB / line.len() + 1)
}

/// The same, with one real secret shape inserted in the middle.
fn dirty_1mb() -> String {
    let mut text = clean_1mb();
    let secret = format!("ghp_{}", "a".repeat(36));
    let mid = text.len() / 2;
    text.insert_str(mid, &format!(" token={secret} "));
    text
}

fn bench_scrub(c: &mut Criterion) {
    let scrubber = scrubber_with_exact_secrets();
    let clean = clean_1mb();
    let dirty = dirty_1mb();

    c.bench_function("scrub_model_facing_1mb_no_secret", |b| {
        b.iter(|| black_box(scrubber.scrub_model_facing(black_box(&clean))))
    });
    c.bench_function("scrub_model_facing_1mb_one_pattern_secret", |b| {
        // The model-facing path never applies pattern rules, so this measures
        // the same cost as the clean case: the pattern secret is not there to
        // find. Kept as its own benchmark so a future change that makes this
        // path pattern-aware again shows up as a regression here too.
        b.iter(|| black_box(scrubber.scrub_model_facing(black_box(&dirty))))
    });
    c.bench_function("scrub_persist_1mb_no_secret", |b| {
        b.iter(|| black_box(scrubber.scrub_persist(black_box(&clean))))
    });
    c.bench_function("scrub_persist_1mb_one_pattern_secret", |b| {
        b.iter(|| black_box(scrubber.scrub_persist(black_box(&dirty))))
    });
}

criterion_group!(benches, bench_scrub);
criterion_main!(benches);
