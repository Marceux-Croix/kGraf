//! `kgraf deps`: the project's consent to fetch dependencies (decision 0087),
//! and to run its other package managers' installs in kGraf's sandbox
//! (amended 2026-10-07); and the terminal prompt a run asks with when nobody
//! has answered yet.
//!
//! Workers build with the network off (decision 0084). A run whose
//! dependencies are not in the local cache stops before it starts unless the
//! project allows kGraf to fetch them. `kgraf deps allow` is the one-time
//! setup step for cargo, `kgraf deps allow npm` (or `python`, `go`, `maven`,
//! `gradle`, `dotnet`, `composer`, `bundler`) for an install command;
//! `kgraf deps fetch` fetches now, `kgraf deps install` installs now, once.

use anyhow::{Context, Result, bail};
use kgraf_core::RunId;
use kgraf_host::Host;
use kgraf_host::deps::{
    CargoDeps, ConsentQuestion, ConsentStore, ConsentSubject, DependencyTool, Ecosystem, FetchSite,
    InstallEnv, InstallHosts, InstallRunner, MissingInstall, Readiness, Registry, SandboxedInstall,
    Stop, Trigger, fetch_and_confirm, missing_installs,
};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

pub use kgraf_host::deps::Consent;

fn store(root: &Path) -> Result<ConsentStore> {
    let store = ConsentStore::in_state_dir(&kgraf_host::state_dir(root)?);
    // Tests never make or read the user's own install answer key.
    #[cfg(test)]
    let store = store.with_key(*b"kgraf-cli-test-consent-key-32by!");
    Ok(store)
}

/// The installs the checkout at `root` lacks, as a run's start finds them.
fn missing(root: &Path) -> Vec<MissingInstall> {
    missing_installs(
        root,
        &InstallEnv::for_checkout(root),
        &kgraf_host::pytest_importable,
    )
}

/// `kgraf deps status`: the stored consent, and whether the dependencies are
/// ready offline now (the same check a run makes; no network).
pub fn status(root: &Path) -> Result<()> {
    let store = store(root)?;
    let registry = Registry::CratesIo;
    println!(
        "{registry}: {} ({})",
        store.get(registry)?.name(),
        store.path().display()
    );
    cargo_status(root);
    let installs = missing(root);
    if installs.is_empty() {
        println!("installs: nothing missing");
    }
    for m in &installs {
        println!(
            "install: {} in {} ({}): {}; the stored answer for this command: {}",
            m.command,
            m.place(),
            m.ecosystem.name(),
            m.what,
            store.install_consent(m)?.name()
        );
    }
    for (key, answer) in store.installs()? {
        println!(
            "install answer {key}: {} for `{}`",
            answer.consent.name(),
            answer.command
        );
    }
    Ok(())
}

fn cargo_status(root: &Path) {
    let tool = CargoDeps::default();
    let site = FetchSite::project(root);
    if !tool.applies(&site) {
        match tool.nested_manifest(&site) {
            Some(nested) => println!(
                "no Cargo.toml at the project root, but {} exists: kGraf checks and fetches \
                 only for a root Cargo.toml (run `cargo fetch` in that folder yourself)",
                nested.display()
            ),
            None => println!("no Cargo.toml at the project root: nothing to fetch"),
        }
        return;
    }
    let started = std::time::Instant::now();
    let readiness = tool.check(&site);
    let ms = started.elapsed().as_millis();
    match readiness {
        Readiness::Ready => println!("dependencies: ready offline (checked in {ms} ms)"),
        Readiness::Missing(d) => println!("dependencies: missing from the local cache: {d}"),
        Readiness::Unchecked(d) => println!("dependencies: the check could not decide: {d}"),
        Readiness::NotApplicable => println!("dependencies: nothing to fetch"),
    }
}

/// Which consent `kgraf deps allow|refuse|reset` is about: cargo's fetch
/// when no ecosystem is named, else that ecosystem's install commands.
pub fn parse_ecosystem(word: Option<&str>) -> Result<Option<Ecosystem>> {
    match word {
        None => Ok(None),
        Some("cargo" | "crates.io") => Ok(None),
        Some(w) => Ecosystem::parse(w).map(Some).with_context(|| {
            format!(
                "`{w}` is no package manager kGraf installs for; name one of cargo, {}",
                Ecosystem::ALL.map(Ecosystem::key).join(", ")
            )
        }),
    }
}

/// `kgraf deps allow|refuse|reset [ECOSYSTEM]`: store the answer.
pub fn set(root: &Path, consent: Consent, ecosystem: Option<Ecosystem>) -> Result<()> {
    let store = store(root)?;
    let Some(ecosystem) = ecosystem else {
        return set_cargo(&store, consent);
    };
    if consent == Consent::Unset {
        let removed = store.reset_installs(ecosystem)?;
        println!(
            "{}: {removed} stored install answer(s) forgotten; a run that needs an install \
             asks again",
            ecosystem.name()
        );
        return Ok(());
    }
    let installs: Vec<MissingInstall> = missing(root)
        .into_iter()
        .filter(|m| m.ecosystem == ecosystem)
        .collect();
    if installs.is_empty() {
        println!(
            "{}: no install is missing now, so there is no command to answer for; nothing \
             stored. kGraf asks when a run needs one.",
            ecosystem.name()
        );
        return Ok(());
    }
    for m in &installs {
        store.set_install(m, consent)?;
        let what = match consent {
            Consent::Allowed => {
                "kGraf runs it in its sandbox when a run needs it, without asking again (writes \
                 only in the project, never its .git or .kgraf; network only to the allowed \
                 registry hosts, except on Windows); a changed command asks again"
            }
            _ => "kGraf never runs it; runs go on without the install and say so",
        };
        println!(
            "{}: {} `{}` in {} ({what})",
            ecosystem.name(),
            consent.name(),
            m.command,
            m.place()
        );
    }
    Ok(())
}

fn set_cargo(store: &ConsentStore, consent: Consent) -> Result<()> {
    store.set(Registry::CratesIo, consent)?;
    let what = match consent {
        Consent::Allowed => {
            "kGraf fetches this project's dependencies from crates.io when a run needs them, \
             outside the workers' sandbox, without asking again"
        }
        Consent::Refused => {
            "kGraf never fetches this project's dependencies; a run that needs a fetch stops \
             before it starts"
        }
        Consent::Unset => "a run that needs a fetch asks first (or stops, when nobody can answer)",
    };
    println!("crates.io: {} ({what})", consent.name());
    if consent == Consent::Allowed {
        println!(
            "{}",
            kgraf_host::deps::worker_fetch_scope(Registry::CratesIo)
        );
    }
    Ok(())
}

/// `kgraf deps fetch`: fetch now, once, on the user's own command.
pub fn fetch(root: &Path) -> Result<()> {
    let tool = CargoDeps::default();
    let site = FetchSite::project(root);
    if !tool.applies(&site) {
        bail!("no Cargo.toml at {}: nothing to fetch", root.display());
    }
    if tool.check(&site) == Readiness::Ready {
        println!("dependencies: already ready offline; nothing fetched");
        return Ok(());
    }
    let report = fetch_and_confirm(&tool, &site, Trigger::RunStart, &Stop::default())
        .context("the fetch did not make the dependencies ready")?;
    println!("fetched    {}", report.summary());
    Ok(())
}

/// `kgraf deps install [ECOSYSTEM]`: run each missing install now, once, in
/// kGraf's sandbox, on the user's own command (nothing is stored).
pub fn install(root: &Path, ecosystem: Option<Ecosystem>) -> Result<()> {
    let installs: Vec<MissingInstall> = missing(root)
        .into_iter()
        .filter(|m| ecosystem.is_none_or(|e| m.ecosystem == e))
        .collect();
    if installs.is_empty() {
        println!("installs: nothing missing; nothing run");
        return Ok(());
    }
    let hosts = InstallHosts::load_default().map_err(anyhow::Error::msg)?;
    let runner = SandboxedInstall::default();
    for m in &installs {
        println!(
            "installing `{}` in {} ({}) in kGraf's sandbox",
            m.command,
            m.place(),
            m.ecosystem.name()
        );
        let report = runner.run(m, root, hosts.for_ecosystem(m.ecosystem), &Stop::default())?;
        println!("installed  {}", report.summary());
    }
    let left: Vec<MissingInstall> = missing(root)
        .into_iter()
        .filter(|m| {
            installs
                .iter()
                .any(|i| i.ecosystem == m.ecosystem && i.dir == m.dir)
        })
        .collect();
    if let Some(m) = left.first() {
        bail!(
            "`{}` in {} succeeded, but the install is still missing: {}",
            m.command,
            m.place(),
            m.what
        );
    }
    Ok(())
}

/// A person's answer to the run-start question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// This run only; nothing stored.
    Once,
    /// Store "allowed".
    Always,
    /// Not this run; nothing stored.
    NotNow,
    /// Store "refused".
    Never,
}

/// The terminal's answer words.
pub fn parse_answer(line: &str) -> Option<Answer> {
    match line.trim().to_ascii_lowercase().as_str() {
        "o" | "once" | "y" | "yes" => Some(Answer::Once),
        "a" | "always" => Some(Answer::Always),
        "n" | "no" | "not now" | "" => Some(Answer::NotNow),
        "v" | "never" => Some(Answer::Never),
        _ => None,
    }
}

/// Store or grant `answer` to `question` for the run `run`. Returns whether
/// to build the run again ("not now" does not).
pub fn apply_answer(
    host: &Host,
    run: &RunId,
    question: &ConsentQuestion,
    answer: Answer,
) -> Result<bool> {
    let consent = match answer {
        Answer::NotNow => return Ok(false),
        Answer::Once => {
            host.grant_dependency_once(run, &question.subject);
            return Ok(true);
        }
        Answer::Always => Consent::Allowed,
        Answer::Never => Consent::Refused,
    };
    match &question.subject {
        ConsentSubject::Fetch(registry) => host.set_dependency_consent(*registry, consent)?,
        ConsentSubject::Install(installs) => {
            for install in installs {
                host.set_install_consent(install, consent)?;
            }
        }
    }
    Ok(true)
}

/// Whether a person is at the terminal to answer (stdin and stderr both are
/// terminals). Otherwise the run fails with the instruction, never a guess.
pub fn attended() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Ask on the terminal; an empty answer is "not now".
pub fn ask(question: &ConsentQuestion) -> Result<Answer> {
    let mut err = std::io::stderr();
    writeln!(err, "\n{}", question.text())?;
    writeln!(err, "Missing: {}", question.detail)?;
    let stdin = std::io::stdin();
    loop {
        write!(
            err,
            "[o]nce for this run, [a]lways for this project, [n]ot now, ne[v]er for this \
             project: "
        )?;
        err.flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(Answer::NotNow);
        }
        if let Some(a) = parse_answer(&line) {
            return Ok(a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_answer_word_maps_to_one_answer() {
        for (word, want) in [
            ("o", Answer::Once),
            ("Once\n", Answer::Once),
            ("y", Answer::Once),
            ("a", Answer::Always),
            ("always", Answer::Always),
            ("n", Answer::NotNow),
            ("", Answer::NotNow),
            ("v", Answer::Never),
            ("never", Answer::Never),
        ] {
            assert_eq!(parse_answer(word), Some(want), "{word:?}");
        }
        assert_eq!(parse_answer("maybe"), None);
    }

    fn git_init(root: &Path) {
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap();
    }

    #[test]
    fn allow_refuse_and_reset_store_the_projects_answer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_init(root);
        set(root, Consent::Allowed, None).unwrap();
        assert_eq!(
            store(root).unwrap().get(Registry::CratesIo).unwrap(),
            Consent::Allowed
        );
        set(root, Consent::Refused, None).unwrap();
        assert_eq!(
            store(root).unwrap().get(Registry::CratesIo).unwrap(),
            Consent::Refused
        );
        set(root, Consent::Unset, None).unwrap();
        assert_eq!(
            store(root).unwrap().get(Registry::CratesIo).unwrap(),
            Consent::Unset
        );
        assert!(
            root.join(".kgraf")
                .join(kgraf_host::deps::CONSENT_FILE)
                .is_file(),
            "in the project's state directory"
        );
    }

    /// `kgraf deps allow npm` stores the answer for the exact command the
    /// project's missing install needs, and `reset npm` forgets it.
    #[test]
    fn allow_npm_stores_the_current_command_and_reset_forgets_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_init(root);
        std::fs::write(root.join("package.json"), r#"{"dependencies":{"x":"1"}}"#).unwrap();
        std::fs::write(root.join("package-lock.json"), "{}").unwrap();
        let eco = parse_ecosystem(Some("npm")).unwrap();
        set(root, Consent::Allowed, eco).unwrap();
        let stored = store(root).unwrap().installs().unwrap();
        assert_eq!(stored["npm:"].command, "npm ci");
        assert_eq!(stored["npm:"].consent, Consent::Allowed);
        set(root, Consent::Unset, eco).unwrap();
        assert!(store(root).unwrap().installs().unwrap().is_empty());
        assert_eq!(parse_ecosystem(Some("cargo")).unwrap(), None);
        assert!(parse_ecosystem(Some("cobol")).is_err());
    }
}
