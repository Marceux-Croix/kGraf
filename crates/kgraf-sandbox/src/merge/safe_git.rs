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
    /// The hardened command for `args` at `target`, built by the host runner
    /// (`kgraf_core::host_git`: hooks, every configured driver, the file
    /// system monitor and transports off, inherited `GIT_*` removed). `Ok(Err)`
    /// is git failing to read the config, returned as git's own failure.
    fn command(
        &self,
        target: &Target<'_>,
        args: &[OsString],
    ) -> Result<Result<Command, GitOut>, MergeError> {
        let mut pre: Vec<OsString> = Vec::new();
        let cwd: &Path = match target {
            Target::Repo => {
                pre.push(join_flag("--git-dir=", &self.common_dir));
                &self.common_dir
            }
            Target::Discover(dir) => dir,
            Target::Worktree { git_dir, work_tree } => {
                // A worktree command without a trusted attribute source would
                // read the worker's `.gitattributes`. The host runner already
                // neutralizes every configured driver; the pinned source also
                // keeps a worker's builtin attributes (`merge=union`, `eol`)
                // from changing how its own attempt is staged or merged.
                // Refuse rather than fall back.
                if self.attr_tree.is_none() {
                    return Err(WorkspaceError::GitOutput {
                        args: "(worktree command)".into(),
                        detail: "no trusted attribute source; refusing to run in a worktree".into(),
                    }
                    .into());
                }
                pre.push(join_flag("--git-dir=", git_dir));
                pre.push(join_flag("--work-tree=", work_tree));
                work_tree
            }
        };
        if let Some(tree) = &self.attr_tree {
            pre.push(OsString::from(format!("--attr-source={tree}")));
        }
        let inv = kgraf_core::host_git::Invocation {
            exe: self.git.exe(),
            cwd: Some(cwd),
            pre: &pre,
        };
        let mut cmd = match kgraf_core::host_git::command(&inv, args) {
            Ok(cmd) => cmd,
            Err(kgraf_core::host_git::HostGitError::Config { code, stderr }) => {
                return Ok(Err(GitOut {
                    code,
                    stdout: Vec::new(),
                    stderr,
                }));
            }
            Err(kgraf_core::host_git::HostGitError::Spawn { source, .. }) => {
                return Err(WorkspaceError::io(self.git.exe(), source).into());
            }
            Err(e) => {
                return Err(WorkspaceError::GitOutput {
                    args: display(args),
                    detail: e.to_string(),
                }
                .into());
            }
        };
        // Merge-back reads no system config: its results must not depend on
        // a machine-wide file.
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        Ok(Ok(cmd))
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
        // Shared repository lock for this one process (decision 0094): a
        // merge step never runs while a worker's Bypass git window is open.
        // Taken before the command is built, so the host runner's config
        // listing and the command it prepares run in the same hold and a
        // Bypass write to the config cannot fall between them.
        // None while bootstrapping (no shared directory known yet), when only
        // `version` and `rev-parse` run.
        let _repo = if self.common_dir.as_os_str().is_empty() {
            None
        } else {
            Some(
                crate::repo_lock::acquire(
                    &self.common_dir,
                    crate::repo_lock::Mode::Shared,
                    &format!("kGraf merge-back git {}", display(&args)),
                    crate::repo_lock::DEFAULT_TIMEOUT,
                )
                .map_err(WorkspaceError::from)?,
            )
        };
        let mut cmd = match self.command(target, &args)? {
            Ok(cmd) => cmd,
            Err(failed) => return Ok(failed),
        };
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
