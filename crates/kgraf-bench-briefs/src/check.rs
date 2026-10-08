//! Running a brief's [`Check`] against a checked-out attempt.
//!
//! A check ends in a [`Verdict`]. `Unsatisfied` carries the reason (a test
//! failed or never ran, a protected file changed, a script printed the wrong
//! thing, a doc file is missing a required fact, the tree carries a build
//! script) so the outcome can say why. `Err` is reserved for the check itself
//! being unable to run (the sandbox, `cargo` or `sh` could not be started, a
//! path could not be read), a harness problem, not a measurement. [`self_test`]
//! runs the whole pipeline on a known-good crate before any model is called,
//! so a broken toolchain or sandbox is a harness error and not a run of
//! failed briefs.
//!
//! **The attempt's code is untrusted.** `cargo test` runs the worker's tests
//! and any `build.rs` it wrote, and a script check runs the worker's shell
//! script. Every such process runs through `kgraf_sandbox`: network off
//! (open on Windows, whose sandbox cannot deny it, and an unsatisfied verdict
//! says so; decision 0011), writes limited to the attempt directory and a scratch directory, a cleared
//! environment (only what a build needs), a wall-clock timeout, and a kill of
//! the whole process group at the timeout.
//!
//! **What the runner controls.** Before a cargo check builds anything, the
//! runner refuses a tree that carries a build script, a `.cargo` directory or
//! a toolchain pin, puts its own copy of every protected file back, and after
//! the run confirms those files are unchanged. Mutant copies are made before
//! any of the attempt's code runs, and each mutant is built and run at the
//! attempt's own path and target directory, so a test cannot tell a mutant
//! run from the real one by where it runs.
//!
//! **How tests are counted.** The test executables are built once
//! (`cargo test --no-run`), each is asked for its tests with `--list`, and each
//! test then runs in its own process, judged by that process's exit status
//! alone (0 passed, 101 failed). Nothing a test writes to its standard output
//! is read, so a test cannot forge a report line or a summary. A test that
//! calls `exit(0)` passes only itself. Doc tests are not run. A test binary
//! with its own `main` (`harness = false`, `#![no_main]`) answers `--list`
//! itself and is not covered by this.

use crate::brief::{Brief, Check, DocFact, Mutation, ScriptCase};
use crate::gitutil;
use crate::structure;
use anyhow::{Context, Result, bail};
use kgraf_sandbox::{CheckNetwork, Redirect, SandboxPolicy, sandboxed_command};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The wall-clock limit for one check (a cold `cargo test` of a fixture crate
/// is well inside it).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// The most output kept from one check process. Cargo's build messages are a
/// few kilobytes; more than this is not a build report.
const OUTPUT_CAP: u64 = 1 << 20;

/// The most bytes of one attempt file a check reads. The files read are a
/// source file and a notes file; more than this is not one.
const MAX_ATTEMPT_FILE: u64 = 1 << 20;

/// The cargo target directory name under the scratch directory, the same for
/// the real run and every mutant run.
const TARGET: &str = "target";

/// Where and how long the check's processes run.
pub struct CheckEnv<'a> {
    /// A directory the check may write (build output, TMPDIR, the mutation
    /// copy); it must exist and lie outside the attempt directory.
    pub scratch: &'a Path,
    /// The wall-clock limit for one check (a build and its tests share it).
    pub timeout: Duration,
}

/// One checked-out attempt.
pub struct Attempt<'a> {
    /// The attempt's files, extracted from its commit.
    pub dir: &'a Path,
    /// The repository holding the fixture commit and the attempt's commit.
    pub repo: &'a Path,
    /// The fixture commit (the state the worker started from).
    pub base: &'a str,
    /// The attempt's commit.
    pub commit: &'a str,
}

/// What a check that ran decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Satisfied,
    /// The brief was not satisfied, and why.
    Unsatisfied(String),
}

impl Verdict {
    pub fn is_satisfied(&self) -> bool {
        matches!(self, Verdict::Satisfied)
    }

    /// The reason, when unsatisfied.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Verdict::Satisfied => None,
            Verdict::Unsatisfied(why) => Some(why),
        }
    }
}

/// A step that either went on or ended the check unsatisfied, with the reason.
type Judged<T> = std::result::Result<T, String>;

fn unmet<T>(reason: impl Into<String>) -> Result<Judged<T>> {
    Ok(Err(reason.into()))
}

/// Decide whether `attempt` satisfies `brief`: no protected path changed, the
/// brief's check holds, and (for a named test) the test fails on every
/// mutated copy of the fixture's function.
pub fn verify(brief: &Brief, attempt: &Attempt<'_>, env: &CheckEnv<'_>) -> Result<Verdict> {
    verify_on(brief, attempt, env).map(|v| with_network_notice(v, CheckNetwork::current()))
}

/// An unsatisfied verdict reached with the network open says so: the
/// attempt's code ran with the network open (decision 0011, amended
/// 2026-10-06). A satisfied verdict carries no text; the notice is in the
/// platform's strength and the run-start list instead.
fn with_network_notice(verdict: Verdict, network: CheckNetwork) -> Verdict {
    match (verdict, network.notice()) {
        (Verdict::Unsatisfied(why), Some(notice)) => {
            Verdict::Unsatisfied(format!("{why} [sandbox] {notice}"))
        }
        (verdict, _) => verdict,
    }
}

fn verify_on(brief: &Brief, attempt: &Attempt<'_>, env: &CheckEnv<'_>) -> Result<Verdict> {
    let touched =
        gitutil::changed_protected(attempt.repo, attempt.base, attempt.commit, brief.protected)?;
    if !touched.is_empty() {
        return Ok(Verdict::Unsatisfied(format!(
            "the attempt changed protected paths: {touched:?}"
        )));
    }
    // The commit matches the fixture on every protected path. The check runs
    // the attempt's code, which can rewrite files at check time, so the
    // runner puts its own copy of each protected file back before the build
    // and confirms it is still there after (see [`Guard`]).
    let guard = Guard {
        files: gitutil::files_at(attempt.repo, attempt.base, brief.protected)?,
    };
    guard.restore(attempt.dir)?;
    run_guarded(&brief.check, attempt.dir, env, &guard)
}

/// The runner's own copy of the protected files, read from the repository at
/// the fixture commit.
struct Guard {
    files: Vec<(String, Vec<u8>)>,
}

impl Guard {
    fn none() -> Self {
        Self { files: Vec::new() }
    }

    /// Write every protected file into `dir` as the fixture has it (a link
    /// there is replaced by the file).
    fn restore(&self, dir: &Path) -> Result<()> {
        for (rel, bytes) in &self.files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
            if path
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                std::fs::remove_file(&path)
                    .with_context(|| format!("cannot replace link {}", path.display()))?;
            }
            std::fs::write(&path, bytes)
                .with_context(|| format!("cannot restore {}", path.display()))?;
        }
        Ok(())
    }

    /// Whether every protected file in `dir` still holds the fixture's bytes.
    fn intact(&self, dir: &Path) -> bool {
        self.files.iter().all(|(rel, bytes)| {
            let path = dir.join(rel);
            path.symlink_metadata()
                .is_ok_and(|m| m.file_type().is_file() && m.len() == bytes.len() as u64)
                && std::fs::read(&path).is_ok_and(|now| now == *bytes)
        })
    }
}

/// A file in the attempt that would run code of the attempt's choosing
/// outside its tests, or change how cargo builds them: a build script
/// (`build.rs` is found by cargo without any manifest entry), a `.cargo`
/// directory (a `runner` or `rustc-wrapper` setting) or a toolchain pin.
/// The fixture crates have none, so an attempt that adds one is unsatisfied.
fn untrusted_build_input(dir: &Path) -> Option<&'static str> {
    [
        ("build.rs", "a build script"),
        (".cargo", "a cargo configuration"),
        ("rust-toolchain", "a toolchain pin"),
        ("rust-toolchain.toml", "a toolchain pin"),
    ]
    .into_iter()
    .find(|(name, _)| dir.join(name).symlink_metadata().is_ok())
    .map(|(_, why)| why)
}

/// Read the text file `rel` of the attempt. The tree is the worker's, so the
/// read refuses a symbolic link on any component of the path (it could point
/// at any file the checking process can read), anything but a regular file,
/// a file over [`MAX_ATTEMPT_FILE`], and text that is not UTF-8. A missing
/// file is unsatisfied, not an error.
fn read_attempt_file(dir: &Path, rel: &str) -> Result<Judged<String>> {
    let mut path = dir.to_path_buf();
    for component in Path::new(rel).components() {
        path.push(component);
        match path.symlink_metadata() {
            Ok(m) if m.file_type().is_symlink() => {
                return unmet(format!("{rel} passes through a symbolic link"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return unmet(format!("{rel} is missing"));
            }
            Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", path.display())),
        }
    }
    let meta = path
        .symlink_metadata()
        .with_context(|| format!("cannot inspect {}", path.display()))?;
    if !meta.is_file() {
        return unmet(format!("{rel} is not a regular file"));
    }
    if meta.len() > MAX_ATTEMPT_FILE {
        return unmet(format!("{rel} is larger than {MAX_ATTEMPT_FILE} bytes"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A link swapped in after the check above is refused by the kernel.
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_ATTEMPT_FILE + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if bytes.len() as u64 > MAX_ATTEMPT_FILE {
        return unmet(format!("{rel} is larger than {MAX_ATTEMPT_FILE} bytes"));
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(Ok(text)),
        Err(_) => unmet(format!("{rel} is not UTF-8 text")),
    }
}

/// Run `check` against `dir` (the attempt's checked-out tree). `Ok(false)` is
/// an unsatisfied brief; see [`run_verdict`] for the reason.
pub fn run(check: &Check, dir: &Path, env: &CheckEnv<'_>) -> Result<bool> {
    run_verdict(check, dir, env).map(|v| v.is_satisfied())
}

/// [`run`], with the reason an unsatisfied check gives.
pub fn run_verdict(check: &Check, dir: &Path, env: &CheckEnv<'_>) -> Result<Verdict> {
    run_guarded(check, dir, env, &Guard::none())
        .map(|v| with_network_notice(v, CheckNetwork::current()))
}

/// The verdict for a cargo run that must pass and must run at least
/// `min_passed` tests.
fn enough_passed(ran: &CargoRun, min_passed: u64) -> Verdict {
    if ran.failed > 0 {
        return Verdict::Unsatisfied(format!(
            "{} test(s) failed: {}",
            ran.failed,
            ran.failed_names.join(", ")
        ));
    }
    if ran.passed < min_passed {
        return Verdict::Unsatisfied(format!(
            "{} test(s) passed, at least {min_passed} needed",
            ran.passed
        ));
    }
    Verdict::Satisfied
}

fn run_guarded(check: &Check, dir: &Path, env: &CheckEnv<'_>, guard: &Guard) -> Result<Verdict> {
    match check {
        Check::CargoTest { min_passed } => match cargo_test(dir, &[], None, env, guard)? {
            Ok(ran) => Ok(enough_passed(&ran, *min_passed)),
            Err(why) => Ok(Verdict::Unsatisfied(why)),
        },
        Check::CargoTestUses {
            min_passed,
            file,
            function,
            helper,
            absent,
        } => {
            // The cheap structural read goes first: no build for an attempt
            // that never calls its helper.
            let source = match read_attempt_file(dir, file)? {
                Ok(text) => text,
                Err(why) => return Ok(Verdict::Unsatisfied(why)),
            };
            if !structure::body_uses(&source, function, helper, absent) {
                return Ok(Verdict::Unsatisfied(format!(
                    "{function} in {file} does not use {helper}"
                )));
            }
            match cargo_test(dir, &[], None, env, guard)? {
                Ok(ran) => Ok(enough_passed(&ran, *min_passed)),
                Err(why) => Ok(Verdict::Unsatisfied(why)),
            }
        }
        Check::CargoTestNamed {
            test_bin,
            test_name,
            mutations,
        } => {
            // Mutant copies come from the tree as it is before any of the
            // attempt's code has run, so a test that rewrites files when it
            // runs cannot change what the mutants are built from.
            let pristine = env.scratch.join("pristine");
            std::fs::create_dir_all(&pristine)
                .with_context(|| format!("cannot create {}", pristine.display()))?;
            gitutil::copy_tree(dir, &pristine, &[TARGET, ".git"])?;
            if let Err(why) = named_test_passes(dir, test_bin, test_name, env, guard)? {
                return Ok(Verdict::Unsatisfied(why));
            }
            for (i, mutation) in mutations.iter().enumerate() {
                if let Err(why) = named_test_fails_on_mutant(
                    dir, &pristine, test_bin, test_name, mutation, env, i,
                )? {
                    return Ok(Verdict::Unsatisfied(why));
                }
            }
            Ok(Verdict::Satisfied)
        }
        Check::Script { script, cases } => script_cases(dir, script, cases, env),
        Check::Docs {
            file,
            required,
            max_words,
        } => docs(dir, file, required, *max_words),
    }
}

/// What a sandboxed process did. `status` is `None` when it was killed at the
/// timeout.
struct Ran {
    status: Option<ExitStatus>,
    stdout: String,
    /// The tail of what the process wrote to standard error, for a reason.
    stderr: String,
    /// A process the command started left its process group and still holds
    /// the attempt's hold file.
    escaped: bool,
}

/// The environment a check process gets: only what a build needs.
fn build_env(scratch: &Path, target: &Path) -> Vec<(&'static str, PathBuf)> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".cargo")));
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".rustup")));
    let mut vars = vec![
        ("CARGO_TARGET_DIR", target.to_path_buf()),
        ("TMPDIR", scratch.to_path_buf()),
        ("TMP", scratch.to_path_buf()),
        ("TEMP", scratch.to_path_buf()),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        vars.push(("PATH", PathBuf::from(path)));
    }
    if let Some(h) = home {
        vars.push(("HOME", h));
    }
    if let Some(c) = cargo_home {
        vars.push(("CARGO_HOME", c));
    }
    if let Some(r) = rustup_home {
        vars.push(("RUSTUP_HOME", r));
    }
    vars
}

/// The toolchain directories a build reads: the cargo home's `bin`,
/// `registry` and `git`, and the rustup home. The cargo home itself is not
/// granted: `credentials.toml` (registry tokens) lives in it, and where reads
/// are allowlisted a grant of the directory would let the attempt's build
/// read it. A cargo home configuration file is not granted either, since a
/// registry token can sit in it; [`self_test`] runs a build under this exact
/// policy, so a toolchain that needs more is reported before any attempt.
fn toolchain_reads(cargo_home: Option<&Path>, rustup_home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = cargo_home
        .into_iter()
        .flat_map(|c| ["bin", "registry", "git"].map(|d| c.join(d)))
        .collect();
    roots.extend(rustup_home.map(Path::to_path_buf));
    roots.retain(|p| p.is_dir());
    roots
}

/// Run `program args` in `dir` under the sandbox. `dir` and `env.scratch`
/// are the only writable roots; the network is off where the sandbox can
/// deny it ([`CheckNetwork`]).
fn sandboxed(
    dir: &Path,
    program: &str,
    args: &[&str],
    env: &CheckEnv<'_>,
    target_name: &str,
) -> Result<Ran> {
    sandboxed_with(dir, program, args, env, target_name, &[])
}

/// [`sandboxed`] with more environment variables.
fn sandboxed_with(
    dir: &Path,
    program: &str,
    args: &[&str],
    env: &CheckEnv<'_>,
    target_name: &str,
    extra_env: &[(&str, &Path)],
) -> Result<Ran> {
    let target = env.scratch.join(target_name);
    std::fs::create_dir_all(&target)
        .with_context(|| format!("cannot create {}", target.display()))?;
    let vars = build_env(env.scratch, &target);
    // The network decision of every check (decision 0011, amended
    // 2026-10-06): denied on macOS and Linux; open on the Windows backend,
    // which cannot deny it, with its partial isolation accepted, and the
    // notice in the failure reason (`with_network_notice`).
    let network = CheckNetwork::current();
    let mut policy = SandboxPolicy::new(
        vec![dir.to_path_buf(), env.scratch.to_path_buf()],
        network.network(),
    )
    .accept_partial_isolation(network.accepts_partial_isolation())
    .with_tmp_dir(env.scratch.to_path_buf())
    .holding(env.scratch.join("hold"));
    let var = |name: &str| {
        vars.iter()
            .find(|(k, _)| *k == name)
            .map(|(_, p)| p.as_path())
    };
    for root in toolchain_reads(var("CARGO_HOME"), var("RUSTUP_HOME")) {
        policy = policy.with_readable_root(root);
    }
    let mut cmd = sandboxed_command(&policy, program, dir)
        .with_context(|| format!("cannot sandbox `{program}` in {}", dir.display()))?;
    // Nothing from kGraf's own environment (provider API keys among it)
    // reaches a check; on Windows the sandbox keeps the few variables a
    // program needs to start (`WINDOWS_BASE_VARS`) and its own git and TLS
    // settings.
    cmd.env_clear();
    for (k, v) in &vars {
        cmd.env(k, v);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    // Builds must not reach a registry even if the sandbox's network rule
    // were ever bypassed, nor where the network is open (Windows); a fixture
    // crate has no dependencies.
    cmd.env("CARGO_NET_OFFLINE", "true");
    cmd.args(args)
        .stdin(Redirect::Null)
        .stdout(Redirect::Piped)
        .stderr(Redirect::Piped);
    // Bounds an escaped descendant's CPU: the group kill below does not reach
    // a process that calls its own `setsid`.
    #[cfg(unix)]
    // SAFETY: the hook only calls `setrlimit` and allocates nothing.
    unsafe {
        kgraf_sandbox::lifecycle::limit_cpu_seconds(
            cmd.command_mut(),
            kgraf_sandbox::lifecycle::cpu_rlimit_seconds(env.timeout),
        );
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot run `{program}` in {}", dir.display()))?;
    let out_rx = drain(child.stdout.take());
    let err_rx = drain(child.stderr.take());

    let started = Instant::now();
    let status = loop {
        #[cfg(unix)]
        {
            // Kill the group as soon as the leader exits, before it is
            // reaped: a background process must not outlive the command.
            match kgraf_sandbox::lifecycle::exited_unreaped(child.id()) {
                Ok(true) => kgraf_sandbox::lifecycle::kill_group_until_hold_free(
                    child.id(),
                    &env.scratch.join("hold"),
                ),
                Ok(false) => {}
                Err(e) => bail!("waiting for `{program}`: {e}"),
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= env.timeout => {
                #[cfg(unix)]
                // SAFETY: the child called `setsid`, so its id is its own
                // group; killpg sends a signal to that group only.
                unsafe {
                    libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => bail!("waiting for `{program}`: {e}"),
        }
    };
    #[cfg(unix)]
    {
        // SIGKILL lands asynchronously: give the group a bounded time to go.
        let deadline = Instant::now() + kgraf_sandbox::lifecycle::GROUP_EXIT_GRACE;
        // SAFETY: signal 0 only probes whether the group has members.
        while unsafe { libc::killpg(child.id() as libc::pid_t, 0) } == 0
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(unix)]
    let escaped = !kgraf_sandbox::lifecycle::hold_free(&env.scratch.join("hold"));
    #[cfg(not(unix))]
    let escaped = false;
    // A descendant that left the group can hold the pipes open; wait a bounded
    // time for the readers rather than forever.
    let grace = Duration::from_secs(2);
    let stdout = out_rx.recv_timeout(grace).unwrap_or_default();
    let stderr = err_rx.recv_timeout(grace).unwrap_or_default();
    Ok(Ran {
        status,
        stdout,
        stderr,
        escaped,
    })
}

/// Read a pipe on its own thread, keep at most [`OUTPUT_CAP`] bytes, keep
/// draining past it so the writer never blocks, and send the text once the
/// pipe closes. The thread is detached: it ends when every writer is gone.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.by_ref().take(OUTPUT_CAP).read_to_end(&mut kept);
            let _ = std::io::copy(&mut pipe, &mut std::io::sink());
        }
        let _ = tx.send(String::from_utf8_lossy(&kept).into_owned());
    });
    rx
}

/// The last few lines of a process's standard error, for a reason.
fn stderr_tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(4)..].join(" | ");
    let mut tail: String = tail.chars().take(400).collect();
    if tail.is_empty() {
        tail.push_str("no output");
    }
    tail
}

/// How one test process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestEnd {
    Passed,
    /// Exit status 101: libtest's code for a failed test.
    Failed,
    /// Any other end: a signal, another exit status.
    Crashed,
}

/// A cargo test run reduced to what the checks read.
struct CargoRun {
    passed: u64,
    failed: u64,
    failed_names: Vec<String>,
    /// Every test the executables list, ignored ones included.
    listed: Vec<String>,
    /// The listed tests marked `#[ignore]`: they do not run.
    ignored: Vec<String>,
    /// How each test that ran ended.
    ends: Vec<(String, TestEnd)>,
}

/// The test executables in cargo's `--message-format=json` build output, only
/// those that lie under `target`.
fn test_executables(cargo_json: &str, target: &Path) -> Vec<PathBuf> {
    let target =
        kgraf_core::plain_path::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    let mut found: Vec<PathBuf> = Vec::new();
    for line in cargo_json.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"] != "compiler-artifact" || message["profile"]["test"] != true {
            continue;
        }
        let Some(exe) = message["executable"].as_str() else {
            continue;
        };
        let exe = PathBuf::from(exe);
        let exe = kgraf_core::plain_path::canonicalize(&exe).unwrap_or(exe);
        if exe.starts_with(&target) && !found.contains(&exe) {
            found.push(exe);
        }
    }
    found
}

/// The names `exe --list [extra]` prints as tests (`name: test`).
fn list_tests(
    dir: &Path,
    exe: &str,
    extra: &[&str],
    env: &CheckEnv<'_>,
    manifest: &Path,
) -> Result<Judged<Vec<String>>> {
    let mut args = vec!["--list"];
    args.extend_from_slice(extra);
    let ran = sandboxed_with(
        dir,
        exe,
        &args,
        env,
        TARGET,
        &[("CARGO_MANIFEST_DIR", manifest)],
    )?;
    if ran.escaped {
        return unmet("a process started by a test executable outlived it");
    }
    match ran.status {
        Some(status) if status.success() => Ok(Ok(ran
            .stdout
            .lines()
            .filter_map(|l| l.strip_suffix(": test"))
            .map(str::to_owned)
            .collect())),
        Some(_) => unmet(format!(
            "a test executable could not list its tests: {}",
            stderr_tail(&ran.stderr)
        )),
        None => unmet("a test executable timed out while listing its tests"),
    }
}

/// `cargo test [selector]` in `dir`, run test by test (see the module docs).
/// `only` limits the run to the test of that name. The build and the tests
/// share `env.timeout`. Unsatisfied, with the reason, when the crate is
/// missing, carries a build input the runner refuses, does not build, leaves
/// a process behind, changes a protected file, or times out.
fn cargo_test(
    dir: &Path,
    selector: &[&str],
    only: Option<&str>,
    env: &CheckEnv<'_>,
    guard: &Guard,
) -> Result<Judged<CargoRun>> {
    if !dir.join("Cargo.toml").is_file() {
        return unmet("the attempt has no Cargo.toml");
    }
    if let Some(why) = untrusted_build_input(dir) {
        return unmet(format!("the tree carries {why}"));
    }
    let started = Instant::now();
    let left = || CheckEnv {
        scratch: env.scratch,
        timeout: env.timeout.saturating_sub(started.elapsed()),
    };
    let timed_out = || format!("the check timed out after {}s", env.timeout.as_secs());

    let mut args = vec!["test"];
    args.extend_from_slice(selector);
    args.extend(["--no-run", "--message-format=json"]);
    let built = sandboxed(dir, "cargo", &args, &left(), TARGET)?;
    if built.escaped {
        return unmet("a process started by the build outlived it");
    }
    let Some(status) = built.status else {
        return unmet(timed_out());
    };
    if !status.success() {
        return unmet(format!("the build failed: {}", stderr_tail(&built.stderr)));
    }
    if !guard.intact(dir) || untrusted_build_input(dir).is_some() {
        return unmet("the build changed a protected file or added a build input");
    }
    let executables = test_executables(&built.stdout, &env.scratch.join(TARGET));
    if executables.is_empty() {
        return unmet("the build produced no test executable");
    }

    let mut run = CargoRun {
        passed: 0,
        failed: 0,
        failed_names: Vec::new(),
        listed: Vec::new(),
        ignored: Vec::new(),
        ends: Vec::new(),
    };
    for exe in &executables {
        let Some(exe) = exe.to_str() else {
            bail!("a test executable path is not UTF-8: {}", exe.display());
        };
        let listed = match list_tests(dir, exe, &[], &left(), dir)? {
            Ok(names) => names,
            Err(why) => return unmet(why),
        };
        let ignored = match list_tests(dir, exe, &["--ignored"], &left(), dir)? {
            Ok(names) => names,
            Err(why) => return unmet(why),
        };
        for name in listed.iter().filter(|n| !ignored.contains(n)) {
            if only.is_some_and(|o| o != name) {
                continue;
            }
            let ran = sandboxed_with(
                dir,
                exe,
                &["--exact", name, "--test-threads=1"],
                &left(),
                TARGET,
                &[("CARGO_MANIFEST_DIR", dir)],
            )?;
            if ran.escaped {
                return unmet(format!("test {name} left a process running"));
            }
            let end = match ran.status {
                None => return unmet(timed_out()),
                Some(s) if s.code() == Some(0) => TestEnd::Passed,
                Some(s) if s.code() == Some(101) => TestEnd::Failed,
                Some(_) => TestEnd::Crashed,
            };
            match end {
                TestEnd::Passed => run.passed += 1,
                TestEnd::Failed | TestEnd::Crashed => {
                    run.failed += 1;
                    run.failed_names.push(name.clone());
                }
            }
            run.ends.push((name.clone(), end));
        }
        run.listed.extend(listed);
        run.ignored.extend(ignored);
    }
    // A run that rewrote a protected file, or left a build input behind, is
    // not a run of the fixture's tests.
    if !guard.intact(dir) || untrusted_build_input(dir).is_some() {
        return unmet("a test changed a protected file or added a build input");
    }
    Ok(Ok(run))
}

fn named_args(test_bin: &str) -> [&str; 2] {
    ["--test", test_bin]
}

/// The named test ran, passed, and was the only test that did.
fn named_test_passes(
    dir: &Path,
    test_bin: &str,
    test_name: &str,
    env: &CheckEnv<'_>,
    guard: &Guard,
) -> Result<Judged<()>> {
    let ran = match cargo_test(dir, &named_args(test_bin), Some(test_name), env, guard)? {
        Ok(ran) => ran,
        Err(why) => return unmet(why),
    };
    if !ran.listed.iter().any(|n| n == test_name) {
        return unmet(format!("there is no test named {test_name} in {test_bin}"));
    }
    if ran.ignored.iter().any(|n| n == test_name) {
        return unmet(format!("test {test_name} is ignored, so it did not run"));
    }
    if ran.failed > 0 {
        return unmet(format!("test {test_name} failed"));
    }
    if ran.passed != 1 {
        return unmet(format!(
            "{} tests named {test_name} passed, exactly one is needed",
            ran.passed
        ));
    }
    Ok(Ok(()))
}

/// Replace the contents of `dir` with `pristine`, keeping `dir`'s own `.git`
/// and `target`. Links in `dir` are removed, not followed.
fn reset_tree(dir: &Path, pristine: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let entry = entry?;
        if entry.file_name() == TARGET || entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        }
        .with_context(|| format!("cannot remove {}", path.display()))?;
    }
    gitutil::copy_tree(pristine, dir, &[])
}

/// Apply `mutation` to the attempt and require the named test to run and
/// fail there (a test that ended with libtest's failure status: a tree that
/// no longer builds proves nothing about the test). The mutant is built and
/// run at the attempt's own path, in the same target directory, so the test
/// cannot tell a mutant run from the real one by where it runs. `dir` is
/// left as the pristine copy.
fn named_test_fails_on_mutant(
    dir: &Path,
    pristine: &Path,
    test_bin: &str,
    test_name: &str,
    mutation: &Mutation,
    env: &CheckEnv<'_>,
    index: usize,
) -> Result<Judged<()>> {
    reset_tree(dir, pristine)?;
    // `pristine` is a copy the runner made, with no links in it.
    let path = dir.join(mutation.file);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("mutation target {} is unreadable", path.display()))?;
    if !text.contains(mutation.from) {
        // The fixture the mutation was written for is not what the attempt
        // holds: the function was changed (protected, so caught earlier) or
        // the mutation is stale. Either way the test cannot be judged.
        bail!(
            "mutation text {:?} is not in {}; the mutation fixture is stale",
            mutation.from,
            mutation.file
        );
    }
    std::fs::write(&path, text.replacen(mutation.from, mutation.to, 1))
        .with_context(|| format!("cannot write {}", path.display()))?;
    let outcome = cargo_test(
        dir,
        &named_args(test_bin),
        Some(test_name),
        env,
        &Guard::none(),
    );
    // Whatever happened, the next mutant and the caller start from `pristine`.
    let restored = reset_tree(dir, pristine);
    let ran = match outcome? {
        Ok(ran) => ran,
        Err(why) => return unmet(format!("mutant {index} could not be judged: {why}")),
    };
    restored?;
    match ran.ends.iter().find(|(n, _)| n == test_name) {
        Some((_, TestEnd::Failed)) => Ok(Ok(())),
        Some((_, TestEnd::Passed)) => unmet(format!(
            "test {test_name} still passed with {:?} replaced by {:?} in {}",
            mutation.from, mutation.to, mutation.file
        )),
        Some((_, TestEnd::Crashed)) => unmet(format!(
            "test {test_name} crashed on mutant {index} instead of failing"
        )),
        None => unmet(format!("test {test_name} did not run on mutant {index}")),
    }
}

fn script_cases(
    dir: &Path,
    script: &str,
    cases: &[ScriptCase],
    env: &CheckEnv<'_>,
) -> Result<Verdict> {
    let path = dir.join(script);
    if !path.is_file() {
        return Ok(Verdict::Unsatisfied(format!("{script} is missing")));
    }
    for case in cases {
        let mut args = vec![script];
        args.extend_from_slice(case.args);
        let ran = sandboxed(dir, "sh", &args, env, TARGET)?;
        if ran.escaped {
            return Ok(Verdict::Unsatisfied(format!(
                "{script} left a process running"
            )));
        }
        let Some(status) = ran.status else {
            return Ok(Verdict::Unsatisfied(format!(
                "{script} timed out after {}s",
                env.timeout.as_secs()
            )));
        };
        let code = status.code().unwrap_or(-1);
        if ran.stdout != case.expect_stdout || code != case.expect_status {
            return Ok(Verdict::Unsatisfied(format!(
                "{script} {:?}: exit status {code} (wanted {}), output {} the expected text",
                case.args,
                case.expect_status,
                if ran.stdout == case.expect_stdout {
                    "matches"
                } else {
                    "differs from"
                }
            )));
        }
    }
    Ok(Verdict::Satisfied)
}

/// Whole-token test: `needle` in `line` with no alphanumeric, `.` or `-`
/// neighbor (so `0.2.0` is not inside `10.2.0` or `0.2.01`).
fn has_token(line: &str, needle: &str) -> bool {
    let is_part = |c: char| c.is_alphanumeric() || c == '.' || c == '-';
    line.match_indices(needle).any(|(i, m)| {
        let before = line[..i].chars().next_back();
        let after = line[i + m.len()..].chars().next();
        // A trailing sentence period is punctuation, not part of the token.
        let after_ok = match after {
            None => true,
            Some('.') => !line[i + m.len() + 1..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric),
            Some(c) => !is_part(c),
        };
        !before.is_some_and(is_part) && after_ok
    })
}

/// A token of `line` that starts with `word`, case-insensitive.
fn has_word_prefix(line: &str, word: &str) -> bool {
    let word = word.to_lowercase();
    line.split(|c: char| !c.is_alphanumeric())
        .any(|t| t.to_lowercase().starts_with(&word))
}

fn fact_holds(text: &str, fact: &DocFact) -> bool {
    match fact {
        DocFact::Text(s) => text.contains(s),
        DocFact::Entry {
            version,
            date,
            word,
        } => {
            let lines: Vec<&str> = text.lines().collect();
            lines.iter().enumerate().any(|(i, l)| {
                has_token(l, version)
                    && has_token(l, date)
                    && lines[i..].iter().any(|later| has_word_prefix(later, word))
            })
        }
    }
}

fn docs(dir: &Path, file: &str, required: &[DocFact], max_words: usize) -> Result<Verdict> {
    let text = match read_attempt_file(dir, file)? {
        Ok(text) => text,
        Err(why) => return Ok(Verdict::Unsatisfied(why)),
    };
    let words = text.split_whitespace().count();
    if words > max_words {
        return Ok(Verdict::Unsatisfied(format!(
            "{file} has {words} words, over the cap of {max_words}"
        )));
    }
    match required.iter().find(|fact| !fact_holds(&text, fact)) {
        Some(fact) => Ok(Verdict::Unsatisfied(format!(
            "{file} lacks a required fact: {fact:?}"
        ))),
        None => Ok(Verdict::Satisfied),
    }
}

/// Run the whole check pipeline once on a known-good crate: build, list, run
/// a test and its mutant, under the same sandbox policy and environment an
/// attempt gets. An `Err` means the harness cannot judge any attempt (no
/// toolchain, a sandbox that blocks the build), which is not a fact about a
/// model. Runs once per process; the result is kept.
pub fn self_test(timeout: Duration) -> Result<()> {
    static RESULT: std::sync::OnceLock<std::result::Result<(), String>> =
        std::sync::OnceLock::new();
    RESULT
        .get_or_init(|| control_run(timeout).map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn control_run(timeout: Duration) -> Result<()> {
    let root = tempfile::tempdir().context("cannot create the control's temp dir")?;
    let dir = root.path().join("crate");
    let scratch = root.path().join("scratch");
    for d in [dir.join("src"), dir.join("tests"), scratch.clone()] {
        std::fs::create_dir_all(&d).with_context(|| format!("cannot create {}", d.display()))?;
    }
    let files = [
        (
            "Cargo.toml",
            "[package]\nname = \"bench_control\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        ),
        ("src/lib.rs", "pub fn one() -> u32 {\n    1\n}\n"),
        (
            "tests/control.rs",
            "use bench_control::one;\n\n#[test]\nfn one_is_one() {\n    assert_eq!(one(), 1);\n}\n",
        ),
    ];
    for (rel, text) in files {
        std::fs::write(dir.join(rel), text).with_context(|| format!("cannot write {rel}"))?;
    }
    let check = Check::CargoTestNamed {
        test_bin: "control",
        test_name: "one_is_one",
        mutations: &[Mutation {
            file: "src/lib.rs",
            from: "    1\n",
            to: "    2\n",
        }],
    };
    let env = CheckEnv {
        scratch: &scratch,
        timeout,
    };
    match run_verdict(&check, &dir, &env)? {
        Verdict::Satisfied => Ok(()),
        Verdict::Unsatisfied(why) => bail!(
            "the check harness could not judge a known-good crate ({why}); \
             its toolchain or sandbox is not working, and no attempt can be judged"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brief::suite;

    fn docs(dir: &Path, file: &str, required: &[DocFact], max_words: usize) -> Result<bool> {
        super::docs(dir, file, required, max_words).map(|v| v.is_satisfied())
    }

    fn script_cases(
        dir: &Path,
        script: &str,
        cases: &[ScriptCase],
        env: &CheckEnv<'_>,
    ) -> Result<bool> {
        super::script_cases(dir, script, cases, env).map(|v| v.is_satisfied())
    }

    /// Where the network is open (the Windows decision, stood in for here),
    /// an unsatisfied verdict says so; a satisfied one, and every verdict
    /// where the network is denied, is unchanged.
    #[test]
    fn an_unsatisfied_verdict_with_the_network_open_says_so() {
        let open = CheckNetwork::open_with_notice();
        let failed = Verdict::Unsatisfied("1 test(s) failed: t".into());
        assert_eq!(
            with_network_notice(failed.clone(), open),
            Verdict::Unsatisfied(format!(
                "1 test(s) failed: t [sandbox] {}",
                kgraf_sandbox::NETWORK_OPEN_NOTICE
            ))
        );
        assert_eq!(
            with_network_notice(Verdict::Satisfied, open),
            Verdict::Satisfied
        );
        assert_eq!(
            with_network_notice(failed.clone(), CheckNetwork::deny()),
            failed
        );
    }

    fn run_guarded(check: &Check, dir: &Path, env: &CheckEnv<'_>, guard: &Guard) -> Result<bool> {
        super::run_guarded(check, dir, env, guard).map(|v| v.is_satisfied())
    }

    fn verify(brief: &Brief, attempt: &Attempt<'_>, env: &CheckEnv<'_>) -> Result<bool> {
        super::verify(brief, attempt, env).map(|v| v.is_satisfied())
    }

    fn env_in(scratch: &Path, secs: u64) -> CheckEnv<'_> {
        CheckEnv {
            scratch,
            timeout: Duration::from_secs(secs),
        }
    }

    #[test]
    fn docs_check_wants_every_fact_and_the_word_cap() {
        let dir = tempfile::tempdir().expect("dir");
        let file = "NOTES.md";
        let x = [DocFact::Text("x")];
        // Missing file.
        assert!(!docs(dir.path(), file, &x, 10).expect("check"));
        std::fs::write(dir.path().join(file), "one two x").expect("write");
        assert!(docs(dir.path(), file, &x, 10).expect("check"));
        assert!(!docs(dir.path(), file, &[DocFact::Text("missing")], 10).expect("check"));
        assert!(!docs(dir.path(), file, &x, 2).expect("check: over cap"));
    }

    const ENTRY: DocFact = DocFact::Entry {
        version: "0.2.0",
        date: "2026-09-28",
        word: "crash",
    };

    #[test]
    fn the_changelog_entry_is_read_structurally() {
        let good = "## 0.2.0 - 2026-09-28\n\nFixed a crash on an empty input file.\n";
        assert!(fact_holds(good, &ENTRY));
        assert!(fact_holds("0.2.0 (2026-09-28): fixed a crash.", &ENTRY));
        assert!(fact_holds(
            "## 0.2.0 2026-09-28.\nCrashes on empty input fixed",
            &ENTRY
        ));

        // The three strings scattered with no entry: the old substring
        // check passed this.
        assert!(!fact_holds("2026-09-28\n0.2.0\ncrash", &ENTRY));
        // A different version that contains the text.
        assert!(!fact_holds("## 10.2.0 - 2026-09-28\ncrash fixed", &ENTRY));
        assert!(!fact_holds("## 0.2.01 - 2026-09-28\ncrash fixed", &ENTRY));
        // A longer date.
        assert!(!fact_holds("## 0.2.0 - 2026-09-281\ncrash fixed", &ENTRY));
        // The word only before the entry.
        assert!(!fact_holds("crash\n## 0.2.0 - 2026-09-28\nnothing", &ENTRY));
        // Missing pieces.
        assert!(!fact_holds("## 0.2.0 - 2026-09-28\nfixed a bug", &ENTRY));
        assert!(!fact_holds("## 0.2.0\ncrash, dated 2026-09-28", &ENTRY));
    }

    #[test]
    fn script_check_compares_stdout_and_status_exactly() {
        let dir = tempfile::tempdir().expect("dir");
        let scratch = tempfile::tempdir().expect("scratch");
        std::fs::write(dir.path().join("r.sh"), "#!/bin/sh\necho hi\nexit 3\n").expect("write");
        let env = env_in(scratch.path(), 30);
        let ok = script_cases(
            dir.path(),
            "r.sh",
            &[ScriptCase {
                args: &[],
                expect_stdout: "hi\n",
                expect_status: 3,
            }],
            &env,
        )
        .expect("check");
        assert!(ok);
        let wrong = script_cases(
            dir.path(),
            "r.sh",
            &[ScriptCase {
                args: &[],
                expect_stdout: "hi\n",
                expect_status: 0,
            }],
            &env,
        )
        .expect("check");
        assert!(!wrong);
    }

    #[test]
    fn a_missing_manifest_or_script_is_unsatisfied_not_an_error() {
        let dir = tempfile::tempdir().expect("dir");
        let scratch = tempfile::tempdir().expect("scratch");
        let env = env_in(scratch.path(), 30);
        assert!(
            !run(&Check::CargoTest { min_passed: 1 }, dir.path(), &env).expect("check"),
            "no manifest"
        );
        assert!(
            !script_cases(dir.path(), "nope.sh", &[], &env).expect("check"),
            "no cases still needs the file to exist"
        );
    }

    /// A test can write a forged report straight to the standard output
    /// descriptor, which libtest does not capture. Nothing a test prints is
    /// read: each test is judged by its own exit status.
    #[test]
    fn a_forged_report_on_stdout_cannot_pass_a_failing_test() {
        let dir = fixture_copy("tests-sum");
        let scratch = tempfile::tempdir().expect("scratch");
        std::fs::create_dir_all(dir.path().join("tests")).expect("tests dir");
        std::fs::write(
            dir.path().join("tests/forge.rs"),
            "use std::io::Write;\n#[test]\nfn forges() {\n    let mut out = std::io::stdout();\n    \
             out.write_all(b\"test forges ... ok\\ntest result: ok. 99 passed; 0 failed; 0 ignored; \
             0 measured; 0 filtered out\\n\").unwrap();\n    out.flush().unwrap();\n    \
             panic!(\"a real failure\");\n}\n",
        )
        .expect("write");
        let verdict = run_verdict(
            &Check::CargoTest { min_passed: 1 },
            dir.path(),
            &env_in(scratch.path(), 240),
        )
        .expect("check");
        let reason = verdict.reason().expect("a failing test is unsatisfied");
        assert!(reason.contains("forges"), "{reason}");
    }

    /// The number of passing tests is the number of test processes that
    /// passed, whatever the tests print.
    #[test]
    fn forged_passes_on_stdout_do_not_raise_the_count() {
        let dir = fixture_copy("tests-sum");
        let scratch = tempfile::tempdir().expect("scratch");
        std::fs::create_dir_all(dir.path().join("tests")).expect("tests dir");
        std::fs::write(
            dir.path().join("tests/forge.rs"),
            "use std::io::Write;\n#[test]\nfn forges() {\n    \
             std::io::stdout().write_all(b\"test a ... ok\\ntest b ... ok\\ntest c ... ok\\n\").unwrap();\n}\n",
        )
        .expect("write");
        let env = env_in(scratch.path(), 240);
        let count =
            |min| run(&Check::CargoTest { min_passed: min }, dir.path(), &env).expect("check");
        // The fixture has no tests of its own: `forges` is the one test, and
        // its three forged lines are not three passes.
        assert!(count(1));
        assert!(!count(2), "forged lines counted as passes");
    }

    /// A vacuous test that fails only when it can tell it runs in a mutant
    /// copy (by its path or its target directory) must not pass: a mutant
    /// runs at the attempt's own path and target.
    #[test]
    fn a_test_cannot_tell_a_mutant_run_by_where_it_runs() {
        assert!(
            !run_tests_sum(
                "#[test]\nfn handles_empty_and_negative() {\n    \
                 let here = env!(\"CARGO_MANIFEST_DIR\");\n    \
                 let target = std::env::var(\"CARGO_TARGET_DIR\").unwrap_or_default();\n    \
                 assert!(!here.contains(\"mutant\") && !target.contains(\"mutant\"));\n}\n"
            ),
            "a path-sniffing vacuous test passed"
        );
    }

    /// A refusal carries its reason.
    #[test]
    fn an_unsatisfied_check_says_why() {
        let dir = tempfile::tempdir().expect("dir");
        let scratch = tempfile::tempdir().expect("scratch");
        let env = env_in(scratch.path(), 30);
        let verdict =
            run_verdict(&Check::CargoTest { min_passed: 1 }, dir.path(), &env).expect("check");
        assert!(
            verdict.reason().is_some_and(|r| r.contains("Cargo.toml")),
            "{verdict:?}"
        );
        let verdict = super::script_cases(dir.path(), "nope.sh", &[], &env).expect("check");
        assert!(
            verdict.reason().is_some_and(|r| r.contains("nope.sh")),
            "{verdict:?}"
        );

        let greeter = greeter_with_trait();
        std::fs::write(greeter.path().join("build.rs"), "fn main() {}\n").expect("write");
        let verdict = run_verdict(&greeter_check(), greeter.path(), &env).expect("check");
        assert!(
            verdict.reason().is_some_and(|r| r.contains("build script")),
            "{verdict:?}"
        );
    }

    /// Files read from the attempt are not followed through a link, are
    /// capped, and are text.
    #[cfg(unix)]
    #[test]
    fn attempt_files_are_read_without_links_and_within_a_cap() {
        let dir = tempfile::tempdir().expect("dir");
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "SECRET-TEXT").expect("write");

        std::os::unix::fs::symlink(&secret, dir.path().join("NOTES.md")).expect("link");
        let refused = read_attempt_file(dir.path(), "NOTES.md").expect("read");
        assert!(
            refused.as_ref().is_err_and(|w| w.contains("symbolic link")),
            "{refused:?}"
        );

        // A link on a directory component of the path.
        std::os::unix::fs::symlink(outside.path(), dir.path().join("docs")).expect("link");
        std::fs::write(outside.path().join("a.md"), "x").expect("write");
        assert!(
            read_attempt_file(dir.path(), "docs/a.md")
                .expect("read")
                .is_err()
        );

        // A device behind a link is never opened.
        std::os::unix::fs::symlink("/dev/zero", dir.path().join("ZERO.md")).expect("link");
        assert!(
            read_attempt_file(dir.path(), "ZERO.md")
                .expect("read")
                .is_err()
        );

        let big = dir.path().join("BIG.md");
        std::fs::write(&big, vec![b'a'; MAX_ATTEMPT_FILE as usize + 1]).expect("write");
        let refused = read_attempt_file(dir.path(), "BIG.md").expect("read");
        assert!(
            refused.as_ref().is_err_and(|w| w.contains("larger")),
            "{refused:?}"
        );

        std::fs::write(dir.path().join("BIN.md"), [0xff, 0xfe, 0x00]).expect("write");
        assert!(
            read_attempt_file(dir.path(), "BIN.md")
                .expect("read")
                .is_err()
        );

        std::fs::write(dir.path().join("OK.md"), "fine").expect("write");
        assert_eq!(
            read_attempt_file(dir.path(), "OK.md").expect("read"),
            Ok("fine".to_owned())
        );
        assert!(
            read_attempt_file(dir.path(), "MISSING.md")
                .expect("read")
                .is_err()
        );

        // The docs check gives a link the same answer.
        assert!(!docs(dir.path(), "NOTES.md", &[DocFact::Text("SECRET")], 100).expect("docs"));
    }

    /// The runner proves its own toolchain and sandbox on a known-good crate.
    #[test]
    fn the_positive_control_passes_on_a_working_harness() {
        self_test(Duration::from_secs(240)).expect("the control crate is judged satisfied");
    }

    #[test]
    fn the_cargo_home_itself_is_not_a_readable_root() {
        let home = tempfile::tempdir().expect("home");
        let cargo = home.path().join(".cargo");
        let rustup = home.path().join(".rustup");
        for d in ["bin", "registry", "git"] {
            std::fs::create_dir_all(cargo.join(d)).expect("dir");
        }
        std::fs::create_dir_all(&rustup).expect("dir");
        std::fs::write(cargo.join("credentials.toml"), "token = \"FAKE\"").expect("write");
        let roots = toolchain_reads(Some(&cargo), Some(&rustup));
        assert!(!roots.contains(&cargo), "{roots:?}");
        for d in ["bin", "registry", "git"] {
            assert!(roots.contains(&cargo.join(d)), "{d} in {roots:?}");
        }
        assert!(roots.contains(&rustup), "{roots:?}");
        // A directory that does not exist is left out.
        assert!(toolchain_reads(Some(&home.path().join("none")), None).is_empty());
    }

    /// Needs a Linux kernel with Landlock ABI v3 or later. Not run on macOS,
    /// where reads are open apart from the sandbox's own credential deny list.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_check_cannot_read_the_cargo_credentials() {
        let home = tempfile::tempdir().expect("home");
        let cargo = home.path().join(".cargo");
        std::fs::create_dir_all(cargo.join("registry")).expect("dir");
        std::fs::write(cargo.join("credentials.toml"), "FAKE-TOKEN").expect("write");
        std::fs::write(cargo.join("registry/index"), "index-ok").expect("write");
        let dir = tempfile::tempdir().expect("dir");
        let scratch = tempfile::tempdir().expect("scratch");
        let mut policy = SandboxPolicy::new(
            vec![dir.path().to_path_buf(), scratch.path().to_path_buf()],
            kgraf_sandbox::Network::Deny,
        )
        .with_tmp_dir(scratch.path().to_path_buf());
        for root in toolchain_reads(Some(&cargo), None) {
            policy = policy.with_readable_root(root);
        }
        let cat = |path: PathBuf| {
            let mut cmd = sandboxed_command(&policy, "cat", dir.path()).expect("sandbox");
            cmd.arg(path)
                .stdin(Redirect::Null)
                .stdout(Redirect::Piped)
                .stderr(Redirect::Piped);
            let mut child = cmd.spawn().expect("spawn");
            let mut out = String::new();
            child
                .stdout
                .take()
                .expect("stdout")
                .read_to_string(&mut out)
                .expect("read");
            (child.wait().expect("wait").success(), out)
        };
        let (ok, out) = cat(cargo.join("credentials.toml"));
        assert!(!ok && !out.contains("FAKE-TOKEN"), "{out}");
        let (ok, out) = cat(cargo.join("registry/index"));
        assert!(ok && out == "index-ok", "{out}");
    }

    /// The API key a worker's code could read must not reach a check process.
    #[test]
    fn a_check_process_sees_no_credential_environment() {
        // Set only for this process; the check must not inherit it. Unique
        // name so a parallel test never reads it.
        // SAFETY: a variable with a name nothing else in this test binary reads.
        unsafe {
            std::env::set_var("KGRAF_BENCH_TEST_API_KEY", "sk-should-not-leak");
            // A name no credential scrub would match: only env_clear hides it.
            std::env::set_var("KGRAF_BENCH_TEST_PLAIN", "visible");
        }
        let dir = tempfile::tempdir().expect("dir");
        let scratch = tempfile::tempdir().expect("scratch");
        std::fs::write(
            dir.path().join("r.sh"),
            "#!/bin/sh\necho \"key=[$KGRAF_BENCH_TEST_API_KEY] anth=[$ANTHROPIC_API_KEY] plain=[$KGRAF_BENCH_TEST_PLAIN]\"\n",
        )
        .expect("write");
        let env = env_in(scratch.path(), 30);
        let saw_nothing = script_cases(
            dir.path(),
            "r.sh",
            &[ScriptCase {
                args: &[],
                expect_stdout: "key=[] anth=[] plain=[]\n",
                expect_status: 0,
            }],
            &env,
        )
        .expect("check");
        assert!(saw_nothing, "the check process inherited a credential");
    }

    /// The check process sees the build variables and nothing else from this
    /// process. On Windows the environment is listed by `cmd /d /c set`, which
    /// shows the process environment as given, with no shell runtime adding
    /// its own names, and the allowance also holds the variables a Windows
    /// program needs to start (`WINDOWS_BASE_VARS`), the sandbox's git and
    /// TLS settings, and the `PROMPT` that `cmd.exe` sets for itself.
    #[test]
    fn only_the_build_variables_are_passed() {
        // A plain name no scrub would match: only clearing hides it.
        // SAFETY: a variable with a name nothing else in this test binary reads.
        unsafe { std::env::set_var("KGRAF_BENCH_TEST_ENV_PROBE", "visible") };
        let dir = tempfile::tempdir().expect("dir");
        let scratch = tempfile::tempdir().expect("scratch");
        std::fs::write(dir.path().join("r.sh"), "#!/bin/sh\nenv\n").expect("write");
        let env = env_in(scratch.path(), 30);
        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd", &["/d", "/c", "set"])
        } else {
            ("sh", &["r.sh"])
        };
        let ran = sandboxed(dir.path(), program, args, &env, "target").expect("run");
        let names: Vec<&str> = ran
            .stdout
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim()))
            .collect();
        let allowed = [
            "PATH",
            "HOME",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "CARGO_TARGET_DIR",
            "CARGO_NET_OFFLINE",
            "TMPDIR",
            "TMP",
            "TEMP",
            "PWD",
            "SHLVL",
            "_",
            "OLDPWD",
            "NPM_CONFIG_USERCONFIG",
            "NPM_CONFIG_CACHE",
            "GOCACHE",
        ];
        // Windows compares variable names without case.
        let same = |a: &str, b: &str| {
            if cfg!(windows) {
                a.eq_ignore_ascii_case(b)
            } else {
                a == b
            }
        };
        let windows_only = |n: &str| {
            cfg!(windows)
                && (kgraf_sandbox::WINDOWS_BASE_VARS
                    .iter()
                    .any(|b| b.eq_ignore_ascii_case(n))
                    || ["GIT_CONFIG_COUNT", "CURL_SSL_BACKEND", "PROMPT"]
                        .iter()
                        .any(|b| b.eq_ignore_ascii_case(n))
                    || ["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"].iter().any(|p| {
                        n.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p))
                            && n.len() > p.len()
                            && n[p.len()..].bytes().all(|b| b.is_ascii_digit())
                    }))
        };
        for n in &names {
            assert!(
                allowed.iter().any(|a| same(a, n)) || windows_only(n),
                "unexpected variable {n} in {names:?}"
            );
        }
        assert!(
            !names.iter().any(|n| same(n, "KGRAF_BENCH_TEST_ENV_PROBE")),
            "{names:?}"
        );
        assert!(
            names.iter().any(|n| same(n, "CARGO_NET_OFFLINE")),
            "{names:?}"
        );
        assert!(
            names.iter().any(|n| same(n, "CARGO_TARGET_DIR")),
            "{names:?}"
        );
    }

    #[test]
    fn an_infinite_loop_is_killed_at_the_timeout() {
        let dir = tempfile::tempdir().expect("dir");
        let scratch = tempfile::tempdir().expect("scratch");
        // A background child too: the group kill must reach it.
        std::fs::write(
            dir.path().join("r.sh"),
            "#!/bin/sh\nsleep 600 &\nwhile true; do :; done\n",
        )
        .expect("write");
        let env = env_in(scratch.path(), 2);
        let started = Instant::now();
        let ok = script_cases(
            dir.path(),
            "r.sh",
            &[ScriptCase {
                args: &[],
                expect_stdout: "",
                expect_status: 0,
            }],
            &env,
        )
        .expect("check");
        assert!(!ok, "a killed script is unsatisfied");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the loop ran past the timeout: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_check_cannot_write_outside_its_roots() {
        let dir = tempfile::tempdir().expect("dir");
        let scratch = tempfile::tempdir().expect("scratch");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("planted");
        std::fs::write(
            dir.path().join("r.sh"),
            format!("#!/bin/sh\necho x > '{}'\n", target.display()),
        )
        .expect("write");
        let env = env_in(scratch.path(), 30);
        let _ = sandboxed(dir.path(), "sh", &["r.sh"], &env, "target").expect("run");
        assert!(!target.exists(), "the sandbox let a check write outside");
    }

    // ---- cargo checks against real fixtures ----------------------------

    fn fixture_copy(id: &str) -> tempfile::TempDir {
        let brief = suite().into_iter().find(|b| b.id == id).expect("brief");
        let dir = tempfile::tempdir().expect("dir");
        gitutil::copy_tree(&brief.fixture_dir(), dir.path(), &[]).expect("copy");
        dir
    }

    const GOOD_SUM_TEST: &str = "use fx_tests_sum::sum;\n\n#[test]\nfn handles_empty_and_negative() {\n    assert_eq!(sum(&[]), 0);\n    assert_eq!(sum(&[-3, 5, -2]), 0);\n}\n";

    fn tests_sum_check() -> Check {
        suite()
            .into_iter()
            .find(|b| b.id == "tests-sum")
            .expect("brief")
            .check
    }

    fn run_tests_sum(test_file: &str) -> bool {
        let dir = fixture_copy("tests-sum");
        let scratch = tempfile::tempdir().expect("scratch");
        std::fs::create_dir_all(dir.path().join("tests")).expect("tests dir");
        std::fs::write(dir.path().join("tests/more_tests.rs"), test_file).expect("write");
        run(&tests_sum_check(), dir.path(), &env_in(scratch.path(), 240)).expect("check")
    }

    #[test]
    fn a_named_test_check_needs_the_test_to_run_pass_and_catch_a_defect() {
        // A real test that asserts both things passes.
        assert!(run_tests_sum(GOOD_SUM_TEST), "the good test must pass");

        // Cargo exits 0 with zero matching tests: a differently named test,
        // and an empty file, must not pass.
        assert!(
            !run_tests_sum(&GOOD_SUM_TEST.replace("handles_empty_and_negative", "other_name")),
            "a test with another name matches nothing"
        );
        assert!(!run_tests_sum("// no tests here\n"), "an empty file");

        // A test with the right name that asserts nothing passes on the
        // real function and must fail the mutation check.
        assert!(
            !run_tests_sum("#[test]\nfn handles_empty_and_negative() {\n    assert!(true);\n}\n"),
            "a vacuous test proves nothing"
        );
        // A test that asserts only one of the two behaviors misses a mutant.
        assert!(
            !run_tests_sum(
                "use fx_tests_sum::sum;\n#[test]\nfn handles_empty_and_negative() {\n    \
                 assert_eq!(sum(&[]), 0);\n}\n"
            ),
            "the negative case is missing"
        );
        // An ignored test never runs.
        assert!(
            !run_tests_sum(&GOOD_SUM_TEST.replace("#[test]", "#[test]\n#[ignore]")),
            "an ignored test did not run"
        );
    }

    #[test]
    fn a_stale_mutation_is_a_harness_error() {
        let dir = fixture_copy("tests-sum");
        let scratch = tempfile::tempdir().expect("scratch");
        std::fs::create_dir_all(dir.path().join("tests")).expect("tests dir");
        std::fs::write(dir.path().join("tests/more_tests.rs"), GOOD_SUM_TEST).expect("write");
        let check = Check::CargoTestNamed {
            test_bin: "more_tests",
            test_name: "handles_empty_and_negative",
            mutations: &[Mutation {
                file: "src/lib.rs",
                from: "this text is not in the fixture",
                to: "x",
            }],
        };
        let err = run(&check, dir.path(), &env_in(scratch.path(), 240));
        assert!(err.is_err(), "{err:?}");
    }

    #[test]
    fn a_plain_cargo_check_wants_the_expected_number_of_tests() {
        // interface-greeter has one consumer test and no trait yet: a build
        // failure is unsatisfied. Adding the trait passes; asking for more
        // tests than exist does not.
        let dir = fixture_copy("interface-greeter");
        let scratch = tempfile::tempdir().expect("scratch");
        let env = env_in(scratch.path(), 240);
        assert!(!run(&Check::CargoTest { min_passed: 1 }, dir.path(), &env).expect("check"));
        let lib = dir.path().join("src/lib.rs");
        let mut text = std::fs::read_to_string(&lib).expect("lib");
        text.push_str("\npub trait Greeter {\n    fn greet(&self, name: &str) -> String;\n}\n");
        std::fs::write(&lib, text).expect("write");
        assert!(run(&Check::CargoTest { min_passed: 1 }, dir.path(), &env).expect("check"));
        assert!(
            !run(&Check::CargoTest { min_passed: 2 }, dir.path(), &env).expect("check"),
            "fewer tests than the fixture has"
        );
        // Deleting the tests must not pass.
        std::fs::remove_file(dir.path().join("tests/consumer.rs")).expect("rm");
        assert!(!run(&Check::CargoTest { min_passed: 1 }, dir.path(), &env).expect("check"));
    }

    /// The refactor briefs: passing tests are not enough. The extracted helper
    /// must be used by the original function, and a mutation that inlines the
    /// old logic (tests still green) must fail the check.
    #[test]
    fn a_refactor_check_needs_the_helper_used_not_only_tests_passing() {
        let brief = suite()
            .into_iter()
            .find(|b| b.id == "refactor-shipping")
            .expect("brief");
        let scratch = tempfile::tempdir().expect("scratch");
        let env = env_in(scratch.path(), 240);
        let dir = fixture_copy("refactor-shipping");
        let lib = dir.path().join("src/lib.rs");
        let good = "pub fn apply_discount(price: f64, pct_off: f64) -> f64 {\n    price - price * pct_off / 100.0\n}\n\npub fn total_price(price: f64, qty: u32, pct_off: f64) -> f64 {\n    apply_discount(price, pct_off) * qty as f64\n}\n";
        std::fs::write(&lib, good).expect("write");
        assert!(
            run(&brief.check, dir.path(), &env).expect("check"),
            "a real refactor"
        );
        // The mutation: the old logic inlined again; the helper stays, so
        // the tests still pass, but the check must not.
        let inlined = good.replace(
            "apply_discount(price, pct_off) * qty as f64",
            "(price - price * pct_off / 100.0) * qty as f64",
        );
        std::fs::write(&lib, &inlined).expect("write");
        assert!(
            run(&Check::CargoTest { min_passed: 2 }, dir.path(), &env).expect("plain"),
            "the tests alone cannot tell the mutant apart"
        );
        assert!(
            !run(&brief.check, dir.path(), &env).expect("check"),
            "inlined mutant"
        );
    }

    #[test]
    fn verify_fails_when_a_protected_file_changed() {
        let brief = suite()
            .into_iter()
            .find(|b| b.id == "interface-greeter")
            .expect("brief");
        let root = tempfile::tempdir().expect("root");
        let base = gitutil::init_fixture_repo(&brief.fixture_dir(), root.path()).expect("init");
        // A worker that adds the trait and also rewrites the consumer test.
        let lib = root.path().join("src/lib.rs");
        let mut text = std::fs::read_to_string(&lib).expect("lib");
        text.push_str("\npub trait Greeter {\n    fn greet(&self, name: &str) -> String;\n}\n");
        std::fs::write(&lib, text).expect("write");
        let consumer = root.path().join("tests/consumer.rs");
        let original = std::fs::read_to_string(&consumer).expect("consumer");
        std::fs::write(&consumer, format!("{original}\n// edited\n")).expect("write");
        let commit_all = |msg: &str| {
            for args in [vec!["add", "."], vec!["commit", "-qm", msg]] {
                let ok = std::process::Command::new("git")
                    .arg("-C")
                    .arg(root.path())
                    .args(&args)
                    .status()
                    .expect("git")
                    .success();
                assert!(ok);
            }
        };
        commit_all("worker");
        let head_of = || {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("git");
            String::from_utf8(out.stdout)
                .expect("utf8")
                .trim()
                .to_owned()
        };
        let commit = head_of();
        let checked = tempfile::tempdir().expect("checked");
        gitutil::checkout_commit(root.path(), &commit, checked.path()).expect("checkout");
        let scratch = tempfile::tempdir().expect("scratch");
        let attempt = Attempt {
            dir: checked.path(),
            repo: root.path(),
            base: &base,
            commit: &commit,
        };
        let env = env_in(scratch.path(), 240);
        // The build and the tests pass, so only the protected diff fails it.
        assert!(run(&brief.check, checked.path(), &env).expect("plain check"));
        assert!(!verify(&brief, &attempt, &env).expect("verify"));

        // The same attempt without the consumer edit is satisfied.
        std::fs::write(&consumer, original).expect("restore");
        commit_all("restore consumer");
        let commit = head_of();
        let checked2 = tempfile::tempdir().expect("checked");
        gitutil::checkout_commit(root.path(), &commit, checked2.path()).expect("checkout");
        let attempt = Attempt {
            dir: checked2.path(),
            repo: root.path(),
            base: &base,
            commit: &commit,
        };
        assert!(verify(&brief, &attempt, &env_in(scratch.path(), 240)).expect("verify"));
    }

    /// The interface-greeter fixture with the trait its brief asks for added.
    fn greeter_with_trait() -> tempfile::TempDir {
        let dir = fixture_copy("interface-greeter");
        let lib = dir.path().join("src/lib.rs");
        let mut text = std::fs::read_to_string(&lib).expect("lib");
        text.push_str("\npub trait Greeter {\n    fn greet(&self, name: &str) -> String;\n}\n");
        std::fs::write(&lib, text).expect("write");
        dir
    }

    fn greeter_guard() -> Guard {
        let brief = suite()
            .into_iter()
            .find(|b| b.id == "interface-greeter")
            .expect("brief");
        let files = ["tests/consumer.rs", "Cargo.toml"]
            .iter()
            .map(|rel| {
                let bytes = std::fs::read(brief.fixture_dir().join(rel)).expect("fixture file");
                ((*rel).to_owned(), bytes)
            })
            .collect();
        Guard { files }
    }

    fn greeter_check() -> Check {
        Check::CargoTest { min_passed: 1 }
    }

    /// A build script the attempt adds runs before its tests and can rewrite
    /// them; cargo finds `build.rs` with no manifest entry. The check refuses
    /// the tree and never builds it.
    #[test]
    fn an_added_build_script_makes_the_attempt_unsatisfied() {
        let scratch = tempfile::tempdir().expect("scratch");
        let env = env_in(scratch.path(), 240);
        let dir = greeter_with_trait();
        assert!(run(&greeter_check(), dir.path(), &env).expect("plain"));

        let marker = dir.path().join("build-ran");
        std::fs::write(
            dir.path().join("build.rs"),
            "fn main() { std::fs::write(\"build-ran\", \"x\").unwrap(); }\n",
        )
        .expect("write");
        assert!(!run(&greeter_check(), dir.path(), &env).expect("with build.rs"));
        assert!(!marker.exists(), "the build script ran");
    }

    /// A `.cargo/config.toml` `runner` runs the test binary through a program
    /// of the attempt's choosing, and can print any summary.
    #[test]
    fn an_added_cargo_config_makes_the_attempt_unsatisfied() {
        let scratch = tempfile::tempdir().expect("scratch");
        let env = env_in(scratch.path(), 240);
        let dir = greeter_with_trait();
        std::fs::create_dir_all(dir.path().join(".cargo")).expect("dir");
        std::fs::write(
            dir.path().join(".cargo/config.toml"),
            "[target.'cfg(all())']\nrunner = \"echo\"\n",
        )
        .expect("write");
        assert!(!run(&greeter_check(), dir.path(), &env).expect("with config"));
    }

    /// A test that rewrites a protected file while it runs makes the run
    /// void, whatever it prints.
    #[test]
    fn a_test_that_rewrites_a_protected_file_voids_the_run() {
        let scratch = tempfile::tempdir().expect("scratch");
        let env = env_in(scratch.path(), 240);
        let dir = greeter_with_trait();
        let guard = greeter_guard();
        assert!(run_guarded(&greeter_check(), dir.path(), &env, &guard).expect("plain"));

        std::fs::create_dir_all(dir.path().join("tests")).expect("dir");
        std::fs::write(
            dir.path().join("tests/rewrite.rs"),
            "#[test]\nfn rewrites() {\n    let p = concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/tests/consumer.rs\");\n    \
             std::fs::write(p, \"// emptied\\n\").unwrap();\n}\n",
        )
        .expect("write");
        assert!(
            !run_guarded(&greeter_check(), dir.path(), &env, &guard).expect("rewriting"),
            "a protected file changed during the run"
        );
    }

    #[test]
    fn the_guard_puts_protected_files_back_and_sees_a_change() {
        let dir = tempfile::tempdir().expect("dir");
        let guard = Guard {
            files: vec![("tests/t.rs".to_owned(), b"original".to_vec())],
        };
        assert!(!guard.intact(dir.path()), "a missing file is not intact");
        guard.restore(dir.path()).expect("restore");
        assert!(guard.intact(dir.path()));
        std::fs::write(dir.path().join("tests/t.rs"), "changed").expect("write");
        assert!(!guard.intact(dir.path()));
        guard.restore(dir.path()).expect("restore again");
        assert!(guard.intact(dir.path()));
        #[cfg(unix)]
        {
            std::fs::remove_file(dir.path().join("tests/t.rs")).expect("rm");
            std::os::unix::fs::symlink("/etc/hosts", dir.path().join("tests/t.rs")).expect("link");
            assert!(!guard.intact(dir.path()), "a link is not the file");
            guard.restore(dir.path()).expect("replace the link");
            assert!(guard.intact(dir.path()));
        }
    }
}
