//! The setup pipe between the user's own process and the elevated step.
//!
//! The user's unelevated process creates the pipe (first instance only, no
//! remote clients, a random name) before it starts the elevated step, so no
//! other program can create a pipe of that name first. Its access list admits
//! SYSTEM, the Administrators group (the elevated step's token) and the
//! user's own SID. Each side then checks the other: the user's process checks
//! that the client is the elevated child it started, and the elevated step
//! checks that the server is the process that started it.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_PIPE_CONNECTED, GENERIC_WRITE, GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
    LocalFree, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

use super::super::handshake::MAX_LINE;
use super::{last_error, wide, win32};
use crate::error::SandboxError;

/// The server end, held by the user's own process.
pub struct Server {
    handle: OwnedHandle,
    name: String,
}

/// Create the pipe `name` for the owner with SID `owner_sid`.
pub fn create(name: &str, owner_sid: &str) -> Result<Server, SandboxError> {
    // Protected DACL: SYSTEM, Administrators (the elevated step) and the owner.
    let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{owner_sid})"));
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
        return Err(last_error("build the setup pipe's access list"));
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let w = wide(name);
    // SAFETY: creates one inbound instance; `sa` and `w` live across the call.
    let h = unsafe {
        CreateNamedPipeW(
            w.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            0,
            MAX_LINE as u32,
            0,
            &sa,
        )
    };
    // SAFETY: frees the descriptor the conversion allocated.
    unsafe { LocalFree(sd as HLOCAL) };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(last_error("create the setup pipe"));
    }
    // SAFETY: a fresh handle this code now owns.
    let handle = unsafe { OwnedHandle::from_raw_handle(h) };
    Ok(Server {
        handle,
        name: name.to_owned(),
    })
}

impl Server {
    /// Wait for the elevated child `child` (pid `child_pid`) to connect, then
    /// return its lines as they arrive. Fails if the child ends first or a
    /// different process connects.
    pub fn accept(
        self,
        child: &OwnedHandle,
        child_pid: u32,
    ) -> Result<mpsc::Receiver<Result<String, String>>, SandboxError> {
        wait_for_client(
            &self.handle,
            &self.name,
            child,
            child_pid,
            "wait for the elevated step",
            "the elevated step ended before it connected (UAC refused or it failed to start)",
        )?;
        let file = File::from(self.handle);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(file).lines() {
                let msg = line.map_err(|e| e.to_string());
                let stop = msg.is_err();
                if tx.send(msg).is_err() || stop {
                    break;
                }
            }
        });
        Ok(rx)
    }
}

/// Wait until a client connects to the server end `server` of pipe `name`,
/// then check that it is process `child_pid`. Fails with `early_end` if
/// `child` ends first, and when another process connected. Shared by the
/// setup pipe and the runner pipe.
pub(super) fn wait_for_client(
    server: &OwnedHandle,
    name: &str,
    child: &OwnedHandle,
    child_pid: u32,
    step: &'static str,
    early_end: &'static str,
) -> Result<(), SandboxError> {
    let raw = server.as_raw_handle() as usize;
    let (connected_tx, connected_rx) = mpsc::channel::<Result<(), u32>>();
    std::thread::spawn(move || {
        // SAFETY: a blocking connect on the server handle, which outlives
        // this thread's use of it (the caller waits for the result).
        let ok = unsafe { ConnectNamedPipe(raw as HANDLE, ptr::null_mut()) };
        // SAFETY: reads the thread's last error.
        let err = unsafe { GetLastError() };
        let _ = connected_tx.send(if ok != 0 || err == ERROR_PIPE_CONNECTED {
            Ok(())
        } else {
            Err(err)
        });
    });
    loop {
        match connected_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(())) => break,
            Ok(Err(code)) => return Err(win32(step, code)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // SAFETY: a zero-timeout check of the child's handle.
                if unsafe { WaitForSingleObject(child.as_raw_handle(), 0) } == WAIT_OBJECT_0 {
                    // Unblock the connect thread by connecting to our own
                    // pipe, then report the child's early end.
                    let _ = connect_client(name);
                    return Err(SandboxError::PlatformSetup {
                        step,
                        source: std::io::Error::other(early_end),
                    });
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(win32(step, 0)),
        }
    }
    let mut client_pid = 0u32;
    // SAFETY: reads the connected client's process ID.
    if unsafe { GetNamedPipeClientProcessId(server.as_raw_handle(), &mut client_pid) } == 0 {
        return Err(last_error(step));
    }
    if client_pid != child_pid {
        return Err(SandboxError::PlatformSetup {
            step,
            source: std::io::Error::other(format!(
                "process {client_pid} connected, not the expected process {child_pid}"
            )),
        });
    }
    Ok(())
}

fn connect_client(name: &str) -> Result<File, SandboxError> {
    let w = wide(name);
    // SAFETY: opens the pipe for writing; the handle is owned by the File.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            GENERIC_WRITE,
            0,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(last_error("open the setup pipe"));
    }
    // SAFETY: a fresh handle this code now owns.
    Ok(File::from(unsafe { OwnedHandle::from_raw_handle(h) }))
}

/// The client end, held by the elevated step.
pub struct Client(File);

/// Connect to pipe `name` and check its server is process `owner_pid`.
pub fn connect(name: &str, owner_pid: u32) -> Result<Client, SandboxError> {
    let file = connect_client(name)?;
    let mut server = 0u32;
    // SAFETY: reads the pipe server's process ID.
    if unsafe { GetNamedPipeServerProcessId(file.as_raw_handle(), &mut server) } == 0 {
        return Err(last_error("identify the setup pipe's server"));
    }
    if server != owner_pid {
        return Err(SandboxError::PlatformSetup {
            step: "identify the setup pipe's server",
            source: std::io::Error::other(format!(
                "the pipe belongs to process {server}, not the process {owner_pid} that started \
                 this step"
            )),
        });
    }
    Ok(Client(file))
}

impl Client {
    /// Send one encoded message line.
    pub fn send(&mut self, line: &str) -> Result<(), SandboxError> {
        self.0
            .write_all(line.as_bytes())
            .and_then(|()| self.0.flush())
            .map_err(|source| SandboxError::PlatformSetup {
                step: "write to the setup pipe",
                source,
            })
    }
}
