//! Whether a commit kGraf makes on the user's behalf is signed, from the
//! user's own configuration.
//!
//! A commit that lands on the user's branch is the user's commit, so it
//! follows the user's `commit.gpgSign`: when it is on, kGraf passes `-S`
//! (with `user.signingKey` when set) to `git commit-tree`; git reads
//! `gpg.format` and `gpg.program` itself. Nothing prompts
//! (`GIT_TERMINAL_PROMPT=0`); when signing fails, the commit is refused with
//! git's own message and no ref moves. kGraf never makes an unsigned commit
//! on the user's branch when the user asked for signed ones.
//!
//! kGraf's internal commits (attempts, speculative merges, the merge-back
//! commits on `kgraf/<run>`) stay unsigned (`--no-gpg-sign`): they are kGraf's
//! working state, not commits the user lands.
//!
//! The settings are read with the user's own git ([`crate::user_git`]),
//! because kGraf's host runner overrides `commit.gpgSign` to off.

use std::path::Path;

use crate::error::WorkspaceError;
use crate::user_git::UserGit;

/// How a user-facing commit is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitSigning {
    /// `commit.gpgSign` is off or unset.
    Unsigned,
    /// `commit.gpgSign` is on. `key` is `user.signingKey` when set; `format`
    /// is `gpg.format` when set (git reads it itself; kept for the notice).
    Signed {
        key: Option<String>,
        format: Option<String>,
    },
}

impl CommitSigning {
    /// The `git commit-tree` (or `git commit`) flag for this choice.
    pub fn commit_flag(&self) -> String {
        match self {
            CommitSigning::Unsigned => "--no-gpg-sign".to_owned(),
            CommitSigning::Signed { key: Some(key), .. } => format!("-S{key}"),
            CommitSigning::Signed { key: None, .. } => "-S".to_owned(),
        }
    }

    pub fn is_signed(&self) -> bool {
        matches!(self, CommitSigning::Signed { .. })
    }

    /// One line for the user: how the commit is made.
    pub fn describe(&self) -> String {
        match self {
            CommitSigning::Unsigned => "unsigned (commit.gpgSign is off)".to_owned(),
            CommitSigning::Signed { key, format } => format!(
                "signed ({}{})",
                format.as_deref().unwrap_or("openpgp"),
                key.as_deref()
                    .map(|k| format!(", key {k}"))
                    .unwrap_or_default()
            ),
        }
    }
}

/// The user's signing choice for commits in the repository at `repo`.
/// An unreadable or malformed setting fails loud rather than reading as off.
pub fn user_commit_signing(git: &UserGit, repo: &Path) -> Result<CommitSigning, WorkspaceError> {
    let read = |args: &[&str]| -> Result<Option<String>, WorkspaceError> {
        let out = git.exec(repo, args)?;
        match out.code {
            Some(0) => Ok(Some(out.stdout_str())),
            // Exit 1: the key is not set.
            Some(1) => Ok(None),
            code => Err(WorkspaceError::GitFailed {
                args: args.join(" "),
                cwd: repo.to_path_buf(),
                code,
                stderr: out.stderr,
            }),
        }
    };
    let on = read(&["config", "--type=bool", "--get", "commit.gpgSign"])?;
    if on.as_deref() != Some("true") {
        return Ok(CommitSigning::Unsigned);
    }
    let key = read(&["config", "--get", "user.signingKey"])?.filter(|k| !k.is_empty());
    let format = read(&["config", "--get", "gpg.format"])?.filter(|f| !f.is_empty());
    Ok(CommitSigning::Signed { key, format })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, UserGit) {
        let dir = tempfile::tempdir().expect("tmp");
        let git = UserGit::from_git(&crate::git::Git::discover().expect("git"));
        assert!(
            git.exec(dir.path(), ["init", "-q"])
                .expect("init")
                .success()
        );
        (dir, git)
    }

    fn set(git: &UserGit, dir: &Path, key: &str, value: &str) {
        assert!(
            git.exec(dir, ["config", key, value])
                .expect("config")
                .success()
        );
    }

    #[test]
    fn off_is_unsigned() {
        let (dir, git) = repo();
        // The repository's setting outranks a global one on the test machine.
        set(&git, dir.path(), "commit.gpgSign", "false");
        let signing = user_commit_signing(&git, dir.path()).expect("read");
        assert_eq!(signing, CommitSigning::Unsigned);
        assert_eq!(signing.commit_flag(), "--no-gpg-sign");
    }

    #[test]
    fn on_reads_the_key_and_format() {
        let (dir, git) = repo();
        set(&git, dir.path(), "commit.gpgSign", "yes");
        set(&git, dir.path(), "user.signingKey", "ABC123");
        set(&git, dir.path(), "gpg.format", "ssh");
        let signing = user_commit_signing(&git, dir.path()).expect("read");
        assert_eq!(
            signing,
            CommitSigning::Signed {
                key: Some("ABC123".into()),
                format: Some("ssh".into())
            }
        );
        assert_eq!(signing.commit_flag(), "-SABC123");
    }

    #[test]
    fn a_malformed_setting_fails_loud() {
        let (dir, git) = repo();
        set(&git, dir.path(), "commit.gpgSign", "perhaps");
        let err = user_commit_signing(&git, dir.path()).expect_err("refused");
        assert!(err.to_string().contains("commit.gpgSign"), "{err}");
    }
}
