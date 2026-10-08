//! The runtime the multi-worker end-to-end tests run on.
//!
//! `#[tokio::test]` drops its runtime at the end of the test, and dropping a
//! runtime waits without limit for every `spawn_blocking` task. A blocking
//! task stuck on a child process or a lock then holds the test until the
//! test runner kills the whole binary, and the run says nothing about what
//! was stuck. Here the runtime is shut down with `shutdown_timeout`, and a
//! watchdog prints the lock waits and child processes in flight
//! (`kgraf_core::waits::report`) when a test runs long, and again when its
//! blocking tasks outlive it, which then fails the test.

use std::future::Future;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// When the watchdog first prints. The slowest of these tests takes about
/// 20 s on a loaded Windows runner; each bounds its own run at 120 s.
const WATCHDOG_AFTER: Duration = Duration::from_secs(90);
/// How often it prints again after that.
const WATCHDOG_REPEAT: Duration = Duration::from_secs(60);
/// How long blocking tasks get to end after the test's future has.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Which runtime a test gets.
#[derive(Clone, Copy)]
pub(crate) enum Flavor {
    /// Four worker threads.
    MultiThread,
    /// The test's own thread.
    CurrentThread,
}

/// Run `fut` on a fresh runtime of `flavor`, watched, and shut the runtime
/// down within [`SHUTDOWN_TIMEOUT`].
pub(crate) fn run<F: Future<Output = ()>>(name: &str, flavor: Flavor, fut: F) {
    run_with(name, flavor, fut, WATCHDOG_AFTER, SHUTDOWN_TIMEOUT);
}

/// [`run`] with its bounds given. Panics when the test panicked (with the
/// test's own panic) or when a blocking task was still running at
/// `shutdown`.
pub(crate) fn run_with<F: Future<Output = ()>>(
    name: &str,
    flavor: Flavor,
    fut: F,
    watchdog_after: Duration,
    shutdown: Duration,
) {
    let rt = match flavor {
        Flavor::MultiThread => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build(),
        Flavor::CurrentThread => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
    }
    .expect("a runtime");
    let (done, watched) = mpsc::channel::<()>();
    let label = name.to_owned();
    let started = Instant::now();
    let watchdog = std::thread::spawn(move || {
        let mut after = watchdog_after;
        loop {
            match watched.recv_timeout(after) {
                Err(RecvTimeoutError::Timeout) => {
                    eprintln!(
                        "[watchdog] {label} still running after {:?}; in flight:\n{}",
                        started.elapsed(),
                        in_flight()
                    );
                    after = WATCHDOG_REPEAT;
                }
                _ => return,
            }
        }
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.block_on(fut)));
    let shutting = Instant::now();
    rt.shutdown_timeout(shutdown);
    let took = shutting.elapsed();
    drop(done);
    let _ = watchdog.join();
    let stuck = took >= shutdown;
    if stuck {
        eprintln!(
            "[watchdog] {name}: a blocking task was still running {shutdown:?} after the test \
             ended; in flight:\n{}",
            in_flight()
        );
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(
        !stuck,
        "{name}: a blocking task was still running {shutdown:?} after the test ended"
    );
}

fn in_flight() -> String {
    let report = kgraf_core::waits::report();
    if report.is_empty() {
        "(no lock wait or child process recorded)".to_owned()
    } else {
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `spawn_blocking` task that never ends: the wrapper returns at the
    /// shutdown bound, failing the test, instead of waiting for the task.
    #[test]
    fn a_stuck_blocking_task_fails_the_test_at_the_shutdown_bound() {
        let (release, stuck) = mpsc::channel::<()>();
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(|| {
                run_with(
                    "probe",
                    Flavor::MultiThread,
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let _ = stuck.recv();
                        });
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    },
                    Duration::from_secs(60),
                    Duration::from_millis(500),
                );
            });
            let _ = tx.send(outcome.is_err());
        });
        let failed = rx.recv_timeout(Duration::from_secs(20));
        let _ = release.send(());
        assert_eq!(failed, Ok(true), "the wrapper returned and failed the test");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn a_test_that_ends_cleanly_passes() {
        run_with(
            "probe",
            Flavor::CurrentThread,
            async {
                tokio::task::spawn_blocking(|| ()).await.expect("joined");
            },
            Duration::from_secs(60),
            Duration::from_secs(10),
        );
    }
}
