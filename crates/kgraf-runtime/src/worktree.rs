//! What a worktree looks like to git after a call that may have written
//! anywhere inside it.
//!
//! A sandboxed build or test command declares no paths: the sandbox confines its
//! writes to the worktree, but the files it changed are not known from the call.
//! The loop asks git after such a call, so the handoff and the result list what
//! the command changed (a lockfile, a generated file, a file a test wrote).
//! Ignored paths (a build directory) are not listed: git does not report them.
//!
//! The read runs `git` on the host, outside the sandbox, over a tree a
//! sandboxed command could write. `git status` runs programs its
//! configuration names: an fsmonitor hook, and a clean filter for a file whose
//! stat data changed (measured: a same-size rewrite of a filtered file runs
//! the filter). So the read runs only where every configuration git reads is
//! out of the command's reach:
//!
//! - the worktree root holds a gitlink file (a linked worktree), and the git
//!   directory and common directory it names resolve outside the root; a
//!   `.git` directory inside the root is refused, since its config is as
//!   writable as the tree;
//! - the gitlink sits at the root itself and is the one read when the worker
//!   first started, before any call ran; a resumed worker takes that record
//!   from its transcript and never trusts a `.git` found on disk at resume.
//!   The one exception is a transcript written before the gitlink was
//!   recorded at all: its resume reads the gitlink the host's reopen has just
//!   verified against the host's own record, and records it from then on.
//!   A root below the top of its work tree has no gitlink of its own, and a
//!   `.git` found there later may have been planted, so it is not read;
//! - submodules are not entered (`--ignore-submodules=all`): a submodule's
//!   git directory is inside the tree, so a planted one could name a filter;
//! - git runs through the host runner (`kgraf_core::host_git`): hooks
//!   (file and config-defined) and fsmonitor are off, every filter, merge and
//!   diff driver the config defines is neutralized by name, and no lazy fetch
//!   or transport may run.
//!
//! An in-tree `.gitattributes` can still name a filter driver, but none runs:
//! the host runner empties every configured driver's command.

use std::path::{Path, PathBuf};

/// The `.git` entry of a worker's root as it was when the worker started,
/// before any of its calls ran: the text of the gitlink file at the root.
///
/// A sandboxed command can write its worktree, and on a backend without
/// gitlink protection that includes the `.git` file. Rewritten, it could aim
/// the host-side status read at a git directory the command planted, whose
/// configuration names programs git runs. So the status read runs against
/// the git directory recorded here, and not at all once the entry changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitEntry {
    /// The worktree root that holds the gitlink.
    root: PathBuf,
    /// The gitlink's text, trimmed (`gitdir: <admin dir>`).
    gitlink: String,
}

impl GitEntry {
    /// An entry recorded earlier (the gitlink text a first segment stored),
    /// for the worktree at `root`.
    pub fn recorded(root: &Path, gitlink: String) -> Self {
        Self {
            root: root.to_path_buf(),
            gitlink,
        }
    }

    /// The gitlink's text, trimmed.
    pub fn gitlink(&self) -> &str {
        &self.gitlink
    }
}

/// The largest `.git` or `commondir` file read; each is one short line.
const MAX_LINK_BYTES: u64 = 4096;

/// What a status read found under the root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeStatus {
    /// Paths changed, deleted or untracked, relative to the root. A rename
    /// lists both the old and the new path.
    pub paths: Vec<PathBuf>,
    /// Untracked directories that hold a repository of their own. Git lists
    /// each as one entry and never the files inside it, so the caller says
    /// so rather than let the list look complete.
    pub nested_repositories: Vec<PathBuf>,
    /// The paths of [`TreeStatus::paths`] git lists as untracked (new).
    pub untracked: Vec<PathBuf>,
}

impl TreeStatus {
    /// `self` without the files a build or install generated as a side effect
    /// (`kgraf_sandbox::generated_kind`: a lockfile beside its manifest,
    /// `node_modules/`, `.venv/`, Maven's `target/`, bytecode, caches,
    /// compiled programs), and how many were left out. Only an untracked path
    /// outside `in_scope` is left out: the rule merge-back uses to leave the
    /// same files out of the commit. An install of thousands of files then
    /// costs the worker's change list and the progress measure nothing, as it
    /// costs the commit nothing.
    ///
    /// Blocking: reads the tree. Call it through `spawn_blocking`.
    pub fn without_generated(
        mut self,
        root: &Path,
        in_scope: impl Fn(&Path) -> bool,
    ) -> (TreeStatus, usize) {
        let untracked: std::collections::HashSet<&PathBuf> = self.untracked.iter().collect();
        let manifests = kgraf_sandbox::TreeManifests::new(root);
        let mut generated: std::collections::HashSet<PathBuf> = Default::default();
        for p in &self.paths {
            if untracked.contains(p)
                && !in_scope(p)
                && kgraf_sandbox::generated_kind(
                    p,
                    |m| manifests.present(m),
                    |f| kgraf_sandbox::file_head(root, f),
                )
                .is_some()
            {
                generated.insert(p.clone());
            }
        }
        let left_out = generated.len();
        self.paths.retain(|p| !generated.contains(p));
        self.untracked.retain(|p| !generated.contains(p));
        (self, left_out)
    }
}

/// Read a short file whole, up to [`MAX_LINK_BYTES`].
fn read_link_file(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_LINK_BYTES).read_to_string(&mut text))
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(text.trim().to_owned())
}

/// Read the `.git` entry at `root` as it is now. Only a gitlink file at the
/// root itself is accepted: no `.git` there (a root below the top of its
/// work tree), a `.git` directory, a symbolic link or any other kind of entry
/// is an error, since nothing the host recorded pins it.
pub fn git_entry(root: &Path) -> Result<GitEntry, String> {
    let path = root.join(".git");
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "{} has no .git entry of its own, so no recorded git directory \
                 applies to it",
                root.display()
            ));
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    if meta.is_dir() {
        return Err(format!(
            "{} is a git directory inside the worktree, whose configuration a \
             sandboxed command can rewrite",
            path.display()
        ));
    }
    if !meta.is_file() {
        return Err(format!(
            "{} is neither a gitlink file nor a directory",
            path.display()
        ));
    }
    Ok(GitEntry {
        root: root.to_path_buf(),
        gitlink: read_link_file(&path)?,
    })
}

/// Resolve a path a git link file names, relative to the directory holding
/// that file, and refuse it when it lies inside `root`.
fn outside_root(named: &str, base: &Path, root: &Path, what: &str) -> Result<PathBuf, String> {
    let named = Path::new(named);
    // git 2.48+ can write relative gitlinks (worktree.useRelativePaths).
    let path = if named.is_absolute() {
        named.to_path_buf()
    } else {
        base.join(named)
    };
    let resolved = path
        .canonicalize()
        .map_err(|e| format!("cannot resolve the {what} {}: {e}", path.display()))?;
    let root = root
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", root.display()))?;
    if resolved.starts_with(&root) {
        return Err(format!(
            "the {what} {} is inside the worktree, whose files a sandboxed \
             command can rewrite",
            resolved.display()
        ));
    }
    Ok(resolved)
}

/// The git directory `entry` names, after checking that it and the common
/// directory it names both lie outside the root.
fn checked_git_dir(entry: &GitEntry) -> Result<PathBuf, String> {
    let target = entry
        .gitlink
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("{}/.git names no git directory", entry.root.display()))?;
    let git_dir = outside_root(target, &entry.root, &entry.root, "git directory")?;
    // A linked worktree's admin directory names the shared repository in
    // `commondir`, whose config git reads as well.
    let commondir = git_dir.join("commondir");
    if std::fs::symlink_metadata(&commondir).is_ok() {
        let common = read_link_file(&commondir)?;
        outside_root(&common, &git_dir, &entry.root, "common git directory")?;
    }
    Ok(git_dir)
}

/// What git reports as changed, deleted or untracked under `root`.
///
/// `at_start` is [`git_entry`] as read when the worker started. The read is
/// refused if `<root>/.git` no longer matches it, and git runs with that
/// recorded git directory named explicitly (see the module docs for what
/// else is pinned), so a `.git` swapped in after the check cannot redirect
/// it either.
///
/// Blocking: runs `git`. Call it through `spawn_blocking`.
pub fn dirty_paths(root: &Path, at_start: &GitEntry) -> Result<TreeStatus, String> {
    if at_start.root != root {
        return Err(format!(
            "the recorded .git entry is for {}, not {}",
            at_start.root.display(),
            root.display()
        ));
    }
    let now = git_entry(root)?;
    if now != *at_start {
        return Err(format!(
            "the .git entry of {} changed since the worker started, so git was not run on it",
            root.display()
        ));
    }
    let admin = checked_git_dir(at_start)?;
    // Shared repository lock for this one process (decision 0094), found from
    // the recorded admin directory rather than the `.git` on disk. Taken
    // before the command is built, so the host runner's config listing and
    // the status read run in the same hold and a worker's Bypass git window
    // cannot change the config or the index between them.
    let _repo = kgraf_core::repo_lock::shared_for(&admin, "kGraf git status (worker tree read)")
        .map_err(|e| e.to_string())?;
    let mut git_dir = std::ffi::OsString::from("--git-dir=");
    git_dir.push(&admin);
    let mut work_tree = std::ffi::OsString::from("--work-tree=");
    work_tree.push(root);
    // The host runner (`kgraf_core::host_git`) removes the inherited `GIT_*`
    // environment (a stray index or object directory), turns hooks, every
    // configured driver and the file system monitor off, and allows no
    // transport (a missing object in a partial clone fails the read).
    let pre: Vec<std::ffi::OsString> =
        vec!["-C".into(), root.as_os_str().to_owned(), git_dir, work_tree];
    let mut cmd = kgraf_core::host_git::command(
        &kgraf_core::host_git::Invocation::in_dir(&pre),
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            // A submodule's git directory is inside the tree: never entered.
            "--ignore-submodules=all",
            "--",
            ".",
        ],
    )
    .map_err(|e| format!("cannot run git: {e}"))?;
    let output = cmd
        // A status read must not take the index lock a concurrent git holds.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut status = parse_porcelain_z(&output.stdout);
    // The links kGraf makes to the checkout's installed dependencies
    // (`kgraf_sandbox::link_dependency_trees`) are not the worker's changes.
    // A `node_modules` kGraf made as a folder of links (a workspace install,
    // `kgraf_sandbox::MIRROR_MARK`) is not theirs either: its mark and links;
    // nor anything in a pnpm install kGraf cloned into the tree
    // (`kgraf_sandbox::is_dependency_clone`).
    let in_mirror = |p: &Path| {
        let marked = |dir: &Path| {
            !dir.as_os_str().is_empty()
                && kgraf_sandbox::is_dependency_link(dir)
                && root.join(dir).join(kgraf_sandbox::MIRROR_MARK).is_file()
        };
        let in_clone = p
            .ancestors()
            .skip(1)
            .any(|dir| marked(dir) && kgraf_sandbox::is_dependency_clone(&root.join(dir)));
        in_clone
            || p.ancestors().skip(1).any(marked)
                && std::fs::symlink_metadata(root.join(p)).is_ok_and(|m| {
                    m.file_type().is_symlink() || p.ends_with(kgraf_sandbox::MIRROR_MARK)
                })
    };
    let is_link = |p: &Path| {
        kgraf_sandbox::is_dependency_link(p)
            && std::fs::symlink_metadata(root.join(p)).is_ok_and(|m| m.file_type().is_symlink())
    };
    status.paths.retain(|p| !(is_link(p) || in_mirror(p)));
    Ok(status)
}

/// Parse `git status --porcelain=v1 -z`: entries are `XY path` separated by NUL,
/// and a rename or copy is followed by one more entry, the old path. With
/// `--untracked-files=all` an untracked entry ending in `/` is a directory
/// holding its own repository.
fn parse_porcelain_z(bytes: &[u8]) -> TreeStatus {
    let mut status = TreeStatus::default();
    let mut entries = bytes.split(|b| *b == 0).filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let path = &entry[3..];
        if entry.starts_with(b"??") && path.ends_with(b"/") {
            status
                .nested_repositories
                .push(path_from_git(&path[..path.len() - 1]));
        }
        if entry.starts_with(b"??") {
            status.untracked.push(path_from_git(path));
        }
        status.paths.push(path_from_git(path));
        if matches!(entry[0], b'R' | b'C')
            && let Some(old) = entries.next()
        {
            status.paths.push(path_from_git(old));
        }
    }
    status
}

/// A path as git printed it. On Unix the bytes are the file name exactly, so
/// a name that is not valid UTF-8 is kept as it is.
#[cfg(unix)]
fn path_from_git(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

/// A path as git printed it: git writes UTF-8 on Windows.
#[cfg(not(unix))]
fn path_from_git(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn porcelain_entries_and_renames_parse() {
        let raw = b" M src/a.rs\0?? new file.txt\0R  b.rs\0old.rs\0 D gone.rs\0";
        let status = parse_porcelain_z(raw);
        assert_eq!(
            status.paths,
            vec![
                PathBuf::from("src/a.rs"),
                PathBuf::from("new file.txt"),
                PathBuf::from("b.rs"),
                PathBuf::from("old.rs"),
                PathBuf::from("gone.rs"),
            ]
        );
        assert!(status.nested_repositories.is_empty());
        assert_eq!(parse_porcelain_z(b""), TreeStatus::default());
    }

    /// An untracked directory holding its own repository is one entry; it is
    /// reported apart so the caller can say its files are not listed.
    #[test]
    fn a_nested_repository_is_reported_apart() {
        let dir = tempfile::tempdir().expect("dir");
        let repo = dir.path().join("repo");
        let wt = dir.path().join("wt");
        linked_worktree(&repo, &wt, |_| {});
        let at_start = git_entry(&wt).expect("entry");
        std::fs::create_dir(wt.join("vendored")).expect("nested");
        git(&wt.join("vendored"), &["init", "-q"]);
        std::fs::write(wt.join("vendored/inner.txt"), "i").expect("inner");
        let status = dirty_paths(&wt, &at_start).expect("status");
        assert_eq!(status.nested_repositories, vec![PathBuf::from("vendored")]);
        assert_eq!(status.paths, vec![PathBuf::from("vendored")]);
    }

    /// A link kGraf made to the checkout's `node_modules` is not one of the
    /// worker's changes; a folder or file the worker wrote is.
    #[cfg(unix)]
    #[test]
    fn a_dependency_link_is_not_a_change_of_the_worker() {
        let dir = tempfile::tempdir().expect("dir");
        let repo = dir.path().join("repo");
        let wt = dir.path().join("wt");
        linked_worktree(&repo, &wt, |r| {
            std::fs::write(r.join("package.json"), "{}").expect("package.json");
            git(r, &["add", "package.json"]);
        });
        let at_start = git_entry(&wt).expect("entry");
        let installed = dir.path().join("installed");
        std::fs::create_dir_all(&installed).expect("installed");
        std::os::unix::fs::symlink(&installed, wt.join("node_modules")).expect("link");
        std::fs::write(wt.join("b.txt"), "b").expect("write");
        let status = dirty_paths(&wt, &at_start).expect("status");
        assert_eq!(status.paths, vec![PathBuf::from("b.txt")]);
    }

    /// The folder of links kGraf makes for a workspace install, in a tree
    /// whose `.gitignore` does not name `node_modules`: its mark and links
    /// are not the worker's changes; a file the worker wrote there is.
    #[cfg(unix)]
    #[test]
    fn a_dependency_mirror_is_not_a_change_of_the_worker() {
        let dir = tempfile::tempdir().expect("dir");
        let repo = dir.path().join("repo");
        let wt = dir.path().join("wt");
        linked_worktree(&repo, &wt, |r| {
            std::fs::write(r.join("package.json"), "{}").expect("package.json");
            git(r, &["add", "package.json"]);
        });
        let at_start = git_entry(&wt).expect("entry");
        let installed = dir.path().join("installed");
        std::fs::create_dir_all(&installed).expect("installed");
        let nm = wt.join("node_modules");
        std::fs::create_dir_all(nm.join("@s")).expect("mkdir");
        std::fs::write(nm.join(kgraf_sandbox::MIRROR_MARK), "1").expect("mark");
        std::os::unix::fs::symlink(&installed, nm.join("dep")).expect("link");
        std::os::unix::fs::symlink("../../packages/web", nm.join("@s/web")).expect("link");
        std::fs::write(nm.join("own.js"), "x").expect("write");
        let status = dirty_paths(&wt, &at_start).expect("status");
        assert_eq!(status.paths, vec![PathBuf::from("node_modules/own.js")]);
    }

    /// A pnpm install kGraf cloned into a tree whose `.gitignore` does not
    /// name `node_modules`: none of its files are the worker's changes.
    #[cfg(unix)]
    #[test]
    fn a_dependency_clone_is_not_a_change_of_the_worker() {
        let dir = tempfile::tempdir().expect("dir");
        let repo = dir.path().join("repo");
        let wt = dir.path().join("wt");
        linked_worktree(&repo, &wt, |r| {
            std::fs::write(r.join("package.json"), "{}").expect("package.json");
            git(r, &["add", "package.json"]);
        });
        let at_start = git_entry(&wt).expect("entry");
        let nm = wt.join("node_modules");
        std::fs::create_dir_all(nm.join(".pnpm/dep@1.0.0/node_modules/dep")).expect("mkdir");
        std::fs::write(nm.join(".pnpm/dep@1.0.0/node_modules/dep/index.js"), "x").expect("write");
        std::fs::write(nm.join(kgraf_sandbox::MIRROR_MARK), "clone 1").expect("mark");
        std::fs::write(wt.join("a.txt"), "work").expect("write");
        let status = dirty_paths(&wt, &at_start).expect("status");
        assert_eq!(status.paths, vec![PathBuf::from("a.txt")]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_file_name_is_listed_exactly() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().expect("dir");
        let repo = dir.path().join("repo");
        let wt = dir.path().join("wt");
        linked_worktree(&repo, &wt, |_| {});
        let at_start = git_entry(&wt).expect("entry");
        let odd = std::ffi::OsStr::from_bytes(b"a\xff.txt");
        std::fs::write(wt.join(odd), "a").expect("write");
        let status = dirty_paths(&wt, &at_start).expect("status");
        assert_eq!(status.paths, vec![PathBuf::from(odd)]);
    }

    /// An install in a worktree with no `.gitignore` (thousands of files
    /// under `node_modules/`, a lockfile, a `.venv/`) is left out of the
    /// worker's change list and the progress measure, as the commit leaves it
    /// out; the work itself, a declared lockfile and a modified tracked file
    /// stay. Before, every installed file was listed and hashed.
    #[test]
    fn an_install_is_left_out_of_the_change_list_and_the_work_stays() {
        let dir = tempfile::tempdir().expect("dir");
        let wt = dir.path().join("wt");
        linked_worktree(&wt.with_extension("repo"), &wt, |root| {
            std::fs::write(root.join("package.json"), "{}").expect("manifest");
            std::fs::create_dir_all(root.join("py")).expect("py");
            std::fs::write(root.join("py/pyproject.toml"), "").expect("py manifest");
            std::fs::write(root.join("tracked.lock"), "a").expect("tracked");
            git(root, &["add", "-A"]);
        });
        let at_start = git_entry(&wt).expect("entry");
        for i in 0..300 {
            let p = wt.join(format!("node_modules/pkg{i}/index.js"));
            std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
            std::fs::write(p, "x").expect("installed");
        }
        std::fs::write(wt.join("package-lock.json"), "{}").expect("lock");
        std::fs::create_dir_all(wt.join("py/.venv/lib")).expect("venv");
        std::fs::write(wt.join("py/.venv/lib/site.py"), "x").expect("venv file");
        std::fs::write(wt.join("src.js"), "work").expect("work");
        std::fs::write(wt.join("tracked.lock"), "b").expect("tracked change");
        let status = dirty_paths(&wt, &at_start).expect("status");
        assert!(status.paths.len() > 300, "{}", status.paths.len());
        let (kept, left_out) = status.clone().without_generated(&wt, |_| false);
        assert_eq!(left_out, 302, "{:?}", kept.paths);
        let mut paths = kept.paths.clone();
        paths.sort();
        assert_eq!(
            paths,
            [PathBuf::from("src.js"), PathBuf::from("tracked.lock")]
        );
        // A lockfile the contract declares is its work, and stays.
        let (kept, _) = status.without_generated(&wt, |p| p == Path::new("package-lock.json"));
        assert!(kept.paths.contains(&PathBuf::from("package-lock.json")));
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_an_error() {
        let dir = tempfile::tempdir().expect("dir");
        let missing = dir.path().join("does-not-exist");
        assert!(git_entry(&missing).is_err());
        let entry = GitEntry {
            root: missing.clone(),
            gitlink: "gitdir: nowhere".into(),
        };
        assert!(dirty_paths(&missing, &entry).is_err());
    }

    /// A gitlink naming a git directory inside the root (one a command could
    /// have written) is refused before git runs.
    #[test]
    fn a_gitlink_into_the_root_is_not_read() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("inner")).expect("root");
        git(&root.join("inner"), &["init", "-q"]);
        std::fs::write(root.join(".git"), "gitdir: inner/.git\n").expect("gitlink");
        let entry = git_entry(&root).expect("a gitlink file");
        let err = dirty_paths(&root, &entry).expect_err("inside the root");
        assert!(err.contains("inside the worktree"), "{err}");
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@localhost.invalid"])
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The status read of a linked worktree lists its changes, runs no
    /// fsmonitor hook the repository names (a plain `git status` does, the
    /// control), and is refused once the gitlink changed or became a link.
    #[cfg(unix)]
    #[test]
    fn the_status_read_uses_the_recorded_git_directory_and_runs_no_hook() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("dir");
        let repo = dir.path().join("repo");
        let wt = dir.path().join("wt");
        linked_worktree(&repo, &wt, |_| {});
        let at_start = git_entry(&wt).expect("gitlink");

        let marker = dir.path().join("hook-ran");
        let hook = dir.path().join("hook.sh");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).expect("hook");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        git(
            &repo,
            &["config", "core.fsmonitor", hook.to_str().expect("utf-8")],
        );
        std::fs::write(wt.join("new.txt"), "n").expect("change");

        let status = dirty_paths(&wt, &at_start).expect("status");
        assert_eq!(status.paths, vec![PathBuf::from("new.txt")]);
        assert!(!marker.exists(), "the fsmonitor hook must not run");
        // Control: git on its own runs the hook, so the check above is not
        // vacuous.
        git(&wt, &["status", "--porcelain"]);
        assert!(marker.exists(), "control: plain git status runs the hook");

        // A gitlink aimed elsewhere is refused before git runs.
        let original = std::fs::read_to_string(wt.join(".git")).expect("gitlink");
        std::fs::write(wt.join(".git"), "gitdir: planted\n").expect("rewrite");
        let err = dirty_paths(&wt, &at_start).expect_err("changed gitlink");
        assert!(err.contains("changed since the worker started"), "{err}");
        // A `.git` that is a symbolic link is refused as well.
        std::fs::remove_file(wt.join(".git")).expect("remove");
        std::fs::write(dir.path().join("gitlink"), &original).expect("copy");
        std::os::unix::fs::symlink(dir.path().join("gitlink"), wt.join(".git")).expect("link");
        assert!(dirty_paths(&wt, &at_start).is_err());
    }

    /// A filter script that touches `marker` when git runs it, and passes
    /// its input through.
    #[cfg(unix)]
    fn filter_script(dir: &Path, marker: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("filter.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
        )
        .expect("filter");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        script
    }

    /// A repository at `repo` with one commit, and a linked worktree of it at `wt`.
    fn linked_worktree(repo: &Path, wt: &Path, seed: impl FnOnce(&Path)) {
        std::fs::create_dir(repo).expect("repo");
        git(repo, &["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "a").expect("seed");
        git(repo, &["add", "a.txt"]);
        seed(repo);
        git(repo, &["commit", "-qm", "init"]);
        git(
            repo,
            &["worktree", "add", "-q", wt.to_str().expect("utf-8")],
        );
    }

    /// Plant a repository in `dir` whose own config names a clean filter for
    /// `f.txt`, with `f.txt` added and then rewritten at the same size, so a
    /// status of it compares content and runs the filter.
    #[cfg(unix)]
    fn plant_filtered_repository(dir: &Path, script: &Path, marker: &Path) {
        git(dir, &["init", "-q"]);
        git(
            dir,
            &[
                "config",
                "filter.planted.clean",
                script.to_str().expect("utf-8"),
            ],
        );
        std::fs::write(dir.join(".gitattributes"), "f.txt filter=planted\n").expect("attrs");
        std::fs::write(dir.join("f.txt"), "aaaa").expect("file");
        git(dir, &["add", "f.txt", ".gitattributes"]);
        std::fs::write(dir.join("f.txt"), "bbbb").expect("rewrite");
        let _ = std::fs::remove_file(marker);
    }

    /// A worktree whose commit holds a submodule entry: a command that plants
    /// a repository with its own filter there must not have that filter run
    /// by the host's status read. Plain `git status` does run it (the
    /// control), so the check is not vacuous.
    #[cfg(unix)]
    #[test]
    fn a_planted_submodule_repository_runs_no_filter_on_the_host() {
        let dir = tempfile::tempdir().expect("dir");
        let repo = dir.path().join("repo");
        let wt = dir.path().join("wt");
        linked_worktree(&repo, &wt, |repo| {
            // Any object id serves: status compares the submodule's HEAD with
            // it and then asks the submodule for its own status.
            let id = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
            git(
                repo,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{id},sub"),
                ],
            );
        });
        let at_start = git_entry(&wt).expect("entry");
        let marker = dir.path().join("filter-ran");
        let script = filter_script(dir.path(), &marker);
        std::fs::create_dir_all(wt.join("sub")).expect("sub");
        plant_filtered_repository(&wt.join("sub"), &script, &marker);

        let status = dirty_paths(&wt, &at_start).expect("status");
        assert!(!marker.exists(), "the planted filter must not run");
        // The submodule itself is not entered, so nothing inside it is listed.
        assert!(status.paths.is_empty(), "{status:?}");
        // Control: git on its own enters the submodule and runs the filter.
        git(&wt, &["status", "--porcelain"]);
        assert!(marker.exists(), "control: plain git status runs the filter");
    }

    /// A `.git` directory inside the root is sandbox-writable on a backend
    /// without gitlink protection: its config can name a filter. The status
    /// read refuses it rather than run git on it.
    #[cfg(unix)]
    #[test]
    fn a_git_directory_inside_the_root_is_not_read() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("root");
        std::fs::create_dir(&root).expect("root");
        let marker = dir.path().join("filter-ran");
        let script = filter_script(dir.path(), &marker);
        plant_filtered_repository(&root, &script, &marker);

        let result = git_entry(&root).and_then(|entry| dirty_paths(&root, &entry));
        assert!(result.is_err(), "{result:?}");
        assert!(!marker.exists(), "the planted filter must not run");
        git(&root, &["status", "--porcelain"]);
        assert!(marker.exists(), "control: plain git status runs the filter");
    }

    /// A root below the top of its work tree has no `.git` of its own that
    /// the host recorded: one found there later may be planted. Refused.
    #[test]
    fn a_root_below_the_top_of_its_work_tree_is_not_read() {
        let dir = tempfile::tempdir().expect("dir");
        let repo = dir.path().join("repo");
        let wt = dir.path().join("wt");
        linked_worktree(&repo, &wt, |_| {});
        std::fs::create_dir(wt.join("sub")).expect("sub");
        let err = git_entry(&wt.join("sub")).expect_err("below the top");
        assert!(err.contains("no .git entry of its own"), "{err}");
    }
}
