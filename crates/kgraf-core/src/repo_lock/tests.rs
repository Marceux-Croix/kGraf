use super::*;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(&key);
        }
    }
    let out = cmd
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository with one commit and a linked worktree `wt`.
fn repo() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().canonicalize().expect("canonical");
    let main = base.join("main");
    std::fs::create_dir_all(main.join("sub")).expect("mkdir");
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("sub/a"), "a").expect("write");
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "one"]);
    let wt = base.join("wt");
    git(
        &main,
        &["worktree", "add", "-q", wt.to_str().expect("utf8")],
    );
    (dir, main, wt)
}

const SHORT: Duration = Duration::from_millis(150);

#[test]
fn the_shared_directory_is_found_from_every_place_in_the_repository() {
    let (dir, main, wt) = repo();
    let common = main.join(".git");
    assert_eq!(common_dir_of(&main), Some(common.clone()));
    assert_eq!(common_dir_of(&main.join("sub")), Some(common.clone()));
    assert_eq!(common_dir_of(&wt), Some(common.clone()));
    assert_eq!(common_dir_of(&common), Some(common.clone()));
    assert_eq!(
        common_dir_of(&common.join("worktrees/wt")),
        Some(common.clone())
    );
    let outside = dir.path().join("plain");
    std::fs::create_dir_all(&outside).expect("mkdir");
    assert_eq!(common_dir_of(&outside), None);
}

#[test]
fn shared_holders_share_and_exclude_an_exclusive_one() {
    let (_dir, main, wt) = repo();
    let common = main.join(".git");
    let a = acquire(&common, Mode::Shared, "merge", SHORT).expect("shared");
    // The same repository reached from the worktree is the same lock.
    let b = shared_for(&wt, "worktree add")
        .expect("shared")
        .expect("a repository");
    let err = acquire(&common, Mode::Exclusive, "window", SHORT).expect_err("excluded");
    let text = err.to_string();
    assert!(
        text.contains("merge") && text.contains("worktree add"),
        "{text}"
    );
    drop((a, b));
    let ex = acquire(&common, Mode::Exclusive, "window w1", SHORT).expect("free now");
    let err = acquire(&common, Mode::Shared, "merge", SHORT).expect_err("excluded");
    assert!(
        matches!(err, RepoLockError::TimedOut { .. }) && err.to_string().contains("window w1"),
        "{err}"
    );
    assert!(acquire(&common, Mode::Exclusive, "w2", SHORT).is_err());
    drop(ex);
    acquire(&common, Mode::Shared, "merge", SHORT).expect("free now");
}

/// A waiter gets the lock as soon as the holder lets go, within its bound.
#[test]
fn a_waiter_proceeds_when_the_holder_releases() {
    let (_dir, main, _wt) = repo();
    let common = main.join(".git");
    let held = acquire(&common, Mode::Exclusive, "window", SHORT).expect("ex");
    let c = common.clone();
    let waiter = std::thread::spawn(move || {
        let t = Instant::now();
        acquire(&c, Mode::Shared, "merge", Duration::from_secs(10))
            .map(|_| t.elapsed())
            .map_err(|e| e.to_string())
    });
    std::thread::sleep(Duration::from_millis(200));
    drop(held);
    let waited = waiter.join().expect("join").expect("acquired");
    assert!(waited >= Duration::from_millis(150), "{waited:?}");
}

const HELPER_ENV: &str = "KGRAF_REPO_LOCK_TEST_HOLD";

/// Not a test of its own: the child process of
/// `a_crashed_holders_lock_is_free_once_it_is_gone`, which takes the lock and
/// sleeps until killed.
#[test]
#[ignore = "run as a child process by another test"]
fn hold_until_killed() {
    let Some(dir) = std::env::var_os(HELPER_ENV) else {
        return;
    };
    let _held = acquire(Path::new(&dir), Mode::Exclusive, "child", SHORT).expect("child lock");
    println!("held");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::thread::sleep(Duration::from_secs(120));
}

/// Another process's lock excludes this one (across processes), and a
/// holder killed outright leaves no stale lock: the kernel releases it.
#[cfg(unix)]
#[test]
fn a_crashed_holders_lock_is_free_once_it_is_gone() {
    use std::io::{BufRead, BufReader};
    let (_dir, main, _wt) = repo();
    let common = main.join(".git");
    let mut child = Command::new(std::env::current_exe().expect("exe"))
        .args([
            "--exact",
            "repo_lock::tests::hold_until_killed",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_ENV, &common)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("child");
    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
    let held = lines.any(|l| l.map(|l| l.trim().ends_with("held")).unwrap_or(false));
    assert!(held, "the child took the lock");
    let err = acquire(&common, Mode::Shared, "merge", SHORT).expect_err("held elsewhere");
    assert!(err.to_string().contains("another process"), "{err}");
    child.kill().expect("kill");
    child.wait().expect("reaped");
    acquire(&common, Mode::Exclusive, "after", Duration::from_secs(5))
        .expect("free once the holder is gone");
}
