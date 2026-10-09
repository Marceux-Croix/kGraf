//! Waiting for a command the sandbox started, and collecting its output,
//! with every step bounded. Used by the Windows `SandboxedChild`, whose
//! process is not a std `Child`, so `kgraf_core::child_run` cannot run it;
//! the steps and bounds are that runner's (its constants are reused here).
//!
//! 1. Output is read on two threads that pass chunks over a channel, so a
//!    reader that never ends cannot hold the caller.
//! 2. The command's exit is waited for, at most the caller's bound. At the
//!    bound its whole tree is ended and the wait fails with
//!    [`std::io::ErrorKind::TimedOut`], naming the command and the bound.
//! 3. After a normal exit, what the command left running is ended too, and
//!    the wait for that is bounded ([`kgraf_core::child_run::KILL_BOUND`]): a
//!    process's pipe ends close only once it has ended.
//! 4. The readers then get [`kgraf_core::child_run::DRAIN_GRACE`] to reach
//!    the end of the pipes. A pipe still open after that is held by a process
//!    outside the tree; what the command wrote is already read, so the output
//!    is returned and the case recorded (`kgraf_core::waits::note_pipes_held`).
//!
//! The logic sits behind [`Exits`] so it is tested on every host; only the
//! Windows backend uses it.

use std::io::{self, Read};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// What waiting needs of a running command.
pub(crate) trait Exits {
    type Status;
    /// The exit status once the command has exited, `None` if it still runs
    /// after `bound`.
    fn wait_timeout(&mut self, bound: Duration) -> io::Result<Option<Self::Status>>;
    /// End the command and everything it started, then wait until none is
    /// left, at most `bound`. True when none is left.
    fn end_tree(&mut self, bound: Duration) -> io::Result<bool>;
}

/// The bounds of one wait.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bounds {
    /// For the command to exit.
    pub exit: Duration,
    /// For its tree to end once ended.
    pub kill: Duration,
    /// For the readers to reach the end of the pipes once the tree ended.
    pub drain: Duration,
}

impl Bounds {
    pub(crate) const fn new(exit: Duration) -> Self {
        Self {
            exit,
            kill: kgraf_core::child_run::KILL_BOUND,
            drain: kgraf_core::child_run::DRAIN_GRACE,
        }
    }
}

/// A command that ran to its end, with what it wrote.
#[derive(Debug)]
pub(crate) struct Collected<S> {
    pub status: S,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// An output pipe stayed open after the tree ended. `Output` has no room
    /// for it, so outside tests the case is read from the record
    /// (`kgraf_core::waits::pipes_held_count`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub pipes_held: bool,
}

fn named(what: &str, why: String) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, format!("{what}: {why}"))
}

/// Wait for `child` to exit, at most `bounds.exit`. At the bound its tree is
/// ended (at most `bounds.kill`) and the result is a `TimedOut` error naming
/// `what` and the bound, never a silent hang.
pub(crate) fn wait_exit<C: Exits>(
    child: &mut C,
    what: &str,
    bounds: Bounds,
) -> io::Result<C::Status> {
    if let Some(status) = child.wait_timeout(bounds.exit)? {
        return Ok(status);
    }
    let ended = match child.end_tree(bounds.kill) {
        Ok(true) => "its process tree was ended".to_owned(),
        Ok(false) => format!(
            "its process tree had not ended {:?} after it was ended",
            bounds.kill
        ),
        Err(e) => format!("ending its process tree failed: {e}"),
    };
    Err(named(
        what,
        format!("did not exit within {:?}; {ended}", bounds.exit),
    ))
}

enum Event {
    Chunk(usize, Vec<u8>),
    End(usize),
    Failed(usize, io::Error),
}

fn read_on_thread(mut pipe: Box<dyn Read + Send>, stream: usize, tx: mpsc::Sender<Event>) {
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
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let _ = tx.send(Event::Failed(stream, e));
                    return;
                }
            }
        }
        let _ = tx.send(Event::End(stream));
    });
}

/// Read `stdout` and `stderr` to their end while `child` runs, wait for it
/// ([`wait_exit`]), end what it left running, and return what it wrote.
/// Every wait is bounded (see the module documentation). A reader that
/// cannot finish is left behind: it holds nothing but its pipe.
pub(crate) fn collect<C: Exits>(
    child: &mut C,
    stdout: Option<Box<dyn Read + Send>>,
    stderr: Option<Box<dyn Read + Send>>,
    what: &str,
    bounds: Bounds,
) -> io::Result<Collected<C::Status>> {
    let (tx, rx) = mpsc::channel();
    let mut ended = [false, false];
    for (i, pipe) in [stdout, stderr].into_iter().enumerate() {
        match pipe {
            Some(pipe) => read_on_thread(pipe, i, tx.clone()),
            None => ended[i] = true,
        }
    }
    drop(tx);
    let status = wait_exit(child, what, bounds)?;
    // A process the command left behind would hold the pipes open; the
    // readers' wait starts once none is left.
    if !child.end_tree(bounds.kill)? {
        return Err(named(
            what,
            format!(
                "a process it started was still running {:?} after its tree was ended",
                bounds.kill
            ),
        ));
    }
    let mut out = [Vec::new(), Vec::new()];
    let grace_end = Instant::now() + bounds.drain;
    let mut pipes_held = false;
    while !(ended[0] && ended[1]) {
        let left = grace_end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Event::Chunk(i, bytes)) => out[i].extend_from_slice(&bytes),
            Ok(Event::End(i)) => ended[i] = true,
            Ok(Event::Failed(i, e)) => {
                let stream = if i == 0 { "output" } else { "error output" };
                return Err(io::Error::new(
                    e.kind(),
                    format!("{what}: reading its {stream} failed: {e}"),
                ));
            }
            Err(RecvTimeoutError::Timeout) => {
                pipes_held = true;
                kgraf_core::waits::note_pipes_held(what);
                break;
            }
            // Every reader has sent its last event.
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let [stdout, stderr] = out;
    Ok(Collected {
        status,
        stdout,
        stderr,
        pipes_held,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A command whose exit and tree are scripted: it exits after `runs`
    /// (never when `None`), and its tree ends on `end_tree` unless `stuck`.
    struct Fake {
        started: Instant,
        runs: Option<Duration>,
        stuck: bool,
        ended: Arc<Mutex<u32>>,
    }

    impl Fake {
        fn new(runs: Option<Duration>) -> Self {
            Self {
                started: Instant::now(),
                runs,
                stuck: false,
                ended: Arc::new(Mutex::new(0)),
            }
        }
    }

    impl Exits for Fake {
        type Status = i32;
        fn wait_timeout(&mut self, bound: Duration) -> io::Result<Option<i32>> {
            let exit_at = self.runs.map(|r| self.started + r);
            let give_up = Instant::now() + bound;
            match exit_at {
                Some(at) if at <= give_up => {
                    std::thread::sleep(at.saturating_duration_since(Instant::now()));
                    Ok(Some(7))
                }
                _ => {
                    std::thread::sleep(bound);
                    Ok(None)
                }
            }
        }
        fn end_tree(&mut self, _bound: Duration) -> io::Result<bool> {
            *self.ended.lock().unwrap() += 1;
            Ok(!self.stuck)
        }
    }

    /// A pipe nobody closes: a read blocks until the sender is dropped,
    /// which these tests never do while the read runs.
    struct HeldPipe(mpsc::Receiver<()>);

    impl Read for HeldPipe {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }

    fn held_pipe() -> (mpsc::Sender<()>, Box<dyn Read + Send>) {
        let (tx, rx) = mpsc::channel();
        (tx, Box::new(HeldPipe(rx)))
    }

    fn quick() -> Bounds {
        Bounds {
            exit: Duration::from_secs(5),
            kill: Duration::from_secs(1),
            drain: Duration::from_millis(200),
        }
    }

    /// Runs `f` on a thread so a regression fails instead of hanging.
    fn within<T: Send + 'static>(bound: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(bound).expect("returned within the bound")
    }

    #[test]
    fn a_command_still_running_at_the_bound_is_a_named_timeout_and_its_tree_is_ended() {
        let mut child = Fake::new(None);
        let ended = child.ended.clone();
        let bounds = Bounds {
            exit: Duration::from_millis(100),
            ..quick()
        };
        let err = within(Duration::from_secs(10), move || {
            wait_exit(&mut child, "probe command", bounds)
        })
        .expect_err("never exits");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let text = err.to_string();
        assert!(text.contains("probe command"), "{text}");
        assert!(text.contains("100ms"), "{text}");
        assert!(text.contains("tree was ended"), "{text}");
        assert_eq!(*ended.lock().unwrap(), 1);
    }

    #[test]
    fn a_command_that_exits_in_time_gives_its_status() {
        let mut child = Fake::new(Some(Duration::from_millis(20)));
        assert_eq!(wait_exit(&mut child, "probe", quick()).unwrap(), 7);
    }

    #[test]
    fn default_bounds_are_the_child_runners() {
        let b = Bounds::new(Duration::from_secs(3));
        assert_eq!(b.exit, Duration::from_secs(3));
        assert_eq!(b.kill, kgraf_core::child_run::KILL_BOUND);
        assert_eq!(b.drain, kgraf_core::child_run::DRAIN_GRACE);
    }

    #[test]
    fn output_is_collected_when_the_pipes_end() {
        let mut child = Fake::new(Some(Duration::from_millis(20)));
        let got = within(Duration::from_secs(10), move || {
            collect(
                &mut child,
                Some(Box::new(io::Cursor::new(b"out".to_vec()))),
                Some(Box::new(io::Cursor::new(b"err".to_vec()))),
                "probe",
                quick(),
            )
        })
        .unwrap();
        assert_eq!(got.status, 7);
        assert_eq!(got.stdout, b"out");
        assert_eq!(got.stderr, b"err");
        assert!(!got.pipes_held);
    }

    #[test]
    fn a_pipe_held_outside_the_tree_does_not_hold_the_wait() {
        let mut child = Fake::new(Some(Duration::from_millis(20)));
        let ended = child.ended.clone();
        let (keep, pipe) = held_pipe();
        let before = kgraf_core::waits::pipes_held_count();
        let started = Instant::now();
        let got = within(Duration::from_secs(10), move || {
            collect(
                &mut child,
                Some(Box::new(io::Cursor::new(b"written".to_vec()))),
                Some(pipe),
                "held pipe probe",
                quick(),
            )
        })
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(got.stdout, b"written");
        assert!(got.pipes_held);
        assert!(kgraf_core::waits::pipes_held_count() > before);
        assert!(kgraf_core::waits::report().contains("held pipe probe"));
        // The tree is ended after a normal exit, before the drain.
        assert_eq!(*ended.lock().unwrap(), 1);
        drop(keep);
    }

    #[test]
    fn a_tree_that_does_not_end_after_exit_is_a_named_error() {
        let mut child = Fake::new(Some(Duration::from_millis(10)));
        child.stuck = true;
        let (keep, pipe) = held_pipe();
        let err = within(Duration::from_secs(10), move || {
            collect(&mut child, Some(pipe), None, "stuck tree probe", quick())
        })
        .expect_err("tree stuck");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("stuck tree probe"), "{err}");
        assert!(err.to_string().contains("still running"), "{err}");
        drop(keep);
    }

    #[test]
    fn a_read_error_is_returned_not_dropped() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("device gone"))
            }
        }
        let mut child = Fake::new(Some(Duration::from_millis(10)));
        let err = within(Duration::from_secs(10), move || {
            collect(&mut child, Some(Box::new(Broken)), None, "probe", quick())
        })
        .expect_err("read failed");
        assert!(err.to_string().contains("device gone"), "{err}");
    }
}
