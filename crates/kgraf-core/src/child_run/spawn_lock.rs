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
//! Only callers that take the lock are covered. A spawn through std or tokio
//! elsewhere in the process does not take it, and can still copy a handle
//! during the window in which it is inheritable (the one creation call).
//!
//! The wait is bounded ([`SPAWN_LOCK_WAIT`]) and fails with
//! [`crate::waits::LockTimeout`], like every lock wait in kGraf.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::waits::LockTimeout;

/// The longest a caller waits for the spawn lock. A holder makes one process
/// creation call: milliseconds for `CreateProcess`, and for a command that
/// runs as the Windows sandbox user one `CreateProcessWithLogonW`, which logs
/// the user on and takes up to a few seconds on a loaded machine. 60 s covers
/// a queue of a dozen such holders, so a wait this long means a holder that
/// is stuck in the call, not a slow one.
pub const SPAWN_LOCK_WAIT: Duration = Duration::from_secs(60);

/// What the lock is called in [`LockTimeout`] and in [`crate::waits::report`].
pub const SPAWN_LOCK_NAME: &str = "the process spawn lock";

struct State {
    held: Mutex<bool>,
    freed: Condvar,
}

static STATE: State = State {
    held: Mutex::new(false),
    freed: Condvar::new(),
};

fn held() -> MutexGuard<'static, bool> {
    // A bool cannot be left inconsistent by a panic elsewhere.
    STATE.held.lock().unwrap_or_else(|p| p.into_inner())
}

/// The spawn lock, held until dropped.
#[derive(Debug)]
#[must_use = "the lock is released when this is dropped"]
pub struct SpawnLock(());

impl Drop for SpawnLock {
    fn drop(&mut self) {
        *held() = false;
        STATE.freed.notify_one();
    }
}

/// Take the spawn lock, waiting at most `bound`. Not reentrant: a holder
/// that asks again waits for itself until `bound`.
pub fn spawn_lock(bound: Duration) -> Result<SpawnLock, LockTimeout> {
    let started = std::time::Instant::now();
    let mut guard = held();
    if *guard {
        // Recorded only while contended, as `waits::lock_file` does.
        let _waiting = crate::waits::waiting_for_lock(SPAWN_LOCK_NAME);
        let (g, result) = STATE
            .freed
            .wait_timeout_while(guard, bound, |h| *h)
            .unwrap_or_else(|p| p.into_inner());
        guard = g;
        if result.timed_out() && *guard {
            return Err(LockTimeout {
                lock: SPAWN_LOCK_NAME.to_owned(),
                waited: started.elapsed(),
            });
        }
    }
    *guard = true;
    Ok(SpawnLock(()))
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
}
