//! Sub-file leases, keyed on the hierarchical node id (decisions 0049, 0050).
//!
//! **The key.** `Chunk::id`, never a line range or a positional ordinal, both were
//! measured to rebind silently (0050's identity-survival run: 1.01% of unchanged line
//! ranges and 12 positional ids rebound to different code across 449 ruff commits,
//! against zero for the hierarchical scheme). `range_start`/`range_end` on the
//! file-level `leases` table stay measurement-only, as 0050 requires; they are never
//! read here.
//!
//! **The conflict shape.** A write lease on a node conflicts with a lease on any of its
//! ancestors or descendants, and with no lease on a sibling. Node ids are opaque
//! signature hashes (`kgraf-tools::Access`'s doc comment makes the same point for
//! the same reason), so containment cannot be read off the id string; it is read from
//! `nodes.parent_id`, walked both up and down in one recursive query
//! ([`related_node_ids`]).
//!
//! **A node lease also conflicts with a whole-file lease on its file, and vice versa.**
//! The first version of this module had two registries that never
//! checked each other at all, an agent could hold the whole file while a second agent
//! took a node inside it. [`LeaseRegistry::acquire_node_set`] and
//! [`super::LeaseRegistry::acquire_all`] are both thin wrappers over
//! [`super::combined::acquire`], which checks both tables in one transaction; see that
//! module for why a single admission path is the only way to make this true safely.
//!
//! **`identity_stable` is read from the graph, not taken from the caller (decision
//! 0049).** An earlier version had the caller declare it, mirroring `declared_range`
//! on the file-level registry. The asymmetry is a defect: a
//! *measurement* the registry itself never checks is a fact taken on faith, and a
//! caller with a stale copy of the graph, or a hand-built request, can simply
//! assert stability that is not there. `nodes.identity_stable` is populated by the
//! indexer at the moment `Chunk::identity_stable` is computed, and read back here in
//! the same query that checks existence, so both checks come from one source of
//! truth. [`NodeAcquisition::Unstable`] is the registry refusing to mint a key it
//! cannot promise stays attached to the right code; the caller's fallback is a
//! whole-file [`super::LeaseRegistry::acquire_all`].
//!
//! **A vanished id invalidates its lease loudly (0050).** Existence is checked against
//! `nodes` at acquire and renewal time. On reindex, `GraphStore::replace_file_nodes`
//! and `GraphStore::delete_file` purge any node lease naming an id that stopped
//! existing, in the same transaction as the deletion, so there is no window in which a
//! coincidentally reused id could renew a lease that was issued for different code.

use super::LeaseRegistry;
use crate::error::Result;
use rusqlite::{OptionalExtension, params};
use std::collections::BTreeSet;

/// Permission to write one node, and proof the registry granted it.
///
/// No public constructor, exactly as [`super::WriteLease`]: the only source is
/// [`LeaseRegistry::acquire_node_set`] (via [`super::combined::acquire`]), which has
/// already written the row.
#[derive(Debug)]
pub struct NodeLease {
    node_id: String,
    agent_id: String,
    generation: i64,
}

impl NodeLease {
    /// `pub(super)`, not public: only `leases::combined` mints one, after writing the
    /// row in the same transaction. See [`super::WriteLease::new`] for why this is
    /// the same pattern, not a relaxation of it.
    pub(super) fn new(node_id: String, agent_id: String, generation: i64) -> Self {
        Self {
            node_id,
            agent_id,
            generation,
        }
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn generation(&self) -> i64 {
        self.generation
    }
}

/// One row of the node-lease table, as anyone may read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeLeaseRecord {
    pub node_id: String,
    pub agent_id: String,
    pub task_id: String,
    pub acquired_ms: i64,
    pub expires_ms: i64,
    pub generation: i64,
}

impl NodeLeaseRecord {
    pub fn is_expired(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_ms
    }
}

/// Why a node acquisition did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeConflict {
    pub node_id: String,
    /// Set when the block came from another node lease: the node actually holding
    /// it (`node_id` itself, an ancestor, or a descendant). Mutually exclusive with
    /// `blocking_path`.
    pub conflicting_node_id: Option<String>,
    /// Set when the block came from a whole-file lease covering this node's file,
    /// rather than another node lease. Mutually exclusive with `conflicting_node_id`.
    pub blocking_path: Option<String>,
    pub held_by: String,
    pub held_task: String,
    pub expires_ms: i64,
}

/// The outcome of asking for a set of node leases. All-or-nothing, for the same
/// deadlock-prevention reason as [`super::Acquisition`].
#[derive(Debug)]
pub enum NodeAcquisition {
    Granted(Vec<NodeLease>),
    /// Nothing was granted; these ids conflict with a lease already held by another
    /// agent, at the node itself, an ancestor, a descendant, or the whole file.
    Blocked(Vec<NodeConflict>),
    /// Nothing was granted; these ids are `identity_stable = false` in the graph
    /// (0049) and cannot be leased at node granularity. The caller leases the whole
    /// file instead.
    Unstable(Vec<String>),
    /// Nothing was granted; these ids no longer exist in the graph. Never carried to
    /// a "nearest" element (0050), the caller re-derives ids from the current
    /// snapshot and retries.
    Vanished(Vec<String>),
}

impl LeaseRegistry<'_> {
    /// Acquire a whole set of node leases, or nothing.
    ///
    /// A thin wrapper over [`super::combined::acquire`], which is the only place
    /// admission logic lives: it is the single transaction that checks both this
    /// table and the file-level `leases` table, in both directions, so a node lease
    /// and a whole-file lease on its file can never both be granted at once.
    pub fn acquire_node_set(
        &self,
        agent_id: &str,
        task_id: &str,
        node_ids: &[String],
        now_ms: i64,
    ) -> Result<NodeAcquisition> {
        match self.acquire(agent_id, task_id, &[], &[], node_ids, now_ms)? {
            super::combined::MixedAcquisition::Granted { paths, nodes } => {
                debug_assert!(paths.is_empty(), "no paths were requested");
                Ok(NodeAcquisition::Granted(nodes))
            }
            super::combined::MixedAcquisition::Blocked { paths, nodes } => {
                debug_assert!(paths.is_empty(), "no paths were requested");
                Ok(NodeAcquisition::Blocked(nodes))
            }
            super::combined::MixedAcquisition::Unstable(ids) => Ok(NodeAcquisition::Unstable(ids)),
            super::combined::MixedAcquisition::Vanished(ids) => Ok(NodeAcquisition::Vanished(ids)),
        }
    }

    /// Renew a held node lease.
    ///
    /// Fails fenced (`Ok(false)`) when the generation no longer matches, or **when the
    /// row has already lapsed as of `now_ms`**, even if the generation still matches
    /// and nobody else has taken the node yet. The original
    /// version renewed a stale row unconditionally, which let a lapsed parent lease
    /// come back to life over a child a second agent had since been granted. See
    /// [`super::LeaseRegistry::renew`]'s doc for the identical fix on the file-level
    /// registry.
    ///
    /// A vanished target is a different kind of failure and raises instead (fail loud):
    /// the lease is deleted so nothing can renew it again, and the caller is told why,
    /// rather than reading an ordinary `false` and assuming mere contention.
    pub fn renew_node(&self, lease: &NodeLease, now_ms: i64) -> Result<bool> {
        let conn = self.store.conn();
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM nodes WHERE id = ?1",
                params![lease.node_id],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            conn.execute(
                "DELETE FROM node_leases WHERE node_id = ?1 AND agent_id = ?2 AND generation = ?3",
                params![lease.node_id, lease.agent_id, lease.generation],
            )?;
            return Err(crate::error::GraphError::VanishedLeaseTarget {
                node_id: lease.node_id.clone(),
            });
        }
        let changed = conn.execute(
            "UPDATE node_leases SET expires_ms = ?1
             WHERE node_id = ?2 AND agent_id = ?3 AND generation = ?4 AND expires_ms > ?5",
            params![
                now_ms + self.ttl_ms,
                lease.node_id,
                lease.agent_id,
                lease.generation,
                now_ms,
            ],
        )?;
        Ok(changed == 1)
    }

    /// Release one node lease. Consumes it, so a released lease cannot be used again.
    pub fn release_node(&self, lease: NodeLease) -> Result<()> {
        self.store.conn().execute(
            "DELETE FROM node_leases WHERE node_id = ?1 AND agent_id = ?2 AND generation = ?3",
            params![lease.node_id, lease.agent_id, lease.generation],
        )?;
        Ok(())
    }

    /// Release every node lease an agent holds.
    pub fn release_node_all(&self, agent_id: &str) -> Result<usize> {
        Ok(self.store.conn().execute(
            "DELETE FROM node_leases WHERE agent_id = ?1",
            params![agent_id],
        )?)
    }

    /// Who holds a node right now, if anyone whose lease has not lapsed.
    pub fn node_holder_of(&self, node_id: &str, now_ms: i64) -> Result<Option<NodeLeaseRecord>> {
        Ok(self
            .read_node_one(node_id)?
            .filter(|l| !l.is_expired(now_ms)))
    }

    /// Everything this agent holds at node granularity.
    pub fn held_nodes_by(&self, agent_id: &str) -> Result<Vec<NodeLeaseRecord>> {
        let conn = self.store.conn();
        let mut stmt = conn.prepare(
            "SELECT node_id, agent_id, task_id, acquired_ms, expires_ms, generation
             FROM node_leases WHERE agent_id = ?1 ORDER BY node_id",
        )?;
        let rows = stmt.query_map(params![agent_id], node_lease_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The whole node-lease list.
    pub fn all_nodes(&self) -> Result<Vec<NodeLeaseRecord>> {
        let conn = self.store.conn();
        let mut stmt = conn.prepare(
            "SELECT node_id, agent_id, task_id, acquired_ms, expires_ms, generation
             FROM node_leases ORDER BY node_id",
        )?;
        let rows = stmt.query_map([], node_lease_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn read_node_one(&self, node_id: &str) -> Result<Option<NodeLeaseRecord>> {
        Ok(self
            .store
            .conn()
            .query_row(
                "SELECT node_id, agent_id, task_id, acquired_ms, expires_ms, generation
                 FROM node_leases WHERE node_id = ?1",
                params![node_id],
                node_lease_from_row,
            )
            .optional()?)
    }
}

fn node_lease_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeLeaseRecord> {
    Ok(NodeLeaseRecord {
        node_id: row.get(0)?,
        agent_id: row.get(1)?,
        task_id: row.get(2)?,
        acquired_ms: row.get(3)?,
        expires_ms: row.get(4)?,
        generation: row.get(5)?,
    })
}

/// Every ancestor and descendant of `id`, excluding `id` itself.
///
/// One bidirectional recursive query rather than two, since both directions are
/// needed for every conflict check: `up` walks `parent_id` to the root, `down` walks
/// it away from the root. `pub(super)` because [`super::combined::acquire`] is the
/// only other caller.
pub(super) fn related_node_ids(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
) -> rusqlite::Result<BTreeSet<String>> {
    let mut stmt = tx.prepare(
        "WITH RECURSIVE
             up(id) AS (
                 SELECT parent_id FROM nodes WHERE id = ?1 AND parent_id IS NOT NULL
                 UNION ALL
                 SELECT n.parent_id FROM nodes n JOIN up ON n.id = up.id
                 WHERE n.parent_id IS NOT NULL
             ),
             down(id) AS (
                 SELECT id FROM nodes WHERE parent_id = ?1
                 UNION ALL
                 SELECT n.id FROM nodes n JOIN down d ON n.parent_id = d.id
             )
         SELECT id FROM up
         UNION
         SELECT id FROM down",
    )?;
    let rows = stmt.query_map(params![id], |row| row.get::<_, String>(0))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeKind;
    use crate::error::GraphError;
    use crate::store::{FileRecord, GraphStore, NodeRecord};

    fn store() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        (dir, store)
    }

    /// Insert one node directly, bypassing the chunker: these tests are about the
    /// registry's conflict arithmetic over `nodes.parent_id`, not about chunking.
    fn insert_node(store: &GraphStore, id: &str, parent: Option<&str>) {
        insert_node_stability(store, id, parent, true);
    }

    fn insert_node_stability(store: &GraphStore, id: &str, parent: Option<&str>, stable: bool) {
        store
            .conn()
            .execute(
                "INSERT INTO files(id, path, content_hash, size_bytes, mtime_ns, language, line_count)
                 VALUES ('f', 'f.rs', 'h', 0, 0, NULL, 1)
                 ON CONFLICT(id) DO NOTHING",
                [],
            )
            .expect("file");
        store
            .conn()
            .execute(
                "INSERT INTO nodes(id, file_id, kind, name, qualified_name, parent_id,
                                   start_line, end_line, start_byte, end_byte, signature,
                                   identity_stable)
                 VALUES (?1, 'f', 'function', ?1, ?1, ?2, 1, 1, 0, 0, '', ?3)",
                params![id, parent, stable],
            )
            .expect("node");
    }

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_node_lease_conflicts_with_a_lease_on_its_ancestor() {
        let (_dir, store) = store();
        insert_node(&store, "grandparent", None);
        insert_node(&store, "parent", Some("grandparent"));
        insert_node(&store, "child", Some("parent"));
        let registry = LeaseRegistry::new(&store);

        registry
            .acquire_node_set("agent-a", "t1", &ids(&["grandparent"]), 1_000)
            .expect("acquire");

        let attempt = registry
            .acquire_node_set("agent-b", "t2", &ids(&["child"]), 1_000)
            .expect("acquire");
        match attempt {
            NodeAcquisition::Blocked(conflicts) => {
                assert_eq!(conflicts.len(), 1);
                assert_eq!(
                    conflicts[0].conflicting_node_id.as_deref(),
                    Some("grandparent")
                );
                assert_eq!(conflicts[0].held_by, "agent-a");
            }
            other => panic!("an ancestor's lease must block a descendant: {other:?}"),
        }
    }

    #[test]
    fn a_node_lease_conflicts_with_a_lease_on_its_descendant() {
        let (_dir, store) = store();
        insert_node(&store, "parent", None);
        insert_node(&store, "child", Some("parent"));
        let registry = LeaseRegistry::new(&store);

        registry
            .acquire_node_set("agent-a", "t1", &ids(&["child"]), 1_000)
            .expect("acquire");

        let attempt = registry
            .acquire_node_set("agent-b", "t2", &ids(&["parent"]), 1_000)
            .expect("acquire");
        assert!(
            matches!(attempt, NodeAcquisition::Blocked(_)),
            "a descendant's lease must block an ancestor"
        );
    }

    #[test]
    fn siblings_do_not_conflict() {
        let (_dir, store) = store();
        insert_node(&store, "parent", None);
        insert_node(&store, "sibling-a", Some("parent"));
        insert_node(&store, "sibling-b", Some("parent"));
        let registry = LeaseRegistry::new(&store);

        registry
            .acquire_node_set("agent-a", "t1", &ids(&["sibling-a"]), 1_000)
            .expect("acquire");

        let attempt = registry
            .acquire_node_set("agent-b", "t2", &ids(&["sibling-b"]), 1_000)
            .expect("acquire");
        assert!(
            matches!(attempt, NodeAcquisition::Granted(_)),
            "two siblings must not conflict: {attempt:?}"
        );
    }

    #[test]
    fn a_chunk_declared_unstable_in_the_graph_is_refused_not_leased() {
        let (_dir, store) = store();
        insert_node_stability(&store, "ambiguous", None, false);
        let registry = LeaseRegistry::new(&store);

        let attempt = registry
            .acquire_node_set("agent-a", "t1", &ids(&["ambiguous"]), 1_000)
            .expect("acquire");
        assert!(
            matches!(attempt, NodeAcquisition::Unstable(ids) if ids == vec!["ambiguous".to_owned()])
        );
    }

    /// Regression: when identity stability was
    /// still a caller-declared flag, asserting `true` for a chunk the graph itself
    /// recorded as unstable was granted without complaint, because the registry never
    /// checked. Ported as a regression test: the graph's own column is now what is
    /// read, so there is nothing left for a caller to lie about.
    #[test]
    fn a_caller_cannot_override_the_graphs_own_instability_record() {
        let (_dir, store) = store();
        insert_node_stability(&store, "ambiguous", None, false);
        let registry = LeaseRegistry::new(&store);
        let attempt = registry
            .acquire_node_set("agent-a", "t1", &ids(&["ambiguous"]), 1_000)
            .expect("acquire");
        assert!(
            matches!(attempt, NodeAcquisition::Unstable(_)),
            "the graph says this id is not identity-stable; no caller-supplied flag \
             can be asked for any more, so there is no way to grant it: {attempt:?}"
        );
    }

    #[test]
    fn a_vanished_node_id_is_refused_loudly_not_granted() {
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store);
        // Never inserted: simulates a reindex that removed the element.
        let attempt = registry
            .acquire_node_set("agent-a", "t1", &ids(&["ghost"]), 1_000)
            .expect("acquire");
        assert!(
            matches!(attempt, NodeAcquisition::Vanished(ids) if ids == vec!["ghost".to_owned()])
        );
    }

    #[test]
    fn renewing_a_lease_whose_node_vanished_raises_and_deletes_it() {
        let (_dir, store) = store();
        insert_node(&store, "n1", None);
        let registry = LeaseRegistry::new(&store).with_ttl_ms(10_000);
        let lease = match registry
            .acquire_node_set("agent-a", "t1", &ids(&["n1"]), 1_000)
            .expect("acquire")
        {
            NodeAcquisition::Granted(mut l) => l.remove(0),
            other => panic!("expected a grant: {other:?}"),
        };

        // The node vanishes, as a reindex that dropped the element would cause.
        store
            .conn()
            .execute("DELETE FROM nodes WHERE id = 'n1'", [])
            .expect("delete");

        let err = registry
            .renew_node(&lease, 2_000)
            .expect_err("a vanished target must raise, not silently fail");
        assert!(matches!(err, GraphError::VanishedLeaseTarget { node_id } if node_id == "n1"));
        assert!(
            registry
                .node_holder_of("n1", 2_000)
                .expect("read")
                .is_none(),
            "the invalid lease must not remain grantable to anyone"
        );
    }

    #[test]
    fn ttl_expiry_reclaims_a_node_lease() {
        let (_dir, store) = store();
        insert_node(&store, "n1", None);
        let registry = LeaseRegistry::new(&store).with_ttl_ms(1_000);
        registry
            .acquire_node_set("agent-a", "t1", &ids(&["n1"]), 1_000)
            .expect("acquire");

        let attempt = registry
            .acquire_node_set("agent-b", "t2", &ids(&["n1"]), 5_000)
            .expect("acquire");
        assert!(matches!(attempt, NodeAcquisition::Granted(_)));
    }

    /// Regression: A holds the
    /// parent, its lease lapses, B is granted the child (correctly, since a lapsed
    /// lease is reclaimable), and A then tried to renew its long-dead parent lease.
    /// Before the fix this succeeded, leaving both a "renewed" parent and a live
    /// child lease active at once -- an actual, not merely theoretical, violation of
    /// the ancestor/descendant exclusion this whole module exists to enforce.
    #[test]
    fn a_lapsed_parent_lease_cannot_be_renewed_over_a_live_child() {
        let (_dir, store) = store();
        insert_node(&store, "parent", None);
        insert_node(&store, "child", Some("parent"));
        let registry = LeaseRegistry::new(&store).with_ttl_ms(1_000);
        let parent_lease = match registry
            .acquire_node_set("agent-a", "t1", &ids(&["parent"]), 1_000)
            .expect("acquire")
        {
            NodeAcquisition::Granted(mut l) => l.remove(0),
            other => panic!("expected a grant: {other:?}"),
        };

        // The parent lease lapses (ttl 1_000, acquired at 1_000, so dead at 2_000+).
        let b = registry
            .acquire_node_set("agent-b", "t2", &ids(&["child"]), 2_500)
            .expect("acquire");
        assert!(
            matches!(b, NodeAcquisition::Granted(_)),
            "a lapsed parent lease must not block the child: {b:?}"
        );

        let renewed = registry
            .renew_node(&parent_lease, 2_600)
            .expect("renew_node does not error for an existing node");
        assert!(
            !renewed,
            "a lease that was already lapsed when renew was called must not renew"
        );
        assert!(
            registry
                .node_holder_of("parent", 2_700)
                .expect("read")
                .is_none(),
            "the parent must not be held again after a refused renewal"
        );
        assert_eq!(
            registry
                .node_holder_of("child", 2_700)
                .expect("read")
                .expect("still held")
                .agent_id,
            "agent-b"
        );
    }

    #[test]
    fn no_cycle_of_waits_can_form_over_an_ancestor_descendant_pair() {
        // The classic deadlock shape: two agents want the same two nodes in opposite
        // orders. As with the file-level registry, sorting the set and taking it in
        // one transaction means whichever agent's call runs first gets everything and
        // the other gets nothing, never a partial hold on either side.
        let (_dir, store) = store();
        insert_node(&store, "parent", None);
        insert_node(&store, "child", Some("parent"));
        let registry = LeaseRegistry::new(&store);

        let a = registry
            .acquire_node_set("agent-a", "t1", &ids(&["child", "parent"]), 1_000)
            .expect("acquire");
        let b = registry
            .acquire_node_set("agent-b", "t2", &ids(&["parent", "child"]), 1_000)
            .expect("acquire");
        assert!(matches!(a, NodeAcquisition::Granted(_)));
        assert!(matches!(b, NodeAcquisition::Blocked(_)));
        assert_eq!(registry.held_nodes_by("agent-a").expect("read").len(), 2);
        assert_eq!(registry.held_nodes_by("agent-b").expect("read").len(), 0);
    }

    #[test]
    fn releasing_an_agent_frees_every_node_it_held() {
        let (_dir, store) = store();
        insert_node(&store, "n1", None);
        insert_node(&store, "n2", None);
        let registry = LeaseRegistry::new(&store);
        registry
            .acquire_node_set("agent-a", "t1", &ids(&["n1", "n2"]), 1_000)
            .expect("acquire");
        assert_eq!(registry.release_node_all("agent-a").expect("release"), 2);
        assert!(registry.all_nodes().expect("read").is_empty());
    }

    #[test]
    fn an_agent_re_acquiring_its_own_node_is_a_renewal_not_a_conflict() {
        let (_dir, store) = store();
        insert_node(&store, "parent", None);
        insert_node(&store, "child", Some("parent"));
        let registry = LeaseRegistry::new(&store);
        registry
            .acquire_node_set("agent-a", "t1", &ids(&["parent"]), 1_000)
            .expect("acquire");
        let again = registry
            .acquire_node_set("agent-a", "t1", &ids(&["parent", "child"]), 1_100)
            .expect("acquire");
        assert!(matches!(again, NodeAcquisition::Granted(_)));
        assert_eq!(registry.held_nodes_by("agent-a").expect("read").len(), 2);
    }

    // ---- cross-registry: file lease vs. node lease -----------------------------

    /// Regression: a whole-file
    /// lease and a node lease on a node inside that file were granted to two
    /// different agents at once, because `acquire_all` never looked at `node_leases`
    /// and `acquire_node_set` never looked at `leases`.
    #[test]
    fn a_whole_file_lease_blocks_a_node_lease_inside_it() {
        let (_dir, store) = store();
        insert_node(&store, "parent", None);
        insert_node(&store, "child", Some("parent"));
        let registry = LeaseRegistry::new(&store);

        registry
            .acquire_all("agent-a", "t1", &ids(&["f.rs"]), &[], 1_000)
            .expect("acquire");

        let attempt = registry
            .acquire_node_set("agent-b", "t2", &ids(&["child"]), 1_000)
            .expect("acquire");
        match attempt {
            NodeAcquisition::Blocked(conflicts) => {
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].blocking_path.as_deref(), Some("f.rs"));
                assert_eq!(conflicts[0].held_by, "agent-a");
            }
            other => panic!("a whole-file lease must block a node inside it: {other:?}"),
        }
    }

    #[test]
    fn a_node_lease_blocks_a_whole_file_lease_on_its_file() {
        let (_dir, store) = store();
        insert_node(&store, "parent", None);
        insert_node(&store, "child", Some("parent"));
        let registry = LeaseRegistry::new(&store);

        registry
            .acquire_node_set("agent-a", "t1", &ids(&["child"]), 1_000)
            .expect("acquire");

        let attempt = registry
            .acquire_all("agent-b", "t2", &ids(&["f.rs"]), &[], 1_000)
            .expect("acquire");
        match attempt {
            super::super::Acquisition::Blocked(conflicts) => {
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].blocking_node_id.as_deref(), Some("child"));
                assert_eq!(conflicts[0].held_by, "agent-a");
            }
            other => panic!("a node lease must block a whole-file lease on its file: {other:?}"),
        }
    }

    #[test]
    fn a_node_lease_on_a_different_file_does_not_block_a_file_lease() {
        let (_dir, store) = store();
        insert_node(&store, "n1", None);
        store
            .conn()
            .execute(
                "INSERT INTO files(id, path, content_hash, size_bytes, mtime_ns, language, line_count)
                 VALUES ('g', 'g.rs', 'h', 0, 0, NULL, 1)",
                [],
            )
            .expect("file");
        let registry = LeaseRegistry::new(&store);
        registry
            .acquire_node_set("agent-a", "t1", &ids(&["n1"]), 1_000)
            .expect("acquire");
        let attempt = registry
            .acquire_all("agent-b", "t2", &ids(&["g.rs"]), &[], 1_000)
            .expect("acquire");
        assert!(matches!(attempt, super::super::Acquisition::Granted(_)));
    }

    // ---- reindex invalidation ---------------------------------------------------

    /// Regression, the file-delete half: a
    /// whole file being deleted must take every node lease on it with it, in the same
    /// transaction, not leave them to be discovered stale later.
    #[test]
    fn deleting_a_file_purges_every_node_lease_on_its_nodes() {
        let (_dir, store) = store();
        insert_node(&store, "parent", None);
        insert_node(&store, "child", Some("parent"));
        let registry = LeaseRegistry::new(&store);
        registry
            .acquire_node_set("agent-a", "t1", &ids(&["parent", "child"]), 1_000)
            .expect("acquire");

        store.delete_file("f.rs").expect("delete_file");

        assert!(
            registry.all_nodes().expect("read").is_empty(),
            "every node lease on the deleted file's nodes must be gone"
        );
    }

    /// Regression: a node renamed by a
    /// reindex (so its old id genuinely stops existing) must invalidate the lease on
    /// the old id, and, the harder case, a *different*, unrelated node that later
    /// happens to reuse that same id string must not inherit the old lease. Ported
    /// against the real reindex API (`replace_file_nodes`) rather than raw SQL, so it
    /// exercises the actual fix (the purge in `replace_file_nodes`), not a simulation
    /// of it.
    #[test]
    fn a_reindex_that_renames_a_node_invalidates_its_lease_even_if_the_old_id_reappears() {
        let (_dir, store) = store();
        let file = FileRecord {
            id: "f".to_owned(),
            path: "f.rs".to_owned(),
            content_hash: "h1".to_owned(),
            size_bytes: 0,
            mtime_ns: 0,
            language: None,
            line_count: 1,
        };
        store.upsert_file(&file).expect("file");
        let node = |id: &str, parent: Option<&str>| NodeRecord {
            id: id.to_owned(),
            file_id: file.id.clone(),
            path: file.path.clone(),
            kind: NodeKind::Function,
            name: id.to_owned(),
            qualified_name: id.to_owned(),
            parent_id: parent.map(str::to_owned),
            start_line: 1,
            end_line: 1,
            start_byte: 0,
            end_byte: 0,
            signature: String::new(),
            identity_stable: true,
        };
        store
            .replace_file_nodes(&file.id, &[node("P", None), node("C", Some("P"))], &[], &[])
            .expect("initial nodes");

        let registry = LeaseRegistry::new(&store).with_ttl_ms(10_000);
        let lease = match registry
            .acquire_node_set("agent-a", "t1", &ids(&["C"]), 1_000)
            .expect("acquire")
        {
            NodeAcquisition::Granted(mut l) => l.remove(0),
            other => panic!("expected a grant: {other:?}"),
        };

        // Reindex: "C" is renamed to "C2" (its id changes, as a real edit would cause
        // a signature-derived id to change), and an unrelated new element is inserted
        // that happens to reuse the bare id "C" -- adversarial, but this is exactly
        // the case decision 0050 requires never rebind a lease.
        store
            .replace_file_nodes(
                &file.id,
                &[node("P", None), node("C2", Some("P")), node("C", None)],
                &[],
                &[],
            )
            .expect("reindex");

        // The old id's lease is gone outright, purged by the reindex itself (in
        // `replace_file_nodes`) the instant it noticed "C"'s fingerprint changed --
        // before agent-a ever calls back in, and before the id's reuse could matter.
        assert!(registry.node_holder_of("C", 1_500).expect("read").is_none());
        // The id now names a different, unrelated node, so the registry cannot tell
        // this apart from ordinary fencing (its row is simply gone) -- and does not
        // need to: the purge already did the safety-critical part. `renew_node`
        // raises `VanishedLeaseTarget` only when the id is truly absent; here it
        // answers the weaker but sufficient "no, not renewed".
        let renewed = registry
            .renew_node(&lease, 1_500)
            .expect("the id exists again, so this is fencing, not a vanished-id error");
        assert!(
            !renewed,
            "agent-a's old lease on the reused id must not renew"
        );

        // The reused id "C" is a brand-new, unleased node: anyone may take it.
        let fresh = registry
            .acquire_node_set("agent-b", "t2", &ids(&["C"]), 1_500)
            .expect("acquire");
        assert!(
            matches!(fresh, NodeAcquisition::Granted(_)),
            "the reused id must be a fresh, unleased node, not still bound to agent-a: {fresh:?}"
        );
    }

    /// A node whose id is unchanged across a reindex (the common case: most of a file
    /// did not change) must keep its lease. This is the negative case for the purge
    /// logic: it must not be trigger-happy.
    #[test]
    fn a_reindex_that_leaves_a_node_unchanged_keeps_its_lease() {
        let (_dir, store) = store();
        let file = FileRecord {
            id: "f".to_owned(),
            path: "f.rs".to_owned(),
            content_hash: "h1".to_owned(),
            size_bytes: 0,
            mtime_ns: 0,
            language: None,
            line_count: 1,
        };
        store.upsert_file(&file).expect("file");
        let node = |id: &str| NodeRecord {
            id: id.to_owned(),
            file_id: file.id.clone(),
            path: file.path.clone(),
            kind: NodeKind::Function,
            name: id.to_owned(),
            qualified_name: id.to_owned(),
            parent_id: None,
            start_line: 1,
            end_line: 1,
            start_byte: 0,
            end_byte: 0,
            signature: String::new(),
            identity_stable: true,
        };
        store
            .replace_file_nodes(&file.id, &[node("unchanged"), node("edited")], &[], &[])
            .expect("initial nodes");
        let registry = LeaseRegistry::new(&store).with_ttl_ms(10_000);
        registry
            .acquire_node_set("agent-a", "t1", &ids(&["unchanged"]), 1_000)
            .expect("acquire");

        // Reindex: "edited" is gone (its content changed, so its id changed), a new
        // "edited2" takes its place, and "unchanged" comes back with the identical id
        // because nothing about it changed.
        store
            .replace_file_nodes(&file.id, &[node("unchanged"), node("edited2")], &[], &[])
            .expect("reindex");

        assert_eq!(
            registry
                .node_holder_of("unchanged", 1_500)
                .expect("read")
                .expect("must still be held")
                .agent_id,
            "agent-a",
            "an id that survives a reindex unchanged must keep its lease"
        );
    }
}
