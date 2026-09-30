//! The write-lease registry: one global list of who is touching what.
//!
//! Decision 0022 replaced predictive partitioning with **runtime per-file mutual
//! exclusion: exact, not probabilistic, no hot set, no ranking**. Two
//! experiments falsified prediction and the literature had published the same result
//! in 2004, so this is what is left, and it is the whole of the safety mechanism.
//!
//! Four properties are structural rather than procedural, which is the point: a rule
//! a worker is *told* gets broken by a worker with a good local reason, and kGraf's
//! workers write kGraf's code.
//!
//! **One list, and the database enforces exclusion.** `path` is the primary key. Two
//! workers racing for one file issue two inserts and exactly one succeeds, decided by
//! SQLite inside a transaction. There is no check to forget, no window to race in, and
//! no code path that can hold a path twice.
//!
//! **A write needs a token that only an acquire can mint.** [`WriteLease`] has no public
//! constructor, exactly as `CallToken` has none (decisions 0039, 0040). A file-write tool
//! that takes one cannot be called for an unleased path, so the violation does not
//! compile rather than failing review.
//!
//! **Cross-indexed both ways, because both directions are needed.** Path to agent answers
//! admission: who holds this? Agent to paths answers release and recovery: what is this
//! worker holding? The second is what makes a dead worker's leases reclaimable.
//!
//! **Real time, on the existing spine.** The table carries change-log triggers like every
//! other (decision 0007), so lease state reaches the interface without new plumbing, and
//! a worker blocked on a lease is a visible state distinct from dependency-blocked,
//! collision-deferred and concurrency-capped, which decision 0022 requires and decision
//! 0014 draws.
//!
//! ## Deadlock, which is new
//!
//! Decision 0022 names this: two workers each holding a file the other wants was
//! impossible under prediction and is possible now. It offers ordered acquisition or
//! lease timeouts with rollback. This module does **both**, because they fail differently.
//!
//! [`LeaseRegistry::acquire_all`] takes a whole set, sorts it, and acquires it in one
//! transaction. A canonical order plus atomicity makes the deadlock cycle unconstructible
//! rather than merely unlikely, a worker never holds a partial set while waiting.
//! Expiry is the backstop for the case ordering cannot reach: a worker that stops
//! renewing, because it crashed or hung.
//!
//! ## Why expiry rather than death detection
//!
//! Decision 0007 says terminate conservatively, because a failed probe is not proof of
//! death. That sits awkwardly beside needing a crashed worker's leases back. The
//! reconciliation is that expiry makes no claim about the worker: it says **the lease is
//! stale**, which is a fact about the lease. A worker that is alive renews and keeps it.
//! A worker that returns after its lease lapsed finds its token fenced by generation, so
//! it cannot write against a lease someone else now holds.
//!
//! ## Two grains, one module
//!
//! This module now owns two lease mechanisms, in two submodules:
//!
//! - [`node`]: sub-file leases keyed on `Chunk::id` (decisions 0049, 0050), used when a
//!   node's identity is stable enough to trust and a whole file would over-serialize.
//! - [`schedule`]: Haystack's layered write-set scheduler (Apache-2.0), ported to decide
//!   which pending work can be admitted to either registry concurrently. See
//!   `third_party/haystack/UPSTREAM`.
//!
//! The file-level registry stays exactly as decision 0022 built it: the sub-file grain
//! is additive, not a replacement, because a chunk with `identity_stable = false` (0049)
//! has no key to lease it by and must still fall back to the whole file.

mod combined;
mod node;
mod schedule;

pub use combined::MixedAcquisition;
pub use node::{NodeAcquisition, NodeConflict, NodeLease, NodeLeaseRecord};
pub use schedule::{WorkFootprint, admit_layers, schedule_layers};

use crate::error::Result;
use crate::store::GraphStore;
use rusqlite::{OptionalExtension, params};

/// How long a lease survives without renewal.
///
/// Long enough that an ordinary slow write does not lose its lease, short enough that a
/// crashed worker does not block the fleet for a coffee break. Not a researched number;
/// a starting point to tune, like the concurrency band in decision 0017.
pub const DEFAULT_LEASE_TTL_MS: i64 = 120_000;

/// Permission to write one path, and proof the registry granted it.
///
/// No public constructor. The only source is [`LeaseRegistry::acquire_all`], which has
/// already written the row. A tool that takes `&WriteLease` therefore cannot be called
/// for a path nobody holds, and that is a compile error rather than a review comment.
#[derive(Debug)]
pub struct WriteLease {
    path: String,
    agent_id: String,
    /// Fences a returning worker whose lease lapsed and was re-granted elsewhere.
    generation: i64,
}

impl WriteLease {
    /// Still no *public* constructor: this is `pub(super)` so only code inside
    /// `leases` (namely `leases::combined`, which has to build one for the mixed
    /// path-and-node admission call) can mint one, and only after it has already
    /// written the row in the same transaction. An outside caller still cannot
    /// construct a `WriteLease` for a path nobody granted it.
    pub(super) fn new(path: String, agent_id: String, generation: i64) -> Self {
        Self {
            path,
            agent_id,
            generation,
        }
    }

    /// The path this lease covers.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The agent holding it.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The generation this lease was granted at.
    pub fn generation(&self) -> i64 {
        self.generation
    }
}

/// The lease measurements of decision 0022 (serialization is a measured cost) and
/// 0049 (the share of waits whose declared ranges did not overlap).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LeaseMeasurements {
    /// Paths that more than one agent held, one after another.
    pub serialized_paths: u64,
    /// Handoffs between two agents where both grants declared a range.
    pub known_handoffs: u64,
    /// The share of those handoffs whose ranges did not overlap; `None` with no
    /// handoff to judge.
    pub needless_wait_fraction: Option<f64>,
}

/// One row of the registry, as anyone may read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRecord {
    pub path: String,
    pub agent_id: String,
    pub task_id: String,
    /// The line range the worker declared it would touch, or `None` for the whole file.
    ///
    /// **Recorded, not enforced.** Exclusion is per file, which is what decision 0022
    /// specifies. The range is kept because the measured weak point of this whole
    /// approach is granularity, a 2026 admission-control study failed closed on
    /// undeclared scope in 46 of 90 executions, and **45 of those 46 targeted files
    /// already in the declarations**, meaning the declared region was right and too
    /// coarse. Sub-file leasing would need this data and cannot recover it later, so it
    /// accrues now (decision 0020's weight-do-not-threshold, applied to a schema).
    pub declared_range: Option<(u32, u32)>,
    pub acquired_ms: i64,
    pub expires_ms: i64,
    pub generation: i64,
}

impl LeaseRecord {
    /// Whether this lease has lapsed at `now_ms`.
    pub fn is_expired(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_ms
    }
}

/// Why an acquisition did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    /// The agent that already holds it.
    pub held_by: String,
    pub held_task: String,
    /// When the current holder's lease lapses, so a caller knows how long a wait is.
    pub expires_ms: i64,
    /// Set when the block came from a sub-file lease on a node inside this path,
    /// rather than a whole-file lease on the path itself (the
    /// two registries once never checked each other; see `leases::combined`).
    pub blocking_node_id: Option<String>,
}

/// The outcome of asking for a set of paths.
///
/// All or nothing. A partial grant is what makes deadlock possible, so it is not
/// representable.
#[derive(Debug)]
pub enum Acquisition {
    /// Every path was granted.
    Granted(Vec<WriteLease>),
    /// Nothing was granted, and these are the paths that stopped it.
    ///
    /// Plural deliberately: a caller that learns only the first blocker retries in a
    /// loop, which is how a livelock is built.
    Blocked(Vec<Conflict>),
}

/// The registry. One per project, alongside the graph it shares a database with.
pub struct LeaseRegistry<'store> {
    store: &'store GraphStore,
    ttl_ms: i64,
}

impl<'store> LeaseRegistry<'store> {
    /// Open the registry over a project's store.
    pub fn new(store: &'store GraphStore) -> Self {
        Self {
            store,
            ttl_ms: DEFAULT_LEASE_TTL_MS,
        }
    }

    /// Override the lease lifetime.
    pub fn with_ttl_ms(mut self, ttl_ms: i64) -> Self {
        self.ttl_ms = ttl_ms;
        self
    }

    /// Acquire a whole write set, or nothing.
    ///
    /// Paths are sorted and taken in one transaction, so a canonical order plus
    /// atomicity removes the hold-and-wait condition deadlock needs. Lapsed leases held
    /// by others are reclaimed in the same transaction, which is the only place expiry
    /// takes effect, there is no sweeper, so a lease cannot vanish between a check and
    /// a write.
    pub fn acquire_all(
        &self,
        agent_id: &str,
        task_id: &str,
        paths: &[String],
        declared_ranges: &[Option<(u32, u32)>],
        now_ms: i64,
    ) -> Result<Acquisition> {
        self.acquire_all_impl(agent_id, task_id, paths, declared_ranges, now_ms)
    }

    /// Renew a held lease, pushing its expiry out.
    ///
    /// Fails when the generation no longer matches, which is how a worker that hung past
    /// its expiry and came back learns it no longer holds the file rather than writing
    /// over whoever does. **Also fails when the row has already lapsed as of `now_ms`**,
    /// even if the generation still matches and nobody else has acquired the path yet.
    /// The original version renewed a stale row unconditionally:
    /// a lease that lapsed is meant to be reclaimable the instant it lapses (0048), and
    /// silently reviving it after the fact (even briefly, even uncontested) reopens
    /// the exact window ordered acquisition exists to close. `expires_ms > ?now` in the
    /// `WHERE` clause reads the row's state from *before* this call's own update, so a
    /// lease that was already stale the moment `renew` was called cannot renew itself
    /// back to life.
    pub fn renew(&self, lease: &WriteLease, now_ms: i64) -> Result<bool> {
        let changed = self.store.conn().execute(
            "UPDATE leases SET expires_ms = ?1
             WHERE path = ?2 AND agent_id = ?3 AND generation = ?4 AND expires_ms > ?5",
            params![
                now_ms + self.ttl_ms,
                lease.path,
                lease.agent_id,
                lease.generation,
                now_ms,
            ],
        )?;
        Ok(changed == 1)
    }

    /// Release one lease. Consumes it, so a released lease cannot be used again.
    pub fn release(&self, lease: WriteLease) -> Result<()> {
        self.store.conn().execute(
            "DELETE FROM leases WHERE path = ?1 AND agent_id = ?2 AND generation = ?3",
            params![lease.path, lease.agent_id, lease.generation],
        )?;
        Ok(())
    }

    /// Release everything an agent holds.
    ///
    /// The agent-to-paths direction of the cross-index, and what a worker's teardown
    /// calls. Returns how many were released.
    pub fn release_all(&self, agent_id: &str) -> Result<usize> {
        Ok(self
            .store
            .conn()
            .execute("DELETE FROM leases WHERE agent_id = ?1", params![agent_id])?)
    }

    /// Who holds this path right now, if anyone whose lease has not lapsed.
    pub fn holder_of(&self, path: &str, now_ms: i64) -> Result<Option<LeaseRecord>> {
        Ok(self.read_one(path)?.filter(|l| !l.is_expired(now_ms)))
    }

    /// Everything this agent holds. The other direction of the index.
    pub fn held_by(&self, agent_id: &str) -> Result<Vec<LeaseRecord>> {
        let conn = self.store.conn();
        let mut stmt = conn.prepare(
            "SELECT path, agent_id, task_id, range_start, range_end, acquired_ms,
                    expires_ms, generation
             FROM leases WHERE agent_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![agent_id], lease_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The whole list, which is one list.
    pub fn all(&self) -> Result<Vec<LeaseRecord>> {
        let conn = self.store.conn();
        let mut stmt = conn.prepare(
            "SELECT path, agent_id, task_id, range_start, range_end, acquired_ms,
                    expires_ms, generation
             FROM leases ORDER BY path",
        )?;
        let rows = stmt.query_map([], lease_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Decision 0049's needless-wait fraction: of the grants that serialized behind a
    /// different agent's prior grant on the same path, the share whose declared ranges
    /// did not actually overlap.
    ///
    /// A handoff is one grant immediately following a different agent's grant on the
    /// same path in `lease_history`, ordered by insertion. It counts only when **both**
    /// grants declared a range; a handoff involving an undeclared (whole-file) range is
    /// excluded from both the numerator and the denominator, because "was this needless"
    /// has no answer when one side never said what it touched. This mirrors the range
    /// column itself: recorded when given, never guessed (0022, 0049).
    ///
    /// Returns `None` when there are no handoffs with both ranges known, rather than a
    /// division by zero pretending to be a rate.
    pub fn needless_wait_fraction(&self) -> Result<Option<f64>> {
        Ok(self.measurements("")?.needless_wait_fraction)
    }

    /// The lease measurements decision 0022 says must be visible, over the grants
    /// whose agent id starts with `holder_prefix` (a run's id and a slash scopes them
    /// to one run; the empty prefix is every grant). A handoff is counted only
    /// between two grants inside the prefix.
    ///
    /// - `serialized_paths`: paths that agents of the prefix held one after another,
    ///   so a second agent waited behind a first: the serialization cost.
    /// - `known_handoffs`, `needless_wait_fraction`: see
    ///   [`Self::needless_wait_fraction`].
    pub fn measurements(&self, holder_prefix: &str) -> Result<LeaseMeasurements> {
        let conn = self.store.conn();
        let serialized: i64 = conn.query_row(
            "SELECT COUNT(*) FROM (
                 SELECT path FROM lease_history
                 WHERE substr(agent_id, 1, length(?1)) = ?1
                 GROUP BY path HAVING COUNT(DISTINCT agent_id) > 1)",
            params![holder_prefix],
            |row| row.get(0),
        )?;
        let (known, needless): (i64, i64) = conn.query_row(
            "WITH scoped AS (
                 SELECT id, path, agent_id, range_start, range_end
                 FROM lease_history
                 WHERE substr(agent_id, 1, length(?1)) = ?1
             ),
             ordered AS (
                 SELECT agent_id, range_start, range_end,
                        LAG(agent_id) OVER (PARTITION BY path ORDER BY id) AS prev_agent,
                        LAG(range_start) OVER (PARTITION BY path ORDER BY id) AS prev_start,
                        LAG(range_end) OVER (PARTITION BY path ORDER BY id) AS prev_end
                 FROM scoped
             ),
             handoffs AS (
                 SELECT * FROM ordered
                 WHERE prev_agent IS NOT NULL AND prev_agent <> agent_id
             )
             SELECT
                 COUNT(*) FILTER (
                     WHERE range_start IS NOT NULL AND range_end IS NOT NULL
                       AND prev_start IS NOT NULL AND prev_end IS NOT NULL
                 ),
                 COUNT(*) FILTER (
                     WHERE range_start IS NOT NULL AND range_end IS NOT NULL
                       AND prev_start IS NOT NULL AND prev_end IS NOT NULL
                       AND (range_start > prev_end OR prev_start > range_end)
                 )
             FROM handoffs",
            params![holder_prefix],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(LeaseMeasurements {
            serialized_paths: serialized as u64,
            known_handoffs: known as u64,
            needless_wait_fraction: (known > 0).then(|| needless as f64 / known as f64),
        })
    }

    fn read_one(&self, path: &str) -> Result<Option<LeaseRecord>> {
        Ok(self
            .store
            .conn()
            .query_row(
                "SELECT path, agent_id, task_id, range_start, range_end, acquired_ms,
                        expires_ms, generation
                 FROM leases WHERE path = ?1",
                params![path],
                lease_from_row,
            )
            .optional()?)
    }
}

fn lease_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LeaseRecord> {
    let start: Option<u32> = row.get(3)?;
    let end: Option<u32> = row.get(4)?;
    Ok(LeaseRecord {
        path: row.get(0)?,
        agent_id: row.get(1)?,
        task_id: row.get(2)?,
        declared_range: start.zip(end),
        acquired_ms: row.get(5)?,
        expires_ms: row.get(6)?,
        generation: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ChangeOp, EventTail};

    fn store() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        (dir, store)
    }

    fn paths(items: &[&str]) -> Vec<String> {
        items.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn one_path_can_be_held_by_exactly_one_agent() {
        // The whole mechanism. Enforced by the primary key, not by a check.
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store);
        let first = registry
            .acquire_all("agent-a", "task-1", &paths(&["src/lib.rs"]), &[], 1_000)
            .expect("acquire");
        assert!(matches!(first, Acquisition::Granted(_)));

        let second = registry
            .acquire_all("agent-b", "task-2", &paths(&["src/lib.rs"]), &[], 1_001)
            .expect("acquire");
        match second {
            Acquisition::Blocked(conflicts) => {
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].held_by, "agent-a");
                assert_eq!(conflicts[0].held_task, "task-1");
            }
            Acquisition::Granted(_) => panic!("two agents hold one path"),
        }
    }

    #[test]
    fn a_blocked_set_grants_nothing_at_all() {
        // A partial grant is the hold-and-wait condition deadlock needs, so it must not
        // be representable. This asserts the other paths stayed free.
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store);
        registry
            .acquire_all("agent-a", "t1", &paths(&["b.rs"]), &[], 1_000)
            .expect("acquire");

        let attempt = registry
            .acquire_all(
                "agent-b",
                "t2",
                &paths(&["a.rs", "b.rs", "c.rs"]),
                &[],
                1_000,
            )
            .expect("acquire");
        assert!(matches!(attempt, Acquisition::Blocked(_)));
        assert!(
            registry.holder_of("a.rs", 1_000).expect("read").is_none(),
            "a.rs must not have been taken by a failed acquisition"
        );
        assert!(registry.holder_of("c.rs", 1_000).expect("read").is_none());
        assert!(registry.held_by("agent-b").expect("read").is_empty());
    }

    #[test]
    fn every_blocker_is_reported_not_just_the_first() {
        // A caller that learns one blocker at a time retries in a loop, which is a
        // livelock.
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store);
        registry
            .acquire_all("holder-1", "t1", &paths(&["a.rs"]), &[], 1_000)
            .expect("acquire");
        registry
            .acquire_all("holder-2", "t2", &paths(&["c.rs"]), &[], 1_000)
            .expect("acquire");
        match registry
            .acquire_all(
                "agent-x",
                "t3",
                &paths(&["a.rs", "b.rs", "c.rs"]),
                &[],
                1_000,
            )
            .expect("acquire")
        {
            Acquisition::Blocked(conflicts) => {
                assert_eq!(conflicts.len(), 2, "{conflicts:#?}");
                let holders: Vec<&str> = conflicts.iter().map(|c| c.held_by.as_str()).collect();
                assert!(holders.contains(&"holder-1") && holders.contains(&"holder-2"));
            }
            Acquisition::Granted(_) => panic!("should have blocked"),
        }
    }

    #[test]
    fn acquisition_is_ordered_so_a_deadlock_cycle_cannot_be_built() {
        // Two agents wanting the same two files in opposite orders is the textbook
        // deadlock. Sorting inside one transaction makes it unconstructible: one agent
        // gets both, the other gets none.
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store);
        let a = registry
            .acquire_all("agent-a", "t1", &paths(&["x.rs", "y.rs"]), &[], 1_000)
            .expect("acquire");
        let b = registry
            .acquire_all("agent-b", "t2", &paths(&["y.rs", "x.rs"]), &[], 1_000)
            .expect("acquire");
        assert!(matches!(a, Acquisition::Granted(_)));
        assert!(matches!(b, Acquisition::Blocked(_)));
        assert_eq!(registry.held_by("agent-a").expect("read").len(), 2);
        assert_eq!(registry.held_by("agent-b").expect("read").len(), 0);
    }

    #[test]
    fn a_lapsed_lease_is_reclaimable_and_fences_the_old_holder() {
        // Expiry makes no claim that the old worker died, which decision 0007 forbids
        // concluding from a failed probe. It says the lease is stale. The generation is
        // what stops a returning worker writing against it.
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store).with_ttl_ms(1_000);
        let first = registry
            .acquire_all("agent-a", "t1", &paths(&["src/lib.rs"]), &[], 1_000)
            .expect("acquire");
        let old_lease = match first {
            Acquisition::Granted(mut leases) => leases.remove(0),
            Acquisition::Blocked(_) => unreachable!(),
        };

        // Past expiry, someone else may take it.
        let second = registry
            .acquire_all("agent-b", "t2", &paths(&["src/lib.rs"]), &[], 5_000)
            .expect("acquire");
        assert!(matches!(second, Acquisition::Granted(_)));

        // The original holder's renewal now fails: it no longer holds the file.
        assert!(
            !registry.renew(&old_lease, 5_001).expect("renew"),
            "a fenced lease must not renew"
        );
        assert_eq!(
            registry
                .holder_of("src/lib.rs", 5_001)
                .expect("read")
                .expect("held")
                .agent_id,
            "agent-b"
        );
    }

    #[test]
    fn renewal_keeps_a_live_lease_and_the_holder_stays() {
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store).with_ttl_ms(1_000);
        let lease = match registry
            .acquire_all("agent-a", "t1", &paths(&["a.rs"]), &[], 1_000)
            .expect("acquire")
        {
            Acquisition::Granted(mut l) => l.remove(0),
            Acquisition::Blocked(_) => unreachable!(),
        };
        assert!(registry.renew(&lease, 1_900).expect("renew"));
        // Renewed at 1900 with a 1000ms ttl, so still held at 2500 where it would
        // otherwise have lapsed at 2000.
        assert!(registry.holder_of("a.rs", 2_500).expect("read").is_some());
    }

    #[test]
    fn an_agent_re_acquiring_its_own_path_is_a_renewal_not_a_conflict() {
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store);
        registry
            .acquire_all("agent-a", "t1", &paths(&["a.rs"]), &[], 1_000)
            .expect("acquire");
        let again = registry
            .acquire_all("agent-a", "t1", &paths(&["a.rs", "b.rs"]), &[], 1_100)
            .expect("acquire");
        assert!(matches!(again, Acquisition::Granted(_)));
        assert_eq!(registry.held_by("agent-a").expect("read").len(), 2);
    }

    #[test]
    fn the_list_is_cross_indexed_in_both_directions() {
        // Path to agent answers admission. Agent to paths answers teardown and recovery.
        // Both are needed and neither is derivable cheaply from the other at scale.
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store);
        registry
            .acquire_all("agent-a", "t1", &paths(&["a.rs", "b.rs"]), &[], 1_000)
            .expect("acquire");
        registry
            .acquire_all("agent-b", "t2", &paths(&["c.rs"]), &[], 1_000)
            .expect("acquire");

        assert_eq!(
            registry
                .holder_of("b.rs", 1_000)
                .expect("read")
                .expect("held")
                .agent_id,
            "agent-a"
        );
        let a_paths: Vec<String> = registry
            .held_by("agent-a")
            .expect("read")
            .into_iter()
            .map(|l| l.path)
            .collect();
        assert_eq!(a_paths, vec!["a.rs".to_owned(), "b.rs".to_owned()]);
        assert_eq!(
            registry.all().expect("read").len(),
            3,
            "one list, three rows"
        );
    }

    #[test]
    fn releasing_an_agent_frees_everything_it_held() {
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store);
        registry
            .acquire_all("agent-a", "t1", &paths(&["a.rs", "b.rs"]), &[], 1_000)
            .expect("acquire");
        assert_eq!(registry.release_all("agent-a").expect("release"), 2);
        assert!(registry.all().expect("read").is_empty());
        // And the path is immediately available.
        assert!(matches!(
            registry
                .acquire_all("agent-b", "t2", &paths(&["a.rs"]), &[], 1_001)
                .expect("acquire"),
            Acquisition::Granted(_)
        ));
    }

    #[test]
    fn lease_changes_reach_the_event_spine_without_any_hand_placed_emission() {
        // Decision 0007: triggers write, a poller tails. Decision 0022 requires a
        // lease wait to be a visible state, and this is what makes it visible.
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store);
        let mut tail = EventTail::from_start();
        registry
            .acquire_all("agent-a", "t1", &paths(&["a.rs"]), &[], 1_000)
            .expect("acquire");
        let batch = tail.next_batch(&store, 100).expect("batch");
        assert!(
            batch
                .iter()
                .any(|c| c.table == "leases" && c.op == ChangeOp::Insert),
            "an acquisition must be announced: {batch:#?}"
        );

        registry.release_all("agent-a").expect("release");
        let batch = tail.next_batch(&store, 100).expect("batch");
        assert!(
            batch
                .iter()
                .any(|c| c.table == "leases" && c.op == ChangeOp::Delete),
            "a release must be announced: {batch:#?}"
        );
    }

    #[test]
    fn the_declared_range_is_recorded_even_though_exclusion_is_per_file() {
        // The measured weak point of this approach is granularity: 45 of 46 failed-closed
        // blocks in the admission study targeted files already declared, meaning the
        // region was right and too coarse. Sub-file leasing needs this data and cannot
        // recover it later, so it accrues now.
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store);
        registry
            .acquire_all("agent-a", "t1", &paths(&["a.rs"]), &[Some((10, 40))], 1_000)
            .expect("acquire");
        let held = registry
            .holder_of("a.rs", 1_000)
            .expect("read")
            .expect("held");
        assert_eq!(held.declared_range, Some((10, 40)));

        // And it does not weaken exclusion: a disjoint range on the same file still
        // blocks, because 0022 specifies per-file mutual exclusion.
        let other = registry
            .acquire_all(
                "agent-b",
                "t2",
                &paths(&["a.rs"]),
                &[Some((900, 999))],
                1_000,
            )
            .expect("acquire");
        assert!(
            matches!(other, Acquisition::Blocked(_)),
            "exclusion is per file; a disjoint declared range must not grant"
        );
    }

    #[test]
    fn serialization_is_counted_because_it_is_the_cost_of_the_mechanism() {
        // An experiment established that every prevented collision is a serialized pair, so
        // 0022's acceptance criterion asks for the rate. It has to be observable.
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store).with_ttl_ms(10);
        registry
            .acquire_all(
                "agent-a",
                "t1",
                &paths(&["shared.rs", "only-a.rs"]),
                &[],
                1_000,
            )
            .expect("acquire");
        registry.release_all("agent-a").expect("release");
        registry
            .acquire_all("agent-b", "t2", &paths(&["shared.rs"]), &[], 2_000)
            .expect("acquire");
        assert_eq!(
            registry.measurements("").expect("count").serialized_paths,
            1,
            "only shared.rs was held by two agents"
        );
    }

    /// A run's measurements count only that run's grants: a handoff between one
    /// run's agent and another run's agent is neither run's serialization.
    #[test]
    fn measurements_are_scoped_to_the_runs_own_grants() {
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store).with_ttl_ms(10);
        let grant = |agent: &str, path: &str, range: (u32, u32)| {
            registry
                .acquire_all(agent, "t", &paths(&[path]), &[Some(range)], 1_000)
                .expect("acquire");
            registry.release_all(agent).expect("release");
        };
        // Run r1: two agents hand `x.rs` over with disjoint ranges.
        grant("r1/a@1", "x.rs", (1, 10));
        grant("r1/b@1", "x.rs", (50, 60));
        // Run r2 takes `x.rs` next, overlapping r1/b's range, and `y.rs` alone.
        grant("r2/a@1", "x.rs", (55, 70));
        grant("r2/a@1", "y.rs", (1, 5));

        let r1 = registry.measurements("r1/").expect("r1");
        assert_eq!(r1.serialized_paths, 1);
        assert_eq!(r1.known_handoffs, 1);
        assert_eq!(r1.needless_wait_fraction, Some(1.0));

        let r2 = registry.measurements("r2/").expect("r2");
        assert_eq!(
            r2.serialized_paths, 0,
            "r2 alone never waited behind itself"
        );
        assert_eq!(r2.known_handoffs, 0);
        assert_eq!(r2.needless_wait_fraction, None);

        let all = registry.measurements("").expect("all");
        assert_eq!(all.serialized_paths, 1, "x.rs was held by three agents");
        assert_eq!(all.known_handoffs, 2);
        assert_eq!(all.needless_wait_fraction, Some(0.5));
    }

    #[test]
    fn needless_wait_fraction_is_none_with_no_handoffs() {
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store);
        registry
            .acquire_all("agent-a", "t1", &paths(&["a.rs"]), &[Some((1, 10))], 1_000)
            .expect("acquire");
        assert_eq!(
            registry.needless_wait_fraction().expect("query"),
            None,
            "one agent's own grant is not a handoff between two agents"
        );
    }

    #[test]
    fn needless_wait_fraction_counts_non_overlapping_handoffs_as_needless() {
        // Decision 0049: of the pairs the registry serialized (a second agent forced
        // to wait for the file), the share whose declared ranges did not actually
        // overlap. Two handoffs here: one genuinely overlapping (not needless), one
        // disjoint (needless) -- so the fraction must land at exactly one half.
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store).with_ttl_ms(10);

        registry
            .acquire_all(
                "agent-a",
                "t1",
                &paths(&["overlap.rs"]),
                &[Some((1, 20))],
                1_000,
            )
            .expect("acquire");
        registry.release_all("agent-a").expect("release");
        registry
            .acquire_all(
                "agent-b",
                "t2",
                &paths(&["overlap.rs"]),
                &[Some((15, 30))],
                2_000,
            )
            .expect("acquire");

        registry
            .acquire_all(
                "agent-a",
                "t3",
                &paths(&["disjoint.rs"]),
                &[Some((1, 10))],
                1_000,
            )
            .expect("acquire");
        registry.release_all("agent-a").expect("release");
        registry
            .acquire_all(
                "agent-b",
                "t4",
                &paths(&["disjoint.rs"]),
                &[Some((900, 999))],
                2_000,
            )
            .expect("acquire");

        assert_eq!(registry.needless_wait_fraction().expect("query"), Some(0.5));
    }

    #[test]
    fn a_duplicate_path_in_one_request_does_not_self_conflict() {
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store);
        let got = registry
            .acquire_all("agent-a", "t1", &paths(&["a.rs", "a.rs"]), &[], 1_000)
            .expect("acquire");
        match got {
            Acquisition::Granted(leases) => assert_eq!(leases.len(), 1),
            Acquisition::Blocked(c) => panic!("self-conflicted: {c:#?}"),
        }
    }

    #[test]
    fn a_mismatched_range_list_is_refused_rather_than_silently_misaligned() {
        let (_dir, store) = store();
        let registry = LeaseRegistry::new(&store);
        assert!(
            registry
                .acquire_all(
                    "agent-a",
                    "t1",
                    &paths(&["a.rs", "b.rs"]),
                    &[Some((1, 2))],
                    1_000
                )
                .is_err()
        );
    }
}
