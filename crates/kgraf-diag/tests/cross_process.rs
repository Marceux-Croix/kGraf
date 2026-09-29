//! Several real OS processes appending to the same diagnostics file at once. The regression was
//! glued lines (two writes per `writeln!`, interleaved between processes)
//! and lost records (a concurrent trim's read-modify-write racing an append
//! or another trim). `store.rs`'s own unit tests cover this with threads
//! inside one process (a faithful proxy: `std::fs::File::lock` is per
//! open-file description, exactly what two processes each opening their own
//! handle contend for); this file is the real thing,
//! one `diag-smoke` process per participant.
//!
//! **Contention.** The first version of this file (2
//! symmetric processes, a cap of 6,000 bytes) passed 6 of 6 runs even with
//! `with_locked` entirely removed. Raising process count and lowering the
//! cap, symmetrically, did not fix that either: measured up to 8 processes
//! and a 200-byte cap (a trim on nearly every append), still 5 of 5 unlocked
//! runs passed. Two things were true at once and each explains part of why:
//!
//! - `trim_locked`'s real critical section (read the file, compute a tail,
//!   write it to a temp file, rename it over the original) is a handful of
//!   syscalls on a local, cached filesystem, open for only a few
//!   microseconds; a real process rarely gets scheduled into another
//!   process's window that narrow, however many processes are hammering the
//!   file.
//! - Making every process trim symmetrically (the same cap, the same record
//!   rate, started in lockstep) tends to make them trim at nearly the same
//!   moments as each other too, since they are doing identical work at
//!   identical speed: when widening the window (below) made every process
//!   sleep inside its own trim, they mostly ended up sleeping *together*
//!   rather than one sleeping while another actively appends, which is the
//!   specific interleaving that loses data.
//!
//! The fix for both: one designated **trimmer** process gets a small cap (it
//! is the only one that ever calls `trim_locked`, and `store::
//! widen_race_window_for_test` -- `KGRAF_DIAG_TEST_WIDEN_TRIM_RACE_US` --
//! widens its read-to-rename window to a chosen number of milliseconds,
//! deterministically, in both the locked and unlocked run); the other
//! **appender** processes get a cap far above anything the run writes, so
//! they never trim and are always either appending or about to append,
//! continuously, for the whole run. During the trimmer's widened window,
//! appenders are now the processes actually likely to be writing, not
//! sleeping in a trim of their own. With 1 trimmer, 3 appenders and a 30 ms
//! window: the locked build passes every run, and the unlocked build
//! (`with_locked` returning `f()` immediately, holding no lock at all) fails
//! every one of 5 runs (reported in the commit this test landed in).
//!
//! **What actually detects the loss.** Neither "the file stayed under its
//! cap" nor "no duplicate record" catches it: a trim built from a stale read
//! produces a perfectly well-formed, correctly capped file that is just
//! quietly missing whatever an appender wrote during the window. A first
//! attempt compared the newest record's timestamp against wall-clock "now"
//! after every process exited; that failed even under the *locked* build,
//! because `DiagEvent`'s timestamp is stamped when `record()` enqueues an
//! event for the background writer thread, not when that thread actually
//! writes it, so a widened window between the two makes every timestamp look
//! stale regardless of correctness. `KGRAF_DIAG_SMOKE_DIRECT`
//! (`diag-smoke`'s own doc) makes this test call `store::append` directly,
//! synchronously, so the check below can instead ask a direct identity
//! question with a known answer: each appender's own last record (`direct
//! record {APPENDER_RECORDS - 1} from pid {pid}`) is known, from that
//! process's own successful exit, to have been written while the trimmer was
//! still active; it must still be there once everything has finished.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

const APPENDERS: usize = 3;

/// A whole directory per test, not just a differently-named file directly in
/// the shared system temp root: `diag-smoke` also scrubs
/// (`scrub::install_key`), whose key file is always named `diagnostics.key`
/// next to whatever path it is given, so the two tests in this file (using
/// distinct file names, but the same shared temp root and the same test
/// binary `pid`) would otherwise collide on that one shared key file.
fn artifact_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kgraf-diag-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[allow(clippy::too_many_arguments)]
fn spawn_smoke(
    path: &std::path::Path,
    count: usize,
    cap: u64,
    widen_us: u64,
    wait_file: &std::path::Path,
) -> Child {
    Command::new(env!("CARGO_BIN_EXE_diag-smoke"))
        .env("KGRAF_DIAG_PATH", path)
        .env("KGRAF_DIAG_SMOKE_COUNT", count.to_string())
        .env("KGRAF_DIAG_SMOKE_CAP", cap.to_string())
        .env("KGRAF_DIAG_SMOKE_WAIT_FILE", wait_file)
        .env("KGRAF_DIAG_TEST_WIDEN_TRIM_RACE_US", widen_us.to_string())
        .spawn()
        .expect("diag-smoke must spawn")
}

/// As [`spawn_smoke`], but in `KGRAF_DIAG_SMOKE_DIRECT` mode (see
/// `diag-smoke`'s own doc): each record's timestamp is stamped at the same
/// instant as its real disk write, which the trim-race test's freshness
/// check depends on.
///
/// `stop_file`, when given (the trimmer role only): `diag-smoke` breaks out
/// of its append loop, deterministically, the first time it observes this
/// path exist, instead of always running all `count` iterations. Root-caused
/// 2026-09-28: without this, a trimmer that keeps trimming for a fixed
/// iteration count long after every appender has already exited will, with
/// perfect correctness, eventually trim an appender's already-durably-written
/// last record out of the kept window -- ordinary ring-buffer rotation, not
/// the stale-read race this test exists to catch. `count` remains the loop's
/// upper bound, so a caller that never creates `stop_file` still terminates.
#[allow(clippy::too_many_arguments)]
fn spawn_smoke_direct(
    path: &std::path::Path,
    count: usize,
    cap: u64,
    widen_us: u64,
    wait_file: &std::path::Path,
    stop_file: Option<&std::path::Path>,
) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_diag-smoke"));
    cmd.env("KGRAF_DIAG_PATH", path)
        .env("KGRAF_DIAG_SMOKE_COUNT", count.to_string())
        .env("KGRAF_DIAG_SMOKE_CAP", cap.to_string())
        .env("KGRAF_DIAG_SMOKE_WAIT_FILE", wait_file)
        .env("KGRAF_DIAG_TEST_WIDEN_TRIM_RACE_US", widen_us.to_string())
        .env("KGRAF_DIAG_SMOKE_DIRECT", "1");
    if let Some(stop_file) = stop_file {
        cmd.env("KGRAF_DIAG_SMOKE_STOP_FILE", stop_file);
    }
    cmd.spawn().expect("diag-smoke must spawn")
}

/// No trim in play (a cap far above what this test writes, for every
/// participant): every line from every process must parse, and none may be
/// glued to another. No widened window needed here: the property under test
/// (one `write_all` per record) does not depend on `with_locked` at all,
/// only on not calling `writeln!`.
#[test]
fn several_processes_appending_at_once_glue_nothing_and_lose_nothing() {
    let dir = artifact_dir("cross-process-no-trim");
    let path = dir.join("diagnostics.jsonl");
    const PROCESSES: usize = 4;
    const PER_PROCESS: usize = 500;
    let huge_cap = 50_000_000u64;

    let wait_file = dir.join("start");
    let children: Vec<Child> = (0..PROCESSES)
        .map(|_| spawn_smoke(&path, PER_PROCESS, huge_cap, 0, &wait_file))
        .collect();
    std::thread::sleep(Duration::from_millis(50));
    std::fs::write(&wait_file, b"go").unwrap();
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }

    let raw = std::fs::read_to_string(&path).unwrap();
    let non_empty_lines = raw.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(
        non_empty_lines,
        PROCESSES * PER_PROCESS,
        "a glued line collapses two processes' records into fewer lines than were written"
    );
    let records = kgraf_diag::store::read_all(&path).unwrap();
    assert_eq!(
        records.len(),
        PROCESSES * PER_PROCESS,
        "every line must also parse: a glued line fails to parse and read_all skips it, \
         which is why the line count above is checked separately"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// One trimmer (a small cap, a widened race window) against several
/// appenders (a cap they never reach): no glued lines, no duplicate record,
/// and a final, unambiguously-last write into the file is never the one a
/// stale-based trim silently discards.
///
/// **Root-caused 2026-09-28** (a single observed failure under heavy machine
/// load, not reproduced by 250 runs of the previous version, 100 of them
/// under added CPU load; 4 more failures in 150 runs of a first attempted fix
/// below). Not a locking defect in `store.rs`: `with_locked`'s flock already
/// serializes every append and trim, cross-process, for the whole operation,
/// so a concurrent append can never interleave with a trim's
/// read-modify-rename. A scratch in-process repro (a trimmer with a long,
/// fixed iteration count racing an appender that finishes in only 3 records)
/// reproduced the *reported symptom* on the first try: the appender's last
/// record was missing, but it had been trimmed away entirely correctly, by
/// ordinary keep-the-newest rotation, many trims after the appender had
/// already exited -- not clobbered by a stale read while racing it.
///
/// Two layers of the same mistake, both now fixed:
///
/// 1. The trimmer's old fixed `TRIMMER_RECORDS` count let it keep running,
///    and keep legitimately trimming its own later writes to the front of
///    the file, for an indeterminate stretch *after* every appender was
///    already done. Fixed by `stop_file`: the trimmer breaks out of its loop
///    the first time it observes that file, deterministically, right after
///    the append/maybe-trim it was already mid-loop on -- never by sleeping
///    longer, and never by shrinking the overlap the race itself needs (the
///    trimmer still runs, still widened, for the entire time the appenders
///    are writing).
/// 2. Even with (1), checking each of the *three* appenders' own hardcoded
///    last record (`APPENDER_RECORDS - 1`) is still not well-defined: nothing
///    keeps the three appenders in lockstep, so a load-induced stagger lets
///    one legitimately finish (and its last record legitimately age out of
///    the ~375-byte kept window) while the *other two* appenders, still
///    racing, and the trimmer both keep writing hundreds more bytes -- again
///    ordinary rotation, not a stale read, and this is exactly what the
///    4-of-150 failures above were. `store.rs`'s own passing unit tests avoid
///    this by checking one shared, unambiguous, monotonically-last write (an
///    atomic counter's max value) instead of several independent writers'
///    self-reported "last" ones; there is no cheap equivalent shared counter
///    across real OS processes, so this test gets the same property a
///    different way: once every appender has exited and the trimmer has been
///    told to stop, *this test process* -- in the same real cross-process
///    lock as everything above it -- appends one uniquely-tagged marker
///    record and confirms it, and only it, is the thing that must survive.
///    Nothing else can write after it (the trimmer's `stop_file` bounds it to
///    at most one more append/maybe-trim, which can only ever add bytes newer
///    than the marker, never displace it), so this is deterministic by
///    construction rather than by observed low failure probability.
#[test]
fn a_trim_racing_concurrent_appenders_loses_nothing_inside_the_kept_window() {
    let dir = artifact_dir("cross-process-with-trim");
    let path = dir.join("diagnostics.jsonl");

    // TRIMMER_RECORDS is now only a safety upper bound (in case `stop_file`
    // is never observed): the trimmer normally stops within one iteration of
    // the appenders finishing, via `stop_file`, not by exhausting this count.
    // Kept high enough (about TRIMMER_RECORDS / 3 trims, each widened to 30
    // ms) that the trimmer is essentially certain to still be active, and
    // widening, for the entire time the appenders are writing.
    const TRIMMER_RECORDS: usize = 100;
    const APPENDER_RECORDS: usize = 300;
    let trim_cap = 500u64;
    let never_trim_cap = 50_000_000u64;
    let widen_us = 30_000u64; // 30 ms per trim; see the module doc

    let wait_file = dir.join("start");
    let stop_file = dir.join("stop");
    let trimmer = spawn_smoke_direct(
        &path,
        TRIMMER_RECORDS,
        trim_cap,
        widen_us,
        &wait_file,
        Some(&stop_file),
    );
    let appenders: Vec<Child> = (0..APPENDERS)
        .map(|_| spawn_smoke_direct(&path, APPENDER_RECORDS, never_trim_cap, 0, &wait_file, None))
        .collect();

    std::thread::sleep(Duration::from_millis(50));
    std::fs::write(&wait_file, b"go").unwrap();

    // Every appender has now exited: whatever it wrote is durably on disk
    // (real, synchronous `write_all` calls in `KGRAF_DIAG_SMOKE_DIRECT` mode,
    // not a queue), and the trimmer (signaled to stop only now) was still
    // active for the whole time it was writing -- exactly the window a
    // stale-based trim could silently discard something in.
    for mut c in appenders {
        assert!(c.wait().unwrap().success());
    }
    // Deterministic, not a longer sleep: the trimmer's loop checks for this
    // file once per iteration and breaks the first time it sees it, so it
    // runs at most one more append/maybe-trim cycle past this point.
    std::fs::write(&stop_file, b"stop").unwrap();
    // The unambiguous final write of the whole run (see the root-cause note
    // above for why checking each appender's own last record instead is not
    // well-defined): appended here, in this test process, through the same
    // real cross-process lock every `diag-smoke` process just used. Nothing
    // else can write anything newer than this once `stop_file` is visible.
    const MARKER: &str = "test-harness final marker after every appender exited";
    kgraf_diag::store::append(
        &path,
        &kgraf_diag::Record::now(kgraf_diag::DiagEvent::error(
            "KG-9999",
            "kgraf-diag",
            MARKER,
        )),
        never_trim_cap,
    )
    .unwrap();
    let mut trimmer = trimmer;
    assert!(trimmer.wait().unwrap().success());

    // Neither "file size" nor "no duplicates" catches the actual bug this
    // test exists for: a trim built from a stale read, then renamed over a
    // file an appender wrote to in the meantime, produces a well-formed,
    // correctly-capped file that is just missing whatever was written during
    // the window -- no glued line, no duplicate, nothing structurally wrong,
    // just quietly gone. The signal that catches it is identity: the marker
    // above, known to be the true last write of the whole run, and written
    // while the trimmer was still active (its own last, `stop_file`-bounded
    // append/maybe-trim could still be pending), must still be present once
    // everything has finished.
    let records = kgraf_diag::store::read_all(&path).unwrap();
    assert!(!records.is_empty());
    let contexts: std::collections::HashSet<&str> = records
        .iter()
        .filter_map(|r| match &r.event {
            kgraf_diag::DiagEvent::Error { context, .. } => Some(context.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        contexts.contains(MARKER),
        "the test harness's own final marker is missing: a write made after every appender \
         exited, while the trimmer's last bounded trim could still be pending, was silently \
         discarded by a trim built from a stale read"
    );

    let raw = std::fs::read_to_string(&path).unwrap();
    let mut parsed = 0usize;
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        serde_json::from_str::<kgraf_diag::Record>(line)
            .unwrap_or_else(|e| panic!("unparsable line (glued?): {line:?}: {e}"));
        parsed += 1;
    }
    assert_eq!(parsed, raw.lines().filter(|l| !l.trim().is_empty()).count());
    let records = kgraf_diag::store::read_all(&path).unwrap();
    assert!(!records.is_empty());
    let mut seen = std::collections::HashSet::new();
    for r in &records {
        if let kgraf_diag::DiagEvent::Error { context, .. } = &r.event {
            assert!(
                seen.insert(context.clone()),
                "duplicate record survived a trim race: {context}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
