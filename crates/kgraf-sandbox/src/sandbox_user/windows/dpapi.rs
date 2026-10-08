//! DPAPI for the current user: [`super::super::Protector`] on Windows.
//!
//! `CryptProtectData` without `CRYPTPROTECT_LOCAL_MACHINE` seals data with
//! a key derived from the current user's logon credentials, so only this
//! user (on this machine) can open it.

use std::ptr;

use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use zeroize::Zeroizing;

use super::super::blob::Protector;

/// DPAPI for the current user.
pub struct Dpapi;

fn blob_of(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr().cast_mut(),
    }
}

/// Copies the output blob and frees it.
///
/// # Safety
/// `out` was filled by a successful CryptProtectData or CryptUnprotectData.
unsafe fn take(out: &CRYPT_INTEGER_BLOB) -> Vec<u8> {
    // SAFETY: the call allocated `cbData` bytes at `pbData`.
    let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    // SAFETY: frees the output buffer the call allocated with LocalAlloc.
    unsafe { LocalFree(out.pbData as HLOCAL) };
    v
}

impl Protector for Dpapi {
    fn protect(&self, plain: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob_of(plain);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        // SAFETY: `input` borrows `plain`; the call allocates `out`.
        let ok = unsafe {
            CryptProtectData(
                &input,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // SAFETY: the call succeeded.
        Ok(unsafe { take(&out) })
    }

    fn unprotect(&self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
        let input = blob_of(sealed);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        // SAFETY: `input` borrows `sealed`; the call allocates `out`.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // SAFETY: `out` holds `cbData` plaintext bytes; they are wiped before
        // the buffer is freed, and the copy lives in a Zeroizing vector.
        let plain = unsafe {
            let bytes = std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize);
            let copy = Zeroizing::new(bytes.to_vec());
            bytes.iter_mut().for_each(|b| *b = 0);
            LocalFree(out.pbData as HLOCAL);
            copy
        };
        Ok(plain)
    }
}
