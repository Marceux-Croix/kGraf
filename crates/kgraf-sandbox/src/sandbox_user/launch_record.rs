//! What the Windows launch path did in this process, for a person reading a
//! test run: each window station and desktop it made, the jobserver
//! semaphores it made for the commands' cargo and rustc, how long each
//! `CreateProcessWithLogonW` call took, how long it waited for the spawn lock
//! first, and how the spawn lock was used by every spawn in the process.
//! `kgraf sandbox selfcheck` prints [`lines`], so a Windows run collects them
//! with its output. Platform-neutral, so the text is tested on every host.
//!
//! The logon call runs under the process-wide spawn lock
//! (`kgraf_core::child_run::spawn_lock`) for its whole length: the standard
//! handles it passes must stay inheritable until the secondary logon service
//! has copied them, at a point inside the call the documentation does not
//! name, and the call copies the caller's inheritable handles, so no other
//! spawn may run beside it. Every other spawn waits for it. The durations
//! recorded here say whether that wait matters.
//!
//! Kept in memory only and never sent anywhere; the record holds window
//! station names, SIDs and durations, no path and no secret.

use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// At most this many station lines are kept: one or two per sandbox user and
/// process, so a full record means a defect, and the newest lines are the
/// ones dropped.
const MAX_STATION_LINES: usize = 64;

/// Durations of one kind of call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Timings {
    count: u64,
    total: Duration,
    max: Duration,
    last: Duration,
}

impl Timings {
    const ZERO: Self = Self {
        count: 0,
        total: Duration::ZERO,
        max: Duration::ZERO,
        last: Duration::ZERO,
    };

    fn add(&mut self, d: Duration) {
        self.count += 1;
        self.total += d;
        self.max = self.max.max(d);
        self.last = d;
    }

    fn mean(&self) -> Duration {
        match u32::try_from(self.count) {
            Ok(0) | Err(_) => Duration::ZERO,
            Ok(n) => self.total / n,
        }
    }
}

struct Record {
    stations: Vec<String>,
    /// The first jobserver semaphore made, and how many were made.
    jobserver: Option<String>,
    jobservers: u64,
    /// Each `CreateProcessWithLogonW` call, from its start to its return.
    logon: Timings,
    /// How many of those calls failed.
    logon_failed: u64,
    /// The wait for the spawn lock (and the marking of the standard handles)
    /// before each logon call.
    lock_wait: Timings,
}

static RECORD: Mutex<Record> = Mutex::new(Record {
    stations: Vec::new(),
    jobserver: None,
    jobservers: 0,
    logon: Timings::ZERO,
    logon_failed: 0,
    lock_wait: Timings::ZERO,
});

fn record() -> std::sync::MutexGuard<'static, Record> {
    // Plain data: a panic elsewhere leaves it consistent.
    RECORD.lock().unwrap_or_else(|p| p.into_inner())
}

/// Seconds since the Unix epoch, for ordering lines against other logs.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Record that this process made the window station `station` with the
/// desktop `desktop` for the `role` (the runner or the command) of the
/// sandbox user `user_sid`, admitting `admitted`.
pub fn note_station(role: &str, station: &str, desktop: &str, user_sid: &str, admitted: &str) {
    let line = station_line(now(), role, station, desktop, user_sid, admitted);
    let mut r = record();
    if r.stations.len() < MAX_STATION_LINES {
        r.stations.push(line);
    }
}

/// Record that this process made the jobserver semaphore `name` with `tokens`
/// tokens for a command of the sandbox user `user_sid`
/// (`sandbox_user::jobserver`). The first is kept whole; later ones are
/// counted.
pub fn note_jobserver(name: &str, tokens: i32, user_sid: &str) {
    let mut r = record();
    r.jobservers += 1;
    if r.jobserver.is_none() {
        r.jobserver = Some(jobserver_line(now(), name, tokens, user_sid));
    }
}

fn jobserver_line(at: u64, name: &str, tokens: i32, user_sid: &str) -> String {
    format!(
        "launch: made jobserver semaphore {name} with {tokens} tokens for a command of \
         sandbox user {user_sid}, named in CARGO_MAKEFLAGS (at {at})"
    )
}

/// Record one `CreateProcessWithLogonW` call: `lock_wait` before it (the
/// spawn lock and marking the standard handles), `call` from its start to its
/// return, and whether it started the runner.
pub fn note_logon(lock_wait: Duration, call: Duration, ok: bool) {
    let mut r = record();
    r.lock_wait.add(lock_wait);
    r.logon.add(call);
    if !ok {
        r.logon_failed += 1;
    }
}

fn station_line(
    at: u64,
    role: &str,
    station: &str,
    desktop: &str,
    user_sid: &str,
    admitted: &str,
) -> String {
    format!(
        "launch: made window station {station} and desktop {desktop} for the {role} of \
         sandbox user {user_sid}, admitting {admitted} (at {at})"
    )
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

fn logon_line(logon: &Timings, failed: u64, lock_wait: &Timings) -> Option<String> {
    (logon.count > 0).then(|| {
        format!(
            "launch: CreateProcessWithLogonW calls {} ({failed} failed): last {}, mean {}, \
             longest {}; spawn lock wait before them: mean {}, longest {}",
            logon.count,
            ms(logon.last),
            ms(logon.mean()),
            ms(logon.max),
            ms(lock_wait.mean()),
            ms(lock_wait.max),
        )
    })
}

fn lock_line(s: &kgraf_core::child_run::SpawnLockStats) -> Option<String> {
    (s.taken > 0 || s.timeouts > 0).then(|| {
        let mean = u32::try_from(s.taken)
            .ok()
            .filter(|n| *n > 0)
            .map_or(Duration::ZERO, |n| s.waited_total / n);
        format!(
            "launch: spawn lock, every spawn in this process: taken {}, waits that ran out {}; \
             wait mean {}, longest {}; longest hold {}",
            s.taken,
            s.timeouts,
            ms(mean),
            ms(s.waited_max),
            ms(s.held_max),
        )
    })
}

/// Every recorded line: the stations made, oldest first, then the logon
/// calls and the spawn lock. Empty when nothing was launched as a sandbox
/// user and nothing took the spawn lock in this process.
pub fn lines() -> Vec<String> {
    let mut out = {
        let r = record();
        let mut out = r.stations.clone();
        out.extend(r.jobserver.clone());
        if r.jobservers > 1 {
            out.push(format!(
                "launch: jobserver semaphores made in this process: {}",
                r.jobservers
            ));
        }
        out.extend(logon_line(&r.logon, r.logon_failed, &r.lock_wait));
        out
    };
    out.extend(lock_line(&kgraf_core::child_run::spawn_lock_stats()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_station_line_names_the_station_the_desktop_the_role_and_the_user() {
        let line = station_line(
            1_700_000_000,
            "command",
            "kgraf-cmd-00ff",
            "Default",
            "S-1-5-21-1-2-3-1001",
            "SYSTEM, the owner and that user",
        );
        assert_eq!(
            line,
            "launch: made window station kgraf-cmd-00ff and desktop Default for the command \
             of sandbox user S-1-5-21-1-2-3-1001, admitting SYSTEM, the owner and that user \
             (at 1700000000)"
        );
    }

    #[test]
    fn noted_stations_are_listed_in_order() {
        note_station("runner", "kgraf-run-test-a", "Default", "S-1-5-21-9", "x");
        note_station("command", "kgraf-cmd-test-a", "Default", "S-1-5-21-9", "x");
        let all = lines();
        let run = all.iter().position(|l| l.contains("kgraf-run-test-a"));
        let cmd = all.iter().position(|l| l.contains("kgraf-cmd-test-a"));
        assert!(run.is_some() && cmd.is_some() && run < cmd, "{all:?}");
    }

    #[test]
    fn a_noted_jobserver_is_in_the_lines_with_its_name_tokens_and_user() {
        assert_eq!(
            jobserver_line(1_700_000_000, "kgraf-jobserver-00ff", 4, "S-1-5-21-9"),
            "launch: made jobserver semaphore kgraf-jobserver-00ff with 4 tokens for a command \
             of sandbox user S-1-5-21-9, named in CARGO_MAKEFLAGS (at 1700000000)"
        );
        note_jobserver("kgraf-jobserver-test", 2, "S-1-5-21-9");
        let all = lines();
        assert!(
            all.iter()
                .any(|l| l.starts_with("launch: made jobserver semaphore ")),
            "{all:?}"
        );
    }

    #[test]
    fn the_logon_line_gives_the_count_the_failures_and_each_duration() {
        let mut logon = Timings::ZERO;
        let mut wait = Timings::ZERO;
        assert_eq!(logon_line(&logon, 0, &wait), None, "no call, no line");
        for (w, c) in [(1, 900), (3, 2100), (2, 1200)] {
            wait.add(Duration::from_millis(w));
            logon.add(Duration::from_millis(c));
        }
        assert_eq!(
            logon_line(&logon, 1, &wait).unwrap(),
            "launch: CreateProcessWithLogonW calls 3 (1 failed): last 1200.0 ms, mean 1400.0 ms, \
             longest 2100.0 ms; spawn lock wait before them: mean 2.0 ms, longest 3.0 ms"
        );
    }

    #[test]
    fn a_noted_logon_call_is_in_the_lines() {
        note_logon(Duration::from_millis(4), Duration::from_millis(1500), true);
        let all = lines();
        assert!(
            all.iter()
                .any(|l| l.starts_with("launch: CreateProcessWithLogonW calls ")),
            "{all:?}"
        );
    }

    #[test]
    fn the_spawn_lock_line_reports_its_use() {
        let s = kgraf_core::child_run::SpawnLockStats {
            taken: 4,
            timeouts: 1,
            waited_total: Duration::from_millis(80),
            waited_max: Duration::from_millis(50),
            held_max: Duration::from_millis(2500),
        };
        assert_eq!(
            lock_line(&s).unwrap(),
            "launch: spawn lock, every spawn in this process: taken 4, waits that ran out 1; \
             wait mean 20.0 ms, longest 50.0 ms; longest hold 2500.0 ms"
        );
        assert_eq!(
            lock_line(&kgraf_core::child_run::SpawnLockStats::default()),
            None
        );
    }
}
