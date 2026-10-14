//! The user's own git: a plain `git` process that honors the user's hooks,
//! filters, drivers and configuration, as the user's own `git` command would.
//!
//! kGraf's own git runs through the host runner (`kgraf_core::host_git`),
//! which turns hooks and every configured filter and driver off, so nothing a
//! worker could write reaches a program on the host. Two steps of a land are
//! the user's own git operations and run here instead:
//!
//! - reading the user's commit-signing settings ([`crate::signing`]), which
//!   the host runner overrides (`commit.gpgSign=false`), so it cannot read
//!   them;
//! - moving a checked-out target branch with `git merge --ff-only` in the
//!   user's worktree, so the user's `post-merge` hook runs and a path with a
//!   clean or smudge filter is checked out filtered, exactly as after the
//!   user's own merge.
//!
//! What is still removed: the inherited variables that redirect git away
//! from the directory it is run in (`GIT_DIR`, `GIT_WORK_TREE`, ...; set
//! inside a git hook) and inherited `-c` style overrides (`GIT_CONFIG_COUNT`,
//! `GIT_CONFIG_PARAMETERS`), which are a caller's settings, not the user's.
//! `GIT_TERMINAL_PROMPT=0` is set so nothing waits on a terminal prompt.
//! Each process is bounded by [`kgraf_core::host_git::COMMAND_TIMEOUT`].

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::error::WorkspaceError;
use crate::git::GIT_REDIRECT_ENV;

/// The result of one user git process.
#[derive(Debug, Clone)]
pub struct UserGitOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl UserGitOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Standard output, trimmed, lossily decoded.
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

/// Runs the user's own git (see the module documentation).
#[derive(Debug, Clone)]
pub struct UserGit {
    exe: PathBuf,
    timeout: Duration,
}

impl UserGit {
    /// The git executable at `exe`.
    pub fn at(exe: PathBuf) -> Self {
        Self {
            exe,
            timeout: kgraf_core::host_git::COMMAND_TIMEOUT,
        }
    }

    /// The same executable with another bound on each command.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The same git kGraf's own runner resolved.
    pub fn from_git(git: &crate::git::Git) -> Self {
        Self::at(git.exe().to_path_buf())
    }

    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// Run `git <args>` in `cwd`. A non-zero exit is returned, not an error:
    /// the caller decides what a refusal means.
    pub fn exec<I, S>(&self, cwd: &Path, args: I) -> Result<UserGitOutput, WorkspaceError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let shown = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        // Shared repository lock, as every kGraf git process on a repository
        // takes (decision 0094): never inside a Bypass write window.
        let _repo = crate::repo_lock::shared_for(cwd, &format!("user git {shown}"))?;
        let mut cmd = Command::new(&self.exe);
        cmd.current_dir(cwd).args(&args);
        for var in GIT_REDIRECT_ENV {
            cmd.env_remove(var);
        }
        for key in caller_overrides(std::env::vars_os().map(|(k, _)| k)) {
            cmd.env_remove(key);
        }
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        let out = kgraf_core::child_run::run(
            cmd,
            &format!("git {shown}"),
            None,
            kgraf_core::child_run::Limits::new(self.timeout),
        )
        .map_err(|e| match e {
            kgraf_core::child_run::RunError::Spawn { source, .. } => {
                WorkspaceError::io(&self.exe, source)
            }
            other => WorkspaceError::Run(other),
        })?;
        Ok(UserGitOutput {
            code: out.status.code(),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// The inherited variables that carry a caller's `-c` style settings.
fn caller_overrides(keys: impl Iterator<Item = OsString>) -> Vec<OsString> {
    keys.filter(|key| {
        let k = key.to_string_lossy();
        k == "GIT_CONFIG_COUNT"
            || k == "GIT_CONFIG_PARAMETERS"
            || k.starts_with("GIT_CONFIG_KEY_")
            || k.starts_with("GIT_CONFIG_VALUE_")
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_config_overrides_are_named_for_removal() {
        let keys = [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_GLOBAL",
            "HOME",
        ]
        .map(OsString::from);
        let removed = caller_overrides(keys.into_iter());
        assert_eq!(
            removed,
            [
                "GIT_CONFIG_COUNT",
                "GIT_CONFIG_KEY_0",
                "GIT_CONFIG_VALUE_0",
                "GIT_CONFIG_PARAMETERS"
            ]
            .map(OsString::from)
            .to_vec(),
            "the user's config file location is kept"
        );
    }

    #[test]
    fn a_failing_command_is_returned_not_raised() {
        let dir = tempfile::tempdir().expect("tmp");
        let user = UserGit::from_git(&crate::git::Git::discover().expect("git"));
        assert!(
            user.exec(dir.path(), ["init", "-q"])
                .expect("init")
                .success()
        );
        let out = user
            .exec(dir.path(), ["config", "--get", "user.kgraf-unset"])
            .expect("run");
        assert_eq!(out.code, Some(1));
    }
}
