//! The resumable transcript seam.
//!
//! A worker's transcript is the ordered list of neutral events it produced ("a neutral
//! event sequence that the transcript is rebuilt from"). The trait is
//! generic over the event type because the event type lives in `kgraf-runtime`,
//! which depends on this crate, not the other way round.
//!
//! The durable implementation is codex-rs `rollout/` (a JSONL session log), vendored
//! by the transcript stream. [`MemoryTranscript`] is the in-process reference for
//! tests and for the seam's contract.
//!
//! **Contract.** `append` returns the entry's sequence number, starting at 0 and
//! increasing by one. `load` returns every appended entry in order. An append that
//! fails returns an error; it never reports success for an event it did not store,
//! because resume would then rebuild a transcript that is silently short (fail loud).
//!
//! **Durability envelope.** By default, [`JsonlTranscript::append`]
//! returns once `write` and `flush` complete: the bytes have left the process and
//! are in the operating system's page cache. That survives *this process* crashing
//! or being killed. It does **not** survive the machine losing power or the kernel
//! panicking before the page cache is written back to physical storage. Call
//! [`JsonlTranscript::fsync_every_append`] to additionally `fsync` after every
//! append, which does survive a power loss, at real per-append latency cost (see
//! the `session` benchmark, which measures both).

use crate::SessionError;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

/// One stored event and its position.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranscriptEntry<E> {
    pub seq: u64,
    pub event: E,
}

/// Append neutral events; load them for resume.
#[async_trait::async_trait]
pub trait Transcript<E>: Send
where
    E: Send + Sync + 'static,
{
    /// Store one event and return its sequence number.
    async fn append(&mut self, event: E) -> Result<u64, SessionError>;

    /// Every stored event, in order.
    async fn load(&self) -> Result<Vec<TranscriptEntry<E>>, SessionError>;
}

/// An in-memory transcript.
#[derive(Clone, Debug, Default)]
pub struct MemoryTranscript<E> {
    entries: Vec<TranscriptEntry<E>>,
}

impl<E> MemoryTranscript<E> {
    /// An empty transcript.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl<E> Transcript<E> for MemoryTranscript<E>
where
    E: Clone + Send + Sync + 'static,
{
    async fn append(&mut self, event: E) -> Result<u64, SessionError> {
        let seq = self.entries.len() as u64;
        self.entries.push(TranscriptEntry { seq, event });
        Ok(seq)
    }

    async fn load(&self) -> Result<Vec<TranscriptEntry<E>>, SessionError> {
        Ok(self.entries.clone())
    }
}

/// A file that ended mid-write, discarded on repair.
///
/// Never constructed silently: [`JsonlTranscript::open`] refuses to open a torn
/// file at all, and [`JsonlTranscript::open_repairing_torn_tail`] is the only way
/// to get one of these, so a caller that wants to resume past a crash has to look
/// at what it is discarding (fail loud; codex-rs `rollout/` pads and logs a `warn!`
/// instead, and the padded line then silently vanishes from `load_rollout_items`'
/// count -- see `third_party/codex/UPSTREAM`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TornTail {
    /// The byte offset where the last complete, newline-terminated record ends.
    pub valid_prefix_len: u64,
    /// The bytes after that offset: the torn or corrupt tail, exactly as found.
    pub discarded: String,
}

/// A write in progress when `append`'s caller stopped awaiting it (its future
/// was dropped -- cancelled by a `select!` or a `timeout`, not by an error).
///
/// Recorded *before* the write is issued and resolved by [`JsonlTranscript::resolve_pending`]
/// at the start of the next `append`, so a cancelled call's write is always
/// folded into `next_seq` (or rolled back) before a new `seq` is chosen. Without
/// this, a duplicate `seq` could be written: the abandoned write can still land
/// durably (the file handle and its OS-level state outlive the cancelled Rust
/// future), while `next_seq` was never advanced to account for it.
///
/// **Cleared only once resolved.** Resolution itself awaits (`flush`, `metadata`,
/// `set_len`), so it can be cancelled too. The record therefore stays in
/// `self.pending` until the outcome is settled (`next_seq` advanced, or the file
/// truncated back to `pre_len`), and a cancelled resolution is simply redone by
/// the next call. Taking it out before the first await lost it when the caller
/// dropped `append` during its trailing `flush`: the next append reused the seq
/// (`TranscriptSeqMismatch` on reopen) or rolled back to a length read while the
/// orphaned write was still in flight (a torn tail). Both reproduced under load
/// by `a_genuinely_cancelled_large_append_never_corrupts_the_log`.
#[derive(Clone, Copy, Debug)]
struct PendingWrite {
    seq: u64,
    pre_len: u64,
    /// The exact byte length of the record `append` issued. A clean `flush`
    /// only proves no I/O is in flight, not that the *whole* record landed:
    /// a write cancelled partway through a large record
    /// (needing several internal `write` calls) can leave a shorter prefix
    /// durably written, and `flush` on an otherwise-idle file still returns
    /// `Ok`. [`JsonlTranscript::resolve_pending`] compares the file's actual
    /// length against `pre_len + line_len` before trusting the write.
    line_len: u64,
}

/// A durable, JSONL-backed [`Transcript`].
///
/// One record per line: `{"seq":<u64>,"event":<E>}`, the same shape
/// [`TranscriptEntry`] already serializes to, so a JSONL file and a
/// [`MemoryTranscript`] agree on wire format. Every [`Transcript::append`] does
/// one `write_all` plus one `flush` before returning (codex-rs `rollout/`'s
/// `JsonlWriter::write_line`; `third_party/codex/UPSTREAM`), so a crash can only
/// ever land between two complete records, never inside the byte range a caller
/// was already told is stored.
///
/// **Resume reads the file, not a cache.** [`Transcript::load`] re-reads from
/// disk on every call rather than trusting in-memory state, so a transcript
/// opened by one process and appended to would still be read correctly by
/// another (the file, not the handle, is the source of truth); kGraf's
/// single-writer-per-worktree rule (disjoint writes) means that case does not arise in
/// practice, but the read path does not assume it.
///
/// **A failed write never leaves a half-written record on disk**: `append` records the file's length before writing, and on any error
/// (from the write itself, or from a durability failure) truncates back to
/// that length. If the truncation itself fails, the handle is poisoned
/// ([`SessionError::TranscriptPoisoned`]) rather than risk appending on top of
/// an unknown-length file.
pub struct JsonlTranscript<E> {
    path: PathBuf,
    file: tokio::fs::File,
    next_seq: u64,
    /// Set when a rollback failed; every further operation refuses rather than
    /// compound a write onto a file whose length we could not restore.
    poisoned: bool,
    /// `fsync` after every append (see the module doc's durability envelope).
    fsync: bool,
    pending: Option<PendingWrite>,
    /// False while a repair (`set_len`, `sync_all` in
    /// [`JsonlTranscript::open_repairing_torn_tail`]) may be in flight on
    /// `file`. With `pending`, it tells `Drop` whether the handle is idle.
    settled: bool,
    _event: PhantomData<fn() -> E>,
}

/// The exclusive lock is released by an explicit `LOCK_UN` when the handle is
/// idle, not only by the close: a close leaves the lock held by any child
/// another thread forked meanwhile until that child execs, and a resume of
/// this transcript in the window was refused as `TranscriptLocked`
/// (`kgraf_core::flock`). With a write or repair possibly still in flight
/// (a cancelled `append`, `pending` set) the lock is left to the close, which
/// tokio performs only after that operation ends: releasing early would let a
/// new handle read the tail while the orphaned write lands.
impl<E> Drop for JsonlTranscript<E> {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.pending.is_none() && self.settled {
            use std::os::fd::AsRawFd;
            // SAFETY: releases the lock this handle's own descriptor holds; the
            // descriptor is open for the whole call.
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

impl<E> std::fmt::Debug for JsonlTranscript<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlTranscript")
            .field("path", &self.path)
            .field("next_seq", &self.next_seq)
            .field("poisoned", &self.poisoned)
            .field("fsync", &self.fsync)
            .finish()
    }
}

impl<E> JsonlTranscript<E>
where
    E: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// Open (creating if absent) the transcript at `path`.
    ///
    /// Refuses to open a file whose last line is torn, whose content is
    /// otherwise corrupt (fail loud), whose bytes are not valid UTF-8, or whose
    /// `seq` numbering does not match each entry's position:
    /// [`SessionError::TornTranscriptLine`], [`SessionError::CorruptTranscriptLine`],
    /// [`SessionError::InvalidUtf8`], or [`SessionError::TranscriptSeqMismatch`].
    /// Use [`JsonlTranscript::open_repairing_torn_tail`] to resume past a torn
    /// tail instead of failing.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        // Held as `Self` from the lock on, so a refusal below releases the lock
        // through `Drop` like any other handle.
        let mut t = Self::open_locked(path.into()).await?;
        // The lock is already held, so nothing else with this
        // fix can be concurrently appending while `next_seq` is read below.
        let read = read_jsonl::<E>(&t.path).await?;
        if let Some(torn) = read.torn {
            return Err(SessionError::TornTranscriptLine {
                path: t.path.clone(),
                byte_offset: torn.valid_prefix_len,
                content: torn.discarded,
            });
        }
        t.next_seq = read.next_seq;
        Ok(t)
    }

    /// Open at `path`, truncating a torn tail if present rather than refusing.
    /// Returns the truncated tail alongside the transcript so the caller
    /// records what was discarded instead of it vanishing into a log line
    /// (fail loud). Non-final corruption is still refused: it is not
    /// attributable to a crash mid-write, so there is nothing safe to repair.
    pub async fn open_repairing_torn_tail(
        path: impl Into<PathBuf>,
    ) -> Result<(Self, Option<TornTail>), SessionError> {
        let mut t = Self::open_locked(path.into()).await?;
        // The lock is already held: both the read that finds
        // the torn tail and the truncation that repairs it happen while no
        // other handle can be appending, so neither races a concurrent write.
        let read = read_jsonl::<E>(&t.path).await?;
        if let Some(torn) = &read.torn {
            // Truncate through the *same* locked handle, not a second `open`
            // of the path: a second `open` would not be covered by the lock
            // this handle holds and would defeat the point of taking it early.
            // Unsettled until both land: a cancellation in between leaves the
            // lock to the close (see `Drop`).
            // An operation that returned, with an error or not, has ended, so
            // the handle is settled again before the error is mapped: only a
            // cancellation mid-await leaves it unsettled.
            t.settled = false;
            let truncated = t.file.set_len(torn.valid_prefix_len).await;
            t.settled = true;
            truncated.map_err(|source| io_err(&t.path, source))?;
            t.settled = false;
            let synced = t.file.sync_all().await;
            t.settled = true;
            synced.map_err(|source| io_err(&t.path, source))?;
        }
        t.next_seq = read.next_seq;
        Ok((t, read.torn))
    }

    /// Toggle `fsync` after every append (see the module doc's durability
    /// envelope). Off by default.
    pub fn fsync_every_append(mut self, enabled: bool) -> Self {
        self.fsync = enabled;
        self
    }

    /// Create (if absent) and open `path`, take its exclusive lock, and set
    /// its mode, all before any content is read (the lock must
    /// be held before `next_seq` is read or a torn tail is truncated, not
    /// after).
    async fn open_locked(path: PathBuf) -> Result<Self, SessionError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| io_err(&path, source))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .await
            .map_err(|source| io_err(&path, source))?;
        // An exclusive, non-blocking `flock` for the handle's lifetime: disjoint writes already mean only one
        // worker owns this worktree, so this should never contend in
        // practice, but it turns "two processes both think they own this
        // transcript" from silently interleaved writes into a loud refusal at
        // open time, rather than relying only on the organizational
        // invariant. Released by `Drop` with an explicit unlock (see there).
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                return Err(SessionError::TranscriptLocked { path });
            }
        }
        let t = Self {
            path,
            file,
            next_seq: 0,
            poisoned: false,
            fsync: false,
            pending: None,
            settled: true,
            _event: PhantomData,
        };
        // Mode 0600: a transcript can carry file contents,
        // command output and reasoning text, so it gets the same treatment as
        // any other file holding a worker's private data. Applied every open,
        // not only on creation, so a file that predates this fix is also
        // tightened.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&t.path, std::fs::Permissions::from_mode(0o600))
                .await
                .map_err(|source| io_err(&t.path, source))?;
        }
        Ok(t)
    }

    /// The file this transcript is stored in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Resolve any write left in flight by a cancelled `append` (see
    /// [`PendingWrite`]) before doing anything else. A no-op when nothing is
    /// pending.
    async fn resolve_pending(&mut self) -> Result<(), SessionError> {
        // Copied, not taken: see "Cleared only once resolved" on `PendingWrite`.
        let Some(pending) = self.pending else {
            return Ok(());
        };
        // `flush` on the same file handle waits for any write tokio's
        // `fs::File` left running internally, even one this struct's own
        // caller stopped awaiting -- `tokio::fs::File`'s `poll_flush` joins a
        // still-`Busy` internal write task rather than ignoring it. That is
        // what makes resolving here safe rather than a guess.
        if let Err(source) = self.file.flush().await {
            self.recover_or_poison(pending.pre_len).await;
            return Err(io_err(&self.path, source));
        }
        // A clean `flush` only proves no write is *in flight*; it does not
        // prove the *whole* record landed. A large record
        // needs several internal `write` calls, and a cancellation landing
        // between two of them leaves a shorter prefix durably written while
        // `flush` on the now-idle file still returns `Ok`. Verify the byte
        // count explicitly before trusting the write.
        let actual_len = self
            .file
            .metadata()
            .await
            .map_err(|source| io_err(&self.path, source))?
            .len();
        let expected_len = pending.pre_len + pending.line_len;
        if actual_len != expected_len {
            let written = actual_len.saturating_sub(pending.pre_len);
            self.recover_or_poison(pending.pre_len).await;
            return Err(SessionError::TranscriptPartialWrite {
                path: self.path.clone(),
                seq: pending.seq,
                written,
                expected: pending.line_len,
            });
        }
        if self.fsync
            && let Err(source) = self.file.sync_all().await
        {
            // The write reached the page cache but durability is not
            // confirmed; treated the same as a write failure rather than
            // silently claiming success that a fsync-backed caller relies on.
            self.recover_or_poison(pending.pre_len).await;
            return Err(io_err(&self.path, source));
        }
        // No await between these two assignments, so a cancellation cannot
        // separate them.
        self.next_seq = pending.seq + 1;
        self.pending = None;
        Ok(())
    }

    /// Undo a failed (or unconfirmed) write by truncating back to `pre_len`.
    /// If that itself fails, poison the handle: a length we
    /// cannot restore is a state no further append may build on.
    ///
    /// `pending` is cleared only after the truncation is issued and awaited, so a
    /// caller that cancels mid-rollback leaves the write pending and the next
    /// call redoes the check and the rollback (both are idempotent).
    async fn recover_or_poison(&mut self, pre_len: u64) {
        if self.file.set_len(pre_len).await.is_err() || self.file.sync_all().await.is_err() {
            self.poisoned = true;
        }
        self.pending = None;
    }
}

#[async_trait::async_trait]
impl<E> Transcript<E> for JsonlTranscript<E>
where
    E: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    async fn append(&mut self, event: E) -> Result<u64, SessionError> {
        self.resolve_pending().await?;
        if self.poisoned {
            return Err(SessionError::TranscriptPoisoned {
                path: self.path.clone(),
            });
        }
        let seq = self.next_seq;
        let entry = TranscriptEntry { seq, event };
        let mut line =
            serde_json::to_string(&entry).map_err(|e| SessionError::CorruptTranscriptLine {
                path: self.path.clone(),
                line: seq,
                reason: format!("could not serialize the event to append: {e}"),
            })?;
        line.push('\n');
        let line_len = line.len() as u64;
        // `tokio::fs::File::metadata` does not wait for a write tokio still runs
        // in the background (only `flush`, `set_len` and further writes do), so
        // `pre_len` must be read from an idle handle. `resolve_pending` above
        // leaves it idle whenever a write was outstanding; this flush makes the
        // invariant local instead of relying on that. It costs nothing when idle.
        self.file
            .flush()
            .await
            .map_err(|source| io_err(&self.path, source))?;
        let pre_len = self
            .file
            .metadata()
            .await
            .map_err(|source| io_err(&self.path, source))?
            .len();
        // Recorded before the write is issued, not after: if this call's own
        // future is dropped while `write_all` below is still pending, this
        // assignment already happened (it is not itself behind an `.await`),
        // so `self.pending` survives the cancellation and the next operation
        // resolves the real outcome before allocating a new `seq`.
        self.pending = Some(PendingWrite {
            seq,
            pre_len,
            line_len,
        });
        if let Err(source) = self.file.write_all(line.as_bytes()).await {
            self.recover_or_poison(pre_len).await;
            return Err(io_err(&self.path, source));
        }
        self.resolve_pending().await?;
        Ok(seq)
    }

    async fn load(&self) -> Result<Vec<TranscriptEntry<E>>, SessionError> {
        let read = read_jsonl::<E>(&self.path).await?;
        if let Some(torn) = read.torn {
            return Err(SessionError::TornTranscriptLine {
                path: self.path.clone(),
                byte_offset: torn.valid_prefix_len,
                content: torn.discarded,
            });
        }
        Ok(read.entries)
    }
}

/// The result of one forward scan of a JSONL transcript file.
struct JsonlRead<E> {
    entries: Vec<TranscriptEntry<E>>,
    /// One past the highest `seq` among `entries`, or 0 if empty.
    next_seq: u64,
    /// Set when the file's content after the last complete record is
    /// non-empty: either a write that never finished, or trailing garbage.
    /// Distinguished from a corrupt *earlier* line, which is always a hard
    /// error (see below) because it cannot be explained by a crash mid-append.
    torn: Option<TornTail>,
}

/// Read every complete line of a JSONL transcript, forward from byte 0.
///
/// A line that is not the last one and fails to parse is
/// [`SessionError::CorruptTranscriptLine`] unconditionally: an append-only,
/// flush-per-event log (see [`JsonlTranscript::append`]) can only ever be
/// incomplete at its very end, so corruption earlier in the file means
/// something other than a crash touched it, and there is no safe amount to
/// discard automatically.
///
/// **Bytes must be valid UTF-8**: a lossy substitution could
/// turn a corrupt byte into a plausible-looking character inside a JSON
/// string, hiding the corruption instead of reporting it.
///
/// **Every entry's `seq` must equal its position** among the entries read so
/// far: this is the read-time check that catches a duplicated
/// or skipped sequence number, which is the on-disk symptom a write left in
/// flight by a cancelled, un-resolved `append` would produce.
async fn read_jsonl<E>(path: &Path) -> Result<JsonlRead<E>, SessionError>
where
    E: DeserializeOwned,
{
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(JsonlRead {
                entries: Vec::new(),
                next_seq: 0,
                torn: None,
            });
        }
        Err(source) => return Err(io_err(path, source)),
    };
    let text = String::from_utf8(bytes).map_err(|e| SessionError::InvalidUtf8 {
        path: path.to_path_buf(),
        byte_offset: e.utf8_error().valid_up_to() as u64,
    })?;

    let mut entries = Vec::new();
    let mut offset: u64 = 0;
    let mut torn = None;
    let mut lines = text.split('\n').peekable();
    let mut line_number: u64 = 0;
    while let Some(line) = lines.next() {
        let is_last_fragment = lines.peek().is_none();
        if is_last_fragment {
            // `split('\n')` yields a trailing "" when the file ends with `\n`
            // (the well-formed case); anything else here is content after the
            // last newline, i.e. a write that never completed.
            if !line.is_empty() {
                torn = Some(TornTail {
                    valid_prefix_len: offset,
                    discarded: line.to_owned(),
                });
            }
            break;
        }
        if !line.trim().is_empty() {
            let entry: TranscriptEntry<E> =
                serde_json::from_str(line).map_err(|e| SessionError::CorruptTranscriptLine {
                    path: path.to_path_buf(),
                    line: line_number,
                    reason: e.to_string(),
                })?;
            let index = entries.len() as u64;
            if entry.seq != index {
                return Err(SessionError::TranscriptSeqMismatch {
                    path: path.to_path_buf(),
                    index,
                    seq: entry.seq,
                });
            }
            entries.push(entry);
        }
        offset += line.len() as u64 + 1; // +1 for the newline `split` consumed.
        line_number += 1;
    }
    let next_seq = entries.last().map_or(0, |e| e.seq + 1);
    Ok(JsonlRead {
        entries,
        next_seq,
        torn,
    })
}

fn io_err(path: &Path, source: std::io::Error) -> SessionError {
    SessionError::TranscriptIo {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn appended_events_load_in_order_with_contiguous_sequence_numbers() {
        let mut t = MemoryTranscript::new();
        assert_eq!(t.append("a".to_owned()).await.expect("a"), 0);
        assert_eq!(t.append("b".to_owned()).await.expect("b"), 1);
        assert_eq!(t.append("c".to_owned()).await.expect("c"), 2);
        let loaded = t.load().await.expect("load");
        let events: Vec<&str> = loaded.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(events, vec!["a", "b", "c"]);
        let seqs: Vec<u64> = loaded.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn a_transcript_is_usable_as_a_trait_object() {
        let mut t: Box<dyn Transcript<u32>> = Box::new(MemoryTranscript::new());
        t.append(7).await.expect("append");
        assert_eq!(t.load().await.expect("load")[0].event, 7);
    }

    fn temp_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("transcript.jsonl");
        (dir, path)
    }

    #[tokio::test]
    async fn a_jsonl_transcript_resumes_the_exact_event_sequence() {
        let (_dir, path) = temp_path();
        {
            let mut t: JsonlTranscript<String> = JsonlTranscript::open(&path).await.expect("open");
            assert_eq!(t.append("a".to_owned()).await.expect("a"), 0);
            assert_eq!(t.append("b".to_owned()).await.expect("b"), 1);
            assert_eq!(t.append("c".to_owned()).await.expect("c"), 2);
        }
        // A fresh open, as a resumed process would do.
        let mut resumed: JsonlTranscript<String> =
            JsonlTranscript::open(&path).await.expect("reopen");
        let loaded = resumed.load().await.expect("load");
        let events: Vec<&str> = loaded.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(events, vec!["a", "b", "c"]);
        let seqs: Vec<u64> = loaded.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2]);
        // Appending after resume continues the same sequence.
        assert_eq!(resumed.append("d".to_owned()).await.expect("d"), 3);
        let reloaded = resumed.load().await.expect("reload");
        assert_eq!(reloaded.len(), 4);
        assert_eq!(reloaded[3].seq, 3);
    }

    #[tokio::test]
    async fn a_torn_last_line_is_reported_not_dropped() {
        let (_dir, path) = temp_path();
        {
            let mut t: JsonlTranscript<String> = JsonlTranscript::open(&path).await.expect("open");
            t.append("a".to_owned()).await.expect("a");
            t.append("b".to_owned()).await.expect("b");
        }
        // Simulate a crash mid-write: a third record cut off before its
        // closing brace and with no trailing newline.
        let mut raw = tokio::fs::read(&path).await.expect("read");
        raw.extend_from_slice(br#"{"seq":2,"event":"c"#);
        tokio::fs::write(&path, &raw)
            .await
            .expect("write torn tail");

        let opened = JsonlTranscript::<String>::open(&path).await;
        let err = opened.expect_err("a torn last line must not open silently");
        let SessionError::TornTranscriptLine {
            content,
            byte_offset,
            ..
        } = &err
        else {
            panic!("expected TornTranscriptLine, got {err:?}");
        };
        assert!(content.contains(r#""seq":2"#), "{content}");
        assert!(*byte_offset > 0);

        // `load` on an already-open (well-formed-until-now) transcript also
        // refuses rather than silently returning only the two good entries.
        let (_dir2, good_path) = temp_path();
        let mut good: JsonlTranscript<String> =
            JsonlTranscript::open(&good_path).await.expect("open");
        good.append("a".to_owned()).await.expect("a");
        good.append("b".to_owned()).await.expect("b");
        let mut raw = tokio::fs::read(good.path()).await.expect("read");
        raw.extend_from_slice(b"{not json");
        tokio::fs::write(good.path(), &raw).await.expect("torn");
        let load_err = good
            .load()
            .await
            .expect_err("torn tail refused on load too");
        assert!(matches!(load_err, SessionError::TornTranscriptLine { .. }));
    }

    #[tokio::test]
    async fn open_repairing_torn_tail_truncates_and_reports_what_was_discarded() {
        let (_dir, path) = temp_path();
        {
            let mut t: JsonlTranscript<String> = JsonlTranscript::open(&path).await.expect("open");
            t.append("a".to_owned()).await.expect("a");
            t.append("b".to_owned()).await.expect("b");
        }
        let mut raw = tokio::fs::read(&path).await.expect("read");
        let torn_bytes = br#"{"seq":2,"event":"c"#;
        raw.extend_from_slice(torn_bytes);
        tokio::fs::write(&path, &raw)
            .await
            .expect("write torn tail");

        let (mut transcript, torn) = JsonlTranscript::<String>::open_repairing_torn_tail(&path)
            .await
            .expect("repair");
        let torn = torn.expect("a torn tail must be reported, not silently absent");
        assert_eq!(torn.discarded, String::from_utf8_lossy(torn_bytes));

        // The transcript is now usable and resumes from where the two good
        // entries left off, not from a corrupted seq counter.
        let loaded = transcript.load().await.expect("load after repair");
        assert_eq!(loaded.len(), 2);
        assert_eq!(transcript.append("c".to_owned()).await.expect("append"), 2);

        // Repairing an already well-formed file is a no-op that reports no
        // torn tail.
        let (_dir2, clean_path) = temp_path();
        {
            let mut t: JsonlTranscript<String> =
                JsonlTranscript::open(&clean_path).await.expect("open");
            t.append("x".to_owned()).await.expect("x");
        }
        let (_clean, none_torn) = JsonlTranscript::<String>::open_repairing_torn_tail(&clean_path)
            .await
            .expect("repair no-op");
        assert!(none_torn.is_none());
    }

    #[tokio::test]
    async fn a_corrupt_non_final_line_is_a_hard_error_never_silently_skipped() {
        let (_dir, path) = temp_path();
        tokio::fs::write(&path, b"not json at all\n{\"seq\":1,\"event\":\"b\"}\n")
            .await
            .expect("write");
        let err = JsonlTranscript::<String>::open(&path)
            .await
            .expect_err("corrupt non-final line must be refused");
        assert!(matches!(
            err,
            SessionError::CorruptTranscriptLine { line: 0, .. }
        ));
    }

    #[tokio::test]
    async fn opening_an_absent_file_starts_a_fresh_empty_transcript() {
        let (_dir, path) = temp_path();
        let mut t: JsonlTranscript<String> = JsonlTranscript::open(&path).await.expect("open");
        assert!(t.load().await.expect("load").is_empty());
        assert_eq!(t.append("a".to_owned()).await.expect("first append"), 0);
    }

    // -- Regressions --

    #[tokio::test]
    async fn invalid_utf8_is_rejected_not_lossily_substituted() {
        // `from_utf8_lossy` would turn 0xff/0xfe into U+FFFD and keep
        // going, hiding real corruption inside what looks like a parseable
        // (if garbled) line.
        let (_dir, path) = temp_path();
        tokio::fs::write(&path, [b'{', 0xff, 0xfe, b'}', b'\n'])
            .await
            .expect("write raw bytes");
        let err = JsonlTranscript::<String>::open(&path)
            .await
            .expect_err("invalid utf8 must be refused, not substituted");
        assert!(
            matches!(err, SessionError::InvalidUtf8 { byte_offset: 1, .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_duplicate_seq_is_rejected_on_read() {
        // The on-disk symptom a cancelled,
        // un-resolved append would produce is caught here even if the
        // write-time guard (below) were absent.
        let (_dir, path) = temp_path();
        tokio::fs::write(
            &path,
            b"{\"seq\":0,\"event\":\"a\"}\n{\"seq\":0,\"event\":\"b\"}\n",
        )
        .await
        .expect("write");
        let err = JsonlTranscript::<String>::open(&path)
            .await
            .expect_err("a duplicate seq must be refused");
        assert!(
            matches!(
                err,
                SessionError::TranscriptSeqMismatch {
                    index: 1,
                    seq: 0,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn an_out_of_order_seq_is_rejected_on_read() {
        let (_dir, path) = temp_path();
        tokio::fs::write(
            &path,
            b"{\"seq\":0,\"event\":\"a\"}\n{\"seq\":2,\"event\":\"c\"}\n",
        )
        .await
        .expect("write");
        let err = JsonlTranscript::<String>::open(&path)
            .await
            .expect_err("a skipped seq must be refused");
        assert!(matches!(
            err,
            SessionError::TranscriptSeqMismatch {
                index: 1,
                seq: 2,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn resolve_pending_settles_a_write_left_in_flight_by_a_cancelled_append_before_a_new_seq_is_chosen()
     {
        // This reproduces exactly what a cancelled
        // `append` leaves behind without relying on racing real async
        // cancellation timing (which is not deterministic to test): the
        // record's bytes already durably written -- as they would be, since
        // the write is not itself cut short by cancellation, only the Rust
        // code *after* it is skipped -- but `next_seq` not yet advanced,
        // because `self.pending` was set and the cancelled future never
        // reached `resolve_pending`'s continuation.
        let (_dir, path) = temp_path();
        let mut t: JsonlTranscript<String> = JsonlTranscript::open(&path).await.expect("open");
        let entry = TranscriptEntry {
            seq: 0u64,
            event: "a".to_owned(),
        };
        let mut line = serde_json::to_string(&entry).expect("ser");
        line.push('\n');
        let line_len = line.len() as u64;
        t.file.write_all(line.as_bytes()).await.expect("write");
        t.file.flush().await.expect("flush");
        t.pending = Some(PendingWrite {
            seq: 0,
            pre_len: 0,
            line_len,
        });
        assert_eq!(t.next_seq, 0, "as a cancelled append would leave it");

        // Without the fix, this would read `next_seq` (still 0) and write a
        // second record also stamped seq=0.
        let seq = t
            .append("b".to_owned())
            .await
            .expect("append resolves the pending write first");
        assert_eq!(
            seq, 1,
            "the pending write must be resolved (seq 0 claimed) before a new seq is chosen"
        );
        let loaded = t.load().await.expect("load");
        let seqs: Vec<u64> = loaded.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1], "no duplicate seq");
    }

    #[tokio::test]
    async fn a_failed_write_is_rolled_back_not_left_corrupting_the_log() {
        // Directly exercises the rollback path a write failure takes,
        // the same one `append` calls on its own write error.
        let (_dir, path) = temp_path();
        let mut t: JsonlTranscript<String> = JsonlTranscript::open(&path).await.expect("open");
        t.append("a".to_owned()).await.expect("a");
        let pre_len = t.file.metadata().await.expect("meta").len();

        // Simulate a write that landed only partially (as a genuine crash
        // mid-syscall would leave it) by writing incomplete bytes directly,
        // bypassing `append`, then invoking the same recovery `append` uses.
        t.file
            .write_all(b"{\"seq\":1,\"event\":\"junk")
            .await
            .expect("simulate a partial write");
        t.recover_or_poison(pre_len).await;

        assert!(!t.poisoned, "a successful rollback must not poison it");
        let len_after = t.file.metadata().await.expect("meta").len();
        assert_eq!(len_after, pre_len, "the partial write must be rolled back");

        drop(t);
        let mut reopened: JsonlTranscript<String> =
            JsonlTranscript::open(&path).await.expect("reopen");
        let loaded = reopened.load().await.expect("load: no corruption remains");
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            reopened.append("b".to_owned()).await.expect("append"),
            1,
            "the transcript is fully usable after the rollback"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_transcript_file_is_created_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = temp_path();
        let _t: JsonlTranscript<String> = JsonlTranscript::open(&path).await.expect("open");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_second_writer_is_refused_not_silently_interleaved() {
        // Two writers: disjoint writes say this should not arise, but
        // the lock turns a violation of that invariant into a loud refusal
        // instead of two processes silently interleaving appends.
        let (_dir, path) = temp_path();
        let first: JsonlTranscript<String> = JsonlTranscript::open(&path).await.expect("first");
        let second = JsonlTranscript::<String>::open(&path).await;
        assert!(
            matches!(second, Err(SessionError::TranscriptLocked { .. })),
            "{second:?}"
        );
        // Releasing the first handle frees the lock for a genuine resume.
        drop(first);
        let _third: JsonlTranscript<String> = JsonlTranscript::open(&path)
            .await
            .expect("a resume after the prior handle is dropped must succeed");
    }

    /// A dropped handle's lock is free for a resume at once, while other
    /// threads fork: the drop releases it explicitly, so no forked copy of the
    /// descriptor keeps it and the resume is not refused as `TranscriptLocked`.
    /// The same holds for a refused open (a torn tail): its lock goes too.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_resume_right_after_a_drop_is_not_refused_while_other_threads_fork() {
        let (_dir, path) = temp_path();
        let _forkers = kgraf_core::flock::Forkers::start(4);
        let mut refused = 0;
        for i in 0..300u64 {
            match JsonlTranscript::<String>::open(&path).await {
                Ok(mut t) => {
                    let seq = t.append(format!("e{i}")).await.expect("append");
                    assert_eq!(seq, i - refused);
                }
                Err(SessionError::TranscriptLocked { .. }) => refused += 1,
                Err(e) => panic!("{e:?}"),
            }
        }
        assert_eq!(refused, 0, "a resume was refused by a released lock");

        let torn = path.with_extension("torn");
        std::fs::write(&torn, "{\"seq\":0").expect("torn");
        let mut held = 0;
        for _ in 0..300 {
            match JsonlTranscript::<String>::open(&torn).await {
                Err(SessionError::TornTranscriptLine { .. }) => {}
                Err(SessionError::TranscriptLocked { .. }) => held += 1,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(held, 0, "a refused open left its lock held");
    }

    // -- Cancellation regression tests --

    /// A regression that hangs (a write that never settles) must fail, not
    /// stall the suite.
    const CANCEL_TEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

    /// Poll `fut` exactly once.
    async fn poll_once<F: std::future::Future + Unpin>(fut: &mut F) -> std::task::Poll<F::Output> {
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::pin::Pin::new(&mut *fut).poll(cx)))
            .await
    }

    /// Reopen `path` and check the one invariant every cancellation must keep:
    /// the file parses and each `seq` equals its position.
    async fn reopen_events(path: &Path) -> Vec<String> {
        let reopened = match JsonlTranscript::<String>::open(path).await {
            Ok(t) => t,
            Err(e) => panic!("the log must stay resumable after a cancellation: {e:?}"),
        };
        let loaded = reopened.load().await.expect("load");
        for (i, e) in loaded.iter().enumerate() {
            assert_eq!(e.seq, i as u64, "contiguous seq");
        }
        loaded.into_iter().map(|e| e.event).collect()
    }

    #[tokio::test]
    async fn an_append_cancelled_after_its_write_landed_claims_its_seq() {
        // Deterministic form of the race found under load: `append` is
        // dropped while it awaits the trailing `flush` in `resolve_pending`,
        // after its record is fully on disk. The next append must see that
        // record, not reuse its seq. A small record is one `write` that tokio
        // issues in the background and reports done at once, so once the file
        // holds the whole record while the future is still pending, the
        // future is past `write_all` and inside the resolution.
        tokio::time::timeout(CANCEL_TEST_LIMIT, async {
            let (_dir, path) = temp_path();
            let mut t: JsonlTranscript<String> = JsonlTranscript::open(&path).await.expect("open");
            let mut expected: Vec<String> = Vec::new();
            let mut caught = false;
            // Each attempt either catches the window or completes normally
            // (then it is a valid append, and the next attempt tries again).
            for attempt in 0..100 {
                let event = format!("e{attempt}");
                let before = std::fs::metadata(&path).expect("metadata").len();
                let record = TranscriptEntry {
                    seq: expected.len() as u64,
                    event: event.clone(),
                };
                let full = before + serde_json::to_string(&record).expect("ser").len() as u64 + 1;
                let mut fut = t.append(event.clone());
                loop {
                    match poll_once(&mut fut).await {
                        std::task::Poll::Ready(r) => {
                            assert_eq!(r.expect("append"), expected.len() as u64);
                            break;
                        }
                        std::task::Poll::Pending => {
                            // Let background work finish without polling the
                            // future, so it cannot advance past its await.
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                            if std::fs::metadata(&path).expect("metadata").len() == full {
                                caught = true;
                                break; // drop `fut` inside `resolve_pending`
                            }
                        }
                    }
                }
                drop(fut);
                expected.push(event);
                if caught {
                    break;
                }
            }
            assert!(
                caught,
                "never observed an append pending after its write landed"
            );

            let seq = t
                .append("next".to_owned())
                .await
                .expect("append after cancel");
            assert_eq!(
                seq,
                expected.len() as u64,
                "the cancelled record's seq must be claimed, not reused"
            );
            expected.push("next".to_owned());
            drop(t);
            assert_eq!(reopen_events(&path).await, expected);
        })
        .await
        .expect("test exceeded its time limit");
    }

    #[tokio::test]
    async fn a_resolution_cancelled_at_any_await_keeps_the_write_pending() {
        // `resolve_pending` awaits `flush`, `metadata`, then (for a short
        // write) `set_len` and `sync_all`. Drop it after each successive
        // poll: until it finishes, the pending record must survive so the
        // next call redoes the check and the rollback. Before the fix the
        // record was taken out before the first await and lost.
        tokio::time::timeout(CANCEL_TEST_LIMIT, async {
            let mut exercised_every_await = false;
            for polls in 1..=16 {
                let (_dir, path) = temp_path();
                let mut t: JsonlTranscript<String> =
                    JsonlTranscript::open(&path).await.expect("open");
                t.append("a".to_owned()).await.expect("a");
                let pre_len = std::fs::metadata(&path).expect("metadata").len();
                // What a write cut short leaves: a prefix on disk, the record
                // pending, `next_seq` not advanced.
                t.file
                    .write_all(b"{\"seq\":1,\"event\":\"par")
                    .await
                    .expect("partial");
                t.file.flush().await.expect("flush");
                t.pending = Some(PendingWrite {
                    seq: 1,
                    pre_len,
                    line_len: 100,
                });

                let mut settled = false;
                {
                    let mut fut = Box::pin(t.resolve_pending());
                    for _ in 0..polls {
                        if let std::task::Poll::Ready(r) = poll_once(&mut fut).await {
                            assert!(
                                matches!(
                                    r,
                                    Err(SessionError::TranscriptPartialWrite { seq: 1, .. })
                                ),
                                "{r:?}"
                            );
                            settled = true;
                            break;
                        }
                        // Give the background operation time to finish, so the
                        // next poll moves to the following await point.
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                }
                if settled {
                    assert!(t.pending.is_none(), "resolved: nothing left pending");
                    assert_eq!(std::fs::metadata(&path).expect("metadata").len(), pre_len);
                    // Every await point was cancelled at once; later counts add nothing.
                    exercised_every_await = true;
                    break;
                }
                assert!(
                    t.pending.is_some(),
                    "cancelled after {polls} poll(s): the pending write was lost"
                );
                // The next append redoes the resolution and reports the torn write.
                let err = t.append("b".to_owned()).await;
                assert!(
                    matches!(
                        err,
                        Err(SessionError::TranscriptPartialWrite { seq: 1, .. })
                    ),
                    "{err:?}"
                );
                assert_eq!(t.append("b".to_owned()).await.expect("b"), 1);
                drop(t);
                assert_eq!(reopen_events(&path).await, vec!["a", "b"]);
            }
            assert!(
                exercised_every_await,
                "resolution never completed within 16 polls"
            );
        })
        .await
        .expect("test exceeded its time limit");
    }

    // -- Regression: cancelling a large append --

    #[tokio::test]
    async fn a_genuinely_cancelled_large_append_never_corrupts_the_log() {
        // This
        // drives a real `append` future by hand, watching the file grow on
        // disk, and drops the future the instant it sees bytes land: a
        // genuine cancellation, not a simulated one. Where it lands is
        // timing-dependent: mid-record (some 2 MiB chunks written) or after
        // the whole record landed (inside the trailing `flush`). Every
        // outcome is checked exactly against what is on disk afterwards.
        tokio::time::timeout(CANCEL_TEST_LIMIT, async {
            let (_dir, path) = temp_path();
            let mut t: JsonlTranscript<String> = JsonlTranscript::open(&path).await.expect("open");
            t.append("a".to_owned()).await.expect("a");
            let before = std::fs::metadata(&path).expect("metadata").len();

            let big = "x".repeat(5 * 1024 * 1024);
            let mut cancelled = false;
            let mut fully_completed = false;
            {
                let mut fut = Box::pin(t.append(big.clone()));
                loop {
                    match poll_once(&mut fut).await {
                        std::task::Poll::Ready(result) => {
                            // Finished before this loop saw it mid-flight: a
                            // legitimate, completed append.
                            assert_eq!(result.expect("a completed append must not error"), 1);
                            fully_completed = true;
                            break;
                        }
                        std::task::Poll::Pending => {
                            if std::fs::metadata(&path).expect("metadata").len() > before {
                                cancelled = true;
                                break; // drop `fut`: a genuine cancellation
                            }
                            tokio::time::sleep(std::time::Duration::from_micros(200)).await;
                        }
                    }
                }
            }
            assert!(cancelled != fully_completed);

            // The next append resolves the dropped one first. The big record
            // either landed whole (it keeps seq 1, "b" gets seq 2) or did not
            // (rolled back, reported loudly as a partial write of seq 1).
            let expected = match t.append("b".to_owned()).await {
                Ok(seq) => {
                    assert_eq!(seq, 2, "the big record claimed seq 1");
                    vec!["a".to_owned(), big, "b".to_owned()]
                }
                Err(SessionError::TranscriptPartialWrite { seq: 1, .. }) => {
                    assert!(cancelled, "a partial write with nothing cancelled is a bug");
                    assert_eq!(
                        std::fs::metadata(&path).expect("metadata").len(),
                        before,
                        "the partial record must be rolled back"
                    );
                    assert_eq!(t.append("b".to_owned()).await.expect("b"), 1);
                    vec!["a".to_owned(), "b".to_owned()]
                }
                Err(other) => panic!("unexpected error after cancellation: {other:?}"),
            };
            drop(t);
            assert_eq!(reopen_events(&path).await, expected);
        })
        .await
        .expect("test exceeded its time limit");
    }
}
