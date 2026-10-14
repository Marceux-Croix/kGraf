//! Landing in the orchestrator box (decision 0100, section 5): the `/land`
//! command, the land suggestion card (Land, Not now), and the untested-land
//! warning card. The plan root node's land action calls [`root_node_land`],
//! the same path as `/land`.
//!
//! Nothing lands without the user's explicit yes, in every permission mode:
//! `/land` typed by the user is that yes; a suggestion lands only on Land
//! and its confirm step (CMP-CARD-3); an untested land needs its own second
//! yes after the warning. The suggestion rules run on facts the backend
//! reads from the run record and the repository, at most once a second
//! (no model call), and the one-open and "Not now" rules are
//! `kgraf_orchestrator::land_suggest`'s.

use kgraf_orchestrator::land_flow::{LandOutcome, LandRequest, UntestedBinding};
use kgraf_orchestrator::land_suggest::{SuggestAnswer, Suggester, Suggestion};

use crate::orchestrator_box::{BoxBackend, BoxCommand, BoxModel, Line};

/// How often the suggestion rules are evaluated.
pub const CHECK_EVERY_MS: i64 = 500;

/// Milliseconds since the epoch, the clock the suggestion rules read.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The box's landing state.
#[derive(Debug, Default)]
pub struct LandCard {
    suggester: Suggester,
    /// A land waiting for a second yes (untested, or changed programs):
    /// the request that yes sends, bound to the candidate and target the
    /// question named, the card's title and its words.
    untested: Option<(LandRequest, String)>,
    question_title: String,
    /// What the open suggestion would land (`<sha> (<n> commits) into <branch>`).
    suggest_what: Option<String>,
    /// The chosen answer on a card, waiting for Confirm (CMP-CARD-3).
    confirming: Option<Choice>,
    last_check_ms: i64,
    /// The input as last seen, and when it last changed: no suggestion
    /// within five seconds of typing.
    last_input: String,
    last_typed_ms: Option<i64>,
    /// The request of the land in flight, kept for an untested yes.
    pending_request: Option<LandRequest>,
}

/// A card choice waiting for Confirm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Suggestion(SuggestAnswer),
    LandUntested,
}

impl LandCard {
    /// The open suggestion, if any.
    pub fn suggestion(&self) -> Option<&Suggestion> {
        self.suggester.open()
    }

    /// The untested-land warning waiting for a yes, if any.
    pub fn untested_warning(&self) -> Option<&str> {
        self.untested.as_ref().map(|(_, w)| w.as_str())
    }
}

/// `/land` and `/land into <branch>`.
pub fn parse(line: &str, args: &[&str]) -> BoxCommand {
    match args {
        [] => BoxCommand::Land(None),
        ["into", branch] => BoxCommand::Land(Some((*branch).to_owned())),
        _ => BoxCommand::Unknown(line.to_owned()),
    }
}

/// The user typed `/land`: their explicit yes.
pub fn submit(model: &mut BoxModel, backend: &mut dyn BoxBackend, into: Option<String>) {
    let request = LandRequest {
        into,
        attended: true,
        ..LandRequest::default()
    };
    send(model, backend, request);
}

/// `<short sha> (<n> commits) into <branch>`, as every card names it.
pub fn what_lands(candidate: &str, commits: usize, target: &str) -> String {
    format!(
        "{} ({commits} commit{}) into {target}",
        candidate.get(..12).unwrap_or(candidate),
        if commits == 1 { "" } else { "s" }
    )
}

/// The plan root node's land action: the same land as `/land`.
pub fn root_node_land(model: &mut BoxModel, backend: &mut dyn BoxBackend) {
    submit(model, backend, None);
}

fn send(model: &mut BoxModel, backend: &mut dyn BoxBackend, request: LandRequest) {
    model.land.suggester.withdraw();
    match backend.land(request.clone()) {
        Ok(()) => {
            model.land.untested = None;
            model.say(Line::Notice("landing...".to_owned()));
        }
        Err(e) => model.say(Line::Error(format!("not landed: {e}"))),
    }
    // A synchronous backend answers at once; keep the request for the
    // untested yes.
    model.land.pending_request = Some(request);
}

/// What the backend answered to a land.
pub fn report(model: &mut BoxModel, outcome: Result<LandOutcome, String>) {
    let request = model.land.pending_request.take().unwrap_or_default();
    match outcome {
        Ok(LandOutcome::Landed { entry, notices }) => {
            for n in notices {
                model.say(Line::Notice(n));
            }
            model.say(Line::Notice(format!(
                "landed into {} as merge commit {}",
                entry.target, entry.merge_commit
            )));
        }
        Ok(LandOutcome::NothingToLand { target, .. }) => model.say(Line::Notice(format!(
            "nothing to land: it is already on {target}"
        ))),
        Ok(LandOutcome::Conflict { target, paths }) => model.say(Line::Error(format!(
            "not landed: the run does not merge cleanly into {target} ({}); a resolver must \
             merge {target} into the run's branch first",
            paths.join(", ")
        ))),
        Ok(LandOutcome::Remediating { contract, target }) => model.say(Line::Notice(format!(
            "the gate failed on the merge with {target}; remediation {contract} runs, then \
             land again"
        ))),
        Ok(LandOutcome::NeedsUntestedYes {
            gate,
            reason,
            candidate,
            target,
            target_tip,
            commits,
        }) => {
            let what = what_lands(&candidate, commits, &target);
            let warning = format!(
                "Root gate {gate} did not pass on what would land: {reason}. Landing {what} \
                 now lands untested code, marked UNTESTED in the merge commit."
            );
            model.say(Line::Error(warning.clone()));
            // The yes is for this candidate, target and target tip only.
            model.land.untested = Some((
                LandRequest {
                    candidate: Some(candidate.clone()),
                    into: Some(target.clone()),
                    untested_confirmed: true,
                    untested_for: Some(UntestedBinding {
                        candidate,
                        target,
                        target_tip,
                    }),
                    ..request
                },
                warning,
            ));
            model.land.question_title = "Land untested?".to_owned();
        }
        Ok(LandOutcome::NeedsProgramsYes {
            changes,
            candidate,
            target,
            commits,
        }) => {
            let what = what_lands(&candidate, commits, &target);
            let warning = format!(
                "Landing {what} runs your git's hooks, filters or signing programs, and these \
                 changed since the run started (a worker could have changed them): {}.",
                changes.join("; ")
            );
            model.say(Line::Error(warning.clone()));
            model.land.untested = Some((
                LandRequest {
                    candidate: Some(candidate),
                    into: Some(target),
                    programs_confirmed: changes,
                    ..request
                },
                warning,
            ));
            model.land.question_title = "Run the changed programs?".to_owned();
        }
        Err(e) => model.say(Line::Error(format!("not landed: {e}"))),
    }
}

/// The user confirmed the untested land.
pub fn confirm_untested(model: &mut BoxModel, backend: &mut dyn BoxBackend) {
    if let Some((request, _)) = model.land.untested.take() {
        send(model, backend, request);
    }
}

/// Evaluate the suggestion rules (at most every [`CHECK_EVERY_MS`]).
/// Returns a newly raised suggestion.
pub fn tick(model: &mut BoxModel, backend: &mut dyn BoxBackend, now_ms: i64) -> Option<Suggestion> {
    if model.input != model.land.last_input {
        model.land.last_input = model.input.clone();
        model.land.last_typed_ms = Some(now_ms);
    }
    if now_ms - model.land.last_check_ms < CHECK_EVERY_MS {
        return None;
    }
    model.land.last_check_ms = now_ms;
    let (book, mut facts) = backend.land_facts(now_ms)?;
    // The facts may be a refresh old; the clock and the typing are now's.
    facts.now_ms = now_ms;
    facts.last_typed_ms = model.land.last_typed_ms;
    let raised = model.land.suggester.evaluate(&book, &facts)?;
    let what = what_lands(
        facts.candidate.as_deref().unwrap_or_default(),
        facts.commits,
        facts.target.as_deref().unwrap_or_default(),
    );
    model.say(Line::Notice(format!(
        "kGraf suggests landing {what}: {}",
        raised.rule.reason()
    )));
    model.land.suggest_what = Some(what);
    Some(raised)
}

/// The user answered the suggestion (after its confirm step). Land only on
/// Land; "Not now" is recorded in the run record.
pub fn answer(
    model: &mut BoxModel,
    backend: &mut dyn BoxBackend,
    answer: SuggestAnswer,
    now_ms: i64,
) {
    let mode = backend
        .permissions()
        .map(|p| p.control.mode())
        .unwrap_or_default();
    let verified = backend
        .land_facts(now_ms)
        .map(|(_, f)| f.verified)
        .unwrap_or_default();
    let mut scratch = kgraf_orchestrator::land_record::LandBook::default();
    let land = model
        .land
        .suggester
        .answer(&mut scratch, mode, answer, now_ms, verified);
    if land {
        submit(model, backend, None);
        return;
    }
    match backend.decline_land(now_ms) {
        Ok(()) => model.say(Line::Notice(
            "not now: kGraf asks again after new verified work and 15 minutes".to_owned(),
        )),
        Err(e) => model.say(Line::Error(format!("the decline was not recorded: {e}"))),
    }
}

/// The suggestion card and the untested-land card, in the box's pinned
/// cards. Draws nothing when neither is open.
pub fn show(
    ui: &mut egui::Ui,
    model: &mut BoxModel,
    backend: &mut dyn BoxBackend,
    p: &crate::theme::Palette,
    now_ms: i64,
) {
    tick(model, backend, now_ms);
    let warning = model.land.untested.as_ref().map(|(_, w)| w.clone());
    let suggestion = model.land.suggestion().cloned();
    if warning.is_none() && suggestion.is_none() {
        return;
    }
    use egui::RichText;
    egui::Frame::new()
        .fill(p.additive)
        .corner_radius(crate::theme::radius::CARD)
        .inner_margin(egui::Margin {
            left: 14,
            right: 14,
            top: 10,
            bottom: 12,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let (title, body) = match (&warning, &suggestion) {
                (Some(w), _) => (model.land.question_title.clone(), w.clone()),
                (None, Some(s)) => (
                    format!(
                        "Land {}?",
                        model.land.suggest_what.as_deref().unwrap_or("this run")
                    ),
                    s.rule.reason().to_owned(),
                ),
                (None, None) => return,
            };
            ui.label(
                RichText::new(title.as_str())
                    .font(crate::theme::medium(13.0))
                    .color(p.text),
            );
            ui.label(RichText::new(body).color(p.text));
            match model.land.confirming {
                None => {
                    ui.horizontal_wrapped(|ui| {
                        // CMP-BTN-2: for a suggestion, Land is the recommended,
                        // filled choice; for an untested land, Cancel is the
                        // safe one, so landing untested is tonal.
                        if warning.is_some() {
                            if crate::chrome::filled_button(ui, p, "Cancel").clicked() {
                                model.land.untested = None;
                            } else if crate::chrome::tonal_button(ui, p, "Yes, land").clicked() {
                                model.land.confirming = Some(Choice::LandUntested);
                            }
                        } else if crate::chrome::filled_button(ui, p, "Land").clicked() {
                            model.land.confirming = Some(Choice::Suggestion(SuggestAnswer::Land));
                        } else if crate::chrome::tonal_button(ui, p, "Not now").clicked() {
                            model.land.confirming = Some(Choice::Suggestion(SuggestAnswer::NotNow));
                        }
                    });
                }
                Some(choice) => {
                    let consequence = match choice {
                        Choice::Suggestion(SuggestAnswer::Land) => {
                            "Your branch gets one merge commit with the run's gated work."
                        }
                        Choice::Suggestion(SuggestAnswer::NotNow) => {
                            "No suggestion until new verified work and 15 minutes."
                        }
                        Choice::LandUntested => {
                            "Your branch moves, as the warning above describes."
                        }
                    };
                    ui.horizontal(|ui| {
                        ui.label(crate::icons::text(crate::icons::WARNING, 16.0).color(p.danger));
                        ui.label(RichText::new(consequence).small().color(p.danger));
                    });
                    ui.horizontal(|ui| {
                        if crate::chrome::cta_button(ui, p, "Confirm", true).clicked() {
                            model.land.confirming = None;
                            match choice {
                                Choice::Suggestion(a) => answer(model, backend, a, now_ms),
                                Choice::LandUntested => confirm_untested(model, backend),
                            }
                        }
                        if crate::chrome::text_button(ui, p.text, "Cancel").clicked() {
                            model.land.confirming = None;
                        }
                    });
                }
            }
        });
    ui.add_space(8.0);
}

/// The app host's land work, off the UI thread: a land with no live loop
/// and the suggestion facts both read the repository and the store, so each
/// runs on the host's runtime and arrives at the next poll.
pub struct LandBackground {
    tx: std::sync::mpsc::Sender<LandMsg>,
    rx: std::sync::mpsc::Receiver<LandMsg>,
    facts: Option<(
        kgraf_orchestrator::land_record::LandBook,
        kgraf_orchestrator::land_suggest::SuggestFacts,
    )>,
    refreshing: bool,
    last_refresh_ms: i64,
}

enum LandMsg {
    Facts(
        Box<
            Option<(
                kgraf_orchestrator::land_record::LandBook,
                kgraf_orchestrator::land_suggest::SuggestFacts,
            )>,
        >,
    ),
    Outcome(Box<Result<LandOutcome, String>>),
    Note(Result<String, String>),
}

impl Default for LandBackground {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx,
            rx,
            facts: None,
            refreshing: false,
            last_refresh_ms: 0,
        }
    }
}

impl LandBackground {
    /// The newest facts, and a refresh started when they are older than
    /// [`CHECK_EVERY_MS`] and none is in flight.
    pub fn facts(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        root: std::path::PathBuf,
        run: kgraf_core::RunId,
        now_ms: i64,
    ) -> Option<(
        kgraf_orchestrator::land_record::LandBook,
        kgraf_orchestrator::land_suggest::SuggestFacts,
    )> {
        self.drain_facts();
        if !self.refreshing && now_ms - self.last_refresh_ms >= CHECK_EVERY_MS {
            self.refreshing = true;
            self.last_refresh_ms = now_ms;
            let tx = self.tx.clone();
            runtime.spawn_blocking(move || {
                // The app never runs benchmarks.
                let facts = kgraf_host::land::project_suggest_facts(&root, &run, false, now_ms)
                    .ok()
                    .flatten()
                    .map(|(record, facts)| (record.land, facts));
                let _ = tx.send(LandMsg::Facts(Box::new(facts)));
            });
        }
        self.facts.clone()
    }

    /// Land with no live loop; the outcome arrives at [`Self::drain`].
    pub fn land(
        &self,
        runtime: &tokio::runtime::Runtime,
        root: std::path::PathBuf,
        run: kgraf_core::RunId,
        request: LandRequest,
    ) {
        let tx = self.tx.clone();
        runtime.spawn_blocking(move || {
            let outcome = kgraf_host::land::land_project_run(&root, &run, &request)
                .map_err(|e| e.to_string());
            let _ = tx.send(LandMsg::Outcome(Box::new(outcome)));
        });
    }

    /// Record "Not now" with no live loop.
    pub fn decline(
        &self,
        runtime: &tokio::runtime::Runtime,
        root: std::path::PathBuf,
        run: kgraf_core::RunId,
        now_ms: i64,
    ) {
        let tx = self.tx.clone();
        runtime.spawn_blocking(move || {
            let done = kgraf_host::land::record_land_decline(&root, &run, now_ms)
                .map(|()| "the decline is recorded".to_owned())
                .map_err(|e| format!("the decline was not recorded: {e}"));
            let _ = tx.send(LandMsg::Note(done));
        });
    }

    fn drain_facts(&mut self) {
        let mut rest = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            match m {
                LandMsg::Facts(f) => {
                    self.facts = *f;
                    self.refreshing = false;
                }
                other => rest.push(other),
            }
        }
        for m in rest {
            let _ = self.tx.send(m);
        }
    }

    /// What arrived for the box.
    pub fn drain(&mut self) -> Vec<crate::orchestrator_box::BackendEvent> {
        use crate::orchestrator_box::BackendEvent;
        let mut out = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            match m {
                LandMsg::Facts(f) => {
                    self.facts = *f;
                    self.refreshing = false;
                }
                LandMsg::Outcome(o) => out.push(BackendEvent::Land(o)),
                LandMsg::Note(Ok(n)) => out.push(BackendEvent::Notice(n)),
                LandMsg::Note(Err(e)) => out.push(BackendEvent::Error(e)),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator_box::{BackendEvent, BackendState};
    use kgraf_orchestrator::land_record::LandBook;
    use kgraf_orchestrator::land_suggest::SuggestFacts;
    use kgraf_orchestrator::{AskAnswer, InboundKind, PendingAsk};

    #[derive(Default)]
    struct Fake {
        landed: Vec<LandRequest>,
        declined: Vec<i64>,
        facts: Option<(LandBook, SuggestFacts)>,
    }

    impl BoxBackend for Fake {
        fn state(&self) -> BackendState {
            BackendState::Running { run: "r1".into() }
        }
        fn send(&mut self, _k: InboundKind, _t: &str) -> Result<String, String> {
            Ok("1".into())
        }
        fn start(&mut self, _i: &str) -> Result<(), String> {
            Ok(())
        }
        fn compact(&mut self, _t: Option<String>) -> Result<(), String> {
            Ok(())
        }
        fn pending_asks(&self) -> Vec<PendingAsk> {
            Vec::new()
        }
        fn answer(&mut self, _id: u64, _a: AskAnswer) -> Result<(), String> {
            Ok(())
        }
        fn poll(&mut self) -> Vec<BackendEvent> {
            Vec::new()
        }
        fn status_from_record(&self) -> Option<String> {
            None
        }
        fn land(&mut self, request: LandRequest) -> Result<(), String> {
            self.landed.push(request);
            Ok(())
        }
        fn land_facts(&mut self, _now: i64) -> Option<(LandBook, SuggestFacts)> {
            self.facts.clone()
        }
        fn decline_land(&mut self, now_ms: i64) -> Result<(), String> {
            self.declined.push(now_ms);
            if let Some((book, _)) = &mut self.facts {
                book.land_declined_at = Some(now_ms);
                book.land_decline_count += 1;
                book.verified_at_decline = Some(3);
            }
            Ok(())
        }
    }

    fn stopping_point() -> (LandBook, SuggestFacts) {
        (
            LandBook::default(),
            SuggestFacts {
                landable: true,
                stopping_point: true,
                verified: 3,
                ..SuggestFacts::default()
            },
        )
    }

    #[test]
    fn slash_land_parses_with_and_without_a_target() {
        assert_eq!(
            crate::orchestrator_box::parse("/land"),
            BoxCommand::Land(None)
        );
        assert_eq!(
            crate::orchestrator_box::parse("/land into release"),
            BoxCommand::Land(Some("release".into()))
        );
        assert_eq!(
            crate::orchestrator_box::parse("/land now"),
            BoxCommand::Unknown("/land now".into())
        );
    }

    #[test]
    fn slash_land_is_the_users_yes_and_lands_once() {
        let mut model = BoxModel::default();
        let mut backend = Fake::default();
        model.input = "/land".into();
        model.submit(&mut backend, None);
        assert_eq!(backend.landed.len(), 1);
        assert!(backend.landed[0].attended && !backend.landed[0].untested_confirmed);
    }

    #[test]
    fn an_untested_land_needs_a_second_yes() {
        let mut model = BoxModel::default();
        let mut backend = Fake::default();
        submit(&mut model, &mut backend, None);
        report(
            &mut model,
            Ok(LandOutcome::NeedsUntestedYes {
                gate: "gate:root".into(),
                reason: "never ran".into(),
                candidate: "abc".into(),
                target: "main".into(),
                target_tip: "t0".into(),
                commits: 2,
            }),
        );
        let warning = model.land.untested_warning().expect("warned");
        assert!(warning.contains("abc (2 commits) into main"), "{warning}");
        assert_eq!(backend.landed.len(), 1, "the warning lands nothing");
        confirm_untested(&mut model, &mut backend);
        assert_eq!(backend.landed.len(), 2);
        let yes = &backend.landed[1];
        assert!(yes.untested_confirmed);
        assert_eq!(
            yes.candidate.as_deref(),
            Some("abc"),
            "bound to the candidate"
        );
        assert_eq!(yes.into.as_deref(), Some("main"), "bound to the target");
        assert_eq!(
            yes.untested_for.as_ref().map(|b| b.target_tip.as_str()),
            Some("t0"),
            "bound to the target tip"
        );
    }

    /// A suggestion is shown, never acted on by itself, and "Not now"
    /// suppresses the next one.
    #[test]
    fn a_suggestion_is_shown_and_not_now_suppresses_it() {
        let mut model = BoxModel::default();
        let mut backend = Fake {
            facts: Some(stopping_point()),
            ..Fake::default()
        };
        let raised = tick(&mut model, &mut backend, 10_000).expect("raised");
        assert_eq!(
            raised.rule,
            kgraf_orchestrator::land_suggest::SuggestRule::StoppingPoint
        );
        assert!(
            backend.landed.is_empty(),
            "a suggestion never lands by itself"
        );
        assert!(tick(&mut model, &mut backend, 20_000).is_none(), "one open");
        answer(&mut model, &mut backend, SuggestAnswer::NotNow, 20_000);
        assert_eq!(backend.declined, [20_000]);
        assert!(backend.landed.is_empty());
        assert!(
            tick(&mut model, &mut backend, 30_000).is_none(),
            "suppressed after Not now"
        );
    }

    #[test]
    fn accepting_a_suggestion_lands() {
        let mut model = BoxModel::default();
        let mut backend = Fake {
            facts: Some(stopping_point()),
            ..Fake::default()
        };
        tick(&mut model, &mut backend, 10_000).expect("raised");
        answer(&mut model, &mut backend, SuggestAnswer::Land, 10_001);
        assert_eq!(backend.landed.len(), 1);
    }

    #[test]
    fn no_suggestion_right_after_typing() {
        let mut model = BoxModel::default();
        let mut backend = Fake {
            facts: Some(stopping_point()),
            ..Fake::default()
        };
        model.input = "hel".into();
        assert!(tick(&mut model, &mut backend, 10_000).is_none());
        assert!(tick(&mut model, &mut backend, 16_000).is_some());
    }
}
