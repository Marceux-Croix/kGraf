//! Decision 0094 through the real macOS Seatbelt backend: with the shared git
//! directory writable, the files that decide which programs git runs stay
//! read-only (written, created, deleted, renamed, linked, in any letter case,
//! through a symlink, or by moving a replacement directory in), while the
//! rest of the git directory stays writable.

#![cfg(target_os = "macos")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use common::{fixture, git};
use kgraf_sandbox::{
    Network, SandboxPolicy, Workspace, WorkspaceManager, WorkspaceSpec, sandboxed_command,
};

const EPERM_TEXT: &str = "Operation not permitted";

struct Env {
    _f: common::Fixture,
    _mgr: WorkspaceManager,
    ws: Workspace,
    common: PathBuf,
    /// An include target under the worktree, which does not exist yet.
    include: PathBuf,
}

fn env() -> Env {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let ws = mgr
        .create(&WorkspaceSpec {
            name: "w",
            base: "HEAD",
            branch: None,
        })
        .unwrap();
    let common = kgraf_core::plain_path::canonicalize(ws.git_common_dir()).unwrap();
    let include = kgraf_core::plain_path::canonicalize(ws.root())
        .unwrap()
        .join("conf/extra.cfg");
    Env {
        _f: f,
        _mgr: mgr,
        ws,
        common,
        include,
    }
}

fn sh(e: &Env, frozen: bool, script: &str) -> Output {
    let mut policy =
        SandboxPolicy::for_workspace(&e.ws, Network::Deny).with_writable_root(e.common.clone());
    if frozen {
        policy = policy.with_frozen_git_config(e.common.clone(), vec![e.include.clone()]);
    }
    sandboxed_command(&policy, "/bin/sh", e.ws.root())
        .unwrap()
        .arg("-c")
        .arg(script)
        .output()
        .unwrap()
}

fn q(p: &Path) -> String {
    format!("'{}'", p.display())
}

fn assert_eperm(e: &Env, script: &str) {
    let out = sh(e, true, script);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{script}: succeeded: {out:?}");
    assert!(
        stderr.contains(EPERM_TEXT),
        "{script}: wrong failure: {stderr}"
    );
}

fn assert_ok(e: &Env, script: &str) {
    let out = sh(e, true, script);
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every way to change the shared config, the per-worktree config, a
/// worktree's `commondir`, the info attributes or an include target fails,
/// and each file is unchanged afterward.
#[test]
fn the_git_configuration_stays_read_only_in_a_writable_git_directory() {
    let e = env();
    let c = &e.common;
    let config = c.join("config");
    let admin = e.ws.admin_dir().expect("admin dir");
    let admin = kgraf_core::plain_path::canonicalize(admin).unwrap();
    let commondir = admin.join("commondir");
    let before_config = std::fs::read(&config).unwrap();
    let before_commondir = std::fs::read(&commondir).unwrap();
    let elsewhere = kgraf_core::plain_path::canonicalize(e.ws.scratch()).unwrap();
    let evil = "[merge \"evil\"]\n\tdriver = touch /tmp/never\n";
    std::fs::write(elsewhere.join("evil.cfg"), evil).unwrap();

    for script in [
        format!("printf %s {} >> {}", q(Path::new(evil)), q(&config)),
        format!("echo x > {}", q(&c.join("config.lock"))),
        format!("echo x > {}", q(&c.join("CONFIG"))),
        format!("mv {} {}", q(&elsewhere.join("evil.cfg")), q(&config)),
        format!("rm {}", q(&config)),
        format!("mv {} {}", q(&config), q(&c.join("config.old"))),
        format!("ln {} {}", q(&config), q(&elsewhere.join("hard"))),
        format!("chmod 600 {}", q(&config)),
        format!(
            "ln -s {} {} && echo x >> {}",
            q(&config),
            q(&elsewhere.join("sl")),
            q(&elsewhere.join("sl"))
        ),
        format!("echo x > {}", q(&c.join("config.worktree"))),
        format!("echo x > {}", q(&c.join("Config.Worktree"))),
        format!("echo x > {}", q(&c.join("info/attributes"))),
        format!("mv {} {}", q(&c.join("info")), q(&c.join("info.old"))),
        format!("echo x > {}", q(&commondir)),
        format!("echo x > {}", q(&admin.join("config.worktree"))),
        format!("echo x > {}", q(&admin.join("COMMONDIR.lock"))),
        format!("mv {} {}", q(&admin), q(&c.join("worktrees/moved"))),
        format!("rm -rf {}", q(&admin)),
        format!("mkdir {}", q(&c.join("worktrees/planted"))),
        format!("mv {} {}", q(&c.join("worktrees")), q(&c.join("wt.old"))),
        format!(
            "mkdir -p {} && echo x > {}",
            q(e.include.parent().unwrap()),
            q(&e.include)
        ),
    ] {
        assert_eperm(&e, &script);
    }
    assert_eq!(std::fs::read(&config).unwrap(), before_config);
    assert_eq!(std::fs::read(&commondir).unwrap(), before_commondir);
    assert!(!c.join("config.worktree").exists());
    assert!(!c.join("info/attributes").exists());
    assert!(!e.include.exists() && !e.include.parent().unwrap().exists());
    assert!(!elsewhere.join("hard").exists());
}

/// A replacement worktree admin directory built elsewhere, holding a
/// `commondir` that points at a planted repository, cannot be moved in, so a
/// git command run in the worktree still reads the real config.
#[test]
fn a_replacement_admin_directory_cannot_be_moved_in() {
    let e = env();
    let admin = kgraf_core::plain_path::canonicalize(e.ws.admin_dir().unwrap()).unwrap();
    let built = kgraf_core::plain_path::canonicalize(e.ws.scratch())
        .unwrap()
        .join("admin");
    let script = format!(
        "cp -R {a} {b} && echo /tmp/planted > {b}/commondir && mv {a} {a}.gone",
        a = q(&admin),
        b = q(&built)
    );
    assert_eperm(&e, &script);
    assert!(admin.join("commondir").is_file());
    let rel = std::fs::read_to_string(admin.join("commondir")).unwrap();
    assert!(!rel.contains("planted"));
}

/// The rest of the git directory stays writable: refs, objects, the
/// worktree's own HEAD and index, `info/exclude`, and a git commit inside the
/// sandbox; only a config write fails, with git's own message.
#[test]
fn the_rest_of_the_git_directory_stays_writable() {
    let e = env();
    let c = &e.common;
    assert_ok(
        &e,
        &format!("printf '/scratch/\\n' >> {}", q(&c.join("info/exclude"))),
    );
    assert_ok(
        &e,
        "git -c user.name=t -c user.email=t@t commit -q --allow-empty -m inside && \
         git update-ref refs/heads/made-inside HEAD && git tag inside-tag",
    );
    assert!(c.join("refs/heads/made-inside").is_file());
    let out = sh(&e, true, "git config merge.evil.driver 'touch /tmp/never'");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("config"), "{stderr}");
    let config = std::fs::read_to_string(c.join("config")).unwrap();
    assert!(!config.contains("evil"), "{config}");
}

/// Control: without the freeze, the same config write lands. The freeze, not
/// some other rule, is what stops it.
#[test]
fn without_the_freeze_the_config_write_lands() {
    let e = env();
    let out = sh(&e, false, "git config merge.evil.driver 'touch /tmp/never'");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        git(e.ws.root(), &["config", "merge.evil.driver"]),
        "touch /tmp/never"
    );
}
