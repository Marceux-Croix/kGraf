//! The Windows sandbox user setup end to end (decision 0097): `kgraf sandbox
//! setup`, `status`, a second `setup`, `remove`, `status`, with a test tag so
//! the users and rules have names of their own.
//!
//! It creates local accounts and firewall rules, so it runs only when the test
//! process is already elevated (as on a hosted Windows runner) and
//! `KGRAF_SANDBOX_SETUP_TEST=1` is set; otherwise it prints why and returns.
//! An elevated process starts the elevated step without a UAC prompt.
//!
//! Setup installs kGraf's launcher service, which runs as LocalSystem, and it
//! refuses to install it from a program file, or a folder above one, that
//! anyone but an administrator may write (decision 0097). A test build's
//! `kgraf.exe` sits in a build or work folder its own user may write, so the
//! test runs a copy of it and of the runner, placed in a folder on the system
//! drive whose access list admits writes by SYSTEM and the Administrators
//! group only, owned by that group, as an installed kGraf would be.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// High mandatory integrity level: the process runs elevated.
const HIGH_INTEGRITY: &str = "S-1-16-12288";

fn elevated() -> bool {
    Command::new("whoami")
        .arg("/groups")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(HIGH_INTEGRITY))
}

/// The runner the launcher service starts, which setup looks for beside
/// `kgraf.exe`.
const RUNNER: &str = "kgraf-sandbox-runner.exe";

/// The copy of `kgraf.exe` and the runner setup may install the launcher
/// service from, removed when dropped.
struct Installed {
    dir: PathBuf,
}

impl Installed {
    fn kgraf(&self) -> PathBuf {
        self.dir.join("kgraf.exe")
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        // Runs after `remove`, which stops and deletes the service, so no
        // process holds the files. A folder left behind is named.
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            eprintln!("could not remove {}: {e}", self.dir.display());
        }
    }
}

fn icacls(dir: &Path, args: &[&str]) {
    let out = Command::new("icacls")
        .arg(dir)
        .args(args)
        .output()
        .expect("run icacls");
    assert!(
        out.status.success(),
        "icacls {} {args:?}: {}",
        dir.display(),
        text(&out)
    );
}

/// Copy the build's `kgraf.exe` and the runner beside it into a new folder
/// on the system drive that only SYSTEM and the Administrators group may
/// write (Users may read and run), owned by that group, with nothing
/// inherited: the shape setup's check of the service's program accepts.
fn install(tag: &str) -> Installed {
    let built = PathBuf::from(env!("CARGO_BIN_EXE_kgraf"));
    let beside = built.with_file_name(RUNNER);
    let runner = if beside.is_file() {
        beside
    } else {
        // A test binary's deps folder: the bins are one folder up.
        built
            .parent()
            .and_then(Path::parent)
            .map(|p| p.join(RUNNER))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| panic!("{RUNNER} is not built beside {}", built.display()))
    };
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let dir = PathBuf::from(format!("{drive}\\kgraf-setup-test-{tag}"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("remove a folder an earlier run left");
    }
    std::fs::create_dir(&dir).expect("make the program folder");
    let installed = Installed { dir };
    icacls(
        &installed.dir,
        &[
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
            "*S-1-5-32-545:(OI)(CI)RX",
        ],
    );
    std::fs::copy(&built, installed.kgraf()).expect("copy kgraf.exe");
    std::fs::copy(&runner, installed.dir.join(RUNNER)).expect("copy the runner");
    // The owner may rewrite an access list whatever it says, and the files
    // were made by this account: the owner becomes the Administrators group.
    icacls(&installed.dir, &["/setowner", "*S-1-5-32-544", "/T", "/C"]);
    installed
}

fn kgraf_at(exe: &Path, args: &[&str]) -> Output {
    Command::new(exe).args(args).output().expect("run kgraf")
}

fn text(o: &Output) -> String {
    format!(
        "exit {:?}\n{}{}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn setup_status_again_remove_leave_nothing_behind() {
    if std::env::var("KGRAF_SANDBOX_SETUP_TEST").as_deref() != Ok("1") {
        eprintln!("skipped: set KGRAF_SANDBOX_SETUP_TEST=1 to create test sandbox users");
        return;
    }
    if !elevated() {
        eprintln!("skipped: the setup test needs an elevated test process");
        return;
    }
    let tag = format!("t{:x}", std::process::id() % 0xffff);
    let t = ["--tag", tag.as_str()];
    let installed = install(&tag);
    let exe = installed.kgraf();
    let kgraf = |args: &[&str]| kgraf_at(&exe, args);

    let before = kgraf(&[&["sandbox", "status"][..], &t].concat());
    assert!(
        !before.status.success(),
        "nothing set up yet: {}",
        text(&before)
    );

    let setup = kgraf(&[&["sandbox", "setup"][..], &t].concat());
    let removed_anyway = || kgraf(&[&["sandbox", "remove"][..], &t].concat());
    // Each panic message ends with the step that failed: a report keeps the
    // tail of the output, and the removal's long output must not push the
    // reason out of it.
    if !setup.status.success() {
        let r = removed_anyway();
        panic!(
            "remove, after the failure:\n{}\nsetup failed:\n{}",
            text(&r),
            text(&setup)
        );
    }
    let out = text(&setup);
    assert!(out.contains("created"), "{out}");

    let status = kgraf(&[&["sandbox", "status"][..], &t].concat());
    let st = text(&status);
    // A DNS filter Windows refused at setup fails status with a FAILED line
    // and says so; that gap is reported, not hidden, and is not what this
    // test checks. Any other failing line fails the test.
    if !status.status.success() && !only_refused_dns_filters(&st) {
        let r = removed_anyway();
        panic!(
            "remove, after the failure:\n{}\nstatus after setup failed:\n{st}",
            text(&r)
        );
    }
    if !status.status.success() {
        eprintln!("DNS is not confined on this machine (status says why):\n{st}");
    }
    assert!(st.contains("set up"), "{st}");

    let again = kgraf(&[&["sandbox", "setup"][..], &t].concat());
    let ag = text(&again);
    assert!(again.status.success(), "{ag}");
    assert!(ag.contains("updated"), "a second setup refreshes: {ag}");

    let remove = removed_anyway();
    assert!(remove.status.success(), "{}", text(&remove));

    let after = kgraf(&[&["sandbox", "status"][..], &t].concat());
    let af = text(&after);
    assert!(!after.status.success(), "everything removed: {af}");
    // A failing status says only that something is missing. Nothing setup
    // made may still be present: a removal that dropped record lines for
    // users it did not read once left every slot user behind while this
    // check passed.
    let left = left_behind(&af);
    assert!(left.is_empty(), "left behind after remove: {left:?}\n{af}");
}

/// Whether a failing status fails only for DNS filters Windows refused at
/// setup: its closing sentence says so, and every line that is not `present`
/// or `unknown` is a `FAILED` DNS filter line.
fn only_refused_dns_filters(status: &str) -> bool {
    status.contains("running setup again does not change that.")
        && status
            .lines()
            .map(str::trim)
            .filter(|l| {
                ["MISSING", "WRONG", "FAILED"]
                    .iter()
                    .any(|w| l.starts_with(w))
            })
            .all(|l| l.starts_with("FAILED") && l.contains("DNS filter"))
}

/// The `present` lines of a status report that name something setup makes,
/// not a fact of the machine's firewall.
fn left_behind(status: &str) -> Vec<&str> {
    const MACHINE_FACTS: [&str; 2] = [
        "Windows Firewall on for every profile",
        "local firewall rules take effect",
    ];
    status
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("present"))
        .map(str::trim)
        .filter(|item| !MACHINE_FACTS.contains(item))
        .collect()
}
