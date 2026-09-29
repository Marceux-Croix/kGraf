//! Typed ingest errors.
//!
//! Fail loud: every failure mode a file can hit gets its own variant
//! rather than being folded into a generic "parse failed" case, so a caller
//! can distinguish "this PDF is corrupt" from "this PDF's parser panicked"
//! from "the root directory does not exist". A file that is not a document
//! at all (an unrecognized or binary extension, a lock file) is not an
//! error; see `model::SkipReason` and `IngestReport::skipped`.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum IngestError {
    #[error("root directory is not readable: {path}")]
    RootUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("walking {path}: {source}")]
    Walk {
        path: PathBuf,
        #[source]
        source: ignore::Error,
    },

    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not valid UTF-8 text")]
    InvalidUtf8 { path: PathBuf },

    #[error("extracting PDF text from {path}: {source}")]
    Pdf {
        path: PathBuf,
        #[source]
        source: pdf_extract::OutputError,
    },

    /// `pdf-extract` (and the `lopdf` object model it walks) is known to
    /// panic on some malformed PDFs rather than returning `Err`. That panic
    /// is caught at the call site (see `parse::pdf`) so one bad PDF cannot
    /// abort `ingest_directory` for the rest of the corpus; this variant is
    /// the caught result.
    #[error("the PDF parser panicked while reading {path}; the file is likely malformed")]
    PdfPanicked { path: PathBuf },

    #[error("reading DOCX {path}: {source}")]
    Docx {
        path: PathBuf,
        #[source]
        source: docx_rs::ReaderError,
    },

    #[error("{path} is {size} bytes, over the {limit} byte ingest limit")]
    TooLarge {
        path: PathBuf,
        size: u64,
        limit: u64,
    },

    #[error("stat {path}: {source}")]
    Stat {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl kgraf_diag::DiagCode for IngestError {
    fn diag_code(&self) -> &'static str {
        match self {
            IngestError::TooLarge { .. } => "KG-1101",
            IngestError::Pdf { .. }
            | IngestError::PdfPanicked { .. }
            | IngestError::Docx { .. } => "KG-1102",
            _ => kgraf_diag::catch_all_for("kgraf-ingest").code,
        }
    }

    fn diag_component(&self) -> &'static str {
        "kgraf-ingest"
    }
}

impl IngestError {
    /// The file the error is about, for a caller building a report keyed by
    /// path without pattern-matching every variant.
    pub fn path(&self) -> &std::path::Path {
        match self {
            IngestError::RootUnreadable { path, .. }
            | IngestError::Walk { path, .. }
            | IngestError::Read { path, .. }
            | IngestError::InvalidUtf8 { path }
            | IngestError::Pdf { path, .. }
            | IngestError::PdfPanicked { path }
            | IngestError::Docx { path, .. }
            | IngestError::TooLarge { path, .. }
            | IngestError::Stat { path, .. } => path,
        }
    }
}

#[cfg(test)]
mod diag_code_tests {
    use super::*;
    use kgraf_diag::DiagCode;

    /// Every `diag_code` branch resolves to a real glossary entry. `PdfPanicked` alone (no `pdf-extract`
    /// or `docx-rs` error value needed) exercises the branch it shares with
    /// `Pdf` and `Docx`.
    #[test]
    fn every_diag_code_branch_names_a_real_glossary_entry() {
        let instances: Vec<IngestError> = vec![
            IngestError::TooLarge {
                path: "x".into(),
                size: 1,
                limit: 0,
            },
            IngestError::PdfPanicked { path: "x".into() },
            // Not named in `diag_code`'s match: exercises the catch-all arm.
            IngestError::InvalidUtf8 { path: "x".into() },
        ];
        for e in &instances {
            let code = e.diag_code();
            assert!(
                kgraf_diag::GLOSSARY.iter().any(|spec| spec.code == code),
                "{e:?} names code {code}, which is not in kgraf_diag::GLOSSARY"
            );
        }
    }
}
