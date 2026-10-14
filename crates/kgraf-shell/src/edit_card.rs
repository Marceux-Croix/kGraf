//! `/edit` and `/edit --new` in the orchestrator box (decision 0100, section
//! 7): the run's effective instruction (the original and every applied
//! change, as one text) opens in the composer; on send, the lines that
//! differ are the change, applied to the run's plan with one re-plan call.
//! `/edit` reopens the run on its branch; `/edit --new` starts a new run
//! from a copy of its plan.
//!
//! The target is the run attached to this window, else the newest stored
//! run, and the box names it, as `/status` does. An edit that changes no
//! line sends nothing. A send the backend refuses (the run is busy, a land
//! is pending) keeps the text in the composer, so the edit is not lost.
//! Any other `/` command while an edit is open cancels it, saying so.
//!
//! Opening makes no call and reads nothing but what the backend already
//! holds: the open step of decision 0100's B11 budget is the box's own work
//! (`opening_makes_no_call_and_fills_the_composer` measures its structure).

use crate::orchestrator_box::{BoxBackend, BoxCommand, BoxModel, Line};

/// The run an edit applies to, with its instruction as it stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditTarget {
    pub run: String,
    /// The original instruction and every applied change, as one text.
    pub instruction: String,
    /// The run attached to this window (else the newest stored one).
    pub attached: bool,
}

/// An edit to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditRequest {
    pub run: String,
    /// The text the edit was made from.
    pub opened: String,
    /// The edited instruction, whole.
    pub edited: String,
    /// `/edit --new`: a new run copied from `run`.
    pub new: bool,
}

/// An edit open in the composer.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Draft {
    run: String,
    opened: String,
    new: bool,
}

/// The box's edit state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditCard {
    draft: Option<Draft>,
}

impl EditCard {
    /// The run an open edit applies to.
    pub fn editing(&self) -> Option<&str> {
        self.draft.as_ref().map(|d| d.run.as_str())
    }
}

/// `/edit` or `/edit --new`.
pub fn parse(line: &str, args: &[&str]) -> BoxCommand {
    match args {
        [] => BoxCommand::Edit { new: false },
        ["--new"] => BoxCommand::Edit { new: true },
        _ => BoxCommand::Unknown(line.to_owned()),
    }
}

/// Open the target's instruction in the composer.
pub fn open(model: &mut BoxModel, backend: &mut dyn BoxBackend, new: bool) {
    let Some(target) = backend.edit_target() else {
        model.say(Line::Error(
            "/edit: there is no run to edit: none is attached to this window and the project \
             store holds no run record"
                .to_owned(),
        ));
        return;
    };
    let which = if target.attached {
        "the run in this window"
    } else {
        "the newest stored run"
    };
    let then = if new {
        "on send it starts a new run from a copy of its plan, with your edit as one change"
    } else {
        "on send the lines you changed are one change to its plan, and the run reopens on its \
         branch"
    };
    model.say(Line::Notice(format!(
        "/edit: the instruction of run {} ({which}) is in the composer; {then}. Any other / \
         command cancels the edit.",
        target.run
    )));
    model.input = target.instruction.clone();
    model.edit.draft = Some(Draft {
        run: target.run,
        opened: target.instruction,
        new,
    });
}

/// With an edit open, take the composer's text as the edit. Returns whether
/// the input was taken (an edit was open). `input` is the text submitted.
pub fn take_input(model: &mut BoxModel, backend: &mut dyn BoxBackend, input: &str) -> bool {
    let Some(draft) = model.edit.draft.clone() else {
        return false;
    };
    let line = input.trim();
    if line.starts_with('/') && !line.starts_with("//") {
        model.edit.draft = None;
        model.say(Line::Notice(format!(
            "the edit of run {} was cancelled; nothing was sent",
            draft.run
        )));
        return false;
    }
    let Some(diff) = kgraf_orchestrator::delta::line_diff(&draft.opened, input) else {
        model.edit.draft = None;
        model.say(Line::Notice(format!(
            "the edit of run {} changes no line, so nothing was sent",
            draft.run
        )));
        return true;
    };
    let changed = diff.lines().count();
    model.say(Line::You(format!(
        "(the instruction of run {}, {changed} line(s) changed)",
        draft.run
    )));
    let request = EditRequest {
        run: draft.run.clone(),
        opened: draft.opened.clone(),
        edited: input.to_owned(),
        new: draft.new,
    };
    match backend.edit(request) {
        Ok(()) => {
            model.edit.draft = None;
            model.say(Line::Notice(if draft.new {
                format!("starting a new run from a copy of run {}'s plan", draft.run)
            } else {
                format!("applying the edit to run {} as one change", draft.run)
            }));
        }
        Err(e) => {
            // The edit is not lost: it stays in the composer, still open.
            model.input = input.to_owned();
            model.say(Line::Error(format!(
                "the edit of run {} was not sent: {e}; it is still in the composer",
                draft.run
            )));
        }
    }
    true
}

/// The user's answer to the change card `card`: `true` applies the change,
/// `false` refuses it. The answer names the card's change, so a card replaced
/// meanwhile is not answered.
pub fn answer_change(
    model: &mut BoxModel,
    backend: &mut dyn BoxBackend,
    card: &kgraf_orchestrator::ChangeView,
    yes: bool,
) {
    match backend.answer_change(card.number, yes) {
        Ok(()) => model.say(Line::Notice(match (yes, card.stops_run) {
            (true, _) => format!("change {} applies; the run goes on with it", card.number),
            (false, true) => format!(
                "change {} is refused; the run stops before it dispatches anything",
                card.number
            ),
            (false, false) => format!(
                "change {} is refused; the plan stays as it was shown",
                card.number
            ),
        })),
        Err(e) => model.say(Line::Error(format!("the change was not answered: {e}"))),
    }
}

/// The card's title and lines: the change, its kind, the mode that asks, and
/// what it does to the plan.
pub fn change_card_text(card: &kgraf_orchestrator::ChangeView) -> (String, Vec<String>) {
    (
        format!(
            "Apply change {}? It is a {} change, and your permission mode ({}) asks first",
            card.number, card.class, card.mode
        ),
        card.ops.clone(),
    )
}

/// The change card (decision 0100), in the box's pinned cards: Apply or
/// Refuse. Draws nothing when no change waits.
pub fn show_change(
    ui: &mut egui::Ui,
    model: &mut BoxModel,
    backend: &mut dyn BoxBackend,
    p: &crate::theme::Palette,
) {
    let Some(card) = backend.pending_change() else {
        return;
    };
    use egui::RichText;
    let (title, ops) = change_card_text(&card);
    let mut answer = None;
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
            ui.label(
                RichText::new(title.as_str())
                    .font(crate::theme::medium(13.0))
                    .color(p.text),
            );
            for op in &ops {
                ui.label(RichText::new(op.as_str()).color(p.text));
            }
            ui.horizontal_wrapped(|ui| {
                // CMP-BTN-2: applying is the asked-for choice; CMP-BTN-4: verbs.
                if crate::chrome::filled_button(ui, p, "Apply change").clicked() {
                    answer = Some(true);
                } else if crate::chrome::tonal_button(ui, p, "Refuse change").clicked() {
                    answer = Some(false);
                }
            });
        });
    if let Some(yes) = answer {
        answer_change(model, backend, &card, yes);
    }
    ui.add_space(8.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator_box::{BackendEvent, BackendState};
    use kgraf_orchestrator::{AskAnswer, InboundKind, PendingAsk};

    /// A backend with one stored run, that records every call.
    #[derive(Default)]
    struct Fake {
        target: Option<EditTarget>,
        sent: Vec<EditRequest>,
        refuse: Option<String>,
        calls: Vec<&'static str>,
        change: Option<kgraf_orchestrator::ChangeView>,
        answered: Vec<bool>,
    }

    impl BoxBackend for Fake {
        fn state(&self) -> BackendState {
            BackendState::Idle {
                can_start: true,
                reason: String::new(),
            }
        }
        fn send(&mut self, _kind: InboundKind, _text: &str) -> Result<String, String> {
            self.calls.push("send");
            Err("no run".into())
        }
        fn start(&mut self, _instruction: &str) -> Result<(), String> {
            self.calls.push("start");
            Ok(())
        }
        fn compact(&mut self, _target: Option<String>) -> Result<(), String> {
            Ok(())
        }
        fn pending_asks(&self) -> Vec<PendingAsk> {
            Vec::new()
        }
        fn answer(&mut self, _id: u64, _answer: AskAnswer) -> Result<(), String> {
            Ok(())
        }
        fn poll(&mut self) -> Vec<BackendEvent> {
            Vec::new()
        }
        fn status_from_record(&self) -> Option<String> {
            None
        }
        fn edit_target(&self) -> Option<EditTarget> {
            self.target.clone()
        }
        fn pending_change(&self) -> Option<kgraf_orchestrator::ChangeView> {
            self.change.clone()
        }
        fn answer_change(&mut self, number: u32, yes: bool) -> Result<(), String> {
            let shown = self.change.take().ok_or("no change")?;
            if shown.number != number {
                return Err("another change".to_owned());
            }
            self.answered.push(yes);
            Ok(())
        }
        fn edit(&mut self, request: EditRequest) -> Result<(), String> {
            self.calls.push("edit");
            if let Some(e) = &self.refuse {
                return Err(e.clone());
            }
            self.sent.push(request);
            Ok(())
        }
    }

    fn fake() -> Fake {
        Fake {
            target: Some(EditTarget {
                run: "r7".into(),
                instruction: "build the parser\nwith tests".into(),
                attached: false,
            }),
            ..Fake::default()
        }
    }

    fn submit(model: &mut BoxModel, backend: &mut Fake, text: &str) {
        model.input = text.to_owned();
        model.submit(backend, None);
    }

    /// B11's open step: `/edit` fills the composer with the target's
    /// instruction and names the run, with no call to the backend beyond
    /// reading what it holds.
    #[test]
    fn opening_makes_no_call_and_fills_the_composer() {
        let mut model = BoxModel::default();
        let mut backend = fake();
        submit(&mut model, &mut backend, "/edit");
        assert_eq!(model.input, "build the parser\nwith tests");
        assert!(backend.calls.is_empty(), "{:?}", backend.calls);
        assert_eq!(model.edit.editing(), Some("r7"));
        assert!(
            matches!(model.lines().last(), Some(Line::Notice(n)) if n.contains("run r7") && n.contains("newest stored run"))
        );
    }

    #[test]
    fn an_edit_sends_the_whole_text_and_a_new_run_copies() {
        let mut model = BoxModel::default();
        let mut backend = fake();
        submit(&mut model, &mut backend, "/edit");
        submit(
            &mut model,
            &mut backend,
            "build the parser\nwith tests and docs",
        );
        assert_eq!(backend.sent.len(), 1);
        let sent = &backend.sent[0];
        assert_eq!(
            (sent.run.as_str(), sent.new),
            ("r7", false),
            "the in-place edit targets r7"
        );
        assert_eq!(sent.edited, "build the parser\nwith tests and docs");
        assert_eq!(sent.opened, "build the parser\nwith tests");
        assert_eq!(model.edit.editing(), None);
        submit(&mut model, &mut backend, "/edit --new");
        submit(&mut model, &mut backend, "build the lexer");
        assert!(backend.sent[1].new);
    }

    /// Acceptance 11: an edit that changes no line makes no call.
    #[test]
    fn an_unchanged_edit_sends_nothing() {
        let mut model = BoxModel::default();
        let mut backend = fake();
        submit(&mut model, &mut backend, "/edit");
        let text = model.input.clone();
        submit(&mut model, &mut backend, &format!("{text}  \n"));
        assert!(backend.calls.is_empty(), "{:?}", backend.calls);
        assert!(
            matches!(model.lines().last(), Some(Line::Notice(n)) if n.contains("changes no line"))
        );
    }

    /// Acceptance 5: a refused send keeps the composer text and the edit
    /// open.
    #[test]
    fn a_refused_edit_stays_in_the_composer() {
        let mut model = BoxModel::default();
        let mut backend = fake();
        backend.refuse = Some("run `r7` is held by another process".into());
        submit(&mut model, &mut backend, "/edit");
        submit(&mut model, &mut backend, "build the parser fast");
        assert_eq!(model.input, "build the parser fast");
        assert_eq!(model.edit.editing(), Some("r7"));
        assert!(
            matches!(model.lines().last(), Some(Line::Error(e)) if e.contains("held by another"))
        );
    }

    /// The change card (decision 0100): its text names the change, its kind
    /// and the mode that asks, with what it does; an answer reaches the
    /// backend and is said in the box.
    #[test]
    fn the_change_card_names_the_change_and_carries_the_answer() {
        let card = kgraf_orchestrator::ChangeView {
            run: "r7".into(),
            number: 2,
            class: "cutting".into(),
            mode: "manual".into(),
            change: "write d instead".into(),
            ops: vec!["replace c with d2.c".into()],
            stops_run: true,
        };
        let (title, ops) = change_card_text(&card);
        assert!(
            title.contains("change 2") && title.contains("cutting") && title.contains("manual")
        );
        assert_eq!(ops, vec!["replace c with d2.c".to_owned()]);
        let mut model = BoxModel::default();
        let mut backend = Fake {
            change: Some(card.clone()),
            ..Fake::default()
        };
        answer_change(&mut model, &mut backend, &card, false);
        assert_eq!(backend.answered, vec![false]);
        assert!(matches!(model.lines().last(),
            Some(Line::Notice(n)) if n.contains("refused") && n.contains("the run stops")));
        answer_change(&mut model, &mut backend, &card, true);
        assert!(
            matches!(model.lines().last(), Some(Line::Error(_))),
            "nothing waits now"
        );
        // Plan review's change: a refusal leaves the shown plan, and the run
        // is not said to stop.
        let review = kgraf_orchestrator::ChangeView {
            stops_run: false,
            ..card.clone()
        };
        backend.change = Some(review.clone());
        answer_change(&mut model, &mut backend, &review, false);
        assert!(matches!(model.lines().last(),
            Some(Line::Notice(n)) if n.contains("plan stays") && !n.contains("stops")));
    }

    #[test]
    fn another_command_cancels_the_edit_and_no_target_is_said() {
        let mut model = BoxModel::default();
        let mut backend = fake();
        submit(&mut model, &mut backend, "/edit");
        submit(&mut model, &mut backend, "/help");
        assert_eq!(model.edit.editing(), None);
        let mut none = Fake::default();
        submit(&mut model, &mut none, "/edit");
        assert!(
            matches!(model.lines().last(), Some(Line::Error(e)) if e.contains("no run to edit"))
        );
        assert_eq!(
            parse("/edit x", &["x"]),
            BoxCommand::Unknown("/edit x".into())
        );
    }
}
