//! Worker transcripts for the one text surface (0034) and for search: every
//! transcript under `<state dir>/runs/*/workers/*.jsonl`, read verbatim and kept
//! current by one background thread.
//!
//! A transcript is `kgraf-session`'s JSON lines of `TranscriptEntry<WorkerEvent>`
//! (codex-rs `rollout/`, as the runtime writes it). [`turns`] groups its events into
//! readable turns without dropping or rewriting any text: consecutive text deltas are
//! concatenated (that concatenation is the text the model sent), tool calls keep
//! their full arguments and results, approvals and notices are kept, and a line that
//! does not parse is shown as unreadable with its reason, never skipped.
//! Only `ModelCall` events are left out: they carry a ledger id and no content.
//!
//! **Why a thread.** Transcripts grow while workers run, and the UI repaints only on
//! change (0054). The thread stats each file every [`POLL`], reads only the bytes
//! appended since its last read, and sends a new corpus only when a file grew,
//! appeared or went; an idle project costs one directory walk per poll.
//!
//! **Bounded memory.** The corpus holds at most [`MAX_CORPUS_BYTES`] of transcript;
//! past that, the oldest transcripts are left out and counted
//! ([`Corpus::left_out`]), so a search that cannot reach them says so. A left-out
//! file is remembered by its length and modification time and not read again
//! until one of them changes. The text is held once: each transcript is an `Arc`
//! shared between the loader and the UI, and search reads it in place.
//!
//! **Time.** Transcript events carry no timestamps; each transcript's file
//! modification time stands in as the time of its turns, for `before:` and
//! `after:`. The text surface says so.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use kgraf_runtime::WorkerEvent;
use kgraf_session::transcript::TranscriptEntry;

/// How often the transcript files are checked.
pub const POLL: Duration = Duration::from_millis(1_000);
/// Most transcript text held for the text surface and search.
pub const MAX_CORPUS_BYTES: u64 = 64 * 1024 * 1024;

/// One readable turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Turn {
    /// Text the model sent, verbatim.
    Assistant(String),
    /// Reasoning the model sent, verbatim.
    Reasoning(String),
    ToolCall {
        call_id: String,
        tool: String,
        /// The arguments as pretty JSON, whole.
        input: String,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    /// A message injected into this session's transcript (decision 0067,
    /// point 3), with who it came from.
    Message { source: String, text: String },
    /// An approval decision, a loop notice, usage, a stop, a cancel, or a
    /// round abandoned by an interrupt-mode steer.
    Event(String),
    /// A line that did not parse, with its reason.
    Unreadable { line: usize, reason: String },
}

impl Turn {
    /// The turn's searchable text, in place, as up to two parts (a tool call's
    /// name and arguments); search reads these without copying.
    pub fn parts(&self) -> [&str; 2] {
        match self {
            Turn::Assistant(t) | Turn::Reasoning(t) | Turn::Event(t) => [t, ""],
            Turn::ToolCall { tool, input, .. } => [tool, input],
            Turn::ToolResult { output, .. } => [output, ""],
            Turn::Message { source, text } => [source, text],
            Turn::Unreadable { reason, .. } => [reason, ""],
        }
    }

    /// Whether this turn closes a model round: a stop, a cancel, or an
    /// abandoned round. A settled message replaces text turns only after the
    /// last of these.
    fn ends_a_round(&self) -> bool {
        matches!(self, Turn::Event(e)
            if e.starts_with(STOP) || e.starts_with(CANCELLED) || e.starts_with(ABANDONED))
    }

    /// The turn's text as one string (for display).
    pub fn text(&self) -> String {
        match self {
            Turn::Assistant(t) | Turn::Reasoning(t) | Turn::Event(t) => t.clone(),
            Turn::ToolCall { tool, input, .. } => format!("{tool} {input}"),
            Turn::ToolResult { output, .. } => output.clone(),
            Turn::Message { source, text } => format!("{source}: {text}"),
            Turn::Unreadable { reason, .. } => reason.clone(),
        }
    }
}

/// The event-turn prefixes that close a round (see [`Turn::ends_a_round`]).
const STOP: &str = "stop: ";
const CANCELLED: &str = "cancelled during ";
const ABANDONED: &str = "round abandoned during ";

fn event_turn(event: WorkerEvent, turns: &mut Vec<Turn>) {
    match event {
        WorkerEvent::TextDelta { text } => match turns.last_mut() {
            Some(Turn::Assistant(t)) => t.push_str(&text),
            _ => turns.push(Turn::Assistant(text)),
        },
        WorkerEvent::ReasoningDelta { text } => match turns.last_mut() {
            Some(Turn::Reasoning(t)) => t.push_str(&text),
            _ => turns.push(Turn::Reasoning(text)),
        },
        // The round's settled, whole-scrubbed text and reasoning (decision
        // 0071), emitted once right before `Stop`. Replaces every text and
        // reasoning turn the round's deltas built, back to the round's start
        // a `Usage` event can sit between the deltas and
        // this record, so removing only trailing turns left the deltas, and a
        // secret split across two of them, on screen and in search. Deltas
        // were scrubbed one at a time, so a split secret is whole only in
        // the joined turn; this record was scrubbed as one string.
        WorkerEvent::AssistantMessage {
            text, reasoning, ..
        } => {
            let start = turns
                .iter()
                .rposition(Turn::ends_a_round)
                .map_or(0, |i| i + 1);
            let mut i = start;
            while i < turns.len() {
                if matches!(turns[i], Turn::Assistant(_) | Turn::Reasoning(_)) {
                    turns.remove(i);
                } else {
                    i += 1;
                }
            }
            if !reasoning.is_empty() {
                turns.push(Turn::Reasoning(reasoning));
            }
            if !text.is_empty() {
                turns.push(Turn::Assistant(text));
            }
        }
        WorkerEvent::ToolCallStarted {
            call_id,
            tool,
            input, .. } => turns.push(Turn::ToolCall {
            call_id,
            tool,
            input: serde_json::to_string_pretty(&input).unwrap_or_else(|_| input.to_string()),
        }),
        WorkerEvent::ToolCallFinished {
            call_id,
            output,
            is_error,
        } => turns.push(Turn::ToolResult {
            call_id,
            output,
            is_error,
        }),
        WorkerEvent::Usage { call, usage } => turns.push(Turn::Event(format!(
            "usage for call {call}: {} input, {} cache read, {} cache write, {} output, {} reasoning",
            usage.uncached_input,
            usage.cache_read,
            usage.cache_write,
            usage.text_output,
            usage.reasoning_output
        ))),
        WorkerEvent::Stop { reason, .. } => turns.push(Turn::Event(format!("{STOP}{reason:?}"))),
        WorkerEvent::Cancelled { phase } => {
            turns.push(Turn::Event(format!("{CANCELLED}{phase:?}")))
        }
        WorkerEvent::Approval { record } => turns.push(Turn::Event(format!(
            "approval of {} ({}) by {}: {}",
            record.tool,
            record.call_id,
            record.decided_by,
            serde_json::to_string(&record.decision).unwrap_or_default()
        ))),
        WorkerEvent::Context {
            prefix_digest,
            slices,
            ..
        } => turns.push(Turn::Event(format!(
            "context: prefix {prefix_digest}, {} slice(s): {}",
            slices.len(),
            slices
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        WorkerEvent::Notice { kind, text } => {
            turns.push(Turn::Event(format!("notice ({kind:?}): {text}")))
        }
        // A ledger id with no content.
        WorkerEvent::ModelCall { .. } => {}
        WorkerEvent::Injected { source, text, .. } => turns.push(Turn::Message {
            source: source.to_string(),
            text,
        }),
        WorkerEvent::RoundAbandoned { phase, reason } => turns.push(Turn::Event(format!(
            "{ABANDONED}{phase:?}: {reason}"
        ))),
        // Kinds and counts only: says that text was
        // redacted and what kind, never any of it.
        WorkerEvent::Redacted { counts } => turns.push(Turn::Event(format!(
            "redacted before writing: {}",
            counts
                .iter()
                .map(|(kind, n)| format!("{n} {kind}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        WorkerEvent::Waiting { call_id, reason } => {
            turns.push(Turn::Event(format!("call {call_id} waiting: {reason}")))
        }
        // Decision 0095: the runtime compacted this worker; the turns above
        // are the full earlier history, kept as written.
        WorkerEvent::Compacted {
            notice, summary, ..
        } => turns.push(Turn::Event(
            kgraf_runtime::render_notice(&notice, &summary),
        )),
        WorkerEvent::Resumed { call_id } => {
            turns.push(Turn::Event(format!("call {call_id} resumed")))
        }
        // Decision 0096: a round checkpoint, as the loop measured it.
        WorkerEvent::Checkpoint { checkpoint, .. } => {
            turns.push(Turn::Event(checkpoint.render()))
        }
    }
}

/// Group one transcript's lines into turns, appending to `turns`. `first_line` is
/// the 1-based number of the first line, for the unreadable-line report.
pub fn turns(text: &str, first_line: usize, turns: &mut Vec<Turn>) {
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<TranscriptEntry<WorkerEvent>>(line) {
            Ok(entry) => event_turn(entry.event, turns),
            Err(e) => turns.push(Turn::Unreadable {
                line: first_line + i,
                reason: format!("line {} is not a transcript event: {e}", first_line + i),
            }),
        }
    }
}

/// One worker's transcript, as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub run: String,
    pub worker: String,
    pub path: PathBuf,
    pub turns: Vec<Turn>,
    /// Modification time, ms since the epoch (the turns' stand-in time).
    pub modified_ms: Option<i64>,
    /// Bytes read so far.
    read: u64,
    /// Lines read so far.
    lines: usize,
    /// A trailing partial line waiting for its end, as bytes: a UTF-8 character
    /// split across two appends is decoded only once it is whole.
    partial: Vec<u8>,
}

impl Transcript {
    fn new(run: &str, worker: &str, path: PathBuf) -> Self {
        Self {
            run: run.into(),
            worker: worker.into(),
            path,
            turns: Vec::new(),
            modified_ms: None,
            read: 0,
            lines: 0,
            partial: Vec::new(),
        }
    }

    /// A transcript already read, for tests of what reads transcripts.
    #[cfg(test)]
    pub(crate) fn for_test(run: &str, worker: &str, path: PathBuf, turns: Vec<Turn>) -> Self {
        Self {
            turns,
            ..Self::new(run, worker, path)
        }
    }

    /// Bytes of transcript this holds.
    pub fn bytes(&self) -> u64 {
        self.read
    }
}

/// A file's identity for the left-out set: a file whose length and modification
/// time are unchanged is not read again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub modified_ms: Option<i64>,
}

/// Every transcript within the memory bound.
///
/// Each transcript is its own `Arc`: the loader shares an unchanged transcript
/// with the UI's copy, so a change in one transcript copies that one alone, never
/// the corpus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Corpus {
    /// By (run, worker).
    pub transcripts: BTreeMap<(String, String), Arc<Transcript>>,
    /// Transcripts left out by the memory bound, with the file stamp they were
    /// left out at. Not read again until the stamp changes.
    pub left_out: BTreeMap<(String, String), FileStamp>,
    /// Why the runs directory could not be read, if it could not.
    pub error: Option<String>,
    /// Transcript bytes read from disk so far (for tests and the report).
    pub bytes_read: u64,
}

impl Corpus {
    pub fn get(&self, run: &str, worker: &str) -> Option<&Transcript> {
        self.transcripts
            .get(&(run.to_owned(), worker.to_owned()))
            .map(|t| t.as_ref())
    }

    /// Transcripts left out by the memory bound.
    pub fn left_out_count(&self) -> usize {
        self.left_out.len()
    }
}

/// The worker a transcript belongs to, and its contract, from the worker id the
/// orchestrator gives it: `<run>-<contract>-r<failures>-a<attempt>`.
pub fn contract_of(run: &str, worker: &str) -> Option<String> {
    let rest = worker.strip_prefix(run)?.strip_prefix('-')?;
    let (rest, attempt) = rest.rsplit_once("-a")?;
    let (contract, failures) = rest.rsplit_once("-r")?;
    (attempt.chars().all(|c| c.is_ascii_digit())
        && failures.chars().all(|c| c.is_ascii_digit())
        && !contract.is_empty())
    .then(|| contract.to_owned())
}

fn stamp_of(path: &Path) -> std::io::Result<FileStamp> {
    let meta = std::fs::metadata(path)?;
    Ok(FileStamp {
        len: meta.len(),
        modified_ms: meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64),
    })
}

/// Reads what is new in `t`'s file, which has `stamp`. Returns the bytes read.
fn refresh(t: &mut Transcript, stamp: FileStamp) -> std::io::Result<u64> {
    t.modified_ms = stamp.modified_ms;
    let len = stamp.len;
    if len < t.read {
        // Truncated (a torn tail repaired): read it again from the start.
        t.read = 0;
        t.lines = 0;
        t.partial.clear();
        t.turns.clear();
    }
    let mut f = std::fs::File::open(&t.path)?;
    f.seek(SeekFrom::Start(t.read))?;
    let mut bytes = std::mem::take(&mut t.partial);
    let before = bytes.len();
    f.take(len - t.read).read_to_end(&mut bytes)?;
    let got = (bytes.len() - before) as u64;
    t.read += got;
    // Keep a partial last line (an append in progress) as bytes for the next read.
    match bytes.iter().rposition(|&b| b == b'\n') {
        Some(i) => {
            t.partial = bytes.split_off(i + 1);
            let complete = String::from_utf8_lossy(&bytes);
            let first = t.lines + 1;
            t.lines += complete.lines().count();
            turns(&complete, first, &mut t.turns);
        }
        None => t.partial = bytes,
    }
    Ok(got)
}

/// One pass over `runs_dir`: new files are added, grown files read, gone files
/// dropped, left-out files skipped while unchanged. Returns whether the corpus
/// changed.
pub fn scan(runs_dir: &Path, corpus: &mut Corpus) -> bool {
    let mut changed = false;
    let mut seen = std::collections::BTreeSet::new();
    let runs = match std::fs::read_dir(runs_dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let had = !corpus.transcripts.is_empty()
                || corpus.error.is_some()
                || !corpus.left_out.is_empty();
            let read = corpus.bytes_read;
            *corpus = Corpus::default();
            corpus.bytes_read = read;
            return had;
        }
        Err(e) => {
            let msg = format!("{}: {e}", runs_dir.display());
            let changed = corpus.error.as_ref() != Some(&msg);
            corpus.error = Some(msg);
            return changed;
        }
    };
    if corpus.error.take().is_some() {
        changed = true;
    }
    // Every transcript file with its stamp, newest first: the bound keeps the
    // newest, so a file that would not fit is left out before it is read.
    let mut found: Vec<((String, String), PathBuf, std::io::Result<FileStamp>)> = Vec::new();
    for run in runs.flatten() {
        let run_name = run.file_name().to_string_lossy().into_owned();
        let Ok(workers) = std::fs::read_dir(run.path().join("workers")) else {
            continue;
        };
        for w in workers.flatten() {
            let path = w.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(worker) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            let stamp = stamp_of(&path);
            found.push(((run_name.clone(), worker), path, stamp));
        }
    }
    found.sort_by_key(|(_, _, s)| std::cmp::Reverse(s.as_ref().ok().and_then(|s| s.modified_ms)));
    // Held bytes of the transcripts still on disk (a gone one frees its room now).
    let present: std::collections::BTreeSet<&(String, String)> =
        found.iter().map(|(k, _, _)| k).collect();
    let mut held: u64 = corpus
        .transcripts
        .iter()
        .filter(|(k, _)| present.contains(k))
        .map(|(_, t)| t.read)
        .sum();
    drop(present);
    for (key, path, stamp) in found {
        seen.insert(key.clone());
        let (run_name, worker) = (key.0.clone(), key.1.clone());
        let stamp = match stamp {
            Ok(s) => s,
            Err(e) => {
                let reason = format!("{} could not be read: {e}", path.display());
                let t = corpus
                    .transcripts
                    .entry(key)
                    .or_insert_with(|| Arc::new(Transcript::new(&run_name, &worker, path.clone())));
                if !matches!(t.turns.last(), Some(Turn::Unreadable { reason: r, .. }) if *r == reason)
                {
                    Arc::make_mut(t)
                        .turns
                        .push(Turn::Unreadable { line: 0, reason });
                    changed = true;
                }
                continue;
            }
        };
        // Left out, unchanged, and still no room: not read again. When room has
        // freed (a held transcript went away), it comes back.
        if corpus.left_out.get(&key) == Some(&stamp) && held + stamp.len > MAX_CORPUS_BYTES {
            continue;
        }
        // A file not held yet that would take the corpus past the bound is left
        // out unread (older files come later in this order, so it is not
        // displacing anything newer).
        if !corpus.transcripts.contains_key(&key) && held + stamp.len > MAX_CORPUS_BYTES {
            corpus.left_out.insert(key, stamp);
            changed = true;
            continue;
        }
        // A left-out file that changed is the newest now: it comes back, and the
        // bound leaves out older ones instead.
        corpus.left_out.remove(&key);
        let t = corpus
            .transcripts
            .entry(key)
            .or_insert_with(|| Arc::new(Transcript::new(&run_name, &worker, path.clone())));
        if stamp.len == t.read && stamp.modified_ms == t.modified_ms {
            continue;
        }
        // Only this transcript is copied, and only when the UI still holds the
        // old one.
        let t = Arc::make_mut(t);
        let before = t.read;
        match refresh(t, stamp) {
            Ok(got) => {
                corpus.bytes_read += got;
                changed = true;
            }
            Err(e) => {
                let reason = format!("{} could not be read: {e}", path.display());
                t.turns.push(Turn::Unreadable { line: 0, reason });
                changed = true;
            }
        }
        held = held.saturating_sub(before) + t.read;
    }
    let before = (corpus.transcripts.len(), corpus.left_out.len());
    corpus.transcripts.retain(|k, _| seen.contains(k));
    corpus.left_out.retain(|k, _| seen.contains(k));
    changed |= (corpus.transcripts.len(), corpus.left_out.len()) != before;
    changed |= enforce_bound(corpus);
    changed
}

/// Leave out the oldest transcripts past [`MAX_CORPUS_BYTES`], remembering each
/// one's file stamp so it is not read again while unchanged.
fn enforce_bound(corpus: &mut Corpus) -> bool {
    let total: u64 = corpus.transcripts.values().map(|t| t.read).sum();
    if total <= MAX_CORPUS_BYTES {
        return false;
    }
    let mut by_age: Vec<((String, String), Option<i64>, u64)> = corpus
        .transcripts
        .iter()
        .map(|(k, t)| (k.clone(), t.modified_ms, t.read))
        .collect();
    by_age.sort_by_key(|(_, m, _)| *m);
    let mut over = total - MAX_CORPUS_BYTES;
    for (k, modified_ms, bytes) in by_age {
        if over == 0 {
            break;
        }
        corpus.transcripts.remove(&k);
        corpus.left_out.insert(
            k,
            FileStamp {
                len: bytes,
                modified_ms,
            },
        );
        over = over.saturating_sub(bytes);
    }
    true
}

/// The loader's handle. Dropping it stops and joins the thread.
pub struct TranscriptWatch {
    rx: Receiver<Arc<Corpus>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for TranscriptWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptWatch").finish_non_exhaustive()
    }
}

impl Drop for TranscriptWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

impl TranscriptWatch {
    /// Watch `runs_dir` every `poll`; `wake` runs after each new corpus.
    pub fn spawn(
        runs_dir: PathBuf,
        poll: Duration,
        wake: impl Fn() + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("transcripts".into())
            .spawn(move || watch(&runs_dir, poll, &flag, &tx, &wake))?;
        Ok(Self {
            rx,
            stop,
            thread: Some(thread),
        })
    }

    /// The newest corpus, if a new one arrived.
    pub fn latest(&self) -> Option<Arc<Corpus>> {
        self.rx.try_iter().last()
    }
}

fn watch(
    runs_dir: &Path,
    poll: Duration,
    stop: &AtomicBool,
    tx: &Sender<Arc<Corpus>>,
    wake: &dyn Fn(),
) {
    let mut corpus = Corpus::default();
    let mut first = true;
    while !stop.load(Ordering::SeqCst) {
        if scan(runs_dir, &mut corpus) || first {
            first = false;
            // A clone of the map of `Arc`s: no transcript text is copied.
            if tx.send(Arc::new(corpus.clone())).is_err() {
                return;
            }
            wake();
        }
        if !stop.load(Ordering::SeqCst) {
            std::thread::park_timeout(poll);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn line(seq: u64, event: serde_json::Value) -> String {
        format!("{}\n", serde_json::json!({"seq": seq, "event": event}))
    }

    #[test]
    fn events_become_turns_verbatim_and_a_bad_line_is_shown_not_skipped() {
        let text = [
            line(0, serde_json::json!({"type": "text_delta", "text": "I will "})),
            line(1, serde_json::json!({"type": "text_delta", "text": "write a.rs"})),
            line(
                2,
                serde_json::json!({"type": "tool_call_started", "call_id": "t1", "tool": "write_file",
                    "input": {"path": "src/a.rs", "content": "fn a() {}"}}),
            ),
            line(
                3,
                serde_json::json!({"type": "tool_call_finished", "call_id": "t1", "output": "wrote 9 bytes", "is_error": false}),
            ),
            "{broken\n".to_owned(),
            line(4, serde_json::json!({"type": "text_delta", "text": "done"})),
        ]
        .concat();
        let mut out = Vec::new();
        turns(&text, 1, &mut out);
        assert_eq!(out[0], Turn::Assistant("I will write a.rs".into()));
        assert!(matches!(&out[1], Turn::ToolCall { tool, input, .. }
            if tool == "write_file" && input.contains("fn a() {}")));
        assert_eq!(
            out[2],
            Turn::ToolResult {
                call_id: "t1".into(),
                output: "wrote 9 bytes".into(),
                is_error: false
            }
        );
        assert!(matches!(&out[3], Turn::Unreadable { line: 5, .. }));
        assert_eq!(out[4], Turn::Assistant("done".into()));
    }

    /// The exact order: deltas split
    /// a `ghp_` token, a `Usage` sits between them and the settled message,
    /// and the settled message carries the redacted text. The shell and its
    /// search corpus must show neither the joined token nor a second copy of
    /// the round's text.
    #[test]
    fn a_settled_message_replaces_its_deltas_across_a_usage_event() {
        let token = format!("ghp_{}", "Zq8vN2kLm4Rt6Yw1Xc3Bp5Hd7Jf9Gs0Ae2Ui");
        let (a, b) = token.split_at(12);
        let marker = "[redacted:github-pat; re-read the source for exact text]";
        let usage = serde_json::json!({"type": "usage", "call": "c2", "usage": {
            "uncached_input": 1, "cache_read": 0, "cache_write": 0,
            "text_output": 1, "reasoning_output": 0}});
        let text = [
            // An earlier, settled round: kept once.
            line(0, serde_json::json!({"type": "model_call", "call": "c1"})),
            line(1, serde_json::json!({"type": "text_delta", "text": "first"})),
            line(2, serde_json::json!({"type": "assistant_message", "text": "first", "reasoning": ""})),
            line(3, serde_json::json!({"type": "stop", "reason": "tool_use"})),
            line(4, serde_json::json!({"type": "tool_call_started", "call_id": "t1", "tool": "read", "input": {}})),
            line(5, serde_json::json!({"type": "tool_call_finished", "call_id": "t1", "output": "ok", "is_error": false})),
            // The round under test.
            line(6, serde_json::json!({"type": "model_call", "call": "c2"})),
            line(7, serde_json::json!({"type": "reasoning_delta", "text": "think"})),
            line(8, serde_json::json!({"type": "text_delta", "text": "key "})),
            line(9, serde_json::json!({"type": "text_delta", "text": a})),
            line(10, serde_json::json!({"type": "text_delta", "text": b})),
            line(11, usage),
            line(12, serde_json::json!({"type": "assistant_message",
                "text": format!("key {marker}"), "reasoning": "think"})),
            line(13, serde_json::json!({"type": "stop", "reason": "end_turn"})),
        ]
        .concat();
        let mut out = Vec::new();
        turns(&text, 1, &mut out);
        let all: String = out
            .iter()
            .flat_map(|t| t.parts())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!all.contains(&token), "the joined token is shown: {out:#?}");
        assert!(!all.contains(a), "a half of the token is shown: {out:#?}");
        let assistant: Vec<&Turn> = out
            .iter()
            .filter(|t| matches!(t, Turn::Assistant(_)))
            .collect();
        assert_eq!(
            assistant,
            [
                &Turn::Assistant("first".into()),
                &Turn::Assistant(format!("key {marker}"))
            ],
            "each round's text once: {out:#?}"
        );
        let reasoning = out
            .iter()
            .filter(|t| matches!(t, Turn::Reasoning(_)))
            .count();
        assert_eq!(reasoning, 1, "{out:#?}");
    }

    /// A redaction is visible in the text surface and search as kinds
    /// and counts.
    #[test]
    fn a_redaction_record_is_shown_as_kinds_and_counts() {
        let text = line(
            0,
            serde_json::json!({"type": "redacted", "counts": {"github-pat": 2, "provider-api-key": 1}}),
        );
        let mut out = Vec::new();
        turns(&text, 1, &mut out);
        assert_eq!(
            out,
            [Turn::Event(
                "redacted before writing: 2 github-pat, 1 provider-api-key".into()
            )]
        );
    }

    #[test]
    fn a_worker_id_names_its_contract() {
        assert_eq!(contract_of("r1", "r1-a-r0-a0").as_deref(), Some("a"));
        assert_eq!(
            contract_of("r1", "r1-parse-lexer-r2-a1").as_deref(),
            Some("parse-lexer")
        );
        assert_eq!(contract_of("r1", "r2-a-r0-a0"), None);
        assert_eq!(contract_of("r1", "r1-a"), None);
    }

    #[test]
    fn a_growing_transcript_is_read_incrementally_and_a_partial_line_waits() {
        let dir = tempfile::tempdir().expect("dir");
        let workers = dir.path().join("r1").join("workers");
        std::fs::create_dir_all(&workers).expect("mkdir");
        let path = workers.join("r1-a-r0-a0.jsonl");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(line(0, serde_json::json!({"type": "text_delta", "text": "one"})).as_bytes())
            .expect("write");
        let mut corpus = Corpus::default();
        assert!(scan(dir.path(), &mut corpus));
        assert_eq!(
            corpus.get("r1", "r1-a-r0-a0").expect("t").turns,
            [Turn::Assistant("one".into())]
        );
        assert!(!scan(dir.path(), &mut corpus), "nothing new, no change");
        // Half a line: held until its end arrives.
        let next = line(1, serde_json::json!({"type": "text_delta", "text": " two"}));
        let (a, b) = next.split_at(10);
        f.write_all(a.as_bytes()).expect("write");
        f.flush().expect("flush");
        scan(dir.path(), &mut corpus);
        assert_eq!(corpus.get("r1", "r1-a-r0-a0").expect("t").turns.len(), 1);
        f.write_all(b.as_bytes()).expect("write");
        f.flush().expect("flush");
        assert!(scan(dir.path(), &mut corpus));
        assert_eq!(
            corpus.get("r1", "r1-a-r0-a0").expect("t").turns,
            [Turn::Assistant("one two".into())]
        );
        std::fs::remove_file(&path).expect("rm");
        assert!(scan(dir.path(), &mut corpus));
        assert!(
            corpus.transcripts.is_empty(),
            "a removed file leaves the corpus"
        );
    }

    /// Past the bound, left-out transcripts are remembered by file stamp and
    /// never read again while unchanged. Two 40 MB transcripts: one is kept, one
    /// is left out; repeated scans report no change and read no byte.
    #[test]
    fn a_left_out_transcript_is_not_read_again_while_unchanged() {
        let dir = tempfile::tempdir().expect("dir");
        let workers = dir.path().join("r1").join("workers");
        std::fs::create_dir_all(&workers).expect("mkdir");
        let one = line(
            0,
            serde_json::json!({"type": "tool_call_finished", "call_id": "t",
                "output": "x".repeat(1_000_000), "is_error": false}),
        );
        for name in ["r1-a-r0-a0", "r1-b-r0-a0"] {
            let mut f = std::fs::File::create(workers.join(format!("{name}.jsonl"))).expect("f");
            for _ in 0..40 {
                f.write_all(one.as_bytes()).expect("write");
            }
            f.sync_all().expect("sync");
            // Distinct modification times, so "oldest" is defined.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut corpus = Corpus::default();
        assert!(scan(dir.path(), &mut corpus));
        assert_eq!(corpus.transcripts.len(), 1, "one fits under 64 MB");
        assert_eq!(corpus.left_out_count(), 1);
        let read = corpus.bytes_read;
        assert!(
            read < 41_000_000,
            "the one that did not fit was left out unread ({read} bytes read)"
        );
        assert!(
            corpus.get("r1", "r1-b-r0-a0").is_some(),
            "the newer transcript is the one kept"
        );
        for _ in 0..5 {
            assert!(!scan(dir.path(), &mut corpus), "no change reported");
        }
        assert_eq!(corpus.bytes_read, read, "no byte read again");
        assert_eq!(corpus.left_out_count(), 1);

        // The kept transcript goes; the left-out one comes back into the room.
        std::fs::remove_file(workers.join("r1-b-r0-a0.jsonl")).expect("rm");
        assert!(scan(dir.path(), &mut corpus));
        assert_eq!(corpus.left_out_count(), 0);
        assert!(corpus.get("r1", "r1-a-r0-a0").is_some(), "brought back");
    }

    /// A UTF-8 character split across two appends is decoded whole.
    #[test]
    fn a_character_split_across_appends_is_not_corrupted() {
        let dir = tempfile::tempdir().expect("dir");
        let workers = dir.path().join("r1").join("workers");
        std::fs::create_dir_all(&workers).expect("mkdir");
        let path = workers.join("r1-a-r0-a0.jsonl");
        let text = line(
            0,
            serde_json::json!({"type": "text_delta", "text": "caf\u{e9} \u{2713}"}),
        );
        let bytes = text.as_bytes();
        let cut = text.find('\u{e9}').expect("e acute") + 1; // inside the two bytes
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(&bytes[..cut]).expect("write");
        f.flush().expect("flush");
        let mut corpus = Corpus::default();
        scan(dir.path(), &mut corpus);
        f.write_all(&bytes[cut..]).expect("write");
        f.flush().expect("flush");
        scan(dir.path(), &mut corpus);
        assert_eq!(
            corpus.get("r1", "r1-a-r0-a0").expect("t").turns,
            [Turn::Assistant("caf\u{e9} \u{2713}".into())]
        );
    }

    /// Each transcript is its own `Arc`: a change to one leaves the others
    /// shared, not copied.
    #[test]
    fn a_change_copies_only_the_changed_transcript() {
        let dir = tempfile::tempdir().expect("dir");
        let workers = dir.path().join("r1").join("workers");
        std::fs::create_dir_all(&workers).expect("mkdir");
        for name in ["r1-a-r0-a0", "r1-b-r0-a0"] {
            std::fs::write(
                workers.join(format!("{name}.jsonl")),
                line(0, serde_json::json!({"type": "text_delta", "text": "hi"})),
            )
            .expect("write");
        }
        let mut corpus = Corpus::default();
        scan(dir.path(), &mut corpus);
        let ui_copy = corpus.clone();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(workers.join("r1-a-r0-a0.jsonl"))
            .expect("open");
        f.write_all(
            line(
                1,
                serde_json::json!({"type": "text_delta", "text": " there"}),
            )
            .as_bytes(),
        )
        .expect("write");
        f.flush().expect("flush");
        assert!(scan(dir.path(), &mut corpus));
        let key = |w: &str| ("r1".to_owned(), w.to_owned());
        assert!(Arc::ptr_eq(
            &corpus.transcripts[&key("r1-b-r0-a0")],
            &ui_copy.transcripts[&key("r1-b-r0-a0")]
        ));
        assert!(!Arc::ptr_eq(
            &corpus.transcripts[&key("r1-a-r0-a0")],
            &ui_copy.transcripts[&key("r1-a-r0-a0")]
        ));
    }
}
