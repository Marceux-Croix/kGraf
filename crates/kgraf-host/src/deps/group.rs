//! Process groups of fetch commands that die with kGraf.
//!
//! A fetch command leads its own process group, so a timeout or a cancel can
//! kill the command and everything it started. A terminal's Ctrl-C reaches
//! only the foreground group, so without this module an interrupted kGraf
//! would leave the command downloading in the background for as long as its
//! timeout.
//!
//! Two mechanisms, both Unix:
//!
//! - Linux: the child asks the kernel for `SIGKILL` when its parent dies
//!   (`PR_SET_PDEATHSIG`). This covers `SIGKILL` and a crash of kGraf, but
//!   kills the command only, not its own children.
//! - Every Unix: a process-wide registry of the groups in flight, killed from
//!   a `SIGINT` and `SIGTERM` handler. The handler only calls `killpg`,
//!   `signal` and `raise`, all async-signal-safe, and reads a fixed array of
//!   atomics, never a lock. It then hands the signal on: to the handler that
//!   was installed before it, or, when there was none, to the default action
//!   (the process ends as it would have). A signal that was ignored stays
//!   ignored and no handler is installed for it.
//!
//! Limits: a handler that a library installs after this one replaces it (the
//! registry is then killed only by the cancel path), and a `SIGKILL` of kGraf
//! on macOS cannot be caught.

use std::io;
use std::os::raw::{c_int, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

/// Groups that can be in flight at once. A fetch runs one command at a time
/// per call and few calls overlap.
const SLOTS: usize = 64;

/// Process group ids in fixed slots; `0` is a free slot. Atomics only, so a
/// signal handler can read it.
struct Registry<const N: usize> {
    cells: [AtomicI32; N],
}

impl<const N: usize> Registry<N> {
    const fn new() -> Self {
        Self {
            cells: [const { AtomicI32::new(0) }; N],
        }
    }

    fn claim(&self, pgid: i32) -> Option<usize> {
        self.cells.iter().position(|cell| {
            cell.compare_exchange(0, pgid, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        })
    }

    fn release(&self, slot: usize) {
        self.cells[slot].store(0, Ordering::SeqCst);
    }

    /// `SIGKILL` every registered group. Async-signal-safe.
    fn kill_all(&self) {
        for cell in &self.cells {
            let pgid = cell.load(Ordering::SeqCst);
            if pgid > 0 {
                // SAFETY: killpg is async-signal-safe and only signals a
                // group a command of ours leads (the registration is removed
                // before the leader is reaped).
                unsafe {
                    libc::killpg(pgid, libc::SIGKILL);
                }
            }
        }
    }
}

/// The process groups of the commands in flight.
static GROUPS: Registry<SLOTS> = Registry::new();

/// The handler a signal had before ours: its address, and whether it takes
/// the three-argument form.
struct Previous {
    handler: AtomicUsize,
    siginfo: AtomicBool,
}

static PREVIOUS: [Previous; 2] = [const {
    Previous {
        handler: AtomicUsize::new(0),
        siginfo: AtomicBool::new(false),
    }
}; 2];

const SIGNALS: [c_int; 2] = [libc::SIGINT, libc::SIGTERM];

/// A group registered for the signal handler to kill. Dropping it removes the
/// registration; call [`GroupGuard::release`] before reaping the leader, so a
/// pid that is reused cannot be signalled.
pub(super) struct GroupGuard {
    slot: usize,
}

impl GroupGuard {
    /// Register the group led by `pid`, and make sure the handlers are in
    /// place. Fails loud when the registry is full or a handler cannot be
    /// installed: a command that could outlive an interrupt must not start
    /// unnoticed.
    pub(super) fn register(pid: u32) -> io::Result<Self> {
        install_handlers()?;
        GROUPS
            .claim(pid as i32)
            .map(|slot| Self { slot })
            .ok_or_else(|| {
                io::Error::other(format!(
                    "more than {SLOTS} commands in flight: cannot track another"
                ))
            })
    }

    pub(super) fn release(&self) {
        GROUPS.release(self.slot);
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// `SIGKILL` every registered group. Async-signal-safe.
pub(super) fn kill_registered() {
    GROUPS.kill_all();
}

extern "C" fn on_signal(sig: c_int, info: *mut libc::siginfo_t, ctx: *mut c_void) {
    kill_registered();
    let Some(at) = SIGNALS.iter().position(|s| *s == sig) else {
        return;
    };
    let previous = PREVIOUS[at].handler.load(Ordering::SeqCst);
    if previous == libc::SIG_DFL {
        // SAFETY: signal and raise are async-signal-safe. `sig` is blocked
        // while this handler runs, so the default action happens when it
        // returns.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    } else if previous != libc::SIG_IGN {
        // SAFETY: `previous` is the address the kernel returned for this
        // signal's earlier handler, of the form its flags said.
        unsafe {
            if PREVIOUS[at].siginfo.load(Ordering::SeqCst) {
                let f: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) =
                    std::mem::transmute(previous);
                f(sig, info, ctx);
            } else {
                let f: extern "C" fn(c_int) = std::mem::transmute(previous);
                f(sig);
            }
        }
    }
}

/// Install the `SIGINT` and `SIGTERM` handlers once per process.
fn install_handlers() -> io::Result<()> {
    static INSTALLED: OnceLock<Result<(), i32>> = OnceLock::new();
    let result = INSTALLED.get_or_init(|| {
        for (at, sig) in SIGNALS.iter().enumerate() {
            // SAFETY: zeroed sigaction structs are valid out and in
            // parameters; `on_signal` has the SA_SIGINFO signature.
            unsafe {
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(*sig, std::ptr::null(), &mut old) != 0 {
                    return Err(io::Error::last_os_error().raw_os_error().unwrap_or(0));
                }
                // Ignored (a background job, `nohup`): the process was never
                // going to be interrupted, so nothing to do for it.
                if old.sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                PREVIOUS[at]
                    .handler
                    .store(old.sa_sigaction, Ordering::SeqCst);
                PREVIOUS[at]
                    .siginfo
                    .store(old.sa_flags & libc::SA_SIGINFO != 0, Ordering::SeqCst);
                let mut new: libc::sigaction = std::mem::zeroed();
                new.sa_sigaction = on_signal as extern "C" fn(_, _, _) as usize;
                new.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
                libc::sigemptyset(&mut new.sa_mask);
                if libc::sigaction(*sig, &new, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error().raw_os_error().unwrap_or(0));
                }
            }
        }
        Ok(())
    });
    match result {
        Ok(()) => Ok(()),
        Err(errno) => Err(io::Error::other(format!(
            "cannot install the interrupt handler that stops fetch commands: {}",
            io::Error::from_raw_os_error(*errno)
        ))),
    }
}

/// Have the kernel `SIGKILL` the command when kGraf dies (Linux). The
/// signal fires when the thread that started the command exits, and the
/// caller keeps that thread for the command's whole run.
#[cfg(target_os = "linux")]
pub(super) fn die_with_parent(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: getpid is async-signal-safe and called before the fork.
    let parent = unsafe { libc::getpid() };
    // SAFETY: the closure calls only prctl and getppid, both
    // async-signal-safe, and allocates nothing.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                return Err(io::Error::last_os_error());
            }
            // The parent may have died before the request took effect.
            if libc::getppid() != parent {
                return Err(io::Error::other("the parent process ended"));
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn alive(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only probes whether the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn wait_gone(pid: libc::pid_t) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        !alive(pid)
    }

    /// A `sleep` in its own group, reaped by a thread so it does not linger
    /// as a zombie the probe would still see.
    fn sleeper() -> (libc::pid_t, std::thread::JoinHandle<()>) {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new("sleep");
        cmd.arg("30").process_group(0).stdin(Stdio::null());
        let mut child = cmd.spawn().expect("spawn sleep");
        let pid = child.id() as libc::pid_t;
        (pid, std::thread::spawn(move || drop(child.wait())))
    }

    /// A private registry: the process-wide one holds the commands other
    /// tests have in flight, and killing those would break them.
    #[test]
    fn a_registered_group_is_killed_and_a_released_one_is_not() {
        let registry = Registry::<4>::new();
        let (kept, kept_reaper) = sleeper();
        let (killed, killed_reaper) = sleeper();
        registry.claim(killed).expect("slot");
        let released = registry.claim(kept).expect("slot");
        registry.release(released);
        registry.kill_all();
        assert!(wait_gone(killed), "the registered group is still running");
        assert!(alive(kept), "a released group was killed");
        // SAFETY: kills the sleeper this test started.
        unsafe { libc::kill(kept, libc::SIGKILL) };
        kept_reaper.join().expect("reaper");
        killed_reaper.join().expect("reaper");
    }

    #[test]
    fn a_full_registry_has_no_slot_and_a_released_one_is_reused() {
        let registry = Registry::<2>::new();
        let (a, b) = (
            registry.claim(11).expect("a"),
            registry.claim(12).expect("b"),
        );
        assert_ne!(a, b);
        assert!(registry.claim(13).is_none(), "no free slot");
        registry.release(a);
        assert_eq!(registry.claim(13), Some(a));
    }
}
