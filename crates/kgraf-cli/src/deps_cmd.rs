//! `kgraf deps`: the project's consent to fetch dependencies (decision 0087),
//! and the terminal prompt a run asks with when nobody has answered yet.
//!
//! Workers build with the network off (decision 0084). A run whose
//! dependencies are not in the local cache stops before it starts unless the
//! project allows kGraf to fetch them. `kgraf deps allow` is the one-time
//! setup step; `kgraf deps fetch` fetches now, once.

use anyhow::{Context, Result, bail};
use kgraf_host::deps::{
    CargoDeps, ConsentQuestion, ConsentStore, DependencyTool, FetchSite, Readiness, Registry, Stop,
    Trigger, fetch_and_confirm,
};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

pub use kgraf_host::deps::Consent;

fn store(root: &Path) -> Result<ConsentStore> {
    Ok(ConsentStore::in_state_dir(&kgraf_host::state_dir(root)?))
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
        return Ok(());
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
    Ok(())
}

/// `kgraf deps allow|refuse|reset`: store the answer.
pub fn set(root: &Path, consent: Consent) -> Result<()> {
    let store = store(root)?;
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

    #[test]
    fn allow_refuse_and_reset_store_the_projects_answer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap();
        set(root, Consent::Allowed).unwrap();
        assert_eq!(
            store(root).unwrap().get(Registry::CratesIo).unwrap(),
            Consent::Allowed
        );
        set(root, Consent::Refused).unwrap();
        assert_eq!(
            store(root).unwrap().get(Registry::CratesIo).unwrap(),
            Consent::Refused
        );
        set(root, Consent::Unset).unwrap();
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
}
