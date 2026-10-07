//! The full run on Windows: facts, the source mirror, the test binaries,
//! the end-to-end checks, the cleanup, and the report on the Desktop.
//!
//! **Why a drive letter.** A test binary holds its package folder as the
//! build machine spelled it (`env!("CARGO_MANIFEST_DIR")`, for example
//! `/home/dev/kgraf/crates/kgraf-host`), and reads fixtures and starts
//! programs (`CARGO_BIN_EXE_*`) under it. Windows reads such a path as
//! rooted on the current drive. The runner copies the sources and programs
//! into the work folder at that same relative path and maps a free drive
//! letter there with `subst`, then runs each binary with its working folder
//! on that drive, so every compile-time path resolves inside the work
//! folder with no change to the tests. The mapping lasts for this logon
//! session at most and the cleanup removes and verifies it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::cli::RunOptions;
use crate::console::Console;
use crate::e2e;
use crate::facts::{self, MachineFacts};
use crate::libtest::{self, Event, Outcome, Parser};
use crate::manifest::{MANIFEST_NAME, Manifest, Set, TestRun};
use crate::platform::{self, CANCEL, FINISHED};
use crate::progress::Progress;
use crate::redact::Redactor;
use crate::report::{self, BinaryResult, Failure, Report, StepTime};
use crate::workdir::{self, Layout, Made, Traces};

/// Steps of docs/WINDOWS-TESTING.md that stay manual.
pub const MANUAL: &[&str] = &[
    "Section 2 (installing Git for Windows, the build tools and Rust): the bundle needs only Git for Windows, and it never installs anything.",
    "Section 3 (building from source): replaced by the cross-built bundle; a native Windows build is still worth a manual run before a release.",
    "Section 6a cases 1 to 3 (accepting or declining the winget install of Git for Windows, in the app and on a terminal): the runner never installs; the decision logic is covered by unit tests and the unattended answer by the end-to-end checks.",
    "Section 6a case 4 (per-machine, per-user and other-drive Git installs): needs reinstalling Git; the runner records which install it found.",
    "Section 6a case 5 (renaming the winget stub): the runner removes winget from PATH for one start instead.",
    "The desktop app (kgraf-app.exe) window, icon and installer: needs a person looking at the screen.",
    "Network isolation with Windows Firewall: see MANUAL-NETWORK-TEST.txt (needs an administrator PowerShell).",
];

/// Seconds a step outside the test binaries is expected to take.
const SECS_FACTS: f64 = 20.0;
const SECS_MIRROR: f64 = 20.0;
const SECS_E2E: f64 = 90.0;
const SECS_CLEANUP: f64 = 15.0;
/// Per binary: start-up and listing.
const SECS_BINARY_BASE: f64 = 2.0;
const SECS_LIST_EACH: f64 = 0.5;

fn utc_now() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    let (hh, mm, ss) = (rem / 3600, (rem / 60) % 60, rem % 60);
    (
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z"),
        format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}"),
    )
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn bundle_dir() -> Result<PathBuf, String> {
    std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "the runner has no folder".into())
}

pub fn load_manifest(dir: &Path) -> Result<Manifest, String> {
    let text = std::fs::read_to_string(dir.join(MANIFEST_NAME)).map_err(|e| {
        format!("{MANIFEST_NAME} not found next to the runner ({e}); unzip the whole bundle first")
    })?;
    serde_json::from_str(&text).map_err(|e| format!("{MANIFEST_NAME}: {e}"))
}

/// One reader of the console input for the whole run: a question that
/// timed out must not leave a reader behind that eats the next answer.
fn stdin_lines() -> &'static std::sync::Mutex<std::sync::mpsc::Receiver<String>> {
    static RX: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Receiver<String>>> =
        std::sync::OnceLock::new();
    RX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let mut line = String::new();
                match std::io::stdin().read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        std::sync::Mutex::new(rx)
    })
}

/// The next line typed within `wait`, after dropping lines typed earlier.
fn read_answer(wait: Option<Duration>) -> Option<String> {
    let rx = stdin_lines().lock().ok()?;
    while rx.try_recv().is_ok() {}
    match wait {
        Some(w) => rx.recv_timeout(w).ok(),
        None => rx.recv().ok(),
    }
}

/// Ask a yes/no question with a default taken after `secs` seconds.
fn ask(console: &Console, question: &str, default: bool, secs: u64) -> bool {
    console.log(&format!(
        "{question} [{}] (answers {} by itself in {secs} s)",
        if default { "Y/n" } else { "y/N" },
        if default { "yes" } else { "no" }
    ));
    match read_answer(Some(Duration::from_secs(secs))) {
        Some(line) => match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => default,
        },
        None => default,
    }
}

fn wait_for_key() {
    println!("Press Enter to close this window.");
    let _ = read_answer(None);
}

/// Machine facts; every command here only reads.
pub fn collect_facts(work: &Path) -> MachineFacts {
    let mut f = MachineFacts {
        binary_arch: std::env::consts::ARCH.into(),
        ..MachineFacts::default()
    };
    if let Some(text) =
        platform::reg_query(r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion", None)
    {
        let v = facts::parse_reg_query(&text);
        let build: u32 = v
            .get("CurrentBuild")
            .and_then(|b| b.parse().ok())
            .unwrap_or(0);
        f.windows = v.get("ProductName").map(|p| facts::windows_name(p, build));
        f.display_version = v.get("DisplayVersion").cloned();
        f.build = v
            .get("CurrentBuild")
            .map(|b| format!("{b}.{}", v.get("UBR").cloned().unwrap_or_default()));
        f.edition = v.get("EditionID").cloned();
        f.installation_type = v.get("InstallationType").cloned();
    }
    if let Some((process, native)) = platform::machines() {
        f.native_arch = Some(facts::machine_name(native).into());
        // An x86_64 process on ARM64 is emulated (IsWow64Process2 reports an
        // unknown process machine for it, as for any non-WOW64 process).
        f.emulated = Some(native == 0xAA64 && cfg!(target_arch = "x86_64"));
        let _ = process;
    }
    {
        use sysinfo::System;
        let mut sys = System::new();
        sys.refresh_cpu_all();
        sys.refresh_memory();
        f.cpu = sys.cpus().first().map(|c| c.brand().trim().to_owned());
        f.cpu_cores = Some(sys.cpus().len());
        f.ram_gib = Some((sys.total_memory() as f64 / (1u64 << 30) as f64 * 10.0).round() / 10.0);
    }
    f.elevated =
        crate::proc::capture("whoami", &["/groups"]).and_then(|g| facts::elevated_from_whoami(&g));
    f.git_version = crate::proc::capture("git", &["--version"]).and_then(|t| facts::first_line(&t));
    #[cfg(windows)]
    {
        use kgraf_sandbox::git_for_windows::{Probe, SystemProbe};
        match SystemProbe.find() {
            Ok(p) => {
                f.git_bash_version = crate::proc::capture(&p.display().to_string(), &["--version"])
                    .and_then(|t| facts::first_line(&t));
                f.git_bash_path = Some(p.display().to_string());
            }
            Err(e) => f.git_bash_search = Some(e),
        }
    }
    f.winget_version =
        crate::proc::capture("winget", &["--version"]).and_then(|t| facts::first_line(&t));
    f.cargo_version =
        crate::proc::capture("cargo", &["--version"]).and_then(|t| facts::first_line(&t));
    if let Some(t) = crate::proc::capture(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-MpComputerStatus | Select-Object AMServiceEnabled,AntivirusEnabled,RealTimeProtectionEnabled,IsTamperProtected,AMProductVersion | Format-List",
        ],
    ) {
        f.defender = facts::parse_format_list(&t);
    } else {
        f.notes.push("Defender state not readable without administrator rights, or Defender is not the antivirus".into());
    }
    let reg1 = |key: &str, value: &str| {
        platform::reg_query(key, Some(value)).and_then(|t| facts::parse_reg_query(&t).remove(value))
    };
    f.smartscreen = reg1(
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer",
        "SmartScreenEnabled",
    )
    .or_else(|| {
        reg1(
            r"HKLM\SOFTWARE\Policies\Microsoft\Windows\System",
            "EnableSmartScreen",
        )
    });
    f.smart_app_control = Some(
        facts::smart_app_control(
            reg1(
                r"HKLM\SYSTEM\CurrentControlSet\Control\CI\Policy",
                "VerifiedAndReputablePolicyState",
            )
            .as_deref(),
        )
        .into(),
    );
    f.developer_mode = reg1(
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock",
        "AllowDevelopmentWithoutDevLicense",
    )
    .map(|v| v == "1");
    f.locale = reg1(r"HKCU\Control Panel\International", "LocaleName");
    f.ansi_code_page = reg1(r"HKLM\SYSTEM\CurrentControlSet\Control\Nls\CodePage", "ACP");
    f.work_volume_fs = platform::volume_fs(work);
    f
}

fn traces() -> Traces {
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let mut roots = Vec::new();
    if let Some(a) = std::env::var_os("APPDATA") {
        roots.push(("APPDATA", PathBuf::from(a)));
    }
    if let Some(a) = std::env::var_os("LOCALAPPDATA") {
        roots.push(("LOCALAPPDATA", PathBuf::from(a)));
    }
    Traces {
        profile_files: profile
            .map(|p| platform::profile_escape_files(&p))
            .unwrap_or_default(),
        run_values: platform::reg_query(platform::RUN_KEY, None)
            .map(|t| platform::escape_values(&t))
            .unwrap_or_default(),
        kgraf_app_data: platform::kgraf_app_data(&roots),
    }
}

/// Remove what this run left outside the work folder; returns notes.
fn remove_traces(new: &Traces, before: &Traces) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(profile) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
        for f in &new.profile_files {
            let p = profile.join(f);
            let ok = std::fs::remove_file(&p).is_ok();
            notes.push(format!("escape trace %USERPROFILE%\\{f}: removed {ok}"));
        }
    }
    for v in &new.run_values {
        let ok = platform::delete_run_value(v);
        notes.push(format!("escape trace HKCU Run value {v}: removed {ok}"));
    }
    // A kGraf folder that did not exist before the run is this run's.
    for label in ["APPDATA", "LOCALAPPDATA"] {
        let folder = format!("{label}/kGraf");
        let created =
            new.kgraf_app_data.contains(&folder) && !before.kgraf_app_data.contains(&folder);
        if created && let Some(root) = std::env::var_os(label) {
            let ok = std::fs::remove_dir_all(PathBuf::from(root).join("kGraf")).is_ok();
            notes.push(format!(
                "%{label}%\\kGraf appeared during the run: removed {ok}"
            ));
        } else if new.kgraf_app_data.iter().any(|e| e.starts_with(&folder)) {
            notes.push(format!(
                "new entries under %{label}%\\kGraf, which existed before the run; left in place: {:?}",
                new.kgraf_app_data
            ));
        }
    }
    notes
}

/// Copy a folder tree (the bundle's `src`), plain files and folders only.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<u64> {
    let mut n = 0;
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let t = e.file_type()?;
        let dest = to.join(e.file_name());
        if t.is_dir() {
            n += copy_tree(&e.path(), &dest)?;
        } else if t.is_file() {
            std::fs::copy(e.path(), dest)?;
            n += 1;
        }
    }
    Ok(n)
}

/// The build root as a path under the mirror: `/home/dev/k` becomes
/// `<mirror>\Users\dev\k`.
pub fn mirrored(mirror_root: &Path, build_root: &str) -> PathBuf {
    let mut p = mirror_root.to_path_buf();
    for part in build_root.split(['/', '\\']).filter(|s| !s.is_empty()) {
        p.push(part);
    }
    p
}

/// The arguments for one test binary run.
pub fn test_args(run: &TestRun) -> Vec<String> {
    let mut a = vec!["--test-threads=1".to_owned()];
    a.extend(run.filters.iter().cloned());
    for s in &run.skips {
        a.push("--skip".into());
        a.push(s.pattern.clone());
    }
    a
}

/// Time limit for one binary: five times its expected time, between 10 and
/// 40 minutes.
pub fn limit_for(expected_secs: f64) -> Duration {
    Duration::from_secs_f64((expected_secs * 5.0).clamp(600.0, 2400.0))
}

struct Env {
    /// Variables every child gets.
    vars: Vec<(String, String)>,
}

fn base_env(layout: &Layout) -> Env {
    let gitconfig = layout.kgraf_home.join("gitconfig");
    let _ = std::fs::write(
        &gitconfig,
        "[user]\n\tname = kgraf-wintest\n\temail = kgraf-wintest@localhost\n[commit]\n\tgpgsign = false\n",
    );
    let t = layout.tmp.display().to_string();
    let vars = vec![
        ("TEMP".to_owned(), t.clone()),
        ("TMP".to_owned(), t),
        (
            "KGRAF_DIAG_PATH".to_owned(),
            layout
                .kgraf_home
                .join("diagnostics.jsonl")
                .display()
                .to_string(),
        ),
        (
            "KGRAF_SETTINGS_PATH".to_owned(),
            layout
                .kgraf_home
                .join("settings.toml")
                .display()
                .to_string(),
        ),
        (
            "GIT_CONFIG_GLOBAL".to_owned(),
            gitconfig.display().to_string(),
        ),
        ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
        ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
    ];
    Env { vars }
}

/// Run one test binary and read its results.
fn run_binary(
    console: &Console,
    run: &TestRun,
    bundle: &Path,
    cwd: &Path,
    env: &Env,
    expected: f64,
    listed: Option<u64>,
) -> BinaryResult {
    let mut cmd = Command::new(bundle.join(&run.exe));
    let mut vars = env.vars.clone();
    vars.push(("CARGO_MANIFEST_DIR".into(), cwd.display().to_string()));
    vars.push(("CARGO_PKG_NAME".into(), run.package.clone()));
    cmd.args(test_args(run))
        .current_dir(cwd)
        .env_clear()
        .envs(e2e::child_env(std::env::vars(), &vars));
    let mut parser = Parser::default();
    let total = listed.unwrap_or(0);
    let f = crate::proc::run(cmd, limit_for(expected), &CANCEL, |line| {
        let ev = parser.feed(line);
        console.with(|p, _| match ev {
            Event::Started(_) => {
                p.detail = format!(
                    "test {} of {}",
                    parser.tests.len() + 1,
                    total.max(parser.announced.unwrap_or(0))
                );
            }
            Event::Finished(Outcome::Passed) => p.passed += 1,
            Event::Finished(Outcome::Failed) => p.failed += 1,
            _ => {}
        });
    });
    parser.finish();
    let failures: Vec<Failure> = parser
        .tests
        .iter()
        .filter(|t| t.outcome == Outcome::Failed)
        .map(|t| {
            let tail = parser
                .failure_text
                .iter()
                .find(|(n, _)| *n == t.name)
                .map(|(_, s)| s.clone())
                .unwrap_or_default();
            Failure {
                test: t.name.clone(),
                class: report::classify_failure(&tail),
                tail,
            }
        })
        .collect();
    let ended_cleanly = parser.summary.is_some() && f.code == Some(0);
    BinaryResult {
        id: run.id.clone(),
        set: run.set,
        exit_code: f.code,
        timed_out: f.timed_out,
        cancelled: f.cancelled,
        start_error: f.start_error.clone(),
        secs: f.secs,
        expected_secs: expected,
        listed,
        passed: report::count(&parser.tests, Outcome::Passed),
        failed: report::count(&parser.tests, Outcome::Failed),
        ignored: report::count(&parser.tests, Outcome::Ignored),
        tests: parser.tests.clone(),
        failures,
        open_test: parser.open.clone(),
        tail: (!ended_cleanly).then(|| f.tail_text(60)),
        skipped: run
            .skips
            .iter()
            .map(|s| format!("{}: {}", s.pattern, s.reason))
            .collect(),
    }
}

/// `--list` count for a binary, with the same filters and skips.
fn list_count(run: &TestRun, bundle: &Path, cwd: &Path, env: &Env) -> Option<u64> {
    let mut cmd = Command::new(bundle.join(&run.exe));
    let mut args = test_args(run);
    args.retain(|a| a != "--test-threads=1");
    args.push("--list".into());
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(e2e::child_env(std::env::vars(), &env.vars));
    // Counted as the lines stream: the kept tail is shorter than a long list.
    let mut n = 0;
    let f = crate::proc::run(cmd, Duration::from_secs(60), &CANCEL, |line| {
        n += libtest::count_listed(line);
    });
    (f.code == Some(0)).then_some(n)
}

/// Where the report goes: the flag, the Desktop known folder, the profile's
/// Desktop, then the bundle folder.
fn report_dir(o: &RunOptions, bundle: &Path) -> (PathBuf, &'static str) {
    if let Some(d) = &o.report_dir {
        return (d.clone(), "the folder given with --report-dir");
    }
    if let Some(d) = platform::desktop_dir().filter(|d| d.is_dir()) {
        return (d, "the Desktop");
    }
    if let Some(d) = std::env::var_os("USERPROFILE")
        .map(|p| PathBuf::from(p).join("Desktop"))
        .filter(|d| d.is_dir())
    {
        return (d, "%USERPROFILE%\\Desktop");
    }
    (bundle.to_path_buf(), "the bundle folder (no Desktop found)")
}

/// The bundle folder holds only the bundle's own entries.
pub fn only_bundle_entries(names: &[String], allowed: &[String]) -> bool {
    !names.is_empty()
        && names
            .iter()
            .all(|n| allowed.iter().any(|a| a.eq_ignore_ascii_case(n)))
}

pub fn run(o: RunOptions) -> i32 {
    let bundle = match bundle_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let manifest = match load_manifest(&bundle) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            if !o.no_wait {
                wait_for_key();
            }
            return 2;
        }
    };
    platform::install_ctrl_handler();
    let (started_utc, stamp) = utc_now();
    let t0 = Instant::now();
    let base = std::env::temp_dir();
    let id = workdir::run_id(&stamp, std::process::id());
    let layout = workdir::layout(&base, &id);
    let mut made = vec![Made::Folder {
        path: layout.root.display().to_string(),
    }];
    let mut notes = Vec::new();
    for d in [
        &layout.root,
        &layout.mirror,
        &layout.tmp,
        &layout.kgraf_home,
        &layout.e2e,
        &layout.logs,
    ] {
        if let Err(e) = std::fs::create_dir_all(d) {
            eprintln!("cannot create the work folder {}: {e}", d.display());
            return 2;
        }
    }
    if let Err(e) = platform::join_kill_on_close_job() {
        notes.push(format!(
            "the runner could not join a kill-on-close Job Object: {e}"
        ));
    }

    let core: Vec<&TestRun> = manifest
        .runs
        .iter()
        .filter(|r| r.set == Set::Core)
        .collect();
    let extended: Vec<&TestRun> = manifest
        .runs
        .iter()
        .filter(|r| r.set == Set::Extended)
        .collect();
    let mut steps: Vec<(String, f64)> = vec![
        ("machine facts".into(), SECS_FACTS),
        ("source mirror".into(), SECS_MIRROR),
        (
            "count tests".into(),
            SECS_LIST_EACH * manifest.runs.len() as f64,
        ),
    ];
    for (i, r) in core.iter().enumerate() {
        steps.push((format!("core {}/{} {}", i + 1, core.len(), r.id), 30.0));
    }
    steps.push(("end-to-end checks".into(), SECS_E2E));
    let console = Console::new(Progress::new(steps));
    println!(
        "kGraf Windows self-test, kGraf {} ({})",
        manifest.kgraf_version, manifest.git_commit
    );
    println!(
        "Work folder: {} (removed at the end)",
        layout.root.display()
    );
    println!("Press Ctrl+C to stop; the work folder is removed either way.\n");
    let ticker = console.spawn_ticker();

    let traces_before = traces();
    // Step 0: facts.
    console.with(|p, now| p.start(0, now));
    let machine = collect_facts(&layout.root);
    let resources_start = facts::resources(&layout.root);
    console.with(|p, now| p.finish(0, now));
    if machine.elevated == Some(true) {
        console.log("Note: this window runs as administrator. kGraf's sandbox is made for a normal user; results are recorded with this noted.");
        notes.push(
            "the runner ran elevated (as administrator); the sandbox is meant for a normal user"
                .into(),
        );
    }
    if machine
        .work_volume_fs
        .as_deref()
        .is_some_and(|fs| !fs.eq_ignore_ascii_case("NTFS"))
    {
        notes.push(format!(
            "the work folder is on {:?}, not NTFS; the sandbox refuses such a folder",
            machine.work_volume_fs
        ));
    }
    if machine.git_bash_path.is_none() {
        console.log("Git for Windows was not found. The sandbox's sh and bash tests and the worker shell checks need it; they will fail or be skipped. Nothing will be installed.");
    }

    // Step 1: the source mirror and the drive letter.
    console.with(|p, now| p.start(1, now));
    let mut cwd_root: Option<PathBuf> = None;
    let mirror_build_root = mirrored(&layout.mirror, &manifest.build_root);
    match copy_tree(&bundle.join("src"), &mirror_build_root) {
        Ok(n) => notes.push(format!("source mirror: {n} files")),
        Err(e) => notes.push(format!("source mirror copy failed: {e}")),
    }
    for p in &manifest.programs {
        let to = mirror_build_root.join(&p.build_path);
        if let Some(d) = to.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Err(e) = std::fs::copy(bundle.join(&p.exe), &to) {
            notes.push(format!("program {} not placed: {e}", p.name));
        }
    }
    match platform::map_drive(&layout.mirror) {
        Ok(letter) => {
            made.push(Made::Drive {
                letter,
                target: layout.mirror.display().to_string(),
            });
            cwd_root = Some(mirrored(
                Path::new(&format!("{letter}:\\")),
                &manifest.build_root,
            ));
        }
        Err(e) => notes.push(format!(
            "drive mapping failed ({e}); tests that read fixtures by build path will fail"
        )),
    }
    console.with(|p, now| p.finish(1, now));
    let cwd_root = cwd_root.unwrap_or_else(|| mirror_build_root.clone());
    let env = base_env(&layout);
    let cwd_for = |r: &TestRun| {
        let mut p = cwd_root.clone();
        for part in r.package_dir.split(['/', '\\']).filter(|s| !s.is_empty()) {
            p.push(part);
        }
        p
    };

    // Step 2: count every binary's tests, so the question about the
    // extended set and the estimate rest on real counts.
    console.with(|p, now| p.start(2, now));
    let all: Vec<&TestRun> = core.iter().chain(extended.iter()).copied().collect();
    let mut listed_all = Vec::new();
    for (i, r) in all.iter().enumerate() {
        if CANCEL.load(Ordering::Relaxed) {
            break;
        }
        console.with(|p, _| p.detail = format!("{}/{}", i + 1, all.len()));
        listed_all.push(list_count(r, &bundle, &cwd_for(r), &env));
    }
    console.with(|p, now| p.finish(2, now));
    let expected_of = |i: usize, r: &TestRun| {
        let n = listed_all.get(i).copied().flatten().unwrap_or(20);
        SECS_BINARY_BASE + n as f64 * r.secs_per_test
    };
    for (i, r) in core.iter().enumerate() {
        let e = expected_of(i, r);
        console.with(|p, _| p.set_expected(3 + i, e));
    }
    let est_ext: f64 = extended
        .iter()
        .enumerate()
        .map(|(j, r)| expected_of(core.len() + j, r))
        .sum();

    // Which set.
    let run_extended = match o.extended {
        Some(x) => x,
        None => ask(
            &console,
            &format!(
                "Also run the extended set ({} more test binaries, {} tests, expected {} on a fast PC)?",
                extended.len(),
                listed_all.iter().skip(core.len()).flatten().sum::<u64>(),
                crate::progress::eta_words(est_ext)
            ),
            true,
            20,
        ),
    };
    if run_extended {
        console.with(|p, _| {
            for (j, r) in extended.iter().enumerate() {
                p.push(
                    format!("extended {}/{} {}", j + 1, extended.len(), r.id),
                    expected_of(core.len() + j, r),
                );
            }
        });
    }
    console.with(|p, _| p.push("cleanup".into(), SECS_CLEANUP));
    let step_e2e = 3 + core.len();
    let first_ext = step_e2e + 1;
    let step_cleanup = if run_extended {
        first_ext + extended.len()
    } else {
        first_ext
    };
    let chosen: Vec<&TestRun> = core
        .iter()
        .copied()
        .chain(extended.iter().copied().filter(|_| run_extended))
        .collect();
    let listed = listed_all;

    // The test binaries: the core set, the end-to-end checks, then the
    // extended set.
    let mut binaries = Vec::new();
    let mut e2e_checks = Vec::new();
    for (i, r) in chosen.iter().enumerate() {
        if CANCEL.load(Ordering::Relaxed) {
            break;
        }
        if i == core.len() {
            e2e_checks = run_e2e_step(&console, step_e2e, &bundle, &layout, &env, &machine);
        }
        let step = if i < core.len() {
            3 + i
        } else {
            first_ext + (i - core.len())
        };
        let expected = console
            .with(|p, _| p.steps()[step].expected_secs)
            .unwrap_or(30.0);
        console.with(|p, now| p.start(step, now));
        let b = run_binary(
            &console,
            r,
            &bundle,
            &cwd_for(r),
            &env,
            expected,
            listed.get(i).copied().flatten(),
        );
        console.with(|p, now| p.finish(step, now));
        if !b.clean() {
            console.log(&format!(
                "  {}: {} passed, {} failed{}",
                r.id,
                b.passed,
                b.failed,
                if b.timed_out { ", TIMED OUT" } else { "" }
            ));
        }
        binaries.push(b);
    }
    if chosen.len() <= core.len() && !CANCEL.load(Ordering::Relaxed) {
        e2e_checks = run_e2e_step(&console, step_e2e, &bundle, &layout, &env, &machine);
    }

    // Cleanup.
    let interrupted = CANCEL.load(Ordering::Relaxed);
    console.with(|p, now| p.start(step_cleanup, now));
    let resources_end = facts::resources(&layout.root);
    let traces_after = traces();
    let new = workdir::new_traces(&traces_before, &traces_after);
    notes.extend(remove_traces(&new, &traces_before));
    let mut cleanup = workdir::clean_up(&made, &base, &platform::ops());
    // A second look for traces after the removals above.
    let still = workdir::new_traces(&traces_before, &traces());
    if !still.profile_files.is_empty() || !still.run_values.is_empty() {
        cleanup.verified = false;
        cleanup
            .left
            .push(format!("escape traces still present: {still:?}"));
    }
    console.with(|p, now| p.finish(step_cleanup, now));
    console.stop(ticker);

    let (finished_utc, _) = utc_now();
    let step_times = console
        .with(|p, now| {
            p.steps()
                .iter()
                .map(|s| StepTime {
                    step: s.name.clone(),
                    expected_secs: s.expected_secs,
                    actual_secs: match s.state {
                        crate::progress::StepState::Done { secs } => secs,
                        crate::progress::StepState::Running { started } => now - started,
                        crate::progress::StepState::Pending => 0.0,
                    },
                })
                .collect()
        })
        .unwrap_or_default();
    let mut r = Report {
        schema: report::SCHEMA,
        runner_version: env!("CARGO_PKG_VERSION").into(),
        kgraf_version: manifest.kgraf_version.clone(),
        git_commit: manifest.git_commit.clone(),
        built_utc: manifest.built_utc.clone(),
        started_utc,
        finished_utc,
        mode: if run_extended {
            "core and extended".into()
        } else {
            "core only".into()
        },
        interrupted,
        machine,
        resources_start,
        resources_end,
        binaries,
        e2e: e2e_checks,
        not_shipped: manifest.not_shipped.clone(),
        manual: MANUAL.iter().map(|s| (*s).to_owned()).collect(),
        network_isolation_experiment:
            "not included (the manual Windows Firewall test is in MANUAL-NETWORK-TEST.txt)".into(),
        cleanup,
        traces_found: new,
        bundle_removal: String::new(),
        step_times,
        total_secs: t0.elapsed().as_secs_f64(),
        notes,
    };
    if !run_extended {
        r.notes.push(format!(
            "the extended set ({} binaries) was not run",
            extended.len()
        ));
    }

    // Offer to delete the bundle folder; the report says what was chosen.
    let names: Vec<String> = std::fs::read_dir(&bundle)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let removable = only_bundle_entries(&names, &manifest.bundle_entries);
    let remove_bundle = !o.keep_bundle
        && removable
        && !o.no_wait
        && ask(
            &console,
            "Delete this unzipped bundle folder after the window closes? Keep it if you still want to run MANUAL-NETWORK-TEST.txt.",
            false,
            30,
        );
    r.bundle_removal = if remove_bundle {
        "removal scheduled after exit (a detached cmd.exe deletes the folder once the runner has ended; not verifiable from inside the run)".into()
    } else if !removable {
        "kept: the folder holds files that are not part of the bundle".into()
    } else {
        "kept (delete the unzipped folder by hand when done)".into()
    };

    let user = std::env::var("USERNAME").ok();
    let computer = std::env::var("COMPUTERNAME").ok();
    let redactor = Redactor::new(
        user.as_deref(),
        computer.as_deref(),
        Some(&manifest.build_root),
    );
    let text = redactor.apply(&report::render(&r));
    let (dir, which) = report_dir(&o, &bundle);
    let name = format!("kgraf-wintest-report-{stamp}.md");
    let mut path = dir.join(&name);
    if std::fs::write(&path, &text).is_err() {
        path = bundle.join(&name);
        let _ = std::fs::write(&path, &text);
    }
    FINISHED.store(true, Ordering::SeqCst);

    let t = report::totals(&r);
    println!();
    println!("{}", report::verdict(&t, &r));
    println!(
        "Tests: {} passed, {} failed, {} failed for a missing tool, {} ignored. End-to-end: {} passed, {} failed.",
        t.passed, t.failed, t.failed_needs_tool, t.ignored, t.e2e_pass, t.e2e_fail
    );
    println!(
        "Cleanup: {}",
        if r.cleanup.verified {
            "work folder removed and verified"
        } else {
            "NOT verified, see the report"
        }
    );
    println!("Report written to {which}:\n  {}", path.display());
    println!("Send that file back. Nothing else is left on this machine by the run.");
    if remove_bundle {
        match platform::schedule_folder_removal(&bundle) {
            Ok(()) => println!("This folder will be deleted when the window closes."),
            Err(e) => println!(
                "This folder could not be scheduled for deletion ({e}); delete it by hand."
            ),
        }
    } else {
        println!("Delete the unzipped bundle folder when you are done.");
    }
    if !o.no_wait {
        wait_for_key();
    }
    i32::from(!(t.failed == 0 && t.hung_or_crashed == 0 && t.e2e_fail == 0))
}

fn run_e2e_step(
    console: &Console,
    step: usize,
    bundle: &Path,
    layout: &Layout,
    env: &Env,
    machine: &MachineFacts,
) -> Vec<e2e::Check> {
    console.with(|p, now| p.start(step, now));
    let ctx = e2e::Ctx {
        kgraf: bundle.join("programs").join("kgraf.exe"),
        dir: layout.e2e.clone(),
        env: env.vars.clone(),
        git_bash_found: machine.git_bash_path.is_some(),
        cancel: &CANCEL,
    };
    let checks = e2e::run_all(&ctx, cfg!(windows));
    for c in &checks {
        if c.status == e2e::Status::Fail {
            console.log(&format!("  e2e {} failed: {}", c.id, c.title));
        }
    }
    console.with(|p, now| p.finish(step, now));
    checks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Skip;

    #[test]
    fn civil_dates_are_right() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_733), (2026, 10, 7));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn the_mirror_path_keeps_the_build_path_under_the_root() {
        let m = mirrored(Path::new("/w/m"), "/home/dev/kgraf");
        assert_eq!(m, Path::new("/w/m/home/dev/kgraf"));
    }

    #[test]
    fn test_arguments_carry_filters_and_skips() {
        let r = TestRun {
            id: "p/lib/x#core".into(),
            package: "p".into(),
            kind: "lib".into(),
            name: "x".into(),
            exe: "tests/x.exe".into(),
            package_dir: "crates/p".into(),
            set: Set::Core,
            filters: vec!["windows".into()],
            skips: vec![Skip {
                pattern: "needs_cargo".into(),
                reason: "r".into(),
            }],
            secs_per_test: 1.0,
        };
        assert_eq!(
            test_args(&r),
            vec!["--test-threads=1", "windows", "--skip", "needs_cargo"]
        );
        assert_eq!(limit_for(10.0), Duration::from_secs(600));
        assert_eq!(limit_for(1000.0), Duration::from_secs(2400));
    }

    #[test]
    fn the_bundle_folder_is_removable_only_when_it_holds_bundle_entries() {
        let allowed: Vec<String> = ["kgraf-wintest.exe", "src", "tests"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let ok: Vec<String> = ["KGRAF-WINTEST.EXE", "src"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let extra: Vec<String> = ["kgraf-wintest.exe", "My Photos"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert!(only_bundle_entries(&ok, &allowed));
        assert!(!only_bundle_entries(&extra, &allowed));
        assert!(!only_bundle_entries(&[], &allowed));
    }
}
