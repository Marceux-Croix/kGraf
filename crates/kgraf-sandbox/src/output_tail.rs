//! The last bytes a sandboxed command wrote to a pipe, read on a thread of
//! its own, with a bounded wait at the end.
//!
//! A failed command must say why: its exit code and what it printed. This
//! module keeps the tail of each stream (memory bounded by `keep`, however
//! much the command prints, and the pipe is drained to its end so the
//! command never blocks on a full pipe) and gives it back even when the end
//! of the pipe never comes.
//!
//! The end of a pipe comes when every copy of its write end is closed. A
//! sandbox ends the whole process tree first (the job on Windows, the process
//! group on Unix), so the end normally follows at once. A process outside the
//! tree can still hold a copy it never writes to: a program another thread
//! of this process started at the same moment, which inherited the pipe
//! before it was marked close-on-exec (macOS has no `pipe2`, so std marks a
//! new pipe in a second call) or, on Windows, while the sandbox's handles
//! were inheritable for its spawn. [`TailReader::finish`] then returns once
//! the pipe has been quiet for [`QUIET`], with what arrived, and says the end
//! did not come ([`Tail::complete`]), instead of waiting forever or
//! returning nothing. Since the command's own processes are gone by then,
//! what they wrote was read ([`Tail::drained`]); only a reader that was
//! still receiving bytes when the wait ran out may have missed some.
//!
//! The reader is a plain thread, not a task on an async runtime's blocking
//! pool: a runtime waits for every blocking task when it shuts down
//! (tokio's `spawn_blocking` documentation), so a read that never ends
//! would stop the runtime from ending. A thread that is still blocked when
//! `finish` returns is left to end with its pipe.

use std::io::Read;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a pipe whose end has not come must have carried no byte before
/// [`TailReader::finish`] takes what the command wrote as read.
pub const QUIET: Duration = Duration::from_millis(200);

/// The last `keep` bytes of a stream, and how many bytes came in all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TailBuffer {
    keep: usize,
    bytes: std::collections::VecDeque<u8>,
    total: u64,
}

impl TailBuffer {
    /// A buffer that keeps at most `keep` bytes.
    pub fn new(keep: usize) -> Self {
        Self {
            keep,
            bytes: std::collections::VecDeque::new(),
            total: 0,
        }
    }

    /// Add `chunk`; the oldest bytes go first when more than `keep` are held.
    pub fn push(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len() as u64);
        // Only the last `keep` bytes of a large chunk can survive.
        let chunk = &chunk[chunk.len().saturating_sub(self.keep)..];
        let over = (self.bytes.len() + chunk.len()).saturating_sub(self.keep);
        self.bytes.drain(..over.min(self.bytes.len()));
        self.bytes.extend(chunk);
    }

    /// The kept bytes, oldest first.
    pub fn bytes(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }

    /// Bytes that came in all, the dropped ones included.
    pub fn total(&self) -> u64 {
        self.total
    }
}

/// What [`TailReader::finish`] gives back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tail {
    /// The last bytes of the stream, oldest first.
    pub bytes: Vec<u8>,
    /// Bytes the stream carried in all; more than `bytes.len()` when the
    /// start was dropped.
    pub total: u64,
    /// The end of the pipe came: no process holds its write end any more.
    pub complete: bool,
    /// Every byte in the pipe was read: it is complete, or it carried no
    /// byte for [`QUIET`] after the command's processes ended. False only
    /// when bytes were still arriving when the wait ran out, so some may be
    /// missing.
    pub drained: bool,
    /// A read error other than the end of the pipe.
    pub error: Option<String>,
}

impl Tail {
    /// The kept bytes as text, invalid UTF-8 replaced.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// Bytes dropped from the start.
    pub fn cut(&self) -> u64 {
        self.total.saturating_sub(self.bytes.len() as u64)
    }

    /// Whether the text can be trusted to hold everything the command wrote
    /// (up to the kept tail): drained and read without error.
    pub fn whole(&self) -> bool {
        self.drained && self.error.is_none()
    }

    /// A one-line note on how this stream was read, for a reason: `None`
    /// when the end came and there was no error.
    pub fn read_note(&self, stream: &str) -> Option<String> {
        let mut notes = Vec::new();
        if !self.complete {
            notes.push(if self.drained {
                format!(
                    "the {stream} pipe stayed open after the command's processes ended (a \
                     process outside them holds a copy of it); what they wrote was read"
                )
            } else {
                format!(
                    "the {stream} pipe stayed open and was still carrying bytes when the wait \
                     for it ended, so {stream} may be cut"
                )
            });
        }
        if let Some(e) = &self.error {
            notes.push(format!("reading {stream} failed: {e}"));
        }
        (!notes.is_empty()).then(|| notes.join("; "))
    }
}

struct State {
    tail: TailBuffer,
    done: bool,
    error: Option<String>,
    /// When the last bytes arrived, or the reader started.
    last_bytes: Instant,
}

/// A thread reading one pipe into a [`TailBuffer`].
pub struct TailReader {
    shared: Arc<(Mutex<State>, Condvar)>,
}

/// Read `pipe` on a new thread, keeping its last `keep` bytes. `None` (no
/// pipe) gives an empty, complete [`Tail`].
pub fn read_tail<R: Read + Send + 'static>(pipe: Option<R>, keep: usize) -> TailReader {
    let shared = Arc::new((
        Mutex::new(State {
            tail: TailBuffer::new(keep),
            done: pipe.is_none(),
            error: None,
            last_bytes: Instant::now(),
        }),
        Condvar::new(),
    ));
    let Some(mut pipe) = pipe else {
        return TailReader { shared };
    };
    let thread_shared = Arc::clone(&shared);
    std::thread::spawn(move || {
        let (lock, ended) = &*thread_shared;
        let mut chunk = vec![0u8; 16 * 1024];
        let error = loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break None,
                Ok(n) => {
                    let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    state.tail.push(&chunk[..n]);
                    state.last_bytes = Instant::now();
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => break Some(e.to_string()),
            }
        };
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        state.done = true;
        state.error = error;
        ended.notify_all();
    });
    TailReader { shared }
}

impl TailReader {
    /// Wait for the end of the pipe, then return what arrived. Call it only
    /// after the command's processes have ended: the wait is for the bytes
    /// still in the pipe, not for the command.
    ///
    /// Returns at the end of the pipe; or once no byte has arrived for
    /// [`QUIET`] (a process outside the command holds a copy of the write
    /// end, and the command, gone, cannot write more: [`Tail::drained`]); or
    /// at `grace` at the latest, with bytes still arriving (not drained).
    pub fn finish(self, grace: Duration) -> Tail {
        let (lock, ended) = &*self.shared;
        let started = Instant::now();
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let drained = loop {
            if state.done {
                break true;
            }
            let now = Instant::now();
            let idle = now.saturating_duration_since(state.last_bytes);
            if now.saturating_duration_since(started) >= QUIET.min(grace) && idle >= QUIET {
                break true;
            }
            let left = grace.saturating_sub(now.saturating_duration_since(started));
            if left.is_zero() {
                break false;
            }
            let step = left.min(QUIET.saturating_sub(idle).max(Duration::from_millis(10)));
            state = ended
                .wait_timeout(state, step)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        Tail {
            bytes: state.tail.bytes(),
            total: state.tail.total(),
            complete: state.done,
            drained,
            error: state.error.clone(),
        }
    }
}

/// How a process ended, for a failure reason: the exit code, also in hex
/// when it is a Windows status (`0xC0000142` is a program that could not
/// start).
pub fn exit_code_note(code: Option<i32>) -> String {
    match code {
        Some(code) if !(0..=255).contains(&code) => {
            format!("exit code {code} = {:#010x}", code as u32)
        }
        Some(code) => format!("exit code {code}"),
        None => "no exit code".to_owned(),
    }
}

/// One stream for a failure reason: `<name> (last N of M bytes): <text>`,
/// `<name>: (empty)`, and the [`Tail::read_note`] when there is one. The text
/// is cut to its last `max` bytes on a character boundary.
pub fn stream_note(name: &str, tail: &Tail, max: usize) -> String {
    let text = tail.text();
    let text = text.trim();
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let shown = &text[start..];
    let mut note = if shown.is_empty() {
        format!("{name}: (empty)")
    } else if start > 0 || tail.cut() > 0 {
        format!(
            "{name} (last {} of {} bytes):\n{shown}",
            shown.len(),
            tail.total
        )
    } else {
        format!("{name}:\n{shown}")
    };
    if let Some(read) = tail.read_note(name) {
        note.push_str(&format!("\n[{read}]"));
    }
    note
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_keeps_the_last_bytes_and_counts_all() {
        let mut t = TailBuffer::new(5);
        t.push(b"abc");
        assert_eq!(t.bytes(), b"abc");
        t.push(b"defg");
        assert_eq!(t.bytes(), b"cdefg");
        t.push(b"0123456789");
        assert_eq!(t.bytes(), b"56789");
        assert_eq!(t.total(), 17);
        let mut zero = TailBuffer::new(0);
        zero.push(b"xyz");
        assert!(zero.bytes().is_empty());
        assert_eq!(zero.total(), 3);
    }

    /// A pipe that gives some bytes, then never ends, as a pipe a process
    /// outside the command holds a copy of.
    struct NeverEnds {
        sent: bool,
    }

    impl Read for NeverEnds {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.sent {
                self.sent = true;
                buf[..6].copy_from_slice(b"before");
                return Ok(6);
            }
            loop {
                std::thread::park();
            }
        }
    }

    #[test]
    fn a_pipe_that_never_ends_returns_what_arrived_and_says_so() {
        let reader = read_tail(Some(NeverEnds { sent: false }), 64);
        let started = Instant::now();
        let tail = reader.finish(Duration::from_secs(30));
        // Back after the quiet period, long before the grace.
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(tail.text(), "before");
        assert!(!tail.complete);
        assert!(tail.drained && tail.whole());
        let note = stream_note("stderr", &tail, 4096);
        assert!(note.contains("before"), "{note}");
        assert!(note.contains("stayed open"), "{note}");
        assert!(note.contains("what they wrote was read"), "{note}");
    }

    /// A pipe that keeps carrying bytes past the grace: what arrived is
    /// returned, marked as possibly cut.
    #[test]
    fn a_pipe_still_carrying_bytes_at_the_grace_is_not_drained() {
        struct Trickle;
        impl Read for Trickle {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_millis(20));
                buf[0] = b'.';
                Ok(1)
            }
        }
        let tail = read_tail(Some(Trickle), 1 << 20).finish(Duration::from_millis(500));
        assert!(!tail.complete && !tail.drained && !tail.whole());
        assert!(!tail.bytes.is_empty());
        assert!(
            tail.read_note("stdout")
                .is_some_and(|n| n.contains("may be cut"))
        );
    }

    #[test]
    fn a_pipe_that_ends_is_complete_and_keeps_only_its_tail() {
        let data: Vec<u8> = (0..10_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let tail = read_tail(Some(std::io::Cursor::new(data.clone())), 4096)
            .finish(Duration::from_secs(10));
        assert!(tail.complete && tail.drained);
        assert_eq!(tail.total, 10_000);
        assert_eq!(tail.bytes, data[data.len() - 4096..]);
        assert_eq!(tail.cut(), 10_000 - 4096);
        assert_eq!(tail.read_note("stdout"), None);
        let note = stream_note("stdout", &tail, 100);
        assert!(
            note.starts_with("stdout (last 100 of 10000 bytes):"),
            "{note}"
        );
    }

    #[test]
    fn no_pipe_is_an_empty_complete_tail() {
        let tail = read_tail(None::<std::io::Empty>, 10).finish(Duration::ZERO);
        assert!(tail.complete && tail.whole());
        assert_eq!(stream_note("stdout", &tail, 10), "stdout: (empty)");
    }

    #[test]
    fn a_read_error_is_reported() {
        struct Fails;
        impl Read for Fails {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("the pipe broke oddly"))
            }
        }
        let tail = read_tail(Some(Fails), 10).finish(Duration::from_secs(10));
        assert!(tail.complete && !tail.whole());
        assert!(
            tail.read_note("stderr")
                .is_some_and(|n| n.contains("the pipe broke oddly"))
        );
    }

    #[test]
    fn the_note_cuts_on_a_character_boundary() {
        let tail = Tail {
            bytes: "aé".repeat(10).into_bytes(),
            total: 30,
            complete: true,
            drained: true,
            error: None,
        };
        // Any cut inside the two-byte letter moves forward to a boundary.
        for max in 0..30 {
            let _ = stream_note("stderr", &tail, max);
        }
    }

    #[test]
    fn exit_codes_are_shown_in_hex_when_they_are_windows_statuses() {
        assert_eq!(exit_code_note(Some(101)), "exit code 101");
        assert_eq!(
            exit_code_note(Some(-1073741502)),
            "exit code -1073741502 = 0xc0000142"
        );
        assert_eq!(exit_code_note(None), "no exit code");
    }
}
