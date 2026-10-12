//! A Python check runs with the check tree's root on the import path. A
//! package at the root and a test in `tests/` that imports it (no
//! `conftest.py`, no `tests/__init__.py`) then pass against the reference, as
//! they do when the project is run from its root.
//!
//! How to run: `cargo test -p kgraf-orchestrator --test check_python_path`.
//! Needs `python3` on PATH (skipped without it); the pytest cases also need
//! `pytest` on PATH.

#![cfg(unix)]

use kgraf_orchestrator::{CheckBase, FileOverlay, SandboxRunner};

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
        .unwrap_or(false)
}

fn runner() -> (tempfile::TempDir, tempfile::TempDir, SandboxRunner) {
    let root = tempfile::tempdir().expect("root");
    let scratch = tempfile::tempdir().expect("scratch");
    let r = SandboxRunner::new(
        CheckBase::Directory(root.path().to_path_buf()),
        scratch.path().to_path_buf(),
        vec![],
    )
    .expect("runner")
    .accept_partial_isolation(!cfg!(target_os = "macos"));
    (root, scratch, r)
}

/// The reference tree: a package at the root, a test beside it in `tests/`.
fn reference() -> Vec<FileOverlay> {
    vec![
        FileOverlay::new(
            "vectorops/__init__.py",
            "def dotproduct(a, b):\n    return sum(x * y for x, y in zip(a, b))\n",
        ),
        FileOverlay::new(
            "tests/test_vec_contract.py",
            "from vectorops import dotproduct\n\n\
             def test_dot():\n    assert dotproduct([1, 2], [3, 4]) == 11\n\n\n\
             if __name__ == '__main__':\n    test_dot()\n",
        ),
    ]
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_test_run_as_a_script_imports_the_package_at_the_tree_root() {
    if !on_path("python3") {
        return;
    }
    let (_root, _scratch, r) = runner();
    let layer = reference();
    let res = r
        .run(&[&layer], &argv(&["python3", "tests/test_vec_contract.py"]))
        .await
        .expect("ran");
    assert!(res.success, "{res:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pytest_run_of_a_test_in_tests_imports_the_package_at_the_tree_root() {
    if !on_path("python3") || !on_path("pytest") {
        return;
    }
    let (_root, _scratch, r) = runner();
    let layer = reference();
    let res = r
        .run(&[&layer], &argv(&["pytest", "tests/test_vec_contract.py"]))
        .await
        .expect("ran");
    assert!(res.success, "{res:?}");
}
