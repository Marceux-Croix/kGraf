//! The Windows calls of the named-object probe
//! ([`super::super::probe`] says why, and holds the subcommand, the names
//! and every line, tested on every host).

use std::io;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Threading::{CreateMutexW, CreateSemaphoreW};

use super::super::probe::Object;
use super::wide;

/// Make the named object `object` with the name `name`, and keep it until
/// the handle is dropped.
///
/// The object is made with the default access list and no namespace prefix,
/// which is exactly what cargo's jobserver and rustc's diagnostic mutex do.
/// A probe that asked for anything else would measure a call no build makes.
///
/// The error text carries Windows' own wording for the code and the code
/// itself, so a caller can tell a refused object from any other failure
/// ([`super::super::named_objects::denied`]).
pub(crate) fn make(object: Object, name: &str) -> Result<OwnedHandle, String> {
    let w = wide(name);
    let handle = match object {
        // One token, one at most: the probe only proves the object can be
        // made, and never waits on it.
        // SAFETY: `w` is NUL-terminated and lives across the call, which
        // returns a handle or null.
        Object::Semaphore => unsafe { CreateSemaphoreW(ptr::null(), 1, 1, w.as_ptr()) },
        // Not owned by the caller, as rustc's own call leaves it.
        // SAFETY: as above.
        Object::Mutex => unsafe { CreateMutexW(ptr::null(), 0, w.as_ptr()) },
    };
    // SAFETY: plain call, right after the one above, before anything else
    // can overwrite the thread's last error.
    let code = unsafe { GetLastError() };
    if handle.is_null() {
        return Err(format!(
            "{} (Windows error {code})",
            io::Error::from_raw_os_error(code as i32)
        ));
    }
    // SAFETY: a fresh handle this code now owns, closed when it is dropped.
    let owned = unsafe { OwnedHandle::from_raw_handle(handle) };
    if code == ERROR_ALREADY_EXISTS {
        // The call opened an object another program made, so it proves a
        // lookup and not a creation. With a random name of sixty-four bits
        // this does not happen; it is refused rather than counted, so a
        // proof always means what it says. The words carry no access denial,
        // so this reads as a probe that could not run and never moves the
        // machine to the weaker fallback.
        drop(owned);
        return Err(format!(
            "the name {name} already existed, so the probe made nothing (Windows error {code})"
        ));
    }
    Ok(owned)
}
