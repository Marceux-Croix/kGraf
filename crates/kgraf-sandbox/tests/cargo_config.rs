//! The user's cargo config reaches a sandboxed cargo without its secrets: a
//! source replacement in a stand-in user cargo home (a temp directory, never
//! the real `~/.cargo`) resolves a dependency that exists nowhere else, and
//! the config the command can read holds no token.

#![cfg(any(target_os = "macos", target_os = "linux"))]

mod common;

use std::path::Path;
use std::process::Output;

use common::fixture;
use kgraf_sandbox::sandboxed_command;
use kgraf_sandbox::{Network, SandboxPolicy, Workspace, WorkspaceManager, WorkspaceSpec};

/// A user cargo home whose config replaces crates.io with a vendored
/// directory holding `vdep`, and carries a registry token.
fn user_cargo_home(dir: &Path) {
    let vdep = dir.join("vendor/vdep");
    std::fs::create_dir_all(vdep.join("src")).unwrap();
    std::fs::write(
        vdep.join("Cargo.toml"),
        "[package]\nname = \"vdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(vdep.join("src/lib.rs"), "pub fn v() -> u32 { 7 }\n").unwrap();
    std::fs::write(
        vdep.join(".cargo-checksum.json"),
        "{\"files\":{},\"package\":null}",
    )
    .unwrap();
    let home = dir.join("cargo");
    std::fs::create_dir_all(&home).unwrap();
    // A relative directory: cargo resolves it against the cargo home's
    // parent, and so must the copy the sandbox gets.
    std::fs::write(
        home.join("config.toml"),
        "[registry]\ntoken = \"cio-secret-4711\"\n\
         [source.crates-io]\nreplace-with = \"vend\"\n\
         [source.vend]\ndirectory = \"vendor\"\n",
    )
    .unwrap();
}

fn project(ws: &Workspace) {
    let root = ws.root();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nvdep = \"0.1\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}\", vdep::v()); }\n",
    )
    .unwrap();
}

fn cargo_run(ws: &Workspace, policy: &SandboxPolicy) -> Output {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    sandboxed_command(policy, &cargo, ws.root())
        .unwrap()
        .args(["run", "--quiet", "--offline"])
        .env("CARGO_TARGET_DIR", ws.scratch().join("target"))
        .output()
        .unwrap()
}

#[test]
fn a_source_replacement_in_the_user_config_applies_without_its_token() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let ws = mgr
        .create(&WorkspaceSpec {
            name: "w",
            base: "HEAD",
            branch: None,
        })
        .unwrap();
    project(&ws);
    let user = tempfile::tempdir().unwrap();
    let user = user.path().canonicalize().unwrap();
    user_cargo_home(&user);
    let policy = |home: &Path| {
        SandboxPolicy::for_workspace(&ws, Network::Deny)
            // Linux reads are an allowlist; the vendored crates must be readable.
            .with_readable_root(user.join("vendor"))
            .with_user_cargo_home(home.to_path_buf())
    };

    // Control: a cargo home without the config cannot find `vdep`.
    let bare = user.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    let out = cargo_run(&ws, &policy(&bare));
    assert!(
        !out.status.success(),
        "resolved without the config: {out:?}"
    );

    let out = cargo_run(&ws, &policy(&user.join("cargo")));
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "7");

    // What the command can read as its cargo config holds no token.
    let seen = sandboxed_command(&policy(&user.join("cargo")), "/bin/sh", ws.root())
        .unwrap()
        .args(["-c", "cat \"$CARGO_HOME/config.toml\""])
        .output()
        .unwrap();
    assert!(seen.status.success(), "{seen:?}");
    let text = String::from_utf8_lossy(&seen.stdout);
    assert!(text.contains("replace-with"), "{text}");
    assert!(!text.contains("cio-secret-4711"), "{text}");
}
