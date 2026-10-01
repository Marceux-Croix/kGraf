//! One git worktree per worker (disjoint writes).
//!
//! Lifecycle ported from vibe-kanban `crates/worktree-manager/src/worktree_manager.rs`,
//! commit d5cbb5380fa0b32e98ef9b8d987f63decce4be3a, Apache-2.0
//! (see `third_party/vibe-kanban/UPSTREAM` for what changed). Combined with
//! this recipe:
//! prune unconditionally on startup; create every worktree locked with a
//! kGraf-tagged reason, so a crash leaves a locked entry, not a silent
//! orphan; on the next startup, unlock, force-remove and prune any tagged
//! entry whose directory is gone or whose owner process is dead.
//!
//! Layout under the app-owned base directory:
//!
//! ```text
//! <base>/.kgraf-workspaces   marker: names the one repository that owns
//!                                this base; also the cross-process lock file
//! <base>/<name>/                 the linked worktree (writable in the sandbox)
//! <base>/.scratch/<name>/        per-workspace TMPDIR (writable, outside git)
//! ```
//!
//! Concurrency: `create` and `remove` hold a shared lock, orphan cleanup an
//! exclusive one, both in-process ([`ScanGate`], first come first served) and
//! across processes (`flock` on the marker via `File::lock`). Cleanup therefore
//! never sees a half-created workspace, in this process or another. Lock order
//! is always gate, then `flock`, then the per-path busy set.
//!
//! Owner liveness: a lock records its owner's PID, start time and host
//! ([`HostId`]: machine, boot, PID namespace, time namespace), and one rule
//! judges it. Known limits, both from copying a machine: a clone that keeps
//! the original's `/etc/machine-id` judges the original's live owners dead
//! (as if this machine had rebooted), and two running restores of one memory
//! snapshot share a boot id and judge each other's owners by PID. See
//! [`HostId`].
//!
//! The API is synchronous: each call is a handful of short git processes.
//! Callers on an async runtime wrap calls in `spawn_blocking`.

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::error::WorkspaceError;
use crate::git::{Git, WorktreeEntry};
use kgraf_core::flock::HeldLock;

/// Prefix of every lock reason this crate writes. Orphan cleanup only touches
/// entries carrying it.
pub const LOCK_TAG: &str = "kgraf:v1";
/// [`LOCK_TAG`] under kGraf's former name (decision 0062). Locks written
/// before the rename carry it; they are read as ours, never rewritten, so a
/// worktree a Haustoria process locked is judged and cleaned like any other.
pub const LEGACY_LOCK_TAG: &str = "haustoria:v1";
const MARKER: &str = ".kgraf-workspaces";
/// [`MARKER`] under the former name. A base that has only this one (a state
/// directory moved from `.haustoria`) has it renamed on open.
const LEGACY_MARKER: &str = ".haustoria-workspaces";
const MARKER_HEADER: &str = "kgraf-workspaces v1";
const SCRATCH: &str = ".scratch";
/// Hold files, one per workspace, outside every writable root (a worker cannot
/// replace its own).
const HOLDS: &str = ".holds";

/// What to create.
#[derive(Debug, Clone)]
pub struct WorkspaceSpec<'a> {
    /// Directory and lock name: ASCII letters, digits, `-`, `_`, `.`; no leading `.`.
    pub name: &'a str,
    /// Commit-ish to check out (`HEAD`, a branch, a SHA).
    pub base: &'a str,
    /// Create this new branch at `base`; `None` checks out detached.
    pub branch: Option<&'a str>,
}

/// A live worker workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    name: String,
    root: PathBuf,
    scratch: PathBuf,
    git_common_dir: PathBuf,
    gitlink: String,
    head: String,
    hold: PathBuf,
}

impl Workspace {
    /// The file every process started in this worktree holds a shared lock on
    /// until it exits (`SandboxPolicy::holding`). Outside the writable roots.
    pub fn hold_file(&self) -> &Path {
        &self.hold
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Canonical worktree root. The only tracked-file write root for the worker.
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Canonical per-workspace scratch directory, used as TMPDIR.
    pub fn scratch(&self) -> &Path {
        &self.scratch
    }
    /// The repository's shared git directory. Read-only in the sandbox; git
    /// inside the worktree needs to read objects and refs from it.
    pub fn git_common_dir(&self) -> &Path {
        &self.git_common_dir
    }
    /// Commit checked out at creation.
    pub fn head(&self) -> &str {
        &self.head
    }
    /// The gitlink (`gitdir: ...`) recorded at creation. A caller that must
    /// reopen this worktree in a later process records it with [`Self::head`]
    /// and passes both to [`WorkspaceManager::reopen`].
    pub fn gitlink(&self) -> &str {
        &self.gitlink
    }

    /// This worktree's admin directory (`<common>/worktrees/<id>`), taken
    /// from the gitlink recorded at creation, never from the file on disk
    /// (the worker can write that file). Host-side git passes it as
    /// `--git-dir`, so a later swap of `<root>/.git` cannot redirect git.
    pub fn admin_dir(&self) -> Option<PathBuf> {
        let target = Path::new(self.gitlink.strip_prefix("gitdir:")?.trim());
        // git 2.48+ can write relative gitlinks (worktree.useRelativePaths).
        Some(if target.is_absolute() {
            target.to_path_buf()
        } else {
            self.root.join(target)
        })
    }

    /// Check that `<root>/.git` still points at this worktree's admin
    /// directory. A sandboxed worker can write its root, so on a backend
    /// without gitlink protection it can rewrite the gitlink and aim later
    /// host-side git commands at another repository. Call this before any
    /// host-side git operation inside the workspace.
    pub fn verify_gitlink(&self) -> Result<(), WorkspaceError> {
        let path = self.root.join(".git");
        let found = std::fs::read_to_string(&path).map_err(|e| WorkspaceError::io(&path, e))?;
        if found.trim() == self.gitlink {
            Ok(())
        } else {
            Err(WorkspaceError::GitlinkTampered {
                path: self.root.clone(),
                expected: self.gitlink.clone(),
                found: found.trim().to_string(),
            })
        }
    }
}

/// A worktree to reopen: what was recorded when it was created.
#[derive(Debug, Clone)]
pub struct ReopenSpec<'a> {
    /// The workspace name it was created under.
    pub name: &'a str,
    /// [`Workspace::gitlink`] as recorded at creation, never re-read from disk.
    pub gitlink: &'a str,
    /// [`Workspace::head`] as recorded at creation: the base its changes are
    /// measured against.
    pub base: &'a str,
    /// The branch attempts merge into (`refs/heads/...` or a short name). A
    /// merge-back commit already reachable from it was merged before the
    /// kill: the worktree is retired, never rolled back and merged again.
    pub integration: Option<&'a str>,
}

/// Why startup cleanup removed an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanCause {
    /// Tagged lock, directory gone.
    DirectoryMissing,
    /// Tagged lock, owner process no longer running (or its PID was reused
    /// by a process with a different start time).
    OwnerDead { pid: u32 },
    /// A directory under the base with no worktree registration (crash between
    /// directory creation and registration, or a leftover scratch directory).
    Unregistered,
}

/// The exclusive hold [`WorkspaceManager::quiesce`] took: while it lives, no
/// sandboxed process can start in the worktree. Released on drop.
#[derive(Debug)]
pub struct QuietGuard(#[allow(dead_code)] HeldLock);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanRemoval {
    pub path: PathBuf,
    pub cause: OrphanCause,
}

/// Manages the workspaces of one repository under one base directory.
pub struct WorkspaceManager {
    git: Git,
    repo: PathBuf,
    common_dir: PathBuf,
    base: PathBuf,
    /// Shared: create and remove. Exclusive: orphan cleanup.
    scan: ScanGate,
    /// Per-path guard replacing upstream's process-global mutex map: create
    /// and remove of the same path never interleave.
    busy: Mutex<HashSet<PathBuf>>,
    freed: Condvar,
}

struct PathGuard<'a> {
    mgr: &'a WorkspaceManager,
    path: PathBuf,
}

impl Drop for PathGuard<'_> {
    fn drop(&mut self) {
        // A poisoned set means another thread panicked mid-lifecycle; the
        // entry is released anyway so the panic does not deadlock others.
        let mut set = match self.mgr.busy.lock() {
            Ok(set) => set,
            Err(poisoned) => poisoned.into_inner(),
        };
        set.remove(&self.path);
        self.mgr.freed.notify_all();
    }
}

/// The in-process half of the scan lock: a reader-writer gate that admits
/// waiters in arrival order, consecutive shared holders together.
///
/// Why not `std::sync::RwLock`: it promises no ordering, and on macOS a thread
/// that releases the write lock and asks again at once takes it back before a
/// woken reader is scheduled. A cleanup loop (`startup` back to back) then
/// starved `create` for over 60 s under load; `sample` showed the creating
/// thread parked in `rwlock::queue::lock_contended` for the whole window
/// while both cleanup threads alternated. Cleanup waits for its `flock` while
/// holding the exclusive side, so even without a loop an unfair lock lets a
/// burst of cleanups hold off creates. A ticket makes every waiter's turn
/// come; the cost is one mutex and condvar round per lock, next to the git
/// processes each call runs.
struct ScanGate {
    state: Mutex<GateState>,
    turn: Condvar,
}

#[derive(Default)]
struct GateState {
    /// Next ticket to hand out.
    next: u64,
    /// The ticket allowed to enter next.
    serving: u64,
    readers: u32,
    writer: bool,
}

struct GateGuard<'a> {
    gate: &'a ScanGate,
    exclusive: bool,
}

impl ScanGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(GateState::default()),
            turn: Condvar::new(),
        }
    }

    /// Wait for this caller's turn. Errors only if the state mutex is
    /// poisoned, which needs a panic inside this module's short critical
    /// sections.
    fn enter(&self, exclusive: bool) -> Result<GateGuard<'_>, ()> {
        let mut state = self.state.lock().map_err(|_| ())?;
        let ticket = state.next;
        state.next += 1;
        let may_enter =
            |st: &GateState| st.serving == ticket && !st.writer && (!exclusive || st.readers == 0);
        while !may_enter(&state) {
            state = self.turn.wait(state).map_err(|_| ())?;
        }
        state.serving += 1;
        if exclusive {
            state.writer = true;
        } else {
            state.readers += 1;
        }
        // The next ticket may be a reader that can share with this one.
        self.turn.notify_all();
        Ok(GateGuard {
            gate: self,
            exclusive,
        })
    }

    fn state(&self) -> MutexGuard<'_, GateState> {
        // Released even when poisoned, so one panic does not deadlock others.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.state();
        if self.exclusive {
            state.writer = false;
        } else {
            state.readers -= 1;
        }
        self.gate.turn.notify_all();
    }
}

/// Shared lock: in-process shared gate plus a shared `flock` on the marker.
/// Field order is drop order: the file lock is released first.
struct SharedLock<'a> {
    _file: HeldLock,
    _guard: GateGuard<'a>,
}

/// Exclusive lock for orphan cleanup.
struct ExclusiveLock<'a> {
    _file: HeldLock,
    _guard: GateGuard<'a>,
}

impl WorkspaceManager {
    /// Open the repository containing `repo` and an app-owned `base` directory.
    ///
    /// `base` is created if absent and stamped with the canonical repository
    /// path. A base stamped for another repository is refused, as is an
    /// existing non-empty `base` without the stamp, because orphan cleanup
    /// deletes unregistered directories inside it. `base` must not contain
    /// the repository.
    pub fn open(repo: &Path, base: &Path) -> Result<Self, WorkspaceError> {
        let git = Git::discover()?;
        let top = git
            .run_str(repo, ["rev-parse", "--show-toplevel"])
            .map_err(|_| WorkspaceError::NotARepository(repo.to_path_buf()))?;
        let repo = canonical(Path::new(&top))?;
        let common = git.run_str(
            &repo,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common_dir = canonical(Path::new(&common))?;

        let existed_nonempty = std::fs::read_dir(base)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
        std::fs::create_dir_all(base).map_err(|e| WorkspaceError::io(base, e))?;
        let base = canonical(base)?;
        if repo.starts_with(&base) {
            return Err(WorkspaceError::UnsafeBaseDir {
                path: base,
                reason: "base directory contains the repository".into(),
            });
        }
        let marker = base.join(MARKER);
        adopt_legacy_marker(&base)?;
        if existed_nonempty && !marker.is_file() {
            return Err(WorkspaceError::UnsafeBaseDir {
                path: base,
                reason: "directory is not empty and was not created by kGraf".into(),
            });
        }
        stamp_or_check_marker(&marker, &repo)?;
        let scratch = base.join(SCRATCH);
        std::fs::create_dir_all(&scratch).map_err(|e| WorkspaceError::io(&scratch, e))?;

        Ok(Self {
            git,
            repo,
            common_dir,
            base,
            scan: ScanGate::new(),
            busy: Mutex::new(HashSet::new()),
            freed: Condvar::new(),
        })
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Startup recipe: prune unconditionally, then clean orphans. Safe to run
    /// while this or another process creates workspaces on the same base.
    pub fn startup(&self) -> Result<Vec<OrphanRemoval>, WorkspaceError> {
        self.startup_keeping(&|_| false)
    }

    /// [`Self::startup`], keeping every dead owner's worktree whose name
    /// `referenced` accepts: a worktree a run record still names is a killed
    /// worker's work, which a resume reopens ([`Self::reopen`]). Everything
    /// else a dead owner left (conversation and probe worktrees, worktrees
    /// dropped from their record) is removed.
    pub fn startup_keeping(
        &self,
        referenced: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<OrphanRemoval>, WorkspaceError> {
        let _lock = self.exclusive()?;
        self.git.run(&self.repo, ["worktree", "prune"])?;
        self.clean_orphans_locked(referenced)
    }

    /// Registered worktrees of the repository, including the main one.
    pub fn list(&self) -> Result<Vec<WorktreeEntry>, WorkspaceError> {
        self.git.worktree_list(&self.repo)
    }

    /// kGraf worktrees under this base whose owner cannot be judged from
    /// here ([`Liveness::Unknowable`]: another machine or PID namespace, an
    /// unreadable boot, or another boot of an unknown machine), and those
    /// whose kGraf lock cannot be read at all ([`is_unreadable_own_lock`]).
    /// Nothing removes them on its own; they are listed so a person can, with
    /// [`Self::clean`].
    pub fn stranded(&self) -> Result<Vec<PathBuf>, WorkspaceError> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|e| e.path.starts_with(&self.base))
            .filter(|e| {
                e.locked.as_deref().is_some_and(|reason| {
                    is_unreadable_own_lock(reason)
                        || parse_owner(reason).is_some_and(|o| o.liveness() == Liveness::Unknowable)
                })
            })
            .map(|e| e.path)
            .collect())
    }

    /// Remove the worktree `name` on a person's word (`kgraf clean`): the
    /// way out for a stranded worktree. Refused while its owner is known to
    /// be running here, or while any process started in it runs. A session
    /// logged in another PID namespace cannot be seen from here and does not
    /// stop it: this is the way out for stranded work, on a person's word.
    pub fn clean(&self, name: &str) -> Result<PathBuf, WorkspaceError> {
        validate_name(name)?;
        let path = self.base.join(name);
        let _lock = self.exclusive()?;
        let entry = self
            .list()?
            .into_iter()
            .find(|e| e.path == path)
            .ok_or_else(|| WorkspaceError::NotReopenable {
                path: path.clone(),
                reason: "not a registered worktree of this repository".into(),
            })?;
        match entry.locked.as_deref().map(|r| (r, parse_owner(r))) {
            Some((_, Some(holder))) if holder.liveness() == Liveness::Alive => {
                return Err(WorkspaceError::Busy {
                    path,
                    reason: format!(
                        "its owner (pid {}) is still running here; stop it first",
                        holder.pid
                    ),
                });
            }
            // kGraf's own lock, unreadable: no owner to judge, so it is
            // removed on the person's word, as a stranded worktree is.
            Some((reason, None)) if !is_unreadable_own_lock(reason) => {
                return Err(WorkspaceError::NotReopenable {
                    path,
                    reason: format!("locked by someone else: {reason}"),
                });
            }
            _ => {}
        }
        let hold = self.hold_path(name);
        let _held = self.quiet(&path, &hold, false)?;
        self.purge(&path)?;
        remove_dir_if_exists(&self.base.join(SCRATCH).join(name))?;
        remove_file_if_exists(&session_log(&hold))?;
        Ok(path)
    }

    /// Remove tagged entries whose directory is gone or whose owner is dead,
    /// then unregistered directories under the base.
    fn clean_orphans_locked(
        &self,
        referenced: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<OrphanRemoval>, WorkspaceError> {
        let mut removed = Vec::new();
        for entry in self.list()? {
            let Some(holder) = entry.locked.as_deref().and_then(parse_owner) else {
                continue;
            };
            // Only entries under our base: a tag in another tree is not ours to delete.
            if !entry.path.starts_with(&self.base) {
                continue;
            }
            let cause = if !entry.path.exists() {
                OrphanCause::DirectoryMissing
            } else if entry
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(referenced)
            {
                continue;
            } else if !holder.alive() {
                OrphanCause::OwnerDead { pid: holder.pid }
            } else {
                continue;
            };
            // The owner is dead, but an orphan it started may still write in
            // the worktree: left alone until it ends (the next startup).
            let _held = match entry.path.file_name().and_then(|n| n.to_str()) {
                Some(name) if cause != OrphanCause::DirectoryMissing => {
                    match self.quiet(&entry.path, &self.hold_path(name), true) {
                        Ok(held) => Some(held),
                        Err(WorkspaceError::Busy { .. }) => continue,
                        Err(e) => return Err(e),
                    }
                }
                _ => None,
            };
            self.purge(&entry.path)?;
            removed.push(OrphanRemoval {
                path: entry.path.clone(),
                cause,
            });
        }

        let registered: HashSet<PathBuf> = self.list()?.into_iter().map(|e| e.path).collect();
        for dir in read_subdirs(&self.base)? {
            if dir.file_name().is_some_and(|n| n == SCRATCH || n == HOLDS)
                || registered.contains(&dir)
            {
                continue;
            }
            self.check_not_foreign(&dir)?;
            remove_dir_if_exists(&dir)?;
            removed.push(OrphanRemoval {
                path: dir,
                cause: OrphanCause::Unregistered,
            });
        }
        for dir in read_subdirs(&self.base.join(SCRATCH))? {
            let Some(name) = dir.file_name() else {
                continue;
            };
            if !registered.contains(&self.base.join(name)) {
                remove_dir_if_exists(&dir)?;
                removed.push(OrphanRemoval {
                    path: dir,
                    cause: OrphanCause::Unregistered,
                });
            }
        }
        Ok(removed)
    }

    /// Defense in depth behind the marker: an unregistered directory that is a
    /// repository, or a worktree of another repository, is never deleted.
    fn check_not_foreign(&self, dir: &Path) -> Result<(), WorkspaceError> {
        let dotgit = dir.join(".git");
        let foreign = if dotgit.is_dir() {
            true
        } else if dotgit.is_file() {
            let text =
                std::fs::read_to_string(&dotgit).map_err(|e| WorkspaceError::io(&dotgit, e))?;
            let target = text.trim().strip_prefix("gitdir:").map(str::trim);
            !target.is_some_and(|t| {
                let t = Path::new(t);
                std::fs::canonicalize(t)
                    .unwrap_or_else(|_| t.to_path_buf())
                    .starts_with(&self.common_dir)
            })
        } else {
            false
        };
        if foreign {
            return Err(WorkspaceError::UnsafeBaseDir {
                path: dir.to_path_buf(),
                reason: format!(
                    "contains a repository or worktree not belonging to {}; not deleting it",
                    self.repo.display()
                ),
            });
        }
        Ok(())
    }

    /// Create a locked worktree for one worker.
    pub fn create(&self, spec: &WorkspaceSpec<'_>) -> Result<Workspace, WorkspaceError> {
        validate_name(spec.name)?;
        if let Some(branch) = spec.branch {
            // `check-ref-format --branch` rejects names git would refuse later,
            // before any state is created.
            self.git
                .run(&self.repo, ["check-ref-format", "--branch", branch])?;
        }
        let path = self.base.join(spec.name);
        let _lock = self.shared()?;
        let _guard = self.guard(&path)?;

        match self.list()?.into_iter().find(|e| e.path == path) {
            Some(entry) => {
                // kGraf wrote this lock, but its owner cannot be read (a
                // truncated or damaged reason), so no liveness rule applies.
                // Never replaced unasked, and never kept without a word: the
                // error names the way out.
                if let Some(reason) = entry
                    .locked
                    .as_deref()
                    .filter(|r| is_unreadable_own_lock(r))
                {
                    return Err(WorkspaceError::UnreadableLock {
                        path,
                        name: spec.name.to_owned(),
                        lock: reason.to_owned(),
                    });
                }
                // Keep it when a live kGraf owner holds it, or when the
                // lock is not ours at all (a user lock is never overridden).
                let keep = match entry.locked.as_deref() {
                    Some(reason) => match parse_owner(reason) {
                        Some(holder) => holder.alive() && path.exists(),
                        None => true,
                    },
                    None => false,
                };
                if keep {
                    return Err(WorkspaceError::AlreadyExists(path));
                }
                self.purge(&path)?;
            }
            None => {
                self.check_not_foreign(&path)?;
                remove_dir_if_exists(&path)?;
            }
        }

        let reason = lock_reason(spec.name)?;
        let mut args: Vec<&std::ffi::OsStr> = vec![
            "worktree".as_ref(),
            "add".as_ref(),
            "--lock".as_ref(),
            "--reason".as_ref(),
            reason.as_ref(),
        ];
        match spec.branch {
            Some(branch) => {
                args.push("-b".as_ref());
                args.push(branch.as_ref());
            }
            None => args.push("--detach".as_ref()),
        }
        args.push(path.as_os_str());
        args.push(spec.base.as_ref());

        // Upstream retries once after clearing stale metadata: a crashed
        // earlier add can leave an admin entry that blocks this one.
        if let Err(first) = self.git.run(&self.repo, &args) {
            self.purge(&path)?;
            self.git
                .run(&self.repo, &args)
                .map_err(|second| match second {
                    WorkspaceError::GitFailed {
                        args,
                        cwd,
                        code,
                        stderr,
                    } => WorkspaceError::GitFailed {
                        args,
                        cwd,
                        code,
                        stderr: format!("{stderr} (first attempt: {first})"),
                    },
                    other => other,
                })?;
        }
        if !path.is_dir() {
            return Err(WorkspaceError::MissingAfterCreate(path));
        }

        let gitlink_path = path.join(".git");
        let gitlink = std::fs::read_to_string(&gitlink_path)
            .map_err(|e| WorkspaceError::io(&gitlink_path, e))?
            .trim()
            .to_string();
        let head = self.git.run_str(&path, ["rev-parse", "HEAD"])?;
        let scratch = self.base.join(SCRATCH).join(spec.name);
        remove_dir_if_exists(&scratch)?;
        std::fs::create_dir_all(&scratch).map_err(|e| WorkspaceError::io(&scratch, e))?;

        Ok(Workspace {
            name: spec.name.to_string(),
            root: path,
            scratch,
            git_common_dir: self.common_dir.clone(),
            gitlink,
            head,
            hold: {
                let hold = self.hold_path(spec.name);
                // A new worktree: sessions logged under an earlier one of this
                // name are not its own.
                remove_file_if_exists(&session_log(&hold))?;
                hold
            },
        })
    }

    /// Reopen a worktree an earlier process (or this one) created, by its
    /// recorded gitlink, instead of replacing it: a killed run continues its
    /// worker in the tree the worker wrote, with its changes.
    ///
    /// The tree is reopened only when it is intact; otherwise nothing is
    /// changed and [`WorkspaceError::NotReopenable`] (or `GitlinkTampered`)
    /// says why:
    /// - it is registered under this base and its lock is kGraf's, held by
    ///   a dead owner or by this process (a live other owner keeps it);
    /// - `<root>/.git` is exactly the recorded gitlink, and the admin directory
    ///   it names is under this repository's `worktrees/` and points back at
    ///   `<root>/.git` (a worker can write its root, never the admin directory);
    /// - HEAD is the recorded base, or is a merge-back commit of this worktree:
    ///   a commit whose first parent is the base and that an attempt ref of
    ///   this workspace name (`<name>` or `<name>+<n>`) points at, which is a
    ///   verification the earlier process committed but never merged. HEAD and
    ///   the index are moved back to the base, the working tree keeps every
    ///   change, and the old attempt ref is kept: a numbered re-verification
    ///   ([`crate::MergeBack::attempt_numbered`]) commits under a new ref and
    ///   never collides with it. Any other HEAD (a worker's own commit) is not
    ///   intact.
    ///
    /// The lock is then re-tagged with this process as its owner. Git runs with
    /// hooks off and an explicit `--git-dir` taken from the recorded gitlink,
    /// never following the worker-writable `.git` file.
    pub fn reopen(&self, spec: &ReopenSpec<'_>) -> Result<Workspace, WorkspaceError> {
        validate_name(spec.name)?;
        let path = self.base.join(spec.name);
        let refuse = |reason: String| WorkspaceError::NotReopenable {
            path: path.clone(),
            reason,
        };
        let _lock = self.shared()?;
        let _guard = self.guard(&path)?;

        let entry = self
            .list()?
            .into_iter()
            .find(|e| e.path == path)
            .ok_or_else(|| refuse("not a registered worktree of this repository".into()))?;
        if !path.is_dir() {
            return Err(refuse("its directory is gone".into()));
        }
        match entry.locked.as_deref().map(|r| (r, parse_owner(r))) {
            Some((_, Some(holder))) if holder.is_self() || !holder.alive() => {}
            Some((_, Some(holder))) if holder.liveness() == Liveness::Alive => {
                return Err(WorkspaceError::Busy {
                    path: path.clone(),
                    reason: format!("held by a live process (pid {})", holder.pid),
                });
            }
            Some((_, Some(_))) => {
                return Err(refuse(
                    "its owner ran on another machine or in another PID namespace (or its \
                     boot is unreadable, or it ran in another boot of a machine not known \
                     to be this one), so nothing here can tell whether it still runs"
                        .into(),
                ));
            }
            Some((reason, None)) if is_unreadable_own_lock(reason) => {
                return Err(refuse(format!(
                    "its kGraf lock cannot be read ({reason:?}), so nothing can tell \
                     whether its owner still runs; check, then run `kgraf clean {}`",
                    spec.name
                )));
            }
            Some((reason, None)) => {
                return Err(refuse(format!("locked by someone else: {reason}")));
            }
            None => return Err(refuse("not locked by kGraf".into())),
        }

        let gitlink_path = path.join(".git");
        let found = std::fs::read_to_string(&gitlink_path)
            .map_err(|e| WorkspaceError::io(&gitlink_path, e))?;
        if found.trim() != spec.gitlink {
            return Err(WorkspaceError::GitlinkTampered {
                path: path.clone(),
                expected: spec.gitlink.to_string(),
                found: found.trim().to_string(),
            });
        }
        let ws = Workspace {
            name: spec.name.to_string(),
            root: path.clone(),
            scratch: self.base.join(SCRATCH).join(spec.name),
            git_common_dir: self.common_dir.clone(),
            gitlink: spec.gitlink.to_string(),
            head: String::new(),
            hold: self.hold_path(spec.name),
        };
        // No process started in this worktree may still be running: an orphan
        // of a killed orchestrator would keep writing while the worker is
        // continued. Held exclusively until the reopen is done. Two checks:
        // the inherited lock, and (for a process that closed its inherited
        // descriptors) the sessions its sandboxed children started.
        let _held = self.quiet(&path, &ws.hold, true)?;
        let admin = ws
            .admin_dir()
            .ok_or_else(|| refuse("the recorded gitlink names no admin directory".into()))?;
        let admin = canonical(&admin).map_err(|_| refuse("its admin directory is gone".into()))?;
        if !admin.starts_with(self.common_dir.join("worktrees")) {
            return Err(refuse(format!(
                "the recorded gitlink names {}, outside this repository",
                admin.display()
            )));
        }
        let back_file = admin.join("gitdir");
        let back =
            std::fs::read_to_string(&back_file).map_err(|e| WorkspaceError::io(&back_file, e))?;
        let back = Path::new(back.trim());
        let back = if back.is_absolute() {
            back.to_path_buf()
        } else {
            admin.join(back)
        };
        if canonical(&back).ok() != Some(canonical(&gitlink_path)?) {
            return Err(refuse(format!(
                "its admin directory points at {}, not at this worktree",
                back.display()
            )));
        }

        // Hooks off: nothing here runs code the repository configured.
        let in_admin = |args: &[&str]| {
            let mut all: Vec<std::ffi::OsString> = vec![
                "-c".into(),
                "core.hooksPath=/dev/null".into(),
                "--git-dir".into(),
                admin.as_os_str().to_owned(),
            ];
            all.extend(args.iter().map(std::ffi::OsString::from));
            self.git.run_str(&self.repo, all)
        };
        let base_spec = format!("{}^{{commit}}", spec.base);
        let base = self
            .git
            .run_str(&self.repo, ["rev-parse", "--verify", base_spec.as_str()])
            .map_err(|_| refuse(format!("the recorded base {} is not a commit", spec.base)))?;
        let head = in_admin(&["rev-parse", "--verify", "HEAD^{commit}"])?;
        if head != base {
            let parent_spec = format!("{head}^1");
            let parent = self
                .git
                .run_str(
                    &self.repo,
                    ["rev-parse", "--verify", "--quiet", parent_spec.as_str()],
                )
                .unwrap_or_default();
            let exact = format!("{}{}", crate::merge::ATTEMPT_REF_PREFIX, spec.name);
            let numbered = format!("{exact}+");
            let refs = self.git.run_str(
                &self.repo,
                [
                    "for-each-ref",
                    "--format=%(refname)",
                    "--points-at",
                    head.as_str(),
                    crate::merge::ATTEMPT_REF_PREFIX,
                ],
            )?;
            let ours = refs.lines().any(|r| r == exact || r.starts_with(&numbered));
            if parent != base || !ours {
                return Err(refuse(format!(
                    "HEAD is {head}, neither the recorded base {base} nor a merge-back \
                     commit of this worktree on it"
                )));
            }
            let our_refs: Vec<&str> = refs
                .lines()
                .filter(|r| *r == exact || r.starts_with(&numbered))
                .collect();
            // Merged, then killed before the worktree was retired: rolling back
            // would merge the same change a second time. Retire it instead.
            if let Some(branch) = spec.integration {
                let merged = self
                    .git
                    .run(
                        &self.repo,
                        ["merge-base", "--is-ancestor", head.as_str(), branch],
                    )
                    .is_ok();
                if merged {
                    for r in &our_refs {
                        self.git
                            .run(&self.repo, ["update-ref", "-d", r, head.as_str()])?;
                    }
                    self.purge(&path)?;
                    remove_dir_if_exists(&ws.scratch)?;
                    return Err(WorkspaceError::AlreadyMerged {
                        path: path.clone(),
                        commit: head,
                    });
                }
            }
            // Back to the base: HEAD by compare-and-swap, the index to the
            // base's tree. The working tree keeps every change. The unmerged
            // commit's attempt refs go with it (only while they still name it).
            in_admin(&["update-ref", "--no-deref", "HEAD", &base, &head])?;
            in_admin(&["read-tree", &base])?;
            for r in &our_refs {
                self.git
                    .run(&self.repo, ["update-ref", "-d", r, head.as_str()])?;
            }
        }

        // This process owns it now.
        self.git.run(
            &self.repo,
            ["worktree".as_ref(), "unlock".as_ref(), path.as_os_str()],
        )?;
        let reason = lock_reason(spec.name)?;
        self.git.run(
            &self.repo,
            [
                "worktree".as_ref(),
                "lock".as_ref(),
                "--reason".as_ref(),
                std::ffi::OsStr::new(&reason),
                path.as_os_str(),
            ],
        )?;
        std::fs::create_dir_all(&ws.scratch).map_err(|e| WorkspaceError::io(&ws.scratch, e))?;
        // Every logged session has ended: start the log afresh.
        remove_file_if_exists(&session_log(&ws.hold))?;
        Ok(Workspace { head: base, ..ws })
    }

    /// [`WorkspaceError::Busy`] unless no process started in `ws` (through a
    /// sandbox policy that holds it, as every worker shell and check does)
    /// still runs, including one that left its process group or session. The
    /// same hold and session check as reopen and startup cleanup. The returned
    /// guard keeps the hold exclusively: no sandboxed process can start in the
    /// worktree until it is dropped. Drop it before [`WorkspaceManager::remove`],
    /// which takes the hold itself.
    pub fn quiesce(&self, ws: &Workspace) -> Result<QuietGuard, WorkspaceError> {
        self.quiet(&ws.root, &ws.hold, true).map(QuietGuard)
    }

    /// Remove a workspace: unlock, force-remove, delete scratch, and verify
    /// that neither the directory nor the registration remains. The branch,
    /// if any, is kept; merge-back owns branch lifecycle.
    ///
    /// [`WorkspaceError::Busy`] while a process started in the worktree still
    /// runs (the same hold and session check as [`Self::quiesce`]): deleting
    /// the tree under a live writer would let it recreate files in a path
    /// git no longer tracks. The hold is kept exclusively until the removal
    /// is done, so no sandboxed process can start meanwhile. A busy
    /// worktree stays registered and locked by this process; startup cleanup
    /// removes it once its processes have ended.
    pub fn remove(&self, ws: Workspace) -> Result<(), WorkspaceError> {
        let _lock = self.shared()?;
        let _guard = self.guard(&ws.root)?;
        let _held = self.quiet_retrying(&ws.root, &ws.hold)?;
        self.purge(&ws.root)?;
        remove_dir_if_exists(&ws.scratch)
    }

    /// [`Self::quiet`], retried for up to about one second while it reads
    /// busy: a command that has just been stopped can still hold its shared
    /// lock for a moment while the kernel tears its processes down.
    fn quiet_retrying(&self, path: &Path, hold: &Path) -> Result<HeldLock, WorkspaceError> {
        const TRIES: u32 = 40;
        const PAUSE: std::time::Duration = std::time::Duration::from_millis(25);
        let mut tries = 0;
        loop {
            match self.quiet(path, hold, true) {
                Err(WorkspaceError::Busy { .. }) if tries + 1 < TRIES => {
                    tries += 1;
                    std::thread::sleep(PAUSE);
                }
                other => return other,
            }
        }
    }

    /// Unlock, remove and prune one path, then verify the outcome. Upstream
    /// treats each step's failure as non-fatal and logs it; here the steps may
    /// fail individually (for example `worktree remove` refuses a worktree
    /// whose gitlink a worker rewrote), but the end state is checked and a
    /// leftover is an error.
    fn purge(&self, path: &Path) -> Result<(), WorkspaceError> {
        let entry = self.list()?.into_iter().find(|e| e.path == path);
        let mut remove_err = None;
        if let Some(entry) = &entry {
            if entry.locked.is_some() {
                self.git.run(
                    &self.repo,
                    ["worktree".as_ref(), "unlock".as_ref(), path.as_os_str()],
                )?;
            }
            if path.exists() {
                // Double --force also removes a locked worktree; unlock ran
                // first so a concurrent reader never sees a locked orphan.
                let args = [
                    "worktree".as_ref(),
                    "remove".as_ref(),
                    "--force".as_ref(),
                    "--force".as_ref(),
                    path.as_os_str(),
                ];
                remove_err = self.git.run(&self.repo, args).err();
            }
        }
        remove_dir_if_exists(path)?;
        self.git.run(&self.repo, ["worktree", "prune"])?;

        let still_registered = self.list()?.iter().any(|e| e.path == path);
        if still_registered || path.exists() {
            let mut detail = format!(
                "registered={still_registered}, directory_exists={}",
                path.exists()
            );
            if let Some(e) = remove_err {
                detail.push_str(&format!("; git worktree remove: {e}"));
            }
            return Err(WorkspaceError::RemovalIncomplete {
                path: path.to_path_buf(),
                detail,
            });
        }
        Ok(())
    }

    fn hold_path(&self, name: &str) -> PathBuf {
        self.base.join(HOLDS).join(name)
    }

    /// Sessions logged in `hold`'s session log that still have a live
    /// process (Unix; empty elsewhere). The caller holds the hold exclusively,
    /// so no child appends meanwhile; the log is rewritten to the sessions
    /// still live.
    ///
    /// A line is `<sid> <leader start> <boot> <pid ns> <time ns>` (`-` for an
    /// unreadable part; lines written before the namespace fields have only
    /// the first three and are read as this namespace's, as they always
    /// were).
    ///
    /// A logged session counts only when it is this boot's (a log from
    /// before a reboot names PIDs of another boot and is dropped), and when
    /// its leader, if running, is the logged process (a leader PID reused by
    /// an unrelated process with another start time is not ours, and while
    /// the old session still had members that PID could not have been reused
    /// as a session id). The start time is compared only within one time
    /// namespace (Linux reports it relative to the reader's). A session whose
    /// leader has exited still counts while any member runs: on macOS (and on
    /// Linux without namespaces) an orphan outlives the shell that started it.
    /// On Linux with namespaces the kernel kills the orphans when the command
    /// (PID 1 of its PID namespace) exits, so only the running command and its
    /// children count. One scan of the process table serves every logged
    /// session.
    ///
    /// A line from another PID namespace (or, on Linux, with the namespace
    /// unreadable on either side) names a PID this reader cannot see. It is
    /// counted in [`Sessions::foreign`] and kept as it is, never pruned: only
    /// a reader in that namespace can tell whether it still runs.
    fn live_sessions(hold: &Path) -> Result<Sessions, WorkspaceError> {
        let log = session_log(hold);
        let text = match std::fs::read_to_string(&log) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Sessions::default());
            }
            Err(e) => return Err(WorkspaceError::io(&log, e)),
        };
        let here = HostId::current();
        let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
        let part = |v: &str| (v != "-").then(|| v.to_owned());
        let mut logged: std::collections::HashMap<u32, Option<u64>> =
            std::collections::HashMap::new();
        let mut foreign: Vec<&str> = Vec::new();
        for line in text.lines() {
            let mut f = line.split_whitespace();
            let Some(sid) = f.next().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let start = f.next().and_then(|s| s.parse::<u64>().ok());
            let logged_boot = f.next();
            // Another boot (or none recorded, or unreadable now): its
            // processes are gone in every namespace.
            if logged_boot.is_none() || here.boot.as_deref() != logged_boot {
                continue;
            }
            let logged_ns = f.next().map(part);
            let logged_time_ns = f.next().map(part);
            if let Some(ns) = logged_ns {
                let unreadable = PID_NAMESPACES_EXIST && (ns.is_none() || here.ns.is_none());
                if unreadable || ns != here.ns {
                    foreign.push(line);
                    continue;
                }
            }
            let comparable = logged_time_ns.is_none_or(|t| t == here.time_ns);
            // The leader runs, but it is another process: a recycled id.
            if let Some(start) = start
                && comparable
                && pid_alive(sid)
                && process_start_time(sid).is_some_and(|now| now != start)
            {
                continue;
            }
            logged.insert(sid, start);
        }
        let mut live: Vec<u32> = if logged.is_empty() {
            Vec::new()
        } else {
            // The zombie check reads /proc only for members of a logged
            // session, so the scan stays one getsid per process.
            all_pids()
                .into_iter()
                .filter_map(|pid| Some((pid, session_of(pid)?)))
                .filter(|(pid, sid)| logged.contains_key(sid) && !is_zombie(*pid))
                .map(|(_, sid)| sid)
                .collect()
        };
        live.sort_unstable();
        live.dedup();
        // Keep what is still live, and every foreign line as it was;
        // everything else is pruned.
        let kept: String = live
            .iter()
            .map(|sid| {
                let start = logged
                    .get(sid)
                    .copied()
                    .flatten()
                    .map_or_else(|| "-".to_owned(), |s| s.to_string());
                format!(
                    "{sid} {start} {} {} {}\n",
                    dash(&here.boot),
                    dash(&here.ns),
                    dash(&here.time_ns)
                )
            })
            .chain(foreign.iter().map(|l| format!("{l}\n")))
            .collect();
        if kept != text {
            if kept.is_empty() {
                remove_file_if_exists(&log)?;
            } else {
                std::fs::write(&log, kept).map_err(|e| WorkspaceError::io(&log, e))?;
            }
        }
        Ok(Sessions {
            live,
            foreign: foreign.len(),
        })
    }

    /// Take `hold` exclusively, and check that no logged session of it has a
    /// live process: no process started in the worktree at `path` still runs.
    /// Returns the held lock (kept while the caller works on the worktree), or
    /// [`WorkspaceError::Busy`].
    ///
    /// A session logged in another PID namespace cannot be checked from here.
    /// With `foreign_blocks` it refuses (reopen, startup cleanup: nothing may
    /// continue or delete what might still run); without it (`clean`, on a
    /// person's word, the way out for stranded work) it proceeds.
    fn quiet(
        &self,
        path: &Path,
        hold: &Path,
        foreign_blocks: bool,
    ) -> Result<HeldLock, WorkspaceError> {
        let held = self.hold_exclusively(hold).map_err(|e| {
            e.unwrap_or_else(|| WorkspaceError::Busy {
                path: path.to_path_buf(),
                reason: "a process started in this worktree is still running (its hold is taken)"
                    .into(),
            })
        })?;
        let sessions = Self::live_sessions(hold)?;
        if !sessions.live.is_empty() {
            return Err(WorkspaceError::Busy {
                path: path.to_path_buf(),
                reason: format!(
                    "a process started in this worktree is still running (session {:?})",
                    sessions.live
                ),
            });
        }
        if foreign_blocks && sessions.foreign > 0 {
            return Err(WorkspaceError::Busy {
                path: path.to_path_buf(),
                reason: format!(
                    "{} session(s) of this worktree were started in another PID namespace, \
                     so nothing here can tell whether they still run; remove it with \
                     `kgraf clean` once they have stopped",
                    sessions.foreign
                ),
            });
        }
        Ok(held)
    }

    /// Take `hold` exclusively without waiting. `Err(None)`: another process
    /// holds it.
    /// Released by an explicit unlock (`HeldLock`), so a child forked while it
    /// is held does not keep it past the release.
    fn hold_exclusively(&self, hold: &Path) -> Result<HeldLock, Option<WorkspaceError>> {
        if let Some(dir) = hold.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Some(WorkspaceError::io(dir, e)))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(hold)
            .map_err(|e| Some(WorkspaceError::io(hold, e)))?;
        match file.try_lock() {
            Ok(()) => Ok(HeldLock::new(file)),
            Err(std::fs::TryLockError::WouldBlock) => Err(None),
            Err(std::fs::TryLockError::Error(e)) => Err(Some(WorkspaceError::io(hold, e))),
        }
    }

    fn marker_file(&self) -> Result<File, WorkspaceError> {
        let marker = self.base.join(MARKER);
        File::open(&marker).map_err(|e| WorkspaceError::io(&marker, e))
    }

    fn shared(&self) -> Result<SharedLock<'_>, WorkspaceError> {
        let guard = self
            .scan
            .enter(false)
            .map_err(|()| WorkspaceError::GuardPoisoned(self.base.clone()))?;
        let file = self.marker_file()?;
        file.lock_shared()
            .map_err(|e| WorkspaceError::io(self.base.join(MARKER), e))?;
        Ok(SharedLock {
            _file: HeldLock::new(file),
            _guard: guard,
        })
    }

    fn exclusive(&self) -> Result<ExclusiveLock<'_>, WorkspaceError> {
        let guard = self
            .scan
            .enter(true)
            .map_err(|()| WorkspaceError::GuardPoisoned(self.base.clone()))?;
        let file = self.marker_file()?;
        file.lock()
            .map_err(|e| WorkspaceError::io(self.base.join(MARKER), e))?;
        Ok(ExclusiveLock {
            _file: HeldLock::new(file),
            _guard: guard,
        })
    }

    fn guard(&self, path: &Path) -> Result<PathGuard<'_>, WorkspaceError> {
        let poisoned = || WorkspaceError::GuardPoisoned(path.to_path_buf());
        let mut set = self.busy.lock().map_err(|_| poisoned())?;
        while set.contains(path) {
            set = self.freed.wait(set).map_err(|_| poisoned())?;
        }
        set.insert(path.to_path_buf());
        Ok(PathGuard {
            mgr: self,
            path: path.to_path_buf(),
        })
    }
}

/// Rename a base's marker from its former name (decision 0062) when only the
/// former one exists. Its contents (the owning repository) are kept: the
/// header line is not checked, the `repo=` line is. Both present is refused:
/// which one is current cannot be told, and guessing could hand the base to
/// the wrong repository.
fn adopt_legacy_marker(base: &Path) -> Result<(), WorkspaceError> {
    let legacy = base.join(LEGACY_MARKER);
    let marker = base.join(MARKER);
    if !legacy.is_file() {
        return Ok(());
    }
    if marker.exists() {
        return Err(WorkspaceError::UnsafeBaseDir {
            path: base.to_path_buf(),
            reason: format!(
                "both {MARKER} and {LEGACY_MARKER} (the marker under kGraf's former name) \
                 exist; remove the one that does not name this repository"
            ),
        });
    }
    std::fs::rename(&legacy, &marker).map_err(|e| WorkspaceError::io(&legacy, e))
}

/// Whether the owner a worktree lock reason names may still be running:
/// `None` when the reason is not one of ours (neither [`LOCK_TAG`] nor
/// [`LEGACY_LOCK_TAG`]), `Some(true)` when it is alive or cannot be judged
/// from here ([`Liveness::Unknowable`] counts as alive: nothing may move its
/// work). For a caller that must not move a directory under a running process.
pub fn lock_owner_may_be_running(reason: &str) -> Option<bool> {
    let owner = parse_owner(reason)?;
    Some(!owner.is_self() && owner.liveness() != Liveness::Dead)
}

/// Write the marker for `repo`, or verify an existing one names `repo`.
/// `create_new` makes two managers opening a fresh base at once agree on one
/// owner: the loser reads the winner's stamp and is checked against it.
fn stamp_or_check_marker(marker: &Path, repo: &Path) -> Result<(), WorkspaceError> {
    let contents = format!(
        "{MARKER_HEADER}\nrepo={}\nContents of this directory are managed by kGraf.\n",
        repo.display()
    );
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
    {
        Ok(mut file) => {
            use std::io::Write;
            return file
                .write_all(contents.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|e| WorkspaceError::io(marker, e));
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(WorkspaceError::io(marker, e)),
    }
    let text = std::fs::read_to_string(marker).map_err(|e| WorkspaceError::io(marker, e))?;
    let stamped = text.lines().find_map(|l| l.strip_prefix("repo="));
    match stamped {
        Some(holder) if Path::new(holder) == repo => Ok(()),
        Some(holder) => Err(WorkspaceError::UnsafeBaseDir {
            path: marker.parent().unwrap_or(marker).to_path_buf(),
            reason: format!("base belongs to repository {holder}"),
        }),
        None => Err(WorkspaceError::UnsafeBaseDir {
            path: marker.parent().unwrap_or(marker).to_path_buf(),
            reason: "marker names no repository (written by an older version?)".into(),
        }),
    }
}

fn canonical(path: &Path) -> Result<PathBuf, WorkspaceError> {
    std::fs::canonicalize(path).map_err(|e| WorkspaceError::io(path, e))
}

fn validate_name(name: &str) -> Result<(), WorkspaceError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(WorkspaceError::InvalidName(name.to_string()))
    }
}

/// `kgraf:v1:pid=<pid>:start=<t>:machine=<m>:boot=<b>:ns=<n>:name=<name>`
/// for this process (`-` for a host field this platform cannot read).
fn lock_reason(name: &str) -> Result<String, WorkspaceError> {
    Ok(format!(
        "{LOCK_TAG}:pid={}:start={}:{}:name={name}",
        std::process::id(),
        own_start_time()?,
        HostId::current().fields(),
    ))
}

/// Where a PID was read: the machine, its boot, the PID namespace and the time
/// namespace. Each part is `None` when it could not be read.
///
/// **The rule** (one function, [`owner_alive`], for worktree locks and run
/// leases alike):
/// - no host recorded at all (a lock written before host ids): judged by PID
///   and start time, as it always was; those were written on this machine;
/// - another PID namespace, or the boot unreadable on either side, or (Linux,
///   where PID namespaces exist) the namespace unreadable on either side: the
///   PID means nothing here, so the owner counts as **alive** (unknowable);
/// - the same boot and namespace: judged by PID and start time. The boot id is
///   random per boot (Linux `boot_id`, macOS `kern.bootsessionuuid`), so an
///   equal boot id and namespace prove the same running kernel and the same
///   PID table, even when the machine id is unreadable on one or both sides
///   (a slim container has no `/etc/machine-id`). Two known machine ids that
///   differ contradict that proof: the owner counts as alive. The start time
///   is compared only when the time namespace is the same too: Linux reports
///   a start time relative to the reader's time namespace, so across two of
///   them a live owner would look like a reused PID;
/// - another boot, the same known machine and namespace: the machine
///   restarted since, so the owner is **dead**;
/// - another boot with the machine unknown on either side (or different):
///   nothing proves it is this machine, so the owner counts as alive. A slim
///   container's worktrees are thus stranded after a host reboot, listed by
///   `WorkspaceManager::stranded`, until removed with `kgraf clean`.
///
/// **Known limits** (both come from copying a machine, not from kGraf):
/// - a VM or disk image cloned without regenerating `/etc/machine-id` gives
///   two machines one machine id. Each boots with its own boot id, so a lock
///   one wrote looks, from the other, like this machine before a reboot: a
///   live owner on the clone is judged **dead**. Regenerate the machine id of
///   a clone (`systemd-machine-id-setup`).
/// - a VM restored from a memory snapshot resumes the snapshot's boot id, and
///   the init PID namespace has the same inode on every Linux machine. Two
///   restores of one snapshot running at once therefore look like one PID
///   table, and each judges the other's owners by PID. Do not run two
///   restores of one snapshot against one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostId {
    /// macOS: the hardware UUID (`gethostuuid`). Linux: `/etc/machine-id`,
    /// else `/var/lib/dbus/machine-id`, else unknown (a slim container often
    /// has neither). Needed only to tell a reboot of this machine from
    /// another machine; within one boot the boot id suffices.
    pub machine: Option<String>,
    /// macOS `kern.bootsessionuuid`; Linux `/proc/sys/kernel/random/boot_id`.
    pub boot: Option<String>,
    /// Linux: the inode of `/proc/self/ns/pid`. macOS has no PID namespaces:
    /// `None` on both sides compares equal there. On Linux an unreadable
    /// namespace is unknowable (see [`host_verdict`]).
    pub ns: Option<String>,
    /// Linux: the inode of `/proc/self/ns/time` (`None` on a kernel without
    /// time namespaces, and on macOS). Start times are compared only when it
    /// is equal on both sides.
    pub time_ns: Option<String>,
}

impl HostId {
    /// This process's.
    pub fn current() -> Self {
        host_probe()
    }

    /// `machine=<m>:boot=<b>:ns=<n>:tns=<t>`, `-` for an unreadable part.
    pub fn fields(&self) -> String {
        let f = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
        format!(
            "machine={}:boot={}:ns={}:tns={}",
            f(&self.machine),
            f(&self.boot),
            f(&self.ns),
            f(&self.time_ns)
        )
    }

    /// Whether `field` is one [`Self::fields`] writes (`machine=`, `boot=`,
    /// `ns=`, `tns=`), so a parser can find the host part of a record.
    pub fn is_field(field: &str) -> bool {
        ["machine=", "boot=", "ns=", "tns="]
            .iter()
            .any(|k| field.starts_with(k))
    }

    /// Parse the fields [`Self::fields`] writes, from any order among other
    /// `key=value` fields; `None` when none of them is present. A record
    /// written before `tns=` existed parses with `time_ns: None`.
    pub fn parse_fields<'a>(fields: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let mut out = HostId::default();
        let mut seen = false;
        let val = |v: &str| (v != "-" && !v.is_empty()).then(|| v.to_owned());
        for f in fields {
            if let Some(v) = f.strip_prefix("machine=") {
                out.machine = val(v);
                seen = true;
            } else if let Some(v) = f.strip_prefix("boot=") {
                out.boot = val(v);
                seen = true;
            } else if let Some(v) = f.strip_prefix("ns=") {
                out.ns = val(v);
                seen = true;
            } else if let Some(v) = f.strip_prefix("tns=") {
                out.time_ns = val(v);
                seen = true;
            }
        }
        seen.then_some(out)
    }
}

/// Whether this platform has PID namespaces. Where it does (Linux), a
/// namespace that cannot be read is unknowable, not equal.
const PID_NAMESPACES_EXIST: bool = cfg!(target_os = "linux");

/// Whether the owner recorded as `pid`, `start` and `host` may still run.
/// See [`HostId`] for the rule; errs toward alive.
fn owner_alive(pid: u32, start: Option<u64>, host: Option<&HostId>) -> bool {
    owner_liveness(pid, start, host) != Liveness::Dead
}

/// What can be said about a recorded owner from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// Running, here.
    Alive,
    /// Not running (its PID gone or reused, or the machine rebooted since).
    Dead,
    /// Recorded on another machine or in another PID namespace, with the boot
    /// (or on Linux the namespace) unreadable, or in another boot of a machine
    /// not known to be this one: nothing here can tell. Its work is
    /// **stranded** until a person removes it (`kgraf clean <worktree>`);
    /// a container restarted with a new PID namespace strands its worktrees
    /// this way.
    Unknowable,
}

fn owner_liveness(pid: u32, start: Option<u64>, host: Option<&HostId>) -> Liveness {
    let here = HostId::current();
    if let Some(recorded) = host {
        match host_verdict(recorded, &here, PID_NAMESPACES_EXIST) {
            HostVerdict::SamePidTable => {}
            HostVerdict::Rebooted => return Liveness::Dead,
            HostVerdict::Unknowable => return Liveness::Unknowable,
        }
    }
    // A zombie has exited; only its parent has not reaped it (see
    // `is_zombie`). `kill(pid, 0)` still succeeds for it.
    if !pid_alive(pid) || is_zombie(pid) {
        return Liveness::Dead;
    }
    // Start times from another time namespace are offset: not comparable.
    let comparable = host.is_none_or(|h| h.time_ns == here.time_ns);
    match (start, process_start_time(pid)) {
        (Some(recorded), Some(current)) if comparable && recorded != current => Liveness::Dead,
        _ => Liveness::Alive,
    }
}

/// How a PID recorded under one host relates to the PID table seen here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostVerdict {
    /// The same boot and PID namespace: the PID names a process here.
    SamePidTable,
    /// The same known machine and namespace, another boot: the owner is gone.
    Rebooted,
    /// Nothing here can tell.
    Unknowable,
}

/// The rule [`HostId`] states, as a pure function of both sides and of
/// whether the platform has PID namespaces, so every case is testable on any
/// host (the machine id is readable on some hosts and not on others).
fn host_verdict(recorded: &HostId, here: &HostId, pid_namespaces: bool) -> HostVerdict {
    // Where PID namespaces exist, an unreadable one proves nothing: two
    // containers share the host's boot id, and with the namespace unknown a
    // live owner in the other one would be judged by a PID it does not have.
    if pid_namespaces && (recorded.ns.is_none() || here.ns.is_none()) {
        return HostVerdict::Unknowable;
    }
    if recorded.ns != here.ns {
        return HostVerdict::Unknowable;
    }
    let (Some(recorded_boot), Some(here_boot)) = (&recorded.boot, &here.boot) else {
        return HostVerdict::Unknowable;
    };
    let same_machine = match (&recorded.machine, &here.machine) {
        (Some(a), Some(b)) => Some(a == b),
        _ => None,
    };
    match (recorded_boot == here_boot, same_machine) {
        // Equal boot ids name one running kernel. Two known machine ids that
        // differ contradict that, so err toward alive.
        (true, Some(false)) => HostVerdict::Unknowable,
        (true, _) => HostVerdict::SamePidTable,
        (false, Some(true)) => HostVerdict::Rebooted,
        // Another boot of an unknown (or another) machine: a reboot of this
        // machine and another machine look the same from here.
        (false, _) => HostVerdict::Unknowable,
    }
}

/// The process that holds a tagged lock.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Owner {
    pid: u32,
    /// Process start time recorded at lock time; `None` in reasons written
    /// without one. Distinguishes the owner from a later process that reused
    /// its PID.
    start: Option<u64>,
    /// Where the PID was read; `None` in reasons written without host fields.
    host: Option<HostId>,
}

impl Owner {
    /// This process: the same host (where recorded), PID and, where one was
    /// recorded, start time.
    fn is_self(&self) -> bool {
        self.host.as_ref().is_none_or(|h| *h == HostId::current())
            && self.pid == std::process::id()
            && match (self.start, process_start_time(self.pid)) {
                (Some(recorded), Some(current)) => recorded == current,
                _ => true,
            }
    }

    fn alive(&self) -> bool {
        owner_alive(self.pid, self.start, self.host.as_ref())
    }

    fn liveness(&self) -> Liveness {
        owner_liveness(self.pid, self.start, self.host.as_ref())
    }
}

/// A process named by PID, start time and host, so neither a later process
/// that reused the PID nor a PID read on another machine, boot or namespace
/// is mistaken for it. Recorded by an owner (a run lease, a worktree lock) and
/// checked by whoever finds the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Start time as this platform's probe reports it; `None` where there is no
    /// probe.
    pub start: Option<u64>,
    /// Where the PID was read; `None` for a record written without one.
    pub host: Option<HostId>,
}

impl ProcessIdentity {
    /// This process.
    pub fn current() -> Self {
        let pid = std::process::id();
        Self {
            pid,
            start: process_start_time(pid),
            host: Some(HostId::current()),
        }
    }

    /// Whether the process may still be running, by the same rule as a
    /// worktree lock ([`HostId`]).
    pub fn is_alive(&self) -> bool {
        owner_alive(self.pid, self.start, self.host.as_ref())
    }
}

/// A lock reason that starts with kGraf's tag ([`LOCK_TAG`] or
/// [`LEGACY_LOCK_TAG`]) but names no owner [`parse_owner`] can read: kGraf
/// wrote it, and it was cut short or damaged. A reason without the tag is
/// someone else's and is never judged or removed by this crate.
fn is_unreadable_own_lock(reason: &str) -> bool {
    (reason.starts_with(LOCK_TAG) || reason.starts_with(LEGACY_LOCK_TAG))
        && parse_owner(reason).is_none()
}

/// `kgraf:v1:pid=<pid>[:start=<t>][:machine=..:boot=..:ns=..]:name=...`
/// -> owner. Any other reason is not ours.
fn parse_owner(reason: &str) -> Option<Owner> {
    let rest = reason
        .strip_prefix(LOCK_TAG)
        .or_else(|| reason.strip_prefix(LEGACY_LOCK_TAG))?
        .strip_prefix(":pid=")?;
    let mut fields = rest.split(':');
    let pid = fields.next()?.parse().ok()?;
    let fields: Vec<&str> = fields.take_while(|f| !f.starts_with("name=")).collect();
    let start = fields
        .iter()
        .find_map(|f| f.strip_prefix("start="))
        .and_then(|s| s.parse().ok());
    let host = HostId::parse_fields(fields.iter().copied());
    Some(Owner { pid, start, host })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn clean_id(raw: &str) -> Option<String> {
    let id = raw.trim();
    (!id.is_empty() && !id.contains(':') && !id.contains(char::is_whitespace))
        .then(|| id.to_owned())
}

#[cfg(target_os = "linux")]
fn host_probe() -> HostId {
    let read = |p: &str| std::fs::read_to_string(p).ok().and_then(|t| clean_id(&t));
    // `pid:[4026531836]` -> `4026531836`.
    let inode = |link: &str| {
        std::fs::read_link(link).ok().and_then(|l| {
            let inode: String = l.to_str()?.chars().filter(char::is_ascii_digit).collect();
            (!inode.is_empty()).then_some(inode)
        })
    };
    HostId {
        machine: read("/etc/machine-id").or_else(|| read("/var/lib/dbus/machine-id")),
        boot: read("/proc/sys/kernel/random/boot_id"),
        ns: inode("/proc/self/ns/pid"),
        time_ns: inode("/proc/self/ns/time"),
    }
}

#[cfg(target_os = "macos")]
fn sysctl_string(name: &std::ffi::CStr) -> Option<String> {
    let mut buf = [0u8; 128];
    let mut len = buf.len();
    // SAFETY: the name is NUL-terminated; `buf` has `len` bytes and the kernel
    // writes at most `len` and stores the count written in `len`.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 || len > buf.len() {
        return None;
    }
    let text = std::ffi::CStr::from_bytes_until_nul(&buf[..len]).ok()?;
    clean_id(text.to_str().ok()?)
}

/// The hardware UUID (IOPlatformUUID) through `gethostuuid`: stable across
/// OS updates and distinct per Mac. (`kern.uuid` is the kernel binary's UUID:
/// it changes with every OS update and is shared by Macs on one build.)
#[cfg(target_os = "macos")]
fn hardware_uuid() -> Option<String> {
    let mut id = [0u8; 16];
    let wait = libc::timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    // SAFETY: `id` is the 16 bytes gethostuuid writes; `wait` is a valid
    // timespec for the duration of the call.
    if unsafe { libc::gethostuuid(id.as_mut_ptr(), &wait) } != 0 {
        return None;
    }
    if id.iter().all(|b| *b == 0) {
        return None;
    }
    let hex: String = id.iter().map(|b| format!("{b:02X}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

#[cfg(target_os = "macos")]
fn host_probe() -> HostId {
    HostId {
        machine: hardware_uuid(),
        boot: sysctl_string(c"kern.bootsessionuuid"),
        ns: None,
        time_ns: None,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn host_probe() -> HostId {
    HostId::default()
}

/// What a worktree's session log says, read from here.
#[derive(Debug, Default, PartialEq, Eq)]
struct Sessions {
    /// Logged sessions of this namespace with a running member.
    live: Vec<u32>,
    /// Lines from another PID namespace: not checkable here, kept.
    foreign: usize,
}

/// The session log beside a hold file: `<hold>+sessions` (`+` never appears in
/// a workspace name, so it cannot be another workspace's hold).
pub(crate) fn session_log(hold: &Path) -> PathBuf {
    let mut name = hold.as_os_str().to_owned();
    name.push("+sessions");
    PathBuf::from(name)
}

fn remove_file_if_exists(path: &Path) -> Result<(), WorkspaceError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(WorkspaceError::io(path, e)),
    }
}

/// Every process ID on the system.
#[cfg(target_os = "macos")]
fn all_pids() -> Vec<u32> {
    // SAFETY: a null buffer asks for the count only.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    // Room for processes started meanwhile.
    let mut pids = vec![0 as libc::pid_t; count as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: `pids` has `bytes` bytes; the call writes at most that many and
    // returns the number of PIDs written.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(n.max(0) as usize);
    pids.into_iter()
        .filter_map(|p| u32::try_from(p).ok())
        .collect()
}

/// Every process ID on the system (this PID namespace).
#[cfg(target_os = "linux")]
fn all_pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn all_pids() -> Vec<u32> {
    Vec::new()
}

/// The session a live process belongs to.
#[cfg(unix)]
fn session_of(pid: u32) -> Option<u32> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    // SAFETY: getsid only reads the process table.
    let sid = unsafe { libc::getsid(pid) };
    u32::try_from(sid).ok()
}

#[cfg(not(unix))]
fn session_of(_pid: u32) -> Option<u32> {
    None
}

/// Whether `pid` has exited and waits only to be reaped. A zombie holds no
/// descriptors and no memory, so it cannot write a worktree; it must not keep
/// a session live. Found on Linux: when the process holding a sandboxed
/// command is killed, the command (PID 1 of its PID namespace) is reparented
/// to the caller's init, and an init that never reaps (a container's `sleep`,
/// or kGraf itself as a container's PID 1) leaves it a zombie for good,
/// which blocked reopen forever. Linux `getsid` answers for a zombie.
///
/// `/proc/<pid>/stat` alone is not enough: it shows the thread-group
/// leader's state, and a leader that called `pthread_exit` while other
/// threads run shows `Z` though the process lives, holds its descriptors and
/// can write. So a process is a zombie only when its own state **and** the
/// state of every thread under `/proc/<pid>/task` is `Z` or `X`. Any read
/// that fails (the process just went, or `/proc` is not what is expected)
/// counts as not a zombie: errs toward live.
#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
    fn exited(stat_path: &Path) -> Option<bool> {
        let stat = std::fs::read_to_string(stat_path).ok()?;
        let state = stat
            .get(stat.rfind(')')? + 1..)?
            .split_whitespace()
            .next()?;
        Some(state == "Z" || state == "X")
    }
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    if exited(&proc_dir.join("stat")) != Some(true) {
        return false;
    }
    let Ok(tasks) = std::fs::read_dir(proc_dir.join("task")) else {
        return false;
    };
    let mut any = false;
    for task in tasks {
        let Ok(task) = task else {
            return false;
        };
        if exited(&task.path().join("stat")) != Some(true) {
            return false;
        }
        any = true;
    }
    any
}

/// macOS: `p_stat` is `SZOMB`. (`getsid` already fails with ESRCH for a
/// zombie, so [`session_of`] leaves it out, but `kill(pid, 0)` succeeds, so
/// an owner check needs this.) `proc_pidinfo` finds a zombie only when its
/// `arg` is 1 (`findzomb` in XNU `bsd/kern/proc_info.c`); with 0 it fails.
#[cfg(target_os = "macos")]
fn is_zombie(pid: u32) -> bool {
    let Ok(pid) = libc::c_int::try_from(pid) else {
        return false;
    };
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let Ok(size) = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()) else {
        return false;
    };
    // SAFETY: the buffer is a zeroed proc_bsdinfo of exactly `size` bytes;
    // the kernel writes at most `size` bytes and returns the count written.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            1,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return false;
    }
    // SAFETY: fully written by the successful call above.
    let info = unsafe { info.assume_init() };
    info.pbi_status == libc::SZOMB
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn is_zombie(_pid: u32) -> bool {
    false
}

fn own_start_time() -> Result<u64, WorkspaceError> {
    let pid = std::process::id();
    if cfg!(any(target_os = "macos", target_os = "linux")) {
        process_start_time(pid).ok_or_else(|| {
            WorkspaceError::io(
                format!("/proc/{pid}"),
                std::io::Error::other("cannot read this process's start time"),
            )
        })
    } else {
        // No probe on this platform; 0 means "not recorded" to `alive`.
        Ok(0)
    }
}

/// This process's start time, read between fork and exec: only system calls
/// and stack buffers, no allocation (the child of a fork may not allocate).
/// The same value [`process_start_time`] reads.
#[cfg(target_os = "macos")]
pub(crate) fn start_time_after_fork(pid: u32) -> Option<u64> {
    // proc_pidinfo into a stack structure: already allocation-free.
    process_start_time(pid)
}

/// This process's start time, read between fork and exec (see the macOS
/// twin): `/proc/self/stat` into a stack buffer, field 22 parsed in place.
#[cfg(target_os = "linux")]
pub(crate) fn start_time_after_fork(_pid: u32) -> Option<u64> {
    let mut buf = [0u8; 1024];
    // SAFETY: a NUL-terminated literal path; `buf` has `buf.len()` bytes.
    let n = unsafe {
        let fd = libc::open(
            c"/proc/self/stat".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        );
        if fd < 0 {
            return None;
        }
        let n = libc::read(fd, buf.as_mut_ptr().cast(), buf.len());
        libc::close(fd);
        n
    };
    let text = &buf[..usize::try_from(n).ok()?];
    let close = text.iter().rposition(|b| *b == b')')?;
    // Fields after the command name start at field 3; field 22 is index 19.
    let field = text[close + 1..]
        .split(|b| *b == b' ')
        .filter(|f| !f.is_empty())
        .nth(19)?;
    let mut v: u64 = 0;
    for b in field {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
    }
    Some(v)
}

// Only the unix child path calls this (sandbox/child.rs), so Windows has no copy.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub(crate) fn start_time_after_fork(_pid: u32) -> Option<u64> {
    None
}

/// Process start time in microseconds since the epoch.
#[cfg(target_os = "macos")]
fn process_start_time(pid: u32) -> Option<u64> {
    let pid = libc::c_int::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: the buffer is a zeroed proc_bsdinfo of exactly `size` bytes;
    // the kernel writes at most `size` bytes and returns the count written.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: fully written by the successful call above.
    let info = unsafe { info.assume_init() };
    Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

/// Process start time in clock ticks since boot (`/proc/<pid>/stat` field 22).
#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) may contain spaces and ')'; fields after the
    // last ')' start at field 3, so field 22 is index 19.
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_start_time(_pid: u32) -> Option<u64> {
    None
}

/// Whether `pid` names a running process. `EPERM` means it exists but belongs
/// to another user, which still counts as alive.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill with signal 0 performs only the existence and permission
    // check; it sends nothing.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// No liveness probe off Unix yet: every owner counts as alive, so cleanup
/// removes only entries whose directory is gone. Conservative, never destructive.
#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

fn read_subdirs(dir: &Path) -> Result<Vec<PathBuf>, WorkspaceError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(WorkspaceError::io(dir, e)),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| WorkspaceError::io(dir, e))?;
        let file_type = entry
            .file_type()
            .map_err(|e| WorkspaceError::io(entry.path(), e))?;
        if file_type.is_dir() {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

fn remove_dir_if_exists(path: &Path) -> Result<(), WorkspaceError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(WorkspaceError::io(path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_parses_only_our_reason() {
        assert_eq!(
            parse_owner("kgraf:v1:pid=42:start=7:name=w"),
            Some(Owner {
                pid: 42,
                start: Some(7),
                host: None,
            })
        );
        assert_eq!(
            parse_owner("kgraf:v1:pid=42:name=w"),
            Some(Owner {
                pid: 42,
                start: None,
                host: None,
            })
        );
        assert_eq!(parse_owner("kgraf:v2:pid=42"), None);
        assert_eq!(parse_owner("user lock"), None);
        assert_eq!(parse_owner(""), None);
        // Decision 0062: a lock written under the former name is still ours.
        assert_eq!(
            parse_owner("haustoria:v1:pid=42:start=7:name=w"),
            Some(Owner {
                pid: 42,
                start: Some(7),
                host: None,
            })
        );
        assert_eq!(parse_owner("haustoria:v2:pid=42"), None);
    }

    /// Decision 0062: a base moved from `.haustoria` keeps its marker under the
    /// former name; opening it adopts that marker, keeping the owner it names.
    #[test]
    fn a_base_with_the_former_marker_is_adopted_and_both_are_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let base = dir.path();
        std::fs::write(
            base.join(LEGACY_MARKER),
            "haustoria-workspaces v1\nrepo=/r\n",
        )
        .expect("write");
        adopt_legacy_marker(base).expect("adopted");
        assert!(!base.join(LEGACY_MARKER).exists());
        let text = std::fs::read_to_string(base.join(MARKER)).expect("marker");
        assert!(text.contains("repo=/r"), "{text}");
        // Nothing to do the second time.
        adopt_legacy_marker(base).expect("idempotent");

        std::fs::write(base.join(LEGACY_MARKER), "x").expect("write");
        let refused = adopt_legacy_marker(base).expect_err("both");
        assert!(refused.to_string().contains(LEGACY_MARKER), "{refused}");
    }

    #[test]
    fn names_reject_path_tricks() {
        for bad in [
            "",
            ".scratch",
            "../x",
            "a/b",
            "a b",
            ".hidden",
            &"x".repeat(65),
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?} accepted");
        }
        for good in ["w1", "worker-2", "a_b.c"] {
            assert!(validate_name(good).is_ok(), "{good:?} rejected");
        }
    }

    /// Spin until `n` tickets have been handed out (a waiter is queued).
    fn wait_for_tickets(gate: &ScanGate, n: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while gate.state().next < n {
            assert!(std::time::Instant::now() < deadline, "waiter never queued");
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_released_exclusive_holder_cannot_overtake_a_queued_reader() {
        // The starvation found under load: a cleanup loop releasing and
        // retaking the exclusive side ahead of a waiting `create`.
        let gate = ScanGate::new();
        let order = Mutex::new(Vec::new());
        let held = gate.enter(true).unwrap();
        std::thread::scope(|s| {
            s.spawn(|| {
                let _shared = gate.enter(false).unwrap();
                order.lock().unwrap().push("reader");
            });
            wait_for_tickets(&gate, 2);
            drop(held);
            let _again = gate.enter(true).unwrap();
            order.lock().unwrap().push("writer");
        });
        assert_eq!(*order.lock().unwrap(), ["reader", "writer"]);
    }

    #[test]
    fn readers_share_and_a_writer_waits_for_them() {
        let gate = ScanGate::new();
        let first = gate.enter(false).unwrap();
        // A second reader enters while the first holds: shared.
        let second = gate.enter(false).unwrap();
        let entered = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            let writer = s.spawn(|| {
                let _w = gate.enter(true).unwrap();
                entered.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            wait_for_tickets(&gate, 3);
            drop(first);
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(
                !entered.load(std::sync::atomic::Ordering::SeqCst),
                "writer entered while a reader held the gate"
            );
            drop(second);
            writer.join().unwrap();
        });
        assert!(entered.load(std::sync::atomic::Ordering::SeqCst));
    }

    fn host(machine: Option<&str>, boot: Option<&str>, ns: Option<&str>) -> HostId {
        HostId {
            machine: machine.map(Into::into),
            boot: boot.map(Into::into),
            ns: ns.map(Into::into),
            time_ns: None,
        }
    }

    /// Every case of the rule, on any host: the machine id is readable on
    /// some hosts and not on others (a slim Linux container has none).
    #[test]
    fn the_host_rule_covers_every_case() {
        use HostVerdict::{Rebooted, SamePidTable, Unknowable};
        let m = Some("m");
        let (b, b2) = (Some("b"), Some("b2"));
        let (n, n2) = (Some("4026531836"), Some("4026532000"));
        let cases = [
            // Known machine, same boot and namespace.
            (host(m, b, n), host(m, b, n), SamePidTable),
            // Machine unknown on either or both sides; the equal boot id and
            // namespace prove the same PID table.
            (host(None, b, n), host(None, b, n), SamePidTable),
            (host(m, b, n), host(None, b, n), SamePidTable),
            (host(None, b, n), host(m, b, n), SamePidTable),
            // macOS: no namespace on either side.
            (host(m, b, None), host(m, b, None), SamePidTable),
            // Two known machine ids that differ: contradictory, err alive.
            (host(Some("other"), b, n), host(m, b, n), Unknowable),
            // Another namespace, whatever else matches.
            (host(m, b, n2), host(m, b, n), Unknowable),
            (host(None, b, n2), host(None, b, n), Unknowable),
            (host(m, b, None), host(m, b, n), Unknowable),
            // The boot unreadable on either side.
            (host(m, None, n), host(m, b, n), Unknowable),
            (host(m, b, n), host(m, None, n), Unknowable),
            (host(None, None, n), host(None, None, n), Unknowable),
            // Another boot of the same known machine: a reboot.
            (host(m, b2, n), host(m, b, n), Rebooted),
            // Another boot, the machine unknown or different: cannot tell a
            // reboot here from another machine.
            (host(None, b2, n), host(None, b, n), Unknowable),
            (host(m, b2, n), host(None, b, n), Unknowable),
            (host(None, b2, n), host(m, b, n), Unknowable),
            (host(Some("other"), b2, n), host(m, b, n), Unknowable),
            // Nothing recorded readable.
            (HostId::default(), host(m, b, n), Unknowable),
        ];
        for (recorded, here, want) in cases {
            // No PID namespaces on this platform (macOS): as listed.
            assert_eq!(
                host_verdict(&recorded, &here, false),
                want,
                "{recorded:?} seen from {here:?}"
            );
            // Linux: a namespace unreadable on either side is unknowable,
            // whatever else matches; otherwise as listed.
            let linux_want = if recorded.ns.is_none() || here.ns.is_none() {
                Unknowable
            } else {
                want
            };
            assert_eq!(
                host_verdict(&recorded, &here, true),
                linux_want,
                "Linux: {recorded:?} seen from {here:?}"
            );
        }
        // Two Linux containers share the host's boot id.
        // With both namespace links unreadable, `None == None` must not make
        // them one PID table: a live owner in the other one would be judged
        // by a PID it does not have, and found dead.
        let container = host(None, b, None);
        assert_eq!(
            host_verdict(&container, &container, true),
            Unknowable,
            "unreadable namespaces on Linux"
        );
        assert_eq!(
            host_verdict(&host(m, b, None), &host(m, b, n), true),
            Unknowable
        );
        assert_eq!(
            host_verdict(&host(m, b, n), &host(m, b, None), true),
            Unknowable
        );
        // The time namespace is not part of the verdict (it only gates the
        // start-time comparison; see `start_times_compare_only_within_one_time_namespace`).
        let mut other_time = host(m, b, n);
        other_time.time_ns = Some("4026532999".into());
        assert_eq!(
            host_verdict(&other_time, &host(m, b, n), true),
            SamePidTable
        );
    }

    /// Linux reports a start time relative to the reader's
    /// time namespace, so a start time recorded in another one is not
    /// comparable. The owner is then judged by PID alone (errs toward alive);
    /// within one time namespace a different start time is still a reused PID.
    #[cfg(unix)]
    #[test]
    fn start_times_compare_only_within_one_time_namespace() {
        let me = std::process::id();
        let start = own_start_time().unwrap();
        let here = HostId::current();
        assert_eq!(
            owner_liveness(me, Some(start + 1), Some(&here)),
            Liveness::Dead,
            "same time namespace, another start time: a reused PID"
        );
        let other_time = HostId {
            time_ns: Some(format!("{}9", here.time_ns.clone().unwrap_or_default())),
            ..here.clone()
        };
        assert_eq!(
            owner_liveness(me, Some(start + 1), Some(&other_time)),
            Liveness::Alive,
            "another time namespace: start times are not comparable"
        );
        assert_eq!(
            owner_liveness(me, Some(start), Some(&here)),
            Liveness::Alive
        );
    }

    /// Each session line names the PID namespace it was
    /// written in. A line from another namespace names a PID this reader
    /// cannot see: it is reported as foreign and kept, never pruned. A dead
    /// line of this namespace is still pruned, and so is a line from another
    /// boot. Lines in the three-field format written before the namespace
    /// fields are read as this namespace's.
    #[test]
    fn session_lines_from_another_pid_namespace_are_kept_not_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let hold = dir.path().join("hold");
        let here = HostId::current();
        let Some(boot) = here.boot.clone() else {
            // No boot id (not macOS or Linux): every line is another boot's.
            return;
        };
        let dead = {
            let mut c = std::process::Command::new("true").spawn().unwrap();
            let pid = c.id();
            c.wait().unwrap();
            pid
        };
        let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
        let foreign = format!("{dead} - {boot} 1 -");
        let ours = format!("{dead} - {boot} {} {}", dash(&here.ns), dash(&here.time_ns));
        let legacy = format!("{dead} - {boot}");
        let other_boot = format!("{dead} - previous-boot 1 -");
        std::fs::write(
            session_log(&hold),
            format!("{ours}\n{foreign}\n{legacy}\n{other_boot}\n"),
        )
        .unwrap();
        let sessions = WorkspaceManager::live_sessions(&hold).unwrap();
        assert_eq!(
            sessions,
            Sessions {
                live: vec![],
                foreign: 1
            }
        );
        assert_eq!(
            std::fs::read_to_string(session_log(&hold)).unwrap(),
            format!("{foreign}\n"),
            "only the foreign line is kept"
        );
        // An unreadable namespace in the line: on Linux, where namespaces
        // exist, that is foreign too; on macOS `-` is this (absent) namespace.
        let unreadable = format!("{dead} - {boot} - -");
        std::fs::write(session_log(&hold), format!("{unreadable}\n")).unwrap();
        let sessions = WorkspaceManager::live_sessions(&hold).unwrap();
        assert_eq!(sessions.foreign, usize::from(PID_NAMESPACES_EXIST));
    }

    /// A process that has exited but is not reaped (what a sandboxed command
    /// becomes under an init that never reaps) is dead as an owner and keeps
    /// no session live, although `kill(pid, 0)` still answers for it.
    #[cfg(unix)]
    #[test]
    fn a_zombie_is_dead_and_keeps_no_session_live() {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        // SAFETY: setsid is async-signal-safe and touches no parent state.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let pid = child.id();
        let dir = tempfile::tempdir().unwrap();
        let hold = dir.path().join("hold");
        let boot = HostId::current().boot.unwrap();
        let log = || std::fs::write(session_log(&hold), format!("{pid} - {boot}\n")).unwrap();
        let here = HostId::current();

        log();
        assert!(!is_zombie(pid));
        assert_eq!(owner_liveness(pid, None, Some(&here)), Liveness::Alive);
        assert_eq!(
            WorkspaceManager::live_sessions(&hold).unwrap().live,
            vec![pid]
        );

        child.kill().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !is_zombie(pid) {
            assert!(std::time::Instant::now() < deadline, "never a zombie");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(pid_alive(pid), "kill(pid, 0) still answers for a zombie");
        assert_eq!(owner_liveness(pid, None, Some(&here)), Liveness::Dead);
        log();
        assert_eq!(
            WorkspaceManager::live_sessions(&hold).unwrap().live,
            Vec::<u32>::new()
        );
        child.wait().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn own_process_is_alive_and_a_reused_pid_is_not() {
        let me = std::process::id();
        let start = own_start_time().unwrap();
        assert!(
            Owner {
                pid: me,
                start: Some(start),
                host: Some(HostId::current()),
            }
            .alive()
        );
        // Same PID, different start time: a PID-reuse impostor.
        assert!(
            !Owner {
                pid: me,
                start: Some(start + 1),
                host: Some(HostId::current()),
            }
            .alive()
        );
    }
}
