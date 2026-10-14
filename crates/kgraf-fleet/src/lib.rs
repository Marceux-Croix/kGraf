//! The vendor catalog.
//!
//! The adapter crates know a **protocol** and nothing else, that is decision 0006's
//! rule that provider types never escape their adapter, and it is why accounting keys
//! on the protocol rather than on a vendor name. But something has to know that
//! DeepSeek reports its cache split as hit-and-miss while z.ai reports it as a details
//! object, and that neither of them serves the surface OpenAI's own models want. That
//! knowledge is here, as **data**, in one reviewable table.
//!
//! Three properties are deliberate.
//!
//! **Every entry carries a [`Confidence`].** The project's endpoint research found
//! three confirmed cases of a provider accepting a field and silently ignoring it, all
//! returning HTTP 200, and several vendors whose usage shape is formally unverified. An
//! entry that says "unverified" is doing its job; a catalog that presented every row
//! with equal authority would be the silent-degradation failure the fail-loud rule exists to stop.
//!
//! **No entry lists model names.** Model identifiers change faster than a release, and
//! a stale list is worse than no list: at best it trips the model-echo assertion, at
//! worst it routes to a cheap tier that answers plausibly. The caller supplies the
//! models it means to use, and the adapter refuses anything outside that set before a
//! request leaves.
//!
//! **Cache control defaults to not honored**, per decision 0006, and is set true only
//! where the research verified it. Emitting breakpoints at a provider that drops them
//! costs money silently and gains nothing.
//!
//! # Model kinds (decision 0089)
//!
//! A route serves one or more model kinds (`kgraf_core::ModelKind`). Two tables say
//! which and how, and neither matches on a kind or a wire:
//!
//! - [`KIND_CLAIMS`]: per catalog route, the kinds it serves, from the vendor's own
//!   documentation (cited in the row), and the dialect its embedding and rerank
//!   endpoints speak. **A route with no row serves generative work only**, so a new
//!   generative route needs no row at all.
//! - [`KIND_BUILDERS`]: per kind, how a route of it is built.
//!
//! [`build_kind`] is the one entry point for every kind; [`build`] stays the
//! generative one and refuses a route that serves no generative model.
//!
//! The routes claimed today: **embedding** on `openai` (verified live), `google`,
//! `ollama` (its native endpoint, which refuses rather than truncates an over-long
//! text), `llama-cpp`, `tei`, `azure-openai` and `azure-openai-responses` (the
//! Azure embeddings transport, decision 0092); **rerank** on `together`,
//! `llama-cpp` and `tei`; **typed-decision** on `systemone`, through
//! `kgraf_decide`'s `/v1/systemone` client. Each with the route's own
//! credential. Each claim row carries its [`Confidence`]: only `openai`
//! embeddings were exchanged with the live service.

pub mod reasoning;
pub mod tool_choice;

pub use reasoning::{
    LevelOutcome, REASONING_MEASUREMENTS, ReasoningMeasurement, reasoning_measurement,
};
pub use tool_choice::{
    TOOL_CHOICE_MEASUREMENTS, ToolChoiceMeasurement, ToolChoiceOutcome, tool_choice_measurement,
};

use kgraf_core::session::ReasoningVisibility;
use kgraf_core::{KindError, ModelKind, Protocol};
use kgraf_provider::EmbedLimits;
use kgraf_provider::{Embedder, ModelRoute, Provider, ProviderConfig, ProviderError, Reranker};
use kgraf_provider_anthropic::AnthropicAdapter;
use kgraf_provider_bedrock::BedrockAdapter;
use kgraf_provider_google::{
    GEMINI_ADAPTER, GeminiAdapter, VERTEX_ANTHROPIC_ADAPTER, VERTEX_GEMINI_ADAPTER, VertexSettings,
};
use kgraf_provider_openai::azure::{
    CHAT_ADAPTER as AZURE_CHAT_ADAPTER, RESPONSES_ADAPTER as AZURE_RESPONSES_ADAPTER,
};
use kgraf_provider_openai::embeddings::{EchoRule, EmbedDialect, RerankDialect};
use kgraf_provider_openai::{
    AzureSettings, AzureSettingsError, AzureTransport, ChatCompletionsAdapter, OpenAiAdapter,
    OpenAiEmbeddings, OpenAiRerank, ReasoningDialect, StreamedUsageLocation, UsageDialect,
};
use std::sync::Arc;

/// Which wire protocol an endpoint speaks.
///
/// "OpenAI-compatible" is two of these, not one, and conflating them is what left the
/// entire third-party ecosystem unreachable until decision 0041.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    /// The Anthropic Messages shape: `POST /messages`, additive usage.
    AnthropicMessages,
    /// OpenAI's proprietary Responses surface: `POST /responses`.
    OpenAiResponses,
    /// Chat Completions: `POST /chat/completions`. What the compatible ecosystem
    /// actually serves.
    OpenAiChat(UsageDialect),
    /// AWS Bedrock's Converse and ConverseStream operations, SigV4-signed, additive
    /// usage. Credentials are the standard AWS ones, not a single API key.
    BedrockConverse,
    /// Gemini `generateContent` on the Gemini Developer API, with an API key.
    GeminiGenerateContent,
    /// Gemini `generateContent` on Vertex AI, with an OAuth bearer token.
    VertexGemini,
    /// The Anthropic Messages body on Vertex AI (`rawPredict`), with an OAuth
    /// bearer token. Additive usage, like the direct Anthropic wire.
    VertexAnthropic,
    /// The Chat Completions body on Azure OpenAI: per-deployment URL, `api-key`
    /// header, and a configured deployment-to-model mapping.
    AzureOpenAiChat,
    /// The Responses body on Azure OpenAI, with the same address and credential
    /// rules as [`Wire::AzureOpenAiChat`].
    AzureOpenAiResponses,
    /// Typed decisions: `POST /v1/systemone` (decision 0042). Serves the
    /// typed-decision kind only, never generative work.
    SystemOne,
}

impl Wire {
    /// The wire's stable name, for display and configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Wire::AnthropicMessages => "anthropic-messages",
            Wire::OpenAiResponses => "openai-responses",
            Wire::OpenAiChat(_) => "openai-chat-completions",
            Wire::BedrockConverse => "bedrock-converse",
            Wire::GeminiGenerateContent => GEMINI_ADAPTER,
            Wire::VertexGemini => VERTEX_GEMINI_ADAPTER,
            Wire::VertexAnthropic => VERTEX_ANTHROPIC_ADAPTER,
            Wire::AzureOpenAiChat => AZURE_CHAT_ADAPTER,
            Wire::AzureOpenAiResponses => AZURE_RESPONSES_ADAPTER,
            Wire::SystemOne => "systemone",
        }
    }

    /// Whether the route is Azure OpenAI. Such a route's credential is the key
    /// together with the resource, API version and deployment mapping, resolved
    /// from the environment (see [`credential_from`]).
    fn is_azure(self) -> bool {
        matches!(self, Wire::AzureOpenAiChat | Wire::AzureOpenAiResponses)
    }

    /// Whether the route authenticates with Google OAuth instead of an API key.
    /// Such a route's credential is the Vertex settings, resolved from the
    /// environment (see [`credential_from`]).
    fn is_vertex(self) -> bool {
        matches!(self, Wire::VertexGemini | Wire::VertexAnthropic)
    }
}

/// How much is actually known about an entry.
///
/// This is not decoration. It is the difference between a route the project probed and
/// one it read about in a third party's integration guide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Probed or documented first-party, with the usage shape confirmed.
    Verified,
    /// The endpoint and protocol are documented, but the usage or cache behavior was
    /// not confirmed. Usable; the raw usage blob is the safety net.
    Documented,
    /// Reachable in principle and not verified in any respect. Expect to check the
    /// recorded raw usage blob on the first live call.
    Unverified,
}

impl Confidence {
    /// The confidence's stable name.
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Verified => "verified",
            Confidence::Documented => "documented",
            Confidence::Unverified => "unverified",
        }
    }
}

/// One route to one vendor.
///
/// A vendor may have several, and where it does they are separate entries, because the
/// choice between them is a real decision with different accounting consequences.
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    /// Stable selector, as a user types it.
    pub id: &'static str,
    /// Human-readable vendor name.
    pub vendor: &'static str,
    /// Default endpoint root. Overridable, since a self-hosted or proxied deployment
    /// moves it.
    pub base_url: &'static str,
    pub wire: Wire,
    pub protocol: Protocol,
    /// Whether this route is verified to honor explicit cache breakpoints.
    pub honors_cache_control: bool,
    /// Whether reasoning tokens are reported separately from text output.
    pub reports_reasoning_tokens: bool,
    /// Whether the route rejects a sampling temperature on a reasoning request.
    /// Where true, such a request is refused before it leaves (fail loud) rather than
    /// having the temperature stripped.
    pub refuses_temperature_with_reasoning: bool,
    /// Whether the endpoint requires a credential at all.
    pub requires_credential: bool,
    /// Another route whose credential this one takes, where one account's key works
    /// on both. Such routes read one environment variable, so the user sets a key
    /// once. A route whose key is a different kind stays separate: a platform key and
    /// a Kimi Code subscription key are refused on each other's hosts.
    pub shares_credential_with: Option<&'static str>,
    pub confidence: Confidence,
    /// Where a Chat Completions route reports usage inside a **streamed**
    /// response, and how confident the catalog is about it, or `None` off
    /// the Chat Completions wire (Anthropic Messages and Responses do not
    /// use this fact; each has its own streaming usage shape handled inside
    /// its own adapter).
    ///
    /// Independent of the entry's overall [`Confidence`]: a route can be
    /// `Verified` end to end for a non-streaming call and still have never
    /// exchanged a single streamed request, which is exactly the gap an
    /// Testing found this project's evidence did not close. Defaults to
    /// [`Confidence::Unverified`] everywhere it has not been probed live.
    /// [`StreamedUsageLocation::None`] must never build an adapter willing
    /// to stream (checked in `build`, and enforced again inside the adapter
    /// itself per the fail-loud rule): paying for a request whose usage can never be
    /// attributed, then discovering that only after the connection closed,
    /// is worse than refusing before it leaves.
    pub streamed_usage: Option<(StreamedUsageLocation, Confidence)>,
    /// What reasoning this route returns, declared as a model limit rather than
    /// a bug (decision 0067). This is the
    /// catalog's stated intent for the route, checked against what a session
    /// actually observes (`session_panel::observed_visibility`); a mismatch is
    /// shown as a disagreement, never resolved silently (the fail-loud rule, the real-speaker rule). An
    /// [`Confidence::Unverified`] entry always declares
    /// [`ReasoningVisibility::Unknown`] (checked in `check_catalog`): a route
    /// nobody has run is not the place to assert what it returns.
    pub reasoning_visibility: ReasoningVisibility,
    /// Whether the route's completion count includes the reasoning share
    /// ([`ReasoningDialect::Inclusive`], OpenAI's own shape and the default) or
    /// reports it as a separate count ([`ReasoningDialect::Additive`]). Read on
    /// the OpenAI-family wires; the Anthropic wire has its own convention.
    pub reasoning_dialect: ReasoningDialect,
    /// What a reader needs to know before choosing this route. Kept short and factual.
    pub note: &'static str,
}

/// Errors selecting or building a fleet member.
#[derive(Debug, thiserror::Error)]
pub enum FleetError {
    /// A route declared to take `prompt_cache_key` is not on a Chat
    /// Completions wire.
    #[error(
        "`{id}` cannot be declared to take `prompt_cache_key`: its wire is {wire}, and only \
         Chat Completions routes are declared (Responses routes always send it, other wires \
         have no such field)"
    )]
    PromptCacheKeyWire { id: String, wire: &'static str },
    /// No catalog entry with that id.
    #[error("unknown provider `{id}`; known: {known}")]
    Unknown { id: String, known: String },
    /// A route that needs a credential has none configured.
    #[error(
        "no credential for `{id}`: set {variable} in your own shell (never paste a key into a chat or a file you commit)"
    )]
    CredentialMissing { id: String, variable: String },
    /// A route's credentials exist in some form but cannot be used, or are
    /// incomplete. The reason says what to set or change.
    #[error("no usable credential for `{id}`: {reason}")]
    CredentialUnusable { id: String, reason: String },
    /// The adapter refused the configuration.
    #[error("cannot configure `{id}`")]
    Provider {
        id: String,
        #[source]
        source: ProviderError,
    },
    /// The route does not serve the model kind asked of it (decision 0089).
    #[error(transparent)]
    Kind(#[from] KindError),
    /// A local route's endpoint override is not an http or https URL.
    #[error("{variable} for `{id}` must be an http:// or https:// URL, not `{value}`")]
    BadBaseUrl {
        id: String,
        variable: String,
        value: String,
    },
    /// The typed-decision client could not be built.
    #[error("cannot configure `{id}`")]
    Decider {
        id: String,
        #[source]
        source: kgraf_decide::DecideError,
    },
}

/// The catalog.
///
/// Ordered roughly by how much is known about each route, so the most load-bearing
/// entries read first.
pub const CATALOG: &[Entry] = &[
    Entry {
        id: "anthropic",
        vendor: "Anthropic",
        base_url: "https://api.anthropic.com/v1",
        wire: Wire::AnthropicMessages,
        protocol: Protocol::Additive,
        honors_cache_control: true,
        reports_reasoning_tokens: false,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Verified,
        streamed_usage: None,
        reasoning_visibility: ReasoningVisibility::None,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "The additive reference. Input excludes cache reads and writes, so the \
               true input is the sum of all three. Reasoning is billed inside output \
               and not reported separately. Verified live against \
               api.anthropic.com on 2026-09-28: a plain call, a streaming call, a \
               tool round trip, prompt caching (cold write, then warm read), the \
               rate-limit headers and a steer note after tool results all passed \
               (6 of 6).",
    },
    Entry {
        id: "openai",
        vendor: "OpenAI",
        base_url: "https://api.openai.com/v1",
        wire: Wire::OpenAiResponses,
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: true,
        refuses_temperature_with_reasoning: true,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Verified,
        streamed_usage: None,
        reasoning_visibility: ReasoningVisibility::Summary,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Responses, not Chat Completions: on the current flagship the chat \
               surface requires reasoning effort set to none for function calling, so \
               it gives tools or reasoning but never both (decision 0006). Caching is \
               automatic, with no breakpoint field. Current reasoning models accept a \
               temperature only at effort none, so a request setting both is refused \
               before it leaves (measured by big-AGI, not yet by this project). The \
               reasoning summaries are requested (decision 0067): the adapter now sends \
               `reasoning.summary: \"auto\"` whenever an effort is set \
               (`kgraf-provider-openai/src/responses.rs`), verified live on main \
               (1ad5550) -- the field was accepted and a summary came back -- so this \
               route declares `summary`, not `none`.",
    },
    Entry {
        id: "kimi-anthropic",
        vendor: "Moonshot (Kimi)",
        // The Anthropic protocol's messages path is `<root>/v1/messages`, and this
        // adapter appends `/messages`, so the root carries `/v1` as the `anthropic`
        // route's does. To be confirmed by the first live call on this route.
        base_url: "https://api.moonshot.ai/anthropic/v1",
        wire: Wire::AnthropicMessages,
        protocol: Protocol::Additive,
        honors_cache_control: true,
        reports_reasoning_tokens: true,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: Some("kimi"),
        confidence: Confidence::Verified,
        streamed_usage: None,
        reasoning_visibility: ReasoningVisibility::None,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "The same API key against this route and Kimi's native route gives \
               OPPOSITE usage conventions: additive here, inclusive there. This is the \
               case decision 0006 cites for keying accounting on protocol rather than \
               vendor. Cache control is top-level only. Model enum is narrowed. First \
               live call 2026-09-26 (kimi-k3): the /v1 root is correct, the model \
               echoed, and the usage object is a hybrid of both protocols' fields, \
               with reasoning in output_tokens_details.thinking_tokens and cache \
               writes split into 5-minute and 1-hour buckets.",
    },
    Entry {
        id: "kimi",
        vendor: "Moonshot (Kimi)",
        base_url: "https://api.moonshot.ai/v1",
        wire: Wire::OpenAiChat(UsageDialect::Details),
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: true,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Verified,
        // Verified against a recorded live stream (verified against a real server), not the earlier
        // recollection: `tests/fixtures/2026-09-26-kimi-chat-stream.sse`
        // carries usage inside `choices[0].usage` on the finish chunk
        // (finish_reason "length"). Kimi also repeats the identical usage a
        // second time, top-level, on a trailing chunk with empty `choices`;
        // the adapter dedupes that repeat rather than double-recording it.
        streamed_usage: Some((StreamedUsageLocation::InChoice, Confidence::Verified)),
        reasoning_visibility: ReasoningVisibility::Full,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Kimi's native route, for platform keys. Inclusive, against its own \
               Anthropic route's additive. Pick one per worker and let the catalog \
               carry the difference. Kimi Code subscription keys (sk-kimi-) are \
               refused here with 401; they use the kimi-code route. Documentation \
               moved to platform.kimi.ai. First live call 2026-09-26 (kimi-k3): \
               model echoed, reasoning tokens reported in completion details, and \
               cache writes reported in prompt details. A live run on 2026-09-28 \
               showed a cache read of 256 tokens on a second call. A live streamed call on 2026-09-26 confirmed usage arrives inside \
               `choices[0].usage`, repeated verbatim top-level on the closing chunk.",
    },
    Entry {
        id: "kimi-code",
        vendor: "Moonshot (Kimi Code subscription)",
        base_url: "https://api.kimi.com/coding/v1",
        wire: Wire::OpenAiChat(UsageDialect::Details),
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: false,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Unverified,
        streamed_usage: Some((StreamedUsageLocation::TopLevelChunk, Confidence::Unverified)),
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "The Kimi Code subscription route, for sk-kimi- keys only; a platform \
               key does not work here, and a subscription key returns 401 on the \
               platform route. It serves its own small model catalog, separate from \
               the platform's. Billed by subscription, not per token, so cost derives \
               only from a price table the user sets, typically zero. Reported by \
               big-AGI as caching with cached_tokens in usage; unverified here.",
    },
    Entry {
        id: "deepseek",
        vendor: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        wire: Wire::OpenAiChat(UsageDialect::HitAndMiss),
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: true,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Documented,
        streamed_usage: Some((StreamedUsageLocation::TopLevelChunk, Confidence::Unverified)),
        reasoning_visibility: ReasoningVisibility::Full,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "The hit-and-miss usage dialect decision 0006 requires as a third usage \
               adapter. WARNING, confirmed from its own documentation: an unsupported \
               model name is SILENTLY REMAPPED to a cheap model and returns HTTP 200. \
               The model-echo assertion is what catches that. Its tool-result error \
               flag is also ignored, so failure is stated in the content too.",
    },
    Entry {
        id: "zai",
        vendor: "z.ai (GLM)",
        base_url: "https://api.z.ai/api/paas/v4",
        wire: Wire::OpenAiChat(UsageDialect::Details),
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: false,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Unverified,
        streamed_usage: Some((StreamedUsageLocation::TopLevelChunk, Confidence::Unverified)),
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Reached over Chat Completions, which is the surface z.ai's models are \
               compatible with. An Anthropic-shaped route also exists at \
               api.z.ai/api/anthropic but is documented only inside a Claude Code \
               integration guide, absent from the API reference, with every \
               Anthropic-specific field unverified; prefer this route. Check the \
               recorded raw usage blob on the first live call.",
    },
    Entry {
        id: "qwen",
        vendor: "Alibaba (Qwen)",
        base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        wire: Wire::OpenAiChat(UsageDialect::Details),
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: false,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Unverified,
        streamed_usage: Some((StreamedUsageLocation::TopLevelChunk, Confidence::Unverified)),
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "DashScope's OpenAI-compatible mode. The international and mainland \
               endpoints differ; override base_url for the mainland one. Usage shape \
               unverified.",
    },
    Entry {
        id: "grok",
        vendor: "xAI (Grok)",
        base_url: "https://api.x.ai/v1",
        wire: Wire::OpenAiResponses,
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: true,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Documented,
        streamed_usage: None,
        reasoning_visibility: ReasoningVisibility::None,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Responses, deliberately. xAI's Anthropic-shaped route is FULLY \
               DEPRECATED and has no cache_control field anywhere in its schema, so \
               breakpoints there are dropped without comment (decision 0006). If this \
               route rejects /responses, switch to the chat entry below.",
    },
    Entry {
        id: "grok-chat",
        vendor: "xAI (Grok)",
        base_url: "https://api.x.ai/v1",
        wire: Wire::OpenAiChat(UsageDialect::Details),
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: true,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Unverified,
        streamed_usage: Some((StreamedUsageLocation::TopLevelChunk, Confidence::Unverified)),
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Chat Completions fallback for xAI, since which surfaces it serves has \
               moved more than once.",
    },
    Entry {
        id: "google",
        vendor: "Google (Gemini)",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        wire: Wire::OpenAiChat(UsageDialect::Details),
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: false,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Documented,
        streamed_usage: Some((StreamedUsageLocation::TopLevelChunk, Confidence::Unverified)),
        reasoning_visibility: ReasoningVisibility::None,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Gemini's OpenAI-compatibility layer. WARNING, confirmed: it IGNORES \
               unsupported parameters rather than rejecting them, so a configuration \
               that looks applied can be inert. Treat any capability here as not \
               honored until a live call proves otherwise.",
    },
    Entry {
        id: "bedrock",
        vendor: "AWS Bedrock (Converse)",
        base_url: "https://bedrock-runtime.{region}.amazonaws.com",
        wire: Wire::BedrockConverse,
        protocol: Protocol::Additive,
        honors_cache_control: false,
        reports_reasoning_tokens: false,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Unverified,
        streamed_usage: None,
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Bedrock's model-independent Converse and ConverseStream operations, \
               signed with SigV4. Credentials are the standard AWS ones, read on every \
               call: a Bedrock API key in AWS_BEARER_TOKEN_BEDROCK (sent as a bearer \
               token, unsigned), else AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY (and \
               AWS_SESSION_TOKEN), or a static-key profile named by AWS_PROFILE; the \
               region is AWS_REGION, \
               AWS_DEFAULT_REGION or the profile's. A profile that needs SSO, role \
               assumption or a credential process is refused: export temporary keys \
               instead. Input excludes cache reads and writes (additive). The response \
               names no model, so the model is checked only against the configured set. \
               Cache points are off because the service accepts them for some models \
               only. Never run against the service: recorded-shape tests only.",
    },
    Entry {
        id: "meta",
        vendor: "Meta (Meta Model API)",
        base_url: "https://api.meta.ai/v1",
        wire: Wire::OpenAiResponses,
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: true,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Unverified,
        streamed_usage: None,
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Meta's own first-party API for its Muse models, over the Responses \
               dialect. Reported by big-AGI (2026-09-02), unverified here: unknown \
               top-level parameters are rejected, tool_choice accepts only auto, and \
               store defaults to TRUE upstream, which this adapter always overrides. \
               WARNING: a 'contributor' pricing tier lets Meta train on prompts and \
               completions; do not configure those model ids for private code.",
    },
    Entry {
        id: "together",
        vendor: "Together (Llama hosting)",
        base_url: "https://api.together.xyz/v1",
        wire: Wire::OpenAiChat(UsageDialect::Details),
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: false,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Unverified,
        streamed_usage: Some((StreamedUsageLocation::TopLevelChunk, Confidence::Unverified)),
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "One route to Meta's open-weight Llama models; Meta's own API serves \
               its Muse models instead (the meta entry). \
               Groq, Fireworks and any other Chat Completions host work the same way; \
               copy this entry with a different base_url.",
    },
    Entry {
        id: "ollama",
        vendor: "Local (Ollama)",
        base_url: "http://localhost:11434/v1",
        wire: Wire::OpenAiChat(UsageDialect::Details),
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: false,
        refuses_temperature_with_reasoning: false,
        requires_credential: false,
        shares_credential_with: None,
        confidence: Confidence::Verified,
        streamed_usage: Some((StreamedUsageLocation::TopLevelChunk, Confidence::Unverified)),
        // Full: a thinking model's text arrives whole as `reasoning` (streamed
        // `delta.reasoning`, buffered `message.reasoning`), recorded live with
        // qwen3.6:35b. A model that does not think returns none, which a session
        // shows as observed rather than as a catalog error.
        reasoning_visibility: ReasoningVisibility::Full,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Self-hosted, no credential. Reports prompt and completion tokens and, \
               on recent builds, `prompt_tokens_details.cached_tokens` for a reused \
               prompt prefix. A thinking model's text arrives as `reasoning` and is \
               kept. Cost derives to zero only if the price table says so, which for \
               a local model it should.",
    },
    Entry {
        id: "llama-cpp",
        vendor: "Local (llama.cpp server)",
        base_url: "http://localhost:8080/v1",
        wire: Wire::OpenAiChat(UsageDialect::Details),
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: false,
        refuses_temperature_with_reasoning: false,
        requires_credential: false,
        shares_credential_with: None,
        confidence: Confidence::Documented,
        streamed_usage: Some((StreamedUsageLocation::TopLevelChunk, Confidence::Unverified)),
        reasoning_visibility: ReasoningVisibility::None,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Self-hosted, no credential. vLLM, LM Studio and text-generation-webui \
               all serve the same surface on different ports; override base_url.",
    },
    Entry {
        id: "gemini",
        vendor: "Google (Gemini native)",
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        wire: Wire::GeminiGenerateContent,
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: true,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: Some("google"),
        confidence: Confidence::Unverified,
        streamed_usage: None,
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Gemini's own generateContent and streamGenerateContent, with tool \
               calling and a thinking control; not the OpenAI-compatible layer that \
               the `google` route uses, and it takes the same API key. Usage is \
               inclusive: the cached share is inside the prompt count, and thinking \
               tokens are counted apart from the candidate count and recorded as \
               reasoning output. A total above the counted parts is billed as \
               reasoning; a total below them is refused. Caching is implicit, with no \
               breakpoint field. Not run against the live service.",
    },
    Entry {
        id: "vertex-gemini",
        vendor: "Google Vertex AI (Gemini)",
        base_url: "https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/google",
        wire: Wire::VertexGemini,
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: true,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Unverified,
        streamed_usage: None,
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Gemini on Vertex AI: the generateContent body with an OAuth bearer \
               token from a service-account key file, the gcloud user credentials \
               file, the metadata server or the gcloud command. Set \
               KGRAF_VERTEX_CREDENTIALS to the key file path, `adc` (the key file \
               GOOGLE_APPLICATION_CREDENTIALS names, else the gcloud user \
               credentials file), `metadata` or `gcloud`; the last two are never \
               tried unless named. Also set KGRAF_VERTEX_PROJECT and \
               KGRAF_VERTEX_REGION; the Google variables \
               GOOGLE_APPLICATION_CREDENTIALS, GOOGLE_CLOUD_PROJECT and \
               GOOGLE_CLOUD_LOCATION are the fallbacks. Usage rules match the `gemini` \
               route. Not run against the live service.",
    },
    Entry {
        id: "vertex-anthropic",
        vendor: "Google Vertex AI (Anthropic)",
        base_url: "https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/anthropic",
        wire: Wire::VertexAnthropic,
        protocol: Protocol::Additive,
        honors_cache_control: false,
        reports_reasoning_tokens: false,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: Some("vertex-gemini"),
        confidence: Confidence::Unverified,
        streamed_usage: None,
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Anthropic models on Vertex AI: the Messages body posted to the model's \
               rawPredict and streamRawPredict URLs, with `anthropic_version: \
               vertex-2023-10-16` in the body and the model in the path, using the \
               same credentials and settings as `vertex-gemini`. Usage is additive, \
               as on the direct route. The model id is `name@version` in the path; the \
               echo check accepts the id as sent, `name-version`, and a bare name \
               answered with its dated id. Cache control is not claimed. Not run \
               against the live service.",
    },
    Entry {
        id: "azure-openai",
        vendor: "Microsoft Azure OpenAI (Chat Completions)",
        base_url: "https://{resource}.openai.azure.com/openai",
        wire: Wire::AzureOpenAiChat,
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: true,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: None,
        confidence: Confidence::Unverified,
        streamed_usage: Some((StreamedUsageLocation::TopLevelChunk, Confidence::Unverified)),
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "The Chat Completions body on an Azure OpenAI resource, sent with the \
               `api-key` header. Set KGRAF_AZURE_OPENAI_API_KEY, \
               KGRAF_AZURE_OPENAI_RESOURCE (the resource name alone), \
               KGRAF_AZURE_OPENAI_API_VERSION (`v1`, or a dated version such as \
               2024-10-21) and KGRAF_AZURE_OPENAI_DEPLOYMENTS \
               (`deployment=model,deployment=model`). The model in a request is the \
               deployment name; the response names the underlying model, so each \
               deployment must be mapped to the exact model string the service \
               returns, and any other answer raises. A completion cut by the content \
               filter, or a filtered prompt, settles the call and raises. Usage is \
               inclusive, as on OpenAI. Microsoft Entra ID tokens, sovereign-cloud \
               hosts and Foundry project hosts are not supported. The output limit is \
               sent as max_completion_tokens, so reasoning deployments work; a dated \
               version must be 2024-09-01-preview or later (2024-10-21 or later without \
               -preview). Deployments Azure updates \
               automatically and model-router deployments cannot be mapped to one \
               model string and cannot be used. A price is looked up only under \
               `<deployment>@azure-openai`, built from the mapped model; a route with \
               no such price shows as unpriced. Not run against the live service.",
    },
    Entry {
        id: "azure-openai-responses",
        vendor: "Microsoft Azure OpenAI (Responses)",
        base_url: "https://{resource}.openai.azure.com/openai",
        wire: Wire::AzureOpenAiResponses,
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: true,
        refuses_temperature_with_reasoning: false,
        requires_credential: true,
        shares_credential_with: Some("azure-openai"),
        confidence: Confidence::Unverified,
        streamed_usage: None,
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "The Responses body on an Azure OpenAI resource, with the same \
               variables, deployment mapping and model check as `azure-openai`. The \
               deployment is the `model` in the body on both address forms. A \
               response with a `content_filters` entry marked blocked, or one \
               incomplete for the content filter, settles the call and raises. Only \
               the `v1` version is accepted, since the Responses body is documented \
               only at /openai/v1/responses. Prices follow `azure-openai`, under \
               `<deployment>@azure-openai-responses`. Not run against the live \
               service.",
    },
    Entry {
        id: "systemone",
        vendor: "Local (System-One server)",
        // The server root: the client appends `/v1/systemone`.
        base_url: "http://localhost:8000",
        wire: Wire::SystemOne,
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: false,
        refuses_temperature_with_reasoning: false,
        requires_credential: false,
        shares_credential_with: None,
        confidence: Confidence::Unverified,
        streamed_usage: None,
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Typed decisions only (decision 0042): a yes/no probability, a pick-one or \
               a score, answered inside the declared space. Serves no generative work. \
               Self-hosted, no credential; the servers that speak this format start with \
               authentication off. The client is tested against recorded replies only; \
               no server has been reached from this route.",
    },
    Entry {
        id: "tei",
        vendor: "Local (Text Embeddings Inference)",
        // TEI's OpenAI-compatible root; the embedding and rerank calls use the
        // native `/embed` and `/rerank` beside it (see `KIND_CLAIMS`).
        base_url: "http://localhost:8080/v1",
        wire: Wire::OpenAiChat(UsageDialect::Details),
        protocol: Protocol::Inclusive,
        honors_cache_control: false,
        reports_reasoning_tokens: false,
        refuses_temperature_with_reasoning: false,
        requires_credential: false,
        shares_credential_with: None,
        confidence: Confidence::Unverified,
        // It serves no chat, so no stream is ever asked of it.
        streamed_usage: Some((StreamedUsageLocation::None, Confidence::Unverified)),
        reasoning_visibility: ReasoningVisibility::Unknown,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Embedding and rerank only, no generative work. Self-hosted, no credential \
               (start the server without --api-key). \
               Serves one model, so its replies name none and the model echo cannot be \
               checked. Asks the server not to truncate. Its default port is 8080, as \
               llama.cpp's is; run one of them elsewhere. Tested against recorded replies \
               only; no server has been reached from this route.",
    },
];

/// The entry with this id.
pub fn entry(id: &str) -> Result<&'static Entry, FleetError> {
    CATALOG
        .iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| FleetError::Unknown {
            id: id.to_owned(),
            known: CATALOG
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>()
                .join(", "),
        })
}

/// The environment variable a route's credential is read from.
///
/// One variable per kind of key, never per vendor. `kimi` and `kimi-anthropic` take
/// the same platform key and both read `KGRAF_KIMI_API_KEY`. `kimi-code` takes a
/// subscription key, which the platform hosts refuse, so it reads its own
/// `KGRAF_KIMI_CODE_API_KEY`. The name is the route id (or the id of the route it
/// shares with) upper-cased with `-` as `_`.
pub fn credential_env_var(entry: &Entry) -> String {
    if entry.wire.is_vertex() {
        return kgraf_provider_google::CREDENTIALS_VARIABLE.to_owned();
    }
    let id = entry.shares_credential_with.unwrap_or(entry.id);
    format!(
        "KGRAF_{}_API_KEY",
        id.to_ascii_uppercase().replace('-', "_")
    )
}

/// The environment variable that moves a local route's endpoint
/// (`KGRAF_OLLAMA_BASE_URL` for `ollama`): the route id upper-cased with `-` as
/// `_`. Read only for routes that take no credential.
pub fn base_url_env_var(entry: &Entry) -> String {
    format!(
        "KGRAF_{}_BASE_URL",
        entry.id.to_ascii_uppercase().replace('-', "_")
    )
}

/// The endpoint root a route is called at, through `lookup` (`std::env::var`
/// in production, a map in tests).
///
/// A route that takes no credential and has a fixed catalog address may be moved
/// with [`base_url_env_var`], for a server on another port or host. A route that
/// takes a credential is never moved this way, so a variable in the environment
/// cannot send a key to another host. A value that is not an http or https URL
/// raises.
pub fn local_endpoint_from(
    entry: &Entry,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String, FleetError> {
    let movable = !entry.requires_credential
        && !entry.wire.is_azure()
        && !entry.wire.is_vertex()
        && !entry.base_url.contains('{');
    if !movable {
        return Ok(entry.base_url.to_owned());
    }
    let variable = base_url_env_var(entry);
    match lookup(&variable) {
        None => Ok(entry.base_url.to_owned()),
        Some(value) => {
            let url = value.trim();
            let rest = url
                .strip_prefix("http://")
                .or_else(|| url.strip_prefix("https://"));
            if rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/')) {
                Ok(url.to_owned())
            } else {
                Err(FleetError::BadBaseUrl {
                    id: entry.id.to_owned(),
                    variable,
                    value,
                })
            }
        }
    }
}

/// [`local_endpoint_from`] over the process environment.
pub fn local_endpoint(entry: &Entry) -> Result<String, FleetError> {
    local_endpoint_from(entry, kgraf_core::legacy::env_var)
}

/// Resolve a route's credential through `lookup`, which is `std::env::var` in
/// production and a map in tests.
///
/// A route that takes no credential resolves to an empty string. A route that needs
/// one and has none **raises**, naming the variable to set: an empty key sent to a
/// provider presents as an authentication failure far from its cause (fail loud).
pub fn credential_from(
    entry: &Entry,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String, FleetError> {
    if !entry.requires_credential {
        return Ok(String::new());
    }
    if entry.wire == Wire::BedrockConverse {
        // AWS credentials are a key pair, a token, a profile and a region rather
        // than one variable. The secret access key stands for them as the value the
        // caller registers for scrubbing; the adapter reads the full set itself on
        // every call.
        return kgraf_provider_bedrock::credential_probe(&lookup).map_err(|e| {
            FleetError::CredentialUnusable {
                id: entry.id.to_owned(),
                reason: e.to_string(),
            }
        });
    }
    if entry.wire.is_azure() {
        // The key, the resource, the API version and the deployment mapping stand
        // together as one credential string. Each is validated here, so a bad
        // resource or version fails before it can reach a hostname or a query.
        return AzureSettings::from_lookup(&lookup)
            .and_then(|settings| {
                settings.check_for(entry.wire == Wire::AzureOpenAiResponses)?;
                Ok(settings)
            })
            .map(|settings| settings.to_credential())
            .map_err(|e| azure_error(entry, e));
    }
    if entry.wire.is_vertex() {
        // Not a secret: where credentials come from, the project and the region,
        // resolved and validated here so a missing one fails before any request.
        return VertexSettings::from_lookup(&lookup)
            .map(|settings| settings.to_credential())
            .map_err(|e| FleetError::CredentialMissing {
                id: entry.id.to_owned(),
                variable: e.variable().to_owned(),
            });
    }
    let variable = credential_env_var(entry);
    match lookup(&variable) {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(FleetError::CredentialMissing {
            id: entry.id.to_owned(),
            variable,
        }),
    }
}

/// Resolve a route's credential from the process environment. A variable set
/// only under its former `HAUSTORIA_*` name is used, with a warning (0062).
pub fn credential_from_env(entry: &Entry) -> Result<String, FleetError> {
    credential_from(entry, kgraf_core::legacy::env_var)
}

/// Build a configuration for a catalog entry.
///
/// `credential` is ignored for an entry that takes none, so a caller does not have to
/// branch on whether the endpoint is local.
pub fn configure(
    entry: &Entry,
    credential: &str,
    models: Vec<String>,
) -> Result<ProviderConfig, FleetError> {
    if entry.wire.is_vertex() {
        let settings = vertex_settings(entry, credential)?;
        return Ok(match entry.wire {
            Wire::VertexAnthropic => {
                kgraf_provider_google::vertex_anthropic_config(&settings, models)
            }
            _ => kgraf_provider_google::vertex_gemini_config(&settings, models),
        });
    }
    if entry.wire.is_azure() {
        return azure_settings(entry, credential)?
            .provider_config(entry.wire.as_str_static(), models)
            .map_err(|source| FleetError::Provider {
                id: entry.id.to_owned(),
                source,
            });
    }
    let mut config = if entry.requires_credential {
        ProviderConfig::new(
            entry.wire.as_str_static(),
            entry.base_url,
            credential,
            models,
            entry.protocol,
        )
        .map_err(|source| FleetError::Provider {
            id: entry.id.to_owned(),
            source,
        })?
    } else {
        ProviderConfig::local(&local_endpoint(entry)?, models, entry.protocol)
    };
    if entry.honors_cache_control {
        config = config.honoring_cache_control();
    }
    Ok(config)
}

/// Check that route `id` can be declared to take a `prompt_cache_key` field
/// (the run file's `prompt_cache_key_routes`): it must be a catalog route on
/// a Chat Completions wire. A Responses route sends the key without a
/// declaration, and every other wire has no such field, so naming one is an
/// error, never a silent no-op.
pub fn check_prompt_cache_key_route(id: &str) -> Result<(), FleetError> {
    let entry = entry(id)?;
    match entry.wire {
        Wire::OpenAiChat(_) | Wire::AzureOpenAiChat => Ok(()),
        _ => Err(FleetError::PromptCacheKeyWire {
            id: id.to_owned(),
            wire: entry.wire.as_str(),
        }),
    }
}

/// The key under which adapters record a route's observed rate limits and last
/// 429, and under which the host reads them back.
///
/// A route whose catalog base URL has no placeholder is keyed by that URL. A route
/// whose URL has one (`{resource}`, `{region}`, `{project}`) is keyed by the URL
/// its adapter holds once the settings are resolved: the catalog text would never
/// match what the adapter records. An Azure route is keyed per deployment, since
/// Azure limits apply per deployment; `model` is the deployment name.
///
/// Fails when the route's settings cannot be resolved, in which case no adapter
/// for it could have recorded anything either.
pub fn limit_key(
    entry: &Entry,
    lookup: impl Fn(&str) -> Option<String>,
    model: &str,
) -> Result<String, FleetError> {
    if !entry.base_url.contains('{') {
        return Ok(local_endpoint_from(entry, &lookup)?
            .trim_end_matches('/')
            .to_owned());
    }
    if entry.wire == Wire::BedrockConverse {
        // The adapter records under the unresolved `{region}` URL, so the key is
        // the catalog text. Reading credentials here would touch the AWS files on
        // every capacity tick and fail on a route that has a key to look under.
        return Ok(entry.base_url.trim_end_matches('/').to_owned());
    }
    let credential = credential_from(entry, &lookup)?;
    if entry.wire.is_azure() {
        return azure_settings(entry, &credential).map(|settings| settings.limit_key(model));
    }
    configure(entry, &credential, Vec::new()).map(|config| config.base_url)
}

/// The key a route's `kind` calls to `model` record their rate limits under.
///
/// Generative calls use [`limit_key`]. The embedding and rerank adapters
/// record under the model's own key, `scoped_key(<limit_key>, model)`, since
/// OpenAI states limits per model and an embedding model's must not overwrite
/// the chat model's; on Azure the deployment key already names the model.
/// A typed-decision route records no headers; its key is [`limit_key`]'s.
pub fn kind_limit_key(
    entry: &Entry,
    lookup: impl Fn(&str) -> Option<String>,
    kind: ModelKind,
    model: &str,
) -> Result<String, FleetError> {
    let base = limit_key(entry, lookup, model)?;
    let per_model = kind_builder(kind).is_some_and(|b| b.limits_per_model);
    Ok(if per_model && !entry.wire.is_azure() {
        kgraf_provider::limits::scoped_key(&base, model)
    } else {
        base
    })
}

/// Whether a route's price is looked up only under its route-scoped key
/// (`<model>@<route>`), never under the bare model name.
///
/// An Azure request names a deployment, and a deployment name is chosen by the
/// account owner: one named `gpt-4o` can serve a different model than the bare
/// `gpt-4o` price describes.
pub fn route_scoped_price_only(route_id: &str) -> bool {
    entry(route_id).is_ok_and(|e| e.wire.is_azure())
}

/// The ids of every route priced only under its route-scoped key.
pub fn route_scoped_price_routes() -> impl Iterator<Item = &'static str> {
    CATALOG.iter().filter(|e| e.wire.is_azure()).map(|e| e.id)
}

/// For an Azure route, each configured deployment with the model it serves, read
/// through `lookup`. `None` for any other route, or when the settings cannot be
/// resolved (the route is then unpriced, never priced by guess).
pub fn azure_deployments(
    entry: &Entry,
    lookup: impl Fn(&str) -> Option<String>,
) -> Option<Vec<(String, String)>> {
    if !entry.wire.is_azure() {
        return None;
    }
    let settings = AzureSettings::from_lookup(&lookup).ok()?;
    Some(
        settings
            .deployments()
            .iter()
            .map(|(deployment, model)| (deployment.clone(), model.clone()))
            .collect(),
    )
}

/// The LiteLLM price-table key for a model served on Azure.
pub fn azure_litellm_key(model: &str) -> String {
    format!("azure/{model}")
}

/// The Azure settings an Azure route's credential string carries.
fn azure_settings(entry: &Entry, credential: &str) -> Result<AzureSettings, FleetError> {
    AzureSettings::from_credential(credential).map_err(|e| azure_error(entry, e))
}

/// A setting error as a fleet error naming the variable and never the key.
fn azure_error(entry: &Entry, error: AzureSettingsError) -> FleetError {
    match error {
        AzureSettingsError::Missing { variable } => FleetError::CredentialMissing {
            id: entry.id.to_owned(),
            variable: variable.to_owned(),
        },
        invalid @ AzureSettingsError::Invalid { .. } => FleetError::CredentialUnusable {
            id: entry.id.to_owned(),
            reason: invalid.to_string(),
        },
    }
}

/// The Vertex settings a Vertex route's credential string carries.
fn vertex_settings(entry: &Entry, credential: &str) -> Result<VertexSettings, FleetError> {
    VertexSettings::from_credential(credential).map_err(|e| FleetError::CredentialMissing {
        id: entry.id.to_owned(),
        variable: e.variable().to_owned(),
    })
}

/// Build a provider for a catalog entry.
///
/// Returns a boxed `dyn Provider`, which is the only thing the agent runtime will ever
/// see: past this function, nothing in kGraf knows which vendor is answering.
pub fn build(
    id: &str,
    credential: &str,
    models: Vec<String>,
) -> Result<Box<dyn Provider>, FleetError> {
    build_with_lookup(id, credential, models, kgraf_core::legacy::env_var)
}

/// [`build`] for a caller that resolves variables through `lookup`. A route whose
/// credentials are several variables (AWS's key pair, token and region) reads them
/// through it on every call, the same lookup [`credential_from`] checked, so the
/// readiness check and the running adapter cannot disagree about the environment.
pub fn build_with_lookup(
    id: &str,
    credential: &str,
    models: Vec<String>,
    lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
) -> Result<Box<dyn Provider>, FleetError> {
    build_declared(id, credential, models, lookup, false)
}

/// [`build_with_lookup`] for a route the run file declares to take
/// `prompt_cache_key` on Chat Completions (`prompt_cache_key`): a declaration
/// on any other wire is refused ([`check_prompt_cache_key_route`]).
pub fn build_declared(
    id: &str,
    credential: &str,
    models: Vec<String>,
    lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    prompt_cache_key: bool,
) -> Result<Box<dyn Provider>, FleetError> {
    if prompt_cache_key {
        check_prompt_cache_key_route(id)?;
    }
    let entry = entry(id)?;
    if !serves(entry, ModelKind::GENERATIVE) {
        return Err(not_served(entry, ModelKind::GENERATIVE));
    }
    // Registered with kGraf's process-wide scrubber (decision 0071) before the
    // provider is built, so every worker transcript and the shell also have
    // this credential scrubbed, not only the request kGraf itself sends. A
    // route that takes no credential passes `""`, which `register_exact_secret`
    // ignores without a word (below `kgraf_secrets::MIN_EXACT_SECRET_LEN`). A
    // placeholder such as LM Studio's `lm-studio` is refused, and the scrubber
    // itself says so to the user (the CLI's stderr, the shell's notice), so the outcome needs no handling here. Safe to call
    // unconditionally: every caller of `build` reaches here,
    // which is the point of this being the one function past which "nothing
    // in kGraf knows which vendor is answering" (see this function's doc).
    if entry.wire.is_azure() {
        // The credential string also carries the resource and version, so the
        // key alone is what the scrubber must know.
        kgraf_secrets::register_exact_secret(
            "provider-api-key",
            azure_settings(entry, credential)?.api_key(),
        );
    } else if !entry.wire.is_vertex() {
        kgraf_secrets::register_exact_secret("provider-api-key", credential);
    }
    let wrap = |source: ProviderError| FleetError::Provider {
        id: entry.id.to_owned(),
        source,
    };
    // A Vertex route has no API key, but its credentials file holds secrets: a
    // service account's private key and key id, a user credential's refresh
    // token and client secret. The file sits where a worker's shell can read
    // it, so its values are registered with the scrubber before the adapter is
    // built, even if the build then fails on the file. The bearer tokens
    // minted from them never leave the adapter.
    if entry.wire.is_vertex() {
        let settings = vertex_settings(entry, credential)?;
        for (kind, value) in
            kgraf_provider_google::credential_file_secrets(&settings.credentials(), &lookup)
        {
            kgraf_secrets::register_exact_secret(kind, &value);
        }
        return Ok(match entry.wire {
            Wire::VertexAnthropic => {
                Box::new(kgraf_provider_google::vertex_anthropic(&settings, models).map_err(wrap)?)
            }
            _ => Box::new(kgraf_provider_google::vertex_gemini(&settings, models).map_err(wrap)?),
        });
    }
    let mut config = configure(entry, credential, models)?;
    if prompt_cache_key {
        config = config.honoring_prompt_cache_key();
    }
    if entry.wire.is_azure() {
        let settings = azure_settings(entry, credential)?;
        return Ok(match entry.wire {
            Wire::AzureOpenAiResponses => Box::new(
                responses_adapter(entry, config)
                    .and_then(|a| a.with_azure(AzureTransport::responses(&settings)?))
                    .map_err(wrap)?,
            ),
            _ => Box::new(
                chat_adapter(entry, config, UsageDialect::Details)
                    .and_then(|a| a.with_azure(AzureTransport::chat(&settings)?))
                    .map_err(wrap)?,
            ),
        });
    }
    Ok(match entry.wire {
        Wire::AnthropicMessages => Box::new(AnthropicAdapter::new(config).map_err(wrap)?),
        Wire::OpenAiResponses => Box::new(responses_adapter(entry, config).map_err(wrap)?),
        Wire::OpenAiChat(dialect) => Box::new(chat_adapter(entry, config, dialect).map_err(wrap)?),
        Wire::BedrockConverse => {
            Box::new(BedrockAdapter::with_lookup(config, lookup).map_err(wrap)?)
        }
        Wire::GeminiGenerateContent => Box::new(GeminiAdapter::api_key(config).map_err(wrap)?),
        Wire::VertexGemini | Wire::VertexAnthropic => {
            unreachable!("Vertex wires return above")
        }
        Wire::AzureOpenAiChat | Wire::AzureOpenAiResponses => {
            unreachable!("Azure wires return above")
        }
        // A wire with no generative adapter. Which routes serve generative
        // work is `KIND_CLAIMS`'s, checked above; this arm exists because the
        // match over wires is exhaustive, and it refuses rather than guesses.
        Wire::SystemOne => return Err(not_served(entry, ModelKind::GENERATIVE)),
    })
}

/// One model kind's builder (decision 0089): how a route of the kind is built
/// into a [`ModelRoute`]. Which routes serve the kind is [`KIND_CLAIMS`]'s.
#[derive(Clone, Copy)]
pub struct KindBuilder {
    pub kind: ModelKind,
    /// `(entry, credential, model)`. The credential is already registered with
    /// the scrubber when this is called.
    pub build: fn(&Entry, &str, &str) -> Result<ModelRoute, FleetError>,
    /// Whether its adapter records rate-limit headers under the model's own
    /// key ([`kind_limit_key`]) rather than the route's.
    pub limits_per_model: bool,
}

/// The kind rows. Adding a kind is one row here and its claims in
/// [`KIND_CLAIMS`]; nothing else in this crate matches on kinds.
pub const KIND_BUILDERS: &[KindBuilder] = &[
    KindBuilder {
        kind: ModelKind::GENERATIVE,
        build: build_generative,
        limits_per_model: false,
    },
    KindBuilder {
        kind: ModelKind::TYPED_DECISION,
        build: build_typed_decision,
        limits_per_model: false,
    },
    KindBuilder {
        kind: ModelKind::EMBEDDING,
        build: build_embedding,
        limits_per_model: true,
    },
    KindBuilder {
        kind: ModelKind::RERANK,
        build: build_rerank,
        limits_per_model: true,
    },
];

/// The kinds one catalog route serves, from the vendor's own documentation, and
/// the shape its non-generative endpoints speak.
#[derive(Clone, Copy, Debug)]
pub struct KindClaim {
    /// The catalog route id.
    pub route: &'static str,
    pub kinds: &'static [ModelKind],
    /// How its embedding endpoint speaks, where it claims the embedding kind.
    pub embed: EmbedDialect,
    /// What one embedding request may carry.
    pub embed_limits: EmbedLimits,
    /// How its rerank endpoint speaks, where it claims the rerank kind.
    pub rerank: RerankDialect,
    /// How its rerank reply's `model` is checked (see [`EchoRule`]).
    pub rerank_echo: EchoRule,
    /// The most documents one rerank request may carry.
    pub rerank_max_documents: usize,
    /// How far the claim is verified: [`Confidence::Verified`] only where a
    /// call of the claimed kind was exchanged with the live service, else
    /// what the cited page shows. A reply field the page does not show is
    /// not claimed as verified.
    pub confidence: Confidence,
    /// Where the vendor documents the endpoints claimed beyond generative.
    pub source: &'static str,
}

impl KindClaim {
    /// A claim with the OpenAI dialects and bounds, not verified live.
    const fn new(route: &'static str, kinds: &'static [ModelKind], source: &'static str) -> Self {
        Self {
            route,
            kinds,
            embed: EmbedDialect::OpenAi,
            embed_limits: EmbedLimits::OPENAI,
            rerank: RerankDialect::Common,
            rerank_echo: EchoRule::Requested,
            rerank_max_documents: kgraf_provider::DEFAULT_RERANK_MAX_DOCUMENTS,
            confidence: Confidence::Unverified,
            source,
        }
    }
}

/// Azure OpenAI's embedding bounds: at most 2,048 inputs per request and
/// 300,000 tokens across them (Microsoft Learn, "How to generate embeddings
/// with Azure OpenAI", best practices).
const AZURE_EMBED_LIMITS: EmbedLimits = EmbedLimits {
    max_inputs: 2_048,
    max_request_tokens: 300_000,
};

const AZURE_EMBEDDINGS_SOURCE: &str = "https://learn.microsoft.com/en-us/azure/ai-foundry/openai/how-to/embeddings and the \
     2024-10-21 inference specification (reply schema: model and usage required)";

/// Routes that serve more than generative work, or other work than it. **A
/// route with no row here serves the generative kind only.** A route claims a
/// kind only where its vendor documents the endpoint at the route's base URL:
/// a claim written from a guess would send a call to an endpoint that is not
/// there, and a claim whose documented reply lacks what the adapter needs
/// (usage, above all) would bill every call and then raise.
///
/// Not claimed, because the vendor's pages document no embedding or rerank
/// endpoint at the route's base URL: Moonshot (`kimi`, `kimi-code`),
/// DeepSeek, z.ai, xAI (`grok`, `grok-chat`) and Meta; Alibaba documents its
/// OpenAI-compatible embeddings on a per-workspace host, not the `qwen`
/// route's `dashscope-intl` base URL. Together's embeddings are not claimed:
/// its reply schema (`EmbeddingsResponse`,
/// <https://docs.together.ai/reference/embeddings-2>) has no `usage`, so every
/// call would bill and then raise `UsageMissing`.
pub const KIND_CLAIMS: &[KindClaim] = &[
    // Verified live: one call of one text (2026-09-29, 20:54 UTC), before
    // request batching, the vector-width check and per-model header recording
    // were added; those are tested against recorded replies only.
    KindClaim {
        confidence: Confidence::Verified,
        ..KindClaim::new(
            "openai",
            &[ModelKind::GENERATIVE, ModelKind::EMBEDDING],
            "https://platform.openai.com/docs/api-reference/embeddings/create",
        )
    },
    // The page shows the endpoint and request only: the reply's `model` and
    // `usage` are not shown, so the echo check and usage reading are
    // unverified here. The embeddings guide says input over the model's
    // limit is cut without an error ("The system silently truncates inputs
    // that exceed this limit", of the 8,192-token limit shared across
    // modalities, https://ai.google.dev/gemini-api/docs/embeddings), and the
    // OpenAI-compatible body has no field to refuse it, so keep texts within
    // the model's limit on this route. No per-request text count is published for this
    // surface; 100 is a bound below any a request would plausibly meet (the
    // Vertex AI page states 250), so a smaller batch costs one more request
    // rather than a refused one.
    KindClaim {
        embed_limits: EmbedLimits {
            max_inputs: 100,
            max_request_tokens: EmbedLimits::OPENAI.max_request_tokens,
        },
        ..KindClaim::new(
            "google",
            &[ModelKind::GENERATIVE, ModelKind::EMBEDDING],
            "https://ai.google.dev/gemini-api/docs/openai#embeddings",
        )
    },
    // The rerank reply documents `usage.prompt_tokens`, and its example names
    // a serving checkpoint (`salesforce/turboranker-0.8-3778-6328`) for a
    // request to `Salesforce/Llama-Rank-V1`, with no rule relating the two:
    // the name is not compared, or every call would bill and then raise.
    KindClaim {
        rerank_echo: EchoRule::NotCompared,
        ..KindClaim::new(
            "together",
            &[ModelKind::GENERATIVE, ModelKind::RERANK],
            "https://docs.together.ai/reference/rerank-1",
        )
    },
    KindClaim {
        embed: EmbedDialect::OllamaNative,
        ..KindClaim::new(
            "ollama",
            &[ModelKind::GENERATIVE, ModelKind::EMBEDDING],
            "https://github.com/ollama/ollama/blob/main/docs/api.md#generate-embeddings",
        )
    },
    // The README shows the endpoints and requests only; the replies (the
    // requested `model` echoed, `usage.prompt_tokens` and `total_tokens`) are
    // those `format_embeddings_response_oaicompat` and
    // `format_response_rerank` build in the server's source.
    KindClaim::new(
        "llama-cpp",
        &[
            ModelKind::GENERATIVE,
            ModelKind::EMBEDDING,
            ModelKind::RERANK,
        ],
        "https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md and \
         https://github.com/ggml-org/llama.cpp/blob/master/tools/server/server-common.cpp",
    ),
    KindClaim {
        embed: EmbedDialect::TeiNative,
        // TEI's default `--max-client-batch-size`, which bounds the texts of
        // an embed request and of a rerank request alike.
        embed_limits: EmbedLimits {
            max_inputs: 32,
            max_request_tokens: EmbedLimits::OPENAI.max_request_tokens,
        },
        rerank: RerankDialect::TeiNative,
        rerank_max_documents: 32,
        ..KindClaim::new(
            "tei",
            &[ModelKind::EMBEDDING, ModelKind::RERANK],
            "https://huggingface.github.io/text-embeddings-inference/openapi.json",
        )
    },
    // Decision 0092: the embeddings body through the Azure transport.
    KindClaim {
        embed_limits: AZURE_EMBED_LIMITS,
        ..KindClaim::new(
            "azure-openai",
            &[ModelKind::GENERATIVE, ModelKind::EMBEDDING],
            AZURE_EMBEDDINGS_SOURCE,
        )
    },
    KindClaim {
        embed_limits: AZURE_EMBED_LIMITS,
        ..KindClaim::new(
            "azure-openai-responses",
            &[ModelKind::GENERATIVE, ModelKind::EMBEDDING],
            AZURE_EMBEDDINGS_SOURCE,
        )
    },
    KindClaim::new(
        "systemone",
        &[ModelKind::TYPED_DECISION],
        "decision 0042: the /v1/systemone wire",
    ),
];

/// The route's claim row, if it has one.
pub fn kind_claim(entry: &Entry) -> Option<&'static KindClaim> {
    KIND_CLAIMS.iter().find(|c| c.route == entry.id)
}

fn kind_builder(kind: ModelKind) -> Option<&'static KindBuilder> {
    KIND_BUILDERS.iter().find(|b| b.kind == kind)
}

/// Whether the catalog route serves `kind`: its [`KIND_CLAIMS`] row lists it,
/// or it has no row and `kind` is generative. A kind with no builder row is
/// served nowhere.
pub fn serves(entry: &Entry, kind: ModelKind) -> bool {
    if kind_builder(kind).is_none() {
        return false;
    }
    match kind_claim(entry) {
        Some(claim) => claim.kinds.contains(&kind),
        None => kind.is_generative(),
    }
}

/// The kinds a catalog route serves, in registry order.
pub fn kinds_served(entry: &Entry) -> Vec<ModelKind> {
    kgraf_core::KINDS
        .iter()
        .map(|spec| spec.kind)
        .filter(|kind| serves(entry, *kind))
        .collect()
}

fn not_served(entry: &Entry, kind: ModelKind) -> FleetError {
    let known: Vec<&str> = CATALOG
        .iter()
        .filter(|e| serves(e, kind))
        .map(|e| e.id)
        .collect();
    FleetError::Kind(KindError::NotServed {
        route: entry.id.to_owned(),
        kind,
        known: if known.is_empty() {
            "none".to_owned()
        } else {
            known.join(", ")
        },
    })
}

/// Build a route of any kind from the catalog.
///
/// A route that does not serve `kind` raises [`KindError::NotServed`]; nothing is
/// substituted. The credential is handled as [`build`] handles it: the same
/// variable, registered with the scrubber before anything is built.
pub fn build_kind(
    kind: ModelKind,
    id: &str,
    credential: &str,
    model: &str,
) -> Result<ModelRoute, FleetError> {
    let entry = entry(id)?;
    let builder = kind_builder(kind)
        .filter(|_| serves(entry, kind))
        .ok_or_else(|| not_served(entry, kind))?;
    if entry.wire.is_azure() {
        // The credential string also carries the resource, version and
        // deployments; the key alone is the secret, as in `build`.
        kgraf_secrets::register_exact_secret(
            "provider-api-key",
            azure_settings(entry, credential)?.api_key(),
        );
    } else {
        kgraf_secrets::register_exact_secret("provider-api-key", credential);
    }
    (builder.build)(entry, credential, model)
}

fn route_name(entry: &Entry, model: &str) -> String {
    format!("{}:{model}", entry.id)
}

fn build_generative(
    entry: &Entry,
    credential: &str,
    model: &str,
) -> Result<ModelRoute, FleetError> {
    let provider: Arc<dyn Provider> = Arc::from(build(entry.id, credential, vec![model.into()])?);
    Ok(ModelRoute::new(
        route_name(entry, model),
        ModelKind::GENERATIVE,
        provider.adapter(),
        provider.protocol(),
        model,
        provider,
    ))
}

/// The claim row of a route [`build_kind`] already found serving a
/// non-generative kind: every such route has one.
fn claim_of(entry: &Entry, kind: ModelKind) -> Result<&'static KindClaim, FleetError> {
    kind_claim(entry).ok_or_else(|| not_served(entry, kind))
}

fn build_embedding(entry: &Entry, credential: &str, model: &str) -> Result<ModelRoute, FleetError> {
    let claim = claim_of(entry, ModelKind::EMBEDDING)?;
    let config = configure_input_only(entry, credential, model)?;
    let wrap = |source| FleetError::Provider {
        id: entry.id.to_owned(),
        source,
    };
    let mut built = OpenAiEmbeddings::new(config)
        .and_then(|e| e.with_dialect(claim.embed))
        .map_err(wrap)?
        .with_limits(claim.embed_limits);
    if entry.wire.is_azure() {
        let settings = azure_settings(entry, credential)?;
        built = AzureTransport::embeddings(&settings)
            .and_then(|transport| built.with_azure(transport))
            .map_err(wrap)?;
    }
    let embedder: Arc<dyn Embedder> = Arc::new(built);
    Ok(ModelRoute::new(
        route_name(entry, model),
        ModelKind::EMBEDDING,
        embedder.adapter(),
        embedder.protocol(),
        model,
        embedder,
    ))
}

fn build_rerank(entry: &Entry, credential: &str, model: &str) -> Result<ModelRoute, FleetError> {
    let claim = claim_of(entry, ModelKind::RERANK)?;
    let config = configure_input_only(entry, credential, model)?;
    let reranker: Arc<dyn Reranker> = Arc::new(
        OpenAiRerank::new(config)
            .and_then(|r| r.with_dialect(claim.rerank))
            .map_err(|source| FleetError::Provider {
                id: entry.id.to_owned(),
                source,
            })?
            .with_echo(claim.rerank_echo)
            .with_max_documents(claim.rerank_max_documents),
    );
    Ok(ModelRoute::new(
        route_name(entry, model),
        ModelKind::RERANK,
        reranker.adapter(),
        reranker.protocol(),
        model,
        reranker,
    ))
}

/// The route's configuration for an input-only kind: cache control has no
/// meaning on an embedding or rerank call, so it is off whatever the route says.
fn configure_input_only(
    entry: &Entry,
    credential: &str,
    model: &str,
) -> Result<ProviderConfig, FleetError> {
    let mut config = configure(entry, credential, vec![model.to_owned()])?;
    config.honors_cache_control = false;
    Ok(config)
}

/// The state a typed-decision route reads, in estimated tokens. The small
/// models in this class sit near 8k (decision 0042); a larger state raises
/// rather than being truncated.
pub const TYPED_DECISION_STATE_LIMIT: u64 = 8_192;

fn build_typed_decision(
    entry: &Entry,
    credential: &str,
    model: &str,
) -> Result<ModelRoute, FleetError> {
    if entry.requires_credential && credential.trim().is_empty() {
        return Err(FleetError::CredentialMissing {
            id: entry.id.to_owned(),
            variable: credential_env_var(entry),
        });
    }
    let mut client =
        kgraf_decide::SystemOneClient::new(&local_endpoint(entry)?, TYPED_DECISION_STATE_LIMIT)
            .map_err(|source| FleetError::Decider {
                id: entry.id.to_owned(),
                source,
            })?
            .with_model(model);
    if !credential.trim().is_empty() {
        client = client.with_api_key(credential);
    }
    let decider: Arc<dyn kgraf_decide::Decider> = Arc::new(client);
    Ok(ModelRoute::new(
        route_name(entry, model),
        ModelKind::TYPED_DECISION,
        kgraf_decide::systemone::BACKEND,
        entry.protocol,
        model,
        decider,
    ))
}

/// The Responses adapter for `entry`, with the route's declared flags applied.
fn responses_adapter(
    entry: &Entry,
    config: ProviderConfig,
) -> Result<OpenAiAdapter, ProviderError> {
    let adapter = OpenAiAdapter::new(config)?.with_reasoning_dialect(entry.reasoning_dialect);
    Ok(if entry.refuses_temperature_with_reasoning {
        adapter.refusing_temperature_with_reasoning()
    } else {
        adapter
    })
}

/// The Chat Completions adapter for `entry`, with the route's declared flags applied.
fn chat_adapter(
    entry: &Entry,
    config: ProviderConfig,
    dialect: UsageDialect,
) -> Result<ChatCompletionsAdapter, ProviderError> {
    let mut adapter = ChatCompletionsAdapter::new(config, dialect)?
        .with_reasoning_dialect(entry.reasoning_dialect);
    if let Some((location, _confidence)) = entry.streamed_usage {
        adapter = adapter.streaming_usage_from(location);
    }
    Ok(if entry.reports_reasoning_tokens {
        adapter.reporting_reasoning_tokens()
    } else {
        adapter
    })
}

/// The litellm provider prefix under which a route's models are priced, where
/// the route's vendor is known to match one. `None` means the provider is not
/// known and no prefixed price is ever tried for the route.
///
/// Only the platform routes of a vendor litellm prices under its own prefix
/// are listed. The Kimi Code subscription route is not: its billing is not the
/// platform's per-token price.
pub fn litellm_provider(route_id: &str) -> Option<&'static str> {
    match route_id {
        "kimi" | "kimi-anthropic" => Some("moonshot"),
        "gemini" => Some("gemini"),
        "vertex-gemini" | "vertex-anthropic" => Some("vertex_ai"),
        _ => None,
    }
}

impl Wire {
    /// A `'static` name, needed because `ProviderConfig` holds one for error messages.
    fn as_str_static(self) -> &'static str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_vendor_the_project_promises_has_a_route() {
        // If a vendor named in the project's documentation has no entry, it is not
        // reachable, whatever the architecture says.
        for vendor in [
            "anthropic",
            "openai",
            "kimi",
            "kimi-code",
            "meta",
            "zai",
            "deepseek",
            "qwen",
            "grok",
            "google",
            "gemini",
            "vertex-gemini",
            "vertex-anthropic",
            "azure-openai",
            "azure-openai-responses",
            "together",
            "ollama",
            "llama-cpp",
        ] {
            entry(vendor).unwrap_or_else(|e| panic!("{vendor}: {e}"));
        }
    }

    /// An unverified route has not been probed live, so it must not assert what
    /// reasoning it returns: it declares `Unknown`, never `Full`, `Summary` or
    /// `None` (0067). Mutation check: setting one
    /// `Unverified` entry to a declared value other than `Unknown` fails this
    /// test (tried by hand while writing it, reverted).
    #[test]
    fn every_unverified_entry_declares_unknown_reasoning_visibility() {
        for e in CATALOG {
            if e.confidence == Confidence::Unverified {
                assert_eq!(
                    e.reasoning_visibility,
                    ReasoningVisibility::Unknown,
                    "{}: an unverified route must declare unknown reasoning visibility",
                    e.id
                );
            }
        }
    }

    #[test]
    fn ids_are_unique() {
        let ids: BTreeSet<&str> = CATALOG.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), CATALOG.len(), "duplicate catalog id");
    }

    #[test]
    fn an_unknown_id_names_what_is_available() {
        let err = entry("nosuchvendor").expect_err("must raise");
        match err {
            FleetError::Unknown { known, .. } => {
                assert!(
                    known.contains("anthropic"),
                    "the error must be actionable: {known}"
                );
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn no_entry_hardcodes_model_names() {
        // Model identifiers change faster than a release, and a stale list is worse
        // than none: at best it trips the model-echo assertion, at worst it routes to
        // a cheap tier that answers plausibly. The caller supplies the models.
        for entry in CATALOG {
            let text = format!("{} {}", entry.base_url, entry.note).to_lowercase();
            for invented in [
                "gpt-4", "gpt-5", "claude-3", "claude-4", "glm-4", "qwen-max",
            ] {
                assert!(
                    !text.contains(invented),
                    "{} mentions a specific model name: {invented}",
                    entry.id
                );
            }
        }
    }

    #[test]
    fn the_additive_convention_is_confined_to_the_anthropic_wire() {
        // Every Chat Completions and Responses variant surveyed is inclusive. An
        // additive entry on either would undercount every cached request. The
        // Messages shape and Bedrock's Converse are the additive ones.
        for entry in CATALOG {
            match entry.wire {
                Wire::AnthropicMessages | Wire::VertexAnthropic => {}
                // Converse reports input excluding the cache counts, like the
                // Messages shape.
                Wire::BedrockConverse => assert_eq!(entry.protocol, Protocol::Additive),
                _ => assert_eq!(
                    entry.protocol,
                    Protocol::Inclusive,
                    "{} is an OpenAI-family wire marked additive",
                    entry.id
                ),
            }
        }
    }

    #[test]
    fn streamed_usage_is_declared_only_on_the_chat_completions_wire() {
        // The fact is meaningless off Chat Completions: the other two protocols
        // handle streamed usage inside their own adapters, not through this catalog
        // field.
        for entry in CATALOG {
            match entry.wire {
                Wire::OpenAiChat(_) | Wire::AzureOpenAiChat => assert!(
                    entry.streamed_usage.is_some(),
                    "{} is a Chat Completions route and must declare where its \
                     streamed usage arrives",
                    entry.id
                ),
                _ => assert!(
                    entry.streamed_usage.is_none(),
                    "{} is not a Chat Completions route and must not declare a \
                     streamed-usage fact",
                    entry.id
                ),
            }
        }
    }

    #[test]
    fn the_kimi_route_s_streamed_usage_location_is_verified() {
        // A live streamed call on 2026-09-26 confirmed the earlier recollection:
        // Kimi reports streamed usage inside `choices[0].usage`, recorded at
        // `tests/fixtures/2026-09-26-kimi-chat-stream.sse`. The
        // catalog must not understate the confidence once a live call has settled it.
        let kimi = entry("kimi").expect("kimi");
        assert_eq!(
            kimi.streamed_usage,
            Some((StreamedUsageLocation::InChoice, Confidence::Verified))
        );
    }

    #[tokio::test]
    async fn a_route_declared_to_send_no_streamed_usage_is_wired_through_to_refuse_streaming() {
        // The mechanism, not the setting (test the mechanism, not the setting): a synthetic entry declared
        // `StreamedUsageLocation::None` must produce an adapter that refuses to
        // stream at all, built the same way `build()` builds every real entry,
        // not just an adapter method called directly.
        let synthetic = Entry {
            streamed_usage: Some((StreamedUsageLocation::None, Confidence::Verified)),
            ..*entry("ollama").expect("ollama")
        };
        let config = configure(&synthetic, "", vec!["a-model".into()]).expect("configure");
        let mut adapter =
            ChatCompletionsAdapter::new(config, UsageDialect::Details).expect("adapter");
        if let Some((location, _)) = synthetic.streamed_usage {
            adapter = adapter.streaming_usage_from(location);
        }

        let ledger = kgraf_provider::Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(
                synthetic.wire.as_str(),
                synthetic.protocol,
                "a-model",
                "w",
                "t",
            )
            .expect("begin");
        let err = kgraf_provider::capability::Streaming::stream(
            &adapter,
            &kgraf_provider::Request::new("a-model", "hi", 10),
            &token,
            &mut |_| {},
        )
        .await
        .expect_err("must refuse before sending");
        assert!(matches!(err, ProviderError::CapabilityNotHonored { .. }));
    }

    /// Only a Chat Completions route can be declared to take the key; an
    /// unknown route or another wire fails loud.
    #[test]
    fn only_a_chat_route_can_be_declared_to_take_the_prompt_cache_key() {
        assert!(check_prompt_cache_key_route("kimi").is_ok());
        assert!(matches!(
            check_prompt_cache_key_route("no-such-route"),
            Err(FleetError::Unknown { .. })
        ));
        for non_chat in ["openai", "anthropic", "gemini"] {
            assert!(
                matches!(
                    check_prompt_cache_key_route(non_chat),
                    Err(FleetError::PromptCacheKeyWire { .. })
                ),
                "{non_chat}"
            );
        }
        let declared = build_declared(
            "kimi",
            "placeholder-key-value",
            vec!["m".into()],
            |_| None,
            true,
        );
        assert!(declared.is_ok());
        let refused = build_declared(
            "openai",
            "placeholder-key-value",
            vec!["m".into()],
            |_| None,
            true,
        );
        assert!(matches!(
            refused.err(),
            Some(FleetError::PromptCacheKeyWire { .. })
        ));
    }

    #[test]
    fn cache_control_is_claimed_only_where_it_was_verified() {
        // Decision 0006: the flag defaults to false, because emitting breakpoints at a
        // provider that drops them costs money silently and gains nothing.
        for entry in CATALOG {
            if entry.honors_cache_control {
                assert_eq!(
                    entry.wire,
                    Wire::AnthropicMessages,
                    "{} claims cache control on a wire that has no breakpoint field",
                    entry.id
                );
                assert_ne!(
                    entry.confidence,
                    Confidence::Unverified,
                    "{} claims cache control without having verified it",
                    entry.id
                );
            }
        }
    }

    #[test]
    fn the_two_kimi_routes_disagree_on_the_convention_and_that_is_the_point() {
        // The concrete case decision 0006 was written around: one API key, two routes,
        // opposite semantics. If these ever match, the catalog has lost the
        // distinction that makes per-protocol accounting necessary.
        let native = entry("kimi").expect("kimi");
        let anthropic_route = entry("kimi-anthropic").expect("kimi-anthropic");
        assert_ne!(native.protocol, anthropic_route.protocol);
        assert_eq!(native.vendor, anthropic_route.vendor);
    }

    #[test]
    fn a_vendor_with_a_confirmed_silent_failure_says_so_in_its_note() {
        // A catalog that presented every row with equal authority would be the
        // silent-degradation failure the fail-loud rule exists to stop.
        for id in ["deepseek", "google"] {
            let entry = entry(id).expect(id);
            assert!(
                entry.note.contains("WARNING"),
                "{id} has a confirmed silent failure and its note does not warn"
            );
        }
    }

    #[test]
    fn local_entries_need_no_credential_and_build_without_one() {
        for id in ["ollama", "llama-cpp"] {
            let entry = entry(id).expect(id);
            assert!(!entry.requires_credential, "{id} should need no credential");
            assert!(
                entry.base_url.starts_with("http://localhost"),
                "{id} should default to localhost"
            );
            let provider = build(id, "", vec!["whatever-the-user-pulled".into()])
                .unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(provider.protocol(), Protocol::Inclusive);
        }
    }

    #[test]
    fn a_remote_entry_still_refuses_a_missing_credential() {
        match build("anthropic", "", vec!["m".into()]) {
            Err(FleetError::Provider { .. }) => {}
            Err(other) => panic!("wrong error: {other:?}"),
            Ok(_) => panic!("a remote entry must refuse an empty credential"),
        }
    }

    /// A credential that builds `entry`: the Vertex routes need the settings
    /// their credential step resolves, not an API key, and the Azure routes need
    /// a resource, an API version and a mapping for the model the tests ask for.
    fn test_credential(entry: &Entry) -> String {
        credential_from(entry, |name| match name {
            "KGRAF_VERTEX_CREDENTIALS" => Some("adc".to_owned()),
            "KGRAF_VERTEX_PROJECT" => Some("a-project".to_owned()),
            "KGRAF_VERTEX_REGION" => Some("us-central1".to_owned()),
            "KGRAF_AZURE_OPENAI_API_KEY" => Some("an-azure-key-value".to_owned()),
            "KGRAF_AZURE_OPENAI_RESOURCE" => Some("a-resource".to_owned()),
            "KGRAF_AZURE_OPENAI_API_VERSION" => Some("v1".to_owned()),
            "KGRAF_AZURE_OPENAI_DEPLOYMENTS" => Some("a-model=an-underlying-model".to_owned()),
            _ => None,
        })
        .unwrap_or_else(|_| "a-credential".to_owned())
    }

    #[test]
    fn every_kind_in_the_registry_has_exactly_one_builder() {
        for spec in kgraf_core::KINDS {
            let rows = KIND_BUILDERS.iter().filter(|b| b.kind == spec.kind).count();
            assert_eq!(rows, 1, "{} has {rows} builder rows", spec.kind);
        }
    }

    #[test]
    fn every_entry_serves_at_least_one_kind_and_the_non_generative_ones_are_refused_as_workers() {
        let mut non_generative = Vec::new();
        for entry in CATALOG {
            let kinds = kinds_served(entry);
            assert!(!kinds.is_empty(), "{} serves no kind", entry.id);
            if !kinds.contains(&ModelKind::GENERATIVE) {
                non_generative.push(entry.id);
                assert!(
                    matches!(
                        build(entry.id, "", vec!["m".into()]),
                        Err(FleetError::Kind(KindError::NotServed { .. }))
                    ),
                    "{}",
                    entry.id
                );
            }
        }
        assert_eq!(non_generative, vec!["systemone", "tei"]);
    }

    #[test]
    fn every_claim_names_a_catalog_route_once_and_cites_its_source() {
        let mut seen = std::collections::BTreeSet::new();
        for claim in KIND_CLAIMS {
            assert!(
                entry(claim.route).is_ok(),
                "{} is not in the catalog",
                claim.route
            );
            assert!(seen.insert(claim.route), "{} is claimed twice", claim.route);
            assert!(!claim.kinds.is_empty(), "{}", claim.route);
            assert!(!claim.source.is_empty(), "{}", claim.route);
        }
    }

    #[test]
    fn embedding_and_rerank_are_claimed_only_on_the_openai_family_wires() {
        // The embedding and rerank adapters speak the OpenAI family's
        // credential and base URL; a claim on another wire would build a
        // route that cannot authenticate.
        for entry in CATALOG {
            if serves(entry, ModelKind::EMBEDDING) || serves(entry, ModelKind::RERANK) {
                assert!(
                    matches!(
                        entry.wire,
                        Wire::OpenAiResponses
                            | Wire::OpenAiChat(_)
                            | Wire::AzureOpenAiChat
                            | Wire::AzureOpenAiResponses
                    ),
                    "{}",
                    entry.id
                );
            }
            // Rerank is not served through the Azure transport.
            if entry.wire.is_azure() {
                assert!(!serves(entry, ModelKind::RERANK), "{}", entry.id);
            }
        }
    }

    #[test]
    fn only_a_claim_exchanged_with_the_live_service_is_marked_verified() {
        for claim in KIND_CLAIMS {
            let expected = if claim.route == "openai" {
                Confidence::Verified
            } else {
                Confidence::Unverified
            };
            assert_eq!(claim.confidence, expected, "{}", claim.route);
        }
    }

    /// The generative routes with a recorded live exchange, and the date of
    /// the first one, as `docs/MODEL-SUPPORT.md` records them. A route is added
    /// here only together with its recorded exchange.
    const LIVE_RECORDED_GENERATIVE: &[(&str, &str)] = &[
        ("anthropic", "2026-09-28"),
        ("openai", "2026-09-26"),
        ("kimi-anthropic", "2026-09-26"),
        ("kimi", "2026-09-26"),
        ("ollama", "2026-09-28"),
    ];

    #[test]
    fn the_catalog_confidence_matches_the_recorded_live_exchanges() {
        let support = include_str!("../../../docs/MODEL-SUPPORT.md");
        let mut seen = 0;
        for line in support.lines() {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            // `| generative | `id` | vendor | wire | status |` splits to 7 cells.
            if cells.len() != 7 || cells[1] != "generative" {
                continue;
            }
            let id = cells[2].trim_matches('`');
            let status = cells[5];
            let entry = CATALOG
                .iter()
                .find(|e| e.id == id)
                .unwrap_or_else(|| panic!("{id} is in MODEL-SUPPORT.md but not in the catalog"));
            seen += 1;
            let recorded = LIVE_RECORDED_GENERATIVE.iter().find(|(r, _)| *r == id);
            match recorded {
                Some((_, date)) => {
                    assert!(
                        status.starts_with("VERIFIED") && status.contains(date),
                        "{id}: the doc row must say VERIFIED with {date}, has {status:?}"
                    );
                    assert_eq!(entry.confidence, Confidence::Verified, "{id}");
                }
                None => {
                    assert!(
                        status.starts_with("BUILT, UNTESTED"),
                        "{id}: no live exchange is listed for it, yet the doc says {status:?}"
                    );
                    assert_ne!(
                        entry.confidence,
                        Confidence::Verified,
                        "{id} has no recorded live exchange, so it cannot be Verified"
                    );
                }
            }
        }
        assert!(seen >= 20, "the doc table was not parsed: {seen} rows");
        // Every route listed as recorded must be in the doc table.
        for (id, _) in LIVE_RECORDED_GENERATIVE {
            assert!(
                support.contains(&format!("| generative | `{id}` |")),
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn each_claim_bounds_its_requests_as_its_vendor_documents() {
        let limits = |id: &str| {
            let e = entry(id).expect(id);
            build_kind(ModelKind::EMBEDDING, id, &test_credential(e), "a-model")
                .unwrap_or_else(|err| panic!("{id}: {err}"))
                .handle::<Arc<dyn Embedder>>(ModelKind::EMBEDDING)
                .expect("embedder")
                .limits()
        };
        assert_eq!(limits("google").max_inputs, 100);
        assert_eq!(limits("azure-openai"), AZURE_EMBED_LIMITS);
        assert_eq!(limits("openai"), EmbedLimits::OPENAI);
        let max_documents = |id: &str| {
            build_kind(ModelKind::RERANK, id, "", "r")
                .expect(id)
                .handle::<Arc<dyn Reranker>>(ModelKind::RERANK)
                .expect("reranker")
                .max_documents()
        };
        assert_eq!(max_documents("tei"), 32);
        assert_eq!(
            max_documents("llama-cpp"),
            kgraf_provider::DEFAULT_RERANK_MAX_DOCUMENTS
        );
    }

    #[test]
    fn an_azure_embedding_route_builds_the_azure_transport_and_registers_only_the_key() {
        let e = entry("azure-openai").expect("azure");
        let route = build_kind(
            ModelKind::EMBEDDING,
            "azure-openai",
            &test_credential(e),
            "a-model",
        )
        .expect("azure embedding");
        assert_eq!(
            route.adapter(),
            kgraf_provider_openai::azure::EMBEDDINGS_ADAPTER
        );
        assert!(route_scoped_price_only("azure-openai"));
        // A deployment the settings do not map is refused when built.
        assert!(matches!(
            build_kind(
                ModelKind::EMBEDDING,
                "azure-openai",
                &test_credential(e),
                "unmapped"
            ),
            Err(FleetError::Provider { .. })
        ));
    }

    #[test]
    fn embedding_limits_are_keyed_per_model_and_generative_ones_per_route() {
        let openai = entry("openai").expect("openai");
        let lookup = |_: &str| Some("a-credential".to_owned());
        let route = limit_key(openai, lookup, "gpt").expect("route key");
        assert_eq!(
            kind_limit_key(openai, lookup, ModelKind::GENERATIVE, "gpt").expect("gen"),
            route
        );
        assert_eq!(
            kind_limit_key(openai, lookup, ModelKind::EMBEDDING, "e").expect("embed"),
            kgraf_provider::limits::scoped_key(&route, "e")
        );
        // Azure keys already name the deployment.
        let azure = entry("azure-openai").expect("azure");
        let azure_lookup = |v: &str| {
            Some(match v {
                "KGRAF_AZURE_OPENAI_RESOURCE" => "a-resource".to_owned(),
                "KGRAF_AZURE_OPENAI_API_VERSION" => "v1".to_owned(),
                "KGRAF_AZURE_OPENAI_DEPLOYMENTS" => "a-model=an-underlying-model".to_owned(),
                _ => "an-azure-key-value".to_owned(),
            })
        };
        assert_eq!(
            kind_limit_key(azure, azure_lookup, ModelKind::EMBEDDING, "a-model").expect("az"),
            limit_key(azure, azure_lookup, "a-model").expect("az route")
        );
    }

    #[test]
    fn a_route_whose_vendor_documents_no_embedding_endpoint_does_not_claim_one() {
        for id in [
            "kimi",
            "kimi-code",
            "deepseek",
            "zai",
            "qwen",
            "grok",
            "grok-chat",
            "meta",
        ] {
            let e = entry(id).expect(id);
            assert_eq!(kinds_served(e), vec![ModelKind::GENERATIVE], "{id}");
            assert!(matches!(
                build_kind(ModelKind::EMBEDDING, id, &test_credential(e), "m"),
                Err(FleetError::Kind(KindError::NotServed { .. }))
            ));
        }
        // A typed-decision wire claims no generative work, and the claims, not
        // the wire, say so.
        assert_eq!(
            kinds_served(entry("systemone").expect("systemone")),
            vec![ModelKind::TYPED_DECISION]
        );
    }

    /// Together's embeddings reply schema (`EmbeddingsResponse` in
    /// <https://docs.together.ai/reference/embeddings-2>) has `object`, `model`
    /// and `data` only: no `usage`. Every call would bill and then raise
    /// `UsageMissing`, by design of the reply, so the route claims no
    /// embeddings. Its rerank reply documents `usage`, so rerank stays.
    #[test]
    fn together_claims_no_embeddings_because_its_documented_reply_has_no_usage() {
        let together = entry("together").expect("together");
        assert!(!serves(together, ModelKind::EMBEDDING));
        assert!(matches!(
            build_kind(ModelKind::EMBEDDING, "together", "a-credential", "m"),
            Err(FleetError::Kind(KindError::NotServed { .. }))
        ));
        assert!(serves(together, ModelKind::RERANK));
    }

    #[tokio::test]
    async fn the_tei_route_builds_its_native_dialects_and_its_batch_bound() {
        let route = build_kind(ModelKind::EMBEDDING, "tei", "", "bge").expect("tei embedding");
        let embedder = route
            .handle::<Arc<dyn Embedder>>(ModelKind::EMBEDDING)
            .expect("embedder");
        assert_eq!(embedder.limits().max_inputs, 32);
        build_kind(ModelKind::RERANK, "tei", "", "bge")
            .expect("tei rerank")
            .handle::<Arc<dyn Reranker>>(ModelKind::RERANK)
            .expect("reranker");
        let route = build_kind(ModelKind::EMBEDDING, "openai", "a-credential", "e").expect("e");
        let embedder = route
            .handle::<Arc<dyn Embedder>>(ModelKind::EMBEDDING)
            .expect("embedder");
        assert_eq!(embedder.limits(), EmbedLimits::OPENAI);
    }

    #[test]
    fn a_non_generative_route_is_refused_as_a_worker_and_names_the_generative_routes() {
        let err = build("systemone", "", vec!["jev-latest".into()])
            .err()
            .expect("refused");
        let text = err.to_string();
        assert!(
            matches!(err, FleetError::Kind(KindError::NotServed { .. })),
            "{text}"
        );
        assert!(text.contains("anthropic"), "{text}");
    }

    #[test]
    fn generative_work_on_a_typed_decision_route_and_the_reverse_both_raise() {
        assert!(matches!(
            build_kind(ModelKind::GENERATIVE, "systemone", "", "m"),
            Err(FleetError::Kind(KindError::NotServed { .. }))
        ));
        assert!(matches!(
            build_kind(ModelKind::TYPED_DECISION, "ollama", "", "m"),
            Err(FleetError::Kind(KindError::NotServed { .. }))
        ));
        // Anthropic's wire has no embeddings endpoint.
        assert!(matches!(
            build_kind(ModelKind::EMBEDDING, "anthropic", "a-credential", "m"),
            Err(FleetError::Kind(KindError::NotServed { .. }))
        ));
    }

    #[test]
    fn each_kind_builds_a_route_holding_its_own_handle() {
        let route = build_kind(ModelKind::EMBEDDING, "openai", "a-credential", "e").expect("e");
        assert_eq!(route.name(), "openai:e");
        assert_eq!(route.adapter(), kgraf_provider_openai::EMBEDDINGS_ADAPTER);
        route
            .handle::<Arc<dyn Embedder>>(ModelKind::EMBEDDING)
            .expect("embedder");
        assert!(
            route
                .handle::<Arc<dyn Provider>>(ModelKind::GENERATIVE)
                .is_err()
        );
        let route = build_kind(ModelKind::RERANK, "llama-cpp", "", "r").expect("r");
        route
            .handle::<Arc<dyn Reranker>>(ModelKind::RERANK)
            .expect("reranker");
        let route =
            build_kind(ModelKind::TYPED_DECISION, "systemone", "", "jev-latest").expect("decider");
        assert_eq!(route.adapter(), kgraf_decide::systemone::BACKEND);
        route
            .handle::<Arc<dyn kgraf_decide::Decider>>(ModelKind::TYPED_DECISION)
            .expect("decider handle");
        let route = build_kind(ModelKind::GENERATIVE, "ollama", "", "g").expect("g");
        route
            .handle::<Arc<dyn Provider>>(ModelKind::GENERATIVE)
            .expect("provider");
    }

    #[test]
    fn a_remote_embedding_route_still_refuses_a_missing_credential() {
        assert!(matches!(
            build_kind(ModelKind::EMBEDDING, "openai", "", "e"),
            Err(FleetError::Provider { .. })
        ));
    }

    #[test]
    fn every_entry_builds_and_reports_its_own_protocol() {
        for entry in CATALOG.iter().filter(|e| serves(e, ModelKind::GENERATIVE)) {
            let provider = build(entry.id, &test_credential(entry), vec!["a-model".into()])
                .unwrap_or_else(|e| panic!("{}: {e}", entry.id));
            assert_eq!(
                provider.protocol(),
                entry.protocol,
                "{} builds with the wrong protocol",
                entry.id
            );
            assert_eq!(provider.adapter(), entry.wire.as_str());
        }
    }

    #[test]
    fn the_declared_reasoning_capability_matches_what_is_built() {
        for entry in CATALOG.iter().filter(|e| serves(e, ModelKind::GENERATIVE)) {
            let provider = build(entry.id, &test_credential(entry), vec!["a-model".into()])
                .unwrap_or_else(|e| panic!("{}: {e}", entry.id));
            let declared = provider.capabilities().reasoning.is_some();
            // The Anthropic wire bills reasoning inside output and never reports it
            // separately, so it declares nothing regardless of the catalog row.
            let expected = entry.reports_reasoning_tokens
                && !matches!(entry.wire, Wire::AnthropicMessages | Wire::VertexAnthropic);
            assert_eq!(
                declared, expected,
                "{} declares reasoning reporting as {declared}, catalog says {expected}",
                entry.id
            );
        }
    }

    #[test]
    fn an_unverified_entry_is_honest_about_it() {
        // Not a style rule. A reader choosing a route needs to know whether the usage
        // shape behind it was confirmed or assumed.
        let unverified: Vec<&str> = CATALOG
            .iter()
            .filter(|e| e.confidence == Confidence::Unverified)
            .map(|e| e.id)
            .collect();
        assert!(
            !unverified.is_empty(),
            "no route is marked unverified, which given the research is not credible"
        );
        for id in unverified {
            let entry = entry(id).expect(id);
            assert!(
                !entry.honors_cache_control,
                "{id} is unverified and must not claim a paid capability"
            );
        }
    }

    #[test]
    fn only_routes_measured_to_reject_it_refuse_temperature_with_reasoning() {
        let refusing: Vec<&str> = CATALOG
            .iter()
            .filter(|entry| entry.refuses_temperature_with_reasoning)
            .map(|entry| entry.id)
            .collect();
        assert_eq!(refusing, vec!["openai"]);
    }

    #[test]
    fn a_subscription_key_and_a_platform_key_reach_different_kimi_hosts() {
        let platform = entry("kimi").expect("kimi");
        let code = entry("kimi-code").expect("kimi-code");
        assert_ne!(platform.base_url, code.base_url);
        assert!(code.base_url.starts_with("https://api.kimi.com/coding"));
    }

    #[tokio::test]
    async fn the_openai_route_built_from_the_catalog_refuses_before_sending() {
        // Through `build`, not the flag: a catalog value that never reached the
        // adapter would pass a test of the value and refuse nothing (test the mechanism, not the setting). The
        // real endpoint is configured, and the refusal must come before any
        // connection is attempted.
        use kgraf_provider::message::ReasoningEffort;
        use kgraf_provider::{Ledger, Request, Settlement};
        let provider = build("openai", "not-a-real-key", vec!["m".into()]).expect("build");
        let mut request = Request::new("m", "hi", 10);
        request.temperature = Some(0.5);
        request.reasoning_effort = Some(ReasoningEffort::Low);
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(
                "openai-responses",
                kgraf_core::Protocol::Inclusive,
                "m",
                "w",
                "t",
            )
            .expect("begin");
        let err = provider
            .complete(&request, &token)
            .await
            .expect_err("must refuse");
        assert!(
            matches!(err, ProviderError::CapabilityNotHonored { .. }),
            "{err:?}"
        );
        drop(token);
        assert_eq!(
            ledger.calls().expect("calls")[0].settlement,
            Settlement::Failed
        );
    }

    #[test]
    fn a_missing_credential_raises_and_names_the_variable() {
        let kimi = entry("kimi").expect("kimi");
        let err = credential_from(kimi, |_| None).expect_err("must raise");
        let message = err.to_string();
        assert!(message.contains("KGRAF_KIMI_API_KEY"), "{message}");
        let blank = credential_from(kimi, |_| Some("  ".into())).expect_err("blank must raise");
        assert!(matches!(blank, FleetError::CredentialMissing { .. }));
    }

    #[test]
    fn each_kimi_route_reads_its_own_variable() {
        let code = entry("kimi-code").expect("kimi-code");
        assert_eq!(credential_env_var(code), "KGRAF_KIMI_CODE_API_KEY");
        let got = credential_from(code, |name| {
            (name == "KGRAF_KIMI_CODE_API_KEY").then(|| "sk-kimi-test".to_owned())
        })
        .expect("resolved");
        assert_eq!(got, "sk-kimi-test");
    }

    #[test]
    fn a_local_route_needs_no_credential() {
        let ollama = entry("ollama").expect("ollama");
        assert_eq!(credential_from(ollama, |_| None).expect("ok"), "");
    }

    #[test]
    fn both_kimi_platform_routes_read_one_variable_and_the_subscription_route_its_own() {
        let platform = credential_env_var(entry("kimi").expect("kimi"));
        let anthropic_route = credential_env_var(entry("kimi-anthropic").expect("route"));
        let code = credential_env_var(entry("kimi-code").expect("kimi-code"));
        assert_eq!(platform, "KGRAF_KIMI_API_KEY");
        assert_eq!(anthropic_route, platform);
        assert_ne!(code, platform);
    }

    #[test]
    fn the_bedrock_route_is_unverified_and_takes_aws_credentials_not_one_key() {
        let bedrock = entry("bedrock").expect("bedrock");
        assert_eq!(bedrock.confidence, Confidence::Unverified);
        assert_eq!(bedrock.wire.as_str(), "bedrock-converse");
        assert_eq!(bedrock.protocol, Protocol::Additive);
        assert!(bedrock.requires_credential, "a hosted route is not local");
        assert!(bedrock.base_url.contains("{region}"));

        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let resolved = credential_from(
            bedrock,
            env(&[
                ("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE"),
                ("AWS_SECRET_ACCESS_KEY", "the-secret-access-key"),
                ("AWS_REGION", "us-east-1"),
            ]),
        )
        .expect("resolved");
        assert_eq!(resolved, "the-secret-access-key");

        // No region: unusable, and the reason names what to set.
        let err = credential_from(
            bedrock,
            env(&[
                ("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE"),
                ("AWS_SECRET_ACCESS_KEY", "the-secret-access-key"),
                ("HOME", "/nonexistent-home-for-test"),
            ]),
        )
        .expect_err("no region");
        assert!(
            matches!(err, FleetError::CredentialUnusable { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("AWS_REGION"), "{err}");

        let err = credential_from(bedrock, env(&[("HOME", "/nonexistent-home-for-test")]))
            .expect_err("no keys");
        assert!(err.to_string().contains("AWS_ACCESS_KEY_ID"), "{err}");
        assert!(
            err.to_string().contains("AWS_BEARER_TOKEN_BEDROCK"),
            "{err}"
        );

        // A Bedrock API key alone is a usable credential (with a region).
        let key = credential_from(
            bedrock,
            env(&[
                ("AWS_BEARER_TOKEN_BEDROCK", "the-bedrock-api-key"),
                ("AWS_REGION", "us-east-1"),
            ]),
        )
        .expect("api key resolved");
        assert_eq!(key, "the-bedrock-api-key");
    }

    #[test]
    fn a_bedrock_provider_built_with_a_lookup_reads_credentials_through_it() {
        // The process environment holds none of these values. The adapter registers
        // the secret it resolved with the scrubber when it is built, so a scrubbed
        // secret proves it was read through the caller's lookup and not the
        // environment.
        let secret = "fleet-lookup-secret-Zq8Xv3Lm7Wt2Kd9Rb5Ny1Hc4";
        let lookup = move |name: &str| match name {
            "AWS_ACCESS_KEY_ID" => Some("AKIAFLEETLOOKUP".to_owned()),
            "AWS_SECRET_ACCESS_KEY" => Some(secret.to_owned()),
            "AWS_REGION" => Some("us-east-1".to_owned()),
            _ => None,
        };
        let provider =
            build_with_lookup("bedrock", secret, vec!["model-a".into()], lookup).expect("built");
        assert_eq!(provider.adapter(), "bedrock-converse");
        assert_ne!(
            kgraf_secrets::scrub_persist(&format!("leak {secret}")),
            format!("leak {secret}"),
            "the secret the lookup returned was not registered"
        );
    }

    #[test]
    fn the_google_routes_read_the_variables_their_notes_name() {
        // The native route takes the key the compatibility route already reads.
        let gemini = entry("gemini").expect("gemini");
        assert_eq!(credential_env_var(gemini), "KGRAF_GOOGLE_API_KEY");
        // Both Vertex routes read one set of settings, not an API key.
        for id in ["vertex-gemini", "vertex-anthropic"] {
            let route = entry(id).expect(id);
            assert_eq!(credential_env_var(route), "KGRAF_VERTEX_CREDENTIALS");
            let missing = credential_from(route, |_| None).expect_err("must raise");
            assert!(
                matches!(&missing, FleetError::CredentialMissing { variable, .. }
                    if variable == "KGRAF_VERTEX_CREDENTIALS"),
                "{missing:?}"
            );
            let no_region = credential_from(route, |name| {
                (name == "KGRAF_VERTEX_CREDENTIALS" || name == "KGRAF_VERTEX_PROJECT")
                    .then(|| "adc".to_owned())
            })
            .expect_err("a region is required");
            assert!(
                matches!(&no_region, FleetError::CredentialMissing { variable, .. }
                    if variable == "KGRAF_VERTEX_REGION"),
                "{no_region:?}"
            );
            // An API key is not a Vertex credential and must not build one.
            assert!(build(id, "an-api-key", vec!["m".into()]).is_err());
        }
    }

    fn azure_env<'a>(overrides: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            if let Some((_, value)) = overrides.iter().find(|(k, _)| *k == name) {
                return (!value.is_empty()).then(|| (*value).to_owned());
            }
            match name {
                "KGRAF_AZURE_OPENAI_API_KEY" => Some("an-azure-key-value".to_owned()),
                "KGRAF_AZURE_OPENAI_RESOURCE" => Some("a-resource".to_owned()),
                "KGRAF_AZURE_OPENAI_API_VERSION" => Some("v1".to_owned()),
                "KGRAF_AZURE_OPENAI_DEPLOYMENTS" => Some("dep=some-model".to_owned()),
                _ => None,
            }
        }
    }

    #[test]
    fn the_azure_routes_are_unverified_and_read_the_variables_their_notes_name() {
        for id in ["azure-openai", "azure-openai-responses"] {
            let route = entry(id).expect(id);
            assert_eq!(route.confidence, Confidence::Unverified);
            assert_eq!(credential_env_var(route), "KGRAF_AZURE_OPENAI_API_KEY");
            assert_eq!(route.protocol, Protocol::Inclusive);
            for variable in [
                "KGRAF_AZURE_OPENAI_API_KEY",
                "KGRAF_AZURE_OPENAI_RESOURCE",
                "KGRAF_AZURE_OPENAI_API_VERSION",
                "KGRAF_AZURE_OPENAI_DEPLOYMENTS",
            ] {
                let missing = credential_from(route, azure_env(&[(variable, "")]))
                    .expect_err("a missing setting must raise");
                assert!(
                    matches!(&missing, FleetError::CredentialMissing { variable: v, .. } if v == variable),
                    "{id}: {missing:?}"
                );
            }
        }
    }

    #[test]
    fn an_azure_resource_or_version_that_could_steer_a_request_is_refused_at_the_credential_step() {
        let route = entry("azure-openai").expect("route");
        for (variable, value) in [
            ("KGRAF_AZURE_OPENAI_RESOURCE", "evil.example/path"),
            ("KGRAF_AZURE_OPENAI_RESOURCE", "res.openai.azure.com"),
            ("KGRAF_AZURE_OPENAI_API_VERSION", "2024-10-21&x=1"),
            ("KGRAF_AZURE_OPENAI_API_VERSION", "latest"),
            ("KGRAF_AZURE_OPENAI_DEPLOYMENTS", "no-equals"),
        ] {
            let err =
                credential_from(route, azure_env(&[(variable, value)])).expect_err("must raise");
            match &err {
                FleetError::CredentialUnusable { reason, .. } => {
                    assert!(reason.contains(variable), "{reason}");
                    assert!(
                        !reason.contains("an-azure-key-value"),
                        "the key must not leak"
                    );
                }
                other => panic!("wrong error: {other:?}"),
            }
        }
    }

    #[test]
    fn the_azure_routes_are_configured_with_the_resource_host_and_the_key() {
        for id in ["azure-openai", "azure-openai-responses"] {
            let route = entry(id).expect(id);
            let credential = credential_from(route, azure_env(&[])).expect("credential");
            let config = configure(route, &credential, vec!["dep".into()]).expect("config");
            assert_eq!(config.base_url, "https://a-resource.openai.azure.com");
            assert_eq!(config.api_key, "an-azure-key-value");
            assert_eq!(config.protocol, Protocol::Inclusive);
            let provider = build(id, &credential, vec!["dep".into()]).expect("builds");
            assert_eq!(provider.adapter(), route.wire.as_str());
        }
    }

    #[test]
    fn an_azure_deployment_without_a_mapped_model_does_not_build() {
        for id in ["azure-openai", "azure-openai-responses"] {
            let route = entry(id).expect(id);
            let credential = credential_from(route, azure_env(&[])).expect("credential");
            match build(id, &credential, vec!["dep".into(), "unmapped".into()]) {
                Err(FleetError::Provider {
                    source: ProviderError::ModelUnsupported { model, .. },
                    ..
                }) => assert_eq!(model, "unmapped"),
                Err(other) => panic!("wrong error: {other:?}"),
                Ok(_) => panic!("{id}: an unmapped deployment must not build"),
            }
        }
    }

    #[test]
    fn the_vertex_routes_are_configured_for_their_publisher_and_protocol() {
        let credential = test_credential(entry("vertex-gemini").expect("route"));
        let gemini = configure(
            entry("vertex-gemini").expect("route"),
            &credential,
            vec!["m".into()],
        )
        .expect("config");
        assert_eq!(
            gemini.base_url,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/a-project/locations/us-central1/publishers/google"
        );
        assert_eq!(gemini.protocol, Protocol::Inclusive);
        let claude = configure(
            entry("vertex-anthropic").expect("route"),
            &credential,
            vec!["m".into()],
        )
        .expect("config");
        assert!(claude.base_url.ends_with("/publishers/anthropic"));
        assert_eq!(claude.protocol, Protocol::Additive);
    }

    /// A PEM-shaped key of `lines` random base64 lines.
    fn fake_pem(seed: u64, lines: usize) -> String {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut state = seed;
        let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
        for _ in 0..lines {
            for _ in 0..64 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                pem.push(alphabet[(state % 64) as usize] as char);
            }
            pem.push('\n');
        }
        pem.push_str("-----END PRIVATE KEY-----\n");
        pem
    }

    /// A worker that prints the key file, the key, or a few lines of it, must
    /// find them redacted. The key is several lines, and the file writes its
    /// line breaks as `\n`, so the scrubber has to know both spellings.
    #[test]
    fn a_vertex_key_file_is_registered_with_the_scrubber_in_every_form_it_can_be_printed() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("sa.json");
        let pem = fake_pem(0x1234_5678_9abc_def1, 4);
        let key_id = "7f3a9c1d5e8b2a4c6d0e1f3a5b7c9d2e4f6a8b0c";
        let file = serde_json::json!({
            "type": "service_account",
            "project_id": "a-project",
            "private_key_id": key_id,
            "private_key": pem,
            "client_email": "sa@a-project.iam.gserviceaccount.com",
        })
        .to_string();
        std::fs::write(&path, &file).expect("write");
        let credential = VertexSettings::new(
            kgraf_provider_google::CredentialSource::ServiceAccountFile(path),
            "a-project",
            "us-central1",
        )
        .expect("settings")
        .to_credential();
        // The key is not a valid RSA key, so the build fails; the values are
        // registered before it does.
        let _ = build_with_lookup("vertex-gemini", &credential, vec!["m".into()], |_| None);

        let scrubbed = kgraf_secrets::scrub_persist(&file);
        assert!(!scrubbed.contains(key_id), "the key id was printed");
        for line in pem.lines().filter(|l| !l.starts_with("-----")) {
            assert!(!scrubbed.contains(line), "a key line survived: the file");
        }
        // The decoded key with real line breaks, as a tool reading the JSON
        // field would print it.
        let decoded = kgraf_secrets::scrub_persist(pem.trim());
        for line in pem.lines().filter(|l| !l.starts_with("-----")) {
            assert!(!decoded.contains(line), "a key line survived: decoded");
        }
        // One body line on its own, as `head` would show.
        let one = pem.lines().nth(2).expect("a body line");
        assert!(!kgraf_secrets::scrub_persist(one).contains(one));
        // Ordinary fields of the file are left alone.
        assert!(scrubbed.contains("a-project"));
    }

    /// The settings every placeholder route resolves from.
    fn placeholder_env(name: &str) -> Option<String> {
        match name {
            "KGRAF_VERTEX_CREDENTIALS" => Some("adc".to_owned()),
            "KGRAF_VERTEX_PROJECT" => Some("a-project".to_owned()),
            "KGRAF_VERTEX_REGION" => Some("us-central1".to_owned()),
            "KGRAF_AZURE_OPENAI_API_KEY" => Some("an-azure-key-value".to_owned()),
            "KGRAF_AZURE_OPENAI_RESOURCE" => Some("a-resource".to_owned()),
            "KGRAF_AZURE_OPENAI_API_VERSION" => Some("v1".to_owned()),
            "KGRAF_AZURE_OPENAI_DEPLOYMENTS" => Some("dep=some-model".to_owned()),
            "AWS_ACCESS_KEY_ID" => Some("AKIDEXAMPLE".to_owned()),
            "AWS_SECRET_ACCESS_KEY" => Some("a-secret-access-key".to_owned()),
            "AWS_REGION" => Some("us-west-2".to_owned()),
            _ => None,
        }
    }

    #[test]
    fn a_limit_key_is_the_url_an_adapter_records_under_for_every_placeholder_route() {
        let mut checked = 0;
        for route in CATALOG.iter().filter(|e| e.base_url.contains('{')) {
            checked += 1;
            let key = limit_key(route, placeholder_env, "dep").expect(route.id);
            let credential = credential_from(route, placeholder_env).expect(route.id);
            let recorded = if route.wire.is_azure() {
                // The adapters key an Azure route by resource origin and deployment.
                let config = configure(route, &credential, vec!["dep".into()]).expect(route.id);
                kgraf_provider::limits::scoped_key(&config.base_url, "dep")
            } else {
                configure(route, &credential, vec!["m".into()])
                    .expect(route.id)
                    .base_url
            };
            assert_eq!(key, recorded, "{}", route.id);
            if route.wire.is_vertex() || route.wire.is_azure() {
                assert!(!key.contains('{'), "{}: {key}", route.id);
                assert_ne!(key, route.base_url, "{}", route.id);
            } else {
                // Bedrock keeps `{region}` in the URL its adapter holds: one region
                // per process, so the configured URL is a stable key.
                assert_eq!(route.wire, Wire::BedrockConverse, "{}", route.id);
            }
        }
        assert!(checked >= 5, "azure x2, bedrock, vertex x2: {checked}");
    }

    #[test]
    fn two_azure_deployments_on_one_resource_have_two_limit_keys() {
        let route = entry("azure-openai").expect("route");
        let a = limit_key(route, placeholder_env, "dep-a").expect("a");
        let b = limit_key(route, placeholder_env, "dep-b").expect("b");
        assert_ne!(a, b);
        assert!(a.starts_with("https://a-resource.openai.azure.com"));
    }

    #[test]
    fn a_route_with_no_placeholder_is_keyed_by_its_catalog_url_and_needs_no_settings() {
        let none = |_: &str| None;
        for route in CATALOG.iter().filter(|e| !e.base_url.contains('{')) {
            assert_eq!(
                limit_key(route, none, "m").expect(route.id),
                route.base_url,
                "{}",
                route.id
            );
        }
    }

    #[test]
    fn a_placeholder_route_with_unresolved_settings_has_no_key() {
        let none = |_: &str| None;
        for id in ["azure-openai", "vertex-gemini"] {
            assert!(limit_key(entry(id).expect(id), none, "m").is_err(), "{id}");
        }
    }

    #[test]
    fn the_bedrock_limit_key_reads_no_credential_and_no_setting() {
        let touched = std::cell::Cell::new(false);
        let lookup = |_: &str| {
            touched.set(true);
            None
        };
        let key = limit_key(entry("bedrock").expect("bedrock"), lookup, "m").expect("key");
        assert_eq!(key, "https://bedrock-runtime.{region}.amazonaws.com");
        assert!(!touched.get(), "the key lookup read a setting");
    }

    #[test]
    fn azure_routes_are_priced_only_under_route_scoped_keys() {
        let ids: Vec<_> = route_scoped_price_routes().collect();
        assert_eq!(ids, ["azure-openai", "azure-openai-responses"]);
        assert!(route_scoped_price_only("azure-openai"));
        assert!(!route_scoped_price_only("openai"));
        assert!(!route_scoped_price_only("nonexistent"));
        let deployments = azure_deployments(entry("azure-openai").expect("route"), placeholder_env)
            .expect("deployments");
        assert_eq!(deployments, [("dep".to_owned(), "some-model".to_owned())]);
        assert!(azure_deployments(entry("openai").expect("route"), placeholder_env).is_none());
        assert_eq!(
            azure_litellm_key("gpt-4o-2024-08-06"),
            "azure/gpt-4o-2024-08-06"
        );
    }

    #[test]
    fn the_azure_responses_route_refuses_a_dated_version_at_the_credential_step() {
        let dated = |name: &str| match name {
            "KGRAF_AZURE_OPENAI_API_VERSION" => Some("2025-04-01-preview".to_owned()),
            other => placeholder_env(other),
        };
        let responses = entry("azure-openai-responses").expect("route");
        let err = credential_from(responses, dated).expect_err("dated responses");
        assert!(
            matches!(&err, FleetError::CredentialUnusable { reason, .. }
                if reason.contains("/openai/v1/responses")),
            "{err:?}"
        );
        credential_from(entry("azure-openai").expect("route"), dated).expect("chat takes it");
        let old = |name: &str| match name {
            "KGRAF_AZURE_OPENAI_API_VERSION" => Some("2024-06-01".to_owned()),
            other => placeholder_env(other),
        };
        assert!(credential_from(entry("azure-openai").expect("route"), old).is_err());
    }

    #[test]
    fn a_shared_credential_names_a_route_that_exists() {
        for route in CATALOG {
            if let Some(other) = route.shares_credential_with {
                entry(other).unwrap_or_else(|e| panic!("{}: {e}", route.id));
            }
        }
    }

    #[test]
    fn a_route_can_select_the_additive_reasoning_dialect_and_the_default_is_inclusive() {
        for route in CATALOG {
            assert_eq!(route.reasoning_dialect, ReasoningDialect::Inclusive);
        }
        let chat = *CATALOG
            .iter()
            .find(|e| matches!(e.wire, Wire::OpenAiChat(_)))
            .expect("a chat route");
        let Wire::OpenAiChat(dialect) = chat.wire else {
            unreachable!()
        };
        let config = || configure(&chat, "placeholder-key-value", vec!["m".to_owned()]);
        let default = chat_adapter(&chat, config().expect("config"), dialect).expect("adapter");
        assert_eq!(default.reasoning_dialect(), ReasoningDialect::Inclusive);
        let additive = Entry {
            reasoning_dialect: ReasoningDialect::Additive,
            ..chat
        };
        let adapter = chat_adapter(&additive, config().expect("config"), dialect).expect("adapter");
        assert_eq!(adapter.reasoning_dialect(), ReasoningDialect::Additive);

        let responses = *CATALOG
            .iter()
            .find(|e| e.wire == Wire::OpenAiResponses)
            .expect("a responses route");
        let additive = Entry {
            reasoning_dialect: ReasoningDialect::Additive,
            ..responses
        };
        let config = configure(&additive, "placeholder-key-value", vec!["m".to_owned()]);
        let adapter = responses_adapter(&additive, config.expect("config")).expect("adapter");
        assert_eq!(adapter.reasoning_dialect(), ReasoningDialect::Additive);
    }

    #[test]
    fn a_litellm_provider_is_named_only_for_routes_in_the_catalog() {
        assert_eq!(litellm_provider("kimi"), Some("moonshot"));
        assert_eq!(litellm_provider("kimi-anthropic"), Some("moonshot"));
        assert_eq!(litellm_provider("kimi-code"), None);
        assert_eq!(litellm_provider("nonexistent"), None);
        for id in ["kimi", "kimi-anthropic"] {
            entry(id).expect("a mapped route exists in the catalog");
        }
    }

    #[test]
    fn every_anthropic_protocol_root_ends_in_the_version_segment() {
        // The adapter appends `/messages`; a root without `/v1` posts to a path the
        // protocol does not define.
        for route in CATALOG.iter().filter(|e| e.wire == Wire::AnthropicMessages) {
            assert!(
                route.base_url.ends_with("/v1"),
                "{}: {}",
                route.id,
                route.base_url
            );
        }
    }

    /// A local route moves with its variable; a route that takes a credential
    /// never does, so the environment cannot send a key to another host.
    #[test]
    fn a_local_route_moves_by_variable_and_a_credentialed_one_does_not() {
        let ollama = entry("ollama").expect("ollama");
        assert_eq!(base_url_env_var(ollama), "KGRAF_OLLAMA_BASE_URL");
        assert_eq!(
            local_endpoint_from(ollama, |_| None).expect("default"),
            "http://localhost:11434/v1"
        );
        let moved = |name: &str| {
            (name == "KGRAF_OLLAMA_BASE_URL").then(|| " http://127.0.0.1:9/v1 ".to_owned())
        };
        assert_eq!(
            local_endpoint_from(ollama, moved).expect("moved"),
            "http://127.0.0.1:9/v1"
        );
        // The configuration the adapter holds is at the moved address.
        let config = ProviderConfig::local(
            local_endpoint_from(ollama, moved).expect("moved"),
            vec![],
            ollama.protocol,
        );
        assert_eq!(config.base_url, "http://127.0.0.1:9/v1");
        let openai = entry("openai").expect("openai");
        let any = |_: &str| Some("http://127.0.0.1:9/v1".to_owned());
        assert_eq!(
            local_endpoint_from(openai, any).expect("not moved"),
            openai.base_url
        );
    }

    #[test]
    fn a_base_url_override_that_is_not_a_url_raises() {
        let ollama = entry("ollama").expect("ollama");
        for bad in [
            "localhost:11434",
            "ftp://host/v1",
            "http://",
            "http:///v1",
            "",
        ] {
            let error = local_endpoint_from(ollama, |_| Some(bad.to_owned()))
                .expect_err("a value that is not http(s) must raise");
            assert!(
                matches!(error, FleetError::BadBaseUrl { .. }),
                "{bad}: {error}"
            );
            assert!(
                error.to_string().contains("KGRAF_OLLAMA_BASE_URL"),
                "{error}"
            );
        }
    }
}
