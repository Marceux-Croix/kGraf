//! Small git helpers for one attempt's throwaway repository.
//!
//! `kgraf-sandbox`'s `WorkspaceManager` and `MergeBack` (reused by
//! `runner.rs`, the same pieces `kgraf worker` uses) need a real git
//! repository with a first commit to work from, and merge-back leaves an
//! attempt as a commit under `refs/kgraf/attempts/<worker>-<n>` rather than
//! moving any branch (never the worker's decision, per `kgraf-cli`'s worker
//! command docs), so checking a brief means reading that commit's tree, not
//! a working directory. `git archive | tar -x` does that without a second
//! worktree registration to clean up.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// A git command through the host runner (`kgraf_core::host_git`): hooks,
/// every configured driver, the file system monitor and transports off. The
/// guard is the repository's shared lock (decision 0094), taken before the
/// command is built so the runner's config listing and the command run in
/// one hold; keep it until the process has exited.
fn host_git(
    dir: &Path,
    args: &[&str],
) -> Result<(Option<kgraf_core::repo_lock::RepoLockGuard>, Command)> {
    let guard = kgraf_core::repo_lock::shared_for(dir, &format!("kGraf bench git {args:?}"))
        .with_context(|| format!("cannot lock the repository at {}", dir.display()))?;
    let pre = kgraf_core::host_git::dash_c(dir);
    let cmd = kgraf_core::host_git::command(&kgraf_core::host_git::Invocation::in_dir(&pre), args)
        .with_context(|| format!("cannot run git {args:?} in {}", dir.display()))?;
    Ok((guard, cmd))
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let (_repo, mut cmd) = host_git(dir, args)?;
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("cannot run git {args:?} in {}", dir.display()))?;
    if !out.status.success() {
        bail!(
            "git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8(out.stdout)
        .with_context(|| format!("git {args:?}: not utf-8"))?
        .trim()
        .to_owned())
}

/// Copy `fixture`'s files into `root` (created if absent) and commit them as
/// a fresh git repository on branch `main`. Returns the fixture commit's id,
/// the base a protected-file diff is taken from.
pub fn init_fixture_repo(fixture: &Path, root: &Path) -> Result<String> {
    std::fs::create_dir_all(root).with_context(|| format!("cannot create {}", root.display()))?;
    copy_tree(fixture, root, &[])?;
    git(root, &["init", "-q", "-b", "main"])?;
    git(root, &["config", "user.name", "kgraf-bench-briefs"])?;
    git(
        root,
        &["config", "user.email", "kgraf-bench-briefs@localhost"],
    )?;
    git(root, &["add", "."])?;
    git(root, &["commit", "-q", "-m", "fixture"])?;
    git(root, &["rev-parse", "HEAD"])
}

/// The paths among `protected` (git pathspecs, relative to the repository
/// root) that differ between `base` and `commit`: added, changed, deleted or
/// renamed. Empty means the attempt left every protected path alone.
pub fn changed_protected(
    root: &Path,
    base: &str,
    commit: &str,
    protected: &[&str],
) -> Result<Vec<String>> {
    if protected.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["diff", "--name-only", "--no-renames", base, commit, "--"];
    args.extend_from_slice(protected);
    Ok(git(root, &args)?.lines().map(str::to_owned).collect())
}

/// The files under `pathspecs` at `commit`, as `(path, bytes)`: what the
/// runner itself holds for each protected path, read from the repository and
/// not from any working tree. A pathspec that matches nothing at `commit`
/// gives no entry.
pub fn files_at(root: &Path, commit: &str, pathspecs: &[&str]) -> Result<Vec<(String, Vec<u8>)>> {
    if pathspecs.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["ls-tree", "-r", "--name-only", commit, "--"];
    args.extend_from_slice(pathspecs);
    let mut files = Vec::new();
    for path in git(root, &args)?.lines() {
        let (_repo, mut cmd) = host_git(root, &["show", &format!("{commit}:{path}")])?;
        let out = cmd
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("cannot run git show {commit}:{path}"))?;
        if !out.status.success() {
            bail!(
                "git show {commit}:{path}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        files.push((path.to_owned(), out.stdout));
    }
    Ok(files)
}

/// Copy the directory `from` into `to`. Entries directly under `from` whose
/// name is in `skip_top` are left out (a mutation copy must not carry a
/// `target/`).
pub(crate) fn copy_tree(from: &Path, to: &Path, skip_top: &[&str]) -> Result<()> {
    for entry in
        std::fs::read_dir(from).with_context(|| format!("cannot read {}", from.display()))?
    {
        let entry = entry?;
        if skip_top.iter().any(|s| entry.file_name() == *s) {
            continue;
        }
        let dest = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            std::fs::create_dir_all(&dest)
                .with_context(|| format!("cannot create {}", dest.display()))?;
            copy_tree(&entry.path(), &dest, &[])?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &dest)
                .with_context(|| format!("cannot copy to {}", dest.display()))?;
        }
    }
    Ok(())
}

/// Extract `commit`'s tree from the repository at `root` into `dest`
/// (created if absent, must be empty), through `git archive | tar -x`: no
/// second worktree to register or remove. Both `git archive` and `tar` must
/// exit 0: a failed archive can leave tar with an empty stream that it
/// accepts, and an empty tree would then read as a model failure.
pub fn checkout_commit(root: &Path, commit: &str, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("cannot create {}", dest.display()))?;
    let (_repo, mut cmd) = host_git(root, &["archive", "--format=tar", commit])?;
    let mut archive = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run `git archive` in {}", root.display()))?;
    let Some(stdout) = archive.stdout.take() else {
        let _ = archive.kill();
        let _ = archive.wait();
        bail!("git archive gave no stdout pipe");
    };
    let tar_status = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(dest)
        .stdin(stdout)
        .status();
    // Always wait for git (no zombie) and read its verdict first.
    let archived = archive
        .wait_with_output()
        .context("cannot wait for `git archive`")?;
    if !archived.status.success() {
        bail!(
            "git archive {commit} in {} failed: {} {}",
            root.display(),
            archived.status,
            String::from_utf8_lossy(&archived.stderr).trim()
        );
    }
    let tar_status = tar_status.context("cannot run tar -x")?;
    if !tar_status.success() {
        bail!("tar -x into {} failed: {tar_status}", dest.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(repo: &Path) -> String {
        git(repo, &["rev-parse", "HEAD"]).expect("head")
    }

    #[test]
    fn a_fixture_is_committed_and_a_commit_round_trips_through_archive() {
        let src = tempfile::tempdir().expect("src");
        std::fs::write(src.path().join("a.txt"), "hello").expect("write");
        std::fs::create_dir_all(src.path().join("sub")).expect("sub");
        std::fs::write(src.path().join("sub/b.txt"), "world").expect("write");

        let repo = tempfile::tempdir().expect("repo");
        let base = init_fixture_repo(src.path(), repo.path()).expect("init");
        assert_eq!(base, head(repo.path()), "the fixture commit is returned");

        let dest = tempfile::tempdir().expect("dest");
        checkout_commit(repo.path(), &base, dest.path()).expect("checkout");
        assert_eq!(
            std::fs::read_to_string(dest.path().join("a.txt")).expect("a.txt"),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dest.path().join("sub/b.txt")).expect("b.txt"),
            "world"
        );
    }

    #[test]
    fn a_failed_git_archive_is_an_error_not_an_empty_tree() {
        let src = tempfile::tempdir().expect("src");
        std::fs::write(src.path().join("a.txt"), "hello").expect("write");
        let repo = tempfile::tempdir().expect("repo");
        init_fixture_repo(src.path(), repo.path()).expect("init");
        let dest = tempfile::tempdir().expect("dest");
        let err = checkout_commit(repo.path(), "no-such-commit-0000", dest.path())
            .expect_err("a commit that does not exist must not check out");
        assert!(format!("{err:#}").contains("git archive"), "{err:#}");
        assert!(
            !dest.path().join("a.txt").exists(),
            "nothing may be extracted"
        );
    }

    #[test]
    fn changed_protected_reports_edits_additions_and_deletions_only_in_scope() {
        let src = tempfile::tempdir().expect("src");
        std::fs::create_dir_all(src.path().join("tests")).expect("dir");
        std::fs::write(src.path().join("tests/consumer.rs"), "a").expect("write");
        std::fs::write(src.path().join("lib.rs"), "l").expect("write");
        let repo = tempfile::tempdir().expect("repo");
        let base = init_fixture_repo(src.path(), repo.path()).expect("init");
        let protected = ["tests/consumer.rs", "keep.txt"];

        // An unprotected change reports nothing.
        std::fs::write(repo.path().join("lib.rs"), "l2").expect("write");
        git(repo.path(), &["commit", "-qam", "edit lib"]).expect("commit");
        assert!(
            changed_protected(repo.path(), &base, &head(repo.path()), &protected)
                .expect("diff")
                .is_empty()
        );

        // An edit and an addition each show up.
        std::fs::write(repo.path().join("tests/consumer.rs"), "b").expect("write");
        std::fs::write(repo.path().join("keep.txt"), "new").expect("write");
        git(repo.path(), &["add", "."]).expect("add");
        git(repo.path(), &["commit", "-qm", "touch protected"]).expect("commit");
        let mut changed =
            changed_protected(repo.path(), &base, &head(repo.path()), &protected).expect("diff");
        changed.sort();
        assert_eq!(changed, ["keep.txt", "tests/consumer.rs"]);

        // A deletion counts.
        let after_edit = head(repo.path());
        std::fs::remove_file(repo.path().join("tests/consumer.rs")).expect("rm");
        git(repo.path(), &["commit", "-qam", "delete"]).expect("commit");
        assert_eq!(
            changed_protected(
                repo.path(),
                &after_edit,
                &head(repo.path()),
                &["tests/consumer.rs"]
            )
            .expect("diff"),
            ["tests/consumer.rs"]
        );
    }
}
