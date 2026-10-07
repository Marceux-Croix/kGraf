//! Per-worktree build directories seeded by a copy-on-write clone
//! (decision 0060).
//!
//! Worktrees never share a `CARGO_TARGET_DIR`: cargo judges freshness by
//! modification time, and a shared directory handed one worktree another's
//! compiled code (measured, recorded in 0060). Each worktree gets its own
//! directory, seeded from a warm one so it does not pay a full cold build:
//! `cp -c` (APFS `clonefile`) on macOS, `cp --reflink=always` on Linux, and
//! a plain copy otherwise. The method used is always reported; a plain copy
//! is never passed off as a clone (fail loud).
//!
//! Novel code: no crate on crates.io clones a directory tree with a reported
//! method (`reflink-copy` clones single files and falls back silently).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::workspace::Workspace;

/// How the seed was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedMethod {
    /// APFS `clonefile` through `cp -c`: blocks shared until written.
    Clone,
    /// Linux reflink through `cp --reflink=always`: blocks shared until written.
    Reflink,
    /// A full byte copy, and why no clone was made.
    Copy { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedReport {
    pub dest: PathBuf,
    pub method: SeedMethod,
    pub elapsed: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    #[error("warm build directory {0} is not a directory")]
    NoWarmDir(PathBuf),
    #[error("destination {0} already exists; refusing to merge into it")]
    DestExists(PathBuf),
    #[error("warm directory {warm} and destination {dest} overlap")]
    Overlap { warm: PathBuf, dest: PathBuf },
    #[error("no copy-on-write clone could be made: {0}")]
    NoClone(String),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> SeedError + '_ {
    move |source| SeedError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Seed `<workspace root>/target`, cargo's default build directory for the
/// worktree, from `warm`.
pub fn seed_workspace_target(warm: &Path, ws: &Workspace) -> Result<SeedReport, SeedError> {
    seed_target_dir(warm, &ws.root().join("target"))
}

/// Make `dest` a copy of the build directory `warm`, by copy-on-write clone
/// where the file system allows it. `dest` must not exist.
pub fn seed_target_dir(warm: &Path, dest: &Path) -> Result<SeedReport, SeedError> {
    seed(warm, dest, true)
}

/// [`seed_target_dir`] by copy-on-write clone only: where no clone can be
/// made, `dest` is left absent and the error says why. For a seed made once
/// per check, where a byte copy of a large build folder would cost more than
/// the build it saves.
pub fn clone_target_dir(warm: &Path, dest: &Path) -> Result<SeedReport, SeedError> {
    seed(warm, dest, false)
}

fn seed(warm: &Path, dest: &Path, may_copy: bool) -> Result<SeedReport, SeedError> {
    let started = Instant::now();
    if !warm.is_dir() {
        return Err(SeedError::NoWarmDir(warm.to_path_buf()));
    }
    if dest.symlink_metadata().is_ok() {
        return Err(SeedError::DestExists(dest.to_path_buf()));
    }
    let warm = kgraf_core::plain_path::canonicalize(warm).map_err(io(warm))?;
    let parent = dest.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(io(parent))?;
    let parent = kgraf_core::plain_path::canonicalize(parent).map_err(io(parent))?;
    let dest_abs = match dest.file_name() {
        Some(name) => parent.join(name),
        None => return Err(SeedError::DestExists(dest.to_path_buf())),
    };
    if dest_abs.starts_with(&warm) || warm.starts_with(&dest_abs) {
        return Err(SeedError::Overlap {
            warm,
            dest: dest_abs,
        });
    }

    let method = match clone_tree(&warm, &dest_abs) {
        Ok(method) => method,
        Err(reason) if may_copy => {
            remove_partial(&dest_abs)?;
            copy_tree(&warm, &dest_abs)?;
            SeedMethod::Copy { reason }
        }
        Err(reason) => {
            remove_partial(&dest_abs)?;
            return Err(SeedError::NoClone(reason));
        }
    };
    Ok(SeedReport {
        dest: dest_abs,
        method,
        elapsed: started.elapsed(),
    })
}

/// Clone with the platform `cp`, or say why not.
pub(crate) fn clone_tree(warm: &Path, dest: &Path) -> Result<SeedMethod, String> {
    let (flags, method): (&[&str], SeedMethod) = if cfg!(target_os = "macos") {
        (&["-c", "-R"], SeedMethod::Clone)
    } else if cfg!(target_os = "linux") {
        (&["-R", "--reflink=always"], SeedMethod::Reflink)
    } else {
        return Err("no copy-on-write clone on this platform".into());
    };
    let out = Command::new("cp")
        .args(flags)
        .arg(warm)
        .arg(dest)
        .output()
        .map_err(|e| format!("cp could not start: {e}"))?;
    if out.status.success() {
        Ok(method)
    } else {
        Err(format!(
            "cp {} failed ({}): {}",
            flags.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn remove_partial(dest: &Path) -> Result<(), SeedError> {
    match std::fs::remove_dir_all(dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(dest)(e)),
    }
}

/// Plain recursive copy that keeps symlinks as symlinks.
fn copy_tree(src: &Path, dest: &Path) -> Result<(), SeedError> {
    std::fs::create_dir(dest).map_err(io(dest))?;
    for entry in std::fs::read_dir(src).map_err(io(src))? {
        let entry = entry.map_err(io(src))?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let kind = entry.file_type().map_err(io(&from))?;
        if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else if kind.is_symlink() {
            let target = std::fs::read_link(&from).map_err(io(&from))?;
            symlink(&target, &to).map_err(io(&to))?;
        } else {
            std::fs::copy(&from, &to).map_err(io(&from))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warm_dir(root: &Path) -> PathBuf {
        let warm = root.join("warm");
        std::fs::create_dir_all(warm.join("debug/deps")).unwrap();
        std::fs::write(warm.join("debug/deps/libx.rlib"), b"artifact").unwrap();
        std::fs::write(warm.join(".rustc_info.json"), b"{}").unwrap();
        warm
    }

    #[test]
    fn seeds_an_independent_copy_and_reports_the_method() {
        let tmp = tempfile::tempdir().unwrap();
        let warm = warm_dir(tmp.path());
        let dest = tmp.path().join("wt/target");
        let report = seed_target_dir(&warm, &dest).unwrap();
        assert_eq!(
            std::fs::read(dest.join("debug/deps/libx.rlib")).unwrap(),
            b"artifact"
        );
        // Writing the seed never reaches the warm directory.
        std::fs::write(dest.join("debug/deps/libx.rlib"), b"changed").unwrap();
        assert_eq!(
            std::fs::read(warm.join("debug/deps/libx.rlib")).unwrap(),
            b"artifact"
        );
        if cfg!(target_os = "macos") {
            // The temporary directory is on APFS on every supported macOS.
            assert_eq!(report.method, SeedMethod::Clone);
        } else if let SeedMethod::Copy { reason } = &report.method {
            assert!(!reason.is_empty());
        }
    }

    #[test]
    fn plain_copy_keeps_contents_and_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let warm = warm_dir(tmp.path());
        #[cfg(unix)]
        std::os::unix::fs::symlink("deps/libx.rlib", warm.join("debug/link")).unwrap();
        let dest = tmp.path().join("copy");
        copy_tree(&warm, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join(".rustc_info.json")).unwrap(), b"{}");
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(dest.join("debug/link")).unwrap(),
            Path::new("deps/libx.rlib")
        );
    }

    #[test]
    fn refuses_existing_missing_and_overlapping() {
        let tmp = tempfile::tempdir().unwrap();
        let warm = warm_dir(tmp.path());
        let taken = tmp.path().join("taken");
        std::fs::create_dir(&taken).unwrap();
        assert!(matches!(
            seed_target_dir(&warm, &taken),
            Err(SeedError::DestExists(_))
        ));
        assert!(matches!(
            seed_target_dir(&tmp.path().join("nope"), &tmp.path().join("d")),
            Err(SeedError::NoWarmDir(_))
        ));
        assert!(matches!(
            seed_target_dir(&warm, &warm.join("inner")),
            Err(SeedError::Overlap { .. })
        ));
    }

    /// A clone only: the seed is a copy-on-write clone, or there is none and
    /// the destination is left absent. It never falls back to a byte copy.
    #[test]
    fn a_clone_only_seed_is_a_clone_or_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let warm = warm_dir(tmp.path());
        let dest = tmp.path().join("wt/target/debug");
        match clone_target_dir(&warm, &dest) {
            Ok(report) => {
                assert!(matches!(
                    report.method,
                    SeedMethod::Clone | SeedMethod::Reflink
                ));
                assert_eq!(
                    report.dest,
                    kgraf_core::plain_path::canonicalize(&dest).unwrap()
                );
                assert_eq!(
                    std::fs::read(dest.join("debug/deps/libx.rlib")).unwrap(),
                    b"artifact"
                );
                assert_eq!(std::fs::read(dest.join(".rustc_info.json")).unwrap(), b"{}");
            }
            Err(SeedError::NoClone(why)) => {
                assert!(!why.is_empty());
                assert!(!dest.exists(), "no partial seed is left");
            }
            Err(other) => panic!("{other}"),
        }
        // The same folder, cloned from its own `debug` profile.
        let dest = tmp.path().join("wt2/target/debug");
        match clone_target_dir(&warm.join("debug"), &dest) {
            Ok(report) => {
                assert!(matches!(
                    report.method,
                    SeedMethod::Clone | SeedMethod::Reflink
                ));
                assert_eq!(
                    std::fs::read(dest.join("deps/libx.rlib")).unwrap(),
                    b"artifact"
                );
                // Writing the clone never reaches the warm folder.
                std::fs::write(dest.join("deps/libx.rlib"), b"changed").unwrap();
                assert_eq!(
                    std::fs::read(warm.join("debug/deps/libx.rlib")).unwrap(),
                    b"artifact"
                );
            }
            Err(SeedError::NoClone(_)) => assert!(!dest.exists()),
            Err(other) => panic!("{other}"),
        }
    }

    #[test]
    fn a_clone_only_seed_refuses_what_a_seed_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let warm = warm_dir(tmp.path());
        let taken = tmp.path().join("taken");
        std::fs::create_dir(&taken).unwrap();
        assert!(matches!(
            clone_target_dir(&warm, &taken),
            Err(SeedError::DestExists(_))
        ));
        assert!(matches!(
            clone_target_dir(&tmp.path().join("nope"), &tmp.path().join("d")),
            Err(SeedError::NoWarmDir(_))
        ));
        assert!(matches!(
            clone_target_dir(&warm, &warm.join("inner")),
            Err(SeedError::Overlap { .. })
        ));
    }

    /// Where `cp` cannot clone (a file it cannot read), a clone-only seed
    /// reports it and leaves nothing behind, where a seed with a copy
    /// fallback would have tried a byte copy.
    #[cfg(unix)]
    #[test]
    fn a_clone_that_fails_leaves_no_partial_folder() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let warm = warm_dir(tmp.path());
        let locked = warm.join("debug/deps/locked.rlib");
        std::fs::write(&locked, b"x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // A process allowed to read it anyway (root) cannot show the failure.
        if std::fs::read(&locked).is_ok() {
            return;
        }
        let dest = tmp.path().join("wt/target");
        let err = clone_target_dir(&warm, &dest).expect_err("a file cp cannot read");
        assert!(matches!(err, SeedError::NoClone(_)), "{err}");
        assert!(!dest.exists(), "no partial seed is left");
        // A seed that may copy tries the byte copy, which fails the same way.
        let dest2 = tmp.path().join("wt2/target");
        assert!(seed_target_dir(&warm, &dest2).is_err());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    /// A partial seed is removed whole; one that is not there is no error;
    /// anything else that stops the removal is.
    #[test]
    fn a_partial_seed_is_removed_and_a_missing_one_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let partial = tmp.path().join("partial");
        std::fs::create_dir_all(partial.join("a/b")).unwrap();
        std::fs::write(partial.join("a/b/f"), b"x").unwrap();
        remove_partial(&partial).unwrap();
        assert!(!partial.exists());
        remove_partial(&tmp.path().join("never-made")).unwrap();
        // A file is not a folder: the removal fails, and says so.
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(matches!(remove_partial(&file), Err(SeedError::Io { .. })));
        assert!(file.exists());
    }
}
