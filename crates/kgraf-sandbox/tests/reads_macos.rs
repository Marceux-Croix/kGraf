//! Credential-read denial and environment scrubbing, against a throwaway
//! HOME holding fake credentials. One test per binary, because it sets
//! process environment variables before any other thread exists.

#![cfg(target_os = "macos")]

use std::path::Path;

use kgraf_sandbox::{Network, SandboxPolicy, sandboxed_command};

const FAKE: &str = "FAKE-CREDENTIAL-FOR-TEST";

#[test]
fn credential_paths_are_unreadable_and_secrets_are_scrubbed() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let denied = [
        ".ssh/id_ed25519",
        ".aws/credentials",
        ".gnupg/private-keys-v1.d/k.key",
        ".config/gh/hosts.yml",
        ".netrc",
        ".docker/config.json",
        "Library/Keychains/login.keychain-db",
        "Library/Application Support/Google/Chrome/Default/Cookies",
        "Library/Application Support/Firefox/Profiles/p/logins.json",
        "Library/Application Support/BraveSoftware/Brave-Browser/Default/Login Data",
        "Library/Cookies/Cookies.binarycookies",
        "Library/Messages/chat.db",
        ".zsh_history",
        ".bash_history",
        ".cargo/credentials.toml",
        ".cargo/credentials",
        ".npmrc",
        "Library/Application Support/kGraf/diagnostics.key",
        "Library/Caches/kGraf/locks/oauth.lock",
        ".claude.json",
        // A sibling of the configuration file above, not the file itself: an
        // editor or the agent's own backup step leaves these next to it, and
        // they can hold the same secrets.
        ".claude.json.backup",
        ".claude.json.tmp.482913",
        ".claude/backups/2026-09-28.json",
    ];
    for rel in denied.iter().chain(["notes.txt"].iter()) {
        let path = home.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, FAKE).unwrap();
    }
    // SAFETY: this binary has exactly one test, and it sets the variables
    // before spawning any thread.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("KGRAF_FAKE_API_TOKEN", "fake-token");
        std::env::set_var("KGRAF_FAKE_PASSWORD", "fake-password");
        std::env::set_var("KGRAF_PASSED_SECRET", "passed-on-purpose");
        std::env::set_var("SSH_AUTH_SOCK", "/tmp/fake-agent.sock");
    }

    let policy = SandboxPolicy::new(vec![work.path().to_path_buf()], Network::Deny)
        .pass_env("KGRAF_PASSED_SECRET");
    let cat = |path: &Path| {
        sandboxed_command(&policy, "/bin/cat", work.path())
            .unwrap()
            .arg(path)
            .output()
            .unwrap()
    };

    for rel in denied {
        let out = cat(&home.path().join(rel));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{rel} readable");
        assert!(
            stderr.contains("Operation not permitted"),
            "{rel}: {stderr}"
        );
        assert!(!String::from_utf8_lossy(&out.stdout).contains(FAKE));
    }
    // Another spelling of kGraf's data directory names the same directory
    // on a case-insensitive volume, and must be denied too.
    let other_case = home
        .path()
        .join("Library/Application Support/KgRaF/diagnostics.key");
    if other_case.exists() {
        let out = cat(&other_case);
        assert!(!out.status.success(), "other spelling readable");
        assert!(!String::from_utf8_lossy(&out.stdout).contains(FAKE));
    }
    // Control: an ordinary file in the same HOME stays readable.
    let out = cat(&home.path().join("notes.txt"));
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), FAKE);

    let out = sandboxed_command(&policy, "/usr/bin/env", work.path())
        .unwrap()
        .output()
        .unwrap();
    let env = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    for gone in [
        "KGRAF_FAKE_API_TOKEN=",
        "KGRAF_FAKE_PASSWORD=",
        "SSH_AUTH_SOCK=",
    ] {
        assert!(!env.contains(gone), "{gone} leaked");
    }
    assert!(env.contains("KGRAF_PASSED_SECRET=passed-on-purpose"));
    assert!(env.contains("PATH="), "scrub removed ordinary variables");
}
