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
//! Output is bounded in memory in one of three ways, chosen by what the caller
//! does with it (see `sink`):
//!
//! - held whole under [`Limits::max_output`], failing loud with
//!   [`RunError::OutputTooLarge`] ([`OutputMode::Hold`], the default);
//! - read record by record and not held ([`run_streaming`]);
//! - kept as a head and a tail with a marker for what was dropped, without
//!   ending the child ([`OutputMode::HeadTail`]).
//!
//! The spawn itself is made under the process-wide spawn lock
//! ([`spawn_lock`]): on Windows std's spawn copies every inheritable handle
//! in this process into the child, and kGraf's sandbox marks its children's
//! pipe ends inheritable only while it holds that lock. Taken on every
//! platform so the behavior is one; on Unix nothing else takes it, and an
//! uncontended take is one mutex round next to a process creation.
//!
//! Known limit (Windows): the child is placed in its job just after it
//! starts, so a process it starts in the first instant would escape the job.
//! Git starts nothing before it has read its configuration.

use sink::Records;
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

mod job;
mod sink;
mod spawn_lock;
pub use job::{Job, end_job};
pub use sink::{HeadTail, KEEP_HEAD, KEEP_TAIL, human_bytes, omitted_marker};
pub use spawn_lock::{
    SPAWN_LOCK_NAME, SPAWN_LOCK_WAIT, SpawnLock, SpawnLockStats, spawn_lock, spawn_lock_stats,
    spawn_locked,
};

#[cfg(test)]
mod spawn_sites;

/// `cmd.spawn()` under the spawn lock ([`spawn_locked`]).
pub fn spawn(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    spawn_locked(|| cmd.spawn())
}

/// What `cmd.output()` does, with the spawn under the spawn lock and the
/// wait outside it. Standard input is the null device and output and error
/// are piped, as `Command::output` sets them; a caller's own settings of
/// these three are replaced. Like `Command::output`, the wait is not
/// bounded; use [`run`] for a child that may not end.
pub fn output(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn(cmd)?.wait_with_output()
}

/// What `cmd.status()` does, with the spawn under the spawn lock and the
/// wait outside it. Standard streams are inherited unless the caller set
/// them, as with `Command::status`.
pub fn status(cmd: &mut Command) -> std::io::Result<ExitStatus> {
    spawn(cmd)?.wait()
}

/// How long a killed tree has to end before [`RunError::Stuck`]. A killed
/// process ends in milliseconds; 10 s covers a loaded machine flushing a
/// large process's memory.
pub const KILL_BOUND: Duration = Duration::from_secs(10);

/// How long the readers get, after the tree has ended, to reach the end of
/// the output. The bytes are already in the pipe, so this is only the time
/// for a reader thread to be scheduled: microseconds normally, and 5 s
/// covers a machine with every core busy.
pub const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// The most output [`run`] holds per stream (standard output, standard
/// error) in [`OutputMode::Hold`] before it ends the child and fails with
/// [`RunError::OutputTooLarge`]. This is a memory guard for a caller that
/// needs the whole output and has no better option, not a statement of what
/// output is reasonable: several children run at once, two streams each, all
/// held in RAM, and a machine has been brought down by one memory spike. 1 GiB
/// is above any output measured from a git command on a very large
/// repository (a listing runs to hundreds of megabytes), and a child that
/// writes more than that is not producing a result. A caller that reads a
/// long output record by record uses [`run_streaming`] and holds none of it;
/// a caller that shows the output uses [`OutputMode::HeadTail`] and is never
/// ended for being chatty.
pub const MAX_OUTPUT: usize = 1024 * 1024 * 1024;

/// What [`run`] does with each output stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Hold all of it, up to [`Limits::max_output`]; more ends the child with
    /// [`RunError::OutputTooLarge`].
    Hold,
    /// Keep the first `head` and the last `tail` bytes. What lies between is
    /// dropped and a marker line says how many bytes ([`omitted_marker`]).
    /// The child runs on and is not ended for the size of its output.
    HeadTail { head: usize, tail: usize },
}

impl OutputMode {
    /// [`KEEP_HEAD`] and [`KEEP_TAIL`].
    pub const STANDARD_HEAD_TAIL: Self = Self::HeadTail {
        head: KEEP_HEAD,
        tail: KEEP_TAIL,
    };
}

/// The bounds of one [`run`].
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Killed after this.
    pub timeout: Duration,
    /// See [`KILL_BOUND`].
    pub kill_bound: Duration,
    /// See [`DRAIN_GRACE`].
    pub drain_grace: Duration,
    /// See [`SPAWN_LOCK_WAIT`].
    pub spawn_lock_wait: Duration,
    /// See [`MAX_OUTPUT`].
    pub max_output: usize,
    /// See [`OutputMode`].
    pub output: OutputMode,
}

impl Limits {
    /// `timeout` with the default kill bound and drain grace.
    pub const fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            kill_bound: KILL_BOUND,
            drain_grace: DRAIN_GRACE,
            spawn_lock_wait: SPAWN_LOCK_WAIT,
            max_output: MAX_OUTPUT,
            output: OutputMode::Hold,
        }
    }

    /// The same limits, with output kept as a head and a tail
    /// ([`OutputMode::STANDARD_HEAD_TAIL`]).
    pub const fn keeping_head_and_tail(mut self) -> Self {
        self.output = OutputMode::STANDARD_HEAD_TAIL;
        self
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
    /// Bytes of standard output and of standard error read and dropped
    /// ([`OutputMode::HeadTail`]); each stream's marker line is in its bytes.
    pub omitted: [u64; 2],
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
    #[error(
        "{command} wrote more than {cap} bytes to its {stream}; its process tree was killed \
         and the output cut"
    )]
    OutputTooLarge {
        command: String,
        stream: &'static str,
        cap: usize,
    },
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
    cmd: Command,
    command: &str,
    input: Option<&[u8]>,
    limits: Limits,
) -> Result<Finished, RunError> {
    let sink = || match limits.output {
        OutputMode::Hold => Sink::Hold(Vec::new(), limits.max_output),
        OutputMode::HeadTail { head, tail } => Sink::Kept(HeadTail::new(head, tail)),
    };
    run_inner(cmd, command, input, limits, [sink(), sink()], &mut |_| {})
}

/// [`run`] for a long output read as it comes: standard output is cut into
/// records ended by `delimiter` (`b'\n'`, or `0` for `git -z`) and each
/// record, without its delimiter, goes to `on_record` on the calling thread;
/// a last record with no delimiter after it goes at the end. Nothing of
/// standard output is held, so [`Limits::max_output`] applies only to one
/// record. Standard error is kept as a head and a tail
/// ([`OutputMode::STANDARD_HEAD_TAIL`]) whatever `limits.output` says.
///
/// Everything else is [`run`]'s: the spawn lock, the deadline, the tree
/// kill, the drain. `on_record` runs between reads, so a slow one slows the
/// child through the pipe, and the deadline is checked between records. For
/// a child that did not succeed, records may have been given already; the
/// caller decides what a partial read is worth.
pub fn run_streaming(
    cmd: Command,
    command: &str,
    limits: Limits,
    delimiter: u8,
    mut on_record: impl FnMut(&[u8]),
) -> Result<Finished, RunError> {
    let sinks = [
        Sink::Records(Records::new(delimiter, limits.max_output)),
        Sink::Kept(HeadTail::standard()),
    ];
    run_inner(cmd, command, None, limits, sinks, &mut on_record)
}

fn run_inner(
    mut cmd: Command,
    command: &str,
    input: Option<&[u8]>,
    limits: Limits,
    mut sinks: [Sink; 2],
    on_record: &mut dyn FnMut(&[u8]),
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
    let spawning = spawn_lock(limits.spawn_lock_wait).map_err(|timeout| RunError::Spawn {
        command: command.to_owned(),
        source: std::io::Error::new(std::io::ErrorKind::TimedOut, timeout),
    })?;
    let spawned = cmd.spawn();
    drop(spawning);
    let mut child = spawned.map_err(spawn_err)?;
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
    // The waiter must not keep the process tree alive. On Windows the tree
    // is a job object that ends its processes when its last handle closes;
    // the waiter waits for as long as the child lives, so a full handle
    // there would keep the job open after `run` gave up on a kill that
    // failed. It gets a handle that does not count (`tree::Waiter`).
    let waiter_tree = tree.waiter();
    std::thread::spawn(move || {
        let status = waiter_tree.wait_and_end(child, limits.kill_bound);
        // Removed from the record only once the child is reaped.
        drop(registration);
        let _ = tx.send(Event::Exited(status));
    });

    let mut ended = [false, false];
    let deadline = crate::waits::deadline_after(limits.timeout);
    let status = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Event::Chunk(s, bytes)) => {
                if sinks[s as usize].push(&bytes, on_record) {
                    return Err(output_too_large(&tree, &rx, command, limits, s));
                }
            }
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
    let grace_end = crate::waits::deadline_after(limits.drain_grace);
    let mut pipes_held = false;
    while !(ended[0] && ended[1]) {
        let left = grace_end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Event::Chunk(s, bytes)) => {
                if sinks[s as usize].push(&bytes, on_record) {
                    return Err(output_too_large(&tree, &rx, command, limits, s));
                }
            }
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
    let [out_sink, err_sink] = sinks;
    let (stdout, out_omitted) = out_sink.finish(on_record);
    let (stderr, err_omitted) = err_sink.finish(on_record);
    Ok(Finished {
        status,
        stdout,
        stderr,
        pipes_held,
        omitted: [out_omitted, err_omitted],
    })
}

/// One output stream's destination.
enum Sink {
    /// All of it, up to the cap.
    Hold(Vec<u8>, usize),
    /// A head and a tail.
    Kept(HeadTail),
    /// Handed on, record by record.
    Records(Records),
}

impl Sink {
    /// Take `bytes`; true if that passes the sink's cap.
    fn push(&mut self, bytes: &[u8], on_record: &mut dyn FnMut(&[u8])) -> bool {
        match self {
            Sink::Hold(held, cap) => {
                if held.len().saturating_add(bytes.len()) > *cap {
                    return true;
                }
                held.extend_from_slice(bytes);
                false
            }
            Sink::Kept(kept) => {
                kept.push(bytes);
                false
            }
            Sink::Records(records) => records.push(bytes, on_record),
        }
    }

    /// What was kept, and how many bytes were dropped.
    fn finish(self, on_record: &mut dyn FnMut(&[u8])) -> (Vec<u8>, u64) {
        match self {
            Sink::Hold(held, _) => (held, 0),
            Sink::Kept(kept) => kept.finish(),
            Sink::Records(mut records) => {
                records.finish(on_record);
                (Vec::new(), 0)
            }
        }
    }
}

/// The child wrote past the cap: end its tree (as at the timeout), and the
/// error says the output was cut, unless the kill itself failed.
fn output_too_large(
    tree: &impl Kill,
    rx: &mpsc::Receiver<Event>,
    command: &str,
    limits: Limits,
    stream: Stream,
) -> RunError {
    match kill_after_timeout(tree, rx, command, limits) {
        RunError::TimedOut { .. } => RunError::OutputTooLarge {
            command: command.to_owned(),
            stream: match stream {
                Stream::Out => "standard output",
                Stream::Err => "standard error",
            },
            cap: limits.max_output,
        },
        other => other,
    }
}

/// How often the kill is sent again while the tree has not ended. A process
/// created in the instant of the first kill (a fork or a spawn that was
/// already under way) is not in the first kill's reach; the next one has it.
const KILL_RETRY: Duration = Duration::from_millis(100);

/// What [`kill_after_timeout`] needs of a process tree.
trait Kill {
    fn kill(&self) -> std::io::Result<()>;
}

impl Kill for tree::Tree {
    fn kill(&self) -> std::io::Result<()> {
        tree::Tree::kill(self)
    }
}

/// Kill the tree again and again until the waiter reports the child gone,
/// at most `limits.kill_bound`. A kill that fails is tried again; it is the
/// error ([`RunError::Kill`]) only if the last try failed at the bound. A
/// tree that has not ended at the bound is [`RunError::Stuck`]. On both
/// errors `run` returns and drops the tree, which on Windows closes the job
/// and so ends what is left (see the waiter in [`run`]).
fn kill_after_timeout(
    tree: &impl Kill,
    rx: &mpsc::Receiver<Event>,
    command: &str,
    limits: Limits,
) -> RunError {
    let end = crate::waits::deadline_after(limits.kill_bound);
    let mut last_kill_error;
    loop {
        last_kill_error = tree.kill().err();
        let left = end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left.min(KILL_RETRY)) {
            Ok(Event::Exited(_)) => {
                return RunError::TimedOut {
                    command: command.to_owned(),
                    after: limits.timeout,
                };
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if Instant::now() >= end {
            break;
        }
    }
    match last_kill_error {
        Some(source) => RunError::Kill {
            command: command.to_owned(),
            source,
        },
        None => RunError::Stuck {
            command: command.to_owned(),
            waited: limits.kill_bound,
        },
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

        /// What the waiting thread holds. A group id keeps nothing alive,
        /// so it is the tree itself.
        pub(super) fn waiter(&self) -> Tree {
            self.clone()
        }

        /// Wait for the leader to exit, end the rest of its group, reap it.
        pub(super) fn wait_and_end(
            &self,
            mut child: Child,
            _kill_bound: std::time::Duration,
        ) -> std::io::Result<ExitStatus> {
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
    use std::sync::{Arc, Weak};
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

    /// A job object that holds the child and everything it starts, and
    /// ends them when its last handle closes. Only [`super::run`]'s own
    /// copy counts toward that: the waiting thread holds a [`Waiter`].
    #[derive(Clone)]
    pub(super) struct Tree {
        job: Arc<OwnedHandle>,
    }

    /// The waiting thread's view of the job: it can end the job's processes
    /// while `run` still holds the job, and never keeps the job open.
    pub(super) struct Waiter {
        job: Weak<OwnedHandle>,
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

        pub(super) fn waiter(&self) -> Waiter {
            Waiter {
                job: Arc::downgrade(&self.job),
            }
        }

        pub(super) fn kill(&self) -> std::io::Result<()> {
            terminate(&self.job)
        }
    }

    /// A job handle, as the steps of [`super::end_job`] see it.
    struct JobRef<'a>(&'a OwnedHandle);

    impl super::Job for JobRef<'_> {
        fn terminate(&mut self) -> std::io::Result<()> {
            terminate(self.0)
        }

        fn active(&self) -> std::io::Result<u32> {
            // SAFETY: zeroed is a valid value for this plain C struct.
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid job handle with query access, and a struct of
            // the stated size and class.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    std::ptr::addr_of_mut!(info).cast(),
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(info.ActiveProcesses)
        }
    }

    fn terminate(job: &OwnedHandle) -> std::io::Result<()> {
        // SAFETY: a valid job handle with terminate access.
        if unsafe { TerminateJobObject(job.as_raw_handle(), 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    impl Waiter {
        /// Wait for the child to exit, end what it left running, and wait up
        /// to `kill_bound` until the job holds nothing: `TerminateJobObject`
        /// returns before the processes have ended, and the caller's wait
        /// for the output pipes (which close with them) must not start
        /// before, or a slow teardown reads as a pipe held outside the tree.
        pub(super) fn wait_and_end(
            &self,
            mut child: Child,
            kill_bound: std::time::Duration,
        ) -> std::io::Result<ExitStatus> {
            // SAFETY: the child's process handle, valid while `child` lives.
            if unsafe { WaitForSingleObject(child.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
                return Err(std::io::Error::last_os_error());
            }
            // What the child left running; an empty job makes this a no-op.
            // `run` may have let the job go already, which ended them.
            if let Some(job) = self.job.upgrade() {
                let _ = super::end_job(&mut JobRef(&job), kill_bound);
            }
            child.wait()
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

#[cfg(test)]
mod output_tests;
