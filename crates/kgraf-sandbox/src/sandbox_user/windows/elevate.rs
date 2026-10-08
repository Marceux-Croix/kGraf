//! Starting this program again with administrator rights, through the UAC
//! prompt (`ShellExecuteExW` with the "runas" verb).

use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::System::Threading::GetProcessId;
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::{last_error, wide};
use crate::error::SandboxError;

/// The elevated child: its process handle and ID.
pub struct Elevated {
    pub process: OwnedHandle,
    pub pid: u32,
}

/// Start `exe` with `parameters` (one command line, already quoted) with
/// administrator rights. The user sees the UAC prompt; a refusal is an error
/// (`ERROR_CANCELLED`).
pub fn run_elevated(exe: &str, parameters: &str) -> Result<Elevated, SandboxError> {
    let verb = wide("runas");
    let file = wide(exe);
    let params = wide(parameters);
    // SAFETY: SHELLEXECUTEINFOW is plain data; zeroed is a valid empty value.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.lpDirectory = ptr::null();
    info.nShow = SW_HIDE;
    // SAFETY: `info` points into buffers that live across the call.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(last_error("start the elevated setup step"));
    }
    if info.hProcess.is_null() {
        return Err(last_error("start the elevated setup step"));
    }
    // SAFETY: SEE_MASK_NOCLOSEPROCESS returns a process handle this code now
    // owns.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    // SAFETY: reads the ID of the process just started.
    let pid = unsafe { GetProcessId(info.hProcess) };
    Ok(Elevated { process, pid })
}
