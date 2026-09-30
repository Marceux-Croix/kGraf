//! The writer thread and the handle callers hold. Recording
//! must never block a hot path and must never panic.
//!
//! `record` is a non-blocking `try_send` on a bounded channel to one
//! background thread that owns the file. A full channel drops the event
//! (counted by [`dropped_count`] and reported back into the file itself as
//! soon as a write succeeds again) rather than blocking the caller or growing
//! without bound. This is the same shape `tracing`'s non-blocking writer
//! uses, for the same reason: a diagnostics or logging path must never become
//! the bottleneck of the thing it is observing.
//!
//! **Flushing at exit.** A `static` value's `Drop` never
//! runs at normal process exit (statics are not destructed), so a writer kept
//! only in a `static` never got the chance to send its buffered `Msg::Stop`
//! or wait for the thread to drain: of 20 short-lived runs that each recorded
//! one error, only 18 lines made it to disk. [`shutdown`] fixes this by
//! *taking* the writer out of the global slot (an `Option` inside a `Mutex`,
//! not the `DiagWriter` itself sitting directly in a `OnceLock`) and
//! consuming it: [`DiagWriter::shutdown`] explicitly drops the channel's
//! sending half, which lets the writer thread's `for msg in rx` loop drain
//! every message still queued and then end on its own, and only then joins
//! the thread (bounded by a timeout, so a stuck writer cannot hang the
//! caller's exit forever). [`guard`] wraps this in an RAII type a binary's
//! `main` holds so `shutdown` runs on every return from `main`, `?`-early
//! return included, and even while a panic unwinds through it.

use crate::event::{DiagEvent, Record};
use crate::store;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

/// Channel depth. Generous relative to the event rate this crate expects
/// (human-scale actions and errors, not a per-token or per-frame stream): a
/// burst this size draining slower than it fills is itself worth noticing
/// (see [`DiagWriter::dropped_count`]), which is why a full channel is
/// counted rather than silently ignored.
const CHANNEL_CAPACITY: usize = 4096;

enum Msg {
    Record(Record),
    /// Round-trips through the writer thread so a test can wait for every
    /// prior `Record` to be on disk without sleeping.
    Flush(mpsc::SyncSender<()>),
}

/// A running writer: the sending half of the channel, the thread's handle so
/// it can be joined on shutdown, and this writer's own drop counter.
///
/// `dropped` is **per-writer**, not one process-wide `static` (an earlier
/// version used a bare `static AtomicU64`): a test process routinely runs
/// several independent `DiagWriter`s in parallel threads, each writing to its
/// own temp file, and a single shared counter let one test's dropped events
/// get reported into a *different* test's file, whichever writer happened to
/// process a message next. In production there is exactly one `DiagWriter`
/// (the one behind [`init`]/[`record`]), so this is the same behavior there;
/// it is only observable in a test that starts more than one.
///
/// `tx` is an `Option`: dropping it explicitly, before
/// joining, is what lets the writer's `for msg in rx` loop see the channel
/// disconnect and end, rather than the join waiting forever for a thread
/// still blocked on `rx.recv()` while this struct (and the sender inside it)
/// is itself still alive on the stack of the thread that is waiting on the
/// join.
pub struct DiagWriter {
    tx: Option<SyncSender<Msg>>,
    join: Option<JoinHandle<()>>,
    dropped: Arc<AtomicU64>,
}

impl DiagWriter {
    /// Start the writer thread for `path`, capped at `cap_bytes`.
    pub fn start(path: PathBuf, cap_bytes: u64) -> Self {
        Self::start_with_channel_capacity(path, cap_bytes, CHANNEL_CAPACITY)
    }

    /// As [`Self::start`], with an explicit channel depth instead of
    /// [`CHANNEL_CAPACITY`]. `pub(crate)`, for a test that needs to force a
    /// full channel deterministically (a real writer thread reliably drains
    /// even a `CHANNEL_CAPACITY`-deep burst before a tight producer loop can
    /// fill it, since a `try_send` is far cheaper than the disk write it
    /// waits behind; a channel of depth 1 cannot keep up with any producer
    /// faster than one file write per call, which every test producer is).
    pub(crate) fn start_with_channel_capacity(
        path: PathBuf,
        cap_bytes: u64,
        channel_capacity: usize,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel(channel_capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let join = std::thread::Builder::new()
            .name("kgraf-diag-writer".into())
            .spawn({
                let dropped = Arc::clone(&dropped);
                move || run(path, cap_bytes, rx, dropped)
            })
            .ok(); // A thread that fails to spawn leaves `join` `None`; `record`
        // still only ever `try_send`s, so the caller never blocks or panics
        // even though nothing is now draining the channel (it fills once,
        // to `CHANNEL_CAPACITY`, then every further record is silently
        // dropped, which is the same behavior as a slow writer thread).
        Self {
            tx: Some(tx),
            join,
            dropped,
        }
    }

    /// A writer with no consumer at all: `rx` is dropped immediately, so
    /// every `record` after this point fails deterministically (the channel
    /// is disconnected, not merely full) and is counted. `pub(crate)`, for a
    /// test of the drop-counting path itself, independent of any real writer
    /// thread's scheduling (a live thread's actual drain speed relative to a
    /// tight producer loop is not something a test can force reliably; see
    /// `start_with_channel_capacity`'s doc for the flakiness that caused).
    #[cfg(test)]
    pub(crate) fn started_without_drain(channel_capacity: usize) -> Self {
        let (tx, rx) = mpsc::sync_channel(channel_capacity);
        drop(rx);
        Self {
            tx: Some(tx),
            join: None,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Enqueue `event` with the current time. Never blocks, never panics: a
    /// full or disconnected channel just drops the event and counts it
    /// (`dropped_count`).
    pub fn record(&self, event: DiagEvent) {
        let Some(tx) = &self.tx else { return };
        if tx.try_send(Msg::Record(Record::now(event))).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Events dropped by this writer's full channel so far.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Block until every `Record` sent before this call has been written (or
    /// dropped by a full channel before reaching the thread). Meant for tests
    /// (`kgraf-diag`'s own, and an integration test in another crate that
    /// wants to wait for a write without a sleep loop); a real caller never
    /// waits on the writer.
    pub fn flush_for_test(&self) {
        let Some(tx) = &self.tx else { return };
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if tx.send(Msg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }

    /// Stop the writer and wait for it to drain and exit, bounded by
    /// `timeout`. Dropping `tx` first (rather than sending a `Stop` message)
    /// is deliberate: a message can be lost if the channel is full, but
    /// dropping the sender itself cannot fail, and the receiver's `for msg in
    /// rx` loop already ends once the channel is both empty and disconnected,
    /// which drains every message already enqueued before it ends. If the
    /// thread still has not finished after `timeout` (a write stuck on a
    /// wedged disk, say), this gives up rather than hang the caller's exit
    /// forever; the thread is left to finish or not on its own.
    pub fn shutdown(mut self, timeout: Duration) {
        self.tx.take();
        if let Some(join) = self.join.take() {
            join_with_timeout(join, timeout);
        }
    }
}

/// Join `handle`, but give up waiting after `timeout`. `std::thread::JoinHandle`
/// has no timed join, so this proxies through a helper thread and a
/// `recv_timeout`; if the timeout elapses, the helper (and the handle it is
/// joining) is abandoned rather than blocking the caller, which is safe here
/// because the caller is a process about to exit, not code that needs the
/// thread's resources back.
fn join_with_timeout(handle: JoinHandle<()>, timeout: Duration) {
    let (done_tx, done_rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("kgraf-diag-shutdown-join".into())
        .spawn(move || {
            let _ = handle.join();
            let _ = done_tx.send(());
        });
    let _ = done_rx.recv_timeout(timeout);
}

fn run(path: PathBuf, cap_bytes: u64, rx: Receiver<Msg>, dropped: Arc<AtomicU64>) {
    for msg in rx {
        match msg {
            Msg::Record(record) => {
                write_one(&path, &record, cap_bytes);
                report_drops_if_any(&path, cap_bytes, &dropped);
            }
            Msg::Flush(ack) => {
                let _ = ack.send(());
            }
        }
    }
}

fn write_one(path: &std::path::Path, record: &Record, cap_bytes: u64) {
    // A write failure here (disk full, permissions) is exactly the kind of
    // thing this crate exists to surface, but it cannot recurse into
    // recording itself; `eprintln!` is the floor everything else stands on
    // (kGraf fails loud, stopped short of a panic: a lost
    // diagnostics line must never take the process down with it).
    if let Err(e) = store::append(path, record, cap_bytes) {
        eprintln!("kgraf-diag: {e}");
    }
}

/// After a successful write, fold in a `Dropped` record for any events a full
/// channel has lost since the last time this ran, so the file itself shows
/// "N were dropped here" instead of a silent gap. Reads-then-resets
/// `dropped`, so a drop is reported at most once and never double-counted if
/// two writes race (the writer thread is the only place this runs, so there
/// is no real race, but `swap` is used anyway to keep that invariant
/// unnecessary to maintain by hand).
fn report_drops_if_any(path: &std::path::Path, cap_bytes: u64, dropped: &AtomicU64) {
    let n = dropped.swap(0, Ordering::Relaxed);
    if n > 0 {
        write_one(
            path,
            &Record::now(DiagEvent::Dropped { count: n }),
            cap_bytes,
        );
    }
}

static GLOBAL: OnceLock<Mutex<Option<DiagWriter>>> = OnceLock::new();

fn global() -> &'static Mutex<Option<DiagWriter>> {
    GLOBAL.get_or_init(|| Mutex::new(None))
}

/// Start the process-wide diagnostics writer. Call once, from the binary's
/// entry point (`kgraf-cli`'s `main`, `kgraf-shell`'s startup), before any
/// other crate calls `record`. A second call is a no-op: the first writer
/// (and the path and cap it was given) wins, matching `tracing`'s
/// set-global-default-once precedent already used in this workspace
/// (`kgraf-cli/src/logging.rs`).
pub fn init(path: PathBuf, cap_bytes: u64) {
    let mut guard = global().lock().unwrap_or_else(|p| p.into_inner());
    if guard.is_none() {
        *guard = Some(DiagWriter::start(path, cap_bytes));
    }
}

/// Record `event` on the process-wide writer. A no-op before `init` (tests,
/// and any crate that links this one without a binary having called `init`,
/// such as a library's own unit tests) and after [`shutdown`]: recording is
/// best-effort instrumentation, never a dependency a caller must satisfy
/// first.
pub fn record(event: DiagEvent) {
    let guard = global().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(writer) = guard.as_ref() {
        writer.record(event);
    }
}

/// Events dropped by a full channel so far, on the process-wide writer. `0`
/// before `init`, after `shutdown`, or if nothing has ever been dropped.
pub fn dropped_count() -> u64 {
    let guard = global().lock().unwrap_or_else(|p| p.into_inner());
    guard.as_ref().map(DiagWriter::dropped_count).unwrap_or(0)
}

/// Block until every event recorded before this call is on disk (or was
/// dropped by a full channel before reaching the writer thread), on the
/// process-wide writer. Deterministic, not a timeout race: the round trip
/// through the writer's own channel only completes once every `Record`
/// enqueued ahead of it has actually been written, since the channel is
/// FIFO. Returns `false` (nothing to wait for) before `init` or after
/// `shutdown`.
///
/// For tests only (fix, follow-up to decision 0069: `diag_worker_spawned.rs`
/// raced a fixed 2 s `shutdown` timeout against the writer thread under
/// load). `kgraf-diag`'s own tests reach `DiagWriter::flush_for_test`
/// directly since they hold the writer themselves; a caller in another
/// crate, which only ever sees the process-wide writer through
/// `init`/`record`, needs this wrapper to reach the same thing without also
/// tearing the writer down (which `shutdown` does, and a test may still want
/// to record after flushing).
pub fn flush_for_test() -> bool {
    let guard = global().lock().unwrap_or_else(|p| p.into_inner());
    match guard.as_ref() {
        Some(writer) => {
            writer.flush_for_test();
            true
        }
        None => false,
    }
}

/// Stop the process-wide writer, draining whatever is still queued, bounded
/// by `timeout`. Call this on every exit path of a binary that called
/// [`init`] (normally through [`guard`], not by hand); a `static`'s `Drop`
/// never runs at process exit, so without this, anything recorded in the
/// last `CHANNEL_CAPACITY` messages before the process ends can be lost even
/// though `record` itself never blocked or failed. Idempotent: a second call
/// (or a call when `init` was never called) is a no-op.
pub fn shutdown(timeout: Duration) {
    let taken = global().lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(writer) = taken {
        writer.shutdown(timeout);
    }
}

/// An RAII handle that calls [`shutdown`] when dropped. A binary's `main`
/// holds one (`let _diag = kgraf_diag::guard(TIMEOUT);`, immediately after
/// `init`) so every return from `main` flushes the writer: a normal return,
/// an early `?`/`bail!`, and a panic unwinding through `main` (assuming the
/// default `panic = "unwind"`; this workspace sets no `panic = "abort"`
/// profile). This is the one call site both `kgraf-cli` and `kgraf-shell`
/// need, instead of each binary having to reason about its own exit paths.
#[must_use = "dropping this immediately defeats the point of holding it: keep it bound in `main` \
              for its whole body"]
pub struct ShutdownGuard(Duration);

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        shutdown(self.0);
    }
}

/// Build a [`ShutdownGuard`] with the given join timeout.
pub fn guard(timeout: Duration) -> ShutdownGuard {
    ShutdownGuard(timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_before_init_does_not_panic() {
        // A fresh process would have no global writer yet; this test only
        // demonstrates that the call itself is safe when nothing is set,
        // exercising the fallback path `record` takes in that case. It does
        // not call `init` (which is a true once-per-process global) so it
        // stays independent of test execution order.
        record(DiagEvent::launch());
    }

    #[test]
    fn writer_appends_and_flush_waits_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diag.jsonl");
        let writer = DiagWriter::start(path.clone(), store::DEFAULT_CAP_BYTES);
        writer.record(DiagEvent::launch());
        writer.record(DiagEvent::FeatureUsed {
            feature: "index".into(),
        });
        writer.flush_for_test();
        let records = store::read_all(&path).unwrap();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn a_full_channel_drops_rather_than_blocks_and_counts_the_drop() {
        // No consumer at all (see `started_without_drain`'s doc for why not a
        // real writer thread): deterministic, unlike racing a live thread's
        // drain speed against a producer loop.
        let writer = DiagWriter::started_without_drain(4);
        for _ in 0..100 {
            writer.record(DiagEvent::launch());
        }
        assert!(
            writer.dropped_count() > 0,
            "a channel with no consumer must drop every event once it fills"
        );
    }

    /// A dropped event is not silent forever: once the writer catches up and
    /// writes again, a `Dropped{count}` record lands in the file.
    #[test]
    fn a_drop_is_reported_in_the_file_on_the_next_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diag.jsonl");
        let writer = DiagWriter::start(path.clone(), store::DEFAULT_CAP_BYTES);
        // Simulates a drop directly (see `a_full_channel_drops_...` for why a
        // real full channel is not forced here): this test is about what
        // happens to a nonzero drop count on the *next* write, not about
        // reproducing the race that produces one.
        writer.dropped.fetch_add(3, Ordering::Relaxed);
        writer.record(DiagEvent::FeatureUsed {
            feature: "index".into(),
        });
        writer.flush_for_test();
        let records = store::read_all(&path).unwrap();
        assert!(
            records
                .iter()
                .any(|r| matches!(r.event, DiagEvent::Dropped { count } if count == 3)),
            "expected a Dropped record among {records:?}"
        );
    }

    /// The bug this fixes: a writer with a full channel used to hang on
    /// shutdown, because the sender was dropped only as an ordinary struct
    /// field, after the blocking `join()` call had already started waiting
    /// on a thread that could never see the channel disconnect while that
    /// same sender was still alive on this call stack.
    #[test]
    fn shutdown_does_not_hang_when_the_channel_is_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diag.jsonl");
        let writer =
            DiagWriter::start_with_channel_capacity(path.clone(), store::DEFAULT_CAP_BYTES, 1);
        for _ in 0..256 {
            writer.record(DiagEvent::launch());
        }
        // If shutdown hangs, this test itself hangs and the suite times out;
        // there is no clean way to assert "did not hang" other than reaching
        // this point at all.
        writer.shutdown(Duration::from_secs(5));
        let records = store::read_all(&path).unwrap();
        assert!(!records.is_empty());
    }

    // The process-wide `init`/`record`/`shutdown` trio is deliberately not
    // exercised end-to-end here: this test binary runs every `#[test]` in
    // parallel threads of one process, and those functions share one
    // `static`, so two tests calling `init` (or `shutdown`, which un-sets it
    // for the next `init` to reinitialize) would race each other. The real
    // regression this crate's global state is for (records lost when a
    // `static`'s `Drop` never runs at process exit) is instead covered by a
    // true subprocess test: `tests/exit_flush.rs`, one process per assertion.
}
