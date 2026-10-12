//! The earlier behavior signal (decision 0018, amended 2026-10-12): when an
//! attempt passes its narrowed check, the packages that depend on its
//! packages and can build now run their checks too, at most
//! `DEPENDENTS_CAP` of them. A failure there fails the attempt only when it
//! is tied to the attempt; otherwise it is a notice.
//!
//! The fixture is a cargo workspace with no registry dependencies:
//!
//! - `a`, the contract's package, `rate()`;
//! - `b`, `e`, `f`, each depending on `a`, with a test that `rate()` is 2;
//! - `c`, depending on `a` and on `d`, with a test over both;
//! - `d`, a stub an open contract still writes.
//!
//! How to run: `cargo test -p kgraf-host dependents` on macOS or Linux, with
//! `cargo` on `PATH`; no network (`--offline`).

use super::*;

const CHECK: &str = "cargo test --workspace --offline";

fn manifest(name: &str, deps: &[&str]) -> String {
    let mut text =
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    if !deps.is_empty() {
        text.push_str("\n[dependencies]\n");
        for d in deps {
            text.push_str(&format!("{d} = {{ path = \"../{d}\" }}\n"));
        }
    }
    text
}

const RATE_TEST: &str = "#[test]\nfn rate_is_two() {\n    assert_eq!(a::rate(), 2);\n}\n";

/// The workspace, with `a/src/lib.rs` as `a_lib`, its worktrees ready.
fn workspace(a_lib: &str) -> Repo {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let (ma, mb, mc, md, me, mf) = (
        manifest("a", &[]),
        manifest("b", &["a"]),
        manifest("c", &["a", "d"]),
        manifest("d", &[]),
        manifest("e", &["a"]),
        manifest("f", &["a"]),
    );
    let files: Vec<(&str, &str)> = vec![
        (".gitignore", "target/\nCargo.lock\n"),
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\"]\n",
        ),
        ("a/Cargo.toml", &ma),
        ("a/src/lib.rs", a_lib),
        ("b/Cargo.toml", &mb),
        ("b/src/lib.rs", ""),
        ("b/tests/behavior.rs", RATE_TEST),
        ("c/Cargo.toml", &mc),
        ("c/src/lib.rs", ""),
        (
            "c/tests/both.rs",
            "#[test]\nfn sum() {\n    assert_eq!(a::rate() + d::extra(), 5);\n}\n",
        ),
        ("d/Cargo.toml", &md),
        ("d/src/lib.rs", "pub fn extra() -> u32 {\n    todo!()\n}\n"),
        ("e/Cargo.toml", &me),
        ("e/src/lib.rs", ""),
        ("e/tests/behavior.rs", RATE_TEST),
        ("f/Cargo.toml", &mf),
        ("f/src/lib.rs", ""),
        ("f/tests/behavior.rs", RATE_TEST),
    ];
    let r = repo_in(dir, root.clone(), &base, &files);
    owner_only_modes(&root, &files);
    r
}

/// `a`'s contract, and the open contract that still writes `d`.
fn contracts() -> (OpenContract, Vec<OpenContract>) {
    (
        writing("ca", &["a/src/lib.rs"]),
        vec![writing("cd", &["d/src/lib.rs"])],
    )
}

async fn verdict_for(r: &Repo, worker: &str, a_lib: &str) -> Verdict {
    let (ca, open) = contracts();
    let ws = prepared(r, &ca, worker).await;
    std::fs::write(ws.join("a/src/lib.rs"), a_lib).expect("write");
    let v = CommandVerifier {
        root_gate: true,
        ..verifier(r, CHECK)
    };
    v.verify_among(&ca, &ws, &result(), &open)
        .await
        .expect("ran")
}

/// `a` is right at the base (its dependents pass there); the attempt
/// changes its behavior. Its own package has no tests, so its narrowed check
/// passes; `b`'s test fails with the attempt and passes without it, so the
/// attempt fails, and the reason names `b`'s check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_behavior_fails_on_a_dependents_test_and_is_attributed() {
    let r = workspace("pub fn rate() -> u32 {\n    2\n}\n");
    let verdict = verdict_for(&r, "w-wrong", "pub fn rate() -> u32 {\n    3\n}\n").await;
    let Verdict::Fail { reason } = &verdict else {
        panic!("b's test fails with the attempt: {verdict:?}");
    };
    assert!(
        reason.starts_with("`cargo test -p a --offline` passed, but `cargo test -p b --offline`"),
        "{reason}"
    );
    assert!(
        reason.contains("fails with this attempt and passes without it"),
        "{reason}"
    );
    assert!(reason.contains("rate_is_two"), "{reason}");
    // `c` needs `d`, which an open contract still writes: never run.
    assert!(!reason.contains("-p c "), "{reason}");
}

/// The right attempt passes; `b` and `e` ran (the cap is 2), `f` was left
/// out and said so, and `c`, which needs the open contract's `d`, did not run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_right_attempt_passes_with_its_dependents_run_up_to_the_cap() {
    let r = workspace("pub fn rate() -> u32 {\n    2\n}\n");
    let verdict = verdict_for(&r, "w-right", "pub fn rate() -> u32 {\n    1 + 1\n}\n").await;
    let Verdict::Pass { by, commit, .. } = &verdict else {
        panic!("its dependents pass: {verdict:?}");
    };
    assert!(commit.is_some());
    assert_eq!(
        by,
        "cargo test -p a --offline (dependents passed: cargo test -p b --offline, cargo test \
         -p e --offline; dependents not run, at most 2 per attempt: f)"
    );
    assert!(!by.contains("-p c "), "{by}");
}

/// A dependent's test that fails at the base too, inside its own code, may
/// be merged work's fault: a notice, never a failure. `a` is a stub at the
/// base; the attempt is wrong, and `b`'s assertion (in `b`'s file) fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_not_tied_to_the_attempt_is_a_notice_and_the_attempt_passes() {
    let r = workspace("pub fn rate() -> u32 {\n    todo!()\n}\n");
    let verdict = verdict_for(&r, "w-stub", "pub fn rate() -> u32 {\n    3\n}\n").await;
    let Verdict::Pass { notices, .. } = &verdict else {
        panic!("an unattributed failure never fails the attempt: {verdict:?}");
    };
    let notice = notices
        .iter()
        .find(|n| n.contains("could not be tied to this contract's files"))
        .unwrap_or_else(|| panic!("said in a notice: {notices:?}"));
    assert!(notice.contains("contract ca"), "{notice}");
    assert!(notice.contains("cargo test -p b --offline"), "{notice}");
    assert!(notice.contains("the root gate judges"), "{notice}");
}

/// A dependent's test that reaches a panic in the contract's own file is the
/// contract's: a line new to the output names `a/src/lib.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dependents_test_that_panics_in_the_contracts_file_is_attributed() {
    let r = workspace("pub fn rate() -> u32 {\n    todo!()\n}\n");
    let verdict = verdict_for(
        &r,
        "w-panic",
        "pub fn rate() -> u32 {\n    let v: Vec<u32> = Vec::new();\n    v[0]\n}\n",
    )
    .await;
    let Verdict::Fail { reason } = &verdict else {
        panic!("the panic is in a's file: {verdict:?}");
    };
    assert!(reason.contains("cargo test -p b --offline"), "{reason}");
    assert!(
        reason.contains("names a file this contract writes"),
        "{reason}"
    );
    assert!(reason.contains("a/src/lib.rs"), "{reason}");
}
