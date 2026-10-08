//! Process identity on Windows: whether a PID names a running process, its
//! creation time, and the machine and boot ids [`crate::HostId`] records.
//!
//! **Liveness.** A process is opened with `PROCESS_QUERY_LIMITED_INFORMATION`
//! and `PROCESS_SYNCHRONIZE`, then waited on with a zero timeout: a signaled
//! handle means the process has exited. A process object outlives its exit
//! while any handle to it is open, and its PID is not given to another process
//! until then, so "opened" does not mean "running". The exit code is not used
//! for that where the wait is possible: `GetExitCodeProcess` reports
//! `STILL_ACTIVE` (259) for a running process, and a process can also exit
//! with 259 (Microsoft Learn, "GetExitCodeProcess function", Remarks).
//!
//! `OpenProcess` fails with `ERROR_INVALID_PARAMETER` for a PID that names no
//! process (the error psutil also reads as "no such process":
//! `psutil/arch/windows/proc_utils.c`, BSD-3-Clause). `ERROR_ACCESS_DENIED`
//! means the process exists but this user may not open it (another user's
//! service, a protected process, the System process: Microsoft Learn,
//! "OpenProcess function", Remarks). Such a process is running, with an
//! identity that cannot be read; it is never reported as gone. Any other
//! failure is treated the same way.
//!
//! **Identity.** The creation time from `GetProcessTimes`: a `FILETIME`, the
//! count of 100 ns intervals since 1601-01-01 UTC, recorded as one `u64`
//! (`high << 32 | low`). It is absolute (not relative to boot) and fixed when
//! the process is created, so a later process that reuses a PID has another
//! creation time, in this boot or after a restart.
//!
//! **Boot id.** `KUSER_SHARED_DATA.BootId`: "Boot sequence, incremented for
//! each boot attempt by the OS loader" (Microsoft Learn, "KUSER_SHARED_DATA
//! structure (ntddk.h)"), at offset 0x2C4 of the page the kernel maps
//! read-only at 0x7FFE0000 in every process (offset as asserted in the phnt
//! headers, `ntexapi.h`, `static_assert(FIELD_OFFSET(KUSER_SHARED_DATA, ...))`).
//! It changes only when the loader starts the system, never while it runs.
//!
//! Rejected:
//! - the boot time (`NtQuerySystemInformation(SystemTimeOfDayInformation)`
//!   `BootTime`, or now minus `GetTickCount64`): the kernel moves the boot
//!   time when the system clock is set, and the tick count drifts against the
//!   clock, so the value can change within one boot. A changed boot id on a
//!   known machine reads as a restart, which judges every live owner dead:
//!   the one failure that deletes live work.
//! - the creation time of the System process (PID 4): `OpenProcess` on it
//!   fails with `ERROR_ACCESS_DENIED` for user-level code (Microsoft Learn,
//!   "OpenProcess function").
//! - the registry value `PrefetchParameters\BootId`: the same counter, read
//!   through an undocumented registry value instead of a documented structure.
//!
//! **Fast startup.** A "shut down" with fast startup closes all applications
//! and logs off all user sessions, then hibernates the kernel session; the
//! next start resumes that kernel (Microsoft Learn, "Distinguishing fast
//! startup from wake-from-hibernation"). Whether the loader counts that as a
//! boot attempt is not documented, so the boot id may stay the same. That is
//! safe: every kGraf process ran in a user session and is gone, and with an
//! equal boot id an owner is judged by PID and creation time. Its PID is then
//! either free (dead) or held by a later process with a later creation time
//! (dead). An equal boot id never keeps a dead owner alive; only a boot id
//! that changes within one boot could judge a live owner dead, and this one
//! does not.
//!
//! **Machine id.** `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`, written
//! once when Windows is installed and readable by every user. Read from the
//! 64-bit registry view, so a 32-bit build reads the same value.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_SUCCESS, FILETIME, GetLastError,
    STILL_ACTIVE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

use crate::workspace::{Probe, filetime_to_u64};

/// Where the kernel maps `KUSER_SHARED_DATA` in every user-mode process.
const KUSER_SHARED_DATA: usize = 0x7FFE_0000;
/// Offset of `BootId` in it.
const BOOT_ID_OFFSET: usize = 0x2C4;

/// What the process table says about `pid` now (see the module doc).
pub(crate) fn probe(pid: u32) -> Probe {
    match open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE) {
        Ok(handle) => {
            // SAFETY: `handle` is an open process handle with SYNCHRONIZE
            // access; a zero timeout only tests whether it is signaled.
            match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
                WAIT_OBJECT_0 => Probe::Gone,
                WAIT_TIMEOUT => Probe::Running {
                    start: creation_time(&handle),
                },
                // WAIT_FAILED: nothing is known, so it counts as running.
                _ => Probe::Running { start: None },
            }
        }
        Err(ERROR_INVALID_PARAMETER) => Probe::Gone,
        // A process this user may query but not wait on (some services grant
        // only query access): the exit code tells a running process from an
        // exited one, except one that exited with 259, which counts as
        // running (never wrongly gone).
        Err(ERROR_ACCESS_DENIED) => match open(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
            Ok(handle) => {
                let mut code = 0u32;
                // SAFETY: `handle` is open with query access; `code` is a
                // valid u32 the call writes.
                let read = unsafe { GetExitCodeProcess(handle.as_raw_handle(), &mut code) };
                if read != 0 && code != STILL_ACTIVE as u32 {
                    Probe::Gone
                } else {
                    Probe::Running {
                        start: creation_time(&handle),
                    }
                }
            }
            Err(_) => Probe::Running { start: None },
        },
        Err(_) => Probe::Running { start: None },
    }
}

/// `OpenProcess`, owning the handle; `Err` carries the thread's last error.
fn open(pid: u32, access: u32) -> Result<OwnedHandle, u32> {
    // SAFETY: OpenProcess takes no pointers; a null result is checked below.
    let raw = unsafe { OpenProcess(access, 0, pid) };
    if raw.is_null() {
        // SAFETY: reads this thread's last error, set by the failed call.
        return Err(unsafe { GetLastError() });
    }
    // SAFETY: `raw` is a handle OpenProcess just returned, owned by no one
    // else; OwnedHandle closes it once.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// The process's creation time, or `None` when it cannot be read.
fn creation_time(handle: &OwnedHandle) -> Option<u64> {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: `handle` is open with PROCESS_QUERY_LIMITED_INFORMATION, which
    // GetProcessTimes requires; the four FILETIMEs are valid for writing.
    let ok = unsafe {
        GetProcessTimes(
            handle.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    let t = filetime_to_u64(created.dwLowDateTime, created.dwHighDateTime);
    (t != 0).then_some(t)
}

/// This boot's sequence number (see the module doc); `None` when it reads 0,
/// which no boot of a supported Windows writes.
pub(crate) fn boot_sequence() -> Option<u32> {
    let address = std::ptr::with_exposed_provenance::<u32>(KUSER_SHARED_DATA + BOOT_ID_OFFSET);
    // SAFETY: the kernel maps KUSER_SHARED_DATA read-only at this fixed
    // address in every user-mode process on every supported Windows, and
    // `BootId` is an aligned u32 inside it. Volatile: the kernel owns the page.
    let id = unsafe { std::ptr::read_volatile(address) };
    (id != 0).then_some(id)
}

/// `MachineGuid` (see the module doc); `None` when it cannot be read.
pub(crate) fn machine_guid() -> Option<String> {
    let key: Vec<u16> = "SOFTWARE\\Microsoft\\Cryptography\0"
        .encode_utf16()
        .collect();
    let value: Vec<u16> = "MachineGuid\0".encode_utf16().collect();
    // A GUID in braces is 38 characters; room to spare, with the NUL.
    let mut buf = [0u16; 128];
    let mut bytes = u32::try_from(std::mem::size_of_val(&buf)).ok()?;
    // SAFETY: both names are NUL-terminated UTF-16; `buf` has `bytes` bytes,
    // and the call writes at most that many and stores the count in `bytes`.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let units = (bytes as usize / 2).min(buf.len());
    let text = String::from_utf16(&buf[..units]).ok()?;
    let id = text.trim_end_matches('\0').trim();
    (!id.is_empty() && !id.contains(':') && !id.contains(char::is_whitespace))
        .then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{HostId, Liveness, owner_liveness};
    use std::process::{Command, Stdio};

    /// A child that waits on its standard input until it is killed.
    fn live_child() -> std::process::Child {
        Command::new("cmd")
            .args(["/D", "/C", "pause"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd")
    }

    fn exited_child(code: u32) -> std::process::Child {
        let mut child = Command::new("cmd")
            .args(["/D", "/C", &format!("exit {code}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd");
        let status = child.wait().expect("wait");
        assert_eq!(status.code(), Some(code as i32));
        child
    }

    #[test]
    fn a_live_child_is_running_with_its_creation_time() {
        let mut child = live_child();
        let pid = child.id();
        let Probe::Running { start: Some(start) } = probe(pid) else {
            panic!("a live child must be running with a readable start time");
        };
        // The same value on a second read: the pair is compared exactly.
        assert_eq!(probe(pid), Probe::Running { start: Some(start) });
        let here = HostId::current();
        assert_eq!(
            owner_liveness(pid, Some(start), Some(&here)),
            Liveness::Alive
        );
        // A PID reused by another process: the recorded start time differs.
        assert_eq!(
            owner_liveness(pid, Some(start + 1), Some(&here)),
            Liveness::Dead,
            "another start time under the same PID is a reused PID"
        );
        assert_eq!(
            owner_liveness(pid, Some(start - 1), None),
            Liveness::Dead,
            "with no host recorded too"
        );
        child.kill().expect("kill");
        child.wait().expect("wait");
    }

    /// The child's handle stays open (the `Child` is kept), so its process
    /// object and PID still exist: only the wait tells it has exited.
    #[test]
    fn an_exited_child_is_gone_while_its_handle_is_open() {
        let mut child = exited_child(0);
        assert_eq!(probe(child.id()), Probe::Gone);
        assert_eq!(
            owner_liveness(child.id(), None, Some(&HostId::current())),
            Liveness::Dead
        );
        // Already waited in `exited_child`; this returns the kept status.
        child.wait().expect("wait");
    }

    /// 259 is `STILL_ACTIVE`: an exit code check alone would call it running.
    #[test]
    fn a_child_that_exited_with_259_is_gone() {
        let mut child = exited_child(259);
        assert_eq!(probe(child.id()), Probe::Gone);
        child.wait().expect("wait");
    }

    #[test]
    fn a_reaped_child_is_gone() {
        let pid = exited_child(0).id();
        assert_eq!(probe(pid), Probe::Gone);
        assert_eq!(
            owner_liveness(pid, None, Some(&HostId::current())),
            Liveness::Dead
        );
    }

    /// The System process cannot be opened by user-level code (or, run as an
    /// administrator, can be): either way it is running, never gone.
    #[test]
    fn a_process_that_cannot_be_opened_is_running_not_gone() {
        assert!(matches!(probe(4), Probe::Running { .. }), "{:?}", probe(4));
    }

    #[test]
    fn this_process_is_running_and_its_start_time_matches_its_record() {
        let me = crate::ProcessIdentity::current();
        assert!(me.start.is_some(), "{me:?}");
        assert_eq!(probe(me.pid), Probe::Running { start: me.start });
        assert!(me.is_alive());
    }

    #[test]
    fn the_boot_and_machine_ids_are_read_and_stable() {
        let boot = boot_sequence().expect("a boot sequence");
        assert_eq!(boot_sequence(), Some(boot), "stable within one boot");
        let machine = machine_guid().expect("a machine guid");
        assert_eq!(machine_guid().as_deref(), Some(machine.as_str()));
        let here = HostId::current();
        assert_eq!(here.machine.as_deref(), Some(machine.as_str()));
        assert_eq!(here.boot, Some(boot.to_string()));
        assert_eq!(HostId::current(), here);
    }
}
