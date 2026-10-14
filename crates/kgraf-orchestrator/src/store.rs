//! Where a run's durable state lives.
//!
//! - The **run-state record** is a row in the graph store (0056: "the record lives
//!   in the graph store, not in a second surface"), under the `meta` key
//!   `orchestrator/run/<run id>`. `orchestrator/active/<run id>` marks each run a
//!   restart should resume, one key per run, so two runs never overwrite each
//!   other's marker. Each write is one SQLite upsert, so a kill leaves either the
//!   previous record or the new one, never half of either.
//! - **Which process owns a run** is a lease in the graph store's lease registry
//!   (key `run:<run id>`), so a second orchestrator on the same run is refused
//!   (see `orchestrator.rs`).
//! - **Worker transcripts** are JSON lines, one file per worker, through
//!   `kgraf-session`'s `JsonlTranscript` (ported from codex-rs `rollout/`), so a
//!   resumed worker continues its transcript instead of repeating paid rounds.
//! - The **trace** (the six open-question measurements, `trace.rs`) is JSON lines
//!   in the same run directory.
//!
//! A graph schema bump keeps run records. `GraphStore` rebuilds only derived
//! data (the code index and the change log) when its schema version changes;
//! `meta`, where the records live, the work tables and the facts are not in
//! `DROP_DERIVED_ON_SCHEMA_CHANGE`, and a test reopens a store at another
//! version to show the record and the plan mirror survive.
//!
//! The **plan mirror** (`work_mirror.rs`) is written in the same transaction as
//! the record: one `GraphStore::put_record_with_mirror`. It is rows derived from
//! the record; the record wins on any disagreement, and [`RunStore::rebuild_mirror`]
//! writes it again from the record.

use crate::run_state::{RunStateError, RunStateRecord};
use kgraf_core::RunId;
use kgraf_graph::GraphStore;
use kgraf_runtime::blocking;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn record_key(run: &RunId) -> String {
    format!("orchestrator/run/{}", run.as_str())
}

fn active_key(run: &RunId) -> String {
    format!("orchestrator/active/{}", run.as_str())
}

/// A store failure.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("graph store: {0}")]
    Graph(#[from] kgraf_graph::GraphError),
    #[error(transparent)]
    Record(#[from] RunStateError),
    #[error("the graph store lock was poisoned by a panic")]
    Poisoned,
    #[error("gate decision: {0}")]
    Decision(String),
    #[error("run directory {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The graph store, shared. `GraphStore` holds one SQLite connection and is not
/// `Sync`, so every use takes the lock, and every call runs through
/// [`blocking`] so SQLite never stalls an executor thread.
pub type SharedGraph = Arc<Mutex<GraphStore>>;

/// What writing a record's mirror cost and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MirrorReport {
    /// Microseconds kGraf spent on the mirror: building it, diffing it against
    /// what this process wrote, and the rows written.
    pub micros: i64,
    /// Rows added or changed. 0: every item was already written as it is.
    pub rows: u64,
    /// The record's plan-relevant parts were as at the last write, so no row was
    /// built or compared.
    pub skipped: bool,
    /// Why the mirror was not written. The record was.
    pub error: Option<String>,
}

/// What [`RunStore::rebuild_mirror`] found and did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorRepair {
    /// The mirror already held exactly what the record gives.
    pub matched: bool,
    /// Items the record gives (the plan root excluded).
    pub items: usize,
    /// Rows added or changed to make it so.
    pub rows: u64,
    /// Microseconds the repair took: reading the record, building the mirror,
    /// comparing it and writing what differed.
    pub micros: i64,
}

/// Run records in the graph store.
#[derive(Clone)]
pub struct RunStore {
    graph: SharedGraph,
    /// What this store wrote of each run's mirror, so an unchanged item is not
    /// written again.
    mirrored: Arc<Mutex<crate::work_mirror::MirrorCache>>,
}

impl RunStore {
    pub fn new(graph: SharedGraph) -> Self {
        Self {
            graph,
            mirrored: Arc::default(),
        }
    }

    pub fn graph(&self) -> &SharedGraph {
        &self.graph
    }

    fn with<T>(
        &self,
        f: impl FnOnce(&GraphStore) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        blocking(|| {
            let g = self.graph.lock().map_err(|_| StoreError::Poisoned)?;
            f(&g)
        })
    }

    /// Validate and write the record.
    ///
    /// **Does not scrub; intake does** (decision 0071).
    /// Scrubbing the whole serialized record here was built and reverted: a
    /// `RegexSet` scan of the whole record on nearly every state transition,
    /// plus several threads racing to build the scrubber, made a lease
    /// release land late enough to fail
    /// `a_run_in_one_project_continues_while_another_is_shown`. The record's
    /// free text is scrubbed where it is taken in instead, once:
    /// `RunStateRecord::new` (the instruction), `OpenContract::planned` (a
    /// brief and its acceptance), a note, a posted message body, a gate
    /// decision's note, a wait's detail and a grounding verdict's reason.
    pub fn save(&self, record: &RunStateRecord) -> Result<(), StoreError> {
        let report = self.save_mirrored(record)?;
        if let Some(e) = report.error {
            // Loud, and never a reason to lose the record. The loop reads the
            // report of `save_mirrored` and says it in a notice.
            kgraf_diag::record(kgraf_diag::DiagEvent::error(
                "KG-0499",
                "kgraf-orchestrator",
                format!("the plan mirror of run {} was not written: {e}", record.run),
            ));
        }
        Ok(())
    }

    /// [`Self::save`], with the report of the mirror write. The record and the
    /// mirror's changed items are written in one transaction; a mirror that
    /// cannot be written is in the report and the record is still saved.
    pub fn save_mirrored(&self, record: &RunStateRecord) -> Result<MirrorReport, StoreError> {
        let text = record.to_json()?;
        let key = record_key(&record.run);
        // The mirror is built and diffed before the graph lock is taken. A record
        // whose plan-relevant parts hash as they did at the last write builds
        // nothing at all: that is nearly every save.
        let started = Instant::now();
        let plan = crate::work_mirror::plan_id(record.run.as_str());
        let fingerprint = crate::work_mirror::fingerprint(record);
        let prepared = {
            let cache = self
                .mirrored
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cache.fingerprint_is(&plan, fingerprint) {
                None
            } else {
                let (part, hashes) = cache.diff(&crate::work_mirror::mirror(record));
                Some((part, hashes))
            }
        };
        let built = started.elapsed().as_micros().min(i64::MAX as u128) as i64;
        let skipped = prepared.is_none();
        let (part, hashes) = match prepared {
            Some((part, hashes)) => (Some(part), Some(hashes)),
            None => (None, None),
        };
        let part = part.as_ref().filter(|p| !p.nodes.is_empty());
        self.with(|g| {
            let write = g.put_record_with_mirror(&key, &text, part)?;
            if write.error.is_none() {
                let mut cache = self
                    .mirrored
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(hashes) = hashes {
                    cache.remember(&plan, hashes);
                }
                cache.remember_fingerprint(&plan, fingerprint);
            }
            Ok(MirrorReport {
                micros: built.saturating_add(write.micros.min(i64::MAX as u64) as i64),
                rows: write.changed(),
                skipped,
                error: write.error,
            })
        })
    }

    /// How many times this store built and diffed a mirror (a save whose record
    /// did not touch the plan builds none).
    pub fn mirror_builds(&self) -> usize {
        self.mirrored
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .builds()
    }

    /// Write a run's mirror again from its record, the record winning: the
    /// repair command and the resume path. `None`: the run has no record.
    pub fn rebuild_mirror(&self, run: &RunId) -> Result<Option<MirrorRepair>, StoreError> {
        let started = Instant::now();
        let Some(record) = self.load(run)? else {
            return Ok(None);
        };
        let full = crate::work_mirror::mirror(&record);
        self.with(|g| {
            let matched = g.plan_mirror_matches(&full)?;
            // A mirror that already matches is not written: a resume of every run
            // checks, and a check writes nothing.
            let write = if matched {
                kgraf_graph::MirrorWrite::default()
            } else {
                g.put_plan_mirror(&full)?
            };
            let mut cache = self
                .mirrored
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.forget(&full.plan_id);
            let (_, hashes) = cache.diff(&full);
            cache.remember(&full.plan_id, hashes);
            cache.remember_fingerprint(&full.plan_id, crate::work_mirror::fingerprint(&record));
            Ok(Some(MirrorRepair {
                matched,
                items: full.nodes.len().saturating_sub(1),
                rows: write.changed(),
                micros: started.elapsed().as_micros().min(i64::MAX as u128) as i64,
            }))
        })
    }

    /// Whether the graph's mirror of `run` is exactly what its record gives.
    /// `None`: no record.
    pub fn mirror_matches(&self, run: &RunId) -> Result<Option<bool>, StoreError> {
        let Some(record) = self.load(run)? else {
            return Ok(None);
        };
        let full = crate::work_mirror::mirror(&record);
        self.with(|g| Ok(Some(g.plan_mirror_matches(&full)?)))
    }

    /// Every run's record, validated. A record that does not validate is an
    /// error, never skipped.
    pub fn all(&self) -> Result<Vec<RunStateRecord>, StoreError> {
        let rows = self.with(|g| Ok(g.meta_with_prefix("orchestrator/run/")?))?;
        rows.into_iter()
            .map(|(_, text)| Ok(RunStateRecord::from_json(&text)?))
            .collect()
    }

    /// The worktree names every run record still references: the workers of
    /// its open contracts, every worker a worktree was recorded for (a replaced
    /// worker's too), and the worktree a contract inherited from a handed-back
    /// parent (named by path only; the parent is no longer open). Startup
    /// cleanup keeps these for a resume to reopen or continue in.
    pub fn referenced_worktrees(&self) -> Result<std::collections::BTreeSet<String>, StoreError> {
        Ok(self
            .all()?
            .iter()
            .flat_map(|r| r.open_contracts.iter())
            .flat_map(|c| {
                c.workers
                    .iter()
                    .map(|w| w.to_string())
                    .chain(c.worktrees.keys().cloned())
                    .chain(
                        c.workspace
                            .as_deref()
                            .and_then(|p| p.file_name())
                            .and_then(|n| n.to_str())
                            .map(str::to_owned),
                    )
                    .collect::<Vec<_>>()
            })
            .collect())
    }

    /// Read a run's record, validated.
    pub fn load(&self, run: &RunId) -> Result<Option<RunStateRecord>, StoreError> {
        let key = record_key(run);
        match self.with(|g| Ok(g.meta(&key)?))? {
            Some(t) => Ok(Some(RunStateRecord::from_json(&t)?)),
            None => Ok(None),
        }
    }

    /// Record the user's decision about a held or failed gate of a stopped or
    /// blocked run, before it is resumed: attributed to the user and kept in the
    /// gate's record. Resume then runs a retried gate again, or treats a waived
    /// one as passed.
    pub fn decide_gate(
        &self,
        run: &RunId,
        gate: &str,
        decision: crate::gates::GateDecision,
        note: &str,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let mut record = self
            .load(run)?
            .ok_or_else(|| StoreError::Decision(format!("no record for run `{run}`")))?;
        crate::gates::decide(
            &mut record.coordination.gates,
            gate,
            decision,
            "user",
            note,
            now_ms,
        )
        .map_err(|e| StoreError::Decision(e.to_string()))?;
        self.save(&record)
    }

    /// Mark a run as one a restart should resume.
    pub fn set_active(&self, run: &RunId) -> Result<(), StoreError> {
        let key = active_key(run);
        self.with(|g| Ok(g.set_meta(&key, "1")?))
    }

    /// Clear a run's marker (a finished run is not resumed).
    /// Fold the WAL into graph.db and truncate it (decision 0071): old page
    /// images of rewritten run records otherwise stay in the WAL file. Best effort; see `GraphStore::checkpoint_wal`.
    pub fn checkpoint_wal(&self) {
        let _ = self.with(|g| Ok(g.checkpoint_wal()?));
    }

    pub fn clear_active(&self, run: &RunId) -> Result<(), StoreError> {
        let key = active_key(run);
        self.with(|g| Ok(g.set_meta(&key, "")?))
    }
}

/// One run as stored, for a reader that is not the run's owner (the desktop
/// shell's graph overlay and `/status` without a hosted orchestrator).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRun {
    pub run: String,
    /// Marked for resume: not finished.
    pub active: bool,
    /// The record, or why it could not be read. A bad record is reported, never
    /// left out.
    pub record: Result<RunStateRecord, String>,
}

/// Every run record in `store`, read-only, in run-id order. Takes a plain
/// `GraphStore` so a read-only connection (`snapshot::open_existing`) serves.
pub fn read_runs(store: &GraphStore) -> Result<Vec<StoredRun>, StoreError> {
    let active: std::collections::BTreeSet<String> = store
        .meta_with_prefix("orchestrator/active/")?
        .into_iter()
        .filter(|(_, v)| v == "1")
        .filter_map(|(k, _)| k.strip_prefix("orchestrator/active/").map(str::to_owned))
        .collect();
    Ok(store
        .meta_with_prefix("orchestrator/run/")?
        .into_iter()
        .filter_map(|(k, v)| {
            let run = k.strip_prefix("orchestrator/run/")?.to_owned();
            Some(StoredRun {
                active: active.contains(&run),
                record: RunStateRecord::from_json(&v).map_err(|e| e.to_string()),
                run,
            })
        })
        .collect())
}

/// A run's directory of transcripts and trace.
#[derive(Clone, Debug)]
pub struct RunFiles {
    dir: PathBuf,
}

impl RunFiles {
    /// `<base>/<run id>/`, created.
    pub fn create(base: &Path, run: &RunId) -> Result<Self, StoreError> {
        let dir = base.join(run.as_str());
        std::fs::create_dir_all(dir.join("workers")).map_err(|source| StoreError::Io {
            path: dir.clone(),
            source,
        })?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A worker's transcript file.
    pub fn worker_transcript(&self, worker: &str) -> PathBuf {
        self.dir.join("workers").join(format!("{worker}.jsonl"))
    }

    /// The run's trace file.
    pub fn trace(&self) -> PathBuf {
        self.dir.join("trace.jsonl")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl RunStore {
        fn is_active(&self, run: &RunId) -> Result<bool, StoreError> {
            let key = active_key(run);
            self.with(|g| Ok(g.meta(&key)?.is_some_and(|s| s == "1")))
        }
    }

    #[test]
    fn a_record_round_trips_and_each_run_has_its_own_active_marker() {
        let dir = tempfile::tempdir().expect("dir");
        let graph = GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph");
        let store = RunStore::new(Arc::new(Mutex::new(graph)));
        let (r1, r2) = (RunId::new("r1"), RunId::new("r2"));
        assert!(!store.is_active(&r1).expect("active"));
        let record = RunStateRecord::new(r1.clone(), "do the thing", 5);
        store.save(&record).expect("save");
        store.set_active(&r1).expect("mark");
        store.set_active(&r2).expect("mark");
        assert_eq!(store.load(&r1).expect("load"), Some(record.clone()));
        store.clear_active(&r1).expect("clear");
        assert!(!store.is_active(&r1).expect("active"));
        assert!(store.is_active(&r2).expect("active"), "per-run markers");
        assert_eq!(store.load(&RunId::new("missing")).expect("load"), None);
    }

    /// Text is scrubbed at intake, and `save` stays a
    /// plain write (see its doc for the measured reason).
    #[test]
    fn save_stores_what_it_is_given_and_intake_is_where_text_is_scrubbed() {
        let dir = tempfile::tempdir().expect("dir");
        let graph = GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph");
        let store = RunStore::new(Arc::new(Mutex::new(graph)));
        let registered = "kgraf-run-store-registered-Fv4Gb6Hn8Jm0Kq2Lw";
        kgraf_secrets::register_exact_secret("test-run-store-secret", registered);
        // Through intake: scrubbed before it is ever saved.
        let record = RunStateRecord::new(RunId::new("r1"), format!("run this: {registered}"), 5);
        store.save(&record).expect("save");
        let loaded = store
            .load(&record.run)
            .expect("load")
            .expect("record")
            .original_instruction;
        assert!(!loaded.contains(registered), "{loaded}");
        assert!(
            loaded.contains("[redacted:test-run-store-secret;"),
            "{loaded}"
        );
        // Past intake: `save` writes what it is given, so the hot path pays
        // no scan (see its doc).
        let mut raw = record.clone();
        raw.original_instruction = format!("set directly: {registered}");
        store.save(&raw).expect("save");
        let loaded = store.load(&raw.run).expect("load").expect("record");
        assert!(loaded.original_instruction.contains(registered));
    }

    #[test]
    fn a_reader_sees_every_run_its_marker_and_a_bad_record_as_an_error() {
        let dir = tempfile::tempdir().expect("dir");
        let graph = GraphStore::open(&dir.path().join("graph.db"), dir.path()).expect("graph");
        let store = RunStore::new(Arc::new(Mutex::new(graph)));
        let r1 = RunId::new("r1");
        store
            .save(&RunStateRecord::new(r1.clone(), "one", 1))
            .expect("save");
        store
            .save(&RunStateRecord::new(RunId::new("r2"), "two", 2))
            .expect("save");
        store.set_active(&r1).expect("mark");
        {
            let g = store.graph().lock().expect("lock");
            g.set_meta("orchestrator/run/r3", "{not json").expect("raw");
            // Not a run record, though its key shares a prefix with the markers.
            g.set_meta("orchestrator/runner", "x").expect("raw");
        }
        let runs = read_runs(&store.graph().lock().expect("lock")).expect("read");
        let names: Vec<&str> = runs.iter().map(|r| r.run.as_str()).collect();
        assert_eq!(names, ["r1", "r2", "r3"]);
        assert!(runs[0].active && !runs[1].active);
        assert_eq!(
            runs[1].record.as_ref().expect("r2").original_instruction,
            "two"
        );
        assert!(runs[2].record.is_err(), "reported, not dropped");
    }

    #[test]
    fn an_invalid_record_is_not_written() {
        let dir = tempfile::tempdir().expect("dir");
        let graph = GraphStore::open_in_memory(dir.path()).expect("graph");
        let store = RunStore::new(Arc::new(Mutex::new(graph)));
        let record = RunStateRecord::new(RunId::new("r1"), "  ", 5);
        assert!(store.save(&record).is_err());
        assert_eq!(store.load(&RunId::new("r1")).expect("load"), None);
    }
}
