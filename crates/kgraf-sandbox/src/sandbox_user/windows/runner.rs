//! The runner: the first process of a sandboxed command that runs as the
//! sandbox user (decision 0097). The owner's process starts it with
//! `CreateProcessWithLogonW`; it starts the real command and exits with that
//! command's exit code. See [`crate::sandbox_user::runner`] for the protocol.
//!
//! **Logon SID.** `CreateProcessWithLogonW` gives the runner the caller's logon
//! SID, so that it can open the caller's window station (kGraf does not rely
//! on that: it names a window station of its own for the runner, see
//! `launch.rs`). Objects the owner's processes create with their default DACL
//! (pipes, events, sections, processes) grant that SID, so a command that
//! kept it could open them. The
//! runner starts the command with a restricted copy of its own token in which
//! the logon SID is deny-only, checks that it is, and adds no restricting SIDs:
//! restricting SIDs are what make Schannel TLS and the MSYS2 runtime fail in
//! the token sandbox (decision 0011). All privileges but
//! `SeChangeNotifyPrivilege` are removed (`DISABLE_MAX_PRIVILEGE`).
//!
//! **Window station.** A program that loads `user32.dll` stops at start with
//! STATUS_DLL_INIT_FAILED unless it can open its window station and desktop.
//! The owner's process makes a window station and a desktop for the commands
//! of this sandbox user, whose DACLs admit SYSTEM, the owner and that user,
//! and names them in the request (`launch.rs`); the runner checks that it can
//! open the station ([`command_desktop`]) and starts the command there. The
//! runner creates no window station: whether a local user started through
//! the secondary logon service may do so is not documented. No object of the
//! owner's session is changed, and the command cannot reach the owner's
//! windows, clipboard or global atoms. This, not a job's user interface
//! limits, is the command's user interface isolation. Chromium's sandbox runs
//! untrusted processes on an alternate window station the same way
//! (`sandbox/win/src/window.cc`, BSD-3-Clause; followed in design, not
//! copied), also made by its broker.
//!
//! **Default DACL.** What the command creates without an inheritable ACL
//! grants the sandbox user and SYSTEM only, never the logon SID.
//!
//! **Stdio and job.** The runner passes its own standard handles (the pipes
//! and files the owner's process gave it) to the command through an explicit
//! handle list. The owner's process put the runner in a job object that kills
//! on close, allows no breakaway and sets no user interface limits, and the
//! command is created in that job too. The runner makes no job of its own:
//! Windows nests a job inside another only if neither sets user interface
//! limits, so a second job with them could never take the command.
//!
//! **No console.** The runner is a windows-subsystem program
//! (`#![windows_subsystem = "windows"]` in its binary). A console program
//! gets a console host process that runs as the sandbox user with the
//! runner's token, logon SID enabled, and an access list that admits that
//! user fully, so the command could take it over. A windows-subsystem
//! program gets no console and no window; its standard handles are the ones
//! `STARTF_USESTDHANDLES` gave it, which `GetStdHandle` returns as before.
//!
//! **Self-protection** ([`protect_self`]). The runner and the command run as
//! one user, and the runner holds the logon SID the command must not have.
//! Before it opens the pipe, the runner sets its token's default access list
//! to one that admits the sandbox user only to wait, so every thread made
//! from then on is protected, then writes protected lists on its process,
//! on every thread it already has, and on its token, and reads each back.
//! Before that, while the runner is still suspended, the owner's process has
//! already written lists on the runner's process and main thread and read
//! them back ([`protect_before_resume`]); where that write fails, the runner's
//! own step is the first protection, as before.

use std::ffi::c_void;
use std::fs::File;
use std::io::Write;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, GENERIC_ALL, GENERIC_EXECUTE, GENERIC_READ,
    GENERIC_WRITE, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, HLOCAL, INVALID_HANDLE_VALUE,
    LocalFree, SetHandleInformation,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACL, CreateRestrictedToken, DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE,
    GetSecurityDescriptorDacl, GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, SID_AND_ATTRIBUTES,
    SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_GROUPS, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE,
    TOKEN_GROUPS, TOKEN_IMPERSONATE, TOKEN_QUERY, TOKEN_USER, TokenDefaultDacl, TokenGroups,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, OPEN_EXISTING, READ_CONTROL, SYNCHRONIZE, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
use windows_sys::Win32::System::StationsAndDesktops::{CloseWindowStation, OpenWindowStationW};
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE,
    ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_ALLOWED_COMPOUND_ACE_TYPE,
    ACCESS_ALLOWED_OBJECT_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, ACCESS_DENIED_CALLBACK_ACE_TYPE,
    ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_DENIED_OBJECT_ACE_TYPE, SE_GROUP_LOGON_ID,
    SE_GROUP_USE_FOR_DENY_ONLY,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetCurrentProcessId, GetExitCodeProcess,
    INFINITE, OpenProcessToken, OpenThread, PROCESS_CREATE_THREAD, PROCESS_DUP_HANDLE,
    PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, PROCESS_SET_QUOTA, PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE,
    PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    THREAD_DIRECT_IMPERSONATION, THREAD_GET_CONTEXT, THREAD_IMPERSONATE,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SET_CONTEXT, THREAD_SET_INFORMATION,
    THREAD_SET_LIMITED_INFORMATION, THREAD_SET_THREAD_TOKEN, THREAD_SUSPEND_RESUME,
    THREAD_TERMINATE, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::WINSTA_ENUMDESKTOPS;

use super::super::acl;
use super::super::runner::{Reply, Request, RunnerArgs};
use crate::sandbox::AttributeList;

/// Exit codes for a runner that cannot report over its pipe; defined beside
/// the protocol so the owner can name them ([`describe_runner_exit`]).
///
/// [`describe_runner_exit`]: super::super::runner::describe_runner_exit
pub use super::super::runner::{EXIT_ARGS, EXIT_FAILED, EXIT_PIPE, EXIT_SELF_PROTECT, EXIT_WAIT};

/// A failure before the command started: the step, the Win32 error (0 when
/// none applies), and a description.
struct Failure {
    step: &'static str,
    code: u32,
    message: String,
}

impl Failure {
    fn last(step: &'static str) -> Self {
        // SAFETY: reads the calling thread's last error.
        let code = unsafe { GetLastError() };
        Self {
            step,
            code,
            message: std::io::Error::from_raw_os_error(code as i32).to_string(),
        }
    }

    fn other(step: &'static str, message: impl ToString) -> Self {
        Self {
            step,
            code: 0,
            message: message.to_string(),
        }
    }

    fn reply(&self) -> Reply {
        Reply::Failed {
            step: self.step.to_owned(),
            code: self.code,
            message: self.message.clone(),
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The runner's whole life: returns its exit code.
pub fn main() -> i32 {
    let Ok(args) = RunnerArgs::parse(std::env::args().skip(1)) else {
        return EXIT_ARGS;
    };
    // Before anything else, and before the command exists: the runner's own
    // objects admit the sandbox user to no more than waiting and reading
    // basic information (`protect_self`).
    let protected = protect_self();
    let Ok(mut pipe) = connect(&args) else {
        return if protected.is_err() {
            EXIT_SELF_PROTECT
        } else {
            EXIT_PIPE
        };
    };
    // A failed self-protection is still reported over the pipe, with its
    // step and Win32 error, so the owner's error says what failed. The pipe
    // carries data only, and no command exists yet. The request is read
    // first, because the owner sends it before it reads the reply.
    let failed_early = protected.is_err();
    let started = match protected {
        Ok(token) => start(&mut pipe, &token),
        Err(f) => {
            let _ = Request::read_from(&mut pipe);
            Err(f)
        }
    };
    let (running, reply) = match started {
        Ok(command) => {
            let pid = command.pid;
            (Some(command), Reply::Started { pid })
        }
        Err(f) => (None, f.reply()),
    };
    let sent = pipe
        .write_all(reply.encode().as_bytes())
        .and_then(|()| pipe.flush());
    drop(pipe);
    let Some(command) = running else {
        return if failed_early {
            EXIT_SELF_PROTECT
        } else {
            EXIT_FAILED
        };
    };
    // The owner learns the command's PID from the reply; without it the owner
    // ends the job, so a reply that did not arrive does not leave the command
    // running unwatched.
    if sent.is_err() {
        return EXIT_PIPE;
    }
    wait(&command.process)
}

/// The running command. Its window station and desktop stay open in the
/// owner's process, which made them. The owner's job, which holds the runner
/// and the command, ends it.
struct Command {
    process: OwnedHandle,
    pid: u32,
}

/// How long the runner waits for a pipe another process holds. The owner
/// disconnects any client that is not this runner, so a busy pipe frees
/// again; the owner's own handshake deadline bounds the whole start.
const PIPE_BUSY_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Opens the pipe and checks that its server is the owner's process.
fn connect(args: &RunnerArgs) -> Result<File, Failure> {
    let name = wide(&args.pipe);
    let since = std::time::Instant::now();
    let h = loop {
        // SAFETY: opens an existing pipe; the handle is owned by the File
        // below.
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            break h;
        }
        let failure = Failure::last("open the runner pipe");
        // Another program of this user reached the pipe first (decision
        // 0097: the pipe admits the sandbox user). Wait for the owner to
        // disconnect it.
        if failure.code != ERROR_PIPE_BUSY || since.elapsed() >= PIPE_BUSY_WAIT {
            return Err(failure);
        }
        // SAFETY: `name` is NUL-terminated. The result does not matter: the
        // next open says whether the pipe is free.
        unsafe { WaitNamedPipeW(name.as_ptr(), 1000) };
    };
    // SAFETY: a fresh handle this code now owns.
    let file = File::from(unsafe { OwnedHandle::from_raw_handle(h) });
    let mut server = 0u32;
    // SAFETY: reads the pipe server's process ID.
    if unsafe { GetNamedPipeServerProcessId(file.as_raw_handle(), &mut server) } == 0 {
        return Err(Failure::last("identify the runner pipe's server"));
    }
    if server != args.owner_pid {
        return Err(Failure::other(
            "identify the runner pipe's server",
            format!(
                "the pipe belongs to process {server}, not {}",
                args.owner_pid
            ),
        ));
    }
    Ok(file)
}

/// Reads the request and starts the command with `token`, the command token
/// [`protect_self`] made.
fn start(pipe: &mut File, token: &OwnedHandle) -> Result<Command, Failure> {
    let request =
        Request::read_from(pipe).map_err(|e| Failure::other("read the runner request", e))?;
    let desktop = command_desktop(&request)?;
    let (process, pid) = spawn(&request, token, &desktop)?;
    Ok(Command { process, pid })
}

// ---- token ------------------------------------------------------------------

/// A buffer `GetTokenInformation` filled, aligned for the structure it holds.
fn token_info(token: &OwnedHandle, class: i32, step: &'static str) -> Result<Vec<u64>, Failure> {
    let h = token.as_raw_handle();
    let mut needed = 0u32;
    // SAFETY: size query with a null buffer.
    unsafe { GetTokenInformation(h, class, ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(Failure::last(step));
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `needed` bytes.
    if unsafe { GetTokenInformation(h, class, buf.as_mut_ptr().cast(), needed, &mut needed) } == 0 {
        return Err(Failure::last(step));
    }
    Ok(buf)
}

/// The groups of `buf`, a filled `TOKEN_GROUPS`.
fn groups(buf: &[u64]) -> &[SID_AND_ATTRIBUTES] {
    let groups = buf.as_ptr() as *const TOKEN_GROUPS;
    // SAFETY: the system wrote a TOKEN_GROUPS whose `Groups` array has
    // `GroupCount` entries inside `buf`.
    unsafe { std::slice::from_raw_parts((*groups).Groups.as_ptr(), (*groups).GroupCount as usize) }
}

fn is_logon(g: &SID_AND_ATTRIBUTES) -> bool {
    g.Attributes & (SE_GROUP_LOGON_ID as u32) == SE_GROUP_LOGON_ID as u32
}

/// A restricted copy of the runner's token `own`: the logon SID deny-only, no
/// privileges but `SeChangeNotifyPrivilege`, no restricting SIDs, and a
/// default DACL that grants the sandbox user and SYSTEM only.
fn command_token(own: &OwnedHandle) -> Result<OwnedHandle, Failure> {
    let buf = token_info(own, TokenGroups, "read the runner's groups")?;
    let logon: Vec<SID_AND_ATTRIBUTES> = groups(&buf)
        .iter()
        .filter(|g| is_logon(g))
        .map(|g| SID_AND_ATTRIBUTES {
            Sid: g.Sid,
            Attributes: 0,
        })
        .collect();
    if logon.is_empty() {
        return Err(Failure::other(
            "find the logon SID",
            "the runner's token has no logon SID",
        ));
    }
    let mut restricted: HANDLE = ptr::null_mut();
    // SAFETY: the SID pointers refer into `buf`, alive across the call.
    if unsafe {
        CreateRestrictedToken(
            own.as_raw_handle(),
            DISABLE_MAX_PRIVILEGE,
            logon.len() as u32,
            logon.as_ptr(),
            0,
            ptr::null(),
            0,
            ptr::null(),
            &mut restricted,
        )
    } == 0
    {
        return Err(Failure::last("make the logon SID deny-only"));
    }
    // SAFETY: a fresh handle this code now owns.
    let token = unsafe { OwnedHandle::from_raw_handle(restricted) };
    check_logon_deny_only(&token)?;
    let user = token_user_sid(&token)?;
    set_default_dacl(&token, &user)?;
    Ok(token)
}

/// Fails unless every logon SID in `token` is deny-only. The command must not
/// start with access the design says it lacks (fail loud).
fn check_logon_deny_only(token: &OwnedHandle) -> Result<(), Failure> {
    let buf = token_info(token, TokenGroups, "read the command token's groups")?;
    let deny = SE_GROUP_USE_FOR_DENY_ONLY as u32;
    let mut logon = groups(&buf).iter().filter(|g| is_logon(g)).peekable();
    if logon.peek().is_none() {
        return Err(Failure::other(
            "check the command token",
            "the command token lost its logon SID instead of making it deny-only",
        ));
    }
    if logon.all(|g| g.Attributes & deny == deny) {
        Ok(())
    } else {
        Err(Failure::other(
            "check the command token",
            "the logon SID is still enabled in the command token",
        ))
    }
}

/// The token user's SID as a string (`S-1-5-21-...`).
fn token_user_sid(token: &OwnedHandle) -> Result<String, Failure> {
    let buf = token_info(token, TokenUser, "read the token's user")?;
    let user = buf.as_ptr() as *const TOKEN_USER;
    // SAFETY: the system wrote a TOKEN_USER whose SID points into `buf`.
    let sid: PSID = unsafe { (*user).User.Sid };
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is valid; the call allocates `text`, freed below.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(Failure::last("write the user's SID"));
    }
    let mut len = 0usize;
    // SAFETY: a NUL-terminated string the call allocated.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units were just read from `text`.
    let out = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    // SAFETY: frees the string the conversion allocated.
    unsafe { LocalFree(text as HLOCAL) };
    Ok(out)
}

/// A security descriptor from SDDL, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    /// A protected DACL that grants full access to `sid` and SYSTEM only.
    fn for_user(sid: &str) -> Result<Self, Failure> {
        // `sid` comes from `ConvertSidToStringSidW`, so it holds no SDDL
        // syntax characters.
        Self::from_sddl(&format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)"))
    }

    /// The descriptor the SDDL text `sddl` describes.
    fn from_sddl(sddl: &str) -> Result<Self, Failure> {
        let sddl = wide(sddl);
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated; the call allocates `sd`.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(Failure::last("build an access list for the sandbox user"));
        }
        Ok(Self(sd))
    }

    /// The access list inside the descriptor, valid while it lives.
    fn dacl(&self, step: &'static str) -> Result<*mut ACL, Failure> {
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl: *mut ACL = ptr::null_mut();
        // SAFETY: reads the DACL pointer out of `self`, which outlives its
        // use by the caller.
        if unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut acl, &mut defaulted) } == 0
            || present == 0
            || acl.is_null()
        {
            return Err(Failure::last(step));
        }
        Ok(acl)
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: frees the descriptor the conversion allocated, once.
        unsafe { LocalFree(self.0 as HLOCAL) };
    }
}

fn set_default_dacl(token: &OwnedHandle, user: &str) -> Result<(), Failure> {
    let sd = Descriptor::for_user(user)?;
    apply_default_dacl(token, &sd, "set the command's default access list")
}

/// Make the access list of `sd` the default list of `token`.
fn apply_default_dacl(
    token: &OwnedHandle,
    sd: &Descriptor,
    step: &'static str,
) -> Result<(), Failure> {
    let acl = sd.dacl("read the default access list")?;
    let dacl = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
    // SAFETY: `dacl` points into `sd`, alive across the call.
    if unsafe {
        SetTokenInformation(
            token.as_raw_handle(),
            TokenDefaultDacl,
            ptr::addr_of!(dacl).cast::<c_void>(),
            std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    } == 0
    {
        return Err(Failure::last(step));
    }
    Ok(())
}

// ---- the runner's own objects -----------------------------------------------

/// What the sandbox user may do to the runner's process: wait for it and read
/// its basic information. Nothing else.
const SELF_PROCESS_MASK: u32 = SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
/// The same for the runner's threads.
const SELF_THREAD_MASK: u32 = SYNCHRONIZE | THREAD_QUERY_LIMITED_INFORMATION;
/// The same for the runner's token: read it, read its access list.
const SELF_TOKEN_MASK: u32 = TOKEN_QUERY | READ_CONTROL;

/// Generic rights, which map to a type's specific rights when checked. None
/// may reach the sandbox user on the runner's objects.
const GENERIC_ANY: u32 = GENERIC_ALL | GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE;

/// Access that must not reach the sandbox user on the runner's own process.
/// Any one of these would let a program of that user change how the runner
/// runs, and the runner still holds the owner's logon SID, which the command
/// does not ([`command_token`]).
const SELF_FORBIDDEN: u32 = PROCESS_VM_READ
    | PROCESS_VM_WRITE
    | PROCESS_VM_OPERATION
    | PROCESS_CREATE_THREAD
    | PROCESS_DUP_HANDLE
    | PROCESS_SET_INFORMATION
    | PROCESS_SET_QUOTA
    | PROCESS_SUSPEND_RESUME
    | PROCESS_TERMINATE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_ANY;

/// The same for each of the runner's threads: `THREAD_SET_CONTEXT` alone
/// lets a program queue code to run on the thread.
const THREAD_FORBIDDEN: u32 = THREAD_TERMINATE
    | THREAD_SUSPEND_RESUME
    | THREAD_GET_CONTEXT
    | THREAD_SET_CONTEXT
    | THREAD_SET_INFORMATION
    | THREAD_SET_LIMITED_INFORMATION
    | THREAD_SET_THREAD_TOKEN
    | THREAD_IMPERSONATE
    | THREAD_DIRECT_IMPERSONATION
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_ANY;

/// The same for the runner's token.
const TOKEN_FORBIDDEN: u32 = TOKEN_ASSIGN_PRIMARY
    | TOKEN_DUPLICATE
    | TOKEN_IMPERSONATE
    | TOKEN_ADJUST_PRIVILEGES
    | TOKEN_ADJUST_GROUPS
    | TOKEN_ADJUST_DEFAULT
    | TOKEN_ADJUST_SESSIONID
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_ANY;

/// Limit what the sandbox user may do to the runner's own process, threads
/// and token, confirm it by reading each list back, and return the command
/// token.
///
/// The command runs as the same user as the runner. Without this, the access
/// list Windows gives a new process, thread or token admits that user fully,
/// and the owner of an object also keeps an implicit right to rewrite its
/// list. The lists written here admit SYSTEM fully, the sandbox user only to
/// wait and read, and OWNER RIGHTS to reading the list alone, which removes
/// that implicit right (the same entry the token path writes in
/// `set_default_dacl`).
///
/// Order:
///
/// 1. One handle to the runner's token, with every right used later: once
///    the token's own list is written, this user can no longer open it for
///    more than reading.
/// 2. The command token, made before the runner's default list changes, so
///    the new token object gets the list a token of this user normally has
///    and the command can still open its own token, as programs do.
/// 3. The runner token's default list, which every object the runner makes
///    from then on gets, its threads among them: the sandbox user may only
///    wait on them.
/// 4. The process, every thread that exists already (enumerated with
///    `CreateToolhelp32Snapshot`; a thread made after it already has the
///    list of step 3), and the token, each read back.
///
/// Called before the pipe is opened, so the window in which the lists are the
/// defaults ends before the command exists.
fn protect_self() -> Result<OwnedHandle, Failure> {
    let mut raw: HANDLE = ptr::null_mut();
    // SAFETY: opens this process's own token.
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY
                | TOKEN_DUPLICATE
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_ADJUST_DEFAULT
                | READ_CONTROL
                | WRITE_DAC,
            &mut raw,
        )
    } == 0
    {
        return Err(Failure::last("open the runner's token"));
    }
    // SAFETY: a fresh handle this code now owns.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let user = token_user_sid(&token)?;
    let command = command_token(&token)?;
    set_runner_default_dacl(&token, &user)?;
    // SAFETY: a pseudo-handle that needs no close and carries full access to
    // this process.
    let process = unsafe { GetCurrentProcess() };
    write_self_dacl(process, &user, SELF_PROCESS_MASK, "process")?;
    verify_self_dacl(process, SELF_FORBIDDEN, "process")?;
    protect_threads(&user)?;
    write_self_dacl(token.as_raw_handle(), &user, SELF_TOKEN_MASK, "token")?;
    verify_self_dacl(token.as_raw_handle(), TOKEN_FORBIDDEN, "token")?;
    Ok(command)
}

/// What the sandbox user may do to the runner's process from its creation
/// until its own [`protect_self`] has run: wait for it, nothing else. Without
/// `PROCESS_QUERY_LIMITED_INFORMATION` no program of that user can open the
/// runner's token either (`OpenProcessToken` needs it), while the token's own
/// list is still the default. [`protect_self`] then writes
/// [`SELF_PROCESS_MASK`], after the token is protected.
const PRE_RESUME_PROCESS_MASK: u32 = SYNCHRONIZE;

/// Write the protected lists on the suspended runner's process and main
/// thread from the owner's side, before it runs any code, and read each
/// back: the process admits the sandbox user `user_sid` only to wait
/// ([`PRE_RESUME_PROCESS_MASK`]), the thread as [`protect_self`] would
/// ([`SELF_THREAD_MASK`]). An error names the step and the Win32 error.
///
/// Compatible with the runner's own [`protect_self`]: the runner writes its
/// process list through the pseudo-handle, which needs no access check; it
/// can no longer open its main thread to write, and reads that list back
/// instead, as it does for a thread made after its default list changed.
/// Its token is not touched here: the runner must still open it with every
/// right it uses, which a list written now would refuse.
pub(crate) fn protect_before_resume(
    process: HANDLE,
    thread: HANDLE,
    user_sid: &str,
) -> Result<(), String> {
    let describe = |f: Failure| format!("{}: {} (Win32 error {})", f.step, f.message, f.code);
    write_self_dacl(process, user_sid, PRE_RESUME_PROCESS_MASK, "process").map_err(describe)?;
    verify_self_dacl(
        process,
        SELF_FORBIDDEN | PROCESS_QUERY_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
        "process",
    )
    .map_err(describe)?;
    write_self_dacl(thread, user_sid, SELF_THREAD_MASK, "thread").map_err(describe)?;
    verify_self_dacl(thread, THREAD_FORBIDDEN, "thread").map_err(describe)
}

/// The runner token's default access list: SYSTEM fully, the sandbox user
/// only to wait, OWNER RIGHTS to read the list. `SYNCHRONIZE` means the same
/// for every object type, so this list fits whatever the runner makes.
fn set_runner_default_dacl(token: &OwnedHandle, user: &str) -> Result<(), Failure> {
    // `user` comes from Windows' own SID-to-string conversion, so it holds no
    // SDDL syntax characters.
    let sd = Descriptor::from_sddl(&format!(
        "D:P(A;;GA;;;SY)(A;;0x{SYNCHRONIZE:08x};;;{user})(A;;RC;;;OW)"
    ))?;
    apply_default_dacl(token, &sd, "set the runner's default access list")
}

/// Write and verify the list of every thread of this process that exists
/// now. A thread that ended since the snapshot is skipped. A thread made
/// after the default list changed cannot be opened to write its list, and
/// needs none; its list is read back like the others.
fn protect_threads(user: &str) -> Result<(), Failure> {
    // SAFETY: a snapshot of every thread on the machine; owned below.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap.is_null() || snap == INVALID_HANDLE_VALUE {
        return Err(Failure::last("list the runner's threads"));
    }
    // SAFETY: a fresh handle this code now owns.
    let snap = unsafe { OwnedHandle::from_raw_handle(snap) };
    // SAFETY: reads this process's ID.
    let me = unsafe { GetCurrentProcessId() };
    // SAFETY: zeroed is a valid initial THREADENTRY32; `dwSize` set below.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut seen = 0usize;
    // SAFETY: a valid snapshot handle and entry.
    let mut more = unsafe { Thread32First(snap.as_raw_handle(), &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == me {
            protect_thread(entry.th32ThreadID, user)?;
            seen += 1;
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snap.as_raw_handle(), &mut entry) } != 0;
    }
    if seen == 0 {
        // The calling thread is always in the list: an empty one means the
        // enumeration did not work, and nothing was protected (fail loud).
        return Err(Failure::other(
            "list the runner's threads",
            "the thread list holds none of the runner's own threads",
        ));
    }
    Ok(())
}

fn protect_thread(id: u32, user: &str) -> Result<(), Failure> {
    // SAFETY: opens a thread of this process; owned below.
    let h = unsafe { OpenThread(READ_CONTROL | WRITE_DAC, 0, id) };
    if !h.is_null() {
        // SAFETY: a fresh handle this code now owns.
        let thread = unsafe { OwnedHandle::from_raw_handle(h) };
        write_self_dacl(thread.as_raw_handle(), user, SELF_THREAD_MASK, "thread")?;
        return verify_self_dacl(thread.as_raw_handle(), THREAD_FORBIDDEN, "thread");
    }
    let failure = Failure::last("open a runner thread");
    if failure.code != ERROR_ACCESS_DENIED {
        // The thread ended after the snapshot: nothing to protect.
        return Ok(());
    }
    // Made after the default list changed: it has that list. Read it back.
    // SAFETY: as above.
    let h = unsafe { OpenThread(READ_CONTROL, 0, id) };
    if h.is_null() {
        return Err(Failure::last("read a runner thread's access list"));
    }
    // SAFETY: a fresh handle this code now owns.
    let thread = unsafe { OwnedHandle::from_raw_handle(h) };
    verify_self_dacl(thread.as_raw_handle(), THREAD_FORBIDDEN, "thread")
}

/// Write the protected access list that grants `mask` to `user` on `handle`.
fn write_self_dacl(
    handle: HANDLE,
    user: &str,
    mask: u32,
    what: &'static str,
) -> Result<(), Failure> {
    // `user` comes from Windows' own SID-to-string conversion, so it holds no
    // SDDL syntax characters. `P`: inherit nothing from anywhere.
    let sd = Descriptor::from_sddl(&format!(
        "D:P(A;;GA;;;SY)(A;;0x{mask:08x};;;{user})(A;;RC;;;OW)"
    ))?;
    let acl = sd.dacl("read the runner's own access list")?;
    // SAFETY: a valid handle of this process and a valid list that outlives
    // the call.
    let code = unsafe {
        SetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl,
            ptr::null(),
        )
    };
    if code != 0 {
        return Err(Failure {
            step: "limit access to the runner's own objects",
            code,
            message: format!("{}: {what}", std::io::Error::from_raw_os_error(code as i32)),
        });
    }
    Ok(())
}

/// Read the access list of `handle` back and fail unless every entry that
/// allows access to anyone but SYSTEM stays clear of `forbidden`. A list that
/// is absent means full access for everyone, so that fails too (fail loud).
/// Every entry, not only the sandbox user's, because a group the user is in
/// would serve as well. An allow entry of any type but the plain one (object,
/// callback, compound) fails as well: its SID sits at another offset, and it
/// grants access as surely ([`acl::check_self_acl`]).
fn verify_self_dacl(handle: HANDLE, forbidden: u32, what: &'static str) -> Result<(), Failure> {
    let mut acl: *mut ACL = ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: a valid handle; the call allocates `sd`, freed below.
    let code = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut sd,
        )
    };
    if code != 0 {
        return Err(Failure {
            step: "read back the runner's own access list",
            code,
            message: format!("{}: {what}", std::io::Error::from_raw_os_error(code as i32)),
        });
    }
    let _owned = Descriptor(sd);
    if acl.is_null() {
        return Err(Failure::other(
            "read back the runner's own access list",
            format!("the runner's {what} has no access list, which admits every program"),
        ));
    }
    // SAFETY: `acl` points into the descriptor freed by `_owned`; a valid
    // list's header gives the size of the list and its entries, all inside
    // that descriptor.
    let bytes = unsafe { std::slice::from_raw_parts(acl as *const u8, (*acl).AclSize as usize) };
    acl::check_self_acl(bytes, forbidden).map_err(|problem| {
        Failure::other(
            "verify access to the runner's own objects",
            format!("the runner's {what}: {problem}"),
        )
    })
}

// The entry types the platform-neutral check names are Windows' own.
const _: () = assert!(acl::ACE_ALLOWED as u32 == ACCESS_ALLOWED_ACE_TYPE);
const _: () = assert!(acl::ACE_DENY_TYPES[0] as u32 == ACCESS_DENIED_ACE_TYPE);
const _: () = assert!(acl::ACE_DENY_TYPES[1] as u32 == ACCESS_DENIED_OBJECT_ACE_TYPE);
const _: () = assert!(acl::ACE_DENY_TYPES[2] as u32 == ACCESS_DENIED_CALLBACK_ACE_TYPE);
const _: () = assert!(acl::ACE_DENY_TYPES[3] as u32 == ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE);
const _: () = assert!(acl::ACE_OTHER_ALLOW_TYPES[0] as u32 == ACCESS_ALLOWED_COMPOUND_ACE_TYPE);
const _: () = assert!(acl::ACE_OTHER_ALLOW_TYPES[1] as u32 == ACCESS_ALLOWED_OBJECT_ACE_TYPE);
const _: () = assert!(acl::ACE_OTHER_ALLOW_TYPES[2] as u32 == ACCESS_ALLOWED_CALLBACK_ACE_TYPE);
const _: () =
    assert!(acl::ACE_OTHER_ALLOW_TYPES[3] as u32 == ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE);

// ---- the command's desktop --------------------------------------------------

/// The command's `<window station>\<desktop>` from the request,
/// NUL-terminated for `STARTUPINFOW::lpDesktop`, after checking that this
/// runner can open that window station. The owner's process made the station
/// and its desktop, admitting this sandbox user (`launch.rs`); the runner
/// makes none. Without the check, a station the command cannot open would
/// show only as the command's own exit code `STATUS_DLL_INIT_FAILED`; with
/// it, the failure names the call and the Win32 error.
fn command_desktop(request: &Request) -> Result<Vec<u16>, Failure> {
    let slash = u16::from(b'\\');
    let station: Vec<u16> = request
        .desktop
        .split(|&u| u == slash)
        .next()
        .unwrap_or_default()
        .iter()
        .copied()
        .chain([0])
        .collect();
    // SAFETY: `station` is NUL-terminated; the handle is closed below.
    let h = unsafe { OpenWindowStationW(station.as_ptr(), 0, WINSTA_ENUMDESKTOPS as u32) };
    if h.is_null() {
        return Err(Failure::last(
            "open the command's window station (OpenWindowStationW)",
        ));
    }
    // SAFETY: the handle opened above, closed once.
    unsafe { CloseWindowStation(h) };
    Ok(request.desktop.iter().copied().chain([0]).collect())
}

// ---- start and wait ---------------------------------------------------------

/// The runner's standard handles, marked inheritable; null or invalid ones
/// left out. The same handle given for two streams is listed once.
fn std_handles() -> Result<[HANDLE; 3], Failure> {
    let mut out = [ptr::null_mut(); 3];
    for (slot, which) in out
        .iter_mut()
        .zip([STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE])
    {
        // SAFETY: reads one of this process's standard handles.
        let h = unsafe { GetStdHandle(which) };
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: `h` is a valid handle of this process.
        if unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(Failure::last("pass a standard handle to the command"));
        }
        *slot = h;
    }
    Ok(out)
}

/// Start `request` with `token` on `desktop` (NUL-terminated, from
/// [`command_desktop`]), in the runner's job.
fn spawn(
    request: &Request,
    token: &OwnedHandle,
    desktop: &[u16],
) -> Result<(OwnedHandle, u32), Failure> {
    let std = std_handles()?;
    let mut listed: Vec<HANDLE> = Vec::new();
    for h in std.iter().filter(|h| !h.is_null()) {
        if !listed.contains(h) {
            listed.push(*h);
        }
    }
    let inherit = !listed.is_empty();
    let mut attrs = if inherit {
        Some(
            AttributeList::with_handles(listed)
                .map_err(|e| Failure::other("list the command's handles", e))?,
        )
    } else {
        None
    };
    // SAFETY: zeroed is a valid initial STARTUPINFOEXW.
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.lpDesktop = desktop.as_ptr().cast_mut();
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = std[0];
    si.StartupInfo.hStdOutput = std[1];
    si.StartupInfo.hStdError = std[2];
    if let Some(attrs) = attrs.as_mut() {
        si.lpAttributeList = attrs.as_ptr();
    }
    let app: Vec<u16> = request.application.iter().copied().chain([0]).collect();
    let mut line: Vec<u16> = request.command_line.iter().copied().chain([0]).collect();
    let cwd: Vec<u16> = request.directory.iter().copied().chain([0]).collect();
    // The command is created in the runner's job, the owner's, so it needs
    // no step between its creation and its start.
    let flags = CREATE_UNICODE_ENVIRONMENT
        | CREATE_NO_WINDOW
        | if inherit {
            EXTENDED_STARTUPINFO_PRESENT
        } else {
            0
        };
    // SAFETY: zeroed is a valid initial PROCESS_INFORMATION.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: every pointer is NUL-terminated or a valid struct that outlives
    // the call; `line` is mutable as the API requires; the environment block
    // was checked to end in two NULs.
    let ok = unsafe {
        CreateProcessAsUserW(
            token.as_raw_handle(),
            app.as_ptr(),
            line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            i32::from(inherit),
            flags,
            request.environment.as_ptr().cast(),
            cwd.as_ptr(),
            ptr::addr_of!(si.StartupInfo),
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(Failure::last("start the command"));
    }
    drop(attrs);
    // SAFETY: fresh handles this code now owns.
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess) };
    // SAFETY: as above; closed at once, the process handle is enough.
    drop(unsafe { OwnedHandle::from_raw_handle(pi.hThread) });
    Ok((process, pi.dwProcessId))
}

/// Waits for the command and returns its exit code as the runner's own.
fn wait(process: &OwnedHandle) -> i32 {
    // SAFETY: a valid process handle.
    unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) };
    let mut code = 0u32;
    // SAFETY: a valid process handle, now signaled.
    if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
        return EXIT_WAIT;
    }
    code as i32
}
