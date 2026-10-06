//! A `sh -c` script of allowlisted cargo commands, with at most one `cd`
//! into the worktree, runs without approval as a build (decision 0094,
//! amended again 2026-10-02). Nothing outside the allowlist or a grant runs
//! that way, a `cd` that can leave the worktree is never covered, and the
//! user's deny and ask rules still refuse and ask inside the script.
//!
//! How to run: `cargo test -p kgraf-tools execpolicy::script_build_tests`.
//! No network, no files written; the workspace and home paths are fixed
//! strings that do not exist. Each test says what it covers in its doc
//! comment, and every failure message quotes the script that broke.
//!
//! The scripts in the first test are the shapes real workers ran: a
//! planner's acceptance `cd pricing_engine && cargo check`, and
//! `cargo test -p discount_flat` wrapped in `sh -c`.

use super::{BuildAllowlist, Decision, ExecPolicy};
use crate::approval::{ApprovalHook, HookDecision, PendingCall};
use crate::permissions::{
    AllowSet, ModeSource, PermissionControl, PermissionMode, PermissionRulesHook,
    PermissionSettings, ProcessFacts, Rule, UserRules,
};
use kgraf_core::{AgentId, TaskId};
use std::path::PathBuf;
use std::sync::Arc;

fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

fn call(argv: &[String]) -> PendingCall {
    PendingCall {
        call_id: "c".into(),
        tool: "shell".into(),
        input: serde_json::json!({ "command": argv }),
        worker: AgentId::new("w"),
        task: TaskId::new("t"),
    }
}

/// A control in `mode`, with `allow` in the Auto allow set and the user's
/// `deny` and `ask` rules.
fn control(
    mode: PermissionMode,
    allow: &[&str],
    deny: &[&str],
    ask: &[&str],
) -> Arc<PermissionControl> {
    let settings = PermissionSettings {
        mode,
        allow: AllowSet::default()
            .with_programs(allow.iter().copied())
            .expect("allow"),
        rules: UserRules {
            deny: deny.iter().map(|r| Rule::parse(r).expect("rule")).collect(),
            ask: ask.iter().map(|r| Rule::parse(r).expect("rule")).collect(),
        },
        ..PermissionSettings::default()
    };
    Arc::new(
        PermissionControl::new(settings, ModeSource::CommandLine, ProcessFacts::default())
            .expect("control"),
    )
}

/// The policy a worker gets: the default rules, the cargo allowlist plus the
/// run's configured acceptance command, and the live mode.
fn worker_policy(control: Arc<PermissionControl>) -> ExecPolicy {
    ExecPolicy::default_kgraf_policy()
        .with_build_allowlist(
            BuildAllowlist::cargo_default().with_configured("cargo build --workspace"),
        )
        .with_permissions(control)
}

async fn shell_hook_decides(policy: &ExecPolicy, argv: &[String]) -> HookDecision {
    crate::ShellApprovalHook::new(policy.clone())
        .review(&call(argv))
        .await
}

/// The acceptance scripts workers were denied now run unasked as a
/// build in every mode but Bypass (which grants every command whole, as
/// before), and the shell hook allows them. Before the fix each was asked
/// about whole, and an unattended run denied it.
#[tokio::test]
async fn a_cd_into_the_worktree_then_allowlisted_cargo_commands_is_a_build() {
    for mode in PermissionMode::ALL {
        let policy = worker_policy(control(mode, &[], &[], &[]));
        for script in [
            "cd pricing_engine && cargo check",
            "cargo test -p discount_flat",
            "cd discount_flat && cargo check && cargo test --workspace",
            "cd ./pricing_engine; cargo clippy -p pricing_engine -- -D warnings",
            "cargo build --workspace && cargo test --workspace",
        ] {
            let argv = sh(script);
            let v = policy.evaluate(&argv);
            assert_eq!(v.decision, Decision::Allow, "{mode}: {script}: {v:?}");
            if mode == PermissionMode::Bypass {
                assert!(v.granted, "{mode}: {script}: {v:?}");
                continue;
            }
            assert!(
                v.build && !v.granted && v.script_checked,
                "{mode}: {script}: {v:?}"
            );
            assert!(
                v.reason
                    .as_deref()
                    .is_some_and(|r| r.contains("on the build and test allowlist")),
                "{mode}: {script}: {v:?}"
            );
            assert_eq!(
                shell_hook_decides(&policy, &argv).await,
                HookDecision::Allow,
                "{mode}: {script}"
            );
        }
    }
}

/// Decision 0084, amended 2026-10-06: with the standard list, a script that
/// changes into a package and runs its own tests is a build in every mode
/// but Bypass, for each language; an install after the `cd` is not.
#[test]
fn a_cd_then_another_language_s_tests_is_a_build() {
    for mode in PermissionMode::ALL {
        let policy = ExecPolicy::default_kgraf_policy()
            .with_build_allowlist(BuildAllowlist::standard())
            .with_permissions(control(mode, &[], &[], &[]));
        for script in [
            "cd web && npm test",
            "cd web && npm run build && npm test -- --ci",
            "cd service && python3 -m pytest -q",
            "cd svc && go vet ./... && go test ./...",
        ] {
            let v = policy.evaluate(&sh(script));
            assert_eq!(v.decision, Decision::Allow, "{mode}: {script}: {v:?}");
            if mode != PermissionMode::Bypass {
                assert!(v.build && v.script_checked, "{mode}: {script}: {v:?}");
            }
        }
        if mode != PermissionMode::Bypass {
            let v = policy.evaluate(&sh("cd web && npm install"));
            assert!(!v.build, "{mode}: {v:?}");
            assert_ne!(v.decision, Decision::Allow, "{mode}: {v:?}");
        }
    }
}

/// No grant wider than the contract needs: a command outside the allowlist
/// and every grant is still asked about after a `cd`, a refused cargo
/// argument still refuses the script, a `cd` that can leave the worktree or
/// enter kGraf's state is never covered, and an exact allowlist entry (a
/// project command line) is not run after a `cd` unasked.
#[test]
fn a_cd_never_widens_what_a_script_may_run() {
    let policy = ExecPolicy::default_kgraf_policy()
        .with_build_allowlist(BuildAllowlist::cargo_default().with_configured("make test"))
        .with_permissions(control(PermissionMode::Manual, &[], &[], &[]));
    for (script, why) in [
        (
            "cd x && cargo run",
            "`cargo` (`cargo run`), which no grant covers;",
        ),
        ("cd x && curl -s https://example.com", "`curl`"),
        ("cd x && cargo check && python3 gen.py", "`python3`"),
        ("cd .. && cargo check", "leaves the worktree"),
        ("cd x/../.. && cargo check", "leaves the worktree"),
        ("cd /tmp && cargo check", "leaves the worktree"),
        ("cd && cargo check", "exactly one directory"),
        ("cd ~ && cargo check", "no plain directory"),
        ("cd - && cargo check", "no plain directory"),
        ("cd x* && cargo check", "no plain directory"),
        ("cd a b && cargo check", "exactly one directory"),
        ("cd .git && cargo check", "kGraf's own state"),
        ("cd .kgraf && cargo check", "kGraf's own state"),
        ("cd a && cd b && cargo check", "change directory once"),
        ("pushd a && cargo check", "`pushd`"),
        ("cd x && make test", "`make` (`make test`)"),
        ("cd x", "runs no command"),
    ] {
        let v = policy.evaluate(&sh(script));
        assert_eq!(v.decision, Decision::Prompt, "{script}: {v:?}");
        assert!(
            !v.build && !v.granted && !v.script_checked,
            "{script}: {v:?}"
        );
        let note = v.script_note.unwrap_or_default();
        assert!(note.contains(why), "{script}: {note}");
    }
    // An argument the cargo rules refuse refuses the whole script.
    let v = policy.evaluate(&sh(
        "cd x && cargo check --config build.rustc-wrapper=/bin/sh",
    ));
    assert_eq!(v.decision, Decision::Forbidden, "{v:?}");
    assert!(v.reason.unwrap_or_default().contains("--config"));
    // `make test` itself, outside a script, stays on the allowlist.
    let v = policy.evaluate(&["make".to_owned(), "test".to_owned()]);
    assert!(v.decision == Decision::Allow && v.build, "{v:?}");
}

/// A `cd` followed by a granted program is granted as the commands it holds:
/// a program in Auto's allow set after a `cd` runs granted, and a script
/// mixing it with an allowlisted cargo command is granted, not a build.
#[test]
fn a_cd_then_a_granted_program_is_granted_as_its_commands() {
    // The script the grant covers is read from where the `cd` leads; it
    // starts no program (decision 0094, amended 2026-10-06).
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("tools")).expect("mkdir");
    std::fs::write(dir.path().join("tools/gen.py"), "print(1)\n").expect("write");
    let policy =
        worker_policy(control(PermissionMode::Auto, &["python3"], &[], &[])).with_read_context(
            crate::permissions::ReadContext::in_worktree(dir.path(), None),
        );
    let v = policy.evaluate(&sh("cd tools && python3 gen.py"));
    assert!(
        v.decision == Decision::Allow && v.granted && !v.build && v.script_checked,
        "{v:?}"
    );
    let v = policy.evaluate(&sh("cd tools && python3 gen.py && cargo test -p tools"));
    assert!(
        v.decision == Decision::Allow && v.granted && !v.build,
        "{v:?}"
    );
    let reason = v.reason.unwrap_or_default();
    assert!(
        reason.contains("granted or on the build and test allowlist")
            && reason.contains("`cd` into `tools`"),
        "{reason}"
    );
    // A script that starts a program the grant does not cover is asked
    // about, and so is one kGraf cannot find where the `cd` leads.
    std::fs::write(
        dir.path().join("tools/gen.py"),
        "import subprocess\nsubprocess.run(['curl', 'x'])\n",
    )
    .expect("write");
    assert_eq!(
        policy.evaluate(&sh("cd tools && python3 gen.py")).decision,
        Decision::Prompt
    );
    assert_eq!(
        policy.evaluate(&sh("python3 gen.py")).decision,
        Decision::Prompt
    );
}

/// The user's deny and ask rules still refuse and ask about a command inside
/// `cd X && ...` that the policy now allows, in every mode: the rules hook
/// runs after the shell hook on every command of the script.
#[tokio::test]
async fn deny_and_ask_rules_still_apply_inside_a_cd_script() {
    let ws = PathBuf::from("/home/someone/proj/.kgraf/worktrees/w1");
    for mode in PermissionMode::ALL {
        let deny = control(mode, &[], &["shell(cargo test*)"], &[]);
        let ask = control(mode, &[], &[], &["shell(cargo check*)"]);
        let deny_hook = PermissionRulesHook::new(deny.clone(), Some(ws.clone()))
            .with_home(Some(PathBuf::from("/home/someone")));
        let ask_hook = PermissionRulesHook::new(ask.clone(), Some(ws.clone()))
            .with_home(Some(PathBuf::from("/home/someone")));
        let tested = sh("cd discount_flat && cargo test -p discount_flat");
        assert_eq!(
            shell_hook_decides(&worker_policy(deny.clone()), &tested).await,
            HookDecision::Allow,
            "{mode}: the command policy allows it"
        );
        match deny_hook.review(&call(&tested)).await {
            HookDecision::Deny { reason } => {
                assert!(reason.contains("shell(cargo test*)"), "{mode}: {reason}")
            }
            other => panic!("{mode}: the deny rule must refuse it: {other:?}"),
        }
        let checked = sh("cd pricing_engine && cargo check");
        assert!(
            matches!(
                ask_hook.review(&call(&checked)).await,
                HookDecision::Ask { .. }
            ),
            "{mode}: the ask rule must ask"
        );
    }
}
