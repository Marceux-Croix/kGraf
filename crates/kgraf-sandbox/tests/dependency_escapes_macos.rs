//! Attempts to escape through the links kGraf makes to a checkout's
//! installed dependencies, against the real macOS Seatbelt backend
//! (decision 0084, amended 2026-10-06).
//!
//! What it shows: a confined command cannot change the checkout's install
//! through its link in any way (write, create, delete, rename, hard link,
//! mode, times); it may replace the link in its own tree, which leaves the
//! checkout as it was; a link inside the install that points at the
//! checkout's `.git`, its `.env`, a folder whose reads are denied, or
//! another folder of the checkout is refused at its target; and the
//! activated virtual environment is the checkout's folder, so a worker that
//! swaps the link in its tree for a folder of its own does not put its own
//! programs first on `PATH`.
//!
//! How to run: `cargo test -p kgraf-sandbox --test dependency_escapes_macos`
//! on macOS. No network, no real home folder: fixtures in temporary folders.

#![cfg(target_os = "macos")]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use kgraf_sandbox::{Network, SandboxPolicy, link_dependency_trees, sandboxed_command};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

fn canonical(dir: &tempfile::TempDir) -> PathBuf {
    kgraf_core::plain_path::canonicalize(dir.path()).expect("canonical")
}

/// Run `script` with `sh -c` under `policy`, in `cwd`.
fn sh(policy: &SandboxPolicy, cwd: &Path, script: &str) -> std::process::Output {
    sandboxed_command(policy, "/bin/sh", cwd)
        .expect("command")
        .args(["-c", script])
        .output()
        .expect("ran")
}

#[test]
fn a_command_cannot_change_the_checkout_install_through_its_link() {
    let checkout = tempfile::tempdir().expect("checkout");
    let c = canonical(&checkout);
    write(&c.join("package.json"), "{}");
    write(&c.join("node_modules/dep/index.js"), "dep");
    let file = c.join("node_modules/dep/index.js");
    let before = std::fs::metadata(&file).expect("meta");
    let tree = tempfile::tempdir().expect("tree");
    let t = canonical(&tree);
    write(&t.join("package.json"), "{}");
    let links = link_dependency_trees(&t, &c);
    assert_eq!(links.reads, vec![c.join("node_modules")], "{links:?}");
    let policy = SandboxPolicy::new(vec![t.clone()], Network::Deny).with_dependency_links(&links);

    for (what, script) in [
        ("write", "echo changed > node_modules/dep/index.js"),
        ("append", "echo changed >> node_modules/dep/index.js"),
        ("create", "echo new > node_modules/dep/new.js"),
        ("mkdir", "mkdir node_modules/newpkg"),
        ("delete", "rm node_modules/dep/index.js"),
        ("rename out", "mv node_modules/dep stolen"),
        (
            "rename in place",
            "mv node_modules/dep/index.js node_modules/dep/x.js",
        ),
        (
            "hard link",
            "ln node_modules/dep/index.js hard && echo pwned >> hard",
        ),
        ("mode", "chmod 777 node_modules/dep/index.js"),
        (
            "times",
            "touch -m -t 200001010000 node_modules/dep/index.js",
        ),
        ("symlink inside", "ln -s /etc/passwd node_modules/dep/evil"),
    ] {
        let out = sh(&policy, &t, script);
        assert!(!out.status.success(), "{what} succeeded: {out:?}");
    }
    let after = std::fs::metadata(&file).expect("meta");
    assert_eq!(std::fs::read_to_string(&file).expect("read"), "dep");
    assert_eq!(after.permissions().mode(), before.permissions().mode());
    assert_eq!(after.mtime(), before.mtime());
    assert_eq!(
        after.nlink(),
        1,
        "a hard link to the checkout's file was made"
    );
    let names: Vec<_> = std::fs::read_dir(c.join("node_modules/dep"))
        .expect("list")
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(names, ["index.js"]);
    assert!(!c.join("node_modules/newpkg").exists());

    // The link itself is the tree's: replacing it is allowed, and the
    // checkout stays as it was. The next link call leaves the worker's
    // folder and grants nothing for it.
    let out = sh(
        &policy,
        &t,
        "rm node_modules && mkdir -p node_modules/own && echo own > node_modules/own/i.js",
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read_to_string(&file).expect("read"), "dep");
    let again = link_dependency_trees(&t, &c);
    assert!(again.reads.is_empty(), "{again:?}");
}

#[test]
fn links_inside_an_install_reach_nothing_the_sandbox_denies() {
    let checkout = tempfile::tempdir().expect("checkout");
    let c = canonical(&checkout);
    let state = tempfile::tempdir().expect("state");
    let s = canonical(&state);
    write(&s.join("diagnostics.key"), "FAKE");
    write(&c.join("package.json"), "{}");
    write(&c.join(".env"), "FAKE");
    write(&c.join(".git/config"), "FAKE");
    write(&c.join("packages/web/src/secret.js"), "FAKE");
    write(&c.join("node_modules/dep/index.js"), "dep");
    for (name, target) in [
        ("git-config", c.join(".git/config")),
        ("git-dir", c.join(".git")),
        ("dotenv", c.join(".env")),
        ("relative-dotenv", PathBuf::from("../../.env")),
        ("state", s.join("diagnostics.key")),
        ("sibling", c.join("packages/web/src/secret.js")),
    ] {
        std::os::unix::fs::symlink(&target, c.join("node_modules/dep").join(name)).expect("ln");
    }
    let tree = tempfile::tempdir().expect("tree");
    let t = canonical(&tree);
    write(&t.join("package.json"), "{}");
    let links = link_dependency_trees(&t, &c);
    let policy = SandboxPolicy::new(vec![t.clone()], Network::Deny)
        .with_ancestor_reads(c.clone())
        .deny_read(s.clone())
        .with_dependency_links(&links);
    let ok = sh(&policy, &t, "cat node_modules/dep/index.js");
    assert_eq!(String::from_utf8_lossy(&ok.stdout), "dep", "{ok:?}");
    for name in [
        "git-config",
        "git-dir/config",
        "dotenv",
        "relative-dotenv",
        "state",
        "sibling",
    ] {
        let out = sh(&policy, &t, &format!("cat node_modules/dep/{name}"));
        assert!(
            !out.status.success() && !String::from_utf8_lossy(&out.stdout).contains("FAKE"),
            "{name} was read: {out:?}"
        );
    }
}

/// The worker swaps the `.venv` link in its tree for a folder of its own
/// after the link was made (a parallel call can do this between the link
/// and the command's start). The command still runs the checkout's
/// environment, never the worker's programs.
#[test]
fn a_swapped_venv_link_does_not_put_the_worker_s_programs_first_on_path() {
    let checkout = tempfile::tempdir().expect("checkout");
    let c = canonical(&checkout);
    write(&c.join(".venv/pyvenv.cfg"), "home = /usr/bin\n");
    write(
        &c.join(".venv/bin/kgraf-probe"),
        "#!/bin/sh\necho checkout\n",
    );
    std::fs::set_permissions(
        c.join(".venv/bin/kgraf-probe"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    let tree = tempfile::tempdir().expect("tree");
    let t = canonical(&tree);
    let links = link_dependency_trees(&t, &c);
    let policy = SandboxPolicy::new(vec![t.clone()], Network::Deny).with_dependency_links(&links);
    // The swap, after the link and the policy.
    std::fs::remove_file(t.join(".venv")).expect("unlink");
    write(&t.join(".venv/bin/kgraf-probe"), "#!/bin/sh\necho worker\n");
    std::fs::set_permissions(
        t.join(".venv/bin/kgraf-probe"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    let out = sh(&policy, &t, "kgraf-probe; echo \"$VIRTUAL_ENV\"");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("checkout\n"), "{out:?}");
    assert!(
        text.contains(&c.join(".venv").display().to_string()),
        "{out:?}"
    );
}
