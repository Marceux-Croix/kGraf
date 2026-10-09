//! Child-side setup between fork and exec, shared by the Unix backends, and
//! the worktree hold the parent takes before the fork.
//!
//! Every child calls `setsid()` first: a new session has no controlling
//! terminal, so the child cannot inject input into the user's terminal with
//! TIOCSTI. A backend may add one more step (Linux: Landlock).
//!
//! std reports only an errno across the fork boundary, so a failing step
//! also writes a fixed message into a close-on-exec pipe. The parent reads it
//! back through [`SetupProbe`] after a failed spawn. A successful exec closes
//! the child's end, so a probe on a running command reads nothing.
//!
//! Nothing between fork and exec waits. A child that waits there keeps its
//! copies of every descriptor the parent had open at the fork, among them
//! the close-on-exec pipe through which another thread's concurrent spawn
//! learns that its own exec succeeded, so that spawn would wait as long as
//! this child. The worktree hold, which can wait, is taken in the parent
//! ([`HoldGate`]).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use std::os::unix::process::CommandExt;

use crate::error::SandboxError;

/// A worktree's hold as the forked child sees it (see [`install`]).
pub(super) struct Hold {
    /// The descriptor this spawn's shared lock was taken on in the parent
    /// ([`HoldGate::take_until`]), or -1 while no spawn holds it.
    pub(super) fd: Arc<AtomicI32>,
    pub(super) sessions: std::ffi::CString,
    /// `<boot id> <pid ns> <time ns>` of the spawning process (`-` for an
    /// unreadable part), written with each session: the boot tells a log from
    /// before a reboot, the PID namespace tells a line a reader elsewhere
    /// cannot check, and the time namespace tells when start times compare.
    /// The child logs before it enters any namespace, so these are its own.
    pub(super) host: Vec<u8>,
}

/// The longest a starting command waits for its worktree's hold. It is held
/// exclusively only while nothing may start in the worktree: a quiesce
/// guard through one merge-back commit, a removal through `git worktree
/// remove` and the deletion of the tree, a reopen or startup cleanup. One
/// such holder may wait 30 minutes for the worktree-admin lock and then run
/// git commands bounded at 10 minutes each, or delete a large build folder;
/// an hour covers it, so a wait this long means a holder that is stuck.
pub(super) const HOLD_WAIT: Duration = Duration::from_secs(60 * 60);

/// The message of a child forked with a hold its parent did not take.
const HOLD_NOT_TAKEN: &str =
    "the worktree hold was not taken before the spawn; take it with the command's hold gate";

/// The longest pause between two tries for a hold.
const HOLD_POLL_MAX: Duration = Duration::from_millis(20);

/// Takes a command's worktree hold in the parent, before the fork.
///
/// A shared `flock` belongs to the open file, not to a process: the parent's
/// descriptor and the copy the child inherits carry one lock. The child
/// keeps its copy across exec ([`install`]), so the command and every process
/// it starts hold the lock until the last of them exits. The parent closes
/// its own copy once the spawn returns ([`SpawnHold`]) and never unlocks it:
/// an unlock would release the command's lock as well.
///
/// The wait is a sleeping thread of the parent that holds no descriptor of
/// another spawn, so a hold held exclusively delays only the commands that
/// start in its worktree.
#[derive(Debug, Clone, Default)]
pub struct HoldGate(Option<Gate>);

#[derive(Debug, Clone)]
struct Gate {
    path: PathBuf,
    fd: Arc<AtomicI32>,
    wait: Duration,
}

impl HoldGate {
    pub(super) fn new(path: PathBuf, fd: Arc<AtomicI32>, wait: Duration) -> Self {
        Self(Some(Gate { path, fd, wait }))
    }

    /// Take the hold shared for one spawn of the command, waiting at most
    /// [`HOLD_WAIT`] while another process holds it exclusively, then fail
    /// with [`SandboxError::HoldTimeout`], which names the hold file. Blocks
    /// the calling thread: from async code, call it on a blocking thread.
    ///
    /// `Ok(None)` once `stop` returns true, checked between tries (every
    /// 20 ms at most), so a cancelled caller does not leave a thread waiting.
    /// A command without a hold gets an empty [`SpawnHold`] at once.
    pub fn take_until(&self, stop: impl Fn() -> bool) -> Result<Option<SpawnHold>, SandboxError> {
        let Some(gate) = &self.0 else {
            return Ok(Some(SpawnHold(None)));
        };
        let io_err = |source| SandboxError::Io {
            path: gate.path.clone(),
            source,
        };
        // std opens with O_CLOEXEC: a child forked by another thread in this
        // window drops its copy at its own exec.
        let file = std::fs::File::open(&gate.path).map_err(io_err)?;
        let start = Instant::now();
        let mut pause = Duration::from_millis(1);
        loop {
            match file.try_lock_shared() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(e)) => return Err(io_err(e)),
            }
            if stop() {
                return Ok(None);
            }
            let waited = start.elapsed();
            if waited >= gate.wait {
                return Err(SandboxError::HoldTimeout {
                    path: gate.path.clone(),
                    waited,
                });
            }
            std::thread::sleep(pause.min(gate.wait - waited));
            pause = (pause * 2).min(HOLD_POLL_MAX);
        }
        // One spawn at a time: the child reads the one slot.
        if gate
            .fd
            .compare_exchange(-1, file.as_raw_fd(), Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SandboxError::Spawn(io::Error::other(
                "the command's worktree hold is already taken for another spawn",
            )));
        }
        Ok(Some(SpawnHold(Some((file, Arc::clone(&gate.fd))))))
    }

    /// [`Self::take_until`] with no stop.
    pub fn take(&self) -> Result<SpawnHold, SandboxError> {
        match self.take_until(|| false)? {
            Some(held) => Ok(held),
            // Unreachable: the stop never fires. Named, not assumed.
            None => Err(SandboxError::Spawn(io::Error::other(
                "the wait for the worktree hold stopped with no stop asked",
            ))),
        }
    }
}

/// A hold taken for one spawn ([`HoldGate::take_until`]). Drop it once the
/// spawn has returned: the started command keeps the lock through its own
/// descriptor. Dropping closes this process's descriptor and never unlocks.
#[derive(Debug)]
pub struct SpawnHold(Option<(std::fs::File, Arc<AtomicI32>)>);

impl Drop for SpawnHold {
    fn drop(&mut self) {
        if let Some((file, fd)) = self.0.take() {
            // Cleared before the close, so a later fork of this command
            // never reads a number that may name another file.
            fd.store(-1, Ordering::Release);
            drop(file);
        }
    }
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

/// Install `setsid`, the hold (when given), then `step` as the command's
/// pre-exec hook. The hook consumes nothing, but a Linux step consumes its
/// ruleset: spawn once.
///
/// **The hold.** The parent takes a shared `flock` on the hold file before the
/// fork ([`HoldGate`]); the child keeps the inherited descriptor open across
/// exec, so the lock passes to the program and to every process it starts. It
/// is released only when the last of them exits. A child whose parent did not
/// take the hold fails its spawn with `ENOLCK` and says so: a command never
/// runs in a worktree unheld. `setsid` detaches a child from its parent's
/// death (macOS has no `PR_SET_PDEATHSIG`), so after the orchestrator is
/// killed the command, and on macOS any orphan it leaves, can keep writing
/// its worktree; `WorkspaceManager::reopen` takes the lock exclusively first
/// and refuses while any holder lives. On Linux with namespaces the command
/// is PID 1 of its own PID namespace, so an orphan cannot outlive the
/// command: the kernel kills everything left in the namespace when the
/// command exits. The command itself still outlives a killed orchestrator
/// there, and so do its children while it runs.
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
    // backend), plus an atomic load, open, getpid, close and fcntl for the
    // hold. It waits for nothing, touches no process locks and does not
    // allocate.
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
                hold_fd = hold.fd.load(Ordering::Acquire);
                if hold_fd < 0 {
                    return Err(reporter.fail(HOLD_NOT_TAKEN, libc::ENOLCK));
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
            // The parent opened the hold close-on-exec, and a backend step may
            // mark every inherited descriptor close-on-exec (Linux does, so no
            // leaked descriptor passes the sandbox); the hold alone is kept
            // open across exec, in this child only. It is a read-only
            // descriptor of an empty lock file under the state directory:
            // nothing to read, and a lock is all it carries. Its number is
            // whatever the parent's `open` returned; nothing assumes it is low.
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

    /// A hold in `dir`, its gate waiting at most `wait`.
    fn hold_in(dir: &std::path::Path, wait: Duration) -> (Hold, HoldGate) {
        use std::os::unix::ffi::OsStrExt;
        let lock = dir.join("hold");
        std::fs::File::create(&lock).unwrap();
        let fd = Arc::new(AtomicI32::new(-1));
        let hold = Hold {
            fd: Arc::clone(&fd),
            sessions: std::ffi::CString::new(dir.join("sessions").as_os_str().as_bytes()).unwrap(),
            host: b"- - -".to_vec(),
        };
        (hold, HoldGate::new(lock, fd, wait))
    }

    /// Run `f` on a thread, so a wait that never ends fails the test instead
    /// of hanging it.
    fn within<T: Send + 'static>(bound: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(bound)
            .expect("the call returned within the bound")
    }

    #[test]
    fn a_hold_held_exclusively_fails_the_take_at_its_bound_naming_the_hold() {
        let dir = tempfile::tempdir().unwrap();
        let wait = Duration::from_millis(300);
        let (_hold, gate) = hold_in(dir.path(), wait);
        let holder = std::fs::File::open(dir.path().join("hold")).unwrap();
        holder.lock().unwrap();
        let started = Instant::now();
        let taken = {
            let gate = gate.clone();
            within(Duration::from_secs(20), move || gate.take())
        };
        let err = taken.unwrap_err();
        assert!(started.elapsed() >= wait, "{:?}", started.elapsed());
        match &err {
            SandboxError::HoldTimeout { path, .. } => {
                assert_eq!(path, &dir.path().join("hold"));
            }
            other => panic!("expected a hold timeout, got {other:?}"),
        }
        assert!(err.to_string().contains("hold"), "{err}");

        // Free again: the next command takes its shared hold and runs.
        holder.unlock().unwrap();
        let (hold, gate) = hold_in(dir.path(), Duration::from_secs(5));
        let mut cmd = Command::new("/usr/bin/true");
        let probe = install(&mut cmd, Some(hold), None).unwrap();
        let held = gate.take().unwrap();
        let status = cmd.status().unwrap();
        drop(held);
        assert!(status.success());
        assert_eq!(probe.reason(), None);
    }

    #[test]
    fn a_stop_ends_the_wait_without_the_hold() {
        let dir = tempfile::tempdir().unwrap();
        let (_hold, gate) = hold_in(dir.path(), Duration::from_secs(600));
        let holder = std::fs::File::open(dir.path().join("hold")).unwrap();
        holder.lock().unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiting = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || gate.take_until(|| stop.load(Ordering::Relaxed)))
        };
        std::thread::sleep(Duration::from_millis(50));
        stop.store(true, Ordering::Relaxed);
        let taken = within(Duration::from_secs(20), move || waiting.join().unwrap());
        assert!(matches!(taken, Ok(None)), "{taken:?}");
    }

    #[test]
    fn a_shared_hold_does_not_wait_for_another_shared_holder() {
        let dir = tempfile::tempdir().unwrap();
        let (hold, gate) = hold_in(dir.path(), Duration::from_millis(300));
        let other = std::fs::File::open(dir.path().join("hold")).unwrap();
        other.lock_shared().unwrap();
        let mut cmd = Command::new("/usr/bin/true");
        let probe = install(&mut cmd, Some(hold), None).unwrap();
        let held = gate.take().unwrap();
        assert!(cmd.status().unwrap().success());
        drop(held);
        assert_eq!(probe.reason(), None);
    }

    #[test]
    fn a_spawn_whose_hold_was_not_taken_fails_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (hold, _gate) = hold_in(dir.path(), Duration::from_millis(300));
        let mut cmd = Command::new("/usr/bin/true");
        let probe = install(&mut cmd, Some(hold), None).unwrap();
        let err = cmd.spawn().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOLCK), "{err}");
        assert_eq!(probe.reason().as_deref(), Some(HOLD_NOT_TAKEN));
    }

    /// The command keeps the lock after the parent drops its copy, and the
    /// lock goes when the command exits.
    #[test]
    fn the_command_keeps_the_hold_after_the_parent_drops_its_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (hold, gate) = hold_in(dir.path(), Duration::from_secs(5));
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("2");
        let _probe = install(&mut cmd, Some(hold), None).unwrap();
        let held = gate.take().unwrap();
        let mut child = cmd.spawn().unwrap();
        drop(held);
        let probe = std::fs::File::open(dir.path().join("hold")).unwrap();
        assert!(
            matches!(probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "the running command holds the hold"
        );
        child.wait().unwrap();
        probe.try_lock().expect("free once the command exited");
    }
}
