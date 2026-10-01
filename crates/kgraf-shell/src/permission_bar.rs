//! The permission controls and what waits on them, inside the orchestrator box
//! (decision 0094): the permission-mode dropdown and the plan-review switch
//! together above the box's cards, the grouped permission requests, the plan
//! waiting for the user's yes, and the one-time Bypass warning. Nothing here is
//! a new panel: the box is where the user answers the orchestrator, and the plan
//! lights what it touches on the graph ("no second place to look").
//!
//! The model ([`PermissionBar`] and the free functions) is pure over a
//! [`BoxBackend`] and a [`PermissionUi`], so its transitions are tested without
//! a window; the drawing functions only call it.
//!
//! Only the user sets the mode. The dropdown is a user surface
//! ([`ModeSource::UserInterface`]); each change is written to the user-level
//! permissions file, never to a project file.

use std::path::PathBuf;
use std::sync::Arc;

use kgraf_orchestrator::{AskAnswer, PendingAsk, PermissionRequestView, PlanAnswer, PlanView};
use kgraf_tools::permissions::{
    BYPASS_WARNING, ModeError, ModeSource, PermissionControl, PermissionMode, PermissionSettings,
    ProcessFacts, Raised, group_requests,
};

use crate::orchestrator_box::{BoxBackend, Line};

/// The live control and where the user's changes are written.
#[derive(Clone, Debug)]
pub struct PermissionUi {
    pub control: Arc<PermissionControl>,
    /// The user-level permissions file. `None`: changes hold for this process
    /// only, and the bar says so.
    pub file: Option<PathBuf>,
}

impl PermissionUi {
    /// The user-level file at `file` as a live control. A file that cannot be
    /// read, or whose mode this process may not start in (Bypass as root, or
    /// disabled), is not used: the control is Manual, and the notice says why.
    pub fn load(file: Option<PathBuf>) -> (Self, Option<String>) {
        let loaded = match &file {
            Some(path) => PermissionSettings::load(path, None).map_err(|e| e.to_string()),
            None => Ok(PermissionSettings::default()),
        };
        let (control, notice) = match loaded {
            Ok(settings) => {
                match PermissionControl::new(
                    settings.clone(),
                    ModeSource::UserConfig,
                    ProcessFacts::detect(),
                ) {
                    Ok(c) => (c, None),
                    Err(e) => (
                        PermissionControl::new(
                            PermissionSettings {
                                mode: PermissionMode::Manual,
                                ..settings
                            },
                            ModeSource::UserConfig,
                            ProcessFacts::detect(),
                        )
                        .unwrap_or_else(|_| PermissionControl::manual()),
                        Some(format!("Permission mode set to Manual: {e}")),
                    ),
                }
            }
            Err(e) => (
                PermissionControl::manual(),
                Some(format!(
                    "Permission settings could not be read, so Manual applies: {e}"
                )),
            ),
        };
        (
            Self {
                control: Arc::new(control),
                file,
            },
            notice,
        )
    }

    fn persist(&self) -> Result<(), String> {
        match &self.file {
            Some(path) => self
                .control
                .settings()
                .save(path)
                .map_err(|e| e.to_string()),
            None => Err("no user-level permissions file is configured".to_owned()),
        }
    }

    /// A change, then the line that says it, or what kept it from the file.
    fn saved(&self, said: String) -> Line {
        match self.persist() {
            Ok(()) => Line::Notice(said),
            Err(e) => Line::Error(format!("{said}, for this session only: {e}")),
        }
    }
}

/// One permission request in the box: a worker's approval ask, or a request
/// raised by a worker's shortfall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionItem {
    Ask(PendingAsk),
    Request(PermissionRequestView),
}

impl PermissionItem {
    /// Unique across both kinds (their ids come from different counters).
    pub fn key(&self) -> String {
        match self {
            Self::Ask(a) => format!("ask:{}", a.id),
            Self::Request(r) => format!("request:{}", r.id),
        }
    }

    pub fn worker(&self) -> &str {
        match self {
            Self::Ask(a) => &a.worker,
            Self::Request(r) => &r.worker,
        }
    }

    /// What is asked for, in one line.
    pub fn title(&self) -> String {
        match self {
            Self::Ask(a) => format!("{}: {}", a.tool, a.question),
            Self::Request(r) => r.question.clone(),
        }
    }
}

impl Raised for PermissionItem {
    fn raised_at_ms(&self) -> i64 {
        match self {
            Self::Ask(a) => a.raised_at_ms,
            Self::Request(r) => r.raised_at_ms,
        }
    }
}

/// The box's permission requests, grouped: those raised within
/// [`kgraf_tools::permissions::GROUP_WINDOW_MS`] of a group's first request
/// are one group. The dependency question (decision 0087) is a consent for the
/// project, not a worker's request: it stays a card of its own.
pub fn groups(asks: &[PendingAsk], requests: &[PermissionRequestView]) -> Vec<Vec<PermissionItem>> {
    let mut items: Vec<PermissionItem> = asks
        .iter()
        .filter(|a| a.tool != crate::dependency_prompt::FETCH_PROMPT_TOOL)
        .cloned()
        .map(PermissionItem::Ask)
        .collect();
    items.extend(requests.iter().cloned().map(PermissionItem::Request));
    let mut out: Vec<Vec<PermissionItem>> = asks
        .iter()
        .filter(|a| a.tool == crate::dependency_prompt::FETCH_PROMPT_TOOL)
        .cloned()
        .map(|a| vec![PermissionItem::Ask(a)])
        .collect();
    out.extend(group_requests(&items));
    out
}

/// A group's key: its items' keys.
pub fn group_key(group: &[PermissionItem]) -> String {
    group
        .iter()
        .map(PermissionItem::key)
        .collect::<Vec<_>>()
        .join(",")
}

/// Grant or deny one item through the backend.
pub fn answer_item(backend: &mut dyn BoxBackend, item: &PermissionItem, grant: bool) -> Line {
    let what = if grant { "granted" } else { "denied" };
    let result = match item {
        PermissionItem::Ask(a) => backend.answer(
            a.id,
            if grant {
                AskAnswer::AllowOnce
            } else {
                AskAnswer::Deny {
                    reason: "denied in the orchestrator box".to_owned(),
                }
            },
        ),
        PermissionItem::Request(r) => backend.answer_permission(r.id, grant),
    };
    match result {
        Ok(()) => Line::Notice(format!("{what}: {} ({})", item.title(), item.worker())),
        Err(e) => Line::Error(format!("the answer was not taken: {e}")),
    }
}

/// Paths the graph should light for the plan: the focused contract's writes,
/// or everything the plan writes.
pub fn plan_highlight(plan: &PlanView, focus: Option<&str>) -> Vec<String> {
    match focus.and_then(|c| plan.items.iter().find(|i| i.contract == c)) {
        Some(item) => item.writes.clone(),
        None => plan.touched_paths(),
    }
}

/// The box's permission state.
#[derive(Debug, Default)]
pub struct PermissionBar {
    /// The one-time Bypass warning is shown, waiting for Turn on or Cancel.
    pub bypass_warning: bool,
    /// A group's all-at-once answer, waiting for Confirm (CMP-CARD-3).
    pub confirming_group: Option<(String, bool)>,
    /// The plan's Change note, while it is being written.
    pub plan_note: Option<String>,
    /// Reject chosen, waiting for Confirm (CMP-CARD-3).
    pub confirming_reject: bool,
    /// The plan contract whose writes the graph lights; `None`: all of them.
    pub plan_focus: Option<String>,
    /// The highlight to apply on the next frame, taken by the window.
    highlight: Option<Vec<String>>,
    /// The highlight last applied.
    shown: Option<Vec<String>>,
}

impl PermissionBar {
    /// The user chose `mode` in the dropdown.
    pub fn choose_mode(&mut self, ui: &PermissionUi, mode: PermissionMode) -> Line {
        let from = ui.control.mode();
        if from == mode {
            return Line::Notice(format!("The permission mode is already {}.", mode.label()));
        }
        match ui.control.set_mode(mode, ModeSource::UserInterface) {
            Ok(_) => {
                self.bypass_warning = false;
                ui.saved(format!(
                    "Permission mode: {} ({}). It applies at once, to requests already waiting too",
                    mode.label(),
                    mode.summary()
                ))
            }
            Err(ModeError::BypassWarningNotAccepted) => {
                self.bypass_warning = true;
                Line::Notice("Read the Bypass warning, then turn it on or cancel.".to_owned())
            }
            Err(e) => Line::Error(format!("The permission mode was not changed: {e}")),
        }
    }

    /// The user accepted the Bypass warning: recorded, then Bypass is set.
    pub fn accept_bypass(&mut self, ui: &PermissionUi) -> Line {
        self.bypass_warning = false;
        ui.control.accept_bypass_warning();
        self.choose_mode(ui, PermissionMode::Bypass)
    }

    /// The user declined the Bypass warning: nothing changes.
    pub fn decline_bypass(&mut self) -> Line {
        self.bypass_warning = false;
        Line::Notice("Bypass was not turned on; the permission mode is unchanged.".to_owned())
    }

    /// The plan-review switch.
    pub fn set_plan_review(&mut self, ui: &PermissionUi, on: bool) -> Line {
        ui.control.set_plan_review(on, ModeSource::UserInterface);
        ui.saved(if on {
            "Plan review on: the next plan waits for your yes before any worker starts".to_owned()
        } else {
            "Plan review off: plans are dispatched without waiting, and a plan waiting now is \
             approved"
                .to_owned()
        })
    }

    /// Answer every item of `group` at once (after Confirm).
    pub fn answer_group(
        &mut self,
        backend: &mut dyn BoxBackend,
        group: &[PermissionItem],
        grant: bool,
    ) -> Vec<Line> {
        self.confirming_group = None;
        group
            .iter()
            .map(|item| answer_item(backend, item, grant))
            .collect()
    }

    /// Answer the plan.
    pub fn answer_plan(&mut self, backend: &mut dyn BoxBackend, answer: PlanAnswer) -> Line {
        let said = match &answer {
            PlanAnswer::Approve => "Plan approved: workers start now".to_owned(),
            PlanAnswer::Reject { .. } => "Plan rejected: nothing was dispatched".to_owned(),
            PlanAnswer::Change { .. } => {
                "Change sent to the planner: a new plan will be shown".to_owned()
            }
        };
        match backend.answer_plan(answer) {
            Ok(()) => {
                self.plan_note = None;
                self.confirming_reject = false;
                self.plan_focus = None;
                Line::Notice(said)
            }
            Err(e) => Line::Error(format!("the plan answer was not taken: {e}")),
        }
    }

    /// Recompute what the graph should light, from the plan waiting now.
    pub fn update_highlight(&mut self, plan: Option<&PlanView>) {
        let want = plan.map(|p| plan_highlight(p, self.plan_focus.as_deref()));
        if want != self.shown {
            // Clearing a highlight this bar set is a change too (an empty set).
            self.highlight = Some(want.clone().unwrap_or_default());
            self.shown = want;
        }
    }

    /// The paths to light on the graph, once per change (an empty set clears).
    pub fn take_highlight(&mut self) -> Option<Vec<String>> {
        self.highlight.take()
    }
}

/// The graph slots of `paths`: each file's code nodes.
pub fn slots_for_paths(model: &kgraf_galaxy::ProjectModel, paths: &[String]) -> Vec<u32> {
    if paths.is_empty() {
        return Vec::new();
    }
    let wanted: std::collections::HashSet<&str> = paths.iter().map(String::as_str).collect();
    model
        .code_nodes()
        .filter(|(_, _, p, _, _)| wanted.contains(p))
        .map(|(slot, ..)| slot)
        .collect()
}

// ------------------------------------------------------------------ drawing

fn card<R>(
    ui: &mut egui::Ui,
    p: &crate::theme::Palette,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    // CMP-CARD-1: additive fill, radius 12, padding 10 14 12.
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
            add(ui)
        })
        .inner
}

/// The permission-mode dropdown and the plan-review switch, together, above
/// the box's cards.
pub fn draw_bar(
    ui: &mut egui::Ui,
    bar: &mut PermissionBar,
    perm: Option<&PermissionUi>,
    p: &crate::theme::Palette,
    lines: &mut Vec<Line>,
) {
    use egui::RichText;
    let Some(perm) = perm else {
        return;
    };
    let settings = perm.control.settings();
    ui.horizontal(|ui| {
        ui.label(crate::icons::text(crate::icons::SHIELD, 16.0).color(p.text_2));
        ui.label(
            RichText::new("Permissions")
                .font(crate::theme::medium(13.0))
                .color(p.text),
        );
        let current = settings.mode;
        let mut chosen = None;
        // CMP-MENU-1/2: the menu opens below its trigger; each item keeps a
        // check column, and only the current mode shows its check.
        egui::ComboBox::from_id_salt("permission_mode")
            .selected_text(current.label())
            .width(190.0)
            .show_ui(ui, |ui| {
                for mode in PermissionMode::ALL {
                    let blocked = mode == PermissionMode::Bypass
                        && (settings.disable_bypass || !settings.allow_bypass);
                    let check = if mode == current {
                        crate::icons::CHECK.to_string()
                    } else {
                        " ".to_owned()
                    };
                    let resp = ui
                        .add_enabled(
                            !blocked,
                            egui::Button::selectable(
                                mode == current,
                                format!("{check}  {}", mode.label()),
                            ),
                        )
                        .on_hover_text(mode.summary())
                        .on_disabled_hover_text(if settings.disable_bypass {
                            "Bypass is disabled in the user-level permissions file \
                             (disable_bypass = true)."
                        } else {
                            "Turn on Allow Bypass in the settings surface to offer it here."
                        });
                    if resp.clicked() {
                        chosen = Some(mode);
                    }
                }
            });
        if let Some(mode) = chosen {
            lines.push(bar.choose_mode(perm, mode));
        }
        ui.add_space(12.0);
        let mut review = settings.plan_review;
        // CMP-SET-4 names a switch; egui's checkbox is the toggle this shell
        // uses for every on/off setting, so the box matches the settings
        // surface.
        if ui
            .checkbox(&mut review, "Review the plan before workers start")
            .on_hover_text(
                "On: after planning, the plan waits in this box for your yes, a change, or a \
                 rejection. Nothing is dispatched before the yes.",
            )
            .changed()
        {
            lines.push(bar.set_plan_review(perm, review));
        }
    });
    if current_is_bypass(&settings) {
        ui.label(
            RichText::new(
                "Bypass is on: every request is granted without asking, except your deny and \
                 ask rules and deleting system paths.",
            )
            .small()
            .color(p.danger),
        );
    }
    if bar.bypass_warning {
        bypass_warning_card(ui, bar, perm, p, lines);
    }
    ui.add_space(6.0);
}

fn current_is_bypass(s: &PermissionSettings) -> bool {
    s.mode == PermissionMode::Bypass
}

/// The one-time Bypass warning, in place (INT-8: no modal dialog to confirm;
/// CMP-CARD-3: the consequence in the danger color, a primary confirm and a
/// text cancel).
fn bypass_warning_card(
    ui: &mut egui::Ui,
    bar: &mut PermissionBar,
    perm: &PermissionUi,
    p: &crate::theme::Palette,
    lines: &mut Vec<Line>,
) {
    use egui::RichText;
    card(ui, p, |ui| {
        ui.horizontal(|ui| {
            ui.label(crate::icons::text(crate::icons::WARNING, 16.0).color(p.danger));
            ui.label(
                RichText::new("Turn on Bypass?")
                    .font(crate::theme::medium(13.0))
                    .color(p.text),
            );
        });
        ui.label(RichText::new(BYPASS_WARNING).small().color(p.danger));
        ui.horizontal(|ui| {
            if crate::chrome::cta_button(ui, p, "Turn on Bypass", true).clicked() {
                lines.push(bar.accept_bypass(perm));
            }
            if crate::chrome::text_button(ui, p.text, "Cancel").clicked() {
                lines.push(bar.decline_bypass());
            }
        });
    });
    ui.add_space(8.0);
}

/// The plan waiting for the user's yes: each contract, what it waits on, what
/// it writes and reads. Hovering a contract lights its writes on the graph;
/// otherwise the whole plan's writes are lit.
pub fn draw_plan(
    ui: &mut egui::Ui,
    bar: &mut PermissionBar,
    backend: &mut dyn BoxBackend,
    plan: &PlanView,
    p: &crate::theme::Palette,
    lines: &mut Vec<Line>,
) {
    use egui::RichText;
    let mut hovered = None;
    card(ui, p, |ui| {
        // CMP-CARD-2: the header row, then 14/20 text, metadata in 12/18.
        ui.horizontal(|ui| {
            ui.label(crate::icons::text(crate::icons::TREE, 16.0).color(p.needs_input));
            ui.label(
                RichText::new(format!(
                    "Plan {} for review: {} contract(s), nothing dispatched yet",
                    plan.revision,
                    plan.items.len()
                ))
                .font(crate::theme::medium(13.0))
                .color(p.text),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new("Pinned by kGraf").small().color(p.text_2));
                ui.label(crate::icons::text(crate::icons::PIN, 14.0).color(p.text_2));
            });
        });
        egui::ScrollArea::vertical()
            .id_salt("plan_items")
            .max_height(220.0)
            .show(ui, |ui| {
                for item in &plan.items {
                    let resp = ui
                        .vertical(|ui| {
                            ui.label(
                                RichText::new(format!("{}: {}", item.contract, item.instruction))
                                    .color(p.text),
                            );
                            let mut meta = Vec::new();
                            if !item.depends_on.is_empty() {
                                meta.push(format!("after {}", item.depends_on.join(", ")));
                            }
                            meta.push(if item.writes.is_empty() {
                                "writes nothing declared".to_owned()
                            } else {
                                format!("writes {}", item.writes.join(", "))
                            });
                            if !item.slices.is_empty() {
                                meta.push(format!("reads {}", item.slices.join(", ")));
                            }
                            if let Some(route) = &item.route {
                                meta.push(format!("on {route}"));
                            }
                            meta.push(format!("accepted when: {}", item.acceptance));
                            ui.label(RichText::new(meta.join("; ")).small().color(p.text_2));
                        })
                        .response;
                    if resp.hovered() {
                        hovered = Some(item.contract.clone());
                    }
                    ui.add_space(4.0);
                }
            });
        if let Some(mut note) = bar.plan_note.take() {
            ui.add(
                egui::TextEdit::multiline(&mut note)
                    .hint_text("What should the planner change?")
                    .desired_rows(2)
                    .desired_width(f32::INFINITY),
            );
            let (mut send, mut cancel) = (false, false);
            ui.horizontal(|ui| {
                let ready = !note.trim().is_empty();
                send = crate::chrome::cta_button(ui, p, "Send change", ready).clicked();
                cancel = crate::chrome::text_button(ui, p.text, "Cancel").clicked();
            });
            if send {
                let text = note.trim().to_owned();
                bar.plan_note = Some(note);
                lines.push(bar.answer_plan(backend, PlanAnswer::Change { note: text }));
            } else if !cancel {
                bar.plan_note = Some(note);
            }
        } else if bar.confirming_reject {
            // CMP-CARD-3: the consequence, then Confirm and Cancel.
            ui.horizontal(|ui| {
                ui.label(crate::icons::text(crate::icons::WARNING, 16.0).color(p.danger));
                ui.label(
                    RichText::new("Nothing is dispatched, and the run ends blocked.")
                        .small()
                        .color(p.danger),
                );
            });
            ui.horizontal(|ui| {
                if crate::chrome::cta_button(ui, p, "Confirm", true).clicked() {
                    lines.push(bar.answer_plan(
                        backend,
                        PlanAnswer::Reject {
                            reason: String::new(),
                        },
                    ));
                }
                if crate::chrome::text_button(ui, p.text, "Cancel").clicked() {
                    bar.confirming_reject = false;
                }
            });
        } else {
            ui.horizontal_wrapped(|ui| {
                // CMP-BTN-2: the recommended choice is filled, the rest tonal;
                // CMP-BTN-4: verb labels.
                if crate::chrome::filled_button(ui, p, "Approve plan").clicked() {
                    lines.push(bar.answer_plan(backend, PlanAnswer::Approve));
                } else if crate::chrome::tonal_button(ui, p, "Change plan").clicked() {
                    bar.plan_note = Some(String::new());
                } else if crate::chrome::tonal_button(ui, p, "Reject plan").clicked() {
                    bar.confirming_reject = true;
                }
            });
        }
    });
    bar.plan_focus = hovered;
    ui.add_space(8.0);
}

/// A group of permission requests raised close together: each item with its
/// own Grant and Deny, and Grant all and Deny all, which confirm first
/// (CMP-CARD-3). A group of one is the same card with one row.
pub fn draw_group(
    ui: &mut egui::Ui,
    bar: &mut PermissionBar,
    backend: &mut dyn BoxBackend,
    group: &[PermissionItem],
    p: &crate::theme::Palette,
    lines: &mut Vec<Line>,
) {
    use egui::RichText;
    let key = group_key(group);
    card(ui, p, |ui| {
        ui.horizontal(|ui| {
            ui.label(crate::icons::text(crate::icons::SHIELD, 16.0).color(p.needs_input));
            ui.label(
                RichText::new(if group.len() == 1 {
                    "Permission request".to_owned()
                } else {
                    format!("{} permission requests, raised together", group.len())
                })
                .font(crate::theme::medium(13.0))
                .color(p.text),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new("Pinned by kGraf").small().color(p.text_2));
                ui.label(crate::icons::text(crate::icons::PIN, 14.0).color(p.text_2));
            });
        });
        for item in group {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(item.title()).color(p.text));
                ui.label(
                    RichText::new(format!("({})", item.worker()))
                        .small()
                        .color(p.text_2),
                );
                if crate::chrome::tonal_button(ui, p, "Grant").clicked() {
                    lines.push(answer_item(backend, item, true));
                }
                if crate::chrome::tonal_button(ui, p, "Deny").clicked() {
                    lines.push(answer_item(backend, item, false));
                }
            });
        }
        if group.len() > 1 {
            match bar.confirming_group.clone().filter(|(k, _)| *k == key) {
                None => {
                    ui.horizontal(|ui| {
                        // CMP-BTN-2: denying is the safe choice, so it is filled.
                        if crate::chrome::filled_button(ui, p, "Deny all").clicked() {
                            bar.confirming_group = Some((key.clone(), false));
                        }
                        if crate::chrome::tonal_button(ui, p, "Grant all").clicked() {
                            bar.confirming_group = Some((key.clone(), true));
                        }
                    });
                }
                Some((_, grant)) => {
                    ui.horizontal(|ui| {
                        ui.label(crate::icons::text(crate::icons::WARNING, 16.0).color(p.danger));
                        ui.label(
                            RichText::new(format!(
                                "{} all {} requests at once.",
                                if grant { "Grants" } else { "Denies" },
                                group.len()
                            ))
                            .small()
                            .color(p.danger),
                        );
                    });
                    ui.horizontal(|ui| {
                        if crate::chrome::cta_button(ui, p, "Confirm", true).clicked() {
                            lines.extend(bar.answer_group(backend, group, grant));
                        }
                        if crate::chrome::text_button(ui, p.text, "Cancel").clicked() {
                            bar.confirming_group = None;
                        }
                    });
                }
            }
        }
    });
    ui.add_space(8.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator_box::tests::Recorder;
    use kgraf_orchestrator::{PlanItemView, RequestScope};

    fn ui_at(dir: &std::path::Path, settings: PermissionSettings) -> PermissionUi {
        PermissionUi {
            control: Arc::new(
                PermissionControl::new(settings, ModeSource::UserConfig, ProcessFacts::default())
                    .expect("control"),
            ),
            file: Some(dir.join("permissions.toml")),
        }
    }

    fn reread(ui: &PermissionUi) -> PermissionSettings {
        PermissionSettings::load(ui.file.as_ref().expect("file"), None).expect("load")
    }

    #[test]
    fn a_mode_chosen_applies_at_once_and_is_written_to_the_user_file() {
        let dir = tempfile::tempdir().expect("dir");
        let ui = ui_at(dir.path(), PermissionSettings::default());
        let mut bar = PermissionBar::default();
        let line = bar.choose_mode(&ui, PermissionMode::OrchestratorDecides);
        assert!(matches!(line, Line::Notice(_)), "{line:?}");
        assert_eq!(ui.control.mode(), PermissionMode::OrchestratorDecides);
        assert_eq!(reread(&ui).mode, PermissionMode::OrchestratorDecides);
        let last = ui.control.events_since(0).pop().expect("event").1;
        assert!(matches!(
            last,
            kgraf_tools::PermissionEvent::ModeChanged {
                source: ModeSource::UserInterface,
                ..
            }
        ));
    }

    /// Bypass from the dropdown: refused until Allow Bypass is on; then the
    /// warning opens and the mode does not change until it is accepted;
    /// declining leaves the mode as it was.
    #[test]
    fn bypass_needs_allow_bypass_and_the_warning_accepted() {
        let dir = tempfile::tempdir().expect("dir");
        let ui = ui_at(dir.path(), PermissionSettings::default());
        let mut bar = PermissionBar::default();
        let line = bar.choose_mode(&ui, PermissionMode::Bypass);
        assert!(
            matches!(&line, Line::Error(t) if t.contains("Allow Bypass")),
            "{line:?}"
        );
        assert_eq!(ui.control.mode(), PermissionMode::Manual);

        ui.control.set_allow_bypass(true);
        bar.choose_mode(&ui, PermissionMode::Bypass);
        assert!(bar.bypass_warning);
        assert_eq!(ui.control.mode(), PermissionMode::Manual);
        bar.decline_bypass();
        assert!(!bar.bypass_warning);
        assert_eq!(ui.control.mode(), PermissionMode::Manual);

        bar.choose_mode(&ui, PermissionMode::Bypass);
        let line = bar.accept_bypass(&ui);
        assert!(matches!(line, Line::Notice(_)), "{line:?}");
        assert_eq!(ui.control.mode(), PermissionMode::Bypass);
        let saved = reread(&ui);
        assert!(saved.bypass_warning_accepted && saved.mode == PermissionMode::Bypass);
        // Accepted once: a later choice needs no warning.
        bar.choose_mode(&ui, PermissionMode::Manual);
        bar.choose_mode(&ui, PermissionMode::Bypass);
        assert!(!bar.bypass_warning);
        assert_eq!(ui.control.mode(), PermissionMode::Bypass);
    }

    #[test]
    fn a_disabled_bypass_is_refused_from_the_dropdown() {
        let dir = tempfile::tempdir().expect("dir");
        let ui = ui_at(
            dir.path(),
            PermissionSettings {
                disable_bypass: true,
                allow_bypass: true,
                bypass_warning_accepted: true,
                ..PermissionSettings::default()
            },
        );
        let line = PermissionBar::default().choose_mode(&ui, PermissionMode::Bypass);
        assert!(
            matches!(&line, Line::Error(t) if t.contains("disabled")),
            "{line:?}"
        );
        assert_eq!(ui.control.mode(), PermissionMode::Manual);
    }

    #[test]
    fn the_plan_review_switch_changes_the_control_and_the_file() {
        let dir = tempfile::tempdir().expect("dir");
        let ui = ui_at(dir.path(), PermissionSettings::default());
        PermissionBar::default().set_plan_review(&ui, true);
        assert!(ui.control.plan_review());
        assert!(reread(&ui).plan_review);
    }

    fn ask(id: u64, at: i64) -> PendingAsk {
        PendingAsk {
            id,
            worker: format!("w{id}"),
            task: "t".into(),
            tool: "shell".into(),
            question: "run it?".into(),
            asked_by: "shell".into(),
            input: "{}".into(),
            raised_at_ms: at,
        }
    }

    fn request(id: u64, at: i64) -> PermissionRequestView {
        PermissionRequestView {
            id,
            run: "r".into(),
            contract: "c".into(),
            worker: format!("v{id}"),
            scope: RequestScope::Command,
            subject: "python3".into(),
            why: "x".into(),
            question: "grant python3?".into(),
            raised_at_ms: at,
        }
    }

    #[test]
    fn asks_and_requests_raised_together_are_one_group_and_the_fetch_question_stays_alone() {
        let mut fetch = ask(99, 100);
        fetch.tool = crate::dependency_prompt::FETCH_PROMPT_TOOL.into();
        let g = groups(
            &[ask(1, 0), ask(2, 1_500), fetch, ask(3, 9_000)],
            &[request(7, 1_900)],
        );
        let shapes: Vec<Vec<String>> = g
            .iter()
            .map(|g| g.iter().map(PermissionItem::key).collect())
            .collect();
        assert_eq!(
            shapes,
            vec![
                vec!["ask:99".to_owned()],
                vec!["ask:1".into(), "ask:2".into(), "request:7".into()],
                vec!["ask:3".into()],
            ]
        );
    }

    #[test]
    fn granting_a_group_answers_every_item_through_its_own_path() {
        let mut backend = Recorder::default();
        let group = vec![
            PermissionItem::Ask(ask(1, 0)),
            PermissionItem::Request(request(7, 10)),
        ];
        let mut bar = PermissionBar {
            confirming_group: Some((group_key(&group), true)),
            ..PermissionBar::default()
        };
        let lines = bar.answer_group(&mut backend, &group, true);
        assert_eq!(lines.len(), 2);
        assert_eq!(backend.answered, vec![(1, AskAnswer::AllowOnce)]);
        assert_eq!(backend.permission_answers, vec![(7, true)]);
        assert!(bar.confirming_group.is_none());
        bar.answer_group(&mut backend, &group, false);
        assert!(matches!(backend.answered[1].1, AskAnswer::Deny { .. }));
        assert_eq!(backend.permission_answers[1], (7, false));
    }

    fn plan() -> PlanView {
        let item = |c: &str, writes: &[&str]| PlanItemView {
            contract: c.into(),
            instruction: "do".into(),
            acceptance: "done".into(),
            depends_on: vec![],
            writes: writes.iter().map(|w| (*w).to_owned()).collect(),
            slices: vec![],
            route: None,
        };
        PlanView {
            run: "r".into(),
            revision: 1,
            items: vec![
                item("a", &["src/a.rs"]),
                item("b", &["src/b.rs", "src/a.rs"]),
            ],
        }
    }

    #[test]
    fn the_graph_lights_the_plan_and_then_the_focused_contract_and_clears_after() {
        let mut bar = PermissionBar::default();
        let plan = plan();
        bar.update_highlight(Some(&plan));
        assert_eq!(
            bar.take_highlight(),
            Some(vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()])
        );
        bar.update_highlight(Some(&plan));
        assert_eq!(bar.take_highlight(), None, "unchanged: not applied again");
        bar.plan_focus = Some("a".into());
        bar.update_highlight(Some(&plan));
        assert_eq!(bar.take_highlight(), Some(vec!["src/a.rs".to_owned()]));
        bar.update_highlight(None);
        assert_eq!(bar.take_highlight(), Some(vec![]), "cleared once");
        bar.update_highlight(None);
        assert_eq!(bar.take_highlight(), None);
    }

    #[test]
    fn a_plan_answer_goes_to_the_backend_and_closes_the_change_note() {
        let mut backend = Recorder::default();
        let mut bar = PermissionBar {
            plan_note: Some("use b".into()),
            ..PermissionBar::default()
        };
        bar.answer_plan(
            &mut backend,
            PlanAnswer::Change {
                note: "use b".into(),
            },
        );
        assert_eq!(
            backend.plan_answers,
            vec![PlanAnswer::Change {
                note: "use b".into()
            }]
        );
        assert!(bar.plan_note.is_none());
    }

    #[test]
    fn a_file_whose_mode_cannot_start_falls_back_to_manual_and_says_so() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("permissions.toml");
        std::fs::write(&path, "mode = \"bypass\"\ndisable_bypass = true").expect("write");
        let (ui, notice) = PermissionUi::load(Some(path));
        assert_eq!(ui.control.mode(), PermissionMode::Manual);
        assert!(notice.expect("notice").contains("Manual"));
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "mdoe = 1").expect("write");
        let (ui, notice) = PermissionUi::load(Some(bad));
        assert_eq!(ui.control.mode(), PermissionMode::Manual);
        assert!(notice.is_some());
    }
}
