//! PDF text extraction with `pdf-extract`.
//!
//! PDF carries no portable heading markup this crate can rely on (font-size
//! heuristics are out of scope for the parsing half of the pipeline), so a PDF
//! document has text but no sections.
//!
//! `pdf-extract` (and the `lopdf` object model underneath it) is known to
//! panic on some malformed PDFs rather than returning `Err` -- it is a text
//! extractor, not a hardened parser for adversarial input. Left alone, one
//! such PDF would unwind straight through `ingest_directory` and abort the
//! whole corpus over one bad file. `extract` therefore runs the parser
//! inside `std::panic::catch_unwind` and turns a caught panic into
//! `IngestError::PdfPanicked` for that file only.

use std::cell::RefCell;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Mutex;

use crate::error::IngestError;

type Parser = fn(&Path) -> Result<String, pdf_extract::OutputError>;

fn real_parser(path: &Path) -> Result<String, pdf_extract::OutputError> {
    pdf_extract::extract_text(path)
}

thread_local! {
    // Test-only injection seam (see `extract_with` and the tests below): a
    // test on this thread can swap in a parser that panics on demand,
    // without needing a real PDF crafted to panic this exact `pdf-extract`
    // version, which is both fragile across upgrades and not guaranteed to
    // exist.
    static TEST_PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_test_parser(parser: Option<Parser>) {
    TEST_PARSER.with(|cell| *cell.borrow_mut() = parser);
}

pub fn extract(path: &Path) -> Result<String, IngestError> {
    let parser = TEST_PARSER.with(|cell| cell.borrow().unwrap_or(real_parser as Parser));
    extract_with(path, parser)
}

fn extract_with<F>(path: &Path, parser: F) -> Result<String, IngestError>
where
    F: FnOnce(&Path) -> Result<String, pdf_extract::OutputError>,
{
    let guard = suppress_panic_output();
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| parser(path)));
    drop(guard);

    match outcome {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(source)) => Err(IngestError::Pdf {
            path: path.to_path_buf(),
            source,
        }),
        Err(_panic_payload) => Err(IngestError::PdfPanicked {
            path: path.to_path_buf(),
        }),
    }
}

/// Global panic hooks are process-wide, so installing a silent one here
/// would also swallow an unrelated panic message on another thread for as
/// long as the hook is installed. The mutex bounds that window to "one PDF
/// is being parsed" by serializing installs, and the `Drop` guard restores
/// the previous hook unconditionally (including if `catch_unwind` itself
/// somehow failed to catch, or the calling thread later panics), so the
/// swap never leaks past this function. It does not make the suppression
/// thread-local -- `std::panic` has no per-thread hook API -- but it makes
/// it exclusive and bounded, which is the practical version of "scoped"
/// available on stable Rust.
type PanicHook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Sync + Send>;

struct PanicOutputGuard {
    previous: Option<PanicHook>,
    _permit: std::sync::MutexGuard<'static, ()>,
}

impl Drop for PanicOutputGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            panic::set_hook(previous);
        }
    }
}

fn suppress_panic_output() -> PanicOutputGuard {
    static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());
    let permit = PANIC_HOOK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    PanicOutputGuard {
        previous: Some(previous),
        _permit: permit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Object, Stream, dictionary};

    /// Builds a minimal one-page PDF with a `Tj` text-show operation, using
    /// `lopdf` directly rather than committing a binary fixture. `lopdf` is
    /// `pdf-extract`'s own PDF object-model dependency (transitively, at a
    /// different pinned version here as a dev-dependency), so this exercises
    /// the same object model `pdf-extract` reads.
    fn build_fixture(text: &str) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();

        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });

        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![72.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal(text)]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));

        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });

        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );

        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).expect("save synthetic pdf");
        bytes
    }

    #[test]
    fn extracts_text_from_a_synthetic_pdf() {
        let bytes = build_fixture("kGraf ingest test");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fixture.pdf");
        std::fs::write(&path, bytes).expect("write fixture");

        let text = extract(&path).expect("extract pdf text");
        assert!(text.contains("kGraf ingest test"));
    }

    #[test]
    fn reports_a_typed_error_for_a_non_pdf_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not-a-pdf.pdf");
        std::fs::write(&path, b"this is not a pdf").expect("write bogus file");

        let err = extract(&path).expect_err("garbage bytes must not parse as a PDF");
        // Any OutputError variant is acceptable; what matters is that it is
        // Err, not a silently empty Ok(String::new()).
        assert!(matches!(err, IngestError::Pdf { .. }));
    }

    #[test]
    fn a_parser_panic_is_caught_and_reported_as_pdf_panicked() {
        let path = Path::new("irrelevant-since-the-parser-is-fake.pdf");
        let err = extract_with(path, |_path| -> Result<String, pdf_extract::OutputError> {
            panic!("simulated pdf-extract panic on malformed input");
        })
        .expect_err("a panicking parser must become an error, not unwind past this call");

        assert!(matches!(err, IngestError::PdfPanicked { .. }));
    }

    #[test]
    fn other_files_still_succeed_when_one_pdf_panics_during_a_full_ingest() {
        // Exercises the seam through the public `ingest_directory` entry
        // point, not just this module's `extract_with`, so the assertion
        // covers the thing that matters: the panic stays
        // isolated to the one file at the whole-corpus level.
        struct ResetTestParser;
        impl Drop for ResetTestParser {
            fn drop(&mut self) {
                set_test_parser(None);
            }
        }
        set_test_parser(Some(|_path| -> Result<String, pdf_extract::OutputError> {
            panic!("simulated pdf-extract panic")
        }));
        let _reset = ResetTestParser;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("good.txt"), "this file is fine")
            .expect("write good fixture");
        std::fs::write(dir.path().join("bad.pdf"), b"pretend pdf bytes")
            .expect("write pdf fixture");

        let report = crate::ingest_directory(dir.path()).expect("ingest must not abort");

        assert_eq!(report.documents.len(), 1, "the good file must still parse");
        assert_eq!(report.documents[0].path.file_name().unwrap(), "good.txt");

        assert_eq!(
            report.failures.len(),
            1,
            "the panicking pdf must be reported"
        );
        assert_eq!(report.failures[0].path.file_name().unwrap(), "bad.pdf");
        assert!(matches!(
            report.failures[0].error,
            IngestError::PdfPanicked { .. }
        ));
    }
}
