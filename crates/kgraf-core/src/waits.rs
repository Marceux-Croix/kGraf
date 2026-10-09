//! Bounded lock waits, and a record of what this process is waiting on.
//!
//! Every lock wait in kGraf ends: a file lock is taken with a `try_lock`
//! deadline loop ([`lock_file`]), a condition variable with `wait_timeout`.
//! A wait that reaches its bound fails with [`LockTimeout`], which names the
//! lock and how long the caller waited. A hang becomes an error a person can
//! read, never a silent stall.
//!
//! The record ([`report`]) lists the lock waits and child processes in
//! flight, with their age. A watchdog prints it when something takes too
//! long, so a stuck run shows what it was stuck on. Its cost: an entry is
//! added only when a lock is actually contended (the uncontended path
//! records nothing) and once per child process, next to a process spawn
//! that costs a thousand times more. One mutex round and one small
//! allocation per entry.

use std::collections::BTreeMap;
use std::fs::File;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A lock wait that reached its bound.
#[derive(Debug, Clone, thiserror::Error)]
#[error("waited {waited:?} for {lock} and gave up")]
pub struct LockTimeout {
    /// Which lock: its kind and the path or object it guards.
    pub lock: String,
    /// How long the caller waited.
    pub waited: Duration,
}

/// A file lock that was not taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error(transparent)]
    Timeout(#[from] LockTimeout),
    #[error("cannot lock {lock}: {source}")]
    Io {
        lock: String,
        #[source]
        source: std::io::Error,
    },
}

/// How a file lock is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileLockMode {
    Shared,
    Exclusive,
}

/// Take the lock on `file`, waiting at most `bound`. `lock` names it in the
/// error and in [`report`]. The first try costs one system call and records
/// nothing; only a contended lock sleeps (1 ms, doubling to 20 ms) and is
/// recorded while it waits. A sleeping retry rather than a blocking call on
/// a helper thread: a blocking `flock` cannot be cancelled, so the thread
/// and its later lock would outlive the timeout.
pub fn lock_file(
    file: &File,
    mode: FileLockMode,
    lock: &str,
    bound: Duration,
) -> Result<(), LockError> {
    let started = Instant::now();
    let mut pause = Duration::from_millis(1);
    let mut waiting: Option<Registration> = None;
    loop {
        let got = match mode {
            FileLockMode::Shared => file.try_lock_shared(),
            FileLockMode::Exclusive => file.try_lock(),
        };
        match got {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(LockError::Io {
                    lock: lock.to_owned(),
                    source,
                });
            }
        }
        let waited = started.elapsed();
        if waited >= bound {
            return Err(LockTimeout {
                lock: lock.to_owned(),
                waited,
            }
            .into());
        }
        if waiting.is_none() {
            waiting = Some(waiting_for_lock(lock));
        }
        std::thread::sleep(pause.min(bound - waited));
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

/// The instant `after` from now. A duration too large to add (a caller's
/// "no limit" such as `Duration::MAX`) gives a day from now: far past any
/// wait kGraf makes, and an instant that exists on every platform. A plain
/// `Instant::now() + after` panics on overflow.
pub fn deadline_after(after: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(after)
        .or_else(|| now.checked_add(Duration::from_secs(86_400)))
        .unwrap_or(now)
}

/// What a recorded entry is.
#[derive(Debug, Clone)]
enum InFlight {
    Lock { lock: String },
    Child { pid: u32, command: String },
}

#[derive(Debug)]
struct Entry {
    what: InFlight,
    since: Instant,
    thread: Option<String>,
}

#[derive(Default)]
struct Registry {
    entries: BTreeMap<u64, Entry>,
    /// Commands whose output pipes stayed open after their process tree
    /// ended (`crate::child_run`), with how many times.
    pipes_held: BTreeMap<String, u64>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    entries: BTreeMap::new(),
    pipes_held: BTreeMap::new(),
});
static NEXT: AtomicU64 = AtomicU64::new(1);

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    // An entry is plain data: a panic elsewhere leaves it consistent.
    REGISTRY.lock().unwrap_or_else(|p| p.into_inner())
}

/// A recorded entry; removed from the record when dropped.
#[derive(Debug)]
pub struct Registration(u64);

impl Drop for Registration {
    fn drop(&mut self) {
        registry().entries.remove(&self.0);
    }
}

fn register(what: InFlight) -> Registration {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let thread = std::thread::current().name().map(str::to_owned);
    registry().entries.insert(
        id,
        Entry {
            what,
            since: Instant::now(),
            thread,
        },
    );
    Registration(id)
}

/// Record that the calling thread waits for `lock`, until the result is
/// dropped.
pub fn waiting_for_lock(lock: &str) -> Registration {
    register(InFlight::Lock {
        lock: lock.to_owned(),
    })
}

/// Record a running child process, until the result is dropped.
pub fn child_running(pid: u32, command: &str) -> Registration {
    register(InFlight::Child {
        pid,
        command: command.to_owned(),
    })
}

/// Record that `command`'s output pipes stayed open after its process tree
/// ended: a process outside the tree holds a copy of a pipe end.
pub fn note_pipes_held(command: &str) {
    *registry().pipes_held.entry(command.to_owned()).or_insert(0) += 1;
}

/// How many times any command's pipes stayed open after its tree ended.
pub fn pipes_held_count() -> u64 {
    registry().pipes_held.values().sum()
}

/// The lock waits and child processes in flight, oldest first, one per
/// line, and the commands whose pipes stayed open. Empty when nothing is
/// recorded.
pub fn report() -> String {
    let reg = registry();
    let mut entries: Vec<&Entry> = reg.entries.values().collect();
    entries.sort_by_key(|e| e.since);
    let mut out = String::new();
    for e in entries {
        let age = e.since.elapsed();
        let thread = e.thread.as_deref().unwrap_or("unnamed thread");
        match &e.what {
            InFlight::Lock { lock } => {
                out.push_str(&format!("waiting {age:?} for {lock} ({thread})\n"));
            }
            InFlight::Child { pid, command } => {
                out.push_str(&format!(
                    "child pid {pid} running {age:?}: {command} ({thread})\n"
                ));
            }
        }
    }
    for (command, n) in &reg.pipes_held {
        out.push_str(&format!(
            "pipes held open after the tree ended ({n}x): {command}\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_file_lock_times_out_naming_the_lock_and_the_wait() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("l");
        let holder = File::create(&path).expect("create");
        holder.lock().expect("hold");
        let waiter = File::open(&path).expect("open");
        let started = Instant::now();
        let err = lock_file(
            &waiter,
            FileLockMode::Shared,
            "the test lock",
            Duration::from_millis(200),
        )
        .expect_err("held");
        assert!(started.elapsed() < Duration::from_secs(5));
        match err {
            LockError::Timeout(t) => {
                assert_eq!(t.lock, "the test lock");
                assert!(t.waited >= Duration::from_millis(200), "{t:?}");
            }
            other => panic!("{other}"),
        }
        holder.unlock().expect("unlock");
        lock_file(
            &waiter,
            FileLockMode::Exclusive,
            "the test lock",
            Duration::from_secs(5),
        )
        .expect("free");
    }

    #[test]
    fn a_deadline_for_a_huge_duration_does_not_panic() {
        let now = Instant::now();
        assert!(deadline_after(Duration::MAX) > now + Duration::from_secs(3600));
        let near = deadline_after(Duration::from_secs(5));
        assert!(near > now + Duration::from_secs(4) && near < now + Duration::from_secs(6));
    }

    #[test]
    fn a_wait_is_in_the_report_only_while_it_lasts() {
        let reg = waiting_for_lock("report probe lock");
        let child = child_running(4242, "probe --arg");
        let text = report();
        assert!(text.contains("for report probe lock"), "{text}");
        assert!(text.contains("pid 4242"), "{text}");
        assert!(text.contains("probe --arg"), "{text}");
        drop(reg);
        drop(child);
        let text = report();
        assert!(!text.contains("report probe lock"), "{text}");
        assert!(!text.contains("pid 4242"), "{text}");
    }
}
