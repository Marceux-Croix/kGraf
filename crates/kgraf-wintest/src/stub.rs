//! A scripted OpenAI-compatible model on 127.0.0.1, so a real `kgraf worker`
//! run needs no model and no API key.
//!
//! It plays a scripted-reply mode: with `n` tool results in the conversation it answers
//! with tool call `n + 1` of the script, then with a plain "done". It keeps
//! the text of every tool result it receives, by call id, which is what the
//! end-to-end checks judge. One connection per request (`Connection: close`).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

/// One scripted tool call.
#[derive(Clone, Debug)]
pub struct Step {
    pub tool: String,
    pub args: Value,
}

pub fn shell(argv: &[&str]) -> Step {
    Step {
        tool: "shell".into(),
        args: json!({ "command": argv }),
    }
}

#[derive(Debug, Default)]
pub struct Seen {
    pub posts: usize,
    pub gets: usize,
    /// Tool result text by call id (`call_1`, `call_2`, ...).
    pub results: BTreeMap<String, String>,
}

pub struct Stub {
    pub port: u16,
    pub seen: Arc<Mutex<Seen>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Stub {
    pub fn start(script: Vec<Step>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, st2) = (seen.clone(), stop.clone());
        let script = Arc::new(script);
        let thread = std::thread::spawn(move || {
            while !st2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((conn, _)) => {
                        let (seen, script) = (s2.clone(), script.clone());
                        std::thread::spawn(move || {
                            let _ = serve(conn, &script, &seen);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        Ok(Self {
            port,
            seen,
            stop,
            thread: Some(thread),
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    pub fn results(&self) -> BTreeMap<String, String> {
        self.seen
            .lock()
            .map(|s| s.results.clone())
            .unwrap_or_default()
    }

    pub fn posts(&self) -> usize {
        self.seen.lock().map(|s| s.posts).unwrap_or(0)
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The text of a tool message's content: a string or a list of text parts.
fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|p| p.get("text").and_then(Value::as_str).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The next answer for a request: `(content, tool_call, finish_reason)`.
pub fn reply(script: &[Step], req: &Value) -> (Option<String>, Option<Value>, &'static str) {
    let done = req["messages"]
        .as_array()
        .map_or(0, |m| m.iter().filter(|m| m["role"] == "tool").count());
    let has_tools = req["tools"].as_array().is_some_and(|t| !t.is_empty());
    if has_tools && done < script.len() {
        let step = &script[done];
        let call = json!({
            "id": format!("call_{}", done + 1),
            "type": "function",
            "function": {"name": step.tool, "arguments": step.args.to_string()},
        });
        (None, Some(call), "tool_calls")
    } else {
        (Some("done".into()), None, "stop")
    }
}

fn usage() -> Value {
    json!({"prompt_tokens": 1200, "completion_tokens": 80, "total_tokens": 1280})
}

fn serve(conn: TcpStream, script: &[Step], seen: &Mutex<Seen>) -> std::io::Result<()> {
    conn.set_nonblocking(false)?;
    conn.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut length = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let mut out = conn;
    if request_line.starts_with("GET") {
        if let Ok(mut s) = seen.lock() {
            s.gets += 1;
        }
        let body = json!({"data": [{"id": "stub"}], "models": [{"name": "stub"}]}).to_string();
        return respond(&mut out, "application/json", &body);
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let (content, call, finish) = {
        let mut s = seen
            .lock()
            .map_err(|_| std::io::Error::other("stub state poisoned"))?;
        s.posts += 1;
        for m in req["messages"].as_array().into_iter().flatten() {
            if m["role"] == "tool" {
                let id = m["tool_call_id"].as_str().unwrap_or_default().to_owned();
                s.results
                    .entry(id)
                    .or_insert_with(|| content_text(&m["content"]));
            }
        }
        reply(script, &req)
    };
    let model = req["model"].as_str().unwrap_or("stub");
    if req["stream"].as_bool().unwrap_or(false) {
        let mut delta = json!({"role": "assistant"});
        if let Some(c) = &content {
            delta["content"] = json!(c);
        }
        if let Some(call) = &call {
            let mut c = call.clone();
            c["index"] = json!(0);
            delta["tool_calls"] = json!([c]);
        }
        let head =
            json!({"id": "chatcmpl-stub", "object": "chat.completion.chunk", "model": model});
        let chunk = |choices: Value, extra: Option<Value>| {
            let mut v = head.clone();
            v["choices"] = choices;
            if let Some(u) = extra {
                v["usage"] = u;
            }
            format!("data: {v}\n\n")
        };
        let text = chunk(
            json!([{"index": 0, "delta": delta, "finish_reason": null}]),
            None,
        ) + &chunk(
            json!([{"index": 0, "delta": {}, "finish_reason": finish}]),
            None,
        ) + &chunk(json!([]), Some(usage()))
            + "data: [DONE]\n\n";
        return respond(&mut out, "text/event-stream", &text);
    }
    let mut message = json!({"role": "assistant", "content": content});
    if let Some(call) = call {
        message["tool_calls"] = json!([call]);
    }
    let body = json!({
        "id": "chatcmpl-stub",
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": usage(),
    })
    .to_string();
    respond(&mut out, "application/json", &body)
}

fn respond(out: &mut TcpStream, ctype: &str, body: &str) -> std::io::Result<()> {
    write!(
        out,
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(port: u16, body: &Value) -> Value {
        let mut c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let text = body.to_string();
        write!(
            c,
            "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{text}",
            text.len()
        )
        .expect("send");
        let mut resp = String::new();
        c.read_to_string(&mut resp).expect("read");
        let (_, body) = resp.split_once("\r\n\r\n").expect("body");
        serde_json::from_str(body).expect("json")
    }

    #[test]
    fn the_script_is_played_one_call_per_tool_result_then_done() {
        let stub = Stub::start(vec![shell(&["echo", "a"]), shell(&["echo", "b"])]).expect("stub");
        let tools = json!([{"type": "function", "function": {"name": "shell"}}]);
        let first = post(
            stub.port,
            &json!({"messages": [{"role": "user", "content": "go"}], "tools": tools}),
        );
        let call = &first["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["function"]["name"], "shell");
        let args: Value =
            serde_json::from_str(call["function"]["arguments"].as_str().expect("args"))
                .expect("json");
        assert_eq!(args["command"], json!(["echo", "a"]));
        let second = post(
            stub.port,
            &json!({"messages": [
                {"role": "user", "content": "go"},
                {"role": "tool", "tool_call_id": "call_1", "content": [{"type": "text", "text": "a\n"}]},
            ], "tools": tools}),
        );
        assert_eq!(
            second["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_2"
        );
        let third = post(
            stub.port,
            &json!({"messages": [
                {"role": "tool", "tool_call_id": "call_1", "content": "a\n"},
                {"role": "tool", "tool_call_id": "call_2", "content": "b\n"},
            ], "tools": tools}),
        );
        assert_eq!(third["choices"][0]["message"]["content"], "done");
        assert_eq!(third["choices"][0]["finish_reason"], "stop");
        assert_eq!(stub.results()["call_1"], "a\n");
        assert_eq!(stub.results()["call_2"], "b\n");
        assert_eq!(stub.posts(), 3);
    }

    #[test]
    fn without_tools_the_answer_is_plain_text() {
        let (content, call, finish) = reply(&[shell(&["x"])], &json!({"messages": []}));
        assert_eq!(content.as_deref(), Some("done"));
        assert!(call.is_none());
        assert_eq!(finish, "stop");
    }
}
