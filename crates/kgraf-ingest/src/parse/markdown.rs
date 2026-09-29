//! Markdown extraction with `pulldown-cmark`.

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

use crate::model::Section;

/// Extract the plain text of a Markdown document and its sections.
///
/// A section starts at a heading and its text runs to the next heading of
/// any level (see `Section`'s doc comment for why nesting is flattened).
/// Text before the first heading has no section and is present only in the
/// returned full text.
pub fn extract(markdown: &str) -> (String, Vec<Section>) {
    let mut full_text = String::new();
    let mut sections: Vec<Section> = Vec::new();
    let mut in_heading = false;
    let mut heading_level: u8 = 1;
    let mut heading_text = String::new();

    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                in_heading = true;
                heading_level = heading_level_to_u8(level);
                heading_text.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
                sections.push(Section {
                    level: heading_level,
                    heading: heading_text.trim().to_string(),
                    text: String::new(),
                });
            }
            Event::Text(text) | Event::Code(text) => {
                if in_heading {
                    // The heading's own words go into `heading_text` (used
                    // to build the Section) and into the running document
                    // text, but not into any section body -- a heading is
                    // not part of what it introduces.
                    heading_text.push_str(&text);
                    full_text.push_str(&text);
                } else {
                    full_text.push_str(&text);
                    if let Some(section) = sections.last_mut() {
                        section.text.push_str(&text);
                    }
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                full_text.push('\n');
                if let Some(section) = sections.last_mut() {
                    section.text.push('\n');
                }
            }
            Event::End(TagEnd::Paragraph)
            | Event::End(TagEnd::Item)
            | Event::End(TagEnd::CodeBlock) => {
                full_text.push('\n');
                if let Some(section) = sections.last_mut() {
                    section.text.push('\n');
                }
            }
            _ => {}
        }
    }

    for section in &mut sections {
        section.text = section.text.trim().to_string();
    }

    (full_text.trim().to_string(), sections)
}

fn heading_level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_full_text_and_sections() {
        let markdown = "# Title\n\nIntro text.\n\n## Section One\n\nBody one.\n\n## Section Two\n\nBody two.\n";
        let (text, sections) = extract(markdown);

        assert!(text.contains("Title"));
        assert!(text.contains("Intro text."));
        assert!(text.contains("Body one."));
        assert!(text.contains("Body two."));

        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].heading, "Title");
        assert_eq!(sections[0].level, 1);
        assert_eq!(sections[0].text, "Intro text.");
        assert_eq!(sections[1].heading, "Section One");
        assert_eq!(sections[1].level, 2);
        assert_eq!(sections[1].text, "Body one.");
        assert_eq!(sections[2].heading, "Section Two");
        assert_eq!(sections[2].text, "Body two.");
    }

    #[test]
    fn text_with_no_headings_has_no_sections() {
        let (text, sections) = extract("Just a paragraph, no headings.");
        assert!(text.contains("Just a paragraph"));
        assert!(sections.is_empty());
    }
}
