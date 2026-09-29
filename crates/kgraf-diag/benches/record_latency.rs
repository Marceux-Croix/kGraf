//! Benchmarks the call a hot path actually pays: `DiagWriter::record`, the
//! `try_send` onto the bounded channel. The background
//! thread's file I/O is deliberately not on this critical path and is not
//! what this bench measures.

use criterion::{Criterion, criterion_group, criterion_main};
use kgraf_diag::event::DiagEvent;
use kgraf_diag::writer::DiagWriter;

fn bench_record(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let writer = DiagWriter::start(dir.path().join("diag.jsonl"), kgraf_diag::DEFAULT_CAP_BYTES);
    c.bench_function("diag_record_launch", |b| {
        b.iter(|| writer.record(DiagEvent::launch()));
    });
    c.bench_function("diag_record_error", |b| {
        b.iter(|| {
            writer.record(DiagEvent::error(
                "KG-0501",
                "kgraf-host",
                "route not ready: kimi",
            ))
        });
    });
}

criterion_group!(benches, bench_record);
criterion_main!(benches);
