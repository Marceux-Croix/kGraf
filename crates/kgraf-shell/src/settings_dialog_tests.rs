//! Mechanism tests for the settings dialog: each drives the real dialog
//! (kittest, through the accessibility tree) on a window-less app, operates
//! one control, and checks that the control reaches its effect: the shared
//! value a run is opened from, the live permission control a worker's calls
//! are judged with, the process-wide gate, or the key the settings file
//! keeps for the next launch. A test that a value persists proves nothing
//! about its effect, so each names the effect it reads.
//!
//! Every frame runs what the window runs after drawing the dialog: the gate
//! writers of the logic pass (`sync_gates`) and the save of the ui pass
//! (`persist_if_changed`). Paths are temporary folders, never the user's.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use kgraf_tools::permissions::{
    ModeSource, PermissionControl, PermissionMode, PermissionSettings, ProcessFacts,
    ScriptConstruct,
};

use super::SettingsTab;
use crate::adapter::{AdapterFacts, RenderPath};
use crate::app::{AppParts, MeasureOptions, RenderStatus, SettingsStore, ShellApp};
use crate::metrics::FrameClock;
use crate::permission_bar::{PermissionBar, PermissionUi};
use crate::projects::Workspace;
use crate::settings::{Appearance, Settings};
use crate::surface::DropCounters;
use crate::theme::Palette;

/// The machine maximum the fixed capacity reading gives the parallelism
/// slider.
const MACHINE_MAX: u32 = 8;

struct Fixture {
    dir: tempfile::TempDir,
    h: Harness<'static, ShellApp>,
    shared: kgraf_host::limits::SharedLimits,
}

impl Fixture {
    fn settings_path(&self) -> std::path::PathBuf {
        self.dir.path().join("settings.toml")
    }

    /// The settings file as the next launch reads it.
    fn saved(&self) -> Settings {
        let text = std::fs::read_to_string(self.settings_path()).unwrap_or_default();
        Settings::from_toml_str(&text).expect("the saved file loads")
    }

    fn saved_text(&self) -> String {
        std::fs::read_to_string(self.settings_path()).unwrap_or_default()
    }

    fn app(&self) -> &ShellApp {
        self.h.state()
    }

    /// Bring the control named `label` into view and click it.
    fn press(&mut self, label: &str) {
        self.h.get_by_label(label).scroll_to_me();
        self.h.run();
        self.h.get_by_label(label).click();
        self.h.run();
    }

    /// The `n`th button named `label`, into view and clicked.
    fn press_nth(&mut self, label: &str, n: usize) {
        self.h
            .get_all_by_label(label)
            .nth(n)
            .expect("button")
            .scroll_to_me();
        self.h.run();
        self.h
            .get_all_by_label(label)
            .nth(n)
            .expect("button")
            .click();
        self.h.run();
    }

    /// Focus slider `n` of the tab and press `key` `times` times.
    fn nudge_slider(&mut self, n: usize, key: egui::Key, times: usize) {
        {
            let slider = self
                .h
                .get_all_by_role(egui::accesskit::Role::Slider)
                .nth(n)
                .expect("slider");
            slider.scroll_to_me();
        }
        self.h.run();
        self.h
            .get_all_by_role(egui::accesskit::Role::Slider)
            .nth(n)
            .expect("slider")
            .focus();
        self.h.run();
        for _ in 0..times {
            self.h.key_press(key);
            self.h.run();
        }
    }

    /// Type `text` into text field `n` of the tab.
    fn type_into(&mut self, n: usize, text: &str) {
        self.h
            .get_all_by_role(egui::accesskit::Role::TextInput)
            .nth(n)
            .expect("field")
            .scroll_to_me();
        self.h.run();
        self.h
            .get_all_by_role(egui::accesskit::Role::TextInput)
            .nth(n)
            .expect("field")
            .focus();
        self.h.run();
        self.h
            .get_all_by_role(egui::accesskit::Role::TextInput)
            .nth(n)
            .expect("field")
            .type_text(text);
        self.h.run();
    }
}

fn app(dir: &Path, permissions: Option<PermissionUi>) -> ShellApp {
    let settings = Settings::default();
    ShellApp::new(AppParts {
        store: SettingsStore::new(dir.join("settings.toml"), settings.clone()),
        settings,
        render: RenderStatus {
            facts: AdapterFacts {
                name: "test".into(),
                driver: String::new(),
                driver_info: String::new(),
                backend: "test".into(),
                device_kind: crate::adapter::DeviceKind::Cpu,
                surface_compatible: true,
            },
            path: RenderPath::Hardware,
        },
        tray: None,
        clock: FrameClock::new(Instant::now()),
        drops: Arc::new(DropCounters::default()),
        measure: MeasureOptions::default(),
        workspace: Workspace::default(),
        opener: None,
        screenshot: None,
        machine_gate: kgraf_orchestrator::MachineGate::new(kgraf_core::ParallelismLimits::derive(
            1, 1, 1,
        )),
        capacity: Some(crate::capacity_bg::CapacityHandle::fixed(
            kgraf_capacity::Capacity {
                max_agents: MACHINE_MAX,
                max_writers: 2,
                exec_slots: 2,
                bounds: Vec::new(),
            },
        )),
        update: None,
        permissions,
    })
}

/// The dialog open at `tab`, drawn the way the window draws it.
fn open(tab: SettingsTab, permissions: Option<PermissionUi>) -> Fixture {
    open_with(tab, permissions, |_| {})
}

fn open_with(
    tab: SettingsTab,
    permissions: Option<PermissionUi>,
    prepare: impl FnOnce(&mut ShellApp),
) -> Fixture {
    let dir = tempfile::tempdir().expect("dir");
    let mut a = app(dir.path(), permissions);
    let shared = kgraf_host::limits::SharedLimits::default();
    a.use_run_limits(shared.clone());
    a.chrome.settings_tab = Some(tab);
    prepare(&mut a);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1100.0, 1000.0))
        .build_ui_state(
            |ui, a: &mut ShellApp| {
                if !crate::theme::ensure(ui.ctx()) {
                    return;
                }
                let ctx = ui.ctx().clone();
                a.settings_dialog(&ctx, &Palette::dark());
                a.sync_gates();
                a.store.persist_if_changed(&a.settings);
            },
            a,
        );
    h.run();
    Fixture { dir, h, shared }
}

/// A permission control over a file in `dir` holding `text`.
fn permissions_at(dir: &Path, text: &str) -> PermissionUi {
    let path = dir.join("permissions.toml");
    std::fs::write(&path, text).expect("write");
    PermissionUi::load(Some(path)).0
}

fn reread(perm: &PermissionUi) -> PermissionSettings {
    let text = std::fs::read_to_string(perm.file.as_ref().expect("file")).expect("read");
    PermissionSettings::read_entries(&text)
        .expect("a permissions file")
        .settings
}

// ------------------------------------------------------------------ tabs

/// The tab list switches the dialog's content, and Done closes it.
#[test]
fn the_tab_list_switches_tabs_and_done_closes() {
    let mut f = open(SettingsTab::General, None);
    for (tab, shows) in [
        (SettingsTab::Runs, "Parallelism"),
        (SettingsTab::Permissions, "Warn about rule conflicts"),
        (SettingsTab::Appearance, "Reduce motion"),
        (
            SettingsTab::Providers,
            "Open a project to see its provider routes.",
        ),
        (SettingsTab::General, "Background mode"),
    ] {
        f.press(tab.label());
        assert_eq!(f.app().chrome.settings_tab, Some(tab));
        f.h.get_by_label(shows);
    }
    f.press("Done");
    assert_eq!(f.app().chrome.settings_tab, None);
}

// --------------------------------------------------------------- general

/// Background mode is on by default; turning it off is what the close path
/// reads (`close_to_tray` takes `settings.background_mode`) and is kept for
/// the next launch.
#[test]
fn background_mode_reaches_the_close_path_and_the_file() {
    let mut f = open(SettingsTab::General, None);
    assert!(f.app().settings.background_mode);
    f.press("Background mode");
    assert!(!f.app().settings.background_mode);
    assert!(!f.saved().background_mode);
    assert!(f.saved_text().contains("background_mode = false"));
}

#[test]
fn check_for_updates_on_start_is_kept_for_the_next_launch() {
    let mut f = open(SettingsTab::General, None);
    f.press("Check for updates on start");
    assert!(
        f.saved().check_for_updates,
        "the launch passes it to the update thread"
    );
}

/// A recorder that must never be called: no test here accepts an update.
struct NoLaunch;

impl crate::update_check::Launcher for NoLaunch {
    fn launch(
        &self,
        _installer: &crate::update_check::Installer,
    ) -> Result<crate::update_check::InstallOutcome, crate::update_check::UpdateError> {
        panic!("nothing in the settings tests installs")
    }
}

fn wait_for(f: &mut Fixture, done: impl Fn(&crate::update_bg::UpdateState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = f.app().update_state().expect("an update handle");
        if done(&state) || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f.h.run();
}

/// "Check now" asks the update thread at once, and "Include prereleases"
/// decides whether a prerelease counts: against a local server that offers
/// only a prerelease, the first check finds nothing and the second, with
/// the switch on, offers it.
#[tokio::test(flavor = "multi_thread")]
async fn check_now_and_include_prereleases_reach_the_update_check() {
    use crate::update_bg::UpdateState;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let name = crate::update_check::asset_candidates(
        std::env::consts::OS,
        std::env::consts::ARCH,
        "999.0.0-rc.1",
    )
    .expect("this test's own platform is supported")
    .remove(0);
    let url = format!("{}/{name}", server.uri());
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{}/{}/releases",
            crate::update_check::GITHUB_OWNER,
            crate::update_check::GITHUB_REPO
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([{
            "tag_name": "v999.0.0-rc.1",
            "draft": false,
            "prerelease": true,
            "assets": [
                {"name": name, "browser_download_url": url},
                {"name": format!("{name}.sha256"), "browser_download_url": format!("{url}.sha256")},
            ],
        }])))
        .mount(&server)
        .await;
    let uri = server.uri();
    let mut f = open_with(SettingsTab::General, None, move |a| {
        a.update = Some(crate::update_bg::UpdateHandle::spawn_full(
            false,
            false,
            Arc::new(|| {}),
            Arc::new(NoLaunch),
            uri,
        ));
    });
    f.press("Check now");
    wait_for(&mut f, |s| {
        matches!(s, UpdateState::UpToDate | UpdateState::Failed(_))
    });
    assert_eq!(f.app().update_state(), Some(UpdateState::UpToDate));
    f.h.get_by_label("kGraf is up to date.");

    f.press("Include prereleases");
    assert!(f.saved().allow_prerelease_updates);
    f.press("Check now");
    wait_for(&mut f, |s| {
        matches!(s, UpdateState::Available(_) | UpdateState::Failed(_))
    });
    assert!(
        matches!(f.app().update_state(), Some(UpdateState::Available(_))),
        "{:?}",
        f.app().update_state()
    );
    f.h.get_by_label_contains("is available.");
    assert_eq!(server.received_requests().await.expect("recorded").len(), 2);
}

// ------------------------------------------------------------------ runs

/// The parallelism slider moves the process-wide gate every project's
/// admission shares.
#[test]
fn the_parallelism_slider_moves_the_machine_gate() {
    let mut f = open(SettingsTab::Runs, None);
    assert_eq!(f.app().machine_gate().limits().max_agents, MACHINE_MAX);
    f.nudge_slider(0, egui::Key::ArrowLeft, 3);
    assert_eq!(f.app().settings.parallelism.effective(), MACHINE_MAX - 3);
    assert_eq!(f.app().machine_gate().limits().max_agents, MACHINE_MAX - 3);
}

/// The compaction slider reaches the value each project's host is opened
/// from, and a run configuration built from it takes the point.
#[test]
fn the_compaction_slider_reaches_the_run_configuration() {
    let mut f = open(SettingsTab::Runs, None);
    assert_eq!(f.shared.get().compaction_point, None);
    f.nudge_slider(1, egui::Key::ArrowLeft, 2);
    let point = f.shared.get().compaction_point.expect("sent to runs");
    assert!(point < 80, "{point}");
    assert_eq!(f.saved().compaction.value().percent(), point);
    let run = f.dir.path().join("run.json");
    std::fs::write(&run, r#"{"routes": ["kimi:model"], "prices": "p.json"}"#).expect("write");
    let config =
        kgraf_host::RunConfig::load_with(f.dir.path(), &run, &f.shared.get()).expect("load");
    assert_eq!(config.point, Some(point));
}

/// Reads one limit of the shared value.
type ReadLimit = fn(&kgraf_host::limits::UserLimits) -> Option<u64>;

/// Each run-limit slider reaches the shared value a run is opened from.
#[test]
fn each_run_limit_slider_reaches_the_shared_value() {
    let mut f = open(SettingsTab::Runs, None);
    assert_eq!(f.shared.get(), kgraf_host::limits::UserLimits::default());
    let read: [(&str, ReadLimit); 6] = [
        ("rounds", |u| u.rounds.map(u64::from)),
        ("max_checkpoints", |u| u.max_checkpoints.map(u64::from)),
        ("worker_max_tokens", |u| u.worker_max_tokens.map(u64::from)),
        ("planner_max_tokens", |u| {
            u.planner_max_tokens.map(u64::from)
        }),
        ("window", |u| u.window),
        ("answer_wait", |u| u.answer_wait_secs.map(u64::from)),
    ];
    for (n, (name, get)) in read.iter().enumerate() {
        f.nudge_slider(2 + n, egui::Key::ArrowLeft, 1);
        assert!(get(&f.shared.get()).is_some(), "{name} did not reach runs");
    }
    let run = f.dir.path().join("run.json");
    std::fs::write(&run, r#"{"routes": ["kimi:model"], "prices": "p.json"}"#).expect("write");
    let user = f.shared.get();
    let config = kgraf_host::RunConfig::load_with(f.dir.path(), &run, &user).expect("load");
    assert_eq!(Some(config.steps), user.rounds);
    assert_eq!(Some(config.max_checkpoints), user.max_checkpoints);
    assert_eq!(Some(config.max_tokens), user.worker_max_tokens);
    assert_eq!(Some(config.planner_max_tokens), user.planner_max_tokens);
    assert_eq!(Some(config.window), user.window);
    assert_eq!(user.answer_wait_secs, Some(270), "one step down from 300 s");
    assert_eq!(config.answer_wait_secs, user.answer_wait_secs);
}

/// The worktree switch reaches the retention every host is built with.
#[test]
fn the_worktree_switch_reaches_the_retention() {
    let mut f = open(SettingsTab::Runs, None);
    assert_eq!(
        f.app().settings.worktree_retention(),
        kgraf_host::WorktreeRetention::Delete
    );
    f.press("Delete worker worktrees after use");
    assert_eq!(
        f.app().settings.worktree_retention(),
        kgraf_host::WorktreeRetention::Keep
    );
    assert!(!f.saved().delete_worker_worktrees);
}

// ----------------------------------------------------------- permissions

/// The rule-conflict switch reaches the shared value a run is opened from.
#[test]
fn the_rule_conflict_switch_reaches_the_run() {
    let mut f = open(SettingsTab::Permissions, None);
    f.press("Warn about rule conflicts");
    assert_eq!(f.shared.get().warn_rule_conflicts, Some(true));
    let run = f.dir.path().join("run.json");
    std::fs::write(&run, r#"{"routes": ["kimi:model"], "prices": "p.json"}"#).expect("write");
    let config =
        kgraf_host::RunConfig::load_with(f.dir.path(), &run, &f.shared.get()).expect("load");
    assert!(config.warn_rule_conflicts);
}

/// Allow Bypass reaches the live control: the box's dropdown then opens
/// the Bypass warning instead of refusing, and the file keeps it.
#[test]
fn allow_bypass_reaches_the_box_and_the_file() {
    let dir = tempfile::tempdir().expect("dir");
    let perm = permissions_at(dir.path(), "mode = \"manual\"\n");
    let mut f = open(SettingsTab::Permissions, Some(perm.clone()));
    let line = PermissionBar::default().choose_mode(&perm, PermissionMode::Bypass);
    assert!(matches!(line, crate::orchestrator_box::Line::Error(_)));
    f.press("Allow Bypass mode");
    assert!(perm.control.settings().allow_bypass);
    assert!(reread(&perm).allow_bypass);
    let mut bar = PermissionBar::default();
    bar.choose_mode(&perm, PermissionMode::Bypass);
    assert!(bar.bypass_warning, "offered now, behind its warning");
}

/// disable_bypass shows the switch disabled, with why and what to do.
#[test]
fn a_disabled_bypass_shows_a_disabled_switch_and_why() {
    let dir = tempfile::tempdir().expect("dir");
    let perm = permissions_at(dir.path(), "disable_bypass = true\n");
    let f = open(SettingsTab::Permissions, Some(perm));
    assert!(
        f.h.get_by_label("Allow Bypass mode")
            .accesskit_node()
            .is_disabled()
    );
    f.h.get_by_label_contains("disable_bypass = true");
}

const RULES_FILE: &str = "# mine\nmode = \"auto\"\ndeny = [\"shell(curl*)\"]\nask = [\"shell(git push*)\"]\n\n[allow]\nprograms = [\"make\"]\ntools = [\"apply_patch\"]\npaths = [\"docs/*\"]\n";

/// Each of the five lists adds an entry to the file and to the live
/// control a worker's calls are judged with, and removes one from both.
#[test]
fn every_rule_list_adds_and_removes_through_the_live_control() {
    let dir = tempfile::tempdir().expect("dir");
    let perm = permissions_at(dir.path(), RULES_FILE);
    let mut f = open(SettingsTab::Permissions, Some(perm.clone()));
    // Field order: deny, ask, allowed programs, tools, paths.
    for (n, entry) in [
        "shell(rm -rf*)",
        "Bash(git reset*)",
        "cargo",
        "Bash",
        "src/generated/*",
    ]
    .iter()
    .enumerate()
    {
        f.type_into(n, entry);
        f.press_nth("Add", n);
        f.h.get_by_label(entry);
    }
    let rules = perm.control.rules();
    assert!(rules.deny.iter().any(|r| r.text() == "shell(rm -rf*)"));
    assert!(rules.ask.iter().any(|r| r.text() == "Bash(git reset*)"));
    let allow = perm.control.settings().allow;
    assert!(allow.allows_program("cargo"));
    assert!(allow.allows_tool("shell"), "Bash is read as shell");
    assert!(allow.allows_path("src/generated/x.rs"));
    let saved = reread(&perm);
    assert_eq!((saved.rules.deny.len(), saved.rules.ask.len()), (2, 2));
    assert!(saved.allow.allows_program("cargo") && saved.allow.allows_path("src/generated/x"));
    let text = std::fs::read_to_string(perm.file.as_ref().expect("file")).expect("read");
    assert!(text.starts_with("# mine"), "comments kept:\n{text}");

    // Remove buttons in list order: deny 2, ask 2, programs 2, tools 2,
    // paths 2. Removing the first of each list leaves the added entries.
    for n in (0..5).rev() {
        f.press_nth("Remove", n * 2);
    }
    let rules = perm.control.rules();
    assert!(!rules.deny.iter().any(|r| r.text() == "shell(curl*)"));
    assert!(!rules.ask.iter().any(|r| r.text() == "shell(git push*)"));
    let allow = perm.control.settings().allow;
    assert!(!allow.allows_program("make") && allow.allows_program("cargo"));
    assert!(!allow.allows_tool("apply_patch"));
    assert!(!allow.allows_path("docs/a.md"));
    assert!(f.h.query_by_label("shell(curl*)").is_none());
}

/// A Claude Code tool kGraf lacks is added with the loader's notice shown
/// under the field as it is typed, and after the save in the list of what
/// reading the file found. A typo is refused with the loader's message and
/// not written.
#[test]
fn a_tool_kgraf_lacks_is_added_with_its_notice_and_a_typo_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let perm = permissions_at(dir.path(), RULES_FILE);
    let mut f = open(SettingsTab::Permissions, Some(perm.clone()));
    let notice = kgraf_tools::permissions::validate_entry(
        kgraf_tools::permissions::EntryList::Deny,
        "WebFetch",
    )
    .expect("loads")
    .notice
    .expect("a notice");
    assert!(f.h.query_by_label("Notices").is_none());
    f.type_into(0, "WebFetch");
    f.h.get_by_label(&crate::permission_bar::as_sentence(&notice));
    f.press_nth("Add", 0);
    assert!(
        reread(&perm)
            .rules
            .deny
            .iter()
            .any(|r| r.text() == "WebFetch")
    );
    assert_eq!(perm.load_result().notices, vec![notice.clone()]);
    f.h.get_by_label("Notices");
    f.h.get_by_label(&crate::permission_bar::as_sentence(&notice));

    let before = std::fs::read_to_string(perm.file.as_ref().expect("file")).expect("read");
    f.type_into(0, "WebFech");
    let loader = kgraf_tools::permissions::validate_entry(
        kgraf_tools::permissions::EntryList::Deny,
        "WebFech",
    )
    .unwrap_err();
    f.h.get_by_label(&crate::permission_bar::as_sentence(&loader));
    f.press_nth("Add", 0);
    let after = std::fs::read_to_string(perm.file.as_ref().expect("file")).expect("read");
    assert_eq!(before, after, "a refused entry is not written");
}

/// A refused deny entry forces Manual. Removing it in the editor reads the
/// file again: the forced Manual is lifted without a restart, the file's
/// mode applies, and the refused list is gone.
#[test]
fn fixing_the_file_in_the_editor_lifts_a_forced_manual() {
    let dir = tempfile::tempdir().expect("dir");
    let perm = permissions_at(
        dir.path(),
        "mode = \"auto\"\ndeny = [\"Bah(rm*)\", \"shell(curl*)\"]\n",
    );
    assert_eq!(perm.control.mode(), PermissionMode::Manual);
    let why = perm.load_result().forced_manual.clone().expect("forced");
    let mut f = open(SettingsTab::Permissions, Some(perm.clone()));
    f.h.get_by_label(&why);
    f.h.get_by_label("Refused entries in permissions.toml");
    f.press_nth("Remove", 0);
    assert_eq!(perm.load_result().forced_manual, None);
    assert!(perm.load_result().refused.is_empty());
    assert_eq!(perm.control.mode(), PermissionMode::Auto, "the file's mode");
    assert_eq!(perm.control.rules().deny.len(), 1);
    assert!(f.h.query_by_label(&why).is_none());
    assert!(
        f.h.query_by_label("Refused entries in permissions.toml")
            .is_none()
    );
    let line = PermissionBar::default().choose_mode(&perm, PermissionMode::Manual);
    assert!(
        matches!(line, crate::orchestrator_box::Line::Notice(_)),
        "{line:?}"
    );
}

/// With an entry refused elsewhere in the file, an editor change still
/// applies its valid entries to the live control.
#[test]
fn an_edit_applies_while_another_entry_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let perm = permissions_at(
        dir.path(),
        "mode = \"auto\"\ndeny = [\"shell(curl*)\"]\n[allow]\nprograms = [\"two words\"]\n",
    );
    assert_eq!(perm.load_result().refused.len(), 1);
    let mut f = open(SettingsTab::Permissions, Some(perm.clone()));
    f.type_into(0, "shell(wget*)");
    f.press_nth("Add", 0);
    assert_eq!(perm.control.rules().deny.len(), 2, "applied");
    assert_eq!(
        perm.control.mode(),
        PermissionMode::Auto,
        "an allow refusal keeps it"
    );
}

/// A script-construct switch reaches the live control and the file.
#[test]
fn a_script_construct_switch_reaches_the_live_control() {
    let dir = tempfile::tempdir().expect("dir");
    let perm = PermissionUi::with_control(
        Arc::new(
            PermissionControl::new(
                PermissionSettings::default(),
                ModeSource::UserConfig,
                ProcessFacts::default(),
            )
            .expect("control"),
        ),
        Some(dir.path().join("permissions.toml")),
    );
    let mut f = open(SettingsTab::Permissions, Some(perm.clone()));
    f.press(ScriptConstruct::Background.label());
    assert!(
        perm.control
            .script_constructs()
            .allows(ScriptConstruct::Background)
    );
    assert!(
        reread(&perm)
            .script_constructs
            .allows(ScriptConstruct::Background)
    );
}

// ------------------------------------------------------------ appearance

/// The theme list sets the appearance the window applies on its next frame
/// (`ui_pass` compares it with the one applied) and the file keeps it.
#[test]
fn the_theme_list_sets_the_appearance() {
    let mut f = open(SettingsTab::Appearance, None);
    f.press("Light theme");
    assert_eq!(f.app().settings.appearance, Appearance::Light);
    assert_eq!(f.saved().appearance, Appearance::Light);
    f.press("Use device theme");
    assert_eq!(f.saved().appearance, Appearance::Device);
}

#[test]
fn the_color_vision_list_sets_the_palette_mode() {
    let mut f = open(SettingsTab::Appearance, None);
    let v = kgraf_core::palette::Vision::ALL[1];
    f.press(v.label());
    assert_eq!(f.app().settings.color_vision, v);
    assert_eq!(f.saved().color_vision, v);
}

#[test]
fn reduce_motion_and_the_gpu_preference_are_kept() {
    let mut f = open(SettingsTab::Appearance, None);
    f.press("Reduce motion");
    assert!(f.app().settings.reduce_motion, "the indicators read it");
    f.press("Prefer high-performance GPU");
    let saved = f.saved();
    assert!(saved.reduce_motion);
    assert!(
        saved.prefer_high_performance_gpu,
        "the launch reads it to pick the adapter"
    );
}

// --------------------------------------------------------------- picture

/// A picture of each tab, for review: run by hand with
/// `KGRAF_SETTINGS_PICTURES=<folder> cargo test -p kgraf-shell --lib
/// settings_pictures -- --ignored`. It needs a GPU adapter or a software one.
#[test]
#[ignore = "renders with wgpu and writes files; run by hand"]
fn settings_pictures() {
    let Some(out) = std::env::var_os("KGRAF_SETTINGS_PICTURES") else {
        return;
    };
    let out = std::path::PathBuf::from(out);
    std::fs::create_dir_all(&out).expect("folder");
    let dir = tempfile::tempdir().expect("dir");
    let perm = permissions_at(
        dir.path(),
        "mode = \"auto\"\ndeny = [\"shell(curl*)\", \"WebFetch\"]\nask = [\"shell(git push*)\"]\n\
         [allow]\nprograms = [\"make\", \"cargo\"]\ntools = [\"apply_patch\"]\npaths = [\"docs/*\"]\n",
    );
    for tab in SettingsTab::ALL {
        let dir = tempfile::tempdir().expect("dir");
        let mut a = app(dir.path(), Some(perm.clone()));
        a.chrome.settings_tab = Some(tab);
        let mut h = Harness::builder()
            .with_size(egui::vec2(860.0, 680.0))
            .wgpu()
            .build_ui_state(
                |ui, a: &mut ShellApp| {
                    if !crate::theme::ensure(ui.ctx()) {
                        return;
                    }
                    let ctx = ui.ctx().clone();
                    a.settings_dialog(&ctx, &Palette::dark());
                },
                a,
            );
        h.run();
        let image = h.render().expect("rendered");
        let png = crate::png::encode_png_rect(image.width(), image.height(), image.as_raw());
        let name = format!("settings-{}.png", tab.label().to_lowercase());
        std::fs::write(out.join(name), png).expect("write");
    }
}
