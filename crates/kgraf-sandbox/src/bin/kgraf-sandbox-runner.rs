//! The first process of a sandboxed command that runs as the Windows sandbox
//! user (decision 0097): it starts the command with the logon SID deny-only
//! and exits with the command's exit code. kGraf starts it; it is not run by
//! hand. Installed beside the `kgraf` executable.

#[cfg(windows)]
fn main() {
    std::process::exit(kgraf_sandbox::sandbox_user::windows::runner::main());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("kgraf-sandbox-runner runs only on Windows, where kGraf starts it");
    std::process::exit(2);
}
