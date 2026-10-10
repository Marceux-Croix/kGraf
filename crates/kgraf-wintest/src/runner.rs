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
use crate::report::{self, BinaryResult, Failure, Measurement, Report, StepTime};
use crate::sandbox_phase::{self, Exec, StepRun};
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
/// Setup, status, selfcheck, the re-runs and the removal take a few minutes.
const SECS_SANDBOX_USER: f64 = 600.0;
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

/// The working folder of a test binary: its package folder under `root`.
fn package_cwd(root: &Path, r: &TestRun) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in r.package_dir.split(['/', '\\']).filter(|s| !s.is_empty()) {
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

/// Time limit for one solo re-run of a test after its binary hung or crashed.
const SOLO_RERUN_SECS: u64 = 120;
/// Most tests re-run alone per binary; the rest are listed in the report.
const MAX_SOLO_RERUNS: usize = 30;
/// Total time for all solo re-runs of one binary. Re-runs that would start
/// after it is spent are listed in the report as skipped.
const SOLO_RERUN_BUDGET_SECS: u64 = 600;

struct Env {
    /// Variables every child gets.
    vars: Vec<(String, String)>,
    /// Add `--show-output` to the full run of a test binary, so libtest
    /// prints what passing tests printed and their `measurement:` lines
    /// reach the report. Off for the baseline, which stays as earlier runs
    /// ran it.
    show_output: bool,
}

/// The variable that makes the sandbox user mandatory in the sandbox test
/// binaries (`tests/common/mod.rs` of kgraf-sandbox): a test that would take
/// the token sandbox's branch fails instead of passing.
pub const REQUIRE_SANDBOX_USER_VAR: &str = "KGRAF_REQUIRE_SANDBOX_USER";

/// The environment of the re-run (e) of the sandbox user phase: the
/// baseline's, plus [`REQUIRE_SANDBOX_USER_VAR`]` = 1` and `--show-output`.
/// The variable is set here and nowhere else: `e2e::child_env` keeps every
/// `KGRAF_` variable of the host from the children, so neither a value the
/// host has nor this one reaches the baseline.
fn phase_rerun_env(base: &Env) -> Env {
    let mut vars = base.vars.clone();
    vars.push((REQUIRE_SANDBOX_USER_VAR.to_owned(), "1".to_owned()));
    Env {
        vars,
        show_output: true,
    }
}

/// The test binaries that run with a console of their own
/// (`CREATE_NEW_CONSOLE`). The runner reads every child through pipes, so a
/// child has no console unless it gets one. `escape_windows` measures that a
/// sandboxed command cannot attach to its host's console, and with no
/// console there is nothing to attach to: that test then fails under
/// [`REQUIRE_SANDBOX_USER_VAR`] rather than pass without measuring. A new
/// console keeps the pipes (the standard handles are passed explicitly),
/// and on a desktop it shows an empty window while the binary runs.
pub const NEEDS_CONSOLE: &[&str] = &["kgraf-sandbox/test/escape_windows"];

/// `CREATE_NEW_CONSOLE`.
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

/// The process creation flags for a run of the test binary `id`.
pub fn creation_flags_for(id: &str) -> u32 {
    let base = id.split('#').next().unwrap_or(id);
    if NEEDS_CONSOLE.contains(&base) {
        CREATE_NEW_CONSOLE
    } else {
        0
    }
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
    Env {
        vars,
        show_output: false,
    }
}

/// Run one test binary and read its results.
/// The opt-in of the sandbox user setup test. Every other `KGRAF_` variable
/// of the host is kept from the tests; this one is passed on when the host
/// sets it (a throwaway, elevated runner), so the test may create its tagged
/// users and firewall rules there and nowhere else.
const SETUP_TEST_VAR: &str = "KGRAF_SANDBOX_SETUP_TEST";

/// The opt-in variables a test receives, from the host's value of
/// [`SETUP_TEST_VAR`].
fn opt_ins(setup_test: Option<&str>) -> Vec<(String, String)> {
    match setup_test {
        Some("1") => vec![(SETUP_TEST_VAR.to_owned(), "1".to_owned())],
        _ => Vec::new(),
    }
}

/// The command for one run of a test binary, with the environment and working
/// folder every run of that binary uses.
fn binary_command(run: &TestRun, bundle: &Path, cwd: &Path, env: &Env, args: &[String]) -> Command {
    let mut cmd = Command::new(bundle.join(&run.exe));
    let mut vars = env.vars.clone();
    vars.push(("CARGO_MANIFEST_DIR".into(), cwd.display().to_string()));
    vars.push(("CARGO_PKG_NAME".into(), run.package.clone()));
    vars.extend(opt_ins(std::env::var(SETUP_TEST_VAR).ok().as_deref()));
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(e2e::child_env(std::env::vars(), &vars));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(creation_flags_for(&run.id));
    }
    cmd
}

/// The arguments of the full run of a binary in `env`.
fn full_run_args(run: &TestRun, env: &Env) -> Vec<String> {
    let mut args = test_args(run);
    if env.show_output {
        args.push("--show-output".to_owned());
    }
    args
}

/// libtest prints a failed test's panic and captured output only in the
/// summary at the end, which a killed binary never reaches. Run `name` alone
/// to get that output. The first run's outcome is not changed.
fn solo_rerun(
    run: &TestRun,
    bundle: &Path,
    cwd: &Path,
    env: &Env,
    name: &str,
    limit: Duration,
) -> Failure {
    let args = ["--exact", name, "--test-threads=1"].map(String::from);
    let f = crate::proc::run(
        binary_command(run, bundle, cwd, env, &args),
        limit,
        &CANCEL,
        |_| {},
    );
    let mut parser = Parser::default();
    for l in &f.tail {
        parser.feed(l);
    }
    parser.finish();
    let solo = if f.timed_out {
        "hung"
    } else if f.start_error.is_none()
        && f.code == Some(0)
        && parser.tests.iter().any(|t| t.outcome == Outcome::Passed)
    {
        "passed"
    } else {
        "failed"
    };
    let tail = match &f.start_error {
        Some(e) => format!("could not start: {e}"),
        None => f.tail_text(60),
    };
    Failure {
        test: name.to_owned(),
        class: report::classify_failure(&tail),
        tail,
        solo: Some(solo.to_owned()),
    }
}

fn run_binary(
    console: &Console,
    run: &TestRun,
    bundle: &Path,
    cwd: &Path,
    env: &Env,
    expected: f64,
    listed: Option<u64>,
) -> BinaryResult {
    run_binary_limits(
        console,
        run,
        bundle,
        cwd,
        env,
        expected,
        listed,
        limit_for(expected),
        Duration::from_secs(SOLO_RERUN_SECS),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_binary_limits(
    console: &Console,
    run: &TestRun,
    bundle: &Path,
    cwd: &Path,
    env: &Env,
    expected: f64,
    listed: Option<u64>,
    limit: Duration,
    solo_limit: Duration,
) -> BinaryResult {
    let cmd = binary_command(run, bundle, cwd, env, &full_run_args(run, env));
    let mut parser = Parser::default();
    let total = listed.unwrap_or(0);
    // Each test's time: libtest on stable prints no times of its own. With
    // one test thread a test starts when the one before it ends, and its
    // `test name ... ` line usually reaches the pipe only with its result
    // word, so the time runs from the previous result (or the binary's
    // start) to this one; a `test name ... ` line of its own, when one
    // arrives, restarts it.
    let mut mark = Instant::now();
    let mut test_secs: Vec<(String, f64)> = Vec::new();
    let f = crate::proc::run(cmd, limit, &CANCEL, |line| {
        let ev = parser.feed(line);
        match &ev {
            Event::Started(_) => mark = Instant::now(),
            Event::Finished(_) => {
                if let Some(t) = parser.tests.last() {
                    test_secs.push((t.name.clone(), mark.elapsed().as_secs_f64()));
                }
                mark = Instant::now();
            }
            Event::Other => {}
        }
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
    let mut failures: Vec<Failure> = parser
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
                solo: None,
            }
        })
        .collect();
    let ended_cleanly = parser.summary.is_some() && f.code == Some(0);
    let mut open_test_solo = None;
    let mut rerun_skipped = Vec::new();
    if !ended_cleanly && !f.cancelled && f.start_error.is_none() {
        // The open test first: it is the likely cause.
        let mut budget = MAX_SOLO_RERUNS;
        let started = std::time::Instant::now();
        let spent = || started.elapsed() >= Duration::from_secs(SOLO_RERUN_BUDGET_SECS);
        if let Some(name) = &parser.open {
            open_test_solo = Some(solo_rerun(run, bundle, cwd, env, name, solo_limit));
            budget -= 1;
        }
        for fail in &mut failures {
            if budget == 0 || spent() {
                rerun_skipped.push(fail.test.clone());
                continue;
            }
            if CANCEL.load(Ordering::Relaxed) {
                break;
            }
            budget -= 1;
            let s = solo_rerun(run, bundle, cwd, env, &fail.test, solo_limit);
            fail.class = s.class;
            fail.tail = s.tail;
            fail.solo = s.solo;
        }
    }
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
        open_test_solo,
        rerun_skipped,
        tail: (!ended_cleanly).then(|| f.tail_text(60)),
        skipped: run
            .skips
            .iter()
            .map(|s| format!("{}: {}", s.pattern, s.reason))
            .collect(),
        measurements: measurements(&parser.measurements, &test_secs),
    }
}

/// The measurement lines of one run, each with its test's time when the
/// runner timed that test.
fn measurements(lines: &[(Option<String>, String)], secs: &[(String, f64)]) -> Vec<Measurement> {
    lines
        .iter()
        .map(|(test, line)| Measurement {
            test: test.clone(),
            secs: test
                .as_ref()
                .and_then(|t| secs.iter().find(|(n, _)| n == t).map(|(_, s)| *s)),
            line: line.clone(),
        })
        .collect()
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
    let cwd_for = |r: &TestRun| package_cwd(&cwd_root, r);

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
    if o.sandbox_user {
        console.with(|p, _| p.push("sandbox user".into(), SECS_SANDBOX_USER));
    }
    console.with(|p, _| p.push("cleanup".into(), SECS_CLEANUP));
    let step_e2e = 3 + core.len();
    let first_ext = step_e2e + 1;
    let step_sandbox = if run_extended {
        first_ext + extended.len()
    } else {
        first_ext
    };
    let step_cleanup = step_sandbox + usize::from(o.sandbox_user);
    let chosen: Vec<&TestRun> = core
        .iter()
        .copied()
        .chain(extended.iter().copied().filter(|_| run_extended))
        .collect();
    let expected_all: Vec<f64> = all
        .iter()
        .enumerate()
        .map(|(i, r)| expected_of(i, r))
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

    // The sandbox user phase: after the baseline, before the cleanup.
    let sandbox_user = if o.sandbox_user {
        console.with(|p, now| p.start(step_sandbox, now));
        let by_id: std::collections::HashMap<&str, (f64, Option<u64>)> = all
            .iter()
            .enumerate()
            .map(|(i, r)| {
                (
                    r.id.as_str(),
                    (expected_all[i], listed.get(i).copied().flatten()),
                )
            })
            .collect();
        let mut ex = PhaseExec {
            console: &console,
            bundle: &bundle,
            layout: &layout,
            env: &env,
            rerun_env: phase_rerun_env(&env),
            machine: &machine,
            runs: &all,
            by_id,
            cwd_root: &cwd_root,
            placed: None,
        };
        let phase = sandbox_phase::run_phase(&mut ex, &binaries, machine.elevated, &|| {
            CANCEL.load(Ordering::Relaxed)
        });
        console.log(&format!("Sandbox user phase: {}", phase.summary()));
        console.with(|p, now| p.finish(step_sandbox, now));
        Some(phase)
    } else {
        None
    };

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
        sandbox_user,
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
    let left = r.sandbox_user.as_ref().is_some_and(|p| p.left_behind());
    i32::from(!(t.failed == 0 && t.hung_or_crashed == 0 && t.e2e_fail == 0) || left)
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
        // The baseline runs before setup, under the token sandbox, which
        // leaves the network open.
        network_open: true,
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

/// The real [`Exec`] of the sandbox user phase. Every process goes through
/// `proc::run`, and the re-runs through `run_binary` and the end-to-end
/// runner: the baseline's own paths.
struct PhaseExec<'a> {
    console: &'a Console,
    bundle: &'a Path,
    layout: &'a Layout,
    env: &'a Env,
    /// The environment of the re-run (e): [`phase_rerun_env`].
    rerun_env: Env,
    machine: &'a MachineFacts,
    runs: &'a [&'a TestRun],
    /// Expected seconds and listed test count by run id.
    by_id: std::collections::HashMap<&'a str, (f64, Option<u64>)>,
    cwd_root: &'a Path,
    /// The runner copy placed beside the test binaries, removed at the end.
    placed: Option<PathBuf>,
}

impl PhaseExec<'_> {
    fn kgraf(&self) -> PathBuf {
        self.bundle.join("programs").join("kgraf.exe")
    }
}

impl Exec for PhaseExec<'_> {
    fn preflight(&mut self) -> Result<(), String> {
        if !cfg!(windows) {
            return Err("the sandbox user exists only on Windows".into());
        }
        // The phase ends with `remove`: it must not remove a setup the person
        // made before the run.
        if let Some(store) = kgraf_sandbox::sandbox_user::credentials_path(None)
            && store.exists()
        {
            return Err(format!(
                "this Windows user already has a sandbox user set up (stored passwords at {}); \
                 the phase would remove it. Run `kgraf sandbox remove` first to let the phase run",
                store.display()
            ));
        }
        let runner = format!("{}.exe", kgraf_sandbox::sandbox_user::runner::RUNNER_NAME);
        let built = self.bundle.join("programs").join(&runner);
        if !self.kgraf().is_file() || !built.is_file() {
            return Err(format!(
                "the bundle has no programs/kgraf.exe and programs/{runner} side by side"
            ));
        }
        // On the owner-session path kGraf starts the runner beside the
        // running executable. The test binaries are in `tests/`, so that path
        // needs a copy there. The session-0 path never uses the copy: it
        // names the runner the launcher service was installed with, the one
        // in `programs/`, so the copy's owner and the trust check of the
        // service's files do not meet.
        let copy = self.bundle.join("tests").join(&runner);
        if !copy.is_file() {
            std::fs::copy(&built, &copy)
                .map_err(|e| format!("cannot place {runner} beside the test binaries: {e}"))?;
            self.placed = Some(copy);
        }
        // The re-run starts these binaries as the sandbox user. A folder the
        // sandbox group cannot read makes every probe fail before it checks
        // anything, so the phase does not start. The runner copy is checked
        // with the rest, and removed again here: a phase that does not start
        // never reaches its teardown.
        let readable = platform::sandbox_group_can_run(&self.bundle.join("tests"));
        if readable.is_err()
            && let Some(copy) = self.placed.take()
        {
            let _ = std::fs::remove_file(copy);
        }
        readable
    }

    fn cli(&mut self, label: &str, args: &[&str], limit: Duration, honor_cancel: bool) -> StepRun {
        self.console.log(&format!("  sandbox user {label}"));
        let mut cmd = Command::new(self.kgraf());
        cmd.args(args)
            .current_dir(&self.layout.e2e)
            .env_clear()
            .envs(e2e::child_env(std::env::vars(), &self.env.vars));
        let never = std::sync::atomic::AtomicBool::new(false);
        let cancel = if honor_cancel { &CANCEL } else { &never };
        let f = crate::proc::run(cmd, limit, cancel, |_| {});
        StepRun {
            label: label.to_owned(),
            command: format!("kgraf {}", args.join(" ")),
            code: f.code,
            timed_out: f.timed_out,
            cancelled: f.cancelled,
            start_error: f.start_error.clone(),
            secs: f.secs,
            output: f.tail.join("\n"),
        }
    }

    fn rerun(&mut self, id: &str) -> BinaryResult {
        self.console
            .log(&format!("  sandbox user: running {id} again"));
        let Some(run) = self.runs.iter().find(|r| r.id == id) else {
            return BinaryResult {
                id: id.to_owned(),
                set: Set::Core,
                exit_code: None,
                timed_out: false,
                cancelled: false,
                start_error: Some("not in the manifest".into()),
                secs: 0.0,
                expected_secs: 0.0,
                listed: None,
                passed: 0,
                failed: 0,
                ignored: 0,
                tests: Vec::new(),
                failures: Vec::new(),
                open_test: None,
                open_test_solo: None,
                rerun_skipped: Vec::new(),
                tail: None,
                skipped: Vec::new(),
                measurements: Vec::new(),
            };
        };
        let (expected, listed) = self.by_id.get(id).copied().unwrap_or((30.0, None));
        run_binary(
            self.console,
            run,
            self.bundle,
            &package_cwd(self.cwd_root, run),
            &self.rerun_env,
            expected,
            listed,
        )
    }

    fn e2e(&mut self) -> Vec<e2e::Check> {
        let dir = self.layout.e2e.join("sandbox-user");
        let _ = std::fs::create_dir_all(&dir);
        let ctx = e2e::Ctx {
            kgraf: self.kgraf(),
            dir,
            env: self.env.vars.clone(),
            git_bash_found: self.machine.git_bash_path.is_some(),
            // The re-run's checks run as the sandbox user, which denies the
            // network: no shell result may say it is open.
            network_open: false,
            cancel: &CANCEL,
        };
        e2e::run_all(&ctx, cfg!(windows))
    }

    fn grant_named_objects(&mut self) -> StepRun {
        let label = "(b2) fallback grant (elevated action grant-named-objects)";
        let command = "kgraf sandbox elevated --action grant-named-objects".to_owned();
        self.console.log(&format!("  sandbox user {label}"));
        let started = Instant::now();
        #[cfg(windows)]
        let (code, start_error, output) = {
            use kgraf_sandbox::sandbox_user::OwnerNames;
            use kgraf_sandbox::sandbox_user::windows::{accounts, setup};
            let mut out = Vec::new();
            let exe = self.kgraf().to_string_lossy().into_owned();
            let result: Result<(), String> = (|| {
                let sid = accounts::current_user_sid().map_err(|e| e.to_string())?;
                let names = OwnerNames::derive(&sid, None).map_err(|e| e.to_string())?;
                setup::grant_named_objects(
                    &names,
                    &setup::Launch {
                        exe: &exe,
                        subcommand: "sandbox elevated",
                    },
                    &mut |e| out.push(format!("{e:?}")),
                )
                .map_err(|e| e.to_string())
            })();
            match result {
                Ok(()) => (Some(0), None, out.join("\n")),
                Err(e) => (Some(1), None, format!("{}\n{e}", out.join("\n"))),
            }
        };
        #[cfg(not(windows))]
        let (code, start_error, output) = (
            None,
            Some("the fallback grant exists only on Windows".to_owned()),
            String::new(),
        );
        StepRun {
            label: label.to_owned(),
            command,
            code,
            start_error,
            secs: started.elapsed().as_secs_f64(),
            output,
            ..StepRun::default()
        }
    }

    fn object_directory(&mut self) -> sandbox_phase::DirectoryRead {
        #[cfg(windows)]
        {
            use kgraf_sandbox::sandbox_user::windows::named_object_dir as dir;
            let session = dir::session_of_current_process().map_err(|e| e.to_string());
            let access_list = match &session {
                Ok(n) => dir::read_sddl(*n).map_err(|e| e.to_string()),
                Err(e) => Err(e.clone()),
            };
            sandbox_phase::DirectoryRead {
                session,
                access_list,
            }
        }
        #[cfg(not(windows))]
        sandbox_phase::DirectoryRead {
            session: Err("this is not a Windows host".into()),
            access_list: Err("this is not a Windows host".into()),
        }
    }

    fn teardown(&mut self) -> Vec<String> {
        match self.placed.take() {
            Some(p) => {
                let ok = std::fs::remove_file(&p).is_ok();
                vec![format!(
                    "runner copy placed beside the test binaries: removed {ok}"
                )]
            }
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use crate::progress::Progress;

    #[test]
    fn the_setup_test_opt_in_passes_only_when_the_host_sets_it() {
        assert_eq!(
            super::opt_ins(Some("1")),
            vec![("KGRAF_SANDBOX_SETUP_TEST".to_owned(), "1".to_owned())]
        );
        assert!(super::opt_ins(Some("0")).is_empty());
        assert!(super::opt_ins(None).is_empty());
    }

    use super::*;
    use crate::manifest::Skip;

    /// A fake libtest binary: the full run hangs at `hang::open` after two
    /// failures and prints no failure output; a solo run prints it.
    #[cfg(unix)]
    #[test]
    fn a_hung_binary_gets_solo_reruns_for_its_failed_and_open_tests() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("fake-tests");
        let script = r#"#!/bin/sh
if [ "$1" = "--exact" ]; then
  case "$2" in
    a::fails)
      echo 'running 1 test'
      echo 'test a::fails ... FAILED'
      echo
      echo 'failures:'
      echo
      echo '---- a::fails stdout ----'
      echo 'boom: the real reason'
      echo
      echo 'failures:'
      echo '    a::fails'
      echo 'test result: FAILED. 0 passed; 1 failed; 0 ignored'
      exit 101;;
    b::fails)
      echo 'running 1 test'
      echo 'test b::fails ... ok'
      echo 'test result: ok. 1 passed; 0 failed; 0 ignored'
      exit 0;;
    hang::open)
      echo 'running 1 test'
      printf 'test hang::open ... \n'
      exec sleep 30;;
  esac
fi
echo 'running 4 tests'
echo 'test a::ok ... ok'
echo 'test a::fails ... FAILED'
echo 'test b::fails ... FAILED'
printf 'test hang::open ... \n'
exec sleep 30
"#;
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = TestRun {
            id: "p/lib/p".into(),
            package: "p".into(),
            kind: "lib".into(),
            name: "p".into(),
            exe: "fake-tests".into(),
            package_dir: ".".into(),
            set: Set::Core,
            filters: vec![],
            skips: vec![],
            secs_per_test: 1.0,
        };
        let console = Console::new(Progress::default());
        let env = Env {
            vars: vec![],
            show_output: false,
        };
        let b = run_binary_limits(
            &console,
            &run,
            dir.path(),
            dir.path(),
            &env,
            1.0,
            Some(4),
            Duration::from_millis(1500),
            Duration::from_millis(1500),
        );
        // The first run's outcomes stand.
        assert!(b.timed_out);
        assert_eq!(b.failed, 2);
        assert_eq!(b.open_test.as_deref(), Some("hang::open"));
        let a = b.failures.iter().find(|f| f.test == "a::fails").unwrap();
        assert!(a.tail.contains("boom: the real reason"), "{}", a.tail);
        assert_eq!(a.solo.as_deref(), Some("failed"));
        let bf = b.failures.iter().find(|f| f.test == "b::fails").unwrap();
        assert_eq!(bf.solo.as_deref(), Some("passed"));
        let open = b.open_test_solo.as_ref().unwrap();
        assert_eq!(open.solo.as_deref(), Some("hung"));
        assert!(b.rerun_skipped.is_empty());
    }

    fn run_of(exe: &str, id: &str) -> TestRun {
        TestRun {
            id: id.into(),
            package: "kgraf-sandbox".into(),
            kind: "test".into(),
            name: "x".into(),
            exe: exe.into(),
            package_dir: ".".into(),
            set: Set::Core,
            filters: vec![],
            skips: vec![],
            secs_per_test: 1.0,
        }
    }

    fn env_of(cmd: &Command, name: &str) -> Option<String> {
        cmd.get_envs()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    }

    /// The switch reaches the command of a re-run binary and not the
    /// command of the baseline. The commands checked are the ones the runner
    /// starts, after `e2e::child_env` filtered the host's variables: a
    /// filtered variable once skipped a test without a word.
    #[test]
    fn the_sandbox_user_switch_reaches_the_rerun_and_not_the_baseline() {
        let base = Env {
            vars: vec![("TEMP".into(), "t".into())],
            show_output: false,
        };
        let rerun = phase_rerun_env(&base);
        let run = run_of("x", "kgraf-sandbox/test/escape_windows");
        let p = Path::new("b");
        let baseline_cmd = binary_command(&run, p, p, &base, &full_run_args(&run, &base));
        let rerun_cmd = binary_command(&run, p, p, &rerun, &full_run_args(&run, &rerun));
        assert_eq!(env_of(&baseline_cmd, REQUIRE_SANDBOX_USER_VAR), None);
        assert_eq!(
            env_of(&rerun_cmd, REQUIRE_SANDBOX_USER_VAR).as_deref(),
            Some("1")
        );
        // The re-run asks libtest for passing tests' output; the baseline
        // does not.
        let args = |c: &Command| -> Vec<String> {
            c.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        assert!(args(&rerun_cmd).contains(&"--show-output".to_owned()));
        assert!(!args(&baseline_cmd).contains(&"--show-output".to_owned()));
        // A value on the host never reaches a child: the filter drops it, so
        // the baseline cannot inherit one from the shell that ran the runner.
        let host = vec![(REQUIRE_SANDBOX_USER_VAR.to_owned(), "1".to_owned())];
        assert!(
            e2e::child_env(host.clone(), &base.vars)
                .iter()
                .all(|(k, _)| k != REQUIRE_SANDBOX_USER_VAR)
        );
        assert!(
            e2e::child_env(host, &rerun.vars)
                .iter()
                .any(|(k, v)| k == REQUIRE_SANDBOX_USER_VAR && v == "1")
        );
        // The name is the one the sandbox tests read (tests/common/mod.rs).
        assert_eq!(REQUIRE_SANDBOX_USER_VAR, "KGRAF_REQUIRE_SANDBOX_USER");
    }

    /// The same, through a real process: a fake test binary prints the
    /// switch and its arguments as a measurement line of a passing test, the
    /// way `--show-output` prints it, and the runner reads the line back
    /// with the test's name and time.
    #[cfg(unix)]
    #[test]
    fn a_rerun_binary_gets_the_switch_and_its_measurements_are_collected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("fake-tests");
        let script = r#"#!/bin/sh
echo 'running 2 tests'
sleep 1.5
echo 'test e::quick ... ok'
sleep 0.3
echo 'test e::probe ... ok'
echo
echo 'successes:'
echo
echo '---- e::probe stdout ----'
echo "measurement: switch=${KGRAF_REQUIRE_SANDBOX_USER:-unset} args=$*"
echo
echo 'successes:'
echo '    e::probe'
echo
echo 'test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.20s'
"#;
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = run_of("fake-tests", "p/test/fake");
        let console = Console::new(Progress::default());
        let base = Env {
            vars: vec![],
            show_output: false,
        };
        let go = |env: &Env| {
            run_binary_limits(
                &console,
                &run,
                dir.path(),
                dir.path(),
                env,
                1.0,
                Some(1),
                Duration::from_secs(20),
                Duration::from_secs(20),
            )
        };
        let b = go(&base);
        assert!(b.clean(), "{b:?}");
        assert_eq!(b.measurements.len(), 1, "{:?}", b.measurements);
        assert!(
            b.measurements[0].line.contains("switch=unset"),
            "{:?}",
            b.measurements
        );
        assert!(!b.measurements[0].line.contains("--show-output"));
        let r = go(&phase_rerun_env(&base));
        assert!(r.clean(), "{r:?}");
        let m = &r.measurements[0];
        assert!(m.line.contains("switch=1"), "{m:?}");
        assert!(m.line.contains("--show-output"), "{m:?}");
        assert_eq!(m.test.as_deref(), Some("e::probe"));
        // libtest prints `test name ... ok` in one piece when the test
        // ends; the time runs from the result of the test before it.
        let secs = m.secs.expect("the test was timed");
        // Not the 1.5 s of the test before it: the time restarts at each
        // result.
        assert!((0.25..1.2).contains(&secs), "{secs}");
        // The phase's own re-run path, as the runner builds it from the
        // baseline's environment.
        let layout = workdir::layout(dir.path(), "phase");
        let machine = MachineFacts::default();
        let runs = [&run];
        let mut ex = PhaseExec {
            console: &console,
            bundle: dir.path(),
            layout: &layout,
            env: &base,
            rerun_env: phase_rerun_env(&base),
            machine: &machine,
            runs: &runs,
            by_id: std::collections::HashMap::new(),
            cwd_root: dir.path(),
            placed: None,
        };
        let r = ex.rerun("p/test/fake");
        assert!(
            r.measurements[0].line.contains("switch=1"),
            "{:?}",
            r.measurements
        );
    }

    /// The escape tests get a console of their own (they measure an attach
    /// to it); no other binary does.
    #[test]
    fn only_the_escape_tests_get_a_console_of_their_own() {
        assert_eq!(
            creation_flags_for("kgraf-sandbox/test/escape_windows"),
            CREATE_NEW_CONSOLE
        );
        assert_eq!(
            creation_flags_for("kgraf-sandbox/test/escape_windows#core"),
            CREATE_NEW_CONSOLE
        );
        for id in [
            "kgraf-sandbox/test/runner_windows",
            "kgraf-sandbox/lib/kgraf_sandbox",
            "kgraf-cli/test/worker",
        ] {
            assert_eq!(creation_flags_for(id), 0, "{id}");
        }
        assert_eq!(CREATE_NEW_CONSOLE, 0x10);
    }

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
