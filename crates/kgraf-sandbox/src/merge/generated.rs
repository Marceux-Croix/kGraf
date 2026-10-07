//! Files a build or a test run generates as a side effect, which a worker's
//! own shell can leave in its tree without meaning to write them when no
//! `.gitignore` covers them: a lockfile (when the project commits none),
//! Python bytecode, a Python package build's output (`dist/`, `*.egg-info/`,
//! `build/`), the output and install folders of the other languages' build
//! tools (`node_modules/`, `.venv/`, Maven's `target/`, Gradle's `build/`,
//! `bin/` and `obj/` of a .NET project, ...), tool caches, compiled objects
//! and compiled programs. The rule for leaving one out of an attempt's commit is in
//! [`super::MergeBack::commit_retaining`]; this module only classifies paths
//! (and, for a compiled program, the first bytes of the file).

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
    // NuGet's lock file (`RestorePackagesWithLockFile`), beside its project.
    ("packages.lock.json", &["*.csproj", "*.fsproj", "*.vbproj"]),
    ("Podfile.lock", &["Podfile"]),
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

/// Directories a build, an install or a test run writes into a project, by
/// name, with the manifests of the projects that make one beside it. A
/// manifest written `*.ext` is any file of that extension in the folder (a
/// .NET project file has the project's own name).
///
/// Cargo's `target/` is in the list for completeness only: a worker's cargo
/// builds into its scratch directory (`CARGO_TARGET_DIR`), outside the tree.
/// The other tools have no such setting kGraf can give every project, so
/// their output lands in the tree, and without this list a correct project
/// whose `.gitignore` does not name it failed its attempt as a write outside
/// its scope.
pub const OUTPUT_DIRS: &[(&str, &[&str])] = &[
    // npm, pnpm, yarn and bun installs; bundlers' and `tsc`'s usual output;
    // Next.js.
    ("node_modules", &["package.json"]),
    ("dist", &["package.json"]),
    (".next", &["package.json"]),
    // Jest's and c8's, SimpleCov's and PHPUnit's coverage reports.
    ("coverage", &["package.json", "Gemfile", "composer.json"]),
    // create-react-app, Gradle, and a CMake build in a `build/` folder.
    (
        "build",
        &[
            "package.json",
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
            "CMakeLists.txt",
        ],
    ),
    // Maven; cargo with no `CARGO_TARGET_DIR`.
    ("target", &["pom.xml", "Cargo.toml"]),
    // A CMake build configured in the source folder.
    ("CMakeFiles", &["CMakeLists.txt"]),
    // `dotnet build` and `dotnet test`.
    ("bin", &["*.csproj", "*.fsproj", "*.vbproj"]),
    ("obj", &["*.csproj", "*.fsproj", "*.vbproj"]),
    // Python virtual environments and coverage.py's HTML report.
    (".venv", PYTHON_PROJECT_FILES),
    ("venv", PYTHON_PROJECT_FILES),
    ("htmlcov", PYTHON_PROJECT_FILES),
    // Bundler's local configuration and install path; Composer's install.
    (".bundle", &["Gemfile"]),
    ("vendor", &["composer.json"]),
];

/// The paths kGraf links into a tree to reach the checkout's installed
/// dependencies ([`crate::link_dependency_trees`]), as their last parts, with
/// the manifests one stands beside (in the folder holding the first part).
/// git lists a link as one file, never a folder, so these match the whole
/// path's end, not a folder on it. An empty manifest list matches beside
/// any folder: a virtual environment is linked by its `pyvenv.cfg`, with or
/// without a Python manifest.
pub const DEPENDENCY_LINKS: &[(&[&str], &[&str])] = &[
    (&["node_modules"], &["package.json"]),
    (&[".pnp.cjs"], &["package.json"]),
    (&[".pnp.loader.mjs"], &["package.json"]),
    (&[".pnp.data.json"], &["package.json"]),
    (&[".yarn"], &["package.json"]),
    (&[".yarn", "cache"], &["package.json"]),
    (&[".yarn", "unplugged"], &["package.json"]),
    (&[".yarn", "install-state.gz"], &["package.json"]),
    (&[".venv"], &[]),
    (&["venv"], &[]),
    (&["vendor"], &["composer.json", "Gemfile", "go.mod"]),
    (&["vendor", "bundle"], &["Gemfile"]),
];

/// For a path ending like a [`DEPENDENCY_LINKS`] entry: the folder it stands
/// in and that entry's manifests.
fn dependency_link(parts: &[String]) -> Option<(String, &'static [&'static str])> {
    DEPENDENCY_LINKS.iter().find_map(|(tail, manifests)| {
        let n = tail.len();
        (parts.len() >= n && parts[parts.len() - n..].iter().eq(tail.iter()))
            .then(|| (parts[..parts.len() - n].join("/"), *manifests))
    })
}

/// Whether `path`, as git lists it (a link is one file), ends like a link
/// kGraf makes to the checkout's installed dependencies
/// ([`DEPENDENCY_LINKS`]). Such a path is neither the worker's file nor the
/// check's: notices about what a run left skip it.
pub fn is_dependency_link(path: &Path) -> bool {
    dependency_link(&parts(path)).is_some()
}

/// The files any of which marks a folder as a Python project, for its
/// virtual environment and coverage report.
const PYTHON_PROJECT_FILES: &[&str] = &[
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "Pipfile",
];

/// Directories only a tool writes, at any depth, whatever project holds
/// them: a test runner's, type checker's, linter's or build tool's cache.
/// Several of them (pytest's, mypy's, ruff's) put a `.gitignore` of their own
/// inside, which git already honors; the others do not.
pub const CACHE_DIRS: &[&str] = &[
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".hypothesis",
    ".tox",
    ".nox",
    ".nyc_output",
    ".gradle",
    ".parcel-cache",
    ".turbo",
    ".dart_tool",
    ".phpunit.cache",
    ".sass-cache",
];

/// Files only a tool writes, by name, at any depth: ESLint's cache, PHPUnit's
/// result cache, coverage.py's data file (`.coverage`, and `.coverage.<id>`
/// from a parallel run).
const CACHE_FILES: &[&str] = &[".eslintcache", ".phpunit.result.cache", ".coverage"];

/// Extensions of compiled output, at any depth: Java classes, C and C++
/// objects, and TypeScript's incremental build state. A worker writes none
/// of these with a file tool.
const COMPILED_EXTENSIONS: &[&str] = &["class", "o", "tsbuildinfo"];

/// The files `tsc` emits beside a TypeScript source when the project names
/// no output folder: `<stem>.js`, `<stem>.d.ts` and their source maps,
/// longest first (`x.d.ts.map` is the map of `x.d.ts`, whose source is
/// `x.ts`).
const TS_EMIT_SUFFIXES: &[&str] = &[".d.ts.map", ".js.map", ".d.ts", ".js"];
/// The TypeScript sources an emitted `<stem>.js` stands beside.
const TS_SOURCE_SUFFIXES: &[&str] = &[".ts", ".tsx", ".mts", ".cts"];

/// The parts of a repository path, split at `/` (and `\`, which a path from
/// Windows may hold).
fn parts(path: &Path) -> Vec<String> {
    path.to_string_lossy()
        .split(['/', '\\'])
        .map(str::to_owned)
        .collect()
}

/// The folders of a path (by index into its `parts`) named like an output
/// directory, outermost first, each with its manifests. The file name itself
/// is never one.
fn output_dirs(parts: &[String]) -> Vec<(usize, &'static [&'static str])> {
    let dirs = parts.len().saturating_sub(1);
    parts[..dirs]
        .iter()
        .enumerate()
        .filter_map(|(i, part)| {
            OUTPUT_DIRS
                .iter()
                .find(|(name, _)| *name == part.as_str())
                .map(|(_, manifests)| (i, *manifests))
        })
        .collect()
}

/// For `<dir>/<stem>.js` (or another emit suffix), the TypeScript sources it
/// would have been emitted from; empty for any other name.
fn ts_sources(path: &Path) -> Vec<PathBuf> {
    let text = path.to_string_lossy();
    let (dir, name) = text.rsplit_once(['/', '\\']).unwrap_or(("", text.as_ref()));
    let Some(stem) = TS_EMIT_SUFFIXES
        .iter()
        .find_map(|s| name.strip_suffix(s).filter(|stem| !stem.is_empty()))
    else {
        return Vec::new();
    };
    let names: Vec<String> = TS_SOURCE_SUFFIXES
        .iter()
        .map(|s| format!("{stem}{s}"))
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    joined(dir, &names)
}

/// Whether `name` is a tool's cache file ([`CACHE_FILES`], or
/// `.coverage.<id>`).
fn is_cache_file(name: &str) -> bool {
    CACHE_FILES.contains(&name)
        || name
            .strip_prefix(".coverage.")
            .is_some_and(|rest| !rest.is_empty())
}

/// The manifests a generated file at `path` would belong to, or empty when
/// `path` is no generated file's: for a lockfile, its manifests in the same
/// folder; for a Python package build's output, the Python project manifests
/// of the folder it was built in; for a file under an output directory
/// ([`OUTPUT_DIRS`]), the manifests of that directory's project; for a file
/// `tsc` emits, its TypeScript source beside it. Joined with `/`, the
/// separator of a repository path on every platform (git's index uses it). A
/// manifest whose file name is `*.ext` stands for any file of that extension
/// in its folder ([`manifest_matches`]).
pub fn manifests_beside(path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(manifests) = lockfile_manifests(path) {
        let text = path.to_string_lossy();
        let dir = text.rsplit_once(['/', '\\']).map_or("", |(d, _)| d);
        out.extend(joined(dir, manifests));
    }
    out.extend(
        package_build_dirs(path)
            .iter()
            .flat_map(|d| joined(d, PYTHON_BUILD_MANIFESTS)),
    );
    let parts = parts(path);
    for (i, manifests) in output_dirs(&parts) {
        out.extend(joined(&parts[..i].join("/"), manifests));
    }
    if let Some((dir, manifests)) = dependency_link(&parts) {
        out.extend(joined(&dir, manifests));
    }
    out.extend(ts_sources(path));
    out
}

/// The manifests of a tree on disk, for [`generated_kind`]'s
/// `manifest_present` when there is no index to ask: a literal manifest is a
/// regular file at its path (a link is not followed), and a `*.ext` one is any
/// file of that extension in its folder. Each folder is listed once.
pub struct TreeManifests<'a> {
    root: &'a Path,
    folders: std::cell::RefCell<std::collections::HashMap<PathBuf, Vec<String>>>,
}

impl<'a> TreeManifests<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self {
            root,
            folders: Default::default(),
        }
    }

    /// Whether the manifest `pattern` (repository-relative) is in the tree.
    pub fn present(&self, pattern: &Path) -> bool {
        let name = pattern
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let Some(ext) = name.strip_prefix("*.") else {
            return std::fs::symlink_metadata(self.root.join(pattern)).is_ok_and(|m| m.is_file());
        };
        let dir = pattern.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut folders = self.folders.borrow_mut();
        let names = folders.entry(dir.clone()).or_insert_with(|| {
            std::fs::read_dir(self.root.join(&dir))
                .map(|it| {
                    it.filter_map(Result::ok)
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default()
        });
        names.iter().any(|n| {
            n.strip_suffix(ext)
                .and_then(|s| s.strip_suffix('.'))
                .is_some_and(|s| !s.is_empty())
        })
    }
}

/// Whether `candidate`, a repository path, is the manifest `pattern` names:
/// the same path, or, for a pattern whose file name is `*.ext`, a file of
/// that extension in the pattern's folder.
pub fn manifest_matches(pattern: &Path, candidate: &Path) -> bool {
    if pattern == candidate {
        return true;
    }
    let Some(ext) = pattern
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("*."))
    else {
        return false;
    };
    pattern.parent() == candidate.parent()
        && candidate
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(ext))
            .and_then(|stem| stem.strip_suffix('.'))
            .is_some_and(|stem| !stem.is_empty())
}

/// Whether `head`, the first bytes of a file, starts an executable or object
/// file a compiler or linker writes: ELF (Linux), Mach-O and universal
/// binaries (macOS) or PE (Windows). A Java class file shares the universal
/// binary's magic number and is compiled output too.
pub fn is_compiled_binary(head: &[u8]) -> bool {
    const MAGICS: &[&[u8]] = &[
        b"\x7fELF",
        &[0xfe, 0xed, 0xfa, 0xce],
        &[0xfe, 0xed, 0xfa, 0xcf],
        &[0xce, 0xfa, 0xed, 0xfe],
        &[0xcf, 0xfa, 0xed, 0xfe],
        &[0xca, 0xfe, 0xba, 0xbe],
        b"MZ",
    ];
    MAGICS.iter().any(|m| head.starts_with(m))
}

/// The first four bytes of the regular file `rel` under `root`, or `None`
/// when it is not one or cannot be read. No component may be a link, and the
/// leaf is opened without following one and without blocking on a pipe
/// (decision 0083: the tree is the worker's).
pub fn file_head(root: &Path, rel: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut walked = root.to_path_buf();
    for part in rel.components() {
        match part {
            std::path::Component::Normal(name) => walked.push(name),
            _ => return None,
        }
        if std::fs::symlink_metadata(&walked)
            .ok()?
            .file_type()
            .is_symlink()
        {
            return None;
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = opts.open(&walked).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut head = Vec::with_capacity(4);
    file.take(4).read_to_end(&mut head).ok()?;
    Some(head)
}

/// What a build or test run generated, if `path` is such a file
/// (`manifest_present` answers for a path from [`manifests_beside`], `head`
/// gives the file's first bytes or `None`, as [`file_head`] reads them):
///
/// - Python bytecode in `__pycache__`;
/// - a tool's cache, by name at any depth ([`CACHE_DIRS`], `.coverage`,
///   `.eslintcache`);
/// - compiled output by extension (`.class`, `.o`, `.tsbuildinfo`);
/// - a lockfile with one of its manifests beside it;
/// - a Python package build's output with its project's manifest present;
/// - a file under an output directory ([`OUTPUT_DIRS`]) with that directory's
///   project manifest beside it, or a file `tsc` emitted beside its
///   TypeScript source;
/// - a program or object file a compiler wrote ([`is_compiled_binary`]):
///   `go build`, `make` and `cc` leave their program in the project's folder.
///
/// The one rule for a worker-made file, applied in two places so they cannot
/// disagree: merge-back leaves such a file out of an attempt's commit
/// ([`super::MergeBack::commit_retaining`], which also requires the file to be
/// new against the base and outside the declared writes), and the
/// orchestrator's scope check, which runs first, passes it on to that commit
/// instead of failing the attempt for it.
///
/// A package build's output is here because a contract whose work is a build
/// script runs it, as its instruction and acceptance say, and the build
/// leaves its wheel in `dist/`: that attempt was failed for writing outside
/// its scope although no contract writes `dist/` and the acceptance check
/// builds it again. The other build tools are here for the same reason: a
/// worker that runs the project's own install, build or tests leaves their
/// output where the tool puts it, while cargo's goes to the scratch
/// directory.
pub fn generated_kind(
    path: &Path,
    manifest_present: impl Fn(&Path) -> bool,
    head: impl Fn(&Path) -> Option<Vec<u8>>,
) -> Option<GeneratedKind> {
    if is_python_bytecode(path) {
        return Some(GeneratedKind::PythonBytecode);
    }
    let parts = parts(path);
    let (name, dirs) = parts.split_last()?;
    if dirs.iter().any(|d| CACHE_DIRS.contains(&d.as_str())) || is_cache_file(name) {
        return Some(GeneratedKind::ToolCache);
    }
    if name
        .rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && COMPILED_EXTENSIONS.contains(&ext))
    {
        return Some(GeneratedKind::BuildOutput);
    }
    let any = |ms: Vec<PathBuf>| ms.iter().any(|m| manifest_present(m));
    if let Some(manifests) = lockfile_manifests(path)
        && any(joined(&dirs.join("/"), manifests))
    {
        return Some(GeneratedKind::Lockfile);
    }
    let built: Vec<PathBuf> = package_build_dirs(path)
        .iter()
        .flat_map(|d| joined(d, PYTHON_BUILD_MANIFESTS))
        .collect();
    if any(built) {
        return Some(GeneratedKind::PackageBuild);
    }
    if let Some((dir, manifests)) = dependency_link(&parts)
        && (manifests.is_empty() || any(joined(&dir, manifests)))
    {
        return Some(GeneratedKind::BuildOutput);
    }
    let in_output = output_dirs(&parts)
        .into_iter()
        .any(|(i, manifests)| any(joined(&parts[..i].join("/"), manifests)));
    if in_output || any(ts_sources(path)) {
        return Some(GeneratedKind::BuildOutput);
    }
    head(path)
        .is_some_and(|h| is_compiled_binary(&h))
        .then_some(GeneratedKind::BuildOutput)
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
    /// Another build or install's output: a file under an output directory
    /// beside its project's manifest ([`OUTPUT_DIRS`]), compiled output by
    /// extension, a file `tsc` emitted beside its source, or a compiled
    /// program.
    BuildOutput,
    /// A test runner's, linter's or build tool's cache ([`CACHE_DIRS`],
    /// `.coverage`, `.eslintcache`).
    ToolCache,
}

impl GeneratedKind {
    /// Whether a file of this kind may sit under a folder whose name is
    /// protected (an npm package's own `.gitattributes` under
    /// `node_modules/`): the tree is a tool's, and merge-back never commits
    /// a new file of it.
    pub fn is_tool_tree(self) -> bool {
        matches!(self, Self::BuildOutput | Self::ToolCache)
    }
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

    /// No file content: only the path decides.
    fn no_head(_: &Path) -> Option<Vec<u8>> {
        None
    }

    /// A manifest set of small fixture projects, one per language, and the
    /// `manifest_present` answer over it ([`manifest_matches`] for `*.ext`).
    fn projects() -> impl Fn(&Path) -> bool {
        let present = [
            "web/package.json",
            "web/src/app.ts",
            "py/pyproject.toml",
            "reqs/requirements.txt",
            "jvm/pom.xml",
            "gradle/build.gradle.kts",
            "native/CMakeLists.txt",
            "dotnet/App.csproj",
            "ruby/Gemfile",
            "php/composer.json",
        ];
        move |m: &Path| present.iter().any(|p| manifest_matches(m, Path::new(p)))
    }

    /// The links kGraf makes to the checkout's installed dependencies are
    /// generated beside their project, as git lists a link: one path, the
    /// whole name. A virtual environment needs no manifest; the others do.
    /// Without this a link was a write outside the declared scope, and the
    /// attempt that ran the project's tests failed for it.
    #[test]
    fn a_dependency_link_is_generated_beside_its_project() {
        for path in [
            "web/node_modules",
            "web/.pnp.cjs",
            "web/.yarn",
            "web/.yarn/cache",
            ".venv",
            "py/venv",
            "php/vendor",
            "ruby/vendor/bundle",
        ] {
            assert_eq!(
                generated_kind(Path::new(path), projects(), no_head),
                Some(GeneratedKind::BuildOutput),
                "{path}"
            );
            assert!(is_dependency_link(Path::new(path)), "{path}");
        }
        for path in ["node_modules", "other/vendor", "web/vendor", "web/yarn"] {
            assert_eq!(
                generated_kind(Path::new(path), projects(), no_head),
                None,
                "{path}"
            );
        }
        assert!(!is_dependency_link(Path::new(
            "web/node_modules/x/index.js"
        )));
        assert!(
            manifests_beside(Path::new("web/node_modules"))
                .contains(&PathBuf::from("web/package.json"))
        );
    }

    /// Each language's own build, install and test output is generated beside
    /// its project; Rust's `target/` was never in the tree (the worker's cargo
    /// builds into its scratch directory), so before this list only Rust and
    /// Python projects could run their tools without a `.gitignore` entry and
    /// keep the attempt.
    #[test]
    fn every_language_s_build_and_test_output_is_generated_beside_its_project() {
        let has = projects();
        for (out, kind) in [
            (
                "web/node_modules/left-pad/index.js",
                GeneratedKind::BuildOutput,
            ),
            (
                "web/node_modules/pkg/.gitattributes",
                GeneratedKind::BuildOutput,
            ),
            (
                "web/node_modules/.package-lock.json",
                GeneratedKind::BuildOutput,
            ),
            ("web/dist/bundle.js", GeneratedKind::BuildOutput),
            ("web/coverage/lcov.info", GeneratedKind::BuildOutput),
            ("web/src/app.js", GeneratedKind::BuildOutput),
            ("web/src/app.d.ts", GeneratedKind::BuildOutput),
            ("web/src/app.js.map", GeneratedKind::BuildOutput),
            ("web/tsconfig.tsbuildinfo", GeneratedKind::BuildOutput),
            ("py/.venv/bin/python3", GeneratedKind::BuildOutput),
            ("reqs/venv/lib/site.py", GeneratedKind::BuildOutput),
            ("py/htmlcov/index.html", GeneratedKind::BuildOutput),
            ("py/.pytest_cache/v/cache/nodeids", GeneratedKind::ToolCache),
            ("py/.mypy_cache/3.12/x.json", GeneratedKind::ToolCache),
            ("py/.tox/py312/log.txt", GeneratedKind::ToolCache),
            ("py/.coverage", GeneratedKind::ToolCache),
            ("py/.coverage.host.123", GeneratedKind::ToolCache),
            (
                "py/pkg/__pycache__/m.cpython-312.opt-1.pyc",
                GeneratedKind::PythonBytecode,
            ),
            ("jvm/target/classes/App.class", GeneratedKind::BuildOutput),
            (
                "jvm/target/surefire-reports/T.txt",
                GeneratedKind::BuildOutput,
            ),
            ("gradle/build/libs/app.jar", GeneratedKind::BuildOutput),
            (
                "gradle/.gradle/8.5/checksums/x.bin",
                GeneratedKind::ToolCache,
            ),
            ("src/main/java/App.class", GeneratedKind::BuildOutput),
            ("native/build/CMakeCache.txt", GeneratedKind::BuildOutput),
            (
                "native/CMakeFiles/app.dir/main.c.o",
                GeneratedKind::BuildOutput,
            ),
            ("native/src/util.o", GeneratedKind::BuildOutput),
            (
                "dotnet/bin/Debug/net8.0/App.dll",
                GeneratedKind::BuildOutput,
            ),
            ("dotnet/obj/project.assets.json", GeneratedKind::BuildOutput),
            ("ruby/.bundle/config", GeneratedKind::BuildOutput),
            ("ruby/coverage/.resultset.json", GeneratedKind::BuildOutput),
            ("php/vendor/autoload.php", GeneratedKind::BuildOutput),
            ("php/.phpunit.result.cache", GeneratedKind::ToolCache),
            ("web/.eslintcache", GeneratedKind::ToolCache),
        ] {
            assert_eq!(
                generated_kind(Path::new(out), &has, no_head),
                Some(kind),
                "{out}"
            );
        }
    }

    /// The same names with no project to make them, and source files beside
    /// a project, stay the worker's writes: the scope check judges them.
    #[test]
    fn an_output_name_without_its_project_is_a_write() {
        let has = projects();
        for path in [
            // No project manifest where the folder is.
            "docs/node_modules/x.js",
            "dist/app.js",
            "lib/build/x.c",
            "jvm/build/x.txt",
            "py/target/x",
            "native/bin/tool",
            "ruby/vendor/x.rb",
            "py/coverage/x",
            // The project's sources.
            "web/src/app.ts",
            "web/src/other.js",
            "web/index.js",
            "py/pkg/mod.py",
            "dotnet/Program.cs",
            "jvm/src/main/java/App.java",
            "native/src/util.c",
            "src/main.go",
            "run.sh",
            // A folder named like a cache, as the file name only.
            "notes/.tox",
            ".coverage.",
            "x.o.txt",
        ] {
            assert_eq!(
                generated_kind(Path::new(path), &has, no_head),
                None,
                "{path}"
            );
        }
        // `*.csproj` matches a project file in the folder, not a deeper one or
        // a bare extension.
        assert!(manifest_matches(
            Path::new("d/*.csproj"),
            Path::new("d/App.csproj")
        ));
        assert!(!manifest_matches(
            Path::new("d/*.csproj"),
            Path::new("d/x/App.csproj")
        ));
        assert!(!manifest_matches(
            Path::new("d/*.csproj"),
            Path::new("d/.csproj")
        ));
        assert!(!manifest_matches(
            Path::new("d/*.csproj"),
            Path::new("d/Appcsproj")
        ));
    }

    /// `go build` writes the module's program into its folder and `cc` writes
    /// `a.out`: no name tells them, their first bytes do.
    #[test]
    fn a_compiled_program_is_told_by_its_first_bytes() {
        let none = |_: &Path| false;
        for magic in [
            b"\x7fELF".to_vec(),
            vec![0xcf, 0xfa, 0xed, 0xfe],
            vec![0xca, 0xfe, 0xba, 0xbe],
            b"MZ\x90\x00".to_vec(),
        ] {
            let head = |_: &Path| Some(magic.clone());
            assert_eq!(
                generated_kind(Path::new("cmd/vectorops"), none, head),
                Some(GeneratedKind::BuildOutput),
                "{magic:?}"
            );
        }
        for text in [b"#!/b".to_vec(), b"pack".to_vec(), Vec::new()] {
            let head = |_: &Path| Some(text.clone());
            assert_eq!(generated_kind(Path::new("serve_local"), none, head), None);
        }
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("prog"), b"\x7fELF\x02\x01").expect("write");
        std::fs::write(dir.path().join("script"), b"#!/bin/sh\n").expect("write");
        assert_eq!(
            file_head(dir.path(), Path::new("prog")).as_deref(),
            Some(&b"\x7fELF"[..])
        );
        assert_eq!(
            file_head(dir.path(), Path::new("script")).as_deref(),
            Some(&b"#!/b"[..])
        );
        assert_eq!(file_head(dir.path(), Path::new("absent")), None);
        assert_eq!(file_head(dir.path(), Path::new("../prog")), None);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("prog"), dir.path().join("link"))
                .expect("link");
            assert_eq!(file_head(dir.path(), Path::new("link")), None);
        }
    }

    /// Only a tool's own tree (build output, a cache) may hold a file under a
    /// protected folder name; a lockfile, bytecode or a package build may not.
    #[test]
    fn only_build_output_and_tool_caches_are_tool_trees() {
        assert!(GeneratedKind::BuildOutput.is_tool_tree());
        assert!(GeneratedKind::ToolCache.is_tool_tree());
        assert!(!GeneratedKind::Lockfile.is_tool_tree());
        assert!(!GeneratedKind::PythonBytecode.is_tool_tree());
        assert!(!GeneratedKind::PackageBuild.is_tool_tree());
    }

    /// A manifest on disk is a regular file at its path, or a file of the
    /// named extension in its folder. A folder, a link, a bare extension and
    /// another folder's file are not one.
    #[test]
    fn tree_manifests_see_files_not_folders_links_or_bare_extensions() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("app/Cargo.toml")).expect("dir named like a file");
        std::fs::create_dir_all(root.join("dotnet")).expect("dir");
        std::fs::create_dir_all(root.join("bare")).expect("dir");
        std::fs::create_dir_all(root.join("other")).expect("dir");
        std::fs::write(root.join("package.json"), b"{}").expect("write");
        std::fs::write(root.join("dotnet/App.csproj"), b"").expect("write");
        std::fs::write(root.join("bare/.csproj"), b"").expect("write");
        std::fs::write(root.join("other/x.fsproj"), b"").expect("write");
        let m = TreeManifests::new(root);
        assert!(m.present(Path::new("package.json")));
        assert!(!m.present(Path::new("absent.json")));
        assert!(!m.present(Path::new("app/Cargo.toml")), "a folder");
        assert!(m.present(Path::new("dotnet/*.csproj")));
        assert!(m.present(Path::new("dotnet/*.csproj")), "listed once, kept");
        assert!(!m.present(Path::new("bare/*.csproj")), "an empty stem");
        assert!(!m.present(Path::new("other/*.csproj")), "another extension");
        assert!(!m.present(Path::new("gone/*.csproj")), "an absent folder");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("package.json"), root.join("linked.json"))
                .expect("link");
            assert!(
                !m.present(Path::new("linked.json")),
                "a link is not followed"
            );
        }
    }

    /// A pipe named like a binary is read without blocking and without
    /// yielding bytes: the open must not wait for a writer.
    #[cfg(unix)]
    #[test]
    fn file_head_does_not_block_on_a_pipe() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("pipe");
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("cstring");
        // SAFETY: mkfifo on a path in a temp dir this test owns.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let root = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(file_head(&root, Path::new("pipe")));
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("file_head blocked on a pipe");
        assert_eq!(got, None);
    }

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
                generated_kind(Path::new(lock), has, no_head),
                Some(GeneratedKind::Lockfile),
                "{lock}"
            );
        }
        // The NuGet, CocoaPods, SwiftPM, Dart, Mix and Composer lockfiles,
        // each beside its own manifest (a NuGet project file by extension).
        let more = [
            "App/App.csproj",
            "ios/Podfile",
            "swift/Package.swift",
            "dart/pubspec.yaml",
            "ex/mix.exs",
            "php/composer.json",
        ];
        let beside = |m: &Path| more.iter().any(|p| manifest_matches(m, Path::new(p)));
        for lock in [
            "App/packages.lock.json",
            "ios/Podfile.lock",
            "swift/Package.resolved",
            "dart/pubspec.lock",
            "ex/mix.lock",
            "php/composer.lock",
        ] {
            assert_eq!(
                generated_kind(Path::new(lock), beside, no_head),
                Some(GeneratedKind::Lockfile),
                "{lock}"
            );
        }
        for stray in [
            "packages.lock.json",
            "web/packages.lock.json",
            "Podfile.lock",
        ] {
            assert_eq!(
                generated_kind(Path::new(stray), beside, no_head),
                None,
                "{stray}"
            );
        }
        for stray in [
            "docs/Cargo.lock",
            "web/uv.lock",
            "py/yarn.lock",
            "src/lib.rs",
        ] {
            assert_eq!(
                generated_kind(Path::new(stray), has, no_head),
                None,
                "{stray}"
            );
        }
        assert_eq!(
            generated_kind(Path::new("a/__pycache__/m.pyc"), has, no_head),
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
                generated_kind(Path::new(out), has, no_head),
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
            assert_eq!(
                generated_kind(Path::new(not_out), has, no_head),
                None,
                "{not_out}"
            );
        }
        assert_eq!(
            manifests_beside(Path::new("py/dist/p-1.whl")),
            vec![
                PathBuf::from("py/pyproject.toml"),
                PathBuf::from("py/setup.py"),
                PathBuf::from("py/setup.cfg"),
                // `dist/` is also a JavaScript build's output folder.
                PathBuf::from("py/package.json"),
            ]
        );
    }
}
