//! Child-side setup between fork and exec, shared by the Unix backends.
//!
//! Every child calls `setsid()` first: a new session has no controlling
//! terminal, so the child cannot inject input into the user's terminal with
//! TIOCSTI. A backend may add one more step (Linux: Landlock).
//!
//! std reports only an errno across the fork boundary, so a failing step
//! also writes a fixed message into a close-on-exec pipe. The parent reads it
//! back through [`SetupProbe`] after a failed spawn. A successful exec closes
//! the child's end, so a probe on a running command reads nothing.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::Command;

use std::os::unix::process::CommandExt;

use crate::error::SandboxError;

/// A worktree's hold: its lock file and its session log (see [`install`]).
pub(super) struct Hold {
    pub(super) lock: std::ffi::CString,
    pub(super) sessions: std::ffi::CString,
    /// `<boot id> <pid ns> <time ns>` of the spawning process (`-` for an
    /// unreadable part), written with each session: the boot tells a log from
    /// before a reboot, the PID namespace tells a line a reader elsewhere
    /// cannot check, and the time namespace tells when start times compare.
    /// The child logs before it enters any namespace, so these are its own.
    pub(super) host: Vec<u8>,
}

/// Append `n` in decimal to `buf` at `*at`. No allocation.
fn put_u64(buf: &mut [u8], at: &mut usize, mut n: u64) {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for d in &digits[i..] {
        if *at < buf.len() {
            buf[*at] = *d;
            *at += 1;
        }
    }
}

fn put(buf: &mut [u8], at: &mut usize, bytes: &[u8]) {
    for b in bytes {
        if *at < buf.len() {
            buf[*at] = *b;
            *at += 1;
        }
    }
}

/// Handed to a child step so it can report why it failed.
pub(super) struct Reporter(RawFd);

impl Reporter {
    /// Write `msg` to the pipe and return `errno` as the spawn error. Only
    /// async-signal-safe calls: one `write`, no allocation.
    pub(super) fn fail(&self, msg: &'static str, errno: i32) -> io::Error {
        // SAFETY: writes `msg.len()` bytes from a valid static buffer to a fd
        // owned by the enclosing closure. A short or failed write only loses
        // the message, not the error.
        unsafe {
            libc::write(self.0, msg.as_ptr().cast(), msg.len());
        }
        io::Error::from_raw_os_error(errno)
    }
}

/// A backend's extra step, run after `setsid` in the child.
pub(super) type ChildStep = Box<dyn FnMut(&Reporter) -> io::Result<()> + Send + Sync>;

/// Reads the reason a child's setup failed.
#[derive(Debug)]
pub struct SetupProbe {
    read: OwnedFd,
}

impl SetupProbe {
    /// The child's message, if its setup failed. Non-blocking.
    pub fn reason(&self) -> Option<String> {
        let mut buf = [0u8; 512];
        // SAFETY: reads into a local buffer of the stated length from a fd
        // this probe owns; the fd is non-blocking.
        let n = unsafe { libc::read(self.read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        let n = usize::try_from(n).ok().filter(|n| *n > 0)?;
        Some(String::from_utf8_lossy(&buf[..n]).into_owned())
    }
}

fn last_errno() -> i32 {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EINVAL)
}

fn pipe_io_error(source: io::Error) -> SandboxError {
    SandboxError::Io {
        path: "pipe".into(),
        source,
    }
}

/// Install `setsid`, the hold on `hold` (when given), then `step` as the
/// command's pre-exec hook. The hook consumes nothing, but a Linux step
/// consumes its ruleset: spawn once.
///
/// **The hold.** The child opens `hold` without close-on-exec and takes a
/// shared `flock` on it, so the descriptor, and the lock, pass to the program
/// and to every process it starts. The lock is released only when the last of
/// them exits. `setsid` detaches a child from its parent's death (macOS has no
/// `PR_SET_PDEATHSIG`), so after the orchestrator is killed the command, and on
/// macOS any orphan it leaves, can keep writing its worktree;
/// `WorkspaceManager::reopen` takes the lock exclusively first and refuses
/// while any holder lives. On Linux with namespaces the command is PID 1 of
/// its own PID namespace, so an orphan cannot outlive the command: the kernel
/// kills everything left in the namespace when the command exits. The command
/// itself still outlives a killed orchestrator there, and so do its children
/// while it runs. Opened in the child, not the parent, so no descriptor of the
/// orchestrator's own keeps the lock.
///
/// **The session log.** A descriptor is escaped by any program that closes
/// the ones it inherits (Python's `subprocess` with `close_fds=True`, which is
/// its default, or a daemonizing tool). So the child also appends its PID,
/// which after `setsid` is its session id, to the hold's session log; reopen
/// refuses while any process of a logged session runs. What escapes both: a
/// process that closes its descriptors **and** calls `setsid` again, leaving
/// the session. It guards against orphans, not against a worker hiding one.
/// Since decision 0084 a worker's build script or test can do exactly that
/// at will: with a PID namespace (Linux) the kernel still kills such a
/// process with its command; elsewhere it is a known gap, recorded in that
/// decision (no reliable way was found to list the processes of one Seatbelt
/// sandbox), and only the quality gate on the merged tree judges what merges.
pub(super) fn install(
    cmd: &mut Command,
    hold: Option<Hold>,
    mut step: Option<ChildStep>,
) -> Result<SetupProbe, SandboxError> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` has room for the two descriptors pipe(2) writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(pipe_io_error(io::Error::last_os_error()));
    }
    // SAFETY: pipe(2) succeeded, so both descriptors are open and owned here.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // No pipe2 on macOS: a concurrent spawn elsewhere in the process could
    // inherit these fds in the window before FD_CLOEXEC is set. Harmless: it
    // would only hold a copy of an empty pipe.
    for fd in [&read, &write] {
        // SAFETY: fcntl on descriptors owned above.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(pipe_io_error(io::Error::last_os_error()));
        }
    }
    // SAFETY: as above.
    let flags = unsafe { libc::fcntl(read.as_raw_fd(), libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0
        || unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0
    {
        return Err(pipe_io_error(io::Error::last_os_error()));
    }

    // SAFETY: the hook runs in the forked child before exec and calls only
    // setsid, write and the step (documented async-signal-safe by each
    // backend), plus open and flock for the hold. It touches no process locks
    // and does not allocate.
    unsafe {
        cmd.pre_exec(move || {
            let reporter = Reporter(write.as_raw_fd());
            if libc::setsid() == -1 {
                return Err(reporter.fail(
                    "setsid failed: cannot detach from the terminal",
                    last_errno(),
                ));
            }
            let mut hold_fd: libc::c_int = -1;
            if let Some(hold) = hold.as_ref() {
                // No O_CLOEXEC: the descriptor must survive exec.
                let fd = libc::open(hold.lock.as_ptr(), libc::O_RDONLY);
                hold_fd = fd;
                if fd < 0 {
                    return Err(reporter.fail("cannot open the worktree hold file", last_errno()));
                }
                if libc::flock(fd, libc::LOCK_SH) != 0 {
                    return Err(reporter.fail("cannot take the worktree hold", last_errno()));
                }
                // `<session id> <leader start time> <boot> <pid ns> <time ns>`:
                // this PID is the session id after setsid; the start time and
                // boot tell a recycled PID or a log from before a reboot apart,
                // the namespaces tell a reader elsewhere what it cannot check
                // (see `Hold::host`). Built in a stack buffer: no allocation
                // after fork.
                let log = libc::open(
                    hold.sessions.as_ptr(),
                    libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT | libc::O_CLOEXEC,
                    0o600 as libc::c_uint,
                );
                if log < 0 {
                    return Err(reporter.fail("cannot open the worktree session log", last_errno()));
                }
                let mut buf = [0u8; 256];
                let mut at = 0;
                let pid = libc::getpid();
                put_u64(&mut buf, &mut at, pid as u64);
                put(&mut buf, &mut at, b" ");
                match crate::workspace::start_time_after_fork(pid as u32) {
                    Some(t) => put_u64(&mut buf, &mut at, t),
                    None => put(&mut buf, &mut at, b"-"),
                }
                put(&mut buf, &mut at, b" ");
                put(&mut buf, &mut at, &hold.host);
                put(&mut buf, &mut at, b"\n");
                let len = at;
                let written = libc::write(log, buf.as_ptr().cast(), len);
                libc::close(log);
                if written != len as isize {
                    return Err(reporter.fail("cannot log the worktree session", last_errno()));
                }
            }
            if let Some(step) = step.as_mut() {
                step(&reporter)?;
            }
            // A backend step may mark every inherited descriptor close-on-exec
            // (Linux does, so no leaked descriptor passes the sandbox); the
            // hold alone is kept open across exec. It is a read-only
            // descriptor of an empty lock file under the state directory:
            // nothing to read, and a lock is all it carries. Its number is
            // whatever `open` returned; nothing assumes it is low.
            if hold_fd >= 0 && libc::fcntl(hold_fd, libc::F_SETFD, 0) != 0 {
                return Err(
                    reporter.fail("cannot keep the worktree hold across exec", last_errno())
                );
            }
            Ok(())
        });
    }
    Ok(SetupProbe { read })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_step_reports_its_reason_through_the_pipe() {
        let mut cmd = Command::new("/usr/bin/true");
        let step: ChildStep =
            Box::new(|r: &Reporter| Err(r.fail("probe step refused", libc::EPERM)));
        let probe = install(&mut cmd, None, Some(step)).unwrap();
        let err = cmd.spawn().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EPERM));
        assert_eq!(probe.reason().as_deref(), Some("probe step refused"));
    }

    #[test]
    fn a_successful_spawn_reports_nothing() {
        let mut cmd = Command::new("/usr/bin/true");
        let probe = install(&mut cmd, None, None).unwrap();
        assert!(cmd.status().unwrap().success());
        assert_eq!(probe.reason(), None);
    }
}
