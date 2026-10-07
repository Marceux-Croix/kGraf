//! Finding, and offering to install, Git for Windows.
//!
//! On Windows kGraf runs `sh` and `bash` through Git Bash, the `bin\bash.exe`
//! of Git for Windows, and every project needs git. This module holds the
//! search, the `winget` command line and the consent flow as pure functions
//! and a [`Probe`] trait, so they are tested on every platform. The real
//! [`SystemProbe`] (registry, `PATH`, `winget`) exists on Windows only.
//!
//! Git for Windows is never bundled with kGraf: it is a separate package
//! under its own license. kGraf installs it only after a yes from the user.

use std::path::{Path, PathBuf};

/// Names the Git Bash to run (its `bin\bash.exe`), in place of the search.
pub const GIT_BASH_PATH_VAR: &str = "KGRAF_GIT_BASH_PATH";

/// Where a person downloads Git for Windows by hand.
pub const DOWNLOAD_URL: &str = "https://gitforwindows.org/";

/// The registry key Git for Windows writes, under `HKLM` for an all-users
/// install and `HKCU` for a per-user one. Its `InstallPath` value is the
/// install folder (the one holding `bin\bash.exe`).
pub const REGISTRY_KEY: &str = r"SOFTWARE\GitForWindows";
/// The value of [`REGISTRY_KEY`] that holds the install folder.
pub const REGISTRY_VALUE: &str = "InstallPath";

/// The arguments of the `winget` install. User scope needs no elevation;
/// the agreement flags keep it from stopping at a prompt that has no window.
pub fn winget_install_args() -> [&'static str; 10] {
    [
        "install",
        "--id",
        "Git.Git",
        "-e",
        "--scope",
        "user",
        "--silent",
        "--accept-package-agreements",
        "--accept-source-agreements",
        "--disable-interactivity",
    ]
}

/// The same command as one line, for a message the user can copy.
pub fn winget_command_line() -> String {
    let mut line = String::from("winget");
    for arg in winget_install_args() {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// Everything the search reads, gathered by the caller.
#[derive(Debug, Default, Clone)]
pub struct SearchInputs {
    /// [`GIT_BASH_PATH_VAR`], when set and not empty.
    pub override_path: Option<PathBuf>,
    /// The directories of `PATH`.
    pub path_dirs: Vec<PathBuf>,
    /// `%ProgramFiles%`, `%ProgramW6432%`, `%ProgramFiles(x86)%` and
    /// `%LOCALAPPDATA%\Programs`: each may hold `Git\bin\bash.exe`.
    pub install_roots: Vec<PathBuf>,
    /// The `InstallPath` values from the registry: each is a Git folder.
    pub registry_homes: Vec<PathBuf>,
}

/// Registry values to install folders: unset, blank and repeated values drop.
pub fn registry_homes(values: impl IntoIterator<Item = Option<String>>) -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = Vec::new();
    for value in values.into_iter().flatten() {
        let trimmed = value.trim().trim_end_matches(['\\', '/']);
        if trimmed.is_empty() {
            continue;
        }
        let home = PathBuf::from(trimmed);
        if !homes.contains(&home) {
            homes.push(home);
        }
    }
    homes
}

/// Find Git Bash, the `bin\bash.exe` of Git for Windows.
///
/// - The override: used when set, and an error when it is not a file; the
///   search does not run.
/// - The registry: `<InstallPath>\bin\bash.exe`.
/// - Each `PATH` directory holding `git.exe` (Git's `cmd` or `mingw64\bin`):
///   `..\bin\bash.exe` and `..\..\bin\bash.exe` beside it.
/// - Each install root: `Git\bin\bash.exe`.
///
/// `bin\bash.exe` is Git's launcher: it puts `usr\bin` and `mingw64\bin` on
/// `PATH` and passes its command line on unchanged (Git for Windows
/// `git-wrapper.c`). `usr\bin\bash.exe` alone would not find `ls` or `grep`.
/// The WSL `bash.exe` in System32 is never a candidate.
pub fn find_git_bash(
    inputs: &SearchInputs,
    is_file: impl Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    if let Some(path) = &inputs.override_path {
        return if is_file(path) {
            Ok(path.clone())
        } else {
            Err(format!(
                "{GIT_BASH_PATH_VAR} names {}, which is not a file; set it to the bin\\bash.exe \
                 of Git for Windows",
                path.display()
            ))
        };
    }
    let mut looked = Vec::new();
    let from_registry = inputs
        .registry_homes
        .iter()
        .map(|home| home.join("bin").join("bash.exe"));
    let beside_git = inputs
        .path_dirs
        .iter()
        .filter(|dir| is_file(&dir.join("git.exe")))
        .flat_map(|dir| {
            [
                dir.join("..").join("bin").join("bash.exe"),
                dir.join("..").join("..").join("bin").join("bash.exe"),
            ]
        });
    let installed = inputs
        .install_roots
        .iter()
        .map(|root| root.join("Git").join("bin").join("bash.exe"));
    for candidate in from_registry.chain(beside_git).chain(installed) {
        if is_file(&candidate) {
            return Ok(candidate);
        }
        looked.push(candidate.display().to_string());
    }
    Err(format!(
        "on Windows, kGraf runs `sh` and `bash` with Git Bash from Git for Windows \
         ({DOWNLOAD_URL}), and none was found; install Git for Windows, or set \
         {GIT_BASH_PATH_VAR} to its bin\\bash.exe. Looked in: {}",
        if looked.is_empty() {
            "no candidate (no git.exe on PATH and no install folder set)".to_owned()
        } else {
            looked.join(", ")
        }
    ))
}

/// What [`ensure`] needs from the machine. [`SystemProbe`] is the real one.
pub trait Probe {
    /// The Git Bash path, or the search's error text.
    fn find(&self) -> Result<PathBuf, String>;
    /// True when `winget` starts.
    fn winget_available(&self) -> bool;
    /// Runs [`winget_install_args`]; the error text names the failure.
    fn run_winget(&self) -> Result<(), String>;
}

/// How [`ensure`] ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Git Bash was already there.
    Ready(PathBuf),
    /// `winget` installed Git for Windows and the search now finds it.
    Installed(PathBuf),
    /// Git Bash is missing and the user said no; the text says how to fix it.
    Declined(String),
    /// Git Bash is missing and `winget` is not available.
    NoWinget(String),
    /// `winget` ran and failed, or the search still fails after it.
    InstallFailed(String),
}

impl Outcome {
    /// The Git Bash path, or the text to show the user.
    pub fn into_result(self) -> Result<PathBuf, String> {
        match self {
            Outcome::Ready(p) | Outcome::Installed(p) => Ok(p),
            Outcome::Declined(m) | Outcome::NoWinget(m) | Outcome::InstallFailed(m) => Err(m),
        }
    }
}

/// The question put to the user before `winget` runs.
pub fn consent_question() -> String {
    format!(
        "kGraf needs Git for Windows (it provides Git Bash and git), and it was not found.\n\n\
         Install it now with winget for your user account? This runs:\n{}",
        winget_command_line()
    )
}

/// The manual fix, for every case where the install does not run.
fn manual_fix(search_error: &str) -> String {
    format!(
        "{search_error}\n\nTo fix it by hand: download Git for Windows from {DOWNLOAD_URL} and \
         install it, or run:\n  {}\nIf Git Bash is somewhere else, set {GIT_BASH_PATH_VAR} to its \
         bin\\bash.exe. kGraf checks again at its next start.",
        winget_command_line()
    )
}

/// Make sure Git Bash exists. When it does not, `ask` gets
/// [`consent_question`] once; on a yes `winget` installs Git for Windows for
/// the user and the search runs again. Nothing runs without that yes, and no
/// step falls back silently: every miss comes back with the manual fix.
pub fn ensure(probe: &impl Probe, ask: impl FnOnce(&str) -> bool) -> Outcome {
    let search_error = match probe.find() {
        Ok(path) => return Outcome::Ready(path),
        Err(why) => why,
    };
    // An override that names a missing file is the user's own mistake; an
    // install would not fix it, so it is reported, not answered with winget.
    if search_error.starts_with(GIT_BASH_PATH_VAR) {
        return Outcome::Declined(search_error);
    }
    if !probe.winget_available() {
        return Outcome::NoWinget(format!(
            "{}\n\nwinget is not available on this machine.",
            manual_fix(&search_error)
        ));
    }
    if !ask(&consent_question()) {
        return Outcome::Declined(manual_fix(&search_error));
    }
    if let Err(why) = probe.run_winget() {
        return Outcome::InstallFailed(format!(
            "winget could not install Git for Windows: {why}\n\n{}",
            manual_fix(&search_error)
        ));
    }
    match probe.find() {
        Ok(path) => Outcome::Installed(path),
        Err(still) => Outcome::InstallFailed(format!(
            "winget finished, but Git Bash is still not found.\n\n{}",
            manual_fix(&still)
        )),
    }
}

#[cfg(windows)]
pub use system::SystemProbe;

#[cfg(windows)]
mod system {
    use super::{
        GIT_BASH_PATH_VAR, Probe, REGISTRY_KEY, REGISTRY_VALUE, SearchInputs, find_git_bash,
        registry_homes, winget_install_args,
    };
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY,
        RegGetValueW,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// One string value from the 64-bit registry view; `None` when absent.
    fn read_registry_string(root: HKEY, key: &str, value: &str) -> Option<String> {
        let key = wide(key);
        let value = wide(value);
        let flags = RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY;
        let mut bytes: u32 = 0;
        // SAFETY: both names are NUL-terminated; a null data pointer asks for the size only.
        let status = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                value.as_ptr(),
                flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut bytes,
            )
        };
        if status != 0 || bytes < 2 {
            return None;
        }
        let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
        // SAFETY: `buf` holds `bytes` bytes, as the size query returned.
        let status = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                value.as_ptr(),
                flags,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if status != 0 {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    /// The real machine: environment, registry, `winget`.
    #[derive(Debug, Default)]
    pub struct SystemProbe;

    impl SystemProbe {
        /// The search inputs read from this machine now. The registry is read
        /// on every call, so a Git installed while kGraf runs is found without
        /// a restart (the process `PATH` does not change).
        pub fn inputs() -> SearchInputs {
            let env = |v: &str| std::env::var_os(v).map(PathBuf::from);
            SearchInputs {
                override_path: std::env::var_os(GIT_BASH_PATH_VAR)
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from),
                path_dirs: std::env::var_os("PATH")
                    .map(|p| std::env::split_paths(&p).collect())
                    .unwrap_or_default(),
                install_roots: ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
                    .iter()
                    .filter_map(|v| env(v))
                    .chain(env("LOCALAPPDATA").map(|d| d.join("Programs")))
                    .collect(),
                registry_homes: registry_homes([
                    read_registry_string(HKEY_LOCAL_MACHINE, REGISTRY_KEY, REGISTRY_VALUE),
                    read_registry_string(HKEY_CURRENT_USER, REGISTRY_KEY, REGISTRY_VALUE),
                ]),
            }
        }
    }

    impl Probe for SystemProbe {
        fn find(&self) -> Result<PathBuf, String> {
            find_git_bash(&Self::inputs(), Path::is_file)
        }

        fn winget_available(&self) -> bool {
            no_window(std::process::Command::new("winget").arg("--version"))
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }

        fn run_winget(&self) -> Result<(), String> {
            let out = no_window(std::process::Command::new("winget").args(winget_install_args()))
                .output()
                .map_err(|e| format!("winget did not start: {e}"))?;
            if out.status.success() {
                return Ok(());
            }
            let text = String::from_utf8_lossy(&out.stdout);
            let tail: Vec<&str> = text.lines().rev().take(3).collect();
            Err(format!(
                "winget exited with {}; {}",
                out.status,
                tail.into_iter().rev().collect::<Vec<_>>().join(" ")
            ))
        }
    }

    /// No console window for a child of the GUI app.
    fn no_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn exists(files: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |p: &Path| files.iter().any(|f| Path::new(f) == p)
    }

    #[test]
    fn winget_line_is_the_user_scope_silent_install() {
        assert_eq!(
            winget_command_line(),
            "winget install --id Git.Git -e --scope user --silent \
             --accept-package-agreements --accept-source-agreements --disable-interactivity"
        );
    }

    #[test]
    fn registry_values_become_unique_trimmed_folders() {
        let homes = registry_homes([
            Some(r"C:\Program Files\Git\".to_owned()),
            None,
            Some("  ".to_owned()),
            Some(r"C:\Program Files\Git".to_owned()),
            Some(r"D:\Tools\Git".to_owned()),
        ]);
        assert_eq!(
            homes,
            vec![
                PathBuf::from(r"C:\Program Files\Git"),
                PathBuf::from(r"D:\Tools\Git")
            ]
        );
    }

    #[test]
    fn the_registry_folder_is_found_when_nothing_else_is() {
        let inputs = SearchInputs {
            registry_homes: vec![PathBuf::from("D:/Tools/Git")],
            ..SearchInputs::default()
        };
        let found = find_git_bash(&inputs, exists(&["D:/Tools/Git/bin/bash.exe"])).unwrap();
        assert_eq!(found, PathBuf::from("D:/Tools/Git/bin/bash.exe"));
    }

    #[test]
    fn a_stale_registry_entry_falls_through_to_path() {
        let inputs = SearchInputs {
            registry_homes: vec![PathBuf::from("D:/Gone/Git")],
            path_dirs: vec![PathBuf::from("E:/Git/cmd")],
            ..SearchInputs::default()
        };
        let found = find_git_bash(
            &inputs,
            exists(&["E:/Git/cmd/git.exe", "E:/Git/cmd/../bin/bash.exe"]),
        )
        .unwrap();
        assert_eq!(found, PathBuf::from("E:/Git/cmd/../bin/bash.exe"));
    }

    #[test]
    fn wsl_bash_is_never_taken_and_a_miss_lists_what_was_tried() {
        let inputs = SearchInputs {
            path_dirs: vec![PathBuf::from("C:/Windows/System32")],
            install_roots: vec![PathBuf::from("C:/Program Files")],
            registry_homes: vec![PathBuf::from("C:/Program Files/Git")],
            ..SearchInputs::default()
        };
        let err = find_git_bash(&inputs, exists(&["C:/Windows/System32/bash.exe"])).unwrap_err();
        assert!(
            err.contains(DOWNLOAD_URL) && err.contains(GIT_BASH_PATH_VAR),
            "{err}"
        );
        assert!(err.contains("C:/Program Files/Git/bin/bash.exe"), "{err}");
        assert!(!err.contains("System32/bash.exe"), "{err}");
    }

    #[test]
    fn the_override_wins_and_a_bad_one_is_an_error() {
        let named = PathBuf::from("D:/tools/Git/bin/bash.exe");
        let inputs = SearchInputs {
            override_path: Some(named.clone()),
            registry_homes: vec![PathBuf::from("C:/Program Files/Git")],
            ..SearchInputs::default()
        };
        let both = exists(&[
            "D:/tools/Git/bin/bash.exe",
            "C:/Program Files/Git/bin/bash.exe",
        ]);
        assert_eq!(find_git_bash(&inputs, both).unwrap(), named);
        let err = find_git_bash(&inputs, |_| false).unwrap_err();
        assert!(err.contains("not a file"), "{err}");
    }

    struct Fake {
        found_before: bool,
        found_after: bool,
        winget: bool,
        winget_ok: bool,
        override_error: bool,
        ran: Cell<u32>,
        finds: Cell<u32>,
        asked: RefCell<Vec<String>>,
    }

    impl Fake {
        fn missing() -> Self {
            Fake {
                found_before: false,
                found_after: true,
                winget: true,
                winget_ok: true,
                override_error: false,
                ran: Cell::new(0),
                finds: Cell::new(0),
                asked: RefCell::new(Vec::new()),
            }
        }
    }

    impl Probe for Fake {
        fn find(&self) -> Result<PathBuf, String> {
            let n = self.finds.get();
            self.finds.set(n + 1);
            let ok = if n == 0 {
                self.found_before
            } else {
                self.found_after
            };
            if ok {
                Ok(PathBuf::from("C:/Git/bin/bash.exe"))
            } else if self.override_error {
                Err(format!("{GIT_BASH_PATH_VAR} names x, which is not a file"))
            } else {
                Err("none was found".to_owned())
            }
        }
        fn winget_available(&self) -> bool {
            self.winget
        }
        fn run_winget(&self) -> Result<(), String> {
            self.ran.set(self.ran.get() + 1);
            if self.winget_ok {
                Ok(())
            } else {
                Err("exit 1".to_owned())
            }
        }
    }

    #[test]
    fn present_git_asks_nothing_and_runs_nothing() {
        let mut fake = Fake::missing();
        fake.found_before = true;
        let out = ensure(&fake, |_| panic!("must not ask"));
        assert_eq!(out, Outcome::Ready(PathBuf::from("C:/Git/bin/bash.exe")));
        assert_eq!(fake.ran.get(), 0);
    }

    #[test]
    fn a_yes_installs_with_winget_and_searches_again() {
        let fake = Fake::missing();
        let out = ensure(&fake, |q| {
            fake.asked.borrow_mut().push(q.to_owned());
            true
        });
        assert_eq!(
            out,
            Outcome::Installed(PathBuf::from("C:/Git/bin/bash.exe"))
        );
        assert_eq!(fake.ran.get(), 1);
        assert_eq!(fake.asked.borrow().len(), 1);
        assert!(fake.asked.borrow()[0].contains(&winget_command_line()));
    }

    #[test]
    fn a_no_runs_nothing_and_gives_the_manual_fix() {
        let fake = Fake::missing();
        let Outcome::Declined(msg) = ensure(&fake, |_| false) else {
            panic!("expected Declined");
        };
        assert_eq!(fake.ran.get(), 0);
        assert!(
            msg.contains(DOWNLOAD_URL) && msg.contains(GIT_BASH_PATH_VAR),
            "{msg}"
        );
    }

    #[test]
    fn no_winget_gives_the_link_and_the_override_and_never_asks() {
        let mut fake = Fake::missing();
        fake.winget = false;
        let Outcome::NoWinget(msg) = ensure(&fake, |_| panic!("must not ask")) else {
            panic!("expected NoWinget");
        };
        assert_eq!(fake.ran.get(), 0);
        assert!(
            msg.contains(DOWNLOAD_URL) && msg.contains(GIT_BASH_PATH_VAR),
            "{msg}"
        );
    }

    #[test]
    fn a_failed_winget_and_a_still_missing_bash_both_fail_loud() {
        let mut fake = Fake::missing();
        fake.winget_ok = false;
        let out = ensure(&fake, |_| true);
        assert!(
            matches!(&out, Outcome::InstallFailed(m) if m.contains("exit 1")),
            "{out:?}"
        );
        let mut fake = Fake::missing();
        fake.found_after = false;
        let out = ensure(&fake, |_| true);
        assert!(
            matches!(&out, Outcome::InstallFailed(m) if m.contains("still not found")),
            "{out:?}"
        );
    }

    #[test]
    fn a_bad_override_is_not_answered_with_an_install() {
        let mut fake = Fake::missing();
        fake.override_error = true;
        let out = ensure(&fake, |_| panic!("must not ask"));
        assert!(matches!(out, Outcome::Declined(_)), "{out:?}");
        assert_eq!(fake.ran.get(), 0);
    }
}
