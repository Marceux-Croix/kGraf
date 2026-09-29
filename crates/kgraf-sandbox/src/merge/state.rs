//! The attempt state machine and the typed outcomes of merge-back.

use std::path::PathBuf;
use std::time::Duration;

use crate::error::WorkspaceError;
use crate::workspace::Workspace;

use super::parse::MIN_GIT;

/// Ref namespace holding each committed attempt until it is merged or
/// discarded. It keeps the commit reachable after the worktree is gone and
/// preserves the attempt's side of a conflict.
pub const ATTEMPT_REF_PREFIX: &str = "refs/kgraf/attempts/";

/// Author and committer of merge-back commits. Required, never guessed from
/// ambient config: a commit attributed to whoever happened to be configured
/// is a silent substitution (fail loud).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

/// Work bounds. Checked before anything is staged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// A file larger than this refuses the attempt. Default 100 MiB.
    pub max_file_bytes: u64,
    /// All the files the attempt would stage together larger than this
    /// refuse it. Default 512 MiB. Bounds an attempt made of many files each
    /// under `max_file_bytes` (a build directory the repository does not
    /// ignore, say), which would otherwise be staged whole.
    pub max_total_bytes: u64,
    /// Every git child process is killed after this long. Default 300 s.
    pub git_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: 100 * 1024 * 1024,
            max_total_bytes: 512 * 1024 * 1024,
            git_timeout: Duration::from_secs(300),
        }
    }
}

/// Lifecycle of one worker attempt. Port of container-use's environment
/// lifecycle, made explicit:
///
/// ```text
///   Working --commit--> Committed --merge--> Merged      (worktree removed)
///      |                   |          \----> Conflicted  (worktree + ref kept)
///      |--commit--> Empty    (no changes; worktree removed)
///      |--commit--> Refused  (unsafe content; nothing staged is committed)
///      |                   |                     |          |
///      +------discard------+--------discard------+----------+--> Discarded
/// ```
///
/// Merged, Empty and Discarded are terminal. Conflicted and Refused have one
/// exit, discard, at which point the orchestrator re-dispatches the task from
/// the current integration tip -- for a `Conflicted` attempt, only once a
/// resolver worker has had its own try at a fresh worktree first
/// (decision 0073); `discard` itself is unchanged, just called
/// later. Resolving inside the *old* worktree is still never offered, at any
/// point, because that would be kGraf choosing a resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptState {
    /// The worker may still be writing its worktree.
    Working,
    /// The worktree was committed from outside the sandbox.
    Committed { commit: String },
    /// The worker changed nothing. Nothing to merge.
    Empty,
    /// The worktree holds content merge-back will not commit.
    Refused(Box<Refusal>),
    /// Merged into the integration branch.
    Merged {
        commit: String,
        merge_commit: String,
    },
    /// The merge conflicted; the integration branch did not move.
    Conflicted(Box<MergeConflict>),
    /// Removed without merging (a verifier loser, or a conflict given up).
    Discarded,
}

/// What moved an attempt. Input to [`AttemptState::next`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptEvent {
    Committed { commit: String },
    NoChanges,
    Refused(Box<Refusal>),
    Merged { merge_commit: String },
    Conflicted(Box<MergeConflict>),
    Discarded,
}

impl AttemptState {
    /// The transition function. Pure; every path through merge-back goes
    /// through it, so an illegal move fails the same way everywhere.
    pub fn next(&self, event: AttemptEvent) -> Result<AttemptState, MergeError> {
        use AttemptEvent as E;
        use AttemptState as S;
        match (self, event) {
            (S::Working, E::Committed { commit }) => Ok(S::Committed { commit }),
            (S::Working, E::NoChanges) => Ok(S::Empty),
            (S::Working, E::Refused(r)) => Ok(S::Refused(r)),
            (S::Committed { commit }, E::Merged { merge_commit }) => Ok(S::Merged {
                commit: commit.clone(),
                merge_commit,
            }),
            (S::Committed { .. }, E::Conflicted(conflict)) => Ok(S::Conflicted(conflict)),
            (S::Working | S::Committed { .. } | S::Conflicted(_) | S::Refused(_), E::Discarded) => {
                Ok(S::Discarded)
            }
            (state, event) => Err(MergeError::InvalidTransition {
                from: state.label(),
                event: event.label(),
            }),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Committed { .. } => "committed",
            Self::Empty => "empty",
            Self::Refused(_) => "refused",
            Self::Merged { .. } => "merged",
            Self::Conflicted(_) => "conflicted",
            Self::Discarded => "discarded",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Merged { .. } | Self::Empty | Self::Discarded)
    }
}

impl AttemptEvent {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Committed { .. } => "commit",
            Self::NoChanges => "commit (no changes)",
            Self::Refused(_) => "commit (refused)",
            Self::Merged { .. } => "merge",
            Self::Conflicted(_) => "merge (conflict)",
            Self::Discarded => "discard",
        }
    }
}

/// One worker's attempt: its workspace and where it is in the lifecycle.
/// Created by [`super::MergeBack::attempt`], which checks the name forms a
/// valid ref.
#[derive(Debug)]
pub struct Attempt {
    pub(super) workspace: Workspace,
    pub(super) state: AttemptState,
    /// The verification this attempt commits for, when the caller numbers
    /// them ([`super::MergeBack::attempt_numbered`]).
    pub(super) number: Option<u32>,
    /// Files the worker's run generated that the commit left out
    /// ([`super::MergeBack::commit_retaining`]).
    pub(super) excluded: Vec<super::Excluded>,
    /// Untracked `.gitignore` files present when the snapshot was taken, ignored
    /// or not. One absent from this set after a check was made by the check.
    /// `None` until [`super::MergeBack::commit_retaining`] takes the snapshot.
    pub(super) ignore_baseline: Option<std::collections::BTreeSet<PathBuf>>,
    /// What the commit changed outside its declared write scope: `None` when
    /// it was committed with no scope ([`super::MergeBack::commit`]), which
    /// [`super::MergeBack::merge`] refuses; `Some` of the paths otherwise, and
    /// merge refuses any but an empty list (decision 0083).
    pub(super) outside_scope: Option<Vec<String>>,
}

impl Attempt {
    /// Files the worker's run generated that its commit left out.
    pub fn excluded(&self) -> &[super::Excluded] {
        &self.excluded
    }
    pub fn name(&self) -> &str {
        self.workspace.name()
    }
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }
    pub fn state(&self) -> &AttemptState {
        &self.state
    }
    /// `refs/kgraf/attempts/<workspace name>`, or `<workspace name>+<n>`
    /// for a numbered verification. `+` never appears in a workspace name, so a
    /// numbered ref never equals another workspace's ref.
    pub fn attempt_ref(&self) -> String {
        match self.number {
            Some(n) => format!("{ATTEMPT_REF_PREFIX}{}+{n}", self.workspace.name()),
            None => format!("{ATTEMPT_REF_PREFIX}{}", self.workspace.name()),
        }
    }
    pub(super) fn advance(&mut self, event: AttemptEvent) -> Result<(), MergeError> {
        self.state = self.state.next(event)?;
        Ok(())
    }
}

/// Parallel attempts at one task (verifier mode, disjoint writes). The group owns its
/// attempts, so none of them can be merged on its own;
/// [`super::MergeBack::select`] is the only way out, and it merges one.
#[derive(Debug)]
pub struct AttemptGroup {
    pub(super) task: String,
    pub(super) attempts: Vec<Attempt>,
}

impl AttemptGroup {
    pub fn new(task: impl Into<String>, attempts: Vec<Attempt>) -> Result<Self, MergeError> {
        let task = task.into();
        let mut names: Vec<&str> = attempts.iter().map(Attempt::name).collect();
        names.sort_unstable();
        if names.windows(2).any(|w| w[0] == w[1]) {
            return Err(MergeError::Selection {
                task,
                detail: "two attempts share a workspace name".into(),
            });
        }
        Ok(Self { task, attempts })
    }
    pub fn task(&self) -> &str {
        &self.task
    }
    pub fn attempts(&self) -> &[Attempt] {
        &self.attempts
    }
}

/// Why an attempt was refused before commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusalReason {
    /// The attempt adds, changes or deletes a path that steers git
    /// (`.gitattributes`, `.gitmodules`, a `.git*` directory).
    ProtectedPath { path: String },
    /// A directory holding its own repository would be recorded as a gitlink
    /// to a commit no one can fetch, and its files would vanish.
    NestedRepository { path: String },
    FileTooLarge {
        path: String,
        bytes: u64,
        limit: u64,
    },
    /// A FIFO, socket or device: git cannot record it and would skip it.
    NotARegularFile { path: String, kind: &'static str },
    /// Every file the attempt would stage, together, is over the limit.
    /// `largest` names the biggest few, so the cause is visible.
    TooMuchData {
        bytes: u64,
        limit: u64,
        files: usize,
        largest: Vec<String>,
    },
    /// The attempt adds a symbolic link, retargets one, or turns a file into
    /// one (decision 0083). A link on the integration branch is followed by
    /// every host-side write that later lands in a checkout of it; deleting a
    /// link, or leaving one the base already held, is allowed.
    Symlink { path: String, target: String },
    /// The attempt changes a path outside the writes it declared (decision
    /// 0083). An attempt that declared none may change nothing.
    OutsideDeclaredWrites { path: String },
    /// The attempt leaves two distinct paths that a case- or
    /// normalization-insensitive file system resolves as one (decision 0083).
    PathCollision { paths: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub attempt: String,
    pub reasons: Vec<RefusalReason>,
}

/// One stage of a conflicted path, as `git merge-tree` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictSide {
    pub mode: String,
    pub blob: String,
}

/// A path git could not merge. Every side present in the merge is kept as a
/// blob id, so nothing is lost: stage 1 is the merge base, stage 2 the
/// integration branch, stage 3 the attempt. An absent side means the path
/// was deleted (or never existed) on that side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictedPath {
    pub path: String,
    pub base: Option<ConflictSide>,
    pub integration: Option<ConflictSide>,
    pub attempt: Option<ConflictSide>,
}

/// Git's explanation of one merge event (for example "CONFLICT (contents)").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictMessage {
    pub paths: Vec<String>,
    /// Stable machine label, for example `CONFLICT (modify/delete)` or `Auto-merging`.
    pub kind: String,
    pub message: String,
}

/// A merge that cannot land as is. The integration branch was not moved;
/// the attempt commit is kept under its attempt ref and its worktree is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeConflict {
    pub attempt: String,
    pub integration_branch: String,
    /// Integration tip the merge was computed against.
    pub integration_tip: String,
    /// The attempt's commit (also at `refs/kgraf/attempts/<attempt>`).
    pub attempt_commit: String,
    /// Tree git wrote (with conflict markers, if any), for display.
    /// Unreferenced: it lives until git's object pruning.
    pub conflicted_tree: String,
    /// Paths git could not merge.
    pub paths: Vec<ConflictedPath>,
    /// Paths git merged cleanly that differ only by case or Unicode
    /// normalization, so one overwrites the other on a case- or
    /// normalization-insensitive file system. Only groups the merge creates.
    pub collisions: Vec<Vec<String>>,
    pub messages: Vec<ConflictMessage>,
}

/// Result of committing a worker's worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    Committed {
        commit: String,
    },
    /// Nothing changed. [`super::MergeBack::commit`] removed the workspace;
    /// [`super::MergeBack::commit_retaining`] kept it.
    Empty,
    /// Content merge-back will not commit; the workspace is kept.
    Refused(Box<Refusal>),
}

/// Result of merging one committed attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged {
        merge_commit: String,
        integration_before: String,
    },
    Conflicted(Box<MergeConflict>),
}

/// Result of [`super::MergeBack::finish`]: commit then merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishOutcome {
    /// The worker changed nothing; its workspace was removed.
    Empty,
    Refused(Box<Refusal>),
    Merged {
        merge_commit: String,
        integration_before: String,
    },
    Conflicted(Box<MergeConflict>),
}

impl From<MergeOutcome> for FinishOutcome {
    fn from(outcome: MergeOutcome) -> Self {
        match outcome {
            MergeOutcome::Merged {
                merge_commit,
                integration_before,
            } => Self::Merged {
                merge_commit,
                integration_before,
            },
            MergeOutcome::Conflicted(c) => Self::Conflicted(c),
        }
    }
}

/// A failed [`super::MergeBack::select`]. Every attempt not yet discarded
/// comes back, so none is lost with its worktree still on disk.
#[derive(Debug)]
pub struct SelectFailure {
    pub error: MergeError,
    pub attempts: Vec<Attempt>,
}

impl std::fmt::Display for SelectFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({} attempts returned undiscarded)",
            self.error,
            self.attempts.len()
        )
    }
}

impl std::error::Error for SelectFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// A merge-back failure. A conflict or a refusal is not an error: each is a
/// typed outcome.
#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("illegal attempt transition: {event} from state {from}")]
    InvalidTransition {
        from: &'static str,
        event: &'static str,
    },
    #[error("git {found} is too old: merge-back needs {}.{} or later", MIN_GIT.0, MIN_GIT.1)]
    GitTooOld { found: String },
    #[error("git {args} did not finish within {after:?}; killed")]
    GitTimeout { args: String, after: Duration },
    #[error("integration branch {branch} does not exist")]
    NoIntegrationBranch { branch: String },
    /// Moving a checked-out branch under a working tree would leave that tree
    /// silently dirty, so it is refused.
    #[error(
        "integration branch {branch} is checked out at {worktree}; use a branch no worktree has checked out"
    )]
    IntegrationCheckedOut { branch: String, worktree: PathBuf },
    /// The commit identity is empty or names Claude or Anthropic.
    #[error("commit identity refused: {0}")]
    IdentityRefused(String),
    #[error("attempt name {name:?} does not form a valid ref: {detail}")]
    InvalidAttemptName { name: String, detail: String },
    #[error("workspace {name} has no usable recorded gitlink")]
    NoAdminDir { name: String },
    /// The worktree HEAD is not where the attempt started. Workers never
    /// commit inside the sandbox, so this is tampering or a lifecycle bug.
    #[error("workspace {name} HEAD moved from {expected} to {found}")]
    HeadMoved {
        name: String,
        expected: String,
        found: String,
    },
    #[error("attempt ref {0} already exists; an attempt is committed once")]
    AttemptRefExists(String),
    #[error("integration branch {branch} kept moving during merge; gave up after {tries} tries")]
    Contended { branch: String, tries: usize },
    #[error("attempt group {task}: {detail}")]
    Selection { task: String, detail: String },
    /// The merge landed; removing the worktree or attempt ref afterward failed.
    #[error("merged as {merge_commit}, but cleanup failed: {source}")]
    CleanupFailed {
        merge_commit: String,
        #[source]
        source: Box<MergeError>,
    },
    #[error("merge-back state lock poisoned")]
    Poisoned,
    /// A command run after the commit left changes that could not be removed
    /// from the worktree. The commit is intact; the worktree is not clean.
    #[error("workspace {name} still holds changes after the cleanup: {remaining:?}")]
    LeftoversRemain {
        name: String,
        remaining: Vec<PathBuf>,
    },
    /// Leftovers are measured against a snapshot only
    /// [`super::MergeBack::commit_retaining`] takes.
    #[error("workspace {name} was not committed with commit_retaining; no snapshot to clean to")]
    NoSnapshot { name: String },
    /// Only an attempt committed against a declared write scope can be
    /// merged (decision 0083): [`super::MergeBack::commit`] records none.
    #[error(
        "attempt {name} was committed without a declared write scope, so it cannot be \
         merged; commit it with commit_declared or commit_retaining"
    )]
    Unscoped { name: String },
    /// The attempt's commit changes paths outside the writes it declared
    /// (decision 0083); the branch did not move.
    #[error("attempt {name} changes paths outside its declared writes: {paths:?}; not merged")]
    OutsideDeclaredWrites { name: String, paths: Vec<String> },
}

/// What a command run in an attempt's worktree after [`super::MergeBack::commit_retaining`]
/// left there, compared with the snapshot that commit took. Paths are relative
/// to the worktree root, as git names them, byte for byte. Ignored paths
/// are not listed (git would never stage them), except a `.gitignore` the
/// command created, which is listed and removed before anything else so it
/// cannot hide what the command wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Leftovers {
    /// Paths not in the snapshot (a generated `Cargo.lock`, an unignored
    /// `__pycache__`, a new `.gitignore`). A directory git will not enter (a
    /// nested repository) ends in `/`.
    pub created: Vec<PathBuf>,
    /// Files of the snapshot the command changed or deleted.
    pub changed: Vec<PathBuf>,
}

impl Leftovers {
    pub fn is_empty(&self) -> bool {
        self.created.is_empty() && self.changed.is_empty()
    }

    /// Every path, created then changed.
    pub fn paths(&self) -> impl Iterator<Item = &std::path::Path> {
        self.created
            .iter()
            .chain(self.changed.iter())
            .map(PathBuf::as_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict() -> Box<MergeConflict> {
        Box::new(MergeConflict {
            attempt: "a".into(),
            integration_branch: "i".into(),
            integration_tip: "t".into(),
            attempt_commit: "c".into(),
            conflicted_tree: "x".into(),
            paths: vec![],
            collisions: vec![],
            messages: vec![],
        })
    }

    fn refusal() -> Box<Refusal> {
        Box::new(Refusal {
            attempt: "a".into(),
            reasons: vec![],
        })
    }

    #[test]
    fn transitions_follow_the_diagram() {
        let committed = AttemptState::Working
            .next(AttemptEvent::Committed { commit: "c".into() })
            .unwrap();
        let merged = committed
            .next(AttemptEvent::Merged {
                merge_commit: "m".into(),
            })
            .unwrap();
        assert!(merged.is_terminal());
        let conflicted = committed
            .next(AttemptEvent::Conflicted(conflict()))
            .unwrap();
        assert_eq!(
            conflicted.next(AttemptEvent::Discarded).unwrap(),
            AttemptState::Discarded
        );
        let refused = AttemptState::Working
            .next(AttemptEvent::Refused(refusal()))
            .unwrap();
        assert!(!refused.is_terminal());
        assert_eq!(
            refused.next(AttemptEvent::Discarded).unwrap(),
            AttemptState::Discarded
        );
        assert_eq!(
            AttemptState::Working.next(AttemptEvent::NoChanges).unwrap(),
            AttemptState::Empty
        );
    }

    #[test]
    fn illegal_transitions_are_refused() {
        let merged = AttemptState::Merged {
            commit: "c".into(),
            merge_commit: "m".into(),
        };
        let merge = || AttemptEvent::Merged {
            merge_commit: "m".into(),
        };
        let cases = [
            (AttemptState::Working, merge()),
            (merged.clone(), AttemptEvent::Discarded),
            (merged, merge()),
            (AttemptState::Empty, AttemptEvent::Discarded),
            (AttemptState::Discarded, AttemptEvent::NoChanges),
            (AttemptState::Conflicted(conflict()), merge()),
            (AttemptState::Refused(refusal()), merge()),
            (
                AttemptState::Refused(refusal()),
                AttemptEvent::Committed { commit: "c".into() },
            ),
            (
                AttemptState::Committed { commit: "c".into() },
                AttemptEvent::Committed { commit: "d".into() },
            ),
            (
                AttemptState::Committed { commit: "c".into() },
                AttemptEvent::Refused(refusal()),
            ),
        ];
        for (state, event) in cases {
            let label = state.label();
            let err = state.next(event).unwrap_err();
            assert!(
                matches!(err, MergeError::InvalidTransition { from, .. } if from == label),
                "{err}"
            );
        }
    }
}
