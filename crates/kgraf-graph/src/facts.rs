//! Temporal validity on graph facts (decision 0056; long-running support).
//!
//! Ported from Graphiti (`getzep/graphiti`, commit
//! ba4a9cb32495b6864160616f8dfa2b898f4a500c, Apache-2.0, license read at the source
//! 2026-09-26; see `third_party/graphiti/UPSTREAM`):
//!
//! - `graphiti_core/edges.py` `EntityEdge`: the four timestamps. `valid_at` and
//!   `invalid_at` are valid time (when the fact became true and stopped being true);
//!   `created_at` and `expired_at` are transaction time (when the row was recorded
//!   and when it was superseded). The model is bi-temporal.
//! - `graphiti_core/utils/maintenance/edge_operations.py`
//!   `resolve_extracted_edge` (the block after "Determine if the new_edge needs to
//!   be expired") and `resolve_edge_contradictions`: the rules below.
//!
//! **What differs from Graphiti.** Graphiti asks a model which existing edges a new
//! edge contradicts. Here a contradiction is structural and needs no model call:
//! a `(subject, predicate)` pair is single-valued, so an open fact with the same
//! subject and predicate and a different object is a candidate. A multi-valued
//! relation uses distinct predicates or subjects. Graphiti's duplicate detection
//! (also a model call) is exact equality here: the same subject, predicate and
//! object on an open fact adds the source to its episodes instead of a new row.
//!
//! **Rules, as ported.** With `new` the asserted fact and `c` each candidate:
//!
//! 1. If `new` arrives with `invalid_at` set, it is expired on arrival.
//! 2. If any candidate became valid after `new` did, `new` is closed at the
//!    earliest such candidate's `valid_at` (newer information already exists).
//! 3. A candidate is skipped when the two intervals do not overlap
//!    (`new.invalid_at <= c.valid_at`).
//! 4. Otherwise, a candidate that became valid at or before `new` is closed:
//!    `c.invalid_at = new.valid_at`, `c.expired_at = now`.
//!
//! **Two changes to those rules**, both so that at most one fact per
//! `(subject, predicate)` is open. Only open facts are candidates (Graphiti also
//! considers closed ones, whose own end already answers rule 3). An equal
//! `valid_at` is a tie that Graphiti leaves with both facts open; here it is broken
//! by transaction time, so the later assertion stands and closes the earlier.
//!
//! Nothing is ever deleted (decision 0028): a fact that stops being true is closed.

use crate::error::{GraphError, Result};
use crate::store::GraphStore;
use rusqlite::{OptionalExtension, params};

/// One stored fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    pub id: i64,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    /// Milliseconds since the epoch.
    pub valid_at: i64,
    pub invalid_at: Option<i64>,
    pub created_at: i64,
    pub expired_at: Option<i64>,
    /// The latest sources that asserted this fact, oldest first, at most
    /// [`MAX_EPISODES`].
    pub episodes: Vec<String>,
    /// How many assertions this fact has had in all.
    pub episode_count: u64,
}

/// A fact to assert.
#[derive(Clone, Debug)]
pub struct NewFact<'a> {
    pub subject: &'a str,
    pub predicate: &'a str,
    pub object: &'a str,
    pub valid_at: i64,
    /// Known end of validity, if the source states one.
    pub invalid_at: Option<i64>,
    /// The source asserting it (a run id, a commit, a transcript entry).
    pub episode: &'a str,
}

/// What an assertion did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assertion {
    /// The asserted fact as stored (or the open duplicate it merged into).
    pub fact: Fact,
    /// Facts this assertion closed.
    pub invalidated: Vec<Fact>,
    /// The assertion matched an open fact exactly and only added its episode.
    pub duplicate: bool,
}

/// Sources kept per fact. The count keeps growing; only the last this many
/// source names are kept, so a fact asserted by every run stays bounded.
pub const MAX_EPISODES: usize = 32;

const COLUMNS: &str = "id, subject, predicate, object, valid_at, invalid_at, created_at, \
                       expired_at, episodes, episode_count";

fn row_fact(row: &rusqlite::Row<'_>) -> rusqlite::Result<Fact> {
    let episodes: String = row.get(8)?;
    // Written only by this module, always as a JSON string array; anything else
    // is a corrupt row and raises rather than reading as "no sources" (fail loud).
    let episodes: Vec<String> = serde_json::from_str(&episodes).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Fact {
        id: row.get(0)?,
        subject: row.get(1)?,
        predicate: row.get(2)?,
        object: row.get(3)?,
        valid_at: row.get(4)?,
        invalid_at: row.get(5)?,
        created_at: row.get(6)?,
        expired_at: row.get(7)?,
        episodes,
        episode_count: row.get::<_, i64>(9)?.max(0) as u64,
    })
}

fn episodes_json(episodes: &[String]) -> String {
    serde_json::Value::from(episodes.to_vec()).to_string()
}

/// Every write takes the write lock at BEGIN: a read that informs the write
/// must not be overtaken by another writer (the lease registry's reasoning).
fn immediate(conn: &rusqlite::Connection) -> rusqlite::Result<rusqlite::Transaction<'_>> {
    rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
}

impl GraphStore {
    /// Assert a fact, closing the facts it contradicts (module doc, rules 1 to 4).
    pub fn assert_fact(&self, new: &NewFact<'_>, now_ms: i64) -> Result<Assertion> {
        if let Some(end) = new.invalid_at
            && end < new.valid_at
        {
            return Err(GraphError::Fact {
                id: 0,
                reason: format!("invalid_at {end} is before valid_at {}", new.valid_at),
            });
        }
        let tx = immediate(self.conn())?;
        // Only open facts are candidates: a closed one already has its end.
        let open: Vec<Fact> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {COLUMNS} FROM facts
                 WHERE subject = ?1 AND predicate = ?2
                   AND invalid_at IS NULL AND expired_at IS NULL
                 ORDER BY id"
            ))?;
            stmt.query_map(params![new.subject, new.predicate], row_fact)?
                .collect::<rusqlite::Result<_>>()?
        };
        // Exact duplicate of an open fact: record the source, add no row.
        if new.invalid_at.is_none()
            && let Some(dup) = open.iter().find(|f| f.object == new.object)
        {
            let mut fact = dup.clone();
            fact.episode_count += 1;
            fact.episodes.retain(|e| e != new.episode);
            fact.episodes.push(new.episode.to_owned());
            if fact.episodes.len() > MAX_EPISODES {
                let excess = fact.episodes.len() - MAX_EPISODES;
                fact.episodes.drain(..excess);
            }
            tx.execute(
                "UPDATE facts SET episodes = ?2, episode_count = ?3 WHERE id = ?1",
                params![
                    fact.id,
                    episodes_json(&fact.episodes),
                    fact.episode_count as i64
                ],
            )?;
            tx.commit()?;
            return Ok(Assertion {
                fact,
                invalidated: Vec::new(),
                duplicate: true,
            });
        }
        let mut candidates: Vec<Fact> = open
            .into_iter()
            .filter(|f| f.object != new.object)
            .collect();
        let mut invalid_at = new.invalid_at;
        // Rule 1.
        let mut expired_at = invalid_at.map(|_| now_ms);
        // Rule 2: sorted by valid_at, the earliest strictly newer candidate closes
        // `new`. An equal valid_at is a tie, broken by transaction time: the later
        // assertion (this one) stands.
        if expired_at.is_none() {
            candidates.sort_by_key(|c| (c.valid_at, c.id));
            if let Some(c) = candidates.iter().find(|c| c.valid_at > new.valid_at) {
                invalid_at = Some(c.valid_at);
                expired_at = Some(now_ms);
            }
        }
        // Rules 3 and 4, for open candidates: close each one that began at or
        // before `new` and overlaps it.
        let mut invalidated = Vec::new();
        for mut c in candidates {
            let disjoint = invalid_at.is_some_and(|end| end <= c.valid_at);
            if disjoint || c.valid_at > new.valid_at {
                continue;
            }
            c.invalid_at = Some(new.valid_at);
            c.expired_at = Some(now_ms);
            tx.execute(
                "UPDATE facts SET invalid_at = ?2, expired_at = ?3 WHERE id = ?1",
                params![c.id, c.invalid_at, c.expired_at],
            )?;
            invalidated.push(c);
        }
        let episodes = vec![new.episode.to_owned()];
        tx.execute(
            "INSERT INTO facts(subject, predicate, object, valid_at, invalid_at, created_at,
                               expired_at, episodes, episode_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1)",
            params![
                new.subject,
                new.predicate,
                new.object,
                new.valid_at,
                invalid_at,
                now_ms,
                expired_at,
                episodes_json(&episodes)
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Assertion {
            fact: Fact {
                id,
                subject: new.subject.to_owned(),
                predicate: new.predicate.to_owned(),
                object: new.object.to_owned(),
                valid_at: new.valid_at,
                invalid_at,
                created_at: now_ms,
                expired_at,
                episodes,
                episode_count: 1,
            },
            invalidated,
            duplicate: false,
        })
    }

    /// One fact by id.
    pub fn fact(&self, id: i64) -> Result<Option<Fact>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {COLUMNS} FROM facts WHERE id = ?1"),
                params![id],
                row_fact,
            )
            .optional()?)
    }

    /// Every fact about a subject and predicate, closed ones included, oldest
    /// first: the full history.
    pub fn fact_history(&self, subject: &str, predicate: &str) -> Result<Vec<Fact>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {COLUMNS} FROM facts WHERE subject = ?1 AND predicate = ?2
             ORDER BY valid_at, id"
        ))?;
        Ok(stmt
            .query_map(params![subject, predicate], row_fact)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Open facts with this predicate whose subject starts with `subject_prefix`.
    pub fn open_facts(&self, predicate: &str, subject_prefix: &str) -> Result<Vec<Fact>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {COLUMNS} FROM facts
             WHERE predicate = ?1 AND invalid_at IS NULL AND expired_at IS NULL
               AND substr(subject, 1, length(?2)) = ?2
             ORDER BY subject, id"
        ))?;
        Ok(stmt
            .query_map(params![predicate, subject_prefix], row_fact)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_open(f: &Fact) -> bool {
        f.invalid_at.is_none() && f.expired_at.is_none()
    }

    fn valid_at_time(f: &Fact, t: i64) -> bool {
        f.valid_at <= t && f.invalid_at.is_none_or(|end| t < end)
    }

    fn store() -> GraphStore {
        GraphStore::open_in_memory(std::path::Path::new(".")).expect("store")
    }

    fn new<'a>(object: &'a str, valid_at: i64, episode: &'a str) -> NewFact<'a> {
        NewFact {
            subject: "template:t1",
            predicate: "standing",
            object,
            valid_at,
            invalid_at: None,
            episode,
        }
    }

    #[test]
    fn a_contradicting_fact_closes_the_old_one_and_keeps_it() {
        let g = store();
        let first = g
            .assert_fact(&new("verified", 100, "run-1"), 100)
            .expect("a");
        let second = g.assert_fact(&new("failed", 200, "run-2"), 250).expect("b");
        assert_eq!(second.invalidated.len(), 1);
        let closed = &second.invalidated[0];
        assert_eq!(closed.id, first.fact.id);
        assert_eq!(
            closed.invalid_at,
            Some(200),
            "closed at the new fact's valid_at"
        );
        assert_eq!(
            closed.expired_at,
            Some(250),
            "superseded at transaction time"
        );
        // Not deleted: the history has both, and each answers for its own time.
        let history = g.fact_history("template:t1", "standing").expect("history");
        assert_eq!(history.len(), 2);
        let at = |t| -> Vec<Fact> {
            history
                .iter()
                .filter(|f| valid_at_time(f, t))
                .cloned()
                .collect()
        };
        assert_eq!(at(150)[0].object, "verified");
        assert_eq!(at(300)[0].object, "failed");
        assert!(at(50).is_empty());
        let open = g.open_facts("standing", "template:").expect("open");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].object, "failed");
    }

    #[test]
    fn a_late_arriving_older_fact_is_closed_by_the_newer_one_already_stored() {
        let g = store();
        g.assert_fact(&new("verified", 500, "run-2"), 600)
            .expect("newer");
        // Learned later, but about an earlier time: Graphiti's rule 2.
        let late = g
            .assert_fact(&new("failed", 100, "run-1"), 700)
            .expect("older");
        assert_eq!(late.fact.invalid_at, Some(500));
        assert_eq!(late.fact.expired_at, Some(700));
        assert!(late.invalidated.is_empty(), "the newer fact stays open");
        let open = g.open_facts("standing", "template:").expect("open");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].object, "verified");
    }

    #[test]
    fn disjoint_intervals_do_not_invalidate_each_other() {
        let g = store();
        let old = g
            .assert_fact(
                &NewFact {
                    invalid_at: Some(150),
                    ..new("verified", 100, "run-1")
                },
                160,
            )
            .expect("old");
        assert_eq!(old.fact.expired_at, Some(160), "rule 1: expired on arrival");
        let later = g
            .assert_fact(&new("failed", 200, "run-2"), 210)
            .expect("new");
        assert!(later.invalidated.is_empty());
        assert!(is_open(&later.fact));
    }

    #[test]
    fn an_exact_repeat_adds_its_source_instead_of_a_row() {
        let g = store();
        g.assert_fact(&new("verified", 100, "run-1"), 100)
            .expect("a");
        let again = g
            .assert_fact(&new("verified", 300, "run-2"), 300)
            .expect("b");
        assert!(again.duplicate);
        assert_eq!(again.fact.episodes, vec!["run-1", "run-2"]);
        assert_eq!(again.fact.valid_at, 100, "the original validity stands");
        assert_eq!(
            g.fact_history("template:t1", "standing").expect("h").len(),
            1
        );
    }

    #[test]
    fn an_equal_valid_at_is_a_tie_the_later_assertion_wins() {
        let g = store();
        let a = g
            .assert_fact(&new("verified", 100, "run-1"), 100)
            .expect("a");
        let b = g.assert_fact(&new("failed", 100, "run-2"), 101).expect("b");
        assert_eq!(b.invalidated.len(), 1, "the tie closes the earlier row");
        assert_eq!(b.invalidated[0].id, a.fact.id);
        assert!(is_open(&b.fact));
        let open = g.open_facts("standing", "template:").expect("open");
        assert_eq!(open.len(), 1, "never two open facts for one subject");
        assert_eq!(open[0].object, "failed");
    }

    #[test]
    fn a_closed_fact_is_not_a_candidate_again() {
        let g = store();
        g.assert_fact(&new("a", 100, "r1"), 100).expect("a");
        g.assert_fact(&new("b", 200, "r2"), 200).expect("b");
        // "c" at 150 is older than the open "b", so it is closed by it (rule 2);
        // the already-closed "a" is not touched again.
        let c = g.assert_fact(&new("c", 150, "r3"), 300).expect("c");
        assert!(c.invalidated.is_empty());
        let history = g.fact_history("template:t1", "standing").expect("h");
        let a = history.iter().find(|f| f.object == "a").expect("a");
        assert_eq!((a.invalid_at, a.expired_at), (Some(200), Some(200)));
    }

    #[test]
    fn episodes_are_capped_but_counted() {
        let g = store();
        for i in 0..(MAX_EPISODES + 5) {
            g.assert_fact(&new("verified", 100, &format!("run-{i}")), 100 + i as i64)
                .expect("assert");
        }
        let f = &g.open_facts("standing", "").expect("open")[0];
        assert_eq!(f.episode_count, (MAX_EPISODES + 5) as u64);
        assert_eq!(f.episodes.len(), MAX_EPISODES);
        assert_eq!(f.episodes.last().map(String::as_str), Some("run-36"));
        assert_eq!(f.episodes.first().map(String::as_str), Some("run-5"));
    }

    #[test]
    fn a_corrupt_episode_list_raises_instead_of_reading_as_empty() {
        let g = store();
        let f = g.assert_fact(&new("verified", 1, "run-1"), 1).expect("a");
        g.conn()
            .execute(
                "UPDATE facts SET episodes = 'not json' WHERE id = ?1",
                params![f.fact.id],
            )
            .expect("corrupt");
        assert!(g.fact(f.fact.id).is_err());
        assert!(g.open_facts("standing", "").is_err());
    }

    /// A facts table written before `episode_count` existed is migrated in
    /// place when the store opens, its count filled from the episodes listed.
    #[test]
    fn a_facts_table_from_before_episode_count_is_migrated_in_place() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("g.db");
        {
            let conn = rusqlite::Connection::open(&db).expect("conn");
            conn.execute_batch(
                "CREATE TABLE facts (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    subject TEXT NOT NULL, predicate TEXT NOT NULL, object TEXT NOT NULL,
                    valid_at INTEGER NOT NULL, invalid_at INTEGER,
                    created_at INTEGER NOT NULL, expired_at INTEGER,
                    episodes TEXT NOT NULL DEFAULT '[]');
                 INSERT INTO facts(subject, predicate, object, valid_at, created_at, episodes)
                 VALUES ('template:t1', 'standing', 'verified', 1, 1, '[\"r1\",\"r2\",\"r3\"]');",
            )
            .expect("old shape");
        }
        let g = GraphStore::open(&db, dir.path()).expect("open migrates");
        let open = g.open_facts("standing", "template:").expect("open");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].episode_count, 3);
        let again = g
            .assert_fact(&new("verified", 5, "r4"), 5)
            .expect("assert on the migrated table");
        assert_eq!(again.fact.episode_count, 4);
        drop(g);
        GraphStore::open(&db, dir.path()).expect("a second open is a no-op");
    }

    #[test]
    fn facts_survive_a_schema_rebuild() {
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("g.db");
        {
            let g = GraphStore::open(&db, dir.path()).expect("open");
            g.assert_fact(&new("verified", 1, "run-1"), 1).expect("a");
            // Force the rebuild path on the next open.
            g.set_meta("schema_version", "0").expect("meta");
        }
        let g = GraphStore::open(&db, dir.path()).expect("reopen");
        assert_eq!(g.open_facts("standing", "").expect("open").len(), 1);
    }
}
