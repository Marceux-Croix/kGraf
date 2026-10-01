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
//!   (a shell git command in Bypass, whose window snapshots the store).
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
//! - *Serialization.* Every git window (a protected git write, or a Bypass
//!   shell command with the shared git directory writable) holds the
//!   repository's lock exclusively ([`kgraf_sandbox::repo_lock`]) from its
//!   snapshot through its check and any restore; each git process kGraf runs
//!   on the repository holds it shared. So while a window is open no other
//!   worker and no kGraf merge, ref update or worktree change touches the
//!   repository, and the check sees only the window's own change. Two kGraf
//!   processes on one repository serialize the same way (the lock is a
//!   `flock`). A wait longer than the lock timeout fails loud: the write is
//!   not made, the command is not run.
//! - *Which shell commands open a window.* In Bypass, a command whose words
//!   name git (any word containing `git`, case-insensitively: the program,
//!   a `.git` path, a script that runs it) or the shared git directory's
//!   path. Every other command runs with the git directory read-only, as in
//!   every other mode, and holds no lock, so builds and tests of parallel
//!   workers never queue behind each other; a git write such a command makes
//!   anyway fails with a permission error, and a failed command's output
//!   says why.
//! - *Snapshot.* At the start of each window, under the lock: for git, the
//!   files a restore needs, copied byte for byte: the shared `config`,
//!   `HEAD`, `packed-refs`, `shallow`, every file under `refs/` and `info/`,
//!   each worktree admin directory's `HEAD`, `gitdir` and `commondir`, and
//!   this worktree's `index`. Bounded by [`GIT_SNAPSHOT_LIMIT`] in total; a
//!   repository over the bound gets no protected git write (refused, with the
//!   size). For a shell window, also the object store
//!   ([`objects`]): hard links into a directory under the project's state
//!   directory (outside every sandbox writable root), taken at the attempt's
//!   first shell window and brought up to date at each later one; copies
//!   bounded by [`OBJECT_COPY_LIMIT`] where hard links are impossible. For a
//!   settings file, its bytes (or its absence), bounded by
//!   [`SETTINGS_FILE_LIMIT`]. What the per-write check finds at the start of
//!   a window is the repository's own state, not counted against the window.
//! - *Check,* at the end of each window, before the tool call returns, so the
//!   run's next git operation never meets an unchecked write: the
//!   repository's config parses, `git for-each-ref` reports no broken ref,
//!   every ref and `HEAD` file names an object that exists (or is symbolic),
//!   this worktree's `HEAD` resolves to a commit and its index reads; and,
//!   when an object file the snapshot holds is missing or replaced, `git fsck
//!   --connectivity-only` over the shared repository (so a `git gc` that
//!   packed loose objects passes, and a deleted object does not). When the
//!   attempt ends, [`ProtectedWrites::finish`] runs that fsck once more under
//!   the lock, then deletes the object snapshot. A settings file is checked
//!   by the validator the host gives (the same parser the host loads it
//!   with).
//! - *On a failed check:* the object files that are missing or replaced are
//!   put back from the snapshot (each verified first) and new object files
//!   are kept; then, if the check still fails, the snapshotted files that
//!   changed and are invalid on their own; then every changed one. If the
//!   check still fails after that, its output says so. Either way the attempt
//!   is marked failed with the check's output ([`ProtectedWrites::failure`]),
//!   the window is recorded as `restored: <output>`, and the worker's result
//!   is failed, so it is never merged: the damage was the attempt's own
//!   (the lock rules out anyone else), and a repository made whole again
//!   does not make the attempt's work trustworthy.
//! - *A passing write* is kept and recorded (`kept`) with its path, worker and
//!   mode, as a [`PermissionEvent::ProtectedWrite`] the run loop traces and
//!   puts on the activity card.
//!
//! Known limits: an object file altered in place (git writes them read-only,
//! so this takes a `chmod`) under the hard-link method altered the snapshot
//! too, and is reported as not restorable. The end-of-attempt fsck counts a
//! problem against the attempt when the attempt's first window did not see
//! it, including one another worker's window left between this attempt's
//! windows (each window's own check already runs fsck when it loses an
//! object).

use crate::permissions::{PermissionControl, PermissionMode};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
// The tests build their fixtures with plain git.
#[cfg(test)]
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use kgraf_sandbox::repo_lock::{self, RepoLockGuard};

mod objects;
pub use objects::Method as ObjectSnapshotMethod;
use objects::ObjectSnapshot;

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
/// store over it gets no Bypass shell git window (the command runs with the
/// git directory read-only, and says why) rather than an unrestorable one.
/// Hard links have no bound: they copy no data.
pub const OBJECT_COPY_LIMIT: u64 = 512 * 1024 * 1024;

/// Where object snapshots live, under the project's state directory, which
/// is never a sandbox writable root.
pub const SNAPSHOT_DIR: &str = "git-snapshots";

/// Why a git window was not opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowError {
    /// The repository lock was not taken in time: nothing may run that
    /// writes the git directory. A shell command is not run.
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

/// An open Bypass shell git window: the git directory the sandbox may also
/// write, and the repository lock, held until
/// [`ProtectedWrites::after_shell`] has checked the command's changes.
#[derive(Debug)]
#[must_use = "pass the window to after_shell, which checks the command's git changes"]
pub struct ShellWindow {
    root: PathBuf,
    lock: Option<RepoLockGuard>,
}

impl ShellWindow {
    /// The shared git directory, to add as a sandbox writable root.
    pub fn root(&self) -> &Path {
        &self.root
    }
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
    /// The last good copy of the git files, by absolute path.
    git: Option<BTreeMap<PathBuf, Vec<u8>>>,
    /// The object store's snapshot, from the attempt's first shell window
    /// until [`ProtectedWrites::finish`].
    objects: Option<ObjectSnapshot>,
    /// A shell window was opened and not yet checked (its window was dropped
    /// without [`ProtectedWrites::after_shell`]): the next window or the
    /// finish checks first.
    shell_unchecked: bool,
    /// The last good copy of each settings file (`None`: it did not exist).
    settings: BTreeMap<String, Option<Vec<u8>>>,
    /// A git write or a Bypass shell command happened since the last full
    /// check.
    git_touched: bool,
    failure: Option<String>,
    /// Every path a protected write named, kept or restored.
    written: BTreeSet<String>,
    /// What the cheap repository check found when the current window
    /// opened (the repository's own state, not the window's).
    baseline: BTreeSet<String>,
    /// What it found when the attempt's first window opened, for the
    /// end-of-attempt check.
    first_baseline: BTreeSet<String>,
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
    state: Mutex<State>,
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
            state: Mutex::new(State::default()),
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

    /// Whether a shell command opens a git window in Bypass: one of its words
    /// contains `git` (any case) or the shared git directory's path.
    pub fn names_git(&self, argv: &[String]) -> bool {
        let common = self.common_dir.to_string_lossy();
        argv.iter().any(|w| {
            w.to_ascii_lowercase().contains("git") || (!common.is_empty() && w.contains(&*common))
        })
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
        let out = self
            .git(
                &self.admin_dir,
                [
                    OsStr::new("rev-parse"),
                    OsStr::new("--git-path"),
                    rest.as_os_str(),
                ],
            )
            .map_err(|e| format!("cannot resolve `.git/{}`: {e}", rest.display()))?;
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
            rest.display(),
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
                self.settle_unchecked(&mut st);
                self.open_git(&mut st, false)
                    .map_err(WindowError::Unprotected)?;
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

    /// The start of a git window, under the lock: the repository's state now
    /// is the baseline and the snapshot; a shell window also brings the
    /// object snapshot up to date (taking it at the attempt's first one).
    fn open_git(&self, st: &mut State, shell: bool) -> Result<(), String> {
        let baseline: BTreeSet<String> = self.problems(false)?.into_iter().collect();
        let snapshot = self.snapshot_git()?;
        if shell {
            match st.objects.as_mut() {
                Some(o) => o.sync()?,
                None => {
                    st.objects = Some(ObjectSnapshot::take(
                        &self.common_dir.join("objects"),
                        self.snapshot_dir(),
                        self.force_object_copy,
                        self.object_copy_limit,
                    )?);
                }
            }
        }
        if st.git.is_none() {
            st.first_baseline = baseline.clone();
        }
        st.baseline = baseline;
        st.git = Some(snapshot);
        Ok(())
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
                self.check_git_and_restore(&mut st, false, &[])
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

    /// Before a shell command in Bypass that names git ([`Self::names_git`]):
    /// take the repository lock and the snapshots, and return the window
    /// whose root the sandbox may also write. `Ok(None)` outside Bypass and
    /// for a command that does not name git (it runs with the git directory
    /// read-only).
    pub fn before_shell(&self, argv: &[String]) -> Result<Option<ShellWindow>, WindowError> {
        if !self.bypass() || !self.names_git(argv) {
            return Ok(None);
        }
        let lock = self.lock("shell git command")?;
        let mut st = self.state();
        self.settle_unchecked(&mut st);
        self.open_git(&mut st, true)
            .map_err(WindowError::Unprotected)?;
        st.shell_unchecked = true;
        Ok(Some(ShellWindow {
            root: self.common_dir.clone(),
            lock: Some(lock),
        }))
    }

    /// After a shell command that ran with the git directory writable, still
    /// under its lock: if a snapshotted file changed or an object file is
    /// missing or replaced, check (and restore on failure), recording the
    /// changed files as one protected write. The lock is released when this
    /// returns.
    pub fn after_shell(&self, mut window: ShellWindow) -> Result<(), String> {
        let _lock = window.lock.take();
        let mut st = self.state();
        st.shell_unchecked = false;
        self.check_shell(&mut st)
    }

    fn check_shell(&self, st: &mut State) -> Result<(), String> {
        let Some(snapshot) = st.git.as_ref() else {
            return Ok(());
        };
        let changed = match self.changed_files(snapshot) {
            Ok(c) => c,
            Err(e) => return self.settle(st, ".git", Err(e)),
        };
        let lost = match st.objects.as_ref().map(ObjectSnapshot::lost).transpose() {
            Ok(l) => l.unwrap_or_default(),
            Err(e) => return self.settle(st, ".git/objects", Err(e)),
        };
        if changed.is_empty() && lost.is_empty() {
            return Ok(());
        }
        st.git_touched = true;
        // Exact: the window held the repository lock, so every change since
        // its snapshot is the command's own.
        let mut shown: Vec<String> = changed.iter().map(|p| self.shown_git_path(p)).collect();
        if !lost.is_empty() {
            shown.push(format!(
                ".git/objects ({} object files removed or replaced)",
                lost.len()
            ));
        }
        let result = self.check_git_and_restore(st, !lost.is_empty(), &lost);
        self.settle(st, &shown.join(", "), result)
    }

    /// A shell window dropped without its check (its call was abandoned): the
    /// lock was released at the drop, so check now, before anything else,
    /// and say the attribution is no longer exact.
    fn settle_unchecked(&self, st: &mut State) {
        if std::mem::take(&mut st.shell_unchecked)
            && let Err(e) = self.check_shell(st)
        {
            push_failure(
                st,
                format!(
                    "{e} (checked after an abandoned shell command; the repository may have \
                     changed since)"
                ),
            );
        }
    }

    /// When the attempt ends: under the repository lock, the full check
    /// (`git fsck --connectivity-only` over the shared repository) if any git
    /// write happened, with the restore on failure; then the object snapshot
    /// is deleted, and the first failure, if any, is returned. The worker's
    /// result is failed on `Err`.
    pub fn finish(&self) -> Result<(), String> {
        let touched = {
            let st = self.state();
            (st.git_touched || st.shell_unchecked) && st.git.is_some()
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
            self.settle_unchecked(&mut st);
            st.git_touched = false;
            st.baseline = st.first_baseline.clone();
            let lost = match st.objects.as_ref().map(ObjectSnapshot::lost).transpose() {
                Ok(l) => l.unwrap_or_default(),
                Err(e) => {
                    let _ = self.settle(&mut st, ".git/objects", Err(e));
                    Vec::new()
                }
            };
            let result = self.check_git_and_restore(&mut st, true, &lost);
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
                return format!(".git/{}", rel.display());
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
        use std::io::Write;
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
        let mut cmd = match kgraf_core::host_git::command(&inv, &args) {
            Ok(cmd) => cmd,
            Err(kgraf_core::host_git::HostGitError::Config { stderr, .. }) => {
                return Ok((false, String::new(), stderr));
            }
            Err(e) => return Err(format!("cannot run git: {e}")),
        };
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("cannot run git: {e}"))?;
        let writer = child.stdin.take();
        let input = stdin.unwrap_or_default().to_vec();
        let output = std::thread::scope(|scope| {
            if let Some(mut w) = writer {
                scope.spawn(move || {
                    let _ = w.write_all(&input);
                });
            }
            child.wait_with_output()
        })
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
    /// changed one. A passing check refreshes the snapshot.
    fn check_git_and_restore(
        &self,
        st: &mut State,
        full: bool,
        lost: &[PathBuf],
    ) -> Result<(), String> {
        let first = match self.check_git(full, &st.baseline) {
            Ok(()) => {
                st.git = Some(self.snapshot_git()?);
                if let Some(o) = st.objects.as_mut() {
                    o.sync()?;
                }
                return Ok(());
            }
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
            if self.check_git(full, &st.baseline).is_ok() {
                return Err(with_notes(first, &notes));
            }
        }
        let snapshot = st.git.clone().unwrap_or_default();
        let changed = self.changed_files(&snapshot)?;
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
        if self.check_git(full, &st.baseline).is_ok() {
            return Err(with_notes(first, &notes));
        }
        for path in &changed {
            restore_file(path, snapshot.get(path).map(Vec::as_slice)).map_err(|e| {
                format!("{first}; and the restore of {} failed: {e}", path.display())
            })?;
        }
        match self.check_git(full, &st.baseline) {
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
        let st = self.state.get_mut().unwrap_or_else(|p| p.into_inner());
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
