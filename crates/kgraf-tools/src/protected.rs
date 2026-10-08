//! Worker writes to protected paths, allowed in the Bypass permission mode
//! only, each one guarded by a snapshot and an integrity check (decision 0094).
//!
//! **What a worker may write in Bypass.** Two areas, named as the worker sees
//! its worktree, which is a checkout of the user's project:
//!
//! - `.git/<path>`: the repository's git directory. A worker worktree's `.git`
//!   is a file pointing at its admin directory inside the shared repository,
//!   so the path is resolved the way git resolves it
//!   (`git rev-parse --git-path <path>`, with the admin directory recorded when
//!   the worktree was made, never the file on disk): `HEAD` and `index` land in
//!   this worktree's admin directory, `config`, `info/exclude` and `refs/...`
//!   in the shared directory. `.git` itself (the link) and `.git/objects` are
//!   refused to `apply_patch`: rewriting the link aims later git commands at
//!   another repository, and objects are compressed binary files git writes
//!   (any shell command in Bypass, whose window snapshots the store).
//! - `.kgraf/run.json` and `.kgraf/worker.json`: kGraf's settings in the
//!   project's state directory. Nothing else under `.kgraf`: the spend ledger,
//!   the trace, run records, worker transcripts, the graph, the schedule, the
//!   persisted allow rules and the worktrees are kGraf's own books, refused in
//!   every mode, Bypass included.
//!
//! - `.gitattributes`, `.gitmodules` and anything under `.githooks`
//!   ([`Area::Tracked`]): ordinary tracked files in the worktree, written
//!   there with no snapshot (git history keeps every earlier version) and
//!   under the declared write scope like any file. kGraf's own git never
//!   runs a hook, filter or driver they select (`kgraf_core::host_git`).
//!
//! Every other protected name (`.gitconfig`, a nested `.git`, the former
//! state directory, an NTFS short name of a protected name) stays refused in
//! every mode, as do paths that leave the worktree. In every mode other than
//! Bypass, every protected path stays refused with the reason
//! [`kgraf_core::paths::forbidden_reason`] gives.
//!
//! **Protection against a faulty write.**
//!
//! - *Two kinds of window.* A protected git write through `apply_patch` holds
//!   the repository's lock exclusively ([`kgraf_sandbox::repo_lock`]) from its
//!   snapshot through its check and any restore. A Bypass shell command holds
//!   no lock while it runs: every Bypass command gets the shared git
//!   directory writable, and the commands of parallel workers run at the same
//!   time. Each git process kGraf runs on the repository holds the lock
//!   shared, as before. Outside Bypass a command runs with the git directory
//!   read-only.
//! - *Frozen configuration.* Inside that writable directory, the sandbox
//!   keeps the files git reads configuration from read-only
//!   ([`ShellWindow::frozen_files`], [`kgraf_sandbox::FROZEN_GIT_FILES`]):
//!   kGraf's own git lists the configuration and then runs with every driver
//!   and hook it found neutralized, so a command that could change it between
//!   the two could make kGraf's git run a program. A configuration write goes
//!   through `apply_patch`, under the exclusive lock. A sandbox that cannot
//!   freeze single files (Linux, Windows) gets no shell window: the command
//!   runs with the git directory read-only and says why.
//! - *Shell pre-state,* before each Bypass command, without a lock: the git
//!   files a restore needs (the shared `config`, `HEAD`, `packed-refs`,
//!   `shallow`, every file under `refs/` and `info/`, each worktree admin
//!   directory's `HEAD`, `gitdir` and `commondir`, and this worktree's
//!   `index`), each with its stamp (length, times, inode); a file whose stamp
//!   is unchanged since the last pre-state, and was settled when read, is
//!   not read again. Bounded by [`GIT_SNAPSHOT_LIMIT`]; a repository over the
//!   bound runs the command with the git directory read-only, and says why.
//!   Also the object store's snapshot ([`objects`]): hard links into a
//!   directory under the project's state directory (outside every sandbox
//!   writable root), taken at the attempt's first command and brought up to
//!   date before each later one, reading only the directories that changed;
//!   copies bounded by [`OBJECT_COPY_LIMIT`] where hard links are impossible.
//!   The cheap repository check runs once, at the attempt's first command:
//!   what it finds is the repository's own state, never counted against a
//!   command.
//! - *Change detection,* after the command, without a lock: a git file whose
//!   bytes differ from the pre-state (an unchanged settled stamp means equal
//!   bytes; otherwise the bytes are compared), a file added or removed, or an
//!   object file of the snapshot missing or replaced in a changed directory.
//!   Nothing changed: that is all; no check, no lock, nothing recorded.
//! - *Check,* when something changed, under the repository's exclusive lock,
//!   before the tool call returns, so the run's next git operation never
//!   meets an unchecked write: the repository's config parses, `git
//!   for-each-ref` reports no broken ref, every ref and `HEAD` file names an
//!   object that exists (or is symbolic), this worktree's `HEAD` resolves to a
//!   commit and its index reads; and, when an object file was lost, `git
//!   fsck --connectivity-only` over the shared repository (so a `git gc` that
//!   packed loose objects passes, and a deleted object does not). A changed
//!   file exactly as a check that passed during the command left it was
//!   already checked, and is not checked or recorded again. When the attempt
//!   ends, [`ProtectedWrites::finish`] compares every object file with the
//!   snapshot (finding one altered in place, which leaves its directory
//!   unchanged) and, if anything changed during the attempt, runs that fsck
//!   once more under the lock; then it deletes the object snapshot. A
//!   settings file is checked by the validator the host gives (the same
//!   parser the host loads it with), with its bytes (or absence) as the
//!   snapshot, bounded by [`SETTINGS_FILE_LIMIT`].
//! - *Attribution* ([`windows`]): commands run at the same time, so a change
//!   one command's check finds may be another's. The check names every other
//!   command still running that started before the change was there; a kept
//!   change then reads `kept; ` [`NOT_EXCLUSIVE`] with their workers.
//! - *On a failed check:* the other commands it names are marked first;
//!   then the object files that are missing or replaced are put back from
//!   the snapshot (each verified first) and new object files are kept; then,
//!   if the check still fails, the changed files that are invalid on their
//!   own; then every changed one. If the check still fails after that, its
//!   output says so. The attempt is failed with the check's output
//!   ([`ProtectedWrites::failure`]), the change recorded as `restored:
//!   <output>` (with [`NOT_EXCLUSIVE`] and the marked workers, if any), and
//!   the worker's result failed, so it is never merged: a repository made
//!   whole again does not make the attempt's work trustworthy. Each marked
//!   command's attempt fails too when it ends, whatever it changed, recorded
//!   as `restored by another worker's check`: a restore can erase the trace
//!   of the write that caused the damage, so the cause is shared.
//! - *A passing write* is kept and recorded (`kept`) with its path, worker and
//!   mode, as a [`PermissionEvent::ProtectedWrite`] the run loop traces and
//!   puts on the activity card.
//!
//! Known limits: an object file altered in place (git writes them read-only,
//! so this takes a `chmod`) under the hard-link method altered the snapshot
//! too, and is reported as not restorable; it is found by the end-of-attempt
//! pass unless its directory changed too. A change kGraf's own git work makes
//! while a command runs (a merge moving the integration branch) is seen by
//! that command's check: checked and, when valid, recorded against it. The
//! last restore step (every changed file) can put back a ref another command
//! moved validly during the same command; it runs only when restoring the
//! invalid files did not make the check pass. The end-of-attempt fsck counts
//! a problem against the attempt when the attempt's first window did not see
//! it. Commands of another kGraf process on the same repository are not
//! named in attribution (the registry is per process; the lock still
//! serializes the checks).

use crate::permissions::{PermissionControl, PermissionMode};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
// The tests build their fixtures with plain git.
#[cfg(test)]
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use kgraf_sandbox::repo_lock::{self, RepoLockGuard};

mod objects;
mod windows;
pub use objects::Method as ObjectSnapshotMethod;
use objects::ObjectSnapshot;
use windows::{Pre, PreFile};

/// The words every event and failure carries when a change cannot be pinned
/// on one command: another command on the same repository ran at the same
/// time and also changed the git directory.
pub const NOT_EXCLUSIVE: &str =
    "changed while another worker's command also ran; cause not exclusive";

/// The settings files under `.kgraf` a worker may write in Bypass.
pub const SETTINGS_FILES: [&str; 2] = ["run.json", "worker.json"];

/// The most a git snapshot may copy. Refs, config and one index of a large
/// repository stay well under it; a repository over it gets no protected git
/// write rather than an unrestorable one.
pub const GIT_SNAPSHOT_LIMIT: u64 = 32 * 1024 * 1024;

/// The largest settings file that may be snapshotted and written.
pub const SETTINGS_FILE_LIMIT: u64 = 1024 * 1024;

/// The most an object-store snapshot may copy when hard links are impossible
/// (the state directory on another file system than the repository). A
/// store over it gets no Bypass shell window (the command runs with the git
/// directory read-only, and says why) rather than an unrestorable one.
/// Hard links have no bound: they copy no data.
pub const OBJECT_COPY_LIMIT: u64 = 512 * 1024 * 1024;

/// Where object snapshots live, under the project's state directory, which
/// is never a sandbox writable root.
pub const SNAPSHOT_DIR: &str = "git-snapshots";

/// Why a git window was not opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowError {
    /// The repository lock was not taken in time: the protected write is not
    /// made. (A shell command takes no lock before it runs, so it never
    /// gets this.)
    Busy(String),
    /// A snapshot cannot be taken (a bound, an unreadable file): no
    /// protected git write is made; a shell command runs with the git
    /// directory read-only.
    Unprotected(String),
}

impl std::fmt::Display for WindowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WindowError::Busy(e) | WindowError::Unprotected(e) => f.write_str(e),
        }
    }
}

/// An open protected write: holds the repository lock for a git write until
/// dropped, which must come after [`ProtectedWrites::after_write`].
#[derive(Debug)]
#[must_use = "the window holds the repository lock until the write is checked"]
pub struct WriteWindow {
    _lock: Option<RepoLockGuard>,
}

/// An open Bypass shell window: the git directory the sandbox may also
/// write, and the git directory's state when the command started, until
/// [`ProtectedWrites::after_shell`] has looked for the command's changes.
/// It holds no lock. Dropped without that call (an abandoned tool call), its
/// check runs at the attempt's next window or at its end.
#[must_use = "pass the window to after_shell, which checks the command's git changes"]
pub struct ShellWindow {
    root: PathBuf,
    frozen: Vec<PathBuf>,
    open: Option<OpenShell>,
}

struct OpenShell {
    id: u64,
    pre: Arc<Pre>,
    common: PathBuf,
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for ShellWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellWindow")
            .field("root", &self.root)
            .field("id", &self.open.as_ref().map(|o| o.id))
            .finish()
    }
}

impl ShellWindow {
    /// The shared git directory, to add as a sandbox writable root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The repository's configuration files beyond the fixed set
    /// ([`kgraf_sandbox::FROZEN_GIT_FILES`]): every config file git lists and
    /// every include target, existing or not. The sandbox keeps these and
    /// the fixed set read-only ([`kgraf_sandbox::SandboxPolicy::with_frozen_git_config`]),
    /// so the command cannot add a driver or hook while kGraf's own git runs
    /// between its configuration listing and its command.
    pub fn frozen_files(&self) -> &[PathBuf] {
        &self.frozen
    }
}

impl Drop for ShellWindow {
    fn drop(&mut self) {
        if let Some(open) = self.open.take() {
            // A mark another check left is not lost: the deferred check
            // fails the attempt with it.
            let suspect = windows::close(&open.common, open.id);
            let mut st = open.state.lock().unwrap_or_else(|p| p.into_inner());
            st.open_shells = st.open_shells.saturating_sub(1);
            for why in suspect {
                push_failure(
                    &mut st,
                    format!("{why}; an abandoned command may have caused it ({NOT_EXCLUSIVE})"),
                );
            }
            st.abandoned.push(open.pre);
        }
    }
}

/// What the check after a shell command found.
enum ShellCheck {
    /// Nothing changed, or only what a check during the command already
    /// passed.
    Unchanged,
    /// Changed, checked, kept. `shown`: the changed paths; `others`: the
    /// workers of other commands still running that may have made the change.
    Kept { shown: String, others: Vec<String> },
    /// Changed, failed its check, restored. `others`: as for `Kept`; each of
    /// their attempts fails too.
    Restored {
        shown: String,
        output: String,
        others: Vec<String>,
    },
    /// The git directory could not be read to tell.
    Failed { shown: String, output: String },
}

/// Validates a settings file after a write: its name (one of
/// [`SETTINGS_FILES`]) and its path. `Err` carries what is wrong.
pub type SettingsCheck = Arc<dyn Fn(&str, &Path) -> Result<(), String> + Send + Sync>;

/// Which protected area a write lands in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Area {
    Git,
    /// `.gitattributes`, `.gitmodules` or a path under `.githooks`
    /// (`kgraf_core::paths::is_bypass_git_file`): an ordinary tracked file in
    /// the worktree. No snapshot or check: git history keeps every earlier
    /// version, and kGraf's own git never runs what these files select
    /// (`kgraf_core::host_git`). The declared write scope applies to it like
    /// any file.
    Tracked,
    /// One of [`SETTINGS_FILES`].
    Settings(String),
}

/// Where an admitted protected path is written: `rel` under `root`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub area: Area,
    pub root: PathBuf,
    pub rel: PathBuf,
}

#[derive(Default)]
struct State {
    /// The last good copy of the git files, by absolute path, for a
    /// protected write through `apply_patch`.
    git: Option<BTreeMap<PathBuf, Vec<u8>>>,
    /// The object store's snapshot, from the attempt's first shell window
    /// until [`ProtectedWrites::finish`].
    objects: Option<ObjectSnapshot>,
    /// The attempt's shell windows open now.
    open_shells: usize,
    /// The pre-states of shell windows dropped without
    /// [`ProtectedWrites::after_shell`]: the next window or the finish checks
    /// them first.
    abandoned: Vec<Arc<Pre>>,
    /// The git files last read for a pre-state, with their stamps, so an
    /// unchanged file is not read again for the next one.
    git_cache: BTreeMap<PathBuf, PreFile>,
    /// The last good copy of each settings file (`None`: it did not exist).
    settings: BTreeMap<String, Option<Vec<u8>>>,
    /// A git write or a Bypass shell command's change happened since the
    /// last full check.
    git_touched: bool,
    failure: Option<String>,
    /// Every path a protected write named, kept or restored.
    written: BTreeSet<String>,
    /// What the cheap repository check found when the current write window
    /// opened (the repository's own state, not the window's).
    baseline: BTreeSet<String>,
    /// What it found at the attempt's first shell window, for each shell
    /// check: the repository's own problems, not any command's.
    shell_baseline: Option<BTreeSet<String>>,
    /// What it found when the attempt's first window opened, for the
    /// end-of-attempt check.
    first_baseline: Option<BTreeSet<String>>,
    /// The repository's configuration files, cached until one changes
    /// (decision 0094: a Bypass command's sandbox keeps them read-only).
    includes: kgraf_core::host_git::IncludeScan,
}

/// One attempt's guard over protected writes. Shared by its `apply_patch`
/// and shell tools and its worker loop.
pub struct ProtectedWrites {
    permissions: Arc<PermissionControl>,
    worker: String,
    worktree: PathBuf,
    admin_dir: PathBuf,
    common_dir: PathBuf,
    state_dir: PathBuf,
    settings_check: SettingsCheck,
    git_exe: Option<PathBuf>,
    lock_timeout: Duration,
    object_copy_limit: u64,
    force_object_copy: bool,
    /// Shared with each open [`ShellWindow`], whose drop hands its
    /// pre-state back for the deferred check.
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for ProtectedWrites {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProtectedWrites")
            .field("worker", &self.worker)
            .field("worktree", &self.worktree)
            .field("admin_dir", &self.admin_dir)
            .field("common_dir", &self.common_dir)
            .field("state_dir", &self.state_dir)
            .finish_non_exhaustive()
    }
}

impl ProtectedWrites {
    /// The guard for a worker in `ws`, with the project's state directory and
    /// the settings validator.
    pub fn for_workspace(
        permissions: Arc<PermissionControl>,
        worker: &str,
        ws: &kgraf_sandbox::Workspace,
        state_dir: PathBuf,
        settings_check: SettingsCheck,
    ) -> Result<Self, String> {
        let admin = ws.admin_dir().ok_or_else(|| {
            format!(
                "the worktree's recorded gitlink {:?} names no directory",
                ws.gitlink()
            )
        })?;
        Ok(Self::from_parts(
            permissions,
            worker,
            ws.root().to_path_buf(),
            admin,
            ws.git_common_dir().to_path_buf(),
            state_dir,
            settings_check,
        ))
    }

    /// The guard from its parts. `admin_dir` must be the worktree's git
    /// directory as recorded by the host, never read from the worktree.
    pub fn from_parts(
        permissions: Arc<PermissionControl>,
        worker: &str,
        worktree: PathBuf,
        admin_dir: PathBuf,
        common_dir: PathBuf,
        state_dir: PathBuf,
        settings_check: SettingsCheck,
    ) -> Self {
        Self {
            permissions,
            worker: worker.to_owned(),
            worktree,
            admin_dir,
            common_dir,
            state_dir,
            settings_check,
            git_exe: kgraf_sandbox::git::Git::discover()
                .ok()
                .map(|g| g.exe().to_path_buf()),
            lock_timeout: repo_lock::DEFAULT_TIMEOUT,
            object_copy_limit: OBJECT_COPY_LIMIT,
            force_object_copy: false,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// The longest a window waits for the repository lock
    /// ([`repo_lock::DEFAULT_TIMEOUT`] unless set).
    pub fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }

    /// The object-copy bound ([`OBJECT_COPY_LIMIT`] unless set).
    pub fn with_object_copy_limit(mut self, bytes: u64) -> Self {
        self.object_copy_limit = bytes;
        self
    }

    /// Copy object files as if hard links were impossible (the path taken
    /// when the state directory is on another file system).
    pub fn with_object_copies(mut self) -> Self {
        self.force_object_copy = true;
        self
    }

    /// How the attempt's object snapshot holds its files, while it has one.
    pub fn object_snapshot(&self) -> Option<(ObjectSnapshotMethod, PathBuf)> {
        self.state()
            .objects
            .as_ref()
            .map(|o| (o.method(), o.dir().to_path_buf()))
    }

    fn lock(&self, purpose: &str) -> Result<RepoLockGuard, WindowError> {
        repo_lock::acquire(
            &self.common_dir,
            repo_lock::Mode::Exclusive,
            &format!("worker {} {purpose}", self.worker),
            self.lock_timeout,
        )
        .map_err(|e| WindowError::Busy(e.to_string()))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether the user's mode is Bypass now. Read on every write: a mode
    /// change applies to the next one.
    pub fn bypass(&self) -> bool {
        self.permissions.mode() == PermissionMode::Bypass
    }

    /// The repository's shared git directory.
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// How a path the worker names is treated: `None` when it is not
    /// protected; `Some(Ok(target))` when it is and this mode admits it;
    /// `Some(Err(reason))` when it is refused.
    pub fn admit(&self, rel: &Path) -> Option<Result<Target, String>> {
        let why = kgraf_core::paths::forbidden_reason(rel)?;
        if !self.bypass() {
            return Some(Err(why));
        }
        Some(self.map(rel).ok_or(why).and_then(|r| r))
    }

    /// The Bypass mapping of a protected path: `None` when no mapping covers
    /// it (the plain refusal applies), `Some(Err)` when one does and refuses.
    fn map(&self, rel: &Path) -> Option<Result<Target, String>> {
        let mut parts = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(n) => parts.push(n.to_string_lossy().into_owned()),
                Component::CurDir => {}
                _ => return None,
            }
        }
        if kgraf_core::paths::is_bypass_git_file(rel) {
            return Some(Ok(Target {
                area: Area::Tracked,
                root: self.worktree.clone(),
                rel: rel.to_path_buf(),
            }));
        }
        let (first, rest) = parts.split_first()?;
        let rest_path: PathBuf = rest.iter().collect();
        match first.as_str() {
            ".git" => Some(self.map_git(&rest_path)),
            ".kgraf" => Some(self.map_settings(rest)),
            _ => None,
        }
    }

    fn map_git(&self, rest: &Path) -> Result<Target, String> {
        let mut comps = rest.components();
        let Some(Component::Normal(head)) = comps.next() else {
            return Err(
                "`.git` itself is this worktree's link to its repository; rewriting it \
                        would aim git at another repository, so it is refused in every mode"
                    .into(),
            );
        };
        if head == OsStr::new("objects") {
            return Err(
                "`.git/objects` holds the repository's objects, compressed files only git \
                        writes; let git write them (a shell git command in Bypass)"
                    .into(),
            );
        }
        if rest
            .components()
            .any(|c| kgraf_core::paths::is_protected_name(&c.as_os_str().to_string_lossy()))
        {
            return Err("a protected name inside `.git` is refused".into());
        }
        // The path in the repository's own spelling: git decides which names
        // are shared with the main repository by matching `info/exclude` or
        // `refs/...` with `/`, so a name spelled with a backslash on Windows
        // would be read as a name inside this worktree's own directory.
        let rest_arg = std::ffi::OsString::from(slashed(rest));
        let out = self
            .git(
                &self.admin_dir,
                [
                    OsStr::new("rev-parse"),
                    OsStr::new("--git-path"),
                    rest_arg.as_os_str(),
                ],
            )
            .map_err(|e| format!("cannot resolve `.git/{}`: {e}", slashed(rest)))?;
        let resolved = PathBuf::from(out.trim());
        let resolved = if resolved.is_absolute() {
            resolved
        } else {
            self.admin_dir.join(resolved)
        };
        for root in [&self.admin_dir, &self.common_dir] {
            if let Ok(rel) = resolved.strip_prefix(root)
                && !rel.as_os_str().is_empty()
                && rel.components().all(|c| matches!(c, Component::Normal(_)))
            {
                return Ok(Target {
                    area: Area::Git,
                    root: root.clone(),
                    rel: rel.to_path_buf(),
                });
            }
        }
        Err(format!(
            "`.git/{}` resolves to {}, outside the repository's git directory",
            slashed(rest),
            resolved.display()
        ))
    }

    fn map_settings(&self, rest: &[String]) -> Result<Target, String> {
        match rest {
            [name] if SETTINGS_FILES.contains(&name.as_str()) => Ok(Target {
                area: Area::Settings(name.clone()),
                root: self.state_dir.clone(),
                rel: PathBuf::from(name),
            }),
            _ => Err(format!(
                "only kGraf's settings ({}) are writable in Bypass; the spend ledger, the trace, \
                 run records, worker transcripts, the graph and the rest of `.kgraf` are kGraf's \
                 own books, refused in every mode",
                SETTINGS_FILES
                    .iter()
                    .map(|f| format!("`.kgraf/{f}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            )),
        }
    }

    /// Open a protected write to `area`: for git, take the repository lock
    /// and the snapshot. Keep the window until [`Self::after_write`] has run
    /// for every write it covers; an `Err` means the write must not be made.
    pub fn before_write(&self, area: &Area) -> Result<WriteWindow, WindowError> {
        match area {
            Area::Git => {
                let lock = self.lock("protected git write")?;
                let mut st = self.state();
                self.settle_abandoned(&mut st);
                self.open_git(&mut st).map_err(WindowError::Unprotected)?;
                Ok(WriteWindow { _lock: Some(lock) })
            }
            // An ordinary tracked file: no snapshot, no lock (git history keeps
            // every version).
            Area::Tracked => Ok(WriteWindow { _lock: None }),
            Area::Settings(name) => {
                let mut st = self.state();
                if !st.settings.contains_key(name) {
                    let path = self.state_dir.join(name);
                    let bytes = read_bounded(&path, SETTINGS_FILE_LIMIT)
                        .map_err(WindowError::Unprotected)?;
                    st.settings.insert(name.clone(), bytes);
                }
                Ok(WriteWindow { _lock: None })
            }
        }
    }

    /// The start of a protected write's window, under the lock: the
    /// repository's state now is the baseline and the snapshot.
    fn open_git(&self, st: &mut State) -> Result<(), String> {
        let baseline: BTreeSet<String> = self.problems(false)?.into_iter().collect();
        let snapshot = self.snapshot_git()?;
        if st.first_baseline.is_none() {
            st.first_baseline = Some(baseline.clone());
        }
        st.baseline = baseline;
        st.git = Some(snapshot);
        Ok(())
    }

    /// The git files as they are now, for a shell window's pre-state: each
    /// file's stamp, and its bytes, read again only when its stamp differs
    /// from the cached one or was not settled when that was read. Bounded by
    /// [`GIT_SNAPSHOT_LIMIT`].
    fn capture_pre(&self, cache: &mut BTreeMap<PathBuf, PreFile>) -> Result<Pre, String> {
        let mut files = BTreeMap::new();
        let mut total = 0u64;
        for path in self.git_scope()? {
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("cannot snapshot {}: {e}", path.display())),
            };
            let stamp = objects::stamp_of(&meta);
            let file = match cache.get(&path) {
                Some(f) if f.trusted && f.stamp == stamp => f.clone(),
                _ => {
                    let read_at = SystemTime::now();
                    let bytes = match std::fs::read(&path) {
                        Ok(b) => b,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(e) => return Err(format!("cannot snapshot {}: {e}", path.display())),
                    };
                    PreFile {
                        trusted: stamp.settled(read_at),
                        stamp,
                        bytes: Arc::new(bytes),
                    }
                }
            };
            total += file.bytes.len() as u64;
            if total > GIT_SNAPSHOT_LIMIT {
                return Err(format!(
                    "the repository's refs, config and index exceed the {GIT_SNAPSHOT_LIMIT}-byte \
                     snapshot bound, so a git write could not be restored; the git directory \
                     stays read-only"
                ));
            }
            files.insert(path, file);
        }
        cache.clone_from(&files);
        Ok(Pre { files })
    }

    /// The git files that differ from `pre` now, each with its bytes now
    /// (`None`: removed). A file whose stamp is unchanged and was settled is
    /// not read.
    fn changed_since(&self, pre: &Pre) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>, String> {
        let now = self.git_scope()?;
        let mut out = BTreeMap::new();
        for path in &now {
            let meta = match std::fs::symlink_metadata(path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
            };
            let old = pre.files.get(path);
            if old.is_some_and(|f| f.trusted && f.stamp == objects::stamp_of(&meta)) {
                continue;
            }
            let cur = match std::fs::read(path) {
                Ok(b) => Some(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
            };
            if old.map(|f| f.bytes.as_slice()) != cur.as_deref() {
                out.insert(path.clone(), cur);
            }
        }
        let now: BTreeSet<&PathBuf> = now.iter().collect();
        for path in pre.files.keys() {
            if !now.contains(path) {
                out.insert(path.clone(), None);
            }
        }
        Ok(out)
    }

    /// A directory of this attempt's own under the state directory.
    fn snapshot_dir(&self) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let worker: String = self
            .worker
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        self.state_dir.join(SNAPSHOT_DIR).join(format!(
            "{worker}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    /// Check the area after a write to `shown` (the path as the worker named
    /// it), restore the snapshot when the check fails, and record the write.
    /// Call while the write's [`WriteWindow`] is held. `Err` carries the
    /// check's output; the attempt is then failed.
    pub fn after_write(&self, area: &Area, shown: &str) -> Result<(), String> {
        if *area == Area::Tracked {
            // A worktree file: recorded, but kept out of `written`, so the
            // worker's change list (which drops what [`Self::wrote`] names,
            // as landing outside the worktree) still carries it to the scope
            // checks.
            self.permissions
                .record_protected_write(&self.worker, shown, "kept");
            return Ok(());
        }
        let mut st = self.state();
        let result = match area {
            Area::Git => {
                st.git_touched = true;
                let snapshot = st.git.clone().unwrap_or_default();
                let baseline = st.baseline.clone();
                self.check_git_and_restore(&mut st, &snapshot, &baseline, false, &[])
                    .and_then(|()| {
                        st.git = Some(self.snapshot_git()?);
                        Ok(())
                    })
            }
            Area::Settings(name) => self.check_settings_and_restore(&mut st, name),
            Area::Tracked => Ok(()),
        };
        self.settle(&mut st, shown, result)
    }

    fn settle(
        &self,
        st: &mut State,
        shown: &str,
        result: Result<(), String>,
    ) -> Result<(), String> {
        st.written.insert(shown.to_owned());
        match result {
            Ok(()) => {
                self.permissions
                    .record_protected_write(&self.worker, shown, "kept");
                Ok(())
            }
            Err(output) => {
                self.permissions.record_protected_write(
                    &self.worker,
                    shown,
                    &format!("restored: {output}"),
                );
                let line = format!(
                    "a protected write to {shown} failed its integrity check and was restored: {output}"
                );
                push_failure(st, line.clone());
                Err(line)
            }
        }
    }

    /// Before a shell command in Bypass: bring the object snapshot up to
    /// date (taking it at the attempt's first command), record the git files'
    /// state, register the window, and return it; its root is the git
    /// directory the sandbox may also write. No lock is taken, so commands of
    /// parallel workers run at the same time. `Ok(None)` outside Bypass (the
    /// command runs with the git directory read-only); `Err(Unprotected)`
    /// when a snapshot cannot be taken (it runs read-only too, and says why).
    pub fn before_shell(&self) -> Result<Option<ShellWindow>, WindowError> {
        if !self.bypass() {
            return Ok(None);
        }
        self.settle_abandoned_locked();
        // First, before anything is registered: without the list of files
        // to keep read-only, the git directory is not made writable at all.
        let frozen = {
            let exe = self.git_exe.clone().ok_or_else(|| {
                WindowError::Unprotected(
                    "git is not on PATH, so the repository's configuration files cannot be listed"
                        .to_owned(),
                )
            })?;
            let mut st = self.state();
            st.includes.files(&exe, &self.common_dir).map_err(|e| {
                WindowError::Unprotected(format!(
                    "the repository's configuration files cannot be listed: {e}"
                ))
            })?
        };
        let mut st = self.state();
        if st.shell_baseline.is_none() {
            let baseline: BTreeSet<String> = self
                .problems(false)
                .map_err(WindowError::Unprotected)?
                .into_iter()
                .collect();
            if st.first_baseline.is_none() {
                st.first_baseline = Some(baseline.clone());
            }
            st.shell_baseline = Some(baseline);
        }
        let forget = st.open_shells == 0;
        match st.objects.as_mut() {
            Some(o) => o.sync(forget).map_err(WindowError::Unprotected)?,
            None => {
                st.objects = Some(
                    ObjectSnapshot::take(
                        &self.common_dir.join("objects"),
                        self.snapshot_dir(),
                        self.force_object_copy,
                        self.object_copy_limit,
                    )
                    .map_err(WindowError::Unprotected)?,
                );
            }
        }
        let pre = Arc::new(
            self.capture_pre(&mut st.git_cache)
                .map_err(WindowError::Unprotected)?,
        );
        st.open_shells += 1;
        drop(st);
        let id = windows::open(&self.common_dir, &self.worker, pre.clone());
        Ok(Some(ShellWindow {
            root: self.common_dir.clone(),
            frozen,
            open: Some(OpenShell {
                id,
                pre,
                common: self.common_dir.clone(),
                state: self.state.clone(),
            }),
        }))
    }

    /// After a shell command that ran with the git directory writable. First,
    /// without a lock, whether anything the check covers changed since the
    /// window's pre-state (a git file's bytes, an object file lost). If
    /// nothing did, that is all. If something did: under the repository's
    /// exclusive lock, the check, the restore on failure, and one recorded
    /// protected write naming the changed files. `Err` fails the attempt:
    /// the check failed, or another command's check found damage while this
    /// one ran (see [`windows`] for who fails).
    pub fn after_shell(&self, mut window: ShellWindow) -> Result<(), String> {
        let Some(open) = window.open.take() else {
            return Ok(());
        };
        let changed = {
            let st = self.state();
            let lost = st.objects.as_ref().map(ObjectSnapshot::lost).transpose();
            // A read error is not "unchanged": the check below reports it.
            !matches!(lost, Ok(ref l) if l.as_ref().is_none_or(|l| l.is_empty()))
                || !matches!(self.changed_since(&open.pre), Ok(ref c) if c.is_empty())
        };
        if !changed {
            let suspect = windows::close(&self.common_dir, open.id);
            let mut st = self.state();
            st.open_shells = st.open_shells.saturating_sub(1);
            return self.settle_shell(&mut st, ShellCheck::Unchanged, suspect);
        }
        let lock = match self.lock("check after a shell command") {
            Ok(l) => l,
            Err(e) => {
                let suspect = windows::close(&self.common_dir, open.id);
                let mut st = self.state();
                st.open_shells = st.open_shells.saturating_sub(1);
                st.git_touched = true;
                let check = ShellCheck::Failed {
                    shown: ".git".into(),
                    output: format!(
                        "the command changed the repository's git directory and its check could \
                         not run: {e}"
                    ),
                };
                return self.settle_shell(&mut st, check, suspect);
            }
        };
        let mut st = self.state();
        let check = self.check_shell(&mut st, &open.pre, Some(open.id));
        let suspect = windows::close(&self.common_dir, open.id);
        st.open_shells = st.open_shells.saturating_sub(1);
        let result = self.settle_shell(&mut st, check, suspect);
        drop(st);
        drop(lock);
        result
    }

    /// The check after a shell command, under the repository's exclusive
    /// lock: what changed since `pre` and was not already checked, checked
    /// and restored on failure. `id`: the window, while it is registered
    /// (`None` for an abandoned one, checked later).
    fn check_shell(&self, st: &mut State, pre: &Pre, id: Option<u64>) -> ShellCheck {
        let changed = match self.changed_since(pre) {
            Ok(c) => c,
            Err(output) => {
                return ShellCheck::Failed {
                    shown: ".git".into(),
                    output,
                };
            }
        };
        let lost = match st.objects.as_ref().map(ObjectSnapshot::lost).transpose() {
            Ok(l) => l.unwrap_or_default(),
            Err(output) => {
                return ShellCheck::Failed {
                    shown: ".git/objects".into(),
                    output,
                };
            }
        };
        let changed = match id {
            Some(id) => windows::not_yet_checked(&self.common_dir, id, changed),
            None => changed,
        };
        if changed.is_empty() && lost.is_empty() {
            return ShellCheck::Unchanged;
        }
        st.git_touched = true;
        let mut shown: Vec<String> = changed.keys().map(|p| self.shown_git_path(p)).collect();
        if !lost.is_empty() {
            shown.push(format!(
                ".git/objects ({} object files removed or replaced)",
                lost.len()
            ));
        }
        let shown = shown.join(", ");
        let baseline = st.shell_baseline.clone().unwrap_or_default();
        let full = !lost.is_empty();
        // Damage is named, and the other commands that may have caused it
        // marked, before anything is restored (see `windows`).
        let damaged = match self.check_git(full, &baseline) {
            Ok(()) => None,
            Err(_) => {
                let invalid: windows::Found = changed
                    .iter()
                    .filter(|(p, now)| now.is_none() || self.valid_file(p).is_err())
                    .map(|(p, now)| (p.clone(), now.clone()))
                    .collect();
                Some(if invalid.is_empty() {
                    changed.clone()
                } else {
                    invalid
                })
            }
        };
        let found = damaged.as_ref().unwrap_or(&changed);
        let what: Vec<String> = found.keys().map(|p| self.shown_git_path(p)).collect();
        let others = windows::others(
            &self.common_dir,
            id,
            &self.worker,
            &if what.is_empty() {
                ".git/objects".to_owned()
            } else {
                what.join(", ")
            },
            found,
            !lost.is_empty(),
            damaged.is_some(),
        );
        // A passing first look is the check: no second one. On damage the
        // restore runs its own looks.
        let result = if damaged.is_some() {
            let snapshot: BTreeMap<PathBuf, Vec<u8>> = pre
                .files
                .iter()
                .map(|(p, f)| (p.clone(), f.bytes.to_vec()))
                .collect();
            self.check_git_and_restore(st, &snapshot, &baseline, full, &lost)
        } else {
            Ok(())
        };
        match result {
            // Damage that is gone by the second look (a command still
            // running put it right meanwhile) still fails: it was there.
            Ok(()) if damaged.is_some() => ShellCheck::Restored {
                shown,
                output: "the check failed, and passed when run again".into(),
                others,
            },
            Ok(()) => {
                let forget = st.open_shells <= 1;
                let refreshed = st
                    .objects
                    .as_mut()
                    .map(|o| o.sync(forget))
                    .transpose()
                    .and_then(|_| self.capture_pre(&mut st.git_cache));
                match refreshed {
                    Ok(now) => {
                        windows::passed(
                            &self.common_dir,
                            now.files.into_iter().map(|(p, f)| (p, f.bytes)).collect(),
                        );
                        ShellCheck::Kept { shown, others }
                    }
                    Err(output) => ShellCheck::Failed { shown, output },
                }
            }
            Err(output) => ShellCheck::Restored {
                shown,
                output,
                others,
            },
        }
    }

    /// Record what a shell check found and turn it into the call's result.
    /// Each outcome is one [`PermissionEvent::ProtectedWrite`]:
    ///
    /// - kept: `kept`, and when other commands still running may have made
    ///   the change, `kept; ` [`NOT_EXCLUSIVE`] with their workers;
    /// - restored: `restored: <check output>`, with [`NOT_EXCLUSIVE`] and the
    ///   workers whose attempts fail with this one, when there are any;
    /// - while this command ran, another command's check found damage it may
    ///   have caused (`suspect`): `restored by another worker's check: ...`
    ///   with [`NOT_EXCLUSIVE`], and the attempt fails, whatever this check
    ///   found.
    fn settle_shell(
        &self,
        st: &mut State,
        check: ShellCheck,
        suspect: Vec<String>,
    ) -> Result<(), String> {
        let with = |names: &[String]| format!("{NOT_EXCLUSIVE} ({})", names.join(", "));
        let fail_suspect = |st: &mut State, shown: &str| -> Result<(), String> {
            let why = suspect.join("; ");
            self.permissions.record_protected_write(
                &self.worker,
                shown,
                &format!("restored by another worker's check: {why}; {NOT_EXCLUSIVE}"),
            );
            let line = format!(
                "{why}; this command may have caused it ({NOT_EXCLUSIVE}), so this attempt is \
                 failed too"
            );
            push_failure(st, line.clone());
            Err(line)
        };
        match check {
            ShellCheck::Unchanged if suspect.is_empty() => Ok(()),
            ShellCheck::Unchanged => fail_suspect(st, ".git"),
            ShellCheck::Kept { shown, others } => {
                st.written.insert(shown.clone());
                if !suspect.is_empty() {
                    return fail_suspect(st, &shown);
                }
                let outcome = if others.is_empty() {
                    "kept".to_owned()
                } else {
                    format!("kept; {}", with(&others))
                };
                self.permissions
                    .record_protected_write(&self.worker, &shown, &outcome);
                Ok(())
            }
            ShellCheck::Restored {
                shown,
                output,
                others,
            } => {
                st.written.insert(shown.clone());
                let mut outcome = format!("restored: {output}");
                let mut line = format!(
                    "a protected write to {shown} failed its integrity check and was restored: \
                     {output}"
                );
                if !others.is_empty() {
                    outcome.push_str(&format!("; {}", with(&others)));
                    line.push_str(&format!(
                        " ({}; each of those attempts is failed too)",
                        with(&others)
                    ));
                }
                if !suspect.is_empty() {
                    let why = suspect.join("; ");
                    outcome.push_str(&format!("; {why}; {NOT_EXCLUSIVE}"));
                    line.push_str(&format!(" ({why}; {NOT_EXCLUSIVE})"));
                }
                self.permissions
                    .record_protected_write(&self.worker, &shown, &outcome);
                push_failure(st, line.clone());
                Err(line)
            }
            ShellCheck::Failed { shown, output } => {
                st.written.insert(shown.clone());
                self.permissions.record_protected_write(
                    &self.worker,
                    &shown,
                    &format!("unchecked: {output}"),
                );
                let line = format!(
                    "the repository's git directory could not be checked after the command: \
                     {output}"
                );
                push_failure(st, line.clone());
                if suspect.is_empty() {
                    Err(line)
                } else {
                    fail_suspect(st, &shown)
                }
            }
        }
    }

    /// The deferred checks of abandoned shell windows, under the lock
    /// (taken here when there is one to check).
    fn settle_abandoned_locked(&self) {
        if self.state().abandoned.is_empty() {
            return;
        }
        match self.lock("check after an abandoned shell command") {
            Ok(_lock) => self.settle_abandoned(&mut self.state()),
            Err(e) => {
                let mut st = self.state();
                st.abandoned.clear();
                push_failure(
                    &mut st,
                    format!("the check after an abandoned shell command did not run: {e}"),
                );
            }
        }
    }

    /// Shell windows dropped without their check (their calls were
    /// abandoned): check each now, with the lock held by the caller, and say
    /// the attribution is no longer exact.
    fn settle_abandoned(&self, st: &mut State) {
        for pre in std::mem::take(&mut st.abandoned) {
            let check = self.check_shell(st, &pre, None);
            if let Err(e) = self.settle_shell(st, check, Vec::new()) {
                push_failure(
                    st,
                    format!(
                        "{e} (checked after an abandoned shell command; the repository may have \
                         changed since)"
                    ),
                );
            }
        }
    }

    /// When the attempt ends: under the repository lock, the full check
    /// (`git fsck --connectivity-only` over the shared repository, and every
    /// object file compared with the snapshot) if any git write happened,
    /// with the restore on failure; then the object snapshot is deleted, and
    /// the first failure, if any, is returned. The worker's result is failed
    /// on `Err`.
    pub fn finish(&self) -> Result<(), String> {
        // Every object file is compared once here, whatever the per-command
        // checks saw: a file altered in place leaves its directory unchanged,
        // so only this full pass finds it.
        let touched = {
            let st = self.state();
            let altered = st
                .objects
                .as_ref()
                .is_some_and(|o| o.lost_full().map_or(true, |l| !l.is_empty()));
            (st.git_touched || altered || !st.abandoned.is_empty()) && st.first_baseline.is_some()
        };
        let lock = if touched {
            match self.lock("end-of-attempt check") {
                Ok(l) => Some(l),
                Err(e) => {
                    push_failure(
                        &mut self.state(),
                        format!("the end-of-attempt repository check did not run: {e}"),
                    );
                    None
                }
            }
        } else {
            None
        };
        let mut st = self.state();
        if lock.is_some() {
            self.settle_abandoned(&mut st);
            st.git_touched = false;
            let baseline = st.first_baseline.clone().unwrap_or_default();
            let lost = match st
                .objects
                .as_ref()
                .map(ObjectSnapshot::lost_full)
                .transpose()
            {
                Ok(l) => l.unwrap_or_default(),
                Err(e) => {
                    let _ = self.settle(&mut st, ".git/objects", Err(e));
                    Vec::new()
                }
            };
            // The ref snapshot is the last protected write's, if any: a shell
            // command's changes were each checked when it ended, and an older
            // copy of refs other workers have moved since must not be put back.
            let snapshot = st.git.clone().unwrap_or_default();
            let result = self.check_git_and_restore(&mut st, &snapshot, &baseline, true, &lost);
            if result.is_err() {
                // The shown path is the repository: the full check covers it all.
                let _ = self.settle(&mut st, ".git", result);
            }
        }
        if let Some(mut o) = st.objects.take() {
            o.remove();
        }
        drop(lock);
        match &st.failure {
            Some(f) => Err(f.clone()),
            None => Ok(()),
        }
    }

    /// Whether `path` (as the worker named it) was written through this
    /// guard: such a path is not a change to the worktree, so it is not
    /// reported among the worker's changed files.
    pub fn wrote(&self, path: &Path) -> bool {
        self.state()
            .written
            .contains(path.to_string_lossy().as_ref())
    }

    /// The attempt's failure so far, if any.
    pub fn failure(&self) -> Option<String> {
        self.state().failure.clone()
    }

    fn shown_git_path(&self, abs: &Path) -> String {
        for root in [&self.admin_dir, &self.common_dir] {
            if let Ok(rel) = abs.strip_prefix(root) {
                return format!(".git/{}", slashed(rel));
            }
        }
        abs.display().to_string()
    }

    // ---- settings ----

    fn check_settings_and_restore(&self, st: &mut State, name: &str) -> Result<(), String> {
        let path = self.state_dir.join(name);
        let check = match std::fs::symlink_metadata(&path) {
            Ok(m) if m.len() > SETTINGS_FILE_LIMIT => Err(format!(
                "`.kgraf/{name}` is {} bytes, over the {SETTINGS_FILE_LIMIT}-byte bound",
                m.len()
            )),
            Ok(m) if !m.is_file() => Err(format!("`.kgraf/{name}` is not a regular file")),
            Ok(_) => (self.settings_check)(name, &path),
            // A deleted settings file is a valid state: kGraf's defaults apply.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot read `.kgraf/{name}`: {e}")),
        };
        match check {
            Ok(()) => {
                let now = read_bounded(&path, SETTINGS_FILE_LIMIT)?;
                st.settings.insert(name.to_owned(), now);
                Ok(())
            }
            Err(output) => {
                let saved = st.settings.get(name).cloned().flatten();
                restore_file(&path, saved.as_deref())
                    .map_err(|e| format!("{output}; and the restore failed: {e}"))?;
                Err(output)
            }
        }
    }

    // ---- git ----

    fn git<I, S>(&self, git_dir: &Path, args: I) -> Result<String, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (ok, out, err) = self.git_raw(git_dir, args, None)?;
        if ok {
            Ok(out)
        } else {
            Err(err.trim().to_owned())
        }
    }

    /// Run git against `git_dir` with hooks and the file system monitor off
    /// (a written config must never make kGraf's own git run a program), the
    /// redirecting environment removed. Returns (success, stdout, stderr).
    fn git_raw<I, S>(
        &self,
        git_dir: &Path,
        args: I,
        stdin: Option<&[u8]>,
    ) -> Result<(bool, String, String), String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let exe = self.git_exe.as_ref().ok_or_else(|| {
            "git is not on PATH; a protected git write cannot be checked".to_owned()
        })?;
        let args: Vec<std::ffi::OsString> =
            args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let pre = [
            std::ffi::OsString::from("--git-dir"),
            git_dir.as_os_str().to_owned(),
        ];
        let inv = kgraf_core::host_git::Invocation {
            exe,
            cwd: Some(&self.worktree),
            pre: &pre,
        };
        // The host runner: hooks, every configured driver, the file system
        // monitor and transports off, the inherited `GIT_*` removed. A config
        // git cannot read is the check's failure, reported as git's.
        let cmd = match kgraf_core::host_git::command(&inv, &args) {
            Ok(cmd) => cmd,
            Err(kgraf_core::host_git::HostGitError::Config { stderr, .. }) => {
                return Ok((false, String::new(), stderr));
            }
            Err(e) => return Err(format!("cannot run git: {e}")),
        };
        // Bounded, input written and output read on threads, the tree
        // killed at the bound (`kgraf_core::child_run`).
        let output = kgraf_core::child_run::run(
            cmd,
            "git (protected write check)",
            stdin,
            kgraf_core::child_run::Limits::new(kgraf_core::host_git::COMMAND_TIMEOUT),
        )
        .map_err(|e| format!("cannot run git: {e}"))?;
        Ok((
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }

    /// Every file a git snapshot holds, as it is on disk now.
    fn git_scope(&self) -> Result<Vec<PathBuf>, String> {
        let c = &self.common_dir;
        let mut files = Vec::new();
        for name in ["config", "HEAD", "packed-refs", "shallow"] {
            push_if_file(&mut files, c.join(name));
        }
        walk_files(&c.join("refs"), &mut files)?;
        walk_files(&c.join("info"), &mut files)?;
        if let Ok(entries) = std::fs::read_dir(c.join("worktrees")) {
            for e in entries.flatten() {
                for name in ["HEAD", "gitdir", "commondir"] {
                    push_if_file(&mut files, e.path().join(name));
                }
            }
        }
        push_if_file(&mut files, self.admin_dir.join("HEAD"));
        push_if_file(&mut files, self.admin_dir.join("index"));
        files.sort();
        files.dedup();
        Ok(files)
    }

    fn snapshot_git(&self) -> Result<BTreeMap<PathBuf, Vec<u8>>, String> {
        let mut total = 0u64;
        let mut out = BTreeMap::new();
        for path in self.git_scope()? {
            let bytes = std::fs::read(&path)
                .map_err(|e| format!("cannot snapshot {}: {e}", path.display()))?;
            total += bytes.len() as u64;
            if total > GIT_SNAPSHOT_LIMIT {
                return Err(format!(
                    "the repository's refs, config and index exceed the {GIT_SNAPSHOT_LIMIT}-byte \
                     snapshot bound, so a protected git write could not be restored; it is not made"
                ));
            }
            out.insert(path, bytes);
        }
        Ok(out)
    }

    /// The snapshotted files that differ now, new ones and removed ones
    /// included.
    fn changed_files(&self, snapshot: &BTreeMap<PathBuf, Vec<u8>>) -> Result<Vec<PathBuf>, String> {
        let now = self.git_scope()?;
        let mut changed = Vec::new();
        for path in &now {
            match (snapshot.get(path), std::fs::read(path)) {
                (Some(old), Ok(cur)) if *old == cur => {}
                _ => changed.push(path.clone()),
            }
        }
        let now: BTreeSet<&PathBuf> = now.iter().collect();
        for path in snapshot.keys() {
            if !now.contains(path) {
                changed.push(path.clone());
            }
        }
        changed.sort();
        changed.dedup();
        Ok(changed)
    }

    /// The repository check; `Err` carries every failure found that was not
    /// already there when the attempt's first snapshot was taken (a problem
    /// the repository had before is not this attempt's write).
    fn check_git(&self, full: bool, baseline: &BTreeSet<String>) -> Result<(), String> {
        let problems: Vec<String> = self
            .problems(full)?
            .into_iter()
            .filter(|p| !baseline.contains(p))
            .collect();
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// Every problem the repository check finds.
    fn problems(&self, full: bool) -> Result<Vec<String>, String> {
        let mut problems = Vec::new();
        if let Err(e) = self.git(&self.common_dir, ["config", "--list"]) {
            problems.push(format!("the repository config does not parse: {e}"));
        }
        match self.git_raw(
            &self.common_dir,
            ["for-each-ref", "--format=%(objectname) %(refname)"],
            None,
        ) {
            Ok((true, _, err)) if err.trim().is_empty() => {}
            Ok((_, _, err)) => problems.push(format!("git for-each-ref: {}", err.trim())),
            Err(e) => problems.push(e),
        }
        // Each file's own syntax first, then every object id they name in one
        // `cat-file --batch-check`: one git process however many refs.
        let mut named: Vec<(PathBuf, Vec<String>)> = Vec::new();
        for path in self.git_scope()? {
            match self.file_ids(&path) {
                Ok(ids) => named.push((path, ids)),
                Err(e) => problems.push(format!("{}: {e}", self.shown_git_path(&path))),
            }
        }
        let all: BTreeSet<String> = named.iter().flat_map(|(_, ids)| ids.clone()).collect();
        match self.missing_ids(&all) {
            Ok(missing) => {
                for (path, ids) in &named {
                    if let Some(id) = ids.iter().find(|id| missing.contains(*id)) {
                        problems.push(format!(
                            "{}: names an object that does not exist ({id} missing)",
                            self.shown_git_path(path)
                        ));
                    }
                }
            }
            Err(e) => problems.push(e),
        }
        if let Err(e) = self.git(
            &self.admin_dir,
            ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        ) {
            problems.push(format!(
                "this worktree's HEAD does not resolve to a commit {e}"
            ));
        }
        if full {
            match self.git_raw(
                &self.common_dir,
                [
                    "fsck",
                    "--connectivity-only",
                    "--no-dangling",
                    "--no-progress",
                ],
                None,
            ) {
                Ok((true, _, _)) => {}
                Ok((false, out, err)) => {
                    problems.push(format!("git fsck: {} {}", out.trim(), err.trim()))
                }
                Err(e) => problems.push(e),
            }
        }
        Ok(problems)
    }

    /// Whether one snapshotted file is valid on its own.
    fn valid_file(&self, path: &Path) -> Result<(), String> {
        let ids: BTreeSet<String> = self.file_ids(path)?.into_iter().collect();
        match self.missing_ids(&ids)?.into_iter().next() {
            Some(id) => Err(format!(
                "names an object that does not exist ({id} missing)"
            )),
            None => Ok(()),
        }
    }

    /// The object ids in `ids` the repository does not have.
    fn missing_ids(&self, ids: &BTreeSet<String>) -> Result<BTreeSet<String>, String> {
        if ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        let mut input = ids.iter().cloned().collect::<Vec<_>>().join("\n");
        input.push('\n');
        let (ok, out, err) = self.git_raw(
            &self.common_dir,
            ["cat-file", "--batch-check"],
            Some(input.as_bytes()),
        )?;
        if !ok {
            return Err(format!("cat-file: {}", err.trim()));
        }
        Ok(out
            .lines()
            .filter_map(|l| l.strip_suffix(" missing"))
            .map(str::to_owned)
            .collect())
    }

    /// One snapshotted file's own syntax; `Ok` carries the object ids it
    /// names, for the caller to look up.
    fn file_ids(&self, path: &Path) -> Result<Vec<String>, String> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err("not a regular file".into());
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let in_refs = path.starts_with(self.common_dir.join("refs"));
        if path.starts_with(self.common_dir.join("info")) || name == "shallow" {
            return Ok(Vec::new());
        }
        if name == "config" && path.parent() == Some(self.common_dir.as_path()) {
            return self
                .git(
                    &self.common_dir,
                    [
                        OsStr::new("config"),
                        OsStr::new("--file"),
                        path.as_os_str(),
                        OsStr::new("--list"),
                    ],
                )
                .map(|_| Vec::new())
                .map_err(|e| format!("does not parse: {e}"));
        }
        if name == "index" {
            let (ok, _, err) = self.git_raw(
                &self.admin_dir,
                [
                    OsStr::new("--work-tree"),
                    self.worktree.as_os_str(),
                    OsStr::new("ls-files"),
                    OsStr::new("--stage"),
                ],
                None,
            )?;
            return if ok {
                Ok(Vec::new())
            } else {
                Err(format!("the index does not read: {}", err.trim()))
            };
        }
        if name == "gitdir" || name == "commondir" {
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            return if text.trim().is_empty() || text.trim().lines().count() != 1 {
                Err("is not one path".into())
            } else {
                Ok(Vec::new())
            };
        }
        let text = std::fs::read(path).map_err(|e| e.to_string())?;
        let text = String::from_utf8(text).map_err(|_| "is not text".to_owned())?;
        let mut ids = Vec::new();
        if name == "packed-refs" {
            for line in text.lines() {
                if line.starts_with('#') || line.trim().is_empty() {
                    continue;
                }
                let id = line.strip_prefix('^').unwrap_or(line);
                let id = id.split(' ').next().unwrap_or("");
                if !is_object_id(id) {
                    return Err(format!("line {line:?} is not a packed ref"));
                }
                ids.push(id.to_owned());
            }
        } else if in_refs || name == "HEAD" {
            let t = text.trim();
            if let Some(target) = t.strip_prefix("ref: ") {
                if !target.starts_with("refs/") {
                    return Err(format!("symbolic ref to {target:?}, not under refs/"));
                }
            } else if is_object_id(t) {
                ids.push(t.to_owned());
            } else {
                return Err(format!("{t:?} is neither an object id nor a symbolic ref"));
            }
        }
        Ok(ids)
    }

    /// Check (with fsck when `full`); on failure restore and check again.
    /// The restore goes from least to most: the object files in `lost` that
    /// are missing or replaced (new object files are kept), then the
    /// snapshotted files that changed and are invalid on their own, then every
    /// changed one, each put back from `snapshot`. Problems in `baseline`
    /// (the repository's own) are not counted. The caller refreshes its
    /// snapshots after a pass.
    fn check_git_and_restore(
        &self,
        st: &mut State,
        snapshot: &BTreeMap<PathBuf, Vec<u8>>,
        baseline: &BTreeSet<String>,
        full: bool,
        lost: &[PathBuf],
    ) -> Result<(), String> {
        let first = match self.check_git(full, baseline) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        let mut notes = Vec::new();
        if !lost.is_empty()
            && let Some(o) = st.objects.as_mut()
        {
            match o.restore(lost) {
                Ok(n) => notes.push(format!("{n} object files restored from the snapshot")),
                Err(e) => notes.push(format!("not restored: {e}")),
            }
            if self.check_git(full, baseline).is_ok() {
                return Err(with_notes(first, &notes));
            }
        }
        if snapshot.is_empty() {
            // No copy of the git files to go back to (an attempt whose only
            // git changes came from shell commands, each checked when it
            // ended): every file would count as new and be removed.
            return Err(format!(
                "{}; no copy of the refs to restore from",
                with_notes(first, &notes)
            ));
        }
        let changed = self.changed_files(snapshot)?;
        let invalid: Vec<PathBuf> = changed
            .iter()
            .filter(|p| !p.exists() || self.valid_file(p).is_err())
            .cloned()
            .collect();
        for path in &invalid {
            restore_file(path, snapshot.get(path).map(Vec::as_slice)).map_err(|e| {
                format!("{first}; and the restore of {} failed: {e}", path.display())
            })?;
        }
        if self.check_git(full, baseline).is_ok() {
            return Err(with_notes(first, &notes));
        }
        for path in &changed {
            restore_file(path, snapshot.get(path).map(Vec::as_slice)).map_err(|e| {
                format!("{first}; and the restore of {} failed: {e}", path.display())
            })?;
        }
        match self.check_git(full, baseline) {
            Ok(()) => Err(with_notes(first, &notes)),
            Err(still) => Err(format!(
                "{}; the snapshot was restored and the repository still fails its check: {still}",
                with_notes(first, &notes)
            )),
        }
    }
}

impl Drop for ProtectedWrites {
    /// The object snapshot goes with the attempt even when
    /// [`ProtectedWrites::finish`] never ran.
    fn drop(&mut self) {
        let mut st = self.state();
        if let Some(mut o) = st.objects.take() {
            o.remove();
        }
    }
}

/// The restore's notes first: a long check output (fsck) is cut short where
/// it is shown, and what was put back must survive the cut.
fn with_notes(first: String, notes: &[String]) -> String {
    if notes.is_empty() {
        first
    } else {
        format!("{}; the check found: {first}", notes.join("; "))
    }
}

fn push_failure(st: &mut State, line: String) {
    st.failure = Some(match st.failure.take() {
        Some(prev) => format!("{prev}\n{line}"),
        None => line,
    });
}

fn is_object_id(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn push_if_file(files: &mut Vec<PathBuf>, path: PathBuf) {
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
        files.push(path);
    }
}

/// Every regular file under `dir` (none when it is missing). Links are not
/// followed: a link in the git directory is not a file a snapshot restores.
fn walk_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let ty = entry
            .file_type()
            .map_err(|e| format!("cannot read {}: {e}", entry.path().display()))?;
        if ty.is_dir() {
            walk_files(&entry.path(), files)?;
        } else if ty.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

/// `path` with `/` between its components on every platform: a path shown to
/// a worker or a person is spelled as the repository spells it
/// (`.git/refs/heads/x`), not in the host's separator.
fn slashed(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// A file's bytes, `None` when it does not exist; refused over `limit`.
fn read_bounded(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if !m.is_file() => Err(format!("{} is not a regular file", path.display())),
        Ok(m) if m.len() > limit => Err(format!(
            "{} is {} bytes, over the {limit}-byte snapshot bound",
            path.display(),
            m.len()
        )),
        Ok(_) => std::fs::read(path)
            .map(Some)
            .map_err(|e| format!("cannot read {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Put `saved` back at `path` (remove it when `None`). Whatever is there now,
/// a link or a directory included, is removed first, so the restore writes a
/// plain file and never through a link.
fn restore_file(path: &Path, saved: Option<&[u8]>) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path).map_err(|e| e.to_string())?,
        Ok(_) => std::fs::remove_file(path).map_err(|e| e.to_string())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    if let Some(bytes) = saved {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, bytes).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
