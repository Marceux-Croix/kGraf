//! The Windows call of the sandbox user's jobserver
//! ([`super::super::jobserver`] says why, and holds the name, the variable's
//! value and the access list, tested on every host).

use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::Threading::CreateSemaphoreW;

use super::super::jobserver::{makeflags, semaphore_name, semaphore_sddl, tokens};
use super::super::launch_record;
use super::{last_error, wide};
use crate::error::SandboxError;

/// How many fresh names are tried before the call gives up. A name is taken
/// only by a program that made the same random name first; one retry would
/// do, and a few cost nothing.
const NAME_TRIES: usize = 8;

/// A jobserver semaphore made for one command, closed when dropped.
#[derive(Debug)]
pub(crate) struct Jobserver {
    _handle: OwnedHandle,
    /// The value for [`super::super::jobserver::JOBSERVER_VAR`].
    pub makeflags: String,
}

/// Make a jobserver semaphore that admits the sandbox user `user_sid` to
/// take and release its tokens. A name that already exists is never used:
/// `CreateSemaphoreW` would open the existing object, which another program
/// made, so the call closes it and tries a fresh name.
pub(crate) fn create(owner_sid: &str, user_sid: &str) -> Result<Jobserver, SandboxError> {
    let sddl = wide(&semaphore_sddl(owner_sid, user_sid));
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
        return Err(last_error(
            "build the jobserver semaphore's access list \
             (ConvertStringSecurityDescriptorToSecurityDescriptorW)",
        ));
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let count = tokens(std::thread::available_parallelism().map_or(1, usize::from));
    let made = (|| {
        for _ in 0..NAME_TRIES {
            let mut nonce = [0u8; 8];
            getrandom::fill(&mut nonce).map_err(|e| SandboxError::PlatformSetup {
                step: "name the jobserver semaphore (getrandom)",
                source: std::io::Error::other(e.to_string()),
            })?;
            let name = semaphore_name(nonce);
            let w = wide(&name);
            // SAFETY: `sa` and `w` live across the call; the result is a
            // handle or null.
            let h = unsafe { CreateSemaphoreW(&sa, count, count, w.as_ptr()) };
            // SAFETY: plain call, right after the one above.
            let code = unsafe { GetLastError() };
            if h.is_null() {
                return Err(last_error(
                    "make the jobserver semaphore (CreateSemaphoreW)",
                ));
            }
            // SAFETY: a fresh handle this code now owns.
            let handle = unsafe { OwnedHandle::from_raw_handle(h) };
            if code == ERROR_ALREADY_EXISTS {
                // Another program's object: closed, never used.
                drop(handle);
                continue;
            }
            launch_record::note_jobserver(&name, count, user_sid);
            return Ok(Jobserver {
                _handle: handle,
                makeflags: makeflags(&name),
            });
        }
        Err(SandboxError::PlatformSetup {
            step: "make the jobserver semaphore (CreateSemaphoreW)",
            source: std::io::Error::other(format!(
                "every one of {NAME_TRIES} fresh names already existed"
            )),
        })
    })();
    // SAFETY: frees the descriptor the conversion allocated, once.
    unsafe { LocalFree(sd as HLOCAL) };
    made
}
