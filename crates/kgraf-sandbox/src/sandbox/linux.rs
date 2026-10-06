//! Linux backend: Landlock read and write allowlists, a seccomp filter, and
//! user, mount, IPC and PID namespaces where the kernel allows them, all
//! applied in the child between fork and exec.
//!
//! Layers, in the order the child applies them:
//! 0. Inherited descriptors from 3 up are marked close-on-exec
//!    ([`privileges`]), so nothing opened before the sandbox passes through.
//! 1. Namespaces ([`namespaces`], when [`LinuxConfinement::namespaces`]): the
//!    gitlink read-only, other processes invisible, network gone on deny.
//! 2. Landlock (`landlock` crate, MIT OR Apache-2.0): reads and writes limited
//!    to the allowlists. On ABI 6 (Linux 6.12) also signal and abstract-socket
//!    scoping.
//! 3. seccomp ([`seccomp`], ported from codex-rs `linux-sandbox`): no unix
//!    socket can be created, so `connect()` to docker.sock, D-Bus or
//!    ssh-agent is impossible; only AF_INET and AF_INET6 with network
//!    allowed, no socket at all with network denied; no ptrace, no
//!    process_vm, no io_uring, no keyring, no new namespaces.
//! 4. Capabilities: all dropped ([`privileges`]), so a sandbox run as root
//!    (or as root of its own user namespace) execs with none.
//!
//! Landlock and seccomp are required for full isolation. Without seccomp the
//! backend is the old Landlock-only one, which reports partial isolation and
//! runs only on explicit opt-in (fail loud). Namespaces are optional: without
//! them the gitlink is writable and signals are scoped only on ABI 6, and
//! [`IsolationStrength`] says so.
//!
//! **Stricter than macOS:** unix-domain sockets are refused even under the
//! writable roots, and a policy with [`super::SandboxPolicy::allow_unix_socket`]
//! entries is refused rather than run with the entries ignored.

mod namespaces;
mod privileges;
mod seccomp;

use std::ffi::OsStr;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use landlock::{
    ABI, Access, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr, RulesetStatus, Scope,
};

use super::child::{ChildStep, Reporter};
use super::{Backend, IsolationStrength, Network, ReadConfinement, Resolved};
use crate::error::SandboxError;

pub(super) const BACKEND_NAME: &str = "Linux Landlock + seccomp";

/// What this kernel offers the backend, probed once per process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxConfinement {
    /// Landlock ABI version; 0 when Landlock is absent or disabled. At
    /// least 3 is required to run at all.
    pub landlock_abi: i32,
    /// seccomp filters with errno returns are available.
    pub seccomp: bool,
    /// Unprivileged user, mount, IPC, PID and network namespaces work.
    pub namespaces: bool,
    /// A fresh procfs can be mounted in the PID namespace, hiding host
    /// processes. False where `/proc` has locked overmounts (containers).
    pub fresh_proc: bool,
}

impl LinuxConfinement {
    /// Signals to processes outside the sandbox are refused: by the PID
    /// namespace, or by Landlock's signal scope (ABI 6).
    pub fn signals_scoped(&self) -> bool {
        self.namespaces || self.landlock_abi >= LANDLOCK_SCOPES
    }

    fn full(&self) -> bool {
        self.seccomp && self.landlock_abi >= ABI_REQUIRED as i32
    }
}

/// Probe the kernel (Landlock ABI, seccomp, namespaces). The namespace probe
/// forks a short-lived child, so the result is cached.
pub fn confinement() -> LinuxConfinement {
    static PROBED: OnceLock<LinuxConfinement> = OnceLock::new();
    *PROBED.get_or_init(|| {
        let ns = namespaces::available();
        LinuxConfinement {
            landlock_abi: landlock_abi(),
            seccomp: seccomp::available(),
            namespaces: ns.namespaces,
            fresh_proc: ns.fresh_proc,
        }
    })
}

fn landlock_abi() -> i32 {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: with a null attribute pointer, size 0 and the VERSION flag the
    // call only returns the ABI version (or -1).
    let rc = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    i32::try_from(rc).unwrap_or(0).max(0)
}

/// Before seccomp: Landlock alone cannot stop `connect()` to an existing
/// socket, and a Docker daemon writes anywhere.
const PARTIAL: IsolationStrength = IsolationStrength {
    backend: Backend::LinuxLandlock,
    writes_confined: false,
    reads: ReadConfinement::AllowList,
    unix_sockets_confined: false,
    network_deniable: false,
    gitlink_protected: false,
    git_config_freezable: false,
    gaps: "seccomp is unavailable, so connect() to existing unix sockets is not \
           blocked (D-Bus, docker.sock, ssh-agent; the Docker daemon can write \
           anywhere); network is not blocked; signals to other processes are not \
           scoped; the gitlink is writable",
};

/// The strength on this kernel. Full when Landlock ABI 3 and seccomp are
/// present; the flags that depend on namespaces follow the probe.
pub(super) fn strength() -> IsolationStrength {
    static STRENGTH: OnceLock<IsolationStrength> = OnceLock::new();
    *STRENGTH.get_or_init(|| strength_for(confinement()))
}

/// Each call leaks its gap text (the field is `&'static str`); `strength`
/// computes it once per process.
fn strength_for(c: LinuxConfinement) -> IsolationStrength {
    if !c.full() {
        return PARTIAL;
    }
    IsolationStrength {
        backend: Backend::LinuxLandlock,
        writes_confined: true,
        reads: ReadConfinement::AllowList,
        unix_sockets_confined: true,
        network_deniable: true,
        gitlink_protected: c.namespaces,
        // Landlock grants whole hierarchies and cannot deny one file under
        // a writable root; a read-only bind mount cannot stop the directory
        // above it being renamed away and replaced.
        git_config_freezable: false,
        gaps: Box::leak(gaps(c).into_boxed_str()),
    }
}

fn gaps(c: LinuxConfinement) -> String {
    let mut gaps = Vec::new();
    if c.namespaces {
        gaps.push(
            "only an existing top-level .git of a writable root is read-only: a root \
             without one can gain one, and nested .git directories are writable",
        );
    } else {
        gaps.push("no user namespaces, so the gitlink is writable");
    }
    if !c.signals_scoped() {
        gaps.push("signals to other processes of the same user are not scoped");
    }
    if !(c.namespaces && c.fresh_proc) {
        gaps.push(
            "/proc lists host processes: the command lines and environments of processes \
             of the same user are readable, and an environment can hold secrets",
        );
    }
    gaps.push(
        "unix-domain sockets cannot be created at all, even in the workspace \
         (socketpair works)",
    );
    gaps.push("no resource limits");
    gaps.join("; ")
}

/// Whether running needs the policy's partial-isolation opt-in.
pub(super) fn is_partial() -> bool {
    !confinement().full()
}

/// ABI v3 (Linux 6.2) adds truncate. Below it a sandboxed process could
/// truncate any file outside its roots, so older kernels are refused rather
/// than partially enforced (fail loud).
const ABI_REQUIRED: ABI = ABI::V3;

/// ABI v6 (Linux 6.12) adds signal and abstract unix socket scoping.
const LANDLOCK_SCOPES: i32 = 6;

/// System paths readable by every sandboxed command.
const SYSTEM_READS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/etc",
    "/opt",
    "/proc",
    "/sys",
    "/dev",
    "/run/systemd/resolve",
    "/nix/store",
];

/// Toolchain paths under `$HOME`, readable (not writable).
const HOME_READS: &[&str] = &[
    // Not ~/.cargo itself: credentials.toml lives there. config.toml is
    // therefore not read either; its non-secret settings reach the command
    // through a cargo home of its own that the host writes
    // (`super::cargo_home`).
    ".cargo/bin",
    ".cargo/registry",
    ".cargo/git",
    ".rustup",
    ".npm",
    // Not `~/.cache` itself: it holds tokens (`~/.cache/huggingface/token`)
    // beside the package caches. Only the caches build tools read.
    ".cache/pip",
    ".cache/uv",
    ".cache/go-build",
    // Go's module cache under the default `GOPATH`: the downloaded module
    // sources a build reads. Go takes private-module logins from `~/.netrc`
    // or a credential helper, never from here.
    "go/pkg/mod",
    ".cache/pnpm",
    ".cache/yarn",
    // The compiler cache, node-gyp's downloaded headers, and pre-commit's
    // hook checkouts. None stores a login: sccache's storage credentials
    // live in its config under `~/.config`, which is not granted.
    ".cache/sccache",
    ".cache/node-gyp",
    ".cache/pre-commit",
    // Poetry's cache (its package cache and, by default, the project
    // virtualenvs; its repository logins live in `~/.config/pypoetry` or the
    // system keyring), the browser builds Playwright and Cypress download,
    // and Deno's module cache (`DENO_DIR`; Deno takes registry tokens from
    // `DENO_AUTH_TOKENS`, never from a file here). Each tool's documented
    // Linux default.
    ".cache/pypoetry",
    ".cache/ms-playwright",
    ".cache/Cypress",
    ".cache/deno",
    ".nvm",
    ".pyenv",
    ".gitconfig",
    // Not `~/.config/git` itself: the credential-store file lives there, and
    // the allow list cannot carve a file out of a directory it grants. The
    // three files git reads from it are granted one by one.
    ".config/git/config",
    ".config/git/ignore",
    ".config/git/attributes",
    // Not `~/.local` itself: it holds keyrings, shell histories and other
    // per-user data (`.local/share`). Only the places tools install to.
    ".local/bin",
    ".local/lib",
    ".local/share/uv",
    ".local/share/pnpm",
    // Version managers' and installers' toolchain trees: installed runtimes,
    // application virtualenvs and seed wheels. Their tokens, where they take
    // any, are read from `~/.config` or the environment, not from here.
    ".local/share/mise",
    ".local/share/fnm",
    ".local/share/pipx",
    ".local/share/virtualenv",
    ".local/share/rustup",
    // Pipenv's virtualenvs (`WORKON_HOME`) and the Poetry installation the
    // official installer makes (`POETRY_HOME`). No login is stored in either;
    // the keyrings beside them in `~/.local/share` stay unreadable.
    ".local/share/virtualenvs",
    ".local/share/pypoetry",
];

// Files cargo reads in every ancestor of the directory it runs in, relative
// to that ancestor: the manifest, while it looks for an enclosing workspace
// (`find_root_iter`: a package whose own manifest has no `[workspace]`
// walks up), and the configuration, which it always gathers upward.
//
// Read only when the policy opts in with a limit
// ([`super::SandboxPolicy::with_ancestor_reads`]), and then only between a
// writable root and that limit. Why: a worktree lies inside the user's
// checkout (`<root>/.kgraf/worktrees/<name>`), so cargo in the worktree
// reads `<root>/Cargo.toml` and `<root>/.cargo/config.toml`; Landlock lets
// it `stat` them but refuses the read, and cargo stops with "failed to read
// `<root>/Cargo.toml`: Permission denied". Never above the limit: an earlier
// version walked up to `/`, and a planted `/tmp/.cargo/config.toml` whose
// `runner` then ran as the user inside every contract check forged a pass.
// Rejected: letting the whole checkout be read (far more than cargo needs),
// and stopping cargo's walk (its only stop is its internal
// `__CARGO_TEST_ROOT`).
use super::ANCESTOR_READS;

/// Device files every build tool writes to.
const DEVICE_WRITES: &[&str] = &["/dev/null", "/dev/zero", "/dev/full", "/dev/tty"];

/// The child-side state, built in the parent and consumed by one spawn.
struct Prepared {
    ruleset: RulesetCreated,
    filter: Option<seccompiler::BpfProgram>,
    namespaces: Option<namespaces::Plan>,
}

#[cfg(test)]
pub(super) fn command(
    policy: &Resolved,
    program: &OsStr,
) -> Result<(Command, Option<ChildStep>), SandboxError> {
    command_with(policy, program, confinement()).map(|(cmd, step, _)| (cmd, step))
}

/// The command, its child step, and the cargo files above a writable root the
/// policy opted in to but could not grant, each with its cause
/// ([`AncestorReads::refused`]): the caller shows them beside the command's
/// own error, so a refusal is never silent.
pub(super) fn command_noting(
    policy: &Resolved,
    program: &OsStr,
) -> Result<(Command, Option<ChildStep>, Vec<String>), SandboxError> {
    command_with(policy, program, confinement())
}

/// The refusals [`command_noting`] would report, without building a command.
pub(super) fn ancestor_refusals(policy: &Resolved) -> Result<Vec<String>, SandboxError> {
    Ok(ancestor_reads(policy)?.refused)
}

fn command_with(
    policy: &Resolved,
    program: &OsStr,
    kernel: LinuxConfinement,
) -> Result<(Command, Option<ChildStep>, Vec<String>), SandboxError> {
    if !kernel.seccomp && policy.network == Network::Deny {
        return Err(SandboxError::PolicyNotEnforceable {
            backend: BACKEND_NAME,
            what: "network denial without seccomp (Landlock covers TCP bind and connect only; \
                   UDP and unix sockets stay open)",
        });
    }
    if kernel.seccomp && !policy.unix_sockets.is_empty() {
        return Err(SandboxError::PolicyNotEnforceable {
            backend: BACKEND_NAME,
            what: "a unix-socket allowlist (seccomp refuses every unix socket; it cannot \
                   tell one path from another)",
        });
    }
    let (ruleset, refused) = build_ruleset(policy, kernel.landlock_abi >= LANDLOCK_SCOPES)?;
    let mut slot = Some(Prepared {
        ruleset,
        filter: if kernel.seccomp {
            Some(seccomp::build(policy.network)?)
        } else {
            None
        },
        namespaces: if kernel.namespaces {
            Some(namespaces::Plan::new(
                policy.network,
                &policy.writable,
                kernel.fresh_proc,
            )?)
        } else {
            None
        },
    });
    let step: ChildStep = Box::new(move |reporter: &Reporter| {
        let Some(Prepared {
            mut ruleset,
            filter,
            namespaces,
        }) = slot.take()
        else {
            return Err(reporter.fail(
                "sandbox already used: spawn a sandboxed command once",
                libc::EINVAL,
            ));
        };
        let fail = |(msg, errno): namespaces::StepError| reporter.fail(msg, errno);
        privileges::cloexec_inherited().map_err(fail)?;
        if let Some(plan) = &namespaces {
            namespaces::enter(plan).map_err(fail)?;
            namespaces::fork_into_pid_namespace().map_err(fail)?;
            if plan.fresh_proc {
                namespaces::mount_fresh_proc().map_err(fail)?;
                ruleset = allow_fresh_proc(ruleset).map_err(fail)?;
            }
        }
        let status = ruleset
            .restrict_self()
            .map_err(|_| reporter.fail("landlock_restrict_self failed", libc::EPERM))?;
        if status.ruleset != RulesetStatus::FullyEnforced {
            return Err(reporter.fail(
                "landlock ruleset not fully enforced by this kernel",
                libc::ENOTSUP,
            ));
        }
        if !status.no_new_privs {
            return Err(reporter.fail("no_new_privs not set", libc::ENOTSUP));
        }
        privileges::drop_capabilities().map_err(fail)?;
        // Last: the filter denies nothing the steps above need, but installing
        // it after them keeps that true as the list grows.
        if let Some(filter) = &filter {
            seccomp::install(filter)
                .map_err(|errno| reporter.fail("seccomp filter install failed", errno))?;
        }
        Ok(())
    });
    Ok((Command::new(program), Some(step), refused))
}

/// Readable rule for the procfs just mounted on `/proc`. The parent's rule
/// names the host procfs, a different filesystem, so the new one needs its
/// own. Child only: open, then `landlock_add_rule` through the crate on a
/// borrowed descriptor (no path conversion, so no allocation).
fn allow_fresh_proc(ruleset: RulesetCreated) -> Result<RulesetCreated, namespaces::StepError> {
    // SAFETY: opens a static NUL-terminated path; the fd is closed below.
    let fd = unsafe {
        libc::open(
            c"/proc".as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let e = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL);
        return Err(("cannot open the fresh /proc", e));
    }
    // SAFETY: `fd` is open for the duration of the borrow.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let result = ruleset.add_rule(PathBeneath::new(
        borrowed,
        AccessFs::from_read(ABI_REQUIRED),
    ));
    // SAFETY: closes the fd opened above; the rule holds no reference to it.
    unsafe { libc::close(fd) };
    result.map_err(|_| {
        (
            "cannot add the fresh /proc to the Landlock rules",
            libc::EINVAL,
        )
    })
}

/// Build the ruleset in the parent so an unsupported kernel fails here, with
/// a message, before any process starts.
fn build_ruleset(
    policy: &Resolved,
    scopes: bool,
) -> Result<(RulesetCreated, Vec<String>), SandboxError> {
    let all = AccessFs::from_all(ABI_REQUIRED);
    let read = AccessFs::from_read(ABI_REQUIRED);
    let write = AccessFs::from_write(ABI_REQUIRED);
    let kernel = |e: landlock::RulesetError| SandboxError::KernelUnsupported(e.to_string());

    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(all)
        .map_err(kernel)?;
    if scopes {
        // Probed present, so a hard requirement: a kernel that then refuses
        // it fails here instead of running unscoped.
        ruleset = ruleset
            .scope(Scope::Signal | Scope::AbstractUnixSocket)
            .map_err(kernel)?;
    }
    let mut ruleset = ruleset.create().map_err(kernel)?;

    let mut optional: Vec<PathBuf> = SYSTEM_READS.iter().map(PathBuf::from).collect();
    if let Some(home) = &policy.home {
        optional.extend(HOME_READS.iter().map(|rel| home.join(rel)));
    }
    // Optional: a path this machine lacks, or cannot reach (a regular file
    // `~/.config` makes `~/.config/git` "not a directory"), grants nothing.
    for path in &optional {
        if let Some(r) = rule(path, read, Presence::Optional)? {
            ruleset = ruleset.add_rule(r).map_err(kernel)?;
        }
    }
    // Required: the caller named these; failing to grant one is an error.
    for path in &policy.readable {
        let r = rule(path, read, Presence::Required)?.ok_or_else(|| gone(path))?;
        ruleset = ruleset.add_rule(r).map_err(kernel)?;
    }
    let ancestors = ancestor_reads(policy)?;
    // Read, not execute: a manifest is never a program.
    for fd in ancestors.granted {
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::ReadFile))
            .map_err(kernel)?;
    }
    for root in &policy.writable {
        let r = rule(root, all, Presence::Required)?.ok_or_else(|| gone(root))?;
        ruleset = ruleset.add_rule(r).map_err(kernel)?;
    }
    for dev in DEVICE_WRITES.iter().map(Path::new) {
        if let Some(r) = rule(dev, read | write, Presence::Optional)? {
            ruleset = ruleset.add_rule(r).map_err(kernel)?;
        }
    }
    Ok((ruleset, ancestors.refused))
}

fn gone(path: &Path) -> SandboxError {
    SandboxError::Io {
        path: path.to_path_buf(),
        source: std::io::ErrorKind::NotFound.into(),
    }
}

/// What [`ancestor_reads`] found: the descriptors to grant, and each existing
/// cargo file it would not grant, with the cause ("`<root>` is
/// group-writable (mode 775) by group 100, which is not this user's private
/// group").
struct AncestorReads {
    granted: Vec<OwnedFd>,
    refused: Vec<String>,
}

/// The [`ANCESTOR_READS`] files above each writable root, up to the policy's
/// limit: descriptors for those that may be read, and the reason for each
/// existing one that may not.
///
/// Every path is opened once, component by component from the limit, with
/// `openat2(RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH)`, and each descriptor is
/// checked with `fstat` and handed to Landlock as it is: nothing is resolved
/// twice, so a link swapped in between a check and the open cannot redirect
/// the rule. A file is taken only when it is a regular file and it
/// and every directory from it up to the limit pass
/// [`crate::trust::untrusted_reason`].
///
/// The limit is opened by its full path, so the directories above it are
/// not checked: whoever can rename one of them can put another directory at
/// the limit's path. A caller must give a limit whose parents only it and
/// root can change (a checkout under the user's home, not one under a
/// shared, non-sticky directory). The limit's own owner and mode are checked.
fn ancestor_reads(policy: &Resolved) -> Result<AncestorReads, SandboxError> {
    let mut found = AncestorReads {
        granted: Vec::new(),
        refused: Vec::new(),
    };
    let Some(limit) = &policy.ancestor_limit else {
        return Ok(found);
    };
    // The caller named the limit: failing to open it is an error, not a
    // quiet refusal.
    let limit_fd = openat2(
        None,
        limit,
        libc::O_PATH | libc::O_DIRECTORY,
        libc::RESOLVE_NO_SYMLINKS,
    )
    .map_err(|source| SandboxError::Io {
        path: limit.clone(),
        source,
    })?;
    let limit_meta = fd_meta(&limit_fd).map_err(|source| SandboxError::Io {
        path: limit.clone(),
        source,
    })?;
    if let Some(reason) = crate::trust::untrusted_reason(&limit_meta) {
        found.refused.push(format!(
            "no cargo file above the worktree is readable in the sandbox: {} {reason}",
            limit.display()
        ));
        return Ok(found);
    }
    // Each directory strictly above a root, up to and including the limit.
    let mut dirs: Vec<&Path> = Vec::new();
    for root in &policy.writable {
        for dir in root.ancestors().skip(1) {
            if !dir.starts_with(limit) {
                break;
            }
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    let mut refuse = |file: &Path, why: String| {
        // Only a file that is there matters to cargo; an absent one is
        // simply absent. Advisory: the grant never depends on this stat.
        if std::fs::symlink_metadata(file).is_ok() {
            let line = format!("{} is not readable in the sandbox: {why}", file.display());
            if !found.refused.contains(&line) {
                found.refused.push(line);
            }
        }
    };
    for dir in dirs {
        for rel in ANCESTOR_READS {
            let file = dir.join(rel);
            if policy.read_denied.iter().any(|d| file.starts_with(d)) {
                refuse(
                    &file,
                    "it is beneath a path the sandbox denies reads of".into(),
                );
                continue;
            }
            match &policy.cargo_homes {
                Some(homes) if homes.iter().any(|h| file.starts_with(h)) => {
                    refuse(
                        &file,
                        "it is in a cargo home, which may hold credentials".into(),
                    );
                    continue;
                }
                None if rel.starts_with(".cargo") => {
                    refuse(
                        &file,
                        "no home directory is known, so `~/.cargo` cannot be told apart \
                         and no `.cargo` file above a worktree is read"
                            .into(),
                    );
                    continue;
                }
                _ => {}
            }
            let Ok(below) = file.strip_prefix(limit) else {
                continue;
            };
            match open_trusted_file(&limit_fd, limit, below) {
                Ok(fd) => found.granted.push(fd),
                Err(Some(why)) => refuse(&file, why),
                Err(None) => {}
            }
        }
    }
    Ok(found)
}

/// Open `rel` beneath `limit` one component at a time, never through a link,
/// and return the final descriptor when every directory on the way and the
/// file itself are trusted and the file is a regular file. `Err(None)`: the
/// file is absent (or a directory on its way is not one); `Err(Some(why))`:
/// it is there but may not be granted, and `why` names the path at fault.
fn open_trusted_file(
    limit_fd: &OwnedFd,
    limit: &Path,
    rel: &Path,
) -> Result<OwnedFd, Option<String>> {
    let parts: Vec<_> = rel.components().collect();
    let Some((last, dirs)) = parts.split_last() else {
        return Err(None);
    };
    let mut at_path = limit.to_path_buf();
    let mut parent: Option<OwnedFd> = None;
    let open = |at: &OwnedFd, name: &std::ffi::OsStr, flags: libc::c_int, path: &Path| {
        openat2(
            Some(at),
            Path::new(name),
            flags | libc::O_PATH | libc::O_NOFOLLOW,
            libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_BENEATH,
        )
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ENOENT) | Some(libc::ENOTDIR) => None,
            Some(libc::ELOOP) => Some(format!("{} is a symbolic link", path.display())),
            _ => Some(format!("{} cannot be opened: {e}", path.display())),
        })
    };
    for part in dirs {
        at_path.push(part.as_os_str());
        let fd = open(
            parent.as_ref().unwrap_or(limit_fd),
            part.as_os_str(),
            libc::O_DIRECTORY,
            &at_path,
        )?;
        let meta = fd_meta(&fd).map_err(|e| Some(format!("{}: {e}", at_path.display())))?;
        if let Some(reason) = crate::trust::untrusted_reason(&meta) {
            return Err(Some(format!("{} {reason}", at_path.display())));
        }
        parent = Some(fd);
    }
    at_path.push(last.as_os_str());
    let fd = open(
        parent.as_ref().unwrap_or(limit_fd),
        last.as_os_str(),
        0,
        &at_path,
    )?;
    let meta = fd_meta(&fd).map_err(|e| Some(format!("{}: {e}", at_path.display())))?;
    if !meta.file_type().is_file() {
        return Err(Some(format!("{} is not a regular file", at_path.display())));
    }
    if let Some(reason) = crate::trust::untrusted_reason(&meta) {
        return Err(Some(format!("{} {reason}", at_path.display())));
    }
    Ok(fd)
}

/// `openat2(2)` relative to `dir` (the working directory when `None`),
/// `O_CLOEXEC` always. Linux 5.6; Landlock ABI 3 already needs 6.2.
fn openat2(
    dir: Option<&OwnedFd>,
    path: &Path,
    flags: libc::c_int,
    resolve: u64,
) -> std::io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // `open_how` is non-exhaustive in libc (the kernel may grow it): start
    // from all zeros, which the kernel requires of every unused field.
    // SAFETY: an all-zero `open_how` is valid; its fields are plain integers.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (flags | libc::O_CLOEXEC) as u64;
    how.resolve = resolve;
    let dirfd = dir.map_or(libc::AT_FDCWD, |d| d.as_raw_fd());
    // SAFETY: `path` is NUL-terminated and `how` is a valid `open_how` of
    // the size passed; both outlive the call.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dirfd,
            path.as_ptr(),
            &how as *const libc::open_how,
            std::mem::size_of::<libc::open_how>(),
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a non-negative return is a new descriptor this process owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// `fstat` of a descriptor (an `O_PATH` one included), keeping the
/// descriptor.
fn fd_meta(fd: &OwnedFd) -> std::io::Result<std::fs::Metadata> {
    let file = std::fs::File::from(fd.try_clone()?);
    file.metadata()
}

/// Whether a path a rule is built for may be missing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Presence {
    /// A system or toolchain path this machine may not have, or may not let
    /// this user reach: absent, not reachable through a non-directory, or
    /// not permitted are all "grants nothing" (a regular-file
    /// `~/.config` made every sandboxed command fail on `~/.config/git`).
    Optional,
    /// A path the caller named (a writable or readable root): any failure
    /// is an error.
    Required,
}

/// A rule for `path`, or `None` when an optional path is absent. The path is
/// opened once (`O_PATH`, following links: `/bin` is a link on merged-/usr
/// systems) and that descriptor is both inspected and handed to Landlock,
/// so the rights are narrowed to file rights exactly when the object the
/// rule covers is not a directory (hard-requirement mode rejects directory
/// rights on a file). Resolving the path once for the check and again for
/// the rule let a swap in between change what the rule covered.
fn rule(
    path: &Path,
    access: BitFlags<AccessFs>,
    presence: Presence,
) -> Result<Option<PathBeneath<OwnedFd>>, SandboxError> {
    let io = |source: std::io::Error| SandboxError::Io {
        path: path.to_path_buf(),
        source,
    };
    let fd = match openat2(None, path, libc::O_PATH, 0) {
        Ok(fd) => fd,
        Err(e)
            if presence == Presence::Optional
                && matches!(
                    e.raw_os_error(),
                    Some(libc::ENOENT) | Some(libc::ENOTDIR) | Some(libc::EACCES)
                ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(io(e)),
    };
    let access = if fd_meta(&fd).map_err(io)?.is_dir() {
        access
    } else {
        access & AccessFs::from_file(ABI_REQUIRED)
    };
    Ok(Some(PathBeneath::new(fd, access)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel(abi: i32, seccomp: bool, namespaces: bool) -> LinuxConfinement {
        LinuxConfinement {
            landlock_abi: abi,
            seccomp,
            namespaces,
            fresh_proc: namespaces,
        }
    }

    #[test]
    fn strength_is_full_only_with_landlock_3_and_seccomp() {
        assert_eq!(strength_for(kernel(4, false, true)), PARTIAL);
        assert_eq!(strength_for(kernel(2, true, true)), PARTIAL);
        let full = strength_for(kernel(4, true, false));
        assert!(full.writes_confined && full.unix_sockets_confined && full.network_deniable);
        assert!(!full.gitlink_protected);
        assert!(full.gaps.contains("signals"), "{}", full.gaps);
        let with_ns = strength_for(kernel(4, true, true));
        assert!(with_ns.gitlink_protected);
        assert!(!with_ns.gaps.contains("signals"), "{}", with_ns.gaps);
        assert!(
            !strength_for(kernel(6, true, false))
                .gaps
                .contains("signals")
        );
        assert!(!with_ns.gaps.contains("/proc lists"), "{}", with_ns.gaps);
        let no_proc = strength_for(LinuxConfinement {
            fresh_proc: false,
            ..kernel(4, true, true)
        });
        assert!(no_proc.gaps.contains("/proc lists"), "{}", no_proc.gaps);
    }

    fn policy(work: &Path, home: Option<&Path>, network: Network) -> Resolved {
        Resolved {
            writable: vec![std::fs::canonicalize(work).unwrap()],
            readable: vec![],
            read_denied: vec![],
            unix_sockets: vec![],
            network,
            home: home.map(|h| std::fs::canonicalize(h).unwrap()),
            ancestor_limit: None,
            cargo_homes: home.map(|h| vec![std::fs::canonicalize(h).unwrap().join(".cargo")]),
            frozen_git: vec![],
        }
    }

    /// A policy writable at `work` that opts in to ancestor reads up to
    /// `limit`, with `cargo_homes` as given (`None`: no home was found).
    fn opted(work: &Path, limit: &Path, cargo_homes: Option<Vec<PathBuf>>) -> Resolved {
        Resolved {
            ancestor_limit: Some(std::fs::canonicalize(limit).unwrap()),
            cargo_homes,
            ..policy(work, None, Network::Deny)
        }
    }

    fn run(policy: &Resolved, program: &Path, args: &[&Path]) -> std::process::Output {
        let (mut cmd, step) = command(policy, program.as_os_str()).unwrap();
        let _probe = super::super::child::install(&mut cmd, None, step).unwrap();
        cmd.args(args).output().unwrap()
    }

    fn cat(policy: &Resolved, path: &Path) -> std::process::Output {
        run(policy, Path::new("/bin/cat"), &[path])
    }

    fn assert_read(policy: &Resolved, path: &Path, text: &str) {
        let out = cat(policy, path);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            text,
            "{path:?}: {out:?}"
        );
    }

    fn assert_refused(policy: &Resolved, path: &Path) {
        let out = cat(policy, path);
        assert!(!out.status.success(), "{path:?} was read: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Permission denied"),
            "{path:?}: {out:?}"
        );
    }

    fn mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Directories 0755 and files 0644 beneath `dir`, links left alone: the
    /// layout passes the ownership test whatever the umask (0002 on many
    /// desktops makes every new directory group-writable).
    fn tighten(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                mode(&path, 0o755);
                tighten(&path);
            } else if meta.is_file() {
                mode(&path, 0o644);
            }
        }
    }

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let base = tempfile::tempdir().unwrap();
        let b = std::fs::canonicalize(base.path()).unwrap();
        (base, b)
    }

    /// The home grants are narrow: git's credential-store file and the
    /// per-user data under `~/.local/share` are unreadable, while the files
    /// git reads and the install directories stay readable. Fake fixtures
    /// under a temp HOME only. Needs Landlock ABI v3 or later.
    #[test]
    fn home_grants_exclude_git_credentials_and_local_share_data() {
        let (_base, b) = canonical_tempdir();
        let home = b.join("home");
        let work = b.join("work");
        std::fs::create_dir_all(home.join(".config/git")).unwrap();
        std::fs::create_dir_all(home.join(".local/share/keyrings")).unwrap();
        std::fs::create_dir_all(home.join(".local/share/uv")).unwrap();
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(home.join(".config/git/credentials"), "FAKE-TOKEN").unwrap();
        std::fs::write(home.join(".config/git/config"), "[user]\n").unwrap();
        std::fs::write(home.join(".local/share/keyrings/login"), "FAKE-KEYRING").unwrap();
        std::fs::write(home.join(".local/share/uv/tool"), "ok").unwrap();
        std::fs::write(home.join(".local/bin/tool"), "ok").unwrap();
        std::fs::create_dir_all(home.join(".cache/huggingface")).unwrap();
        std::fs::create_dir_all(home.join(".cache/pip")).unwrap();
        std::fs::write(home.join(".cache/huggingface/token"), "FAKE-HF").unwrap();
        std::fs::write(home.join(".cache/pip/wheel"), "ok").unwrap();
        let policy = policy(&work, Some(&home), Network::Deny);
        assert_refused(&policy, &home.join(".config/git/credentials"));
        assert_refused(&policy, &home.join(".local/share/keyrings/login"));
        assert_refused(&policy, &home.join(".cache/huggingface/token"));
        assert_read(&policy, &home.join(".cache/pip/wheel"), "ok");
        assert_read(&policy, &home.join(".config/git/config"), "[user]\n");
        assert_read(&policy, &home.join(".local/share/uv/tool"), "ok");
        assert_read(&policy, &home.join(".local/bin/tool"), "ok");
        // Toolchain trees that version managers and build caches keep.
        for rel in [
            ".local/share/mise/installs/node/bin/node",
            ".local/share/fnm/node-versions/v1/node",
            ".local/share/pipx/venvs/tool/bin/tool",
            ".local/share/virtualenv/wheel/seed.whl",
            ".local/share/rustup/toolchains/stable/bin/rustc",
            ".cache/sccache/0/entry",
            ".cache/node-gyp/20/include/node.h",
            ".cache/pre-commit/repo/hook.py",
            ".cache/pypoetry/virtualenvs/p-py3.12/bin/python",
            ".cache/ms-playwright/chromium-1140/chrome-linux/chrome",
            ".cache/Cypress/13.6.0/Cypress/Cypress",
            ".cache/deno/remote/https/deno.land/mod.ts",
            ".local/share/virtualenvs/p-AbCd/bin/python",
            ".local/share/pypoetry/venv/bin/poetry",
        ] {
            let path = home.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "ok").unwrap();
            assert_read(&policy, &path, "ok");
        }
        // Never granted, so unreadable on this backend by construction.
        for rel in [
            ".vault-token",
            ".pgpass",
            ".terraform.d/c",
            ".gem/credentials",
            ".codex/auth.json",
            ".claude/.credentials.json",
            ".m2/settings.xml",
            ".gradle/gradle.properties",
            ".config/containers/auth.json",
            ".oci/config",
            ".bundle/config",
        ] {
            let path = home.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "FAKE").unwrap();
            assert_refused(&policy, &path);
        }
    }

    /// Needs a Linux kernel with Landlock ABI v3 or later.
    #[test]
    fn cargo_credentials_are_unreadable_but_the_registry_is_readable() {
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("home");
        let work = base.path().join("work");
        std::fs::create_dir_all(home.join(".cargo/registry")).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(home.join(".cargo/credentials.toml"), "FAKE-TOKEN").unwrap();
        std::fs::write(home.join(".cargo/registry/index"), "ok").unwrap();
        let policy = policy(&work, Some(&home), Network::Allow);
        let cat = |path: PathBuf| {
            let (mut cmd, step) = command(&policy, OsStr::new("/bin/cat")).unwrap();
            let _probe = super::super::child::install(&mut cmd, None, step).unwrap();
            cmd.arg(path).output().unwrap()
        };
        let denied = cat(home.join(".cargo/credentials.toml"));
        assert!(!denied.status.success(), "{denied:?}");
        assert!(
            String::from_utf8_lossy(&denied.stderr).contains("Permission denied"),
            "{denied:?}"
        );
        let allowed = cat(home.join(".cargo/registry/index"));
        assert_eq!(
            String::from_utf8_lossy(&allowed.stdout),
            "ok",
            "{allowed:?}"
        );
    }

    /// The live defect : cargo in
    /// a worktree under `<root>/.kgraf` read `<root>/Cargo.toml` and got
    /// "Permission denied". Opted in up to the checkout, the cargo files
    /// between the worktree and the checkout are readable; nothing else is:
    /// not a sibling, not a cargo file under a denied directory or above the
    /// limit, not a link, not `~/.cargo/config.toml`. Without the opt-in none
    /// is. Needs Landlock ABI v3 or later, as every test here.
    #[test]
    fn cargo_files_up_to_the_limit_are_readable_and_nothing_else() {
        let (_base, b) = canonical_tempdir();
        let home = b.join("home");
        let root = home.join("repo");
        let state = root.join(".kgraf");
        let work = state.join("worktrees/w");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(root.join(".cargo/config.toml"), "[build]\n").unwrap();
        std::fs::write(root.join("secret.txt"), "SECRET").unwrap();
        std::fs::write(state.join("Cargo.toml"), "SECRET").unwrap();
        std::fs::write(home.join("Cargo.toml"), "SECRET").unwrap();
        std::fs::write(home.join(".cargo/config.toml"), "SECRET").unwrap();
        std::fs::write(b.join("target.txt"), "SECRET").unwrap();
        std::os::unix::fs::symlink(b.join("target.txt"), root.join(".cargo/config")).unwrap();
        tighten(&b);

        let none = policy(&work, Some(&home), Network::Deny);
        assert_refused(&none, &root.join("Cargo.toml"));

        let mut opted = opted(&work, &root, Some(vec![home.join(".cargo")]));
        opted.read_denied = vec![state.clone()];
        assert_read(&opted, &root.join("Cargo.toml"), "[package]\n");
        assert_read(&opted, &root.join(".cargo/config.toml"), "[build]\n");
        for path in [
            root.join("secret.txt"),
            state.join("Cargo.toml"),
            home.join("Cargo.toml"),
            home.join(".cargo/config.toml"),
            root.join(".cargo/config"),
            b.join("target.txt"),
        ] {
            assert_refused(&opted, &path);
        }
    }

    /// The refusals a policy reports, from the same build that makes the
    /// command.
    fn refusals(policy: &Resolved) -> Vec<String> {
        command_noting(policy, OsStr::new("/bin/true")).unwrap().2
    }

    fn chown(path: &Path, uid: u32, gid: u32) {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path; chown only changes ownership.
        assert_eq!(unsafe { libc::chown(c.as_ptr(), uid, gid) }, 0, "{path:?}");
    }

    fn is_root() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    /// A cargo file someone else could change is not
    /// read, though it lies between the root and the limit (an other-writable
    /// `.cargo`, an other-writable manifest, and, as root, where a file can be
    /// given away, a manifest owned by another user), and each refusal names
    /// the path at fault and why.
    #[test]
    fn a_cargo_file_another_user_could_change_is_not_read_and_says_why() {
        let (_base, b) = canonical_tempdir();
        let root = b.join("repo");
        let inner = root.join("inner");
        let work = inner.join("w");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(inner.join(".cargo")).unwrap();
        std::fs::write(inner.join(".cargo/config.toml"), "PLANTED").unwrap();
        std::fs::write(inner.join("Cargo.toml"), "PLANTED").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        tighten(&b);
        mode(&inner.join(".cargo"), 0o757);
        mode(&inner.join("Cargo.toml"), 0o646);
        if is_root() {
            chown(&root.join("Cargo.toml"), 65534, 65534);
        }
        let p = opted(&work, &b, Some(vec![]));
        assert_refused(&p, &inner.join(".cargo/config.toml"));
        assert_refused(&p, &inner.join("Cargo.toml"));
        let said = refusals(&p).join("\n");
        assert!(
            said.contains(&format!(
                "{} is writable by others (mode 757)",
                inner.join(".cargo").display()
            )),
            "{said}"
        );
        assert!(said.contains("(mode 646)"), "{said}");
        if is_root() {
            assert_refused(&p, &root.join("Cargo.toml"));
            assert!(said.contains("owned by user 65534"), "{said}");
        } else {
            assert_read(&p, &root.join("Cargo.toml"), "[package]\n");
        }
    }

    /// Umask 0002 (directories 0775, files 0664) in this user's
    /// private group is trusted, so a desktop checkout's cargo files are
    /// read; the same modes under a shared group are refused, and say so.
    #[test]
    fn group_write_is_trusted_only_in_the_users_private_group() {
        let (_base, b) = canonical_tempdir();
        let root = b.join("repo");
        let work = root.join("w");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(root.join(".cargo/config.toml"), "[build]\n").unwrap();
        tighten(&b);
        let umask_0002 = |gid: Option<u32>| {
            for (path, m) in [
                (root.clone(), 0o775),
                (root.join(".cargo"), 0o775),
                (root.join("Cargo.toml"), 0o664),
                (root.join(".cargo/config.toml"), 0o664),
            ] {
                if let Some(g) = gid {
                    chown(&path, u32::MAX, g);
                }
                mode(&path, m);
            }
        };
        let upg = crate::trust::private_group();
        let my_gid = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&root).unwrap().gid()
        };
        // Private group: set it where root can; otherwise the files already
        // carry this process's group, which is the private one or not.
        match upg {
            Some(g) if is_root() || g == my_gid => {
                umask_0002(is_root().then_some(g));
                let p = opted(&work, &b, Some(vec![]));
                assert_read(&p, &root.join("Cargo.toml"), "[package]\n");
                assert_read(&p, &root.join(".cargo/config.toml"), "[build]\n");
                assert!(refusals(&p).is_empty(), "{:?}", refusals(&p));
            }
            _ => {
                umask_0002(None);
                let p = opted(&work, &b, Some(vec![]));
                assert_refused(&p, &root.join("Cargo.toml"));
            }
        }
        // A shared group (gid 100, `users` on Debian): only root can hand the
        // files to it here.
        if is_root() && upg != Some(100) {
            umask_0002(Some(100));
            let p = opted(&work, &b, Some(vec![]));
            assert_refused(&p, &root.join("Cargo.toml"));
            assert_refused(&p, &root.join(".cargo/config.toml"));
            let said = refusals(&p).join("\n");
            assert!(
                said.contains(&format!(
                    "no cargo file above the worktree is readable in the sandbox: {} is \
                     group-writable (mode 775) by group 100",
                    root.display()
                )) || said.contains("by group 100"),
                "{said}"
            );
        }
    }

    /// An optional system or toolchain path that cannot be
    /// reached (here `~/.config` is a regular file, so `~/.config/git` is
    /// "not a directory") grants nothing and the command still runs; a
    /// readable root, a writable root or a limit that is not there is an
    /// error.
    #[test]
    fn an_unreachable_optional_read_is_absent_but_a_named_path_must_exist() {
        let (_base, b) = canonical_tempdir();
        let home = b.join("home");
        let work = b.join("w");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(home.join(".config"), "a file").unwrap();
        let p = policy(&work, Some(&home), Network::Deny);
        let out = run(&p, Path::new("/bin/true"), &[]);
        assert!(out.status.success(), "{out:?}");

        let mut readable = policy(&work, None, Network::Deny);
        readable.readable = vec![b.join("gone")];
        assert!(command(&readable, OsStr::new("/bin/true")).is_err());
        let mut writable = policy(&work, None, Network::Deny);
        writable.writable.push(b.join("gone"));
        assert!(command(&writable, OsStr::new("/bin/true")).is_err());
        let mut limit = policy(&work, None, Network::Deny);
        limit.ancestor_limit = Some(b.join("gone"));
        assert!(command(&limit, OsStr::new("/bin/true")).is_err());
    }

    /// The owner of an ancestor `.cargo` swaps `config`
    /// between a regular file and a link to a secret directory while
    /// policies are built. The first version resolved the path three times
    /// and granted the directory in under a second. Now it is opened once
    /// without following links, so over three seconds of attempts the secret
    /// is never read.
    #[test]
    fn a_link_swapped_in_while_the_rules_are_built_grants_nothing() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (_base, b) = canonical_tempdir();
        let secret = b.join("secret");
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("id_ed25519"), "SECRET-KEY").unwrap();
        let dotcargo = b.join(".cargo");
        std::fs::create_dir_all(&dotcargo).unwrap();
        std::fs::write(dotcargo.join("config"), "x").unwrap();
        let work = b.join("checks/run");
        std::fs::create_dir_all(&work).unwrap();
        tighten(&b);
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let swapper = {
            let (stop, dotcargo, secret) = (stop.clone(), dotcargo.clone(), secret.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = std::fs::write(dotcargo.join("f.tmp"), "x");
                    let _ = std::fs::rename(dotcargo.join("f.tmp"), dotcargo.join("config"));
                    let _ = std::os::unix::fs::symlink(&secret, dotcargo.join("l.tmp"));
                    let _ = std::fs::rename(dotcargo.join("l.tmp"), dotcargo.join("config"));
                }
            })
        };
        let p = opted(&work, &b, Some(vec![]));
        let (mut tries, mut wins) = (0, 0);
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(3) {
            tries += 1;
            let out = cat(&p, &secret.join("id_ed25519"));
            if String::from_utf8_lossy(&out.stdout).contains("SECRET-KEY") {
                wins += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        swapper.join().unwrap();
        assert_eq!(wins, 0, "the secret was read in {wins} of {tries} tries");
        assert!(tries > 10, "only {tries} tries");
    }

    /// An ancestor manifest is read, never executed.
    #[test]
    fn an_ancestor_manifest_is_read_but_never_executed() {
        let (_base, b) = canonical_tempdir();
        let work = b.join("repo/w");
        std::fs::create_dir_all(&work).unwrap();
        let manifest = b.join("repo/Cargo.toml");
        std::fs::write(&manifest, "#!/bin/sh\necho EXECUTED\n").unwrap();
        tighten(&b);
        mode(&manifest, 0o755);
        let p = opted(&work, &b, Some(vec![]));
        assert_read(&p, &manifest, "#!/bin/sh\necho EXECUTED\n");
        let (mut cmd, step) = command(&p, manifest.as_os_str()).unwrap();
        let _probe = super::super::child::install(&mut cmd, None, step).unwrap();
        match cmd.output() {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}"),
            Ok(out) => panic!("the manifest ran: {out:?}"),
        }
    }

    /// With no home directory found, no `.cargo` file
    /// above a root is read (it may be `~/.cargo/config.toml`, with a registry
    /// token); `$CARGO_HOME`, like `~/.cargo`, is never entered.
    #[test]
    fn without_a_home_no_dot_cargo_file_is_read_and_cargo_home_never_is() {
        let (_base, b) = canonical_tempdir();
        let home = b.join("home");
        let work = home.join("repo/.kgraf/worktrees/w");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        std::fs::write(home.join(".cargo/config.toml"), "token = \"TOKEN\"\n").unwrap();
        std::fs::write(home.join("Cargo.toml"), "[package]\n").unwrap();
        tighten(&b);
        let homeless = opted(&work, &b, None);
        assert_refused(&homeless, &home.join(".cargo/config.toml"));
        assert_read(&homeless, &home.join("Cargo.toml"), "[package]\n");

        let cargo_home = home.join(".cargo");
        let with_cargo_home = opted(&work, &b, Some(vec![cargo_home]));
        assert_refused(&with_cargo_home, &home.join(".cargo/config.toml"));
    }

    fn seccomp_filters(status: &str) -> usize {
        status
            .lines()
            .find_map(|l| l.strip_prefix("Seccomp_filters:"))
            .map(|v| v.trim().parse().unwrap())
            .unwrap_or(0)
    }

    /// The fallback path, forced: without namespaces the command still runs
    /// under Landlock and one more seccomp filter, whatever this kernel
    /// offers.
    #[test]
    fn without_namespaces_seccomp_and_landlock_still_apply() {
        let k = confinement();
        if !k.full() {
            return;
        }
        let base = tempfile::tempdir().unwrap();
        let work = base.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let outside = base.path().join("escaped");
        let policy = policy(&work, None, Network::Deny);
        let no_ns = LinuxConfinement {
            namespaces: false,
            ..k
        };
        let (mut cmd, step, _) = command_with(&policy, OsStr::new("/bin/sh"), no_ns).unwrap();
        let _probe = super::super::child::install(&mut cmd, None, step).unwrap();
        let script = format!(
            "cat /proc/self/status; echo x > '{}' && echo WROTE",
            outside.display()
        );
        let out = cmd.args(["-c", &script]).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let host = std::fs::read_to_string("/proc/self/status").unwrap();
        assert_eq!(
            seccomp_filters(&stdout),
            seccomp_filters(&host) + 1,
            "{stdout}"
        );
        assert!(stdout.contains("NoNewPrivs:\t1"), "{stdout}");
        assert!(!stdout.contains("WROTE") && !outside.exists(), "{out:?}");
    }

    #[test]
    fn a_unix_socket_allowlist_is_refused_under_seccomp() {
        let base = tempfile::tempdir().unwrap();
        let mut policy = policy(base.path(), None, Network::Allow);
        policy.unix_sockets = vec![base.path().join("s.sock")];
        let err = command_with(&policy, OsStr::new("/bin/true"), kernel(3, true, false))
            .err()
            .unwrap();
        assert!(
            matches!(err, SandboxError::PolicyNotEnforceable { .. }),
            "{err}"
        );
    }
}
