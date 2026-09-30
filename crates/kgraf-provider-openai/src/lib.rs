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
