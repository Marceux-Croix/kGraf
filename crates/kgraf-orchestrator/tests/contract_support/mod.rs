//! Real repositories and check runners for the contract tests.

#![allow(dead_code)]

use kgraf_orchestrator::{CheckBase, SandboxRunner};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}

pub fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Make `repo` a git repository whose one commit holds its current files.
pub fn commit_all(repo: &Path) {
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.name", "t"]);
    git(repo, &["config", "user.email", "t@example.invalid"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "base"]);
}

/// A kGraf state directory the checks must not read.
pub fn state_dir(root: &Path) -> PathBuf {
    let state = root.join("state");
    std::fs::create_dir_all(&state).expect("mkdir");
    std::fs::write(state.join("secret.txt"), "ledger secret").expect("write");
    state
}

/// A runner over the repository's HEAD, with `denies` unreadable.
pub fn runner_with(
    root: &Path,
    repo: &Path,
    denies: Vec<PathBuf>,
    timeout: Duration,
) -> SandboxRunner {
    SandboxRunner::new(
        CheckBase::Commit {
            repo: repo.to_path_buf(),
            rev: "HEAD".into(),
        },
        root.join("scratch"),
        denies,
    )
    .expect("runner")
    // Linux and Windows backends report gaps; the test host accepts them.
    .accept_partial_isolation(!cfg!(target_os = "macos"))
    .with_timeout(timeout)
}

pub fn runner(root: &Path, repo: &Path) -> SandboxRunner {
    runner_with(root, repo, vec![state_dir(root)], Duration::from_secs(120))
}
