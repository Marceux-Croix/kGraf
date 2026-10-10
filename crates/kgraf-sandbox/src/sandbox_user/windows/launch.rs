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
//! kGraf changes no access list of the owner's session. It creates two window
//! stations, each with one desktop, for each sandbox user, once per process
//! ([`desktops`]), whose access lists admit SYSTEM, the owner and that user:
//! one for the runner, named when it starts the runner, and one for the
//! command, named in the runner request. The runner's start therefore depends
//! on no logon SID and no entry on the owner's window station, and the runner
//! creates no window station itself: whether a local user started through the
//! secondary logon service may do that is not documented. The command's
//! station keeps it away from the owner's windows, clipboard and global atoms,
//! and from the runner's. Each station made is recorded
//! ([`launch_record`]).
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
//!
//! **Runner lists before resume.** While the runner is suspended, this side
//! writes protected access lists on its process and main thread and reads
//! them back (`runner::protect_before_resume`), so no program of the same
//! sandbox user can open them while the runner starts. Whether the handles
//! the logon call returns carry the rights for that write is not measured;
//! a failed write is kept on the started command
//! (`SandboxedChild::runner_lists_before_resume`) and added to any later
//! launch error, and the runner's own protection is then its first step.

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
    ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, GENERIC_ALL, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
    LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, READ_CONTROL,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, IsProcessInJob, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT, WaitNamedPipeW,
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
    GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW,
    TerminateProcess,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CWF_CREATE_ONLY, WINSTA_ALL_ACCESS};

use zeroize::Zeroize;

use super::super::launch_record;
use super::super::password::Password;
use super::super::runner::{
    LogonSidPolicy, MAX_REPLY, Reply, Request, RunnerArgs, describe_runner_exit, logon_error_hint,
};
use super::super::service_args;
use super::super::session0::{self, MAX_REPLY as MAX_LAUNCHER_REPLY, ServiceReply, ServiceRequest};
use super::runner as runner_side;
use super::{accounts, from_wide, last_error, pipe, service, wide, wide_secret};
use crate::error::SandboxError;
use crate::sandbox::InheritWindow;
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
    /// Whether the runner's process and main thread got their protected
    /// access lists before it ran, read back
    /// ([`runner_side::protect_before_resume`]); the error names the step and
    /// the Win32 error.
    pub protected_before_resume: Result<(), String>,
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

/// How long the owner waits for kGraf's launcher service to be running
/// before a command is refused. The service is demand start, so the first
/// command of a session pays for its start.
const SERVICE_START_TIMEOUT: Duration = Duration::from_secs(30);

/// The SID of the LocalSystem account, which kGraf's launcher service runs
/// as. The owner checks it on the server of the launcher pipe, so a program
/// that owns the pipe name before the service does cannot take a request,
/// and with it a sandbox user's password.
const LOCAL_SYSTEM_SID: &str = "S-1-5-18";

/// Start the runner through kGraf's launcher service, in session 0.
///
/// The service starts nothing but the runner, as the sandbox user, with the
/// standard handles and the job of this process; the runner then builds the
/// command's token and speaks to this process over the runner pipe, as it
/// does on the owner-session path. Two things differ:
///
/// - **The logon SID stays enabled** (`LogonSidPolicy::Keep`). The token the
///   service makes comes from `LogonUser` for the sandbox user alone, so its
///   logon SID names a logon session that nothing of the owner's admits, and
///   the object directory of session 0 is reached through it.
/// - **No window station is named.** The service passes an empty desktop, so
///   Windows makes a window station and desktop for the new logon session,
///   and the command joins the runner's, which is the same sandbox user in
///   the same logon session. Nothing of the owner's session is named or
///   changed.
fn via_service(
    user: &Credential<'_>,
    std: [HANDLE; 3],
    job: &OwnedHandle,
    mut request: Request,
) -> Result<Started, SandboxError> {
    let users = super::state::users()?.ok_or_else(|| {
        failed(
            "ask kGraf's launcher service",
            "the sandbox users are not loaded",
        )
    })?;
    // The runner the service was installed with, read from the service's
    // own image path when the users were loaded
    // (`super::super::session0::runner_for`). Taken from there and not from
    // the caller, so no other runner can reach a request: the service would
    // refuse it, and its trust check below would judge the wrong file.
    let runner_exe = users.runner.as_path();
    // SAFETY: reads this process's ID.
    let owner_pid = unsafe { GetCurrentProcessId() };
    let args = RunnerArgs::fresh(owner_pid)
        .map_err(|e| failed("name the runner pipe", e))?
        .with_logon_sid(LogonSidPolicy::Keep);
    let owner_sid = accounts::current_user_sid()?;
    let server = create_pipe(&args.pipe, &owner_sid, user.sid)?;
    // Empty: the window station Windows made for the runner's logon session.
    request.desktop = Vec::new();
    let frame = request
        .encode()
        .map_err(|e| failed("write the runner request", e))?;

    // Where this program and its runner live is checked again here, once per
    // process, and not only at the install.
    //
    // The service runs as the local system account and the owner may start
    // it, so a program file a non-administrator may write is a way to run
    // code as that account. Setup refuses to install from such a place, but
    // a grant written later over a tree that holds this program would open
    // the same door, and Windows reads the image at every start. So a launch
    // asks the same question before it asks the service for anything.
    launcher_image_is_trusted(runner_exe).inspect_err(|_| {
        // SAFETY: a valid job handle; ends anything already in the job.
        unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
    })?;
    // Every failure from here on ends the job, as the owner-session path
    // does: nothing kGraf started may keep running after a launch failed.
    let end_job = |e: SandboxError| {
        // SAFETY: a valid job handle; ends the runner and anything it
        // started.
        unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
        e
    };
    service::start_if_stopped(&users.service, SERVICE_START_TIMEOUT).map_err(end_job)?;
    let pipe_file = connect_to_launcher(&users.service_pipe).map_err(end_job)?;
    let asked = Instant::now();
    let reply = ask_launcher(
        pipe_file,
        &ServiceRequest {
            owner_pid,
            runner_exe: runner_exe.as_os_str().encode_wide().collect(),
            command_line: runner_command_line(runner_exe, &args).map_err(end_job)?,
            user_name: user.name.encode_utf16().collect(),
            user_sid: user.sid.encode_utf16().collect(),
            // One buffer of the exact size: a buffer that grows as it
            // encodes leaves copies of the password in freed memory that
            // nothing clears.
            password: exact_wide(user.password.expose()),
            stdin: std[0] as u64,
            stdout: std[1] as u64,
            stderr: std[2] as u64,
            job: job.as_raw_handle() as u64,
        },
    );
    launch_record::note_launch(session0::LaunchPath::Session0, asked.elapsed(), &reply);
    let (runner_pid, handle, protected) = match reply.map_err(end_job)? {
        ServiceReply::Started {
            pid,
            handle,
            protected,
        } => (pid, handle, protected),
        ServiceReply::Failed { step, code, text } => {
            return Err(end_job(SandboxError::PlatformSetup {
                step: "start the runner through kGraf's launcher service",
                source: std::io::Error::other(
                    ServiceReply::Failed { step, code, text }.to_string(),
                ),
            }));
        }
    };
    // SAFETY: the launcher duplicated this handle into this process and holds
    // no copy of it, so this code now owns it.
    let runner = unsafe { OwnedHandle::from_raw_handle(handle as HANDLE) };
    // The runner is already in this job: the launcher put it there while it
    // was still suspended, so a failure of that step never leaves a command
    // outside the job.
    handshake(
        server, &args.pipe, runner, runner_pid, frame, job, protected,
    )
}

/// Whether this program and the runner at `runner_exe` are still where only
/// administrators may write, read once per process.
///
/// Once per process, not once per command: the answer changes only when an
/// access list above the program changes, a walk of every folder above two
/// paths costs several opens, and a command that starts after such a change
/// is a command of the same process. A failure is kept, so every later
/// command of this process is refused with the same words rather than
/// quietly taking the weaker path.
fn launcher_image_is_trusted(runner_exe: &Path) -> Result<(), SandboxError> {
    static CHECKED: OnceLock<Result<(), String>> = OnceLock::new();
    let outcome = CHECKED.get_or_init(|| {
        let exe = std::env::current_exe().map_err(|e| {
            format!("kGraf's own program could not be found to check where it lives: {e}")
        })?;
        // The bare reason, put in a launch's words: the error that carries it
        // is the one a command gets, so it must say what to do on one line
        // and not wrap an install's whole error inside its own.
        super::setup::first_untrusted(&[&exe, runner_exe])
            .map_err(|why| super::super::trusted_path::launch_refusal_message(&why))
    });
    outcome.clone().map_err(|why| {
        failed(
            "check where kGraf's launcher service starts its program from",
            why,
        )
    })
}

/// `text` as UTF-16 in one buffer of its exact size, with no terminator.
/// For a password: a buffer that grows as it encodes gives up buffers that
/// keep a copy of the text, and nothing clears those.
fn exact_wide(text: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(text.encode_utf16().count());
    out.extend(text.encode_utf16());
    out
}

/// The runner's command line for the launcher: the runner's own path and its
/// arguments, quoted as `CreateProcessAsUser` reads them.
fn runner_command_line(runner_exe: &Path, args: &RunnerArgs) -> Result<Vec<u16>, SandboxError> {
    let exe: Vec<u16> = runner_exe.as_os_str().encode_wide().collect();
    if exe.contains(&u16::from(b'"')) {
        return Err(failed(
            "start the runner",
            "the runner's path contains a quote",
        ));
    }
    Ok(command_line(
        &exe,
        args.to_args()
            .iter()
            .map(|a| a.encode_utf16().collect::<Vec<u16>>()),
    ))
}

/// Open kGraf's launcher pipe and check that its server runs as LocalSystem.
///
/// The connection offers the server identification alone
/// (`SECURITY_IDENTIFICATION`), so a program that owned the name first can
/// read who the owner is and never act as the owner. The server's account is
/// then checked, so such a program gets no request and no password: kGraf
/// speaks only to a pipe a service of the local system holds.
fn connect_to_launcher(pipe: &str) -> Result<File, SandboxError> {
    let name = format!(r"\\.\pipe\{pipe}");
    let w = wide(&name);
    let deadline = kgraf_core::waits::deadline_after(SERVICE_START_TIMEOUT);
    let file = loop {
        // SAFETY: `w` is NUL-terminated; the handle is owned by the File.
        // The exact rights a client needs, never `GENERIC_WRITE`: that
        // carries `FILE_APPEND_DATA`, which is the same bit as
        // `FILE_CREATE_PIPE_INSTANCE`, and the service's access list leaves
        // that bit out on purpose, so a `GENERIC_WRITE` open is refused.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                service_args::PIPE_CLIENT_MASK,
                0,
                ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        };
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            // SAFETY: a fresh handle this code now owns.
            break File::from(unsafe { OwnedHandle::from_raw_handle(h) });
        }
        let err = std::io::Error::last_os_error();
        // Busy: every instance is taken. Not found: the service control
        // manager says the service is running before the service has made
        // its first instance, so the name can be absent for a moment after
        // the start. Both are waited out.
        let again = matches!(
            err.raw_os_error(),
            Some(code) if code == ERROR_PIPE_BUSY as i32 || code == ERROR_FILE_NOT_FOUND as i32
        );
        if !again || Instant::now() >= deadline {
            return Err(SandboxError::PlatformSetup {
                step: "open the pipe of kGraf's launcher service (CreateFileW)",
                source: std::io::Error::other(format!(
                    "{err}; the service may not be running, or another program holds the pipe \
                     name. Run `kgraf sandbox status` to see which launch path this machine \
                     uses"
                )),
            });
        }
        // Every instance is busy: wait for one and try again.
        // SAFETY: `w` is NUL-terminated.
        unsafe { WaitNamedPipeW(w.as_ptr(), 1_000) };
    };
    let mut server_pid = 0u32;
    // SAFETY: reads the pipe server's process ID.
    if unsafe { GetNamedPipeServerProcessId(file.as_raw_handle(), &mut server_pid) } == 0 {
        return Err(last_error(
            "identify the server of kGraf's launcher pipe (GetNamedPipeServerProcessId)",
        ));
    }
    let sid = process_user_sid(server_pid)?;
    if sid != LOCAL_SYSTEM_SID {
        return Err(failed(
            "identify the server of kGraf's launcher pipe",
            format!(
                "the pipe is held by process {server_pid}, which runs as {sid} and not as the \
                 local system account, so it is not kGraf's launcher service. Nothing was sent"
            ),
        ));
    }
    Ok(file)
}

/// The SID of the account process `pid` runs as.
fn process_user_sid(pid: u32) -> Result<String, SandboxError> {
    // SAFETY: opens the process for the one right the token read needs.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(last_error(
            "open the server of kGraf's launcher pipe (OpenProcess)",
        ));
    }
    // SAFETY: a fresh handle this code now owns.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: a valid process handle; `token` is written on success.
    if unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error(
            "read the token of kGraf's launcher pipe's server (OpenProcessToken)",
        ));
    }
    // SAFETY: a fresh handle this code now owns.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut size = 0u32;
    // SAFETY: asks for the size alone; the call fails with the size written.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut size,
        )
    };
    if size == 0 {
        return Err(last_error(
            "size the user of kGraf's launcher pipe's server (GetTokenInformation)",
        ));
    }
    let mut buf = vec![0u8; size as usize];
    // SAFETY: `buf` holds the size the call just asked for.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(last_error(
            "read the user of kGraf's launcher pipe's server (GetTokenInformation)",
        ));
    }
    // SAFETY: the call wrote a TOKEN_USER into `buf`, whose SID pointer is
    // valid while `buf` lives.
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: a valid SID from the token; the call allocates `text`, freed
    // below.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(last_error(
            "name the user of kGraf's launcher pipe's server \
             (ConvertSidToStringSidW)",
        ));
    }
    // SAFETY: a NUL-terminated string the call allocated.
    let sid = unsafe { from_wide(text) };
    // SAFETY: frees what the conversion allocated.
    unsafe { LocalFree(text as HLOCAL) };
    Ok(sid)
}

/// Send one request to kGraf's launcher service and read its one reply line.
/// The password is cleared from the frame as soon as it is written.
fn ask_launcher(pipe: File, request: &ServiceRequest) -> Result<ServiceReply, SandboxError> {
    let mut frame = zeroize::Zeroizing::new(
        request
            .encode()
            .map_err(|e| failed("write the launcher request", e))?,
    );
    // The pipe is synchronous, so the send and the reply are read on their own
    // thread with a deadline, as the runner handshake is. The service bounds
    // its own pipe work, but a thread of its own that stops inside a Windows
    // call never reaches the reply, and a sandboxed command must never block
    // the caller for longer than this. Dropping this side closes the pipe,
    // which releases that thread.
    let (tx, rx) = mpsc::channel();
    let mut sent = zeroize::Zeroizing::new(std::mem::take(&mut *frame));
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let outcome = pipe
            .write_all(&sent)
            .and_then(|()| pipe.flush())
            .and_then(|()| {
                sent.zeroize();
                let mut reply = String::new();
                BufReader::new((&mut pipe).take(MAX_LAUNCHER_REPLY as u64))
                    .read_line(&mut reply)?;
                Ok(reply)
            })
            .map_err(|e| e.to_string());
        let _ = tx.send(outcome);
    });
    let reply = match rx.recv_timeout(HANDSHAKE_TIMEOUT) {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => return Err(failed("ask kGraf's launcher service", e)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return Err(failed(
                "ask kGraf's launcher service",
                "the launcher service did not reply in time",
            ));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(failed(
                "ask kGraf's launcher service",
                "the thread that spoke to the launcher service ended without a reply",
            ));
        }
    };
    if reply.is_empty() {
        return Err(failed(
            "read the launcher reply",
            "kGraf's launcher service ended the connection without a reply",
        ));
    }
    ServiceReply::parse(&reply).map_err(|e| failed("read the launcher reply", e))
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

/// What the sandbox user may do on a window station kGraf makes for it:
/// `WINSTA_ALL_ACCESS` and reading the access list, never rewriting it,
/// taking ownership or deleting it.
const USER_STATION_ACCESS: u32 = WINSTA_ALL_ACCESS as u32 | READ_CONTROL;
/// The same on its desktop: every desktop right but switching to it
/// (`DESKTOP_SWITCHDESKTOP`), and reading the access list.
const USER_DESKTOP_ACCESS: u32 = DESKTOP_READOBJECTS
    | DESKTOP_CREATEWINDOW
    | DESKTOP_CREATEMENU
    | DESKTOP_HOOKCONTROL
    | DESKTOP_JOURNALRECORD
    | DESKTOP_JOURNALPLAYBACK
    | DESKTOP_ENUMERATE
    | DESKTOP_WRITEOBJECTS
    | READ_CONTROL;

/// Who a window station kGraf makes is for. Both kinds are made by this
/// process, never by the sandbox user: whether a local user started through
/// the secondary logon service may create a window station in the session
/// is not documented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// The runner starts here (`STARTUPINFOW::lpDesktop` of the logon call).
    Runner,
    /// The command starts here (named in the runner request). It is a
    /// second station, so the command shares no desktop, clipboard or atom
    /// table with the runner, which holds the owner's logon SID.
    Command,
}

impl Role {
    fn prefix(self) -> &'static str {
        match self {
            Role::Runner => "kgraf-run-",
            Role::Command => "kgraf-cmd-",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Role::Runner => "runner",
            Role::Command => "command",
        }
    }

    // Each step names the Win32 call, so a failure on Windows says which
    // call refused and, through the error's source, with which Win32 error.
    fn steps(self) -> Steps {
        match self {
            Role::Runner => Steps {
                name: "name the runner's window station (getrandom)",
                station_sd: "build the runner window station's access list \
                             (ConvertStringSecurityDescriptorToSecurityDescriptorW)",
                desktop_sd: "build the runner desktop's access list \
                             (ConvertStringSecurityDescriptorToSecurityDescriptorW)",
                create: "make the runner's window station (CreateWindowStationW)",
                current: "read this process's window station (GetProcessWindowStation)",
                enter: "enter the runner's window station (SetProcessWindowStation)",
                desktop: "make the runner's desktop (CreateDesktopW)",
                leave: "leave the runner's window station (SetProcessWindowStation)",
            },
            Role::Command => Steps {
                name: "name the command's window station (getrandom)",
                station_sd: "build the command window station's access list \
                             (ConvertStringSecurityDescriptorToSecurityDescriptorW)",
                desktop_sd: "build the command desktop's access list \
                             (ConvertStringSecurityDescriptorToSecurityDescriptorW)",
                create: "make the command's window station (CreateWindowStationW)",
                current: "read this process's window station (GetProcessWindowStation)",
                enter: "enter the command's window station (SetProcessWindowStation)",
                desktop: "make the command's desktop (CreateDesktopW)",
                leave: "leave the command's window station (SetProcessWindowStation)",
            },
        }
    }
}

struct Steps {
    name: &'static str,
    station_sd: &'static str,
    desktop_sd: &'static str,
    create: &'static str,
    current: &'static str,
    enter: &'static str,
    desktop: &'static str,
    leave: &'static str,
}

/// A window station and its one desktop, made by this process for one
/// sandbox user. Closed on drop; the ones in use are kept for the life of
/// the process ([`desktops`]), since a window station ends with its last
/// handle and a command may start on it at any time.
struct Station {
    station: HWINSTA,
    desktop: HDESK,
    /// `<station>\Default`.
    full_name: String,
}

// SAFETY: window station and desktop handles are process-wide kernel handles
// with no thread affinity, and these are only kept, never used, once made.
unsafe impl Send for Station {}

impl Drop for Station {
    fn drop(&mut self) {
        // SAFETY: closes the handles made in `create`, once.
        unsafe {
            CloseDesktop(self.desktop);
            CloseWindowStation(self.station);
        }
    }
}

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

/// The window stations of one sandbox user in this process.
struct UserStations {
    runner: Station,
    command: Station,
}

/// The desktops this process made for the sandbox user `user_sid`, made on
/// first use: the runner's `<station>\Default`, NUL-terminated for
/// `STARTUPINFOW::lpDesktop`, and the command's `<station>\Default` for the
/// runner request. A failure is not kept, so a later command tries again.
///
/// One pair per sandbox user and process, not per command: making a desktop
/// switches the whole process's window station for a moment (below), and
/// the commands of one sandbox user already share that user's objects.
fn desktops(owner_sid: &str, user_sid: &str) -> Result<(Vec<u16>, String), SandboxError> {
    static STATIONS: OnceLock<Mutex<HashMap<String, UserStations>>> = OnceLock::new();
    let mut stations = STATIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| {
            failed(
                "make the sandbox user's window stations",
                "a lock was poisoned",
            )
        })?;
    if let Some(found) = stations.get(user_sid) {
        return Ok((
            wide(&found.runner.full_name),
            found.command.full_name.clone(),
        ));
    }
    let made = UserStations {
        runner: Station::create(Role::Runner, owner_sid, user_sid)?,
        command: Station::create(Role::Command, owner_sid, user_sid)?,
    };
    let names = (wide(&made.runner.full_name), made.command.full_name.clone());
    stations.insert(user_sid.to_owned(), made);
    Ok(names)
}

/// Who the access lists of both stations and desktops admit, for the record.
const ADMITTED: &str = "SYSTEM, the owner and that sandbox user";

impl Station {
    /// Make a window station and its `Default` desktop for `role`, whose
    /// lists admit SYSTEM fully, the owner fully, and the sandbox user
    /// `user_sid` to [`USER_STATION_ACCESS`] and [`USER_DESKTOP_ACCESS`].
    ///
    /// The owner's entry lets this process make the desktop on the station
    /// it entered; the command holds neither the owner's SID nor, enabled,
    /// the owner's logon SID (`runner::command_token`), so the entry admits
    /// no sandboxed program. Called with the lock of [`desktops`] held, so two
    /// of these never overlap.
    fn create(role: Role, owner_sid: &str, user_sid: &str) -> Result<Self, SandboxError> {
        let steps = role.steps();
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).map_err(|e| failed(steps.name, e))?;
        let name = format!(
            "{}{}",
            role.prefix(),
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        // Both SIDs come from Windows' own SID-to-string conversion, so
        // neither holds SDDL syntax characters. `P`: inherit nothing.
        let station_sd = Sddl::new(
            &format!(
                "D:P(A;;GA;;;SY)(A;;GA;;;{owner_sid})(A;;0x{USER_STATION_ACCESS:08x};;;{user_sid})"
            ),
            steps.station_sd,
        )?;
        let desktop_sd = Sddl::new(
            &format!(
                "D:P(A;;GA;;;SY)(A;;GA;;;{owner_sid})(A;;0x{USER_DESKTOP_ACCESS:08x};;;{user_sid})"
            ),
            steps.desktop_sd,
        )?;
        let wname = wide(&name);
        let sa = station_sd.attributes();
        // SAFETY: creates a new window station (CWF_CREATE_ONLY: never opens
        // one another program made under this name); `wname` and `sa` live
        // across the call.
        let station =
            unsafe { CreateWindowStationW(wname.as_ptr(), CWF_CREATE_ONLY, GENERIC_ALL, &sa) };
        if station.is_null() {
            return Err(last_error(steps.create));
        }
        // `CreateDesktopW` makes the desktop on this process's window
        // station, so enter the new one and return at once. Every thread of
        // this process sees the switch for that moment; it happens twice per
        // sandbox user and process. Chromium's broker does the same.
        // SAFETY: reads this process's window station.
        let original = unsafe { GetProcessWindowStation() };
        if original.is_null() {
            let e = last_error(steps.current);
            // SAFETY: closes the station made above.
            unsafe { CloseWindowStation(station) };
            return Err(e);
        }
        // SAFETY: `station` is the handle made above.
        if unsafe { SetProcessWindowStation(station) } == 0 {
            let e = last_error(steps.enter);
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
        let left = std::io::Error::last_os_error();
        if desktop.is_null() || back == 0 {
            let e = if desktop.is_null() {
                SandboxError::PlatformSetup {
                    step: steps.desktop,
                    source: made,
                }
            } else {
                SandboxError::PlatformSetup {
                    step: steps.leave,
                    source: left,
                }
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
        launch_record::note_station(role.name(), &name, "Default", user_sid, ADMITTED);
        Ok(Self {
            station,
            desktop,
            full_name: format!("{name}\\Default"),
        })
    }
}

/// Start the runner at `runner_exe` as `user`, with `std` as its standard
/// handles, in `job`, and have it start `request`. On the session-0 path the
/// runner is the one the launcher service was installed with
/// ([`super::state::Users::runner`]), which is also what callers pass.
///
/// Which path starts the runner is the one setup proved on this machine and
/// recorded ([`super::super::session0`]): kGraf's launcher service in session
/// 0, where a command can make the named kernel objects cargo and rustc need,
/// or the owner's own session through `CreateProcessWithLogonW`, where it
/// cannot unless setup made the fallback grant. A machine with no record
/// takes the owner's session: the path every setup before the launcher
/// service made. On any failure the job is ended, so nothing started here
/// keeps running.
pub(crate) fn start(
    user: &Credential<'_>,
    runner_exe: &Path,
    std: [HANDLE; 3],
    job: &OwnedHandle,
    request: Request,
) -> Result<Started, SandboxError> {
    let session0 = super::state::users()?
        .is_some_and(|users| users.launch.path == session0::LaunchPath::Session0);
    if session0 {
        return via_service(user, std, job, request);
    }
    in_owner_session(user, runner_exe, std, job, request)
}

/// Start the runner in the owner's own session with
/// `CreateProcessWithLogonW`, on the window station and desktop this process
/// made for this sandbox user, which this function names in the request (any
/// `desktop` the caller set is replaced).
fn in_owner_session(
    user: &Credential<'_>,
    runner_exe: &Path,
    std: [HANDLE; 3],
    job: &OwnedHandle,
    mut request: Request,
) -> Result<Started, SandboxError> {
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
    let (mut desktop, command_desktop) = desktops(&owner_sid, user.sid)?;
    request.desktop = command_desktop.encode_utf16().collect();
    let frame = request
        .encode()
        .map_err(|e| failed("write the runner request", e))?;
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
    // The standard handles are inheritable only for this call, under the
    // spawn lock (`InheritWindow`): the call's handling of non-inheritable
    // standard handles is not documented.
    //
    // The lock is held for the whole call and cannot be held for less: the
    // handles must stay inheritable until seclogon has copied them, at a
    // point inside the call that is not documented, and the call copies
    // every inheritable handle of this process, so no other spawn may run
    // beside it, whatever the standard handles are. The call logs the user
    // on and may take seconds; its duration and the wait for the lock are
    // recorded (`launch_record`) so a Windows run measures what other spawns
    // wait for.
    let asked = Instant::now();
    let window = InheritWindow::open(&std)?;
    let lock_wait = asked.elapsed();
    let called = Instant::now();
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
    launch_record::note_logon(lock_wait, called.elapsed(), ok != 0);
    drop(password);
    if ok == 0 {
        drop(window);
        return Err(logon_failed(user.name, err));
    }
    // SAFETY: fresh handles this code now owns.
    let runner = unsafe { OwnedHandle::from_raw_handle(pi.hProcess) };
    // SAFETY: as above.
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread) };
    if let Err(e) = window.close() {
        // SAFETY: the suspended runner never ran; end it.
        unsafe { TerminateProcess(runner.as_raw_handle(), 1) };
        return Err(e);
    }
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
    // The runner's process and main thread get their protected lists before
    // it runs any code, read back here. The rights of the handles the logon
    // call returns are not documented; a failed write falls back to the
    // runner's own protection as its first step, and the error is kept for
    // the caller (`Started::protected_before_resume`) and for any later
    // launch error.
    let protected = runner_side::protect_before_resume(
        runner.as_raw_handle(),
        thread.as_raw_handle(),
        user.sid,
    );
    let note = note_for_protection(&protected);
    let end_job = |e: SandboxError| {
        // SAFETY: a valid job handle; ends the runner and anything it started.
        unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
        match (&note, e) {
            (Some(note), SandboxError::PlatformSetup { step, source }) => {
                SandboxError::PlatformSetup {
                    step,
                    source: std::io::Error::new(source.kind(), format!("{source}; {note}")),
                }
            }
            (_, e) => e,
        }
    };
    // SAFETY: the primary thread of the suspended runner.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        return Err(end_job(last_error("resume the runner")));
    }
    drop(thread);
    handshake(
        server,
        &args.pipe,
        runner,
        pi.dwProcessId,
        frame,
        job,
        protected,
    )
}

/// The note a failed write of the runner's access lists adds to any later
/// launch error.
fn note_for_protection(protected: &Result<(), String>) -> Option<String> {
    protected
        .as_ref()
        .err()
        .map(|e| format!("also, the runner's access lists could not be written before it ran: {e}"))
}

/// Wait for the runner of process `runner_pid` to connect to `server`, send
/// it `frame` and read its reply. The same for both launch paths: the runner
/// speaks to the owner's process, never to the launcher. On any failure the
/// job is ended, so neither the runner nor the command keeps running.
fn handshake(
    server: OwnedHandle,
    pipe_name: &str,
    runner: OwnedHandle,
    runner_pid: u32,
    frame: Vec<u8>,
    job: &OwnedHandle,
    protected: Result<(), String>,
) -> Result<Started, SandboxError> {
    let note = note_for_protection(&protected);
    let end_job = |e: SandboxError| {
        // SAFETY: a valid job handle; ends the runner and anything it started.
        unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
        match (&note, e) {
            (Some(note), SandboxError::PlatformSetup { step, source }) => {
                SandboxError::PlatformSetup {
                    step,
                    source: std::io::Error::new(source.kind(), format!("{source}; {note}")),
                }
            }
            (_, e) => e,
        }
    };
    let deadline = kgraf_core::waits::deadline_after(HANDSHAKE_TIMEOUT);
    pipe::wait_for_client(
        &server,
        pipe_name,
        &runner,
        runner_pid,
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
            protected_before_resume: protected,
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
