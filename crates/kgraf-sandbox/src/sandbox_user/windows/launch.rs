//! The owner's side of a command that runs as the sandbox user (decision
//! 0097): create the runner pipe, start the runner with
//! `CreateProcessWithLogonW`, put it in the command's job before it runs,
//! check that the pipe's client is that runner, send the request and read the
//! runner's reply. See [`crate::sandbox_user::runner`] for the protocol and
//! `runner.rs` for the runner itself.
//!
//! **Desktop.** The documentation of `CreateProcessWithLogonW` says that with
//! no desktop named the call adds the new user to the access lists of the
//! caller's window station and desktop, and that with one named, the
//! application must add that permission itself, even for `WinSta0\Default`.
//! kGraf changes no access list of the owner's session. It creates a window
//! station and desktop of its own for each sandbox user, once per process
//! ([`runner_desktop`]), whose access lists admit SYSTEM, the owner and that
//! user, and names that desktop when it starts the runner. The runner's start
//! therefore depends on no logon SID and no entry on the owner's window
//! station. The command itself runs on a second window station the runner
//! creates.
//!
//! **Job.** The runner, and through it the command, runs in the job the
//! caller passes, which kills on close, allows no breakaway and sets no user
//! interface limits: Windows nests a job inside another only if neither sets
//! such limits, and the process the secondary logon service creates, or kGraf
//! itself, may already be in a job. The command's user interface isolation
//! comes from its own window station and desktop.
//!
//! **Unverified on Windows:** that seclogon passes pipe and file handles
//! through `STARTF_USESTDHANDLES` to a process of another user (the call's
//! documentation says it does), and that the runner can be put in kGraf's job
//! when seclogon has put it in a job of its own. A failure of either is
//! reported with what the next run needs: the Win32 error, whether the runner
//! was in a job, and the runner's exit code, named where it is known.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    GENERIC_ALL, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, READ_CONTROL,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, IsProcessInJob, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CloseWindowStation, CreateDesktopW, CreateWindowStationW, DESKTOP_CREATEMENU,
    DESKTOP_CREATEWINDOW, DESKTOP_ENUMERATE, DESKTOP_HOOKCONTROL, DESKTOP_JOURNALPLAYBACK,
    DESKTOP_JOURNALRECORD, DESKTOP_READOBJECTS, DESKTOP_WRITEOBJECTS, GetProcessWindowStation,
    HDESK, HWINSTA, SetProcessWindowStation,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessWithLogonW,
    GetCurrentProcessId, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW,
    TerminateProcess,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CWF_CREATE_ONLY, WINSTA_ALL_ACCESS};

use super::super::password::Password;
use super::super::runner::{
    MAX_REPLY, Reply, Request, RunnerArgs, describe_runner_exit, logon_error_hint,
};
use super::{accounts, last_error, pipe, wide, wide_secret};
use crate::error::SandboxError;
use crate::win_command_line::command_line;

/// How long the whole handshake may take: the runner must start, connect,
/// read the request and reply within this. A command that takes longer to
/// run is not affected, because the handshake ends once the command has
/// started. A runner that stops responding is ended with its job, so a
/// sandboxed command can never block the caller for longer than this.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// The sandbox user a command starts as.
pub(crate) struct Credential<'a> {
    pub name: &'a str,
    pub sid: &'a str,
    pub password: &'a Password,
}

/// A started command: the runner's process (its exit code is the
/// command's) and the command's own process ID.
pub(crate) struct Started {
    pub runner: OwnedHandle,
    pub command_pid: u32,
}

/// Whether `process` is in any job, as text for an error: `true`, `false`,
/// or why it could not be read.
fn in_a_job(process: &OwnedHandle) -> String {
    let mut result = 0;
    // SAFETY: a valid process handle; a null job asks about any job.
    if unsafe { IsProcessInJob(process.as_raw_handle(), ptr::null_mut(), &mut result) } == 0 {
        return format!("unknown ({})", std::io::Error::last_os_error());
    }
    (result != 0).to_string()
}

/// The error for a failed `CreateProcessWithLogonW` as the sandbox user
/// `name`, with what to do for the errors whose cause is known.
fn logon_failed(name: &str, err: std::io::Error) -> SandboxError {
    let hint = err
        .raw_os_error()
        .and_then(|code| logon_error_hint(code as u32));
    let source = match hint {
        Some(hint) => std::io::Error::other(format!("{err} ({name}): {hint}")),
        None => err,
    };
    SandboxError::PlatformSetup {
        step: "start the runner as the sandbox user",
        source,
    }
}

fn failed(step: &'static str, why: impl std::fmt::Display) -> SandboxError {
    SandboxError::PlatformSetup {
        step,
        source: std::io::Error::other(why.to_string()),
    }
}

/// The runner pipe: one duplex instance, no remote clients, admitting SYSTEM,
/// the owner and the sandbox user.
fn create_pipe(name: &str, owner_sid: &str, user_sid: &str) -> Result<OwnedHandle, SandboxError> {
    // Both SIDs come from Windows' own SID-to-string conversion (the owner's
    // token, and the stored user record, checked as a SID string when it was
    // read), so neither holds SDDL syntax characters.
    let sddl = wide(&format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;{owner_sid})(A;;GRGW;;;{user_sid})"
    ));
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; the call allocates `sd`, freed below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error("build the runner pipe's access list"));
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let w = wide(name);
    // SAFETY: creates one duplex instance; `sa` and `w` live across the call.
    let h = unsafe {
        CreateNamedPipeW(
            w.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            64 * 1024,
            64 * 1024,
            0,
            &sa,
        )
    };
    // SAFETY: frees the descriptor the conversion allocated.
    unsafe { LocalFree(sd as HLOCAL) };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(last_error("create the runner pipe"));
    }
    // SAFETY: a fresh handle this code now owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

/// The system folder (`C:\Windows\System32`), NUL-terminated: the runner's
/// working directory. Every user can read it, so the runner's start does not
/// depend on a grant; the command gets its own directory from the request.
fn system_directory() -> Result<Vec<u16>, SandboxError> {
    let mut buf = vec![0u16; 512];
    // SAFETY: `buf` has the stated length in UTF-16 units.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return Err(last_error("find the system folder"));
    }
    buf.truncate(n);
    buf.push(0);
    Ok(buf)
}

/// What the sandbox user may do on the window station kGraf makes for its
/// runner: `WINSTA_ALL_ACCESS` and reading the access list, never rewriting
/// it, taking ownership or deleting it.
const RUNNER_STATION_ACCESS: u32 = WINSTA_ALL_ACCESS as u32 | READ_CONTROL;
/// The same on its desktop: every desktop right but switching to it
/// (`DESKTOP_SWITCHDESKTOP`), and reading the access list.
const RUNNER_DESKTOP_ACCESS: u32 = DESKTOP_READOBJECTS
    | DESKTOP_CREATEWINDOW
    | DESKTOP_CREATEMENU
    | DESKTOP_HOOKCONTROL
    | DESKTOP_JOURNALRECORD
    | DESKTOP_JOURNALPLAYBACK
    | DESKTOP_ENUMERATE
    | DESKTOP_WRITEOBJECTS
    | READ_CONTROL;

/// A window station and its one desktop, made by this process for the
/// runners of one sandbox user. Kept for the life of the process: a window
/// station ends with its last handle, and a runner may start on it at any
/// time.
struct RunnerStation {
    _station: HWINSTA,
    _desktop: HDESK,
    /// `<station>\Default`, NUL-terminated, for `STARTUPINFOW::lpDesktop`.
    startup_name: Vec<u16>,
}

// SAFETY: window station and desktop handles are process-wide kernel handles
// with no thread affinity, and these are only kept, never used, once made.
unsafe impl Send for RunnerStation {}

/// A security descriptor from SDDL, freed on drop.
struct Sddl(PSECURITY_DESCRIPTOR);

impl Sddl {
    fn new(text: &str, step: &'static str) -> Result<Self, SandboxError> {
        let w = wide(text);
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `w` is NUL-terminated; the call allocates `sd`, freed on
        // drop.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(last_error(step));
        }
        Ok(Self(sd))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for Sddl {
    fn drop(&mut self) {
        // SAFETY: frees the descriptor the conversion allocated, once.
        unsafe { LocalFree(self.0 as HLOCAL) };
    }
}

/// The `<station>\Default` name of the window station and desktop this
/// process made for the runners of the sandbox user `user_sid`, made on first
/// use. A failure is not kept, so a later command tries again.
fn runner_desktop(owner_sid: &str, user_sid: &str) -> Result<Vec<u16>, SandboxError> {
    static STATIONS: OnceLock<Mutex<HashMap<String, RunnerStation>>> = OnceLock::new();
    let mut stations = STATIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| failed("make the runner's window station", "a lock was poisoned"))?;
    if let Some(found) = stations.get(user_sid) {
        return Ok(found.startup_name.clone());
    }
    let made = RunnerStation::create(owner_sid, user_sid)?;
    let name = made.startup_name.clone();
    stations.insert(user_sid.to_owned(), made);
    Ok(name)
}

impl RunnerStation {
    fn create(owner_sid: &str, user_sid: &str) -> Result<Self, SandboxError> {
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).map_err(|e| failed("name the runner's window station", e))?;
        let name = format!(
            "kgraf-run-{}",
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        // Both SIDs come from Windows' own SID-to-string conversion, so
        // neither holds SDDL syntax characters. `P`: inherit nothing.
        let station_sd = Sddl::new(
            &format!(
                "D:P(A;;GA;;;SY)(A;;GA;;;{owner_sid})(A;;0x{RUNNER_STATION_ACCESS:08x};;;{user_sid})"
            ),
            "build the runner window station's access list",
        )?;
        let desktop_sd = Sddl::new(
            &format!(
                "D:P(A;;GA;;;SY)(A;;GA;;;{owner_sid})(A;;0x{RUNNER_DESKTOP_ACCESS:08x};;;{user_sid})"
            ),
            "build the runner desktop's access list",
        )?;
        let wname = wide(&name);
        let sa = station_sd.attributes();
        // SAFETY: creates a new window station (CWF_CREATE_ONLY: never opens
        // one another program made under this name); `wname` and `sa` live
        // across the call.
        let station =
            unsafe { CreateWindowStationW(wname.as_ptr(), CWF_CREATE_ONLY, GENERIC_ALL, &sa) };
        if station.is_null() {
            return Err(last_error("make the runner's window station"));
        }
        // `CreateDesktopW` makes the desktop on this process's window
        // station, so enter the new one and return at once. Every thread of
        // this process sees the switch for that moment; it happens once per
        // sandbox user and process, and the lock above keeps two of these
        // from overlapping.
        // SAFETY: reads this process's window station.
        let original = unsafe { GetProcessWindowStation() };
        // SAFETY: `station` is the handle made above.
        if original.is_null() || unsafe { SetProcessWindowStation(station) } == 0 {
            let e = last_error("enter the runner's window station");
            // SAFETY: closes the station made above.
            unsafe { CloseWindowStation(station) };
            return Err(e);
        }
        let desk = wide("Default");
        let dsa = desktop_sd.attributes();
        // SAFETY: makes a desktop on the window station entered above.
        let desktop = unsafe {
            CreateDesktopW(
                desk.as_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                GENERIC_ALL,
                &dsa,
            )
        };
        let made = std::io::Error::last_os_error();
        // SAFETY: returns to the window station this process started on.
        let back = unsafe { SetProcessWindowStation(original) };
        if desktop.is_null() || back == 0 {
            let e = if desktop.is_null() {
                SandboxError::PlatformSetup {
                    step: "make the runner's desktop",
                    source: made,
                }
            } else {
                last_error("leave the runner's window station")
            };
            // SAFETY: closes what was made above.
            unsafe {
                if !desktop.is_null() {
                    CloseDesktop(desktop);
                }
                CloseWindowStation(station);
            }
            return Err(e);
        }
        Ok(Self {
            _station: station,
            _desktop: desktop,
            startup_name: wide(&format!("{name}\\Default")),
        })
    }
}

/// Start the runner at `runner_exe` as `user`, with `std` as its standard
/// handles, in `job`, and have it start `request`. On any failure the job is
/// ended, so nothing started here keeps running.
pub(crate) fn start(
    user: &Credential<'_>,
    runner_exe: &Path,
    std: [HANDLE; 3],
    job: &OwnedHandle,
    request: &Request,
) -> Result<Started, SandboxError> {
    let frame = request
        .encode()
        .map_err(|e| failed("write the runner request", e))?;
    // SAFETY: reads this process's ID.
    let owner_pid = unsafe { GetCurrentProcessId() };
    let args = RunnerArgs::fresh(owner_pid).map_err(|e| failed("name the runner pipe", e))?;
    let owner_sid = accounts::current_user_sid()?;
    let server = create_pipe(&args.pipe, &owner_sid, user.sid)?;

    let exe: Vec<u16> = runner_exe.as_os_str().encode_wide().collect();
    if exe.contains(&u16::from(b'"')) {
        return Err(failed(
            "start the runner",
            "the runner's path contains a quote",
        ));
    }
    let mut line = command_line(
        &exe,
        args.to_args()
            .iter()
            .map(|a| a.encode_utf16().collect::<Vec<u16>>()),
    );
    line.push(0);
    let app = wide(&runner_exe.to_string_lossy());
    let cwd = system_directory()?;
    let mut desktop = runner_desktop(&owner_sid, user.sid)?;
    let name = wide(user.name);
    let domain = wide(".");
    // One buffer of the exact size, cleared when it is dropped. `wide` grows
    // its buffer as it encodes, which would leave copies of the password in
    // memory that nothing clears.
    let password = wide_secret(user.password.expose());

    // SAFETY: zeroed is a valid initial STARTUPINFOW.
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.lpDesktop = desktop.as_mut_ptr();
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = std[0];
    si.hStdOutput = std[1];
    si.hStdError = std[2];
    // SAFETY: zeroed is a valid initial PROCESS_INFORMATION.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // No profile is loaded (logon flags 0); the runner's environment is built
    // from the sandbox user's, and the command gets its own from the request.
    // CREATE_SUSPENDED: the runner joins the job before it runs any code.
    // SAFETY: every pointer is NUL-terminated or a valid struct that outlives
    // the call; `line` is mutable as the API requires.
    let ok = unsafe {
        CreateProcessWithLogonW(
            name.as_ptr(),
            domain.as_ptr(),
            password.as_ptr(),
            0,
            app.as_ptr(),
            line.as_mut_ptr(),
            CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            ptr::null(),
            cwd.as_ptr(),
            &si,
            &mut pi,
        )
    };
    let err = std::io::Error::last_os_error();
    drop(password);
    if ok == 0 {
        return Err(logon_failed(user.name, err));
    }
    // SAFETY: fresh handles this code now owns.
    let runner = unsafe { OwnedHandle::from_raw_handle(pi.hProcess) };
    // SAFETY: as above.
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread) };
    // SAFETY: valid job and process handles.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), runner.as_raw_handle()) } == 0 {
        let err = std::io::Error::last_os_error();
        let in_job = in_a_job(&runner);
        // SAFETY: the suspended runner never ran; end it.
        unsafe { TerminateProcess(runner.as_raw_handle(), 1) };
        return Err(failed(
            "put the runner in the command's job",
            format!("{err}; IsProcessInJob(runner) = {in_job}"),
        ));
    }
    let end_job = |e: SandboxError| {
        // SAFETY: a valid job handle; ends the runner and anything it started.
        unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
        e
    };
    // SAFETY: the primary thread of the suspended runner.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        return Err(end_job(last_error("resume the runner")));
    }
    drop(thread);

    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    pipe::wait_for_client(
        &server,
        &args.pipe,
        &runner,
        pi.dwProcessId,
        "wait for the runner",
        "the runner ended before it connected",
        Some(deadline),
        describe_runner_exit,
    )
    .map_err(end_job)?;
    // The pipe is synchronous, so the send and the reply are read on their own
    // thread: ending the job closes the pipe, which releases that thread.
    let (tx, rx) = mpsc::channel();
    let mut file = File::from(server);
    std::thread::spawn(move || {
        let outcome = file
            .write_all(&frame)
            .and_then(|()| file.flush())
            .and_then(|()| {
                let mut reply = String::new();
                BufReader::new((&mut file).take(MAX_REPLY as u64)).read_line(&mut reply)?;
                Ok(reply)
            })
            .map_err(|e| e.to_string());
        let _ = tx.send(outcome);
    });
    let left = deadline.saturating_duration_since(Instant::now());
    let reply = match rx.recv_timeout(left) {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => return Err(end_job(failed("ask the runner to start the command", e))),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return Err(end_job(failed(
                "ask the runner to start the command",
                "the runner did not reply in time",
            )));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(end_job(failed(
                "ask the runner to start the command",
                "the thread that spoke to the runner ended without a reply",
            )));
        }
    };
    match Reply::parse(&reply) {
        Ok(Reply::Started { pid }) => Ok(Started {
            runner,
            command_pid: pid,
        }),
        Ok(failure) => Err(end_job(failed(
            "start the command as the sandbox user",
            failure,
        ))),
        Err(_) if reply.is_empty() => Err(end_job(failed(
            "read the runner's reply",
            "the runner ended without a reply",
        ))),
        Err(e) => Err(end_job(failed("read the runner's reply", e))),
    }
}
