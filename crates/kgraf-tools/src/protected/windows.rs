//! The Bypass shell commands running on each repository, for attribution
//! (decision 0094). Bypass shell commands run in parallel, each with the
//! shared git directory writable and no lock held while it runs, so a change
//! found after one command may be another's. This registry is how a check
//! says so.
//!
//! **What it holds,** per repository (its shared git directory): every open
//! window (a command from its pre-state to the end of its check) with its
//! worker and pre-state, and the git files' bytes after the latest passing
//! check.
//!
//! **The rules it serves.**
//!
//! - *Who may have made a change.* A check that finds a change names every
//!   other window still open whose pre-state differs from what the check
//!   found in the changed files (or every open window, when object files
//!   were lost): those commands started before the change was there, so any
//!   of them may have made it. A command that ended before the change was
//!   found is not named: its own check ran when it had ended, so it either
//!   checked that change itself or the change came later.
//! - *Kept, cause not exclusive.* A passing check that names other windows
//!   records its outcome with [`super::NOT_EXCLUSIVE`] and their workers.
//! - *Already checked.* A change a window finds is not checked again, and
//!   not recorded again, when every changed file is exactly as a check that
//!   passed after the window opened left it: that state was verified, and
//!   recorded (with this window named, if it may have made it) by the check
//!   that verified it.
//! - *Who fails on damage.* The attempt whose check found the damage fails,
//!   and each named window is marked before anything is restored; its attempt
//!   fails when it closes, whatever it changed, since the restore can erase
//!   the trace of the write that caused the damage. Marking before the
//!   restore matters: a named command that ends during the restore sees the
//!   damage still there, waits for the lock, and finds its mark.
//!
//! **Scope.** In this process only. A second kGraf process on the same
//! repository keeps its own registry: its checks are serialized with this
//! process's by the repository lock, and a change it makes is checked by
//! whichever window sees it, but its commands are not named here.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use super::objects::Stamp;

/// One git file as a pre-state holds it.
#[derive(Clone, Debug)]
pub(crate) struct PreFile {
    pub(crate) stamp: Stamp,
    pub(crate) bytes: Arc<Vec<u8>>,
    /// The stamp was settled when the bytes were read
    /// ([`Stamp::settled`]): an equal stamp later means equal bytes. When
    /// not, the bytes are compared instead.
    pub(crate) trusted: bool,
}

/// A window's view of the git files when its command started.
#[derive(Debug, Default)]
pub(crate) struct Pre {
    pub(crate) files: BTreeMap<PathBuf, PreFile>,
}

impl Pre {
    pub(crate) fn bytes(&self, path: &Path) -> Option<&[u8]> {
        self.files.get(path).map(|f| f.bytes.as_slice())
    }
}

/// The git files' bytes after a passing check (a file not in it was absent).
pub(crate) type Checked = BTreeMap<PathBuf, Arc<Vec<u8>>>;

/// Each changed file's bytes as a check found them (`None`: absent).
pub(crate) type Found = BTreeMap<PathBuf, Option<Vec<u8>>>;

struct Open {
    worker: String,
    opened: u64,
    pre: Arc<Pre>,
    /// Why this window's attempt fails when it closes: another window's
    /// check found damage while this command ran.
    suspect: Vec<String>,
}

#[derive(Default)]
struct Repo {
    seq: u64,
    next_id: u64,
    open: BTreeMap<u64, Open>,
    checked: Option<(u64, Arc<Checked>)>,
}

impl Repo {
    fn tick(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

static REPOS: Mutex<BTreeMap<PathBuf, Repo>> = Mutex::new(BTreeMap::new());

fn repos() -> MutexGuard<'static, BTreeMap<PathBuf, Repo>> {
    REPOS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Register a window whose pre-state is `pre`; returns its id.
pub(crate) fn open(common: &Path, worker: &str, pre: Arc<Pre>) -> u64 {
    let mut all = repos();
    let repo = all.entry(common.to_path_buf()).or_default();
    let opened = repo.tick();
    repo.next_id += 1;
    let id = repo.next_id;
    repo.open.insert(
        id,
        Open {
            worker: worker.to_owned(),
            opened,
            pre,
            suspect: Vec::new(),
        },
    );
    id
}

/// Remove window `id`; returns why its attempt fails (empty: it does not),
/// from damage another window's check found while it ran.
pub(crate) fn close(common: &Path, id: u64) -> Vec<String> {
    let mut all = repos();
    let Some(repo) = all.get_mut(common) else {
        return Vec::new();
    };
    let suspect = repo.open.remove(&id).map(|o| o.suspect).unwrap_or_default();
    if repo.open.is_empty() {
        all.remove(common);
    }
    suspect
}

/// `found` with every file dropped that is exactly as a check that passed
/// after window `id` opened left it: that change was already checked.
pub(crate) fn not_yet_checked(common: &Path, id: u64, found: Found) -> Found {
    let all = repos();
    let Some(repo) = all.get(common) else {
        return found;
    };
    let (Some(me), Some((at, checked))) = (repo.open.get(&id), repo.checked.as_ref()) else {
        return found;
    };
    if *at <= me.opened {
        return found;
    }
    found
        .into_iter()
        .filter(|(path, bytes)| checked.get(path).map(|b| b.as_slice()) != bytes.as_deref())
        .collect()
}

/// Record the git files' bytes after a passing check. Kept only while a
/// window is open (with none, nothing can use it).
pub(crate) fn passed(common: &Path, files: Checked) {
    let mut all = repos();
    if let Some(repo) = all.get_mut(common) {
        let at = repo.tick();
        repo.checked = Some((at, Arc::new(files)));
    }
}

/// The workers of the other open windows that may have made the change a
/// check by `finder` (window `me`, when it is one) found: `found` (`what`
/// names it for people), and whether object files were lost. With
/// `damage`, each is marked: its attempt fails when it closes.
pub(crate) fn others(
    common: &Path,
    me: Option<u64>,
    finder: &str,
    what: &str,
    found: &Found,
    objects_lost: bool,
    damage: bool,
) -> Vec<String> {
    let mut all = repos();
    let Some(repo) = all.get_mut(common) else {
        return Vec::new();
    };
    let mut names = BTreeSet::new();
    for (id, open) in repo.open.iter_mut() {
        if Some(*id) == me {
            continue;
        }
        let arrived = objects_lost
            || found
                .iter()
                .any(|(path, now)| open.pre.bytes(path) != now.as_deref());
        if !arrived {
            continue;
        }
        if damage {
            open.suspect.push(format!(
                "{finder}'s check found the repository's git directory damaged ({what}) while \
                 this command ran, and restored it"
            ));
        }
        names.insert(open.worker.clone());
    }
    names.into_iter().collect()
}

/// The open windows on `common`, for tests.
#[cfg(test)]
pub(crate) fn open_count(common: &Path) -> usize {
    repos().get(common).map_or(0, |r| r.open.len())
}
