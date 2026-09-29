//! Ingest throughput on a synthetic 1,000-file corpus.
//!
//! The corpus is generated once per benchmark run (not shipped as fixtures):
//! 700 plain-text files, 200 Markdown files with two headings each, and 100
//! source files, roughly matching what a real project's non-binary tree
//! looks like. PDF and DOCX are excluded from the throughput number because
//! their cost is dominated by the (already separately understood) cost of
//! `pdf-extract`/`docx-rs` per file, not by the walker or dispatch this
//! benchmark is measuring; each format's own extraction cost is exercised by
//! its own unit test.

use std::path::Path;

use criterion::{Criterion, criterion_group, criterion_main};
use kgraf_ingest::ingest_directory;

const FILE_COUNT: usize = 1_000;
const MARKDOWN_SHARE: usize = 200;
const CODE_SHARE: usize = 100;

fn build_corpus(root: &Path) {
    for i in 0..FILE_COUNT {
        if i < MARKDOWN_SHARE {
            let path = root.join(format!("doc-{i:04}.md"));
            let content = format!(
                "# Document {i}\n\nIntro paragraph for document {i}.\n\n## Details\n\nBody text repeated a few times to give the walker something to read. Body text repeated a few times to give the walker something to read.\n"
            );
            std::fs::write(path, content).expect("write markdown fixture");
        } else if i < MARKDOWN_SHARE + CODE_SHARE {
            let path = root.join(format!("module_{i:04}.rs"));
            let content = format!("//! Module {i}\npub fn handler_{i}() -> u32 {{\n    {i}\n}}\n");
            std::fs::write(path, content).expect("write code fixture");
        } else {
            let path = root.join(format!("note-{i:04}.txt"));
            let content = format!(
                "Plain note {i}. This is a short line of prose repeated for realism. Repeated line for note {i}."
            );
            std::fs::write(path, content).expect("write text fixture");
        }
    }
}

fn bench_ingest_throughput(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    build_corpus(dir.path());

    let mut group = c.benchmark_group("ingest_directory");
    group.sample_size(10);
    group.bench_function("1000_file_mixed_corpus", |b| {
        b.iter(|| {
            let report = ingest_directory(dir.path()).expect("ingest");
            assert_eq!(report.documents.len(), FILE_COUNT);
            assert!(report.failures.is_empty());
        });
    });
    group.finish();
}

criterion_group!(benches, bench_ingest_throughput);
criterion_main!(benches);
