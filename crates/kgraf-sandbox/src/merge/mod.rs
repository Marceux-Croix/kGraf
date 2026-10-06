//! Continuous merge-back (decision 0018): each worker's finished worktree is
//! committed from outside the sandbox and merged into the integration branch
//! as that worker finishes, not batched at a boundary.
//!
//! Ported from dagger/container-use at 2e43e625e95216b719ec9338f4034fd3a0be2734,
//! Apache-2.0 (see `third_party/container-use/UPSTREAM`):
//! - the environment lifecycle of `environment/environment.go` and
//!   `repository/repository.go` (create, apply changes, commit, merge,
//!   delete-after-merge) becomes the explicit [`AttemptState`] machine;
//! - `repository/git.go` `commitWorktreeChanges` (status, stage, commit with
//!   `-c core.hooksPath=/dev/null`) becomes [`MergeBack::commit`];
//! - `cmd/container-use/merge.go` and `Repository.Merge` (`merge --no-ff -m
//!   "Merge environment <id>"`, then `--delete`) become [`MergeBack::merge`].
//!
//! What changed, and why:
//! - Upstream merges into the user's checked-out branch with `--autostash`
//!   and leaves conflict markers in the user's working tree. Here the merge is
//!   computed in the object database (`git merge-tree --write-tree`) and the
//!   branch moves by compare-and-swap (`update-ref <new> <old>`). No working
//!   tree is touched, so a conflict can never half-apply, and it comes back as
//!   a typed [`MergeConflict`] (never auto-resolved, never a silent
//!   overwrite). The integration branch must therefore not be checked out.
//!   A clean git merge that creates paths differing only by case or Unicode
//!   normalization is also returned as a conflict.
//! - Upstream silently skips binary files and a fixed list of directories when
//!   staging. Here everything not ignored is staged (`add -A`), and whatever
//!   git would drop or mangle refuses the attempt as a typed [`Refusal`]
//!   (fail loud): FIFOs and other non-regular files, files over
//!   [`Limits::max_file_bytes`], nested repositories (git would record a
//!   gitlink to an unreachable commit and the files would vanish), and paths
//!   that steer git (`.gitattributes`, `.gitmodules`, `.git*` directories).
//!   In the Bypass mode the caller lets `.gitattributes`, `.gitmodules` and
//!   `.githooks` through as plain data ([`Attempt::allow_bypass_git_files`]).
//! - Upstream runs git with `cmd.Dir = worktree`, so git follows the worktree's
//!   `.git` file. Here every invocation passes an explicit `--git-dir` taken
//!   from the gitlink recorded at creation, goes through the host runner
//!   (`kgraf_core::host_git`: inherited `GIT_*` removed, hooks, config
//!   hooks, fsmonitor and transports off, every configured filter, merge and
//!   diff driver neutralized by name), sets `GIT_CONFIG_NOSYSTEM=1`, and is
//!   killed after [`Limits::git_timeout`].
//!   `Workspace::verify_gitlink` runs before any git process that concerns a
//!   worker's worktree.
//! - Attributes are read from one tree the owner trusts, fixed at open: the
//!   integration tip's tree when [`MergeBack`] was opened. Every invocation
//!   passes it as `--attr-source`, and a worktree invocation without it is
//!   refused. So no `.gitattributes` a worker writes can name a filter driver
//!   (which git would run on the host while hashing files) or a merge driver
//!   (a `merge=union` would silently auto-resolve conflicts) for its own
//!   attempt. Independently, the host runner empties every driver the config
//!   defines, so even the trusted tree's attributes run no program.
//! - Parallel attempts at one task ([`AttemptGroup`]) are never merged
//!   together: the verifier's selection merges one and discards the rest with
//!   their worktrees (disjoint writes).
//!
//! The API is synchronous, like the rest of this crate.

pub mod blob;
mod generated;
mod parse;
mod safe_git;
mod state;

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::WorkspaceError;
use crate::git::{Git, parse_porcelain_z};
use crate::workspace::{Workspace, WorkspaceManager};

pub use generated::{
    Excluded, GeneratedKind, LOCKFILES, file_head, generated_kind, is_compiled_binary,
    is_python_bytecode, lockfile_manifests, manifest_matches, manifests_beside,
};
use parse::{
    GITLINK_MODE, MIN_GIT, SYMLINK_MODE, is_protected, new_collisions, parse_merge_tree_z,
    parse_raw_diff_z, parse_version,
};
use safe_git::{SafeGit, Target};
pub use state::{
    ATTEMPT_REF_PREFIX, Attempt, AttemptEvent, AttemptGroup, AttemptState, CommitOutcome,
    ConflictMessage, ConflictSide, ConflictedPath, FinishOutcome, Identity, Leftovers, Limits,
    MergeConflict, MergeError, MergeOutcome, MergedWorktree, Refusal, RefusalReason, SelectFailure,
};

/// Retries of the integration-branch compare-and-swap when another process
/// moved the branch between computing and publishing a merge.
const CAS_RETRIES: usize = 3;

/// One merge computed by [`MergeBack::compute_merge`].
struct ComputedMerge {
    conflicted: bool,
    tree: String,
    paths: Vec<ConflictedPath>,
    collisions: Vec<Vec<String>>,
    messages: Vec<ConflictMessage>,
}

/// How a commit judges the attempt's changes against its declared writes
/// (decision 0083).
#[derive(Clone, Copy)]
enum Scope<'a> {
    /// [`MergeBack::commit`]: none declared; committed, never merged.
    Unscoped,
    /// [`MergeBack::commit_declared`]: a change to any other path refuses
    /// the commit.
    Declared(&'a [&'a str]),
    /// [`MergeBack::commit_retaining`]: generated files outside the scope are
    /// left out; any other change outside it is recorded for merge to refuse.
    Retain(&'a dyn Fn(&Path) -> bool),
}

/// Whether `path` is one of `writes`, compared as the file system resolves
/// names (case, normalization; `kgraf_core::paths::same_path`).
fn declares(writes: &[&str], path: &str) -> bool {
    writes
        .iter()
        .any(|w| kgraf_core::paths::same_path(Path::new(w), Path::new(path)))
}

impl ComputedMerge {
    fn is_clean(&self) -> bool {
        !self.conflicted && self.collisions.is_empty()
    }
}

/// Result of [`MergeBack::trial_merge`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrialMerge {
    /// Every input merged. `commit` is the unreferenced merge of all of them
    /// onto `tip` (`tip` itself when no input was given).
    Clean {
        tip: String,
        commit: String,
        tree: String,
    },
    /// Input `step` (an index into the inputs) did not merge onto the tip
    /// plus every input before it.
    Conflicted {
        tip: String,
        step: usize,
        paths: Vec<String>,
        /// Git's conflict labels, such as `CONFLICT (contents)`.
        kinds: Vec<String>,
    },
}

/// Commits finished worktrees from outside the sandbox and merges them into
/// one integration branch as each finishes.
pub struct MergeBack {
    git: SafeGit,
    manager: Arc<WorkspaceManager>,
    branch: String,
    identity: Identity,
    limits: Limits,
    /// Serializes merges in this process. Other processes are handled by the
    /// compare-and-swap on the branch ref.
    merging: Mutex<()>,
}

impl MergeBack {
    /// Merge into `refs/heads/<integration_branch>` of the manager's
    /// repository, with default [`Limits`].
    pub fn open(
        manager: Arc<WorkspaceManager>,
        integration_branch: &str,
        identity: Identity,
    ) -> Result<Self, MergeError> {
        Self::open_with(
            manager,
            integration_branch,
            identity,
            Limits::default(),
            Git::discover()?,
        )
    }

    /// As [`MergeBack::open`], with given limits and git executable.
    pub fn open_with(
        manager: Arc<WorkspaceManager>,
        integration_branch: &str,
        identity: Identity,
        limits: Limits,
        git: Git,
    ) -> Result<Self, MergeError> {
        check_identity(&identity)?;
        // Bootstrap from the main repository root (the user's checkout, not a
        // worker's), with every override but no attribute source yet, which
        // SafeGit enforces by refusing worktree targets.
        let boot = SafeGit {
            git,
            common_dir: PathBuf::new(),
            attr_tree: None,
            timeout: limits.git_timeout,
        };
        let root = Target::Discover(manager.repo());
        let version = boot.ok(&root, &[], ["version"], None)?;
        if parse_version(&version).is_none_or(|v| v < MIN_GIT) {
            return Err(MergeError::GitTooOld { found: version });
        }
        let common = boot.ok(
            &root,
            &[],
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
            None,
        )?;
        let mut git = SafeGit {
            git: boot.git,
            common_dir: PathBuf::from(common),
            attr_tree: None,
            timeout: limits.git_timeout,
        };
        git.ok(
            &Target::Repo,
            &[],
            ["check-ref-format", "--branch", integration_branch],
            None,
        )?;
        let tree = git.exec(
            &Target::Repo,
            &[],
            [
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{integration_branch}^{{tree}}"),
            ],
            None,
        )?;
        if tree.code != Some(0) {
            return Err(MergeError::NoIntegrationBranch {
                branch: integration_branch.to_string(),
            });
        }
        git.attr_tree = Some(String::from_utf8_lossy(&tree.stdout).trim().to_string());
        Ok(Self {
            git,
            manager,
            branch: integration_branch.to_string(),
            identity,
            limits,
            merging: Mutex::new(()),
        })
    }

    pub fn integration_branch(&self) -> &str {
        &self.branch
    }

    /// Current integration tip.
    pub fn integration_tip(&self) -> Result<String, MergeError> {
        self.git.ok(
            &Target::Repo,
            &[],
            [
                "rev-parse",
                "--verify",
                &format!("{}^{{commit}}", self.full_ref()),
            ],
            None,
        )
    }

    /// Whether `commit` is on the integration branch now: an ancestor of its
    /// current tip. This is the ground truth a caller checks a recorded
    /// `Integration::Merged { commit }` against before trusting it -- a merge
    /// commit this module returned is durable the instant `update-ref`
    /// succeeds, but nothing stops a later `merge`, a force-reset, or a
    /// record read well after the fact from disagreeing with what is on the
    /// branch right now, so "recorded as merged" and "actually on
    /// `kgraf/<run>`" are checked separately rather than assumed equal.
    pub fn commit_is_on_branch(&self, commit: &str) -> Result<bool, MergeError> {
        let tip = self.integration_tip()?;
        let out = self.git.exec(
            &Target::Repo,
            &[],
            ["merge-base", "--is-ancestor", commit, &tip],
            None,
        )?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(self.git.failed(
                &Target::Repo,
                &[
                    "merge-base".into(),
                    "--is-ancestor".into(),
                    commit.into(),
                    tip.into(),
                ],
                &out,
            )),
        }
    }

    /// Adopt a freshly created workspace as an attempt. Its name must form a
    /// valid ref under [`ATTEMPT_REF_PREFIX`] (`git check-ref-format`), so a
    /// name like `x.lock` or `a..b` fails here, before any work is done.
    pub fn attempt(&self, workspace: Workspace) -> Result<Attempt, MergeError> {
        self.adopt(workspace, None)
    }

    /// Adopt a workspace as its `verification`-th attempt: the commit goes to
    /// `refs/kgraf/attempts/<name>+<verification>`. A caller that verifies
    /// one worktree more than once (a run resumed after its verification was
    /// committed but not merged, or a pass voided and verified again) numbers
    /// each verification from a counter it persists, so the new commit never
    /// collides with the ref an earlier verification left.
    pub fn attempt_numbered(
        &self,
        workspace: Workspace,
        verification: u32,
    ) -> Result<Attempt, MergeError> {
        self.adopt(workspace, Some(verification))
    }

    fn adopt(&self, workspace: Workspace, number: Option<u32>) -> Result<Attempt, MergeError> {
        let attempt = Attempt {
            workspace,
            state: AttemptState::Working,
            number,
            excluded: Vec::new(),
            ignore_baseline: None,
            outside_scope: None,
            bypass_git_files: false,
        };
        let refname = attempt.attempt_ref();
        let out = self
            .git
            .exec(&Target::Repo, &[], ["check-ref-format", &refname], None)?;
        if out.code != Some(0) {
            return Err(MergeError::InvalidAttemptName {
                name: attempt.name().to_string(),
                detail: format!("git check-ref-format rejects {refname}"),
            });
        }
        Ok(attempt)
    }

    fn full_ref(&self) -> String {
        format!("refs/heads/{}", self.branch)
    }

    fn identity_env(&self) -> [(&str, &str); 4] {
        [
            ("GIT_AUTHOR_NAME", self.identity.name.as_str()),
            ("GIT_AUTHOR_EMAIL", self.identity.email.as_str()),
            ("GIT_COMMITTER_NAME", self.identity.name.as_str()),
            ("GIT_COMMITTER_EMAIL", self.identity.email.as_str()),
        ]
    }

    /// Verify the gitlink, then return the trusted admin dir. The only way
    /// this module obtains a worktree [`Target`].
    fn worktree_admin(ws: &Workspace) -> Result<PathBuf, MergeError> {
        ws.verify_gitlink()?;
        ws.admin_dir().ok_or_else(|| MergeError::NoAdminDir {
            name: ws.name().to_string(),
        })
    }

    /// Commit the worker's worktree (Working -> Committed, Empty or Refused).
    /// An empty attempt's workspace is removed; a refused one is kept for
    /// inspection until discarded. The worker must have exited.
    ///
    /// No write scope is declared, so the attempt is kept under its ref but
    /// can never be merged ([`MergeError::Unscoped`], decision 0083): for a run
    /// segment that is recorded, not integrated.
    pub fn commit(
        &self,
        attempt: &mut Attempt,
        message: &str,
    ) -> Result<CommitOutcome, MergeError> {
        self.commit_inner(attempt, message, Scope::Unscoped)
    }

    /// As [`MergeBack::commit`], against the writes the attempt declared: a
    /// change to any other path refuses the attempt, naming the path
    /// ([`RefusalReason::OutsideDeclaredWrites`], decision 0083). Empty
    /// `writes` means the attempt may change nothing. Paths compare as the file
    /// system resolves them (`kgraf_core::paths::same_path`).
    pub fn commit_declared<S: AsRef<str>>(
        &self,
        attempt: &mut Attempt,
        message: &str,
        writes: &[S],
    ) -> Result<CommitOutcome, MergeError> {
        let writes: Vec<&str> = writes.iter().map(AsRef::as_ref).collect();
        self.commit_inner(attempt, message, Scope::Declared(&writes))
    }

    /// As [`MergeBack::commit`], for a caller that runs a command in the
    /// worktree after the commit, such as an acceptance check. Three
    /// differences:
    ///
    /// - An empty attempt keeps its workspace; the caller removes it
    ///   (`WorkspaceManager::remove`) when done with it.
    /// - The commit is the snapshot of what the worker changed, taken before
    ///   the command can add to it, and the untracked `.gitignore` files of the
    ///   moment are recorded. What the command then writes is never part of the
    ///   attempt; [`MergeBack::discard_leftovers`] lists and removes it.
    /// - Files the worker's own run generated as a side effect are left out of
    ///   the commit (after `add -A`, so the size checks still see them) and
    ///   recorded in [`Attempt::excluded`]. A path is left out only when it is
    ///   new against the base, outside the declared scope (`in_scope` false),
    ///   and either a lockfile name ([`LOCKFILES`]) with one of its manifests in
    ///   the same folder of the snapshot, `**/__pycache__/*.pyc|*.pyo`, or a
    ///   Python package build's output (`dist/*.whl|*.tar.gz`,
    ///   `*.egg-info/`, `build/lib*/`, `build/bdist.*/`) with a Python
    ///   project manifest in the folder it was built in, or another build
    ///   tool's output: a file under an output folder with its project's
    ///   manifest beside it (`node_modules/` by `package.json`, `.venv/` by a
    ///   Python project file, Maven's `target/`, Gradle's `build/`, a .NET
    ///   project's `bin/` and `obj/`, ...), a tool cache (`.pytest_cache/`,
    ///   `.tox/`, `.gradle/`, `.coverage`, ...), compiled output by extension
    ///   (`.class`, `.o`, `.tsbuildinfo`), a file `tsc` emitted beside its
    ///   source, or a compiled program told by its first bytes
    ///   ([`generated_kind`]). A tracked lockfile, an in-scope path, or a
    ///   lockfile name with no manifest beside it is never left out: those are
    ///   the worker's writes, judged by the scope check like any other.
    /// - Any other change outside the declared scope is committed (the caller
    ///   reports it and keeps the attempt for discard) and recorded, and
    ///   [`MergeBack::merge`] refuses the attempt for it
    ///   ([`MergeError::OutsideDeclaredWrites`], decision 0083): the caller's
    ///   own scope check is the first line, this is the second.
    pub fn commit_retaining(
        &self,
        attempt: &mut Attempt,
        message: &str,
        in_scope: &dyn Fn(&Path) -> bool,
    ) -> Result<CommitOutcome, MergeError> {
        self.commit_inner(attempt, message, Scope::Retain(in_scope))
    }

    /// The paths of `changes` to leave out of the commit under the rule of
    /// [`MergeBack::commit_retaining`]. Manifests are looked up in the index,
    /// which after `add -A` is the snapshot.
    fn generated_by_worker(
        &self,
        wt: &Target<'_>,
        changes: &[parse::RawChange],
        in_scope: &dyn Fn(&Path) -> bool,
    ) -> Result<Vec<Excluded>, MergeError> {
        let new_outside: Vec<&str> = changes
            .iter()
            .filter(|c| c.old_mode.bytes().all(|b| b == b'0'))
            // A nested repository is never a build's output: it stays a
            // refusal (`RefusalReason::NestedRepository`) wherever it is.
            .filter(|c| c.new_mode != GITLINK_MODE)
            .map(|c| c.path.as_str())
            .filter(|p| !in_scope(Path::new(p)))
            .collect();
        // The manifests beside every candidate path, looked up in the index
        // with one `ls-files`; the rule itself is `generated_kind`. A
        // manifest named `*.ext` (a .NET project file) is a glob over its
        // folder; every other one is a literal path.
        let beside: BTreeSet<String> = new_outside
            .iter()
            .flat_map(|p| manifests_beside(Path::new(p)))
            .map(|m| m.to_string_lossy().into_owned())
            .collect();
        let listed = if beside.is_empty() {
            Vec::new()
        } else {
            let mut args: Vec<String> = ["ls-files", "-z", "--cached", "--"]
                .iter()
                .map(|a| (*a).to_owned())
                .collect();
            args.extend(beside.iter().map(|m| {
                let glob = Path::new(m)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("*."));
                if glob {
                    format!(":(glob){m}")
                } else {
                    format!(":(literal){m}")
                }
            }));
            self.git.ok_bytes(wt, &[], &args, None)?
        };
        let present: Vec<PathBuf> = listed
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| PathBuf::from(String::from_utf8_lossy(p).into_owned()))
            .collect();
        let root = match wt {
            Target::Worktree { work_tree, .. } => Some(*work_tree),
            _ => None,
        };
        let mut excluded: Vec<Excluded> = new_outside
            .into_iter()
            .filter_map(|path| {
                generated_kind(
                    Path::new(path),
                    |m| present.iter().any(|p| manifest_matches(m, p)),
                    // A program a compiler wrote is told by its first bytes,
                    // read without following a link.
                    |p| root.and_then(|r| file_head(r, p)),
                )
                .map(|kind| Excluded {
                    path: PathBuf::from(path),
                    kind,
                })
            })
            .collect();
        excluded.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(excluded)
    }

    fn commit_inner(
        &self,
        attempt: &mut Attempt,
        message: &str,
        scope: Scope<'_>,
    ) -> Result<CommitOutcome, MergeError> {
        let retain = match scope {
            Scope::Retain(in_scope) => Some(in_scope),
            _ => None,
        };
        let keep_empty = retain.is_some();
        // Check the transition before touching anything.
        attempt.state.next(AttemptEvent::NoChanges)?;
        attempt.outside_scope = None;
        let git_dir = Self::worktree_admin(&attempt.workspace)?;
        let ws = &attempt.workspace;
        let wt = Target::Worktree {
            git_dir: &git_dir,
            work_tree: ws.root(),
        };
        let base = ws.head().to_string();

        let head = self
            .git
            .ok(&wt, &[], ["rev-parse", "--verify", "HEAD"], None)?;
        if head != base {
            return Err(MergeError::HeadMoved {
                name: ws.name().to_string(),
                expected: base,
                found: head,
            });
        }

        // Bound and inspect the tree before git reads any of it.
        let ignored = self.git.ok_bytes(
            &wt,
            &[],
            [
                "ls-files",
                "-z",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
            ],
            None,
        )?;
        let ignored: HashSet<String> = ignored
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        // What `add -A` would add or change: untracked (not ignored) and
        // modified files. Only these count toward the total-bytes bound; an
        // unchanged tracked file is already in the base commit.
        let changed = self.git.ok_bytes(
            &wt,
            &[],
            [
                "ls-files",
                "-z",
                "--others",
                "--modified",
                "--exclude-standard",
            ],
            None,
        )?;
        let changed: HashSet<String> = changed
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        let mut reasons = prescan(ws.root(), &ignored, &changed, &self.limits)?;
        let mut excluded = Vec::new();

        if reasons.is_empty() {
            // Upstream: `status --porcelain`, stage, `-c core.hooksPath=/dev/null
            // commit`. Plumbing here: stage all, inspect what was staged,
            // write the tree, and build the commit.
            self.git.ok(&wt, &[], ["add", "-A", "--", "."], None)?;
            let raw = self.git.ok_bytes(
                &wt,
                &[],
                [
                    "diff-index",
                    "--cached",
                    "--raw",
                    "-z",
                    "--no-renames",
                    &base,
                ],
                None,
            )?;
            let mut changes =
                parse_raw_diff_z(&raw).map_err(|detail| WorkspaceError::GitOutput {
                    args: "diff-index --cached --raw -z".into(),
                    detail,
                })?;
            if let Some(in_scope) = retain {
                excluded = self.generated_by_worker(&wt, &changes, in_scope)?;
                if !excluded.is_empty() {
                    let mut stdin = Vec::new();
                    for e in &excluded {
                        stdin.extend_from_slice(&path_bytes(&e.path)?);
                        stdin.push(0);
                    }
                    self.git.ok(
                        &wt,
                        &[],
                        ["update-index", "-z", "--force-remove", "--stdin"],
                        Some(&stdin),
                    )?;
                    changes.retain(|c| !excluded.iter().any(|e| e.path == Path::new(&c.path)));
                }
            }
            // Two distinct paths the file system resolves as one (`Foo.rs` and
            // `foo.rs` on APFS or NTFS): one would overwrite the other in any
            // checkout there, and a declared write would match both
            // (decision 0083). Deleting one and adding the other is a rename
            // and passes.
            //
            // Compared against the whole base tree, not only among the changed
            // paths: a new `foo.rs` beside an existing `Foo.rs` overwrites it
            // in a checkout on those volumes although only one of the two is
            // in the change list. A collision the base already had is not
            // this attempt's doing, and a rename that removes the twin leaves
            // none behind.
            if !changes.is_empty() {
                let staged = self.git.ok_bytes(&wt, &[], ["ls-files", "-z"], None)?;
                let staged: Vec<String> = staged
                    .split(|b| *b == 0)
                    .filter(|p| !p.is_empty())
                    .map(|p| String::from_utf8_lossy(p).into_owned())
                    .collect();
                for paths in new_collisions(&self.tree_paths(&base)?, &staged) {
                    reasons.push(RefusalReason::PathCollision { paths });
                }
            }
            let mut outside = Vec::new();
            for change in changes {
                if is_protected(&change.path)
                    && !(attempt.bypass_git_files
                        && kgraf_core::paths::is_bypass_git_file(Path::new(&change.path)))
                {
                    reasons.push(RefusalReason::ProtectedPath {
                        path: change.path.clone(),
                    });
                }
                if change.new_mode == SYMLINK_MODE {
                    // Any link the attempt adds or retargets, whatever its
                    // target: a target judged lexically inside can still leave
                    // the tree through another link (`a/b/x -> ../..`, then
                    // `l -> a/b/x/..`), and two attempts can each add one half
                    // of such a chain. Deleting a link is a change to a
                    // non-link mode and passes (decision 0083).
                    let target = self
                        .git
                        .ok_bytes(
                            &Target::Repo,
                            &[],
                            ["cat-file", "blob", &change.new_oid],
                            None,
                        )
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_else(|e| format!("<unreadable: {e}>"));
                    reasons.push(RefusalReason::Symlink {
                        path: change.path.clone(),
                        target,
                    });
                }
                if change.new_mode == GITLINK_MODE && change.old_mode != GITLINK_MODE {
                    reasons.push(RefusalReason::NestedRepository {
                        path: change.path.clone(),
                    });
                }
                match scope {
                    Scope::Declared(writes) if !declares(writes, &change.path) => {
                        reasons.push(RefusalReason::OutsideDeclaredWrites { path: change.path });
                    }
                    Scope::Retain(in_scope) if !in_scope(Path::new(&change.path)) => {
                        outside.push(change.path);
                    }
                    _ => {}
                }
            }
            attempt.outside_scope = match scope {
                Scope::Unscoped => None,
                Scope::Declared(_) | Scope::Retain(_) => Some(outside),
            };
        }
        if !reasons.is_empty() {
            let refusal = Box::new(Refusal {
                attempt: attempt.name().to_string(),
                reasons,
            });
            attempt.advance(AttemptEvent::Refused(refusal.clone()))?;
            return Ok(CommitOutcome::Refused(refusal));
        }

        if retain.is_some() {
            attempt.ignore_baseline = Some(self.untracked_ignore_files(&wt)?.into_iter().collect());
            attempt.excluded = excluded;
        }
        let tree = self.git.ok(&wt, &[], ["write-tree"], None)?;
        let base_tree = self
            .git
            .ok(&wt, &[], ["rev-parse", &format!("{base}^{{tree}}")], None)?;
        if tree == base_tree {
            // Checked, removed, then recorded: `Empty` is final, so a removal
            // that fails (a process still running in the worktree) must leave
            // the attempt `Working` for the caller to commit again later.
            attempt.state.next(AttemptEvent::NoChanges)?;
            if !keep_empty {
                self.manager.remove(attempt.workspace.clone())?;
            }
            attempt.advance(AttemptEvent::NoChanges)?;
            return Ok(CommitOutcome::Empty);
        }
        let env = self.identity_env();
        let commit = self.git.ok(
            &wt,
            &env,
            [
                "commit-tree",
                "--no-gpg-sign",
                &tree,
                "-p",
                &base,
                "-m",
                message,
            ],
            None,
        )?;
        let attempt_ref = attempt.attempt_ref();
        // One transaction: the worktree HEAD (or its branch) moves to the
        // commit only if it is still at the base, and the attempt ref is
        // created only if absent.
        let script = format!("update HEAD {commit} {base}\ncreate {attempt_ref} {commit}\n");
        let args = ["update-ref", "-m", "kgraf: merge-back commit", "--stdin"];
        let out = self.git.exec(&wt, &[], args, Some(script.as_bytes()))?;
        if out.code != Some(0) {
            if out.stderr.contains("already exists") || out.stderr.contains("reference already") {
                return Err(MergeError::AttemptRefExists(attempt_ref));
            }
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            return Err(self.git.failed(&wt, &args, &out));
        }
        attempt.advance(AttemptEvent::Committed {
            commit: commit.clone(),
        })?;
        Ok(CommitOutcome::Committed { commit })
    }

    /// Every path a committed attempt changes against the base its workspace was
    /// created at, for a caller that checks the attempt against a declared write
    /// scope. Read in the shared repository (`diff-tree` of two commits), so no
    /// git process runs in the worker's tree. Everything the worker did is in the
    /// commit: [`MergeBack::commit`] refuses a worktree whose HEAD moved, so a
    /// worker cannot hide a change in a commit of its own.
    pub fn changed_paths(&self, attempt: &Attempt) -> Result<Vec<String>, MergeError> {
        let AttemptState::Committed { commit } = &attempt.state else {
            return Err(MergeError::InvalidTransition {
                from: attempt.state.label(),
                event: "changed_paths",
            });
        };
        let base = attempt.workspace.head().to_string();
        let out = self.git.ok_bytes(
            &Target::Repo,
            &[],
            [
                "diff-tree",
                "-r",
                "-z",
                "--name-only",
                "--no-renames",
                "--no-commit-id",
                &base,
                commit,
            ],
            None,
        )?;
        Ok(out
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect())
    }

    /// The tree an attempt's worktree must match after its commit: the attempt
    /// commit, or the base for an empty attempt. `None` in any other state.
    fn snapshot(attempt: &Attempt) -> Option<String> {
        match &attempt.state {
            AttemptState::Committed { commit } => Some(commit.clone()),
            AttemptState::Empty => Some(attempt.workspace.head().to_string()),
            _ => None,
        }
    }

    /// NUL-separated paths from a git listing, byte for byte, sorted.
    fn list_paths(&self, wt: &Target<'_>, args: &[&str]) -> Result<Vec<PathBuf>, MergeError> {
        let out = self.git.ok_bytes(wt, &[], args, None)?;
        let mut paths = out
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(git_path)
            .collect::<Result<Vec<_>, _>>()?;
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Every untracked `.gitignore` in the worktree, ignored or not. Listed
    /// without `--exclude-standard`, so a `.gitignore` that ignores itself (a
    /// `*` in it) is still seen, and matched without case (`icase`): on a
    /// case-insensitive volume git reads `.GitIgnore` as `.gitignore`.
    fn untracked_ignore_files(&self, wt: &Target<'_>) -> Result<Vec<PathBuf>, MergeError> {
        self.list_paths(
            wt,
            &[
                "ls-files",
                "-z",
                "--others",
                "--",
                ":(glob,icase)**/.gitignore",
            ],
        )
    }

    /// Remove each of `paths` beneath the worktree root, never following a
    /// symlink and never leaving the worktree ([`crate::confined`]). A path that
    /// no longer resolves as listed is skipped: the next listing shows what is
    /// there instead.
    fn remove_confined(&self, attempt: &Attempt, paths: &[PathBuf]) -> Result<(), MergeError> {
        if paths.is_empty() {
            return Ok(());
        }
        let root = attempt.workspace.root();
        let dir = crate::confined::open_root(root).map_err(|e| WorkspaceError::io(root, e))?;
        for rel in paths {
            crate::confined::remove(&dir, rel)
                .map_err(|e| WorkspaceError::io(root.join(rel), e))?;
        }
        Ok(())
    }

    /// Remove every file [`MergeBack::commit_retaining`] left out of the
    /// commit, before a check runs, so the check runs against the committed
    /// tree only: Python bytecode, so it imports from the committed sources;
    /// and a lockfile, so it never resolves dependencies to the versions,
    /// `git` sources or registry URLs the worker's run chose, which are not
    /// merged. A build the check runs generates its own lockfile, which goes
    /// with the check's leftovers.
    pub fn remove_excluded(&self, attempt: &Attempt) -> Result<(), MergeError> {
        let excluded: Vec<PathBuf> = attempt.excluded.iter().map(|e| e.path.clone()).collect();
        Self::worktree_admin(&attempt.workspace)?;
        self.remove_confined(attempt, &excluded)
    }

    /// Put the worktree back to the snapshot [`MergeBack::commit_retaining`]
    /// took, and return what was not in it. The caller must have stopped every
    /// process that could still write the worktree (killed the command's
    /// process group and checked the hold): removal is descriptor-relative and
    /// cannot be steered outside the worktree, but a writer still running would
    /// only add more.
    ///
    /// Each round, all read with the hardened git runner in the worktree:
    ///
    /// 1. untracked `.gitignore` files that were not there at the snapshot are
    ///    removed first, so none can hide what the command wrote;
    /// 2. untracked, unignored paths are removed (a directory git will not
    ///    enter, such as a nested repository, whole);
    /// 3. snapshot files changed or deleted are listed after those removals (a
    ///    case-only rename on a case-insensitive volume shows up only then)
    ///    and restored from the snapshot: the index is set to the snapshot's
    ///    tree with `read-tree -m`, which keeps the cached stat data of entries
    ///    that match, then `checkout-index` writes the listed files.
    ///
    /// Rounds repeat until one finds nothing. Anything still there after the
    /// last round is an error, never ignored (fail loud): a worktree that is
    /// reopened later (a worker continued after a lost lease) would carry it
    /// into its next commit as if the worker had written it.
    pub fn discard_leftovers(&self, attempt: &Attempt) -> Result<Leftovers, MergeError> {
        const ROUNDS: usize = 4;
        let Some(snapshot) = Self::snapshot(attempt) else {
            return Err(MergeError::InvalidTransition {
                from: attempt.state.label(),
                event: "discard_leftovers",
            });
        };
        let Some(baseline) = attempt.ignore_baseline.as_ref() else {
            return Err(MergeError::NoSnapshot {
                name: attempt.name().to_string(),
            });
        };
        let git_dir = Self::worktree_admin(&attempt.workspace)?;
        let wt = Target::Worktree {
            git_dir: &git_dir,
            work_tree: attempt.workspace.root(),
        };
        let mut found = Leftovers::default();
        for _ in 0..ROUNDS {
            // Refresh cached stat data first, so a file whose stat changed but
            // whose content did not (a `touch`) is not listed. Exit 1 only
            // means "needs update", which the listing reports precisely.
            self.git
                .exec(&wt, &[], ["update-index", "-q", "--refresh"], None)?;
            let ignores: Vec<PathBuf> = self
                .untracked_ignore_files(&wt)?
                .into_iter()
                .filter(|p| !baseline.contains(p))
                .collect();
            self.remove_confined(attempt, &ignores)?;
            let created =
                self.list_paths(&wt, &["ls-files", "-z", "--others", "--exclude-standard"])?;
            self.remove_confined(attempt, &created)?;
            // `--modified` lists deleted files too.
            let changed = self.list_paths(&wt, &["ls-files", "-z", "--modified"])?;
            if !changed.is_empty() {
                self.git
                    .ok(&wt, &[], ["read-tree", "-m", &snapshot], None)?;
                let mut stdin = Vec::new();
                for rel in &changed {
                    stdin.extend_from_slice(&path_bytes(rel)?);
                    stdin.push(0);
                }
                self.git.ok(
                    &wt,
                    &[],
                    ["checkout-index", "-f", "-u", "-q", "-z", "--stdin"],
                    Some(&stdin),
                )?;
            }
            if ignores.is_empty() && created.is_empty() && changed.is_empty() {
                found.created.sort();
                found.created.dedup();
                found.changed.sort();
                found.changed.dedup();
                return Ok(found);
            }
            found.created.extend(ignores);
            found.created.extend(created);
            found.changed.extend(changed);
        }
        self.git
            .exec(&wt, &[], ["update-index", "-q", "--refresh"], None)?;
        let mut remaining: Vec<PathBuf> = self
            .untracked_ignore_files(&wt)?
            .into_iter()
            .filter(|p| !baseline.contains(p))
            .collect();
        remaining
            .extend(self.list_paths(&wt, &["ls-files", "-z", "--others", "--exclude-standard"])?);
        remaining.extend(self.list_paths(&wt, &["ls-files", "-z", "--modified"])?);
        if remaining.is_empty() {
            return Ok(found);
        }
        Err(MergeError::LeftoversRemain {
            name: attempt.name().to_string(),
            remaining,
        })
    }

    /// Merge a committed attempt into the integration branch (Committed ->
    /// Merged or Conflicted). On a clean merge the workspace and attempt ref
    /// are removed; on a conflict both are kept and the branch does not move.
    pub fn merge(&self, attempt: &mut Attempt) -> Result<MergeOutcome, MergeError> {
        self.merge_then(attempt, MergedWorktree::Remove)
    }

    /// [`Self::merge`], with the user's choice of what a clean merge does
    /// with the attempt's worktree and its build output (decision 0088). The
    /// attempt ref is deleted either way; a conflict keeps both, as in
    /// [`Self::merge`].
    pub fn merge_then(
        &self,
        attempt: &mut Attempt,
        after: MergedWorktree,
    ) -> Result<MergeOutcome, MergeError> {
        let AttemptState::Committed { commit } = attempt.state.clone() else {
            return Err(MergeError::InvalidTransition {
                from: attempt.state.label(),
                event: "merge",
            });
        };
        // Only what the attempt declared reaches the branch (decision 0083).
        // The caller's scope check comes first; this holds without it.
        match &attempt.outside_scope {
            None => {
                return Err(MergeError::Unscoped {
                    name: attempt.name().to_string(),
                });
            }
            Some(paths) if !paths.is_empty() => {
                return Err(MergeError::OutsideDeclaredWrites {
                    name: attempt.name().to_string(),
                    paths: paths.clone(),
                });
            }
            Some(_) => {}
        }
        // The merge itself runs in the shared repository, but it acts on this
        // worker's output: refuse a tampered worktree before any git process.
        Self::worktree_admin(&attempt.workspace)?;
        let _serial = self.merging.lock().map_err(|_| MergeError::Poisoned)?;
        self.refuse_checked_out()?;

        for _ in 0..CAS_RETRIES {
            let tip = self.integration_tip()?;
            let computed = self.compute_merge(&tip, &commit)?;
            if !computed.is_clean() {
                let conflict = Box::new(MergeConflict {
                    attempt: attempt.name().to_string(),
                    integration_branch: self.branch.clone(),
                    integration_tip: tip,
                    attempt_commit: commit,
                    conflicted_tree: computed.tree,
                    paths: computed.paths,
                    collisions: computed.collisions,
                    messages: computed.messages,
                });
                attempt.advance(AttemptEvent::Conflicted(conflict.clone()))?;
                return Ok(MergeOutcome::Conflicted(conflict));
            }
            // Upstream: `merge --no-ff -m "Merge environment <id>"`. Always a
            // merge commit, so each attempt stays one attributable unit.
            let message = format!("Merge attempt {} into {}", attempt.name(), self.branch);
            let env = self.identity_env();
            let merge_commit = self.git.ok(
                &Target::Repo,
                &env,
                [
                    "commit-tree",
                    "--no-gpg-sign",
                    &computed.tree,
                    "-p",
                    &tip,
                    "-p",
                    &commit,
                    "-m",
                    &message,
                ],
                None,
            )?;
            let moved = self.git.exec(
                &Target::Repo,
                &[],
                [
                    "update-ref",
                    "-m",
                    &message,
                    &self.full_ref(),
                    &merge_commit,
                    &tip,
                ],
                None,
            )?;
            if moved.code != Some(0) {
                // Another process moved the branch: recompute against the new tip.
                if self.integration_tip()? != tip {
                    continue;
                }
                return Err(self.git.failed(
                    &Target::Repo,
                    &["update-ref".into(), self.full_ref().into()],
                    &moved,
                ));
            }
            attempt.advance(AttemptEvent::Merged {
                merge_commit: merge_commit.clone(),
            })?;
            // Upstream `merge --delete`: the environment has served its
            // purpose, unless the user keeps worktrees and their builds.
            let retired = match after {
                MergedWorktree::Remove => self.retire(attempt, Some(&commit)),
                MergedWorktree::Keep => self.delete_attempt_ref(attempt, &commit),
            };
            retired.map_err(|source| MergeError::CleanupFailed {
                merge_commit: merge_commit.clone(),
                source: Box::new(source),
            })?;
            return Ok(MergeOutcome::Merged {
                merge_commit,
                integration_before: tip,
            });
        }
        Err(MergeError::Contended {
            branch: self.branch.clone(),
            tries: CAS_RETRIES,
        })
    }

    /// Commit and merge in one step, against the writes the attempt declared
    /// ([`MergeBack::commit_declared`]): a change to any other path refuses
    /// the attempt, and empty `writes` may change nothing (decision 0083).
    pub fn finish<S: AsRef<str>>(
        &self,
        attempt: &mut Attempt,
        message: &str,
        writes: &[S],
    ) -> Result<FinishOutcome, MergeError> {
        match self.commit_declared(attempt, message, writes)? {
            CommitOutcome::Empty => Ok(FinishOutcome::Empty),
            CommitOutcome::Refused(r) => Ok(FinishOutcome::Refused(r)),
            CommitOutcome::Committed { .. } => self.merge(attempt).map(FinishOutcome::from),
        }
    }

    /// Delete the attempt refs (`<name>` and every numbered `<name>+<n>`) of
    /// each workspace name in `names`, and return the refs deleted. For the end
    /// of a run: refs a verification left and nothing merged or discarded (a
    /// kill between commit and merge, a stop). The caller passes only names no
    /// open work still uses: a reopen needs the ref of a committed, unmerged
    /// worktree.
    pub fn sweep_attempt_refs(&self, names: &[String]) -> Result<Vec<String>, MergeError> {
        let listed = self.git.ok(
            &Target::Repo,
            &[],
            [
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                ATTEMPT_REF_PREFIX,
            ],
            None,
        )?;
        let mut deleted = Vec::new();
        for line in listed.lines() {
            let Some((refname, oid)) = line.split_once(' ') else {
                continue;
            };
            let Some(tail) = refname.strip_prefix(ATTEMPT_REF_PREFIX) else {
                continue;
            };
            let name = tail.split_once('+').map_or(tail, |(n, _)| n);
            if names.iter().any(|n| n == name) {
                // Only while it still names what was listed.
                self.git
                    .ok(&Target::Repo, &[], ["update-ref", "-d", refname, oid], None)?;
                deleted.push(refname.to_owned());
            }
        }
        Ok(deleted)
    }

    /// Remove an unmerged attempt: its worktree and, if committed, its ref.
    pub fn discard(&self, attempt: &mut Attempt) -> Result<(), MergeError> {
        let commit = match &attempt.state {
            AttemptState::Committed { commit } => Some(commit.clone()),
            AttemptState::Conflicted(c) => Some(c.attempt_commit.clone()),
            _ => None,
        };
        attempt.state.next(AttemptEvent::Discarded)?;
        self.retire(attempt, commit.as_deref())?;
        attempt.advance(AttemptEvent::Discarded)
    }

    /// The verifier picked `winner` among parallel attempts at one task.
    ///
    /// Order: check the winner, commit it if still working, then discard
    /// every other attempt with its worktree, then merge the winner. A
    /// failure before a loser is discarded costs no loser its work: the
    /// failure carries back every attempt not yet discarded. No loser can
    /// reach the integration branch; the attempts are never merged together.
    /// When the winner turns out empty or refused, the losers are still
    /// discarded: the selection is final and the task is re-dispatched.
    /// A winner still working is committed against the task's declared
    /// `writes` ([`MergeBack::commit_declared`]); one already committed must
    /// have been committed against a scope, or its merge is refused.
    pub fn select<S: AsRef<str>>(
        &self,
        group: AttemptGroup,
        winner: &str,
        message: &str,
        writes: &[S],
    ) -> Result<(Attempt, FinishOutcome), Box<SelectFailure>> {
        let AttemptGroup { task, attempts } = group;
        let (mut chosen, mut losers): (Vec<Attempt>, Vec<Attempt>) =
            attempts.into_iter().partition(|a| a.name() == winner);
        let fail = |error, attempts| Box::new(SelectFailure { error, attempts });
        let Some(mut chosen) = chosen.pop() else {
            let error = MergeError::Selection {
                task,
                detail: format!("no attempt named {winner}"),
            };
            return Err(fail(error, losers));
        };
        let early = match &chosen.state {
            AttemptState::Working => match self.commit_declared(&mut chosen, message, writes) {
                Ok(CommitOutcome::Committed { .. }) => None,
                Ok(CommitOutcome::Empty) => Some(FinishOutcome::Empty),
                Ok(CommitOutcome::Refused(r)) => Some(FinishOutcome::Refused(r)),
                Err(error) => {
                    losers.push(chosen);
                    return Err(fail(error, losers));
                }
            },
            AttemptState::Committed { .. } => None,
            other => {
                let error = MergeError::InvalidTransition {
                    from: other.label(),
                    event: "select",
                };
                losers.push(chosen);
                return Err(fail(error, losers));
            }
        };
        while let Some(mut loser) = losers.pop() {
            if let Err(error) = self.discard(&mut loser) {
                losers.push(loser);
                losers.push(chosen);
                return Err(fail(error, losers));
            }
        }
        if let Some(outcome) = early {
            return Ok((chosen, outcome));
        }
        match self.merge(&mut chosen) {
            Ok(outcome) => Ok((chosen, outcome.into())),
            Err(error) => Err(fail(error, vec![chosen])),
        }
    }

    /// Delete the attempt ref (only if it still names `commit`) and remove
    /// the workspace.
    fn retire(&self, attempt: &Attempt, commit: Option<&str>) -> Result<(), MergeError> {
        if let Some(commit) = commit {
            self.delete_attempt_ref(attempt, commit)?;
        }
        // WorkspaceManager::remove runs `worktree unlock/remove --force
        // --force/prune` from the main repository. None of those runs a hook
        // or reads the worktree's index, so they need no override here.
        self.manager.remove(attempt.workspace.clone())?;
        Ok(())
    }

    /// Delete the attempt ref, only if it still names `commit`.
    fn delete_attempt_ref(&self, attempt: &Attempt, commit: &str) -> Result<(), MergeError> {
        self.git.ok(
            &Target::Repo,
            &[],
            ["update-ref", "-d", &attempt.attempt_ref(), commit],
            None,
        )?;
        Ok(())
    }

    /// Every path (files and directories) in a tree-ish.
    /// The merge of `theirs` into `ours`, computed in the object database
    /// (`merge-tree --write-tree`), with the case and normalization collision
    /// check. Writes objects only: no ref moves and no working tree changes.
    /// Shared by [`MergeBack::merge`] and [`MergeBack::trial_merge`], so a
    /// trial reports exactly what a real merge-back would.
    fn compute_merge(&self, ours: &str, theirs: &str) -> Result<ComputedMerge, MergeError> {
        let out = self.git.exec(
            &Target::Repo,
            &[],
            ["merge-tree", "--write-tree", "-z", ours, theirs],
            None,
        )?;
        let conflicted = match out.code {
            Some(0) => false,
            Some(1) => true,
            _ => {
                return Err(self.git.failed(
                    &Target::Repo,
                    &["merge-tree".into(), ours.into(), theirs.into()],
                    &out,
                ));
            }
        };
        let parsed =
            parse_merge_tree_z(&out.stdout).map_err(|detail| WorkspaceError::GitOutput {
                args: "merge-tree --write-tree -z".into(),
                detail,
            })?;
        let collisions = if conflicted {
            Vec::new()
        } else {
            new_collisions(&self.tree_paths(ours)?, &self.tree_paths(&parsed.tree)?)
        };
        let mut messages = parsed.messages;
        for group in &collisions {
            messages.push(ConflictMessage {
                paths: group.clone(),
                kind: "CONFLICT (path collision)".into(),
                message: format!(
                    "paths differ only by case or Unicode normalization: {}",
                    group.join(", ")
                ),
            });
        }
        Ok(ComputedMerge {
            conflicted,
            tree: parsed.tree,
            paths: parsed.paths,
            collisions,
            messages,
        })
    }

    /// `git gc --auto` in the shared repository, with merge-back's hardened
    /// invocation: git packs or prunes only past its own thresholds, as it
    /// does after its own commands. For callers that wrote many unreferenced
    /// objects ([`MergeBack::trial_merge`]).
    pub fn gc_auto(&self) -> Result<(), MergeError> {
        self.git
            .ok(&Target::Repo, &[], ["gc", "--auto", "--quiet"], None)
            .map(|_| ())
    }

    /// Speculative merge (decisions 0022, 0074): merge `commits` one after
    /// another onto the current integration tip, in the object database only.
    ///
    /// Each clean step is recorded as an unreferenced merge commit so the next
    /// step (and a validation run) has a commit to start from; nothing else is
    /// written. **The integration branch never moves, no ref is created, and
    /// no working tree is touched**, so a trial can run while workers write and
    /// while merge-back lands other attempts. The unreferenced commits are
    /// left to git's object pruning, like a conflict's `conflicted_tree`.
    ///
    /// Stops at the first step that does not merge cleanly (a textual conflict
    /// or a path collision) and names which input it was.
    pub fn trial_merge(&self, commits: &[String]) -> Result<TrialMerge, MergeError> {
        let tip = self.integration_tip()?;
        let mut acc = tip.clone();
        let mut tree = self.git.ok(
            &Target::Repo,
            &[],
            ["rev-parse", &format!("{tip}^{{tree}}")],
            None,
        )?;
        for (step, commit) in commits.iter().enumerate() {
            let computed = self.compute_merge(&acc, commit)?;
            if !computed.is_clean() {
                return Ok(TrialMerge::Conflicted {
                    tip,
                    step,
                    paths: computed
                        .paths
                        .into_iter()
                        .map(|p| p.path)
                        .chain(computed.collisions.into_iter().flatten())
                        .collect(),
                    // Only the conflicts; git also reports `Auto-merging`.
                    kinds: computed
                        .messages
                        .into_iter()
                        .map(|m| m.kind)
                        .filter(|k| k.starts_with("CONFLICT"))
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                });
            }
            let env = self.identity_env();
            acc = self.git.ok(
                &Target::Repo,
                &env,
                [
                    "commit-tree",
                    "--no-gpg-sign",
                    &computed.tree,
                    "-p",
                    &acc,
                    "-p",
                    commit,
                    "-m",
                    "kgraf: speculative merge (never on a branch)",
                ],
                None,
            )?;
            tree = computed.tree;
        }
        Ok(TrialMerge::Clean {
            tip,
            commit: acc,
            tree,
        })
    }

    fn tree_paths(&self, treeish: &str) -> Result<Vec<String>, MergeError> {
        let out = self.git.ok_bytes(
            &Target::Repo,
            &[],
            ["ls-tree", "-r", "-t", "-z", "--name-only", treeish],
            None,
        )?;
        Ok(out
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect())
    }

    fn refuse_checked_out(&self) -> Result<(), MergeError> {
        let args = ["worktree", "list", "--porcelain", "-z"];
        let out = self.git.ok_bytes(&Target::Repo, &[], args, None)?;
        let entries = parse_porcelain_z(&out).map_err(|detail| WorkspaceError::GitOutput {
            args: args.join(" "),
            detail,
        })?;
        let full = self.full_ref();
        if let Some(entry) = entries
            .into_iter()
            .find(|e| e.branch.as_deref() == Some(full.as_str()))
        {
            return Err(MergeError::IntegrationCheckedOut {
                branch: self.branch.clone(),
                worktree: entry.path,
            });
        }
        Ok(())
    }
}

/// A path as git printed it, byte for byte: a name that is not UTF-8 is
/// never replaced by a lossy spelling that names a different file.
fn git_path(bytes: &[u8]) -> Result<PathBuf, MergeError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes.to_vec())
            .map(PathBuf::from)
            .map_err(|e| {
                WorkspaceError::GitOutput {
                    args: "ls-files -z".into(),
                    detail: format!("a path is not UTF-8: {e}"),
                }
                .into()
            })
    }
}

/// The bytes git reads for `path` on stdin, the inverse of [`git_path`].
fn path_bytes(path: &Path) -> Result<Vec<u8>, MergeError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(path.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        path.to_str()
            .map(|s| s.replace('\\', "/").into_bytes())
            .ok_or_else(|| {
                WorkspaceError::GitOutput {
                    args: "checkout-index --stdin".into(),
                    detail: format!("{} is not UTF-8", path.display()),
                }
                .into()
            })
    }
}

/// The user is the sole author of every commit. An empty identity or one naming Claude or Anthropic is refused.
fn check_identity(identity: &Identity) -> Result<(), MergeError> {
    let name = identity.name.trim();
    let email = identity.email.trim();
    if name.is_empty() || email.is_empty() {
        return Err(MergeError::IdentityRefused(
            "name and email are both required".into(),
        ));
    }
    let both = format!("{name} {email}").to_lowercase();
    if both.contains("claude") || both.contains("anthropic") {
        return Err(MergeError::IdentityRefused(format!(
            "{name} <{email}> names Claude or Anthropic; the user is the sole author"
        )));
    }
    Ok(())
}

/// Walk the worktree before staging. Report every file git would skip
/// (FIFOs, sockets, devices) or that exceeds `limit`. Ignored paths (from
/// `ls-files --others --ignored --directory`, directories ending in `/`) are
/// not walked, and `.git` entries are git's own.
fn prescan(
    root: &Path,
    ignored: &HashSet<String>,
    changed: &HashSet<String>,
    limits: &Limits,
) -> Result<Vec<RefusalReason>, MergeError> {
    let limit = limits.max_file_bytes;
    let mut reasons = Vec::new();
    // Every added or modified regular file, for the total-bytes bound.
    let mut staged: Vec<(u64, String)> = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel_dir)) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| WorkspaceError::io(&dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| WorkspaceError::io(&dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            let rel = format!("{rel_dir}{name}");
            let meta = entry
                .path()
                .symlink_metadata()
                .map_err(|e| WorkspaceError::io(entry.path(), e))?;
            let kind = meta.file_type();
            if kind.is_dir() {
                let rel_slash = format!("{rel}/");
                if !ignored.contains(&rel_slash) {
                    stack.push((entry.path(), rel_slash));
                }
            // A link is not walked or sized here (it is never followed); git
            // stages it as a link, and the staged-change pass refuses any link
            // the attempt adds or retargets (decision 0083).
            } else if ignored.contains(&rel) || kind.is_symlink() {
                continue;
            } else if kind.is_file() {
                if changed.contains(&rel) {
                    staged.push((meta.len(), rel.clone()));
                }
                if meta.len() > limit {
                    reasons.push(RefusalReason::FileTooLarge {
                        path: rel,
                        bytes: meta.len(),
                        limit,
                    });
                }
            } else {
                reasons.push(RefusalReason::NotARegularFile {
                    path: rel,
                    kind: special_kind(&kind),
                });
            }
        }
    }
    let total: u64 = staged.iter().map(|(bytes, _)| bytes).sum();
    if total > limits.max_total_bytes {
        staged.sort_by(|a, b| b.cmp(a));
        reasons.push(RefusalReason::TooMuchData {
            bytes: total,
            limit: limits.max_total_bytes,
            files: staged.len(),
            largest: staged.iter().take(5).map(|(_, p)| p.clone()).collect(),
        });
    }
    reasons.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    Ok(reasons)
}

#[cfg(unix)]
fn special_kind(kind: &std::fs::FileType) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    if kind.is_fifo() {
        "fifo"
    } else if kind.is_socket() {
        "socket"
    } else if kind.is_block_device() {
        "block device"
    } else if kind.is_char_device() {
        "character device"
    } else {
        "unknown"
    }
}

#[cfg(not(unix))]
fn special_kind(_kind: &std::fs::FileType) -> &'static str {
    "not a regular file"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_must_be_the_owners() {
        let id = |name: &str, email: &str| Identity {
            name: name.into(),
            email: email.into(),
        };
        assert!(check_identity(&id("Marceux-Croix", "m@example.com")).is_ok());
        for bad in [
            id("", "m@example.com"),
            id("M", " "),
            id("Claude", "noreply@anthropic.com"),
            id("M", "CLAUDE@example.com"),
            id("Anthropic Bot", "x@y"),
        ] {
            assert!(
                matches!(check_identity(&bad), Err(MergeError::IdentityRefused(_))),
                "{bad:?}"
            );
        }
    }
}
