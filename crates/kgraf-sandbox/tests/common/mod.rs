//! Shared fixtures: a throwaway repository with one commit.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// `<tmp>/repo` with one commit on `main`, and `<tmp>/ws` as the base path.
pub struct Fixture {
    pub tmp: tempfile::TempDir,
    pub repo: PathBuf,
    pub base: PathBuf,
}

pub fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_repo(&tmp.path().join("repo"));
    let base = tmp.path().join("ws");
    Fixture { tmp, repo, base }
}

/// A repository with one commit on `main` at `repo`.
pub fn init_repo(repo: &Path) -> PathBuf {
    let repo = repo.to_path_buf();
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "hello\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
    repo
}

/// PID of a process that has exited and been reaped.
pub fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}
