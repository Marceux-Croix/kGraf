//! Decision 0067: a steering note injected after a tool round is
//! a text block at the end of the user turn that carries the tool results.
//!
//! - OpenAI Responses: the `function_call_output` items, then one `user`
//!   message with the note.
//! - Chat Completions: the `tool` messages, then one `user` message with the
//!   note.
//!
//! In both, the model's reasoning (a Responses reasoning summary, or a Chat
//! vendor's `reasoning_content`) is display only and never resent as input.
//! Asserted on each adapter's request body builder, which is what `complete`
//! and `stream` post.

use kgraf_core::Protocol;
use kgraf_provider::{Content, Message, ProviderConfig, Request, Role};
use kgraf_provider_openai::{ChatCompletionsAdapter, OpenAiAdapter, UsageDialect};
use serde_json::{Value, json};

const NOTE: &str = "Note from the user: keep the public API.";

fn request() -> Request {
    let mut request = Request::new("model-a", "unused", 64);
    request.messages = vec![
        Message::user("fix the bug"),
        Message {
            role: Role::Assistant,
            content: vec![
                Content::Reasoning {
                    text: "private chain of thought".into(),
                },
                Content::text("reading two files"),
                Content::ToolUse {
                    id: "call_a".into(),
                    name: "read".into(),
                    input: json!({"path": "a.rs"}),
                },
                Content::ToolUse {
                    id: "call_b".into(),
                    name: "read".into(),
                    input: json!({"path": "b.rs"}),
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![
                Content::ToolResult {
                    tool_use_id: "call_a".into(),
                    content: "fn a() {}".into(),
                    is_error: false,
                },
                Content::ToolResult {
                    tool_use_id: "call_b".into(),
                    content: "no such file".into(),
                    is_error: true,
                },
                Content::text(NOTE),
            ],
        },
    ];
    request
}

fn config(adapter: &'static str) -> ProviderConfig {
    ProviderConfig::new(
        adapter,
        "http://127.0.0.1:9",
        "test-key",
        vec!["model-a".into()],
        Protocol::Inclusive,
    )
    .expect("config")
}

#[test]
fn responses_sends_the_function_call_outputs_then_a_user_message_with_the_note() {
    let adapter = OpenAiAdapter::new(config("openai")).expect("adapter");
    let body = adapter.to_wire(&request());
    let input = body["input"].as_array().expect("input");
    let shape: Vec<(Option<&str>, Option<&str>)> = input
        .iter()
        .map(|item| (item["type"].as_str(), item["role"].as_str()))
        .collect();
    assert_eq!(
        shape,
        [
            (None, Some("user")),
            (None, Some("assistant")),
            (Some("function_call"), None),
            (Some("function_call"), None),
            (Some("function_call_output"), None),
            (Some("function_call_output"), None),
            (None, Some("user")),
        ]
    );
    assert_eq!(input[4]["call_id"], "call_a");
    assert_eq!(input[5]["call_id"], "call_b");
    assert_eq!(input[5]["output"], "ERROR: no such file");
    assert_eq!(input[6]["content"], NOTE);
    assert_no_reasoning(&body);
}

#[test]
fn chat_completions_sends_the_tool_messages_then_a_user_message_with_the_note() {
    let adapter =
        ChatCompletionsAdapter::new(config("openai-chat"), UsageDialect::Details).expect("adapter");
    let body = adapter.to_wire(&request());
    let messages = body["messages"].as_array().expect("messages");
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().expect("role"))
        .collect();
    assert_eq!(roles, ["user", "assistant", "tool", "tool", "user"]);
    assert_eq!(messages[1]["tool_calls"].as_array().map(Vec::len), Some(2));
    assert_eq!(messages[2]["tool_call_id"], "call_a");
    assert_eq!(messages[3]["tool_call_id"], "call_b");
    assert_eq!(messages[4]["content"], NOTE);
    assert_no_reasoning(&body);
}

fn assert_no_reasoning(body: &Value) {
    let raw = body.to_string();
    assert!(
        !raw.contains("private chain of thought"),
        "reasoning is never resent: {raw}"
    );
    assert!(!raw.contains("\"reasoning\""), "no reasoning item: {raw}");
}
