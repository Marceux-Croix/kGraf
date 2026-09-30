//! A second real OS process against `kgraf_provider::SharedLimiterStore`
//! (decision 0072). Dev-dependency-only, built by `kgraf-provider`'s
//! `tests/crosslimit_processes.rs` through `escargot`, mirroring
//! `kgraf-mcp-test-server`'s pattern: a protocol or shared-state
//! mechanism between processes is verified against a real second process, not
//! an in-process stand-in for one.
//!
//! ## `admit <path> <route> <key> <spec> <duration_ms>`
//!
//! Calls `SharedLimiterStore::check` in a tight loop against the same route
//! and key for `duration_ms`, and prints the number of `Admitted` outcomes on
//! its own line to stdout when it exits. Several of these against one store
//! at once is the multi-process rate test: the sum of every process's printed
//! count must stay within the configured rate plus tolerance.
//!
//! ## `hold <path>`
//!
//! Opens its own connection to `path`, starts `BEGIN IMMEDIATE`, prints
//! `HOLDING` and flushes, then parks forever. A test sends `SIGKILL` (or the
//! platform equivalent) to this process while it holds the transaction open,
//! then checks that a fresh `SharedLimiterStore::check` against the same
//! path still succeeds promptly: SQLite releases a process's locks when its
//! last file descriptor on the database closes, which a kill does, so no
//! stuck lock should outlive it.

use kgraf_provider::{RateLimit, SharedLimiterStore};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("admit") => admit(&args[2..]),
        Some("hold") => hold(&args[2..]),
        other => {
            eprintln!("unknown mode {other:?}; expected `admit` or `hold`");
            std::process::exit(2);
        }
    }
}

fn admit(args: &[String]) {
    let [path, route, key, spec, duration_ms] = args else {
        eprintln!("usage: crosslimit-hammer admit <path> <route> <key> <spec> <duration_ms>");
        std::process::exit(2);
    };
    let store = open_retrying_on_contention(&PathBuf::from(path));
    let limit = RateLimit::parse(spec).expect("parse the rate limit spec");
    let duration = Duration::from_millis(duration_ms.parse().expect("parse duration_ms"));
    // Each call's own wait-for-the-lock budget: generous, since this binary's
    // job is to present load, not to exercise the budget itself (that is
    // `a_held_write_lock_returns_within_its_budget`, in `crosslimit.rs`).
    let lock_budget = Duration::from_secs(5);
    let start = Instant::now();
    let mut admitted = 0_u64;
    while start.elapsed() < duration {
        match store.check_now(route, key, limit, lock_budget) {
            Ok(kgraf_provider::crosslimit::Admission::Admitted) => admitted += 1,
            Ok(kgraf_provider::crosslimit::Admission::Wait(w)) => {
                // A short, bounded sleep: a busy loop here would spend this
                // process's whole budget on SQLite contention rather than on
                // presenting real concurrent load, which is what the
                // multi-process test needs to measure.
                std::thread::sleep(w.min(Duration::from_millis(5)));
            }
            // A lock timeout under real saturation is an expected, correct
            // outcome, not a bug in this
            // load-generating tool: retry rather than aborting the whole
            // process, which would understate the contention a still-running
            // sibling process presents for the rest of the measured window.
            Err(kgraf_provider::CrossLimitError::LockTimeout { .. }) => {}
            Err(e) => panic!("check the shared limiter store: {e}"),
        }
    }
    println!("{admitted}");
}

/// `SharedLimiterStore::open`, retrying its own `LockTimeout` (review
/// MEDIUM, fix 4 and fix 5): many of these processes can start at once
/// against the same fresh file (this bench's own concurrency levels do
/// exactly that), and open's writability probe contending briefly with
/// another process's own `open` is expected, correct contention, not a bug
/// in this load-generating tool.
fn open_retrying_on_contention(path: &std::path::Path) -> SharedLimiterStore {
    loop {
        match SharedLimiterStore::open(path) {
            Ok(store) => return store,
            Err(kgraf_provider::CrossLimitError::LockTimeout { .. }) => {}
            Err(e) => panic!("open the shared limiter store: {e}"),
        }
    }
}

fn hold(args: &[String]) {
    let [path] = args else {
        eprintln!("usage: crosslimit-hammer hold <path>");
        std::process::exit(2);
    };
    let conn = rusqlite::Connection::open(path).expect("open a raw connection");
    conn.busy_timeout(Duration::from_secs(5))
        .expect("set busy_timeout");
    conn.execute_batch("BEGIN IMMEDIATE")
        .expect("begin a transaction and hold its lock");
    println!("HOLDING");
    std::io::stdout()
        .flush()
        .expect("flush HOLDING to the parent");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
