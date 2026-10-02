//! The OpenAI-family protocol adapters.
//!
//! "OpenAI-compatible" is not one protocol. It is two, and conflating them is a
//! mistake this crate exists to make impossible:
//!
//! - [`responses`] speaks the **Responses** API: `POST /responses`, with `input`
//!   and `max_output_tokens`, and usage in `input_tokens` /
//!   `input_tokens_details.cached_tokens`. It is OpenAI-proprietary, and decision
//!   0006 targets it for OpenAI's own models because on the current flagship the
//!   chat surface gives tools or reasoning but never both.
//! - [`chat`] speaks **Chat Completions**: `POST /chat/completions`, with `messages`
//!   and `max_tokens`, and usage in `prompt_tokens` / `completion_tokens`. This is
//!   the surface the entire third-party compatible ecosystem actually implements,
//!   z.ai, DeepSeek, MiniMax, Together, Groq, vLLM, llama.cpp. None of them serve
//!   `/responses`.
//!
//! The distinction is load-bearing rather than cosmetic. Decision 0006's instruction
//! to prefer Responses over Chat Completions is an argument about **OpenAI's own
//! models**, and reading it as an instruction about "the OpenAI adapter" leaves every
//! compatible vendor unreachable. Decision 0041 records the correction.
//!
//! [`embeddings`] adds the embedding and rerank kinds (decision 0089) on the same
//! endpoints: `POST /embeddings` and the common `POST /rerank` shape.
//!
//! Both adapters normalize to the same six-field usage form under the **inclusive**
//! convention, and both assert that the response model equals the requested one.
//! What they do not share is the wire shape or the usage field names, which is
//! exactly why they are separate types rather than one type with a flag.

pub mod azure;
pub mod chat;
pub mod embeddings;
pub mod responses;

#[cfg(test)]
mod azure_tests;

pub use azure::{AzureSettings, AzureSettingsError, AzureTransport};
pub use chat::{ChatCompletionsAdapter, ReasoningDialect, StreamedUsageLocation, UsageDialect};
pub use embeddings::{
    EMBEDDINGS_ADAPTER, EchoRule, OpenAiEmbeddings, OpenAiRerank, RERANK_ADAPTER,
};
pub use responses::OpenAiAdapter;

/// Read a non-success response's body for the error message, bounded by
/// `timeout`.
///
/// Only ever used to enrich an error already being raised, so a body that
/// never finishes reading falls back to an empty string rather than hanging
/// the failure path: `connect_timeout` alone (see each adapter's `new`)
/// leaves reading an error body with no bound at all, which is the same
/// stream-hostile-whole-request-timeout defect this project fixed for a
/// healthy response, just on the failure path instead.
pub(crate) async fn bounded_error_body(
    timeout: std::time::Duration,
    response: reqwest::Response,
) -> String {
    match tokio::time::timeout(timeout, response.text()).await {
        Ok(Ok(text)) => text,
        Ok(Err(_)) | Err(_) => String::new(),
    }
}

/// Substrings (lowercase) a server's 5xx body carries when the failure is that
/// it could not parse the model's own tool-call output. Ollama answers HTTP 500
/// `XML syntax error on line N: ...` for a Qwen-style XML call and `error
/// parsing tool call: raw='...'` for a JSON call; llama.cpp says `Failed to
/// parse tool call`.
const MALFORMED_OUTPUT_MARKERS: [&str; 3] = [
    "xml syntax error",
    "error parsing tool call",
    "failed to parse tool call",
];

/// Whether a 5xx body says the server failed to parse the model's own output.
fn is_malformed_output_body(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    MALFORMED_OUTPUT_MARKERS.iter().any(|m| lower.contains(m))
}

/// The error for a non-success status. A 5xx whose body names a parse failure of
/// the model's own output is [`ProviderError::ModelOutputMalformed`]; every
/// other status is a plain [`ProviderError::Http`].
pub(crate) fn http_error(
    adapter: &'static str,
    status: u16,
    body: String,
) -> kgraf_provider::ProviderError {
    if (500..600).contains(&status) && is_malformed_output_body(&body) {
        kgraf_provider::ProviderError::ModelOutputMalformed {
            adapter,
            status,
            body,
        }
    } else {
        kgraf_provider::ProviderError::Http {
            adapter,
            status,
            body,
        }
    }
}

/// Classify a failure to send.
///
/// Only a connection that never opened is known not to have reached the provider.
/// A timeout or a reset after the request was written may have been billed in full.
pub(crate) fn send_error(
    adapter: &'static str,
    e: reqwest::Error,
) -> kgraf_provider::ProviderError {
    if e.is_connect() || e.is_builder() {
        kgraf_provider::ProviderError::Unsent {
            adapter,
            reason: e.to_string(),
        }
    } else {
        kgraf_provider::ProviderError::Transport {
            adapter,
            reason: e.to_string(),
        }
    }
}

#[cfg(test)]
mod http_error_tests {
    use super::http_error;
    use kgraf_provider::ProviderError;

    #[test]
    fn recognizes_the_server_bodies_that_name_a_model_output_parse_failure() {
        for body in [
            r#"{"error":{"message":"XML syntax error on line 8: element <function> closed by </parameter>"}}"#,
            r#"{"error":{"message":"error parsing tool call: raw='{\"name\":', err=unexpected end of JSON input"}}"#,
            "Failed to parse tool call",
        ] {
            let e = http_error("a", 500, body.to_owned());
            assert!(e.is_malformed_model_output(), "{body}");
        }
    }

    #[test]
    fn a_5xx_is_malformed_output_only_with_a_parse_body() {
        assert!(matches!(
            http_error("a", 503, "overloaded".to_owned()),
            ProviderError::Http { .. }
        ));
        assert!(matches!(
            http_error("a", 429, "XML syntax error".to_owned()),
            ProviderError::Http { .. }
        ));
        assert!(matches!(
            http_error("a", 500, String::new()),
            ProviderError::Http { .. }
        ));
    }
}
