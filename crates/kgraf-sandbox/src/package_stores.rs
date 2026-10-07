//! Package stores kGraf fills for one project when it runs the project's
//! dependency install itself (decision 0087, amended 2026-10-07).
//!
//! Four package managers keep what an install downloads in a store outside
//! the project, not in it: Go's module cache, Maven's local repository,
//! Gradle's dependency cache and NuGet's global packages folder. An install
//! kGraf runs runs project code (Maven plugins, Gradle build scripts, MSBuild
//! targets), so it never writes the user's own stores, which every other
//! project on the machine reads. It writes a store of this project's own,
//! under the checkout's state directory:
//!
//! ```text
//!   <checkout>/.kgraf/package-stores/
//!     go/       GOMODCACHE
//!     maven/    the Maven local repository (-Dmaven.repo.local)
//!     gradle/   GRADLE_USER_HOME of the install; its caches/ is read-only later
//!     nuget/    NUGET_PACKAGES
//!     ...       download caches of the other package managers and the
//!               install's own home folder, which no later command reads
//! ```
//!
//! A sandboxed command of the same project then reads the store it needs,
//! never writes it ([`crate::SandboxPolicy::with_dependency_links`]): the
//! store is a readable root (the state directory around it stays denied), and
//! the package manager is pointed at it by its own variable. A store a
//! command finds is used instead of the user's own, since the install that
//! filled it resolved this project's dependencies.
//!
//! Written new; nothing in this repository or the sandboxes it ports keeps a
//! per-project store.

use std::path::{Path, PathBuf};

/// The folder, in the checkout's state directory, that holds the stores.
pub const PACKAGE_STORES_DIR: &str = "package-stores";

/// The stores of one project that a later command reads. Each is canonical
/// and a real folder (never a link), or `None` when kGraf has filled none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageStores {
    /// Go's module cache (`GOMODCACHE`).
    pub go: Option<PathBuf>,
    /// Maven's local repository.
    pub maven: Option<PathBuf>,
    /// The install's Gradle user home: its `caches` (holding `modules-2`)
    /// is a later command's `GRADLE_RO_DEP_CACHE`, and its `wrapper` the
    /// source of the Gradle wrapper's distribution.
    pub gradle_home: Option<PathBuf>,
    /// NuGet's global packages folder (`NUGET_PACKAGES`).
    pub nuget: Option<PathBuf>,
}

/// Where the stores of the project checked out at `checkout` live.
pub fn package_stores_root(checkout: &Path) -> PathBuf {
    checkout
        .join(kgraf_core::legacy::STATE_DIR)
        .join(PACKAGE_STORES_DIR)
}

/// The store folder an install of each kind writes, relative to
/// [`package_stores_root`].
pub mod layout {
    pub const GO: &str = "go";
    pub const MAVEN: &str = "maven";
    /// The install's `GRADLE_USER_HOME`; later commands read its `caches`.
    pub const GRADLE_HOME: &str = "gradle";
    pub const NUGET: &str = "nuget";
}

/// The file kGraf writes in a store once an install into it succeeded. A
/// store without it (an install that failed half way, or none yet) is not
/// used: the user's own store is read instead.
pub const STORE_COMPLETE: &str = ".kgraf-store-complete";

/// Mark the store `rel` (a [`layout`] name) of the project at `checkout` as
/// filled by a successful install. The install could write the stores, so
/// the marker is written without following a link (decision 0083); on
/// Windows, which has no such walk, by path.
pub fn mark_store_complete(checkout: &Path, rel: &str) -> std::io::Result<()> {
    let root = package_stores_root(checkout);
    std::fs::create_dir_all(&root)?;
    let file = Path::new(rel).join(STORE_COMPLETE);
    #[cfg(unix)]
    {
        crate::write_confined(&root, &file, b"")
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(root.join(rel))?;
        std::fs::write(root.join(file), "")
    }
}

impl PackageStores {
    /// The stores kGraf filled for the project at `checkout`. A store that
    /// is a link, or lies below one, is not used: its target is not a folder
    /// kGraf made. Nor is one without [`STORE_COMPLETE`].
    pub fn find(checkout: &Path) -> Self {
        let root = package_stores_root(checkout);
        let real = |rel: &str| -> Option<PathBuf> {
            let path = root.join(rel);
            let meta = std::fs::symlink_metadata(&path).ok()?;
            if !meta.is_dir() || !path.join(STORE_COMPLETE).is_file() {
                return None;
            }
            let canonical = std::fs::canonicalize(&path).ok()?;
            // A link anywhere on the way resolves elsewhere.
            let expected = std::fs::canonicalize(checkout)
                .ok()?
                .join(kgraf_core::legacy::STATE_DIR)
                .join(PACKAGE_STORES_DIR)
                .join(rel);
            (canonical == expected).then_some(canonical)
        };
        Self {
            go: real(layout::GO),
            maven: real(layout::MAVEN),
            gradle_home: real(layout::GRADLE_HOME),
            nuget: real(layout::NUGET),
        }
    }

    /// Every store, for read grants.
    pub fn all(&self) -> Vec<&Path> {
        [&self.go, &self.maven, &self.gradle_home, &self.nuget]
            .into_iter()
            .flatten()
            .map(PathBuf::as_path)
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.all().is_empty()
    }
}

/// `path`, spelled so a JVM option can carry it. `JAVA_TOOL_OPTIONS` and
/// `MAVEN_OPTS` are split on white space (the `mvn` script expands
/// `$MAVEN_OPTS` unquoted), so `-Dmaven.repo.local=/a b/repo` breaks. A path
/// with white space is given as a link without it, in the system temporary
/// folder (`kgraf-jvm-<hash>`, owned by this user and pointing at `path`),
/// made when missing. A link there that another user owns or that points
/// elsewhere is never used: then `path` is returned as it is, and the tool
/// fails loud on it. Windows makes no link (a symbolic link needs a
/// privilege there) and returns `path`.
pub fn jvm_path(path: &Path) -> PathBuf {
    if !path.to_string_lossy().chars().any(char::is_whitespace) {
        return path.to_path_buf();
    }
    #[cfg(unix)]
    {
        use std::hash::{Hash, Hasher};
        use std::os::unix::fs::MetadataExt;
        // SipHash with fixed keys: the same name for the same path in every
        // process.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut h);
        let tmp =
            std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
        let alias = tmp.join(format!("kgraf-jvm-{:016x}", h.finish()));
        if alias.to_string_lossy().chars().any(char::is_whitespace) {
            return path.to_path_buf();
        }
        // SAFETY: getuid cannot fail.
        let me = unsafe { libc::getuid() };
        let ours = |alias: &Path| {
            std::fs::symlink_metadata(alias)
                .is_ok_and(|m| m.file_type().is_symlink() && m.uid() == me)
                && std::fs::read_link(alias).is_ok_and(|t| t == path)
        };
        if ours(&alias) {
            return alias;
        }
        if std::fs::symlink_metadata(&alias).is_err()
            && std::os::unix::fs::symlink(path, &alias).is_ok()
            && ours(&alias)
        {
            return alias;
        }
        path.to_path_buf()
    }
    #[cfg(not(unix))]
    {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path with a space gets a link without one, owned by this user and
    /// pointing at the path; a path without one is kept.
    #[cfg(unix)]
    #[test]
    fn a_path_with_white_space_gets_a_jvm_safe_link() {
        let dir = tempfile::tempdir().expect("dir");
        let plain = std::fs::canonicalize(dir.path())
            .expect("canonical")
            .join("store");
        std::fs::create_dir_all(&plain).expect("mkdir");
        assert_eq!(jvm_path(&plain), plain);
        let spaced = plain.join("with space");
        std::fs::create_dir_all(&spaced).expect("mkdir");
        let alias = jvm_path(&spaced);
        assert_ne!(alias, spaced);
        assert!(
            !alias.to_string_lossy().contains(' '),
            "{}",
            alias.display()
        );
        assert_eq!(std::fs::read_link(&alias).expect("a link"), spaced);
        assert_eq!(jvm_path(&spaced), alias, "the same link again");
        let _ = std::fs::remove_file(&alias);
    }

    #[test]
    fn only_real_store_folders_are_found() {
        let dir = tempfile::tempdir().expect("dir");
        let c = std::fs::canonicalize(dir.path()).expect("canonical");
        assert!(PackageStores::find(&c).is_empty());
        let root = package_stores_root(&c);
        std::fs::create_dir_all(root.join("go")).expect("go");
        std::fs::create_dir_all(root.join("gradle/caches")).expect("gradle");
        std::fs::write(root.join("nuget"), "a file").expect("nuget");
        assert!(
            PackageStores::find(&c).is_empty(),
            "a store an install did not finish is not used"
        );
        mark_store_complete(&c, layout::GO).expect("mark");
        mark_store_complete(&c, layout::GRADLE_HOME).expect("mark");
        let found = PackageStores::find(&c);
        assert_eq!(found.go, Some(root.join("go")));
        assert_eq!(found.gradle_home, Some(root.join("gradle")));
        assert_eq!(found.nuget, None, "a file is no store");
        assert_eq!(found.maven, None);
        #[cfg(unix)]
        {
            let elsewhere = tempfile::tempdir().expect("elsewhere");
            std::os::unix::fs::symlink(elsewhere.path(), root.join("maven")).expect("link");
            assert_eq!(PackageStores::find(&c).maven, None, "a link is no store");
        }
    }
}
