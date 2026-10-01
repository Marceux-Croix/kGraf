//! A restorable snapshot of a repository's object store (decision 0094):
//! every regular file under `objects/` (loose objects, packs with their
//! `.idx`, `.rev`, `.bitmap` and the rest, `info/*`), kept in a kGraf-owned
//! directory outside every sandbox writable root.
//!
//! **Method.** Git never changes an object file once written (it writes a
//! new file and renames it into place), so a hard link to each file is a
//! snapshot: it costs one directory entry per file and no data. Where a hard
//! link is impossible (the snapshot directory is on another file system, or
//! the file system has no hard links) the file is copied instead, at most
//! [`super::OBJECT_COPY_LIMIT`] bytes in all by default; a store whose copy
//! would pass the bound is not snapshotted, and the caller refuses the
//! protected write rather than make one it could not restore.
//!
//! **What is recorded.** Per file: its length, modification time and (Unix)
//! device and inode at capture. A file is *lost* when it is missing or any of
//! those differs now (removed, replaced, truncated or rewritten). New files
//! are never lost: objects are additive, and another worker's or kGraf's new
//! objects are never removed by a restore.
//!
//! **Verification before a restore.** A linked entry shares its file with the
//! store, so it is put back only if that shared file still has the recorded
//! length, time and inode; a file altered in place (git makes object files
//! read-only, so this takes a `chmod` first) altered the snapshot too and is
//! reported as not restorable. A copied entry is put back only if its BLAKE3
//! hash, taken while copying, still matches. Hashing every linked file at
//! capture was not done: it reads the whole store, which the link method
//! exists to avoid.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use kgraf_sandbox::{HostId, ProcessIdentity};

/// What identifies a file's state cheaply.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Meta {
    len: u64,
    mtime: Option<SystemTime>,
    /// Device and inode (Unix); `None` elsewhere.
    id: Option<(u64, u64)>,
}

fn meta_of(m: &std::fs::Metadata) -> Meta {
    #[cfg(unix)]
    let id = {
        use std::os::unix::fs::MetadataExt;
        Some((m.dev(), m.ino()))
    };
    #[cfg(not(unix))]
    let id = None;
    Meta {
        len: m.len(),
        mtime: m.modified().ok(),
        id,
    }
}

#[derive(Clone, Debug)]
struct Entry {
    /// The store file's state at capture (and after a restore).
    meta: Meta,
    /// `Some` for a copied entry: the copy's BLAKE3 hash.
    hash: Option<blake3::Hash>,
}

/// How a snapshot holds its files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// Hard links; the default when the file system allows them.
    HardLink,
    /// Byte copies, bounded; when a hard link failed (or a test forces it).
    Copy,
}

/// One attempt's object-store snapshot.
#[derive(Debug)]
pub(crate) struct ObjectSnapshot {
    objects: PathBuf,
    dir: PathBuf,
    method: Method,
    entries: BTreeMap<PathBuf, Entry>,
    copied: u64,
    copy_limit: u64,
    /// Snapshot directories already made, so each file costs one link and
    /// not a `create_dir_all` too.
    made: std::collections::HashSet<PathBuf>,
}

impl ObjectSnapshot {
    /// Snapshot `objects` into `dir` (created; must not exist yet): the
    /// files under `dir/objects`, and `dir/owner` naming this process, so a
    /// snapshot a crashed process left is found and removed
    /// ([`sweep_abandoned`]). With `force_copy`, copies are made as if hard
    /// links were impossible.
    pub(crate) fn take(
        objects: &Path,
        dir: PathBuf,
        force_copy: bool,
        copy_limit: u64,
    ) -> Result<Self, String> {
        if let Some(base) = dir.parent() {
            sweep_abandoned(base);
        }
        std::fs::create_dir_all(dir.join(FILES))
            .map_err(|e| format!("cannot make the object snapshot {}: {e}", dir.display()))?;
        std::fs::write(dir.join(OWNER), owner_record(&ProcessIdentity::current()))
            .map_err(|e| format!("cannot write {}: {e}", dir.join(OWNER).display()))?;
        let mut snap = Self {
            objects: objects.to_path_buf(),
            dir,
            method: if force_copy {
                Method::Copy
            } else {
                Method::HardLink
            },
            entries: BTreeMap::new(),
            copied: 0,
            copy_limit,
            made: std::collections::HashSet::new(),
        };
        if let Err(e) = snap.sync() {
            snap.remove();
            return Err(e);
        }
        Ok(snap)
    }

    pub(crate) fn method(&self) -> Method {
        self.method
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where the snapshot keeps the store's file `rel`.
    pub(crate) fn snap(&self, rel: &Path) -> PathBuf {
        self.dir.join(FILES).join(rel)
    }

    /// Bring the snapshot to the store as it is now: capture new and changed
    /// files, forget files that are gone. Called while no one else writes the
    /// repository (under its exclusive lock), before a guarded command.
    pub(crate) fn sync(&mut self) -> Result<(), String> {
        let now = scan(&self.objects)?;
        let gone: Vec<PathBuf> = self
            .entries
            .keys()
            .filter(|k| !now.contains_key(*k))
            .cloned()
            .collect();
        for rel in gone {
            self.forget(&rel);
        }
        for (rel, meta) in now {
            if self.entries.get(&rel).is_some_and(|e| e.meta == meta) {
                continue;
            }
            self.forget(&rel);
            self.capture(&rel, meta)?;
        }
        Ok(())
    }

    fn forget(&mut self, rel: &Path) {
        if let Some(e) = self.entries.remove(rel) {
            if e.hash.is_some() {
                self.copied = self.copied.saturating_sub(e.meta.len);
            }
            let _ = std::fs::remove_file(self.snap(rel));
        }
    }

    fn capture(&mut self, rel: &Path, meta: Meta) -> Result<(), String> {
        let src = self.objects.join(rel);
        let dst = self.snap(rel);
        if let Some(parent) = dst.parent()
            && !self.made.contains(parent)
        {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            self.made.insert(parent.to_path_buf());
        }
        if self.method == Method::HardLink {
            let mut linked = std::fs::hard_link(&src, &dst);
            if matches!(&linked, Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists) {
                // Left by a removal that failed: replace it.
                let _ = std::fs::remove_file(&dst);
                linked = std::fs::hard_link(&src, &dst);
            }
            match linked {
                Ok(()) => {
                    // The link is the snapshot and shares the scanned file's
                    // inode, length and time.
                    self.entries
                        .insert(rel.to_path_buf(), Entry { meta, hash: None });
                    return Ok(());
                }
                // A file that vanished between the scan and the link is not
                // in the store any more: nothing to keep.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                // Another file system, or none with hard links: copy from
                // here on, inside the bound.
                Err(_) => self.method = Method::Copy,
            }
        }
        if self.copied + meta.len > self.copy_limit {
            return Err(format!(
                "the repository's objects cannot be hard-linked into {} (another file system?), \
                 and copying them would pass the {}-byte bound, so a git write could not be \
                 restored; it is not made",
                self.dir.display(),
                self.copy_limit
            ));
        }
        let hash = match copy_hashed(&src, &dst) {
            Ok(h) => h,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("cannot copy {}: {e}", src.display())),
        };
        self.copied += meta.len;
        self.entries.insert(
            rel.to_path_buf(),
            Entry {
                meta,
                hash: Some(hash),
            },
        );
        Ok(())
    }

    /// The snapshotted files that are missing from the store now, or not
    /// the file that was captured.
    pub(crate) fn lost(&self) -> Result<Vec<PathBuf>, String> {
        let now = scan(&self.objects)?;
        Ok(self
            .entries
            .iter()
            .filter(|(rel, e)| now.get(*rel) != Some(&e.meta))
            .map(|(rel, _)| rel.clone())
            .collect())
    }

    /// Put each of `lost` back from the snapshot, after verifying the
    /// snapshot's file. Nothing else in the store is touched. `Err` lists
    /// every file that could not be restored and why.
    pub(crate) fn restore(&mut self, lost: &[PathBuf]) -> Result<usize, String> {
        let mut failed = Vec::new();
        let mut restored = 0;
        for rel in lost {
            match self.restore_one(rel) {
                Ok(()) => restored += 1,
                Err(e) => failed.push(format!("objects/{}: {e}", rel.display())),
            }
        }
        if failed.is_empty() {
            Ok(restored)
        } else {
            Err(failed.join("; "))
        }
    }

    fn restore_one(&mut self, rel: &Path) -> Result<(), String> {
        let entry = self
            .entries
            .get(rel)
            .cloned()
            .ok_or_else(|| "not in the snapshot".to_owned())?;
        let snap = self.snap(rel);
        let path = self.objects.join(rel);
        match entry.hash {
            None => {
                let m = std::fs::symlink_metadata(&snap)
                    .map_err(|e| format!("the snapshot's link is gone: {e}"))?;
                if meta_of(&m) != entry.meta {
                    return Err(
                        "altered in place; the snapshot shares that file, so it cannot be \
                         restored"
                            .into(),
                    );
                }
                clear(&path)?;
                ensure_parent(&path)?;
                if std::fs::hard_link(&snap, &path).is_err() {
                    std::fs::copy(&snap, &path).map_err(|e| e.to_string())?;
                    let m = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
                    if let Some(e) = self.entries.get_mut(rel) {
                        e.meta = meta_of(&m);
                    }
                }
            }
            Some(hash) => {
                let found = hash_file(&snap).map_err(|e| format!("cannot read the copy: {e}"))?;
                if found != hash {
                    return Err("the snapshot's copy no longer matches its hash".into());
                }
                clear(&path)?;
                ensure_parent(&path)?;
                // Into place by rename, so no reader sees half a file.
                let tmp = path.with_extension("kgraf-restore");
                copy_hashed(&snap, &tmp).map_err(|e| e.to_string())?;
                std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
                let m = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
                if let Some(e) = self.entries.get_mut(rel) {
                    e.meta = meta_of(&m);
                }
            }
        }
        Ok(())
    }

    /// Delete the snapshot directory. The store's own files are untouched
    /// (a hard link's removal leaves the other name).
    pub(crate) fn remove(&mut self) {
        self.entries.clear();
        self.made.clear();
        self.copied = 0;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The snapshot's files, under its directory.
const FILES: &str = "objects";
/// The snapshot's owner record, beside them.
const OWNER: &str = "owner";

/// `pid=<p>:start=<t>:machine=..:boot=..:ns=..:tns=..`, `-` for an unknown
/// part: the fields a worktree lock records, judged by the same rule
/// ([`ProcessIdentity::is_alive`]).
fn owner_record(who: &ProcessIdentity) -> String {
    format!(
        "pid={}:start={}:{}\n",
        who.pid,
        who.start.map_or_else(|| "-".to_owned(), |s| s.to_string()),
        who.host.clone().unwrap_or_default().fields()
    )
}

fn parse_owner(text: &str) -> Option<ProcessIdentity> {
    let fields: Vec<&str> = text.trim().split(':').collect();
    let pid = fields
        .iter()
        .find_map(|f| f.strip_prefix("pid="))?
        .parse()
        .ok()?;
    let start = fields
        .iter()
        .find_map(|f| f.strip_prefix("start="))
        .and_then(|s| s.parse().ok());
    Some(ProcessIdentity {
        pid,
        start,
        host: HostId::parse_fields(fields.iter().copied()),
    })
}

/// Remove every snapshot under `base` whose owner process is gone (crashed
/// or killed before its attempt ended). One whose owner may still run, or
/// cannot be judged from here, or whose record does not read, is left.
pub(crate) fn sweep_abandoned(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(text) = std::fs::read_to_string(dir.join(OWNER)) else {
            continue;
        };
        if parse_owner(&text).is_some_and(|who| !who.is_alive()) {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// Every regular file under `objects`, by relative path. Links are not
/// followed.
fn scan(objects: &Path) -> Result<BTreeMap<PathBuf, Meta>, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = objects.join(&rel);
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
        };
        for entry in entries {
            let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
            let child = rel.join(entry.file_name());
            let m = match std::fs::symlink_metadata(entry.path()) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("cannot read {}: {e}", entry.path().display())),
            };
            if m.is_dir() {
                stack.push(child);
            } else if m.is_file() {
                out.insert(child, meta_of(&m));
            }
        }
    }
    Ok(out)
}

fn clear(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path).map_err(|e| e.to_string()),
        Ok(_) => std::fs::remove_file(path).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn ensure_parent(path: &Path) -> Result<(), String> {
    match path.parent() {
        Some(p) => std::fs::create_dir_all(p).map_err(|e| e.to_string()),
        None => Ok(()),
    }
}

/// Copy `src` to `dst` (a new file, the source's permissions) and return the
/// hash of what was written.
fn copy_hashed(src: &Path, dst: &Path) -> std::io::Result<blake3::Hash> {
    let mut input = std::fs::File::open(src)?;
    let perms = input.metadata()?.permissions();
    let _ = std::fs::remove_file(dst);
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        output.write_all(&buf[..n])?;
    }
    output.sync_all()?;
    drop(output);
    std::fs::set_permissions(dst, perms)?;
    Ok(hasher.finalize())
}

fn hash_file(path: &Path) -> std::io::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    let mut f = std::fs::File::open(path)?;
    std::io::copy(&mut f, &mut hasher)?;
    Ok(hasher.finalize())
}
