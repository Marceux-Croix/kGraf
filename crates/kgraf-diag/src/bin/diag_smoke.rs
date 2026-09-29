//! A minimal process for the exit-flush, cross-process and env-override tests
//! (`tests/exit_flush.rs`, `tests/cross_process.rs`,
//! `tests/disable_and_override_env.rs`): `init` from
//! [`kgraf_diag::resolve_path`] exactly the way `kgraf-cli`'s and
//! `kgraf-shell`'s own `main` do (so `KGRAF_DIAG_PATH`/`KGRAF_DIAG_DISABLE`
//! behave identically here as in a real binary; an earlier version of this
//! file used its own `KGRAF_DIAG_SMOKE_PATH` for `init`, which meant a test
//! that forgot to *also* set `KGRAF_DIAG_PATH` still let `scrub`'s key
//! loading fall through to the real per-user path), hold a
//! [`kgraf_diag::guard`], record `KGRAF_DIAG_SMOKE_COUNT` (default 1) error
//! events, and return. Nothing else. The point is that `main` does no
//! explicit flush of its own beyond the guard: if records are lost, the
//! guard (or its absence) is the only variable.

use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn main() {
    let count: usize = env::var("KGRAF_DIAG_SMOKE_COUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let cap: u64 = env::var("KGRAF_DIAG_SMOKE_CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(kgraf_diag::DEFAULT_CAP_BYTES);

    // A lockstep start: several of
    // these processes spawned back-to-back still drift apart by scheduling
    // and process-startup jitter before they reach their record loop, which
    // softens the contention a cross-process locking test needs. When the
    // caller (`tests/cross_process.rs`) sets this, every participant spins
    // on the same not-yet-existing file and starts its loop within
    // microseconds of every other participant, once the test creates it.
    let wait = || {
        if let Some(wait_path) = env::var_os("KGRAF_DIAG_SMOKE_WAIT_FILE") {
            let wait_path = PathBuf::from(wait_path);
            while !wait_path.exists() {
                std::thread::sleep(Duration::from_micros(200));
            }
        }
    };

    // `KGRAF_DIAG_SMOKE_DIRECT`: call `kgraf_diag::store::append` synchronously,
    // once per record, instead of going through the process-wide writer
    // thread's channel. `tests/cross_process.rs`'s trim-race test needs this:
    // the normal path stamps a record's `ts_ms` when `record()` enqueues it
    // (on this thread), not when the background writer actually appends it,
    // so a freshness check ("the newest surviving record is recent") is
    // meaningless there once a widened trim window (`KGRAF_DIAG_TEST_
    // WIDEN_TRIM_RACE_US`) makes the queue drain far slower than it fills:
    // every timestamp ends up bunched near process start regardless of when
    // the disk write actually lands. Calling `append` directly makes
    // `ts_ms` and the real write happen at the same instant, which is what
    // that check needs to mean anything.
    if env::var_os("KGRAF_DIAG_SMOKE_DIRECT").is_some() {
        let path = kgraf_diag::resolve_path().expect("KGRAF_DIAG_PATH must be set");
        // `KGRAF_DIAG_SMOKE_STOP_FILE` (the trimmer role only, in
        // `tests/cross_process.rs`'s trim-race test): a capped ring buffer
        // legitimately drops its oldest bytes as newer ones arrive, so a
        // trimmer that keeps outliving the appenders it is racing by a fixed,
        // timing-dependent iteration count will, completely correctly, trim
        // an appender's already-safely-written last record away long after
        // the race the test cares about is over -- not the bug this test
        // exists to catch (a stale-read trim clobbering a concurrent write),
        // just ordinary rotation given enough of the trimmer's own later
        // writes. `count` stays as a safety upper bound (so a missing or
        // never-created stop file cannot hang this process), but the loop
        // also breaks, deterministically, as soon as it observes the stop
        // file, one iteration after the last check -- never by sleeping
        // longer, and without capping how many *appender* records can land
        // during the race itself.
        let stop_file = env::var_os("KGRAF_DIAG_SMOKE_STOP_FILE").map(PathBuf::from);
        wait();
        for i in 0..count {
            let record = kgraf_diag::Record::now(kgraf_diag::DiagEvent::error(
                "KG-9999",
                "kgraf-diag",
                format!("direct record {i} from pid {}", std::process::id()),
            ));
            if let Err(e) = kgraf_diag::store::append(&path, &record, cap) {
                eprintln!("diag-smoke: {e}");
                std::process::exit(1);
            }
            if stop_file.as_deref().is_some_and(Path::exists) {
                break;
            }
        }
        return;
    }

    if let Some(path) = kgraf_diag::resolve_path() {
        kgraf_diag::init(path, cap);
    }
    let _guard = kgraf_diag::guard(Duration::from_secs(5));
    wait();

    for i in 0..count {
        // Scrubbing (not just recording) is what used to touch the real
        // per-user key file even when `KGRAF_DIAG_DISABLE`/`KGRAF_DIAG_PATH`
        // redirected everything else (`scrub::install_key` used
        // `default_path` instead of `resolve_path`); every record here goes
        // through `scrub_text` first, the same as `kgraf-cli`'s own error
        // path, so `tests/exit_flush.rs` and any future key-leak test
        // exercise the real call path.
        let context = kgraf_diag::scrub::scrub_text(
            &format!(
                "smoke record {i} from pid {} at /some/path",
                std::process::id()
            ),
            None,
        );
        kgraf_diag::record(kgraf_diag::DiagEvent::error(
            "KG-9999",
            "kgraf-diag",
            context,
        ));
    }
    // No explicit flush call: only the guard's `Drop`, at the end of this
    // function's scope, stands between "recorded" and "on disk".
}
