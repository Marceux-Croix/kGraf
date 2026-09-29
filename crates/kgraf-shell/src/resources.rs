//! Process memory and CPU-time sampling for `--measure`.
//!
//! On macOS, "physical footprint" (what Activity Monitor shows as Memory) is the right
//! figure: RSS counts shared framework pages, and macOS has no PSS. Only macOS is
//! implemented; elsewhere the sample is `None` and the report says "n/a" rather than
//! printing a number from a different metric.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceSample {
    /// Current physical footprint, bytes.
    pub phys_footprint: u64,
    /// Lifetime peak physical footprint, bytes.
    pub lifetime_peak: u64,
    /// User plus system CPU time consumed so far.
    pub cpu_time: Duration,
}

#[cfg(target_os = "macos")]
pub fn sample() -> Option<ResourceSample> {
    // SAFETY: `proc_pid_rusage` fills a caller-owned `rusage_info_v4` for our own pid;
    // the pointer is valid for the call and the struct is plain data. `getrusage`
    // likewise writes into a caller-owned `rusage`.
    unsafe {
        let mut info: libc::rusage_info_v4 = std::mem::zeroed();
        let rc = libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            (&mut info as *mut libc::rusage_info_v4).cast::<libc::rusage_info_t>(),
        );
        if rc != 0 {
            return None;
        }
        // CPU time from getrusage: rusage_info's ri_user_time is in Mach ticks on
        // Apple silicon, not nanoseconds, so it is not used.
        let mut ru: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut ru) != 0 {
            return None;
        }
        let tv = |t: libc::timeval| {
            Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
        };
        Some(ResourceSample {
            phys_footprint: info.ri_phys_footprint,
            // The kernel updates the lifetime maximum lazily, so one snapshot can
            // report it below the current footprint; the peak is never less than
            // the footprint the same call observed.
            lifetime_peak: info
                .ri_lifetime_max_phys_footprint
                .max(info.ri_phys_footprint),
            cpu_time: tv(ru.ru_utime) + tv(ru.ru_stime),
        })
    }
}

#[cfg(not(target_os = "macos"))]
pub fn sample() -> Option<ResourceSample> {
    None
}

/// Bytes to MiB with one decimal, for the report.
pub fn mib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mib_formats() {
        assert_eq!(mib(1024 * 1024 * 3 / 2), "1.5");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sample_reads_a_live_footprint() {
        let s = sample().expect("proc_pid_rusage works for our own pid");
        assert!(s.phys_footprint > 0);
        assert!(s.lifetime_peak >= s.phys_footprint);
    }
}
