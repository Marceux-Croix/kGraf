//! Files a build or a test run generates as a side effect, which a worker's
//! own shell can leave in its tree without meaning to write them: a lockfile
//! (when the project commits none) and Python bytecode (when no `.gitignore`
//! covers it). The rule for leaving one out of an attempt's commit is in
//! [`super::MergeBack::commit_retaining`]; this module only classifies names.

use std::path::{Path, PathBuf};

/// Lockfiles, by file name, with the manifests one belongs to. A lockfile is
/// only recognized next to one of its manifests: a `Cargo.lock` in a folder
/// with no `Cargo.toml` is not a build's side effect.
pub const LOCKFILES: &[(&str, &[&str])] = &[
    ("Cargo.lock", &["Cargo.toml"]),
    ("package-lock.json", &["package.json"]),
    ("npm-shrinkwrap.json", &["package.json"]),
    ("yarn.lock", &["package.json"]),
    ("pnpm-lock.yaml", &["package.json"]),
    ("bun.lock", &["package.json"]),
    ("bun.lockb", &["package.json"]),
    ("poetry.lock", &["pyproject.toml"]),
    ("uv.lock", &["pyproject.toml"]),
    ("pdm.lock", &["pyproject.toml"]),
    ("Pipfile.lock", &["Pipfile"]),
    ("Gemfile.lock", &["Gemfile", "gems.rb"]),
    ("go.sum", &["go.mod"]),
    ("composer.lock", &["composer.json"]),
    ("flake.lock", &["flake.nix"]),
    ("mix.lock", &["mix.exs"]),
    ("pubspec.lock", &["pubspec.yaml"]),
    ("Package.resolved", &["Package.swift"]),
    ("gradle.lockfile", &["build.gradle", "build.gradle.kts"]),
];

/// The manifests a lockfile named like `path` belongs to, or `None` when the
/// name is no lockfile's.
pub fn lockfile_manifests(path: &Path) -> Option<&'static [&'static str]> {
    let name = path.file_name()?.to_str()?;
    LOCKFILES
        .iter()
        .find(|(lock, _)| *lock == name)
        .map(|(_, manifests)| *manifests)
}

/// The manifests a lockfile at `path` would belong to, as paths in the same
/// folder, or empty when the name is no lockfile's. Joined with `/`, the
/// separator of a repository path on every platform (git's index uses it).
pub fn manifests_beside(path: &Path) -> Vec<PathBuf> {
    let Some(manifests) = lockfile_manifests(path) else {
        return Vec::new();
    };
    let text = path.to_string_lossy();
    let dir = text.rsplit_once(['/', '\\']).map(|(d, _)| d);
    manifests
        .iter()
        .map(|m| match dir {
            Some(d) => PathBuf::from(format!("{d}/{m}")),
            None => PathBuf::from(m),
        })
        .collect()
}

/// What a build or test run generated, if `path` is such a file: Python
/// bytecode, or a lockfile with one of its manifests beside it
/// (`manifest_present` answers for a path from [`manifests_beside`]).
///
/// The one rule for a worker-made file, applied in two places so they cannot
/// disagree: merge-back leaves such a file out of an attempt's commit
/// ([`super::MergeBack::commit_retaining`], which also requires the file to be
/// new against the base), and the orchestrator's scope check, which runs first,
/// passes it on to that commit instead of failing the attempt for it.
pub fn generated_kind(
    path: &Path,
    manifest_present: impl Fn(&Path) -> bool,
) -> Option<GeneratedKind> {
    if is_python_bytecode(path) {
        return Some(GeneratedKind::PythonBytecode);
    }
    manifests_beside(path)
        .iter()
        .any(|m| manifest_present(m))
        .then_some(GeneratedKind::Lockfile)
}

/// `**/__pycache__/*.pyc` or `**/__pycache__/*.pyo`.
pub fn is_python_bytecode(path: &Path) -> bool {
    let in_cache = path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|d| d == "__pycache__");
    let ext = path.extension().and_then(|e| e.to_str());
    in_cache && matches!(ext, Some("pyc" | "pyo"))
}

/// What kind of generated file a worker's run left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratedKind {
    Lockfile,
    PythonBytecode,
}

/// A file the worker's run made that was left out of its commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excluded {
    pub path: PathBuf,
    pub kind: GeneratedKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_classified_by_file_name_and_folder() {
        assert_eq!(
            lockfile_manifests(Path::new("sub/Cargo.lock")),
            Some(&["Cargo.toml"][..])
        );
        assert!(lockfile_manifests(Path::new("Cargo.lock.bak")).is_none());
        assert!(lockfile_manifests(Path::new("node_modules/.package-lock.json")).is_none());
        assert!(is_python_bytecode(Path::new(
            "pkg/__pycache__/m.cpython-312.pyc"
        )));
        assert!(is_python_bytecode(Path::new("__pycache__/m.pyo")));
        assert!(!is_python_bytecode(Path::new("pkg/m.pyc")));
        assert!(!is_python_bytecode(Path::new("__pycache__/deep/m.pyc")));
        assert!(!is_python_bytecode(Path::new("__pycache__/m.py")));
    }

    #[test]
    fn a_lockfile_is_generated_only_beside_one_of_its_manifests() {
        let present = ["Cargo.toml", "web/package.json", "py/pyproject.toml"];
        let has = |m: &Path| present.iter().any(|p| Path::new(p) == m);
        for lock in [
            "Cargo.lock",
            "web/package-lock.json",
            "web/pnpm-lock.yaml",
            "web/yarn.lock",
            "py/uv.lock",
            "py/poetry.lock",
        ] {
            assert_eq!(
                generated_kind(Path::new(lock), has),
                Some(GeneratedKind::Lockfile),
                "{lock}"
            );
        }
        for stray in [
            "docs/Cargo.lock",
            "web/uv.lock",
            "py/yarn.lock",
            "src/lib.rs",
        ] {
            assert_eq!(generated_kind(Path::new(stray), has), None, "{stray}");
        }
        assert_eq!(
            generated_kind(Path::new("a/__pycache__/m.pyc"), has),
            Some(GeneratedKind::PythonBytecode)
        );
        assert_eq!(
            manifests_beside(Path::new("a/b/Gemfile.lock")),
            vec![PathBuf::from("a/b/Gemfile"), PathBuf::from("a/b/gems.rb")]
        );
    }
}
