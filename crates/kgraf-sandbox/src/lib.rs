//! Sandboxed execution: a worktree per worker and per-platform isolation.
//!
//! - [`WorkspaceManager`] owns the git-worktree lifecycle through the git CLI
//!   (create locked, prune on startup, clean orphans, remove).
//! - [`MergeBack`] commits each finished worktree from outside the sandbox
//!   and merges it into the integration branch as the worker finishes
//!   (decision 0018); a conflict is a typed [`MergeConflict`].
//! - [`seed_target_dir`] gives each worktree its own build directory, seeded
//!   by a copy-on-write clone of a warm one (decision 0060).
//! - [`sandboxed_command`] builds a command confined to a policy: Seatbelt on
//!   macOS, Landlock plus seccomp and namespaces on Linux (Landlock-only,
//!   partial and opt-in, where seccomp is missing), a restricted token with a job
//!   object (partial, opt-in) on Windows, an error elsewhere.

mod build_dir;
mod confined;
mod error;
pub mod git;
pub mod lifecycle;
pub mod merge;
mod sandbox;
#[cfg(unix)]
pub mod trust;
mod workspace;

pub use build_dir::{SeedError, SeedMethod, SeedReport, seed_target_dir, seed_workspace_target};
pub use confined::{open_append_confined, remove_confined, write_confined, write_confined_mode};
pub use error::{SandboxError, WorkspaceError};
pub use merge::{
    ATTEMPT_REF_PREFIX, Attempt, AttemptEvent, AttemptGroup, AttemptState, CommitOutcome,
    ConflictMessage, ConflictSide, ConflictedPath, Excluded, FinishOutcome, GeneratedKind,
    Identity, LOCKFILES, Leftovers, Limits, MergeBack, MergeConflict, MergeError, MergeOutcome,
    MergedWorktree, Refusal, RefusalReason, SelectFailure, TrialMerge, is_python_bytecode,
    lockfile_manifests,
};
#[cfg(windows)]
pub use sandbox::SandboxedChild;
#[cfg(unix)]
pub use sandbox::SetupProbe;
pub use sandbox::{
    Backend, IsolationStrength, Network, ReadConfinement, Redirect, SandboxPolicy,
    SandboxedCommand, platform_isolation, sandboxed_command, workspace_command,
};
#[cfg(target_os = "linux")]
pub use sandbox::{LinuxConfinement, linux_confinement};
pub use workspace::{
    HostId, LEGACY_LOCK_TAG, LOCK_TAG, Liveness, OrphanCause, OrphanRemoval, ProcessIdentity,
    QuietGuard, ReopenSpec, Workspace, WorkspaceManager, WorkspaceSpec, lock_owner_may_be_running,
};
