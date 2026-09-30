//! Gathering a corpus: the code through the graph's incremental indexer, then the
//! documents through `kgraf-ingest`.
//!
//! **Covered means indexed.** A source file the graph indexed is not read a second
//! time by ingest; it is counted as `covered`. The test is index membership (the
//! graph's own file list after this pass), not the file extension: a source file
//! the graph did not index because of its language is still read as a document.
//! **What the index excludes, ingest excludes too, and both are named:** the
//! directories the index actually skipped, by path (those with an excluded name
//! such as `node_modules`, `target`, `build`, `dist`, and nested checkouts), are
//! not walked, so vendored and generated code is not read as prose; each is listed
//! in [`CorpusStats`] and in the reading. Everything else the walker admits (prose, and configuration such as
//! `Cargo.toml` or `package.json`) is read as a document.

use crate::error::InterpretError;
use kgraf_graph::{GraphStore, IndexReport, Indexer};
use kgraf_ingest::{Document, DocumentKind, IngestOptions, SkipReason};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// One file ingest could not read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unreadable {
    pub path: String,
    pub error: String,
}

/// How to gather.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GatherOptions {
    /// Index nested repositories and git worktrees too (see
    /// `kgraf_graph::Indexer::with_nested_repositories`). Off by default.
    pub include_nested_repositories: bool,
}

/// What was gathered.
#[derive(Debug)]
pub struct Corpus {
    pub root: PathBuf,
    /// Parsed documents, paths relative to the root.
    pub documents: Vec<Document>,
    /// Files that are documents and could not be read, reported in the reading.
    pub unreadable: Vec<Unreadable>,
    /// Files never in scope (binary, unknown extension, lockfiles).
    pub skipped: usize,
    /// Source files ingest left to the graph because the graph indexed them.
    pub covered: usize,
    /// The index pass this gathering ran.
    pub index: IndexReport,
    /// Files in the graph that are not Markdown: the code there is.
    pub code_files: u64,
    /// Wall-clock milliseconds of the index pass.
    pub index_ms: u64,
    /// Blake3 over every indexed file's content hash and every document's text:
    /// two gatherings with the same fingerprint read the same corpus.
    pub fingerprint: String,
}

/// Summary counts, kept with the interpretation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CorpusStats {
    pub documents: usize,
    pub unreadable: Vec<Unreadable>,
    pub skipped: usize,
    pub covered: usize,
    pub code_files: u64,
    pub files_indexed: u64,
    pub files_reindexed: u64,
    /// Nested checkouts the index skipped.
    pub nested_repositories_skipped: Vec<String>,
    /// Directories the index skipped by excluded name (`target`, `build`, ...),
    /// not read as documents either.
    pub excluded_directories: Vec<String>,
}

impl Corpus {
    /// Nothing to read: no documents and no code.
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty() && self.code_files == 0
    }

    /// Documents that are prose, not configuration or code.
    pub fn prose(&self) -> impl Iterator<Item = &Document> {
        self.documents
            .iter()
            .filter(|d| d.kind != DocumentKind::Code)
    }

    pub fn stats(&self) -> CorpusStats {
        CorpusStats {
            documents: self.documents.len(),
            unreadable: self.unreadable.clone(),
            skipped: self.skipped,
            covered: self.covered,
            code_files: self.code_files,
            files_indexed: self.index.files_seen,
            files_reindexed: self.index.files_added + self.index.files_changed,
            nested_repositories_skipped: self.index.nested_repositories_skipped.clone(),
            excluded_directories: self.index.excluded_dirs_skipped.clone(),
        }
    }
}

fn relative(root: &std::path::Path, path: &std::path::Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

/// Index the store's root incrementally and ingest its documents.
pub fn gather(store: &GraphStore, options: &GatherOptions) -> Result<Corpus, InterpretError> {
    let root = store.root().to_path_buf();
    let t = std::time::Instant::now();
    let index = Indexer::new(store)
        .with_nested_repositories(options.include_nested_repositories)
        .index()?;
    let index_ms = t.elapsed().as_millis() as u64;
    let indexed: BTreeSet<String> = store.file_paths()?.into_iter().collect();
    let report = kgraf_ingest::ingest_directory_with(
        &root,
        &IngestOptions {
            covered_paths: indexed.clone(),
            // Exactly the directories the index skipped (by excluded name, or as
            // a nested checkout), by path; both are named in the stats.
            excluded_dir_paths: index
                .nested_repositories_skipped
                .iter()
                .chain(&index.excluded_dirs_skipped)
                .cloned()
                .collect(),
            ..IngestOptions::default()
        },
    )?;
    let covered = report
        .skipped
        .iter()
        .filter(|s| matches!(s.reason, SkipReason::CoveredElsewhere))
        .count();
    let mut documents: Vec<Document> = report
        .documents
        .into_iter()
        .map(|mut d| {
            d.path = relative(&root, &d.path);
            d
        })
        .collect();
    documents.sort_by(|a, b| a.path.cmp(&b.path));
    let unreadable = report
        .failures
        .iter()
        .map(|f| Unreadable {
            path: relative(&root, &f.path).display().to_string(),
            error: f.error.to_string(),
        })
        .collect();
    let code_files = indexed
        .iter()
        .filter(|p| !(p.ends_with(".md") || p.ends_with(".markdown")))
        .count() as u64;
    let mut hasher = blake3::Hasher::new();
    for path in &indexed {
        if let Some(f) = store.file(path)? {
            hasher.update(path.as_bytes());
            hasher.update(&[0]);
            hasher.update(f.content_hash.as_bytes());
            hasher.update(&[0]);
        }
    }
    for d in &documents {
        hasher.update(d.path.to_string_lossy().as_bytes());
        hasher.update(&[0]);
        hasher.update(d.text.as_bytes());
        hasher.update(&[0]);
    }
    Ok(Corpus {
        root,
        documents,
        unreadable,
        skipped: report.skipped.len() - covered,
        covered,
        index,
        code_files,
        index_ms,
        fingerprint: hasher.finalize().to_hex().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &std::path::Path, p: &str, t: &str) {
        let p = root.join(p);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
        std::fs::write(p, t).expect("write");
    }

    /// A source file counted
    /// as covered by its extension alone. Covered now means indexed: a source file
    /// in a language the graph does not index is read as a document, and one in a
    /// directory the index excludes (`build/`) is read by neither (ingest skips what the index skips).
    #[test]
    fn covered_means_indexed_and_excluded_directories_are_skipped_by_both() {
        let dir = tempfile::tempdir().expect("dir");
        let db = tempfile::tempdir().expect("db");
        write(dir.path(), "build/gen.rs", "pub fn generated_thing() {}\n");
        write(dir.path(), "src/lib.rs", "pub fn real() {}\n");
        write(dir.path(), "tools/gen.sh", "echo generated\n");
        write(dir.path(), "README.md", "# R\n\nx\n");
        let store = GraphStore::open(&db.path().join("g.db"), dir.path()).expect("store");
        let c = gather(&store, &GatherOptions::default()).expect("gather");
        let files = store.file_paths().expect("files");
        assert!(!files.iter().any(|f| f.contains("gen.rs")));
        assert_eq!(c.covered, 1, "only src/lib.rs is indexed");
        let docs: Vec<String> = c
            .documents
            .iter()
            .map(|d| d.path.display().to_string())
            .collect();
        assert_eq!(docs, ["README.md", "tools/gen.sh"]);
    }

    /// A virtual environment's packages are neither indexed nor read as documents,
    /// and the directory is named in the stats rather than dropped silently.
    #[test]
    fn a_virtual_environment_is_read_by_neither_the_index_nor_ingest_and_is_named() {
        let dir = tempfile::tempdir().expect("dir");
        let db = tempfile::tempdir().expect("db");
        write(dir.path(), "README.md", "# R\n\nx\n");
        write(dir.path(), "src/app.py", "def app():\n    pass\n");
        write(dir.path(), "envs/bench/pyvenv.cfg", "home = /usr/bin\n");
        write(
            dir.path(),
            "envs/bench/lib/pkg/README.md",
            "# Vendored\n\nnot ours\n",
        );
        write(
            dir.path(),
            "envs/bench/lib/pkg/mod.py",
            "def vendored():\n    pass\n",
        );
        let store = GraphStore::open(&db.path().join("g.db"), dir.path()).expect("store");
        let c = gather(&store, &GatherOptions::default()).expect("gather");
        let docs: Vec<String> = c
            .documents
            .iter()
            .map(|d| d.path.display().to_string())
            .collect();
        assert_eq!(docs, ["README.md"]);
        assert_eq!(c.stats().excluded_directories, ["envs/bench"]);
        assert_eq!(c.code_files, 1, "only src/app.py is code");
    }
}
