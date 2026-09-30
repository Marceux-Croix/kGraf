//! Vertex AI builders: Gemini and Anthropic Claude behind OAuth.
//!
//! Both are built from [`VertexSettings`] and a list of models. Neither makes a
//! network call at build time.
//!
//! Claude on Vertex takes the Anthropic Messages body at
//! `.../publishers/anthropic/models/{model}:rawPredict` and `:streamRawPredict`,
//! with `anthropic_version: vertex-2023-10-16` in the body and the model in the
//! URL. It is served by the Anthropic adapter through its per-model transport, so
//! the streaming parser, tool handling and usage rules are the same code the
//! direct route runs. Reference:
//! <https://cloud.google.com/vertex-ai/generative-ai/docs/partner-models/claude/use-claude>

use std::sync::Arc;
use std::time::Duration;

use kgraf_core::Protocol;
use kgraf_provider::{ProviderConfig, ProviderError};
use kgraf_provider_anthropic::{AnthropicAdapter, BearerSource, ModelInUrlTransport};

use crate::auth::{GoogleAuth, VertexSettings};
use crate::gemini::{GeminiAdapter, VERTEX_GEMINI_ADAPTER};

/// The adapter name for Claude on Vertex AI.
pub const VERTEX_ANTHROPIC_ADAPTER: &str = "vertex-raw-predict";

/// The body version Vertex requires for Claude.
pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

/// The default request timeout for a hosted Vertex route.
const TIMEOUT: Duration = Duration::from_secs(600);

fn auth(
    adapter: &'static str,
    settings: &VertexSettings,
) -> Result<Arc<dyn BearerSource>, ProviderError> {
    Ok(Arc::new(GoogleAuth::new(
        adapter,
        settings.credentials(),
        TIMEOUT,
    )?))
}

/// The configuration for a Vertex route.
///
/// Built through the no-API-key constructor: authentication is a bearer token
/// per request, so there is no static credential to hold.
fn config(
    settings: &VertexSettings,
    publisher: &str,
    models: Vec<String>,
    protocol: Protocol,
) -> ProviderConfig {
    ProviderConfig::local(settings.publisher_root(publisher), models, protocol)
        .with_timeout(TIMEOUT)
}

/// The configuration for Gemini on Vertex AI.
pub fn vertex_gemini_config(settings: &VertexSettings, models: Vec<String>) -> ProviderConfig {
    config(settings, "google", models, Protocol::Inclusive)
}

/// The configuration for Claude on Vertex AI.
pub fn vertex_anthropic_config(settings: &VertexSettings, models: Vec<String>) -> ProviderConfig {
    config(settings, "anthropic", models, Protocol::Additive)
}

/// Gemini on Vertex AI.
pub fn vertex_gemini(
    settings: &VertexSettings,
    models: Vec<String>,
) -> Result<GeminiAdapter, ProviderError> {
    vertex_gemini_at(
        vertex_gemini_config(settings, models),
        auth(VERTEX_GEMINI_ADAPTER, settings)?,
    )
}

/// Gemini on Vertex AI with the configuration and token source given, for a
/// caller that overrides the host.
pub fn vertex_gemini_at(
    config: ProviderConfig,
    auth: Arc<dyn BearerSource>,
) -> Result<GeminiAdapter, ProviderError> {
    GeminiAdapter::vertex(config, auth)
}

/// Claude on Vertex AI.
pub fn vertex_anthropic(
    settings: &VertexSettings,
    models: Vec<String>,
) -> Result<AnthropicAdapter, ProviderError> {
    vertex_anthropic_at(
        vertex_anthropic_config(settings, models),
        auth(VERTEX_ANTHROPIC_ADAPTER, settings)?,
    )
}

/// Claude on Vertex AI with the configuration and token source given.
///
/// `config.base_url` is the publisher root, ending in `/publishers/anthropic`.
pub fn vertex_anthropic_at(
    config: ProviderConfig,
    auth: Arc<dyn BearerSource>,
) -> Result<AnthropicAdapter, ProviderError> {
    let transport = ModelInUrlTransport::new(
        VERTEX_ANTHROPIC_ADAPTER,
        format!("{}/models", config.base_url),
        "rawPredict",
        "streamRawPredict",
        VERTEX_ANTHROPIC_VERSION,
        auth,
    );
    Ok(AnthropicAdapter::new(config)?.with_transport(transport))
}
