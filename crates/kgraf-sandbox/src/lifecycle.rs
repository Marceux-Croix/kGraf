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

/// macOS: the processes that have `file` open, read from the kernel in this
/// process (libproc), never this process or a child of it. Matched by the
/// file's device and inode, not its path.
///
/// A child of this process is left out because the only way one holds the
/// file is a descriptor copied by a fork in another thread while this
/// process had the file open for a moment ([`hold_free`]); the copy is gone
/// at the child's exec. A sandboxed command that holds it is never a child
/// here once its leader is reaped: what it starts is a child of its own
/// processes, or of launchd after its parent exits.
///
/// In this process and not through `lsof`: one scan of about 500 processes
/// took 0.1 s, and 0.23 s with four CPU-bound processes per core, where one
/// `lsof` run took 0.4 s and 0.5 to 1.1 s. A caller that kills what this
/// names (a process that left its group) must find it before it does its
/// work.
///
/// An error when the process list cannot be read. A process that ends
/// during the scan is skipped.
#[cfg(target_os = "macos")]
pub fn holders(file: &Path) -> std::io::Result<Vec<libc::pid_t>> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(file)?;
    // `vinfo_stat` keeps the device as 32 bits, as `stat` does on macOS.
    let (dev, ino) = (meta.dev() as u32, meta.ino());
    let me = std::process::id() as libc::pid_t;
    let mut found = Vec::new();
    for pid in all_pids()? {
        if pid <= 1 || pid == me || !has_open(pid, dev, ino) {
            continue;
        }
        if parent_of(pid) == Some(me) {
            continue;
        }
        found.push(pid);
    }
    Ok(found)
}

/// Every process ID on the system.
#[cfg(target_os = "macos")]
fn all_pids() -> std::io::Result<Vec<libc::pid_t>> {
    // SAFETY: a null buffer asks for the count only.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Room for processes started meanwhile.
    let mut pids = vec![0 as libc::pid_t; count as usize + 256];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: `pids` has `bytes` bytes; the call writes at most that many and
    // returns the number of PIDs written.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if n <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    pids.truncate(n as usize);
    Ok(pids)
}

/// The parent of `pid`, or `None` when it cannot be read (it ended).
#[cfg(target_os = "macos")]
fn parent_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: zeroed proc_bsdinfo is a valid out-parameter.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (got == size).then_some(info.pbi_ppid as libc::pid_t)
}

/// `struct proc_fileinfo` of `<sys/proc_info.h>` (not in the libc crate).
#[cfg(target_os = "macos")]
#[repr(C)]
struct ProcFileInfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: libc::off_t,
    fi_type: i32,
    fi_guardflags: u32,
}

/// `struct vnode_fdinfo` of `<sys/proc_info.h>` (not in the libc crate).
#[cfg(target_os = "macos")]
#[repr(C)]
struct VnodeFdInfo {
    pfi: ProcFileInfo,
    pvi: libc::vnode_info,
}

/// `PROC_PIDFDVNODEINFO` of `<sys/proc_info.h>` (not in the libc crate).
#[cfg(target_os = "macos")]
const PROC_PIDFDVNODEINFO: libc::c_int = 1;

/// Whether `pid` has a descriptor open on the file `dev`/`ino`. `false` for a
/// process that cannot be read: another user's, or one that ended.
#[cfg(target_os = "macos")]
fn has_open(pid: libc::pid_t, dev: u32, ino: u64) -> bool {
    use std::mem::size_of;
    // SAFETY: a null buffer asks for the size of the descriptor list only.
    let need =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if need <= 0 {
        return false;
    }
    // Room for descriptors opened in between.
    let mut fds: Vec<libc::proc_fdinfo> =
        Vec::with_capacity(need as usize / size_of::<libc::proc_fdinfo>() + 32);
    let bytes = (fds.capacity() * size_of::<libc::proc_fdinfo>()) as libc::c_int;
    // SAFETY: the buffer has room for `bytes` bytes; the call returns how many
    // it wrote, whole entries only.
    let used = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            bytes,
        )
    };
    if used <= 0 {
        return false;
    }
    // SAFETY: proc_pidinfo initialized `used` bytes of plain-data entries,
    // within the capacity.
    unsafe { fds.set_len(used as usize / size_of::<libc::proc_fdinfo>()) };
    let size = size_of::<VnodeFdInfo>() as libc::c_int;
    fds.iter()
        .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        .any(|fd| {
            // SAFETY: zeroed VnodeFdInfo is a valid out-parameter (plain data).
            let mut info: VnodeFdInfo = unsafe { std::mem::zeroed() };
            // SAFETY: proc_pidfdinfo writes at most `size` bytes into `info`.
            let got = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDVNODEINFO,
                    (&mut info as *mut VnodeFdInfo).cast(),
                    size,
                )
            };
            got == size && info.pvi.vi_stat.vst_dev == dev && info.pvi.vi_stat.vst_ino == ino
        })
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

    /// The lister names a process that has the file open and left this
    /// process's tree (its own `setsid`, parent gone), and leaves out a child
    /// of this process that has it open, and this process itself.
    #[cfg(target_os = "macos")]
    #[test]
    fn holders_names_a_detached_holder_and_not_a_child_or_this_process() {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        let dir = tempfile::tempdir().expect("dir");
        let hold = dir.path().join("hold");
        std::fs::File::create(&hold).expect("hold");
        let other = dir.path().join("other");
        std::fs::File::create(&other).expect("other");
        let _mine = std::fs::File::open(&hold).expect("open");
        // Prints the detached holder's PID once it has the file open.
        let mut spawner = Command::new("perl")
            .arg("-e")
            .arg(
                "use POSIX; open(my $f, '<', $ARGV[0]) or die; my $p = fork(); \
                 if ($p) { print \"$p\\n\"; exit 0 } POSIX::setsid(); \
                 close(STDOUT); sleep 30;",
            )
            .arg(&hold)
            .stdout(Stdio::piped())
            .spawn()
            .expect("perl");
        let mut line = String::new();
        std::io::BufReader::new(spawner.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("read");
        spawner.wait().expect("wait");
        let detached: libc::pid_t = line.trim().parse().expect("pid");
        let mut child = Command::new("perl")
            .arg("-e")
            .arg("$| = 1; open(my $f, '<', $ARGV[0]) or die; print \"ok\\n\"; sleep 30;")
            .arg(&hold)
            .stdout(Stdio::piped())
            .spawn()
            .expect("perl");
        let mut ok = String::new();
        std::io::BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut ok)
            .expect("read");
        let found = super::holders(&hold);
        let none = super::holders(&other);
        // SAFETY: kills the two processes this test started.
        unsafe {
            libc::kill(detached, libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
        let found = found.expect("list");
        assert_eq!(found, vec![detached], "child {}", child.id());
        assert_eq!(none.expect("list"), Vec::<libc::pid_t>::new());
    }
}
