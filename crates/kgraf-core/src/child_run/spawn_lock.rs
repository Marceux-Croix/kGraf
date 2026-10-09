//! One process-wide lock around process creation that makes handles
//! inheritable.
//!
//! On Windows a child created with handle inheritance (std's spawn, which
//! passes `bInheritHandles = TRUE` with no handle list) receives a copy of
//! every inheritable handle in this process at that instant, not only its
//! own. kGraf's sandbox creates its children's pipe ends non-inheritable and
//! marks them inheritable only for the one creation call that passes them,
//! under this lock; [`super::run`] takes the same lock around its spawn. So
//! neither can copy the other's pipe ends, and a reader never waits for an
//! unrelated process that holds one.
//!
//! The other direction matters as much: during its own spawn std marks its
//! child's pipe ends inheritable, and a `CreateProcessWithLogonW` call made
//! at that moment (the Windows sandbox user's runner) copies them, so that
//! child's reader then waits as long as the runner lives.
//!
//! So every process creation in kGraf takes the lock: through [`super::run`],
//! through [`spawn_locked`] (std's, tokio's, process-wrap's and rmcp's
//! spawns) or [`super::spawn`], [`super::output`] and [`super::status`] (std
//! commands), and the sandbox's own creation calls. A test
//! (`child_run::spawn_sites`) reads every crate's source and fails on a
//! spawn that is not one of these, unless it is listed there with the reason
//! it cannot overlap a sandboxed command's start.
//!
//! The wait is bounded ([`SPAWN_LOCK_WAIT`]) and fails with
//! [`crate::waits::LockTimeout`], like every lock wait in kGraf.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::waits::LockTimeout;

/// Make one process creation call, `spawn`, under the spawn lock, waiting
/// for the lock at most [`SPAWN_LOCK_WAIT`]; a wait that runs out is an
/// error of kind `TimedOut` naming the lock. `spawn` must create the process
/// and return: std's or tokio's `Command::spawn`, process-wrap's, rmcp's
/// child builder. Never `output` or `status`, which would hold the lock until
/// the child ends; [`super::output`] and [`super::status`] split them.
pub fn spawn_locked<T>(spawn: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    let _lock = spawn_lock(SPAWN_LOCK_WAIT)
        .map_err(|timeout| std::io::Error::new(std::io::ErrorKind::TimedOut, timeout))?;
    spawn()
}

/// The longest a caller waits for the spawn lock. A holder makes one process
/// creation call: milliseconds for `CreateProcess`, and for a command that
/// runs as the Windows sandbox user one `CreateProcessWithLogonW`, which logs
/// the user on and takes up to a few seconds on a loaded machine. 60 s covers
/// a queue of a dozen such holders, so a wait this long means a holder that
/// is stuck in the call, not a slow one.
pub const SPAWN_LOCK_WAIT: Duration = Duration::from_secs(60);

/// What the lock is called in [`LockTimeout`] and in [`crate::waits::report`].
pub const SPAWN_LOCK_NAME: &str = "the process spawn lock";

/// How the spawn lock has been used in this process since it started: how
/// often it was taken, the longest and total wait for it, the longest hold,
/// and how many waits ran out. For diagnostics: a longest wait near
/// [`SPAWN_LOCK_WAIT`] says holders queue for too long.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpawnLockStats {
    pub taken: u64,
    pub timeouts: u64,
    pub waited_total: Duration,
    pub waited_max: Duration,
    pub held_max: Duration,
}

struct Inner {
    held: bool,
    stats: SpawnLockStats,
}

struct State {
    inner: Mutex<Inner>,
    freed: Condvar,
}

static STATE: State = State {
    inner: Mutex::new(Inner {
        held: false,
        stats: SpawnLockStats {
            taken: 0,
            timeouts: 0,
            waited_total: Duration::ZERO,
            waited_max: Duration::ZERO,
            held_max: Duration::ZERO,
        },
    }),
    freed: Condvar::new(),
};

fn inner() -> MutexGuard<'static, Inner> {
    // A flag and counters cannot be left inconsistent by a panic elsewhere.
    STATE.inner.lock().unwrap_or_else(|p| p.into_inner())
}

/// The spawn lock, held until dropped.
#[derive(Debug)]
#[must_use = "the lock is released when this is dropped"]
pub struct SpawnLock {
    since: Instant,
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        let held = self.since.elapsed();
        let mut s = inner();
        s.held = false;
        s.stats.held_max = s.stats.held_max.max(held);
        drop(s);
        STATE.freed.notify_one();
    }
}

/// Take the spawn lock, waiting at most `bound`. Not reentrant: a holder
/// that asks again waits for itself until `bound`.
pub fn spawn_lock(bound: Duration) -> Result<SpawnLock, LockTimeout> {
    let started = Instant::now();
    let mut guard = inner();
    if guard.held {
        // Recorded only while contended, as `waits::lock_file` does.
        let _waiting = crate::waits::waiting_for_lock(SPAWN_LOCK_NAME);
        let (g, result) = STATE
            .freed
            .wait_timeout_while(guard, bound, |s| s.held)
            .unwrap_or_else(|p| p.into_inner());
        guard = g;
        if result.timed_out() && guard.held {
            guard.stats.timeouts += 1;
            return Err(LockTimeout {
                lock: SPAWN_LOCK_NAME.to_owned(),
                waited: started.elapsed(),
            });
        }
    }
    let waited = started.elapsed();
    guard.held = true;
    guard.stats.taken += 1;
    guard.stats.waited_total += waited;
    guard.stats.waited_max = guard.stats.waited_max.max(waited);
    Ok(SpawnLock {
        since: Instant::now(),
    })
}

/// The spawn lock's use so far in this process ([`SpawnLockStats`]).
pub fn spawn_lock_stats() -> SpawnLockStats {
    inner().stats
}

/// Tests that hold the one process-wide lock run one at a time.
#[cfg(test)]
pub(super) static TEST_SERIAL: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::TEST_SERIAL as SERIAL;
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn a_held_spawn_lock_times_out_naming_the_lock() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let holder = spawn_lock(Duration::from_secs(5)).expect("free");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(spawn_lock(Duration::from_millis(200)));
        });
        let err = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the wait ended")
            .expect_err("held");
        assert_eq!(err.lock, SPAWN_LOCK_NAME);
        assert!(err.waited >= Duration::from_millis(200), "{err:?}");
        drop(holder);
    }

    #[test]
    fn a_waiter_gets_the_spawn_lock_when_it_is_released() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let holder = spawn_lock(Duration::from_secs(5)).expect("free");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let got = spawn_lock(Duration::from_secs(10)).map(drop);
            let _ = tx.send(got);
        });
        // The waiter is blocked while the lock is held.
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        drop(holder);
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the waiter returned")
            .expect("and took the lock");
        // Released again by the waiter's drop.
        drop(spawn_lock(Duration::from_millis(100)).expect("free again"));
    }

    #[test]
    fn the_stats_count_takes_waits_holds_and_timeouts() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let before = spawn_lock_stats();
        let holder = spawn_lock(Duration::from_secs(5)).expect("free");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let short = spawn_lock(Duration::from_millis(50)).map(drop);
            let long = spawn_lock(Duration::from_secs(10)).map(drop);
            let _ = tx.send((short, long));
        });
        std::thread::sleep(Duration::from_millis(300));
        drop(holder);
        let (short, long) = rx.recv_timeout(Duration::from_secs(10)).expect("ended");
        assert!(short.is_err() && long.is_ok());
        let after = spawn_lock_stats();
        // At least: other tests of this binary spawn through the lock too.
        assert!(after.taken - before.taken >= 2, "{before:?} {after:?}");
        assert!(after.timeouts > before.timeouts, "{before:?} {after:?}");
        // The second waiter waited for the rest of the 300 ms hold.
        assert!(after.waited_max >= Duration::from_millis(200), "{after:?}");
        assert!(after.held_max >= Duration::from_millis(300), "{after:?}");
        assert!(after.waited_total >= before.waited_total + Duration::from_millis(200));
    }
}
