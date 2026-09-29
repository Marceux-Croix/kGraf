//! A symlinked credential directory is denied at its target, against a
//! throwaway HOME holding fake credentials. One test per binary, because it
//! sets HOME before any other thread exists.

#![cfg(target_os = "macos")]

use std::path::Path;

use kgraf_sandbox::{Network, SandboxPolicy, sandboxed_command};

const FAKE: &str = "FAKE-CREDENTIAL-FOR-TEST";

#[test]
fn a_symlinked_credential_directory_is_unreadable_through_the_link_and_at_its_target() {
    let home = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let target = elsewhere.path().join("keys");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("id_ed25519"), FAKE).unwrap();
    std::os::unix::fs::symlink(&target, home.path().join(".ssh")).unwrap();
    std::fs::write(home.path().join("notes.txt"), FAKE).unwrap();
    // SAFETY: this binary has exactly one test, and it sets the variable
    // before spawning any thread.
    unsafe {
        std::env::set_var("HOME", home.path());
    }
    let policy = SandboxPolicy::new(vec![work.path().to_path_buf()], Network::Deny);
    let cat = |path: &Path| {
        sandboxed_command(&policy, "/bin/cat", work.path())
            .unwrap()
            .arg(path)
            .output()
            .unwrap()
    };
    for path in [
        home.path().join(".ssh/id_ed25519"),
        target.join("id_ed25519"),
    ] {
        let out = cat(&path);
        assert!(!out.status.success(), "{path:?} readable: {out:?}");
        assert!(!String::from_utf8_lossy(&out.stdout).contains(FAKE));
    }
    // Control: an ordinary file in the same HOME stays readable.
    let out = cat(&home.path().join("notes.txt"));
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), FAKE);
}
