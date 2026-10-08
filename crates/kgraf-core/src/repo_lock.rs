//! One lock per repository that serializes a worker's Bypass git writes
//! against every other git write kGraf makes there (decision 0094).
//!
//! **Modes.** Exclusive: a worker's protected git write with the integrity
//! check and restore that follow it, and the check (and restore) after a
//! Bypass shell command that changed the git directory. The shell command
//! itself runs without the lock, in parallel with other workers' commands
//! and kGraf's git. Shared: each git process kGraf itself runs on the
//! repository (merges into the integration branch and its view, ref and
//! branch updates, worktree add and remove, prune, the worker's status read,
//! the co-change history read). Each holder takes it before
//! `crate::host_git::command` builds the invocation, so the runner's config
//! listing and the command it prepares run in one hold. Because a Bypass
//! shell command holds no lock, it can still change the git configuration
//! between that listing and the command; that gap is open. kGraf's own
//! processes do not wait for each other (they already serialize where they
//! must: merges by a mutex and a compare-and-swap on the branch); they wait
//! only while an exclusive holder runs, and an exclusive taker waits until no
//! kGraf git process runs, so a check never runs while kGraf's git is
//! mid-write.
//!
//! **Granularity.** kGraf takes the shared lock per git process, never across
//! a multi-step operation, so no lock is ever taken while another is held in
//! the same thread (no nesting, no lock-order deadlock). A protected write
//! that opens between two steps of a merge sees the first step's result in
//! its snapshot, never as its own change.
//!
//! **Across processes.** An advisory `flock` (`File::lock`), so two kGraf
//! processes on one repository serialize as two threads of one do (each
//! acquisition opens its own file description, and flock locks of separate
//! descriptions conflict in one process too). On Unix the lock is taken on
//! the shared git directory itself, not on a file inside it: a worker's
//! sandbox may write inside that directory while its command runs, and
//! could delete or replace a lock file there, after which two takers would
//! lock different files. The directory cannot be
//! removed or renamed from inside the sandbox (its parent is not writable),
//! so its lock cannot be dodged. On Windows, where a directory cannot be
//! locked, the lock is `kgraf-repo.lock` in that directory, opened without
//! delete sharing, so it cannot be deleted or renamed while held.
//!
//! **Stale locks.** None to judge: the kernel releases a flock when the last
//! descriptor of its description closes, which a crash or a kill does. A
//! crashed holder's lock is free the moment its process is gone; no PID file
//! is read and none can be left behind. A holder that hangs while alive is
//! not stale: waiters time out and fail loud. Known limit: on a network file
//! system without flock support the lock may not exclude another machine.
//!
//! **Waits** are bounded ([`DEFAULT_TIMEOUT`] unless the caller gives one);
//! a wait that times out fails loud, naming the holders in this process, if
//! any. No fairness is promised between shared and exclusive takers: kGraf's
//! git processes are short, so an exclusive taker finds a gap; a starved one
//! times out loud rather than waits forever.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::flock::HeldLock;

/// The longest a taker waits. Longer than the shell tool's command timeout
/// (10 minutes), since a kGraf merge may wait out a worker's whole git
/// command and its check.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The lock file on Windows, inside the shared git directory.
#[cfg(windows)]
pub const WINDOWS_LOCK_FILE: &str = "kgraf-repo.lock";

/// How a lock is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// kGraf's own git process: many at once, none while an exclusive
    /// holder runs.
    Shared,
    /// A worker's protected write, or the check after a Bypass shell
    /// command that changed the git directory: alone.
    Exclusive,
}

/// Why a lock was not taken.
#[derive(Debug, thiserror::Error)]
pub enum RepoLockError {
    #[error("cannot open the repository lock on {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "waited {waited:?} for the repository lock on {path} ({mode:?}) and gave up: {holders}"
    )]
    TimedOut {
        path: PathBuf,
        mode: Mode,
        waited: Duration,
        holders: String,
    },
}

/// A held repository lock; released on drop.
#[derive(Debug)]
pub struct RepoLockGuard {
    _file: HeldLock,
    key: PathBuf,
    id: u64,
}

impl Drop for RepoLockGuard {
    fn drop(&mut self) {
        let mut holders = HOLDERS.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(list) = holders.get_mut(&self.key) {
            list.retain(|h| h.id != self.id);
            if list.is_empty() {
                holders.remove(&self.key);
            }
        }
    }
}

struct Holder {
    id: u64,
    mode: Mode,
    purpose: String,
    since: Instant,
}

/// The holders in this process, by lock path, for the timeout message only.
/// Another process's holders are not known here.
static HOLDERS: Mutex<BTreeMap<PathBuf, Vec<Holder>>> = Mutex::new(BTreeMap::new());
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Take the lock of the repository whose shared git directory is
/// `common_dir`, waiting at most `timeout`. `purpose` names the holder in
/// another waiter's timeout message.
pub fn acquire(
    common_dir: &Path,
    mode: Mode,
    purpose: &str,
    timeout: Duration,
) -> Result<RepoLockGuard, RepoLockError> {
    let path = lock_path(common_dir);
    let file = open(&path).map_err(|source| RepoLockError::Open {
        path: path.clone(),
        source,
    })?;
    let started = Instant::now();
    let mut pause = Duration::from_millis(1);
    // In the record of lock waits (`crate::waits::report`) once contended.
    let mut waiting = None;
    loop {
        let got = match mode {
            Mode::Shared => file.try_lock_shared(),
            Mode::Exclusive => file.try_lock(),
        };
        match got {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(RepoLockError::Open { path, source });
            }
        }
        let waited = started.elapsed();
        if waited >= timeout {
            return Err(RepoLockError::TimedOut {
                holders: describe_holders(&path),
                path,
                mode,
                waited,
            });
        }
        waiting.get_or_insert_with(|| {
            crate::waits::waiting_for_lock(&format!(
                "the repository lock {} ({mode:?}) for {purpose}",
                path.display()
            ))
        });
        std::thread::sleep(pause.min(timeout - waited));
        pause = (pause * 2).min(Duration::from_millis(20));
    }
    drop(waiting);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    HOLDERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(path.clone())
        .or_default()
        .push(Holder {
            id,
            mode,
            purpose: purpose.to_owned(),
            since: Instant::now(),
        });
    Ok(RepoLockGuard {
        _file: HeldLock::new(file),
        key: path,
        id,
    })
}

/// [`acquire`] in [`Mode::Shared`] for a git process kGraf runs with its
/// working directory at `cwd`: no lock (`Ok(None)`) when `cwd` is in no
/// repository ([`common_dir_of`]).
pub fn shared_for(cwd: &Path, purpose: &str) -> Result<Option<RepoLockGuard>, RepoLockError> {
    match common_dir_of(cwd) {
        Some(common) => acquire(&common, Mode::Shared, purpose, DEFAULT_TIMEOUT).map(Some),
        None => Ok(None),
    }
}

fn describe_holders(path: &Path) -> String {
    let holders = HOLDERS.lock().unwrap_or_else(|p| p.into_inner());
    match holders.get(path) {
        Some(list) if !list.is_empty() => {
            let parts: Vec<String> = list
                .iter()
                .map(|h| format!("{} ({:?}, held {:?})", h.purpose, h.mode, h.since.elapsed()))
                .collect();
            format!("held in this process by {}", parts.join(", "))
        }
        _ => "no holder in this process: another process holds it (another kGraf run on this \
              repository)"
            .into(),
    }
}

#[cfg(unix)]
fn lock_path(common_dir: &Path) -> PathBuf {
    common_dir.to_path_buf()
}

#[cfg(windows)]
fn lock_path(common_dir: &Path) -> PathBuf {
    common_dir.join(WINDOWS_LOCK_FILE)
}

#[cfg(unix)]
fn open(path: &Path) -> std::io::Result<File> {
    // Opening a directory read-only is allowed on Unix and gives a
    // descriptor flock accepts. Close-on-exec, as std opens every file.
    File::open(path)
}

#[cfg(windows)]
fn open(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_SHARE_READ | FILE_SHARE_WRITE, without FILE_SHARE_DELETE: the
    // file cannot be deleted or renamed while any taker has it open.
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0x1 | 0x2)
        .open(path)
}

/// The shared git directory of the repository that holds `dir`, found the
/// way git discovers one, from files alone (no git process, so a git runner
/// can call it before each command): the nearest `.git` above `dir`, a
/// directory (a main checkout) or a `gitdir:` file (a linked worktree, whose
/// admin directory's `commondir` names the shared one); or `dir` itself, or
/// an ancestor, when it is a git directory (`HEAD` and `objects` in it; an
/// admin directory with `commondir`). `None` outside any repository.
pub fn common_dir_of(dir: &Path) -> Option<PathBuf> {
    let start = crate::plain_path::canonicalize(dir).ok()?;
    for cur in start.ancestors() {
        if let Some(common) = git_dir_common(cur) {
            return Some(common);
        }
        let dot = cur.join(".git");
        match std::fs::symlink_metadata(&dot) {
            Ok(m) if m.is_dir() => return git_dir_common(&dot),
            Ok(m) if m.is_file() => {
                let text = std::fs::read_to_string(&dot).ok()?;
                let target = Path::new(text.trim().strip_prefix("gitdir:")?.trim());
                let admin = if target.is_absolute() {
                    target.to_path_buf()
                } else {
                    cur.join(target)
                };
                return git_dir_common(&admin);
            }
            _ => {}
        }
    }
    None
}

/// The shared directory of the git directory `dir`, when it is one.
fn git_dir_common(dir: &Path) -> Option<PathBuf> {
    if !dir.join("HEAD").is_file() {
        return None;
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("commondir")) {
        let rel = Path::new(text.trim());
        let common = if rel.is_absolute() {
            rel.to_path_buf()
        } else {
            dir.join(rel)
        };
        return crate::plain_path::canonicalize(common).ok();
    }
    if dir.join("objects").is_dir() {
        return crate::plain_path::canonicalize(dir).ok();
    }
    None
}

#[cfg(test)]
mod tests;
