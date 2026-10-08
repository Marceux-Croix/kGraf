//! The **Chat Completions** adapter: the third-party compatible ecosystem.
//!
//! This is the surface almost every "OpenAI-compatible" provider actually serves,
//! z.ai, DeepSeek, MiniMax, Together, Groq, OpenRouter's OpenAI route, vLLM,
//! llama.cpp. None of them implement `/responses`, so without this adapter none of
//! them is reachable, whatever decision 0006 says about preferring Responses for
//! OpenAI's own models (decision 0041).
//!
//! Two things make this more than a path change from [`crate::responses`].
//!
//! **The usage object has more than one dialect**, and decision 0006 already
//! required at least three usage adapters across the whole fleet. Chat Completions
//! carries two of them, and they are not distinguishable from the endpoint URL, so
//! the dialect is configured per worker like the protocol itself. See
//! [`UsageDialect`].
//!
//! **Compatibility is advertised and then not delivered.** The project's endpoint
//! research found three confirmed instances of a provider accepting a field and
//! silently ignoring it, all returning HTTP 200, an unknown model remapped to a
//! cheap one, unsupported parameters dropped, stop sequences ignored. So this
//! adapter asserts the model echo like every other, declares **no** cache-control
//! capability by default, and keeps the raw usage blob verbatim because for several
//! of these vendors the usage shape is formally unverified.

use futures_util::StreamExt;
use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::capability::{Capabilities, Reasoning, StreamEvent, Streaming};
use kgraf_provider::{
    CallToken, Content, ProviderConfig, ProviderError, Request, Response, Role, StopReason,
    StreamGuard,
};

/// Rate-limit header capture (decision 0067). Recorded whether or
/// not the call succeeded: a 429's own headers are exactly what the
/// per-machine capacity's learned-rate backoff needs. Keyed by base URL, not
/// by the route name this adapter does not know
/// (`kgraf_provider::limits`'s own module doc explains why).
fn observe_rate_limit_headers(
    key: &str,
    parse: impl Fn(&kgraf_provider::limits::HeaderGet) -> kgraf_provider::limits::ObservedLimits,
    http: &reqwest::Response,
) {
    let headers = http.headers().clone();
    kgraf_provider::limits::record(
        key,
        parse(&move |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        }),
    );
    // Decision 0067: a 429 feeds the
    // per-machine capacity's learned rate (`kgraf_capacity::LearnedRate`),
    // read back by the shell's re-measure thread.
    if http.status().as_u16() == 429 {
        kgraf_provider::limits::record_429_now(key);
    }
}

/// The adapter's stable name.
pub const ADAPTER: &str = "openai-chat-completions";

/// How a Chat Completions endpoint reports its cached-input split.
///
/// Both dialects are **inclusive**: the reported prompt total already contains the
/// cached portion. They differ in how the cached portion is named and whether the
/// uncached remainder is stated or must be derived.
///
/// This cannot be inferred from the base URL, and guessing wrong does not fail
/// loudly, it silently attributes cached tokens to the uncached bucket, which is a
/// cost error of roughly 10x at current cache discounts. So it is configured, per
/// decision 0006's rule that the accounting keys on the protocol rather than on a
/// vendor name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDialect {
    /// OpenAI's own shape, and the one most compatible vendors copy:
    /// `prompt_tokens_details.cached_tokens` and
    /// `completion_tokens_details.reasoning_tokens`, both optional.
    ///
    /// A missing details object is read as zero cached tokens, which attributes the
    /// whole prompt to uncached input. That **overstates** cost rather than
    /// understating it, which is the right direction to be wrong in for a figure a
    /// user budgets against, and the raw blob is retained either way, so a vendor
    /// later found to report caching differently can be re-derived rather than
    /// re-run.
    Details,
    /// The hit-and-miss shape, where the split is stated as two sibling fields
    /// summing to the prompt total: `prompt_cache_hit_tokens` and
    /// `prompt_cache_miss_tokens`.
    ///
    /// Decision 0006 names this as a required third usage adapter. Because the miss
    /// count is stated rather than derived, this dialect is the only one that can
    /// detect its own inconsistency, and it does: a split that does not sum to the
    /// prompt total raises rather than being silently reconciled.
    HitAndMiss,
}

impl UsageDialect {
    /// The dialect's stable name, for configuration and display.
    pub fn as_str(self) -> &'static str {
        match self {
            UsageDialect::Details => "details",
            UsageDialect::HitAndMiss => "hit_and_miss",
        }
    }
}

/// Whether a Chat Completions endpoint reports reasoning tokens as part of
/// `completion_tokens`, or as an extra count alongside it.
///
/// OpenAI's own shape is inclusive: `completion_tokens` already contains the
/// reasoning share, so text output is the remainder after subtracting it.
/// Reading an additive vendor under the inclusive assumption, or the reverse,
/// either double-counts or under-counts text output, so this is a stated
/// dialect rather than a fixed subtraction, matching how [`UsageDialect`]
/// already treats the cache split as configured rather than guessed. Every
/// vendor surveyed so far reports the inclusive shape; [`Self::Additive`]
/// exists for the day one does not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningDialect {
    /// `completion_tokens` includes the reasoning share; text output is
    /// `completion_tokens - reasoning_tokens`. Raises rather than clamps when
    /// the reported reasoning count exceeds the completion total, since that
    /// is arithmetically impossible under this dialect and means either the
    /// dialect is misconfigured or the vendor's usage object is inconsistent.
    #[default]
    Inclusive,
    /// `completion_tokens` and `reasoning_tokens` are independent counts;
    /// text output is `completion_tokens` unchanged.
    Additive,
}

/// Where a Chat Completions endpoint reports usage inside a **streamed**
/// response, if anywhere.
///
/// Distinct from [`UsageDialect`], which is about the field names inside a
/// usage object; this is about which chunk carries that object at all. An
/// Testing found this codebase assumed the top-level shape
/// unconditionally and would have silently accepted a stream with no usage
/// at all as `Counted`, with zero usage, from a route that never sends any.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamedUsageLocation {
    /// In the top-level chunk's own `usage` field, alongside an empty
    /// `choices` array, once `stream_options.include_usage` is set. OpenAI's
    /// own documented shape, and the one this adapter defaults to.
    TopLevelChunk,
    /// Inside `choices[0].usage`, typically on the same chunk that carries
    /// `finish_reason`. At least one vendor in this family has been reported
    /// to use this shape instead of, or in addition to, the top-level one.
    InChoice,
    /// This endpoint sends no usage at all in streaming mode. A route
    /// declared this refuses to stream at all (fail loud): paying for a call
    /// whose usage can never be attributed, then discovering that after the
    /// fact, is worse than refusing before the request leaves.
    None,
}

impl StreamedUsageLocation {
    /// The location's stable name.
    pub fn as_str(self) -> &'static str {
        match self {
            StreamedUsageLocation::TopLevelChunk => "top_level_chunk",
            StreamedUsageLocation::InChoice => "in_choice",
            StreamedUsageLocation::None => "none",
        }
    }
}

/// Whether a JSON value is a usage object worth normalizing, as opposed to
/// an absent or empty placeholder.
///
/// Some vendors send `"usage":{}` on an intermediate chunk. This project's
/// research found this reported for at least one vendor's mid-stream chunks.
/// It must not be read the same as a real, if partial, usage object:
/// `{}` has neither `prompt_tokens` nor `completion_tokens`, so passing it to
/// [`ChatCompletionsAdapter::normalize_usage`] raises `UsageMissing` and
/// would fail an otherwise healthy stream over a placeholder.
fn is_meaningful_usage(value: &serde_json::Value) -> bool {
    match value.as_object() {
        Some(object) => !object.is_empty(),
        None => !value.is_null(),
    }
}

/// The cache-related fields, by dotted path from the usage object, that the
/// Chat Completions adapter reads under the details dialect. Anything else at
/// any other path whose name contains "cache" is a field this adapter was not
/// written against. Recognition is by path, not by bare name: a
/// `cached_tokens` at a place nothing reads would otherwise pass unread.
///
/// The top-level `cached_tokens` is Moonshot's documented shape. It is read
/// as a fallback when `prompt_tokens_details.cached_tokens` is absent.
pub(crate) const CHAT_DETAILS_CACHE_PATHS: &[&str] = &[
    "cached_tokens",
    "prompt_tokens_details.cached_tokens",
    "prompt_tokens_details.cache_write_tokens",
];

/// The same, under the hit-and-miss dialect. `prompt_tokens_details.
/// cached_tokens` is recognized because vendors of this dialect repeat the hit
/// count there; it equals `prompt_cache_hit_tokens` and is not read twice.
pub(crate) const CHAT_HIT_AND_MISS_CACHE_PATHS: &[&str] = &[
    "prompt_cache_hit_tokens",
    "prompt_cache_miss_tokens",
    "prompt_tokens_details.cached_tokens",
];

/// Whether a JSON value is, or contains, a nonzero number. Null, strings and
/// booleans carry no count and are zero-equivalent.
fn has_nonzero_number(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        serde_json::Value::Object(map) => map.values().any(has_nonzero_number),
        serde_json::Value::Array(items) => items.iter().any(has_nonzero_number),
        _ => false,
    }
}

/// Raise on a usage object carrying a cache-named field neither adapter
/// recognizes, if it reports a nonzero amount.
///
/// Both adapters read the cache split by fixed field name and treat an absent
/// field as zero, which is correct for a field a vendor simply never reports.
/// It is not correct for a field the vendor reports under a *different* name
/// this codebase has never seen: that field's tokens would silently vanish
/// from the cache accounting into nothing, understating cost with no signal
/// that anything was missed. A vendor renaming or adding a cache field is
/// exactly the kind of schema drift the raw-blob-retention design in
/// `kgraf_core::TokenUsage` exists to catch, so this raises rather than
/// staying quiet (fail loud). Scans one level of nesting, which is as deep as
/// every usage object surveyed goes.
pub(crate) fn warn_on_unrecognized_cache_key(
    adapter: &'static str,
    raw: &serde_json::Value,
    recognized_paths: &[&str],
) -> Result<(), ProviderError> {
    fn scan(
        adapter: &'static str,
        value: &serde_json::Value,
        path: &str,
        recognized_paths: &[&str],
    ) -> Result<(), ProviderError> {
        let Some(object) = value.as_object() else {
            return Ok(());
        };
        for (key, field_value) in object {
            let full_path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            let is_cache_named = key.to_ascii_lowercase().contains("cache");
            let is_recognized = recognized_paths.contains(&full_path.as_str());
            if is_cache_named && !is_recognized {
                // A null, a string or a boolean carries no count and reads as
                // zero. A number, or an object holding one, must be nonzero
                // to raise.
                let nonzero = has_nonzero_number(field_value);
                if nonzero {
                    return Err(ProviderError::Decode {
                        adapter,
                        reason: format!(
                            "usage object has an unrecognized cache-related field `{full_path}` \
                             with value {field_value}; treating it as absent would silently \
                             drop those tokens from cache accounting"
                        ),
                    });
                }
            }
            scan(adapter, field_value, &full_path, recognized_paths)?;
        }
        Ok(())
    }
    scan(adapter, raw, "", recognized_paths)
}

/// Reconcile the parts of a usage object with the vendor's own `total_tokens`,
/// when it sends one, and return the billed tokens the parts leave out.
///
/// A vendor may bill tokens it does not put in the counted fields: a reasoning
/// share left out of `completion_tokens` shows up only as a total larger than
/// the parts (Gemini's thinking tokens sit outside its candidate count). That is
/// a normal response, so the difference is returned for the caller to record as
/// reasoning output, which is billed at the output rate. A total smaller than
/// the parts cannot occur in a correct response and raises, leaving the call
/// unreconciled. Under the additive reasoning dialect the reasoning count is
/// outside the output count, so it joins the sum.
pub(crate) fn unaccounted_total_tokens(
    adapter: &'static str,
    raw: &serde_json::Value,
    input: u64,
    output: u64,
    reasoning: u64,
    reasoning_dialect: ReasoningDialect,
) -> Result<u64, ProviderError> {
    let Some(total) = raw.get("total_tokens").and_then(|v| v.as_u64()) else {
        return Ok(0);
    };
    let parts = input
        + output
        + match reasoning_dialect {
            ReasoningDialect::Inclusive => 0,
            ReasoningDialect::Additive => reasoning,
        };
    if total < parts {
        return Err(ProviderError::Decode {
            adapter,
            reason: format!(
                "usage is inconsistent: input {input} plus output {output} \
                 (reasoning {reasoning}, {reasoning_dialect:?} dialect) is {parts} but \
                 `total_tokens` is {total}, below the counted fields"
            ),
        });
    }
    Ok(total - parts)
}

/// A lower bound for a usage object the strict parse refused: the counted
/// fields as reported, with no consistency check, and the raw object kept.
/// `None` only when the object carries neither a prompt nor a completion count.
/// A refused call keeps this on its open row as a floor, never as the spend.
pub(crate) fn lower_bound_usage(
    protocol: Protocol,
    dialect: UsageDialect,
    reasoning_dialect: ReasoningDialect,
    raw: &serde_json::Value,
) -> Option<TokenUsage> {
    let field = |name: &str| raw.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
    raw.get("prompt_tokens")
        .or_else(|| raw.get("completion_tokens"))?;
    let prompt = field("prompt_tokens");
    let completion = field("completion_tokens");
    let reasoning = raw
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let (cache_read, cache_write) = match dialect {
        UsageDialect::Details => (
            raw.get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .or_else(|| raw.get("cached_tokens").and_then(|v| v.as_u64()))
                .unwrap_or(0),
            raw.get("prompt_tokens_details")
                .and_then(|d| d.get("cache_write_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        ),
        UsageDialect::HitAndMiss => (field("prompt_cache_hit_tokens"), 0),
    };
    let uncached_input = match protocol {
        Protocol::Additive => prompt,
        Protocol::Inclusive => prompt.saturating_sub(cache_read.saturating_add(cache_write)),
    };
    let text_output = match reasoning_dialect {
        ReasoningDialect::Inclusive => completion.saturating_sub(reasoning),
        ReasoningDialect::Additive => completion,
    };
    Some(TokenUsage {
        uncached_input,
        cache_read,
        cache_write,
        text_output,
        reasoning_output: reasoning,
        unsplit_reasoning_chars: None,
        raw: Some(raw.clone()),
    })
}

fn is_tool_use(content: &Content) -> bool {
    matches!(content, Content::ToolUse { .. })
}

/// The chain-of-thought text a message or stream delta carries, if any.
///
/// Servers in this family disagree on the field name: DeepSeek, Kimi and
/// llama.cpp send `reasoning_content`; Ollama (recorded live, fixture
/// `tests/fixtures/2026-10-01-ollama-qwen3.6-chat-stream.sse`), OpenRouter and
/// recent vLLM send `reasoning`. Both are read. When both are present (a
/// server mid-rename may send the same text twice) `reasoning_content` wins,
/// so the text is never carried twice. Only a string is read: OpenRouter's
/// structured `reasoning_details` array is a different field.
fn reasoning_field(object: &serde_json::Value) -> Option<&str> {
    ["reasoning_content", "reasoning"].iter().find_map(|name| {
        object
            .get(*name)
            .and_then(|r| r.as_str())
            .filter(|r| !r.is_empty())
    })
}

/// Map a `finish_reason` onto kGraf's stop reasons.
///
/// A vendor may report `stop` on a turn that carries tool calls (Gemini's
/// compatible endpoint and older Ollama builds do). The presence of tool calls
/// is what decides the caller's next step, so `stop` with tool calls is
/// [`StopReason::ToolUse`]. The mapping is adapter-side: the core keeps one
/// meaning per stop reason.
fn stop_reason_for(reason: &str, has_tool_calls: bool) -> StopReason {
    match reason {
        "stop" if has_tool_calls => StopReason::ToolUse,
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        // Anything else, including a content filter or a vendor-specific
        // reason, is carried as `Other` rather than mapped onto `EndTurn`, so
        // an unmodeled terminal state is visible.
        _ => StopReason::Other,
    }
}

/// Record that a response carried reasoning text its usage object did not
/// count apart.
///
/// Ollama and llama.cpp return one `completion_tokens` total that includes the
/// reasoning and no `reasoning_tokens`. The reasoning is then inside text
/// output, and a `reasoning_output` of zero would read as "no reasoning". The
/// text length is kept so reports can say the split is unknown. No token count
/// is derived from it: no tokenizer for the served model is available here.
fn mark_unsplit_reasoning(usage: &mut TokenUsage, reasoning: Option<&str>) {
    if usage.reasoning_output > 0 {
        return;
    }
    if let Some(text) = reasoning.filter(|t| !t.is_empty()) {
        usage.unsplit_reasoning_chars = Some(text.chars().count() as u64);
    }
}

/// A Chat Completions client.
pub struct ChatCompletionsAdapter {
    config: ProviderConfig,
    http: reqwest::Client,
    dialect: UsageDialect,
    /// Whether this endpoint reports reasoning tokens separately from text output.
    ///
    /// Several compatible vendors emit reasoning *content* without ever reporting a
    /// reasoning *token count*. Declaring the capability in that case would claim an
    /// accounting split that does not exist, so it defaults to false.
    reports_reasoning_tokens: bool,
    /// Whether `completion_tokens` includes or excludes the reasoning share.
    /// Defaults to [`ReasoningDialect::Inclusive`], OpenAI's own shape.
    reasoning_dialect: ReasoningDialect,
    /// Where this endpoint reports usage in a streamed response.
    ///
    /// Defaults to [`StreamedUsageLocation::TopLevelChunk`], OpenAI's own
    /// documented shape. `stream()` reads usage from either the top-level
    /// chunk or `choices[0].usage` regardless of this value (a vendor that
    /// sends both, or the "wrong" one, still gets counted) so the only
    /// behavioral consequence of this field is refusing to stream at all
    /// when it is [`StreamedUsageLocation::None`] (fail loud).
    streamed_usage_location: StreamedUsageLocation,
    /// Azure OpenAI transport, when set: its URL, `api-key` header, model
    /// mapping and content-filter handling replace the defaults.
    azure: Option<crate::azure::AzureTransport>,
}

impl ChatCompletionsAdapter {
    /// Record the rate-limit headers of one response. Azure limits are per
    /// deployment, so its key names the deployment and its headers are read
    /// with the Azure rules; every other endpoint is keyed by its base URL.
    fn observe_limits(&self, model: &str, http: &reqwest::Response) {
        match &self.azure {
            Some(transport) => observe_rate_limit_headers(
                &transport.limit_key(model),
                kgraf_provider::limits::parse_azure_style,
                http,
            ),
            None => observe_rate_limit_headers(
                &self.config.base_url,
                kgraf_provider::limits::parse_openai_style,
                http,
            ),
        }
    }

    /// Build an adapter over a configured endpoint.
    ///
    /// The configured protocol is asserted, as in the Responses adapter: every Chat
    /// Completions variant surveyed is inclusive, and accepting an additive
    /// configuration here would undercount every cached request while returning
    /// HTTP 200 (fail loud).
    pub fn new(config: ProviderConfig, dialect: UsageDialect) -> Result<Self, ProviderError> {
        if config.protocol != Protocol::Inclusive {
            return Err(ProviderError::CapabilityNotHonored {
                adapter: ADAPTER,
                capability: "inclusive usage accounting",
            });
        }
        if config.honors_cache_control {
            // No Chat Completions variant has a breakpoint mechanism. Caching, where
            // it exists, is automatic and server-side. Accepting a configuration that
            // claims otherwise would let a caller believe breakpoints were being
            // placed when nothing is ever emitted.
            return Err(ProviderError::CapabilityNotHonored {
                adapter: ADAPTER,
                capability: "explicit cache breakpoints",
            });
        }
        // `connect_timeout`, not `.timeout()`: the latter bounds the whole
        // request including reading the entire response body, which for a
        // stream is the exact defect of an early
        // streaming adapter: a stream still healthily
        // producing tokens past `config.timeout` would be cut off mid-answer.
        // `complete` and `stream` each apply `config.timeout` at the call
        // site instead: a whole-call bound for `complete`, a per-chunk idle
        // bound for `stream`.
        let http = reqwest::Client::builder()
            .connect_timeout(config.timeout)
            .build()
            .map_err(|e| ProviderError::Transport {
                adapter: ADAPTER,
                reason: e.to_string(),
            })?;
        Ok(Self {
            config,
            http,
            dialect,
            reports_reasoning_tokens: false,
            reasoning_dialect: ReasoningDialect::Inclusive,
            streamed_usage_location: StreamedUsageLocation::TopLevelChunk,
            azure: None,
        })
    }

    /// Serve this adapter's body through Azure OpenAI.
    ///
    /// Every configured model must be a deployment with a configured underlying
    /// model, or construction fails: a response could otherwise be accepted with
    /// nothing to check its `model` against.
    pub fn with_azure(
        mut self,
        transport: crate::azure::AzureTransport,
    ) -> Result<Self, ProviderError> {
        transport.check_models(transport.name(), &self.config.models)?;
        self.azure = Some(transport);
        Ok(self)
    }

    /// The name reported to the ledger: the transport's when one is set.
    fn adapter_name(&self) -> &'static str {
        self.azure.as_ref().map_or(ADAPTER, |t| t.name())
    }

    /// Checks that run before a request leaves: the model is one this adapter
    /// serves, and a response for it has a model to be checked against.
    fn preflight(&self, model: &str) -> Result<(), ProviderError> {
        self.config.check_model(self.adapter_name(), model)?;
        self.echo_target(model).map(|_| ())
    }

    /// The model the response must name for a request to `requested`.
    fn echo_target<'a>(&'a self, requested: &'a str) -> Result<&'a str, ProviderError> {
        match &self.azure {
            Some(transport) => transport.underlying(self.adapter_name(), requested),
            None => Ok(requested),
        }
    }

    /// Declare where this endpoint puts usage in a streamed response.
    pub fn streaming_usage_from(mut self, location: StreamedUsageLocation) -> Self {
        self.streamed_usage_location = location;
        self
    }

    /// Declare that this endpoint reports reasoning tokens separately.
    pub fn reporting_reasoning_tokens(mut self) -> Self {
        self.reports_reasoning_tokens = true;
        self
    }

    /// Declare how this endpoint relates `completion_tokens` to
    /// `reasoning_tokens`. Defaults to [`ReasoningDialect::Inclusive`].
    pub fn with_reasoning_dialect(mut self, dialect: ReasoningDialect) -> Self {
        self.reasoning_dialect = dialect;
        self
    }

    /// The reasoning dialect this adapter reads usage under.
    pub fn reasoning_dialect(&self) -> ReasoningDialect {
        self.reasoning_dialect
    }

    /// The configuration this adapter was built with.
    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    /// The usage dialect this adapter reads.
    pub fn dialect(&self) -> UsageDialect {
        self.dialect
    }

    /// Issue the request and check the status, returning the raw response for
    /// the caller to read (buffered or incrementally) as it sees fit.
    ///
    /// The `send()` itself is bounded by `bound`: `config.timeout` for a
    /// streamed call, whose first byte does not wait for the answer, and
    /// [`ProviderConfig::buffered_timeout`] for a buffered one, whose headers
    /// arrive only when generation ends.
    async fn post(
        &self,
        body: &serde_json::Value,
        model: &str,
        bound: std::time::Duration,
    ) -> Result<reqwest::Response, ProviderError> {
        let builder = match &self.azure {
            Some(transport) => {
                let mut builder = self.http.post(transport.url(model)).json(body);
                if let Some(query) = transport.query() {
                    builder = builder.query(&[query]);
                }
                match self.config.credential() {
                    Some(key) => builder.header(
                        "api-key",
                        crate::azure::key_header(self.adapter_name(), key)?,
                    ),
                    None => builder,
                }
            }
            None => {
                let builder = self
                    .http
                    .post(format!("{}/chat/completions", self.config.base_url))
                    .json(body);
                match self.config.credential() {
                    Some(credential) => builder.bearer_auth(credential),
                    None => builder,
                }
            }
        };
        let http = match tokio::time::timeout(bound, builder.send()).await {
            Ok(result) => result.map_err(|e| crate::send_error(self.adapter_name(), e))?,
            Err(_) => {
                return Err(ProviderError::Transport {
                    adapter: self.adapter_name(),
                    reason: format!("no response within {bound:?} of sending the request"),
                });
            }
        };
        Ok(http)
    }

    /// Translate a kGraf request into the wire shape.
    ///
    /// The system prompt becomes a leading `system` message, which is where this
    /// protocol puts it. Cache breakpoints are **never** emitted: there is no field
    /// for them, and `new` refuses a configuration claiming otherwise.
    pub fn to_wire(&self, request: &Request) -> serde_json::Value {
        // `content` on this protocol accepts either a plain string or an array of
        // `{"type": "text", "text": ...}` parts, for every role including `system`:
        // https://platform.openai.com/docs/api-reference/chat/create (the `content`
        // field of a chat completion message). A single block is still sent as a bare
        // string, unchanged from before this fix, because that is the byte-identical
        // wire shape the prompt-cache prefix tests already pin down. Two or more
        // blocks are sent as separate parts rather than joined, so a notice and a
        // steer note (or an instruction and its context slices) stay distinct on the
        // wire instead of fusing into one run-on string. Either form is deterministic
        // over the same input, which is what the prompt-cache byte-prefix property
        // needs.
        let content_of = |texts: &[&str]| -> serde_json::Value {
            match texts {
                [] => serde_json::json!(""),
                [one] => serde_json::json!(one),
                many => serde_json::Value::Array(
                    many.iter()
                        .map(|text| serde_json::json!({"type": "text", "text": text}))
                        .collect(),
                ),
            }
        };

        let mut messages: Vec<serde_json::Value> = Vec::new();
        if !request.system.is_empty() {
            let texts: Vec<&str> = request
                .system
                .iter()
                .filter_map(|block| block.as_text())
                .collect();
            messages.push(serde_json::json!({
                "role": "system",
                "content": content_of(&texts),
            }));
        }
        for message in &request.messages {
            // A tool result is its own role on this protocol, not a content block, so
            // one kGraf message can expand into several wire messages.
            let tool_results: Vec<&Content> = message
                .content
                .iter()
                .filter(|block| matches!(block, Content::ToolResult { .. }))
                .collect();
            let tool_calls: Vec<serde_json::Value> = message
                .content
                .iter()
                .filter_map(|block| match block {
                    Content::ToolUse {
                        id, name, input, ..
                    } => Some(serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            // Arguments are a JSON *string* here, not an object.
                            "arguments": kgraf_provider::tool_arguments_text(input),
                        }
                    })),
                    _ => None,
                })
                .collect();
            let texts: Vec<&str> = message
                .content
                .iter()
                .filter_map(|block| match block {
                    Content::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();

            for result in tool_results {
                if let Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } = result
                {
                    // Failure is stated in the content as well as flagged, because at
                    // least one provider in this family ignores the error flag
                    // entirely, which would make a failed tool call
                    // indistinguishable from a successful one.
                    let body = if *is_error {
                        format!("ERROR: {content}")
                    } else {
                        content.clone()
                    };
                    messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": body,
                    }));
                }
            }
            if !texts.is_empty() || !tool_calls.is_empty() {
                let mut wire = serde_json::json!({
                    "role": match message.role { Role::User => "user", Role::Assistant => "assistant" },
                    "content": content_of(&texts),
                });
                if !tool_calls.is_empty() {
                    wire["tool_calls"] = serde_json::Value::Array(tool_calls);
                }
                messages.push(wire);
            }
        }

        let mut body = serde_json::json!({
            "model": request.model,
            "messages": messages,
            "max_tokens": request.max_output_tokens,
        });
        if self.azure.is_some()
            && let Some(object) = body.as_object_mut()
        {
            // Azure reasoning deployments refuse `max_tokens`; every Azure Chat
            // Completions version this transport accepts takes
            // `max_completion_tokens` for all deployments.
            if let Some(limit) = object.remove("max_tokens") {
                object.insert("max_completion_tokens".to_owned(), limit);
            }
        }
        if self.azure.as_ref().is_some_and(|t| !t.body_names_model())
            && let Some(object) = body.as_object_mut()
        {
            // The deployment is in the URL on this surface.
            object.remove("model");
        }
        if let Some(temperature) = request.temperature {
            body["temperature"] = serde_json::json!(temperature);
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.input_schema,
                            }
                        })
                    })
                    .collect(),
            );
        }
        if let Some(effort) = request.reasoning_effort {
            // Spelled as OpenAI spells it. A vendor that does not support it will
            // ignore it, which is the silent-parameter class the model echo and the
            // capability flags exist to bound; nothing about correctness depends on
            // it being honored.
            body["reasoning_effort"] = serde_json::json!(effort);
        }
        body
    }

    /// Normalize this protocol's usage object under the given dialect.
    ///
    /// Public so the dialect arithmetic can be asserted directly, which is where the
    /// inclusive-versus-additive property actually lives.
    pub fn normalize_usage(
        protocol: Protocol,
        dialect: UsageDialect,
        reasoning_dialect: ReasoningDialect,
        raw: &serde_json::Value,
    ) -> Result<TokenUsage, ProviderError> {
        Self::normalize_usage_as(ADAPTER, protocol, dialect, reasoning_dialect, raw)
    }

    /// [`Self::normalize_usage`] with errors labeled as `adapter`, so a transport
    /// that serves this body under its own name is named correctly.
    fn normalize_usage_as(
        adapter: &'static str,
        protocol: Protocol,
        dialect: UsageDialect,
        reasoning_dialect: ReasoningDialect,
        raw: &serde_json::Value,
    ) -> Result<TokenUsage, ProviderError> {
        let field = |name: &str| raw.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
        // An absent usage object is not a zero-token call, and must not read as one.
        if raw.get("prompt_tokens").is_none() && raw.get("completion_tokens").is_none() {
            return Err(ProviderError::UsageMissing { adapter });
        }
        warn_on_unrecognized_cache_key(
            adapter,
            raw,
            match dialect {
                UsageDialect::Details => CHAT_DETAILS_CACHE_PATHS,
                UsageDialect::HitAndMiss => CHAT_HIT_AND_MISS_CACHE_PATHS,
            },
        )?;
        let prompt = field("prompt_tokens");
        let completion = field("completion_tokens");
        let reasoning = raw
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let cache_read = match dialect {
            // Moonshot reports the count at the top level of `usage`
            // (`cached_tokens`), not in the details object. Read the details
            // field first and fall back to the top-level one only when the
            // details field is absent.
            UsageDialect::Details => raw
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .or_else(|| raw.get("cached_tokens").and_then(|v| v.as_u64()))
                .unwrap_or(0),
            UsageDialect::HitAndMiss => {
                let hit = field("prompt_cache_hit_tokens");
                let miss = field("prompt_cache_miss_tokens");
                // This dialect states both halves, so it can be checked. A split that
                // does not sum to the prompt total means the dialect is misconfigured
                // or the vendor changed shape, and either way the cost derived from it
                // would be wrong. Fail loud: raise rather than reconcile.
                if hit + miss != prompt {
                    return Err(ProviderError::Decode {
                        adapter,
                        reason: format!(
                            "hit-and-miss usage is inconsistent: {hit} hit plus {miss} miss \
                             does not equal {prompt} prompt tokens; the dialect is likely \
                             misconfigured"
                        ),
                    });
                }
                hit
            }
        };

        // Reported by some vendors in the details object. Moonshot's native route
        // does, as its first live call showed (evidence fixture
        // `2026-09-26-kimi-live-call.json`), which falsified this adapter's earlier
        // assumption that no Chat Completions vendor reports writes. Zero where it is
        // not reported: unreported, not known to be zero.
        let cache_write = match dialect {
            UsageDialect::Details => raw
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cache_write_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            UsageDialect::HitAndMiss => 0,
        };

        let text_output = match reasoning_dialect {
            ReasoningDialect::Inclusive => {
                // `completion_tokens` is documented to already contain the reasoning
                // share, so a reasoning count exceeding it is arithmetically
                // impossible under this dialect: either the dialect is
                // misconfigured for this vendor or the usage object itself is
                // inconsistent. A silent `saturating_sub` here previously clamped
                // this to zero text output instead of raising on an inconsistent
                // usage object, unlike the Anthropic adapter and the hit-and-miss
                // dialect above, both of which already raise.
                if reasoning > completion {
                    return Err(ProviderError::Decode {
                        adapter,
                        reason: format!(
                            "{reasoning} reasoning tokens reported against {completion} \
                             completion tokens under the inclusive reasoning dialect; the \
                             usage object is inconsistent or the dialect is misconfigured"
                        ),
                    });
                }
                completion - reasoning
            }
            // Additive: the two counts are independent, so no subtraction and no
            // possible inconsistency to detect from these two fields alone.
            ReasoningDialect::Additive => completion,
        };

        // Billed tokens the vendor left out of the counted fields are recorded
        // as reasoning output, the billed share nothing else accounts for.
        let hidden = unaccounted_total_tokens(
            adapter,
            raw,
            prompt,
            completion,
            reasoning,
            reasoning_dialect,
        )?;
        TokenUsage::normalize(
            protocol,
            prompt,
            cache_read,
            cache_write,
            text_output,
            reasoning + hidden,
            Some(raw.clone()),
        )
        .map_err(|source| ProviderError::Usage { adapter, source })
    }

    fn parse_response(
        &self,
        requested_model: &str,
        body: &serde_json::Value,
    ) -> Result<Response, ProviderError> {
        let returned_model =
            body.get("model")
                .and_then(|m| m.as_str())
                .ok_or_else(|| ProviderError::Decode {
                    adapter: self.adapter_name(),
                    reason: "response has no `model` field, so the echo cannot be checked"
                        .to_owned(),
                })?;
        // The assertion every adapter owes. This is the one that catches a provider
        // remapping an unknown model name to its cheap tier and returning HTTP 200.
        kgraf_provider::provider::assert_model_echo(
            self.echo_target(requested_model)?,
            returned_model,
        )?;

        let usage = body.get("usage").ok_or(ProviderError::UsageMissing {
            adapter: self.adapter_name(),
        })?;
        let mut usage = Self::normalize_usage_as(
            self.adapter_name(),
            self.config.protocol,
            self.dialect,
            self.reasoning_dialect,
            usage,
        )?;

        let choice = body
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .ok_or_else(|| ProviderError::Decode {
                adapter: self.adapter_name(),
                reason: "response carries no choices".to_owned(),
            })?;
        let message = choice.get("message").cloned().unwrap_or_default();

        let mut content = Vec::new();
        // Several vendors in this family put chain-of-thought in a sibling field.
        // It is carried as reasoning rather than merged into the text, so a caller
        // can tell the difference.
        mark_unsplit_reasoning(&mut usage, reasoning_field(&message));
        if let Some(reasoning) = reasoning_field(&message) {
            content.push(Content::Reasoning {
                text: reasoning.to_owned(),
            });
        }
        if let Some(text) = message
            .get("content")
            .and_then(|c| c.as_str())
            .filter(|t| !t.is_empty())
        {
            content.push(Content::text(text));
        }
        if let Some(calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
            for call in calls {
                let function = call.get("function").cloned().unwrap_or_default();
                let arguments = function
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .map(|a| {
                        // Arguments arrive as a JSON string. An empty one is a call
                        // with no arguments. A string that will not parse is kept
                        // verbatim rather than dropped, so a worker can see what the
                        // model actually emitted instead of receiving silence.
                        if a.trim().is_empty() {
                            return serde_json::json!({});
                        }
                        serde_json::from_str(a).unwrap_or(serde_json::Value::String(a.to_owned()))
                    })
                    .unwrap_or(serde_json::Value::Null);
                content.push(Content::ToolUse {
                    id: call
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned(),
                    name: function
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned(),
                    input: arguments,
                    signature: None,
                });
            }
        }

        let raw_finish = choice.get("finish_reason").and_then(|f| f.as_str());
        let stop_reason = match raw_finish {
            Some(reason) => stop_reason_for(reason, content.iter().any(is_tool_use)),
            None => StopReason::Other,
        };
        Ok(Response {
            finish_reason: raw_finish.map(str::to_owned),
            model: returned_model.to_owned(),
            content,
            stop_reason,
            usage,
            request_id: body.get("id").and_then(|i| i.as_str()).map(str::to_owned),
        })
    }
}

#[async_trait::async_trait]
impl kgraf_provider::Provider for ChatCompletionsAdapter {
    fn adapter(&self) -> &'static str {
        self.adapter_name()
    }

    fn protocol(&self) -> Protocol {
        self.config.protocol
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        if let Err(error) = self.preflight(&request.model) {
            // Refused before sending, so nothing billed; close the row as such
            // rather than letting the dropped token report a hole.
            token.close_on_async(&error, None).await?;
            return Err(error);
        }
        let body = self.to_wire(request);
        // Buffered: no byte arrives until the answer exists, so the bound
        // scales with the output cap (`ProviderConfig::buffered_timeout`).
        let bound = self.config.buffered_timeout(request.max_output_tokens);
        let http = match self.post(&body, &request.model, bound).await {
            Ok(http) => http,
            Err(error) => {
                token.close_on_async(&error, None).await?;
                return Err(error);
            }
        };
        let status = http.status();
        self.observe_limits(&request.model, &http);
        let request_id = http
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if !status.is_success() {
            let text = crate::bounded_error_body(self.config.timeout, http).await;
            let error = crate::http_error(self.adapter_name(), status.as_u16(), text);
            // A 4xx rejection billed nothing; a 5xx or 408 may have. `close_on`
            // keeps the second kind open for reconciliation.
            token.close_on_async(&error, request_id.as_deref()).await?;
            return Err(error);
        }
        // A whole-call bound is correct here: `complete` buffers one response
        // and returns, unlike `stream`, so there is no legitimate reason for
        // reading the body to take longer than `config.timeout`.
        let text = match tokio::time::timeout(bound, http.text()).await {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                token
                    .leave_unreconciled_async(request_id.as_deref())
                    .await?;
                return Err(ProviderError::Transport {
                    adapter: self.adapter_name(),
                    reason: e.to_string(),
                });
            }
            Err(_) => {
                token
                    .leave_unreconciled_async(request_id.as_deref())
                    .await?;
                return Err(ProviderError::Transport {
                    adapter: self.adapter_name(),
                    reason: format!("response body did not finish within {:?}", bound),
                });
            }
        };
        let parsed: serde_json::Value = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(e) => {
                token
                    .leave_unreconciled_async(request_id.as_deref())
                    .await?;
                return Err(ProviderError::Decode {
                    adapter: self.adapter_name(),
                    reason: e.to_string(),
                });
            }
        };
        // A vendor in this family has been seen to return HTTP 200 with the
        // body carrying an `error` object instead of a normal completion, the
        // non-streaming twin of the mid-stream `error` chunk `stream` below
        // must also raise on. Checked before `parse_response`, which would
        // otherwise fail with a less specific "no `model` field" decode error.
        if let Some(error) = parsed.get("error").filter(|e| !e.is_null()) {
            token
                .leave_unreconciled_async(request_id.as_deref())
                .await?;
            return Err(crate::in_band_error(
                self.adapter_name(),
                error,
                format!("provider returned HTTP 200 with an `error` object in the body: {error}"),
            ));
        }
        match self.parse_response(&request.model, &parsed) {
            Ok(mut response) => {
                if response.request_id.is_none() {
                    response.request_id = request_id.clone();
                }
                token
                    .settle_async(&response.usage, response.request_id.as_deref())
                    .await?;
                // The call billed, so it settles above; the filtered turn is
                // then refused rather than returned as an answer.
                if self.azure.is_some()
                    && let Some(stop_reason) = crate::azure::chat_filter(&parsed)
                {
                    return Err(ProviderError::TurnNotUsable {
                        adapter: self.adapter_name(),
                        stop_reason: stop_reason.to_owned(),
                    });
                }
                Ok(response)
            }
            Err(error) => {
                // The call billed even though its response was wrong or undecodable,
                // so the row stays open for reconciliation. Usage the body already
                // carries is kept on the open row, not dropped with the parse error.
                let carried = parsed.get("usage").and_then(|u| {
                    Self::normalize_usage_as(
                        self.adapter_name(),
                        self.config.protocol,
                        self.dialect,
                        self.reasoning_dialect,
                        u,
                    )
                    .ok()
                    // A usage a consistency check refused is still a floor.
                    .or_else(|| {
                        lower_bound_usage(
                            self.config.protocol,
                            self.dialect,
                            self.reasoning_dialect,
                            u,
                        )
                    })
                });
                token
                    .leave_unreconciled_with_async(carried.as_ref(), request_id.as_deref())
                    .await?;
                Err(error)
            }
        }
    }

    fn capabilities(&self) -> Capabilities<'_> {
        // No cache control on any variant of this protocol: caching is automatic and
        // server-side where it exists, with no breakpoint field to emit. Absent
        // rather than a no-op that would silently discard breakpoints.
        let mut capabilities = Capabilities::none().with_streaming(self);
        if self.reports_reasoning_tokens {
            capabilities = capabilities.with_reasoning(self);
        }
        capabilities
    }

    /// A Chat Completions reply carries its whole text share: the message
    /// content, the tool calls and, where the server returns it, the
    /// reasoning. Reasoning a server hides is counted apart in
    /// `completion_tokens_details.reasoning_tokens`, which the floor leaves
    /// out. A local server that cannot parse the model's tool call can drop it
    /// and still answer 200 with `finish_reason: "stop"`; only the count shows
    /// it.
    fn undelivered_output_tokens(&self, response: &Response) -> Option<u64> {
        Some(response.undelivered_output_floor())
    }
}

impl Reasoning for ChatCompletionsAdapter {
    fn reports_reasoning_tokens(&self) -> bool {
        self.reports_reasoning_tokens
    }
}

/// One tool call accumulating across `delta.tool_calls` chunks.
///
/// `id` and `function.name` arrive once, on the chunk that opens the call;
/// `function.arguments` arrives as successive string fragments that are not
/// valid JSON until concatenated in full, so parsing is deferred to
/// [`ToolCallAccumulator::finish`], exactly as a `tool_use` block's
/// `input_json_delta` fragments are on the Anthropic protocol.
#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

/// How one `delta.tool_calls` fragment is matched back to the call it
/// belongs to.
///
/// `index` is the documented disambiguator and is used whenever a fragment
/// carries one, exactly as before. Testing found that a fragment
/// lacking `index` entirely was previously folded into a single slot
/// (`unwrap_or(0)`), silently merging distinct calls into one with the first
/// call's name and both calls' arguments concatenated into invalid JSON,
/// and the row still settled `Counted`. Falling back to `id` fixes the
/// common case (a vendor that never sends `index` but does send a distinct
/// `id` per call); a fragment with neither is only safe to attribute to the
/// stream's one already-open call, and is a decode error otherwise, because
/// there is no way to know which call it continues.
#[derive(Clone, PartialEq, Eq)]
enum ToolCallKey {
    Index(u64),
    Id(String),
}

/// Hard cap on how many bytes of `function.arguments` fragments a single tool
/// call may accumulate without closing. Protects against a provider that
/// never finishes the call: without this, `arguments` would grow without
/// bound rather than raising.
const MAX_TOOL_ARGUMENT_BYTES: usize = 4 * 1024 * 1024;

impl ToolCallAccumulator {
    /// Turn an accumulated call into its [`Content`]. Unparseable arguments
    /// are kept verbatim, matching [`ChatCompletionsAdapter::parse_response`]:
    /// a worker debugging a malformed tool call needs to see what the model
    /// actually emitted, not silence.
    fn finish(self) -> Content {
        let input = if self.arguments.trim().is_empty() {
            // A call with no arguments.
            serde_json::json!({})
        } else {
            serde_json::from_str(&self.arguments)
                .unwrap_or(serde_json::Value::String(self.arguments))
        };
        Content::ToolUse {
            id: self.id,
            name: self.name,
            input,
            signature: None,
        }
    }
}

#[async_trait::async_trait]
impl Streaming for ChatCompletionsAdapter {
    async fn stream(
        &self,
        request: &Request,
        token: &CallToken,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response, ProviderError> {
        if let Err(error) = self.preflight(&request.model) {
            token.close_on_async(&error, None).await?;
            return Err(error);
        }
        if self.streamed_usage_location == StreamedUsageLocation::None {
            // Declared, not discovered mid-call: paying for a request whose
            // usage can never be attributed and finding that out only after
            // the connection closed would be exactly the silent spend the fail-loud rule
            // forbids. Refused here, before anything is sent.
            let error = ProviderError::CapabilityNotHonored {
                adapter: self.adapter_name(),
                capability: "streamed usage accounting",
            };
            token.close_on_async(&error, None).await?;
            return Err(error);
        }
        let mut body = self.to_wire(request);
        body["stream"] = serde_json::json!(true);
        // Requested explicitly on every stream, so a vendor that silently
        // ignores it, rather than one that genuinely lacks the field, is
        // caught by the same truncation check as a stream that dies before
        // its usage arrives, instead of quietly proceeding with no usage at
        // all (fail loud).
        body["stream_options"] = serde_json::json!({"include_usage": true});

        let http = match self.post(&body, &request.model, self.config.timeout).await {
            Ok(http) => http,
            Err(error) => {
                token.close_on_async(&error, None).await?;
                return Err(error);
            }
        };
        let status = http.status();
        self.observe_limits(&request.model, &http);
        let request_id = http
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if !status.is_success() {
            let text = crate::bounded_error_body(self.config.timeout, http).await;
            let error = crate::http_error(self.adapter_name(), status.as_u16(), text);
            token.close_on_async(&error, request_id.as_deref()).await?;
            return Err(error);
        }

        let mut usage = TokenUsage::default();
        let mut saw_usage = false;
        let mut saw_finish_reason = false;
        let mut saw_model = false;
        let mut model = request.model.clone();
        let mut started = false;
        // Set when the Azure content filter acted on the prompt or the
        // completion; raised after the call settles.
        let mut filtered: Option<&'static str> = None;
        // Whether `stream_request_id` came from a chunk's `id`.
        let mut id_from_chunk = false;
        let expected_model = self.echo_target(&request.model)?.to_owned();
        let mut stop_reason = StopReason::Other;
        let mut raw_finish: Option<String> = None;
        let mut stream_request_id = request_id.clone();
        let mut text = String::new();
        let mut reasoning_text = String::new();
        let mut tool_calls: Vec<(ToolCallKey, ToolCallAccumulator)> = Vec::new();

        // Every exit from here on must still settle the ledger row with
        // whatever usage and request id are already known, including the
        // caller dropping this whole future to cancel (every call is attributed to the ledger). Disarmed only
        // on the single success path below.
        let guard = StreamGuard::new(token);

        // Read incrementally, never buffered whole: see `kgraf_provider::
        // streaming` for why. A dropped future drops `byte_stream` with it,
        // which aborts the in-flight request.
        let mut decoder = kgraf_provider::SseDecoder::new(self.adapter_name());
        let mut byte_stream = http.bytes_stream();
        let mut pending: std::collections::VecDeque<serde_json::Value> =
            std::collections::VecDeque::new();
        loop {
            let event = match pending.pop_front() {
                Some(event) => event,
                None => {
                    // No whole-call timeout applies to a stream: only a gap
                    // this long between chunks counts as stalled, however far
                    // into an otherwise healthy stream it happens.
                    match tokio::time::timeout(self.config.timeout, byte_stream.next()).await {
                        Ok(Some(Ok(chunk))) => {
                            pending.extend(decoder.push(&chunk)?);
                            continue;
                        }
                        Ok(Some(Err(e))) => {
                            return Err(ProviderError::Transport {
                                adapter: self.adapter_name(),
                                reason: e.to_string(),
                            });
                        }
                        Ok(None) => {
                            pending.extend(decoder.finish()?);
                            match pending.pop_front() {
                                Some(event) => event,
                                None => break,
                            }
                        }
                        Err(_) => {
                            return Err(ProviderError::Transport {
                                adapter: self.adapter_name(),
                                reason: format!(
                                    "no data received for {:?}; the stream stalled",
                                    self.config.timeout
                                ),
                            });
                        }
                    }
                }
            };

            // A vendor in this family has been observed sending a bare
            // `{"error": {...}}` object mid-stream in place of a normal
            // chunk, because the response already committed to HTTP 200
            // before the failure happened. Left unhandled, this previously
            // fell through to the ordinary chunk parser, found no `choices`,
            // and was silently skipped, a failed generation would return
            // `Ok` with whatever partial content had streamed so far.
            if let Some(error) = event.get("error").filter(|e| !e.is_null()) {
                return Err(crate::in_band_error(
                    self.adapter_name(),
                    error,
                    format!("provider sent a stream `error` chunk: {error}"),
                ));
            }

            // Azure opens a stream with a chunk that carries the prompt filter
            // results and an empty `id` and `model`. An empty value is no value:
            // the first chunk that names an id or a model supplies it.
            if let Some(id) = event
                .get("id")
                .and_then(|i| i.as_str())
                .filter(|i| !i.is_empty())
                && !id_from_chunk
            {
                id_from_chunk = true;
                stream_request_id = Some(id.to_owned());
                guard.record_request_id(stream_request_id.as_deref());
            }
            if self.azure.is_some()
                && let Some(reason) = crate::azure::chat_filter(&event)
            {
                filtered = Some(reason);
            }
            if !started {
                started = true;
                sink(StreamEvent::Started {
                    model: model.clone(),
                    request_id: stream_request_id.clone(),
                });
            }

            // Checked on **every** chunk that carries a `model` field, not
            // only the first. Testing found the previous version
            // skipped the assertion entirely when the opening chunk happened
            // to omit `model`, a real shape at least one vendor sends, so a
            // later chunk substituting a different model went unchecked for
            // the rest of the call (fail loud).
            if let Some(returned) = event
                .get("model")
                .and_then(|m| m.as_str())
                .filter(|m| !m.is_empty() || self.azure.is_none())
            {
                saw_model = true;
                kgraf_provider::provider::assert_model_echo(&expected_model, returned)?;
                model = returned.to_owned();
            }

            // Usage is accepted from either shape a vendor in this family has
            // been observed to use, regardless of `streamed_usage_location`:
            // that field's only job is refusing to stream at all against a
            // route declared to send neither (checked above, before
            // sending). `"usage":{}`, seen on at least one vendor's
            // intermediate chunks, is treated as absent, not as a real,
            // if-empty, usage report.
            let top_level_usage = event.get("usage").filter(|u| is_meaningful_usage(u));
            let choice = event
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|c| c.first());
            let in_choice_usage = choice
                .and_then(|c| c.get("usage"))
                .filter(|u| is_meaningful_usage(u));
            if let Some(usage_raw) = top_level_usage.or(in_choice_usage) {
                // Not cumulative on this protocol: the usage chunk carries the
                // call's final totals in one shot, so it is taken as-is rather
                // than merged.
                let normalized = match Self::normalize_usage_as(
                    self.adapter_name(),
                    self.config.protocol,
                    self.dialect,
                    self.reasoning_dialect,
                    usage_raw,
                ) {
                    Ok(normalized) => normalized,
                    Err(error) => {
                        // The call is refused, but it billed: keep the counted
                        // fields as reported and the raw object on the open row.
                        if let Some(floor) = lower_bound_usage(
                            self.config.protocol,
                            self.dialect,
                            self.reasoning_dialect,
                            usage_raw,
                        ) {
                            guard.record_usage(&floor);
                        }
                        return Err(error);
                    }
                };
                // Kimi has been observed sending the identical final usage
                // twice: once inside the finish chunk's `choices[0].usage`,
                // once more top-level on a trailing chunk with empty
                // `choices` (recorded fixture:
                // tests/fixtures/2026-09-26-kimi-chat-stream.sse).
                // Recording and sinking it twice would double the ledger's
                // usage events for one call, so only a usage that differs
                // from the last one recorded is emitted; a differing second
                // report still replaces the first, since a genuine revision
                // is not the same fault as a plain repeat.
                if !saw_usage || normalized != usage {
                    usage = normalized;
                    saw_usage = true;
                    guard.record_usage(&usage);
                    sink(StreamEvent::Usage {
                        usage: usage.clone(),
                    });
                }
            }

            if let Some(choice) = choice {
                let delta = choice.get("delta").cloned().unwrap_or_default();
                if let Some(reasoning) = reasoning_field(&delta) {
                    // Collected, not only sunk: a previous version sank each
                    // fragment to the caller's live-display sink but never
                    // accumulated it, so a consumer reading the returned
                    // `Response` instead of watching the sink saw no
                    // reasoning at all, unlike `complete()`'s equivalent path.
                    reasoning_text.push_str(reasoning);
                    sink(StreamEvent::Delta {
                        content: Content::Reasoning {
                            text: reasoning.to_owned(),
                        },
                    });
                }
                if let Some(fragment) = delta
                    .get("content")
                    .and_then(|c| c.as_str())
                    .filter(|c| !c.is_empty())
                {
                    text.push_str(fragment);
                    sink(StreamEvent::Delta {
                        content: Content::text(fragment),
                    });
                }
                if let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for call in calls {
                        let index = call.get("index").and_then(|i| i.as_u64());
                        let id = call
                            .get("id")
                            .and_then(|v| v.as_str())
                            .filter(|id| !id.is_empty());
                        let key = match (index, id) {
                            (Some(index), _) => ToolCallKey::Index(index),
                            (None, Some(id)) => ToolCallKey::Id(id.to_owned()),
                            (None, None) => {
                                // Neither disambiguator is present. Safe only
                                // as a continuation of the single call already
                                // open; with zero or more than one, there is
                                // no way to know which call this fragment
                                // belongs to, and guessing (the previous
                                // `unwrap_or(0)`) silently merged distinct
                                // calls into one.
                                match tool_calls.as_slice() {
                                    [(only_key, _)] => only_key.clone(),
                                    _ => {
                                        return Err(ProviderError::Decode {
                                            adapter: self.adapter_name(),
                                            reason: format!(
                                                "a tool_calls delta fragment carries neither \
                                                 `index` nor `id`, and {} tool call(s) are \
                                                 already open, so it cannot be attributed to \
                                                 one",
                                                tool_calls.len()
                                            ),
                                        });
                                    }
                                }
                            }
                        };
                        let slot = match tool_calls.iter().position(|(k, _)| *k == key) {
                            Some(position) => position,
                            None => {
                                tool_calls.push((key, ToolCallAccumulator::default()));
                                tool_calls.len() - 1
                            }
                        };
                        let entry = &mut tool_calls[slot].1;
                        if let Some(id) = id {
                            entry.id = id.to_owned();
                        }
                        let function = call.get("function").cloned().unwrap_or_default();
                        if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
                            entry.name = name.to_owned();
                        }
                        if let Some(fragment) = function.get("arguments").and_then(|v| v.as_str()) {
                            if entry.arguments.len() + fragment.len() > MAX_TOOL_ARGUMENT_BYTES {
                                return Err(ProviderError::Decode {
                                    adapter: self.adapter_name(),
                                    reason: format!(
                                        "a tool call accumulated more than \
                                         {MAX_TOOL_ARGUMENT_BYTES} bytes of argument \
                                         fragments without closing; treated as corrupt \
                                         rather than buffered without limit"
                                    ),
                                });
                            }
                            entry.arguments.push_str(fragment);
                        }
                    }
                }
                if let Some(reason) = choice
                    .get("finish_reason")
                    .and_then(|f| f.as_str())
                    // An empty string is sent by at least one vendor on an
                    // intermediate chunk and must not be read as a genuine,
                    // if unmodeled, terminal reason.
                    .filter(|r| !r.is_empty())
                {
                    saw_finish_reason = true;
                    raw_finish = Some(reason.to_owned());
                    stop_reason = stop_reason_for(reason, !tool_calls.is_empty());
                }
            }
        }

        if !saw_finish_reason || !saw_usage {
            // Checked before the model-echo completeness check just below:
            // a connection that produced nothing at all (or died before its
            // terminal chunks) is a truncated stream first and foremost,
            // whether or not it happened to carry a `model` field along the
            // way. The connection closed before the chunk that carries
            // `finish_reason`, before the usage chunk `stream_options.
            // include_usage` was asked to send, or both. Settling this as
            // `Counted` would bill whatever partial usage was seen (possibly
            // none) as the call's final figure, silently understating spend
            // (every call is attributed to the ledger), recorded as unreconciled instead, with the partial
            // figure kept as a stated lower bound.
            //
            // A stream the Azure content filter cut may end with no usage chunk. It
            // raises the filter reason, which says why it ended, and the row stays
            // open for reconciliation the same way (the guard is not disarmed).
            // Only a cut reason stands in for the truncation: a filter that merely
            // did not run says nothing about why the stream ended.
            if let Some(stop_reason) = filtered
                && stop_reason != "content_filter_results_error"
            {
                return Err(ProviderError::TurnNotUsable {
                    adapter: self.adapter_name(),
                    stop_reason: stop_reason.to_owned(),
                });
            }
            return Err(ProviderError::StreamTruncated {
                adapter: self.adapter_name(),
            });
        }

        if !saw_model {
            // The stream reached both its terminal chunks but no chunk along
            // the way ever carried a `model` field, so the response could
            // never be checked against the requested model (fail loud): the
            // per-chunk assertion above only fires on a chunk that has the
            // field to check in the first place.
            return Err(ProviderError::Decode {
                adapter: self.adapter_name(),
                reason: "no chunk in the stream carried a `model` field; the response \
                         could not be verified against the requested model"
                    .to_owned(),
            });
        }

        mark_unsplit_reasoning(&mut usage, Some(&reasoning_text));
        let mut content = Vec::new();
        if !reasoning_text.is_empty() {
            content.push(Content::Reasoning {
                text: reasoning_text,
            });
        }
        if !text.is_empty() {
            content.push(Content::text(text));
        }
        // Tool calls that arrived after a `stop` finish chunk still decide it.
        if stop_reason == StopReason::EndTurn && !tool_calls.is_empty() {
            stop_reason = StopReason::ToolUse;
        }
        for (_, call) in tool_calls {
            content.push(call.finish());
        }
        // Announced once, after the stream ends, so it carries the same stop
        // reason as the assembled response (tool calls that arrive after a `stop`
        // chunk turn it into `ToolUse`).
        sink(StreamEvent::Finished { stop_reason });

        guard.disarm();
        token
            .settle_async(&usage, stream_request_id.as_deref())
            .await?;
        if let Some(stop_reason) = filtered {
            return Err(ProviderError::TurnNotUsable {
                adapter: self.adapter_name(),
                stop_reason: stop_reason.to_owned(),
            });
        }
        Ok(Response {
            finish_reason: raw_finish,
            model,
            content,
            stop_reason,
            usage,
            request_id: stream_request_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_provider::{Ledger, Provider, Settlement};
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn run_stream(
        adapter: &ChatCompletionsAdapter,
        token: &kgraf_provider::CallToken<'_>,
    ) -> Result<Response, ProviderError> {
        Streaming::stream(
            adapter,
            &Request::new("compatible-model", "hi", 100),
            token,
            &mut |_| {},
        )
        .await
    }

    fn config(base_url: &str) -> ProviderConfig {
        ProviderConfig::new(
            ADAPTER,
            base_url,
            "test-key",
            vec!["compatible-model".into()],
            Protocol::Inclusive,
        )
        .expect("config")
    }

    /// A response in the shape a `Details`-dialect vendor returns.
    fn details_body(model: &str) -> serde_json::Value {
        json!({
            "id": "chatcmpl-01",
            "model": model,
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": "hello from a compatible endpoint"}
            }],
            "usage": {
                "prompt_tokens": 1050,
                "prompt_tokens_details": {"cached_tokens": 900},
                "completion_tokens": 30,
                "completion_tokens_details": {"reasoning_tokens": 10}
            }
        })
    }

    /// A buffered call sends no header until the answer exists, so the bound
    /// scales with the output cap: headers later than the base timeout but
    /// inside the scaled bound succeed, and a server that stays silent past
    /// the scaled bound fails loud.
    #[tokio::test]
    async fn a_buffered_call_waits_for_late_headers_within_the_scaled_bound() {
        use std::time::{Duration, Instant};
        let slow = |delay: Duration| async move {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(details_body("compatible-model"))
                        .set_delay(delay),
                )
                .mount(&server)
                .await;
            // Base 200 ms; 100 tokens at a floor of 100 tokens/s scales to 1 s.
            let config = config(&server.uri())
                .with_timeout(Duration::from_millis(200))
                .with_floor_rate(100);
            let adapter =
                ChatCompletionsAdapter::new(config, UsageDialect::Details).expect("adapter");
            let ledger = Ledger::open_in_memory().expect("ledger");
            let token = ledger
                .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
                .expect("begin");
            let started = Instant::now();
            let result = adapter
                .complete(&Request::new("compatible-model", "hi", 100), &token)
                .await;
            (result, started.elapsed())
        };
        let (late, _) = slow(Duration::from_millis(600)).await;
        late.expect("headers after the base timeout, inside the scaled bound");
        let (never, took) = slow(Duration::from_secs(5)).await;
        let err = never.expect_err("past the scaled bound");
        assert!(
            matches!(&err, ProviderError::Transport { reason, .. } if reason.contains("no response within")),
            "{err:?}"
        );
        assert!(
            took < Duration::from_secs(3),
            "fails at the bound: {took:?}"
        );
    }

    /// A model echo mismatch is a parse error, but the body still carries the
    /// usage the provider billed. The open row keeps that figure.
    #[tokio::test]
    async fn a_parse_failure_keeps_the_usage_the_body_already_carried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(details_body("cheaper-model")))
            .mount(&server)
            .await;
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let err = adapter
            .complete(&Request::new("compatible-model", "hi", 100), &token)
            .await
            .expect_err("must raise");
        assert!(matches!(err, ProviderError::ModelSubstituted { .. }));
        let open = ledger.unreconciled().expect("query");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].usage.total_input(), 1050);
        assert_eq!(open[0].usage.cache_read, 900);
    }

    /// A usage refused on its total (below the counted parts) still billed. The
    /// open row keeps the counted fields and the raw object, whether the call
    /// was buffered or streamed.
    #[tokio::test]
    async fn a_call_refused_on_its_total_keeps_its_usage_and_raw_object() {
        let refused_usage = json!({
            "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 90
        });
        // Buffered.
        let server = MockServer::start().await;
        let mut body = details_body("compatible-model");
        body["usage"] = refused_usage.clone();
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        {
            let token = ledger
                .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
                .expect("begin");
            adapter
                .complete(&Request::new("compatible-model", "hi", 100), &token)
                .await
                .expect_err("refused");
        }
        let open = ledger.unreconciled().expect("query");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].usage.total_input(), 100);
        assert_eq!(open[0].usage.text_output, 10);
        assert_eq!(open[0].usage.raw.as_ref().expect("raw")["total_tokens"], 90);

        // Streamed.
        let server = MockServer::start().await;
        let sse = format!(
            "data: {{\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":[{{\"index\":0,\
             \"delta\":{{\"content\":\"hi\"}}}}]}}\n\n\
             data: {{\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":[],\"usage\":{refused_usage}}}\n\n"
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        {
            let token = ledger
                .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
                .expect("begin");
            run_stream(&adapter, &token).await.expect_err("refused");
        }
        let open = ledger.unreconciled().expect("query");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].usage.total_input(), 100);
        assert_eq!(open[0].usage.raw.as_ref().expect("raw")["total_tokens"], 90);
    }

    #[tokio::test]
    async fn a_compatible_endpoint_is_reached_at_chat_completions() {
        // The whole point of this adapter: the path and the request shape a
        // third-party compatible vendor actually serves. `/responses` with an
        // `input` array would 404 at every one of them.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_partial_json(
                json!({"model": "compatible-model", "max_tokens": 500}),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(details_body("compatible-model")),
            )
            .mount(&server)
            .await;

        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(
                ADAPTER,
                Protocol::Inclusive,
                "compatible-model",
                "worker",
                "task",
            )
            .expect("begin");
        let response = adapter
            .complete(&Request::new("compatible-model", "hi", 500), &token)
            .await
            .expect("complete");

        assert_eq!(response.text(), "hello from a compatible endpoint");
        // Inclusive: the 900 cached tokens are carved out of the reported 1050.
        assert_eq!(response.usage.uncached_input, 150);
        assert_eq!(response.usage.cache_read, 900);
        assert_eq!(response.usage.total_input(), 1050);
        // Reasoning is carved out of completion rather than double counted.
        assert_eq!(response.usage.reasoning_output, 10);
        assert_eq!(response.usage.text_output, 20);
        assert_eq!(response.usage.total_output(), 30);

        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Counted);
        assert!(
            record.usage.raw.is_some(),
            "the raw blob must be kept verbatim"
        );
    }

    #[tokio::test]
    async fn the_system_prompt_becomes_a_leading_system_message() {
        let server = MockServer::start().await;
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let mut request = Request::new("compatible-model", "the question", 100);
        request.system = vec![Content::text("the standing instructions")];
        let wire = adapter.to_wire(&request);
        assert_eq!(wire["messages"][0]["role"], "system");
        assert_eq!(wire["messages"][0]["content"], "the standing instructions");
        assert_eq!(wire["messages"][1]["role"], "user");
        // This protocol names the output ceiling differently from Responses.
        assert!(wire.get("max_tokens").is_some());
        assert!(wire.get("max_output_tokens").is_none());
    }

    #[test]
    fn two_system_text_blocks_stay_distinct_on_the_wire() {
        // Mutation check: replacing `content_of` with a `.join("")` over the two
        // blocks' text makes this fail, because the two strings below would fuse into
        // one and neither would appear as its own part.
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let mut request = Request::new("compatible-model", "the question", 100);
        request.system = vec![Content::text("a notice"), Content::text("a steer note")];
        let wire = adapter.to_wire(&request);
        let content = &wire["messages"][0]["content"];
        assert_eq!(
            content.as_array().expect("array of parts").len(),
            2,
            "two blocks must stay as two parts: {wire}"
        );
        assert_eq!(content[0], json!({"type": "text", "text": "a notice"}));
        assert_eq!(content[1], json!({"type": "text", "text": "a steer note"}));
        assert!(
            !wire.to_string().contains("a noticea steer note"),
            "the two blocks must not fuse into one run-on string: {wire}"
        );
    }

    #[test]
    fn two_message_text_blocks_stay_distinct_on_the_wire() {
        // Mutation check: restoring the old `.join("")` over the message's text
        // blocks fuses "an instruction" and "context slices" into one string and this
        // test fails.
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let mut request = Request::new("compatible-model", "x", 10);
        request.messages = vec![kgraf_provider::Message {
            role: Role::User,
            content: vec![
                Content::text("an instruction"),
                Content::text("context slices"),
            ],
        }];
        let wire = adapter.to_wire(&request);
        let content = &wire["messages"][0]["content"];
        assert_eq!(content.as_array().expect("array of parts").len(), 2);
        assert_eq!(
            content[0],
            json!({"type": "text", "text": "an instruction"})
        );
        assert_eq!(
            content[1],
            json!({"type": "text", "text": "context slices"})
        );
        assert!(!wire.to_string().contains("an instructioncontext slices"));
    }

    #[test]
    fn the_first_live_kimi_usage_replays_to_the_recorded_normal_form() {
        // The real-speaker rule: this is a real speaker's usage object, recorded through this adapter
        // on 2026-09-26, not a shape written from documentation. Replaying it guards
        // the normalization against a change that would have misread it.
        let record: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/2026-09-26-kimi-live-call.json"
        ))
        .expect("fixture");
        let raw = &record["usage"]["raw"];
        let usage = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Inclusive,
            raw,
        )
        .expect("usage");
        let recorded = &record["usage"];
        let field = |name: &str| recorded[name].as_u64().expect("recorded field");
        assert_eq!(usage.uncached_input, field("uncached_input"));
        assert_eq!(usage.cache_read, field("cache_read"));
        assert_eq!(usage.cache_write, field("cache_write"));
        assert_eq!(usage.text_output, field("text_output"));
        assert_eq!(usage.reasoning_output, field("reasoning_output"));
        assert_eq!(
            usage.total_input() + usage.total_output(),
            raw["total_tokens"].as_u64().expect("total"),
            "the normal form must sum back to the vendor's own total"
        );
    }

    #[test]
    fn reported_cache_writes_are_carved_out_of_inclusive_input() {
        // The field the live call revealed, exercised with a non-zero value.
        let raw = json!({
            "prompt_tokens": 5000,
            "prompt_tokens_details": {"cached_tokens": 1000, "cache_write_tokens": 3500},
            "completion_tokens": 20
        });
        let usage = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Inclusive,
            &raw,
        )
        .expect("usage");
        assert_eq!(usage.cache_read, 1000);
        assert_eq!(usage.cache_write, 3500);
        assert_eq!(usage.uncached_input, 500);
        assert_eq!(usage.total_input(), 5000);
    }

    #[test]
    fn the_hit_and_miss_dialect_reads_the_split_from_sibling_fields() {
        // Decision 0006 names this as a required third usage adapter.
        let raw = json!({
            "prompt_tokens": 1050,
            "prompt_cache_hit_tokens": 900,
            "prompt_cache_miss_tokens": 150,
            "completion_tokens": 30
        });
        let usage = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::HitAndMiss,
            ReasoningDialect::Inclusive,
            &raw,
        )
        .expect("usage");
        assert_eq!(usage.cache_read, 900);
        assert_eq!(usage.uncached_input, 150);
        assert_eq!(usage.total_input(), 1050);
    }

    #[test]
    fn an_inconsistent_hit_and_miss_split_raises_rather_than_reconciling() {
        // This dialect states both halves, so it can check itself. A split that does
        // not sum to the prompt total means the dialect is misconfigured or the
        // vendor changed shape; either way the derived cost is wrong.
        let raw = json!({
            "prompt_tokens": 1050,
            "prompt_cache_hit_tokens": 900,
            "prompt_cache_miss_tokens": 900,
            "completion_tokens": 30
        });
        let err = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::HitAndMiss,
            ReasoningDialect::Inclusive,
            &raw,
        )
        .expect_err("must raise");
        assert!(matches!(err, ProviderError::Decode { .. }), "got {err:?}");
    }

    #[test]
    fn the_wrong_dialect_does_not_silently_report_everything_as_uncached() {
        // A hit-and-miss vendor read with the Details dialect would find no
        // `prompt_tokens_details` and attribute the whole prompt to uncached
        // input, a real cost error. The nonzero hit count sits at a path the
        // Details dialect does not read, so it raises instead. Read with the
        // right dialect, the same body is correct.
        let raw = json!({
            "prompt_tokens": 1050,
            "prompt_cache_hit_tokens": 900,
            "prompt_cache_miss_tokens": 150,
            "completion_tokens": 30
        });
        let wrong = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Inclusive,
            &raw,
        )
        .expect_err("the hit count is at a path this dialect does not read");
        assert!(matches!(wrong, ProviderError::Decode { .. }), "{wrong:?}");

        let right = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::HitAndMiss,
            ReasoningDialect::Inclusive,
            &raw,
        )
        .expect("usage");
        assert_eq!(right.cache_read, 900);
        assert_eq!(right.total_input(), 1050);
    }

    #[test]
    fn a_top_level_cached_tokens_is_read_as_the_cache_read_in_the_details_dialect() {
        // Moonshot reports the hit count at the top level of `usage`.
        let raw = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 10,
            "cached_tokens": 800
        });
        let usage = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Inclusive,
            &raw,
        )
        .expect("usage");
        assert_eq!(usage.cache_read, 800);
        assert_eq!(usage.uncached_input, 200);
        // The details field wins when both are present.
        let both = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 10,
            "cached_tokens": 800,
            "prompt_tokens_details": {"cached_tokens": 600}
        });
        let usage = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Inclusive,
            &both,
        )
        .expect("usage");
        assert_eq!(usage.cache_read, 600);
        // Under the hit-and-miss dialect nothing reads it, so it raises.
        let raw = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 10,
            "prompt_cache_hit_tokens": 0,
            "prompt_cache_miss_tokens": 1000,
            "cached_tokens": 800
        });
        ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::HitAndMiss,
            ReasoningDialect::Inclusive,
            &raw,
        )
        .expect_err("an unread nonzero cached_tokens must raise");
    }

    #[test]
    fn a_cache_named_key_with_a_non_count_value_reads_as_zero() {
        for value in [
            json!(null),
            json!("n/a"),
            json!(false),
            json!({"note": "none", "count": 0}),
        ] {
            let raw = json!({
                "prompt_tokens": 100,
                "completion_tokens": 10,
                "cache_status": value
            });
            ChatCompletionsAdapter::normalize_usage(
                Protocol::Inclusive,
                UsageDialect::Details,
                ReasoningDialect::Inclusive,
                &raw,
            )
            .unwrap_or_else(|e| panic!("{value} must not raise: {e:?}"));
        }
        // A nonzero number, or an object holding one, still raises.
        for value in [json!(3), json!({"hit": {"tokens": 3}})] {
            let raw = json!({
                "prompt_tokens": 100,
                "completion_tokens": 10,
                "cache_status": value
            });
            ChatCompletionsAdapter::normalize_usage(
                Protocol::Inclusive,
                UsageDialect::Details,
                ReasoningDialect::Inclusive,
                &raw,
            )
            .expect_err("a nonzero unrecognized count must raise");
        }
    }

    #[test]
    fn reasoning_exceeding_completion_under_the_inclusive_dialect_raises() {
        // `completion_tokens` is documented to already contain the reasoning share
        // (OpenAI's own shape), so a reported reasoning count above it is
        // impossible, not merely surprising. The old behavior clamped this to zero
        // text output via `saturating_sub`, which is a silent-degradation defect;
        // this must raise instead, the same way the Anthropic adapter and the
        // hit-and-miss dialect above already do.
        let raw = json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "completion_tokens_details": {"reasoning_tokens": 25}
        });
        let err = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Inclusive,
            &raw,
        )
        .expect_err("must raise, not clamp");
        assert!(matches!(err, ProviderError::Decode { .. }), "got {err:?}");
    }

    #[test]
    fn the_additive_reasoning_dialect_does_not_carve_reasoning_out_of_completion() {
        // Under the additive dialect the two counts are independent, so text
        // output is the completion count unchanged and no inconsistency check
        // applies between them.
        let raw = json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "completion_tokens_details": {"reasoning_tokens": 25}
        });
        let usage = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Additive,
            &raw,
        )
        .expect("additive dialect accepts reasoning independent of completion");
        assert_eq!(usage.text_output, 10);
        assert_eq!(usage.reasoning_output, 25);
    }

    #[test]
    fn a_total_larger_than_the_counted_parts_is_recorded_as_billed_reasoning() {
        // Billed reasoning tokens absent from `completion_tokens` show up only in
        // the vendor's total. The call is accepted and the difference is billed
        // as reasoning output, so the figure matches the vendor's total.
        let hidden = json!({
            "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 150
        });
        let usage = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Inclusive,
            &hidden,
        )
        .expect("a total above the parts is a normal response");
        assert_eq!(usage.text_output, 10);
        assert_eq!(usage.reasoning_output, 40);
        assert_eq!(usage.total(), 150);

        // A total below the parts cannot occur and is refused.
        let below = json!({
            "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 90
        });
        let err = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Inclusive,
            &below,
        )
        .expect_err("must raise");
        assert!(matches!(err, ProviderError::Decode { .. }), "got {err:?}");

        // The refused object still yields a floor with the raw object kept.
        let floor = lower_bound_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Inclusive,
            &below,
        )
        .expect("floor");
        assert_eq!((floor.uncached_input, floor.text_output), (100, 10));
        assert_eq!(floor.raw.as_ref().expect("raw")["total_tokens"], 90);

        // A matching total passes, and under the additive dialect the reasoning
        // count joins the sum.
        let matching = json!({
            "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 135,
            "completion_tokens_details": {"reasoning_tokens": 25}
        });
        ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Additive,
            &matching,
        )
        .expect("additive parts sum to the total");
        assert!(
            ChatCompletionsAdapter::normalize_usage(
                Protocol::Inclusive,
                UsageDialect::Details,
                ReasoningDialect::Inclusive,
                &matching,
            )
            .is_err(),
            "the same object is inconsistent when reasoning is inside completion"
        );
    }

    #[test]
    fn an_unrecognized_nonzero_cache_field_raises_rather_than_vanishing() {
        // A vendor reporting a cache-related count under a field name neither
        // dialect reads must not have those tokens silently dropped.
        let raw = json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 0, "cache_hit_input_tokens": 40}
        });
        let err = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Inclusive,
            &raw,
        )
        .expect_err("must raise on an unrecognized cache field");
        assert!(matches!(err, ProviderError::Decode { .. }), "got {err:?}");
    }

    #[test]
    fn an_unrecognized_zero_cache_field_is_not_flagged() {
        let raw = json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 0, "cache_hit_input_tokens": 0}
        });
        let usage = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Inclusive,
            &raw,
        )
        .expect("a zero unrecognized field must not raise");
        assert_eq!(usage.cache_read, 0);
    }

    async fn settlement_after(status: u16) -> Settlement {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_string("upstream said no"))
            .mount(&server)
            .await;
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let err = adapter
            .complete(&Request::new("compatible-model", "hi", 100), &token)
            .await
            .expect_err("must raise");
        assert!(matches!(err, ProviderError::Http { .. }), "{err:?}");
        drop(token);
        ledger.calls().expect("calls")[0].settlement
    }

    #[tokio::test]
    async fn a_server_error_after_the_request_arrived_stays_unreconciled() {
        // A 5xx or a 408 arrives after the provider may have generated, and a
        // vendor bills a failed attempt that ran. Closing it as failed would
        // write that spend off silently.
        assert_eq!(settlement_after(500).await, Settlement::Unreconciled);
        assert_eq!(settlement_after(503).await, Settlement::Unreconciled);
        assert_eq!(settlement_after(408).await, Settlement::Unreconciled);
    }

    #[tokio::test]
    async fn a_rejection_before_generation_closes_as_failed() {
        assert_eq!(settlement_after(400).await, Settlement::Failed);
        assert_eq!(settlement_after(401).await, Settlement::Failed);
        assert_eq!(settlement_after(429).await, Settlement::Failed);
    }

    const OLLAMA_XML_500: &str = r#"{"error":{"message":"XML syntax error on line 8: element <function> closed by </parameter>","type":"api_error","param":null,"code":null}}"#;

    async fn error_for(status: u16, body: &str, stream: bool) -> (ProviderError, Settlement) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let err = if stream {
            run_stream(&adapter, &token).await
        } else {
            adapter
                .complete(&Request::new("compatible-model", "hi", 100), &token)
                .await
        }
        .expect_err("must raise");
        drop(token);
        (err, ledger.calls().expect("calls")[0].settlement)
    }

    #[tokio::test]
    async fn a_5xx_naming_a_tool_call_parse_failure_is_a_typed_malformed_output_error() {
        for stream in [false, true] {
            let (err, settlement) = error_for(500, OLLAMA_XML_500, stream).await;
            assert!(err.is_malformed_model_output(), "stream={stream}: {err:?}");
            assert!(!err.is_route_fault());
            // The model ran: the row stays open like any other 5xx.
            assert_eq!(settlement, Settlement::Unreconciled);
        }
    }

    #[tokio::test]
    async fn other_5xx_bodies_and_other_statuses_stay_plain_http_errors() {
        for stream in [false, true] {
            let (err, _) = error_for(500, "upstream said no", stream).await;
            assert!(matches!(err, ProviderError::Http { .. }), "{err:?}");
            let (err, _) = error_for(400, OLLAMA_XML_500, stream).await;
            assert!(matches!(err, ProviderError::Http { .. }), "{err:?}");
        }
    }

    #[tokio::test]
    async fn a_connection_that_never_opened_closes_as_failed() {
        // Bind and release a port so nothing is listening on it.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();
        let adapter = ChatCompletionsAdapter::new(
            config(&format!("http://127.0.0.1:{port}")),
            UsageDialect::Details,
        )
        .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let err = adapter
            .complete(&Request::new("compatible-model", "hi", 100), &token)
            .await
            .expect_err("must raise");
        assert!(matches!(err, ProviderError::Unsent { .. }), "{err:?}");
        drop(token);
        assert_eq!(
            ledger.calls().expect("calls")[0].settlement,
            Settlement::Failed
        );
    }

    #[tokio::test]
    async fn an_error_object_in_a_200_body_raises_rather_than_an_opaque_decode_failure() {
        // A vendor has been observed returning HTTP 200 with an `error` object
        // in the body instead of a normal completion. This must not fall
        // through to "response has no `model` field" and must stay visible
        // as spend, since the connection did complete.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": {"message": "rate limited mid-generation", "type": "server_error"}
            })))
            .mount(&server)
            .await;
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let err = adapter
            .complete(&Request::new("compatible-model", "hi", 100), &token)
            .await
            .expect_err("must raise");
        match &err {
            ProviderError::Decode { reason, .. } => {
                assert!(reason.contains("rate limited mid-generation"), "{reason}");
            }
            other => panic!("wrong error: {other:?}"),
        }
        assert_eq!(ledger.unreconciled().expect("query").len(), 1);
    }

    #[tokio::test]
    async fn a_remapped_model_raises_and_leaves_the_spend_visible() {
        // The confirmed failure this family is worst for: an unknown model name
        // silently mapped to a cheap tier, returning HTTP 200.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(details_body("their-cheap-tier")),
            )
            .mount(&server)
            .await;
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let err = adapter
            .complete(&Request::new("compatible-model", "hi", 100), &token)
            .await
            .expect_err("must raise");
        match err {
            ProviderError::ModelSubstituted { returned, .. } => {
                assert_eq!(returned, "their-cheap-tier");
            }
            other => panic!("wrong error: {other:?}"),
        }
        assert_eq!(
            ledger.unreconciled().expect("query").len(),
            1,
            "the call billed, so it must stay visible"
        );
    }

    #[tokio::test]
    async fn tool_calls_round_trip_through_the_protocol_s_own_shapes() {
        let server = MockServer::start().await;
        let mut body = details_body("compatible-model");
        body["choices"][0]["finish_reason"] = json!("tool_calls");
        body["choices"][0]["message"] = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":\"src/lib.rs\"}"}
            }]
        });
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let response = adapter
            .complete(&Request::new("compatible-model", "hi", 100), &token)
            .await
            .expect("complete");
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
        let calls = response.tool_uses();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "read_file");
        // Arguments arrive as a JSON string and must come back as structured input.
        assert_eq!(calls[0].2["path"], "src/lib.rs");
    }

    #[test]
    fn unparseable_tool_arguments_are_kept_verbatim_rather_than_dropped() {
        // A worker debugging a malformed tool call needs to see what the model
        // emitted. Silence would be the worst possible report.
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let mut body = details_body("compatible-model");
        body["choices"][0]["message"] = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "c1",
                "function": {"name": "f", "arguments": "{not json"}
            }]
        });
        let response = adapter
            .parse_response("compatible-model", &body)
            .expect("parse");
        let calls = response.tool_uses();
        assert_eq!(calls[0].2, &json!("{not json"));
    }

    #[test]
    fn an_empty_argument_string_is_a_call_with_no_arguments() {
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let mut body = details_body("compatible-model");
        body["choices"][0]["message"] = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id": "c1", "function": {"name": "f", "arguments": ""}}]
        });
        let response = adapter
            .parse_response("compatible-model", &body)
            .expect("parse");
        assert_eq!(response.tool_uses()[0].2, &json!({}));
    }

    #[test]
    fn reasoning_is_not_serialized_into_the_next_request() {
        // `Provider::resends_reasoning` is false: the context estimate leaves
        // reasoning output out of the next prompt because this holds.
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        assert!(!adapter.resends_reasoning());
        let mut request = Request::new("compatible-model", "x", 10);
        request.messages = vec![kgraf_provider::Message {
            role: Role::Assistant,
            content: vec![
                Content::Reasoning {
                    text: "SECRET-REASONING-TEXT".into(),
                },
                Content::text("visible"),
            ],
        }];
        let text = serde_json::to_string(&adapter.to_wire(&request)).expect("wire");
        assert!(text.contains("visible"), "{text}");
        assert!(!text.contains("SECRET-REASONING-TEXT"), "{text}");
    }

    #[test]
    fn a_call_with_no_arguments_is_replayed_as_an_empty_object() {
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let mut request = Request::new("compatible-model", "x", 10);
        request.messages = vec![kgraf_provider::Message {
            role: Role::Assistant,
            content: vec![Content::ToolUse {
                id: "c1".into(),
                name: "f".into(),
                input: json!(""),
                signature: None,
            }],
        }];
        let wire = adapter.to_wire(&request);
        let text = serde_json::to_string(&wire).expect("wire");
        assert!(text.contains(r#""arguments":"{}""#), "{text}");
    }

    #[test]
    fn a_tool_failure_is_stated_in_the_content_as_well_as_flagged() {
        // At least one provider in this family ignores the error flag entirely.
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let mut request = Request::new("compatible-model", "x", 10);
        request.messages = vec![kgraf_provider::Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                tool_use_id: "c1".into(),
                content: "file not found".into(),
                is_error: true,
            }],
        }];
        let wire = adapter.to_wire(&request);
        assert_eq!(wire["messages"][0]["role"], "tool");
        assert!(
            wire["messages"][0]["content"]
                .as_str()
                .expect("string")
                .contains("ERROR"),
            "failure must be legible in the content: {wire}"
        );
    }

    #[test]
    fn unparseable_tool_arguments_go_back_verbatim_not_encoded_twice() {
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let mut request = Request::new("compatible-model", "x", 10);
        request.messages = vec![kgraf_provider::Message {
            role: Role::Assistant,
            content: vec![Content::ToolUse {
                id: "c1".into(),
                name: "f".into(),
                input: json!("{not json"),
                signature: None,
            }],
        }];
        let wire = adapter.to_wire(&request);
        assert_eq!(
            wire["messages"][0]["tool_calls"][0]["function"]["arguments"],
            "{not json"
        );
    }

    #[test]
    fn cache_control_cannot_be_claimed_for_this_protocol() {
        // No variant has a breakpoint field. Accepting a configuration that claims
        // otherwise would let a caller believe breakpoints were being placed while
        // nothing is ever emitted (fail loud).
        let claiming = config("https://example.invalid").honoring_cache_control();
        match ChatCompletionsAdapter::new(claiming, UsageDialect::Details) {
            Err(ProviderError::CapabilityNotHonored { capability, .. }) => {
                assert!(capability.contains("cache"));
            }
            Err(other) => panic!("wrong error: {other:?}"),
            Ok(_) => panic!("a cache-control claim must be refused"),
        }
    }

    #[test]
    fn reasoning_token_reporting_is_opt_in() {
        // Several vendors emit reasoning content without ever reporting a reasoning
        // token count. Declaring the capability then would claim an accounting split
        // that does not exist.
        let quiet =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        assert!(quiet.capabilities().reasoning.is_none());
        let loud =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter")
                .reporting_reasoning_tokens();
        assert!(loud.capabilities().reasoning.is_some());
    }

    /// The shape a local server (Ollama 0.34.4, qwen3.6:35b) answered with
    /// after it dropped the model's apply_patch call: 200, no tool call,
    /// `finish_reason: "stop"`, 718 bytes of reasoning, 1,659 tokens counted.
    #[test]
    fn a_reply_whose_tool_call_the_server_dropped_shows_undelivered_output() {
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let reasoning = format!("{}Let me write the patch.", "p".repeat(695));
        let body = json!({
            "id": "chatcmpl-126",
            "object": "chat.completion",
            "model": "compatible-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "", "reasoning": reasoning},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 8768, "prompt_tokens_details": {"cached_tokens": 7693},
                      "completion_tokens": 1659, "total_tokens": 10427}
        });
        let response = adapter
            .parse_response("compatible-model", &body)
            .expect("parse");
        assert!(response.tool_uses().is_empty());
        assert_eq!(
            adapter.undelivered_output_tokens(&response),
            Some(1659 - 718 - kgraf_provider::message::SPECIAL_TOKEN_ALLOWANCE)
        );

        // The same count with the call delivered: nothing is missing.
        let mut body = body;
        body["choices"][0]["message"]["tool_calls"] = json!([{
            "id": "call_1", "type": "function",
            "function": {"name": "apply_patch",
                         "arguments": serde_json::to_string(&json!({"patch": "x".repeat(4500)}))
                             .expect("args")}
        }]);
        body["choices"][0]["finish_reason"] = json!("tool_calls");
        let response = adapter
            .parse_response("compatible-model", &body)
            .expect("parse");
        assert_eq!(adapter.undelivered_output_tokens(&response), Some(0));
    }

    /// A server that hides reasoning counts it apart; the hidden share is
    /// not taken for lost output.
    #[test]
    fn hidden_reasoning_counted_apart_is_not_undelivered_output() {
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter")
                .reporting_reasoning_tokens();
        let body = json!({
            "id": "c", "object": "chat.completion", "model": "compatible-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "done"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3002, "total_tokens": 3012,
                      "completion_tokens_details": {"reasoning_tokens": 3000}}
        });
        let response = adapter
            .parse_response("compatible-model", &body)
            .expect("parse");
        assert_eq!(adapter.undelivered_output_tokens(&response), Some(0));
    }

    #[test]
    fn reasoning_content_is_carried_separately_from_text() {
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let mut body = details_body("compatible-model");
        body["choices"][0]["message"] = json!({
            "role": "assistant",
            "reasoning_content": "thinking it through",
            "content": "the answer"
        });
        let response = adapter
            .parse_response("compatible-model", &body)
            .expect("parse");
        assert_eq!(
            response.text(),
            "the answer",
            "reasoning must not leak into text"
        );
        assert!(
            response
                .content
                .iter()
                .any(|c| matches!(c, Content::Reasoning { .. })),
            "reasoning must be carried, not discarded"
        );
    }

    #[test]
    fn constructing_against_an_additive_endpoint_raises() {
        let wrong = ProviderConfig::new(
            ADAPTER,
            "https://example.invalid",
            "k",
            vec!["m".into()],
            Protocol::Additive,
        )
        .expect("config");
        match ChatCompletionsAdapter::new(wrong, UsageDialect::Details) {
            Err(ProviderError::CapabilityNotHonored { .. }) => {}
            Err(other) => panic!("wrong error: {other:?}"),
            Ok(_) => panic!("an additive endpoint must not be accepted here"),
        }
    }

    #[test]
    fn an_absent_usage_object_is_distinguished_from_a_zero_token_call() {
        let err = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Inclusive,
            &json!({}),
        )
        .expect_err("must raise");
        assert!(matches!(err, ProviderError::UsageMissing { .. }));

        let zero = ChatCompletionsAdapter::normalize_usage(
            Protocol::Inclusive,
            UsageDialect::Details,
            ReasoningDialect::Inclusive,
            &json!({"prompt_tokens": 0, "completion_tokens": 0}),
        )
        .expect("a genuine zero is fine");
        assert!(zero.is_empty());
    }

    #[test]
    fn a_stop_finish_reason_with_tool_calls_is_a_tool_use() {
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let mut body = details_body("compatible-model");
        body["choices"][0]["message"] = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": "read", "arguments": "{}"}
            }]
        });
        // finish_reason stays "stop", as a Gemini-compatible endpoint sends it.
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        let response = adapter
            .parse_response("compatible-model", &body)
            .expect("parse");
        assert_eq!(response.stop_reason, StopReason::ToolUse);

        // Without tool calls, stop stays a clean end of turn.
        let plain = adapter
            .parse_response("compatible-model", &details_body("compatible-model"))
            .expect("parse");
        assert_eq!(plain.stop_reason, StopReason::EndTurn);
        assert_eq!(stop_reason_for("length", true), StopReason::MaxTokens);
    }

    #[test]
    fn an_unmodeled_finish_reason_is_not_mapped_onto_a_clean_end_turn() {
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let mut body = details_body("compatible-model");
        body["choices"][0]["finish_reason"] = json!("content_filter");
        let response = adapter
            .parse_response("compatible-model", &body)
            .expect("parse");
        assert_eq!(response.stop_reason, StopReason::Other);
        // The normalized `Other` stays explainable: the wire string is kept.
        assert_eq!(response.finish_reason.as_deref(), Some("content_filter"));
    }

    #[tokio::test]
    async fn a_healthy_stream_is_counted_with_the_usage_chunk_and_the_request_id() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"model\":\"compatible-model\",",
            "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"model\":\"compatible-model\",",
            "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"model\":\"compatible-model\",",
            "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"model\":\"compatible-model\",",
            "\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse)
                    .insert_header("x-request-id", "req-stream-1"),
            )
            .mount(&server)
            .await;

        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let response = run_stream(&adapter, &token).await.expect("stream");
        assert_eq!(response.text(), "hello");
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
        assert_eq!(response.usage.total_input(), 10);
        assert_eq!(response.request_id.as_deref(), Some("chatcmpl-1"));

        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Counted);
    }

    #[tokio::test]
    async fn a_stream_that_ends_before_the_usage_chunk_is_left_unreconciled() {
        // The defect this guards against: a `finish_reason` chunk arrived, so
        // a naive reader might treat the answer as complete, but the usage
        // chunk `stream_options.include_usage` asked for never showed up.
        // Settling this as `Counted` would report no usage for a call that
        // demonstrably produced output.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"chatcmpl-2\",\"model\":\"compatible-model\",",
            "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"par\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl-2\",\"model\":\"compatible-model\",",
            "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            // No usage chunk, no [DONE]: the connection simply ends here.
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let err = run_stream(&adapter, &token).await.expect_err("must raise");
        assert!(
            matches!(err, ProviderError::StreamTruncated { .. }),
            "{err:?}"
        );

        let open = ledger.unreconciled().expect("query");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].request_id.as_deref(), Some("chatcmpl-2"));
        assert!(
            ledger.usage_by_model().expect("usage").is_empty(),
            "a truncated stream must not appear in spend totals"
        );
    }

    #[tokio::test]
    async fn a_stream_that_dies_before_any_content_leaves_no_partial_usage_and_no_id_guess() {
        // Nothing at all was ever seen, not even the opening chunk carrying
        // the request id: `leave_unreconciled_with(None, None)`, not a
        // fabricated figure.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&server)
            .await;
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let err = run_stream(&adapter, &token).await.expect_err("must raise");
        assert!(
            matches!(err, ProviderError::StreamTruncated { .. }),
            "{err:?}"
        );
        let open = ledger.unreconciled().expect("query");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].request_id, None);
        assert!(open[0].usage.is_empty());
    }

    #[tokio::test]
    async fn a_mid_stream_error_chunk_raises_and_leaves_the_spend_visible() {
        // A vendor in this family sends a bare `{"error": {...}}` chunk mid-
        // stream instead of a normal choices/delta shape, because the
        // response already committed to HTTP 200. This must not fall through
        // to the ordinary parser and be silently skipped.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"chatcmpl-3\",\"model\":\"compatible-model\",",
            "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"par\"}}]}\n\n",
            "data: {\"error\":{\"message\":\"upstream overloaded\",\"type\":\"server_error\"}}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let err = run_stream(&adapter, &token).await.expect_err("must raise");
        match &err {
            ProviderError::Decode { reason, .. } => {
                assert!(reason.contains("upstream overloaded"), "{reason}");
            }
            other => panic!("wrong error: {other:?}"),
        }
        assert_eq!(
            ledger.unreconciled().expect("query").len(),
            1,
            "a call that streamed content billed, so it must stay visible"
        );
    }

    /// A stream cut mid-answer by an in-band error that names a parse failure
    /// of the model's own output (Ollama's bare `EOF`) is the typed
    /// malformed-output kind, which callers retry, never the text that
    /// streamed before it and never a plain decode failure.
    #[tokio::test]
    async fn a_mid_stream_eof_error_chunk_is_malformed_output_not_answer_text() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"chatcmpl-4\",\"model\":\"compatible-model\",",
            "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"{\\\"serves\\\":true,\\\"reason\\\":\\\"The step\"}}]}\n\n",
            "data: {\"error\":{\"message\":\"EOF\",\"type\":\"api_error\"}}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let err = run_stream(&adapter, &token).await.expect_err("must raise");
        assert!(err.is_malformed_model_output(), "{err:?}");
        match &err {
            ProviderError::ModelOutputMalformed { status, body, .. } => {
                assert_eq!(*status, 200);
                assert!(body.contains("EOF"), "{body}");
                assert!(!body.contains("The step"), "streamed text leaked: {body}");
            }
            other => panic!("wrong error: {other:?}"),
        }
        assert_eq!(ledger.unreconciled().expect("query").len(), 1);
    }

    /// The non-streamed twin: HTTP 200 whose body is an `EOF` error object.
    #[tokio::test]
    async fn an_eof_error_object_in_a_200_body_is_malformed_output() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": {"message": "EOF", "type": "api_error", "param": null, "code": null}
            })))
            .mount(&server)
            .await;
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let err = adapter
            .complete(&Request::new("compatible-model", "hi", 100), &token)
            .await
            .expect_err("must raise");
        assert!(err.is_malformed_model_output(), "{err:?}");
        assert_eq!(ledger.unreconciled().expect("query").len(), 1);
    }

    #[tokio::test]
    async fn a_dropped_stream_future_settles_through_the_guard_with_its_request_id() {
        // The caller cancels mid-stream (a `select!` or, here, a short
        // `tokio::time::timeout`) while the connection is still open and
        // producing chunks. `CallToken`'s own `Drop` would record zero usage
        // and no id if the future were simply abandoned; `StreamGuard` is
        // what preserves the id this adapter had already read from the first
        // chunk. A raw TCP server is used, not `wiremock`, because the
        // property under test is that the client actually drops the
        // in-flight connection rather than merely returning an error.
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (disconnected_tx, disconnected_rx) = tokio::sync::oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            drain_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nx-request-id: req-dropped\r\n\
                      Content-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("headers");
            stream
                .write_all(
                    b"data: {\"id\":\"chatcmpl-dropped\",\"model\":\"compatible-model\",\
                      \"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}}]}\n\n",
                )
                .await
                .expect("first chunk");
            let mut disconnected_tx = Some(disconnected_tx);
            for i in 0..20u32 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let chunk = format!(
                    "data: {{\"id\":\"chatcmpl-dropped\",\"model\":\"compatible-model\",\
                     \"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{i}\"}}}}]}}\n\n"
                );
                if stream.write_all(chunk.as_bytes()).await.is_err() {
                    if let Some(tx) = disconnected_tx.take() {
                        let _ = tx.send(());
                    }
                    return;
                }
            }
        });

        let adapter =
            ChatCompletionsAdapter::new(config(&format!("http://{addr}")), UsageDialect::Details)
                .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(120),
            run_stream(&adapter, &token),
        )
        .await;
        assert!(
            outcome.is_err(),
            "the stream must still be in flight at this short timeout, or the \
             test proves nothing about cancellation"
        );
        // `outcome` going out of scope here drops the still-running future,
        // and with it the byte stream reqwest was reading from.

        tokio::time::timeout(std::time::Duration::from_millis(1000), disconnected_rx)
            .await
            .expect("the server must observe the dropped connection promptly")
            .expect("the sender must not be dropped without sending");
        server.abort();

        drop(token);
        let open = ledger.unreconciled().expect("query");
        assert_eq!(
            open.len(),
            1,
            "a dropped stream future must still leave a reconcilable row"
        );
        assert_eq!(
            open[0].request_id.as_deref(),
            Some("chatcmpl-dropped"),
            "the request id already read before cancellation must survive the drop"
        );
    }

    /// Drain a raw request off the wire without knowing its length in
    /// advance: read until the connection goes quiet for a beat, which for a
    /// client that writes its request and then waits for a response means the
    /// whole request has arrived.
    async fn drain_request(stream: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut buffer = [0_u8; 4096];
        loop {
            match tokio::time::timeout(
                std::time::Duration::from_millis(30),
                stream.read(&mut buffer),
            )
            .await
            {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                Ok(Ok(_)) => continue,
            }
        }
    }

    /// Shared harness for the regression tests below: mounts `sse` as the
    /// whole response body, streams it, and returns the outcome, the
    /// settlement it left, and every event sunk.
    async fn probe(
        sse: &str,
    ) -> (
        Result<Response, ProviderError>,
        Settlement,
        Vec<StreamEvent>,
    ) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse.to_owned()))
            .mount(&server)
            .await;
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let mut events = Vec::new();
        let r = Streaming::stream(
            &adapter,
            &Request::new("compatible-model", "hi", 100),
            &token,
            &mut |e| events.push(e),
        )
        .await;
        let id = token.id().clone();
        drop(token);
        let rec = ledger.call(&id).expect("r").expect("p").settlement;
        (r, rec, events)
    }

    #[tokio::test]
    async fn kimi_shaped_usage_inside_the_choice_is_counted() {
        // Regression for `probe_kimi_usage_inside_choice`:
        // Moonshot/Kimi reports streamed
        // usage inside `choices[0].usage` rather than the top-level chunk
        // OpenAI itself documents. Usage must be accepted from either shape.
        let sse = concat!(
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\",\
             \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (r, settlement, _) = probe(sse).await;
        let response = r.expect("in-choice usage must be accepted, not treated as missing");
        assert_eq!(response.usage.total_input(), 10);
        assert_eq!(response.usage.total_output(), 5);
        assert_eq!(settlement, Settlement::Counted);
    }

    #[tokio::test]
    async fn a_streamed_stop_with_tool_calls_is_a_tool_use() {
        let sse = concat!(
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"a\",\"type\":\"function\",\
             \"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\
             \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
        );
        let (r, _, events) = probe(sse).await;
        let response = r.expect("stream");
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::Finished {
                stop_reason: StopReason::ToolUse
            }
        )));
    }

    #[tokio::test]
    async fn tool_calls_after_a_stop_chunk_still_make_the_turn_a_tool_use() {
        let sse = concat!(
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"a\",\"type\":\"function\",\
             \"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]}}],\
             \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
        );
        let (r, _, events) = probe(sse).await;
        assert_eq!(r.expect("stream").stop_reason, StopReason::ToolUse);
        let finished: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Finished { stop_reason } => Some(*stop_reason),
                _ => None,
            })
            .collect();
        assert_eq!(
            finished,
            vec![StopReason::ToolUse],
            "one Finished event, agreeing with the response"
        );
    }

    #[tokio::test]
    async fn tool_calls_with_no_index_are_kept_as_separate_calls() {
        // Regression for `probe_tool_calls_without_index`: two tool calls
        // distinguished only by `id`, neither fragment carrying `index`.
        // `unwrap_or(0)` previously folded both into one slot, losing the
        // first call and concatenating both calls' arguments into a string
        // that fails to parse as JSON for either.
        let sse = concat!(
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"a\",\"type\":\"function\",\
             \"function\":{\"name\":\"f\",\"arguments\":\"{\\\"x\\\":1}\"}}]}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"b\",\"type\":\"function\",\
             \"function\":{\"name\":\"g\",\"arguments\":\"{\\\"y\\\":2}\"}}]}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\
             \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
        );
        let (r, settlement, _) = probe(sse).await;
        let response = r.expect("stream");
        let calls = response.tool_uses();
        assert_eq!(
            calls.len(),
            2,
            "both calls must survive, not merge into one"
        );
        assert_eq!(calls[0].1, "f");
        assert_eq!(calls[0].2, &json!({"x": 1}));
        assert_eq!(calls[1].1, "g");
        assert_eq!(calls[1].2, &json!({"y": 2}));
        assert_eq!(settlement, Settlement::Counted);
    }

    #[tokio::test]
    async fn an_empty_usage_object_mid_stream_does_not_raise_usage_missing() {
        // Regression for `probe_empty_usage_object_midstream`: `"usage":{}`
        // on an intermediate chunk must read as absent, not as a real,
        // if-empty, usage report that then fails to normalize.
        let sse = concat!(
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"content\":\"hi\"}}],\"usage\":{}}\n\n",
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\
             \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
        );
        let (r, settlement, _) = probe(sse).await;
        let response = r.expect("an empty usage object mid-stream must not fail the call");
        assert_eq!(response.usage.total_input(), 10);
        assert_eq!(settlement, Settlement::Counted);
    }

    #[tokio::test]
    async fn a_model_substituted_on_a_later_chunk_raises_even_when_the_first_chunk_had_none() {
        // Regression for `probe_no_model_field_and_reasoning`: the opening
        // chunk carries no `model` field at all (a real shape, not only a
        // hypothetical one), and a later chunk substitutes a different model.
        // The previous version checked only the first chunk and, finding no
        // `model` there, never checked at all for the rest of the call. This
        // also exercises reasoning collection into the final `Response`.
        let sse = concat!(
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":\
             {\"reasoning_content\":\"think\"}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"cheap-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}],\
             \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
        );
        let (r, settlement, _) = probe(sse).await;
        match r {
            Err(ProviderError::ModelSubstituted { returned, .. }) => {
                assert_eq!(returned, "cheap-model");
            }
            other => panic!("wrong outcome: {other:?}"),
        }
        // The call demonstrably ran (it streamed reasoning content), so the
        // row must stay open for reconciliation, not vanish as a rejection.
        assert_eq!(settlement, Settlement::Unreconciled);
    }

    #[tokio::test]
    async fn reasoning_content_streamed_in_deltas_is_collected_into_the_response() {
        // The sink alone is not enough: a caller reading the returned
        // `Response` rather than watching the sink must also see reasoning,
        // matching `complete()`'s equivalent path, and it must stay out of
        // `text()`.
        let sse = concat!(
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"reasoning_content\":\"thinking \"}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"reasoning_content\":\"it through\"}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"content\":\"the answer\"},\"finish_reason\":\"stop\"}],\
             \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
        );
        let (r, settlement, _) = probe(sse).await;
        let response = r.expect("stream");
        assert_eq!(
            response.text(),
            "the answer",
            "reasoning must not leak into text"
        );
        assert!(
            response
                .content
                .iter()
                .any(|c| matches!(c, Content::Reasoning { text } if text == "thinking it through")),
            "the collected reasoning must be a single joined block: {:?}",
            response.content
        );
        assert_eq!(settlement, Settlement::Counted);
    }

    #[tokio::test]
    async fn a_route_declared_to_send_no_streamed_usage_refuses_before_sending() {
        let server = MockServer::start().await;
        // No mock mounted: a request reaching the server would 404, and the
        // test asserts it never does.
        let adapter = ChatCompletionsAdapter::new(config(&server.uri()), UsageDialect::Details)
            .expect("adapter")
            .streaming_usage_from(StreamedUsageLocation::None);
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "compatible-model", "w", "t")
            .expect("begin");
        let err = run_stream(&adapter, &token).await.expect_err("must refuse");
        assert!(
            matches!(err, ProviderError::CapabilityNotHonored { .. }),
            "{err:?}"
        );
        assert!(
            server
                .received_requests()
                .await
                .expect("recorded")
                .is_empty(),
            "a route declared to send no streamed usage must never be asked to stream"
        );
        drop(token);
        assert_eq!(
            ledger.calls().expect("calls")[0].settlement,
            Settlement::Failed
        );
    }

    #[tokio::test]
    async fn a_kimi_streamed_call_recorded_live_settles_with_one_usage_event() {
        // Replays `tests/fixtures/2026-09-26-kimi-chat-stream.sse`
        // (verified against a real server), a real speaker's stream, not a shape written from
        // documentation. Kimi sends its final usage twice: once inside the
        // finish chunk's `choices[0].usage`, once more top-level on a
        // trailing chunk with empty `choices`; both copies carry identical
        // totals. The adapter must record and sink that usage exactly once,
        // not twice, while still settling on the values both copies agree on.
        let sse = include_str!("../tests/fixtures/2026-09-26-kimi-chat-stream.sse");
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        // Kimi's own catalog entry: Inclusive protocol, Details usage dialect,
        // usage inside the choice (see `crates/kgraf-fleet/src/lib.rs`).
        let adapter_config = ProviderConfig::new(
            ADAPTER,
            server.uri(),
            "test-key",
            vec!["kimi-k3".into()],
            Protocol::Inclusive,
        )
        .expect("config");
        let adapter = ChatCompletionsAdapter::new(adapter_config, UsageDialect::Details)
            .expect("adapter")
            .streaming_usage_from(StreamedUsageLocation::InChoice);
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "kimi-k3", "w", "t")
            .expect("begin");
        let mut events = Vec::new();
        let r = Streaming::stream(
            &adapter,
            &Request::new("kimi-k3", "hi", 100),
            &token,
            &mut |e| events.push(e),
        )
        .await;
        let id = token.id().clone();
        drop(token);
        let settlement = ledger.call(&id).expect("read").expect("present").settlement;

        let response = r.expect("stream");
        let usage_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Usage { .. }))
            .collect();
        assert_eq!(
            usage_events.len(),
            1,
            "Kimi's repeated identical usage must reach the sink exactly once: {events:?}"
        );

        assert_eq!(
            response.usage.uncached_input, 92,
            "prompt as uncached input"
        );
        assert_eq!(response.usage.total_input(), 92);
        assert_eq!(response.usage.reasoning_output, 61);
        assert_eq!(
            response.usage.text_output, 3,
            "64 completion minus 61 reasoning"
        );
        assert_eq!(response.usage.total_output(), 64);
        assert_eq!(
            response.stop_reason,
            StopReason::MaxTokens,
            "finish_reason \"length\""
        );
        assert_eq!(settlement, Settlement::Counted);
    }

    #[tokio::test]
    async fn an_ollama_stream_recorded_live_keeps_its_thinking_text() {
        // Replays `tests/fixtures/2026-10-01-ollama-qwen3.6-chat-stream.sse`,
        // recorded from a real Ollama server (model qwen3.6:35b) over its
        // OpenAI-compatible endpoint. Ollama streams thinking as
        // `delta.reasoning`, not `delta.reasoning_content`; a previous version
        // read only the latter, so the thinking text vanished from transcripts
        // while its tokens were still counted.
        let sse = include_str!("../tests/fixtures/2026-10-01-ollama-qwen3.6-chat-stream.sse");
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        // Ollama's catalog entry: Inclusive protocol, Details dialect, usage on
        // a trailing top-level chunk.
        let adapter_config = ProviderConfig::new(
            ADAPTER,
            server.uri(),
            "test-key",
            vec!["qwen3.6:35b".into()],
            Protocol::Inclusive,
        )
        .expect("config");
        let adapter = ChatCompletionsAdapter::new(adapter_config, UsageDialect::Details)
            .expect("adapter")
            .streaming_usage_from(StreamedUsageLocation::TopLevelChunk);
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "qwen3.6:35b", "w", "t")
            .expect("begin");
        let mut events = Vec::new();
        let r = Streaming::stream(
            &adapter,
            &Request::new("qwen3.6:35b", "hi", 200),
            &token,
            &mut |e| events.push(e),
        )
        .await;
        drop(token);
        let response = r.expect("stream");
        assert_eq!(response.text(), "5", "thinking must not leak into text");
        let reasoning: Vec<&str> = response
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Reasoning { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning.len(),
            1,
            "one joined block: {:?}",
            response.content
        );
        assert!(
            reasoning[0].starts_with("Here's a thinking process:")
                && reasoning[0].contains("2 + 3 = 5"),
            "the whole streamed thinking must be kept: {:?}",
            reasoning[0]
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                StreamEvent::Delta {
                    content: Content::Reasoning { .. }
                }
            )),
            "the live sink must see the thinking too"
        );
        assert_eq!(response.usage.total_output(), 107);
    }

    #[test]
    fn an_ollama_response_recorded_live_keeps_its_thinking_text() {
        // `tests/fixtures/2026-10-01-ollama-qwen3.6-chat-complete.json`, a real
        // non-streamed Ollama answer: thinking arrives as `message.reasoning`.
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/2026-10-01-ollama-qwen3.6-chat-complete.json"
        ))
        .expect("fixture");
        let adapter_config = ProviderConfig::new(
            ADAPTER,
            "https://example.invalid",
            "test-key",
            vec!["qwen3.6:35b".into()],
            Protocol::Inclusive,
        )
        .expect("config");
        let adapter =
            ChatCompletionsAdapter::new(adapter_config, UsageDialect::Details).expect("adapter");
        let response = adapter.parse_response("qwen3.6:35b", &body).expect("parse");
        assert_eq!(response.text(), "5");
        // Ollama reports a reused prompt prefix as `cached_tokens` (18 of 22).
        assert_eq!(response.usage.cache_read, 18);
        assert_eq!(response.usage.uncached_input, 4);
        assert_eq!(response.usage.total_output(), 109);
        assert!(
            response.content.iter().any(|c| matches!(
                c,
                Content::Reasoning { text } if text.starts_with("Here's a thinking process:")
            )),
            "message.reasoning must be carried: {:?}",
            response.content
        );
    }

    #[test]
    fn a_message_carrying_both_reasoning_names_yields_one_block() {
        // A server may send both names while it renames the field; the text
        // must not be carried twice.
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let mut body = details_body("compatible-model");
        body["choices"][0]["message"] = json!({
            "role": "assistant",
            "reasoning_content": "same thought",
            "reasoning": "same thought",
            "content": "the answer"
        });
        let response = adapter
            .parse_response("compatible-model", &body)
            .expect("parse");
        let blocks: Vec<_> = response
            .content
            .iter()
            .filter(|c| matches!(c, Content::Reasoning { .. }))
            .collect();
        assert_eq!(blocks.len(), 1, "{:?}", response.content);
    }

    #[tokio::test]
    async fn a_delta_carrying_both_reasoning_names_is_collected_once() {
        let sse = concat!(
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"reasoning_content\":\"think\",\"reasoning\":\"think\"}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}],\
             \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
        );
        let (r, _, _) = probe(sse).await;
        let response = r.expect("stream");
        assert!(
            response
                .content
                .iter()
                .any(|c| matches!(c, Content::Reasoning { text } if text == "think")),
            "{:?}",
            response.content
        );
    }

    /// A body with reasoning text and the given usage object.
    fn reasoning_body(usage: serde_json::Value) -> serde_json::Value {
        json!({
            "id": "chatcmpl-02",
            "model": "compatible-model",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "5",
                    "reasoning": "2 + 3 is 5, so answer 5"
                }
            }],
            "usage": usage
        })
    }

    #[tokio::test]
    async fn reasoning_text_with_a_total_only_usage_is_marked_unsplit_in_the_ledger() {
        // Ollama and llama.cpp send one `completion_tokens` total that includes
        // the reasoning, and no `reasoning_tokens`. The ledger row must not
        // read as "no reasoning": it keeps the length of the reasoning text and
        // marks the split unknown. Recorded live shape:
        // `tests/fixtures/2026-10-01-ollama-qwen3.6-chat-complete.json`.
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/2026-10-01-ollama-qwen3.6-chat-complete.json"
        ))
        .expect("fixture");
        let reasoning_chars = fixture["choices"][0]["message"]["reasoning"]
            .as_str()
            .expect("reasoning text")
            .chars()
            .count() as u64;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&fixture))
            .mount(&server)
            .await;
        let mut cfg = config(&server.uri());
        cfg.models = vec!["qwen3.6:35b".into()];
        let adapter = ChatCompletionsAdapter::new(cfg, UsageDialect::Details).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "qwen3.6:35b", "w", "t")
            .expect("begin");
        let id = token.id().clone();
        let response = adapter
            .complete(&Request::new("qwen3.6:35b", "hi", 200), &token)
            .await
            .expect("complete");
        drop(token);
        for usage in [
            &response.usage,
            &ledger.call(&id).expect("read").expect("row").usage,
        ] {
            assert_eq!(usage.reasoning_output, 0, "no count was reported");
            assert_eq!(usage.text_output, 109, "the total stays in output");
            assert_eq!(usage.unsplit_reasoning_chars, Some(reasoning_chars));
            assert!(
                usage.reasoning_label().contains("not reported"),
                "{}",
                usage.reasoning_label()
            );
        }
        let by_model = ledger.usage_by_model().expect("sums");
        assert_eq!(by_model[0].1.unsplit_reasoning_chars, Some(reasoning_chars));
        assert!(ledger.verify().expect("verify").ok);
    }

    #[tokio::test]
    async fn a_streamed_total_only_usage_with_reasoning_text_is_marked_unsplit() {
        let sse = concat!(
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"reasoning\":\"think\"}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"compatible-model\",\"choices\":\
             [{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}],\
             \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
        );
        let (r, _, _) = probe(sse).await;
        let usage = r.expect("stream").usage;
        assert_eq!(usage.reasoning_output, 0);
        assert_eq!(usage.text_output, 5);
        assert_eq!(usage.unsplit_reasoning_chars, Some(5));
    }

    #[test]
    fn a_reported_reasoning_count_is_used_and_leaves_no_unsplit_mark() {
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let body = reasoning_body(json!({
            "prompt_tokens": 20,
            "completion_tokens": 30,
            "completion_tokens_details": {"reasoning_tokens": 22}
        }));
        let usage = adapter
            .parse_response("compatible-model", &body)
            .expect("parse")
            .usage;
        assert_eq!(usage.reasoning_output, 22);
        assert_eq!(usage.text_output, 8);
        assert_eq!(usage.unsplit_reasoning_chars, None);
        assert_eq!(usage.reasoning_label(), "22");
    }

    #[test]
    fn a_response_without_reasoning_text_has_no_unsplit_mark() {
        let adapter =
            ChatCompletionsAdapter::new(config("https://example.invalid"), UsageDialect::Details)
                .expect("adapter");
        let mut body = reasoning_body(json!({"prompt_tokens": 20, "completion_tokens": 3}));
        body["choices"][0]["message"] = json!({"role": "assistant", "content": "5"});
        let usage = adapter
            .parse_response("compatible-model", &body)
            .expect("parse")
            .usage;
        assert_eq!(usage.unsplit_reasoning_chars, None);
        assert_eq!(usage.reasoning_label(), "0");
    }
}
