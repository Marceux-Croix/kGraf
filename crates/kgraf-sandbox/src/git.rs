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
        // Shared repository lock for this one process (decision 0094): never
        // while a worker's Bypass git window is open on the same repository.
        // Taken before the command is built, so the host runner's config
        // listing and the command it prepares run in the same hold and a
        // Bypass write to the config cannot fall between them.
        let _repo =
            crate::repo_lock::shared_for(cwd, &format!("kGraf git {}", display_args(&args)))?;
        // The host runner: hooks, drivers, the file system monitor and
        // transports off, the inherited `GIT_*` environment (`GIT_REDIRECT_ENV`
        // among it) removed, messages in one language so callers can match
        // the "is not a working tree" case.
        let pre = kgraf_core::host_git::dash_c(cwd);
        let inv = kgraf_core::host_git::Invocation {
            exe: &self.exe,
            cwd: None,
            pre: &pre,
        };
        let mut cmd = match kgraf_core::host_git::command(&inv, &args) {
            Ok(cmd) => cmd,
            Err(kgraf_core::host_git::HostGitError::Config { code, stderr }) => {
                return Err(WorkspaceError::GitFailed {
                    args: display_args(&args),
                    cwd: cwd.to_path_buf(),
                    code,
                    stderr,
                });
            }
            Err(kgraf_core::host_git::HostGitError::Spawn { source, .. }) => {
                return Err(WorkspaceError::io(&self.exe, source));
            }
            Err(e) => {
                return Err(WorkspaceError::GitOutput {
                    args: display_args(&args),
                    detail: e.to_string(),
                });
            }
        };
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

    /// The absolute common git directory of the repository at `repo`.
    pub fn common_dir(&self, repo: &Path) -> Result<PathBuf, WorkspaceError> {
        self.run_str(
            repo,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .map(PathBuf::from)
    }

    /// `git worktree list --porcelain -z` for the repository at `repo`, under
    /// the worktree-admin lock ([`with_worktree_admin_lock`]). Do not call it
    /// while holding that lock; use [`Self::worktree_list_held`] there.
    pub fn worktree_list(&self, repo: &Path) -> Result<Vec<WorktreeEntry>, WorkspaceError> {
        let common = self.common_dir(repo)?;
        with_worktree_admin_lock(&common, || self.worktree_list_held(repo))
    }

    /// [`Self::worktree_list`] for a caller that already holds the
    /// worktree-admin lock.
    pub fn worktree_list_held(&self, repo: &Path) -> Result<Vec<WorktreeEntry>, WorkspaceError> {
        let args = ["worktree", "list", "--porcelain", "-z"];
        let out = self.run(repo, args)?;
        parse_porcelain_z(&out).map_err(|detail| WorkspaceError::GitOutput {
            args: args.join(" "),
            detail,
        })
    }
}

/// Lock file in the common git directory that serializes worktree-entry git
/// calls.
const ADMIN_LOCK: &str = "kgraf-worktree-admin.lock";

/// Run `f` while holding the repository's worktree-admin lock.
///
/// Git writes `worktrees/<id>/` in steps (`gitdir`, `commondir`, `HEAD`) and
/// `git worktree list` fails on an entry caught between them, or on a
/// `worktrees` directory a prune has just removed. Every `git worktree`
/// call that adds, changes, removes, repairs or lists entries runs inside this
/// one lock, so none sees another's half-made state. It is an `flock` on a file
/// in the common git directory: it excludes other threads (each call opens its
/// own file description) and other kGraf processes on the same repository,
/// whatever their base. Hold it for the git call only. It is not reentrant: a
/// nested call on the same directory waits for itself forever.
pub fn with_worktree_admin_lock<T, E>(
    common_dir: &Path,
    f: impl FnOnce() -> Result<T, E>,
) -> Result<T, E>
where
    E: From<WorkspaceError>,
{
    let path = common_dir.join(ADMIN_LOCK);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| WorkspaceError::io(&path, e))?;
    file.lock().map_err(|e| WorkspaceError::io(&path, e))?;
    let _held = kgraf_core::flock::HeldLock::new(file);
    f()
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
