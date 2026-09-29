//! What a worktree looks like to git after a call that may have written
//! anywhere inside it.
//!
//! A sandboxed build or test command declares no paths: the sandbox confines its
//! writes to the worktree, but the files it changed are not known from the call.
//! The loop asks git after such a call, so the handoff and the result list what
//! the command changed (a lockfile, a generated file, a file a test wrote).
//! Ignored paths (a build directory) are not listed: git does not report them.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The paths git reports as changed, deleted or untracked under `root`, relative
/// to it. A rename lists both the old and the new path.
///
/// Blocking: runs `git`. Call it through `spawn_blocking`.
pub fn dirty_paths(root: &Path) -> Result<Vec<PathBuf>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        // A status read must not take the index lock a concurrent git holds.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(parse_porcelain_z(&output.stdout))
}

/// Parse `git status --porcelain=v1 -z`: entries are `XY path` separated by NUL,
/// and a rename or copy is followed by one more entry, the old path.
fn parse_porcelain_z(bytes: &[u8]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut entries = bytes.split(|b| *b == 0).filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        paths.push(PathBuf::from(
            String::from_utf8_lossy(&entry[3..]).into_owned(),
        ));
        if matches!(entry[0], b'R' | b'C')
            && let Some(old) = entries.next()
        {
            paths.push(PathBuf::from(String::from_utf8_lossy(old).into_owned()));
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_entries_and_renames_parse() {
        let raw = b" M src/a.rs\0?? new file.txt\0R  b.rs\0old.rs\0 D gone.rs\0";
        let paths = parse_porcelain_z(raw);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("src/a.rs"),
                PathBuf::from("new file.txt"),
                PathBuf::from("b.rs"),
                PathBuf::from("old.rs"),
                PathBuf::from("gone.rs"),
            ]
        );
        assert!(parse_porcelain_z(b"").is_empty());
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_an_error() {
        let dir = tempfile::tempdir().expect("dir");
        // The temp directory may sit inside a repository on some machines, so
        // pin a directory git cannot open.
        let missing = dir.path().join("does-not-exist");
        assert!(dirty_paths(&missing).is_err());
    }
}
