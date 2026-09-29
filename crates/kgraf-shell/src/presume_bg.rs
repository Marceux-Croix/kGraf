//! Presumed work nodes at spin-up, in the desktop shell (decision 0064).
//!
//! Opening a project (the `--project` flag, the switcher's "Add project", and
//! restoring the saved workspace) runs the same read `kgraf index` runs:
//! [`kgraf_interpret::presumed::presume_spinup`], the one function every caller
//! shares (no parser, no sync loop, no retirement rule is duplicated here). It:
//!
//! - runs once, the first time [`PresumeWatcher::tick`] is called for a project
//!   (its signature starts at `None`, which never equals a real one, even an
//!   empty map);
//! - runs again whenever a discovered roadmap document's modification time,
//!   size or (on Unix) inode changes, or the discovered set itself changes;
//! - **never creates or rebuilds a project's graph.** A folder with no
//!   `.kgraf/graph.db` has never been indexed; this module never writes to it
//!   (no `.kgraf`, no `graph.db`), and says so. An existing `graph.db` is
//!   opened with [`kgraf_graph::GraphStore::open_existing_for_facts`], which
//!   refuses a schema version other than this build's rather than rebuilding
//!   it (that would drop the derived tables - `nodes`, `files`, `edges` - a
//!   viewer must never do; only `kgraf index` may rebuild). A refusal is an
//!   error notice, never a silent skip.
//! - never runs twice for one project's graph at once, even across two
//!   [`PresumeWatcher`]s over the same root (a quick close and reopen): the
//!   background thread takes a process-wide, per-root lock
//!   ([`root_lock`]) before it opens the graph, and holds it for the read;
//! - retries a failed read with backoff ([`BACKOFF`]) regardless of whether
//!   the roadmap changed again meanwhile - a transient failure (`kgraf index`
//!   holding the write lock, "database is locked") is not the roadmap's fault,
//!   and must not be latched forever; a retry that fails the *same* way again
//!   says nothing more (its text is remembered), so a failure that lasts is
//!   said once, not on every backoff step - and a fix is said once too, the
//!   next time a read succeeds after one had failed;
//! - runs off the UI thread: not only the read, parse and graph write, but
//!   also the `stat` of each discovered document, so a project on a network
//!   mount can never stall a frame (a local disk's `stat` is cheap enough that
//!   `crate::layout::FileCache` still makes them, more of them, right on the UI
//!   thread for `.kgraf/run.json`; roadmap discovery walks two directories, so
//!   its cost is less predictable and it is moved off the UI thread instead).
//!
//! **Spends no token.** [`kgraf_interpret::presumed::presume_spinup`] takes only
//! the graph: no `Asker`, no ledger, no provider handle reaches it, so no call
//! is even constructible through it (every token is attributed through
//! one accounting path; a path that cannot reach a provider needs none).
//!
//! **Fail loud.** A fresh, never-indexed project is not an error: its notice
//! says to run `kgraf index` first. A project already indexed but with no
//! roadmap document is not an error either: its notice names what was looked
//! for and offers `kgraf orchestrate --corpus` as the way to get a plan
//! without one, never run automatically, since that call is paid (a call is deliberate and
//! accounted, never implied by opening a folder). A document discovery finds but cannot read, and a
//! schema mismatch, are both error lines, never a silent skip. All of it goes to the project's one
//! text surface (the box, `orchestrator_box::Line`); the graph's own change-log rows carry
//! everything that did write (0063's activity card already shows them), so nothing here duplicates
//! that as a second list
//!.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use kgraf_graph::GraphStore;
use kgraf_interpret::presumed::{self, SpinupReport};

/// How often a project's discovered roadmap documents are `stat`-ed again, once
/// a signature is known (same cadence [`crate::layout::FILE_RECHECK`] uses).
pub const RECHECK: Duration = Duration::from_secs(1);

/// How long a failed read backs off before it is retried, regardless of
/// whether the roadmap changed again meanwhile: a transient failure (a lock
/// held by a concurrent `kgraf index`) is not a content change, so the normal
/// signature check would never see it and would never retry.
/// Escalates on repeated failure, then holds at its last step.
const BACKOFF: &[Duration] = &[
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
];

/// A discovered document's identity: its modification time, size and (Unix
/// only) inode, `None` when it cannot be `stat`-ed (itself a change, worth a
/// fresh presume that reports it). mtime alone missed a rewrite that happened
/// to land on the same second on a coarse file system and kept the same size
/// by coincidence; size and inode narrow that without claiming to
/// close it completely - a content-identical rewrite is still invisible, as it
/// is to `kgraf index`'s own change detection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    inode: u64,
}

fn stamp_of(path: &Path) -> Stamp {
    let Ok(meta) = std::fs::metadata(path) else {
        return Stamp::default();
    };
    Stamp {
        modified: meta.modified().ok(),
        len: meta.len(),
        #[cfg(unix)]
        inode: {
            use std::os::unix::fs::MetadataExt;
            meta.ino()
        },
    }
}

type Signature = BTreeMap<String, Stamp>;

fn signature(root: &Path) -> Signature {
    presumed::discover(root)
        .into_iter()
        .map(|p| {
            let stamp = stamp_of(&root.join(&p));
            (p, stamp)
        })
        .collect()
}

/// What [`PresumeWatcher`] found to say, once, on the project's box: an error
/// line for a document discovery found but could not read, a notice otherwise
/// (nothing to show, or nothing to show beyond what the graph now carries).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresumeNotice {
    pub text: String,
    pub is_error: bool,
}

/// Said when the project has no `.kgraf/graph.db` at all: this module never
/// creates one, so there is nothing to presume into yet.
const NOT_INDEXED: &str =
    "not indexed; presumed work waits for `kgraf index` (or opening it from a corpus reading)";

/// Said once, the first read after a run of failures finally succeeds
/// (post-merge: a repeated failure was otherwise re-shown on every
/// forced retry, about 30 times an hour at the 120 s backoff step).
const RECOVERED: &str = "presumed work: recovered; the read that had been failing now succeeded";

/// The words for one spin-up read, for the project's box. A clean read of a
/// roadmap with nothing to say (no failures, a roadmap found) says nothing: its
/// new, updated and gone nodes are the graph's own change-log rows, already
/// shown by the activity card (0063); repeating them here would be a second
/// place to look. Only what would otherwise be silent is said: no
/// roadmap at all, and any document that could not be read.
pub fn notice_of(report: &SpinupReport) -> Option<PresumeNotice> {
    if !report.failed.is_empty() {
        let each: Vec<String> = report
            .failed
            .iter()
            .map(|(s, e)| format!("{s}: {e}"))
            .collect();
        return Some(PresumeNotice {
            text: format!(
                "presumed work: {} roadmap document(s) could not be read: {}",
                report.failed.len(),
                each.join("; ")
            ),
            is_error: true,
        });
    }
    if let Some(looked) = &report.no_roadmap {
        return Some(PresumeNotice {
            text: format!(
                "no roadmap found (looked for {looked}); `kgraf orchestrate --corpus` reads the \
                 project with a model and proposes a plan (a paid call, run only when you ask)"
            ),
            is_error: false,
        });
    }
    None
}

/// One project's presumed roots, each read serialized process-wide: a project closed and reopened
/// quickly makes a second [`PresumeWatcher`] over the same root before the first background thread
/// necessarily finished; without this, both could open `graph.db` for writing at once. Entries are
/// never removed - the process opens a small, bounded number of distinct project roots in its
/// lifetime, so this holds at most that many single-unit mutexes, not one per read.
fn root_lock(root: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut guard = locks.lock().unwrap_or_else(|p| p.into_inner());
    guard
        .entry(root.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// A test hook: observes when a real read starts and ends (to catch two
/// overlapping across separate `PresumeWatcher`s sharing a root),
/// counts real reads (to catch a change that starts a second read
/// while one is pending), and can slow a read down so a test can reliably poll
/// it while in flight. Kept unconditionally rather than behind `cfg(test)`
/// itself: a few atomics behind an `Option` cost nothing worth gating, and it
/// keeps `spawn` free of `cfg` branches.
#[derive(Clone, Default)]
struct ReadProbe {
    delay: Duration,
    reads: Arc<AtomicUsize>,
    busy: Arc<AtomicBool>,
    overlaps: Arc<AtomicUsize>,
}

/// One project's presumed-work spin-up: the check, and the background read it
/// starts when the check finds a change.
pub struct PresumeWatcher {
    root: PathBuf,
    checked: Instant,
    /// `None` until the first check; never equal to a real signature, so the
    /// first [`Self::tick`] always starts a read.
    signature: Option<Signature>,
    pending: Option<Receiver<(Signature, Result<Outcome, String>)>>,
    /// A notice waiting to be shown; drained by the caller once shown.
    notice: Option<PresumeNotice>,
    /// Set when a read failed; cleared on a clean one. Backs a read off
    /// regardless of whether the roadmap changed again meanwhile.
    retry_after: Option<Instant>,
    retry_step: usize,
    /// The text of the last error notice shown, kept so a forced retry that
    /// fails the same way again is not re-shown (post-merge): only a
    /// changed failure, or a recovery from one, produces a new notice.
    last_error: Option<String>,
    probe: Option<ReadProbe>,
}

/// What a background read found.
enum Outcome {
    /// The signature matched what was last read (or a retry was not forced):
    /// nothing was opened, nothing was read.
    Unchanged,
    /// No `graph.db`: the folder has never been indexed. Nothing was created.
    NotIndexed,
    /// A spin-up read ran.
    Report(SpinupReport),
}

impl PresumeWatcher {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            checked: Instant::now() - RECHECK,
            signature: None,
            pending: None,
            notice: None,
            retry_after: None,
            retry_step: 0,
            last_error: None,
            probe: None,
        }
    }

    /// Call every frame ([`crate::projects::OpenProject::poll`]); cheap (an
    /// `Instant` comparison only - the `stat`s themselves run on the
    /// background thread, never here). Drains a finished background read
    /// first, then, when none is in flight and it is time, starts one; the
    /// thread itself decides whether anything actually changed.
    pub fn tick(&mut self) {
        self.drain();
        if self.pending.is_some() {
            // Never two reads of one project's graph at once from this
            // watcher; the next check runs as soon as this one drains.
            return;
        }
        let now = Instant::now();
        let forced = match self.retry_after {
            Some(at) if now >= at => true,
            Some(_) => return,
            None => false,
        };
        if !forced && self.signature.is_some() && now.duration_since(self.checked) < RECHECK {
            return;
        }
        self.checked = now;
        self.retry_after = None;
        self.spawn(forced);
    }

    fn spawn(&mut self, forced: bool) {
        let root = self.root.clone();
        let last = self.signature.clone();
        let probe = self.probe.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("project-presume".into())
            .spawn(move || {
                let sig = signature(&root);
                let outcome = if !forced && Some(&sig) == last.as_ref() {
                    Ok(Outcome::Unchanged)
                } else {
                    // Never two reads of one root's graph at once, even across
                    // two `PresumeWatcher`s: a project closed and
                    // reopened before its first read finished must not race a
                    // second one opening `graph.db` for writing.
                    let lock = root_lock(&root);
                    let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(p) = &probe {
                        let was_busy = p.busy.swap(true, Ordering::SeqCst);
                        if was_busy {
                            p.overlaps.fetch_add(1, Ordering::SeqCst);
                        }
                        p.reads.fetch_add(1, Ordering::SeqCst);
                        if !p.delay.is_zero() {
                            std::thread::sleep(p.delay);
                        }
                    }
                    let outcome = run(&root);
                    if let Some(p) = &probe {
                        p.busy.store(false, Ordering::SeqCst);
                    }
                    outcome
                };
                let _ = tx.send((sig, outcome));
            });
        match spawned {
            Ok(_) => self.pending = Some(rx),
            Err(e) => {
                self.notice = Some(PresumeNotice {
                    text: format!("presumed work: could not start its read: {e}"),
                    is_error: true,
                })
            }
        }
    }

    fn drain(&mut self) {
        let Some(rx) = &self.pending else { return };
        let (sig, outcome) = match rx.try_recv() {
            Ok(pair) => pair,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.pending = None;
                return;
            }
        };
        self.pending = None;
        self.signature = Some(sig);
        match outcome {
            Ok(Outcome::Unchanged) => {}
            Ok(Outcome::NotIndexed) => {
                self.retry_reset();
                self.notice = self.recovered_or(Some(PresumeNotice {
                    text: NOT_INDEXED.to_owned(),
                    is_error: false,
                }));
            }
            Ok(Outcome::Report(report)) => {
                if report.failed.is_empty() {
                    self.retry_reset();
                    let n = notice_of(&report);
                    self.notice = self.recovered_or(n);
                } else {
                    self.retry_schedule();
                    if let Some(n) = notice_of(&report) {
                        self.note_error(n);
                    }
                }
            }
            Err(e) => {
                self.retry_schedule();
                self.note_error(PresumeNotice {
                    text: format!("presumed work: error: {e}"),
                    is_error: true,
                });
            }
        }
    }

    /// A notice from a failed read, shown only when its text differs from the
    /// last one shown: a forced retry that fails the same way
    /// again must not restate it, or it repeats every backoff step forever
    /// (about 30 times an hour at the 120 s step - the bug this fixes).
    fn note_error(&mut self, n: PresumeNotice) {
        if self.last_error.as_deref() == Some(n.text.as_str()) {
            return;
        }
        self.last_error = Some(n.text.clone());
        self.notice = Some(n);
    }

    /// `base` (whatever this clean read would otherwise say, if anything),
    /// with one "recovered" line folded in when the last read shown had
    /// failed. Clears the remembered failure either way, so the next failure,
    /// even an identical one, is shown once more.
    fn recovered_or(&mut self, base: Option<PresumeNotice>) -> Option<PresumeNotice> {
        let was_failing = self.last_error.take().is_some();
        match (was_failing, base) {
            (true, Some(n)) => Some(PresumeNotice {
                text: format!("{RECOVERED}; {}", n.text),
                is_error: n.is_error,
            }),
            (true, None) => Some(PresumeNotice {
                text: RECOVERED.to_owned(),
                is_error: false,
            }),
            (false, n) => n,
        }
    }

    fn retry_reset(&mut self) {
        self.retry_after = None;
        self.retry_step = 0;
    }

    fn retry_schedule(&mut self) {
        let step = self.retry_step.min(BACKOFF.len() - 1);
        self.retry_after = Some(Instant::now() + BACKOFF[step]);
        self.retry_step = self.retry_step.saturating_add(1);
    }

    /// The notice to show now, if one arrived since the last call.
    pub fn take_notice(&mut self) -> Option<PresumeNotice> {
        self.notice.take()
    }

    /// No read in flight, and at least one check has run (so a caller waiting
    /// for the first spin-up read to settle, as the integration tests in
    /// `projects.rs` do, knows it is safe to look at the graph). `pub(crate)`,
    /// `cfg(test)`: a test seam, not part of the watcher's own behavior.
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        self.pending.is_none() && self.signature.is_some()
    }

    /// Attaches a [`ReadProbe`]; test-only.
    #[cfg(test)]
    fn with_probe(mut self, probe: ReadProbe) -> Self {
        self.probe = Some(probe);
        self
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Opens the project's graph, if it has one, and runs the spin-up read. Runs on
/// the background thread [`PresumeWatcher::spawn`] starts, under that root's
/// lock; never on the UI thread.
///
/// **Never creates, never rebuilds**. A missing `graph.db`
/// means the folder has never been indexed: nothing is written, not even
/// `.kgraf/` itself, and [`Outcome::NotIndexed`] is returned so the caller can
/// say so. An existing one is opened with
/// [`kgraf_graph::GraphStore::open_existing_for_facts`] (existing only, facts
/// only): a schema version other than this build's is refused with
/// `GraphError::SchemaMismatch` rather than silently dropping the derived
/// tables and rebuilding them, which only an index command may do.
fn run(root: &Path) -> Result<Outcome, String> {
    let path = kgraf_graph::snapshot::graph_db_path(root);
    if !path.exists() {
        return Ok(Outcome::NotIndexed);
    }
    let store = GraphStore::open_existing_for_facts(&path, root).map_err(|e| format!("{e:#}"))?;
    presumed::presume_spinup(&store, now_ms())
        .map(Outcome::Report)
        .map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Opens (and thereby creates) a project's graph, as `kgraf index` does,
    /// then drops it: the fixture for "an already-indexed project" that every
    /// test below except the not-indexed and schema-mismatch ones needs, now
    /// that this module refuses to create one itself.
    fn index(root: &Path) {
        let path = kgraf_graph::snapshot::graph_db_path(root);
        std::fs::create_dir_all(path.parent().expect("state dir")).expect("mkdir");
        drop(GraphStore::open(&path, root).expect("graph"));
    }

    fn open_for_reading(root: &Path) -> GraphStore {
        GraphStore::open_read_only(&kgraf_graph::snapshot::graph_db_path(root), root)
            .expect("graph")
    }

    fn wait_for_notice(w: &mut PresumeWatcher) -> PresumeNotice {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            w.tick();
            if let Some(n) = w.take_notice() {
                return n;
            }
            assert!(Instant::now() < deadline, "no notice arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn settle(w: &mut PresumeWatcher) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while w.pending.is_some() {
            assert!(Instant::now() < deadline, "the read never finished");
            w.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Skips a real backoff wait (5 s, 30 s, 2 min) by declaring it already
    /// elapsed, starts the forced retry it unblocks, and waits for it to
    /// finish.
    fn force_retry_and_settle(w: &mut PresumeWatcher) {
        w.retry_after = Some(Instant::now() - Duration::from_millis(1));
        w.tick();
        settle(w);
    }

    /// A folder with no `graph.db` is left exactly as it is (no
    /// `.kgraf`, no `graph.db`), and the notice says the project is not
    /// indexed, even when a roadmap document is sitting right there.
    #[test]
    fn an_unindexed_folder_is_never_written_to() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("ROADMAP.md"), "- [ ] a\n").expect("write");
        let mut w = PresumeWatcher::new(dir.path().to_path_buf());
        let n = wait_for_notice(&mut w);
        assert!(!n.is_error, "{n:?}");
        assert!(n.text.contains("not indexed"), "{n:?}");
        assert!(n.text.contains("kgraf index"), "{n:?}");
        assert!(
            !dir.path().join(".kgraf").exists(),
            "viewing an unindexed folder must never create its state directory"
        );
    }

    /// An index of another schema version is refused, not rebuilt - its
    /// `files` row (derived data a rebuild would drop) is read back afterward,
    /// unchanged, through a read-only connection that itself never migrates.
    #[test]
    fn a_schema_mismatch_is_an_error_and_never_rebuilds() {
        let dir = tempfile::tempdir().expect("dir");
        let path = kgraf_graph::snapshot::graph_db_path(dir.path());
        std::fs::create_dir_all(path.parent().expect("state dir")).expect("mkdir");
        {
            let store = GraphStore::open(&path, dir.path()).expect("graph");
            store
                .upsert_file(&kgraf_graph::FileRecord {
                    id: "f1".into(),
                    path: "src/lib.rs".into(),
                    content_hash: "deadbeef".into(),
                    size_bytes: 10,
                    mtime_ns: 0,
                    language: None,
                    line_count: 1,
                })
                .expect("upsert");
            // A version this build does not write: `open_existing_for_facts`
            // must refuse rather than treat it as derived data to rebuild.
            store.set_meta("schema_version", "3").expect("meta");
        }
        std::fs::write(dir.path().join("ROADMAP.md"), "- [ ] a\n").expect("write");

        let mut w = PresumeWatcher::new(dir.path().to_path_buf());
        let n = wait_for_notice(&mut w);
        assert!(n.is_error, "{n:?}");
        assert!(n.text.to_lowercase().contains("schema"), "{n:?}");

        // Read-only: never migrates, so this itself proves nothing about a
        // rebuild; it only lets the test see whether one already happened.
        let after = open_for_reading(dir.path());
        assert_eq!(
            after.file_paths().expect("file_paths"),
            ["src/lib.rs"],
            "a refused mismatch must leave the derived files table exactly as it was"
        );
        assert_eq!(
            after.meta("schema_version").expect("meta"),
            Some("3".to_owned()),
            "the version itself must be untouched too"
        );
    }

    /// Opening a project with no roadmap shows the notice, once, off the UI
    /// thread (the test thread never blocks on the read). The project is
    /// already indexed, so this is "no roadmap", not "not indexed".
    #[test]
    fn no_roadmap_shows_a_notice() {
        let dir = tempfile::tempdir().expect("dir");
        index(dir.path());
        let mut w = PresumeWatcher::new(dir.path().to_path_buf());
        let n = wait_for_notice(&mut w);
        assert!(!n.is_error);
        assert!(n.text.contains("no roadmap found"), "{n:?}");
        assert!(n.text.contains("orchestrate --corpus"), "{n:?}");
    }

    /// A project with a roadmap presumes its nodes on open, and reopening (a
    /// second watcher over the same graph, nothing changed) writes nothing more.
    #[test]
    fn opening_presumes_and_reopening_does_not_duplicate() {
        let dir = tempfile::tempdir().expect("dir");
        index(dir.path());
        std::fs::write(
            dir.path().join("ROADMAP.md"),
            "## A\n\n- [ ] one\n- [x] two\n",
        )
        .expect("write");
        let mut w = PresumeWatcher::new(dir.path().to_path_buf());
        w.tick();
        settle(&mut w);
        assert!(w.take_notice().is_none(), "a clean read says nothing");

        let store = open_for_reading(dir.path());
        let before: Vec<_> = kgraf_interpret::presumed::views(&store)
            .expect("views")
            .into_iter()
            .filter(|v| v.kind == kgraf_graph::WorkKind::Item)
            .collect();
        assert_eq!(before.len(), 2);
        drop(store);

        // A second open of the same project (a fresh watcher, as a relaunch
        // makes) presumes again and writes no duplicate.
        let mut w2 = PresumeWatcher::new(dir.path().to_path_buf());
        w2.tick();
        settle(&mut w2);
        let store = open_for_reading(dir.path());
        let after: Vec<_> = kgraf_interpret::presumed::views(&store)
            .expect("views")
            .into_iter()
            .filter(|v| v.kind == kgraf_graph::WorkKind::Item && !v.gone)
            .collect();
        assert_eq!(after.len(), 2, "no duplicate from reopening");
    }

    /// Editing the roadmap after it was already presumed is picked up as a
    /// signature change and re-read.
    #[test]
    fn editing_the_roadmap_updates_the_nodes() {
        let dir = tempfile::tempdir().expect("dir");
        index(dir.path());
        let path = dir.path().join("ROADMAP.md");
        std::fs::write(&path, "## A\n\n- [ ] one\n").expect("write");
        let mut w = PresumeWatcher::new(dir.path().to_path_buf());
        w.tick();
        settle(&mut w);
        w.take_notice();

        // A modification time some file systems resolve at one-second
        // granularity: back-date the check so the coming write is seen as later.
        w.checked = Instant::now() - RECHECK - Duration::from_millis(1);
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(&path, "## A\n\n- [x] one\n").expect("write");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            w.tick();
            if w.is_idle() {
                let store = open_for_reading(dir.path());
                let items: Vec<_> = kgraf_interpret::presumed::views(&store)
                    .expect("views")
                    .into_iter()
                    .filter(|v| v.kind == kgraf_graph::WorkKind::Item && !v.gone)
                    .collect();
                if items.len() == 1
                    && items[0].state == kgraf_interpret::presumed::PresumedState::Done
                {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "the edit was never picked up");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Deleting the roadmap after it was presumed marks its nodes gone, and the
    /// no-roadmap notice returns.
    #[test]
    fn deleting_the_roadmap_marks_its_nodes_gone() {
        let dir = tempfile::tempdir().expect("dir");
        index(dir.path());
        let path = dir.path().join("ROADMAP.md");
        std::fs::write(&path, "## A\n\n- [ ] one\n").expect("write");
        let mut w = PresumeWatcher::new(dir.path().to_path_buf());
        w.tick();
        settle(&mut w);
        w.take_notice();
        w.checked = Instant::now() - RECHECK - Duration::from_millis(1);
        std::fs::remove_file(&path).expect("rm");

        let n = wait_for_notice(&mut w);
        assert!(!n.is_error);
        assert!(n.text.contains("no roadmap found"), "{n:?}");

        let store = open_for_reading(dir.path());
        let items: Vec<_> = kgraf_interpret::presumed::views(&store)
            .expect("views")
            .into_iter()
            .filter(|v| v.kind == kgraf_graph::WorkKind::Item)
            .collect();
        assert_eq!(items.len(), 1);
        assert!(items[0].gone);
    }

    /// A document discovery finds but cannot read is a visible error, never a
    /// silent skip.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_roadmap_is_an_error_notice() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("dir");
        index(dir.path());
        let path = dir.path().join("ROADMAP.md");
        std::fs::write(&path, "## A\n\n- [ ] one\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        // Root ignores file permissions; skip where this test cannot bite (CI
        // sometimes runs as root inside a container).
        if std::fs::read_to_string(&path).is_ok() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
            return;
        }
        let mut w = PresumeWatcher::new(dir.path().to_path_buf());
        let n = wait_for_notice(&mut w);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
        assert!(n.is_error, "{n:?}");
        assert!(n.text.contains("could not be read"), "{n:?}");
    }

    /// The spin-up read takes only the graph: no ledger is ever created by it.
    #[test]
    fn presuming_spends_no_token() {
        let dir = tempfile::tempdir().expect("dir");
        index(dir.path());
        std::fs::write(dir.path().join("ROADMAP.md"), "- [ ] a\n").expect("write");
        let mut w = PresumeWatcher::new(dir.path().to_path_buf());
        w.tick();
        settle(&mut w);
        w.take_notice();
        assert!(
            !dir.path().join(".kgraf").join("ledger.db").exists(),
            "presuming must never create a ledger"
        );
    }

    /// A failed read (here, a schema mismatch that the fix keeps failing
    /// forever on its own) is retried with backoff, not latched. This double
    /// checks the mechanism directly rather than waiting out the real 5 s step.
    #[test]
    fn a_failed_read_is_retried_not_latched() {
        let dir = tempfile::tempdir().expect("dir");
        let mut w = PresumeWatcher::new(dir.path().to_path_buf());
        // No graph.db yet: the first read is clean (`NotIndexed`), so force a
        // failing condition by making a `graph.db` that is not a database at
        // all, which every open of it (including retries) keeps failing on -
        // exactly the "still broken, must still retry" shape the forced retry needs.
        let path = kgraf_graph::snapshot::graph_db_path(dir.path());
        std::fs::create_dir_all(path.parent().expect("state dir")).expect("mkdir");
        std::fs::write(&path, b"not a sqlite file").expect("write");
        std::fs::write(dir.path().join("ROADMAP.md"), "- [ ] a\n").expect("write");

        let first = wait_for_notice(&mut w);
        assert!(first.is_error, "{first:?}");
        assert!(
            w.retry_after.is_some(),
            "a failed read must schedule a retry"
        );

        // Force the backoff to have already elapsed, as the real 5 s step
        // eventually does, without the test waiting for it. The failure is
        // identical, so no second notice is shown (post-merge fix below), but
        // the step still advances: the retry itself really ran.
        force_retry_and_settle(&mut w);
        assert!(
            w.take_notice().is_none(),
            "an identical failure repeats silently"
        );
        assert!(
            w.retry_step >= 2,
            "the step must advance across a second failure: {}",
            w.retry_step
        );
    }

    /// Post-merge fix: a read that keeps failing the same way was re-shown on
    /// every forced retry (5 s, 30 s, then every 120 s - about 30 times an
    /// hour at that step). Two failed retries with the same error now produce
    /// exactly one notice.
    #[test]
    fn two_failed_retries_with_the_same_error_produce_one_notice() {
        let dir = tempfile::tempdir().expect("dir");
        let path = kgraf_graph::snapshot::graph_db_path(dir.path());
        std::fs::create_dir_all(path.parent().expect("state dir")).expect("mkdir");
        std::fs::write(&path, b"not a sqlite file").expect("write");
        std::fs::write(dir.path().join("ROADMAP.md"), "- [ ] a\n").expect("write");

        let mut w = PresumeWatcher::new(dir.path().to_path_buf());
        let first = wait_for_notice(&mut w);
        assert!(first.is_error, "{first:?}");

        force_retry_and_settle(&mut w);
        assert!(
            w.take_notice().is_none(),
            "the first retry's identical failure must say nothing more"
        );
        force_retry_and_settle(&mut w);
        assert!(
            w.take_notice().is_none(),
            "the second retry's identical failure must say nothing more either"
        );
        assert!(
            w.retry_step >= 3,
            "each retry still really ran and the backoff still advanced: {}",
            w.retry_step
        );
    }

    /// Post-merge fix: the first read that succeeds after a run of failures
    /// says so, once, in one "recovered" line.
    #[test]
    fn a_recovery_after_failure_produces_one_recovered_line() {
        let dir = tempfile::tempdir().expect("dir");
        let path = kgraf_graph::snapshot::graph_db_path(dir.path());
        std::fs::create_dir_all(path.parent().expect("state dir")).expect("mkdir");
        std::fs::write(&path, b"not a sqlite file").expect("write");
        std::fs::write(dir.path().join("ROADMAP.md"), "- [ ] a\n").expect("write");

        let mut w = PresumeWatcher::new(dir.path().to_path_buf());
        let first = wait_for_notice(&mut w);
        assert!(first.is_error, "{first:?}");
        force_retry_and_settle(&mut w);
        w.take_notice();

        // Fix the underlying problem: a real, valid index.
        std::fs::remove_file(&path).expect("rm");
        index(dir.path());

        force_retry_and_settle(&mut w);
        let recovered = w.take_notice().expect("a recovery must say so once");
        assert!(!recovered.is_error, "{recovered:?}");
        assert!(recovered.text.contains("recovered"), "{recovered:?}");

        // The failure is forgotten: a later, different failure is shown again
        // (not silently suppressed as if it were the same old one).
        std::fs::remove_file(&path).expect("rm");
        std::fs::write(&path, b"broken again, differently this time").expect("write");
        force_retry_and_settle(&mut w);
        let again = w
            .take_notice()
            .expect("a fresh failure after a recovery is shown");
        assert!(again.is_error, "{again:?}");
    }

    /// A read-count hook proves a mutation that starts a second read while
    /// one is pending would be caught. One change gives exactly one read, even
    /// under many rapid `tick()` calls made while the (deliberately slowed)
    /// read is still in flight.
    #[test]
    fn one_change_starts_exactly_one_read() {
        let dir = tempfile::tempdir().expect("dir");
        index(dir.path());
        std::fs::write(dir.path().join("ROADMAP.md"), "- [ ] a\n").expect("write");
        let probe = ReadProbe {
            delay: Duration::from_millis(200),
            ..ReadProbe::default()
        };
        let mut w = PresumeWatcher::new(dir.path().to_path_buf()).with_probe(probe.clone());
        w.tick();
        assert!(w.pending.is_some(), "the first tick must start a read");
        // The thread is spawned, not yet necessarily scheduled: wait for it to
        // actually reach the read (bounded well under its own 200 ms delay)
        // before asserting nothing else joins it.
        let started = Instant::now() + Duration::from_secs(2);
        while probe.reads.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < started, "the read never started");
            std::thread::sleep(Duration::from_millis(2));
        }
        let until = Instant::now() + Duration::from_millis(150);
        while Instant::now() < until {
            w.tick();
            assert_eq!(
                probe.reads.load(Ordering::SeqCst),
                1,
                "a second read must not start while the first is pending"
            );
        }
        settle(&mut w);
        assert_eq!(probe.reads.load(Ordering::SeqCst), 1);
        // Nothing changed since: further ticks read nothing more.
        for _ in 0..5 {
            w.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(probe.reads.load(Ordering::SeqCst), 1);
    }

    /// A project closed and reopened quickly (two watchers over the same
    /// root, each starting a slow read) never runs two reads of that root's
    /// graph at once - the process-wide root lock serializes them.
    #[test]
    fn two_watchers_over_one_root_never_read_at_once() {
        let dir = tempfile::tempdir().expect("dir");
        index(dir.path());
        std::fs::write(dir.path().join("ROADMAP.md"), "- [ ] a\n").expect("write");
        let probe = ReadProbe {
            delay: Duration::from_millis(150),
            ..ReadProbe::default()
        };
        let mut a = PresumeWatcher::new(dir.path().to_path_buf()).with_probe(probe.clone());
        let mut b = PresumeWatcher::new(dir.path().to_path_buf()).with_probe(probe.clone());
        a.tick();
        // Started right after `a`, as a quick close-then-reopen would: `a`'s
        // background thread has not necessarily reached the lock yet.
        std::thread::sleep(Duration::from_millis(10));
        b.tick();
        settle(&mut a);
        settle(&mut b);
        assert_eq!(
            probe.overlaps.load(Ordering::SeqCst),
            0,
            "two reads of one root's graph ran at once"
        );
        assert_eq!(
            probe.reads.load(Ordering::SeqCst),
            2,
            "each watcher reads once"
        );
    }
}
