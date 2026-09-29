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

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
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
/// a fresh git repository on branch `main`.
pub fn init_fixture_repo(fixture: &Path, root: &Path) -> Result<()> {
    std::fs::create_dir_all(root).with_context(|| format!("cannot create {}", root.display()))?;
    copy_tree(fixture, root)?;
    git(root, &["init", "-q", "-b", "main"])?;
    git(root, &["config", "user.name", "kgraf-bench-briefs"])?;
    git(
        root,
        &["config", "user.email", "kgraf-bench-briefs@localhost"],
    )?;
    git(root, &["add", "."])?;
    git(root, &["commit", "-q", "-m", "fixture"])?;
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    for entry in
        std::fs::read_dir(from).with_context(|| format!("cannot read {}", from.display()))?
    {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            std::fs::create_dir_all(&dest)
                .with_context(|| format!("cannot create {}", dest.display()))?;
            copy_tree(&entry.path(), &dest)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &dest)
                .with_context(|| format!("cannot copy to {}", dest.display()))?;
        }
    }
    Ok(())
}

/// Extract `commit`'s tree from the repository at `root` into `dest`
/// (created if absent, must be empty), through `git archive | tar -x`: no
/// second worktree to register or remove.
pub fn checkout_commit(root: &Path, commit: &str, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("cannot create {}", dest.display()))?;
    let archive = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["archive", "--format=tar", commit])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run `git archive` in {}", root.display()))?;
    let status = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(dest)
        .stdin(archive.stdout.context("git archive gave no stdout pipe")?)
        .status()
        .context("cannot run tar -x")?;
    if !status.success() {
        bail!("tar -x into {} failed: {status}", dest.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixture_is_committed_and_a_commit_round_trips_through_archive() {
        let src = tempfile::tempdir().expect("src");
        std::fs::write(src.path().join("a.txt"), "hello").expect("write");
        std::fs::create_dir_all(src.path().join("sub")).expect("sub");
        std::fs::write(src.path().join("sub/b.txt"), "world").expect("write");

        let repo = tempfile::tempdir().expect("repo");
        init_fixture_repo(src.path(), repo.path()).expect("init");
        let head = git(repo.path(), &["rev-parse", "HEAD"]).expect("head");

        let dest = tempfile::tempdir().expect("dest");
        checkout_commit(repo.path(), &head, dest.path()).expect("checkout");
        assert_eq!(
            std::fs::read_to_string(dest.path().join("a.txt")).expect("a.txt"),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dest.path().join("sub/b.txt")).expect("b.txt"),
            "world"
        );
    }
}
