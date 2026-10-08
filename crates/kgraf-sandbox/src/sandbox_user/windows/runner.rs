//! The runner: the first process of a sandboxed command that runs as the
//! sandbox user (decision 0097). The owner's process starts it with
//! `CreateProcessWithLogonW`; it starts the real command and exits with that
//! command's exit code. See [`crate::sandbox_user::runner`] for the protocol.
//!
//! **Logon SID.** `CreateProcessWithLogonW` gives the runner the caller's logon
//! SID, so that it can open the caller's window station. Objects the owner's
//! processes create with their default DACL (pipes, events, sections,
//! processes) grant that SID, so a command that kept it could open them. The
//! runner starts the command with a restricted copy of its own token in which
//! the logon SID is deny-only, checks that it is, and adds no restricting SIDs:
//! restricting SIDs are what make Schannel TLS and the MSYS2 runtime fail in
//! the token sandbox (decision 0011). All privileges but
//! `SeChangeNotifyPrivilege` are removed (`DISABLE_MAX_PRIVILEGE`).
//!
//! **Window station.** With its logon SID deny-only, the command could not open
//! the owner's window station, and every program that loads `user32.dll` would
//! stop at start with STATUS_DLL_INIT_FAILED. The runner creates a window
//! station and a desktop of its own, whose DACLs admit the sandbox user and
//! SYSTEM only, and starts the command there. No object of the owner's session
//! is changed, and the command cannot reach the owner's windows, clipboard or
//! global atoms. Chromium's sandbox runs untrusted processes on an alternate
//! window station the same way (`sandbox/win/src/window.cc`, BSD-3-Clause;
//! followed in design, not copied).
//!
//! **Default DACL.** What the command creates without an inheritable ACL
//! grants the sandbox user and SYSTEM only, never the logon SID.
//!
//! **Stdio and job.** The runner passes its own standard handles (the pipes
//! and files the owner's process gave it) to the command through an explicit
//! handle list. The owner's process put the runner in a job object that kills
//! on close and allows no breakaway, so the command is in that job too.

use std::ffi::c_void;
use std::fs::File;
use std::io::Write;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{
    GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, HLOCAL,
    INVALID_HANDLE_VALUE, LocalFree, SetHandleInformation,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CreateRestrictedToken, DACL_SECURITY_INFORMATION,
    DISABLE_MAX_PRIVILEGE, EqualSid, GetAce, GetSecurityDescriptorDacl, GetTokenInformation,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES, SetTokenInformation,
    TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_GROUPS,
    TOKEN_QUERY, TOKEN_USER, TokenDefaultDacl, TokenGroups, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, OPEN_EXISTING, READ_CONTROL, SYNCHRONIZE, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_UI_RESTRICTIONS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CloseWindowStation, CreateDesktopW, CreateWindowStationW,
    GetProcessWindowStation, HDESK, HWINSTA, SetProcessWindowStation,
};
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, JOB_OBJECT_UILIMIT_ALL, SE_GROUP_LOGON_ID, SE_GROUP_USE_FOR_DENY_ONLY,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetCurrentThread, GetExitCodeProcess,
    INFINITE, OpenProcessToken, PROCESS_CREATE_THREAD, PROCESS_DUP_HANDLE, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SET_QUOTA,
    PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE, PROCESS_VM_OPERATION, PROCESS_VM_READ,
    PROCESS_VM_WRITE, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    THREAD_QUERY_LIMITED_INFORMATION, TerminateProcess, WaitForSingleObject,
};

use super::super::runner::{Reply, Request, RunnerArgs};
use crate::sandbox::AttributeList;

/// Exit codes for a runner that cannot report over its pipe. Each sets bit 29
/// (the customer bit of an NTSTATUS-shaped code), so none is a code a program
/// or Windows itself returns. Once the pipe is connected, a failure is sent
/// as a [`Reply::Failed`] instead, and the owner reads that first.
pub const EXIT_ARGS: i32 = 0x2B47_0001;
pub const EXIT_PIPE: i32 = 0x2B47_0002;
pub const EXIT_FAILED: i32 = 0x2B47_0003;
pub const EXIT_WAIT: i32 = 0x2B47_0004;
pub const EXIT_SELF_PROTECT: i32 = 0x2B47_0005;

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
    if protect_self().is_err() {
        return EXIT_SELF_PROTECT;
    }
    let Ok(mut pipe) = connect(&args) else {
        return EXIT_PIPE;
    };
    let started = start(&mut pipe);
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
        return EXIT_FAILED;
    };
    // The owner learns the command's PID from the reply; without it the owner
    // ends the job, so a reply that did not arrive does not leave the command
    // running unwatched.
    if sent.is_err() {
        return EXIT_PIPE;
    }
    wait(&command.process)
}

/// The running command, with what must stay open while it runs: its window
/// station and desktop (a window station is destroyed with its last handle)
/// and its job, which ends it when the runner exits.
struct Command {
    process: OwnedHandle,
    pid: u32,
    _station: Station,
    _job: OwnedHandle,
}

/// Opens the pipe and checks that its server is the owner's process.
fn connect(args: &RunnerArgs) -> Result<File, Failure> {
    let name = wide(&args.pipe);
    // SAFETY: opens an existing pipe; the handle is owned by the File below.
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
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(Failure::last("open the runner pipe"));
    }
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

/// Reads the request and starts the command.
fn start(pipe: &mut File) -> Result<Command, Failure> {
    let request =
        Request::read_from(pipe).map_err(|e| Failure::other("read the runner request", e))?;
    let token = command_token()?;
    let user = token_user_sid(&token)?;
    let station = Station::create(&user)?;
    let job = command_job()?;
    let (process, pid) = spawn(&request, &token, &station, &job)?;
    Ok(Command {
        process,
        pid,
        _station: station,
        _job: job,
    })
}

/// The command's own job: every UI limit, ended when its last handle closes
/// (the runner's), and no breakaway. The runner's own job, which the owner's
/// process made, carries every UI limit but the one on creating desktops,
/// because the runner creates the command's desktop; this job, a child of
/// it, puts that limit back on the command itself.
fn command_job() -> Result<OwnedHandle, Failure> {
    // SAFETY: anonymous job, default security.
    let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if job.is_null() {
        return Err(Failure::last("create the command's job"));
    }
    // SAFETY: a fresh handle this code now owns.
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    // SAFETY: zeroed is a valid initial value for this plain C struct.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    // SAFETY: a valid struct of the stated size and class.
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            ptr::addr_of!(limits).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(Failure::last("limit the command's job"));
    }
    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_OBJECT_UILIMIT_ALL,
    };
    // SAFETY: as above.
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicUIRestrictions,
            ptr::addr_of!(ui).cast(),
            std::mem::size_of_val(&ui) as u32,
        )
    } == 0
    {
        return Err(Failure::last("set the command's user interface limits"));
    }
    Ok(job)
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

/// A restricted copy of the runner's token: the logon SID deny-only, no
/// privileges but `SeChangeNotifyPrivilege`, no restricting SIDs, and a
/// default DACL that grants the sandbox user and SYSTEM only.
fn command_token() -> Result<OwnedHandle, Failure> {
    let mut raw: HANDLE = ptr::null_mut();
    // SAFETY: opens this process's own token.
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
            &mut raw,
        )
    } == 0
    {
        return Err(Failure::last("open the runner's token"));
    }
    // SAFETY: a fresh handle this code now owns.
    let own = unsafe { OwnedHandle::from_raw_handle(raw) };
    let buf = token_info(&own, TokenGroups, "read the runner's groups")?;
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

/// A SID the system allocated from its text, freed on drop.
struct LocalSid(PSID);

impl LocalSid {
    fn from_string(text: &str) -> Result<Self, Failure> {
        let w = wide(text);
        let mut sid: PSID = ptr::null_mut();
        // SAFETY: `w` is NUL-terminated; the call allocates `sid`, freed on
        // drop.
        if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == 0 {
            return Err(Failure::last("read a SID"));
        }
        Ok(Self(sid))
    }

    fn as_psid(&self) -> PSID {
        self.0
    }
}

impl Drop for LocalSid {
    fn drop(&mut self) {
        // SAFETY: frees the SID the conversion allocated, once.
        unsafe { LocalFree(self.0 as HLOCAL) };
    }
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

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
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
    let mut present = 0;
    let mut defaulted = 0;
    let mut acl: *mut ACL = ptr::null_mut();
    // SAFETY: reads the DACL pointer out of `sd`, which outlives its use.
    if unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut acl, &mut defaulted) } == 0
        || present == 0
        || acl.is_null()
    {
        return Err(Failure::last("read the default access list"));
    }
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
        return Err(Failure::last("set the command's default access list"));
    }
    Ok(())
}

// ---- the runner's own objects -----------------------------------------------

/// What the sandbox user may do to the runner's process: wait for it and read
/// its basic information. Nothing else.
const SELF_PROCESS_MASK: u32 = SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
/// The same for the runner's thread.
const SELF_THREAD_MASK: u32 = SYNCHRONIZE | THREAD_QUERY_LIMITED_INFORMATION;
/// The same for the runner's token: read it, read its access list.
const SELF_TOKEN_MASK: u32 = TOKEN_QUERY | READ_CONTROL;

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
    | WRITE_OWNER;

/// Limit what the sandbox user may do to the runner's own process, thread and
/// token, then confirm it by reading the process's list back.
///
/// The command runs as the same user as the runner. Without this, the access
/// list Windows gives a new process admits that user fully, and the owner of
/// an object also keeps an implicit right to rewrite its list. The lists
/// written here admit SYSTEM fully, the sandbox user only to wait and read,
/// and OWNER RIGHTS to reading the list alone, which removes that implicit
/// right (the same entry the token path writes in `set_default_dacl`).
///
/// Called before the pipe is opened, so the window in which the lists are the
/// defaults ends before the command exists.
fn protect_self() -> Result<(), Failure> {
    let mut raw: HANDLE = ptr::null_mut();
    // SAFETY: opens this process's own token for reading and for writing its
    // access list.
    if unsafe { OpenProcessToken(GetCurrentProcess(), READ_CONTROL | WRITE_DAC, &mut raw) } == 0 {
        return Err(Failure::last("open the runner's token"));
    }
    // SAFETY: a fresh handle this code now owns.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let user = token_user_sid(&token)?;
    // SAFETY: both are pseudo-handles that need no close and carry full
    // access to this process and this thread.
    let (process, thread) = unsafe { (GetCurrentProcess(), GetCurrentThread()) };
    for (handle, mask, what) in [
        (process, SELF_PROCESS_MASK, "process"),
        (thread, SELF_THREAD_MASK, "thread"),
        (token.as_raw_handle(), SELF_TOKEN_MASK, "token"),
    ] {
        write_self_dacl(handle, &user, mask, what)?;
    }
    verify_self_dacl(process, &user)
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
    let mut present = 0;
    let mut defaulted = 0;
    let mut acl: *mut ACL = ptr::null_mut();
    // SAFETY: reads the access list out of `sd`, which outlives its use.
    if unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut acl, &mut defaulted) } == 0
        || present == 0
        || acl.is_null()
    {
        return Err(Failure::last("read the runner's own access list"));
    }
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

/// Read the process's access list back and fail unless every entry that names
/// `user` stays inside [`SELF_PROCESS_MASK`]. A list that is absent means full
/// access for everyone, so that fails too (fail loud).
fn verify_self_dacl(process: HANDLE, user: &str) -> Result<(), Failure> {
    let sid = LocalSid::from_string(user)?;
    let mut acl: *mut ACL = ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: a valid handle; the call allocates `sd`, freed below.
    let code = unsafe {
        GetSecurityInfo(
            process,
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
            message: std::io::Error::from_raw_os_error(code as i32).to_string(),
        });
    }
    let _owned = Descriptor(sd);
    if acl.is_null() {
        return Err(Failure::other(
            "read back the runner's own access list",
            "the runner's process has no access list, which admits every program",
        ));
    }
    // SAFETY: `acl` points into the descriptor freed by `_owned`.
    let count = unsafe { (*acl).AceCount };
    for i in 0..u32::from(count) {
        let mut ace: *mut c_void = ptr::null_mut();
        // SAFETY: `i` is below the entry count of a valid list.
        if unsafe { GetAce(acl, i, &mut ace) } == 0 {
            return Err(Failure::last("read back the runner's own access list"));
        }
        // SAFETY: every entry starts with an ACE_HEADER.
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE {
            continue;
        }
        // SAFETY: an allowed entry has the ACCESS_ALLOWED_ACE layout, whose
        // SID starts at `SidStart`.
        let body = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
        let ace_sid = ptr::addr_of!(body.SidStart) as PSID;
        // SAFETY: both are valid SIDs.
        let same = unsafe { EqualSid(ace_sid, sid.as_psid()) } != 0;
        if same && body.Mask & SELF_FORBIDDEN != 0 {
            return Err(Failure::other(
                "verify access to the runner's own objects",
                format!(
                    "the sandbox user still has access 0x{:08x} to the runner's process",
                    body.Mask & SELF_FORBIDDEN
                ),
            ));
        }
    }
    Ok(())
}

// ---- window station ---------------------------------------------------------

/// A window station and one desktop on it, both admitting the sandbox user
/// and SYSTEM only.
struct Station {
    station: HWINSTA,
    desktop: HDESK,
    /// `<station>\Default`, NUL-terminated, for `STARTUPINFO::lpDesktop`.
    startup_name: Vec<u16>,
}

impl Station {
    fn create(user: &str) -> Result<Self, Failure> {
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce)
            .map_err(|e| Failure::other("name the command's window station", e))?;
        let name = format!(
            "kgraf-sandbox-{}",
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let sd = Descriptor::for_user(user)?;
        let sa = sd.attributes();
        let wname = wide(&name);
        // SAFETY: creates a new window station; `wname` and `sa` live across
        // the call.
        let station = unsafe { CreateWindowStationW(wname.as_ptr(), 0, GENERIC_ALL, &sa) };
        if station.is_null() {
            return Err(Failure::last("create the command's window station"));
        }
        // `CreateDesktopW` creates on the calling process's window station, so
        // switch to the new one and back.
        // SAFETY: reads this process's window station.
        let original = unsafe { GetProcessWindowStation() };
        // SAFETY: `station` is the handle created above.
        if original.is_null() || unsafe { SetProcessWindowStation(station) } == 0 {
            let f = Failure::last("enter the command's window station");
            // SAFETY: closes the station created above.
            unsafe { CloseWindowStation(station) };
            return Err(f);
        }
        let desk = wide("Default");
        // SAFETY: creates a desktop on the window station entered above.
        let desktop =
            unsafe { CreateDesktopW(desk.as_ptr(), ptr::null(), ptr::null(), 0, GENERIC_ALL, &sa) };
        let created = Failure::last("create the command's desktop");
        // SAFETY: returns to the window station this process started on.
        let back = unsafe { SetProcessWindowStation(original) };
        if desktop.is_null() || back == 0 {
            let f = if desktop.is_null() {
                created
            } else {
                Failure::last("leave the command's window station")
            };
            // SAFETY: closes what was created above.
            unsafe {
                if !desktop.is_null() {
                    CloseDesktop(desktop);
                }
                CloseWindowStation(station);
            }
            return Err(f);
        }
        Ok(Self {
            station,
            desktop,
            startup_name: wide(&format!("{name}\\Default")),
        })
    }
}

impl Drop for Station {
    fn drop(&mut self) {
        // SAFETY: closes the handles created in `create`, once.
        unsafe {
            CloseDesktop(self.desktop);
            CloseWindowStation(self.station);
        }
    }
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

fn spawn(
    request: &Request,
    token: &OwnedHandle,
    station: &Station,
    job: &OwnedHandle,
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
    si.StartupInfo.lpDesktop = station.startup_name.as_ptr().cast_mut();
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
    // CREATE_SUSPENDED: the command joins its job before it runs any code.
    let flags = CREATE_SUSPENDED
        | CREATE_UNICODE_ENVIRONMENT
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
    // SAFETY: as above.
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread) };
    // SAFETY: valid job and process handles.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) } == 0 {
        let f = Failure::last("put the command in its job");
        // SAFETY: the suspended command never ran; end it.
        unsafe { TerminateProcess(process.as_raw_handle(), 1) };
        return Err(f);
    }
    // SAFETY: the primary thread of the suspended command.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        let f = Failure::last("start the command running");
        // SAFETY: a valid process handle; the command never ran.
        unsafe { TerminateProcess(process.as_raw_handle(), 1) };
        return Err(f);
    }
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
