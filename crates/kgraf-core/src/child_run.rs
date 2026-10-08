//! Run a child process to its end, bounded, and collect its output.
//!
//! `Command::output` and `Child::wait_with_output` wait until the child's
//! output pipes close, and nothing bounds that. A pipe closes when every
//! copy of its write end is closed, not when the child exits: a process the
//! child started, or any process that got a copy of the handle, keeps it
//! open. On Windows every inheritable handle in a process is copied into
//! every child created with handle inheritance while it exists, so a
//! concurrent spawn can take a copy of another command's pipe. The caller
//! then waits for an unrelated process, or forever.
//!
//! [`run`] bounds every step:
//!
//! - The child leads a process tree: its own process group on Unix
//!   (`process_group(0)`, set through `posix_spawn`, so no fork path), a job
//!   object on Windows (killed when its last handle closes).
//! - Output is read on two threads that pass chunks over a channel; standard
//!   input is written from a third. Waiting for exit happens on a fourth
//!   thread, which owns the child. The calling thread only waits on the
//!   channel (`recv_timeout`), so nothing it does polls or sleeps.
//! - At the deadline the whole tree is killed. A kill that fails, or a tree
//!   that has not ended [`Limits::kill_bound`] after it, is an error naming
//!   the command.
//! - When the child exits, what is left of its tree is killed (on Unix
//!   before the leader is reaped, so its group id cannot be reused). Then
//!   the readers get [`Limits::drain_grace`] to reach the end of the pipes.
//!   A pipe still open after that is held by a process outside the tree;
//!   what the child wrote is already in the pipe and has been read, so the
//!   output is returned, marked [`Finished::pipes_held`], and recorded
//!   ([`crate::waits::note_pipes_held`]).
//!
//! A thread that cannot finish (a reader on a pipe a stranger holds, a
//! waiter on a process the kernel will not end) is left behind rather than
//! joined: it holds nothing but its pipe or its child.
//!
//! Known limit (Windows): the child is placed in its job just after it
//! starts, so a process it starts in the first instant would escape the job.
//! Git starts nothing before it has read its configuration.

use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// How long a killed tree has to end before [`RunError::Stuck`]. A killed
/// process ends in milliseconds; 10 s covers a loaded machine flushing a
/// large process's memory.
pub const KILL_BOUND: Duration = Duration::from_secs(10);

/// How long the readers get, after the tree has ended, to reach the end of
/// the output. The bytes are already in the pipe, so this is only the time
/// for a reader thread to be scheduled: microseconds normally, and 5 s
/// covers a machine with every core busy.
pub const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// The bounds of one [`run`].
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Killed after this.
    pub timeout: Duration,
    /// See [`KILL_BOUND`].
    pub kill_bound: Duration,
    /// See [`DRAIN_GRACE`].
    pub drain_grace: Duration,
}

impl Limits {
    /// `timeout` with the default kill bound and drain grace.
    pub const fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            kill_bound: KILL_BOUND,
            drain_grace: DRAIN_GRACE,
        }
    }
}

/// A child that ran to its end.
#[derive(Debug)]
pub struct Finished {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// An output pipe stayed open after the tree ended (see the module
    /// documentation). The output is what the child wrote.
    pub pipes_held: bool,
}

/// Why [`run`] did not return a finished child.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("cannot start {command}: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{command} did not finish within {after:?}; its process tree was killed")]
    TimedOut { command: String, after: Duration },
    #[error("{command}: killing its process tree failed: {source}")]
    Kill {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{command}: its process tree had not ended {waited:?} after the kill")]
    Stuck { command: String, waited: Duration },
    #[error("{command}: waiting for it failed: {source}")]
    Wait {
        command: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Clone, Copy)]
enum Stream {
    Out = 0,
    Err = 1,
}

enum Event {
    Chunk(Stream, Vec<u8>),
    End(Stream),
    Exited(std::io::Result<ExitStatus>),
}

/// Run `cmd` to its end within `limits`, `input` (if any) written to its
/// standard input. `command` names it in errors and in the record of
/// running children ([`crate::waits::report`]). Standard input, output and
/// error are set here: input piped or null, output and error piped.
pub fn run(
    mut cmd: Command,
    command: &str,
    input: Option<&[u8]>,
    limits: Limits,
) -> Result<Finished, RunError> {
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let spawn_err = |source| RunError::Spawn {
        command: command.to_owned(),
        source,
    };
    let tree = tree::Tree::prepare(&mut cmd).map_err(spawn_err)?;
    let mut child = cmd.spawn().map_err(spawn_err)?;
    let tree = tree.adopt(&mut child).map_err(spawn_err)?;
    let registration = crate::waits::child_running(child.id(), command);
    let (tx, rx) = mpsc::channel();
    if let Some(pipe) = child.stdout.take() {
        read_on_thread(pipe, Stream::Out, tx.clone());
    } else {
        let _ = tx.send(Event::End(Stream::Out));
    }
    if let Some(pipe) = child.stderr.take() {
        read_on_thread(pipe, Stream::Err, tx.clone());
    } else {
        let _ = tx.send(Event::End(Stream::Err));
    }
    if let (Some(bytes), Some(mut pipe)) = (input, child.stdin.take()) {
        // Owned, so a writer blocked on a pipe nobody reads can be left
        // behind. A write error is the child having exited early; its
        // status says why.
        let bytes = bytes.to_vec();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = pipe.write_all(&bytes);
        });
    }
    let waiter_tree = tree.clone();
    std::thread::spawn(move || {
        let status = waiter_tree.wait_and_end(child);
        // Removed from the record only once the child is reaped.
        drop(registration);
        let _ = tx.send(Event::Exited(status));
    });

    let mut out = [Vec::new(), Vec::new()];
    let mut ended = [false, false];
    let deadline = Instant::now() + limits.timeout;
    let status = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Event::Chunk(s, bytes)) => out[s as usize].extend_from_slice(&bytes),
            Ok(Event::End(s)) => ended[s as usize] = true,
            Ok(Event::Exited(status)) => {
                break status.map_err(|source| RunError::Wait {
                    command: command.to_owned(),
                    source,
                })?;
            }
            Err(RecvTimeoutError::Timeout) => {
                return Err(kill_after_timeout(&tree, &rx, command, limits));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(RunError::Wait {
                    command: command.to_owned(),
                    source: std::io::Error::other("the waiting thread ended without a status"),
                });
            }
        }
    };
    let grace_end = Instant::now() + limits.drain_grace;
    let mut pipes_held = false;
    while !(ended[0] && ended[1]) {
        let left = grace_end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Event::Chunk(s, bytes)) => out[s as usize].extend_from_slice(&bytes),
            Ok(Event::End(s)) => ended[s as usize] = true,
            Ok(Event::Exited(_)) => {}
            Err(RecvTimeoutError::Timeout) => {
                pipes_held = true;
                crate::waits::note_pipes_held(command);
                break;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let [stdout, stderr] = out;
    Ok(Finished {
        status,
        stdout,
        stderr,
        pipes_held,
    })
}

fn kill_after_timeout(
    tree: &tree::Tree,
    rx: &mpsc::Receiver<Event>,
    command: &str,
    limits: Limits,
) -> RunError {
    if let Err(source) = tree.kill() {
        return RunError::Kill {
            command: command.to_owned(),
            source,
        };
    }
    let end = Instant::now() + limits.kill_bound;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Event::Exited(_)) => {
                return RunError::TimedOut {
                    command: command.to_owned(),
                    after: limits.timeout,
                };
            }
            Ok(_) => {}
            Err(_) => {
                return RunError::Stuck {
                    command: command.to_owned(),
                    waited: limits.kill_bound,
                };
            }
        }
    }
}

fn read_on_thread(mut pipe: impl Read + Send + 'static, stream: Stream, tx: mpsc::Sender<Event>) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(Event::Chunk(stream, buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // A broken pipe reads as its end.
                Err(_) => break,
            }
        }
        let _ = tx.send(Event::End(stream));
    });
}

#[cfg(unix)]
mod tree {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, ExitStatus};
    use std::sync::{Arc, Mutex};

    /// The child's process group, whose id is the child's pid. `reaped`
    /// turns true, under its lock, when the leader is reaped: from then on
    /// the id may name another program's group and is never signalled.
    #[derive(Clone)]
    pub(super) struct Tree {
        pid: u32,
        reaped: Arc<Mutex<bool>>,
    }

    pub(super) struct Unadopted;

    impl Unadopted {
        pub(super) fn adopt(self, child: &mut Child) -> std::io::Result<Tree> {
            Ok(Tree {
                pid: child.id(),
                reaped: Arc::new(Mutex::new(false)),
            })
        }
    }

    impl Tree {
        pub(super) fn prepare(cmd: &mut Command) -> std::io::Result<Unadopted> {
            cmd.process_group(0);
            Ok(Unadopted)
        }

        /// Wait for the leader to exit, end the rest of its group, reap it.
        pub(super) fn wait_and_end(&self, mut child: Child) -> std::io::Result<ExitStatus> {
            wait_exited_unreaped(self.pid)?;
            let mut reaped = self.reaped.lock().unwrap_or_else(|p| p.into_inner());
            // SAFETY: the leader is an unreaped zombie, so its pid, the
            // group's id, is still reserved for this group.
            unsafe {
                libc::killpg(self.pid as libc::pid_t, libc::SIGKILL);
            }
            let status = child.wait();
            *reaped = true;
            status
        }

        /// Kill the group and its leader, unless the leader is reaped.
        pub(super) fn kill(&self) -> std::io::Result<()> {
            let reaped = self.reaped.lock().unwrap_or_else(|p| p.into_inner());
            if *reaped {
                return Ok(());
            }
            // SAFETY: the leader is not reaped (checked under the lock that
            // reaping takes), so the ids name this child and its group.
            let group = unsafe { libc::killpg(self.pid as libc::pid_t, libc::SIGKILL) };
            let group_err = std::io::Error::last_os_error();
            // The leader too, in case it left its group.
            // SAFETY: as above.
            let leader = unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) };
            let leader_err = std::io::Error::last_os_error();
            let gone = |e: &std::io::Error| e.raw_os_error() == Some(libc::ESRCH);
            if group != 0 && !gone(&group_err) {
                return Err(group_err);
            }
            if leader != 0 && !gone(&leader_err) {
                return Err(leader_err);
            }
            Ok(())
        }
    }

    /// Block until the child `pid` has exited, without reaping it.
    fn wait_exited_unreaped(pid: u32) -> std::io::Result<()> {
        loop {
            // SAFETY: zeroed siginfo_t is a valid out-parameter for waitid.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid writes only into `info`; P_PID names our own
            // child, which only the caller reaps.
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if rc == 0 {
                return Ok(());
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

#[cfg(windows)]
mod tree {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::process::{Child, Command, ExitStatus};
    use std::sync::Arc;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

    /// A job object that holds the child and everything it starts, and
    /// ends them when its last handle closes.
    #[derive(Clone)]
    pub(super) struct Tree {
        job: Arc<OwnedHandle>,
    }

    pub(super) struct Unadopted(Tree);

    impl Tree {
        pub(super) fn prepare(_cmd: &mut Command) -> std::io::Result<Unadopted> {
            // SAFETY: no name, default security; the handle is checked.
            let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if raw.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: a new handle this function owns.
            let job = unsafe { OwnedHandle::from_raw_handle(raw) };
            // SAFETY: zeroed is a valid value for this plain C struct.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: a valid job handle and a struct of the stated size
            // and class.
            let ok = unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    std::ptr::addr_of!(info).cast(),
                    std::mem::size_of_val(&info) as u32,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Unadopted(Tree { job: Arc::new(job) }))
        }

        pub(super) fn wait_and_end(&self, mut child: Child) -> std::io::Result<ExitStatus> {
            // SAFETY: the child's process handle, valid while `child` lives.
            if unsafe { WaitForSingleObject(child.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
                return Err(std::io::Error::last_os_error());
            }
            // What the child left running; an empty job makes this a no-op.
            let _ = self.kill();
            child.wait()
        }

        pub(super) fn kill(&self) -> std::io::Result<()> {
            // SAFETY: a valid job handle with terminate access.
            if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Unadopted {
        pub(super) fn adopt(self, child: &mut Child) -> std::io::Result<Tree> {
            // SAFETY: valid job and process handles.
            let ok = unsafe {
                AssignProcessToJobObject(self.0.job.as_raw_handle(), child.as_raw_handle())
            };
            if ok != 0 {
                return Ok(self.0);
            }
            let err = std::io::Error::last_os_error();
            // A child that already exited cannot be assigned, and needs no
            // job. One still running is ended rather than run unbounded.
            if let Ok(Some(_)) = child.try_wait() {
                return Ok(self.0);
            }
            let _ = child.kill();
            // SAFETY: the child's process handle; bounded.
            unsafe { WaitForSingleObject(child.as_raw_handle(), 10_000) };
            Err(std::io::Error::new(
                err.kind(),
                format!("cannot place the child in a job object: {err}"),
            ))
        }
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!("child_run supports Unix and Windows only");

#[cfg(test)]
mod tests;
