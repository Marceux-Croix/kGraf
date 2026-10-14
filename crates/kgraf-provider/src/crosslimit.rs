//! Cross-process GCRA rate limiting, shared by every kGraf process on this
//! machine (decision 0072).
//!
//! [`ratelimit`](crate::ratelimit)'s `governor` limiter is per process: the
//! desktop app's per-project hosts, the CLI, and a scheduled run each build
//! their own, and together they can exceed a route's real limit even though
//! each one individually honors it. This module gives every process on one
//! machine one shared counter per `(route, key)` instead.
//!
//! ## Reuse search (requirement 1)
//!
//! Searched crates.io and GitHub (2026-09-27) for a cross-process token
//! bucket or GCRA crate before writing this:
//!
//! - `governor` (already in this workspace, `ratelimit.rs`) is explicitly
//!   in-process: its keyed variant uses `dashmap`, which is still one
//!   process's memory.
//! - `ratelimit_meter`, `leaky-bucket`, `nonzero_ext`-based limiters: all
//!   in-process, same limit.
//! - `redis-cell` and `governor`'s own suggested pairing with a Redis
//!   backend implement GCRA cross-process, but need a Redis server, which
//!   this project's "local only, single-user" rule rules out (no hosted
//!   dependency for a desktop app feature).
//! - No maintained crate on crates.io implements a file- or
//!   SQLite-backed GCRA or token bucket as of this search (`ratelimit`,
//!   `throttle`, `governor-*` name variants inspected and rejected: either
//!   in-process or unmaintained since before 2023).
//!
//! What is reused instead: this crate's own SQLite WAL / `busy_timeout` /
//! `BEGIN IMMEDIATE` transaction pattern from `accounting.rs` (decision
//! 0059), which already proves out under concurrent writers on every
//! platform kGraf ships (`benches/ledger_concurrent.rs`), and the per-user
//! `directories::ProjectDirs::from("", "", "kGraf").data_local_dir()`
//! convention already used by `kgraf-shell::app_dirs` and
//! `kgraf-mcp::oauth`'s keychain lock directory.
//!
//! ## Design (requirement 2)
//!
//! One SQLite file per user, at [`default_store_path`], holding one row per
//! `(route, key hash)`: `tat_micros` (the GCRA "theoretical arrival time" in
//! microseconds since the Unix epoch), `rpm`/`burst` (whichever
//! configuration is currently in force; see "The most recently configured
//! limit wins" below) and `limit_set_at_micros` (when that configuration was
//! opened). The credential itself is never stored or logged; [`key_id`]
//! hashes it with `blake3` and truncates to a 64-bit (16 hex characters)
//! prefix, which is not a security boundary (the file has the same access as
//! the ledger already does) but is enough to keep two different keys on the
//! same route from sharing a bucket by accident.
//!
//! [`SharedLimiterStore`] keeps **one persistent connection per route**, not
//! one shared lock for the whole store: a single store-wide lock would
//! serialize every route's checks against each other in one process even
//! when they contend for nothing (a stalled holder on route A would queue up
//! route B's checks behind it, in-process, before B ever reached SQLite;
//! opening a fresh connection per call was tried first and measured to cost
//! 1-2 ms per call on its own from `open(2)` and page-fault overhead alone,
//! blowing the budget even at concurrency 1). Two calls on the *same* route
//! in one process do still serialize (correctly, since they contend for the
//! same row regardless of connection identity), while two different routes'
//! calls only ever contend for the brief, sub-microsecond lock around the
//! per-route connection map itself, never for each other's SQLite work.
//!
//! Every check runs inside one `BEGIN IMMEDIATE ... COMMIT` transaction, so
//! two processes racing for the same key serialize on SQLite's own file
//! lock rather than a hand-rolled `flock` plus shared memory: a second
//! coordination mechanism next to the one `accounting.rs` already
//! maintains and benchmarks would be new surface for no new guarantee.
//! SQLite's locking is portable to Windows through `rusqlite`'s bundled
//! build, which is already a dependency here, so this needs no
//! platform-specific file-locking code at all.
//!
//! **Every per-route connection sets `synchronous = NORMAL` itself**
//! (`connection_for`): WAL mode is sticky in the file
//! (a later connection inherits it automatically), but `synchronous` is a
//! per-connection setting with no such persistence, so a route's connection
//! that never set it explicitly ran at SQLite's own default, `FULL`, paying
//! a full fsync on every commit regardless of contention. That alone was the
//! measured cause of a 0.1-0.9 s tail at concurrency 1 (no contention at
//! all) before this fix; with `NORMAL` set, the concurrency-1 max drops to
//! well under 1 ms (`benches/crosslimit.rs`). `NORMAL`, not `FULL` as the
//! ledger uses: losing the last commit to a crash between a WAL write and
//! its checkpoint makes the bucket look emptier than it should, which only
//! makes the *next* few checks slightly more permissive (the safe direction
//! for a rate limit, unlike the ledger, where losing a row hides real
//! spend).
//!
//! ## Bounded waiting, in two stages
//!
//! A check has two different locks to wait for, and each is now bounded
//! separately, with the second recomputed from what the first already spent,
//! so their sum never exceeds the caller's own budget:
//!
//! 1. **The in-process lock on the route's connection**
//!    ([`RouteLimiter::acquire`](crate::ratelimit::RouteLimiter::acquire),
//!    via [`SharedLimiterStore::check_bounded`]). This used to be a
//!    `std::sync::Mutex` taken unconditionally before any budget applied at
//!    all: eight callers on one route stalled behind a ninth could each wait
//!    up to the *sum* of everyone ahead of them (measured: about 8.5 s for
//!    eight 1 s-budget callers behind one stalled holder), each one parked on
//!    its own blocking-pool thread the whole time. It is now a
//!    `tokio::sync::Mutex`, acquired with `tokio::time::timeout(budget, ...)`
//!    **before** any blocking-pool thread is spent at all: a caller that
//!    cannot get the connection within its own budget returns
//!    [`CrossLimitError::LockTimeout`] immediately, without ever touching
//!    SQLite or a blocking thread.
//! 2. **SQLite's own write lock** (`busy_timeout`, inside
//!    [`tokio::task::spawn_blocking`]), for whatever budget is left after
//!    stage 1: `check_bounded` recomputes it from the elapsed wait, so a
//!    caller that spent most of its budget queued for the connection still
//!    cannot overrun its own total.
//!
//! [`SharedLimiterStore::check`] (used by tests, the bench and
//! `crosslimit-hammer`, all synchronous, single-purpose callers with nothing
//! else contending for the connection in-process) takes the connection with
//! a plain blocking lock and skips stage 1's bookkeeping.
//!
//! ## Clock jumps
//!
//! A `tat` written under a clock that later gets corrected backward (or a
//! forward jump followed by an NTP correction) must not lock a route out for
//! the size of that jump. A check treats a stored `tat` more than one
//! `tau + period` ahead of the current call's `now` as a clock anomaly, not a
//! real future reservation, and resets it to `now` rather than honoring it.
//! `now` is read after the write lock is held: a value read before waiting
//! for the lock could predate a reservation committed meanwhile, and the
//! reset would erase it.
//! A clock before the Unix epoch is a distinct, loud error
//! ([`CrossLimitError::ClockBeforeEpoch`]), never silently treated as
//! [`std::time::Duration::ZERO`] (`unwrap_or` would make a broken clock look
//! identical to "no time has passed," admitting everything).
//!
//! ## Read-only and non-WAL stores
//!
//! [`SharedLimiterStore::open`] verifies, once, that the file is actually
//! writable (`BEGIN IMMEDIATE; ROLLBACK`, after a fast
//! [`rusqlite::Connection::is_readonly`] check) and that `journal_mode`
//! actually switched to `wal` (`rusqlite::Connection::pragma_update_and_check`,
//! whose returned mode is checked, not assumed): a read-only mount or a
//! filesystem that silently refuses WAL (common on network shares) would
//! otherwise pass `open` and only fail on the first real check. **Both
//! probes together share one bounded deadline** (`OPEN_DEADLINE`, five
//! seconds): the previous version paired a fixed five-second `busy_timeout`
//! with its own outer, 200-attempt retry loop for *each* probe separately,
//! which could compound to roughly 1,000 seconds worst case and then
//! misreport the result as [`CrossLimitError::ReadOnly`] once the retries
//! ran out, when the real cause was contention, not a genuinely read-only
//! file. A busy failure within the deadline is now reported as
//! [`CrossLimitError::LockTimeout`], distinct from a genuine, non-busy
//! failure on either probe.
//!
//! **Known platform limits, stated rather than silently hit later:** WAL
//! mode needs a shared-memory (`-shm`) file with `mmap`-style sharing between
//! processes, which does not work over SMB or NFS (SQLite's own
//! documentation says so); a store placed on a network drive fails this
//! open-time WAL check rather than corrupting silently later. On Windows,
//! mandatory byte-range locking and antivirus real-time scanning can each add
//! their own delay on top of `busy_timeout`, which is why a check's budget is
//! the caller's own `max_wait_ms`, not a value this module invents.
//!
//! ## The most recently configured limit wins
//!
//! Two processes can configure different `RPM[/BURST]` for the same route (a
//! misconfiguration, not a supported feature, but one the store must survive
//! without becoming unusable). The **previous** version had a check
//! reconcile a mismatch itself, in favor of whichever limit was *stricter*,
//! and write that back on every call it saw a mismatch on. That has a defect
//! a user cannot work around: once the row holds the stricter of two
//! limits, it can never be raised again by configuring a looser one, because
//! every future check re-derives "stricter" from the same two candidates and
//! picks the same answer forever, and the only signal was a `stderr` line
//! nothing in the desktop app surfaces.
//!
//! Decision 0072: **the most recently configured limit wins.**
//! [`SharedLimiterStore::register_limit`] writes a route's configured
//! `RPM[/BURST]` once, stamped with when it was opened
//! (`RouteLimiter::bind_key`, once per process per route: see
//! `ratelimit.rs`); a later registration (a config change, then a restart)
//! overwrites an earlier one, never the other way around, and a check reads
//! whatever the row currently holds as authoritative. A check that finds no
//! row at all (nothing has ever registered this key) seeds one from its own
//! configured limit, exactly as `register_limit` would; a check never
//! otherwise touches `rpm`, `burst` or `limit_set_at_micros`, only `tat`, so
//! repeatedly *checking* a route can never look like repeatedly
//! *reconfiguring* it. `RouteLimiter::bind_key` prints one warning to
//! `stderr` per process per route, only when its own registration lost to a
//! newer one already there, never per check.
//!
//! ## Fail loud (requirement 3)
//!
//! [`SharedLimiterStore::open`], [`SharedLimiterStore::check`] and
//! [`SharedLimiterStore::check_bounded`] return `Err` on any I/O or SQLite
//! failure, or when there is no per-user data directory at all
//! ([`CrossLimitError::NoDataDir`]). There is no fallback to per-process
//! limiting anywhere in this module or its caller
//! (`kgraf-host::config::limiters`, its only production call site): a run
//! whose shared store cannot be opened refuses to start
//! ([`crate::RateLimitError`] carries the failure through
//! `RouteLimiter::new_shared`). `RouteLimiter::new` (the in-process
//! `governor` limiter) is a distinct, separately named constructor kept for
//! this crate's tests and its bench; production wiring never calls it.

use blake3::Hasher;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A shared rate limit check that could not be performed at all. The caller
/// must fail loud (fail loud): there is no silent per-process fallback.
#[derive(Debug, thiserror::Error)]
pub enum CrossLimitError {
    /// No per-user data directory exists on this machine (for example, `HOME`
    /// is unset). The route cannot be limited across processes at all.
    #[error(
        "no per-user data directory is available, so the cross-process rate limit for route \
         `{route}` cannot be enforced"
    )]
    NoDataDir { route: String },
    /// The store's directory could not be created.
    #[error("cannot create the shared rate limit directory {}: {source}", path.display())]
    Dir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The store file could not be opened or configured.
    #[error("cannot open the shared rate limit store at {}: {source}", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    /// `PRAGMA journal_mode = WAL` did not actually take: a network
    /// filesystem (SMB, NFS) or a read-only mount is the usual cause (see the
    /// module doc's "Read-only and non-WAL stores" section).
    #[error(
        "the shared rate limit store at {} did not switch to WAL mode (got `{got}`); this \
         usually means the per-user data directory is on a network drive or a read-only mount, \
         neither of which this store supports",
        path.display()
    )]
    NotWal { path: PathBuf, got: String },
    /// The store file is not writable.
    #[error("the shared rate limit store at {} is read-only", path.display())]
    ReadOnly { path: PathBuf },
    /// A query against an already-open store failed.
    #[error("the shared rate limit store at {} is unreadable: {source}", path.display())]
    Sqlite {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    /// The system clock reads before the Unix epoch. Never silently treated
    /// as "no time has passed": that would make a broken
    /// clock indistinguishable from an empty bucket and admit everything.
    #[error(
        "the system clock reads before 1970-01-01, so the shared rate limit for route `{route}` \
         cannot be computed"
    )]
    ClockBeforeEpoch { route: String },
    /// A write lock could not be reached inside a caller-given (or, for
    /// [`SharedLimiterStore::open`], this module's own) budget: another process, or
    /// another connection's blocking check, held it the whole time. `route`
    /// is `"<open>"` when this comes from `open` rather than from a check on
    /// a real route.
    #[error(
        "route `{route}` could not reach the shared rate limit store within {budget_ms} ms: \
         another process held its write lock the whole time"
    )]
    LockTimeout { route: String, budget_ms: u64 },
    /// The blocking task running a check panicked or was cancelled.
    #[error("the shared rate limit check for route `{route}` panicked or was cancelled: {detail}")]
    TaskFailed { route: String, detail: String },
}

/// The outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The call may proceed now. The shared state already reserved the slot,
    /// exactly as `governor`'s in-process `check()` does, so a second racing
    /// process cannot also be admitted for the same slot.
    Admitted,
    /// The call must wait this long before its next attempt. Nothing was
    /// reserved: a caller that gives up here never consumed a slot another
    /// process could have used.
    Wait(Duration),
}

/// Hash a credential into the key id half of a shared limiter's row key.
/// Never store or log the credential itself (requirement 2). A 64-bit
/// (16 hex characters) `blake3` prefix: not a security boundary, only enough
/// to keep two different keys on one route from sharing a bucket by accident.
pub fn key_id(credential: &str) -> String {
    let mut hasher = Hasher::new();
    hasher.update(credential.as_bytes());
    hasher.finalize().to_hex()[..16].to_string()
}

/// The per-user file every kGraf process on this machine shares, or `None`
/// when there is no home directory to place it under.
///
/// **Never callable from a test.** On 2026-09-27 a test run wrote a real
/// `rate-limits.sqlite3` into the user's actual per-user data directory,
/// before this guard (and `kgraf-host::config`'s explicit-store-path seams,
/// `check_configured_with_store` and `limiters`'s `store_path` parameter)
/// existed. Under `cfg(test)` this panics instead of resolving anything, so
/// this crate's own tests, and this crate's own bench and
/// `crosslimit-hammer`, cannot reach it either (all three already pass an
/// explicit tempdir path to `SharedLimiterStore::open` directly, never this
/// function, so the panic should never fire). `cfg(test)` only guards this
/// one crate; `kgraf-host` (the crate with the two real production call
/// sites, and the one the 2026-09-27 incident came through) carries its own
/// matching guard around both of them (`config.rs`'s and `host.rs`'s
/// `production_rate_limit_store_path`).
pub fn default_store_path() -> Option<PathBuf> {
    #[cfg(test)]
    {
        panic!(
            "kgraf_provider::crosslimit::default_store_path() must never be called from a \
             test: it resolves the user's real per-user data directory. Pass an explicit \
             store path instead (see this function's doc comment)."
        );
    }
    #[cfg(not(test))]
    {
        directories::ProjectDirs::from("", "", "kGraf")
            .map(|d| d.data_local_dir().join("rate-limits.sqlite3"))
    }
}

/// The SQLite-backed GCRA store: one per user. [`SharedLimiterStore::open`]
/// validates the file once (writable, WAL); every other method reuses one
/// persistent connection per route (see the module doc's "Design" section),
/// so no single in-process lock serializes routes that are not actually
/// contending with each other, and the steady-state cost stays a SQLite
/// transaction, not a connection open, per call.
#[derive(Debug)]
pub struct SharedLimiterStore {
    path: PathBuf,
    route_conns: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Connection>>>>,
}

/// [`retry_until_deadline`]'s failure: contention that ran out the clock, or
/// a genuine, non-busy SQLite error.
enum BusyOrOther {
    Busy,
    Other(rusqlite::Error),
}

/// Retry `f` against transient `SQLITE_BUSY`/`SQLITE_LOCKED` until `deadline`
/// elapses, resetting `conn`'s own `busy_timeout` to what remains on every
/// pass so the whole call, across every retry, never waits past `deadline`
/// (the previous version paired a fixed `busy_timeout`
/// with a fixed attempt count, which could compound far past either bound
/// alone). Only used by [`SharedLimiterStore::open`]; a check's own bounded
/// wait needs no retry loop at all (see the module doc's "Bounded waiting"
/// section).
fn retry_until_deadline<T>(
    conn: &Connection,
    deadline: Instant,
    mut f: impl FnMut() -> rusqlite::Result<T>,
) -> Result<T, BusyOrOther> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(BusyOrOther::Busy);
        }
        let _ = conn.busy_timeout(remaining);
        match f() {
            Ok(value) => return Ok(value),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if matches!(
                    err.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(BusyOrOther::Busy);
                }
                // `busy_timeout` already waited out its share; a short sleep
                // avoids a tight spin on a platform that returns busy at
                // once instead of honoring it.
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(other) => return Err(BusyOrOther::Other(other)),
        }
    }
}

impl SharedLimiterStore {
    /// Open (creating if absent) the store at `path`, and validate it is
    /// actually usable: writable, and really in WAL mode. A
    /// store that fails either check would otherwise pass this call and only
    /// fail on the first real check.
    pub fn open(path: &Path) -> Result<Self, CrossLimitError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| CrossLimitError::Dir {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let wrap_open = |source| CrossLimitError::Open {
            path: path.to_path_buf(),
            source,
        };
        let conn = Connection::open(path).map_err(wrap_open)?;
        // One deadline for the whole call, both probes together (see the module doc's "Read-only and non-WAL
        // stores" section for why.
        const OPEN_DEADLINE: Duration = Duration::from_secs(5);
        let deadline = Instant::now() + OPEN_DEADLINE;
        let open_timeout = || CrossLimitError::LockTimeout {
            route: "<open>".to_owned(),
            budget_ms: OPEN_DEADLINE.as_millis() as u64,
        };

        let mode: String = retry_until_deadline(&conn, deadline, || {
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
        })
        .map_err(|failure| match failure {
            BusyOrOther::Busy => open_timeout(),
            BusyOrOther::Other(source) => wrap_open(source),
        })?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(CrossLimitError::NotWal {
                path: path.to_path_buf(),
                got: mode,
            });
        }
        conn.execute_batch(
            // `synchronous = NORMAL` on this validation connection too, for
            // consistency; the connections that matter for the tail latency
            // fix are the per-route ones `connection_for` opens later, which
            // set it themselves.
            "PRAGMA synchronous = NORMAL; \
             CREATE TABLE IF NOT EXISTS gcra (\
                 key TEXT PRIMARY KEY, \
                 tat_micros INTEGER NOT NULL, \
                 rpm INTEGER NOT NULL, \
                 burst INTEGER NOT NULL, \
                 limit_set_at_micros INTEGER NOT NULL\
             );",
        )
        .map_err(wrap_open)?;
        // Belt: a fast, syscall-only check. Suspenders: a real write, since a
        // filesystem can report itself writable and still refuse a lock (a
        // read-only bind mount some drivers still open `O_RDWR` against).
        if conn.is_readonly(rusqlite::MAIN_DB).unwrap_or(false) {
            return Err(CrossLimitError::ReadOnly {
                path: path.to_path_buf(),
            });
        }
        retry_until_deadline(&conn, deadline, || {
            conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
        })
        .map_err(|failure| match failure {
            BusyOrOther::Busy => open_timeout(),
            BusyOrOther::Other(_source) => CrossLimitError::ReadOnly {
                path: path.to_path_buf(),
            },
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            route_conns: Mutex::new(HashMap::new()),
        })
    }

    /// This route's persistent connection, opening one on first use. The
    /// outer map lock is held only for the lookup-or-insert (a `HashMap` get
    /// or a fresh `Connection::open`, never a SQLite transaction), so two
    /// different routes' first connections do not serialize on each other's
    /// SQLite work, only briefly on this map.
    fn connection_for(
        &self,
        route: &str,
    ) -> Result<Arc<tokio::sync::Mutex<Connection>>, CrossLimitError> {
        let mut conns = self.route_conns.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(existing) = conns.get(route) {
            return Ok(existing.clone());
        }
        let wrap = |source| CrossLimitError::Sqlite {
            path: self.path.clone(),
            source,
        };
        let conn = Connection::open(&self.path).map_err(wrap)?;
        // WAL mode is sticky in the file, but
        // `synchronous` is not; every connection defaults to SQLite's own
        // `FULL` unless it sets `NORMAL` itself. See the module doc.
        conn.execute_batch("PRAGMA synchronous = NORMAL;")
            .map_err(wrap)?;
        let conn = Arc::new(tokio::sync::Mutex::new(conn));
        conns.insert(route.to_owned(), conn.clone());
        Ok(conn)
    }

    /// Register `limit` as this process's configured limit for `route`+`key`,
    /// opened at `opened_at`. The most recently configured limit wins (see
    /// the module doc): if the row already reflects a limit opened more
    /// recently by another process, this call does not overwrite it. Returns
    /// the limit now in effect after this call, so a caller whose own
    /// registration lost the race can warn once (`RouteLimiter::bind_key`),
    /// not on every check.
    pub fn register_limit(
        &self,
        route: &str,
        key: &str,
        limit: crate::RateLimit,
        opened_at: SystemTime,
    ) -> Result<crate::RateLimit, CrossLimitError> {
        let row_key = format!("{route}:{key}");
        let opened_at_us = opened_at
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CrossLimitError::ClockBeforeEpoch {
                route: route.to_owned(),
            })?
            .as_micros() as i64;
        let conn_arc = self.connection_for(route)?;
        // A one-time, startup event (once per process per route): a plain
        // bounded wait, with no retry loop beyond `lock_until`'s own polling,
        // is enough here, the same reasoning as a check's own bounded wait.
        const REGISTER_BUDGET: Duration = Duration::from_secs(5);
        let deadline = Instant::now() + REGISTER_BUDGET;
        let timeout = || CrossLimitError::LockTimeout {
            route: route.to_owned(),
            budget_ms: REGISTER_BUDGET.as_millis() as u64,
        };
        let conn = lock_until(&conn_arc, deadline).ok_or_else(timeout)?;
        let wrap = |source| CrossLimitError::Sqlite {
            path: self.path.clone(),
            source,
        };
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        conn.busy_timeout(remaining).map_err(wrap)?;
        match conn.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => {}
            Err(rusqlite::Error::SqliteFailure(err, _))
                if matches!(
                    err.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                return Err(CrossLimitError::LockTimeout {
                    route: route.to_owned(),
                    budget_ms: REGISTER_BUDGET.as_millis() as u64,
                });
            }
            Err(source) => return Err(wrap(source)),
        }
        let outcome = (|| -> rusqlite::Result<crate::RateLimit> {
            let existing: Option<(i64, u32, u32, i64)> = conn
                .query_row(
                    "SELECT tat_micros, rpm, burst, limit_set_at_micros FROM gcra WHERE key = ?1",
                    params![row_key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            match existing {
                None => {
                    conn.execute(
                        "INSERT INTO gcra (key, tat_micros, rpm, burst, limit_set_at_micros) \
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            row_key,
                            opened_at_us,
                            limit.requests_per_minute,
                            limit.burst,
                            opened_at_us
                        ],
                    )?;
                    Ok(limit)
                }
                // What is stored is at least as new as this registration: it
                // wins instead (ties go to whatever is already there, so
                // identical timestamps under coarse clock resolution do not
                // flap back and forth).
                Some((_, rpm, burst, set_at)) if opened_at_us <= set_at => Ok(crate::RateLimit {
                    requests_per_minute: rpm,
                    burst,
                    max_wait_ms: limit.max_wait_ms,
                }),
                // This registration is newer: it wins, and is written back.
                Some(_) => {
                    conn.execute(
                        "UPDATE gcra SET rpm = ?2, burst = ?3, limit_set_at_micros = ?4 \
                         WHERE key = ?1",
                        params![
                            row_key,
                            limit.requests_per_minute,
                            limit.burst,
                            opened_at_us
                        ],
                    )?;
                    Ok(limit)
                }
            }
        })();
        finish_transaction(&conn, outcome, wrap)
    }

    /// Check and, if admitted, reserve one slot for `route`+`key`, as of
    /// the caller-supplied `now`, waiting for the write lock for at most
    /// `budget`. Synchronous. `now` is fixed before the wait, so this is for
    /// tests that need a chosen instant; real-time callers use
    /// [`Self::check_now`].
    /// [`RouteLimiter::acquire`](crate::ratelimit::RouteLimiter::acquire)
    /// uses [`Self::check_bounded`] instead.
    pub fn check(
        &self,
        route: &str,
        key: &str,
        limit: crate::RateLimit,
        now: SystemTime,
        budget: Duration,
    ) -> Result<Admission, CrossLimitError> {
        let conn_arc = self.connection_for(route)?;
        let deadline = Instant::now() + budget;
        let conn = lock_until(&conn_arc, deadline).ok_or_else(|| CrossLimitError::LockTimeout {
            route: route.to_owned(),
            budget_ms: budget.as_millis() as u64,
        })?;
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        Self::run_check(&conn, &self.path, route, key, limit, &|| now, remaining)
    }

    /// [`Self::check`] with the wall clock read after the write lock is held, as
    /// the async path does. `check` takes `now` before it waits, so under
    /// contention the instant it carries can be older than a decision another
    /// caller already stored. Use this whenever the time is the real one.
    pub fn check_now(
        &self,
        route: &str,
        key: &str,
        limit: crate::RateLimit,
        budget: Duration,
    ) -> Result<Admission, CrossLimitError> {
        let conn_arc = self.connection_for(route)?;
        let deadline = Instant::now() + budget;
        let conn = lock_until(&conn_arc, deadline).ok_or_else(|| CrossLimitError::LockTimeout {
            route: route.to_owned(),
            budget_ms: budget.as_millis() as u64,
        })?;
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        Self::run_check(
            &conn,
            &self.path,
            route,
            key,
            limit,
            &SystemTime::now,
            remaining,
        )
    }

    /// [`Self::check`], but bounds the in-process wait for the route's
    /// connection too, and runs the SQLite work off
    /// the async runtime.
    ///
    /// Two stages, each bounded: `tokio::time::timeout(budget, ...)` waits
    /// for the connection itself, so a caller that cannot even get the
    /// connection within `budget` returns [`CrossLimitError::LockTimeout`]
    /// without ever spending a blocking-pool thread; if it does get the
    /// connection, the remaining budget (what `budget` had left after that
    /// wait) is what bounds SQLite's own `busy_timeout`, inside
    /// [`tokio::task::spawn_blocking`].
    ///
    /// `clock` is read only after the write lock is held (see
    /// [`Self::run_check`]), never before waiting for it.
    pub(crate) async fn check_bounded(
        &self,
        route: &str,
        key: &str,
        limit: crate::RateLimit,
        clock: impl Fn() -> SystemTime + Send + 'static,
        budget: Duration,
    ) -> Result<Admission, CrossLimitError> {
        let conn_arc = self.connection_for(route)?;
        let wait_start = Instant::now();
        let guard = tokio::time::timeout(budget, conn_arc.lock_owned())
            .await
            .map_err(|_elapsed| CrossLimitError::LockTimeout {
                route: route.to_owned(),
                budget_ms: budget.as_millis() as u64,
            })?;
        let remaining = budget
            .saturating_sub(wait_start.elapsed())
            .max(Duration::from_millis(1));
        let path = self.path.clone();
        let route_owned = route.to_owned();
        let key_owned = key.to_owned();
        let route_for_panic = route.to_owned();
        tokio::task::spawn_blocking(move || {
            Self::run_check(
                &guard,
                &path,
                &route_owned,
                &key_owned,
                limit,
                &clock,
                remaining,
            )
        })
        .await
        .unwrap_or_else(|join_error| {
            Err(CrossLimitError::TaskFailed {
                route: route_for_panic,
                detail: join_error.to_string(),
            })
        })
    }

    /// The bounded, transactional GCRA check, against an already-acquired
    /// connection. `budget` bounds only SQLite's own wait for the write
    /// lock (`busy_timeout`); acquiring `conn` itself is the caller's job
    /// (see the module doc's
    /// "Bounded waiting" section).
    fn run_check(
        conn: &Connection,
        path: &Path,
        route: &str,
        key: &str,
        limit: crate::RateLimit,
        clock: &dyn Fn() -> SystemTime,
        budget: Duration,
    ) -> Result<Admission, CrossLimitError> {
        let row_key = format!("{route}:{key}");
        let wrap = |source| CrossLimitError::Sqlite {
            path: path.to_path_buf(),
            source,
        };
        conn.busy_timeout(budget).map_err(wrap)?;
        // No retry loop here: `busy_timeout` alone bounds the
        // whole wait for the write lock to `budget`, so one `SQLITE_BUSY` (or
        // `SQLITE_LOCKED`, a lock held by this same process on another
        // connection) after that is the budget being exhausted, reported as
        // `LockTimeout` rather than retried further.
        match conn.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => {}
            Err(rusqlite::Error::SqliteFailure(err, _))
                if matches!(
                    err.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                return Err(CrossLimitError::LockTimeout {
                    route: route.to_owned(),
                    budget_ms: budget.as_millis() as u64,
                });
            }
            Err(source) => return Err(wrap(source)),
        }

        // The clock is read here, with the write lock held, and not before
        // waiting for it. A `now` read before the wait can be older than a
        // reservation another caller committed meanwhile; the anomaly reset
        // below would then read that newer reservation as a clock jump and
        // erase it, admitting more calls than the burst allows.
        let now_us = match clock().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_micros() as i64,
            Err(_) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(CrossLimitError::ClockBeforeEpoch {
                    route: route.to_owned(),
                });
            }
        };

        let outcome = (|| -> rusqlite::Result<Admission> {
            let stored: Option<(i64, u32, u32)> = conn
                .query_row(
                    "SELECT tat_micros, rpm, burst FROM gcra WHERE key = ?1",
                    params![row_key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;

            // The row's own `rpm`/`burst` is
            // authoritative once anything has ever registered this key,
            // reflecting whichever configuration was opened most recently
            // (`register_limit`), never "whichever is stricter". `limit`
            // (this call's own configured value) is used only to seed a key
            // nothing has ever registered.
            let effective = match stored {
                Some((_, rpm, burst)) => crate::RateLimit {
                    requests_per_minute: rpm,
                    burst,
                    max_wait_ms: limit.max_wait_ms,
                },
                None => limit,
            };
            let period_us = 60_000_000_i64 / i64::from(effective.requests_per_minute.max(1));
            let tau_us = period_us * i64::from(effective.burst.saturating_sub(1));

            let raw_tat = stored.map(|(tat, ..)| tat).unwrap_or(now_us);
            // A stored `tat` further ahead of `now` than one full
            // `tau + period` is a clock anomaly (a clock that
            // ran fast and was then corrected, or was set fast by mistake),
            // not a real reservation this far out. Reset rather than honor
            // it, so a corrected clock does not lock the route out for the
            // size of the jump, on disk, forever.
            let reset_threshold = now_us.saturating_add(tau_us).saturating_add(period_us);
            let tat = if raw_tat > reset_threshold {
                now_us
            } else {
                raw_tat
            };

            let allow_at = tat - tau_us;
            if now_us < allow_at {
                Ok(Admission::Wait(Duration::from_micros(
                    (allow_at - now_us) as u64,
                )))
            } else {
                let new_tat = tat.max(now_us) + period_us;
                if stored.is_none() {
                    // Seed the row: the first-ever check on this key is also
                    // its first registration, so a
                    // later explicit `register_limit` can tell whether it is
                    // newer.
                    conn.execute(
                        "INSERT INTO gcra (key, tat_micros, rpm, burst, limit_set_at_micros) \
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            row_key,
                            new_tat,
                            effective.requests_per_minute,
                            effective.burst,
                            now_us
                        ],
                    )?;
                } else {
                    // Never touch `rpm`/`burst`/`limit_set_at_micros` here:
                    // only `register_limit` changes which configuration is
                    // in force. A check only ever
                    // advances `tat`.
                    conn.execute(
                        "UPDATE gcra SET tat_micros = ?2 WHERE key = ?1",
                        params![row_key, new_tat],
                    )?;
                }
                Ok(Admission::Admitted)
            }
        })();
        finish_transaction(conn, outcome, wrap)
    }
}

/// A best-effort, poll-based lock acquisition, bounded by `deadline`, safe to
/// call from any context, sync or async. `tokio::sync::Mutex::blocking_lock`
/// is not: it panics when called from inside a tokio runtime of any flavor
///, which [`SharedLimiterStore::check`]
/// and [`SharedLimiterStore::register_limit`] can both reach in practice
/// (`register_limit`, via `RouteLimiter::bind_key`, is itself synchronous but
/// can run inside an async task in production). `try_lock` never makes that
/// check, so polling it is safe everywhere, at the cost of a short sleep
/// between attempts rather than a true blocking wait.
fn lock_until(
    conn_arc: &Arc<tokio::sync::Mutex<Connection>>,
    deadline: Instant,
) -> Option<tokio::sync::MutexGuard<'_, Connection>> {
    loop {
        match conn_arc.try_lock() {
            Ok(guard) => return Some(guard),
            Err(_) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

/// `COMMIT` on success, `ROLLBACK` on any failure (a failed
/// `COMMIT` used to leave the transaction open on the connection). Shared by
/// every method that opens a `BEGIN IMMEDIATE` above.
fn finish_transaction<T>(
    conn: &Connection,
    outcome: rusqlite::Result<T>,
    wrap: impl FnOnce(rusqlite::Error) -> CrossLimitError,
) -> Result<T, CrossLimitError> {
    match outcome {
        Ok(value) => match conn.execute_batch("COMMIT") {
            Ok(()) => Ok(value),
            Err(source) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(wrap(source))
            }
        },
        Err(source) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(wrap(source))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RateLimit;

    const BUDGET: Duration = Duration::from_secs(1);

    fn limit(spec: &str) -> RateLimit {
        RateLimit::parse(spec).expect("spec")
    }

    /// `default_store_path` must never
    /// resolve the user's real per-user data directory from a test.
    #[test]
    #[should_panic(expected = "must never be called from a test")]
    fn default_store_path_refuses_to_resolve_under_cfg_test() {
        let _ = default_store_path();
    }

    #[test]
    fn a_key_id_never_carries_the_credential_and_is_stable() {
        let a = key_id("sk-real-secret-value");
        assert!(!a.contains("secret"));
        assert_eq!(a.len(), 16);
        assert_eq!(a, key_id("sk-real-secret-value"));
        assert_ne!(a, key_id("sk-a-different-value"));
    }

    #[test]
    fn check_now_reads_the_clock_under_the_lock_and_behaves_like_check() {
        let dir = tempfile::tempdir().expect("dir");
        let store = SharedLimiterStore::open(&dir.path().join("shared.sqlite3")).expect("open");
        assert_eq!(
            store
                .check_now("route", "key", limit("60"), BUDGET)
                .expect("first"),
            Admission::Admitted
        );
        assert!(matches!(
            store
                .check_now("route", "key", limit("60"), BUDGET)
                .expect("second"),
            Admission::Wait(_)
        ));
    }

    #[test]
    fn a_fresh_key_is_admitted_and_the_next_call_within_the_period_must_wait() {
        let dir = tempfile::tempdir().expect("dir");
        let store = SharedLimiterStore::open(&dir.path().join("shared.sqlite3")).expect("open");
        let now = SystemTime::now();
        assert_eq!(
            store
                .check("route", "key", limit("60"), now, BUDGET)
                .expect("first"),
            Admission::Admitted
        );
        match store
            .check("route", "key", limit("60"), now, BUDGET)
            .expect("second")
        {
            Admission::Wait(w) => assert!(w > Duration::ZERO && w <= Duration::from_secs(1)),
            Admission::Admitted => panic!("a second call inside the period must wait"),
        }
    }

    #[test]
    fn two_keys_on_one_route_do_not_share_a_bucket() {
        let dir = tempfile::tempdir().expect("dir");
        let store = SharedLimiterStore::open(&dir.path().join("shared.sqlite3")).expect("open");
        let now = SystemTime::now();
        assert_eq!(
            store
                .check("route", "a", limit("1"), now, BUDGET)
                .expect("a"),
            Admission::Admitted
        );
        assert_eq!(
            store
                .check("route", "b", limit("1"), now, BUDGET)
                .expect("b"),
            Admission::Admitted
        );
    }

    #[test]
    fn a_burst_of_more_than_one_admits_that_many_back_to_back() {
        let dir = tempfile::tempdir().expect("dir");
        let store = SharedLimiterStore::open(&dir.path().join("shared.sqlite3")).expect("open");
        let now = SystemTime::now();
        for _ in 0..5 {
            assert_eq!(
                store
                    .check("route", "key", limit("60/5"), now, BUDGET)
                    .expect("burst"),
                Admission::Admitted
            );
        }
        assert!(matches!(
            store
                .check("route", "key", limit("60/5"), now, BUDGET)
                .expect("sixth"),
            Admission::Wait(_)
        ));
    }

    #[test]
    fn reopening_the_same_file_sees_the_other_opens_state() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("shared.sqlite3");
        let now = SystemTime::now();
        {
            let store = SharedLimiterStore::open(&path).expect("open 1");
            assert_eq!(
                store
                    .check("route", "key", limit("1"), now, BUDGET)
                    .expect("first"),
                Admission::Admitted
            );
        }
        let reopened = SharedLimiterStore::open(&path).expect("open 2");
        assert!(matches!(
            reopened
                .check("route", "key", limit("1"), now, BUDGET)
                .expect("second, another process's view"),
            Admission::Wait(_)
        ));
    }

    /// A clock that ran an hour fast, corrected back to the
    /// true time, does not lock the route out for the size of the jump.
    #[test]
    fn a_backward_clock_correction_does_not_lock_the_route_out() {
        let dir = tempfile::tempdir().expect("dir");
        let store = SharedLimiterStore::open(&dir.path().join("shared.sqlite3")).expect("open");
        let now = SystemTime::now();
        let an_hour_fast = now + Duration::from_secs(3600);
        assert_eq!(
            store
                .check("route", "key", limit("60"), an_hour_fast, BUDGET)
                .expect("admitted an hour fast"),
            Admission::Admitted
        );
        // The clock is corrected back to the true time. Without the reset,
        // `tat` would still read about an hour in the future and every check
        // here would report a `Wait` of about an hour.
        assert_eq!(
            store
                .check("route", "key", limit("60"), now, BUDGET)
                .expect("admitted after the correction, not locked out for an hour"),
            Admission::Admitted
        );
    }

    /// A pre-1970 clock is a loud error, not a silent zero
    /// that would make a broken clock look like an empty bucket.
    #[test]
    fn a_pre_epoch_clock_is_a_loud_error_not_a_silent_zero() {
        let dir = tempfile::tempdir().expect("dir");
        let store = SharedLimiterStore::open(&dir.path().join("shared.sqlite3")).expect("open");
        let before_epoch = UNIX_EPOCH - Duration::from_secs(1);
        assert!(matches!(
            store.check("route", "key", limit("60"), before_epoch, BUDGET),
            Err(CrossLimitError::ClockBeforeEpoch { .. })
        ));
    }

    /// Another process holding the write lock (`BEGIN
    /// IMMEDIATE`, never committed) makes `check` return inside its own
    /// budget, not after SQLite's much longer internal default.
    #[test]
    fn a_held_write_lock_returns_within_its_budget() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("shared.sqlite3");
        let store = SharedLimiterStore::open(&path).expect("open");
        let holder = Connection::open(&path).expect("second connection");
        holder
            .busy_timeout(Duration::from_secs(5))
            .expect("busy_timeout");
        holder
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold the write lock");
        let budget = Duration::from_millis(200);
        let start = Instant::now();
        let result = store.check("route", "key", limit("60"), SystemTime::now(), budget);
        let elapsed = start.elapsed();
        drop(holder);
        assert!(
            matches!(result, Err(CrossLimitError::LockTimeout { .. })),
            "{result:?}"
        );
        // The defect this guards is a fixed five-second `busy_timeout` that
        // ignored the budget. Two seconds of slack still tells that apart, and
        // a loaded host (a full gate beside builders) cannot fail it.
        assert!(
            elapsed < budget + Duration::from_secs(2),
            "check took {elapsed:?}, over its {budget:?} budget plus slack"
        );
    }

    /// Another connection holding the write lock makes
    /// `open` return inside its bounded deadline (five seconds plus slack),
    /// not after an unbounded (attempts x busy_timeout) stall, and reports
    /// contention as `LockTimeout`, never `ReadOnly`.
    #[test]
    fn open_returns_within_its_deadline_against_a_held_write_lock() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("shared.sqlite3");
        // Prime the file (creates the schema, sets WAL) while nothing holds it.
        drop(SharedLimiterStore::open(&path).expect("prime"));
        let holder = Connection::open(&path).expect("holder connection");
        holder
            .busy_timeout(Duration::from_secs(30))
            .expect("busy_timeout");
        holder
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold the write lock");
        let start = Instant::now();
        let result = SharedLimiterStore::open(&path);
        let elapsed = start.elapsed();
        drop(holder);
        assert!(
            matches!(result, Err(CrossLimitError::LockTimeout { .. })),
            "{result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(7),
            "open took {elapsed:?}, over its ~5 s deadline plus slack"
        );
    }

    /// Every per-route connection sets
    /// `synchronous = NORMAL` itself (it is not sticky in the file the way
    /// `journal_mode` is), so a check on it never pays a full fsync
    /// regardless of contention.
    #[test]
    fn connection_for_sets_synchronous_normal_on_the_check_connection() {
        let dir = tempfile::tempdir().expect("dir");
        let store = SharedLimiterStore::open(&dir.path().join("shared.sqlite3")).expect("open");
        let conn_arc = store.connection_for("route").expect("connection");
        let conn = conn_arc.blocking_lock();
        let mode: i64 = conn
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .expect("read synchronous back");
        // SQLite's `PRAGMA synchronous` encoding: 0 = OFF, 1 = NORMAL,
        // 2 = FULL (SQLite's own default), 3 = EXTRA.
        assert_eq!(
            mode, 1,
            "connection_for must set synchronous=NORMAL, not SQLite's own FULL default"
        );
    }

    /// A route's limit can be raised again after being
    /// lowered, once reopened with a newer timestamp. The previous
    /// "stricter wins" rule could never do this: a route checked at 60/min
    /// stayed there forever even after being reconfigured to 600/min.
    #[test]
    fn a_raised_limit_takes_effect_after_reopening() {
        let dir = tempfile::tempdir().expect("dir");
        let store = SharedLimiterStore::open(&dir.path().join("shared.sqlite3")).expect("open");
        let t0 = SystemTime::now();

        // A process opens the route at 60/min and burns its one admission.
        let effective = store
            .register_limit("route", "key", limit("60"), t0)
            .expect("register 60/min");
        assert_eq!(effective, limit("60"));
        assert_eq!(
            store
                .check("route", "key", limit("60"), t0, BUDGET)
                .expect("burn the 60/min admission"),
            Admission::Admitted
        );

        // The config is changed and the route is reopened, a second later,
        // at 600/min: this must win over the earlier, still-live 60/min.
        let t1 = t0 + Duration::from_secs(1);
        let effective = store
            .register_limit("route", "key", limit("600"), t1)
            .expect("register 600/min");
        assert_eq!(
            effective,
            limit("600"),
            "the newer registration must win over the older one"
        );

        // At 60/min the very next check would have to wait close to a
        // second; at the raised 600/min rate it is admitted at once.
        assert_eq!(
            store
                .check("route", "key", limit("600"), t1, BUDGET)
                .expect("admitted at the raised rate"),
            Admission::Admitted
        );
    }

    /// An older registration arriving after a newer one
    /// (a stale process still starting up, or a clock skewed backward) does
    /// not un-do the newer configuration.
    #[test]
    fn an_older_registration_arriving_late_does_not_undo_a_newer_one() {
        let dir = tempfile::tempdir().expect("dir");
        let store = SharedLimiterStore::open(&dir.path().join("shared.sqlite3")).expect("open");
        let t0 = SystemTime::now();
        let t1 = t0 + Duration::from_secs(1);

        store
            .register_limit("route", "key", limit("600"), t1)
            .expect("register the newer limit first");
        let effective = store
            .register_limit("route", "key", limit("60"), t0)
            .expect("an older registration arrives late");
        assert_eq!(
            effective,
            limit("600"),
            "the already-newer registration must still win"
        );
    }

    /// The clock is read after the write lock is held, never before the wait
    /// for it. A `now` read earlier can predate a reservation another caller
    /// committed meanwhile, and the anomaly reset would then erase that
    /// reservation and over-admit.
    #[tokio::test]
    async fn the_clock_is_read_with_the_write_lock_held() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("shared.sqlite3");
        let store = SharedLimiterStore::open(&path).expect("open");
        let probe_path = path.clone();
        let held_at_read = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = held_at_read.clone();
        let clock = move || {
            // A second connection cannot take the write lock while the
            // checking transaction is open.
            let probe = Connection::open(&probe_path).expect("probe");
            probe.busy_timeout(Duration::ZERO).expect("timeout");
            let busy = probe.execute_batch("BEGIN IMMEDIATE").is_err();
            flag.store(busy, std::sync::atomic::Ordering::SeqCst);
            SystemTime::now()
        };
        store
            .check_bounded("route", "key", limit("60"), clock, BUDGET)
            .await
            .expect("check");
        assert!(
            held_at_read.load(std::sync::atomic::Ordering::SeqCst),
            "the clock must be read inside the write transaction"
        );
    }

    /// A full bucket answers `Wait` when the clock is read after the lock, so
    /// concurrent callers cannot exceed the burst.
    #[tokio::test]
    async fn concurrent_callers_never_exceed_the_burst() {
        let dir = tempfile::tempdir().expect("dir");
        let store = std::sync::Arc::new(
            SharedLimiterStore::open(&dir.path().join("shared.sqlite3")).expect("open"),
        );
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                store
                    .check_bounded("route", "key", limit("6/5"), SystemTime::now, BUDGET)
                    .await
                    .expect("check")
            }));
        }
        let mut admitted = 0;
        for t in tasks {
            if t.await.expect("join") == Admission::Admitted {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 5);
    }
}
