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

use kgraf_core::Protocol;
use kgraf_core::session::ReasoningVisibility;
use kgraf_provider::{Provider, ProviderConfig, ProviderError};
use kgraf_provider_anthropic::AnthropicAdapter;
use kgraf_provider_openai::{
    ChatCompletionsAdapter, OpenAiAdapter, ReasoningDialect, StreamedUsageLocation, UsageDialect,
};

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
}

impl Wire {
    /// The wire's stable name, for display and configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Wire::AnthropicMessages => "anthropic-messages",
            Wire::OpenAiResponses => "openai-responses",
            Wire::OpenAiChat(_) => "openai-chat-completions",
        }
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
    /// No catalog entry with that id.
    #[error("unknown provider `{id}`; known: {known}")]
    Unknown { id: String, known: String },
    /// A route that needs a credential has none configured.
    #[error(
        "no credential for `{id}`: set {variable} in your own shell (never paste a key into a chat or a file you commit)"
    )]
    CredentialMissing { id: String, variable: String },
    /// The adapter refused the configuration.
    #[error("cannot configure `{id}`")]
    Provider {
        id: String,
        #[source]
        source: ProviderError,
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
        confidence: Confidence::Documented,
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
        confidence: Confidence::Documented,
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
               cache writes reported in prompt details; the cache-read field has not \
               yet been seen on a warm call, so the usage shape is not fully verified. \
               A live streamed call on 2026-09-26 confirmed usage arrives inside \
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
        confidence: Confidence::Verified,
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
        confidence: Confidence::Documented,
        streamed_usage: Some((StreamedUsageLocation::TopLevelChunk, Confidence::Unverified)),
        reasoning_visibility: ReasoningVisibility::None,
        reasoning_dialect: ReasoningDialect::Inclusive,
        note: "Self-hosted, no credential. Reports prompt and completion tokens with \
               no cache split, which is correct rather than missing: there is no \
               server-side prompt cache to report. Cost derives to zero only if the \
               price table says so, which for a local model it should.",
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
    let id = entry.shares_credential_with.unwrap_or(entry.id);
    format!(
        "KGRAF_{}_API_KEY",
        id.to_ascii_uppercase().replace('-', "_")
    )
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
        ProviderConfig::local(entry.base_url, models, entry.protocol)
    };
    if entry.honors_cache_control {
        config = config.honoring_cache_control();
    }
    Ok(config)
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
    let entry = entry(id)?;
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
    kgraf_secrets::register_exact_secret("provider-api-key", credential);
    let config = configure(entry, credential, models)?;
    let wrap = |source: ProviderError| FleetError::Provider {
        id: entry.id.to_owned(),
        source,
    };
    Ok(match entry.wire {
        Wire::AnthropicMessages => Box::new(AnthropicAdapter::new(config).map_err(wrap)?),
        Wire::OpenAiResponses => Box::new(responses_adapter(entry, config).map_err(wrap)?),
        Wire::OpenAiChat(dialect) => Box::new(chat_adapter(entry, config, dialect).map_err(wrap)?),
    })
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
        // additive entry on either would undercount every cached request.
        for entry in CATALOG {
            match entry.wire {
                Wire::AnthropicMessages => {}
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
                Wire::OpenAiChat(_) => assert!(
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

    #[test]
    fn every_entry_builds_and_reports_its_own_protocol() {
        for entry in CATALOG {
            let provider = build(entry.id, "a-credential", vec!["a-model".into()])
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
        for entry in CATALOG {
            let provider = build(entry.id, "a-credential", vec!["a-model".into()])
                .unwrap_or_else(|e| panic!("{}: {e}", entry.id));
            let declared = provider.capabilities().reasoning.is_some();
            // The Anthropic wire bills reasoning inside output and never reports it
            // separately, so it declares nothing regardless of the catalog row.
            let expected =
                entry.reports_reasoning_tokens && !matches!(entry.wire, Wire::AnthropicMessages);
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
}
