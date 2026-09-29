//! What a tool returns to the model.
//!
//! **Binary is summarized, never base64** (from LibreChat's
//! `parsers.ts`). A NUL byte marks a payload as binary. Base64 of an image or an
//! archive costs input tokens the model cannot use, so the model is told what the
//! payload is (size, media type, digest) instead of being sent its bytes.
//!
//! **Failure is stated in the text too.** One provider ignores the tool-result
//! error flag, so a failed result carries `ERROR:` in its content as well as
//! `is_error` (see `kgraf_provider::message::Content::ToolResult`).
//!
//! **Text is untrusted.** Tool output is written by whatever the tool read: a web
//! page, a file, an MCP server. It is held as [`UntrustedText`], whose only exit to
//! the model is the tool-result block built by [`ToolOutput::into_content`].

use crate::untrusted::UntrustedText;
use kgraf_provider::message::Content;

/// A description of a binary payload, sent in place of its bytes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BinarySummary {
    /// Media type, where the source declared one.
    pub mime: Option<String>,
    /// Payload size in bytes.
    pub bytes: u64,
    /// blake3 of the payload, hex, so two summaries of the same bytes are
    /// recognizably the same payload.
    pub blake3: String,
}

/// One part of a tool's output.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputPart {
    /// Text, from outside kGraf.
    Text { text: UntrustedText },
    /// A binary payload, summarized.
    Binary(BinarySummary),
}

impl OutputPart {
    /// Classify raw bytes: a NUL byte or invalid UTF-8 makes them binary.
    ///
    /// Invalid UTF-8 is treated as binary as well as NUL, because lossy decoding
    /// would put replacement characters in front of the model and call it text.
    pub fn from_bytes(bytes: &[u8], mime: Option<&str>) -> Self {
        if !bytes.contains(&0)
            && let Ok(text) = std::str::from_utf8(bytes)
        {
            return OutputPart::Text {
                text: UntrustedText::new(text),
            };
        }
        OutputPart::Binary(BinarySummary {
            mime: mime.map(str::to_owned),
            bytes: bytes.len() as u64,
            blake3: blake3::hash(bytes).to_hex().to_string(),
        })
    }

    /// The part as the model sees it, for the tool-result sink only.
    fn render(self) -> String {
        match self {
            OutputPart::Text { text } => text.into_tool_result(),
            OutputPart::Binary(b) => format!(
                "[binary payload not shown: {} bytes, {}, blake3 {}]",
                b.bytes,
                b.mime.as_deref().unwrap_or("unknown type"),
                &b.blake3[..16.min(b.blake3.len())],
            ),
        }
    }
}

/// A tool call's result.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolOutput {
    pub parts: Vec<OutputPart>,
    /// The tool ran and failed. The model sees the failure.
    pub is_error: bool,
}

impl ToolOutput {
    /// A successful text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            parts: vec![OutputPart::Text {
                text: UntrustedText::new(text),
            }],
            is_error: false,
        }
    }

    /// A failed text result.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            parts: vec![OutputPart::Text {
                text: UntrustedText::new(text),
            }],
            is_error: true,
        }
    }

    /// The result as the model sees it, in a tool-result block: parts joined by
    /// newlines, with a leading `ERROR: ` on a failure that does not already say so.
    /// This is the sink; there is no other way to read the text out.
    pub fn render(self) -> String {
        let body = self
            .parts
            .into_iter()
            .map(OutputPart::render)
            .collect::<Vec<_>>()
            .join("\n");
        if self.is_error && !body.starts_with("ERROR") {
            format!("ERROR: {body}")
        } else {
            body
        }
    }

    /// The provider-neutral tool-result block for this output.
    pub fn into_content(self, tool_use_id: impl Into<String>) -> Content {
        Content::ToolResult {
            tool_use_id: tool_use_id.into(),
            is_error: self.is_error,
            content: self.render(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nul_byte_makes_a_payload_binary_and_it_is_summarized_not_encoded() {
        let bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        let part = OutputPart::from_bytes(bytes, Some("image/png"));
        let OutputPart::Binary(summary) = &part else {
            panic!("expected binary, got {part:?}");
        };
        assert_eq!(summary.bytes, bytes.len() as u64);
        let rendered = part.clone().render();
        assert!(rendered.contains("image/png"));
        assert!(rendered.contains(&bytes.len().to_string()));
        // No base64 of the payload leaks into what the model sees.
        assert!(!rendered.contains("iVBOR"));
    }

    #[test]
    fn invalid_utf8_without_nul_is_binary_too() {
        let part = OutputPart::from_bytes(&[0xff, 0xfe, 0x41], None);
        assert!(matches!(part, OutputPart::Binary(_)));
        assert!(part.render().contains("unknown type"));
    }

    #[test]
    fn plain_utf8_stays_text() {
        let part = OutputPart::from_bytes("héllo".as_bytes(), None);
        assert_eq!(
            part,
            OutputPart::Text {
                text: UntrustedText::new("héllo")
            }
        );
    }

    #[test]
    fn a_failure_is_carried_in_the_flag_and_the_text() {
        let content = ToolOutput::error("file not found").into_content("t1");
        match content {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "t1");
                assert!(is_error);
                assert_eq!(content, "ERROR: file not found");
            }
            other => panic!("wrong block: {other:?}"),
        }
        // Not doubled when the tool already said so.
        assert_eq!(ToolOutput::error("ERROR: x").render(), "ERROR: x");
        assert_eq!(ToolOutput::text("ok").render(), "ok");
    }
}
