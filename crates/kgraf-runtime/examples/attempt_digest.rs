//! Print the retry digest of a stored worker transcript (read only).
//!
//! ```text
//! cargo run -p kgraf-runtime --example attempt_digest -- <worker.jsonl> [written-path ...]
//! ```
//!
//! The transcript is the JSON lines a worker writes (`{"seq": N, "event": {...}}`);
//! the optional paths stand for the files the attempt changed, which the
//! orchestrator takes from the worker's result.

use kgraf_runtime::{ATTEMPT_DIGEST_TOKENS, WorkerEvent, attempt_digest};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: attempt_digest <worker.jsonl> [written ...]")?;
    let written: Vec<PathBuf> = args.map(PathBuf::from).collect();
    let text = std::fs::read_to_string(&path)?;
    let mut events = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut entry: serde_json::Value = serde_json::from_str(line)?;
        let event: WorkerEvent = serde_json::from_value(entry["event"].take())?;
        events.push(event);
    }
    match attempt_digest(&events, &written, ATTEMPT_DIGEST_TOKENS) {
        Some(digest) => println!("{digest}"),
        None => println!("(no digest: nothing written and no command run)"),
    }
    Ok(())
}
