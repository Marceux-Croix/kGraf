//! Moving a project's state from `.haustoria` to `.kgraf` (decision 0062).
//!
//! Haustoria was renamed kGraf on 2026-09-27. A project used under the former
//! name has its state (ledger, graph, schedules, run records, allow rules,
//! transcripts, worktrees) in [`LEGACY_STATE_DIR`]. On first use under kGraf,
//! [`migrate_state_dir`] moves it to [`STATE_DIR`] and repairs what points
//! into the old path. Every entry point reaches it: [`crate::state_dir`] calls
//! it first, and the desktop shell calls it for its `--project`.
//!
//! # States
//!
//! ```text
//!   neither dir, no lock   -> nothing to do (the common case: two stat calls)
//!   anything else          -> take the lock, then decide under it:
//!     both dirs            -> refused, naming both (which one is current is
//!                             not ours to guess)
//!     only .haustoria      -> refuse links, find git, check no older process
//!                             still works there, mark, rename, repair
//!     .kgraf + pending     -> an earlier move stopped before its repairs
//!                             finished: repair again (idempotent)
//!     otherwise            -> another process finished it: nothing to do
//!   then                   -> clear the mark (unless a repair failed after
//!                             the rename) and remove the lock
//! ```
//!
//! The lock and the pending mark are one file, [`MIGRATION_LOCK`], next to the
//! state directory (not inside it: Windows refuses to rename a directory while
//! a file in it is open). It exists only during a move, or after a move that
//! was killed. The mark is cleared before the file is removed, and a process
//! that locked a file since removed opens the path again, so exactly one of
//! many concurrent first uses moves and the rest find nothing to do.
//!
//! # Refusals, all before the rename
//!
//! - **Links.** A repository can ship a `.haustoria/` or `.kgraf.migrating`
//!   holding symbolic links. The lock, the JSON files and temporary files the
//!   move writes, the graph database and the hold files are opened without
//!   following a link (`O_NOFOLLOW`, `FILE_FLAG_OPEN_REPARSE_POINT`), and a
//!   link at any of them refuses the move.
//! - **git.** The repairs need git. git missing while the old directory holds
//!   worktrees, or git failing for any reason other than "not a git
//!   repository", refuses the move: moved without its repairs, each worktree
//!   would be unregistered at its new path, and startup cleanup would delete
//!   it with its uncommitted work.
//! - **A live older process**: a worktree whose git lock names a running
//!   owner, or a held hold file.
//!
//! # What is repaired
//!
//! - **Git worktrees** registered under the old path: `git worktree repair`
//!   for each one that moved, a direct rewrite of the admin `gitdir` file for
//!   one whose directory was already gone (so startup cleanup finds it as it
//!   would have), then a check that every worktree's `.git` file and every
//!   `$GIT_COMMON_DIR/worktrees/*/gitdir` agree and none names the old path.
//!   A move that cannot be verified fails.
//! - **Attempt refs** `refs/haustoria/attempts/*` move to `refs/kgraf/...`.
//!   Integration branches (`refs/heads/haustoria/<run>`) are the user's and
//!   may have been pushed: they are not renamed; a resumed run keeps using its
//!   branch (`workspaces::ensure_integration_branch`).
//! - **Absolute paths** into the old directory, in the graph store's `meta`
//!   rows (run records, schedules) and in the JSON files directly in the state
//!   directory (`run.json`, `worker.json`, `allow-rules.json`). The text is
//!   rewritten in place, so a hand-edited file keeps its layout. A path is
//!   rewritten only where it is whole (not inside a longer path such as a
//!   backup copy's). Whatever still names `.haustoria` afterwards (another
//!   spelling of the project path, a relative path) is listed in the notice.
//!
//! # What is not rewritten
//!
//! History. The ledger moves as a file with its `.tip` anchor beside it: no
//! row is touched, so its hash chain verifies after the move exactly as
//! before. Run transcripts and traces under `runs/` are the record of what a
//! model was sent; a path in them names where the work was then. The worktree
//! marker's `repo=` line names the repository, which does not move; the
//! marker file itself is renamed by `kgraf-sandbox` when it opens the base.
//! Locks written under the former tag are read by `kgraf-sandbox` as ours.
//! `mcp-home/` holds per-worker scratch homes, emptied at every start.
//!
//! Known limit: an older process that holds no worktree (a `haustoria index`,
//! say) is not detected. On Unix it keeps writing through its open files; on
//! Windows the rename fails, loudly, while it runs.

use crate::error::{Error, Result, ResultExt};
use kgraf_core::legacy::{LEGACY_STATE_DIR, STATE_DIR};
use kgraf_sandbox::WorkspaceError;
use kgraf_sandbox::git::Git;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

/// The lock and pending mark of a move, next to the state directory.
pub const MIGRATION_LOCK: &str = kgraf_core::paths::MIGRATION_LOCK_NAME;

/// Written into [`MIGRATION_LOCK`] once the move is decided; its presence
/// under the lock after a crash means the repairs must run again.
pub const MIGRATION_PENDING: &str = "kgraf state directory migration in progress (decision 0062)\n";

/// The attempt-ref namespace under the former name, and the current one
/// (`kgraf_sandbox::ATTEMPT_REF_PREFIX`).
const LEGACY_REF_PREFIX: &str = "refs/haustoria/";
const REF_PREFIX: &str = "refs/kgraf/";

/// What one move did, for the notice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// The directory moved from, and to.
    pub from: PathBuf,
    pub to: PathBuf,
    /// This process finished an earlier, interrupted move.
    pub resumed: bool,
    /// Worktrees whose registration now names the new path.
    pub worktrees_repaired: Vec<PathBuf>,
    /// Registrations of worktrees already gone, rewritten for startup cleanup.
    pub registrations_rewritten: usize,
    /// Attempt refs moved to `refs/kgraf/`.
    pub refs_moved: usize,
    /// Graph-store `meta` rows (run records, schedules) whose paths changed.
    pub records_rewritten: usize,
    /// Files in the state directory whose paths changed.
    pub files_rewritten: Vec<PathBuf>,
    /// Why the graph store's records were not rewritten, when they were not.
    pub records_skipped: Option<String>,
    /// What still names the former directory after the rewrite (a record
    /// with another spelling of the root, a relative path): each graph-store
    /// key or file, said in the notice, never silently left.
    pub still_naming_old: Vec<String>,
}

impl MigrationReport {
    /// The one notice a move prints.
    pub fn notice(&self) -> String {
        let mut text = format!(
            "kGraf: {} this project's state from {} to {} (Haustoria is kGraf's former \
             name, decision 0062).",
            if self.resumed {
                "finished moving"
            } else {
                "moved"
            },
            self.from.display(),
            self.to.display()
        );
        text.push_str(&format!(
            " Repaired {} git worktree registration(s), rewrote {} for worktrees already \
             gone, moved {} attempt ref(s), updated paths in {} run or schedule record(s)",
            self.worktrees_repaired.len(),
            self.registrations_rewritten,
            self.refs_moved,
            self.records_rewritten,
        ));
        if self.files_rewritten.is_empty() {
            text.push('.');
        } else {
            let names: Vec<String> = self
                .files_rewritten
                .iter()
                .map(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            text.push_str(&format!(" and in {}.", names.join(", ")));
        }
        if let Some(why) = &self.records_skipped {
            text.push_str(&format!(" Run records were not updated: {why}."));
        }
        if !self.still_naming_old.is_empty() {
            text.push_str(&format!(
                " Still naming {LEGACY_STATE_DIR} after the rewrite (another spelling of the \
                 project path, or a relative path; check them by hand): {}.",
                self.still_naming_old.join(", ")
            ));
        }
        text.push_str(" The ledger, transcripts and traces are history and were moved unchanged.");
        text
    }
}

/// Finds git for a move. Production passes `Git::discover`; tests pass a git
/// that is missing or fails in a chosen way.
pub(crate) type Discover<'a> = &'a dyn Fn() -> std::result::Result<Git, WorkspaceError>;

/// Move `root`'s state directory from its former name, if it has one, and
/// repair what points into it. `Ok(None)` when there was nothing to do.
/// Fails loud on every error, including a refusal when both
/// directories exist; nothing is left half-moved without a pending mark that
/// makes the next call finish it.
pub fn migrate_state_dir(root: &Path) -> Result<Option<MigrationReport>> {
    migrate_state_dir_with(root, &Git::discover)
}

/// [`migrate_state_dir`] with the way git is found passed in.
pub(crate) fn migrate_state_dir_with(
    root: &Path,
    discover: Discover<'_>,
) -> Result<Option<MigrationReport>> {
    let old = root.join(LEGACY_STATE_DIR);
    let new = root.join(STATE_DIR);
    let lock_path = root.join(MIGRATION_LOCK);
    // Fast path: no old directory and no interrupted move. Every other
    // decision, the "both exist" refusal included, is taken under the lock:
    // decided outside it, a check could see the old directory before another
    // process renamed it and the new one after.
    if !exists(&old) && !exists(&lock_path) {
        return Ok(None);
    }

    let (mut lock, pending) = acquire_lock(&lock_path)?;
    let outcome = if exists(&old) && exists(&new) {
        Err(conflict(&old, &new))
    } else if exists(&old) {
        move_and_repair(root, &old, &new, &mut lock, &lock_path, discover)
    } else if pending && exists(&new) {
        git_context(root, &new, discover)
            .and_then(|git| repair(root, &old, &new, git.as_ref()))
            .map(|mut report| {
                report.resumed = true;
                Some(report)
            })
    } else {
        // Nothing left to do: another process finished it, or this lock file
        // was just created by this call.
        Ok(None)
    };
    // The mark stays only when a move happened and a repair then failed: the
    // next call finishes it. Every other outcome (success, a refusal before
    // the rename, a failed rename) leaves no file behind.
    let mut now = String::new();
    let marked = lock
        .rewind()
        .and_then(|()| lock.read_to_string(&mut now))
        .map(|_| now == MIGRATION_PENDING)
        .unwrap_or(true);
    let keep = outcome.is_err() && marked && exists(&new);
    if !keep {
        // Cleared before it is removed, still under the lock: a process that
        // opened the file before the removal and gets the lock after it reads
        // no mark, so it never reports a finished move as one to resume
        //. It also sees the path gone and opens it again. An
        // empty file (no move happened) needs no clearing.
        if !now.is_empty() {
            lock.set_len(0)
                .and_then(|()| lock.sync_all())
                .with_step(|| format!("cannot clear {}", lock_path.display()))?;
        }
        remove_lock(&lock_path);
    }
    // Unlocked explicitly, as `kgraf_core::flock::HeldLock` does: a git child
    // forked while the lock was held may still hold a copy of the descriptor.
    let _ = lock.unlock();
    drop(lock);
    outcome
}

fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

fn is_link(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

/// `FILE_FLAG_OPEN_REPARSE_POINT`: open a symbolic link or junction itself,
/// never its target (Windows' `O_NOFOLLOW`).
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// Options that never follow a symbolic link at the final component:
/// `O_NOFOLLOW` on Unix, `FILE_FLAG_OPEN_REPARSE_POINT` on Windows. A
/// repository can ship a `.haustoria/` (or a `.kgraf.migrating`) holding
/// links; a write through one would land outside the project.
fn no_follow(opts: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    opts
}

fn refuse_link(path: &Path) -> Error {
    Error::Invalid(format!(
        "{} is a symbolic link; kGraf never writes through one while moving the state \
         directory (decision 0062). Remove the link and run again.",
        path.display()
    ))
}

/// Open the lock file without following a link, and refuse anything but a
/// regular file.
fn open_lock(lock_path: &Path) -> Result<File> {
    if let Ok(meta) = lock_path.symlink_metadata()
        && (is_link(&meta) || !meta.is_file())
    {
        return Err(refuse_link(lock_path));
    }
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    no_follow(&mut opts);
    // Windows: std's default share mode includes FILE_SHARE_DELETE, so the
    // holder can remove the file while waiters hold it open (they then see,
    // by file identity, that the path names another file or none; see
    // `still_at`). Without it the holder's own handle blocked the delete and
    // the lock file was never removed.
    let file = open_retrying_delete_pending(&opts, lock_path).map_err(|e| {
        // ELOOP: the path became a link between the check and the open.
        if lock_path.symlink_metadata().is_ok_and(|m| is_link(&m)) {
            refuse_link(lock_path)
        } else {
            Error::wrap(e).within(format!(
                "cannot open the migration lock {}",
                lock_path.display()
            ))
        }
    })?;
    let meta = file
        .metadata()
        .with_step(|| format!("cannot read {}", lock_path.display()))?;
    if is_link(&meta) || !meta.is_file() {
        return Err(refuse_link(lock_path));
    }
    Ok(file)
}

/// Whether the open, locked `file` is still the one at `path`. The holder
/// before us removes the file when it is done; a lock taken on the removed
/// file excludes nobody who opens the path afresh.
#[cfg(unix)]
fn still_at(file: &File, path: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let held = file
        .metadata()
        .with_step(|| format!("cannot read {}", path.display()))?;
    match path.symlink_metadata() {
        Ok(now) => Ok(now.dev() == held.dev() && now.ino() == held.ino()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_step(|| format!("cannot read {}", path.display())),
    }
}

/// `opts.open(path)`. On Windows, a file whose delete is pending (the holder
/// removed it, and a waiter still has it open) cannot be opened, not even to
/// create a new one, until the last handle closes: `ERROR_ACCESS_DENIED`. That
/// is retried for up to two seconds; anything else fails at once.
fn open_retrying_delete_pending(opts: &OpenOptions, path: &Path) -> std::io::Result<File> {
    #[cfg(windows)]
    {
        const ERROR_ACCESS_DENIED: i32 = 5;
        let mut tries = 0;
        loop {
            match opts.open(path) {
                Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) && tries < 200 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                other => return other,
            }
        }
    }
    #[cfg(not(windows))]
    opts.open(path)
}

/// A file's identity on Windows: volume serial number and file index, from
/// `GetFileInformationByHandle` (std's `MetadataExt::file_index` is unstable).
#[cfg(windows)]
fn windows_file_id(file: &File) -> std::io::Result<(u32, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value (plain
    // integers and FILETIMEs); it is only an out parameter below.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is owned by `file`, open for the whole call, and
    // `info` is a valid, writable out pointer of the documented type.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

/// Windows: the held file and the file now at `path` compared by volume and
/// file index, as `dev` and `ino` are on Unix. The path is opened without
/// following a link, for attributes only, sharing everything; a path whose
/// delete is pending (access denied) or that is gone names another file.
#[cfg(windows)]
fn still_at(file: &File, path: &Path) -> Result<bool> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const ERROR_ACCESS_DENIED: i32 = 5;
    let held = windows_file_id(file).with_step(|| format!("cannot read {}", path.display()))?;
    let mut opts = OpenOptions::new();
    opts.access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(0x1 | 0x2 | 0x4)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    match opts.open(path) {
        Ok(now) => {
            let now =
                windows_file_id(&now).with_step(|| format!("cannot read {}", path.display()))?;
            Ok(now == held)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => Ok(false),
        Err(e) => Err(e).with_step(|| format!("cannot read {}", path.display())),
    }
}

/// Neither Unix nor Windows: no identity to compare, so a removed lock file
/// could be taken again by two processes. kGraf ships on neither such target.
#[cfg(not(any(unix, windows)))]
fn still_at(_file: &File, _path: &Path) -> Result<bool> {
    Ok(true)
}

/// The longest a process waits for the migration lock. The holder moves one
/// state directory (a rename), repairs its worktrees with git commands
/// bounded at 10 minutes each (`git worktree repair`, the attempt refs) and
/// rewrites a few JSON files. A project has a handful of worktrees, each
/// repaired in milliseconds; 30 minutes covers a holder whose git commands
/// are slow, so a wait this long means a holder that is stuck.
const MIGRATION_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Take the migration lock, and read whether an interrupted move left its
/// mark. Retries while the file this call locked is not the one at the path.
fn acquire_lock(lock_path: &Path) -> Result<(File, bool)> {
    acquire_lock_within(lock_path, MIGRATION_LOCK_WAIT)
}

/// [`acquire_lock`], waiting at most `bound` for each take of the lock.
fn acquire_lock_within(lock_path: &Path, bound: std::time::Duration) -> Result<(File, bool)> {
    for _ in 0..64 {
        let mut file = open_lock(lock_path)?;
        kgraf_core::waits::lock_file(
            &file,
            kgraf_core::waits::FileLockMode::Exclusive,
            &format!("the migration lock {}", lock_path.display()),
            bound,
        )
        .with_step(|| format!("cannot lock {}", lock_path.display()))?;
        if !still_at(&file, lock_path)? {
            continue;
        }
        let mut mark = String::new();
        file.read_to_string(&mut mark)
            .with_step(|| format!("cannot read {}", lock_path.display()))?;
        return Ok((file, mark == MIGRATION_PENDING));
    }
    Err(Error::failed(format!(
        "the migration lock {} was replaced 64 times while waiting for it; stop the other \
         kGraf processes in this project and run again",
        lock_path.display()
    )))
}

fn conflict(old: &Path, new: &Path) -> Error {
    Error::Invalid(format!(
        "both {} and {} exist. {} is this project's state directory under kGraf's former \
         name, Haustoria (decision 0062), and kGraf cannot tell which one holds the \
         current state. Keep one: move the other out of the project, then run again.",
        old.display(),
        new.display(),
        LEGACY_STATE_DIR
    ))
}

/// Remove the (already cleared) lock file. A failure is said on stderr, never
/// ignored: the file then stays, empty, so it holds no mark and the next
/// first use takes it, finds nothing to do and tries the removal again; the
/// move itself is not undone by it.
fn remove_lock(lock_path: &Path) {
    match std::fs::remove_file(lock_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!(
            "warning: cannot remove the migration lock {} ({e}); it is empty and holds no \
             mark. Remove it by hand.",
            lock_path.display()
        ),
    }
}

fn move_and_repair(
    root: &Path,
    old: &Path,
    new: &Path,
    lock: &mut File,
    lock_path: &Path,
    discover: Discover<'_>,
) -> Result<Option<MigrationReport>> {
    let meta = old
        .symlink_metadata()
        .with_step(|| format!("cannot read {}", old.display()))?;
    if !meta.is_dir() {
        return Err(Error::Invalid(format!(
            "{} is not a directory, so it cannot be moved to {}; remove it and run again",
            old.display(),
            new.display()
        )));
    }
    // Every refusal comes before the rename, so a refused move leaves the
    // project exactly as it was.
    refuse_links(old)?;
    let git = git_context(root, old, discover)?;
    refuse_if_in_use(root, old, git.as_ref())?;
    lock.set_len(0)
        .and_then(|()| lock.rewind())
        .and_then(|()| lock.write_all(MIGRATION_PENDING.as_bytes()))
        .and_then(|()| lock.sync_all())
        .with_step(|| format!("cannot write {}", lock_path.display()))?;
    std::fs::rename(old, new).with_step(|| {
        format!(
            "cannot move {} to {}; stop every Haustoria or kGraf process working in this \
             project and run again",
            old.display(),
            new.display()
        )
    })?;
    repair(root, old, new, git.as_ref()).map(Some)
}

/// Refuse a symbolic link where the move writes or locks: a JSON
/// file or temporary file directly in the state directory, the graph
/// database and its side files, and a worktree hold file.
fn refuse_links(state: &Path) -> Result<()> {
    let entries =
        std::fs::read_dir(state).with_step(|| format!("cannot list {}", state.display()))?;
    for entry in entries {
        let entry = entry.with_step(|| format!("cannot list {}", state.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let written = name.ends_with(".json")
            || name.ends_with(TMP_SUFFIX)
            || name.starts_with(kgraf_graph::snapshot::GRAPH_DB);
        let kind = entry
            .file_type()
            .with_step(|| format!("cannot read {}", entry.path().display()))?;
        if written && kind.is_symlink() {
            return Err(refuse_link(&entry.path()));
        }
    }
    for base in ["worktrees", "workspaces"] {
        let holds = state.join(base).join(".holds");
        let Ok(entries) = std::fs::read_dir(&holds) else {
            continue;
        };
        for entry in entries {
            let entry = entry.with_step(|| format!("cannot list {}", holds.display()))?;
            let kind = entry
                .file_type()
                .with_step(|| format!("cannot read {}", entry.path().display()))?;
            if kind.is_symlink() {
                return Err(refuse_link(&entry.path()));
            }
        }
    }
    Ok(())
}

/// Whether `state` holds worktrees: an entry under `worktrees/` or
/// `workspaces/` that is not one of the manager's own dot files.
fn has_worktrees(state: &Path) -> bool {
    ["worktrees", "workspaces"].iter().any(|base| {
        std::fs::read_dir(state.join(base)).is_ok_and(|mut entries| {
            entries.any(|e| e.is_ok_and(|e| !e.file_name().to_string_lossy().starts_with('.')))
        })
    })
}

/// git for the repairs, or `None` only when git itself says `root` is not a
/// git repository (a scratch directory for `kgraf worker run`), which has no
/// registrations to fix. Any other failure refuses the move: a
/// move whose repairs were skipped leaves each worktree unregistered at its
/// new path, and the next startup cleanup would delete it with its work. git
/// missing refuses too, when the state directory holds worktrees.
fn git_context(root: &Path, state: &Path, discover: Discover<'_>) -> Result<Option<Git>> {
    let git = match discover() {
        Ok(git) => git,
        Err(e) if has_worktrees(state) => {
            return Err(Error::Invalid(format!(
                "git is needed to move {} and repair its worktrees, and it cannot be run \
                 ({e}); nothing was moved. Put git on PATH and run again.",
                state.display()
            )));
        }
        Err(_) => return Ok(None),
    };
    match git.run_str(root, ["rev-parse", "--show-toplevel"]) {
        Ok(_) => Ok(Some(git)),
        Err(WorkspaceError::GitFailed { stderr, .. })
            if stderr.contains("not a git repository") =>
        {
            Ok(None)
        }
        Err(e) => Err(Error::Invalid(format!(
            "git cannot read the repository at {} ({e}); {} was not moved, since its \
             worktree registrations could not be repaired. Fix the repository (for \
             example `git config --global --add safe.directory`), then run again.",
            root.display(),
            state.display()
        ))),
    }
}

/// Refuse to move a directory an older process is still working in: a live
/// owner of a worktree lock under it, or a process holding a worktree's hold
/// file (every sandboxed process holds one until it exits).
fn refuse_if_in_use(root: &Path, old: &Path, git: Option<&Git>) -> Result<()> {
    let forms = path_forms(old);
    if let Some(git) = git {
        let listed = git
            .worktree_list(root)
            .step("cannot list the repository's worktrees before moving the state directory")?;
        for entry in listed {
            let under = forms.iter().any(|f| entry.path.starts_with(f));
            let running = entry
                .locked
                .as_deref()
                .and_then(kgraf_sandbox::lock_owner_may_be_running)
                == Some(true);
            if under && running {
                return Err(Error::Invalid(format!(
                    "a process of an earlier version still works in {}; stop it (or let \
                     it finish), then run again to move {} to {}",
                    entry.path.display(),
                    LEGACY_STATE_DIR,
                    STATE_DIR
                )));
            }
        }
    }
    for base in ["worktrees", "workspaces"] {
        let holds = old.join(base).join(".holds");
        let Ok(entries) = std::fs::read_dir(&holds) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Links were refused by `refuse_links`; open without following
            // anyway, so a link planted since then is not opened either.
            let Ok(file) = no_follow(OpenOptions::new().read(true)).open(&path) else {
                continue;
            };
            match file.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(Error::Invalid(format!(
                        "a process is still running in worktree `{}` under {}; stop it, \
                         then run again to move {} to {}",
                        entry.file_name().to_string_lossy(),
                        old.display(),
                        LEGACY_STATE_DIR,
                        STATE_DIR
                    )));
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(e).with_step(|| format!("cannot check {}", path.display()));
                }
            }
        }
    }
    Ok(())
}

/// Every repair, each idempotent, so an interrupted move is finished by
/// running this again.
fn repair(root: &Path, old: &Path, new: &Path, git: Option<&Git>) -> Result<MigrationReport> {
    let mut report = MigrationReport {
        from: old.to_path_buf(),
        to: new.to_path_buf(),
        ..MigrationReport::default()
    };
    let pairs = prefix_pairs(root);
    if let Some(git) = git {
        repair_worktrees(git, root, &pairs, &mut report)?;
        move_attempt_refs(git, root, &mut report)?;
    }
    rewrite_records(root, new, &pairs, &mut report)?;
    rewrite_files(new, &pairs, &mut report)?;
    Ok(report)
}

/// The spellings of `path` a record may hold: as given, canonical (plain),
/// and on Windows canonical with the `\\?\` prefix, which older versions
/// wrote because the standard canonicalization returns it.
fn path_forms(path: &Path) -> Vec<PathBuf> {
    let mut forms = vec![path.to_path_buf()];
    let canonical = path
        .parent()
        .and_then(|p| kgraf_core::plain_path::canonicalize(p).ok())
        .zip(path.file_name())
        .map(|(p, n)| p.join(n));
    if let Some(c) = canonical {
        if cfg!(windows) {
            let text = c.to_string_lossy();
            let verbatim = match text.strip_prefix(r"\\") {
                Some(unc) => format!(r"\\?\UNC\{unc}"),
                None => format!(r"\\?\{text}"),
            };
            forms.push(PathBuf::from(verbatim));
        }
        forms.push(c);
    }
    forms.dedup();
    forms
}

/// `(old, new)` absolute state-directory paths, in every spelling of `root`.
fn prefix_pairs(root: &Path) -> Vec<(String, String)> {
    let olds = path_forms(&root.join(LEGACY_STATE_DIR));
    let mut pairs: Vec<(String, String)> = olds
        .iter()
        .filter_map(|o| {
            // The new path is written in plain form even for a verbatim old
            // spelling: git, which reads some of these records, rejects `\\?\`.
            let parent = kgraf_core::plain_path::simplify(o.parent()?).into_owned();
            Some((
                o.to_string_lossy().into_owned(),
                parent.join(STATE_DIR).to_string_lossy().into_owned(),
            ))
        })
        .collect();
    pairs.dedup();
    pairs
}

/// `path` with an old state-directory prefix replaced, if it has one.
fn moved_path(path: &Path, pairs: &[(String, String)]) -> Option<PathBuf> {
    pairs.iter().find_map(|(old, new)| {
        path.strip_prefix(old)
            .ok()
            .map(|rest| Path::new(new).join(rest))
    })
}

/// Resolve the path a gitdir-style file holds, relative to `base` if relative.
fn read_link_file(file: &Path, base: &Path) -> Result<PathBuf> {
    let text =
        std::fs::read_to_string(file).with_step(|| format!("cannot read {}", file.display()))?;
    let raw = text.trim();
    let raw = raw.strip_prefix("gitdir:").map(str::trim).unwrap_or(raw);
    let path = PathBuf::from(raw);
    Ok(if path.is_absolute() {
        path
    } else {
        base.join(path)
    })
}

/// `path` as the `gitdir` file of a worktree holds it: with `/` on Windows
/// too, so the file reads the same whether the tool or this migration wrote it.
fn as_git_writes(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

fn repair_worktrees(
    git: &Git,
    root: &Path,
    pairs: &[(String, String)],
    report: &mut MigrationReport,
) -> Result<()> {
    let common = git
        .run_str(
            root,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .step("cannot find the repository's git directory")?;
    let admin_root = PathBuf::from(common.trim()).join("worktrees");
    let Ok(admins) = std::fs::read_dir(&admin_root) else {
        return Ok(());
    };
    let mut moved = Vec::new();
    for admin in admins.flatten() {
        let admin = admin.path();
        let gitdir_file = admin.join("gitdir");
        if !gitdir_file.is_file() {
            continue;
        }
        let pointed = read_link_file(&gitdir_file, &admin)?;
        let Some(target) = moved_path(&normalize(&pointed), pairs) else {
            continue;
        };
        // `target` is `<new state dir>/.../<worktree>/.git`.
        let worktree = target.parent().map(Path::to_path_buf).unwrap_or_default();
        if worktree.is_dir() {
            moved.push(worktree);
        } else {
            // The worktree was already gone before the move. Point its
            // registration at where it would now be, so startup cleanup finds
            // it missing there and handles it as it would have.
            write_atomically(
                &gitdir_file,
                format!("{}\n", as_git_writes(&target)).as_bytes(),
            )?;
            report.registrations_rewritten += 1;
        }
    }
    if !moved.is_empty() {
        let mut args: Vec<std::ffi::OsString> = vec!["worktree".into(), "repair".into()];
        args.extend(moved.iter().map(|p| p.as_os_str().to_owned()));
        let common = git
            .common_dir(root)
            .step("cannot find the git directory to repair worktrees in")?;
        kgraf_sandbox::git::with_worktree_admin_lock(&common, || git.run(root, &args))
            .step("git worktree repair failed after moving the state directory")?;
    }
    // Verify: every moved worktree's link and its registration agree, and no
    // registration still names the old directory.
    for worktree in &moved {
        let link = read_link_file(&worktree.join(".git"), worktree)?;
        let back = read_link_file(&link.join("gitdir"), &link)?;
        let expected = worktree.join(".git");
        if canonical_or(&back) != canonical_or(&expected) {
            return Err(Error::failed(format!(
                "after moving the state directory, worktree {} is registered as {}, not \
                 {}; run `git worktree repair {}` in {}",
                worktree.display(),
                back.display(),
                expected.display(),
                worktree.display(),
                root.display()
            )));
        }
        report.worktrees_repaired.push(worktree.clone());
    }
    if let Ok(admins) = std::fs::read_dir(&admin_root) {
        for admin in admins.flatten() {
            let admin = admin.path();
            let gitdir_file = admin.join("gitdir");
            if !gitdir_file.is_file() {
                continue;
            }
            let pointed = normalize(&read_link_file(&gitdir_file, &admin)?);
            if moved_path(&pointed, pairs).is_some() {
                return Err(Error::failed(format!(
                    "{} still names {} after the move",
                    gitdir_file.display(),
                    pointed.display()
                )));
            }
        }
    }
    Ok(())
}

/// Lexically resolve `.` and `..` (a relative gitdir may contain them); the
/// path may not exist, so no canonicalize.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn canonical_or(path: &Path) -> PathBuf {
    kgraf_core::plain_path::canonicalize(path).unwrap_or_else(|_| normalize(path))
}

fn move_attempt_refs(git: &Git, root: &Path, report: &mut MigrationReport) -> Result<()> {
    let listed = git
        .run_str(
            root,
            [
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                LEGACY_REF_PREFIX,
            ],
        )
        .step("cannot list the attempt refs")?;
    for line in listed.lines().filter(|l| !l.trim().is_empty()) {
        let Some((old_ref, oid)) = line.split_once(' ') else {
            continue;
        };
        let Some(rest) = old_ref.strip_prefix(LEGACY_REF_PREFIX) else {
            continue;
        };
        let new_ref = format!("{REF_PREFIX}{rest}");
        let current = git
            .run_str(root, ["rev-parse", "--verify", "--quiet", new_ref.as_str()])
            .ok();
        match current.as_deref().map(str::trim) {
            None => {
                git.run(root, ["update-ref", new_ref.as_str(), oid, ""])
                    .with_step(|| format!("cannot create {new_ref}"))?;
            }
            Some(existing) if existing == oid => {}
            Some(existing) => {
                return Err(Error::failed(format!(
                    "{old_ref} ({oid}) cannot move to {new_ref}, which already names \
                     {existing}; remove the one that is not wanted and run again"
                )));
            }
        }
        git.run(root, ["update-ref", "-d", old_ref, oid])
            .with_step(|| format!("cannot remove {old_ref}"))?;
        report.refs_moved += 1;
    }
    Ok(())
}

/// Replace each old state-directory prefix in JSON (or plain) text, where it
/// is followed by something that ends a path component. The text is compared
/// in its JSON-escaped form, so a Windows path (`\` written `\\`) matches.
/// `None` when nothing changed.
pub(crate) fn rewrite_text(text: &str, pairs: &[(String, String)]) -> Option<String> {
    let mut out = text.to_owned();
    for (old, new) in pairs {
        for (from, to) in [
            (json_escaped(old), json_escaped(new)),
            (old.clone(), new.clone()),
        ] {
            out = replace_bounded(&out, &from, &to);
        }
    }
    (out != text).then_some(out)
}

fn json_escaped(s: &str) -> String {
    let quoted = serde_json::to_string(s).unwrap_or_default();
    quoted
        .strip_prefix('"')
        .and_then(|q| q.strip_suffix('"'))
        .unwrap_or(&quoted)
        .to_owned()
}

/// `text` with every `from` replaced by `to`, only where `from` is a whole
/// path: it neither continues into a longer name after the match
/// (`.haustoria-x`, `.haustorias`) nor sits inside a longer path before it
/// (`/backup/<root>/.haustoria/x` names a copy, not this project).
fn replace_bounded(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return text.to_owned();
    }
    let path_char = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '~');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    // The character just before `rest`, in the whole text.
    let mut prev: Option<char> = None;
    while let Some(at) = rest.find(from) {
        let before = rest[..at].chars().next_back().or(prev);
        let after = &rest[at + from.len()..];
        let continues = after.chars().next().is_some_and(path_char);
        let inside = before.is_some_and(|c| path_char(c) || matches!(c, '/' | '\\'));
        out.push_str(&rest[..at]);
        out.push_str(if continues || inside { from } else { to });
        prev = from.chars().next_back();
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Whether `text` still names the former state directory in any spelling.
fn names_old(text: &str) -> bool {
    text.to_lowercase().contains(LEGACY_STATE_DIR)
}

fn rewrite_records(
    root: &Path,
    new: &Path,
    pairs: &[(String, String)],
    report: &mut MigrationReport,
) -> Result<()> {
    let db = new.join(kgraf_graph::snapshot::GRAPH_DB);
    match db.symlink_metadata() {
        Ok(meta) if is_link(&meta) => return Err(refuse_link(&db)),
        Ok(meta) if meta.is_file() => {}
        _ => return Ok(()),
    }
    // Never rebuilds: a schema version this build does not have is refused,
    // and a rebuild would drop the records anyway (see `RunStore`).
    let store = match kgraf_graph::GraphStore::open_existing_for_facts(&db, root) {
        Ok(store) => store,
        Err(kgraf_graph::GraphError::SchemaMismatch { .. }) => {
            report.records_skipped = Some(format!(
                "{} has another schema version and is rebuilt on its next open",
                db.display()
            ));
            return Ok(());
        }
        Err(e) => return Err(e).with_step(|| format!("cannot open {}", db.display())),
    };
    let rows = store
        .meta_with_prefix("")
        .with_step(|| format!("cannot read the records in {}", db.display()))?;
    for (key, value) in rows {
        let value = match rewrite_text(&value, pairs) {
            Some(updated) => {
                store
                    .set_meta(&key, &updated)
                    .with_step(|| format!("cannot update record {key} in {}", db.display()))?;
                report.records_rewritten += 1;
                updated
            }
            None => value,
        };
        if names_old(&value) {
            report.still_naming_old.push(format!(
                "{} record `{key}`",
                kgraf_graph::snapshot::GRAPH_DB
            ));
        }
    }
    Ok(())
}

/// The JSON files directly in the state directory (configuration and allow
/// rules); never `runs/` (history) or `worktrees/` (the user's code).
fn rewrite_files(
    new: &Path,
    pairs: &[(String, String)],
    report: &mut MigrationReport,
) -> Result<()> {
    let entries = std::fs::read_dir(new).with_step(|| format!("cannot list {}", new.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.with_step(|| format!("cannot list {}", new.display()))?;
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let kind = entry
            .file_type()
            .with_step(|| format!("cannot read {}", path.display()))?;
        if kind.is_symlink() {
            return Err(refuse_link(&path));
        }
        if kind.is_file() {
            paths.push(path);
        }
    }
    paths.sort();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .with_step(|| format!("cannot read {}", path.display()))?;
        let text = match rewrite_text(&text, pairs) {
            Some(updated) => {
                // A file that parsed must still parse: the rewrite changes
                // only characters inside strings.
                if serde_json::from_str::<serde_json::Value>(&text).is_ok()
                    && serde_json::from_str::<serde_json::Value>(&updated).is_err()
                {
                    return Err(Error::failed(format!(
                        "rewriting paths in {} would break its JSON; it was left as it was",
                        path.display()
                    )));
                }
                write_atomically(&path, updated.as_bytes())?;
                report.files_rewritten.push(path.clone());
                updated
            }
            None => text,
        };
        if names_old(&text) {
            report.still_naming_old.push(
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
        }
    }
    Ok(())
}

/// The suffix of the temporary file [`write_atomically`] writes beside its
/// target.
const TMP_SUFFIX: &str = ".kgraf-migrate.tmp";

/// Replace `path`'s contents through a temporary file and a rename, keeping
/// its permissions (the allow-rule store refuses a file others can write).
/// Neither the target nor the temporary file is ever a link that is followed
///: the target must be a regular file, and the temporary file is
/// created new, without following a link, after a leftover regular one from
/// a killed run is removed.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let meta = path
        .symlink_metadata()
        .with_step(|| format!("cannot read {}", path.display()))?;
    if is_link(&meta) || !meta.is_file() {
        return Err(refuse_link(path));
    }
    let permissions = meta.permissions();
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(TMP_SUFFIX);
    let tmp = path.with_file_name(tmp_name);
    match tmp.symlink_metadata() {
        Ok(m) if is_link(&m) || !m.is_file() => return Err(refuse_link(&tmp)),
        Ok(_) => std::fs::remove_file(&tmp)
            .with_step(|| format!("cannot remove the leftover {}", tmp.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_step(|| format!("cannot read {}", tmp.display())),
    }
    let mut created = false;
    let written = (|| -> std::io::Result<()> {
        let mut file = no_follow(OpenOptions::new().write(true).create_new(true)).open(&tmp)?;
        created = true;
        file.set_permissions(permissions)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    // Only a file this call created is removed: never a link or a file some
    // other writer planted at the temporary name.
    if written.is_err() && created {
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_step(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_migration_lock_times_out_with_a_named_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MIGRATION_LOCK);
        let holder = File::create(&path).unwrap();
        holder.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter_path = path.clone();
        std::thread::spawn(move || {
            let got = acquire_lock_within(&waiter_path, std::time::Duration::from_millis(200));
            let _ = tx.send(got.map(|_| ()).map_err(|e| format!("{e:#}")));
        });
        let err = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the wait ended")
            .expect_err("held");
        assert!(err.contains("the migration lock"), "{err}");
        assert!(err.contains("gave up"), "{err}");
        holder.unlock().unwrap();
        acquire_lock_within(&path, std::time::Duration::from_secs(5)).expect("free");
    }

    fn pairs() -> Vec<(String, String)> {
        vec![("/p/.haustoria".to_owned(), "/p/.kgraf".to_owned())]
    }

    /// A registration this migration writes reads as git writes its own,
    /// with `/` between the names on Windows too.
    #[test]
    fn a_registration_is_written_with_slashes_as_git_writes_it() {
        let path = Path::new("state").join("worktrees").join("w2").join(".git");
        assert_eq!(as_git_writes(&path), "state/worktrees/w2/.git");
    }

    #[test]
    fn a_path_prefix_is_rewritten_only_where_it_ends_a_component() {
        let text = r#"{"workspace":"/p/.haustoria/worktrees/w","other":"/p/.haustoria-x/y","end":"/p/.haustoria","note":"/q/.haustoria/z"}"#;
        let out = rewrite_text(text, &pairs()).expect("changed");
        assert_eq!(
            out,
            r#"{"workspace":"/p/.kgraf/worktrees/w","other":"/p/.haustoria-x/y","end":"/p/.kgraf","note":"/q/.haustoria/z"}"#
        );
        assert_eq!(rewrite_text(&out, &pairs()), None, "idempotent");
    }

    /// The prefix inside a longer path names a copy of the
    /// project, not the project, and is left as it is.
    #[test]
    fn a_path_prefix_inside_a_longer_path_is_left_alone() {
        let text = r#"{"a":"/backup/p/.haustoria/x","b":"x/p/.haustoria/y","c":"cwd=/p/.haustoria/z","d":"/p/.haustoria/w"}"#;
        let out = rewrite_text(text, &pairs()).expect("changed");
        assert_eq!(
            out,
            r#"{"a":"/backup/p/.haustoria/x","b":"x/p/.haustoria/y","c":"cwd=/p/.kgraf/z","d":"/p/.kgraf/w"}"#
        );
    }

    /// A planted link at the temporary name is never written
    /// through, and never removed.
    #[cfg(unix)]
    #[test]
    fn a_link_at_the_temporary_name_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, "keep").expect("victim");
        let target = dir.path().join("run.json");
        std::fs::write(&target, "{}").expect("target");
        let tmp = dir.path().join(format!("run.json{TMP_SUFFIX}"));
        std::os::unix::fs::symlink(&victim, &tmp).expect("link");
        let err = write_atomically(&target, b"{\"x\":1}").expect_err("refused");
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert_eq!(std::fs::read_to_string(&victim).expect("read"), "keep");
        assert!(
            tmp.symlink_metadata()
                .expect("still there")
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "{}");

        // A link as the target itself is refused too.
        let linked = dir.path().join("worker.json");
        std::os::unix::fs::symlink(&victim, &linked).expect("link");
        assert!(write_atomically(&linked, b"x").is_err());
        assert_eq!(std::fs::read_to_string(&victim).expect("read"), "keep");

        // A leftover regular temporary file from a killed run is replaced.
        std::fs::remove_file(&tmp).expect("rm");
        std::fs::write(&tmp, "stale").expect("stale");
        write_atomically(&target, b"{\"x\":1}").expect("written");
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "{\"x\":1}");
        assert!(!tmp.exists());
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .output()
            .expect("git");
        assert!(out.status.success(), "{args:?}: {out:?}");
    }

    /// A repository whose `.haustoria/worktrees` holds one registered
    /// worktree with uncommitted work.
    fn legacy_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("dir");
        let root = kgraf_core::plain_path::canonicalize(dir.path()).expect("canonical");
        git_in(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "a").expect("write");
        git_in(&root, &["add", "a.txt"]);
        git_in(&root, &["commit", "-q", "-m", "one"]);
        let base = root.join(LEGACY_STATE_DIR).join("worktrees");
        let manager = kgraf_sandbox::WorkspaceManager::open(&root, &base).expect("manager");
        let ws = manager
            .create(&kgraf_sandbox::WorkspaceSpec {
                name: "w1",
                base: "HEAD",
                branch: None,
            })
            .expect("w1");
        std::fs::write(ws.root().join("work.txt"), "unsaved").expect("work");
        (dir, root)
    }

    /// A git that runs `script` (a POSIX shell body; `"$@"` holds git's
    /// arguments, `--no-pager -C <dir>` first) with the real git as `$REAL`.
    /// The host runner's version check and config listing always reach the
    /// real git, so `script` sees only the commands kGraf asked for.
    #[cfg(unix)]
    fn scripted_git(dir: &Path, script: &str) -> Git {
        use std::os::unix::fs::PermissionsExt;
        let real = Git::discover().expect("git");
        let path = dir.join("fake-git");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nREAL='{}'\ncase \"$*\" in version|*' config --includes -z --list') exec \"$REAL\" \"$@\";; esac\n{script}\n",
                real.exe().display()
            ),
        )
        .expect("script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Git::at(path)
    }

    /// Git missing while the old directory holds worktrees refuses
    /// before anything moves; the worktree and its work are untouched.
    #[test]
    fn git_missing_with_worktrees_refuses_before_the_move() {
        let (_dir, root) = legacy_repo();
        let missing = || Err(WorkspaceError::GitNotFound);
        let err = migrate_state_dir_with(&root, &missing).expect_err("refused");
        assert!(err.to_string().contains("git is needed"), "{err}");
        assert!(
            root.join(LEGACY_STATE_DIR)
                .join("worktrees/w1/work.txt")
                .is_file()
        );
        assert!(!root.join(STATE_DIR).exists());
        assert!(!root.join(MIGRATION_LOCK).exists());
    }

    /// Git missing and no worktrees: nothing needs git, so the move goes on.
    #[test]
    fn git_missing_without_worktrees_moves() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(LEGACY_STATE_DIR).join("runs")).expect("mkdir");
        let missing = || Err(WorkspaceError::GitNotFound);
        let report = migrate_state_dir_with(root, &missing)
            .expect("moved")
            .expect("report");
        assert!(report.worktrees_repaired.is_empty());
        assert!(root.join(STATE_DIR).join("runs").is_dir());
    }

    /// Git that fails for another reason than "not a git
    /// repository" (here dubious ownership) refuses before the move.
    #[cfg(unix)]
    #[test]
    fn a_git_failure_other_than_not_a_repository_refuses_before_the_move() {
        let (dir, root) = legacy_repo();
        let tools = dir.path().join("tools");
        std::fs::create_dir_all(&tools).expect("mkdir");
        let dubious = scripted_git(
            &tools,
            "echo \"fatal: detected dubious ownership in repository at '$3'\" >&2\nexit 128",
        );
        let discover = move || Ok(dubious.clone());
        let err = migrate_state_dir_with(&root, &discover).expect_err("refused");
        assert!(err.to_string().contains("dubious ownership"), "{err}");
        assert!(
            root.join(LEGACY_STATE_DIR)
                .join("worktrees/w1/work.txt")
                .is_file()
        );
        assert!(!root.join(STATE_DIR).exists());

        // "not a git repository" is the one failure that means no repairs.
        let not_repo = scripted_git(
            &tools,
            "echo 'fatal: not a git repository (or any of the parent directories): .git' >&2\n\
             exit 128",
        );
        assert!(
            git_context(&root, &root.join(LEGACY_STATE_DIR), &move || Ok(
                not_repo.clone()
            ))
            .expect("no git context")
            .is_none()
        );
    }

    /// A repair that git reports as done but that did not take is
    /// caught by the verification, and the pending mark stays for the next
    /// use to finish.
    #[cfg(unix)]
    #[test]
    fn a_repair_that_did_not_take_fails_the_verification() {
        let (dir, root) = legacy_repo();
        let tools = dir.path().join("tools");
        std::fs::create_dir_all(&tools).expect("mkdir");
        // Passes everything to the real git except `worktree repair`, which
        // claims success and does nothing.
        let lazy = scripted_git(
            &tools,
            "if [ \"$4\" = worktree ] && [ \"$5\" = repair ]; then exit 0; fi\nexec \"$REAL\" \"$@\"",
        );
        let discover = move || Ok(lazy.clone());
        let err = migrate_state_dir_with(&root, &discover).expect_err("caught");
        assert!(err.to_string().contains("is registered as"), "{err}");
        assert!(root.join(MIGRATION_LOCK).is_file(), "pending mark kept");
        // The next use, with a git that repairs, finishes the move.
        let report = migrate_state_dir(&root).expect("finished").expect("report");
        assert!(report.resumed);
        assert_eq!(report.worktrees_repaired.len(), 1);
        assert!(!root.join(MIGRATION_LOCK).exists());
    }

    #[test]
    fn a_windows_path_is_matched_in_its_json_escaped_form() {
        let pairs = vec![(r"C:\p\.haustoria".to_owned(), r"C:\p\.kgraf".to_owned())];
        let text = r#"{"workspace": "C:\\p\\.haustoria\\worktrees\\w"}"#;
        let out = rewrite_text(text, &pairs).expect("changed");
        assert_eq!(out, r#"{"workspace": "C:\\p\\.kgraf\\worktrees\\w"}"#);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(parsed["workspace"], r"C:\p\.kgraf\worktrees\w");
    }

    #[test]
    fn the_notice_names_both_directories_and_the_former_name() {
        let report = MigrationReport {
            from: PathBuf::from("/p/.haustoria"),
            to: PathBuf::from("/p/.kgraf"),
            worktrees_repaired: vec![PathBuf::from("/p/.kgraf/worktrees/w")],
            files_rewritten: vec![PathBuf::from("/p/.kgraf/run.json")],
            ..MigrationReport::default()
        };
        let notice = report.notice();
        assert!(notice.contains("/p/.haustoria"), "{notice}");
        assert!(notice.contains("/p/.kgraf"), "{notice}");
        assert!(notice.contains("former name"), "{notice}");
        assert!(notice.contains("Repaired 1 git worktree"), "{notice}");
        assert!(notice.contains("run.json"), "{notice}");
    }
}
