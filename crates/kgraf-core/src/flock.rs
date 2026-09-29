//! File locks that are released for every process when their holder lets go.
//!
//! An `flock` belongs to the open file description, not to one descriptor or
//! one process (flock(2)). A descriptor opened close-on-exec is still copied
//! into every child forked while it is open, and the copy lives until that
//! child execs: the time a sandboxed spawn spends in its `pre_exec` hook
//! (setsid, taking the worktree hold, the Seatbelt or Landlock step), longer on
//! a loaded machine. Closing the parent's descriptor inside that window does
//! not release the lock; the child's copy keeps it until the exec.
//!
//! What that did before this type (moved here from `kgraf-sandbox` so every
//! crate that locks a file releases it the same way):
//!
//! - **False busy reads.** A probe or guard of a worktree hold that only closed
//!   its `File` could leave the hold locked for a moment after it let go, and
//!   the next probe read the leftover as a live holder. In a contract check that
//!   was "a process the check started left its process group" (the check could
//!   not run and its interface's parties were held); for a worktree, a spurious
//!   busy. Measured on macOS: 2,369 of 20,000 probes of a free hold read it as
//!   held while four threads spawned commands with a 5 ms `pre_exec`.
//! - **A permanent deadlock.** A child whose `pre_exec` takes the hold shared
//!   (`LOCK_SH`, every sandboxed spawn) could inherit the parent's copy of an
//!   exclusive lock on that same hold (a quiesce guard held at the fork). When
//!   the guard was dropped the parent's close released nothing, because the
//!   child's own copy still held the exclusive lock its `LOCK_SH` waited on: the
//!   child waited on itself, and `spawn()`, which waits for the child's exec,
//!   never returned.
//!
//! `flock(LOCK_UN)` through any descriptor of the description releases the lock
//! for all of them, so [`HeldLock`] unlocks explicitly before it closes. A child
//! forked in between keeps a descriptor that holds nothing.
//!
//! Rejected: serializing lock releases against forks with a process-wide
//! read-write lock. Every fork-path spawn would have to take it, and a spawn
//! whose child waits in `pre_exec` for a hold that a guard keeps would deadlock
//! against that guard's release.

use std::fs::File;

/// A file whose lock is released with an explicit unlock on drop, so no
/// descriptor copied into a concurrently forked child keeps it.
#[derive(Debug)]
pub struct HeldLock(File);

impl HeldLock {
    /// Keep `file`, which already holds its lock (shared or exclusive).
    pub fn new(file: File) -> Self {
        Self(file)
    }

    /// The locked file, to write through while the lock is held.
    pub fn file(&self) -> &File {
        &self.0
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        // If the unlock fails, the close releases the lock as it did before,
        // once no forked copy of the descriptor is left.
        let _ = self.0.unlock();
    }
}

/// Threads that spawn commands through the fork path (a `pre_exec` hook, as
/// every sandboxed spawn has) with a slow hook, so some child is nearly always
/// between fork and exec holding copies of the parent's descriptors. For tests
/// of lock release (feature `test-support`); stopped and joined on drop.
#[cfg(all(unix, any(test, feature = "test-support")))]
pub struct Forkers {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

#[cfg(all(unix, any(test, feature = "test-support")))]
impl Forkers {
    /// Start `n` spawning threads, and give the first children time to reach
    /// their hooks.
    pub fn start(n: usize) -> Self {
        use std::os::unix::process::CommandExt;
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let threads = (0..n)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let mut c = std::process::Command::new("true");
                        // SAFETY: the hook only sleeps; `std::thread::sleep`
                        // is `nanosleep` in a loop, async-signal-safe and
                        // allocation-free, and touches no parent state.
                        unsafe {
                            c.pre_exec(|| {
                                std::thread::sleep(std::time::Duration::from_millis(5));
                                Ok(())
                            });
                        }
                        let _ = c.status();
                    }
                })
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(20));
        Self { stop, threads }
    }
}

#[cfg(all(unix, any(test, feature = "test-support")))]
impl Drop for Forkers {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{Forkers, HeldLock};

    /// A released exclusive lock is free for the next taker at once while
    /// other threads fork: no forked copy of the descriptor keeps it.
    #[test]
    fn a_released_lock_is_free_at_once_while_other_threads_fork() {
        let dir = std::env::temp_dir().join(format!("kgraf-flock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("hold");
        std::fs::File::create(&path).expect("hold");
        let _forkers = Forkers::start(4);
        let mut still_held = 0;
        for _ in 0..2_000 {
            let f = std::fs::File::open(&path).expect("open");
            f.try_lock()
                .unwrap_or_else(|e| panic!("the lock is free: {e}"));
            drop(HeldLock::new(f));
            let next = std::fs::File::open(&path).expect("open");
            match next.try_lock() {
                Ok(()) => drop(HeldLock::new(next)),
                Err(_) => still_held += 1,
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(still_held, 0, "a released lock read as held");
    }
}
