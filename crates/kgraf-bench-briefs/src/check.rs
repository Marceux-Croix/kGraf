//! Running a brief's [`Check`] against a checked-out attempt.
//!
//! `Ok(false)` is a legitimate, expected outcome: the brief was not
//! satisfied (a test failed or never ran, a protected file changed, a script
//! printed the wrong thing, a doc file is missing a required fact). `Err` is
//! reserved for the check itself being unable to run (the sandbox, `cargo` or
//! `sh` could not be started, a path could not be read), a harness problem,
//! not a measurement.
//!
//! **The attempt's code is untrusted.** `cargo test` runs the worker's tests
//! and any `build.rs` it wrote, and a script check runs the worker's shell
//! script. Every such process runs through `kgraf_sandbox`: network off,
//! writes limited to the attempt directory and a scratch directory, a cleared
//! environment (only what a build needs), a wall-clock timeout, and a kill of
//! the whole process group at the timeout.

use crate::brief::{Brief, Check, DocFact, Mutation, ScriptCase};
use crate::gitutil;
use crate::structure;
use anyhow::{Context, Result, bail};
use kgraf_sandbox::{Network, Redirect, SandboxPolicy, sandboxed_command};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The wall-clock limit for one check process (a cold `cargo test` of a
/// fixture crate is well inside it).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// The most output kept from one check process. Cargo's test summary is a
/// few hundred bytes; more than this is not a test report.
const OUTPUT_CAP: u64 = 1 << 20;

/// Where and how long the check's processes run.
pub struct CheckEnv<'a> {
    /// A directory the check may write (build output, TMPDIR, the mutation
    /// copy); it must exist and lie outside the attempt directory.
    pub scratch: &'a Path,
    /// The wall-clock limit for each process.
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

/// Decide whether `attempt` satisfies `brief`: no protected path changed, the
/// brief's check holds, and (for a named test) the test fails on every
/// mutated copy of the fixture's function.
pub fn verify(brief: &Brief, attempt: &Attempt<'_>, env: &CheckEnv<'_>) -> Result<bool> {
    let touched =
        gitutil::changed_protected(attempt.repo, attempt.base, attempt.commit, brief.protected)?;
    if !touched.is_empty() {
        return Ok(false);
    }
    run(&brief.check, attempt.dir, env)
}

/// Run `check` against `dir` (the attempt's checked-out tree). See the module
/// docs for what `Ok(false)` versus `Err` means.
pub fn run(check: &Check, dir: &Path, env: &CheckEnv<'_>) -> Result<bool> {
    match check {
        Check::CargoTest { min_passed } => {
            let Some(ran) = cargo_test(dir, &["test"], env, "target")? else {
                return Ok(false);
            };
            Ok(ran.status_ok && ran.failed == 0 && ran.passed >= *min_passed)
        }
        Check::CargoTestUses {
            min_passed,
            file,
            function,
            helper,
            absent,
        } => {
            // The cheap structural read goes first: no build for an attempt
            // that never calls its helper.
            let source = match std::fs::read_to_string(dir.join(file)) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e).with_context(|| format!("cannot read {file}")),
            };
            if !structure::body_uses(&source, function, helper, absent) {
                return Ok(false);
            }
            let Some(ran) = cargo_test(dir, &["test"], env, "target")? else {
                return Ok(false);
            };
            Ok(ran.status_ok && ran.failed == 0 && ran.passed >= *min_passed)
        }
        Check::CargoTestNamed {
            test_bin,
            test_name,
            mutations,
        } => {
            if !named_test_passes(dir, test_bin, test_name, env, "target")? {
                return Ok(false);
            }
            for (i, mutation) in mutations.iter().enumerate() {
                if !named_test_fails_on_mutant(dir, test_bin, test_name, mutation, env, i)? {
                    return Ok(false);
                }
            }
            Ok(true)
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

/// Run `program args` in `dir` under the sandbox. `dir` and `env.scratch`
/// are the only writable roots; the network is off.
fn sandboxed(
    dir: &Path,
    program: &str,
    args: &[&str],
    env: &CheckEnv<'_>,
    target_name: &str,
) -> Result<Ran> {
    let target = env.scratch.join(target_name);
    std::fs::create_dir_all(&target)
        .with_context(|| format!("cannot create {}", target.display()))?;
    let vars = build_env(env.scratch, &target);
    let mut policy = SandboxPolicy::new(
        vec![dir.to_path_buf(), env.scratch.to_path_buf()],
        Network::Deny,
    )
    .with_tmp_dir(env.scratch.to_path_buf())
    .holding(env.scratch.join("hold"));
    for name in ["CARGO_HOME", "RUSTUP_HOME"] {
        if let Some((_, p)) = vars.iter().find(|(k, _)| *k == name)
            && p.is_dir()
        {
            policy = policy.with_readable_root(p.clone());
        }
    }
    let mut cmd = sandboxed_command(&policy, program, dir)
        .with_context(|| format!("cannot sandbox `{program}` in {}", dir.display()))?;
    #[cfg(unix)]
    cmd.command_mut().env_clear();
    for (k, v) in &vars {
        cmd.env(k, v);
    }
    // Builds must not reach a registry even if the sandbox's network rule
    // were ever bypassed; a fixture crate has no dependencies.
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
    // Stderr carries only compiler and panic text the checks do not read; it is
    // drained so the process never blocks on a full pipe.
    let _ = err_rx.recv_timeout(grace);
    Ok(Ran {
        status,
        stdout,
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

/// A `cargo test` run reduced to what the checks read.
struct CargoRun {
    status_ok: bool,
    /// Passed tests summed over every `test result:` line.
    passed: u64,
    failed: u64,
    stdout: String,
}

/// Sum the `N passed; M failed` counts of every `test result:` line.
fn count_results(stdout: &str) -> (u64, u64) {
    let (mut passed, mut failed) = (0, 0);
    for line in stdout.lines() {
        let Some(rest) = line.trim().strip_prefix("test result:") else {
            continue;
        };
        for part in rest.split(';') {
            let part = part.trim();
            let part = part.strip_prefix("ok.").unwrap_or(part);
            let part = part.strip_prefix("FAILED.").unwrap_or(part).trim();
            if let Some(n) = part.strip_suffix(" passed") {
                passed += n.trim().parse::<u64>().unwrap_or(0);
            } else if let Some(n) = part.strip_suffix(" failed") {
                failed += n.trim().parse::<u64>().unwrap_or(0);
            }
        }
    }
    (passed, failed)
}

/// `cargo <args>` in `dir`. `None` when there is no `Cargo.toml` (the worker
/// never made the crate: unsatisfied, not a harness error) or a process left
/// its group (unsatisfied: the tree is not quiet); a timeout is a run with
/// `status_ok == false`.
fn cargo_test(
    dir: &Path,
    args: &[&str],
    env: &CheckEnv<'_>,
    target_name: &str,
) -> Result<Option<CargoRun>> {
    if !dir.join("Cargo.toml").is_file() {
        return Ok(None);
    }
    let ran = sandboxed(dir, "cargo", args, env, target_name)?;
    if ran.escaped {
        return Ok(None);
    }
    let (passed, failed) = count_results(&ran.stdout);
    Ok(Some(CargoRun {
        status_ok: ran.status.is_some_and(|s| s.success()),
        passed,
        failed,
        stdout: ran.stdout,
    }))
}

fn named_args<'a>(test_bin: &'a str, test_name: &'a str) -> [&'a str; 6] {
    ["test", "--test", test_bin, "--", "--exact", test_name]
}

/// The named test ran, passed, and was the only test that did.
fn named_test_passes(
    dir: &Path,
    test_bin: &str,
    test_name: &str,
    env: &CheckEnv<'_>,
    target_name: &str,
) -> Result<bool> {
    let Some(ran) = cargo_test(dir, &named_args(test_bin, test_name), env, target_name)? else {
        return Ok(false);
    };
    let ok_line = format!("test {test_name} ... ok");
    Ok(ran.status_ok
        && ran.failed == 0
        && ran.passed == 1
        && ran.stdout.lines().any(|l| l.trim() == ok_line))
}

/// Apply `mutation` to a copy of `dir` and require the named test to run and
/// fail there (a `FAILED` line for it: a copy that no longer compiles proves
/// nothing about the test).
fn named_test_fails_on_mutant(
    dir: &Path,
    test_bin: &str,
    test_name: &str,
    mutation: &Mutation,
    env: &CheckEnv<'_>,
    index: usize,
) -> Result<bool> {
    let mutant = env.scratch.join(format!("mutant-{index}"));
    std::fs::create_dir_all(&mutant)
        .with_context(|| format!("cannot create {}", mutant.display()))?;
    gitutil::copy_tree(dir, &mutant, &["target", ".git"])?;
    let path = mutant.join(mutation.file);
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
    let Some(ran) = cargo_test(
        &mutant,
        &named_args(test_bin, test_name),
        env,
        &format!("target-mutant-{index}"),
    )?
    else {
        return Ok(false);
    };
    let failed_line = format!("test {test_name} ... FAILED");
    Ok(!ran.status_ok && ran.stdout.lines().any(|l| l.trim() == failed_line))
}

fn script_cases(
    dir: &Path,
    script: &str,
    cases: &[ScriptCase],
    env: &CheckEnv<'_>,
) -> Result<bool> {
    let path = dir.join(script);
    if !path.is_file() {
        return Ok(false);
    }
    for case in cases {
        let mut args = vec![script];
        args.extend_from_slice(case.args);
        let ran = sandboxed(dir, "sh", &args, env, "target")?;
        if ran.escaped {
            return Ok(false);
        }
        let Some(status) = ran.status else {
            return Ok(false);
        };
        let code = status.code().unwrap_or(-1);
        if ran.stdout != case.expect_stdout || code != case.expect_status {
            return Ok(false);
        }
    }
    Ok(true)
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

fn docs(dir: &Path, file: &str, required: &[DocFact], max_words: usize) -> Result<bool> {
    let path = dir.join(file);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    if text.split_whitespace().count() > max_words {
        return Ok(false);
    }
    Ok(required.iter().all(|fact| fact_holds(&text, fact)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brief::suite;

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

    #[test]
    fn libtest_summaries_are_summed() {
        let out = "running 1 test\ntest a ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; \
                   0 measured; 3 filtered out; finished in 0.00s\n\ntest result: FAILED. 0 passed; \
                   2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        assert_eq!(count_results(out), (1, 2));
        assert_eq!(count_results("no summary"), (0, 0));
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

    #[test]
    fn only_the_build_variables_are_passed() {
        let dir = tempfile::tempdir().expect("dir");
        let scratch = tempfile::tempdir().expect("scratch");
        std::fs::write(dir.path().join("r.sh"), "#!/bin/sh\nenv\n").expect("write");
        let env = env_in(scratch.path(), 30);
        let ran = sandboxed(dir.path(), "sh", &["r.sh"], &env, "target").expect("run");
        let names: Vec<&str> = ran
            .stdout
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, _)| k))
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
        ];
        for n in &names {
            assert!(allowed.contains(n), "unexpected variable {n} in {names:?}");
        }
        assert!(names.contains(&"CARGO_NET_OFFLINE"), "{names:?}");
        assert!(names.contains(&"CARGO_TARGET_DIR"), "{names:?}");
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
}
