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
pub mod dependencies;
pub mod egress;
mod error;
pub mod git;
pub mod git_for_windows;
pub mod gradle_wrapper;
pub mod lifecycle;
pub mod merge;
pub mod output_tail;
mod package_stores;
/// The repository lock lives in `kgraf-core` so every crate that runs git on
/// a repository (the worker's status read, the co-change history read) takes
/// it without depending on this crate; re-exported under its old path.
pub use kgraf_core::repo_lock;
mod sandbox;
pub mod sandbox_user;
#[cfg(unix)]
pub mod trust;
mod win_command_line;
#[cfg(windows)]
mod win_process;
mod workspace;

/// The name of the Python interpreter the platform installs: Windows installs
/// `python` (`python3` there is at most a Store stub), the others `python3`.
/// Every command kGraf writes for Python uses this name.
pub fn python_program() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

pub use build_dir::{
    SeedError, SeedMethod, SeedReport, clone_target_dir, seed_target_dir, seed_workspace_target,
};
pub use confined::{open_append_confined, remove_confined, write_confined, write_confined_mode};
pub use dependencies::{
    DependencyKind, DependencyLinks, DependencyTree, MIRROR_MARK, Names, VirtualEnv,
    find_dependency_trees, is_dependency_clone, link_dependency_trees, script_start_dir,
    walk_project_dirs,
};
pub use error::{SandboxError, WorkspaceError};
pub use merge::{
    ATTEMPT_REF_PREFIX, Attempt, AttemptEvent, AttemptGroup, AttemptState, CommitOutcome,
    ConflictMessage, ConflictSide, ConflictedPath, Excluded, FinishOutcome, GeneratedKind,
    Identity, LOCKFILES, Leftovers, Limits, MergeBack, MergeConflict, MergeError, MergeOutcome,
    MergedWorktree, Refusal, RefusalReason, SelectFailure, TreeManifests, TrialMerge, file_head,
    generated_kind, is_compiled_binary, is_dependency_link, is_python_bytecode, lockfile_manifests,
    manifest_matches, manifests_beside,
};
pub use package_stores::{
    PACKAGE_STORES_DIR, PackageStores, STORE_COMPLETE, jvm_path, layout as store_layout,
    mark_store_complete, package_stores_root,
};
#[cfg(windows)]
pub use sandbox::SandboxedChild;
#[cfg(unix)]
pub use sandbox::SetupProbe;
pub use sandbox::{
    Backend, CheckNetwork, FROZEN_GIT_FILES, FrozenGit, IsolationStrength, NETWORK_OPEN_NOTICE,
    Network, ReadConfinement, Redirect, SHELL_NETWORK_OPEN_NOTICE, SandboxPolicy, SandboxedCommand,
    TLS_UNAVAILABLE_HINT, WINDOWS_BASE_VARS, WorkerSlot, is_git_config_entry_var,
    platform_isolation, sandboxed_command, tls_unavailable_hint, worker_slot, workspace_command,
};
#[cfg(target_os = "linux")]
pub use sandbox::{LinuxConfinement, linux_confinement};
pub use win_command_line::windows_command_line;
pub use workspace::{
    CheckFolder, HostId, LEGACY_LOCK_TAG, LOCK_TAG, Liveness, OrphanCause, OrphanRemoval,
    ProcessIdentity, QuietGuard, ReopenSpec, Workspace, WorkspaceManager, WorkspaceSpec,
    lock_owner_may_be_running, pid_is_gone,
};
