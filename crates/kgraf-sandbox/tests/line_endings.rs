//! Merge-back and worktree checkout under a machine-wide line-ending setting.
//!
//! Git for Windows installs with `core.autocrlf=true` in its system config.
//! These tests give every git in this binary that setting, through a system
//! config file in a temporary folder (`GIT_CONFIG_SYSTEM`), and an empty
//! global one (`GIT_CONFIG_GLOBAL`), so the real user's files are never read.
//! A file the worker never wrote must never count as written, and kGraf's
//! worktrees must hold the bytes the commit holds, whatever the machine says.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use common::{fixture, git};
use kgraf_sandbox::{
    Attempt, CommitOutcome, FinishOutcome, Identity, MergeBack, WorkspaceManager, WorkspaceSpec,
};

const BRANCH: &str = "integration";

/// Serializes the tests of this binary. The first one sets the process
/// environment while it holds the lock; no other test has started a process
/// yet, so nothing reads the environment while it changes.
fn machine_with_autocrlf() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    static CONFIG: OnceLock<PathBuf> = OnceLock::new();
    let guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    CONFIG.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let system = dir.join("system.gitconfig");
        let global = dir.join("global.gitconfig");
        std::fs::write(&system, "[core]\n\tautocrlf = true\n").unwrap();
        std::fs::write(&global, "").unwrap();
        // SAFETY: called while the lock is held and before any test of this
        // binary has started a thread or a process (see above).
        unsafe {
            std::env::set_var("GIT_CONFIG_SYSTEM", &system);
            std::env::set_var("GIT_CONFIG_GLOBAL", &global);
            std::env::remove_var("GIT_CONFIG_NOSYSTEM");
        }
        dir
    });
    guard
}

struct Env {
    f: common::Fixture,
    mgr: Arc<WorkspaceManager>,
}

fn env_with(setup: impl FnOnce(&Path)) -> Env {
    let f = fixture();
    setup(&f.repo);
    git(&f.repo, &["branch", BRANCH, "main"]);
    let mgr = Arc::new(WorkspaceManager::open(&f.repo, &f.base).unwrap());
    mgr.startup().unwrap();
    Env { f, mgr }
}

fn merger(e: &Env) -> MergeBack {
    MergeBack::open(
        e.mgr.clone(),
        BRANCH,
        Identity {
            name: "kGraf Merge".into(),
            email: "merge@kgraf.invalid".into(),
        },
    )
    .unwrap()
}

fn attempt(e: &Env, name: &str) -> Attempt {
    let ws = e
        .mgr
        .create(&WorkspaceSpec {
            name,
            base: BRANCH,
            branch: None,
        })
        .unwrap();
    merger(e).attempt(ws).unwrap()
}

/// The system config the tests set is in effect for a plain git.
#[test]
fn the_machine_setting_is_in_effect_for_a_plain_git() {
    let _m = machine_with_autocrlf();
    let f = fixture();
    assert_eq!(git(&f.repo, &["config", "--get", "core.autocrlf"]), "true");
}

/// The worktree holds the committed bytes (no CRLF added at checkout), and an
/// attempt that wrote one file merges with only that file changed.
#[test]
fn an_untouched_file_is_not_a_write_under_a_machine_autocrlf() {
    let _m = machine_with_autocrlf();
    let e = env_with(|_| {});
    let mb = merger(&e);
    let mut a = attempt(&e, "one");
    let root = a.workspace().root().to_path_buf();
    assert_eq!(std::fs::read(root.join("README")).unwrap(), b"hello\n");
    std::fs::write(root.join("a.txt"), "new\n").unwrap();
    let before = git(&e.f.repo, &["rev-parse", BRANCH]);
    let out = mb.finish(&mut a, "one: add a", &["a.txt"]).unwrap();
    assert!(matches!(out, FinishOutcome::Merged { .. }), "{out:?}");
    let changed = git(&e.f.repo, &["diff", "--name-only", &before, BRANCH, "--"]);
    assert_eq!(changed, "a.txt");
}

/// An attempt that wrote nothing is empty, not a change to every text file.
#[test]
fn an_attempt_that_wrote_nothing_is_empty_under_a_machine_autocrlf() {
    let _m = machine_with_autocrlf();
    let e = env_with(|_| {});
    let mb = merger(&e);
    let mut a = attempt(&e, "idle");
    let out = mb.commit_declared(&mut a, "idle", &[] as &[&str]).unwrap();
    assert_eq!(out, CommitOutcome::Empty);
}

/// What the merge compares is the staged blob: bytes a worker writes are
/// kept as written where no attribute asks git to convert them, so a CRLF
/// file is committed with CRLF, on every machine.
#[test]
fn a_file_without_attributes_is_committed_byte_for_byte() {
    let _m = machine_with_autocrlf();
    let e = env_with(|_| {});
    let mb = merger(&e);
    let mut a = attempt(&e, "crlf");
    std::fs::write(a.workspace().root().join("win.txt"), "a\r\nb\r\n").unwrap();
    let out = mb.finish(&mut a, "crlf: add", &["win.txt"]).unwrap();
    assert!(matches!(out, FinishOutcome::Merged { .. }), "{out:?}");
    let blob = std::process::Command::new("git")
        .arg("-C")
        .arg(&e.f.repo)
        .args(["cat-file", "blob", &format!("{BRANCH}:win.txt")])
        .output()
        .unwrap();
    assert_eq!(blob.stdout, b"a\r\nb\r\n");
}

/// A repository's own `.gitattributes` still applies: a path marked
/// `eol=crlf` is checked out with CRLF, and that checkout is not a change.
#[test]
fn a_repository_eol_attribute_is_still_applied() {
    let _m = machine_with_autocrlf();
    let e = env_with(|repo| {
        std::fs::write(repo.join(".gitattributes"), "*.bat text eol=crlf\n").unwrap();
        std::fs::write(repo.join("run.bat"), "@echo off\necho hi\n").unwrap();
        git(repo, &["add", "."]);
        git(
            repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "attributes",
            ],
        );
    });
    let mb = merger(&e);
    let mut a = attempt(&e, "attr");
    let root = a.workspace().root().to_path_buf();
    assert_eq!(
        std::fs::read(root.join("run.bat")).unwrap(),
        b"@echo off\r\necho hi\r\n"
    );
    std::fs::write(root.join("a.txt"), "new\n").unwrap();
    let out = mb.finish(&mut a, "attr: add a", &["a.txt"]).unwrap();
    assert!(matches!(out, FinishOutcome::Merged { .. }), "{out:?}");
}

/// A worker's own git, run in the sandbox, gets the same line-ending
/// settings as kGraf's, so a `git checkout -- <path>` or `git stash` by the
/// worker writes the same bytes kGraf's checkout wrote.
#[cfg(unix)]
#[test]
fn a_worker_git_sees_the_same_line_ending_settings() {
    let _m = machine_with_autocrlf();
    let e = env_with(|_| {});
    let ws = e
        .mgr
        .create(&WorkspaceSpec {
            name: "sh",
            base: BRANCH,
            branch: None,
        })
        .unwrap();
    let out = kgraf_sandbox::workspace_command(&ws, kgraf_sandbox::Network::Deny, "/bin/sh")
        .unwrap()
        .arg("-c")
        .arg("git config --get core.autocrlf; git config --get core.eol")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "false\nlf\n");
}

/// The host's own `GIT_CONFIG_COUNT` entries reach the worker's git, and the
/// line-ending settings come after them.
#[cfg(unix)]
#[test]
fn the_hosts_git_config_entries_are_kept_before_the_line_ending_settings() {
    let _m = machine_with_autocrlf();
    let e = env_with(|_| {});
    let ws = e
        .mgr
        .create(&WorkspaceSpec {
            name: "host",
            base: BRANCH,
            branch: None,
        })
        .unwrap();
    // SAFETY: the lock is held, so no other test of this binary runs; the
    // variables are removed again before it is released.
    unsafe {
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "user.name");
        std::env::set_var("GIT_CONFIG_VALUE_0", "Preset");
    }
    let out = kgraf_sandbox::workspace_command(&ws, kgraf_sandbox::Network::Deny, "/bin/sh").map(
        |mut c| {
            c.arg("-c")
                .arg("git config --get user.name; git config --get core.autocrlf");
            c.output()
        },
    );
    unsafe {
        std::env::remove_var("GIT_CONFIG_COUNT");
        std::env::remove_var("GIT_CONFIG_KEY_0");
        std::env::remove_var("GIT_CONFIG_VALUE_0");
    }
    let out = out.unwrap().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Preset\nfalse\n");
}
