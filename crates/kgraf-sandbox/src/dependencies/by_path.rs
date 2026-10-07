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
        // `get_target` drops the `\??\` prefix; `read_link` gives a symbolic
        // link's target as `\\?\C:\...`. Both are put in plain form so the
        // target compares equal to a canonical source path.
        junction::get_target(path)
            .ok()
            .or_else(|| std::fs::read_link(path).ok())
            .map(|target| kgraf_core::plain_path::simplify(&target).into_owned())
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
    match (
        kgraf_core::plain_path::canonicalize(parent),
        kgraf_core::plain_path::canonicalize(tree),
    ) {
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
                .and_then(|t| kgraf_core::plain_path::canonicalize(t).ok())
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
                .and_then(|t| kgraf_core::plain_path::canonicalize(t).ok())
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
        let p = kgraf_core::plain_path::canonicalize(d.path()).expect("canonical");
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
        let nm = kgraf_core::plain_path::canonicalize(c.join("node_modules")).expect("nm");
        let pnp = kgraf_core::plain_path::canonicalize(c.join(".pnp.cjs")).expect("pnp");
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

    /// Symbolic links with the failures a parallel command could cause: a
    /// `link_dir` that fails with a chosen error, and a hook run right after
    /// a link is removed (to swap what is there).
    type AfterRemove = Box<dyn Fn(&Path)>;

    struct Hooked {
        link_dir_error: Option<io::ErrorKind>,
        after_remove: Option<AfterRemove>,
        removed: std::cell::Cell<usize>,
    }

    impl Hooked {
        fn new() -> Self {
            Self {
                link_dir_error: None,
                after_remove: None,
                removed: std::cell::Cell::new(0),
            }
        }
        fn failing(kind: io::ErrorKind) -> Self {
            Self {
                link_dir_error: Some(kind),
                ..Self::new()
            }
        }
    }

    impl LinkOps for Hooked {
        fn is_link(&self, path: &Path) -> bool {
            Symlinks.is_link(path)
        }
        fn link_target(&self, path: &Path) -> Option<PathBuf> {
            Symlinks.link_target(path)
        }
        fn link_dir(&self, target: &Path, at: &Path) -> io::Result<()> {
            match self.link_dir_error {
                Some(kind) => Err(io::Error::from(kind)),
                None => Symlinks.link_dir(target, at),
            }
        }
        fn remove_link(&self, at: &Path) -> io::Result<()> {
            Symlinks.remove_link(at)?;
            self.removed.set(self.removed.get() + 1);
            if let Some(hook) = &self.after_remove {
                hook(at);
            }
            Ok(())
        }
    }

    /// A checkout with a workspace install, its `node_modules` and the plan.
    fn workspace_install() -> (tempfile::TempDir, PathBuf, Vec<MirrorEntry>) {
        let (c, root) = canonical_tempdir();
        write(&root.join("packages/web/index.js"), "checkout");
        write(&root.join("node_modules/dep/index.js"), "dep");
        std::fs::create_dir_all(root.join("node_modules/@s")).expect("mkdir");
        std::os::unix::fs::symlink("../../packages/web", root.join("node_modules/@s/web"))
            .expect("ln");
        let source = root.join("node_modules");
        let plan = super::super::mirror_plan(&source, &root).expect("a workspace install");
        (c, source, plan)
    }

    fn is_real_dir(p: &Path) -> bool {
        std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
    }

    /// Nothing is made under a folder the tree does not have.
    #[test]
    fn a_link_is_not_made_under_an_absent_folder() {
        let (_c, c) = canonical_tempdir();
        write(&c.join("node_modules/dep/index.js"), "dep");
        let nm = c.join("node_modules");
        let (_t, t) = canonical_tempdir();
        assert!(matches!(
            link(&Symlinks, &t, Path::new("gone/node_modules"), &nm),
            Ok(Linked::NoParent)
        ));
        assert!(!t.join("gone").exists());
    }

    /// A made entry whose folder resolves outside the tree is removed again
    /// and reported, for a folder link and for a file copy.
    #[test]
    fn an_entry_made_outside_the_tree_is_removed_and_reported() {
        let (_c, c) = canonical_tempdir();
        write(&c.join("node_modules/dep/index.js"), "dep");
        write(&c.join(".pnp.cjs"), "pnp");
        let (_t, t) = canonical_tempdir();
        let (_o, o) = canonical_tempdir();
        std::os::unix::fs::symlink(&o, t.join("svc")).expect("ln");
        let nm = c.join("node_modules");
        assert!(make(&Symlinks, &t, &nm, &t.join("svc/x")).is_err());
        assert!(std::fs::symlink_metadata(o.join("x")).is_err(), "link left");
        assert!(make(&Symlinks, &t, &c.join(".pnp.cjs"), &t.join("svc/y")).is_err());
        assert!(std::fs::symlink_metadata(o.join("y")).is_err(), "copy left");
    }

    /// Only "already exists" (a parallel command made it first) is kept as
    /// present; any other failure is reported.
    #[test]
    fn link_failures_are_kept_as_present_only_when_the_entry_exists() {
        let (_c, c) = canonical_tempdir();
        write(&c.join("node_modules/dep/index.js"), "dep");
        let nm = c.join("node_modules");
        let (_t, t) = canonical_tempdir();
        let raced = Hooked::failing(io::ErrorKind::AlreadyExists);
        assert!(matches!(
            link(&raced, &t, Path::new("node_modules"), &nm),
            Ok(Linked::Present)
        ));
        let denied = Hooked::failing(io::ErrorKind::PermissionDenied);
        let err = link(&denied, &t, Path::new("node_modules"), &nm).err();
        assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::PermissionDenied));
    }

    /// A mirror counts as current only when every part holds.
    #[test]
    fn a_mirror_is_current_only_when_folder_mark_and_stamp_all_hold() {
        let (_w, source, plan) = workspace_install();
        let rel = Path::new("node_modules");
        let (_t, t) = canonical_tempdir();
        assert!(!mirror_is_current(&Symlinks, &t, rel, "s"), "absent");
        mirror(&Symlinks, &t, rel, &source, &plan, "s").expect("mirror");
        assert!(mirror_is_current(&Symlinks, &t, rel, "s"));
        assert!(!mirror_is_current(&Symlinks, &t, rel, "other stamp"));
        // The mark is a link to a file with the right stamp.
        let mark = t.join("node_modules").join(MIRROR_MARK);
        let real = t.join("stamp-file");
        std::fs::write(&real, "s").expect("write");
        std::fs::remove_file(&mark).expect("rm");
        std::os::unix::fs::symlink(&real, &mark).expect("ln");
        assert!(
            !mirror_is_current(&Symlinks, &t, rel, "s"),
            "mark is a link"
        );
        std::fs::remove_file(&mark).expect("rm");
        assert!(!mirror_is_current(&Symlinks, &t, rel, "s"), "no mark");
        // The folder is a link to a folder holding a right mark.
        let (_o, o) = canonical_tempdir();
        write(&o.join(MIRROR_MARK), "s");
        std::os::unix::fs::symlink(&o, t.join("linked")).expect("ln");
        assert!(!mirror_is_current(&Symlinks, &t, Path::new("linked"), "s"));
        // A parent that is a link to a folder holding a right mirror.
        std::fs::create_dir_all(o.join("node_modules")).expect("mkdir");
        write(&o.join("node_modules").join(MIRROR_MARK), "s");
        std::os::unix::fs::symlink(&o, t.join("svc")).expect("ln");
        assert!(!mirror_is_current(
            &Symlinks,
            &t,
            Path::new("svc/node_modules"),
            "s"
        ));
        // A file where the folder goes.
        std::fs::write(t.join("file"), "s").expect("write");
        assert!(!mirror_is_current(&Symlinks, &t, Path::new("file"), "s"));
    }

    /// kGraf's own earlier link is replaced by the folder; a link to
    /// something else is left, and nothing is written through it.
    #[test]
    fn the_mirror_replaces_its_own_link_and_leaves_a_foreign_one() {
        let (_w, source, plan) = workspace_install();
        let rel = Path::new("node_modules");
        let (_t, t) = canonical_tempdir();
        std::os::unix::fs::symlink(&source, t.join("node_modules")).expect("ln");
        let made = mirror(&Symlinks, &t, rel, &source, &plan, "s").expect("mirror");
        assert!(matches!(made, Linked::Made));
        assert!(is_real_dir(&t.join("node_modules")));
        assert!(t.join("node_modules").join(MIRROR_MARK).is_file());

        let (_f, f) = canonical_tempdir();
        let (_o, o) = canonical_tempdir();
        std::os::unix::fs::symlink(&o, f.join("node_modules")).expect("ln");
        let left = mirror(&Symlinks, &f, rel, &source, &plan, "s").expect("mirror");
        assert!(matches!(left, Linked::Present));
        assert!(Symlinks.is_link(&f.join("node_modules")));
        assert!(std::fs::read_dir(&o).expect("read").next().is_none());
        // A file where the folder goes is left too.
        let (_g, g) = canonical_tempdir();
        std::fs::write(g.join("node_modules"), "x").expect("write");
        let file = mirror(&Symlinks, &g, rel, &source, &plan, "s").expect("mirror");
        assert!(matches!(file, Linked::Present));
    }

    /// A parallel command that swaps what the mirror works in, between the
    /// check and the write, makes the mirror stop and write nothing there.
    #[test]
    fn the_mirror_stops_when_its_folder_is_swapped_for_a_link() {
        let (_w, source, plan) = workspace_install();
        let rel = Path::new("node_modules");
        // Swapped for a link to another folder of the tree.
        let (_t, t) = canonical_tempdir();
        std::os::unix::fs::symlink(&source, t.join("node_modules")).expect("ln");
        let inner = t.join("inner");
        std::fs::create_dir(&inner).expect("mkdir");
        let mut ops = Hooked::new();
        let target = inner.clone();
        ops.after_remove = Some(Box::new(move |at| {
            std::os::unix::fs::symlink(&target, at).expect("swap");
        }));
        let out = mirror(&ops, &t, rel, &source, &plan, "s").expect("mirror");
        assert!(matches!(out, Linked::Present));
        assert!(std::fs::read_dir(&inner).expect("read").next().is_none());
        // Swapped: a folder above it becomes a link out of the tree.
        let (_u, u) = canonical_tempdir();
        let (_o, o) = canonical_tempdir();
        std::fs::create_dir(u.join("svc")).expect("mkdir");
        std::os::unix::fs::symlink(&source, u.join("svc/node_modules")).expect("ln");
        let mut ops = Hooked::new();
        let (svc, out_dir) = (u.join("svc"), o.clone());
        ops.after_remove = Some(Box::new(move |_| {
            std::fs::remove_dir(&svc).expect("rmdir");
            std::os::unix::fs::symlink(&out_dir, &svc).expect("swap");
        }));
        let out = mirror(&ops, &u, Path::new("svc/node_modules"), &source, &plan, "s");
        assert!(matches!(out, Ok(Linked::Present)));
        assert!(!o.join("node_modules").join(MIRROR_MARK).exists());
        assert!(!o.join("node_modules/dep").exists());
    }

    /// A folder made by a parallel command first is taken as it is.
    #[test]
    fn the_mirror_goes_on_when_its_folder_appears_first() {
        let (_w, source, plan) = workspace_install();
        let (_t, t) = canonical_tempdir();
        std::os::unix::fs::symlink(&source, t.join("node_modules")).expect("ln");
        let mut ops = Hooked::new();
        ops.after_remove = Some(Box::new(|at| {
            std::fs::create_dir(at).expect("made first");
        }));
        let out = mirror(&ops, &t, Path::new("node_modules"), &source, &plan, "s");
        assert!(matches!(out, Ok(Linked::Made)));
        assert!(t.join("node_modules").join(MIRROR_MARK).is_file());
    }

    /// A workspace package the tree lacks and an entry that exists are
    /// skipped; any other failure is reported.
    #[test]
    fn mirror_link_failures_are_skipped_only_when_expected() {
        let (_w, source, plan) = workspace_install();
        let rel = Path::new("node_modules");
        for kind in [io::ErrorKind::AlreadyExists, io::ErrorKind::NotFound] {
            let (_t, t) = canonical_tempdir();
            let out = mirror(&Hooked::failing(kind), &t, rel, &source, &plan, "s");
            assert!(matches!(out, Ok(Linked::Made)), "{kind:?}");
        }
        let (_t, t) = canonical_tempdir();
        let denied = Hooked::failing(io::ErrorKind::PermissionDenied);
        let err = mirror(&denied, &t, rel, &source, &plan, "s").err();
        assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::PermissionDenied));
    }

    /// A folder entry is made, an existing one is kept, and a failure is
    /// reported.
    #[test]
    fn mirror_folder_entries_are_made_kept_or_reported() {
        let (_w, source, _) = workspace_install();
        let rel = Path::new("node_modules");
        let (_t, t) = canonical_tempdir();
        let plan = vec![MirrorEntry::Dir(PathBuf::from("@s"))];
        mirror(&Symlinks, &t, rel, &source, &plan, "1").expect("first");
        assert!(t.join("node_modules/@s").is_dir());
        mirror(&Symlinks, &t, rel, &source, &plan, "2").expect("second keeps it");
        let deep = vec![MirrorEntry::Dir(PathBuf::from("no/such/parent"))];
        let err = mirror(&Symlinks, &t, rel, &source, &deep, "3").err();
        assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::NotFound));
    }

    /// A folder that cannot be made or a mark that cannot be replaced is
    /// reported as that failure.
    #[cfg(unix)]
    #[test]
    fn mirror_write_failures_are_reported_as_they_are() {
        use std::os::unix::fs::PermissionsExt;
        let (_w, source, _) = workspace_install();
        let rel = Path::new("node_modules");
        let set = |p: &Path, mode| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).expect("chmod");
        };
        // The tree cannot take a new folder.
        let (_t, t) = canonical_tempdir();
        set(&t, 0o500);
        let probe = std::fs::File::create(t.join("probe"));
        if probe.is_ok() {
            set(&t, 0o700);
            return; // running as a user that ignores permissions
        }
        let err = mirror(&Symlinks, &t, rel, &source, &[], "s").err();
        set(&t, 0o700);
        assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::PermissionDenied));
        // The folder cannot take a new mark.
        mirror(&Symlinks, &t, rel, &source, &[], "s").expect("first");
        let folder = t.join("node_modules");
        set(&folder, 0o500);
        let err = mirror(&Symlinks, &t, rel, &source, &[], "t").err();
        set(&folder, 0o700);
        assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::PermissionDenied));
    }

    /// Gradle folders: an existing folder is reused, a file in the way is an
    /// error, and a failure to make one is that failure.
    #[test]
    fn gradle_folders_reuse_folders_and_refuse_a_file() {
        use std::os::unix::fs::PermissionsExt;
        let (_r, r) = canonical_tempdir();
        make_dirs(&Symlinks, &r, Path::new("a/b")).expect("first");
        make_dirs(&Symlinks, &r, Path::new("a/b")).expect("existing folders");
        std::fs::write(r.join("a/file"), "x").expect("write");
        assert!(make_dirs(&Symlinks, &r, Path::new("a/file")).is_err());
        std::fs::set_permissions(&r, std::fs::Permissions::from_mode(0o500)).expect("chmod");
        let can_write = std::fs::File::create(r.join("probe")).is_ok();
        let err = make_dirs(&Symlinks, &r, Path::new("new")).err();
        std::fs::set_permissions(&r, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        if !can_write {
            assert_eq!(err.map(|e| e.kind()), Some(io::ErrorKind::PermissionDenied));
        }
    }

    /// A link is removed as a link; a file is removed as a file; a real
    /// folder is an error and stays; an absent name is simply linked.
    #[test]
    fn replace_with_link_treats_a_link_a_file_a_folder_and_nothing_apart() {
        let (_d, d) = canonical_tempdir();
        let (_u, u) = canonical_tempdir();
        let (_v, v) = canonical_tempdir();
        let ops = Hooked::new();
        replace_with_link(&ops, &d, Path::new("fresh"), &u).expect("absent");
        assert_eq!(
            kgraf_core::plain_path::canonicalize(d.join("fresh")).expect("c"),
            u
        );
        assert_eq!(ops.removed.get(), 0);
        replace_with_link(&ops, &d, Path::new("fresh"), &v).expect("a link");
        assert_eq!(
            kgraf_core::plain_path::canonicalize(d.join("fresh")).expect("c"),
            v
        );
        assert_eq!(ops.removed.get(), 1, "a link goes through remove_link");
        std::fs::write(d.join("file"), "x").expect("write");
        replace_with_link(&ops, &d, Path::new("file"), &u).expect("a file");
        assert!(Symlinks.is_link(&d.join("file")));
        assert_eq!(ops.removed.get(), 1, "a file is not removed as a link");
        std::fs::create_dir(d.join("real")).expect("mkdir");
        let err = replace_with_link(&ops, &d, Path::new("real"), &u)
            .err()
            .map(|e| e.to_string());
        assert!(err.is_some_and(|m| m.contains("is a folder")));
        assert!(is_real_dir(&d.join("real")));
    }
}
