use super::*;

/// Run on a thread so a runner that hangs fails the test instead of the
/// suite.
fn bounded<T: Send + 'static>(bound: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(bound)
        .expect("the runner returned within the bound")
}

#[cfg(unix)]
fn sh(script: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script]);
    cmd
}

#[cfg(windows)]
fn sh(script: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.args(["/d", "/c", script]);
    cmd
}

#[test]
fn output_status_and_input_are_carried() {
    #[cfg(unix)]
    let script = "cat; echo err >&2; exit 3";
    #[cfg(windows)]
    let script = "findstr x & echo err 1>&2 & exit /b 3";
    let got = bounded(Duration::from_secs(60), move || {
        run(
            sh(script),
            "probe",
            Some(b"x1\nx2\n"),
            Limits::new(Duration::from_secs(30)),
        )
    })
    .expect("ran");
    assert_eq!(got.status.code(), Some(3));
    assert_eq!(
        String::from_utf8_lossy(&got.stdout).replace('\r', ""),
        "x1\nx2\n"
    );
    assert_eq!(String::from_utf8_lossy(&got.stderr).trim(), "err");
    assert!(!got.pipes_held);
}

/// Output and input far larger than a pipe's buffer: neither side stalls.
#[cfg(unix)]
#[test]
fn large_input_and_output_do_not_deadlock() {
    let input = vec![b'a'; 4 << 20];
    let got = bounded(Duration::from_secs(60), move || {
        run(
            sh("cat; head -c 3000000 /dev/zero >&2"),
            "probe",
            Some(&input),
            Limits::new(Duration::from_secs(30)),
        )
    })
    .expect("ran");
    assert!(got.status.success());
    assert_eq!(got.stdout.len(), 4 << 20);
    assert_eq!(got.stderr.len(), 3_000_000);
}

/// At the deadline the whole tree is killed, the leader and what it
/// started, and the error names the command.
#[test]
fn a_command_past_its_deadline_is_killed_with_its_tree() {
    #[cfg(unix)]
    let script = "sleep 60 & sleep 60";
    #[cfg(windows)]
    let script = "ping -n 60 127.0.0.1 >NUL";
    let started = Instant::now();
    let err = bounded(Duration::from_secs(30), move || {
        run(
            sh(script),
            "probe sleeper",
            None,
            Limits::new(Duration::from_millis(300)),
        )
    })
    .expect_err("timed out");
    assert!(
        matches!(&err, RunError::TimedOut { command, .. } if command == "probe sleeper"),
        "{err}"
    );
    assert!(started.elapsed() < Duration::from_secs(15));
}

/// A process the command left behind in its tree is ended when the
/// command exits, so its copy of the output pipe cannot hold the read.
#[test]
fn what_the_command_leaves_in_its_tree_is_ended_at_its_exit() {
    #[cfg(unix)]
    let script = "sleep 60 & echo done";
    #[cfg(windows)]
    let script = "start /b ping -n 60 127.0.0.1 >NUL & echo done";
    let started = Instant::now();
    let got = bounded(Duration::from_secs(30), move || {
        run(
            sh(script),
            "probe",
            None,
            Limits::new(Duration::from_secs(30)),
        )
    })
    .expect("ran");
    assert!(got.status.success());
    assert_eq!(String::from_utf8_lossy(&got.stdout).trim(), "done");
    assert!(!got.pipes_held, "the leftover was killed, the pipe closed");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

/// A process outside the tree (here a new session) holding the output
/// pipe: the output is returned after the drain grace, marked and
/// recorded, instead of waiting for that process.
#[cfg(unix)]
#[test]
fn a_pipe_held_outside_the_tree_ends_after_the_grace() {
    let dir = tempfile::tempdir().expect("tmp");
    let pidfile = dir.path().join("pid");
    let script = format!(
        "perl -MPOSIX -e 'POSIX::setsid(); open(F, \">\", \"{}\"); print F $$; close F; sleep 60' & \
         while [ ! -s '{}' ]; do sleep 0.01; done; echo done",
        pidfile.display(),
        pidfile.display()
    );
    let before = crate::waits::pipes_held_count();
    let mut limits = Limits::new(Duration::from_secs(30));
    limits.drain_grace = Duration::from_millis(500);
    let got = bounded(Duration::from_secs(20), move || {
        run(sh(&script), "probe held", None, limits)
    });
    if let Ok(pid) = std::fs::read_to_string(&pidfile) {
        // SAFETY: signals only the test's own detached sleeper.
        unsafe {
            libc::kill(pid.trim().parse().unwrap_or(0), libc::SIGKILL);
        }
    }
    let got = got.expect("ran");
    assert!(got.status.success());
    assert_eq!(String::from_utf8_lossy(&got.stdout).trim(), "done");
    assert!(got.pipes_held);
    assert!(crate::waits::pipes_held_count() > before);
}

/// The runner's cost next to `Command::output`, for one short git command:
/// both run alternately, `ROUNDS` times each, and the medians and 90th
/// percentiles are printed, with the cost of one entry in the record of
/// running children. Ignored by default (timing, not correctness):
///
/// `cargo test -p kgraf-core --release --lib child_run::tests::overhead_next_to_output -- --ignored --nocapture`
#[test]
#[ignore = "a measurement; run by hand"]
fn overhead_next_to_output() {
    const ROUNDS: usize = 300;
    let git = || {
        let mut cmd = Command::new("git");
        cmd.arg("--version");
        cmd
    };
    let mut std_us = Vec::with_capacity(ROUNDS);
    let mut run_us = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t = Instant::now();
        let out = git().stdin(Stdio::null()).output().expect("git runs");
        assert!(out.status.success());
        std_us.push(t.elapsed().as_micros());
        let t = Instant::now();
        let got = run(
            git(),
            "git --version",
            None,
            Limits::new(Duration::from_secs(60)),
        )
        .expect("git runs");
        assert!(got.status.success());
        run_us.push(t.elapsed().as_micros());
    }
    let stats = |v: &mut Vec<u128>| {
        v.sort_unstable();
        (v[v.len() / 2], v[v.len() * 9 / 10])
    };
    let (s50, s90) = stats(&mut std_us);
    let (r50, r90) = stats(&mut run_us);
    println!("git --version x{ROUNDS}: Command::output median {s50} us, p90 {s90} us");
    println!("git --version x{ROUNDS}: child_run::run median {r50} us, p90 {r90} us");
    let t = Instant::now();
    for i in 0..100_000u32 {
        drop(crate::waits::child_running(i, "git --version"));
    }
    println!(
        "record entry added and removed: {} ns each",
        t.elapsed().as_nanos() / 100_000
    );
}

#[test]
fn a_missing_program_is_a_spawn_error() {
    let err = run(
        Command::new("kgraf-no-such-program-anywhere"),
        "nothing",
        None,
        Limits::new(Duration::from_secs(5)),
    )
    .expect_err("missing");
    assert!(matches!(err, RunError::Spawn { .. }), "{err}");
}
