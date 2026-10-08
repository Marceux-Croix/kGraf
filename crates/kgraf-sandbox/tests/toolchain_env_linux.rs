//! A toolchain outside the home folder, named by `RUSTUP_HOME` and
//! `CARGO_HOME`, is readable and runnable inside the Linux sandbox; the cargo
//! home's credentials are not.
//!
//! One test in its own binary: it sets process environment variables, which
//! no other test in the process may race with.

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use kgraf_sandbox::{Network, SandboxPolicy, sandboxed_command};

/// A program that prints `ok`, at `dir/rel`, mode 0755.
fn tool(dir: &Path, rel: &str) -> PathBuf {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "#!/bin/sh\necho ok\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The command's result, or the error that stopped it from starting: a
/// program the sandbox cannot read fails at exec, before it prints anything.
type Ran = Result<Output, String>;

fn run(work: &Path, program: &Path, args: &[&str]) -> (Ran, Vec<String>) {
    let policy = SandboxPolicy::new(vec![work.to_path_buf()], Network::Deny);
    let mut cmd = sandboxed_command(&policy, program, work).unwrap();
    let notes = cmd.refused_reads().to_vec();
    let out = cmd.args(args).output().map_err(|e| e.to_string());
    (out, notes)
}

fn ran(out: &Ran) -> bool {
    out.as_ref()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout) == "ok\n")
}

fn denied(out: &Ran) -> bool {
    match out {
        Err(e) => e.contains("Permission denied"),
        Ok(o) => {
            !o.status.success() && String::from_utf8_lossy(&o.stderr).contains("Permission denied")
        }
    }
}

fn set(name: &str, value: Option<&Path>) {
    // SAFETY: this binary holds one test, so no other thread reads or writes
    // the environment while it runs.
    unsafe {
        match value {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }
}

#[test]
fn a_toolchain_outside_the_home_folder_runs_in_the_sandbox() {
    let base = tempfile::tempdir().unwrap();
    let base = kgraf_core::plain_path::canonicalize(base.path()).unwrap();
    let work = base.join("work");
    std::fs::create_dir_all(&work).unwrap();
    // Neither folder is under `$HOME`, `/usr`, `/opt` or any other default.
    let rustup = base.join("data/rustup");
    let cargo = base.join("data/cargo");
    let rustc = tool(&rustup, "toolchains/stable/bin/rustc");
    let cargo_bin = tool(&cargo, "bin/cargo-fmt");
    std::fs::write(cargo.join("credentials.toml"), "token = \"secret\"\n").unwrap();
    std::fs::create_dir_all(cargo.join("registry")).unwrap();
    std::fs::write(cargo.join("registry/index"), "x").unwrap();

    // Control: with neither variable set, the folders are unreadable.
    set("RUSTUP_HOME", None);
    set("CARGO_HOME", None);
    assert!(denied(&run(&work, &rustc, &[]).0));
    assert!(denied(&run(&work, &cargo_bin, &[]).0));

    // `RUSTUP_HOME` grants the whole folder.
    set("RUSTUP_HOME", Some(&rustup));
    let (out, notes) = run(&work, &rustc, &[]);
    assert!(ran(&out), "{out:?}");
    assert!(notes.is_empty(), "{notes:?}");

    // `CARGO_HOME` grants `bin` and `registry`, not the folder.
    set("RUSTUP_HOME", None);
    set("CARGO_HOME", Some(&cargo));
    assert!(ran(&run(&work, &cargo_bin, &[]).0));
    let (out, _) = run(
        &work,
        Path::new("/bin/cat"),
        &[cargo.join("registry/index").to_str().unwrap()],
    );
    assert_eq!(
        String::from_utf8_lossy(&out.as_ref().unwrap().stdout),
        "x",
        "{out:?}"
    );
    let (out, _) = run(
        &work,
        Path::new("/bin/cat"),
        &[cargo.join("credentials.toml").to_str().unwrap()],
    );
    assert!(denied(&out), "{out:?}");

    // A relative value grants nothing and is reported.
    set("CARGO_HOME", None);
    set("RUSTUP_HOME", Some(Path::new("data/rustup")));
    let (out, notes) = run(&work, &rustc, &[]);
    assert!(denied(&out), "{out:?}");
    assert!(
        notes
            .iter()
            .any(|n| n.contains("RUSTUP_HOME") && n.contains("absolute")),
        "{notes:?}"
    );

    // The root grants nothing and is reported.
    set("RUSTUP_HOME", Some(Path::new("/")));
    let (out, notes) = run(&work, &rustc, &[]);
    assert!(denied(&out), "{out:?}");
    assert!(notes.iter().any(|n| n.contains("RUSTUP_HOME")), "{notes:?}");

    set("RUSTUP_HOME", None);
}
