//! Where a child's output goes as it arrives: held whole under a cap, kept
//! as a bounded head and tail with a marker, or handed on record by record.
//!
//! Each is bounded in memory. Which one a caller wants follows from what it
//! does with the output:
//!
//! - It parses the whole output (a JSON document): hold it, under a cap that
//!   fails loud ([`super::RunError::OutputTooLarge`]). A cut document would
//!   parse as a different document.
//! - It parses the output record by record (a long listing): stream it
//!   ([`super::run_streaming`]); nothing is held, so no cap applies.
//! - It shows the output to a person or a model: keep a head and a tail
//!   ([`HeadTail`]). The middle is dropped and the kept text says how much,
//!   so the cut is never silent, and the child is not ended for being chatty.

/// The head [`HeadTail::standard`] keeps. The start of a log names the
/// command, its version and its settings; 1 MiB is far more than that.
pub const KEEP_HEAD: usize = 1024 * 1024;

/// The tail [`HeadTail::standard`] keeps. The end of a log holds the failure
/// and the summary. 8 MiB is what the check runner reads from the end of each
/// stream when it looks for compiler errors, so a caller of either sees the
/// same window.
pub const KEEP_TAIL: usize = 8 * 1024 * 1024;

/// How much a person reads: `312.4 MiB`, `17 bytes`.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} bytes");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// The line that stands where the middle was.
pub fn omitted_marker(omitted: u64) -> String {
    format!(
        "[{} of output omitted from the middle]",
        human_bytes(omitted)
    )
}

/// The first `head` bytes and the last `tail` bytes of a stream.
///
/// The cut is by bytes, so it can fall inside a character; a caller that
/// shows the text converts it lossily.
#[derive(Debug)]
pub struct HeadTail {
    head_cap: usize,
    tail_cap: usize,
    head: Vec<u8>,
    /// Up to twice `tail_cap`: trimmed in one move when it passes that, so a
    /// push is amortized constant per byte and the buffer stays near
    /// `2 * tail_cap` plus one chunk.
    tail: Vec<u8>,
    total: u64,
}

impl HeadTail {
    pub fn new(head: usize, tail: usize) -> Self {
        Self {
            head_cap: head,
            tail_cap: tail,
            head: Vec::new(),
            tail: Vec::new(),
            total: 0,
        }
    }

    /// [`KEEP_HEAD`] and [`KEEP_TAIL`].
    pub fn standard() -> Self {
        Self::new(KEEP_HEAD, KEEP_TAIL)
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        let room = self.head_cap - self.head.len();
        let (to_head, rest) = bytes.split_at(room.min(bytes.len()));
        self.head.extend_from_slice(to_head);
        if rest.is_empty() {
            return;
        }
        self.tail.extend_from_slice(rest);
        if self.tail.len() > self.tail_cap.saturating_mul(2).max(self.tail_cap + 1) {
            let cut = self.tail.len() - self.tail_cap;
            self.tail.drain(..cut);
        }
    }

    /// Bytes read and not kept.
    pub fn omitted(&self) -> u64 {
        let kept_tail = self.tail.len().min(self.tail_cap) as u64;
        self.total - self.head.len() as u64 - kept_tail
    }

    /// The head, then (if anything was dropped) the marker on its own line,
    /// then the tail; and the count of bytes dropped.
    pub fn finish(mut self) -> (Vec<u8>, u64) {
        let omitted = self.omitted();
        if self.tail.len() > self.tail_cap {
            let cut = self.tail.len() - self.tail_cap;
            self.tail.drain(..cut);
        }
        if omitted == 0 {
            self.head.extend_from_slice(&self.tail);
            return (self.head, 0);
        }
        let mut out = self.head;
        out.extend_from_slice(format!("\n{}\n", omitted_marker(omitted)).as_bytes());
        out.extend_from_slice(&self.tail);
        (out, omitted)
    }
}

/// Splits a stream into records ended by one byte, holding only the record
/// in progress.
#[derive(Debug)]
pub(super) struct Records {
    delim: u8,
    pending: Vec<u8>,
    /// The longest record held; a record with no end before it is an error,
    /// not a reason to hold the rest of the stream.
    cap: usize,
}

impl Records {
    pub(super) fn new(delim: u8, cap: usize) -> Self {
        Self {
            delim,
            pending: Vec::new(),
            cap,
        }
    }

    /// Hand each complete record (without its delimiter) to `on_record`.
    /// True if the record in progress passed the cap.
    pub(super) fn push(&mut self, mut bytes: &[u8], on_record: &mut dyn FnMut(&[u8])) -> bool {
        while let Some(at) = bytes.iter().position(|b| *b == self.delim) {
            let (record, rest) = (&bytes[..at], &bytes[at + 1..]);
            if self.pending.is_empty() {
                on_record(record);
            } else {
                self.pending.extend_from_slice(record);
                on_record(&self.pending);
                self.pending.clear();
            }
            bytes = rest;
        }
        if self.pending.len().saturating_add(bytes.len()) > self.cap {
            return true;
        }
        self.pending.extend_from_slice(bytes);
        false
    }

    /// The last record, if the stream did not end with the delimiter.
    pub(super) fn finish(&mut self, on_record: &mut dyn FnMut(&[u8])) {
        if !self.pending.is_empty() {
            on_record(&self.pending);
            self.pending.clear();
        }
    }
}
