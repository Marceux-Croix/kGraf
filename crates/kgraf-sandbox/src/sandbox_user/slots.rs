//! Which sandbox user a command runs as: one worker slot per folder at a
//! time, held for as long as a worker or a command needs it.
//!
//! Each slot has two users, one without the network ([`Pool::Offline`]) and
//! one with it ([`Pool::Online`]). A command takes a slot of its pool for its
//! first writable root, its folder, and holds it while it runs. A worker
//! holds one for its worktree for its whole life ([`Leases::lease`] through
//! `kgraf_sandbox::worker_slot`), so its slot does not go to another folder
//! between its commands.
//!
//! - Two holders of one folder in one process share the slot: they share the
//!   folder, so they must be able to write each other's files.
//! - Holders of different folders never share a slot. Each slot has a lock
//!   file, held exclusively by the process that holds the slot, so this also
//!   holds between processes.
//! - When every slot of a pool is held by another folder, a worker is
//!   refused at once, with an error that names the fix: a wait would hide a
//!   setup with fewer slots than the machine runs workers, and the
//!   orchestrator already bounds how many run at once. A command without a
//!   worker slot waits for a free slot up to [`COMMAND_WAIT`], because the
//!   holders are commands that end, then is refused the same way.
//!
//! A write grant on a folder stays when its command ends (writing a folder's
//! access list rewrites the inherited entries of everything below it, which
//! costs seconds to minutes on a large build folder). So a slot stays bound
//! to the folders it was granted, in a table beside the stored passwords,
//! and a free slot already bound to a folder is taken again for it. A slot
//! that moves to another folder first loses its grants on the folders it
//! held, so a user never holds write access to a folder that another user
//! may be working in.
//!
//! No lock is held while a grant is revoked. A revoke rewrites the access
//! list of a whole folder tree and can take minutes, so the table lock and
//! the in-process lock are held only to choose a slot and write the table
//! (both bounded). The slot is then leased but not yet usable by a second
//! holder of its folder in this process: that holder waits, with a bound, for
//! the revokes to end ([`SETTLE_WAIT`]). The table lists a slot's old folders
//! until their revokes are done, so a failed or interrupted revoke is
//! retried by the next holder. A slot whose revokes fail is put back as it
//! was and the next candidate is tried; that failure is the answer only
//! when no candidate works.
//!
//! Each write of an access list, a revoke included, is made under the
//! access-list lock ([`lock_acls`]), which is taken for that one write.
//!
//! This replaces a stable fold of the folder's path to a slot, which put two
//! folders on one user, with no message, whenever their paths folded alike.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use kgraf_core::flock::HeldLock;
use kgraf_core::waits::{FileLockMode, LockError, LockTimeout, lock_file, waiting_for_lock};

use super::names::Role;

/// The two kinds of slot user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Pool {
    /// Commands without the network, or limited to kGraf's egress proxy.
    Offline,
    /// Commands with the network open, such as dependency installs.
    Online,
}

impl Pool {
    pub fn word(self) -> &'static str {
        match self {
            Pool::Offline => "offline",
            Pool::Online => "online",
        }
    }

    fn from_word(s: &str) -> Option<Pool> {
        match s {
            "offline" => Some(Pool::Offline),
            "online" => Some(Pool::Online),
            _ => None,
        }
    }

    /// How many users the pool has in a setup with `slots` slots: one per
    /// slot, or the pool's one fixed user where the setup made no slots.
    pub fn size(slots: u8) -> u8 {
        slots.max(1)
    }

    /// The user of slot `slot` in a setup with `slots` slots.
    pub fn role(self, slot: u8, slots: u8) -> Role {
        match (self, slots) {
            (Pool::Offline, 0) => Role::Offline,
            (Pool::Online, 0) => Role::Online,
            (Pool::Offline, _) => Role::Slot(slot),
            (Pool::Online, _) => Role::OnlineSlot(slot),
        }
    }
}

/// One folder, compared without regard to case, as Windows compares paths.
fn key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// The folders one slot user holds a write grant on. The first is the folder
/// the slot is held for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub pool: Pool,
    pub slot: u8,
    pub roots: Vec<PathBuf>,
}

/// Every slot's folders, least recently taken first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    bindings: Vec<Binding>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("line {line} of the sandbox slot table is not `<pool> <slot> <folder>`: {text:?}")]
pub struct TableError {
    pub line: usize,
    pub text: String,
}

/// One line of the table, `<pool> <slot> <folder>`, or `None` where the
/// line is not that.
fn parse_line(line: &str) -> Option<(Pool, u8, PathBuf)> {
    let mut parts = line.splitn(3, ' ');
    let pool = parts.next().and_then(Pool::from_word)?;
    let slot: u8 = parts.next().filter(|s| s.len() == 2)?.parse().ok()?;
    let root = PathBuf::from(parts.next()?);
    root.is_absolute().then_some((pool, slot, root))
}

/// Every folder a slot table text names, each once, and the numbers of the
/// lines that are not a table line.
///
/// For `kgraf sandbox remove`, which must revoke what it can read: one
/// damaged line (a write cut short by a power loss) must not keep the
/// removal from clearing every other folder. [`Table::parse`], which the
/// lease path uses, refuses the whole table instead, because a slot whose
/// folders it cannot read is a slot whose grants it cannot revoke.
pub fn roots_in_text(text: &str) -> (Vec<PathBuf>, Vec<usize>) {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut damaged = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        match parse_line(line) {
            Some((_, _, root)) => {
                if !roots.iter().any(|r| key(r) == key(&root)) {
                    roots.push(root);
                }
            }
            None => damaged.push(n + 1),
        }
    }
    (roots, damaged)
}

impl Table {
    /// One line per folder: `<pool> <slot> <folder>`. A slot's lines are
    /// together, and the slots are in the order they were last taken.
    pub fn parse(text: &str) -> Result<Table, TableError> {
        let mut table = Table::default();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            let (pool, slot, root) = parse_line(line).ok_or(TableError {
                line: n + 1,
                text: line.to_owned(),
            })?;
            match table
                .bindings
                .iter_mut()
                .find(|b| b.pool == pool && b.slot == slot)
            {
                Some(b) if !b.roots.iter().any(|r| key(r) == key(&root)) => b.roots.push(root),
                Some(_) => {}
                None => table.bindings.push(Binding {
                    pool,
                    slot,
                    roots: vec![root],
                }),
            }
        }
        Ok(table)
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for b in &self.bindings {
            for root in &b.roots {
                out.push_str(&format!(
                    "{} {:02} {}\n",
                    b.pool.word(),
                    b.slot,
                    root.display()
                ));
            }
        }
        out
    }

    pub fn binding(&self, pool: Pool, slot: u8) -> Option<&Binding> {
        self.bindings
            .iter()
            .find(|b| b.pool == pool && b.slot == slot)
    }

    /// Every folder any slot user holds a grant on, each once.
    pub fn all_roots(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for root in self.bindings.iter().flat_map(|b| b.roots.iter()) {
            if !out.iter().any(|r| key(r) == key(root)) {
                out.push(root.clone());
            }
        }
        out
    }

    /// The slots of `pool` to try for the folder `primary`, best first:
    /// slots bound to that folder (no grant to move), slots bound to
    /// nothing, slots whose folders are all gone, then the rest, least
    /// recently taken first (their grants are revoked when taken).
    pub fn candidates(
        &self,
        pool: Pool,
        size: u8,
        primary: &Path,
        exists: &dyn Fn(&Path) -> bool,
    ) -> Vec<u8> {
        let want = key(primary);
        let bound: Vec<&Binding> = self
            .bindings
            .iter()
            .filter(|b| b.pool == pool && b.slot < size)
            .collect();
        let mut out: Vec<u8> = Vec::with_capacity(usize::from(size));
        let push = |slot: u8, out: &mut Vec<u8>| {
            if !out.contains(&slot) {
                out.push(slot);
            }
        };
        for b in &bound {
            if b.roots.first().is_some_and(|r| key(r) == want) {
                push(b.slot, &mut out);
            }
        }
        for slot in 0..size {
            if !bound.iter().any(|b| b.slot == slot) {
                push(slot, &mut out);
            }
        }
        for b in &bound {
            if b.roots.iter().all(|r| !exists(r)) {
                push(b.slot, &mut out);
            }
        }
        for b in &bound {
            push(b.slot, &mut out);
        }
        out
    }

    /// The folder each slot of `pool` below `size` was last taken for, as
    /// one sentence for the message of [`SlotError::AllHeld`]. A slot the
    /// table does not name is left out; a slot's holder may be another
    /// process, and a lock file does not say whose it is, so this is what
    /// the slots were last taken for, not proof of who holds them now.
    pub fn holders(&self, pool: Pool, size: u8) -> String {
        let mut named: Vec<String> = Vec::new();
        for slot in 0..size {
            if let Some(root) = self
                .binding(pool, slot)
                .and_then(|b| b.roots.first())
                .map(|r| r.display().to_string())
            {
                named.push(format!("{slot:02} {root}"));
            }
        }
        if named.is_empty() {
            "No folder is recorded for them".to_owned()
        } else {
            format!("They were last taken for: {}", named.join("; "))
        }
    }

    /// Bind `slot` of `pool` to `roots` and make it the most recently taken.
    /// Returns the folders whose grant for that slot's user must be revoked
    /// first: every folder it held for another folder. Taken again for the
    /// same folder, it keeps what it held.
    pub fn bind(&mut self, pool: Pool, slot: u8, roots: &[PathBuf]) -> Vec<PathBuf> {
        let old = self
            .bindings
            .iter()
            .position(|b| b.pool == pool && b.slot == slot)
            .map(|i| self.bindings.remove(i));
        let same_folder = match (&old, roots.first()) {
            (Some(b), Some(first)) => b.roots.first().is_some_and(|r| key(r) == key(first)),
            _ => false,
        };
        let mut binding = Binding {
            pool,
            slot,
            roots: Vec::new(),
        };
        let mut revoke = Vec::new();
        if let Some(old) = old {
            if same_folder {
                binding.roots = old.roots;
            } else {
                revoke = old
                    .roots
                    .into_iter()
                    .filter(|r| !roots.iter().any(|n| key(n) == key(r)))
                    .collect();
            }
        }
        for root in roots {
            if !binding.roots.iter().any(|r| key(r) == key(root)) {
                binding.roots.push(root.clone());
            }
        }
        self.bindings.push(binding);
        revoke
    }

    /// Put `roots` ahead of what `slot` of `pool` holds, so the slot is not
    /// yet bound to the folder it was just given.
    pub fn list_first(&mut self, pool: Pool, slot: u8, roots: &[PathBuf]) {
        if let Some(b) = self
            .bindings
            .iter_mut()
            .find(|b| b.pool == pool && b.slot == slot)
        {
            b.roots.retain(|r| !roots.iter().any(|d| key(d) == key(r)));
            let mut all = roots.to_vec();
            all.append(&mut b.roots);
            b.roots = all;
        }
    }

    /// Make `roots` exactly what `slot` of `pool` holds, in the place the
    /// slot already has in the order, or take the slot out of the table where
    /// `roots` is empty. For a move whose revokes failed
    /// ([`Leases::put_back`]): the slot must look as it did, down to how
    /// recently it was taken, or the next lease would offer the candidates in
    /// another order.
    pub fn set_roots(&mut self, pool: Pool, slot: u8, roots: &[PathBuf]) {
        let at = self
            .bindings
            .iter()
            .position(|b| b.pool == pool && b.slot == slot);
        match (at, roots.is_empty()) {
            (Some(i), true) => {
                self.bindings.remove(i);
            }
            (Some(i), false) => self.bindings[i].roots = roots.to_vec(),
            (None, true) => {}
            (None, false) => self.bindings.push(Binding {
                pool,
                slot,
                roots: roots.to_vec(),
            }),
        }
    }

    /// Remove `roots` from what `slot` of `pool` holds, once their grants are
    /// revoked.
    pub fn drop_roots(&mut self, pool: Pool, slot: u8, roots: &[PathBuf]) {
        if let Some(b) = self
            .bindings
            .iter_mut()
            .find(|b| b.pool == pool && b.slot == slot)
        {
            b.roots.retain(|r| !roots.iter().any(|d| key(d) == key(r)));
        }
    }

    /// Add `roots` to what `slot` of `pool` holds. True when anything was
    /// added.
    pub fn add_roots(&mut self, pool: Pool, slot: u8, roots: &[PathBuf]) -> bool {
        let Some(b) = self
            .bindings
            .iter_mut()
            .find(|b| b.pool == pool && b.slot == slot)
        else {
            self.bindings.push(Binding {
                pool,
                slot,
                roots: roots.to_vec(),
            });
            return !roots.is_empty();
        };
        let mut added = false;
        for root in roots {
            if !b.roots.iter().any(|r| key(r) == key(root)) {
                b.roots.push(root.clone());
                added = true;
            }
        }
        added
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SlotError {
    #[error(
        "every {pool} sandbox user ({size}) is held by a command or worker in another folder, \
         so a command in {} has no user of its own. {held}. Two commands in different folders \
         never run as one sandbox user. Wait for one to finish, or run `kgraf sandbox setup \
         --slots N` with a larger N",
        root.display()
    )]
    AllHeld {
        pool: &'static str,
        size: u8,
        root: PathBuf,
        /// The folder each slot of the pool was last taken for, from the
        /// table, so the message names who holds them. A lock file does not
        /// say who holds it, and the holder may have been another process.
        held: String,
    },
    #[error(transparent)]
    Wait(#[from] LockTimeout),
    #[error(
        "the folder {} cannot be written in the sandbox slot table, which holds one line of \
         text per folder, so a write grant on it could not be revoked later. Rename it \
         without a line break, and with characters this system can give back unchanged",
        path.display()
    )]
    PathNotText { path: PathBuf },
    #[error("a sandboxed command has no writable folder to take a worker slot for")]
    NoRoot,
    #[error(
        "the sandbox slot table {}: {source}. Run `kgraf sandbox remove`, which revokes the \
         folders its readable lines name and deletes it, then `kgraf sandbox setup` again",
        path.display()
    )]
    Table {
        path: PathBuf,
        #[source]
        source: TableError,
    },
    /// The lock names itself and the wait it gave up on.
    #[error(transparent)]
    Lock {
        #[from]
        source: LockError,
    },
    #[error("the sandbox slot files at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "revoking the {role} sandbox user's write access to {}, before it moves to another \
         folder: {why}",
        path.display()
    )]
    Revoke {
        role: Role,
        path: PathBuf,
        why: String,
    },
    #[error("the worker slot was taken for {}, and this command writes {}", held.display(), wants.display())]
    OtherFolder { held: PathBuf, wants: PathBuf },
}

/// How a slot was had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Taken {
    /// Taken now; no other holder had it.
    Fresh,
    /// Already held in this process for the same folder, and shared.
    Shared,
}

/// One held slot. The slot is free again, for every process, once the last
/// clone is dropped.
#[derive(Debug)]
pub struct Lease {
    pool: Pool,
    slot: u8,
    slots: u8,
    primary: PathBuf,
    dir: PathBuf,
    /// The folders this process has recorded for the slot, by key, so a
    /// command that writes nothing new does not touch the table.
    noted: Mutex<Vec<String>>,
    /// Whether the revokes the move to this folder needed are done.
    settle: Settle,
    table_wait: Duration,
    _lock: HeldLock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Settled {
    Pending,
    Ready,
    Failed,
}

/// The state of the revokes a fresh lease waits on, shared with the holders
/// of the same folder in this process.
#[derive(Debug)]
struct Settle {
    state: Mutex<Settled>,
    changed: Condvar,
}

impl Settle {
    fn new(state: Settled) -> Self {
        Self {
            state: Mutex::new(state),
            changed: Condvar::new(),
        }
    }

    fn set(&self, to: Settled) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = to;
        self.changed.notify_all();
    }

    /// Wait for the revokes to end, at most `bound`. True: the slot is
    /// usable. False: the revokes failed, and the caller starts over.
    fn wait(&self, what: &str, bound: Duration) -> Result<bool, LockTimeout> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let _record = (*state == Settled::Pending).then(|| waiting_for_lock(what));
        let started = Instant::now();
        let (state, result) = self
            .changed
            .wait_timeout_while(state, bound, |s| *s == Settled::Pending)
            .unwrap_or_else(|e| e.into_inner());
        if result.timed_out() && *state == Settled::Pending {
            return Err(LockTimeout {
                lock: what.to_owned(),
                waited: started.elapsed(),
            });
        }
        Ok(*state == Settled::Ready)
    }
}

/// Marks a fresh lease failed if its revokes end any way but by `ready`,
/// a panic included, so a waiting holder never waits for nobody.
struct Settling<'a>(&'a Settle, bool);

impl Settling<'_> {
    fn ready(mut self) {
        self.1 = true;
        self.0.set(Settled::Ready);
    }
}

impl Drop for Settling<'_> {
    fn drop(&mut self) {
        if !self.1 {
            self.0.set(Settled::Failed);
        }
    }
}

impl Lease {
    pub fn pool(&self) -> Pool {
        self.pool
    }

    pub fn slot(&self) -> u8 {
        self.slot
    }

    /// The sandbox user that runs commands in this slot.
    pub fn role(&self) -> Role {
        self.pool.role(self.slot, self.slots)
    }

    /// The folder the slot is held for.
    pub fn primary(&self) -> &Path {
        &self.primary
    }

    /// Whether this slot is held for the folder `root`.
    pub fn is_for(&self, root: &Path) -> bool {
        key(&self.primary) == key(root)
    }

    /// Record that the slot's user is granted write on `roots`, so the grant
    /// is revoked when the slot moves to another folder. Call it before the
    /// grant is written.
    pub fn note_roots(&self, roots: &[PathBuf]) -> Result<(), SlotError> {
        let mut noted = self.noted.lock().unwrap_or_else(|e| e.into_inner());
        let new: Vec<PathBuf> = roots
            .iter()
            .filter(|r| !noted.contains(&key(r)))
            .cloned()
            .collect();
        if new.is_empty() {
            return Ok(());
        }
        refuse_untextable(&new)?;
        let _table_lock = lock_table(&self.dir, self.table_wait)?;
        let mut table = read_table(&self.dir)?;
        if table.add_roots(self.pool, self.slot, &new) {
            write_table(&self.dir, &table)?;
        }
        noted.extend(new.iter().map(|r| key(r)));
        Ok(())
    }
}

/// How long a command without a worker slot waits for a free slot before it
/// is refused. Longer than a build or test step usually takes, shorter than
/// a command's own timeout.
pub const COMMAND_WAIT: Duration = Duration::from_secs(60);

/// How long the table lock is waited for. Its longest legitimate holder is
/// another process that reads the table, chooses a slot and writes a small
/// text file: milliseconds. No revoke runs under it. 30 s covers a machine
/// stalled by a scan of its disk.
pub const TABLE_LOCK_WAIT: Duration = Duration::from_secs(30);

/// How long a second holder of a folder waits for the revokes of the holder
/// that took its slot. The longest legitimate holder is a revoke of a large
/// build folder, which the module documentation puts at minutes. 10 minutes
/// is past that; a share that hangs is the case this ends.
pub const SETTLE_WAIT: Duration = Duration::from_secs(600);

/// How often a waiting command tries again.
const RETRY: Duration = Duration::from_millis(100);

/// Revokes the write grant of a slot user on one folder.
pub type Revoke<'a> = &'a mut dyn FnMut(Role, &Path) -> Result<(), String>;

/// The slots of one setup: its slot table, its lock files, and the slots
/// this process holds.
#[derive(Debug)]
pub struct Leases {
    dir: PathBuf,
    slots: u8,
    live: Mutex<HashMap<(Pool, String), Weak<Lease>>>,
    table_wait: Duration,
    settle_wait: Duration,
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> SlotError + '_ {
    move |source| SlotError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// How many times a lock file is opened before the failure is reported.
/// `kgraf sandbox remove` deletes the slot folder, and it can do that while
/// a holder lives: on Windows a file waiting to be deleted refuses every
/// open until its last handle is closed, and the folder itself may be gone.
/// The folder is made again between tries.
const OPEN_TRIES: u32 = 5;
const OPEN_PAUSE: Duration = Duration::from_millis(20);

fn open_lock_file(path: &Path) -> Result<File, SlotError> {
    let mut last = None;
    for attempt in 0..OPEN_TRIES {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
        {
            Ok(file) => return Ok(file),
            Err(e) => last = Some(e),
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if attempt + 1 < OPEN_TRIES {
            std::thread::sleep(OPEN_PAUSE);
        }
    }
    Err(io_error(path)(last.unwrap_or_else(|| {
        io::Error::other("the lock file could not be opened")
    })))
}

/// Whether the table can hold `path` and give it back unchanged: the text
/// of the path must survive the round trip through a line of the table, and
/// it must be one line. A path this refuses would be written in another
/// spelling, and the revoke of that spelling would find nothing and skip
/// without a word; a line break in it would make a line of its own, which
/// the table would read back as another folder.
fn is_text(path: &Path) -> bool {
    let text = path.to_string_lossy();
    Path::new(text.as_ref()) == path && !text.contains(['\n', '\r'])
}

/// Refuse every one of `roots` the table cannot hold exactly ([`is_text`]).
fn refuse_untextable(roots: &[PathBuf]) -> Result<(), SlotError> {
    match roots.iter().find(|r| !is_text(r)) {
        Some(path) => Err(SlotError::PathNotText { path: path.clone() }),
        None => Ok(()),
    }
}

/// The name of the lock around every access-list read-modify-write.
const ACL_LOCK: &str = "acl.lock";

/// How long a read-modify-write of an access list waits for [`lock_acls`].
///
/// The bound is set by the longest legitimate holder: one access-list write
/// on a large build tree. Writing a folder's list rewrites the inherited
/// entries of everything below it (decision 0097, change (a)), which costs
/// seconds to minutes on a worktree with a large `target` folder. Every slot
/// of the pool can want the lock at once, so the bound allows several such
/// writes in a row before it calls the wait a defect. A command's own
/// timeout is usually shorter, so the command normally fails first; this
/// bound exists so a lost or stuck holder becomes an error a person can
/// read, never a hang.
pub const ACL_LOCK_WAIT: Duration = Duration::from_secs(600);

thread_local! {
    /// How deep this thread is inside [`lock_acls`]. The file lock is taken
    /// by the outermost call only: a lock file is exclusive against another
    /// handle of this process as well as against another process, so a
    /// nested take of it would wait for the caller itself.
    static ACL_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A held access-list lock.
#[derive(Debug)]
pub struct AclLock {
    /// The file lock, held by the outermost guard of this thread only.
    _held: Option<HeldLock>,
}

impl Drop for AclLock {
    fn drop(&mut self) {
        ACL_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// The one lock around every read-modify-write of a Windows access list:
/// every grant, every deny and every revoke a sandbox user's confinement
/// writes. Without it, two processes that read one folder's list, then write
/// it back, each write a list that holds its own entry and not the other's,
/// so the last writer takes the first writer's grant away, and a stale list
/// written back can restore a revoked user's entries.
///
/// Held for one such read-modify-write, never for longer: a revoke a lease
/// runs takes it for the one folder it is writing, and [`Leases::lease`]
/// itself takes it at no point. One take around a whole revoke loop would
/// hold a lock with a ten-minute bound for minutes of tree writing, so a
/// slow revoke of one folder would block every other folder's lease, and it
/// would protect nothing more: the hazard is one list, read and written by
/// two writers.
///
/// Reentrant within a thread, so a write that runs inside another write of a
/// list can take it again rather than wait for its own caller.
///
/// **Lock order: this lock, then the slot table's lock.** It is never taken
/// while the table lock is held, so a process that holds one and wants the
/// other can never wait on a process holding them the other way round.
pub fn lock_acls(dir: &Path) -> Result<AclLock, SlotError> {
    lock_acls_within(dir, ACL_LOCK_WAIT)
}

/// [`lock_acls`] with another bound on the wait (its tests).
pub fn lock_acls_within(dir: &Path, bound: Duration) -> Result<AclLock, SlotError> {
    let outermost = ACL_DEPTH.with(|d| {
        let depth = d.get();
        d.set(depth + 1);
        depth == 0
    });
    // The guard owns the count from here: every `?` below drops it, and the
    // count goes back to what it was.
    let mut guard = AclLock { _held: None };
    if !outermost {
        return Ok(guard);
    }
    std::fs::create_dir_all(dir).map_err(io_error(dir))?;
    let path = dir.join(ACL_LOCK);
    let file = open_lock_file(&path)?;
    lock_file(
        &file,
        FileLockMode::Exclusive,
        &format!("the sandbox access-list lock {}", path.display()),
        bound,
    )
    .map_err(|source| SlotError::Lock { source })?;
    guard._held = Some(HeldLock::new(file));
    Ok(guard)
}

/// The table's own lock, held while the table is read and written. Waits at
/// most `bound` for another holder ([`TABLE_LOCK_WAIT`]); a wait that ends
/// is a [`LockTimeout`] that names the lock, and is in
/// `kgraf_core::waits::report` while it lasts.
fn lock_table(dir: &Path, bound: Duration) -> Result<HeldLock, SlotError> {
    std::fs::create_dir_all(dir).map_err(io_error(dir))?;
    let path = dir.join("table.lock");
    let file = open_lock_file(&path)?;
    let name = format!("the sandbox slot table lock ({})", path.display());
    match lock_file(&file, FileLockMode::Exclusive, &name, bound) {
        Ok(()) => Ok(HeldLock::new(file)),
        Err(LockError::Timeout(t)) => Err(t.into()),
        Err(LockError::Io { source, .. }) => Err(io_error(&path)(source)),
    }
}

fn table_path(dir: &Path) -> PathBuf {
    dir.join("table.txt")
}

/// The slot table of the slot folder `dir`; empty when there is none.
pub fn read_table(dir: &Path) -> Result<Table, SlotError> {
    let path = table_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => Table::parse(&text).map_err(|source| SlotError::Table { path, source }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Table::default()),
        Err(e) => Err(io_error(&path)(e)),
    }
}

/// How many times the rename of a written table is tried, and the pause
/// between tries. A scanner (a backup or anti-virus program) can hold the
/// table open for a moment, and the rename fails while it does.
const RENAME_TRIES: u32 = 5;
const RENAME_PAUSE: Duration = Duration::from_millis(20);

fn write_table(dir: &Path, table: &Table) -> Result<(), SlotError> {
    use std::io::Write;
    let path = table_path(dir);
    let tmp = dir.join("table.tmp");
    // Flushed to the device before the rename: without that, a power loss
    // can leave a table of zero length, which reads as a setup that holds no
    // folder at all, so the grants of every folder it listed would never be
    // revoked, and a user would keep write access to a folder another user
    // works in.
    let mut file = File::create(&tmp).map_err(io_error(&tmp))?;
    file.write_all(table.to_text().as_bytes())
        .map_err(io_error(&tmp))?;
    file.sync_all().map_err(io_error(&tmp))?;
    drop(file);
    let mut last = None;
    for attempt in 0..RENAME_TRIES {
        match std::fs::rename(&tmp, &path) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        if attempt + 1 < RENAME_TRIES {
            std::thread::sleep(RENAME_PAUSE);
        }
    }
    Err(io_error(&path)(last.unwrap_or_else(|| {
        io::Error::other("the slot table could not be renamed")
    })))
}

impl Leases {
    /// The slots of a setup with `slots` slots, whose table and lock files
    /// live in `dir`.
    pub fn new(dir: PathBuf, slots: u8) -> Self {
        Self {
            dir,
            slots,
            live: Mutex::new(HashMap::new()),
            table_wait: TABLE_LOCK_WAIT,
            settle_wait: SETTLE_WAIT,
        }
    }

    /// Other bounds, for tests that must reach them quickly.
    #[cfg(test)]
    fn with_waits(mut self, table_wait: Duration, settle_wait: Duration) -> Self {
        self.table_wait = table_wait;
        self.settle_wait = settle_wait;
        self
    }

    pub fn slots(&self) -> u8 {
        self.slots
    }

    /// A slot of `pool` for the folder `roots[0]`, whose user is then
    /// granted write on every one of `roots`. Shared with a holder of the
    /// same folder in this process; never with a holder of another folder,
    /// in this process or another. Refused when every slot of the pool is
    /// held for another folder. A slot that held other folders loses its
    /// grants there first, through `revoke`; one whose grants cannot all be
    /// revoked is left as it is and the next candidate is tried, and that
    /// failure is returned only when no candidate works.
    ///
    /// Order, and why it keeps every rule of the module:
    ///
    /// 1. Under the in-process lock and the table lock (both short, the
    ///    second bounded), one candidate slot is chosen, bound, and the table
    ///    written, with the folders the slot is losing listed ahead of the
    ///    new ones. The in-process lock is held until the new lease is in
    ///    `live`, so two threads of this process never both take a slot for
    ///    one folder.
    /// 2. Both locks are released. Then the old folders are revoked, with the
    ///    slot's file lock held, so no other process takes the slot, and with
    ///    the lease marked pending, so a second holder of the folder in this
    ///    process waits ([`Settle`]) and runs nothing until the old grants
    ///    are gone. The caller of this function gets the lease only after
    ///    that, so the same holds for it.
    /// 3. A revoke that fails ends that candidate: its slot lock goes, its
    ///    table entry is put back as it was ([`Leases::put_back`]), and step
    ///    1 runs again for the next candidate. The first failure is returned
    ///    only when no candidate works.
    ///
    /// Lock order: the access-list lock ([`lock_acls`]), then the table lock,
    /// never the other way round. This function holds neither across a
    /// revoke, and never holds both: the table lock is released before the
    /// revokes run, and each revoke takes the access-list lock for its own
    /// read-modify-write of one folder's list (the Windows backend's
    /// `revoke_sandbox_grant` does), which is where that lock belongs. One
    /// take of it around the whole revoke loop instead would hold a lock with
    /// a ten-minute bound across minutes of tree writing, so a slow revoke of
    /// one folder would block every other folder's lease, which rules 1 and 2
    /// above exist to prevent; and it would protect nothing more, because the
    /// hazard is one list read and written by two writers, which the per-write
    /// take already shuts out.
    pub fn lease(
        &self,
        pool: Pool,
        roots: &[PathBuf],
        revoke: Revoke<'_>,
    ) -> Result<(Arc<Lease>, Taken), SlotError> {
        loop {
            if let Some(done) = self.lease_once(pool, roots, &mut *revoke)? {
                return Ok(done);
            }
        }
    }

    /// One try of [`Self::lease`]. `None`: the holder this one shared with
    /// failed its revokes, so start over.
    fn lease_once(
        &self,
        pool: Pool,
        roots: &[PathBuf],
        revoke: Revoke<'_>,
    ) -> Result<Option<(Arc<Lease>, Taken)>, SlotError> {
        let primary = roots.first().ok_or(SlotError::NoRoot)?;
        refuse_untextable(roots)?;
        // The candidates whose revokes failed in this call, never offered
        // again here, and the first of those failures, which is the answer
        // when no candidate works at all.
        let mut tried: Vec<u8> = Vec::new();
        let mut stuck: Option<SlotError> = None;
        loop {
            // Held while a slot is chosen, so two threads of this process do
            // not both take a slot for one folder. Dropped before the revokes
            // run, which can take minutes.
            let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            live.retain(|_, held| held.strong_count() > 0);
            if let Some(held) = live.get(&(pool, key(primary))).and_then(Weak::upgrade) {
                drop(live);
                let what = format!(
                    "the revokes of sandbox slot {}-{:02} for {}",
                    pool.word(),
                    held.slot,
                    primary.display()
                );
                if !held.settle.wait(&what, self.settle_wait)? {
                    return Ok(None);
                }
                held.note_roots(roots)?;
                return Ok(Some((held, Taken::Shared)));
            }
            let table_lock = lock_table(&self.dir, self.table_wait)?;
            let mut table = read_table(&self.dir)?;
            let size = Pool::size(self.slots);
            let mut chosen = None;
            for slot in table.candidates(pool, size, primary, &|p: &Path| p.exists()) {
                if tried.contains(&slot) {
                    continue;
                }
                let path = self.dir.join(format!("{}-{slot:02}.lock", pool.word()));
                let file = open_lock_file(&path)?;
                match file.try_lock() {
                    Ok(()) => {
                        chosen = Some((slot, HeldLock::new(file)));
                        break;
                    }
                    Err(std::fs::TryLockError::WouldBlock) => continue,
                    Err(std::fs::TryLockError::Error(e)) => return Err(io_error(&path)(e)),
                }
            }
            let Some((slot, lock)) = chosen else {
                return Err(stuck.unwrap_or_else(|| SlotError::AllHeld {
                    pool: pool.word(),
                    size,
                    root: primary.clone(),
                    held: table.holders(pool, size),
                }));
            };
            let role = pool.role(slot, self.slots);
            // What the slot held, to put back where a revoke fails below.
            let held_before = table
                .binding(pool, slot)
                .map(|b| b.roots.clone())
                .unwrap_or_default();
            let to_revoke = table.bind(pool, slot, roots);
            // The old folders stay listed, ahead of the new ones, until their
            // grants are revoked. A holder that fails or stops in between
            // leaves a slot that is not yet bound to the new folder, so the
            // next holder of that folder revokes them again.
            table.list_first(pool, slot, &to_revoke);
            write_table(&self.dir, &table)?;
            drop(table_lock);
            let lease = Arc::new(Lease {
                pool,
                slot,
                slots: self.slots,
                primary: primary.clone(),
                dir: self.dir.clone(),
                noted: Mutex::new(roots.iter().map(|r| key(r)).collect()),
                settle: Settle::new(if to_revoke.is_empty() {
                    Settled::Ready
                } else {
                    Settled::Pending
                }),
                table_wait: self.table_wait,
                _lock: lock,
            });
            live.insert((pool, key(primary)), Arc::downgrade(&lease));
            drop(live);
            if to_revoke.is_empty() {
                return Ok(Some((lease, Taken::Fresh)));
            }
            match self.revoke_old(&lease, role, &to_revoke, revoke) {
                Ok(()) => return Ok(Some((lease, Taken::Fresh))),
                // The invariant: a slot loses its grant on every folder it
                // held before its new holder runs a command in the new
                // folder. So a candidate is taken only when every revoke
                // succeeded. One whose revoke fails is let go, with its table
                // entry as it was, and the next candidate is tried; before
                // this, that one bad path (a junction, a volume without
                // access lists, a folder whose list this user cannot write)
                // refused every folder that reached the slot, for as long as
                // the path existed.
                Err(why @ SlotError::Revoke { .. }) => {
                    // Dropping the lease frees the slot's lock file for
                    // another process. A holder of the same folder in this
                    // process, if there is one, already reads the lease as
                    // failed ([`Settling`]) and starts over.
                    drop(lease);
                    self.put_back(pool, slot, &held_before)?;
                    tried.push(slot);
                    stuck = stuck.or(Some(why));
                }
                // Not a revoke: the table write or a lock after the revokes.
                // Nothing to try on another slot.
                Err(other) => return Err(other),
            }
        }
    }

    /// Put back what `slot` of `pool` held, where the move to another folder
    /// failed its revokes. The entry keeps the place it had in the order, so
    /// the slot is still the least recently taken candidate it was.
    ///
    /// A folder whose revoke did succeed before the failure stays listed,
    /// which costs one revoke that finds nothing. The other way round, a
    /// folder dropped from the list while its grant lives, would leave a user
    /// with write access to a folder nothing will ever revoke.
    fn put_back(&self, pool: Pool, slot: u8, held: &[PathBuf]) -> Result<(), SlotError> {
        let _table_lock = lock_table(&self.dir, self.table_wait)?;
        let mut table = read_table(&self.dir)?;
        table.set_roots(pool, slot, held);
        write_table(&self.dir, &table)
    }

    /// Revoke `old` for the user of a fresh lease, then take the folders off
    /// the slot's table entry. No lock of this module is held while the
    /// revokes run; each revoke takes the access-list lock for its own write.
    fn revoke_old(
        &self,
        lease: &Lease,
        role: Role,
        old: &[PathBuf],
        revoke: Revoke<'_>,
    ) -> Result<(), SlotError> {
        let settling = Settling(&lease.settle, false);
        for path in old {
            revoke(role, path).map_err(|why| SlotError::Revoke {
                role,
                path: path.clone(),
                why,
            })?;
        }
        let _table_lock = lock_table(&self.dir, self.table_wait)?;
        let mut table = read_table(&self.dir)?;
        table.drop_roots(lease.pool, lease.slot, old);
        write_table(&self.dir, &table)?;
        settling.ready();
        Ok(())
    }

    /// [`Self::lease`], waiting up to `wait` while every slot is held for
    /// another folder. Any other failure returns at once.
    pub fn lease_waiting(
        &self,
        pool: Pool,
        roots: &[PathBuf],
        revoke: Revoke<'_>,
        wait: Duration,
    ) -> Result<(Arc<Lease>, Taken), SlotError> {
        let deadline = kgraf_core::waits::deadline_after(wait);
        loop {
            match self.lease(pool, roots, &mut *revoke) {
                Err(SlotError::AllHeld { .. }) if Instant::now() < deadline => {
                    std::thread::sleep(
                        RETRY.min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                other => return other,
            }
        }
    }

    /// The slot this process holds for the folder `root`, if any.
    pub fn held(&self, pool: Pool, root: &Path) -> Option<Arc<Lease>> {
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        live.get(&(pool, key(root))).and_then(Weak::upgrade)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never(_: Role, _: &Path) -> Result<(), String> {
        panic!("nothing is revoked here")
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        slots: PathBuf,
        work: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let slots = dir.path().join("slots");
            let work = dir.path().join("work");
            std::fs::create_dir(&work).unwrap();
            Self {
                _dir: dir,
                slots,
                work,
            }
        }

        /// A folder that exists, as a worktree does.
        fn tree(&self, name: &str) -> PathBuf {
            let p = self.work.join(name);
            std::fs::create_dir_all(&p).unwrap();
            p
        }
    }

    #[test]
    fn two_folders_at_the_same_time_get_different_users() {
        let f = Fixture::new();
        // Two processes: each has its own `Leases`, as each process does.
        let a = Leases::new(f.slots.clone(), 4);
        let b = Leases::new(f.slots.clone(), 4);
        let mut seen = Vec::new();
        let mut held = Vec::new();
        for (i, leases) in [&a, &b, &a, &b].into_iter().enumerate() {
            let root = f.tree(&format!("tree-{i}"));
            let (lease, taken) = leases.lease(Pool::Offline, &[root], &mut never).unwrap();
            assert_eq!(taken, Taken::Fresh);
            assert!(matches!(lease.role(), Role::Slot(_)));
            assert!(!seen.contains(&lease.role()), "{:?} twice", lease.role());
            seen.push(lease.role());
            held.push(lease);
        }
        // Even where the table points a new folder at a held slot (a table
        // written by another process, or left by one that stopped), the
        // held slot's lock keeps the new folder off it.
        let late = f.tree("late");
        point_at(&f, Pool::Offline, held[0].slot(), &late);
        let (lease, _) = Leases::new(f.slots.clone(), 5)
            .lease(Pool::Offline, &[late], &mut |_: Role, _: &Path| Ok(()))
            .unwrap();
        assert_eq!(lease.role(), Role::Slot(4), "took a held slot");
    }

    /// Rewrite the table so slot `slot` of `pool` is bound to `root`.
    fn point_at(f: &Fixture, pool: Pool, slot: u8, root: &Path) {
        let mut table = read_table(&f.slots).unwrap();
        table
            .bindings
            .retain(|b| !(b.pool == pool && b.slot == slot));
        table.bindings.insert(
            0,
            Binding {
                pool,
                slot,
                roots: vec![root.to_path_buf()],
            },
        );
        write_table(&f.slots, &table).unwrap();
    }

    #[test]
    fn one_folder_in_one_process_shares_its_slot() {
        let f = Fixture::new();
        let leases = Leases::new(f.slots.clone(), 4);
        let root = f.tree("tree");
        let (first, _) = leases
            .lease(Pool::Offline, std::slice::from_ref(&root), &mut never)
            .unwrap();
        let scratch = root.join(".scratch");
        let (second, taken) = leases
            .lease(Pool::Offline, &[root.clone(), scratch.clone()], &mut never)
            .unwrap();
        assert_eq!(taken, Taken::Shared);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(
            &leases.held(Pool::Offline, &root).unwrap(),
            &first
        ));
        // The extra folder is recorded, so it is revoked when the slot moves.
        let table = read_table(&f.slots).unwrap();
        let roots = &table.binding(Pool::Offline, first.slot()).unwrap().roots;
        assert_eq!(roots, &vec![root, scratch]);
    }

    #[test]
    fn more_folders_than_users_is_refused_not_shared() {
        let f = Fixture::new();
        let leases = Leases::new(f.slots.clone(), 2);
        let a = leases
            .lease(Pool::Offline, &[f.tree("a")], &mut never)
            .unwrap()
            .0;
        let b = leases
            .lease(Pool::Offline, &[f.tree("b")], &mut never)
            .unwrap()
            .0;
        assert_ne!(a.role(), b.role());
        let err = leases
            .lease(Pool::Offline, &[f.tree("c")], &mut never)
            .unwrap_err();
        assert!(matches!(err, SlotError::AllHeld { size: 2, .. }), "{err}");
        let text = err.to_string();
        assert!(text.contains("never run as one sandbox user"), "{text}");
        assert!(text.contains("--slots"), "{text}");
        // Another process sees the same: the lock files are what is held.
        let other = Leases::new(f.slots.clone(), 2);
        assert!(matches!(
            other.lease(Pool::Offline, &[f.tree("c")], &mut never),
            Err(SlotError::AllHeld { .. })
        ));
    }

    #[test]
    fn a_command_waits_for_a_slot_that_frees_and_is_refused_when_none_does() {
        let f = Fixture::new();
        let leases = Leases::new(f.slots.clone(), 1);
        let held = leases
            .lease(Pool::Offline, &[f.tree("held")], &mut never)
            .unwrap()
            .0;
        let other = f.tree("other");
        // Nobody lets go: refused once the wait is over.
        let started = Instant::now();
        let err = leases
            .lease_waiting(
                Pool::Offline,
                std::slice::from_ref(&other),
                &mut |_: Role, _: &Path| Ok(()),
                Duration::from_millis(300),
            )
            .unwrap_err();
        assert!(matches!(err, SlotError::AllHeld { .. }), "{err}");
        assert!(started.elapsed() >= Duration::from_millis(300));
        // The holder ends while the command waits: the command gets the slot.
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(held);
        });
        let (lease, _) = leases
            .lease_waiting(
                Pool::Offline,
                &[other],
                &mut |_: Role, _: &Path| Ok(()),
                Duration::from_secs(10),
            )
            .unwrap();
        release.join().unwrap();
        assert_eq!(lease.role(), Role::Slot(0));
    }

    #[test]
    fn a_command_without_a_worker_slot_never_takes_a_held_user() {
        // A setup without slots has one offline user. A command in another
        // folder while it is held is refused; before leases, both ran as
        // that user with no message.
        let f = Fixture::new();
        let leases = Leases::new(f.slots.clone(), 0);
        let worker = leases
            .lease(Pool::Offline, &[f.tree("worker")], &mut never)
            .unwrap()
            .0;
        assert_eq!(worker.role(), Role::Offline);
        let err = Leases::new(f.slots.clone(), 0)
            .lease(Pool::Offline, &[f.tree("command")], &mut never)
            .unwrap_err();
        assert!(matches!(err, SlotError::AllHeld { size: 1, .. }), "{err}");
        drop(worker);
    }

    #[test]
    fn two_installs_at_the_same_time_get_different_online_users() {
        let f = Fixture::new();
        let stores = f.tree("stores");
        let leases = Leases::new(f.slots.clone(), 4);
        let a = leases
            .lease(
                Pool::Online,
                &[f.tree("checkout-a"), stores.clone()],
                &mut never,
            )
            .unwrap()
            .0;
        // The table points the second install at the first one's slot: its
        // lock keeps the second off it all the same.
        let checkout_b = f.tree("checkout-b");
        point_at(&f, Pool::Online, a.slot(), &checkout_b);
        let b = Leases::new(f.slots.clone(), 4)
            .lease(
                Pool::Online,
                &[checkout_b, stores.clone()],
                &mut |_: Role, _: &Path| Ok(()),
            )
            .unwrap()
            .0;
        assert!(matches!(a.role(), Role::OnlineSlot(_)));
        assert!(matches!(b.role(), Role::OnlineSlot(_)));
        assert_ne!(a.role(), b.role());
        // The pools are apart: an offline command takes slot users of its
        // own, never an online one.
        let off = leases
            .lease(Pool::Offline, &[f.tree("checkout-a")], &mut never)
            .unwrap()
            .0;
        assert!(matches!(off.role(), Role::Slot(_)));
    }

    #[test]
    fn a_slot_that_moves_to_another_folder_loses_its_grants_first() {
        let f = Fixture::new();
        let leases = Leases::new(f.slots.clone(), 1);
        let stores = f.tree("stores");
        let first = f.tree("first");
        let a = leases
            .lease(Pool::Online, &[first.clone(), stores.clone()], &mut never)
            .unwrap()
            .0;
        let role = a.role();
        drop(a);
        // The same folder takes its slot again and nothing is revoked.
        let again = leases
            .lease(Pool::Online, std::slice::from_ref(&first), &mut never)
            .unwrap()
            .0;
        assert_eq!(again.role(), role);
        drop(again);
        // Another folder: the old folder's grant goes, the shared store
        // stays, because the new folder needs it too.
        let mut revoked = Vec::new();
        let second = f.tree("second");
        let b = leases
            .lease(
                Pool::Online,
                &[second.clone(), stores.clone()],
                &mut |r: Role, p: &Path| {
                    revoked.push((r, p.to_path_buf()));
                    Ok(())
                },
            )
            .unwrap()
            .0;
        assert_eq!(b.role(), role);
        assert_eq!(revoked, vec![(role, first.clone())]);
        let table = read_table(&f.slots).unwrap();
        assert_eq!(
            table.binding(Pool::Online, b.slot()).unwrap().roots,
            vec![second.clone(), stores.clone()]
        );
        drop(b);
        // A failed revoke refuses the slot. The table keeps listing the
        // folders that were not revoked, so the next holder revokes them.
        let err = leases
            .lease(
                Pool::Online,
                std::slice::from_ref(&first),
                &mut |_: Role, _: &Path| Err("access denied".to_owned()),
            )
            .unwrap_err();
        assert!(matches!(err, SlotError::Revoke { .. }), "{err}");
        let table = read_table(&f.slots).unwrap();
        let listed = &table.binding(Pool::Online, 0).unwrap().roots;
        assert!(
            listed.contains(&second) && listed.contains(&stores),
            "{listed:?}"
        );
        // The next holder of the slot revokes them again.
        let mut revoked = Vec::new();
        let c = leases
            .lease(Pool::Online, &[first], &mut |_: Role, p: &Path| {
                revoked.push(p.to_path_buf());
                Ok(())
            })
            .unwrap()
            .0;
        assert!(
            revoked.contains(&second) && revoked.contains(&stores),
            "{revoked:?}"
        );
        drop(c);
    }

    /// A revoke that does not end, run on its own thread. `release` ends it.
    fn slow_revoke_thread(
        leases: Arc<Leases>,
        root: PathBuf,
        started: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> std::thread::JoinHandle<Arc<Lease>> {
        std::thread::spawn(move || {
            let mut first = true;
            leases
                .lease(Pool::Offline, &[root], &mut |_: Role, _: &Path| {
                    if std::mem::take(&mut first) {
                        started.send(()).unwrap();
                        let _ = release.recv_timeout(Duration::from_secs(30));
                    }
                    Ok(())
                })
                .unwrap()
                .0
        })
    }

    /// Run `work` on a thread and fail, not hang, if it does not end.
    fn within<T: Send + 'static>(what: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        rx.recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("{what} did not end within 10 s"))
    }

    #[test]
    fn a_slow_revoke_blocks_no_other_lease_and_a_second_holder_gets_a_bounded_wait() {
        let f = Fixture::new();
        let leases = Arc::new(
            Leases::new(f.slots.clone(), 2)
                .with_waits(Duration::from_secs(5), Duration::from_millis(300)),
        );
        // Both slots have held another folder, so the next lease revokes.
        let old = f.tree("old");
        for _ in 0..2 {
            let held: Vec<_> = (0..2)
                .map(|i| {
                    leases
                        .lease(
                            Pool::Offline,
                            &[f.tree(&format!("old{i}")), old.clone()],
                            &mut never,
                        )
                        .unwrap()
                        .0
                })
                .collect();
            drop(held);
        }
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let x = f.tree("x");
        let slow = slow_revoke_thread(leases.clone(), x.clone(), started_tx, release_rx);
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("revoke started");
        // The table lock is free while the revoke runs.
        let dir = f.slots.clone();
        within("taking the table lock during a revoke", move || {
            drop(lock_table(&dir, Duration::from_millis(500)).expect("table lock free"))
        });
        // A lease for another folder does not wait for the revoke.
        let y = f.tree("y");
        let l2 = leases.clone();
        let other = within("a lease for another folder", move || {
            l2.lease(Pool::Offline, &[y], &mut |_: Role, _: &Path| Ok(()))
        });
        assert!(other.is_ok(), "{:?}", other.err());
        // A second holder of the same folder waits a bounded time, and the
        // wait is named in the record while it lasts.
        let l3 = leases.clone();
        let x2 = x.clone();
        let second = std::thread::spawn(move || l3.lease(Pool::Offline, &[x2], &mut never));
        std::thread::sleep(Duration::from_millis(100));
        let text = kgraf_core::waits::report();
        assert!(
            text.contains("the revokes of sandbox slot offline-"),
            "{text}"
        );
        let err = within("the second holder", move || second.join().unwrap()).unwrap_err();
        assert!(matches!(err, SlotError::Wait(_)), "{err}");
        // When the revoke ends the folder is shared.
        release_tx.send(()).unwrap();
        let first = slow.join().unwrap();
        let (again, taken) = leases.lease(Pool::Offline, &[x], &mut never).unwrap();
        assert_eq!(taken, Taken::Shared);
        assert!(Arc::ptr_eq(&first, &again));
    }

    #[test]
    fn a_held_table_lock_is_a_named_bounded_wait() {
        let f = Fixture::new();
        let leases = Arc::new(
            Leases::new(f.slots.clone(), 2)
                .with_waits(Duration::from_millis(300), Duration::from_secs(5)),
        );
        let holder = lock_table(&f.slots, Duration::from_secs(5)).unwrap();
        let root = f.tree("tree");
        let l = leases.clone();
        let t = std::thread::spawn(move || l.lease(Pool::Offline, &[root], &mut never));
        std::thread::sleep(Duration::from_millis(100));
        let text = kgraf_core::waits::report();
        assert!(text.contains("the sandbox slot table lock"), "{text}");
        let err = within("a lease with the table lock held", move || {
            t.join().unwrap()
        })
        .unwrap_err();
        assert!(matches!(err, SlotError::Wait(_)), "{err}");
        drop(holder);
    }

    /// A folder whose grant cannot be revoked no longer wedges its slot: the
    /// next candidate is tried, and the failure is reported only when every
    /// candidate fails.
    #[test]
    fn a_slot_whose_grant_cannot_be_revoked_is_left_alone_and_the_next_is_tried() {
        let f = Fixture::new();
        let leases = Leases::new(f.slots.clone(), 2);
        // Both slots are bound, slot 0 to a folder whose grant can never be
        // revoked: a junction, a volume without access lists, or a list this
        // user cannot write.
        let bad = f.tree("bad");
        let good = f.tree("good");
        for root in [&bad, &good] {
            drop(
                leases
                    .lease(Pool::Offline, std::slice::from_ref(root), &mut never)
                    .unwrap()
                    .0,
            );
        }
        let tried = std::cell::RefCell::new(Vec::new());
        let mut refuse_bad = |_: Role, p: &Path| {
            tried.borrow_mut().push(p.to_path_buf());
            if p == bad {
                Err("access denied".to_owned())
            } else {
                Ok(())
            }
        };
        let next = f.tree("next");
        let lease = leases
            .lease(Pool::Offline, std::slice::from_ref(&next), &mut refuse_bad)
            .unwrap()
            .0;
        // Slot 0 was offered first (the least recently taken), refused, and
        // left bound to `bad`; slot 1 took the folder.
        assert_eq!(*tried.borrow(), vec![bad.clone(), good.clone()]);
        assert_eq!(lease.slot(), 1);
        let table = read_table(&f.slots).unwrap();
        assert_eq!(
            table.binding(Pool::Offline, 0).unwrap().roots,
            vec![bad.clone()]
        );
        assert_eq!(
            table.binding(Pool::Offline, 1).unwrap().roots,
            vec![next.clone()]
        );
        drop(lease);
        // With every candidate refused, the failure is the answer, and it
        // names the first folder that could not be revoked.
        let err = leases
            .lease(
                Pool::Offline,
                &[f.tree("third")],
                &mut |_: Role, _: &Path| Err("access denied".to_owned()),
            )
            .unwrap_err();
        match &err {
            SlotError::Revoke { path, .. } => assert_eq!(path, &bad),
            other => panic!("{other}"),
        }
        // Nothing moved: both slots keep what they held.
        let table = read_table(&f.slots).unwrap();
        assert_eq!(table.binding(Pool::Offline, 0).unwrap().roots, vec![bad]);
        assert_eq!(table.binding(Pool::Offline, 1).unwrap().roots, vec![next]);
    }

    #[test]
    fn a_free_slot_bound_to_nothing_or_to_a_gone_folder_is_taken_before_a_busy_one() {
        let mut table = Table::default();
        let gone = PathBuf::from(if cfg!(windows) { r"C:\gone" } else { "/gone" });
        let here = PathBuf::from(if cfg!(windows) { r"C:\here" } else { "/here" });
        let mine = PathBuf::from(if cfg!(windows) { r"C:\Mine" } else { "/Mine" });
        table.bind(Pool::Offline, 0, std::slice::from_ref(&here));
        table.bind(Pool::Offline, 1, std::slice::from_ref(&gone));
        table.bind(Pool::Offline, 2, std::slice::from_ref(&mine));
        let exists = |p: &Path| p != gone.as_path();
        let lower = PathBuf::from(mine.to_string_lossy().to_lowercase());
        assert_eq!(
            table.candidates(Pool::Offline, 4, &lower, &exists),
            vec![2, 3, 1, 0],
            "own folder, unbound, gone, then the least recently taken"
        );
        // Slots beyond the pool's size are never offered.
        assert_eq!(
            table.candidates(Pool::Offline, 2, &lower, &exists),
            vec![1, 0]
        );
        // The other pool is apart.
        assert_eq!(
            table.candidates(Pool::Online, 2, &lower, &exists),
            vec![0, 1]
        );
    }

    #[test]
    fn the_table_round_trips_and_a_damaged_one_is_refused() {
        let root = |s: &str| {
            PathBuf::from(if cfg!(windows) {
                format!(r"C:\{s}")
            } else {
                format!("/{s}")
            })
        };
        let mut table = Table::default();
        table.bind(Pool::Offline, 3, &[root("a b"), root("a b/.scratch")]);
        table.bind(Pool::Online, 0, &[root("c")]);
        let text = table.to_text();
        assert_eq!(Table::parse(&text).unwrap(), table);
        assert_eq!(table.all_roots().len(), 3);
        for bad in [
            "offline 3 /a\n",
            "elsewhere 03 /a\n",
            "offline 03 relative\n",
            "offline 03\n",
        ] {
            assert!(Table::parse(bad).is_err(), "{bad:?}");
        }
    }

    /// One lock covers every access-list read-modify-write, so two of them
    /// never read one folder's list and each write back a list that holds
    /// only its own entry.
    #[test]
    fn the_access_list_lock_shuts_out_another_holder_and_lets_its_own_thread_in_again() {
        let f = Fixture::new();
        let bound = Duration::from_millis(200);
        let outer = lock_acls_within(&f.slots, bound).unwrap();
        // Another holder of the lock, a second process in the field, waits
        // its bound and then says what it waited for.
        let dir = f.slots.clone();
        let other = std::thread::spawn(move || lock_acls_within(&dir, bound));
        let err = other.join().unwrap().unwrap_err();
        assert!(matches!(err, SlotError::Lock { .. }), "{err}");
        let text = err.to_string();
        assert!(text.contains("access-list lock"), "{text}");
        // The thread that holds it takes it again: a revoke inside a lease
        // writes access lists while the lease holds the lock.
        let started = Instant::now();
        let nested = lock_acls_within(&f.slots, bound).unwrap();
        assert!(started.elapsed() < bound, "the nested take waited");
        drop(nested);
        // Still held by the outer guard.
        let dir = f.slots.clone();
        assert!(
            std::thread::spawn(move || lock_acls_within(&dir, bound))
                .join()
                .unwrap()
                .is_err()
        );
        drop(outer);
        let dir = f.slots.clone();
        std::thread::spawn(move || lock_acls_within(&dir, bound))
            .join()
            .unwrap()
            .expect("free once every guard is dropped");
    }

    /// A revoke a lease runs is covered by the access-list lock, and the
    /// lease itself holds that lock at no point: each revoke takes it for the
    /// one list it writes (the Windows backend's `revoke_sandbox_grant`
    /// does), and shuts another holder out only for that write. A take around
    /// the whole revoke loop would block every other folder's lease for the
    /// length of a tree write, which [`Leases::lease`] exists to prevent.
    #[test]
    fn each_revoke_of_a_lease_holds_the_access_list_lock_and_the_lease_holds_it_at_no_point() {
        let f = Fixture::new();
        let leases = Leases::new(f.slots.clone(), 1);
        let first = f.tree("first");
        drop(
            leases
                .lease(Pool::Online, std::slice::from_ref(&first), &mut never)
                .unwrap()
                .0,
        );
        let mut revoked = 0;
        let dir = f.slots.clone();
        let take_elsewhere = |dir: &Path| {
            let other = dir.to_path_buf();
            std::thread::spawn(move || lock_acls_within(&other, Duration::from_millis(50)))
                .join()
                .unwrap()
        };
        let lease = leases
            .lease(
                Pool::Online,
                &[f.tree("second")],
                &mut |_: Role, _: &Path| {
                    revoked += 1;
                    // The lease holds nothing of its own: a revoke of another
                    // folder, in another process, runs while this one does.
                    take_elsewhere(&dir).expect("the lease holds no access-list lock");
                    // The revoke's own write of one list takes it, and
                    // another holder waits while that write lasts.
                    let held = lock_acls_within(&dir, Duration::from_millis(50))
                        .expect("the revoke takes the lock for its own write");
                    assert!(
                        take_elsewhere(&dir).is_err(),
                        "the revoke's write is not covered"
                    );
                    // Reentrant, so a write inside that write does not wait
                    // for its own caller.
                    let nested = lock_acls_within(&dir, Duration::from_millis(50))
                        .expect("reentrant within the thread");
                    drop(nested);
                    assert!(
                        take_elsewhere(&dir).is_err(),
                        "the outer guard still holds it"
                    );
                    drop(held);
                    Ok(())
                },
            )
            .unwrap()
            .0;
        assert_eq!(revoked, 1);
        // Free once the revokes are done, as it was while they ran.
        lock_acls_within(&f.slots, Duration::from_millis(50)).expect("free once the slot is taken");
        drop(lease);
    }

    /// A removal must revoke what it can read: one damaged line (a write cut
    /// short by a power loss) no longer hides every other folder, which
    /// before this kept the users, the firewall rules and the DNS filter on
    /// the machine for good.
    #[test]
    fn a_damaged_table_still_names_the_folders_a_removal_can_revoke() {
        let root = |s: &str| {
            PathBuf::from(if cfg!(windows) {
                format!(r"C:\{s}")
            } else {
                format!("/{s}")
            })
        };
        let text = format!(
            "offline 00 {}\nnonsense\noffline 00 {}\nonline 01 {}\n\noffline 3 {}\n",
            root("a").display(),
            root("a").display(),
            root("b").display(),
            root("c").display()
        );
        let (roots, damaged) = roots_in_text(&text);
        assert_eq!(roots, vec![root("a"), root("b")], "each folder once");
        assert_eq!(damaged, vec![2, 6], "the numbers of the damaged lines");
        // The lease path still refuses the whole table: a slot whose folders
        // it cannot read is a slot whose grants it cannot revoke.
        let err = Table::parse(&text).unwrap_err();
        assert_eq!(err.line, 2);
    }

    /// A written table leaves nothing half-written behind, and a table that
    /// cannot be read says what to do about it.
    #[test]
    fn a_written_table_replaces_the_last_one_and_a_damaged_one_says_how_to_recover() {
        let f = Fixture::new();
        let leases = Leases::new(f.slots.clone(), 1);
        let lease = leases
            .lease(Pool::Offline, &[f.tree("tree")], &mut never)
            .unwrap()
            .0;
        drop(lease);
        assert!(!f.slots.join("table.tmp").exists(), "a leftover temporary");
        assert!(f.slots.join("table.txt").exists());
        // What a power loss between the write and the rename could leave: a
        // table of zero length reads as a setup that holds no folder, so the
        // grants of every folder it listed would never be revoked. The write
        // is flushed to the device before the rename for that reason.
        std::fs::write(f.slots.join("table.txt"), "nonsense\n").unwrap();
        let err = read_table(&f.slots).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("kgraf sandbox remove"), "{text}");
        assert!(text.contains("kgraf sandbox setup"), "{text}");
    }

    /// A folder the table cannot hold exactly is refused, rather than
    /// written in another spelling, whose revoke would later find nothing
    /// and skip without a word. A line break in a folder name would make a
    /// line of its own, which the table would read back as another folder.
    #[test]
    fn a_folder_the_table_cannot_hold_exactly_is_refused() {
        let f = Fixture::new();
        let leases = Leases::new(f.slots.clone(), 2);
        let two_lines = f.work.join("one\ntwo");
        let err = leases
            .lease(Pool::Offline, std::slice::from_ref(&two_lines), &mut never)
            .unwrap_err();
        assert!(matches!(err, SlotError::PathNotText { .. }), "{err}");
        let text = err.to_string();
        assert!(text.contains("line break"), "{text}");
        // A folder taken for its own line is refused when a later command
        // names a second root the table cannot hold, so the grant on it is
        // never written without a record of it.
        let good = f.tree("good");
        let lease = leases
            .lease(Pool::Offline, std::slice::from_ref(&good), &mut never)
            .unwrap()
            .0;
        let err = lease.note_roots(&[two_lines]).unwrap_err();
        assert!(matches!(err, SlotError::PathNotText { .. }), "{err}");
        // What a lossy path would do, in the one case a host can make: the
        // round trip of every other folder is exact.
        assert!(is_text(&good));
    }

    /// The refusal names the folders the slots were last taken for, which
    /// a lock file cannot say.
    #[test]
    fn the_refusal_names_the_folders_the_slots_were_taken_for() {
        let f = Fixture::new();
        let leases = Leases::new(f.slots.clone(), 1);
        let held = f.tree("held");
        let lease = leases
            .lease(Pool::Offline, std::slice::from_ref(&held), &mut never)
            .unwrap()
            .0;
        let err = Leases::new(f.slots.clone(), 1)
            .lease(Pool::Offline, &[f.tree("other")], &mut never)
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("last taken for"), "{text}");
        assert!(text.contains(&held.display().to_string()), "{text}");
        drop(lease);
    }

    /// A lock file whose folder went away is opened again: `kgraf sandbox
    /// remove` deletes the slot folder, and it can do that while a holder
    /// lives. Before this, the open failed and the command with it.
    #[test]
    fn a_lock_file_is_opened_again_after_its_folder_went_away() {
        let f = Fixture::new();
        let gone = f.slots.join("gone").join("offline-00.lock");
        open_lock_file(&gone).expect("the folder is made again");
        assert!(gone.exists());
        // The whole path holds up too: a lease after the folder was deleted
        // under a live holder still gets a slot. The slot number can repeat,
        // because the deleted lock file is not the one a new open makes.
        let leases = Leases::new(f.slots.clone(), 2);
        let lease = leases
            .lease(Pool::Offline, &[f.tree("first")], &mut never)
            .unwrap()
            .0;
        std::fs::remove_dir_all(&f.slots).unwrap();
        leases
            .lease(Pool::Offline, &[f.tree("second")], &mut never)
            .expect("a slot after the folder was deleted");
        drop(lease);
    }

    #[test]
    fn pool_roles_follow_the_setup() {
        assert_eq!(Pool::Offline.role(0, 0), Role::Offline);
        assert_eq!(Pool::Online.role(0, 0), Role::Online);
        assert_eq!(Pool::Offline.role(2, 4), Role::Slot(2));
        assert_eq!(Pool::Online.role(2, 4), Role::OnlineSlot(2));
        assert_eq!(Pool::size(0), 1);
        assert_eq!(Pool::size(4), 4);
    }
}
