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

/// Decision 0094: a run in Bypass announces it on stderr at start, whatever
/// set the mode: the flag, or the user's permissions file with no flag. The
/// notice is written before anything else the run does, so a run that then
/// fails (here: the directory is not a git repository) has still announced it.
/// A run not in Bypass prints no notice.
#[test]
fn a_bypass_run_announces_it_on_stderr_from_either_source() {
    let project = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("home");
    let bypass_file = home.path().join("bypass.toml");
    std::fs::write(&bypass_file, "mode = \"bypass\"\n").expect("write");
    let manual_file = home.path().join("manual.toml");
    std::fs::write(&manual_file, "mode = \"manual\"\n").expect("write");
    let run = |file: &std::path::Path, extra: &[&str]| {
        let mut args = vec![
            "-C".to_owned(),
            project.path().to_string_lossy().into_owned(),
            "orchestrate".to_owned(),
            "--instruction".to_owned(),
            "do nothing".to_owned(),
            "--route".to_owned(),
            "ollama:none".to_owned(),
            "--prices".to_owned(),
            home.path()
                .join("prices.json")
                .to_string_lossy()
                .into_owned(),
        ];
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        Command::new(env!("CARGO_BIN_EXE_kgraf"))
            .env("KGRAF_DIAG_DISABLE", "1")
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("XDG_DATA_HOME", home.path())
            .env("KGRAF_PERMISSIONS_PATH", file)
            .env_remove("KGRAF_CONTAINER_SANDBOX")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run kgraf")
    };
    let notice = "BYPASS PERMISSION MODE";
    for (label, out) in [
        ("flag", run(&manual_file, &["--permission-mode", "bypass"])),
        ("user file", run(&bypass_file, &[])),
    ] {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stderr.matches(notice).count(),
            1,
            "{label}: one notice on stderr: {stderr}"
        );
        assert!(!stdout.contains(notice), "{label}: not on stdout: {stdout}");
        assert!(!stderr.contains("panicked"), "{label}: {stderr}");
    }
    let out = run(&manual_file, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains(notice), "manual: no notice: {stderr}");
}
