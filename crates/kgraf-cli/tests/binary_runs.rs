//! The built `kgraf` binary runs. A debug build panicked on every command
//! when the log subscriber asked for ANSI colors without tracing-subscriber's
//! `ansi` feature; unit tests never install the process subscriber, so only
//! running the binary catches it.

use std::process::Command;

#[test]
fn the_binary_runs_a_command_without_a_panic() {
    let dir = tempfile::tempdir().expect("dir");
    let out = Command::new(env!("CARGO_BIN_EXE_kgraf"))
        .env("KGRAF_DIAG_DISABLE", "1")
        .args(["-C", &dir.path().to_string_lossy(), "providers"])
        .output()
        .expect("run kgraf");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{}: {stderr}", out.status);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(!stderr.contains("ansi"), "{stderr}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("ollama"),
        "the catalog was printed"
    );
}

/// Decision 0062: a variable set only under the former name (`HAUSTORIA_LOG`)
/// is used, and one warning names the new variable. The value is shown to be
/// read: a directive naming a locked target is reported as having no effect.
#[test]
fn the_former_log_variable_is_used_with_one_warning() {
    let dir = tempfile::tempdir().expect("dir");
    let out = Command::new(env!("CARGO_BIN_EXE_kgraf"))
        .env("KGRAF_DIAG_DISABLE", "1")
        .args(["-C", &dir.path().to_string_lossy(), "providers"])
        .env_remove("KGRAF_LOG")
        .env("HAUSTORIA_LOG", "rmcp::transport::auth=trace")
        .output()
        .expect("run kgraf");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{}: {stderr}", out.status);
    assert_eq!(
        stderr.matches("rename it to KGRAF_LOG").count(),
        1,
        "one warning naming the new variable: {stderr}"
    );
    assert!(
        stderr.contains("`rmcp::transport::auth=trace` has no effect"),
        "the former variable's value was read: {stderr}"
    );

    // Both set: the new one wins, and the ignored one is said.
    let out = Command::new(env!("CARGO_BIN_EXE_kgraf"))
        .env("KGRAF_DIAG_DISABLE", "1")
        .args(["-C", &dir.path().to_string_lossy(), "providers"])
        .env("KGRAF_LOG", "error")
        .env("HAUSTORIA_LOG", "rmcp::transport::auth=trace")
        .output()
        .expect("run kgraf");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("HAUSTORIA_LOG is ignored"), "{stderr}");
    assert!(!stderr.contains("has no effect"), "{stderr}");
}
