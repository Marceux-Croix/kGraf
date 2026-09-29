//! Cross-process rate limit latency (decision 0072, requirement 4): what
//! [`RouteLimiter::acquire`] pays extra when its backend is
//! [`kgraf_provider::SharedLimiterStore`] instead of the in-process
//! `governor` limiter, at 1, 4 and 16 concurrent processes contending for the
//! same SQLite file.
//!
//! Real OS processes, not threads: threads in this bench's own process would
//! share SQLite's connection-level page cache and lock state in ways a truly
//! separate `kgraf-host` process would not, understating contention (the real-speaker rule's
//! "verified against a real speaker" reasoning applies equally to "real
//! contention"). Each background process is `crosslimit-hammer admit`
//! (`kgraf-crosslimit-hammer`, dev-dependency-only, built with `escargot` as
//! `kgraf-mcp`'s live tests build `mcp-echo-server`), checking its own key on
//! the same route so it contends for the file lock without also competing
//! with the foreground thread's own admissions. Background processes are
//! spawned for the entire measured window (warm-up plus every sample), not a
//! fixed guess at how long the foreground loop will take: a
//! background process that exits partway through would understate
//! contention for whatever samples come after it dies.
//!
//! Run: `cargo bench -p kgraf-provider --bench crosslimit`.
//!
//! **Budget: under 2 ms p99, asserted only at concurrency 1** Uncontended, this is the reliable number the store
//! commits to (the reuse rule: a missed budget fails acceptance), and it is exactly
//! what caught the missing `PRAGMA synchronous = NORMAL` on per-route
//! connections: before that fix, concurrency 1's own p99 and max were both
//! in the hundreds of milliseconds, from every check paying a full fsync
//! regardless of contention. At 4 and 16 concurrent processes, SQLite's own
//! busy backoff is not FIFO, so **p50, p99, p99.9 and max are all reported,
//! honestly, but only p99 at concurrency 1 is asserted against**: a small
//! number of checks can queue behind the backoff for seconds under many
//! tight-loop processes hammering one file continuously, a harsher load than
//! any real caller sees (a real caller is bounded by its own `max_wait_ms`,
//! `RouteLimiter::acquire`, never by this bench's own contention), and
//! asserting a budget there would either be so loose it asserts nothing or
//! so tight it fails nondeterministically on ordinary machine-load variance.
//! This bench's own per-call budget is set generously high (5 s, matching
//! the ledger's own `busy_timeout`) specifically so a genuine tail is
//! measured rather than truncated into a `LockTimeout` error partway
//! through; a `LockTimeout` that still happens under load is retried, not
//! treated as fatal (see `check_once` below).

use kgraf_provider::crosslimit::Admission;
use kgraf_provider::{RateLimit, SharedLimiterStore};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime};

/// Same rate on every key: high enough that neither the foreground
/// measurement nor a background process is refused (`Wait`) during the run,
/// so what is measured is lock contention, not the policy's own throttling.
fn spec() -> &'static str {
    "1000000/1000000@0"
}

/// Generous: this bench measures how long contention can make a check wait,
/// so its own budget must not be the thing that caps what gets measured (see
/// the module doc's "budget" paragraph).
const LOCK_BUDGET: Duration = Duration::from_secs(5);

const WARM_UP_CALLS: usize = 200;
const MEASURED_CALLS: usize = 20_000;

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

/// One background process contending for `path`'s file lock on its own key,
/// for up to `duration`. Killed on drop if the caller does not wait it out.
struct Background(Child);

impl Background {
    fn spawn(path: &Path, key: &str, duration: Duration) -> Self {
        let child = Command::new(hammer_binary())
            .args([
                "admit",
                path.to_str().expect("utf-8 path"),
                "route",
                key,
                spec(),
                &duration.as_millis().to_string(),
            ])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn crosslimit-hammer");
        Self(child)
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

struct Stats {
    p50: Duration,
    p99: Duration,
    p999: Duration,
    max: Duration,
}

/// Measure `SharedLimiterStore::check` latency on the foreground thread while
/// `concurrency - 1` background processes contend for the same file, alive
/// for the whole warm-up-plus-measured window (an upper bound on it,
/// generously padded: the background processes are killed as soon as this
/// function returns, so outliving the measurement wastes nothing but a
/// little wall-clock time).
fn measure(path: &Path, concurrency: usize) -> Stats {
    // A flat, generous ceiling well above any realistic warm-up-plus-measured
    // run (even 20,200 calls at a saturated, contended microsecond-to-
    // millisecond pace is not close to this), rather than an estimate tuned
    // to the common case, (background processes must stay alive
    // through the whole window): the background processes are killed the
    // moment this function returns regardless, so outliving the measurement
    // costs nothing but is never allowed to fall short of it.
    let background_duration = Duration::from_secs(600);
    let _background: Vec<Background> = (0..concurrency.saturating_sub(1))
        .map(|i| Background::spawn(path, &format!("bg-{i}"), background_duration))
        .collect();
    // Give background processes a moment to reach their first transaction, so
    // the measured window starts under real contention, not a cold start.
    std::thread::sleep(Duration::from_millis(50));

    let store = SharedLimiterStore::open(path).expect("open the shared limiter store");
    let limit = RateLimit::parse(spec()).expect("parse spec");
    let check_once = || {
        let start = Instant::now();
        // A `LockTimeout` here is a real, correct outcome under extreme
        // saturation (this is what `RouteLimiter::acquire` turns into a
        // `RateLimited` refusal for a real caller), not a bug in this
        // measurement loop: retried rather than treated as fatal, so one
        // unlucky sample under a debug build's much slower foreground loop
        // (`cargo test --all-targets` runs this bench unoptimized) does not
        // abort the whole run. Its own elapsed time is still folded into the
        // latency this iteration reports, so a run that hits this path shows
        // up honestly in the tail, not silently discarded.
        loop {
            match store.check("route", "fg", limit, SystemTime::now(), LOCK_BUDGET) {
                Ok(Admission::Admitted) => return start.elapsed(),
                Ok(Admission::Wait(_)) => panic!(
                    "the spec's rate is far above what this bench issues; a Wait means the \
                     rate, not contention, was measured"
                ),
                Err(kgraf_provider::CrossLimitError::LockTimeout { .. }) => {}
                Err(e) => panic!("check the shared limiter store: {e}"),
            }
        }
    };

    // Warm-up: not recorded. The first few calls against a freshly primed
    // store (and freshly spawned background processes still reaching their
    // own first transaction) are not the steady-state this bench reports.
    for _ in 0..WARM_UP_CALLS {
        check_once();
    }

    let mut latencies = Vec::with_capacity(MEASURED_CALLS);
    for _ in 0..MEASURED_CALLS {
        latencies.push(check_once());
    }
    latencies.sort();
    Stats {
        p50: percentile(&latencies, 0.50),
        p99: percentile(&latencies, 0.99),
        p999: percentile(&latencies, 0.999),
        max: *latencies.last().expect("MEASURED_CALLS > 0"),
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() {
    // Criterion's harness is not used: percentiles over a fixed, real
    // multi-process contention window are the thing being measured, and
    // criterion's own iteration model would spawn a fresh process fleet per
    // sample. `harness = false` in Cargo.toml already opts this bench out of
    // the criterion test harness; this remains a `cargo bench`-run binary.
    const BUDGET_P99: Duration = Duration::from_millis(2);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bench.sqlite3");
    // Touch the file once outside the measured window so every concurrency
    // level starts from an already-created, already-migrated store.
    drop(SharedLimiterStore::open(&path).expect("prime the store"));

    for concurrency in [1usize, 4, 16] {
        let stats = measure(&path, concurrency);
        println!(
            "crosslimit concurrency={concurrency:>2}: p50={:>8.3} ms  p99={:>8.3} ms  \
             p99.9={:>9.3} ms  max={:>9.3} ms  (n={MEASURED_CALLS}, warm-up={WARM_UP_CALLS})",
            ms(stats.p50),
            ms(stats.p99),
            ms(stats.p999),
            ms(stats.max),
        );
        // The p99 budget is asserted only at concurrency 1 Uncontended, it is the reliable number this store commits
        // to and a regression there is real (this is exactly what caught
        // the missing `synchronous = NORMAL` fix: concurrency 1's own p99
        // and max were both in the hundreds of milliseconds before it).
        // Under real multi-process contention (4 and 16), SQLite's busy
        // backoff is not FIFO: measured maxes of over a second (occasionally
        // into the low tens of seconds under a slower or more loaded
        // machine, per a measurement) are
        // expected from many tight-loop processes hammering one file
        // continuously, which is a harsher load than any real caller sees,
        // since a real caller is bounded by its own `max_wait_ms`
        // (`RouteLimiter::acquire`), never by this bench's own contention.
        // Asserting a p99 budget there would either be so loose it asserts
        // nothing or so tight it fails nondeterministically on ordinary
        // machine-load variance; p99, p99.9 and max at 4 and 16 are printed,
        // honestly, and recorded in decision 0072, but never asserted here.
        if concurrency == 1 {
            assert!(
                stats.p99 <= BUDGET_P99,
                "concurrency 1 (uncontended): p99 {:?} exceeds the {BUDGET_P99:?} budget \
                 (requirement 4, decision 0072)",
                stats.p99
            );
        }
    }
}
