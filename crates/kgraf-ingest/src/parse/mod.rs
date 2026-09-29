//! Per-format extraction, dispatched by file extension.

pub(crate) mod docx;
mod markdown;
pub(crate) mod pdf;
mod text;

use std::path::Path;

use crate::error::IngestError;
use crate::model::{Document, DocumentKind, DocumentNotice, SkipReason};
use kgraf_core::{FileId, ids::stable_id};

/// Extensions treated as source code. Not exhaustive of every language
/// kGraf may ever meet; an extension missing here is a
/// `SkipReason::UnsupportedExtension` (never silently merged into another
/// kind), so a gap shows up as a visible, counted skip rather than a
/// document silently misclassified as prose.
const CODE_EXTENSIONS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "h", "cc", "cpp", "hpp", "cs", "rb",
    "sh", "bash", "zsh", "toml", "yaml", "yml", "json", "sql",
];

const PLAIN_TEXT_EXTENSIONS: &[&str] = &["txt", "rst", "adoc", "text", "log"];

/// Dependency-lockfile names recognized regardless of the extension they
/// happen to carry (`pnpm-lock.yaml` would otherwise classify as `Code` via
/// the `yaml` extension). These are generated artifacts, never authored
/// prose or planning content, so they are a skip rather than a document.
const LOCK_FILENAMES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "composer.lock",
    "Gemfile.lock",
    "poetry.lock",
    "Pipfile.lock",
    "go.sum",
];

fn is_lock_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    LOCK_FILENAMES.contains(&name) || path.extension().is_some_and(|ext| ext == "lock")
}

fn classify(path: &Path) -> Option<DocumentKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" => Some(DocumentKind::Markdown),
        "pdf" => Some(DocumentKind::Pdf),
        "docx" => Some(DocumentKind::Docx),
        other if PLAIN_TEXT_EXTENSIONS.contains(&other) => Some(DocumentKind::PlainText),
        other if CODE_EXTENSIONS.contains(&other) => Some(DocumentKind::Code),
        _ => None,
    }
}

/// Whether `path` is a source file listed in `covered` (root-relative, forward
/// slashes). Only `DocumentKind::Code` files qualify: prose is never covered.
pub fn covered_code(
    path: &Path,
    root: &Path,
    covered: &std::collections::BTreeSet<String>,
) -> bool {
    if covered.is_empty() || classify(path) != Some(DocumentKind::Code) {
        return false;
    }
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    covered.contains(&relative)
}

/// What became of one walked file: either it was never a document candidate
/// (`Skipped`) or it was and is now parsed (`Parsed`). A file that *was* a
/// candidate but could not be read is neither of these; it is an `Err`.
#[derive(Debug)]
pub enum ParseOutcome {
    Parsed(Document),
    Skipped(SkipReason),
}

/// Parse one file into a `Document`, given the corpus root it was found
/// under (used to derive a stable, root-relative id) and the maximum file
/// size this call will read into memory.
pub fn parse_file(
    path: &Path,
    root: &Path,
    max_file_size: u64,
) -> Result<ParseOutcome, IngestError> {
    if is_lock_file(path) {
        return Ok(ParseOutcome::Skipped(SkipReason::LockFile));
    }

    let Some(kind) = classify(path) else {
        let extension = path.extension().and_then(|e| e.to_str()).map(str::to_owned);
        return Ok(ParseOutcome::Skipped(SkipReason::UnsupportedExtension {
            extension,
        }));
    };

    // Stat before reading: a file over the limit is never loaded into
    // memory, regardless of format (a huge "PDF" or "DOCX" is still read as
    // whole-file bytes by `docx-rs`/`pdf-extract`, and plain text/code are
    // read directly by this crate).
    let metadata = std::fs::metadata(path).map_err(|source| IngestError::Stat {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > max_file_size {
        return Err(IngestError::TooLarge {
            path: path.to_path_buf(),
            size: metadata.len(),
            limit: max_file_size,
        });
    }

    let mut notices = Vec::new();
    let (text, sections) = match kind {
        DocumentKind::Markdown => {
            let raw = text::extract(path)?;
            markdown::extract(&raw)
        }
        DocumentKind::Pdf => {
            let extracted = pdf::extract(path)?;
            (extracted, Vec::new())
        }
        DocumentKind::Docx => {
            let bytes = std::fs::read(path).map_err(|source| IngestError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            let extraction = docx::extract(&bytes).map_err(|source| IngestError::Docx {
                path: path.to_path_buf(),
                source,
            })?;
            if extraction.sections.is_empty()
                && extraction.paragraph_count > docx::NO_HEADINGS_NOTICE_THRESHOLD
            {
                notices.push(DocumentNotice::DocxNoHeadingsDetected {
                    paragraph_count: extraction.paragraph_count,
                    paragraph_count_threshold: docx::NO_HEADINGS_NOTICE_THRESHOLD,
                });
            }
            (extraction.text, extraction.sections)
        }
        DocumentKind::PlainText | DocumentKind::Code => (text::extract(path)?, Vec::new()),
    };

    let relative = path.strip_prefix(root).unwrap_or(path);
    let id = FileId::new(stable_id("ingest-file", &[&relative.to_string_lossy()]));

    Ok(ParseOutcome::Parsed(Document {
        id,
        path: path.to_path_buf(),
        kind,
        text,
        sections,
        notices,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(path: &Path, root: &Path) -> Document {
        match parse_file(path, root, u64::MAX).expect("parse") {
            ParseOutcome::Parsed(document) => document,
            ParseOutcome::Skipped(reason) => panic!("expected a parsed document, got {reason:?}"),
        }
    }

    #[test]
    fn classify_covers_the_documented_kinds() {
        assert_eq!(classify(Path::new("a.md")), Some(DocumentKind::Markdown));
        assert_eq!(classify(Path::new("a.PDF")), Some(DocumentKind::Pdf));
        assert_eq!(classify(Path::new("a.docx")), Some(DocumentKind::Docx));
        assert_eq!(classify(Path::new("a.txt")), Some(DocumentKind::PlainText));
        assert_eq!(classify(Path::new("a.rs")), Some(DocumentKind::Code));
        assert_eq!(classify(Path::new("a.exe")), None);
        assert_eq!(classify(Path::new("no-extension")), None);
    }

    #[test]
    fn unsupported_extension_is_a_skip_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("image.png");
        std::fs::write(&path, [0u8; 4]).expect("write fixture");

        match parse_file(&path, dir.path(), u64::MAX).expect("must not error") {
            ParseOutcome::Skipped(SkipReason::UnsupportedExtension { extension }) => {
                assert_eq!(extension.as_deref(), Some("png"));
            }
            other => panic!("expected an unsupported-extension skip, got {other:?}"),
        }
    }

    #[test]
    fn a_recognized_lockfile_is_skipped_even_with_a_code_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `pnpm-lock.yaml` carries the `yaml` extension, which `classify`
        // would otherwise treat as code.
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(&path, "lockfileVersion: 6").expect("write fixture");

        match parse_file(&path, dir.path(), u64::MAX).expect("must not error") {
            ParseOutcome::Skipped(SkipReason::LockFile) => {}
            other => panic!("expected a lockfile skip, got {other:?}"),
        }
    }

    #[test]
    fn a_file_over_the_size_limit_is_reported_and_never_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge.txt");
        std::fs::write(&path, vec![b'a'; 1024]).expect("write fixture");

        let err = parse_file(&path, dir.path(), 100).expect_err("must be reported as too large");
        match err {
            IngestError::TooLarge { size, limit, .. } => {
                assert_eq!(size, 1024);
                assert_eq!(limit, 100);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn id_is_stable_across_repeated_parses_of_the_same_relative_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "hello").expect("write fixture");

        let first = parsed(&path, dir.path());
        let second = parsed(&path, dir.path());
        assert_eq!(first.id, second.id);
    }
}
