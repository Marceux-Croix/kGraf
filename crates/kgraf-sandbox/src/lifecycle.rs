//! Ending a sandboxed command's whole process tree before its leader is
//! reaped (Unix). Shared with the
//! contract-check runner.
//!
//! The rules, in order:
//!
//! 1. Wait for the leader to exit **without reaping it** (`waitid` with
//!    `WNOWAIT`). While it is an unreaped zombie its PID, which is its process
//!    group's id after the sandbox's `setsid`, cannot be reused, so a
//!    `killpg` on it cannot reach another program's group.
//! 2. Kill the group, again and again, until the hold file is free (every
//!    sandboxed process takes a shared lock on it and keeps it until it exits,
//!    `SandboxPolicy::holding`) or the grace runs out: a member can survive the
//!    first signal for a few milliseconds, and one forked after it needs its
//!    own.
//! 3. Only then reap the leader.
//!
//! A process that left the group with its own `setsid` is not reached by the
//! group kill; the hold file still shows it ([`hold_free`]), and the caller
//! refuses to treat the tree as quiet.

#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

/// How long the members of a killed process group get to finish dying.
pub const GROUP_EXIT_GRACE: Duration = Duration::from_secs(5);

/// Whether the child `pid` has exited, without reaping it (`WNOWAIT`).
pub fn exited_unreaped(pid: u32) -> std::io::Result<bool> {
    // SAFETY: zeroed siginfo_t is a valid out-parameter for waitid.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waitid writes only into `info`; P_PID names our own child.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: waitid filled `info` for an exited child.
    let got = unsafe { info.si_pid() };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let got = info.si_pid;
    Ok(got != 0)
}

/// Whether no process holds `hold` (every sandboxed process takes it shared and
/// keeps it until it exits). Probed with an exclusive try-lock released at once,
/// by an explicit unlock: a child another thread forks during the probe copies
/// its descriptor, and a close alone would leave that copy holding the lock
/// until the child execs, so the next probe would read this one as a holder
/// (`kgraf_core::flock`).
pub fn hold_free(hold: &Path) -> bool {
    match std::fs::File::open(hold) {
        Ok(f) => match f.try_lock() {
            Ok(()) => {
                drop(kgraf_core::flock::HeldLock::new(f));
                true
            }
            Err(_) => false,
        },
        // No hold file: nothing was started holding it.
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Kill the exited leader's process group until the hold is free or
/// [`GROUP_EXIT_GRACE`] runs out. Call only while the leader is an unreaped
/// zombie ([`exited_unreaped`]) or still running, so its PID, the group's id,
/// cannot have been reused by another process. Blocks the calling thread.
pub fn kill_group_until_hold_free(pid: u32, hold: &Path) {
    let deadline = std::time::Instant::now() + GROUP_EXIT_GRACE;
    loop {
        // SAFETY: killpg only sends a signal to the child's own group, whose
        // id the unreaped leader still reserves.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
        if hold_free(hold) || std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// `RLIMIT_CPU` seconds to give a command bounded by a `timeout` wall-clock
/// budget: twice the timeout, at least one second.
///
/// The rule: a well-behaved command is already stopped at `timeout` by the
/// group kill above (or its caller's equivalent); this rlimit exists only to
/// bound the one process that mechanism does not reach, a descendant that
/// calls its own `setsid` and so leaves the group before the group kill can
/// reach it (documented at the gate and check-runner call sites as an escaped
/// descendant). Doubling the wall budget leaves headroom for a legitimate
/// build that spends more CPU time than wall time across its own threads or
/// child processes, while still forcing a spinning escapee to stop rather
/// than run unbounded.
pub fn cpu_rlimit_seconds(timeout: Duration) -> u64 {
    timeout.as_secs().saturating_mul(2).max(1)
}

/// Sets `RLIMIT_CPU` to `cpu_seconds` on the process `cmd` will exec, so both
/// the gate runner and the contract-check runner apply the same bound the
/// same way. Must be called before `spawn` (it registers a `pre_exec` hook
/// that runs in the child, between `fork` and `exec`). Calling it more than
/// once registers more hooks; all of them run in order, so the last limit set
/// is the one that holds.
///
/// # Safety
/// The closure only calls `setrlimit`, which is async-signal-safe, and
/// allocates nothing, so it is safe to run in the forked child before exec
/// (the same contract `std::os::unix::process::CommandExt::pre_exec`
/// documents).
pub unsafe fn limit_cpu_seconds(cmd: &mut std::process::Command, cpu_seconds: u64) {
    use std::os::unix::process::CommandExt;
    // SAFETY: forwarded from the caller's contract above.
    unsafe {
        cmd.pre_exec(move || {
            let lim = libc::rlimit {
                rlim_cur: cpu_seconds as libc::rlim_t,
                rlim_max: cpu_seconds as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_CPU, &lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::hold_free;
    use kgraf_core::flock::Forkers;

    /// A hold nobody holds reads free on every probe while other threads
    /// fork. Before the probe released its lock explicitly, a child forked
    /// during one probe kept that probe's lock until it exec'd, and the next
    /// probe read it as a holder: 2,369 of 20,000 probes on macOS, and in a
    /// contract check the refusal "a process the check started left its
    /// process group" (`kgraf_core::flock`).
    #[test]
    fn a_free_hold_reads_free_while_other_threads_fork() {
        let dir = tempfile::tempdir().expect("dir");
        let hold = dir.path().join("hold");
        std::fs::File::create(&hold).expect("hold");
        let _forkers = Forkers::start(4);
        let held = (0..20_000).filter(|_| !hold_free(&hold)).count();
        assert_eq!(held, 0, "probes of a free hold read it as held");
    }

    /// The probe still sees a real holder: a shared lock kept by another
    /// descriptor, as a sandboxed process keeps it.
    #[test]
    fn a_held_hold_reads_held() {
        let dir = tempfile::tempdir().expect("dir");
        let hold = dir.path().join("hold");
        std::fs::File::create(&hold).expect("hold");
        let holder = std::fs::File::open(&hold).expect("open");
        holder.lock_shared().expect("shared lock");
        assert!(!hold_free(&hold));
        // Released explicitly: a plain close would leave the lock in any child
        // the forking test beside this one forks meanwhile (seen once in a
        // workspace run), the very leak `kgraf_core::flock` describes.
        drop(kgraf_core::flock::HeldLock::new(holder));
        assert!(hold_free(&hold));
    }
}
