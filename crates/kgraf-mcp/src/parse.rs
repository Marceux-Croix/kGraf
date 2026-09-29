//! MCP `tools/call` result parsing.
//!
//! Ported from LibreChat's `packages/api/src/mcp/parsers.ts` (MIT), adapted to
//! `kgraf_tools::ToolOutput`'s stronger rule: **binary is summarized, never sent
//! as base64, ever** (not only above a size limit as LibreChat's `formatToolContent`
//! does for images). `rmcp`'s `ContentBlock` already tells us which parts are
//! declared binary (`image`, `audio`), so those always become a [`BinarySummary`].
//!
//! **The NUL-byte heuristic applies wherever text and binary are otherwise
//! ambiguous** (originally applied only to a resource's `blob`
//! field, not to plain `text` content or a resource's `text` field). MCP requires
//! `text` content to be a JSON string, which is always valid UTF-8, but a JSON
//! string can still carry an embedded NUL codepoint; the same rule `git` uses to
//! classify a file applies here too, so a `text` block or a `TextResourceContents`
//! carrying one is summarized as binary rather than passed through.
//!
//! **Metadata is flattened, not the payload, and never re-capped with it.** A
//! resource's `uri`, `mimeType`, `name`, `title` and `description` are
//! attacker-controlled strings a hostile server could use to forge a fake second
//! field with an embedded line break; each is rendered through
//! [`UntrustedText::to_one_line`]. The text or resource body is capped by itself,
//! before it is combined with its metadata (capping the already
//! -combined string could truncate the metadata lines that followed the body).
//!
//! **Sizes are capped**, in three ways: one text or resource-text part
//! ([`MAX_TEXT_PART_CHARS`]); the number of content blocks one result may
//! contribute ([`MAX_OUTPUT_PARTS`]); and the total size of one result
//! ([`MAX_TOTAL_OUTPUT_BYTES`]), all with a visible truncation marker rather than a
//! silent drop (fail loud). A `BinarySummary` is already the smallest possible
//! representation of a binary payload (mime, byte count, `blake3`), so it needs no
//! further capping and does not count meaningfully against the total.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use kgraf_tools::{BinarySummary, OutputPart, ToolOutput, UntrustedText};
use rmcp::model::{CallToolResult, ContentBlock, Resource, ResourceContents};

/// The longest one text or resource-text part is allowed to be, in `char`s, before
/// it is truncated with a visible marker. Chosen generously: large enough that a
/// normal file read or command output is never cut, small enough that one runaway
/// tool result cannot dominate a worker's private remainder (0057).
pub const MAX_TEXT_PART_CHARS: usize = 65_536;

/// The longest a single metadata label (a resource's name, uri, mime type, ...) may
/// be before [`UntrustedText::to_one_line`] truncates it.
pub const MAX_METADATA_CHARS: usize = 512;

/// The most content blocks one `tools/call` result may contribute (bounded memory). A well-behaved tool answers with a handful of blocks;
/// this bounds a server that answers with an unbounded list of them.
pub const MAX_OUTPUT_PARTS: usize = 256;

/// The most total bytes one `tools/call` result may contribute across all of its
/// parts (bounded memory), measured as the sum of each text
/// part's byte length (a `BinarySummary` is already tiny and is not counted).
pub const MAX_TOTAL_OUTPUT_BYTES: usize = 1_048_576;

/// Convert a server's `tools/call` response into what the model sees.
pub fn parse_call_tool_result(result: CallToolResult) -> ToolOutput {
    let is_error = result.is_error.unwrap_or(false);
    let total_blocks = result.content.len();
    let mut parts: Vec<OutputPart> = Vec::new();
    let mut total_bytes: usize = 0;
    for block in result.content {
        if parts.len() >= MAX_OUTPUT_PARTS {
            parts.push(text_part(&format!(
                "[{} further content block(s) omitted: over the {MAX_OUTPUT_PARTS}-part limit]",
                total_blocks.saturating_sub(parts.len()),
            )));
            break;
        }
        let part = content_part(block);
        let part_bytes = part_byte_size(&part);
        if !parts.is_empty() && total_bytes.saturating_add(part_bytes) > MAX_TOTAL_OUTPUT_BYTES {
            parts.push(text_part(&format!(
                "[remaining content omitted: over the {MAX_TOTAL_OUTPUT_BYTES}-byte total output limit]"
            )));
            break;
        }
        total_bytes = total_bytes.saturating_add(part_bytes);
        parts.push(part);
    }
    if parts.is_empty() {
        parts.push(text_part("(no content)"));
    }
    ToolOutput { parts, is_error }
}

fn part_byte_size(part: &OutputPart) -> usize {
    match part {
        OutputPart::Text { text } => text.len(),
        // A `BinarySummary` is a handful of small fields; not worth measuring
        // precisely against a byte budget meant to bound unbounded text.
        OutputPart::Binary(_) => 128,
    }
}

fn content_part(block: ContentBlock) -> OutputPart {
    match block {
        ContentBlock::Text(t) => text_content_part(&t.text),
        ContentBlock::Image(i) => binary_part(&i.data, &i.mime_type),
        ContentBlock::Audio(a) => binary_part(&a.data, &a.mime_type),
        ContentBlock::Resource(r) => resource_part(r.resource),
        ContentBlock::ResourceLink(link) => resource_link_part(&link),
        // `ContentBlock` is `#[non_exhaustive]`: a future MCP revision may add a
        // variant. Fail loud, not silent (fail loud): the model still sees the raw
        // block as JSON, rather than the tool result quietly dropping a part.
        other => text_part(&format!(
            "[unrecognized MCP content block, shown as JSON: {}]",
            serde_json::to_string(&other).unwrap_or_else(|_| "<unserializable>".to_owned())
        )),
    }
}

/// A `text` content block: `text` is guaranteed valid UTF-8 by MCP's JSON encoding,
/// but a JSON string can still carry an embedded NUL codepoint (the NUL heuristic applies here too, not only to a resource's ambiguous `blob`
/// field).
fn text_content_part(text: &str) -> OutputPart {
    if text.as_bytes().contains(&0) {
        return OutputPart::Binary(BinarySummary {
            mime: None,
            bytes: text.len() as u64,
            blake3: blake3::hash(text.as_bytes()).to_hex().to_string(),
        });
    }
    text_part(text)
}

/// A declared-binary content block (`image` or `audio`): always summarized, never
/// decoded into text, regardless of what the decoded bytes look like. Unlike the
/// blob branch of [`resource_part`], there is no ambiguity to resolve here: the
/// wire type already says "binary".
fn binary_part(data: &str, mime: &str) -> OutputPart {
    match BASE64.decode(data) {
        Ok(bytes) => OutputPart::Binary(BinarySummary {
            mime: Some(mime.to_owned()),
            bytes: bytes.len() as u64,
            blake3: blake3::hash(&bytes).to_hex().to_string(),
        }),
        Err(_) => text_part(&format!(
            "[invalid base64 in a {mime} content block, {} chars]",
            data.len()
        )),
    }
}

/// `Some(text)` when `bytes` is unambiguously text (no NUL byte, valid UTF-8);
/// `None` when it should be treated as binary. Computed once, so the caller never
/// needs to re-parse a slice it has already validated (removes
/// an `expect` that existed only because the original code re-derived `&str` from
/// bytes a match guard had already checked).
fn text_if_unambiguous(bytes: &[u8]) -> Option<&str> {
    if bytes.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

fn resource_part(resource: ResourceContents) -> OutputPart {
    match resource {
        ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } => match text_if_unambiguous(text.as_bytes()) {
            Some(text) => composed_resource_text(text, mime_type.as_deref(), &uri),
            None => OutputPart::Binary(BinarySummary {
                mime: mime_type,
                bytes: text.len() as u64,
                blake3: blake3::hash(text.as_bytes()).to_hex().to_string(),
            }),
        },
        ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            ..
        } => match BASE64.decode(&blob) {
            Ok(bytes) => match text_if_unambiguous(&bytes) {
                Some(text) => composed_resource_text(text, mime_type.as_deref(), &uri),
                None => OutputPart::Binary(BinarySummary {
                    mime: mime_type,
                    bytes: bytes.len() as u64,
                    blake3: blake3::hash(&bytes).to_hex().to_string(),
                }),
            },
            Err(_) => text_part(&format!(
                "[resource blob at {} had invalid base64 data]",
                one_line(&uri)
            )),
        },
        // `ResourceContents` is `#[non_exhaustive]`: fail loud on a future variant
        // (fail loud), same as the catch-all in `content_part`.
        other => text_part(&format!(
            "[unrecognized MCP resource content, shown as JSON: {}]",
            serde_json::to_string(&other).unwrap_or_else(|_| "<unserializable>".to_owned())
        )),
    }
}

/// Build a resource's text part: the body capped on its own, then its metadata
/// (already flattened to one line each) appended -- never the other way around
/// (appending metadata to an already-capped body, rather than
/// capping the combined string, is what stops the body's own truncation from
/// eating the metadata that follows it).
fn composed_resource_text(text: &str, mime_type: Option<&str>, uri: &str) -> OutputPart {
    let mut body = format!(
        "Resource Text: {}\nResource URI: {}",
        cap(text),
        one_line(uri)
    );
    if let Some(mime_type) = mime_type {
        body.push_str(&format!("\nResource MIME Type: {}", one_line(mime_type)));
    }
    composed_part(body)
}

fn resource_link_part(link: &Resource) -> OutputPart {
    let mut lines = vec![format!("Resource Name: {}", one_line(&link.name))];
    if let Some(title) = &link.title {
        lines.push(format!("Resource Title: {}", one_line(title)));
    }
    if let Some(description) = &link.description {
        lines.push(format!("Resource Description: {}", one_line(description)));
    }
    lines.push(format!("Resource URI: {}", one_line(&link.uri)));
    if let Some(mime_type) = &link.mime_type {
        lines.push(format!("Resource MIME Type: {}", one_line(mime_type)));
    }
    if let Some(size) = link.size {
        lines.push(format!("Resource Size: {size} bytes"));
    }
    // Every line here is already bounded by `one_line`'s own cap; capping the join
    // again (as `text_part` would) risks truncating a later metadata line for no
    // reason, the same bug `composed_resource_text` avoids.
    composed_part(lines.join("\n"))
}

/// Wrap an already-finished string as a text part, with no further capping. For
/// strings assembled from parts that were each already bounded (a capped body plus
/// flattened metadata, or metadata alone).
fn composed_part(body: String) -> OutputPart {
    OutputPart::Text {
        text: UntrustedText::new(body),
    }
}

/// Wrap and cap a single, not-yet-bounded string as a text part: plain content,
/// diagnostic messages, and anything else with no metadata appended after it.
fn text_part(text: &str) -> OutputPart {
    OutputPart::Text {
        text: UntrustedText::new(cap(text)),
    }
}

fn one_line(text: &str) -> String {
    UntrustedText::new(text).to_one_line(MAX_METADATA_CHARS)
}

/// Truncate at [`MAX_TEXT_PART_CHARS`] with a visible marker. Unlike
/// `UntrustedText::to_description`, this keeps every character as written: a tool
/// result is payload the model needs verbatim (it may be source code, a diff, or
/// command output with meaningful control bytes), not a label meant for a UI.
fn cap(text: &str) -> String {
    if text.chars().count() <= MAX_TEXT_PART_CHARS {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(MAX_TEXT_PART_CHARS).collect();
    out.push_str(" [truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{AudioContent, EmbeddedResource, ImageContent, TextContent};

    fn render(parts: Vec<ContentBlock>, is_error: Option<bool>) -> ToolOutput {
        // `CallToolResult` is `#[non_exhaustive]`: build it through `success`, then
        // set the field directly (still `pub`; `#[non_exhaustive]` blocks struct-
        // literal construction and exhaustive matching from another crate, not
        // access to a `pub` field) so the test can also cover the wire's `None`
        // ("absent means complete, not an error") alongside `Some(true/false)`.
        let mut result = CallToolResult::success(parts);
        result.is_error = is_error;
        parse_call_tool_result(result)
    }

    #[test]
    fn text_content_passes_through() {
        let out = render(vec![ContentBlock::Text(TextContent::new("hi"))], None);
        assert_eq!(out.render(), "hi");
    }

    /// A NUL byte marks even a plain `text` block binary.
    #[test]
    fn text_content_with_a_nul_byte_is_treated_as_binary() {
        let out = render(vec![ContentBlock::Text(TextContent::new("a\0b"))], None);
        let rendered = out.render();
        assert!(
            rendered.contains("3 bytes") || rendered.contains("bytes"),
            "{rendered}"
        );
        assert!(!rendered.contains('\0'));
    }

    #[test]
    fn an_image_is_summarized_never_sent_as_base64() {
        let data = BASE64.encode(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        let out = render(
            vec![ContentBlock::Image(ImageContent::new(
                data.clone(),
                "image/png",
            ))],
            None,
        );
        let rendered = out.render();
        assert!(rendered.contains("image/png"));
        assert!(
            !rendered.contains(&data),
            "base64 must not leak: {rendered}"
        );
    }

    #[test]
    fn small_valid_utf8_audio_is_still_summarized_never_decoded_as_text() {
        // No NUL byte and valid UTF-8 once decoded, but declared `audio`: the wire
        // type, not a content sniff, decides.
        let data = BASE64.encode(b"not actually audio but valid utf8");
        let out = render(
            vec![ContentBlock::Audio(AudioContent::new(
                data.clone(),
                "audio/wav",
            ))],
            None,
        );
        let rendered = out.render();
        assert!(rendered.contains("audio/wav"));
        assert!(!rendered.contains("not actually audio"));
    }

    #[test]
    fn a_text_resource_keeps_its_body_and_flattens_its_metadata() {
        let resource = ResourceContents::TextResourceContents {
            uri: "file:///a\nX-Forged: 1".into(),
            mime_type: Some("text/plain".into()),
            text: "line one\nline two".into(),
            meta: None,
        };
        let out = render(
            vec![ContentBlock::Resource(EmbeddedResource::new(resource))],
            None,
        );
        let rendered = out.render();
        assert!(rendered.contains("line one\nline two"), "{rendered}");
        assert!(!rendered.contains("file:///a\nX-Forged"), "{rendered}");
        assert_eq!(rendered.matches("Resource URI:").count(), 1);
    }

    /// A NUL byte in a resource's `text` field, not only its
    /// `blob` field, marks it binary.
    #[test]
    fn a_text_resource_with_a_nul_byte_is_treated_as_binary() {
        let resource = ResourceContents::TextResourceContents {
            uri: "file:///a".into(),
            mime_type: Some("text/plain".into()),
            text: "zebra\0giraffe\0lion".into(),
            meta: None,
        };
        let out = render(
            vec![ContentBlock::Resource(EmbeddedResource::new(resource))],
            None,
        );
        let rendered = out.render();
        assert!(!rendered.contains("zebra"), "{rendered}");
        assert!(!rendered.contains("giraffe"), "{rendered}");
        assert!(rendered.contains("bytes"), "{rendered}");
    }

    /// Capping a resource's body must never eat the metadata
    /// lines that follow it. A body sized to land exactly at the cap, with metadata
    /// after it, used to lose the metadata to the single combined-string cap.
    #[test]
    fn a_long_resource_body_is_capped_without_losing_its_metadata() {
        let text = "y".repeat(MAX_TEXT_PART_CHARS + 1_000);
        let resource = ResourceContents::TextResourceContents {
            uri: "file:///big.txt".into(),
            mime_type: Some("text/plain".into()),
            text,
            meta: None,
        };
        let out = render(
            vec![ContentBlock::Resource(EmbeddedResource::new(resource))],
            None,
        );
        let rendered = out.render();
        assert!(rendered.contains("Resource Text:"), "{rendered}");
        assert!(rendered.contains(" [truncated]"));
        assert!(
            rendered.contains("Resource URI: file:///big.txt"),
            "metadata must survive body truncation: {rendered}"
        );
        assert!(rendered.contains("Resource MIME Type: text/plain"));
    }

    #[test]
    fn a_binary_blob_resource_is_summarized_by_the_nul_heuristic() {
        let blob = BASE64.encode([0u8, 1, 2, 3]);
        let resource = ResourceContents::BlobResourceContents {
            uri: "file:///bin".into(),
            mime_type: Some("application/octet-stream".into()),
            blob,
            meta: None,
        };
        let out = render(
            vec![ContentBlock::Resource(EmbeddedResource::new(resource))],
            None,
        );
        let rendered = out.render();
        assert!(rendered.contains("application/octet-stream"));
        assert!(
            rendered.contains('4'),
            "byte count should be reported: {rendered}"
        );
    }

    #[test]
    fn a_resource_link_reports_flattened_metadata_only() {
        let link = Resource::new("file:///x\nInjected: 1", "readme")
            .with_title("Read Me")
            .with_size(42);
        let out = render(vec![ContentBlock::ResourceLink(link)], None);
        let rendered = out.render();
        assert!(rendered.contains("Resource Name: readme"));
        assert!(rendered.contains("Resource Size: 42 bytes"));
        assert!(!rendered.contains("file:///x\nInjected"));
    }

    #[test]
    fn an_error_result_is_carried_in_the_flag_and_the_text() {
        let out = render(
            vec![ContentBlock::Text(TextContent::new("boom"))],
            Some(true),
        );
        assert!(out.is_error);
        assert_eq!(out.render(), "ERROR: boom");
    }

    #[test]
    fn an_empty_result_is_not_silently_blank() {
        let out = render(vec![], None);
        assert_eq!(out.render(), "(no content)");
    }

    #[test]
    fn a_huge_text_part_is_capped_with_a_visible_marker() {
        let huge = "x".repeat(MAX_TEXT_PART_CHARS + 10);
        let out = render(vec![ContentBlock::Text(TextContent::new(huge))], None);
        let rendered = out.render();
        assert!(rendered.ends_with(" [truncated]"));
        assert!(rendered.len() < MAX_TEXT_PART_CHARS + 30);
    }

    /// A result with more content blocks than
    /// `MAX_OUTPUT_PARTS` is truncated with a visible marker, not admitted whole.
    #[test]
    fn a_result_with_too_many_parts_is_capped() {
        let blocks: Vec<ContentBlock> = (0..MAX_OUTPUT_PARTS + 50)
            .map(|i| ContentBlock::Text(TextContent::new(i.to_string())))
            .collect();
        let out = render(blocks, None);
        assert!(out.parts.len() <= MAX_OUTPUT_PARTS + 1);
        assert!(out.render().contains("further content block"));
    }

    /// Many small parts that together exceed the total
    /// output budget are capped, not just one huge part.
    #[test]
    fn a_result_over_the_total_byte_budget_is_capped() {
        let chunk = "z".repeat(10_000);
        let block_count = MAX_TOTAL_OUTPUT_BYTES / chunk.len() + 5;
        let blocks: Vec<ContentBlock> = (0..block_count)
            .map(|_| ContentBlock::Text(TextContent::new(chunk.clone())))
            .collect();
        let out = render(blocks, None);
        let total: usize = out
            .parts
            .iter()
            .map(|p| match p {
                OutputPart::Text { text } => text.len(),
                OutputPart::Binary(_) => 0,
            })
            .sum();
        assert!(out.render().contains("total output limit"));
        assert!(total < MAX_TOTAL_OUTPUT_BYTES + chunk.len() + 200);
    }
}
