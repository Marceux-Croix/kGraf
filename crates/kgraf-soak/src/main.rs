//! `kgraf-soak`: runs many orchestrations back to back on ONE `Host` in one process, the way the
//! desktop app stays open for days, and prints one JSON line per run with what the process holds:
//! memory, open file descriptors, threads, child processes, worktrees on disk, temporary
//! directories, the host's per-run tables, and the size of the project's state directory.
//!
//! ```text
//! kgraf-soak --repo DIR --work DIR [--runs N]
//! ```
//!
//! Every run goes through the library path the desktop app uses: `Host::prepare`,
//! `PreparedRun::into_orchestrator`, `Orchestrator::start`, then `Host::finish_run`. A stub OpenAI-compatible server (this binary, started as a child process
//! with the hidden `--serve-stub`) plans a fixed two-contract plan, writes one file per
//! contract through `apply_patch`, and finishes. No real model is called and nothing leaves the
//! loopback interface. The driver points `HOME`, the temporary directory and the stub's address
//! at directories under `--work`, so nothing outside `--repo` and `--work` is read or written.
//!
//! Line `"run": 0` is the baseline taken before the first run. A later line has the keys:
//! `run`, `status`, `verified`, `wall_ms`, `mem_bytes`, `mem_metric`, `rss_bytes`, `fds`,
//! `threads`, `children` (the stub excluded), `worktree_dirs`, `git_worktrees`, `tmp_entries`,
//! `git_refs`, `state_bytes`, `retained` (see `Host::retained`).
//!
//! Sampling happens after a settle pause, and after the allocator hands its free pages back to the
//! OS (`sample::release_free_memory`), so memory readings count memory in use.
//! The settle pause also lets finished tasks end. The runtime's idle blocking threads are kept for a
//! quarter second only (the default is ten seconds), so a thread count that stays up after the
//! pause is a thread that is stuck, not one that is idle.
//!
//! Exit status: 0 when every run completed, 1 on a setup error, 2 when a run did not complete.

mod sample;
mod stub;

use kgraf_core::RunId;
use kgraf_host::{Host, RunConfig};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DEFAULT_RUNS: u32 = 30;
/// Pause before each sample: lets finished tasks release their sockets and threads.
const SETTLE: Duration = Duration::from_millis(1000);

struct Args {
    repo: PathBuf,
    work: PathBuf,
    runs: u32,
}

fn parse_args(mut argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let (mut repo, mut work, mut runs) = (None, None, DEFAULT_RUNS);
    while let Some(flag) = argv.next() {
        let mut value = |name: &str| argv.next().ok_or(format!("{name} needs a value"));
        match flag.as_str() {
            "--repo" => repo = Some(PathBuf::from(value("--repo")?)),
            "--work" => work = Some(PathBuf::from(value("--work")?)),
            "--runs" => {
                runs = value("--runs")?
                    .parse()
                    .map_err(|_| "--runs needs a number".to_owned())?
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        repo: repo.ok_or("--repo DIR is required")?,
        work: work.ok_or("--work DIR is required")?,
        runs,
    })
}

fn main() {
    let mut argv = std::env::args().skip(1).peekable();
    if argv.peek().map(String::as_str) == Some("--serve-stub") {
        if let Err(e) = stub::serve() {
            eprintln!("kgraf-soak: stub failed: {e}");
            std::process::exit(1);
        }
        return;
    }
    let code = match parse_args(argv).map_err(|e| e.to_string()).and_then(soak) {
        Ok(true) => 0,
        Ok(false) => 2,
        Err(e) => {
            eprintln!("kgraf-soak: {e}");
            1
        }
    };
    std::process::exit(code);
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=kgraf-soak",
            "-c",
            "user.email=kgraf-soak@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A repository with one commit; an existing one is used as it is.
fn ensure_repo(repo: &Path) -> Result<(), String> {
    std::fs::create_dir_all(repo).map_err(|e| format!("cannot create {}: {e}", repo.display()))?;
    if repo.join(".git").exists() {
        return Ok(());
    }
    git(repo, &["init", "-q", "-b", "main"])?;
    // Merge-back commits need an identity in the repository's own configuration.
    git(repo, &["config", "user.name", "kgraf-soak"])?;
    git(repo, &["config", "user.email", "kgraf-soak@localhost"])?;
    std::fs::write(repo.join("README.md"), "soak scratch repository\n")
        .map_err(|e| format!("cannot write README.md: {e}"))?;
    git(repo, &["add", "-A"])?;
    git(repo, &["commit", "-q", "-m", "soak fixture"])?;
    Ok(())
}

/// The stub as a child process, and the port it announced. Closing the returned stdin handle
/// ends it.
fn start_stub() -> Result<(std::process::Child, u16), String> {
    let exe = std::env::current_exe().map_err(|e| format!("no executable path: {e}"))?;
    let mut child = Command::new(exe)
        .arg("--serve-stub")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("cannot start the stub: {e}"))?;
    let mut line = String::new();
    let stdout = child.stdout.take().ok_or("no stub stdout")?;
    std::io::BufRead::read_line(&mut std::io::BufReader::new(stdout), &mut line)
        .map_err(|e| format!("cannot read the stub's port: {e}"))?;
    let port = line
        .trim()
        .strip_prefix("PORT ")
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| format!("the stub said {line:?}, not a port"))?;
    Ok((child, port))
}

fn dir_entries(dir: &Path) -> u64 {
    std::fs::read_dir(dir).map_or(0, |d| d.count() as u64)
}

fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

struct Probe<'a> {
    host: &'a Host,
    repo: &'a Path,
    tmp: &'a Path,
    stub_pid: u32,
}

impl Probe<'_> {
    fn line(&self, run: u32, extra: serde_json::Value) -> Result<serde_json::Value, String> {
        std::thread::sleep(SETTLE);
        sample::release_free_memory();
        let s = sample::sample(Some(self.stub_pid))
            .ok_or("this platform has no process sampling (Linux and macOS only)")?;
        let git_worktrees = git(self.repo, &["worktree", "list", "--porcelain"])?
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count()
            .saturating_sub(1);
        let git_refs = git(self.repo, &["for-each-ref"])?.lines().count();
        let mut line = serde_json::json!({
            "run": run,
            "mem_bytes": s.mem_bytes,
            "mem_metric": s.mem_metric,
            "rss_bytes": s.rss_bytes,
            "fds": s.fds,
            "threads": s.threads,
            "children": s.children,
            "worktree_dirs": dir_entries(&self.host.state_dir().join("worktrees")),
            "git_worktrees": git_worktrees,
            "tmp_entries": dir_entries(self.tmp),
            "git_refs": git_refs,
            "state_bytes": dir_bytes(self.host.state_dir()),
            "retained": self.host.retained(),
        });
        if let (Some(into), Some(from)) = (line.as_object_mut(), extra.as_object()) {
            into.extend(from.clone());
        }
        Ok(line)
    }
}

/// Runs the soak. `Ok(true)` when every run completed.
fn soak(args: Args) -> Result<bool, String> {
    ensure_repo(&args.repo)?;
    let home = args.work.join("home");
    let tmp = args.work.join("tmp");
    for d in [&home, &tmp] {
        std::fs::create_dir_all(d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
    }
    let prices = args.work.join("prices.json");
    std::fs::write(&prices, "{}").map_err(|e| format!("cannot write the price table: {e}"))?;
    let permissions = args.work.join("permissions.toml");
    std::fs::write(&permissions, "mode = \"manual\"\n")
        .map_err(|e| format!("cannot write the permissions file: {e}"))?;

    let (mut stub, port) = start_stub()?;
    let _stdin = stub.stdin.take(); // dropped last: closing it ends the stub
    let stub_pid = stub.id();
    // SAFETY: no other thread exists yet (the runtime is built below), so nothing reads the
    // environment while it changes.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &home);
        std::env::set_var("XDG_DATA_HOME", &home);
        std::env::set_var("TMPDIR", &tmp);
        std::env::set_var("KGRAF_DIAG_DISABLE", "1");
        std::env::set_var("KGRAF_PERMISSIONS_PATH", &permissions);
        std::env::set_var(
            "KGRAF_OLLAMA_BASE_URL",
            format!("http://127.0.0.1:{port}/v1"),
        );
        std::env::remove_var("KGRAF_CONTAINER_SANDBOX");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_keep_alive(Duration::from_millis(250))
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
    let mut config = RunConfig::new(&args.repo, vec!["ollama:stub-model".into()], &prices);
    config.verify = "ls soak".into();
    config.gate_commands = vec!["true".into()];
    let host = Host::open(config).map_err(|e| format!("cannot open the host: {e}"))?;
    let probe = Probe {
        host: &host,
        repo: &args.repo,
        tmp: &tmp,
        stub_pid,
    };
    let emit = |v: serde_json::Value| println!("{v}");
    emit(probe.line(0, serde_json::json!({"status": "baseline"}))?);

    let mut all_completed = true;
    for n in 1..=args.runs {
        let started = Instant::now();
        let run = RunId::new(format!("soak-{n}"));
        // The desktop's path: build the run, hand its orchestrator to the caller, start it,
        // then `finish_run` on every outcome (it releases the host's per-run tables and
        // sweeps the attempt refs).
        let outcome = runtime.block_on(async {
            let prepared = host.prepare(run.clone(), None, &mut |_| {}).await?;
            let orchestrator = prepared.into_orchestrator(&host);
            Ok::<_, kgraf_host::Error>(
                orchestrator
                    .start(
                        run.clone(),
                        "Create soak/a.txt and soak/b.txt, one line each.".into(),
                    )
                    .await,
            )
        });
        let housekeeping = host.finish_run(&run);
        let wall_ms = started.elapsed().as_millis() as u64;
        let (status, verified) = match &outcome {
            Ok(Ok(report)) => (format!("{:?}", report.status), report.verified.len()),
            Ok(Err(e)) => (format!("error: {e}"), 0),
            Err(e) => (format!("error: {e}"), 0),
        };
        drop((outcome, housekeeping));
        let completed = status == "Completed" && verified == 2;
        emit(probe.line(
            n,
            serde_json::json!({"status": status, "verified": verified, "wall_ms": wall_ms}),
        )?);
        if !completed {
            eprintln!("kgraf-soak: run {n} ended `{status}` with {verified} verified; stopping");
            all_completed = false;
            break;
        }
    }
    drop(host);
    runtime.shutdown_timeout(Duration::from_secs(5));
    let _ = stub.kill();
    let _ = stub.wait();
    Ok(all_completed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> impl Iterator<Item = String> {
        v.iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn arguments_parse_and_refuse() {
        let a = parse_args(args(&["--repo", "r", "--work", "w", "--runs", "3"])).expect("ok");
        assert_eq!((a.repo, a.work, a.runs), ("r".into(), "w".into(), 3));
        assert_eq!(
            parse_args(args(&["--repo", "r", "--work", "w"]))
                .expect("ok")
                .runs,
            DEFAULT_RUNS
        );
        assert!(
            parse_args(args(&["--repo", "r"])).is_err(),
            "--work is required"
        );
        assert!(parse_args(args(&["--repo", "r", "--work", "w", "--runs", "x"])).is_err());
        assert!(parse_args(args(&["--bogus"])).is_err());
        assert!(
            parse_args(args(&["--repo"])).is_err(),
            "a flag with no value"
        );
    }
}
