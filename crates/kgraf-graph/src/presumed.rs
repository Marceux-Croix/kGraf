//! Presumed work nodes (decision 0064): work a roadmap or plan document says
//! exists, written into the graph before any of it is approved or built, so the
//! graph shows what is done, in flight and still to do from the first index.
//!
//! A presumed node is an ordinary [`WorkNode`] plus one `work_presumed` row that
//! records its provenance and the bookkeeping a re-read needs. The rules:
//!
//! - **Identity is the source's, not the read's.** The caller derives each id
//!   from the document and its structure (a table row key, a heading path), so
//!   reading the same document again names the same nodes. [`GraphStore::sync_presumed`]
//!   with the same content writes nothing and appends nothing to the change log.
//! - **A re-read updates, never duplicates, and never deletes.** A node whose
//!   source text changed is updated in place. A node its source no longer lists is
//!   marked gone (`gone_ms`), not deleted (decision 0015: work collapses, it does
//!   not vanish); listed again, it returns under the same id.
//! - **Someone else's change wins.** `written_hash` is the hash of the parent,
//!   title and body this module last wrote. A node whose current content no longer
//!   hashes to it was changed by something else (the user, another tool) and is
//!   left exactly as it is.
//! - **Real work adopts, it does not replace.** [`GraphStore::adopt_presumed`]
//!   records which real work item took the node over. The node keeps its id; a
//!   re-read leaves an adopted node's content alone and never marks it gone.
//! - **Facts only.** Nothing here stores a status. Whether a presumed item is done,
//!   partly done or still to do is derived at read time from what its document
//!   claims and what the code holds (`kgraf_interpret::presumed`).

use crate::error::{GraphError, Result};
use crate::store::GraphStore;
use crate::work::{WorkNode, put_work_in};
use rusqlite::{OptionalExtension, params};

/// Where a presumed node was read, and by what.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresumedOrigin {
    /// `roadmap` (a deterministic parse of a document) or `reading` (a model's
    /// reading of the corpus, through the ledger).
    pub source_kind: String,
    /// The document's repository-relative path, or `reading` for a model reading.
    pub source: String,
    /// Where in the source: a heading path, a table row, a task line.
    pub section: String,
    /// What produced it: the parser and its version, or the model, its reading
    /// and its ledger call.
    pub parser: String,
    /// What the document says of it (`complete`, `partial`, `not_started`,
    /// `unstated`), as of the last read. The document's claim, not a status:
    /// it is kept outside the node's hashed content, so it stays current after
    /// someone edits or real work adopts the node.
    pub claim: String,
    /// The claim as written (a status cell, a task box), cut short.
    pub claim_text: String,
}

/// A node as stored, read once per node by a sync.
struct Stored {
    kind: String,
    source: String,
    parent: Option<String>,
    title: String,
    body: String,
    written: String,
    gone: Option<i64>,
    adopted: Option<String>,
    claim: String,
    claim_text: String,
}

/// One node to write, with its origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresumedNode {
    pub node: WorkNode,
    pub origin: PresumedOrigin,
}

/// A stored presumed node and its bookkeeping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPresumed {
    pub node: WorkNode,
    pub origin: PresumedOrigin,
    /// When this node was first written.
    pub first_ms: i64,
    /// When a re-read of its source stopped listing it; `None` while listed.
    pub gone_ms: Option<i64>,
    /// The real work that adopted it, and when.
    pub adopted: Option<(String, i64)>,
    /// Whether its content is still what the last read wrote. False means
    /// someone else changed it, and re-reads leave it alone.
    pub as_written: bool,
}

/// What one [`GraphStore::sync_presumed`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PresumedSync {
    pub inserted: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: u64,
    /// Listed again after being gone.
    pub returned: Vec<String>,
    /// Changed by someone else since the last read: left as they are.
    pub kept_edited: Vec<String>,
    /// Adopted by real work: left as they are.
    pub kept_adopted: Vec<String>,
    /// No longer listed by their source: marked gone, not deleted.
    pub gone: Vec<String>,
    /// Whose document claim changed, whether or not their content was written
    /// (an edited or adopted node's claim still follows its document).
    pub claims_updated: Vec<String>,
}

impl PresumedSync {
    /// Whether the sync wrote anything.
    pub fn wrote(&self) -> bool {
        !(self.inserted.is_empty()
            && self.updated.is_empty()
            && self.returned.is_empty()
            && self.gone.is_empty()
            && self.claims_updated.is_empty())
    }
}

/// The hash a node's written content is kept under.
fn content_hash(parent: Option<&str>, title: &str, body: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(parent.unwrap_or("").as_bytes());
    h.update(&[0]);
    h.update(title.as_bytes());
    h.update(&[0]);
    h.update(body.as_bytes());
    h.finalize().to_hex().to_string()
}

fn has_table(conn: &rusqlite::Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'work_presumed'",
        [],
        |r| r.get::<_, i64>(0).map(|n| n > 0),
    )?)
}

/// Whether `work_presumed` has the claim columns (added after the table first
/// shipped on a branch; `GraphStore::migrate` adds them).
/// Both columns, not one: a database with only `doc_claim`
/// must read as not migrated.
pub(crate) fn has_claim_columns(conn: &rusqlite::Connection) -> Result<bool> {
    let (claim, text) = claim_columns(conn)?;
    Ok(claim && text)
}

/// Which of `doc_claim` and `doc_claim_text` exist.
pub(crate) fn claim_columns(conn: &rusqlite::Connection) -> Result<(bool, bool)> {
    let has = |name: &str| -> Result<bool> {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('work_presumed') WHERE name = ?1",
            params![name],
            |r| r.get::<_, i64>(0).map(|n| n > 0),
        )?)
    };
    Ok((has("doc_claim")?, has("doc_claim_text")?))
}

impl GraphStore {
    /// Bring the presumed nodes of one source (`source_kind`, `source`) into step
    /// with `nodes`, in one transaction (see the module documentation). Every node
    /// must name that source in its origin and be new or already presumed from it:
    /// a node id held by real work, or presumed from another source, is refused as
    /// a whole, like `put_work` refuses a dangling parent. `deps` are
    /// `(from, to)` among these nodes; existing ones are kept.
    ///
    /// An edited or adopted node keeps its stored parent. A new or updated child
    /// of one still names it, so nothing dangles.
    pub fn sync_presumed(
        &self,
        source_kind: &str,
        source: &str,
        nodes: &[PresumedNode],
        deps: &[(String, String)],
        now_ms: i64,
    ) -> Result<PresumedSync> {
        let invalid = |reason: String| GraphError::InvalidWork { reason };
        let tx = self.conn().unchecked_transaction()?;
        let mut sync = PresumedSync::default();
        let mut write: Vec<WorkNode> = Vec::new();
        // (index into `nodes`, whether it is new, the hash to record), for the
        // rows whose content is written; by index, so no per-row scan.
        let mut rows: Vec<(usize, bool, String)> = Vec::new();
        // Rows whose document claim changed, and rows listed again whose
        // content stays as someone else left it.
        let mut claims: Vec<usize> = Vec::new();
        let mut ungone: Vec<usize> = Vec::new();
        let mut listed = std::collections::BTreeSet::new();
        for (i, p) in nodes.iter().enumerate() {
            let n = &p.node;
            if p.origin.source_kind != source_kind || p.origin.source != source {
                return Err(invalid(format!(
                    "`{}` is presumed from {} `{}`, not from the {source_kind} `{source}` being read",
                    n.id, p.origin.source_kind, p.origin.source
                )));
            }
            if !listed.insert(n.id.clone()) {
                return Err(invalid(format!("`{}` is listed twice in one read", n.id)));
            }
            let stored: Option<Stored> = tx
                .query_row(
                    "SELECT p.source_kind, p.source, n.parent_id, n.title, n.body,
                            p.written_hash, p.gone_ms, p.adopted_by, p.doc_claim,
                            p.doc_claim_text
                     FROM work_nodes n LEFT JOIN work_presumed p ON p.node_id = n.id
                     WHERE n.id = ?1",
                    params![n.id],
                    |r| {
                        Ok(Stored {
                            kind: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                            source: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                            parent: r.get(2)?,
                            title: r.get(3)?,
                            body: r.get(4)?,
                            written: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                            gone: r.get(6)?,
                            adopted: r.get(7)?,
                            claim: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
                            claim_text: r.get::<_, Option<String>>(9)?.unwrap_or_default(),
                        })
                    },
                )
                .optional()?;
            let new_hash = content_hash(n.parent_id.as_deref(), &n.title, &n.body);
            let Some(s) = stored else {
                write.push(n.clone());
                rows.push((i, true, new_hash));
                sync.inserted.push(n.id.clone());
                continue;
            };
            if s.kind.is_empty() {
                return Err(invalid(format!(
                    "`{}` is a work node no document presumed; a read cannot take it over",
                    n.id
                )));
            }
            if s.kind != source_kind || s.source != source {
                return Err(invalid(format!(
                    "`{}` is presumed from {} `{}`, not from {source_kind} `{source}`",
                    n.id, s.kind, s.source
                )));
            }
            // What the document says is the document's own fact: kept current
            // whoever changed or adopted the node.
            let claim_changed = s.claim != p.origin.claim || s.claim_text != p.origin.claim_text;
            if claim_changed {
                claims.push(i);
                sync.claims_updated.push(n.id.clone());
            }
            if s.adopted.is_some() {
                sync.kept_adopted.push(n.id.clone());
                continue;
            }
            if content_hash(s.parent.as_deref(), &s.title, &s.body) != s.written {
                sync.kept_edited.push(n.id.clone());
                if s.gone.is_some() {
                    // Listed again: no longer gone. Its content and recorded hash
                    // stay as they are, so it stays the editor's.
                    ungone.push(i);
                    sync.returned.push(n.id.clone());
                }
                continue;
            }
            let changed = new_hash != s.written;
            if changed {
                write.push(n.clone());
                sync.updated.push(n.id.clone());
            } else if s.gone.is_none() && !claim_changed {
                sync.unchanged += 1;
            }
            if s.gone.is_some() {
                sync.returned.push(n.id.clone());
            }
            if changed || s.gone.is_some() {
                rows.push((i, false, new_hash));
            }
        }
        put_work_in(&tx, &write, deps)?;
        for (i, new, hash) in &rows {
            let (id, o) = (&nodes[*i].node.id, &nodes[*i].origin);
            if *new {
                tx.execute(
                    "INSERT INTO work_presumed(node_id, source_kind, source, section, parser,
                                               written_hash, first_ms, doc_claim, doc_claim_text)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        id,
                        o.source_kind,
                        o.source,
                        o.section,
                        o.parser,
                        hash,
                        now_ms,
                        o.claim,
                        o.claim_text
                    ],
                )?;
            } else {
                tx.execute(
                    "UPDATE work_presumed SET section = ?2, parser = ?3, written_hash = ?4,
                                              gone_ms = NULL
                     WHERE node_id = ?1",
                    params![id, o.section, o.parser, hash],
                )?;
            }
        }
        for i in &claims {
            let (id, o) = (&nodes[*i].node.id, &nodes[*i].origin);
            tx.execute(
                "UPDATE work_presumed SET doc_claim = ?2, doc_claim_text = ?3 WHERE node_id = ?1",
                params![id, o.claim, o.claim_text],
            )?;
        }
        for i in &ungone {
            tx.execute(
                "UPDATE work_presumed SET gone_ms = NULL WHERE node_id = ?1",
                params![nodes[*i].node.id],
            )?;
        }
        // What this source no longer lists: marked gone, never deleted. Adopted
        // nodes are real work now and never go.
        let mut stmt = tx.prepare(
            "SELECT node_id FROM work_presumed
             WHERE source_kind = ?1 AND source = ?2 AND gone_ms IS NULL AND adopted_by IS NULL",
        )?;
        let absent: Vec<String> = stmt
            .query_map(params![source_kind, source], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|id| !listed.contains(id))
            .collect();
        drop(stmt);
        for id in &absent {
            tx.execute(
                "UPDATE work_presumed SET gone_ms = ?2 WHERE node_id = ?1",
                params![id, now_ms],
            )?;
        }
        sync.gone = absent;
        tx.commit()?;
        Ok(sync)
    }

    /// Record that real work (`by`: an approved plan's item node, a run's
    /// contract) adopted presumed node `node_id`. The node keeps its id and its
    /// content, and later reads leave it alone. Adopting an adopted node again is
    /// a no-op that returns `false`; a node that is not presumed is refused.
    pub fn adopt_presumed(&self, node_id: &str, by: &str, now_ms: i64) -> Result<bool> {
        let tx = self.conn().unchecked_transaction()?;
        let adopted: Option<Option<String>> = tx
            .query_row(
                "SELECT adopted_by FROM work_presumed WHERE node_id = ?1",
                params![node_id],
                |r| r.get(0),
            )
            .optional()?;
        let changed = match adopted {
            None => {
                return Err(GraphError::InvalidWork {
                    reason: format!("`{node_id}` is not a presumed work node"),
                });
            }
            Some(Some(_)) => false,
            Some(None) => {
                tx.execute(
                    "UPDATE work_presumed SET adopted_by = ?2, adopted_ms = ?3, gone_ms = NULL
                     WHERE node_id = ?1",
                    params![node_id, by, now_ms],
                )?;
                true
            }
        };
        tx.commit()?;
        Ok(changed)
    }

    /// Every presumed node, by id. Empty for an index written before presumed
    /// nodes existed (a read-only viewer never creates the table).
    pub fn presumed_nodes(&self) -> Result<Vec<StoredPresumed>> {
        if !has_table(self.conn())? {
            return Ok(Vec::new());
        }
        // A read-only viewer never migrates: an index written before the claim
        // columns reads them as empty (the claim then comes from the body).
        let claim_cols = if has_claim_columns(self.conn())? {
            "p.doc_claim, p.doc_claim_text"
        } else {
            "'', ''"
        };
        let mut stmt = self.conn().prepare(&format!(
            "SELECT n.id, n.plan_id, n.kind, n.parent_id, n.title, n.body, n.created_ms,
                    p.source_kind, p.source, p.section, p.parser, p.written_hash,
                    p.first_ms, p.gone_ms, p.adopted_by, p.adopted_ms, {claim_cols}
             FROM work_presumed p JOIN work_nodes n ON n.id = p.node_id
             ORDER BY n.id"
        ))?;
        let rows = stmt.query_map([], |r| {
            let kind: String = r.get(2)?;
            let node = WorkNode {
                id: r.get(0)?,
                plan_id: r.get(1)?,
                kind: crate::work::WorkKind::Plan,
                parent_id: r.get(3)?,
                title: r.get(4)?,
                body: r.get(5)?,
                created_ms: r.get(6)?,
            };
            let adopted_by: Option<String> = r.get(14)?;
            let adopted_ms: Option<i64> = r.get(15)?;
            Ok((
                kind,
                StoredPresumed {
                    as_written: content_hash(node.parent_id.as_deref(), &node.title, &node.body)
                        == r.get::<_, String>(11)?,
                    node,
                    origin: PresumedOrigin {
                        source_kind: r.get(7)?,
                        source: r.get(8)?,
                        section: r.get(9)?,
                        parser: r.get(10)?,
                        claim: r.get(16)?,
                        claim_text: r.get(17)?,
                    },
                    first_ms: r.get(12)?,
                    gone_ms: r.get(13)?,
                    adopted: adopted_by.zip(adopted_ms),
                },
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (kind, mut p) = row?;
            p.node.kind = crate::work::WorkKind::parse(&kind).ok_or(GraphError::UnknownKind {
                what: "work",
                value: kind,
            })?;
            out.push(p);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventTail;
    use crate::work::WorkKind;

    const SRC: &str = "ROADMAP.md";

    fn node(id: &str, kind: WorkKind, parent: Option<&str>, title: &str) -> PresumedNode {
        PresumedNode {
            node: WorkNode {
                id: format!("presumed/{id}"),
                plan_id: "presumed/plan".into(),
                kind,
                parent_id: parent.map(|p| format!("presumed/{p}")),
                title: title.into(),
                body: format!(r#"{{"t":"{title}"}}"#),
                created_ms: 1,
            },
            origin: PresumedOrigin {
                source_kind: "roadmap".into(),
                source: SRC.into(),
                section: title.into(),
                parser: "test/1".into(),
                claim: "not_started".into(),
                claim_text: "[ ]".into(),
            },
        }
    }

    fn read(titles: &[(&str, &str)]) -> Vec<PresumedNode> {
        let mut v = vec![node("plan", WorkKind::Plan, None, "plan")];
        for (id, title) in titles {
            v.push(node(id, WorkKind::Item, Some("plan"), title));
        }
        v
    }

    fn sync(store: &GraphStore, nodes: &[PresumedNode], now: i64) -> PresumedSync {
        store
            .sync_presumed("roadmap", SRC, nodes, &[], now)
            .expect("sync")
    }

    #[test]
    fn a_re_read_is_idempotent_updates_in_place_and_marks_the_absent_gone() {
        let dir = tempfile::tempdir().expect("dir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        let first = sync(&store, &read(&[("a", "A"), ("b", "B")]), 10);
        assert_eq!(first.inserted.len(), 3);

        // The same read again: nothing written, nothing on the change log.
        let mut tail = EventTail::from_now(&store).expect("tail");
        let again = sync(&store, &read(&[("a", "A"), ("b", "B")]), 20);
        assert!(!again.wrote(), "{again:?}");
        assert_eq!(again.unchanged, 3);
        assert!(tail.next_batch(&store, 100).expect("batch").is_empty());

        // A changed title updates the same node; a dropped item goes, not deleted.
        let third = sync(&store, &read(&[("a", "A, renamed")]), 30);
        assert_eq!(third.updated, ["presumed/a"]);
        assert_eq!(third.gone, ["presumed/b"]);
        let all = store.presumed_nodes().expect("nodes");
        assert_eq!(all.len(), 3, "nothing is deleted");
        let b = all.iter().find(|p| p.node.id == "presumed/b").expect("b");
        assert_eq!(b.gone_ms, Some(30));
        let a = all.iter().find(|p| p.node.id == "presumed/a").expect("a");
        assert_eq!((a.node.title.as_str(), a.first_ms), ("A, renamed", 10));

        // Listed again, it returns under the same id.
        let fourth = sync(&store, &read(&[("a", "A, renamed"), ("b", "B")]), 40);
        assert_eq!(fourth.returned, ["presumed/b"]);
        let b = store
            .presumed_nodes()
            .expect("nodes")
            .into_iter()
            .find(|p| p.node.id == "presumed/b")
            .expect("b");
        assert_eq!(b.gone_ms, None);
    }

    #[test]
    fn an_edit_by_someone_else_is_never_overwritten() {
        let dir = tempfile::tempdir().expect("dir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        sync(&store, &read(&[("a", "A")]), 10);
        // The user renames the node through the ordinary work-node write.
        let mut edited = read(&[("a", "A")])[1].node.clone();
        edited.title = "A, as the user calls it".into();
        store.put_work(&[edited], &[]).expect("edit");

        // The document now also says it is done.
        let mut now = read(&[("a", "A, as the roadmap now says")]);
        now[1].origin.claim = "complete".into();
        now[1].origin.claim_text = "[x]".into();
        let s = sync(&store, &now, 20);
        assert_eq!(s.kept_edited, ["presumed/a"]);
        assert_eq!(s.claims_updated, ["presumed/a"]);
        let a = store
            .presumed_nodes()
            .expect("nodes")
            .into_iter()
            .find(|p| p.node.id == "presumed/a")
            .expect("a");
        assert_eq!(a.node.title, "A, as the user calls it");
        assert!(!a.as_written);
        // The edit no longer freezes what the document says.
        assert_eq!(
            (a.origin.claim.as_str(), a.origin.claim_text.as_str()),
            ("complete", "[x]")
        );
        // And an unchanged read is still idempotent.
        assert!(!sync(&store, &now, 30).wrote());
    }

    #[test]
    fn adopted_nodes_keep_their_identity_and_content_and_never_go() {
        let dir = tempfile::tempdir().expect("dir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        sync(&store, &read(&[("a", "A"), ("b", "B")]), 10);
        assert!(
            store
                .adopt_presumed("presumed/a", "plan-x/item/a", 15)
                .expect("adopt")
        );
        assert!(
            !store
                .adopt_presumed("presumed/a", "plan-y/item/a", 16)
                .expect("again")
        );
        assert!(store.adopt_presumed("presumed/nope", "x", 17).is_err());

        let s = sync(&store, &read(&[("a", "A changed"), ("b", "B")]), 20);
        assert_eq!(s.kept_adopted, ["presumed/a"]);
        let s = sync(&store, &read(&[("b", "B")]), 30);
        assert!(s.gone.is_empty(), "an adopted node is real work: {s:?}");
        let a = store
            .presumed_nodes()
            .expect("nodes")
            .into_iter()
            .find(|p| p.node.id == "presumed/a")
            .expect("a");
        assert_eq!(a.node.title, "A");
        assert_eq!(a.adopted, Some(("plan-x/item/a".to_owned(), 15)));
        assert_eq!(a.gone_ms, None);
    }

    #[test]
    fn a_read_cannot_take_over_real_work_or_another_sources_node() {
        let dir = tempfile::tempdir().expect("dir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        let real = WorkNode {
            id: "presumed/a".into(),
            plan_id: "presumed/plan".into(),
            kind: WorkKind::Plan,
            parent_id: None,
            title: "real".into(),
            body: "{}".into(),
            created_ms: 1,
        };
        store.put_work(&[real], &[]).expect("real");
        let n = vec![node("a", WorkKind::Plan, None, "A")];
        assert!(store.sync_presumed("roadmap", SRC, &n, &[], 1).is_err());
        let other = store.sync_presumed("roadmap", "OTHER.md", &n, &[], 1);
        assert!(other.is_err(), "the origin must name the source being read");
    }

    #[test]
    fn presumed_nodes_survive_a_schema_rebuild() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("graph.db");
        {
            let store = GraphStore::open(&db, dir.path()).expect("store");
            sync(&store, &read(&[("a", "A")]), 10);
            store.set_meta("schema_version", "0").expect("force");
        }
        let store = GraphStore::open(&db, dir.path()).expect("reopen");
        assert_eq!(store.presumed_nodes().expect("nodes").len(), 2);
    }

    /// A database with one claim column (a migration cut off
    /// between its two ALTERs, as the unguarded version could leave it), or with
    /// neither (a table from before the columns), is migrated on open, and a
    /// read and a sync then work. Before, the half-migrated case read as
    /// migrated and every read failed on `doc_claim_text`.
    #[test]
    fn a_half_migrated_or_old_table_gains_the_missing_claim_columns() {
        for dropped in [
            &["doc_claim_text"][..],
            &["doc_claim", "doc_claim_text"][..],
        ] {
            let dir = tempfile::tempdir().expect("dir");
            let db = dir.path().join("graph.db");
            {
                let store = GraphStore::open(&db, dir.path()).expect("store");
                sync(&store, &read(&[("a", "A")]), 10);
            }
            {
                let raw = rusqlite::Connection::open(&db).expect("raw");
                for col in dropped {
                    raw.execute_batch(&format!("ALTER TABLE work_presumed DROP COLUMN {col};"))
                        .expect("drop");
                }
                assert!(!has_claim_columns(&raw).expect("check"), "{dropped:?}");
            }
            let store = GraphStore::open(&db, dir.path()).expect("reopen migrates");
            assert!(
                has_claim_columns(store.conn()).expect("check"),
                "{dropped:?}"
            );
            let mut now = read(&[("a", "A")]);
            now[1].origin.claim = "complete".into();
            let s = sync(&store, &now, 20);
            // A recreated column is empty, so the first read after the
            // migration writes every node's claim again.
            assert_eq!(
                s.claims_updated,
                ["presumed/plan", "presumed/a"],
                "{dropped:?}"
            );
            let a = store
                .presumed_nodes()
                .expect("nodes")
                .into_iter()
                .find(|p| p.node.id == "presumed/a")
                .expect("a");
            assert_eq!(a.origin.claim, "complete");
            // Opening again finds nothing to do.
            drop(store);
            GraphStore::open(&db, dir.path()).expect("reopen again");
        }
    }
}
