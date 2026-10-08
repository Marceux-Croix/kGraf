//! The owner's side of a command that runs as the sandbox user (decision
//! 0097): create the runner pipe, start the runner with
//! `CreateProcessWithLogonW`, put it in the command's job before it runs,
//! check that the pipe's client is that runner, send the request and read the
//! runner's reply. See [`crate::sandbox_user::runner`] for the protocol and
//! `runner.rs` for the runner itself.
//!
//! **Desktop.** The runner is started on the owner's own window station and
//! desktop, named explicitly. With no desktop named, `CreateProcessWithLogonW`
//! adds an entry for the sandbox user to the DACLs of the caller's window
//! station and desktop; with one named, it changes nothing, and the runner
//! opens them through the caller's logon SID, which that call gives it. The
//! command itself runs on the runner's own window station.
//!
//! **Unverified on Windows:** that seclogon passes pipe and file handles
//! through `STARTF_USESTDHANDLES` to a process of another user (the call's
//! documentation says it does), and that a runner on an explicitly named
//! desktop starts under a hosted runner's service session.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, TerminateJobObject};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessWithLogonW,
    GetCurrentProcessId, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW,
    TerminateProcess,
};

use super::super::password::Password;
use super::super::runner::{MAX_REPLY, Reply, Request, RunnerArgs};
use super::{accounts, last_error, pipe, wide};
use crate::error::SandboxError;
use crate::win_command_line::command_line;

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

/// The owner's own `<window station>\<desktop>`, NUL-terminated.
fn own_desktop() -> Result<Vec<u16>, SandboxError> {
    use windows_sys::Win32::System::StationsAndDesktops::{
        GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW, UOI_NAME,
    };
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    let name = |h: *mut core::ffi::c_void, step: &'static str| -> Result<Vec<u16>, SandboxError> {
        let mut buf = [0u16; 256];
        let mut needed = 0u32;
        // SAFETY: `buf` has the stated size in bytes.
        if unsafe {
            GetUserObjectInformationW(
                h,
                UOI_NAME,
                buf.as_mut_ptr().cast(),
                std::mem::size_of_val(&buf) as u32,
                &mut needed,
            )
        } == 0
        {
            return Err(last_error(step));
        }
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        Ok(buf[..len].to_vec())
    };
    // SAFETY: both calls return pseudo-handles that are not closed.
    let (station, desktop) = unsafe {
        (
            GetProcessWindowStation(),
            GetThreadDesktop(GetCurrentThreadId()),
        )
    };
    if station.is_null() || desktop.is_null() {
        return Err(last_error("find this process's desktop"));
    }
    let mut out = name(station, "name this process's window station")?;
    out.push(u16::from(b'\\'));
    out.extend(name(desktop, "name this process's desktop")?);
    out.push(0);
    Ok(out)
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
    let mut desktop = own_desktop()?;
    let name = wide(user.name);
    let domain = wide(".");
    let mut password = wide(user.password.expose());

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
    password.iter_mut().for_each(|c| *c = 0);
    if ok == 0 {
        return Err(SandboxError::PlatformSetup {
            step: "start the runner as the sandbox user",
            source: err,
        });
    }
    // SAFETY: fresh handles this code now owns.
    let runner = unsafe { OwnedHandle::from_raw_handle(pi.hProcess) };
    // SAFETY: as above.
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread) };
    // SAFETY: valid job and process handles.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), runner.as_raw_handle()) } == 0 {
        let err = last_error("put the runner in the command's job");
        // SAFETY: the suspended runner never ran; end it.
        unsafe { TerminateProcess(runner.as_raw_handle(), 1) };
        return Err(err);
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

    pipe::wait_for_client(
        &server,
        &args.pipe,
        &runner,
        pi.dwProcessId,
        "wait for the runner",
        "the runner ended before it connected (it could not start as the sandbox user, or \
         could not open the pipe)",
    )
    .map_err(end_job)?;
    let mut file = File::from(server);
    file.write_all(&frame)
        .and_then(|()| file.flush())
        .map_err(|e| end_job(failed("send the runner request", e)))?;
    let mut reply = String::new();
    BufReader::new((&mut file).take(MAX_REPLY as u64))
        .read_line(&mut reply)
        .map_err(|e| end_job(failed("read the runner's reply", e)))?;
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
