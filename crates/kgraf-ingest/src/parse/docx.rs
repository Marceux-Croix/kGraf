//! DOCX extraction with `docx-rs`.
//!
//! `docx-rs` is a writer-first crate (it also targets WASM for a JS/TS
//! consumer) but ships a full OOXML reader; `read_docx` is what this parser
//! uses. A heading is decided, in order:
//!
//! 1. The paragraph's `w:outlineLvl` (`property.outline_lvl`), when present.
//!    This is Word's own "this paragraph is at outline depth N" signal and
//!    survives a renamed or localized style, which a style-id match cannot.
//! 2. Otherwise, a style id matching the standard convention `Heading<N>`
//!    or `Title`, case-insensitively (Word templates and non-English UIs
//!    both produce style ids that differ only in case, e.g. `heading 1`).
//!
//! Anything else is body text folded into the current section.

use docx_rs::{DocumentChild, ParagraphChild, RunChild};

use crate::model::Section;

/// A DOCX with more paragraphs than this that still yields zero sections
/// gets a `DocumentNotice::DocxNoHeadingsDetected` rather than looking
/// exactly like a short document that never had headings to begin with.
/// Chosen as "long enough that a real document of this length almost always
/// has at least one heading" without being so low that a genuinely
/// unstructured but substantial memo trips it constantly; it is a judgment
/// call the interpretation pass is free to override once it has real
/// corpora to measure against.
pub const NO_HEADINGS_NOTICE_THRESHOLD: usize = 20;

pub struct DocxExtraction {
    pub text: String,
    pub sections: Vec<Section>,
    pub paragraph_count: usize,
}

pub fn extract(bytes: &[u8]) -> Result<DocxExtraction, docx_rs::ReaderError> {
    let docx = docx_rs::read_docx(bytes)?;

    let mut full_text = String::new();
    let mut sections: Vec<Section> = Vec::new();
    let mut paragraph_count = 0usize;

    for child in &docx.document.children {
        let DocumentChild::Paragraph(paragraph) = child else {
            continue;
        };
        paragraph_count += 1;

        let paragraph_text = paragraph_text(paragraph);
        if paragraph_text.is_empty() {
            continue;
        }

        if let Some(level) = heading_level(paragraph) {
            full_text.push_str(&paragraph_text);
            full_text.push('\n');
            sections.push(Section {
                level,
                heading: paragraph_text,
                text: String::new(),
            });
        } else {
            full_text.push_str(&paragraph_text);
            full_text.push('\n');
            if let Some(section) = sections.last_mut() {
                if !section.text.is_empty() {
                    section.text.push('\n');
                }
                section.text.push_str(&paragraph_text);
            }
        }
    }

    Ok(DocxExtraction {
        text: full_text.trim().to_string(),
        sections,
        paragraph_count,
    })
}

fn paragraph_text(paragraph: &docx_rs::Paragraph) -> String {
    let mut text = String::new();
    for child in &paragraph.children {
        if let ParagraphChild::Run(run) = child {
            for run_child in &run.children {
                if let RunChild::Text(t) = run_child {
                    text.push_str(&t.text);
                }
            }
        }
    }
    text.trim().to_string()
}

/// See the module doc comment for the two-step rule this implements.
fn heading_level(paragraph: &docx_rs::Paragraph) -> Option<u8> {
    if let Some(outline) = &paragraph.property.outline_lvl {
        // `w:outlineLvl` is 0-based (0 == the first heading level); this
        // crate's `Section::level` is 1-based to match Markdown's `#`
        // convention, so it is shifted up by one.
        return u8::try_from(outline.v).ok().map(|v| v.saturating_add(1));
    }

    let style_id = paragraph.property.style.as_ref()?.val.as_str();
    if style_id.eq_ignore_ascii_case("Title") {
        return Some(1);
    }
    let lower = style_id.to_ascii_lowercase();
    let digits = lower.strip_prefix("heading")?.trim();
    digits.parse::<u8>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use docx_rs::{Docx, Paragraph, Run};

    fn build_fixture() -> Vec<u8> {
        let docx = Docx::new()
            .add_paragraph(
                Paragraph::new()
                    .style("Heading1")
                    .add_run(Run::new().add_text("Section One")),
            )
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Body of section one.")))
            .add_paragraph(
                Paragraph::new()
                    .style("Heading2")
                    .add_run(Run::new().add_text("Section Two")),
            )
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Body of section two.")));

        let mut buffer = Vec::new();
        docx.build()
            .pack(std::io::Cursor::new(&mut buffer))
            .expect("pack docx fixture");
        buffer
    }

    #[test]
    fn extracts_text_and_sections_from_headings() {
        let bytes = build_fixture();
        let extraction = extract(&bytes).expect("extract docx");

        assert!(extraction.text.contains("Section One"));
        assert!(extraction.text.contains("Body of section one."));
        assert!(extraction.text.contains("Section Two"));
        assert!(extraction.text.contains("Body of section two."));
        assert_eq!(extraction.paragraph_count, 4);

        let sections = extraction.sections;
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "Section One");
        assert_eq!(sections[0].level, 1);
        assert_eq!(sections[0].text, "Body of section one.");
        assert_eq!(sections[1].heading, "Section Two");
        assert_eq!(sections[1].level, 2);
        assert_eq!(sections[1].text, "Body of section two.");
    }

    #[test]
    fn heading_style_ids_are_matched_case_insensitively() {
        let docx = Docx::new()
            .add_paragraph(
                Paragraph::new()
                    .style("heading 1")
                    .add_run(Run::new().add_text("Lowercase Heading")),
            )
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Body text.")));
        let mut buffer = Vec::new();
        docx.build()
            .pack(std::io::Cursor::new(&mut buffer))
            .expect("pack docx fixture");

        let extraction = extract(&buffer).expect("extract docx");
        assert_eq!(extraction.sections.len(), 1);
        assert_eq!(extraction.sections[0].heading, "Lowercase Heading");
        assert_eq!(extraction.sections[0].level, 1);
    }

    #[test]
    fn outline_level_is_honored_even_without_a_heading_style_id() {
        let docx = Docx::new()
            .add_paragraph(
                Paragraph::new()
                    .outline_lvl(1) // 0-based: level 2
                    .add_run(Run::new().add_text("Outline-only heading")),
            )
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Body text.")));
        let mut buffer = Vec::new();
        docx.build()
            .pack(std::io::Cursor::new(&mut buffer))
            .expect("pack docx fixture");

        let extraction = extract(&buffer).expect("extract docx");
        assert_eq!(extraction.sections.len(), 1);
        assert_eq!(extraction.sections[0].heading, "Outline-only heading");
        assert_eq!(extraction.sections[0].level, 2);
    }
}
