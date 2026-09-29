//! Read-only queries for the galaxy view.
//!
//! The renderer draws three populations from one store: code nodes and edges with
//! their cached layout positions (0007, 0058), and the agents and work items that
//! act on them. The agent data is the lease registry (0022, 0048, 0050): a held
//! lease is the one record of "who is acting on what" that is written live, and it
//! reaches the change log through the lease triggers. The run-state record (0056)
//! describes the same leases but is not persisted yet, and the orchestrator has no
//! worker registry yet, so the registry tables are the live source.
//!
//! Nothing here writes, and [`open_existing`] cannot: it opens the database with
//! `SQLITE_OPEN_READ_ONLY` ([`GraphStore::open_read_only`]) and never runs the
//! schema migration. Every multi-query read runs inside one `BEGIN DEFERRED`
//! transaction, so the rows and the change-log watermark come from one snapshot:
//! a tail resumed from that watermark sees exactly the changes the snapshot lacks.
//! Each read also reports whether an index pass was in progress in that snapshot
//! ([`crate::index::INDEX_IN_PROGRESS`]), since mid-pass a rewritten file's
//! incoming cross-file call edges are briefly absent.
//!
//! Leases expire by time (0048), which writes nothing: an expired lease stays in
//! its table until the registry reclaims it. So "live" is `expires_ms > now`,
//! and each activity read reports the next expiry so a tailer knows when to look
//! again without a change-log row to prompt it.

use crate::chunk::NodeKind;
use crate::error::{GraphError, Result};
use crate::layout::{LayoutError, Position};
use crate::store::{EdgeKind, GraphStore};
use rusqlite::params_from_iter;
use std::path::{Path, PathBuf};

/// The project's state directory, relative to its root. Matches `kgraf-cli`,
/// which creates it; the index is gitignored there.
pub const STATE_DIR: &str = kgraf_core::legacy::STATE_DIR;
/// The graph database's file name inside [`STATE_DIR`].
pub const GRAPH_DB: &str = "graph.db";

/// Where a project's graph database lives.
pub fn graph_db_path(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join(GRAPH_DB)
}

/// What opening a project directory found.
pub enum ProjectIndex {
    /// No index: the directory was never indexed (or is not a directory).
    Missing { db_path: PathBuf },
    /// An index from another schema version. Opening it read-write would rebuild
    /// it empty (`GraphStore::migrate`), so it is reported, not opened.
    OtherSchema {
        db_path: PathBuf,
        found: Option<i64>,
        expected: i64,
    },
    /// An index this build reads.
    Ready(GraphStore),
}

impl std::fmt::Debug for ProjectIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { db_path } => {
                f.debug_struct("Missing").field("db_path", db_path).finish()
            }
            Self::OtherSchema {
                db_path,
                found,
                expected,
            } => f
                .debug_struct("OtherSchema")
                .field("db_path", db_path)
                .field("found", found)
                .field("expected", expected)
                .finish(),
            Self::Ready(store) => f.debug_tuple("Ready").field(&store.root()).finish(),
        }
    }
}

/// Opens an existing index read-only, without creating or migrating one.
///
/// [`GraphStore::open`] creates a missing database and rebuilds one of another
/// version, and a viewer must do neither, so this opens with
/// `SQLITE_OPEN_READ_ONLY` and checks the schema version itself.
pub fn open_existing(root: &Path) -> Result<ProjectIndex> {
    let db_path = graph_db_path(root);
    if !db_path.is_file() {
        return Ok(ProjectIndex::Missing { db_path });
    }
    // `open_read_only_checked` refuses another schema version; here that is a state to
    // report, not an error.
    match GraphStore::open_read_only_checked(&db_path, root) {
        Ok(store) => Ok(ProjectIndex::Ready(store)),
        Err(GraphError::SchemaMismatch {
            found, expected, ..
        }) => Ok(ProjectIndex::OtherSchema {
            db_path,
            found,
            expected,
        }),
        Err(other) => Err(other),
    }
}

/// One code node as the galaxy draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneNode {
    pub id: String,
    /// Repository-relative path of the node's file (its directory is the cluster).
    pub path: String,
    pub kind: NodeKind,
    /// For the selection label in the view.
    pub qualified_name: String,
    /// The cached layout position (never recomputed here).
    pub position: Position,
}

/// One code edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SceneEdge {
    pub id: String,
    pub src: String,
    pub dst: String,
    pub kind: EdgeKind,
}

/// How an agent relates to a node.
///
/// There is no "written by the task" kind: `task_writes` has no change-log trigger
/// and no production writer yet, so a view built on it could not stay live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActivityKind {
    /// A lease held now and not expired (file or node).
    Holds,
    /// A lease the agent held earlier in the same task (lease history) and does
    /// not hold now. History is written at grant, so a held lease also has a
    /// history row; it is reported once, as `Holds`.
    Touched,
}

/// One live relation between an agent and a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    pub agent: String,
    pub task: String,
    pub kind: ActivityKind,
    /// The leased path; for a node lease, the node's file.
    pub path: String,
    /// The node drawn for it: the leased node, or the file node of a leased path.
    /// `None` when the path is not in the index (a new file, or one the indexer
    /// skips), so the relation is reported and not silently dropped.
    pub node_id: Option<String>,
}

/// Everything the galaxy draws, as one consistent read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphSnapshot {
    pub nodes: Vec<SceneNode>,
    pub edges: Vec<SceneEdge>,
    pub activity: Vec<Activity>,
    /// The change-log sequence number the snapshot includes; tail from here.
    pub watermark: i64,
    /// When the earliest live lease expires (ms since the epoch). Expiry writes
    /// nothing, so a tailer re-reads activity at this time on its own.
    pub next_expiry_ms: Option<i64>,
    /// Start time of an index pass that was running in this snapshot, if any.
    pub index_in_progress: Option<i64>,
}

/// Milliseconds since the epoch, the unit of lease times.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The current state of the rows a change batch touched.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resolved {
    /// Touched nodes that exist now.
    pub nodes: Vec<SceneNode>,
    /// Touched node ids that no longer exist.
    pub removed_nodes: Vec<String>,
    /// Touched edges that exist now.
    pub edges: Vec<SceneEdge>,
    /// Touched edge ids that no longer exist.
    pub removed_edges: Vec<String>,
    /// The whole activity set, when the batch touched a lease.
    pub activity: Option<Vec<Activity>>,
    /// With `activity`: when the earliest live lease expires.
    pub next_expiry_ms: Option<i64>,
    /// Start time of an index pass that was running in this snapshot, if any. A
    /// reader should hold the batch and resolve again once the pass ends.
    pub index_in_progress: Option<i64>,
}

/// Runs `f` inside one deferred (snapshot) read transaction.
fn read_tx<T>(store: &GraphStore, f: impl FnOnce(&rusqlite::Connection) -> Result<T>) -> Result<T> {
    let conn = store.conn();
    conn.execute_batch("BEGIN DEFERRED")?;
    let out = f(conn);
    // A read-only transaction: commit and rollback are equivalent. Rollback also
    // ends the transaction when `f` failed, so the connection is left usable.
    let end = conn.execute_batch("ROLLBACK");
    let value = out?;
    end?;
    Ok(value)
}

const NODE_SELECT: &str = "SELECT n.id, f.path, n.kind, l.x, l.y, l.z, n.qualified_name
     FROM nodes n JOIN files f ON f.id = n.file_id
     LEFT JOIN layout l ON l.node_id = n.id";

fn node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(SceneNodeRow, Option<Position>)> {
    let x: Option<f32> = row.get(3)?;
    let y: Option<f32> = row.get(4)?;
    let z: Option<f32> = row.get(5)?;
    let position = match (x, y, z) {
        (Some(x), Some(y), Some(z)) => Some(Position { x, y, z }),
        _ => None,
    };
    Ok((
        SceneNodeRow {
            id: row.get(0)?,
            path: row.get(1)?,
            kind: row.get(2)?,
            qualified_name: row.get(6)?,
        },
        position,
    ))
}

struct SceneNodeRow {
    id: String,
    path: String,
    kind: String,
    qualified_name: String,
}

/// A node without a layout row, or of an unknown kind, is an error (fail loud): the
/// store writes both with the node, so either means the index is out of step.
fn finish_node(row: SceneNodeRow, position: Option<Position>) -> Result<SceneNode> {
    let position = position.ok_or_else(|| LayoutError::StaleCache {
        node: row.id.clone(),
    })?;
    let kind = NodeKind::parse(&row.kind).ok_or_else(|| GraphError::UnknownKind {
        what: "node",
        value: row.kind.clone(),
    })?;
    Ok(SceneNode {
        id: row.id,
        path: row.path,
        kind,
        qualified_name: row.qualified_name,
        position,
    })
}

fn edge_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, String, String)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn finish_edge((id, src, dst, kind): (String, String, String, String)) -> Result<SceneEdge> {
    let kind = EdgeKind::parse(&kind).ok_or_else(|| GraphError::UnknownKind {
        what: "edge",
        value: kind.clone(),
    })?;
    Ok(SceneEdge { id, src, dst, kind })
}

fn read_all_nodes(conn: &rusqlite::Connection) -> Result<Vec<SceneNode>> {
    // No ORDER BY: slot order is irrelevant to drawing, and ordering by the text
    // primary key turns the table scan into an index walk: 499 ms with it, 133 ms
    // without, for a 100,275-node, 286,166-edge snapshot (benches/project.rs).
    let mut stmt = conn.prepare(NODE_SELECT)?;
    let rows = stmt.query_map([], node_from_row)?;
    let mut out = Vec::new();
    for row in rows {
        let (row, position) = row?;
        out.push(finish_node(row, position)?);
    }
    Ok(out)
}

fn read_all_edges(conn: &rusqlite::Connection) -> Result<Vec<SceneEdge>> {
    let mut stmt = conn.prepare("SELECT id, src, dst, kind FROM edges")?;
    let rows = stmt.query_map([], edge_from_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(finish_edge(row?)?);
    }
    Ok(out)
}

/// Live agents' held and touched leases, and the earliest live expiry.
///
/// "Live" means holding at least one unexpired lease at `now`. History is limited
/// to the `(agent, task)` pairs that are live, so a finished task's trail does not
/// accumulate on screen, and excludes what the agent holds now (history rows are
/// written at grant). A file lease is drawn on the file's node.
fn read_activity(conn: &rusqlite::Connection, now: i64) -> Result<(Vec<Activity>, Option<i64>)> {
    const FILE_NODE: &str = "(SELECT n.id FROM nodes n JOIN files f ON f.id = n.file_id
                              WHERE f.path = {path} AND n.kind = 'file')";
    let file_node = |col: &str| FILE_NODE.replace("{path}", col);
    let sql = format!(
        "WITH live_file AS (SELECT * FROM leases WHERE expires_ms > ?1),
              live_node AS (SELECT * FROM node_leases WHERE expires_ms > ?1),
              live_pairs AS (SELECT agent_id, task_id FROM live_file
                             UNION SELECT agent_id, task_id FROM live_node)
         SELECT agent_id, task_id, 0, path, {held_file}
           FROM live_file
         UNION ALL
         SELECT nl.agent_id, nl.task_id, 0, f.path, nl.node_id
           FROM live_node nl
           LEFT JOIN nodes n ON n.id = nl.node_id
           LEFT JOIN files f ON f.id = n.file_id
         UNION ALL
         SELECT DISTINCT h.agent_id, h.task_id, 1, h.path, {hist_file}
           FROM lease_history h
           JOIN live_pairs p ON p.agent_id = h.agent_id AND p.task_id = h.task_id
           WHERE NOT EXISTS (SELECT 1 FROM live_file l
                             WHERE l.path = h.path AND l.agent_id = h.agent_id)
         UNION ALL
         SELECT DISTINCT h.agent_id, h.task_id, 1, f.path, h.node_id
           FROM node_lease_history h
           JOIN live_pairs p ON p.agent_id = h.agent_id AND p.task_id = h.task_id
           JOIN nodes n ON n.id = h.node_id
           JOIN files f ON f.id = n.file_id
           WHERE NOT EXISTS (SELECT 1 FROM live_node l
                             WHERE l.node_id = h.node_id AND l.agent_id = h.agent_id)
         ORDER BY 2, 1, 3, 4",
        held_file = file_node("live_file.path"),
        hist_file = file_node("h.path"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([now], |row| {
        let kind: i64 = row.get(2)?;
        let path: Option<String> = row.get(3)?;
        let node_id: Option<String> = row.get(4)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            kind,
            path,
            node_id,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (agent, task, kind, path, node_id) = row?;
        let kind = if kind == 0 {
            ActivityKind::Holds
        } else {
            ActivityKind::Touched
        };
        // A node lease whose node vanished has no path; the lease is still shown
        // as a relation to nothing rather than dropped.
        out.push(Activity {
            agent,
            task,
            kind,
            path: path.unwrap_or_default(),
            node_id,
        });
    }
    let next_expiry: Option<i64> = conn.query_row(
        "SELECT MIN(e) FROM (SELECT MIN(expires_ms) AS e FROM leases WHERE expires_ms > ?1
                             UNION ALL
                             SELECT MIN(expires_ms) FROM node_leases WHERE expires_ms > ?1)",
        [now],
        |row| row.get(0),
    )?;
    Ok((out, next_expiry))
}

/// The start time of the earliest running index pass, if any marker is present:
/// the bare key or any per-pass key (`index_in_progress/<token>`).
fn index_in_progress(conn: &rusqlite::Connection) -> Result<Option<i64>> {
    let values: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT value FROM meta WHERE key = ?1 OR substr(key, 1, ?2) = ?3")?;
        let prefix = format!("{}/", crate::index::INDEX_IN_PROGRESS);
        let rows = stmt.query_map(
            rusqlite::params![crate::index::INDEX_IN_PROGRESS, prefix.len() as i64, prefix],
            |row| row.get::<_, String>(0),
        )?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    Ok(values.iter().map(|v| v.parse::<i64>().unwrap_or(0)).min())
}

/// The start time of a running index pass, if one is marked (one indexed read, for
/// a poller deciding whether to resolve now or wait for the pass to end).
pub fn index_pass_running(store: &GraphStore) -> Result<Option<i64>> {
    index_in_progress(store.conn())
}

fn watermark(conn: &rusqlite::Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM change_log", [], |row| {
            row.get(0)
        })?,
    )
}

/// Reads every node, edge and live activity plus the change-log watermark, in one
/// snapshot.
pub fn read_snapshot(store: &GraphStore) -> Result<GraphSnapshot> {
    read_snapshot_at(store, now_ms())
}

/// [`read_snapshot`] with lease liveness judged at `now` (ms since the epoch).
pub fn read_snapshot_at(store: &GraphStore, now: i64) -> Result<GraphSnapshot> {
    read_tx(store, |conn| {
        let (activity, next_expiry_ms) = read_activity(conn, now)?;
        Ok(GraphSnapshot {
            watermark: watermark(conn)?,
            index_in_progress: index_in_progress(conn)?,
            nodes: read_all_nodes(conn)?,
            edges: read_all_edges(conn)?,
            activity,
            next_expiry_ms,
        })
    })
}

/// SQLite's default bound-parameter limit is 32,766; chunks stay well below it.
const ID_CHUNK: usize = 500;

/// The current state of the given node and edge ids, and the activity set when
/// `activity` is set, in one snapshot.
///
/// This is how a change batch is applied idempotently: the log says which rows
/// were touched, and the current rows say what they are now. A file rewrite
/// deletes and reinserts every node of the file (`replace_file_nodes`); resolving
/// to current state turns that into "unchanged" instead of a removal and an
/// insert.
pub fn resolve(
    store: &GraphStore,
    node_ids: &[String],
    edge_ids: &[String],
    activity: bool,
) -> Result<Resolved> {
    resolve_at(store, node_ids, edge_ids, activity, now_ms())
}

/// [`resolve`] with lease liveness judged at `now` (ms since the epoch).
pub fn resolve_at(
    store: &GraphStore,
    node_ids: &[String],
    edge_ids: &[String],
    activity: bool,
    now: i64,
) -> Result<Resolved> {
    read_tx(store, |conn| {
        let mut out = Resolved {
            index_in_progress: index_in_progress(conn)?,
            ..Resolved::default()
        };
        let mut found = std::collections::HashSet::new();
        for chunk in node_ids.chunks(ID_CHUNK) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut stmt = conn.prepare(&format!("{NODE_SELECT} WHERE n.id IN ({marks})"))?;
            let rows = stmt.query_map(params_from_iter(chunk), node_from_row)?;
            for row in rows {
                let (row, position) = row?;
                let node = finish_node(row, position)?;
                found.insert(node.id.clone());
                out.nodes.push(node);
            }
        }
        out.removed_nodes = node_ids
            .iter()
            .filter(|id| !found.contains(id.as_str()))
            .cloned()
            .collect();
        found.clear();
        for chunk in edge_ids.chunks(ID_CHUNK) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut stmt = conn.prepare(&format!(
                "SELECT id, src, dst, kind FROM edges WHERE id IN ({marks})"
            ))?;
            let rows = stmt.query_map(params_from_iter(chunk), edge_from_row)?;
            for row in rows {
                let edge = finish_edge(row?)?;
                found.insert(edge.id.clone());
                out.edges.push(edge);
            }
        }
        out.removed_edges = edge_ids
            .iter()
            .filter(|id| !found.contains(id.as_str()))
            .cloned()
            .collect();
        if activity {
            let (acts, next) = read_activity(conn, now)?;
            out.activity = Some(acts);
            out.next_expiry_ms = next;
        }
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Indexer, LeaseRegistry};
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) {
        let full = root.join(rel);
        fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        fs::write(full, body).expect("write");
    }

    fn indexed() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "a/one.rs",
            "pub fn top() { low(); }\npub fn low() {}\n",
        );
        write(dir.path(), "b/two.rs", "pub fn apart() {}\n");
        fs::create_dir_all(dir.path().join(STATE_DIR)).expect("state");
        let store = GraphStore::open(&graph_db_path(dir.path()), dir.path()).expect("store");
        Indexer::new(&store).index().expect("index");
        (dir, store)
    }

    #[test]
    fn a_missing_index_is_reported_and_not_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let found = open_existing(dir.path()).expect("open");
        assert!(matches!(found, ProjectIndex::Missing { .. }), "{found:?}");
        assert!(
            !dir.path().join(STATE_DIR).exists(),
            "a viewer must not create an index"
        );
    }

    #[test]
    fn another_schema_is_reported_and_left_intact() {
        let (dir, store) = indexed();
        store.set_meta("schema_version", "1").expect("meta");
        let nodes = store.node_count().expect("count");
        drop(store);
        let found = open_existing(dir.path()).expect("open");
        assert!(
            matches!(found, ProjectIndex::OtherSchema { found: Some(1), .. }),
            "{found:?}"
        );
        // Still there: nothing rebuilt it.
        let conn = rusqlite::Connection::open(graph_db_path(dir.path())).expect("raw");
        let still: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .expect("count");
        assert_eq!(still as u64, nodes);
    }

    #[test]
    fn snapshot_matches_the_store_and_its_cached_positions() {
        let (dir, store) = indexed();
        drop(store);
        let ProjectIndex::Ready(store) = open_existing(dir.path()).expect("open") else {
            panic!("expected a ready index");
        };
        let snap = read_snapshot(&store).expect("snapshot");
        assert_eq!(snap.nodes.len() as u64, store.node_count().expect("n"));
        assert_eq!(snap.edges.len() as u64, store.edge_count().expect("e"));
        assert_eq!(snap.watermark, store.change_watermark().expect("w"));
        for node in &snap.nodes {
            assert_eq!(
                Some(node.position),
                store.position(&node.id).expect("position"),
                "{}",
                node.id
            );
        }
        assert!(snap.activity.is_empty());
    }

    #[test]
    fn a_failed_read_leaves_the_connection_usable() {
        let (_dir, store) = indexed();
        let err = read_tx(&store, |conn| {
            conn.execute_batch("SELECT * FROM no_such_table")?;
            Ok(())
        });
        assert!(err.is_err());
        // The transaction was ended; a second one can begin.
        read_snapshot(&store).expect("snapshot after a failed read");
    }

    #[test]
    fn resolve_reports_current_state_and_removals() {
        let (dir, store) = indexed();
        let before = read_snapshot(&store).expect("snapshot");
        let apart = before
            .nodes
            .iter()
            .find(|n| n.path == "b/two.rs" && n.kind == NodeKind::Function)
            .expect("apart")
            .id
            .clone();
        let top = before
            .nodes
            .iter()
            .find(|n| n.path == "a/one.rs" && n.kind == NodeKind::Function)
            .expect("top")
            .id
            .clone();
        fs::remove_file(dir.path().join("b/two.rs")).expect("rm");
        Indexer::new(&store).index().expect("reindex");
        let r = resolve(&store, &[apart.clone(), top.clone()], &[], false).expect("resolve");
        assert_eq!(r.removed_nodes, vec![apart]);
        assert_eq!(r.nodes.len(), 1);
        assert_eq!(r.nodes[0].id, top);
        assert!(r.activity.is_none());
    }

    #[test]
    fn the_viewer_connection_is_read_only_and_never_migrates() {
        let (dir, store) = indexed();
        let before = store.change_watermark().expect("w");
        drop(store);
        let ProjectIndex::Ready(view) = open_existing(dir.path()).expect("open") else {
            panic!("expected a ready index");
        };
        let err = view
            .set_meta("probe", "x")
            .expect_err("a write through the viewer must fail");
        assert!(format!("{err:?}").contains("ReadOnly"), "{err:?}");
        assert_eq!(view.change_watermark().expect("w"), before);
    }

    #[test]
    fn a_running_index_pass_is_visible_to_readers() {
        let (_dir, store) = indexed();
        assert_eq!(read_snapshot(&store).expect("s").index_in_progress, None);
        store
            .set_meta(crate::index::INDEX_IN_PROGRESS, "1234")
            .expect("marker");
        assert_eq!(
            read_snapshot(&store).expect("s").index_in_progress,
            Some(1234)
        );
        assert_eq!(
            resolve(&store, &[], &[], false)
                .expect("r")
                .index_in_progress,
            Some(1234)
        );
        // A completed pass clears its own marker and never another pass's.
        Indexer::new(&store).index().expect("index");
        assert_eq!(
            read_snapshot(&store).expect("s").index_in_progress,
            Some(1234)
        );
        store
            .clear_meta(crate::index::INDEX_IN_PROGRESS)
            .expect("clear");
        Indexer::new(&store).index().expect("index");
        assert_eq!(read_snapshot(&store).expect("s").index_in_progress, None);
    }

    #[test]
    fn expired_leases_are_not_live_and_the_next_expiry_is_reported() {
        let (_dir, store) = indexed();
        let reg = LeaseRegistry::new(&store).with_ttl_ms(1_000);
        let got = reg
            .acquire_all("w1", "t1", &["b/two.rs".to_owned()], &[None], 10_000)
            .expect("acquire");
        assert!(matches!(got, crate::Acquisition::Granted(_)));
        let live = read_snapshot_at(&store, 10_500).expect("s");
        assert_eq!(live.activity.len(), 1, "{:#?}", live.activity);
        assert_eq!(live.next_expiry_ms, Some(11_000));
        // Past expiry: the row is still in the table (expiry writes nothing), but
        // the agent is not live.
        let late = read_snapshot_at(&store, 11_000).expect("s");
        assert!(late.activity.is_empty(), "{:#?}", late.activity);
        assert_eq!(late.next_expiry_ms, None);
        let r = resolve_at(&store, &[], &[], true, 11_000).expect("r");
        assert_eq!(r.activity, Some(Vec::new()));
    }

    #[test]
    fn activity_covers_file_and_node_leases_and_history_once() {
        let (_dir, store) = indexed();
        let snap = read_snapshot(&store).expect("snapshot");
        let file_node = |path: &str| {
            snap.nodes
                .iter()
                .find(|n| n.path == path && n.kind == NodeKind::File)
                .expect("file node")
                .id
                .clone()
        };
        let low = snap
            .nodes
            .iter()
            .find(|n| n.path == "a/one.rs" && n.kind == NodeKind::Function)
            .map(|n| n.id.clone())
            .expect("fn");
        let reg = LeaseRegistry::new(&store);
        // Held then released: history only.
        let first = reg
            .acquire_all("w1", "t1", &["b/two.rs".to_owned()], &[None], 1_000)
            .expect("acquire");
        let crate::Acquisition::Granted(leases) = first else {
            panic!("granted expected");
        };
        for lease in leases {
            reg.release(lease).expect("release");
        }
        let second = reg
            .acquire_node_set("w1", "t1", std::slice::from_ref(&low), 2_000)
            .expect("node lease");
        assert!(matches!(second, crate::NodeAcquisition::Granted(_)));

        let now = 2_500;
        let acts = read_snapshot_at(&store, now).expect("snapshot").activity;
        let has = |kind, node: &str| {
            acts.iter()
                .any(|a| a.kind == kind && a.node_id.as_deref() == Some(node))
        };
        assert!(has(ActivityKind::Holds, &low), "{acts:#?}");
        assert!(
            has(ActivityKind::Touched, &file_node("b/two.rs")),
            "{acts:#?}"
        );
        // The held node has a history row too (written at grant); it is reported
        // once, as held.
        assert!(!has(ActivityKind::Touched, &low), "{acts:#?}");
        assert_eq!(acts.len(), 2, "{acts:#?}");
        assert!(acts.iter().all(|a| a.agent == "w1" && a.task == "t1"));

        // Once the agent holds nothing, it and its trail leave the live set.
        reg.release_node_all("w1").expect("release");
        assert!(
            read_snapshot_at(&store, now)
                .expect("snapshot")
                .activity
                .is_empty()
        );
    }
}
