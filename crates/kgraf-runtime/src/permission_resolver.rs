//! Who answers an approval ask under the user's permission mode
//! (decision 0094).
//!
//! [`ModeResolver`] wraps the resolver that asks a person (the box's desk,
//! the terminal prompt, or the unattended denial):
//!
//! - **A hard ask** (from [`PERMISSION_RULES_HOOK`]: an ask rule, a critical
//!   path) always goes to the person, whatever the mode. With nobody there it
//!   is denied at once: a headless run denies what would still ask.
//! - **Any other ask** is routed by the mode ([`PermissionControl::route_ask`]).
//!   A grant is recorded on the control (the trace, the run record and the
//!   activity card read it from there) and the call runs; otherwise the
//!   person answers.
//! - **A waiting ask follows mode changes.** While the person has not
//!   answered, every mode change re-routes the ask; a mode that now grants it
//!   releases it at once, and the person's card leaves the box (dropping the
//!   inner wait removes it).

use crate::AskResolver;
use kgraf_tools::approval::{PausedCall, UserAnswer};
use kgraf_tools::permissions::{AskRoute, PERMISSION_RULES_HOOK, PermissionControl};
use std::sync::Arc;

/// Routes each ask by the live mode before a person is asked.
pub struct ModeResolver {
    inner: Arc<dyn AskResolver>,
    control: Arc<PermissionControl>,
    attended: bool,
}

impl ModeResolver {
    /// `attended`: a person can answer (the box, a terminal). Without one, a
    /// hard ask is denied here and every other ask the mode does not grant
    /// goes to `inner`, which denies it unattended.
    pub fn new(
        inner: Arc<dyn AskResolver>,
        control: Arc<PermissionControl>,
        attended: bool,
    ) -> Self {
        Self {
            inner,
            control,
            attended,
        }
    }

    fn grant(&self, paused: &PausedCall, by: &str) -> UserAnswer {
        let call = paused.call();
        let subject = subject_of(&call.tool, &call.input);
        // Staged: the chain goes on after this answer, and a later hook may
        // still refuse the call.
        self.control
            .stage_grant(call.worker.as_str(), &call.call_id, &subject, by);
        UserAnswer::Allow
    }
}

/// What a grant names: a shell call's program, or the tool. A command that
/// runs several programs (`env curl x`) names each, the innermost first,
/// so the record says what ran and not only its wrapper (decision 0094,
/// amended 2026-10-06).
fn subject_of(tool: &str, input: &serde_json::Value) -> String {
    let programs = (tool == "shell")
        .then(|| kgraf_tools::command_argv(input))
        .flatten()
        .and_then(|argv| kgraf_tools::permissions::programs_to_grant(&argv).ok())
        .filter(|ps| ps.len() > 1);
    match programs {
        Some(ps) => ps
            .iter()
            .rev()
            .map(|p| format!("`{p}`"))
            .collect::<Vec<_>>()
            .join(" and "),
        None => format!("`{}`", crate::shortfall::capability_of(tool, input)),
    }
}

#[async_trait::async_trait]
impl AskResolver for ModeResolver {
    async fn answer(&self, paused: &PausedCall) -> UserAnswer {
        if paused.asked_by() == PERMISSION_RULES_HOOK {
            if !self.attended {
                return UserAnswer::Deny {
                    reason: format!(
                        "{} Nobody attends this run, so what would ask is denied.",
                        paused.question()
                    ),
                };
            }
            return self.inner.answer(paused).await;
        }
        let call = paused.call();
        if let AskRoute::Grant { by } = self.control.route_ask(&call.tool, &call.input) {
            return self.grant(paused, by);
        }
        let mut changes = self.control.subscribe();
        let person = self.inner.answer(paused);
        tokio::pin!(person);
        loop {
            tokio::select! {
                answer = &mut person => return answer,
                changed = changes.changed() => {
                    if changed.is_err() {
                        // The control is gone; only the person can answer now.
                        return person.await;
                    }
                    if let AskRoute::Grant { by } =
                        self.control.route_ask(&call.tool, &call.input)
                    {
                        return self.grant(paused, by);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_tools::permissions::{
        AllowSet, ModeSource, PermissionMode, PermissionSettings, ProcessFacts,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A paused call as a chain hands it over (the resolver never checks the
    /// seal; the chain does on resume).
    fn paused(tool: &str, asked_by: &str, command: &[&str]) -> PausedCall {
        serde_json::from_value(serde_json::json!({
            "call": {
                "call_id": "c1",
                "tool": tool,
                "input": {"command": command},
                "worker": "w1",
                "task": "t1"
            },
            "asked_by": asked_by,
            "question": "run it?",
            "next_hook": 1,
            "rewritten": false,
            "deadline_ms": null,
            "chain_digest": "d",
            "seal": "s"
        }))
        .expect("paused call")
    }

    /// The person: answers `answer` after `delay`, counting how often asked.
    struct Person {
        asked: Arc<AtomicUsize>,
        answer: UserAnswer,
        delay: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl AskResolver for Person {
        async fn answer(&self, _paused: &PausedCall) -> UserAnswer {
            self.asked.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.answer.clone()
        }
    }

    fn person(answer: UserAnswer, delay_ms: u64) -> (Arc<dyn AskResolver>, Arc<AtomicUsize>) {
        let asked = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Person {
                asked: asked.clone(),
                answer,
                delay: std::time::Duration::from_millis(delay_ms),
            }),
            asked,
        )
    }

    fn deny() -> UserAnswer {
        UserAnswer::Deny {
            reason: "the person said no".into(),
        }
    }

    fn bypass() -> Arc<PermissionControl> {
        Arc::new(
            PermissionControl::new(
                PermissionSettings {
                    mode: PermissionMode::Bypass,
                    ..PermissionSettings::default()
                },
                ModeSource::CommandLine,
                ProcessFacts::default(),
            )
            .expect("bypass"),
        )
    }

    #[tokio::test]
    async fn bypass_grants_an_ordinary_ask_and_records_it_without_asking_anyone() {
        let control = bypass();
        let (inner, asked) = person(deny(), 0);
        let r = ModeResolver::new(inner, control.clone(), false);
        assert_eq!(
            r.answer(&paused("shell", "shell", &["curl", "x"])).await,
            UserAnswer::Allow
        );
        assert_eq!(asked.load(Ordering::SeqCst), 0);
        assert!(
            control.events_since(0).is_empty(),
            "a grant is recorded when the chain settles the call, not before"
        );
        control.settle_call("w1", "c1", None);
        assert!(control.events_since(0).iter().any(|(_, e)| matches!(
            e,
            kgraf_tools::PermissionEvent::Granted { subject, .. } if subject == "`curl`"
        )));
    }

    /// A hard ask reaches the person even in Bypass, and a run nobody attends
    /// denies it without asking.
    #[tokio::test]
    async fn a_hard_ask_goes_to_the_person_in_bypass_and_is_denied_headless() {
        let (inner, asked) = person(deny(), 0);
        let attended = ModeResolver::new(inner.clone(), bypass(), true);
        let hard = paused("shell", PERMISSION_RULES_HOOK, &["git", "push"]);
        assert_eq!(attended.answer(&hard).await, deny());
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        let headless = ModeResolver::new(inner, bypass(), false);
        let UserAnswer::Deny { reason } = headless.answer(&hard).await else {
            panic!("a headless hard ask must be denied");
        };
        assert!(reason.contains("Nobody attends"), "{reason}");
        assert_eq!(asked.load(Ordering::SeqCst), 1, "nobody was asked headless");
    }

    #[tokio::test]
    async fn manual_asks_the_person_and_auto_grants_only_its_allow_set() {
        let control = Arc::new(PermissionControl::manual());
        let (inner, asked) = person(deny(), 0);
        let r = ModeResolver::new(inner, control.clone(), true);
        // A program that runs no code of its own: an interpreter given no
        // code reads its input, which no grant covers (decision 0094,
        // amended 2026-10-06).
        assert_eq!(
            r.answer(&paused("shell", "shell", &["jq", "."])).await,
            deny()
        );
        control.set_allow(AllowSet::default().with_programs(["jq"]).expect("allow"));
        control
            .set_mode(PermissionMode::Auto, ModeSource::UserInterface)
            .expect("auto");
        assert_eq!(
            r.answer(&paused("shell", "shell", &["jq", "."])).await,
            UserAnswer::Allow
        );
        assert_eq!(r.answer(&paused("shell", "shell", &["curl"])).await, deny());
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    /// A mode change reaches an ask already waiting: switching to a mode that
    /// grants it releases it before the person answers.
    #[tokio::test]
    async fn a_mode_change_releases_a_waiting_ask() {
        let control = Arc::new(PermissionControl::manual());
        let (inner, asked) = person(deny(), 60_000);
        let r = Arc::new(ModeResolver::new(inner, control.clone(), true));
        let waiting = {
            let r = r.clone();
            tokio::spawn(async move { r.answer(&paused("mcp__web__fetch", "mcp", &["x"])).await })
        };
        while asked.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        control
            .set_mode(PermissionMode::AcceptEdits, ModeSource::UserInterface)
            .expect("mode");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !waiting.is_finished(),
            "Accept edits does not grant an MCP call"
        );
        control
            .set_mode(
                PermissionMode::OrchestratorDecides,
                ModeSource::UserInterface,
            )
            .expect("mode");
        let answer = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("released before the person answered")
            .expect("join");
        assert_eq!(answer, UserAnswer::Allow);
    }
}
