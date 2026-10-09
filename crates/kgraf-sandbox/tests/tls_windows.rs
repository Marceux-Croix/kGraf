//! HTTPS from inside the sandbox on Windows: what works and what does not.
//!
//! **Status: written for real Windows, NOT YET RUN.** The development host is
//! macOS; this file is compile-checked for the Windows targets only. Run it on
//! Windows 10 1803 or later (which ship `curl.exe` in System32), as a normal
//! (non-elevated) user, with Git for Windows installed:
//! `cargo test -p kgraf-sandbox --test tls_windows`.
//!
//! The server is a TCP listener on 127.0.0.1 that only reads the first bytes
//! the client sends: a TLS handshake starts with a record of type 22
//! (`0x16`), version major 3. No certificate, no internet, no proxy.
//!
//! The documented state (decision 0011, the Schannel amendment and its note
//! on the TLS backend):
//!
//! - Outside the sandbox, `curl.exe` from System32 (Schannel) starts a
//!   handshake. This is the control.
//! - Inside the sandbox, Schannel cannot run: its credential call returns
//!   `SEC_E_NO_CREDENTIALS` for a token with restricting SIDs, so the same
//!   `curl.exe` stops before it sends a byte.
//! - Inside the sandbox, the curl of Git for Windows
//!   (`mingw64\bin\curl.exe`, OpenSSL) starts a handshake, with the
//!   environment the sandbox gives every command that runs with the network
//!   open (`CURL_SSL_BACKEND=openssl`).
//!
//! The unit tests in `src/sandbox/windows.rs` (`schannel`) pin the cause of
//! the Schannel refusal.

#![cfg(windows)]

mod common;

use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

use kgraf_sandbox::git_for_windows::{Probe as _, SystemProbe};
use kgraf_sandbox::{Network, SandboxPolicy, sandboxed_command};

/// `curl.exe` in System32: always the Schannel build.
fn system_curl() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let curl = Path::new(&root).join("System32").join("curl.exe");
    assert!(
        curl.is_file(),
        "{} is missing (Windows 10 1803 or later ships it)",
        curl.display()
    );
    curl
}

/// `mingw64\bin\curl.exe` of Git for Windows (the OpenSSL build), found from
/// the `bin\bash.exe` that kGraf itself finds. `None`, with the reason
/// printed, when Git for Windows or its curl is not there.
fn git_curl() -> Option<PathBuf> {
    let bash = match SystemProbe.find() {
        Ok(bash) => bash,
        Err(why) => {
            println!("skipped: no Git for Windows found ({why})");
            return None;
        }
    };
    let root = bash.parent().and_then(Path::parent)?;
    let curl = root.join("mingw64").join("bin").join("curl.exe");
    match kgraf_core::plain_path::canonicalize(&curl) {
        Ok(curl) if curl.is_file() => Some(curl),
        _ => {
            println!(
                "skipped: {} is missing from the Git for Windows install",
                curl.display()
            );
            None
        }
    }
}

/// The arguments for one HTTPS request to `127.0.0.1:port`: no proxy from
/// the environment, no certificate or revocation check (the listener has
/// no certificate; the test only needs the handshake to start).
fn curl_args(port: u16) -> Vec<String> {
    [
        "-sS",
        "-k",
        "--ssl-no-revoke",
        "--noproxy",
        "*",
        "--max-time",
        "15",
        "-o",
        "NUL",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([format!("https://127.0.0.1:{port}/")])
    .collect()
}

/// A listener on a free loopback port that accepts one connection and
/// sends back up to the first 5 bytes it reads (a TLS record header), or
/// fewer if the client closes first.
fn first_bytes_server() -> (u16, mpsc::Receiver<std::io::Result<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let read = || -> std::io::Result<Vec<u8>> {
            let (mut conn, _) = listener.accept()?;
            conn.set_read_timeout(Some(Duration::from_secs(15)))?;
            let mut buf = [0u8; 5];
            let mut got = 0;
            while got < buf.len() {
                match conn.read(&mut buf[got..])? {
                    0 => break,
                    n => got += n,
                }
            }
            Ok(buf[..got].to_vec())
        };
        let _ = tx.send(read());
    });
    (port, rx)
}

/// Whether `bytes` begin a TLS handshake record: type 22, version 3.x.
fn is_tls_handshake(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0x16 && bytes[1] == 0x03
}

/// curl's exit code and stderr, for failure messages.
fn said(curl: &Output) -> String {
    format!(
        "curl exit {:?}, stderr: {}",
        curl.status.code(),
        String::from_utf8_lossy(&curl.stderr).trim()
    )
}

/// What the listener read, with curl's own output for the failure message.
fn first_bytes(
    rx: &mpsc::Receiver<std::io::Result<Vec<u8>>>,
    curl: &Output,
) -> Result<Vec<u8>, String> {
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(e)) => Err(format!("the listener failed: {e}; {}", said(curl))),
        Err(_) => Err(format!("curl never connected; {}", said(curl))),
    }
}

/// Runs `curl` inside the sandbox, with the network allowed and the
/// environment the sandbox builds, against a fresh listener. Returns what the
/// listener read and curl's output.
fn curl_in_the_sandbox(curl: &Path) -> (Result<Vec<u8>, String>, Output) {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("box");
    let scratch = tmp.path().join("tmp");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(&scratch).unwrap();
    let policy = SandboxPolicy::new(vec![folder.clone(), scratch.clone()], Network::Allow)
        .accept_partial_isolation(true)
        .with_tmp_dir(scratch);
    let (port, rx) = first_bytes_server();
    let out = sandboxed_command(&policy, curl, &folder)
        .unwrap()
        .args(curl_args(port))
        .output()
        .unwrap_or_else(|e| panic!("{} did not run in the sandbox: {e}", curl.display()));
    (first_bytes(&rx, &out), out)
}

/// The control: on this machine, outside the sandbox, the same curl starts
/// a handshake with the same listener. Without it a failure inside the
/// sandbox could be a host without working Schannel.
#[test]
fn curl_starts_a_tls_handshake_outside_the_sandbox() {
    let (port, rx) = first_bytes_server();
    let out = Command::new(system_curl())
        .args(curl_args(port))
        .output()
        .unwrap();
    let bytes = first_bytes(&rx, &out).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        is_tls_handshake(&bytes),
        "no TLS handshake record ({bytes:02x?}); {}",
        said(&out)
    );
}

/// Inside the sandbox, the curl of Git for Windows (OpenSSL) starts a
/// handshake, with no setting from this test: the environment the sandbox
/// builds selects the backend.
#[test]
fn git_for_windows_curl_starts_a_tls_handshake_inside_the_sandbox() {
    let found = git_curl();
    assert!(
        found.is_some() || !common::require_sandbox_user(),
        "Git for Windows and its OpenSSL curl are not here, so this test measures nothing. \
         A run that must measure the sandbox user needs them: install Git for Windows"
    );
    let Some(curl) = found else {
        return;
    };
    let (bytes, out) = curl_in_the_sandbox(&curl);
    let bytes = bytes.unwrap_or_else(|e| panic!("{e}"));
    assert!(
        is_tls_handshake(&bytes),
        "no TLS handshake record from the Git for Windows curl inside the sandbox \
         ({bytes:02x?}); {}",
        said(&out)
    );
}

/// The documented gap: inside the sandbox, the curl of System32, whose only
/// TLS backend is Schannel, still stops at `AcquireCredentialsHandle`
/// (`SEC_E_NO_CREDENTIALS`) and sends no handshake. If this test fails
/// because a handshake arrived, the gap is closed: update decision 0011 and
/// the gaps text of the Windows backend.
/// The Windows TLS stack inside the sandbox, against what the backend in use
/// claims. Under the token sandbox it cannot work: Schannel refuses a client
/// credential to a token that carries restricting SIDs, and those SIDs are
/// that backend's write boundary (decision 0011, Schannel amendment). Under
/// the sandbox user the token carries none, so it must work; that is one of
/// the two failures decision 0097 exists to fix.
#[test]
fn system32_curl_starts_tls_only_where_the_backend_allows_it() {
    // `KGRAF_REQUIRE_SANDBOX_USER=1` fails here when the machine runs the
    // token sandbox, so a run about the sandbox user cannot pass on the
    // other backend's branch.
    let as_user = common::sandbox_user_mode();
    let (bytes, out) = curl_in_the_sandbox(&system_curl());
    let sent = bytes.as_deref().map(is_tls_handshake).unwrap_or(false);
    if as_user {
        assert!(
            sent,
            "the Windows TLS stack did not start a handshake as the sandbox user, where no \
             restricting SID should stop it; {}",
            said(&out)
        );
        return;
    }
    // A listener that read nothing is the expected shape; one that never saw
    // a connection is also a failed handshake, and curl's message decides.
    let stderr = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
    assert!(
        !sent,
        "a TLS handshake arrived from the Schannel curl inside the sandbox: the documented \
         gap (decision 0011, Schannel amendment) is closed; update the decision and the \
         backend's gaps text; {}",
        said(&out)
    );
    assert!(
        stderr.contains("sec_e_no_credentials") || stderr.contains("0x8009030e"),
        "the Schannel curl failed inside the sandbox for another reason than the documented \
         SEC_E_NO_CREDENTIALS (decision 0011, Schannel amendment); {}",
        said(&out)
    );
}
