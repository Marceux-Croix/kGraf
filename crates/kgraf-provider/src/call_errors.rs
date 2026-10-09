//! What a model call's failure said, kept for the caller that asked the call again.
//!
//! A call that failed in a way that may have billed settles its ledger row
//! unreconciled, with no usage and no error text: the hash-chained row cannot
//! carry the server's answer. When the caller then asks again (a malformed
//! answer, a server that could not parse the model's own output), the failure
//! would be gone. A reader of the run's files could not tell a server fault
//! from a call that was dropped mid-flight.
//!
//! This log is the side channel for that evidence. It lives on the [`Ledger`]
//! because every orchestrator call of a run already shares one, so no call
//! signature changes. It is memory only, never part of the chain, and bounded;
//! the run's trace drains it and writes each entry as a record.
//!
//! [`Ledger`]: crate::Ledger

use std::collections::VecDeque;
use std::sync::Mutex;

use kgraf_core::CallId;

/// The most entries held between two drains. A run drains at every trace
/// write, so the bound is reached only by a caller that never drains.
const CAPACITY: usize = 256;

/// The most bytes of a server's error kept. A server can quote the whole
/// unparsed tool call, up to the output limit; the status and the parse error
/// come first, so the head is the useful part.
pub const ERROR_KEEP_BYTES: usize = 2048;

/// `error` cut to [`ERROR_KEEP_BYTES`] at a char boundary, with the cut marked.
pub fn bounded_error_text(error: &str) -> String {
    let mut cut = error.len().min(ERROR_KEEP_BYTES);
    while !error.is_char_boundary(cut) {
        cut -= 1;
    }
    let omitted = error.len() - cut;
    if omitted > 0 {
        format!("{} [{omitted} more bytes omitted]", &error[..cut])
    } else {
        error.to_owned()
    }
}

/// One failed call that its caller asked again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallError {
    /// The failed call's ledger id.
    pub call: CallId,
    /// The ledger agent that made it (`orchestrator:<run>...`).
    pub agent: String,
    /// The call kind (`handshake`, `reground`, ...).
    pub what: String,
    /// The HTTP status the server sent, when the failure was a provider answer.
    pub status: Option<u16>,
    /// The server could not parse the model's own output (a provider error
    /// kind, never read from text). With status 200 it said so in the stream.
    pub server_parse: bool,
    /// The failure, bounded by [`bounded_error_text`]. The caller scrubs it
    /// before it is written anywhere.
    pub error: String,
}

/// The entries taken by one drain.
#[derive(Debug, Default)]
pub struct Drained {
    pub errors: Vec<CallError>,
    /// Entries that were evicted for lack of room since the last drain.
    pub dropped: u64,
}

/// Memory-only, bounded log of [`CallError`]s.
#[derive(Default)]
pub struct CallErrorLog {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    entries: VecDeque<CallError>,
    dropped: u64,
}

impl CallErrorLog {
    pub fn push(&self, entry: CallError) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.entries.len() >= CAPACITY {
            g.entries.pop_front();
            g.dropped += 1;
        }
        g.entries.push_back(entry);
    }

    /// Take the entries whose agent starts with `agent_prefix`, in order. The
    /// others stay for the run they belong to.
    pub fn drain(&self, agent_prefix: &str) -> Drained {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.entries.is_empty() && g.dropped == 0 {
            return Drained::default();
        }
        let (mine, rest): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut g.entries)
            .into_iter()
            .partition(|e| e.agent.starts_with(agent_prefix));
        g.entries = rest;
        let dropped = std::mem::take(&mut g.dropped);
        Drained {
            errors: mine.into(),
            dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(agent: &str, n: u32) -> CallError {
        CallError {
            call: CallId::new(format!("c{n}")),
            agent: agent.into(),
            what: "t".into(),
            status: Some(500),
            server_parse: true,
            error: "e".into(),
        }
    }

    #[test]
    fn drain_takes_only_the_runs_own_entries() {
        let log = CallErrorLog::default();
        log.push(entry("orchestrator:a", 1));
        log.push(entry("orchestrator:b", 2));
        log.push(entry("orchestrator:a:interface_review", 3));
        let a = log.drain("orchestrator:a");
        assert_eq!(a.errors.len(), 2);
        assert_eq!(log.drain("orchestrator:b").errors.len(), 1);
        assert!(log.drain("orchestrator:").errors.is_empty());
    }

    #[test]
    fn overflow_evicts_the_oldest_and_counts_it() {
        let log = CallErrorLog::default();
        for n in 0..(CAPACITY as u32 + 3) {
            log.push(entry("o", n));
        }
        let d = log.drain("o");
        assert_eq!((d.errors.len(), d.dropped), (CAPACITY, 3));
        assert_eq!(d.errors[0].call, CallId::new("c3"));
    }

    #[test]
    fn a_long_error_is_cut_at_a_char_boundary_and_marked() {
        let text = "\u{e9}".repeat(5000);
        let cut = bounded_error_text(&text);
        assert!(cut.contains("more bytes omitted]"), "{cut}");
        assert!(cut.len() < ERROR_KEEP_BYTES + 40);
        assert_eq!(bounded_error_text("short"), "short");
    }
}
