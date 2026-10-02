//! `kgraf orchestrate` stops honestly on SIGTERM and SIGINT.
//!
//! The mechanism under test is the process's: a real `kgraf orchestrate` child
//! runs against a stub provider, a worker's model call hangs, the child is sent
//! a signal, and what the child leaves behind is read from disk. A unit test of
//! the signal listener would show only that a listener exists; this shows that
//! the call is settled in the ledger, the report says stopped, the worktree is
//! named and the exit code is the signal's.
//!
//! Replicating it by hand: in a git repository with one commit, run
//! `kgraf orchestrate --instruction FILE --route ollama:stub-model`, with
//! `KGRAF_OLLAMA_BASE_URL` naming an OpenAI-compatible server that answers the first request
//! with a one-contract plan and never answers the second; when the second
//! request has arrived, `kill -TERM` the process.
//!
//! The stub listens on a free loopback port and the child is pointed at it with
//! `KGRAF_OLLAMA_BASE_URL`, so a real Ollama on the default port is never
//! reached.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The one-contract plan the stub answers the planner with.
const PLAN: &str = r#"{"contracts":[{"id":"c1","instruction":"write hello.txt containing hi","acceptance":"hello.txt exists","writes":[{"path":"hello.txt"}]}]}"#;

/// An OpenAI-compatible chat server: the first request is answered with
/// `PLAN`, every later one is read whole and never answered. `arrived` gets
/// one message per request read whole.
struct Stub {
    arrived: mpsc::Receiver<usize>,
}

fn read_request(stream: &mut TcpStream) -> Option<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
        let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
        let length = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if buffer.len() >= end + 4 + length {
            return Some(());
        }
    }
}

impl Stub {
    /// Requests from number `hang_from` on are never answered.
    fn start(listener: TcpListener, hang_from: usize) -> Self {
        let (tx, arrived) = mpsc::channel();
        let count = Arc::new(AtomicUsize::new(0));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let (tx, count) = (tx.clone(), count.clone());
                std::thread::spawn(move || {
                    while read_request(&mut stream).is_some() {
                        let n = count.fetch_add(1, Ordering::SeqCst) + 1;
                        let _ = tx.send(n);
                        if n >= hang_from {
                            // Hang: hold the socket open and read until the
                            // client goes away.
                            let mut sink = [0u8; 1024];
                            while matches!(stream.read(&mut sink), Ok(k) if k > 0) {}
                            return;
                        }
                        let body = serde_json::json!({
                            "id": "chatcmpl-stub",
                            "model": "stub-model",
                            "choices": [{
                                "index": 0,
                                "finish_reason": "stop",
                                "message": {"role": "assistant", "content": PLAN}
                            }],
                            "usage": {"prompt_tokens": 100, "completion_tokens": 50}
                        })
                        .to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        if stream.write_all(response.as_bytes()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Stub { arrived }
    }

    /// Wait until `n` requests have been read whole.
    fn wait_for(&self, n: usize, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.arrived.recv_timeout(left) {
                Ok(seen) if seen >= n => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

struct Ended {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Starts `kgraf orchestrate` on a fresh repository, waits for the worker's
/// call to hang, sends `signal`, and returns how the process ended.
fn interrupt_a_run(project: &Path, signal: &str, hang_from: usize) -> Ended {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let port = listener.local_addr().expect("address").port();
    let stub = Stub::start(listener, hang_from);

    git(project, &["init", "-q", "-b", "main"]);
    git(project, &["config", "user.name", "Test"]);
    git(project, &["config", "user.email", "test@example.invalid"]);
    git(project, &["config", "commit.gpgsign", "false"]);
    std::fs::write(project.join("README.md"), "stub\n").expect("write");
    git(project, &["add", "."]);
    git(project, &["commit", "-q", "-m", "init"]);
    let instruction = project.join("instruction.txt");
    std::fs::write(&instruction, "write hello.txt containing hi").expect("write");

    let home = tempfile::tempdir().expect("home");
    let permissions = home.path().join("permissions.toml");
    std::fs::write(&permissions, "mode = \"manual\"\n").expect("write");
    let prices = home.path().join("prices.json");
    std::fs::write(&prices, "{}").expect("write");
    let mut child = Command::new(env!("CARGO_BIN_EXE_kgraf"))
        .env("KGRAF_DIAG_DISABLE", "1")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("XDG_DATA_HOME", home.path())
        .env("KGRAF_PERMISSIONS_PATH", &permissions)
        .env(
            "KGRAF_OLLAMA_BASE_URL",
            format!("http://127.0.0.1:{port}/v1"),
        )
        .env_remove("KGRAF_CONTAINER_SANDBOX")
        .args(["-C"])
        .arg(project)
        .args(["orchestrate", "--instruction"])
        .arg(&instruction)
        .args(["--route", "ollama:stub-model", "--prices"])
        .arg(&prices)
        .args(["--gate-command", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kgraf");
    let mut out = child.stdout.take().expect("stdout");
    let mut err = child.stderr.take().expect("stderr");
    let stdout = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let stderr = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });

    // The planner call answered (1), then the worker's call arrived (2) and
    // hangs. Anything less means the run never reached the state under test.
    let started = Instant::now();
    while !stub.wait_for(hang_from, Duration::from_millis(200)) {
        let exited = child.try_wait().expect("wait");
        if exited.is_some() || started.elapsed() > Duration::from_secs(90) {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the worker's call never arrived (exited: {exited:?})\nstdout: {}\nstderr: {}",
                stdout.join().unwrap_or_default(),
                stderr.join().unwrap_or_default()
            );
        }
    }
    // The call is in the provider's hands; give the run a moment to be
    // inside the await, as a person pressing Ctrl-C would be.
    std::thread::sleep(Duration::from_millis(500));
    let sent = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(child.id().to_string())
        .status()
        .expect("kill");
    assert!(sent.success());

    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the run did not exit within 60 s of the signal\nstdout: {}",
                stdout.join().unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ended {
        code: status.code(),
        stdout: stdout.join().expect("stdout thread"),
        stderr: stderr.join().expect("stderr thread"),
    }
}

fn check_a_stopped_run(project: &Path, ended: &Ended, code: i32, name: &str, workers: bool) {
    let stdout = &ended.stdout;
    assert_eq!(
        ended.code,
        Some(code),
        "exit code\nstdout: {stdout}\nstderr: {}",
        ended.stderr
    );
    assert!(
        stdout.contains(&format!("stopping   {name} received")),
        "the stop is announced: {stdout}"
    );
    assert!(
        stdout.contains("status     Stopped"),
        "the report says stopped: {stdout}"
    );
    assert_eq!(
        stdout.contains("kept       worktree "),
        workers,
        "a worker's worktree is named exactly when a worker ran: {stdout}"
    );
    assert!(
        ended.stderr.contains("was interrupted by"),
        "the exit says why: {}",
        ended.stderr
    );
    assert!(
        !stdout.contains("cut short"),
        "cleanup was not cut short: {stdout}"
    );
    // Every worktree the output names is on disk.
    for line in stdout
        .lines()
        .filter(|l| l.starts_with("kept       worktree "))
    {
        let kept = line
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(path, _)| path.to_owned())
            .expect("a path in the kept line");
        assert!(Path::new(&kept).is_dir(), "{kept} is on disk");
    }

    // The ledger: the planner's call is counted, and the call that was in
    // flight when the signal came is settled, not left in flight.
    let ledger = kgraf_provider::Ledger::open(
        &kgraf_host::state_dir(project)
            .expect("state dir")
            .join("ledger.db"),
    )
    .expect("open ledger");
    let calls = ledger.calls().expect("calls");
    assert!(
        calls.len() >= if workers { 2 } else { 1 },
        "the calls made before the signal are in the ledger: {calls:?}"
    );
    for call in &calls {
        assert!(
            !ledger.is_in_flight(&call.id).expect("read"),
            "call {} is still in flight: {calls:?}",
            call.id
        );
    }
    assert!(
        calls.iter().any(|c| matches!(
            c.settlement,
            kgraf_provider::Settlement::Cancelled | kgraf_provider::Settlement::Unreconciled
        )),
        "the hanging call is settled as cancelled or may-have-billed: {calls:?}"
    );
}

#[test]
fn sigterm_stops_a_run_and_settles_what_was_in_flight() {
    let project = tempfile::tempdir().expect("project");
    let ended = interrupt_a_run(project.path(), "TERM", 2);
    check_a_stopped_run(project.path(), &ended, 143, "SIGTERM", true);
}

#[test]
fn sigint_stops_a_run_the_same_way() {
    let project = tempfile::tempdir().expect("project");
    let ended = interrupt_a_run(project.path(), "INT", 2);
    check_a_stopped_run(project.path(), &ended, 130, "SIGINT", true);
}

/// The planner's own call is the one in flight: no worker exists yet, and the
/// stop still ends the call and the run.
#[test]
fn a_signal_during_the_planner_call_stops_the_run() {
    let project = tempfile::tempdir().expect("project");
    let ended = interrupt_a_run(project.path(), "TERM", 1);
    check_a_stopped_run(project.path(), &ended, 143, "SIGTERM", false);
}
