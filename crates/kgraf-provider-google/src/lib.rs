//! Google adapters.
//!
//! Three routes, two wire protocols:
//!
//! - [`GeminiAdapter::api_key`]: the Gemini Developer API, `generateContent`
//!   and `streamGenerateContent`, with an API key.
//! - [`vertex_gemini`]: the same body on Vertex AI, with an OAuth bearer token.
//! - [`vertex_anthropic`]: Claude on Vertex AI. The body is the Anthropic
//!   Messages body, so the Anthropic adapter serves it through its per-model
//!   transport; this crate supplies the URL, the version string and the token.
//!
//! **Token handling is reused, not written.** OAuth for Vertex is `gcp_auth`
//! (MIT): Application Default Credentials, service-account key files, the
//! metadata server, token caching and refresh. Searched before writing any:
//! crates.io for `gcp_auth`, `google-cloud-auth` (official, Apache-2.0; native
//! TLS and a second `reqwest` in its default features) and `yup-oauth2`
//! (device-flow first); the reasoning is in the workspace `Cargo.toml`. Wire
//! shapes are hand-rolled on `reqwest` and `serde` like the other adapters,
//! because the usage and thinking-token arithmetic is the cost path.
//!
//! **Unverified.** No route here has exchanged a request with the live service.
//! The tests run against recorded bodies written from Google's published
//! references, each cited in its fixture.

mod auth;
mod gemini;
mod vertex;

pub use auth::{
    CLOUD_PLATFORM_SCOPE, CREDENTIALS_VARIABLE, CredentialSource, GoogleAuth, PROJECT_VARIABLE,
    REGION_VARIABLE, SettingError, VertexSettings, credential_file_secrets,
};
pub use gemini::{
    GEMINI_ADAPTER, GeminiAdapter, ThinkingStyle, VERTEX_GEMINI_ADAPTER, vertex_schema,
};
pub use vertex::{
    VERTEX_ANTHROPIC_ADAPTER, VERTEX_ANTHROPIC_VERSION, vertex_anthropic, vertex_anthropic_at,
    vertex_anthropic_config, vertex_gemini, vertex_gemini_at, vertex_gemini_config,
};
