//! Files a build or a test run generates as a side effect, which a worker's
//! own shell can leave in its tree without meaning to write them: a lockfile
//! (when the project commits none), Python bytecode, and a Python package
//! build's output (`dist/`, `*.egg-info/`, `build/`) when no `.gitignore`
//! covers them. The rule for leaving one out of an attempt's commit is in
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

/// The manifests of a Python project, any of which a package build runs from.
pub const PYTHON_BUILD_MANIFESTS: &[&str] = &["pyproject.toml", "setup.py", "setup.cfg"];

/// `dir` joined with each of `names`, with `/`; `dir` empty is the root.
fn joined(dir: &str, names: &[&str]) -> Vec<PathBuf> {
    names
        .iter()
        .map(|m| {
            if dir.is_empty() {
                PathBuf::from(m)
            } else {
                PathBuf::from(format!("{dir}/{m}"))
            }
        })
        .collect()
}

/// The project directories a Python package build that made `path` ran in,
/// or empty when `path` is no such build's output. The outputs, as
/// `python -m build`, `pip wheel`, setuptools and Poetry leave them in the
/// project's tree:
///
/// - `<dir>/dist/<name>.whl` and `<dir>/dist/<name>.tar.gz` (the wheel and
///   the source distribution);
/// - `<dir>/<name>.egg-info/...` (setuptools' metadata; `<dir>/src/` for a
///   `src` layout, whose project is `<dir>`);
/// - `<dir>/build/lib/...`, `<dir>/build/lib.<platform>/...` and
///   `<dir>/build/bdist.<platform>/...` (setuptools' staging tree).
fn package_build_dirs(path: &Path) -> Vec<String> {
    let text = path.to_string_lossy();
    let parts: Vec<&str> = text.split(['/', '\\']).collect();
    let n = parts.len();
    let dir = |end: usize| parts[..end].join("/");
    let mut dirs = Vec::new();
    let name = parts[n - 1];
    if n >= 2
        && parts[n - 2] == "dist"
        && (name.ends_with(".whl") || name.ends_with(".tar.gz"))
        && name.len() > ".whl".len()
    {
        dirs.push(dir(n - 2));
    }
    for i in 0..n.saturating_sub(1) {
        let part = parts[i];
        if part.len() > ".egg-info".len() && part.ends_with(".egg-info") {
            dirs.push(dir(i));
            if i >= 1 && parts[i - 1] == "src" {
                dirs.push(dir(i - 1));
            }
        }
        if part == "build"
            && i + 2 < n
            && (parts[i + 1] == "lib"
                || parts[i + 1].starts_with("lib.")
                || parts[i + 1].starts_with("bdist."))
        {
            dirs.push(dir(i));
        }
    }
    dirs
}

/// The manifests a generated file at `path` would belong to, or empty when
/// `path` is no generated file's: for a lockfile, its manifests in the same
/// folder; for a Python package build's output, the Python project manifests
/// of the folder it was built in. Joined with `/`, the separator of a
/// repository path on every platform (git's index uses it).
pub fn manifests_beside(path: &Path) -> Vec<PathBuf> {
    if let Some(manifests) = lockfile_manifests(path) {
        let text = path.to_string_lossy();
        let dir = text.rsplit_once(['/', '\\']).map_or("", |(d, _)| d);
        return joined(dir, manifests);
    }
    package_build_dirs(path)
        .iter()
        .flat_map(|d| joined(d, PYTHON_BUILD_MANIFESTS))
        .collect()
}

/// What a build or test run generated, if `path` is such a file: Python
/// bytecode, a lockfile with one of its manifests beside it, or a Python
/// package build's output with its project's manifest present
/// (`manifest_present` answers for a path from [`manifests_beside`]).
///
/// The one rule for a worker-made file, applied in two places so they cannot
/// disagree: merge-back leaves such a file out of an attempt's commit
/// ([`super::MergeBack::commit_retaining`], which also requires the file to be
/// new against the base), and the orchestrator's scope check, which runs first,
/// passes it on to that commit instead of failing the attempt for it.
///
/// A package build's output is here because a contract whose work is a build
/// script runs it, as its instruction and acceptance say, and the build
/// leaves its wheel in `dist/`: that attempt was failed for writing outside
/// its scope although no contract writes `dist/` and the acceptance check
/// builds it again.
pub fn generated_kind(
    path: &Path,
    manifest_present: impl Fn(&Path) -> bool,
) -> Option<GeneratedKind> {
    if is_python_bytecode(path) {
        return Some(GeneratedKind::PythonBytecode);
    }
    if !manifests_beside(path).iter().any(|m| manifest_present(m)) {
        return None;
    }
    Some(if lockfile_manifests(path).is_some() {
        GeneratedKind::Lockfile
    } else {
        GeneratedKind::PackageBuild
    })
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
    /// A Python package build's output: a wheel or source distribution in
    /// `dist/`, `*.egg-info/`, or setuptools' `build/` staging tree.
    PackageBuild,
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

    /// Round 10 Terminal-Bench (pypi-server): the contract that owned only
    /// `dev/setup_pypi.sh` ran it, the build left the wheel and the source
    /// distribution in `dist/`, and the attempt failed as a write outside
    /// its scope.
    #[test]
    fn a_python_package_build_is_generated_only_beside_its_project() {
        let present = ["pyproject.toml", "py/setup.py", "lib/setup.cfg"];
        let has = |m: &Path| present.iter().any(|p| Path::new(p) == m);
        for out in [
            "dist/vectorops-0.1.0-py2.py3-none-any.whl",
            "dist/vectorops-0.1.0.tar.gz",
            "vectorops.egg-info/PKG-INFO",
            "src/vectorops.egg-info/SOURCES.txt",
            "build/lib/vectorops/__init__.py",
            "build/bdist.linux-x86_64/wheel/vectorops/__init__.py",
            "py/dist/pkg-1.0-py3-none-any.whl",
            "py/build/lib.linux-x86_64-cpython-313/pkg/_ext.so",
            "lib/pkg.egg-info/top_level.txt",
        ] {
            assert_eq!(
                generated_kind(Path::new(out), has),
                Some(GeneratedKind::PackageBuild),
                "{out}"
            );
        }
        for not_out in [
            // No Python project where it was built.
            "web/dist/app-1.0.tar.gz",
            "docs/dist/x.whl",
            // Not a build's output name.
            "dist/notes.txt",
            "dist/.whl",
            "build/script.sh",
            "build/lib",
            "tools/build/lib/x.py",
            ".egg-info/x",
            "dev/setup_pypi.sh",
        ] {
            assert_eq!(generated_kind(Path::new(not_out), has), None, "{not_out}");
        }
        assert_eq!(
            manifests_beside(Path::new("py/dist/p-1.whl")),
            vec![
                PathBuf::from("py/pyproject.toml"),
                PathBuf::from("py/setup.py"),
                PathBuf::from("py/setup.cfg"),
            ]
        );
    }
}
