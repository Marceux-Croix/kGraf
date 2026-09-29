//! The run watch: one background thread that reads the project's run-state records
//! (0056) from its graph store and sends them when they change.
//!
//! The records are where the orchestrator keeps each contract's state, wait reason,
//! gates and workers, so they are what the graph's work-node status and the
//! box's `/status` without a hosted run read. The orchestrator owns them; this thread
//! only reads, through a read-only connection
//! ([`kgraf_graph::snapshot::open_existing`]).
//!
//! **What a poll costs.** A record is one `meta` row rewritten in place, and `meta`
//! has no change-log trigger, so this polls every [`POLL`], in three steps, each
//! skipped when the one before shows nothing new:
//! 1. `PRAGMA data_version` (no table read): unchanged means no other connection
//!    committed since the last poll;
//! 2. each record's length and content hash (`GraphStore::meta_digests`);
//! 3. the full value, only for the records whose digest changed.
//!
//! Each poll also asks who holds each run's lease (`run:<id>`), because a lease
//! lapses by time without a write: Play must not resume a run a live process holds.
//! An idle project costs one pragma and a lease lookup per run per poll, and wakes
//! nobody.
//!
//! Same lifecycle as the galaxy feed: dropping the handle stops and joins the thread.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use kgraf_graph::GraphStore;
use kgraf_graph::snapshot::{ProjectIndex, open_existing};
use kgraf_orchestrator::{RunStateRecord, StoredRun};

/// How often the store is read.
pub const POLL: Duration = Duration::from_millis(750);

/// What the watch reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunsUpdate {
    /// Every stored run, as of this read, and the runs whose lease a live process
    /// holds now; and the presumed work nodes (decision 0064) with their states
    /// derived as of the last change to the graph's tables, or why they could not
    /// be read (said on the box, never dropped: fail loud).
    Runs {
        runs: Vec<StoredRun>,
        leased: BTreeSet<String>,
        presumed: Result<Vec<kgraf_interpret::PresumedView>, String>,
    },
    /// The store could not be read; said until a read succeeds.
    Unavailable(String),
}

/// The watch's handle.
pub struct RunWatch {
    rx: Receiver<RunsUpdate>,
    stop: Arc<AtomicBool>,
    values_read: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for RunWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunWatch").finish_non_exhaustive()
    }
}

impl Drop for RunWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

impl RunWatch {
    /// Watch `root`'s store every `poll`; `wake` runs after each update.
    pub fn spawn(
        root: PathBuf,
        poll: Duration,
        wake: impl Fn() + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let values_read = Arc::new(AtomicU64::new(0));
        let flag = stop.clone();
        let counter = values_read.clone();
        let thread = std::thread::Builder::new()
            .name("run-watch".into())
            .spawn(move || watch(&root, poll, &flag, &counter, &tx, &wake))?;
        Ok(Self {
            rx,
            stop,
            values_read,
            thread: Some(thread),
        })
    }

    /// The newest update, if any arrived; older ones are superseded.
    pub fn latest(&self) -> Option<RunsUpdate> {
        self.rx.try_iter().last()
    }

    /// Full `meta` values read so far (tests: a poll reads only changed records).
    pub fn values_read(&self) -> u64 {
        self.values_read.load(Ordering::SeqCst)
    }
}

/// What the watch has read, by `meta` key: the digest and the value.
struct Cache {
    data_version: Option<i64>,
    rows: BTreeMap<String, ((i64, i64), String)>,
    leased: BTreeSet<String>,
    /// The change-log watermark the presumed states were derived at. Presumed
    /// nodes and the code they name live in tables with change-log triggers, so
    /// an unchanged watermark means nothing they derive from changed; run records
    /// live in `meta`, which has none, so a record write does not re-derive them.
    watermark: Option<i64>,
    presumed: Result<Vec<kgraf_interpret::PresumedView>, String>,
    sent: Option<Sent>,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            data_version: None,
            rows: BTreeMap::new(),
            leased: BTreeSet::new(),
            watermark: None,
            presumed: Ok(Vec::new()),
            sent: None,
        }
    }
}

type Sent = (
    Vec<StoredRun>,
    BTreeSet<String>,
    Result<Vec<kgraf_interpret::PresumedView>, String>,
);

impl Cache {
    fn runs(&self) -> Vec<StoredRun> {
        let active: BTreeSet<&str> = self
            .rows
            .iter()
            .filter(|(_, (_, v))| v == "1")
            .filter_map(|(k, _)| k.strip_prefix("orchestrator/active/"))
            .collect();
        self.rows
            .iter()
            .filter_map(|(k, (_, v))| {
                let run = k.strip_prefix("orchestrator/run/")?;
                Some(StoredRun {
                    run: run.to_owned(),
                    active: active.contains(run),
                    record: RunStateRecord::from_json(v).map_err(|e| e.to_string()),
                })
            })
            .collect()
    }
}

/// One poll. Returns the update to send, if anything a reader sees changed.
fn poll_once(
    store: &GraphStore,
    cache: &mut Cache,
    values_read: &AtomicU64,
) -> Result<Option<RunsUpdate>, String> {
    let e = |x: kgraf_graph::GraphError| x.to_string();
    let dv = store.data_version().map_err(e)?;
    if cache.data_version != Some(dv) {
        cache.data_version = Some(dv);
        let digests = store.meta_digests("orchestrator/").map_err(e)?;
        let keys: BTreeSet<&str> = digests.iter().map(|(k, ..)| k.as_str()).collect();
        cache.rows.retain(|k, _| keys.contains(k.as_str()));
        for (key, len, hash) in &digests {
            let digest = (*len, *hash);
            if cache.rows.get(key).is_some_and(|(d, _)| *d == digest) {
                continue;
            }
            let value = store.meta(key).map_err(e)?.unwrap_or_default();
            values_read.fetch_add(1, Ordering::SeqCst);
            cache.rows.insert(key.clone(), (digest, value));
        }
        let watermark = store.change_watermark().map_err(e)?;
        if cache.watermark != Some(watermark) {
            cache.watermark = Some(watermark);
            cache.presumed = kgraf_interpret::presumed::views(store).map_err(|x| x.to_string());
        }
    }
    let runs = cache.runs();
    let now = kgraf_graph::snapshot::now_ms();
    let reg = kgraf_graph::LeaseRegistry::new(store);
    let mut leased = BTreeSet::new();
    for r in &runs {
        // A holder whose process is gone does not count: resume takes its
        // lease over at once.
        if reg
            .holder_of(&format!("run:{}", r.run), now)
            .map_err(e)?
            .is_some_and(|l| kgraf_orchestrator::run_lease_holder_is_live(&l.agent_id))
        {
            leased.insert(r.run.clone());
        }
    }
    cache.leased = leased.clone();
    let now_sent = (runs.clone(), leased.clone(), cache.presumed.clone());
    if cache.sent.as_ref() == Some(&now_sent) {
        return Ok(None);
    }
    cache.sent = Some(now_sent);
    Ok(Some(RunsUpdate::Runs {
        runs,
        leased,
        presumed: cache.presumed.clone(),
    }))
}

fn watch(
    root: &Path,
    poll: Duration,
    stop: &AtomicBool,
    values_read: &AtomicU64,
    tx: &Sender<RunsUpdate>,
    wake: &dyn Fn(),
) {
    let mut cache = Cache::default();
    let mut last_error: Option<String> = None;
    let mut store = None;
    while !stop.load(Ordering::SeqCst) {
        let result = (|| -> Result<Option<RunsUpdate>, String> {
            if store.is_none() {
                match open_existing(root).map_err(|e| e.to_string())? {
                    ProjectIndex::Ready(s) => store = Some(s),
                    // No index yet: no runs to show, and the galaxy says why.
                    ProjectIndex::Missing { .. } | ProjectIndex::OtherSchema { .. } => {
                        return Ok(None);
                    }
                }
            }
            let Some(s) = &store else { return Ok(None) };
            poll_once(s, &mut cache, values_read)
        })();
        let update = match result {
            Ok(Some(u)) => {
                last_error = None;
                Some(u)
            }
            Ok(None) => None,
            Err(e) => {
                // A failed read drops the connection so the next poll reopens it.
                store = None;
                cache = Cache::default();
                (last_error.as_ref() != Some(&e)).then(|| {
                    last_error = Some(e.clone());
                    RunsUpdate::Unavailable(e)
                })
            }
        };
        if let Some(u) = update {
            if tx.send(u).is_err() {
                return;
            }
            wake();
        }
        if !stop.load(Ordering::SeqCst) {
            std::thread::park_timeout(poll);
        }
    }
}

/// The newest parsed run (by last update), for `/status` without a hosted run.
pub fn newest(runs: &[StoredRun]) -> Option<&RunStateRecord> {
    runs.iter()
        .filter_map(|r| r.record.as_ref().ok())
        .max_by_key(|r| r.updated_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::RunId;
    use kgraf_orchestrator::RunStore;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    fn project() -> (tempfile::TempDir, RunStore) {
        let dir = tempfile::tempdir().expect("dir");
        let db = kgraf_graph::snapshot::graph_db_path(dir.path());
        std::fs::create_dir_all(db.parent().expect("parent")).expect("mkdir");
        let graph = GraphStore::open(&db, dir.path()).expect("graph");
        (dir, RunStore::new(Arc::new(Mutex::new(graph))))
    }

    fn wait(watch: &RunWatch) -> RunsUpdate {
        for _ in 0..400 {
            if let Some(u) = watch.latest() {
                return u;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("no update");
    }

    fn runs_of(u: RunsUpdate) -> (Vec<StoredRun>, BTreeSet<String>) {
        match u {
            RunsUpdate::Runs { runs, leased, .. } => (runs, leased),
            RunsUpdate::Unavailable(e) => panic!("{e}"),
        }
    }

    #[test]
    fn a_record_written_by_the_orchestrator_reaches_the_ui_and_an_unchanged_one_does_not() {
        let (dir, store) = project();
        let woken = Arc::new(AtomicUsize::new(0));
        let w = woken.clone();
        let watch = RunWatch::spawn(
            dir.path().to_owned(),
            Duration::from_millis(10),
            move || {
                w.fetch_add(1, Ordering::SeqCst);
            },
        )
        .expect("watch");
        assert!(
            runs_of(wait(&watch)).0.is_empty(),
            "an empty store is news once"
        );

        let mut record = RunStateRecord::new(RunId::new("r1"), "make a", 10);
        store.save(&record).expect("save");
        let (runs, _) = runs_of(wait(&watch));
        assert_eq!(
            newest(&runs).map(|r| r.run.to_string()),
            Some("r1".to_owned())
        );

        // Nothing changed: no update, no wake.
        let before = woken.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(60));
        assert!(watch.latest().is_none());
        assert_eq!(
            woken.load(Ordering::SeqCst),
            before,
            "an idle store wakes nobody"
        );

        record.updated_ms = 20;
        record.verified_contracts.push("a".into());
        store.save(&record).expect("save");
        let (runs, _) = runs_of(wait(&watch));
        let r = newest(&runs).expect("record");
        assert_eq!(r.verified_contracts, ["a"]);
        assert!(r.status_line().starts_with("run r1 (running): 1 verified"));
    }

    /// Decision 0064: presumed nodes written to the project's graph reach the UI
    /// with their derived states, and a re-read of a changed roadmap reaches it
    /// again; the watch reads through its read-only connection.
    #[test]
    fn presumed_nodes_reach_the_ui_and_follow_the_roadmap() {
        let (dir, store) = project();
        let roadmap = |rows: &str| {
            std::fs::write(
                dir.path().join("ROADMAP.md"),
                format!("## P\n\n| Phase | What | Status |\n|---|---|---|\n{rows}"),
            )
            .expect("write");
        };
        roadmap("| 1 | One | Complete |\n");
        let presume = || {
            let g = store.graph();
            let g = g.lock().expect("lock");
            kgraf_interpret::presumed::presume_roadmaps(&g, &["ROADMAP.md".into()], 1)
                .expect("presume");
        };
        presume();
        let watch = RunWatch::spawn(dir.path().to_owned(), Duration::from_millis(10), || {})
            .expect("watch");
        let items = |u: RunsUpdate| match u {
            RunsUpdate::Runs { presumed, .. } => presumed
                .expect("presumed")
                .into_iter()
                .filter(|v| v.kind == kgraf_graph::WorkKind::Item)
                .map(|v| (v.title, v.state))
                .collect::<Vec<_>>(),
            RunsUpdate::Unavailable(e) => panic!("{e}"),
        };
        use kgraf_interpret::PresumedState::*;
        let title = |n: u32, name: &str| format!("Phase {n}: {name}");
        assert_eq!(items(wait(&watch)), [(title(1, "One"), Done)]);

        roadmap("| 1 | One | Complete |\n| 2 | Two | Not started |\n");
        presume();
        assert_eq!(
            items(wait(&watch)),
            [(title(1, "One"), Done), (title(2, "Two"), Remaining)]
        );
    }

    /// L7: a poll reads the full value of only the records that changed.
    #[test]
    fn only_changed_records_are_read_again() {
        let (dir, store) = project();
        store
            .save(&RunStateRecord::new(RunId::new("r1"), "one", 10))
            .expect("save");
        let mut r2 = RunStateRecord::new(RunId::new("r2"), "two", 10);
        store.save(&r2).expect("save");
        let watch = RunWatch::spawn(dir.path().to_owned(), Duration::from_millis(10), || {})
            .expect("watch");
        assert_eq!(runs_of(wait(&watch)).0.len(), 2);
        let read = watch.values_read();
        assert_eq!(read, 2);
        r2.updated_ms = 11;
        store.save(&r2).expect("save");
        let _ = wait(&watch);
        assert_eq!(watch.values_read(), read + 1, "only r2 was read again");
        // A commit that touches no record (a lease) reads no value.
        {
            let g = store.graph().lock().expect("lock");
            g.set_meta("unrelated", "x").expect("meta");
        }
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(watch.values_read(), read + 1);
    }

    /// N2: two saves in the same millisecond with the same length are both seen
    /// (the digest is a content hash, not length and `updated_ms`).
    #[test]
    fn two_same_millisecond_same_length_saves_are_both_seen() {
        let (dir, store) = project();
        let mut r = RunStateRecord::new(RunId::new("r1"), "one", 10);
        r.verified_contracts = vec!["a".into()];
        store.save(&r).expect("save");
        let watch = RunWatch::spawn(dir.path().to_owned(), Duration::from_millis(10), || {})
            .expect("watch");
        let (runs, _) = runs_of(wait(&watch));
        assert_eq!(newest(&runs).expect("r1").verified_contracts, ["a"]);
        // Same updated_ms, same length, different content.
        r.verified_contracts = vec!["b".into()];
        store.save(&r).expect("save");
        let (runs, _) = runs_of(wait(&watch));
        assert_eq!(newest(&runs).expect("r1").verified_contracts, ["b"]);
    }

    /// L10: a run whose lease a live process holds is reported as leased.
    #[test]
    fn a_run_held_by_a_live_process_is_reported_as_leased() {
        let (dir, store) = project();
        store
            .save(&RunStateRecord::new(RunId::new("r1"), "one", 10))
            .expect("save");
        {
            let g = store.graph().lock().expect("lock");
            let got = kgraf_graph::LeaseRegistry::new(&g)
                .acquire_all(
                    "other-process",
                    "r1",
                    &["run:r1".to_owned()],
                    &[],
                    kgraf_graph::snapshot::now_ms(),
                )
                .expect("lease");
            assert!(matches!(got, kgraf_graph::Acquisition::Granted(_)));
        }
        let watch = RunWatch::spawn(dir.path().to_owned(), Duration::from_millis(10), || {})
            .expect("watch");
        let (_, leased) = runs_of(wait(&watch));
        assert!(leased.contains("r1"));
    }
}
