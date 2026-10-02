//! A tiny OpenAI-compatible chat server for the soak driver, run as a child process of the driver.
//!
//! It runs in its own process so its sockets and threads never count as the driver's. It plans
//! (a fixed two-contract plan), plays a worker (one `apply_patch` call that adds a file, then
//! "done"), and answers the orchestrator's yes/no checks. It makes no model call and has no
//! network access beyond the loopback listener.
//!
//! Protocol with the driver: the first stdout line is `PORT <n>`. The server exits when its
//! stdin closes, so it never outlives a driver that was killed.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

/// The plan every run gets: two independent contracts, one file each.
pub const PLAN: &str = r#"{"contracts":[
  {"id":"a","instruction":"Create the file soak/a.txt containing the line ok.","acceptance":"soak/a.txt exists","writes":[{"path":"soak/a.txt"}],"tools":[]},
  {"id":"b","instruction":"Create the file soak/b.txt containing the line ok.","acceptance":"soak/b.txt exists","writes":[{"path":"soak/b.txt"}],"tools":[]}
]}"#;

/// Bind a loopback port, announce it on stdout, serve until stdin closes.
pub fn serve() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    println!("PORT {port}");
    std::io::stdout().flush()?;
    std::thread::spawn(|| {
        let mut sink = [0u8; 64];
        while matches!(std::io::stdin().read(&mut sink), Ok(n) if n > 0) {}
        std::process::exit(0);
    });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        std::thread::spawn(move || handle(stream));
    }
    Ok(())
}

fn respond(stream: &mut TcpStream, content_type: &str, body: &[u8]) -> bool {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).is_ok() && stream.write_all(body).is_ok()
}

fn handle(mut stream: TcpStream) {
    while let Some((path, body)) = read_request(&mut stream) {
        let sent = if path.contains("/models") || path.contains("/tags") {
            let listing =
                json!({"data": [{"id": "stub-model"}], "models": [{"name": "stub-model"}]});
            respond(
                &mut stream,
                "application/json",
                listing.to_string().as_bytes(),
            )
        } else {
            let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let streaming = request["stream"].as_bool().unwrap_or(false);
            let out = render(&request, answer(&request), streaming);
            let kind = if streaming {
                "text/event-stream"
            } else {
                "application/json"
            };
            respond(&mut stream, kind, &out)
        };
        if !sent {
            return;
        }
    }
}

/// One request: its path and body, or `None` when the peer closed.
fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).to_string();
            let length = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if buffer.len() >= end + 4 + length {
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_owned();
                return Some((path, buffer[end + 4..end + 4 + length].to_vec()));
            }
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

/// All text of the messages with one of `roles`: string contents and text parts.
fn texts(request: &Value, roles: &[&str]) -> String {
    let mut out = String::new();
    for m in request["messages"].as_array().into_iter().flatten() {
        if !roles.contains(&m["role"].as_str().unwrap_or("")) {
            continue;
        }
        match &m["content"] {
            Value::String(s) => out.push_str(s),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .for_each(|s| out.push_str(s)),
            _ => {}
        }
        out.push('\n');
    }
    out
}

/// What to say next: `(content, tool_call)`.
fn answer(request: &Value) -> (Option<String>, Option<Value>) {
    let system = texts(request, &["system", "developer"]);
    let has_tools = request["tools"].as_array().is_some_and(|t| !t.is_empty());
    if system.contains("Split the user's instruction") {
        return (Some(PLAN.to_owned()), None);
    }
    if has_tools {
        let done = request["messages"]
            .as_array()
            .map_or(0, |m| m.iter().filter(|m| m["role"] == "tool").count());
        if done > 0 {
            return (Some("done".to_owned()), None);
        }
        let user = texts(request, &["user"]);
        let file = ["soak/b.txt", "soak/a.txt"]
            .into_iter()
            .find(|f| user.contains(f))
            .unwrap_or("soak/a.txt");
        let patch = format!("*** Begin Patch\n*** Add File: {file}\n+ok\n*** End Patch");
        let call = json!({
            "id": "call_1", "type": "function",
            "function": {"name": "apply_patch", "arguments": json!({"patch": patch}).to_string()}
        });
        return (None, Some(call));
    }
    if system.contains("still serves") {
        return (
            Some(r#"{"serves":true,"reason":"on track"}"#.to_owned()),
            None,
        );
    }
    eprintln!(
        "soak stub: unrecognized request, system starts: {}",
        system.chars().take(160).collect::<String>()
    );
    (Some("{}".to_owned()), None)
}

fn render(
    request: &Value,
    (content, call): (Option<String>, Option<Value>),
    streaming: bool,
) -> Vec<u8> {
    let model = request["model"].as_str().unwrap_or("stub-model");
    let finish = if call.is_some() { "tool_calls" } else { "stop" };
    let usage = json!({"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110});
    if !streaming {
        let mut message = json!({"role": "assistant", "content": content});
        if let Some(c) = call {
            message["tool_calls"] = json!([c]);
        }
        return json!({
            "id": "chatcmpl-soak", "object": "chat.completion", "model": model,
            "choices": [{"index": 0, "message": message, "finish_reason": finish}],
            "usage": usage
        })
        .to_string()
        .into_bytes();
    }
    let chunk = |choices: Value| json!({"id": "chatcmpl-soak", "object": "chat.completion.chunk", "model": model, "choices": choices});
    let mut delta = json!({"role": "assistant"});
    if let Some(c) = content {
        delta["content"] = json!(c);
    }
    if let Some(mut c) = call {
        c["index"] = json!(0);
        delta["tool_calls"] = json!([c]);
    }
    let mut usage_chunk = chunk(json!([]));
    usage_chunk["usage"] = usage;
    let mut out = String::new();
    for c in [
        chunk(json!([{"index": 0, "delta": delta, "finish_reason": null}])),
        chunk(json!([{"index": 0, "delta": {}, "finish_reason": finish}])),
        usage_chunk,
    ] {
        out.push_str(&format!("data: {c}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out.into_bytes()
}
