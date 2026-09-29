//! The pre-dispatch contract checks and revision classification against real
//! temporary Rust and Python repositories: every command runs for real, inside the
//! sandbox (the mechanism, not a setting). No model is called here.

mod contract_support;

use contract_support::*;
use kgraf_orchestrator::{
    CheckError, CheckStage, FileOverlay, InterfaceContract, RevisionClass, WriteTarget,
    check_mechanical, classify, resolve_by_recompute,
};
// Only referenced by the `#[cfg(unix)]` rlimit test below (file-size/CPU rlimits are a
// unix-specific mechanism); a plain, unconditional import left this "unused" on a
// non-unix `--all-targets` build (a Windows cross-build under
// `-D warnings` found this).
#[cfg(unix)]
use kgraf_orchestrator::ResourceLimits;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ Rust

const RUST_STUB: &str = "pub struct Config { pub port: u16 }\n\
pub fn parse_config(_text: &str) -> Config { Config { port: 0 } }\n";

const RUST_REFERENCE: &str = "pub struct Config { pub port: u16 }\n\
pub fn parse_config(text: &str) -> Config {\n\
    let port = text.strip_prefix(\"port=\").and_then(|p| p.trim().parse().ok()).unwrap_or(0);\n\
    Config { port }\n\
}\n";

const RUST_WRONG: &str = "pub struct Config { pub port: u16 }\n\
pub fn parse_config(_text: &str) -> Config { Config { port: 80 } }\n";

const RUST_TEST: &str = "use cfgcrate::config::parse_config;\n\
#[test]\nfn port_is_parsed() { assert_eq!(parse_config(\"port=8080\").port, 8080); }\n";

const RUST_CONSUMER: &str = "use cfgcrate::config::{parse_config, Config};\n\
pub fn port_of(t: &str) -> u16 { let c: Config = parse_config(t); c.port }\n\
#[test]\nfn uses() { let _ = port_of(\"port=1\"); }\n";

fn rust_repo(root: &Path) -> std::path::PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).expect("mkdir");
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"cfgcrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .expect("write");
    std::fs::write(repo.join("src/lib.rs"), "pub mod config;\n").expect("write");
    commit_all(&repo);
    repo
}

fn rust_ic() -> InterfaceContract {
    InterfaceContract {
        id: "config".into(),
        version: 1,
        parties: s(&["prod", "cons"]),
        symbols: s(&["parse_config", "Config"]),
        stub: vec![FileOverlay::new("src/config.rs", RUST_STUB)],
        wrong: vec![FileOverlay::new("src/config.rs", RUST_WRONG)],
        reference: vec![FileOverlay::new("src/config.rs", RUST_REFERENCE)],
        consumer: vec![FileOverlay::new("tests/consumer.rs", RUST_CONSUMER)],
        tests: vec![FileOverlay::new("tests/contract.rs", RUST_TEST)],
        compile: s(&["cargo", "test", "--no-run", "--offline", "--quiet"]),
        test: s(&[
            "cargo",
            "test",
            "--offline",
            "--quiet",
            "--test",
            "contract",
        ]),
        consulted: vec![],
        replaces: vec![],
    }
}

fn writes() -> BTreeMap<String, Vec<WriteTarget>> {
    let w = |p: &str| WriteTarget {
        path: p.into(),
        node: None,
    };
    [
        ("prod".to_owned(), vec![w("src/config.rs"), w("config.py")]),
        (
            "cons".to_owned(),
            vec![w("tests/consumer.rs"), w("consumer.py")],
        ),
    ]
    .into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sound_rust_contract_passes_every_stage_in_order_and_leaves_the_base_untouched() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = rust_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let runs = check_mechanical(&r, &rust_ic())
        .await
        .expect("ran")
        .expect("passes");
    let order: Vec<(CheckStage, &str, bool)> = runs
        .iter()
        .map(|x| (x.stage, x.against.as_str(), x.success))
        .collect();
    assert_eq!(
        order,
        vec![
            (CheckStage::StubCompile, "stub", true),
            (CheckStage::StubCompile, "canary test", false),
            (CheckStage::StubCompile, "canary consumer", false),
            (CheckStage::Vacuity, "stub", false),
            (CheckStage::Vacuity, "wrong compile", true),
            (CheckStage::Vacuity, "wrong", false),
            (CheckStage::Vacuity, "reference", true),
        ]
    );
    assert!(runs.iter().all(|x| x.expected));
    assert!(
        !repo.join("src/config.rs").exists() && !repo.join("tests").exists(),
        "the checks ran on copies; the base holds no stub"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_consumer_expecting_another_signature_fails_stub_compile_naming_the_files() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = rust_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let mut ic = rust_ic();
    ic.consumer = vec![FileOverlay::new(
        "tests/consumer.rs",
        "use cfgcrate::config::parse_config;\n\
         pub fn port_of(t: &str) -> u16 { parse_config(t).map(|c| c.port).unwrap_or(0) }\n",
    )];
    let f = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect_err("fails");
    assert_eq!(f.stage, CheckStage::StubCompile);
    assert!(
        f.artifact.contains("config@v1")
            && f.artifact.contains("tests/consumer.rs")
            && f.artifact.contains("src/config.rs")
            && f.artifact.contains("cargo test --no-run"),
        "{}",
        f.artifact
    );
    // The compiler's own words, not a guess.
    assert!(f.detail.contains("error"), "{}", f.detail);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_test_that_asserts_nothing_fails_vacuity_against_the_stub() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = rust_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let mut ic = rust_ic();
    ic.tests = vec![FileOverlay::new(
        "tests/contract.rs",
        "use cfgcrate::config::parse_config;\n\
         #[test]\nfn runs() { let _ = parse_config(\"port=8080\"); }\n",
    )];
    let f = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect_err("fails");
    assert_eq!(f.stage, CheckStage::Vacuity);
    assert!(
        f.detail.contains("passes against the stub producer"),
        "{}",
        f.detail
    );
    assert!(f.artifact.contains("tests/contract.rs"), "{}", f.artifact);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_test_nothing_can_satisfy_fails_against_the_reference() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = rust_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let mut ic = rust_ic();
    ic.tests = vec![FileOverlay::new(
        "tests/contract.rs",
        "use cfgcrate::config::parse_config;\n\
         #[test]\nfn port() { assert_eq!(parse_config(\"port=8080\").port, 9090); }\n",
    )];
    let f = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect_err("fails");
    assert_eq!(f.stage, CheckStage::Vacuity);
    assert!(
        f.detail.contains("fails against the reference"),
        "{}",
        f.detail
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wrong_stub_catches_a_test_that_only_catches_a_placeholder() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = rust_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let mut ic = rust_ic();
    ic.tests = vec![FileOverlay::new(
        "tests/contract.rs",
        "use cfgcrate::config::parse_config;\n\
         #[test]\nfn port() { assert_ne!(parse_config(\"port=8080\").port, 0); }\n",
    )];
    let f = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect_err("fails");
    assert_eq!(f.stage, CheckStage::Vacuity);
    assert!(
        f.detail.contains("passes against the wrong producer"),
        "{}",
        f.detail
    );
    // Without the wrong stub the same weak test passes: the optional half of 0031
    // is what caught it.
    ic.wrong.clear();
    check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect("passes without the wrong stub");
}

// ------------------------------------------------------------------ revisions

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_additive_revision_is_an_amend_because_the_prior_test_still_holds() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = rust_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let prior = rust_ic();
    let mut revised = rust_ic();
    revised.version = 2;
    revised.symbols.push("DEFAULT_PORT".into());
    let extra = "pub const DEFAULT_PORT: u16 = 8080;\n";
    revised.stub[0].content = format!("{RUST_STUB}{extra}");
    revised.reference[0].content = format!("{RUST_REFERENCE}{extra}");
    revised.wrong[0].content = format!("{RUST_WRONG}{extra}");
    let c = classify(&r, &prior, &revised, &writes(), 0, 2)
        .await
        .expect("ran")
        .expect("valid");
    assert_eq!(c.class, RevisionClass::Amend, "{}", c.detail);
    assert!(c.runs.iter().any(|x| x.against.contains("prior test")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_signature_is_a_supersede_and_past_the_cap_a_re_decompose() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = rust_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let prior = rust_ic();
    let mut revised = rust_ic();
    revised.version = 2;
    let result_api = |body: &str| {
        format!(
            "pub struct Config {{ pub port: u16 }}\n\
             pub fn parse_config(text: &str) -> Result<Config, String> {{ {body} }}\n"
        )
    };
    revised.stub[0].content = result_api("let _ = text; Ok(Config { port: 0 })");
    revised.reference[0].content = result_api(
        "text.strip_prefix(\"port=\").and_then(|p| p.parse().ok()).map(|port| Config { port }).ok_or_else(|| text.to_owned())",
    );
    revised.wrong[0].content = result_api("let _ = text; Ok(Config { port: 80 })");
    revised.tests[0].content = "use cfgcrate::config::parse_config;\n\
        #[test]\nfn port() { assert_eq!(parse_config(\"port=8080\").map(|c| c.port), Ok(8080)); }\n"
        .into();
    revised.consumer[0].content = "use cfgcrate::config::parse_config;\n\
        pub fn port_of(t: &str) -> u16 { parse_config(t).map(|c| c.port).unwrap_or(0) }\n"
        .into();
    let c = classify(&r, &prior, &revised, &writes(), 0, 2)
        .await
        .expect("ran")
        .expect("valid");
    assert_eq!(c.class, RevisionClass::Supersede, "{}", c.detail);
    assert!(
        c.detail.contains("do not compile against the revised stub"),
        "{}",
        c.detail
    );
    // The same breaking revision on a lineage already superseded twice.
    let c = classify(&r, &prior, &revised, &writes(), 2, 2)
        .await
        .expect("ran")
        .expect("valid");
    assert_eq!(c.class, RevisionClass::Redecompose, "{}", c.detail);
    assert!(c.detail.contains("superseded 2 times"), "{}", c.detail);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_behavior_change_with_the_same_signature_is_a_supersede() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = rust_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let prior = rust_ic();
    let mut revised = rust_ic();
    revised.version = 2;
    // Same types; the format is now `port:N`.
    revised.reference[0].content = RUST_REFERENCE.replace("port=", "port:");
    revised.tests[0].content = RUST_TEST.replace("port=8080", "port:8080");
    let c = classify(&r, &prior, &revised, &writes(), 0, 2)
        .await
        .expect("ran")
        .expect("valid");
    assert_eq!(c.class, RevisionClass::Supersede, "{}", c.detail);
    assert!(
        c.detail.contains("fails against the revised reference"),
        "{}",
        c.detail
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revision_failing_its_own_checks_is_refused_and_a_straddle_re_decomposes() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = rust_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let prior = rust_ic();
    let mut revised = rust_ic();
    revised.version = 2;
    revised.tests[0].content = "#[test]\nfn nothing() {}\n".into();
    let f = classify(&r, &prior, &revised, &writes(), 0, 2)
        .await
        .expect("ran")
        .expect_err("refused");
    assert_eq!(f.stage, CheckStage::Vacuity);
    assert!(f.artifact.contains("config@v2"), "{}", f.artifact);

    let mut straddle = writes();
    straddle.get_mut("cons").expect("cons").push(WriteTarget {
        path: "src/config.rs".into(),
        node: None,
    });
    let c = classify(&r, &prior, &rust_ic_v(2), &straddle, 0, 2)
        .await
        .expect("ran")
        .expect("classified");
    assert_eq!(c.class, RevisionClass::Redecompose);
    assert!(c.runs.is_empty(), "decided before any command ran");

    let same = classify(&r, &prior, &rust_ic(), &writes(), 0, 2)
        .await
        .expect("ran")
        .expect_err("same version refused");
    assert_eq!(same.stage, CheckStage::Artifact);
}

fn rust_ic_v(version: u32) -> InterfaceContract {
    InterfaceContract {
        version,
        ..rust_ic()
    }
}

// ------------------------------------------------------------------ Python

const PY_STUB: &str = "class Config:\n    def __init__(self, port):\n        self.port = port\n\n\
def parse_config(text):\n    return Config(0)\n";

const PY_REFERENCE: &str = "class Config:\n    def __init__(self, port):\n        self.port = port\n\n\
def parse_config(text):\n    return Config(int(text.split('=', 1)[1]))\n";

const PY_TEST: &str = "import unittest\nfrom config import parse_config\n\n\
class T(unittest.TestCase):\n    def test_port(self):\n        self.assertEqual(parse_config('port=8080').port, 8080)\n";

fn py_repo(root: &Path) -> std::path::PathBuf {
    let repo = root.join("pyrepo");
    std::fs::create_dir_all(&repo).expect("mkdir");
    std::fs::write(repo.join("README.md"), "python project\n").expect("write");
    commit_all(&repo);
    repo
}

fn py_ic() -> InterfaceContract {
    InterfaceContract {
        id: "config".into(),
        version: 1,
        parties: s(&["prod", "cons"]),
        symbols: s(&["parse_config", "Config"]),
        stub: vec![FileOverlay::new("config.py", PY_STUB)],
        wrong: vec![],
        reference: vec![FileOverlay::new("config.py", PY_REFERENCE)],
        consumer: vec![FileOverlay::new(
            "consumer.py",
            "from config import parse_config, Config\n",
        )],
        tests: vec![FileOverlay::new("test_contract.py", PY_TEST)],
        compile: s(&["python3", "-c", "import consumer, test_contract"]),
        test: s(&["python3", "-m", "unittest", "-q", "test_contract"]),
        consulted: vec![],
        replaces: vec![],
    }
}

/// M6 measurement, not a check: the wall time of the mechanical checks of one
/// interface, cold (a fresh runner, so a fresh archive and a fresh target per
/// run, as a live run pays), on the small real Rust crate of these tests and
/// on the Python project. Run with
/// `cargo test -p kgraf-orchestrator --test contract_checks -- --ignored --nocapture measure_`.
/// The numbers are not asserted; record them from the output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a measurement: run it by hand and record the numbers"]
async fn measure_the_check_cost_of_one_interface() {
    // The fixed cost of one sandboxed run: tree, overlay, sandbox, `true`.
    let dir = tempfile::tempdir().expect("dir");
    let repo = py_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let mut fixed = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        assert!(r.run(&[], &s(&["true"])).await.expect("ran").success);
        fixed.push(t.elapsed());
    }
    println!("MEASURE fixed cost of one run (`true`): {fixed:?}");
    for (lang, make, ic) in [
        (
            "rust",
            rust_repo as fn(&Path) -> std::path::PathBuf,
            rust_ic(),
        ),
        (
            "python",
            py_repo as fn(&Path) -> std::path::PathBuf,
            py_ic(),
        ),
    ] {
        let mut times = Vec::new();
        let mut run_count = 0;
        for _ in 0..3 {
            let dir = tempfile::tempdir().expect("dir");
            let repo = make(dir.path());
            let r = runner(dir.path(), &repo);
            let t = Instant::now();
            let runs = check_mechanical(&r, &ic)
                .await
                .expect("ran")
                .expect("passes");
            times.push(t.elapsed());
            run_count = runs.len();
        }
        println!(
            "MEASURE {lang}: {run_count} sandboxed runs per interface; wall {:?}",
            times
        );
    }
}

/// M2: check runs queue behind one process-wide bound. One more run than the
/// bound, each sleeping a second, takes at least two seconds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn check_runs_beyond_the_process_bound_queue() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = py_repo(dir.path());
    let r = std::sync::Arc::new(runner(dir.path(), &repo));
    let n = kgraf_orchestrator::max_concurrent_checks() + 1;
    let started = Instant::now();
    let mut runs = Vec::new();
    for _ in 0..n {
        let r = r.clone();
        runs.push(tokio::spawn(async move {
            r.run(&[], &s(&["sleep", "1"])).await.expect("ran")
        }));
    }
    for run in runs {
        assert!(run.await.expect("join").success);
    }
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "{n} runs finished in {:?}: the bound did not hold",
        started.elapsed()
    );
}

/// H1: a folder symlink committed in the base (`tests -> <outside>`) never
/// carries an overlay out of the check tree. The overlay is refused, so the
/// check could not run (no verdict on the contract), and nothing is written
/// outside.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_committed_folder_symlink_never_carries_an_overlay_outside_the_tree() {
    let dir = tempfile::tempdir().expect("dir");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).expect("mkdir");
    let repo = dir.path().join("pyrepo");
    std::fs::create_dir_all(&repo).expect("mkdir");
    std::fs::write(repo.join("README.md"), "python project\n").expect("write");
    std::os::unix::fs::symlink(&outside, repo.join("tests")).expect("link");
    commit_all(&repo);
    let r = runner(dir.path(), &repo);
    let mut ic = py_ic();
    ic.tests = vec![FileOverlay::new("tests/test_contract.py", PY_TEST)];
    let e = check_mechanical(&r, &ic)
        .await
        .expect_err("an overlay through a committed link is refused");
    assert!(
        e.to_string().contains("crosses the symbolic link tests"),
        "{e}"
    );
    assert_eq!(
        std::fs::read_dir(&outside).expect("read").count(),
        0,
        "nothing was written outside the tree"
    );
}

/// A compile or test program that is not installed is a check that could not
/// run, naming the program, never a stub-compile failure of the contract: a
/// sandboxed exec of a missing program fails like any failing command, which
/// read as "does not compile against the stub" before the runner resolved it.
/// A relative program resolves against the check tree, as the child runs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_program_this_machine_lacks_is_not_run_not_a_contract_failure() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = py_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let mut ic = py_ic();
    ic.compile = s(&["kgraf-no-such-compiler", "--check"]);
    let e = check_mechanical(&r, &ic)
        .await
        .expect_err("a check that cannot run is no verdict");
    assert!(
        matches!(&e, CheckError::ProgramMissing { program, .. } if program == "kgraf-no-such-compiler"),
        "{e}"
    );

    // H4: wrapped in a shell, the program is found only by running it, in
    // the sandbox; exit 127 there is a missing program, named.
    let mut ic = py_ic();
    ic.compile = s(&["sh", "-c", "kgraf-no-such-compiler --check"]);
    let e = check_mechanical(&r, &ic)
        .await
        .expect_err("a wrapped missing program is no verdict");
    assert!(
        matches!(&e, CheckError::ProgramMissing { program, .. } if program == "kgraf-no-such-compiler"),
        "{e}"
    );

    // H4: a macOS shim with no Command Line Tools behind it exists on PATH and
    // runs, and says so; simulated here, since the test machine has them.
    let mut ic = py_ic();
    ic.compile = s(&[
        "sh",
        "-c",
        "echo 'xcrun: error: invalid active developer path (/Library/Developer/CommandLineTools), \
         missing xcrun at: /Library/Developer/CommandLineTools/usr/bin/xcrun' >&2; exit 1",
    ]);
    let e = check_mechanical(&r, &ic)
        .await
        .expect_err("a shim with nothing behind it is no verdict");
    assert!(
        matches!(&e, CheckError::ProgramMissing { detail, .. } if detail.contains("invalid active developer path")),
        "{e}"
    );

    let mut ic = py_ic();
    ic.test = s(&["./run-tests.sh"]);
    let e = check_mechanical(&r, &ic)
        .await
        .expect_err("a relative program missing from the tree");
    assert!(
        matches!(&e, CheckError::ProgramMissing { detail, .. } if detail.contains("run-tests.sh")),
        "{e}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_a_sound_contract_passes_and_a_missing_name_fails_stub_compile() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = py_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let runs = check_mechanical(&r, &py_ic())
        .await
        .expect("ran")
        .expect("passes");
    assert_eq!(runs.len(), 5, "stub-compile, two canaries, stub, reference");

    let mut ic = py_ic();
    ic.consumer[0].content = "from config import load_config\n".into();
    let f = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect_err("fails");
    assert_eq!(f.stage, CheckStage::StubCompile);
    assert!(f.detail.contains("ImportError"), "{}", f.detail);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_a_hang_against_the_stub_is_a_failure_not_a_pass() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = py_repo(dir.path());
    // The test runs are bounded at 10 s, the compiles separately and generously:
    // under a loaded machine a cold compile timed out at 10 s and was misreported
    // as a stub-compile failure (the flake this separation fixes).
    let r = runner_with(
        dir.path(),
        &repo,
        vec![state_dir(dir.path())],
        Duration::from_secs(10),
    )
    .with_compile_timeout(Duration::from_secs(60));
    let mut ic = py_ic();
    ic.stub[0].content = PY_STUB.replace("return Config(0)", "while True:\n        pass");
    let started = Instant::now();
    let f = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect_err("fails");
    assert_eq!(f.stage, CheckStage::Vacuity);
    assert!(f.detail.contains("timed out"), "{}", f.detail);
    // Compiles (up to 60 s, retried once each) plus one 10 s test timeout: bounded,
    // not hung.
    assert!(started.elapsed() < Duration::from_secs(480));
}

/// A wrong-producer compile that times out is inconclusive too:
/// it runs under the compile bound, is retried, and is a check not run, not a
/// vacuity failure of the contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_producer_compile_timeout_is_inconclusive_not_a_vacuity_failure() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = py_repo(dir.path());
    // The bound must cover the stub and canary compiles on a loaded machine: at 5 s
    // they timed out first under a load average of about 35, so the test saw
    // "stub" rather than "wrong". The wrong producer sleeps 600 s, so 30 s still
    // times it out.
    let r = runner(dir.path(), &repo).with_compile_timeout(Duration::from_secs(30));
    let mut ic = py_ic();
    // Importing the wrong producer hangs; the stub and the canaries compile fast.
    ic.wrong = vec![FileOverlay::new(
        "config.py",
        format!("import time\ntime.sleep(600)\n{PY_STUB}"),
    )];
    match check_mechanical(&r, &ic).await {
        Err(CheckError::CompileTimedOut { what, .. }) => assert_eq!(what, "wrong"),
        other => panic!("expected an inconclusive compile timeout, got {other:?}"),
    }
}

/// A stub compile that times out is inconclusive: retried, then reported as a
/// check that could not run, never as a stub-compile failure of the contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stub_compile_timeout_is_inconclusive_not_a_contract_failure() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = py_repo(dir.path());
    let r = runner(dir.path(), &repo).with_compile_timeout(Duration::from_millis(1));
    match check_mechanical(&r, &py_ic()).await {
        Err(CheckError::CompileTimedOut { what, .. }) => assert_eq!(what, "stub"),
        other => panic!("expected an inconclusive compile timeout, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_a_check_cannot_write_outside_its_tree() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = py_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let outside = dir.path().join("outside.txt");
    let mut ic = py_ic();
    // The test first tries to write next to the repository, then asserts.
    ic.tests[0].content = format!(
        "import unittest\nfrom config import parse_config\n\n\
         class T(unittest.TestCase):\n    def test_port(self):\n        \
         try:\n            open({:?}, 'w').write('x')\n        except OSError:\n            pass\n        \
         self.assertEqual(parse_config('port=8080').port, 8080)\n",
        outside.to_string_lossy()
    );
    check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect("passes");
    assert!(!outside.exists(), "the sandbox confined the check's writes");
}

// ------------------------------------------------------------------ review probes

fn sh(script: &str) -> Vec<String> {
    s(&["/bin/sh", "-c", script])
}

fn shell_repo(root: &Path) -> std::path::PathBuf {
    let repo = root.join("shrepo");
    std::fs::create_dir_all(repo.join("src")).expect("mkdir");
    std::fs::write(repo.join("src/x"), "x").expect("write");
    // A nested `target` that is not ignored is part of the base.
    std::fs::create_dir_all(repo.join("sub/target")).expect("mkdir");
    std::fs::write(repo.join("sub/target/keep.txt"), "kept").expect("write");
    commit_all(&repo);
    repo
}

/// Probe P1: a backgrounded descendant held the output pipe, so the run waited
/// for it (or forever), whatever the timeout.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p1_a_backgrounded_descendant_neither_stalls_the_run_nor_outlives_it() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = shell_repo(dir.path());
    let r = runner_with(
        dir.path(),
        &repo,
        vec![state_dir(dir.path())],
        Duration::from_secs(3),
    );
    // Measured from the moment the run took its check slot: the wait behind
    // the process-wide bound (M2) is reported apart, so the bound on the run
    // itself stays tight. Under the 3-second check bound: the run did not wait
    // for the background sleep's 20 seconds.
    let t = Instant::now();
    let out = r
        .run(&[], &sh("sleep 20 & echo $!; exit 1"))
        .await
        .expect("ran");
    let ran = t.elapsed().saturating_sub(out.queued);
    println!("p1: queued {:?}, ran {ran:?}", out.queued);
    assert!(
        ran < Duration::from_secs(3),
        "ran {ran:?}, queued {:?}",
        out.queued
    );
    assert!(!out.success && !out.timed_out, "{out:?}");
    // Probed by pid only where the pid is the host's. In a PID namespace
    // (Linux, where the kernel allows one) the check printed its namespace
    // pid, which the host must never signal, not even with signal 0 (as root
    // in a container it names an unrelated process); the kernel kills the
    // background sleep with the namespace there.
    #[cfg(target_os = "linux")]
    let own_pids = !kgraf_sandbox::linux_confinement().namespaces;
    #[cfg(not(target_os = "linux"))]
    let own_pids = true;
    if own_pids {
        let pid = out.tail.trim().to_owned();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid])
            .status()
            .expect("kill")
            .success();
        assert!(
            !alive,
            "the background sleep {pid} was killed with the group"
        );
    }

    // A foreground hang ends at the timeout.
    let t = Instant::now();
    let out = r.run(&[], &sh("sleep 20")).await.expect("ran");
    assert!(out.timed_out && !out.success);
    let ran = t.elapsed().saturating_sub(out.queued);
    println!("p1 hang: queued {:?}, ran {ran:?}", out.queued);
    // The 3-second bound, then the group kill's grace at most.
    assert!(
        ran < Duration::from_secs(9),
        "ran {ran:?}, queued {:?}",
        out.queued
    );
}

/// Whether a check runs as PID 1 of its own PID namespace (Linux, where the
/// kernel allows one). There an escaped descendant cannot outlive the check:
/// when PID 1 exits the kernel kills everything left in the namespace. Its pid
/// is the namespace's, so the host never signals it, not even with signal 0.
#[cfg(target_os = "linux")]
fn check_has_its_own_pid_namespace() -> bool {
    kgraf_sandbox::linux_confinement().namespaces
}

#[cfg(not(target_os = "linux"))]
fn check_has_its_own_pid_namespace() -> bool {
    false
}

/// With a PID namespace: the check's escaped descendant (it would sleep 8 s)
/// died with PID 1, so the run ends at once as a normal result. `Ok` is
/// itself the proof that no process held the run's hold file when the check
/// ended (`hold_free`, which the escapee's inherited lock would fail); ending
/// before the escapee's 8-second sleep says the runner did not wait it out.
/// The bound leaves room for a loaded machine (the 5-second kill grace plus
/// start-up); on an idle one the run takes well under a second.
#[cfg(unix)]
async fn assert_the_namespace_ended_the_escapee(
    r: &kgraf_orchestrator::SandboxRunner,
    argv: &[String],
) -> kgraf_orchestrator::RunResult {
    let t = Instant::now();
    let out = r
        .run(&[], argv)
        .await
        .expect("the escapee died with the namespace; the check ends normally");
    let ran = t.elapsed().saturating_sub(out.queued);
    assert!(out.success && !out.timed_out, "{out:?}");
    assert!(ran < Duration::from_secs(7), "ran {ran:?}: {out:?}");
    out
}

/// M5: a descendant that leaves the process group with its own `setsid` is
/// not reached by the group kill, but it still holds the run's hold file, so
/// the check ends as one that could not run, never as a quiet tree. The
/// descendant sleeps past the kill grace (5 s) and then exits by itself.
///
/// Linux with a PID namespace: the kernel kills the descendant when the check
/// (PID 1) exits, so the tree is quiet and the check ends normally.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_descendant_that_leaves_the_group_is_seen_through_the_hold() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = shell_repo(dir.path());
    let r = runner_with(
        dir.path(),
        &repo,
        vec![state_dir(dir.path())],
        Duration::from_secs(30),
    );
    let escape = "import os, time\nif os.fork() == 0:\n    os.setsid()\n    time.sleep(8)\n";
    if check_has_its_own_pid_namespace() {
        let _ = assert_the_namespace_ended_the_escapee(&r, &s(&["python3", "-c", escape])).await;
        return;
    }
    let e = r
        .run(&[], &s(&["python3", "-c", escape]))
        .await
        .expect_err("a tree that is not quiet is no result");
    assert!(e.to_string().contains("left its process group"), "{e}");
}

/// N1, the verifier's probe: the check's forked child calls `setsid` and
/// sleeps, and the parent deletes and recreates the hold file where it can
/// reach (`../hold`, and any `*.hold` above that). The hold file lives outside
/// the check's writable root, so the escaped child's lock stays visible.
///
/// Linux with a PID namespace: the escaped child dies with the check (PID 1),
/// and the hold the parent tried to recreate is outside its writable root, so
/// the run ends normally with nothing left running.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_check_cannot_hide_an_escaped_child_by_recreating_the_hold() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = shell_repo(dir.path());
    let r = runner_with(
        dir.path(),
        &repo,
        vec![state_dir(dir.path())],
        Duration::from_secs(30),
    );
    let probe = [
        "import os, time, glob",
        "if os.fork() == 0:",
        "    os.setsid()",
        "    time.sleep(8)",
        "    os._exit(0)",
        "time.sleep(0.3)",
        "def ino(p):",
        "    try:",
        "        return os.stat(p).st_ino",
        "    except OSError:",
        "        return 0",
        // Named as well as globbed: listing the scratch directory is refused
        // on Linux, so the glob alone finds nothing there.
        "own = os.path.abspath('..') + '.hold'",
        "for p in ['../hold', own] + [g for g in glob.glob('../../*.hold') if os.path.abspath(g) != own]:",
        "    before = ino(p)",
        "    try:",
        "        os.remove(p)",
        "    except OSError:",
        "        pass",
        "    try:",
        "        open(p, 'w').close()",
        "    except OSError:",
        "        pass",
        "    if p.endswith('.hold'):",
        "        print('HOLD', before, ino(p), flush=True)",
    ]
    .join("\n");
    if check_has_its_own_pid_namespace() {
        let out = assert_the_namespace_ended_the_escapee(&r, &s(&["python3", "-c", &probe])).await;
        // The check found its own hold (a `stat`, which Landlock allows) and
        // could neither remove nor replace it: the same inode after the try.
        let holds: Vec<&str> = out
            .tail
            .lines()
            .filter(|l| l.starts_with("HOLD "))
            .collect();
        assert!(!holds.is_empty(), "the probe found no hold: {out:?}");
        for line in holds {
            let ino: Vec<&str> = line.split_whitespace().skip(1).collect();
            assert!(
                ino.len() == 2 && ino[0] != "0" && ino[0] == ino[1],
                "the hold was replaced: {line}"
            );
        }
        return;
    }
    let e = r
        .run(&[], &s(&["python3", "-c", &probe]))
        .await
        .expect_err("the escaped child is seen");
    assert!(e.to_string().contains("left its process group"), "{e}");
}

/// A `.cargo/config.toml` above the check's
/// scratch, as another user could plant one in a shared `/tmp`, names a
/// `runner` that would report every test green. The check policy reads no
/// cargo file above its tree, so cargo fails loud: the planted runner never
/// runs and the failing test is never reported as passing. The first
/// ancestor-read version read it and forged the pass.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cargo_config_planted_above_the_check_scratch_is_never_obeyed() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).expect("mkdir");
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .expect("write");
    std::fs::write(
        repo.join("src/lib.rs"),
        "#[test]\nfn fails() { panic!(\"real failure\"); }\n",
    )
    .expect("write");
    commit_all(&repo);
    let marker = dir.path().join("pwned");
    std::fs::create_dir_all(dir.path().join(".cargo")).expect("mkdir");
    std::fs::write(
        dir.path().join(".cargo/config.toml"),
        format!(
            "[target.'cfg(all())']\nrunner = [\"/bin/sh\", \"-c\", \"echo PWNED > {}; exit 0\"]\n",
            marker.display()
        ),
    )
    .expect("write");
    let r = runner(dir.path(), &repo);
    let out = r
        .run(&[], &s(&["cargo", "test", "--offline"]))
        .await
        .expect("ran");
    assert!(!out.success, "the failing test was reported green: {out:?}");
    assert!(!marker.exists(), "the planted runner ran: {out:?}");
    assert!(
        out.tail.contains("config.toml") && out.tail.contains("Permission denied"),
        "cargo did not fail loud on the planted file: {out:?}"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_is_bounded_and_a_large_file_is_refused_by_rlimit() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = shell_repo(dir.path());
    let r = runner_with(
        dir.path(),
        &repo,
        vec![state_dir(dir.path())],
        Duration::from_secs(30),
    )
    .with_limits(ResourceLimits {
        file_size_bytes: 1_000_000,
        cpu_seconds: 30,
    });
    let out = r
        .run(
            &[],
            &sh("head -c 400000 /dev/zero | tr '\\0' 'a'; echo END"),
        )
        .await
        .expect("ran");
    assert!(out.success, "{out:?}");
    assert!(out.tail.len() <= 2 * kgraf_orchestrator::DEFAULT_TAIL_BYTES + 1);
    assert!(out.tail.ends_with("END"), "the last bytes are kept");
    let out = r
        .run(&[], &sh("head -c 5000000 /dev/zero > big && echo WROTE"))
        .await
        .expect("ran");
    assert!(!out.success && !out.tail.contains("WROTE"), "{out:?}");
}

/// Probe P2: a test with no assertion passed vacuity, because its failure against
/// the stub was a build error (an undefined name) and nothing compiled it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p2_a_compile_command_that_builds_nothing_fails_the_canary() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = shell_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let ic = InterfaceContract {
        id: "i".into(),
        version: 1,
        parties: s(&["a", "b"]),
        symbols: s(&["provide"]),
        stub: vec![FileOverlay::new("src/api.sh", "# stub\n")],
        wrong: vec![],
        reference: vec![FileOverlay::new("src/api.sh", "provide() { :; }\n")],
        consumer: vec![],
        tests: vec![FileOverlay::new("t.sh", ". ./src/api.sh; provide\n")],
        compile: s(&["/usr/bin/true"]),
        test: sh(". ./t.sh"),
        consulted: vec![],
        replaces: vec![],
    };
    let f = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect_err("fails");
    assert_eq!(f.stage, CheckStage::StubCompile);
    assert!(f.detail.contains("canary"), "{}", f.detail);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_test_that_does_not_build_against_the_wrong_stub_fails_vacuity() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = rust_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let mut ic = rust_ic();
    // The wrong stub lacks `Config`: the test's failure there would be a build error.
    ic.wrong[0].content = "pub fn parse_config(_text: &str) -> u16 { 80 }\n".into();
    let f = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect_err("fails");
    assert_eq!(f.stage, CheckStage::Vacuity);
    assert!(
        f.detail
            .contains("does not build against the wrong producer"),
        "{}",
        f.detail
    );
}

/// Probes P7 to P9: a check read kGraf's state, and wrote into the shared
/// scratch and build directory other checks used.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p7_p9_a_check_cannot_read_state_or_touch_another_checks_files() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = shell_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let secret = dir.path().join("state").join("secret.txt");
    let out = r
        .run(&[], &sh(&format!("cat '{}'", secret.display())))
        .await
        .expect("ran");
    assert!(
        !out.success && !out.tail.contains("ledger secret"),
        "{out:?}"
    );

    let scratch = dir.path().join("scratch");
    let out = r
        .run(&[], &sh("echo poison > \"$PWD/../../poison\" && echo ok"))
        .await
        .expect("ran");
    assert!(!out.success, "{out:?}");
    assert!(!scratch.join("poison").exists());

    let out = r
        .run(&[], &sh("echo p > \"$CARGO_TARGET_DIR/poison\" && echo ok"))
        .await
        .expect("ran");
    assert!(out.success, "a check writes its own target: {out:?}");
    let out = r
        .run(&[], &sh("test ! -e \"$CARGO_TARGET_DIR/poison\""))
        .await
        .expect("ran");
    assert!(out.success, "the next check does not see it: {out:?}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tree_is_the_base_commit_not_the_working_directory() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = shell_repo(dir.path());
    std::fs::write(repo.join("src/uncommitted.txt"), "draft").expect("write");
    let r = runner(dir.path(), &repo);
    let out = r
        .run(
            &[],
            &sh("test -e src/x && test -e sub/target/keep.txt && test ! -e src/uncommitted.txt"),
        )
        .await
        .expect("ran");
    assert!(out.success, "{out:?}");
    assert!(r.base_commit().is_some_and(|c| c.len() >= 40));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revision_that_demands_a_new_function_is_a_supersede() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = rust_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let prior = rust_ic();
    let mut revised = rust_ic();
    revised.version = 2;
    revised.symbols.push("default_port".into());
    let extra = "pub fn default_port() -> u16 { 8080 }\n";
    let stub_extra = "pub fn default_port() -> u16 { 0 }\n";
    revised.stub[0].content = format!("{RUST_STUB}{stub_extra}");
    revised.wrong[0].content = format!("{RUST_WRONG}{stub_extra}");
    revised.reference[0].content = format!("{RUST_REFERENCE}{extra}");
    revised.tests[0].content = format!(
        "{RUST_TEST}#[test]\nfn default() {{ assert_eq!(cfgcrate::config::default_port(), 8080); }}\n"
    );
    let c = classify(&r, &prior, &revised, &writes(), 0, 2)
        .await
        .expect("ran")
        .expect("valid");
    assert_eq!(c.class, RevisionClass::Supersede, "{}", c.detail);
    assert!(
        c.detail
            .contains("revised test of config@v2 fails against the prior reference"),
        "{}",
        c.detail
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recompute_that_disagrees_with_itself_is_flaky_not_an_overturn() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = py_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let mut ic = py_ic();
    // The reference is right half the time.
    ic.reference[0].content = format!(
        "{PY_REFERENCE}\nimport os\n_real = parse_config\n\
         def parse_config(text):\n    return _real(text) if os.urandom(1)[0] % 2 else Config(0)\n"
    );
    let rc = resolve_by_recompute(&r, &ic, 16).await.expect("ran");
    assert!(rc.flaky, "{rc:?}");
    assert!(matches!(
        rc.resolution,
        kgraf_orchestrator::Resolution::Rejected { .. }
    ));
}
