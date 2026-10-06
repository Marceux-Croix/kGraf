//! Project-local dependency trees, reached read-only from a worktree or a
//! check tree.
//!
//! **Why.** A worktree holds only what git tracks. The dependencies the user
//! installed in the checkout (`node_modules/`, a `.venv/`, Composer's or
//! Bundler's `vendor/`, Yarn's Plug'n'Play files) are ignored by git, so a
//! worktree has none of them, and the sandbox denies reads of the checkout
//! (macOS) or never grants them (Linux). A correct npm, pytest or PHP project
//! then failed every test inside kGraf.
//!
//! **What this does.** [`find_dependency_trees`] walks the checkout (bounded,
//! never through a link) for those trees beside their project's manifest.
//! [`link_dependency_trees`] makes each one that the tree lacks a symbolic
//! link at the same place in the tree, pointing at the checkout's copy, and
//! returns the paths the sandbox must let a command read
//! ([`crate::SandboxPolicy::with_dependency_links`]). Reads only: the link
//! target is outside every writable root, so a write through the link is
//! denied by the sandbox (both backends match the resolved path). A link
//! inside a dependency tree that points elsewhere is judged at its target: a
//! credential store stays denied, and on Linux anything outside the granted
//! trees stays unreadable.
//!
//! A link the worker replaced with its own folder is left alone; a tree the
//! worktree already has (a committed `vendor/`) needs no link and no grant.
//! Each link is a generated file beside its project (merge-back leaves it out
//! of a commit, [`crate::merge::generated`]).
//!
//! A `node_modules` whose workspace packages are links into the checkout
//! (`node_modules/@scope/pkg -> ../../packages/pkg`, npm, pnpm and Yarn
//! workspaces) becomes a folder of links instead ([`mirror_plan`]): each
//! workspace link points at the tree's own copy of the package, so the
//! worker's tests run its own edits, and every other entry at the
//! checkout's install. A virtual environment is activated by the folder a
//! command starts in: the one owning that folder, or the nearest above it
//! ([`DependencyLinks::virtual_env_for`]).
//!
//! **pnpm.** A pnpm install keeps every package as a real folder in
//! `node_modules/.pnpm` and links workspace packages there too
//! (`.pnpm/node_modules/@scope/web -> ../../../../packages/web`, its hidden
//! hoisting folder). Node resolves a package from its real path, so a
//! package reached through any link into the checkout's `.pnpm` looks up its
//! own dependencies in the checkout's folders, never the tree's: no folder
//! of links can point those lookups at the tree. When a pnpm install has
//! workspace links, the tree gets a copy-on-write clone of it instead
//! ([`unix::clone_install`]): pnpm's links are relative, so in the clone
//! every one of them, nested ones included, resolves inside the tree, and a
//! workspace package to the tree's own copy. A clone costs one file system
//! call per file and no data (APFS `clonefile`, a Linux reflink); where no
//! clone can be made the install gets a folder of links as before, and the
//! output says that nested workspace links reach the checkout's copies.
//!
//! **Windows.** A symbolic link needs a privilege a user account lacks by
//! default, so a folder is reached through a directory junction and a file
//! (`.pnp.cjs`) is copied ([`by_path`]). The writable root's ACL is set
//! before the first junction is made in it. Junctions are made by path, not
//! relative to an open folder; see [`by_path`] for what that leaves.
//!
//! **Not covered**, stated rather than hidden: a dependency link that
//! resolves into the checkout to a folder the tree does not have (a package
//! that is not tracked) stays unreadable; such a run fails with the
//! permission error. A third-party package of a pnpm install whose own
//! folder (`.pnpm/<name>@<version>/node_modules`) links a workspace package
//! reaches the tree's copy only through a clone.
//!
//! Written new. Searched: this repository (the cargo dependency gate, decision
//! 0087, handles cargo's own cache only) and the sandboxes it ports (codex-rs
//! grants whole home directories instead); no reusable crate maps manifests
//! to install trees.

use std::path::{Path, PathBuf};

pub(crate) mod by_path;

/// Deepest folder level searched for a project's manifest, from the checkout
/// root: `packages/web/app` is three. A monorepo's packages sit at two or
/// three; six leaves room without walking a whole source tree.
pub const MAX_DEPTH: usize = 6;

/// Most folders the search opens. A checkout larger than this is searched
/// in part, and the result says so ([`DependencyLinks::notes`]).
pub const MAX_DIRS: usize = 20_000;

/// Folders never searched for a manifest: version control, kGraf's own
/// state, the dependency trees themselves, and build output.
const SKIPPED: &[&str] = &[
    ".git",
    ".kgraf",
    ".haustoria",
    "node_modules",
    ".venv",
    "venv",
    "vendor",
    "target",
    "build",
    "dist",
    "coverage",
    "__pycache__",
    "bin",
    "obj",
];

/// What kind of install a tree is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyKind {
    /// `node_modules/` beside a `package.json` (npm, pnpm, yarn, bun).
    NodeModules,
    /// Yarn Plug'n'Play: `.pnp.cjs`, `.pnp.loader.mjs`, `.pnp.data.json` and
    /// the `.yarn/` cache beside a `package.json`.
    YarnPnp,
    /// A Python virtual environment: `.venv/` or `venv/` with a `pyvenv.cfg`.
    PythonVenv,
    /// `vendor/` beside a `go.mod` (with `vendor/modules.txt`), a
    /// `composer.json` or a `Gemfile` (Bundler's `vendor/bundle`).
    Vendor,
}

/// One installed tree in the checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyTree {
    /// Relative to the checkout root, with `/`.
    pub rel: PathBuf,
    /// Canonical absolute path in the checkout.
    pub source: PathBuf,
    pub kind: DependencyKind,
    /// A folder (`node_modules/`) rather than a file (`.pnp.cjs`).
    pub is_dir: bool,
}

/// Why `tree`, found by an earlier search, may no longer be granted, or
/// `None` when it still is what the search found: the same canonical path
/// (neither it nor a folder above it became a link), of the same kind,
/// that only its owner can change. Asked right before each link and grant,
/// so neither a reused search ([`TREES_FRESH_FOR`]) nor a swap after the
/// search makes the sandbox resolve a grant to a link's target.
fn changed_since_search(tree: &DependencyTree) -> Option<String> {
    let meta = match std::fs::symlink_metadata(&tree.source) {
        Ok(meta) => meta,
        Err(e) => return Some(format!("cannot be read ({e})")),
    };
    if meta.file_type().is_symlink() {
        return Some("became a symbolic link".into());
    }
    if meta.is_dir() != tree.is_dir {
        return Some("changed kind".into());
    }
    #[cfg(unix)]
    if let Some(why) = crate::trust::untrusted_reason(&meta) {
        return Some(why);
    }
    match std::fs::canonicalize(&tree.source) {
        Ok(now) if now == tree.source => None,
        Ok(now) => Some(format!("now resolves to {}", now.display())),
        Err(e) => Some(format!("cannot be resolved ({e})")),
    }
}

/// What [`link_dependency_trees`] did for one tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DependencyLinks {
    /// Canonical paths in the checkout a command may read (and run programs
    /// from): the target of every link now in the tree.
    pub reads: Vec<PathBuf>,
    /// Relative paths in the tree that are links to the checkout's trees.
    pub linked: Vec<PathBuf>,
    /// Each linked virtual environment: the tree folder it belongs to (the
    /// one holding the `.venv` or `venv` link) and its canonical folder in
    /// the checkout (a granted path the worker cannot write), never the link
    /// in the tree. A command runs as if the one owning its folder were
    /// activated ([`Self::virtual_env_for`]).
    pub virtual_envs: Vec<VirtualEnv>,
    /// A tree found but not linked, and why; a search cut short. Shown with
    /// a failing command's output.
    pub notes: Vec<String>,
}

/// A virtual environment linked into a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualEnv {
    /// The tree folder holding the link (the tree root for `<tree>/.venv`).
    pub dir: PathBuf,
    /// The environment's canonical folder in the checkout.
    pub source: PathBuf,
}

impl DependencyLinks {
    /// The virtual environment a command starting in `dir` runs with: the
    /// one whose folder is `dir` or the nearest folder above it, as Python
    /// tools find a project's `.venv`. `None` when no linked environment
    /// owns `dir` (a `.venv` in a sibling project is not this command's).
    pub fn virtual_env_for(&self, dir: &Path) -> Option<&Path> {
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        nearest_virtual_env(&self.virtual_envs, &dir)
    }
}

/// [`DependencyLinks::virtual_env_for`] over a list, for the sandbox
/// policy that keeps the list.
pub(crate) fn nearest_virtual_env<'a>(envs: &'a [VirtualEnv], dir: &Path) -> Option<&'a Path> {
    envs.iter()
        .filter(|v| dir.starts_with(&v.dir))
        .max_by_key(|v| v.dir.components().count())
        .map(|v| v.source.as_path())
}

/// The folder a shell script starts its work in, relative to where it is
/// run: `DIR` when the script begins with `cd DIR` followed by `&&`, `;` or
/// a line end, `DIR` being one plain relative path (letters, digits and
/// `._-+@/`, no `..`); else `None`. Used only to choose the virtual
/// environment of a command such as `cd services/api && python -m pytest`.
pub fn script_start_dir(script: &str) -> Option<PathBuf> {
    let first = script
        .trim_start()
        .split(['\n', ';'])
        .next()?
        .split("&&")
        .next()?
        .trim();
    let mut words = first.split_whitespace();
    if words.next()? != "cd" {
        return None;
    }
    let dir = words.next()?;
    if words.next().is_some()
        || dir.starts_with('/')
        || !dir
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-+@/".contains(c))
    {
        return None;
    }
    let path = Path::new(dir);
    path.components()
        .all(|c| {
            matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
        .then(|| path.to_path_buf())
}

/// The candidates beside one folder's manifests: (relative name, kind,
/// whether it is a folder). `names` are the folder's entries, as the walk
/// read them: a name that is not there costs no file system call.
fn candidates(dir: &Path, names: &Names) -> Vec<(&'static str, DependencyKind, bool)> {
    let has = |name: &str| names.contains(name);
    let mut out = Vec::new();
    if has("package.json") {
        out.push(("node_modules", DependencyKind::NodeModules, true));
        out.push((".pnp.cjs", DependencyKind::YarnPnp, false));
        out.push((".pnp.loader.mjs", DependencyKind::YarnPnp, false));
        out.push((".pnp.data.json", DependencyKind::YarnPnp, false));
        // `.yarn/` whole when the tree has none; otherwise (its `releases/`
        // or `plugins/` are committed) the installed parts one by one.
        out.push((".yarn", DependencyKind::YarnPnp, true));
        out.push((".yarn/cache", DependencyKind::YarnPnp, true));
        out.push((".yarn/unplugged", DependencyKind::YarnPnp, true));
        out.push((".yarn/install-state.gz", DependencyKind::YarnPnp, false));
    }
    // A virtual environment is told by its `pyvenv.cfg`, with or without a
    // Python manifest beside it.
    for venv in [".venv", "venv"] {
        if has(venv) && dir.join(venv).join("pyvenv.cfg").is_file() {
            out.push((venv, DependencyKind::PythonVenv, true));
        }
    }
    let go_vendor = has("go.mod") && has("vendor") && dir.join("vendor/modules.txt").is_file();
    if go_vendor || has("composer.json") || has("Gemfile") {
        out.push(("vendor", DependencyKind::Vendor, true));
    }
    if has("Gemfile") {
        out.push(("vendor/bundle", DependencyKind::Vendor, true));
    }
    out
}

/// The entry names of one folder, as the walk read them (names that are not
/// valid UTF-8 are left out: no manifest is named so).
pub type Names = std::collections::HashSet<String>;

/// Visit every project folder of the checkout at `checkout`, breadth first:
/// `visit(absolute, relative, names)`, the root first (relative empty), with
/// the folder's entry names so a visitor asks the file system only about
/// names that are there. Never follows a link; never enters a [`SKIPPED`] or
/// hidden folder; at most [`MAX_DEPTH`] levels and [`MAX_DIRS`] folders.
/// Returns a note when the search was cut short. The same walk serves the
/// dependency links here and the host's check for installs that are
/// missing, so the two never disagree about which folders are projects.
///
/// Measured (macOS, APFS, release build, 2026-10-06, the ignored test
/// `walk_cost_at_the_folder_bound`): a checkout of 20,000 folders, the bound,
/// takes about 0.4 s, one directory read per folder; it was 0.8 to 0.95 s
/// while each folder's manifests were asked for one by one. A checkout past
/// the bound is searched in part and says so. [`link_dependency_trees`]
/// keeps the result for [`TREES_FRESH_FOR`], so a worker's shell calls do
/// not pay it each time.
pub fn walk_project_dirs(
    checkout: &Path,
    mut visit: impl FnMut(&Path, &Path, &Names),
) -> Option<String> {
    let root = std::fs::canonicalize(checkout).ok()?;
    // (absolute folder, relative folder, depth).
    let mut queue = std::collections::VecDeque::from([(root, PathBuf::new(), 0usize)]);
    let mut opened = 0usize;
    while let Some((dir, rel, depth)) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names = Names::new();
        let mut subdirs: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let Ok(text) = entry.file_name().into_string() else {
                continue;
            };
            // `file_type` does not follow links (and needs no extra call where
            // the directory listing carries the type): a link to a folder is
            // not entered.
            let enter = depth < MAX_DEPTH
                && !text.starts_with('.')
                && !SKIPPED.contains(&text.as_str())
                && entry.file_type().is_ok_and(|k| k.is_dir());
            if enter {
                subdirs.push(text.clone());
            }
            names.insert(text);
        }
        visit(&dir, &rel, &names);
        // A stable order, so a search cut short is cut at the same place.
        subdirs.sort();
        for name in subdirs {
            opened += 1;
            if opened > MAX_DIRS {
                return Some(format!(
                    "the checkout has more than {MAX_DIRS} folders; installed dependencies \
                     were searched for in part only"
                ));
            }
            queue.push_back((dir.join(&name), rel.join(&name), depth + 1));
        }
    }
    None
}

/// The installed dependency trees of the checkout at `checkout`, and a note
/// for each one that is not usable (a link, a tree another user can change)
/// or for a search cut short ([`walk_project_dirs`]).
pub fn find_dependency_trees(checkout: &Path) -> (Vec<DependencyTree>, Vec<String>) {
    let mut trees = Vec::new();
    let mut notes = Vec::new();
    let Ok(root) = std::fs::canonicalize(checkout) else {
        return (trees, notes);
    };
    let cut = walk_project_dirs(&root, |dir, rel, names| {
        for (name, kind, is_dir) in candidates(dir, names) {
            let path = dir.join(name);
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let shown = rel.join(name);
            if meta.file_type().is_symlink() {
                notes.push(format!(
                    "{} is a symbolic link in the checkout; kGraf does not follow it into the \
                     sandbox",
                    shown.display()
                ));
                continue;
            }
            if meta.is_dir() != is_dir {
                continue;
            }
            #[cfg(unix)]
            if let Some(why) = crate::trust::untrusted_reason(&meta) {
                notes.push(format!(
                    "{} {why}, so it is not linked into the sandbox",
                    shown.display()
                ));
                continue;
            }
            match std::fs::canonicalize(&path) {
                Ok(source) if source.starts_with(&root) => trees.push(DependencyTree {
                    rel: shown,
                    source,
                    kind,
                    is_dir,
                }),
                _ => {}
            }
        }
    });
    notes.extend(cut);
    (trees, notes)
}

/// Link each of the checkout's dependency trees that `tree` lacks, at the
/// same relative path, and say what the sandbox must let a command read.
/// Idempotent: a link already there to the same target is kept and counted.
/// A path the tree already holds as anything else (a committed folder, the
/// worker's own install) is left as it is and needs no grant. A link is made
/// only where its parent folder already exists in the tree as a real folder,
/// reached without crossing a link.
pub fn link_dependency_trees(tree: &Path, checkout: &Path) -> DependencyLinks {
    let (trees, notes) = recent_trees(checkout);
    let mut out = DependencyLinks {
        notes,
        ..DependencyLinks::default()
    };
    if cfg!(not(any(unix, windows))) {
        return out;
    }
    let checkout_root = std::fs::canonicalize(checkout).unwrap_or_else(|_| checkout.to_path_buf());
    // Canonical, as the command folder it is compared with.
    let tree_root = std::fs::canonicalize(tree).unwrap_or_else(|_| tree.to_path_buf());
    #[cfg(unix)]
    let Ok(root) = crate::confined::open_root(tree) else {
        return out;
    };
    // The writable root's ACL is set before any junction is in it, so its
    // inheritable entries are never written while a junction is there.
    #[cfg(windows)]
    if !trees.is_empty()
        && let Err(e) = crate::sandbox::prepare_windows_root(&tree_root)
    {
        out.notes.push(format!(
            "the installed dependencies are not linked into the tree: its access control \
             could not be set first: {e}"
        ));
        return out;
    }
    for found in trees {
        if let Some(why) = changed_since_search(&found) {
            out.notes.push(format!(
                "the installed {} in the checkout {why} since it was found, so it is not \
                 linked into the sandbox",
                found.rel.display()
            ));
            continue;
        }
        // An install whose workspace packages are links into the checkout
        // gets a folder of links instead of one link ([`mirror_plan`]), or,
        // for pnpm, a clone ([`unix::clone_install`]). A folder made from the
        // install as it is now (its stamp, [`mirror_stamp`]) is kept as is.
        let stamp = match found.kind {
            DependencyKind::NodeModules => mirror_stamp(&found.source),
            _ => None,
        };
        #[cfg(unix)]
        let current = stamp
            .as_deref()
            .is_some_and(|stamp| unix::mirror_is_current(&root, &found.rel, stamp));
        #[cfg(windows)]
        let current = stamp.as_deref().is_some_and(|stamp| {
            by_path::mirror_is_current(&by_path::Junctions, &tree_root, &found.rel, stamp)
        });
        #[cfg(not(any(unix, windows)))]
        let current = false;
        let plan = match (&stamp, current) {
            (Some(_), false) => mirror_plan(&found.source, &checkout_root),
            _ => None,
        };
        let pnpm_clone = stamp.is_some()
            && !current
            && is_pnpm(&found.source)
            && (plan.is_some() || pnpm_hoists_workspace(&found.source, &checkout_root));
        #[cfg(unix)]
        let linked = {
            let cloned = match (&stamp, pnpm_clone) {
                (Some(stamp), true) => {
                    match unix::clone_install(&root, &tree_root, &found.rel, &found.source, stamp) {
                        Ok(Ok(linked)) => Some(Ok(linked)),
                        Ok(Err(why)) => {
                            out.notes.push(pnpm_unclonable(&found.rel, &why));
                            None
                        }
                        Err(e) => Some(Err(e)),
                    }
                }
                _ => None,
            };
            match (cloned, &plan, &stamp) {
                (Some(linked), _, _) => linked,
                _ if current => Ok(Linked::Kept),
                (None, Some(plan), Some(stamp)) => {
                    unix::mirror(&root, &found.rel, &found.source, plan, stamp)
                }
                _ => unix::link(&root, &found.rel, &found.source),
            }
        };
        #[cfg(windows)]
        let linked = {
            if pnpm_clone {
                out.notes.push(pnpm_unclonable(
                    &found.rel,
                    "Windows makes no copy-on-write clone here",
                ));
            }
            match (&plan, &stamp) {
                _ if current => Ok(Linked::Kept),
                (Some(plan), Some(stamp)) => by_path::mirror(
                    &by_path::Junctions,
                    &tree_root,
                    &found.rel,
                    &found.source,
                    plan,
                    stamp,
                ),
                _ => by_path::link(&by_path::Junctions, &tree_root, &found.rel, &found.source),
            }
        };
        #[cfg(not(any(unix, windows)))]
        let linked: std::io::Result<Linked> = {
            let _ = (pnpm_clone, &plan);
            Ok(Linked::NoParent)
        };
        match linked {
            Ok(Linked::Made | Linked::Kept) => {
                if found.kind == DependencyKind::PythonVenv {
                    let dir = tree_root.join(found.rel.parent().unwrap_or(Path::new("")));
                    if !out.virtual_envs.iter().any(|v| v.dir == dir) {
                        // The checkout's folder, not the link in the tree: the
                        // worker can swap the link for a folder of its own
                        // before the command starts.
                        out.virtual_envs.push(VirtualEnv {
                            dir,
                            source: found.source.clone(),
                        });
                    }
                }
                out.reads.push(found.source);
                out.linked.push(found.rel);
            }
            Ok(Linked::Present | Linked::NoParent) => {}
            Err(e) => out.notes.push(format!(
                "cannot link the installed {} into the tree: {e}",
                found.rel.display()
            )),
        }
    }
    out
}

/// The note for a pnpm install with workspace links that could not be
/// cloned into the tree.
fn pnpm_unclonable(rel: &Path, why: &str) -> String {
    format!(
        "the pnpm install {} got a folder of links, not a clone ({why}): a workspace package \
         that another package of the install links inside node_modules/.pnpm resolves to the \
         checkout's copy, which the sandbox does not grant",
        rel.display()
    )
}

/// What one link call did.
pub(crate) enum Linked {
    /// The link was made now.
    Made,
    /// The same link was already there.
    Kept,
    /// Something else is at the path: left as it is.
    Present,
    /// The parent folder is not in the tree (or is a link): no link.
    NoParent,
}

/// One entry of a `node_modules` folder of links ([`mirror_plan`]): a link
/// at `name` (one or two components: `dep`, `@scope/pkg`, `.bin/jest`)
/// whose text is the second path, or a folder at `name` holding such links.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MirrorEntry {
    Dir(PathBuf),
    Link(PathBuf, PathBuf),
}

/// The file name of the mark kGraf leaves in a `node_modules` folder it made
/// of links, so a later call knows the folder as its own and adds what the
/// checkout installed since. A worker that writes this mark into a folder
/// of its own gains nothing: the grant is the same checkout install a link
/// would have had.
pub const MIRROR_MARK: &str = ".kgraf-dependency-mirror";

/// What a clone's mark ([`MIRROR_MARK`]) starts with, before its stamp: a
/// pnpm install kGraf cloned into the tree, whose files are all kGraf's
/// ([`is_dependency_clone`]).
const CLONE_PREFIX: &str = "clone ";

/// Whether the folder at `dir` is a pnpm install kGraf cloned into a tree:
/// its mark, a regular file, names a clone. Every file in it is a copy of
/// the user's install, not the worker's work.
pub fn is_dependency_clone(dir: &Path) -> bool {
    let mark = dir.join(MIRROR_MARK);
    std::fs::symlink_metadata(&mark).is_ok_and(|m| m.is_file())
        && std::fs::read_to_string(&mark).is_ok_and(|t| t.starts_with(CLONE_PREFIX))
}

/// What a folder of links made from the install at `source` depends on: the
/// modification times of `source`, of its `.bin` and of each `@scope`
/// folder (an install adds or removes an entry in one of them). Written in
/// the mark; a later call that reads the same stamp keeps the folder as it
/// is. `None` when `source` cannot be read.
pub(crate) fn mirror_stamp(source: &Path) -> Option<String> {
    use std::fmt::Write as _;
    let time = |meta: &std::fs::Metadata| -> u128 {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos())
    };
    let mut stamp = format!("{}", time(&std::fs::metadata(source).ok()?));
    let mut dirs: Vec<(String, u128)> = std::fs::read_dir(source)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let is_dir = e.file_type().ok()?.is_dir();
            (is_dir && (name == ".bin" || name.starts_with('@')))
                .then(|| Some((name, time(&e.metadata().ok()?))))
                .flatten()
        })
        .collect();
    dirs.sort();
    for (name, t) in dirs {
        let _ = write!(stamp, " {name}={t}");
    }
    // pnpm keeps its packages in `.pnpm` and hoists them into
    // `.pnpm/node_modules`: an install changes one of them.
    for sub in [".pnpm", ".pnpm/node_modules"] {
        if let Ok(meta) = std::fs::metadata(source.join(sub)) {
            let _ = write!(stamp, " {sub}={}", time(&meta));
        }
    }
    Some(stamp)
}

/// The path of the workspace package the link `entry` of the install at
/// `source` points at, relative to the checkout: a link whose target is a
/// folder of the checkout outside this install. `None` for any other entry.
fn workspace_target(entry: &Path, source: &Path, checkout: &Path) -> Option<PathBuf> {
    let target = std::fs::canonicalize(entry).ok()?;
    if target.starts_with(checkout) && !target.starts_with(source) {
        target.strip_prefix(checkout).ok().map(Path::to_path_buf)
    } else {
        None
    }
}

/// Whether the install at `source` is pnpm's: its packages are in a real
/// `.pnpm` folder.
fn is_pnpm(source: &Path) -> bool {
    std::fs::symlink_metadata(source.join(".pnpm")).is_ok_and(|m| m.is_dir())
}

/// Whether pnpm's hidden hoisting folder `.pnpm/node_modules` of the install
/// at `source` links a workspace package, directly or in an `@scope` folder.
fn pnpm_hoists_workspace(source: &Path, checkout: &Path) -> bool {
    let hoisted = source.join(".pnpm/node_modules");
    let Ok(entries) = std::fs::read_dir(&hoisted) else {
        return false;
    };
    let linked = |e: &std::fs::DirEntry| {
        e.file_type().is_ok_and(|k| k.is_symlink())
            && workspace_target(&e.path(), source, checkout).is_some()
    };
    entries.flatten().any(|e| {
        if linked(&e) {
            return true;
        }
        let scoped = e.file_name().to_string_lossy().starts_with('@')
            && e.file_type().is_ok_and(|k| k.is_dir());
        scoped
            && std::fs::read_dir(e.path())
                .map(|inner| inner.flatten().any(|c| linked(&c)))
                .unwrap_or(false)
    })
}

/// How a tree's `node_modules` is made when the checkout's copy holds
/// workspace packages: links (npm, pnpm and Yarn workspaces) whose target is
/// a folder of the checkout outside this install, such as
/// `node_modules/@scope/web -> ../../packages/web`. Reached through one link
/// to the checkout's folder, such a link resolves into the checkout, whose
/// files the sandbox does not grant, so a monorepo's tests failed; and it
/// would run the checkout's copy of the package, not the worker's edits.
///
/// So the tree gets a real folder: each entry a link to the checkout's
/// entry, except each workspace link, which points at the same package in
/// the tree (`../../packages/web` from the tree's `node_modules/@scope`).
/// `.bin` is a folder of the same links, its relative links copied as they
/// are, so a workspace package's program resolves to the tree's copy too.
/// An `@scope` folder holding a workspace link is a folder of links; any
/// other is one link. `None` when the install has no workspace link: one
/// link, as before.
///
/// Measured (macOS, APFS, release build, 2026-10-06, the ignored test
/// `mirror_cost`): an install of 2,000 entries takes about 0.1 s to mirror
/// the first time and about 1 ms on each later call while the install is
/// unchanged ([`mirror_stamp`]); one link costs one call.
pub(crate) fn mirror_plan(source: &Path, checkout: &Path) -> Option<Vec<MirrorEntry>> {
    let workspace_target = |entry: &Path| workspace_target(entry, source, checkout);
    let up = |levels: usize| -> PathBuf { (0..levels).map(|_| "..").collect() };
    // From a link directly in the tree's copy of `source` to the tree root.
    let depth = source.strip_prefix(checkout).ok()?.components().count();
    let mut plan = Vec::new();
    let mut workspace = false;
    let mut entries: Vec<_> = std::fs::read_dir(source).ok()?.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = PathBuf::from(entry.file_name());
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let text = name.to_string_lossy().into_owned();
        if text == MIRROR_MARK {
            continue;
        }
        if kind.is_dir() && (text == ".bin" || text.starts_with('@')) {
            let mut inner = Vec::new();
            let mut scoped_workspace = false;
            let mut children: Vec<_> = match std::fs::read_dir(&path) {
                Ok(read) => read.flatten().collect(),
                Err(_) => continue,
            };
            children.sort_by_key(std::fs::DirEntry::file_name);
            for child in children {
                let child_name = name.join(child.file_name());
                let child_path = child.path();
                let is_link = child.file_type().is_ok_and(|k| k.is_symlink());
                if text == ".bin" {
                    // A program's link, relative to `.bin`, resolves the same
                    // way in the tree's folder of links.
                    let relative = std::fs::read_link(&child_path)
                        .ok()
                        .filter(|t| is_link && t.is_relative());
                    inner.push(MirrorEntry::Link(
                        child_name,
                        relative.unwrap_or(child_path),
                    ));
                } else if let Some(rel) = is_link.then(|| workspace_target(&child_path)).flatten() {
                    scoped_workspace = true;
                    inner.push(MirrorEntry::Link(child_name, up(depth + 1).join(rel)));
                } else {
                    inner.push(MirrorEntry::Link(child_name, child_path));
                }
            }
            if text == ".bin" || scoped_workspace {
                workspace |= scoped_workspace;
                plan.push(MirrorEntry::Dir(name));
                plan.extend(inner);
            } else {
                plan.push(MirrorEntry::Link(name, path));
            }
            continue;
        }
        match kind.is_symlink().then(|| workspace_target(&path)).flatten() {
            Some(rel) => {
                workspace = true;
                plan.push(MirrorEntry::Link(name, up(depth).join(rel)));
            }
            None => plan.push(MirrorEntry::Link(name, path)),
        }
    }
    workspace.then_some(plan)
}

/// How long a search of one checkout is reused by [`link_dependency_trees`].
/// An install made while a run goes on is linked at most this long after;
/// a tree removed meanwhile is dropped at once (each one is looked at again
/// before it is linked).
pub const TREES_FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(30);

/// Checkouts whose search is kept at once; the oldest goes first.
const TREES_KEPT: usize = 16;

type Found = (Vec<DependencyTree>, Vec<String>);

/// [`find_dependency_trees`], reused for [`TREES_FRESH_FOR`]. Why a process
/// memo rather than state the caller passes: the callers (a worker's shell
/// call, a gate or verifier command, a contract check) are many and short,
/// and each already knows only the checkout path; the memo holds nothing but
/// what any of them would find again by walking. Local mutation behind one
/// lock, never held across the walk.
fn recent_trees(checkout: &Path) -> Found {
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;
    static KEPT: OnceLock<Mutex<Vec<(PathBuf, Instant, Found)>>> = OnceLock::new();
    let kept = KEPT.get_or_init(|| Mutex::new(Vec::new()));
    let key = std::fs::canonicalize(checkout).unwrap_or_else(|_| checkout.to_path_buf());
    let fresh = {
        let list = kept.lock().unwrap_or_else(|p| p.into_inner());
        list.iter()
            .find(|(k, at, _)| *k == key && at.elapsed() < TREES_FRESH_FOR)
            .map(|(_, _, found)| found.clone())
    };
    if let Some((trees, notes)) = fresh {
        // A tree removed since the search is not linked.
        let trees = trees
            .into_iter()
            .filter(|t| std::fs::symlink_metadata(&t.source).is_ok())
            .collect();
        return (trees, notes);
    }
    let found = find_dependency_trees(&key);
    let mut list = kept.lock().unwrap_or_else(|p| p.into_inner());
    list.retain(|(k, _, _)| *k != key);
    if list.len() >= TREES_KEPT {
        list.remove(0);
    }
    list.push((key, Instant::now(), found.clone()));
    found
}

#[cfg(unix)]
mod unix {
    use cap_primitives::fs::{FollowSymlinks, open_dir_nofollow, stat};
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path, PathBuf};

    use super::Linked;

    fn symlink_at(dir: &File, name: &Path, target: &Path) -> io::Result<()> {
        let to = CString::new(target.as_os_str().as_bytes())?;
        let name = CString::new(name.as_os_str().as_bytes())?;
        // SAFETY: both strings are NUL-terminated and `dir` is open.
        let rc = unsafe { libc::symlinkat(to.as_ptr(), dir.as_raw_fd(), name.as_ptr()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn read_link_at(dir: &File, name: &Path) -> Option<Vec<u8>> {
        let name = CString::new(name.as_os_str().as_bytes()).ok()?;
        let mut buf = vec![0u8; 4096];
        // SAFETY: `dir` is an open directory, `name` is NUL-terminated and
        // `buf` is valid for `buf.len()` bytes.
        let n = unsafe {
            libc::readlinkat(
                dir.as_raw_fd(),
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        let n = usize::try_from(n).ok()?;
        buf.truncate(n);
        Some(buf)
    }

    /// The folder holding `rel`'s last component, reached one no-follow
    /// `openat` at a time, and that component; `None` when a folder on the
    /// way is absent or a link.
    fn open_parent<'a>(root: &File, rel: &'a Path) -> io::Result<Option<(File, &'a Path)>> {
        let mut names = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(n) => names.push(n),
                _ => return Err(io::Error::other("not a plain relative path")),
            }
        }
        let Some((last, parents)) = names.split_last() else {
            return Err(io::Error::other("an empty path"));
        };
        let last: &'a std::ffi::OsStr = last;
        let mut dir = root.try_clone()?;
        for name in parents {
            dir = match open_dir_nofollow(&dir, Path::new(name)) {
                Ok(d) => d,
                Err(_) => return Ok(None),
            };
        }
        Ok(Some((dir, Path::new(last))))
    }

    /// Whether `rel` beneath `root` is kGraf's folder of links whose mark
    /// holds `stamp` ([`super::mirror_stamp`]), reached without following a
    /// link.
    pub(super) fn mirror_is_current(root: &File, rel: &Path, stamp: &str) -> bool {
        use cap_primitives::fs::{OpenOptions, open};
        use std::io::Read;
        let Ok(Some((parent, last))) = open_parent(root, rel) else {
            return false;
        };
        let Ok(dir) = open_dir_nofollow(&parent, last) else {
            return false;
        };
        let mut options = OpenOptions::new();
        options.read(true)._cap_fs_ext_follow(FollowSymlinks::No);
        let Ok(mut mark) = open(&dir, Path::new(super::MIRROR_MARK), &options) else {
            return false;
        };
        let mut text = String::new();
        mark.read_to_string(&mut text).is_ok()
            && (text == stamp || text.strip_prefix(super::CLONE_PREFIX) == Some(stamp))
    }

    /// Make `rel` beneath `root` a folder of links ([`super::mirror_plan`]),
    /// or bring kGraf's own such folder up to date with `plan` (an entry
    /// already there is left as it is), then write `stamp` in its mark. A
    /// link kGraf made earlier to `source` as a whole is replaced. Every
    /// folder is reached and every link made relative to an open
    /// descriptor, never through a link.
    pub(super) fn mirror(
        root: &File,
        rel: &Path,
        source: &Path,
        plan: &[super::MirrorEntry],
        stamp: &str,
    ) -> io::Result<Linked> {
        use cap_primitives::fs::{DirOptions, OpenOptions, create_dir, open};
        let Some((parent, last)) = open_parent(root, rel)? else {
            return Ok(Linked::NoParent);
        };
        let mark = Path::new(super::MIRROR_MARK);
        let made = match stat(&parent, last, FollowSymlinks::No) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if read_link_at(&parent, last).as_deref() != Some(source.as_os_str().as_bytes()) {
                    return Ok(Linked::Present);
                }
                // kGraf's own link from before the install had workspace
                // packages: replaced by the folder.
                cap_primitives::fs::remove_file(&parent, last)?;
                true
            }
            Ok(meta) if meta.is_dir() => false,
            Ok(_) => return Ok(Linked::Present),
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(e) => return Err(e),
        };
        if made {
            match create_dir(&parent, last, &DirOptions::new()) {
                Ok(()) => {}
                // Another call made it first; judged by its mark below.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        let Ok(dir) = open_dir_nofollow(&parent, last) else {
            return Ok(Linked::Present);
        };
        if !made && !stat(&dir, mark, FollowSymlinks::No).is_ok_and(|m| m.is_file()) {
            // A folder that is not kGraf's (the worker's own install).
            return Ok(Linked::Present);
        }
        let mut sub: Option<(PathBuf, File)> = None;
        for entry in plan {
            match entry {
                super::MirrorEntry::Dir(name) => {
                    match create_dir(&dir, name, &DirOptions::new()) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e),
                    }
                    sub = open_dir_nofollow(&dir, name)
                        .ok()
                        .map(|f| (name.clone(), f));
                }
                super::MirrorEntry::Link(name, text) => {
                    let Some(file_name) = name.file_name() else {
                        continue;
                    };
                    let at = match (name.parent(), &sub) {
                        (Some(p), _) if p.as_os_str().is_empty() => &dir,
                        (Some(p), Some((open_name, f))) if p == open_name => f,
                        // Its folder could not be opened (a link there): skipped.
                        _ => continue,
                    };
                    match symlink_at(at, Path::new(file_name), text) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        // The stamp last: a call cut short leaves an old stamp, and the next
        // call completes the folder.
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create(true)
            .truncate(true)
            ._cap_fs_ext_follow(FollowSymlinks::No);
        std::io::Write::write_all(&mut open(&dir, mark, &options)?, stamp.as_bytes())?;
        Ok(if made { Linked::Made } else { Linked::Kept })
    }

    fn rename_at(from_dir: &File, from: &Path, to_dir: &File, to: &Path) -> io::Result<()> {
        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: both strings are NUL-terminated and both folders are open.
        let rc = unsafe {
            libc::renameat(
                from_dir.as_raw_fd(),
                from.as_ptr(),
                to_dir.as_raw_fd(),
                to.as_ptr(),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Make `rel` beneath `root` a copy-on-write clone of the install at
    /// `source` (pnpm's, [`super::is_pnpm`]), or replace kGraf's own link or
    /// folder of links there with one; then write `stamp` in its mark.
    /// `Ok(Err(why))`: no clone can be made here (another volume, no
    /// copy-on-write), and nothing was changed.
    ///
    /// The clone is made in a new folder beside the tree (`tree`'s parent:
    /// the workspace base or a check run's folder, which no sandboxed
    /// command of this tree writes), then moved into place with `renameat`
    /// relative to folders opened without following links, so a folder the
    /// worker swapped for a link never makes the host write outside the
    /// tree. What it replaces is moved out the same way and removed there.
    pub(super) fn clone_install(
        root: &File,
        tree: &Path,
        rel: &Path,
        source: &Path,
        stamp: &str,
    ) -> io::Result<Result<Linked, String>> {
        use cap_primitives::ambient_authority;
        use cap_primitives::fs::open_ambient_dir;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let Some((parent, last)) = open_parent(root, rel)? else {
            return Ok(Ok(Linked::NoParent));
        };
        let mark = Path::new(super::MIRROR_MARK);
        let replaces = match stat(&parent, last, FollowSymlinks::No) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if read_link_at(&parent, last).as_deref() != Some(source.as_os_str().as_bytes()) {
                    return Ok(Ok(Linked::Present));
                }
                true
            }
            Ok(meta) if meta.is_dir() => {
                let ours = open_dir_nofollow(&parent, last)
                    .is_ok_and(|d| stat(&d, mark, FollowSymlinks::No).is_ok_and(|m| m.is_file()));
                if !ours {
                    // A folder that is not kGraf's (the worker's own install).
                    return Ok(Ok(Linked::Present));
                }
                true
            }
            Ok(_) => return Ok(Ok(Linked::Present)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        let Some(beside) = tree.parent() else {
            return Ok(Err("the tree has no parent folder".into()));
        };
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let staging = beside.join(format!(".kgraf-install-{}-{nanos}-{n}", std::process::id()));
        std::fs::create_dir(&staging)?;
        let result = (|| -> io::Result<Result<Linked, String>> {
            let made = staging.join("install");
            if let Err(why) = crate::build_dir::clone_tree(source, &made) {
                return Ok(Err(why));
            }
            std::fs::write(made.join(mark), format!("{}{stamp}", super::CLONE_PREFIX))?;
            let staged = open_ambient_dir(&staging, ambient_authority())?;
            if replaces {
                rename_at(&parent, last, &staged, Path::new("replaced"))?;
            }
            match rename_at(&staged, Path::new("install"), &parent, last) {
                Ok(()) => Ok(Ok(Linked::Made)),
                // Something was put there meanwhile: left as it is.
                Err(e)
                    if matches!(
                        e.raw_os_error(),
                        Some(libc::EEXIST | libc::ENOTEMPTY | libc::ENOTDIR | libc::EISDIR)
                    ) =>
                {
                    Ok(Ok(Linked::Present))
                }
                Err(e) => Err(e),
            }
        })();
        // The staging folder is the host's; nothing in it is followed.
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    /// Make `rel` beneath `root` a symbolic link to `target`, reaching its
    /// parent one no-follow `openat` at a time and creating the link relative
    /// to that descriptor (`symlinkat`): a folder the worker swapped for a
    /// link can never make the host create the link outside the tree.
    pub(super) fn link(root: &File, rel: &Path, target: &Path) -> io::Result<Linked> {
        let mut names = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(n) => names.push(n),
                _ => return Err(io::Error::other("not a plain relative path")),
            }
        }
        let Some((last, parents)) = names.split_last() else {
            return Err(io::Error::other("an empty path"));
        };
        let mut dir = root.try_clone()?;
        for name in parents {
            dir = match open_dir_nofollow(&dir, Path::new(name)) {
                Ok(d) => d,
                Err(_) => return Ok(Linked::NoParent),
            };
        }
        let last = Path::new(last);
        match stat(&dir, last, FollowSymlinks::No) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let name = CString::new(last.as_os_str().as_bytes())?;
                let mut buf = vec![0u8; 4096];
                // SAFETY: `dir` is an open directory, `name` is NUL-terminated
                // and `buf` is valid for `buf.len()` bytes.
                let n = unsafe {
                    libc::readlinkat(
                        dir.as_raw_fd(),
                        name.as_ptr(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                    )
                };
                if n >= 0 && &buf[..n as usize] == target.as_os_str().as_bytes() {
                    return Ok(Linked::Kept);
                }
                Ok(Linked::Present)
            }
            Ok(_) => Ok(Linked::Present),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let to = CString::new(target.as_os_str().as_bytes())?;
                let name = CString::new(last.as_os_str().as_bytes())?;
                // SAFETY: both strings are NUL-terminated and `dir` is open.
                let rc = unsafe { libc::symlinkat(to.as_ptr(), dir.as_raw_fd(), name.as_ptr()) };
                if rc == 0 {
                    Ok(Linked::Made)
                } else {
                    let e = io::Error::last_os_error();
                    // Another call linked it first.
                    if e.kind() == io::ErrorKind::AlreadyExists {
                        Ok(Linked::Present)
                    } else {
                        Err(e)
                    }
                }
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    /// A checkout with a root npm package, a nested one, a root venv, a
    /// Composer vendor folder, a Go project whose vendor folder is committed
    /// in the tree, and trees that must not be found.
    fn checkout() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("dir");
        let c = dir.path();
        write(&c.join("package.json"), "{}");
        write(&c.join("node_modules/dep/index.js"), "module.exports = 1;");
        write(&c.join("packages/web/package.json"), "{}");
        write(&c.join("packages/web/node_modules/w/index.js"), "");
        write(&c.join(".venv/pyvenv.cfg"), "home = /usr/bin\n");
        write(&c.join("php/composer.json"), "{}");
        write(&c.join("php/vendor/autoload.php"), "<?php");
        write(&c.join("gosvc/go.mod"), "module x\n");
        write(&c.join("gosvc/vendor/modules.txt"), "");
        // No manifest beside it: not an install.
        write(&c.join("loose/node_modules/x/index.js"), "");
        // A folder named venv without pyvenv.cfg: not a virtual environment.
        write(&c.join("tool/venv/readme"), "");
        // kGraf's own state is never searched.
        write(&c.join(".kgraf/w/package.json"), "{}");
        write(&c.join(".kgraf/w/node_modules/s/index.js"), "");
        dir
    }

    fn rels(trees: &[DependencyTree]) -> Vec<String> {
        let mut out: Vec<String> = trees
            .iter()
            .map(|t| t.rel.to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn installs_are_found_beside_their_manifests_only() {
        let c = checkout();
        let (trees, notes) = find_dependency_trees(c.path());
        assert_eq!(
            rels(&trees),
            [
                ".venv",
                "gosvc/vendor",
                "node_modules",
                "packages/web/node_modules",
                "php/vendor"
            ],
            "{notes:?}"
        );
        let real = std::fs::canonicalize(c.path()).expect("canonical");
        assert!(trees.iter().all(|t| t.source.starts_with(&real)));
    }

    #[test]
    fn a_tree_that_is_a_link_is_named_and_never_followed() {
        let c = tempfile::tempdir().expect("dir");
        let elsewhere = tempfile::tempdir().expect("dir");
        write(&c.path().join("package.json"), "{}");
        std::os::unix::fs::symlink(elsewhere.path(), c.path().join("node_modules")).expect("ln");
        let (trees, notes) = find_dependency_trees(c.path());
        assert!(trees.is_empty(), "{trees:?}");
        assert!(
            notes
                .iter()
                .any(|n| n.contains("node_modules is a symbolic link")),
            "{notes:?}"
        );
    }

    #[test]
    fn links_are_made_where_the_tree_lacks_them_and_kept_on_a_second_call() {
        let c = checkout();
        let tree = tempfile::tempdir().expect("tree");
        let t = tree.path();
        write(&t.join("package.json"), "{}");
        write(&t.join("packages/web/package.json"), "{}");
        // Committed in the tree: no link, no grant.
        write(&t.join("gosvc/go.mod"), "module x\n");
        write(&t.join("gosvc/vendor/modules.txt"), "");
        // `php/` is not in this tree: no link without its folder.
        let first = link_dependency_trees(t, c.path());
        let mut linked: Vec<String> = first
            .linked
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        linked.sort();
        assert_eq!(
            linked,
            [".venv", "node_modules", "packages/web/node_modules"],
            "{first:?}"
        );
        assert_eq!(first.reads.len(), 3);
        assert_eq!(
            first.virtual_env_for(t),
            Some(
                std::fs::canonicalize(c.path().join(".venv"))
                    .expect("venv")
                    .as_path()
            )
        );
        assert!(
            std::fs::symlink_metadata(t.join("node_modules"))
                .expect("link")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(t.join("node_modules/dep/index.js")).expect("read"),
            "module.exports = 1;"
        );
        assert!(
            std::fs::symlink_metadata(t.join("gosvc/vendor"))
                .expect("dir")
                .is_dir()
        );
        let second = link_dependency_trees(t, c.path());
        assert_eq!(second.linked.len(), 3, "{second:?}");
        assert_eq!(second.reads, first.reads);
    }

    #[test]
    fn a_worker_s_own_folder_is_left_and_a_linked_parent_is_never_entered() {
        let c = checkout();
        let tree = tempfile::tempdir().expect("tree");
        let outside = tempfile::tempdir().expect("outside");
        let t = tree.path();
        write(&t.join("package.json"), "{}");
        write(&t.join("node_modules/own/index.js"), "");
        // `packages` swapped for a link to a folder outside the tree.
        write(&outside.path().join("web/package.json"), "{}");
        std::os::unix::fs::symlink(outside.path(), t.join("packages")).expect("ln");
        let links = link_dependency_trees(t, c.path());
        assert!(
            !links
                .linked
                .iter()
                .any(|p| p.ends_with("node_modules") && p.components().count() == 1),
            "{links:?}"
        );
        assert!(t.join("node_modules/own/index.js").is_file());
        assert!(
            !outside.path().join("web/node_modules").exists(),
            "a link was made outside the tree"
        );
    }

    /// The walk's cost at its bound: a checkout of [`MAX_DIRS`] folders,
    /// searched whole. Ignored by default (it makes 20,000 folders); run with
    /// `cargo test -p kgraf-sandbox --release walk_cost -- --ignored
    /// --nocapture` to print the time.
    #[test]
    #[ignore = "a measurement: makes 20,000 folders"]
    fn walk_cost_at_the_folder_bound() {
        let c = tempfile::tempdir().expect("dir");
        for a in 0..100 {
            for b in 0..199 {
                std::fs::create_dir_all(c.path().join(format!("p{a}/m{b}"))).expect("mkdir");
            }
        }
        write(&c.path().join("p0/m0/package.json"), "{}");
        write(&c.path().join("p0/m0/node_modules/x/index.js"), "");
        let started = std::time::Instant::now();
        let (trees, notes) = find_dependency_trees(c.path());
        let warm = std::time::Instant::now();
        let _ = find_dependency_trees(c.path());
        println!(
            "first {:?}, again {:?}, {} tree(s), notes {notes:?}",
            warm - started,
            warm.elapsed(),
            trees.len()
        );
        assert_eq!(trees.len(), 1);
    }

    /// The reused search never links a tree removed since it ran: a link to
    /// a missing target would make the sandbox refuse the command.
    #[test]
    fn a_tree_removed_since_the_search_is_not_linked() {
        let c = tempfile::tempdir().expect("dir");
        write(&c.path().join("package.json"), "{}");
        write(&c.path().join("node_modules/x/index.js"), "");
        let first = tempfile::tempdir().expect("tree");
        write(&first.path().join("package.json"), "{}");
        assert_eq!(
            link_dependency_trees(first.path(), c.path()).linked.len(),
            1
        );
        std::fs::remove_dir_all(c.path().join("node_modules")).expect("rm");
        let second = tempfile::tempdir().expect("tree");
        write(&second.path().join("package.json"), "{}");
        let links = link_dependency_trees(second.path(), c.path());
        assert!(
            links.linked.is_empty() && links.reads.is_empty(),
            "{links:?}"
        );
        assert!(std::fs::symlink_metadata(second.path().join("node_modules")).is_err());
    }

    /// A tree that became a link after the search (the reused search, or a
    /// swap between the search and the grant) is never granted: the grant
    /// is resolved when the sandbox is built, so the link's target, a folder
    /// outside the checkout, would become readable.
    #[test]
    fn a_tree_swapped_for_a_link_after_the_search_is_never_granted() {
        let c = tempfile::tempdir().expect("dir");
        let outside = tempfile::tempdir().expect("outside");
        write(&outside.path().join("secret"), "FAKE");
        write(&c.path().join("package.json"), "{}");
        write(&c.path().join("node_modules/x/index.js"), "");
        let first = tempfile::tempdir().expect("tree");
        write(&first.path().join("package.json"), "{}");
        assert_eq!(link_dependency_trees(first.path(), c.path()).reads.len(), 1);
        std::fs::remove_dir_all(c.path().join("node_modules")).expect("rm");
        std::os::unix::fs::symlink(outside.path(), c.path().join("node_modules")).expect("ln");
        let second = tempfile::tempdir().expect("tree");
        write(&second.path().join("package.json"), "{}");
        let links = link_dependency_trees(second.path(), c.path());
        assert!(links.reads.is_empty(), "{links:?}");
        assert!(
            links.notes.iter().any(|n| n.contains("node_modules")),
            "{links:?}"
        );
        assert!(std::fs::symlink_metadata(second.path().join("node_modules")).is_err());
    }

    /// The activated virtual environment is the checkout's own folder, never
    /// the link in the tree: the worker can replace that link (between the
    /// link and the command's start, from a parallel call) with a folder of
    /// its own whose `bin` would then come first on `PATH`.
    #[test]
    fn the_activated_environment_is_the_checkout_s_folder() {
        let c = tempfile::tempdir().expect("dir");
        write(&c.path().join(".venv/pyvenv.cfg"), "home = /usr/bin\n");
        let tree = tempfile::tempdir().expect("tree");
        let links = link_dependency_trees(tree.path(), c.path());
        let real = std::fs::canonicalize(c.path().join(".venv")).expect("canonical");
        assert_eq!(links.virtual_env_for(tree.path()), Some(real.as_path()));
    }

    /// An npm, pnpm or Yarn workspace: the checkout's `node_modules` holds
    /// links to the workspace packages (`@s/web -> ../../packages/web`).
    /// The tree gets a folder of links in which those resolve to the tree's
    /// own packages (the worker's edits), and every other entry to the
    /// checkout's install. A second call keeps it and adds what was
    /// installed since; a program in `.bin` that is a workspace package's
    /// resolves to the tree's copy.
    #[test]
    fn workspace_package_links_resolve_to_the_tree_s_own_packages() {
        let c = tempfile::tempdir().expect("dir");
        let c = std::fs::canonicalize(c.keep()).expect("canonical");
        write(&c.join("package.json"), r#"{"workspaces":["packages/*"]}"#);
        write(&c.join("packages/web/index.js"), "checkout");
        write(&c.join("packages/web/cli.js"), "checkout cli");
        write(&c.join("packages/api/index.js"), "checkout api");
        write(&c.join("node_modules/dep/index.js"), "dep");
        write(&c.join("node_modules/@other/x/index.js"), "x");
        std::fs::create_dir_all(c.join("node_modules/@s")).expect("mkdir");
        std::os::unix::fs::symlink("../../packages/web", c.join("node_modules/@s/web"))
            .expect("ln");
        std::os::unix::fs::symlink("../packages/api", c.join("node_modules/api")).expect("ln");
        std::fs::create_dir_all(c.join("node_modules/.bin")).expect("mkdir");
        std::os::unix::fs::symlink("../@s/web/cli.js", c.join("node_modules/.bin/webcli"))
            .expect("ln");
        let tree = tempfile::tempdir().expect("tree");
        let t = std::fs::canonicalize(tree.path()).expect("canonical");
        write(&t.join("package.json"), r#"{"workspaces":["packages/*"]}"#);
        write(&t.join("packages/web/index.js"), "tree");
        write(&t.join("packages/web/cli.js"), "tree cli");
        write(&t.join("packages/api/index.js"), "tree api");

        let links = link_dependency_trees(&t, &c);
        assert_eq!(links.reads, vec![c.join("node_modules")], "{links:?}");
        let nm = t.join("node_modules");
        let meta = std::fs::symlink_metadata(&nm).expect("node_modules");
        assert!(meta.is_dir(), "a folder of links, not one link");
        assert!(nm.join(MIRROR_MARK).is_file());
        let read = |rel: &str| std::fs::read_to_string(nm.join(rel)).expect(rel);
        assert_eq!(read("@s/web/index.js"), "tree");
        assert_eq!(read("api/index.js"), "tree api");
        assert_eq!(read(".bin/webcli"), "tree cli");
        assert_eq!(read("dep/index.js"), "dep");
        assert_eq!(read("@other/x/index.js"), "x");
        // `@other` holds no workspace link: one link to the checkout's.
        assert!(
            std::fs::symlink_metadata(nm.join("@other"))
                .expect("@other")
                .file_type()
                .is_symlink()
        );

        write(&c.join("node_modules/later/index.js"), "later");
        let again = link_dependency_trees(&t, &c);
        assert_eq!(again.reads, links.reads, "{again:?}");
        assert_eq!(read("later/index.js"), "later");
        assert_eq!(read("@s/web/index.js"), "tree");
        let _ = std::fs::remove_dir_all(&c);
    }

    /// A worker's own `node_modules` folder (no mark) is left as it is and
    /// granted nothing, as with a single link.
    #[test]
    fn a_worker_s_own_node_modules_is_never_turned_into_a_mirror() {
        let c = tempfile::tempdir().expect("dir");
        write(&c.path().join("package.json"), "{}");
        write(&c.path().join("packages/web/index.js"), "checkout");
        std::fs::create_dir_all(c.path().join("node_modules")).expect("mkdir");
        std::os::unix::fs::symlink("../packages/web", c.path().join("node_modules/web"))
            .expect("ln");
        let tree = tempfile::tempdir().expect("tree");
        write(&tree.path().join("package.json"), "{}");
        write(&tree.path().join("node_modules/own/index.js"), "own");
        let links = link_dependency_trees(tree.path(), c.path());
        assert!(links.reads.is_empty(), "{links:?}");
        assert!(!tree.path().join("node_modules").join(MIRROR_MARK).exists());
        assert!(std::fs::symlink_metadata(tree.path().join("node_modules/web")).is_err());
    }

    /// A command's virtual environment is the one owning its folder, or the
    /// nearest above it; a sibling project's is not its.
    /// A pnpm workspace install, laid out by hand as pnpm documents it
    /// ("Symlinked `node_modules` structure"; pnpm is not installed here):
    /// every package a real folder in `.pnpm/<name>@<version>/node_modules`,
    /// the root's entries relative links into it, the workspace packages
    /// linked at the root and in the hidden hoisting folder
    /// `.pnpm/node_modules`, all links relative.
    fn pnpm_checkout() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("dir");
        let c = std::fs::canonicalize(dir.path()).expect("canonical");
        write(&c.join("package.json"), r#"{"name":"root","private":true}"#);
        write(
            &c.join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\n",
        );
        write(&c.join("packages/web/index.js"), "checkout web");
        write(&c.join("packages/util/index.js"), "checkout util");
        let nm = c.join("node_modules");
        write(&nm.join(".modules.yaml"), "layoutVersion: 5\n");
        // A third-party package that requires the workspace package
        // `@s/util`, found through the hoisting folder.
        write(
            &nm.join(".pnpm/dep@1.0.0/node_modules/dep/index.js"),
            "module.exports = require('@s/util');",
        );
        let link = |target: &str, at: &Path| {
            std::fs::create_dir_all(at.parent().expect("parent")).expect("mkdir");
            std::os::unix::fs::symlink(target, at).expect("ln");
        };
        link(
            "../../../../packages/util",
            &nm.join(".pnpm/node_modules/@s/util"),
        );
        link(
            "../dep@1.0.0/node_modules/dep",
            &nm.join(".pnpm/node_modules/dep"),
        );
        link(".pnpm/dep@1.0.0/node_modules/dep", &nm.join("dep"));
        link("../../packages/web", &nm.join("@s/web"));
        (dir, c)
    }

    /// Node resolves a package from its real path. With the install linked
    /// (one link or a folder of links), `dep` resolves into the checkout's
    /// `.pnpm`, and its `require('@s/util')` walks the checkout's folders to
    /// the checkout's `packages/util`. The tree gets a clone, so `dep`'s real
    /// path is in the tree and the hoisted workspace link resolves to the
    /// tree's own `packages/util`, as the root's `@s/web` does.
    #[test]
    fn pnpm_workspace_links_inside_the_store_resolve_to_the_tree_s_packages() {
        let (_c, c) = pnpm_checkout();
        // The tree in a folder of its own, so the check below of what is left
        // beside it sees no other test's staging folder.
        let outer = tempfile::tempdir().expect("outer");
        let t = std::fs::canonicalize(outer.path())
            .expect("canonical")
            .join("tree");
        write(&t.join("package.json"), r#"{"name":"root","private":true}"#);
        write(&t.join("packages/web/index.js"), "tree web");
        write(&t.join("packages/util/index.js"), "tree util");

        let links = link_dependency_trees(&t, &c);
        assert_eq!(
            links.linked,
            vec![PathBuf::from("node_modules")],
            "{links:?}"
        );
        let nm = t.join("node_modules");
        assert!(nm.join(MIRROR_MARK).is_file(), "{links:?}");
        // Where Node finds `dep`, and the folders it then looks in for
        // `@s/util`, as Node's own lookup from `dep`'s real path does.
        let dep = std::fs::canonicalize(nm.join("dep")).expect("dep");
        assert!(
            dep.starts_with(&t),
            "dep resolves into the checkout: {}",
            dep.display()
        );
        // Node's lookup folders: `<a>/node_modules` for each folder `a` above
        // the package's real path that is not itself a `node_modules`.
        let util = dep
            .ancestors()
            .filter(|a| a.file_name().is_some_and(|n| n != "node_modules"))
            .map(|a| a.join("node_modules/@s/util/index.js"))
            .find(|p| p.is_file())
            .expect("@s/util is found from dep");
        assert_eq!(std::fs::read_to_string(&util).expect("read"), "tree util");
        assert_eq!(
            std::fs::read_to_string(nm.join("@s/web/index.js")).expect("read"),
            "tree web"
        );
        // Kept on a second call; the checkout's install is unchanged.
        let again = link_dependency_trees(&t, &c);
        assert_eq!(again.linked, links.linked);
        assert!(
            std::fs::symlink_metadata(c.join("node_modules/dep"))
                .expect("checkout dep")
                .file_type()
                .is_symlink()
        );
        assert!(!c.join("node_modules").join(MIRROR_MARK).exists());
        // Nothing is left beside the tree.
        let beside: Vec<_> = std::fs::read_dir(t.parent().expect("parent"))
            .expect("read")
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".kgraf-install-")
            })
            .collect();
        assert!(beside.is_empty(), "{beside:?}");
    }

    /// A worker's own `node_modules` folder in a pnpm workspace is left as
    /// it is; kGraf's earlier folder of links is replaced by the clone.
    #[test]
    fn a_pnpm_clone_replaces_only_kgraf_s_own_install() {
        let (_c, c) = pnpm_checkout();
        let own = tempfile::tempdir().expect("tree");
        write(&own.path().join("package.json"), "{}");
        write(&own.path().join("node_modules/own/index.js"), "own");
        let links = link_dependency_trees(own.path(), &c);
        assert!(links.linked.is_empty(), "{links:?}");
        assert!(!own.path().join("node_modules").join(MIRROR_MARK).exists());

        let tree = tempfile::tempdir().expect("tree");
        write(&tree.path().join("package.json"), "{}");
        std::fs::create_dir_all(tree.path().join("node_modules")).expect("mkdir");
        write(
            &tree.path().join("node_modules").join(MIRROR_MARK),
            "old stamp",
        );
        let links = link_dependency_trees(tree.path(), &c);
        assert_eq!(
            links.linked,
            vec![PathBuf::from("node_modules")],
            "{links:?}"
        );
        // A clone: `.pnpm` is a real folder in the tree, not a link.
        assert!(
            std::fs::symlink_metadata(tree.path().join("node_modules/.pnpm"))
                .expect(".pnpm")
                .is_dir()
        );
    }

    #[test]
    fn the_virtual_environment_is_the_nearest_one_above_the_command_folder() {
        let c = tempfile::tempdir().expect("dir");
        write(&c.path().join(".venv/pyvenv.cfg"), "home = /usr/bin\n");
        write(&c.path().join("svc/api/pyproject.toml"), "");
        write(
            &c.path().join("svc/api/.venv/pyvenv.cfg"),
            "home = /usr/bin\n",
        );
        write(&c.path().join("svc/web/pyproject.toml"), "");
        let tree = tempfile::tempdir().expect("tree");
        let t = std::fs::canonicalize(tree.path()).expect("canonical");
        write(&t.join("svc/api/pyproject.toml"), "");
        write(&t.join("svc/web/pyproject.toml"), "");
        let links = link_dependency_trees(&t, c.path());
        let root_env = std::fs::canonicalize(c.path().join(".venv")).expect("root");
        let api_env = std::fs::canonicalize(c.path().join("svc/api/.venv")).expect("api");
        assert_eq!(links.virtual_env_for(&t), Some(root_env.as_path()));
        assert_eq!(
            links.virtual_env_for(&t.join("svc/api/tests")),
            Some(api_env.as_path())
        );
        assert_eq!(
            links.virtual_env_for(&t.join("svc/web")),
            Some(root_env.as_path())
        );
    }

    #[test]
    fn a_script_s_start_folder_is_read_from_a_leading_cd_only() {
        for (script, dir) in [
            ("cd svc/api && python -m pytest", Some("svc/api")),
            ("cd svc/api; pytest", Some("svc/api")),
            ("  cd ./web\nnpm test", Some("./web")),
            ("pytest && cd x", None),
            ("cd ../x && pytest", None),
            ("cd /abs && pytest", None),
            ("cd \"a b\" && pytest", None),
            ("cd $HOME && pytest", None),
            ("cd", None),
        ] {
            assert_eq!(
                script_start_dir(script).as_deref(),
                dir.map(Path::new),
                "{script}"
            );
        }
    }

    /// The cost of a folder of links at 2,000 entries, made and then brought
    /// up to date. Ignored by default; run with `cargo test -p kgraf-sandbox
    /// --release mirror_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement: makes 2,000 entries"]
    fn mirror_cost() {
        let c = tempfile::tempdir().expect("dir");
        write(&c.path().join("package.json"), "{}");
        write(&c.path().join("packages/web/index.js"), "");
        for i in 0..2_000 {
            std::fs::create_dir_all(c.path().join(format!("node_modules/p{i}"))).expect("mkdir");
        }
        std::os::unix::fs::symlink("../packages/web", c.path().join("node_modules/web"))
            .expect("ln");
        let tree = tempfile::tempdir().expect("tree");
        write(&tree.path().join("package.json"), "{}");
        write(&tree.path().join("packages/web/index.js"), "");
        let started = std::time::Instant::now();
        let first = link_dependency_trees(tree.path(), c.path());
        let made = started.elapsed();
        let started = std::time::Instant::now();
        let _ = link_dependency_trees(tree.path(), c.path());
        println!("made {made:?}, brought up to date {:?}", started.elapsed());
        assert_eq!(first.reads.len(), 1);
    }

    #[test]
    fn the_search_is_bounded_in_depth() {
        let c = tempfile::tempdir().expect("dir");
        let mut deep = c.path().to_path_buf();
        for i in 0..=MAX_DEPTH {
            deep.push(format!("d{i}"));
        }
        write(&deep.join("package.json"), "{}");
        write(&deep.join("node_modules/x/index.js"), "");
        let (trees, _) = find_dependency_trees(c.path());
        assert!(trees.is_empty(), "{trees:?}");
    }
}
