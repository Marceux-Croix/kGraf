//! Shared streaming primitives for adapters that speak server-sent events.
//!
//! Factored out of the invariants missing from the
//! first streaming adapter, so every later adapter gets them
//! for free instead of re-discovering them one at a time:
//!
//! - **Incremental read, never a whole-body buffer.** A caller that drops the
//!   returned future (a cancel) must drop the underlying HTTP body with it and
//!   abort the in-flight request; a `.text()` future already resolved cannot do
//!   that, and buffering the whole body also withholds every delta from the sink
//!   until the response ends, which looks indistinguishable from a hang.
//! - **CR/CRLF-normalized event boundaries**, so `\r\n` and a bare `\r` are both
//!   treated the same as `\n` rather than leaving it to `str::lines()` to apply
//!   its own, separate notion of a line ending on top of whatever normalization
//!   (or lack of it) happened upstream.
//! - **A bounded per-event buffer.** A connection that never terminates an event
//!   with a blank line must raise, not grow the buffer without limit.
//! - **A ledger row that never silently loses a known partial usage figure.** A
//!   stream that ends abnormally, an early return, a `?`, a panic unwind, or the
//!   caller dropping the whole future, must still settle through
//!   [`crate::CallToken::leave_unreconciled_with`] with whatever usage and
//!   request id are already known, never through `CallToken`'s own `Drop`, which
//!   always records zero usage because it cannot be handed a partial figure after
//!   the fact (every call is attributed to the ledger). A call its caller
//!   stopped on purpose ([`crate::CallToken::stop_on_purpose`]) settles
//!   through [`crate::CallToken::cancel_with_async`] instead, with the same
//!   usage and request id.

use std::cell::{Cell, RefCell};

use kgraf_core::TokenUsage;

use crate::accounting::CallToken;
use crate::error::ProviderError;

/// Hard cap on how many undecoded bytes [`SseDecoder`] may buffer without
/// completing a single event. Protects against a connection that never
/// terminates an event with a blank line (a bug, or a hostile peer): without
/// this, the buffer would grow without bound rather than raising.
pub const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// Incrementally decodes a server-sent-event byte stream into its JSON
/// payloads, one event at a time, so a caller does not have to buffer the
/// whole body before the first event is usable.
///
/// Hand-rolled because the protocol's framing is trivial and a dependency here
/// would be a dependency in the cost path.
///
/// Buffers raw, CR-normalized bytes, not `String`, and finds event boundaries
/// by scanning for the ASCII byte `\n`. That is safe against a multi-byte
/// UTF-8 character split across two network chunks: a UTF-8 continuation byte
/// is always in `0x80..=0xBF`, so it can never be mistaken for `\n` (`0x0A`),
/// and any prefix of the buffer that ends right after a `\n` is therefore
/// always a complete, validly-encoded run of whole characters, it cannot end
/// mid codepoint. Decoding is deferred until a full event is known so a
/// fragment that split a byte sequence never has to be decoded on its own.
pub struct SseDecoder {
    /// The adapter this decoder is decoding for, used only to name the error on
    /// the failure paths below.
    adapter: &'static str,
    /// Already normalized: every `\r\n` and bare `\r` this decoder has been
    /// fed has already become `\n`, so every later step only ever looks for
    /// `\n`, never `str::lines()`, which would apply its own, separate
    /// notion of a line ending on top of this one.
    buffer: Vec<u8>,
    /// Set when the most recently appended byte was a `\r`, which has already
    /// been normalized to a `\n` in `buffer`. If the very next byte turns out
    /// to be `\n` (possibly in the next chunk, if `\r\n` was split across a
    /// chunk boundary) it is the second half of the same line ending and
    /// must be swallowed rather than normalized into a second, blank line.
    pending_cr: bool,
    /// How many leading bytes of `buffer` are already confirmed not to start
    /// an event boundary. Re-scanning from here, not from zero, on every
    /// `push` is what keeps this O(n) over the life of a connection instead
    /// of O(n^2). Reset to `0` whenever `buffer`'s contents shift, i.e. after
    /// draining a completed event.
    scanned: usize,
}

impl SseDecoder {
    /// A fresh decoder. `adapter` names only the error messages this decoder
    /// raises; it carries no protocol-specific behavior.
    pub fn new(adapter: &'static str) -> Self {
        Self {
            adapter,
            buffer: Vec::new(),
            pending_cr: false,
            scanned: 0,
        }
    }

    /// Feed one chunk from the wire; returns every event it completed, in
    /// order. A `data:` payload that is not valid JSON raises rather than
    /// being dropped (fail loud): only a genuinely dataless event (a bare
    /// `event:` or `:` comment line, both real parts of the protocol) is
    /// silently skipped.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<serde_json::Value>, ProviderError> {
        self.normalize_into_buffer(chunk);
        self.drain_events()
    }

    /// Flush whatever remains once the connection has closed, for a server
    /// that ends its last event without a trailing blank line.
    pub fn finish(&mut self) -> Result<Vec<serde_json::Value>, ProviderError> {
        // A trailing bare `\r` already pushed its normalized `\n` when it was
        // seen; `pending_cr` only tracked whether a following `\n` should be
        // swallowed as the second half of the same pair, and no such byte is
        // coming now that the stream has ended.
        self.pending_cr = false;
        let mut events = self.drain_events()?;
        if !self.buffer.is_empty() {
            let block = std::mem::take(&mut self.buffer);
            if let Some(event) = self.parse_block(&block)? {
                events.push(event);
            }
        }
        self.scanned = 0;
        Ok(events)
    }

    /// Append `chunk` to `buffer`, translating every `\r\n` and bare `\r`
    /// into `\n` as it goes, one byte at a time, so nothing downstream of
    /// this ever has to reckon with more than one line-ending convention.
    fn normalize_into_buffer(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    // The `\n` half of a `\r\n` pair: the `\r` already pushed
                    // its normalized `\n` below, so this byte is swallowed.
                    continue;
                }
                // Not part of a `\r\n` pair, a bare `\r` line ending, already
                // normalized. `byte` itself still needs handling below.
            }
            match byte {
                // Push the `\n` immediately: a `\r\n` pair and a bare `\r`
                // both normalize to exactly one `\n`, and `pending_cr` exists
                // only to swallow a `\n` that turns out to complete this same
                // pair, not to defer the push itself.
                b'\r' => {
                    self.buffer.push(b'\n');
                    self.pending_cr = true;
                }
                other => self.buffer.push(other),
            }
        }
    }

    fn drain_events(&mut self) -> Result<Vec<serde_json::Value>, ProviderError> {
        let mut events = Vec::new();
        while let Some(end) = self.find_boundary() {
            let block: Vec<u8> = self.buffer.drain(..end).collect();
            self.scanned = 0;
            if let Some(event) = self.parse_block(&block)? {
                events.push(event);
            }
        }
        if self.buffer.len() > MAX_EVENT_BYTES {
            return Err(ProviderError::Decode {
                adapter: self.adapter,
                reason: format!(
                    "a single SSE event exceeded {MAX_EVENT_BYTES} bytes without a \
                     terminating blank line; treated as corrupt rather than buffered \
                     without limit"
                ),
            });
        }
        Ok(events)
    }

    /// The index just past the blank line (`\n\n`, after normalization)
    /// ending the first complete event in `buffer`, if one has fully arrived.
    /// Resumes scanning from `self.scanned` rather than from zero.
    fn find_boundary(&mut self) -> Option<usize> {
        let len = self.buffer.len();
        let mut i = self.scanned;
        while i + 1 < len {
            if self.buffer[i] == b'\n' && self.buffer[i + 1] == b'\n' {
                return Some(i + 2);
            }
            i += 1;
        }
        // Every pair checked and not matched is safe to skip next time,
        // except the very last byte: a future append could still pair it
        // with the next byte to arrive.
        self.scanned = len.saturating_sub(1);
        None
    }

    /// Extract the `data:` payload (SSE allows several `data:` lines per
    /// event, joined by `\n`) from one already-normalized event block and
    /// parse it as JSON.
    fn parse_block(&self, block: &[u8]) -> Result<Option<serde_json::Value>, ProviderError> {
        let text = std::str::from_utf8(block).map_err(|e| ProviderError::Decode {
            adapter: self.adapter,
            reason: format!("an SSE event was not valid UTF-8: {e}"),
        })?;
        let mut payload = String::new();
        for line in text.split('\n') {
            if let Some(rest) = line.strip_prefix("data:") {
                if !payload.is_empty() {
                    payload.push('\n');
                }
                payload.push_str(rest.trim());
            }
        }
        if payload.is_empty() || payload == "[DONE]" {
            // A genuinely dataless event: an `event:` line alone, a `:`
            // comment (both real SSE constructs used for keep-alives), or
            // the sentinel some providers send before closing. Nothing to
            // decode, so nothing to raise about.
            return Ok(None);
        }
        serde_json::from_str(&payload)
            .map(Some)
            .map_err(|e| ProviderError::Decode {
                adapter: self.adapter,
                reason: format!("an SSE `data:` payload was not valid JSON: {e}"),
            })
    }
}

/// Settles a streaming call's ledger row when `stream` exits by any path other
/// than its own explicit success: an early `return Err`, a `?`, a panic
/// unwind, or the caller dropping the whole returned future to cancel it.
///
/// `CallToken`'s own `Drop` already guarantees a row is never lost outright
/// (every call is attributed to the ledger), but it always writes zero usage and no request id, because a
/// `CallToken` cannot be handed either after the fact once the call is
/// abandoned. `StreamGuard` is the piece that closes that gap: an adapter
/// records the latest usage and request id it has seen as it reads the
/// stream, and unless [`StreamGuard::disarm`] is called on the single success
/// path, `Drop` settles the row through
/// [`CallToken::leave_unreconciled_with`] with whatever was last recorded:
/// `Some(usage)` if any usage was ever observed, `None` otherwise. So a known
/// partial cost is never silently zeroed and the row still carries its
/// request id for reconciliation. When the caller stopped the call on purpose
/// ([`CallToken::stop_on_purpose`]) before dropping it, the row settles
/// `Cancelled` through [`CallToken::cancel_with_async`] with the same figures.
pub struct StreamGuard<'a, 'ledger> {
    token: &'a CallToken<'ledger>,
    usage: RefCell<TokenUsage>,
    saw_usage: Cell<bool>,
    request_id: RefCell<Option<String>>,
    armed: Cell<bool>,
}

impl<'a, 'ledger> StreamGuard<'a, 'ledger> {
    /// A guard armed over `token`. Disarmed only by the adapter's own
    /// successful settlement.
    pub fn new(token: &'a CallToken<'ledger>) -> Self {
        Self {
            token,
            usage: RefCell::new(TokenUsage::default()),
            saw_usage: Cell::new(false),
            request_id: RefCell::new(None),
            armed: Cell::new(true),
        }
    }

    /// Record the latest usage known, so an abandoned call closes with the
    /// real partial figure rather than zero.
    pub fn record_usage(&self, usage: &TokenUsage) {
        *self.usage.borrow_mut() = usage.clone();
        self.saw_usage.set(true);
    }

    /// Record the provider's request id, once the protocol has sent one.
    pub fn record_request_id(&self, id: Option<&str>) {
        if let Some(id) = id {
            *self.request_id.borrow_mut() = Some(id.to_owned());
        }
    }

    /// The call reached its own explicit, successful ending; the guard must
    /// not also settle it.
    pub fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for StreamGuard<'_, '_> {
    fn drop(&mut self) {
        if !self.armed.get() {
            return;
        }
        // Best-effort, exactly like `CallToken`'s own `Drop`: this can run
        // during a panic unwind or because the enclosing future was simply
        // dropped, and `drop` cannot propagate a failure either way. A second
        // settlement attempt on a token this adapter already settled directly
        // is refused internally by `CallToken` and ignored here, matching its
        // own documented idempotence.
        //
        // Submitted without waiting for the commit: this drop usually runs on an
        // async worker thread (the adapter's future was dropped), which must not
        // block on the ledger. The write is queued before this returns, so a later
        // read through the same ledger sees it (`Ledger`'s read barrier).
        let usage = self.usage.borrow();
        let partial = if self.saw_usage.get() {
            Some(&*usage)
        } else {
            None
        };
        // A caller that stopped the call on purpose (`CallToken::stop_on_purpose`)
        // gets it cancelled, with the same partial usage; anything else that
        // ended the stream early leaves it unreconciled.
        let request_id = self.request_id.borrow();
        let _not_awaited = if self.token.stopped_on_purpose() {
            self.token.cancel_with_async(partial, request_id.as_deref())
        } else {
            self.token
                .leave_unreconciled_with_async(partial, request_id.as_deref())
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounting::{Ledger, Settlement};
    use kgraf_core::Protocol;

    /// A guard dropped armed settles the row it guards: cancelled, with the
    /// partial usage and request id, when the caller stopped the call on
    /// purpose; unreconciled, with the same figures, otherwise.
    #[test]
    fn an_armed_guard_cancels_a_stopped_call_and_leaves_any_other_unreconciled() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let partial = TokenUsage {
            uncached_input: 321,
            ..TokenUsage::default()
        };
        let mut ids = Vec::new();
        for stop in [true, false] {
            let token = ledger
                .begin("test", Protocol::Additive, "m", "a", "t")
                .expect("begin");
            {
                let guard = StreamGuard::new(&token);
                guard.record_request_id(Some("req-1"));
                guard.record_usage(&partial);
                if stop {
                    token.stop_on_purpose();
                }
            }
            ids.push(token.id().clone());
        }
        let read = |i: usize| ledger.call(&ids[i]).expect("read").expect("present");
        for (i, settlement) in [(0, Settlement::Cancelled), (1, Settlement::Unreconciled)] {
            let row = read(i);
            assert_eq!(row.settlement, settlement);
            assert_eq!(row.usage.uncached_input, 321);
            assert_eq!(row.request_id.as_deref(), Some("req-1"));
        }
    }

    #[test]
    fn an_event_split_across_chunk_boundaries_still_decodes() {
        let mut decoder = SseDecoder::new("test");
        let mut events = decoder.push(b"data: {\"a\":").expect("push");
        assert!(events.is_empty());
        events.extend(decoder.push(b"1}\n\n").expect("push"));
        assert_eq!(events, vec![serde_json::json!({"a": 1})]);
    }

    #[test]
    fn crlf_and_bare_cr_line_endings_both_normalize() {
        let mut decoder = SseDecoder::new("test");
        // `\r\n\r\n` and a bare `\r\r` both normalize to the same blank-line
        // boundary, so both events are complete without ever calling `finish`.
        let events = decoder
            .push(b"data: {\"a\":1}\r\n\r\ndata: {\"b\":2}\r\r")
            .expect("push");
        assert_eq!(
            events,
            vec![serde_json::json!({"a": 1}), serde_json::json!({"b": 2})]
        );
    }

    #[test]
    fn a_crlf_pair_split_across_two_chunks_is_not_double_counted() {
        let mut decoder = SseDecoder::new("test");
        let mut events = decoder.push(b"data: {\"a\":1}\r").expect("push");
        events.extend(decoder.push(b"\n\r\n").expect("push"));
        assert_eq!(events, vec![serde_json::json!({"a": 1})]);
    }

    #[test]
    fn the_done_sentinel_and_a_comment_line_decode_to_nothing() {
        let mut decoder = SseDecoder::new("test");
        let events = decoder
            .push(b": keep-alive\n\ndata: [DONE]\n\n")
            .expect("push");
        assert!(events.is_empty());
    }

    #[test]
    fn a_malformed_data_payload_raises_rather_than_being_dropped() {
        let mut decoder = SseDecoder::new("test");
        let err = decoder
            .push(b"data: {not json}\n\n")
            .expect_err("must raise");
        assert!(matches!(err, ProviderError::Decode { .. }));
    }

    #[test]
    fn an_event_that_never_terminates_is_bounded() {
        let mut decoder = SseDecoder::new("test");
        let oversized = vec![b'a'; MAX_EVENT_BYTES + 1];
        let err = decoder.push(&oversized).expect_err("must raise");
        assert!(matches!(err, ProviderError::Decode { .. }));
    }

    #[test]
    fn a_finished_stream_flushes_a_trailing_event_with_no_blank_line() {
        let mut decoder = SseDecoder::new("test");
        assert!(decoder.push(b"data: {\"a\":1}\n").expect("push").is_empty());
        let flushed = decoder.finish().expect("finish");
        assert_eq!(flushed, vec![serde_json::json!({"a": 1})]);
    }
}
