//! The launcher service itself: it starts kGraf's runner as one slot user in
//! session 0, and does nothing else (decision 0097).
//!
//! A command that runs as a sandbox user in the owner's own Windows session
//! can make or open no named kernel object: `kernelbase.dll` opens
//! `\Sessions\<n>\BaseNamedObjects` with explicit rights before it looks up
//! any name, that directory admits the session's logon SID, and the command
//! holds that SID deny-only. cargo needs a named semaphore and rustc needs
//! the unprefixed mutex `rustc_errors` for every diagnostic, so no Rust build
//! runs. A process in session 0, the session Windows gives its services,
//! resolves an unprefixed name in the machine-wide `\BaseNamedObjects`
//! instead (Microsoft, "Kernel Object Namespaces": a service application uses
//! the global namespace by default). This service puts the runner there.
//!
//! What it does **not** do: it never runs an arbitrary command, it never runs
//! anything as SYSTEM, and it widens no access list. The program it starts is
//! the one path an administrator installed it with; the command itself comes
//! over the owner-to-runner pipe and still runs with the token the runner
//! builds ([`super::runner`]).
//!
//! ```text
//!   owner (the owner's session)              launcher service (session 0)
//!     connect \\.\pipe\<name>  ------------>  accept, next instance at once
//!     --- ServiceRequest ------------------>  read the whole request
//!                                             impersonate: client SID == owner?
//!                                             client PID == request PID?
//!                                             runner path == installed path?
//!                                             the process still runs as the
//!                                               owner's own user?
//!                                             duplicate 3 stdio + the job out
//!                                               of that process
//!                                             the job ends what it holds?
//!                                             LogonUser(slot user, INTERACTIVE)
//!                                             CreateEnvironmentBlock
//!                                             CreateProcessAsUser(suspended,
//!                                               handle list + job list)
//!                                             confirm the job membership
//!                                             write the runner's lists
//!                                             ResumeThread
//!                                             duplicate the runner's process
//!                                               handle into the owner, to
//!                                               wait on and read
//!     <--- "started <pid> <handle> ..." ----   close the instance
//! ```
//!
//! The owner's side opens the pipe with `SECURITY_SQOS_PRESENT |
//! SECURITY_IDENTIFICATION`, so this service may learn who the client is and
//! may not act as that client anywhere else, and it refuses a pipe whose
//! server process is not LocalSystem. Both are the owner's side, not this
//! one.
//!
//! **Where a failure is read.** Every refusal and every failure after a client
//! connects is the reply on that client's pipe instance
//! ([`ServiceReply::Failed`], with the step and the Win32 error), which the
//! owner's side reads and reports; the owner is the only client the pipe's
//! access list admits. A failure before that, where there is no client to
//! answer, is the service's own stop: the Win32 error goes in
//! `SERVICE_STATUS::dwWin32ExitCode`, which the service control manager
//! reports, and in this process's exit code ([`EXIT_PIPE`] and the codes
//! beside it). No password, command line or environment is ever reported.
//!
//! **Stop.** `SERVICE_CONTROL_STOP` and `SERVICE_CONTROL_SHUTDOWN` end the
//! accept loop and report `SERVICE_STOPPED` within a bounded time. A command
//! that is already running is never ended by a stop: the owner's job owns the
//! command's life, not the service. Nothing else stops the service: once it is
//! started it keeps answering until it is stopped or removed, so a command
//! ending does not end it.
//!
//! **The pipe before `SERVICE_RUNNING`.** The first instance of the pipe is
//! created before the service reports that it is running, so a client that
//! opens the pipe as soon as the service control manager says `RUNNING` finds
//! the name. A first instance that cannot be created stops the service with
//! the Win32 error instead, which is also what a name another program already
//! holds gives (`FILE_FLAG_FIRST_PIPE_INSTANCE`).

use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

use windows_sys::Wdk::Foundation::{NtQueryObject, ObjectTypeInformation};
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_BROKEN_PIPE, ERROR_CALL_NOT_IMPLEMENTED,
    ERROR_PIPE_CONNECTED, GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree, NO_ERROR,
    UNICODE_STRING,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, LOGON32_LOGON_INTERACTIVE, LOGON32_PROVIDER_DEFAULT, LogonUserW,
    PSECURITY_DESCRIPTOR, PSID, RevertToSelf, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, PIPE_ACCESS_DUPLEX, ReadFile, SYNCHRONIZE,
    WriteFile,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, IsProcessInJob, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, QueryInformationJobObject,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, ImpersonateNamedPipeClient,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT,
};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    GetCurrentThread, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess,
    OpenProcessToken, OpenThreadToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_DUP_HANDLE, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute,
};
use zeroize::Zeroizing;

use super::super::names::is_sid_string;
use super::super::service_args::{
    PIPE_OWNER_MASK, ServiceArgs, handle_type_allowed, job_limits_allowed, same_runner_path,
};
use super::super::session0::{ServiceReply, ServiceRequest};
use super::{from_wide, wide};

/// Exit codes for a service that cannot report over a pipe, because no client
/// is connected. Each sets bit 29 (the customer bit of an NTSTATUS-shaped
/// code), so none is a code Windows itself returns.
pub const EXIT_DISPATCH: i32 = 0x2B48_0001;
pub const EXIT_HANDLER: i32 = 0x2B48_0002;
pub const EXIT_PIPE: i32 = 0x2B48_0003;
pub const EXIT_TWICE: i32 = 0x2B48_0004;

/// How long one client's whole request may take: connect, send, be checked,
/// and have the runner started and answered. This is the service's own bound
/// and the only one it can enforce: when it passes, the client's pipe I/O is
/// cancelled and the client reads a broken pipe rather than waiting on.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a stop waits for the clients still in flight once their pipe I/O
/// is cancelled, before it reports `SERVICE_STOPPED` anyway. A started
/// command is not affected: the owner's job owns it.
const STOP_WAIT: Duration = Duration::from_secs(20);

/// How often the accept loop looks at the stop flag and at the deadline of
/// each client in flight.
const TICK: Duration = Duration::from_millis(200);

/// How many requests may be in flight at once. One per sandboxed command
/// start, which takes about a second, so a full list means a defect or a
/// flood; a further client is refused with a reply and its instance closed,
/// never left to pile up threads.
const MAX_CLIENTS: usize = 64;

/// The arguments the service control manager started this process with,
/// parsed before the dispatcher is called. The service's entry point is a
/// plain function pointer and can carry nothing, so they live here; they are
/// read only, and set once.
static ARGS: OnceLock<ServiceArgs> = OnceLock::new();
/// The handle `SetServiceStatus` needs, which the control handler also uses.
static STATUS: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
/// Set by the control handler; the accept loop ends on it.
static STOP: AtomicBool = AtomicBool::new(false);
/// What this process exits with, set where no client can be told.
static RESULT: AtomicI32 = AtomicI32::new(0);

/// The service's whole life: returns the process's exit code.
///
/// The caller parses the command line into `args`
/// ([`ServiceArgs::parse`]) and calls this once.
pub fn main(args: ServiceArgs) -> i32 {
    if ARGS.set(args).is_err() {
        return EXIT_TWICE;
    }
    // For `SERVICE_WIN32_OWN_PROCESS` the service control manager ignores the
    // name in the dispatch table, but it may not be null.
    let mut name = wide("");
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: a table of two entries, the second a null terminator as the API
    // requires; `name` and `table` live across the call, which returns only
    // when the service has stopped.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return EXIT_DISPATCH;
    }
    RESULT.load(Ordering::SeqCst)
}

/// The service's entry point, called on a thread of the service control
/// manager's making.
///
/// The arguments the manager passes here are not read: they come from
/// whoever started the service, and the service acts only on the command line
/// an administrator installed, which [`main`] has already parsed.
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    // SAFETY: an empty name, which this service type ignores; the handler is
    // a plain function pointer with the required signature, and the context
    // is unused.
    let handle = unsafe {
        let name = wide("");
        RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control_handler), ptr::null())
    };
    if handle.is_null() {
        // Nothing can be reported without this handle: the service control
        // manager sees the process exit instead.
        RESULT.store(EXIT_HANDLER, Ordering::SeqCst);
        return;
    }
    STATUS.store(handle, Ordering::SeqCst);
    report(SERVICE_START_PENDING, 0, Duration::from_secs(10), 1);
    let Some(args) = ARGS.get() else {
        report(
            SERVICE_STOPPED,
            ERROR_CALL_NOT_IMPLEMENTED,
            Duration::ZERO,
            0,
        );
        RESULT.store(EXIT_TWICE, Ordering::SeqCst);
        return;
    };
    // The pipe exists before the service says it is running: a client that
    // opens it the moment the service control manager reports `RUNNING` would
    // otherwise fail at once with `ERROR_FILE_NOT_FOUND`.
    let listening = match Listening::open(args) {
        Ok(listening) => listening,
        Err(failure) => {
            RESULT.store(EXIT_PIPE, Ordering::SeqCst);
            report(SERVICE_STOPPED, failure.code, Duration::ZERO, 0);
            return;
        }
    };
    report(SERVICE_RUNNING, 0, Duration::ZERO, 0);
    let code = match serve(args, listening) {
        Ok(()) => 0,
        Err(failure) => {
            RESULT.store(EXIT_PIPE, Ordering::SeqCst);
            failure.code
        }
    };
    report(SERVICE_STOPPED, code, Duration::ZERO, 0);
}

/// The one control handler. Only stop and shutdown are accepted; anything
/// else is answered with `ERROR_CALL_NOT_IMPLEMENTED`, as the API asks.
unsafe extern "system" fn control_handler(
    control: u32,
    _event: u32,
    _data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOP.store(true, Ordering::SeqCst);
            report(SERVICE_STOP_PENDING, 0, STOP_WAIT + TICK, 1);
            NO_ERROR
        }
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

/// Tell the service control manager where the service is. A failure cannot be
/// acted on: the manager stops the service itself when it hears nothing.
fn report(state: u32, win32_code: u32, wait_hint: Duration, checkpoint: u32) {
    let handle: SERVICE_STATUS_HANDLE = STATUS.load(Ordering::SeqCst);
    if handle.is_null() {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: win32_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: u32::try_from(wait_hint.as_millis()).unwrap_or(u32::MAX),
    };
    // SAFETY: a handle this service registered and a fully initialized
    // status that lives across the call.
    unsafe { SetServiceStatus(handle, &status) };
}

// ---- failures ---------------------------------------------------------------

/// A step that failed, with the Win32 error it failed with (0 where none
/// applies). The step is a short phrase, so one reply line names it. Holds no
/// password, command line or environment, so it is safe to print.
#[derive(Debug)]
struct Failure {
    step: &'static str,
    code: u32,
    text: String,
}

impl Failure {
    /// `step` with the calling thread's last error.
    fn last(step: &'static str) -> Self {
        // SAFETY: reads the calling thread's last error.
        let code = unsafe { GetLastError() };
        Self {
            step,
            code,
            text: io::Error::from_raw_os_error(code as i32).to_string(),
        }
    }

    fn other(step: &'static str, text: impl ToString) -> Self {
        Self {
            step,
            code: 0,
            text: text.to_string(),
        }
    }

    fn with(step: &'static str, code: u32, text: impl ToString) -> Self {
        Self {
            step,
            code,
            text: text.to_string(),
        }
    }

    fn reply(self) -> ServiceReply {
        ServiceReply::Failed {
            step: self.step.to_owned(),
            code: self.code,
            text: self.text,
        }
    }
}

// ---- the pipe ---------------------------------------------------------------

/// A security descriptor from SDDL, freed on drop.
struct Sddl(PSECURITY_DESCRIPTOR);

impl Sddl {
    fn new(text: &str) -> Result<Self, Failure> {
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
            return Err(Failure::last(
                "build the launcher pipe's access list \
                 (ConvertStringSecurityDescriptorToSecurityDescriptorW)",
            ));
        }
        Ok(Self(sd))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            // No instance of the pipe is inherited by the runner the service
            // starts.
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

/// Make one instance of the service's pipe.
///
/// The first instance carries `FILE_FLAG_FIRST_PIPE_INSTANCE`, so the service
/// fails loud instead of sharing a name another program already owns. Remote
/// clients are rejected, the pipe is a byte stream in both directions, and its
/// access list admits SYSTEM, which the service runs as, and the owner alone,
/// without `FILE_CREATE_PIPE_INSTANCE` ([`PIPE_OWNER_MASK`]): a program
/// running as the owner must not be able to add an instance of the name and
/// answer in the service's place.
///
/// Windows checks `FILE_CREATE_PIPE_INSTANCE` on the existing pipe before it
/// makes another instance, so only SYSTEM can call this with `first` false.
/// The service runs as SYSTEM; a caller running as the owner is refused with
/// `ERROR_ACCESS_DENIED`, which is the property above.
fn create_instance(
    path: &[u16],
    sa: &SECURITY_ATTRIBUTES,
    first: bool,
) -> Result<OwnedHandle, Failure> {
    let mode = if first {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        PIPE_ACCESS_DUPLEX
    };
    // SAFETY: `path` is NUL-terminated and `sa` lives across the call.
    let h = unsafe {
        CreateNamedPipeW(
            path.as_ptr(),
            mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            sa,
        )
    };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(Failure::last("create the launcher pipe (CreateNamedPipeW)"));
    }
    // SAFETY: a fresh handle this code now owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

/// Reads and writes one pipe instance. The instance is shared with the accept
/// loop, which cancels this I/O when the client's deadline passes or the
/// service stops, so both ends use the same handle: `CancelIoEx` acts on the
/// handle the request was issued on.
struct PipeIo(Arc<OwnedHandle>);

impl Read for PipeIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let mut read = 0u32;
        // SAFETY: a valid pipe handle and a buffer of `len` bytes.
        let ok = unsafe {
            ReadFile(
                self.0.as_raw_handle(),
                buf.as_mut_ptr().cast(),
                len,
                &mut read,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            // The client closed its end: the end of the request.
            if e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                return Ok(0);
            }
            return Err(e);
        }
        Ok(read as usize)
    }
}

impl Write for PipeIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let mut written = 0u32;
        // SAFETY: a valid pipe handle and a buffer of `len` bytes.
        let ok = unsafe {
            WriteFile(
                self.0.as_raw_handle(),
                buf.as_ptr(),
                len,
                &mut written,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(written as usize)
    }

    /// Waits until the client has read what was written, so the reply is not
    /// lost when the instance is closed. The client's deadline bounds the
    /// wait: a client that never reads has its I/O cancelled.
    fn flush(&mut self) -> io::Result<()> {
        // SAFETY: a valid pipe handle.
        if unsafe { FlushFileBuffers(self.0.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Cancel every pending read or write on `pipe`, so the thread blocked on it
/// returns. The result is not acted on: there may be no I/O to cancel.
fn cancel(pipe: &OwnedHandle) {
    // SAFETY: a valid handle; a null overlapped cancels every request on it,
    // whichever thread issued them.
    unsafe { CancelIoEx(pipe.as_raw_handle(), ptr::null()) };
}

/// What the service needs to accept a client: the pipe's access list, its
/// path, and the first instance of it.
///
/// The first instance is created before the service reports `SERVICE_RUNNING`,
/// and this type carries it into the accept loop, which uses it for the first
/// client and makes the instances after it with [`Listening::next`].
struct Listening {
    sddl: Sddl,
    path: Vec<u16>,
    first: Option<OwnedHandle>,
}

impl Listening {
    /// Build the access list and create the first instance of the pipe. A
    /// failure here stops the service: there is no client to answer yet.
    fn open(args: &ServiceArgs) -> Result<Self, Failure> {
        let sddl = Sddl::new(&args.pipe_sddl())?;
        let path = wide(&args.pipe_path());
        let first = create_instance(&path, &sddl.attributes(), true)?;
        Ok(Self {
            sddl,
            path,
            first: Some(first),
        })
    }

    /// Another instance of the same pipe, with the same access list.
    fn next(&self) -> Result<OwnedHandle, Failure> {
        create_instance(&self.path, &self.sddl.attributes(), false)
    }
}

/// One request being served.
struct InFlight {
    pipe: Arc<OwnedHandle>,
    deadline: Instant,
    done: Arc<AtomicBool>,
    cancelled: bool,
}

/// Marks one client finished whatever ends its thread, a panic included.
///
/// The accept loop forgets a client only when its flag is set, and refuses a
/// new client while [`MAX_CLIENTS`] are in flight. A thread that panicked
/// without setting the flag would hold its place for the service's whole life,
/// so the store happens in a drop, which a panic also runs.
struct DoneOnDrop(Arc<AtomicBool>);

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

// ---- the accept loop --------------------------------------------------------

/// Serve requests until the service is told to stop.
///
/// Each instance is handed to its own thread and the next instance is created
/// at once, so one client cannot keep the next from connecting. This loop is
/// also the watchdog: every tick it cancels the pipe I/O of a client whose
/// deadline has passed, which releases that client's thread.
fn serve(args: &'static ServiceArgs, mut listening: Listening) -> Result<(), Failure> {
    let mut clients: Vec<InFlight> = Vec::new();
    let mut ready = listening.first.take();
    while !STOP.load(Ordering::SeqCst) {
        let instance = Arc::new(match ready.take() {
            Some(first) => first,
            None => listening.next()?,
        });
        let waiting = instance.clone();
        let (tx, rx) = mpsc::channel::<Result<(), u32>>();
        // The connect thread holds its own reference, so the handle outlives
        // the blocking call whatever this loop does with its own.
        std::thread::spawn(move || {
            // SAFETY: a blocking connect on a valid pipe instance this thread
            // holds a reference to.
            let ok = unsafe { ConnectNamedPipe(waiting.as_raw_handle(), ptr::null_mut()) };
            // SAFETY: reads this thread's last error.
            let err = unsafe { GetLastError() };
            let _ = tx.send(if ok != 0 || err == ERROR_PIPE_CONNECTED {
                Ok(())
            } else {
                Err(err)
            });
        });
        let connected = loop {
            match rx.recv_timeout(TICK) {
                Ok(Ok(())) => break true,
                // A client that went away before it was accepted, or a
                // cancelled connect while stopping: this instance is spent,
                // and the loop makes another.
                Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => break false,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    watch(&mut clients);
                    if STOP.load(Ordering::SeqCst) {
                        cancel(&instance);
                        break false;
                    }
                }
            }
        };
        if !connected {
            // A connect that failed at once would otherwise spin this loop,
            // making an instance per turn.
            std::thread::sleep(TICK);
            continue;
        }
        watch(&mut clients);
        if clients.len() >= MAX_CLIENTS {
            let mut io = PipeIo(instance);
            answer(
                &mut io,
                Failure::other(
                    "take another request",
                    format!("{MAX_CLIENTS} requests are already being served"),
                )
                .reply(),
            );
            continue;
        }
        let done = Arc::new(AtomicBool::new(false));
        clients.push(InFlight {
            pipe: instance.clone(),
            deadline: Instant::now() + CLIENT_TIMEOUT,
            done: done.clone(),
            cancelled: false,
        });
        std::thread::spawn(move || {
            // Set from the guard's drop, so a panic on this thread also frees
            // the client's place in the list.
            let _done = DoneOnDrop(done);
            let mut io = PipeIo(instance);
            let reply = match serve_client(&mut io, args) {
                Ok(reply) => reply,
                Err(failure) => failure.reply(),
            };
            answer(&mut io, reply);
        });
    }
    stop(clients);
    Ok(())
}

/// Cancel the pipe I/O of every client whose deadline has passed, and forget
/// the ones that are finished.
fn watch(clients: &mut Vec<InFlight>) {
    let now = Instant::now();
    for client in clients.iter_mut() {
        if !client.cancelled && !client.done.load(Ordering::SeqCst) && now >= client.deadline {
            cancel(&client.pipe);
            client.cancelled = true;
        }
    }
    clients.retain(|c| !c.done.load(Ordering::SeqCst));
}

/// End the clients still in flight: cancel each one's pipe I/O, then wait up
/// to [`STOP_WAIT`] for its thread to finish. A command already started is
/// untouched; the owner's job owns it.
fn stop(mut clients: Vec<InFlight>) {
    for client in &clients {
        cancel(&client.pipe);
    }
    let until = Instant::now() + STOP_WAIT;
    let mut checkpoint = 2;
    while !clients.is_empty() && Instant::now() < until {
        std::thread::sleep(TICK);
        clients.retain(|c| !c.done.load(Ordering::SeqCst));
        checkpoint += 1;
        report(SERVICE_STOP_PENDING, 0, STOP_WAIT, checkpoint);
    }
}

/// Send one reply line and wait for the client to read it. A write that fails
/// cannot be reported anywhere else: the client is gone.
fn answer(io: &mut PipeIo, reply: ServiceReply) {
    let line = reply.line();
    let _ = io.write_all(line.as_bytes()).and_then(|()| io.flush());
}

// ---- one request ------------------------------------------------------------

/// Read one request, check who sent it and what it asks for, and start the
/// runner.
fn serve_client(io: &mut PipeIo, args: &ServiceArgs) -> Result<ServiceReply, Failure> {
    // The whole request is read first, because impersonation uses the
    // security context of the last message read from the pipe.
    let request =
        ServiceRequest::decode(io).map_err(|e| Failure::other("read the launcher request", e))?;
    check_client(&io.0, &request, args)?;
    let handles = duplicate(&request, args)?;
    let token = logon(&request)?;
    let environment = Environment::of(&token)?;
    let (pid, handle, protected) = start(args, &request, &token, &handles, &environment)?;
    Ok(ServiceReply::Started {
        pid,
        handle,
        protected,
    })
}

/// Who may ask, and for what. Every failure is a refusal: a request that
/// cannot be shown to come from the owner is never acted on.
///
/// The order is fixed. The request is already read, because
/// `ImpersonateNamedPipeClient` works on the context of the last message read
/// from the pipe. Then the client is impersonated, its thread token is opened
/// and its user SID read, and `RevertToSelf` runs on every path out,
/// including every error path: a thread left impersonating would run the rest
/// of the work as the client.
///
/// The user SID is the check that matters: the pipe's access list admits
/// SYSTEM and the owner alone, and this confirms the client's own token
/// carries the owner's SID. The client's process ID is a second, weaker
/// check, because Windows reuses process IDs; it only ties the request's
/// `owner_pid`, out of which handles are duplicated, to the process on the
/// other end of this pipe instance.
fn check_client(
    pipe: &OwnedHandle,
    request: &ServiceRequest,
    args: &ServiceArgs,
) -> Result<(), Failure> {
    let sid = client_sid(pipe)?;
    if sid != args.owner_sid {
        return Err(Failure::other(
            "refuse a request from another user",
            format!("the request came from {sid}, not from the owner"),
        ));
    }
    let mut client_pid = 0u32;
    // SAFETY: a valid pipe instance with a client connected.
    if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut client_pid) } == 0 {
        return Err(Failure::last(
            "read the client's process (GetNamedPipeClientProcessId)",
        ));
    }
    if client_pid != request.owner_pid {
        return Err(Failure::other(
            "refuse a request from another process",
            format!(
                "the client is process {client_pid}, and the request names {}",
                request.owner_pid
            ),
        ));
    }
    // The service starts only the runner an administrator installed it with.
    // Windows compares a file name without regard to case, and so does this.
    let installed: Vec<u16> = args.runner.encode_utf16().collect();
    if !same_runner_path(&installed, &request.runner_exe) {
        return Err(Failure::other(
            "refuse a request for another program",
            "the request names a runner the service was not installed with",
        ));
    }
    // The slot user's SID is written into an access list later
    // (`runner::protect_before_resume`), so it is checked here as a SID.
    let user_sid = String::from_utf16_lossy(&request.user_sid);
    if !is_sid_string(&user_sid) {
        return Err(Failure::other(
            "refuse a request with a malformed SID",
            "the request's sandbox user SID is not a SID",
        ));
    }
    Ok(())
}

/// The user SID of the client on the other end of `pipe`, as a string.
fn client_sid(pipe: &OwnedHandle) -> Result<String, Failure> {
    // SAFETY: a valid pipe instance with a client connected and its request
    // read.
    if unsafe { ImpersonateNamedPipeClient(pipe.as_raw_handle()) } == 0 {
        // The thread is not impersonating, so there is no client identity to
        // act on: refuse.
        return Err(Failure::last(
            "identify the client (ImpersonateNamedPipeClient)",
        ));
    }
    let found = impersonated_sid();
    // SAFETY: ends the impersonation this thread started, on every path.
    let reverted = unsafe { RevertToSelf() };
    if reverted == 0 {
        // A thread that is still the client must do nothing more.
        return Err(Failure::last(
            "stop impersonating the client (RevertToSelf)",
        ));
    }
    found
}

/// The user SID of the token the calling thread is impersonating. Called
/// between the impersonation and `RevertToSelf`.
fn impersonated_sid() -> Result<String, Failure> {
    let mut raw: HANDLE = ptr::null_mut();
    // SAFETY: opens the impersonation token of this thread. Open as self: the
    // client's own rights must not decide whether its token can be read.
    if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut raw) } == 0 {
        return Err(Failure::last("read the client's token (OpenThreadToken)"));
    }
    // SAFETY: a fresh handle this code now owns.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    token_user_sid(&token)
}

/// The user SID of `token`, as a string. Shared by the client check and the
/// check that the process the handles are duplicated out of is still the
/// owner's.
fn token_user_sid(token: &OwnedHandle) -> Result<String, Failure> {
    let mut needed = 0u32;
    // SAFETY: a size query with a null buffer.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed == 0 {
        return Err(Failure::last("read a token's user (GetTokenInformation)"));
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `needed` bytes.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(Failure::last("read a token's user (GetTokenInformation)"));
    }
    let user = buf.as_ptr() as *const TOKEN_USER;
    // SAFETY: the system wrote a TOKEN_USER whose SID points into `buf`.
    let sid: PSID = unsafe { (*user).User.Sid };
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is valid; the call allocates `text`, freed below.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(Failure::last(
            "write a token's user SID (ConvertSidToStringSidW)",
        ));
    }
    // SAFETY: a NUL-terminated string the call allocated.
    let out = unsafe { from_wide(text) };
    // SAFETY: frees the string the conversion allocated.
    unsafe { LocalFree(text as HLOCAL) };
    Ok(out)
}

// ---- the owner's handles ----------------------------------------------------

/// The owner's standard handles and job, duplicated into this process, with
/// the owner's own process handle they came out of: the runner's process
/// handle is duplicated back into the owner through it ([`start`]).
struct Handles {
    owner: OwnedHandle,
    stdin: OwnedHandle,
    stdout: OwnedHandle,
    stderr: OwnedHandle,
    job: OwnedHandle,
}

/// Duplicate the four handles the request names out of the owner's process.
///
/// Each one's object type is read with `NtQueryObject` and refused unless it
/// is one the launcher accepts (`service_args::ALLOWED_HANDLE_TYPES`): the
/// service puts nothing in the sandbox but the owner's own standard handles
/// and job. Every duplicate is closed on every path out, by
/// [`OwnedHandle`]'s own drop.
///
/// The three standard handles are made inheritable, because the runner is
/// started with `STARTF_USESTDHANDLES` and a list of the handles it may
/// inherit. The job is not made inheritable: an inheritable job handle would
/// reach the runner, which runs as the sandbox user, with the rights on the
/// job that the side which made it holds.
///
/// **The process this opens is checked, not just its ID.** The client's
/// process ID was compared with the request's before this, but Windows reuses
/// a process ID, and the process could have exited between the two steps. So
/// the process is opened for `PROCESS_DUP_HANDLE` and
/// `PROCESS_QUERY_LIMITED_INFORMATION` together, its own token is read, and
/// nothing is duplicated out of it unless its user SID is the SID the service
/// was installed to answer. A reused ID then names a process of some other
/// user and the request is refused.
///
/// **The job is checked for its limits.** A job object passes the type check
/// by being a job, which says nothing about whether it ends its processes.
/// `job_limits_allowed` refuses a job that does not end them when it is
/// closed, or that lets one break away, because the job is what stops a
/// sandboxed command from outliving the side that started it.
fn duplicate(request: &ServiceRequest, args: &ServiceArgs) -> Result<Handles, Failure> {
    // SAFETY: opens the process the request names, for handle duplication and
    // for reading its token alone.
    let process = unsafe {
        OpenProcess(
            PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            request.owner_pid,
        )
    };
    if process.is_null() {
        return Err(Failure::last("open the owner's process (OpenProcess)"));
    }
    // SAFETY: a fresh handle this code now owns.
    let owner = unsafe { OwnedHandle::from_raw_handle(process) };
    let found = process_user_sid(&owner)?;
    if found != args.owner_sid {
        return Err(Failure::other(
            "refuse a request whose process is not the owner's",
            format!(
                "process {} runs as {found}, and the service answers another user",
                request.owner_pid
            ),
        ));
    }
    let mut out = Vec::with_capacity(4);
    for (what, value, inherit) in [
        ("standard input", request.stdin, true),
        ("standard output", request.stdout, true),
        ("standard error", request.stderr, true),
        ("job", request.job, false),
    ] {
        let copy = duplicate_one(&owner, value, inherit, what)?;
        if what == "job" {
            check_job_limits(&copy)?;
        }
        out.push(copy);
    }
    let mut it = out.into_iter();
    let mut next = |what: &'static str| {
        it.next()
            .ok_or_else(|| Failure::other("duplicate the owner's handles", what))
    };
    Ok(Handles {
        owner,
        stdin: next("standard input")?,
        stdout: next("standard output")?,
        stderr: next("standard error")?,
        job: next("job")?,
    })
}

fn duplicate_one(
    owner: &OwnedHandle,
    value: u64,
    inherit: bool,
    what: &'static str,
) -> Result<OwnedHandle, Failure> {
    let mut copy: HANDLE = ptr::null_mut();
    // SAFETY: a valid process handle with PROCESS_DUP_HANDLE; `value` is a
    // handle value in that process, and a value that names no handle there
    // makes the call fail. The copy is owned below.
    let ok = unsafe {
        DuplicateHandle(
            owner.as_raw_handle(),
            value as usize as HANDLE,
            GetCurrentProcess(),
            &mut copy,
            0,
            i32::from(inherit),
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        let mut failure = Failure::last("duplicate the owner's standard handles");
        failure.text = format!("{}: {what}", failure.text);
        return Err(failure);
    }
    // SAFETY: a fresh handle this code now owns; closed on every path out.
    let copy = unsafe { OwnedHandle::from_raw_handle(copy) };
    let kind = object_type(copy.as_raw_handle())?;
    if !handle_type_allowed(&kind) {
        return Err(Failure::other(
            "refuse a handle that is not stdio or a job",
            format!("the {what} handle names a {kind} object"),
        ));
    }
    Ok(copy)
}

/// The user SID of the process `process` runs as.
///
/// The process is opened with `PROCESS_QUERY_LIMITED_INFORMATION`, which is
/// enough for `OpenProcessToken` with `TOKEN_QUERY`; no right over the process
/// itself is needed or taken.
fn process_user_sid(process: &OwnedHandle) -> Result<String, Failure> {
    let mut raw: HANDLE = ptr::null_mut();
    // SAFETY: a valid process handle opened for limited query; the token is
    // owned below.
    if unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(Failure::last(
            "read the user of the requesting process (OpenProcessToken)",
        ));
    }
    // SAFETY: a fresh handle this code now owns.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    token_user_sid(&token)
}

/// Refuse a job whose limits would let a sandboxed command outlive the side
/// that asked for it.
///
/// The rule itself is platform-neutral
/// ([`job_limits_allowed`](super::super::service_args::job_limits_allowed));
/// this reads the flags the rule judges.
fn check_job_limits(job: &OwnedHandle) -> Result<(), Failure> {
    // SAFETY: zeroed is a valid initial JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    let size = u32::try_from(std::mem::size_of_val(&info)).unwrap_or(u32::MAX);
    let mut returned = 0u32;
    // SAFETY: a valid job handle and a buffer of `size` bytes of the structure
    // this information class writes.
    let ok = unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            ptr::addr_of_mut!(info).cast(),
            size,
            &mut returned,
        )
    };
    if ok == 0 {
        return Err(Failure::last(
            "read the job's limits (QueryInformationJobObject)",
        ));
    }
    if let Err(why) = job_limits_allowed(info.BasicLimitInformation.LimitFlags) {
        return Err(Failure::other("refuse a job that holds nothing", why));
    }
    Ok(())
}

/// What the object manager calls the type of `handle`: `File` for a pipe or a
/// file, `Job` for a job object, and so on. `NtQueryObject` is used because
/// it names the type; `GetFileType` would say only whether a handle is a
/// file-like object and nothing about a job.
fn object_type(handle: HANDLE) -> Result<String, Failure> {
    /// `PUBLIC_OBJECT_TYPE_INFORMATION` from `winternl.h`: the type's name,
    /// then room the system reserves. windows-sys does not bind it.
    #[repr(C)]
    struct TypeInformation {
        type_name: UNICODE_STRING,
        reserved: [u32; 22],
    }
    /// `STATUS_INFO_LENGTH_MISMATCH`: the buffer is too small, and the call
    /// says how much it needs.
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;

    let mut buf = vec![0u64; 128];
    let mut needed = 0u32;
    for attempt in 0..2 {
        let bytes = u32::try_from(buf.len() * 8).unwrap_or(u32::MAX);
        // SAFETY: a valid handle and a buffer of `bytes` bytes, aligned for
        // the structure the call writes.
        let status = unsafe {
            NtQueryObject(
                handle,
                ObjectTypeInformation,
                buf.as_mut_ptr().cast(),
                bytes,
                &mut needed,
            )
        };
        if status == 0 {
            let info = buf.as_ptr() as *const TypeInformation;
            // SAFETY: the call wrote a PUBLIC_OBJECT_TYPE_INFORMATION whose
            // name buffer lies inside `buf`.
            let name = unsafe { (*info).type_name };
            if name.Buffer.is_null() {
                return Err(Failure::other(
                    "read a handle's object type (NtQueryObject)",
                    "the object type has no name",
                ));
            }
            // SAFETY: `Length` is the name's size in bytes, inside `buf`.
            let units =
                unsafe { std::slice::from_raw_parts(name.Buffer, usize::from(name.Length) / 2) };
            return Ok(String::from_utf16_lossy(units));
        }
        if status == STATUS_INFO_LENGTH_MISMATCH && attempt == 0 && needed > 0 {
            buf = vec![0u64; (needed as usize).div_ceil(8) + 1];
            continue;
        }
        return Err(Failure::with(
            "read a handle's object type (NtQueryObject)",
            status as u32,
            format!("NtQueryObject returned status 0x{:08X}", status as u32),
        ));
    }
    Err(Failure::other(
        "read a handle's object type (NtQueryObject)",
        "the object type did not fit the buffer twice",
    ))
}

// ---- the logon --------------------------------------------------------------

/// `units` as a NUL-terminated UTF-16 string in one buffer of its final size,
/// cleared when it is dropped. For a password: a buffer that grows as it is
/// filled leaves copies of the text that nothing clears.
fn secret_wide(units: &[u16]) -> Zeroizing<Vec<u16>> {
    let mut out = Zeroizing::new(Vec::with_capacity(units.len() + 1));
    out.extend_from_slice(units);
    out.push(0);
    out
}

/// Log the slot user on, as that user alone.
///
/// `LOGON32_LOGON_INTERACTIVE` is used on purpose: setup denies each sandbox
/// user the batch, service, network and remote-interactive logon rights, and
/// that is not loosened for the launcher. So `LogonUserW` fails with
/// `ERROR_LOGON_TYPE_NOT_GRANTED` (1385) where a policy denies the user the
/// local logon, and the reply says so.
///
/// The password is in the request because the slot passwords are sealed for
/// the owner's account, which the service cannot open. It is copied into one
/// buffer that is cleared as soon as the call returns, and the request clears
/// its own copy when it is dropped.
fn logon(request: &ServiceRequest) -> Result<OwnedHandle, Failure> {
    let user = secret_wide(&request.user_name);
    let domain = wide(".");
    let password = secret_wide(&request.password);
    let mut raw: HANDLE = ptr::null_mut();
    // SAFETY: three NUL-terminated strings that live across the call; the
    // token is owned below.
    let ok = unsafe {
        LogonUserW(
            user.as_ptr(),
            domain.as_ptr(),
            password.as_ptr(),
            LOGON32_LOGON_INTERACTIVE,
            LOGON32_PROVIDER_DEFAULT,
            &mut raw,
        )
    };
    let failure = Failure::last("log the sandbox user on");
    drop(password);
    if ok == 0 {
        return Err(match failure.code {
            super::super::runner::ERROR_LOGON_TYPE_NOT_GRANTED => Failure::with(
                failure.step,
                failure.code,
                format!(
                    "{}: the sandbox user may not log on locally, and the launcher logs it \
                     on interactively because the batch, service, network and \
                     remote-interactive logon rights are denied on purpose",
                    failure.text
                ),
            ),
            _ => failure,
        });
    }
    // SAFETY: a fresh handle this code now owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// The environment block of the logged-on user, freed on drop.
///
/// Never a null environment for the created process: the runner, and through
/// it the command, would inherit the service's own LocalSystem environment.
/// `LoadUserProfile` is not called: it needs the backup and restore
/// privileges and leaves a loaded registry hive behind. What it would give is
/// the user's `HKEY_CURRENT_USER`, which a build reads for some toolchain and
/// shell settings; without it, those reads find nothing.
struct Environment(*mut c_void);

impl Environment {
    fn of(token: &OwnedHandle) -> Result<Self, Failure> {
        let mut block: *mut c_void = ptr::null_mut();
        // SAFETY: a valid primary token; the call allocates the block, freed
        // on drop. `false`: the service's own environment is not merged in.
        if unsafe { CreateEnvironmentBlock(&mut block, token.as_raw_handle(), 0) } == 0 {
            return Err(Failure::last(
                "build the sandbox user's environment (CreateEnvironmentBlock)",
            ));
        }
        Ok(Self(block))
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        // SAFETY: frees the block the call allocated, once.
        unsafe { DestroyEnvironmentBlock(self.0) };
    }
}

// ---- the start --------------------------------------------------------------

/// Which call put the runner in the job the request named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JoinedJob {
    /// `PROC_THREAD_ATTRIBUTE_JOB_LIST`, in the creation call itself. The
    /// process is in the job before its first instruction runs, so there is
    /// no moment in which it is a live process in no job.
    AtCreation,
    /// `AssignProcessToJobObject`, after the creation. Used only where the
    /// attribute is not available.
    AfterCreation,
}

/// A failure while the creation attributes are built. The job-list attribute
/// is told apart from the rest, because the launcher has a second way to join
/// the job and no second way to bound inheritance.
enum AttributeFailure {
    JobList(Failure),
    Other(Failure),
}

/// Owns a `PROC_THREAD_ATTRIBUTE_LIST` for the launcher's creation call and
/// the handle arrays it points at, which must live until the call returns.
///
/// Two attributes in one list:
///
/// * `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, the three standard handles of this
///   request and nothing else. The service serves requests on many threads
///   with nothing between them, and `bInheritHandles` on its own gives a new
///   process every inheritable handle in the service, so a command could
///   inherit another command's standard handles, and a redirected standard
///   output is a file handle with the rights of the side that opened it. The
///   handle list bounds inheritance to the listed handles whatever any other
///   thread holds open at that moment.
/// * `PROC_THREAD_ATTRIBUTE_JOB_LIST`, the job the request named, so the
///   process joins the job at creation.
///
/// A list is initialized for a fixed number of attributes, so the shared
/// one-attribute helper (`crate::sandbox::AttributeList`) cannot carry the
/// job list as well; this builds the two-attribute list the launcher needs
/// and nothing else.
struct StartAttributes {
    /// The attribute list itself. Empty where initialization failed, which
    /// [`Drop`] reads: an uninitialized list must not be deleted.
    buf: Vec<u64>,
    /// The handles the new process may inherit. The attribute points here.
    handles: Box<[HANDLE]>,
    /// The job it joins at creation, or empty where the attribute is not
    /// available. The attribute points here.
    jobs: Box<[HANDLE]>,
    joined: JoinedJob,
}

impl StartAttributes {
    /// The list for one creation call: `handles` to inherit, and `job` to join
    /// at creation where Windows supports it.
    ///
    /// Where the job-list attribute is refused, which is what a Windows older
    /// than the attribute does, the list is built again with the handle list
    /// alone and [`JoinedJob::AfterCreation`] is reported, so the caller knows
    /// it must assign the job itself. A failure to bound inheritance is never
    /// worked around: it is returned.
    fn new(handles: &[HANDLE], job: HANDLE) -> Result<Self, Failure> {
        match Self::build(handles, Some(job)) {
            Ok(list) => Ok(list),
            Err(AttributeFailure::Other(failure)) => Err(failure),
            Err(AttributeFailure::JobList(_)) => Self::build(handles, None).map_err(|e| match e {
                AttributeFailure::JobList(failure) | AttributeFailure::Other(failure) => failure,
            }),
        }
    }

    fn build(handles: &[HANDLE], job: Option<HANDLE>) -> Result<Self, AttributeFailure> {
        let count = if job.is_some() { 2 } else { 1 };
        let mut size = 0usize;
        // SAFETY: a size query, which a null list is for.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &mut size) };
        if size == 0 {
            return Err(AttributeFailure::Other(Failure::last(
                "size the runner's creation attributes (InitializeProcThreadAttributeList)",
            )));
        }
        let mut list = Self {
            buf: vec![0u64; size.div_ceil(8)],
            handles: handles.to_vec().into_boxed_slice(),
            jobs: job.into_iter().collect::<Vec<HANDLE>>().into_boxed_slice(),
            joined: if job.is_some() {
                JoinedJob::AtCreation
            } else {
                JoinedJob::AfterCreation
            },
        };
        // SAFETY: `buf` holds at least `size` bytes.
        if unsafe { InitializeProcThreadAttributeList(list.as_ptr(), count, 0, &mut size) } == 0 {
            let failure = Failure::last(
                "build the runner's creation attributes (InitializeProcThreadAttributeList)",
            );
            // Nothing was initialized, so Drop must not delete it.
            list.buf.clear();
            return Err(AttributeFailure::Other(failure));
        }
        // SAFETY: an initialized list, and a boxed slice whose address is
        // stable for the life of the list, with its size in bytes.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                list.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                list.handles.as_ptr().cast(),
                std::mem::size_of_val(&*list.handles),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if ok == 0 {
            return Err(AttributeFailure::Other(Failure::last(
                "bound the runner's inherited handles (UpdateProcThreadAttribute)",
            )));
        }
        if !list.jobs.is_empty() {
            // SAFETY: as above, for the job array.
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    list.as_ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                    list.jobs.as_ptr().cast(),
                    std::mem::size_of_val(&*list.jobs),
                    ptr::null_mut(),
                    ptr::null(),
                )
            };
            if ok == 0 {
                return Err(AttributeFailure::JobList(Failure::last(
                    "join the job at creation (UpdateProcThreadAttribute)",
                )));
            }
        }
        Ok(list)
    }

    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buf.as_mut_ptr().cast()
    }
}

impl Drop for StartAttributes {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            // SAFETY: an initialized list, deleted once.
            unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
        }
    }
}

/// Start the runner as the slot user, in session 0, and return its process ID
/// with the result of writing its protected access lists.
///
/// **The session is not set.** `CreateProcessAsUser` runs the process in the
/// session of the token, and a token from `LogonUser` carries the session of
/// the process that called it, which is this service's session 0. So no
/// `SetTokenInformation` call is made, and none is needed; moving a token to
/// another session needs `SeTcbPrivilege` in any case.
///
/// **The window station.** `lpDesktop` is an empty string, not null. For a
/// noninteractive logon session Windows then forms a window station name from
/// the logon session's ID and creates that station and its default desktop
/// (Microsoft, "Process Connection to a Window Station"). Each command
/// therefore gets a window station of its own, and no access list of the
/// owner's is touched. A null `lpDesktop` would give the runner this
/// service's own station and desktop, which is wrong.
///
/// **The job is joined in the creation call.** The job the request named is in
/// the creation attributes (`PROC_THREAD_ATTRIBUTE_JOB_LIST`), so the process
/// is in the job before its first instruction runs. Assigning the job after
/// the creation leaves a window: if this service ended inside it, a suspended
/// process of the sandbox user would be left in no job, never resumed, never
/// ended, and still holding the standard handles it was given.
/// `AssignProcessToJobObject` runs only where the attribute is not available,
/// and either way `IsProcessInJob` confirms the membership before the process
/// is resumed. A process that is in no job is ended: a command outside the job
/// would answer no kill, no timeout and no limit, and could outlive the side
/// that asked for it.
///
/// **Then the owner's handle, then the lists, then the run.** The process is
/// duplicated back into the requesting process to wait on, given its protected
/// access lists ([`super::runner::protect_before_resume`]) and only then
/// resumed. A failed duplication ends it: the caller would hold a running
/// command it cannot wait for.
///
/// **Inheritance is bounded by the handle list.** `bInheritHandles` is true,
/// which alone would pass every inheritable handle in this service, including
/// another request's standard handles. The creation attributes name the three
/// handles of this request ([`StartAttributes`]), and `EXTENDED_STARTUPINFO_PRESENT`
/// is set so Windows reads them.
///
/// The owner's handle is made before the access lists are written, and with
/// `SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION` alone, not the rights
/// this service holds. The owner cannot open the process itself once those
/// lists are written, because they admit the sandbox user and SYSTEM alone; a
/// handle keeps the rights it was made with, so this one goes on working for
/// the wait and the exit status. Where a later step fails, the process is
/// ended and the reply says so, and the owner's handle then names a process
/// that has exited: the owner reads the failure, not the handle.
fn start(
    args: &ServiceArgs,
    request: &ServiceRequest,
    token: &OwnedHandle,
    handles: &Handles,
    environment: &Environment,
) -> Result<(u32, u64, Result<(), String>), Failure> {
    let app = wide(&args.runner);
    let mut line: Vec<u16> = request.command_line.iter().copied().chain([0]).collect();
    let mut desktop: Vec<u16> = vec![0];
    let stdio = [
        handles.stdin.as_raw_handle(),
        handles.stdout.as_raw_handle(),
        handles.stderr.as_raw_handle(),
    ];
    let mut attributes = StartAttributes::new(&stdio, handles.job.as_raw_handle())?;
    let joined = attributes.joined;
    // SAFETY: zeroed is a valid initial STARTUPINFOEXW.
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.lpDesktop = desktop.as_mut_ptr();
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = stdio[0];
    si.StartupInfo.hStdOutput = stdio[1];
    si.StartupInfo.hStdError = stdio[2];
    si.lpAttributeList = attributes.as_ptr();
    // SAFETY: zeroed is a valid initial PROCESS_INFORMATION.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // A non-null application name: the runner path an administrator installed
    // the service with, never a program the command line's first word names.
    // No working directory, so the runner starts in this service's own, the
    // system folder; the command's directory comes from the runner request.
    // SAFETY: every pointer is NUL-terminated or a valid struct that lives
    // across the call; `line` is mutable as the API requires; the environment
    // block belongs to `environment`.
    let ok = unsafe {
        CreateProcessAsUserW(
            token.as_raw_handle(),
            app.as_ptr(),
            line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            CREATE_SUSPENDED
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT,
            environment.0,
            ptr::null(),
            ptr::addr_of!(si.StartupInfo),
            &mut pi,
        )
    };
    if ok == 0 {
        let mut failure = Failure::last("start the runner in session 0 (CreateProcessAsUserW)");
        if joined == JoinedJob::AtCreation {
            // The job is part of the creation call, so a job that refuses the
            // new process fails here and not in a later step.
            failure.text = format!("{}: the job is joined in this call", failure.text);
        }
        return Err(failure);
    }
    // The attributes point at arrays that must live until the call returns,
    // which it has.
    drop(attributes);
    // SAFETY: fresh handles this code now owns.
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess) };
    // SAFETY: as above.
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread) };
    let kill = |failure: Failure| {
        // SAFETY: the suspended runner never ran; end it.
        unsafe { TerminateProcess(process.as_raw_handle(), 1) };
        failure
    };
    if joined == JoinedJob::AfterCreation {
        // SAFETY: valid job and process handles.
        if unsafe { AssignProcessToJobObject(handles.job.as_raw_handle(), process.as_raw_handle()) }
            == 0
        {
            return Err(kill(Failure::last(
                "put the runner in the owner's job (AssignProcessToJobObject)",
            )));
        }
    }
    // Whichever call acted, the membership is read back: the runner is never
    // resumed outside the job.
    let mut inside = 0;
    // SAFETY: valid process and job handles, and an initialized result.
    if unsafe {
        IsProcessInJob(
            process.as_raw_handle(),
            handles.job.as_raw_handle(),
            &mut inside,
        )
    } == 0
    {
        return Err(kill(Failure::last(
            "check the runner's job (IsProcessInJob)",
        )));
    }
    if inside == 0 {
        return Err(kill(Failure::other(
            "refuse a runner that is in no job",
            "the runner is not in the job the request named",
        )));
    }
    // The owner's own handle to the command, for the wait and the exit
    // status. Only this service's copy is closed; the owner's is the owner's
    // to close.
    let mut for_owner: HANDLE = ptr::null_mut();
    // SAFETY: a valid source process (this one), a valid handle of it, and
    // the owner's process opened for PROCESS_DUP_HANDLE.
    let duplicated = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            process.as_raw_handle(),
            handles.owner.as_raw_handle(),
            &mut for_owner,
            SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            0,
        )
    };
    if duplicated == 0 || for_owner.is_null() {
        return Err(kill(Failure::last(
            "give the owner a handle to the command (DuplicateHandle)",
        )));
    }
    let user_sid = String::from_utf16_lossy(&request.user_sid);
    let protected = super::runner::protect_before_resume(
        process.as_raw_handle(),
        thread.as_raw_handle(),
        &user_sid,
    );
    // SAFETY: the primary thread of the suspended runner.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        return Err(kill(Failure::last("resume the runner (ResumeThread)")));
    }
    Ok((pi.dwProcessId, for_owner as usize as u64, protected))
}

// The right the owner's entry must never carry is the one Windows names
// `FILE_APPEND_DATA` on a file and `FILE_CREATE_PIPE_INSTANCE` on a pipe, so
// the platform-neutral value and this build's cannot drift apart.
const _: () = assert!(
    super::super::service_args::FILE_CREATE_PIPE_INSTANCE
        == windows_sys::Win32::Storage::FileSystem::FILE_APPEND_DATA
);
const _: () = assert!(PIPE_OWNER_MASK & super::super::service_args::FILE_CREATE_PIPE_INSTANCE == 0);

// The job limits the platform-neutral rule judges are this build's own values,
// so the rule and the flags Windows reports cannot drift apart.
const _: () = assert!(
    super::super::service_args::REQUIRED_JOB_LIMITS
        == windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
);
const _: () = assert!(
    super::super::service_args::FORBIDDEN_JOB_LIMITS
        == windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_BREAKAWAY_OK
            | windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK
);

#[cfg(test)]
mod tests {
    use super::*;

    /// The service's own pipe name and access list come from its arguments,
    /// and the list carries no right to add an instance of the name.
    ///
    /// Windows asks for `FILE_CREATE_PIPE_INSTANCE` on the pipe that exists
    /// before it makes another instance of it (`CreateNamedPipeW`). The
    /// service runs as SYSTEM, which the list grants every right, so it makes
    /// as many instances as it needs. This test runs as the owner, whom the
    /// list grants [`PIPE_OWNER_MASK`] alone, so here another instance must be
    /// refused: that refusal is the property, because a program running as
    /// the owner must not add an instance of the name and answer in the
    /// service's place. A list that grants the owner every right then shows
    /// that the refusal comes from the list and not from the flags.
    /// Compile-checked on the development host; it runs on Windows.
    #[test]
    fn the_pipes_access_list_is_accepted_by_windows() {
        const ACCESS_DENIED: u32 = 5;
        let owner_sid = super::super::accounts::current_user_sid().expect("own SID");
        let as_system = owner_sid == "S-1-5-18";
        let args = ServiceArgs {
            owner_sid: owner_sid.clone(),
            pipe: format!("kgraf-launcher-test-{}", std::process::id()),
            runner: r"C:\Windows\System32\cmd.exe".to_owned(),
        };
        let sddl = Sddl::new(&args.pipe_sddl()).expect("the access list");
        let sa = sddl.attributes();
        let path = wide(&args.pipe_path());
        let first = create_instance(&path, &sa, true).expect("the first instance");
        // A second first instance of the same name is refused, so no other
        // program can take the name from the service. Windows documents
        // ERROR_ACCESS_DENIED for it.
        let again = create_instance(&path, &sa, true).expect_err("a second first instance");
        assert_eq!(again.code, ACCESS_DENIED, "{again:?}");
        let another = create_instance(&path, &sa, false);
        if as_system {
            drop(another.expect("SYSTEM makes another instance"));
        } else {
            let refused = another.expect_err("the owner made another instance of the pipe");
            assert_eq!(refused.code, ACCESS_DENIED, "{refused:?}");
        }
        drop(first);

        // The same calls under a list that grants the owner every right: the
        // second instance is made, so the refusal above is the list's.
        let open = Sddl::new(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{owner_sid})"))
            .expect("a list that grants the owner every right");
        let open_sa = open.attributes();
        let open_path = wide(&format!("{}-open", args.pipe_path()));
        let first = create_instance(&open_path, &open_sa, true).expect("the first instance");
        let second =
            create_instance(&open_path, &open_sa, false).expect("another instance, right held");
        drop(second);
        drop(first);
    }

    /// A job is judged by its limits and not by its type: a job object of the
    /// right type that ends nothing when it closes, or that lets a process
    /// break away, is refused.
    /// Compile-checked on the development host; it runs on Windows.
    #[test]
    fn a_job_is_accepted_on_its_limits_and_not_on_its_type() {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, SetInformationJobObject,
        };

        // SAFETY: an unnamed job object with the default access list.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        assert!(!raw.is_null(), "a fresh job object");
        // SAFETY: a fresh handle this test now owns.
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        assert_eq!(object_type(job.as_raw_handle()).unwrap(), "Job");
        assert!(
            handle_type_allowed("Job"),
            "the type check alone accepts this job"
        );
        // The type check accepts it; the limits do not, because a job with no
        // limits ends nothing when the last handle to it is closed.
        assert!(check_job_limits(&job).is_err(), "a job with no limits");

        let set = |flags: u32| {
            // SAFETY: zeroed is a valid initial
            // JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = flags;
            let size = std::mem::size_of_val(&info) as u32;
            // SAFETY: a valid job handle and a filled structure of `size`
            // bytes of the class named.
            unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    ptr::addr_of!(info).cast(),
                    size,
                )
            }
        };
        assert_ne!(
            set(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE),
            0,
            "set the job's limits"
        );
        check_job_limits(&job).expect("a job that ends its processes when it is closed");
        assert_ne!(
            set(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK),
            0,
            "set the job's limits"
        );
        assert!(
            check_job_limits(&job).is_err(),
            "a job that lets a process break away"
        );
    }

    /// The creation attributes carry the handle list and the job list in one
    /// list, so inheritance is bounded and the job is joined in the creation
    /// call.
    /// Compile-checked on the development host; it runs on Windows.
    #[test]
    fn the_creation_attributes_bound_inheritance_and_join_the_job() {
        use windows_sys::Win32::System::JobObjects::CreateJobObjectW;

        // SAFETY: an unnamed job object with the default access list.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        assert!(!raw.is_null(), "a fresh job object");
        // SAFETY: a fresh handle this test now owns.
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        let args = ServiceArgs {
            owner_sid: super::super::accounts::current_user_sid().expect("own SID"),
            pipe: format!("kgraf-launcher-attrs-{}", std::process::id()),
            runner: r"C:\Windows\System32\cmd.exe".to_owned(),
        };
        let sddl = Sddl::new(&args.pipe_sddl()).expect("the access list");
        // Three different handles, as a request's three standard handles are:
        // the first instance of three pipes. Three instances of one pipe are
        // not a choice here, because the list refuses the owner, whom this
        // test runs as, the right to make another instance
        // (`the_pipes_access_list_is_accepted_by_windows`).
        let instances: Vec<OwnedHandle> = (0..3)
            .map(|i| {
                let path = wide(&format!("{}-{i}", args.pipe_path()));
                create_instance(&path, &sddl.attributes(), true).expect("an instance")
            })
            .collect();
        let stdio = [
            instances[0].as_raw_handle(),
            instances[1].as_raw_handle(),
            instances[2].as_raw_handle(),
        ];
        let attributes =
            StartAttributes::new(&stdio, job.as_raw_handle()).expect("the creation attributes");
        assert_eq!(
            attributes.joined,
            JoinedJob::AtCreation,
            "the job is joined in the creation call"
        );
        assert_eq!(attributes.handles.len(), 3, "the three listed handles");
        assert_eq!(attributes.jobs.len(), 1, "the one listed job");
    }

    /// A handle to a pipe is a `File` object and is accepted; a handle to
    /// this process is a `Process` object and is refused.
    /// Compile-checked on the development host; it runs on Windows.
    #[test]
    fn a_handle_type_is_read_and_a_process_handle_is_refused() {
        let args = ServiceArgs {
            owner_sid: super::super::accounts::current_user_sid().expect("own SID"),
            pipe: format!("kgraf-launcher-type-{}", std::process::id()),
            runner: r"C:\Windows\System32\cmd.exe".to_owned(),
        };
        let sddl = Sddl::new(&args.pipe_sddl()).expect("the access list");
        let sa = sddl.attributes();
        let path = wide(&args.pipe_path());
        let instance = create_instance(&path, &sa, true).expect("an instance");
        assert_eq!(object_type(instance.as_raw_handle()).unwrap(), "File");
        // SAFETY: a pseudo-handle that needs no close.
        let me = unsafe { GetCurrentProcess() };
        let kind = object_type(me).expect("the type of a process handle");
        assert_eq!(kind, "Process");
        assert!(!handle_type_allowed(&kind));
    }
}
