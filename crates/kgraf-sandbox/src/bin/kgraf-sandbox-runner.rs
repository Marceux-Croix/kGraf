//! The first process of a sandboxed command that runs as the Windows sandbox
//! user (decision 0097): it starts the command with the logon SID deny-only
//! and exits with the command's exit code. kGraf starts it; it is not run by
//! hand. Installed beside the `kgraf` executable.
//!
//! A windows-subsystem program: Windows gives it no console host and no
//! console window. A console host would run as the sandbox user with this
//! process's token, which still holds the owner's logon SID, and admit that
//! user fully, so the command could take it over. The standard handles kGraf
//! passes still arrive (`STARTF_USESTDHANDLES`).

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    std::process::exit(kgraf_sandbox::sandbox_user::windows::runner::main());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("kgraf-sandbox-runner runs only on Windows, where kGraf starts it");
    std::process::exit(2);
}
