//! Decision 0067: a steering note injected after a tool round
//! is a text block at the end of the user turn that carries the tool results.
//! On Anthropic Messages that turn must be `tool_result` blocks first, then the
//! `text` block, in one `user` message; and the model's reasoning from the
//! previous turn is never sent back as input (extended thinking is kept
//! off, so no signed thinking chain exists to preserve).
//!
//! Asserted on the body the adapter actually sends, captured by a local server.

use kgraf_core::Protocol;
use kgraf_provider::{
    Content, Ledger, Message, Provider, ProviderConfig, Request, Role, StopReason,
};
use kgraf_provider_anthropic::AnthropicAdapter;
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn transcript() -> Vec<Message> {
    vec![
        Message::user("fix the bug"),
        Message {
            role: Role::Assistant,
            content: vec![
                Content::Reasoning {
                    text: "private chain of thought".into(),
                },
                Content::text("reading two files"),
                Content::ToolUse {
                    id: "toolu_a".into(),
                    name: "read".into(),
                    input: json!({"path": "a.rs"}),
                },
                Content::ToolUse {
                    id: "toolu_b".into(),
                    name: "read".into(),
                    input: json!({"path": "b.rs"}),
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![
                Content::ToolResult {
                    tool_use_id: "toolu_a".into(),
                    content: "fn a() {}".into(),
                    is_error: false,
                },
                Content::ToolResult {
                    tool_use_id: "toolu_b".into(),
                    content: "no such file".into(),
                    is_error: true,
                },
                Content::text("Note from the user: keep the public API."),
            ],
        },
    ]
}

#[tokio::test]
async fn a_note_after_tool_results_is_sent_as_tool_result_blocks_then_one_text_block() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "model": "model-a",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 10, "output_tokens": 2}
        })))
        .mount(&server)
        .await;
    let config = ProviderConfig::new(
        "anthropic",
        server.uri(),
        "test-key",
        vec!["model-a".into()],
        Protocol::Additive,
    )
    .expect("config");
    let adapter = AnthropicAdapter::new(config).expect("adapter");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin("anthropic", Protocol::Additive, "model-a", "w", "t")
        .expect("begin");
    let mut request = Request::new("model-a", "unused", 64);
    request.messages = transcript();
    let response = adapter.complete(&request, &token).await.expect("complete");
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    let received = server.received_requests().await.expect("recorded");
    let body: Value = serde_json::from_slice(&received[0].body).expect("json");
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3, "one wire message per transcript turn");

    let turn = &messages[2];
    assert_eq!(turn["role"], "user");
    let blocks = turn["content"].as_array().expect("blocks");
    let kinds: Vec<&str> = blocks
        .iter()
        .map(|b| b["type"].as_str().expect("type"))
        .collect();
    assert_eq!(kinds, ["tool_result", "tool_result", "text"]);
    assert_eq!(blocks[0]["tool_use_id"], "toolu_a");
    assert_eq!(blocks[1]["tool_use_id"], "toolu_b");
    assert_eq!(blocks[1]["is_error"], true);
    assert_eq!(
        blocks[2]["text"],
        "Note from the user: keep the public API."
    );

    // Reasoning is display only: never resent as input.
    let assistant = messages[1]["content"].as_array().expect("blocks");
    let kinds: Vec<&str> = assistant
        .iter()
        .map(|b| b["type"].as_str().expect("type"))
        .collect();
    assert_eq!(kinds, ["text", "tool_use", "tool_use"]);
    let raw = String::from_utf8_lossy(&received[0].body);
    assert!(!raw.contains("private chain of thought"));
    assert!(!raw.contains("thinking"));
}
