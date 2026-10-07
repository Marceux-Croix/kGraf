//! Runs one child process to its end, a time limit, or a cancel, handing
//! each output line to a callback as it arrives.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Lines kept in memory per process; the report needs the tail only.
const KEEP_LINES: usize = 400;

#[derive(Debug, Default)]
pub struct Finished {
    pub code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub secs: f64,
    /// The last [`KEEP_LINES`] lines of stdout and stderr, interleaved as read.
    pub tail: Vec<String>,
    pub start_error: Option<String>,
}

impl Finished {
    pub fn tail_text(&self, n: usize) -> String {
        crate::libtest::tail_lines(&self.tail, n)
    }
}

fn pump(r: impl Read + Send + 'static, tx: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(r);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf).into_owned();
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// Run `cmd` with stdin closed. On a time limit or `cancel`, the child's
/// whole process tree is ended (`kill_tree`).
pub fn run(
    mut cmd: Command,
    limit: Duration,
    cancel: &AtomicBool,
    mut on_line: impl FnMut(&str),
) -> Finished {
    let started = Instant::now();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Finished {
                start_error: Some(e.to_string()),
                ..Finished::default()
            };
        }
    };
    let (tx, rx) = mpsc::channel();
    if let Some(out) = child.stdout.take() {
        pump(out, tx.clone());
    }
    if let Some(err) = child.stderr.take() {
        pump(err, tx.clone());
    }
    drop(tx);
    let mut f = Finished::default();
    let mut keep = |line: String, f: &mut Finished| {
        on_line(&line);
        f.tail.push(line.trim_end_matches(['\r', '\n']).to_owned());
        if f.tail.len() > KEEP_LINES * 2 {
            f.tail.drain(..KEEP_LINES);
        }
    };
    let status = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => keep(line, &mut f),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Both pipes closed: the child has ended or is about to.
                break child.wait().ok();
            }
        }
        if let Ok(Some(s)) = child.try_wait() {
            // Drain what the readers still hold, briefly.
            let until = Instant::now() + Duration::from_secs(2);
            while let Ok(line) = rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                keep(line, &mut f);
            }
            break Some(s);
        }
        let over = started.elapsed() > limit;
        let stop = cancel.load(Ordering::Relaxed);
        if over || stop {
            f.timed_out = over;
            f.cancelled = stop;
            kill_tree(&mut child);
            let until = Instant::now() + Duration::from_secs(2);
            while let Ok(line) = rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                keep(line, &mut f);
            }
            break child.wait().ok();
        }
    };
    f.code = status.and_then(|s| s.code());
    f.secs = started.elapsed().as_secs_f64();
    f
}

/// End the child and everything it started. On Windows `taskkill /T` walks
/// the tree; a sandboxed grandchild is also in kGraf's own Job Object, which
/// ends it when the test process's handle closes.
pub fn kill_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// Run a short read-only command and return its stdout, or `None`.
pub fn capture(program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    let f = run(
        cmd,
        Duration::from_secs(30),
        &AtomicBool::new(false),
        |_| {},
    );
    (f.start_error.is_none() && f.code == Some(0)).then(|| f.tail.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn lines_arrive_and_a_time_limit_ends_the_child() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo one; echo two >&2; sleep 30"]);
        let mut seen = Vec::new();
        let f = run(
            cmd,
            Duration::from_millis(500),
            &AtomicBool::new(false),
            |l| {
                seen.push(l.trim().to_owned());
            },
        );
        assert!(f.timed_out);
        assert!(f.secs < 10.0);
        seen.sort();
        assert_eq!(seen, vec!["one", "two"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_exit_code_comes_back() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo x; exit 3"]);
        let f = run(
            cmd,
            Duration::from_secs(10),
            &AtomicBool::new(false),
            |_| {},
        );
        assert_eq!(f.code, Some(3));
        assert_eq!(f.tail, vec!["x"]);
        assert!(!f.timed_out);
    }

    #[test]
    fn a_missing_program_is_a_start_error() {
        let f = run(
            Command::new("kgraf-wintest-no-such-program"),
            Duration::from_secs(5),
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(f.start_error.is_some());
    }
}
