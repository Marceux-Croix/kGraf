//! Types shared across the walker and the parsers.

use std::path::PathBuf;

use kgraf_core::FileId;
use serde::{Deserialize, Serialize};

use crate::error::IngestError;

/// The kind of document a file parsed to, decided from its extension.
///
/// `Code` covers source files: the interpretation pass (not this crate's
/// concern) treats them differently from prose, but extraction is the same
/// "read the bytes as UTF-8 text" path as `PlainText`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocumentKind {
    Markdown,
    Pdf,
    Docx,
    PlainText,
    Code,
}

/// One heading and the text that follows it, up to the next heading of any
/// level. Nesting is intentionally flat: a subsection's text is folded into
/// its parent section's text as read order, rather than a tree, because the
/// interpretation pass (the other half of the pipeline) reads sections as spans of a
/// document, not as an outline it needs to walk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    /// 1 for a top-level heading (Markdown `#`, DOCX `Heading1`), 2 for the
    /// next level down, and so on.
    pub level: u8,
    pub heading: String,
    pub text: String,
}

/// A condition about a successfully parsed document worth surfacing, short
/// of a failure: the document became a `Document`, but something about it
/// means a consumer should not trust it at face value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocumentNotice {
    /// A DOCX with more than `paragraph_count_threshold` paragraphs produced
    /// zero sections. That is far more often a heading style this crate's
    /// detection missed (a custom template, a non-English style name) than
    /// a document that truly has no headings, and it would otherwise look
    /// identical to a short, genuinely headingless file (fail loud: a
    /// degraded read must not look like a clean one).
    DocxNoHeadingsDetected {
        paragraph_count: usize,
        paragraph_count_threshold: usize,
    },
}

/// One successfully parsed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    /// Stable id derived from the path relative to the ingested root, so a
    /// reindex of an unchanged tree produces the same id (matches the
    /// convention in `kgraf_core::ids`).
    pub id: FileId,
    pub path: PathBuf,
    pub kind: DocumentKind,
    /// The full extracted text, concatenated in document order. For
    /// Markdown and DOCX this is the same text the sections below are drawn
    /// from; for PDF, plain text and code there are no sections and this is
    /// the only content.
    pub text: String,
    /// Headings and their text. Empty for PDF, plain text and code, whose
    /// formats carry no structural heading markup this crate parses.
    pub sections: Vec<Section>,
    /// Conditions worth a consumer's attention that stopped short of a
    /// parse failure. Empty for the overwhelming majority of documents.
    pub notices: Vec<DocumentNotice>,
}

/// One file that could not be turned into a `Document`.
///
/// Fail loud: a file the walker finds is never dropped silently.
/// It becomes a `Document`, an `IngestFailure`, or a `SkippedFile`; the
/// caller decides what to do with each, but none of the three is ever
/// hidden. A failure is reserved for a file that *is* a document kGraf
/// should have been able to read and could not (too large, corrupt,
/// unreadable); see `SkippedFile` for a file that was never a document to
/// begin with.
#[derive(Debug)]
pub struct IngestFailure {
    pub path: PathBuf,
    pub error: IngestError,
}

/// Why a file was not treated as a candidate document at all. Distinct from
/// `IngestError`: nothing went wrong reading these files, they were simply
/// never in scope, and folding them into `failures` would drown real faults
/// in expected noise (a lockfile in every corpus with a package manager).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    /// The extension (or its absence) is not one this crate parses.
    UnsupportedExtension { extension: Option<String> },
    /// A recognized dependency-lockfile name (`Cargo.lock`,
    /// `package-lock.json`, `*.lock`, ...): generated, not authored, and
    /// never prose or planning content.
    LockFile,
    /// A source file the caller holds another way (the code graph indexed it),
    /// listed in `IngestOptions::covered_paths`.
    CoveredElsewhere,
}

/// One file that was walked but never treated as a document candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFile {
    pub path: PathBuf,
    pub reason: SkipReason,
}

/// The result of walking and parsing a whole corpus.
#[derive(Debug, Default)]
pub struct IngestReport {
    pub documents: Vec<Document>,
    pub failures: Vec<IngestFailure>,
    pub skipped: Vec<SkippedFile>,
}

impl IngestReport {
    pub fn is_fully_successful(&self) -> bool {
        self.failures.is_empty()
    }

    /// Total notices carried by successfully parsed documents, so a caller
    /// can tell "clean corpus" from "parsed, but N documents need a look"
    /// without iterating `documents` itself.
    pub fn notice_count(&self) -> usize {
        self.documents.iter().map(|d| d.notices.len()).sum()
    }
}
