//! The only git runner merge-back uses. Every invocation carries the same
//! overrides; there is no way to run git in this module without them.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::WorkspaceError;
use crate::git::Git;

use super::state::MergeError;

/// Where a hardened git invocation points.
pub(super) enum Target<'a> {
    /// The shared repository, no working tree.
    Repo,
    /// Discover the repository from a directory. Only for the main
    /// repository root at open; never for a worker's worktree.
    Discover(&'a Path),
    /// A worker's worktree: its trusted admin dir and its root.
    Worktree {
        git_dir: &'a Path,
        work_tree: &'a Path,
    },
}

pub(super) struct SafeGit {
    pub git: Git,
    pub common_dir: PathBuf,
    /// Tree git reads attributes from (`--attr-source`), fixed at open.
    /// `None` only while bootstrapping, when no worktree call is possible.
    pub attr_tree: Option<String>,
    pub timeout: Duration,
}

pub(super) struct GitOut {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl SafeGit {
    fn command(&self, target: &Target<'_>) -> Result<Command, MergeError> {
        let mut cmd = Command::new(self.git.exe());
        // Strip everything git reads from the environment (GIT_DIR,
        // GIT_CONFIG_PARAMETERS, GIT_CONFIG_COUNT, GIT_ATTR_SOURCE,
        // GIT_EXEC_PATH, ...): an inherited value could redirect the
        // repository or inject config.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor="]);
        match target {
            Target::Repo => {
                cmd.arg(join_flag("--git-dir=", &self.common_dir));
                cmd.current_dir(&self.common_dir);
            }
            Target::Discover(dir) => {
                cmd.current_dir(dir);
            }
            Target::Worktree { git_dir, work_tree } => {
                // A worktree command without a trusted attribute source would
                // read the worker's `.gitattributes` and could run a filter
                // driver on the host. Refuse rather than fall back.
                if self.attr_tree.is_none() {
                    return Err(WorkspaceError::GitOutput {
                        args: "(worktree command)".into(),
                        detail: "no trusted attribute source; refusing to run in a worktree".into(),
                    }
                    .into());
                }
                cmd.arg(join_flag("--git-dir=", git_dir));
                cmd.arg(join_flag("--work-tree=", work_tree));
                cmd.current_dir(work_tree);
            }
        }
        if let Some(tree) = &self.attr_tree {
            cmd.arg(format!("--attr-source={tree}"));
        }
        Ok(cmd)
    }

    /// Run git, bounded by the timeout. Returns whatever exit status git gave.
    pub fn exec<I, S>(
        &self,
        target: &Target<'_>,
        env: &[(&str, &str)],
        args: I,
        stdin: Option<&[u8]>,
    ) -> Result<GitOut, MergeError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let mut cmd = self.command(target)?;
        cmd.args(&args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let exe = self.git.exe().to_path_buf();
        let io = |e| MergeError::from(WorkspaceError::io(&exe, e));
        let mut child = cmd.spawn().map_err(io)?;
        // Drain both pipes on threads so a large output cannot block git
        // while this thread waits on the deadline.
        let drain = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut p) = pipe {
                    let _ = p.read_to_end(&mut buf);
                }
                buf
            })
        };
        let out_thread = drain(child.stdout.take().map(|p| Box::new(p) as _));
        let err_thread = drain(child.stderr.take().map(|p| Box::new(p) as _));
        if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(bytes).map_err(io)?;
        }
        let deadline = Instant::now() + self.timeout;
        let mut pause = Duration::from_millis(1);
        let status = loop {
            if let Some(status) = child.try_wait().map_err(io)? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(MergeError::GitTimeout {
                    args: display(&args),
                    after: self.timeout,
                });
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(50));
        };
        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();
        Ok(GitOut {
            code: status.code(),
            stdout,
            stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
        })
    }

    /// Run and require exit 0; return raw stdout.
    pub fn ok_bytes<I, S>(
        &self,
        target: &Target<'_>,
        env: &[(&str, &str)],
        args: I,
        stdin: Option<&[u8]>,
    ) -> Result<Vec<u8>, MergeError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let out = self.exec(target, env, &args, stdin)?;
        if out.code != Some(0) {
            return Err(self.failed(target, &args, &out));
        }
        Ok(out.stdout)
    }

    /// Run and require exit 0; return trimmed UTF-8 stdout.
    pub fn ok<I, S>(
        &self,
        target: &Target<'_>,
        env: &[(&str, &str)],
        args: I,
        stdin: Option<&[u8]>,
    ) -> Result<String, MergeError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let out = self.ok_bytes(target, env, &args, stdin)?;
        String::from_utf8(out)
            .map(|s| s.trim().to_string())
            .map_err(|e| {
                WorkspaceError::GitOutput {
                    args: display(&args),
                    detail: e.to_string(),
                }
                .into()
            })
    }

    pub fn failed(&self, target: &Target<'_>, args: &[OsString], out: &GitOut) -> MergeError {
        let cwd = match target {
            Target::Repo => self.common_dir.clone(),
            Target::Discover(dir) => dir.to_path_buf(),
            Target::Worktree { work_tree, .. } => work_tree.to_path_buf(),
        };
        WorkspaceError::GitFailed {
            args: display(args),
            cwd,
            code: out.code,
            stderr: out.stderr.clone(),
        }
        .into()
    }
}

fn join_flag(flag: &str, path: &Path) -> OsString {
    let mut s = OsString::from(flag);
    s.push(path.as_os_str());
    s
}

pub(super) fn display(args: &[OsString]) -> String {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}
