//! A program given to another program as an argument is a command (decision
//! 0094, amended 2026-10-06): `find -exec PROG`, `su -c CMD`, `ssh HOST CMD`,
//! `script -c CMD`, `trap CMD SIG`, `parallel CMD :::`, the process wrappers
//! (`env`, `xargs`, `nice`, `timeout`, `nohup`, `sudo`, `exec`, `command`,
//! `time`, `watch`, `flock`, `stdbuf`, `chroot`) and `SHELL -c`. Two
//! properties, checked through the whole approval chain in every permission
//! mode:
//!
//! 1. The user's deny and ask rules judge the inner program like a top-level
//!    command. A command whose inner commands kGraf cannot read (`source
//!    FILE`, `. FILE`, `bash FILE`, `ssh HOST` with no command) is asked about
//!    in every mode while the user has a shell rule it could break.
//! 2. A grant of the outer program (the Auto allow set, a contract's grant)
//!    does not grant what it runs: every program of the command needs its own
//!    grant, and the exec wrappers and `find -exec` / `-delete` are never
//!    granted by a program grant, as in Claude Code.
//!
//! How to run: `cargo test -p kgraf-tools --test inner_programs`. No network,
//! no process started, no file written; the workspace and home directory are
//! fictional fixed paths. Set `KGRAF_PRINT_MATRIX=1` to print the case x mode
//! table the assertions check.

use std::path::PathBuf;
use std::sync::Arc;

use kgraf_core::{AgentId, TaskId};
use kgraf_tools::approval::{ApprovalHook, HookDecision, PendingCall};
use kgraf_tools::execpolicy::{Decision, ExecPolicy};
use kgraf_tools::permissions::{
    AllowSet, AskRoute, ModeSource, PermissionControl, PermissionMode, PermissionRulesHook,
    PermissionSettings, ProcessFacts, Rule, UserRules,
};
use serde_json::json;

fn call(words: &[String]) -> PendingCall {
    PendingCall {
        call_id: "c".into(),
        tool: "shell".into(),
        input: json!({ "command": words }),
        worker: AgentId::new("w"),
        task: TaskId::new("t"),
    }
}

fn rules(deny: &[&str], ask: &[&str]) -> UserRules {
    UserRules {
        deny: deny.iter().map(|r| Rule::parse(r).expect("rule")).collect(),
        ask: ask.iter().map(|r| Rule::parse(r).expect("rule")).collect(),
    }
}

/// What the whole chain does with one call: `deny`, `ask` (a hard ask from
/// the rules hook, which no mode answers), `user` (a soft ask the mode
/// routes to the user), `orch` (a soft ask the orchestrator decides) or
/// `run` (allowed without anyone being asked).
async fn outcome(
    mode: PermissionMode,
    allow: &[&str],
    contract: &[&str],
    user: UserRules,
    words: &[String],
) -> &'static str {
    let settings = PermissionSettings {
        mode,
        allow: AllowSet::default()
            .with_programs(allow.iter().copied())
            .expect("allow"),
        rules: user,
        ..PermissionSettings::default()
    };
    let control = Arc::new(
        PermissionControl::new(settings, ModeSource::CommandLine, ProcessFacts::default())
            .expect("control"),
    );
    let hook = PermissionRulesHook::new(
        control.clone(),
        Some(PathBuf::from("/home/someone/proj/.kgraf/worktrees/w1")),
    )
    .with_home(Some(PathBuf::from("/home/someone")));
    let policy = ExecPolicy::default_kgraf_policy()
        .with_permissions(control.clone())
        .with_granted_programs(contract.iter().map(|p| (*p).to_owned()));
    let verdict = policy.evaluate(words);
    let c = call(words);
    let ruled = hook.review(&c).await;
    if verdict.decision == Decision::Forbidden || matches!(ruled, HookDecision::Deny { .. }) {
        return "deny";
    }
    if matches!(ruled, HookDecision::Ask { .. }) {
        return "ask";
    }
    match verdict.decision {
        Decision::Allow => "run",
        _ => match control.route_ask("shell", &c.input) {
            AskRoute::Grant { by: "orchestrator" } => "orch",
            AskRoute::Grant { .. } => "run",
            AskRoute::User => "user",
        },
    }
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// Each case runs `curl` as an argument of another program, as an argv and
/// (the second form) inside `sh -c`. The grant column names the outer
/// programs a user would allow.
fn visible_cases() -> Vec<(&'static str, Vec<String>, &'static [&'static str])> {
    let cases: Vec<(&'static str, Vec<&'static str>, &'static [&'static str])> = vec![
        (
            "find -exec",
            vec!["find", ".", "-exec", "curl", "x", ";"],
            &["find"],
        ),
        (
            "find -execdir",
            vec!["find", ".", "-execdir", "curl", "{}", "+"],
            &["find"],
        ),
        (
            "find -ok",
            vec!["find", ".", "-ok", "curl", "x", ";"],
            &["find"],
        ),
        ("su -c", vec!["su", "-c", "curl x"], &["su"]),
        ("su USER -c", vec!["su", "bob", "-c", "ls; curl x"], &["su"]),
        (
            "runuser -u",
            vec!["runuser", "-u", "bob", "--", "curl", "x"],
            &["runuser"],
        ),
        ("ssh HOST CMD", vec!["ssh", "host", "curl", "x"], &["ssh"]),
        (
            "ssh -p N HOST 'CMD'",
            vec!["ssh", "-p", "22", "host", "ls && curl x"],
            &["ssh"],
        ),
        (
            "script -c",
            vec!["script", "-c", "curl x", "/dev/null"],
            &["script"],
        ),
        (
            "script -qc",
            vec!["script", "-qc", "curl x", "/dev/null"],
            &["script"],
        ),
        (
            "script FILE CMD (BSD)",
            vec!["script", "-q", "/dev/null", "curl", "x"],
            &["script"],
        ),
        ("trap", vec!["trap", "curl x", "EXIT"], &["trap"]),
        (
            "parallel CMD :::",
            vec!["parallel", "curl", ":::", "a", "b"],
            &["parallel"],
        ),
        (
            "parallel 'CMD {}' :::",
            vec!["parallel", "-j", "4", "curl {}", ":::", "a"],
            &["parallel"],
        ),
        (
            "parallel ::: CMDS",
            vec!["parallel", ":::", "ls", "curl x"],
            &["parallel"],
        ),
        ("xargs", vec!["xargs", "curl"], &["xargs"]),
        ("xargs -n1", vec!["xargs", "-n1", "curl"], &["xargs"]),
        (
            "xargs sh -c",
            vec!["xargs", "sh", "-c", "curl x"],
            &["xargs", "sh"],
        ),
        ("env", vec!["env", "curl", "x"], &["env"]),
        ("env -S", vec!["env", "-S", "curl x"], &["env"]),
        ("nice", vec!["nice", "-n", "5", "curl", "x"], &["nice"]),
        ("timeout", vec!["timeout", "5", "curl", "x"], &["timeout"]),
        ("nohup", vec!["nohup", "curl", "x"], &["nohup"]),
        ("sudo", vec!["sudo", "curl", "x"], &["sudo"]),
        ("exec", vec!["exec", "curl", "x"], &["exec"]),
        ("command", vec!["command", "curl", "x"], &["command"]),
        ("time", vec!["time", "curl", "x"], &["time"]),
        ("watch", vec!["watch", "curl", "x"], &["watch"]),
        (
            "watch 'CMDS'",
            vec!["watch", "-n", "1", "ls; curl x"],
            &["watch"],
        ),
        (
            "flock FILE CMD",
            vec!["flock", "/tmp/l", "curl", "x"],
            &["flock"],
        ),
        (
            "flock FILE -c",
            vec!["flock", "/tmp/l", "-c", "curl x"],
            &["flock"],
        ),
        ("stdbuf", vec!["stdbuf", "-oL", "curl", "x"], &["stdbuf"]),
        ("chroot", vec!["chroot", "/", "curl", "x"], &["chroot"]),
        ("bash -c", vec!["bash", "-c", "curl x"], &["bash"]),
        ("zsh -c", vec!["zsh", "-c", "curl x"], &["zsh"]),
        ("bash -lc", vec!["bash", "-lc", "curl x"], &["bash"]),
        (
            "bash -o OPT -c",
            vec!["bash", "-o", "pipefail", "-c", "curl x"],
            &["bash"],
        ),
        (
            "sh -c find -exec",
            vec!["sh", "-c", "find . -exec curl x \\;"],
            &["sh", "find"],
        ),
        (
            "direnv exec",
            vec!["direnv", "exec", ".", "curl", "x"],
            &["direnv"],
        ),
        ("uv run", vec!["uv", "run", "curl", "x"], &["uv"]),
        (
            "ssh -o ProxyCommand",
            vec!["ssh", "-o", "ProxyCommand=curl x", "host", "ls"],
            &["ssh"],
        ),
        ("rg --pre", vec!["rg", "--pre", "curl", "pattern"], &["rg"]),
        (
            "tar --to-command",
            vec!["tar", "-xf", "a.tar", "--to-command=curl x"],
            &["tar"],
        ),
        (
            "rsync -e",
            vec!["rsync", "-e", "curl x", "a", "b:c"],
            &["rsync"],
        ),
    ];
    let mut out = Vec::new();
    for (label, words, grant) in cases {
        let line = shlex::try_join(words.iter().copied()).expect("join");
        out.push((label, argv(&words), grant));
        out.push((label, argv(&["sh", "-c", &line]), grant));
    }
    out
}

/// Each case runs commands kGraf cannot read.
fn opaque_cases() -> Vec<(&'static str, Vec<String>, &'static [&'static str])> {
    vec![
        ("source FILE", argv(&["source", "./env.sh"]), &["source"]),
        (". FILE", argv(&[".", "./env.sh"]), &["."]),
        (
            "sh -c '. FILE'",
            argv(&["sh", "-c", ". ./env.sh && ls"]),
            &["sh", ".", "ls"],
        ),
        ("bash FILE", argv(&["bash", "run.sh"]), &["bash"]),
        (
            "bash reading input",
            argv(&["sh", "-c", "cat run.sh | bash"]),
            &["sh", "cat", "bash"],
        ),
        ("ssh HOST", argv(&["ssh", "host"]), &["ssh"]),
        ("su (no command)", argv(&["su", "bob"]), &["su"]),
        (
            "script (no command)",
            argv(&["script", "out.log"]),
            &["script"],
        ),
        (
            "parallel :::: FILE",
            argv(&["parallel", "::::", "cmds.txt"]),
            &["parallel"],
        ),
    ]
}

fn print_matrix() -> bool {
    std::env::var_os("KGRAF_PRINT_MATRIX").is_some()
}

/// Property 1: a deny rule on the inner program refuses the call in every
/// mode.
#[tokio::test]
async fn a_deny_rule_refuses_the_inner_program_in_every_mode() {
    let mut failures = Vec::new();
    for (label, words, _) in visible_cases() {
        let mut row = Vec::new();
        for mode in PermissionMode::ALL {
            let got = outcome(mode, &[], &[], rules(&["shell(curl*)"], &[]), &words).await;
            row.push(got);
            if got != "deny" {
                failures.push(format!("{mode}: {words:?}: {got}"));
            }
            // An ask rule asks in every mode.
            let asked = outcome(mode, &[], &[], rules(&[], &["shell(curl*)"]), &words).await;
            if asked != "ask" {
                failures.push(format!("ask rule, {mode}: {words:?}: {asked}"));
            }
        }
        if print_matrix() {
            println!("RULE | {label} | {words:?} | {}", row.join(" | "));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Property 1, no over-reach: an inner program no rule names is not refused,
/// and a program named only as data is not read as a command.
#[tokio::test]
async fn a_deny_rule_does_not_refuse_another_inner_program_or_data() {
    for mode in PermissionMode::ALL {
        for words in [
            argv(&["find", ".", "-name", "curl"]),
            argv(&["find", ".", "-exec", "grep", "curl", "{}", ";"]),
            argv(&["ssh", "host", "ls"]),
            argv(&["su", "-c", "echo curl"]),
            argv(&["trap", "echo curl", "EXIT"]),
            argv(&["xargs", "grep", "curl"]),
            argv(&["bash", "--version"]),
        ] {
            let got = outcome(mode, &[], &[], rules(&["shell(curl*)"], &[]), &words).await;
            assert!(got != "deny" && got != "ask", "{mode}: {words:?}: {got}");
        }
    }
}

/// Property 1 for commands kGraf cannot read: with a shell rule set, asked
/// about in every mode (Bypass too); with no rule, decided by the mode as
/// before, and never granted by a program grant.
#[tokio::test]
async fn an_unreadable_inner_command_is_asked_about_and_never_granted() {
    let mut failures = Vec::new();
    for (label, words, grant) in opaque_cases() {
        let mut row = Vec::new();
        for mode in PermissionMode::ALL {
            for user in [rules(&["shell(curl*)"], &[]), rules(&[], &["shell(curl*)"])] {
                let got = outcome(mode, &[], &[], user, &words).await;
                row.push(got);
                if got != "ask" {
                    failures.push(format!("{mode}: {words:?}: {got}"));
                }
            }
            let granted = outcome(mode, grant, grant, UserRules::default(), &words).await;
            row.push(granted);
            let want = match mode {
                PermissionMode::Bypass => "run",
                PermissionMode::OrchestratorDecides => "orch",
                _ => "user",
            };
            if granted != want {
                failures.push(format!("grant {grant:?}, {mode}: {words:?}: {granted}"));
            }
        }
        if print_matrix() {
            println!("OPAQUE | {label} | {words:?} | {}", row.join(" | "));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Property 2: a grant of the outer program, in the Auto allow set and to the
/// contract, does not grant `curl`. Bypass grants every command, and the
/// orchestrator decides its own asks.
#[tokio::test]
async fn a_grant_of_the_outer_program_does_not_grant_the_inner_one() {
    let mut failures = Vec::new();
    for (label, words, grant) in visible_cases() {
        let mut row = Vec::new();
        for mode in PermissionMode::ALL {
            let got = outcome(mode, grant, grant, UserRules::default(), &words).await;
            row.push(got);
            let want = match mode {
                PermissionMode::Bypass => "run",
                PermissionMode::OrchestratorDecides => "orch",
                _ => "user",
            };
            if got != want {
                failures.push(format!("grant {grant:?}, {mode}: {words:?}: {got}"));
            }
        }
        if print_matrix() {
            println!("GRANT | {label} | {words:?} | {}", row.join(" | "));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Property 2, the other side: when every program of the command is granted,
/// the command runs; the exec wrappers and `find -exec` / `-delete` never do.
#[tokio::test]
async fn a_command_runs_when_every_program_it_runs_is_granted() {
    for (mode, allow, contract) in [
        (PermissionMode::Auto, &["env", "curl"][..], &[][..]),
        (PermissionMode::Manual, &[][..], &["env", "curl"][..]),
    ] {
        for words in [
            argv(&["env", "curl", "x"]),
            argv(&["env", "A=1", "curl", "x"]),
            argv(&["curl", "x"]),
        ] {
            assert_eq!(
                outcome(mode, allow, contract, UserRules::default(), &words).await,
                "run",
                "{mode}: {words:?}"
            );
        }
    }
    let both = [
        "find", "curl", "watch", "flock", "setsid", "ionice", "sh", "su", "uv", "pytest",
    ];
    for words in [
        argv(&["find", ".", "-name", "x"]),
        argv(&["su", "-c", "curl x"]),
        argv(&["sh", "-c", "curl x"]),
        // A script's command that starts another is granted command by
        // command when every program is granted.
        argv(&["sh", "-c", "uv run pytest && su -c 'curl x'"]),
    ] {
        assert_eq!(
            outcome(
                PermissionMode::Auto,
                &both,
                &[],
                UserRules::default(),
                &words
            )
            .await,
            "run",
            "{words:?}"
        );
    }
    for words in [
        argv(&["find", ".", "-exec", "curl", "x", ";"]),
        argv(&["find", ".", "-delete"]),
        argv(&["watch", "curl", "x"]),
        argv(&["flock", "/tmp/l", "curl", "x"]),
        argv(&["setsid", "curl", "x"]),
        argv(&["ionice", "-c", "3", "curl", "x"]),
        argv(&["sh", "-c", "find . -exec curl x \\;"]),
        argv(&["sh", "-c", "uv run python3 x.py"]),
        argv(&["sh", "-c", "ls && watch curl x"]),
    ] {
        assert_eq!(
            outcome(
                PermissionMode::Auto,
                &both,
                &both,
                UserRules::default(),
                &words
            )
            .await,
            "user",
            "{words:?}"
        );
    }
}

/// The mode's command policy's `forbid` rules see a command inside a script
/// or behind a program that runs it, in Bypass too.
#[tokio::test]
async fn a_forbid_rule_sees_the_inner_command_in_bypass() {
    for words in [
        argv(&["sh", "-c", "git push --force origin"]),
        argv(&["ssh", "host", "git", "push", "--force"]),
        argv(&["find", ".", "-exec", "git", "push", "--force", ";"]),
        argv(&["su", "-c", "git push --force"]),
    ] {
        assert_eq!(
            outcome(
                PermissionMode::Bypass,
                &[],
                &[],
                UserRules::default(),
                &words
            )
            .await,
            "deny",
            "{words:?}"
        );
    }
}
