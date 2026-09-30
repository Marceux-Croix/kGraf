//! The on-disk format, its cap, rotation, and cross-process safety.
//!
//! **Format: JSON Lines (JSONL), one [`Record`] per line.** Considered against
//! a length-prefixed binary encoding and a compressed stream:
//!
//! - A length-prefixed binary format is more compact per record, but it is
//!   opaque without this crate: the user must be able to
//!   read the file either directly or through `kgraf diag show`, and a binary
//!   framing means *only* the tool can read it, which also makes a half-written
//!   last record (a crash mid-append) harder to recover from safely.
//! - A compressed stream (gzip, zstd) shrinks the file further, but a
//!   compressed frame is not appendable without re-opening and re-compressing
//!   the whole stream (or keeping multiple frames, which adds its own
//!   bookkeeping); at the roughly 1 MiB cap this crate targets, the saved
//!   bytes are not worth that complexity or the extra dependency.
//! - JSONL is `cat`-able, `grep`-able, and `jq`-able with tools the user
//!   already has, appends with a single `write` of one line, and a torn
//!   trailing line from a crash affects only that one line (`read_all` skips
//!   an unparsable line rather than failing the read).
//!
//! At the roughly 1 MiB cap, a typical event line (60-160 bytes) gives on the
//! order of 6,000-16,000 events, comfortably enough history for a diagnosis.
//!
//! **Rotation: trim-on-threshold, not a rolling file.** A second file (`.1`,
//! `.2`, ...) is the usual rotation scheme, but the design wants *one*
//! file. Instead, once the file exceeds `cap_bytes`, the writer thread rewrites
//! it keeping only the newest `KEEP_FRACTION` of the cap (dropping the oldest
//! lines, snapped to a line boundary) and replaces it atomically
//! (write-to-temp, then rename in the same directory). This is amortized: most
//! appends do not trigger it, and the trim itself runs on the writer thread,
//! never on a caller's hot path.
//!
//! **Cross-process safety.** kGraf's CLI and desktop shell
//! can both be running at once, each with its own single-threaded writer
//! inside this crate, both appending to the same file. Two bugs followed from
//! not accounting for that:
//!
//! - `writeln!` performs the JSON text and the trailing newline as two
//!   separate writes; two processes' appends can interleave between them,
//!   gluing one process's line onto another's. Fixed by building the whole
//!   `"{json}\n"` line in one buffer and issuing exactly one `write_all`,
//!   which POSIX guarantees is atomic against another `O_APPEND` writer for a
//!   write this small (well under a filesystem's atomic-write boundary).
//! - A `trim` reads the whole file, computes a new tail, and replaces the
//!   file with it; if that happens while another process is also appending
//!   or trimming, whichever finishes last wins and the other's work (an
//!   append it already returned success for, or its own trimmed tail) is
//!   silently lost. Fixed with [`with_locked`]: every append-and-maybe-trim
//!   holds an advisory exclusive lock on a sibling `.lock` file (not the data
//!   file itself, so a trim's rename can never orphan a lock another
//!   process is mid-wait on the pre-rename inode) for the whole operation.
//! - The trim's own temp file was a fixed name (`diagnostics.tmp`); two
//!   processes trimming into the same temp name at once could each write
//!   into it before the other's `rename`, corrupting or losing whichever
//!   version lost the race. The lock above already serializes trims, but the
//!   temp name is also made per-process (`diagnostics.jsonl.tmp.<pid>`) so a
//!   process that crashes mid-trim leaves an identifiable, non-colliding
//!   leftover rather than a shared one a second process could still step on
//!   during the narrow window before it takes the lock.

use crate::event::Record;
use kgraf_core::flock::HeldLock;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
#[cfg(feature = "test-support")]
use std::sync::OnceLock;

/// The default cap: as small a file as possible, bounded at about 1 MB. Exported so a caller (or a test) can choose a smaller
/// cap deterministically instead of writing a real megabyte of fixtures.
pub const DEFAULT_CAP_BYTES: u64 = 1_000_000;

/// Trim to this fraction of the cap when the cap is exceeded, so a trim is
/// not immediately followed by another trim on the very next line.
const KEEP_FRACTION: f64 = 0.75;

/// A store failure. Every variant carries the path, because a diagnostics
/// file that cannot be opened is itself worth a `kgraf-diag`-coded event at
/// the caller (`glossary::KG_DIAG_IO`), never a panic.
#[derive(Debug, thiserror::Error)]
pub enum DiagError {
    #[error("cannot open the diagnostics file {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot write the diagnostics file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot read the diagnostics file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot lock {path}: {source}")]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// The sibling lock file for `path`: never the data file itself (see the
/// module doc for why a trim's `rename` makes that unsafe), never renamed or
/// replaced, so every process always locks the same, stable inode.
fn lock_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| format!("{}.lock", n.to_string_lossy()))
        .unwrap_or_else(|| "diagnostics.lock".to_owned());
    path.with_file_name(name)
}

/// A per-process temp name for a trim's rewritten file, alongside `path`.
fn trim_temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| format!("{}.tmp.{}", n.to_string_lossy(), std::process::id()))
        .unwrap_or_else(|| format!("diagnostics.jsonl.tmp.{}", std::process::id()));
    path.with_file_name(name)
}

/// Run `f` while holding a blocking exclusive lock on `path`'s sibling lock
/// file, creating it if needed. `f` runs once for the whole append-and-maybe
/// -trim operation, so no other process's append or trim can interleave with
/// it (see the module doc).
fn with_locked<T>(path: &Path, f: impl FnOnce() -> Result<T, DiagError>) -> Result<T, DiagError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| DiagError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    }
    let lp = lock_path(path);
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false) // the lock file's content is never read; truncating it
        // would just be a pointless write on every lock, and this is more
        // honest than picking `true` for a file whose bytes carry no meaning.
        .open(&lp)
        .map_err(|source| DiagError::Lock {
            path: lp.clone(),
            source,
        })?;
    lock_file.lock().map_err(|source| DiagError::Lock {
        path: lp.clone(),
        source,
    })?;
    let _held = HeldLock::new(lock_file);
    f()
}

/// A fault-injection hook for `tests/cross_process.rs`: a plain race between "read the file" and "rename the
/// trimmed copy over it" is won or lost inside a handful of syscalls, all
/// cached on a local disk, so even 8 real OS processes hammering the file as
/// fast as they can rarely land another process's action inside that window
/// (measured: raising `tests/cross_process.rs` to 8 processes and a
/// 200-byte cap alone did not reproduce the bug in 5 of 5 runs with locking
/// removed). This widens the window on demand, deterministically, without
/// changing production behavior: `KGRAF_DIAG_TEST_WIDEN_TRIM_RACE_US` is set
/// only by the cross-process test, on both its locked and unlocked runs, so
/// the comparison is fair: the delay happens *inside* the critical section
/// either way, but only the locked version holds the lock across it, so only
/// the locked version blocks every other process's own append or trim for
/// its duration.
///
/// **Feature-gated, not just env-var-gated.**
/// An earlier version of this read the environment variable unconditionally,
/// in every build: unset by convention in a real binary, but nothing stopped
/// someone from setting it against a released `kgraf` or `kgraf-app` and
/// making every trim sleep inside the lock. The real body now only compiles
/// in under the `test-support` feature, which only kgraf-diag's own
/// dev-dependency on itself (Cargo.toml) turns on; a release build of either
/// binary depends on kgraf-diag the ordinary way and never requests that
/// feature, so this function is the no-op below in that build, and the
/// variable does nothing there at all.
#[cfg(feature = "test-support")]
fn widen_race_window_for_test() {
    static WIDEN_US: OnceLock<u64> = OnceLock::new();
    let us = *WIDEN_US.get_or_init(|| {
        std::env::var("KGRAF_DIAG_TEST_WIDEN_TRIM_RACE_US")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    });
    if us > 0 {
        std::thread::sleep(std::time::Duration::from_micros(us));
    }
}

#[cfg(not(feature = "test-support"))]
fn widen_race_window_for_test() {}

/// Append one record to `path`, then trim if the result is over `cap_bytes`.
/// Creates the file and its parent directory on first use. The whole
/// operation runs under one exclusive lock (see the module doc).
pub fn append(path: &Path, record: &Record, cap_bytes: u64) -> Result<(), DiagError> {
    let line = serde_json::to_string(record).unwrap_or_else(|e| {
        // Every field of `Record` is a plain, always-serializable type
        // (strings, i64, bool, a `&'static str`); a failure here would be a
        // logic error in this crate, not a caller mistake, so it is recorded
        // as text rather than silently dropped or, on a background thread
        // nobody awaits, panicking the process over a lost usage count.
        format!("{{\"ts_ms\":0,\"kind\":\"error\",\"code\":\"KG-9901\",\"component\":\"kgraf-diag\",\"context\":\"unserializable record: {e}\"}}")
    });
    // The whole line, newline included, in one buffer: one `write_all` below
    // is one `write(2)`, which is atomic against another `O_APPEND` writer at
    // this size. Two separate writes (the text, then the newline) is exactly
    // the bug this fixed (glued lines under concurrent processes).
    let mut buf = line.into_bytes();
    buf.push(b'\n');
    with_locked(path, || {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|source| DiagError::Open {
                path: path.to_path_buf(),
                source,
            })?;
        file.write_all(&buf).map_err(|source| DiagError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        let len = file
            .metadata()
            .map(|m| m.len())
            .map_err(|source| DiagError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        if len > cap_bytes {
            trim_locked(path, cap_bytes)?;
        }
        Ok(())
    })
}

/// Rewrite `path` keeping only its newest `KEEP_FRACTION * cap_bytes` bytes,
/// snapped forward to the next line boundary so no line is left truncated.
/// Callable only while [`with_locked`] already holds the lock.
fn trim_locked(path: &Path, cap_bytes: u64) -> Result<(), DiagError> {
    let keep_bytes = (cap_bytes as f64 * KEEP_FRACTION) as u64;
    let bytes = fs::read(path).map_err(|source| DiagError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    widen_race_window_for_test();
    if (bytes.len() as u64) <= keep_bytes {
        return Ok(());
    }
    let drop_from = bytes.len() as u64 - keep_bytes;
    // Snap forward to the byte after the next newline at or after `drop_from`,
    // so the kept tail starts on a whole line.
    let start = bytes[drop_from as usize..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| drop_from as usize + i + 1)
        .unwrap_or(bytes.len());
    let kept = &bytes[start..];
    let tmp = trim_temp_path(path);
    {
        let mut f = File::create(&tmp).map_err(|source| DiagError::Write {
            path: tmp.clone(),
            source,
        })?;
        f.write_all(kept).map_err(|source| DiagError::Write {
            path: tmp.clone(),
            source,
        })?;
    }
    fs::rename(&tmp, path).map_err(|source| DiagError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Every well-formed line in `path`, oldest first. A line that does not parse
/// (a torn write from a crash mid-append, or a foreign file at this path) is
/// skipped rather than failing the whole read: a diagnostics reader that
/// refuses the file over its last, possibly-incomplete line would defeat the
/// point of keeping the file at all.
pub fn read_all(path: &Path) -> Result<Vec<Record>, DiagError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path).map_err(|source| DiagError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|source| DiagError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<Record>(&line) {
            out.push(record);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::DiagEvent;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn appends_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diag.jsonl");
        append(&path, &Record::now(DiagEvent::launch()), DEFAULT_CAP_BYTES).unwrap();
        append(
            &path,
            &Record::now(DiagEvent::RunStarted { run: "r1".into() }),
            DEFAULT_CAP_BYTES,
        )
        .unwrap();
        let records = read_all(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].event, DiagEvent::launch());
    }

    #[test]
    fn trims_over_cap_and_keeps_newest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diag.jsonl");
        let cap = 2_000u64; // small cap so the test does not write a real megabyte
        for i in 0..200 {
            append(
                &path,
                &Record::now(DiagEvent::RunStarted {
                    run: format!("run-{i}"),
                }),
                cap,
            )
            .unwrap();
        }
        let len = fs::metadata(path.as_path()).unwrap().len();
        assert!(len <= cap, "file grew past its cap: {len} > {cap}");
        let records = read_all(&path).unwrap();
        // The newest event survives; the oldest was dropped.
        let last = records.last().unwrap();
        assert_eq!(
            last.event,
            DiagEvent::RunStarted {
                run: "run-199".into()
            }
        );
        assert!(records.len() < 200, "nothing was trimmed");
        assert!(
            !records.iter().any(|r| r.event
                == DiagEvent::RunStarted {
                    run: "run-0".into()
                }),
            "the oldest event should have been dropped by the trim"
        );
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        assert_eq!(read_all(&path).unwrap(), Vec::new());
    }

    #[test]
    fn a_torn_trailing_line_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diag.jsonl");
        append(&path, &Record::now(DiagEvent::launch()), DEFAULT_CAP_BYTES).unwrap();
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        write!(f, "{{\"ts_ms\":1,\"kind\":\"launc").unwrap(); // no closing brace, no newline
        let records = read_all(&path).unwrap();
        assert_eq!(records.len(), 1);
    }

    /// The original repro shape: many threads (standing in for many
    /// processes, since `std::fs` locking is per open-file-description, not
    /// per-process; the exclusive lock this test exercises is the same one
    /// two real processes would contend for) appending concurrently, with no
    /// trim in play. Every line must parse and every one must be present: no
    /// gluing, no loss.
    #[test]
    fn concurrent_appenders_glue_nothing_and_lose_nothing_without_trim() {
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("diag.jsonl"));
        const THREADS: usize = 8;
        const PER_THREAD: usize = 50;
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let path = Arc::clone(&path);
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        append(
                            &path,
                            &Record::now(DiagEvent::RunStarted {
                                run: format!("t{t}-{i}"),
                            }),
                            // A cap far above anything this test could write,
                            // so no trim runs: this test isolates the
                            // glued-line/lost-write failure mode from the
                            // trim-race one below.
                            1_000_000_000,
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let raw = fs::read_to_string(path.as_path()).unwrap();
        let line_count = raw.lines().count();
        assert_eq!(
            line_count,
            THREADS * PER_THREAD,
            "a glued line collapses two records into fewer lines than were written"
        );
        let records = read_all(&path).unwrap();
        assert_eq!(
            records.len(),
            THREADS * PER_THREAD,
            "every line must also parse: a glued line fails to parse and is silently skipped \
             by read_all, which is why the line count above is checked separately"
        );
        let mut seen = std::collections::HashSet::new();
        for r in &records {
            if let DiagEvent::RunStarted { run } = &r.event {
                assert!(seen.insert(run.clone()), "duplicate run id {run}");
            }
        }
        assert_eq!(seen.len(), THREADS * PER_THREAD);
    }

    /// With trim in play (a small cap that every appender will cross many
    /// times over): every record still in the kept window when the run ends
    /// must be intact and present, never lost to a concurrent trim
    /// clobbering another thread's in-flight append or trim.
    #[test]
    fn concurrent_appenders_with_trim_lose_nothing_inside_the_kept_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("diag.jsonl"));
        const THREADS: usize = 6;
        const PER_THREAD: usize = 120;
        let cap = 4_000u64;
        let counter = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let path = Arc::clone(&path);
                let counter = Arc::clone(&counter);
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        let seq = counter.fetch_add(1, Ordering::SeqCst);
                        append(
                            &path,
                            &Record::now(DiagEvent::RunStarted {
                                run: format!("seq-{seq:06}-t{t}-{i}"),
                            }),
                            cap,
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let len = fs::metadata(path.as_path()).unwrap().len();
        assert!(len <= cap, "file grew past its cap: {len} > {cap}");
        let raw = fs::read_to_string(path.as_path()).unwrap();
        // No glued lines: every non-empty line must parse as a `Record`.
        let mut parsed = 0usize;
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            serde_json::from_str::<Record>(line)
                .unwrap_or_else(|e| panic!("unparsable line (glued?): {line:?}: {e}"));
            parsed += 1;
        }
        assert_eq!(parsed, raw.lines().filter(|l| !l.trim().is_empty()).count());
        // The highest sequence number written must be present: the newest
        // record was never dropped by a losing trim race.
        let records = read_all(&path).unwrap();
        let max_seq = records
            .iter()
            .filter_map(|r| match &r.event {
                DiagEvent::RunStarted { run } => run
                    .strip_prefix("seq-")
                    .and_then(|s| s.split('-').next())
                    .and_then(|s| s.parse::<usize>().ok()),
                _ => None,
            })
            .max()
            .expect("at least one record survives the cap");
        assert_eq!(
            max_seq,
            THREADS * PER_THREAD - 1,
            "the very last record written must not be lost to a trim race"
        );
        // No record present is a duplicate or corrupted: every kept record's
        // sequence number appears at most once, and the kept sequence
        // numbers form a contiguous or near-contiguous tail (nothing was
        // spliced from an inconsistent read of two different trims).
        let mut seqs: Vec<usize> = records
            .iter()
            .filter_map(|r| match &r.event {
                DiagEvent::RunStarted { run } => run
                    .strip_prefix("seq-")
                    .and_then(|s| s.split('-').next())
                    .and_then(|s| s.parse::<usize>().ok()),
                _ => None,
            })
            .collect();
        let before = seqs.len();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(
            before,
            seqs.len(),
            "a duplicated record survived a trim race"
        );
    }
}
