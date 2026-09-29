//! The Anthropic-protocol adapter.
//!
//! Hand-rolled on `reqwest` and `serde`, per decision 0006. There is no official
//! Anthropic Rust SDK, and each credible cross-provider crate corrupts exactly the
//! cache and usage data the cost readout depends on, one drops the cache TTL
//! split, one concatenates text blocks, one sums cache writes and reads into a
//! single number, a 20x error. The decisive property is the failure mode: a stale
//! crate parses the response, drops the new field, and hands back a plausible wrong
//! number, which for a product promising a live cost readout is worse than a
//! compile error.
//!
//! This adapter serves **the protocol, not the company**. Any endpoint speaking the
//! Anthropic Messages shape is configured through it, including a second vendor's
//! Anthropic-compatible path, and per decision 0006 the usage convention travels
//! in [`ProviderConfig::protocol`] rather than being inferred here, because one
//! vendor ships both conventions behind a single API key.
//!
//! Three protocol specifics are handled deliberately:
//!
//! - **Usage is additive by default.** `input_tokens` excludes cache reads and
//!   writes, so the true input is the sum of the three. Normalization is in
//!   [`kgraf_core::TokenUsage::normalize`] and keyed on the configured protocol.
//! - **Streaming usage arrives across two events and the delta is cumulative**, so
//!   each field takes the last value seen and is never summed.
//! - **Cache reads and writes are counted separately**, because they are priced
//!   differently and collapsing them is the 20x error above.
//! - **Streaming is read incrementally, never buffered whole.** The body is
//!   consumed as it arrives over the wire, one SSE event at a time, so a
//!   dropped caller aborts the in-flight request and a slow first token still
//!   reaches the sink promptly. A `tool_use` block's `input` arrives as
//!   `input_json_delta` fragments that are not valid JSON until the block's
//!   `content_block_stop`; concatenating and parsing them then is the only
//!   place a tool call becomes usable.

use std::collections::HashMap;

use futures_util::StreamExt;
use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::capability::{CacheControl, Capabilities, StreamEvent, Streaming};
use kgraf_provider::{
    CallToken, Content, ProviderConfig, ProviderError, Request, Response, Role, SseDecoder,
    StopReason, StreamGuard,
};

/// The adapter's stable name.
pub const ADAPTER: &str = "anthropic-messages";

/// Which usage fields `raw` reports. An event that names a field, even at
/// zero, replaces the earlier figure; one that omits it leaves it alone. The
/// output split (text and reasoning) is taken together whenever
/// `output_tokens` is present, since the reasoning share is carved out of it.
fn reported_fields(raw: &serde_json::Value) -> kgraf_core::usage::ReportedFields {
    let has = |name: &str| raw.get(name).is_some_and(|v| v.as_u64().is_some());
    kgraf_core::usage::ReportedFields {
        uncached_input: has("input_tokens"),
        cache_read: has("cache_read_input_tokens"),
        cache_write: has("cache_creation_input_tokens"),
        text_output: has("output_tokens"),
        reasoning_output: has("output_tokens"),
    }
}

/// Stop reasons that end a turn without an answer. The core has no variant for
/// them, so they surface as [`ProviderError::TurnNotUsable`] carrying the
/// provider's own word, after the call's usage is settled.
fn unusable_stop(reason: &str) -> Option<&str> {
    matches!(reason, "refusal" | "model_context_window_exceeded").then_some(reason)
}

/// The most cache breakpoints one request may carry on this wire.
const MAX_CACHE_BREAKPOINTS: usize = 4;

/// An Anthropic-protocol client.
pub struct AnthropicAdapter {
    config: ProviderConfig,
    http: reqwest::Client,
    /// The API version header this build sends.
    api_version: String,
}

impl AnthropicAdapter {
    /// Build an adapter over a configured endpoint.
    ///
    /// The client itself is given only a `connect_timeout` (bounding the TCP/TLS
    /// handshake), never a whole-request `.timeout()`. A whole-request timeout
    /// caps how long reading the *entire* response body may take, which for a
    /// non-streaming call is the right bound but for a stream is exactly the
    /// defect testing found: a stream that is still healthily producing
    /// tokens past `config.timeout` would be cut off mid-answer even though data
    /// keeps arriving. `complete` and `stream` each apply `config.timeout` at
    /// the call site instead (as a whole-call bound for `complete`, and as a
    /// per-chunk idle bound for `stream`) so a long-running stream survives as
    /// long as it keeps producing, and only a genuinely stalled connection times
    /// out.
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
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
            api_version: "2023-06-01".to_owned(),
        })
    }

    /// Override the API version header.
    pub fn with_api_version(mut self, version: impl Into<String>) -> Self {
        self.api_version = version.into();
        self
    }

    /// The configuration this adapter was built with.
    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    /// Refuse a request this adapter cannot serve as asked, before any
    /// connection: a model it does not serve, or a reasoning effort it has no
    /// wire field for. Sending the request with the effort dropped would look
    /// like success and silently change what was asked for (fail loud).
    fn check_request(&self, request: &Request) -> Result<(), ProviderError> {
        self.config.check_model(ADAPTER, &request.model)?;
        if request.reasoning_effort.is_some() {
            return Err(ProviderError::CapabilityNotHonored {
                adapter: ADAPTER,
                capability: "reasoning effort",
            });
        }
        Ok(())
    }

    /// Translate a kGraf request into the wire shape.
    ///
    /// Cache breakpoints are emitted **only where the endpoint is configured as
    /// honoring them**. Emitting them at a provider that drops them costs money
    /// silently and gains nothing, so the default is off.
    fn to_wire(&self, request: &Request, stream: bool) -> serde_json::Value {
        let honor = self.config.honors_cache_control;
        let block = |content: &Content| -> Option<serde_json::Value> {
            match content {
                Content::Text {
                    text,
                    cache_breakpoint,
                } => {
                    let mut value = serde_json::json!({"type": "text", "text": text});
                    if *cache_breakpoint && honor {
                        value["cache_control"] = serde_json::json!({"type": "ephemeral"});
                    }
                    Some(value)
                }
                Content::ToolUse { id, name, input } => Some(serde_json::json!({
                    "type": "tool_use", "id": id, "name": name, "input": input
                })),
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                    "is_error": is_error
                })),
                // Reasoning is not resent as input on this protocol.
                Content::Reasoning { .. } => None,
            }
        };

        let mut messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|message| {
                serde_json::json!({
                    "role": match message.role { Role::User => "user", Role::Assistant => "assistant" },
                    "content": message.content.iter().filter_map(block).collect::<Vec<_>>(),
                })
            })
            .collect();
        // Where caching is honored, the conversation so far is cached too, not
        // only the system prompt: the last block of the last message gets a
        // breakpoint whatever its type (text, tool_result or tool_use; thinking
        // blocks are never sent on this wire). Each tool round then reads the
        // earlier rounds from the cache instead of re-billing them at the full
        // input rate. History caching is new behavior and cheaper per call; the
        // standing induced-demand rule means its effect on call counts is a
        // measurement for the benchmark rerun, not a claim made here.
        if honor {
            let placed = request
                .system
                .iter()
                .chain(request.messages.iter().flat_map(|m| m.content.iter()))
                .filter(|c| {
                    matches!(
                        c,
                        Content::Text {
                            cache_breakpoint: true,
                            ..
                        }
                    )
                })
                .count();
            if placed < MAX_CACHE_BREAKPOINTS
                && let Some(last) = messages
                    .last_mut()
                    .and_then(|m| m.get_mut("content"))
                    .and_then(|c| c.as_array_mut())
                    .and_then(|blocks| blocks.last_mut())
                && last.get("cache_control").is_none()
                && last.get("type").and_then(|t| t.as_str()) != Some("thinking")
            {
                last["cache_control"] = serde_json::json!({"type": "ephemeral"});
            }
        }

        let mut body = serde_json::json!({
            "model": request.model,
            "max_tokens": request.max_output_tokens,
            "messages": messages,
        });
        if !request.system.is_empty() {
            body["system"] =
                serde_json::Value::Array(request.system.iter().filter_map(block).collect());
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
                            "name": tool.name,
                            "description": tool.description,
                            "input_schema": tool.input_schema,
                        })
                    })
                    .collect(),
            );
        }
        if stream {
            body["stream"] = serde_json::json!(true);
        }
        body
    }

    async fn post(&self, body: &serde_json::Value) -> Result<reqwest::Response, ProviderError> {
        let mut builder = self
            .http
            .post(format!("{}/messages", self.config.base_url))
            .header("anthropic-version", &self.api_version)
            .header("content-type", "application/json");
        // A local endpoint takes no credential, and sending an empty header is worse
        // than sending none: some servers reject a malformed `Authorization` outright,
        // which presents as an auth failure against a server that has no auth.
        if let Some(credential) = self.config.credential() {
            builder = builder.header("x-api-key", credential);
        }
        // Bounds only "does the provider ever start responding", not "how long
        // may the whole body take", the client no longer sets that second,
        // stream-hostile bound itself (see `new`'s doc comment).
        let response =
            match tokio::time::timeout(self.config.timeout, builder.json(body).send()).await {
                Ok(result) => result.map_err(send_error)?,
                Err(_) => {
                    return Err(ProviderError::Transport {
                        adapter: ADAPTER,
                        reason: format!(
                            "no response within {:?} of sending the request",
                            self.config.timeout
                        ),
                    });
                }
            };
        let status = response.status();
        // Rate-limit header capture (decision 0067). Recorded
        // whether or not the call succeeded: a 429's own headers are exactly
        // what the per-machine capacity's learned-rate backoff needs. Keyed
        // by base URL, not by the route name this adapter does not know
        // (`kgraf_provider::limits`'s own module doc). Kimi's
        // Anthropic-compatible endpoint is captured through
        // this same path, since `post` is the one place both `complete` and
        // `stream` send a request through.
        {
            let headers = response.headers().clone();
            kgraf_provider::limits::record(
                &self.config.base_url,
                kgraf_provider::limits::parse_anthropic_style(&move |name: &str| {
                    headers
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned)
                }),
            );
        }
        // Decision 0067: a 429 feeds
        // the per-machine capacity's learned rate
        // (`kgraf_capacity::LearnedRate`), read back by the shell's
        // re-measure thread.
        if status.as_u16() == 429 {
            kgraf_provider::limits::record_429_now(&self.config.base_url);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Http {
                adapter: ADAPTER,
                status: status.as_u16(),
                body,
            });
        }
        Ok(response)
    }

    /// Normalize this protocol's usage object.
    ///
    /// Named and public so the test suite can assert the conversion directly, which
    /// is where the additive-versus-inclusive property actually lives.
    pub fn normalize_usage(
        protocol: Protocol,
        raw: &serde_json::Value,
    ) -> Result<TokenUsage, ProviderError> {
        let field = |name: &str| raw.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
        // `input_tokens` absent entirely means no usage arrived, which is different
        // from a zero-token call and must not read as one.
        if raw.get("input_tokens").is_none() && raw.get("output_tokens").is_none() {
            return Err(ProviderError::UsageMissing { adapter: ADAPTER });
        }
        // Reasoning is billed inside `output_tokens`. Anthropic itself does not report
        // the share separately; Moonshot's Anthropic route does, in
        // `output_tokens_details.thinking_tokens`, as its first live call showed
        // (evidence fixture `2026-09-26-kimi-anthropic-live-call.json`). Where it is
        // reported it is carved out, so text and reasoning sum back to output; where
        // it is not, reasoning stays zero rather than being invented.
        let output = field("output_tokens");
        let reasoning = raw
            .get("output_tokens_details")
            .and_then(|d| d.get("thinking_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if reasoning > output {
            return Err(ProviderError::Decode {
                adapter: ADAPTER,
                reason: format!(
                    "{reasoning} thinking tokens reported against {output} output tokens; \
                     the usage object is inconsistent"
                ),
            });
        }
        TokenUsage::normalize(
            protocol,
            field("input_tokens"),
            field("cache_read_input_tokens"),
            field("cache_creation_input_tokens"),
            output - reasoning,
            reasoning,
            Some(raw.clone()),
        )
        .map_err(|source| ProviderError::Usage {
            adapter: ADAPTER,
            source,
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
                    adapter: ADAPTER,
                    reason: "response has no `model` field, so the echo cannot be checked"
                        .to_owned(),
                })?;
        // The assertion decision 0006 requires of every adapter. Three providers were
        // confirmed to silently substitute or ignore, all returning HTTP 200.
        kgraf_provider::provider::assert_model_echo(requested_model, returned_model)?;

        let usage = body
            .get("usage")
            .ok_or(ProviderError::UsageMissing { adapter: ADAPTER })?;
        let usage = Self::normalize_usage(self.config.protocol, usage)?;

        let mut content = Vec::new();
        if let Some(blocks) = body.get("content").and_then(|c| c.as_array()) {
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => content.push(Content::text(
                        block
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default(),
                    )),
                    Some("thinking") => content.push(Content::Reasoning {
                        text: block
                            .get("thinking")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                    }),
                    Some("tool_use") => content.push(Content::ToolUse {
                        id: block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                        name: block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                        input: block
                            .get("input")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    }),
                    // An unmodeled block type is skipped rather than guessed at; the
                    // raw usage blob and the transcript keep the evidence.
                    _ => {}
                }
            }
        }

        Ok(Response {
            model: returned_model.to_owned(),
            content,
            stop_reason: match body.get("stop_reason").and_then(|s| s.as_str()) {
                Some("end_turn") => StopReason::EndTurn,
                Some("max_tokens") => StopReason::MaxTokens,
                Some("tool_use") => StopReason::ToolUse,
                Some("stop_sequence") => StopReason::StopSequence,
                // A server-side tool (this vendor's own web search or code
                // execution) is still running; the turn paused rather than
                // ended, and the caller continues it by sending the response
                // back.
                Some("pause_turn") => StopReason::PauseTurn,
                _ => StopReason::Other,
            },
            usage,
            request_id: body.get("id").and_then(|i| i.as_str()).map(str::to_owned),
        })
    }
}

#[async_trait::async_trait]
impl kgraf_provider::Provider for AnthropicAdapter {
    fn adapter(&self) -> &'static str {
        ADAPTER
    }

    fn protocol(&self) -> Protocol {
        self.config.protocol
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        // Refuse before the request goes out: one provider silently remaps an
        // unknown model name to its cheap model and returns 200.
        if let Err(error) = self.check_request(request) {
            // Refused before sending, so nothing billed; close the row as such
            // rather than letting the dropped token report a hole.
            token.close_on_async(&error, None).await?;
            return Err(error);
        }

        let body = self.to_wire(request, false);
        let http = match self.post(&body).await {
            Ok(http) => http,
            Err(error) => {
                // A connection that never opened, or a rejection before generation,
                // billed nothing; anything later may have. `close_on` decides.
                token.close_on_async(&error, None).await?;
                return Err(error);
            }
        };
        let request_id = http
            .headers()
            .get("request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        // A whole-call bound is correct here: `complete` buffers one response and
        // returns, so unlike `stream` there is no legitimate reason for reading it
        // to take longer than `config.timeout`.
        let text = match tokio::time::timeout(self.config.timeout, http.text()).await {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                // The request reached the provider and may well have billed, but its
                // usage never arrived. That is the unreconciled case, not a failure.
                token
                    .leave_unreconciled_async(request_id.as_deref())
                    .await?;
                return Err(ProviderError::Transport {
                    adapter: ADAPTER,
                    reason: e.to_string(),
                });
            }
            Err(_) => {
                token
                    .leave_unreconciled_async(request_id.as_deref())
                    .await?;
                return Err(ProviderError::Transport {
                    adapter: ADAPTER,
                    reason: format!(
                        "response body did not finish within {:?}",
                        self.config.timeout
                    ),
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
                    adapter: ADAPTER,
                    reason: e.to_string(),
                });
            }
        };
        match self.parse_response(&request.model, &parsed) {
            Ok(mut response) => {
                if response.request_id.is_none() {
                    response.request_id = request_id.clone();
                }
                token
                    .settle_async(&response.usage, response.request_id.as_deref())
                    .await?;
                if let Some(reason) = parsed
                    .get("stop_reason")
                    .and_then(|s| s.as_str())
                    .and_then(unusable_stop)
                {
                    return Err(ProviderError::TurnNotUsable {
                        adapter: ADAPTER,
                        stop_reason: reason.to_owned(),
                    });
                }
                Ok(response)
            }
            Err(error) => {
                // A substituted model or an undecodable body still billed, so the row
                // stays open for reconciliation rather than being written off. Usage
                // the body already carries is kept on the open row, not dropped with
                // the parse error.
                let carried = parsed
                    .get("usage")
                    .and_then(|u| Self::normalize_usage(self.config.protocol, u).ok());
                token
                    .leave_unreconciled_with_async(carried.as_ref(), request_id.as_deref())
                    .await?;
                Err(error)
            }
        }
    }

    fn capabilities(&self) -> Capabilities<'_> {
        let mut capabilities = Capabilities::none().with_streaming(self);
        if self.config.honors_cache_control {
            capabilities = capabilities.with_cache_control(self);
        }
        capabilities
    }
}

impl CacheControl for AnthropicAdapter {
    fn max_breakpoints(&self) -> usize {
        MAX_CACHE_BREAKPOINTS
    }

    fn minimum_cacheable_tokens(&self) -> Option<u32> {
        // Deliberately unknown rather than a number copied from documentation. One
        // rejected option in decision 0006 reproduced a stale minimum-cacheable-prefix
        // table, and a wrong number here is worse than an absent one because it
        // would be acted on.
        None
    }
}

#[async_trait::async_trait]
impl Streaming for AnthropicAdapter {
    async fn stream(
        &self,
        request: &Request,
        token: &CallToken,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response, ProviderError> {
        if let Err(error) = self.check_request(request) {
            // Refused before sending, so nothing billed; close the row as such
            // rather than letting the dropped token report a hole.
            token.close_on_async(&error, None).await?;
            return Err(error);
        }
        let body = self.to_wire(request, true);
        let http = match self.post(&body).await {
            Ok(http) => http,
            Err(error) => {
                token.close_on_async(&error, None).await?;
                return Err(error);
            }
        };
        let request_id = http
            .headers()
            .get("request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        let mut usage = TokenUsage::default();
        let mut saw_usage = false;
        let mut saw_message_stop = false;
        let mut model = request.model.clone();
        let mut stop_reason = StopReason::Other;
        let mut unusable_reason: Option<String> = None;
        let mut stream_request_id = request_id.clone();
        // Anthropic streams one content block at a time, each opened by
        // `content_block_start`, grown by zero or more `content_block_delta`
        // events keyed by `index`, and closed by `content_block_stop`. A `tool_use`
        // block's `input` never arrives whole: it is spread across
        // `input_json_delta` fragments (`partial_json`) that are only valid JSON
        // once fully concatenated, so each block's text is accumulated here and
        // only turned into a [`Content`] block when it closes.
        let mut blocks: HashMap<u64, BlockAccumulator> = HashMap::new();
        // Keyed by index and ordered on read, not on arrival: a block that
        // closes need not close in index order (testing flagged the
        // previous append-on-close ordering as coincidentally correct only
        // because Anthropic happens to close blocks sequentially today).
        let mut content: std::collections::BTreeMap<u64, Content> =
            std::collections::BTreeMap::new();

        // Every exit from here on (an explicit `return Err`, a `?`, or the
        // caller dropping this whole future to cancel) must still settle the
        // ledger row with whatever usage and request id are already known
        // (every call is attributed to the ledger). `CallToken`'s own `Drop` already guarantees the row is
        // never lost, but it always records zero usage; this guard is what
        // keeps the partial usage and id testing found being discarded
        // on every early exit. Disarmed only on the single success path below.
        let guard = StreamGuard::new(token);
        // Recorded immediately, not only once `message_start`'s own `id` field
        // arrives: every failure before that point
        // (the model-echo check below included) closed the row with no
        // request id at all, even though the transport-level header was
        // already in hand. `message_start`'s id, once it arrives, is the more
        // specific handle and overwrites this below.
        guard.record_request_id(request_id.as_deref());

        // The body is read as a byte stream, not buffered whole with `.text()`.
        // A caller that drops the returned future (a cancel) drops `byte_stream`
        // with it, which drops the underlying `hyper` body and aborts the
        // in-flight HTTP request, reqwest gives this for free from a stream, not
        // from a `.text()` future already resolved. Buffering the whole body
        // first was also the reason a mid-stream cancel presented as "nothing
        // happened until the connection closed": every delta, tool call and
        // usage event was withheld from `sink` until after the response ended.
        let mut decoder = SseDecoder::new(ADAPTER);
        let mut byte_stream = http.bytes_stream();
        let mut pending: std::collections::VecDeque<serde_json::Value> =
            std::collections::VecDeque::new();
        loop {
            let event = match pending.pop_front() {
                Some(event) => event,
                None => {
                    // No whole-call timeout applies to a stream (see `new`'s doc
                    // comment): only a gap this long between chunks, however far
                    // into an otherwise healthy stream, counts as stalled.
                    match tokio::time::timeout(self.config.timeout, byte_stream.next()).await {
                        Ok(Some(Ok(chunk))) => {
                            pending.extend(decoder.push(&chunk)?);
                            continue;
                        }
                        Ok(Some(Err(e))) => {
                            // The connection was live and may have generated before
                            // it broke, so this is the unreconciled/cancelled case,
                            // not a provably-unbilled one.
                            return Err(ProviderError::Transport {
                                adapter: ADAPTER,
                                reason: e.to_string(),
                            });
                        }
                        Ok(None) => {
                            // The connection closed. Anthropic always terminates an
                            // event with a blank line, but flushing here catches a
                            // server that closes right after the last event's data
                            // line without it, rather than silently dropping that
                            // event.
                            pending.extend(decoder.finish()?);
                            match pending.pop_front() {
                                Some(event) => event,
                                None => break,
                            }
                        }
                        Err(_) => {
                            return Err(ProviderError::Transport {
                                adapter: ADAPTER,
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
                Some("message_start") => {
                    let message = event.get("message").cloned().unwrap_or_default();
                    if let Some(started) = message.get("model").and_then(|m| m.as_str()) {
                        model = started.to_owned();
                        // Check the echo as early as the protocol allows, so a
                        // substituted model is caught before the output is consumed.
                        kgraf_provider::provider::assert_model_echo(&request.model, started)?;
                    }
                    if let Some(id) = message.get("id").and_then(|i| i.as_str()) {
                        stream_request_id = Some(id.to_owned());
                        guard.record_request_id(stream_request_id.as_deref());
                    }
                    sink(StreamEvent::Started {
                        model: model.clone(),
                        request_id: stream_request_id.clone(),
                    });
                    if let Some(raw) = message.get("usage") {
                        // `message_start` carries the input side, including cache
                        // counts. It is the half a consumer that only reads
                        // `message_delta` would miss.
                        let partial = Self::normalize_usage(self.config.protocol, raw)?;
                        usage.merge_cumulative_reported(&partial, reported_fields(raw));
                        saw_usage = true;
                        guard.record_usage(&usage);
                        sink(StreamEvent::Usage {
                            usage: usage.clone(),
                        });
                    }
                }
                Some("content_block_start") => {
                    let index = block_index(&event);
                    if blocks.contains_key(&index) || content.contains_key(&index) {
                        // A repeated `content_block_start` for an index that is
                        // still open, or already closed, is a protocol
                        // violation: silently overwriting it would discard
                        // whatever that block had already accumulated (fail loud).
                        return Err(ProviderError::Decode {
                            adapter: ADAPTER,
                            reason: format!(
                                "content_block_start received twice for content \
                                 block {index}"
                            ),
                        });
                    }
                    if let Some(block) = event.get("content_block") {
                        match block.get("type").and_then(|t| t.as_str()) {
                            Some("tool_use") => {
                                blocks.insert(
                                    index,
                                    BlockAccumulator::ToolUse {
                                        id: block
                                            .get("id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or_default()
                                            .to_owned(),
                                        name: block
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or_default()
                                            .to_owned(),
                                        // Anthropic always sends `{}` here for a
                                        // tool with no arguments, and then emits
                                        // zero `input_json_delta` fragments for it.
                                        // Parsing an empty accumulated string as
                                        // JSON always fails, so this is the
                                        // fallback `BlockAccumulator::finish` uses
                                        // when no fragment ever arrived, every no-argument tool call
                                        // was previously undecodable because this
                                        // field was discarded.
                                        start_input: block
                                            .get("input")
                                            .cloned()
                                            .unwrap_or_else(|| serde_json::json!({})),
                                        json: String::new(),
                                    },
                                );
                            }
                            Some("thinking") => {
                                blocks.insert(index, BlockAccumulator::Reasoning(String::new()));
                            }
                            Some("text") => {
                                blocks.insert(index, BlockAccumulator::Text(String::new()));
                            }
                            Some("redacted_thinking") => {
                                // The model reasoned but Anthropic redacted it for
                                // safety; the block carries an encrypted `data`
                                // blob and no incremental deltas at all. There is
                                // no `Content` variant for it (that type lives in
                                // `kgraf-provider`, out of this crate's reach)
                                // so it is surfaced as `Content::Reasoning`
                                // carrying the opaque data verbatim: legible as
                                // "something was redacted here", never silently
                                // dropped, which is what happened before this
                                // arm existed.
                                let data = block
                                    .get("data")
                                    .and_then(|d| d.as_str())
                                    .unwrap_or_default();
                                content.insert(
                                    index,
                                    Content::Reasoning {
                                        text: format!("[redacted_thinking:{data}]"),
                                    },
                                );
                            }
                            // `server_tool_use` (a server-side tool call, e.g.
                            // a built-in web search) and its result blocks
                            // (`web_search_tool_result` and siblings) accumulate
                            // their input the same way an ordinary `tool_use`
                            // block does, via `input_json_delta`, but
                            // `complete`'s non-streaming parser already skips
                            // this and every other unmodeled block type
                            // outright, and streaming stays consistent with it
                            // rather than inventing a shape for it here. Any
                            // other unmodeled type (a future protocol addition)
                            // gets the same treatment. Tracked as `Skipped`
                            // rather than left out of `blocks` entirely, so its
                            // deltas are recognized as belonging to a
                            // deliberately-skipped block rather than raising as
                            // an orphaned fragment.
                            _ => {
                                blocks.insert(index, BlockAccumulator::Skipped);
                            }
                        }
                    }
                }
                Some("content_block_delta") => {
                    let index = block_index(&event);
                    let delta = event.get("delta").cloned().unwrap_or_default();
                    match delta.get("type").and_then(|t| t.as_str()) {
                        Some("thinking_delta") => {
                            let fragment = delta
                                .get("thinking")
                                .and_then(|t| t.as_str())
                                .unwrap_or_default();
                            match blocks
                                .entry(index)
                                .or_insert_with(|| BlockAccumulator::Reasoning(String::new()))
                            {
                                BlockAccumulator::Reasoning(buffer) => buffer.push_str(fragment),
                                // The index is open as something else entirely
                                // (a `text` or `tool_use` block, say). Silently
                                // dropping the fragment, as a previous version
                                // did, would understate that block's reasoning
                                // with no sign anything was lost (fail loud).
                                _ => {
                                    return Err(ProviderError::Decode {
                                        adapter: ADAPTER,
                                        reason: format!(
                                            "thinking_delta for content block {index}, \
                                             which is not an open thinking block"
                                        ),
                                    });
                                }
                            }
                            sink(StreamEvent::Delta {
                                content: Content::Reasoning {
                                    text: fragment.to_owned(),
                                },
                            });
                        }
                        // Accompanies a `thinking` block once its content is
                        // fully streamed: a cryptographic signature over the
                        // thinking text, not text itself. Falling into the
                        // catch-all below previously sank a spurious empty
                        // `Content::text("")` for every extended-thinking call
                        // and tried to open a `Text` accumulator over the same
                        // index a `Reasoning` accumulator already owned.
                        // Deliberately ignored: there is nowhere in `Content`
                        // to carry a signature, and dropping it changes nothing
                        // observable about the answer.
                        Some("signature_delta") => {}
                        Some("input_json_delta") => {
                            // A `tool_use` fragment is not sunk on its own: a
                            // `partial_json` chunk is not valid JSON by itself, so
                            // there is nothing legible to hand a consumer until
                            // the block closes and the concatenation is parsed.
                            let fragment = delta
                                .get("partial_json")
                                .and_then(|t| t.as_str())
                                .unwrap_or_default();
                            match blocks.get_mut(&index) {
                                Some(BlockAccumulator::ToolUse { id, name, json, .. }) => {
                                    if json.len() + fragment.len() > MAX_TOOL_INPUT_BYTES {
                                        return Err(ProviderError::Decode {
                                            adapter: ADAPTER,
                                            reason: format!(
                                                "tool_use `{name}` (id `{id}`) accumulated \
                                                 more than {MAX_TOOL_INPUT_BYTES} bytes of \
                                                 input_json_delta fragments without closing; \
                                                 treated as corrupt rather than buffered \
                                                 without limit"
                                            ),
                                        });
                                    }
                                    json.push_str(fragment);
                                }
                                // A `server_tool_use` block (or another
                                // deliberately-skipped block type) accumulates
                                // its input the same way, but nothing is kept
                                // for it, consistent with `complete` silently
                                // omitting the whole block.
                                Some(BlockAccumulator::Skipped) => {}
                                // Either this index was never opened, or it was
                                // opened as something other than `tool_use`. Both
                                // are a protocol violation this adapter cannot
                                // make sense of, so it raises rather than
                                // silently dropping a fragment of a tool call's
                                // input (fail loud).
                                _ => {
                                    return Err(ProviderError::Decode {
                                        adapter: ADAPTER,
                                        reason: format!(
                                            "input_json_delta for content block {index}, \
                                             which is not an open tool_use block"
                                        ),
                                    });
                                }
                            }
                        }
                        Some("text_delta") => {
                            let fragment = delta
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or_default();
                            match blocks
                                .entry(index)
                                .or_insert_with(|| BlockAccumulator::Text(String::new()))
                            {
                                BlockAccumulator::Text(buffer) => buffer.push_str(fragment),
                                // The index is open as something else (a
                                // `thinking` or `tool_use` block, say). Silently
                                // dropping the fragment or, worse, overwriting
                                // whichever accumulator was already there, would
                                // lose text with no sign anything was lost.
                                _ => {
                                    return Err(ProviderError::Decode {
                                        adapter: ADAPTER,
                                        reason: format!(
                                            "text_delta for content block {index}, which \
                                             is not an open text block"
                                        ),
                                    });
                                }
                            }
                            sink(StreamEvent::Delta {
                                content: Content::text(fragment),
                            });
                        }
                        // An unmodeled delta type (a future protocol addition),
                        // skipped like an unmodeled block type above rather than
                        // guessed at.
                        _ => {}
                    }
                }
                Some("content_block_stop") => {
                    let index = block_index(&event);
                    if let Some(accumulated) = blocks.remove(&index) {
                        // `None` only for a deliberately-skipped block type
                        // (`server_tool_use` and siblings): nothing to sink or
                        // keep, consistent with `complete` omitting it too.
                        if let Some(block) = accumulated.finish()? {
                            if let Content::ToolUse { .. } = &block {
                                // Text and thinking blocks were already sunk
                                // incrementally, per delta; a tool call could not
                                // be, so its one and only sink is here, complete.
                                sink(StreamEvent::Delta {
                                    content: block.clone(),
                                });
                            }
                            content.insert(index, block);
                        }
                    }
                }
                Some("message_delta") => {
                    if let Some(reason) = event
                        .get("delta")
                        .and_then(|d| d.get("stop_reason"))
                        .and_then(|s| s.as_str())
                    {
                        unusable_reason = unusable_stop(reason).map(str::to_owned);
                        stop_reason = match reason {
                            "end_turn" => StopReason::EndTurn,
                            "max_tokens" => StopReason::MaxTokens,
                            "tool_use" => StopReason::ToolUse,
                            "stop_sequence" => StopReason::StopSequence,
                            // A server-side tool (this vendor's own web search
                            // or code execution) is still running; the turn
                            // paused rather than ended, and the caller
                            // continues it by sending the response back.
                            "pause_turn" => StopReason::PauseTurn,
                            _ => StopReason::Other,
                        };
                    }
                    if let Some(raw) = event.get("usage") {
                        // Cumulative: merge field by field, taking the last value.
                        // Summing here is the common bug and would double the output
                        // count on any stream carrying two usage events.
                        let partial = Self::normalize_usage(self.config.protocol, raw)?;
                        usage.merge_cumulative_reported(&partial, reported_fields(raw));
                        saw_usage = true;
                        guard.record_usage(&usage);
                        sink(StreamEvent::Usage {
                            usage: usage.clone(),
                        });
                    }
                }
                Some("message_stop") => {
                    saw_message_stop = true;
                    sink(StreamEvent::Finished { stop_reason });
                }
                Some("error") => {
                    // A mid-stream failure the provider itself reports (e.g.
                    // `overloaded_error`), arriving as its own SSE event rather
                    // than an HTTP error status, because the response already
                    // committed to 200 before generation started. Previously
                    // matched the catch-all below and was silently ignored,
                    // which testing found let a failed generation return
                    // `Ok` with whatever partial content had streamed so far.
                    let error = event.get("error").cloned().unwrap_or_default();
                    let kind = error
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("unknown");
                    let message = error
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("(no message)");
                    return Err(ProviderError::Decode {
                        adapter: ADAPTER,
                        reason: format!("provider sent a stream `error` event ({kind}): {message}"),
                    });
                }
                _ => {}
            }
        }

        if !saw_usage {
            // One provider states explicitly that an interrupted stream may never
            // deliver its usage chunk at all. The call billed; the row stays open
            // with its request id so it can be reconciled, and the accounting rule is not
            // violated by a silently dropped cost. Checked first: with no usage
            // whatsoever, `UsageMissing` says exactly what is known and nothing
            // more, rather than characterizing the truncation like the next
            // check below, which needs a partial figure to report.
            return Err(ProviderError::UsageMissing { adapter: ADAPTER });
        }
        if !saw_message_stop {
            // The connection closed without ever reaching `message_stop`, but
            // some usage was already seen (`message_start` carries the input
            // side before any output exists). Every block already closed is
            // genuine, but nothing guarantees the model was done: returning
            // `Ok` here, as a previous version did, hands back truncated
            // content as though it were a complete, ordinary turn, and
            // settling it as `Counted` would bill the partial figure as if it
            // were final (both are defects). The guard
            // settles this as unreconciled with that figure kept as a stated
            // lower bound.
            return Err(ProviderError::StreamTruncated { adapter: ADAPTER });
        }
        if !blocks.is_empty() {
            // A content block that opened but never closed: `message_stop`
            // arrived, so the stream claims to be complete, but a provider
            // sent malformed framing. Whatever text or tool call that block
            // was building is not in `content`, so returning `Ok` would
            // silently hand back an incomplete answer.
            return Err(ProviderError::Decode {
                adapter: ADAPTER,
                reason: format!(
                    "{} content block(s) never received a content_block_stop \
                     before the stream ended",
                    blocks.len()
                ),
            });
        }

        guard.disarm();
        token
            .settle_async(&usage, stream_request_id.as_deref())
            .await?;
        if let Some(stop_reason) = unusable_reason {
            return Err(ProviderError::TurnNotUsable {
                adapter: ADAPTER,
                stop_reason,
            });
        }
        Ok(Response {
            model,
            content: content.into_values().collect(),
            stop_reason,
            usage,
            request_id: stream_request_id,
        })
    }
}

/// A content block accumulating across `content_block_delta` events, keyed by
/// the stream's `index` field until its `content_block_stop` arrives.
enum BlockAccumulator {
    Text(String),
    Reasoning(String),
    ToolUse {
        id: String,
        name: String,
        /// The `input` object `content_block_start` carried. Anthropic always
        /// sends `{}` here, even for a tool with no arguments, and a tool
        /// called with no arguments then emits zero `input_json_delta`
        /// fragments, so this is the fallback `finish` uses whenever no
        /// fragment ever arrived, rather than trying to parse an empty string.
        start_input: serde_json::Value,
        /// The concatenation of every `input_json_delta.partial_json`
        /// fragment seen for this block. It is not valid JSON until the block
        /// closes, so it is parsed only in [`BlockAccumulator::finish`].
        json: String,
    },
    /// A block type this adapter deliberately does not model (a server-side
    /// tool call (`server_tool_use`), its result (`web_search_tool_result`
    /// and siblings), or any future type this build predates) tracked here
    /// rather than left entirely absent from `blocks`. `complete`'s
    /// non-streaming parser already skips these block types outright; the
    /// point of tracking one here, rather than ignoring its index, is so a
    /// `content_block_delta` for it (a `server_tool_use` block accumulates
    /// its input via `input_json_delta`, exactly like an ordinary `tool_use`
    /// block) is recognized as belonging to a deliberately-skipped block, not
    /// mistaken for an orphaned fragment targeting an index that was never
    /// opened at all.
    Skipped,
}

/// Hard cap on how many bytes of `input_json_delta.partial_json` fragments a
/// single `tool_use` block may accumulate. Protects against a provider that
/// never closes the block: without this, `json` would grow without bound
/// rather than raising. Generous relative to any real tool call's arguments.
const MAX_TOOL_INPUT_BYTES: usize = 4 * 1024 * 1024;

impl BlockAccumulator {
    /// Turn a closed block into its [`Content`].
    ///
    /// A `tool_use` block whose concatenated fragments do not parse as JSON is a
    /// typed [`ProviderError::Decode`], never a silently empty input (fail loud): an
    /// agent acting on a tool call with fabricated-empty arguments is worse than
    /// one that never receives the call.
    /// `Ok(None)` only for [`BlockAccumulator::Skipped`]: a deliberately
    /// unmodeled block contributes nothing to the final `content`, consistent
    /// with `complete`'s non-streaming parser silently omitting the same
    /// block types.
    fn finish(self) -> Result<Option<Content>, ProviderError> {
        match self {
            BlockAccumulator::Text(text) => Ok(Some(Content::text(text))),
            BlockAccumulator::Reasoning(text) => Ok(Some(Content::Reasoning { text })),
            BlockAccumulator::ToolUse {
                id,
                name,
                start_input,
                json,
            } => {
                let input = if json.trim().is_empty() {
                    start_input
                } else {
                    serde_json::from_str(&json).map_err(|e| ProviderError::Decode {
                        adapter: ADAPTER,
                        reason: format!(
                            "tool_use `{name}` (id `{id}`) accumulated invalid JSON input \
                             from its input_json_delta fragments: {e}"
                        ),
                    })?
                };
                Ok(Some(Content::ToolUse { id, name, input }))
            }
            BlockAccumulator::Skipped => Ok(None),
        }
    }
}

/// The `index` field every content-block event carries, identifying which
/// concurrently-open block a delta or a stop belongs to. Anthropic streams
/// blocks sequentially rather than interleaved, but the index is still the
/// documented way to associate an event with its block, so it is honored
/// rather than assumed to be strictly increasing from zero.
fn block_index(event: &serde_json::Value) -> u64 {
    event.get("index").and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Classify a failure to send.
///
/// Only a connection that never opened is known not to have reached the provider.
/// A timeout or a reset after the request was written may have been billed in full.
fn send_error(e: reqwest::Error) -> ProviderError {
    if e.is_connect() || e.is_builder() {
        ProviderError::Unsent {
            adapter: ADAPTER,
            reason: e.to_string(),
        }
    } else {
        ProviderError::Transport {
            adapter: ADAPTER,
            reason: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_provider::streaming::MAX_EVENT_BYTES;
    use kgraf_provider::{Ledger, Provider, Settlement};
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config(base_url: &str) -> ProviderConfig {
        ProviderConfig::new(
            ADAPTER,
            base_url,
            "test-key",
            vec!["model-a".into(), "model-b".into()],
            Protocol::Additive,
        )
        .expect("config")
    }

    fn body(model: &str) -> serde_json::Value {
        json!({
            "id": "msg_01",
            "model": model,
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "hello"}],
            "usage": {
                "input_tokens": 100,
                "cache_read_input_tokens": 900,
                "cache_creation_input_tokens": 50,
                "output_tokens": 20
            }
        })
    }

    #[tokio::test]
    async fn a_completion_is_issued_and_counted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .and(header("x-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body("model-a")))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "agent-1", "task-1")
            .expect("begin");
        let response = adapter
            .complete(&Request::new("model-a", "hi", 100), &token)
            .await
            .expect("complete");

        assert_eq!(response.text(), "hello");
        // Additive: 100 uncached plus 900 read plus 50 write.
        assert_eq!(response.usage.uncached_input, 100);
        assert_eq!(response.usage.total_input(), 1050);

        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Counted);
        assert_eq!(record.usage.cache_read, 900);
        assert_eq!(record.usage.cache_write, 50);
        assert!(
            record.usage.raw.is_some(),
            "the raw usage blob must be persisted verbatim"
        );
    }

    #[tokio::test]
    async fn a_refusal_or_an_exceeded_context_window_raises_with_the_reason_and_stays_counted() {
        for reason in ["refusal", "model_context_window_exceeded"] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "id": "msg_r",
                    "model": "model-a",
                    "stop_reason": reason,
                    "content": [],
                    "usage": {"input_tokens": 10, "output_tokens": 0}
                })))
                .mount(&server)
                .await;
            let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
            let ledger = Ledger::open_in_memory().expect("ledger");
            let token = ledger
                .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
                .expect("begin");
            let err = adapter
                .complete(&Request::new("model-a", "hi", 100), &token)
                .await
                .expect_err("must raise");
            match &err {
                ProviderError::TurnNotUsable { stop_reason, .. } => {
                    assert_eq!(stop_reason, reason);
                }
                other => panic!("wrong error: {other:?}"),
            }
            assert!(err.to_string().contains(reason), "{err}");
            let record = ledger.call(token.id()).expect("read").expect("present");
            assert_eq!(record.settlement, Settlement::Counted, "billed, not open");
            assert_eq!(record.usage.uncached_input, 10);
        }
    }

    #[tokio::test]
    async fn a_streamed_refusal_raises_with_the_reason_and_stays_counted() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_r\",\"model\":\"model-a\",",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"},",
            "\"usage\":{\"output_tokens\":1}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect_err("must raise");
        assert!(
            matches!(err, ProviderError::TurnNotUsable { .. }),
            "{err:?}"
        );
        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Counted);
    }

    #[tokio::test]
    async fn a_reasoning_effort_is_refused_before_sending_not_silently_dropped() {
        use kgraf_provider::message::ReasoningEffort;
        // Unreachable address: a refusal must come before any connection.
        let adapter = AnthropicAdapter::new(config("http://127.0.0.1:1")).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let mut request = Request::new("model-a", "hi", 10);
        request.reasoning_effort = Some(ReasoningEffort::Low);

        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = adapter
            .complete(&request, &token)
            .await
            .expect_err("must refuse");
        assert!(
            matches!(err, ProviderError::CapabilityNotHonored { .. }),
            "{err:?}"
        );
        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Failed, "nothing was billed");

        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = Streaming::stream(&adapter, &request, &token, &mut |_| {})
            .await
            .expect_err("must refuse");
        assert!(matches!(err, ProviderError::CapabilityNotHonored { .. }));
    }

    #[tokio::test]
    async fn a_pause_turn_stop_reason_is_carried_not_flattened_into_other() {
        // A server-side tool (this vendor's own web search or code execution)
        // still running pauses the turn rather than ending it. Before this
        // fix that fell into the catch-all and was indistinguishable from an
        // unmodeled stop reason, which the runtime worker fails the run on;
        // `pause_turn` must be its own modeled state so the worker can
        // continue the turn instead.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_pause",
                "model": "model-a",
                "stop_reason": "pause_turn",
                "content": [{"type": "text", "text": "still searching"}],
                "usage": {"input_tokens": 10, "output_tokens": 5}
            })))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "agent-1", "task-1")
            .expect("begin");
        let response = adapter
            .complete(&Request::new("model-a", "hi", 100), &token)
            .await
            .expect("complete");
        assert_eq!(response.stop_reason, StopReason::PauseTurn);
        assert_ne!(response.stop_reason, StopReason::Other);
    }

    #[tokio::test]
    async fn a_model_the_adapter_does_not_serve_never_reaches_the_network() {
        let server = MockServer::start().await;
        // No mock is mounted: any request at all fails the test.
        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "typo-model", "a", "t")
            .expect("begin");
        let err = adapter
            .complete(&Request::new("typo-model", "hi", 10), &token)
            .await
            .expect_err("must refuse");
        assert!(matches!(err, ProviderError::ModelUnsupported { .. }));
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty(),
            "the request must not have been sent"
        );
    }

    #[tokio::test]
    async fn a_silently_substituted_model_raises_and_leaves_the_spend_visible() {
        // The provider returns 200 with a different model. This is the confirmed
        // failure class decision 0006 requires every adapter to assert against, and
        // the call still billed, so the ledger row must stay open.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body("something-cheaper")))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = adapter
            .complete(&Request::new("model-a", "hi", 10), &token)
            .await
            .expect_err("must raise");
        match err {
            ProviderError::ModelSubstituted { returned, .. } => {
                assert_eq!(returned, "something-cheaper");
            }
            other => panic!("wrong error: {other:?}"),
        }
        let open = ledger.unreconciled().expect("query");
        assert_eq!(
            open.len(),
            1,
            "a call that billed must not vanish because its response was wrong"
        );
        // The usage the body carried is kept on the open row.
        assert_eq!(open[0].usage.total_input(), 1050);
        assert_eq!(open[0].usage.cache_read, 900);
    }

    #[tokio::test]
    async fn a_response_with_no_usage_raises_rather_than_counting_zero() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_02", "model": "model-a", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "hi"}]
            })))
            .mount(&server)
            .await;
        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = adapter
            .complete(&Request::new("model-a", "hi", 10), &token)
            .await
            .expect_err("must raise");
        assert!(matches!(err, ProviderError::UsageMissing { .. }));
        assert_eq!(ledger.unreconciled().expect("query").len(), 1);
    }

    #[tokio::test]
    async fn an_http_error_records_a_failure_not_an_unreconciled_hole() {
        // Nothing billed, so the row is closed as failed. The distinction matters:
        // `unreconciled` is a list a human has to work through.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server)
            .await;
        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = adapter
            .complete(&Request::new("model-a", "hi", 10), &token)
            .await
            .expect_err("must raise");
        assert!(matches!(err, ProviderError::Http { status: 400, .. }));
        assert_eq!(
            ledger
                .call(token.id())
                .expect("read")
                .expect("present")
                .settlement,
            Settlement::Failed
        );
        assert!(ledger.unreconciled().expect("query").is_empty());
    }

    #[tokio::test]
    async fn cache_breakpoints_are_only_emitted_where_the_endpoint_honors_them() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body("model-a")))
            .mount(&server)
            .await;

        let mut request = Request::new("model-a", "hi", 10);
        request.system = vec![kgraf_provider::Content::cached_text("stable prefix")];

        // Default: not honored, so no breakpoint is sent. Emitting one at a provider
        // that drops it costs money silently and gains nothing.
        let plain = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let sent = plain.to_wire(&request, false);
        assert!(
            sent["system"][0].get("cache_control").is_none(),
            "a breakpoint was emitted to an endpoint not known to honor it: {sent}"
        );
        assert!(plain.capabilities().cache_control.is_none());

        // Declared: the breakpoint is sent and the capability is discoverable.
        let honoring =
            AnthropicAdapter::new(config(&server.uri()).honoring_cache_control()).expect("adapter");
        let sent = honoring.to_wire(&request, false);
        assert_eq!(sent["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(honoring.capabilities().cache_control.is_some());
    }

    fn honoring_adapter() -> AnthropicAdapter {
        AnthropicAdapter::new(config("http://127.0.0.1:1").honoring_cache_control())
            .expect("adapter")
    }

    fn breakpoint_count(wire: &serde_json::Value) -> usize {
        let text = wire.to_string();
        text.matches("\"cache_control\"").count()
    }

    #[test]
    fn the_last_block_of_the_last_message_gets_a_breakpoint_whatever_its_type() {
        let adapter = honoring_adapter();
        let mut request = Request::new("model-a", "hi", 10);
        request.system = vec![kgraf_provider::Content::cached_text("stable prefix")];
        // Text.
        let sent = adapter.to_wire(&request, false);
        assert_eq!(
            sent["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        // tool_use, then tool_result.
        request.messages = vec![
            kgraf_provider::Message::user("go"),
            kgraf_provider::Message {
                role: kgraf_provider::Role::Assistant,
                content: vec![
                    kgraf_provider::Content::text("calling"),
                    kgraf_provider::Content::ToolUse {
                        id: "t1".into(),
                        name: "read".into(),
                        input: serde_json::json!({}),
                    },
                ],
            },
        ];
        let sent = adapter.to_wire(&request, false);
        assert_eq!(sent["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(
            sent["messages"][1]["content"][1]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(
            sent["messages"][1]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert!(
            sent["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
        request.messages.push(kgraf_provider::Message {
            role: kgraf_provider::Role::User,
            content: vec![kgraf_provider::Content::ToolResult {
                tool_use_id: "t1".into(),
                content: "ok".into(),
                is_error: false,
            }],
        });
        let sent = adapter.to_wire(&request, false);
        assert_eq!(sent["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(
            sent["messages"][2]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(breakpoint_count(&sent), 2, "system plus the last block");
    }

    #[test]
    fn a_reasoning_only_tail_never_carries_a_breakpoint_on_a_thinking_block() {
        let adapter = honoring_adapter();
        let mut request = Request::new("model-a", "hi", 10);
        request.messages.push(kgraf_provider::Message {
            role: kgraf_provider::Role::Assistant,
            content: vec![kgraf_provider::Content::Reasoning { text: "hm".into() }],
        });
        let sent = adapter.to_wire(&request, false);
        assert!(!sent.to_string().contains("thinking"));
        assert_eq!(breakpoint_count(&sent), 0);
    }

    #[test]
    fn history_breakpoints_never_push_the_total_past_four_and_never_apply_when_not_honored() {
        let adapter = honoring_adapter();
        let mut request = Request::new("model-a", "hi", 10);
        request.system = vec![
            kgraf_provider::Content::cached_text("a"),
            kgraf_provider::Content::cached_text("b"),
            kgraf_provider::Content::cached_text("c"),
            kgraf_provider::Content::cached_text("d"),
        ];
        let sent = adapter.to_wire(&request, false);
        assert_eq!(breakpoint_count(&sent), 4);
        assert!(
            sent["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );

        let plain = AnthropicAdapter::new(config("http://127.0.0.1:1")).expect("adapter");
        let request = Request::new("model-a", "hi", 10);
        assert_eq!(breakpoint_count(&plain.to_wire(&request, false)), 0);
    }

    #[tokio::test]
    async fn a_later_event_reporting_a_zero_replaces_the_earlier_figure() {
        // `message_start` says 900 cache-read tokens; the final `message_delta`
        // names the field again at zero. The zero is the later, cumulative
        // figure, and an unmentioned field (`cache_creation_input_tokens`)
        // keeps its earlier value.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_z\",\"model\":\"model-a\",",
            "\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":900,",
            "\"cache_creation_input_tokens\":50,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"cache_read_input_tokens\":0,\"output_tokens\":20}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let response = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect("stream");
        assert_eq!(response.usage.cache_read, 0);
        assert_eq!(response.usage.cache_write, 50);
        assert_eq!(response.usage.uncached_input, 100);
        assert_eq!(response.usage.text_output, 20);
    }

    #[tokio::test]
    async fn streaming_usage_is_merged_cumulatively_across_both_events() {
        // The protocol sends the input side in `message_start` and the output side in
        // `message_delta`, and the second is cumulative. Summing would double the
        // output count; reading only `message_delta` would lose the cache counts
        // entirely.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s\",\"model\":\"model-a\",",
            "\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":900,",
            "\"cache_creation_input_tokens\":50,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"he\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"llo\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"input_tokens\":100,\"output_tokens\":20}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let mut events = Vec::new();
        let response = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |event| events.push(event),
        )
        .await
        .expect("stream");

        assert_eq!(response.text(), "hello");
        assert_eq!(
            response.usage.text_output, 20,
            "cumulative, not summed to 21"
        );
        assert_eq!(response.usage.cache_read, 900, "the cache counts survived");
        assert_eq!(response.usage.total_input(), 1050);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::Started { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::Finished { .. }))
        );

        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Counted);
        assert_eq!(record.usage.text_output, 20);
    }

    #[tokio::test]
    async fn a_streamed_pause_turn_is_carried_not_flattened_into_other() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_p\",\"model\":\"model-a\",",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"still searching\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"pause_turn\"},",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let response = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_event| {},
        )
        .await
        .expect("stream");

        assert_eq!(response.stop_reason, StopReason::PauseTurn);
        assert_ne!(response.stop_reason, StopReason::Other);
    }

    #[tokio::test]
    async fn a_stream_that_dies_before_its_usage_event_is_left_unreconciled() {
        // The documented case: an interrupted stream may never deliver its usage
        // chunk at all, this fixture's `message_start` carries no `usage` field,
        // so it must be surfaced as a hole with its request id, not dropped.
        // `UsageMissing` is exactly right here, distinct from `StreamTruncated`
        // (some usage observed, but no terminal event): with genuinely no usage
        // ever reported, there is no partial figure to characterize a truncation
        // with.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_dropped\",\"model\":\"model-a\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"par\"}}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect_err("must raise");
        assert!(matches!(err, ProviderError::UsageMissing { .. }), "{err:?}");

        let open = ledger.unreconciled().expect("query");
        assert_eq!(open.len(), 1);
        assert_eq!(
            open[0].request_id.as_deref(),
            Some("msg_dropped"),
            "the handle for reconciling the call against the provider's record"
        );

        // And it can be closed once the real figure is known.
        ledger
            .reconcile(
                &open[0].id,
                &TokenUsage {
                    uncached_input: 100,
                    text_output: 7,
                    ..TokenUsage::default()
                },
            )
            .expect("reconcile");
        assert!(ledger.unreconciled().expect("query").is_empty());
    }

    #[tokio::test]
    async fn a_streamed_tool_use_block_survives_split_across_input_json_delta_fragments() {
        // The defect this fixes: the streaming parser had no `content_block_start`
        // or `content_block_stop` handling at all, so a `tool_use` block's id,
        // name and `input_json_delta` fragments were dropped on the floor and the
        // assembled response never carried a tool call. Documented shape (not a
        // recorded live call; no live tool-use stream fixture exists yet), built
        // from Anthropic's published streaming example: a thinking block, a text
        // block, and a tool_use block whose `input` arrives in three fragments
        // that are not valid JSON until concatenated.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_tool\",\"model\":\"model-a\",",
            "\"usage\":{\"input_tokens\":200,\"cache_read_input_tokens\":0,",
            "\"cache_creation_input_tokens\":0,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"I should check the weather.\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me check that for you.\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,",
            "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01A\",",
            "\"name\":\"get_weather\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,",
            "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"location\\\": \\\"San\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,",
            "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" Francisco, C\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,",
            "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"A\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},",
            "\"usage\":{\"input_tokens\":200,\"output_tokens\":30,",
            "\"output_tokens_details\":{\"thinking_tokens\":10}}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let mut events = Vec::new();
        let response = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |event| events.push(event),
        )
        .await
        .expect("stream");

        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.text(), "Let me check that for you.");
        let tool_uses = response.tool_uses();
        assert_eq!(tool_uses.len(), 1, "exactly one tool call, not dropped");
        let (id, name, input) = tool_uses[0];
        assert_eq!(id, "toolu_01A");
        assert_eq!(name, "get_weather");
        assert_eq!(
            input,
            &json!({"location": "San Francisco, CA"}),
            "the three input_json_delta fragments must concatenate to valid JSON"
        );
        assert!(
            matches!(&response.content[0], Content::Reasoning { text } if text == "I should check the weather.")
        );
        assert_eq!(response.usage.reasoning_output, 10);
        assert_eq!(
            response.usage.text_output, 20,
            "30 output minus 10 thinking"
        );

        // The complete tool call, with parsed input, must have reached the sink;
        // a partial `partial_json` fragment must never have been sunk on its own.
        let sunk_tool_use = events.iter().any(|event| {
            matches!(
                event,
                StreamEvent::Delta {
                    content: Content::ToolUse { name, input, .. }
                } if name == "get_weather" && input == &json!({"location": "San Francisco, CA"})
            )
        });
        assert!(sunk_tool_use, "the complete tool call must reach the sink");
    }

    #[tokio::test]
    async fn a_streamed_tool_use_block_with_invalid_json_raises_rather_than_an_empty_input() {
        // The fail-loud rule: invalid accumulated JSON is a typed error, never a silently
        // empty input an agent could act on as though the model asked for nothing.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_bad\",\"model\":\"model-a\",",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_bad\",",
            "\"name\":\"get_weather\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{not json\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect_err("must raise rather than fabricate an empty input");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");

        // The stream had already reported real usage (from `message_start`)
        // before the decode failure, so the row must not be closed with that
        // usage silently zeroed (the `AbandonGuard` defect testing
        // found: every early exit after `message_start` discarded the known
        // input and cache usage). `leave_unreconciled_with` is what carries a
        // known partial usage alongside the row staying visible in the
        // reconciliation queue.
        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Unreconciled);
        assert_eq!(
            record.usage.uncached_input, 10,
            "the observed input usage must survive as a stated lower bound"
        );
        assert_eq!(
            ledger.unreconciled().expect("query").len(),
            1,
            "the call reached the provider and may have billed"
        );
        assert!(
            ledger.usage_by_model().expect("usage").is_empty(),
            "a partial, unreconciled figure must not appear in the totals \
             as though it were final"
        );
    }

    #[tokio::test]
    async fn a_failure_before_message_start_still_carries_the_transport_request_id() {
        // The guard previously only learned a
        // request id from `message_start`'s own `id` field, so any failure
        // before that ever arrived (here, an `error` event that is the very
        // first thing sent) closed the row with no request id at all, even
        // though the HTTP `request-id` header was already in hand by the time
        // `post` returned.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",",
            "\"message\":\"Overloaded\"}}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse)
                    .insert_header("request-id", "req-early"),
            )
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect_err("must raise");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");

        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(
            record.request_id.as_deref(),
            Some("req-early"),
            "the transport-level request id must survive a failure before \
             message_start ever arrived"
        );
    }

    #[tokio::test]
    async fn a_server_tool_use_block_is_skipped_consistently_not_fatal() {
        // `server_tool_use` (a built-in,
        // server-executed tool such as web search) and its result block
        // accumulate input the same way an ordinary `tool_use` block does,
        // via `input_json_delta`, but `complete`'s non-streaming parser
        // already skips both block types outright. Streaming previously had
        // no arm for `server_tool_use` at `content_block_start` at all, so
        // its `input_json_delta` fragments looked exactly like an orphaned
        // fragment once the unknown-index check existed, and failed the
        // whole stream. Documented shape (not a recorded live call), built
        // from Anthropic's published server-tool-use streaming example.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_srvtool\",",
            "\"model\":\"model-a\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srvtoolu_01\",",
            "\"name\":\"web_search\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"query\\\": \\\"weather\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,",
            "\"content_block\":{\"type\":\"web_search_tool_result\",",
            "\"tool_use_id\":\"srvtoolu_01\",\"content\":[]}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"It is sunny.\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let response = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect("a server_tool_use block must not fail the stream");

        assert_eq!(
            response.text(),
            "It is sunny.",
            "the skipped blocks must not appear in content, matching complete()"
        );
        assert!(
            response.tool_uses().is_empty(),
            "a server_tool_use block is skipped, not surfaced as an ordinary \
             tool call"
        );
    }

    #[tokio::test]
    async fn a_text_delta_for_a_block_open_as_reasoning_raises() {
        // A `text_delta` landing on an index that
        // is open as a `thinking` block is a protocol violation. A previous
        // version's `or_insert_with` left the existing `Reasoning`
        // accumulator alone but also did not raise, so the text fragment
        // simply vanished with no sign anything was lost.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_mix\",",
            "\"model\":\"model-a\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"should not land here\"}}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect_err("a text_delta on a thinking block must raise");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn a_repeated_content_block_start_for_the_same_index_raises() {
        // A second `content_block_start` for an
        // index that is already open is a protocol violation. A previous
        // version's `blocks.insert` silently overwrote the first block,
        // discarding whatever it had already accumulated with no error.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_dup\",",
            "\"model\":\"model-a\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"first\"}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect_err("a repeated content_block_start for the same index must raise");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn a_mid_stream_error_event_raises_rather_than_returning_ok_with_partial_content() {
        // Anthropic reports a mid-stream failure (the response
        // already committed to HTTP 200) as its own `error` SSE event. The
        // previous parser matched the catch-all arm and silently ignored it, so
        // a caller got `Ok` back with whatever had streamed before the failure.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_err\",\"model\":\"model-a\",",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",",
            "\"message\":\"Overloaded\"}}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect_err("a mid-stream error event must raise, not return Ok");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");
        if let ProviderError::Decode { reason, .. } = &err {
            assert!(reason.contains("overloaded_error"), "{reason}");
        }

        // Usage had already arrived from `message_start`; it must not be
        // silently zeroed by the abnormal exit, and the row must
        // stay visible in the reconciliation queue.
        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Unreconciled);
        assert_eq!(record.usage.uncached_input, 10);
        assert_eq!(
            ledger.unreconciled().expect("query").len(),
            1,
            "the call reached the provider and may have billed"
        );
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_message_stop_after_usage_arrived_is_not_returned_as_ok() {
        // `message_delta` (carrying usage) arrived,
        // so a version that only checked `saw_usage` would have returned `Ok`
        // with truncated content. Both the missing `message_stop` and the block
        // that never closed must be caught.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_trunc\",\"model\":\"model-a\",",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{},",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect_err("must not return Ok with truncated content");
        assert!(
            matches!(err, ProviderError::StreamTruncated { .. }),
            "{err:?}"
        );

        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(
            record.settlement,
            Settlement::Unreconciled,
            "usage had arrived, so it must be preserved as a stated lower \
             bound, not zeroed, and the row must stay visible for reconciliation"
        );
        assert_eq!(record.usage.uncached_input, 10);
        assert_eq!(record.request_id.as_deref(), Some("msg_trunc"));
    }

    #[tokio::test]
    async fn a_stream_truncated_after_message_start_is_left_unreconciled_with_its_partial_usage() {
        // Ported from the ledger stream's own regression test (merged on main
        // against the pre-rewrite parser), kept here against the rewritten
        // one: `message_start` carries the input-side usage before any output
        // exists, so a stream cut off before `message_stop` still has
        // `saw_usage == true`. Settling that as `Counted` would bill only the
        // input-side figure as if it were the whole call's final usage,
        // understating spend silently (every call is attributed to the ledger).
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_truncated\",\"model\":\"model-a\",\
             \"usage\":{\"input_tokens\":1200,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"par\"}}\n\n",
            // No `message_delta` and no `message_stop`: the body ends here.
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect_err("must raise");
        assert!(
            matches!(err, ProviderError::StreamTruncated { .. }),
            "got {err:?}"
        );

        let open = ledger.unreconciled().expect("query");
        assert_eq!(open.len(), 1);
        assert_eq!(
            open[0].request_id.as_deref(),
            Some("msg_truncated"),
            "the handle for reconciling against the provider's own record"
        );
        assert_eq!(
            open[0].usage.uncached_input, 1200,
            "the partial figure is kept as a stated lower bound, not dropped to zero"
        );

        // Excluded from spend until reconciled: a lower bound must not look
        // like a final figure.
        assert!(
            ledger.usage_by_model().expect("usage").is_empty(),
            "a partial unreconciled figure must not appear in the totals"
        );

        ledger
            .reconcile(
                &open[0].id,
                &TokenUsage {
                    uncached_input: 1200,
                    text_output: 42,
                    ..TokenUsage::default()
                },
            )
            .expect("reconcile");
        assert!(ledger.unreconciled().expect("query").is_empty());
    }

    #[tokio::test]
    async fn a_content_block_that_never_closes_before_message_stop_raises() {
        // `message_stop` legitimately arrives, but a block
        // opened earlier never received its `content_block_stop`. Its text is
        // not in the final `content`, so returning `Ok` would silently hand
        // back an incomplete answer.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_open\",\"model\":\"model-a\",",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"never closed\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect_err("an unclosed block must raise, not be silently discarded");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn a_tool_call_with_no_arguments_uses_the_content_block_start_input() {
        // a tool with no arguments emits zero
        // `input_json_delta` fragments at all, so the accumulated `json` string
        // is empty. Parsing `""` as JSON always fails; the fix falls back to
        // the `input` object `content_block_start` already carried (Anthropic
        // sends `{}` there even for a no-argument call).
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_noargs\",\"model\":\"model-a\",",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_noargs\",",
            "\"name\":\"get_current_time\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let response = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect("a no-argument tool call must decode, not fail");
        let tool_uses = response.tool_uses();
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].1, "get_current_time");
        assert_eq!(tool_uses[0].2, &json!({}));
    }

    #[test]
    fn an_sse_data_payload_that_fails_to_parse_raises_rather_than_being_dropped() {
        // a genuinely dataless event (no `data:` line at all) is
        // fine to skip, but a `data:` line that fails to parse as JSON is
        // corruption, not a comment, and must raise.
        let mut decoder = SseDecoder::new(ADAPTER);
        let err = decoder
            .push(b"data: {not valid json at all\n\n")
            .expect_err("must raise on a malformed data payload");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");

        // The dataless case is unaffected: a pure comment or `event:` line
        // with no `data:` payload at all is still skipped, not raised.
        let mut decoder = SseDecoder::new(ADAPTER);
        let events = decoder
            .push(b": keep-alive\n\n")
            .expect("a comment line must not raise");
        assert!(events.is_empty());
    }

    #[test]
    fn crlf_and_bare_cr_line_endings_decode_identically_to_lf() {
        // some proxies rewrite line endings. All three
        // conventions, including a split exactly on the `\r`/`\n` boundary,
        // must decode to the same event.
        let lf = b"data: {\"type\":\"message_stop\"}\n\n".to_vec();
        let crlf = b"data: {\"type\":\"message_stop\"}\r\n\r\n".to_vec();
        let cr = b"data: {\"type\":\"message_stop\"}\r\r".to_vec();

        let reference = {
            let mut decoder = SseDecoder::new(ADAPTER);
            decoder.push(&lf).expect("lf")
        };
        assert_eq!(reference.len(), 1);

        for variant in [&crlf, &cr] {
            let mut decoder = SseDecoder::new(ADAPTER);
            let events = decoder.push(variant).expect("push");
            assert_eq!(events, reference, "{variant:?}");
        }

        // A `\r\n\r\n` boundary split exactly between the `\r` and the `\n`
        // across two chunks must still be recognized once the second chunk
        // arrives, not treated as two separate, unterminated fragments.
        let mut decoder = SseDecoder::new(ADAPTER);
        let mut events = decoder.push(&crlf[..crlf.len() - 2]).expect("push prefix");
        events.extend(
            decoder
                .push(&crlf[crlf.len() - 2..])
                .expect("push remainder"),
        );
        assert_eq!(events, reference);
    }

    #[test]
    fn an_sse_event_that_never_terminates_is_capped_rather_than_buffered_without_limit() {
        // a connection that never sends a blank line must not
        // grow the decoder's buffer forever.
        let mut decoder = SseDecoder::new(ADAPTER);
        let oversized = vec![b'a'; MAX_EVENT_BYTES + 1];
        let err = decoder
            .push(&oversized)
            .expect_err("an unterminated event past the cap must raise");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn a_tool_calls_accumulated_input_json_delta_bytes_are_capped() {
        // independent of the SSE event cap above, a single
        // tool call's `input_json_delta` fragments must not accumulate without
        // limit even if every individual `content_block_delta` event is well
        // formed and properly terminated.
        let server = MockServer::start().await;
        let oversized_fragment = "a".repeat(MAX_TOOL_INPUT_BYTES + 1);
        let delta_event = format!(
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\
             \"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{oversized_fragment}\"}}}}\n\n"
        );
        let sse = format!(
            "{}{delta_event}{}",
            concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_big\",",
                "\"model\":\"model-a\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,",
                "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_big\",",
                "\"name\":\"f\",\"input\":{}}}\n\n",
            ),
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect_err("an oversized tool input must raise, not accumulate forever");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn a_signature_delta_does_not_emit_a_stray_delta_or_corrupt_the_thinking_block() {
        // `signature_delta` accompanies a
        // `thinking` block's signature, not text. It previously fell into the
        // text-delta catch-all, sinking a spurious empty `Content::text("")`
        // and colliding with the `Reasoning` accumulator already open at the
        // same index.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_sig\",\"model\":\"model-a\",",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reasoning here\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"signature_delta\",\"signature\":\"deadbeef\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let mut events = Vec::new();
        let response = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |event| events.push(event),
        )
        .await
        .expect("stream");

        assert_eq!(
            response.content.len(),
            1,
            "only the thinking block, uncorrupted"
        );
        assert!(
            matches!(&response.content[0], Content::Reasoning { text } if text == "reasoning here")
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                StreamEvent::Delta {
                    content: Content::Text { text, .. }
                } if text.is_empty()
            )),
            "a signature_delta must never sink an empty text delta"
        );
    }

    #[tokio::test]
    async fn a_redacted_thinking_block_is_reported_not_silently_dropped() {
        // a `redacted_thinking` block was
        // previously unmodeled at `content_block_start` and fell into the
        // silent-skip arm, so its occurrence was invisible to a caller.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_redacted\",",
            "\"model\":\"model-a\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque-blob\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let response = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect("stream");

        assert_eq!(
            response.content.len(),
            1,
            "the redacted block must be reported"
        );
        match &response.content[0] {
            Content::Reasoning { text } => assert!(
                text.contains("opaque-blob"),
                "the opaque data must be legible, not dropped: {text}"
            ),
            other => panic!("expected a reported reasoning block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_input_json_delta_for_a_block_that_is_not_an_open_tool_use_raises() {
        // a fragment for an index that was never opened as a
        // `tool_use` block (here, never opened at all) must raise rather than
        // being silently discarded.
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_orphan\",",
            "\"model\":\"model-a\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":7,",
            "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let err = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect_err("an orphaned input_json_delta must raise");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn content_is_ordered_by_index_not_by_which_block_closes_first() {
        // block 1 closes before block 0 here. The final
        // content must still read in index order (block 0 then block 1), not
        // in close order (which would read "secondfirst").
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_order\",",
            "\"model\":\"model-a\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"second\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"first\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let response = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect("stream");

        assert_eq!(
            response.text(),
            "firstsecond",
            "content must be ordered by index, not by which block closed first"
        );
    }

    #[tokio::test]
    async fn a_stalled_stream_times_out_on_the_idle_gap_not_the_whole_call_duration() {
        // a connection that stops sending anything at all
        // must eventually be treated as dead, bounded by the configured
        // per-chunk idle timeout.
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            drain_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("headers");
            stream
                .write_all(
                    b"data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_stall\",\
                      \"model\":\"model-a\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                )
                .await
                .expect("first event");
            // Then goes silent forever (until the test ends and drops the
            // socket); the client must not wait forever for it.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });

        let adapter = AnthropicAdapter::new(
            config(&format!("http://{addr}")).with_timeout(std::time::Duration::from_millis(150)),
        )
        .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            Streaming::stream(
                &adapter,
                &Request::new("model-a", "hi", 100),
                &token,
                &mut |_| {},
            ),
        )
        .await
        .expect("must time out well inside 2s, not hang")
        .expect_err("a stalled connection must raise");
        assert!(matches!(err, ProviderError::Transport { .. }), "{err:?}");
        server.abort();
    }

    #[tokio::test]
    async fn a_long_running_stream_survives_past_the_configured_timeout_while_chunks_keep_arriving()
    {
        // the defect this fixes, a whole-call timeout would
        // cut off a stream that is still healthily producing tokens. Here the
        // configured timeout (150ms) is shorter than the whole call (well over
        // 500ms across several chunks), and it must still succeed as long as no
        // single gap between chunks exceeds the timeout.
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            drain_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("headers");
            stream
                .write_all(
                    concat!(
                        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_long\",",
                        "\"model\":\"model-a\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                        "data: {\"type\":\"content_block_start\",\"index\":0,",
                        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                    )
                    .as_bytes(),
                )
                .await
                .expect("first events");
            for _ in 0..6u32 {
                tokio::time::sleep(std::time::Duration::from_millis(90)).await;
                stream
                    .write_all(
                        b"data: {\"type\":\"content_block_delta\",\"index\":0,\
                          \"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n",
                    )
                    .await
                    .expect("delta");
            }
            stream
                .write_all(
                    concat!(
                        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
                        "\"usage\":{\"input_tokens\":1,\"output_tokens\":6}}\n\n",
                        "data: {\"type\":\"message_stop\"}\n\n",
                    )
                    .as_bytes(),
                )
                .await
                .expect("finish");
        });

        let adapter = AnthropicAdapter::new(
            config(&format!("http://{addr}")).with_timeout(std::time::Duration::from_millis(200)),
        )
        .expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");

        let started = std::time::Instant::now();
        let response = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |_| {},
        )
        .await
        .expect("a healthy long stream must not be cut off by the whole-call duration");
        server.await.expect("server task");
        assert!(
            started.elapsed() > std::time::Duration::from_millis(200),
            "sanity: the call must genuinely have run longer than its own \
             configured timeout to prove anything"
        );
        assert_eq!(response.text(), "xxxxxx");
    }

    #[test]
    fn the_sse_decoder_produces_identical_events_no_matter_where_the_wire_splits() {
        // A network chunk boundary can fall anywhere, including mid multi-byte
        // UTF-8 character (`café`, `☕`) or mid `input_json_delta` fragment. The
        // decoded events must not depend on where reqwest happened to split
        // the bytes.
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg\",\"model\":\"model-a\",",
            "\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"caf\\u00e9 \\u2615\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,",
            "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",",
            "\"name\":\"get_weather\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,",
            "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\": \"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,",
            "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Boston\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let bytes = body.as_bytes();

        let reference = {
            let mut decoder = SseDecoder::new(ADAPTER);
            let mut events = decoder.push(bytes).expect("push");
            events.extend(decoder.finish().expect("finish"));
            events
        };
        assert_eq!(reference.len(), 9, "sanity: every event was decoded whole");

        for split in 0..=bytes.len() {
            let mut decoder = SseDecoder::new(ADAPTER);
            let mut events = decoder.push(&bytes[..split]).expect("push prefix");
            events.extend(decoder.push(&bytes[split..]).expect("push suffix"));
            events.extend(decoder.finish().expect("finish"));
            assert_eq!(
                events, reference,
                "splitting the wire at byte offset {split} must not change \
                 the decoded events"
            );
        }

        // The extreme case: every multi-byte character and every JSON
        // fragment boundary is split at least once across this run.
        let mut byte_at_a_time = SseDecoder::new(ADAPTER);
        let mut events = Vec::new();
        for byte in bytes {
            events.extend(
                byte_at_a_time
                    .push(std::slice::from_ref(byte))
                    .expect("push one byte"),
            );
        }
        events.extend(byte_at_a_time.finish().expect("finish"));
        assert_eq!(events, reference);
    }

    #[tokio::test]
    async fn a_streamed_delta_reaches_the_sink_before_a_delayed_later_chunk_arrives() {
        // The defect: `stream()` read the whole body with `.text()`
        // before parsing anything, so a caller learned nothing until the
        // connection closed. This proves the fix is genuinely incremental: the
        // sink sees the first event while the server is still holding the
        // connection open, mid-sleep, well before it sends the rest.
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            drain_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("headers");
            stream
                .write_all(
                    b"data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_slow\",\
                      \"model\":\"model-a\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                )
                .await
                .expect("first event");
            stream.flush().await.expect("flush");
            // A consumer that buffered the whole body before parsing anything
            // could not see the first event before this sleep elapses.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let _ = stream
                .write_all(
                    concat!(
                        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
                        "\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n\n",
                        "data: {\"type\":\"message_stop\"}\n\n",
                    )
                    .as_bytes(),
                )
                .await;
        });

        let adapter = AnthropicAdapter::new(config(&format!("http://{addr}"))).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");

        let started = std::time::Instant::now();
        let first_event_at: std::sync::Arc<std::sync::Mutex<Option<std::time::Duration>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let record = first_event_at.clone();
        let response = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |event| {
                if matches!(event, StreamEvent::Started { .. }) {
                    record
                        .lock()
                        .expect("lock")
                        .get_or_insert(started.elapsed());
                }
            },
        )
        .await
        .expect("stream");
        let total_elapsed = started.elapsed();
        server.await.expect("server task");

        let first_event_at = first_event_at
            .lock()
            .expect("lock")
            .expect("the started event must have reached the sink");
        // A relative bound, not an absolute millisecond one, so this is not
        // flaky under CI scheduling noise: the first event must have reached
        // the sink well before the run finished, not merely at the end of it,
        // which is what buffering the whole body first would look like.
        assert!(
            first_event_at < total_elapsed / 3,
            "the first event must reach the sink long before the delayed \
             second chunk, not only after the connection closed: first event \
             at {first_event_at:?}, whole call took {total_elapsed:?}"
        );
        assert_eq!(response.stop_reason, StopReason::EndTurn);
    }

    #[tokio::test]
    async fn dropping_the_stream_future_stops_the_request_before_the_server_finishes() {
        // The requirement: a cancelled caller (dropping the future
        // returned by `stream()`, e.g. via a `select!` or a timeout) must abort
        // the HTTP request rather than let it run to completion unread.
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
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("headers");
            let mut disconnected_tx = Some(disconnected_tx);
            for i in 0..20u32 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let chunk = format!(
                    "data: {{\"type\":\"content_block_delta\",\"index\":0,\
                     \"delta\":{{\"type\":\"text_delta\",\"text\":\"{i}\"}}}}\n\n"
                );
                if stream.write_all(chunk.as_bytes()).await.is_err() {
                    if let Some(tx) = disconnected_tx.take() {
                        let _ = tx.send(());
                    }
                    return;
                }
            }
            // Every write succeeded: the client never dropped the request, and
            // the assertion below fails because the signal never arrives.
        });

        let adapter = AnthropicAdapter::new(config(&format!("http://{addr}"))).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(120),
            Streaming::stream(
                &adapter,
                &Request::new("model-a", "hi", 100),
                &token,
                &mut |_| {},
            ),
        )
        .await;
        assert!(
            outcome.is_err(),
            "the stream must still be in flight at this short timeout, or the \
             test proves nothing about cancellation"
        );
        // `outcome` going out of scope here drops the still-running `stream()`
        // future, and with it the byte stream reqwest was reading from.

        tokio::time::timeout(std::time::Duration::from_millis(1000), disconnected_rx)
            .await
            .expect("the server must observe the dropped connection promptly")
            .expect("the sender must not be dropped without sending");

        server.abort();
    }

    /// Drain a raw request off the wire without knowing its length in
    /// advance: read until the connection goes quiet for a beat, which for a
    /// client that writes its request and then waits for a response means the
    /// whole request has arrived.
    async fn drain_request(stream: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut buffer = [0_u8; 4096];
        loop {
            // Short: a loopback connection delivers a small POST body in one
            // read almost immediately, so this only has to wait long enough
            // to notice the second read has nothing more coming. Kept well
            // under the short `config.timeout` values the idle-timeout tests
            // configure, so draining the request never itself trips the
            // timeout under test.
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

    #[test]
    fn usage_normalization_follows_the_configured_protocol_not_the_adapter_name() {
        // One vendor ships both conventions behind a single API key, so the same
        // adapter must produce the right answer for either.
        let raw = json!({
            "input_tokens": 1050,
            "cache_read_input_tokens": 900,
            "cache_creation_input_tokens": 50,
            "output_tokens": 20
        });
        let inclusive =
            AnthropicAdapter::normalize_usage(Protocol::Inclusive, &raw).expect("inclusive");
        assert_eq!(inclusive.uncached_input, 100);
        assert_eq!(inclusive.total_input(), 1050);

        let additive =
            AnthropicAdapter::normalize_usage(Protocol::Additive, &raw).expect("additive");
        assert_eq!(additive.uncached_input, 1050);
        assert_eq!(additive.total_input(), 2000);
    }

    #[test]
    fn an_absent_usage_object_is_distinguished_from_a_zero_token_call() {
        let err = AnthropicAdapter::normalize_usage(Protocol::Additive, &json!({}))
            .expect_err("must raise");
        assert!(matches!(err, ProviderError::UsageMissing { .. }));

        let zero = AnthropicAdapter::normalize_usage(
            Protocol::Additive,
            &json!({"input_tokens": 0, "output_tokens": 0}),
        )
        .expect("a genuine zero is fine");
        assert!(zero.is_empty());
    }

    #[test]
    fn the_first_live_kimi_anthropic_usage_replays_with_thinking_carved_out() {
        // The real-speaker rule: a real speaker's usage object, recorded through this adapter on
        // 2026-09-26. The recorded normal form predates the fix and put the 37
        // thinking tokens under text; the replay asserts the corrected split and that
        // it still sums to the vendor's output total.
        let record: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/2026-09-26-kimi-anthropic-live-call.json"
        ))
        .expect("fixture");
        let raw = &record["usage"]["raw"];
        let usage = AnthropicAdapter::normalize_usage(Protocol::Additive, raw).expect("usage");
        assert_eq!(usage.reasoning_output, 37);
        assert_eq!(usage.text_output, 17);
        assert_eq!(
            usage.total_output(),
            raw["output_tokens"].as_u64().expect("out")
        );
        assert_eq!(usage.uncached_input, 92);
        assert_eq!(
            usage.total_input() + usage.total_output(),
            raw["total_tokens"].as_u64().expect("total")
        );
    }

    #[test]
    fn thinking_reported_beyond_output_is_refused() {
        let raw = json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "output_tokens_details": {"thinking_tokens": 9}
        });
        let err =
            AnthropicAdapter::normalize_usage(Protocol::Additive, &raw).expect_err("must refuse");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");
    }

    /// A streamed tool call (a `tool_use` block whose input arrives as
    /// `input_json_delta` fragments) reaches the assembled response whole, in
    /// block order; before this, it was dropped and the loop saw `tool_use` as
    /// the stop reason with no call to run.
    #[tokio::test]
    async fn a_streamed_tool_call_is_assembled_into_the_response() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_t\",\"model\":\"model-a\",",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"reading\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"pa\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"th\\\": \\\"a.txt\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"list_directory\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},",
            "\"usage\":{\"output_tokens\":30}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(sse)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let adapter = AnthropicAdapter::new(config(&server.uri())).expect("adapter");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let token = ledger
            .begin(ADAPTER, Protocol::Additive, "model-a", "a", "t")
            .expect("begin");
        let mut events = Vec::new();
        let response = Streaming::stream(
            &adapter,
            &Request::new("model-a", "hi", 100),
            &token,
            &mut |event| events.push(event),
        )
        .await
        .expect("stream");
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.text(), "reading");
        let calls: Vec<(&str, &str, serde_json::Value)> = response
            .tool_uses()
            .into_iter()
            .map(|(id, name, input)| (id, name, input.clone()))
            .collect();
        assert_eq!(
            calls,
            vec![
                ("toolu_1", "read_file", serde_json::json!({"path": "a.txt"})),
                ("toolu_2", "list_directory", serde_json::json!({})),
            ]
        );
        // The partial JSON never leaked out as text.
        assert!(!events.iter().any(|e| matches!(
            e,
            StreamEvent::Delta { content: Content::Text { text, .. } } if text.contains("path")
        )));
        let record = ledger.call(token.id()).expect("read").expect("present");
        assert_eq!(record.settlement, Settlement::Counted);
    }
}
