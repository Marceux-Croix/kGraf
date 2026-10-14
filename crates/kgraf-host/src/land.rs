//! Landing a run onto the user's branch over the real repository (decision
//! 0100, section 5): the git side of the one land function
//! ([`kgraf_orchestrator::land_flow::land`]) and its no-loop context.
//!
//! - The merge commit is built by merge-back's own merge computation
//!   (`MergeBack::landing_merge`: `merge-tree --write-tree -z`, target first
//!   parent), so git 2.42 or later is required and an older git is refused
//!   before anything is written (`MergeBack::open`).
//! - It is signed when the user's `commit.gpgSign` is on
//!   (`kgraf_sandbox::signing`); a signing failure refuses the land.
//! - **A target branch that is checked out** (in the main checkout or a
//!   linked worktree) moves with the user's own git: `git merge --ff-only
//!   <merge commit>` in that worktree. This is the only step of a land that
//!   runs the user's hooks (`post-merge`) and filters (a smudge filter
//!   checks the landed files out filtered). Git refuses when a local change
//!   would be overwritten (modified, staged, or an untracked file in the
//!   way), and then nothing changes; local edits to other files stay as
//!   they are.
//! - **A target nobody has checked out** moves by `update-ref` with its old
//!   value as the expected one; a concurrent move refuses.
//! - Then the merge commit is folded back into `kgraf/<run>` (idempotent,
//!   never backward).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use kgraf_core::RunId;
use kgraf_orchestrator::land_flow::{
    Built, FoldResult, LandContext, LandError, LandGit, LandOutcome, LandRequest, TargetInfo,
    TargetLocation,
};
use kgraf_orchestrator::land_record::{
    Ancestry, LandBook, LandingEntry, ProgramSnapshot, newest_passed, stub_blockers,
    stub_lands_in_trace,
};
use kgraf_orchestrator::land_suggest::SuggestFacts;
use kgraf_orchestrator::{
    GateRequest, GateRunner, GateVerdict, ROOT_GATE, RunFiles, RunStateRecord, RunStore,
    SharedGraph, VerifiedCommit,
};
use kgraf_sandbox::git::Git;
use kgraf_sandbox::signing::{CommitSigning, user_commit_signing};
use kgraf_sandbox::user_git::UserGit;
use kgraf_sandbox::{Fold, LandingMerge, Limits, MergeBack, MergeError, WorkspaceManager};

fn merge_error(e: MergeError) -> LandError {
    match e {
        MergeError::GitTooOld { .. } => LandError::GitTooOld(e.to_string()),
        MergeError::SigningFailed { stderr } => LandError::Signing(stderr),
        other => LandError::Git(other.to_string()),
    }
}

fn git_error(e: impl std::fmt::Display) -> LandError {
    LandError::Git(e.to_string())
}

/// The run's integration branch name, as the run created it: `kgraf/<run>`
/// with every character but letters, digits, `-` and `_` replaced by `-`,
/// or the branch under the former prefix when only that one exists.
pub fn run_branch_name(git: &Git, repo: &Path, run: &str) -> String {
    let safe: String = run
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let branch = format!("kgraf/{safe}");
    let legacy = format!("haustoria/{safe}");
    let exists = |name: &str| {
        let full = format!("refs/heads/{name}");
        git.run(repo, ["rev-parse", "--verify", "--quiet", full.as_str()])
            .is_ok()
    };
    if !exists(&branch) && exists(&legacy) {
        legacy
    } else {
        branch
    }
}

/// [`LandGit`] over a real repository and one run branch.
pub struct GitLander {
    repo: PathBuf,
    merge_back: Arc<MergeBack>,
    manager: Arc<WorkspaceManager>,
    git: Git,
    user_git: UserGit,
    /// The program asked whether a process holds a lock file.
    lsof: PathBuf,
}

impl GitLander {
    /// Open the land of `run_branch` in the repository at `repo`, with the
    /// worktree base `worktrees` (where a gate's worktree goes). Refuses a
    /// git older than merge-back needs, an identity merge-back refuses, and
    /// a missing run branch, before anything is written.
    pub fn open(repo: &Path, run_branch: &str, worktrees: &Path) -> Result<Self, LandError> {
        Self::open_with_git(
            repo,
            run_branch,
            worktrees,
            Git::discover().map_err(git_error)?,
        )
    }

    /// [`Self::open`] with a given git executable.
    pub fn open_with_git(
        repo: &Path,
        run_branch: &str,
        worktrees: &Path,
        git: Git,
    ) -> Result<Self, LandError> {
        let manager = Arc::new(
            WorkspaceManager::open_with_git(repo, worktrees, git.clone()).map_err(git_error)?,
        );
        let identity = crate::worker::workspace::owner_identity(repo).map_err(git_error)?;
        let merge_back = MergeBack::open_with(
            manager.clone(),
            run_branch,
            identity,
            Limits::default(),
            git.clone(),
        )
        .map_err(merge_error)?;
        Ok(Self::from_parts(manager, Arc::new(merge_back), git))
    }

    /// Over a run's own merge-back (a live loop's): its in-process merge
    /// lock then serializes the land's fold with the loop's merges.
    pub fn from_parts(
        manager: Arc<WorkspaceManager>,
        merge_back: Arc<MergeBack>,
        git: Git,
    ) -> Self {
        Self {
            repo: manager.repo().to_path_buf(),
            merge_back,
            manager,
            user_git: UserGit::from_git(&git),
            git,
            lsof: PathBuf::from("lsof"),
        }
    }

    /// Ask another program whether a lock file is held (tests).
    pub fn with_lsof(mut self, lsof: PathBuf) -> Self {
        self.lsof = lsof;
        self
    }

    pub fn merge_back(&self) -> &Arc<MergeBack> {
        &self.merge_back
    }

    pub fn manager(&self) -> &Arc<WorkspaceManager> {
        &self.manager
    }

    /// The user's own git with another bound on each command (tests).
    pub fn with_user_git_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.user_git = self.user_git.with_timeout(timeout);
        self
    }

    fn branch_tip(&self, name: &str) -> Result<Option<String>, LandError> {
        self.merge_back
            .resolve_commit(&format!("refs/heads/{name}"))
            .map_err(merge_error)
    }

    fn ignorecase(&self) -> bool {
        self.user_git
            .exec(
                &self.repo,
                ["config", "--type=bool", "--get", "core.ignorecase"],
            )
            .map(|o| o.success() && o.stdout_str() == "true")
            .unwrap_or(false)
    }

    /// Where `full` (`refs/heads/<name>`) is checked out. On a file system
    /// that ignores case, a worktree with a branch differing only by case
    /// refuses: git there cannot tell the two apart.
    fn location(&self, full: &str, ignorecase: bool) -> Result<TargetLocation, LandError> {
        let entries = self.git.worktree_list(&self.repo).map_err(git_error)?;
        if ignorecase
            && let Some(e) = entries.iter().find(|e| {
                e.branch
                    .as_deref()
                    .is_some_and(|b| b != full && b.eq_ignore_ascii_case(full))
            })
        {
            return Err(LandError::NoSuchBranch(format!(
                "{} has {} checked out, which differs from {full} only by case; this file \
                 system ignores case, so kGraf refuses to guess",
                e.path.display(),
                e.branch.as_deref().unwrap_or_default()
            )));
        }
        Ok(entries
            .iter()
            .enumerate()
            .find(|(_, e)| e.branch.as_deref() == Some(full))
            .map(|(i, e)| {
                // `git worktree list` names the main working tree first.
                if i == 0 && !e.bare {
                    TargetLocation::MainCheckout(e.path.clone())
                } else {
                    TargetLocation::LinkedWorktree(e.path.clone())
                }
            })
            .unwrap_or(TargetLocation::NotCheckedOut))
    }

    /// Put back a branch that `git merge --ff-only` moved because another
    /// branch was checked out by the time it ran.
    fn restore_wrong_branch(&self, worktree: &Path, entry: &LandingEntry) -> Option<String> {
        let head = self
            .user_git
            .exec(worktree, ["symbolic-ref", "--quiet", "HEAD"])
            .ok()?;
        let branch = head.stdout_str();
        let name = branch.strip_prefix("refs/heads/")?;
        if name == entry.target || self.branch_tip(name).ok()?? != entry.merge_commit {
            return None;
        }
        let wt = worktree.display();
        let moved = format!(
            "{name} was checked out in {wt} instead of {} by the time git ran, so git moved \
             {name} to the merge commit {}",
            entry.target, entry.merge_commit
        );
        let before = self
            .git
            .run_str(
                &self.repo,
                [
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{branch}@{{1}}"),
                ],
            )
            .ok()
            .filter(|b| !b.is_empty());
        let Some(before) = before else {
            return Some(format!(
                "{moved}; it could not be put back: {name} has no reflog entry from before. \
                 Point it back yourself, and check the files in {wt}"
            ));
        };
        Some(
            match self.merge_back.move_branch(
                name,
                &before,
                &entry.merge_commit,
                "kgraf land: put back",
            ) {
                Ok(true) => format!(
                    "{moved}; the branch was put back to {before}, but the files in {wt} are the \
                     merge commit's. To put them back too: git -C {wt} reset --keep {before}"
                ),
                _ => format!(
                    "{moved}; it could not be put back (it moved again). It was at {before} \
                     before: check {name} and the files in {wt}"
                ),
            },
        )
    }

    /// The paths checking out `merge` over `old` changes (NUL-separated, no
    /// renames, so every name is as git stores it). Fails closed.
    fn merge_paths(&self, old: &str, merge: &str) -> Result<Vec<String>, LandError> {
        self.diff_paths(old, merge, &[])
    }

    fn diff_paths(
        &self,
        old: &str,
        merge: &str,
        only: &[String],
    ) -> Result<Vec<String>, LandError> {
        let mut args: Vec<String> = ["diff", "-z", "--no-renames", "--name-only", old, merge]
            .map(str::to_owned)
            .to_vec();
        if !only.is_empty() {
            args.push("--".into());
            args.extend(only.iter().map(|p| format!(":(literal){p}")));
        }
        let out = self.git.run(&self.repo, &args).map_err(git_error)?;
        Ok(out
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect())
    }

    /// `path` in `commit`: its mode and blob, or `None` when absent. Fails
    /// closed.
    fn tree_entry(&self, commit: &str, path: &str) -> Result<Option<(String, String)>, LandError> {
        let out = self
            .git
            .run(
                &self.repo,
                ["ls-tree", "-z", commit, "--", &format!(":(literal){path}")],
            )
            .map_err(git_error)?;
        let line = out.split(|b| *b == 0).find(|l| !l.is_empty());
        Ok(line.and_then(|l| {
            let text = String::from_utf8_lossy(l);
            let (meta, _) = text.split_once('\t')?;
            let mut parts = meta.split(' ');
            let mode = parts.next()?.to_owned();
            let _kind = parts.next()?;
            Some((mode, parts.next()?.to_owned()))
        }))
    }
}

/// Whether `key` (as `git config --list` prints it) names a program the
/// user's own git would run, or selects one: the settings a land enforces.
pub fn program_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    let Some((section, rest)) = lower.split_once('.') else {
        return false;
    };
    let var = rest.rsplit('.').next().unwrap_or(rest);
    let has_sub = rest.contains('.');
    match section {
        "core" => matches!(
            rest,
            "fsmonitor" | "hookspath" | "askpass" | "sshcommand" | "attributesfile"
        ),
        "filter" => has_sub && matches!(var, "clean" | "smudge" | "process" | "required"),
        "diff" => (has_sub && matches!(var, "textconv" | "command")) || rest == "external",
        "merge" => has_sub && var == "driver",
        "gpg" => {
            matches!(rest, "program" | "format" | "ssh.defaultkeycommand")
                || (has_sub && var == "program")
        }
        _ => false,
    }
}

/// The program-valued settings in effect in `dir` (every scope, includes
/// resolved), key to value; the last value wins, as git reads it. Fails
/// closed: an error is an error, never an empty map.
fn program_config(
    user_git: &UserGit,
    dir: &Path,
) -> Result<std::collections::BTreeMap<String, String>, LandError> {
    let out = user_git
        .exec(
            dir,
            ["config", "--list", "--show-scope", "--show-origin", "-z"],
        )
        .map_err(git_error)?;
    if !out.success() {
        return Err(LandError::Git(format!(
            "cannot read the git configuration in {}: {}",
            dir.display(),
            out.stderr
        )));
    }
    let mut map = std::collections::BTreeMap::new();
    let mut fields = out.stdout.split(|b| *b == 0);
    while let (Some(_scope), Some(_origin), Some(item)) =
        (fields.next(), fields.next(), fields.next())
    {
        if item.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(item);
        let (k, v) = text.split_once('\n').unwrap_or((&text, "true"));
        if program_key(k) {
            map.insert(k.to_owned(), v.to_owned());
        }
    }
    Ok(map)
}

/// The names of the program-valued settings set in `dir` now.
fn program_keys_now(user_git: &UserGit, dir: &Path) -> Result<BTreeSet<String>, LandError> {
    let out = user_git
        .exec(dir, ["config", "--list", "--name-only", "-z"])
        .map_err(git_error)?;
    if !out.success() {
        return Err(LandError::Git(format!(
            "cannot list the git configuration in {}: {}",
            dir.display(),
            out.stderr
        )));
    }
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter(|k| !k.is_empty())
        .map(|k| String::from_utf8_lossy(k).into_owned())
        .filter(|k| program_key(k))
        .collect())
}

/// A hash of a hooks directory's files: names, modes, contents. Symbolic
/// links are followed; a link that points nowhere counts by its target text.
fn hash_hooks(dir: &Path) -> Result<String, LandError> {
    let mut hasher = blake3::Hasher::new();
    let mut names = Vec::new();
    match std::fs::read_dir(dir) {
        Ok(read) => {
            for e in read {
                let e = e.map_err(git_error)?;
                names.push(e.file_name());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(git_error(e)),
    }
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        hasher.update(name.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            hasher.update(&meta.permissions().mode().to_le_bytes());
        }
        hasher.update(&std::fs::read(&path).map_err(git_error)?);
        hasher.update(b"\0");
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Copy the hooks in `from` (regular files, links followed, modes kept)
/// into `to`, which must not exist yet.
fn copy_hooks(from: &Path, to: &Path) -> Result<(), LandError> {
    std::fs::create_dir_all(to).map_err(git_error)?;
    let read = match std::fs::read_dir(from) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(git_error(e)),
    };
    for e in read {
        let e = e.map_err(git_error)?;
        let src = e.path();
        let Ok(meta) = std::fs::metadata(&src) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let dst = to.join(e.file_name());
        std::fs::write(&dst, std::fs::read(&src).map_err(git_error)?).map_err(git_error)?;
        std::fs::set_permissions(&dst, meta.permissions()).map_err(git_error)?;
    }
    Ok(())
}

/// What the user's own git runs, read in `dir`: the hooks directory as git
/// resolves it there, and every program-valued setting. With `copy_to`, the
/// hooks are copied there (once: an existing copy is kept, so a later
/// worker edit never replaces it) and the hash is the copy's.
pub fn program_snapshot(
    user_git: &UserGit,
    dir: &Path,
    copy_to: Option<&Path>,
) -> Result<ProgramSnapshot, LandError> {
    let hooks = user_git
        .exec(
            dir,
            ["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
        )
        .map_err(git_error)?;
    if !hooks.success() {
        return Err(LandError::Git(format!(
            "cannot find the hooks directory: {}",
            hooks.stderr
        )));
    }
    let hooks_dir = hooks.stdout_str();
    let config = program_config(user_git, dir)?;
    let (hooks_hash, hooks_copy) = match copy_to {
        Some(to) => {
            if !to.exists() {
                copy_hooks(Path::new(&hooks_dir), to)?;
            }
            (hash_hooks(to)?, Some(to.display().to_string()))
        }
        None => (hash_hooks(Path::new(&hooks_dir))?, None),
    };
    Ok(ProgramSnapshot {
        hooks_dir,
        hooks_hash,
        config,
        hooks_copy,
    })
}

/// The `-c` settings every user-git command of a land runs with: hooks from
/// the run-start copy (or none), each program-valued setting at its
/// run-start value, and each one set since passed empty, which disables it.
/// The returned directory, when any, must live while the command runs.
fn land_overrides(
    user_git: &UserGit,
    dir: &Path,
    programs: Option<&ProgramSnapshot>,
) -> Result<(Vec<String>, Option<tempfile::TempDir>), LandError> {
    let mut args = Vec::new();
    let mut empty = None;
    let hooks = match programs.and_then(|p| p.hooks_copy.clone().map(|c| (p, c))) {
        Some((p, copy)) => {
            if hash_hooks(Path::new(&copy))? != p.hooks_hash {
                return Err(LandError::Git(format!(
                    "the run-start copy of the hooks in {copy} changed; nothing was moved"
                )));
            }
            copy
        }
        None => {
            let dir = tempfile::tempdir().map_err(git_error)?;
            let path = dir.path().display().to_string();
            empty = Some(dir);
            path
        }
    };
    args.push("-c".to_owned());
    args.push(format!("core.hooksPath={hooks}"));
    if let Some(p) = programs {
        for (k, v) in &p.config {
            if k.eq_ignore_ascii_case("core.hookspath") {
                continue;
            }
            args.push("-c".to_owned());
            args.push(format!("{k}={v}"));
        }
        for k in program_keys_now(user_git, dir)? {
            if !p.config.contains_key(&k) && !k.eq_ignore_ascii_case("core.hookspath") {
                args.push("-c".to_owned());
                args.push(format!("{k}="));
            }
        }
        if !p
            .config
            .keys()
            .any(|k| k.eq_ignore_ascii_case("core.fsmonitor"))
        {
            args.push("-c".to_owned());
            args.push("core.fsmonitor=false".to_owned());
        }
    }
    Ok((args, empty))
}

impl LandGit for GitLander {
    fn run_branch(&self) -> &str {
        self.merge_back.integration_branch()
    }

    fn run_tip(&self) -> Result<String, LandError> {
        self.merge_back.integration_tip().map_err(merge_error)
    }

    fn checked_out_branch(&self) -> Result<Option<String>, LandError> {
        Ok(self
            .git
            .run_str(&self.repo, ["symbolic-ref", "--quiet", "HEAD"])
            .ok()
            .and_then(|h| h.strip_prefix("refs/heads/").map(str::to_owned)))
    }

    fn resolve_target(&self, name: &str) -> Result<TargetInfo, LandError> {
        self.git
            .run(&self.repo, ["check-ref-format", "--branch", name])
            .map_err(|_| LandError::NoSuchBranch(format!("{name:?} is not a valid branch name")))?;
        // The exact ref name: on a file system that ignores case,
        // `refs/heads/Main` would resolve to the file of `refs/heads/main`.
        let full = format!("refs/heads/{name}");
        let listed = self
            .git
            .run_str(
                &self.repo,
                ["for-each-ref", "--format=%(refname)", "refs/heads/"],
            )
            .map_err(git_error)?;
        let exact = listed.lines().any(|r| r == full);
        let folded: Vec<&str> = listed
            .lines()
            .filter(|r| *r != full && r.eq_ignore_ascii_case(&full))
            .collect();
        let ignorecase = self.ignorecase();
        if !exact {
            if let Some(other) = folded.first() {
                return Err(LandError::NoSuchBranch(format!(
                    "there is no branch named {name}; {} differs from it only by case",
                    other.trim_start_matches("refs/heads/")
                )));
            }
            let head = self
                .git
                .run_str(&self.repo, ["symbolic-ref", "--quiet", "HEAD"])
                .unwrap_or_default();
            if head == full {
                return Err(LandError::UnbornTarget {
                    branch: name.to_owned(),
                });
            }
            return Err(LandError::NoSuchBranch(format!(
                "there is no branch named {name}"
            )));
        }
        if ignorecase && let Some(other) = folded.first() {
            return Err(LandError::NoSuchBranch(format!(
                "branches {name} and {} differ only by case, and this file system ignores \
                 case; rename one first",
                other.trim_start_matches("refs/heads/")
            )));
        }
        let tip = self
            .branch_tip(name)?
            .ok_or_else(|| LandError::UnbornTarget {
                branch: name.to_owned(),
            })?;
        Ok(TargetInfo {
            location: self.location(&full, ignorecase)?,
            name: name.to_owned(),
            tip,
        })
    }

    fn target_busy(&self, target: &str) -> Result<Option<String>, LandError> {
        let full = format!("refs/heads/{target}");
        for e in self.git.worktree_list(&self.repo).map_err(git_error)? {
            if !e.path.exists() {
                continue;
            }
            let Ok(dir) = self
                .git
                .run_str(&e.path, ["rev-parse", "--absolute-git-dir"])
            else {
                continue;
            };
            let dir = PathBuf::from(dir);
            let read = |p: &str| {
                std::fs::read_to_string(dir.join(p))
                    .map(|s| s.trim().to_owned())
                    .ok()
            };
            for head_name in ["rebase-merge/head-name", "rebase-apply/head-name"] {
                if read(head_name).as_deref() == Some(full.as_str()) {
                    return Ok(Some(format!(
                        "a rebase of {target} is in progress in {}",
                        e.path.display()
                    )));
                }
            }
            if let Some(start) = read("BISECT_START")
                && (start == target || start == full)
            {
                return Ok(Some(format!(
                    "a bisect started from {target} is in progress in {}",
                    e.path.display()
                )));
            }
        }
        Ok(None)
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, LandError> {
        self.merge_back
            .is_ancestor(ancestor, descendant)
            .map_err(merge_error)
    }

    fn tree_of(&self, commit: &str) -> Result<String, LandError> {
        self.merge_back.tree_of(commit).map_err(merge_error)
    }

    fn count_commits(&self, base: &str, tip: &str) -> Result<usize, LandError> {
        let n = self
            .git
            .run_str(
                &self.repo,
                ["rev-list", "--count", &format!("{base}..{tip}")],
            )
            .map_err(git_error)?;
        n.parse()
            .map_err(|e| LandError::Git(format!("rev-list --count printed {n:?}: {e}")))
    }

    fn programs(&self, worktree: Option<&Path>) -> Result<ProgramSnapshot, LandError> {
        // Read where the target is checked out: its config.worktree,
        // `includeIf gitdir:` and relative `core.hooksPath` apply there.
        program_snapshot(&self.user_git, worktree.unwrap_or(&self.repo), None)
    }

    fn post_land_config_changes(
        &self,
        target: &TargetInfo,
        merge_commit: &str,
    ) -> Result<Vec<String>, LandError> {
        let Some(wt) = target.location.worktree() else {
            return Ok(Vec::new());
        };
        let top = self
            .user_git
            .exec(wt, ["rev-parse", "--show-toplevel"])
            .map_err(git_error)?;
        if !top.success() {
            return Err(LandError::Git(format!(
                "cannot find the top of {}: {}",
                wt.display(),
                top.stderr
            )));
        }
        let top = canonical(Path::new(&top.stdout_str()));
        // Every config file git reads there, and every include it names
        // (a missing included file is not listed by git, but the merge may
        // create it).
        let mut files: BTreeSet<PathBuf> = BTreeSet::new();
        let origins = self
            .user_git
            .exec(wt, ["config", "--list", "--show-origin", "-z"])
            .map_err(git_error)?;
        if !origins.success() {
            return Err(LandError::Git(format!(
                "cannot read the git configuration in {}: {}",
                wt.display(),
                origins.stderr
            )));
        }
        let mut fields = origins.stdout.split(|b| *b == 0);
        while let (Some(origin), Some(item)) = (fields.next(), fields.next()) {
            let origin = String::from_utf8_lossy(origin);
            let Some(file) = origin.strip_prefix("file:") else {
                continue;
            };
            let file = wt.join(file);
            files.insert(canonical(&file));
            let text = String::from_utf8_lossy(item);
            let (key, value) = text.split_once('\n').unwrap_or((&text, ""));
            let key = key.to_ascii_lowercase();
            if key == "include.path" || (key.starts_with("includeif.") && key.ends_with(".path")) {
                let value = Path::new(value);
                let target_file = if value.is_absolute() {
                    value.to_path_buf()
                } else {
                    file.parent().unwrap_or(wt).join(value)
                };
                files.insert(canonical(&target_file));
            }
        }
        files.insert(top.join(".lfsconfig"));
        // Attribute files can assign one of the user's trusted drivers (lfs)
        // to new paths: named with their diff.
        let mut out = Vec::new();
        for path in self
            .merge_paths(&target.tip, merge_commit)?
            .into_iter()
            .filter(|p| p.rsplit('/').next() == Some(".gitattributes"))
        {
            let diff = self
                .git
                .run_str(
                    &self.repo,
                    [
                        "diff",
                        "--no-ext-diff",
                        "--unified=0",
                        &target.tip,
                        merge_commit,
                        "--",
                        &format!(":(literal){path}"),
                    ],
                )
                .map_err(git_error)?;
            let lines: Vec<&str> = diff
                .lines()
                .filter(|l| {
                    (l.starts_with('+') || l.starts_with('-'))
                        && !l.starts_with("+++")
                        && !l.starts_with("---")
                })
                .take(10)
                .collect();
            out.push(format!(
                "the merge changes {path}, which assigns attributes (drivers included): {}",
                lines.join(" | ")
            ));
        }
        let rels: Vec<String> = files
            .iter()
            .filter_map(|f| f.strip_prefix(&top).ok())
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .filter(|r| !r.is_empty() && !r.starts_with(".git/"))
            .collect();
        if rels.is_empty() {
            return Ok(out);
        }
        for path in self.diff_paths(&target.tip, merge_commit, &rels)? {
            let now = match self.tree_entry(merge_commit, &path)? {
                Some((_, blob)) => format!("blob {blob}"),
                None => "deleted".into(),
            };
            out.push(format!(
                "the merge changes {path} ({now}), a config file your git reads after the land"
            ));
        }
        Ok(out)
    }

    fn worktree_clean(&self, worktree: &Path) -> Result<bool, LandError> {
        let dir = self
            .git
            .run_str(worktree, ["rev-parse", "--absolute-git-dir"])
            .map_err(git_error)?;
        if Path::new(&dir).join("index.lock").exists() {
            return Ok(false);
        }
        let status = self
            .git
            .run_str(worktree, ["status", "--porcelain", "--untracked-files=no"])
            .map_err(git_error)?;
        Ok(status.is_empty())
    }

    fn checkout_recovery(&self, worktree: &Path, entry: &LandingEntry) -> String {
        let run = entry
            .run_branch
            .strip_prefix("kgraf/")
            .unwrap_or(&entry.run_branch);
        format!(
            "Run `kgraf land --run {run} --recover`: it puts back each file the land changed in \
             {} that is still the old or the new version, removes files the land added, \
             removes a stale index.lock, and lists every file it leaves alone. Then land again",
            worktree.display()
        )
    }

    fn signing_on(&self) -> Result<bool, LandError> {
        Ok(user_commit_signing(&self.user_git, &self.repo)
            .map_err(git_error)?
            .is_signed())
    }

    fn build(
        &self,
        target_tip: &str,
        candidate: &str,
        message: &str,
        signed: bool,
    ) -> Result<Built, LandError> {
        // Read at each land: the user may change the setting at any time.
        let signing = if signed {
            user_commit_signing(&self.user_git, &self.repo).map_err(git_error)?
        } else {
            CommitSigning::Unsigned
        };
        match self
            .merge_back
            .landing_merge(target_tip, candidate, message, &signing)
            .map_err(merge_error)?
        {
            LandingMerge::Clean { commit, tree } => Ok(Built::Merge { commit, tree }),
            LandingMerge::Conflicted { paths, .. } => Ok(Built::Conflict { paths }),
        }
    }

    fn move_target(
        &self,
        entry: &LandingEntry,
        location: &TargetLocation,
        programs: Option<&ProgramSnapshot>,
    ) -> Result<(), LandError> {
        match location.worktree() {
            None => {
                let reason = format!("kgraf land: run branch {}", entry.run_branch);
                let moved = self
                    .merge_back
                    .move_branch(
                        &entry.target,
                        &entry.merge_commit,
                        &entry.target_old,
                        &reason,
                    )
                    .map_err(merge_error)?;
                if !moved {
                    return Err(LandError::TargetMoved {
                        target: entry.target.clone(),
                        expected: entry.target_old.clone(),
                        found: self.branch_tip(&entry.target)?.unwrap_or_default(),
                    });
                }
                Ok(())
            }
            Some(worktree) => {
                let full = format!("refs/heads/{}", entry.target);
                let refused = |stderr: String| LandError::TargetRefused {
                    target: entry.target.clone(),
                    worktree: worktree.clone(),
                    stderr,
                };
                // One repository lock across the checks and the move.
                let _lock =
                    kgraf_core::repo_lock::shared_for(worktree, "kgraf land: move the target")
                        .map_err(git_error)?;
                let target = TargetInfo {
                    name: entry.target.clone(),
                    tip: entry.target_old.clone(),
                    location: location.clone(),
                };
                let includes = self.post_land_config_changes(&target, &entry.merge_commit)?;
                if includes != entry.include_changes {
                    return Err(refused(format!(
                        "the config files the merge changes are not the ones confirmed (now: \
                         {}); nothing was moved, land again",
                        if includes.is_empty() {
                            "none".to_owned()
                        } else {
                            includes.join("; ")
                        }
                    )));
                }
                let (overrides, _empty_hooks) = land_overrides(&self.user_git, worktree, programs)?;
                let head = self
                    .user_git
                    .exec(worktree, ["symbolic-ref", "--quiet", "HEAD"])
                    .map_err(git_error)?;
                if !head.success() || head.stdout_str() != full {
                    return Err(refused(format!(
                        "{} no longer has {} checked out",
                        worktree.display(),
                        entry.target
                    )));
                }
                // The user's own git, so git itself refuses to overwrite a
                // local change; hooks and program settings as at run start.
                let mut args = overrides;
                args.extend(
                    ["merge", "--ff-only", "--quiet", entry.merge_commit.as_str()]
                        .map(str::to_owned),
                );
                let out = self.user_git.exec(worktree, &args);
                if let Some(put_back) = self.restore_wrong_branch(worktree, entry) {
                    return Err(refused(put_back));
                }
                // Stopped part way (its time limit, a signal): files may be
                // partly written, never "nothing changed".
                let interrupted = |detail: String| LandError::CheckoutInterrupted {
                    target: entry.target.clone(),
                    worktree: worktree.clone(),
                    detail,
                    recovery: self.checkout_recovery(worktree, entry),
                };
                let out = match out {
                    Ok(o) if o.code.is_none() => {
                        return Err(interrupted("git was ended by a signal".into()));
                    }
                    Ok(o) => o,
                    Err(e) => return Err(interrupted(e.to_string())),
                };
                if !out.success() {
                    return Err(refused(out.stderr));
                }
                match self.branch_tip(&entry.target)? {
                    Some(tip) if tip == entry.merge_commit => Ok(()),
                    found => Err(LandError::Git(format!(
                        "git merge --ff-only in {} exited 0 but {} is at {}, not {}",
                        worktree.display(),
                        entry.target,
                        found.unwrap_or_default(),
                        entry.merge_commit
                    ))),
                }
            }
        }
    }

    fn fold(&self, commit: &str) -> Result<FoldResult, LandError> {
        Ok(
            match self
                .merge_back
                .fold_into_integration(commit)
                .map_err(merge_error)?
            {
                Fold::AlreadyContains { .. } => FoldResult::AlreadyContains,
                Fold::FastForwarded { .. } => FoldResult::FastForwarded,
                Fold::Merged { commit, .. } => FoldResult::Merged { commit },
                Fold::Conflicted { paths, .. } => FoldResult::Conflicted { paths },
            },
        )
    }
}

/// What `kgraf land --recover` did to a worktree a stopped checkout left.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recovery {
    /// Files put back to the target's old version.
    pub restored: Vec<String>,
    /// Files the land added, removed.
    pub removed: Vec<String>,
    /// Files left alone, each with why.
    pub left: Vec<(String, String)>,
    /// What happened to `index.lock`, when there was one.
    pub lock: Option<String>,
}

impl GitLander {
    /// The blob id of the file at `path` as it is on disk (no filters), or
    /// `None` when absent.
    fn disk_blob(&self, worktree: &Path, path: &str) -> Result<Option<String>, LandError> {
        let file = worktree.join(path);
        let meta = match std::fs::symlink_metadata(&file) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(git_error(e)),
        };
        let bytes = if meta.file_type().is_symlink() {
            std::fs::read_link(&file)
                .map_err(git_error)?
                .to_string_lossy()
                .into_owned()
                .into_bytes()
        } else if meta.is_file() {
            std::fs::read(&file).map_err(git_error)?
        } else {
            return Ok(Some("not a file".into()));
        };
        let out = self
            .git
            .run_with_stdin(
                &self.repo,
                ["hash-object", "--no-filters", "--stdin"],
                &bytes,
            )
            .map_err(git_error)?;
        Ok(Some(String::from_utf8_lossy(&out).trim().to_owned()))
    }

    fn write_blob(
        &self,
        worktree: &Path,
        path: &str,
        mode: &str,
        blob: &str,
    ) -> Result<(), LandError> {
        // Modes (executable, link) apply where the file system has them.
        #[cfg(not(unix))]
        let _ = mode;
        let file = worktree.join(path);
        let bytes = self
            .git
            .run(&self.repo, ["cat-file", "blob", blob])
            .map_err(git_error)?;
        if std::fs::symlink_metadata(&file).is_ok() {
            std::fs::remove_file(&file).map_err(git_error)?;
        }
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(git_error)?;
        }
        #[cfg(unix)]
        if mode == "120000" {
            let target = String::from_utf8_lossy(&bytes).into_owned();
            return std::os::unix::fs::symlink(target, &file).map_err(git_error);
        }
        std::fs::write(&file, &bytes).map_err(git_error)?;
        #[cfg(unix)]
        if mode == "100755" {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
                .map_err(git_error)?;
        }
        Ok(())
    }

    /// Recover `worktree` after a stopped checkout of `entry`: for each path
    /// the land changes, act only when the file is the old version, the new
    /// version, or absent. A path the old version has is put back from it;
    /// a path the land added is removed. Every other path is left alone and
    /// listed. A stale `index.lock` is removed only when no process holds
    /// it. The index entries of the paths acted on are reset to `HEAD`.
    pub fn recover_checkout(
        &self,
        worktree: &Path,
        entry: &LandingEntry,
    ) -> Result<Recovery, LandError> {
        let mut r = Recovery::default();
        let gitdir = self
            .git
            .run_str(worktree, ["rev-parse", "--absolute-git-dir"])
            .map_err(git_error)?;
        let lock = Path::new(&gitdir).join("index.lock");
        if lock.exists() {
            let mut probe = Command::new(&self.lsof);
            probe.arg("-t").arg("--").arg(&lock);
            // Under the spawn lock, as every spawn: see `kgraf_core::child_run`.
            match kgraf_core::child_run::output(&mut probe) {
                Ok(o) if o.status.code() == Some(1) && o.stdout.is_empty() => {
                    std::fs::remove_file(&lock).map_err(git_error)?;
                    r.lock = Some(format!("removed {}, which no process held", lock.display()));
                }
                Ok(o) if !o.stdout.is_empty() => {
                    return Err(LandError::Run(format!(
                        "{} is held by process {}; nothing was changed. Let it finish, then \
                         recover again",
                        lock.display(),
                        String::from_utf8_lossy(&o.stdout).trim()
                    )));
                }
                _ => {
                    return Err(LandError::Run(format!(
                        "cannot tell whether a process holds {}; nothing was changed. Make sure \
                         no git runs there, remove it, and recover again",
                        lock.display()
                    )));
                }
            }
        }
        let mut acted = Vec::new();
        for path in self.merge_paths(&entry.target_old, &entry.merge_commit)? {
            let old = self.tree_entry(&entry.target_old, &path)?;
            let new = self.tree_entry(&entry.merge_commit, &path)?;
            if let Some(why) = unsafe_path(worktree, &path) {
                r.left.push((path, why));
                continue;
            }
            let disk = self.disk_blob(worktree, &path)?;
            let known = match &disk {
                None => true,
                Some(b) => {
                    old.as_ref().is_some_and(|(_, o)| o == b)
                        || new.as_ref().is_some_and(|(_, n)| n == b)
                }
            };
            if !known {
                r.left.push((
                    path,
                    "it is neither the old nor the new version (partly written, or your edit)"
                        .into(),
                ));
                continue;
            }
            match (old, disk) {
                (Some((mode, blob)), disk) => {
                    if disk.as_deref() != Some(blob.as_str()) {
                        self.write_blob(worktree, &path, &mode, &blob)?;
                    }
                    r.restored.push(path.clone());
                }
                (None, Some(_)) => {
                    std::fs::remove_file(worktree.join(&path)).map_err(git_error)?;
                    r.removed.push(path.clone());
                }
                (None, None) => {}
            }
            acted.push(path);
        }
        if !acted.is_empty() {
            let mut args = vec!["reset".to_owned(), "-q".into(), "--".into()];
            args.extend(acted.iter().map(|p| format!(":(literal){p}")));
            self.git.run(worktree, &args).map_err(git_error)?;
        }
        Ok(r)
    }
}

/// Why recovery must not write `path` under `worktree`: it leaves the work
/// tree, or a directory on the way is a link (writing would follow it).
fn unsafe_path(worktree: &Path, path: &str) -> Option<String> {
    let rel = Path::new(path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Some("the path leaves the work tree".into());
    }
    let mut at = worktree.to_path_buf();
    let parts: Vec<_> = rel.components().collect();
    for c in &parts[..parts.len().saturating_sub(1)] {
        at.push(c);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => {
                return Some(format!(
                    "{} is a link, and writing through it could leave the work tree",
                    at.display()
                ));
            }
            Ok(m) if !m.is_dir() => {
                return Some(format!("{} is not a directory", at.display()));
            }
            _ => {}
        }
    }
    None
}

/// `kgraf land --recover`: recover the worktree a stopped checkout of run
/// `run`'s open land left, under the run's lease. The journal stays open;
/// land again to finish.
pub fn recover_project_run(root: &Path, run: &RunId) -> Result<Recovery, LandError> {
    let p = open_project(root, run)?;
    let _lease = kgraf_orchestrator::take_run_lease_for_land(&p.graph, run, now_ms())
        .map_err(|e| LandError::Run(e.to_string()))?;
    let store = RunStore::new(p.graph.clone());
    let record = store
        .load(run)
        .map_err(|e| LandError::Run(e.to_string()))?
        .ok_or_else(|| LandError::Run(format!("there is no run {run} in this project")))?;
    let entry = record
        .land
        .landing
        .filter(|e| e.checkout_interrupted)
        .ok_or_else(|| {
            LandError::Run(format!("run {run} has no land stopped part way to recover"))
        })?;
    let target = p.lander.resolve_target(&entry.target)?;
    let worktree = target.location.worktree().ok_or_else(|| {
        LandError::Run(format!("{} is not checked out anywhere now", entry.target))
    })?;
    p.lander.recover_checkout(worktree, &entry)
}

fn canonical(path: &Path) -> PathBuf {
    if let Ok(c) = kgraf_core::plain_path::canonicalize(path) {
        return c;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// How often a land with no live loop renews the run's lease while its gate
/// runs.
pub const LEASE_RENEW_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// A land's context with no live loop: the run record from the store,
/// written back after each step, only while the run's lease is still held.
pub struct RecordLand {
    store: RunStore,
    record: RunStateRecord,
    gate: Option<Arc<dyn GateRunner>>,
    known_stubs: BTreeSet<String>,
    lease: Option<kgraf_orchestrator::LandLease>,
    renew_every: std::time::Duration,
}

impl RecordLand {
    pub fn new(
        store: RunStore,
        record: RunStateRecord,
        gate: Option<Arc<dyn GateRunner>>,
        known_stubs: BTreeSet<String>,
        lease: Option<kgraf_orchestrator::LandLease>,
    ) -> Self {
        Self {
            store,
            record,
            gate,
            known_stubs,
            lease,
            renew_every: LEASE_RENEW_EVERY,
        }
    }

    /// Renew the lease this often while a gate runs (tests).
    pub fn renewing_every(mut self, every: std::time::Duration) -> Self {
        self.renew_every = every;
        self
    }

    pub fn record(&self) -> &RunStateRecord {
        &self.record
    }
}

#[async_trait::async_trait]
impl LandContext for RecordLand {
    fn book(&mut self) -> &mut LandBook {
        &mut self.record.land
    }

    fn run_id(&self) -> String {
        self.record.run.to_string()
    }

    fn contracts(&self) -> Vec<String> {
        self.record.verified_contracts.clone()
    }

    fn known_stubs(&self) -> BTreeSet<String> {
        self.known_stubs.clone()
    }

    fn now_ms(&self) -> i64 {
        now_ms()
    }

    /// Writes the record only while the lease is held: a copy loaded under a
    /// lease that lapsed may be stale (a loop may have taken the run).
    fn persist(&mut self) -> Result<(), String> {
        if let Some(lease) = &self.lease {
            match lease.renew(now_ms()) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(format!(
                        "the lease of run {} lapsed or was taken by another process, so this \
                         copy of the record may be stale and was not written",
                        self.record.run
                    ));
                }
                Err(e) => return Err(format!("cannot renew the run's lease: {e}")),
            }
        }
        self.record.updated_ms = now_ms();
        self.store.save(&self.record).map_err(|e| e.to_string())
    }

    fn loop_live(&self) -> bool {
        false
    }

    async fn run_merge_gate(&mut self, commit: &str) -> Option<Result<GateVerdict, String>> {
        let runner = self.gate.clone()?;
        let commands = runner.commands();
        if commands.is_empty() {
            return None;
        }
        let run = runner.run(GateRequest {
            gate: ROOT_GATE.into(),
            covers: Vec::new(),
            commands,
            pinned: Some(VerifiedCommit {
                sha: commit.to_owned(),
                verified_by: "land: merge result".into(),
                verified_ms: now_ms(),
            }),
        });
        tokio::pin!(run);
        let mut tick = tokio::time::interval(self.renew_every);
        tick.tick().await;
        loop {
            tokio::select! {
                verdict = &mut run => return Some(verdict),
                _ = tick.tick() => {
                    // A lapse here is caught by `persist` before anything is
                    // written.
                    if let Some(lease) = &self.lease {
                        let _ = lease.renew(now_ms());
                    }
                }
            }
        }
    }

    fn open_remediation(&mut self, _commit: &str, _verdict: GateVerdict) -> Option<String> {
        None
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Land run `run` with no live loop: take the run's lease (refused while a
/// loop or another land holds it), load its record, run the one land
/// function, and save each step while the lease holds. `runs_dir` holds the
/// run's trace, read for the interfaces whose stubs landed.
pub async fn land_without_loop(
    lander: &GitLander,
    graph: &SharedGraph,
    runs_dir: &Path,
    run: &RunId,
    gate: Option<Arc<dyn GateRunner>>,
    request: &LandRequest,
) -> Result<LandOutcome, LandError> {
    let lease = kgraf_orchestrator::take_run_lease_for_land(graph, run, now_ms()).map_err(|e| {
        LandError::Run(format!(
            "run {run} is live or another land holds it; land it from its box with /land: {e}"
        ))
    })?;
    let store = RunStore::new(graph.clone());
    let record = store
        .load(run)
        .map_err(|e| LandError::Run(e.to_string()))?
        .ok_or_else(|| LandError::Run(format!("there is no run {run} in this project")))?;
    let trace = RunFiles::create(runs_dir, run)
        .map(|f| f.trace())
        .map_err(|e| LandError::Run(e.to_string()))?;
    let known_stubs = match std::fs::read_to_string(&trace) {
        Ok(text) => stub_lands_in_trace(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(e) => {
            return Err(LandError::Run(format!(
                "cannot read the run's trace {}: {e}",
                trace.display()
            )));
        }
    };
    let mut ctx = RecordLand::new(store, record, gate, known_stubs, Some(lease));
    kgraf_orchestrator::land_flow::land(lander, &mut ctx, request).await
}

/// The gate a land with no live loop runs on a merge result: the project's
/// validation commands, in a sandboxed worktree of the merge commit, as the
/// root gate runs them.
pub fn merge_result_gate(
    lander: &GitLander,
    state: &Path,
    commands: Vec<String>,
    timeout: std::time::Duration,
) -> Result<Arc<dyn GateRunner>, LandError> {
    let read_denies = crate::worker::assemble::state_read_denies(
        state,
        None,
        &state.join("ledger.db"),
        &state.join("runs"),
    )
    .map_err(git_error)?;
    Ok(Arc::new(crate::gates::CommandGateRunner {
        commands,
        manager: lander.manager.clone(),
        merge_back: lander.merge_back.clone(),
        read_denies,
        timeout,
        output_cap: crate::gates::GATE_OUTPUT_CAP,
    }))
}

/// What the land-suggestion rules read for `record`, from the repository:
/// whether a commit the root gate passed on is landable into the branch
/// checked out now (not on it yet, and no interface stub held without its
/// verified producer), whether the run reached a stopping point, and
/// whether that branch moved since the run started or last landed. No model
/// call.
pub fn suggest_facts(
    git: &dyn LandGit,
    record: &RunStateRecord,
    known_stubs: &BTreeSet<String>,
    benchmark: bool,
    now_ms: i64,
) -> Result<SuggestFacts, LandError> {
    let mut facts = SuggestFacts {
        benchmark,
        verified: record.verified_contracts.len(),
        now_ms,
        ..SuggestFacts::default()
    };
    let tip = git.run_tip()?;
    let on_branch = |sha: &str| git.is_ancestor(sha, &tip).map_err(|e| e.to_string());
    let Some(newest) = newest_passed(&record.land, &on_branch).map_err(LandError::Git)? else {
        return Ok(facts);
    };
    // The default target is the branch the run started from; with none
    // recorded a land needs --into, so nothing is suggested.
    let Some(start) = record.land.start_branch.clone() else {
        return Ok(facts);
    };
    let target = match git.resolve_target(&start.name) {
        Ok(t) => t,
        Err(LandError::UnbornTarget { .. } | LandError::NoSuchBranch(_)) => return Ok(facts),
        Err(e) => return Err(e),
    };
    struct Repo<'a>(&'a dyn LandGit);
    impl Ancestry for Repo<'_> {
        fn is_ancestor(&self, a: &str, d: &str) -> Result<bool, String> {
            self.0.is_ancestor(a, d).map_err(|e| e.to_string())
        }
    }
    let held =
        stub_blockers(&record.land, known_stubs, &newest, &Repo(git)).map_err(LandError::Git)?;
    facts.landable = !git.is_ancestor(&newest, &target.tip)? && held.is_empty();
    if facts.landable {
        facts.commits = git.count_commits(&target.tip, &newest)?;
        facts.target = Some(target.name.clone());
        facts.candidate = Some(newest.clone());
    }
    facts.stopping_point = facts.landable && record.open_contracts.is_empty() && newest == tip;
    let baseline = record
        .land
        .landed
        .iter()
        .rev()
        .find(|e| e.target == target.name)
        .map(|e| e.merge_commit.clone())
        .or_else(|| {
            record
                .land
                .start_branch
                .as_ref()
                .filter(|b| b.name == target.name)
                .map(|b| b.sha.clone())
        });
    facts.target_moved = facts.landable && baseline.is_some_and(|b| b != target.tip);
    Ok(facts)
}

/// The parts of a land of a project's run with no live loop.
struct ProjectLand {
    state: PathBuf,
    graph: SharedGraph,
    lander: GitLander,
    gate: Option<Arc<dyn GateRunner>>,
}

fn open_graph(root: &Path) -> Result<(PathBuf, SharedGraph), LandError> {
    let state = crate::state_dir(root).map_err(git_error)?;
    let path = state.join("graph.db");
    let graph = kgraf_graph::GraphStore::open_existing_for_facts(&path, root)
        .map_err(|e| LandError::Run(format!("cannot open {}: {e}", path.display())))?;
    Ok((state, Arc::new(std::sync::Mutex::new(graph))))
}

fn open_project(root: &Path, run: &RunId) -> Result<ProjectLand, LandError> {
    let (state, graph) = open_graph(root)?;
    let git = Git::discover().map_err(git_error)?;
    let branch = run_branch_name(&git, root, run.as_str());
    let lander = GitLander::open_with_git(root, &branch, &state.join("worktrees"), git)?;
    let commands = crate::gates::discover_validation_commands(root, &[]).map_err(git_error)?;
    let gate = if commands.is_empty() {
        None
    } else {
        Some(merge_result_gate(
            &lander,
            &state,
            commands,
            crate::config::defaults::gate_timeout(),
        )?)
    };
    Ok(ProjectLand {
        state,
        graph,
        lander,
        gate,
    })
}

/// Land run `run` of the project at `root` with no live loop, blocking:
/// the one land function under the run's lease (`kgraf land`, and the
/// app's `/land` when the run's loop is not live).
pub fn land_project_run(
    root: &Path,
    run: &RunId,
    request: &LandRequest,
) -> Result<LandOutcome, LandError> {
    let p = open_project(root, run)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(git_error)?;
    runtime.block_on(land_without_loop(
        &p.lander,
        &p.graph,
        &p.state.join("runs"),
        run,
        p.gate.clone(),
        request,
    ))
}

/// The run record and the suggestion facts of run `run` of the project at
/// `root`; `None` when the project has no such run.
pub fn project_suggest_facts(
    root: &Path,
    run: &RunId,
    benchmark: bool,
    now_ms: i64,
) -> Result<Option<(RunStateRecord, SuggestFacts)>, LandError> {
    let p = open_project(root, run)?;
    let store = RunStore::new(p.graph.clone());
    let Some(record) = store.load(run).map_err(|e| LandError::Run(e.to_string()))? else {
        return Ok(None);
    };
    let trace = p.state.join("runs").join(run.as_str()).join("trace.jsonl");
    let known = std::fs::read_to_string(&trace)
        .map(|t| stub_lands_in_trace(&t))
        .unwrap_or_default();
    let facts = suggest_facts(&p.lander, &record, &known, benchmark, now_ms)?;
    Ok(Some((record, facts)))
}

/// Record the user's "Not now" to a land suggestion in run `run`'s record,
/// with no live loop (under the run's lease).
pub fn record_land_decline(root: &Path, run: &RunId, now_ms: i64) -> Result<(), LandError> {
    let (_, graph) = open_graph(root)?;
    let _lease = kgraf_orchestrator::take_run_lease_for_land(&graph, run, now_ms)
        .map_err(|e| LandError::Run(e.to_string()))?;
    let store = RunStore::new(graph.clone());
    let Some(mut record) = store.load(run).map_err(|e| LandError::Run(e.to_string()))? else {
        return Ok(());
    };
    let verified = record.verified_contracts.len();
    record.land.land_declined_at = Some(now_ms);
    record.land.land_decline_count += 1;
    record.land.verified_at_decline = Some(verified);
    store
        .save(&record)
        .map_err(|e| LandError::Persist(e.to_string()))
}

/// `kgraf mirror rebuild RUN`: write the run's plan mirror in the graph again from
/// its record, the record winning (decision 0100, section 2). `None`: the
/// project has no record of the run. Touches only the mirror's rows; the record
/// is read, never written.
pub fn rebuild_run_mirror(
    root: &Path,
    run: &RunId,
) -> Result<Option<kgraf_orchestrator::MirrorRepair>, LandError> {
    let (_, graph) = open_graph(root)?;
    RunStore::new(graph)
        .rebuild_mirror(run)
        .map_err(|e| LandError::Run(e.to_string()))
}

/// The branch checked out at `root` and its commit; `None` on a detached
/// or unborn HEAD.
pub(crate) fn checked_out_branch(root: &Path) -> Option<(String, String)> {
    let git = Git::discover().ok()?;
    let name = git
        .run_str(root, ["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()?;
    let sha = git
        .run_str(root, ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .ok()?;
    Some((name, sha))
}

/// The run-start snapshot, read in the worktree that has the start branch
/// checked out (its `config.worktree`, `includeIf gitdir:` and hooks
/// apply), else in `root`.
pub fn start_snapshot(
    git: &Git,
    root: &Path,
    start_branch: Option<&str>,
    hooks_copy: &Path,
) -> Result<ProgramSnapshot, LandError> {
    let mut dir = root.to_path_buf();
    if let Some(name) = start_branch {
        let full = format!("refs/heads/{name}");
        if let Some(e) = git
            .worktree_list(root)
            .map_err(git_error)?
            .into_iter()
            .find(|e| e.branch.as_deref() == Some(full.as_str()))
        {
            dir = e.path;
        }
    }
    program_snapshot(&UserGit::from_git(git), &dir, Some(hooks_copy))
}

/// Give a run's loop its land side: `/land` through the live loop uses the
/// run's own merge-back, and a fresh run records its start branch.
pub(crate) fn attach(
    orchestrator: kgraf_orchestrator::Orchestrator,
    manager: &Arc<WorkspaceManager>,
    merge_back: &Arc<MergeBack>,
    start_branch: Option<(String, String)>,
    hooks_copy: &Path,
) -> crate::Result<kgraf_orchestrator::Orchestrator> {
    use crate::error::ResultExt;
    let git = Git::discover().step("git is needed for landing")?;
    // What the user's own git runs, as the run starts: the hooks are copied
    // into the run's own directory, which workers cannot write, and a land
    // runs with that copy and these settings.
    let programs = start_snapshot(
        &git,
        manager.repo(),
        start_branch.as_ref().map(|(name, _)| name.as_str()),
        hooks_copy,
    )
    .step("cannot read the repository's hooks and program settings")?;
    let lander = GitLander::from_parts(manager.clone(), merge_back.clone(), git);
    let orchestrator = orchestrator
        .with_lander(Arc::new(lander))
        .with_program_snapshot(programs);
    Ok(match start_branch {
        Some((name, sha)) => orchestrator.with_start_branch(name, sha),
        None => orchestrator,
    })
}

#[cfg(all(test, unix))]
#[path = "land_tests.rs"]
mod tests;
