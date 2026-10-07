//! The Gradle wrapper's distribution, reachable offline from a Gradle home
//! of the command's own (decision 0084, amended 2026-10-06).
//!
//! **Why.** A sandboxed command's Gradle home is a folder in its temporary
//! folder (`GRADLE_USER_HOME`): the user's home holds `gradle.properties`
//! and init scripts with repository passwords, and Gradle locks and writes
//! its home on every build. The wrapper (`./gradlew`) looks for its
//! distribution in `<home>/wrapper/dists/<name>/<hash>/`, takes a lock file
//! there, and downloads the distribution when it is absent: in an empty home
//! with the network off it fails, so a wrapper project had no gate.
//!
//! **What this does.** For the distribution the project's
//! `gradle/wrapper/gradle-wrapper.properties` names, each `<hash>` folder
//! of the user's `~/.gradle/wrapper/dists/<name>/` that holds a finished
//! unpack (its `<name>.zip.ok` mark) is made again in the command's Gradle
//! home: the folder itself (writable, for the lock file), an empty `.ok`
//! mark, and a link to each unpacked distribution folder. The wrapper takes
//! the folder as installed, writes its lock beside the link, and runs the
//! user's distribution read-only. Nothing in the user's home is written.
//! The folders are made one no-follow `openat` at a time and the files with
//! `O_NOFOLLOW`: the temporary folder is writable by earlier commands, and a
//! link left there never makes the host write outside it.
//!
//! Measured (macOS, APFS, debug build, 2026-10-06, test
//! `the_wrapper_distribution_is_reachable_and_the_user_home_is_not_written`):
//! well under a millisecond per command; nothing is copied (the test counts
//! the files and bytes made instead of timing the call).
//!
//! Not run here: a real `./gradlew` (Gradle was not installed on the machine
//! this was written on). The layout follows the wrapper's documented
//! `PathAssembler` and `Install` behavior: the `<hash>` folder name is taken
//! from the user's home as it is, never computed.
//!
//! Written new. Searched: this repository (the cargo home of
//! `sandbox::cargo_home` makes the same kind of folder for cargo); no crate
//! prepares a Gradle home.

use std::path::{Path, PathBuf};

/// The distribution name the wrapper of the project at `project` uses
/// (`gradle-8.5-bin` for `.../gradle-8.5-bin.zip`), from its
/// `gradle/wrapper/gradle-wrapper.properties`; `None` when there is no
/// wrapper or no plain name.
pub fn wrapper_distribution(project: &Path) -> Option<String> {
    let text =
        std::fs::read_to_string(project.join("gradle/wrapper/gradle-wrapper.properties")).ok()?;
    let url = text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "distributionUrl").then(|| value.trim().replace("\\:", ":"))
    })?;
    let file = url.rsplit('/').next()?;
    let name = file.strip_suffix(".zip")?;
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c));
    plain.then(|| name.to_owned())
}

/// The finished unpacks of distribution `name` in the user's Gradle home at
/// `gradle_home` (`~/.gradle`): each `wrapper/dists/<name>/<hash>` folder
/// holding the `<name>.zip.ok` mark, with the unpacked folders in it.
pub fn installed_distributions(gradle_home: &Path, name: &str) -> Vec<(PathBuf, Vec<PathBuf>)> {
    let dists = gradle_home.join("wrapper/dists").join(name);
    let Ok(hashes) = std::fs::read_dir(&dists) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for hash in hashes.flatten() {
        let dir = hash.path();
        if !hash.file_type().is_ok_and(|k| k.is_dir())
            || !dir.join(format!("{name}.zip.ok")).is_file()
        {
            continue;
        }
        let unpacked: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_type().is_ok_and(|k| k.is_dir()))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default();
        if !unpacked.is_empty() {
            out.push((dir, unpacked));
        }
    }
    out.sort();
    out
}

/// Make the wrapper distribution of the project at `project` reachable from
/// the command Gradle home `<tmp>/gradle-home` ([module docs](self)).
/// `root` is the writable root holding `tmp`. Returns how many `<hash>`
/// folders were made; zero when the project has no wrapper or the user's
/// home has no finished unpack of its distribution.
#[cfg(unix)]
pub(crate) fn prepare(
    gradle_home: &Path,
    root: &Path,
    tmp: &Path,
    project: &Path,
) -> std::io::Result<usize> {
    let Some(name) = wrapper_distribution(project) else {
        return Ok(0);
    };
    let installed = installed_distributions(gradle_home, &name);
    if installed.is_empty() {
        return Ok(0);
    }
    let rel = tmp
        .strip_prefix(root)
        .map_err(|_| std::io::Error::other("the temporary folder is not under its root"))?
        .join("gradle-home/wrapper/dists")
        .join(&name);
    let dists = unix::open_dirs(root, &rel)?;
    for (hash_dir, unpacked) in &installed {
        let Some(hash) = hash_dir.file_name() else {
            continue;
        };
        let dir = unix::open_dirs_from(&dists, Path::new(hash))?;
        unix::touch(&dir, &format!("{name}.zip.ok"))?;
        for folder in unpacked {
            if let Some(leaf) = folder.file_name() {
                unix::link(&dir, Path::new(leaf), folder)?;
            }
        }
    }
    Ok(installed.len())
}

/// [`prepare`] on Windows, by path ([`crate::dependencies`]'s by-path
/// links): each folder is made only through real folders, the mark with
/// `create_new`, and each unpacked distribution is reached through a
/// directory junction (a symbolic link needs a privilege there). Runs after
/// the sandbox set the writable root's ACL. Not run on Windows here.
#[cfg(windows)]
pub(crate) fn prepare(
    gradle_home: &Path,
    root: &Path,
    tmp: &Path,
    project: &Path,
) -> std::io::Result<usize> {
    use crate::dependencies::by_path::{Junctions, make_dirs, replace_with_link};
    let Some(name) = wrapper_distribution(project) else {
        return Ok(0);
    };
    let installed = installed_distributions(gradle_home, &name);
    if installed.is_empty() {
        return Ok(0);
    }
    let rel = tmp
        .strip_prefix(root)
        .map_err(|_| std::io::Error::other("the temporary folder is not under its root"))?
        .join("gradle-home/wrapper/dists")
        .join(&name);
    for (hash_dir, unpacked) in &installed {
        let Some(hash) = hash_dir.file_name() else {
            continue;
        };
        let dir = make_dirs(&Junctions, root, &rel.join(hash))?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(format!("{name}.zip.ok")))
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        for folder in unpacked {
            if let Some(leaf) = folder.file_name() {
                replace_with_link(&Junctions, &dir, Path::new(leaf), folder)?;
            }
        }
    }
    Ok(installed.len())
}

#[cfg(unix)]
mod unix {
    use cap_primitives::ambient_authority;
    use cap_primitives::fs::{
        DirOptions, FollowSymlinks, OpenOptions, create_dir, open, open_ambient_dir,
        open_dir_nofollow, remove_file, stat,
    };
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path};

    /// Open `rel` beneath `root`, creating each absent folder, one no-follow
    /// `openat` at a time: a link on the way fails.
    pub(super) fn open_dirs(root: &Path, rel: &Path) -> io::Result<File> {
        let dir = open_ambient_dir(root, ambient_authority())?;
        open_dirs_from(&dir, rel)
    }

    pub(super) fn open_dirs_from(start: &File, rel: &Path) -> io::Result<File> {
        let mut dir = start.try_clone()?;
        for c in rel.components() {
            let Component::Normal(name) = c else {
                return Err(io::Error::other("not a plain relative path"));
            };
            let name = Path::new(name);
            match create_dir(&dir, name, &DirOptions::new()) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            dir = open_dir_nofollow(&dir, name)?;
        }
        Ok(dir)
    }

    /// An empty file `name` in `dir`, made when absent, never through a link.
    pub(super) fn touch(dir: &File, name: &str) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create(true)
            ._cap_fs_ext_follow(FollowSymlinks::No);
        open(dir, Path::new(name), &options).map(drop)
    }

    /// `name` in `dir` a link to `target`; whatever else was there (a file
    /// or link an earlier command left) is replaced, a folder is an error.
    pub(super) fn link(dir: &File, name: &Path, target: &Path) -> io::Result<()> {
        match stat(dir, name, FollowSymlinks::No) {
            Ok(m) if m.is_dir() => {
                return Err(io::Error::other(format!(
                    "{} is a folder in the command's Gradle home",
                    name.display()
                )));
            }
            Ok(_) => remove_file(dir, name)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let to = CString::new(target.as_os_str().as_bytes())?;
        let at = CString::new(name.as_os_str().as_bytes())?;
        // SAFETY: both strings are NUL-terminated and `dir` is open.
        let rc = unsafe { libc::symlinkat(to.as_ptr(), dir.as_raw_fd(), at.as_ptr()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    const PROPERTIES: &str = "distributionBase=GRADLE_USER_HOME\n\
distributionPath=wrapper/dists\n\
distributionUrl=https\\://services.gradle.org/distributions/gradle-8.5-bin.zip\n";

    #[test]
    fn the_distribution_name_is_read_from_the_wrapper_properties() {
        let p = tempfile::tempdir().expect("dir");
        assert_eq!(wrapper_distribution(p.path()), None);
        write(
            &p.path().join("gradle/wrapper/gradle-wrapper.properties"),
            PROPERTIES,
        );
        assert_eq!(
            wrapper_distribution(p.path()).as_deref(),
            Some("gradle-8.5-bin")
        );
        write(
            &p.path().join("gradle/wrapper/gradle-wrapper.properties"),
            "distributionUrl=https\\://x/../../evil$(x).zip\n",
        );
        assert_eq!(wrapper_distribution(p.path()), None);
    }

    /// Regular files, their total bytes and symlinks under `dir`, counting a
    /// symlink as one entry and never following it.
    fn count_entries(dir: &Path) -> (usize, u64, usize) {
        let mut total = (0, 0, 0);
        for entry in std::fs::read_dir(dir).expect("list").flatten() {
            let meta = std::fs::symlink_metadata(entry.path()).expect("meta");
            if meta.file_type().is_symlink() {
                total.2 += 1;
            } else if meta.is_dir() {
                let (f, b, l) = count_entries(&entry.path());
                total = (total.0 + f, total.1 + b, total.2 + l);
            } else {
                total.0 += 1;
                total.1 += meta.len();
            }
        }
        total
    }

    /// The command's Gradle home gets the wrapper's folder for each
    /// finished unpack: a writable folder (the wrapper's lock goes there),
    /// the `.ok` mark, and a link to the user's unpacked distribution. The
    /// user's home is not written; a link an earlier command planted in the
    /// temporary folder is never followed.
    #[test]
    fn the_wrapper_distribution_is_reachable_and_the_user_home_is_not_written() {
        let home = tempfile::tempdir().expect("home");
        let g = home.path().join(".gradle");
        let real = g.join("wrapper/dists/gradle-8.5-bin/abc123");
        write(&real.join("gradle-8.5/lib/gradle-launcher.jar"), "jar");
        write(&real.join("gradle-8.5-bin.zip.ok"), "");
        // An unpack still running (no mark) is not offered.
        write(
            &g.join("wrapper/dists/gradle-8.5-bin/def456/gradle-8.5/x"),
            "",
        );
        let project = tempfile::tempdir().expect("project");
        write(
            &project
                .path()
                .join("gradle/wrapper/gradle-wrapper.properties"),
            PROPERTIES,
        );
        let scratch = tempfile::tempdir().expect("scratch");
        let root = std::fs::canonicalize(scratch.path()).expect("canonical");
        let tmp = root.clone();
        assert_eq!(
            prepare(&g, &root, &tmp, project.path()).expect("prepare"),
            1
        );
        // Nothing is copied: the only regular file made is the empty `.ok`
        // mark, and the distribution is one link. Counted by content, not
        // by wall-clock time, which a loaded machine bends.
        let (files, bytes, links) = count_entries(&tmp.join("gradle-home"));
        assert_eq!((files, bytes, links), (1, 0, 1));
        let mine = tmp.join("gradle-home/wrapper/dists/gradle-8.5-bin/abc123");
        assert!(mine.join("gradle-8.5-bin.zip.ok").is_file());
        assert_eq!(
            std::fs::read_to_string(mine.join("gradle-8.5/lib/gradle-launcher.jar")).expect("read"),
            "jar"
        );
        assert!(
            std::fs::symlink_metadata(mine.join("gradle-8.5"))
                .expect("link")
                .file_type()
                .is_symlink()
        );
        assert!(
            !tmp.join("gradle-home/wrapper/dists/gradle-8.5-bin/def456")
                .exists()
        );
        // The wrapper's lock goes beside the link, in the command's home.
        std::fs::write(mine.join("gradle-8.5-bin.zip.lck"), "").expect("lock");
        assert!(!real.join("gradle-8.5-bin.zip.lck").exists());
        // Again: idempotent.
        assert_eq!(prepare(&g, &root, &tmp, project.path()).expect("again"), 1);

        // A link planted where the command home goes is never followed.
        let other = tempfile::tempdir().expect("other");
        let root2 = std::fs::canonicalize(other.path()).expect("canonical");
        std::os::unix::fs::symlink(&g, root2.join("gradle-home")).expect("plant");
        assert!(prepare(&g, &root2, &root2, project.path()).is_err());
        assert!(
            !g.join("wrapper/dists/gradle-8.5-bin/abc123/gradle-8.5-bin.zip.lck")
                .exists()
        );
        let names: Vec<_> = std::fs::read_dir(&g)
            .expect("list")
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, ["wrapper"]);
    }

    /// A real folder where the distribution link goes is refused by name,
    /// and left in place.
    #[test]
    fn a_real_folder_where_the_distribution_link_goes_is_refused_by_name() {
        let home = tempfile::tempdir().expect("home");
        let g = home.path().join(".gradle");
        let real = g.join("wrapper/dists/gradle-8.5-bin/abc123");
        write(&real.join("gradle-8.5-bin.zip.ok"), "");
        write(&real.join("gradle-8.5/x"), "");
        let project = tempfile::tempdir().expect("project");
        write(
            &project
                .path()
                .join("gradle/wrapper/gradle-wrapper.properties"),
            PROPERTIES,
        );
        let scratch = tempfile::tempdir().expect("scratch");
        let root = std::fs::canonicalize(scratch.path()).expect("canonical");
        let mine = root.join("gradle-home/wrapper/dists/gradle-8.5-bin/abc123/gradle-8.5");
        std::fs::create_dir_all(&mine).expect("mkdir");
        let err = prepare(&g, &root, &root, project.path())
            .expect_err("a folder in the way")
            .to_string();
        assert!(
            err.contains("is a folder in the command's Gradle home"),
            "{err}"
        );
        assert!(mine.is_dir());
    }

    /// A folder that cannot be made is that failure, not a later one from
    /// opening a folder that is not there.
    #[test]
    fn a_folder_that_cannot_be_made_is_reported_as_it_is() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().expect("home");
        let g = home.path().join(".gradle");
        write(
            &g.join("wrapper/dists/gradle-8.5-bin/abc123/gradle-8.5-bin.zip.ok"),
            "",
        );
        write(
            &g.join("wrapper/dists/gradle-8.5-bin/abc123/gradle-8.5/x"),
            "",
        );
        let project = tempfile::tempdir().expect("project");
        write(
            &project
                .path()
                .join("gradle/wrapper/gradle-wrapper.properties"),
            PROPERTIES,
        );
        let scratch = tempfile::tempdir().expect("scratch");
        let root = std::fs::canonicalize(scratch.path()).expect("canonical");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500)).expect("chmod");
        let can_write = std::fs::File::create(root.join(".probe")).is_ok();
        let err = prepare(&g, &root, &root, project.path()).err();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        if !can_write {
            assert_eq!(
                err.map(|e| e.kind()),
                Some(std::io::ErrorKind::PermissionDenied)
            );
        }
    }
}
