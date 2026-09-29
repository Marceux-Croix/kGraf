//! The live feed: one background thread that opens a project's index, reads a
//! snapshot, then tails the change log (decision 0007: triggers write, one poller
//! tails with a watermark, and that log is the only event source).
//!
//! Why a thread and a channel. The UI repaints only on change (0054), so it cannot be
//! the poller: an idle UI never runs. The thread owns its own read-only `GraphStore`
//! connection (a SQLite connection is not `Sync`), polls every [`FeedOptions::poll`],
//! and sends a message only when something changed, then calls `wake` (the shell
//! passes egui's `request_repaint`). An idle project costs a few indexed reads per
//! poll and no frames.
//!
//! Batches are resolved, not replayed: the log names the rows a window touched, and
//! [`kgraf_graph::snapshot::resolve`] reads their current state in one snapshot.
//! A reindexed file (its nodes deleted and reinserted) therefore arrives as "no
//! change" for the nodes that survived, and a batch applied twice is harmless.
//!
//! Bounded memory. At most one data message ([`FeedEvent::Loaded`] or
//! [`FeedEvent::Changes`]) is in flight: until the UI takes it, the feed keeps
//! coalescing touched ids into one set, and a set larger than
//! [`FeedOptions::reload_above`] is dropped in favor of one snapshot reload. An
//! occluded window that never drains therefore holds one message and one bounded
//! set, whatever the indexer does. Status messages ([`FeedEvent::Unindexed`],
//! [`FeedEvent::Failed`]) share a small bounded channel; when it is full a status
//! is dropped, and a condition that persists is reported again.
//!
//! Consistency. The feed does not resolve while an index pass is marked running
//! ([`kgraf_graph::index::INDEX_IN_PROGRESS`]): mid-pass, a rewritten file's
//! incoming cross-file call edges are briefly absent. A marker older than
//! [`FeedOptions::max_index_wait`] is taken as left by a crashed pass and ignored.
//!
//! A replaced index. `rm -rf .kgraf` and a reindex leave the old connection
//! reading the deleted file without error. Each poll compares the database file's
//! identity (device and inode on Unix) with the one opened, and a missing file or a
//! different identity, or a change log that fell below the watermark, reopens it.
//!
//! Lease expiry writes nothing, so the feed re-reads activity on its own when the
//! earliest live lease expires.
//!
//! State machine:
//! ```text
//!   Waiting --index readable and not mid-pass-> Loaded (snapshot sent) --> Tailing
//!   Waiting --no index / other schema / mid-pass--> Waiting (reported once, rechecked)
//!   Tailing --changes, nothing in flight--------> Tailing (batch sent)
//!   Tailing --changes, message in flight--------> Tailing (coalesced)
//!   Tailing --index pass marked running---------> Tailing (held until it ends)
//!   Tailing --lease expiry time reached---------> Tailing (activity re-read)
//!   Tailing --file replaced / log regressed-----> Waiting (reopen)
//!   Tailing --touched set above reload_above----> Loaded (a new snapshot replaces the scene)
//!   Tailing --request_reload--------------------> Loaded (the view could not apply a batch)
//!   Tailing --read error------------------------> Tailing (reported; touched ids kept)
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kgraf_graph::snapshot::{
    GraphSnapshot, ProjectIndex, Resolved, graph_db_path, index_pass_running, now_ms,
    open_existing, read_snapshot, resolve,
};
use kgraf_graph::{EventTail, GraphStore};

/// One message from the feed.
#[derive(Debug)]
pub enum FeedEvent {
    /// The directory has no index this build can read yet. The feed keeps checking
    /// and sends [`FeedEvent::Loaded`] once one appears.
    Unindexed { reason: String },
    /// A full snapshot: build (or rebuild) the scene from it.
    Loaded {
        snapshot: Box<GraphSnapshot>,
        /// Time spent reading it from SQLite.
        read: Duration,
    },
    /// The current state of the rows the change-log windows since the last message
    /// touched.
    Changes {
        batch: Box<Resolved>,
        /// Change-log rows those windows held.
        rows: usize,
        watermark: i64,
    },
    /// A read failed. The feed keeps running; the message is for the view to show.
    Failed(String),
}

impl FeedEvent {
    fn is_data(&self) -> bool {
        matches!(self, Self::Loaded { .. } | Self::Changes { .. })
    }
}

/// Feed tuning.
#[derive(Debug, Clone, Copy)]
pub struct FeedOptions {
    /// Change-log poll interval. 250 ms keeps an agent's edit visible within a
    /// quarter second at the cost of a few indexed reads per second.
    pub poll: Duration,
    /// How often to look for an index that is missing.
    pub recheck: Duration,
    /// Change-log rows read per query.
    pub batch_rows: usize,
    /// A touched set (nodes plus edges) larger than this reloads a snapshot instead:
    /// a first index of a large tree applied slot by slot would be slower than a
    /// rebuild, and the set is the feed's only growing state.
    pub reload_above: usize,
    /// An index-pass marker older than this is treated as left by a crashed pass.
    /// A full index of 133,824 nodes took 38 s (2026-09-26), so 120 s.
    pub max_index_wait: Duration,
}

impl Default for FeedOptions {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(250),
            recheck: Duration::from_secs(2),
            batch_rows: 10_000,
            reload_above: 20_000,
            max_index_wait: Duration::from_secs(120),
        }
    }
}

/// Status messages that may wait in the channel besides the one data message.
const STATUS_SLOTS: usize = 3;

/// What the UI side and the feed thread share.
#[derive(Default)]
struct Flags {
    stop: AtomicBool,
    /// Send a fresh snapshot at the next poll (after a batch failed to apply).
    reload: AtomicBool,
    /// Data messages sent and not yet received (0 or 1).
    data_in_flight: AtomicUsize,
}

/// The feed's handle. Dropping it stops and joins the thread.
pub struct Feed {
    rx: Receiver<FeedEvent>,
    flags: Arc<Flags>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feed").finish_non_exhaustive()
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.flags.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            // A panicked feed thread has nothing left to clean up.
            let _ = t.join();
        }
    }
}

impl Feed {
    /// Starts the feed for the project at `root`. `wake` is called after each message.
    pub fn spawn(
        root: PathBuf,
        options: FeedOptions,
        wake: impl Fn() + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = sync_channel(1 + STATUS_SLOTS);
        let flags = Arc::new(Flags::default());
        let theirs = Arc::clone(&flags);
        let thread = std::thread::Builder::new()
            .name("kgraf-galaxy-feed".into())
            .spawn(move || {
                Worker {
                    root: &root,
                    opts: options,
                    tx: &tx,
                    flags: &theirs,
                    wake: &wake,
                }
                .run();
            })?;
        Ok(Self {
            rx,
            flags,
            thread: Some(thread),
        })
    }

    fn took(&self, event: FeedEvent) -> FeedEvent {
        if event.is_data() {
            self.flags.data_in_flight.fetch_sub(1, Ordering::SeqCst);
            // The feed may be holding coalesced changes for this moment.
            if let Some(t) = &self.thread {
                t.thread().unpark();
            }
        }
        event
    }

    /// Data messages ([`FeedEvent::Loaded`], [`FeedEvent::Changes`]) sent and not yet
    /// received: 0 or 1. While one is waiting the feed only coalesces, so the count
    /// cannot change until it is taken; taking it wakes the feed, which may then send
    /// the coalesced batch at once.
    pub fn data_in_flight(&self) -> usize {
        self.flags.data_in_flight.load(Ordering::SeqCst)
    }

    /// The next message, if one is waiting.
    pub fn try_recv(&self) -> Option<FeedEvent> {
        self.rx.try_recv().ok().map(|e| self.took(e))
    }

    /// The next message, waiting up to `timeout`. `None` on timeout or a stopped feed.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<FeedEvent> {
        self.rx.recv_timeout(timeout).ok().map(|e| self.took(e))
    }

    /// Asks for a fresh snapshot ([`FeedEvent::Loaded`]) at the next poll.
    pub fn request_reload(&self) {
        self.flags.reload.store(true, Ordering::SeqCst);
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
    }
}

/// Rows the drained windows touched, kept until resolved and sent.
#[derive(Default)]
struct Touched {
    nodes: BTreeSet<String>,
    edges: BTreeSet<String>,
    activity: bool,
    rows: usize,
}

impl Touched {
    fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty() && !self.activity
    }

    fn len(&self) -> usize {
        self.nodes.len() + self.edges.len()
    }
}

/// The identity of the database file: device and inode, so a file deleted and
/// recreated at the same path is a different file.
#[cfg(unix)]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// Elsewhere: the creation time, which also differs for a recreated file.
#[cfg(not(unix))]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    let created = std::fs::metadata(path).ok()?.created().ok()?;
    let d = created.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((d.as_secs(), u64::from(d.subsec_nanos())))
}

struct Tailing {
    store: GraphStore,
    identity: Option<(u64, u64)>,
    tail: EventTail,
    touched: Touched,
    /// Replace the scene with a snapshot as soon as nothing is in flight.
    reload: bool,
    /// When to re-read activity because a lease expires.
    next_expiry_ms: Option<i64>,
}

struct Worker<'a> {
    root: &'a Path,
    opts: FeedOptions,
    tx: &'a SyncSender<FeedEvent>,
    flags: &'a Flags,
    wake: &'a dyn Fn(),
}

/// Whether to keep going.
type Go = bool;

impl Worker<'_> {
    fn stopped(&self) -> bool {
        self.flags.stop.load(Ordering::SeqCst)
    }

    fn sleep(&self, d: Duration) {
        if !self.stopped() {
            std::thread::park_timeout(d);
        }
    }

    fn data_free(&self) -> bool {
        self.flags.data_in_flight.load(Ordering::SeqCst) == 0
    }

    /// Sends a message. Data is counted in flight; status is dropped when the
    /// channel is full. `false` when the UI side is gone.
    fn send(&self, event: FeedEvent) -> Go {
        let data = event.is_data();
        if data {
            self.flags.data_in_flight.fetch_add(1, Ordering::SeqCst);
        }
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                if data {
                    // Unreachable while one data message is in flight at a time and
                    // status leaves room for it; undo the count rather than lose track.
                    self.flags.data_in_flight.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
        (self.wake)();
        true
    }

    /// Whether an index pass marker started at `start` is recent enough to wait on.
    fn fresh_pass(&self, start: Option<i64>) -> bool {
        let max = self.opts.max_index_wait.as_millis() as i64;
        start.is_some_and(|s| now_ms() - s < max)
    }

    fn run(&self) {
        let mut tailing: Option<Tailing> = None;
        let mut last_waiting: Option<String> = None;
        while !self.stopped() {
            let Some(t) = tailing.as_mut() else {
                match self.open(&mut last_waiting) {
                    Some(Ok(t)) => {
                        tailing = Some(t);
                        continue;
                    }
                    Some(Err(())) => return,
                    None => {
                        self.sleep(self.opts.recheck);
                        continue;
                    }
                }
            };
            match self.step(t) {
                Ok(Step::Continue) => {}
                Ok(Step::Reopen) => {
                    tailing = None;
                    continue;
                }
                Ok(Step::Stop) => return,
                Err(e) => {
                    if !self.send(FeedEvent::Failed(format!("tailing the change log: {e}"))) {
                        return;
                    }
                }
            }
            self.sleep(self.opts.poll);
        }
    }

    /// Opens the index and sends its first snapshot. `None` to wait and retry,
    /// `Some(Err)` when the UI side is gone.
    fn open(&self, last_waiting: &mut Option<String>) -> Option<Result<Tailing, ()>> {
        let db_path = graph_db_path(self.root);
        let identity = file_identity(&db_path);
        let t0 = Instant::now();
        let waiting = match open_existing(self.root) {
            Ok(ProjectIndex::Ready(store)) => match read_snapshot(&store) {
                Ok(snapshot) if self.fresh_pass(snapshot.index_in_progress) => {
                    Some("an index pass is running; the view loads when it finishes".into())
                }
                Ok(snapshot) => {
                    if !self.data_free() {
                        return None;
                    }
                    let tail = EventTail::resume_from(snapshot.watermark);
                    let next_expiry_ms = snapshot.next_expiry_ms;
                    self.flags.reload.store(false, Ordering::SeqCst);
                    if !self.send(FeedEvent::Loaded {
                        snapshot: Box::new(snapshot),
                        read: t0.elapsed(),
                    }) {
                        return Some(Err(()));
                    }
                    *last_waiting = None;
                    return Some(Ok(Tailing {
                        store,
                        identity,
                        tail,
                        touched: Touched::default(),
                        reload: false,
                        next_expiry_ms,
                    }));
                }
                Err(e) => {
                    if !self.send(FeedEvent::Failed(format!("reading the index: {e}"))) {
                        return Some(Err(()));
                    }
                    None
                }
            },
            Ok(ProjectIndex::Missing { db_path }) => Some(format!(
                "not indexed: no {} (run `kgraf index` in {})",
                db_path.display(),
                self.root.display()
            )),
            Ok(ProjectIndex::OtherSchema {
                db_path,
                found,
                expected,
            }) => Some(format!(
                "{} is schema {} and this build reads schema {expected}; \
                 run `kgraf index` to rebuild it",
                db_path.display(),
                found.map_or("unknown".to_owned(), |v| v.to_string()),
            )),
            Err(e) => {
                if !self.send(FeedEvent::Failed(format!("opening the index: {e}"))) {
                    return Some(Err(()));
                }
                None
            }
        };
        // Reported once per distinct reason, then rechecked quietly.
        if let Some(reason) = waiting
            && last_waiting.as_deref() != Some(reason.as_str())
        {
            if !self.send(FeedEvent::Unindexed {
                reason: reason.clone(),
            }) {
                return Some(Err(()));
            }
            *last_waiting = Some(reason);
        }
        None
    }

    fn step(&self, t: &mut Tailing) -> kgraf_graph::Result<Step> {
        // The same file, and a log that did not restart below us.
        let identity = file_identity(&graph_db_path(self.root));
        if identity.is_none() || identity != t.identity {
            return Ok(Step::Reopen);
        }
        if t.store.change_watermark()? < t.tail.watermark() {
            return Ok(Step::Reopen);
        }
        if self.flags.reload.swap(false, Ordering::SeqCst) {
            t.reload = true;
        }
        if t.reload {
            return self.reload(t);
        }

        // Drain everything new into the touched set.
        loop {
            let batch = t.tail.next_batch(&t.store, self.opts.batch_rows)?;
            if batch.is_empty() {
                break;
            }
            for change in &batch {
                match change.table.as_str() {
                    "nodes" | "layout" => {
                        t.touched.nodes.insert(change.row_id.clone());
                    }
                    "edges" => {
                        t.touched.edges.insert(change.row_id.clone());
                    }
                    "leases" | "node_leases" => t.touched.activity = true,
                    // A file's rows arrive as node rows; other tables are not drawn.
                    _ => {}
                }
            }
            t.touched.rows += batch.len();
            if t.touched.len() > self.opts.reload_above {
                t.touched = Touched::default();
                t.reload = true;
                return self.reload(t);
            }
        }
        if t.next_expiry_ms.is_some_and(|e| e <= now_ms()) {
            t.touched.activity = true;
            t.next_expiry_ms = None;
        }
        if t.touched.is_empty() {
            // Rows from tables the galaxy does not draw.
            t.touched.rows = 0;
            return Ok(Step::Continue);
        }
        if !self.data_free() || self.fresh_pass(index_pass_running(&t.store)?) {
            // Coalesce: keep the set until the UI took the last message and no index
            // pass is mid-way.
            return Ok(Step::Continue);
        }
        let nodes: Vec<String> = t.touched.nodes.iter().cloned().collect();
        let edges: Vec<String> = t.touched.edges.iter().cloned().collect();
        // On error the touched ids stay pending and are resolved on the next poll.
        let resolved = resolve(&t.store, &nodes, &edges, t.touched.activity)?;
        if self.fresh_pass(resolved.index_in_progress) {
            // A pass began between the check and the read.
            return Ok(Step::Continue);
        }
        if resolved.activity.is_some() {
            t.next_expiry_ms = resolved.next_expiry_ms;
        }
        let rows = std::mem::take(&mut t.touched).rows;
        let go = self.send(FeedEvent::Changes {
            batch: Box::new(resolved),
            rows,
            watermark: t.tail.watermark(),
        });
        Ok(if go { Step::Continue } else { Step::Stop })
    }

    fn reload(&self, t: &mut Tailing) -> kgraf_graph::Result<Step> {
        if !self.data_free() {
            // Nothing is drained meanwhile: the snapshot will include it.
            return Ok(Step::Continue);
        }
        let t0 = Instant::now();
        let snapshot = read_snapshot(&t.store)?;
        if self.fresh_pass(snapshot.index_in_progress) {
            return Ok(Step::Continue);
        }
        t.tail = EventTail::resume_from(snapshot.watermark);
        t.touched = Touched::default();
        t.reload = false;
        t.next_expiry_ms = snapshot.next_expiry_ms;
        let go = self.send(FeedEvent::Loaded {
            snapshot: Box::new(snapshot),
            read: t0.elapsed(),
        });
        Ok(if go { Step::Continue } else { Step::Stop })
    }
}

enum Step {
    Continue,
    Reopen,
    Stop,
}
