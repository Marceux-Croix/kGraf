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

/// Whether sandboxed commands here run as a dedicated Windows sandbox user
/// (decision 0097) rather than under the restricted token of decision 0011.
///
/// `KGRAF_REQUIRE_SANDBOX_USER=1` makes the sandbox user mandatory: a test
/// that would otherwise take the other backend's branch fails instead, so a
/// run whose purpose is the sandbox user cannot pass while it tests the
/// backend it did not mean to test.
#[cfg(windows)]
pub fn sandbox_user_mode() -> bool {
    let strength = kgraf_sandbox::platform_isolation().expect("a backend");
    let on = strength.backend == kgraf_sandbox::Backend::WindowsSandboxUser;
    assert!(
        on || !require_sandbox_user(),
        "KGRAF_REQUIRE_SANDBOX_USER=1 asks for the Windows sandbox user, and commands here \
         run under {:?}. Run `kgraf sandbox setup` once, with administrator rights, or set \
         KGRAF_SANDBOX_TAG to a tagged setup, then run these tests again",
        strength.backend
    );
    on
}

/// The name of the switch that makes the sandbox user mandatory.
pub const REQUIRE_VAR: &str = "KGRAF_REQUIRE_SANDBOX_USER";

/// Whether [`REQUIRE_VAR`] makes the sandbox user mandatory: `1` does, `0`
/// and unset do not, any other value is an error.
#[cfg(windows)]
pub fn require_sandbox_user() -> bool {
    requirement(std::env::var_os(REQUIRE_VAR).as_deref())
}

/// [`require_sandbox_user`] without the environment, so the decision itself
/// is testable on any platform. A value that is neither `1` nor `0` is an
/// error: a run that meant to make the sandbox user mandatory, and set the
/// switch to `yes` or `true`, must not quietly get the opposite.
pub fn requirement(value: Option<&std::ffi::OsStr>) -> bool {
    let Some(value) = value else {
        return false;
    };
    match value.to_str() {
        Some("1") => true,
        Some("") | Some("0") => false,
        _ => panic!("{REQUIRE_VAR} is {value:?}; it takes 1 or 0"),
    }
}

#[cfg(test)]
mod require_tests {
    use super::{REQUIRE_VAR, requirement};
    use std::ffi::OsStr;

    #[test]
    fn only_one_asks_for_the_sandbox_user() {
        assert!(requirement(Some(OsStr::new("1"))));
        assert!(!requirement(None));
        assert!(!requirement(Some(OsStr::new("0"))));
        assert!(!requirement(Some(OsStr::new(""))));
    }

    #[test]
    #[should_panic(expected = "it takes 1 or 0")]
    fn another_value_is_an_error() {
        let _ = requirement(Some(OsStr::new("true")));
    }

    #[test]
    fn the_name_is_the_one_the_run_sets() {
        assert_eq!(REQUIRE_VAR, "KGRAF_REQUIRE_SANDBOX_USER");
    }
}
