//! Dependency links made by path, for Windows, where a symbolic link needs a
//! privilege a user account lacks by default: a folder is reached through a
//! directory junction (no privilege needed), a file is copied. The logic is
//! written against [`LinkOps`] so the same code runs on Windows with
//! junctions ([`Junctions`]) and in this crate's tests on macOS and Linux
//! with symbolic links.
//!
//! **Why by path.** The Unix code makes every link relative to a folder
//! opened without following links (`openat`); the Windows API has no such
//! call in this crate's dependencies. Instead every folder on the way is
//! checked to be a real folder (not a junction or a link) right before the
//! link is made, the link or copy is made only where nothing is (a copy
//! opens its file with `create_new`, which never follows a link), and the
//! made entry's folder is resolved again afterwards: one that left the tree
//! (a folder swapped for a junction by a parallel command of the worker) is
//! removed again and reported. A swap in the moment between the check and
//! the creation can still place one junction or copy outside the tree for
//! that moment; stated, not hidden.
//!
//! **Read-only on Windows.** A junction target is judged by its own ACL:
//! the sandbox's restricted token writes only where a writable root's
//! capability SID is granted, and runs at Low integrity, which cannot write
//! the user's Medium files. The checkout's installs have neither, so a write
//! through a junction is refused while reads (not confined on Windows) pass.
//! Not run on Windows here: compile-checked with `cargo xwin clippy`.

// Used on Windows only outside the tests (as in `sandbox/windows.rs`).
#![cfg_attr(not(windows), allow(dead_code))]

use std::io;
use std::path::{Component, Path, PathBuf};

use super::{Linked, MIRROR_MARK, MirrorEntry};

/// The link operations the by-path code needs.
pub(crate) trait LinkOps {
    /// Whether `path` is a link of any kind (a junction or a symbolic link):
    /// never entered, never followed.
    fn is_link(&self, path: &Path) -> bool;
    /// Where the link at `path` points.
    fn link_target(&self, path: &Path) -> Option<PathBuf>;
    /// Make `at` a link to the folder `target` (absolute).
    fn link_dir(&self, target: &Path, at: &Path) -> io::Result<()>;
    /// Remove the link at `at` itself.
    fn remove_link(&self, at: &Path) -> io::Result<()>;
}

/// Directory junctions (Windows).
#[cfg(windows)]
pub(crate) struct Junctions;

#[cfg(windows)]
impl LinkOps for Junctions {
    fn is_link(&self, path: &Path) -> bool {
        use std::os::windows::fs::MetadataExt;
        const REPARSE: u32 = 0x400; // FILE_ATTRIBUTE_REPARSE_POINT
        std::fs::symlink_metadata(path).is_ok_and(|m| m.file_attributes() & REPARSE != 0)
    }
    fn link_target(&self, path: &Path) -> Option<PathBuf> {
        junction::get_target(path)
            .ok()
            .or_else(|| std::fs::read_link(path).ok())
    }
    fn link_dir(&self, target: &Path, at: &Path) -> io::Result<()> {
        junction::create(target, at)
    }
    fn remove_link(&self, at: &Path) -> io::Result<()> {
        // A junction is a folder entry: `remove_dir` removes it, never what
        // it points at.
        std::fs::remove_dir(at)
    }
}

/// Symbolic links, for the tests of this module on macOS and Linux.
#[cfg(all(test, unix))]
pub(crate) struct Symlinks;

#[cfg(all(test, unix))]
impl LinkOps for Symlinks {
    fn is_link(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
    }
    fn link_target(&self, path: &Path) -> Option<PathBuf> {
        std::fs::read_link(path).ok()
    }
    fn link_dir(&self, target: &Path, at: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, at)
    }
    fn remove_link(&self, at: &Path) -> io::Result<()> {
        std::fs::remove_file(at)
    }
}

/// `path` with `.` and `..` folded away, without asking the file system.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether every folder from `tree` down to `rel`'s parent is a real folder
/// (not a link, not absent).
fn parents_are_real<O: LinkOps>(ops: &O, tree: &Path, rel: &Path) -> io::Result<bool> {
    let mut at = tree.to_path_buf();
    let names: Vec<_> = rel.components().collect();
    let Some((_, parents)) = names.split_last() else {
        return Err(io::Error::other("an empty path"));
    };
    for c in parents {
        let Component::Normal(name) = c else {
            return Err(io::Error::other("not a plain relative path"));
        };
        at.push(name);
        let real = std::fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir()) && !ops.is_link(&at);
        if !real {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether `made`'s folder still resolves inside `tree` (canonical).
fn still_inside(tree: &Path, made: &Path) -> bool {
    let Some(parent) = made.parent() else {
        return false;
    };
    match (std::fs::canonicalize(parent), std::fs::canonicalize(tree)) {
        (Ok(p), Ok(t)) => p.starts_with(t),
        _ => false,
    }
}

/// Copy the file `from` to `to`, which must not exist: opened with
/// `create_new`, so a link put at `to` is never followed.
fn copy_new(from: &Path, to: &Path) -> io::Result<()> {
    let mut src = std::fs::File::open(from)?;
    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    io::copy(&mut src, &mut dst).map(drop)
}

/// Make `at` (absent) a link to the folder `target`, or a copy of the file
/// `target`; then make sure it is still inside `tree`, else remove it again.
fn make<O: LinkOps>(ops: &O, tree: &Path, target: &Path, at: &Path) -> io::Result<()> {
    let is_dir = std::fs::metadata(target)?.is_dir();
    if is_dir {
        ops.link_dir(target, at)?;
    } else {
        copy_new(target, at)?;
    }
    if !still_inside(tree, at) {
        let _ = if is_dir {
            ops.remove_link(at)
        } else {
            std::fs::remove_file(at)
        };
        return Err(io::Error::other(format!(
            "a folder above {} left the tree while the link was made",
            at.display()
        )));
    }
    Ok(())
}

/// The by-path form of the Unix `link`: make `rel` beneath `tree` a link to
/// the folder `source`, or a copy of the file `source`, where nothing is.
/// A link already there to `source` is kept; anything else is left.
pub(crate) fn link<O: LinkOps>(
    ops: &O,
    tree: &Path,
    rel: &Path,
    source: &Path,
) -> io::Result<Linked> {
    if !parents_are_real(ops, tree, rel)? {
        return Ok(Linked::NoParent);
    }
    let at = tree.join(rel);
    match std::fs::symlink_metadata(&at) {
        Ok(_) if ops.is_link(&at) => {
            let same = ops
                .link_target(&at)
                .and_then(|t| std::fs::canonicalize(t).ok())
                .is_some_and(|t| t == source);
            Ok(if same { Linked::Kept } else { Linked::Present })
        }
        Ok(_) => Ok(Linked::Present),
        Err(e) if e.kind() == io::ErrorKind::NotFound => match make(ops, tree, source, &at) {
            Ok(()) => Ok(Linked::Made),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(Linked::Present),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    }
}

/// Whether `rel` beneath `tree` is kGraf's folder of links whose mark holds
/// `stamp`, reached without following a link.
pub(crate) fn mirror_is_current<O: LinkOps>(ops: &O, tree: &Path, rel: &Path, stamp: &str) -> bool {
    let at = tree.join(rel);
    let mark = at.join(MIRROR_MARK);
    parents_are_real(ops, tree, rel).unwrap_or(false)
        && std::fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir())
        && !ops.is_link(&at)
        && std::fs::symlink_metadata(&mark).is_ok_and(|m| m.is_file())
        && !ops.is_link(&mark)
        && std::fs::read_to_string(&mark).is_ok_and(|t| t == stamp)
}

/// The by-path form of the Unix `mirror`: make `rel` beneath `tree` a
/// folder of links following `plan` (`super::mirror_plan`), or bring
/// kGraf's own such folder up to date, then write `stamp` in its mark. A
/// relative link text is resolved from the link's own folder in the tree
/// (a junction needs an absolute target); a folder target becomes a
/// junction, a file target a copy (npm's `.bin` holds `.cmd` and `.ps1`
/// files on Windows, which find their package through the junction beside
/// them).
pub(crate) fn mirror<O: LinkOps>(
    ops: &O,
    tree: &Path,
    rel: &Path,
    source: &Path,
    plan: &[MirrorEntry],
    stamp: &str,
) -> io::Result<Linked> {
    if !parents_are_real(ops, tree, rel)? {
        return Ok(Linked::NoParent);
    }
    let at = tree.join(rel);
    let made = match std::fs::symlink_metadata(&at) {
        Ok(_) if ops.is_link(&at) => {
            let ours = ops
                .link_target(&at)
                .and_then(|t| std::fs::canonicalize(t).ok())
                .is_some_and(|t| t == source);
            if !ours {
                return Ok(Linked::Present);
            }
            // kGraf's own link from before the install had workspace
            // packages: replaced by the folder.
            ops.remove_link(&at)?;
            true
        }
        Ok(meta) if meta.is_dir() => false,
        Ok(_) => return Ok(Linked::Present),
        Err(e) if e.kind() == io::ErrorKind::NotFound => true,
        Err(e) => return Err(e),
    };
    let mark = at.join(MIRROR_MARK);
    if made {
        match std::fs::create_dir(&at) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    if ops.is_link(&at) || !still_inside(tree, &mark) {
        return Ok(Linked::Present);
    }
    if !made && !std::fs::symlink_metadata(&mark).is_ok_and(|m| m.is_file()) {
        // A folder that is not kGraf's (the worker's own install).
        return Ok(Linked::Present);
    }
    for entry in plan {
        match entry {
            MirrorEntry::Dir(name) => {
                let dir = at.join(name);
                match std::fs::create_dir(&dir) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
            }
            MirrorEntry::Link(name, text) => {
                let dest = at.join(name);
                if std::fs::symlink_metadata(&dest).is_ok() {
                    continue;
                }
                // Its folder must be a real folder of this mirror.
                if !parents_are_real(ops, tree, &rel.join(name))? {
                    continue;
                }
                let target = if text.is_relative() {
                    normalize(&dest.parent().unwrap_or(&at).join(text))
                } else {
                    text.clone()
                };
                match make(ops, tree, &target, &dest) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    // A workspace package or program the tree does not have.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }
    // The stamp last: a call cut short leaves an old stamp, and the next call
    // completes the folder. Any mark there (a link a command left) is
    // removed itself first; the new one is made with `create_new`.
    match std::fs::remove_file(&mark) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&mark)?;
    io::Write::write_all(&mut file, stamp.as_bytes())?;
    Ok(if made { Linked::Made } else { Linked::Kept })
}

/// Make each absent folder of `rel` beneath `root`; every folder on the way
/// must be a real folder, never a link. For the Gradle wrapper's folders in
/// a command's temporary folder on Windows.
pub(crate) fn make_dirs<O: LinkOps>(ops: &O, root: &Path, rel: &Path) -> io::Result<PathBuf> {
    let mut at = root.to_path_buf();
    for c in rel.components() {
        let Component::Normal(name) = c else {
            return Err(io::Error::other("not a plain relative path"));
        };
        at.push(name);
        match std::fs::create_dir(&at) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        if ops.is_link(&at) || !std::fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir()) {
            return Err(io::Error::other(format!(
                "{} is not a real folder",
                at.display()
            )));
        }
    }
    Ok(at)
}

/// Make `name` in the real folder `dir` a link to the folder `target`;
/// whatever else was there (a file or a link an earlier command left) is
/// replaced, a real folder is an error.
pub(crate) fn replace_with_link<O: LinkOps>(
    ops: &O,
    dir: &Path,
    name: &Path,
    target: &Path,
) -> io::Result<()> {
    let at = dir.join(name);
    match std::fs::symlink_metadata(&at) {
        Ok(_) if ops.is_link(&at) => {
            if ops.remove_link(&at).is_err() {
                std::fs::remove_file(&at)?;
            }
        }
        Ok(m) if m.is_dir() => {
            return Err(io::Error::other(format!(
                "{} is a folder in the command's Gradle home",
                name.display()
            )));
        }
        Ok(_) => std::fs::remove_file(&at)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    ops.link_dir(target, &at)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().expect("dir");
        let p = std::fs::canonicalize(d.path()).expect("canonical");
        (d, p)
    }

    /// The by-path link of a folder, a file copy, a kept link, a parent that
    /// is a link (never entered) and something already there (left).
    #[test]
    fn links_and_copies_are_made_only_where_nothing_is() {
        let (_c, c) = canonical_tempdir();
        write(&c.join("node_modules/dep/index.js"), "dep");
        write(&c.join(".pnp.cjs"), "pnp");
        let (_t, t) = canonical_tempdir();
        let nm = std::fs::canonicalize(c.join("node_modules")).expect("nm");
        let pnp = std::fs::canonicalize(c.join(".pnp.cjs")).expect("pnp");
        assert!(matches!(
            link(&Symlinks, &t, Path::new("node_modules"), &nm).expect("link"),
            Linked::Made
        ));
        assert_eq!(
            std::fs::read_to_string(t.join("node_modules/dep/index.js")).expect("read"),
            "dep"
        );
        assert!(matches!(
            link(&Symlinks, &t, Path::new("node_modules"), &nm).expect("link"),
            Linked::Kept
        ));
        assert!(matches!(
            link(&Symlinks, &t, Path::new(".pnp.cjs"), &pnp).expect("copy"),
            Linked::Made
        ));
        let meta = std::fs::symlink_metadata(t.join(".pnp.cjs")).expect("meta");
        assert!(meta.is_file(), "a file is copied, not linked");
        assert!(matches!(
            link(&Symlinks, &t, Path::new(".pnp.cjs"), &pnp).expect("again"),
            Linked::Present
        ));
        // A parent that is a link is never entered.
        let (_o, o) = canonical_tempdir();
        std::os::unix::fs::symlink(&o, t.join("svc")).expect("ln");
        assert!(matches!(
            link(&Symlinks, &t, Path::new("svc/node_modules"), &nm).expect("link"),
            Linked::NoParent
        ));
        assert!(std::fs::read_dir(&o).expect("read").next().is_none());
    }

    /// The by-path mirror of an npm workspace install: workspace links
    /// resolve to the tree's own packages, other entries to the checkout's,
    /// `.bin` files are copies; a second call keeps it and adds what was
    /// installed since; a worker's own folder (no mark) is left.
    #[test]
    fn the_mirror_by_path_resolves_workspace_packages_to_the_tree() {
        let (_c, c) = canonical_tempdir();
        write(&c.join("packages/web/index.js"), "checkout");
        write(&c.join("node_modules/dep/index.js"), "dep");
        std::fs::create_dir_all(c.join("node_modules/@s")).expect("mkdir");
        std::os::unix::fs::symlink("../../packages/web", c.join("node_modules/@s/web"))
            .expect("ln");
        write(&c.join("node_modules/.bin/tool.cmd"), "@node tool");
        let (_t, t) = canonical_tempdir();
        write(&t.join("packages/web/index.js"), "tree");
        let source = c.join("node_modules");
        let plan = super::super::mirror_plan(&source, &c).expect("a workspace install");
        let made = mirror(
            &Symlinks,
            &t,
            Path::new("node_modules"),
            &source,
            &plan,
            "s1",
        )
        .expect("mirror");
        assert!(matches!(made, Linked::Made));
        let read =
            |rel: &str| std::fs::read_to_string(t.join("node_modules").join(rel)).expect(rel);
        assert_eq!(read("@s/web/index.js"), "tree");
        assert_eq!(read("dep/index.js"), "dep");
        assert_eq!(read(".bin/tool.cmd"), "@node tool");
        assert!(mirror_is_current(
            &Symlinks,
            &t,
            Path::new("node_modules"),
            "s1"
        ));

        write(&c.join("node_modules/later/index.js"), "later");
        let plan = super::super::mirror_plan(&source, &c).expect("plan");
        let kept = mirror(
            &Symlinks,
            &t,
            Path::new("node_modules"),
            &source,
            &plan,
            "s2",
        )
        .expect("mirror");
        assert!(matches!(kept, Linked::Kept));
        assert_eq!(read("later/index.js"), "later");

        let (_w, w) = canonical_tempdir();
        write(&w.join("node_modules/own/index.js"), "own");
        let left = mirror(
            &Symlinks,
            &w,
            Path::new("node_modules"),
            &source,
            &plan,
            "s1",
        )
        .expect("mirror");
        assert!(matches!(left, Linked::Present));
        assert!(!w.join("node_modules").join(MIRROR_MARK).exists());
    }

    /// A Gradle home's folders are made only through real folders, and a
    /// link left where the distribution link goes is replaced.
    #[test]
    fn gradle_folders_and_links_by_path() {
        let (_r, r) = canonical_tempdir();
        let dist = make_dirs(
            &Symlinks,
            &r,
            Path::new("tmp/gradle-home/wrapper/dists/g/h"),
        )
        .expect("dirs");
        let (_u, u) = canonical_tempdir();
        std::fs::write(dist.join("gradle-8"), "left by a command").expect("write");
        replace_with_link(&Symlinks, &dist, Path::new("gradle-8"), &u).expect("link");
        assert!(Symlinks.is_link(&dist.join("gradle-8")));
        let (_o, o) = canonical_tempdir();
        std::os::unix::fs::symlink(&o, r.join("tmp/elsewhere")).expect("ln");
        assert!(make_dirs(&Symlinks, &r, Path::new("tmp/elsewhere/x")).is_err());
        assert!(std::fs::read_dir(&o).expect("read").next().is_none());
    }
}
