//! Process resource sampling for the soak driver: memory, open file descriptors, threads and
//! child processes of this process. Linux reads `/proc`; macOS uses `libproc`. Other platforms
//! report nothing, and the driver says so rather than printing zeros.

/// One reading of this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// Memory the judge follows: physical footprint on macOS (what Activity Monitor shows),
    /// resident set size on Linux.
    pub mem_bytes: u64,
    /// Which metric `mem_bytes` is.
    pub mem_metric: &'static str,
    /// Resident set size, on both.
    pub rss_bytes: u64,
    pub fds: u64,
    pub threads: u64,
    /// Direct children of this process, not counting `except`.
    pub children: u64,
}

/// Hand the allocator's free pages back to the OS, so a reading counts memory in use and not memory
/// the allocator kept for reuse. Without it a reading moves by a few MiB with the system's memory
/// pressure and hides a slow leak behind noise (measured: a flat process swung by 4 MiB).
pub fn release_free_memory() {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut libc::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone means every zone; a goal of 0 means release all that can be.
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: `malloc_trim` takes no pointers; 0 keeps no padding at the top of the heap.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(target_os = "macos")]
fn sample_with(except: Option<u32>, pids: &[u32]) -> Option<Sample> {
    use std::mem::size_of;
    let pid = std::process::id() as libc::pid_t;
    // SAFETY: each libproc call writes into a caller-owned, correctly sized buffer for our own
    // pid; the structs are plain data and zero-initialized.
    unsafe {
        let mut info: libc::rusage_info_v4 = std::mem::zeroed();
        if libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            (&mut info as *mut libc::rusage_info_v4).cast::<libc::rusage_info_t>(),
        ) != 0
        {
            return None;
        }
        let mut task: libc::proc_taskinfo = std::mem::zeroed();
        let got = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut task as *mut libc::proc_taskinfo).cast(),
            size_of::<libc::proc_taskinfo>() as libc::c_int,
        );
        if got <= 0 {
            return None;
        }
        // The file descriptor list: ask for the size, then read into a buffer with room to
        // spare for descriptors opened in between.
        let need = libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0);
        if need <= 0 {
            return None;
        }
        let entry = size_of::<libc::proc_fdinfo>();
        let mut fds = vec![0u8; need as usize + 16 * entry];
        let used = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            fds.len() as libc::c_int,
        );
        if used < 0 {
            return None;
        }
        let children = count_excluding(pids, except);
        Some(Sample {
            mem_bytes: info.ri_phys_footprint,
            mem_metric: "phys_footprint",
            rss_bytes: info.ri_resident_size,
            fds: used as u64 / entry as u64,
            threads: task.pti_threadnum as u64,
            children,
        })
    }
}

/// Direct children of this process, in one listing. A child that is mid-spawn can appear in a
/// listing and be gone from the next, so a caller that needs a stable answer takes one listing.
#[cfg(target_os = "macos")]
fn child_pids() -> Vec<u32> {
    // The buffer is zeroed, so the filled entries are the nonzero ones. The return value is a
    // count on some releases and a byte size on others.
    let mut kids = vec![0 as libc::pid_t; 4096];
    // SAFETY: the call writes at most the given byte size into the caller-owned buffer.
    unsafe {
        libc::proc_listchildpids(
            std::process::id() as libc::pid_t,
            kids.as_mut_ptr().cast(),
            (kids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        );
    }
    kids.into_iter()
        .filter(|&k| k > 0)
        .map(|k| k as u32)
        .collect()
}

#[cfg(target_os = "linux")]
fn child_pids() -> Vec<u32> {
    let me = std::process::id();
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&p| p != me && parent_of(p) == Some(me))
        .collect()
}

/// A reading of this process, with the children counted from one listing.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn sample(except: Option<u32>) -> Option<Sample> {
    sample_with(except, &child_pids())
}

/// The number of pids in `pids`, not counting `except`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn count_excluding(pids: &[u32], except: Option<u32>) -> u64 {
    pids.iter().filter(|&&p| Some(p) != except).count() as u64
}

#[cfg(target_os = "linux")]
fn sample_with(except: Option<u32>, pids: &[u32]) -> Option<Sample> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let rss_kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    // `read_dir` holds its own descriptor while it lists `/proc/self/fd`.
    let fds = (std::fs::read_dir("/proc/self/fd").ok()?.count() as u64).saturating_sub(1);
    let threads = std::fs::read_dir("/proc/self/task").ok()?.count() as u64;
    let children = count_excluding(pids, except);
    Some(Sample {
        mem_bytes: rss_kb * 1024,
        mem_metric: "rss",
        rss_bytes: rss_kb * 1024,
        fds,
        threads,
        children,
    })
}

/// The parent pid in `/proc/<pid>/stat`: the field after the command's closing parenthesis and
/// the state letter. A process that exited between the listing and the read is `None`.
#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn sample(_except: Option<u32>) -> Option<Sample> {
    None
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;

    /// Tests share one process: a reading in one must not see the other's descriptors.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_live_process_reads_nonzero() {
        let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let s = sample(None).expect("sample");
        assert!(s.mem_bytes > 0 && s.rss_bytes > 0);
        assert!(s.fds >= 3, "stdio is open: {s:?}");
        assert!(s.threads >= 1);
    }

    #[test]
    fn an_opened_file_and_a_child_show_up_and_go() {
        let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let before = sample(None).expect("before");
        let file = std::fs::File::open("/dev/null").expect("open");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn");
        let known = child.id();
        let during = sample(None).expect("during");
        assert_eq!(during.fds, before.fds + 1, "{before:?} {during:?}");
        // Children are checked by pid, not by total: a child mid-spawn (seen on a loaded macOS
        // host as an extra pid beside the sleep that is gone one listing later) or any other
        // unrelated child must not change the claim. One listing is taken, so the three
        // readings below describe the same set.
        let listing = child_pids();
        assert!(listing.contains(&known), "{listing:?} lacks {known}");
        assert_eq!(
            count_excluding(&listing, Some(known)) + 1,
            count_excluding(&listing, None),
            "{listing:?}"
        );
        // The named child is left out of the count, and only that one.
        let with = sample_with(None, &listing).expect("with");
        let without = sample_with(Some(known), &listing).expect("without");
        assert_eq!(with.children, without.children + 1);
        child.kill().expect("kill");
        child.wait().expect("wait");
        drop(file);
        let after = sample(None).expect("after");
        assert_eq!(after.fds, before.fds);
        assert!(
            !child_pids().contains(&known),
            "a reaped child is not listed"
        );
    }
}
