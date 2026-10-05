//! The permission controls and what waits on them, inside the orchestrator box
//! (decision 0094): the permission-mode dropdown and the plan-review switch
//! together above the box's cards, the grouped permission requests, the plan
//! waiting for the user's yes, and the one-time Bypass warning. Nothing here is
//! a new panel: the box is where the user answers the orchestrator, and the plan
//! lights what it touches on the graph ("no second place to look").
//!
//! The settings dialog's Permissions tab draws its parts from here too: what
//! reading the file found ([`draw_load_result`]), the rule editor
//! ([`draw_rule_editor`]) and the script-construct switches
//! ([`draw_script_constructs`]). The mode itself stays in the box.
//!
//! The model ([`PermissionBar`] and the free functions) is pure over a
//! [`BoxBackend`] and a [`PermissionUi`], so its transitions are tested without
//! a window; the drawing functions only call it.
//!
//! Only the user sets the mode. The dropdown is a user surface
//! ([`ModeSource::UserInterface`]); each change is written to the user-level
//! permissions file, never to a project file.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use kgraf_orchestrator::{AskAnswer, PendingAsk, PermissionRequestView, PlanAnswer, PlanView};
use kgraf_tools::permission_edit::{self, RuleKind};
use kgraf_tools::permissions::{
    BYPASS_WARNING, ModeError, ModeSource, PermissionControl, PermissionMode, PermissionSettings,
    ProcessFacts, Raised, SCRIPT_CONSTRUCTS_RISK, ScriptConstruct, SettingsLoad, group_requests,
    write_settings_file,
};

use crate::orchestrator_box::{BoxBackend, Line};
use crate::settings_rows as row;

/// The live control and where the user's changes are written.
#[derive(Clone, Debug)]
pub struct PermissionUi {
    pub control: Arc<PermissionControl>,
    /// The user-level permissions file. `None`: changes hold for this process
    /// only, and the bar says so.
    pub file: Option<PathBuf>,
    /// What reading the file found: the valid entries, each refused entry
    /// with its reason, the notices, and why Manual is forced, if it is.
    /// Shared by every clone (the box of each project, the settings dialog),
    /// and replaced when the rule editor reads the file again
    /// ([`PermissionUi::load_result`]).
    loaded: Arc<RwLock<Arc<SettingsLoad>>>,
}

impl PermissionUi {
    /// The user-level file at `file` as a live control, read entry by entry
    /// ([`PermissionSettings::load_entries`]): every valid entry loads, and
    /// each refused entry is listed with its reason. A refused deny or ask
    /// entry forces Manual, since a block or ask the user wrote is missing; a
    /// refused allow entry keeps the user's mode. A file that is not a
    /// permissions file at all (bad TOML, an unknown key), or whose mode this
    /// process may not start in (Bypass as root, or disabled), gives Manual.
    /// The returned line, if any, is for the log; Settings shows the same.
    pub fn load(file: Option<PathBuf>) -> (Self, Option<String>) {
        let mut load = match &file {
            Some(path) => PermissionSettings::load_entries(path, None)
                .unwrap_or_else(|e| SettingsLoad::unreadable(&e.to_string())),
            None => SettingsLoad::default(),
        };
        let settings = load.effective();
        let control = match PermissionControl::new(
            settings.clone(),
            ModeSource::UserConfig,
            ProcessFacts::detect(),
        ) {
            Ok(c) => c,
            Err(e) => {
                // Not `forced_manual`: the user may still choose another
                // mode this process can start in.
                load.notices
                    .insert(0, format!("Permission mode set to Manual: {e}"));
                PermissionControl::new(
                    PermissionSettings {
                        mode: PermissionMode::Manual,
                        ..settings
                    },
                    ModeSource::UserConfig,
                    ProcessFacts::detect(),
                )
                .unwrap_or_else(|_| PermissionControl::manual())
            }
        };
        let notice = load.summary();
        (
            Self {
                control: Arc::new(control),
                file,
                loaded: Arc::new(RwLock::new(Arc::new(load))),
            },
            notice,
        )
    }

    /// A live control with no file read: `settings` as they are, nothing
    /// refused.
    pub fn with_control(control: Arc<PermissionControl>, file: Option<PathBuf>) -> Self {
        let settings = control.settings();
        Self {
            control,
            file,
            loaded: Arc::new(RwLock::new(Arc::new(SettingsLoad {
                settings,
                ..SettingsLoad::default()
            }))),
        }
    }

    /// What the last read of the file found.
    pub fn load_result(&self) -> Arc<SettingsLoad> {
        self.loaded
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn set_load_result(&self, load: SettingsLoad) {
        *self.loaded.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(load);
    }

    /// Write the live settings to the user-level file, keeping each refused
    /// entry as written (the user must still fix it, and a change made here
    /// must not delete it) and, while Manual is forced, the mode the user
    /// chose ([`SettingsLoad::file_text`]).
    fn persist(&self) -> Result<(), String> {
        match &self.file {
            Some(path) => {
                let text = self.load_result().file_text(&self.control.settings())?;
                write_settings_file(path, &text).map_err(|e| e.to_string())
            }
            None => Err("no user-level permissions file is configured".to_owned()),
        }
    }

    /// Persist after a change made outside this module (the Allow Bypass
    /// switch in Settings): the same rules as every other change.
    pub fn save(&self) -> Result<(), String> {
        self.persist()
    }

    /// A change, then the line that says it, or what kept it from the file.
    fn saved(&self, said: String) -> Line {
        match self.persist() {
            Ok(()) => Line::Notice(said),
            Err(e) => Line::Error(format!("{said}, for this session only: {e}")),
        }
    }
}

/// One script-construct switch changed in the settings surface (decision
/// 0094, amended 2026-10-02): the live control first, so the next call any
/// worker makes is judged with it, then the user-level file.
pub fn set_script_construct(ui: &PermissionUi, construct: ScriptConstruct, on: bool) -> Line {
    ui.control.set_script_construct(construct, on);
    ui.saved(format!(
        "{}: {}. It applies to the next shell call of every worker",
        construct.label(),
        if on {
            "on, so a mode that grants programs may grant a script using it"
        } else {
            "off, so a script using it is asked about"
        }
    ))
}

/// The script-construct switches, for the settings surface's Permissions
/// tab: the risk the user accepts as the section's description, then one
/// switch per construct with its plain explanation below (CMP-SET-1, through
/// [`crate::settings_rows`]). Each is off until the user turns it on.
/// Returns the lines of any change.
pub fn draw_script_constructs(
    ui: &mut egui::Ui,
    perm: &PermissionUi,
    p: &crate::theme::Palette,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let on = perm.control.script_constructs();
    row::heading(ui, p, "Shell scripts a mode may grant");
    row::description(ui, p, SCRIPT_CONSTRUCTS_RISK);
    for construct in ScriptConstruct::ALL {
        let mut checked = on.allows(construct);
        ui.add_space(8.0);
        if row::toggle(
            ui,
            p,
            construct.label(),
            construct.explanation(),
            &mut checked,
        ) {
            lines.push(set_script_construct(perm, construct, checked));
        }
    }
    lines
}

/// The lists the editor shows (decision 0094): every list of the file, the
/// rules first, then Auto's allow set.
pub const EDITOR_KINDS: [RuleKind; 5] = RuleKind::ALL;

/// The rule lists, under one heading: blocks and asks in every mode.
pub const RULE_KINDS: [RuleKind; 2] = [RuleKind::Deny, RuleKind::Ask];

/// The allow set, under one heading: what Auto grants without asking.
pub const ALLOW_KINDS: [RuleKind; 3] = [
    RuleKind::AllowProgram,
    RuleKind::AllowTool,
    RuleKind::AllowPath,
];

type Lists = Vec<(RuleKind, Vec<String>)>;

/// What the rule editor keeps between frames: the file's lists as last read,
/// each field's draft text, and the line the last change produced.
#[derive(Clone, Debug, Default)]
struct RuleEditorState {
    /// `None` until read, and again after a change, so the next frame reads.
    lists: Option<Result<Lists, String>>,
    drafts: [String; 5],
    said: Option<Line>,
}

/// Add `text` to the list of `kind` in the user file, then read the file
/// again and apply it to the live control, so the next call any worker makes
/// is judged with it. The entry is checked first with the loader's own check
/// ([`permission_edit::validate`], which calls
/// [`kgraf_tools::permissions::validate_entry`]); an invalid one returns the
/// loader's message and writes nothing. An entry that loads with a notice is
/// added, and the notice is said with it.
pub fn add_rule(ui: &PermissionUi, kind: RuleKind, text: &str) -> Result<Line, String> {
    let checked = permission_edit::validate(kind, text)?;
    let path = ui
        .file
        .as_ref()
        .ok_or("no user-level permissions file is configured")?;
    if !path.exists() {
        // Write the current settings first so the new file also holds them.
        ui.persist()?;
    }
    permission_edit::add_to_file(path, kind, &checked.text)?;
    let mut said = format!("Added {} `{}`", kind_noun(kind), checked.text);
    if let Some(notice) = &checked.notice {
        said = format!("{said}. {}", as_sentence(notice).trim_end_matches('.'));
    }
    reload_from_file(ui, said)
}

/// Remove `entry` from the list of `kind` in the user file, then read the
/// file again and apply it.
pub fn remove_rule(ui: &PermissionUi, kind: RuleKind, entry: &str) -> Result<Line, String> {
    let path = ui
        .file
        .as_ref()
        .ok_or("no user-level permissions file is configured")?;
    permission_edit::remove_from_file(path, kind, entry)?;
    reload_from_file(ui, format!("Removed {} `{entry}`", kind_noun(kind)))
}

fn kind_noun(kind: RuleKind) -> &'static str {
    match kind {
        RuleKind::Deny => "deny rule",
        RuleKind::Ask => "ask rule",
        RuleKind::AllowProgram => "allowed program",
        RuleKind::AllowTool => "allowed tool",
        RuleKind::AllowPath => "allowed path",
    }
}

/// Read the file the edit just wrote entry by entry, as the window reads it
/// at start ([`PermissionSettings::load_entries`]), and apply it: the valid
/// rules and allow set go to the live control, and the load result is
/// replaced, so the refused entries and notices Settings lists are current.
/// The plan-review switch, Allow Bypass and the script-construct switches
/// are left as they are: the editor changes only the lists.
///
/// A refused deny or ask entry forces Manual, as at start. When the file no
/// longer has one, the forced Manual is lifted and the file's own mode
/// applies, as it would at the next start; a mode this process cannot start
/// in stays Manual with the reason as a notice.
fn reload_from_file(ui: &PermissionUi, said: String) -> Result<Line, String> {
    let path = ui
        .file
        .as_ref()
        .ok_or("no user-level permissions file is configured")?;
    let mut load = PermissionSettings::load_entries(path, None).map_err(|e| {
        format!("{said}, but the file is not a permissions file, so it is not applied: {e}")
    })?;
    ui.control.set_rules(load.settings.rules.clone());
    ui.control.set_allow(load.settings.allow.clone());
    let was_forced = ui.load_result().forced_manual.is_some();
    if load.forced_manual.is_some() {
        if ui.control.mode() != PermissionMode::Manual {
            // Manual can always be set.
            let _ = ui
                .control
                .set_mode(PermissionMode::Manual, ModeSource::UserConfig);
        }
    } else if was_forced
        && load.settings.mode != ui.control.mode()
        && let Err(e) = ui
            .control
            .set_mode(load.settings.mode, ModeSource::UserConfig)
    {
        load.notices
            .insert(0, format!("Permission mode set to Manual: {e}"));
    }
    ui.set_load_result(load);
    Ok(Line::Notice(format!(
        "{said}. It applies to the next call of every worker"
    )))
}

/// One list of the rule editor: its label and description, each entry with a
/// Remove button, then a field with an Add button. A listed entry the loader
/// refuses is drawn in `--danger`, with the reason on hover (COL-7: a broken
/// setting). The field checks its text as the user types: a refused entry
/// shows the loader's message in `--danger`, and an entry that loads but
/// matches no call shows the loader's notice as helper text (CMP-FLD-4).
fn draw_rule_list(
    ui: &mut egui::Ui,
    perm: &PermissionUi,
    p: &crate::theme::Palette,
    kind: RuleKind,
    entries: &[String],
    draft: &mut String,
) -> Option<Result<Line, String>> {
    use egui::RichText;
    let mut change = None;
    ui.push_id(kind, |ui| {
        row::label(ui, p, kind.label());
        row::description(ui, p, kind.explanation());
        ui.add_space(4.0);
        for entry in entries {
            ui.horizontal(|ui| {
                // The row is the button's height, so the entry and its Remove
                // button center on one line.
                ui.set_min_height(crate::theme::size::BUTTON_S);
                let refused = permission_edit::validate(kind, entry).err();
                // TYP-6: an entry is inline code, mono on the additive fill.
                let text = RichText::new(entry)
                    .monospace()
                    .background_color(p.additive)
                    .color(if refused.is_some() { p.danger } else { p.text });
                let shown = ui.label(text);
                if let Some(why) = &refused {
                    shown.on_hover_text(why);
                }
                // TXT-5: the hover says what the button does.
                if crate::chrome::text_button(ui, p.text_2, "Remove")
                    .on_hover_text(format!("Remove {entry} from {}", kind.label()))
                    .clicked()
                {
                    change = Some(remove_rule(perm, kind, entry));
                }
            });
        }
        let mut submit = false;
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(draft)
                    .hint_text(format!("for example {}", kind.example()))
                    .desired_width(ui.available_width() - 64.0),
            );
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            submit = crate::chrome::tonal_button(ui, p, "Add")
                .on_hover_text(format!("Add the entry to {}", kind.label()))
                .clicked()
                || enter;
        });
        let checked = (!draft.trim().is_empty()).then(|| permission_edit::validate(kind, draft));
        match &checked {
            Some(Err(e)) => row::error(ui, p, &as_sentence(e)),
            Some(Ok(c)) => {
                if let Some(notice) = &c.notice {
                    row::helper(ui, p, &as_sentence(notice));
                }
            }
            None => {}
        }
        if submit {
            // An invalid entry is not sent: its message is already under
            // the field.
            match &checked {
                Some(Ok(_)) => {
                    let result = add_rule(perm, kind, draft);
                    if result.is_ok() {
                        draft.clear();
                    }
                    change = Some(result);
                }
                Some(Err(_)) => {}
                // CMP-FLD-4: an empty submit says so instead of doing nothing.
                None => change = Some(Err("Type an entry first".to_owned())),
            }
        }
    });
    change
}

/// The rule editor, in Settings > Permissions: every list of the user file in
/// one form. "Rules" holds deny and ask; "Allowed in Auto" holds the allow
/// set's programs, tools and paths. Each entry is one line with Remove, and
/// each list has one field with Add. Configuration only (CMP-DLG-6). Each add
/// or remove is written to the user file at once, keeping its comments, and
/// the file is read again and applied.
pub fn draw_rule_editor(ui: &mut egui::Ui, perm: &PermissionUi, p: &crate::theme::Palette) {
    let id = ui.make_persistent_id("permission-rule-editor");
    let mut st: RuleEditorState = ui.data_mut(|d| d.get_temp(id)).unwrap_or_default();
    if st.lists.is_none() {
        st.lists = Some(match &perm.file {
            None => Err("no user-level permissions file is configured".to_owned()),
            Some(path) => permission_edit::read_text(path).and_then(|text| {
                EDITOR_KINDS
                    .iter()
                    .map(|k| permission_edit::entries(&text, *k).map(|e| (*k, e)))
                    .collect()
            }),
        });
    }
    let mut change: Option<Result<Line, String>> = None;
    match st.lists.clone().expect("set above") {
        Err(e) => row::error(ui, p, &e),
        Ok(lists) => {
            for (title, about, kinds) in [
                (
                    "Rules",
                    "A rule names a tool and a pattern: shell(git push*), apply_patch(docs/*). \
                     Claude Code names such as Bash(...) are read as kGraf tools; one kGraf \
                     does not have, such as WebFetch, loads with a notice and matches nothing. \
                     A refused deny or ask rule keeps the mode at Manual until you correct it.",
                    &RULE_KINDS[..],
                ),
                (
                    "Allowed in Auto",
                    "What the Auto mode grants without asking. Deny and ask rules still apply.",
                    &ALLOW_KINDS[..],
                ),
            ] {
                row::divider(ui, p);
                row::heading(ui, p, title);
                row::description(ui, p, about);
                for kind in kinds {
                    let n = EDITOR_KINDS
                        .iter()
                        .position(|k| k == kind)
                        .expect("every kind is listed");
                    let entries = lists
                        .iter()
                        .find(|(k, _)| k == kind)
                        .map(|(_, e)| e.as_slice())
                        .unwrap_or_default();
                    ui.add_space(12.0);
                    if let Some(c) = draw_rule_list(ui, perm, p, *kind, entries, &mut st.drafts[n])
                    {
                        change = Some(c);
                    }
                }
            }
        }
    }
    if let Some(result) = change {
        st.said = Some(result.unwrap_or_else(Line::Error));
        st.lists = None;
    }
    match &st.said {
        Some(Line::Notice(t)) => {
            ui.add_space(4.0);
            row::helper(ui, p, t);
        }
        Some(Line::Error(t)) => {
            ui.add_space(4.0);
            row::error(ui, p, t);
        }
        _ => {}
    }
    ui.data_mut(|d| d.insert_temp(id, st));
}

/// What reading `permissions.toml` found, at the top of Settings >
/// Permissions: why Manual is forced (`--danger`, COL-7: a broken setting),
/// each refused entry with its reason, and each notice for an entry that
/// loads but matches no call. Drawn from [`PermissionUi::load_result`], which
/// the rule editor replaces after each save. Nothing when the file loaded
/// clean.
pub fn draw_load_result(ui: &mut egui::Ui, perm: &PermissionUi, p: &crate::theme::Palette) {
    let load = perm.load_result();
    if load.forced_manual.is_none() && load.refused.is_empty() && load.notices.is_empty() {
        return;
    }
    ui.add_space(8.0);
    if let Some(why) = &load.forced_manual {
        row::error(ui, p, why);
    }
    if !load.refused.is_empty() {
        ui.add_space(4.0);
        row::label(ui, p, "Refused entries in permissions.toml");
        for line in &load.refused {
            row::description(ui, p, &line.sentence());
        }
    }
    if !load.notices.is_empty() {
        ui.add_space(4.0);
        row::label(ui, p, "Notices");
        for notice in &load.notices {
            row::description(ui, p, &as_sentence(notice));
        }
    }
}

/// Loader text shown as a sentence in Settings. The loader writes its notices
/// and refusals to follow a `notice` or `error` label on the command line, so
/// they start in lower case; a line of its own in the window starts with a
/// capital (TXT-1).
pub(crate) fn as_sentence(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
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
        if let Some(why) = &ui.load_result().forced_manual {
            return Line::Error(format!("The permission mode stays Manual. {why}"));
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
                let load = perm.load_result();
                let forced = load.forced_manual.as_deref();
                for mode in PermissionMode::ALL {
                    if let Some(why) = forced.filter(|_| mode != PermissionMode::Manual) {
                        ui.add_enabled(
                            false,
                            egui::Button::selectable(false, format!("   {}", mode.label())),
                        )
                        .on_disabled_hover_text(why);
                        continue;
                    }
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
        PermissionUi::with_control(
            Arc::new(
                PermissionControl::new(settings, ModeSource::UserConfig, ProcessFacts::default())
                    .expect("control"),
            ),
            Some(dir.join("permissions.toml")),
        )
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

    /// The script-construct switches draw off by default, and clicking one
    /// reaches the shared control the workers' shells read and the user
    /// file; clicking it again turns it off in both.
    #[test]
    fn the_script_construct_switches_start_off_and_reach_the_shared_control() {
        use egui_kittest::kittest::{NodeT, Queryable};
        let dir = tempfile::tempdir().expect("dir");
        let perm = ui_at(dir.path(), PermissionSettings::default());
        let drawn = perm.clone();
        let mut h = egui_kittest::Harness::new_ui(move |ui| {
            if !crate::theme::ensure(ui.ctx()) {
                return;
            }
            let p = crate::theme::Palette::dark();
            let _ = draw_script_constructs(ui, &drawn, &p);
        });
        h.run();
        for c in ScriptConstruct::ALL {
            let node = h.get_by_label(c.label());
            assert_eq!(
                node.accesskit_node().toggled(),
                Some(egui::accesskit::Toggled::False),
                "{c} starts off"
            );
            h.get_by_label_contains(&c.explanation()[..40]);
        }
        h.get_by_label_contains("kGraf cannot see what a variable or a substitution");
        assert!(perm.control.script_constructs().is_empty());

        h.get_by_label(ScriptConstruct::Variables.label()).click();
        h.run();
        assert!(
            perm.control
                .script_constructs()
                .allows(ScriptConstruct::Variables)
        );
        assert!(
            reread(&perm)
                .script_constructs
                .allows(ScriptConstruct::Variables)
        );
        assert_eq!(
            h.get_by_label(ScriptConstruct::Variables.label())
                .accesskit_node()
                .toggled(),
            Some(egui::accesskit::Toggled::True)
        );
        // The switch reaches what a worker's shell decides.
        perm.control
            .set_mode(PermissionMode::Auto, ModeSource::UserInterface)
            .expect("auto");
        perm.control.set_allow(
            kgraf_tools::permissions::AllowSet::default()
                .with_programs(["mkdir", "ls"])
                .expect("allow"),
        );
        let policy = kgraf_tools::execpolicy::ExecPolicy::default_kgraf_policy()
            .with_permissions(perm.control.clone());
        let script: Vec<String> = ["sh", "-c", "mkdir out && ls $(ls)"]
            .iter()
            .map(|w| (*w).to_owned())
            .collect();
        assert!(policy.evaluate(&script).granted);

        h.get_by_label(ScriptConstruct::Variables.label()).click();
        h.run();
        assert!(perm.control.script_constructs().is_empty());
        assert!(reread(&perm).script_constructs.is_empty());
        assert!(!policy.evaluate(&script).granted);
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
        let (ui, notice) = PermissionUi::load(Some(bad.clone()));
        assert_eq!(ui.control.mode(), PermissionMode::Manual);
        assert!(notice.is_some());
        assert!(ui.control.rules().deny.is_empty());
        assert!(ui.load_result().unreadable && ui.load_result().forced_manual.is_some());
        // A change in the window does not write over a file it could not read.
        let line = PermissionBar::default().set_plan_review(&ui, true);
        assert!(matches!(line, Line::Error(_)), "{line:?}");
        assert_eq!(std::fs::read_to_string(&bad).expect("read"), "mdoe = 1");
    }

    /// The window reads the same rules the command line does: a Claude Code
    /// name is read as the kGraf tool, and a rule on a Claude Code tool kGraf
    /// lacks loads with its notice, refusing nothing and keeping the mode.
    #[test]
    fn a_claude_code_name_loads_and_a_tool_kgraf_lacks_is_a_notice_in_the_window() {
        let dir = tempfile::tempdir().expect("dir");
        let good = dir.path().join("good.toml");
        std::fs::write(&good, "mode = \"auto\"\ndeny = [\"Bash(rm -rf*)\"]").expect("write");
        let (ui, notice) = PermissionUi::load(Some(good));
        assert_eq!(notice, None);
        assert_eq!(ui.control.mode(), PermissionMode::Auto);
        assert!(ui.control.rules().deny[0].names_tool("shell"));
        let web = dir.path().join("web.toml");
        std::fs::write(&web, "mode = \"auto\"\ndeny = [\"WebFetch\"]").expect("write");
        let (ui, notice) = PermissionUi::load(Some(web));
        assert_eq!(ui.control.mode(), PermissionMode::Auto);
        assert!(ui.load_result().refused.is_empty() && ui.load_result().forced_manual.is_none());
        assert_eq!(ui.load_result().notices.len(), 1);
        assert!(
            ui.load_result().notices[0].contains("kGraf has no WebFetch tool"),
            "{:?}",
            ui.load_result().notices
        );
        assert!(notice.expect("logged").contains("shell(curl*)"));
    }

    /// One refused deny entry: the window keeps every other entry, names the
    /// refused one with its reason, and runs in Manual with the reason until
    /// the file is fixed; the dropdown refuses another mode. A change made in
    /// the window keeps the refused entry and the user's mode in the file.
    #[test]
    fn a_bad_deny_entry_keeps_the_other_rules_and_forces_manual_with_the_reason() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("permissions.toml");
        std::fs::write(
            &path,
            "mode = \"auto\"\ndeny = [\"Bah(rm*)\", \"Bash(curl*)\"]\n\
             ask = [\"shell(git push*)\"]\n[allow]\nprograms = [\"ls\"]",
        )
        .expect("write");
        let (ui, notice) = PermissionUi::load(Some(path.clone()));
        assert_eq!(ui.control.mode(), PermissionMode::Manual);
        let rules = ui.control.rules();
        assert_eq!(rules.deny.len(), 1, "the valid deny rule is kept");
        assert_eq!(rules.deny[0].text(), "Bash(curl*)");
        assert_eq!(rules.ask.len(), 1);
        assert!(ui.control.settings().allow.allows_program("ls"));
        assert_eq!(ui.load_result().refused.len(), 1);
        assert_eq!(ui.load_result().refused[0].text, "Bah(rm*)");
        assert!(
            ui.load_result().refused[0]
                .reason
                .contains("which is no kGraf tool")
        );
        let why = ui.load_result().forced_manual.clone().expect("forced");
        assert!(why.contains("deny rule \"Bah(rm*)\""), "{why}");
        assert!(notice.expect("logged").contains(&why));

        let mut bar = PermissionBar::default();
        let line = bar.choose_mode(&ui, PermissionMode::Auto);
        assert!(
            matches!(&line, Line::Error(e) if e.contains(&why)),
            "{line:?}"
        );
        assert_eq!(ui.control.mode(), PermissionMode::Manual);

        let line = bar.set_plan_review(&ui, true);
        assert!(matches!(line, Line::Notice(_)), "{line:?}");
        let back = PermissionSettings::read_entries(&std::fs::read_to_string(&path).expect("read"))
            .expect("parse");
        assert_eq!(back.settings.mode, PermissionMode::Auto, "the user's mode");
        assert!(back.settings.plan_review);
        assert_eq!(
            back.refused,
            ui.load_result().refused,
            "the refused entry is kept"
        );
        assert_eq!(back.settings.rules.deny.len(), 1);
    }

    /// Refused allow entries alone keep the user's mode: what they would
    /// have granted asks instead, which is safe.
    #[test]
    fn a_bad_allow_entry_keeps_the_mode() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("permissions.toml");
        std::fs::write(
            &path,
            "mode = \"auto\"\ndeny = [\"Bash(curl*)\"]\n[allow]\n\
             programs = [\"python3 -c\", \"ls\"]\ntools = [\"fetch\"]",
        )
        .expect("write");
        let (ui, _) = PermissionUi::load(Some(path));
        assert_eq!(ui.control.mode(), PermissionMode::Auto);
        assert_eq!(ui.load_result().forced_manual, None);
        assert_eq!(
            ui.load_result().refused.len(),
            2,
            "{:#?}",
            ui.load_result().refused
        );
        assert!(ui.control.settings().allow.allows_program("ls"));
        assert_eq!(ui.control.rules().deny.len(), 1);
    }

    const EDITOR_FILE: &str = "# my rules\nmode = \"auto\" # keep\nplan_review = true\ndeny = [\"shell(curl*)\"] # no network\nask = [\"shell(git push*)\"]\n\n[allow]\nprograms = [\"make\"]\n";

    fn editor_harness(perm: &PermissionUi) -> egui_kittest::Harness<'static> {
        let drawn = perm.clone();
        egui_kittest::Harness::new_ui(move |ui| {
            if !crate::theme::ensure(ui.ctx()) {
                return;
            }
            draw_rule_editor(ui, &drawn, &crate::theme::Palette::dark());
        })
    }

    fn editor_at(dir: &std::path::Path) -> (PermissionUi, egui_kittest::Harness<'static>) {
        std::fs::write(dir.join("permissions.toml"), EDITOR_FILE).expect("write");
        let (perm, notice) = PermissionUi::load(Some(dir.join("permissions.toml")));
        assert_eq!(notice, None);
        let mut h = editor_harness(&perm);
        h.run();
        (perm, h)
    }

    fn type_into(h: &mut egui_kittest::Harness<'_>, field: usize, text: &str) {
        use egui_kittest::kittest::Queryable;
        {
            let fields: Vec<_> = h
                .get_all_by_role(egui::accesskit::Role::TextInput)
                .collect();
            fields[field].focus();
        }
        h.run();
        h.get_all_by_role(egui::accesskit::Role::TextInput)
            .nth(field)
            .expect("field")
            .type_text(text);
        h.run();
    }

    fn file_text(perm: &PermissionUi) -> String {
        std::fs::read_to_string(perm.file.as_ref().expect("file")).expect("read")
    }

    #[test]
    fn the_editor_lists_the_rules_of_the_file_by_kind() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().expect("dir");
        let (_perm, h) = editor_at(dir.path());
        for t in [
            "Deny rules",
            "Ask rules",
            "Allowed programs",
            "shell(curl*)",
            "shell(git push*)",
            "make",
        ] {
            h.get_by_label(t);
        }
        assert_eq!(h.get_all_by_label("Remove").count(), 3);
        h.get_by_label_contains("Claude Code names such as Bash(...)");
    }

    #[test]
    fn a_valid_rule_is_saved_applied_and_listed_after_a_reload() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().expect("dir");
        let (perm, mut h) = editor_at(dir.path());
        type_into(&mut h, 0, "Bash(rm -rf*)");
        h.get_all_by_label("Add").next().expect("add").click();
        h.run();
        assert!(file_text(&perm).contains("Bash(rm -rf*)"));
        assert_eq!(reread(&perm).rules.deny.len(), 2);
        // The live control, which the workers' hook reads, has it too.
        assert_eq!(perm.control.rules().deny.len(), 2);
        h.get_by_label("Bash(rm -rf*)");
        // A new window reads the file and shows it.
        let mut again = editor_harness(&perm);
        again.run();
        again.get_by_label("Bash(rm -rf*)");
    }

    #[test]
    fn loader_text_starts_with_a_capital_in_the_window() {
        assert_eq!(
            as_sentence("your deny rule `WebFetch` matches no call"),
            "Your deny rule `WebFetch` matches no call"
        );
        assert_eq!(as_sentence(""), "");
    }

    #[test]
    fn an_invalid_rule_shows_the_loaders_message_and_is_not_saved() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().expect("dir");
        let (perm, mut h) = editor_at(dir.path());
        let before = file_text(&perm);
        type_into(&mut h, 0, "WebFech");
        let loader = kgraf_tools::permissions::Rule::parse("WebFech").unwrap_err();
        h.get_by_label(&as_sentence(&loader));
        h.get_all_by_label("Add").next().expect("add").click();
        h.run();
        assert_eq!(file_text(&perm), before);
        assert_eq!(perm.control.rules().deny.len(), 1);
        h.get_by_label(&as_sentence(&loader));
    }

    #[test]
    fn removing_a_rule_saves_and_applies() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().expect("dir");
        let (perm, mut h) = editor_at(dir.path());
        h.get_all_by_label("Remove").next().expect("remove").click();
        h.run();
        assert!(reread(&perm).rules.deny.is_empty());
        assert!(perm.control.rules().deny.is_empty());
        assert!(!file_text(&perm).contains("shell(curl*)"));
        assert!(h.query_by_label("shell(curl*)").is_none());
        assert_eq!(reread(&perm).rules.ask.len(), 1);
    }

    #[test]
    fn other_keys_and_comments_survive_a_save() {
        let dir = tempfile::tempdir().expect("dir");
        let (perm, mut h) = editor_at(dir.path());
        type_into(&mut h, 1, "shell(rm*)");
        {
            use egui_kittest::kittest::Queryable;
            h.get_all_by_label("Add").nth(1).expect("add").click();
        }
        h.run();
        let text = file_text(&perm);
        for kept in [
            "# my rules",
            "mode = \"auto\" # keep",
            "plan_review = true",
            "# no network",
            "programs = [\"make\"]",
        ] {
            assert!(text.contains(kept), "{kept} lost:\n{text}");
        }
        let saved = reread(&perm);
        assert_eq!(saved.mode, PermissionMode::Auto);
        assert!(saved.plan_review);
        assert_eq!(saved.rules.ask.len(), 2);
        // The mode the user chose is untouched in the live control.
        assert_eq!(perm.control.mode(), PermissionMode::Auto);
    }
}
