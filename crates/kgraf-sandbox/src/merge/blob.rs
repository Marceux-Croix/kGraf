//! Reading one file of one commit, from the host, through the hardened git
//! runner (`safe_git`): the environment scrubbed, hooks and fsmonitor off, no
//! working tree. Used where a recorded artifact must be read as of a commit a
//! worker cannot rewrite afterwards (a challenge's quote).
//!
//! The blob is found with `ls-tree` and read with `cat-file blob`, which never
//! runs a textconv driver or a filter (the concern `git show --no-textconv`
//! answers for `show`). A symbolic link, a submodule, a directory, an absent
//! path or a blob over the size bound is refused, never followed or cut.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::safe_git::{SafeGit, Target};
use super::state::MergeError;
use crate::git::Git;

/// Why a blob was not read.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("`{path}` is not in commit {rev}")]
    Absent { rev: String, path: String },
    #[error("`{path}` in commit {rev} is a symbolic link; it is not followed")]
    Symlink { rev: String, path: String },
    #[error("`{path}` in commit {rev} is a {kind}, not a file")]
    NotAFile {
        rev: String,
        path: String,
        kind: String,
    },
    #[error("`{path}` in commit {rev} is {size} bytes, over the {max}-byte bound")]
    TooLarge {
        rev: String,
        path: String,
        size: u64,
        max: u64,
    },
    #[error(transparent)]
    Git(#[from] MergeError),
}

/// The contents of `path` in commit `rev` of the repository at `repo`, at most
/// `max_bytes` long. `path` is taken literally (no pathspec magic).
pub fn read_blob(repo: &Path, rev: &str, path: &str, max_bytes: u64) -> Result<Vec<u8>, BlobError> {
    let git = SafeGit {
        git: Git::discover().map_err(MergeError::from)?,
        common_dir: PathBuf::new(),
        attr_tree: None,
        timeout: Duration::from_secs(30),
    };
    let at = Target::Discover(repo);
    let literal = [("GIT_LITERAL_PATHSPECS", "1")];
    let listing = git.ok_bytes(&at, &literal, ["ls-tree", "-z", rev, "--", path], None)?;
    let entries: Vec<&[u8]> = listing
        .split(|b| *b == 0)
        .filter(|e| !e.is_empty())
        .collect();
    let (rev_s, path_s) = (rev.to_owned(), path.to_owned());
    let [entry] = entries[..] else {
        return Err(BlobError::Absent {
            rev: rev_s,
            path: path_s,
        });
    };
    // `<mode> SP <type> SP <object> TAB <path>`
    let text = String::from_utf8_lossy(entry);
    let (meta, listed) = text.split_once('\t').unwrap_or((&text, ""));
    if listed != path {
        return Err(BlobError::Absent {
            rev: rev_s,
            path: path_s,
        });
    }
    let fields: Vec<&str> = meta.split(' ').collect();
    let (mode, kind, object) = match fields[..] {
        [m, k, o] => (m, k, o),
        _ => {
            return Err(BlobError::Absent {
                rev: rev_s,
                path: path_s,
            });
        }
    };
    if mode == "120000" {
        return Err(BlobError::Symlink {
            rev: rev_s,
            path: path_s,
        });
    }
    if kind != "blob" {
        return Err(BlobError::NotAFile {
            rev: rev_s,
            path: path_s,
            kind: kind.to_owned(),
        });
    }
    let size: u64 = git
        .ok(&at, &[], ["cat-file", "-s", object], None)?
        .parse()
        .unwrap_or(u64::MAX);
    if size > max_bytes {
        return Err(BlobError::TooLarge {
            rev: rev_s,
            path: path_s,
            size,
            max: max_bytes,
        });
    }
    Ok(git.ok_bytes(&at, &[], ["cat-file", "blob", object], None)?)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .output()
            .expect("git");
        assert!(out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[test]
    fn a_blob_is_read_as_of_its_commit_and_links_and_large_files_are_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let repo = dir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(repo.join("docs")).expect("mkdir");
        std::fs::write(repo.join("docs/rule.md"), "old rule\n").expect("write");
        std::fs::write(repo.join("big.txt"), "x".repeat(64)).expect("write");
        std::os::unix::fs::symlink("/etc/passwd", repo.join("link.md")).expect("link");
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
                "a",
            ],
        );
        let sha = git(repo, &["rev-parse", "HEAD"]);
        // The working copy changes afterwards; the commit's blob is what is read.
        std::fs::write(repo.join("docs/rule.md"), "new rule\n").expect("write");
        assert_eq!(
            read_blob(repo, &sha, "docs/rule.md", 1024).expect("read"),
            b"old rule\n"
        );
        assert!(matches!(
            read_blob(repo, &sha, "link.md", 1024),
            Err(BlobError::Symlink { .. })
        ));
        assert!(matches!(
            read_blob(repo, &sha, "big.txt", 16),
            Err(BlobError::TooLarge { size: 64, .. })
        ));
        assert!(matches!(
            read_blob(repo, &sha, "docs", 1024),
            Err(BlobError::NotAFile { .. })
        ));
        assert!(matches!(
            read_blob(repo, &sha, "missing.md", 1024),
            Err(BlobError::Absent { .. })
        ));
        assert!(
            matches!(
                read_blob(repo, &sha, "docs/*.md", 1024),
                Err(BlobError::Absent { .. })
            ),
            "a pathspec is taken literally"
        );
    }
}
