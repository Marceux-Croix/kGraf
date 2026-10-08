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
    /// Only TCP to one loopback port, where kGraf's egress proxy listens
    /// ([`crate::egress::EgressProxy`]); every other address, UDP, and DNS
    /// are denied. The proxy decides which hosts are reached.
    ///
    /// **macOS**: Seatbelt allows `connect()` to `localhost:port` and no
    /// other IP destination; the proxy listens on the host's loopback.
    /// **Linux**: the command gets a network namespace of its own, whose only
    /// interface is its loopback. Before the command starts, its first process
    /// opens a listening socket on `127.0.0.1:port` inside that namespace and
    /// hands it to kGraf, which serves the proxy on it from outside
    /// ([`SandboxedCommand::take_loopback_listener`]). Needs user namespaces;
    /// without them the policy is refused. **Windows** cannot confine the
    /// network and refuses it.
    Proxy { port: u16 },
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
    /// Folders beneath a writable root the command can neither read nor
    /// write, apart from writable roots beneath them ([`Self::hide`]).
    hidden: Vec<PathBuf>,
    /// Package stores kGraf filled for this project, read-only
    /// ([`Self::with_dependency_links`]).
    package_stores: crate::PackageStores,
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
            hidden: Vec::new(),
            package_stores: crate::PackageStores::default(),
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
        // The stores kGraf filled for this project (`crate::PackageStores`):
        // readable roots (each stays readable beneath the denied state
        // directory), never writable, and named to their package manager.
        let stores = &links.package_stores;
        let pick = |mine: &mut Option<PathBuf>, found: &Option<PathBuf>| {
            if found.is_some() {
                mine.clone_from(found);
            }
        };
        pick(&mut self.package_stores.go, &stores.go);
        pick(&mut self.package_stores.maven, &stores.maven);
        pick(&mut self.package_stores.gradle_home, &stores.gradle_home);
        pick(&mut self.package_stores.nuget, &stores.nuget);
        self
    }

    /// Hide `dir`, a folder beneath a writable root, from the command: it can
    /// neither read nor write anything in it, apart from writable roots that
    /// lie beneath it. kGraf's own install of a project's dependencies runs
    /// with the checkout writable and its state directory (`.kgraf`: consents,
    /// permission rules, transcripts) hidden this way, except the package
    /// stores inside it ([`crate::PackageStores`]).
    ///
    /// **macOS**: reads and writes of `dir` are denied, and the writable roots
    /// beneath it allowed again after that. **Linux**: an empty, read-only
    /// file system is mounted on `dir` in the command's mount namespace, and
    /// each writable root beneath it is mounted again on top (needs user
    /// namespaces; refused without). **Windows**: writes are denied by an
    /// access entry for the capability of each writable root above `dir`;
    /// reads are not confined there.
    ///
    /// `dir` must exist and lie beneath a writable root (checked when the
    /// command is built).
    pub fn hide(mut self, dir: PathBuf) -> Self {
        self.hidden.push(dir);
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
    /// A host git configuration entry (`is_git_config_entry_var`) cannot be
    /// kept: the command is refused.
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
    /// Whether [`Self::accept_partial_isolation`] opted in.
    pub fn accepts_partial_isolation(&self) -> bool {
        self.accept_partial
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
        let mut hidden = Vec::new();
        for dir in &self.hidden {
            let dir = resolve(dir)?;
            if !writable.iter().any(|r| dir.starts_with(r) && *r != dir) {
                return Err(SandboxError::InvalidPolicy(format!(
                    "hidden folder {} is not beneath a writable root",
                    dir.display()
                )));
            }
            let keep: Vec<PathBuf> = writable
                .iter()
                .filter(|r| r.starts_with(&dir) && **r != dir)
                .cloned()
                .collect();
            hidden.push(Hidden { dir, keep });
        }
        // Each store is a canonical folder `PackageStores::find` checked; one
        // that now resolves elsewhere is refused, as a dependency tree is.
        let mut readable = resolve_all(&self.readable_roots)?;
        for store in self.package_stores.all() {
            let now = resolve(store)?;
            if now != store {
                return Err(SandboxError::InvalidPolicy(format!(
                    "package store {} now resolves to {}; it is not granted",
                    store.display(),
                    now.display()
                )));
            }
            if !readable.contains(&now) {
                readable.push(now);
            }
        }
        let home = home_dir();
        Ok(Resolved {
            writable,
            readable,
            read_denied,
            dependency_reads,
            // Sockets may not exist yet; resolve the parent directory.
            unix_sockets: self
                .unix_sockets
                .iter()
                .map(|p| resolve_socket(p))
                .collect::<Result<_, _>>()?,
            network: self.network,
            toolchain: toolchain_reads(home.as_deref()),
            home,
            ancestor_limit: self.ancestor_limit.as_deref().map(resolve).transpose()?,
            cargo_homes: cargo_homes(),
            hidden,
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
    /// Toolchain folders named by `$RUSTUP_HOME` and `$CARGO_HOME` (Linux).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    toolchain: ToolchainReads,
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
    /// See [`SandboxPolicy::hide`]: canonical, each beneath a writable root.
    hidden: Vec<Hidden>,
}

/// A folder hidden from the command ([`SandboxPolicy::hide`]), and the
/// writable roots beneath it that stay usable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", windows)),
    allow(dead_code)
)]
struct Hidden {
    dir: PathBuf,
    keep: Vec<PathBuf>,
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
        if let Ok(real) = kgraf_core::plain_path::canonicalize(cur) {
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
    kgraf_core::plain_path::canonicalize(path).map_err(|source| SandboxError::Io {
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
    home_dir_from(std::env::var_os("HOME"), passwd_home)
}

/// [`home_dir`] with `$HOME` and the password database lookup given.
fn home_dir_from(
    home_env: Option<OsString>,
    passwd: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    home_env
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(passwd)
        .and_then(|h| kgraf_core::plain_path::canonicalize(h).ok())
}

/// The cargo homes ancestor reads never enter ([`Resolved::cargo_homes`]).
/// A home that does not exist is kept as written: no file lies beneath it.
fn cargo_homes() -> Option<Vec<PathBuf>> {
    cargo_homes_from(
        std::env::var_os("HOME"),
        passwd_home(),
        std::env::var_os("CARGO_HOME"),
    )
}

/// [`cargo_homes`] with its three inputs given, so a test needs no
/// environment change: `$HOME`, the password database's home and
/// `$CARGO_HOME`. An empty `$HOME` or `$CARGO_HOME` counts as unset.
fn cargo_homes_from(
    home_env: Option<OsString>,
    passwd: Option<PathBuf>,
    cargo_home_env: Option<OsString>,
) -> Option<Vec<PathBuf>> {
    let canonical = |p: PathBuf| kgraf_core::plain_path::canonicalize(&p).unwrap_or(p);
    let homes: Vec<PathBuf> = [
        home_env.filter(|h| !h.is_empty()).map(PathBuf::from),
        passwd,
    ]
    .into_iter()
    .flatten()
    .map(|h| canonical(h).join(".cargo"))
    .collect();
    if homes.is_empty() {
        return None;
    }
    let mut out = homes;
    if let Some(cargo_home) = cargo_home_env.filter(|h| !h.is_empty()) {
        out.push(canonical(PathBuf::from(cargo_home)));
    }
    Some(out)
}

/// Toolchain folders outside the home folder that a command may read, from
/// `$RUSTUP_HOME` and `$CARGO_HOME` (decision 0084).
///
/// Where reads are an allowlist (Linux), the defaults under `$HOME`
/// (`~/.rustup`, `~/.cargo/{bin,registry,git}`) miss a toolchain installed
/// elsewhere (a CI image, a shared machine), and `cargo` fails with
/// "Permission denied". A set variable adds:
///
/// - `$RUSTUP_HOME`, whole. Rustup keeps toolchains, `settings.toml`, the
///   download cache and update hashes there, and no login.
/// - `$CARGO_HOME/bin`, `registry` and `git`, never the folder itself:
///   `credentials.toml` and `config.toml` live in it.
///
/// A value that is not usable is ignored and named in `notes`, which the
/// caller shows beside the command's own error: a relative path, one that
/// does not exist or is not a folder, and one that would grant too much
/// (the root, the home folder or a folder above it, or a rustup home that
/// holds the cargo home). The command then runs with the defaults only.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct ToolchainReads {
    reads: Vec<PathBuf>,
    notes: Vec<String>,
}

/// [`toolchain_reads_from`] with the host environment.
fn toolchain_reads(home: Option<&Path>) -> ToolchainReads {
    toolchain_reads_from(
        std::env::var_os("RUSTUP_HOME"),
        std::env::var_os("CARGO_HOME"),
        home,
    )
}

/// [`ToolchainReads`] for the two variables given, so a test needs no
/// environment change. An empty value counts as unset.
fn toolchain_reads_from(
    rustup_home: Option<OsString>,
    cargo_home: Option<OsString>,
    home: Option<&Path>,
) -> ToolchainReads {
    let mut out = ToolchainReads::default();
    // A folder is too broad when granting it would expose the home folder
    // (equal to it or above it) or everything (the root).
    let too_broad = |dir: &Path| dir.parent().is_none() || home.is_some_and(|h| h.starts_with(dir));
    let folder = |name: &str, value: OsString, notes: &mut Vec<String>| -> Option<PathBuf> {
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            notes.push(format!(
                "${name} is {}, not an absolute path; the toolchain folders it names are not readable",
                path.display()
            ));
            return None;
        }
        match kgraf_core::plain_path::canonicalize(&path) {
            Ok(real) if real.is_dir() && too_broad(&real) => {
                notes.push(format!(
                    "${name} is {}, which holds the root or the home folder; it is not readable",
                    real.display()
                ));
                None
            }
            Ok(real) if real.is_dir() => Some(real),
            _ => {
                notes.push(format!(
                    "${name} is {}, which is not a folder this user can resolve; it is not readable",
                    path.display()
                ));
                None
            }
        }
    };
    let mut cargo_real = None;
    if let Some(value) = cargo_home.filter(|v| !v.is_empty())
        && let Some(real) = folder("CARGO_HOME", value, &mut out.notes)
    {
        for sub in ["bin", "registry", "git"] {
            // A link such as `registry` -> elsewhere is judged at its target.
            if let Ok(target) = kgraf_core::plain_path::canonicalize(real.join(sub))
                && target.is_dir()
                && !too_broad(&target)
            {
                out.reads.push(target);
            }
        }
        cargo_real = Some(real);
    }
    if let Some(value) = rustup_home.filter(|v| !v.is_empty())
        && let Some(real) = folder("RUSTUP_HOME", value, &mut out.notes)
    {
        // Granting a rustup home that holds the cargo home would expose
        // `credentials.toml`.
        if cargo_real.as_ref().is_some_and(|c| c.starts_with(&real)) {
            out.notes.push(format!(
                "$RUSTUP_HOME is {}, which holds $CARGO_HOME; it is not readable",
                real.display()
            ));
        } else {
            out.reads.push(real);
        }
    }
    out
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

/// The sentence a check or gate carries when it ran with the network open
/// because the backend cannot deny it ([`CheckNetwork`]).
pub const NETWORK_OPEN_NOTICE: &str = "network not blocked: the Windows sandbox cannot deny \
    network access without administrator rights (decision 0011); this check ran with the \
    network open";

/// The sentence the worker's shell carries when its commands run with the
/// network open because the backend cannot deny it ([`CheckNetwork`]).
pub const SHELL_NETWORK_OPEN_NOTICE: &str = "network not blocked: the Windows sandbox cannot \
    deny network access without administrator rights (decision 0011); shell commands run with \
    the network open";

/// The line a shell result carries on Windows when a command's output shows
/// the Windows TLS stack (Schannel) refused to start: that stack cannot run
/// inside the sandbox (decision 0011), and tools with their own TLS library
/// can.
pub const TLS_UNAVAILABLE_HINT: &str = "the Windows TLS stack (Schannel) cannot run inside \
    kGraf's sandbox (decision 0011); git and the curl of Git for Windows use OpenSSL here, \
    and tools that bring their own TLS library work";

/// [`TLS_UNAVAILABLE_HINT`] when `output` holds the Schannel refusal
/// (`SEC_E_NO_CREDENTIALS`, status `0x8009030E`), otherwise `None`.
pub fn tls_unavailable_hint(output: &str) -> Option<&'static str> {
    let lower = output.to_ascii_lowercase();
    (lower.contains("sec_e_no_credentials") || lower.contains("0x8009030e"))
        .then_some(TLS_UNAVAILABLE_HINT)
}

/// The network a verifier check, a quality gate, a contract check or a
/// worker's shell command asks for (decision 0011, amended 2026-10-06 and
/// 2026-10-07). One decision, made from the backend's own capability: it is
/// [`Network::Deny`] wherever the backend can deny it or refuse to run
/// (macOS, Linux), and [`Network::Allow`] with a notice only on the Windows
/// restricted-token backend, which cannot deny it and would otherwise run
/// nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckNetwork {
    network: Network,
    notice: Option<&'static str>,
}

impl CheckNetwork {
    /// The decision for one backend's strength. Pure, so it is tested with a
    /// strength value on every platform.
    pub fn for_strength(strength: &IsolationStrength) -> Self {
        if strength.backend == Backend::WindowsRestrictedToken && !strength.network_deniable {
            Self::open_with_notice()
        } else {
            Self::deny()
        }
    }

    /// The decision for this platform. A platform with no backend keeps
    /// [`Network::Deny`], so the sandbox refuses as before.
    pub fn current() -> Self {
        platform_isolation().map_or_else(|_| Self::deny(), |s| Self::for_strength(&s))
    }

    /// What a backend that cannot deny the network gets: the network open and
    /// the notice. Public so a test of a caller can stand in for that backend.
    pub fn open_with_notice() -> Self {
        Self {
            network: Network::Allow,
            notice: Some(NETWORK_OPEN_NOTICE),
        }
    }

    /// What every other backend gets: the network denied, no notice.
    pub fn deny() -> Self {
        Self {
            network: Network::Deny,
            notice: None,
        }
    }

    /// The network to put in the check's [`SandboxPolicy`].
    pub fn network(&self) -> Network {
        self.network
    }

    /// Set where the network is open: the sentence every such run carries.
    pub fn notice(&self) -> Option<&'static str> {
        self.notice
    }

    /// Set where the network is open: the sentence the worker's shell says
    /// ([`SHELL_NETWORK_OPEN_NOTICE`]).
    pub fn shell_notice(&self) -> Option<&'static str> {
        self.notice.map(|_| SHELL_NETWORK_OPEN_NOTICE)
    }

    /// Whether a run under this decision accepts the backend's partial
    /// isolation ([`SandboxPolicy::accept_partial_isolation`]). True exactly
    /// where the network is open with the notice: the Windows restricted-token
    /// backend is always partial (reads and the network are not confined), so
    /// a run there either accepts that, said by the notice and by the
    /// strength's `gaps`, or runs nothing. False everywhere else, so a
    /// partial Linux backend still refuses as before.
    pub fn accepts_partial_isolation(&self) -> bool {
        self.notice.is_some()
    }
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

/// The variables a command with a cleared environment
/// (`SandboxedCommand::env_clear`) still inherits on Windows: what a Windows
/// program needs to start and find its files. Without `SystemRoot`, Winsock
/// and the crypto providers fail to load; without `PATH` and `PATHEXT`,
/// programs and their DLLs are not found; without the profile folders, git,
/// cargo and npm do not find their configuration. None holds a credential
/// (a unit test checks each name against the credential test the scrub
/// uses). `TMP` and `TEMP` are not here: the sandbox sets its own.
pub const WINDOWS_BASE_VARS: &[&str] = &[
    "ALLUSERSPROFILE",
    "APPDATA",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "CommonProgramW6432",
    "ComSpec",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "NUMBER_OF_PROCESSORS",
    "OS",
    "PATH",
    "PATHEXT",
    "PROCESSOR_ARCHITECTURE",
    "PROCESSOR_ARCHITEW6432",
    "PROCESSOR_IDENTIFIER",
    "PROCESSOR_LEVEL",
    "PROCESSOR_REVISION",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "PUBLIC",
    "SystemDrive",
    "SystemRoot",
    "USERNAME",
    "USERPROFILE",
    "windir",
];

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

/// Whether an inherited variable carries git configuration through the
/// environment: `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>`,
/// `GIT_CONFIG_VALUE_<n>`, and `GIT_CONFIG_PARAMETERS` (what `git -c` hands
/// to the git processes it starts). Any case and any suffix: Windows
/// compares names without case, and a name git does not read still shows
/// its value to the process.
///
/// A sandboxed command gets none of the host's entries, only kGraf's own
/// ([`worker_git_config_env`]). Their values can carry a credential
/// (`http.<url>.extraHeader` with a bearer token, which is how CI checkouts
/// hand a token to git; `url.<base>.insteadOf` or `remote.<name>.url` with a
/// login; `http.proxy` with a password; `credential.helper` with an inline
/// secret), and the names do not say so. A deny list of keys would miss a
/// key it does not name and every new one git adds, so all are dropped.
pub fn is_git_config_entry_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper == "GIT_CONFIG_COUNT"
        || upper == "GIT_CONFIG_PARAMETERS"
        || upper.starts_with("GIT_CONFIG_KEY_")
        || upper.starts_with("GIT_CONFIG_VALUE_")
}

/// The environment changes that leave a sandboxed command with kGraf's own
/// git configuration entries and none of the host's: each `inherited` name
/// that [`is_git_config_entry_var`] matches is removed (`None`), then `ours`
/// (numbered from zero, with their `GIT_CONFIG_COUNT`) are set. Applied in
/// order, a set after the removal of the same name wins, so the count always
/// equals the number of entries the command sees. Values are never read
/// here: only the names of the inherited variables.
pub(crate) fn worker_git_config_env(
    inherited: impl IntoIterator<Item = OsString>,
    ours: Vec<(String, String)>,
) -> Vec<(OsString, Option<OsString>)> {
    let mut changes: Vec<(OsString, Option<OsString>)> = inherited
        .into_iter()
        .filter(|name| is_git_config_entry_var(&name.to_string_lossy()))
        .map(|name| (name, None))
        .collect();
    changes.extend(
        ours.into_iter()
            .map(|(name, value)| (OsString::from(name), Some(OsString::from(value)))),
    );
    changes
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
        // One component per join: a path with a stray "/" in a Windows path
        // reaches the tool unchanged and is not equal to the path it compares.
        let gradle_cache = home.join(".gradle").join("caches");
        if gradle_cache.join("modules-2").is_dir() {
            cmd.env("GRADLE_RO_DEP_CACHE", gradle_cache);
        }
        let nuget = home.join(".nuget").join("packages");
        if nuget.is_dir() {
            cmd.env("NUGET_PACKAGES", nuget);
        }
    }
}

/// Point each package manager at the store kGraf filled for this project
/// (`crate::PackageStores`), read-only: Go's module cache, the Maven local
/// repository (appended to `MAVEN_OPTS`), Gradle's read-only dependency
/// cache, and NuGet's packages folder. Set after [`package_store_env`], so a
/// store of this project wins over the user's own.
fn package_stores_env(cmd: &mut Command, stores: &crate::PackageStores) {
    if let Some(go) = &stores.go {
        cmd.env("GOMODCACHE", go);
    }
    if let Some(maven) = &stores.maven {
        let mut opts = std::env::var_os("MAVEN_OPTS").unwrap_or_default();
        if !opts.is_empty() {
            opts.push(" ");
        }
        opts.push("-Dmaven.repo.local=");
        // `mvn` splits MAVEN_OPTS on white space.
        opts.push(crate::jvm_path(maven));
        cmd.env("MAVEN_OPTS", opts);
    }
    if let Some(gradle) = &stores.gradle_home {
        cmd.env("GRADLE_RO_DEP_CACHE", gradle.join("caches"));
    }
    if let Some(nuget) = &stores.nuget {
        cmd.env("NUGET_PACKAGES", nuget);
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
    /// [`Network::Proxy`] on Linux: where the loopback listener arrives
    /// ([`Self::take_loopback_listener`]).
    #[cfg(target_os = "linux")]
    handoff: Option<linux::Handoff>,
    #[cfg(unix)]
    probe: child::SetupProbe,
    #[cfg(windows)]
    prepared: windows::Prepared,
    /// Explicit stdin, stdout, stderr; `None` takes the per-method default.
    #[cfg(windows)]
    stdio: [Option<Redirect>; 3],
    /// False after [`Self::env_clear`]: the command then inherits only
    /// [`WINDOWS_BASE_VARS`] from this process.
    #[cfg(windows)]
    inherit_env: bool,
    /// What [`Self::env_clear`] sets again on Windows: the sandbox's `TMP`
    /// and `TEMP`, and its git and curl settings numbered from zero.
    #[cfg(windows)]
    kept_on_clear: Vec<(String, OsString)>,
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

    /// Inherit nothing from this process's environment and drop every
    /// variable set on the command so far, as `Command::env_clear` does; set
    /// what the command needs afterwards with [`Self::env`]. For a command
    /// that must see no credential and no name it was not given.
    ///
    /// On Windows a program cannot start without some variables, so the
    /// command still inherits [`WINDOWS_BASE_VARS`] (none holds a
    /// credential), and the sandbox sets its own `TMP` and `TEMP` and its git
    /// and curl settings again: the reset of git's credential helper keeps
    /// git from asking the Credential Manager, which this backend cannot
    /// make unreadable. They are numbered from zero, as without a clear: the
    /// host's own `GIT_CONFIG_*` entries never reach a sandboxed command.
    pub fn env_clear(&mut self) -> &mut Self {
        self.cmd.env_clear();
        #[cfg(windows)]
        {
            self.inherit_env = false;
            for (key, value) in &self.kept_on_clear {
                self.cmd.env(key, value);
            }
        }
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
            self.inherit_env,
        )
    }

    pub fn output(&mut self) -> Result<Output, SandboxError> {
        #[cfg(windows)]
        {
            windows::output(&self.cmd, &self.prepared, self.stdio, self.inherit_env)
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
            windows::status(&self.cmd, &self.prepared, self.stdio, self.inherit_env)
        }
        #[cfg(not(windows))]
        {
            let result = self.cmd.status();
            result.map_err(|e| self.explain(e))
        }
    }

    /// [`Network::Proxy`] on Linux, after [`Self::spawn`]: the listening
    /// socket the command's first process opened on its own namespace's
    /// loopback, for kGraf's egress proxy to accept on
    /// ([`crate::egress::EgressProxy::serve`]). `None` for any other network
    /// and on other platforms, where the proxy listens on the host's
    /// loopback itself. Taken once; waits up to `wait` for the listener.
    pub fn take_loopback_listener(
        &mut self,
        wait: std::time::Duration,
    ) -> Option<Result<std::net::TcpListener, SandboxError>> {
        #[cfg(target_os = "linux")]
        {
            self.handoff.take().map(|mut h| h.receive(wait))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = wait;
            None
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
        // A policy that asks to pass one of the host's git configuration
        // entries is refused, not silently ignored: the sandbox removes them
        // all (`is_git_config_entry_var`).
        if let Some(name) = policy
            .pass_env
            .iter()
            .find(|name| is_git_config_entry_var(name))
        {
            return Err(SandboxError::InvalidPolicy(format!(
                "{name} cannot be passed: a sandboxed command gets only the sandbox's own \
                 git configuration entries"
            )));
        }
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
        let (mut cmd, setup, mut refused_reads, handoff) = {
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
        let command_dir = kgraf_core::plain_path::canonicalize(command_dir)
            .unwrap_or_else(|_| command_dir.to_path_buf());
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
            // The project's own Gradle store, when kGraf's install filled it,
            // holds the distribution that install downloaded.
            let gradle_dir = policy
                .package_stores
                .gradle_home
                .clone()
                .unwrap_or_else(|| home.join(".gradle"));
            if let Some(root) = resolved
                .writable
                .iter()
                .filter(|r| tmp.starts_with(r))
                .max_by_key(|r| r.components().count())
                && let Err(e) =
                    crate::gradle_wrapper::prepare(&gradle_dir, root, &tmp, &command_dir)
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
        // Git settings for the worker's own git, through `GIT_CONFIG_COUNT`
        // (git-config(1)), numbered from zero. The host's own entries do not
        // reach the worker: their values can carry a credential
        // (`is_git_config_entry_var`), and a worker gets none. Set here,
        // before a caller's own `env` calls, so a caller can still override
        // any of them. Only the names of the host's variables are read.
        //
        // Every platform: the line-ending settings kGraf's own git uses
        // (`kgraf_core::host_git::LINE_ENDINGS`), so a `git checkout`,
        // `git stash` or `git add` by the worker writes and stages the same
        // bytes as kGraf's checkout and merge-back do, whatever the machine's
        // `core.autocrlf` says.
        //
        // Windows also: Windows cannot deny reads of the Credential Manager,
        // so at least stop git from asking it by default: an empty
        // credential.helper resets the helper list. A process that calls the
        // helper directly still reaches it; STRENGTH says so. With the
        // network open, the same count also carries
        // `http.sslBackend=openssl`: the Windows TLS stack cannot run under
        // this token (see the Schannel section in `windows.rs`), and Git for
        // Windows ships OpenSSL beside it. `CURL_SSL_BACKEND=openssl` does
        // the same for the multi-backend curl of Git for Windows.
        #[cfg(windows)]
        let ours = windows::tool_env(network_open(policy.network));
        #[cfg(not(windows))]
        let ours = git_config_vars(kgraf_core::host_git::LINE_ENDINGS);
        for (name, value) in worker_git_config_env(std::env::vars_os().map(|(k, _)| k), ours) {
            match value {
                Some(value) => cmd.env(name, value),
                None => cmd.env_remove(name),
            };
        }
        // The stores kGraf filled for this project, read-only, used in place
        // of the user's own (`crate::PackageStores`).
        package_stores_env(&mut cmd, &policy.package_stores);
        Ok(SandboxedCommand {
            cmd,
            refused_reads,
            #[cfg(target_os = "linux")]
            handoff,
            #[cfg(unix)]
            probe,
            #[cfg(windows)]
            prepared,
            #[cfg(windows)]
            stdio: [None; 3],
            #[cfg(windows)]
            inherit_env: true,
            #[cfg(windows)]
            kept_on_clear: windows::kept_on_clear(
                network_open(policy.network),
                policy.tmp_dir.as_deref(),
            ),
        })
    }
}

/// Whether the command runs with the network open: the condition for the
/// sandbox's TLS settings on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
fn network_open(network: Network) -> bool {
    matches!(network, Network::Allow)
}

/// The `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n` variables for `pairs`,
/// numbered from zero, and their count.
pub(crate) fn git_config_entries(pairs: &[(&str, &str)]) -> (Vec<(String, String)>, u32) {
    let mut entries = Vec::with_capacity(pairs.len() * 2);
    let mut n = 0u32;
    for (key, value) in pairs {
        entries.push((format!("GIT_CONFIG_KEY_{n}"), (*key).to_owned()));
        entries.push((format!("GIT_CONFIG_VALUE_{n}"), (*value).to_owned()));
        n += 1;
    }
    (entries, n)
}

/// [`git_config_entries`] for `pairs` and the `GIT_CONFIG_COUNT` that
/// matches them.
pub(crate) fn git_config_vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let (mut vars, count) = git_config_entries(pairs);
    vars.push(("GIT_CONFIG_COUNT".to_owned(), count.to_string()));
    vars
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

    /// The hint fires on the Schannel refusal in either spelling and on
    /// nothing else; it names the decision.
    #[test]
    fn the_tls_hint_fires_on_the_schannel_refusal_only() {
        for text in [
            "curl: (35) schannel: AcquireCredentialsHandle failed: SEC_E_NO_CREDENTIALS (0x8009030E)",
            "The request failed with status 0x8009030e",
            "sec_e_no_credentials",
        ] {
            let hint = tls_unavailable_hint(text).expect(text);
            assert!(hint.contains("0011") && hint.contains("OpenSSL"), "{hint}");
        }
        for text in ["", "curl: (6) Could not resolve host", "0x80090300"] {
            assert_eq!(tls_unavailable_hint(text), None, "{text}");
        }
    }

    /// Only the Windows backend, which cannot deny the network, gets it open
    /// and a notice; every backend that can deny keeps `Deny` and says nothing.
    #[test]
    fn checks_get_the_network_open_only_on_a_backend_that_cannot_deny_it() {
        let windows = windows::STRENGTH;
        assert!(!windows.network_deniable);
        let open = CheckNetwork::for_strength(&windows);
        assert_eq!(open.network(), Network::Allow);
        assert_eq!(open.notice(), Some(NETWORK_OPEN_NOTICE));
        assert!(NETWORK_OPEN_NOTICE.contains("network not blocked"));
        assert!(NETWORK_OPEN_NOTICE.contains("decision 0011"));
        // The same backend, were it able to deny: nothing changes.
        let deniable = IsolationStrength {
            network_deniable: true,
            ..windows
        };
        assert_eq!(CheckNetwork::for_strength(&deniable), CheckNetwork::deny());
        // Another backend that cannot deny stays refused, as before: Linux
        // without its network namespace is not changed by this.
        let linux = IsolationStrength {
            backend: Backend::LinuxLandlock,
            network_deniable: false,
            ..windows
        };
        let decided = CheckNetwork::for_strength(&linux);
        assert_eq!(decided.network(), Network::Deny);
        assert_eq!(decided.notice(), None);
    }

    /// The worker's shell follows the same decision: its own sentence, and
    /// the partial isolation accepted, only where the network is open; a
    /// backend that denies keeps refusing a partial isolation.
    #[test]
    fn the_shell_notice_and_partial_isolation_follow_the_open_network() {
        let open = CheckNetwork::for_strength(&windows::STRENGTH);
        assert_eq!(open.shell_notice(), Some(SHELL_NETWORK_OPEN_NOTICE));
        assert!(open.accepts_partial_isolation());
        assert!(SHELL_NETWORK_OPEN_NOTICE.contains("network not blocked"));
        assert!(SHELL_NETWORK_OPEN_NOTICE.contains("shell commands run with the network open"));
        let linux = IsolationStrength {
            backend: Backend::LinuxLandlock,
            network_deniable: false,
            ..windows::STRENGTH
        };
        for denied in [CheckNetwork::deny(), CheckNetwork::for_strength(&linux)] {
            assert_eq!(denied.shell_notice(), None);
            assert!(!denied.accepts_partial_isolation());
        }
    }

    /// On this host the decision is the platform's: macOS and Linux keep
    /// the network denied for checks.
    #[cfg(unix)]
    #[test]
    fn checks_keep_the_network_denied_on_unix() {
        assert_eq!(CheckNetwork::current(), CheckNetwork::deny());
    }

    /// With `HOME` unset the home comes from the password
    /// database, so the rules that hang off it do not silently vanish.
    #[cfg(unix)]
    #[test]
    fn the_password_database_names_this_users_home() {
        let home = passwd_home().expect("a home in the password database");
        assert!(home.is_absolute(), "{home:?}");
    }

    /// The policy reports the writable roots it was given.
    #[test]
    fn the_policy_reports_its_writable_roots() {
        let roots = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let policy = SandboxPolicy::new(roots.clone(), Network::Deny);
        assert_eq!(policy.writable_roots(), roots.as_slice());
        let more = policy.with_writable_root(PathBuf::from("/c"));
        assert_eq!(more.writable_roots().len(), 3);
        assert_eq!(more.writable_roots()[2], PathBuf::from("/c"));
    }

    /// The policy reports whether it opted in to partial isolation.
    #[test]
    fn the_policy_reports_the_partial_isolation_opt_in() {
        let policy = SandboxPolicy::new(vec![PathBuf::from("/a")], Network::Deny);
        assert!(!policy.accepts_partial_isolation());
        let policy = policy.accept_partial_isolation(true);
        assert!(policy.accepts_partial_isolation());
        assert!(
            !policy
                .accept_partial_isolation(false)
                .accepts_partial_isolation()
        );
    }

    /// `$HOME` first (empty counts as unset), then the password database;
    /// the result is canonical, and a home that does not exist is no home.
    #[cfg(unix)]
    #[test]
    fn the_home_is_the_environment_then_the_password_database() {
        let base = tempfile::tempdir().unwrap();
        let real = kgraf_core::plain_path::canonicalize(base.path()).unwrap();
        let env_home = real.join("env");
        let pw_home = real.join("pw");
        std::fs::create_dir(&env_home).unwrap();
        std::fs::create_dir(&pw_home).unwrap();
        let pw = || Some(pw_home.clone());
        let got = home_dir_from(Some(env_home.clone().into_os_string()), pw);
        assert_eq!(got, Some(env_home.clone()));
        assert_eq!(
            home_dir_from(Some(OsString::new()), pw),
            Some(pw_home.clone())
        );
        assert_eq!(home_dir_from(None, pw), Some(pw_home.clone()));
        assert_eq!(home_dir_from(None, || None), None);
        // The environment wins without asking the password database.
        let asked = std::cell::Cell::new(false);
        let _ = home_dir_from(Some(env_home.clone().into_os_string()), || {
            asked.set(true);
            None
        });
        assert!(!asked.get());
        // A link is resolved; a missing path yields nothing.
        let link = real.join("link");
        std::os::unix::fs::symlink(&pw_home, &link).unwrap();
        assert_eq!(
            home_dir_from(Some(link.into_os_string()), || None),
            Some(pw_home)
        );
        let gone = env_home.join("missing");
        assert_eq!(home_dir_from(Some(gone.into_os_string()), || None), None);
    }

    /// `$RUSTUP_HOME` is granted whole, and `$CARGO_HOME` by its `bin`,
    /// `registry` and `git` only, never as a folder (`credentials.toml`).
    #[cfg(unix)]
    #[test]
    fn the_toolchain_variables_grant_rustup_whole_and_cargo_by_subfolder() {
        let base = tempfile::tempdir().unwrap();
        let real = kgraf_core::plain_path::canonicalize(base.path()).unwrap();
        let (home, rustup, cargo) = (real.join("h"), real.join("r"), real.join("c"));
        for d in [
            &home,
            &rustup,
            &cargo.join("bin"),
            &cargo.join("registry"),
            &cargo.join("git"),
            &cargo.join("other"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        let got = toolchain_reads_from(
            Some(rustup.clone().into_os_string()),
            Some(cargo.clone().into_os_string()),
            Some(&home),
        );
        assert_eq!(got.notes, Vec::<String>::new());
        assert_eq!(
            got.reads,
            vec![
                cargo.join("bin"),
                cargo.join("registry"),
                cargo.join("git"),
                rustup.clone()
            ]
        );
        assert!(!got.reads.contains(&cargo));
        // A missing `git` folder is skipped without a note; unset and empty
        // variables grant nothing.
        std::fs::remove_dir(cargo.join("git")).unwrap();
        let some = toolchain_reads_from(None, Some(cargo.clone().into_os_string()), Some(&home));
        assert_eq!(some.reads, vec![cargo.join("bin"), cargo.join("registry")]);
        assert!(some.notes.is_empty());
        let none = toolchain_reads_from(Some(OsString::new()), Some(OsString::new()), Some(&home));
        assert_eq!(none, ToolchainReads::default());
        assert_eq!(
            toolchain_reads_from(None, None, None),
            ToolchainReads::default()
        );
    }

    /// A value that is relative, missing, a file, the root, the home folder,
    /// a folder above it, or a rustup home holding the cargo home grants
    /// nothing and is named.
    #[cfg(unix)]
    #[test]
    fn an_unusable_toolchain_variable_grants_nothing_and_says_why() {
        let base = tempfile::tempdir().unwrap();
        let real = kgraf_core::plain_path::canonicalize(base.path()).unwrap();
        let home = real.join("top/home");
        let cargo = real.join("top/rustup/cargo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cargo.join("bin")).unwrap();
        let file = real.join("file");
        std::fs::write(&file, "x").unwrap();
        let rustup_cases: Vec<(PathBuf, &str)> = vec![
            (PathBuf::from("rel/rustup"), "absolute"),
            (real.join("missing"), "not a folder"),
            (file.clone(), "not a folder"),
            (PathBuf::from("/"), "root or the home"),
            (home.clone(), "root or the home"),
            (real.join("top"), "root or the home"),
        ];
        for (value, why) in rustup_cases {
            let got = toolchain_reads_from(Some(value.clone().into_os_string()), None, Some(&home));
            assert!(got.reads.is_empty(), "{value:?}: {got:?}");
            assert_eq!(got.notes.len(), 1, "{value:?}: {got:?}");
            assert!(got.notes[0].contains("$RUSTUP_HOME"), "{got:?}");
            assert!(got.notes[0].contains(why), "{value:?}: {got:?}");
        }
        let got = toolchain_reads_from(None, Some(OsString::from("/")), Some(&home));
        assert!(got.reads.is_empty(), "{got:?}");
        assert!(got.notes[0].contains("$CARGO_HOME"), "{got:?}");
        // A rustup home above the cargo home would expose its credentials;
        // the cargo subfolders stay granted.
        let got = toolchain_reads_from(
            Some(real.join("top/rustup").into_os_string()),
            Some(cargo.clone().into_os_string()),
            Some(&home),
        );
        assert_eq!(got.reads, vec![cargo.join("bin")]);
        assert!(got.notes[0].contains("holds $CARGO_HOME"), "{got:?}");
        // A `registry` link that leads to the home folder is not followed
        // into a grant.
        std::os::unix::fs::symlink(&home, cargo.join("registry")).unwrap();
        let got = toolchain_reads_from(None, Some(cargo.clone().into_os_string()), Some(&home));
        assert_eq!(got.reads, vec![cargo.join("bin")]);
    }

    /// The toolchain variables reach the command: neither name is scrubbed.
    #[test]
    fn the_toolchain_variables_are_not_scrubbed() {
        assert!(!is_scrubbed(OsStr::new("RUSTUP_HOME")));
        assert!(!is_scrubbed(OsStr::new("CARGO_HOME")));
    }

    /// Every cargo home an ancestor read must never enter: `$HOME/.cargo`,
    /// the password database's `.cargo`, `$CARGO_HOME`; an empty variable is
    /// unset; no home at all gives `None`; a missing home is kept as written.
    #[cfg(unix)]
    #[test]
    fn the_cargo_homes_cover_the_environment_the_database_and_cargo_home() {
        let base = tempfile::tempdir().unwrap();
        let real = kgraf_core::plain_path::canonicalize(base.path()).unwrap();
        let env_home = real.join("env");
        let pw_home = real.join("pw");
        let custom = real.join("custom");
        for d in [&env_home, &pw_home, &custom] {
            std::fs::create_dir(d).unwrap();
        }
        let all = cargo_homes_from(
            Some(env_home.clone().into_os_string()),
            Some(pw_home.clone()),
            Some(custom.clone().into_os_string()),
        );
        assert_eq!(
            all,
            Some(vec![
                env_home.join(".cargo"),
                pw_home.join(".cargo"),
                custom.clone()
            ])
        );
        // Empty variables are unset: the database home alone remains.
        let empty = cargo_homes_from(
            Some(OsString::new()),
            Some(pw_home.clone()),
            Some(OsString::new()),
        );
        assert_eq!(empty, Some(vec![pw_home.join(".cargo")]));
        let only_env = cargo_homes_from(Some(env_home.clone().into_os_string()), None, None);
        assert_eq!(only_env, Some(vec![env_home.join(".cargo")]));
        assert_eq!(cargo_homes_from(None, None, None), None);
        assert_eq!(
            cargo_homes_from(Some(OsString::new()), None, Some(custom.into_os_string())),
            None,
            "a cargo home alone is no home directory"
        );
        // A link is resolved; a missing path stays as written.
        let link = real.join("link");
        std::os::unix::fs::symlink(&pw_home, &link).unwrap();
        let missing = real.join("nope");
        let got = cargo_homes_from(
            Some(link.into_os_string()),
            Some(missing.clone()),
            Some(missing.clone().into_os_string()),
        );
        assert_eq!(
            got,
            Some(vec![
                pw_home.join(".cargo"),
                missing.join(".cargo"),
                missing
            ])
        );
    }

    /// An error other than "already there" while the command's `.npmrc` is
    /// made stops the command; an `.npmrc` already there is left as it is.
    #[cfg(unix)]
    #[test]
    fn a_failure_making_the_npmrc_is_reported_and_an_existing_one_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let root = kgraf_core::plain_path::canonicalize(base.path()).unwrap();
        let tmp = root.join("tmp");
        std::fs::create_dir(&tmp).unwrap();
        let policy = SandboxPolicy::new(vec![root.clone()], Network::Deny)
            .with_tmp_dir(tmp.clone())
            .with_user_cargo_home(root.join("no-cargo-home"));
        std::fs::write(tmp.join(".npmrc"), "keep").unwrap();
        sandboxed_command(&policy, "true", &root).expect("an existing .npmrc is fine");
        assert_eq!(std::fs::read_to_string(tmp.join(".npmrc")).unwrap(), "keep");
        std::fs::remove_file(tmp.join(".npmrc")).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o500)).unwrap();
        let writable_anyway = std::fs::File::create(tmp.join("probe")).is_ok();
        let result = sandboxed_command(&policy, "true", &root);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700)).unwrap();
        if writable_anyway {
            return; // rights that ignore the mode, such as root
        }
        match result {
            Err(SandboxError::Io { path, .. }) => assert_eq!(path, tmp.join(".npmrc")),
            other => panic!("expected an Io error, got {:?}", other.map(|_| ())),
        }
    }

    /// The wrappers read the real environment: a user with a password
    /// database entry has a home and a cargo home beside it.
    #[cfg(unix)]
    #[test]
    fn the_wrappers_find_this_users_home_and_cargo_homes() {
        let home = home_dir().expect("a home");
        assert!(home.is_absolute(), "{home:?}");
        let homes = cargo_homes().expect("cargo homes");
        assert!(
            homes.iter().any(|h| h.ends_with(".cargo")),
            "{homes:?} lists no $HOME/.cargo"
        );
    }

    /// Only Linux has ancestor reads to refuse; elsewhere the list is empty.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn no_ancestor_refusals_off_linux() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::new(vec![dir.path().to_path_buf()], Network::Deny);
        assert_eq!(policy.ancestor_refusals().unwrap(), Vec::<String>::new());
    }

    /// The package managers' stores: Go goes offline when the network is
    /// denied; Gradle and .NET homes are in the command's temporary folder;
    /// the user's caches are named only when they exist.
    #[test]
    fn the_projects_own_stores_are_named_and_readable_never_writable() {
        let checkout = tempfile::tempdir().unwrap();
        let c = kgraf_core::plain_path::canonicalize(checkout.path()).unwrap();
        for rel in ["go", "maven", "gradle", "nuget"] {
            crate::mark_store_complete(&c, rel).unwrap();
        }
        let links = crate::DependencyLinks {
            package_stores: crate::PackageStores::find(&c),
            ..Default::default()
        };
        let tree = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::new(vec![tree.path().to_path_buf()], Network::Deny)
            .with_dependency_links(&links);
        let resolved = policy.resolved().unwrap();
        let root = crate::package_stores_root(&c);
        for rel in ["go", "maven", "gradle", "nuget"] {
            assert!(resolved.readable.contains(&root.join(rel)), "{rel}");
            assert!(!resolved.writable.iter().any(|w| w.starts_with(&root)));
        }
        let mut cmd = Command::new("true");
        package_stores_env(&mut cmd, &policy.package_stores);
        let env: std::collections::BTreeMap<String, String> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(env["GOMODCACHE"], root.join("go").to_string_lossy());
        assert_eq!(
            env["GRADLE_RO_DEP_CACHE"],
            root.join("gradle").join("caches").to_string_lossy()
        );
        assert_eq!(env["NUGET_PACKAGES"], root.join("nuget").to_string_lossy());
        assert!(
            env["MAVEN_OPTS"].ends_with(&format!(
                "-Dmaven.repo.local={}",
                crate::jvm_path(&root.join("maven")).display()
            )),
            "{}",
            env["MAVEN_OPTS"]
        );
    }

    #[test]
    fn package_store_variables_follow_the_network_and_the_home() {
        let env_of = |network: Network, home: Option<&Path>| {
            let mut cmd = Command::new("true");
            package_store_env(&mut cmd, Path::new("/tmp/x"), home, network);
            cmd.get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let deny = env_of(Network::Deny, None);
        assert_eq!(deny["GOPROXY"].as_deref(), Some("off"));
        // `join` writes the native separator, so build the expected value the same way.
        let under = |leaf: &str| {
            Path::new("/tmp/x")
                .join(leaf)
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(
            deny["GRADLE_USER_HOME"].as_deref(),
            Some(under("gradle-home").as_str())
        );
        assert_eq!(
            deny["DOTNET_CLI_HOME"].as_deref(),
            Some(under("dotnet-home").as_str())
        );
        assert_eq!(deny["DOTNET_CLI_TELEMETRY_OPTOUT"].as_deref(), Some("1"));
        assert!(!deny.contains_key("GRADLE_RO_DEP_CACHE"));
        assert!(!deny.contains_key("NUGET_PACKAGES"));
        assert!(!env_of(Network::Allow, None).contains_key("GOPROXY"));
        let home = tempfile::tempdir().unwrap();
        let empty = env_of(Network::Allow, Some(home.path()));
        assert!(!empty.contains_key("GRADLE_RO_DEP_CACHE"));
        assert!(!empty.contains_key("NUGET_PACKAGES"));
        std::fs::create_dir_all(home.path().join(".gradle/caches/modules-2")).unwrap();
        std::fs::create_dir_all(home.path().join(".nuget/packages")).unwrap();
        let full = env_of(Network::Allow, Some(home.path()));
        let cache = home
            .path()
            .join(".gradle")
            .join("caches")
            .to_string_lossy()
            .into_owned();
        let nuget = home
            .path()
            .join(".nuget")
            .join("packages")
            .to_string_lossy()
            .into_owned();
        assert_eq!(full["GRADLE_RO_DEP_CACHE"].as_deref(), Some(cache.as_str()));
        assert_eq!(full["NUGET_PACKAGES"].as_deref(), Some(nuget.as_str()));
    }

    /// Each redirect applies to its own stream: stdout and stdin set to null
    /// leave stderr piped.
    #[cfg(target_os = "macos")]
    #[test]
    fn each_redirect_applies_to_its_own_stream() {
        let dir = tempfile::tempdir().unwrap();
        let root = kgraf_core::plain_path::canonicalize(dir.path()).unwrap();
        let policy = SandboxPolicy::new(vec![root.clone()], Network::Deny);
        let mut cmd = sandboxed_command(&policy, "/bin/sh", &root).unwrap();
        // `output` pipes a stream nobody set, so each stream is set here and
        // the stream set last must not undo another: stderr first, then the
        // two that are discarded.
        cmd.args(["-c", "echo out; echo err >&2; cat; echo after"])
            .stderr(Redirect::Piped)
            .stdin(Redirect::Null)
            .stdout(Redirect::Null);
        let out = cmd.output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
    }

    /// A user cargo setting withheld for a login in its URL is reported with
    /// the command.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_withheld_cargo_setting_is_reported_with_the_command() {
        let work = tempfile::tempdir().unwrap();
        let root = kgraf_core::plain_path::canonicalize(work.path()).unwrap();
        let cargo = root.join("cargo-home");
        std::fs::create_dir_all(&cargo).unwrap();
        std::fs::write(
            cargo.join("config.toml"),
            "[registries.priv]\nindex = \"https://user:secret@example.com/index\"\n",
        )
        .unwrap();
        let tmp = root.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let policy = SandboxPolicy::new(vec![root.clone()], Network::Deny)
            .with_tmp_dir(tmp)
            .with_user_cargo_home(cargo);
        let cmd = sandboxed_command(&policy, "/usr/bin/true", &root).unwrap();
        assert_eq!(cmd.refused_reads().len(), 1, "{:?}", cmd.refused_reads());
        assert!(cmd.refused_reads()[0].contains("carries a login"));
        let plain = SandboxPolicy::new(vec![root.clone()], Network::Deny);
        let cmd = sandboxed_command(&plain, "/usr/bin/true", &root).unwrap();
        assert!(cmd.refused_reads().is_empty());
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
        let state = kgraf_core::plain_path::canonicalize(dir.path()).unwrap();
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
        let c = kgraf_core::plain_path::canonicalize(checkout.path()).unwrap();
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
            // The settings a host hands git through its environment: the
            // count, the key names and the values travel together. A key
            // name is the name of a git setting, not a credential.
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "git_config_key_12",
            "GIT_CONFIG_VALUE_0",
        ] {
            assert!(!is_scrubbed(OsStr::new(name)), "{name} scrubbed");
        }
        // Only the numbered form is exempt.
        for name in ["GIT_CONFIG_KEY_", "GIT_CONFIG_KEY_X", "MY_GIT_CONFIG_KEY_0"] {
            assert!(is_scrubbed(OsStr::new(name)), "{name} kept");
        }
    }

    /// Every variable that hands git configuration through the environment
    /// is a host entry, in any case; the variables that only locate a config
    /// file, and look-alike names, are not.
    #[test]
    fn git_config_entry_variables_are_the_count_keys_values_and_parameters() {
        for name in [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_12",
            "git_config_value_1",
            "Git_Config_Key_3",
            "GIT_CONFIG_KEY_X",
            "GIT_CONFIG_PARAMETERS",
        ] {
            assert!(is_git_config_entry_var(name), "{name}");
        }
        for name in [
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG_KEY",
            "MY_GIT_CONFIG_KEY_0",
            "GIT_AUTHOR_NAME",
            "PATH",
        ] {
            assert!(!is_git_config_entry_var(name), "{name}");
        }
    }

    /// The host's entries are removed first and the sandbox's own set
    /// after, so a name in both ends with the sandbox's value, and nothing
    /// else is touched.
    #[test]
    fn the_worker_git_env_removes_the_hosts_entries_then_sets_its_own() {
        let host = [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_KEY_1",
            "GIT_CONFIG_VALUE_1",
            "GIT_CONFIG_PARAMETERS",
            "PATH",
        ]
        .map(OsString::from);
        let changes = worker_git_config_env(host, git_config_vars(&[("core.autocrlf", "false")]));
        let mut cmd = Command::new("true");
        for (name, value) in &changes {
            match value {
                Some(value) => cmd.env(name, value),
                None => cmd.env_remove(name),
            };
        }
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let get = |name: &str| envs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        assert_eq!(get("GIT_CONFIG_COUNT"), Some(Some("1".into())));
        assert_eq!(get("GIT_CONFIG_KEY_0"), Some(Some("core.autocrlf".into())));
        assert_eq!(get("GIT_CONFIG_VALUE_0"), Some(Some("false".into())));
        assert_eq!(get("GIT_CONFIG_KEY_1"), Some(None));
        assert_eq!(get("GIT_CONFIG_VALUE_1"), Some(None));
        assert_eq!(get("GIT_CONFIG_PARAMETERS"), Some(None));
        assert_eq!(get("PATH"), None);
    }

    /// A policy cannot pass a host git configuration entry through the
    /// sandbox: the command is refused, not run without it.
    #[test]
    fn passing_a_host_git_config_entry_is_refused() {
        let work = tempfile::tempdir().unwrap();
        let root = kgraf_core::plain_path::canonicalize(work.path()).unwrap();
        let policy =
            SandboxPolicy::new(vec![root.clone()], Network::Deny).pass_env("GIT_CONFIG_VALUE_0");
        let Err(err) = sandboxed_command(&policy, "true", &root) else {
            panic!("a pass of GIT_CONFIG_VALUE_0 was accepted");
        };
        assert!(
            matches!(&err, SandboxError::InvalidPolicy(m) if m.contains("GIT_CONFIG_VALUE_0")),
            "{err}"
        );
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

    /// What a cleared Windows environment still inherits holds no
    /// credential and no agent socket by the scrub's own test, and leaves out
    /// the temp folders, which the sandbox sets to its own.
    #[test]
    fn the_windows_base_variables_are_not_credentials() {
        for name in WINDOWS_BASE_VARS {
            assert!(!is_scrubbed(OsStr::new(name)), "{name}");
        }
        for name in ["TMP", "TEMP", "TMPDIR"] {
            assert!(
                !WINDOWS_BASE_VARS
                    .iter()
                    .any(|b| b.eq_ignore_ascii_case(name)),
                "{name}"
            );
        }
        for name in ["SystemRoot", "PATH", "USERPROFILE", "ComSpec"] {
            assert!(WINDOWS_BASE_VARS.contains(&name), "{name}");
        }
    }
}
