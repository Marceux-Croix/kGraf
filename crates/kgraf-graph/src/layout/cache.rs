//! Writing call depth and cluster scale into the cached layout.
//!
//! The store caches a node's seed position when the node first appears
//! ([`super::position_for`]: depth 0, smallest disc), which is before `calls`
//! edges are resolved, and carries a surviving node's cached position across a
//! file rewrite. [`refresh_layout`] runs after call resolution (the indexer
//! calls it) and moves each node to [`super::position`] with its real depth and
//! its cluster's population. It writes only the rows whose position changed,
//! in one transaction, so a node that did not move is not written and a reader
//! never sees half a refresh.

use super::LayoutError;
use crate::error::Result;
use crate::store::GraphStore;
use rusqlite::params;
use std::collections::HashMap;

/// What one refresh did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayoutRefresh {
    /// Nodes considered.
    pub nodes: usize,
    /// Cached positions rewritten, because the node's depth or its cluster's
    /// population bucket changed (or it was new and left its seed).
    pub moved: usize,
    /// The deepest call depth in the graph.
    pub max_depth: u32,
}

/// One cached row: node id, repository path, cached position.
struct Row {
    id: String,
    path: String,
    cached: super::Position,
}

/// Recompute call depth over the store's `calls` edges and cluster populations
/// over its nodes, and rewrite the cached position of every node whose
/// position changed.
pub fn refresh_layout(store: &GraphStore) -> Result<LayoutRefresh> {
    let conn = store.conn();
    let rows = read_rows(conn)?;
    let index: HashMap<&str, u32> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| (row.id.as_str(), i as u32))
        .collect();

    let mut calls: Vec<(u32, u32)> = Vec::new();
    for (src, dst) in read_calls(conn)? {
        // Every edge endpoint is a node and every node has a layout row, so a
        // miss means the cache is out of step with the graph. That is a defect
        // to surface, not an edge to drop (fail loud).
        let lookup = |id: &str| {
            index
                .get(id)
                .copied()
                .ok_or_else(|| LayoutError::StaleCache {
                    node: id.to_owned(),
                })
        };
        calls.push((lookup(&src)?, lookup(&dst)?));
    }
    let depths = super::call_depths(rows.len(), &calls)?;

    let mut population: HashMap<&str, usize> = HashMap::new();
    for row in &rows {
        *population
            .entry(super::cluster_key(super::directory_of(&row.path)))
            .or_default() += 1;
    }

    // Compare in memory and write only what moved: a steady-state refresh is
    // then reads only, and the change log (one row per update) carries only
    // real motion.
    let mut changed: Vec<(&str, super::Position)> = Vec::new();
    for (row, depth) in rows.iter().zip(&depths) {
        let directory = super::directory_of(&row.path);
        let target = super::position(
            directory,
            &row.id,
            *depth,
            population[super::cluster_key(directory)],
        );
        if target != row.cached {
            changed.push((row.id.as_str(), target));
        }
    }
    write_positions(conn, &changed)?;

    Ok(LayoutRefresh {
        nodes: rows.len(),
        moved: changed.len(),
        max_depth: depths.iter().copied().max().unwrap_or(0),
    })
}

fn read_rows(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<Row>> {
    let mut stmt = conn.prepare(
        "SELECT l.node_id, f.path, l.x, l.y, l.z
         FROM layout l
         JOIN nodes n ON n.id = l.node_id
         JOIN files f ON f.id = n.file_id
         ORDER BY l.node_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Row {
            id: row.get(0)?,
            path: row.get(1)?,
            cached: super::Position {
                x: row.get(2)?,
                y: row.get(3)?,
                z: row.get(4)?,
            },
        })
    })?;
    rows.collect()
}

fn read_calls(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT src, dst FROM edges WHERE kind = 'calls'")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// Write the changed rows in one transaction through one prepared statement.
fn write_positions(
    conn: &rusqlite::Connection,
    changed: &[(&str, super::Position)],
) -> rusqlite::Result<()> {
    if changed.is_empty() {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    {
        let mut update =
            tx.prepare("UPDATE layout SET x = ?1, y = ?2, z = ?3 WHERE node_id = ?4")?;
        for (id, p) in changed {
            update.execute(params![p.x, p.y, p.z, id])?;
        }
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Indexer;
    use std::fs;

    fn write(root: &std::path::Path, rel: &str, body: &str) {
        let full = root.join(rel);
        fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        fs::write(full, body).expect("write");
    }

    fn cached(store: &GraphStore) -> HashMap<String, crate::Position> {
        let conn = store.conn();
        let mut stmt = conn
            .prepare("SELECT node_id, x, y, z FROM layout")
            .expect("prepare");
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                crate::Position {
                    x: row.get(1)?,
                    y: row.get(2)?,
                    z: row.get(3)?,
                },
            ))
        })
        .expect("query")
        .collect::<rusqlite::Result<HashMap<_, _>>>()
        .expect("rows")
    }

    fn bits(p: &crate::Position) -> (u32, u32, u32) {
        (p.x.to_bits(), p.y.to_bits(), p.z.to_bits())
    }

    fn id_of(store: &GraphStore, name: &str) -> String {
        store.resolve_symbol(name).expect("resolve")[0].id.clone()
    }

    fn z_of(store: &GraphStore, name: &str) -> f32 {
        store
            .position(&id_of(store, name))
            .expect("position")
            .expect("cached")
            .z
    }

    const CHAIN: &str =
        "pub fn entry() { middle(); }\npub fn middle() { leaf(); }\npub fn leaf() {}\n";

    #[test]
    fn the_indexer_sinks_callees_below_their_callers() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a/chain.rs", CHAIN);
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        let report = Indexer::new(&store).index().expect("index");
        assert_eq!(report.layout_rows_moved, 2, "{report:?}");

        assert!(z_of(&store, "entry") > z_of(&store, "middle"));
        assert!(z_of(&store, "middle") > z_of(&store, "leaf"));
        assert_eq!(
            z_of(&store, "leaf"),
            super::super::z_for(&id_of(&store, "leaf"), 2)
        );

        // A second refresh with nothing changed writes nothing.
        let again = refresh_layout(&store).expect("again");
        assert_eq!(again.moved, 0);
        assert_eq!(again.max_depth, 2);
    }

    #[test]
    fn editing_one_line_keeps_every_cached_position() {
        // The reported defect: a rewrite deleted the file's rows, the cascade
        // took their layout rows, and the reinsert wrote depth 0.
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a/chain.rs", CHAIN);
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        let indexer = Indexer::new(&store);
        indexer.index().expect("first");
        let before = cached(&store);
        assert!(z_of(&store, "leaf") < -80.0, "leaf is two layers down");

        write(
            dir.path(),
            "a/chain.rs",
            "pub fn entry() { middle(); }\npub fn middle() { leaf(); }\npub fn leaf() { let _x = 1; }\n",
        );
        let report = indexer.index().expect("second");
        assert_eq!(report.files_changed, 1);
        assert_eq!(report.layout_rows_moved, 0, "{report:?}");
        let after = cached(&store);
        assert_eq!(before.len(), after.len());
        for (id, p) in &before {
            assert_eq!(
                bits(p),
                bits(&after[id]),
                "node {id} moved on a one-line edit"
            );
        }
    }

    #[test]
    fn a_file_rewrite_carries_positions_before_any_refresh() {
        // The store half of the fix, without the indexer's refresh behind it:
        // rewriting a file's rows must not put a surviving node back at its seed.
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a/chain.rs", CHAIN);
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        Indexer::new(&store).index().expect("index");
        let before = cached(&store);

        let nodes = store.nodes_in_file("a/chain.rs").expect("nodes");
        let file_id = nodes[0].file_id.clone();
        store
            .replace_file_nodes(&file_id, &nodes, &[], &[])
            .expect("replace");
        let after = cached(&store);
        for node in &nodes {
            assert_eq!(bits(&before[&node.id]), bits(&after[&node.id]));
        }
    }

    #[test]
    fn inserting_a_node_elsewhere_moves_no_cached_position() {
        // The cached mechanism, not the formula: index, insert a file in
        // another directory that calls only into itself, reindex, and compare
        // every pre-existing row bit for bit.
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a/one.rs", CHAIN);
        write(
            dir.path(),
            "b/two.rs",
            "pub fn other() { helper(); }\npub fn helper() {}\n",
        );
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        let indexer = Indexer::new(&store);
        indexer.index().expect("first");
        let before = cached(&store);

        write(
            dir.path(),
            "c/three.rs",
            "pub fn newcomer() { local(); }\npub fn local() {}\n",
        );
        let report = indexer.index().expect("second");
        let after = cached(&store);
        for (id, p) in &before {
            assert_eq!(
                bits(p),
                bits(&after[id]),
                "node {id} moved when an unrelated node was inserted"
            );
        }
        // Only the newcomer's own callee left its seed.
        assert_eq!(report.layout_rows_moved, 1, "{report:?}");
    }

    #[test]
    fn a_new_caller_moves_only_what_it_reaches() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "a/one.rs",
            "pub fn top() { low(); }\npub fn low() {}\n",
        );
        write(dir.path(), "b/two.rs", "pub fn apart() {}\n");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        let indexer = Indexer::new(&store);
        indexer.index().expect("first");
        let before = cached(&store);

        write(dir.path(), "c/three.rs", "pub fn boss() { top(); }\n");
        indexer.index().expect("second");
        let after = cached(&store);

        let (top, low, apart) = (
            id_of(&store, "top"),
            id_of(&store, "low"),
            id_of(&store, "apart"),
        );
        assert_eq!(before[&apart], after[&apart]);
        for id in [&top, &low] {
            assert_eq!(before[id].x, after[id].x, "X is not a function of depth");
            assert_eq!(before[id].y, after[id].y, "Y is not a function of depth");
            assert!(
                (before[id].z - after[id].z - super::super::Z_DEPTH_SPACING).abs() < 1e-3,
                "a node one call deeper sinks one layer"
            );
        }
    }

    #[test]
    fn a_test_calling_main_demotes_the_whole_tree_below_main() {
        // One call edge into a root. `main` stops being a root, so it and
        // everything it reaches sink one layer; nothing else moves.
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "app/main.rs",
            "pub fn main() { load(); run(); }\npub fn load() { parse(); }\n\
             pub fn run() { step(); }\npub fn parse() {}\npub fn step() {}\n",
        );
        write(
            dir.path(),
            "lib/util.rs",
            "pub fn standalone() { helper(); }\npub fn helper() {}\n",
        );
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        let indexer = Indexer::new(&store);
        indexer.index().expect("first");
        let before = cached(&store);

        write(dir.path(), "tests/smoke.rs", "pub fn smoke() { main(); }\n");
        let report = indexer.index().expect("second");
        let after = cached(&store);

        let demoted = ["main", "load", "run", "parse", "step"];
        for name in demoted {
            let id = id_of(&store, name);
            assert!(
                (before[&id].z - after[&id].z - super::super::Z_DEPTH_SPACING).abs() < 1e-3,
                "{name} sinks one layer"
            );
        }
        let moved_existing = before
            .iter()
            .filter(|(id, p)| bits(p) != bits(&after[*id]))
            .count();
        assert_eq!(moved_existing, demoted.len());
        // Plus the new caller's nothing: smoke is a root at its seed depth.
        assert_eq!(report.layout_rows_moved, demoted.len() as u64);
    }
}
