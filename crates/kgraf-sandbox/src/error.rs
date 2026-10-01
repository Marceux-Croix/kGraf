//! Error types for workspaces and the isolation runner.

use std::path::PathBuf;

/// A git CLI or workspace lifecycle failure.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// The `git` binary is not on `PATH`. Worktrees go through the git CLI
    /// (sandboxing evidence), so there is no fallback.
    #[error("git executable not found on PATH; worktree lifecycle requires the git CLI")]
    GitNotFound,
    /// A git command exited non-zero.
    #[error("git {args} failed in {cwd} (exit {code:?}): {stderr}")]
    GitFailed {
        args: String,
        cwd: PathBuf,
        code: Option<i32>,
        stderr: String,
    },
    /// Git printed output this crate cannot parse.
    #[error("unparseable git output from `git {args}`: {detail}")]
    GitOutput { args: String, detail: String },
    /// The path given as the repository is not inside a git work tree.
    #[error("{0} is not a git repository")]
    NotARepository(PathBuf),
    /// A workspace name contains characters that are unsafe in a path or ref.
    #[error("invalid workspace name {0:?}: use ASCII letters, digits, '-', '_' or '.'")]
    InvalidName(String),
    /// The base directory could hold user data that orphan cleanup would delete.
    #[error("unsafe workspace base directory {path}: {reason}")]
    UnsafeBaseDir { path: PathBuf, reason: String },
    /// A live, registered workspace already occupies the path.
    #[error("workspace {0} already exists and is registered")]
    AlreadyExists(PathBuf),
    /// A registered worktree carries a lock kGraf wrote, but its owner cannot
    /// be read (the reason was cut short or damaged), so nothing can tell
    /// whether a process still uses it. Nothing was changed; a person
    /// removes it with `kgraf clean`.
    #[error(
        "workspace {path} carries a kGraf lock whose owner cannot be read ({lock:?}), so \
         nothing can tell whether a process still uses it; check, then run `kgraf clean {name}`"
    )]
    UnreadableLock {
        path: PathBuf,
        name: String,
        lock: String,
    },
    /// Git reported success but the directory is absent.
    #[error("git reported success but workspace path {0} does not exist")]
    MissingAfterCreate(PathBuf),
    /// Removal finished but state is still present.
    #[error("workspace {path} still present after removal: {detail}")]
    RemovalIncomplete { path: PathBuf, detail: String },
    /// The workspace `.git` gitlink no longer points at its admin directory.
    #[error("workspace {path} gitlink was altered: expected {expected}, found {found}")]
    GitlinkTampered {
        path: PathBuf,
        expected: String,
        found: String,
    },
    /// A worktree left by an earlier process cannot be reopened as recorded:
    /// it is gone, held by a live owner, its gitlink or HEAD is not what was
    /// recorded, or its admin directory does not point back at it. The caller
    /// starts over; nothing was changed.
    #[error("workspace {path} cannot be reopened: {reason}")]
    NotReopenable { path: PathBuf, reason: String },
    /// A process still uses the worktree: its owner runs here, or a process
    /// started in it (the hold is taken, or a logged session has a live
    /// member). Temporary: the same worktree can be reopened once it ends, so
    /// a caller waits rather than replacing the worktree and its work.
    #[error("workspace {path} is in use: {reason}")]
    Busy { path: PathBuf, reason: String },
    /// The worktree's merge-back commit is already on the integration branch:
    /// the attempt was merged before its process died. The worktree and its
    /// attempt refs were retired; there is nothing to continue.
    #[error("workspace {path} was already merged (commit {commit}); retired, not reopened")]
    AlreadyMerged { path: PathBuf, commit: String },
    /// A per-path guard was poisoned by a panic in another thread.
    #[error("workspace guard poisoned for {0}")]
    GuardPoisoned(PathBuf),
    /// The repository lock that serializes kGraf's git processes against a
    /// worker's Bypass git window was not taken in time (decision 0094).
    #[error(transparent)]
    RepoLock(#[from] crate::repo_lock::RepoLockError),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl WorkspaceError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

impl kgraf_diag::DiagCode for WorkspaceError {
    fn diag_code(&self) -> &'static str {
        match self {
            WorkspaceError::Busy { .. } => "KG-0601",
            WorkspaceError::GitFailed { .. }
            | WorkspaceError::GitOutput { .. }
            | WorkspaceError::GitNotFound
            | WorkspaceError::NotARepository(_) => "KG-0603",
            _ => kgraf_diag::catch_all_for("kgraf-sandbox").code,
        }
    }

    fn diag_component(&self) -> &'static str {
        "kgraf-sandbox"
    }
}

/// An isolation runner failure. Every variant means the command was not
/// started; the runner never falls back to running unconfined (fail loud).
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// No isolation backend exists for this platform yet.
    #[error(
        "sandboxed execution is not available on {platform}: {reason}; refusing to run unconfined"
    )]
    Unsupported {
        platform: &'static str,
        reason: &'static str,
    },
    /// The policy asks for something the platform backend cannot enforce.
    #[error("{backend} cannot enforce {what}; refusing to run with a weaker policy")]
    PolicyNotEnforceable {
        backend: &'static str,
        what: &'static str,
    },
    /// The policy has no writable root or a root is not an absolute, existing path.
    #[error("invalid sandbox policy: {0}")]
    InvalidPolicy(String),
    /// The platform sandbox binary is missing (macOS: /usr/bin/sandbox-exec).
    #[error("sandbox launcher {0} not found")]
    LauncherMissing(PathBuf),
    /// The kernel lacks the required isolation feature (Linux: Landlock ABI).
    #[error("kernel isolation unavailable: {0}; refusing to run unconfined")]
    KernelUnsupported(String),
    /// The backend confines only part of what the policy implies, and the
    /// policy did not opt in to that (see `SandboxPolicy::accept_partial_isolation`).
    #[error("{backend} isolation is partial: {gaps}; refusing without explicit opt-in")]
    PartialIsolation {
        backend: &'static str,
        gaps: &'static str,
    },
    /// Sandbox setup failed in the child between fork and exec; the command
    /// did not run. The reason comes from the child through a pipe.
    #[error("sandbox setup failed in the child: {reason} ({source})")]
    ChildSetup {
        reason: String,
        #[source]
        source: std::io::Error,
    },
    /// A platform call that builds the sandbox failed (Windows: token, job
    /// object, desktop, ACL verification). Nothing was started.
    #[error("sandbox setup failed at {step}: {source}; refusing to run unconfined")]
    PlatformSetup {
        step: &'static str,
        #[source]
        source: std::io::Error,
    },
    /// Spawning failed for a reason other than sandbox setup.
    #[error("spawn failed: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl kgraf_diag::DiagCode for SandboxError {
    fn diag_code(&self) -> &'static str {
        match self {
            SandboxError::KernelUnsupported(_) | SandboxError::Unsupported { .. } => "KG-0602",
            _ => kgraf_diag::catch_all_for("kgraf-sandbox").code,
        }
    }

    fn diag_component(&self) -> &'static str {
        "kgraf-sandbox"
    }
}

#[cfg(test)]
mod diag_code_tests {
    use super::*;
    use kgraf_diag::DiagCode;

    /// Every `diag_code` branch, on both `WorkspaceError` and `SandboxError`,
    /// resolves to a real glossary entry. One instance per named branch, plus one unnamed variant per
    /// type for the catch-all.
    #[test]
    fn every_diag_code_branch_names_a_real_glossary_entry() {
        let workspace: Vec<WorkspaceError> = vec![
            WorkspaceError::Busy {
                path: "x".into(),
                reason: "x".into(),
            },
            WorkspaceError::GitFailed {
                args: "x".into(),
                cwd: "x".into(),
                code: None,
                stderr: "x".into(),
            },
            WorkspaceError::GitOutput {
                args: "x".into(),
                detail: "x".into(),
            },
            WorkspaceError::GitNotFound,
            WorkspaceError::NotARepository("x".into()),
            // Not named in `diag_code`'s match: exercises the catch-all arm.
            WorkspaceError::GuardPoisoned("x".into()),
        ];
        for e in &workspace {
            let code = e.diag_code();
            assert!(
                kgraf_diag::GLOSSARY.iter().any(|spec| spec.code == code),
                "{e:?} names code {code}, which is not in kgraf_diag::GLOSSARY"
            );
        }

        let sandbox: Vec<SandboxError> = vec![
            SandboxError::KernelUnsupported("x".into()),
            SandboxError::Unsupported {
                platform: "x",
                reason: "x",
            },
            // Not named in `diag_code`'s match: exercises the catch-all arm.
            SandboxError::InvalidPolicy("x".into()),
        ];
        for e in &sandbox {
            let code = e.diag_code();
            assert!(
                kgraf_diag::GLOSSARY.iter().any(|spec| spec.code == code),
                "{e:?} names code {code}, which is not in kgraf_diag::GLOSSARY"
            );
        }
    }
}
