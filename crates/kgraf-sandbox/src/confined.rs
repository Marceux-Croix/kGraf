//! Removal inside a worktree, from the host, that never follows a symlink and
//! never leaves the worktree.
//!
//! The host runs outside the sandbox, so a plain `remove_file(root.join(rel))`
//! resolves every component of `rel` again at the time of the call. A process
//! a check left behind can swap a listed directory for a symlink to `$HOME`
//! between the listing and the removal, and the host would delete there what
//! the sandbox refuses the check itself. Here each directory on the way is
//! opened with no-follow relative to its parent's descriptor, and each entry
//! is unlinked relative to the descriptor of the directory that holds it. A
//! symlink is only ever removed itself. A component that is no longer a real
//! directory ends the walk for that path, with nothing removed: the caller
//! lists again and sees what is there now.
//!
//! **Depth.** The walk keeps one open directory per level on a heap stack,
//! never the call stack (a check can make a chain of folders thousands deep
//! inside a nested repository, which git lists as one entry; a recursive walk
//! overflowed the host's stack and killed the orchestrator). A
//! tree deeper than [`MAX_DEPTH`] is refused with an error, which fails the
//! verification: nothing legitimate needs it, and the bound also keeps the
//! walk's open descriptors far below the process limit.
//!
//! **Unix.** Descriptor-relative opens and unlinks are cap-primitives' (see
//! the dependency note in Cargo.toml): `openat` with `O_NOFOLLOW`.
//!
//! **Windows.** cap-primitives is not used there: its Windows backend can
//! rebuild a path string from the parent handle, and it checks for a link
//! only after an open that followed it. The `win` module opens each component
//! itself with `NtCreateFile`, one name at a time, relative to the handle of
//! the folder that holds it (`OBJECT_ATTRIBUTES.RootDirectory`), always with
//! `FILE_OPEN_REPARSE_POINT` and `OBJ_DONT_REPARSE`. So no symbolic link,
//! junction (mount point) or other reparse point is ever followed, not even
//! a held folder made one in place: the open gets the reparse point itself,
//! and its `FILE_ATTRIBUTE_REPARSE_POINT` attribute, read on the handle,
//! refuses it as a link is refused on Unix. A name that reached a file under
//! another name (a short 8.3 name) is refused too. Removal sets the delete
//! disposition on the handle it opened, never on a path, so a link is only
//! ever removed itself. The `win` module's notes give the details and the
//! exact guarantee.

use cap_primitives::fs::open_ambient_dir;
use cap_primitives::{AmbientAuthority, ambient_authority};
use std::fs::File;
use std::io;
use std::path::Path;

/// Deepest a removal walks: path components of a listed path, or folder
/// levels inside a listed directory.
pub(crate) const MAX_DEPTH: usize = 100;

/// What [`remove`] did with one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Removal {
    Removed,
    /// Nothing is at that path as a chain of real directories now (absent, or
    /// a component is a symlink or a file). Nothing was removed.
    Gone,
}

/// The worktree root, opened once. The root itself is a path kGraf created
/// and the sandbox cannot rename; only what is beneath it is untrusted.
pub(crate) fn open_root(root: &Path) -> io::Result<File> {
    let authority: AmbientAuthority = ambient_authority();
    open_ambient_dir(root, authority)
}

/// Remove `rel` beneath `root`: a file or a symlink is unlinked, a directory
/// is removed with everything in it (a nested repository, listed by git as
/// `dir/`). Then each parent directory the removal left empty is removed, up
/// to (never including) the root: tidiness only, as git tracks no directories.
/// `rel` must be relative with plain components only.
///
/// On Windows a junction or any other reparse point counts as a link: it is
/// never entered, and at the last component it is removed itself.
pub(crate) fn remove(root: &File, rel: &Path) -> io::Result<Removal> {
    #[cfg(not(windows))]
    {
        unix::remove(root, rel)
    }
    #[cfg(windows)]
    {
        win::remove(root, rel)
    }
}

/// Write `content` to the file `rel` beneath the directory `root`, from the
/// host, never following a symbolic link on the way.
///
/// This is the one helper for every host-side (unsandboxed) write into a tree
/// whose contents a worker or a user's repository decides: a worktree, a check
/// tree, the conversation worktree at the integration tip (decision 0083). The
/// worker file tools use `kgraf-tools`' `fs_safety` instead, which confines to
/// the workspace but follows a link that stays inside it; the host never
/// follows one.
///
/// Each directory component is opened with no-follow relative to its parent's
/// descriptor, and created when absent. A component that is a symbolic link,
/// the last one included, is **refused** with an error naming it: the tree
/// under `root` may come from a commit a worker made, and `tests ->
/// /home/you` or `src/config.rs -> ~/.zshrc` would otherwise send the write
/// outside the tree (decision
/// 0083). A link is never replaced either: whoever planted it answers for
/// it. `rel` must be relative with plain components. On Windows a junction
/// and any other reparse point are refused the same way, and so is a name
/// Windows programs could not open by path (a reserved device name, a
/// trailing dot or space, a stream suffix after `:`), and a name that
/// reaches a file or folder whose long name differs other than by case (a
/// short 8.3 name such as `GITATT~1`): the caller judged the name it passed,
/// not the one it would reach.
///
/// The file is opened without truncation and non-blocking, then checked on the
/// open descriptor: anything but a regular file (a FIFO would block the host
/// forever) and a regular file with more than one hard link (its other name
/// may be outside the tree) are refused. Only then is it emptied and written.
pub fn write_confined(root: &Path, rel: &Path, content: &[u8]) -> io::Result<()> {
    write_confined_mode(root, rel, content, None)
}

/// [`write_confined`], then the executable bits set (`Some(true)`) or cleared
/// (`Some(false)`) on the open descriptor (`fchmod`), never through the path.
/// `None` leaves the mode as created or found. Windows files have no
/// executable bit, so there `executable` changes nothing.
pub fn write_confined_mode(
    root: &Path,
    rel: &Path,
    content: &[u8],
    executable: Option<bool>,
) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        unix::write(&open_root(root)?, rel, content, executable)
    }
    #[cfg(windows)]
    {
        let _ = executable;
        win::write(&open_root(root)?, rel, content)
    }
}

/// Open the regular file `rel` beneath `root` for appending, from the host,
/// creating it and its folders when absent. Returns the file and whether it
/// was empty when opened (a new file wants its header first).
///
/// Every component, the last included, is opened with no-follow: a symbolic
/// link (on Windows also a junction or any other reparse point) anywhere on
/// the path is refused with an error naming it, as [`write_confined`] refuses
/// one. A non-regular or hard-linked file is refused as there.
pub fn open_append_confined(root: &Path, rel: &Path) -> io::Result<(File, bool)> {
    #[cfg(not(windows))]
    {
        unix::open_append(&open_root(root)?, rel)
    }
    #[cfg(windows)]
    {
        win::open_append(&open_root(root)?, rel)
    }
}

/// Remove `rel` beneath `root` from the host, never following a symbolic
/// link on the way ([`remove`]): a file or a link is unlinked, a directory is
/// removed with everything in it. `Ok(false)`: nothing was there as a chain
/// of real directories, so nothing was removed.
pub fn remove_confined(root: &Path, rel: &Path) -> io::Result<bool> {
    Ok(remove(&open_root(root)?, rel)? == Removal::Removed)
}

#[cfg(not(windows))]
mod unix {
    use super::{MAX_DEPTH, Removal};
    use cap_primitives::fs::{FollowSymlinks, open_dir_nofollow, stat};
    use std::ffi::OsString;
    use std::fs::File;
    use std::io;
    use std::path::{Component, Path};

    fn too_deep(what: &Path) -> io::Error {
        io::Error::other(format!(
            "{} is more than {MAX_DEPTH} folder levels deep; refusing to walk it",
            what.display()
        ))
    }

    pub(super) fn remove(root: &File, rel: &Path) -> io::Result<Removal> {
        let mut names = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(n) => names.push(n),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} is not a plain relative path", rel.display()),
                    ));
                }
            }
        }
        if names.len() > MAX_DEPTH {
            return Err(too_deep(rel));
        }
        let Some((last, parents)) = names.split_last() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an empty path names the worktree root",
            ));
        };
        // chain[i] is the directory that holds names[i].
        let mut chain: Vec<File> = vec![root.try_clone()?];
        for name in parents {
            let Some(dir) = chain.last() else {
                return Ok(Removal::Gone);
            };
            match open_dir_nofollow(dir, Path::new(name)) {
                Ok(sub) => chain.push(sub),
                Err(e) if not_a_real_dir(&e) => return Ok(Removal::Gone),
                Err(e) => return Err(e),
            }
        }
        let Some(dir) = chain.last() else {
            return Ok(Removal::Gone);
        };
        let last = Path::new(last);
        let kind = match stat(dir, last, FollowSymlinks::No) {
            Ok(meta) => meta.file_type(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Removal::Gone),
            Err(e) => return Err(e),
        };
        if kind.is_dir() {
            match open_dir_nofollow(dir, last) {
                Ok(sub) => remove_contents(sub, rel)?,
                // Swapped for a symlink after the stat: the unlink below
                // removes the link itself.
                Err(e) if not_a_real_dir(&e) => {}
                Err(e) => return Err(e),
            }
            remove_dir_or_link(dir, last)?;
        } else {
            remove_link_or_file(dir, last)?;
        }
        // Parents left empty, deepest first; the first that is not empty (or
        // not removable) ends it.
        for (i, name) in parents.iter().enumerate().rev() {
            if cap_primitives::fs::remove_dir(&chain[i], Path::new(name)).is_err() {
                break;
            }
        }
        Ok(Removal::Removed)
    }

    fn plain_names(rel: &Path) -> io::Result<Vec<&std::ffi::OsStr>> {
        let mut names = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(n) => names.push(n),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} is not a plain relative path", rel.display()),
                    ));
                }
            }
        }
        if names.len() > MAX_DEPTH {
            return Err(too_deep(rel));
        }
        Ok(names)
    }

    fn crosses_link(rel: &Path, at: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} crosses the symbolic link {}; refusing to write through it",
                rel.display(),
                at.display()
            ),
        )
    }

    /// The directory that holds `rel`'s last component, reached from `root`
    /// one no-follow `openat` at a time, each absent folder created; and that
    /// last component. A symbolic link on the way is refused, naming it.
    fn open_parent<'a>(root: &File, rel: &'a Path) -> io::Result<(File, &'a Path)> {
        use cap_primitives::fs::{DirOptions, create_dir};
        let names = plain_names(rel)?;
        let Some((last, parents)) = names.split_last() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an empty path names the root",
            ));
        };
        let mut dir = root.try_clone()?;
        let mut walked = std::path::PathBuf::new();
        for name in parents {
            walked.push(name);
            let name = Path::new(name);
            match stat(&dir, name, FollowSymlinks::No) {
                Ok(m) if m.file_type().is_symlink() => return Err(crosses_link(rel, &walked)),
                Ok(m) if !m.is_dir() => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} is not a directory", walked.display()),
                    ));
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    create_dir(&dir, name, &DirOptions::new())?;
                }
                Err(e) => return Err(e),
            }
            // No-follow: a component swapped for a link since the stat fails
            // here rather than being followed.
            dir = match open_dir_nofollow(&dir, name) {
                Ok(d) => d,
                Err(e) if not_a_real_dir(&e) => return Err(crosses_link(rel, &walked)),
                Err(e) => return Err(e),
            };
        }
        Ok((dir, Path::new(*last)))
    }

    /// Open `last` in `dir` for writing: created when absent, never through a
    /// link (`O_NOFOLLOW`), never blocking (`O_NONBLOCK`: opening a FIFO for
    /// writing would otherwise wait forever for a reader), never truncated at
    /// open. The descriptor is then checked: a regular file with one link.
    fn open_checked(dir: &File, last: &Path, rel: &Path, append: bool) -> io::Result<File> {
        use cap_primitives::fs::{OpenOptions, open};
        use std::os::unix::fs::MetadataExt;
        let mut options = OpenOptions::new();
        options
            .write(true)
            .append(append)
            .create(true)
            ._cap_fs_ext_nonblock(true)
            ._cap_fs_ext_follow(FollowSymlinks::No);
        let file = match open(dir, last, &options) {
            Ok(f) => f,
            // A link at the last component (planted or swapped in since the
            // caller looked): no-follow refused it.
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} is a symbolic link; refusing to write through it",
                        rel.display()
                    ),
                ));
            }
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is not a regular file (a FIFO, device, socket or directory is refused)",
                    rel.display()
                ),
            ));
        }
        if meta.nlink() > 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} has {} hard links; another name for it may be outside the tree, so \
                     it is not written",
                    rel.display(),
                    meta.nlink()
                ),
            ));
        }
        // Back to blocking I/O for the write itself.
        clear_nonblock(&file)?;
        Ok(file)
    }

    fn clear_nonblock(file: &File) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let fd = file.as_raw_fd();
        // SAFETY: fcntl on a descriptor this function borrows; F_GETFL and
        // F_SETFL touch only its status flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_executable(file: &File, executable: bool) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = file.metadata()?.permissions();
        let bits = perms.mode();
        let wanted = if executable {
            bits | 0o111
        } else {
            bits & !0o111
        };
        if wanted != bits {
            perms.set_mode(wanted);
            // `fchmod` on the descriptor: the path is never resolved again.
            file.set_permissions(perms)?;
        }
        Ok(())
    }

    /// See `super::write_confined_mode`.
    pub(super) fn write(
        root: &File,
        rel: &Path,
        content: &[u8],
        executable: Option<bool>,
    ) -> io::Result<()> {
        use std::io::Write;
        let (dir, last) = open_parent(root, rel)?;
        match stat(&dir, last, FollowSymlinks::No) {
            // Refused, never replaced (decision 0083): whoever planted the
            // link answers for it. The open below is no-follow too, so a link
            // planted after this look is refused the same way.
            Ok(m) if m.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} is a symbolic link; refusing to write through it",
                        rel.display()
                    ),
                ));
            }
            Ok(m) if m.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is a directory", rel.display()),
                ));
            }
            _ => {}
        }
        let mut file = open_checked(&dir, last, rel, false)?;
        // Emptied only after the checks: a refused file is left as it was.
        file.set_len(0)?;
        file.write_all(content)?;
        if let Some(executable) = executable {
            set_executable(&file, executable)?;
        }
        Ok(())
    }

    /// See `super::open_append_confined`.
    pub(super) fn open_append(root: &File, rel: &Path) -> io::Result<(File, bool)> {
        let (dir, last) = open_parent(root, rel)?;
        let file = open_checked(&dir, last, rel, true)?;
        let fresh = file.metadata()?.len() == 0;
        Ok((file, fresh))
    }

    /// One level of the walk: an open directory, the entries of it still to
    /// remove (name, is a real directory), and its name in the level above.
    struct Level {
        dir: File,
        pending: Vec<(OsString, bool)>,
        name: Option<OsString>,
    }

    fn entries(dir: &File) -> io::Result<Vec<(OsString, bool)>> {
        let mut out = Vec::new();
        for entry in cap_primitives::fs::read_base_dir(dir)? {
            let entry = entry?;
            out.push((entry.file_name(), entry.file_type()?.is_dir()));
        }
        Ok(out)
    }

    /// Everything inside the directory `top`, depth first on a heap stack,
    /// never following a link. `label` names the tree in an error.
    fn remove_contents(top: File, label: &Path) -> io::Result<()> {
        let pending = entries(&top)?;
        let mut stack = vec![Level {
            dir: top,
            pending,
            name: None,
        }];
        loop {
            let Some(level) = stack.last_mut() else {
                return Ok(());
            };
            match level.pending.pop() {
                Some((name, true)) => match open_dir_nofollow(&level.dir, Path::new(&name)) {
                    Ok(sub) => {
                        if stack.len() >= MAX_DEPTH {
                            return Err(too_deep(label));
                        }
                        let pending = entries(&sub)?;
                        stack.push(Level {
                            dir: sub,
                            pending,
                            name: Some(name),
                        });
                    }
                    // Swapped for a link since the listing: remove the link.
                    Err(e) if not_a_real_dir(&e) => {
                        remove_link_or_file(&level.dir, Path::new(&name))?
                    }
                    Err(e) => return Err(e),
                },
                Some((name, false)) => remove_link_or_file(&level.dir, Path::new(&name))?,
                None => {
                    let done = stack.pop();
                    if let (Some(done), Some(parent)) = (done, stack.last()) {
                        let Level { dir, name, .. } = done;
                        drop(dir);
                        if let Some(name) = name {
                            remove_dir_or_link(&parent.dir, Path::new(&name))?;
                        }
                    }
                }
            }
        }
    }

    /// Remove the (now empty) directory `name`; if it was swapped for a link
    /// meanwhile, the link.
    fn remove_dir_or_link(dir: &File, name: &Path) -> io::Result<()> {
        match cap_primitives::fs::remove_dir(dir, name) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => remove_link_or_file(dir, name),
        }
    }

    /// Unlink `name` in `dir`: the link itself, never its target.
    fn remove_link_or_file(dir: &File, name: &Path) -> io::Result<()> {
        match cap_primitives::fs::remove_file(dir, name) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// An open error that means the component is not a real directory (now):
    /// a symlink (refused by no-follow), a file, or nothing.
    fn not_a_real_dir(e: &io::Error) -> bool {
        if e.kind() == io::ErrorKind::NotFound {
            return true;
        }
        #[cfg(unix)]
        {
            if let Some(code) = e.raw_os_error() {
                return code == libc::ELOOP || code == libc::ENOTDIR || code == libc::EMLINK;
            }
        }
        // cap-primitives reports a refused escape with its own kind and no OS
        // code.
        matches!(
            e.kind(),
            io::ErrorKind::NotADirectory | io::ErrorKind::PermissionDenied
        )
    }
}

/// The parts of the Windows walk that are plain data handling: reading a
/// folder listing buffer, and telling the name asked for from the name a
/// handle reached. They are pure functions over bytes and UTF-16, so their
/// tests run on every platform.
#[cfg_attr(not(windows), allow(dead_code))]
mod nt_data {
    use std::io;

    /// Byte offsets in a `FILE_FULL_DIR_INFO` record (`FileFullDirectoryInfo`
    /// listing class): `NextEntryOffset`, `FileNameLength`, the first unit of
    /// `FileName`, and the size of the record type with its one name unit
    /// and its padding. The Windows build asserts each against the
    /// `windows-sys` type, so they cannot drift.
    pub(super) const NEXT_ENTRY_OFFSET: usize = 0;
    pub(super) const FILE_NAME_LENGTH: usize = 60;
    pub(super) const FILE_NAME: usize = 68;
    pub(super) const RECORD_SIZE: usize = 72;

    fn bad_record(what: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a folder listing record {what}; refusing to read the listing"),
        )
    }

    fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
        let bytes = buf.get(at..at.checked_add(4)?)?;
        Some(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// The names in one buffer of `FILE_FULL_DIR_INFO` records, "." and ".."
    /// left out, appended to `out`.
    ///
    /// Every field is read by value from the bytes; no reference to a record
    /// is ever formed, so a record the system (or a file system filter)
    /// wrote misaligned or short is never read as a typed value. Each record
    /// must fit whole (`RECORD_SIZE` and its name) inside `buf`, and the next
    /// one must start 8-aligned past the end of this one's name; anything
    /// else is refused with an error.
    pub(super) fn full_dir_names(buf: &[u8], out: &mut Vec<Vec<u16>>) -> io::Result<()> {
        let mut offset = 0usize;
        loop {
            let fits = offset
                .checked_add(RECORD_SIZE)
                .is_some_and(|end| end <= buf.len());
            if !fits {
                return Err(bad_record("overran its buffer"));
            }
            let next = u32_at(buf, offset + NEXT_ENTRY_OFFSET)
                .ok_or_else(|| bad_record("overran its buffer"))? as usize;
            let name_bytes = u32_at(buf, offset + FILE_NAME_LENGTH)
                .ok_or_else(|| bad_record("overran its buffer"))?
                as usize;
            if !name_bytes.is_multiple_of(2) {
                return Err(bad_record("has a name of an odd number of bytes"));
            }
            let start = offset + FILE_NAME;
            let Some(name) = start
                .checked_add(name_bytes)
                .and_then(|end| buf.get(start..end))
            else {
                return Err(bad_record("has a name that overran its buffer"));
            };
            let name: Vec<u16> = name
                .chunks_exact(2)
                .map(|b| u16::from_ne_bytes([b[0], b[1]]))
                .collect();
            let dot = u16::from(b'.');
            if name != [dot] && name != [dot, dot] {
                out.push(name);
            }
            if next == 0 {
                return Ok(());
            }
            if !next.is_multiple_of(8) {
                return Err(bad_record("points to a next record that is not 8-aligned"));
            }
            if next < FILE_NAME + name_bytes {
                return Err(bad_record("points to a next record inside itself"));
            }
            offset = offset
                .checked_add(next)
                .ok_or_else(|| bad_record("overran its buffer"))?;
        }
    }

    /// The last component of an NT path (`\Users\me\tree\a.txt` gives
    /// `a.txt`).
    pub(super) fn last_component(path: &[u16]) -> &[u16] {
        let sep = u16::from(b'\\');
        match path.iter().rposition(|&c| c == sep) {
            Some(i) => &path[i + 1..],
            None => path,
        }
    }

    /// Whether `real`, the name a handle reached, is the name `asked` that the
    /// caller checked, other than by case. Compared with
    /// `kgraf_core::paths::fold_name`, the folding the protected-name check
    /// uses, so two names that fold equal get one answer from that check.
    /// An NTFS or FAT short (8.3) name such as `GITATT~1` or `GI7D29~1`
    /// resolves to a file whose real name folds differently, so it is told
    /// apart here whatever its form. A name that is not valid UTF-16 must
    /// match exactly.
    pub(super) fn same_name(asked: &[u16], real: &[u16]) -> bool {
        if asked == real {
            return true;
        }
        match (String::from_utf16(asked), String::from_utf16(real)) {
            (Ok(a), Ok(r)) => kgraf_core::paths::fold_name(&a) == kgraf_core::paths::fold_name(&r),
            _ => false,
        }
    }
}

/// The Windows walk. Every component is opened by `NtCreateFile` as one name
/// relative to the handle of the folder that holds it, with
/// `FILE_OPEN_REPARSE_POINT`, so the kernel never resolves more than that one
/// name and never follows what is there: a symbolic link, a junction or any
/// other reparse point is opened itself. Its `FILE_ATTRIBUTE_REPARSE_POINT`,
/// read on the handle just opened, then refuses it. A folder held open by
/// handle cannot be swapped under the walk: renaming a name above it changes
/// nothing about the folder the handle refers to.
///
/// Every reparse point is refused, not only links and junctions: a tag this
/// code does not know may also redirect, so it fails loud. A tree inside a
/// cloud-synced folder, whose files are reparse points, is refused that way.
///
/// **A held folder made a reparse point in place.** On NTFS any user who may
/// write a folder can turn it, while empty, into a junction
/// (`FSCTL_SET_REPARSE_POINT`), and the handle this walk holds on it stays
/// open. `FILE_OPEN_REPARSE_POINT` covers only the name being opened, so
/// every open also sets `OBJ_DONT_REPARSE`: the kernel then fails the open
/// (`STATUS_REPARSE_POINT_ENCOUNTERED`) rather than resolve a name through a
/// reparse point anywhere, the folder handle included. Rust's `std` sets the
/// same flag in its `remove_dir_all` (CVE-2022-21658) but opens again without
/// it where an older Windows rejects it as an invalid parameter; here that
/// rejection is an error naming the reason, never a weaker open. After each
/// open the folder it was made in is also read again, and refused if it is
/// now a reparse point.
///
/// **Short names.** A file on NTFS or FAT can also have a short (8.3) name
/// (`GITATT~1` for `.gitattributes`), and the kernel resolves it as well as
/// the long name. A caller judges the name it passes (protected names,
/// decision 0083), so every name opened is compared with the long name of
/// what the handle reached (`FileNormalizedNameInfo`) and refused when the
/// two differ other than by case. That closes every alias form, not only the
/// short names a list could predict.
///
/// **Removal and other processes.** A folder on the way to a removal is
/// opened to list and enter it only, never for deletion: a process whose
/// current folder is on the path holds it without sharing deletion, and an
/// open for deletion would fail. The empty-parent tidy-up opens each parent
/// for deletion again, by name relative to its own parent, and goes on only
/// when that is the same folder the walk held (volume serial and file id).
///
/// **What it guarantees.** No host write or removal lands outside what the
/// worker could already write itself. A worker holds the right to delete in
/// its own tree, so it can move a folder this walk holds to anywhere it can
/// write (its temporary folder, say); a write through that handle then lands
/// there, which the worker could have written anyway. It cannot land in a
/// place the worker cannot write.
#[cfg(windows)]
mod win {
    use super::nt_data;
    use super::{MAX_DEPTH, Removal};
    use std::ffi::{OsStr, OsString};
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::{Component, Path, PathBuf};
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
        FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    };
    use windows_sys::Win32::Foundation::{
        ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, ERROR_NO_MORE_FILES,
        ERROR_NOT_SUPPORTED, HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE,
        RtlNtStatusToDosError, STATUS_DELETE_PENDING, STATUS_FILE_IS_A_DIRECTORY,
        STATUS_INVALID_PARAMETER, STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION,
        STATUS_OBJECT_NAME_INVALID, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
        STATUS_REPARSE_POINT_ENCOUNTERED, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
        FILE_FULL_DIR_INFO, FILE_GENERIC_WRITE, FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_STANDARD_INFO, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES,
        FILE_WRITE_DATA, FileAttributeTagInfo, FileBasicInfo, FileDispositionInfo,
        FileDispositionInfoEx, FileFullDirectoryInfo, FileFullDirectoryRestartInfo, FileIdInfo,
        FileNormalizedNameInfo, FileStandardInfo, GetFileInformationByHandleEx, SYNCHRONIZE,
        SetFileInformationByHandle,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
    use windows_sys::Win32::System::SystemServices::{
        IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
    };

    // The listing parser reads `FILE_FULL_DIR_INFO` by byte offset; these
    // hold it to the type's real layout.
    const _: () = {
        use std::mem::{offset_of, size_of};
        assert!(offset_of!(FILE_FULL_DIR_INFO, NextEntryOffset) == nt_data::NEXT_ENTRY_OFFSET);
        assert!(offset_of!(FILE_FULL_DIR_INFO, FileNameLength) == nt_data::FILE_NAME_LENGTH);
        assert!(offset_of!(FILE_FULL_DIR_INFO, FileName) == nt_data::FILE_NAME);
        assert!(size_of::<FILE_FULL_DIR_INFO>() == nt_data::RECORD_SIZE);
    };

    /// What a folder on the way is opened for: entering and listing it. Never
    /// deletion: another process's current folder on the path would refuse
    /// that open.
    pub(super) const DIR_ACCESS: u32 = FILE_LIST_DIRECTORY | FILE_TRAVERSE;
    /// What an entry to remove is opened for: listing it when it is a folder,
    /// clearing its read-only attribute, and the delete disposition.
    pub(super) const REMOVE_ACCESS: u32 = DELETE | FILE_LIST_DIRECTORY | FILE_WRITE_ATTRIBUTES;

    fn invalid(msg: String) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, msg)
    }

    fn too_deep(what: &Path) -> io::Error {
        io::Error::other(format!(
            "{} is more than {MAX_DEPTH} folder levels deep; refusing to walk it",
            what.display()
        ))
    }

    pub(super) fn nt_error(status: NTSTATUS) -> io::Error {
        match status {
            // Every open here passes only documented options and flags, so
            // an invalid parameter is `OBJ_DONT_REPARSE`, which older Windows
            // releases reject. Opening without it could resolve a name
            // through a folder made a junction in place, so it is an error.
            STATUS_INVALID_PARAMETER => io::Error::new(
                io::ErrorKind::Unsupported,
                "Windows refused an open as an invalid parameter: this Windows release likely \
                 does not support OBJ_DONT_REPARSE, which kGraf needs so that no name in a \
                 worker's tree is resolved through a junction or link; kGraf does not open \
                 without it",
            ),
            STATUS_REPARSE_POINT_ENCOUNTERED => invalid(
                "a folder on the path became a junction or other reparse point while it was \
                 open; refusing to resolve a name through it"
                    .into(),
            ),
            _ => {
                // SAFETY: a pure conversion of a status code; it reads no
                // memory.
                let code = unsafe { RtlNtStatusToDosError(status) };
                io::Error::from_raw_os_error(code as i32)
            }
        }
    }

    /// An open status that means "nothing is at that name as asked now":
    /// absent, a path part absent, not a folder, or being deleted.
    fn is_gone(status: NTSTATUS) -> bool {
        matches!(
            status,
            STATUS_OBJECT_NAME_NOT_FOUND
                | STATUS_OBJECT_PATH_NOT_FOUND
                | STATUS_NOT_A_DIRECTORY
                | STATUS_DELETE_PENDING
        )
    }

    /// Open the single name `name` relative to the folder handle `dir`.
    /// `FILE_OPEN_REPARSE_POINT` is always set, so a reparse point at `name`
    /// is opened itself and never followed, and `OBJ_DONT_REPARSE`, so no
    /// reparse point is resolved anywhere else either (the folder `dir`
    /// itself included, if it was made one in place); `SYNCHRONIZE`,
    /// `FILE_READ_ATTRIBUTES` and synchronous I/O are always asked for, so
    /// the handle works as a `std::fs::File`. Returns the raw status on
    /// failure, for callers that tell "absent" from "is a folder";
    /// [`nt_error`] names the two statuses `OBJ_DONT_REPARSE` can cause.
    pub(super) fn nt_open(
        dir: &File,
        name: &OsStr,
        access: u32,
        disposition: u32,
        options: u32,
    ) -> Result<File, NTSTATUS> {
        let wide: Vec<u16> = name.encode_wide().collect();
        let Ok(bytes) = u16::try_from(wide.len() * 2) else {
            return Err(STATUS_OBJECT_NAME_INVALID);
        };
        let object_name = UNICODE_STRING {
            Length: bytes,
            MaximumLength: bytes,
            Buffer: wide.as_ptr().cast_mut(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: dir.as_raw_handle() as HANDLE,
            ObjectName: &object_name,
            // As every Win32 open is: names compare without case. Never a
            // reparse: see the module notes.
            Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle: HANDLE = std::ptr::null_mut();
        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: every pointer is to a local that outlives the call
        // (`wide` backs `object_name`, which backs `attributes`); `dir` is a
        // handle this function borrows; no EA buffer is passed.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                access | SYNCHRONIZE | FILE_READ_ATTRIBUTES,
                &attributes,
                &mut status_block,
                std::ptr::null(),
                FILE_ATTRIBUTE_NORMAL,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                disposition,
                options | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            return Err(status);
        }
        // SAFETY: NtCreateFile succeeded, so `handle` is a new file handle
        // that nothing else owns; the File closes it.
        Ok(unsafe { File::from_raw_handle(handle as _) })
    }

    /// Read a fixed-size information class of `file` into `T`.
    fn query<T: Default>(file: &File, class: FILE_INFO_BY_HANDLE_CLASS) -> io::Result<T> {
        let mut out = T::default();
        // SAFETY: `out` is a writable `T` of the size passed, and `class` is
        // the information class whose record `T` is.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                class,
                (&mut out as *mut T).cast(),
                std::mem::size_of::<T>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(out)
    }

    fn set<T>(file: &File, class: FILE_INFO_BY_HANDLE_CLASS, value: &T) -> io::Result<()> {
        // SAFETY: `value` is a readable `T` of the size passed, and `class`
        // is the information class whose record `T` is.
        let ok = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle() as HANDLE,
                class,
                (value as *const T).cast(),
                std::mem::size_of::<T>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn attribute_tag(file: &File) -> io::Result<FILE_ATTRIBUTE_TAG_INFO> {
        query(file, FileAttributeTagInfo)
    }

    fn is_reparse(info: &FILE_ATTRIBUTE_TAG_INFO) -> bool {
        info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    fn is_real_dir(info: &FILE_ATTRIBUTE_TAG_INFO) -> bool {
        info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 && !is_reparse(info)
    }

    /// The kind of a reparse point by its tag, for an error message: the
    /// noun, and a detail for a tag that is neither a link nor a junction.
    fn reparse_kind(tag: u32) -> (&'static str, String) {
        match tag {
            IO_REPARSE_TAG_SYMLINK => ("symbolic link", String::new()),
            IO_REPARSE_TAG_MOUNT_POINT => ("junction", String::new()),
            other => ("reparse point", format!(" (reparse tag {other:#010x})")),
        }
    }

    fn crosses_link(rel: &Path, at: &Path, tag: u32) -> io::Error {
        let (noun, detail) = reparse_kind(tag);
        invalid(format!(
            "{} crosses the {noun} {}{detail}; refusing to write through it",
            rel.display(),
            at.display()
        ))
    }

    fn is_link(rel: &Path, tag: u32) -> io::Error {
        let (noun, detail) = reparse_kind(tag);
        invalid(format!(
            "{} is a {noun}{detail}; refusing to write through it",
            rel.display()
        ))
    }

    /// The long name of what `file` is open on: the last component of its
    /// normalized path (`FileNormalizedNameInfo`, which spells every
    /// component in its long form, where `FileNameInfo` keeps a short name
    /// the open used). A file system that cannot report it fails the call,
    /// and the caller refuses: a short name could not be told apart without
    /// it.
    fn long_name(file: &File) -> io::Result<Vec<u16>> {
        // FILE_NAME_INFO: a u32 byte count, then the name. u32 words keep
        // the record aligned. The second size holds the longest NT path.
        let mut buf: Vec<u32> = vec![0; 1 + 256];
        loop {
            let bytes = buf.len() * std::mem::size_of::<u32>();
            // SAFETY: `buf` is writable for `bytes` bytes, as passed, and
            // aligned for FILE_NAME_INFO.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle() as HANDLE,
                    FileNormalizedNameInfo,
                    buf.as_mut_ptr().cast(),
                    bytes as u32,
                )
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                let longest = 1 + (u16::MAX as usize + 2) / 4;
                if e.raw_os_error() == Some(ERROR_MORE_DATA as i32) && buf.len() < longest {
                    buf = vec![0; longest];
                    continue;
                }
                return Err(e);
            }
            let len = buf[0] as usize;
            if !len.is_multiple_of(2) || len > bytes - 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a file name record overran its buffer",
                ));
            }
            let units: Vec<u16> = buf[1..]
                .iter()
                .flat_map(|w| {
                    let b = w.to_ne_bytes();
                    [
                        u16::from_ne_bytes([b[0], b[1]]),
                        u16::from_ne_bytes([b[2], b[3]]),
                    ]
                })
                .take(len / 2)
                .collect();
            return Ok(nt_data::last_component(&units).to_vec());
        }
    }

    /// Refuse `file` unless the name `asked`, which the caller judged, is the
    /// long name of what it reached, other than by case (module notes:
    /// short names). `at` names the component in an error.
    pub(super) fn check_name(file: &File, asked: &OsStr, at: &Path) -> io::Result<()> {
        let real = long_name(file).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "cannot read the long name of {} from its handle ({e}); without it a short \
                     (8.3) name cannot be told from the name asked for, so it is refused",
                    at.display()
                ),
            )
        })?;
        let asked: Vec<u16> = asked.encode_wide().collect();
        if nt_data::same_name(&asked, &real) {
            return Ok(());
        }
        Err(invalid(format!(
            "{} reached a file whose name is {}: a short (8.3) name or other alias of another \
             name is refused",
            at.display(),
            String::from_utf16_lossy(&real)
        )))
    }

    /// Refuse to go on in `dir` once it is a reparse point: read again after
    /// an open made in it, since a held folder can be made a junction in
    /// place. `OBJ_DONT_REPARSE` already stops the kernel resolving through
    /// it; this is the second, independent check.
    fn still_real_dir(dir: &File, at: &Path) -> io::Result<()> {
        let info = attribute_tag(dir)?;
        if is_reparse(&info) {
            let (noun, detail) = reparse_kind(info.ReparseTag);
            return Err(invalid(format!(
                "the folder holding {} became a {noun}{detail} while it was open; refusing to \
                 go on in it",
                at.display()
            )));
        }
        Ok(())
    }

    /// The identity of what `file` is open on: volume serial and file id.
    fn file_id(file: &File) -> io::Result<(u64, [u8; 16])> {
        let info: FILE_ID_INFO = query(file, FileIdInfo)?;
        Ok((info.VolumeSerialNumber, info.FileId.Identifier))
    }

    /// Names a Windows program could not open by path, or that path parsing
    /// would read as something else: a stream (`name:stream`), a reserved
    /// device (`NUL`, `COM1.txt`), a trailing dot or space (dropped by
    /// path parsing), a control character or a wildcard. `NtCreateFile`
    /// would make such a name as given, which then no tool reads back.
    fn unusable_name(name: &[u16]) -> bool {
        const BAD: &[u16] = &[
            b'<' as u16,
            b'>' as u16,
            b':' as u16,
            b'"' as u16,
            b'|' as u16,
            b'?' as u16,
            b'*' as u16,
            b'/' as u16,
            b'\\' as u16,
        ];
        if name.iter().any(|&c| c < 0x20 || BAD.contains(&c)) {
            return true;
        }
        if matches!(name.last(), Some(&c) if c == b'.' as u16 || c == b' ' as u16) {
            return true;
        }
        let stem: String = String::from_utf16_lossy(name)
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end()
            .to_ascii_uppercase();
        let device = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        );
        let numbered = ["COM", "LPT"].iter().any(|p| {
            stem.strip_prefix(p).is_some_and(|n| {
                matches!(n, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
                    || matches!(n, "\u{b9}" | "\u{b2}" | "\u{b3}")
            })
        });
        device || numbered
    }

    /// `rel`'s components, each a single plain name, at most [`MAX_DEPTH`].
    /// `creating`: the names may be made, so a name no tool could read back
    /// is refused too.
    fn plain_names(rel: &Path, creating: bool) -> io::Result<Vec<&OsStr>> {
        let mut names = Vec::new();
        for c in rel.components() {
            let Component::Normal(name) = c else {
                return Err(invalid(format!(
                    "{} is not a plain relative path",
                    rel.display()
                )));
            };
            let wide: Vec<u16> = name.encode_wide().collect();
            // A separator, a stream suffix or a NUL would make the kernel
            // read more than one name; never passed on.
            let multi = wide
                .iter()
                .any(|&c| c == 0 || c == b'\\' as u16 || c == b'/' as u16 || c == b':' as u16);
            if wide.is_empty() || multi || (creating && unusable_name(&wide)) {
                return Err(invalid(format!(
                    "{} has a name Windows cannot use as one plain file name: {}",
                    rel.display(),
                    name.to_string_lossy()
                )));
            }
            names.push(name);
        }
        if names.len() > MAX_DEPTH {
            return Err(too_deep(rel));
        }
        Ok(names)
    }

    /// The folder that holds `rel`'s last component, reached from `root` one
    /// name at a time, each absent folder made; and that last component. A
    /// reparse point on the way is refused, naming it, and so is a name that
    /// reached a folder under another (short) name.
    pub(super) fn open_parent<'a>(root: &File, rel: &'a Path) -> io::Result<(File, &'a OsStr)> {
        let names = plain_names(rel, true)?;
        let Some((last, parents)) = names.split_last() else {
            return Err(invalid("an empty path names the root".into()));
        };
        let mut dir = root.try_clone()?;
        let mut walked = PathBuf::new();
        for name in parents {
            walked.push(name);
            // Any type, so a link or a file in the way is seen and named.
            let next = match nt_open(&dir, name, DIR_ACCESS, FILE_OPEN, 0) {
                Ok(f) => f,
                Err(STATUS_OBJECT_NAME_NOT_FOUND) => {
                    match nt_open(&dir, name, DIR_ACCESS, FILE_CREATE, FILE_DIRECTORY_FILE) {
                        Ok(f) => f,
                        // Made by another writer since the look above: open
                        // what is there now, checked below like any other.
                        Err(STATUS_OBJECT_NAME_COLLISION) => {
                            nt_open(&dir, name, DIR_ACCESS, FILE_OPEN, 0).map_err(nt_error)?
                        }
                        Err(s) => return Err(nt_error(s)),
                    }
                }
                Err(s) => return Err(nt_error(s)),
            };
            still_real_dir(&dir, &walked)?;
            let info = attribute_tag(&next)?;
            if is_reparse(&info) {
                return Err(crosses_link(rel, &walked, info.ReparseTag));
            }
            if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
                return Err(invalid(format!("{} is not a directory", walked.display())));
            }
            check_name(&next, name, &walked)?;
            dir = next;
        }
        Ok((dir, last))
    }

    /// Open `last` in `dir` for writing (`append`: for appending only),
    /// made when absent, never truncated at open, a reparse point opened
    /// itself. Then checked on the handle: no reparse point, no folder, its
    /// long name the one asked for, one link; and `dir` still a real folder.
    /// Windows has no FIFO in a folder, so no open here can block.
    pub(super) fn open_checked(
        dir: &File,
        last: &OsStr,
        rel: &Path,
        append: bool,
    ) -> io::Result<File> {
        // Append: `FILE_APPEND_DATA` without `FILE_WRITE_DATA`, so every
        // write lands at the end, as `std` opens a file for appending.
        let access = if append {
            FILE_GENERIC_WRITE & !FILE_WRITE_DATA
        } else {
            FILE_GENERIC_WRITE
        };
        let file = match nt_open(dir, last, access, FILE_OPEN_IF, FILE_NON_DIRECTORY_FILE) {
            Ok(f) => f,
            // A folder, or a folder link or junction: say which.
            Err(STATUS_FILE_IS_A_DIRECTORY) => {
                let probe = nt_open(dir, last, 0, FILE_OPEN, 0).map_err(nt_error)?;
                let info = attribute_tag(&probe)?;
                return Err(if is_reparse(&info) {
                    is_link(rel, info.ReparseTag)
                } else {
                    invalid(format!("{} is a directory", rel.display()))
                });
            }
            Err(s) => return Err(nt_error(s)),
        };
        still_real_dir(dir, rel)?;
        let info = attribute_tag(&file)?;
        if is_reparse(&info) {
            return Err(is_link(rel, info.ReparseTag));
        }
        if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Err(invalid(format!("{} is a directory", rel.display())));
        }
        check_name(&file, last, rel)?;
        let links = query::<FILE_STANDARD_INFO>(&file, FileStandardInfo)?.NumberOfLinks;
        if links > 1 {
            return Err(invalid(format!(
                "{} has {links} hard links; another name for it may be outside the tree, so \
                 it is not written",
                rel.display()
            )));
        }
        Ok(file)
    }

    /// See `super::write_confined_mode`.
    pub(super) fn write(root: &File, rel: &Path, content: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let (dir, last) = open_parent(root, rel)?;
        let mut file = open_checked(&dir, last, rel, false)?;
        // Emptied only after the checks: a refused file is left as it was.
        file.set_len(0)?;
        file.write_all(content)
    }

    /// See `super::open_append_confined`.
    pub(super) fn open_append(root: &File, rel: &Path) -> io::Result<(File, bool)> {
        let (dir, last) = open_parent(root, rel)?;
        let file = open_checked(&dir, last, rel, true)?;
        let fresh = file.metadata()?.len() == 0;
        Ok((file, fresh))
    }

    /// Delete what `file` is open on (its name, never what a link names).
    /// POSIX semantics first: the name goes at once, and a read-only file
    /// goes too (git makes its object files read-only). A file system or a
    /// Windows release without them gets [`delete_classic`]. `label` names
    /// the entry in an error.
    fn delete(file: &File, label: &Path) -> io::Result<()> {
        let posix = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE
                | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
                | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        };
        let e = match set(file, FileDispositionInfoEx, &posix) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        let unsupported = [
            ERROR_INVALID_PARAMETER,
            ERROR_NOT_SUPPORTED,
            ERROR_INVALID_FUNCTION,
        ];
        if !matches!(e.raw_os_error(), Some(c) if unsupported.contains(&(c as u32))) {
            return Err(e);
        }
        delete_classic(file, label)
    }

    /// The classic delete disposition: the name goes when the last handle
    /// closes, which for this walk is at once. It refuses a read-only file,
    /// so the attribute is cleared on the handle first. The attribute belongs
    /// to the file, not the name: with a second hard link (whose name may be
    /// outside the tree) clearing it would change that file too, so a
    /// read-only file with more than one link is refused, unchanged. When the
    /// delete then fails, the attribute is set back.
    pub(super) fn delete_classic(file: &File, label: &Path) -> io::Result<()> {
        let basic: FILE_BASIC_INFO = query(file, FileBasicInfo)?;
        let read_only = basic.FileAttributes & FILE_ATTRIBUTE_READONLY != 0;
        if read_only {
            let links = query::<FILE_STANDARD_INFO>(file, FileStandardInfo)?.NumberOfLinks;
            if links > 1 {
                return Err(invalid(format!(
                    "{} is read-only and has {links} hard links; clearing the attribute to \
                     delete it would change the file under every name, one maybe outside the \
                     tree, so it is not removed",
                    label.display()
                )));
            }
            set_attributes(file, basic.FileAttributes & !FILE_ATTRIBUTE_READONLY)?;
        }
        let deleted = set(
            file,
            FileDispositionInfo,
            &FILE_DISPOSITION_INFO { DeleteFile: true },
        );
        match deleted {
            Err(e) if read_only => match set_attributes(file, basic.FileAttributes) {
                Ok(()) => Err(e),
                Err(restore) => Err(io::Error::new(
                    e.kind(),
                    format!(
                        "{} was not deleted ({e}), and its read-only attribute, cleared for \
                         the delete, could not be set back ({restore})",
                        label.display()
                    ),
                )),
            },
            other => other,
        }
    }

    /// Set `attributes` on `file` through its handle, leaving its times.
    fn set_attributes(file: &File, attributes: u32) -> io::Result<()> {
        let basic = FILE_BASIC_INFO {
            // Zero times mean "leave as they are".
            CreationTime: 0,
            LastAccessTime: 0,
            LastWriteTime: 0,
            ChangeTime: 0,
            // Zero would mean "leave as they are" too; NORMAL clears them.
            FileAttributes: if attributes == 0 {
                FILE_ATTRIBUTE_NORMAL
            } else {
                attributes
            },
        };
        set(file, FileBasicInfo, &basic)
    }

    /// The names in the folder `dir`, read whole before any is removed. "."
    /// and ".." are left out. What each is gets judged on its own handle when
    /// it is opened, not here. Each buffer is read by
    /// [`nt_data::full_dir_names`], which refuses a malformed record.
    fn entries(dir: &File) -> io::Result<Vec<OsString>> {
        // u64 words: the system writes 8-byte aligned records.
        let mut buf = vec![0u64; 8 * 1024];
        let size = buf.len() * std::mem::size_of::<u64>();
        let mut class = FileFullDirectoryRestartInfo;
        let mut names: Vec<Vec<u16>> = Vec::new();
        loop {
            // SAFETY: `buf` is writable for `size` bytes, as passed.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    dir.as_raw_handle() as HANDLE,
                    class,
                    buf.as_mut_ptr().cast(),
                    size as u32,
                )
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                    return Ok(names.into_iter().map(|n| OsString::from_wide(&n)).collect());
                }
                return Err(e);
            }
            class = FileFullDirectoryInfo;
            let bytes: Vec<u8> = buf.iter().flat_map(|w| w.to_ne_bytes()).collect();
            nt_data::full_dir_names(&bytes, &mut names)?;
        }
    }

    /// A real folder `name` in `dir`, opened with `access`; `None` when it is
    /// absent, not a folder, or a reparse point (never entered). Its long
    /// name must be `name` (`at` names it in an error).
    fn open_real_dir(dir: &File, name: &OsStr, access: u32, at: &Path) -> io::Result<Option<File>> {
        let sub = match nt_open(dir, name, access, FILE_OPEN, FILE_DIRECTORY_FILE) {
            Ok(f) => f,
            Err(s) if is_gone(s) => return Ok(None),
            Err(s) => return Err(nt_error(s)),
        };
        still_real_dir(dir, at)?;
        if !is_real_dir(&attribute_tag(&sub)?) {
            return Ok(None);
        }
        check_name(&sub, name, at)?;
        Ok(Some(sub))
    }

    /// See `super::remove`.
    pub(super) fn remove(root: &File, rel: &Path) -> io::Result<Removal> {
        let names = plain_names(rel, false)?;
        let Some((last, parents)) = names.split_last() else {
            return Err(invalid("an empty path names the worktree root".into()));
        };
        // chain[i] is the folder that holds names[i], open to list and enter
        // only (DIR_ACCESS): another process's current folder on the path
        // must not make the removal fail.
        let mut chain: Vec<File> = vec![root.try_clone()?];
        let mut walked = PathBuf::new();
        for name in parents {
            walked.push(name);
            let Some(dir) = chain.last() else {
                return Ok(Removal::Gone);
            };
            match open_real_dir(dir, name, DIR_ACCESS, &walked)? {
                Some(sub) => chain.push(sub),
                None => return Ok(Removal::Gone),
            }
        }
        let Some(dir) = chain.last() else {
            return Ok(Removal::Gone);
        };
        let target = match nt_open(dir, last, REMOVE_ACCESS, FILE_OPEN, 0) {
            Ok(f) => f,
            Err(s) if is_gone(s) => return Ok(Removal::Gone),
            Err(s) => return Err(nt_error(s)),
        };
        still_real_dir(dir, rel)?;
        check_name(&target, last, rel)?;
        if is_real_dir(&attribute_tag(&target)?) {
            remove_contents(&target, rel)?;
        }
        // A file, an emptied folder, or a link or junction itself.
        delete(&target, rel)?;
        drop(target);
        // Parents left empty, deepest first; the first that is not empty, or
        // cannot be removed, ends it (tidiness only: git tracks no folders).
        while chain.len() > 1 {
            let Some(held) = chain.pop() else { break };
            let Some(parent) = chain.last() else { break };
            // `held` is the folder parents[chain.len() - 1].
            let depth = chain.len();
            let at: PathBuf = parents[..depth].iter().collect();
            if remove_empty_parent(parent, parents[depth - 1], held, &at).is_err() {
                break;
            }
        }
        Ok(Removal::Removed)
    }

    /// Remove the folder `name` in `parent` when it is still the folder
    /// `held` the walk went through, and empty. It is opened again for
    /// deletion, by name relative to `parent`'s handle, and compared with
    /// `held` by volume serial and file id; another folder under that name
    /// now is left alone. Any failure is returned and ends the tidy-up.
    fn remove_empty_parent(parent: &File, name: &OsStr, held: File, at: &Path) -> io::Result<()> {
        let again =
            nt_open(parent, name, DELETE, FILE_OPEN, FILE_DIRECTORY_FILE).map_err(nt_error)?;
        if file_id(&again)? != file_id(&held)? {
            return Err(invalid(format!(
                "{} is no longer the folder the removal went through",
                at.display()
            )));
        }
        // The walk's handle closes first, so a classic delete disposition
        // takes effect when `again` closes.
        drop(held);
        delete(&again, at)
    }

    /// One level of the walk: an open folder, its path for errors, and its
    /// entries still to remove.
    struct Level {
        dir: File,
        path: PathBuf,
        pending: Vec<OsString>,
    }

    /// Everything inside the folder `top`, depth first on a heap stack,
    /// never entering a reparse point. `top` itself stays for the caller.
    /// Names come from the listing, so each is the entry's long name, and no
    /// short-name check is needed for them.
    fn remove_contents(top: &File, label: &Path) -> io::Result<()> {
        let mut stack = vec![Level {
            dir: top.try_clone()?,
            path: label.to_path_buf(),
            pending: entries(top)?,
        }];
        loop {
            let Some(level) = stack.last_mut() else {
                return Ok(());
            };
            match level.pending.pop() {
                Some(name) => {
                    let path = level.path.join(&name);
                    let child = match nt_open(&level.dir, &name, REMOVE_ACCESS, FILE_OPEN, 0) {
                        Ok(f) => f,
                        Err(s) if is_gone(s) => continue,
                        Err(s) => return Err(nt_error(s)),
                    };
                    // Judged on the handle, not the listing: what is there now.
                    if is_real_dir(&attribute_tag(&child)?) {
                        if stack.len() >= MAX_DEPTH {
                            return Err(too_deep(label));
                        }
                        let pending = entries(&child)?;
                        stack.push(Level {
                            dir: child,
                            path,
                            pending,
                        });
                    } else {
                        delete(&child, &path)?;
                    }
                }
                None => {
                    if let Some(done) = stack.pop()
                        && !stack.is_empty()
                    {
                        delete(&done.dir, &done.path)?;
                    }
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The race's effect, set up without the race: a listed path whose
    /// directory is now a symlink to a directory outside the worktree. Nothing
    /// outside is touched; the link itself is removable.
    #[test]
    fn a_directory_swapped_for_a_symlink_never_steers_a_removal_outside() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("wt");
        let outside = dir.path().join("home");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(outside.join(".ssh")).expect("mkdir");
        std::fs::write(outside.join(".ssh/authorized_keys"), "key\n").expect("write");
        std::os::unix::fs::symlink(&outside, root.join("d")).expect("link");
        let r = open_root(&root).expect("root");
        assert_eq!(
            remove(&r, Path::new("d/.ssh/authorized_keys")).expect("ran"),
            Removal::Gone
        );
        assert!(outside.join(".ssh/authorized_keys").is_file());
        assert_eq!(remove(&r, Path::new("d")).expect("ran"), Removal::Removed);
        assert!(root.join("d").symlink_metadata().is_err(), "the link went");
        assert!(
            outside.join(".ssh/authorized_keys").is_file(),
            "not its target"
        );
    }

    /// A write never goes through a symlinked directory; a link at the target
    /// is replaced by a file and its target is untouched; new folders are made.
    #[test]
    fn a_confined_write_refuses_a_symlinked_folder_and_a_final_link() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("tree");
        let outside = dir.path().join("home");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::os::unix::fs::symlink(&outside, root.join("tests")).expect("link");
        let e = write_confined(&root, Path::new("tests/contract.py"), b"x")
            .expect_err("through a folder link");
        assert!(e.to_string().contains("symbolic link tests"), "{e}");
        assert!(!outside.join("contract.py").exists(), "nothing outside");

        // Decision 0083: a leaf link is refused, naming it; neither its
        // target nor the link itself changes.
        std::fs::write(outside.join("target"), "keep\n").expect("write");
        std::os::unix::fs::symlink(outside.join("target"), root.join("f.py")).expect("link");
        let e = write_confined(&root, Path::new("f.py"), b"new").expect_err("leaf link");
        assert!(e.to_string().contains("f.py is a symbolic link"), "{e}");
        assert_eq!(
            std::fs::read_to_string(outside.join("target")).expect("read"),
            "keep\n"
        );
        assert!(
            root.join("f.py")
                .symlink_metadata()
                .expect("meta")
                .is_symlink(),
            "the link is left for whoever planted it"
        );
        std::os::unix::fs::symlink(outside.join("absent"), root.join("g.py")).expect("link");
        assert!(write_confined(&root, Path::new("g.py"), b"new").is_err());
        assert!(
            !outside.join("absent").exists(),
            "a dangling link creates nothing"
        );

        write_confined(&root, Path::new("a/b/c.py"), b"deep").expect("write");
        assert_eq!(
            std::fs::read_to_string(root.join("a/b/c.py")).expect("read"),
            "deep"
        );
        assert!(write_confined(&root, Path::new("../x"), b"x").is_err());
    }

    /// Decision 0083: a directory link and a leaf link, both aimed outside a
    /// temp root, planted in the tree. An append never goes through either;
    /// a dangling leaf link never creates its target.
    #[test]
    fn an_append_refuses_a_folder_link_and_a_leaf_link_and_never_creates_outside() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("tree");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(root.join("docs")).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::os::unix::fs::symlink("../../outside", root.join("docs/shared-notes")).expect("link");
        let e = open_append_confined(&root, Path::new("docs/shared-notes/a--b.md"))
            .expect_err("through a folder link");
        assert!(
            e.to_string().contains("symbolic link docs/shared-notes"),
            "{e}"
        );
        assert!(!outside.join("a--b.md").exists(), "nothing outside");

        std::fs::write(outside.join("rc"), "keep\n").expect("write");
        std::os::unix::fs::symlink(outside.join("rc"), root.join("docs/rc.md")).expect("link");
        let e = open_append_confined(&root, Path::new("docs/rc.md")).expect_err("leaf link");
        assert!(e.to_string().contains("symbolic link"), "{e}");
        assert_eq!(
            std::fs::read_to_string(outside.join("rc")).expect("read"),
            "keep\n"
        );
        std::os::unix::fs::symlink(outside.join("new"), root.join("docs/dangling.md"))
            .expect("link");
        assert!(open_append_confined(&root, Path::new("docs/dangling.md")).is_err());
        assert!(
            !outside.join("new").exists(),
            "a dangling link creates nothing"
        );

        use std::io::Write;
        let (mut f, fresh) = open_append_confined(&root, Path::new("a/b.md")).expect("new");
        assert!(fresh);
        f.write_all(b"one\n").expect("write");
        drop(f);
        let (mut f, fresh) = open_append_confined(&root, Path::new("a/b.md")).expect("again");
        assert!(!fresh);
        f.write_all(b"two\n").expect("write");
        assert_eq!(
            std::fs::read_to_string(root.join("a/b.md")).expect("read"),
            "one\ntwo\n"
        );
    }

    /// A hard link whose other name is outside the tree is never written, and
    /// a FIFO is refused at once rather than blocking the host.
    #[test]
    fn a_hard_linked_file_and_a_fifo_are_refused_and_left_as_they_were() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("tree");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("keys"), "keep\n").expect("write");
        std::fs::hard_link(outside.join("keys"), root.join("keys")).expect("link");
        let e = write_confined(&root, Path::new("keys"), b"pwned").expect_err("hard link");
        assert!(e.to_string().contains("hard links"), "{e}");
        assert!(open_append_confined(&root, Path::new("keys")).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.join("keys")).expect("read"),
            "keep\n",
            "not truncated, not appended"
        );

        let fifo = root.join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("cstr");
        // SAFETY: mkfifo on a path in a temp dir this test owns.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert!(write_confined(&root, Path::new("fifo"), b"x").is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// The mode is set on the descriptor: the executable bit lands on the file
    /// written, and a folder link on the way is refused before any chmod.
    #[test]
    fn the_executable_bit_is_set_on_the_file_written_never_through_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("tree");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("run.sh"), "keep\n").expect("write");
        std::fs::set_permissions(
            outside.join("run.sh"),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
        std::os::unix::fs::symlink(&outside, root.join("bin")).expect("link");
        assert!(write_confined_mode(&root, Path::new("bin/run.sh"), b"x", Some(true)).is_err());
        let mode = std::fs::metadata(outside.join("run.sh"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "outside mode unchanged");

        write_confined_mode(&root, Path::new("tools/run.sh"), b"#!/bin/sh\n", Some(true))
            .expect("write");
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode();
        assert_ne!(mode(&root.join("tools/run.sh")) & 0o111, 0);
        write_confined_mode(&root, Path::new("tools/run.sh"), b"x", Some(false)).expect("write");
        assert_eq!(mode(&root.join("tools/run.sh")) & 0o111, 0);
    }

    #[test]
    fn a_confined_removal_by_path_never_follows_a_folder_link() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("tree");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("f"), "keep\n").expect("write");
        std::os::unix::fs::symlink(&outside, root.join("gen")).expect("link");
        assert!(!remove_confined(&root, Path::new("gen/f")).expect("ran"));
        assert!(outside.join("f").is_file());
        std::fs::write(root.join("x"), "x").expect("write");
        assert!(remove_confined(&root, Path::new("x")).expect("ran"));
        assert!(!root.join("x").exists());
    }

    #[test]
    fn a_directory_is_removed_whole_without_following_links_inside() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("wt");
        let outside = dir.path().join("keep");
        std::fs::create_dir_all(root.join("sub/.git/objects")).expect("mkdir");
        std::fs::create_dir_all(root.join("sub/a/b/c")).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("f"), "keep\n").expect("write");
        std::fs::write(root.join("sub/.git/HEAD"), "ref\n").expect("write");
        std::fs::write(root.join("sub/a/b/c/x"), "x").expect("write");
        std::os::unix::fs::symlink(&outside, root.join("sub/link")).expect("link");
        std::os::unix::fs::symlink(&outside, root.join("sub/a/b/dirlink")).expect("link");
        let r = open_root(&root).expect("root");
        assert_eq!(remove(&r, Path::new("sub")).expect("ran"), Removal::Removed);
        assert!(!root.join("sub").exists());
        assert!(outside.join("f").is_file());
    }

    #[test]
    fn empty_parents_go_and_the_root_stays() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("wt");
        std::fs::create_dir_all(root.join("a/b")).expect("mkdir");
        std::fs::create_dir_all(root.join("keep")).expect("mkdir");
        std::fs::write(root.join("a/b/c"), "x").expect("write");
        std::fs::write(root.join("keep/k"), "x").expect("write");
        let r = open_root(&root).expect("root");
        assert_eq!(
            remove(&r, Path::new("a/b/c")).expect("ran"),
            Removal::Removed
        );
        assert!(!root.join("a").exists());
        assert!(root.join("keep/k").is_file());
        assert!(root.is_dir());
        assert!(remove(&r, Path::new("../x")).is_err());
        assert!(remove(&r, Path::new("")).is_err());
    }

    /// A nested repository with a chain of folders 20,000 levels
    /// deep, built with `mkdirat` relative to each level (no path is ever that
    /// long). The recursive walk overflowed a 2 MiB stack at about 3,500 levels
    /// and killed the process; now the walk is refused with an error at
    /// [`MAX_DEPTH`], on this test thread's own small stack.
    #[test]
    fn a_folder_chain_20000_levels_deep_is_refused_not_a_stack_overflow() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("wt");
        std::fs::create_dir_all(&root).expect("mkdir");
        // Built bottom up with shallow operations only: wrap the chain in a
        // new top folder, 20,000 times. A top-down `mkdirat` at the tip gets
        // slower with depth on APFS; this stays constant per level.
        std::fs::create_dir(root.join("chain")).expect("mkdir");
        for _ in 0..20_000 {
            std::fs::create_dir(root.join("top")).expect("mkdir");
            std::fs::rename(root.join("chain"), root.join("top/d")).expect("wrap");
            std::fs::rename(root.join("top"), root.join("chain")).expect("rename");
        }
        std::fs::create_dir(root.join("n")).expect("mkdir");
        std::fs::create_dir(root.join("n/.git")).expect("mkdir");
        std::fs::rename(root.join("chain/d"), root.join("n/d")).expect("move");
        std::fs::remove_dir(root.join("chain")).expect("rmdir");
        let r = open_root(&root).expect("root");
        let err = remove(&r, Path::new("n")).expect_err("refused");
        assert!(err.to_string().contains("folder levels deep"), "{err}");
        let long: std::path::PathBuf = std::iter::repeat_n("d", MAX_DEPTH + 1).collect();
        let err = remove(&r, &long).expect_err("refused");
        assert!(err.to_string().contains("folder levels deep"), "{err}");
        remove_chain(&root);
    }

    /// Take the chain down one level at a time by moving `n/d` up to `n`, so
    /// no path is ever long; `tempdir`'s own removal would walk it whole.
    fn remove_chain(root: &Path) {
        std::fs::remove_dir(root.join("n/.git")).expect("rmdir .git");
        loop {
            let inner = root.join("n/d");
            if inner.symlink_metadata().is_err() {
                std::fs::remove_dir(root.join("n")).expect("rmdir n");
                return;
            }
            std::fs::rename(&inner, root.join("m")).expect("move up");
            std::fs::remove_dir(root.join("n")).expect("rmdir n");
            std::fs::rename(root.join("m"), root.join("n")).expect("rename");
        }
    }
}

/// What holds on every platform: the plain cases, and a hard link (which
/// needs no privilege on Unix or Windows).
#[cfg(test)]
mod portable_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn an_append_makes_its_folders_reports_a_new_file_and_appends() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("tree");
        std::fs::create_dir_all(&root).expect("mkdir");
        let rel = Path::new("docs/shared-notes/a--b.md");
        let (mut f, fresh) = open_append_confined(&root, rel).expect("new");
        assert!(fresh, "a new file is fresh");
        f.write_all(b"one\n").expect("write");
        f.sync_data().expect("sync");
        drop(f);
        let (mut f, fresh) = open_append_confined(&root, rel).expect("again");
        assert!(!fresh, "a file with content is not fresh");
        f.write_all(b"two\n").expect("write");
        drop(f);
        assert_eq!(
            std::fs::read_to_string(root.join(rel)).expect("read"),
            "one\ntwo\n"
        );
    }

    #[test]
    fn a_write_replaces_the_content_and_refuses_paths_that_leave_the_root() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("tree");
        std::fs::create_dir_all(&root).expect("mkdir");
        write_confined(&root, Path::new("a/b/c.txt"), b"a long first version").expect("w1");
        write_confined(&root, Path::new("a/b/c.txt"), b"short").expect("w2");
        assert_eq!(
            std::fs::read_to_string(root.join("a/b/c.txt")).expect("read"),
            "short"
        );
        assert!(write_confined(&root, Path::new("../x"), b"x").is_err());
        assert!(write_confined(&root, Path::new(""), b"x").is_err());
        assert!(open_append_confined(&root, Path::new("../x")).is_err());
        assert!(!dir.path().join("x").exists(), "nothing beside the root");
        let e = write_confined(&root, Path::new("a"), b"x").expect_err("a folder");
        assert!(e.to_string().contains("directory"), "{e}");
    }

    #[test]
    fn a_hard_linked_file_is_neither_written_nor_appended_to() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("tree");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("keys"), "keep\n").expect("write");
        std::fs::hard_link(outside.join("keys"), root.join("keys")).expect("link");
        let e = write_confined(&root, Path::new("keys"), b"pwned").expect_err("write");
        assert!(e.to_string().contains("hard links"), "{e}");
        let e = open_append_confined(&root, Path::new("keys")).expect_err("append");
        assert!(e.to_string().contains("hard links"), "{e}");
        assert_eq!(
            std::fs::read_to_string(outside.join("keys")).expect("read"),
            "keep\n",
            "not truncated, not appended"
        );
    }

    /// A nested repository with a read-only object file (git makes them
    /// read-only) is removed whole; its empty parents go, the root stays.
    #[test]
    fn a_folder_with_a_read_only_file_is_removed_and_empty_parents_go() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("wt");
        std::fs::create_dir_all(root.join("x/sub/.git/objects/ab")).expect("mkdir");
        std::fs::create_dir_all(root.join("keep")).expect("mkdir");
        std::fs::write(root.join("keep/k"), "k").expect("write");
        let obj = root.join("x/sub/.git/objects/ab/cdef");
        std::fs::write(&obj, "blob").expect("write");
        let mut perms = std::fs::metadata(&obj).expect("meta").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&obj, perms).expect("chmod");
        assert!(remove_confined(&root, Path::new("x/sub")).expect("ran"));
        assert!(!root.join("x").exists(), "the emptied parent went too");
        assert!(root.join("keep/k").is_file());
        assert!(root.is_dir());
        assert!(!remove_confined(&root, Path::new("absent/f")).expect("ran"));
        assert!(remove_confined(&root, Path::new("../wt")).is_err());
    }
}

/// The listing parser and the name comparison of the Windows walk, which are
/// pure, run on every platform with Windows-shaped inputs.
#[cfg(test)]
mod nt_data_tests {
    use super::nt_data::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// A `FILE_FULL_DIR_INFO` buffer: each record at its offset, with its
    /// `NextEntryOffset` and name. Held in u64 words, as the system's buffer
    /// is, so the start is 8-aligned and a misaligned record is misaligned
    /// in memory too.
    struct Listing {
        words: Vec<u64>,
    }

    impl Listing {
        fn new(len: usize) -> Self {
            Listing {
                words: vec![0; len.div_ceil(8)],
            }
        }

        fn put(&mut self, at: usize, next: u32, name: &str) {
            let mut bytes: Vec<u8> = self.words.iter().flat_map(|w| w.to_ne_bytes()).collect();
            let units = wide(name);
            bytes[at + NEXT_ENTRY_OFFSET..at + NEXT_ENTRY_OFFSET + 4]
                .copy_from_slice(&next.to_ne_bytes());
            let len = (units.len() * 2) as u32;
            bytes[at + FILE_NAME_LENGTH..at + FILE_NAME_LENGTH + 4]
                .copy_from_slice(&len.to_ne_bytes());
            for (i, u) in units.iter().enumerate() {
                let o = at + FILE_NAME + i * 2;
                bytes[o..o + 2].copy_from_slice(&u.to_ne_bytes());
            }
            self.words = bytes
                .chunks_exact(8)
                .map(|c| u64::from_ne_bytes(c.try_into().expect("8 bytes")))
                .collect();
        }

        /// The first `len` bytes, as one slice of the aligned buffer.
        fn parse(&self, len: usize) -> std::io::Result<Vec<String>> {
            assert!(len <= self.words.len() * 8);
            // SAFETY: a view of the u64 words as bytes (u8 has no alignment
            // need); `len` is at most their size (asserted above), and the
            // words outlive the borrow.
            let bytes =
                unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), len) };
            let mut out = Vec::new();
            full_dir_names(bytes, &mut out)?;
            Ok(out.iter().map(|n| String::from_utf16_lossy(n)).collect())
        }
    }

    #[test]
    fn a_well_formed_listing_gives_its_names_without_the_dot_entries() {
        let mut l = Listing::new(512);
        l.put(0, 72, ".");
        l.put(72, 72, "..");
        // 68 + 30 bytes of name, so the next record starts at 144 + 104.
        l.put(144, 104, "a-long-file.txt");
        l.put(248, 0, "src");
        assert_eq!(l.parse(512).expect("parsed"), ["a-long-file.txt", "src"]);
    }

    /// A `NextEntryOffset` that is not a multiple of 8 puts the next record
    /// at a misaligned address; it is refused, never read as a typed record.
    #[test]
    fn a_misaligned_next_record_is_refused() {
        let mut l = Listing::new(512);
        l.put(0, 76, "a");
        l.put(76, 0, "b");
        let e = l.parse(512).expect_err("misaligned");
        assert!(e.to_string().contains("not 8-aligned"), "{e}");
    }

    /// The last record's 68-byte header and its name fit, but the record
    /// type (72 bytes with padding) does not: refused, never read past the
    /// buffer.
    #[test]
    fn a_record_shorter_than_its_type_at_the_end_is_refused() {
        let mut l = Listing::new(152);
        l.put(0, 72, "a");
        l.put(72, 0, "b");
        let e = l.parse(72 + 70).expect_err("short");
        assert!(e.to_string().contains("overran"), "{e}");
    }

    #[test]
    fn a_name_past_the_buffer_and_an_overlapping_next_record_are_refused() {
        let mut l = Listing::new(256);
        l.put(0, 0, "a-name-longer-than-what-is-left-of-the-buffer-here");
        assert!(
            l.parse(80)
                .expect_err("name")
                .to_string()
                .contains("overran")
        );

        let mut l = Listing::new(160);
        l.put(0, 8, "abc");
        let e = l.parse(160).expect_err("inside");
        assert!(e.to_string().contains("inside itself"), "{e}");

        let mut l = Listing::new(160);
        l.put(0, 1u32 << 31, "a");
        assert!(l.parse(160).is_err(), "a next record far past the end");
    }

    #[test]
    fn the_last_component_of_an_nt_path() {
        assert_eq!(
            last_component(&wide(r"\Users\me\tree\.gitattributes")),
            wide(".gitattributes").as_slice()
        );
        assert_eq!(last_component(&wide("plain")), wide("plain").as_slice());
    }

    /// A short (8.3) name reaches a file whose long name differs; the walk
    /// refuses it. A case-only difference is one name, as the protected-name
    /// check folds it.
    #[test]
    fn a_short_name_is_not_the_long_name_and_case_is_ignored() {
        let same = |a: &str, b: &str| same_name(&wide(a), &wide(b));
        assert!(!same("GITATT~1", ".gitattributes"));
        assert!(!same("GI7D29~1", ".gitattributes"));
        assert!(!same("A-LONG~1.TXT", "a-long-file.txt"));
        assert!(!same("KGRAF~1", ".kgraf"));
        assert!(same("README.md", "readme.MD"));
        assert!(same("Straße.txt", "STRASSE.TXT"));
        assert!(same("src", "src"));
        // Not valid UTF-16: only an exact match is the same name.
        assert!(same_name(&[0xD800], &[0xD800]));
        assert!(!same_name(&[0xD800], &[0xD801]));
    }
}

/// The Windows walk against real reparse points. A junction needs no
/// privilege; a symbolic link needs developer mode or an administrator, so
/// the link cases are skipped, with the reason printed, where it cannot be
/// made.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    struct Tree {
        _dir: tempfile::TempDir,
        root: std::path::PathBuf,
        outside: std::path::PathBuf,
    }

    fn tree() -> Tree {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("tree");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        Tree {
            _dir: dir,
            root,
            outside,
        }
    }

    fn outside_count(t: &Tree) -> usize {
        std::fs::read_dir(&t.outside).expect("list").count()
    }

    /// `docs` a junction to a folder outside: an append and a write through
    /// it are refused, naming the junction, and nothing is made outside.
    #[test]
    fn a_junction_on_the_path_is_refused_and_nothing_is_written_outside() {
        let t = tree();
        junction::create(&t.outside, t.root.join("docs")).expect("junction");
        let e = open_append_confined(&t.root, Path::new("docs/shared-notes/a--b.md"))
            .expect_err("through a junction");
        assert!(e.to_string().contains("crosses the junction docs"), "{e}");
        let e = write_confined(&t.root, Path::new("docs/x.md"), b"x").expect_err("write");
        assert!(e.to_string().contains("crosses the junction docs"), "{e}");
        assert_eq!(outside_count(&t), 0, "nothing was made outside");
    }

    /// A junction as the last component is refused, never written into.
    #[test]
    fn a_junction_as_the_last_component_is_refused() {
        let t = tree();
        std::fs::create_dir_all(t.root.join("docs")).expect("mkdir");
        junction::create(&t.outside, t.root.join("docs/a--b.md")).expect("junction");
        let e = open_append_confined(&t.root, Path::new("docs/a--b.md")).expect_err("leaf");
        assert!(e.to_string().contains("is a junction"), "{e}");
        assert_eq!(outside_count(&t), 0);
    }

    /// The race's effect without the race: a listed folder that is now a
    /// junction to a folder outside. Removal never enters it; removing the
    /// junction itself leaves its target whole. A junction inside a folder
    /// removed whole is removed itself, not followed.
    #[test]
    fn a_removal_never_follows_a_junction() {
        let t = tree();
        std::fs::write(t.outside.join("f"), "keep\n").expect("write");
        junction::create(&t.outside, t.root.join("gen")).expect("junction");
        assert!(!remove_confined(&t.root, Path::new("gen/f")).expect("ran"));
        assert!(t.outside.join("f").is_file());
        assert!(remove_confined(&t.root, Path::new("gen")).expect("ran"));
        assert!(
            t.root.join("gen").symlink_metadata().is_err(),
            "the junction went"
        );
        assert!(t.outside.join("f").is_file(), "not its target");

        std::fs::create_dir_all(t.root.join("sub/a")).expect("mkdir");
        std::fs::write(t.root.join("sub/a/x"), "x").expect("write");
        junction::create(&t.outside, t.root.join("sub/a/j")).expect("junction");
        assert!(remove_confined(&t.root, Path::new("sub")).expect("ran"));
        assert!(!t.root.join("sub").exists());
        assert!(t.outside.join("f").is_file(), "nothing removed through it");
    }

    /// Names `NtCreateFile` would make as given but no Windows program
    /// could open by path are refused before anything is made.
    #[test]
    fn names_windows_cannot_use_are_refused() {
        let t = tree();
        for bad in [
            "nul",
            "com1.txt",
            "a.md:stream",
            "trailing.",
            "trailing ",
            "x/NUL",
        ] {
            let e = write_confined(&t.root, Path::new(bad), b"x").expect_err(bad);
            assert!(e.to_string().contains("cannot use"), "{bad}: {e}");
        }
        assert_eq!(std::fs::read_dir(&t.root).expect("list").count(), 0);
    }

    /// Symbolic links, file and folder, live and dangling. Skipped where the
    /// account may not make one.
    #[test]
    fn a_symbolic_link_is_refused_where_one_can_be_made() {
        let t = tree();
        std::fs::write(t.outside.join("rc"), "keep\n").expect("write");
        if let Err(e) = std::os::windows::fs::symlink_file(t.outside.join("rc"), t.root.join("rc"))
        {
            eprintln!(
                "skipped: this account cannot make a symbolic link ({e}); developer mode or an \
                 administrator is needed"
            );
            return;
        }
        let e = open_append_confined(&t.root, Path::new("rc")).expect_err("file link");
        assert!(e.to_string().contains("is a symbolic link"), "{e}");
        let e = write_confined(&t.root, Path::new("rc"), b"x").expect_err("file link");
        assert!(e.to_string().contains("is a symbolic link"), "{e}");
        assert_eq!(
            std::fs::read_to_string(t.outside.join("rc")).expect("read"),
            "keep\n"
        );

        std::os::windows::fs::symlink_file(t.outside.join("new"), t.root.join("dangling"))
            .expect("dangling link");
        assert!(open_append_confined(&t.root, Path::new("dangling")).is_err());
        assert!(
            !t.outside.join("new").exists(),
            "a dangling link makes nothing"
        );

        std::os::windows::fs::symlink_dir(&t.outside, t.root.join("d")).expect("folder link");
        let e = open_append_confined(&t.root, Path::new("d/x.md")).expect_err("folder link");
        assert!(e.to_string().contains("crosses the symbolic link d"), "{e}");
        assert!(!t.outside.join("x.md").exists());
        assert!(remove_confined(&t.root, Path::new("d")).expect("ran"));
        assert!(
            t.outside.join("rc").is_file(),
            "the link went, not its target"
        );
    }

    /// Turn the empty folder `folder` into a junction to `target` in place,
    /// as any user who may write the folder can (`FSCTL_SET_REPARSE_POINT`
    /// with a mount point record). Handles already open on it stay open.
    fn make_junction_in_place(folder: &Path, target: &Path) {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::GENERIC_WRITE;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;
        use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
        use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;
        let target = kgraf_core::plain_path::canonicalize(target).expect("target");
        let print: Vec<u16> = target.as_os_str().encode_wide().collect();
        let substitute: Vec<u16> = r"\??\"
            .encode_utf16()
            .chain(print.iter().copied())
            .collect();
        let sub_bytes = (substitute.len() * 2) as u16;
        let print_bytes = (print.len() * 2) as u16;
        // The two names, each followed by a NUL.
        let path_bytes = sub_bytes + 2 + print_bytes + 2;
        let mut record: Vec<u8> = Vec::new();
        record.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        // ReparseDataLength: the four name fields, then the names.
        record.extend((8 + path_bytes).to_le_bytes());
        record.extend(0u16.to_le_bytes());
        record.extend(0u16.to_le_bytes());
        record.extend(sub_bytes.to_le_bytes());
        record.extend((sub_bytes + 2).to_le_bytes());
        record.extend(print_bytes.to_le_bytes());
        for unit in substitute.iter().chain(&[0]).chain(&print).chain(&[0]) {
            record.extend(unit.to_le_bytes());
        }
        let handle = std::fs::OpenOptions::new()
            .access_mode(GENERIC_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(folder)
            .expect("open the folder");
        let mut returned = 0u32;
        // SAFETY: `record` is readable for its length, as passed; no output
        // buffer and no overlapped I/O; `handle` outlives the call.
        let ok = unsafe {
            DeviceIoControl(
                handle.as_raw_handle() as _,
                FSCTL_SET_REPARSE_POINT,
                record.as_ptr().cast(),
                record.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            ok,
            0,
            "FSCTL_SET_REPARSE_POINT: {}",
            std::io::Error::last_os_error()
        );
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("list")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The walk holds the folder `a` open, as it does between two names; a
    /// worker then makes `a` a junction to a folder outside, in place. A
    /// create and an open of a child relative to the held handle are both
    /// refused, a new walk refuses `a`, and nothing appears in the target.
    #[test]
    fn a_held_folder_made_a_junction_in_place_is_never_resolved_through() {
        use std::ffi::OsStr;
        use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
        let t = tree();
        std::fs::write(t.outside.join("t"), "keep\n").expect("write");
        let root = open_root(&t.root).expect("root");
        let (held, _) = win::open_parent(&root, Path::new("a/x")).expect("walk to a");
        make_junction_in_place(&t.root.join("a"), &t.outside);

        let e = win::open_checked(&held, OsStr::new("child"), Path::new("a/child"), false)
            .expect_err("a create through the junction");
        eprintln!("create refused: {e}");
        let e = win::open_checked(&held, OsStr::new("t"), Path::new("a/t"), false)
            .expect_err("an open of a name in the target");
        eprintln!("open refused: {e}");
        assert!(
            win::nt_open(&held, OsStr::new("t"), 0, FILE_OPEN, 0).is_err(),
            "a bare relative open of a name in the target"
        );
        let e = open_append_confined(&t.root, Path::new("a/child")).expect_err("a new walk");
        assert!(e.to_string().contains("junction"), "{e}");

        assert_eq!(
            names_in(&t.outside),
            ["t"],
            "nothing was made in the target"
        );
        assert_eq!(
            std::fs::read_to_string(t.outside.join("t")).expect("read"),
            "keep\n"
        );
    }

    /// Another process has its current folder in `tree\a` (Windows holds it
    /// open without sharing deletion). Removing `a\b\c` opens `a` only to
    /// enter it, so the removal goes on: `c` and its emptied parent `b` go,
    /// and `a` stays, as the tidy-up stops at the folder it cannot delete.
    #[test]
    fn a_removal_goes_on_while_another_process_has_its_current_folder_on_the_path() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
        use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_BACKUP_SEMANTICS};
        let t = tree();
        std::fs::create_dir_all(t.root.join("a/b")).expect("mkdir");
        std::fs::write(t.root.join("a/b/c"), "x").expect("write");
        let mut child = std::process::Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .current_dir(t.root.join("a"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start a process in tree\\a");
        // Held once an open for deletion meets a sharing violation.
        let held = (0..100).any(|_| {
            let open = std::fs::OpenOptions::new()
                .access_mode(DELETE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(t.root.join("a"));
            match open {
                Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => true,
                _ => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    false
                }
            }
        });
        let removed = remove_confined(&t.root, Path::new("a/b/c"));
        let _ = child.kill();
        let _ = child.wait();
        assert!(held, "the process never held tree\\a as its current folder");
        assert!(removed.expect("removal with tree\\a held"), "removed");
        assert!(
            !t.root.join("a/b").exists(),
            "c and its emptied parent b went"
        );
        assert!(t.root.join("a").is_dir(), "a, held by the process, stays");
    }

    /// The short (8.3) name of `path`'s last component, or `None` when it
    /// has none (short names are off on that volume).
    fn short_name(path: &Path) -> Option<String> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
        let long: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut buf = vec![0u16; 32_768];
        // SAFETY: `long` is NUL-terminated; `buf` is writable for its length,
        // as passed.
        let n = unsafe { GetShortPathNameW(long.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        assert!(
            n > 0 && (n as usize) < buf.len(),
            "GetShortPathNameW: {}",
            std::io::Error::last_os_error()
        );
        let short = String::from_utf16_lossy(&buf[..n as usize]);
        let last = short.rsplit('\\').next().unwrap_or_default().to_owned();
        let name = path.file_name()?.to_string_lossy().into_owned();
        (!last.eq_ignore_ascii_case(&name)).then_some(last)
    }

    /// A short name reaches the file whose long name the caller's
    /// protected-name check never saw. A write, an append and a removal by
    /// it are refused and the file is left as it was, for a file and for a
    /// folder on the way. The long name in another case still works.
    #[test]
    fn a_short_name_alias_is_refused() {
        let t = tree();
        for long in [".gitattributes", "a-long-file-name.txt"] {
            std::fs::write(t.root.join(long), "keep\n").expect("write");
            let Some(short) = short_name(&t.root.join(long)) else {
                eprintln!(
                    "skipped: the volume of {} makes no short (8.3) names",
                    t.root.display()
                );
                return;
            };
            let e = write_confined(&t.root, Path::new(&short), b"x").expect_err(&short);
            assert!(e.to_string().contains("short (8.3) name"), "{short}: {e}");
            let e = open_append_confined(&t.root, Path::new(&short)).expect_err(&short);
            assert!(e.to_string().contains("short (8.3) name"), "{short}: {e}");
            let e = remove_confined(&t.root, Path::new(&short)).expect_err(&short);
            assert!(e.to_string().contains("short (8.3) name"), "{short}: {e}");
            assert_eq!(
                std::fs::read_to_string(t.root.join(long)).expect("read"),
                "keep\n",
                "{long} as it was"
            );
        }
        std::fs::create_dir_all(t.root.join("a-long-folder-name")).expect("mkdir");
        let short = short_name(&t.root.join("a-long-folder-name")).expect("a folder short name");
        let e = write_confined(&t.root, &Path::new(&short).join("f.txt"), b"x")
            .expect_err("through a folder's short name");
        assert!(e.to_string().contains("short (8.3) name"), "{e}");
        assert!(!t.root.join("a-long-folder-name/f.txt").exists());

        write_confined(&t.root, Path::new("A-LONG-FILE-NAME.TXT"), b"new").expect("case only");
        assert_eq!(
            std::fs::read_to_string(t.root.join("a-long-file-name.txt")).expect("read"),
            "new"
        );
    }

    /// Where POSIX deletion is missing, the classic delete clears the
    /// read-only attribute first. The attribute is the file's, so with a
    /// second hard link outside the tree that would change the outside file:
    /// refused, the attribute untouched. A read-only file with one link is
    /// deleted.
    #[test]
    fn the_classic_delete_never_clears_read_only_on_a_hard_linked_file() {
        use std::ffi::OsStr;
        use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN;
        let t = tree();
        let read_only = |p: &Path, on: bool| {
            let mut perms = std::fs::metadata(p).expect("meta").permissions();
            perms.set_readonly(on);
            std::fs::set_permissions(p, perms).expect("chmod");
        };
        std::fs::write(t.outside.join("keys"), "keep\n").expect("write");
        read_only(&t.outside.join("keys"), true);
        std::fs::hard_link(t.outside.join("keys"), t.root.join("keys")).expect("link");
        let root = open_root(&t.root).expect("root");
        let f = win::nt_open(&root, OsStr::new("keys"), win::REMOVE_ACCESS, FILE_OPEN, 0)
            .map_err(win::nt_error)
            .expect("open");
        let e = win::delete_classic(&f, Path::new("keys")).expect_err("two links");
        assert!(e.to_string().contains("hard links"), "{e}");
        drop(f);
        assert!(
            std::fs::metadata(t.outside.join("keys"))
                .expect("meta")
                .permissions()
                .readonly(),
            "the outside file stays read-only"
        );
        assert!(t.root.join("keys").is_file(), "not removed");

        std::fs::write(t.root.join("one"), "x").expect("write");
        read_only(&t.root.join("one"), true);
        let f = win::nt_open(&root, OsStr::new("one"), win::REMOVE_ACCESS, FILE_OPEN, 0)
            .map_err(win::nt_error)
            .expect("open");
        win::delete_classic(&f, Path::new("one")).expect("one link");
        drop(f);
        assert!(!t.root.join("one").exists());
        read_only(&t.outside.join("keys"), false);
    }

    /// An open Windows rejects as an invalid parameter (an older release
    /// without `OBJ_DONT_REPARSE`) is an error naming the reason, never an
    /// open without the flag.
    #[test]
    fn an_open_without_obj_dont_reparse_support_fails_loud() {
        use windows_sys::Win32::Foundation::STATUS_INVALID_PARAMETER;
        let e = win::nt_error(STATUS_INVALID_PARAMETER);
        assert_eq!(e.kind(), std::io::ErrorKind::Unsupported);
        assert!(e.to_string().contains("OBJ_DONT_REPARSE"), "{e}");
    }
}
