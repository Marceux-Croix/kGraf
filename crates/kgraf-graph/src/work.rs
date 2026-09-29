//! Work nodes: the plan in the graph (decisions 0014, 0015).
//!
//! A plan, its planned components and its work items are rows in the same store as
//! the code, with dependencies between work items as edges, so the orchestrator
//! and the galaxy read one place (the graph is the one work surface). Two properties follow the
//! store's rules.
//!
//! **Facts only, no status.** A work node records what was planned (title, and a
//! JSON body the planner owns: instruction, acceptance criteria, expected evidence,
//! what the documents claim). Whether it is done is derived from code and tests at
//! read time, never stored (decision 0019, and the store's "no `status` column"
//! rule).
//!
//! **Not derived data.** A plan came from a paid model call and the user's
//! approval; it cannot be re-derived from source. Like the lease tables, the work
//! tables are not in the schema-change drop list, and nothing here deletes a node:
//! a new interpretation is a new plan, and the old one stays readable (0015:
//! completed work collapses, it does not vanish). Triggers append every insert and
//! update to the change log, so the interface hears about a seeded plan the same
//! way it hears about code.

use crate::error::{GraphError, Result};
use crate::store::GraphStore;
use rusqlite::{OptionalExtension, params};

/// What a work node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    /// The root of one interpretation: the reading, constraints, validation
    /// commands.
    Plan,
    /// A planned component: a part of the system work items belong to.
    Component,
    /// One unit of work with acceptance criteria.
    Item,
}

impl WorkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkKind::Plan => "plan",
            WorkKind::Component => "component",
            WorkKind::Item => "item",
        }
    }

    pub fn parse(name: &str) -> Option<WorkKind> {
        Some(match name {
            "plan" => WorkKind::Plan,
            "component" => WorkKind::Component,
            "item" => WorkKind::Item,
            _ => return None,
        })
    }
}

/// One work node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkNode {
    /// Unique across plans: `<plan id>/<local id>` by convention.
    pub id: String,
    pub plan_id: String,
    pub kind: WorkKind,
    /// The plan for a component; the component (or plan) for an item.
    pub parent_id: Option<String>,
    pub title: String,
    /// The planner's JSON. The graph stores it; it does not interpret it.
    pub body: String,
    /// Milliseconds since the epoch.
    pub created_ms: i64,
}

/// What one [`GraphStore::put_work`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkWrite {
    pub inserted: u64,
    /// Rows whose title, body or parent changed.
    pub updated: u64,
    /// Rows already present exactly as given: no write, no change-log row.
    pub unchanged: u64,
    pub deps_added: u64,
}

fn node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(WorkNode, String)> {
    let kind: String = row.get(2)?;
    Ok((
        WorkNode {
            id: row.get(0)?,
            plan_id: row.get(1)?,
            // Placeholder until the kind is checked below; see `parse_kind`.
            kind: WorkKind::Plan,
            parent_id: row.get(3)?,
            title: row.get(4)?,
            body: row.get(5)?,
            created_ms: row.get(6)?,
        },
        kind,
    ))
}

fn parse_kind((mut node, kind): (WorkNode, String)) -> Result<WorkNode> {
    node.kind = WorkKind::parse(&kind).ok_or(GraphError::UnknownKind {
        what: "work",
        value: kind,
    })?;
    Ok(node)
}

impl GraphStore {
    /// Write work nodes and dependencies in one transaction. A node already
    /// stored with the same content is left alone (no change-log row); one whose
    /// content differs is updated; a dependency is `(from, to)`: `from` needs `to`
    /// first. Nodes are never deleted.
    ///
    /// Refused as a whole: a node naming a parent or a dependency naming a node
    /// that is neither stored nor in this write, a node changing plan or kind, and
    /// a dependency crossing plans.
    pub fn put_work(&self, nodes: &[WorkNode], deps: &[(String, String)]) -> Result<WorkWrite> {
        let tx = self.conn().unchecked_transaction()?;
        let write = put_work_in(&tx, nodes, deps)?;
        tx.commit()?;
        Ok(write)
    }
}

/// [`GraphStore::put_work`] inside a caller's transaction, so a write that must be
/// atomic with other rows (the presumed-node bookkeeping, `presumed.rs`) is.
pub(crate) fn put_work_in(
    tx: &rusqlite::Transaction<'_>,
    nodes: &[WorkNode],
    deps: &[(String, String)],
) -> Result<WorkWrite> {
    let invalid = |reason: String| GraphError::InvalidWork { reason };
    let mut write = WorkWrite::default();
    // Parents first, whatever order the caller gave.
    let mut ordered: Vec<&WorkNode> = nodes.iter().collect();
    ordered.sort_by_key(|n| match n.kind {
        WorkKind::Plan => 0,
        WorkKind::Component => 1,
        WorkKind::Item => 2,
    });
    for n in ordered {
        if n.id.trim().is_empty() || n.plan_id.trim().is_empty() {
            return Err(invalid("a work node has an empty id or plan id".into()));
        }
        if let Some(p) = &n.parent_id {
            let parent_plan: Option<String> = tx
                .query_row(
                    "SELECT plan_id FROM work_nodes WHERE id = ?1",
                    params![p],
                    |r| r.get(0),
                )
                .optional()?;
            match parent_plan {
                None => {
                    return Err(invalid(format!(
                        "`{}` names parent `{p}`, which is not stored",
                        n.id
                    )));
                }
                Some(pp) if pp != n.plan_id => {
                    return Err(invalid(format!(
                        "`{}` (plan `{}`) names parent `{p}` of plan `{pp}`",
                        n.id, n.plan_id
                    )));
                }
                Some(_) => {}
            }
        }
        let existing: Option<(String, String, Option<String>, String, String)> = tx
            .query_row(
                "SELECT plan_id, kind, parent_id, title, body FROM work_nodes WHERE id = ?1",
                params![n.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        match existing {
            None => {
                tx.execute(
                    "INSERT INTO work_nodes(id, plan_id, kind, parent_id, title, body, created_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        n.id,
                        n.plan_id,
                        n.kind.as_str(),
                        n.parent_id,
                        n.title,
                        n.body,
                        n.created_ms
                    ],
                )?;
                write.inserted += 1;
            }
            Some((plan, kind, parent, title, body)) => {
                if plan != n.plan_id || kind != n.kind.as_str() {
                    return Err(invalid(format!(
                        "`{}` is stored as a {kind} of plan `{plan}`; it cannot become a {} of plan `{}`",
                        n.id,
                        n.kind.as_str(),
                        n.plan_id
                    )));
                }
                if parent == n.parent_id && title == n.title && body == n.body {
                    write.unchanged += 1;
                } else {
                    tx.execute(
                        "UPDATE work_nodes SET parent_id = ?2, title = ?3, body = ?4 WHERE id = ?1",
                        params![n.id, n.parent_id, n.title, n.body],
                    )?;
                    write.updated += 1;
                }
            }
        }
    }
    for (from, to) in deps {
        let plans: Vec<Option<String>> = [from, to]
            .iter()
            .map(|id| {
                tx.query_row(
                    "SELECT plan_id FROM work_nodes WHERE id = ?1",
                    params![id],
                    |r| r.get::<_, String>(0),
                )
                .optional()
            })
            .collect::<rusqlite::Result<_>>()?;
        match (&plans[0], &plans[1]) {
            (Some(a), Some(b)) if a == b => {
                write.deps_added += tx.execute(
                    "INSERT OR IGNORE INTO work_deps(plan_id, src, dst) VALUES (?1, ?2, ?3)",
                    params![a, from, to],
                )? as u64;
            }
            (Some(a), Some(b)) => {
                return Err(invalid(format!(
                    "dependency `{from}` -> `{to}` crosses plans `{a}` and `{b}`"
                )));
            }
            _ => {
                return Err(invalid(format!(
                    "dependency `{from}` -> `{to}` names a node that is not stored"
                )));
            }
        }
    }
    Ok(write)
}

impl GraphStore {
    /// One plan's nodes: the plan first, then components, then items, each by id.
    pub fn work_nodes(&self, plan_id: &str) -> Result<Vec<WorkNode>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, plan_id, kind, parent_id, title, body, created_ms FROM work_nodes
             WHERE plan_id = ?1
             ORDER BY CASE kind WHEN 'plan' THEN 0 WHEN 'component' THEN 1 ELSE 2 END, id",
        )?;
        let rows = stmt.query_map(params![plan_id], node_from_row)?;
        rows.map(|r| parse_kind(r?)).collect()
    }

    /// One plan's dependencies as `(from, to)`: `from` needs `to` first.
    pub fn work_deps(&self, plan_id: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT src, dst FROM work_deps WHERE plan_id = ?1 ORDER BY src, dst")?;
        let rows = stmt.query_map(params![plan_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every stored plan root, newest first.
    pub fn work_plans(&self) -> Result<Vec<WorkNode>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, plan_id, kind, parent_id, title, body, created_ms FROM work_nodes
             WHERE kind = 'plan' ORDER BY created_ms DESC, id DESC",
        )?;
        let rows = stmt.query_map([], node_from_row)?;
        rows.map(|r| parse_kind(r?)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventTail;

    fn node(id: &str, kind: WorkKind, parent: Option<&str>, body: &str) -> WorkNode {
        WorkNode {
            id: format!("p1/{id}"),
            plan_id: "p1".into(),
            kind,
            parent_id: parent.map(|p| format!("p1/{p}")),
            title: id.into(),
            body: body.into(),
            created_ms: 7,
        }
    }

    fn plan() -> Vec<WorkNode> {
        vec![
            node("w2", WorkKind::Item, Some("c1"), "{}"),
            node("c1", WorkKind::Component, Some("root"), "{}"),
            node("root", WorkKind::Plan, None, "{}"),
            node("w1", WorkKind::Item, Some("c1"), "{}"),
        ]
    }

    #[test]
    fn a_plan_round_trips_and_is_heard_on_the_change_log() {
        let dir = tempfile::tempdir().expect("dir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        let mut tail = EventTail::from_now(&store).expect("tail");
        let deps = vec![("p1/w2".to_owned(), "p1/w1".to_owned())];
        let w = store.put_work(&plan(), &deps).expect("put");
        assert_eq!((w.inserted, w.deps_added), (4, 1));
        let nodes = store.work_nodes("p1").expect("nodes");
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["p1/root", "p1/c1", "p1/w1", "p1/w2"]);
        assert_eq!(store.work_deps("p1").expect("deps"), deps);
        assert_eq!(store.work_plans().expect("plans")[0].id, "p1/root");
        let changes = tail.next_batch(&store, 100).expect("batch");
        let work: Vec<_> = changes
            .iter()
            .filter(|c| c.table == "work_nodes" || c.table == "work_deps")
            .collect();
        assert_eq!(work.len(), 5, "four nodes and one dependency: {changes:?}");

        // The same write again changes nothing and logs nothing.
        let again = store.put_work(&plan(), &deps).expect("put");
        assert_eq!(
            (again.unchanged, again.inserted, again.deps_added),
            (4, 0, 0)
        );
        assert!(tail.next_batch(&store, 100).expect("batch").is_empty());

        // A changed body is an update, heard once.
        let mut changed = plan();
        changed[0].body = r#"{"x":1}"#.into();
        let w = store.put_work(&changed, &[]).expect("put");
        assert_eq!((w.updated, w.unchanged), (1, 3));
        assert_eq!(tail.next_batch(&store, 100).expect("batch").len(), 1);
    }

    #[test]
    fn dangling_parents_and_dependencies_and_kind_changes_are_refused_whole() {
        let dir = tempfile::tempdir().expect("dir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        let orphan = vec![node("w1", WorkKind::Item, Some("nope"), "{}")];
        assert!(matches!(
            store.put_work(&orphan, &[]),
            Err(GraphError::InvalidWork { .. })
        ));
        let bad_dep = vec![("p1/w1".to_owned(), "p1/ghost".to_owned())];
        assert!(store.put_work(&plan(), &bad_dep).is_err());
        assert!(
            store.work_nodes("p1").expect("nodes").is_empty(),
            "a refused write leaves nothing behind"
        );
        store.put_work(&plan(), &[]).expect("put");
        let mut recast = plan();
        recast[0].kind = WorkKind::Component;
        assert!(store.put_work(&recast, &[]).is_err());
    }

    #[test]
    fn work_survives_a_schema_rebuild() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("graph.db");
        {
            let store = GraphStore::open(&db, dir.path()).expect("store");
            store.put_work(&plan(), &[]).expect("put");
            store
                .set_meta("schema_version", "0")
                .expect("force a rebuild");
        }
        let store = GraphStore::open(&db, dir.path()).expect("reopen");
        assert_eq!(store.work_nodes("p1").expect("nodes").len(), 4);
    }
}
