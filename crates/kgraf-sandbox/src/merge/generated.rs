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
}
