//! The plan mirror: a run's contracts as work nodes in the graph (decision 0100,
//! section 2).
//!
//! The run-state record is the truth and the live plan. The mirror is rows
//! derived from it, so every contract is a graph node that can be queried and
//! that can adopt a presumed roadmap node. This module stores a mirror the
//! caller computed; it does not know what a run record is (the orchestrator
//! crate builds the [`PlanMirror`], as a pure function of the record).
//!
//! **One transaction with the record.** [`GraphStore::put_record_with_mirror`]
//! writes the mirror and the record's `meta` row in the same SQLite
//! transaction, so a kill leaves both or neither. The mirror part runs in a
//! savepoint: a mirror that cannot be written is rolled back alone and reported
//! in [`MirrorWrite::error`], and the record still commits. The mirror is never a
//! reason to lose a record.
//!
//! **History as temporal facts.** An item's revisions, its verified commit and
//! its removal are `work:` facts about its node (`work:revised_by`,
//! `work:verified_commit`, `work:removed_by`). Only the `work:` predicates write
//! change-log rows (`facts_work_ai`, `facts_work_au`), and a fact already stored
//! is skipped, so writing a mirror twice logs nothing the second time.
//!
//! **Limit: nothing is deleted.** `work_nodes` and `work_deps` have no delete
//! path (`work.rs`: a plan is history, 0015). A dependency a revision dropped
//! stays as an edge of the old state. An item's body holds its current
//! `depends_on`; a reader that needs the current edges reads the body, not the
//! edge table.

use crate::error::Result;
use crate::facts::{NewFact, assert_in};
use crate::store::GraphStore;
use crate::work::{WorkNode, WorkWrite, put_work_in};
use rusqlite::{OptionalExtension, params};

/// One temporal fact of the mirror.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorFact {
    pub subject: String,
    /// Always a `work:` predicate.
    pub predicate: String,
    pub object: String,
    /// Milliseconds since the epoch: when the fact became true.
    pub valid_at: i64,
}

/// One run's mirror: the plan root, its items, their dependencies and facts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanMirror {
    pub plan_id: String,
    pub nodes: Vec<WorkNode>,
    /// `(from, to)`: `from` needs `to` first.
    pub deps: Vec<(String, String)>,
    pub facts: Vec<MirrorFact>,
    /// Stored as the facts' transaction time.
    pub recorded_ms: i64,
    /// The mirror is the whole plan, not the changed part of it. Only a whole
    /// mirror can say what the graph holds beyond it: a write of one retracts
    /// the rows and facts of the plan that the record does not give.
    pub full: bool,
}

/// The object of the `work:removed_by` fact that retracts an item the record
/// does not give (one left by an older shape of the mirror, or written by hand).
pub const RETRACTED: &str = "mirror-rebuild";

/// What one mirror write did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MirrorWrite {
    pub work: WorkWrite,
    pub facts_added: u64,
    /// Facts already stored exactly as given.
    pub facts_present: u64,
    /// Facts closed and items marked removed because the record does not give them.
    pub retracted: u64,
    /// Microseconds the mirror part of the write took.
    pub micros: u64,
    /// Why the mirror was not written, when it was not. The record was.
    pub error: Option<String>,
}

impl MirrorWrite {
    /// Rows the write added or changed.
    pub fn changed(&self) -> u64 {
        self.work.inserted
            + self.work.updated
            + self.work.deps_added
            + self.facts_added
            + self.retracted
    }
}

/// An open `work:` fact about the plan (its root or one of its items) that the
/// mirror does not give: its row id.
fn wrong_open_facts(conn: &rusqlite::Connection, mirror: &PlanMirror) -> Result<Vec<(i64, i64)>> {
    let given: std::collections::HashSet<(&str, &str, &str, i64)> = mirror
        .facts
        .iter()
        .map(|f| {
            (
                f.subject.as_str(),
                f.predicate.as_str(),
                f.object.as_str(),
                f.valid_at,
            )
        })
        .collect();
    let nodes: std::collections::HashSet<&str> =
        mirror.nodes.iter().map(|n| n.id.as_str()).collect();
    let prefix = format!("{}/item/", mirror.plan_id);
    let mut stmt = conn.prepare(
        "SELECT id, subject, predicate, object, valid_at FROM facts
         WHERE predicate LIKE 'work:%' AND invalid_at IS NULL AND expired_at IS NULL
           AND (subject = ?1 OR substr(subject, 1, length(?2)) = ?2)",
    )?;
    let rows = stmt.query_map(params![mirror.plan_id, prefix], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let mut wrong = Vec::new();
    for row in rows {
        let (id, subject, predicate, object, valid_at) = row?;
        let kept_retraction = predicate == "work:removed_by"
            && object == RETRACTED
            && !nodes.contains(subject.as_str());
        if !kept_retraction && !given.contains(&(&*subject, &*predicate, &*object, valid_at)) {
            wrong.push((id, valid_at));
        }
    }
    Ok(wrong)
}

/// A fact the mirror gives that is stored with another end of validity than the
/// record's history gives it: closed by a wrong later fact, or left open under a
/// later one. Row id and the end it should have (`None`: open).
fn wrong_ends(conn: &rusqlite::Connection, mirror: &PlanMirror) -> Result<Vec<(i64, Option<i64>)>> {
    let mut by_key: std::collections::BTreeMap<(&str, &str), Vec<&MirrorFact>> =
        std::collections::BTreeMap::new();
    for f in &mirror.facts {
        by_key
            .entry((&f.subject, &f.predicate))
            .or_default()
            .push(f);
    }
    let mut wrong = Vec::new();
    for facts in by_key.values_mut() {
        facts.sort_by_key(|f| f.valid_at);
        for (i, f) in facts.iter().enumerate() {
            let want = facts.get(i + 1).map(|next| next.valid_at);
            let stored: Option<(i64, Option<i64>)> = conn
                .query_row(
                    "SELECT id, invalid_at FROM facts
                     WHERE subject = ?1 AND predicate = ?2 AND object = ?3 AND valid_at = ?4",
                    params![f.subject, f.predicate, f.object, f.valid_at],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((id, end)) = stored
                && end != want
            {
                wrong.push((id, want));
            }
        }
    }
    Ok(wrong)
}

/// Items stored under the plan that the mirror does not give and that are not
/// retracted yet.
fn extra_items(conn: &rusqlite::Connection, mirror: &PlanMirror) -> Result<Vec<String>> {
    let nodes: std::collections::HashSet<&str> =
        mirror.nodes.iter().map(|n| n.id.as_str()).collect();
    let mut stmt = conn.prepare(
        "SELECT n.id FROM work_nodes n
         WHERE n.plan_id = ?1 AND n.kind = 'item'
           AND NOT EXISTS (SELECT 1 FROM facts f WHERE f.subject = n.id
                           AND f.predicate = 'work:removed_by' AND f.object = ?2
                           AND f.invalid_at IS NULL AND f.expired_at IS NULL)",
    )?;
    let ids = stmt
        .query_map(params![mirror.plan_id, RETRACTED], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids
        .into_iter()
        .filter(|id| !nodes.contains(id.as_str()))
        .collect())
}

/// Items marked removed by a retraction.
fn retracted_items(conn: &rusqlite::Connection, plan_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT subject FROM facts WHERE predicate = 'work:removed_by' AND object = ?1
           AND invalid_at IS NULL AND expired_at IS NULL
           AND substr(subject, 1, length(?2)) = ?2",
    )?;
    let ids = stmt
        .query_map(params![RETRACTED, format!("{plan_id}/item/")], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

fn put_mirror_in(conn: &rusqlite::Connection, mirror: &PlanMirror) -> Result<MirrorWrite> {
    let started = std::time::Instant::now();
    let mut retracted = 0u64;
    // A whole mirror makes the graph converge on it: a fact it does not give is
    // closed first (so a right fact asserted below is not itself closed by a
    // wrong one that began later).
    if mirror.full {
        for (id, valid_at) in wrong_open_facts(conn, mirror)? {
            conn.execute(
                "UPDATE facts SET invalid_at = ?2, expired_at = ?3 WHERE id = ?1",
                params![id, valid_at.max(mirror.recorded_ms), mirror.recorded_ms],
            )?;
            retracted += 1;
        }
    }
    if mirror.full {
        for (id, end) in wrong_ends(conn, mirror)? {
            conn.execute(
                "UPDATE facts SET invalid_at = ?2, expired_at = ?3 WHERE id = ?1",
                params![id, end, end.map(|_| mirror.recorded_ms)],
            )?;
            retracted += 1;
        }
    }
    let work = put_work_in(conn, &mirror.nodes, &mirror.deps)?;
    let mut write = MirrorWrite {
        work,
        ..MirrorWrite::default()
    };
    // Oldest first, so a later revision closes an earlier one the way it did
    // when it happened.
    let mut facts: Vec<&MirrorFact> = mirror.facts.iter().collect();
    facts.sort_by_key(|f| (f.valid_at, f.subject.as_str(), f.predicate.as_str()));
    for f in facts {
        if GraphStore::has_fact_at(conn, &f.subject, &f.predicate, &f.object, f.valid_at)? {
            write.facts_present += 1;
            continue;
        }
        assert_in(
            conn,
            &NewFact {
                subject: &f.subject,
                predicate: &f.predicate,
                object: &f.object,
                valid_at: f.valid_at,
                invalid_at: None,
                episode: &mirror.plan_id,
            },
            mirror.recorded_ms,
        )?;
        write.facts_added += 1;
    }
    // Items the record does not give are marked removed, never deleted (a plan
    // is history): a `work:removed_by` fact, and they leave the comparison.
    if mirror.full {
        for id in extra_items(conn, mirror)? {
            assert_in(
                conn,
                &NewFact {
                    subject: &id,
                    predicate: "work:removed_by",
                    object: RETRACTED,
                    valid_at: mirror.recorded_ms,
                    invalid_at: None,
                    episode: &mirror.plan_id,
                },
                mirror.recorded_ms,
            )?;
            retracted += 1;
        }
    }
    write.retracted = retracted;
    write.micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    Ok(write)
}

impl GraphStore {
    /// Write a mirror alone, in one transaction (the repair path and a resume).
    /// The record is not touched; a mirror that cannot be written is an error.
    pub fn put_plan_mirror(&self, mirror: &PlanMirror) -> Result<MirrorWrite> {
        let tx = rusqlite::Transaction::new_unchecked(
            self.conn(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let write = put_mirror_in(&tx, mirror)?;
        tx.commit()?;
        Ok(write)
    }

    /// Whether the store holds exactly this mirror for its plan: the same nodes
    /// (kind, parent, title, body; an item retracted by an earlier rebuild is not
    /// one), every dependency, every fact, and no open `work:` fact the mirror
    /// does not give. The plan may hold more dependency edges than the mirror (an
    /// edge is never deleted).
    pub fn plan_mirror_matches(&self, mirror: &PlanMirror) -> Result<bool> {
        let retracted: std::collections::HashSet<String> =
            retracted_items(self.conn(), &mirror.plan_id)?
                .into_iter()
                .collect();
        let mut stored: Vec<WorkNode> = self
            .work_nodes(&mirror.plan_id)?
            .into_iter()
            .filter(|n| !retracted.contains(&n.id))
            .collect();
        let mut want = mirror.nodes.clone();
        stored.sort_by(|a, b| a.id.cmp(&b.id));
        want.sort_by(|a, b| a.id.cmp(&b.id));
        let same_nodes = stored.len() == want.len()
            && stored.iter().zip(&want).all(|(a, b)| {
                (&a.id, a.kind, &a.parent_id, &a.title, &a.body)
                    == (&b.id, b.kind, &b.parent_id, &b.title, &b.body)
            });
        if !same_nodes {
            return Ok(false);
        }
        let deps = self.work_deps(&mirror.plan_id)?;
        if !mirror.deps.iter().all(|d| deps.contains(d)) {
            return Ok(false);
        }
        for f in &mirror.facts {
            if !Self::has_fact_at(self.conn(), &f.subject, &f.predicate, &f.object, f.valid_at)? {
                return Ok(false);
            }
        }
        Ok(wrong_open_facts(self.conn(), mirror)?.is_empty()
            && wrong_ends(self.conn(), mirror)?.is_empty())
    }

    /// Write a record's `meta` row and its mirror in one transaction. The mirror
    /// is written first, inside a savepoint, then the record: a failure of the
    /// record write rolls the mirror back with it, and a failure of the mirror
    /// alone is returned in [`MirrorWrite::error`] with the record committed.
    /// `mirror: None` writes the record only.
    pub fn put_record_with_mirror(
        &self,
        key: &str,
        record_json: &str,
        mirror: Option<&PlanMirror>,
    ) -> Result<MirrorWrite> {
        // The same secure-delete rule as every `meta` write (decision 0071).
        self.conn().pragma_update(None, "secure_delete", "ON")?;
        let written = (|| -> Result<MirrorWrite> {
            let mut tx = rusqlite::Transaction::new_unchecked(
                self.conn(),
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let mut write = MirrorWrite::default();
            if let Some(mirror) = mirror {
                let sp = tx.savepoint()?;
                match put_mirror_in(&sp, mirror) {
                    Ok(w) => {
                        sp.commit()?;
                        write = w;
                    }
                    Err(e) => {
                        // Dropping the savepoint rolls it back.
                        drop(sp);
                        write.error = Some(e.to_string());
                    }
                }
            }
            tx.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, record_json],
            )?;
            tx.commit()?;
            Ok(write)
        })();
        let restored = self.conn().pragma_update(None, "secure_delete", "FAST");
        let write = written?;
        restored?;
        Ok(write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventTail;
    use crate::work::WorkKind;

    fn node(id: &str, kind: WorkKind, parent: Option<&str>, body: &str) -> WorkNode {
        WorkNode {
            id: format!("run:r1{id}"),
            plan_id: "run:r1".into(),
            kind,
            parent_id: parent.map(|p| format!("run:r1{p}")),
            title: id.into(),
            body: body.into(),
            created_ms: 5,
        }
    }

    fn mirror() -> PlanMirror {
        PlanMirror {
            plan_id: "run:r1".into(),
            nodes: vec![
                node("", WorkKind::Plan, None, "{}"),
                node("/item/a", WorkKind::Item, Some(""), "{}"),
                node("/item/b", WorkKind::Item, Some(""), "{}"),
            ],
            deps: vec![("run:r1/item/b".into(), "run:r1/item/a".into())],
            facts: vec![
                MirrorFact {
                    subject: "run:r1/item/a".into(),
                    predicate: "work:revised_by".into(),
                    object: "change:1".into(),
                    valid_at: 100,
                },
                MirrorFact {
                    subject: "run:r1/item/a".into(),
                    predicate: "work:revised_by".into(),
                    object: "change:2".into(),
                    valid_at: 200,
                },
            ],
            recorded_ms: 300,
            full: true,
        }
    }

    fn store() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().expect("dir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        (dir, store)
    }

    fn logged(tail: &mut EventTail, store: &GraphStore, table: &str) -> usize {
        tail.next_batch(store, 10_000)
            .expect("batch")
            .iter()
            .filter(|c| c.table == table)
            .count()
    }

    #[test]
    fn a_mirror_round_trips_and_a_second_write_changes_and_logs_nothing() {
        let (_d, store) = store();
        let mut tail = EventTail::from_now(&store).expect("tail");
        let w = store
            .put_record_with_mirror("orchestrator/run/r1", "{}", Some(&mirror()))
            .expect("write");
        assert_eq!(w.error, None);
        assert_eq!(
            (w.work.inserted, w.work.deps_added, w.facts_added),
            (3, 1, 2)
        );
        assert_eq!(store.work_nodes("run:r1").expect("nodes").len(), 3);
        // The older revision, written beside the newer one, is closed at the
        // newer one's start and kept.
        let h = store
            .fact_history("run:r1/item/a", "work:revised_by")
            .expect("history");
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].invalid_at, Some(200));
        assert_eq!(h[1].invalid_at, None);
        assert_eq!(
            logged(&mut tail, &store, "facts"),
            3,
            "two inserts and one close"
        );

        let again = store
            .put_record_with_mirror("orchestrator/run/r1", "{}", Some(&mirror()))
            .expect("write");
        assert_eq!(again.changed(), 0, "{again:?}");
        assert_eq!(again.facts_present, 2);
        assert_eq!(
            store
                .fact_history("run:r1/item/a", "work:revised_by")
                .expect("history")
                .len(),
            2,
            "no row added"
        );
        let rest = tail.next_batch(&store, 10_000).expect("batch");
        assert!(
            rest.is_empty(),
            "a re-mirror adds no change-log row: {rest:?}"
        );
    }

    /// The trigger covers `work:` predicates only.
    #[test]
    fn the_facts_trigger_covers_work_predicates_only() {
        let (_d, store) = store();
        let mut tail = EventTail::from_now(&store).expect("tail");
        let other = NewFact {
            subject: "template:t1",
            predicate: "standing",
            object: "verified",
            valid_at: 1,
            invalid_at: None,
            episode: "e",
        };
        store.assert_fact(&other, 1).expect("assert");
        store
            .assert_fact(
                &NewFact {
                    object: "failed",
                    valid_at: 2,
                    ..other.clone()
                },
                2,
            )
            .expect("close");
        assert_eq!(logged(&mut tail, &store, "facts"), 0);
        let work = NewFact {
            predicate: "work:revised_by",
            ..other.clone()
        };
        store.assert_fact(&work, 1).expect("assert");
        // A duplicate adds a source, not a row, and logs nothing.
        store.assert_fact(&work, 3).expect("duplicate");
        assert_eq!(logged(&mut tail, &store, "facts"), 1);
        // A close logs once.
        store
            .assert_fact(
                &NewFact {
                    object: "later",
                    valid_at: 5,
                    ..work.clone()
                },
                5,
            )
            .expect("close");
        assert_eq!(
            logged(&mut tail, &store, "facts"),
            2,
            "the new fact and the close"
        );
    }

    #[test]
    fn a_mirror_that_cannot_be_written_rolls_back_alone_and_the_record_commits() {
        let (_d, store) = store();
        let mut bad = mirror();
        bad.deps
            .push(("run:r1/item/a".into(), "run:r1/item/ghost".into()));
        let w = store
            .put_record_with_mirror("orchestrator/run/r1", "{\"a\":1}", Some(&bad))
            .expect("the record commits");
        assert!(
            w.error.as_deref().is_some_and(|e| e.contains("ghost")),
            "{w:?}"
        );
        assert_eq!(
            store.meta("orchestrator/run/r1").expect("meta").as_deref(),
            Some("{\"a\":1}")
        );
        assert!(
            store.work_nodes("run:r1").expect("nodes").is_empty(),
            "no half of the mirror stays"
        );
        assert!(
            store
                .fact_history("run:r1/item/a", "work:revised_by")
                .expect("history")
                .is_empty()
        );
    }

    /// The mirror shares the record's transaction: a failure of the record write
    /// takes the mirror with it.
    #[test]
    fn a_failed_record_write_takes_the_mirror_with_it() {
        let (_d, store) = store();
        store
            .conn()
            .execute_batch(
                "CREATE TRIGGER no_record BEFORE INSERT ON meta
                 WHEN NEW.key = 'orchestrator/run/r1'
                 BEGIN SELECT RAISE(ABORT, 'record refused'); END;",
            )
            .expect("trigger");
        let r = store.put_record_with_mirror("orchestrator/run/r1", "{}", Some(&mirror()));
        assert!(r.is_err());
        assert!(
            store.work_nodes("run:r1").expect("nodes").is_empty(),
            "the mirror did not commit without the record"
        );
        assert!(
            store
                .fact_history("run:r1/item/a", "work:revised_by")
                .expect("history")
                .is_empty()
        );
        // The store still writes after the failure (secure_delete restored).
        store.set_meta("k", "v").expect("meta");
    }

    /// The record wins for rows it does not give too: an extra item and a wrong open
    /// fact are retracted, the mirror then matches, and a second write changes
    /// nothing and logs nothing.
    #[test]
    fn a_whole_mirror_retracts_what_the_record_does_not_give_and_converges() {
        let (_d, store) = store();
        let mut m = mirror();
        m.facts.push(MirrorFact {
            subject: "run:r1/item/b".into(),
            predicate: "work:verified_commit".into(),
            object: "right".into(),
            valid_at: 150,
        });
        store.put_plan_mirror(&m).expect("write");
        assert!(store.plan_mirror_matches(&m).expect("matches"));
        // An older shape left an item the record does not give, and someone
        // asserted a wrong commit that began later than the right one.
        store
            .put_work(&[node("/item/ghost", WorkKind::Item, Some(""), "{}")], &[])
            .expect("extra item");
        store
            .assert_fact(
                &NewFact {
                    subject: "run:r1/item/b",
                    predicate: "work:verified_commit",
                    object: "wrong",
                    valid_at: 400,
                    invalid_at: None,
                    episode: "x",
                },
                400,
            )
            .expect("wrong fact");
        assert!(
            !store.plan_mirror_matches(&m).expect("matches"),
            "both differ"
        );

        let mut tail = EventTail::from_now(&store).expect("tail");
        let w = store.put_plan_mirror(&m).expect("repair");
        assert!(w.retracted >= 2, "{w:?}");
        assert!(store.plan_mirror_matches(&m).expect("matches"));
        let h = store
            .fact_history("run:r1/item/b", "work:verified_commit")
            .expect("history");
        let open: Vec<&str> = h
            .iter()
            .filter(|f| f.invalid_at.is_none())
            .map(|f| f.object.as_str())
            .collect();
        assert_eq!(open, ["right"], "{h:?}");
        let ghost = store
            .fact_history("run:r1/item/ghost", "work:removed_by")
            .expect("history");
        assert_eq!(ghost.len(), 1);
        assert_eq!(ghost[0].object, RETRACTED);
        assert!(
            tail.next_batch(&store, 10_000)
                .expect("batch")
                .iter()
                .any(|c| c.table == "facts"),
            "the retraction is heard"
        );

        let again = store.put_plan_mirror(&m).expect("again");
        assert_eq!(again.changed(), 0, "converged: {again:?}");
        assert!(tail.next_batch(&store, 10_000).expect("batch").is_empty());
        // A wrong fact alone, and a wrong end alone, each make the graph differ.
        store
            .assert_fact(
                &NewFact {
                    subject: "run:r1/item/a",
                    predicate: "work:verified_commit",
                    object: "stray",
                    valid_at: 900,
                    invalid_at: None,
                    episode: "x",
                },
                900,
            )
            .expect("stray");
        assert!(
            !store.plan_mirror_matches(&m).expect("matches"),
            "a wrong fact"
        );
        store.put_plan_mirror(&m).expect("repair");
        assert!(store.plan_mirror_matches(&m).expect("matches"));
        let right = store
            .fact_history("run:r1/item/b", "work:verified_commit")
            .expect("history")
            .into_iter()
            .find(|f| f.object == "right")
            .expect("the right fact");
        store
            .conn()
            .execute(
                "UPDATE facts SET invalid_at = 999, expired_at = 999 WHERE id = ?1",
                [right.id],
            )
            .expect("close");
        assert!(
            !store.plan_mirror_matches(&m).expect("matches"),
            "a wrong end"
        );
        store.put_plan_mirror(&m).expect("repair");
        assert!(store.plan_mirror_matches(&m).expect("matches"));
        // A partial write never retracts what it does not carry.
        let mut part = m.clone();
        part.full = false;
        part.nodes.truncate(1);
        part.deps.clear();
        part.facts.clear();
        assert_eq!(store.put_plan_mirror(&part).expect("part").retracted, 0);
        assert!(store.plan_mirror_matches(&m).expect("matches"));
        // An item the record gives again is back in the comparison.
        let mut back = m.clone();
        back.nodes
            .push(node("/item/ghost", WorkKind::Item, Some(""), "{}"));
        store.put_plan_mirror(&back).expect("back");
        assert!(store.plan_mirror_matches(&back).expect("matches"));
    }

    #[test]
    fn mirror_and_record_survive_a_schema_rebuild_and_no_table_has_a_status_column() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("graph.db");
        {
            let store = GraphStore::open(&db, dir.path()).expect("store");
            store
                .put_record_with_mirror("orchestrator/run/r1", "{\"r\":1}", Some(&mirror()))
                .expect("write");
            store
                .set_meta("schema_version", "0")
                .expect("force a rebuild");
        }
        let store = GraphStore::open(&db, dir.path()).expect("reopen");
        assert_eq!(
            store.meta("orchestrator/run/r1").expect("meta").as_deref(),
            Some("{\"r\":1}")
        );
        assert_eq!(store.work_nodes("run:r1").expect("nodes").len(), 3);
        assert_eq!(
            store
                .fact_history("run:r1/item/a", "work:revised_by")
                .expect("history")
                .len(),
            2
        );
        // No work or fact table carries a status column: doneness is derived.
        for table in [
            "work_nodes",
            "work_deps",
            "work_presumed",
            "work_schedules",
            "facts",
        ] {
            let mut stmt = store
                .conn()
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .expect("info");
            let cols: Vec<String> = stmt
                .query_map([], |r| r.get(0))
                .expect("cols")
                .collect::<rusqlite::Result<_>>()
                .expect("rows");
            assert!(!cols.is_empty(), "{table} exists");
            assert!(
                cols.iter().all(|c| !c.to_lowercase().contains("status")),
                "{table}: {cols:?}"
            );
        }
    }
}
