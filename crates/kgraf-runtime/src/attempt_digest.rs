//! What an attempt tried and what failed, built from its transcript with no
//! model call (decision 0018, amendment of 2026-10-13).
//!
//! When an attempt ends before verification, the next attempt at the same
//! contract starts from the same brief. Without this it learns what stopped
//! the earlier one (`last_stop`) but not what that one already found out: a
//! retry of a 30-round exploration repeated part of it. The digest lists:
//!
//! - the files the attempt changed (the worker's measured `changed_paths`);
//! - each command it ran (the `shell` tool, or a tool whose input names
//!   `command` or `cmd`, as [`crate::progress::command_of`] reads it), with
//!   its exit status and, for a failure, the last meaningful error line of
//!   its output ([`error_line`]); a command kGraf did not run (denied,
//!   refused, invalid) with the first line of why.
//!
//! Lines are de-duplicated: the same command prefix ([`PREFIX_CHARS`]) with
//! the same status and error line is one line with a count, placed where its
//! last run was, so the most recent is last. The whole digest is capped at
//! `max_tokens` by [`kgraf_core::budget::estimate_tokens`]; the oldest command
//! lines are dropped first and the digest says how many were. Deterministic:
//! the same transcript gives the same text.
//!
//! The text quotes commands and their output, which a model wrote, so the
//! caller puts it in a brief only as quoted data.
//!
//! Written new: searched mini-swe-agent, SWE-agent, OpenHands and codex-rs for
//! a retry summary built without a model; they keep the whole history or ask a
//! model to summarize it.

use crate::event::WorkerEvent;
use kgraf_core::budget::estimate_tokens;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;

/// The default cap of a digest, in estimated tokens.
pub const ATTEMPT_DIGEST_TOKENS: u64 = 1_500;
/// Characters of a command kept, and compared when lines are merged.
pub const PREFIX_CHARS: usize = 80;
/// Characters of an error line kept.
pub const ERROR_LINE_CHARS: usize = 200;
/// Files listed before the rest are counted.
const MAX_FILES: usize = 40;

/// How a command ended.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Status {
    /// Ran and exited with this status (`None`: no status in the output).
    Exited(Option<i64>),
    /// Ran and failed.
    Failed(Option<i64>),
    /// kGraf did not run it.
    NotRun,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    command: String,
    status: Status,
    error: Option<String>,
}

struct Line {
    key: Key,
    count: u32,
    /// Position of its last run, for the order.
    last: usize,
}

static EXIT_STATUS: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?m)^(?:ERROR: )?exit status: (-?\d+)")
        .unwrap_or_else(|e| panic!("exit status pattern: {e}"))
});
/// An exception or error line: `ModuleNotFoundError: ...`, `pkg.HTTPError: ...`,
/// `error: ...`, `error[E0425]: ...`, `fatal: ...`, `FAILED ...`.
static ERROR_LIKE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"^(?:[A-Za-z_][A-Za-z0-9_.]*(?:Error|Exception|Exit|Interrupt|Failure)\b|(?i:error)(?:\[[A-Z0-9]+\])?:|(?i:fatal):|FAILED\b|(?i:panic))",
    )
    .unwrap_or_else(|e| panic!("error line pattern: {e}"))
});

/// Cut `text` to `max` characters, marking a cut with `...`.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let kept: String = text.chars().take(max).collect();
        format!("{kept}...")
    }
}

/// Whether a line of command output carries no message of its own: a frame
/// of a traceback, a caret marker, a section label, a chained-exception note.
fn noise(line: &str) -> bool {
    let t = line.trim();
    t.is_empty()
        || t == "stdout:"
        || t == "stderr:"
        || t.starts_with("exit status:")
        || t.starts_with("ERROR: exit status:")
        || t.starts_with("Traceback (most recent call last)")
        || t.starts_with("File \"")
        || t.starts_with("The above exception was the direct cause")
        || t.starts_with("During handling of the above exception")
        || t.starts_with("For more information")
        || (t.starts_with("...<") && t.ends_with(">..."))
        || t.chars().all(|c| matches!(c, '^' | '~' | ' ' | '-' | '='))
        // Summary lines that say only that an earlier error happened.
        || t.starts_with("error: could not compile")
        || t.starts_with("error: aborting due to")
}

/// The last meaningful error line of a failed command's output: the last
/// line that reads as an exception or error, else the last line that is not
/// noise. Cut to [`ERROR_LINE_CHARS`]. `None` for output with neither.
pub fn error_line(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().filter(|l| !noise(l)).collect();
    let pick = lines
        .iter()
        .rev()
        .find(|l| ERROR_LIKE.is_match(l.trim()))
        .or_else(|| lines.last())?;
    Some(cut(pick.trim(), ERROR_LINE_CHARS))
}

/// Why kGraf did not run a call: the first clause of its result's first line
/// (up to the first `;`), without the `ERROR: ` mark. The advice that follows
/// is the same for every such call and the worker's tools say it again.
fn not_run_reason(first_line: &str) -> String {
    let reason = first_line.trim_start_matches("ERROR: ");
    let clause = reason.split(';').next().unwrap_or(reason).trim();
    cut(clause, ERROR_LINE_CHARS)
}

/// The text of a command call: [`crate::progress::command_of`], or, for a
/// `shell` call whose input it cannot read (one string with shell syntax,
/// which kGraf refuses), the raw `command` value.
fn command_text(tool: &str, input: &serde_json::Value) -> Option<String> {
    if let Some(c) = crate::progress::command_of(tool, input) {
        return Some(c);
    }
    if tool != "shell" {
        return None;
    }
    let raw = match input.get("command")? {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    Some(raw.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn status_of(output: &str, is_error: bool) -> Status {
    if crate::progress::did_not_run(output) {
        return Status::NotRun;
    }
    let code = EXIT_STATUS
        .captures(output)
        .and_then(|c| c[1].parse::<i64>().ok());
    if is_error || code.is_some_and(|c| c != 0) {
        Status::Failed(code)
    } else {
        Status::Exited(code)
    }
}

fn render(line: &Line) -> String {
    let Key {
        command,
        status,
        error,
    } = &line.key;
    let what = match status {
        Status::Exited(Some(code)) => format!("exit {code}"),
        Status::Exited(None) => "ok".to_owned(),
        Status::Failed(Some(code)) => format!("failed, exit {code}"),
        Status::Failed(None) => "failed".to_owned(),
        Status::NotRun => "not run".to_owned(),
    };
    let mut s = format!("- `{command}` ({what})");
    if let Some(e) = error {
        s.push_str(": ");
        s.push_str(e);
    }
    if line.count > 1 {
        s.push_str(&format!(" [x{}]", line.count));
    }
    s
}

/// The digest of one attempt: `events` is its transcript, `written` the paths
/// it changed. `None` when it changed nothing and ran no command.
pub fn attempt_digest(
    events: &[WorkerEvent],
    written: &[PathBuf],
    max_tokens: u64,
) -> Option<String> {
    let mut started: HashMap<&str, (&str, &serde_json::Value)> = HashMap::new();
    let mut lines: Vec<Line> = Vec::new();
    let mut index: HashMap<Key, usize> = HashMap::new();
    let mut position = 0usize;
    for event in events {
        match event {
            WorkerEvent::ToolCallStarted {
                call_id,
                tool,
                input,
                ..
            } => {
                started.insert(call_id.as_str(), (tool.as_str(), input));
            }
            WorkerEvent::ToolCallFinished {
                call_id,
                output,
                is_error,
                ..
            } => {
                let Some((tool, input)) = started.remove(call_id.as_str()) else {
                    continue;
                };
                let Some(command) = command_text(tool, input) else {
                    continue;
                };
                position += 1;
                let status = status_of(output, *is_error);
                let error = match status {
                    Status::Failed(_) => error_line(output),
                    Status::NotRun => output.lines().next().map(not_run_reason),
                    Status::Exited(_) => None,
                };
                let key = Key {
                    command: cut(&command, PREFIX_CHARS),
                    status,
                    error,
                };
                match index.get(&key) {
                    Some(&i) => {
                        lines[i].count += 1;
                        lines[i].last = position;
                    }
                    None => {
                        index.insert(key.clone(), lines.len());
                        lines.push(Line {
                            key,
                            count: 1,
                            last: position,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    if lines.is_empty() && written.is_empty() {
        return None;
    }
    lines.sort_by_key(|l| l.last);
    let files = if written.is_empty() {
        "Files written: none.".to_owned()
    } else {
        let mut shown: Vec<String> = written
            .iter()
            .take(MAX_FILES)
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if written.len() > MAX_FILES {
            shown.push(format!("and {} more", written.len() - MAX_FILES));
        }
        format!("Files written: {}.", shown.join(", "))
    };
    let rendered: Vec<String> = lines.iter().map(render).collect();
    // Drop the oldest command lines until the whole fits.
    let mut dropped = 0usize;
    loop {
        let kept = &rendered[dropped..];
        let head = if kept.is_empty() && dropped == 0 {
            "Commands: none.".to_owned()
        } else if dropped == 0 {
            "Commands, oldest first:".to_owned()
        } else {
            format!("Commands, oldest first ({dropped} older line(s) dropped to fit):")
        };
        let text = std::iter::once(files.clone())
            .chain(std::iter::once(head))
            .chain(kept.iter().cloned())
            .collect::<Vec<_>>()
            .join("\n");
        if estimate_tokens(&text) <= max_tokens || kept.is_empty() {
            return Some(text);
        }
        dropped += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(
        id: &str,
        command: serde_json::Value,
        output: &str,
        is_error: bool,
    ) -> [WorkerEvent; 2] {
        [
            WorkerEvent::ToolCallStarted {
                call_id: id.into(),
                tool: "shell".into(),
                input: json!({ "command": command }),
                signature: None,
            },
            WorkerEvent::ToolCallFinished {
                call_id: id.into(),
                output: output.into(),
                is_error,
                duration_ms: Some(1),
            },
        ]
    }

    const PY_FAIL: &str = "ERROR: exit status: 1\nstdout:\nTraceback (most recent call last):\n  \
        File \"<string>\", line 1, in <module>\n    from datasets import load_dataset\n    \
        ^^^^^^^^^^^^^^^^\nModuleNotFoundError: No module named 'datasets'\n\nstderr:\n";

    #[test]
    fn the_error_line_is_the_exception_not_the_frame_or_the_marker() {
        assert_eq!(
            error_line(PY_FAIL).as_deref(),
            Some("ModuleNotFoundError: No module named 'datasets'")
        );
        // Chained: the last exception wins.
        let chained = "ERROR: exit status: 1\nstdout:\nTraceback (most recent call last):\n\
            httpx2.HTTPStatusError: Client error '401 Unauthorized'\n\nThe above exception was \
            the direct cause of the following exception:\n\nTraceback (most recent call last):\n\
            huggingface_hub.errors.HfHubHTTPError: 401 Client Error\n\nstderr:\n";
        assert_eq!(
            error_line(chained).as_deref(),
            Some("huggingface_hub.errors.HfHubHTTPError: 401 Client Error")
        );
        // Lines after the exception (a stack, a version banner) are not it.
        assert_eq!(
            error_line(
                "ERROR: exit status: 1\nstderr:\nTypeError: x is not a function\n    at \
                 Object.<anonymous> (/w/a.js:1:1)\n    at node:internal/main:1:1\n\nNode.js v20.11.0\n"
            )
            .as_deref(),
            Some("TypeError: x is not a function")
        );
        // No exception: the last line that says something.
        assert_eq!(
            error_line("ERROR: exit status: 127\nstdout:\n/usr/bin/env: 'pandas': No such file or directory\n\nstderr:\n")
                .as_deref(),
            Some("/usr/bin/env: 'pandas': No such file or directory")
        );
        // cargo: the error, not the summary that an error happened.
        assert_eq!(
            error_line("exit status: 101\nstderr:\nerror[E0425]: cannot find value `x`\nerror: could not compile `a` (lib) due to 1 previous error\n")
                .as_deref(),
            Some("error[E0425]: cannot find value `x`")
        );
    }

    #[test]
    fn identical_prefix_and_error_collapse_with_a_count_and_order_is_by_last_run() {
        let mut events = Vec::new();
        events.extend(call(
            "1",
            json!(["python3", "-c", "import datasets"]),
            PY_FAIL,
            true,
        ));
        events.extend(call(
            "2",
            json!(["pip", "install", "datasets"]),
            "exit status: 0\nstdout:\nok\n",
            false,
        ));
        events.extend(call(
            "3",
            json!(["python3", "-c", "import datasets"]),
            PY_FAIL,
            true,
        ));
        let digest = attempt_digest(&events, &[], ATTEMPT_DIGEST_TOKENS).expect("digest");
        assert_eq!(
            digest,
            "Files written: none.\nCommands, oldest first:\n\
             - `pip install datasets` (exit 0)\n\
             - `python3 -c import datasets` (failed, exit 1): ModuleNotFoundError: No module named 'datasets' [x2]"
        );
        // A different error under the same prefix is its own line.
        events.extend(call(
            "4",
            json!(["python3", "-c", "import datasets"]),
            "ERROR: exit status: 1\nstdout:\nImportError: cannot import name 'x'\n",
            true,
        ));
        let digest = attempt_digest(&events, &[], ATTEMPT_DIGEST_TOKENS).expect("digest");
        assert!(
            digest.ends_with("ImportError: cannot import name 'x'"),
            "{digest}"
        );
    }

    #[test]
    fn a_command_kgraf_did_not_run_says_why_and_reads_are_left_out() {
        let mut events = vec![
            WorkerEvent::ToolCallStarted {
                call_id: "r".into(),
                tool: "read_file".into(),
                input: json!({"path": "a.txt"}),
                signature: None,
            },
            WorkerEvent::ToolCallFinished {
                call_id: "r".into(),
                output: "text".into(),
                is_error: false,
                duration_ms: None,
            },
        ];
        events.extend(call(
            "s",
            json!("python3 -c \"x\" | head"),
            "ERROR: invalid call: invalid input for tool `shell`: `command` is one string holding shell syntax",
            true,
        ));
        let digest = attempt_digest(&events, &[PathBuf::from("src/a.py")], 1_500).expect("d");
        assert_eq!(
            digest,
            "Files written: src/a.py.\nCommands, oldest first:\n\
             - `python3 -c \"x\" | head` (not run): invalid call: invalid input for tool `shell`: \
             `command` is one string holding shell syntax"
        );
    }

    #[test]
    fn the_cap_drops_the_oldest_lines_and_says_how_many() {
        let mut events = Vec::new();
        for i in 0..200 {
            events.extend(call(
                &i.to_string(),
                json!(["python3", "-c", format!("step_{i}()")]),
                &format!(
                    "ERROR: exit status: 1\nstdout:\nNameError: name 'step_{i}' is not defined\n"
                ),
                true,
            ));
        }
        let digest = attempt_digest(&events, &[], ATTEMPT_DIGEST_TOKENS).expect("digest");
        assert!(
            estimate_tokens(&digest) <= ATTEMPT_DIGEST_TOKENS,
            "{}",
            estimate_tokens(&digest)
        );
        let head = digest.lines().nth(1).expect("head");
        let dropped: usize = head
            .split(['(', ' '])
            .find_map(|w| w.parse().ok())
            .expect("a count");
        assert_eq!(digest.lines().count() - 2 + dropped, 200, "{head}");
        assert!(
            digest.ends_with("NameError: name 'step_199' is not defined"),
            "the newest is kept"
        );
        assert!(!digest.contains("step_0()"), "the oldest is dropped");
    }

    #[test]
    fn nothing_tried_is_no_digest() {
        assert_eq!(attempt_digest(&[], &[], ATTEMPT_DIGEST_TOKENS), None);
    }
}
