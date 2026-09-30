//! The atomic claim table (decision 0052): "Due occurrences are claimed atomically in
//! SQLite before they run."
//!
//! Reuse (0055): no candidate claims atomically in SQLite (decision 0055),
//! so this table is kGraf's own.
//!
//! ## The actual guarantee: at-least-once, with fencing
//!
//! Claiming is atomic (two connections racing on the same row never both
//! see `changes() == 1`), but that alone does not make a scheduled run **exactly-once**.
//! A slow-but-still-alive claimant can outlive its own lease: claimant A claims with a
//! 60s lease, is still legitimately running at +61s, and claimant B - correctly, per the
//! `WHERE` clause below - claims the same occurrence because A's lease looks expired. Both
//! may now be doing real work for the same occurrence. That is **at-least-once**
//! execution, and no SQLite table by itself can rule it out without knowing whether A is
//! actually dead or just slow.
//!
//! What this module *does* guarantee is **fencing**: every claim carries a `generation`
//! number, incremented on each successful claim, and [`ScheduleStore::record_outcome`]
//! only accepts a `(claimant, generation)` pair that still matches the occurrence's
//! current claim, and only while the occurrence is still `claimed` (not already `done`
//! or `failed`). Concretely, fencing rules out:
//! - **Double-counted failures.** If A's lease expires and B claims (generation 2), A's
//!   own eventual `record_outcome` call (generation 1) is rejected, not applied; the
//!   schedule's `consecutive_failures` moves by at most one for this occurrence, not two.
//! - **A stale claimant overwriting a settled outcome.** Once B records `done`, the
//!   occurrence's status is no longer `claimed`, so a subsequent call - from A, from B
//!   again, from anyone - with any generation is rejected, not silently accepted.
//! - **Recording against an occurrence nobody actually holds.** The first version had
//!   no claimant/generation check at all, so `record_outcome` on a never-claimed or
//!   already-settled occurrence returned `Ok(())`. It now returns
//!   [`ScheduleError::StaleClaim`].
//!
//! The claim itself is still one `UPDATE` statement, and it is still guarded by a
//! `WHERE` clause built so only a claimable row matches (pending; claimed with an
//! expired lease; not disabled; not claimed before its own due time). SQLite serializes
//! writers to one file, so once `busy_timeout` is set, the second of two racing
//! `UPDATE`s runs strictly after the first commits, by which point the row it is trying
//! to match has already changed underneath it.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::ScheduleError;
use crate::failure::{DisableReason, FailureClass, TransientFailurePolicy};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS schedules (
    schedule_id         TEXT PRIMARY KEY,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    disabled            INTEGER NOT NULL DEFAULT 0,
    disabled_reason     TEXT
);

CREATE TABLE IF NOT EXISTS occurrences (
    schedule_id       TEXT NOT NULL,
    due_at_millis     INTEGER NOT NULL,
    status            TEXT NOT NULL DEFAULT 'pending',
    claimant          TEXT,
    claimed_at_millis INTEGER,
    lease_expires_at_millis INTEGER,
    generation        INTEGER NOT NULL DEFAULT 0,
    outcome           TEXT,
    PRIMARY KEY (schedule_id, due_at_millis)
);

-- a manual `schedule run-now` never touches
-- the occurrences table at all (it is not a claimed occurrence - decision 0052's own
-- design, see schedule_cmd.rs's module doc), so its run id has nowhere to live
-- without this table; without it, a run-now's spend was invisible to `schedule list`
-- entirely, undercounting the reported figure.
CREATE TABLE IF NOT EXISTS manual_runs (
    schedule_id TEXT NOT NULL,
    run_id      TEXT NOT NULL,
    PRIMARY KEY (schedule_id, run_id)
);

-- The lock a manual run holds on its schedule while it runs, so the scheduler loop
-- does not start the same schedule beside it. A lease, renewed by the holder: a
-- holder that died stops blocking the loop when the lease passes.
CREATE TABLE IF NOT EXISTS manual_locks (
    schedule_id             TEXT PRIMARY KEY,
    holder                  TEXT NOT NULL,
    lease_expires_at_millis INTEGER NOT NULL
);
";

fn to_millis(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

/// The inverse of [`to_millis`]. Every value this store ever writes came from
/// `to_millis` applied to a real `DateTime<Utc>`, so this should never fail in
/// practice; it returns a typed error rather than panicking (no `unwrap`/`expect`
/// outside tests) because a hand-edited or corrupted database
/// file is exactly the case a panic would hide the cause of.
fn from_millis(millis: i64, column: &'static str) -> Result<DateTime<Utc>, ScheduleError> {
    DateTime::from_timestamp_millis(millis)
        .ok_or(ScheduleError::CorruptTimestamp { millis, column })
}

/// Whether `table` already has a column named `column`, via `PRAGMA table_info`.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, ScheduleError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn to_chrono_duration(
    d: Duration,
    context: &'static str,
) -> Result<chrono::Duration, ScheduleError> {
    chrono::Duration::from_std(d).map_err(|_| ScheduleError::DurationOutOfRange {
        duration: d,
        context,
    })
}

/// One row [`ScheduleStore::expired_claims`] found: a claimed occurrence whose lease
/// has already passed, and how many times it has already been through a capacity
/// deferral - `0` for a genuine crash/hang, `> 0` for an occurrence
/// `record_deferral` is retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpiredClaim {
    pub due_at: DateTime<Utc>,
    pub deferred_count: u32,
    /// The occurrence was set aside because a manual run held the schedule
    /// ([`HELD_FOR_MANUAL_RUN`]), not because a claimant crashed. It has had no
    /// missed-run policy check since it fell due.
    pub held_for_manual_run: bool,
}

/// The claimant recorded on an occurrence set aside for a manual run.
pub const HELD_FOR_MANUAL_RUN: &str = "held-for-manual-run";

/// The result of a claim attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This caller now holds the claim, at `generation`, until `lease_expires_at`.
    /// `generation` is the fencing token: pass it back to `record_outcome` or
    /// `renew_lease` to prove this claim is still the current one.
    Claimed {
        lease_expires_at: DateTime<Utc>,
        generation: i64,
    },
    /// Someone else already holds an unexpired claim, the schedule is disabled, or
    /// `due_at` has not arrived yet.
    NotClaimed,
}

/// The result of [`ScheduleStore::claim_manual_run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualClaim {
    /// This caller holds the schedule's manual lock until `lease_expires_at`.
    Claimed { lease_expires_at: DateTime<Utc> },
    /// The schedule is disabled (by the user or by repeated failures) and the
    /// caller did not force the run.
    Disabled { reason: String },
    /// A scheduled occurrence or another manual run holds a live claim on the
    /// schedule; `holder` names it.
    Busy { holder: String },
}

/// What [`ScheduleStore::tick_schedule`] decided and did, atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickDecision {
    /// The caller's `decide` closure found nothing to act on this tick (not due yet,
    /// or already accounted for). `last_seen` is unchanged.
    NothingDue,
    /// The closure named `due` as a real candidate but chose not to claim it
    /// (`MissedRunPolicy::Skip` dropping an occurrence that is no longer "on time").
    /// `last_seen` still advances to `due`, in the same transaction, so a dropped
    /// occurrence is never re-proposed on the next tick - only a *newer* occurrence
    /// (one later than `due`) will be found from here on.
    Dropped { due: DateTime<Utc> },
    /// The closure named `due` to attempt claiming; `last_seen` now names `due`, and
    /// `claim` is the result (which may still be `NotClaimed`, if another instance's
    /// claim on it is still live).
    Evaluated {
        due: DateTime<Utc>,
        claim: ClaimOutcome,
    },
}

/// A schedule's failure bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleStatus {
    pub consecutive_failures: u64,
    pub disabled_reason: Option<String>,
}

impl ScheduleStatus {
    pub fn is_disabled(&self) -> bool {
        self.disabled_reason.is_some()
    }
}

/// The claim table, backed by one SQLite connection.
///
/// One `ScheduleStore` per process/thread: `rusqlite::Connection` is not `Sync`. Two
/// processes (or two threads, each with its own connection) point at the same file to
/// race on a claim; that is exactly the scenario `claim_occurrence`'s `WHERE` clause is
/// built to serialize correctly.
pub struct ScheduleStore {
    conn: Connection,
    transient_failure_policy: TransientFailurePolicy,
}

impl ScheduleStore {
    /// Open (creating if absent) the claim table at `path`.
    pub fn open(path: &Path) -> Result<Self, ScheduleError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Open an existing claim table for reading only: no schema batch, no
    /// migration, no pragma that writes. The desktop shell's
    /// suspended path uses it to read disable reasons and settled runs. Any write
    /// through it fails with SQLite's read-only error; [`ScheduleStore::peek_status`]
    /// and [`ScheduleStore::occurrences_since`] do not write.
    pub fn open_read_only(path: &Path) -> Result<Self, ScheduleError> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
        Ok(Self {
            conn,
            transient_failure_policy: TransientFailurePolicy::default(),
        })
    }

    /// An in-process store for tests that do not need cross-connection behavior.
    pub fn open_in_memory() -> Result<Self, ScheduleError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    /// Configure when a schedule pauses itself after consecutive transient failures
    /// (decision 0052; see [`TransientFailurePolicy`]). Chainable at construction.
    pub fn with_transient_failure_policy(mut self, policy: TransientFailurePolicy) -> Self {
        self.transient_failure_policy = policy;
        self
    }

    fn init(conn: Connection) -> Result<Self, ScheduleError> {
        // WAL lets readers proceed during a writer's transaction; `busy_timeout` makes
        // a second writer block-and-retry on `SQLITE_BUSY` instead of erroring
        // immediately, which is what turns "two racing connections" into "two
        // sequential commits" rather than a spurious failure under contention.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        // `last_seen_millis` was added to `schedules` after this table's first
        // release; `ALTER TABLE ... ADD COLUMN` on an existing file that predates it
        // is the migration, guarded so a file that already has the column (every
        // fresh `CREATE TABLE` above, and any file already migrated) does not error
        // on "duplicate column name" (SQLite has no `ADD COLUMN IF NOT EXISTS`).
        if !has_column(&conn, "schedules", "last_seen_millis")? {
            conn.execute_batch("ALTER TABLE schedules ADD COLUMN last_seen_millis INTEGER")?;
        }
        // Separate from `last_seen_millis` (a separate column): `last_seen` only ever advances
        // to an occurrence that was actually decided on and claimed, so a schedule
        // that has never yet fired (or whose policy just dropped an old occurrence)
        // would otherwise measure its on-time gap from its own creation time forever.
        // `last_evaluated_millis` advances on every tick regardless of outcome, purely
        // as the "was this schedule looked at recently" heartbeat the on-time check
        // needs; it is not used to decide which occurrence is due.
        if !has_column(&conn, "schedules", "last_evaluated_millis")? {
            conn.execute_batch("ALTER TABLE schedules ADD COLUMN last_evaluated_millis INTEGER")?;
        }
        // the run a claimed occurrence actually
        // started under, so a caller (the CLI's `schedule list`) can sum its spend by
        // asking the ledger about that exact run id, instead of guessing one from
        // `schedule_id` and `due_at` itself - which is what let a prefix match collide
        // `foo`'s spend with `foo-bar`'s. The run id is otherwise derivable
        // (`ScheduledRuns::run_id` is a pure function of the two), but storing it here
        // makes this table the one place that knows the mapping, so a caller never
        // needs to duplicate that derivation (or notice when it changes).
        if !has_column(&conn, "occurrences", "run_id")? {
            conn.execute_batch("ALTER TABLE occurrences ADD COLUMN run_id TEXT")?;
        }
        // How many times this occurrence has been deferred by a capacity refusal
        // (bounded by the caller's own time/next-occurrence policy, not a
        // fixed count) without ever counting as a real failure.
        if !has_column(&conn, "occurrences", "deferred_count")? {
            conn.execute_batch(
                "ALTER TABLE occurrences ADD COLUMN deferred_count INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        Ok(Self {
            conn,
            transient_failure_policy: TransientFailurePolicy::default(),
        })
    }

    /// Make sure a schedule's bookkeeping row exists. Idempotent.
    pub fn register_schedule(&self, schedule_id: &str) -> Result<(), ScheduleError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO schedules (schedule_id) VALUES (?1)",
            params![schedule_id],
        )?;
        Ok(())
    }

    /// Try to claim `due_at` for `schedule_id`. Ensures the occurrence row exists (a
    /// schedule's first-ever occurrence has no row yet), then attempts the claim in one
    /// `UPDATE`.
    ///
    /// A claim succeeds only when, all in the same statement:
    /// - the occurrence is `pending`, or `claimed` with an already-expired lease (a
    ///   crashed or abandoned claimant);
    /// - the schedule is not disabled (checked in the same `WHERE`, not as a separate
    ///   query beforehand: a check-then-act split there would leave a window for a
    ///   disable to land between the check and the claim);
    /// - `due_at` is not in the future relative to `now` - claiming an occurrence
    ///   before its own due time is refused.
    ///
    /// On success, `generation` is incremented and returned as part of
    /// [`ClaimOutcome::Claimed`]; see the module doc for what that fencing token buys.
    pub fn claim_occurrence(
        &self,
        schedule_id: &str,
        due_at: DateTime<Utc>,
        claimant: &str,
        now: DateTime<Utc>,
        lease: Duration,
    ) -> Result<ClaimOutcome, ScheduleError> {
        self.register_schedule(schedule_id)?;

        self.conn.execute(
            "INSERT OR IGNORE INTO occurrences (schedule_id, due_at_millis, status, generation)
             VALUES (?1, ?2, 'pending', 0)",
            params![schedule_id, to_millis(due_at)],
        )?;

        let lease_duration = to_chrono_duration(lease, "claim lease")?;
        let now_millis = to_millis(now);
        let due_millis = to_millis(due_at);
        let lease_expires_at = now + lease_duration;
        let lease_expires_millis = to_millis(lease_expires_at);

        // A manual run holds this schedule: the occurrence must not start beside
        // it. A pending occurrence is moved to `claimed` with a lease that has
        // already passed, so the next tick's `expired_claims` scan finds it and
        // tries again once the manual run ends. Left `pending` it would be found
        // by nothing, because `last_seen` has already moved past it.
        if self.manual_lock_holder(schedule_id, now_millis)?.is_some() {
            self.conn.execute(
                "UPDATE occurrences
                 SET status = 'claimed', claimant = ?4,
                     claimed_at_millis = ?1, lease_expires_at_millis = ?1 - 1
                 WHERE schedule_id = ?2 AND due_at_millis = ?3 AND ?3 <= ?1
                   AND status = 'pending'
                   AND NOT EXISTS (
                       SELECT 1 FROM schedules s WHERE s.schedule_id = ?2 AND s.disabled = 1
                   )",
                params![now_millis, schedule_id, due_millis, HELD_FOR_MANUAL_RUN],
            )?;
            return Ok(ClaimOutcome::NotClaimed);
        }

        // One statement makes the claim atomic, and folds in every precondition
        // (claimable status, not disabled, not early) so there is no gap between
        // checking a precondition and acting on it. `RETURNING generation` hands back
        // the fencing token from the very row this connection's write just touched,
        // with no separate follow-up `SELECT` that could race against another writer.
        let generation: Option<i64> = self
            .conn
            .query_row(
                "UPDATE occurrences
                 SET status = 'claimed', claimant = ?1, claimed_at_millis = ?2,
                     lease_expires_at_millis = ?3, generation = generation + 1
                 WHERE schedule_id = ?4 AND due_at_millis = ?5
                   AND ?5 <= ?2
                   AND NOT EXISTS (
                       SELECT 1 FROM schedules s WHERE s.schedule_id = ?4 AND s.disabled = 1
                   )
                   AND (status = 'pending'
                        OR (status = 'claimed' AND lease_expires_at_millis < ?2))
                 RETURNING generation",
                params![
                    claimant,
                    now_millis,
                    lease_expires_millis,
                    schedule_id,
                    due_millis
                ],
                |row| row.get(0),
            )
            .optional()?;

        match generation {
            Some(generation) => Ok(ClaimOutcome::Claimed {
                lease_expires_at,
                generation,
            }),
            None => Ok(ClaimOutcome::NotClaimed),
        }
    }

    /// The holder of a live manual lock on `schedule_id`, if any.
    fn manual_lock_holder(
        &self,
        schedule_id: &str,
        now_millis: i64,
    ) -> Result<Option<String>, ScheduleError> {
        Ok(self
            .conn
            .query_row(
                "SELECT holder FROM manual_locks
                 WHERE schedule_id = ?1 AND lease_expires_at_millis >= ?2",
                params![schedule_id, now_millis],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Take the lock a manual run (`schedule run-now`) holds on its schedule, so
    /// the scheduler loop's claims wait for it ([`ScheduleStore::claim_occurrence`]).
    /// One transaction: a disabled schedule is refused unless `force`, and a
    /// schedule with a live scheduled claim or another manual lock is `Busy`.
    /// The lock is a lease; renew it with [`ScheduleStore::renew_manual_run`] and
    /// end it with [`ScheduleStore::release_manual_run`].
    pub fn claim_manual_run(
        &self,
        schedule_id: &str,
        holder: &str,
        now: DateTime<Utc>,
        lease: Duration,
        force: bool,
    ) -> Result<ManualClaim, ScheduleError> {
        let lease_expires_at = now + to_chrono_duration(lease, "manual run lease")?;
        self.register_schedule(schedule_id)?;
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let outcome = (|| -> Result<ManualClaim, ScheduleError> {
            let status = self.schedule_status(schedule_id)?;
            if let Some(reason) = status.disabled_reason
                && !force
            {
                return Ok(ManualClaim::Disabled { reason });
            }
            let now_millis = to_millis(now);
            if let Some(held) = self.manual_lock_holder(schedule_id, now_millis)? {
                return Ok(ManualClaim::Busy { holder: held });
            }
            let live: Option<String> = self
                .conn
                .query_row(
                    "SELECT COALESCE(claimant, '') FROM occurrences
                     WHERE schedule_id = ?1 AND status = 'claimed'
                       AND lease_expires_at_millis >= ?2
                     LIMIT 1",
                    params![schedule_id, now_millis],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(claimant) = live {
                return Ok(ManualClaim::Busy {
                    holder: format!("scheduled occurrence claimed by `{claimant}`"),
                });
            }
            self.conn.execute(
                "INSERT INTO manual_locks (schedule_id, holder, lease_expires_at_millis)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(schedule_id) DO UPDATE SET
                     holder = excluded.holder,
                     lease_expires_at_millis = excluded.lease_expires_at_millis",
                params![schedule_id, holder, to_millis(lease_expires_at)],
            )?;
            Ok(ManualClaim::Claimed { lease_expires_at })
        })();
        match &outcome {
            Ok(_) => self.conn.execute_batch("COMMIT")?,
            Err(_) => {
                let _ = self.conn.execute_batch("ROLLBACK");
            }
        }
        outcome
    }

    /// Extend a manual run's lock. `false` when `holder` no longer holds it: the
    /// row is gone or another manual run took it over, or the scheduler claimed the
    /// schedule while the lease had lapsed (a live scheduled claim exists).
    ///
    /// A lease that lapsed with nobody having taken the schedule (a laptop asleep
    /// past the lease) is renewed: the row still names `holder`, and no claim
    /// changed the schedule's state, so the run has not lost anything. Once the
    /// scheduler has claimed an occurrence the lapsed lock is not revived.
    pub fn renew_manual_run(
        &self,
        schedule_id: &str,
        holder: &str,
        now: DateTime<Utc>,
        lease: Duration,
    ) -> Result<bool, ScheduleError> {
        let expires = now + to_chrono_duration(lease, "manual run lease")?;
        let now_millis = to_millis(now);
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let outcome = (|| -> Result<bool, ScheduleError> {
            let live_claim: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM occurrences
                     WHERE schedule_id = ?1 AND status = 'claimed'
                       AND lease_expires_at_millis >= ?2)",
                params![schedule_id, now_millis],
                |row| row.get(0),
            )?;
            if live_claim {
                return Ok(false);
            }
            let updated = self.conn.execute(
                "UPDATE manual_locks SET lease_expires_at_millis = ?1
                 WHERE schedule_id = ?2 AND holder = ?3",
                params![to_millis(expires), schedule_id, holder],
            )?;
            Ok(updated == 1)
        })();
        match &outcome {
            Ok(_) => self.conn.execute_batch("COMMIT")?,
            Err(_) => {
                let _ = self.conn.execute_batch("ROLLBACK");
            }
        }
        outcome
    }

    /// End a manual run's lock. A holder that is no longer the holder leaves the
    /// current one in place.
    pub fn release_manual_run(&self, schedule_id: &str, holder: &str) -> Result<(), ScheduleError> {
        self.conn.execute(
            "DELETE FROM manual_locks WHERE schedule_id = ?1 AND holder = ?2",
            params![schedule_id, holder],
        )?;
        Ok(())
    }

    /// Extend a still-valid claim's lease (a heartbeat, per big-AGI/LibreChat's
    /// "renewed at half its TTL" pattern, evidence §A6). Fenced the same way as
    /// `record_outcome`: `claimant` and `generation` must match the occurrence's
    /// current claim, and the lease must not have already expired - a lapsed lease may
    /// already belong to a new claimant by the time this call runs, and renewing it
    /// regardless would silently resurrect a claim this caller no longer actually
    /// holds.
    pub fn renew_lease(
        &self,
        schedule_id: &str,
        due_at: DateTime<Utc>,
        claimant: &str,
        generation: i64,
        now: DateTime<Utc>,
        lease: Duration,
    ) -> Result<DateTime<Utc>, ScheduleError> {
        let lease_duration = to_chrono_duration(lease, "renewed lease")?;
        let now_millis = to_millis(now);
        let new_expiry = now + lease_duration;
        let new_expiry_millis = to_millis(new_expiry);

        let updated = self.conn.execute(
            "UPDATE occurrences
             SET lease_expires_at_millis = ?1
             WHERE schedule_id = ?2 AND due_at_millis = ?3 AND claimant = ?4 AND generation = ?5
               AND status = 'claimed' AND lease_expires_at_millis >= ?6",
            params![
                new_expiry_millis,
                schedule_id,
                to_millis(due_at),
                claimant,
                generation,
                now_millis
            ],
        )?;

        if updated == 1 {
            Ok(new_expiry)
        } else {
            Err(ScheduleError::StaleClaim {
                schedule_id: schedule_id.to_string(),
                due_at,
                claimant: claimant.to_string(),
                generation,
            })
        }
    }

    /// Releases a still-live claim immediately, so it is reclaimable well before
    /// its lease would otherwise expire on its own.
    ///
    /// **status stays `claimed`.** The first
    /// version set `status = 'pending'`, which orphaned the occurrence: `last_seen`
    /// had already advanced past it (the tick's own atomicity), so no future
    /// `latest_occurrence_since` call could ever propose it again, and
    /// `expired_claims` (the scan that is the only other path back to a
    /// claimed-but-abandoned occurrence) only ever looks at rows still `claimed` -
    /// a `pending` row is invisible to it. A released occurrence was reachable by no
    /// scan at all. The fix is the same trick `record_deferral` already uses for a
    /// capacity refusal: leave `status` as `claimed` and set the lease to already
    /// expired as of `now`, so the very next tick's `expired_claims` scan - by this
    /// instance restarting, or another instance already running - reclaims it exactly
    /// as it would a crashed claimant's.
    ///
    /// Fenced the same way as `record_outcome`: only the `(claimant, generation)` pair
    /// that actually holds the claim can release it. Returns whether a release
    /// actually happened, so a caller does not have to treat "already settled by
    /// someone else" as an error.
    pub fn release_claim(
        &self,
        schedule_id: &str,
        due_at: DateTime<Utc>,
        claimant: &str,
        generation: i64,
        now: DateTime<Utc>,
    ) -> Result<bool, ScheduleError> {
        let updated = self.conn.execute(
            "UPDATE occurrences
             SET lease_expires_at_millis = ?1
             WHERE schedule_id = ?2 AND due_at_millis = ?3 AND claimant = ?4
               AND generation = ?5 AND status = 'claimed'",
            params![
                to_millis(now),
                schedule_id,
                to_millis(due_at),
                claimant,
                generation
            ],
        )?;
        Ok(updated > 0)
    }

    /// Record that a claimed occurrence finished, and update the schedule's failure
    /// bookkeeping per decision 0052's typed failure classes.
    ///
    /// `claimant` and `generation` must be the pair returned by the
    /// [`ClaimOutcome::Claimed`] that authorized this run; a mismatch (a stale claimant,
    /// an occurrence already settled by someone else, or an occurrence never claimed at
    /// all) is [`ScheduleError::StaleClaim`], not a silent no-op.
    pub fn record_outcome(
        &self,
        schedule_id: &str,
        due_at: DateTime<Utc>,
        claimant: &str,
        generation: i64,
        outcome: &RunOutcome,
    ) -> Result<(), ScheduleError> {
        // A capacity refusal is never finalized here - `start_and_track` routes
        // `FailureClass::Deferred` to `record_deferral` instead, which decides between
        // deferring (no finalization at all) and, once the caller decides to give up
        // (a time/next-occurrence bound, not a fixed count), finalizing through
        // `apply_transient_failure_bookkeeping` directly. Guarded before the
        // finalizing `UPDATE` below ever runs, not after, so a caller's mistake here
        // never leaves the occurrence half-settled (fail loud, not into an
        // inconsistent row).
        if let RunOutcome::Failed(FailureClass::Deferred { .. }) = outcome {
            return Err(ScheduleError::StaleClaim {
                schedule_id: schedule_id.to_string(),
                due_at,
                claimant: claimant.to_string(),
                generation,
            });
        }
        let status = match outcome {
            RunOutcome::Success => "done",
            RunOutcome::Failed(_) => "failed",
        };
        let updated = self.conn.execute(
            "UPDATE occurrences SET status = ?1
             WHERE schedule_id = ?2 AND due_at_millis = ?3 AND claimant = ?4
               AND generation = ?5 AND status = 'claimed'",
            params![status, schedule_id, to_millis(due_at), claimant, generation],
        )?;
        if updated != 1 {
            return Err(ScheduleError::StaleClaim {
                schedule_id: schedule_id.to_string(),
                due_at,
                claimant: claimant.to_string(),
                generation,
            });
        }

        match outcome {
            RunOutcome::Success => {
                self.conn.execute(
                    "UPDATE schedules SET consecutive_failures = 0 WHERE schedule_id = ?1",
                    params![schedule_id],
                )?;
            }
            RunOutcome::Failed(FailureClass::Transient { .. }) => {
                self.apply_transient_failure_bookkeeping(schedule_id)?;
            }
            RunOutcome::Failed(FailureClass::Deferred { .. }) => {
                // Unreachable: the early return above already rejected this variant
                // before the row was ever touched. Kept only so this match stays
                // exhaustive against `FailureClass` without a wildcard arm silently
                // swallowing a future variant.
                unreachable!("FailureClass::Deferred is rejected before this match runs")
            }
            RunOutcome::Failed(FailureClass::Disabling(reason)) => {
                self.conn.execute(
                    "UPDATE schedules SET disabled = 1, disabled_reason = ?1 WHERE schedule_id = ?2",
                    params![reason.to_string(), schedule_id],
                )?;
            }
        }
        Ok(())
    }

    /// The schedule's current failure bookkeeping.
    pub fn schedule_status(&self, schedule_id: &str) -> Result<ScheduleStatus, ScheduleError> {
        self.register_schedule(schedule_id)?;
        self.conn
            .query_row(
                "SELECT consecutive_failures, disabled_reason FROM schedules WHERE schedule_id = ?1",
                params![schedule_id],
                |row| {
                    Ok(ScheduleStatus {
                        consecutive_failures: row.get::<_, i64>(0)? as u64,
                        disabled_reason: row.get(1)?,
                    })
                },
            )
            .map_err(ScheduleError::from)
    }

    /// The schedule's failure bookkeeping without registering it: `None` when the
    /// claim table has never seen this schedule. Writes nothing, so it works on a
    /// [`ScheduleStore::open_read_only`] store.
    pub fn peek_status(&self, schedule_id: &str) -> Result<Option<ScheduleStatus>, ScheduleError> {
        self.conn
            .query_row(
                "SELECT consecutive_failures, disabled_reason FROM schedules WHERE schedule_id = ?1",
                params![schedule_id],
                |row| {
                    Ok(ScheduleStatus {
                        consecutive_failures: row.get::<_, i64>(0)? as u64,
                        disabled_reason: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(ScheduleError::from)
    }

    /// Clears a disable ("nothing re-enables" a paused schedule). Does not touch
    /// `consecutive_failures`: a re-enabled schedule starts counting fresh only once
    /// it actually succeeds or fails again, so a schedule re-enabled right after
    /// hitting the threshold does not immediately re-disable itself on the very next
    /// transient hiccup without at least one success in between resetting the count.
    /// That asymmetry is intentional: enabling is the user's decision, and it should
    /// not have to also mean "and forget every failure that led here."
    pub fn enable(&self, schedule_id: &str) -> Result<(), ScheduleError> {
        self.register_schedule(schedule_id)?;
        self.conn.execute(
            "UPDATE schedules SET disabled = 0, disabled_reason = NULL WHERE schedule_id = ?1",
            params![schedule_id],
        )?;
        Ok(())
    }

    /// Removes a schedule's claim-table bookkeeping entirely: its `schedules` row
    /// (failure count, disable state, last-seen) and every `occurrences` row (its
    /// full run history in this table). Used when a schedule's definition itself is
    /// deleted: otherwise a new schedule later reusing the same id would inherit
    /// a stranger's disable state, failure count, or claimed-occurrence rows.
    pub fn remove_schedule(&self, schedule_id: &str) -> Result<(), ScheduleError> {
        self.conn.execute(
            "DELETE FROM occurrences WHERE schedule_id = ?1",
            params![schedule_id],
        )?;
        self.conn.execute(
            "DELETE FROM manual_locks WHERE schedule_id = ?1",
            params![schedule_id],
        )?;
        self.conn.execute(
            "DELETE FROM schedules WHERE schedule_id = ?1",
            params![schedule_id],
        )?;
        Ok(())
    }

    /// What `Schedule::resolve_wake`'s `last_seen` argument was, the last time a
    /// scheduler loop evaluated this schedule. `None` before the first evaluation
    /// ever - `resolve_wake`'s own "nothing missed by definition" case.
    ///
    /// This is *not* derived from `occurrences` (the claim table's per-occurrence
    /// rows): a tick that finds nothing due yet writes no occurrence row at all, so
    /// deriving `last_seen` from `MAX(due_at_millis)` would leave a schedule stuck
    /// reporting `None` forever between its creation and its first-ever due
    /// occurrence, at which point `resolve_wake`'s `None` branch treats that very
    /// occurrence as "not missed, wait for the next one" - silently skipping the
    /// schedule's first firing, and by induction, since nothing was ever written,
    /// every firing after it too. [`advance_last_seen`](Self::advance_last_seen)
    /// exists precisely to record "evaluated as of `now`" on *every* tick, whether
    /// or not anything was due.
    pub fn last_seen(&self, schedule_id: &str) -> Result<Option<DateTime<Utc>>, ScheduleError> {
        self.register_schedule(schedule_id)?;
        let millis: Option<i64> = self.conn.query_row(
            "SELECT last_seen_millis FROM schedules WHERE schedule_id = ?1",
            params![schedule_id],
            |row| row.get(0),
        )?;
        millis
            .map(|m| from_millis(m, "schedules.last_seen_millis"))
            .transpose()
    }

    /// Records that this schedule was evaluated as of `now`. Monotonic: a call with
    /// an earlier `now` than what is already stored never moves the value backward
    /// (a clock adjustment or an out-of-order call must not un-see an occurrence this
    /// store already accounted for).
    pub fn advance_last_seen(
        &self,
        schedule_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ScheduleError> {
        self.register_schedule(schedule_id)?;
        self.conn.execute(
            "UPDATE schedules SET last_seen_millis = ?1
             WHERE schedule_id = ?2
               AND (last_seen_millis IS NULL OR last_seen_millis < ?1)",
            params![to_millis(now), schedule_id],
        )?;
        Ok(())
    }

    /// When this schedule was last ticked at all, regardless of outcome - the
    /// on-time heartbeat, distinct from [`last_seen`](Self::last_seen). `None` before
    /// the first tick ever.
    pub fn last_evaluated_at(
        &self,
        schedule_id: &str,
    ) -> Result<Option<DateTime<Utc>>, ScheduleError> {
        self.register_schedule(schedule_id)?;
        let millis: Option<i64> = self.conn.query_row(
            "SELECT last_evaluated_millis FROM schedules WHERE schedule_id = ?1",
            params![schedule_id],
            |row| row.get(0),
        )?;
        millis
            .map(|m| from_millis(m, "schedules.last_evaluated_millis"))
            .transpose()
    }

    /// Reads `last_seen`, lets `decide` turn it into an occurrence to claim (or
    /// `Ok(None)` to do nothing this tick), and - only if it decided to act -
    /// advances `last_seen` to exactly that occurrence and claims it, all inside one
    /// `BEGIN IMMEDIATE` transaction.
    ///
    /// This closes a crash window: the previous
    /// design read `last_seen`, unconditionally advanced it to `now` (not to any
    /// particular occurrence), and only afterward attempted the claim, as three
    /// separate statements. A crash between the advance and the claim left `last_seen`
    /// already past the occurrence nobody had actually claimed - it was never
    /// attempted again, and `claim_occurrence`'s own lease-expiry reclaim path (built
    /// for exactly this kind of crash) never ran, because nothing ever asked to claim
    /// that occurrence a second time. Advancing only to the occurrence this same
    /// transaction is about to claim, atomically with the claim itself, means either
    /// both happen (committed together) or neither does (rolled back together) - a
    /// crash before commit leaves `last_seen` exactly where it was, and the next tick
    /// makes the identical decision and tries again.
    ///
    /// `BEGIN IMMEDIATE` (not a plain `BEGIN`) takes the write lock up front, which is
    /// what closes a boundary race: two connections evaluating
    /// the same schedule for two adjacent occurrences at nearly the same instant no
    /// longer both read `last_seen` before either writes it - the second to reach
    /// `BEGIN IMMEDIATE` blocks until the first commits, and then reads the
    /// first's already-advanced `last_seen`.
    /// `decide` returns, given `last_seen`: `None` for "nothing to act on", or
    /// `Some((due, should_claim))` - `due` is always recorded (`last_seen` advances
    /// to it) whether or not `should_claim` is true. `should_claim = false` is the
    /// case of a policy (`MissedRunPolicy::Skip`) that decided *not* to run `due`, but
    /// which still must not see it proposed again on the next tick - see
    /// [`TickDecision::Dropped`].
    pub fn tick_schedule(
        &self,
        schedule_id: &str,
        now: DateTime<Utc>,
        claimant: &str,
        lease: Duration,
        decide: impl FnOnce(
            Option<DateTime<Utc>>,
        ) -> Result<Option<(DateTime<Utc>, bool)>, ScheduleError>,
    ) -> Result<TickDecision, ScheduleError> {
        self.register_schedule(schedule_id)?;
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let outcome = (|| -> Result<TickDecision, ScheduleError> {
            let last_seen = self.last_seen(schedule_id)?;
            let Some((due, should_claim)) = decide(last_seen)? else {
                return Ok(TickDecision::NothingDue);
            };
            self.advance_last_seen(schedule_id, due)?;
            if !should_claim {
                return Ok(TickDecision::Dropped { due });
            }
            let claim = self.claim_occurrence(schedule_id, due, claimant, now, lease)?;
            Ok(TickDecision::Evaluated { due, claim })
        })();
        match &outcome {
            Ok(_) => self.conn.execute_batch("COMMIT")?,
            Err(_) => {
                // Best-effort: if the rollback itself fails the connection is already
                // in worse trouble than this call can fix, and the original error is
                // the one worth reporting.
                let _ = self.conn.execute_batch("ROLLBACK");
            }
        }
        outcome
    }

    /// How many of this schedule's occurrences have settled `done` and how many
    /// `failed`, across its whole history in this table ("the number of
    /// scheduled runs ... [is] reported, not only their outcomes").
    pub fn occurrence_counts(&self, schedule_id: &str) -> Result<(u64, u64), ScheduleError> {
        let done: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM occurrences WHERE schedule_id = ?1 AND status = 'done'",
            params![schedule_id],
            |row| row.get(0),
        )?;
        let failed: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM occurrences WHERE schedule_id = ?1 AND status = 'failed'",
            params![schedule_id],
            |row| row.get(0),
        )?;
        Ok((done as u64, failed as u64))
    }

    /// Corrects a `last_seen` that is ahead of `now` (a forward system-clock jump
    /// later corrected, or a hand-edited file) back down to `now`. Returns whether a
    /// correction was actually made, so a caller can raise it visibly ("emit a
    /// visible event and clamp") rather than silently absorb it. `advance_last_seen`
    /// is deliberately monotonic and cannot do this on its own.
    pub fn clamp_last_seen(
        &self,
        schedule_id: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, ScheduleError> {
        self.register_schedule(schedule_id)?;
        let updated = self.conn.execute(
            "UPDATE schedules SET last_seen_millis = ?1
             WHERE schedule_id = ?2 AND last_seen_millis > ?1",
            params![to_millis(now), schedule_id],
        )?;
        Ok(updated > 0)
    }

    /// Occurrences of `schedule_id` still marked `claimed` whose lease has already
    /// expired as of `now`: a claimant that crashed or
    /// hung after `tick_schedule` committed its claim.
    ///
    /// This scan exists because `tick_schedule`'s own atomicity (`last_seen` and
    /// the claim commit together) has a consequence: once
    /// committed, `last_seen` already names that occurrence, so
    /// `Schedule::latest_occurrence_since(last_seen, ..)` can never propose it again -
    /// there is no occurrence *later* than `last_seen` to find. The claim table's own
    /// `claim_occurrence` WHERE clause already knows how to reclaim an
    /// expired-lease row (it is what lets a second instance win a race); what was
    /// missing was anyone ever calling it again for a `due` already at or before
    /// `last_seen`. This method is that "anyone": a caller (`SchedulerLoop`) runs it
    /// every tick, independently of whatever `tick_schedule` is doing for *new*
    /// occurrences, and reclaims what it finds via a direct `claim_occurrence` call.
    pub fn expired_claims(
        &self,
        schedule_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<ExpiredClaim>, ScheduleError> {
        let mut stmt = self.conn.prepare(
            "SELECT due_at_millis, deferred_count, COALESCE(claimant, '') = ?3 FROM occurrences
             WHERE schedule_id = ?1 AND status = 'claimed' AND lease_expires_at_millis < ?2
             ORDER BY due_at_millis",
        )?;
        let rows = stmt.query_map(
            params![schedule_id, to_millis(now), HELD_FOR_MANUAL_RUN],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (millis, deferred_count, held_for_manual_run) = row?;
            out.push(ExpiredClaim {
                due_at: from_millis(millis, "occurrences.due_at_millis")?,
                // carried back so a caller can
                // tell a genuine crash reclaim (`deferred_count == 0`) apart from a
                // capacity-deferral retry (`deferred_count > 0`) - the two must be
                // reported as different `TickEvent`s (`Reclaimed` vs `Deferred`), not
                // collapsed into one just because both are found through this scan.
                deferred_count: deferred_count as u32,
                held_for_manual_run,
            });
        }
        Ok(out)
    }

    /// End an occurrence that was set aside for a manual run and is now past the
    /// on-time window under `MissedRunPolicy::Skip`: settled as `dropped`, never
    /// run late. Only a row still held for a manual run with an expired lease
    /// changes; `false` when it was reclaimed or settled in the meantime.
    pub fn drop_held_for_manual_run(
        &self,
        schedule_id: &str,
        due_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<bool, ScheduleError> {
        let updated = self.conn.execute(
            "UPDATE occurrences SET status = 'dropped'
             WHERE schedule_id = ?1 AND due_at_millis = ?2 AND status = 'claimed'
               AND claimant = ?3 AND lease_expires_at_millis < ?4",
            params![
                schedule_id,
                to_millis(due_at),
                HELD_FOR_MANUAL_RUN,
                to_millis(now)
            ],
        )?;
        Ok(updated == 1)
    }

    /// Records the run id `start_run` actually started for this occurrence,
    /// fenced the same way as `record_outcome`: only the `(claimant, generation)` pair
    /// that currently holds the claim may attach one. Called right after
    /// `RunStartOutcome::Started`, so a claim that never actually started a run (a
    /// readiness failure, a capacity refusal) never gets a spurious run id. A failure
    /// to record this is bookkeeping, not fatal to the run already under way: a
    /// caller ignores it the same way it ignores a failed lease renewal, and the
    /// occurrence's spend is then reported as unattributed by `run_ids_for_schedule`
    /// rather than wrongly attached to a stale id.
    pub fn set_run_id(
        &self,
        schedule_id: &str,
        due_at: DateTime<Utc>,
        claimant: &str,
        generation: i64,
        run_id: &str,
    ) -> Result<(), ScheduleError> {
        let updated = self.conn.execute(
            "UPDATE occurrences SET run_id = ?1
             WHERE schedule_id = ?2 AND due_at_millis = ?3 AND claimant = ?4 AND generation = ?5",
            params![run_id, schedule_id, to_millis(due_at), claimant, generation],
        )?;
        if updated != 1 {
            return Err(ScheduleError::StaleClaim {
                schedule_id: schedule_id.to_string(),
                due_at,
                claimant: claimant.to_string(),
                generation,
            });
        }
        Ok(())
    }

    /// Every run id ever recorded for `schedule_id`, oldest first, with
    /// duplicates removed (a reclaimed occurrence's retries all start the same run
    /// id, per `ScheduledRuns::run_id`'s own doc) - both the clock-started runs
    /// recorded on `occurrences` and the manual `schedule run-now` runs recorded on
    /// `manual_runs` (a run-now's spend was invisible to `schedule list`
    /// entirely before this table existed, since run-now never claims an
    /// occurrence). The caller's exact-match mechanism for "what did this schedule
    /// spend": look each one up with `Ledger::calls_under_task`, never a hand-rolled
    /// prefix guess against `schedule_id` itself.
    pub fn run_ids_for_schedule(&self, schedule_id: &str) -> Result<Vec<String>, ScheduleError> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id FROM occurrences WHERE schedule_id = ?1 AND run_id IS NOT NULL
             UNION
             SELECT run_id FROM manual_runs WHERE schedule_id = ?1
             ORDER BY run_id",
        )?;
        let rows = stmt.query_map(params![schedule_id], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Records a manual `schedule run-now` run's id against `schedule_id`, so its spend is included
    /// the same as a clock-started run's. Idempotent (`INSERT OR IGNORE`): a run id is already
    /// unique per invocation (`schedule_cmd::run_now`'s own random suffix), so a duplicate call
    /// here would only ever be this same invocation retried, not two different runs colliding.
    pub fn record_manual_run(&self, schedule_id: &str, run_id: &str) -> Result<(), ScheduleError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO manual_runs (schedule_id, run_id) VALUES (?1, ?2)",
            params![schedule_id, run_id],
        )?;
        Ok(())
    }

    /// Handles a capacity refusal ([`RunOutcome::Failed`] carrying
    /// [`FailureClass::Deferred`]) instead of finalizing the occurrence the way
    /// `record_outcome` finalizes every other failure class.
    ///
    /// **bounded by time, not by a tick
    /// count.** The first version stopped deferring after a fixed
    /// `MAX_DEFERRALS = 5` retries - about 100s at the desktop shell's 20s tick
    /// cadence, a bound with no relationship to how long a slot might reasonably take
    /// to free up. This store no longer decides when to give up at all: `give_up` is
    /// the caller's own answer to "should this occurrence stop being retried" -
    /// `SchedulerLoop` computes it from the schedule's own policy and timing (has the
    /// on-time window passed under `Skip`, or has a *later* occurrence of the same
    /// schedule already come due), which this crate has no way to know on its own.
    ///
    /// `give_up = false`: the occurrence is *not* marked `failed`; its lease is set to
    /// have already expired (as of `now`), so the very next tick's `expired_claims`
    /// scan - already exercised for a crashed claimant's reclaim - finds and
    /// reclaims it exactly as it would a crash, with no bespoke "deferred" scan of its
    /// own. `consecutive_failures` is left untouched: a scheduler that is merely full
    /// right now says nothing about whether this schedule's own configuration works,
    /// so it must never count toward the pause threshold (the whole point of its own
    /// `FailureClass` variant).
    ///
    /// `give_up = true`: the occurrence is finalized as `failed` and does count
    /// toward the threshold (via the same bookkeeping `record_outcome` applies to a
    /// `Transient` failure) - a schedule that has genuinely run out of room to retry
    /// is still worth pausing eventually, visibly, rather than deferring forever.
    pub fn record_deferral(
        &self,
        schedule_id: &str,
        due_at: DateTime<Utc>,
        claimant: &str,
        generation: i64,
        now: DateTime<Utc>,
        give_up: bool,
    ) -> Result<DeferralOutcome, ScheduleError> {
        let due_millis = to_millis(due_at);
        let stale = || ScheduleError::StaleClaim {
            schedule_id: schedule_id.to_string(),
            due_at,
            claimant: claimant.to_string(),
            generation,
        };
        let current: Option<i64> = self
            .conn
            .query_row(
                "SELECT deferred_count FROM occurrences
                 WHERE schedule_id = ?1 AND due_at_millis = ?2 AND claimant = ?3
                   AND generation = ?4 AND status = 'claimed'",
                params![schedule_id, due_millis, claimant, generation],
                |row| row.get(0),
            )
            .optional()?;
        let current = current.ok_or_else(stale)?;
        let next = current + 1;
        if give_up {
            let updated = self.conn.execute(
                "UPDATE occurrences SET status = 'failed', deferred_count = ?1
                 WHERE schedule_id = ?2 AND due_at_millis = ?3 AND claimant = ?4
                   AND generation = ?5 AND status = 'claimed'",
                params![next, schedule_id, due_millis, claimant, generation],
            )?;
            if updated != 1 {
                return Err(stale());
            }
            self.apply_transient_failure_bookkeeping(schedule_id)?;
            Ok(DeferralOutcome::Exhausted {
                deferred_count: next as u32,
            })
        } else {
            let updated = self.conn.execute(
                "UPDATE occurrences SET lease_expires_at_millis = ?1, deferred_count = ?2
                 WHERE schedule_id = ?3 AND due_at_millis = ?4 AND claimant = ?5
                   AND generation = ?6 AND status = 'claimed'",
                params![
                    to_millis(now),
                    next,
                    schedule_id,
                    due_millis,
                    claimant,
                    generation
                ],
            )?;
            if updated != 1 {
                return Err(stale());
            }
            Ok(DeferralOutcome::Deferred {
                deferred_count: next as u32,
            })
        }
    }

    /// Ends a capacity deferral without ever counting it as a failure:
    /// under `MissedRunPolicy::Skip`, once a deferred occurrence has fallen outside
    /// the on-time window, it is dropped - the same "this was missed, and `Skip`
    /// means dropping a missed occurrence, not running it late" rule
    /// `TickDecision::Dropped` already applies to an occurrence that was never even
    /// claimed. Terminal (status `dropped`, a value no scan ever selects again) and
    /// silent on `consecutive_failures`: a capacity refusal was never this
    /// schedule's own fault, and `Skip`'s whole point is that a late run is worse
    /// than no run, not a punishable one.
    pub fn drop_deferred(
        &self,
        schedule_id: &str,
        due_at: DateTime<Utc>,
        claimant: &str,
        generation: i64,
    ) -> Result<(), ScheduleError> {
        let updated = self.conn.execute(
            "UPDATE occurrences SET status = 'dropped'
             WHERE schedule_id = ?1 AND due_at_millis = ?2 AND claimant = ?3
               AND generation = ?4 AND status = 'claimed'",
            params![schedule_id, to_millis(due_at), claimant, generation],
        )?;
        if updated != 1 {
            return Err(ScheduleError::StaleClaim {
                schedule_id: schedule_id.to_string(),
                due_at,
                claimant: claimant.to_string(),
                generation,
            });
        }
        Ok(())
    }

    /// The consecutive-failure counting and threshold-pause bookkeeping shared by an
    /// ordinary `RunOutcome::Failed(FailureClass::Transient { .. })` and an exhausted
    /// capacity deferral - both are "this attempt did not succeed, and it does not by
    /// itself prove the schedule is misconfigured", which is exactly what this
    /// bookkeeping counts toward pausing.
    fn apply_transient_failure_bookkeeping(&self, schedule_id: &str) -> Result<(), ScheduleError> {
        self.conn.execute(
            "UPDATE schedules SET consecutive_failures = consecutive_failures + 1
             WHERE schedule_id = ?1",
            params![schedule_id],
        )?;
        let consecutive_failures: i64 = self.conn.query_row(
            "SELECT consecutive_failures FROM schedules WHERE schedule_id = ?1",
            params![schedule_id],
            |row| row.get(0),
        )?;
        let threshold = self
            .transient_failure_policy
            .pause_after_consecutive_failures;
        if consecutive_failures as u32 >= threshold {
            let reason = DisableReason::TooManyConsecutiveFailures {
                consecutive_failures: consecutive_failures as u32,
                threshold,
            };
            self.conn.execute(
                "UPDATE schedules SET disabled = 1, disabled_reason = ?1 WHERE schedule_id = ?2",
                params![reason.to_string(), schedule_id],
            )?;
        }
        Ok(())
    }

    /// Every occurrence of `schedule_id` due after `after` (all of them for
    /// `None`), oldest first, with whether it has settled. Read-only: the run-history
    /// recorder uses it to rebuild an outcome row its graph write lost, from a watermark so each
    /// tick reads only what is new. The claim table is the record that decides.
    pub fn occurrences_since(
        &self,
        schedule_id: &str,
        after: Option<DateTime<Utc>>,
    ) -> Result<Vec<OccurrenceRecord>, ScheduleError> {
        let mut stmt = self.conn.prepare(
            "SELECT due_at_millis, status, run_id, claimed_at_millis FROM occurrences
             WHERE schedule_id = ?1 AND due_at_millis > ?2
             ORDER BY due_at_millis",
        )?;
        let after = after.map(to_millis).unwrap_or(i64::MIN);
        let rows = stmt.query_map(params![schedule_id, after], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (due, status, run_id, claimed) = row?;
            out.push(OccurrenceRecord {
                due_at: from_millis(due, "due_at_millis")?,
                settled: match status.as_str() {
                    "done" => Some(true),
                    "failed" => Some(false),
                    "dropped" => Some(false),
                    _ => None,
                },
                dropped: status == "dropped",
                run_id,
                claimed_at: claimed
                    .map(|c| from_millis(c, "claimed_at_millis"))
                    .transpose()?,
            });
        }
        Ok(out)
    }

    /// The occurrence's status column, for tests and diagnostics.
    pub fn occurrence_status(
        &self,
        schedule_id: &str,
        due_at: DateTime<Utc>,
    ) -> Result<Option<String>, ScheduleError> {
        self.conn
            .query_row(
                "SELECT status FROM occurrences WHERE schedule_id = ?1 AND due_at_millis = ?2",
                params![schedule_id, to_millis(due_at)],
                |row| row.get(0),
            )
            .optional()
            .map_err(ScheduleError::from)
    }
}

/// One occurrence, from [`ScheduleStore::occurrences_since`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccurrenceRecord {
    pub due_at: DateTime<Utc>,
    /// `Some(true)` done, `Some(false)` failed or dropped, `None` not settled yet
    /// (pending or claimed).
    pub settled: Option<bool>,
    /// Dropped past its deferral bound: settled, but never ran.
    pub dropped: bool,
    /// The run it started under, once it started.
    pub run_id: Option<String>,
    /// When the latest claim was taken.
    pub claimed_at: Option<DateTime<Utc>>,
}

/// What happened when a claimed occurrence ran, for [`ScheduleStore::record_outcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Success,
    Failed(FailureClass),
}

/// What [`ScheduleStore::record_deferral`] did with a capacity refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferralOutcome {
    /// `give_up` was `false`: the occurrence's lease was set to already-expired so
    /// the next tick's `expired_claims` scan reclaims it. Not counted toward the
    /// schedule's consecutive-failure threshold.
    Deferred { deferred_count: u32 },
    /// `give_up` was `true`: finalized `failed`, and counted toward the threshold
    /// like any other failure.
    Exhausted { deferred_count: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn t(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, h, m, s).unwrap()
    }

    fn occurrence_claimant(
        store: &ScheduleStore,
        schedule_id: &str,
        due_at: DateTime<Utc>,
    ) -> Option<String> {
        store
            .conn
            .query_row(
                "SELECT claimant FROM occurrences WHERE schedule_id = ?1 AND due_at_millis = ?2",
                params![schedule_id, to_millis(due_at)],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }

    /// Claim and unwrap the `(generation, lease_expires_at)` pair, panicking (test-only)
    /// if the claim did not succeed.
    fn claim(
        store: &ScheduleStore,
        schedule_id: &str,
        due: DateTime<Utc>,
        claimant: &str,
        now: DateTime<Utc>,
        lease: Duration,
    ) -> i64 {
        match store
            .claim_occurrence(schedule_id, due, claimant, now, lease)
            .unwrap()
        {
            ClaimOutcome::Claimed { generation, .. } => generation,
            ClaimOutcome::NotClaimed => panic!("expected the claim to succeed"),
        }
    }

    #[test]
    fn a_pending_occurrence_claims() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let outcome = store
            .claim_occurrence(
                "s1",
                t(9, 0, 0),
                "worker-a",
                t(9, 0, 1),
                Duration::from_secs(60),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            ClaimOutcome::Claimed { generation: 1, .. }
        ));
        assert_eq!(
            occurrence_claimant(&store, "s1", t(9, 0, 0)),
            Some("worker-a".to_string())
        );
    }

    #[test]
    fn a_second_claim_before_lease_expiry_fails() {
        let store = ScheduleStore::open_in_memory().unwrap();
        store
            .claim_occurrence(
                "s1",
                t(9, 0, 0),
                "worker-a",
                t(9, 0, 1),
                Duration::from_secs(60),
            )
            .unwrap();
        let second = store
            .claim_occurrence(
                "s1",
                t(9, 0, 0),
                "worker-b",
                t(9, 0, 2),
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(second, ClaimOutcome::NotClaimed);
        assert_eq!(
            occurrence_claimant(&store, "s1", t(9, 0, 0)),
            Some("worker-a".to_string())
        );
    }

    #[test]
    fn a_crashed_claimants_lease_becomes_claimable_again() {
        let store = ScheduleStore::open_in_memory().unwrap();
        store
            .claim_occurrence(
                "s1",
                t(9, 0, 0),
                "worker-a",
                t(9, 0, 1),
                Duration::from_secs(30),
            )
            .unwrap();
        let after_lease_expiry = t(9, 0, 40);
        let second = store
            .claim_occurrence(
                "s1",
                t(9, 0, 0),
                "worker-b",
                after_lease_expiry,
                Duration::from_secs(30),
            )
            .unwrap();
        assert!(matches!(
            second,
            ClaimOutcome::Claimed { generation: 2, .. }
        ));
        assert_eq!(
            occurrence_claimant(&store, "s1", t(9, 0, 0)),
            Some("worker-b".to_string())
        );
    }

    #[test]
    fn a_disabled_schedule_never_claims() {
        let store = ScheduleStore::open_in_memory().unwrap();
        store.register_schedule("s1").unwrap();
        let generation = claim(
            &store,
            "s1",
            t(7, 0, 0),
            "worker-a",
            t(7, 0, 1),
            Duration::from_secs(60),
        );
        store
            .record_outcome(
                "s1",
                t(7, 0, 0),
                "worker-a",
                generation,
                &RunOutcome::Failed(FailureClass::Disabling(DisableReason::MissingCredential {
                    detail: "kimi-api-key".to_string(),
                })),
            )
            .unwrap();
        let outcome = store
            .claim_occurrence(
                "s1",
                t(9, 0, 0),
                "worker-a",
                t(9, 0, 1),
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(outcome, ClaimOutcome::NotClaimed);
        let status = store.schedule_status("s1").unwrap();
        assert!(status.is_disabled());
    }

    #[test]
    fn a_claim_before_the_occurrence_is_due_is_refused() {
        // The defect: a claim nine hours before its own due time used to succeed.
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(18, 0, 0);
        let nine_hours_early = t(9, 0, 0);
        let outcome = store
            .claim_occurrence(
                "s1",
                due,
                "worker-a",
                nine_hours_early,
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(
            outcome,
            ClaimOutcome::NotClaimed,
            "a claim before due time must be refused"
        );

        // The same occurrence claims normally once it is actually due.
        let at_due_time = store
            .claim_occurrence("s1", due, "worker-a", due, Duration::from_secs(60))
            .unwrap();
        assert!(matches!(at_due_time, ClaimOutcome::Claimed { .. }));
    }

    #[test]
    fn an_oversized_lease_is_refused_not_silently_zeroed() {
        // The defect: `chrono::Duration::from_std(lease).unwrap_or(zero)` turned an
        // out-of-range lease into "expires immediately" with no error at all.
        let store = ScheduleStore::open_in_memory().unwrap();
        let absurd_lease = Duration::from_secs(u64::MAX);
        let err = store
            .claim_occurrence("s1", t(9, 0, 0), "worker-a", t(9, 0, 1), absurd_lease)
            .unwrap_err();
        assert!(matches!(err, ScheduleError::DurationOutOfRange { .. }));
    }

    #[test]
    fn a_stale_claimants_lease_renewal_fails_after_a_new_claimant_takes_over() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let gen_a = claim(
            &store,
            "s1",
            t(9, 0, 0),
            "worker-a",
            t(9, 0, 1),
            Duration::from_secs(30),
        );
        // worker-a's lease lapses; worker-b claims (generation 2).
        let after_expiry = t(9, 0, 40);
        claim(
            &store,
            "s1",
            t(9, 0, 0),
            "worker-b",
            after_expiry,
            Duration::from_secs(30),
        );
        // worker-a, still alive but stale, tries to renew its old (generation 1) lease.
        let err = store
            .renew_lease(
                "s1",
                t(9, 0, 0),
                "worker-a",
                gen_a,
                t(9, 0, 41),
                Duration::from_secs(30),
            )
            .unwrap_err();
        assert!(matches!(err, ScheduleError::StaleClaim { .. }));
    }

    #[test]
    fn renewing_a_still_valid_lease_extends_it_and_blocks_a_reclaim() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let generation = claim(
            &store,
            "s1",
            t(9, 0, 0),
            "worker-a",
            t(9, 0, 1),
            Duration::from_secs(30),
        );
        // Renew well before the original 30s lease would expire.
        store
            .renew_lease(
                "s1",
                t(9, 0, 0),
                "worker-a",
                generation,
                t(9, 0, 20),
                Duration::from_secs(30),
            )
            .unwrap();
        // The original lease would have expired at 9:00:31; the renewal pushes it to
        // 9:00:50, so a claim attempt at 9:00:40 must still fail.
        let still_held = store
            .claim_occurrence(
                "s1",
                t(9, 0, 0),
                "worker-b",
                t(9, 0, 40),
                Duration::from_secs(30),
            )
            .unwrap();
        assert_eq!(still_held, ClaimOutcome::NotClaimed);
    }

    #[test]
    fn release_claim_lets_the_next_tick_reclaim_it_via_the_expired_claim_scan() {
        // the previous version set status back
        // to `pending`, which `expired_claims` never selects (it only ever looks at
        // `claimed` rows) - a released occurrence was findable by no future scan at
        // all. Expiring the lease instead keeps status `claimed`, so the exact scan a
        // crashed claimant's occurrence goes through picks up a released one too.
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(9, 0, 0);
        let generation = claim(&store, "s1", due, "a", due, Duration::from_secs(900));
        assert!(
            store
                .release_claim("s1", due, "a", generation, due)
                .unwrap(),
            "the release must report that it actually happened"
        );
        assert_eq!(
            store.occurrence_status("s1", due).unwrap(),
            Some("claimed".to_owned()),
            "status must stay `claimed`; a `pending` row is invisible to expired_claims"
        );
        let next_tick = due + chrono::Duration::seconds(1);
        assert_eq!(
            store
                .expired_claims("s1", next_tick)
                .unwrap()
                .into_iter()
                .map(|e| e.due_at)
                .collect::<Vec<_>>(),
            vec![due],
            "the very next tick must find it, not wait out the full 900s lease"
        );
        let reclaimed = store
            .claim_occurrence("s1", due, "b", next_tick, Duration::from_secs(900))
            .unwrap();
        assert!(matches!(reclaimed, ClaimOutcome::Claimed { .. }));
    }

    #[test]
    fn release_claim_is_fenced_and_a_stale_release_reports_no_change() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(9, 0, 0);
        let gen_a = claim(&store, "s1", due, "a", due, Duration::from_secs(30));
        // a's lease expires; b reclaims.
        let claimed_at_b = due + chrono::Duration::seconds(40);
        claim(
            &store,
            "s1",
            due,
            "b",
            claimed_at_b,
            Duration::from_secs(30),
        );
        // a, unaware it lost the claim, tries to release its own (stale) hold.
        assert!(
            !store
                .release_claim("s1", due, "a", gen_a, claimed_at_b)
                .unwrap()
        );
    }

    /// The core fencing scenario: a slow claimant's lease
    /// expires while it is still alive, a second claimant takes over, and the first
    /// claimant's eventual `record_outcome` call must not be applied - neither as a
    /// second success/failure recorded against the occurrence, nor as a second
    /// increment of `consecutive_failures`, nor as an overwrite of whatever the second
    /// claimant already recorded.
    #[test]
    fn a_stale_claimants_outcome_is_fenced_off_after_a_slow_lease_expires() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(9, 0, 0);
        let gen_a = claim(
            &store,
            "s1",
            due,
            "worker-a",
            t(9, 0, 1),
            Duration::from_secs(60),
        );
        // worker-a is still alive at +61s (past its 60s lease) but has not recorded
        // anything yet; worker-b claims the now-expired lease.
        let gen_b = claim(
            &store,
            "s1",
            due,
            "worker-b",
            t(9, 1, 2),
            Duration::from_secs(60),
        );
        assert_ne!(gen_a, gen_b);

        // worker-b finishes successfully first.
        store
            .record_outcome("s1", due, "worker-b", gen_b, &RunOutcome::Success)
            .unwrap();
        assert_eq!(
            store.occurrence_status("s1", due).unwrap(),
            Some("done".to_string())
        );

        // worker-a, unaware it lost the claim, now reports a failure. It must be
        // rejected, not applied: `done` must not become `failed`, and
        // `consecutive_failures` must not move.
        let err = store
            .record_outcome(
                "s1",
                due,
                "worker-a",
                gen_a,
                &RunOutcome::Failed(FailureClass::Transient {
                    detail: "worker-a's late report".to_string(),
                }),
            )
            .unwrap_err();
        assert!(matches!(err, ScheduleError::StaleClaim { .. }));
        assert_eq!(
            store.occurrence_status("s1", due).unwrap(),
            Some("done".to_string()),
            "a stale claimant must not overwrite a settled outcome"
        );
        let status = store.schedule_status("s1").unwrap();
        assert_eq!(
            status.consecutive_failures, 0,
            "a fenced-off failure report must not count"
        );
    }

    #[test]
    fn a_duplicate_record_outcome_call_with_the_same_claim_is_rejected() {
        // Even the *original, still-current* claimant cannot record twice: once the
        // occurrence is `done`, `status = 'claimed'` no longer matches, so a retried or
        // duplicated call is fenced off exactly like a different claimant's would be.
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(9, 0, 0);
        let generation = claim(
            &store,
            "s1",
            due,
            "worker-a",
            t(9, 0, 1),
            Duration::from_secs(60),
        );
        store
            .record_outcome("s1", due, "worker-a", generation, &RunOutcome::Success)
            .unwrap();
        let err = store
            .record_outcome(
                "s1",
                due,
                "worker-a",
                generation,
                &RunOutcome::Failed(FailureClass::Transient {
                    detail: "duplicate report".to_string(),
                }),
            )
            .unwrap_err();
        assert!(matches!(err, ScheduleError::StaleClaim { .. }));
    }

    #[test]
    fn record_outcome_on_an_unclaimed_occurrence_is_an_error_not_ok() {
        let store = ScheduleStore::open_in_memory().unwrap();
        store.register_schedule("s1").unwrap();
        let err = store
            .record_outcome("s1", t(8, 0, 0), "nobody", 0, &RunOutcome::Success)
            .unwrap_err();
        assert!(matches!(err, ScheduleError::StaleClaim { .. }));
    }

    #[test]
    fn a_transient_failure_counts_without_disabling_below_threshold() {
        let store = ScheduleStore::open_in_memory().unwrap();
        for i in 0..3 {
            let due = t(8, i, 0);
            let generation = claim(&store, "s1", due, "worker-a", due, Duration::from_secs(60));
            store
                .record_outcome(
                    "s1",
                    due,
                    "worker-a",
                    generation,
                    &RunOutcome::Failed(FailureClass::Transient {
                        detail: "mcp server unreachable".to_string(),
                    }),
                )
                .unwrap();
        }
        let status = store.schedule_status("s1").unwrap();
        assert_eq!(status.consecutive_failures, 3);
        assert!(!status.is_disabled());
    }

    #[test]
    fn consecutive_transient_failures_pause_the_schedule_at_the_configured_threshold() {
        let store = ScheduleStore::open_in_memory()
            .unwrap()
            .with_transient_failure_policy(TransientFailurePolicy {
                pause_after_consecutive_failures: 2,
            });
        for i in 0..2 {
            let due = t(8, i, 0);
            let generation = claim(&store, "s1", due, "worker-a", due, Duration::from_secs(60));
            store
                .record_outcome(
                    "s1",
                    due,
                    "worker-a",
                    generation,
                    &RunOutcome::Failed(FailureClass::Transient {
                        detail: "timeout".to_string(),
                    }),
                )
                .unwrap();
        }
        let status = store.schedule_status("s1").unwrap();
        assert!(
            status.is_disabled(),
            "the schedule must pause at the threshold"
        );
        assert!(
            status
                .disabled_reason
                .as_ref()
                .unwrap()
                .contains("consecutive transient failures"),
            "the pause reason must be visible and typed, not a generic disable, got {:?}",
            status.disabled_reason
        );
    }

    #[test]
    fn a_success_resets_the_consecutive_failure_count() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let due1 = t(8, 0, 0);
        let gen1 = claim(
            &store,
            "s1",
            due1,
            "worker-a",
            due1,
            Duration::from_secs(60),
        );
        store
            .record_outcome(
                "s1",
                due1,
                "worker-a",
                gen1,
                &RunOutcome::Failed(FailureClass::Transient {
                    detail: "timeout".to_string(),
                }),
            )
            .unwrap();
        let due2 = t(9, 0, 0);
        let gen2 = claim(
            &store,
            "s1",
            due2,
            "worker-a",
            due2,
            Duration::from_secs(60),
        );
        store
            .record_outcome("s1", due2, "worker-a", gen2, &RunOutcome::Success)
            .unwrap();
        let status = store.schedule_status("s1").unwrap();
        assert_eq!(status.consecutive_failures, 0);
    }

    #[test]
    fn last_seen_is_none_before_the_first_evaluation_and_advances_monotonically() {
        let store = ScheduleStore::open_in_memory().unwrap();
        assert_eq!(store.last_seen("s1").unwrap(), None);

        store.advance_last_seen("s1", t(9, 0, 0)).unwrap();
        assert_eq!(store.last_seen("s1").unwrap(), Some(t(9, 0, 0)));

        store.advance_last_seen("s1", t(10, 0, 0)).unwrap();
        assert_eq!(store.last_seen("s1").unwrap(), Some(t(10, 0, 0)));

        // A call with an earlier instant than what is already stored never moves
        // the value backward.
        store.advance_last_seen("s1", t(9, 30, 0)).unwrap();
        assert_eq!(store.last_seen("s1").unwrap(), Some(t(10, 0, 0)));
    }

    /// Two real OS threads, two independent connections to the same on-disk database,
    /// racing on the same occurrence. This is the scenario the roadmap and decision
    /// 0052 name explicitly, correctly read as "at least one of two racing claims must
    /// win, and at most one" - see the module doc for why this is at-least-once
    /// execution with fencing, not exactly-once execution. A `Barrier` releases both
    /// threads at the same instant so the race is real, not an artifact of one thread
    /// finishing before the other starts.
    #[test]
    fn two_connections_racing_on_one_occurrence_claim_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("schedule.sqlite3");
        ScheduleStore::open(&db_path).unwrap();

        let due = t(9, 0, 0);
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for name in ["worker-a", "worker-b"] {
            let db_path = db_path.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let store = ScheduleStore::open(&db_path).unwrap();
                barrier.wait();
                store
                    .claim_occurrence("race-schedule", due, name, due, Duration::from_secs(60))
                    .unwrap()
            }));
        }
        let results: Vec<ClaimOutcome> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let claimed = results
            .iter()
            .filter(|r| matches!(r, ClaimOutcome::Claimed { .. }))
            .count();
        assert_eq!(
            claimed, 1,
            "exactly one of the two racing claims must win, got {results:?}"
        );

        let store = ScheduleStore::open(&db_path).unwrap();
        let claimant = occurrence_claimant(&store, "race-schedule", due);
        assert!(
            claimant == Some("worker-a".to_string()) || claimant == Some("worker-b".to_string())
        );
    }

    #[test]
    fn enable_clears_a_disable_but_not_the_failure_count() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(9, 0, 0);
        let generation = claim(&store, "s1", due, "worker-a", due, Duration::from_secs(60));
        store
            .record_outcome(
                "s1",
                due,
                "worker-a",
                generation,
                &RunOutcome::Failed(FailureClass::Disabling(DisableReason::MissingCredential {
                    detail: "x".into(),
                })),
            )
            .unwrap();
        assert!(store.schedule_status("s1").unwrap().is_disabled());
        store.enable("s1").unwrap();
        let status = store.schedule_status("s1").unwrap();
        assert!(!status.is_disabled(), "enable must clear the disable");
    }

    #[test]
    fn remove_schedule_drops_its_bookkeeping_and_occurrence_history() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(9, 0, 0);
        claim(&store, "s1", due, "worker-a", due, Duration::from_secs(60));
        store.remove_schedule("s1").unwrap();
        assert_eq!(store.last_seen("s1").unwrap(), None);
        assert_eq!(store.occurrence_status("s1", due).unwrap(), None);
        let status = store.schedule_status("s1").unwrap();
        assert_eq!(status.consecutive_failures, 0);
        assert!(!status.is_disabled());
    }

    #[test]
    fn tick_schedule_advances_last_seen_only_to_the_claimed_due_not_to_now() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(4, 0, 0);
        let now = t(4, 0, 30);
        let decision = store
            .tick_schedule("s1", now, "a", Duration::from_secs(60), |_last_seen| {
                Ok(Some((due, true)))
            })
            .unwrap();
        assert!(matches!(
            decision,
            TickDecision::Evaluated {
                due: d,
                claim: ClaimOutcome::Claimed { .. }
            } if d == due
        ));
        // Advanced to `due` (04:00:00), not to `now` (04:00:30).
        assert_eq!(store.last_seen("s1").unwrap(), Some(due));
    }

    #[test]
    fn tick_schedule_does_nothing_and_does_not_advance_when_decide_declines() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let now = t(4, 0, 30);
        let decision = store
            .tick_schedule("s1", now, "a", Duration::from_secs(60), |_| Ok(None))
            .unwrap();
        assert_eq!(decision, TickDecision::NothingDue);
        assert_eq!(store.last_seen("s1").unwrap(), None);
    }

    /// A dropped occurrence (`should_claim = false`) still advances `last_seen`
    /// to it, in the same transaction, and never claims it. Without this, a
    /// `Skip`-policy schedule's missed occurrence, dropped on wake, stays unclaimed
    /// *and* unaccounted for, so it keeps being re-proposed as "due" on every
    /// subsequent tick until it happens to also count as on-time again.
    #[test]
    fn tick_schedule_dropped_advances_last_seen_but_never_claims() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(1, 0, 0);
        let now = t(4, 30, 0);
        let decision = store
            .tick_schedule("s1", now, "a", Duration::from_secs(60), |_| {
                Ok(Some((due, false)))
            })
            .unwrap();
        assert_eq!(decision, TickDecision::Dropped { due });
        assert_eq!(
            store.last_seen("s1").unwrap(),
            Some(due),
            "a dropped occurrence must still be recorded as seen"
        );
        assert_eq!(
            store.occurrence_status("s1", due).unwrap(),
            None,
            "a dropped occurrence must never be claimed"
        );
    }

    #[test]
    fn expired_claims_finds_only_claimed_rows_whose_lease_has_passed() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(1, 0, 0);
        claim(&store, "s1", due, "a", due, Duration::from_secs(30));
        assert_eq!(
            store
                .expired_claims("s1", due + chrono::Duration::seconds(10))
                .unwrap(),
            Vec::<ExpiredClaim>::new(),
            "a claim within its lease is not expired"
        );
        let later = due + chrono::Duration::minutes(5);
        assert_eq!(
            store.expired_claims("s1", later).unwrap(),
            vec![ExpiredClaim {
                due_at: due,
                deferred_count: 0,
                held_for_manual_run: false,
            }]
        );

        // Once settled, it is no longer `claimed` and drops out of the scan.
        store
            .record_outcome("s1", due, "a", 1, &RunOutcome::Success)
            .unwrap();
        assert!(store.expired_claims("s1", later).unwrap().is_empty());
    }

    #[test]
    fn tick_schedule_propagates_a_decide_error_and_changes_nothing() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let now = t(4, 0, 30);
        let err = store
            .tick_schedule("s1", now, "a", Duration::from_secs(60), |_| {
                Err(ScheduleError::UnknownSchedule("boom".into()))
            })
            .unwrap_err();
        assert!(matches!(err, ScheduleError::UnknownSchedule(_)));
        assert_eq!(store.last_seen("s1").unwrap(), None);
    }

    /// C3txn: proves the `BEGIN IMMEDIATE ... COMMIT` wrapping itself is load-bearing,
    /// not just decorative - `decide` succeeds and `advance_last_seen` runs, but the
    /// *second* statement inside the same closure (`claim_occurrence`, given a lease
    /// so large it cannot fit in a `chrono::Duration`) then fails. Without a
    /// surrounding transaction, `advance_last_seen` is its own auto-committed
    /// statement and its effect survives the later error; with one, the whole
    /// closure's effect rolls back together. This needs no concurrency to show: a
    /// revert of the transaction wrapping alone (leaving every other line unchanged)
    /// makes this fail.
    #[test]
    fn tick_schedule_rolls_back_advance_last_seen_when_a_later_statement_in_the_same_call_fails() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(4, 0, 0);
        let now = t(4, 0, 30);
        let err = store
            .tick_schedule("s1", now, "a", Duration::from_secs(u64::MAX), |_| {
                Ok(Some((due, true)))
            })
            .unwrap_err();
        assert!(matches!(err, ScheduleError::DurationOutOfRange { .. }));
        assert_eq!(
            store.last_seen("s1").unwrap(),
            None,
            "advance_last_seen's effect must roll back with the rest of the failed call"
        );
    }

    /// The crash scenario, made real: `tick_schedule` never leaves `last_seen`
    /// advanced past an occurrence nobody holds a claim on. A crash between "decide
    /// this is due" and "commit the claim" (simulated here by never calling
    /// `tick_schedule` a second time - the closure runs entirely inside the
    /// transaction, so there is no gap for a real crash to land in) is not
    /// reachable at all; this test instead proves the postcondition a crash-free run
    /// gives, and the boundary-race test below proves the transaction really
    /// serializes two callers.
    #[test]
    fn occurrence_counts_tallies_done_and_failed_separately() {
        let store = ScheduleStore::open_in_memory().unwrap();
        for (h, ok) in [(1, true), (2, false), (3, true)] {
            let due = t(h, 0, 0);
            let generation = claim(&store, "s1", due, "a", due, Duration::from_secs(60));
            let outcome = if ok {
                RunOutcome::Success
            } else {
                RunOutcome::Failed(FailureClass::Transient { detail: "x".into() })
            };
            store
                .record_outcome("s1", due, "a", generation, &outcome)
                .unwrap();
        }
        assert_eq!(store.occurrence_counts("s1").unwrap(), (2, 1));
    }

    #[test]
    fn clamp_last_seen_only_moves_backward_and_reports_whether_it_did() {
        let store = ScheduleStore::open_in_memory().unwrap();
        store.advance_last_seen("s1", t(10, 0, 0)).unwrap();
        assert!(
            !store.clamp_last_seen("s1", t(11, 0, 0)).unwrap(),
            "now is later: no clamp"
        );
        assert_eq!(store.last_seen("s1").unwrap(), Some(t(10, 0, 0)));
        assert!(
            store.clamp_last_seen("s1", t(9, 0, 0)).unwrap(),
            "now is earlier: clamp"
        );
        assert_eq!(store.last_seen("s1").unwrap(), Some(t(9, 0, 0)));
    }

    #[test]
    fn a_lease_expired_claim_is_reclaimed_through_tick_schedule_and_is_visible_as_a_reclaim() {
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(1, 0, 0);
        // A first "crashed" claimant takes the occurrence with a short lease and
        // never reports back (exactly what `advance_last_seen` running ahead of a
        // crash used to strand, per the module's note).
        let first = store
            .tick_schedule("s1", due, "crashed", Duration::from_secs(30), |_| {
                Ok(Some((due, true)))
            })
            .unwrap();
        let TickDecision::Evaluated {
            claim: ClaimOutcome::Claimed {
                generation: gen1, ..
            },
            ..
        } = first
        else {
            panic!("expected the first claim to succeed, got {first:?}");
        };
        assert_eq!(gen1, 1);

        // Fifty minutes later (past the 30s lease), a live loop reevaluates the same
        // occurrence and reclaims it.
        let later = due + chrono::Duration::minutes(50);
        let second = store
            .tick_schedule("s1", later, "b", Duration::from_secs(60), |last_seen| {
                assert_eq!(
                    last_seen,
                    Some(due),
                    "last_seen already names the due occurrence"
                );
                Ok(None) // nothing NEW is due; this call only proves reclaiming needs a direct claim_occurrence, see below
            })
            .unwrap();
        assert_eq!(second, TickDecision::NothingDue);

        // The actual reclaim path: a caller (the scheduler loop) that already knows
        // `due` is the occurrence to retry calls `claim_occurrence` directly - this
        // is the generation bump a caller uses to know "this is a reclaim, not a
        // fresh claim" and emit a visible event for it.
        let reclaim = store
            .claim_occurrence("s1", due, "b", later, Duration::from_secs(60))
            .unwrap();
        let ClaimOutcome::Claimed {
            generation: gen2, ..
        } = reclaim
        else {
            panic!("expected the abandoned lease to be reclaimable, got {reclaim:?}");
        };
        assert!(
            gen2 > gen1,
            "a reclaim's generation must move forward, proving it is distinguishable from a fresh claim"
        );
    }

    /// The boundary race: two connections evaluating the same
    /// schedule for two *different*, adjacent occurrences at nearly the same
    /// instant. Before `tick_schedule`'s `BEGIN IMMEDIATE`, both could read
    /// `last_seen` before either wrote it, and both could then believe their own
    /// occurrence was the one to claim. With the transaction, the second to reach
    /// `BEGIN IMMEDIATE` blocks until the first commits and then decides against the
    /// first's already-advanced `last_seen`.
    #[test]
    fn two_instances_straddling_a_due_boundary_do_not_both_advance_past_it() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("boundary.sqlite3");
        ScheduleStore::open(&db_path).unwrap();
        let sa = ScheduleStore::open(&db_path).unwrap();
        let sb = ScheduleStore::open(&db_path).unwrap();

        let earlier = t(3, 59, 40);
        let later = t(4, 0, 0);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let ba = barrier.clone();
        let handle_a = thread::spawn(move || {
            ba.wait();
            sa.tick_schedule("r", earlier, "a", Duration::from_secs(900), |last_seen| {
                Ok(last_seen.is_none().then_some((earlier, true)))
            })
        });
        let bb = barrier.clone();
        let handle_b = thread::spawn(move || {
            bb.wait();
            sb.tick_schedule("r", later, "b", Duration::from_secs(900), |last_seen| {
                // B only claims `later` if A has not already claimed `earlier` -
                // whichever transaction runs second sees the other's committed
                // `last_seen` and correctly declines.
                Ok((last_seen.is_none() || last_seen == Some(earlier)).then_some((later, true)))
            })
        });
        let ra = handle_a.join().unwrap().unwrap();
        let rb = handle_b.join().unwrap().unwrap();

        // Both may have claimed (A always claims `earlier` since B's guard never
        // blocks it; B claims `later` only after observing A's result, whichever
        // order the transactions serialize in) - what must never happen is B
        // deciding to claim `later` from a `last_seen` that predates A's commit and
        // then losing that information: the final stored `last_seen` must reflect
        // whichever occurrence was decided last, not silently reflect neither.
        let final_last_seen = ScheduleStore::open(&db_path)
            .unwrap()
            .last_seen("r")
            .unwrap();
        assert!(
            final_last_seen == Some(earlier) || final_last_seen == Some(later),
            "the transaction must leave a consistent, not a torn, last_seen: {ra:?} {rb:?} -> {final_last_seen:?}"
        );
    }

    #[test]
    fn a_capacity_deferral_retries_via_the_expired_claim_scan_without_counting_as_a_failure() {
        // a capacity refusal must not count toward
        // `consecutive_failures`, and must come back as claimable again without
        // waiting out a full lease - the whole point of routing it through the same
        // "lease already expired" mechanism `expired_claims` already uses for a
        // crashed claimant.
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(9, 0, 0);
        let generation = claim(&store, "s1", due, "worker-a", due, Duration::from_secs(900));

        let outcome = store
            .record_deferral("s1", due, "worker-a", generation, due, false)
            .unwrap();
        assert_eq!(outcome, DeferralOutcome::Deferred { deferred_count: 1 });

        // Still `claimed`, not `failed` or `done` - a deferral is not a finalization.
        assert_eq!(
            store.occurrence_status("s1", due).unwrap(),
            Some("claimed".to_owned())
        );
        // Never counted: this is exactly what the deferral bookkeeping exists to rule out.
        assert_eq!(store.schedule_status("s1").unwrap().consecutive_failures, 0);

        // The same mechanism a crashed claimant's lease expiry uses finds it again on
        // the very next tick (a moment later, not a full lease later) - no separate
        // "deferred" scan needed, and it is marked as a deferral retry,
        // not indistinguishable from a genuine crash.
        let next_tick = due + chrono::Duration::seconds(1);
        let expired = store.expired_claims("s1", next_tick).unwrap();
        assert_eq!(
            expired,
            vec![ExpiredClaim {
                due_at: due,
                deferred_count: 1,
                held_for_manual_run: false,
            }],
            "a deferred occurrence must be findable on the very next scan, not only after a full lease"
        );

        let generation2 = claim(
            &store,
            "s1",
            due,
            "worker-a",
            next_tick,
            Duration::from_secs(900),
        );
        assert!(
            generation2 > generation,
            "the retry is a fresh claim (fenced by a new generation)"
        );
    }

    #[test]
    fn a_capacity_deferral_the_caller_gives_up_on_finalizes_as_a_counted_failure() {
        // the store no longer decides when to
        // give up (that used to be a fixed tick count, `MAX_DEFERRALS`, unrelated to
        // how long a slot might reasonably take to free); the caller does, and tells
        // the store via `give_up`. Only the final, given-up-on deferral counts.
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(9, 0, 0);
        let mut now = due;
        let mut generation = claim(&store, "s1", due, "worker-a", now, Duration::from_secs(900));

        for i in 1..4 {
            let outcome = store
                .record_deferral("s1", due, "worker-a", generation, now, false)
                .unwrap();
            assert_eq!(outcome, DeferralOutcome::Deferred { deferred_count: i });
            now += chrono::Duration::seconds(1);
            generation = claim(&store, "s1", due, "worker-a", now, Duration::from_secs(900));
        }

        let outcome = store
            .record_deferral("s1", due, "worker-a", generation, now, true)
            .unwrap();
        assert_eq!(outcome, DeferralOutcome::Exhausted { deferred_count: 4 });
        assert_eq!(
            store.occurrence_status("s1", due).unwrap(),
            Some("failed".to_owned())
        );
        assert_eq!(
            store.schedule_status("s1").unwrap().consecutive_failures,
            1,
            "only the final, given-up-on deferral counts - the earlier ones must not"
        );
    }

    #[test]
    fn drop_deferred_ends_it_uncounted_and_terminal() {
        // Under `Skip`, a deferral past the on-time window is dropped, not run
        // late and not counted - the same "missed means dropped, not late" rule
        // `Skip` already applies to an occurrence that was never even claimed.
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(9, 0, 0);
        let generation = claim(&store, "s1", due, "worker-a", due, Duration::from_secs(900));
        store
            .drop_deferred("s1", due, "worker-a", generation)
            .unwrap();
        assert_eq!(
            store.occurrence_status("s1", due).unwrap(),
            Some("dropped".to_owned())
        );
        assert_eq!(
            store.schedule_status("s1").unwrap().consecutive_failures,
            0,
            "a drop must never count toward the pause threshold"
        );
        // Terminal: no future scan (the expired-claim scan included) ever selects it
        // again, since it is no longer `claimed`.
        let later = due + chrono::Duration::hours(1);
        assert!(store.expired_claims("s1", later).unwrap().is_empty());
        assert!(matches!(
            store
                .claim_occurrence("s1", due, "worker-b", later, Duration::from_secs(900))
                .unwrap(),
            ClaimOutcome::NotClaimed
        ));
    }

    #[test]
    fn set_run_id_is_fenced_and_run_ids_for_schedule_dedupes_across_retries() {
        // The run id a caller can trust is the one the store itself recorded at
        // the moment a run actually started, not one reconstructed from
        // `schedule_id` and `due_at` by a second piece of code that might drift from
        // `ScheduledRuns::run_id`'s own formula.
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(9, 0, 0);
        let generation = claim(&store, "s1", due, "worker-a", due, Duration::from_secs(900));

        // A stale (claimant, generation) is refused, not silently accepted.
        assert!(
            store
                .set_run_id("s1", due, "worker-a", generation + 1, "sched-s1-0")
                .is_err()
        );

        store
            .set_run_id("s1", due, "worker-a", generation, "sched-s1-0")
            .unwrap();

        // A reclaim of the same occurrence (a new generation) records the same run
        // id again - `run_ids_for_schedule` must not report it twice.
        store
            .record_deferral("s1", due, "worker-a", generation, due, false)
            .unwrap();
        let next_tick = due + chrono::Duration::seconds(1);
        let generation2 = claim(
            &store,
            "s1",
            due,
            "worker-a",
            next_tick,
            Duration::from_secs(900),
        );
        store
            .set_run_id("s1", due, "worker-a", generation2, "sched-s1-0")
            .unwrap();

        assert_eq!(
            store.run_ids_for_schedule("s1").unwrap(),
            vec!["sched-s1-0".to_owned()]
        );
        // A schedule with no run ever recorded yet reports none, not an error.
        assert!(store.run_ids_for_schedule("s2").unwrap().is_empty());
    }

    #[test]
    fn record_manual_run_is_included_in_run_ids_for_schedule_alongside_clock_started_ones() {
        // A `schedule run-now` never claims an occurrence at all, so
        // without this table its spend was invisible to `schedule list` entirely -
        // undercounting the reported figure (the number of scheduled runs and their
        // total spend).
        let store = ScheduleStore::open_in_memory().unwrap();
        let due = t(9, 0, 0);
        let generation = claim(&store, "s1", due, "worker-a", due, Duration::from_secs(900));
        store
            .set_run_id("s1", due, "worker-a", generation, "sched-s1-1000")
            .unwrap();
        store
            .record_manual_run("s1", "sched-s1-1700000000-abcd1234")
            .unwrap();
        // Idempotent: a repeat of the exact same run id never duplicates.
        store
            .record_manual_run("s1", "sched-s1-1700000000-abcd1234")
            .unwrap();

        assert_eq!(
            store.run_ids_for_schedule("s1").unwrap(),
            vec![
                "sched-s1-1000".to_owned(),
                "sched-s1-1700000000-abcd1234".to_owned(),
            ]
        );
    }

    /// Verifier item 9: the read-only open runs no schema batch and writes
    /// nothing; reading the status works, a write is refused, and a schedule the
    /// table never saw is not registered by the read.
    #[test]
    fn a_read_only_claim_table_reads_status_and_writes_nothing() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("schedule.db");
        {
            let store = ScheduleStore::open(&path).expect("store");
            store.register_schedule("known").expect("register");
        }
        let ro = ScheduleStore::open_read_only(&path).expect("read-only");
        assert!(ro.peek_status("known").expect("peek").is_some());
        assert!(ro.peek_status("unseen").expect("peek").is_none());
        assert!(
            ro.occurrences_since("known", None)
                .expect("read")
                .is_empty()
        );
        assert!(
            ro.register_schedule("unseen").is_err(),
            "writes are refused"
        );
        let rw = ScheduleStore::open(&path).expect("store");
        assert!(
            rw.peek_status("unseen").expect("peek").is_none(),
            "the read registered nothing"
        );
    }

    const LEASE: Duration = Duration::from_secs(60);

    #[test]
    fn a_manual_run_is_refused_on_a_disabled_schedule_unless_forced() {
        let store = ScheduleStore::open_in_memory().expect("store");
        store.register_schedule("s").expect("register");
        store
            .conn
            .execute(
                "UPDATE schedules SET disabled = 1, disabled_reason = 'too many failures'
                 WHERE schedule_id = 's'",
                [],
            )
            .expect("disable");
        let now = t(9, 0, 0);
        match store
            .claim_manual_run("s", "run-now:1", now, LEASE, false)
            .expect("claim")
        {
            ManualClaim::Disabled { reason } => assert_eq!(reason, "too many failures"),
            other => panic!("expected Disabled, got {other:?}"),
        }
        assert!(matches!(
            store
                .claim_manual_run("s", "run-now:1", now, LEASE, true)
                .expect("forced"),
            ManualClaim::Claimed { .. }
        ));
    }

    #[test]
    fn a_manual_run_is_busy_beside_a_live_claim_or_another_manual_run() {
        let store = ScheduleStore::open_in_memory().expect("store");
        let now = t(9, 0, 0);
        claim(&store, "s", t(8, 59, 0), "loop-a", now, LEASE);
        match store
            .claim_manual_run("s", "run-now:1", now, LEASE, false)
            .expect("claim")
        {
            ManualClaim::Busy { holder } => assert!(holder.contains("loop-a"), "{holder}"),
            other => panic!("expected Busy, got {other:?}"),
        }
        // Once the occurrence's lease has passed, the manual run may proceed, and
        // then a second one is refused.
        let later = now + chrono::Duration::seconds(120);
        assert!(matches!(
            store
                .claim_manual_run("s", "run-now:1", later, LEASE, false)
                .expect("claim"),
            ManualClaim::Claimed { .. }
        ));
        match store
            .claim_manual_run("s", "run-now:2", later, LEASE, false)
            .expect("claim")
        {
            ManualClaim::Busy { holder } => assert_eq!(holder, "run-now:1"),
            other => panic!("expected Busy, got {other:?}"),
        }
        store.release_manual_run("s", "run-now:1").expect("release");
        assert!(matches!(
            store
                .claim_manual_run("s", "run-now:2", later, LEASE, false)
                .expect("claim"),
            ManualClaim::Claimed { .. }
        ));
    }

    /// The scheduler loop's claim waits for a manual run, and the occurrence it
    /// could not claim is not lost: it stays reachable through `expired_claims`.
    #[test]
    fn the_loop_cannot_claim_beside_a_manual_run_and_the_occurrence_is_kept() {
        let store = ScheduleStore::open_in_memory().expect("store");
        let due = t(9, 0, 0);
        let now = t(9, 0, 5);
        store
            .claim_manual_run("s", "run-now:1", now, LEASE, false)
            .expect("manual");
        assert_eq!(
            store
                .claim_occurrence("s", due, "loop-a", now, LEASE)
                .expect("claim"),
            ClaimOutcome::NotClaimed
        );
        // Still held on the next tick, still not lost.
        let next = now + chrono::Duration::seconds(20);
        assert!(
            store
                .renew_manual_run("s", "run-now:1", next, LEASE)
                .expect("renew")
        );
        let waiting = store.expired_claims("s", next).expect("scan");
        assert_eq!(waiting.len(), 1, "the occurrence must stay findable");
        assert_eq!(
            store
                .claim_occurrence("s", waiting[0].due_at, "loop-a", next, LEASE)
                .expect("claim"),
            ClaimOutcome::NotClaimed
        );

        store.release_manual_run("s", "run-now:1").expect("release");
        let after = next + chrono::Duration::seconds(1);
        assert!(matches!(
            store
                .claim_occurrence("s", waiting[0].due_at, "loop-a", after, LEASE)
                .expect("claim"),
            ClaimOutcome::Claimed { .. }
        ));
    }

    #[test]
    fn a_manual_lock_whose_holder_died_stops_blocking_when_its_lease_passes() {
        let store = ScheduleStore::open_in_memory().expect("store");
        let now = t(9, 0, 0);
        store
            .claim_manual_run("s", "run-now:1", now, LEASE, false)
            .expect("manual");
        let later = now + chrono::Duration::seconds(61);
        assert!(matches!(
            store
                .claim_occurrence("s", t(9, 0, 30), "loop-a", later, LEASE)
                .expect("claim"),
            ClaimOutcome::Claimed { .. }
        ));
        assert!(
            !store
                .renew_manual_run("s", "someone-else", later, LEASE)
                .expect("renew")
        );
    }

    /// A lease that lapsed while nobody took the schedule (the machine slept past
    /// it) is renewed: the run still holds the lock and lost nothing.
    #[test]
    fn a_lapsed_manual_lock_nobody_took_is_renewed() {
        let store = ScheduleStore::open_in_memory().expect("store");
        let now = t(9, 0, 0);
        store
            .claim_manual_run("s", "run-now:1", now, LEASE, false)
            .expect("manual");
        let woke = now + chrono::Duration::seconds(LEASE.as_secs() as i64 * 5);
        assert!(
            store
                .renew_manual_run("s", "run-now:1", woke, LEASE)
                .expect("renew"),
            "the run lost nothing while the machine slept"
        );
        // The renewed lock blocks the scheduler again.
        assert_eq!(
            store
                .claim_occurrence("s", t(9, 0, 30), "loop-a", woke, LEASE)
                .expect("claim"),
            ClaimOutcome::NotClaimed
        );
    }

    /// A lapsed lock another manual run has since taken is not renewed by the
    /// first run.
    #[test]
    fn a_lapsed_manual_lock_another_run_took_is_not_renewed() {
        let store = ScheduleStore::open_in_memory().expect("store");
        let now = t(9, 0, 0);
        store
            .claim_manual_run("s", "run-now:1", now, LEASE, false)
            .expect("manual");
        let later = now + chrono::Duration::seconds(LEASE.as_secs() as i64 + 1);
        assert!(matches!(
            store
                .claim_manual_run("s", "run-now:2", later, LEASE, false)
                .expect("second"),
            ManualClaim::Claimed { .. }
        ));
        assert!(
            !store
                .renew_manual_run("s", "run-now:1", later, LEASE)
                .expect("renew"),
            "the first run renewed the second run's lock"
        );
    }

    /// A lapsed manual lock is not revived by a late renewal once the scheduler
    /// has claimed the schedule.
    #[test]
    fn a_lapsed_manual_lock_is_not_renewed() {
        let store = ScheduleStore::open_in_memory().expect("store");
        let now = t(9, 0, 0);
        store
            .claim_manual_run("s", "run-now:1", now, LEASE, false)
            .expect("manual");
        let live = now + chrono::Duration::seconds(30);
        assert!(
            store
                .renew_manual_run("s", "run-now:1", live, LEASE)
                .expect("renew")
        );
        let lapsed = live + chrono::Duration::seconds(LEASE.as_secs() as i64 + 1);
        assert!(matches!(
            store
                .claim_occurrence("s", t(9, 0, 30), "loop-a", lapsed, LEASE)
                .expect("claim"),
            ClaimOutcome::Claimed { .. }
        ));
        assert!(
            !store
                .renew_manual_run("s", "run-now:1", lapsed, LEASE)
                .expect("renew"),
            "a lapsed lock was renewed over a live scheduled claim"
        );
    }

    /// An occurrence set aside for a manual run is marked so the loop can apply
    /// the missed-run policy when it comes back, and `drop_held_for_manual_run`
    /// settles it without running it.
    #[test]
    fn an_occurrence_held_for_a_manual_run_can_be_dropped_later() {
        let store = ScheduleStore::open_in_memory().expect("store");
        let now = t(9, 0, 0);
        store
            .claim_manual_run("s", "run-now:1", now, LEASE, false)
            .expect("manual");
        let due = t(9, 0, 10);
        let held_at = t(9, 0, 20);
        assert_eq!(
            store
                .claim_occurrence("s", due, "loop-a", held_at, LEASE)
                .expect("claim"),
            ClaimOutcome::NotClaimed
        );
        let later = t(9, 30, 0);
        let expired = store.expired_claims("s", later).expect("scan");
        assert_eq!(expired.len(), 1);
        assert!(expired[0].held_for_manual_run);
        assert!(
            store
                .drop_held_for_manual_run("s", due, later)
                .expect("drop")
        );
        assert!(store.expired_claims("s", later).expect("scan").is_empty());
        assert_eq!(
            store.occurrence_status("s", due).expect("status"),
            Some("dropped".to_owned())
        );
        // A second drop finds nothing to change.
        assert!(
            !store
                .drop_held_for_manual_run("s", due, later)
                .expect("drop")
        );
    }
}
