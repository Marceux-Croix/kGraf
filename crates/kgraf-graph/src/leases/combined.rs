//! The single admission path for both grains of lease.
//!
//! An earlier version had a gap: [`super::LeaseRegistry::acquire_all`] and
//! [`super::node::LeaseRegistry::acquire_node_set`] (as they were: two independent
//! functions, each with its own transaction) never checked each other's table. Agent A
//! could hold a whole-file lease on `f.rs` while agent B was, at the same time, granted
//! a node lease on a node inside `f.rs`, the two registries had no way to notice they
//! were protecting the same code.
//!
//! There is exactly one way to close that hole correctly: **one transaction that reads
//! both tables before writing either.** Two transactions checking each other's table
//! cannot be made safe by adding more checks to each side individually, a check in
//! transaction A that reads table B, followed by a write to table A, still races a
//! concurrent transaction B doing the mirror image, unless something serializes them.
//! `BEGIN IMMEDIATE` (the connection-wide default; see
//! [`crate::store::GraphStore`]) is that something: it takes the write lock at the
//! start of the transaction, so the second of any two concurrent admission attempts
//! blocks until the first has committed or rolled back, and sees the first's result
//! when it re-reads.
//!
//! [`acquire`] is that one transaction. [`super::LeaseRegistry::acquire_all`] and
//! [`super::LeaseRegistry::acquire_node_set`] are thin wrappers over it, so a caller
//! that only wants one grain still gets the cross-check for free, and a caller that
//! wants both in one call (a tool that writes a function and, incidentally, the file's
//! trailing newline) can ask for both in the one call this module was built for.
//!
//! ## Deadlock freedom (decision 0048), restated for the combined key space
//!
//! 0048's proof does not depend on what a "key" is, only on two structural facts:
//! every key in a request is sorted into one canonical order, and the whole request
//! is checked-then-written inside one transaction with no partial grant possible. Both
//! hold here exactly as they did for paths alone: paths and node ids are each sorted
//! independently (they are different key spaces, so there is no shared order to
//! violate between them), and `acquire` returns `Blocked` -- writing nothing at all --
//! the instant any key in the combined request conflicts. A caller never holds a
//! partial mix of paths and nodes while waiting on the rest.

use super::node::{NodeConflict, NodeLease, related_node_ids};
use super::{Acquisition, Conflict, LeaseRegistry, WriteLease};
use crate::error::{GraphError, Result};
use rusqlite::{OptionalExtension, params};
use std::collections::BTreeSet;

/// The outcome of a combined admission attempt. All-or-nothing across *both* key
/// spaces at once: a request for two paths and one node either gets all three or
/// none of them.
#[derive(Debug)]
pub enum MixedAcquisition {
    Granted {
        paths: Vec<WriteLease>,
        nodes: Vec<NodeLease>,
    },
    Blocked {
        paths: Vec<Conflict>,
        nodes: Vec<NodeConflict>,
    },
    /// One or more requested node ids are `identity_stable = false` in the graph.
    /// Checked before anything else: an unstable id has no key to lease regardless of
    /// what else was requested, so nothing in the request is worth admitting yet.
    Unstable(Vec<String>),
    /// One or more requested node ids do not exist in the graph.
    Vanished(Vec<String>),
}

impl LeaseRegistry<'_> {
    /// Acquire a mixed set of whole-file and sub-file leases in one transaction.
    ///
    /// See the module doc for why this, and not two separately-locking functions, is
    /// the only correct way to admit both grains at once. [`super::LeaseRegistry::acquire_all`]
    /// and [`super::LeaseRegistry::acquire_node_set`] are wrappers over this for a
    /// caller that only wants one grain; call this directly for a request that mixes
    /// both (a tool writing a function and, incidentally, the file's trailing
    /// newline, for example).
    pub fn acquire(
        &self,
        agent_id: &str,
        task_id: &str,
        paths: &[String],
        declared_ranges: &[Option<(u32, u32)>],
        node_ids: &[String],
        now_ms: i64,
    ) -> Result<MixedAcquisition> {
        if declared_ranges.len() != paths.len() && !declared_ranges.is_empty() {
            return Err(GraphError::History {
                reason: format!(
                    "declared_ranges has {} entries for {} paths",
                    declared_ranges.len(),
                    paths.len()
                ),
            });
        }

        let mut wanted_paths: Vec<(String, Option<(u32, u32)>)> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), declared_ranges.get(i).copied().flatten()))
            .collect();
        wanted_paths.sort_by(|a, b| a.0.cmp(&b.0));
        wanted_paths.dedup_by(|a, b| a.0 == b.0);

        let mut wanted_nodes: Vec<String> = node_ids.to_vec();
        wanted_nodes.sort();
        wanted_nodes.dedup();

        let conn = self.store.conn();
        let tx = conn.unchecked_transaction()?;

        // Existence and identity-stability, read from the graph rather than taken on
        // faith from the caller (decision 0049; the original
        // version trusted a caller-declared flag here). Vanished takes priority over
        // unstable: a node's stability cannot be assessed once it does not exist.
        let mut vanished = Vec::new();
        let mut unstable = Vec::new();
        for id in &wanted_nodes {
            let stable: Option<bool> = tx
                .query_row(
                    "SELECT identity_stable FROM nodes WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()?;
            match stable {
                None => vanished.push(id.clone()),
                Some(false) => unstable.push(id.clone()),
                Some(true) => {}
            }
        }
        if !vanished.is_empty() {
            return Ok(MixedAcquisition::Vanished(vanished));
        }
        if !unstable.is_empty() {
            return Ok(MixedAcquisition::Unstable(unstable));
        }

        // Every node's file path, needed both for the node-vs-file check below and to
        // label a conflict a whole-file lease causes.
        let mut node_paths: std::collections::HashMap<&str, String> =
            std::collections::HashMap::new();
        for id in &wanted_nodes {
            let path: String = tx.query_row(
                "SELECT f.path FROM nodes n JOIN files f ON f.id = n.file_id WHERE n.id = ?1",
                params![id],
                |row| row.get(0),
            )?;
            node_paths.insert(id.as_str(), path);
        }

        let mut path_conflicts = Vec::new();
        let mut node_conflicts = Vec::new();
        let mut purge_paths: BTreeSet<String> = BTreeSet::new();
        let mut purge_nodes: BTreeSet<String> = BTreeSet::new();

        // ---- path-vs-path, and path-vs-node ------------------------------------
        {
            let mut held =
                tx.prepare("SELECT agent_id, task_id, expires_ms FROM leases WHERE path = ?1")?;
            let mut held_by_node = tx.prepare(
                "SELECT nl.node_id, nl.agent_id, nl.task_id, nl.expires_ms
                 FROM node_leases nl
                 JOIN nodes n ON n.id = nl.node_id
                 JOIN files f ON f.id = n.file_id
                 WHERE f.path = ?1",
            )?;
            for (path, _) in &wanted_paths {
                let existing: Option<(String, String, i64)> = held
                    .query_row(params![path], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })
                    .optional()?;
                if let Some((holder, holder_task, expires_ms)) = existing {
                    if holder == agent_id {
                        // Our own lease is a renewal, not a conflict.
                    } else if now_ms >= expires_ms {
                        // A lapsed lease is reclaimable here and nowhere else.
                        purge_paths.insert(path.clone());
                    } else {
                        path_conflicts.push(Conflict {
                            path: path.clone(),
                            held_by: holder,
                            held_task: holder_task,
                            expires_ms,
                            blocking_node_id: None,
                        });
                    }
                }

                let node_rows = held_by_node
                    .query_map(params![path], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (node_id, holder, holder_task, expires_ms) in node_rows {
                    if holder == agent_id {
                        continue;
                    }
                    if now_ms >= expires_ms {
                        purge_nodes.insert(node_id);
                        continue;
                    }
                    path_conflicts.push(Conflict {
                        path: path.clone(),
                        held_by: holder,
                        held_task: holder_task,
                        expires_ms,
                        blocking_node_id: Some(node_id),
                    });
                }
            }
        }

        // ---- node-vs-node (ancestor/descendant), and node-vs-path --------------
        {
            let mut held_by_path =
                tx.prepare("SELECT agent_id, task_id, expires_ms FROM leases WHERE path = ?1")?;
            for id in &wanted_nodes {
                let mut related = related_node_ids(&tx, id)?;
                related.insert(id.clone());
                let candidates: Vec<&str> = related.iter().map(String::as_str).collect();
                let placeholders = std::iter::repeat_n("?", candidates.len())
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "SELECT node_id, agent_id, task_id, expires_ms FROM node_leases
                     WHERE node_id IN ({placeholders}) AND agent_id <> ?"
                );
                let mut stmt = tx.prepare(&sql)?;
                let mut sql_params: Vec<&dyn rusqlite::ToSql> = candidates
                    .iter()
                    .map(|c| c as &dyn rusqlite::ToSql)
                    .collect();
                sql_params.push(&agent_id);
                let rows = stmt
                    .query_map(sql_params.as_slice(), |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (conflicting_node_id, holder, holder_task, expires_ms) in rows {
                    if now_ms >= expires_ms {
                        purge_nodes.insert(conflicting_node_id);
                        continue;
                    }
                    node_conflicts.push(NodeConflict {
                        node_id: id.clone(),
                        conflicting_node_id: Some(conflicting_node_id),
                        blocking_path: None,
                        held_by: holder,
                        held_task: holder_task,
                        expires_ms,
                    });
                }

                let path = &node_paths[id.as_str()];
                let existing: Option<(String, String, i64)> = held_by_path
                    .query_row(params![path], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })
                    .optional()?;
                if let Some((holder, holder_task, expires_ms)) = existing {
                    if holder == agent_id {
                        // Own file lease covers this node; not a conflict.
                    } else if now_ms >= expires_ms {
                        purge_paths.insert(path.clone());
                    } else {
                        node_conflicts.push(NodeConflict {
                            node_id: id.clone(),
                            conflicting_node_id: None,
                            blocking_path: Some(path.clone()),
                            held_by: holder,
                            held_task: holder_task,
                            expires_ms,
                        });
                    }
                }
            }
        }

        if !path_conflicts.is_empty() || !node_conflicts.is_empty() {
            // Nothing was written -- not even a purge of the stale rows found along
            // the way -- so there is nothing to roll back; the transaction is dropped
            // rather than committed. A future attempt will simply find the same
            // stale rows again and purge them then, at zero correctness cost.
            return Ok(MixedAcquisition::Blocked {
                paths: path_conflicts,
                nodes: node_conflicts,
            });
        }

        // Every conflict this request could have hit was either live (and already
        // returned above) or lapsed. Lapsed rows discovered along the way are purged
        // now, in the same transaction as the grant, which is decision 0048's window:
        // a lapsed lease is reclaimed here and nowhere else, so it cannot be renewed
        // out from under this grant between the check and the write.
        if !purge_paths.is_empty() {
            let placeholders = std::iter::repeat_n("?", purge_paths.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("DELETE FROM leases WHERE path IN ({placeholders})");
            let bound: Vec<&dyn rusqlite::ToSql> = purge_paths
                .iter()
                .map(|p| p as &dyn rusqlite::ToSql)
                .collect();
            tx.execute(&sql, bound.as_slice())?;
        }
        if !purge_nodes.is_empty() {
            let placeholders = std::iter::repeat_n("?", purge_nodes.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("DELETE FROM node_leases WHERE node_id IN ({placeholders})");
            let bound: Vec<&dyn rusqlite::ToSql> = purge_nodes
                .iter()
                .map(|n| n as &dyn rusqlite::ToSql)
                .collect();
            tx.execute(&sql, bound.as_slice())?;
        }

        let mut granted_paths = Vec::with_capacity(wanted_paths.len());
        {
            let mut generation_of = tx.prepare(
                "SELECT COALESCE(MAX(generation), 0) + 1 FROM lease_history WHERE path = ?1",
            )?;
            let mut upsert = tx.prepare(
                "INSERT INTO leases(path, agent_id, task_id, range_start, range_end,
                                    acquired_ms, expires_ms, generation)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(path) DO UPDATE SET
                     agent_id = excluded.agent_id,
                     task_id = excluded.task_id,
                     range_start = excluded.range_start,
                     range_end = excluded.range_end,
                     acquired_ms = excluded.acquired_ms,
                     expires_ms = excluded.expires_ms,
                     generation = excluded.generation",
            )?;
            let mut record = tx.prepare(
                "INSERT INTO lease_history(path, agent_id, task_id, generation, acquired_ms,
                                           range_start, range_end)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (path, range) in &wanted_paths {
                let generation: i64 = generation_of.query_row(params![path], |row| row.get(0))?;
                upsert.execute(params![
                    path,
                    agent_id,
                    task_id,
                    range.map(|r| r.0),
                    range.map(|r| r.1),
                    now_ms,
                    now_ms + self.ttl_ms,
                    generation,
                ])?;
                record.execute(params![
                    path,
                    agent_id,
                    task_id,
                    generation,
                    now_ms,
                    range.map(|r| r.0),
                    range.map(|r| r.1),
                ])?;
                granted_paths.push(WriteLease::new(
                    path.clone(),
                    agent_id.to_owned(),
                    generation,
                ));
            }
        }

        let mut granted_nodes = Vec::with_capacity(wanted_nodes.len());
        {
            let mut generation_of = tx.prepare(
                "SELECT COALESCE(MAX(generation), 0) + 1 FROM node_lease_history
                 WHERE node_id = ?1",
            )?;
            let mut upsert = tx.prepare(
                "INSERT INTO node_leases(node_id, agent_id, task_id, acquired_ms,
                                         expires_ms, generation)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(node_id) DO UPDATE SET
                     agent_id = excluded.agent_id,
                     task_id = excluded.task_id,
                     acquired_ms = excluded.acquired_ms,
                     expires_ms = excluded.expires_ms,
                     generation = excluded.generation",
            )?;
            let mut record = tx.prepare(
                "INSERT INTO node_lease_history(node_id, agent_id, task_id, generation,
                                                acquired_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for id in &wanted_nodes {
                let generation: i64 = generation_of.query_row(params![id], |row| row.get(0))?;
                upsert.execute(params![
                    id,
                    agent_id,
                    task_id,
                    now_ms,
                    now_ms + self.ttl_ms,
                    generation,
                ])?;
                record.execute(params![id, agent_id, task_id, generation, now_ms])?;
                granted_nodes.push(NodeLease::new(id.clone(), agent_id.to_owned(), generation));
            }
        }

        tx.commit()?;
        Ok(MixedAcquisition::Granted {
            paths: granted_paths,
            nodes: granted_nodes,
        })
    }

    /// [`super::LeaseRegistry::acquire_all`]'s actual body: acquire paths only.
    pub(super) fn acquire_all_impl(
        &self,
        agent_id: &str,
        task_id: &str,
        paths: &[String],
        declared_ranges: &[Option<(u32, u32)>],
        now_ms: i64,
    ) -> Result<Acquisition> {
        match self.acquire(agent_id, task_id, paths, declared_ranges, &[], now_ms)? {
            MixedAcquisition::Granted { paths, nodes } => {
                debug_assert!(nodes.is_empty(), "no node ids were requested");
                Ok(Acquisition::Granted(paths))
            }
            MixedAcquisition::Blocked { paths, nodes } => {
                debug_assert!(nodes.is_empty(), "no node ids were requested");
                Ok(Acquisition::Blocked(paths))
            }
            MixedAcquisition::Unstable(_) | MixedAcquisition::Vanished(_) => {
                unreachable!("no node ids were requested, so neither case can trigger")
            }
        }
    }
}
