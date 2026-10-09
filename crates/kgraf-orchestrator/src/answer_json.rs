//! The JSON object in a model's answer, and where an answer that is not one
//! went wrong.
//!
//! A model answers with one JSON object, but the text around it varies: a code
//! fence, a sentence before it, a reasoning draft that a reasoning model leaks
//! before a literal `</think>`, a second object after it. Three rules decide
//! which object is the answer, none of which guesses at content:
//!
//! 1. Text through the last `</think>` is reasoning, never the answer.
//! 2. The answer is the last top-level object that parses (one not inside
//!    another). A draft object that does not parse, followed by the real one,
//!    gives the real one.
//! 3. A raw control character (a line break or a tab typed inside a string,
//!    which JSON forbids) is escaped and the object parsed again. The escape is
//!    lossless: inside a string the character can only have meant itself.
//!
//! An answer with no object that parses is refused, and the refusal quotes the
//! answer around the point the parser stopped, marked, so a re-ask shows the
//! model its own mistake instead of the first lines of its answer.
//!
//! An answer the model ended itself inside its object (a normal stop, far
//! below the output limit) is read by [`closing`]: when the text is a prefix
//! of a document that lacks only closing brackets and braces, and stops right
//! after a complete value, the closers are the only thing that can follow, so
//! they are appended. Anything else (a stop inside a string, after a key, a
//! comma, an opening bracket or a number that may be cut) is not guessed at.
//! The caller parses the closed answer exactly as it parses a whole one and
//! records that it closed it.
//!
//! Searched before writing: `serde_json_lenient` (MIT/Apache-2.0, Google)
//! accepts control characters in strings, but finding the top-level objects
//! needs a string-aware scan of its own, which makes the escape a few lines and
//! spares a second JSON parser; the repair crates (`json-repair`,
//! `jsonrepair-rs`, `llm_json`, `anyrepair`) insert quotes, commas and brackets
//! they guess, and a guessed plan is what `plan.rs` refuses to run.

use crate::plan::PlanError;

/// Marks where the parser stopped, inside a quoted window of the answer.
pub const ERROR_MARK: &str = "[ERROR HERE]";

/// Characters of the answer quoted on each side of the error point.
const WINDOW_CHARS: usize = 160;

/// The answer without the reasoning a model leaked before it: the text after
/// the last `</think>`, or the whole text when it has none.
pub fn after_reasoning(text: &str) -> &str {
    const END: &str = "</think>";
    match text.rfind(END) {
        Some(i) => &text[i + END.len()..],
        None => text,
    }
}

/// Where a top-level object starting at byte `start` ends (inclusive), by a
/// string-aware brace count; `None` when it never closes.
fn balanced_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `span` with every raw control character inside a string escaped
/// (`\n`, `\r`, `\t`, or `\u00XX`); `None` when there was none to escape.
fn escape_raw_controls(span: &str) -> Option<String> {
    let mut out = String::with_capacity(span.len() + 16);
    let mut in_string = false;
    let mut escaped = false;
    let mut changed = false;
    for c in span.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            } else if (c as u32) < 0x20 {
                changed = true;
                match c {
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    _ => out.push_str(&format!("\\u{:04x}", c as u32)),
                }
                continue;
            }
        } else if c == '"' {
            in_string = true;
        }
        out.push(c);
    }
    changed.then_some(out)
}

/// One value parsed from the start of `text`, ignoring what follows it, and
/// the bytes it took.
fn parse_prefix(text: &str) -> Result<(serde_json::Value, usize), serde_json::Error> {
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>();
    match stream.next() {
        Some(Ok(v)) => Ok((v, stream.byte_offset())),
        Some(Err(e)) => Err(e),
        // Unreachable for text starting with `{`; mapped to the parser's own
        // end-of-input error rather than a panic.
        None => serde_json::from_str::<serde_json::Value>("").map(|v| (v, 0)),
    }
}

/// The byte offset in `text` of serde's 1-based `line` and `column`.
fn offset_of(text: &str, line: usize, column: usize) -> usize {
    let mut offset = 0;
    for (n, l) in text.split_inclusive('\n').enumerate() {
        if n + 1 == line {
            // serde counts the column in bytes; clamp to a char boundary.
            let mut at = (offset + column.saturating_sub(1)).min(text.len());
            while !text.is_char_boundary(at) {
                at -= 1;
            }
            return at;
        }
        offset += l.len();
    }
    text.len()
}

/// `text` around byte `at`, with [`ERROR_MARK`] at `at`, quoted on one line
/// (line breaks shown as `\n`), with `...` where it was cut.
pub fn window(text: &str, at: usize) -> String {
    let at = at.min(text.len());
    let before: String = {
        let head: Vec<char> = text[..at].chars().collect();
        let from = head.len().saturating_sub(WINDOW_CHARS);
        let s: String = head[from..].iter().collect();
        if from > 0 { format!("...{s}") } else { s }
    };
    let after: String = {
        let mut it = text[at..].chars();
        let s: String = it.by_ref().take(WINDOW_CHARS).collect();
        if it.next().is_some() {
            format!("{s}...")
        } else {
            s
        }
    };
    format!("{before}{ERROR_MARK}{after}")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// What the model can do about a parse error, when the error names a kind the
/// rules above did not fix.
fn parse_hint(e: &serde_json::Error) -> &'static str {
    let msg = e.to_string();
    if msg.contains("control character") {
        " (a raw line break inside a string: a string before this point was probably not \
         closed with a quote; close it, or write a line break inside a string as \\n)"
    } else if msg.contains("key must be a string") {
        " (every key is a string in double quotes)"
    } else if msg.contains("trailing comma") {
        " (no comma before a closing bracket)"
    } else if e.is_eof() {
        " (the answer ends before the object closes)"
    } else {
        ""
    }
}

/// The refusal for an object at `start` in `text` that did not parse.
fn refusal(text: &str, start: usize, e: &serde_json::Error) -> PlanError {
    let mut at = start + offset_of(&text[start..], e.line(), e.column());
    // serde places a control-character error after the character; the mark
    // goes on the character itself (the end of the line it broke).
    if e.to_string().contains("control character") && at > start {
        at -= 1;
        while !text.is_char_boundary(at) {
            at -= 1;
        }
    }
    // Line and column in the whole answer, as the model wrote it.
    let line = text[..at].matches('\n').count() + 1;
    let column = text[..at].rfind('\n').map_or(at, |nl| at - nl - 1) + 1;
    let kind = e.to_string();
    let kind = kind
        .rsplit_once(" at line ")
        .map_or(kind.as_str(), |(k, _)| k)
        .to_owned();
    PlanError::BadOutput {
        reason: format!(
            "{kind} at line {line} column {column}{}; the answer there reads \"{}\"",
            parse_hint(e),
            window(text, at)
        ),
        excerpt: String::new(),
    }
}

/// The answer's JSON object (see the module rules), or a refusal that quotes
/// where it went wrong. With `key`, the last object holding that top-level
/// key wins over a later one without it (a model that adds an example object
/// after its answer).
pub fn json_object_with(text: &str, key: Option<&str>) -> Result<serde_json::Value, PlanError> {
    let body = after_reasoning(text);
    let offset = text.len() - body.len();
    let bytes = text.as_bytes();
    let mut found: Vec<serde_json::Value> = Vec::new();
    let mut first_error: Option<PlanError> = None;
    let mut i = offset;
    while let Some(rel) = text[i..].find('{') {
        let start = i + rel;
        match parse_prefix(&text[start..]) {
            Ok((v, used)) => {
                found.push(v);
                i = start + used.max(1);
                continue;
            }
            Err(e) => {
                let end = balanced_end(bytes, start);
                let span = &text[start..=end.unwrap_or(text.len() - 1)];
                if let Some(fixed) = escape_raw_controls(span)
                    && let Ok((v, _)) = parse_prefix(&fixed)
                {
                    found.push(v);
                } else if first_error.is_none() {
                    first_error = Some(refusal(text, start, &e));
                }
                // The failed object's inner objects are fragments of it, never
                // an answer: the scan goes on after it.
                i = end.map_or(text.len(), |e| e + 1);
            }
        }
    }
    let pick = match key {
        Some(k) => found
            .iter()
            .rposition(|v| v.get(k).is_some())
            .or_else(|| found.iter().rposition(serde_json::Value::is_object)),
        None => found.iter().rposition(serde_json::Value::is_object),
    };
    if let Some(n) = pick {
        return Ok(found.swap_remove(n));
    }
    Err(first_error.unwrap_or_else(|| PlanError::BadOutput {
        reason: "no JSON object".into(),
        excerpt: body.chars().take(200).collect(),
    }))
}

/// The byte where the answer's open object starts: the first top-level
/// object, found as [`json_object_with`] finds them, that never closes before
/// the text ends. `None` when every object the answer starts also closes.
fn open_object_start(text: &str) -> Option<usize> {
    let body = after_reasoning(text);
    let bytes = text.as_bytes();
    let mut i = text.len() - body.len();
    while let Some(rel) = text[i..].find('{') {
        let start = i + rel;
        if let Ok((_, used)) = parse_prefix(&text[start..]) {
            i = start + used.max(1);
            continue;
        }
        match balanced_end(bytes, start) {
            Some(end) => i = end + 1,
            None => return Some(start),
        }
    }
    None
}

/// How an answer that may end inside an open JSON object can be closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Closing {
    /// Every object the answer starts also closes, or the open one breaks
    /// before the text ends (a parser meets an error before it runs out of
    /// text, such as a line break inside a string that was never closed): the
    /// answer has a mistake of its own, and it did not merely stop early.
    NotStoppedEarly,
    /// The answer ends inside an open object right after a complete value:
    /// `text` is the answer (without trailing white space or a closing code
    /// fence) with `appended`, the closing brackets and braces in order.
    Closable { text: String, appended: String },
    /// The answer ends inside an open object at a point no closing bracket
    /// completes; the reason says where it stops.
    Unclosable(&'static str),
}

/// Whether, and how, the answer's open object can be closed by appending
/// closing brackets and braces only (see the module rules). Never inside a
/// string: a string cut mid-way may have been cut mid-word.
pub fn closing(text: &str) -> Closing {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Last {
        Open,
        Comma,
        Colon,
        Key,
        Value,
    }
    let Some(start) = open_object_start(text) else {
        return Closing::NotStoppedEarly;
    };
    let mut tail = text[start..].trim_end();
    // A model that closes its code fence but not its object: the fence is
    // not part of the JSON.
    if let Some(t) = tail.strip_suffix("```") {
        tail = t.trim_end();
    }
    // Stopped early only when the parser reads to the end of the text
    // without an error of another kind. A string left open in the middle of
    // an answer the model finished turns the rest into string text: the
    // parser stops at the first raw line break, far from the end, and the
    // refusal that says so is what the model needs, not a request to finish.
    match parse_prefix(tail) {
        Err(e) if e.is_eof() => {}
        _ => return Closing::NotStoppedEarly,
    }
    let mut stack: Vec<char> = Vec::new();
    let mut last = Last::Open;
    let mut in_string = false;
    let mut is_key = false;
    let mut escaped = false;
    let mut word = String::new();
    for c in tail.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
                last = if is_key { Last::Key } else { Last::Value };
            }
            continue;
        }
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '.') {
            word.push(c);
            continue;
        }
        if !word.is_empty() {
            // A number or a literal followed by a delimiter is complete.
            word.clear();
            last = Last::Value;
        }
        match c {
            '{' => {
                stack.push('}');
                last = Last::Open;
            }
            '[' => {
                stack.push(']');
                last = Last::Open;
            }
            '}' | ']' => {
                if stack.pop() != Some(c) {
                    return Closing::Unclosable("a closing bracket does not match its opening one");
                }
                last = Last::Value;
            }
            '"' => {
                in_string = true;
                is_key = stack.last() == Some(&'}') && matches!(last, Last::Open | Last::Comma);
            }
            ':' => last = Last::Colon,
            ',' => last = Last::Comma,
            c if c.is_whitespace() => {}
            _ => return Closing::Unclosable("a character JSON does not allow there"),
        }
    }
    if in_string {
        return Closing::Unclosable("the answer stops inside a string");
    }
    if !word.is_empty() {
        if !matches!(word.as_str(), "true" | "false" | "null") {
            return Closing::Unclosable(
                "the answer stops after a number or a word that may be cut",
            );
        }
        last = Last::Value;
    }
    match last {
        Last::Value if !stack.is_empty() => {
            let appended: String = stack.iter().rev().collect();
            Closing::Closable {
                text: format!("{}{appended}", &text[..start + tail.len()]),
                appended,
            }
        }
        Last::Value => Closing::NotStoppedEarly,
        Last::Open => Closing::Unclosable("the answer stops right after an opening bracket"),
        Last::Comma => Closing::Unclosable("the answer stops after a comma"),
        Last::Colon | Last::Key => Closing::Unclosable("the answer stops after a key"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_draft_before_the_last_think_end_is_reasoning_not_the_answer() {
        let text = "{\"contracts\": [{\"id\": \"draft\"},...\n</think>\n\n{\"contracts\": [{\"id\": \"c1\"}]}";
        let v = json_object_with(text, Some("contracts")).expect("parsed");
        assert_eq!(v["contracts"][0]["id"], "c1");
    }

    #[test]
    fn the_last_parseable_object_wins_and_a_broken_one_before_it_is_skipped() {
        let text = "draft: {\"contracts\": [ {\"id\": \"x\"} oops }\nfinal: {\"contracts\": []}";
        let v = json_object_with(text, Some("contracts")).expect("parsed");
        assert_eq!(v["contracts"], serde_json::json!([]));
    }

    #[test]
    fn an_inner_object_of_a_broken_answer_is_never_taken_as_the_answer() {
        let text = "{\"contracts\": [{\"id\": \"c1\"} {\"id\": \"c2\"}]}";
        let err = json_object_with(text, Some("contracts")).expect_err("refused");
        let msg = err.to_string();
        assert!(msg.contains(ERROR_MARK), "{msg}");
        assert!(msg.contains("line 1 column"), "{msg}");
    }

    #[test]
    fn a_raw_line_break_inside_a_string_is_escaped() {
        let text =
            "{\"contracts\": [{\"id\": \"c1\", \"instruction\": \"two\nlines\tand a tab\"}]}";
        let v = json_object_with(text, None).expect("parsed");
        assert_eq!(v["contracts"][0]["instruction"], "two\nlines\tand a tab");
    }

    #[test]
    fn a_refusal_quotes_the_answer_at_the_error_not_its_start() {
        let filler = "x".repeat(400);
        let text = format!("{{\"a\": \"{filler}\", \"b\": [1, 2 3]}}");
        let err = json_object_with(&text, None).expect_err("refused");
        let msg = err.to_string();
        assert!(msg.contains("[1, 2 [ERROR HERE]3]"), "{msg}");
        assert!(!msg.contains("answer begins"), "{msg}");
    }

    #[test]
    fn the_window_marks_the_point_and_cuts_long_sides() {
        let text = format!("{}AB{}", "a".repeat(300), "b".repeat(300));
        let w = window(&text, 301);
        assert!(w.starts_with("..."));
        assert!(w.ends_with("..."));
        assert!(w.contains("A[ERROR HERE]B"));
    }
}
