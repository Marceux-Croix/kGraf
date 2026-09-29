//! Real, multiple-process tests of [`kgraf_provider::SharedLimiterStore`]
//! (decision 0072). The real-speaker rule's "verified against a running speaker" reasoning
//! extends to a cross-process mechanism: an in-process stand-in for "another
//! process" would restate this crate's own assumption that SQLite's file
//! locking, not shared memory, is what makes two `RouteLimiter`s agree.
//! Every process here is `crosslimit-hammer` (`kgraf-crosslimit-hammer`,
//! dev-dependency-only), built with `escargot` the same way `kgraf-mcp`'s
//! live tests build `mcp-echo-server`.

use kgraf_provider::{RateLimit, SharedLimiterStore};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

fn hammer_binary() -> PathBuf {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        escargot::CargoBuild::new()
            .bin("crosslimit-hammer")
            .manifest_path(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../kgraf-crosslimit-hammer/Cargo.toml"
            ))
            .current_release()
            .current_target()
            .run()
            .expect("build kgraf-crosslimit-hammer's crosslimit-hammer binary")
            .path()
            .to_path_buf()
    })
    .clone()
}

/// Several real processes hammering one route and key stay within the
/// configured rate plus tolerance, summed across every process: the failure
/// mode decision 0072 exists to close (each process honoring the limit on its
/// own, but together exceeding it).
#[test]
fn several_processes_hammering_one_route_stay_within_the_shared_limit() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("shared.sqlite3");
    // 600/minute = 10/second, burst 1: over a 3 s window, at most ~30 admissions
    // total should be possible, however many processes are hammering, because
    // the state is shared, not per process.
    let spec = "600/1@0";
    let duration = Duration::from_secs(3);
    const PROCESSES: usize = 6;

    let children: Vec<_> = (0..PROCESSES)
        .map(|_| {
            Command::new(hammer_binary())
                .args([
                    "admit",
                    path.to_str().expect("utf-8 path"),
                    "route",
                    "key",
                    spec,
                    &duration.as_millis().to_string(),
                ])
                .stdout(Stdio::piped())
                .spawn()
                .expect("spawn crosslimit-hammer")
        })
        .collect();

    let mut total_admitted = 0_u64;
    for child in children {
        let output = child.wait_with_output().expect("wait for hammer process");
        assert!(output.status.success(), "{output:?}");
        let count: u64 = String::from_utf8(output.stdout)
            .expect("utf-8 stdout")
            .trim()
            .parse()
            .expect("a printed admitted count");
        total_admitted += count;
    }

    // 10/second for 3 s is 30, plus one burst credit at start. Six real
    // processes polling on their own schedules (each backing off up to 5 ms
    // after a `Wait`, `crosslimit-hammer`'s own admit loop) add real
    // scheduling jitter around that ideal figure; a generous ceiling well
    // below what six *unshared* per-process limiters would allow (each on
    // its own 10/second, about 180 total over 3 s) still proves the state is
    // actually shared, not merely under some loose bound.
    let ceiling = 60;
    assert!(
        total_admitted <= ceiling,
        "{PROCESSES} processes admitted {total_admitted} total, over the {ceiling} ceiling for \
         a shared 10/second limit: the processes did not actually share state"
    );
    assert!(
        total_admitted > 0,
        "nothing was ever admitted; the mechanism itself is broken, not just over-permissive"
    );
}

/// A process killed while it holds the shared store's transaction lock (a
/// crash mid-write) leaves no stuck lock: the next check elsewhere succeeds
/// promptly, not after `busy_timeout`'s full five seconds.
#[test]
fn a_process_killed_mid_transaction_leaves_no_stuck_lock() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("shared.sqlite3");
    // Prime the file (creates the schema) before the holder opens its own raw
    // connection, so `hold` only has to `BEGIN IMMEDIATE`, not race table
    // creation with the assertion below's `SharedLimiterStore::open`.
    drop(SharedLimiterStore::open(&path).expect("prime the store"));

    let mut holder = Command::new(hammer_binary())
        .args(["hold", path.to_str().expect("utf-8 path")])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the holder");
    let stdout = holder.stdout.take().expect("holder stdout");
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(stdout), &mut line)
        .expect("read the HOLDING line");
    assert_eq!(
        line.trim(),
        "HOLDING",
        "the holder did not confirm its lock"
    );

    // SIGKILL: the crash case, not a cooperative exit that could release the
    // lock on its own and defeat the point of this test.
    holder.kill().expect("kill the holder");
    holder.wait().expect("reap the holder");

    let store = SharedLimiterStore::open(&path).expect("open after the crash");
    let start = std::time::Instant::now();
    store
        .check(
            "route",
            "key",
            RateLimit::parse("60").expect("spec"),
            SystemTime::now(),
            Duration::from_secs(2),
        )
        .expect("check after the crash");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "a check after the crash took {elapsed:?}: the killed process's lock was still held, \
         waiting out (part of) the 5 s busy_timeout instead of being released with its process"
    );
}
