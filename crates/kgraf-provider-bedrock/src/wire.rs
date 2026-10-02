//! The Converse wire shapes: request building, response parsing, usage and stop
//! reasons. Pure functions over JSON values; nothing here does I/O.

use kgraf_core::{Protocol, TokenUsage};
use kgraf_provider::{Content, ProviderError, Request, Role, StopReason};
use serde_json::{Value, json};

use crate::ADAPTER;

/// Explicit cache points one request may carry (the documented per-request limit
/// for the models that support explicit caching).
pub const MAX_CACHE_POINTS: usize = 4;

/// What an empty tool result is sent as.
pub const EMPTY_TOOL_RESULT: &str = "(no output)";

fn decode(reason: impl Into<String>) -> ProviderError {
    ProviderError::Decode {
        adapter: ADAPTER,
        reason: reason.into(),
    }
}

fn cache_point() -> Value {
    json!({"cachePoint": {"type": "default"}})
}

/// A float as the shortest JSON number that reads back to the same `f32`, so `0.3`
/// is sent as `0.3` and not as `0.30000001192092896`.
fn temperature_number(t: f32) -> Value {
    let text = t.to_string();
    text.parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
        .unwrap_or_else(|| json!(t))
}

/// Blocks for one piece of kGraf content, in Converse's shape. `honor` says whether
/// cache breakpoints become `cachePoint` blocks.
///
/// Reasoning is not resent: the wire wants the block's signature back unmodified and
/// kGraf's content model holds only the text, so a reasoning block cannot be replayed
/// faithfully and is left out rather than replayed wrong.
fn blocks_for(content: &Content, honor: bool) -> Vec<Value> {
    match content {
        Content::Text {
            text,
            cache_breakpoint,
            ..
        } => {
            // The service rejects a blank text block (a validation error), so blank
            // text is left out, and its cache breakpoint with it.
            if text.trim().is_empty() {
                return Vec::new();
            }
            let mut out = vec![json!({"text": text})];
            if *cache_breakpoint && honor {
                out.push(cache_point());
            }
            out
        }
        Content::ToolUse {
            id, name, input, ..
        } => vec![json!({
            "toolUse": {
                "toolUseId": id,
                "name": name,
                "input": kgraf_provider::tool_arguments_object(input),
            }
        })],
        Content::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            // A tool that printed nothing still needs a result block, and the
            // service rejects blank text inside it, so a placeholder stands in.
            let text = if content.trim().is_empty() {
                EMPTY_TOOL_RESULT
            } else {
                content.as_str()
            };
            let mut result = json!({
                "toolUseId": tool_use_id,
                "content": [{"text": text}],
            });
            // `status` is only sent for a failure: it is honored by some models and
            // rejected by others, and the failure is also stated in the content.
            if *is_error {
                result["status"] = json!("error");
            }
            vec![json!({"toolResult": result})]
        }
        Content::Reasoning { .. } => Vec::new(),
    }
}

/// Translate a kGraf request into a Converse request body.
///
/// Consecutive messages of one role are merged: the service requires strictly
/// alternating roles, where the Messages API of other vendors joins them itself.
/// When `honor_cache` is set, the last block of the conversation also gets a cache
/// point while a slot is free, so each round of an agent loop reads the earlier
/// rounds from the cache.
pub fn to_wire(request: &Request, honor_cache: bool) -> Value {
    let mut messages: Vec<(Role, Vec<Value>)> = Vec::new();
    for message in &request.messages {
        let blocks: Vec<Value> = message
            .content
            .iter()
            .flat_map(|c| blocks_for(c, honor_cache))
            .collect();
        if blocks.is_empty() {
            continue;
        }
        match messages.last_mut() {
            Some((role, existing)) if *role == message.role => existing.extend(blocks),
            _ => messages.push((message.role, blocks)),
        }
    }

    if honor_cache {
        let explicit = request
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
        if explicit < MAX_CACHE_POINTS
            && let Some((_, blocks)) = messages.last_mut()
            && blocks.last().is_some_and(|b| b.get("cachePoint").is_none())
        {
            blocks.push(cache_point());
        }
    }

    let mut body = json!({
        "messages": messages
            .into_iter()
            .map(|(role, content)| json!({
                "role": match role { Role::User => "user", Role::Assistant => "assistant" },
                "content": content,
            }))
            .collect::<Vec<_>>(),
        "inferenceConfig": {"maxTokens": request.max_output_tokens},
    });
    if let Some(t) = request.temperature {
        body["inferenceConfig"]["temperature"] = temperature_number(t);
    }
    let system: Vec<Value> = request
        .system
        .iter()
        .flat_map(|c| blocks_for(c, honor_cache))
        .collect();
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    if !request.tools.is_empty() {
        body["toolConfig"] = json!({
            "tools": request.tools.iter().map(|tool| {
                let mut spec = json!({
                    "name": tool.name,
                    "inputSchema": {"json": tool.input_schema},
                });
                if !tool.description.is_empty() {
                    spec["description"] = json!(tool.description);
                }
                json!({"toolSpec": spec})
            }).collect::<Vec<_>>(),
        });
    }
    body
}

/// How a stop reason lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// A turn that produced an answer.
    Usable(StopReason),
    /// A turn that ended without one; raised as `TurnNotUsable` after the call is
    /// settled.
    Unusable(String),
}

/// Map a Converse `stopReason`. An unknown value raises: guessing would hand back a
/// turn whose ending nothing understands.
pub fn map_stop(reason: &str) -> Result<Stop, ProviderError> {
    Ok(match reason {
        "end_turn" => Stop::Usable(StopReason::EndTurn),
        "tool_use" => Stop::Usable(StopReason::ToolUse),
        "max_tokens" => Stop::Usable(StopReason::MaxTokens),
        "stop_sequence" => Stop::Usable(StopReason::StopSequence),
        "guardrail_intervened"
        | "content_filtered"
        | "malformed_model_output"
        | "malformed_tool_use"
        | "model_context_window_exceeded" => Stop::Unusable(reason.to_owned()),
        other => {
            return Err(decode(format!(
                "the response has stop reason `{other}`, which this adapter does not model"
            )));
        }
    })
}

/// Normalize a Converse `usage` object.
///
/// Converse reports `inputTokens` **excluding** cache reads and writes ("the
/// `inputTokens` field represents only the non-cached input tokens"), so the
/// convention is additive and the true input is the sum of the three fields. The
/// documentation does not say whether `totalTokens` includes the cache counts, so
/// both sums are accepted. Any other total is noted in the raw usage blob
/// (`kgraf_note`) and the counts still come from their own fields: the call was
/// billed, so its answer is not discarded over a cross-check.
pub fn normalize_usage(raw: &Value) -> Result<TokenUsage, ProviderError> {
    let field = |name: &str| raw.get(name).and_then(Value::as_u64);
    let (Some(input), Some(output)) = (field("inputTokens"), field("outputTokens")) else {
        return Err(ProviderError::UsageMissing { adapter: ADAPTER });
    };
    let read = field("cacheReadInputTokens").unwrap_or(0);
    let write = field("cacheWriteInputTokens").unwrap_or(0);
    // The ledger sums these fields with plain addition, so a set that does not fit
    // in a u64 is refused here, before it can wrap or panic there. The caller keeps
    // a saturated floor on the open row.
    let checked_sum = [input, read, write, output]
        .into_iter()
        .try_fold(0u64, u64::checked_add);
    let Some(with_cache) = checked_sum else {
        return Err(decode(
            "the usage counts add up to more than a 64-bit total; the figures are not credible",
        ));
    };
    let mut usage = TokenUsage::normalize(
        Protocol::Additive,
        input,
        read,
        write,
        output,
        0,
        Some(raw.clone()),
    )
    .map_err(|source| ProviderError::Usage {
        adapter: ADAPTER,
        source,
    })?;
    if let Some(total) = field("totalTokens") {
        // Both sums fit: `with_cache` was checked above and the other is smaller.
        let without_cache = input + output;
        if total != with_cache && total != without_cache {
            // The call is billed whatever the total says, so the answer is kept and
            // the counts stay as the fields report them. The mismatch is written
            // into the raw usage blob, which the ledger row stores.
            let mut annotated = raw.clone();
            if let Some(object) = annotated.as_object_mut() {
                object.insert(
                    "kgraf_note".to_owned(),
                    json!(format!(
                        "totalTokens {total} matches neither inputTokens + outputTokens \
                         ({without_cache}) nor that plus the cache counts ({with_cache}); \
                         counts taken from the individual fields"
                    )),
                );
            }
            usage.raw = Some(annotated);
            tracing::warn!(
                adapter = ADAPTER,
                total,
                without_cache,
                with_cache,
                "totalTokens matches neither documented sum; counts kept from the fields"
            );
        }
    }
    Ok(usage)
}

/// Why a call's usage cannot be priced at the standard rate, if it cannot.
///
/// The price table has one rate set per model and cannot see how a call was served.
/// A `serviceTier` other than `default` (`priority`, `flex` and `reserved` bill at
/// their own rates), and cache writes with a TTL other than the five-minute default,
/// bill differently, so pricing them at the standard rate would state a wrong figure.
/// The caller leaves such a row unreconciled (unpriced) and keeps the answer.
/// Values: <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ServiceTier.html>
/// and `CacheDetail` in the same reference.
pub fn pricing_refusal(raw_usage: &Value, tier: Option<&Value>) -> Option<String> {
    if let Some(tier) = tier {
        match tier.get("type").and_then(Value::as_str) {
            Some("default") => {}
            Some(other) => {
                return Some(format!(
                    "the call was served on the `{other}` service tier, which bills at its own \
                     rate"
                ));
            }
            None => {
                return Some("the response carries a serviceTier this adapter cannot read".into());
            }
        }
    }
    let details = raw_usage.get("cacheDetails").and_then(Value::as_array)?;
    details.iter().find_map(|d| {
        let written = d.get("inputTokens").and_then(Value::as_u64).unwrap_or(0);
        match d.get("ttl").and_then(Value::as_str) {
            Some("5m") => None,
            Some(ttl) if written > 0 => Some(format!(
                "cache writes with a `{ttl}` TTL bill at a different rate than the default"
            )),
            None if written > 0 => Some("cache writes with no readable TTL".to_owned()),
            _ => None,
        }
    })
}

/// Attach the service tier to a usage blob so the ledger row records it.
pub fn record_tier(usage: &mut TokenUsage, tier: Option<&Value>) {
    if let (Some(tier), Some(Value::Object(object))) = (tier, usage.raw.as_mut()) {
        object.insert("kgraf_service_tier".to_owned(), tier.clone());
    }
}

/// The counted fields as reported, with no consistency check: the floor a refused
/// call keeps on its open row.
pub fn lower_bound_usage(raw: &Value) -> Option<TokenUsage> {
    let field = |name: &str| raw.get(name).and_then(Value::as_u64);
    let input = field("inputTokens");
    let output = field("outputTokens");
    if input.is_none() && output.is_none() {
        return None;
    }
    // Each field is clamped to what still fits in the running total, so the sums
    // the ledger takes cannot overflow. The result is still a lower bound.
    let mut room = u64::MAX;
    let mut take = |value: u64| {
        let kept = value.min(room);
        room -= kept;
        kept
    };
    Some(TokenUsage {
        uncached_input: take(input.unwrap_or(0)),
        cache_read: take(field("cacheReadInputTokens").unwrap_or(0)),
        cache_write: take(field("cacheWriteInputTokens").unwrap_or(0)),
        text_output: take(output.unwrap_or(0)),
        reasoning_output: 0,
        raw: Some(raw.clone()),
    })
}

/// A reasoning block's text: the plain text, or a marker carrying the encrypted
/// payload so a redaction is visible and never silently dropped.
fn reasoning_text(block: &Value) -> Result<String, ProviderError> {
    if let Some(text) = block
        .get("reasoningText")
        .and_then(|r| r.get("text"))
        .and_then(Value::as_str)
    {
        return Ok(text.to_owned());
    }
    if let Some(data) = block.get("redactedContent").and_then(Value::as_str) {
        return Ok(format!("[redacted_thinking:{data}]"));
    }
    Err(decode(
        "a reasoningContent block has neither text nor redacted content",
    ))
}

/// Parse the content blocks of a Converse response message.
///
/// A block kind this adapter does not model raises: dropping it would hand back an
/// answer with part of it missing.
pub fn parse_content(message: &Value) -> Result<Vec<Content>, ProviderError> {
    let blocks = message
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| decode("the response message has no `content` array"))?;
    let mut out = Vec::new();
    for block in blocks {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            out.push(Content::text(text));
        } else if let Some(tool) = block.get("toolUse") {
            let text = |name: &str| {
                tool.get(name)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| decode(format!("a toolUse block has no `{name}`")))
            };
            out.push(Content::ToolUse {
                id: text("toolUseId")?,
                name: text("name")?,
                input: tool.get("input").cloned().unwrap_or_else(|| json!({})),
                signature: None,
            });
        } else if let Some(reasoning) = block.get("reasoningContent") {
            out.push(Content::Reasoning {
                text: reasoning_text(reasoning)?,
            });
        } else {
            let kinds: Vec<&str> = block
                .as_object()
                .map(|o| o.keys().map(String::as_str).collect())
                .unwrap_or_default();
            return Err(decode(format!(
                "the response has a content block this adapter does not model: {kinds:?}"
            )));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_provider::message::ToolDefinition;
    use kgraf_provider::{Message, Request};

    fn request() -> Request {
        Request::new("model-a", "hello", 256)
    }

    #[test]
    fn a_minimal_request_has_messages_and_inference_config_only() {
        let body = to_wire(&request(), false);
        assert_eq!(
            body,
            json!({
                "messages": [{"role": "user", "content": [{"text": "hello"}]}],
                "inferenceConfig": {"maxTokens": 256},
            })
        );
    }

    #[test]
    fn reasoning_is_not_serialized_into_the_next_request() {
        // `Provider::resends_reasoning` is false for this adapter: the context
        // estimate leaves reasoning output out of the next prompt because this
        // holds.
        let mut r = request();
        r.messages = vec![kgraf_provider::Message {
            role: Role::Assistant,
            content: vec![
                Content::Reasoning {
                    text: "SECRET-REASONING-TEXT".into(),
                },
                Content::text("visible"),
            ],
        }];
        let text = to_wire(&r, false).to_string();
        assert!(text.contains("visible"), "{text}");
        assert!(!text.contains("SECRET-REASONING-TEXT"), "{text}");
    }

    #[test]
    fn temperature_is_sent_without_float_noise() {
        let mut r = request();
        r.temperature = Some(0.3);
        let text = to_wire(&r, false).to_string();
        assert!(text.contains("\"temperature\":0.3"), "{text}");
    }

    #[test]
    fn system_tools_tool_use_and_tool_results_take_the_converse_shapes() {
        let mut r = request();
        r.system = vec![Content::text("be brief")];
        r.tools = vec![ToolDefinition {
            name: "top_song".into(),
            description: "Get the most popular song.".into(),
            input_schema: json!({"type": "object", "properties": {"sign": {"type": "string"}}}),
        }];
        r.messages = vec![
            Message::user("play something"),
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: "tooluse_1".into(),
                    name: "top_song".into(),
                    input: json!({"sign": "WZPZ"}),
                    signature: None,
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "tooluse_1".into(),
                    content: "ERROR: no such station".into(),
                    is_error: true,
                }],
            },
        ];
        let body = to_wire(&r, false);
        assert_eq!(body["system"], json!([{"text": "be brief"}]));
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"],
            "object"
        );
        assert_eq!(
            body["messages"][1]["content"][0]["toolUse"],
            json!({"toolUseId": "tooluse_1", "name": "top_song", "input": {"sign": "WZPZ"}})
        );
        assert_eq!(
            body["messages"][2]["content"][0]["toolResult"],
            json!({
                "toolUseId": "tooluse_1",
                "content": [{"text": "ERROR: no such station"}],
                "status": "error",
            })
        );
    }

    #[test]
    fn a_successful_tool_result_sends_no_status() {
        let mut r = request();
        r.messages.push(Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                tool_use_id: "t".into(),
                content: "ok".into(),
                is_error: false,
            }],
        });
        let body = to_wire(&r, false);
        assert!(
            body["messages"][0]["content"][1]["toolResult"]
                .get("status")
                .is_none()
        );
    }

    #[test]
    fn consecutive_same_role_messages_are_merged() {
        let mut r = request();
        r.messages.push(Message::user("and one more thing"));
        let body = to_wire(&r, false);
        let messages = body["messages"].as_array().expect("array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"].as_array().expect("blocks").len(), 2);
    }

    #[test]
    fn reasoning_is_not_replayed_and_an_empty_message_is_dropped() {
        let mut r = request();
        r.messages.push(Message {
            role: Role::Assistant,
            content: vec![Content::Reasoning {
                text: "thinking".into(),
            }],
        });
        let body = to_wire(&r, false);
        assert_eq!(body["messages"].as_array().expect("array").len(), 1);
    }

    #[test]
    fn cache_points_follow_breakpoints_only_where_honored() {
        let mut r = request();
        r.system = vec![Content::cached_text("long stable prefix")];
        let off = to_wire(&r, false);
        assert_eq!(off["system"], json!([{"text": "long stable prefix"}]));
        assert!(!off.to_string().contains("cachePoint"));
        let on = to_wire(&r, true);
        assert_eq!(
            on["system"],
            json!([
                {"text": "long stable prefix"},
                {"cachePoint": {"type": "default"}}
            ])
        );
    }

    #[test]
    fn the_conversation_tail_gets_a_cache_point_while_a_slot_is_free() {
        let r = request();
        let on = to_wire(&r, true);
        let blocks = on["messages"][0]["content"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 2);
        assert!(blocks[1].get("cachePoint").is_some());

        // With every slot taken by explicit breakpoints, no fifth is added.
        let mut full = request();
        full.system = (0..MAX_CACHE_POINTS)
            .map(|i| Content::cached_text(format!("prefix {i}")))
            .collect();
        let body = to_wire(&full, true);
        assert_eq!(
            body["messages"][0]["content"]
                .as_array()
                .expect("blocks")
                .len(),
            1
        );
    }

    #[test]
    fn usage_is_additive_and_the_cache_counts_are_kept_apart() {
        // The documentation says `inputTokens` excludes cache reads and writes.
        let raw = json!({
            "inputTokens": 50, "outputTokens": 100, "totalTokens": 1150,
            "cacheReadInputTokens": 900, "cacheWriteInputTokens": 100
        });
        let usage = normalize_usage(&raw).expect("usage");
        assert_eq!(usage.uncached_input, 50);
        assert_eq!(usage.cache_read, 900);
        assert_eq!(usage.cache_write, 100);
        assert_eq!(usage.total_input(), 1050);
        assert_eq!(usage.text_output, 100);
        assert_eq!(usage.raw.as_ref(), Some(&raw));
        // The same numbers read as inclusive would give 50 - 900 and underflow, so
        // this test fails if the convention is ever flipped.
        assert!(TokenUsage::normalize(Protocol::Inclusive, 50, 900, 100, 100, 0, None).is_err());
    }

    #[test]
    fn a_total_that_excludes_the_cache_counts_is_also_accepted() {
        let raw = json!({
            "inputTokens": 50, "outputTokens": 100, "totalTokens": 150,
            "cacheReadInputTokens": 900
        });
        assert_eq!(normalize_usage(&raw).expect("usage").total_input(), 950);
    }

    #[test]
    fn a_total_that_matches_neither_sum_is_noted_and_the_counts_stand() {
        let raw = json!({"inputTokens": 50, "outputTokens": 100, "totalTokens": 999});
        let usage = normalize_usage(&raw).expect("the call is billed, so it is kept");
        assert_eq!((usage.uncached_input, usage.text_output), (50, 100));
        let note = usage.raw.as_ref().and_then(|r| r["kgraf_note"].as_str());
        assert!(note.is_some_and(|n| n.contains("999")), "{note:?}");
    }

    #[test]
    fn blank_text_is_skipped_and_an_empty_tool_result_gets_a_placeholder() {
        assert!(blocks_for(&Content::text(" \n\t"), true).is_empty());
        assert!(blocks_for(&Content::cached_text(""), true).is_empty());
        let empty = Content::ToolResult {
            tool_use_id: "t".into(),
            content: "  ".into(),
            is_error: true,
        };
        let blocks = blocks_for(&empty, false);
        assert_eq!(
            blocks[0]["toolResult"]["content"][0]["text"],
            json!(EMPTY_TOOL_RESULT)
        );
        assert_eq!(blocks[0]["toolResult"]["status"], json!("error"));
    }

    #[test]
    fn usage_without_the_required_counts_is_missing_not_zero() {
        for raw in [
            json!({}),
            json!({"inputTokens": 5}),
            json!({"totalTokens": 5}),
        ] {
            assert!(matches!(
                normalize_usage(&raw),
                Err(ProviderError::UsageMissing { .. })
            ));
        }
    }

    #[test]
    fn every_documented_stop_reason_maps_and_an_unknown_one_raises() {
        for (wire, expected) in [
            ("end_turn", StopReason::EndTurn),
            ("tool_use", StopReason::ToolUse),
            ("max_tokens", StopReason::MaxTokens),
            ("stop_sequence", StopReason::StopSequence),
        ] {
            assert_eq!(map_stop(wire).expect("mapped"), Stop::Usable(expected));
        }
        for wire in [
            "guardrail_intervened",
            "content_filtered",
            "malformed_model_output",
            "malformed_tool_use",
            "model_context_window_exceeded",
        ] {
            assert_eq!(map_stop(wire).expect("mapped"), Stop::Unusable(wire.into()));
        }
        let err = map_stop("something_new").expect_err("unknown");
        assert!(err.to_string().contains("something_new"), "{err}");
    }

    #[test]
    fn response_blocks_parse_including_reasoning_and_an_unknown_block_raises() {
        let message = json!({"role": "assistant", "content": [
            {"reasoningContent": {"reasoningText": {"text": "hmm", "signature": "sig"}}},
            {"reasoningContent": {"redactedContent": "QUJD"}},
            {"text": "answer"},
            {"toolUse": {"toolUseId": "t1", "name": "top_song", "input": {"sign": "X"}}}
        ]});
        let content = parse_content(&message).expect("parsed");
        assert_eq!(content.len(), 4);
        assert_eq!(content[0], Content::Reasoning { text: "hmm".into() });
        assert_eq!(
            content[1],
            Content::Reasoning {
                text: "[redacted_thinking:QUJD]".into()
            }
        );
        assert_eq!(content[2], Content::text("answer"));

        let bad = json!({"content": [{"citationsContent": {}}]});
        let err = parse_content(&bad).expect_err("unknown block");
        assert!(err.to_string().contains("citationsContent"), "{err}");
    }
}
