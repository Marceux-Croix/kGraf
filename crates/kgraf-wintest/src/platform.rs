//! The Windows calls the runner makes, behind plain functions. Other hosts
//! get inert versions so the crate builds and its pure parts are tested
//! there; the runner itself only does real work on Windows.
//!
//! Nothing here installs, elevates, or writes outside the work folder,
//! except the `subst` drive letter (a per-session mapping that the cleanup
//! removes and verifies) and, only when an escape test proves a sandbox gap,
//! the removal of what that test left (named `kgraf-escape-*`).

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

pub use imp::*;

/// Set by the console handler on Ctrl+C or window close.
pub static CANCEL: AtomicBool = AtomicBool::new(false);
/// Set by the runner when its cleanup and report are done.
pub static FINISHED: AtomicBool = AtomicBool::new(false);

/// The free drive letters to try for the mirror, last letters first (the
/// ones least likely to be used by removable drives).
pub const DRIVE_LETTERS: &str = "WVUTSRQPONMLK";

/// Drive letters `subst` lists: lines like `W:\: => C:\...`.
pub fn parse_subst(text: &str) -> Vec<char> {
    text.lines()
        .filter_map(|l| {
            let mut c = l.trim().chars();
            let letter = c.next()?;
            (c.next() == Some(':') && l.contains("=>")).then(|| letter.to_ascii_uppercase())
        })
        .collect()
}

/// The values `reg query <Run key>` lists whose names look like an escape
/// test's (`kgraf-escape-<pid>`).
pub fn escape_values(reg_output: &str) -> Vec<String> {
    crate::facts::parse_reg_query(reg_output)
        .into_keys()
        .filter(|k| crate::workdir::is_escape_trace(k))
        .collect()
}

fn list_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Files a profile-write escape test would leave.
pub fn profile_escape_files(profile: &Path) -> Vec<String> {
    let mut v: Vec<String> = list_names(profile)
        .into_iter()
        .filter(|n| crate::workdir::is_escape_trace(n))
        .collect();
    v.sort();
    v
}

/// Entries of kGraf's per-user folders, as `<folder>/<entry>`.
pub fn kgraf_app_data(roots: &[(&str, PathBuf)]) -> Vec<String> {
    let mut v = Vec::new();
    for (label, root) in roots {
        let dir = root.join("kGraf");
        if dir.is_dir() {
            v.push(format!("{label}/kGraf"));
            for n in list_names(&dir) {
                v.push(format!("{label}/kGraf/{n}"));
            }
        }
    }
    v.sort();
    v
}

#[cfg(windows)]
mod imp {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
        GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
        SetConsoleMode,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};
    use windows_sys::Win32::UI::Shell::{FOLDERID_Desktop, SHGetKnownFolderPath};

    use super::{CANCEL, FINISHED};
    use crate::workdir::Ops;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// ANSI escapes work only after virtual terminal processing is on;
    /// false means the plain carriage-return line is used.
    pub fn enable_vt() -> bool {
        // SAFETY: GetStdHandle has no preconditions; the mode calls get a
        // handle from it and a valid out pointer.
        unsafe {
            let h = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode = 0;
            if h.is_null() || GetConsoleMode(h, &mut mode) == 0 {
                return false;
            }
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
        }
    }

    pub fn console_width() -> usize {
        // SAFETY: a zeroed plain-data struct and a handle from GetStdHandle.
        unsafe {
            let h = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if !h.is_null() && GetConsoleScreenBufferInfo(h, &mut info) != 0 {
                let w = i32::from(info.srWindow.Right) - i32::from(info.srWindow.Left) + 1;
                if w > 20 {
                    return w as usize;
                }
            }
        }
        80
    }

    unsafe extern "system" fn on_ctrl(kind: u32) -> windows_sys::core::BOOL {
        CANCEL.store(true, Ordering::SeqCst);
        // CTRL_CLOSE_EVENT (2), LOGOFF (5), SHUTDOWN (6): the process ends
        // when this returns, so give the cleanup a few seconds first.
        if matches!(kind, 2 | 5 | 6) {
            for _ in 0..45 {
                if FINISHED.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        1
    }

    pub fn install_ctrl_handler() {
        // SAFETY: registers a handler with the documented signature.
        unsafe {
            SetConsoleCtrlHandler(Some(on_ctrl), 1);
        }
    }

    /// Put this process in a Job Object that kills every process in it when
    /// the last handle closes, as `cargo test` does on Windows: a test
    /// process cannot outlive the runner, even if the runner is killed.
    pub fn join_kill_on_close_job() -> Result<(), String> {
        // SAFETY: a fresh unnamed job, a zeroed limit struct with one flag
        // set, and the current-process pseudo handle. The job handle is kept
        // open for the life of the process on purpose (closing it would end
        // the processes in it).
        unsafe {
            let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err("CreateJobObjectW failed".into());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                CloseHandle(job);
                return Err("SetInformationJobObject failed".into());
            }
            if AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
                CloseHandle(job);
                return Err("AssignProcessToJobObject failed".into());
            }
        }
        Ok(())
    }

    /// The real Desktop folder (OneDrive redirection included).
    pub fn desktop_dir() -> Option<PathBuf> {
        let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
        // SAFETY: documented call; the returned buffer is freed with
        // CoTaskMemFree whether or not the call succeeded.
        unsafe {
            let hr = SHGetKnownFolderPath(&FOLDERID_Desktop, 0, std::ptr::null_mut(), &mut p);
            let out = if hr >= 0 && !p.is_null() {
                let mut len = 0;
                while *p.add(len) != 0 {
                    len += 1;
                }
                Some(PathBuf::from(String::from_utf16_lossy(
                    std::slice::from_raw_parts(p, len),
                )))
            } else {
                None
            };
            CoTaskMemFree(p.cast());
            out
        }
    }

    /// `(process machine, native machine)` from IsWow64Process2: an x86_64
    /// process emulated on ARM64 reports an unknown process machine and an
    /// ARM64 native one.
    pub fn machines() -> Option<(u16, u16)> {
        let (mut process, mut native) = (0u16, 0u16);
        // SAFETY: current-process pseudo handle and two out pointers.
        let ok = unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, &mut native) };
        (ok != 0).then_some((process, native))
    }

    /// The file system name of the volume that holds `path` (NTFS, ReFS, FAT32...).
    pub fn volume_fs(path: &Path) -> Option<String> {
        let p = wide(&path.display().to_string());
        let mut root = [0u16; 512];
        let mut fs = [0u16; 64];
        // SAFETY: NUL-terminated input, buffers with their lengths.
        unsafe {
            if GetVolumePathNameW(p.as_ptr(), root.as_mut_ptr(), root.len() as u32) == 0 {
                return None;
            }
            if GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            ) == 0
            {
                return None;
            }
        }
        Some(from_wide(&fs))
    }

    fn quiet(program: &str) -> Command {
        let mut c = Command::new(program);
        c.stdin(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
        c
    }

    /// `reg query` output for one key (all values) or one value.
    pub fn reg_query(key: &str, value: Option<&str>) -> Option<String> {
        let mut c = quiet("reg");
        c.args(["query", key]);
        if let Some(v) = value {
            c.args(["/v", v]);
        }
        let out = c.output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

    /// Delete one `kgraf-escape-*` value from the Run key.
    pub fn delete_run_value(name: &str) -> bool {
        if !crate::workdir::is_escape_trace(name) {
            return false;
        }
        quiet("reg")
            .args(["delete", RUN_KEY, "/v", name, "/f"])
            .status()
            .is_ok_and(|s| s.success())
    }

    fn subst_list() -> Vec<char> {
        quiet("subst")
            .output()
            .map(|o| super::parse_subst(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default()
    }

    /// Map a free drive letter to `target` with `subst` (no elevation, this
    /// logon session only).
    pub fn map_drive(target: &Path) -> Result<char, String> {
        let used = subst_list();
        for letter in super::DRIVE_LETTERS.chars() {
            if used.contains(&letter) || Path::new(&format!("{letter}:\\")).exists() {
                continue;
            }
            let ok = quiet("subst")
                .arg(format!("{letter}:"))
                .arg(target)
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                return Ok(letter);
            }
        }
        Err("no free drive letter for the source mirror".into())
    }

    pub struct WinOps;

    impl Ops for WinOps {
        fn remove_tree(&self, path: &Path) -> std::io::Result<()> {
            // std's remove_dir_all removes a junction or symbolic link
            // itself, never what it points to, and clears read-only files.
            std::fs::remove_dir_all(path)
        }
        fn reset_acl(&self, path: &Path) -> std::io::Result<()> {
            // /L acts on links themselves, not their targets.
            let s = quiet("icacls")
                .arg(path)
                .args(["/reset", "/T", "/C", "/L", "/Q"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
            if s.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!("icacls exited {s}")))
            }
        }
        fn remove_drive(&self, letter: char) -> std::io::Result<()> {
            let s = quiet("subst")
                .arg(format!("{letter}:"))
                .arg("/D")
                .status()?;
            if s.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!("subst /D exited {s}")))
            }
        }
        fn drive_mapped(&self, letter: char) -> bool {
            subst_list().contains(&letter)
        }
        fn exists(&self, path: &Path) -> bool {
            path.symlink_metadata().is_ok()
        }
    }

    pub fn ops() -> WinOps {
        WinOps
    }

    /// Remove the unzipped bundle folder after this process exits, with a
    /// detached `cmd.exe` that retries until the running exe is unlocked.
    /// No scheduled task, no registry entry. Done only when the folder holds
    /// nothing but the bundle's own entries (checked by the caller).
    pub fn schedule_folder_removal(dir: &Path) -> Result<(), String> {
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        let d = dir.display().to_string();
        if d.contains('"') || d.contains('%') || d.contains('^') || d.contains('&') {
            return Err("the folder path holds characters cmd would interpret".into());
        }
        let parent = dir.parent().ok_or("the folder has no parent")?;
        let script = format!(
            "/d /c for /l %i in (1,1,60) do @(rmdir /s /q \"{d}\" 2>nul & if not exist \"{d}\" exit /b 0) & ping -n 2 127.0.0.1 >nul"
        );
        Command::new("cmd.exe")
            .raw_arg(script)
            .current_dir(parent)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::{Path, PathBuf};

    pub fn enable_vt() -> bool {
        true
    }
    pub fn console_width() -> usize {
        100
    }
    pub fn install_ctrl_handler() {}
    pub fn join_kill_on_close_job() -> Result<(), String> {
        Ok(())
    }
    pub fn desktop_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Desktop"))
    }
    pub fn machines() -> Option<(u16, u16)> {
        None
    }
    pub fn volume_fs(_path: &Path) -> Option<String> {
        None
    }
    pub fn reg_query(_key: &str, _value: Option<&str>) -> Option<String> {
        None
    }
    pub const RUN_KEY: &str = "";
    pub fn delete_run_value(_name: &str) -> bool {
        false
    }
    pub fn map_drive(_target: &Path) -> Result<char, String> {
        Err("drive letters exist on Windows only".into())
    }
    pub fn ops() -> crate::workdir::LocalOps {
        crate::workdir::LocalOps
    }
    pub fn schedule_folder_removal(_dir: &Path) -> Result<(), String> {
        Err("Windows only".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subst_output_gives_the_mapped_letters() {
        let text =
            "W:\\: => C:\\Users\\a\\AppData\\Local\\Temp\\kgraf-wintest-1\\m\r\nk:\\: => D:\\x\r\n";
        assert_eq!(parse_subst(text), vec!['W', 'K']);
        assert!(parse_subst("").is_empty());
    }

    #[test]
    fn only_escape_named_run_values_are_taken() {
        let text = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n    OneDrive    REG_SZ    C:\\x\\OneDrive.exe\r\n    kgraf-escape-4242    REG_SZ    C:\\evil.exe\r\n";
        assert_eq!(escape_values(text), vec!["kgraf-escape-4242".to_owned()]);
    }

    #[test]
    fn profile_files_and_app_data_are_listed() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("kgraf-escape-1.txt"), "x").expect("w");
        std::fs::write(dir.path().join("notes.txt"), "x").expect("w");
        std::fs::create_dir_all(dir.path().join("kGraf")).expect("mkdir");
        std::fs::write(dir.path().join("kGraf").join("settings.toml"), "").expect("w");
        assert_eq!(
            profile_escape_files(dir.path()),
            vec!["kgraf-escape-1.txt".to_owned()]
        );
        assert_eq!(
            kgraf_app_data(&[("APPDATA", dir.path().to_path_buf())]),
            vec![
                "APPDATA/kGraf".to_owned(),
                "APPDATA/kGraf/settings.toml".to_owned()
            ]
        );
    }
}
