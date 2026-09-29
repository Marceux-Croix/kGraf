//! Document ingestion: parsing a corpus into text the interpretation pass
//! can read.
//!
//! This crate is the **parsing half** of the interpretation pipeline only: given a directory, it
//! walks it gitignore-aware, classifies each file, extracts its text and
//! (where the format has structural headings) its sections, and reports a
//! typed error for every file it could not read (fail loud). It does not
//! interpret the corpus into planned components, constraints or work
//! items -- that pass reads this crate's `IngestReport` and is owned
//! elsewhere.
//!
//! A walked file lands in exactly one of three places, never silently
//! nowhere: `IngestReport::documents` (parsed), `IngestReport::failures`
//! (this *was* a document kGraf should have been able to read, and
//! could not -- too large, corrupt, a parser panic), or
//! `IngestReport::skipped` (this was never a document candidate at all -- an
//! unrecognized or binary extension, a dependency lockfile). Keeping those
//! separate means `failures` stays a signal a caller can act on instead of
//! being drowned in the lockfile every corpus with a package manager has.

pub mod chunk;
pub mod error;
pub mod model;
mod parse;
mod walk;

use std::path::Path;

pub use chunk::chunk_text;
pub use error::IngestError;
pub use model::{
    Document, DocumentKind, DocumentNotice, IngestFailure, IngestReport, Section, SkipReason,
    SkippedFile,
};

/// Default ceiling on a single file's size before it is read into memory.
/// 64 MiB comfortably covers any real Markdown, DOCX or source file and
/// most PDFs; a corpus with a larger file almost always has a scanned-image
/// PDF or an accidentally-included binary in it, which this crate should
/// report rather than load whole.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;

/// `ingest_directory` with [`DEFAULT_MAX_FILE_SIZE`].
pub fn ingest_directory(root: &Path) -> Result<IngestReport, IngestError> {
    ingest_directory_with_limit(root, DEFAULT_MAX_FILE_SIZE)
}

/// Walk `root` and parse every file the walker admits, collecting per-file
/// outcomes rather than stopping at the first problem.
///
/// `max_file_size` bounds how large a file this call will read into memory;
/// a file is `stat`ed before any read, and one over the limit becomes an
/// `IngestError::TooLarge` failure without its contents ever being loaded.
///
/// Only a problem with the root itself (missing, unreadable, or a walker
/// error while listing it) is a hard `Err`; every per-file problem lands in
/// `IngestReport::failures` or `IngestReport::skipped` instead, per the fail-loud rule: a
/// file is never dropped without a caller-visible reason.
pub fn ingest_directory_with_limit(
    root: &Path,
    max_file_size: u64,
) -> Result<IngestReport, IngestError> {
    ingest_directory_with(
        root,
        &IngestOptions {
            max_file_size,
            ..IngestOptions::default()
        },
    )
}

/// How [`ingest_directory_with`] reads a corpus.
#[derive(Clone, Debug)]
pub struct IngestOptions {
    /// See [`ingest_directory_with_limit`].
    pub max_file_size: u64,
    /// Source files the caller already holds another way, by path relative to
    /// the root (forward slashes): the files the code graph actually indexed, not
    /// every file with a code extension, so a source file the graph skipped (a
    /// build directory, a nested checkout) is still read here. A listed code file
    /// lands in `IngestReport::skipped` as `SkipReason::CoveredElsewhere`, counted,
    /// never silently absent (fail loud). Prose formats are never skipped this way.
    pub covered_paths: std::collections::BTreeSet<String>,
    /// Root-relative directories never entered (forward slashes): the ones the
    /// code graph's index skipped (its excluded directories and nested checkouts),
    /// named by the caller, who reports them.
    pub excluded_dir_paths: std::collections::BTreeSet<String>,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            covered_paths: std::collections::BTreeSet::new(),
            excluded_dir_paths: std::collections::BTreeSet::new(),
        }
    }
}

/// [`ingest_directory_with_limit`] with the rest of the options.
pub fn ingest_directory_with(
    root: &Path,
    options: &IngestOptions,
) -> Result<IngestReport, IngestError> {
    let paths = walk::walk_corpus_excluding(root, &options.excluded_dir_paths)?;

    let mut report = IngestReport::default();
    for path in paths {
        if parse::covered_code(&path, root, &options.covered_paths) {
            report.skipped.push(SkippedFile {
                path,
                reason: SkipReason::CoveredElsewhere,
            });
            continue;
        }
        match parse::parse_file(&path, root, options.max_file_size) {
            Ok(parse::ParseOutcome::Parsed(document)) => report.documents.push(document),
            Ok(parse::ParseOutcome::Skipped(reason)) => {
                report.skipped.push(SkippedFile { path, reason });
            }
            Err(error) => report.failures.push(IngestFailure { path, error }),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingests_a_mixed_corpus_and_skips_the_one_unsupported_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("readme.md"), "# Title\n\nBody text.\n")
            .expect("write markdown fixture");
        std::fs::write(dir.path().join("notes.txt"), "plain notes").expect("write text fixture");
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").expect("write code fixture");
        // No parser is registered for images; this must surface as a
        // counted skip, not vanish from the corpus and not be counted as a
        // failure (nothing went wrong reading it; it was never in scope).
        std::fs::write(dir.path().join("diagram.png"), [0u8; 8]).expect("write image fixture");

        let report = ingest_directory(dir.path()).expect("ingest");

        assert_eq!(report.documents.len(), 3);
        assert!(report.failures.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].path.file_name().unwrap(), "diagram.png");
        assert!(matches!(
            report.skipped[0].reason,
            SkipReason::UnsupportedExtension { .. }
        ));

        let markdown_doc = report
            .documents
            .iter()
            .find(|d| d.kind == DocumentKind::Markdown)
            .expect("markdown document present");
        assert_eq!(markdown_doc.sections.len(), 1);
        assert_eq!(markdown_doc.sections[0].heading, "Title");
    }

    #[test]
    fn a_lockfile_is_skipped_not_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("Cargo.lock"), "# generated").expect("write fixture");

        let report = ingest_directory(dir.path()).expect("ingest");

        assert!(report.documents.is_empty());
        assert!(report.failures.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(matches!(report.skipped[0].reason, SkipReason::LockFile));
    }

    #[test]
    fn a_file_over_the_configured_limit_is_a_failure_not_a_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("big.txt"), vec![b'x'; 4096]).expect("write fixture");

        let report = ingest_directory_with_limit(dir.path(), 1024).expect("ingest must not abort");

        assert!(report.documents.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert!(matches!(
            report.failures[0].error,
            IngestError::TooLarge { .. }
        ));
    }

    #[test]
    fn covered_code_is_a_counted_skip_and_prose_is_still_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").expect("write");
        std::fs::create_dir_all(dir.path().join("build")).expect("dir");
        std::fs::write(dir.path().join("build/gen.rs"), "fn g() {}").expect("write");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").expect("write");
        std::fs::write(dir.path().join("readme.md"), "# R\n").expect("write");
        // Covered means listed (indexed elsewhere), not "has a code extension":
        // `build/gen.rs` is not listed, so it is read.
        let options = IngestOptions {
            covered_paths: ["main.rs".to_owned(), "readme.md".to_owned()].into(),
            ..IngestOptions::default()
        };
        let report = ingest_directory_with(dir.path(), &options).expect("ingest");
        let names: Vec<_> = report
            .documents
            .iter()
            .map(|d| d.path.file_name().expect("name").to_owned())
            .collect();
        // Markdown is prose: listing it never skips it.
        assert!(names.contains(&"readme.md".into()));
        assert!(names.contains(&"Cargo.toml".into()));
        assert!(names.contains(&"gen.rs".into()));
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].path.file_name().expect("name"), "main.rs");
        assert!(matches!(
            &report.skipped[0].reason,
            SkipReason::CoveredElsewhere
        ));
    }

    #[test]
    fn missing_root_is_a_hard_error() {
        let err = ingest_directory(Path::new("/does/not/exist/kgraf-ingest"))
            .expect_err("missing root must error");
        assert!(matches!(err, IngestError::RootUnreadable { .. }));
    }
}
