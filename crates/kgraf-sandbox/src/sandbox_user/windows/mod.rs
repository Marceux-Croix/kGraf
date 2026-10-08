//! The Windows side of the sandbox user setup: thin calls that read the
//! machine into [`super::Observed`] and carry out the steps the platform-neutral
//! plan returns. Nothing here decides what to do.

pub mod accounts;
pub mod dpapi;
pub mod elevate;
pub mod firewall;
pub mod pipe;
pub mod runner;
pub mod setup;

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;

use crate::error::SandboxError;

/// A Win32 or NetAPI error code from `step`.
fn win32(step: &'static str, code: u32) -> SandboxError {
    SandboxError::PlatformSetup {
        step,
        source: io::Error::from_raw_os_error(code as i32),
    }
}

/// The calling thread's last error, from `step`.
fn last_error(step: &'static str) -> SandboxError {
    SandboxError::PlatformSetup {
        step,
        source: io::Error::last_os_error(),
    }
}

/// `s` as a NUL-terminated UTF-16 string.
fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The text of a NUL-terminated UTF-16 string a Windows call returned.
///
/// # Safety
/// `p` is null or points to a NUL-terminated UTF-16 string that stays valid
/// for the call.
unsafe fn from_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    // SAFETY: the caller promises a NUL-terminated string; reading stops at
    // the terminator.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units were just read from `p`.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}
