//! The embedded store: schema, triggers, and the change-log event spine.
//!
//! One SQLite database per project holds the code graph (decision 0007). Two
//! rules from that record are enforced here rather than by convention.
//!
//! **Only facts are stored; display status is derived at read time.** There is
//! no `status` column anywhere in this schema. A second copy of the truth drifts
//! from the first, and the galaxy view is the product.
//!
//! **Database triggers append to a change log, and that log is the only event
//! source.** A live graph driven by hand-placed event emissions will eventually
//! miss one, and the missed one is invisible until a user notices the graph is
//! stale. Because the triggers are on the tables, no code path, including one
//! written later by an agent, can mutate state without the interface hearing
//! about it.

use crate::chunk::NodeKind;
use crate::error::{GraphError, Result};
use crate::lang::Language;
use crate::layout::{self, Position};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeSet;
use std::path::Path;

/// The schema version this build writes. A version bump rebuilds the *derived* tables
/// (nodes, edges and everything indexed from source) rather than migrating them,
/// because that data is cheap to re-derive. Lease state is not derived data, it is
/// live operational state a running agent is depending on, so a version bump leaves
/// `leases`, `lease_history`, `node_leases` and `node_lease_history` untouched; see
/// [`GraphStore::migrate`].
///
/// 3: sub-file leasing (decision 0050) adds `node_leases` and
/// `node_lease_history`, and `lease_history` gains `range_start`/`range_end` so the
/// needless-wait fraction (decision 0049) can be read back from the registry's own
/// history instead of a side channel.
///
/// 4: the rebuild-on-mismatch path was found dropping
/// live lease tables outright, and a chunk's `identity_stable` (decision 0049)
/// was trusted from the caller rather than read from the graph. `nodes` gains
/// `identity_stable`, populated at index time from `Chunk::identity_stable`, so a
/// node lease request no longer has to be taken on faith. Indexes on
/// `(path, generation)` and `(node_id, generation)` replace the path-only and
/// node-id-only ones so the per-grant generation lookup does not degrade with
/// history size.
pub(crate) const SCHEMA_VERSION: i64 = 4;

/// How long a statement waits for another connection's lock before it fails.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(5_000);

/// What one edge means.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Parent to child in the chunk hierarchy.
    Contains,
    /// Caller to callee, resolved by name. Navigation and context only: decision
    /// 0020 removed the call graph from the collision-prediction role.
    Calls,
    /// A file to a file it imports, where the import resolves on disk.
    Imports,
}

impl EdgeKind {
    /// The kind's stable name.
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Contains => "contains",
            EdgeKind::Calls => "calls",
            EdgeKind::Imports => "imports",
        }
    }

    /// Parse a persisted or queried edge kind.
    pub fn parse(name: &str) -> Option<EdgeKind> {
        Some(match name {
            "contains" => EdgeKind::Contains,
            "calls" => EdgeKind::Calls,
            "imports" => EdgeKind::Imports,
            _ => return None,
        })
    }
}

/// A file as the index sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRecord {
    pub id: String,
    /// Repository-relative, forward-slashed on every platform.
    pub path: String,
    /// Blake3 of the file's bytes. **The change detector**, not a decoration.
    pub content_hash: String,
    pub size_bytes: u64,
    /// Modification time in nanoseconds since the epoch, recorded for diagnosis
    /// only. It is deliberately not consulted when deciding whether to reindex.
    pub mtime_ns: i64,
    pub language: Option<Language>,
    pub line_count: u32,
}

/// A node as the index sees it: a location, never text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRecord {
    pub id: String,
    pub file_id: String,
    pub path: String,
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: String,
    pub parent_id: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub start_byte: u64,
    pub end_byte: u64,
    pub signature: String,
    /// `Chunk::identity_stable` (decision 0049), persisted at index time rather than
    /// trusted from whoever asks for a node lease. A caller-declared flag can lie by
    /// accident (a stale copy, a hand-built request) or on purpose; the graph itself
    /// is the one place that actually ran the disambiguation this flag reports.
    pub identity_stable: bool,
}

/// A handle on one project's index.
pub struct GraphStore {
    conn: Connection,
    /// The working-tree root every stored path is relative to.
    root: std::path::PathBuf,
    /// The background pass retrying the checkpoint a reader blocked after
    /// [`GraphStore::clear_meta`]. See [`GraphStore::schedule_wal_scrub`]. The
    /// store stops and joins it on drop, so the pass never outlives the store and
    /// its connection is closed before the store's own (the last close of a
    /// database checkpoints the WAL).
    wal_scrub: std::sync::Mutex<Option<WalScrub>>,
}

struct WalScrub {
    stop: std::sync::mpsc::Sender<()>,
    handle: std::thread::JoinHandle<bool>,
}

impl Drop for GraphStore {
    fn drop(&mut self) {
        self.stop_wal_scrub();
    }
}

/// How long the background pass keeps retrying a blocked checkpoint, and how
/// often. A viewer's read transaction lasts a frame or a query, so a minute is
/// far past any healthy reader; a reader held longer is reported.
const WAL_SCRUB_BOUND: std::time::Duration = std::time::Duration::from_secs(60);
const WAL_SCRUB_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Whether opening may rebuild the derived tables of an index of another schema
/// version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rebuild {
    Allowed,
    Refused,
}

impl GraphStore {
    /// Open, creating and migrating the schema if needed.
    ///
    /// `root` is the working tree the index describes. It is stored so that
    /// [`GraphStore::source_of`] can re-read a node's lines from disk, which is
    /// the whole point of indexing locations rather than text.
    pub fn open(db_path: &Path, root: &Path) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        Self::from_connection(conn, root, Rebuild::Allowed)
    }

    /// Open an existing index for reading only: `SQLITE_OPEN_READ_ONLY`, no schema
    /// creation, no migration, no pragma that writes. The galaxy view uses this, so
    /// a viewer can never create, rebuild or modify a project's index. Any write
    /// through the returned store fails with SQLite's read-only error.
    pub fn open_read_only(db_path: &Path, root: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        Ok(Self {
            conn,
            root: root.to_path_buf(),
            wal_scrub: Default::default(),
        })
    }

    /// [`GraphStore::open_read_only`] for a command that reads the index and
    /// nothing else: a missing file is [`GraphError::NoIndex`] and an index of
    /// another schema version is [`GraphError::SchemaMismatch`], both naming
    /// `kgraf index`, so a reader never meets an opaque "no such table".
    pub fn open_read_only_checked(db_path: &Path, root: &Path) -> Result<Self> {
        require_index_file(db_path)?;
        let store = Self::open_read_only(db_path, root)?;
        let found = store.schema_version()?;
        if found != Some(SCHEMA_VERSION) {
            return Err(store.mismatch(found));
        }
        Ok(store)
    }

    /// Open an existing index to record facts (schedules and their run history)
    /// without ever rebuilding it: the file is not created, and a schema version
    /// other than this build's is refused with [`GraphError::SchemaMismatch`]
    /// instead of dropping the derived tables. With a matching version, the
    /// non-derived tables a newer build adds are created if missing, as
    /// [`GraphStore::open`] does. The desktop shell's scheduler uses this, so a
    /// background tick can never empty a project's code index.
    pub fn open_existing_for_facts(db_path: &Path, root: &Path) -> Result<Self> {
        require_index_file(db_path)?;
        let conn = Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // The version check is inside `migrate`: a check here, then
        // a migrate that may rebuild, left a window in which another process could
        // change the version between the two.
        Self::from_connection(conn, root, Rebuild::Refused)
    }

    /// Open an in-memory index. Used by tests and by a dry-run index.
    pub fn open_in_memory(root: &Path) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn, root, Rebuild::Allowed)
    }

    fn from_connection(mut conn: Connection, root: &Path, rebuild: Rebuild) -> Result<Self> {
        // WAL so the UI can read while the indexer writes; foreign keys on so a
        // node cannot outlive its file.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Every row id is a hash, so inserts land on random b-tree leaves. With
        // SQLite's default 2 MiB page cache a cold index of a large tree misses the
        // cache on most inserts and rereads and rewrites pages it just held. The
        // cache fills on demand, so a small index costs nothing; 64 MiB is the most
        // a connection holds.
        conn.pragma_update(None, "cache_size", -65536)?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        // Deleted and overwritten text is zeroed where that costs no extra
        // I/O (decision 0071): b-tree pages are purged, freed overflow pages
        // are not. `set_meta` and `clear_meta` raise it to ON for their one
        // write, since run records are large enough to use overflow pages.
        // Measured on a delete-and-reinsert workload (2,000 record rewrites
        // plus 300 batches of 400 row replacements, three rounds): FAST no
        // slower than OFF, ON 3 to 21 percent slower, so ON is not set for
        // the indexer.
        conn.pragma_update(None, "secure_delete", "FAST")?;
        // A concurrent writer (two workers leasing at once) now hits SQLITE_BUSY
        // instead of failing instantly: it waits up to this long for the other
        // transaction to finish rather than surfacing a spurious contention error
        // that the lease registry's own conflict reporting was supposed to own.
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Every transaction on this connection takes its write lock at BEGIN rather
        // than at the first write. A DEFERRED transaction that reads a lease table
        // then writes it can be forced to upgrade mid-transaction and lose the race
        // to a concurrent writer that started later but reached its write first
        // (SQLITE_BUSY at the worst possible moment, after the read already informed
        // a decision). Taking the lock up front for every transaction, lease-related
        // or not, removes that window; the cost is a writer occasionally waiting
        // slightly earlier than it strictly had to, not a lost decision.
        conn.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);
        let store = Self {
            conn,
            root: root.to_path_buf(),
            wal_scrub: Default::default(),
        };
        store.migrate(rebuild)?;
        Ok(store)
    }

    /// The working tree this index describes.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Direct access to the connection, for the modules in this crate that own
    /// their own queries.
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    fn schema_version(&self) -> Result<Option<i64>> {
        let has_meta: bool = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |row| row.get::<_, i64>(0).map(|n| n > 0),
        )?;
        if !has_meta {
            return Ok(None);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse::<i64>().ok()))
    }

    fn mismatch(&self, found: Option<i64>) -> GraphError {
        GraphError::SchemaMismatch {
            path: self.conn.path().unwrap_or("(in memory)").to_owned(),
            found,
            expected: SCHEMA_VERSION,
        }
    }

    fn migrate(&self, rebuild: Rebuild) -> Result<()> {
        // Refused before any write: an index of another version is left exactly as
        // it was, not given this build's tables first.
        if rebuild == Rebuild::Refused {
            let found = self.schema_version()?;
            if found != Some(SCHEMA_VERSION) {
                return Err(self.mismatch(found));
            }
        }
        self.conn.execute_batch(SCHEMA)?;
        // `facts` is not derived data, so it is migrated in place, never rebuilt:
        // a table created before `episode_count` existed gains the column, filled
        // from the episodes it already lists.
        let has_count: bool = self.conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('facts') WHERE name = 'episode_count'",
            [],
            |row| row.get::<_, i64>(0).map(|n| n > 0),
        )?;
        if !has_count {
            // One transaction: a kill between the two statements must not leave
            // the column added with every count at its default of 1.
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(
                "ALTER TABLE facts ADD COLUMN episode_count INTEGER NOT NULL DEFAULT 1;
                 UPDATE facts SET episode_count = MAX(1, json_array_length(episodes))
                  WHERE json_valid(episodes);",
            )?;
            tx.commit()?;
        }
        // `work_presumed` is not derived data either: a table created before the
        // claim columns gains them in place, empty (the claim then comes from
        // the node body until the next read writes it).
        // As the facts migration above: one Immediate transaction, the columns
        // checked again inside it, each added only if missing.
        // Two ALTERs outside a transaction could leave one column (every later
        // read then failed on the other), and two processes opening the same
        // index could both see it unmigrated and race on the ALTER.
        if !crate::presumed::has_claim_columns(&self.conn)? {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let (claim, text) = crate::presumed::claim_columns(&tx)?;
            if !claim {
                tx.execute_batch(
                    "ALTER TABLE work_presumed ADD COLUMN doc_claim TEXT NOT NULL DEFAULT '';",
                )?;
            }
            if !text {
                tx.execute_batch(
                    "ALTER TABLE work_presumed ADD COLUMN doc_claim_text TEXT NOT NULL DEFAULT '';",
                )?;
            }
            tx.commit()?;
        }
        // `agent_sessions` is not derived data either (0067's own module docs):
        // a table created before `addressable` existed gains it in place,
        // defaulted to addressable (the field's own default). As the
        // `work_presumed` migration above: checked, then checked
        // again inside one Immediate transaction, so two processes opening one
        // index cannot both add the column.
        if !crate::sessions::has_addressable_column(&self.conn)? {
            let tx = rusqlite::Transaction::new_unchecked(
                &self.conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            if !crate::sessions::has_addressable_column(&tx)? {
                tx.execute_batch(
                    "ALTER TABLE agent_sessions ADD COLUMN addressable INTEGER NOT NULL DEFAULT 1;",
                )?;
            }
            tx.commit()?;
        }
        let version: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse::<i64>().ok());
        match version {
            Some(v) if v == SCHEMA_VERSION => {}
            // Checked again here, on the value this decision reads, so a version
            // changed by another process since the check above is refused too.
            found if rebuild == Rebuild::Refused => return Err(self.mismatch(found)),
            // The index is derived data, so a version change rebuilds rather than
            // migrating it. Rebuilding a large repository takes minutes; a
            // half-applied migration takes a support conversation. Lease state is
            // not derived data and is excluded from `DROP_DERIVED_ON_SCHEMA_CHANGE`
            // (see its doc); every node lease is purged below instead, because the
            // rebuild just emptied `nodes` and a lease on nothing is exactly the
            // vanished-target case decision 0050 already requires refusing loudly.
            Some(_) => {
                self.conn.execute_batch(DROP_DERIVED_ON_SCHEMA_CHANGE)?;
                self.conn.execute_batch(SCHEMA)?;
                self.conn
                    .execute("DELETE FROM node_leases WHERE 1 = 1", [])?;
                self.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
            }
            None => self.set_meta("schema_version", &SCHEMA_VERSION.to_string())?,
        }
        Ok(())
    }

    /// Record a project-level fact.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.secure_delete(|conn| {
            conn.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
        })
    }

    /// Remove a project-level fact, and try to fold the WAL into the file so the
    /// removed value's old page images leave the WAL too (decision 0071).
    ///
    /// The checkpoint does not wait ([`GraphStore::checkpoint_wal_nowait`]): a
    /// reader holding an older snapshot makes it give up at once, and a waiting
    /// checkpoint here held every writer for the full busy timeout whenever a
    /// viewer was reading. When a reader does block it, a background thread
    /// keeps retrying without waiting on the writer lock, up to a bound
    /// ([`GraphStore::schedule_wal_scrub`]), so the removed value does not stay
    /// in the WAL until the last connection closes; a reader that outlasts the
    /// bound is recorded in the diagnostics file (KG-0208).
    pub fn clear_meta(&self, key: &str) -> Result<()> {
        self.secure_delete(|conn| conn.execute("DELETE FROM meta WHERE key = ?1", params![key]))?;
        if !self.checkpoint_wal_nowait()? {
            self.schedule_wal_scrub(WAL_SCRUB_BOUND, WAL_SCRUB_INTERVAL);
        }
        Ok(())
    }

    /// Retry a checkpoint a reader blocked, on a thread of its own, until it
    /// completes, `bound` passes, or the store is dropped. Each attempt is a
    /// non-waiting `wal_checkpoint(TRUNCATE)` on a second connection, so no
    /// attempt holds the writer lock while it waits for the reader: writers never
    /// stall. At most one pass runs per store; a second request while one runs is
    /// covered by it, because the attempt that succeeds folds in every frame
    /// present then. Returns `true` when a pass started, `false` for an in-memory
    /// index or a pass already running.
    ///
    /// Dropping the store stops the pass and waits for it (at most one interval),
    /// so a short-lived process neither loses the pass silently nor keeps a second
    /// connection open through its own last close. The stop makes one last
    /// attempt. A pass that ends without truncating the WAL writes KG-0208 to the
    /// diagnostics file, with the reason.
    pub(crate) fn schedule_wal_scrub(
        &self,
        bound: std::time::Duration,
        interval: std::time::Duration,
    ) -> bool {
        let Some(path) = self
            .conn
            .path()
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
        else {
            return false;
        };
        let mut slot = self.wal_scrub.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|s| !s.handle.is_finished()) {
            return false;
        }
        if let Some(finished) = slot.take() {
            let _ = finished.handle.join();
        }
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let spawned = std::thread::Builder::new()
            .name("kgraf-wal-scrub".to_owned())
            .spawn(move || {
                let outcome = retry_truncate_checkpoint(
                    std::path::Path::new(&path),
                    bound,
                    interval,
                    &stopped,
                );
                let reason = match &outcome {
                    ScrubOutcome::Done => return true,
                    ScrubOutcome::Bound => {
                        format!("a reader held an older snapshot for {} s", bound.as_secs())
                    }
                    ScrubOutcome::Stopped => {
                        "the store closed while a reader held an older snapshot".to_owned()
                    }
                    ScrubOutcome::Failed(why) => format!("the retry could not run: {why}"),
                };
                kgraf_diag::record(kgraf_diag::DiagEvent::error(
                    "KG-0208",
                    "kgraf-graph",
                    format!("a cleared record may still be in the WAL: {reason}"),
                ));
                false
            });
        match spawned {
            Ok(handle) => {
                *slot = Some(WalScrub { stop, handle });
                true
            }
            Err(_) => false,
        }
    }

    /// Wait for the running pass to end on its own; its result, or `None` when
    /// none was started.
    #[cfg(test)]
    pub(crate) fn wait_wal_scrub(&self) -> Option<bool> {
        let pass = self
            .wal_scrub
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        pass.handle.join().ok()
    }

    /// Stop the pass, if one runs, and wait for its thread to end.
    fn stop_wal_scrub(&self) {
        let pass = self
            .wal_scrub
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(pass) = pass {
            let _ = pass.stop.send(());
            let _ = pass.handle.join();
        }
    }

    /// `PRAGMA wal_checkpoint(TRUNCATE)`: copy every WAL frame into the
    /// database and truncate the WAL file, so old page images (a run record's
    /// earlier text) do not stay in it until the last connection closes. It
    /// waits for readers, up to the connection's busy timeout; a reader holding
    /// an older snapshot past that makes it return busy without an error, and
    /// the frames then go at the next checkpoint. For a caller that expects no
    /// readers (the end of a run); routine writes use
    /// [`GraphStore::checkpoint_wal_nowait`].
    pub fn checkpoint_wal(&self) -> Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// The same checkpoint without waiting: the busy timeout is zero for this
    /// one call, so a reader in the way makes it return `false` at once instead
    /// of holding the caller. `true` when the WAL was fully folded in and
    /// truncated.
    pub fn checkpoint_wal_nowait(&self) -> Result<bool> {
        self.conn.busy_timeout(std::time::Duration::ZERO)?;
        let outcome = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                row.get::<_, i64>(0)
            });
        let restored = self.conn.busy_timeout(BUSY_TIMEOUT);
        let busy = outcome?;
        restored?;
        Ok(busy == 0)
    }

    /// Run one `meta` write with `secure_delete = ON`, then return to FAST
    /// (decision 0071): the old value's overflow pages are zeroed as they are
    /// freed, so an overwritten run record does not stay readable in the
    /// file. The pragma costs no I/O; the zeroing costs one write per freed
    /// page, on a table written a few times per run step.
    fn secure_delete(
        &self,
        write: impl FnOnce(&Connection) -> rusqlite::Result<usize>,
    ) -> Result<()> {
        self.conn.pragma_update(None, "secure_delete", "ON")?;
        let written = write(&self.conn);
        let restored = self.conn.pragma_update(None, "secure_delete", "FAST");
        written?;
        restored?;
        Ok(())
    }

    /// Every project-level fact whose key starts with `prefix`, by key.
    pub fn meta_with_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM meta WHERE substr(key, 1, ?2) = ?1 ORDER BY key")?;
        let rows = stmt.query_map(params![prefix, prefix.chars().count() as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// SQLite's `PRAGMA data_version`: it changes when another connection commits
    /// to the database, and not otherwise. A poller compares it to skip reading
    /// when nothing changed.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }

    /// For each `meta` key starting with `prefix`: its value's length and a 64-bit
    /// FNV-1a hash of its bytes, computed inside SQLite so no value leaves the
    /// database. A poller compares these to read again only the values that
    /// changed; two writes in the same millisecond with the same length still
    /// differ by hash (a length and timestamp digest missed them).
    pub fn meta_digests(&self, prefix: &str) -> Result<Vec<(String, i64, i64)>> {
        self.conn.create_scalar_function(
            "kgraf_fnv64",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let bytes: &[u8] = match ctx.get_raw(0) {
                    rusqlite::types::ValueRef::Text(t) | rusqlite::types::ValueRef::Blob(t) => t,
                    _ => &[],
                };
                let mut h: u64 = 0xcbf2_9ce4_8422_2325;
                for &b in bytes {
                    h ^= u64::from(b);
                    h = h.wrapping_mul(0x0000_0100_0000_01b3);
                }
                Ok(h as i64)
            },
        )?;
        let mut stmt = self.conn.prepare(
            "SELECT key, length(value), kgraf_fnv64(value)
             FROM meta WHERE substr(key, 1, ?2) = ?1 ORDER BY key",
        )?;
        let rows = stmt.query_map(params![prefix, prefix.chars().count() as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every `meta` key starting with `prefix`, sorted.
    pub fn meta_keys(&self, prefix: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key FROM meta WHERE substr(key, 1, length(?1)) = ?1 ORDER BY key")?;
        let rows = stmt.query_map(params![prefix], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Read a project-level fact.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    // ---- files ----------------------------------------------------------

    /// The recorded state of a file, if it is indexed.
    pub fn file(&self, path: &str) -> Result<Option<FileRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, path, content_hash, size_bytes, mtime_ns, language, line_count
                 FROM files WHERE path = ?1",
                params![path],
                file_from_row,
            )
            .optional()?)
    }

    /// Every indexed file path, sorted.
    pub fn file_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM files ORDER BY path")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many files are indexed.
    pub fn file_count(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))?
            as u64)
    }

    /// How many nodes are indexed.
    pub fn node_count(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get::<_, i64>(0))?
            as u64)
    }

    /// How many edges are indexed.
    pub fn edge_count(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get::<_, i64>(0))?
            as u64)
    }

    /// Total indexed lines, which is the figure the acceptance criterion
    /// is stated in.
    pub fn line_total(&self) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(line_count), 0) FROM files",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64)
    }

    /// Insert or update a file row on its own. An indexer must not use this
    /// ahead of writing the file's nodes: the row carries the content hash, and
    /// a hash committed before its nodes makes the file read as unchanged.
    /// [`GraphStore::replace_file_nodes`] writes the row and the nodes together.
    pub fn upsert_file(&self, file: &FileRecord) -> Result<()> {
        Self::upsert_file_on(&self.conn, file)
    }

    fn upsert_file_on(conn: &Connection, file: &FileRecord) -> Result<()> {
        conn.execute(
            "INSERT INTO files(id, path, content_hash, size_bytes, mtime_ns, language, line_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                 content_hash = excluded.content_hash,
                 size_bytes   = excluded.size_bytes,
                 mtime_ns     = excluded.mtime_ns,
                 language     = excluded.language,
                 line_count   = excluded.line_count",
            params![
                file.id,
                file.path,
                file.content_hash,
                file.size_bytes as i64,
                file.mtime_ns,
                file.language.map(|l| l.as_str()),
                file.line_count,
            ],
        )?;
        Ok(())
    }

    fn set_source_root_on(
        conn: &Connection,
        file_id: &str,
        source_root: Option<&Path>,
    ) -> Result<()> {
        match source_root {
            Some(root) => conn.execute(
                "INSERT INTO file_source_roots(file_id, root) VALUES (?1, ?2)
                 ON CONFLICT(file_id) DO UPDATE SET root = excluded.root",
                params![file_id, root.to_string_lossy()],
            )?,
            None => conn.execute(
                "DELETE FROM file_source_roots WHERE file_id = ?1",
                params![file_id],
            )?,
        };
        Ok(())
    }

    /// Forget the recorded source root of the file at `path`, so its lines are
    /// read from the store's root again. Used when the store's root is found to
    /// hold the content already indexed.
    pub fn clear_file_source_root(&self, path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM file_source_roots WHERE file_id IN (SELECT id FROM files WHERE path = ?1)",
            params![path],
        )?;
        Ok(())
    }

    /// The root recorded for the file at `path`, if it was indexed from a tree
    /// other than the store's root.
    pub fn file_source_root(&self, path: &str) -> Result<Option<std::path::PathBuf>> {
        // An index built before the table existed, opened read-only, never ran the
        // schema script: no table means no recorded root. The query runs first and
        // only that one error reads as "none", so a node lookup costs one statement
        // and any other database error still surfaces.
        let found = self.conn.query_row(
            "SELECT r.root FROM file_source_roots r JOIN files f ON f.id = r.file_id
             WHERE f.path = ?1",
            params![path],
            |row| row.get::<_, String>(0),
        );
        match found {
            Ok(root) => Ok(Some(std::path::PathBuf::from(root))),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(rusqlite::Error::SqliteFailure(_, Some(message)))
                if message.starts_with("no such table: file_source_roots") =>
            {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Remove a file and, by cascade, its nodes and their edges.
    ///
    /// Any node lease on one of this file's nodes is purged in the same transaction
    /// (decision 0050): the nodes those leases named are about to stop existing, and
    /// a lease that outlives its target is exactly the vanished-target hazard
    /// this path once left open. There is no cascade from `nodes` to
    /// `node_leases` in the schema on purpose (see `replace_file_nodes`), so this is
    /// explicit rather than automatic.
    pub fn delete_file(&self, path: &str) -> Result<()> {
        let tx = WriteScope::begin(&self.conn)?;
        purge_node_leases_for_file(&tx, path)?;
        tx.execute("DELETE FROM files WHERE path = ?1", params![path])?;
        tx.commit()?;
        Ok(())
    }

    /// Open a write batch: every [`GraphStore::replace_file_nodes`] and
    /// [`GraphStore::delete_file`] until [`WriteBatch::commit`] joins one
    /// transaction. A failed write must end the batch (see [`WriteScope`]).
    /// One commit per file rewrote the same b-tree pages (every id is a hash, so
    /// inserts land at random leaves) once per file instead of once per batch.
    /// Dropped without `commit`, the batch rolls back.
    pub(crate) fn begin_batch(&self) -> Result<WriteBatch<'_>> {
        Ok(WriteBatch(WriteScope::begin(&self.conn)?))
    }

    /// Write one file's row and replace every node and edge belonging to it,
    /// in one transaction (or, inside a write batch, as part of the batch).
    ///
    /// This is the unit of incremental reindexing: one changed file, one
    /// replaced subgraph, and the change log records exactly that. The file row
    /// holds the content hash the change detector compares. It commits with the
    /// nodes or not at all, so a failure between the two can never leave a file
    /// that reads as unchanged with stale or missing nodes.
    pub fn replace_file_nodes(
        &self,
        file: &FileRecord,
        nodes: &[NodeRecord],
        edges: &[(String, String, EdgeKind, i64)],
        call_sites: &[(String, String)],
    ) -> Result<()> {
        self.replace_file_nodes_from(file, nodes, edges, call_sites, None)
    }

    /// [`GraphStore::replace_file_nodes`] for a file read from `source_root`
    /// instead of the store's root (a worktree holding a merge the checkout has
    /// not received yet). The root is recorded in the same transaction as the
    /// file's hash, so [`GraphStore::source_of`] reads the tree the hash
    /// describes. `None` clears any recorded root: the file was read from the
    /// store's root.
    pub fn replace_file_nodes_from(
        &self,
        file: &FileRecord,
        nodes: &[NodeRecord],
        edges: &[(String, String, EdgeKind, i64)],
        call_sites: &[(String, String)],
        source_root: Option<&Path>,
    ) -> Result<()> {
        let file_id = file.id.as_str();
        let tx = WriteScope::begin(&self.conn)?;
        Self::upsert_file_on(&tx, file)?;
        Self::set_source_root_on(&tx, file_id, source_root)?;
        // Deleting the file's nodes cascades to their layout rows. Capture the
        // cached positions first, so a node whose identity survives the rewrite
        // keeps its place (call depth and cluster scale included) instead of
        // falling back to its seed until the next layout refresh.
        let kept: std::collections::HashMap<String, Position> = {
            let mut stmt = tx.prepare(
                "SELECT l.node_id, l.x, l.y, l.z FROM layout l
                 JOIN nodes n ON n.id = l.node_id WHERE n.file_id = ?1",
            )?;
            let rows = stmt.query_map(params![file_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    Position {
                        x: row.get(1)?,
                        y: row.get(2)?,
                        z: row.get(3)?,
                    },
                ))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        // The ids this file had before the rewrite, with a cheap content fingerprint
        // (parent id and signature -- exactly the inputs `chunk::stable_id` hashes
        // into an id), so an id that merely got a fresh row with *unchanged* content
        // can be told apart from one whose id string was, against the odds, reused
        // for something else in the same rewrite. Decision 0050's id is content
        // derived, so the second case should only arise from a hash collision -- but
        // "should not arise" is not "cannot", and
        // the fingerprint costs nothing to keep on hand. An id absent from the new
        // set, or present with a changed fingerprint, is treated as vanished.
        let old_fingerprint: std::collections::HashMap<String, (Option<String>, String)> = {
            let mut stmt =
                tx.prepare("SELECT id, parent_id, signature FROM nodes WHERE file_id = ?1")?;
            let rows = stmt.query_map(params![file_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, Option<String>>(1)?, row.get::<_, String>(2)?),
                ))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        tx.execute("DELETE FROM nodes WHERE file_id = ?1", params![file_id])?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO nodes(id, file_id, kind, name, qualified_name, parent_id,
                                   start_line, end_line, start_byte, end_byte, signature,
                                   identity_stable)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?;
            for node in nodes {
                insert.execute(params![
                    node.id,
                    node.file_id,
                    node.kind.as_str(),
                    node.name,
                    node.qualified_name,
                    node.parent_id,
                    node.start_line,
                    node.end_line,
                    node.start_byte as i64,
                    node.end_byte as i64,
                    node.signature,
                    node.identity_stable,
                ])?;
            }
            let new_fingerprint: std::collections::HashMap<&str, (Option<&str>, &str)> = nodes
                .iter()
                .map(|n| {
                    (
                        n.id.as_str(),
                        (n.parent_id.as_deref(), n.signature.as_str()),
                    )
                })
                .collect();
            let vanished: Vec<&str> = old_fingerprint
                .iter()
                .filter(
                    |(id, (old_parent, old_sig))| match new_fingerprint.get(id.as_str()) {
                        None => true,
                        Some((new_parent, new_sig)) => {
                            old_parent.as_deref() != *new_parent || old_sig != new_sig
                        }
                    },
                )
                .map(|(id, _)| id.as_str())
                .collect();
            purge_node_leases(&tx, &vanished)?;
            let mut layout_insert = tx.prepare(
                "INSERT INTO layout(node_id, x, y, z) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(node_id) DO NOTHING",
            )?;
            for node in nodes {
                // Positions are cached rather than recomputed per request. A
                // surviving node gets its captured position back; a new node
                // gets its seed, which `layout::cache::refresh_layout` moves
                // once call edges are resolved.
                let position = match kept.get(&node.id) {
                    Some(position) => *position,
                    None => layout::position_for(layout::directory_of(&node.path), &node.id),
                };
                layout_insert.execute(params![node.id, position.x, position.y, position.z])?;
            }
            let mut edge_insert = tx.prepare(
                "INSERT INTO edges(id, src, dst, kind, weight) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET weight = excluded.weight",
            )?;
            for (src, dst, kind, weight) in edges {
                let id = kgraf_core::ids::stable_id("edge", &[src, dst, kind.as_str()]);
                edge_insert.execute(params![id, src, dst, kind.as_str(), weight])?;
            }
            let mut call_insert = tx.prepare(
                "INSERT INTO call_sites(id, caller_id, callee_name) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO NOTHING",
            )?;
            for (caller_id, callee_name) in call_sites {
                let id = kgraf_core::ids::stable_id("call", &[caller_id, callee_name]);
                call_insert.execute(params![id, caller_id, callee_name])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- nodes ----------------------------------------------------------

    /// Every node in a file, in emission order.
    pub fn nodes_in_file(&self, path: &str) -> Result<Vec<NodeRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.id, n.file_id, f.path, n.kind, n.name, n.qualified_name, n.parent_id,
                    n.start_line, n.end_line, n.start_byte, n.end_byte, n.signature,
                    n.identity_stable
             FROM nodes n JOIN files f ON f.id = n.file_id
             WHERE f.path = ?1
             ORDER BY n.start_byte, n.end_byte DESC",
        )?;
        let rows = stmt.query_map(params![path], node_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One node by id.
    pub fn node(&self, id: &str) -> Result<Option<NodeRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT n.id, n.file_id, f.path, n.kind, n.name, n.qualified_name, n.parent_id,
                        n.start_line, n.end_line, n.start_byte, n.end_byte, n.signature,
                        n.identity_stable
                 FROM nodes n JOIN files f ON f.id = n.file_id WHERE n.id = ?1",
                params![id],
                node_from_row,
            )
            .optional()?)
    }

    /// Resolve a symbol by name to its nodes.
    ///
    /// Matches a bare name or a fully qualified name, in that order of
    /// preference, and returns every candidate rather than guessing: two files
    /// may legitimately define `parse`, and silently picking one would be the
    /// silent-degradation failure the fail-loud rule forbids.
    ///
    /// A path the graph does not store verbatim is then read as the spellings
    /// a model writes: Rust `::` paths (`TaxUs::tax_cents`), a crate or module
    /// prefix (`iface::Discount`, `config.parse_config`), and a leading
    /// `crate::`, `self::` or `super::`. The graph qualifies a method with a dot
    /// and never prefixes the crate, so the path is matched by its longest tail
    /// the graph holds, and every segment before that tail must name a
    /// directory or the file stem of the defining file's path. A prefix that
    /// names no part of that path matches nothing: handing back a same-named
    /// symbol from another crate would be a silent guess.
    ///
    /// A crate-relative path (`crate::X`) names no crate here, so it returns
    /// every crate's `X`, as the bare name does; a caller that must pick one
    /// uses [`GraphStore::resolve_symbol_from`] with the requesting file.
    pub fn resolve_symbol(&self, name: &str) -> Result<Vec<NodeRecord>> {
        self.resolve_path(name, |_| true)
    }

    /// [`GraphStore::resolve_symbol`] for a lookup made from `from`, a file of
    /// the graph (the file the name was written in or for). A crate-relative
    /// path (`crate::X`, `self::X`, `super::X`) keeps only the candidates in
    /// `from`'s crate: the nearest folder above each file that holds a
    /// `Cargo.toml`. With no `from`, or a `from` in no crate, candidates in
    /// more than one crate are refused as
    /// [`GraphError::AmbiguousSymbol`](crate::GraphError::AmbiguousSymbol),
    /// which lists them, rather than handed back as if the path had chosen
    /// one. Any other name resolves as in [`GraphStore::resolve_symbol`].
    pub fn resolve_symbol_from(&self, name: &str, from: Option<&str>) -> Result<Vec<NodeRecord>> {
        let crate_relative = name
            .split("::")
            .next()
            .is_some_and(|s| matches!(s.trim(), "crate" | "self" | "super"));
        if !crate_relative {
            return self.resolve_symbol(name);
        }
        match from.and_then(|f| self.crate_dir(f)) {
            Some(own) => {
                self.resolve_path(name, |n| self.crate_dir(&n.path).as_ref() == Some(&own))
            }
            None => {
                let found = self.resolve_symbol(name)?;
                let crates: BTreeSet<Option<String>> =
                    found.iter().map(|n| self.crate_dir(&n.path)).collect();
                if crates.len() > 1 {
                    return Err(GraphError::AmbiguousSymbol {
                        name: name.to_owned(),
                        candidates: found
                            .iter()
                            .map(|n| format!("{} ({}:{})", n.qualified_name, n.path, n.start_line))
                            .collect(),
                    });
                }
                Ok(found)
            }
        }
    }

    /// The folder (relative to the root, `""` for the root itself) of the
    /// crate a graph file belongs to: the nearest ancestor folder holding a
    /// `Cargo.toml` on disk, or `None` when no folder up to the root does.
    fn crate_dir(&self, path: &str) -> Option<String> {
        let mut dir = Path::new(path).parent();
        while let Some(d) = dir {
            if self.root.join(d).join("Cargo.toml").is_file() {
                return Some(d.to_string_lossy().replace('\\', "/"));
            }
            dir = d.parent();
        }
        None
    }

    /// The lookup behind [`GraphStore::resolve_symbol`], keeping only the
    /// candidates `keep` accepts at every step, so a restriction chooses
    /// among same-named symbols before a shorter tail is tried.
    fn resolve_path(
        &self,
        name: &str,
        keep: impl Fn(&NodeRecord) -> bool,
    ) -> Result<Vec<NodeRecord>> {
        let found: Vec<NodeRecord> = self
            .symbols_named(name)?
            .into_iter()
            .filter(&keep)
            .collect();
        if !found.is_empty() {
            return Ok(found);
        }
        let segments: Vec<&str> = name
            .split("::")
            .flat_map(|s| s.split('.'))
            .map(str::trim)
            .collect();
        let skip = segments
            .iter()
            .take_while(|s| matches!(**s, "crate" | "self" | "super"))
            .count();
        let segments = &segments[skip..];
        if segments.is_empty() || segments.iter().any(|s| s.is_empty()) {
            return Err(GraphError::UnknownSymbol {
                name: name.to_owned(),
            });
        }
        for split in 0..segments.len() {
            let (prefix, tail) = segments.split_at(split);
            let tail = tail.join(".");
            if tail == name {
                continue;
            }
            let found: Vec<NodeRecord> = self
                .symbols_named(&tail)?
                .into_iter()
                .filter(|n| prefix.iter().all(|m| path_names_module(&n.path, m)) && keep(n))
                .collect();
            if !found.is_empty() {
                return Ok(found);
            }
        }
        Err(GraphError::UnknownSymbol {
            name: name.to_owned(),
        })
    }

    /// Nodes whose qualified name or bare name is exactly `name`, the exact
    /// qualified matches first.
    fn symbols_named(&self, name: &str) -> Result<Vec<NodeRecord>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT n.id, n.file_id, f.path, n.kind, n.name, n.qualified_name, n.parent_id,
                    n.start_line, n.end_line, n.start_byte, n.end_byte, n.signature,
                    n.identity_stable
             FROM nodes n JOIN files f ON f.id = n.file_id
             WHERE n.qualified_name = ?1 OR n.name = ?1
             ORDER BY (n.qualified_name = ?1) DESC, f.path, n.start_line",
        )?;
        let rows = stmt.query_map(params![name], node_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The one-hop neighborhood of a node: everything an edge connects it to, in
    /// either direction, with the edge kind and weight that connects them.
    pub fn neighbors(&self, id: &str) -> Result<Vec<(NodeRecord, EdgeKind, i64, bool)>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.id, n.file_id, f.path, n.kind, n.name, n.qualified_name, n.parent_id,
                    n.start_line, n.end_line, n.start_byte, n.end_byte, n.signature,
                    n.identity_stable,
                    e.kind, e.weight, e.src = ?1 AS outgoing
             FROM edges e
             JOIN nodes n ON n.id = CASE WHEN e.src = ?1 THEN e.dst ELSE e.src END
             JOIN files f ON f.id = n.file_id
             WHERE e.src = ?1 OR e.dst = ?1
             ORDER BY e.weight DESC, n.qualified_name",
        )?;
        let rows = stmt.query_map(params![id], |row| {
            let node = node_from_row(row)?;
            let kind: String = row.get(13)?;
            let weight: i64 = row.get(14)?;
            let outgoing: bool = row.get(15)?;
            Ok((
                node,
                EdgeKind::parse(&kind).unwrap_or(EdgeKind::Calls),
                weight,
                outgoing,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The cached position of a node.
    pub fn position(&self, id: &str) -> Result<Option<Position>> {
        Ok(self
            .conn
            .query_row(
                "SELECT x, y, z FROM layout WHERE node_id = ?1",
                params![id],
                |row| {
                    Ok(Position {
                        x: row.get(0)?,
                        y: row.get(1)?,
                        z: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Read a node's source from disk.
    ///
    /// The index holds no text, so this is the only way to get it. The bytes
    /// are read from the file's recorded source root when it has one (a file
    /// indexed from a worktree that holds a merge the checkout has not received
    /// yet), else from the store's root. A recorded root that no longer holds the
    /// indexed content (removed, or edited) falls back to the store's root. In
    /// every case the content hash of the bytes read is compared with the indexed
    /// one first: a file edited since it was indexed raises
    /// [`GraphError::StaleFile`], because a span that still fits the shorter or
    /// longer file would return the wrong lines. A span that no longer fits the
    /// file raises [`GraphError::StaleSpan`].
    pub fn source_of(&self, node: &NodeRecord) -> Result<String> {
        let indexed: Option<String> = self
            .conn
            .query_row(
                "SELECT content_hash FROM files WHERE id = ?1",
                params![node.file_id],
                |row| row.get(0),
            )
            .optional()?;
        let recorded = self.file_source_root(&node.path)?;
        let matches = |bytes: &[u8]| {
            indexed.as_deref() == Some(blake3::hash(bytes).to_hex().to_string().as_str())
        };
        let from_recorded = recorded
            .and_then(|root| std::fs::read(root.join(&node.path)).ok())
            .filter(|bytes| matches(bytes));
        let bytes = match from_recorded {
            Some(bytes) => bytes,
            None => {
                let full = self.root.join(&node.path);
                let bytes = std::fs::read(&full).map_err(|source| GraphError::Io {
                    path: node.path.clone(),
                    source,
                })?;
                if !matches(&bytes) {
                    return Err(GraphError::StaleFile {
                        node: node.id.clone(),
                        path: node.path.clone(),
                    });
                }
                bytes
            }
        };
        let text = String::from_utf8(bytes).map_err(|_| GraphError::NotUtf8 {
            path: node.path.clone(),
        })?;
        let lines: Vec<&str> = text.lines().collect();
        let start = node.start_line.saturating_sub(1) as usize;
        let end = node.end_line as usize;
        if start >= lines.len() || end > lines.len() {
            return Err(GraphError::StaleSpan {
                node: node.id.clone(),
                path: node.path.clone(),
                start: node.start_line,
                end: node.end_line,
                lines: lines.len() as u32,
            });
        }
        Ok(lines[start..end].join("\n"))
    }

    // ---- change log -----------------------------------------------------

    /// The highest change-log sequence number written so far.
    pub fn change_watermark(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM change_log", [], |row| {
                row.get(0)
            })?)
    }
}

enum ScrubOutcome {
    Done,
    Bound,
    Stopped,
    Failed(String),
}

/// Attempt `wal_checkpoint(TRUNCATE)` on a fresh connection every `interval`
/// until one completes, `bound` has passed, `stopped` fires or closes, or the
/// database cannot be opened. The connection never creates the file, and its
/// busy timeout is zero, so an attempt returns at once when a reader is in the
/// way instead of holding the writer lock.
fn retry_truncate_checkpoint(
    path: &Path,
    bound: std::time::Duration,
    interval: std::time::Duration,
    stopped: &std::sync::mpsc::Receiver<()>,
) -> ScrubOutcome {
    let conn = match Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        Err(error) => return ScrubOutcome::Failed(format!("open {}: {error}", path.display())),
    };
    if let Err(error) = conn.busy_timeout(std::time::Duration::ZERO) {
        return ScrubOutcome::Failed(error.to_string());
    }
    let deadline = std::time::Instant::now() + bound;
    loop {
        let busy = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            row.get::<_, i64>(0)
        });
        match busy {
            Ok(0) => return ScrubOutcome::Done,
            Ok(_) => {}
            Err(error) => return ScrubOutcome::Failed(error.to_string()),
        }
        if std::time::Instant::now() >= deadline {
            return ScrubOutcome::Bound;
        }
        // The wait doubles as the stop signal: a message or a closed channel
        // ends the pass at once.
        match stopped.recv_timeout(interval) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // One last attempt: a reader that let go since the previous
                // poll (a viewer's read lasts a frame) must not read as a
                // lapse. Only a reader still in the way is reported.
                return match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    row.get::<_, i64>(0)
                }) {
                    Ok(0) => ScrubOutcome::Done,
                    _ => ScrubOutcome::Stopped,
                };
            }
        }
    }
}

/// A read or facts open never creates the database file.
fn require_index_file(db_path: &Path) -> Result<()> {
    if db_path.is_file() {
        Ok(())
    } else {
        Err(GraphError::NoIndex {
            path: db_path.display().to_string(),
        })
    }
}

fn file_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    let language: Option<String> = row.get(5)?;
    Ok(FileRecord {
        id: row.get(0)?,
        path: row.get(1)?,
        content_hash: row.get(2)?,
        size_bytes: row.get::<_, i64>(3)? as u64,
        mtime_ns: row.get(4)?,
        language: language.as_deref().and_then(Language::parse),
        line_count: row.get(6)?,
    })
}

/// Whether `module` (a crate or module name from a symbol path) names a
/// directory of `path` or its file stem. A crate `tax_us` lives in `tax_us/` or
/// `tax-us/`, so `-` and `_` compare equal.
fn path_names_module(path: &str, module: &str) -> bool {
    let norm = |s: &str| s.replace('-', "_");
    let module = norm(module);
    let mut parts: Vec<&str> = path.split('/').collect();
    if let Some(file) = parts.pop() {
        let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
        parts.push(stem);
    }
    parts.iter().any(|p| norm(p) == module)
}

fn node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeRecord> {
    let kind: String = row.get(3)?;
    Ok(NodeRecord {
        id: row.get(0)?,
        file_id: row.get(1)?,
        path: row.get(2)?,
        kind: NodeKind::parse(&kind).unwrap_or(NodeKind::File),
        name: row.get(4)?,
        qualified_name: row.get(5)?,
        parent_id: row.get(6)?,
        start_line: row.get(7)?,
        end_line: row.get(8)?,
        start_byte: row.get::<_, i64>(9)? as u64,
        end_byte: row.get::<_, i64>(10)? as u64,
        signature: row.get(11)?,
        identity_stable: row.get(12)?,
    })
}

/// A write scope: a transaction when none is open, and a no-op inside one.
///
/// Takes the write lock at `BEGIN IMMEDIATE`, like every other transaction on the
/// store's connection. Dropped without `commit`, an outermost scope rolls back.
///
/// Inside an open [`WriteBatch`] the scope joins the batch instead of opening a
/// savepoint: a savepoint copies every page it first touches into a journal
/// file, which measured as the largest cost of a batched pass. The price is that
/// a failed write cannot roll back alone. A caller that holds a batch must let
/// the error end the batch (dropping it rolls everything back), never continue
/// past it.
pub(crate) struct WriteScope<'a> {
    conn: &'a Connection,
    /// Whether this scope opened the transaction.
    owns: bool,
    done: bool,
}

impl<'a> WriteScope<'a> {
    pub(crate) fn begin(conn: &'a Connection) -> Result<Self> {
        let owns = conn.is_autocommit();
        if owns {
            conn.execute_batch("BEGIN IMMEDIATE")?;
        }
        Ok(Self {
            conn,
            owns,
            done: false,
        })
    }

    pub(crate) fn commit(mut self) -> Result<()> {
        self.done = true;
        if self.owns {
            self.conn.execute_batch("COMMIT")?;
        }
        Ok(())
    }
}

impl std::ops::Deref for WriteScope<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn
    }
}

impl Drop for WriteScope<'_> {
    fn drop(&mut self) {
        if self.done || !self.owns {
            return;
        }
        // A rollback that fails leaves the connection's own state to report the
        // next statement's error; there is nothing more to do from a destructor.
        let _ = self.conn.execute_batch("ROLLBACK");
    }
}

/// An open write batch; see [`GraphStore::begin_batch`].
pub(crate) struct WriteBatch<'a>(WriteScope<'a>);

impl WriteBatch<'_> {
    pub(crate) fn commit(self) -> Result<()> {
        self.0.commit()
    }
}

/// Delete every node-lease row naming one of `vanished_ids`, in the caller's
/// transaction. A no-op, not a query, when the slice is empty -- called
/// unconditionally by `replace_file_nodes` so the empty case is the common one.
fn purge_node_leases(tx: &Connection, vanished_ids: &[&str]) -> Result<()> {
    if vanished_ids.is_empty() {
        return Ok(());
    }
    let placeholders = vanished_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("DELETE FROM node_leases WHERE node_id IN ({placeholders})");
    let params: Vec<&dyn rusqlite::ToSql> = vanished_ids
        .iter()
        .map(|id| id as &dyn rusqlite::ToSql)
        .collect();
    tx.execute(&sql, params.as_slice())?;
    Ok(())
}

/// Delete every node-lease row for a node that belongs to `path`, before that file's
/// nodes are removed. Used by `delete_file`, where the file (and so every node in it)
/// is about to stop existing outright.
fn purge_node_leases_for_file(tx: &Connection, path: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM node_leases WHERE node_id IN (
             SELECT n.id FROM nodes n JOIN files f ON f.id = n.file_id WHERE f.path = ?1
         )",
        params![path],
    )?;
    Ok(())
}

/// Dropped on a schema-version mismatch. **Derived data only.**
///
/// The original version of this constant dropped
/// `leases` and `node_leases` along with everything else, which meant an app upgrade
/// mid-session silently discarded every write lease an agent was actively holding,
/// exactly the silent degradation the fail-loud rule forbids. Lease state is not indexed from
/// source, so there is nothing to "rebuild" it from; it is dropped from this list on
/// purpose. `meta` is excluded too, because `schema_version` itself lives there and
/// `migrate` needs to read the old value before deciding to run this at all.
const DROP_DERIVED_ON_SCHEMA_CHANGE: &str = r#"
DROP TRIGGER IF EXISTS nodes_ai;
DROP TRIGGER IF EXISTS nodes_au;
DROP TRIGGER IF EXISTS nodes_ad;
DROP TRIGGER IF EXISTS layout_au;
DROP TRIGGER IF EXISTS files_ai;
DROP TRIGGER IF EXISTS files_au;
DROP TRIGGER IF EXISTS files_ad;
DROP TRIGGER IF EXISTS edges_ai;
DROP TRIGGER IF EXISTS edges_ad;
DROP TABLE IF EXISTS change_log;
DROP TABLE IF EXISTS call_sites;
DROP TABLE IF EXISTS layout;
DROP TABLE IF EXISTS edges;
DROP TABLE IF EXISTS nodes;
DROP TABLE IF EXISTS cochange;
DROP TABLE IF EXISTS task_writes;
DROP TABLE IF EXISTS collisions;
DROP TABLE IF EXISTS files;
"#;

/// The schema.
///
/// Note what is absent: no `content` column on `nodes` (locations, not text),
/// and no `status` column anywhere (facts only; status is derived).
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    id           TEXT PRIMARY KEY,
    path         TEXT NOT NULL UNIQUE,
    -- The change detector. Modification time and size are recorded but never
    -- consulted for that decision: a write preserving both is the agent
    -- workload, and the reference implementation misses it.
    content_hash TEXT NOT NULL,
    size_bytes   INTEGER NOT NULL,
    mtime_ns     INTEGER NOT NULL,
    language     TEXT,
    line_count   INTEGER NOT NULL
);

-- Where a file's indexed content lives when that is not the store's root: a
-- worktree holding a merge the checkout has not received yet. Absent for a
-- file indexed from the store's root. `source_of` still verifies the hash.
CREATE TABLE IF NOT EXISTS file_source_roots (
    file_id TEXT PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    root    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS nodes (
    id             TEXT PRIMARY KEY,
    file_id        TEXT NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL,
    name           TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    parent_id      TEXT REFERENCES nodes(id) ON DELETE CASCADE,
    start_line     INTEGER NOT NULL,
    end_line       INTEGER NOT NULL,
    start_byte     INTEGER NOT NULL,
    end_byte       INTEGER NOT NULL,
    signature      TEXT NOT NULL,
    -- Decision 0049, persisted (schema 4). False means the id is unique in this
    -- snapshot only, per `Chunk::identity_stable`'s doc. A sub-file lease request
    -- reads this column rather than trusting a caller-declared flag.
    identity_stable INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS nodes_name      ON nodes(name);
CREATE INDEX IF NOT EXISTS nodes_qualified ON nodes(qualified_name);
CREATE INDEX IF NOT EXISTS nodes_file      ON nodes(file_id);
CREATE INDEX IF NOT EXISTS nodes_kind      ON nodes(kind);

CREATE TABLE IF NOT EXISTS edges (
    id     TEXT PRIMARY KEY,
    src    TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    dst    TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    kind   TEXT NOT NULL,
    -- Weighted, never thresholded at index time (decision 0020). A value
    -- discarded here cannot be recovered by a consumer wanting another
    -- operating point.
    weight INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS edges_src ON edges(src, kind);
CREATE INDEX IF NOT EXISTS edges_dst ON edges(dst, kind);

-- Recorded call sites, kept so a reindex resolves names with SQL instead of
-- reparsing every file. Not a drawn object, so no change-log trigger: the
-- `calls` edges derived from these rows are what the interface hears about.
CREATE TABLE IF NOT EXISTS call_sites (
    id          TEXT PRIMARY KEY,
    caller_id   TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    callee_name TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS call_sites_name ON call_sites(callee_name);

-- Cached layout positions. The reference implementation promises this in a
-- header comment and recomputes on every request, so its nodes teleport.
CREATE TABLE IF NOT EXISTS layout (
    node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    x REAL NOT NULL,
    y REAL NOT NULL,
    z REAL NOT NULL
);

-- The co-change relation (decision 0020). Every observed pair is kept with its
-- count as a weight; `min_commit_size` lets a consumer apply a commit-size
-- filter at query time instead of having one applied for it at index time.
CREATE TABLE IF NOT EXISTS cochange (
    path_a          TEXT NOT NULL,
    path_b          TEXT NOT NULL,
    count           INTEGER NOT NULL,
    last_commit_ts  INTEGER NOT NULL,
    min_commit_size INTEGER NOT NULL,
    PRIMARY KEY (path_a, path_b)
);
CREATE INDEX IF NOT EXISTS cochange_a ON cochange(path_a, count DESC);
CREATE INDEX IF NOT EXISTS cochange_b ON cochange(path_b, count DESC);

-- kGraf's own observation, which is strictly richer than git: per-task write
-- sets, including work later discarded, which review strips out of history.
CREATE TABLE IF NOT EXISTS task_writes (
    task_id   TEXT NOT NULL,
    path      TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    PRIMARY KEY (task_id, path)
);

-- Observed collisions, which git structurally cannot contain because review
-- resolves the conflict before it reaches a commit. First-class because they are
-- the training signal.
CREATE TABLE IF NOT EXISTS collisions (
    id        TEXT PRIMARY KEY,
    task_a    TEXT NOT NULL,
    task_b    TEXT NOT NULL,
    path      TEXT NOT NULL,
    kind      TEXT NOT NULL,
    observed_at INTEGER NOT NULL
);

-- The write-lease registry (decision 0022): ONE global list of who is touching what.
-- `path` is the primary key, so mutual exclusion is enforced by the database rather than
-- by a check some future code path can forget. Cross-indexed by agent because both
-- directions are needed: path to agent for admission, agent to paths for teardown and
-- for reclaiming a crashed worker's set.
CREATE TABLE IF NOT EXISTS leases (
    path        TEXT PRIMARY KEY,
    agent_id    TEXT NOT NULL,
    task_id     TEXT NOT NULL,
    -- Declared, not enforced. Exclusion is per file per 0022; the range accrues because
    -- granularity is this approach's measured weak point and the data cannot be
    -- recovered later.
    range_start INTEGER,
    range_end   INTEGER,
    acquired_ms INTEGER NOT NULL,
    -- Expiry is the deadlock backstop. It claims the lease is stale, never that the
    -- worker is dead, which decision 0007 forbids concluding from a failed probe.
    expires_ms  INTEGER NOT NULL,
    generation  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS leases_agent ON leases(agent_id);

-- Every grant ever made, so the serialization rate 0022's acceptance criterion asks for
-- is measurable, and so a generation number can never be reused for a path. The range
-- columns are carried from `leases.range_start/range_end` at grant time (decision 0049)
-- so the needless-wait fraction survives past the lease that produced it, not just the
-- currently held row.
CREATE TABLE IF NOT EXISTS lease_history (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    path        TEXT NOT NULL,
    agent_id    TEXT NOT NULL,
    task_id     TEXT NOT NULL,
    generation  INTEGER NOT NULL,
    acquired_ms INTEGER NOT NULL,
    range_start INTEGER,
    range_end   INTEGER
);
CREATE INDEX IF NOT EXISTS lease_history_path ON lease_history(path);
-- The per-grant `MAX(generation) WHERE path = ?` lookup used a path-only index, so
-- SQLite scanned every historical row for a hot path to find the max instead of
-- seeking to the end of a (path, generation) ordering. Measured at 111us/op at 2,000
-- rows of history and 377us/op at 10,000 on disk before this index existed.
CREATE INDEX IF NOT EXISTS lease_history_path_generation ON lease_history(path, generation);
-- The galaxy reads a live agent's trail by (agent, task); without this it scans the
-- whole history on every lease event.
CREATE INDEX IF NOT EXISTS lease_history_agent_task ON lease_history(agent_id, task_id);

-- Sub-file leases (decisions 0049, 0050). Keyed on the hierarchical, signature-derived
-- node id from `Chunk::id` -- never a line range or a positional ordinal, both measured
-- to rebind silently. Exclusion is NOT primary-key equality here: a write lease on a node
-- conflicts with its ancestors and descendants too, which the registry computes from
-- `nodes.parent_id` at admission time, not from this table.
CREATE TABLE IF NOT EXISTS node_leases (
    node_id     TEXT PRIMARY KEY,
    agent_id    TEXT NOT NULL,
    task_id     TEXT NOT NULL,
    acquired_ms INTEGER NOT NULL,
    expires_ms  INTEGER NOT NULL,
    generation  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS node_leases_agent ON node_leases(agent_id);

-- Mirrors `lease_history` for node-level grants.
CREATE TABLE IF NOT EXISTS node_lease_history (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id     TEXT NOT NULL,
    agent_id    TEXT NOT NULL,
    task_id     TEXT NOT NULL,
    generation  INTEGER NOT NULL,
    acquired_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS node_lease_history_node ON node_lease_history(node_id);
-- Same fix as `lease_history_path_generation`, for the node-keyed table.
CREATE INDEX IF NOT EXISTS node_lease_history_node_generation
    ON node_lease_history(node_id, generation);
CREATE INDEX IF NOT EXISTS node_lease_history_agent_task
    ON node_lease_history(agent_id, task_id);

-- Bi-temporal facts (decision 0056), ported from Graphiti's edge model
-- (see `facts.rs`). Not derived data: absent from DROP_DERIVED_ON_SCHEMA_CHANGE,
-- so a schema bump keeps it. A fact that stops being true is closed (invalid_at,
-- expired_at set), never deleted (decision 0028). No change-log trigger yet: no
-- view draws facts.
CREATE TABLE IF NOT EXISTS facts (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    subject    TEXT NOT NULL,
    predicate  TEXT NOT NULL,
    object     TEXT NOT NULL,
    -- Valid time: when the fact became true, and when it stopped being true.
    valid_at   INTEGER NOT NULL,
    invalid_at INTEGER,
    -- Transaction time: when this row was recorded, and when it was superseded.
    created_at INTEGER NOT NULL,
    expired_at INTEGER,
    -- JSON array of the latest sources (episodes) that asserted it, capped
    -- (facts.rs MAX_EPISODES); episode_count counts every assertion.
    episodes   TEXT NOT NULL DEFAULT '[]',
    episode_count INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS facts_subject   ON facts(subject, predicate);
CREATE INDEX IF NOT EXISTS facts_predicate ON facts(predicate, invalid_at);

-- The event spine. Triggers write; a poller tails with a watermark. This is the
-- ONLY event source: no code path can mutate the graph without appending here.
CREATE TABLE IF NOT EXISTS change_log (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts         INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000),
    table_name TEXT NOT NULL,
    op         TEXT NOT NULL,
    row_id     TEXT NOT NULL,
    payload    TEXT
);
-- The live activity card reads the newest rows of some (table, operation)
-- pairs (`EventTail::newest_of`): one backward range of this index per pair,
-- so the read costs the rows it returns, not the size of the log (which is
-- never pruned). A read-only viewer uses it once a writer opened the store and
-- created it. Created on every open, like the tables above.
CREATE INDEX IF NOT EXISTS change_log_table_op ON change_log(table_name, op, seq);

CREATE TRIGGER IF NOT EXISTS files_ai AFTER INSERT ON files BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('files', 'insert', NEW.id, json_object('path', NEW.path));
END;
CREATE TRIGGER IF NOT EXISTS files_au AFTER UPDATE ON files BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('files', 'update', NEW.id, json_object('path', NEW.path));
END;
CREATE TRIGGER IF NOT EXISTS files_ad AFTER DELETE ON files BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('files', 'delete', OLD.id, json_object('path', OLD.path));
END;

CREATE TRIGGER IF NOT EXISTS nodes_ai AFTER INSERT ON nodes BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('nodes', 'insert', NEW.id,
            json_object('kind', NEW.kind, 'qualified_name', NEW.qualified_name,
                        'file_id', NEW.file_id));
END;
CREATE TRIGGER IF NOT EXISTS nodes_au AFTER UPDATE ON nodes BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('nodes', 'update', NEW.id,
            json_object('kind', NEW.kind, 'qualified_name', NEW.qualified_name,
                        'file_id', NEW.file_id));
END;
CREATE TRIGGER IF NOT EXISTS nodes_ad AFTER DELETE ON nodes BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('nodes', 'delete', OLD.id,
            json_object('kind', OLD.kind, 'qualified_name', OLD.qualified_name,
                        'file_id', OLD.file_id));
END;

-- A cached position moves when a node's call depth or its cluster's population
-- bucket changes. The renderer must hear it through the same spine as every
-- other drawn change, so it can animate the move (decision 0015). Inserts and
-- deletes ride on the node triggers.
CREATE TRIGGER IF NOT EXISTS layout_au AFTER UPDATE ON layout BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('layout', 'update', NEW.node_id,
            json_object('x', NEW.x, 'y', NEW.y, 'z', NEW.z));
END;

CREATE TRIGGER IF NOT EXISTS leases_ai AFTER INSERT ON leases BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('leases', 'insert', NEW.path,
            json_object('agent_id', NEW.agent_id, 'task_id', NEW.task_id,
                        'expires_ms', NEW.expires_ms, 'generation', NEW.generation));
END;
CREATE TRIGGER IF NOT EXISTS leases_au AFTER UPDATE ON leases BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('leases', 'update', NEW.path,
            json_object('agent_id', NEW.agent_id, 'task_id', NEW.task_id,
                        'expires_ms', NEW.expires_ms, 'generation', NEW.generation));
END;
CREATE TRIGGER IF NOT EXISTS leases_ad AFTER DELETE ON leases BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('leases', 'delete', OLD.path,
            json_object('agent_id', OLD.agent_id, 'task_id', OLD.task_id));
END;

CREATE TRIGGER IF NOT EXISTS node_leases_ai AFTER INSERT ON node_leases BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('node_leases', 'insert', NEW.node_id,
            json_object('agent_id', NEW.agent_id, 'task_id', NEW.task_id,
                        'expires_ms', NEW.expires_ms, 'generation', NEW.generation));
END;
CREATE TRIGGER IF NOT EXISTS node_leases_au AFTER UPDATE ON node_leases BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('node_leases', 'update', NEW.node_id,
            json_object('agent_id', NEW.agent_id, 'task_id', NEW.task_id,
                        'expires_ms', NEW.expires_ms, 'generation', NEW.generation));
END;
CREATE TRIGGER IF NOT EXISTS node_leases_ad AFTER DELETE ON node_leases BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('node_leases', 'delete', OLD.node_id,
            json_object('agent_id', OLD.agent_id, 'task_id', OLD.task_id));
END;

-- Work nodes (`work.rs`): the plan in the graph. Facts only, never a status
-- (doneness is derived from code and tests), and not derived data: absent from
-- `DROP_DERIVED_ON_SCHEMA_CHANGE`, like the lease tables, because a plan came from
-- a paid model call and the user's approval and cannot be rebuilt from source.
-- Created with IF NOT EXISTS on every open, so an existing index gains them
-- without a schema-version bump (which would rebuild the whole index).
CREATE TABLE IF NOT EXISTS work_nodes (
    id         TEXT PRIMARY KEY,
    plan_id    TEXT NOT NULL,
    kind       TEXT NOT NULL,
    parent_id  TEXT REFERENCES work_nodes(id),
    title      TEXT NOT NULL,
    body       TEXT NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS work_nodes_plan ON work_nodes(plan_id, kind);
CREATE TABLE IF NOT EXISTS work_deps (
    plan_id TEXT NOT NULL,
    src     TEXT NOT NULL REFERENCES work_nodes(id),
    dst     TEXT NOT NULL REFERENCES work_nodes(id),
    PRIMARY KEY (src, dst)
);
CREATE INDEX IF NOT EXISTS work_deps_plan ON work_deps(plan_id);

CREATE TRIGGER IF NOT EXISTS work_nodes_ai AFTER INSERT ON work_nodes BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('work_nodes', 'insert', NEW.id,
            json_object('plan_id', NEW.plan_id, 'kind', NEW.kind,
                        'parent_id', NEW.parent_id, 'title', NEW.title));
END;
CREATE TRIGGER IF NOT EXISTS work_nodes_au AFTER UPDATE ON work_nodes BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('work_nodes', 'update', NEW.id,
            json_object('plan_id', NEW.plan_id, 'kind', NEW.kind,
                        'parent_id', NEW.parent_id, 'title', NEW.title));
END;
CREATE TRIGGER IF NOT EXISTS work_deps_ai AFTER INSERT ON work_deps BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('work_deps', 'insert', NEW.src || '->' || NEW.dst,
            json_object('plan_id', NEW.plan_id, 'src', NEW.src, 'dst', NEW.dst));
END;

-- Presumed work nodes (`presumed.rs`, decision 0064): the provenance of a work node
-- read from a roadmap or plan document (or a model's reading of one) before any
-- work was approved for it. One row per presumed node. Facts only: where it was
-- read, by what, when; the content hash last written, so a re-read never
-- overwrites a node someone else changed; when its source stopped listing it; and
-- what real work adopted it. No status: done, partial and remaining are derived at
-- read time. Not derived data (absent from `DROP_DERIVED_ON_SCHEMA_CHANGE`).
CREATE TABLE IF NOT EXISTS work_presumed (
    node_id      TEXT PRIMARY KEY REFERENCES work_nodes(id),
    source_kind  TEXT NOT NULL,
    source       TEXT NOT NULL,
    section      TEXT NOT NULL,
    parser       TEXT NOT NULL,
    written_hash TEXT NOT NULL,
    first_ms     INTEGER NOT NULL,
    gone_ms      INTEGER,
    adopted_by   TEXT,
    adopted_ms   INTEGER,
    -- What the document says of the node, as of the last read: a fact about the
    -- document, outside the node's hashed content, so it stays current after an
    -- edit or an adoption.
    doc_claim      TEXT NOT NULL DEFAULT '',
    doc_claim_text TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS work_presumed_source ON work_presumed(source_kind, source);
CREATE TRIGGER IF NOT EXISTS work_presumed_ai AFTER INSERT ON work_presumed BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('work_presumed', 'insert', NEW.node_id,
            json_object('source', NEW.source, 'section', NEW.section));
END;
CREATE TRIGGER IF NOT EXISTS work_presumed_au AFTER UPDATE ON work_presumed BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('work_presumed', 'update', NEW.node_id,
            json_object('gone_ms', NEW.gone_ms, 'adopted_by', NEW.adopted_by));
END;

-- Schedules on work nodes and their run history (`work_schedule.rs`, decision
-- 0052 section 3). Not derived data: absent from `DROP_DERIVED_ON_SCHEMA_CHANGE`,
-- like the work tables, because a schedule came from the user and its history
-- records paid runs. History rows are append-only; no status column exists.
CREATE TABLE IF NOT EXISTS work_schedules (
    id         TEXT PRIMARY KEY,
    node_id    TEXT NOT NULL REFERENCES work_nodes(id),
    definition TEXT NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS work_schedules_node ON work_schedules(node_id);
CREATE TABLE IF NOT EXISTS work_schedule_history (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    schedule_id TEXT NOT NULL,
    node_id     TEXT NOT NULL REFERENCES work_nodes(id),
    event       TEXT NOT NULL,
    run_id      TEXT,
    due_ms      INTEGER,
    at_ms       INTEGER NOT NULL,
    detail      TEXT NOT NULL DEFAULT '',
    -- The run's spend read from the ledger by run id: settled micro-dollars, the
    -- call count, the calls whose spend is not known, and the label text.
    spend_micros   INTEGER,
    spend_calls    INTEGER,
    spend_unpriced INTEGER,
    spend_text     TEXT
);
CREATE INDEX IF NOT EXISTS work_schedule_history_schedule
    ON work_schedule_history(schedule_id, seq);
CREATE INDEX IF NOT EXISTS work_schedule_history_node
    ON work_schedule_history(node_id, seq);
-- One row per run event: two app instances recording the same run cannot both
-- write it (INSERT OR IGNORE against this index).
CREATE UNIQUE INDEX IF NOT EXISTS work_schedule_history_once
    ON work_schedule_history(schedule_id, event, run_id) WHERE run_id IS NOT NULL;

CREATE TRIGGER IF NOT EXISTS work_schedules_ai AFTER INSERT ON work_schedules BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('work_schedules', 'insert', NEW.id, json_object('node_id', NEW.node_id));
END;
CREATE TRIGGER IF NOT EXISTS work_schedules_ad AFTER DELETE ON work_schedules BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('work_schedules', 'delete', OLD.id, json_object('node_id', OLD.node_id));
END;
-- Permission events (decision 0094): every permission-mode change and every
-- grant a mode, the orchestrator or the user made. Append-only facts; the
-- activity card shows them from the change log.
CREATE TABLE IF NOT EXISTS permission_events (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms   INTEGER NOT NULL,
    run_id  TEXT,
    agent   TEXT,
    event   TEXT NOT NULL,
    line    TEXT NOT NULL
);
CREATE TRIGGER IF NOT EXISTS permission_events_ai AFTER INSERT ON permission_events BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('permission_events', 'insert', NEW.seq,
            json_object('event', NEW.event, 'agent', NEW.agent, 'run_id', NEW.run_id,
                        'line', NEW.line));
END;
CREATE TRIGGER IF NOT EXISTS work_schedule_history_ai AFTER INSERT ON work_schedule_history BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('work_schedule_history', 'insert', NEW.seq,
            json_object('schedule_id', NEW.schedule_id, 'node_id', NEW.node_id,
                        'event', NEW.event, 'run_id', NEW.run_id));
END;

-- Sessions (`sessions.rs`, decision 0067): every agent -- orchestrator, worker,
-- subagent or gate -- is a row here, keyed by the existing `AgentId`. Facts only
-- (`kgraf_core::session::SessionFacts`): kind, parent, route, model and the rest are
-- written once at dispatch. Ending and pausing are separate updates to the same row
-- rather than a new table, because 0067's data model treats them as later facts
-- about one session, not a new kind of row; a session's live display state is still
-- derived at read time (0007), never stored here. Not derived data (absent from
-- `DROP_DERIVED_ON_SCHEMA_CHANGE`): a session came from a paid run and cannot be
-- rebuilt from source. Created with IF NOT EXISTS on every open, like the work
-- tables, so an existing index gains these tables without a schema-version bump.
CREATE TABLE IF NOT EXISTS agent_sessions (
    id            TEXT PRIMARY KEY,
    run_id        TEXT NOT NULL,
    kind          TEXT NOT NULL,
    parent_id     TEXT REFERENCES agent_sessions(id),
    contract      TEXT,
    route         TEXT,
    model         TEXT,
    depth         INTEGER NOT NULL,
    transcript    TEXT,
    worktree      TEXT,
    -- JSON array of strings (`SessionFacts::write_scope`).
    write_scope   TEXT NOT NULL,
    reasoning     TEXT NOT NULL,
    started_ms    INTEGER NOT NULL,
    -- Decision 0067: 0 for a session that never
    -- takes a user note (a future merge-conflict resolver); 1 otherwise.
    addressable   INTEGER NOT NULL DEFAULT 1,
    -- Lifecycle facts (`SessionRecords::end_session`, `::set_paused`), written by a
    -- later UPDATE, never part of the initial upsert's own conflict overwrite.
    ended_outcome TEXT,
    ended_ms      INTEGER,
    paused_ms     INTEGER
);
CREATE INDEX IF NOT EXISTS agent_sessions_run ON agent_sessions(run_id);
CREATE INDEX IF NOT EXISTS agent_sessions_parent ON agent_sessions(parent_id);

-- One row per message to a session (`InboxItem`). `seq` orders delivery FIFO
-- (`SessionRecords::undelivered`) independent of `id`, which is the caller's own
-- message id and the key `mark_delivered` looks it up by.
CREATE TABLE IF NOT EXISTS session_inbox (
    seq            INTEGER PRIMARY KEY AUTOINCREMENT,
    id             TEXT NOT NULL UNIQUE,
    session_id     TEXT NOT NULL REFERENCES agent_sessions(id),
    -- JSON (`InboxSource` carries data for the `Agent` case).
    source         TEXT NOT NULL,
    kind           TEXT NOT NULL,
    mode           TEXT NOT NULL,
    text           TEXT NOT NULL,
    received_ms    INTEGER NOT NULL,
    not_before_ms  INTEGER NOT NULL,
    delivered_seq  INTEGER,
    delivered_ms   INTEGER
);
CREATE INDEX IF NOT EXISTS session_inbox_session ON session_inbox(session_id, seq);

-- One row per connection between two sessions in one run (`ConnectionFacts`), keyed
-- by `kgraf_core::session::connection_id`'s canonical pair id.
CREATE TABLE IF NOT EXISTS agent_connections (
    id          TEXT PRIMARY KEY,
    run_id      TEXT NOT NULL,
    a           TEXT NOT NULL REFERENCES agent_sessions(id),
    b           TEXT NOT NULL REFERENCES agent_sessions(id),
    -- JSON (`ConnectionBasis` and `ConnectionState` both carry data on some variants).
    basis       TEXT NOT NULL,
    state       TEXT NOT NULL,
    decided_by  TEXT,
    decided_ms  INTEGER,
    messages    INTEGER NOT NULL DEFAULT 0,
    last_ms     INTEGER
);
CREATE INDEX IF NOT EXISTS agent_connections_a ON agent_connections(a);
CREATE INDEX IF NOT EXISTS agent_connections_b ON agent_connections(b);
-- `connections_of_run` (`sessions.rs`) filters by `run_id` alone; without this
-- index that query is a full table scan of `agent_connections`.
CREATE INDEX IF NOT EXISTS agent_connections_run ON agent_connections(run_id);

-- Every trigger row below names the node `agent:<session id>` (0067's data model),
-- so the live activity card's routing back to the graph (0063) works the same way
-- for a session event as for any other change-log row. There is no `AFTER DELETE`
-- trigger on any of the three tables below: a session, an inbox message and a
-- connection are all facts (this module's doc comment), never deleted, so no
-- delete path exists here by design, unlike `work_schedules`'s own removable rows.
CREATE TRIGGER IF NOT EXISTS agent_sessions_ai AFTER INSERT ON agent_sessions BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('agent_sessions', 'insert', 'agent:' || NEW.id,
            json_object('run_id', NEW.run_id, 'kind', NEW.kind, 'parent_id', NEW.parent_id));
END;
CREATE TRIGGER IF NOT EXISTS agent_sessions_au AFTER UPDATE ON agent_sessions BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('agent_sessions', 'update', 'agent:' || NEW.id,
            json_object('run_id', NEW.run_id, 'kind', NEW.kind,
                        'ended_outcome', NEW.ended_outcome, 'paused_ms', NEW.paused_ms));
END;

CREATE TRIGGER IF NOT EXISTS session_inbox_ai AFTER INSERT ON session_inbox BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('session_inbox', 'insert', 'agent:' || NEW.session_id,
            json_object('id', NEW.id, 'kind', NEW.kind, 'source', NEW.source));
END;
CREATE TRIGGER IF NOT EXISTS session_inbox_au AFTER UPDATE ON session_inbox BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('session_inbox', 'update', 'agent:' || NEW.session_id,
            json_object('id', NEW.id, 'delivered_seq', NEW.delivered_seq));
END;

-- A connection touches two agents, so each event is heard on both nodes' timelines
-- (two change-log rows, one per side), rather than an arbitrary pick of one.
CREATE TRIGGER IF NOT EXISTS agent_connections_ai AFTER INSERT ON agent_connections BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('agent_connections', 'insert', 'agent:' || NEW.a,
            json_object('id', NEW.id, 'b', NEW.b, 'state', NEW.state));
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('agent_connections', 'insert', 'agent:' || NEW.b,
            json_object('id', NEW.id, 'a', NEW.a, 'state', NEW.state));
END;
CREATE TRIGGER IF NOT EXISTS agent_connections_au AFTER UPDATE ON agent_connections BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('agent_connections', 'update', 'agent:' || NEW.a,
            json_object('id', NEW.id, 'b', NEW.b, 'state', NEW.state));
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('agent_connections', 'update', 'agent:' || NEW.b,
            json_object('id', NEW.id, 'a', NEW.a, 'state', NEW.state));
END;

CREATE TRIGGER IF NOT EXISTS edges_ai AFTER INSERT ON edges BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('edges', 'insert', NEW.id,
            json_object('src', NEW.src, 'dst', NEW.dst, 'kind', NEW.kind));
END;
CREATE TRIGGER IF NOT EXISTS edges_ad AFTER DELETE ON edges BEGIN
    INSERT INTO change_log(table_name, op, row_id, payload)
    VALUES ('edges', 'delete', OLD.id,
            json_object('src', OLD.src, 'dst', OLD.dst, 'kind', OLD.kind));
END;
"#;

#[cfg(test)]
mod wal_scrub_tests {
    use super::*;
    use std::time::Duration;

    fn pinned(path: &Path) -> Connection {
        let reader = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("reader");
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM meta;")
            .expect("pin a snapshot");
        reader
    }

    /// A reader that outlasts the bound makes the pass report failure; one
    /// pass runs per store; the pass never holds the writer lock while it
    /// waits, so a write during it succeeds at once.
    #[test]
    fn a_reader_that_outlasts_the_bound_is_reported_and_writers_are_never_held() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("graph.db");
        let store = GraphStore::open(&path, dir.path()).expect("open");
        store.set_meta("k", &"v".repeat(20_000)).expect("set");
        let reader = pinned(&path);
        store.set_meta("k", "small").expect("overwrite");

        assert!(
            store.schedule_wal_scrub(Duration::from_secs(30), Duration::from_millis(10)),
            "the first request starts a pass"
        );
        assert!(
            !store.schedule_wal_scrub(Duration::from_secs(30), Duration::from_millis(10)),
            "a second request while one pass runs starts no second thread"
        );
        let started = std::time::Instant::now();
        store
            .set_meta("other", "value")
            .expect("write during the pass");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a write waited on the background pass"
        );
        // Releasing the reader lets the long pass finish and succeed.
        reader.execute_batch("COMMIT;").expect("release");
        assert_eq!(
            store.wait_wal_scrub(),
            Some(true),
            "the pass completed once freed"
        );

        // Held past a short bound: the pass gives up and says so.
        let reader = pinned(&path);
        store.set_meta("k", &"w".repeat(20_000)).expect("set");
        store.set_meta("k", "tiny").expect("overwrite");
        assert!(
            store.schedule_wal_scrub(Duration::from_millis(100), Duration::from_millis(10)),
            "a pass can start again after one ends"
        );
        assert_eq!(store.wait_wal_scrub(), Some(false), "the bound passed");
        drop(reader);
    }

    /// Dropping the store ends a pass that a reader still blocks, and does so
    /// within an interval instead of after the 30 s bound.
    #[test]
    fn dropping_the_store_stops_the_pass_promptly() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("graph.db");
        let store = GraphStore::open(&path, dir.path()).expect("open");
        store.set_meta("k", &"v".repeat(20_000)).expect("set");
        let reader = pinned(&path);
        store.set_meta("k", "small").expect("overwrite");
        assert!(store.schedule_wal_scrub(Duration::from_secs(30), Duration::from_millis(10)));
        let started = std::time::Instant::now();
        drop(store);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "drop waited for the bound"
        );
        drop(reader);
    }

    /// The retry connection never creates a missing database file.
    #[test]
    fn the_retry_never_creates_a_missing_database() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("absent.db");
        let (_keep, stopped) = std::sync::mpsc::channel::<()>();
        let outcome = retry_truncate_checkpoint(
            &path,
            Duration::from_millis(50),
            Duration::from_millis(10),
            &stopped,
        );
        assert!(matches!(outcome, ScrubOutcome::Failed(_)));
        assert!(!path.exists(), "the retry created the file");
    }

    /// A stop that arrives after the reader let go is not a lapse: the final
    /// attempt truncates the WAL and the pass reports nothing.
    #[test]
    fn a_stop_after_the_reader_let_go_completes_instead_of_reporting_a_lapse() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("graph.db");
        let store = GraphStore::open(&path, dir.path()).expect("open");
        store.set_meta("k", &"v".repeat(20_000)).expect("set");
        let reader = pinned(&path);
        store.set_meta("k", "small").expect("overwrite");
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn({
            let path = path.clone();
            move || {
                retry_truncate_checkpoint(
                    &path,
                    Duration::from_secs(30),
                    Duration::from_secs(10),
                    &stopped,
                )
            }
        });
        // The first attempt finds the reader; release it, then stop the pass
        // before its next poll.
        std::thread::sleep(Duration::from_millis(300));
        reader.execute_batch("COMMIT;").expect("release");
        stop.send(()).expect("stop");
        assert!(matches!(worker.join().expect("join"), ScrubOutcome::Done));

        // A reader still in the way at the stop is a real lapse.
        let reader = pinned(&path);
        store.set_meta("k", &"w".repeat(20_000)).expect("set");
        store.set_meta("k", "tiny").expect("overwrite");
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        stop.send(()).expect("stop");
        let outcome = retry_truncate_checkpoint(
            &path,
            Duration::from_secs(30),
            Duration::from_secs(10),
            &stopped,
        );
        assert!(matches!(outcome, ScrubOutcome::Stopped));
        drop(reader);
    }

    #[test]
    fn an_in_memory_index_has_no_pass() {
        let store = GraphStore::open_in_memory(Path::new(".")).expect("open");
        assert!(!store.schedule_wal_scrub(Duration::from_secs(1), Duration::from_millis(10)));
    }
}
