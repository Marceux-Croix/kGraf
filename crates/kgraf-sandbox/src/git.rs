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
use std::time::Duration;

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
    /// The bound of each command ([`kgraf_core::host_git::COMMAND_TIMEOUT`]
    /// unless set with [`Self::with_timeout`]).
    timeout: Duration,
}

impl Git {
    /// Find `git` on `PATH`. Fails loud when absent.
    pub fn discover() -> Result<Self, WorkspaceError> {
        let path = std::env::var_os("PATH").ok_or(WorkspaceError::GitNotFound)?;
        let name = if cfg!(windows) { "git.exe" } else { "git" };
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
            .map(Self::at)
            .ok_or(WorkspaceError::GitNotFound)
    }

    /// The resolved executable. `merge` builds its own hardened invocations
    /// (explicit `--git-dir`, hooks and fsmonitor off) around it.
    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// Use a specific git executable (for example a logging wrapper in tests).
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

    /// Run `git -C <cwd> <args>` and return stdout on success.
    pub fn run<I, S>(&self, cwd: &Path, args: I) -> Result<Vec<u8>, WorkspaceError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_stdin(cwd, args, &[])
    }

    /// [`Self::run`] with `input` written to git's standard input, for a
    /// command that reads requests from it (`cat-file --batch`). Bounded
    /// ([`kgraf_core::child_run`]): input is written and output read on
    /// threads, so a large reply cannot deadlock against an unread pipe; a
    /// git still running at the bound is killed with what it started; a
    /// pipe a process outside git's tree keeps open does not hold the call.
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
        let cmd = match kgraf_core::host_git::command(&inv, &args) {
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
        let output = kgraf_core::child_run::run(
            cmd,
            &format!("git {}", display_args(&args)),
            (!input.is_empty()).then_some(input),
            kgraf_core::child_run::Limits::new(self.timeout),
        )
        .map_err(|e| match e {
            kgraf_core::child_run::RunError::Spawn { source, .. } => {
                WorkspaceError::io(&self.exe, source)
            }
            other => WorkspaceError::Run(other),
        })?;
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

/// The longest a caller waits for the worktree-admin lock. A holder runs one
/// git command: up to `kgraf_core::repo_lock::DEFAULT_TIMEOUT` (15 min)
/// waiting for the repository lock, then up to
/// [`kgraf_core::host_git::COMMAND_TIMEOUT`] (10 min) running, or 5 min for
/// a merge-back command. 30 minutes covers one such holder with room to
/// spare, so a wait this long means a holder that is stuck, not a slow one.
pub const ADMIN_LOCK_WAIT: Duration = Duration::from_secs(30 * 60);

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
/// nested call on the same directory waits for itself until
/// [`ADMIN_LOCK_WAIT`], then fails with [`WorkspaceError::LockTimeout`].
pub fn with_worktree_admin_lock<T, E>(
    common_dir: &Path,
    f: impl FnOnce() -> Result<T, E>,
) -> Result<T, E>
where
    E: From<WorkspaceError>,
{
    with_worktree_admin_lock_within(common_dir, ADMIN_LOCK_WAIT, f)
}

/// [`with_worktree_admin_lock`] with another bound on the wait.
pub fn with_worktree_admin_lock_within<T, E>(
    common_dir: &Path,
    bound: Duration,
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
    kgraf_core::waits::lock_file(
        &file,
        kgraf_core::waits::FileLockMode::Exclusive,
        &format!("the worktree-admin lock {}", path.display()),
        bound,
    )
    .map_err(|e| WorkspaceError::from_lock(&path, e))?;
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

/// A stand-in git for tests whose command leaves a process in a new
/// session holding its standard output, then exits.
#[cfg(all(test, unix))]
pub(crate) mod fake_git {
    use std::path::{Path, PathBuf};

    /// Write the stand-in into `dir`: it answers `version` and the config
    /// listing as git would, and for any other command starts the holder,
    /// waits until the holder has left git's session (its pid is in
    /// `pidfile`), prints `out` and exits 0. Returns (executable, pidfile).
    pub fn detached_holder(dir: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let exe = dir.join("git");
        let pidfile = dir.join("holder.pid");
        let script = format!(
            "#!/bin/sh\n\
             for a in \"$@\"; do\n\
               case \"$a\" in\n\
                 version) echo 'git version 2.45.0'; exit 0 ;;\n\
                 config) exit 0 ;;\n\
               esac\n\
             done\n\
             perl -MPOSIX -e 'POSIX::setsid(); open(F, \">\", $ARGV[0]); print F $$; close F; sleep 60' '{p}' &\n\
             while [ ! -s '{p}' ]; do sleep 0.01; done\n\
             echo out\n",
            p = pidfile.display()
        );
        std::fs::write(&exe, script).expect("write the stand-in");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        (exe, pidfile)
    }

    /// End the holder, if it started.
    pub fn kill_holder(pidfile: &Path) {
        if let Ok(pid) = std::fs::read_to_string(pidfile)
            && let Ok(pid) = pid.trim().parse::<i32>()
        {
            // SAFETY: signals only the test's own holder.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A git whose output a process outside its tree keeps open: the call
    /// returns git's output once the drain grace has passed, instead of
    /// waiting for that process (60 s).
    #[cfg(unix)]
    #[test]
    fn a_held_output_pipe_does_not_hold_run_with_stdin() {
        let dir = tempfile::tempdir().expect("tmp");
        let (exe, pidfile) = fake_git::detached_holder(dir.path());
        let cwd = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(Git::at(exe).run_with_stdin(&cwd, ["rev-parse", "HEAD"], b"input\n"));
        });
        let got = rx.recv_timeout(std::time::Duration::from_secs(30));
        fake_git::kill_holder(&pidfile);
        let out = got.expect("returned within the bound").expect("ran");
        assert_eq!(String::from_utf8_lossy(&out).trim(), "out");
    }

    /// The worktree-admin lock, held by another opener, times out naming
    /// itself and the wait.
    #[test]
    fn the_worktree_admin_lock_times_out_while_held() {
        let dir = tempfile::tempdir().expect("tmp");
        let holder = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.path().join(ADMIN_LOCK))
            .expect("open");
        holder.lock().expect("hold");
        let started = std::time::Instant::now();
        // On its own thread: a wait that never ends fails here, not hangs.
        let (tx, rx) = std::sync::mpsc::channel();
        let common = dir.path().to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(with_worktree_admin_lock_within(
                &common,
                std::time::Duration::from_millis(200),
                || Ok::<(), WorkspaceError>(()),
            ));
        });
        let err = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the wait ended within its bound")
            .expect_err("held");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        match err {
            WorkspaceError::LockTimeout(t) => {
                assert!(t.lock.contains("worktree-admin"), "{t}");
                assert!(t.waited >= std::time::Duration::from_millis(200), "{t}");
            }
            other => panic!("{other}"),
        }
    }

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
