//! The OpenAI **Responses** adapter.
//!
//! Hand-rolled for the same reasons as every other adapter here, and shaped by the
//! same decision record. See the crate docs for why this is one of two OpenAI-family
//! protocols rather than "the OpenAI adapter".
//!
//! Decision 0006 targets this surface for OpenAI proper rather than Chat
//! Completions, because on the current flagship the chat surface requires reasoning
//! effort set to none for function calling to work, so it gives tools or reasoning
//! but never both. That argument is about OpenAI's own models; third-party
//! OpenAI-compatible endpoints do not implement this surface at all and are served
//! by [`crate::chat`].
//!
//! What differs from the Anthropic protocol is entirely in the accounting, which is
//! the point of decision 0006's rule that **normalization is per protocol, not per
//! provider**:
//!
//! - **Usage is inclusive.** The reported input total already contains the cached
//!   portion, so adding the cache fields here would double count. The Anthropic
//!   protocol is additive and adding them there is mandatory. The same six-field
//!   normal form comes out of both.
//! - **Reasoning tokens are reported separately**, in a details object, so they land
//!   in `reasoning_output` rather than being folded into text output.
//! - **Cache writes are reported by newer models only.** Caching is implicit, with
//!   no breakpoint field, so this adapter does not implement `CacheControl`. A cold
//!   prompt on a newer model reports `input_tokens_details.cache_write_tokens`, and
//!   those are carved out of input like cache reads; an older model reports none,
//!   and the field stays zero because it is not reported.
//! - **Nothing is stored upstream.** Every request sends `store: false`. The
//!   protocol stores responses on the vendor's side by default, and one vendor
//!   serving this dialect defaults it to true, which would retain a worker's code
//!   past the call.
//! - **Prior reasoning is not replayed.** kGraf holds no encrypted reasoning
//!   payload for this protocol, and replaying a reasoning summary as visible
//!   assistant text would put words in the model's mouth. Continuity of reasoning
//!   across turns is not implemented here; see decision 0005 before adding it, because it degrades silently across model
//!   families.
//! - **Summaries are requested, never resent.** Decision 0067: this protocol
//!   asks for a reasoning summary (`reasoning.summary: "auto"`) whenever a
//!   request sets a reasoning effort, and the summary that comes back is
//!   surfaced as `Content::Reasoning` for this call only. Anthropic extended
//!   thinking stays off; that adapter is untouched by this decision.

use futures_util::StreamExt;
use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::capability::{Capabilities, Reasoning, StreamEvent, Streaming};
use kgraf_provider::{
    CallToken, Content, ProviderConfig, ProviderError, Request, Response, Role, StopReason,
    StreamGuard,
};

use crate::chat::ReasoningDialect;

/// Rate-limit header capture (decision 0067): see
/// `chat.rs`'s own copy of this helper for the full rationale (a wire-shape
/// twin, kept local to each file rather than shared, since neither module
/// depends on the other and the OpenAI Responses and Chat Completions
/// endpoints use the same header names on the same host).
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
pub const ADAPTER: &str = "openai-responses";

/// An OpenAI-protocol client.
pub struct OpenAiAdapter {
    config: ProviderConfig,
    http: reqwest::Client,
    refuses_temperature_with_reasoning: bool,
    /// Whether `output_tokens` includes or excludes the reasoning share.
    /// Defaults to [`ReasoningDialect::Inclusive`], this protocol's own shape.
    reasoning_dialect: ReasoningDialect,
    /// Azure OpenAI transport, when set: its URL, `api-key` header, model
    /// mapping and content-filter handling replace the defaults.
    azure: Option<crate::azure::AzureTransport>,
}

impl OpenAiAdapter {
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
    /// The configured protocol is asserted: this adapter speaks the inclusive
    /// convention, and constructing it against an additive endpoint would silently
    /// undercount every cached request. Per the fail-loud rule that raises here rather than
    /// producing plausible wrong numbers later.
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        if config.protocol != Protocol::Inclusive {
            return Err(ProviderError::CapabilityNotHonored {
                adapter: ADAPTER,
                capability: "inclusive usage accounting",
            });
        }
        // `connect_timeout`, not `.timeout()`: the latter bounds the whole
        // request including reading the entire response body, which for a
        // stream would cut off a still-healthily-producing answer once
        // `config.timeout` elapsed. `complete` and `stream` each apply
        // `config.timeout` at the call site instead: a whole-call bound for
        // `complete`, a per-chunk idle bound for `stream`.
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
            refuses_temperature_with_reasoning: false,
            reasoning_dialect: ReasoningDialect::Inclusive,
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

    /// The model the response must name for a request to `requested`.
    fn echo_target<'a>(&'a self, requested: &'a str) -> Result<&'a str, ProviderError> {
        match &self.azure {
            Some(transport) => transport.underlying(transport.name(), requested),
            None => Ok(requested),
        }
    }

    /// The configuration this adapter was built with.
    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    /// Declare how this endpoint relates `output_tokens` to
    /// `reasoning_tokens`. Defaults to [`ReasoningDialect::Inclusive`].
    pub fn with_reasoning_dialect(mut self, dialect: ReasoningDialect) -> Self {
        self.reasoning_dialect = dialect;
        self
    }

    /// The reasoning dialect this adapter reads usage under.
    pub fn reasoning_dialect(&self) -> ReasoningDialect {
        self.reasoning_dialect
    }

    /// Refuse a request that sets both temperature and reasoning effort.
    ///
    /// For vendors whose reasoning models reject sampling parameters at any effort
    /// above none. The request is refused before it leaves, rather than having the
    /// temperature stripped silently: a dropped parameter is an ignored parameter
    /// (fail loud), and the caller asked for it.
    pub fn refusing_temperature_with_reasoning(mut self) -> Self {
        self.refuses_temperature_with_reasoning = true;
        self
    }

    /// Checks that run before a request leaves, and so before anything can bill.
    fn preflight(&self, request: &Request) -> Result<(), ProviderError> {
        self.config
            .check_model(self.adapter_name(), &request.model)?;
        self.echo_target(&request.model)?;
        if self.refuses_temperature_with_reasoning
            && request.temperature.is_some()
            && request.reasoning_effort.is_some()
        {
            return Err(ProviderError::CapabilityNotHonored {
                adapter: self.adapter_name(),
                capability: "temperature together with reasoning effort",
            });
        }
        Ok(())
    }

    /// Issue the request and return the raw response, bounded by `bound` for
    /// `send()` alone: reading the body, streaming or not, is bounded
    /// separately by the caller. `bound` is `config.timeout` for a streamed
    /// call and [`ProviderConfig::buffered_timeout`] for a buffered one.
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
                    .post(format!("{}/responses", self.config.base_url))
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
                    reason: format!("no response within {:?} of sending the request", bound),
                });
            }
        };
        Ok(http)
    }

    /// Build the request body.
    ///
    /// Input is a list of items, not of chat messages. Text becomes a role message,
    /// a tool call the model made becomes a `function_call` item and a tool result a
    /// `function_call_output` item, in transcript order. That pairing is enforced by
    /// at least one vendor serving this dialect, which rejects an unmatched call or
    /// output outright. Reasoning is never resent; see the module docs.
    pub fn to_wire(&self, request: &Request) -> serde_json::Value {
        let text_of = |content: &Content| content.as_text().unwrap_or_default().to_owned();
        let mut input: Vec<serde_json::Value> = Vec::new();
        for message in &request.messages {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            // The item-level content part type differs by role: a `user` message's
            // parts are `input_text`, an `assistant` message replayed as input uses
            // `output_text` (the same type this adapter's own `parse_response` reads
            // off an `assistant` output item), per
            // https://platform.openai.com/docs/api-reference/responses/create (the
            // `input` field) and the worked multi-turn examples at
            // https://platform.openai.com/docs/guides/text?api-mode=responses.
            let part_type = match message.role {
                Role::User => "input_text",
                Role::Assistant => "output_text",
            };
            // Accumulated text blocks for one wire item. A single block is still sent
            // as a bare string, the byte-identical shape the prompt-cache prefix
            // tests pin down, and two or more are sent as separate content parts
            // instead of being concatenated, so an instruction and its context slices
            // (or a notice and a steer note) stay distinct on the wire rather than
            // fusing into one run-on string.
            let mut texts: Vec<String> = Vec::new();
            let flush = |texts: &mut Vec<String>, input: &mut Vec<serde_json::Value>| {
                if texts.is_empty() {
                    return;
                }
                let content = match std::mem::take(texts).as_slice() {
                    [one] => serde_json::json!(one),
                    many => serde_json::Value::Array(
                        many.iter()
                            .map(|text| serde_json::json!({"type": part_type, "text": text}))
                            .collect(),
                    ),
                };
                input.push(serde_json::json!({"role": role, "content": content}));
            };
            for block in &message.content {
                match block {
                    Content::Text { text: t, .. } => {
                        if !t.is_empty() {
                            texts.push(t.clone());
                        }
                    }
                    Content::ToolUse {
                        id,
                        name,
                        input: args,
                        ..
                    } => {
                        flush(&mut texts, &mut input);
                        input.push(serde_json::json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            // Arguments are a JSON *string* here, not an object.
                            "arguments": kgraf_provider::tool_arguments_text(args),
                        }));
                    }
                    Content::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        flush(&mut texts, &mut input);
                        // Failure is stated in the content as well, because this
                        // item has no error flag at all.
                        let output = if *is_error {
                            format!("ERROR: {content}")
                        } else {
                            content.clone()
                        };
                        input.push(serde_json::json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": output,
                        }));
                    }
                    Content::Reasoning { .. } => {}
                }
            }
            flush(&mut texts, &mut input);
        }
        let mut body = serde_json::json!({
            "model": request.model,
            "input": input,
            "max_output_tokens": request.max_output_tokens,
            "store": false,
        });
        if !request.system.is_empty() {
            // `instructions` is a single plain-string field on this protocol (no
            // array-of-parts form), so multiple system blocks must be joined into one
            // string; "\n\n" keeps them visibly distinct rather than fusing them.
            body["instructions"] = serde_json::json!(
                request
                    .system
                    .iter()
                    .map(text_of)
                    .collect::<Vec<_>>()
                    .join("\n\n")
            );
        }
        if let Some(temperature) = request.temperature {
            body["temperature"] = serde_json::json!(temperature);
        }
        if let Some(effort) = request.reasoning_effort {
            // `summary: "auto"` asks the model to emit a natural-language summary of
            // its reasoning alongside the effort control. Decision 0067: OpenAI
            // reasoning summaries are on; Anthropic extended thinking stays off, so
            // this field is unique to this adapter. Field name and the streamed
            // event names parsed below (`response.reasoning_summary_text.delta` and
            // the `reasoning` output item's `summary` array) are from
            // https://platform.openai.com/docs/api-reference/responses/create
            // (the `reasoning.summary` request field) and
            // https://platform.openai.com/docs/guides/reasoning#reasoning-summaries
            // (the streamed event names). Not every account is verified for
            // summaries; an account that is not sees this field rejected by the
            // provider, which surfaces as an ordinary non-2xx `ProviderError::Http`
            // below rather than being silently dropped (fail loud). Retrying
            // without the field was considered and rejected: that would silently
            // turn a summary request into one without a summary, which is exactly
            // the silent degradation the fail-loud rule forbids.
            body["reasoning"] = serde_json::json!({"effort": effort, "summary": "auto"});
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        serde_json::json!({
                            "type": "function",
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.input_schema,
                        })
                    })
                    .collect(),
            );
        }
        body
    }

    /// Normalize this protocol's usage object.
    ///
    /// The cached-token count lives in a details sub-object, and reasoning tokens in
    /// another. Both are absent on older responses, which is why every field is read
    /// defensively while the *presence of usage at all* is required.
    pub fn normalize_usage(
        protocol: Protocol,
        reasoning_dialect: ReasoningDialect,
        raw: &serde_json::Value,
    ) -> Result<TokenUsage, ProviderError> {
        Self::normalize_usage_as(ADAPTER, protocol, reasoning_dialect, raw)
    }

    /// [`Self::normalize_usage`] with errors labeled as `adapter`, so a transport
    /// that serves this body under its own name is named correctly.
    fn normalize_usage_as(
        adapter: &'static str,
        protocol: Protocol,
        reasoning_dialect: ReasoningDialect,
        raw: &serde_json::Value,
    ) -> Result<TokenUsage, ProviderError> {
        let field = |name: &str| raw.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
        if raw.get("input_tokens").is_none() && raw.get("output_tokens").is_none() {
            return Err(ProviderError::UsageMissing { adapter });
        }
        let cached = raw
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        // Reported by newer models on a cold prompt, nested or at the top level.
        let cache_write = raw
            .get("input_tokens_details")
            .and_then(|d| d.get("cache_write_tokens"))
            .or_else(|| raw.get("cache_write_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        crate::chat::warn_on_unrecognized_cache_key(
            adapter,
            raw,
            &[
                "input_tokens_details.cached_tokens",
                "input_tokens_details.cache_write_tokens",
                "cache_write_tokens",
            ],
        )?;
        let output = field("output_tokens");
        let reasoning = raw
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        // Output tokens on this protocol are the total; the reasoning share is carved
        // out so the two fields sum back to what the provider reported.
        let text_output = match reasoning_dialect {
            ReasoningDialect::Inclusive => {
                // `output_tokens` is documented to already contain the reasoning
                // share. A reported reasoning count above it is arithmetically
                // impossible under this dialect, so it raises rather than the
                // previous `saturating_sub`, which silently clamped the text share
                // to zero.
                if reasoning > output {
                    return Err(ProviderError::Decode {
                        adapter,
                        reason: format!(
                            "{reasoning} reasoning tokens reported against {output} output \
                             tokens under the inclusive reasoning dialect; the usage object \
                             is inconsistent or the dialect is misconfigured"
                        ),
                    });
                }
                output - reasoning
            }
            ReasoningDialect::Additive => output,
        };
        let hidden = crate::chat::unaccounted_total_tokens(
            adapter,
            raw,
            field("input_tokens"),
            output,
            reasoning,
            reasoning_dialect,
        )?;
        TokenUsage::normalize(
            protocol,
            field("input_tokens"),
            cached,
            // Zero on a model that does not report writes: unreported, not known to
            // be zero. The raw blob keeps the evidence either way.
            cache_write,
            text_output,
            reasoning + hidden,
            Some(raw.clone()),
        )
        .map_err(|source| ProviderError::Usage { adapter, source })
    }

    /// The strict parse of a usage object, or, when a consistency check refuses
    /// it, its counted fields as reported with the raw object kept. A refused
    /// call keeps this on its open row as a floor, never as the spend.
    fn usage_or_lower_bound(
        protocol: Protocol,
        reasoning_dialect: ReasoningDialect,
        raw: &serde_json::Value,
    ) -> Option<TokenUsage> {
        if let Ok(usage) = Self::normalize_usage(protocol, reasoning_dialect, raw) {
            return Some(usage);
        }
        let field = |name: &str| raw.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
        raw.get("input_tokens")
            .or_else(|| raw.get("output_tokens"))?;
        let input = field("input_tokens");
        let output = field("output_tokens");
        let cache_read = raw
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_write = raw
            .get("input_tokens_details")
            .and_then(|d| d.get("cache_write_tokens"))
            .or_else(|| raw.get("cache_write_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let reasoning = raw
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        Some(TokenUsage {
            uncached_input: match protocol {
                Protocol::Additive => input,
                Protocol::Inclusive => input.saturating_sub(cache_read.saturating_add(cache_write)),
            },
            cache_read,
            cache_write,
            text_output: match reasoning_dialect {
                ReasoningDialect::Inclusive => output.saturating_sub(reasoning),
                ReasoningDialect::Additive => output,
            },
            reasoning_output: reasoning,
            raw: Some(raw.clone()),
        })
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
        kgraf_provider::provider::assert_model_echo(
            self.echo_target(requested_model)?,
            returned_model,
        )?;

        let usage = body.get("usage").ok_or(ProviderError::UsageMissing {
            adapter: self.adapter_name(),
        })?;
        let usage = Self::normalize_usage_as(
            self.adapter_name(),
            self.config.protocol,
            self.reasoning_dialect,
            usage,
        )?;

        let mut content = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        if let Some(items) = body.get("output").and_then(|o| o.as_array()) {
            for item in items {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("message") => {
                        if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                            for part in parts {
                                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                    content.push(Content::text(text));
                                }
                            }
                        }
                    }
                    Some("reasoning") => {
                        // The summary is a list of parts; an older shape was a bare
                        // string. Either may be empty, which is not an error.
                        let summary = match item.get("summary") {
                            Some(serde_json::Value::String(text)) => text.clone(),
                            Some(serde_json::Value::Array(parts)) => parts
                                .iter()
                                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            _ => String::new(),
                        };
                        if !summary.is_empty() {
                            content.push(Content::Reasoning { text: summary });
                        }
                    }
                    Some("function_call") => {
                        stop_reason = StopReason::ToolUse;
                        // Arguments arrive as a JSON string. One that will not parse
                        // is kept verbatim, as the Chat Completions adapter does, so
                        // a worker sees what the model emitted rather than `null`.
                        let arguments = item
                            .get("arguments")
                            .and_then(|a| a.as_str())
                            .map(|a| {
                                if a.trim().is_empty() {
                                    return serde_json::json!({});
                                }
                                serde_json::from_str(a)
                                    .unwrap_or(serde_json::Value::String(a.to_owned()))
                            })
                            .unwrap_or(serde_json::Value::Null);
                        content.push(Content::ToolUse {
                            id: item
                                .get("call_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_owned(),
                            name: item
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_owned(),
                            input: arguments,
                            signature: None,
                        });
                    }
                    other => {
                        // Only function tools are ever offered, so any other item is
                        // output this adapter would otherwise drop without a word.
                        return Err(ProviderError::Decode {
                            adapter: self.adapter_name(),
                            reason: format!("unexpected output item type {other:?}"),
                        });
                    }
                }
            }
        }
        // The key is present as null on a complete response, so its presence alone
        // says nothing; the status is what reports truncation.
        let incomplete = body.get("status").and_then(|s| s.as_str()) == Some("incomplete")
            || body
                .get("incomplete_details")
                .is_some_and(|details| !details.is_null());
        if incomplete {
            stop_reason = StopReason::MaxTokens;
        }

        Ok(Response {
            model: returned_model.to_owned(),
            content,
            stop_reason,
            usage,
            request_id: body.get("id").and_then(|i| i.as_str()).map(str::to_owned),
        })
    }
}

#[async_trait::async_trait]
impl kgraf_provider::Provider for OpenAiAdapter {
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
        if let Err(error) = self.preflight(request) {
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
            let error = ProviderError::Http {
                adapter: self.adapter_name(),
                status: status.as_u16(),
                body: text,
            };
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
        // A vendor has been observed returning HTTP 200 with an `error`
        // object in the body instead of a normal response, the non-streaming
        // twin of the `response.failed` and `error` events `stream` below
        // raises on. Checked before `parse_response`, which would otherwise
        // fail on the less specific "no `model` field" decode error.
        if let Some(error) = parsed.get("error").filter(|e| !e.is_null()) {
            token
                .leave_unreconciled_async(request_id.as_deref())
                .await?;
            return Err(ProviderError::Decode {
                adapter: self.adapter_name(),
                reason: format!(
                    "provider returned HTTP 200 with an `error` object in the body: {error}"
                ),
            });
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
                    && let Some(stop_reason) = crate::azure::responses_filter(&parsed)
                {
                    return Err(ProviderError::TurnNotUsable {
                        adapter: self.adapter_name(),
                        stop_reason: stop_reason.to_owned(),
                    });
                }
                Ok(response)
            }
            Err(error) => {
                // Usage the body already carries is kept on the open row, not
                // dropped with the parse error.
                let carried = parsed.get("usage").and_then(|u| {
                    Self::usage_or_lower_bound(self.config.protocol, self.reasoning_dialect, u)
                });
                token
                    .leave_unreconciled_with_async(carried.as_ref(), request_id.as_deref())
                    .await?;
                Err(error)
            }
        }
    }

    fn capabilities(&self) -> Capabilities<'_> {
        // No cache control: this protocol has no breakpoint mechanism and no
        // cache-write count, so the capability is absent rather than stubbed.
        Capabilities::none()
            .with_reasoning(self)
            .with_streaming(self)
    }
}

impl Reasoning for OpenAiAdapter {
    fn reports_reasoning_tokens(&self) -> bool {
        true
    }
}

#[async_trait::async_trait]
impl Streaming for OpenAiAdapter {
    /// Stream a completion.
    ///
    /// This protocol's terminal event carries the **entire final response
    /// object**, in exactly the shape [`OpenAiAdapter::parse_response`]
    /// already parses for a non-streaming call, `response.completed` on a
    /// clean finish, `response.incomplete` when the model was cut off by its
    /// own limits (still a genuine, billed completion, not a stream defect).
    /// So rather than re-deriving content and usage from accumulated deltas,
    /// the terminal event's `response` object is hand off to that same
    /// parser, which keeps the model-echo assertion, the usage normalization
    /// and the output-item handling in one place. Deltas are still sunk as
    /// they arrive, for a caller that wants live text.
    async fn stream(
        &self,
        request: &Request,
        token: &CallToken,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response, ProviderError> {
        if let Err(error) = self.preflight(request) {
            token.close_on_async(&error, None).await?;
            return Err(error);
        }
        let mut body = self.to_wire(request);
        body["stream"] = serde_json::json!(true);

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
            let error = ProviderError::Http {
                adapter: self.adapter_name(),
                status: status.as_u16(),
                body: text,
            };
            token.close_on_async(&error, request_id.as_deref()).await?;
            return Err(error);
        }

        let mut model = request.model.clone();
        let mut started = false;
        let mut stream_request_id = request_id.clone();
        let mut terminal: Option<Response> = None;
        // Set when the Azure content filter acted; raised after the call settles.
        let mut filtered: Option<&'static str> = None;
        let expected_model = self.echo_target(&request.model)?.to_owned();

        // Every exit from here on must still settle the ledger row with
        // whatever usage and request id are already known, including the
        // caller dropping this whole future to cancel (every call is attributed to the ledger). Disarmed only
        // on the single success path below. This protocol reports usage only
        // in its terminal event, so a stream that never reaches one settles
        // with `None`, matching `leave_unreconciled_with`'s stated contract.
        let guard = StreamGuard::new(token);

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

            match event.get("type").and_then(|t| t.as_str()) {
                Some("response.created") => {
                    if let Some(response) = event.get("response") {
                        if let Some(returned) = response.get("model").and_then(|m| m.as_str()) {
                            model = returned.to_owned();
                            kgraf_provider::provider::assert_model_echo(&expected_model, returned)?;
                        }
                        if let Some(id) = response.get("id").and_then(|i| i.as_str()) {
                            stream_request_id = Some(id.to_owned());
                            guard.record_request_id(stream_request_id.as_deref());
                        }
                    }
                    started = true;
                    sink(StreamEvent::Started {
                        model: model.clone(),
                        request_id: stream_request_id.clone(),
                    });
                }
                Some("response.output_text.delta") => {
                    if let Some(fragment) = event.get("delta").and_then(|d| d.as_str()) {
                        sink(StreamEvent::Delta {
                            content: Content::text(fragment),
                        });
                    }
                }
                Some("response.reasoning_summary_text.delta") => {
                    if let Some(fragment) = event.get("delta").and_then(|d| d.as_str()) {
                        sink(StreamEvent::Delta {
                            content: Content::Reasoning {
                                text: fragment.to_owned(),
                            },
                        });
                    }
                }
                Some("response.completed") | Some("response.incomplete") => {
                    let object =
                        event
                            .get("response")
                            .cloned()
                            .ok_or_else(|| ProviderError::Decode {
                                adapter: self.adapter_name(),
                                reason: format!(
                                    "a `{}` event carried no `response` object",
                                    event["type"]
                                ),
                            })?;
                    if !started {
                        // A vendor that skips `response.created` still gets a
                        // `Started` event, so a consumer relying on it for
                        // the model-echo timing is not left without one.
                        if let Some(returned) = object.get("model").and_then(|m| m.as_str()) {
                            model = returned.to_owned();
                        }
                        sink(StreamEvent::Started {
                            model: model.clone(),
                            request_id: stream_request_id.clone(),
                        });
                        started = true;
                    }
                    // Usage first: a parse error (a model echo mismatch, say) must
                    // not drop the figure the event already carries.
                    if let Some(usage_raw) = object.get("usage").filter(|u| !u.is_null())
                        && let Some(usage) = Self::usage_or_lower_bound(
                            self.config.protocol,
                            self.reasoning_dialect,
                            usage_raw,
                        )
                    {
                        guard.record_usage(&usage);
                    }
                    let response = self.parse_response(&request.model, &object)?;
                    guard.record_usage(&response.usage);
                    if self.azure.is_some() {
                        filtered = crate::azure::responses_filter(&object);
                    }
                    if response.request_id.is_some() {
                        stream_request_id = response.request_id.clone();
                        guard.record_request_id(stream_request_id.as_deref());
                    }
                    sink(StreamEvent::Finished {
                        stop_reason: response.stop_reason,
                    });
                    terminal = Some(response);
                }
                Some("response.failed") => {
                    // A failed response still carries a `response` object,
                    // usage and all: the call reached the provider and ran
                    // long enough to fail, which is billed work. Recorded
                    // through the guard before returning, so the row settles
                    // with that real figure instead of whatever partial
                    // amount an earlier delta happened to leave behind
                    // (every call is attributed to the ledger), best-effort, since a malformed usage object
                    // on a failure path is not itself worth failing over.
                    if let Some(object) = event.get("response") {
                        if let Some(usage_raw) = object.get("usage").filter(|u| !u.is_null())
                            && let Some(usage) = Self::usage_or_lower_bound(
                                self.config.protocol,
                                self.reasoning_dialect,
                                usage_raw,
                            )
                        {
                            guard.record_usage(&usage);
                        }
                        if let Some(id) = object.get("id").and_then(|i| i.as_str()) {
                            guard.record_request_id(Some(id));
                        }
                    }
                    let error = event
                        .get("response")
                        .and_then(|r| r.get("error"))
                        .or_else(|| event.get("error"))
                        .cloned()
                        .unwrap_or_default();
                    return Err(ProviderError::Decode {
                        adapter: self.adapter_name(),
                        reason: format!("provider sent a `response.failed` event: {error}"),
                    });
                }
                Some("error") => {
                    return Err(ProviderError::Decode {
                        adapter: self.adapter_name(),
                        reason: format!("provider sent a stream `error` event: {event}"),
                    });
                }
                _ => {}
            }
        }

        let response = match terminal {
            Some(response) => response,
            None => {
                // The connection closed before `response.completed` or
                // `response.incomplete` arrived. This protocol carries no
                // usage before its terminal event, so nothing is known to
                // keep as a partial figure; the guard settles this as
                // `leave_unreconciled_with(None, request_id)` on drop.
                return Err(ProviderError::StreamTruncated {
                    adapter: self.adapter_name(),
                });
            }
        };

        guard.disarm();
        let mut response = response;
        if response.request_id.is_none() {
            response.request_id = stream_request_id;
        }
        token
            .settle_async(&response.usage, response.request_id.as_deref())
            .await?;
        if let Some(stop_reason) = filtered {
            return Err(ProviderError::TurnNotUsable {
                adapter: self.adapter_name(),
                stop_reason: stop_reason.to_owned(),
            });
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_provider::{Ledger, Message, Provider, Settlement};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config(base_url: &str) -> ProviderConfig {
        ProviderConfig::new(
            ADAPTER,
            base_url,
            "test-key",
            vec!["model-x".into()],
            Protocol::Inclusive,
        )
        .expect("config")
    }

    fn body(model: &str) -> serde_json::Value {
        json!({
            "id": "resp_01",
            "model": model,
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "hello"}]
            }],
            "usage": {
                "input_tokens": 1050,
                "input_tokens_details": {"cached_tokens": 900},
                "output_tokens": 25,
                "output_tokens_details": {"reasoning_tokens": 5}
            }
        })
    }

    fn adapter() -> OpenAiAdapter {
        OpenAiAdapter::new(config("http://127.0.0.1:9")).expect("adapter")
    }

    #[test]
    fn nothing_is_stored_upstream() {
        let wire = adapter().to_wire(&Request::new("model-x", "hi", 100));
        assert_eq!(wire["store"], json!(false));
    }

    #[test]
    fn a_tool_loop_round_trips_as_paired_items_and_reasoning_is_not_resent() {
        let mut request = Request::new("model-x", "read the file", 100);
        request.messages.push(Message {
            role: Role::Assistant,
            content: vec![
                Content::Reasoning {
                    text: "private chain of thought".into(),
                },
                Content::text("Reading it."),
                Content::ToolUse {
                    id: "call_1".into(),
                    name: "read".into(),
                    input: json!({"path": "a.rs"}),
                    signature: None,
                },
            ],
        });
        request.messages.push(Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                tool_use_id: "call_1".into(),
                content: "no such file".into(),
                is_error: true,
            }],
        });
        let wire = adapter().to_wire(&request);
        let items = wire["input"].as_array().expect("input");
        assert_eq!(
            items[0],
            json!({"role": "user", "content": "read the file"})
        );
        assert_eq!(
            items[1],
            json!({"role": "assistant", "content": "Reading it."})
        );
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "call_1");
        assert_eq!(items[2]["arguments"], json!({"path": "a.rs"}).to_string());
        assert_eq!(items[3]["type"], "function_call_output");
        assert_eq!(items[3]["call_id"], "call_1");
        assert_eq!(items[3]["output"], "ERROR: no such file");
        assert_eq!(items.len(), 4);
        assert!(
            !wire.to_string().contains("private chain of thought"),
            "reasoning must never be replayed as visible text"
        );
    }

    #[test]
    fn two_message_text_blocks_stay_distinct_on_the_wire() {
        // Mutation check: reverting to a `push_str`/`.join("")` accumulation fuses
        // "an instruction" and "context slices" into one string and this fails.
        let mut request = Request::new("model-x", "hi", 100);
        request.messages = vec![Message {
            role: Role::User,
            content: vec![
                Content::text("an instruction"),
                Content::text("context slices"),
            ],
        }];
        let wire = adapter().to_wire(&request);
        let content = &wire["input"][0]["content"];
        assert_eq!(content.as_array().expect("array of parts").len(), 2);
        assert_eq!(
            content[0],
            json!({"type": "input_text", "text": "an instruction"})
        );
        assert_eq!(
            content[1],
            json!({"type": "input_text", "text": "context slices"})
        );
        assert!(!wire.to_string().contains("an instructioncontext slices"));
    }

    #[test]
    fn two_system_text_blocks_are_joined_with_a_blank_line_not_fused() {
        // `instructions` is a plain string on this protocol, so this is the one site
        // where two blocks are still combined into one string; the separator must
        // keep them visibly distinct. Mutation check: reverting the separator to ""
        // makes this fail.
        let mut request = Request::new("model-x", "hi", 100);
        request.system = vec![Content::text("a notice"), Content::text("a steer note")];
        let wire = adapter().to_wire(&request);
        assert_eq!(wire["instructions"], json!("a notice\n\na steer note"));
    }

    #[test]
    fn a_reasoning_request_asks_for_a_summary() {
        // Decision 0067: OpenAI reasoning summaries are on. Mutation check:
        // dropping `"summary": "auto"` from `to_wire` makes this fail.
        let mut request = Request::new("model-x", "hi", 100);
        request.reasoning_effort = Some(kgraf_provider::message::ReasoningEffort::High);
        let wire = adapter().to_wire(&request);
        assert_eq!(
            wire["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );
    }

    #[test]
    fn a_request_without_reasoning_effort_asks_for_no_summary() {
        // No reasoning effort set means no reasoning models are in play, so the
        // request must not carry a `reasoning` object at all.
        let wire = adapter().to_wire(&Request::new("model-x", "hi", 100));
        assert!(wire.get("reasoning").is_none());
    }

    #[tokio::test]
    async fn temperature_with_reasoning_is_refused_before_sending_and_closes_as_failed() {
        let server = MockServer::start().await;
        // No mock mounted: a request reaching the server would 404, and the test
        // asserts it never does.
        let adapter = OpenAiAdapter::new(config(&server.uri()))
            .expect("adapter")
            .refusing_temperature_with_reasoning();
        let mut request = Request::new("model-x", "hi", 100);
        request.temperature = Some(0.2);
        request.reasoning_effort = Some(kgraf_provider::message::ReasoningEffort::High);
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "model-x", "w", "t")
            .expect("begin");
        let err = adapter
            .complete(&request, &token)
            .await
            .expect_err("must refuse");
        assert!(
            matches!(err, ProviderError::CapabilityNotHonored { .. }),
            "{err:?}"
        );
        assert!(
            server
                .received_requests()
                .await
                .expect("recorded")
                .is_empty()
        );
        drop(token);
        assert_eq!(
            ledger.calls().expect("calls")[0].settlement,
            Settlement::Failed
        );
    }

    #[tokio::test]
    async fn an_unsupported_model_is_refused_without_leaving_a_false_hole() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "typo", "w", "t")
            .expect("begin");
        adapter()
            .complete(&Request::new("typo", "hi", 100), &token)
            .await
            .expect_err("must refuse");
        drop(token);
        assert!(ledger.unreconciled().expect("query").is_empty());
    }

    #[test]
    fn a_null_incomplete_details_is_not_truncation() {
        let mut complete = body("model-x");
        complete["status"] = json!("completed");
        complete["incomplete_details"] = serde_json::Value::Null;
        let response = adapter()
            .parse_response("model-x", &complete)
            .expect("parse");
        assert_eq!(response.stop_reason, StopReason::EndTurn);

        let mut truncated = body("model-x");
        truncated["status"] = json!("incomplete");
        truncated["incomplete_details"] = json!({"reason": "max_output_tokens"});
        let response = adapter()
            .parse_response("model-x", &truncated)
            .expect("parse");
        assert_eq!(response.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn an_unexpected_output_item_raises_rather_than_vanishing() {
        let mut odd = body("model-x");
        odd["output"]
            .as_array_mut()
            .expect("output")
            .push(json!({"type": "web_search_call", "id": "ws_1"}));
        let err = adapter()
            .parse_response("model-x", &odd)
            .expect_err("must raise");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");
    }

    #[test]
    fn cache_writes_are_carved_out_of_inclusive_input() {
        let raw = json!({
            "input_tokens": 2000,
            "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 1800},
            "output_tokens": 10
        });
        let usage =
            OpenAiAdapter::normalize_usage(Protocol::Inclusive, ReasoningDialect::Inclusive, &raw)
                .expect("usage");
        assert_eq!(usage.cache_write, 1800);
        assert_eq!(usage.uncached_input, 200);
        assert_eq!(usage.total_input(), 2000);
    }

    #[test]
    fn reasoning_exceeding_output_under_the_inclusive_dialect_raises() {
        // `output_tokens` is documented to already contain the reasoning share, so a
        // reported reasoning count above it is impossible, not merely surprising.
        // The old behavior clamped this to zero text output via `saturating_sub`;
        // this must raise instead.
        let raw = json!({
            "input_tokens": 100,
            "output_tokens": 10,
            "output_tokens_details": {"reasoning_tokens": 25}
        });
        let err =
            OpenAiAdapter::normalize_usage(Protocol::Inclusive, ReasoningDialect::Inclusive, &raw)
                .expect_err("must raise, not clamp");
        assert!(matches!(err, ProviderError::Decode { .. }), "got {err:?}");
    }

    #[test]
    fn the_additive_reasoning_dialect_does_not_carve_reasoning_out_of_output() {
        let raw = json!({
            "input_tokens": 100,
            "output_tokens": 10,
            "output_tokens_details": {"reasoning_tokens": 25}
        });
        let usage =
            OpenAiAdapter::normalize_usage(Protocol::Inclusive, ReasoningDialect::Additive, &raw)
                .expect("additive dialect accepts reasoning independent of output");
        assert_eq!(usage.text_output, 10);
        assert_eq!(usage.reasoning_output, 25);
    }

    #[test]
    fn an_unrecognized_nonzero_cache_field_raises_rather_than_vanishing() {
        // A vendor reporting a cache-related count under a field name this
        // adapter has never seen must not have those tokens silently dropped.
        let raw = json!({
            "input_tokens": 100,
            "output_tokens": 10,
            "input_tokens_details": {"cached_tokens": 0, "cache_hit_input_tokens": 40}
        });
        let err =
            OpenAiAdapter::normalize_usage(Protocol::Inclusive, ReasoningDialect::Inclusive, &raw)
                .expect_err("must raise on an unrecognized cache field");
        assert!(matches!(err, ProviderError::Decode { .. }), "got {err:?}");
    }

    #[test]
    fn an_unrecognized_zero_cache_field_is_not_flagged() {
        // A vendor that always sends every cache field it knows about, zeroed when
        // unused, must not trip the unrecognized-field check on every call.
        let raw = json!({
            "input_tokens": 100,
            "output_tokens": 10,
            "input_tokens_details": {"cached_tokens": 0, "cache_hit_input_tokens": 0}
        });
        let usage =
            OpenAiAdapter::normalize_usage(Protocol::Inclusive, ReasoningDialect::Inclusive, &raw)
                .expect("a zero unrecognized field must not raise");
        assert_eq!(usage.cache_read, 0);
    }

    #[tokio::test]
    async fn a_completion_is_issued_and_counted_under_the_inclusive_convention() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body("model-x")))
            .mount(&server)
            .await;

        let adapter = OpenAiAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "model-x", "agent-1", "task-1")
            .expect("begin");
        let response = adapter
            .complete(&Request::new("model-x", "hi", 100), &token)
            .await
            .expect("complete");

        assert_eq!(response.text(), "hello");
        // Inclusive: the 900 cached tokens are carved out of the reported 1050.
        assert_eq!(response.usage.uncached_input, 150);
        assert_eq!(response.usage.cache_read, 900);
        assert_eq!(response.usage.total_input(), 1050);
        // Reasoning is carved out of output rather than folded into text.
        assert_eq!(response.usage.reasoning_output, 5);
        assert_eq!(response.usage.text_output, 20);
        assert_eq!(response.usage.total_output(), 25);

        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Counted);
        assert!(record.usage.raw.is_some());
    }

    #[tokio::test]
    async fn adding_the_cache_field_would_double_count_and_does_not() {
        // The inclusive-protocol failure mode, stated as a test: a naive adapter
        // treats the reported input as uncached and adds the cache count on top,
        // reporting 1950 input tokens for a 1050-token call.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body("model-x")))
            .mount(&server)
            .await;
        let adapter = OpenAiAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "model-x", "a", "t")
            .expect("begin");
        let response = adapter
            .complete(&Request::new("model-x", "hi", 10), &token)
            .await
            .expect("complete");
        assert_ne!(response.usage.total_input(), 1950);
        assert_eq!(response.usage.total_input(), 1050);
    }

    #[tokio::test]
    async fn an_error_object_in_a_200_body_raises_rather_than_an_opaque_decode_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": {"message": "rate limited mid-generation", "type": "server_error"}
            })))
            .mount(&server)
            .await;
        let adapter = OpenAiAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "model-x", "a", "t")
            .expect("begin");
        let err = adapter
            .complete(&Request::new("model-x", "hi", 10), &token)
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

    #[test]
    fn unparseable_tool_arguments_are_kept_as_the_raw_text() {
        let adapter = OpenAiAdapter::new(config("https://example.invalid")).expect("adapter");
        let mut response = body("model-x");
        response["output"] = json!([
            {"type": "function_call", "call_id": "c1", "name": "read",
             "arguments": "{\"path\": \"a.rs\""},
            {"type": "function_call", "call_id": "c2", "name": "list",
             "arguments": "{\"dir\": \".\"}"}
        ]);
        let parsed = adapter.parse_response("model-x", &response).expect("parse");
        let calls = parsed.tool_uses();
        assert_eq!(calls[0].2, &json!("{\"path\": \"a.rs\""));
        assert_eq!(calls[1].2, &json!({"dir": "."}));
    }

    #[tokio::test]
    async fn a_substituted_model_raises() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body("some-other-model")))
            .mount(&server)
            .await;
        let adapter = OpenAiAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "model-x", "a", "t")
            .expect("begin");
        let err = adapter
            .complete(&Request::new("model-x", "hi", 10), &token)
            .await
            .expect_err("must raise");
        assert!(matches!(err, ProviderError::ModelSubstituted { .. }));
        let open = ledger.unreconciled().expect("query");
        assert_eq!(open.len(), 1);
        // The body's usage is kept on the open row, not dropped with the error.
        assert_eq!(open[0].usage.total_input(), 1050);
    }

    #[tokio::test]
    async fn a_substituted_model_on_the_terminal_stream_event_keeps_its_usage() {
        let server = MockServer::start().await;
        let completed = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
            body("some-other-model")
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_string(completed))
            .mount(&server)
            .await;
        let adapter = OpenAiAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "model-x", "a", "t")
            .expect("begin");
        let err = run_stream(&adapter, &token).await.expect_err("must raise");
        assert!(
            matches!(err, ProviderError::ModelSubstituted { .. }),
            "{err:?}"
        );
        drop(token);
        let open = ledger.unreconciled().expect("query");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].usage.total_input(), 1050);
    }

    #[test]
    fn constructing_against_the_wrong_protocol_raises() {
        // An additive endpoint behind this adapter would undercount every cached
        // request. Fail loud: raise, do not proceed.
        let wrong = ProviderConfig::new(
            ADAPTER,
            "https://example.invalid",
            "k",
            vec!["m".into()],
            Protocol::Additive,
        )
        .expect("config");
        match OpenAiAdapter::new(wrong) {
            Err(ProviderError::CapabilityNotHonored { .. }) => {}
            Err(other) => panic!("wrong error: {other:?}"),
            Ok(_) => panic!("an additive endpoint must not be accepted here"),
        }
    }

    #[test]
    fn cache_control_is_absent_rather_than_stubbed() {
        let adapter = OpenAiAdapter::new(config("https://example.invalid")).expect("adapter");
        let capabilities = adapter.capabilities();
        assert!(
            capabilities.cache_control.is_none(),
            "a capability this protocol does not have must be absent, not a no-op"
        );
        assert!(capabilities.reasoning.is_some());
        assert!(capabilities.streaming.is_some());
        assert_eq!(capabilities.names(), vec!["streaming", "reasoning"]);
    }

    #[test]
    fn reasoning_tokens_are_carved_out_so_the_output_total_is_preserved() {
        let raw = json!({
            "input_tokens": 10,
            "output_tokens": 100,
            "output_tokens_details": {"reasoning_tokens": 70}
        });
        let usage =
            OpenAiAdapter::normalize_usage(Protocol::Inclusive, ReasoningDialect::Inclusive, &raw)
                .expect("usage");
        assert_eq!(usage.reasoning_output, 70);
        assert_eq!(usage.text_output, 30);
        assert_eq!(usage.total_output(), 100);
    }

    #[test]
    fn a_response_from_before_the_details_objects_existed_still_normalizes() {
        // Three of the eight protocols surveyed changed shape within a year, so an
        // older or leaner usage object must not fail the whole call.
        let raw = json!({"input_tokens": 40, "output_tokens": 9});
        let usage =
            OpenAiAdapter::normalize_usage(Protocol::Inclusive, ReasoningDialect::Inclusive, &raw)
                .expect("usage");
        assert_eq!(usage.uncached_input, 40);
        assert_eq!(usage.text_output, 9);
        assert_eq!(usage.reasoning_output, 0);
    }

    async fn run_stream(
        adapter: &OpenAiAdapter,
        token: &kgraf_provider::CallToken<'_>,
    ) -> Result<Response, ProviderError> {
        Streaming::stream(
            adapter,
            &Request::new("model-x", "hi", 100),
            token,
            &mut |_| {},
        )
        .await
    }

    #[tokio::test]
    async fn a_healthy_stream_hands_the_terminal_event_s_response_to_the_ordinary_parser() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_stream_1\",\"model\":\"model-x\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
        );
        let completed = format!(
            "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
            body("model-x")
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!("{sse}{completed}"))
                    .insert_header("x-request-id", "req-stream-1"),
            )
            .mount(&server)
            .await;

        let adapter = OpenAiAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "model-x", "a", "t")
            .expect("begin");
        let response = run_stream(&adapter, &token).await.expect("stream");
        assert_eq!(response.text(), "hello");
        assert_eq!(response.usage.total_input(), 1050);
        assert_eq!(response.request_id.as_deref(), Some("resp_01"));

        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Counted);
    }

    /// A recorded-shape fixture: `response.created`, a run of
    /// `response.reasoning_summary_text.delta` events carrying the live summary
    /// text, then a `response.completed` whose `response` object's output
    /// includes a `reasoning` item with the same text in its `summary` array, as
    /// documented at
    /// https://platform.openai.com/docs/guides/reasoning#reasoning-summaries.
    /// Mutation check: deleting the `Some("reasoning")` match arm in
    /// `parse_response`, or the `Some("response.reasoning_summary_text.delta")`
    /// arm in `stream`, makes the matching assertion below fail.
    #[tokio::test]
    async fn a_reasoning_summary_streams_as_deltas_and_lands_in_the_terminal_response() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_reasoning_1\",\"model\":\"model-x\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Weigh\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"ing options.\"}\n\n",
        );
        let mut terminal_body = body("model-x");
        terminal_body["output"]
            .as_array_mut()
            .expect("output")
            .insert(
                0,
                json!({
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{"type": "summary_text", "text": "Weighing options."}]
                }),
            );
        let completed =
            format!("data: {{\"type\":\"response.completed\",\"response\":{terminal_body}}}\n\n");
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{sse}{completed}")))
            .mount(&server)
            .await;

        let adapter = OpenAiAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "model-x", "a", "t")
            .expect("begin");

        let mut deltas = String::new();
        let response = Streaming::stream(
            &adapter,
            &Request::new("model-x", "hi", 100),
            &token,
            &mut |event| {
                if let StreamEvent::Delta {
                    content: Content::Reasoning { text },
                } = event
                {
                    deltas.push_str(&text);
                }
            },
        )
        .await
        .expect("stream");

        assert_eq!(
            deltas, "Weighing options.",
            "reasoning summary deltas must reach the sink live"
        );
        assert!(
            response
                .content
                .iter()
                .any(|c| matches!(c, Content::Reasoning { text } if text == "Weighing options.")),
            "the terminal response must carry the summary as Content::Reasoning: {:?}",
            response.content
        );
    }

    #[tokio::test]
    async fn a_stream_that_ends_before_response_completed_is_left_unreconciled() {
        // This protocol reports no usage before its terminal event, so a
        // stream that dies after streaming visible text but before
        // `response.completed` must not be settled as `Counted` with no
        // usage at all, it must raise and stay open with whatever request
        // id was already read.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_dropped\",\"model\":\"model-x\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"par\"}\n\n",
            // The connection ends here: no `response.completed`.
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        let adapter = OpenAiAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "model-x", "a", "t")
            .expect("begin");
        let err = run_stream(&adapter, &token).await.expect_err("must raise");
        assert!(
            matches!(err, ProviderError::StreamTruncated { .. }),
            "{err:?}"
        );

        let open = ledger.unreconciled().expect("query");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].request_id.as_deref(), Some("resp_dropped"));
        assert!(
            ledger.usage_by_model().expect("usage").is_empty(),
            "a truncated stream must not appear in spend totals"
        );
    }

    #[tokio::test]
    async fn an_incomplete_terminal_event_is_a_genuine_completion_not_a_defect() {
        // `response.incomplete` (the model was cut off by its own limits) is
        // still a real, billed completion on this protocol, distinct from the
        // connection dying before any terminal event at all.
        let server = MockServer::start().await;
        let mut incomplete_body = body("model-x");
        incomplete_body["status"] = json!("incomplete");
        incomplete_body["incomplete_details"] = json!({"reason": "max_output_tokens"});
        let sse = format!(
            "data: {{\"type\":\"response.incomplete\",\"response\":{incomplete_body}}}\n\n"
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        let adapter = OpenAiAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "model-x", "a", "t")
            .expect("begin");
        let response = run_stream(&adapter, &token).await.expect("stream");
        assert_eq!(response.stop_reason, StopReason::MaxTokens);

        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Counted);
    }

    #[tokio::test]
    async fn a_response_failed_event_raises_and_leaves_the_spend_visible() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_failed\",\"model\":\"model-x\"}}\n\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"server overloaded\"}}}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;
        let adapter = OpenAiAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "model-x", "a", "t")
            .expect("begin");
        let err = run_stream(&adapter, &token).await.expect_err("must raise");
        match &err {
            ProviderError::Decode { reason, .. } => {
                assert!(reason.contains("server overloaded"), "{reason}");
            }
            other => panic!("wrong error: {other:?}"),
        }
        assert_eq!(ledger.unreconciled().expect("query").len(), 1);
    }

    #[tokio::test]
    async fn a_dropped_stream_future_settles_through_the_guard_with_its_request_id() {
        // The caller cancels mid-stream while the connection is still open. A
        // raw TCP server is used, not `wiremock`, because the property under
        // test is that the client actually drops the in-flight connection.
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (disconnected_tx, disconnected_rx) = tokio::sync::oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            drain_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .await
                .expect("headers");
            stream
                .write_all(
                    b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_dropped_2\",\"model\":\"model-x\"}}\n\n",
                )
                .await
                .expect("first chunk");
            let mut disconnected_tx = Some(disconnected_tx);
            for i in 0..20u32 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let chunk = format!(
                    "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{i}\"}}\n\n"
                );
                if stream.write_all(chunk.as_bytes()).await.is_err() {
                    if let Some(tx) = disconnected_tx.take() {
                        let _ = tx.send(());
                    }
                    return;
                }
            }
        });

        let adapter = OpenAiAdapter::new(config(&format!("http://{addr}"))).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Inclusive, "model-x", "a", "t")
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
            Some("resp_dropped_2"),
            "the request id already read before cancellation must survive the drop"
        );
    }

    /// Drain a raw request off the wire without knowing its length in
    /// advance: read until the connection goes quiet for a beat.
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
}
