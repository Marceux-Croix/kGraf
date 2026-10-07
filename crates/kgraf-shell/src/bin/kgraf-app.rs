//! `kgraf-app`: the desktop shell binary.
//!
//! `windows_subsystem = "windows"` (Windows only; no-op elsewhere): `file(1)` reported
//! `kgraf-app.exe` as `(console)`, which on Windows means a black console window flashes up
//! alongside the shell's own GUI window every launch. `kgraf_shell::diag` (this crate's library
//! half, shared with `app.rs`/`lib.rs`) keeps `--probe`/`--measure`'s printed reports and
//! env_logger's warnings reaching the user despite that: see its module doc for how.
//!
//! **What this is not.** Even with a console reattached, `kgraf-app.exe --probe`
//! from `cmd.exe` is not byte-for-byte equivalent to a console-subsystem binary (`cmd.exe` does not
//! wait for a GUI-subsystem child by default, so a plain `kgraf-app --probe` returns the prompt
//! immediately, racing the child's own write, and `%ERRORLEVEL%` is never set from it. Get both the
//! output and the exit code reliably with: `start /wait kgraf-app --probe` (or, in PowerShell,
//! `Start-Process -Wait -NoNewWindow kgraf-app -ArgumentList --probe`). This is documented here,
//! `scripts/release-local.md` and `crates/kgraf-sandbox`'s Windows checklist, not silently left for
//! a user to discover the hard way.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use kgraf_shell::app::MeasureOptions;
use kgraf_shell::settings::Settings;
use kgraf_shell::{LaunchOptions, diag, probe};

#[derive(Debug, Parser)]
#[command(name = "kgraf-app", about = "kGraf desktop shell")]
struct Args {
    /// Settings file (TOML). Defaults to the platform config directory. A missing file
    /// means defaults and is created on the first change; an out-of-range compaction
    /// point or an unknown key is an error.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Accept only a software (CPU) adapter. Fails loudly where none exists.
    #[arg(long)]
    force_software: bool,
    /// Print the adapters this machine offers, and what force_fallback_adapter returns,
    /// then exit without opening a window.
    #[arg(long)]
    probe: bool,
    /// Measure for this many seconds after the first presented frame, idle 3 s, print
    /// a report and exit.
    #[arg(long, value_name = "SECONDS")]
    measure: Option<f64>,
    /// Repaint every frame (frame-time measurement) instead of only on change.
    #[arg(long)]
    continuous: bool,
    /// Present without vsync.
    #[arg(long)]
    no_vsync: bool,
    /// Run without a tray icon; background mode is then unavailable.
    #[arg(long)]
    no_tray: bool,
    /// Project directory whose galaxy to show. An unindexed directory is shown as
    /// such (run `kgraf index` in it); the view loads the index when it appears.
    #[arg(long, value_name = "DIR")]
    project: Option<PathBuf>,
    /// Capture the window to this PNG file after the first frame and
    /// --screenshot-after seconds, then quit.
    #[arg(long, value_name = "FILE")]
    screenshot: Option<PathBuf>,
    /// Seconds to wait after the first frame before --screenshot captures.
    #[arg(long, value_name = "SECONDS", default_value_t = 3.0)]
    screenshot_after: f64,
}

fn default_settings_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", kgraf_shell::app_dirs::APP_NAME)
        .context("no home directory, so no default settings location; pass --config")?;
    Ok(dirs.config_dir().join("settings.toml"))
}

fn main() -> Result<()> {
    // Absolute first, before diag::init below, so cold-start numbers include the
    // console/log-redirect setup too, not just argument parsing.
    let process_start = Instant::now();

    // Parsing itself does no I/O (clap only prints once told to, via e.exit() below or
    // our own code after this), so it happens before any stdout/stderr decision is
    // made -- that decision needs to know whether this is a diagnostic invocation
    // (--probe, --measure, -h/--help, --version, or a usage error), and clap is the
    // only thing that reliably knows the last three of those.
    let parse_result = Args::try_parse();
    let is_diagnostic = match &parse_result {
        Ok(args) => args.probe || args.measure.is_some() || args.screenshot.is_some(),
        Err(_) => true,
    };
    // Decision 0062: settings and log directories under the former name move
    // before diag::init opens the log there or anything reads settings. The
    // notices print after diag::init, so on Windows they reach the log too.
    let moved_dirs = kgraf_shell::app_dirs::migrate();
    diag::init(is_diagnostic);
    for outcome in moved_dirs.context("moving kGraf's settings and log directories")? {
        eprintln!("{}", outcome.notice());
    }
    // Local-only diagnostics (decision 0069): after the
    // directory migration above, so it opens the post-migration path, not one
    // `app_dirs::migrate` is about to move out from under it. `resolve_path`
    // honors `KGRAF_DIAG_PATH`/`KGRAF_DIAG_DISABLE` (a test
    // run without an override would write real lines into the actual
    // per-user file); every test that runs this binary must set one of them.
    // `None` (disabled, or no resolvable per-user data directory) just leaves
    // diagnostics off for this run; `kgraf_diag::record` is always a safe
    // no-op before `init`. Distinct from `kgraf_shell::diag` (this file's
    // `diag::init` above): that module is a console/log-file sink for this
    // binary's own printed text (`--probe`, `--measure`, wgpu warnings);
    // `kgraf_diag` is the structured usage-and-error file `kgraf diag show`
    // reads.
    if let Some(diag_path) = kgraf_diag::resolve_path() {
        kgraf_diag::init(diag_path, kgraf_diag::DEFAULT_CAP_BYTES);
    }
    // Held for the rest of `main`: its `Drop` flushes the writer on every
    // return from here (the early `return Ok(())` for `--probe` below
    // included), and even while a panic unwinds through `main` (2026-09-27
    // A `static`'s own `Drop` never runs at process exit, so records
    // made right before a short-lived run's exit could be lost without this).
    let _diag_shutdown = kgraf_diag::guard(std::time::Duration::from_secs(2));
    kgraf_diag::record(kgraf_diag::DiagEvent::launch());
    // Warnings from wgpu and egui-wgpu (dropped frames, adapter trouble) go to stderr
    // instead of being lost; RUST_LOG overrides the level. Deliberately after diag::init
    // above: env_logger's own writer must see the redirected stderr, not whatever it
    // was before (Windows only; a no-op elsewhere -- see diag's module doc).
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    // Same shape as clap's own Parser::parse(): print (help/version to stdout, a usage
    // error to stderr) and exit with clap's own chosen code. The only difference from
    // calling .parse() directly is that diag::init has already run by this point.
    let args = parse_result.unwrap_or_else(|e| e.exit());

    if args.probe {
        diag::print_out(&probe::run().render());
        return Ok(());
    }

    // The worker shell needs Git Bash on Windows. A diagnostic run (--measure,
    // --screenshot) is unattended and cannot answer a question, so it skips the check.
    #[cfg(windows)]
    if args.measure.is_none() && args.screenshot.is_none() {
        git_for_windows::ensure_or_explain()?;
    }

    let settings_path = match args.config {
        Some(p) => p,
        None => default_settings_path()?,
    };
    let settings = Settings::load(&settings_path)
        .with_context(|| format!("loading settings file {}", settings_path.display()))?;

    let run_for = match args.measure {
        Some(s) if s.is_finite() && s > 0.0 => Some(Duration::from_secs_f64(s)),
        Some(s) => anyhow::bail!("--measure must be a positive number of seconds, got {s}"),
        None => None,
    };
    let screenshot = match args.screenshot {
        Some(path) if args.screenshot_after.is_finite() && args.screenshot_after >= 0.0 => {
            Some(kgraf_shell::app::ScreenshotOptions {
                path,
                after: Duration::from_secs_f64(args.screenshot_after),
            })
        }
        Some(_) => anyhow::bail!(
            "--screenshot-after must be zero or a positive number of seconds, got {}",
            args.screenshot_after
        ),
        None => None,
    };
    // A path that is not a directory is a usage error at launch, not a panel state.
    let project = match args.project {
        Some(p) => Some(
            std::fs::canonicalize(&p)
                .ok()
                .filter(|p| p.is_dir())
                .with_context(|| format!("--project {} is not a directory", p.display()))?,
        ),
        None => None,
    };

    let project_for_diag = project.clone();
    let result = kgraf_shell::run(LaunchOptions {
        process_start,
        settings,
        settings_path,
        force_software: args.force_software,
        measure: MeasureOptions {
            run_for,
            continuous: args.continuous,
        },
        no_vsync: args.no_vsync,
        no_tray: args.no_tray,
        project,
        screenshot,
    })
    .context("kgraf-app could not start");
    if let Err(err) = &result {
        // `Some(&project)` scrubs a path under the open project to relative;
        // anything else (a settings or log path, usually under the user's
        // home directory) is hashed (`kgraf_diag::scrub`).
        let (code, component) = diag_code_for(err);
        kgraf_diag::record(kgraf_diag::DiagEvent::error(
            code,
            component,
            kgraf_diag::scrub::scrub_text(&format!("{err:#}"), project_for_diag.as_deref()),
        ));
    }
    result
}

/// The most specific `KG-nnnn` code and component this `anyhow` chain names
/// (requirement 3): walks every cause looking for a type that implements
/// [`kgraf_diag::DiagCode`], returning the first (closest to the top) match.
/// Falls back to this crate's own catch-all when nothing in the chain is a
/// type this binary knows how to downcast to.
fn diag_code_for(err: &anyhow::Error) -> (&'static str, &'static str) {
    use kgraf_diag::DiagCode;
    macro_rules! try_downcast {
        ($cause:expr, $($ty:ty),+ $(,)?) => {
            $(
                if let Some(e) = $cause.downcast_ref::<$ty>() {
                    return Some((e.diag_code(), e.diag_component()));
                }
            )+
        };
    }
    // The chain runs outermost to innermost; the innermost match is the most
    // specific one (`kgraf-cli`'s identical helper
    // had the same shape: it must not stop at an outer
    // wrapper's own catch-all before ever reaching a more specific cause
    // behind it, such as an `InterpretError` wrapping a `GraphError`).
    let downcast_one =
        |cause: &(dyn std::error::Error + 'static)| -> Option<(&'static str, &'static str)> {
            try_downcast!(
                cause,
                kgraf_shell::ShellError,
                kgraf_host::Error,
                kgraf_host::NotReady,
                kgraf_orchestrator::OrchestratorError,
                kgraf_graph::GraphError,
                kgraf_interpret::InterpretError,
                kgraf_schedule::ScheduleError,
                kgraf_tools::ToolError,
            );
            None
        };
    let chain: Vec<&(dyn std::error::Error + 'static)> = err.chain().collect();
    for cause in chain.into_iter().rev() {
        if let Some(found) = downcast_one(cause) {
            return found;
        }
    }
    let spec = kgraf_diag::catch_all_for("kgraf-shell");
    (spec.code, spec.crate_name)
}

/// The Git for Windows check, with native message boxes because this binary
/// has no console. A yes runs `winget`; every other end shows the fix and
/// stops the launch.
#[cfg(windows)]
mod git_for_windows {
    use anyhow::{Result, anyhow};
    use kgraf_sandbox::git_for_windows::{SystemProbe, ensure};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_ICONERROR, MB_ICONQUESTION, MB_OK, MB_YESNO, MessageBoxW,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn show(text: &str, flags: u32) -> i32 {
        let text = wide(text);
        let title = wide("kGraf");
        // SAFETY: both strings are NUL-terminated and outlive the call; a null
        // owner window is allowed.
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), flags) }
    }

    pub fn ensure_or_explain() -> Result<()> {
        let outcome = ensure(&SystemProbe, |question| {
            show(question, MB_YESNO | MB_ICONQUESTION) == IDYES
        });
        match outcome.into_result() {
            Ok(_) => Ok(()),
            Err(text) => {
                show(&text, MB_OK | MB_ICONERROR);
                Err(anyhow!(text))
            }
        }
    }
}
