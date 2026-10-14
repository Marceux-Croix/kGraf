//! The desktop shell (decision 0054): winit, egui and wgpu, running without
//! a GPU and reporting when it renders on the CPU.
//!
//! The platform layer is eframe (which drives winit and wgpu); the panels are this
//! crate's own code (decision 0055: UI code lives in this tree).

pub mod activity;
pub mod adapter;
pub mod app;
pub mod app_dirs;
pub mod capacity_bg;
pub mod chrome;
pub mod compaction_measure;
pub mod dependency_prompt;
pub mod diag;
pub mod error;
pub mod icon;
pub mod icons;
pub mod interaction;
pub mod land_card;
pub mod layout;
pub mod metrics;
pub mod orchestrator_box;
pub mod orchestrator_host;
pub mod permission_bar;
pub mod presume_bg;
pub mod probe;
pub mod projects;
pub mod resources;
pub mod run_view;
pub mod run_watch;
pub mod schedule_store;
pub mod scheduler_bg;
pub mod search;
pub mod session_feed;
pub mod session_panel;
pub mod settings;
pub mod settings_rows;
pub mod surface;
pub mod text_surface;
pub mod theme;
pub mod transcripts;
pub mod tray;
pub mod update_bg;
pub mod update_check;

/// The PNG encoder `--screenshot` uses for window captures.
#[path = "../../../packaging/png_encoder.rs"]
pub(crate) mod png;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use adapter::{AdapterFacts, AdapterPolicy, GpuPreference, select_adapter};
use app::{AppParts, MeasureOptions, RenderStatus, ScreenshotOptions, SettingsStore, ShellApp};
pub use error::ShellError;
use metrics::FrameClock;
use projects::{ProjectParts, Workspace};
use settings::Settings;
use surface::DropCounters;
use tray::Tray;

/// Everything the binary decides before the window opens.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub process_start: Instant,
    /// Settings as loaded from `settings_path`.
    pub settings: Settings,
    /// Where changed settings are written back.
    pub settings_path: PathBuf,
    /// Accept only a software adapter (exercise the CPU path).
    pub force_software: bool,
    pub measure: MeasureOptions,
    /// Present without vsync, so frame time measures the work and not the display.
    pub no_vsync: bool,
    /// Skip the tray icon. Background mode is then unavailable and the UI says so.
    pub no_tray: bool,
    /// The project directory to show first (a launch choice, configuration only).
    /// The projects the settings file lists open too (decision 0066); with none,
    /// the window says how to open one.
    pub project: Option<PathBuf>,
    /// Capture the window to a PNG and quit (`--screenshot`).
    pub screenshot: Option<ScreenshotOptions>,
}

impl LaunchOptions {
    /// The adapter policy: software only when forced, else hardware ordered by the
    /// "Prefer high-performance GPU" setting.
    pub fn adapter_policy(&self) -> AdapterPolicy {
        if self.force_software {
            AdapterPolicy::SoftwareOnly
        } else {
            AdapterPolicy::PreferHardware(GpuPreference::from_setting(
                self.settings.prefer_high_performance_gpu,
            ))
        }
    }
}

/// Window icon size. The tray uses a smaller embedded size of the same icon asset.
const WINDOW_ICON_PX: u32 = 256;

/// The window's application id: the Wayland `app_id` and the X11 `WM_CLASS`
/// class (eframe passes it to winit through its `wayland` and `x11`
/// features). The Linux desktop entry's `StartupWMClass` must equal it, so
/// the dock groups the window under the entry.
pub const APP_ID: &str = "kgraf";
const TRAY_ICON_PX: u32 = 64;

/// Opens the window and runs until it closes. Blocks the calling (main) thread.
///
/// Each open project is hosted on its own (decision 0066): one
/// `scheduler_bg::ProjectFactory` over the project's `kgraf_host::Host` (opened
/// from `.kgraf/run.json` on first need, reopened when the file changes) serves
/// both its orchestrator box and its scheduled runs, so both share one host, one
/// `Admission` and the box's approval desk. Projects do not share a host.
pub fn run(opts: LaunchOptions) -> Result<(), ShellError> {
    run_with_host(opts, None)
}

/// Builds one project's parts, as the window hosts it: the state directory
/// migrated from the former name (0062), the approval desk and its router, the
/// project's factory shared by its box and its scheduler, and, when a tray exists
/// to show it, the scheduler loop (0052). A failed migration is the project's
/// problem, shown on its row and stage, never a silent skip.
pub fn open_project_parts(
    project: &Path,
    background_mode: bool,
    worktrees: kgraf_host::WorktreeRetention,
    with_scheduler: bool,
    wake: kgraf_orchestrator::Wake,
    permissions: Option<permission_bar::PermissionUi>,
    run_limits: kgraf_host::limits::SharedLimits,
) -> ProjectParts {
    if !project.is_dir() {
        return ProjectParts {
            problem: Some(format!(
                "{} is not a folder (moved or deleted?)",
                project.display()
            )),
            ..ProjectParts::default()
        };
    }
    let notice = match kgraf_host::migrate_state_dir(project) {
        Ok(report) => report.map(|r| r.notice()),
        Err(e) => {
            return ProjectParts {
                problem: Some(format!("its state could not be migrated: {e}")),
                ..ProjectParts::default()
            };
        }
    };
    let desk = project_desk(project, wake.clone());
    // Asks route by run origin: box runs to the desk, scheduled runs unattended
    // unless a person is at the window.
    let router = Arc::new(kgraf_orchestrator::AskRouter::new(desk.clone()));
    let factory = scheduler_bg::ProjectFactory::for_project(project.to_path_buf())
        .with_worktree_retention(worktrees)
        .with_run_limits(run_limits)
        .with_ask_resolver(router.clone());
    // Decision 0094: one permission control for the process, from the
    // user-level file beside the settings file.
    let factory = Arc::new(match permissions {
        Some(p) => factory.with_permissions(p),
        None => factory,
    });
    let scheduler = with_scheduler.then(|| {
        scheduler_bg::SchedulerHandle::spawn_with(
            project.to_path_buf(),
            background_mode,
            factory.clone(),
            Some(wake.clone()),
        )
    });
    ProjectParts {
        host: Some(orchestrator_host::HostParts {
            factory: factory.clone(),
            housekeeping: factory,
            desk,
            router: Some(router),
        }),
        scheduler,
        problem: None,
        notice,
    }
}

/// The box's approval desk for `project`, with the project's persisted allow rules
/// (`.kgraf/allow-rules.json`, as the host's own resolver reads them) when they
/// can be opened. `None` rules is said on stderr and in the box when allow always
/// cannot record a rule.
pub fn project_desk(
    project: &std::path::Path,
    wake: kgraf_orchestrator::Wake,
) -> Arc<kgraf_orchestrator::AskDesk> {
    let rules = (|| -> Result<kgraf_orchestrator::DeskRules, String> {
        let state = project.join(".kgraf");
        if !state.is_dir() {
            return Err(format!("{} does not exist", state.display()));
        }
        // The store refuses a path inside a worktree; worktrees live here.
        let worktrees = state.join("worktrees");
        std::fs::create_dir_all(&worktrees).map_err(|e| e.to_string())?;
        let key = kgraf_core::plain_path::canonicalize(project)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned();
        // Permission subjects are the shell tool's (the tool whose calls ask by
        // command); an MCP tool's any-call rule matches without one.
        let mut registry = kgraf_tools::ToolRegistry::new();
        let (shell, _hook) = kgraf_tools::ShellTool::new(
            kgraf_tools::execpolicy::ExecPolicy::default_kgraf_policy(),
        );
        registry
            .register_builtin(Arc::new(shell), kgraf_tools::Exposure::Prefix)
            .map_err(|e| e.to_string())?;
        kgraf_orchestrator::DeskRules::open(
            state.join("allow-rules.json"),
            worktrees,
            key,
            Arc::new(registry),
        )
        .map_err(|e| e.to_string())
    })();
    let rules = match rules {
        Ok(r) => Some(r),
        Err(e) => {
            log::warn!("the box's approvals have no allow-rule store: {e}");
            None
        }
    };
    Arc::new(kgraf_orchestrator::AskDesk::new(rules, wake))
}

/// [`run`], hosting orchestrator runs through `host`'s factory, so the box can
/// start them and answer their approvals.
pub fn run_with_host(
    opts: LaunchOptions,
    host: Option<orchestrator_host::HostParts>,
) -> Result<(), ShellError> {
    // Decision 0062: a project whose state is still under the former name
    // (`.haustoria`) is moved before anything below reads its state.
    if let Some(project) = &opts.project
        && let Some(report) =
            kgraf_host::migrate_state_dir(project).map_err(ShellError::StateMigration)?
    {
        eprintln!("{}", report.notice());
    }
    let window_icon = icon::asset(WINDOW_ICON_PX);
    let viewport = egui::ViewportBuilder::default()
        .with_title("kGraf")
        .with_app_id(APP_ID)
        .with_inner_size([1200.0, 780.0])
        .with_min_inner_size([640.0, 400.0])
        .with_icon(egui::IconData {
            rgba: window_icon.rgba,
            width: window_icon.size,
            height: window_icon.size,
        });

    let policy = opts.adapter_policy();
    let drops = Arc::new(DropCounters::default());

    let mut wgpu_options = egui_wgpu::WgpuConfiguration::default();
    if opts.no_vsync {
        wgpu_options.surface.present_mode = wgpu::PresentMode::AutoNoVsync;
    }
    // Count every failed surface acquire; the default callback only logs them.
    wgpu_options.on_surface_status = surface::status_callback(Arc::clone(&drops));
    if let egui_wgpu::WgpuSetup::CreateNew(create) = &mut wgpu_options.wgpu_setup {
        if let AdapterPolicy::PreferHardware(pref) = policy {
            // Ignored while a selector is set; kept consistent for anything that reads it.
            create.power_preference = pref.wgpu();
        }
        // Our selector replaces egui-wgpu's power-preference request, so the choice
        // follows `adapter::select_adapter` and a failure carries its full message.
        create.native_adapter_selector = Some(Arc::new(move |adapters, surface| {
            let facts: Vec<AdapterFacts> = adapters
                .iter()
                .map(|a| {
                    let compatible = surface.is_none_or(|s| a.is_surface_supported(s));
                    AdapterFacts::from_info(&a.get_info(), compatible)
                })
                .collect();
            select_adapter(&facts, policy)
                .map(|sel| adapters[sel.index].clone())
                .map_err(|e| e.to_string())
        }));
    }

    let native = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
        wgpu_options,
        centered: true,
        ..Default::default()
    };

    let LaunchOptions {
        process_start,
        mut settings,
        settings_path,
        measure,
        no_tray,
        project,
        screenshot,
        ..
    } = opts;
    // The store holds what the file holds now, so restoring the project list
    // below (a change) is written back once.
    // Decision 0094: the user-level permissions file sits beside the settings
    // file (or where `KGRAF_PERMISSIONS_PATH` says), outside every project.
    let permissions_path = std::env::var_os(kgraf_tools::permissions::PERMISSIONS_PATH_ENV)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            settings_path.with_file_name(kgraf_tools::permissions::PERMISSIONS_FILE_NAME)
        });
    let (permissions, permissions_notice) =
        permission_bar::PermissionUi::load(Some(permissions_path));
    if let Some(notice) = &permissions_notice {
        log::warn!("{notice}");
        diag::print_err(&format!("kgraf-app: {notice}\n"));
    }
    let store = SettingsStore::new(settings_path, settings.clone());

    eframe::run_native(
        "kGraf",
        native,
        Box::new(move |cc| {
            let state = cc.wgpu_render_state.as_ref().ok_or_else(|| {
                ShellError::Window("eframe started without a wgpu render state".to_owned())
            })?;
            let render = RenderStatus::from_render_state(state);
            log::info!("rendering on {}", render.label());
            diag::print_err(&format!("kgraf-app: rendering on {}\n", render.label()));
            crate::theme::install(&cc.egui_ctx, settings.appearance, settings.color_vision);
            let tray = if no_tray {
                None
            } else {
                // Created here, inside the running event loop, as macOS requires.
                Some(Tray::new(&icon::asset(TRAY_ICON_PX), cc.egui_ctx.clone())?)
            };
            // A project's scheduler loop (0052) needs a tray: its status is only
            // shown there (no second surface); without one it is not
            // started. Its initial enabled state follows the settings'
            // background-mode switch; `ShellApp` flips it live from there on.
            let wake_ctx = cc.egui_ctx.clone();
            let wake: kgraf_orchestrator::Wake = Arc::new(move || wake_ctx.request_repaint());
            // Decision 0067: one process-wide `MachineGate`, shared by
            // every project's `Host` (`Host::open` also calls
            // `process_machine_gate`, so this is the same singleton), and
            // the re-measure thread that keeps it current. Neither is built
            // inside `ShellApp::new` (a headless test creating a `ShellApp`
            // must never start a background thread).
            let machine_gate =
                kgraf_orchestrator::process_machine_gate(kgraf_core::ParallelismLimits::derive(
                    kgraf_orchestrator::DEFAULT_MAX_LIVE_AGENTS as u32,
                    kgraf_orchestrator::DEFAULT_MAX_LIVE_AGENTS as u32,
                    std::thread::available_parallelism()
                        .map(|n| n.get() as u32)
                        .unwrap_or(4),
                ));
            let capacity_volumes = capacity_bg::Volumes::default();
            capacity_volumes.set(settings.projects.clone());
            let capacity = Some(capacity_bg::CapacityHandle::spawn(
                capacity_volumes,
                wake.clone(),
            ));
            // The thread always starts (so "Check now" in
            // Settings works without a restart), but it performs no network call at
            // start-up unless `check_for_updates` is on (default off; egress rule,
            // see `update_check`'s module doc).
            let update = Some(update_bg::UpdateHandle::spawn(
                settings.check_for_updates,
                settings.allow_prerelease_updates,
                wake.clone(),
            ));
            let with_scheduler = tray.is_some();
            // The saved projects, then `--project`, which is shown first (0066).
            // A host given by the caller serves the `--project` folder.
            // `restore` hands the opener canonical paths, so the match is on the
            // canonical `--project` too: `./repo` and `/abs/repo` are one folder.
            // One value for every project's factory; the sliders write it.
            let run_limits = kgraf_host::limits::SharedLimits::new(settings.run_limits());
            let mut given = host;
            let launch = project.as_deref().map(projects::canonical);
            let workspace = Workspace::restore(
                &settings.projects,
                settings.active_project.as_deref(),
                launch.as_deref(),
                |root| match (Some(root) == launch.as_deref())
                    .then(|| given.take())
                    .flatten()
                {
                    Some(h) => ProjectParts {
                        host: Some(h),
                        ..ProjectParts::default()
                    },
                    None => open_project_parts(
                        root,
                        settings.background_mode,
                        settings.worktree_retention(),
                        with_scheduler,
                        wake.clone(),
                        Some(permissions.clone()),
                        run_limits.clone(),
                    ),
                },
            );
            settings.projects = workspace.roots();
            settings.active_project = workspace.active().map(|p| p.root.clone());
            let opener_wake = wake.clone();
            let opener_permissions = permissions.clone();
            let opener_limits = run_limits.clone();
            let opener: projects::Opener = Box::new(move |root, background, worktrees| {
                open_project_parts(
                    root,
                    background,
                    worktrees,
                    with_scheduler,
                    opener_wake.clone(),
                    Some(opener_permissions.clone()),
                    opener_limits.clone(),
                )
            });
            let mut app = ShellApp::new(AppParts {
                settings,
                store,
                render,
                tray,
                clock: FrameClock::new(process_start),
                drops,
                measure,
                workspace,
                opener: Some(opener),
                screenshot,
                machine_gate,
                capacity,
                update,
                permissions: Some(permissions.clone()),
            });
            app.use_run_limits(run_limits);
            app.enable_compaction_measurement();
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| ShellError::Window(e.to_string()))
}

#[cfg(test)]
mod desktop_entry_tests {
    /// The desktop entry names the window by the id the window
    /// actually carries.
    #[test]
    fn the_desktop_entry_matches_the_window_app_id() {
        let entry = include_str!("../../../packaging/linux/kgraf.desktop");
        let class = entry
            .lines()
            .find_map(|l| l.strip_prefix("StartupWMClass="))
            .expect("StartupWMClass");
        assert_eq!(class, super::APP_ID);
    }
}
