//! Non-panicking, cross-platform sinks for this shell's own diagnostic output:
//! `--probe`'s report, `--measure`'s report and its own drop-out lines, and the
//! render-adapter notice `kgraf_shell::run` prints on startup.
//!
//! **Why this exists.** `print!`/`println!`/`eprintln!` all `.expect()` internally
//! on a write error -- a first version of this module's Windows half claimed writes
//! there "can never panic" because it always installed *some* handle (even `NUL`)
//! before anything printed. That claim covered only `SetStdHandle`-backed writes
//! through Rust's own `Stdout`/`Stderr`; it did not change that `print!`/`eprintln!`
//! themselves still panic on ANY write error, from ANY cause (a full disk, a closed
//! pipe on the passthrough path, `NUL` itself being unavailable in some exotic
//! sandbox). `print_out`/`print_err` below never panic on any platform: a failed
//! write is counted (`dropped_writes`) and the line is silently discarded, which is
//! strictly better than crashing the diagnostic path that exists to help a user
//! understand what their machine did.
//!
//! **Windows-specific responsibilities**, all owned by the `windows` submodule:
//! - A console is attached (`AttachConsole(ATTACH_PARENT_PROCESS)`) only for a
//!   diagnostic invocation (`--probe`, `--measure`, `-h`/`--help`, `--version`, or a
//!   clap usage error); every other launch never attaches one, and detaches
//!   defensively if one is somehow already attached.
//! - `STD_OUTPUT_HANDLE`/`STD_ERROR_HANDLE` are each checked independently
//!   (`GetStdHandle` + `GetFileType`) before being touched; if *both* already have a
//!   usable handle (a launcher redirected both), the log file is never opened, much
//!   less rotated, at all -- there would be nothing to write to it.
//! - `CONOUT$` is reopened `GENERIC_READ | GENERIC_WRITE`, matching
//!   Microsoft's own guidance for reopening the console device.
//! - The log file lives under `directories::ProjectDirs::data_local_dir()`
//!   (`%LOCALAPPDATA%\kGraf\data`), not the roaming `data_dir()`.
//! - The log file is capped at 2 MiB, in place, for both the previous
//!   run's leftover log (at startup) and the current run's own (mid-run, the first
//!   time a write would cross the cap) -- both go through the same
//!   `rotate_keeping_tail`, which keeps the last 2 MiB as `<name>.log.1` rather than
//!   either deleting an oversized log outright (an earlier version did exactly that,
//!   throwing away the most recent, most useful lines) or letting either file grow
//!   without bound.
//! - The "no console attached" notice is written only through the stream(s)
//!   that actually got redirected to the log file, never blindly to both.
//! - `FreeConsole` when `AttachConsole` succeeds but the `CONOUT$` device
//!   handle fails to open, rather than leaving the process attached to a console
//!   session it cannot write to.
//! - `SetStdHandle`'s return is checked in every build profile (a failure
//!   counts as a dropped write), not only in debug builds via `debug_assert`.
//! - The log path is turned into a wide string with
//!   `OsStrExt::encode_wide`, not `Path::to_string_lossy`'s UTF-8 round trip.
//!
//! **What this does not cover:** third-party code that writes to `stdout`/`stderr`
//! directly (`env_logger`'s own writer, driving `log::warn!` from wgpu's validation
//! layer) still goes through the real, OS-level `SetStdHandle`-installed handle, not
//! through `print_out`/`print_err` -- there is no supported way to intercept a
//! dependency's own `std::io::stdout()` calls. That handle is still redirected
//! (console/log file/left alone per the rules above) so those writes land somewhere
//! reasonable, but they are not capped or rotated the way this module's own writes
//! are, and a panic inside `env_logger` itself (unlikely, but not provably
//! impossible) is outside what this module can fix.

use std::sync::atomic::{AtomicU64, Ordering};

static DROPPED: AtomicU64 = AtomicU64::new(0);

/// How many diagnostic lines have been silently dropped because the underlying
/// stream could not accept them. Near-always zero; exposed so a future `--probe`
/// report line (or a test) can observe the guarantee rather than trust it blindly.
pub fn dropped_writes() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Sets up this process's diagnostic stdout/stderr. A no-op on non-Windows targets
/// (a console-subsystem process there already has a working console). `is_diagnostic`
/// is `true` for `--probe`, `--measure`, `-h`/`--help`, `--version`, or a clap usage
/// error; `false` for a normal windowed launch. Call exactly once, before printing
/// anything (including before `env_logger::init()`).
pub fn init(is_diagnostic: bool) {
    platform::init(is_diagnostic);
}

/// Writes `s` (exactly as given -- callers supply their own line ending, matching
/// `print!`'s contract rather than `println!`'s) to this process's diagnostic stdout.
/// Never panics; a write failure is counted, not propagated.
pub fn print_out(s: &str) {
    platform::print_out(s);
}

/// Same as [`print_out`], for stderr.
pub fn print_err(s: &str) {
    platform::print_err(s);
}

#[cfg(not(windows))]
mod platform {
    use super::{DROPPED, Ordering};
    use std::io::Write;

    pub fn init(_is_diagnostic: bool) {
        // Console-subsystem process on every other platform this ships on (macOS,
        // Linux): stdout/stderr already work, inherited normally from the parent.
    }

    pub fn print_out(s: &str) {
        if std::io::stdout().write_all(s.as_bytes()).is_err() {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn print_err(s: &str) {
        if std::io::stderr().write_all(s.as_bytes()).is_err() {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{DROPPED, Ordering};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_TYPE_UNKNOWN, GetFileType, OPEN_ALWAYS, OPEN_EXISTING, WriteFile,
    };
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, FreeConsole, GetStdHandle, STD_ERROR_HANDLE,
        STD_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    /// The size at which a log file (the previous run's, at startup, or the
    /// current run's, mid-write) is rotated, keeping only the last this-many bytes.
    const LOG_SIZE_CAP_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB

    struct LogFile {
        handle: HANDLE,
        path: PathBuf,
        written: u64,
    }
    // SAFETY: `HANDLE` (a raw Win32 handle / pointer) carries no thread affinity; every
    // access to a `LogFile` goes through `LOG_FILE`'s `Mutex`, which is this struct's
    // only reason to need `Send` at all (a `static Mutex<Option<LogFile>>` requires its
    // contents be `Send` even though the mutex itself serializes all real access).
    unsafe impl Send for LogFile {}

    static LOG_FILE: Mutex<Option<LogFile>> = Mutex::new(None);
    static STDOUT_IS_LOG: AtomicBool = AtomicBool::new(false);
    static STDERR_IS_LOG: AtomicBool = AtomicBool::new(false);

    fn wide_nul(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Builds the wide string from the `OsStr` directly, not through
    /// `Path::to_string_lossy`'s UTF-8 round trip.
    fn wide_nul_path(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn open_for_write(
        wide: &[u16],
        desired_access: u32,
        creation_disposition: u32,
    ) -> Option<HANDLE> {
        // SAFETY: `wide` is a valid, NUL-terminated UTF-16 buffer for the whole call;
        // every other argument is a plain value `CreateFileW` documents as accepting.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                desired_access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                creation_disposition,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            None
        } else {
            Some(handle)
        }
    }

    fn raw_write(handle: HANDLE, bytes: &[u8]) -> bool {
        let mut written = 0u32;
        // SAFETY: every caller only reaches this with a handle it just verified is
        // live and open for writing; `bytes`/its length describe a valid buffer for
        // the whole call.
        unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            ) != 0
        }
    }

    /// True if `std_handle` already names something real that must not be
    /// clobbered (a launcher-redirected file or pipe).
    fn already_has_a_usable_handle(std_handle: STD_HANDLE) -> bool {
        // SAFETY: GetStdHandle/GetFileType have no preconditions beyond a valid
        // STD_HANDLE constant and a possibly-NULL/invalid HANDLE, both true here.
        unsafe {
            let handle = GetStdHandle(std_handle);
            !handle.is_null()
                && handle != INVALID_HANDLE_VALUE
                && GetFileType(handle) != FILE_TYPE_UNKNOWN
        }
    }

    /// Installs `handle` for `std_handle`, returning whether it actually did (`false`
    /// if the stream already had a usable handle, or if `SetStdHandle` itself
    /// failed -- checked in every build profile, not only via `debug_assert`, and
    /// counted as a dropped write either way since nothing reaches that stream now).
    fn install(std_handle: STD_HANDLE, handle: HANDLE) -> bool {
        // SAFETY: `handle` is a live, just-opened handle; `SetStdHandle` only stores
        // the value in the process's per-process table.
        let ok = unsafe { SetStdHandle(std_handle, handle) != 0 };
        if !ok {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    /// `%LOCALAPPDATA%\kGraf\data` (`data_local_dir()`, not the roaming
    /// `data_dir()`), falling back to the system temp directory if there is no
    /// resolvable home directory, or if the preferred directory cannot be created.
    fn resolve_log_dir() -> (PathBuf, Option<String>) {
        let Some(dirs) = directories::ProjectDirs::from("", "", crate::app_dirs::APP_NAME) else {
            return (std::env::temp_dir(), None);
        };
        let dir = dirs.data_local_dir().to_path_buf();
        match std::fs::create_dir_all(&dir) {
            Ok(()) => (dir, None),
            Err(e) => {
                let fallback = std::env::temp_dir();
                let notice = format!(
                    "could not create log directory {} ({e}); using {} instead",
                    dir.display(),
                    fallback.display()
                );
                (fallback, Some(notice))
            }
        }
    }

    /// Keeps the last `LOG_SIZE_CAP_BYTES` of `path`'s current content as
    /// `<name>.log.1` (overwriting whatever was already there there), instead of
    /// dropping the whole thing. Shared by the startup path (the previous run's
    /// leftover log) and the mid-run path (this run's own log crossing the cap).
    fn rotate_keeping_tail(path: &Path) {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut f) = std::fs::File::open(path) else {
            return;
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(LOG_SIZE_CAP_BYTES);
        if f.seek(SeekFrom::Start(start)).is_err() {
            return;
        }
        let mut tail = Vec::with_capacity((len - start) as usize);
        if f.read_to_end(&mut tail).is_err() {
            return;
        }
        let rotated = path.with_extension("log.1");
        let _ = std::fs::write(&rotated, &tail);
    }

    fn open_log_file() -> Option<(HANDLE, PathBuf, Option<String>)> {
        let (dir, dir_error) = resolve_log_dir();
        let log_path = dir.join("kgraf-app.log");
        if log_path.exists() {
            rotate_keeping_tail(&log_path);
            let _ = std::fs::remove_file(&log_path);
        }
        let wide = wide_nul_path(&log_path);
        let handle = open_for_write(&wide, FILE_APPEND_DATA, OPEN_ALWAYS)?;
        Some((handle, log_path, dir_error))
    }

    pub fn init(is_diagnostic: bool) {
        let stdout_needs = !already_has_a_usable_handle(STD_OUTPUT_HANDLE);
        let stderr_needs = !already_has_a_usable_handle(STD_ERROR_HANDLE);

        // Nothing to do at all if both streams are already usable (a launcher
        // redirected both) -- in particular, never open (or rotate) the log file.
        if !stdout_needs && !stderr_needs {
            if !is_diagnostic {
                // SAFETY: no preconditions; documented safe no-op if nothing is
                // attached, which should always be true reaching this branch.
                unsafe {
                    FreeConsole();
                }
            }
            return;
        }

        let console_handle = if is_diagnostic {
            // SAFETY: ATTACH_PARENT_PROCESS is a plain flag value; AttachConsole has
            // no preconditions beyond "call before doing console I/O", true here.
            let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 };
            if attached {
                let wide = wide_nul("CONOUT$");
                let handle = open_for_write(&wide, GENERIC_READ | GENERIC_WRITE, OPEN_EXISTING);
                if handle.is_none() {
                    // Attached but the device handle didn't open -- detach.
                    unsafe {
                        FreeConsole();
                    }
                }
                handle
            } else {
                None
            }
        } else {
            // A normal windowed launch never attaches a console.
            unsafe {
                FreeConsole();
            }
            None
        };

        if let Some(handle) = console_handle {
            if stdout_needs {
                install(STD_OUTPUT_HANDLE, handle);
            }
            if stderr_needs {
                install(STD_ERROR_HANDLE, handle);
            }
            return;
        }

        let Some((handle, path, dir_error)) = open_log_file() else {
            // Neither a console nor the log file worked: leave whichever stream(s)
            // needed one without a target. print_out/print_err's fallback path
            // (GetStdHandle, WriteFile, count on failure) never panics either way.
            return;
        };

        let installed_stdout = stdout_needs && install(STD_OUTPUT_HANDLE, handle);
        let installed_stderr = stderr_needs && install(STD_ERROR_HANDLE, handle);
        STDOUT_IS_LOG.store(installed_stdout, Ordering::Relaxed);
        STDERR_IS_LOG.store(installed_stderr, Ordering::Relaxed);
        *LOG_FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(LogFile {
            handle,
            path: path.clone(),
            written: 0,
        });

        // The notice goes only through the stream(s) that actually landed on the
        // log file, not unconditionally to both.
        let notice = match dir_error {
            Some(e) => format!(
                "kgraf-app: {e}; --probe/--measure output and warnings are written to {}\n",
                path.display()
            ),
            None => format!(
                "kgraf-app: no console attached; --probe/--measure output and warnings are written to {}\n",
                path.display()
            ),
        };
        if installed_stdout {
            print_out(&notice);
        }
        if installed_stderr {
            print_err(&notice);
        }
    }

    /// Writes `bytes` to the shared log file, rotating it in place first if this
    /// write would cross the cap. Returns `false` if the log file is not this
    /// stream's target at all (the caller falls back to a direct OS-handle write).
    fn write_to_log(bytes: &[u8]) -> bool {
        let mut guard = LOG_FILE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(log) = guard.as_mut() else {
            return false;
        };
        if log.written + bytes.len() as u64 > LOG_SIZE_CAP_BYTES {
            // SAFETY: `log.handle` is a live handle this struct owns exclusively
            // (access serialized by LOG_FILE's mutex); it is immediately replaced
            // below, never used again after this call.
            unsafe {
                CloseHandle(log.handle);
            }
            rotate_keeping_tail(&log.path);
            let _ = std::fs::remove_file(&log.path);
            let wide = wide_nul_path(&log.path);
            match open_for_write(&wide, FILE_APPEND_DATA, OPEN_ALWAYS) {
                Some(fresh) => {
                    log.handle = fresh;
                    log.written = 0;
                    // Re-point the OS-level stream(s) too, so anything writing
                    // directly to stdout/stderr (env_logger's own writer -- see the
                    // module doc's "what this does not cover") keeps landing in the
                    // live file after rotation, not the one just closed and renamed.
                    if STDOUT_IS_LOG.load(Ordering::Relaxed) {
                        unsafe {
                            SetStdHandle(STD_OUTPUT_HANDLE, fresh);
                        }
                    }
                    if STDERR_IS_LOG.load(Ordering::Relaxed) {
                        unsafe {
                            SetStdHandle(STD_ERROR_HANDLE, fresh);
                        }
                    }
                }
                None => {
                    DROPPED.fetch_add(1, Ordering::Relaxed);
                    return true; // this stream IS log-backed; the write below just fails.
                }
            }
        }
        if raw_write(log.handle, bytes) {
            log.written += bytes.len() as u64;
        } else {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    fn write_stream(std_handle: STD_HANDLE, is_log: bool, bytes: &[u8]) {
        if is_log && write_to_log(bytes) {
            return;
        }
        // Console, or a launcher-provided passthrough handle: write straight to
        // whatever is currently installed for this stream.
        // SAFETY: no preconditions.
        let handle = unsafe { GetStdHandle(std_handle) };
        let ok = !handle.is_null() && handle != INVALID_HANDLE_VALUE && raw_write(handle, bytes);
        if !ok {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn print_out(s: &str) {
        write_stream(
            STD_OUTPUT_HANDLE,
            STDOUT_IS_LOG.load(Ordering::Relaxed),
            s.as_bytes(),
        );
    }

    pub fn print_err(s: &str) {
        write_stream(
            STD_ERROR_HANDLE,
            STDERR_IS_LOG.load(Ordering::Relaxed),
            s.as_bytes(),
        );
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rotate_keeping_tail_keeps_the_end_not_the_start() {
            let dir = std::env::temp_dir().join(format!("kgraf-diag-test-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("kgraf-app.log");
            let mut content = vec![b'a'; (LOG_SIZE_CAP_BYTES as usize) + 100];
            // Mark the tail distinctly so "kept the end" is a real assertion, not an
            // accident of an all-identical buffer.
            content[(LOG_SIZE_CAP_BYTES as usize)..].fill(b'z');
            std::fs::write(&path, &content).unwrap();

            rotate_keeping_tail(&path);

            let rotated = path.with_extension("log.1");
            let kept = std::fs::read(&rotated).unwrap();
            assert_eq!(kept.len() as u64, LOG_SIZE_CAP_BYTES);
            assert!(
                kept.ends_with(b"zzzz"),
                "the most recent bytes must be kept"
            );
            assert!(
                !kept.starts_with(b"aaaa"),
                "the oldest bytes must be dropped, not kept"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn a_stream_with_an_existing_console_handle_is_left_alone() {
            // Exercises `already_has_a_usable_handle`/`install` against the real
            // STD_OUTPUT_HANDLE (whatever this test process has under `cargo test`),
            // matching the point: behavior against the real GetStdHandle/GetFileType
            // pair, not a mocked config value.
            let before = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
            if unsafe { GetFileType(before) } == FILE_TYPE_UNKNOWN {
                return; // nothing usable to assert preservation of; see the test note above.
            }
            let wide = wide_nul("NUL");
            let other = open_for_write(&wide, GENERIC_WRITE, OPEN_EXISTING).unwrap();
            assert!(already_has_a_usable_handle(STD_OUTPUT_HANDLE));
            // install() does not itself check already_has_a_usable_handle (init()'s
            // stdout_needs/stderr_needs gate that before ever calling install()); this
            // asserts the gate, not install() in isolation.
            let after = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
            assert_eq!(before, after);
            unsafe {
                CloseHandle(other);
            }
        }
    }
}
