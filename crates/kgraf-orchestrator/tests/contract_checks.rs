//! The pre-dispatch contract checks and revision classification against real
//! temporary Rust and Python repositories: every command runs for real, inside the
//! sandbox (the mechanism, not a setting). No model is called here.

mod contract_support;

use contract_support::*;
use kgraf_orchestrator::{
    CheckError, CheckStage, DeliveryCheck, FileOverlay, InterfaceContract, InterfaceKind,
    RevisionClass, WriteTarget, check_delivery, check_mechanical, classify, delivered_files,
    resolve_by_recompute,
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
        kind: kgraf_orchestrator::InterfaceKind::Behavior,
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

/// A consumer file at the tests file's path (the same content, which
/// validation allows) is one file in the tree. Its canary is the canary-test
/// run, so a correct interface passes; before, the valid tests overlay restored
/// the file over the consumer canary and the check blamed the compile command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_consumer_file_shared_with_the_tests_still_gets_a_canary_and_a_sound_contract_passes() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = rust_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let mut ic = rust_ic();
    ic.consumer = vec![FileOverlay::new("tests/contract.rs", RUST_TEST)];
    let runs = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect("a correct interface with a shared path passes");
    let canaries: Vec<(&str, bool)> = runs
        .iter()
        .filter(|x| x.against.starts_with("canary"))
        .map(|x| (x.against.as_str(), x.success))
        .collect();
    assert_eq!(
        canaries,
        vec![("canary test", false)],
        "the shared file is broken by the canary-test run; no consumer-only file is left"
    );

    // One shared file and one consumer-only file: the consumer canary breaks
    // only the second, and a compile command that skips it is still caught.
    ic.consumer
        .push(FileOverlay::new("tests/consumer.rs", RUST_CONSUMER));
    let runs = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect("passes");
    assert!(
        runs.iter()
            .any(|x| x.against == "canary consumer" && !x.success),
        "{runs:?}"
    );
    ic.compile = s(&[
        "cargo",
        "test",
        "--no-run",
        "--offline",
        "--quiet",
        "--test",
        "contract",
    ]);
    let f = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect_err("a compile command that skips the consumer-only file fails");
    assert!(
        f.detail.contains("does not build every consumer file"),
        "{}",
        f.detail
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
        kind: kgraf_orchestrator::InterfaceKind::Behavior,
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
        compile: s(&[py(), "-c", "import consumer, test_contract"]),
        test: s(&[py(), "-m", "unittest", "-q", "test_contract"]),
        consulted: vec![],
        replaces: vec![],
    }
}

/// Measurement, not a check: the wall time of the mechanical checks of one
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

/// Check runs queue behind one process-wide bound. One more run than the
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

/// A folder symlink committed in the base (`tests -> <outside>`) never
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

    // Wrapped in a shell, the program is found only by running it, in
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

    // A macOS shim with no Command Line Tools behind it exists on PATH and
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

/// A program that runs and cannot open a file the command names is a missing
/// file, not a missing program: the message must not say the language cannot
/// be checked on this machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_script_argument_the_tree_lacks_is_a_missing_file_not_a_missing_program() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = py_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let mut ic = py_ic();
    ic.compile = s(&["bash", "-n", "serve_local.sh"]);
    let e = check_mechanical(&r, &ic)
        .await
        .expect_err("a check that cannot open its file is no verdict");
    assert!(
        matches!(&e, CheckError::InputMissing { program, file, .. } if program == "bash" && file == "serve_local.sh"),
        "{e}"
    );
    let text = e.to_string();
    assert!(
        text.contains("`serve_local.sh`") && !text.contains("cannot be checked on this machine"),
        "{text}"
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

/// A contract test at the path of a file the project already tracks would
/// replace it in every check tree: refused as an artifact failure, before any
/// command runs. The same bytes as the tracked file are not a replacement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_contract_test_replacing_a_tracked_file_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path().join("pyrepo");
    std::fs::create_dir_all(&repo).expect("mkdir");
    std::fs::write(repo.join("README.md"), "python project\n").expect("write");
    // The project's own consumer test, which the contract must not rewrite.
    std::fs::write(repo.join("test_contract.py"), PY_TEST).expect("write");
    commit_all(&repo);
    let r = runner(dir.path(), &repo);

    let mut ic = py_ic();
    ic.tests[0].content = PY_TEST.replace("8080", "80");
    let f = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect_err("refused");
    assert_eq!(f.stage, CheckStage::Artifact);
    assert!(
        f.detail
            .contains("`test_contract.py` would replace a file that already exists"),
        "{}",
        f.detail
    );

    // The tracked file's own content, unchanged, is checked as usual.
    check_mechanical(&r, &py_ic())
        .await
        .expect("ran")
        .expect("the unchanged test passes");
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
    // the process-wide bound is reported apart, so the bound on the run
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

#[cfg(all(unix, not(target_os = "linux")))]
fn check_has_its_own_pid_namespace() -> bool {
    false
}

/// With a PID namespace: the check's escaped descendant (it would live until
/// released, which the test does not do before the run ends) died with PID 1,
/// so the run ends at once as a normal result. `Ok` is itself the proof that
/// no process held the run's hold file when the check ended (`hold_free`,
/// which the escapee's inherited lock would fail); ending within the bound
/// says the runner did not wait it out.
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

/// The longest an escaped descendant of the tests below lives while its
/// `alive` file exists, so a stuck test does not leave it running for long.
#[cfg(unix)]
const ESCAPEE_CAP_SECS: u32 = 60;

/// A Python program whose forked child leaves the process group with its own
/// `setsid` and then lives while the file named by its first argument exists
/// (at most [`ESCAPEE_CAP_SECS`]). The test removes the file after the
/// verdict, and the file's temporary directory goes with the test even when
/// it fails. The child waits `delay_secs` before the `setsid`. The parent goes
/// on only after the child has called `setsid`: it reads one byte the child
/// writes after the call. So the child has left the group when the group kill
/// comes, however slowly it is scheduled, and it is alive until the test
/// removes the file. With a fixed sleep in either process the premise
/// depended on the clock: under load, a child that had not yet called
/// `setsid` when the parent ended was killed with the group, the tree was
/// quiet and the check ended normally. `parent` runs in the parent after the
/// wait.
#[cfg(unix)]
fn escapee(delay_secs: f64, parent: &[&str]) -> String {
    let mut lines = vec![
        "import os, sys, time".to_owned(),
        "alive = sys.argv[1]".to_owned(),
        "r, w = os.pipe()".to_owned(),
        "if os.fork() == 0:".to_owned(),
        "    os.close(r)".to_owned(),
        format!("    time.sleep({delay_secs})"),
        "    os.setsid()".to_owned(),
        "    os.write(w, b'x')".to_owned(),
        "    os.close(w)".to_owned(),
        format!("    end = time.time() + {ESCAPEE_CAP_SECS}"),
        "    while time.time() < end and os.path.exists(alive):".to_owned(),
        "        time.sleep(0.05)".to_owned(),
        "    os._exit(0)".to_owned(),
        "os.close(w)".to_owned(),
        // No byte: the child died before its `setsid`, so nothing escaped.
        "if os.read(r, 1) != b'x':".to_owned(),
        "    sys.exit(3)".to_owned(),
    ];
    lines.extend(parent.iter().map(|l| (*l).to_owned()));
    lines.join("\n")
}

/// The argv that runs [`escapee`] with `alive` as the file that keeps its
/// child alive. The file is made here.
#[cfg(unix)]
fn escapee_argv(program: &str, alive: &Path) -> Vec<String> {
    std::fs::write(alive, "").expect("alive file");
    vec![
        py().to_owned(),
        "-c".to_owned(),
        program.to_owned(),
        alive.display().to_string(),
    ]
}

/// A descendant that leaves the process group with its own `setsid` is
/// not reached by the group kill, but it still holds the run's hold file, so
/// the check ends as one that could not run, never as a quiet tree. The
/// descendant lives until the test removes its `alive` file, after the verdict.
///
/// Linux with a PID namespace: the kernel kills the descendant when the check
/// (PID 1) exits, so the tree is quiet and the check ends normally.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_descendant_that_leaves_the_group_is_seen_through_the_hold() {
    assert_an_escaped_descendant_is_seen(0.0).await;
}

/// The same with a child that is slow to call `setsid` (one second), as a
/// child on a loaded machine is: the parent waits for the call, so the child
/// still escapes and is still seen. Before the parent waited, the parent
/// ended first, the group kill took the child and the tree read quiet.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_descendant_slow_to_leave_the_group_is_still_seen() {
    assert_an_escaped_descendant_is_seen(1.0).await;
}

#[cfg(unix)]
async fn assert_an_escaped_descendant_is_seen(delay_secs: f64) {
    let dir = tempfile::tempdir().expect("dir");
    let repo = shell_repo(dir.path());
    let r = runner_with(
        dir.path(),
        &repo,
        vec![state_dir(dir.path())],
        Duration::from_secs(30),
    );
    let alive = dir.path().join("alive");
    let argv = escapee_argv(&escapee(delay_secs, &[]), &alive);
    if check_has_its_own_pid_namespace() {
        let _ = assert_the_namespace_ended_the_escapee(&r, &argv).await;
        return;
    }
    let e = r
        .run(&[], &argv)
        .await
        .expect_err("a tree that is not quiet is no result");
    std::fs::remove_file(&alive).expect("release the escapee");
    assert!(e.to_string().contains("left its process group"), "{e}");
}

/// The check's forked child calls `setsid` and
/// stays alive ([`escapee`]), and the parent deletes and recreates the hold
/// file where it can reach (`../hold`, and any `*.hold` above that). The hold
/// file lives outside the check's writable root, so the escaped child's lock
/// stays visible.
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
    let alive = dir.path().join("alive");
    let parent = [
        "import glob",
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
    ];
    let argv = escapee_argv(&escapee(0.0, &parent), &alive);
    if check_has_its_own_pid_namespace() {
        let out = assert_the_namespace_ended_the_escapee(&r, &argv).await;
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
        .run(&[], &argv)
        .await
        .expect_err("the escaped child is seen");
    std::fs::remove_file(&alive).expect("release the escapee");
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
        kind: kgraf_orchestrator::InterfaceKind::Behavior,
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

// ------------------------------------------------------- compile-only interfaces

/// What kGraf derives from a repository's scaffold: a stub, the consumer files
/// that use it, a compile command, and no reference, no test, no test command.
fn compile_only_ic() -> InterfaceContract {
    InterfaceContract {
        kind: InterfaceKind::CompileOnly,
        id: "derived-prod".into(),
        version: 1,
        parties: s(&["prod", "cons"]),
        symbols: s(&["parse_config", "Config"]),
        stub: vec![FileOverlay::new("config.py", PY_STUB)],
        wrong: vec![],
        reference: vec![],
        consumer: vec![FileOverlay::new(
            "consumer.py",
            "from config import parse_config, Config\n",
        )],
        tests: vec![],
        compile: s(&[py(), "-c", "import consumer"]),
        test: vec![],
        consulted: vec![],
        replaces: vec![],
    }
}

/// A compile-only interface has nothing to run a test against, so `validate`
/// (reference, tests and a test command required) does not apply: its check is
/// the compile half, stub-compile and the consumer canary, and nothing else.
/// A consumer that uses a name the stub lacks fails it at the stub-compile stage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compile_only_interface_is_checked_by_compiling_alone() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = py_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let mut ic = compile_only_ic();
    let runs = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect("a stub that the consumer compiles against passes");
    assert_eq!(
        runs.len(),
        2,
        "stub-compile and the consumer canary: {runs:?}"
    );
    assert!(
        runs.iter()
            .all(|x| x.stage == CheckStage::StubCompile && x.expected)
    );
    ic.consumer[0].content = "from config import load_config\n".into();
    let failure = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect_err("a name the stub lacks");
    assert_eq!(failure.stage, CheckStage::StubCompile, "{failure}");
    assert!(failure.detail.contains("load_config"), "{failure}");
}

/// A revision of a compile-only interface is classified on compatibility
/// alone: the prior consumers must still compile against the revised stub.
/// There is no prior test to pass against a reference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compile_only_revision_is_classified_by_compatibility_alone() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = py_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let prior = compile_only_ic();
    let mut additive = compile_only_ic();
    additive.version = 2;
    additive.stub[0].content = format!("{PY_STUB}\n\ndef extra():\n    return 0\n");
    let c = classify(&r, &prior, &additive, &writes(), 0, 2)
        .await
        .expect("ran")
        .expect("valid");
    assert_eq!(c.class, RevisionClass::Amend, "{}", c.detail);
    assert!(
        c.detail.contains("prior consumers") && !c.detail.contains("reference"),
        "{}",
        c.detail
    );
    let mut breaking = compile_only_ic();
    breaking.version = 2;
    breaking.stub[0].content = "class Config:\n    pass\n".into();
    // The revised interface's own consumers do not need parse_config; the
    // prior ones do, and fail to compile against the revised stub.
    breaking.consumer[0].content = "from config import Config\n".into();
    breaking.compile = s(&[py(), "-c", "import consumer"]);
    let c = classify(&r, &prior, &breaking, &writes(), 0, 2)
        .await
        .expect("ran")
        .expect("valid");
    assert_eq!(c.class, RevisionClass::Supersede, "{}", c.detail);
}

// ------------------------------------- compile-only interfaces of current code

/// The producer's current code in a two-crate Rust workspace: `app` uses
/// `price` and `Rate`; no consumer file uses `unused_helper`.
const PROD_CURRENT: &str = "pub struct Rate { pub cents: i64 }\n\
pub fn price(qty: u32) -> Rate { Rate { cents: qty as i64 * 100 } }\n\
pub fn unused_helper() -> u8 { 1 }\n";

const APP_CURRENT: &str = "use prod::{price, Rate};\n\
pub fn total(qty: u32) -> i64 { let r: Rate = price(qty); r.cents }\n";

fn two_crate_repo(root: &Path) -> std::path::PathBuf {
    let repo = root.join("ws");
    for (path, content) in [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"prod\", \"app\"]\nresolver = \"2\"\n",
        ),
        (
            "prod/Cargo.toml",
            "[package]\nname = \"prod\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("prod/src/lib.rs", PROD_CURRENT),
        (
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\nprod = { path = \"../prod\" }\n",
        ),
        ("app/src/lib.rs", APP_CURRENT),
    ] {
        let at = repo.join(path);
        std::fs::create_dir_all(at.parent().expect("parent")).expect("mkdir");
        std::fs::write(at, content).expect("write");
    }
    commit_all(&repo);
    repo
}

/// What kGraf derives for a producer that edits existing code: the stub is
/// the file as it is, the consumer the file that uses it, the command the one
/// kGraf's derive step gives for a Rust consumer.
fn current_code_rust_ic() -> InterfaceContract {
    InterfaceContract {
        kind: InterfaceKind::CompileOnly,
        id: "derived-prod".into(),
        version: 1,
        parties: s(&["prod", "app"]),
        symbols: s(&["price", "Rate"]),
        stub: vec![FileOverlay::new("prod/src/lib.rs", PROD_CURRENT)],
        wrong: vec![],
        reference: vec![],
        consumer: vec![FileOverlay::new("app/src/lib.rs", APP_CURRENT)],
        tests: vec![],
        compile: s(&["cargo", "test", "--no-run", "-p", "app"]),
        test: vec![],
        consulted: vec![],
        replaces: vec![],
    }
}

/// Rust, current code as the stub: the check before dispatch passes (the
/// consumer builds against the code as it is, and its canary does not), a
/// delivery that changes a signature the consumer uses fails, and a delivery
/// that changes only what the consumer does not use passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_a_delivery_that_breaks_a_used_signature_fails_and_an_unused_change_passes() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = two_crate_repo(dir.path());
    let r = runner(dir.path(), &repo);
    let ic = current_code_rust_ic();
    assert!(ic.stub_is_current_code());
    let runs = check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect("the consumer builds against the current code");
    assert_eq!(
        runs.len(),
        2,
        "stub-compile and the consumer canary: {runs:?}"
    );

    let broken = [FileOverlay::new(
        "prod/src/lib.rs",
        "pub struct Rate { pub cents: i64 }\n\
         pub fn price(qty: u32, region: &str) -> Rate { let _ = region; Rate { cents: qty as i64 } }\n\
         pub fn unused_helper() -> u8 { 1 }\n",
    )];
    match check_delivery(&r, &ic, &broken).await {
        DeliveryCheck::Broken { detail } => {
            assert!(
                detail.contains("derived-prod@v1")
                    && detail.contains("app/src/lib.rs")
                    && detail.contains("breaks a shape"),
                "{detail}"
            );
            assert!(
                detail.contains("E0061") || detail.contains("argument"),
                "{detail}"
            );
        }
        other => panic!("a used signature changed: {other:?}"),
    }

    let unused = [
        FileOverlay::new(
            "prod/src/lib.rs",
            "mod extra;\npub use extra::bonus;\n\
             pub struct Rate { pub cents: i64 }\n\
             pub fn price(qty: u32) -> Rate { Rate { cents: qty as i64 * 90 + bonus() } }\n\
             pub fn unused_helper(scale: u16) -> u16 { scale * 2 }\n",
        ),
        FileOverlay::new("prod/src/extra.rs", "pub fn bonus() -> i64 { 5 }\n"),
    ];
    assert_eq!(
        check_delivery(&r, &ic, &unused).await,
        DeliveryCheck::Passed,
        "only a body, an unused signature and a new module changed"
    );
}

/// The delivery is every declared file the producer's workspace holds, not
/// only the stub's paths: a new module beside the edited file is part of what
/// the consumer compiles against. A declared file the workspace lacks is left
/// out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_delivery_is_every_declared_file_the_workspace_holds() {
    let dir = tempfile::tempdir().expect("dir");
    let ws = dir.path().join("worktree");
    std::fs::create_dir_all(ws.join("prod/src")).expect("mkdir");
    std::fs::write(ws.join("prod/src/lib.rs"), "mod extra;\n").expect("write");
    std::fs::write(ws.join("prod/src/extra.rs"), "pub fn bonus() {}\n").expect("write");
    let w = |p: &str| WriteTarget {
        path: p.into(),
        node: None,
    };
    let got = delivered_files(
        &ws,
        &[
            w("prod/src/lib.rs"),
            w("prod/src/extra.rs"),
            w("prod/src/gone.rs"),
        ],
    )
    .await;
    let paths: Vec<&str> = got.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["prod/src/lib.rs", "prod/src/extra.rs"]);
    assert_eq!(got[1].content, "pub fn bonus() {}\n");
}

/// A declared path listed twice is one file in the delivery. A declared path
/// that leaves the repository or names protected state is never read, even
/// when a file is there: the delivery lays files over the check base, so a
/// read of `../x` would pull a file from outside the workspace into it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_delivery_lists_a_path_once_and_never_reads_an_invalid_one() {
    let dir = tempfile::tempdir().expect("dir");
    let ws = dir.path().join("worktree");
    std::fs::create_dir_all(ws.join("prod/src")).expect("mkdir");
    std::fs::create_dir_all(ws.join(".git")).expect("mkdir");
    std::fs::write(ws.join("prod/src/lib.rs"), "pub fn one() {}\n").expect("write");
    std::fs::write(ws.join(".git/config"), "[core]\n").expect("write");
    std::fs::write(dir.path().join("outside.txt"), "outside\n").expect("write");
    let w = |p: &str| WriteTarget {
        path: p.into(),
        node: None,
    };
    let got = delivered_files(
        &ws,
        &[
            w("../outside.txt"),
            w(".git/config"),
            w("prod/src/lib.rs"),
            w("prod/src/lib.rs"),
            w("  "),
        ],
    )
    .await;
    let paths: Vec<&str> = got.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["prod/src/lib.rs"]);
}

const SHOP_PROD: &str = "def price(qty):\n    return qty * 100\n";
const SHOP_APP: &str = "from shop.prod import price\n\n\ndef total(q):\n    return price(q)\n";

fn shop_repo(root: &Path, app: &str) -> std::path::PathBuf {
    let repo = root.join("shop-repo");
    std::fs::create_dir_all(repo.join("shop")).expect("mkdir");
    std::fs::write(repo.join("shop/__init__.py"), "").expect("write");
    std::fs::write(repo.join("shop/prod.py"), SHOP_PROD).expect("write");
    std::fs::write(repo.join("shop/app.py"), app).expect("write");
    commit_all(&repo);
    repo
}

fn current_code_py_ic(app: &str) -> InterfaceContract {
    InterfaceContract {
        kind: InterfaceKind::CompileOnly,
        id: "derived-prod".into(),
        version: 1,
        parties: s(&["prod", "app"]),
        symbols: s(&["price"]),
        stub: vec![FileOverlay::new("shop/prod.py", SHOP_PROD)],
        wrong: vec![],
        reference: vec![],
        consumer: vec![FileOverlay::new("shop/app.py", app)],
        tests: vec![],
        compile: s(&[py(), "-c", "import shop.app"]),
        test: vec![],
        consulted: vec![],
        replaces: vec![],
    }
}

/// Python, current code as the stub. The import check resolves the names the
/// consumer imports when it loads, and no more: a changed call shape is not a
/// failure (Python does not check one at import, so the check does not block
/// the producer for it), and a name the consumer imports that the delivery
/// removed is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_a_changed_call_shape_does_not_block_and_a_removed_imported_name_does() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = shop_repo(dir.path(), SHOP_APP);
    let r = runner(dir.path(), &repo);
    let ic = current_code_py_ic(SHOP_APP);
    check_mechanical(&r, &ic)
        .await
        .expect("ran")
        .expect("the consumer imports against the current code");
    let reshaped = [FileOverlay::new(
        "shop/prod.py",
        "def price(qty, region):\n    return qty * 90\n",
    )];
    assert_eq!(
        check_delivery(&r, &ic, &reshaped).await,
        DeliveryCheck::Passed
    );
    let renamed = [FileOverlay::new(
        "shop/prod.py",
        "def cost(qty):\n    return qty * 100\n",
    )];
    match check_delivery(&r, &ic, &renamed).await {
        DeliveryCheck::Broken { detail } => {
            assert!(
                detail.contains("price") && detail.contains("shop/app.py"),
                "{detail}"
            );
        }
        other => panic!("an imported name was removed: {other:?}"),
    }
}

/// A consumer that does not build at the check base without the delivery
/// either is not the producer's to fix: no verdict, the reason given, never a
/// failure of the delivery. A check that cannot run gives no verdict either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_base_that_does_not_build_or_a_check_that_cannot_run_gives_no_verdict() {
    let dir = tempfile::tempdir().expect("dir");
    let broken_app = "from shop.prod import price, missing_at_base\n";
    let repo = shop_repo(dir.path(), broken_app);
    let r = runner(dir.path(), &repo);
    let ic = current_code_py_ic(broken_app);
    let delivery = [FileOverlay::new(
        "shop/prod.py",
        "def price(qty):\n    return 1\n",
    )];
    match check_delivery(&r, &ic, &delivery).await {
        DeliveryCheck::NotJudged { reason } => {
            assert!(
                reason.contains("without the delivery either")
                    && reason.contains("not the producer's"),
                "{reason}"
            );
        }
        other => panic!("the base itself does not build: {other:?}"),
    }
    let mut missing = current_code_py_ic(SHOP_APP);
    missing.compile = s(&["kgraf-no-such-compiler", "x"]);
    match check_delivery(&r, &missing, &delivery).await {
        DeliveryCheck::NotJudged { reason } => {
            assert!(reason.contains("could not run"), "{reason}");
        }
        other => panic!("the program is missing: {other:?}"),
    }
}

/// Decision 0084, amended 2026-10-06: a contract check's tree is the base
/// commit, so it has none of the dependencies the user installed in the
/// checkout. They are linked into it and readable: a check that needs a
/// package from the checkout's `node_modules` finds it, and the base's own
/// install is untouched after the run. Real `node`, macOS.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_check_finds_a_package_the_user_installed_in_the_checkout() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir");
    std::fs::write(repo.join("package.json"), r#"{"name":"fx","private":true}"#).expect("w");
    std::fs::write(repo.join(".gitignore"), "node_modules/\n").expect("w");
    commit_all(&repo);
    std::fs::create_dir_all(repo.join("node_modules/dep")).expect("mkdir");
    std::fs::write(
        repo.join("node_modules/dep/index.js"),
        "module.exports = { answer: 42 };\n",
    )
    .expect("w");
    let r = runner_with(
        dir.path(),
        &repo,
        vec![state_dir(dir.path())],
        Duration::from_secs(60),
    );
    let argv: Vec<String> = [
        "node",
        "-e",
        "process.exit(require('dep').answer === 42 ? 0 : 1)",
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect();
    let ran = r.run(&[], &argv).await.expect("ran");
    assert!(ran.success, "{}", ran.tail);
    assert_eq!(
        std::fs::read_to_string(repo.join("node_modules/dep/index.js")).expect("read"),
        "module.exports = { answer: 42 };\n"
    );
}
