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
    /// [`Network::Proxy`]: the socket the loopback listener is handed to
    /// kGraf through, and the port it listens on.
    proxy: Option<(libc::c_int, u16)>,
    /// Folders hidden under an empty file system ([`super::super::SandboxPolicy::hide`]).
    hidden: Vec<HiddenPlan>,
}

/// Most writable roots one hidden folder may hold: the descriptors of their
/// clones live on the child's stack.
pub(super) const MAX_KEPT: usize = 8;

/// One hidden folder, prepared in the parent.
#[derive(Debug)]
struct HiddenPlan {
    dir: CString,
    /// Each writable root beneath `dir`, with the folders to make on the
    /// empty file system on the way to it, outermost first (each absolute;
    /// the last is the root itself).
    keep: Vec<(CString, Vec<CString>)>,
}

impl Plan {
    /// A plan for `network` that makes each existing `<root>/.git` read-only.
    /// A root without `.git` (the scratch directory) has nothing to protect;
    /// that a worker can create one there is a reported gap.
    pub(super) fn new(
        network: Network,
        writable: &[std::path::PathBuf],
        fresh_proc: bool,
        handoff: Option<libc::c_int>,
        hidden: &[super::super::Hidden],
    ) -> Result<Self, SandboxError> {
        let mut plan = Self::bare(network);
        plan.fresh_proc = fresh_proc;
        if let Network::Proxy { port } = network {
            let fd = handoff.ok_or_else(|| {
                SandboxError::InvalidPolicy("a proxy network needs its handoff socket".into())
            })?;
            plan.proxy = Some((fd, port));
        }
        for h in hidden {
            if h.keep.len() > MAX_KEPT {
                return Err(SandboxError::InvalidPolicy(format!(
                    "hidden folder {} holds more than {MAX_KEPT} writable roots",
                    h.dir.display()
                )));
            }
            let mut keep = Vec::new();
            for root in &h.keep {
                let rel = root.strip_prefix(&h.dir).map_err(|_| {
                    SandboxError::InvalidPolicy(format!(
                        "{} is not beneath {}",
                        root.display(),
                        h.dir.display()
                    ))
                })?;
                let mut dirs = Vec::new();
                let mut at = h.dir.clone();
                for part in rel.components() {
                    at.push(part);
                    dirs.push(c_path(&at)?);
                }
                keep.push((c_path(root)?, dirs));
            }
            plan.hidden.push(HiddenPlan {
                dir: c_path(&h.dir)?,
                keep,
            });
        }
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
        if matches!(network, Network::Deny | Network::Proxy { .. }) {
            flags |= libc::CLONE_NEWNET;
        }
        Self {
            flags,
            uid_map: format!("{uid} {uid} 1\n").into_bytes(),
            gid_map: format!("{gid} {gid} 1\n").into_bytes(),
            read_only: Vec::new(),
            fresh_proc: false,
            proxy: None,
            hidden: Vec::new(),
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
    for hidden in &plan.hidden {
        hide(hidden)?;
    }
    if let Some((fd, port)) = plan.proxy {
        hand_off_loopback_listener(fd, port)?;
    }
    Ok(())
}

/// Cover `plan.dir` with an empty, read-only tmpfs, then mount each kept
/// writable root again at its own path on top. The roots are cloned (as
/// detached bind mounts, from their descriptors) before the cover goes on,
/// since their paths lead into the covered folder afterwards. Child only.
fn hide(plan: &HiddenPlan) -> Result<(), StepError> {
    let mut trees = [-1 as libc::c_int; MAX_KEPT];
    let close_all = |trees: &[libc::c_int; MAX_KEPT]| {
        for fd in trees.iter().filter(|fd| **fd >= 0) {
            // SAFETY: closes a descriptor opened below.
            unsafe { libc::close(*fd) };
        }
    };
    for (i, (root, _)) in plan.keep.iter().enumerate() {
        let flags = OPEN_TREE_CLONE | libc::O_CLOEXEC.cast_unsigned();
        // SAFETY: open_tree on a NUL-terminated path from the parent.
        let tree =
            unsafe { libc::syscall(libc::SYS_open_tree, libc::AT_FDCWD, root.as_ptr(), flags) };
        match libc::c_int::try_from(tree) {
            Ok(t) if t >= 0 => trees[i] = t,
            _ => {
                let e = errno();
                close_all(&trees);
                return Err(("cannot clone a writable root beneath a hidden folder", e));
            }
        }
    }
    let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    // SAFETY: mount with NUL-terminated strings from the parent or static.
    let rc = unsafe {
        libc::mount(
            c"tmpfs".as_ptr(),
            plan.dir.as_ptr(),
            c"tmpfs".as_ptr(),
            flags,
            c"size=64k,mode=0755".as_ptr().cast(),
        )
    };
    if rc != 0 {
        let e = errno();
        close_all(&trees);
        return Err(("cannot cover a hidden folder", e));
    }
    for (i, (root, dirs)) in plan.keep.iter().enumerate() {
        for dir in dirs {
            // SAFETY: mkdir on a NUL-terminated path from the parent.
            if unsafe { libc::mkdir(dir.as_ptr(), 0o755) } != 0 && errno() != libc::EEXIST {
                let e = errno();
                close_all(&trees);
                return Err(("cannot make a folder on a hidden folder's cover", e));
            }
        }
        // SAFETY: moves the detached clone opened above onto a path from the
        // parent; the clone's descriptor is closed below.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_move_mount,
                trees[i],
                c"".as_ptr(),
                libc::AT_FDCWD,
                root.as_ptr(),
                MOVE_MOUNT_F_EMPTY_PATH,
            )
        };
        if rc != 0 {
            let e = errno();
            close_all(&trees);
            return Err(("cannot mount a writable root beneath a hidden folder", e));
        }
    }
    close_all(&trees);
    // SAFETY: remount with a NUL-terminated path from the parent, no data.
    let rc = unsafe {
        libc::mount(
            std::ptr::null(),
            plan.dir.as_ptr(),
            std::ptr::null(),
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | flags,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(("cannot make a hidden folder's cover read-only", errno()));
    }
    Ok(())
}

/// [`Network::Proxy`]: bring the new network namespace's loopback up, open a
/// listening TCP socket on `127.0.0.1:port` in it, and send that socket to
/// kGraf over `handoff` (`SCM_RIGHTS`). kGraf accepts on it from outside
/// the namespace and serves the egress proxy there; the command's own
/// connections to that port are the only ones that leave the namespace. A
/// socket keeps the namespace it was made in, so the listener stays inside
/// it after it is passed. Child only, before Landlock and seccomp.
///
/// Written new. Anthropic's sandbox-runtime bridges the same gap with socat
/// relays over a unix socket bound into the namespace; here the seccomp
/// filter refuses unix sockets, and a passed listener needs no helper
/// process.
fn hand_off_loopback_listener(handoff: libc::c_int, port: u16) -> Result<(), StepError> {
    // SAFETY: raw syscalls on stack structs and descriptors opened here;
    // every descriptor is closed before return.
    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if s < 0 {
            return Err(("cannot open a socket to bring the loopback up", errno()));
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        for (dst, src) in ifr.ifr_name.iter_mut().zip(b"lo\0") {
            *dst = *src as libc::c_char;
        }
        if libc::ioctl(s, libc::SIOCGIFFLAGS as _, &mut ifr) != 0 {
            let e = errno();
            libc::close(s);
            return Err(("cannot read the loopback's flags", e));
        }
        ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        if libc::ioctl(s, libc::SIOCSIFFLAGS as _, &ifr) != 0 {
            let e = errno();
            libc::close(s);
            return Err(("cannot bring the loopback up", e));
        }
        libc::close(s);
        let l = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if l < 0 {
            return Err(("cannot open the proxy listener", errno()));
        }
        let addr = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from_be_bytes([127, 0, 0, 1]).to_be(),
            },
            sin_zero: [0; 8],
        };
        if libc::bind(
            l,
            (&addr as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ) != 0
            || libc::listen(l, 128) != 0
        {
            let e = errno();
            libc::close(l);
            return Err(("cannot listen on the namespace's loopback", e));
        }
        // One byte of data carries the descriptor.
        #[repr(C, align(8))]
        struct Control([u8; 64]);
        let mut control = Control([0; 64]);
        let mut byte = [b'L'];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.0.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            libc::close(l);
            return Err(("cannot build the listener handoff", libc::EINVAL));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
        std::ptr::copy_nonoverlapping(
            (&l as *const libc::c_int).cast::<u8>(),
            libc::CMSG_DATA(cmsg),
            std::mem::size_of::<libc::c_int>(),
        );
        let sent = libc::sendmsg(handoff, &msg, libc::MSG_NOSIGNAL);
        let e = errno();
        libc::close(l);
        libc::close(handoff);
        if sent != 1 {
            return Err(("cannot hand the proxy listener to kGraf", e));
        }
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
