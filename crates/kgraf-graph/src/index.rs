//! Indexing and incremental reindexing.
//!
//! The load-bearing detail is the change detector. **A file is reindexed when
//! its content hash differs**, not when its modification time or size differs.
//! The reference implementation compares only time and size while writing an
//! unused hash column, so a change preserving both is missed, and agents
//! writing files rapidly, or a tool that restores timestamps, is precisely that
//! workload. `mtime_ns` and `size_bytes` are still recorded, for diagnosis, and
//! deliberately never consulted here.
//!
//! The second property is that a reindex touches **only the changed subgraph**.
//! Because the change log is written by database triggers, that claim is
//! checkable rather than asserted: reindex after a one-file edit and count the
//! log rows.

use crate::chunk::{self, Chunk, NodeKind};
use crate::error::{GraphError, Result};
use crate::lang::Language;
use crate::store::{EdgeKind, FileRecord, GraphStore, NodeRecord, WriteBatch};
use kgraf_core::ids::stable_id;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// What one index pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexReport {
    /// Files seen in the tree and eligible for indexing.
    pub files_seen: u64,
    /// Files indexed for the first time.
    pub files_added: u64,
    /// Files whose content hash changed and which were reindexed.
    pub files_changed: u64,
    /// Files whose content hash matched, and which were therefore left alone.
    pub files_unchanged: u64,
    /// Files that were indexed and have since been deleted from the tree.
    pub files_removed: u64,
    /// Nodes in the index after the pass.
    pub nodes: u64,
    /// Edges in the index after the pass.
    pub edges: u64,
    /// Lines of indexed source after the pass.
    pub lines: u64,
    /// Change-log rows this pass appended. The measure of how incremental the
    /// pass actually was.
    pub change_log_rows: u64,
    /// Files skipped because their bytes are not valid UTF-8.
    pub skipped_not_utf8: u64,
    /// Files skipped for exceeding the size ceiling, by a walk or by
    /// [`Indexer::index_paths`]. A file that was indexed before it grew past the
    /// ceiling leaves the index.
    pub skipped_too_large: u64,
    /// Files whose metadata could not be read during a walk. Their indexed rows
    /// are kept as they were, not deleted, and the paths are named here.
    pub metadata_errors: Vec<String>,
    /// Symbolic links to files in an indexed language. A walk does not follow
    /// them, and a link never deletes an indexed row; the paths are named here.
    pub symlinks_kept: Vec<String>,
    /// Files whose bytes changed to something that is not valid UTF-8. Their
    /// stale rows were removed rather than kept beside the new content.
    pub non_utf8_removed: Vec<String>,
    /// Call sites whose callee name matches no indexed definition. Expected and
    /// large: standard-library and third-party calls dominate real code.
    pub calls_unresolved: u64,
    /// Call sites whose callee name matches more definitions than
    /// [`MAX_CALL_FANOUT`], and which are therefore recorded as unresolved.
    ///
    /// Reported rather than swallowed, because it is the figure that says how
    /// much of the call graph name-based resolution cannot reach.
    pub calls_too_ambiguous: u64,
    /// Call sites whose callee name is defined only in another language family
    /// ([`Language::family`]), so no edge was written; counted within
    /// [`IndexReport::calls_unresolved`] too. Before this gate a Python call to
    /// `make()` resolved to a Rust `fn make`.
    pub calls_cross_language: u64,
    /// Work call resolution did: one scope lookup per resolvable call site plus
    /// one per candidate kept. Grows linearly with call sites, not with the
    /// product of call sites and same-name definitions.
    pub call_work: u64,
    /// Write transactions the file loop committed. Files are written in batches
    /// rather than one commit each; see [`FILES_PER_WRITE_BATCH`].
    pub write_batches: u64,
    /// Languages whose extractor changed since their files were indexed
    /// ([`Language::extractor_stamp`]), with how many files each had: those files
    /// are re-derived by this pass (or, for [`Indexer::index_paths`], the next
    /// walk) although their bytes did not change. Reported, never silent.
    pub extractor_changed: Vec<ExtractorChange>,
    /// Cached layout positions this pass moved: new nodes leaving their seed,
    /// nodes whose call depth changed, and clusters that crossed a population
    /// bucket ([`crate::layout::cluster_radius`]).
    pub layout_rows_moved: u64,
    /// Conversation files a walk left to the message bus
    /// ([`Indexer::with_bus_owned_dirs`]): present in the tree and not indexed
    /// from it, or absent from it and kept. Reported, never silent.
    pub bus_owned_skipped: u64,
    /// Directories not walked because they hold their own `.git` entry: nested
    /// repositories and git worktrees (agent worktree folders are the common case,
    /// each a full copy of the repository that would count every definition once
    /// per copy). Named rather than counted by file, because counting their files
    /// would mean walking what was skipped. See [`Indexer::with_nested_repositories`].
    pub nested_repositories_skipped: Vec<String>,
    /// Directories not walked because they are excluded (`target` and
    /// `node_modules` at any depth; `build`, `dist` and the like only at the
    /// root), relative to the root, so a reader that must skip exactly what the
    /// index skipped can.
    pub excluded_dirs_skipped: Vec<String>,
}

/// One language whose indexed files must be re-derived.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractorChange {
    pub language: String,
    /// Files of that language whose rows came from another extractor.
    pub files: u64,
    /// Whether a stamp was recorded before. False for an index from before
    /// stamps: nothing is known to have changed, it simply was never recorded.
    pub had_stamp: bool,
}

impl ExtractorChange {
    /// The line an operator reads.
    pub fn describe(&self) -> String {
        if self.had_stamp {
            format!(
                "{} {} file(s) re-derived: the {} extractor changed since they were indexed",
                self.files, self.language, self.language
            )
        } else {
            format!(
                "{} {} file(s) re-derived: no extractor stamp was recorded for them",
                self.files, self.language
            )
        }
    }
}

impl IndexReport {
    fn absorb_calls(&mut self, counts: &CallCounts) {
        self.calls_unresolved = counts.unresolved;
        self.calls_too_ambiguous = counts.too_ambiguous;
        self.calls_cross_language = counts.cross_language;
        self.call_work = counts.examined;
    }
}

/// The first line of every file the message bus writes (0026): what marks a
/// file under a bus-owned directory as the bus's rather than the user's.
pub const BUS_FILE_TITLE: &str = "# Conversation: ";

/// A file's rows, as the indexer produces them: node records, the edges derivable
/// from that file alone, and its recorded call sites as (caller node id, callee
/// name).
type FileRows = (
    Vec<NodeRecord>,
    Vec<(String, String, EdgeKind, i64)>,
    Vec<(String, String)>,
);

/// What one walk found.
#[derive(Default)]
struct Walked {
    /// Eligible files as (repository-relative path, bytes, mtime), or the error
    /// that made one unreadable.
    files: Vec<Result<(String, Vec<u8>, i64)>>,
    /// Nested repositories skipped, relative to the root.
    nested: Vec<String>,
    /// Excluded directories skipped, relative to the root.
    excluded: Vec<String>,
    /// Paths the walk could not judge, which must stay in the index.
    kept: Vec<String>,
    /// Directories the walk could not read, relative to the root. Everything
    /// indexed under one stays: an unreadable directory is not an empty one.
    unreadable_dirs: Vec<String>,
    /// Files over the size ceiling.
    too_large: u64,
    /// Files whose metadata could not be read.
    metadata_errors: Vec<String>,
    /// Symbolic links in an indexed language.
    symlinks: Vec<String>,
}

/// How many definitions a callee name may match before the call site is treated
/// as unresolved rather than as an edge to each candidate.
///
/// Weighting rather than thresholding is the general principle (decision 0020),
/// and this is a deliberate, measured exception to it, on tractability grounds
/// that the principle's own record admits. Names like `new`, `len` and `from`
/// have thousands of definitions in a real repository: on a 6,589-file checkout,
/// writing an edge per candidate produced 6.8 million call edges against 100,769
/// nodes, which is a hundred times the reference implementation's density and
/// carries no information a consumer can use. An edge set that says a call might
/// go to any of three thousand places is not a weaker answer than one that says
/// it is unresolved; it is the same answer, more expensively. The count of call
/// sites dropped this way is reported in [`IndexReport::calls_too_ambiguous`] so
/// the exception is visible rather than silent.
const MAX_CALL_FANOUT: usize = 8;

/// How many file writes one write transaction holds during a pass. A commit per
/// file rewrote the same index pages for every file, and its cost grew with the
/// size of the index; a batch pays that once per batch. The bound keeps the write
/// lock, taken at the first write of a batch, from being held across the whole
/// tree: other writers (a lease grant, the bus) wait at most one batch.
pub const FILES_PER_WRITE_BATCH: usize = 256;

/// Groups a pass's file writes into transactions of [`FILES_PER_WRITE_BATCH`]. A
/// limit of zero writes each file in its own transaction. The transaction opens
/// at the first write, not at the first file, so a pass over an unchanged tree
/// never takes the write lock for its reads.
struct Batch<'a> {
    store: &'a GraphStore,
    limit: usize,
    open: Option<WriteBatch<'a>>,
    writes: usize,
    commits: u64,
}

impl<'a> Batch<'a> {
    fn new(store: &'a GraphStore, limit: usize) -> Self {
        Self {
            store,
            limit,
            open: None,
            writes: 0,
            commits: 0,
        }
    }

    /// Open the transaction if batching and none is open. Call before a write.
    fn ensure(&mut self) -> Result<()> {
        if self.limit > 0 && self.open.is_none() {
            self.open = Some(self.store.begin_batch()?);
        }
        Ok(())
    }

    /// Count one finished write; commit when the batch is full.
    fn wrote(&mut self) -> Result<()> {
        self.writes += 1;
        if self.writes >= self.limit {
            self.finish()?;
        }
        Ok(())
    }

    /// Commit what is open.
    fn finish(&mut self) -> Result<()> {
        self.writes = 0;
        if let Some(open) = self.open.take() {
            open.commit()?;
            self.commits += 1;
        }
        Ok(())
    }
}

/// `meta` key prefix of the in-progress markers. Each pass writes its own key,
/// `index_in_progress/<token>`, whose value is its start time in milliseconds
/// since the epoch, and clears only that key, so two passes that overlap (a
/// `kgraf index` process and the message bus's `index_paths`) never clear
/// each other's marker. Readers honor the bare key as well as any per-pass key
/// and report the earliest start.
pub const INDEX_IN_PROGRESS: &str = "index_in_progress";

/// `meta` key prefix of each language's recorded extractor stamp.
pub const EXTRACTOR_STAMP: &str = "extractor_stamp";

/// A pass's own marker key: the process id, the time and a process-wide counter.
fn pass_marker_key() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{INDEX_IN_PROGRESS}/{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Holds one pass's marker. `clear` on success; `Drop` on an error path, best
/// effort, since the pass's own error is the one to report. A crash leaves the
/// marker; readers bound how long they honor it (`snapshot`'s callers).
struct InProgress<'a> {
    store: &'a GraphStore,
    key: String,
}

impl<'a> InProgress<'a> {
    fn start(store: &'a GraphStore) -> Result<Self> {
        let key = pass_marker_key();
        store.set_meta(&key, &crate::snapshot::now_ms().to_string())?;
        Ok(Self { store, key })
    }

    fn clear(self) -> Result<()> {
        let result = self.store.clear_meta(&self.key);
        std::mem::forget(self);
        result
    }
}

impl Drop for InProgress<'_> {
    fn drop(&mut self) {
        // Already failing; a second error here would hide the first.
        let _ = self.store.clear_meta(&self.key);
    }
}

/// Walks a working tree and keeps an index in step with it.
pub struct Indexer<'store> {
    store: &'store GraphStore,
    /// Directories that are never walked. Build output and vendored
    /// dependencies are not the project's code and would dominate the field. See
    /// [`Indexer::with_excluded_dirs`] for how an entry matches.
    excluded_dirs: Vec<String>,
    /// Files above this many bytes are skipped. A generated file megabytes long
    /// is not something a person navigates or an agent should read.
    max_file_bytes: u64,
    /// Where [`Indexer::index_paths`] reads file content, when not the store's
    /// root. See [`Indexer::with_source_root`].
    source_root: Option<std::path::PathBuf>,
    /// Directories (relative to the root) whose files only a named-path pass
    /// indexes: a walk neither indexes nor removes them. See
    /// [`Indexer::with_bus_owned_dirs`].
    bus_owned: Vec<String>,
    /// Whether directories with their own `.git` entry are walked.
    include_nested: bool,
}

/// Excluded directory names that match at any depth: tool output and tool
/// state that is never source. Every other bare name in the exclusion list
/// (`build`, `dist`, `venv`, ...) matches at the root only, because those names
/// are also ordinary source directories.
pub const ANY_DEPTH_EXCLUDED: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".next",
    ".venv",
];

/// The orchestrator's conversation files (decision 0026). The message bus is
/// their sole writer and their sole indexer.
pub const BUS_OWNED_DIRS: &[&str] = &["docs/agent-comms"];

impl<'store> Indexer<'store> {
    /// An indexer with the default exclusions.
    pub fn new(store: &'store GraphStore) -> Self {
        Self {
            store,
            excluded_dirs: [
                ".git",
                "target",
                "node_modules",
                "dist",
                "build",
                ".venv",
                "venv",
                "__pycache__",
                ".mypy_cache",
                ".pytest_cache",
                ".next",
                ".cargo",
                ".kgraf",
                // The state directory under the former name (decision 0062),
                // in a project not yet migrated: never indexed either.
                ".haustoria",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
            max_file_bytes: 2 * 1024 * 1024,
            source_root: None,
            bus_owned: BUS_OWNED_DIRS.iter().map(|d| (*d).to_owned()).collect(),
            include_nested: false,
        }
    }

    /// Replace the directories only the message bus indexes (default
    /// [`BUS_OWNED_DIRS`]). The bus writes conversation files in a worktree of
    /// the run's integration branch and indexes them from there
    /// ([`Indexer::with_source_root`]); the user's checkout gets them only when
    /// the user merges that branch, and then possibly an older copy. A walk
    /// of the checkout would delete the messages' nodes, or replace them with
    /// the older copy's, and every mailbox reference to them would dangle.
    /// Those files are append-only and never pruned (0028), so a walk leaves
    /// them to the bus.
    pub fn with_bus_owned_dirs(mut self, dirs: Vec<String>) -> Self {
        self.bus_owned = dirs;
        self
    }

    /// Whether a directory is excluded: `rel` is its path relative to the root,
    /// `depth` its depth below the root (1 for a top-level directory).
    fn is_excluded(&self, rel: &str, name: &str, depth: usize) -> bool {
        self.excluded_dirs.iter().any(|entry| {
            if entry.contains('/') {
                rel == entry.trim_end_matches('/')
            } else {
                entry == name && (depth == 1 || ANY_DEPTH_EXCLUDED.contains(&name))
            }
        })
    }

    /// An indexed file the bus wrote: its title heading is the bus's.
    fn is_bus_file(&self, rel: &str) -> Result<bool> {
        let title = BUS_FILE_TITLE.trim_start_matches('#').trim();
        Ok(self
            .store
            .nodes_in_file(rel)?
            .iter()
            .any(|n| n.name.starts_with(title)))
    }

    fn bus_owned(&self, rel: &str) -> bool {
        self.bus_owned.iter().any(|d| {
            rel.strip_prefix(d.as_str())
                .is_some_and(|r| r.starts_with('/'))
        })
    }

    /// Read the files [`Indexer::index_paths`] indexes from `root` instead of
    /// the store's root, keeping their store-relative paths. For a checkout of
    /// the same repository at another commit: the orchestrator's message bus
    /// writes conversation files in a worktree of the run's integration branch,
    /// never in the user's checkout, and indexes them from there. A later full
    /// walk of the store's root sees what that root holds.
    pub fn with_source_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.source_root = Some(root.into());
        self
    }

    /// Walk into nested repositories and worktrees (directories with their own
    /// `.git` entry) instead of skipping them. Off by default.
    pub fn with_nested_repositories(mut self, include: bool) -> Self {
        self.include_nested = include;
        self
    }

    /// The directories a walk never enters.
    pub fn excluded_dirs(&self) -> &[String] {
        &self.excluded_dirs
    }

    /// Replace the excluded directory list. An entry with a `/` is a path
    /// relative to the root and matches that directory only. A bare name matches
    /// a top-level directory of that name, and also any deeper one when it is a
    /// name only tools write ([`ANY_DEPTH_EXCLUDED`]): `src/build/` is source,
    /// `crates/x/target/` is not.
    pub fn with_excluded_dirs(mut self, dirs: Vec<String>) -> Self {
        self.excluded_dirs = dirs;
        self
    }

    /// Set the per-file size ceiling.
    pub fn with_max_file_bytes(mut self, bytes: u64) -> Self {
        self.max_file_bytes = bytes;
        self
    }

    /// Bring the index into step with the working tree.
    ///
    /// Safe to call repeatedly: an unchanged tree produces a report with zero
    /// added, changed and removed files, and appends nothing to the change log.
    pub fn index(&self) -> Result<IndexReport> {
        // Mark the pass so a reader can tell a quiescent index from one mid-pass:
        // between a file's rewrite and call resolution, the file's incoming
        // cross-file call edges are briefly absent. A marker rather than one
        // transaction around the pass, because every file write is already its own
        // transaction (`replace_file_nodes`) and SQLite does not nest them. `meta`
        // has no change-log trigger, so the marker itself is not an event.
        let guard = InProgress::start(self.store)?;
        let report = self.index_pass()?;
        guard.clear()?;
        Ok(report)
    }

    /// Compare each language's [`Language::extractor_stamp`] with the one recorded
    /// in `meta` (`extractor_stamp/<language>`). On a difference, or with none
    /// recorded (an index from before stamps), clear the content hash of that
    /// language's files and record the new stamp, in one transaction: the pass that
    /// follows sees those files as changed and re-derives their rows from the same
    /// bytes. Other languages' files, and the lease and work tables, are untouched.
    /// A crash after the commit leaves the hashes cleared, so the next pass still
    /// re-derives them.
    fn apply_extractor_stamps(&self, report: &mut IndexReport) -> Result<()> {
        for language in Language::all() {
            let stamp = language.extractor_stamp()?;
            let key = format!("{EXTRACTOR_STAMP}/{}", language.as_str());
            let recorded = self.store.meta(&key)?;
            if recorded.as_deref() == Some(stamp.as_str()) {
                continue;
            }
            let tx = self.store.conn().unchecked_transaction()?;
            let cleared = tx.execute(
                "UPDATE files SET content_hash = '' WHERE language = ?1 AND content_hash <> ''",
                rusqlite::params![language.as_str()],
            )? as u64;
            self.store.set_meta(&key, &stamp)?;
            tx.commit()?;
            if cleared > 0 {
                report.extractor_changed.push(ExtractorChange {
                    language: language.as_str().to_owned(),
                    files: cleared,
                    had_stamp: recorded.is_some(),
                });
            }
        }
        Ok(())
    }

    fn index_pass(&self) -> Result<IndexReport> {
        let watermark_before = self.store.change_watermark()?;
        let mut report = IndexReport::default();
        self.apply_extractor_stamps(&mut report)?;
        let root = self.store.root().to_path_buf();

        let mut seen_paths: Vec<String> = Vec::new();
        let mut walked = self.walk(&root);
        report.nested_repositories_skipped = walked.nested;
        report.excluded_dirs_skipped = walked.excluded;
        report.skipped_too_large += walked.too_large;
        // A path the walk could not judge (metadata error, symbolic link) stays
        // in the index as it was: not seeing a file is not the file leaving.
        seen_paths.extend(walked.kept.iter().cloned());
        let unreadable_dirs = std::mem::take(&mut walked.unreadable_dirs);
        report.metadata_errors = walked.metadata_errors;
        report.symlinks_kept = walked.symlinks;
        // One unreadable file or directory must not skip removal, call resolution
        // and layout for the rest of the tree. The pass finishes and then reports
        // the first walk error (fail loud). An unreadable file stays in the index.
        let mut first_walk_error: Option<GraphError> = None;
        let mut batch = Batch::new(self.store, FILES_PER_WRITE_BATCH);
        for entry in walked.files {
            let (rel_path, bytes, mtime_ns) = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    if let GraphError::Io { path, .. } = &error {
                        seen_paths.push(path.clone());
                    }
                    first_walk_error.get_or_insert(error);
                    continue;
                }
            };
            // Only the bus's own files are left to it; a file the user put in
            // the same directory is indexed like any other.
            if self.bus_owned(&rel_path) && bytes.starts_with(BUS_FILE_TITLE.as_bytes()) {
                report.bus_owned_skipped += 1;
                continue;
            }
            report.files_seen += 1;
            seen_paths.push(rel_path.clone());
            self.index_one(&rel_path, bytes, mtime_ns, None, &mut batch, &mut report)?;
        }

        // Files that left the tree.
        seen_paths.sort();
        for indexed in self.store.file_paths()? {
            if seen_paths.binary_search(&indexed).is_err() {
                let under_unreadable = unreadable_dirs.iter().any(|dir| {
                    dir.is_empty()
                        || indexed
                            .strip_prefix(dir.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                });
                if under_unreadable {
                    continue;
                }
                if self.bus_owned(&indexed) && self.is_bus_file(&indexed)? {
                    // Either in the tree and left to the bus (counted above),
                    // or absent from it: kept, and counted here.
                    if !root.join(&indexed).is_file() {
                        report.bus_owned_skipped += 1;
                    }
                    continue;
                }
                batch.ensure()?;
                self.store.delete_file(&indexed)?;
                batch.wrote()?;
                report.files_removed += 1;
            }
        }
        batch.finish()?;
        report.write_batches = batch.commits;

        // Cross-file call resolution runs after every file is present, since a
        // call can precede its callee's file in walk order. It is skipped
        // entirely when nothing changed: rewriting the call edges would append
        // to the change log, and an unchanged tree must produce no events.
        let tree_changed = report.files_added + report.files_changed + report.files_removed > 0;
        if tree_changed {
            let counts = self.resolve_calls()?;
            report.absorb_calls(&counts);
            // Depth needs the resolved call edges, so the layout refresh runs
            // after resolution and, like it, only when the tree changed.
            report.layout_rows_moved =
                crate::layout::cache::refresh_layout(self.store)?.moved as u64;
        }

        report.nodes = self.store.node_count()?;
        report.edges = self.store.edge_count()?;
        report.lines = self.store.line_total()?;
        report.change_log_rows = (self.store.change_watermark()? - watermark_before).max(0) as u64;
        if let Some(error) = first_walk_error {
            return Err(error);
        }
        Ok(report)
    }

    /// Index the named files only, without walking the tree: for a writer that
    /// knows exactly what it wrote, such as the orchestrator's message bus
    /// appending to a conversation file (decision 0026). Paths are
    /// repository-relative. A path that no longer exists leaves the index; a path
    /// in no indexed language is refused, not skipped (fail loud).
    ///
    /// The per-file step is the one [`Indexer::index`] runs, so a file indexed
    /// here and one indexed by a walk produce identical rows. Call resolution and
    /// the layout refresh follow when anything changed, as they do after a walk,
    /// and the pass carries the same in-progress marker.
    ///
    /// A file over the size ceiling is treated as the walk treats it: not
    /// indexed, and removed from the index if it was there
    /// ([`IndexReport::skipped_too_large`] counts it here).
    pub fn index_paths(&self, paths: &[String]) -> Result<IndexReport> {
        let guard = InProgress::start(self.store)?;
        let report = self.index_paths_pass(paths)?;
        guard.clear()?;
        Ok(report)
    }

    fn index_paths_pass(&self, paths: &[String]) -> Result<IndexReport> {
        let watermark_before = self.store.change_watermark()?;
        let mut report = IndexReport::default();
        // Extractor stamps are applied by the full walk only: clearing hashes here
        // would blank every file of the language while only the named files are
        // re-derived, leaving the rest stale until the next walk.
        let root = self
            .source_root
            .clone()
            .unwrap_or_else(|| self.store.root().to_path_buf());
        for rel in paths {
            if Language::from_path(rel).is_none() {
                return Err(GraphError::Io {
                    path: rel.clone(),
                    source: std::io::Error::other("not a file in an indexed language"),
                });
            }
            let full = root.join(rel);
            if std::fs::metadata(&full).is_ok_and(|m| m.len() > self.max_file_bytes) {
                report.skipped_too_large += 1;
                if self.store.file(rel)?.is_some() {
                    self.store.delete_file(rel)?;
                    report.files_removed += 1;
                }
                continue;
            }
            match std::fs::read(&full) {
                Ok(bytes) => {
                    report.files_seen += 1;
                    let mtime_ns = std::fs::metadata(&full)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);
                    self.index_one(
                        rel,
                        bytes,
                        mtime_ns,
                        self.foreign_root(),
                        &mut Batch::new(self.store, 0),
                        &mut report,
                    )?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if self.store.file(rel)?.is_some() {
                        self.store.delete_file(rel)?;
                        report.files_removed += 1;
                    }
                }
                Err(source) => {
                    return Err(GraphError::Io {
                        path: rel.clone(),
                        source,
                    });
                }
            }
        }
        // Markdown has no call sites and defines nothing callable, so a pass over
        // Markdown alone cannot change a call edge: resolution, which reads every
        // call site in the project, is skipped. The layout still refreshes.
        let only_prose = paths
            .iter()
            .all(|p| Language::from_path(p) == Some(Language::Markdown));
        if report.files_added + report.files_changed + report.files_removed > 0 && !only_prose {
            let counts = self.resolve_calls()?;
            report.absorb_calls(&counts);
        }
        if report.files_added + report.files_changed + report.files_removed > 0 {
            report.layout_rows_moved =
                crate::layout::cache::refresh_layout(self.store)?.moved as u64;
        }
        report.nodes = self.store.node_count()?;
        report.edges = self.store.edge_count()?;
        report.lines = self.store.line_total()?;
        report.change_log_rows = (self.store.change_watermark()? - watermark_before).max(0) as u64;
        Ok(report)
    }

    /// The source root when it is a tree other than the store's root.
    fn foreign_root(&self) -> Option<&Path> {
        self.source_root
            .as_deref()
            .filter(|r| *r != self.store.root())
    }

    /// One file's step: compare its content hash and, only when it differs,
    /// rewrite its rows. `foreign_root` is the tree the bytes came from when it
    /// is not the store's root; it is recorded for `GraphStore::source_of`.
    fn index_one(
        &self,
        rel_path: &str,
        bytes: Vec<u8>,
        mtime_ns: i64,
        foreign_root: Option<&Path>,
        batch: &mut Batch<'_>,
        report: &mut IndexReport,
    ) -> Result<()> {
        let content_hash = blake3::hash(&bytes).to_hex().to_string();
        let existing = self.store.file(rel_path)?;

        // The whole decision, in one comparison. Not mtime. Not size.
        if let Some(existing) = &existing
            && existing.content_hash == content_hash
        {
            // The store's root holds the indexed content, so a root recorded by
            // an earlier pass from a worktree is no longer needed. A pass from
            // another root leaves it alone: it says nothing about the checkout.
            if foreign_root.is_none() {
                self.store.clear_file_source_root(rel_path)?;
            }
            report.files_unchanged += 1;
            return Ok(());
        }

        let source = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                report.skipped_not_utf8 += 1;
                // The bytes changed, so the rows describe content that is gone.
                // Keeping them (and the old hash) would serve the old text's
                // nodes for a file that no longer parses; remove them and say so.
                if existing.is_some() {
                    batch.ensure()?;
                    self.store.delete_file(rel_path)?;
                    batch.wrote()?;
                    report.files_removed += 1;
                    report.non_utf8_removed.push(rel_path.to_owned());
                }
                return Ok(());
            }
        };
        let language = Language::from_path(rel_path);
        let file_id = stable_id("file", &[rel_path]);
        let file = FileRecord {
            id: file_id.clone(),
            path: rel_path.to_owned(),
            content_hash,
            size_bytes: source.len() as u64,
            mtime_ns,
            language,
            line_count: source.lines().count() as u32,
        };
        let chunks = match language {
            Some(language) => chunk::chunk_source(rel_path, language, &source)?,
            // A file in a language with no specification row is still a node,
            // so it is findable and drawable. It is simply not chunked.
            None => Vec::new(),
        };
        let (nodes, edges, call_sites) = self.build_records(&file, &chunks);
        batch.ensure()?;
        self.store
            .replace_file_nodes_from(&file, &nodes, &edges, &call_sites, foreign_root)?;
        batch.wrote()?;

        if existing.is_some() {
            report.files_changed += 1;
        } else {
            report.files_added += 1;
        }
        Ok(())
    }

    /// Eligible files in the tree, plus what the walk skipped or could not judge.
    fn walk(&self, root: &Path) -> Walked {
        let mut walked = Walked::default();
        let mut nested = Vec::new();
        let mut excluded = Vec::new();
        let relative = |p: &Path| {
            p.strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/")
        };
        let walker = walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                let name = entry.file_name().to_string_lossy();
                if entry.file_type().is_dir() {
                    let rel = relative(entry.path());
                    if self.is_excluded(&rel, name.as_ref(), entry.depth()) {
                        excluded.push(rel);
                        return false;
                    }
                    // A directory with its own `.git` (a directory in a nested
                    // clone, a file in a git worktree) is another checkout.
                    if !self.include_nested && entry.path().join(".git").exists() {
                        nested.push(rel);
                        return false;
                    }
                }
                true
            });
        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    // A directory that cannot be walked is reported, not
                    // silently treated as empty (fail loud): a missing directory
                    // reading as "nothing yet" is one of the founding failures.
                    // The path is stored relative, like every indexed path, and
                    // the directory's indexed files are kept.
                    let path = error
                        .path()
                        .map(&relative)
                        .unwrap_or_else(|| "<unknown>".to_owned());
                    if error.path().is_some() {
                        walked.unreadable_dirs.push(path.clone());
                    }
                    walked.files.push(Err(GraphError::Io {
                        path,
                        source: error
                            .into_io_error()
                            .unwrap_or_else(|| std::io::Error::other("directory walk failed")),
                    }));
                    continue;
                }
            };
            let rel = match entry.path().strip_prefix(root) {
                Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            if entry.path_is_symlink() {
                // Not followed. A link in an indexed language is named, and any
                // row already indexed under its path is kept.
                if Language::from_path(&rel).is_some() {
                    walked.kept.push(rel.clone());
                    walked.symlinks.push(rel);
                }
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            // Only files in a known language, plus nothing else. An index of
            // every text file in a tree is noise, not a graph.
            if Language::from_path(&rel).is_none() {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => {
                    walked.kept.push(rel.clone());
                    walked.metadata_errors.push(rel);
                    continue;
                }
            };
            if metadata.len() > self.max_file_bytes {
                walked.too_large += 1;
                continue;
            }
            let bytes = match std::fs::read(entry.path()) {
                Ok(bytes) => bytes,
                Err(source) => {
                    walked.files.push(Err(GraphError::Io { path: rel, source }));
                    continue;
                }
            };
            let mtime_ns = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            walked.files.push(Ok((rel, bytes, mtime_ns)));
        }
        nested.sort();
        excluded.sort();
        walked.nested = nested;
        walked.excluded = excluded;
        walked
    }

    /// Turn a file's chunks into node rows plus the edges derivable from one
    /// file alone. Cross-file `calls` edges are added later by
    /// [`Indexer::resolve_calls`].
    fn build_records(&self, file: &FileRecord, chunks: &[Chunk]) -> FileRows {
        let mut nodes = Vec::with_capacity(chunks.len() + 1);
        let mut edges = Vec::new();
        let mut call_sites = Vec::new();

        // Every file is a node, so a file with no chunkable contents is still
        // findable and drawable.
        let file_node_id = stable_id("node", &[&file.path, "file", &file.path, "0"]);
        nodes.push(NodeRecord {
            id: file_node_id.clone(),
            file_id: file.id.clone(),
            path: file.path.clone(),
            kind: NodeKind::File,
            name: file
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&file.path)
                .to_owned(),
            qualified_name: file.path.clone(),
            parent_id: None,
            start_line: 1,
            end_line: file.line_count.max(1),
            start_byte: 0,
            end_byte: file.size_bytes,
            signature: file.path.clone(),
            // A file's id is derived from its path alone, which is unique per file by
            // construction; there is no sibling for it to be confused with.
            identity_stable: true,
        });

        for chunk in chunks {
            let parent_id = match chunk.parent {
                Some(index) => chunks[index].id.clone(),
                None => file_node_id.clone(),
            };
            nodes.push(NodeRecord {
                id: chunk.id.clone(),
                file_id: file.id.clone(),
                path: file.path.clone(),
                kind: chunk.kind,
                name: chunk.name.clone(),
                qualified_name: chunk.qualified_name.clone(),
                parent_id: Some(parent_id.clone()),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                start_byte: chunk.start_byte as u64,
                end_byte: chunk.end_byte as u64,
                signature: chunk.signature.clone(),
                identity_stable: chunk.identity_stable,
            });
            edges.push((parent_id, chunk.id.clone(), EdgeKind::Contains, 1));
            for callee in &chunk.calls {
                call_sites.push((chunk.id.clone(), callee.clone()));
            }
        }
        (nodes, edges, call_sites)
    }

    /// Resolve recorded call sites to nodes, across the whole index.
    ///
    /// Resolution is by name, narrowed by proximity: a definition in the caller's
    /// own file wins, then one in the same directory, then the whole index. That
    /// ordering is what makes the answer useful without a real type resolver, and
    /// it is the cheap half of what a language server would do.
    ///
    /// Where a name still matches more than [`MAX_CALL_FANOUT`] definitions the
    /// call site is recorded as unresolved rather than as an edge to each
    /// candidate; see that constant for why, and the returned counts for how
    /// often it happens.
    ///
    /// Call sites are read from the `call_sites` table rather than by reparsing,
    /// so a reindex does not pay for a second full parse of the tree.
    fn resolve_calls(&self) -> Result<CallCounts> {
        let conn = self.store.conn();

        // A file's language family; `None` for a file in no indexed language (such
        // a file has no definitions and no call sites, so it never matters here).
        let family = |language: Option<String>| {
            language
                .as_deref()
                .and_then(Language::parse)
                .map(Language::family)
        };

        // name -> (node id, path, language family) for every definition a call
        // could reach.
        let definitions: BTreeMap<String, Vec<Definition>> = {
            let mut stmt = conn.prepare(
                "SELECT n.name, n.id, f.path, f.language FROM nodes n
                 JOIN files f ON f.id = n.file_id
                 WHERE n.kind IN
                   ('function','method','macro','class','struct','trait','interface')",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            let mut map: BTreeMap<String, Vec<Definition>> = BTreeMap::new();
            for row in rows {
                let (name, id, path, language) = row?;
                map.entry(name).or_default().push(Definition {
                    id,
                    path,
                    family: family(language),
                });
            }
            map
        };

        type Site = (String, String, String, Option<&'static str>);
        let sites: Vec<Site> = {
            let mut stmt = conn.prepare(
                "SELECT c.caller_id, c.callee_name, f.path, f.language
                 FROM call_sites c
                 JOIN nodes n ON n.id = c.caller_id
                 JOIN files f ON f.id = n.file_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            rows.map(|r| r.map(|(caller, callee, path, l)| (caller, callee, path, family(l))))
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        // Scope lookups by hash, built once per (name, family): a name defined in
        // thousands of files (`new`, `main`, a test helper) is looked up per call
        // site, and filtering its whole definition list per site made resolution
        // quadratic in the number of copies.
        let mut scopes: HashMap<(&str, &str), NameScope<'_>> = HashMap::new();
        for (name, all) in &definitions {
            for d in all {
                let Some(family) = d.family else { continue };
                let scope = scopes.entry((name.as_str(), family)).or_default();
                scope.all.push(d);
                scope.by_path.entry(d.path.as_str()).or_default().push(d);
                scope
                    .by_dir
                    .entry(crate::layout::directory_of(&d.path))
                    .or_default()
                    .push(d);
            }
        }

        let mut counts = CallCounts::default();
        let mut wanted: Vec<(String, String, i64)> = Vec::new();
        for (caller_id, callee_name, caller_path, caller_family) in &sites {
            if !definitions.contains_key(callee_name) {
                counts.unresolved += 1;
                continue;
            }
            // The language-family gate comes before scoping: a definition in
            // another family is not a candidate at any scope.
            let scope = caller_family.and_then(|f| scopes.get(&(callee_name.as_str(), f)));
            let Some(scope) = scope else {
                // Defined only in another language: counted, so the gate is
                // visible rather than silent (fail loud).
                counts.cross_language += 1;
                counts.unresolved += 1;
                continue;
            };
            // Narrowest non-empty scope wins.
            let caller_dir = crate::layout::directory_of(caller_path);
            let scoped: &[&Definition] = if let Some(same) = scope.by_path.get(caller_path.as_str())
            {
                same
            } else if let Some(same) = scope.by_dir.get(caller_dir) {
                same
            } else {
                &scope.all
            };
            counts.examined += 1;
            if scoped.len() > MAX_CALL_FANOUT {
                counts.too_ambiguous += 1;
                continue;
            }
            counts.examined += scoped.len() as u64;
            // Ambiguity that survives scoping is carried in the weight, not
            // discarded: 100 is an unambiguous resolution, 100/n an ambiguous one.
            let weight = (100 / scoped.len().max(1)) as i64;
            for Definition { id: target, .. } in scoped.iter().copied() {
                if target != caller_id {
                    wanted.push((caller_id.clone(), target.clone(), weight));
                }
            }
        }

        // Write only the difference. Deleting and reinserting every call edge made a
        // one-line edit in a 100,000-node tree append about 400,000 change-log rows,
        // which every tailer (the galaxy feed) then had to read; the diff appends
        // rows only for call edges that actually appeared, vanished or changed
        // weight. Later duplicates of an id win, as the old upsert did.
        let wanted: BTreeMap<String, (String, String, i64)> = wanted
            .into_iter()
            .map(|(src, dst, weight)| {
                (
                    stable_id("edge", &[&src, &dst, "calls"]),
                    (src, dst, weight),
                )
            })
            .collect();
        let existing: BTreeMap<String, i64> = {
            let mut stmt = conn.prepare("SELECT id, weight FROM edges WHERE kind = 'calls'")?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let tx = conn.unchecked_transaction()?;
        {
            let mut delete = tx.prepare("DELETE FROM edges WHERE id = ?1")?;
            for id in existing.keys().filter(|id| !wanted.contains_key(*id)) {
                delete.execute([id])?;
            }
            let mut insert = tx.prepare(
                "INSERT INTO edges(id, src, dst, kind, weight) VALUES (?1, ?2, ?3, 'calls', ?4)
                 ON CONFLICT(id) DO UPDATE SET weight = excluded.weight",
            )?;
            for (id, (src, dst, weight)) in &wanted {
                if existing.get(id) != Some(weight) {
                    insert.execute(rusqlite::params![id, src, dst, weight])?;
                }
            }
        }
        tx.commit()?;
        Ok(counts)
    }
}

/// A definition a call could resolve to.
struct Definition {
    id: String,
    path: String,
    family: Option<&'static str>,
}

/// The definitions of one name in one language family, grouped for scope lookup.
#[derive(Default)]
struct NameScope<'a> {
    all: Vec<&'a Definition>,
    by_path: HashMap<&'a str, Vec<&'a Definition>>,
    by_dir: HashMap<&'a str, Vec<&'a Definition>>,
}

/// The call sites resolution could not turn into edges.
#[derive(Default)]
struct CallCounts {
    /// Scope lookups plus candidates carried into edges: the work resolution did,
    /// a count that does not depend on machine load.
    examined: u64,
    unresolved: u64,
    too_ambiguous: u64,
    cross_language: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    struct Tree {
        dir: tempfile::TempDir,
    }

    impl Tree {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("tempdir"),
            }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn write(&self, rel: &str, contents: &str) {
            let full = self.dir.path().join(rel);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("mkdir");
            }
            fs::write(&full, contents).expect("write");
        }

        fn store(&self) -> GraphStore {
            GraphStore::open_in_memory(self.dir.path()).expect("store")
        }
    }

    /// The default, with nothing pre-excluded: a git worktree under `.claude/`
    /// (its `.git` is a file) and a nested clone (a `.git` directory) are skipped
    /// and named; an ordinary `.claude` directory without one is indexed.
    #[test]
    fn nested_checkouts_are_skipped_and_named_by_default() {
        let tree = Tree::new();
        tree.write("src/lib.rs", "fn a() {}\n");
        tree.write("agent-trees/agent-1/.git", "gitdir: /elsewhere\n");
        tree.write("agent-trees/agent-1/src/lib.rs", "fn a() {}\n");
        tree.write("vendor/clone/.git/HEAD", "ref: refs/heads/main\n");
        tree.write("vendor/clone/lib.rs", "fn c() {}\n");
        tree.write(
            "tools/.claude/hooks/hook.py",
            "def hook_main():\n    pass\n",
        );
        let store = tree.store();
        let report = Indexer::new(&store).index().expect("index");
        let files = store.file_paths().expect("files");
        assert_eq!(files, ["src/lib.rs", "tools/.claude/hooks/hook.py"]);
        assert_eq!(
            report.nested_repositories_skipped,
            ["agent-trees/agent-1", "vendor/clone"]
        );

        let store = tree.store();
        let report = Indexer::new(&store)
            .with_nested_repositories(true)
            .index()
            .expect("index");
        assert!(report.nested_repositories_skipped.is_empty());
        assert_eq!(store.file_paths().expect("files").len(), 4);
    }

    /// A changed extractor re-derives
    /// a file whose bytes did not change, and only the files of that language.
    #[test]
    fn a_changed_extractor_stamp_re_derives_that_languages_unchanged_files() {
        let tree = Tree::new();
        tree.write("src/lib.rs", "fn a() {}\nfn b() { a(); }\n");
        tree.write("tool/run.py", "def main():\n    pass\n");
        let store = tree.store();
        let indexer = Indexer::new(&store);
        let first = indexer.index().expect("first");
        assert!(
            first.extractor_changed.is_empty(),
            "a new index has nothing to re-derive"
        );
        let key = format!("{EXTRACTOR_STAMP}/rust");
        assert_eq!(
            store.meta(&key).expect("meta"),
            Some(Language::Rust.extractor_stamp().expect("stamp"))
        );

        // An extractor built from other rules: the recorded stamp differs.
        store.set_meta(&key, "an older extractor").expect("meta");
        let mut tail = crate::events::EventTail::from_now(&store).expect("tail");
        let second = indexer.index().expect("second");
        assert_eq!(
            second.extractor_changed,
            [ExtractorChange {
                language: "rust".into(),
                files: 1,
                had_stamp: true
            }]
        );
        assert!(
            second.extractor_changed[0]
                .describe()
                .contains("extractor changed")
        );
        assert_eq!(
            (second.files_changed, second.files_unchanged),
            (1, 1),
            "the Rust file is re-derived, the Python file is not"
        );
        let changes = tail.next_batch(&store, 1000).expect("batch");
        let rust_file = store.file("src/lib.rs").expect("file").expect("indexed").id;
        let python_file = store
            .file("tool/run.py")
            .expect("file")
            .expect("indexed")
            .id;
        let file_of = |c: &crate::events::Change| {
            c.payload
                .as_ref()
                .and_then(|p| p.get("file_id"))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        };
        let rewritten = changes
            .iter()
            .filter(|c| c.table == "nodes" && c.op.as_str() == "insert")
            .filter(|c| file_of(c).as_deref() == Some(rust_file.as_str()))
            .count();
        assert_eq!(
            rewritten, 3,
            "the file node and both functions were written again: {changes:?}"
        );
        assert!(
            changes
                .iter()
                .all(|c| file_of(c).as_deref() != Some(python_file.as_str())
                    && c.row_id != python_file),
            "nothing of the Python file was touched: {changes:?}"
        );

        // The stamp is recorded again, so the next pass does nothing.
        let third = indexer.index().expect("third");
        assert!(third.extractor_changed.is_empty());
        assert_eq!(third.change_log_rows, 0);

        // An index from before stamps says no stamp was recorded,
        // not that the extractor changed.
        store.clear_meta(&key).expect("clear");
        let fourth = indexer.index().expect("fourth");
        assert!(!fourth.extractor_changed[0].had_stamp);
        assert_eq!(
            fourth.extractor_changed[0].describe(),
            "1 rust file(s) re-derived: no extractor stamp was recorded for them"
        );

        // A named-path pass leaves stamps and hashes alone: it would re-derive
        // only the named files and leave the rest of the language blanked.
        store.set_meta(&key, "an older extractor").expect("meta");
        let named = indexer
            .index_paths(&["tool/run.py".to_owned()])
            .expect("paths");
        assert!(named.extractor_changed.is_empty());
        assert_eq!(
            store.meta(&key).expect("meta").as_deref(),
            Some("an older extractor")
        );
        let hash = store
            .file("src/lib.rs")
            .expect("file")
            .expect("row")
            .content_hash;
        assert!(!hash.is_empty(), "a named pass blanked an unnamed file");
        // The next walk owns the stamp.
        let walked = indexer.index().expect("walk");
        assert_eq!(walked.extractor_changed[0].language, "rust");
    }

    /// One unreadable file does not stop removal, call resolution or layout for
    /// the rest of the tree; the pass still reports the error afterwards.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_does_not_skip_the_rest_of_the_pass() {
        use std::os::unix::fs::PermissionsExt;
        let tree = Tree::new();
        tree.write("src/gone.rs", "fn gone() {}\n");
        tree.write("src/locked.rs", "fn locked() {}\n");
        let store = tree.store();
        Indexer::new(&store).index().expect("first");
        fs::remove_file(tree.path().join("src/gone.rs")).expect("rm");
        tree.write("src/caller.rs", "fn caller() { callee(); }\n");
        tree.write("src/callee.rs", "pub fn callee() {}\n");
        let locked = tree.path().join("src/locked.rs");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
        if fs::read(&locked).is_ok() {
            // Running with rights that ignore modes: nothing to prove here.
            return;
        }
        let result = Indexer::new(&store).index();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).expect("chmod back");
        assert!(matches!(result, Err(GraphError::Io { .. })), "{result:?}");
        assert!(
            store.file("src/gone.rs").expect("file").is_none(),
            "removal was skipped"
        );
        assert!(
            store.file("src/locked.rs").expect("file").is_some(),
            "an unreadable file left the index"
        );
        let caller = store.resolve_symbol("caller").expect("resolve")[0].clone();
        let neighbors = store.neighbors(&caller.id).expect("neighbors");
        assert!(
            neighbors
                .iter()
                .any(|(n, k, _, out)| n.name == "callee" && *k == EdgeKind::Calls && *out),
            "call resolution was skipped"
        );
    }

    /// An index made before the source-root table existed, opened read-only,
    /// still serves `source_of`.
    #[test]
    fn source_of_works_on_an_index_without_the_source_root_table() {
        let tree = Tree::new();
        tree.write("src/m.rs", "pub fn m() {\n    1\n}\n");
        let db = tree.path().join("graph.db");
        let store = GraphStore::open(&db, tree.path()).expect("open");
        Indexer::new(&store).index().expect("index");
        store
            .conn()
            .execute_batch("DROP TABLE file_source_roots;")
            .expect("drop");
        drop(store);
        let reader = GraphStore::open_read_only(&db, tree.path()).expect("read-only");
        let node = reader.resolve_symbol("m").expect("resolve")[0].clone();
        assert!(reader.source_of(&node).expect("source").contains("1"));
        assert_eq!(reader.file_source_root("src/m.rs").expect("root"), None);
    }

    /// A tree of `dirs` directories, each with a file defining `shared` and a file
    /// calling it, so the name has `dirs` definitions and `dirs` call sites.
    fn shared_name_tree(dirs: usize) -> Tree {
        let tree = Tree::new();
        for d in 0..dirs {
            tree.write(&format!("d{d}/def.rs"), "pub fn shared() {}\n");
            tree.write(&format!("d{d}/call.rs"), "pub fn go() { shared(); }\n");
        }
        tree
    }

    /// Call resolution's work must grow with the call sites, not with their
    /// product with the definitions of the name they call. Counted as operations
    /// (`call_work`), so the assertion holds at any machine load. A per-site scan
    /// of every same-name definition would quadruple the count when the tree
    /// doubles twice; grouped lookups double it.
    #[test]
    fn call_resolution_work_grows_linearly_with_a_name_defined_in_every_directory() {
        let work = |dirs: usize| {
            let tree = shared_name_tree(dirs);
            let store = tree.store();
            let report = Indexer::new(&store).index().expect("index");
            // Same-directory scope resolves every site to its own neighbor.
            assert_eq!(report.calls_too_ambiguous, 0);
            assert_eq!(store.edge_count().expect("edges"), 3 * dirs as u64);
            report.call_work
        };
        let (small, large) = (work(50), work(200));
        assert!(small > 0, "nothing was counted");
        assert!(
            large <= small * 4 + small / 2,
            "4x the directories cost {large} against {small}: not linear"
        );
    }

    /// Files are written in batches of [`FILES_PER_WRITE_BATCH`], not one commit
    /// each, and a pass that changes nothing opens no write transaction.
    #[test]
    fn a_pass_commits_files_in_batches_and_an_unchanged_pass_commits_none() {
        let tree = Tree::new();
        let files = FILES_PER_WRITE_BATCH * 2 + 1;
        for i in 0..files {
            tree.write(&format!("src/m{i}.rs"), &format!("pub fn f{i}() {{}}\n"));
        }
        let store = tree.store();
        let first = Indexer::new(&store).index().expect("first");
        assert_eq!(first.files_added, files as u64);
        assert_eq!(first.write_batches, 3, "{files} files in batches of 256");
        assert_eq!(store.node_count().expect("nodes"), 2 * files as u64);

        let second = Indexer::new(&store).index().expect("second");
        assert_eq!(second.files_unchanged, files as u64);
        assert_eq!(second.write_batches, 0);

        // A deletion is a write too, and joins the same batching.
        fs::remove_file(tree.path().join("src/m0.rs")).expect("remove");
        let third = Indexer::new(&store).index().expect("third");
        assert_eq!(third.files_removed, 1);
        assert_eq!(third.write_batches, 1);
        assert!(store.file("src/m0.rs").expect("file").is_none());
    }

    /// Dropping an open batch rolls back every file written in it: a pass that
    /// fails midway leaves no file row without its nodes.
    #[test]
    fn a_dropped_write_batch_rolls_back_the_files_written_in_it() {
        let tree = Tree::new();
        tree.write("src/a.rs", "pub fn a() {}\n");
        let store = tree.store();
        let mut batch = Batch::new(&store, FILES_PER_WRITE_BATCH);
        batch.ensure().expect("open");
        let indexer = Indexer::new(&store);
        let bytes = fs::read(tree.path().join("src/a.rs")).expect("read");
        let mut report = IndexReport::default();
        indexer
            .index_one("src/a.rs", bytes, 0, None, &mut batch, &mut report)
            .expect("one");
        assert_eq!(report.files_added, 1);
        assert!(
            store.file("src/a.rs").expect("file").is_some(),
            "visible inside"
        );
        drop(batch);
        assert!(
            store.file("src/a.rs").expect("file").is_none(),
            "rolled back"
        );
        assert_eq!(store.node_count().expect("nodes"), 0);
    }

    #[test]
    fn a_second_pass_over_an_unchanged_tree_does_nothing() {
        let tree = Tree::new();
        tree.write("src/lib.rs", "fn a() {}\nfn b() { a(); }\n");
        let store = tree.store();
        let indexer = Indexer::new(&store);

        let first = indexer.index().expect("first pass");
        assert_eq!(first.files_added, 1);
        assert!(first.change_log_rows > 0);

        let second = indexer.index().expect("second pass");
        assert_eq!(second.files_added, 0);
        assert_eq!(second.files_changed, 0);
        assert_eq!(second.files_unchanged, 1);
        assert_eq!(
            second.change_log_rows, 0,
            "an unchanged tree must not touch the graph"
        );
    }

    #[test]
    fn indexing_named_paths_matches_a_walk_and_touches_nothing_else() {
        let tree = Tree::new();
        tree.write("src/lib.rs", "fn a() {}\n");
        tree.write("docs/talk.md", "# Talk\n\n## m1\n\nhello\n");
        let walked = tree.store();
        Indexer::new(&walked).index().expect("walk");

        let named = tree.store();
        let report = Indexer::new(&named)
            .index_paths(&["docs/talk.md".to_owned()])
            .expect("named");
        assert_eq!(report.files_added, 1);
        // Only the named file is in the index: nothing else was walked.
        assert_eq!(named.file_paths().expect("paths"), vec!["docs/talk.md"]);
        let ids = |s: &GraphStore| -> Vec<String> {
            s.nodes_in_file("docs/talk.md")
                .expect("nodes")
                .into_iter()
                .map(|n| n.id)
                .collect()
        };
        assert_eq!(ids(&named), ids(&walked), "identical rows either way");
        assert!(ids(&named).len() >= 2, "headings are nodes");

        // Unchanged: nothing appended. Deleted: it leaves the index.
        let again = Indexer::new(&named)
            .index_paths(&["docs/talk.md".to_owned()])
            .expect("again");
        assert_eq!(again.change_log_rows, 0);
        fs::remove_file(tree.path().join("docs/talk.md")).expect("rm");
        let gone = Indexer::new(&named)
            .index_paths(&["docs/talk.md".to_owned()])
            .expect("gone");
        assert_eq!(gone.files_removed, 1);
        // A path in no indexed language is refused, not skipped.
        assert!(
            Indexer::new(&named)
                .index_paths(&["notes.bin".to_owned()])
                .is_err()
        );
    }

    /// A file that exists only in another checkout (the run's integration
    /// worktree) is indexed from there under its relative path, and the store's
    /// root is never read or written.
    #[test]
    fn named_paths_are_read_from_the_source_root_when_one_is_given() {
        let tree = Tree::new();
        let other = Tree::new();
        other.write("docs/talk.md", "# Talk\n\n## m1\n\nhello\n");
        let store = tree.store();
        let report = Indexer::new(&store)
            .with_source_root(other.path())
            .index_paths(&["docs/talk.md".to_owned()])
            .expect("indexed");
        assert_eq!(report.files_added, 1);
        assert_eq!(store.file_paths().expect("paths"), vec!["docs/talk.md"]);
        assert!(
            store
                .nodes_in_file("docs/talk.md")
                .expect("nodes")
                .iter()
                .any(|n| n.name == "m1"),
            "the heading is a node"
        );
        assert!(
            !tree.path().join("docs").exists(),
            "the store root is untouched"
        );
    }

    /// A conversation file indexed from the integration
    /// worktree must survive a full walk of a checkout that lacks it, or holds
    /// an older copy; its message nodes are what the mailbox references.
    #[test]
    fn a_walk_leaves_bus_owned_conversation_files_to_the_bus() {
        let tree = Tree::new();
        let other = Tree::new();
        let conv = format!("{}/a--b.md", BUS_OWNED_DIRS[0]);
        let notes = format!("{}/NOTES.md", BUS_OWNED_DIRS[0]);
        tree.write("src/lib.rs", "fn a() {}\n");
        other.write(
            &conv,
            "# Conversation: a and b\n\n## m1\n\nx\n\n## m2\n\ny\n",
        );
        let ids_of = |s: &GraphStore, f: &str| -> Vec<String> {
            s.nodes_in_file(f)
                .expect("nodes")
                .into_iter()
                .map(|n| n.id)
                .collect()
        };
        let store = tree.store();
        Indexer::new(&store)
            .with_source_root(other.path())
            .index_paths(std::slice::from_ref(&conv))
            .expect("bus");
        let ids = |s: &GraphStore| -> Vec<String> {
            s.nodes_in_file(&conv)
                .expect("nodes")
                .into_iter()
                .map(|n| n.id)
                .collect()
        };
        let before = ids(&store);
        assert!(before.len() >= 3);
        // Absent from the checkout: kept.
        Indexer::new(&store).index().expect("walk");
        assert_eq!(ids(&store), before);
        // An older copy in the checkout (a merge landed earlier): still the
        // bus's version.
        tree.write(&conv, "# Conversation: a and b\n\n## m1\n\nx\n");
        let report = Indexer::new(&store).index().expect("walk");
        assert_eq!(ids(&store), before);
        assert_eq!(report.bus_owned_skipped, 1, "reported, not silent");
        // A file the user wrote in the same directory is theirs: indexed.
        tree.write(&notes, "# Notes\n\n## mine\n");
        let report = Indexer::new(&store).index().expect("walk");
        assert_eq!(report.files_added, 1);
        assert!(!ids_of(&store, &notes).is_empty());
        // And removed like any other when deleted.
        std::fs::remove_file(tree.path().join(&notes)).expect("rm");
        let report = Indexer::new(&store).index().expect("walk");
        assert_eq!(report.files_removed, 1);
        assert_eq!(ids(&store), before, "the bus's file is still kept");
        assert!(
            store
                .file_paths()
                .expect("paths")
                .contains(&"src/lib.rs".to_owned())
        );
    }

    #[test]
    fn a_content_change_preserving_mtime_and_size_is_detected() {
        // This is the reference implementation's bug, written as a test. Both
        // files are the same length and the modification time is restored, so a
        // time-and-size comparison sees nothing. The hash sees it.
        let tree = Tree::new();
        tree.write("src/lib.rs", "fn alpha() {}\n");
        let store = tree.store();
        let indexer = Indexer::new(&store);
        indexer.index().expect("first pass");

        let full = tree.path().join("src/lib.rs");
        let before = fs::metadata(&full).expect("metadata");
        let stamp = before.modified().expect("mtime");
        let size_before = before.len();

        // Same byte count, different bytes.
        fs::write(&full, "fn gamma() {}\n").expect("rewrite");
        // Restore the timestamp exactly, the way a checkout or a restore does.
        let file = fs::File::options().write(true).open(&full).expect("open");
        file.set_modified(stamp).expect("set mtime");
        drop(file);

        let after = fs::metadata(&full).expect("metadata");
        assert_eq!(after.len(), size_before, "the test needs an equal size");
        assert_eq!(
            after.modified().expect("mtime"),
            stamp,
            "the test needs an equal mtime"
        );

        let report = indexer.index().expect("second pass");
        assert_eq!(
            report.files_changed, 1,
            "a content change preserving mtime and size must be detected"
        );
        let names: Vec<_> = store
            .nodes_in_file("src/lib.rs")
            .expect("nodes")
            .into_iter()
            .map(|n| n.name)
            .collect();
        assert!(names.contains(&"gamma".to_owned()), "got {names:?}");
        assert!(!names.contains(&"alpha".to_owned()), "got {names:?}");
    }

    #[test]
    fn a_reindex_touches_only_the_changed_subgraph() {
        let tree = Tree::new();
        for i in 0..12 {
            tree.write(&format!("src/m{i}.rs"), &format!("fn f{i}() {{}}\n"));
        }
        let store = tree.store();
        let indexer = Indexer::new(&store);
        let first = indexer.index().expect("first pass");

        tree.write("src/m3.rs", "fn f3() {}\nfn extra() {}\n");
        let second = indexer.index().expect("second pass");

        assert_eq!(second.files_changed, 1);
        assert_eq!(second.files_unchanged, 11);
        // One file's rows, not twelve files' rows. The exact count is not the
        // property; the property is that it scales with the change, not the tree.
        assert!(
            second.change_log_rows < first.change_log_rows / 4,
            "reindex wrote {} rows against a first pass of {}",
            second.change_log_rows,
            first.change_log_rows
        );
    }

    #[test]
    fn layout_positions_are_stable_across_a_reindex() {
        let tree = Tree::new();
        tree.write("src/lib.rs", "fn keep() {}\n");
        tree.write("src/other.rs", "fn untouched() {}\n");
        let store = tree.store();
        let indexer = Indexer::new(&store);
        indexer.index().expect("first pass");

        let id_of = |name: &str| {
            store
                .resolve_symbol(name)
                .expect("resolve")
                .first()
                .expect("one")
                .id
                .clone()
        };
        let keep_id = id_of("keep");
        let untouched_id = id_of("untouched");
        let keep_before = store.position(&keep_id).expect("position").expect("cached");
        let untouched_before = store
            .position(&untouched_id)
            .expect("position")
            .expect("cached");

        // Insert a node in one region.
        tree.write("src/lib.rs", "fn keep() {}\nfn inserted() {}\n");
        indexer.index().expect("second pass");

        assert_eq!(
            store.position(&keep_id).expect("position").expect("cached"),
            keep_before,
            "an edited file's surviving node moved"
        );
        assert_eq!(
            store
                .position(&untouched_id)
                .expect("position")
                .expect("cached"),
            untouched_before,
            "a node elsewhere in the scene moved"
        );
    }

    #[test]
    fn a_deleted_file_leaves_the_index() {
        let tree = Tree::new();
        tree.write("src/a.rs", "fn a() {}\n");
        tree.write("src/b.rs", "fn b() {}\n");
        let store = tree.store();
        let indexer = Indexer::new(&store);
        indexer.index().expect("first pass");
        assert_eq!(store.file_count().expect("count"), 2);

        fs::remove_file(tree.path().join("src/b.rs")).expect("remove");
        let report = indexer.index().expect("second pass");
        assert_eq!(report.files_removed, 1);
        assert_eq!(store.file_count().expect("count"), 1);
        // The cascade must take the nodes with it, or the graph draws a file
        // that is not there.
        assert!(store.nodes_in_file("src/b.rs").expect("nodes").is_empty());
    }

    #[test]
    fn a_multi_language_tree_indexes_every_language() {
        let tree = Tree::new();
        tree.write("src/lib.rs", "pub fn rust_fn() {}\n");
        tree.write("scripts/tool.py", "def python_fn():\n    pass\n");
        tree.write("ui/app.ts", "export function tsFn(): void {}\n");
        tree.write("ui/legacy.js", "function jsFn() {}\n");
        tree.write("README.md", "# Title\n\nbody\n");
        let store = tree.store();
        let report = Indexer::new(&store).index().expect("index");
        assert_eq!(report.files_added, 5);
        for symbol in ["rust_fn", "python_fn", "tsFn", "jsFn", "Title"] {
            store
                .resolve_symbol(symbol)
                .unwrap_or_else(|e| panic!("{symbol} did not resolve: {e}"));
        }
    }

    #[test]
    fn excluded_directories_are_not_walked() {
        let tree = Tree::new();
        tree.write("src/lib.rs", "fn keep() {}\n");
        tree.write("target/debug/generated.rs", "fn generated() {}\n");
        tree.write("node_modules/dep/index.js", "function dep() {}\n");
        let store = tree.store();
        let report = Indexer::new(&store).index().expect("index");
        assert_eq!(report.files_seen, 1);
        assert!(store.resolve_symbol("generated").is_err());
    }

    #[test]
    fn source_is_read_from_disk_and_refused_once_the_file_has_changed() {
        let tree = Tree::new();
        tree.write("src/lib.rs", "fn target() {\n    original();\n}\n");
        let store = tree.store();
        Indexer::new(&store).index().expect("index");
        let node = store.resolve_symbol("target").expect("resolve")[0].clone();
        assert!(store.source_of(&node).expect("source").contains("original"));

        // An edit without a reindex: the recorded lines may name other text, so
        // the read is refused instead of returning shifted or replaced lines.
        tree.write(
            "src/lib.rs",
            "// new first line\nfn target() {\n    replaced();\n}\n",
        );
        let error = store.source_of(&node).expect_err("stale");
        assert!(matches!(error, GraphError::StaleFile { .. }), "{error:?}");

        // A same-length edit that keeps every span in place is stale too.
        tree.write("src/lib.rs", "fn target() {\n    replaced();\n}\n");
        assert!(store.source_of(&node).is_err());

        Indexer::new(&store).index().expect("reindex");
        let node = store.resolve_symbol("target").expect("resolve")[0].clone();
        assert!(store.source_of(&node).expect("source").contains("replaced"));
    }

    /// A file indexed from another tree (a worktree holding a merge the checkout
    /// has not received) is read from that tree, verified against the indexed
    /// hash, and falls back to the store's root once the other tree stops
    /// holding that content. Indexing it from the store's root clears the record.
    #[test]
    fn source_of_reads_the_recorded_source_root_and_still_checks_the_hash() {
        let tree = Tree::new();
        let other = Tree::new();
        other.write("src/m.rs", "pub fn merged() {\n    1\n}\n");
        let store = tree.store();
        Indexer::new(&store)
            .with_source_root(other.path())
            .index_paths(&["src/m.rs".to_owned()])
            .expect("indexed");
        let node = store.resolve_symbol("merged").expect("resolve")[0].clone();
        assert_eq!(
            store.file_source_root("src/m.rs").expect("root").as_deref(),
            Some(other.path())
        );
        // Not in the checkout at all: the recorded root serves it.
        assert!(store.source_of(&node).expect("read").contains("1"));

        // The checkout holds other content, the worktree is edited: stale.
        tree.write("src/m.rs", "pub fn merged() {\n    2\n}\n");
        other.write("src/m.rs", "pub fn merged() {\n    3\n}\n");
        let error = store.source_of(&node).expect_err("stale");
        assert!(matches!(error, GraphError::StaleFile { .. }), "{error:?}");

        // The worktree is gone but the checkout received the merge: served
        // from the checkout, hash verified.
        tree.write("src/m.rs", "pub fn merged() {\n    1\n}\n");
        std::fs::remove_file(other.path().join("src/m.rs")).expect("rm");
        assert!(store.source_of(&node).expect("read").contains("1"));

        // A walk of the checkout finds the indexed content and clears the record.
        Indexer::new(&store).index().expect("walk");
        assert_eq!(store.file_source_root("src/m.rs").expect("root"), None);
        assert!(store.source_of(&node).expect("read").contains("1"));

        // Content that differs everywhere is refused, not guessed.
        tree.write("src/m.rs", "pub fn merged() {\n    9\n}\n");
        assert!(matches!(
            store.source_of(&node),
            Err(GraphError::StaleFile { .. })
        ));
    }

    #[test]
    fn a_span_that_no_longer_fits_its_file_raises() {
        // The hash matches, yet the recorded span lies outside the file: a
        // corrupt row must not yield the wrong lines quietly.
        let tree = Tree::new();
        tree.write("src/lib.rs", "fn a() {}\nfn b() {}\n");
        let store = tree.store();
        Indexer::new(&store).index().expect("index");
        let mut node = store.resolve_symbol("b").expect("resolve")[0].clone();
        node.start_line = 40;
        node.end_line = 41;
        let err = store.source_of(&node).expect_err("must raise");
        assert!(matches!(err, GraphError::StaleSpan { .. }), "got {err:?}");
    }

    #[test]
    fn call_edges_resolve_across_files() {
        let tree = Tree::new();
        tree.write("src/caller.rs", "fn caller() { callee(); }\n");
        tree.write("src/callee.rs", "pub fn callee() {}\n");
        let store = tree.store();
        Indexer::new(&store).index().expect("index");
        let caller = store.resolve_symbol("caller").expect("resolve")[0].clone();
        let neighbors = store.neighbors(&caller.id).expect("neighbors");
        assert!(
            neighbors
                .iter()
                .any(|(node, kind, _, outgoing)| node.name == "callee"
                    && *kind == EdgeKind::Calls
                    && *outgoing),
            "no call edge to callee in {neighbors:#?}"
        );
    }

    fn outgoing_calls(store: &GraphStore, name: &str) -> Vec<(NodeRecord, i64)> {
        let node = store.resolve_symbol(name).expect("resolve")[0].clone();
        store
            .neighbors(&node.id)
            .expect("neighbors")
            .into_iter()
            .filter(|(_, kind, _, outgoing)| *kind == EdgeKind::Calls && *outgoing)
            .map(|(n, _, w, _)| (n, w))
            .collect()
    }

    /// A Python `make()` call was
    /// recorded as a caller of a Rust `fn make` with weight 100. It failed before
    /// the language-family gate in `resolve_calls`.
    #[test]
    fn a_call_never_resolves_to_a_definition_in_another_language() {
        let tree = Tree::new();
        tree.write("web/app.py", "def handler():\n    make()\n");
        tree.write("backend/lib.rs", "pub fn make() -> u32 { 1 }\n");
        let store = tree.store();
        let report = Indexer::new(&store).index().expect("index");
        let calls = outgoing_calls(&store, "handler");
        assert!(calls.is_empty(), "a Python call reached Rust: {calls:#?}");
        assert_eq!(report.calls_cross_language, 1, "the gate is reported");
        assert!(report.calls_unresolved >= 1);
        // The slice agrees: the Rust function has no Python caller.
        let make = store.resolve_symbol("make").expect("resolve")[0].clone();
        assert!(
            store
                .neighbors(&make.id)
                .expect("neighbors")
                .iter()
                .all(|(n, kind, _, _)| *kind != EdgeKind::Calls || n.path != "web/app.py")
        );
    }

    /// The gate is a family, not a language: TypeScript calls JavaScript.
    #[test]
    fn a_typescript_call_resolves_to_a_javascript_definition() {
        let tree = Tree::new();
        tree.write("ui/main.ts", "function start() { helper(); }\n");
        tree.write("lib/util.js", "function helper() { return 1; }\n");
        tree.write("svc/other.py", "def helper():\n    pass\n");
        let store = tree.store();
        let report = Indexer::new(&store).index().expect("index");
        let calls = outgoing_calls(&store, "start");
        assert_eq!(calls.len(), 1, "{calls:#?}");
        assert_eq!(calls[0].0.path, "lib/util.js");
        assert_eq!(calls[0].1, 100, "the Python candidate does not dilute it");
        assert_eq!(report.calls_cross_language, 0);
    }

    #[test]
    fn an_ambiguous_call_keeps_every_candidate_with_a_reduced_weight() {
        // Decision 0020's general principle: weight, do not threshold. Dropping
        // an ambiguous edge would discard information no consumer can recover.
        let tree = Tree::new();
        tree.write("src/one.rs", "pub fn shared() {}\n");
        tree.write("src/two.rs", "pub fn shared() {}\n");
        tree.write("src/call.rs", "fn go() { shared(); }\n");
        let store = tree.store();
        Indexer::new(&store).index().expect("index");
        let go = store.resolve_symbol("go").expect("resolve")[0].clone();
        let calls: Vec<_> = store
            .neighbors(&go.id)
            .expect("neighbors")
            .into_iter()
            .filter(|(_, kind, _, outgoing)| *kind == EdgeKind::Calls && *outgoing)
            .collect();
        assert_eq!(calls.len(), 2, "both candidates must be kept: {calls:#?}");
        for (_, _, weight, _) in &calls {
            assert_eq!(*weight, 50, "ambiguity must show up in the weight");
        }
    }

    #[test]
    fn a_call_resolves_to_the_definition_in_its_own_file_first() {
        // Proximity scoping is what makes name-based resolution useful without a
        // type resolver: `helper` exists twice, and the local one wins.
        let tree = Tree::new();
        tree.write("src/here.rs", "fn helper() {}\nfn caller() { helper(); }\n");
        tree.write("src/there.rs", "pub fn helper() {}\n");
        let store = tree.store();
        Indexer::new(&store).index().expect("index");
        let caller = store.resolve_symbol("caller").expect("resolve")[0].clone();
        let calls: Vec<_> = store
            .neighbors(&caller.id)
            .expect("neighbors")
            .into_iter()
            .filter(|(_, kind, _, outgoing)| *kind == EdgeKind::Calls && *outgoing)
            .collect();
        assert_eq!(
            calls.len(),
            1,
            "the local definition should win: {calls:#?}"
        );
        assert_eq!(calls[0].0.path, "src/here.rs");
        assert_eq!(calls[0].2, 100, "an unambiguous resolution weighs 100");
    }

    #[test]
    fn a_name_with_too_many_definitions_is_reported_unresolved_not_fanned_out() {
        // The tractability exception, tested for the effect rather than for the
        // constant (test the mechanism, not the setting). Without it, one call site writes an edge
        // per candidate and a real repository's call graph explodes.
        let tree = Tree::new();
        for i in 0..(MAX_CALL_FANOUT + 4) {
            tree.write(&format!("defs/d{i}/mod.rs"), "pub fn ubiquitous() {}\n");
        }
        tree.write("src/caller.rs", "fn go() { ubiquitous(); }\n");
        let store = tree.store();
        let report = Indexer::new(&store).index().expect("index");
        assert_eq!(report.calls_too_ambiguous, 1);
        let go = store.resolve_symbol("go").expect("resolve")[0].clone();
        let calls: Vec<_> = store
            .neighbors(&go.id)
            .expect("neighbors")
            .into_iter()
            .filter(|(_, kind, _, outgoing)| *kind == EdgeKind::Calls && *outgoing)
            .collect();
        assert!(
            calls.is_empty(),
            "an over-ambiguous call must not fan out: {calls:#?}"
        );
    }

    #[test]
    fn an_unresolvable_call_is_counted_rather_than_ignored() {
        let tree = Tree::new();
        tree.write(
            "src/lib.rs",
            "fn go() { println!(\"x\"); external_thing(); }\n",
        );
        let store = tree.store();
        let report = Indexer::new(&store).index().expect("index");
        assert!(
            report.calls_unresolved >= 1,
            "calls into code the index does not hold must be counted"
        );
    }

    #[test]
    fn a_file_in_an_unknown_language_is_skipped_rather_than_guessed_at() {
        let tree = Tree::new();
        tree.write("data.bin", "\u{1}\u{2}\u{3}");
        tree.write("Makefile", "all:\n\techo hi\n");
        let store = tree.store();
        let report = Indexer::new(&store).index().expect("index");
        assert_eq!(report.files_seen, 0);
    }

    #[test]
    fn mtime_is_recorded_but_not_used_as_the_detector() {
        // Touching a file without changing its bytes must not reindex it. The
        // converse of the hash test, and the reason mtime is stored at all is
        // diagnosis, not detection.
        let tree = Tree::new();
        tree.write("src/lib.rs", "fn a() {}\n");
        let store = tree.store();
        let indexer = Indexer::new(&store);
        indexer.index().expect("first pass");

        let full = tree.path().join("src/lib.rs");
        let file = fs::File::options().write(true).open(&full).expect("open");
        file.set_modified(SystemTime::now() + Duration::from_secs(60))
            .expect("touch");
        drop(file);

        let report = indexer.index().expect("second pass");
        assert_eq!(report.files_changed, 0, "a touch must not reindex");
        assert_eq!(report.change_log_rows, 0);
    }

    #[test]
    fn a_one_line_edit_rewrites_only_the_call_edges_it_changes() {
        // Forty files calling each other across directories. Before the diff,
        // resolve_calls deleted and reinserted every call edge, so this edit
        // appended a row per call edge in the whole tree.
        let tree = Tree::new();
        for d in 0..4 {
            for f in 0..10 {
                let other = (d + 1) % 4;
                tree.write(
                    &format!("m{d}/f{f}.rs"),
                    &format!(
                        "pub fn a{d}_{f}() {{ b{d}_{f}(); a{other}_{f}(); }}\npub fn b{d}_{f}() {{}}\n"
                    ),
                );
            }
        }
        let store = tree.store();
        let indexer = Indexer::new(&store);
        indexer.index().expect("first");
        let count_calls = || -> i64 {
            store
                .conn()
                .query_row("SELECT COUNT(*) FROM edges WHERE kind = 'calls'", [], |r| {
                    r.get(0)
                })
                .expect("count")
        };
        let calls = count_calls();
        assert!(calls >= 80, "{calls}");
        let before = store.change_watermark().expect("w");

        tree.write(
            "m0/f0.rs",
            "pub fn a0_0() { b0_0(); a1_0(); }\npub fn b0_0() {}\n// edited\n",
        );
        let report = indexer.index().expect("second");
        assert_eq!(report.files_changed, 1);
        let edges_logged: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM change_log WHERE table_name = 'edges' AND seq > ?1",
                [before],
                |r| r.get(0),
            )
            .expect("count");
        // Only the edited file's own edges cascade and return (its contains edges,
        // its two outgoing calls and the one incoming call from m3/f0), each
        // deleted and reinserted: a dozen or so rows, not one per call edge.
        assert!(
            edges_logged <= 20,
            "{edges_logged} edge rows logged for {calls} call edges"
        );
        assert_eq!(count_calls(), calls, "the same call graph");
        assert!(store.meta(INDEX_IN_PROGRESS).expect("meta").is_none());
        assert_eq!(
            crate::snapshot::index_pass_running(&store).expect("m"),
            None
        );
    }

    #[test]
    fn overlapping_passes_do_not_clear_each_others_marker() {
        let tree = Tree::new();
        tree.write("docs/a.md", "# A\n");
        let store = tree.store();
        // Another pass (another process) is running.
        store
            .set_meta(&format!("{INDEX_IN_PROGRESS}/other-1"), "1000")
            .expect("marker");
        Indexer::new(&store).index().expect("walk");
        Indexer::new(&store)
            .index_paths(&["docs/a.md".to_owned()])
            .expect("named");
        assert_eq!(
            crate::snapshot::index_pass_running(&store).expect("m"),
            Some(1000),
            "the other pass is still marked"
        );
        store
            .clear_meta(&format!("{INDEX_IN_PROGRESS}/other-1"))
            .expect("clear");
        assert_eq!(
            crate::snapshot::index_pass_running(&store).expect("m"),
            None
        );
    }

    #[test]
    fn a_named_file_over_the_ceiling_is_skipped_and_leaves_the_index() {
        let tree = Tree::new();
        tree.write("docs/big.md", "# Big\n\nsmall at first\n");
        let store = tree.store();
        Indexer::new(&store)
            .index_paths(&["docs/big.md".to_owned()])
            .expect("index");
        assert_eq!(store.file_paths().expect("p"), vec!["docs/big.md"]);
        tree.write("docs/big.md", &format!("# Big\n\n{}\n", "x".repeat(200)));
        let report = Indexer::new(&store)
            .with_max_file_bytes(100)
            .index_paths(&["docs/big.md".to_owned()])
            .expect("index");
        assert_eq!(report.skipped_too_large, 1);
        assert!(store.file_paths().expect("p").is_empty(), "as a walk would");
    }
}
