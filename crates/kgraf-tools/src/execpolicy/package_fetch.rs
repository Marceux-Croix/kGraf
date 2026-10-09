//! Package downloads in a worker's shell (decision 0087, amended 2026-10-09).
//!
//! **Why.** A command such as `pip install x`, `npm install` or `cargo fetch`
//! downloads packages from a registry. Decision 0087 makes the host, not a
//! worker, fetch or install a project's dependencies, with the project's
//! consent: crates.io packages by `cargo fetch`, the other package managers'
//! installs in kGraf's sandbox behind its egress proxy, both before the
//! workers start. A worker's shell has the network off. The build allowlist
//! never lists a download, but a grant could still run one: a program
//! granted to a contract after a shortfall, the Bypass mode, or Auto's allow
//! set made it a confined command. Where the sandbox denies the network
//! (macOS, Linux) the download then failed with the package manager's own
//! network error, which says nothing about why or what allows it; where it
//! cannot (Windows) the download went to a registry with no consent asked. A
//! contract that only installs and checks (a check-only contract) met this
//! first, since running commands is its whole work.
//!
//! **The rule.** Outside a disposable remote environment
//! ([`super::ExecPolicy::with_package_downloads`]), a command that downloads
//! packages is refused in every mode, whatever is granted, with a reason that
//! says why and what allows it ([`refusal`]). A worker that reports it short
//! is held with that reason, not granted the program
//! ([`is_package_fetch_refusal`]).
//!
//! **What counts.** The install and fetch forms of the common package
//! managers, matched on the first word after the program that is not an
//! option, in every command a call runs (a script's, and what a wrapper such
//! as `env` or `sh -c` starts): pip (and `python -m pip`), uv, pipx, poetry,
//! conda, npm, pnpm, yarn, bun, cargo, gem, bundler, go, composer and the
//! .NET CLI. A form that says it stays offline (`pip install --no-index`,
//! `npm ci --offline`, `cargo fetch --offline`) does not download and is
//! left to the policy. A global option with a value before the subcommand
//! (`npm --prefix web install`) hides the subcommand from this reading; the
//! sandbox's network denial still stands behind it.
//!
//! Written new: the codex-rs and goose command policies searched in the
//! 0055 survey have no notion of a package download; they ask per command.

/// What a refused download's reason holds, so a reader (the orchestrator's
/// shortfall decision) can tell it from a refusal a grant can lift.
pub const PACKAGE_FETCH_MARK: &str = "a worker does not download packages";

/// Whether `reason` is the refusal of a package download ([`refusal`]).
pub fn is_package_fetch_refusal(reason: &str) -> bool {
    reason.contains(PACKAGE_FETCH_MARK)
}

/// What allows the work a refused download was for (decision 0087).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Allow {
    /// crates.io dependencies: kGraf fetches them with the project's consent
    /// (`kgraf deps allow`).
    CratesFetch,
    /// A package manager whose installs kGraf runs at the run's start with
    /// the project's consent (`kgraf deps allow <name>`), for what the
    /// project's dependency files declare.
    Install(&'static str),
    /// Nothing kGraf runs: a program installed for the user (`cargo
    /// install`, `pipx install`), or a package manager kGraf does not
    /// install for.
    Beforehand,
}

/// A command that downloads packages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PackageFetch {
    /// The command as written (`pip install`, `python3 -m pip install`).
    pub(crate) form: String,
    /// The registry it downloads from, as the reason names it.
    pub(crate) registry: &'static str,
    /// What allows the work.
    pub(crate) allow: Allow,
}

/// The base name of a program, without a directory or a Windows `.exe`,
/// lowercased: `/usr/bin/pip3` and `PIP3.EXE` are `pip3`.
fn program_name(word: &str) -> String {
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let lower = base.to_ascii_lowercase();
    lower
        .strip_suffix(".exe")
        .map(str::to_owned)
        .unwrap_or(lower)
}

/// `name` is `stem` followed by nothing or by a version (`pip3`, `pip3.12`).
fn versioned(name: &str, stem: &str) -> bool {
    name.strip_prefix(stem)
        .is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

/// The words after the options: `["-q", "install", "x"]` gives
/// `["install", "x"]`.
fn after_options(args: &[String]) -> &[String] {
    let at = args
        .iter()
        .position(|a| !a.starts_with('-'))
        .unwrap_or(args.len());
    &args[at..]
}

fn has_any(args: &[String], flags: &[&str]) -> bool {
    args.iter().any(|a| flags.contains(&a.as_str()))
}

const PYPI: &str = "PyPI";
const NPM: &str = "the npm registry";
const PYTHON: Allow = Allow::Install("python");
const NODE: Allow = Allow::Install("npm");

fn fetch(form: impl Into<String>, registry: &'static str, allow: Allow) -> Option<PackageFetch> {
    Some(PackageFetch {
        form: form.into(),
        registry,
        allow,
    })
}

/// The download a simple command `words` (its program first) makes, if any.
pub(crate) fn package_fetch(words: &[String]) -> Option<PackageFetch> {
    let (program, args) = words.split_first()?;
    let name = program_name(program);
    let rest = after_options(args);
    let sub = rest.first().map(String::as_str).unwrap_or("");
    let sub2 = rest.get(1).map(String::as_str).unwrap_or("");
    let pip = |sub: &str, args: &[String]| {
        matches!(sub, "install" | "download" | "wheel") && !has_any(args, &["--no-index"])
    };
    let offline = has_any(args, &["--offline"]);
    match name.as_str() {
        n if versioned(n, "pip") && pip(sub, args) => fetch(format!("{n} {sub}"), PYPI, PYTHON),
        n if versioned(n, "python") || n == "py" => {
            // `python -m pip install`, options before `-m` allowed.
            let m = args.iter().position(|a| a == "-m")?;
            if args.get(m + 1).map(String::as_str) != Some("pip") {
                return None;
            }
            let after = &args[m + 2..];
            let sub = after_options(after).first().map(String::as_str)?;
            if !pip(sub, after) {
                return None;
            }
            fetch(format!("{n} -m pip {sub}"), PYPI, PYTHON)
        }
        "uv" if !offline => match (sub, sub2) {
            ("pip", "install" | "sync") => fetch(format!("uv pip {sub2}"), PYPI, PYTHON),
            ("tool", "install") => fetch("uv tool install", PYPI, Allow::Beforehand),
            ("add" | "sync" | "lock", _) => fetch(format!("uv {sub}"), PYPI, PYTHON),
            _ => None,
        },
        "pipx" if sub == "install" => fetch("pipx install", PYPI, Allow::Beforehand),
        "poetry" if matches!(sub, "install" | "add" | "update" | "lock") => {
            fetch(format!("poetry {sub}"), PYPI, PYTHON)
        }
        "conda" | "mamba" | "micromamba"
            if !offline && matches!(sub, "install" | "create" | "update") =>
        {
            fetch(
                format!("{name} {sub}"),
                "a conda channel",
                Allow::Beforehand,
            )
        }
        "npm" if !offline && matches!(sub, "install" | "i" | "in" | "ci" | "add" | "update") => {
            fetch(format!("npm {sub}"), NPM, NODE)
        }
        "pnpm" if !offline && matches!(sub, "install" | "i" | "add" | "update" | "up") => {
            fetch(format!("pnpm {sub}"), NPM, NODE)
        }
        // A bare `yarn` installs.
        "yarn" if !offline && matches!(sub, "" | "install" | "add" | "upgrade" | "up") => {
            fetch(format!("yarn {sub}").trim_end(), NPM, NODE)
        }
        "bun" if matches!(sub, "install" | "i" | "add") => fetch(format!("bun {sub}"), NPM, NODE),
        "cargo"
            if matches!(sub, "install" | "fetch") && !has_any(args, &["--offline", "--frozen"]) =>
        {
            let allow = if sub == "fetch" {
                Allow::CratesFetch
            } else {
                Allow::Beforehand
            };
            fetch(format!("cargo {sub}"), "crates.io", allow)
        }
        "gem" if sub == "install" => fetch("gem install", "RubyGems", Allow::Beforehand),
        "bundle" | "bundler" if matches!(sub, "install" | "update" | "add") => fetch(
            format!("{name} {sub}"),
            "RubyGems",
            Allow::Install("bundler"),
        ),
        "go" => match (sub, sub2) {
            ("get", _) => fetch("go get", "the Go module proxy", Allow::Install("go")),
            ("mod", "download") => fetch(
                "go mod download",
                "the Go module proxy",
                Allow::Install("go"),
            ),
            _ => None,
        },
        "composer" if matches!(sub, "install" | "require" | "update") => fetch(
            format!("composer {sub}"),
            "Packagist",
            Allow::Install("composer"),
        ),
        "dotnet" => match (sub, sub2) {
            ("restore", _) => fetch("dotnet restore", "NuGet", Allow::Install("dotnet")),
            ("add", "package") => fetch("dotnet add package", "NuGet", Allow::Install("dotnet")),
            ("tool", "install") => fetch("dotnet tool install", "NuGet", Allow::Beforehand),
            _ => None,
        },
        _ => None,
    }
}

/// Why a download is refused, and what allows the work: the project's
/// consent, under which kGraf fetches crates.io dependencies or runs the
/// package manager's install on the host before the workers start; else an
/// install made before the run.
pub(crate) fn refusal(fetch: &PackageFetch) -> String {
    let lead = format!(
        "`{}` downloads packages from {}, and {PACKAGE_FETCH_MARK} outside a disposable \
         container: the network is off in a worker's shell, and kGraf fetches or installs a \
         project's dependencies itself, on the host, with the project's consent (decision \
         0087). No permission mode or grant lifts this.",
        fetch.form, fetch.registry
    );
    match fetch.allow {
        Allow::CratesFetch => format!(
            "{lead} To allow it, run `kgraf deps allow` once for this project, or pass \
             `--fetch-deps` for one run: kGraf then fetches the crates.io dependencies before \
             the workers start, and again when a build finds one missing."
        ),
        Allow::Install(name) => format!(
            "{lead} To allow it, declare the package in the project's dependency files and run \
             `kgraf deps allow {name}` once for this project (or pass `--fetch-deps` for one \
             run): kGraf then installs what those files name before the workers start. Or \
             install it in the project's environment before the run."
        ),
        Allow::Beforehand => format!(
            "{lead} kGraf does not install this for a run: install it before the run, so a \
             check finds it installed."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    fn form(words: &[&str]) -> Option<String> {
        package_fetch(&argv(words)).map(|f| f.form)
    }

    #[test]
    fn the_install_forms_of_each_manager_are_downloads() {
        for (words, expected) in [
            (&["pip", "install", "x"][..], "pip install"),
            (&["pip3", "-q", "install", "x"], "pip3 install"),
            (&["/usr/bin/pip3.12", "download", "x"], "pip3.12 download"),
            (&["PIP.EXE", "wheel", "."], "pip wheel"),
            (
                &["python3", "-m", "pip", "install", "x"],
                "python3 -m pip install",
            ),
            (
                &["python", "-I", "-m", "pip", "-q", "install", "x"],
                "python -m pip install",
            ),
            (&["uv", "pip", "install", "x"], "uv pip install"),
            (&["uv", "add", "x"], "uv add"),
            (&["uv", "tool", "install", "ruff"], "uv tool install"),
            (&["pipx", "install", "x"], "pipx install"),
            (&["poetry", "install"], "poetry install"),
            (&["conda", "install", "x"], "conda install"),
            (&["npm", "install"], "npm install"),
            (&["npm", "ci"], "npm ci"),
            (&["npm", "i", "x"], "npm i"),
            (&["pnpm", "add", "x"], "pnpm add"),
            (&["yarn"], "yarn"),
            (&["yarn", "add", "x"], "yarn add"),
            (&["bun", "install"], "bun install"),
            (&["cargo", "fetch"], "cargo fetch"),
            (&["cargo", "install", "ripgrep"], "cargo install"),
            (&["gem", "install", "x"], "gem install"),
            (&["bundle", "install"], "bundle install"),
            (&["go", "get", "x@v1"], "go get"),
            (&["go", "mod", "download"], "go mod download"),
            (&["composer", "require", "x"], "composer require"),
            (&["dotnet", "restore"], "dotnet restore"),
            (&["dotnet", "add", "package", "x"], "dotnet add package"),
        ] {
            assert_eq!(form(words).as_deref(), Some(expected), "{words:?}");
        }
    }

    #[test]
    fn builds_tests_queries_and_offline_forms_are_not_downloads() {
        for words in [
            &["pip", "show", "x"][..],
            &["pip", "list"],
            &["pip", "install", "--no-index", "--find-links", "dist", "x"],
            &["python3", "-m", "pytest"],
            &["python3", "-c", "import pip"],
            &["python3", "script.py", "install"],
            &["uv", "run", "pytest"],
            &["uv", "sync", "--offline"],
            &["npm", "test"],
            &["npm", "run", "install-deps"],
            &["npm", "ci", "--offline"],
            &["yarn", "test"],
            &["cargo", "build"],
            &["cargo", "fetch", "--offline"],
            &["cargo", "install", "--frozen", "--path", "."],
            &["go", "test", "./..."],
            &["go", "mod", "tidy"],
            &["dotnet", "build"],
            &["gem", "list"],
            &["pipeline", "install"],
        ] {
            assert_eq!(form(words), None, "{words:?}");
        }
    }

    #[test]
    fn the_refusal_says_why_and_what_allows_it() {
        let pip = package_fetch(&argv(&["pip", "install", "x"])).expect("a download");
        let why = refusal(&pip);
        assert!(is_package_fetch_refusal(&why), "{why}");
        assert!(
            why.contains("`pip install` downloads packages from PyPI"),
            "{why}"
        );
        assert!(why.contains("decision 0087"), "{why}");
        assert!(
            why.contains("No permission mode or grant lifts this"),
            "{why}"
        );
        assert!(why.contains("`kgraf deps allow python`"), "{why}");
        assert!(why.contains("`--fetch-deps`"), "{why}");
        assert!(why.contains("dependency files"), "{why}");

        let npm = package_fetch(&argv(&["npm", "ci"])).expect("a download");
        assert!(refusal(&npm).contains("`kgraf deps allow npm`"));

        let fetch = package_fetch(&argv(&["cargo", "fetch"])).expect("a download");
        let why = refusal(&fetch);
        assert!(why.contains("`kgraf deps allow`"), "{why}");
        assert!(why.contains("crates.io dependencies"), "{why}");

        let tool = package_fetch(&argv(&["cargo", "install", "ripgrep"])).expect("a download");
        let why = refusal(&tool);
        assert!(why.contains("install it before the run"), "{why}");
        assert!(!why.contains("kgraf deps allow"), "{why}");
    }
}
