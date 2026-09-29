//! User, mount, IPC and PID namespaces (plus network with `Network::Deny`)
//! for the Linux backend, entered in the forked child before Landlock and
//! seccomp.
//!
//! What each one buys:
//! - user: the others without privilege; the sandbox keeps the caller's uid
//!   and gid, mapped one to one.
//! - mount: each writable root's top-level `.git` is bind-mounted read-only
//!   onto itself, so a worker cannot rewrite or remove its gitlink (EROFS,
//!   EBUSY). Landlock and seccomp forbid unmounting it afterward. A fresh
//!   procfs is mounted on `/proc` inside the PID namespace, so host
//!   processes (their command lines included) are not listed.
//! - PID: processes outside the sandbox have no PID inside it, so `kill`,
//!   `kill(-1)` and `pidfd_open` cannot name them. The command runs as PID 1
//!   of the namespace; when it exits the kernel kills everything it left.
//! - IPC: System V IPC and POSIX message queues of other processes vanish.
//! - network (deny only): no interface but a down loopback. seccomp already
//!   refuses every `socket` call there; this is the second wall.
//!
//! codex-rs gets the same namespaces from bubblewrap (a setuid or bundled
//! binary). kGraf takes the syscalls directly instead, so there is no
//! helper binary to ship, find on PATH or trust.
//!
//! PID namespace mechanics: `unshare(CLONE_NEWPID)` moves only later
//! children, so the child forks once more. The first process (the one std
//! spawned and the caller holds as `Child`) closes every descriptor, waits,
//! and exits with the command's status; its child continues to exec. The
//! grandchild asks for SIGKILL when the waiter dies (`PR_SET_PDEATHSIG`), so
//! killing the process the caller holds, or its process group, ends the
//! whole sandbox.
//!
//! Every function called in the forked child uses only raw syscalls on
//! memory prepared in the parent: no allocation, no locks.

use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::super::Network;
use crate::error::SandboxError;

/// A failed step: a fixed message and the errno.
pub(super) type StepError = (&'static str, i32);

/// Everything the child needs, prepared in the parent.
#[derive(Debug)]
pub(super) struct Plan {
    flags: libc::c_int,
    uid_map: Vec<u8>,
    gid_map: Vec<u8>,
    /// `<root>/.git` of each writable root that has one, bound read-only.
    read_only: Vec<CString>,
    /// Mount a fresh procfs on `/proc` in the PID namespace.
    pub(super) fresh_proc: bool,
}

impl Plan {
    /// A plan for `network` that makes each existing `<root>/.git` read-only.
    /// A root without `.git` (the scratch directory) has nothing to protect;
    /// that a worker can create one there is a reported gap.
    pub(super) fn new(
        network: Network,
        writable: &[std::path::PathBuf],
        fresh_proc: bool,
    ) -> Result<Self, SandboxError> {
        let mut plan = Self::bare(network);
        plan.fresh_proc = fresh_proc;
        for root in writable {
            let gitlink = root.join(".git");
            if std::fs::symlink_metadata(&gitlink).is_ok() {
                // Whether it is a regular file or directory (and not a
                // symlink) is checked in the child, on the descriptor that is
                // then mounted, so a swap in between cannot redirect it.
                plan.read_only.push(c_path(&gitlink)?);
            }
        }
        Ok(plan)
    }

    fn bare(network: Network) -> Self {
        // SAFETY: getuid and getgid cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let mut flags =
            libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWIPC | libc::CLONE_NEWPID;
        if network == Network::Deny {
            flags |= libc::CLONE_NEWNET;
        }
        Self {
            flags,
            uid_map: format!("{uid} {uid} 1\n").into_bytes(),
            gid_map: format!("{gid} {gid} 1\n").into_bytes(),
            read_only: Vec::new(),
            fresh_proc: false,
        }
    }
}

fn c_path(path: &Path) -> Result<CString, SandboxError> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        SandboxError::InvalidPolicy(format!("path {} contains a NUL byte", path.display()))
    })
}

fn errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EINVAL)
}

/// Write `data` to a /proc file in one `write`, as the map files require.
fn write_proc(path: &CStr, data: &[u8], what: &'static str) -> Result<(), StepError> {
    // SAFETY: `path` is NUL-terminated; the fd is closed below.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err((what, errno()));
    }
    // SAFETY: writes `data.len()` bytes from a live slice to the open fd.
    let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
    let err = errno();
    // SAFETY: closes the fd opened above.
    unsafe { libc::close(fd) };
    if usize::try_from(n).ok() == Some(data.len()) {
        Ok(())
    } else {
        Err((what, err))
    }
}

// New mount API (linux/mount.h); libc does not define these for glibc.
const OPEN_TREE_CLONE: libc::c_uint = 1;
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x0000_0004;
const MOVE_MOUNT_T_EMPTY_PATH: libc::c_uint = 0x0000_0040;
const MOUNT_ATTR_RDONLY: u64 = 0x0000_0001;

/// `struct mount_attr` (linux/mount.h, MOUNT_ATTR_SIZE_VER0 = 32).
#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

/// Bind `path` read-only onto itself using descriptors only: open it with
/// O_PATH | O_NOFOLLOW (a symlink is refused, not followed), clone that
/// mount point into a detached tree, set it read-only, and move it onto the
/// same descriptor. No step resolves the path twice. `mount_setattr` only
/// adds the read-only flag, so locked nosuid/nodev/noexec flags are kept.
fn bind_read_only(path: &CStr) -> Result<(), StepError> {
    // SAFETY: raw syscalls on a NUL-terminated path, stack structs and
    // descriptors opened here; every descriptor is closed before return.
    unsafe {
        let fd = libc::open(
            path.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        );
        if fd < 0 {
            return Err(("cannot open the gitlink", errno()));
        }
        let mut st: libc::stat = std::mem::zeroed();
        let kind = if libc::fstat(fd, &mut st) == 0 {
            st.st_mode & libc::S_IFMT
        } else {
            0
        };
        if kind != libc::S_IFREG && kind != libc::S_IFDIR {
            libc::close(fd);
            return Err((
                "the gitlink is not a regular file or directory",
                libc::ELOOP,
            ));
        }
        let flags =
            OPEN_TREE_CLONE | libc::O_CLOEXEC.cast_unsigned() | libc::AT_EMPTY_PATH.cast_unsigned();
        let tree = libc::syscall(libc::SYS_open_tree, fd, c"".as_ptr(), flags);
        let tree = match libc::c_int::try_from(tree) {
            Ok(t) if t >= 0 => t,
            _ => {
                let e = errno();
                libc::close(fd);
                return Err(("open_tree of the gitlink failed", e));
            }
        };
        let attr = MountAttr {
            attr_set: MOUNT_ATTR_RDONLY,
            attr_clr: 0,
            propagation: 0,
            userns_fd: 0,
        };
        let rc = libc::syscall(
            libc::SYS_mount_setattr,
            tree,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH.cast_unsigned(),
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        );
        let result = if rc != 0 {
            Err(("cannot make the gitlink mount read-only", errno()))
        } else if libc::syscall(
            libc::SYS_move_mount,
            tree,
            c"".as_ptr(),
            fd,
            c"".as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
        ) != 0
        {
            Err(("cannot mount the gitlink read-only", errno()))
        } else {
            Ok(())
        };
        libc::close(tree);
        libc::close(fd);
        result
    }
}

/// Enter the namespaces and apply the read-only mounts. Child only.
pub(super) fn enter(plan: &Plan) -> Result<(), StepError> {
    // SAFETY: unshare takes flags only; the child is single-threaded, as
    // CLONE_NEWUSER requires.
    if unsafe { libc::unshare(plan.flags) } != 0 {
        return Err(("unshare of the sandbox namespaces failed", errno()));
    }
    write_proc(
        c"/proc/self/setgroups",
        b"deny",
        "cannot write /proc/self/setgroups",
    )?;
    write_proc(c"/proc/self/uid_map", &plan.uid_map, "cannot write uid_map")?;
    write_proc(c"/proc/self/gid_map", &plan.gid_map, "cannot write gid_map")?;
    // Receive host mount events but never send ours back.
    // SAFETY: mount with static NUL-terminated strings and no data.
    let rc = unsafe {
        libc::mount(
            c"none".as_ptr(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_SLAVE,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(("cannot make the mount namespace a slave", errno()));
    }
    for path in &plan.read_only {
        bind_read_only(path)?;
    }
    Ok(())
}

/// Mount a fresh procfs on `/proc`. Must run in the new PID namespace (the
/// grandchild): procfs shows the PID namespace of the process mounting it.
pub(super) fn mount_fresh_proc() -> Result<(), StepError> {
    // SAFETY: mount with static NUL-terminated strings and no data.
    let rc = unsafe {
        libc::mount(
            c"proc".as_ptr(),
            c"/proc".as_ptr(),
            c"proc".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            std::ptr::null(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(("cannot mount a fresh /proc in the PID namespace", errno()))
    }
}

/// Raw fork: no atfork handlers (they may take locks). Same stack, SIGCHLD
/// to the parent, as fork(2). Returns the pid, 0 in the child, or -1.
fn raw_fork() -> libc::pid_t {
    // SAFETY: clone with only SIGCHLD and null stack/tid pointers behaves
    // as fork; both processes continue on their own copy of the stack.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_clone,
            libc::SIGCHLD as libc::c_ulong,
            0usize,
            0usize,
            0usize,
            0usize,
        )
    };
    libc::pid_t::try_from(rc).unwrap_or(-1)
}

/// Fork into the new PID namespace. Returns in the grandchild (PID 1 of the
/// namespace); the waiting process never returns. Child only.
pub(super) fn fork_into_pid_namespace() -> Result<(), StepError> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe2 writes two descriptors into `fds`.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(("cannot create the liveness pipe", errno()));
    }
    let [alive_r, alive_w] = fds;
    let pid = raw_fork();
    if pid < 0 {
        return Err(("cannot fork into the PID namespace", errno()));
    }
    if pid > 0 {
        wait_and_relay(pid, alive_w);
    }
    // Grandchild. Die with the waiter; if it is already gone (killed between
    // the fork and here), the pipe reports hang-up: stop now.
    // SAFETY: closes our copy of the write end; prctl and poll take plain
    // values and a stack struct.
    unsafe {
        libc::close(alive_w);
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
            return Err(("cannot set the parent-death signal", errno()));
        }
        let mut pfd = libc::pollfd {
            fd: alive_r,
            events: libc::POLLIN,
            revents: 0,
        };
        if libc::poll(&mut pfd, 1, 0) != 0 {
            libc::_exit(1);
        }
        libc::close(alive_r);
    }
    Ok(())
}

/// The waiter: hold `keep` (the liveness pipe) and nothing else, reap the
/// command, and exit the way it did.
fn wait_and_relay(pid: libc::pid_t, keep: libc::c_int) -> ! {
    // SAFETY: only syscalls on plain values and a stack int. close_range
    // exists since Linux 5.9; Landlock ABI 3 needs 6.2.
    unsafe {
        let keep = keep as libc::c_uint;
        // Close std's exec-status pipe, the setup reporter and stdio, so the
        // caller sees exec success and EOF from the command alone.
        if keep > 0 {
            libc::syscall(libc::SYS_close_range, 0u32, keep - 1, 0u32);
        }
        libc::syscall(libc::SYS_close_range, keep + 1, u32::MAX, 0u32);
        let mut status: libc::c_int = 0;
        loop {
            let rc = libc::waitpid(pid, &mut status, 0);
            if rc == pid {
                break;
            }
            if rc < 0 && errno() != libc::EINTR {
                libc::_exit(127);
            }
        }
        if libc::WIFEXITED(status) {
            libc::_exit(libc::WEXITSTATUS(status));
        }
        if libc::WIFSIGNALED(status) {
            let sig = libc::WTERMSIG(status);
            // Die by the same signal, without a second core dump.
            let no_core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &no_core);
            libc::signal(sig, libc::SIG_DFL);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, sig);
            libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::kill(libc::getpid(), sig);
            libc::_exit(128 + sig);
        }
        libc::_exit(127)
    }
}

/// What the namespace probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Probe {
    /// The namespaces, the uid map and the PID-namespace fork all work.
    pub(super) namespaces: bool,
    /// A fresh procfs can be mounted inside them. Fails where `/proc` has
    /// locked overmounts (Docker's masked paths, for one).
    pub(super) fresh_proc: bool,
}

/// What this kernel and its security modules let an unprivileged process
/// do: runs the same `enter`, the PID-namespace fork and the procfs mount in
/// a throwaway child. Ubuntu's AppArmor userns restriction, for one, lets
/// `unshare` succeed and then fails the uid map, so only the whole sequence
/// is a real answer.
pub(super) fn available() -> Probe {
    const NO_PROC: i32 = 3;
    let plan = Plan::bare(Network::Deny);
    // SAFETY: the child calls only async-signal-safe functions (raw
    // syscalls on memory prepared above) and always leaves through _exit.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Probe {
            namespaces: false,
            fresh_proc: false,
        };
    }
    if pid == 0 {
        let code = match enter(&plan) {
            Err(_) => 1,
            Ok(()) => {
                let inner = raw_fork();
                if inner == 0 {
                    let code = if mount_fresh_proc().is_ok() {
                        0
                    } else {
                        NO_PROC
                    };
                    // SAFETY: leaves the grandchild without running
                    // destructors or atexit handlers.
                    unsafe { libc::_exit(code) };
                }
                let mut status = 0;
                // SAFETY: waits on our own child.
                let exited = inner > 0
                    && unsafe { libc::waitpid(inner, &mut status, 0) } == inner
                    && libc::WIFEXITED(status);
                if exited { libc::WEXITSTATUS(status) } else { 1 }
            }
        };
        // SAFETY: as above.
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    loop {
        // SAFETY: waits on the child forked above.
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        if rc == pid {
            break;
        }
        if rc < 0 && errno() != libc::EINTR {
            status = 1 << 8;
            break;
        }
    }
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        1
    };
    Probe {
        namespaces: code == 0 || code == NO_PROC,
        fresh_proc: code == 0,
    }
}
