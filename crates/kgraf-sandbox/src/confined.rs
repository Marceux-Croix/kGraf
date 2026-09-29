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
//! **Windows: refused.** On Windows cap-primitives resolves each step by
//! rebuilding a path string from the parent handle, so a directory swapped for
//! a junction between steps can still steer a removal outside the worktree.
//! Until that is fixed there, [`remove`] refuses on Windows (fail loud)
//! instead of removing unsafely; a check's leftovers then fail verification.
//!
//! Descriptor-relative opens and unlinks are cap-primitives' (see the
//! dependency note in Cargo.toml).

use cap_primitives::fs::open_ambient_dir;
use cap_primitives::{AmbientAuthority, ambient_authority};
use std::fs::File;
use std::io;
use std::path::Path;

/// Deepest a removal walks: path components of a listed path, or folder
/// levels inside a listed directory.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) const MAX_DEPTH: usize = 100;

/// What [`remove`] did with one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))]
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
#[cfg(windows)]
pub(crate) fn remove(_root: &File, rel: &Path) -> io::Result<Removal> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "cannot remove {} safely on Windows: descriptor-relative removal is not yet \
             junction-safe there, so the worktree is left as it is",
            rel.display()
        ),
    ))
}

/// See the Windows variant for the contract.
#[cfg(not(windows))]
pub(crate) fn remove(root: &File, rel: &Path) -> io::Result<Removal> {
    unix::remove(root, rel)
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
/// it. `rel` must be relative with plain components.
///
/// The file is opened without truncation and non-blocking, then checked on the
/// open descriptor: anything but a regular file (a FIFO would block the host
/// forever) and a regular file with more than one hard link (its other name
/// may be outside the tree) are refused. Only then is it emptied and written.
///
/// Refused on Windows for the reason [`remove`] is: cap-primitives resolves
/// each step there by rebuilding a path string, so a junction can steer it.
pub fn write_confined(root: &Path, rel: &Path, content: &[u8]) -> io::Result<()> {
    write_confined_mode(root, rel, content, None)
}

/// [`write_confined`], then the executable bits set (`Some(true)`) or cleared
/// (`Some(false)`) on the open descriptor (`fchmod`), never through the path.
/// `None` leaves the mode as created or found.
#[cfg(windows)]
pub fn write_confined_mode(
    _root: &Path,
    rel: &Path,
    _content: &[u8],
    _executable: Option<bool>,
) -> io::Result<()> {
    Err(windows_refusal("write", rel))
}

/// See the Windows variant for the contract.
#[cfg(not(windows))]
pub fn write_confined_mode(
    root: &Path,
    rel: &Path,
    content: &[u8],
    executable: Option<bool>,
) -> io::Result<()> {
    unix::write(&open_root(root)?, rel, content, executable)
}

/// Open the regular file `rel` beneath `root` for appending, from the host,
/// creating it and its folders when absent. Returns the file and whether it
/// was empty when opened (a new file wants its header first).
///
/// Every component, the last included, is opened with no-follow: a symbolic
/// link anywhere on the path is refused with an error naming it, as
/// [`write_confined`] refuses one. A non-regular or hard-linked file is
/// refused as there.
#[cfg(windows)]
pub fn open_append_confined(_root: &Path, rel: &Path) -> io::Result<(File, bool)> {
    Err(windows_refusal("append to", rel))
}

/// See the Windows variant for the contract.
#[cfg(not(windows))]
pub fn open_append_confined(root: &Path, rel: &Path) -> io::Result<(File, bool)> {
    unix::open_append(&open_root(root)?, rel)
}

/// Remove `rel` beneath `root` from the host, never following a symbolic
/// link on the way ([`remove`]): a file or a link is unlinked, a directory is
/// removed with everything in it. `Ok(false)`: nothing was there as a chain
/// of real directories, so nothing was removed. Refused on Windows.
pub fn remove_confined(root: &Path, rel: &Path) -> io::Result<bool> {
    Ok(remove(&open_root(root)?, rel)? == Removal::Removed)
}

#[cfg(windows)]
fn windows_refusal(what: &str, rel: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "cannot {what} {} safely on Windows: descriptor-relative access is not yet \
             junction-safe there",
            rel.display()
        ),
    )
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
