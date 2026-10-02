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

/// An opaque token a provider attached to a block, kept with the history and
/// sent back verbatim.
///
/// A token is valid only for the model that made it, so it carries the
/// producer: a tag naming the adapter and model. An adapter sends a token only
/// when the producer matches its own tag. History written before the producer
/// was recorded holds a bare string; it reads back with no producer, which
/// matches nothing.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Signature {
    pub value: String,
    /// The adapter and model that made the token, in the adapter's own
    /// spelling. `None` for a token whose maker was not recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
}

impl Signature {
    /// A token made by `producer`.
    pub fn new(value: impl Into<String>, producer: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            producer: Some(producer.into()),
        }
    }

    /// Whether `producer` made this token.
    pub fn is_from(&self, producer: &str) -> bool {
        self.producer.as_deref() == Some(producer)
    }
}

impl From<String> for Signature {
    fn from(value: String) -> Self {
        Self {
            value,
            producer: None,
        }
    }
}

impl From<&str> for Signature {
    fn from(value: &str) -> Self {
        value.to_owned().into()
    }
}

impl<'de> serde::Deserialize<'de> for Signature {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Bare(String),
            Full {
                value: String,
                #[serde(default)]
                producer: Option<String>,
            },
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Bare(value) => Signature {
                value,
                producer: None,
            },
            Wire::Full { value, producer } => Signature { value, producer },
        })
    }
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
        /// The token the provider attached to this text part, kept with the
        /// history. Absent from history written before the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<Signature>,
    },
    /// A tool call the model asked for.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        /// An opaque token the provider attached to this call, kept with the
        /// history and sent back verbatim with the call. Absent from history
        /// written before the field existed, and from calls whose provider
        /// attaches none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<Signature>,
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

/// A tool call whose arguments did not parse as JSON keeps them as a string
/// (the adapters store what the model emitted). Sent back in history as it
/// stands, that string would be encoded a second time on a wire that takes
/// arguments as JSON text, and rejected on a wire that takes an object.
/// These helpers give each wire the right shape.
///
/// The arguments as the JSON text a chat-style wire carries: an unparsed string
/// goes back verbatim, anything else is serialized.
///
/// A call with no arguments goes back as `{}`: an absent input, or an empty
/// argument string (how a vendor spells "no arguments"), is not valid JSON text
/// as it stands.
pub fn tool_arguments_text(input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::Null => "{}".to_owned(),
        serde_json::Value::String(raw) if raw.trim().is_empty() => "{}".to_owned(),
        serde_json::Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

/// The arguments as the JSON object a block-style wire requires: an unparsed
/// string is wrapped so its text survives and the input is still an object. A
/// call with no arguments (an absent input, or an empty string) is the empty
/// object, not a wrapped `null` or `""`.
pub fn tool_arguments_object(input: &serde_json::Value) -> serde_json::Value {
    match input {
        serde_json::Value::Object(_) => input.clone(),
        serde_json::Value::Null => serde_json::json!({}),
        serde_json::Value::String(raw) if raw.trim().is_empty() => serde_json::json!({}),
        serde_json::Value::String(raw) => serde_json::json!({ "unparsed_arguments": raw }),
        other => serde_json::json!({ "unparsed_arguments": other.to_string() }),
    }
}

impl Content {
    /// A plain text block with no cache breakpoint.
    pub fn text(text: impl Into<String>) -> Self {
        Content::Text {
            text: text.into(),
            cache_breakpoint: false,
            signature: None,
        }
    }

    /// A text block ending in a cache breakpoint.
    pub fn cached_text(text: impl Into<String>) -> Self {
        Content::Text {
            text: text.into(),
            cache_breakpoint: true,
            signature: None,
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
    /// The provider's own finish string (`stop`, `tool_calls`, `end_turn`,
    /// `STOP`), exactly as the wire carried it, where the adapter reads one.
    /// `stop_reason` is the normalized form; this keeps what it was mapped
    /// from, so an `Other` is explainable. `None` where the protocol sends no
    /// such string.
    pub finish_reason: Option<String>,
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
                Content::ToolUse {
                    id, name, input, ..
                } => Some((id.as_str(), name.as_str(), input)),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_with_no_arguments_replays_as_an_empty_object_on_both_wires() {
        for none in [
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!("  "),
        ] {
            assert_eq!(tool_arguments_text(&none), "{}", "{none:?}");
            assert_eq!(tool_arguments_object(&none), serde_json::json!({}));
        }
        // Text that is not JSON is still preserved, not replaced.
        assert_eq!(
            tool_arguments_object(&serde_json::json!("{not json")),
            serde_json::json!({"unparsed_arguments": "{not json"})
        );
    }

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
    fn a_signature_reads_from_a_bare_string_and_from_the_tagged_form() {
        let old: Content = serde_json::from_value(serde_json::json!({
            "type": "tool_use", "id": "1", "name": "n", "input": {}, "signature": "c2ln"
        }))
        .expect("history written before the producer was recorded");
        let Content::ToolUse { signature, .. } = &old else {
            unreachable!()
        };
        let signature = signature.as_ref().expect("kept");
        assert_eq!(signature.value, "c2ln");
        assert!(
            !signature.is_from("any"),
            "an unrecorded maker matches nothing"
        );

        let tagged = Signature::new("c2ln", "gemini-generate-content:gemini-3-pro");
        let back: Signature =
            serde_json::from_str(&serde_json::to_string(&tagged).expect("ser")).expect("de");
        assert_eq!(back, tagged);
        assert!(back.is_from("gemini-generate-content:gemini-3-pro"));
    }

    #[test]
    fn a_text_block_without_a_signature_field_still_reads() {
        let block: Content =
            serde_json::from_str(r#"{"type":"text","text":"hi"}"#).expect("old history");
        assert_eq!(block, Content::text("hi"));
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
