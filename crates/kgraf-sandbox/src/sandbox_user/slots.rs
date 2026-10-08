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
//! This replaces a stable fold of the folder's path to a slot, which put two
//! folders on one user, with no message, whenever their paths folded alike.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use kgraf_core::flock::HeldLock;

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
            let bad = || TableError {
                line: n + 1,
                text: line.to_owned(),
            };
            let mut parts = line.splitn(3, ' ');
            let pool = parts.next().and_then(Pool::from_word).ok_or_else(bad)?;
            let slot: u8 = parts
                .next()
                .filter(|s| s.len() == 2)
                .and_then(|s| s.parse().ok())
                .ok_or_else(bad)?;
            let root = PathBuf::from(parts.next().ok_or_else(bad)?);
            if !root.is_absolute() {
                return Err(bad());
            }
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
         so a command in {} has no user of its own. Two commands in different folders never \
         run as one sandbox user. Wait for one to finish, or run `kgraf sandbox setup --slots \
         N` with a larger N",
        root.display()
    )]
    AllHeld {
        pool: &'static str,
        size: u8,
        root: PathBuf,
    },
    #[error("a sandboxed command has no writable folder to take a worker slot for")]
    NoRoot,
    #[error("the sandbox slot table {}: {source}", path.display())]
    Table {
        path: PathBuf,
        #[source]
        source: TableError,
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
    _lock: HeldLock,
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
        let _table_lock = lock_table(&self.dir)?;
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
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> SlotError + '_ {
    move |source| SlotError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn open_lock_file(path: &Path) -> Result<File, SlotError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(io_error(path))
}

/// The table's own lock, held while the table is read and written. Waits
/// for another process that holds it.
fn lock_table(dir: &Path) -> Result<HeldLock, SlotError> {
    std::fs::create_dir_all(dir).map_err(io_error(dir))?;
    let path = dir.join("table.lock");
    let file = open_lock_file(&path)?;
    file.lock().map_err(io_error(&path))?;
    Ok(HeldLock::new(file))
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

fn write_table(dir: &Path, table: &Table) -> Result<(), SlotError> {
    let path = table_path(dir);
    let tmp = dir.join("table.tmp");
    std::fs::write(&tmp, table.to_text()).map_err(io_error(&tmp))?;
    std::fs::rename(&tmp, &path).map_err(io_error(&path))
}

impl Leases {
    /// The slots of a setup with `slots` slots, whose table and lock files
    /// live in `dir`.
    pub fn new(dir: PathBuf, slots: u8) -> Self {
        Self {
            dir,
            slots,
            live: Mutex::new(HashMap::new()),
        }
    }

    pub fn slots(&self) -> u8 {
        self.slots
    }

    /// A slot of `pool` for the folder `roots[0]`, whose user is then
    /// granted write on every one of `roots`. Shared with a holder of the
    /// same folder in this process; never with a holder of another folder,
    /// in this process or another. Refused when every slot of the pool is
    /// held for another folder. A slot that held other folders loses its
    /// grants there first, through `revoke`.
    pub fn lease(
        &self,
        pool: Pool,
        roots: &[PathBuf],
        revoke: Revoke<'_>,
    ) -> Result<(Arc<Lease>, Taken), SlotError> {
        let primary = roots.first().ok_or(SlotError::NoRoot)?;
        // Held while a slot is chosen, so two threads of this process do not
        // both take a slot for one folder.
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        live.retain(|_, held| held.strong_count() > 0);
        if let Some(held) = live.get(&(pool, key(primary))).and_then(Weak::upgrade) {
            held.note_roots(roots)?;
            return Ok((held, Taken::Shared));
        }
        let table_lock = lock_table(&self.dir)?;
        let mut table = read_table(&self.dir)?;
        let size = Pool::size(self.slots);
        let mut chosen = None;
        for slot in table.candidates(pool, size, primary, &|p: &Path| p.exists()) {
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
            return Err(SlotError::AllHeld {
                pool: pool.word(),
                size,
                root: primary.clone(),
            });
        };
        let role = pool.role(slot, self.slots);
        for path in table.bind(pool, slot, roots) {
            revoke(role, &path).map_err(|why| SlotError::Revoke {
                role,
                path: path.clone(),
                why,
            })?;
        }
        write_table(&self.dir, &table)?;
        drop(table_lock);
        let lease = Arc::new(Lease {
            pool,
            slot,
            slots: self.slots,
            primary: primary.clone(),
            dir: self.dir.clone(),
            noted: Mutex::new(roots.iter().map(|r| key(r)).collect()),
            _lock: lock,
        });
        live.insert((pool, key(primary)), Arc::downgrade(&lease));
        Ok((lease, Taken::Fresh))
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
        let deadline = Instant::now() + wait;
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
            vec![second, stores]
        );
        drop(b);
        // A failed revoke refuses the slot and keeps the table as it was.
        let before = read_table(&f.slots).unwrap();
        let err = leases
            .lease(Pool::Online, &[first], &mut |_: Role, _: &Path| {
                Err("access denied".to_owned())
            })
            .unwrap_err();
        assert!(matches!(err, SlotError::Revoke { .. }), "{err}");
        assert_eq!(read_table(&f.slots).unwrap(), before);
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
