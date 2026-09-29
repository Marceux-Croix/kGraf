//! The protocol-neutral request and response types.
//!
//! These are kGraf's own shapes, not any provider's. Decision 0006 requires
//! that provider types never escape their adapter crate, and one surveyed crate's
//! request type is crate-private so a raw request can never be constructed, so
//! the shape an adapter translates *into* lives here, in the open.
//!
//! Content is a **list of blocks, not a concatenated string**. One surveyed crate
//! joins Anthropic's text blocks into a single string, which destroys the
//! boundaries needed to place cache breakpoints, and breakpoint placement is where
//! the cost promise is actually kept.

use kgraf_core::TokenUsage;

/// Who is speaking.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One block of content.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Plain text.
    Text {
        text: String,
        /// Whether a cache breakpoint sits at the end of this block.
        ///
        /// A request, not a guarantee: whether the provider honors it is a
        /// capability question, and a provider that does not is recorded as not
        /// honoring it rather than assumed to have complied.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        cache_breakpoint: bool,
    },
    /// A tool call the model asked for.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// The result of a tool call.
    ToolResult {
        tool_use_id: String,
        content: String,
        /// Whether the tool failed.
        ///
        /// Carried *and* stated in `content` by convention, because one provider
        /// ignores this flag entirely, which would make a failed tool call
        /// indistinguishable from a successful one.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    /// Reasoning the model emitted, kept separate from text because it is
    /// accounted separately.
    Reasoning { text: String },
}

impl Content {
    /// A plain text block with no cache breakpoint.
    pub fn text(text: impl Into<String>) -> Self {
        Content::Text {
            text: text.into(),
            cache_breakpoint: false,
        }
    }

    /// A text block ending in a cache breakpoint.
    pub fn cached_text(text: impl Into<String>) -> Self {
        Content::Text {
            text: text.into(),
            cache_breakpoint: true,
        }
    }

    /// The text of this block, where it has any.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Content::Text { text, .. } | Content::Reasoning { text } => Some(text),
            Content::ToolResult { content, .. } => Some(content),
            Content::ToolUse { .. } => None,
        }
    }
}

/// One turn.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

impl Message {
    /// A user turn carrying one text block.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![Content::text(text)],
        }
    }

    /// An assistant turn carrying one text block.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![Content::text(text)],
        }
    }
}

/// A completion request, in kGraf's own shape.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Request {
    /// The model to use. An adapter that cannot serve it refuses before the
    /// request goes out.
    pub model: String,
    /// System prompt, as blocks so a breakpoint can sit inside it. The system
    /// prompt is the longest stable prefix in an agent loop and therefore the
    /// place a breakpoint earns the most.
    pub system: Vec<Content>,
    pub messages: Vec<Message>,
    pub max_output_tokens: u32,
    /// `None` leaves the provider's default alone rather than sending a value the
    /// caller did not choose.
    pub temperature: Option<f32>,
    /// Tool definitions, if any.
    pub tools: Vec<ToolDefinition>,
    /// Reasoning effort, where the provider has the capability.
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl Request {
    /// A minimal request: one model, one user message, one output ceiling.
    pub fn new(
        model: impl Into<String>,
        prompt: impl Into<String>,
        max_output_tokens: u32,
    ) -> Self {
        Self {
            model: model.into(),
            system: Vec::new(),
            messages: vec![Message::user(prompt)],
            max_output_tokens,
            temperature: None,
            tools: Vec::new(),
            reasoning_effort: None,
        }
    }

    /// Whether any block in this request asks for a cache breakpoint.
    pub fn wants_cache_control(&self) -> bool {
        let block_wants = |content: &Content| {
            matches!(
                content,
                Content::Text {
                    cache_breakpoint: true,
                    ..
                }
            )
        };
        self.system.iter().any(block_wants)
            || self
                .messages
                .iter()
                .any(|m| m.content.iter().any(block_wants))
    }
}

/// How hard the model should think, where the protocol exposes the control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

/// A tool the model may call.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input.
    pub input_schema: serde_json::Value,
}

/// Why the model stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its turn.
    EndTurn,
    /// The output ceiling was reached.
    MaxTokens,
    /// The model wants a tool called.
    ToolUse,
    /// A stop sequence matched.
    StopSequence,
    /// The turn paused mid-generation rather than ending: at least one
    /// provider reports this for a server-side tool (its own web search or
    /// code execution) still in progress. The turn is not finished and is
    /// not a tool call this build has to run; the caller continues it by
    /// sending the same conversation, with the paused response appended,
    /// back to the provider. Distinct from [`Self::Other`] because it is a
    /// modeled, expected mid-turn state rather than an unrecognized one, and
    /// the two call for opposite handling: `Other` ends the run, this
    /// continues it.
    PauseTurn,
    /// The provider reported something this build does not model. Carried rather
    /// than mapped to `EndTurn`, so an unmodeled terminal state is visible.
    Other,
}

/// A completion response.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    /// The model the provider says produced this. Asserted against the request.
    pub model: String,
    pub content: Vec<Content>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
    /// The provider's own request id, where it sends one, for reconciling a call
    /// against a provider-side invoice.
    pub request_id: Option<String>,
}

impl Response {
    /// The response's text blocks joined, for a caller that wants prose.
    ///
    /// The blocks themselves stay intact on the response; this is a convenience,
    /// not the storage form.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// The response's reasoning blocks joined, the `text()` counterpart for
    /// reasoning content (decision 0071): the settled,
    /// whole-round reasoning a resumed worker's context is rebuilt from,
    /// rather than concatenating individually-scrubbed streamed deltas.
    pub fn reasoning(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                Content::Reasoning { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Tool calls the model asked for.
    pub fn tool_uses(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        self.content
            .iter()
            .filter_map(|c| match c {
                Content::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_blocks_survive_a_round_trip_without_being_joined() {
        // One surveyed crate concatenates text blocks into a single string, which
        // destroys the boundaries a cache breakpoint sits on. This asserts the
        // boundaries survive.
        let message = Message {
            role: Role::User,
            content: vec![
                Content::cached_text("a long stable prefix"),
                Content::text("the volatile tail"),
            ],
        };
        let json = serde_json::to_string(&message).expect("serialize");
        let back: Message = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, message);
        assert_eq!(back.content.len(), 2);
        assert!(matches!(
            back.content[0],
            Content::Text {
                cache_breakpoint: true,
                ..
            }
        ));
    }

    #[test]
    fn a_request_reports_whether_it_wants_cache_control() {
        let plain = Request::new("m", "hello", 100);
        assert!(!plain.wants_cache_control());

        let mut cached = Request::new("m", "hello", 100);
        cached.system = vec![Content::cached_text("system prefix")];
        assert!(cached.wants_cache_control());
    }

    #[test]
    fn a_tool_result_carries_failure_in_both_places() {
        // One provider ignores the error flag, so failure must also be legible in
        // the content, or a failed tool call is indistinguishable from a
        // successful one.
        let result = Content::ToolResult {
            tool_use_id: "t1".into(),
            content: "ERROR: file not found".into(),
            is_error: true,
        };
        match &result {
            Content::ToolResult {
                content, is_error, ..
            } => {
                assert!(*is_error);
                assert!(content.contains("ERROR"));
            }
            _ => unreachable!(),
        }
    }
}
