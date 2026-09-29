//! The window's question when a run's dependencies need fetching and the
//! project has no answer yet (decision 0087).
//!
//! The run's start fails at the host's dependency gate, before anything of it
//! is built. The box then shows the question as a pinned approval card (the
//! existing card, CMP-CARD-1 and its confirm step, CMP-CARD-3) with three
//! choices: "Allow once" grants this run only, "Always for this project"
//! stores the consent, "Not now" stores nothing and starts nothing. On either
//! allow, the run starts again.
//!
//! ```text
//!   start --gate: no answer--> Asking(question, restart)
//!   Asking --Allow once--> grant(new run) --> start again
//!   Asking --Always--> store "allowed" --> start again
//!   Asking --Not now--> idle (the instruction is said)
//! ```

use kgraf_core::RunId;
use kgraf_host::Host;
use kgraf_host::deps::{Consent, ConsentQuestion};
use kgraf_orchestrator::{AskAnswer, PendingAsk};

/// The card's tool name: the box words its buttons for this prompt by it.
pub const FETCH_PROMPT_TOOL: &str = "dependency fetch";

/// The card's id, outside the desk's own ids (which count up from zero).
pub const FETCH_PROMPT_ID: u64 = u64::MAX;

/// What a person chose on the card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchChoice {
    AllowOnce,
    AlwaysForProject,
    NotNow,
}

/// The card's three buttons, as the choice they stand for.
pub fn choice_of(answer: &AskAnswer) -> FetchChoice {
    match answer {
        AskAnswer::AllowOnce => FetchChoice::AllowOnce,
        AskAnswer::AllowAlways => FetchChoice::AlwaysForProject,
        AskAnswer::Deny { .. } => FetchChoice::NotNow,
    }
}

/// Record `choice` on the project's host for the run about to start as
/// `run`. Returns whether to start it.
pub fn apply(
    host: &Host,
    run: &RunId,
    question: &ConsentQuestion,
    choice: FetchChoice,
) -> Result<bool, String> {
    match choice {
        FetchChoice::AllowOnce => {
            host.grant_dependency_fetch(run);
            Ok(true)
        }
        FetchChoice::AlwaysForProject => {
            host.set_dependency_consent(question.registry, Consent::Allowed)
                .map_err(|e| format!("the answer was not stored: {e}"))?;
            Ok(true)
        }
        FetchChoice::NotNow => Ok(false),
    }
}

/// The card for `question`.
pub fn pending_ask(question: &ConsentQuestion) -> PendingAsk {
    PendingAsk {
        id: FETCH_PROMPT_ID,
        worker: "kGraf".to_owned(),
        task: "run start".to_owned(),
        tool: FETCH_PROMPT_TOOL.to_owned(),
        question: question.text(),
        asked_by: "the dependency check".to_owned(),
        input: format!(
            "{}\nmissing: {}",
            question.project.display(),
            question.detail
        ),
    }
}

/// What "Not now" leaves the user to do.
pub fn not_now_line(question: &ConsentQuestion) -> String {
    format!(
        "not started: the dependencies are not in the local cache. Run `{}` in the project, \
         or `kgraf deps allow`, then start again",
        question.registry.manual_command()
    )
}

/// How to start the run again once allowed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Restart {
    Start(String),
    Resume(RunId),
}

/// The question the box is showing, and the start it holds.
#[derive(Clone, Debug)]
pub(crate) struct FetchPrompt {
    pub(crate) question: ConsentQuestion,
    pub(crate) restart: Restart,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_button_maps_to_one_choice() {
        assert_eq!(choice_of(&AskAnswer::AllowOnce), FetchChoice::AllowOnce);
        assert_eq!(
            choice_of(&AskAnswer::AllowAlways),
            FetchChoice::AlwaysForProject
        );
        assert_eq!(
            choice_of(&AskAnswer::Deny { reason: "x".into() }),
            FetchChoice::NotNow
        );
    }

    #[test]
    fn the_card_says_what_a_fetch_does_and_what_is_missing() {
        let q = ConsentQuestion {
            registry: kgraf_host::deps::Registry::CratesIo,
            project: "/p".into(),
            detail: "no matching package named `itoa`".into(),
        };
        let card = pending_ask(&q);
        assert_eq!(card.id, FETCH_PROMPT_ID);
        assert_eq!(card.tool, FETCH_PROMPT_TOOL);
        assert!(card.question.contains("crates.io"), "{}", card.question);
        assert!(card.question.contains("outside the workers' sandbox"));
        assert!(card.input.contains("itoa"));
        assert!(not_now_line(&q).contains("kgraf deps allow"));
    }

    /// Through the window's own backend, over a real project host whose
    /// package manager is a fake (no registry is contacted).
    mod through_the_window {
        use super::super::*;
        use crate::orchestrator_box::{BackendEvent, BoxBackend};
        use crate::orchestrator_host::{HostParts, OrchestratorHost};
        use crate::scheduler_bg::ProjectFactory;
        use kgraf_host::RunConfig;
        use kgraf_host::deps::{
            DependencyTool, DepsError, FetchReport, FetchSite, Readiness, Registry,
        };
        use kgraf_orchestrator::AskDesk;
        use std::path::Path;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        /// Missing until fetched.
        #[derive(Default)]
        struct Fake {
            ready: Mutex<bool>,
            fetches: Mutex<usize>,
        }

        impl DependencyTool for Fake {
            fn registry(&self) -> Registry {
                Registry::CratesIo
            }
            fn applies(&self, _: &FetchSite) -> bool {
                true
            }
            fn check(&self, _: &FetchSite) -> Readiness {
                if *self.ready.lock().unwrap() {
                    Readiness::Ready
                } else {
                    Readiness::Missing("no matching package named `itoa`".into())
                }
            }
            fn fetch(
                &self,
                _: &FetchSite,
                _: &kgraf_host::deps::Stop,
            ) -> Result<FetchReport, DepsError> {
                *self.fetches.lock().unwrap() += 1;
                *self.ready.lock().unwrap() = true;
                Ok(FetchReport {
                    registry: Registry::CratesIo,
                    contacted: vec!["Updating crates.io index".into()],
                    downloaded: 1,
                    elapsed: Duration::from_millis(1),
                })
            }
        }

        /// A model that refuses every call: the run, once built, ends at its
        /// first planning call. These tests are about the start.
        struct NoModel;

        #[async_trait::async_trait]
        impl kgraf_provider::Provider for NoModel {
            fn adapter(&self) -> &'static str {
                "fake"
            }
            fn protocol(&self) -> kgraf_core::Protocol {
                kgraf_core::Protocol::Additive
            }
            async fn complete(
                &self,
                request: &kgraf_provider::Request,
                _token: &kgraf_provider::CallToken,
            ) -> Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
                Err(kgraf_provider::ProviderError::ModelUnsupported {
                    adapter: "fake",
                    model: request.model.clone(),
                    available: "none".into(),
                })
            }
        }

        fn git(dir: &Path, args: &[&str]) {
            let out = std::process::Command::new("git")
                .current_dir(dir)
                .args(["-c", "core.hooksPath=/dev/null"])
                .args(args)
                .output()
                .expect("git");
            assert!(out.status.success(), "{args:?}: {out:?}");
        }

        fn window() -> (tempfile::TempDir, Arc<Host>, Arc<Fake>, OrchestratorHost) {
            let dir = tempfile::tempdir().expect("dir");
            let root = dir.path().join("repo");
            std::fs::create_dir_all(&root).expect("mkdir");
            git(&root, &["init", "-q", "-b", "main"]);
            git(&root, &["config", "user.name", "t"]);
            git(&root, &["config", "user.email", "t@t"]);
            std::fs::write(root.join("README.md"), "# t\n").expect("write");
            std::fs::write(root.join(".gitignore"), ".kgraf/\n").expect("write");
            git(&root, &["add", "."]);
            git(&root, &["commit", "-q", "-m", "init"]);
            let prices = dir.path().join("prices.json");
            std::fs::write(
                &prices,
                r#"{"m": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1}}"#,
            )
            .expect("prices");
            let fake = Arc::new(Fake::default());
            let host = Arc::new(
                Host::open(RunConfig::new(&root, vec!["fake:m".into()], &prices))
                    .expect("open")
                    .with_route_provider("fake", Arc::new(NoModel))
                    .expect("route")
                    .with_dependency_tool(fake.clone()),
            );
            let factory = Arc::new(ProjectFactory::with_host(host.clone()));
            let parts = HostParts {
                factory: factory.clone(),
                housekeeping: factory,
                desk: Arc::new(AskDesk::new(None, Arc::new(|| {}))),
                router: None,
            };
            let window = OrchestratorHost::new(Some(parts), Arc::new(|| {})).expect("window");
            (dir, host, fake, window)
        }

        /// Poll until `done`; every event polled is kept.
        fn until(
            w: &mut OrchestratorHost,
            events: &mut Vec<BackendEvent>,
            what: &str,
            done: impl Fn(&OrchestratorHost, &[BackendEvent]) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                events.extend(w.poll());
                if done(w, events) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {what}: {events:?}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn asked(w: &OrchestratorHost) -> bool {
            w.pending_asks().iter().any(|a| a.id == FETCH_PROMPT_ID)
        }

        fn consent(host: &Host) -> Consent {
            host.dependency_consents()
                .get(Registry::CratesIo)
                .expect("read")
        }

        #[test]
        fn a_start_that_needs_a_fetch_asks_and_allow_once_starts_it_storing_nothing() {
            let (_dir, host, fake, mut w) = window();
            let mut events = Vec::new();
            w.start("make x").expect("start");
            until(&mut w, &mut events, "the question", |w, _| asked(w));
            assert!(
                !events.iter().any(|e| matches!(e, BackendEvent::Error(_))),
                "a question, not a failure: {events:?}"
            );
            assert!(w.last_failure().is_none());
            assert_eq!(*fake.fetches.lock().unwrap(), 0);
            w.answer(FETCH_PROMPT_ID, AskAnswer::AllowOnce)
                .expect("answered");
            assert!(!asked(&w), "the card is gone once answered");
            until(&mut w, &mut events, "the fetch", |_, _| {
                *fake.fetches.lock().unwrap() == 1
            });
            assert_eq!(consent(&host), Consent::Unset, "allow once stores nothing");
        }

        #[test]
        fn always_for_this_project_stores_the_consent_and_starts() {
            let (_dir, host, fake, mut w) = window();
            let mut events = Vec::new();
            w.start("make x").expect("start");
            until(&mut w, &mut events, "the question", |w, _| asked(w));
            w.answer(FETCH_PROMPT_ID, AskAnswer::AllowAlways)
                .expect("answered");
            assert_eq!(consent(&host), Consent::Allowed);
            until(&mut w, &mut events, "the fetch", |_, _| {
                *fake.fetches.lock().unwrap() == 1
            });
        }

        #[test]
        fn not_now_starts_nothing_stores_nothing_and_says_what_to_do() {
            let (_dir, host, fake, mut w) = window();
            let mut events = Vec::new();
            w.start("make x").expect("start");
            until(&mut w, &mut events, "the question", |w, _| asked(w));
            w.answer(
                FETCH_PROMPT_ID,
                AskAnswer::Deny {
                    reason: "not now".into(),
                },
            )
            .expect("answered");
            until(&mut w, &mut events, "the instruction", |_, ev| {
                ev.iter()
                    .any(|e| matches!(e, BackendEvent::Notice(t) if t.contains("kgraf deps allow")))
            });
            assert_eq!(consent(&host), Consent::Unset);
            assert_eq!(*fake.fetches.lock().unwrap(), 0);
            assert!(!asked(&w));
            assert!(
                matches!(
                    w.state(),
                    crate::orchestrator_box::BackendState::Idle { .. }
                ),
                "no run started"
            );
        }
    }
}
