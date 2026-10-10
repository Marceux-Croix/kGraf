//! kGraf's launcher service in the service control manager: what is
//! installed, install and repair, start, and remove.
//!
//! The service exists so a sandboxed command runs in session 0, where an
//! unprefixed named kernel object resolves in the machine-wide
//! `\BaseNamedObjects` (`super::super::session0`). It is installed once, by
//! the elevated `kgraf sandbox setup`, and starts nothing but kGraf's own
//! runner.
//!
//! Who needs which rights: installing, repairing and removing need
//! administrator rights, because the service runs as LocalSystem. Reading
//! what is installed needs none, so `kgraf sandbox status` tells the truth
//! for an unelevated user. Starting the service needs the start right the
//! install gives the owner's account alone
//! (`super::super::service_dacl::OWNER_RIGHTS`).
//!
//! Nothing here decides policy: the names, the access list and the image
//! path come from `super::super::names` and
//! `super::super::service_dacl`, which every host tests.

use std::path::Path;
use std::ptr;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_SERVICE_ALREADY_RUNNING,
    ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS, ERROR_SERVICE_MARKED_FOR_DELETE,
    ERROR_SERVICE_NOT_ACTIVE, GetLastError, HLOCAL, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
use windows_sys::Win32::Storage::FileSystem::{DELETE, READ_CONTROL, WRITE_DAC};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceConfig2W,
    QueryServiceStatusEx, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
    SC_STATUS_PROCESS_INFO, SERVICE_ALL_ACCESS, SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONTROL_STOP, SERVICE_DEMAND_START, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL,
    SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START, SERVICE_STATUS,
    SERVICE_STATUS_PROCESS, SERVICE_STOP, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
    SetServiceObjectSecurity, StartServiceW,
};

use super::super::service_dacl;
use super::{from_wide, last_error, wide, win32};
use crate::error::SandboxError;

// The access rights `service_dacl` writes into the service's own access list
// are numbers in a platform-neutral file, so every host tests the mask. These
// checks fail the Windows build if any of them is not the right Windows
// names.
const _: () = assert!(service_dacl::SERVICE_START == SERVICE_START);
const _: () = assert!(service_dacl::SERVICE_STOP == SERVICE_STOP);
const _: () = assert!(service_dacl::SERVICE_QUERY_STATUS == SERVICE_QUERY_STATUS);
const _: () = assert!(service_dacl::SERVICE_QUERY_CONFIG == SERVICE_QUERY_CONFIG);
const _: () = assert!(service_dacl::SERVICE_CHANGE_CONFIG == SERVICE_CHANGE_CONFIG);
const _: () = assert!(service_dacl::READ_CONTROL == READ_CONTROL);
const _: () = assert!(service_dacl::WRITE_DAC == WRITE_DAC);
const _: () = assert!(service_dacl::DELETE == DELETE);

/// How long one poll of a service's state waits before it asks again. The
/// service control manager gives a wait hint, but a stop or a start of this
/// service takes a few milliseconds, so a short fixed pause ends the wait
/// sooner than a hint of several seconds would.
const POLL: Duration = Duration::from_millis(50);

/// What is installed under a service name, as `kgraf sandbox status`,
/// `selfcheck` and setup read it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceFacts {
    /// The service's description, which says whose service it is
    /// (`OwnerNames::owns_service_comment`). `None` when no service has the
    /// name, when it carries no description, or when this user may not read
    /// its configuration.
    pub description: Option<String>,
    /// Whether the service is running. `None` when no service has the name,
    /// or when this user may not read its state.
    pub running: Option<bool>,
}

/// A service control manager or service handle, closed on drop, on every
/// path out of a function.
struct ScHandle(SC_HANDLE);

impl ScHandle {
    fn raw(&self) -> SC_HANDLE {
        self.0
    }
}

impl Drop for ScHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a handle this value owns, from OpenSCManagerW,
            // OpenServiceW or CreateServiceW, closed once.
            unsafe { CloseServiceHandle(self.0) };
        }
    }
}

/// The thread's last error code.
fn code() -> u32 {
    // SAFETY: reads the calling thread's last error.
    unsafe { GetLastError() }
}

fn failed(step: &'static str, why: impl std::fmt::Display) -> SandboxError {
    SandboxError::PlatformSetup {
        step,
        source: std::io::Error::other(why.to_string()),
    }
}

/// A Win32 error from `step`, with what to do for the codes whose cause is
/// known.
fn service_error(step: &'static str, error: u32) -> SandboxError {
    match hint(error) {
        Some(hint) => failed(
            step,
            format!(
                "{} (Windows error {error}): {hint}",
                std::io::Error::from_raw_os_error(error as i32)
            ),
        ),
        None => win32(step, error),
    }
}

/// What to do about a service error whose cause is known.
fn hint(error: u32) -> Option<&'static str> {
    Some(match error {
        ERROR_ACCESS_DENIED => {
            "the owner's account is not admitted to kGraf's launcher service. Run `kgraf sandbox \
             setup` again: it admits the account that runs it"
        }
        ERROR_SERVICE_DOES_NOT_EXIST => {
            "kGraf's launcher service is not installed; run `kgraf sandbox setup`"
        }
        ERROR_SERVICE_MARKED_FOR_DELETE => {
            "the service is waiting to be removed and Windows takes the name back when its last \
             handle is closed; run `kgraf sandbox setup` again, or restart the machine"
        }
        _ => return None,
    })
}

// Each call below gives the Win32 error code back rather than a
// `SandboxError`, and the caller turns it into one. The code is read the
// moment the call fails: building an error allocates and formats, and
// anything on that path that calls into Windows can replace the thread's
// last error.

/// The service control manager, with `access`.
fn open_manager(access: u32) -> Result<ScHandle, u32> {
    // SAFETY: null names ask for this machine and the active database.
    let h = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), access) };
    if h.is_null() {
        return Err(code());
    }
    Ok(ScHandle(h))
}

/// Service `name` with `access`.
fn open_service(manager: &ScHandle, name: &str, access: u32) -> Result<ScHandle, u32> {
    let w = wide(name);
    // SAFETY: a live manager handle and a NUL-terminated name.
    let h = unsafe { OpenServiceW(manager.raw(), w.as_ptr(), access) };
    if h.is_null() {
        return Err(code());
    }
    Ok(ScHandle(h))
}

/// The step every open of the service names.
const OPEN: &str = "open kGraf's launcher service";

/// The service control manager, with `access`, as an error that names `step`.
fn manager(access: u32, step: &'static str) -> Result<ScHandle, SandboxError> {
    open_manager(access).map_err(|e| service_error(step, e))
}

/// Service `name` with `access`, as an error that names the open.
fn service(manager: &ScHandle, name: &str, access: u32) -> Result<ScHandle, SandboxError> {
    open_service(manager, name, access).map_err(|e| service_error(OPEN, e))
}

/// One read of a service's state.
fn read_status(service: &ScHandle) -> Result<SERVICE_STATUS_PROCESS, u32> {
    let mut s = SERVICE_STATUS_PROCESS::default();
    let size = std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32;
    let mut needed = 0u32;
    // SAFETY: a live service handle, and a buffer of exactly the size the
    // call is told, written only by the call.
    let ok = unsafe {
        QueryServiceStatusEx(
            service.raw(),
            SC_STATUS_PROCESS_INFO,
            (&raw mut s).cast(),
            size,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(code());
    }
    Ok(s)
}

/// One read of a service's state, as an error that names the read.
fn status(service: &ScHandle) -> Result<SERVICE_STATUS_PROCESS, SandboxError> {
    read_status(service).map_err(|e| service_error(STATE, e))
}

/// The step every read of the service's state names.
const STATE: &str = "read the launcher service's state";

// ---- read -------------------------------------------------------------------

/// What is installed under `name`, read with no administrator rights.
///
/// A service that does not exist is an empty [`ServiceFacts`], not an error:
/// not installed is a state `kgraf sandbox status` reports, and the first
/// setup on a machine starts from it. A right this user is refused leaves
/// that one field `None` for the same reason: status and selfcheck must work
/// for the unelevated owner, so a refused read is reported as unknown rather
/// than failing the command. The two reads are opened separately, so one
/// refused right does not hide the other.
pub fn read(name: &str) -> Result<ServiceFacts, SandboxError> {
    let manager = match open_manager(SC_MANAGER_CONNECT) {
        Ok(m) => m,
        // Every authenticated user may connect, so this is unexpected;
        // status still reports rather than fails.
        Err(ERROR_ACCESS_DENIED) => return Ok(ServiceFacts::default()),
        Err(e) => return Err(service_error("open the service control manager", e)),
    };
    let description = match open_service(&manager, name, SERVICE_QUERY_CONFIG) {
        Ok(h) => description(&h)?,
        Err(ERROR_SERVICE_DOES_NOT_EXIST | ERROR_ACCESS_DENIED) => None,
        Err(e) => return Err(service_error(OPEN, e)),
    };
    let running = match open_service(&manager, name, SERVICE_QUERY_STATUS) {
        Ok(h) => {
            let state = read_status(&h).map_err(|e| service_error(STATE, e))?;
            Some(state.dwCurrentState == SERVICE_RUNNING)
        }
        Err(ERROR_SERVICE_DOES_NOT_EXIST | ERROR_ACCESS_DENIED) => None,
        Err(e) => return Err(service_error(OPEN, e)),
    };
    Ok(ServiceFacts {
        description,
        running,
    })
}

/// The service's description, or `None` when it carries none.
fn description(service: &ScHandle) -> Result<Option<String>, SandboxError> {
    let mut needed = 0u32;
    // SAFETY: a live service handle; a null buffer of size zero asks for the
    // size, which the call writes to `needed`.
    let ok = unsafe {
        QueryServiceConfig2W(
            service.raw(),
            SERVICE_CONFIG_DESCRIPTION,
            ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if ok == 0 {
        match code() {
            ERROR_INSUFFICIENT_BUFFER => {}
            ERROR_ACCESS_DENIED => return Ok(None),
            other => return Err(win32("read the launcher service's description", other)),
        }
    }
    if needed == 0 {
        return Ok(None);
    }
    // The call writes a `SERVICE_DESCRIPTIONW` and the text it points at into
    // one buffer, so the buffer must be aligned for that structure: a vector
    // of u64 is aligned for a pointer on every Windows target.
    let words = (needed as usize).div_ceil(std::mem::size_of::<u64>());
    let mut buffer = vec![0u64; words.max(1)];
    let size = (words * std::mem::size_of::<u64>()) as u32;
    // SAFETY: the buffer holds at least `needed` bytes and is aligned for
    // SERVICE_DESCRIPTIONW.
    let ok = unsafe {
        QueryServiceConfig2W(
            service.raw(),
            SERVICE_CONFIG_DESCRIPTION,
            buffer.as_mut_ptr().cast(),
            size,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(service_error(
            "read the launcher service's description",
            code(),
        ));
    }
    // SAFETY: on success the call wrote a SERVICE_DESCRIPTIONW at the start
    // of the buffer, whose `lpDescription` is null or a NUL-terminated string
    // inside the same buffer.
    let text = unsafe {
        let d = &*buffer.as_ptr().cast::<SERVICE_DESCRIPTIONW>();
        if d.lpDescription.is_null() {
            return Ok(None);
        }
        from_wide(d.lpDescription)
    };
    Ok((!text.is_empty()).then_some(text))
}

// ---- install ----------------------------------------------------------------

/// Install the service, or bring an installed one to the wanted state
/// (administrator rights).
///
/// Demand start and LocalSystem: demand start so the service runs only while
/// kGraf needs it, LocalSystem because it must log a sandbox user on and give
/// the runner the owner's own handles. `exe` and `args` become the image
/// path, with the executable quoted
/// (`super::super::service_dacl::image_path`). The description is set so
/// [`read`] can tell kGraf's own service from another one with the same name.
///
/// Setup is idempotent, so a service that already exists is brought to the
/// wanted image path, start type, account, display name and description
/// rather than refused. The caller checks the description first: a service of
/// this name that another program installed is not kGraf's to change.
///
/// Last, the service's own access list is set
/// (`super::super::service_dacl::dacl_sddl`): SYSTEM and Administrators keep
/// full control and `owner_sid` gets
/// [`service_dacl::OWNER_RIGHTS`](super::super::service_dacl::OWNER_RIGHTS),
/// so the owner's
/// unelevated process can start and stop the service and read its state, and
/// cannot point it at another program, widen its own rights or remove it.
pub fn install(
    name: &str,
    display: &str,
    description: &str,
    exe: &Path,
    args: &[String],
    owner_sid: &str,
) -> Result<(), SandboxError> {
    let image = service_dacl::image_path(exe, args)
        .map_err(|e| failed("build the launcher service's image path", e))?;
    let sddl = service_dacl::dacl_sddl(owner_sid)
        .map_err(|e| failed("build the launcher service's access list", e))?;
    let manager = manager(
        SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE,
        "open the service control manager to install the launcher service",
    )?;
    let (w_name, w_display, w_image) = (wide(name), wide(display), wide(&image));
    // SAFETY: every string is NUL-terminated and lives across the call. A
    // null account name is the LocalSystem account and takes no password; a
    // null load order group, tag and dependency list mean the service waits
    // for nothing.
    let created = unsafe {
        CreateServiceW(
            manager.raw(),
            w_name.as_ptr(),
            w_display.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_DEMAND_START,
            SERVICE_ERROR_NORMAL,
            w_image.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
        )
    };
    let service = if created.is_null() {
        let error = code();
        if error != ERROR_SERVICE_EXISTS {
            return Err(service_error("install kGraf's launcher service", error));
        }
        let h = service(
            &manager,
            name,
            SERVICE_CHANGE_CONFIG | SERVICE_QUERY_CONFIG | READ_CONTROL | WRITE_DAC,
        )?;
        reconfigure(&h, &w_image, &w_display)?;
        h
    } else {
        ScHandle(created)
    };
    set_description(&service, description)?;
    set_access_list(&service, &sddl)
}

/// Bring an installed service's image path, start type, error control,
/// account and display name to what [`install`] asks for.
fn reconfigure(service: &ScHandle, image: &[u16], display: &[u16]) -> Result<(), SandboxError> {
    // For a built-in account the password is an empty string, which Windows
    // documents; a null password here would mean "leave it as it is".
    let account = wide("LocalSystem");
    let password = wide("");
    // SAFETY: every string is NUL-terminated and lives across the call. A
    // null load order group, tag and dependency list leave those as they are.
    let ok = unsafe {
        ChangeServiceConfigW(
            service.raw(),
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_DEMAND_START,
            SERVICE_ERROR_NORMAL,
            image.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null(),
            account.as_ptr(),
            password.as_ptr(),
            display.as_ptr(),
        )
    };
    if ok == 0 {
        return Err(service_error(
            "set the launcher service's program and start type",
            code(),
        ));
    }
    Ok(())
}

/// Set the description, which says whose service this is.
fn set_description(service: &ScHandle, description: &str) -> Result<(), SandboxError> {
    let mut text = wide(description);
    let info = SERVICE_DESCRIPTIONW {
        lpDescription: text.as_mut_ptr(),
    };
    // SAFETY: `info` points at `text`, which is NUL-terminated and lives
    // across the call.
    let ok = unsafe {
        ChangeServiceConfig2W(
            service.raw(),
            SERVICE_CONFIG_DESCRIPTION,
            (&raw const info).cast(),
        )
    };
    if ok == 0 {
        return Err(service_error("describe kGraf's launcher service", code()));
    }
    Ok(())
}

/// Replace the service's access list with `sddl`. Only the list is written,
/// so the service keeps the owner and group Windows gave it.
fn set_access_list(service: &ScHandle, sddl: &str) -> Result<(), SandboxError> {
    let w = wide(sddl);
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `w` is NUL-terminated; on success the call allocates `sd`,
    // freed below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error("build the launcher service's access list"));
    }
    // SAFETY: a live service handle and the descriptor built above.
    let ok = unsafe { SetServiceObjectSecurity(service.raw(), DACL_SECURITY_INFORMATION, sd) };
    let error = code();
    // SAFETY: frees the descriptor the conversion allocated.
    unsafe { LocalFree(sd as HLOCAL) };
    if ok == 0 {
        return Err(service_error(
            "set the launcher service's access list",
            error,
        ));
    }
    Ok(())
}

// ---- delete -----------------------------------------------------------------

/// Stop the service within `timeout`, then delete it (administrator rights).
///
/// A removal must finish, so a stop that does not end in time does not stop
/// the delete: the service is deleted anyway and the failed stop is reported.
/// Windows then marks the service for deletion and takes its name back when
/// the last handle to it closes, which for a service whose process is still
/// running means when that process ends or at the next restart. Nothing is
/// left for a person to clean up by hand.
///
/// A service that is not installed is nothing to do, not an error: a removal
/// that runs twice, or after a setup that stopped early, must finish.
pub fn delete(name: &str, timeout: Duration) -> Result<(), SandboxError> {
    let manager = manager(
        SC_MANAGER_CONNECT,
        "open the service control manager to remove the launcher service",
    )?;
    let access = SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE;
    let service = match open_service(&manager, name, access) {
        Ok(h) => h,
        // Nothing is installed under the name, so the removal is done.
        Err(ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(()),
        Err(e) => return Err(service_error(OPEN, e)),
    };
    let stopped = stop(&service, timeout);
    // SAFETY: a live service handle opened with DELETE.
    let ok = unsafe { DeleteService(service.raw()) };
    if ok == 0 {
        let error = code();
        // Already marked for deletion by an earlier removal: the service is
        // gone as far as a later setup is concerned.
        if error != ERROR_SERVICE_MARKED_FOR_DELETE {
            return Err(service_error("remove kGraf's launcher service", error));
        }
    }
    match stopped {
        Ok(()) => Ok(()),
        Err(why) => Err(failed(
            "stop kGraf's launcher service before removing it",
            format!(
                "the service was removed and its stop did not finish: {why}. Windows takes the \
                 service name back when the last handle to it closes, which for a running \
                 service means when its process ends or at the next restart"
            ),
        )),
    }
}

/// Ask the service to stop and wait for it, at most `timeout`. The text of
/// an error says what did not finish; the caller deletes the service either
/// way.
fn stop(service: &ScHandle, timeout: Duration) -> Result<(), String> {
    let mut s = SERVICE_STATUS::default();
    // SAFETY: a live service handle opened with SERVICE_STOP; the call
    // writes only `s`.
    let ok = unsafe { ControlService(service.raw(), SERVICE_CONTROL_STOP, &mut s) };
    if ok == 0 {
        let error = code();
        // Not running: nothing to wait for.
        if error == ERROR_SERVICE_NOT_ACTIVE {
            return Ok(());
        }
        return Err(format!(
            "asking it to stop failed: {} (Windows error {error})",
            std::io::Error::from_raw_os_error(error as i32)
        ));
    }
    let deadline = kgraf_core::waits::deadline_after(timeout);
    loop {
        match read_status(service) {
            Ok(s) if s.dwCurrentState == SERVICE_STOPPED => return Ok(()),
            Ok(_) => {}
            // The service control manager stops answering for a service it
            // has already forgotten, which is the state a stop aims at.
            Err(ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(()),
            Err(e) => {
                return Err(format!(
                    "reading its state failed: {} (Windows error {e})",
                    std::io::Error::from_raw_os_error(e as i32)
                ));
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("it was still stopping after {timeout:?}"));
        }
        std::thread::sleep(POLL);
    }
}

// ---- start ------------------------------------------------------------------

/// Start the service if it is not running, and wait for it to be running
/// (the owner's own process, unelevated).
///
/// The service is demand start, so it is not running until kGraf asks: this
/// is what the owner's start right is for. A service that is already running
/// is success, including when it starts between the read and the start call
/// (`ERROR_SERVICE_ALREADY_RUNNING`).
///
/// A service that goes to stopped after it was started is an error that names
/// its exit code: a launcher that cannot run must never look like one that
/// can.
///
/// The states Windows reports are the whole wait. Stopped and not started
/// yet asks for the start; stopped after the start carries the service's exit
/// code; a pending state, either a start or a stop an earlier removal asked
/// for, is waited out and then acted on.
pub fn start_if_stopped(name: &str, timeout: Duration) -> Result<(), SandboxError> {
    let manager = manager(
        SC_MANAGER_CONNECT,
        "open the service control manager to start the launcher service",
    )?;
    let service = service(&manager, name, SERVICE_START | SERVICE_QUERY_STATUS)?;
    let deadline = kgraf_core::waits::deadline_after(timeout);
    let mut asked = false;
    loop {
        let state = status(&service)?;
        match state.dwCurrentState {
            SERVICE_RUNNING => return Ok(()),
            SERVICE_STOPPED if !asked => {
                asked = true;
                // SAFETY: a live service handle opened with SERVICE_START; no
                // arguments, so the vector pointer is null.
                let ok = unsafe { StartServiceW(service.raw(), 0, ptr::null()) };
                if ok == 0 {
                    let error = code();
                    // It started between the read and this call.
                    if error != ERROR_SERVICE_ALREADY_RUNNING {
                        return Err(service_error("start kGraf's launcher service", error));
                    }
                }
            }
            SERVICE_STOPPED => {
                return Err(failed(
                    "start kGraf's launcher service",
                    format!(
                        "the service stopped as it started, with exit code {} \
                         (service-specific code {})",
                        state.dwWin32ExitCode, state.dwServiceSpecificExitCode
                    ),
                ));
            }
            // A start or a stop in flight: wait for it to settle.
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err(failed(
                "start kGraf's launcher service",
                format!(
                    "it was still in state {} after {timeout:?}",
                    state.dwCurrentState
                ),
            ));
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows itself must accept the access list the platform-neutral file
    /// writes. Parsing an SDDL string needs no administrator rights and no
    /// service, so this runs in every Windows test run.
    #[test]
    fn windows_accepts_the_launcher_services_access_list() {
        let sddl = service_dacl::dacl_sddl("S-1-5-21-1-2-3-1001").expect("access list");
        let w = wide(&sddl);
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `w` is NUL-terminated; on success the call allocates `sd`,
        // freed below.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "{sddl}: {}", std::io::Error::last_os_error());
        assert!(!sd.is_null());
        // SAFETY: frees the descriptor the conversion allocated.
        unsafe { LocalFree(sd as HLOCAL) };
    }

    /// A service that nothing installed is a state, not a failure: the first
    /// setup on a machine reads it, and so does `kgraf sandbox status`.
    #[test]
    fn a_service_that_is_not_installed_reads_as_nothing_installed() {
        let facts = read("kgraf-launcher-no-such-service").expect("read");
        assert_eq!(facts, ServiceFacts::default());
        assert_eq!(facts.description, None);
        assert_eq!(facts.running, None);
    }

    /// Removal must finish, so removing what is not there is success.
    #[test]
    fn removing_a_service_that_is_not_installed_is_nothing_to_do() {
        delete("kgraf-launcher-no-such-service", Duration::from_millis(100)).expect("remove");
    }
}
