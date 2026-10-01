//! Thin git CLI runner and `git worktree list --porcelain -z` parser.
//!
//! Ported in part from vibe-kanban `crates/git/src/cli.rs` (`worktree_add`,
//! `worktree_remove`, `worktree_prune`), commit
//! d5cbb5380fa0b32e98ef9b8d987f63decce4be3a, Apache-2.0. See
//! `third_party/vibe-kanban/UPSTREAM`.
//!
//! Why the CLI and not git2: git2 worktree validation reports success for a missing
//! directory and has no lock-reason equivalent.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::WorkspaceError;

/// Environment variables that redirect git away from `-C <dir>`. They are set
/// inside git hooks (this repository's own hooks run the test suite), so an
/// inherited value would point every command at the wrong repository.
pub const GIT_REDIRECT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PREFIX",
];

/// Resolved git executable.
#[derive(Debug, Clone)]
pub struct Git {
    exe: PathBuf,
}

impl Git {
    /// Find `git` on `PATH`. Fails loud when absent.
    pub fn discover() -> Result<Self, WorkspaceError> {
        let path = std::env::var_os("PATH").ok_or(WorkspaceError::GitNotFound)?;
        let name = if cfg!(windows) { "git.exe" } else { "git" };
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
            .map(|exe| Self { exe })
            .ok_or(WorkspaceError::GitNotFound)
    }

    /// The resolved executable. `merge` builds its own hardened invocations
    /// (explicit `--git-dir`, hooks and fsmonitor off) around it.
    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// Use a specific git executable (for example a logging wrapper in tests).
    pub fn at(exe: PathBuf) -> Self {
        Self { exe }
    }

    /// Run `git -C <cwd> <args>` and return stdout on success.
    pub fn run<I, S>(&self, cwd: &Path, args: I) -> Result<Vec<u8>, WorkspaceError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_stdin(cwd, args, &[])
    }

    /// [`Self::run`] with `input` written to git's standard input, for a
    /// command that reads requests from it (`cat-file --batch`). The input is
    /// written from a second thread, so a large reply cannot deadlock against
    /// an unread pipe.
    pub fn run_with_stdin<I, S>(
        &self,
        cwd: &Path,
        args: I,
        input: &[u8],
    ) -> Result<Vec<u8>, WorkspaceError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        use std::io::Write;
        use std::process::Stdio;
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let mut cmd = Command::new(&self.exe);
        cmd.arg("-C").arg(cwd).args(&args);
        for var in GIT_REDIRECT_ENV {
            cmd.env_remove(var);
        }
        // Never block on a credential prompt; keep messages in one language
        // so callers can match the "is not a working tree" case if needed.
        cmd.env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C");
        cmd.stdin(if input.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| WorkspaceError::io(&self.exe, e))?;
        let writer = child.stdin.take();
        let output = std::thread::scope(|scope| {
            if let Some(mut stdin) = writer {
                // A write error is git having exited early; its status says why.
                scope.spawn(move || {
                    let _ = stdin.write_all(input);
                });
            }
            child.wait_with_output()
        })
        .map_err(|e| WorkspaceError::io(&self.exe, e))?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(WorkspaceError::GitFailed {
                args: display_args(&args),
                cwd: cwd.to_path_buf(),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }

    /// Run and return trimmed UTF-8 stdout.
    pub fn run_str<I, S>(&self, cwd: &Path, args: I) -> Result<String, WorkspaceError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let out = self.run(cwd, &args)?;
        String::from_utf8(out)
            .map(|s| s.trim().to_string())
            .map_err(|e| WorkspaceError::GitOutput {
                args: display_args(&args),
                detail: e.to_string(),
            })
    }

    /// `git worktree list --porcelain -z` for the repository at `repo`.
    pub fn worktree_list(&self, repo: &Path) -> Result<Vec<WorktreeEntry>, WorkspaceError> {
        let args = ["worktree", "list", "--porcelain", "-z"];
        let out = self.run(repo, args)?;
        parse_porcelain_z(&out).map_err(|detail| WorkspaceError::GitOutput {
            args: args.join(" "),
            detail,
        })
    }
}

fn display_args(args: &[OsString]) -> String {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// One record of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    /// Full ref name (`refs/heads/...`), `None` when detached or bare.
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    /// `Some(reason)` when locked; the reason is empty when none was given.
    pub locked: Option<String>,
    /// `Some(reason)` when git considers the entry prunable.
    pub prunable: Option<String>,
}

/// Parse NUL-delimited porcelain output. Attributes end in NUL; a record ends
/// with an extra NUL (an empty attribute).
pub fn parse_porcelain_z(out: &[u8]) -> Result<Vec<WorktreeEntry>, String> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for field in out.split(|b| *b == 0) {
        if field.is_empty() {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            continue;
        }
        let text = std::str::from_utf8(field).map_err(|e| format!("non-UTF-8 field: {e}"))?;
        let (key, value) = match text.split_once(' ') {
            Some((k, v)) => (k, Some(v)),
            None => (text, None),
        };
        if key == "worktree" {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            let path = value.ok_or("worktree line without a path")?;
            current = Some(WorktreeEntry {
                path: PathBuf::from(path),
                head: None,
                branch: None,
                bare: false,
                detached: false,
                locked: None,
                prunable: None,
            });
            continue;
        }
        let entry = current
            .as_mut()
            .ok_or_else(|| format!("attribute {key:?} before any worktree line"))?;
        let value_owned = || value.unwrap_or_default().to_string();
        match key {
            "HEAD" => entry.head = Some(value_owned()),
            "branch" => entry.branch = Some(value_owned()),
            "bare" => entry.bare = true,
            "detached" => entry.detached = true,
            "locked" => entry.locked = Some(value_owned()),
            "prunable" => entry.prunable = Some(value_owned()),
            // Newer git may add attributes; unknown ones carry no lifecycle meaning here.
            _ => {}
        }
    }
    if let Some(entry) = current.take() {
        entries.push(entry);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_main_locked_and_prunable_records() {
        let raw = b"worktree /r\0HEAD abc\0branch refs/heads/main\0\0\
worktree /w/a\0HEAD def\0detached\0locked kgraf:v1:pid=12:line\nbreak\0\0\
worktree /w/b\0HEAD 123\0branch refs/heads/b\0prunable gitdir file points to non-existent location\0\0";
        let entries = parse_porcelain_z(raw).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].branch.as_deref(), Some("refs/heads/main"));
        assert!(entries[1].detached);
        assert_eq!(
            entries[1].locked.as_deref(),
            Some("kgraf:v1:pid=12:line\nbreak")
        );
        assert!(entries[2].prunable.is_some());
        assert_eq!(entries[2].locked, None);
    }

    #[test]
    fn bare_lock_without_reason_is_empty_string() {
        let entries = parse_porcelain_z(b"worktree /w\0bare\0locked\0\0").unwrap();
        assert!(entries[0].bare);
        assert_eq!(entries[0].locked.as_deref(), Some(""));
    }

    #[test]
    fn attribute_before_worktree_is_an_error() {
        assert!(parse_porcelain_z(b"HEAD abc\0\0").is_err());
    }
}
