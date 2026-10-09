//! Ending a job object: every process in it, and the wait until it holds
//! none. The steps sit behind [`Job`] so they are tested on every host.

use std::io;
use std::time::{Duration, Instant};

/// A job object: a set of processes that can be ended together.
pub trait Job {
    /// End every process in the job. Returns before they have ended.
    fn terminate(&mut self) -> io::Result<()>;
    /// How many processes the job still holds.
    fn active(&self) -> io::Result<u32>;
}

/// How often the job is polled, and ended again, while it holds a process.
const END_POLL: Duration = Duration::from_millis(10);

/// End every process of `job`, then wait until it holds none, at most
/// `bound`. True when none is left. A process's handles, its ends of the
/// output pipes among them, close only once it has ended, and a terminate
/// call returns before that: a reader that waits for the end of a pipe
/// starts its wait after this, not after the terminate call.
///
/// The job is ended again on every poll that still finds a process: one that
/// was being created when the first terminate ran joins the job after it and
/// would otherwise outlive it, with the loop waiting out the whole bound for
/// it.
pub fn end_job(job: &mut impl Job, bound: Duration) -> io::Result<bool> {
    job.terminate()?;
    let deadline = crate::waits::deadline_after(bound);
    loop {
        if job.active()? == 0 {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(END_POLL);
        job.terminate()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A job whose processes end only when the terminate call that follows
    /// their creation reaches them: `late` processes join the job after each
    /// of the first `late` terminate calls.
    struct LateJob {
        terminates: u32,
        late: u32,
        active: u32,
    }

    impl Job for LateJob {
        fn terminate(&mut self) -> io::Result<()> {
            self.terminates += 1;
            // Everything present now ends; a process being created at this
            // moment joins afterwards.
            self.active = if self.terminates <= self.late { 1 } else { 0 };
            Ok(())
        }
        fn active(&self) -> io::Result<u32> {
            Ok(self.active)
        }
    }

    #[test]
    fn a_process_created_during_the_first_terminate_is_ended_by_the_next() {
        let mut job = LateJob {
            terminates: 0,
            late: 2,
            active: 1,
        };
        let started = Instant::now();
        let ended = end_job(&mut job, Duration::from_secs(5)).unwrap();
        assert!(ended, "the job still held a process");
        assert_eq!(job.terminates, 3);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_bound_too_large_to_add_to_now_does_not_panic() {
        let mut job = LateJob {
            terminates: 0,
            late: 0,
            active: 0,
        };
        assert!(end_job(&mut job, Duration::MAX).unwrap());
    }

    #[test]
    fn a_job_that_never_empties_is_false_after_the_bound() {
        let mut job = LateJob {
            terminates: 0,
            late: u32::MAX,
            active: 1,
        };
        let started = Instant::now();
        let ended = end_job(&mut job, Duration::from_millis(100)).unwrap();
        assert!(!ended);
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(job.terminates > 2, "ended again on each poll");
    }
}
