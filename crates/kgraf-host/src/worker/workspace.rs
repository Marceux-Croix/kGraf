//! The worker's own git worktree and how its work is carried from one
//! run segment to the next, through `kgraf-sandbox`'s merge-back API
//! (decision 0018). No plain host `git` runs in a worker's tree here.
//!
//! **One segment, one attempt.** Each run segment of worker `<w>` works in a
//! worktree named `<w>-<n>`, planned from the transcript
//! ([`super::run::plan_segment`]) and adopted as a merge-back [`Attempt`]
//! before the worker starts. When the segment ends, however it ends (a
//! finished run, a failure, a cancel, an interrupt, an error in the harness),
//! [`finish`] commits it with `MergeBack::commit`, which runs git through the
//! hardened `SafeGit`: attributes read from the integration tip's tree (never
//! the worker's `.gitattributes`), the host runner's overrides
//! (`kgraf_core::host_git`: hooks, drivers, fsmonitor, transports off), the gitlink
//! verified, protected paths (`.gitattributes`, `.gitmodules`, `.git*`
//! directories), nested repositories, FIFOs, oversized files and an oversized
//! total refused as a typed outcome naming each path. The commit is left under
//! `refs/kgraf/attempts/<w>-<n>` and the worktree is removed; a refused or
//! failed commit keeps the worktree for inspection. The end is recorded in
//! the transcript either way.
//!
//! **Never merged here.** Merging an attempt into the integration branch is
//! the orchestrator's decision (a verifier picks among attempts), not the
//! worker's; nothing in this module calls `merge`, `finish` or `select` on
//! `MergeBack`.
//!
//! **Nothing is purged.** `WorkspaceManager::create` replaces a dead owner's
//! worktree of the same name. The plan never names a worktree that exists
//! ([`worktree_exists`]), so a kept or unfinished segment is never replaced.
//!
//! **Build directory (decision 0060).** Each worktree builds into its own
//! directory, `<scratch>/target` (the shell tool's `CARGO_TARGET_DIR`), seeded
//! by a copy-on-write clone of a warm build directory when one is given. It is
//! in the workspace's scratch directory, outside the worktree, so merge-back
//! never sees build output whether or not the repository ignores `target/`,
//! and it is removed with the workspace. This is `seed_target_dir` with that
//! destination rather than `seed_workspace_target`, whose `<root>/target`
//! would be staged in a repository that does not ignore it.

use super::run::{AttemptFacts, NewestAttempt, RepoView, SegmentEnd, SegmentPlan};
use anyhow::{Context, Result};
use kgraf_sandbox::git::Git;
use kgraf_sandbox::{
    ATTEMPT_REF_PREFIX, Attempt, CommitOutcome, Identity, MergeBack, SeedReport, Workspace,
    WorkspaceManager, WorkspaceSpec, seed_target_dir,
};
use kgraf_tools::ShellTool;
use std::path::Path;

/// The newest attempt ref a worker's earlier segments left, with its commit.
/// Read from the dispatching repository, never from a worker's tree.
pub fn newest_attempt(repo: &Path, worker: &str) -> Result<NewestAttempt> {
    let git = Git::discover().context("git is needed to find the worker's attempts")?;
    let prefix = format!("{ATTEMPT_REF_PREFIX}{worker}-");
    let listed = git
        .run_str(
            repo,
            [
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                // A glob: a literal pattern matches only whole path components.
                &format!("{prefix}*"),
            ],
        )
        .context("cannot list the worker's attempt refs")?;
    let mut found: Vec<(u64, String, String)> = listed
        .lines()
        .filter_map(|line| {
            let (reference, commit) = line.split_once(' ')?;
            let n = reference.strip_prefix(&prefix)?.parse::<u64>().ok()?;
            Some((n, reference.to_owned(), commit.to_owned()))
        })
        .collect();
    found.sort();
    Ok(found
        .pop()
        .map(|(_, reference, commit)| (reference, commit)))
}

/// Whether a worktree named `name` exists under the manager, registered or as
/// a directory. The plan refuses such a name rather than let `create` purge it.
pub fn worktree_exists(manager: &WorkspaceManager, name: &str) -> Result<bool> {
    let path = manager.base().join(name);
    if path.exists() {
        return Ok(true);
    }
    Ok(manager
        .list()
        .context("cannot list the repository's worktrees")?
        .iter()
        .any(|entry| entry.path == path))
}

/// The commit identity of the dispatching repository's owner, from its own
/// git configuration (the user's checkout, not a worker's tree). Merge-back
/// refuses an identity naming Claude or Anthropic.
pub fn owner_identity(repo: &Path) -> Result<Identity> {
    let git = Git::discover().context("git is needed to read the commit identity")?;
    let read = |key: &str| {
        git.run_str(repo, ["config", "--get", key])
            .with_context(|| {
                format!("git config {key} is not set; merge-back commits need the owner's identity")
            })
    };
    Ok(Identity {
        name: read("user.name")?,
        email: read("user.email")?,
    })
}

/// The integration branch merge-back reads attributes from: the branch the
/// dispatching repository has checked out.
pub fn current_branch(repo: &Path) -> Result<String> {
    let git = Git::discover().context("git is needed to find the current branch")?;
    git.run_str(repo, ["symbolic-ref", "--short", "HEAD"])
        .context("the repository is on a detached HEAD; name --integration-branch")
}

/// The repository as [`super::run::plan_segment`] reads it, for one worker.
/// Every query runs git in the dispatching repository, never in a worker's
/// tree.
pub struct GitView<'a> {
    pub repo: &'a Path,
    pub manager: &'a WorkspaceManager,
    pub worker: &'a str,
}

impl RepoView for GitView<'_> {
    fn newest_attempt(&self) -> Result<NewestAttempt> {
        newest_attempt(self.repo, self.worker)
    }

    fn worktree_exists(&self, name: &str) -> Result<bool> {
        worktree_exists(self.manager, name)
    }

    fn attempt(&self, segment: &str) -> Result<Option<AttemptFacts>> {
        let git = Git::discover().context("git is needed to read an attempt ref")?;
        let attempt_ref = format!("{ATTEMPT_REF_PREFIX}{segment}");
        let listed = git
            .run_str(
                self.repo,
                [
                    "for-each-ref",
                    "--format=%(objectname) %(parent)",
                    &attempt_ref,
                ],
            )
            .context("cannot read the segment's attempt ref")?;
        listed
            .lines()
            .next()
            .map(|line| parse_attempt_line(line, &attempt_ref))
            .transpose()
    }

    fn resolve(&self, commitish: &str) -> Result<String> {
        let git = Git::discover().context("git is needed to resolve the start")?;
        git.run_str(
            self.repo,
            ["rev-parse", "--verify", &format!("{commitish}^{{commit}}")],
        )
        .with_context(|| format!("`{commitish}` is not a commit"))
    }
}

/// Read one `%(objectname) %(parent)` line of an attempt ref. A root commit
/// has no parent (the field is empty, which matches no recorded start). A
/// merge-back commit has exactly one parent, so a second one means the ref does
/// not name a merge-back commit, and the line is refused instead of trusting
/// the first parent.
fn parse_attempt_line(line: &str, attempt_ref: &str) -> Result<AttemptFacts> {
    let mut fields = line.split_whitespace();
    let commit = fields.next().unwrap_or_default().to_owned();
    let parent = fields.next().unwrap_or_default().to_owned();
    if fields.next().is_some() {
        anyhow::bail!(
            "attempt ref {attempt_ref} names commit {commit}, which has more than one parent; \
             a merge-back commit has exactly one"
        );
    }
    Ok(AttemptFacts {
        attempt_ref: attempt_ref.to_owned(),
        commit,
        parent,
    })
}

/// One run segment's workspace, adopted as a merge-back attempt.
pub struct Segment {
    pub attempt: Attempt,
    /// The seeded build directory, when a warm one was given.
    pub seeded: Option<SeedReport>,
}

impl std::fmt::Debug for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segment")
            .field("name", &self.workspace().name())
            .field("root", &self.workspace().root())
            .field("seeded", &self.seeded)
            .finish()
    }
}

impl Segment {
    pub fn workspace(&self) -> &Workspace {
        self.attempt.workspace()
    }
}

/// Create the planned segment's worktree, seed its build directory from
/// `warm_target` when given (decision 0060), and adopt it as an attempt.
pub fn open(
    manager: &WorkspaceManager,
    merge_back: &MergeBack,
    plan: &SegmentPlan,
    warm_target: Option<&Path>,
) -> Result<Segment> {
    let workspace = manager
        .create(&WorkspaceSpec {
            name: &plan.name,
            base: &plan.start,
            branch: None,
        })
        .with_context(|| format!("cannot create worktree `{}` at {}", plan.name, plan.start))?;
    let seeded = match warm_target {
        Some(warm) => Some(
            seed_target_dir(warm, &ShellTool::target_dir(&workspace))
                .with_context(|| format!("cannot seed {}'s build directory", plan.name))?,
        ),
        None => None,
    };
    let attempt = merge_back
        .attempt(workspace)
        .with_context(|| format!("`{}` cannot be a merge-back attempt", plan.name))?;
    Ok(Segment { attempt, seeded })
}

/// Commit a segment through merge-back, without removing its worktree.
/// Never returns an error: how the commit went is the result. The caller
/// records it, then calls [`remove`] for a committed end (the
/// record comes before the removal, so a kill in between leaves a
/// recorded end and a leftover worktree, never a removed worktree with no
/// record). A refused or failed commit keeps the worktree for inspection;
/// an empty one was already removed by merge-back.
pub fn commit(merge_back: &MergeBack, segment: &mut Segment, message: &str) -> SegmentEnd {
    let kept = segment.workspace().root().display().to_string();
    match merge_back.commit(&mut segment.attempt, message) {
        Ok(CommitOutcome::Committed { commit }) => SegmentEnd::Committed {
            attempt_ref: segment.attempt.attempt_ref(),
            commit,
        },
        Ok(CommitOutcome::Empty) => SegmentEnd::Empty,
        Ok(CommitOutcome::Refused(refusal)) => SegmentEnd::Refused {
            reasons: format!("{:?}", refusal.reasons),
            kept,
        },
        Err(e) => SegmentEnd::NotCarried {
            error: format!("{e:#}"),
            kept,
        },
    }
}

/// Remove a committed segment's worktree. The work is under its attempt ref.
pub fn remove(manager: &WorkspaceManager, segment: Segment) -> Result<()> {
    let root = segment.workspace().root().display().to_string();
    manager
        .remove(segment.attempt.workspace().clone())
        .with_context(|| format!("the work is committed, but {root} could not be removed"))
}

#[cfg(test)]
mod attempt_line_tests {
    use super::parse_attempt_line;

    #[test]
    fn a_commit_with_one_parent_is_read() {
        let facts = parse_attempt_line("c0ffee p1", "refs/kgraf/a").expect("one parent");
        assert_eq!(facts.commit, "c0ffee");
        assert_eq!(facts.parent, "p1");
    }

    #[test]
    fn a_root_commit_has_an_empty_parent() {
        let facts = parse_attempt_line("c0ffee ", "refs/kgraf/a").expect("root");
        assert_eq!(facts.parent, "");
    }

    #[test]
    fn a_commit_with_two_parents_is_refused() {
        let err = parse_attempt_line("c0ffee p1 p2", "refs/kgraf/a").expect_err("two parents");
        assert!(err.to_string().contains("more than one parent"), "{err}");
    }
}
