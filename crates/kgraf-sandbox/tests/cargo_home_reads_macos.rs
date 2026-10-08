//! A `CARGO_HOME` outside the home folder keeps its credentials and config
//! unreadable, and its `bin` folder readable. One test in its own binary: it
//! sets process environment variables.

#![cfg(target_os = "macos")]

use std::path::Path;

use kgraf_sandbox::{Network, SandboxPolicy, sandboxed_command};

fn cat(work: &Path, file: &Path) -> std::process::Output {
    let policy = SandboxPolicy::new(vec![work.to_path_buf()], Network::Deny);
    sandboxed_command(&policy, Path::new("/bin/cat"), work)
        .unwrap()
        .arg(file)
        .output()
        .unwrap()
}

#[test]
fn a_cargo_home_outside_the_home_folder_keeps_its_secrets() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cargo = tempfile::tempdir().unwrap();
    let cargo_dir = kgraf_core::plain_path::canonicalize(cargo.path()).unwrap();
    std::fs::create_dir_all(cargo_dir.join("registry")).unwrap();
    std::fs::write(cargo_dir.join("registry/index"), "index").unwrap();
    for secret in ["credentials", "credentials.toml", "config", "config.toml"] {
        std::fs::write(cargo_dir.join(secret), "FAKE-CREDENTIAL-FOR-TEST").unwrap();
    }
    // SAFETY: this binary holds one test, so no other thread reads or writes
    // the environment while it runs.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("CARGO_HOME", &cargo_dir);
    }
    for secret in ["credentials", "credentials.toml", "config", "config.toml"] {
        let out = cat(work.path(), &cargo_dir.join(secret));
        assert!(!out.status.success(), "{secret} was read: {out:?}");
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("FAKE-CREDENTIAL"),
            "{secret}"
        );
    }
    let out = cat(work.path(), &cargo_dir.join("registry/index"));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "index", "{out:?}");
}
