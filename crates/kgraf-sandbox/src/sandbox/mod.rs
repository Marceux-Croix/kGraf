//! Isolation runner: build a command whose writes are confined to a policy's
//! writable roots, whose reads of credential stores are denied, and whose
//! environment carries no agent sockets or secrets.
//!
//! The runner returns a [`SandboxedCommand`] instead of spawning, so the shell
//! tool (tools stream) owns stdio, timeouts and async spawning. Command policy
//! (codex-rs `execpolicy`) is out of scope here.
//!
//! Per platform (decision 0011):
//! - macOS: `/usr/bin/sandbox-exec` with a generated Seatbelt profile.
//! - Linux: Landlock plus a seccomp filter, and user, mount, IPC and PID
//!   namespaces where the kernel allows them, applied in the child between
//!   fork and exec. Full isolation needs Landlock ABI 3 and seccomp; a kernel
//!   without seccomp gets the Landlock-only backend, which is partial and
//!   runs only when the policy opts in to partial isolation.
//! - Windows: a write-restricted token, per-root capability ACLs, a Job Object
//!   and a private desktop. Partial: reads and network are not confined, so it
//!   runs only when the policy opts in to partial isolation. The command is
//!   spawned with `CreateProcessAsUserW`, not std, so [`SandboxedCommand`]
//!   exposes stdio through [`Redirect`] and `spawn` returns a
//!   [`SandboxedChild`] there.
//! - Others: [`SandboxError::Unsupported`]. Never unconfined (fail loud).
//!
//! Every Unix child runs in a new session with no controlling terminal
//! (`setsid`), so it cannot inject keystrokes into the user's terminal
//! (TIOCSTI). A Windows child gets no console (`CREATE_NO_WINDOW`) and may not
//! inherit a console input handle, for the same reason.

#[cfg(unix)]
mod cargo_home;
#[cfg(unix)]
mod child;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
// Also built for tests elsewhere: its pure helpers (quoting, environment
// block, capability SIDs) are unit-tested on the development host.
#[cfg(any(windows, test))]
mod windows;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Child;
use std::process::{Command, ExitStatus, Output, Stdio};

use crate::error::SandboxError;
use crate::workspace::Workspace;

/// Network access inside the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    /// IP traffic only. Unix-domain sockets stay denied except under the
    /// writable roots and the policy's explicit allowlist.
    Allow,
    /// Deny all network. A backend that cannot enforce this returns
    /// [`SandboxError::PolicyNotEnforceable`] instead of running with network.
    Deny,
}

/// What a sandboxed command may touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    writable_roots: Vec<PathBuf>,
    readable_roots: Vec<PathBuf>,
    read_denies: Vec<PathBuf>,
    unix_sockets: Vec<PathBuf>,
    pass_env: Vec<String>,
    network: Network,
    tmp_dir: Option<PathBuf>,
    accept_partial: bool,
    /// The directory the cargo files above a writable root may be read up
    /// to ([`Self::with_ancestor_reads`]); `None` reads none of them.
    ancestor_limit: Option<PathBuf>,
    /// A worktree's hold file: every child takes a shared lock on it that it
    /// and its descendants keep until they exit (see `child::install`).
    #[cfg_attr(not(unix), allow(dead_code))]
    hold: Option<PathBuf>,
    /// The cargo home whose config is passed in without its secrets
    /// ([`Self::with_user_cargo_home`]); `None` takes `$CARGO_HOME`, else
    /// `~/.cargo`.
    #[cfg_attr(not(unix), allow(dead_code))]
    user_cargo_home: Option<PathBuf>,
    /// Git directories made writable whose configuration stays read-only
    /// ([`Self::with_frozen_git_config`]).
    frozen_git: Vec<FrozenGit>,
    /// Installed dependency trees in the checkout, readable and runnable,
    /// never writable ([`Self::with_dependency_links`]).
    dependency_reads: Vec<PathBuf>,
    /// The virtual environments linked into the tree; the one owning the
    /// command's folder is activated ([`Self::with_dependency_links`]).
    virtual_envs: Vec<crate::dependencies::VirtualEnv>,
    /// The folder that chooses the virtual environment, when it is not the
    /// command's working directory ([`Self::with_command_dir`]).
    command_dir: Option<PathBuf>,
}

/// A writable git directory whose configuration files stay read-only
/// (decision 0094). See [`SandboxPolicy::with_frozen_git_config`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenGit {
    /// The shared git directory (`git rev-parse --git-common-dir`).
    pub common_dir: PathBuf,
    /// Further files git reads configuration from that may lie under a
    /// writable root: every `include.path` and `includeIf.*.path` target,
    /// existing or not.
    pub files: Vec<PathBuf>,
}

/// The files, relative to a frozen git directory, that decide which programs
/// git runs: the shared config, the per-worktree config of the main
/// worktree, the info attributes (they select drivers), and in each worktree
/// admin directory the `commondir` pointer (it names the directory whose
/// config git reads) and its per-worktree config; each with the lock file
/// git writes it through. `*` is one path segment of any name. Every
/// directory above an entry (`info`, `worktrees`, `worktrees/*`) is frozen as
/// a directory entry too, so it cannot be renamed away and replaced by one
/// built elsewhere; writes inside it stay allowed.
pub const FROZEN_GIT_FILES: &[&str] = &[
    "config",
    "config.lock",
    "config.worktree",
    "config.worktree.lock",
    "info/attributes",
    "info/attributes.lock",
    "worktrees/*/commondir",
    "worktrees/*/commondir.lock",
    "worktrees/*/config.worktree",
    "worktrees/*/config.worktree.lock",
];

impl SandboxPolicy {
    /// Writes allowed only beneath `writable_roots` (plus `/dev/null`).
    pub fn new(writable_roots: Vec<PathBuf>, network: Network) -> Self {
        Self {
            writable_roots,
            readable_roots: Vec::new(),
            read_denies: Vec::new(),
            unix_sockets: Vec::new(),
            pass_env: Vec::new(),
            network,
            tmp_dir: None,
            accept_partial: false,
            ancestor_limit: None,
            hold: None,
            user_cargo_home: None,
            frozen_git: Vec::new(),
            dependency_reads: Vec::new(),
            virtual_envs: Vec::new(),
            command_dir: None,
        }
    }

    /// The standard worker policy: the worktree root and its scratch
    /// directory are writable, the scratch directory is TMPDIR, and the
    /// repository's git directory is readable.
    pub fn for_workspace(ws: &Workspace, network: Network) -> Self {
        Self::new(
            vec![ws.root().to_path_buf(), ws.scratch().to_path_buf()],
            network,
        )
        .with_readable_root(ws.git_common_dir().to_path_buf())
        .with_tmp_dir(ws.scratch().to_path_buf())
        .holding(ws.hold_file().to_path_buf())
    }

    /// Every process this policy starts, and each of its descendants, holds a
    /// shared lock on `file` until it exits, so a worktree is never reopened
    /// under a process still running in it ([`crate::WorkspaceManager::reopen`]).
    /// Unix only; on Windows a job object ends the tree with its owner.
    pub fn holding(mut self, file: PathBuf) -> Self {
        self.hold = Some(file);
        self
    }

    /// Set TMPDIR, TMP and TEMP for the child. The directory must also be a
    /// writable root (checked when the command is built).
    pub fn with_tmp_dir(mut self, dir: PathBuf) -> Self {
        self.tmp_dir = Some(dir);
        self
    }

    /// The user's cargo home, whose config's non-secret settings (a source
    /// replacement, a linker, rustflags) a command with a temporary directory
    /// gets in a cargo home of its own, `<tmp>/.kgraf-cargo-home`, named by
    /// `CARGO_HOME` (decision 0084). The real config is never readable inside
    /// the sandbox: it can hold registry tokens. Default: `$CARGO_HOME`, else
    /// `~/.cargo`. Without a config file there, `CARGO_HOME` is left as it
    /// is. A setting withheld for a login in its URL is reported with
    /// [`SandboxedCommand::refused_reads`]. Unix only; Windows does not
    /// confine reads, so cargo reads the real file there.
    pub fn with_user_cargo_home(mut self, dir: PathBuf) -> Self {
        self.user_cargo_home = Some(dir);
        self
    }

    /// An extra writable root (and so a readable one). Decision 0094's Bypass
    /// mode adds the repository's git directory here, so a worker's git
    /// commands can write refs, config and the index; every other policy
    /// keeps that directory read-only.
    pub fn with_writable_root(mut self, dir: PathBuf) -> Self {
        self.writable_roots.push(dir);
        self
    }

    /// Decision 0094: inside `common_dir`, made writable with
    /// [`Self::with_writable_root`], keep every [`FROZEN_GIT_FILES`] entry and
    /// each of `files` read-only: no write, creation, deletion, rename or
    /// link. On a case-insensitive volume a name spelled in another case is
    /// denied too, existing or not: Seatbelt compares names the way the
    /// volume does (measured). kGraf's own git lists the configuration and
    /// then runs with every driver and hook it found neutralized; with these
    /// files frozen, a command running while kGraf's git runs cannot add one
    /// between the two. A configuration write goes through the locked
    /// protected-write path instead.
    ///
    /// Enforced on macOS ([`IsolationStrength::git_config_freezable`]). The
    /// Linux and Windows backends cannot deny a file inside a writable
    /// directory, and refuse a policy that asks
    /// ([`SandboxError::PolicyNotEnforceable`]) rather than run with the
    /// configuration writable.
    pub fn with_frozen_git_config(mut self, common_dir: PathBuf, files: Vec<PathBuf>) -> Self {
        self.frozen_git.push(FrozenGit { common_dir, files });
        self
    }

    /// Let a command read, and run programs from, the checkout's installed
    /// dependency trees that [`crate::link_dependency_trees`] linked into the
    /// tree it works in: `links.reads`, never writable. And when the tree's
    /// root virtual environment is one of them, run every command as if it
    /// were activated: `VIRTUAL_ENV` names it and its `bin` folder comes
    /// first on `PATH`, so `python`, `pytest` and the project's other entry
    /// points are the environment's.
    ///
    /// **macOS**: re-allowed after the checkout deny and before the
    /// credential deny list, so a link inside a tree that points at a
    /// credential store stays denied. **Linux**: each tree is granted read
    /// and execute (Landlock judges the resolved file, so a link inside a
    /// tree that points outside every granted path stays unreadable).
    /// **Windows** does not confine reads; no link is made there.
    ///
    /// A tree beneath a [`Self::deny_read`] path is refused when the command
    /// is built ([`SandboxError::InvalidPolicy`]).
    pub fn with_dependency_links(mut self, links: &crate::DependencyLinks) -> Self {
        for read in &links.reads {
            if !self.dependency_reads.contains(read) {
                self.dependency_reads.push(read.clone());
            }
        }
        for venv in &links.virtual_envs {
            if !self.virtual_envs.contains(venv) {
                self.virtual_envs.push(venv.clone());
            }
        }
        self
    }

    /// The folder whose virtual environment a command runs with, when the
    /// command does its work somewhere other than its working directory: a
    /// script that starts with `cd services/api &&`
    /// ([`crate::script_start_dir`]). Default: the working directory.
    pub fn with_command_dir(mut self, dir: PathBuf) -> Self {
        self.command_dir = Some(dir);
        self
    }

    /// An extra read-only root. Needed where reads are allowlisted (Linux);
    /// on macOS reads are open apart from the credential deny list, and on
    /// Windows reads are not confined.
    pub fn with_readable_root(mut self, dir: PathBuf) -> Self {
        self.readable_roots.push(dir);
        self
    }

    /// Opt in: let cargo read its manifest and configuration (`Cargo.toml`,
    /// `.cargo/config.toml`, `.cargo/config`) in the directories above a
    /// writable root, up to and including `limit` and never past it. Cargo
    /// reads those on its way up from a worktree inside the user's checkout
    /// and stops with "Permission denied" when it cannot.
    ///
    /// **macOS**: the limit itself is denied as a whole, and the roots, the
    /// readable roots (the git common directory), the metadata of each
    /// denied directory on the way to a root, and these cargo files re-allowed
    /// on top. So a build in a worktree cannot read the rest of the checkout
    /// it lives in: a `.env` at its root, another project beside it. Cargo
    /// files beneath a [`Self::deny_read`] path or a cargo home stay denied.
    /// Windows does not confine reads.
    ///
    /// **Linux**: each file is granted read (not
    /// execute) as that one file, opened once without following any link,
    /// and only when it and every directory from it up to `limit` are ones
    /// only their owner can change ([`crate::trust::untrusted_reason`]:
    /// owned by this user or root, never writable by others, group-writable
    /// only by this user's private group). Never beneath a
    /// [`Self::deny_read`] path, `~/.cargo` or `$CARGO_HOME`; and no `.cargo`
    /// file at all when no home directory can be found. A file that exists
    /// but is not granted is reported with its cause
    /// ([`SandboxedCommand::refused_reads`], [`Self::ancestor_refusals`]).
    ///
    /// `limit` is the checkout root the caller knows (the directory holding
    /// `.kgraf`); it is not derived from git, which is wrong for linked
    /// worktrees, `--separate-git-dir` and submodules. It must exist (an
    /// error otherwise), and **its own parents must be trusted too**: the
    /// limit is opened by its full path, so a user who can rename a directory
    /// above it can put another directory at that path. A checkout under the
    /// user's home qualifies; one under a shared, non-sticky directory does
    /// not. A policy whose tree is its own copy (the contract checks) must not
    /// opt in: the directories above such a tree (`/tmp`) are not the
    /// owner's, and another user could plant a `.cargo/config.toml` with a
    /// `runner` there.
    ///
    /// Cargo only. pytest also reads configuration above its directory
    /// (`pytest.ini`, `pyproject.toml`, `tox.ini`, `setup.cfg`) and gets no
    /// rule here: a sandboxed pytest run should be given `--rootdir` and
    /// `--confcutdir` at the tree instead (not yet wired).
    pub fn with_ancestor_reads(mut self, limit: PathBuf) -> Self {
        self.ancestor_limit = Some(limit);
        self
    }

    /// Each cargo file above a writable root that [`Self::with_ancestor_reads`]
    /// would not grant, with its cause, without building a command: for a
    /// notice when a run opens. Empty without the opt-in, and on macOS and
    /// Windows, which grant or do not confine these reads.
    pub fn ancestor_refusals(&self) -> Result<Vec<String>, SandboxError> {
        #[cfg(target_os = "linux")]
        {
            linux::ancestor_refusals(&self.resolved()?)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(Vec::new())
        }
    }

    /// Deny reads of `path`: a directory and everything beneath it, or one
    /// file (kGraf's own state: the worker configuration with MCP header
    /// secrets, the ledger, transcripts, allow rules). A file-level deny is
    /// how a configuration kept in a broad directory such as `$HOME` is
    /// hidden without hiding its neighbors (`~/.cargo`, `~/.rustup`). The
    /// writable and readable roots stay readable even when they lie beneath a
    /// denied directory, as a worker's worktree under `.kgraf/workspaces`
    /// does. So does the metadata (`stat`), never the listing or contents, of
    /// each denied directory on the path to such a root: realpath(3), and so
    /// cargo, fails without it. The path must exist (it is resolved like every
    /// other root).
    ///
    /// Enforced on macOS by the Seatbelt profile. On Linux reads are an
    /// allowlist, so a path outside the roots is already unreadable, and the
    /// cargo manifests and configuration the backend lets a command read in
    /// the ancestors of its writable roots are never taken from beneath a
    /// denied path. Windows
    /// does not confine reads; that gap is part of its reported partial
    /// isolation, which a caller must accept explicitly.
    pub fn deny_read(mut self, path: PathBuf) -> Self {
        self.read_denies.push(path);
        self
    }

    /// Allow connecting to one unix-domain socket (or sockets beneath a
    /// directory) outside the writable roots.
    pub fn allow_unix_socket(mut self, path: PathBuf) -> Self {
        self.unix_sockets.push(path);
        self
    }

    /// Keep one inherited environment variable that the scrub would remove.
    pub fn pass_env(mut self, name: impl Into<String>) -> Self {
        self.pass_env.push(name.into());
        self
    }

    /// Run on a backend whose [`IsolationStrength`] reports partial isolation
    /// (today: Windows, and Linux without seccomp). Without this, such a
    /// backend refuses (fail loud).
    pub fn accept_partial_isolation(mut self, accept: bool) -> Self {
        self.accept_partial = accept;
        self
    }

    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }
    pub fn network(&self) -> Network {
        self.network
    }

    /// Canonicalize and check every root. Seatbelt matches resolved paths
    /// (`/var` is `/private/var` on macOS), so an unresolved root would
    /// silently match nothing.
    fn resolved(&self) -> Result<Resolved, SandboxError> {
        if self.writable_roots.is_empty() {
            return Err(SandboxError::InvalidPolicy(
                "no writable root; a sandbox that can write nowhere cannot build".into(),
            ));
        }
        let writable = resolve_all(&self.writable_roots)?;
        if writable.iter().any(|r| r.parent().is_none()) {
            return Err(SandboxError::InvalidPolicy(
                "writable root / confines nothing".into(),
            ));
        }
        if let Some(tmp) = &self.tmp_dir {
            let tmp = resolve(tmp)?;
            if !writable.iter().any(|r| tmp.starts_with(r)) {
                return Err(SandboxError::InvalidPolicy(format!(
                    "tmp dir {} is not under a writable root",
                    tmp.display()
                )));
            }
        }
        let read_denied = resolve_all(&self.read_denies)?;
        let dependency_reads = resolve_all(&self.dependency_reads)?;
        // Each grant is the canonical path `link_dependency_trees` found.
        // One that now resolves elsewhere (it, or a folder above it, became
        // a link) would grant the link's target: refused, not followed.
        if let Some((given, now)) = self
            .dependency_reads
            .iter()
            .zip(&dependency_reads)
            .find(|(given, now)| given != now)
        {
            return Err(SandboxError::InvalidPolicy(format!(
                "dependency tree {} is a link, or lies below one, and now resolves to {}; \
                 it is not granted",
                given.display(),
                now.display()
            )));
        }
        if let Some(tree) = dependency_reads
            .iter()
            .find(|t| read_denied.iter().any(|d| t.starts_with(d)))
        {
            return Err(SandboxError::InvalidPolicy(format!(
                "dependency tree {} lies beneath a path whose reads are denied",
                tree.display()
            )));
        }
        Ok(Resolved {
            writable,
            readable: resolve_all(&self.readable_roots)?,
            read_denied,
            dependency_reads,
            // Sockets may not exist yet; resolve the parent directory.
            unix_sockets: self
                .unix_sockets
                .iter()
                .map(|p| resolve_socket(p))
                .collect::<Result<_, _>>()?,
            network: self.network,
            home: home_dir(),
            ancestor_limit: self.ancestor_limit.as_deref().map(resolve).transpose()?,
            cargo_homes: cargo_homes(),
            frozen_git: self
                .frozen_git
                .iter()
                .map(|f| {
                    Ok(FrozenGit {
                        common_dir: resolve(&f.common_dir)?,
                        files: f
                            .files
                            .iter()
                            .map(|p| resolve_lenient(p))
                            .collect::<Result<_, SandboxError>>()?,
                    })
                })
                .collect::<Result<_, SandboxError>>()?,
        })
    }
}

/// Files cargo reads in every ancestor of the directory it runs in, relative
/// to that ancestor: the manifest, while it looks for an enclosing workspace
/// (a package whose own manifest has no `[workspace]` walks up), and the
/// configuration, which it always gathers upward. With
/// [`SandboxPolicy::with_ancestor_reads`], these are the only files between a
/// writable root and the limit a command may read: on Linux each is granted
/// on its own (reads are an allowlist there); on macOS the limit is denied
/// as a whole and these are re-allowed one by one, so the rest of the
/// checkout (a `.env` with vendor keys at its root, a sibling project) stays
/// unreadable to a build.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
const ANCESTOR_READS: &[&str] = &["Cargo.toml", ".cargo/config.toml", ".cargo/config"];

/// A policy with every path canonical, handed to a backend.
#[derive(Debug)]
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", windows)),
    allow(dead_code)
)]
struct Resolved {
    writable: Vec<PathBuf>,
    /// Read on Linux (allowlist); macOS reads are open apart from the deny
    /// list; Windows reads are not confined.
    #[cfg_attr(windows, allow(dead_code))]
    readable: Vec<PathBuf>,
    /// Reads denied beneath these (macOS); on Linux, the ancestor manifests
    /// the backend makes readable skip them. See [`SandboxPolicy::deny_read`].
    #[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
    read_denied: Vec<PathBuf>,
    /// Enforced on macOS. Linux with seccomp refuses every unix socket and
    /// so refuses a non-empty list; Landlock-only Linux cannot confine
    /// connect(), so every socket is reachable there. Windows refuses a
    /// non-empty list.
    unix_sockets: Vec<PathBuf>,
    network: Network,
    /// The credential deny list (macOS) and toolchain reads (Linux) hang off it.
    #[cfg_attr(windows, allow(dead_code))]
    home: Option<PathBuf>,
    /// See [`SandboxPolicy::with_ancestor_reads`] (Linux and macOS).
    #[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
    ancestor_limit: Option<PathBuf>,
    /// Every cargo home an ancestor read must never enter: `$HOME/.cargo`,
    /// the password database's home `.cargo`, and `$CARGO_HOME`. `None` when
    /// no home directory can be found at all; then no `.cargo` file above a
    /// root is read (Linux and macOS).
    #[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
    cargo_homes: Option<Vec<PathBuf>>,
    /// See [`SandboxPolicy::with_frozen_git_config`]: the git directory
    /// canonical, each file with its existing ancestors canonical. Enforced
    /// on macOS; the other backends refuse a non-empty list.
    frozen_git: Vec<FrozenGit>,
    /// See [`SandboxPolicy::with_dependency_links`]: canonical, each a
    /// folder or file in the checkout. Granted on macOS and Linux.
    #[cfg_attr(windows, allow(dead_code))]
    dependency_reads: Vec<PathBuf>,
}

/// `path` with its deepest existing ancestor canonical and the rest
/// appended as written, so a file that does not exist yet still matches the
/// resolved paths Seatbelt compares.
fn resolve_lenient(path: &Path) -> Result<PathBuf, SandboxError> {
    if !path.is_absolute() {
        return Err(SandboxError::InvalidPolicy(format!(
            "path {} is not absolute",
            path.display()
        )));
    }
    let mut rest = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(cur) {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return Ok(out);
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                cur = parent;
            }
            _ => return Ok(path.to_path_buf()),
        }
    }
}

fn resolve(path: &Path) -> Result<PathBuf, SandboxError> {
    if !path.is_absolute() {
        return Err(SandboxError::InvalidPolicy(format!(
            "path {} is not absolute",
            path.display()
        )));
    }
    std::fs::canonicalize(path).map_err(|source| SandboxError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn resolve_all(paths: &[PathBuf]) -> Result<Vec<PathBuf>, SandboxError> {
    paths.iter().map(|p| resolve(p)).collect()
}

fn resolve_socket(path: &Path) -> Result<PathBuf, SandboxError> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok(resolve(parent)?.join(name)),
        _ => Err(SandboxError::InvalidPolicy(format!(
            "unix socket path {} has no parent",
            path.display()
        ))),
    }
}

/// `$HOME`, else this user's home from the password database, canonical.
/// Without the fallback an unset `HOME` removed every rule that hangs off
/// the home directory (the macOS credential deny list among them).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(passwd_home)
        .and_then(|h| std::fs::canonicalize(h).ok())
}

/// The cargo homes ancestor reads never enter ([`Resolved::cargo_homes`]).
/// A home that does not exist is kept as written: no file lies beneath it.
fn cargo_homes() -> Option<Vec<PathBuf>> {
    let canonical = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    let homes: Vec<PathBuf> = [
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from),
        passwd_home(),
    ]
    .into_iter()
    .flatten()
    .map(|h| canonical(h).join(".cargo"))
    .collect();
    if homes.is_empty() {
        return None;
    }
    let mut out = homes;
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME").filter(|h| !h.is_empty()) {
        out.push(canonical(PathBuf::from(cargo_home)));
    }
    Some(out)
}

/// This user's home directory from the password database (getpwuid_r(3)).
#[cfg(unix)]
fn passwd_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call, `buf.len()` is its size,
    // and `found` is set to `pwd` or null by getpwuid_r.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            pwd.as_mut_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
            &mut found,
        )
    };
    if rc != 0 || found.is_null() {
        return None;
    }
    // SAFETY: `found` points at `pwd`, filled by the successful call; its
    // `pw_dir` points into `buf`, alive for this scope.
    let dir = unsafe { (*found).pw_dir };
    if dir.is_null() {
        return None;
    }
    // SAFETY: a non-null `pw_dir` is a NUL-terminated string in `buf`.
    let bytes = unsafe { std::ffi::CStr::from_ptr(dir) }.to_bytes();
    (!bytes.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn passwd_home() -> Option<PathBuf> {
    None
}

/// Isolation backend in use on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    MacosSeatbelt,
    /// Landlock, plus seccomp and namespaces where the kernel has them; the
    /// name predates those layers.
    LinuxLandlock,
    WindowsRestrictedToken,
}

/// How reads outside the workspace are limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadConfinement {
    /// Everything readable except a deny list of credential stores, browser
    /// profiles and shell histories.
    CredentialDenyList,
    /// Only system, toolchain and workspace paths are readable.
    AllowList,
    /// Every path the user can read, credential stores included (Windows: a
    /// write-restricted token checks its restricting SIDs on writes only).
    Unconfined,
}

/// Per-platform strength indicator (decision 0011: one badge per platform,
/// never a shared one). Each flag states what the backend enforces today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsolationStrength {
    pub backend: Backend,
    /// No path to writing outside the writable roots is known to remain.
    /// False on Linux without seccomp: Landlock cannot stop `connect()` to an
    /// existing socket such as `docker.sock`, and a Docker daemon writes
    /// anywhere.
    pub writes_confined: bool,
    pub reads: ReadConfinement,
    /// `connect()` to unix-domain sockets outside the allowlist fails
    /// (ssh-agent, D-Bus, docker.sock).
    pub unix_sockets_confined: bool,
    /// [`Network::Deny`] is enforceable.
    pub network_deniable: bool,
    /// The top-level `.git` entry of each writable root is read-only, so a
    /// worker cannot rewrite its gitlink. Where false, call
    /// [`Workspace::verify_gitlink`] before host-side git operations.
    pub gitlink_protected: bool,
    /// [`SandboxPolicy::with_frozen_git_config`] is enforceable: the backend
    /// can keep single files inside a writable directory read-only. Where
    /// false, decision 0094's Bypass shell runs with the git directory
    /// read-only instead.
    pub git_config_freezable: bool,
    /// What the backend does not stop, in words for the interface.
    pub gaps: &'static str,
}

/// The strength of this platform's backend, or the reason there is none.
pub fn platform_isolation() -> Result<IsolationStrength, SandboxError> {
    #[cfg(target_os = "macos")]
    {
        Ok(macos::STRENGTH)
    }
    #[cfg(target_os = "linux")]
    {
        Ok(linux::strength())
    }
    #[cfg(windows)]
    {
        Ok(windows::STRENGTH)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        Err(unsupported())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn unsupported() -> SandboxError {
    SandboxError::Unsupported {
        platform: std::env::consts::OS,
        reason: "only macOS, Linux and Windows have an isolation backend",
    }
}

/// Inherited variables naming an agent or daemon socket. Removing them does
/// not stop a process that already knows the path; the socket rules do that.
/// It stops tools from finding the agent by default.
const AGENT_VARS: &[&str] = &[
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "GPG_AGENT_INFO",
    "DBUS_SESSION_BUS_ADDRESS",
    "DOCKER_HOST",
    "KRB5CCNAME",
    "GNOME_KEYRING_CONTROL",
];

/// Whether an inherited variable is removed: agent sockets, and any name
/// [`kgraf_secrets::name_is_credential_shaped`] judges to hold a
/// credential -- the same test `kgraf-secrets` uses to decide which dotenv
/// values are worth registering, so a name is judged sensitive the same way
/// on both sides of the sandbox boundary.
fn is_scrubbed(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    AGENT_VARS.contains(&name) || kgraf_secrets::name_is_credential_shaped(name)
}

/// Remove scrubbed names among `inherited` from `cmd`, except `pass`.
fn scrub_env(cmd: &mut Command, inherited: impl Iterator<Item = OsString>, pass: &[String]) {
    for name in inherited {
        let passed = name.to_str().is_some_and(|n| pass.iter().any(|p| p == n));
        if is_scrubbed(&name) && !passed {
            cmd.env_remove(&name);
        }
    }
}

/// Run as if the virtual environment `venv` were activated: what its
/// `activate` script does, without running it. Its `bin` folder (`Scripts`
/// on Windows) first on `PATH`, `VIRTUAL_ENV` naming it, and no
/// `PYTHONHOME`, which would make its interpreter load another
/// installation's standard library.
fn activate_virtual_env(cmd: &mut Command, venv: &Path) {
    let bin = venv.join(if cfg!(windows) { "Scripts" } else { "bin" });
    let mut paths = vec![bin];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        cmd.env("PATH", joined);
    }
    cmd.env("VIRTUAL_ENV", venv).env_remove("PYTHONHOME");
}

/// Package-manager settings that keep a command off the user's writable
/// stores and credential files, given only where the command has a
/// temporary folder (decision 0084, amended 2026-10-06):
///
/// - Go: `GOPROXY=off` with the network denied, so a module missing from
///   the local cache fails at once, naming the module, instead of after a
///   network timeout.
/// - Gradle: its user home in the temporary folder (Gradle locks and writes
///   its home on every build, and the user's holds `gradle.properties` and
///   init scripts with repository passwords), with the user's dependency
///   cache read-only beside it (`GRADLE_RO_DEP_CACHE`, the folder holding
///   `modules-2`) when it exists.
/// - .NET: the CLI home in the temporary folder (first-run files, and the
///   user's `NuGet.Config` with package source passwords lives under it),
///   the user's package folder kept as the global packages folder
///   (`NUGET_PACKAGES`) when it exists, and the CLI's telemetry off: no
///   telemetry leaves the machine.
fn package_store_env(cmd: &mut Command, tmp: &Path, home: Option<&Path>, network: Network) {
    if network == Network::Deny {
        cmd.env("GOPROXY", "off");
    }
    cmd.env("GRADLE_USER_HOME", tmp.join("gradle-home"));
    cmd.env("DOTNET_CLI_HOME", tmp.join("dotnet-home"))
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .env("DOTNET_NOLOGO", "1")
        .env("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1");
    if let Some(home) = home {
        let gradle_cache = home.join(".gradle/caches");
        if gradle_cache.join("modules-2").is_dir() {
            cmd.env("GRADLE_RO_DEP_CACHE", gradle_cache);
        }
        let nuget = home.join(".nuget/packages");
        if nuget.is_dir() {
            cmd.env("NUGET_PACKAGES", nuget);
        }
    }
}

/// Where a standard stream of a sandboxed command goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirect {
    /// The parent's stream. On Windows an inherited console input handle is
    /// refused (keystroke injection); pipes and files are passed.
    Inherit,
    Null,
    Piped,
}

impl From<Redirect> for Stdio {
    fn from(r: Redirect) -> Self {
        match r {
            Redirect::Inherit => Stdio::inherit(),
            Redirect::Null => Stdio::null(),
            Redirect::Piped => Stdio::piped(),
        }
    }
}

/// On Windows, `spawn` returns this instead of `std::process::Child`
/// (std cannot spawn under another token).
#[cfg(windows)]
pub use windows::SandboxedChild;

/// Set the write ACL and Low label of the writable root `root` before a
/// dependency junction is made in it (`crate::dependencies`).
#[cfg(windows)]
pub(crate) fn prepare_windows_root(root: &Path) -> Result<(), SandboxError> {
    windows::prepare_root(root)
}

/// A confined command, not yet spawned. Add arguments, environment and stdio,
/// then spawn it once.
#[derive(Debug)]
pub struct SandboxedCommand {
    /// On Windows this is only a builder: program, arguments, environment
    /// and working directory are read back from it at spawn.
    cmd: Command,
    /// See [`Self::refused_reads`].
    refused_reads: Vec<String>,
    #[cfg(unix)]
    probe: child::SetupProbe,
    #[cfg(windows)]
    prepared: windows::Prepared,
    /// Explicit stdin, stdout, stderr; `None` takes the per-method default.
    #[cfg(windows)]
    stdio: [Option<Redirect>; 3],
}

impl SandboxedCommand {
    /// Each cargo file above a writable root the policy opted in to
    /// ([`SandboxPolicy::with_ancestor_reads`]) that exists but was not
    /// granted, with its cause; and each setting of the user's cargo config
    /// withheld from the command ([`SandboxPolicy::with_user_cargo_home`]).
    /// Show them with the command's own output when it fails: cargo's
    /// "Permission denied" alone does not say why.
    pub fn refused_reads(&self) -> &[String] {
        &self.refused_reads
    }

    /// The underlying command, for arguments, stdio and extra environment.
    /// A variable set here is passed even if the scrub would remove it.
    /// Unix only: on Windows the command is a builder whose stdio and
    /// `env_clear` would be silently ignored, so it is not exposed.
    #[cfg(unix)]
    pub fn command_mut(&mut self) -> &mut Command {
        &mut self.cmd
    }

    /// Set one environment variable, passed even if the scrub would remove it.
    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.env(key, val);
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.env_remove(key);
        self
    }

    /// Defaults follow std (`output` pipes stdout and stderr), except that
    /// on Windows stdin defaults to null for every method.
    pub fn stdin(&mut self, r: Redirect) -> &mut Self {
        self.set_stdio(0, r)
    }

    pub fn stdout(&mut self, r: Redirect) -> &mut Self {
        self.set_stdio(1, r)
    }

    pub fn stderr(&mut self, r: Redirect) -> &mut Self {
        self.set_stdio(2, r)
    }

    fn set_stdio(&mut self, stream: usize, r: Redirect) -> &mut Self {
        #[cfg(unix)]
        match stream {
            0 => {
                self.cmd.stdin(r);
            }
            1 => {
                self.cmd.stdout(r);
            }
            _ => {
                self.cmd.stderr(r);
            }
        }
        #[cfg(windows)]
        {
            self.stdio[stream] = Some(r);
        }
        #[cfg(not(any(unix, windows)))]
        let _ = (stream, r);
        self
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.cmd.args(args);
        self
    }

    #[cfg(unix)]
    pub fn spawn(&mut self) -> Result<Child, SandboxError> {
        let result = self.cmd.spawn();
        result.map_err(|e| self.explain(e))
    }

    /// Dropping the returned child kills its whole process tree (job close).
    #[cfg(windows)]
    pub fn spawn(&mut self) -> Result<SandboxedChild, SandboxError> {
        windows::spawn(
            &self.cmd,
            &self.prepared,
            windows::default_stdio(self.stdio),
        )
    }

    pub fn output(&mut self) -> Result<Output, SandboxError> {
        #[cfg(windows)]
        {
            windows::output(&self.cmd, &self.prepared, self.stdio)
        }
        #[cfg(not(windows))]
        {
            let result = self.cmd.output();
            result.map_err(|e| self.explain(e))
        }
    }

    pub fn status(&mut self) -> Result<ExitStatus, SandboxError> {
        #[cfg(windows)]
        {
            windows::status(&self.cmd, &self.prepared, self.stdio)
        }
        #[cfg(not(windows))]
        {
            let result = self.cmd.status();
            result.map_err(|e| self.explain(e))
        }
    }

    /// For async callers (`tokio::process::Command::from`): the command plus
    /// a probe that recovers the child's setup-failure reason after a failed
    /// spawn.
    #[cfg(unix)]
    pub fn into_parts(self) -> (Command, child::SetupProbe) {
        (self.cmd, self.probe)
    }

    #[cfg(not(windows))]
    fn explain(&self, source: std::io::Error) -> SandboxError {
        #[cfg(unix)]
        if let Some(reason) = self.probe.reason() {
            return SandboxError::ChildSetup { reason, source };
        }
        SandboxError::Spawn(source)
    }
}

#[cfg(unix)]
pub use child::SetupProbe;
/// What the running Linux kernel offers the backend (probed once).
#[cfg(target_os = "linux")]
pub use linux::{LinuxConfinement, confinement as linux_confinement};

/// Build a command that runs `program` confined by `policy`, with `cwd` as
/// its working directory. Append arguments with `.arg()`/`.args()`.
pub fn sandboxed_command(
    policy: &SandboxPolicy,
    program: impl AsRef<OsStr>,
    cwd: &Path,
) -> Result<SandboxedCommand, SandboxError> {
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (policy, program, cwd);
        Err(unsupported())
    }
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    {
        let resolved = policy.resolved()?;
        #[cfg(windows)]
        let mut cmd = Command::new(program.as_ref());
        // Linux and Windows cannot keep one file inside a writable directory
        // read-only; run nothing rather than leave the git config writable.
        #[cfg(not(target_os = "macos"))]
        if !resolved.frozen_git.is_empty() {
            return Err(SandboxError::PolicyNotEnforceable {
                #[cfg(windows)]
                backend: windows::BACKEND_NAME,
                #[cfg(target_os = "linux")]
                backend: linux::BACKEND_NAME,
                what: "a read-only git configuration inside a writable git directory",
            });
        }
        #[cfg(windows)]
        let prepared = {
            if !policy.accept_partial {
                return Err(SandboxError::PartialIsolation {
                    backend: windows::BACKEND_NAME,
                    gaps: windows::STRENGTH.gaps,
                });
            }
            windows::prepare(&resolved)?
        };
        #[cfg(target_os = "macos")]
        let (mut cmd, setup) = macos::command(&resolved, program.as_ref())?;
        #[cfg(target_os = "linux")]
        let (mut cmd, setup, mut refused_reads) = {
            if !policy.accept_partial && linux::is_partial() {
                return Err(SandboxError::PartialIsolation {
                    backend: linux::BACKEND_NAME,
                    gaps: linux::strength().gaps,
                });
            }
            linux::command_noting(&resolved, program.as_ref())?
        };
        // Only Linux grants (and so may refuse) ancestor reads.
        #[cfg(not(target_os = "linux"))]
        #[cfg_attr(windows, allow(unused_mut))]
        let mut refused_reads: Vec<String> = Vec::new();
        #[cfg(unix)]
        let hold = match &policy.hold {
            Some(file) => {
                if let Some(dir) = file.parent() {
                    std::fs::create_dir_all(dir).map_err(|source| SandboxError::Io {
                        path: dir.to_path_buf(),
                        source,
                    })?;
                }
                std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(file)
                    .map_err(|source| SandboxError::Io {
                        path: file.clone(),
                        source,
                    })?;
                use std::os::unix::ffi::OsStrExt;
                let c = |p: &Path| {
                    std::ffi::CString::new(p.as_os_str().as_bytes())
                        .map_err(|_| SandboxError::InvalidPolicy("hold path has a NUL".into()))
                };
                Some(child::Hold {
                    lock: c(file)?,
                    sessions: c(&crate::workspace::session_log(file))?,
                    host: {
                        let here = crate::workspace::HostId::current();
                        let dash = |v: Option<String>| v.unwrap_or_else(|| "-".into());
                        format!(
                            "{} {} {}",
                            dash(here.boot),
                            dash(here.ns),
                            dash(here.time_ns)
                        )
                        .into_bytes()
                    },
                })
            }
            None => None,
        };
        #[cfg(unix)]
        let probe = child::install(&mut cmd, hold, setup)?;
        cmd.current_dir(cwd);
        scrub_env(
            &mut cmd,
            std::env::vars_os().map(|(k, _)| k),
            &policy.pass_env,
        );
        // The environment owning the command's folder, or the nearest one
        // above it (`DependencyLinks::virtual_env_for`).
        let command_dir = policy.command_dir.as_deref().unwrap_or(cwd);
        let command_dir =
            std::fs::canonicalize(command_dir).unwrap_or_else(|_| command_dir.to_path_buf());
        if let Some(venv) =
            crate::dependencies::nearest_virtual_env(&policy.virtual_envs, &command_dir)
        {
            activate_virtual_env(&mut cmd, venv);
        }
        // The user's cargo settings without the user's cargo secrets
        // (`cargo_home`, decision 0084).
        #[cfg(unix)]
        if let Some(tmp) = &policy.tmp_dir {
            let user = policy
                .user_cargo_home
                .clone()
                .or_else(|| cargo_home::user_cargo_home(resolved.home.as_deref()));
            if let Some(user) = user {
                let tmp = resolve(tmp)?;
                // The deepest writable root holding it (checked in `resolved`).
                let root = resolved
                    .writable
                    .iter()
                    .filter(|r| tmp.starts_with(r))
                    .max_by_key(|r| r.components().count())
                    .ok_or_else(|| {
                        SandboxError::InvalidPolicy(format!(
                            "tmp dir {} is not under a writable root",
                            tmp.display()
                        ))
                    })?;
                let prepared = cargo_home::prepare(&user, root, &tmp)?;
                if let Some(home) = prepared.home {
                    cmd.env("CARGO_HOME", home);
                }
                refused_reads.extend(prepared.notes);
            }
        }
        // The Gradle wrapper's distribution, reachable from the command's own
        // Gradle home (`crate::gradle_wrapper`): `./gradlew` runs offline,
        // and writes its lock there, never in the user's home.
        #[cfg(any(unix, windows))]
        if let (Some(tmp), Some(home)) = (&policy.tmp_dir, resolved.home.as_deref()) {
            let tmp = resolve(tmp)?;
            if let Some(root) = resolved
                .writable
                .iter()
                .filter(|r| tmp.starts_with(r))
                .max_by_key(|r| r.components().count())
                && let Err(e) =
                    crate::gradle_wrapper::prepare(&home.join(".gradle"), root, &tmp, &command_dir)
            {
                refused_reads.push(format!(
                    "the Gradle wrapper's distribution could not be made reachable in the \
                     command's Gradle home: {e}"
                ));
            }
        }
        if let Some(tmp) = &policy.tmp_dir {
            cmd.env("TMPDIR", tmp).env("TMP", tmp).env("TEMP", tmp);
            // ~/.npmrc holds registry tokens and is read-denied on macOS.
            // npm reads its user config from here instead, so it neither
            // sees the tokens nor trips over the denied path.
            let npmrc = tmp.join(".npmrc");
            // `create_new` (O_CREAT|O_EXCL) never follows a link, dangling or
            // not: the tmp dir is sandbox-writable, and a `.npmrc` an earlier
            // process left as a link to a path outside must not make the host
            // create that path (decision 0083). Whatever is there is left.
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&npmrc)
            {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(source) => {
                    return Err(SandboxError::Io {
                        path: npmrc.clone(),
                        source,
                    });
                }
            }
            cmd.env("NPM_CONFIG_USERCONFIG", &npmrc);
            // ~/.npm is outside the writable roots; without this npm fails on
            // its first cache write. A per-workspace cache is a cold cache.
            cmd.env("NPM_CONFIG_CACHE", tmp.join("npm-cache"));
            // The go command writes its build cache on every build and test,
            // and stops when it cannot ("failed to initialize build cache"):
            // its default, under ~/Library/Caches or ~/.cache, is outside the
            // writable roots. Cargo's build output already goes to scratch
            // (`CARGO_TARGET_DIR`); a per-workspace cache is a cold cache.
            cmd.env("GOCACHE", tmp.join("go-build"));
            package_store_env(&mut cmd, tmp, resolved.home.as_deref(), policy.network);
        }
        // Windows cannot deny reads of the Credential Manager, so at least stop
        // git from asking it by default: an empty credential.helper resets the
        // helper list (git-config(1), GIT_CONFIG_COUNT). A process that calls
        // the helper directly still reaches it; STRENGTH says so. If the host
        // already sets GIT_CONFIG_COUNT, append at index N and bump the count,
        // rather than dropping the reset.
        #[cfg(windows)]
        {
            let next = match std::env::var("GIT_CONFIG_COUNT") {
                Ok(v) => v.trim().parse::<u32>().ok(),
                Err(std::env::VarError::NotPresent) => Some(0),
                // A non-UTF-8 or otherwise unreadable count: fail loud rather
                // than guess an index and leave the helper reachable (fail loud).
                Err(_) => {
                    return Err(SandboxError::InvalidPolicy(
                        "GIT_CONFIG_COUNT is set to a value that is not valid UTF-8".into(),
                    ));
                }
            };
            match next {
                Some(n) => {
                    cmd.env(format!("GIT_CONFIG_KEY_{n}"), "credential.helper")
                        .env(format!("GIT_CONFIG_VALUE_{n}"), "")
                        .env("GIT_CONFIG_COUNT", (n + 1).to_string());
                }
                None => {
                    return Err(SandboxError::InvalidPolicy(
                        "GIT_CONFIG_COUNT is set to a value that is not a number".into(),
                    ));
                }
            }
        }
        Ok(SandboxedCommand {
            cmd,
            refused_reads,
            #[cfg(unix)]
            probe,
            #[cfg(windows)]
            prepared,
            #[cfg(windows)]
            stdio: [None; 3],
        })
    }
}

/// Convenience: a confined command in `ws` under the standard worker policy.
pub fn workspace_command(
    ws: &Workspace,
    network: Network,
    program: impl AsRef<OsStr>,
) -> Result<SandboxedCommand, SandboxError> {
    sandboxed_command(
        &SandboxPolicy::for_workspace(ws, network),
        program,
        ws.root(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With `HOME` unset the home comes from the password
    /// database, so the rules that hang off it do not silently vanish.
    #[cfg(unix)]
    #[test]
    fn the_password_database_names_this_users_home() {
        let home = passwd_home().expect("a home in the password database");
        assert!(home.is_absolute(), "{home:?}");
    }

    #[test]
    fn empty_policy_is_rejected() {
        let err = SandboxPolicy::new(vec![], Network::Allow)
            .resolved()
            .unwrap_err();
        assert!(matches!(err, SandboxError::InvalidPolicy(_)));
    }

    #[test]
    fn relative_and_root_paths_are_rejected() {
        let rel = SandboxPolicy::new(vec![PathBuf::from("rel")], Network::Allow);
        assert!(matches!(
            rel.resolved(),
            Err(SandboxError::InvalidPolicy(_))
        ));
        let root = SandboxPolicy::new(vec![PathBuf::from("/")], Network::Allow);
        assert!(matches!(
            root.resolved(),
            Err(SandboxError::InvalidPolicy(_))
        ));
    }

    /// A dependency tree beneath a read-denied path (kGraf's own state) is
    /// never granted: the policy is refused rather than reopening the state.
    #[test]
    fn a_dependency_tree_beneath_a_denied_path_is_refused() {
        let work = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let state = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(state.join("x/node_modules")).unwrap();
        let links = crate::DependencyLinks {
            reads: vec![state.join("x/node_modules")],
            ..crate::DependencyLinks::default()
        };
        let policy = SandboxPolicy::new(vec![work.path().to_path_buf()], Network::Deny)
            .deny_read(state.clone())
            .with_dependency_links(&links);
        assert!(matches!(
            policy.resolved(),
            Err(SandboxError::InvalidPolicy(why)) if why.contains("beneath a path whose reads are denied")
        ));
    }

    /// A dependency read that is a link, or lies below one, is refused when
    /// the sandbox is built: resolving it would grant the link's target.
    #[cfg(unix)]
    #[test]
    fn a_dependency_read_through_a_link_is_refused() {
        let work = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("pkg")).unwrap();
        let c = std::fs::canonicalize(checkout.path()).unwrap();
        std::os::unix::fs::symlink(outside.path(), c.join("node_modules")).unwrap();
        for read in [c.join("node_modules"), c.join("node_modules/pkg")] {
            let links = crate::DependencyLinks {
                reads: vec![read.clone()],
                ..crate::DependencyLinks::default()
            };
            let policy = SandboxPolicy::new(vec![work.path().to_path_buf()], Network::Deny)
                .with_dependency_links(&links);
            assert!(
                matches!(
                    policy.resolved(),
                    Err(SandboxError::InvalidPolicy(why)) if why.contains("link")
                ),
                "{} was granted",
                read.display()
            );
        }
    }

    /// The root virtual environment is activated: its `bin` first on `PATH`,
    /// `VIRTUAL_ENV` naming it, no `PYTHONHOME`.
    #[test]
    fn a_virtual_environment_is_activated_for_the_command() {
        let mut cmd = Command::new("true");
        cmd.env("PYTHONHOME", "/elsewhere");
        activate_virtual_env(&mut cmd, Path::new("/t/.venv"));
        let envs: Vec<(OsString, Option<OsString>)> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(OsStr::to_owned)))
            .collect();
        let get = |k: &str| {
            envs.iter()
                .find(|(n, _)| n == k)
                .and_then(|(_, v)| v.clone())
        };
        let path = get("PATH").expect("PATH");
        let first = std::env::split_paths(&path).next().expect("first");
        let bin = if cfg!(windows) { "Scripts" } else { "bin" };
        assert_eq!(first, Path::new("/t/.venv").join(bin));
        assert_eq!(get("VIRTUAL_ENV"), Some(OsString::from("/t/.venv")));
        assert!(envs.iter().any(|(k, v)| k == "PYTHONHOME" && v.is_none()));
    }

    #[test]
    fn tmp_dir_outside_roots_is_rejected() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::new(vec![a.path().to_path_buf()], Network::Allow)
            .with_tmp_dir(b.path().to_path_buf());
        assert!(matches!(
            policy.resolved(),
            Err(SandboxError::InvalidPolicy(_))
        ));
    }

    /// Decision 0083: the tmp dir is a sandbox-writable root, so an earlier
    /// sandboxed process can leave `.npmrc` as a dangling link to a path
    /// outside. Building the next command must not create that path.
    #[cfg(unix)]
    #[test]
    fn a_dangling_npmrc_link_in_the_tmp_dir_creates_nothing_outside() {
        let a = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("created-by-the-host");
        std::os::unix::fs::symlink(&target, a.path().join(".npmrc")).unwrap();
        let policy = SandboxPolicy::new(vec![a.path().to_path_buf()], Network::Deny)
            .with_tmp_dir(a.path().to_path_buf());
        let _ = sandboxed_command(&policy, "true", a.path());
        assert!(
            target.symlink_metadata().is_err(),
            "the host wrote through the link"
        );
    }

    #[test]
    fn scrub_matches_secrets_and_agent_sockets_only() {
        for name in [
            "SSH_AUTH_SOCK",
            "GITHUB_TOKEN",
            "OpenAI_Api_Key",
            "AWS_SECRET_ACCESS_KEY",
            "DB_PASSWORD",
            "DOCKER_HOST",
            // The less obvious needles (a credential store, HTTP auth, a
            // personal access token abbreviation).
            "DB_CREDENTIAL",
            "BASIC_AUTH",
            "GITLAB_PAT",
        ] {
            assert!(is_scrubbed(OsStr::new(name)), "{name} kept");
        }
        for name in [
            "PATH",
            "HOME",
            "CARGO_HOME",
            "LANG",
            "TERM",
            // Each contains a needle as a raw substring ("pat" in "path",
            // "auth" in "author"), and must not be scrubbed for it: the
            // needle match is whole-word, not substring.
            "DB_PATH",
            "AUTHOR_NAME",
        ] {
            assert!(!is_scrubbed(OsStr::new(name)), "{name} scrubbed");
        }
    }

    #[test]
    fn scrub_removes_from_the_command_unless_passed() {
        let mut cmd = Command::new("true");
        let inherited = ["GITHUB_TOKEN", "PATH", "NPM_TOKEN"].map(OsString::from);
        scrub_env(&mut cmd, inherited.into_iter(), &["NPM_TOKEN".to_string()]);
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_owned())
            .collect();
        assert_eq!(removed, vec![OsString::from("GITHUB_TOKEN")]);
    }
}
