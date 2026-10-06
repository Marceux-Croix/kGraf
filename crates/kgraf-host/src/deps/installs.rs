//! Installs a project needs and does not have, for the package managers kGraf
//! does not fetch for (decision 0087, amended 2026-10-06).
//!
//! Workers, gates and the verifier run with the network off and read the
//! checkout's installed dependencies through links (`kgraf_sandbox::
//! link_dependency_trees`). When a project declares dependencies that are not
//! installed, every test fails inside kGraf with the package manager's own
//! error ("Cannot find module", "No module named pytest"), which reads like a
//! fault in the work. This module names the missing install and the command
//! that makes it, so the failure, the gate note and the run-start notice say
//! that instead.
//!
//! **Why no fetch for these.** Cargo's fetch downloads and unpacks and runs no
//! package code. The install commands of the other ecosystems run package
//! code on the host, outside the sandbox: npm, pnpm and yarn run install
//! scripts, pip builds a source distribution by running its `setup.py`,
//! Bundler and Composer run plugins. The user's own install, which the
//! sandbox now reads, is what makes the tree runnable; kGraf names the
//! command and the user runs it.
//!
//! Every check here reads files only. None runs a package manager, an
//! interpreter or a project file.

use std::path::{Path, PathBuf};

/// A package manager whose installs this module checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ecosystem {
    Npm,
    Python,
    Go,
    Maven,
    Gradle,
    Dotnet,
}

impl Ecosystem {
    pub fn name(self) -> &'static str {
        match self {
            Ecosystem::Npm => "npm",
            Ecosystem::Python => "Python",
            Ecosystem::Go => "Go",
            Ecosystem::Maven => "Maven",
            Ecosystem::Gradle => "Gradle",
            Ecosystem::Dotnet => ".NET",
        }
    }
}

/// One install a project needs and does not have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingInstall {
    pub ecosystem: Ecosystem,
    /// The project folder, relative to the checkout; empty for the root.
    pub dir: PathBuf,
    /// What is missing, in words ("node_modules has no jest").
    pub what: String,
    /// The command that installs it, run in [`Self::dir`].
    pub command: String,
}

impl MissingInstall {
    /// One sentence for a failure, a gate note or a notice.
    pub fn sentence(&self) -> String {
        let place = if self.dir.as_os_str().is_empty() {
            "the project root".to_owned()
        } else {
            self.dir.display().to_string()
        };
        format!(
            "{} dependencies are not installed: {}. Run `{}` in {place}; workers run with the \
             network off and kGraf does not install {} packages",
            self.ecosystem.name(),
            self.what,
            self.command,
            self.ecosystem.name()
        )
    }
}

/// Where the package stores are, read from the environment once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallEnv {
    pub home: Option<PathBuf>,
    /// Go's module cache: `GOMODCACHE`, else the first `GOPATH` entry's
    /// `pkg/mod`, else `~/go/pkg/mod`.
    pub go_mod_cache: Option<PathBuf>,
}

impl InstallEnv {
    pub fn current() -> Self {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .filter(|h| !h.is_empty())
            .map(PathBuf::from);
        let go_mod_cache = std::env::var_os("GOMODCACHE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("GOPATH")
                    .filter(|v| !v.is_empty())
                    .and_then(|v| std::env::split_paths(&v).next())
                    .map(|p| p.join("pkg/mod"))
            })
            .or_else(|| home.as_ref().map(|h| h.join("go/pkg/mod")));
        Self { home, go_mod_cache }
    }
}

/// Every install the checkout at `checkout` needs and lacks, searched in the
/// project folders the sandbox links dependencies for
/// (`kgraf_sandbox::walk_project_dirs`). `python_ready` says whether Python
/// tests can find pytest without a virtual environment (the host
/// interpreter imports it); it is asked only for a Python project with tests
/// and no virtual environment.
pub fn missing_installs(
    checkout: &Path,
    env: &InstallEnv,
    python_ready: &dyn Fn() -> bool,
) -> Vec<MissingInstall> {
    let mut out = Vec::new();
    let Ok(root) = std::fs::canonicalize(checkout) else {
        return out;
    };
    kgraf_sandbox::walk_project_dirs(&root, |dir, rel, names| {
        if names.contains("package.json") {
            out.extend(npm(&root, dir, rel));
        }
        out.extend(dotnet(dir, rel, names));
    });
    out.extend(python(&root, python_ready));
    out.extend(go(&root, env));
    out.extend(maven(&root, env));
    out.extend(gradle(&root, env));
    out
}

/// The names a `package.json` declares in `dependencies` and
/// `devDependencies`.
fn declared_node_packages(dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for key in ["dependencies", "devDependencies"] {
        if let Some(map) = json[key].as_object() {
            names.extend(map.keys().cloned());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The install command a lockfile at `dir` or the nearest folder above it,
/// up to `root`, asks for.
fn node_install_command(root: &Path, dir: &Path) -> String {
    for d in dir.ancestors() {
        if d.join("pnpm-lock.yaml").is_file() {
            return "pnpm install --frozen-lockfile".into();
        }
        if d.join("yarn.lock").is_file() {
            return "yarn install".into();
        }
        if d.join("bun.lock").is_file() || d.join("bun.lockb").is_file() {
            return "bun install".into();
        }
        if d.join("package-lock.json").is_file() || d.join("npm-shrinkwrap.json").is_file() {
            return "npm ci".into();
        }
        if d == root {
            break;
        }
    }
    "npm install".into()
}

/// A package with declared dependencies is installed when each name resolves
/// in a `node_modules` at its folder or one above it (up to the checkout
/// root), as Node resolves it, or when a Plug'n'Play file is there.
fn npm(root: &Path, dir: &Path, rel: &Path) -> Option<MissingInstall> {
    let declared = declared_node_packages(dir);
    if declared.is_empty() {
        return None;
    }
    let mut chain = Vec::new();
    for d in dir.ancestors() {
        if d.join(".pnp.cjs").is_file() {
            return None;
        }
        chain.push(d.join("node_modules"));
        if d == root {
            break;
        }
    }
    let missing: Vec<&String> = declared
        .iter()
        .filter(|name| !chain.iter().any(|nm| nm.join(name.as_str()).exists()))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let shown: Vec<&str> = missing.iter().take(5).map(|s| s.as_str()).collect();
    let more = missing.len().saturating_sub(shown.len());
    let what = if chain.iter().any(|nm| nm.is_dir()) {
        format!(
            "node_modules has no {}{}",
            shown.join(", "),
            if more > 0 {
                format!(" (and {more} more)")
            } else {
                String::new()
            }
        )
    } else {
        format!(
            "package.json declares {} package(s) and there is no node_modules",
            declared.len()
        )
    };
    Some(MissingInstall {
        ecosystem: Ecosystem::Npm,
        dir: rel.to_path_buf(),
        what,
        command: node_install_command(root, dir),
    })
}

/// The root virtual environment, when there is one.
pub(crate) fn root_venv(root: &Path) -> Option<PathBuf> {
    [".venv", "venv"]
        .iter()
        .map(|n| root.join(n))
        .find(|v| v.join("pyvenv.cfg").is_file())
}

/// Whether the virtual environment at `venv` has `package` installed: its
/// folder in a `site-packages` (`lib/python*/site-packages`, or
/// `Lib/site-packages` on Windows).
pub(crate) fn venv_has(venv: &Path, package: &str) -> bool {
    let windows = venv.join("Lib/site-packages").join(package);
    if windows.is_dir() {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(venv.join("lib")) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_name().to_string_lossy().starts_with("python")
            && e.path().join("site-packages").join(package).is_dir()
    })
}

/// The Python project's test install: with a root virtual environment, its
/// pytest; without one, pytest importable by the host interpreter.
/// Whether the project's Python build files name pytest (a dependency, a
/// test extra, a configuration section): only then is pytest an install the
/// project needs. A project whose tests run with `unittest` needs none.
pub(crate) fn pytest_declared(root: &Path) -> bool {
    root.join("pytest.ini").is_file()
        || [
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "tox.ini",
            "requirements.txt",
            "requirements-dev.txt",
            "requirements-test.txt",
        ]
        .iter()
        .any(|f| {
            std::fs::read_to_string(root.join(f))
                .is_ok_and(|t| t.to_ascii_lowercase().contains("pytest"))
        })
}

/// Whether pytest is where the sandbox will look for it: in the root
/// virtual environment when there is one (the sandbox activates it), else in
/// the host interpreter.
pub(crate) fn pytest_available(root: &Path, python_ready: &dyn Fn() -> bool) -> bool {
    match root_venv(root) {
        Some(venv) => venv_has(&venv, "pytest") || venv_has(&venv, "_pytest"),
        None => python_ready(),
    }
}

fn python(root: &Path, python_ready: &dyn Fn() -> bool) -> Option<MissingInstall> {
    if !crate::gates::has_python_tests(root) || !pytest_declared(root) {
        return None;
    }
    let requirements = [
        "requirements-dev.txt",
        "requirements-test.txt",
        "requirements.txt",
    ]
    .into_iter()
    .find(|f| root.join(f).is_file());
    let what_to_install = match requirements {
        Some(f) => format!("-r {f}"),
        None => "pytest".to_owned(),
    };
    match root_venv(root) {
        Some(venv) => {
            if venv_has(&venv, "pytest") || venv_has(&venv, "_pytest") {
                return None;
            }
            let name = venv
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let python = if cfg!(windows) {
                format!("{name}\\Scripts\\python")
            } else {
                format!("{name}/bin/python")
            };
            Some(MissingInstall {
                ecosystem: Ecosystem::Python,
                dir: PathBuf::new(),
                what: format!("the virtual environment {name} has no pytest"),
                command: format!("{python} -m pip install {what_to_install}"),
            })
        }
        None if python_ready() => None,
        None => {
            let (python, venv_python) = if cfg!(windows) {
                ("python", ".venv\\Scripts\\python")
            } else {
                ("python3", ".venv/bin/python")
            };
            Some(MissingInstall {
                ecosystem: Ecosystem::Python,
                dir: PathBuf::new(),
                what: format!(
                    "the project has Python tests, no virtual environment, and `{python}` \
                     cannot import pytest"
                ),
                command: format!(
                    "{python} -m venv .venv && {venv_python} -m pip install {what_to_install}"
                ),
            })
        }
    }
}

/// Go's module path escaping for the module cache: each upper-case letter
/// becomes `!` and its lower case.
fn go_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The `require` entries of a `go.mod`, as (module, version), in both the
/// one-line and the block form.
pub(crate) fn go_requires(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        if in_block {
            if line == ")" {
                in_block = false;
                continue;
            }
            let mut words = line.split_whitespace();
            if let (Some(m), Some(v)) = (words.next(), words.next()) {
                out.push((m.trim_matches('"').to_owned(), v.to_owned()));
            }
        } else if let Some(rest) = line.strip_prefix("require") {
            let rest = rest.trim();
            if rest == "(" {
                in_block = true;
            } else {
                let mut words = rest.split_whitespace();
                if let (Some(m), Some(v)) = (words.next(), words.next()) {
                    out.push((m.trim_matches('"').to_owned(), v.to_owned()));
                }
            }
        }
    }
    out
}

/// A Go module at the root is installed when it vendors its dependencies, or
/// when each module its `go.mod` requires is in the module cache.
fn go(root: &Path, env: &InstallEnv) -> Option<MissingInstall> {
    let text = std::fs::read_to_string(root.join("go.mod")).ok()?;
    if root.join("vendor/modules.txt").is_file() {
        return None;
    }
    let requires = go_requires(&text);
    if requires.is_empty() {
        return None;
    }
    let cache = env.go_mod_cache.as_ref();
    let missing: Vec<&str> = requires
        .iter()
        .filter(|(m, v)| {
            cache.is_none_or(|c| {
                !c.join("cache/download")
                    .join(go_escape(m))
                    .join("@v")
                    .join(format!("{v}.mod"))
                    .is_file()
            })
        })
        .map(|(m, _)| m.as_str())
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(MissingInstall {
        ecosystem: Ecosystem::Go,
        dir: PathBuf::new(),
        what: format!(
            "the module cache has no {}{}",
            missing
                .iter()
                .take(5)
                .copied()
                .collect::<Vec<_>>()
                .join(", "),
            if missing.len() > 5 {
                format!(" (and {} more)", missing.len() - 5)
            } else {
                String::new()
            }
        ),
        command: "go mod download".into(),
    })
}

/// A Maven project at the root is runnable offline when the local repository
/// holds the test plugin every `mvn test` runs.
fn maven(root: &Path, env: &InstallEnv) -> Option<MissingInstall> {
    if !root.join("pom.xml").is_file() {
        return None;
    }
    let repo = env.home.as_ref()?.join(".m2/repository");
    let surefire = repo.join("org/apache/maven/plugins/maven-surefire-plugin");
    if surefire.is_dir() {
        return None;
    }
    Some(MissingInstall {
        ecosystem: Ecosystem::Maven,
        dir: PathBuf::new(),
        what: if repo.is_dir() {
            "the local repository (~/.m2/repository) has no test plugin \
             (maven-surefire-plugin)"
                .to_owned()
        } else {
            "there is no local repository (~/.m2/repository)".to_owned()
        },
        command: "mvn test".into(),
    })
}

/// The Gradle build files that mark a Gradle project.
pub(crate) const GRADLE_FILES: &[&str] = &[
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
];

/// A Gradle project at the root is runnable offline when the user's
/// dependency cache exists.
fn gradle(root: &Path, env: &InstallEnv) -> Option<MissingInstall> {
    if !GRADLE_FILES.iter().any(|f| root.join(f).is_file()) {
        return None;
    }
    let cache = env.home.as_ref()?.join(".gradle/caches/modules-2");
    if cache.is_dir() {
        return None;
    }
    let command = if root.join("gradlew").is_file() {
        "./gradlew build"
    } else {
        "gradle build"
    };
    Some(MissingInstall {
        ecosystem: Ecosystem::Gradle,
        dir: PathBuf::new(),
        what: "there is no Gradle dependency cache (~/.gradle/caches/modules-2)".to_owned(),
        command: command.into(),
    })
}

/// Whether `name` is a .NET project file.
pub(crate) fn is_dotnet_project(name: &str) -> bool {
    [".csproj", ".fsproj", ".vbproj"]
        .iter()
        .any(|ext| name.len() > ext.len() && name.ends_with(ext))
}

/// A .NET project is restored when `obj/project.assets.json` is beside it.
fn dotnet(dir: &Path, rel: &Path, names: &kgraf_sandbox::Names) -> Option<MissingInstall> {
    let mut projects: Vec<&String> = names.iter().filter(|n| is_dotnet_project(n)).collect();
    projects.sort();
    let project = projects.first()?;
    if dir.join("obj/project.assets.json").is_file() {
        return None;
    }
    Some(MissingInstall {
        ecosystem: Ecosystem::Dotnet,
        dir: rel.to_path_buf(),
        what: format!("{project} is not restored (no obj/project.assets.json)"),
        command: "dotnet restore".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    fn env_at(home: &Path) -> InstallEnv {
        InstallEnv {
            home: Some(home.to_path_buf()),
            go_mod_cache: Some(home.join("go/pkg/mod")),
        }
    }

    #[test]
    fn an_npm_package_names_each_missing_package_and_its_lockfile_s_command() {
        let c = tempfile::tempdir().expect("dir");
        let home = tempfile::tempdir().expect("home");
        write(
            &c.path().join("package.json"),
            r#"{"dependencies":{"left-pad":"1"},"devDependencies":{"jest":"29"}}"#,
        );
        write(&c.path().join("package-lock.json"), "{}");
        let missing = missing_installs(c.path(), &env_at(home.path()), &|| true);
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert_eq!(missing[0].command, "npm ci");
        assert!(missing[0].what.contains("no node_modules"), "{missing:?}");
        assert!(
            missing[0]
                .sentence()
                .contains("Run `npm ci` in the project root"),
            "{}",
            missing[0].sentence()
        );
        write(&c.path().join("node_modules/left-pad/index.js"), "");
        let missing = missing_installs(c.path(), &env_at(home.path()), &|| true);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].what, "node_modules has no jest");
        write(&c.path().join("node_modules/jest/index.js"), "");
        assert!(missing_installs(c.path(), &env_at(home.path()), &|| true).is_empty());
    }

    #[test]
    fn a_workspace_package_resolves_through_the_hoisted_node_modules() {
        let c = tempfile::tempdir().expect("dir");
        let home = tempfile::tempdir().expect("home");
        write(
            &c.path().join("package.json"),
            r#"{"workspaces":["packages/*"]}"#,
        );
        write(&c.path().join("pnpm-lock.yaml"), "");
        write(
            &c.path().join("packages/a/package.json"),
            r#"{"dependencies":{"dep":"1","own":"1"}}"#,
        );
        write(&c.path().join("node_modules/dep/index.js"), "");
        let missing = missing_installs(c.path(), &env_at(home.path()), &|| true);
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert_eq!(missing[0].dir, Path::new("packages/a"));
        assert_eq!(missing[0].what, "node_modules has no own");
        assert_eq!(missing[0].command, "pnpm install --frozen-lockfile");
    }

    #[test]
    fn python_tests_need_pytest_in_the_venv_or_the_interpreter() {
        let c = tempfile::tempdir().expect("dir");
        let home = tempfile::tempdir().expect("home");
        write(&c.path().join("requirements.txt"), "pytest\n");
        write(&c.path().join("tests/test_a.py"), "def test_a(): pass\n");
        let env = env_at(home.path());
        let missing = missing_installs(c.path(), &env, &|| false);
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert!(missing[0].command.contains("-m venv .venv"), "{missing:?}");
        assert!(missing[0].command.ends_with("install -r requirements.txt"));
        assert!(missing_installs(c.path(), &env, &|| true).is_empty());
        // A venv without pytest is missing it whatever the interpreter has.
        write(&c.path().join(".venv/pyvenv.cfg"), "home = /usr/bin\n");
        std::fs::create_dir_all(c.path().join(".venv/lib/python3.12/site-packages"))
            .expect("mkdir");
        let missing = missing_installs(c.path(), &env, &|| true);
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert_eq!(
            missing[0].what,
            "the virtual environment .venv has no pytest"
        );
        std::fs::create_dir_all(c.path().join(".venv/lib/python3.12/site-packages/pytest"))
            .expect("mkdir");
        assert!(missing_installs(c.path(), &env, &|| false).is_empty());
    }

    /// A project whose tests run with `unittest` and whose build files never
    /// name pytest needs no pytest: nothing is missing, so no failure of it
    /// is told to install one.
    #[test]
    fn a_project_that_never_names_pytest_needs_none() {
        let c = tempfile::tempdir().expect("dir");
        let home = tempfile::tempdir().expect("home");
        write(&c.path().join("requirements.txt"), "requests\n");
        write(&c.path().join("tests/test_a.py"), "import unittest\n");
        assert!(missing_installs(c.path(), &env_at(home.path()), &|| false).is_empty());
    }

    #[test]
    fn go_requires_are_read_in_both_forms_and_checked_in_the_module_cache() {
        let text = "module x\n\ngo 1.22\n\nrequire github.com/A/b v1.2.3\n\nrequire (\n\tgolang.org/x/text v0.14.0 // indirect\n\t\"rsc.io/quote\" v1.5.2\n)\n";
        assert_eq!(
            go_requires(text),
            vec![
                ("github.com/A/b".to_owned(), "v1.2.3".to_owned()),
                ("golang.org/x/text".to_owned(), "v0.14.0".to_owned()),
                ("rsc.io/quote".to_owned(), "v1.5.2".to_owned()),
            ]
        );
        assert_eq!(go_escape("github.com/A/b"), "github.com/!a/b");
        let c = tempfile::tempdir().expect("dir");
        let home = tempfile::tempdir().expect("home");
        write(
            &c.path().join("go.mod"),
            "module x\nrequire rsc.io/quote v1.5.2\n",
        );
        let env = env_at(home.path());
        let missing = missing_installs(c.path(), &env, &|| true);
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert_eq!(missing[0].command, "go mod download");
        write(
            &home
                .path()
                .join("go/pkg/mod/cache/download/rsc.io/quote/@v/v1.5.2.mod"),
            "",
        );
        assert!(missing_installs(c.path(), &env, &|| true).is_empty());
    }

    #[test]
    fn jvm_and_dotnet_projects_need_their_stores_and_restores() {
        let c = tempfile::tempdir().expect("dir");
        let home = tempfile::tempdir().expect("home");
        let env = env_at(home.path());
        write(&c.path().join("pom.xml"), "<project/>");
        write(&c.path().join("build.gradle"), "");
        write(&c.path().join("gradlew"), "");
        write(&c.path().join("svc/Svc.csproj"), "<Project/>");
        let mut got: Vec<(Ecosystem, String)> = missing_installs(c.path(), &env, &|| true)
            .into_iter()
            .map(|m| (m.ecosystem, m.command))
            .collect();
        got.sort_by_key(|(e, _)| e.name());
        assert_eq!(
            got,
            vec![
                (Ecosystem::Dotnet, "dotnet restore".to_owned()),
                (Ecosystem::Gradle, "./gradlew build".to_owned()),
                (Ecosystem::Maven, "mvn test".to_owned()),
            ]
        );
        std::fs::create_dir_all(
            home.path()
                .join(".m2/repository/org/apache/maven/plugins/maven-surefire-plugin"),
        )
        .expect("mkdir");
        std::fs::create_dir_all(home.path().join(".gradle/caches/modules-2")).expect("mkdir");
        write(&c.path().join("svc/obj/project.assets.json"), "{}");
        assert!(missing_installs(c.path(), &env, &|| true).is_empty());
    }
}
