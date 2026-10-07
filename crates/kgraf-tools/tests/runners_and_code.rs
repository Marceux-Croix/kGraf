//! What a command runs through a runner, a Windows shell, a git setting, a
//! variable, a build-tool recipe or interpreter code is a command (decision
//! 0094, amended 2026-10-06). Three properties, checked through the whole
//! approval chain (the rules hook, the command policy and the ask routing)
//! in every permission mode, with the files read from a real worktree:
//!
//! 1. The user's deny and ask rules judge the inner program like a top-level
//!    command: `deny = ["shell(curl*)"]` refuses `npx -c 'curl x'`, `docker
//!    exec web curl x`, `cmd /c curl x`, `git -c core.pager='curl x' log`,
//!    `make deploy` whose recipe runs curl, and `python3 x.py` whose code
//!    calls `os.system("curl x")`, in every mode, Bypass included.
//! 2. What kGraf cannot read (`docker run IMAGE` with no command, `$CMD`, a
//!    missing Makefile, `os.system(cmd)`) asks in every mode while a shell
//!    rule with a pattern exists, and no program grant covers it.
//! 3. A grant of the runner, the shell, git, the build tool or the
//!    interpreter does not grant what it runs; ordinary commands with no
//!    rules (`cargo test`, `pytest`, `npm test` of a plain script) are
//!    decided as before.
//!
//! How to run: `cargo test -p kgraf-tools --test runners_and_code`. No
//! network and no process started; each test writes its worktree in a
//! temporary directory and the home directory is a folder inside it. Set
//! `KGRAF_PRINT_MATRIX=1` to print the case x mode table the assertions
//! check.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use kgraf_core::{AgentId, TaskId};
use kgraf_tools::approval::{ApprovalHook, HookDecision, PendingCall};
use kgraf_tools::execpolicy::{Decision, ExecPolicy};
use kgraf_tools::permissions::{
    AllowSet, AskRoute, ModeSource, PermissionControl, PermissionMode, PermissionRulesHook,
    PermissionSettings, ProcessFacts, ReadContext, Rule, UserRules,
};
use serde_json::json;

/// The worktree every case runs in: recipe files, scripts and git's
/// configuration, each running `curl x` where a case expects it.
fn worktree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("dir");
    let files: &[(&str, &str)] = &[
        (
            "Makefile",
            "all: build\n\nbuild:\n\tcc -o app main.c\n\ndeploy: build\n\tcurl -X POST x\n",
        ),
        (
            "package.json",
            r#"{"scripts":{"test":"jest","deploy":"curl -X POST x","pretest":"node setup.js"}}"#,
        ),
        (
            "justfile",
            "build:\n    cargo build\n\ndeploy: build\n    curl -X POST x\n",
        ),
        (
            ".cargo/config.toml",
            "[target.'cfg(all())']\nrunner = \"curl x\"\n",
        ),
        ("x.py", "import subprocess\nsubprocess.run(['curl', 'x'])\n"),
        ("clean.py", "import json\nprint(json.dumps(1))\n"),
        ("dyn.py", "import os, sys\nos.system(sys.argv[1])\n"),
        ("x.js", "require('child_process').execSync('curl x')\n"),
        ("x.pl", "system('curl x');\n"),
        ("x.rb", "system('curl', 'x')\n"),
        (
            "run.py",
            "#!/usr/bin/env python3\nimport os\nos.system('curl x')\n",
        ),
        ("setup.js", "console.log(1)\n"),
        (".git/config", "[alias]\n\tevil = !curl x\n\tst = status\n"),
        ("empty/README", "no makefile here\n"),
        ("bang/Makefile", "PROG != echo curl\nall:\n\t$(PROG) x\n"),
    ];
    for (name, text) in files {
        let p = dir.path().join(name);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(&p, text).expect("write");
    }
    std::fs::create_dir_all(dir.path().join("home")).expect("home");
    dir
}

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

/// What the whole chain does with one call in the worktree `root`: `deny`,
/// `ask` (a hard ask from the rules hook, which no mode answers), `user` (a
/// soft ask the mode routes to the user), `orch` (a soft ask the
/// orchestrator decides) or `run` (allowed without anyone being asked).
async fn outcome(
    root: &Path,
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
    let home = root.join("home");
    let hook = PermissionRulesHook::new(control.clone(), Some(root.to_path_buf()))
        .with_home(Some(home.clone()));
    let policy = ExecPolicy::default_kgraf_policy()
        .with_permissions(control.clone())
        .with_granted_programs(contract.iter().map(|p| (*p).to_owned()))
        .with_read_context(ReadContext::in_worktree(root, Some(&home)));
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

fn encoded(script: &str) -> String {
    let bytes: Vec<u8> = script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Each case runs `curl` (gap, label, argv, the programs a user would
/// allow for the outer command). Each is also run inside `sh -c`.
fn visible_cases() -> Vec<(&'static str, &'static str, Vec<String>, Vec<&'static str>)> {
    let ps = encoded("curl x");
    let cases: Vec<(&str, &str, Vec<String>, Vec<&str>)> = vec![
        ("1", "npx -c", argv(&["npx", "-c", "curl x"]), vec!["npx"]),
        (
            "1",
            "npx -p PKG CMD",
            argv(&["npx", "-p", "pkg", "curl", "x"]),
            vec!["npx"],
        ),
        (
            "1",
            "npm exec",
            argv(&["npm", "exec", "--", "curl", "x"]),
            vec!["npm"],
        ),
        (
            "1",
            "pnpm dlx",
            argv(&["pnpm", "dlx", "curl", "x"]),
            vec!["pnpm"],
        ),
        (
            "1",
            "yarn dlx",
            argv(&["yarn", "dlx", "curl", "x"]),
            vec!["yarn"],
        ),
        ("1", "bunx", argv(&["bunx", "curl", "x"]), vec!["bunx"]),
        (
            "1",
            "docker exec",
            argv(&["docker", "exec", "-it", "web", "curl", "x"]),
            vec!["docker"],
        ),
        (
            "1",
            "docker run",
            argv(&["docker", "run", "--rm", "alpine", "curl", "x"]),
            vec!["docker"],
        ),
        (
            "1",
            "podman exec",
            argv(&["podman", "exec", "web", "curl", "x"]),
            vec!["podman"],
        ),
        (
            "1",
            "kubectl exec --",
            argv(&["kubectl", "exec", "pod", "--", "curl", "x"]),
            vec!["kubectl"],
        ),
        ("2", "cmd /c", argv(&["cmd", "/c", "curl x"]), vec!["cmd"]),
        (
            "2",
            "cmd /k with &",
            argv(&["cmd", "/k", "dir & curl x"]),
            vec!["cmd", "dir"],
        ),
        (
            "2",
            "powershell -Command",
            argv(&["powershell", "-Command", "curl x"]),
            vec!["powershell"],
        ),
        (
            "2",
            "pwsh -c",
            argv(&["pwsh", "-NoProfile", "-c", "Get-Item a; curl x"]),
            vec!["pwsh", "Get-Item"],
        ),
        (
            "2",
            "powershell -EncodedCommand",
            argv(&["powershell", "-EncodedCommand", &ps]),
            vec!["powershell"],
        ),
        (
            "3",
            "git -c core.sshCommand",
            argv(&["git", "-c", "core.sshCommand=curl x", "fetch"]),
            vec!["git"],
        ),
        (
            "3",
            "git -c core.pager",
            argv(&["git", "-c", "core.pager=curl x", "log"]),
            vec!["git"],
        ),
        (
            "3",
            "git -c core.editor",
            argv(&["git", "-c", "core.editor=curl x", "commit"]),
            vec!["git"],
        ),
        (
            "3",
            "git -c core.fsmonitor",
            argv(&["git", "-c", "core.fsmonitor=curl x", "status"]),
            vec!["git"],
        ),
        (
            "3",
            "git -c credential.helper",
            argv(&["git", "-c", "credential.helper=!curl x", "fetch"]),
            vec!["git"],
        ),
        (
            "3",
            "git -c alias.X=!CMD",
            argv(&["git", "-c", "alias.x=!curl x", "x"]),
            vec!["git"],
        ),
        (
            "3",
            "git -c diff.external",
            argv(&["git", "-c", "diff.external=curl x", "diff"]),
            vec!["git"],
        ),
        (
            "3",
            "git -c gpg.program",
            argv(&["git", "-c", "gpg.program=curl", "commit"]),
            vec!["git"],
        ),
        (
            "3",
            "GIT_SSH_COMMAND=",
            argv(&["env", "GIT_SSH_COMMAND=curl x", "git", "fetch"]),
            vec!["env", "git"],
        ),
        (
            "3",
            "GIT_EXTERNAL_DIFF=",
            argv(&["env", "GIT_EXTERNAL_DIFF=curl x", "git", "diff"]),
            vec!["env", "git"],
        ),
        (
            "3",
            "GIT_PAGER=",
            argv(&["env", "GIT_PAGER=curl x", "git", "log"]),
            vec!["env", "git"],
        ),
        (
            "3",
            "GIT_EDITOR=",
            argv(&["env", "GIT_EDITOR=curl x", "git", "commit"]),
            vec!["env", "git"],
        ),
        ("3", "git ALIAS (!)", argv(&["git", "evil"]), vec!["git"]),
        (
            "3",
            "git config alias",
            argv(&["git", "config", "alias.y", "!curl x"]),
            vec!["git"],
        ),
        (
            "3",
            "export GIT_SSH_COMMAND=; git",
            argv(&["sh", "-c", "export GIT_SSH_COMMAND='curl x'; git fetch"]),
            vec!["sh", "export", "git"],
        ),
        (
            "6a",
            "make TARGET",
            argv(&["make", "deploy"]),
            vec!["make", "cc"],
        ),
        (
            "6a",
            "npm run X",
            argv(&["npm", "run", "deploy"]),
            vec!["npm"],
        ),
        (
            "6a",
            "pnpm run X",
            argv(&["pnpm", "run", "deploy"]),
            vec!["pnpm"],
        ),
        ("6a", "yarn X", argv(&["yarn", "deploy"]), vec!["yarn"]),
        (
            "6a",
            "bun run X",
            argv(&["bun", "run", "deploy"]),
            vec!["bun"],
        ),
        (
            "6a",
            "just X",
            argv(&["just", "deploy"]),
            vec!["just", "cargo"],
        ),
        (
            "6a",
            "cargo run (runner)",
            argv(&["cargo", "run"]),
            vec!["cargo"],
        ),
        (
            "6b",
            "python3 -c",
            argv(&["python3", "-c", "import os; os.system('curl x')"]),
            vec!["python3"],
        ),
        (
            "6b",
            "node -e",
            argv(&["node", "-e", "require('child_process').execSync('curl x')"]),
            vec!["node"],
        ),
        (
            "6b",
            "perl -e",
            argv(&["perl", "-e", "system('curl x')"]),
            vec!["perl"],
        ),
        (
            "6b",
            "ruby -e",
            argv(&["ruby", "-e", "system('curl x')"]),
            vec!["ruby"],
        ),
        (
            "6b",
            "php -r",
            argv(&["php", "-r", "system('curl x');"]),
            vec!["php"],
        ),
        (
            "6b",
            "awk system()",
            argv(&["awk", "BEGIN { system(\"curl x\") }"]),
            vec!["awk"],
        ),
        (
            "6b",
            "deno eval",
            argv(&["deno", "eval", "new Deno.Command('curl', { args: ['x'] })"]),
            vec!["deno"],
        ),
        (
            "6b",
            "osascript -e",
            argv(&["osascript", "-e", "do shell script \"curl x\""]),
            vec!["osascript"],
        ),
        (
            "6c",
            "python3 FILE",
            argv(&["python3", "x.py"]),
            vec!["python3"],
        ),
        (
            "6c",
            "uv run FILE",
            argv(&["uv", "run", "x.py"]),
            vec!["uv", "python3"],
        ),
        ("6c", "node FILE", argv(&["node", "x.js"]), vec!["node"]),
        ("6c", "perl FILE", argv(&["perl", "x.pl"]), vec!["perl"]),
        ("6c", "ruby FILE", argv(&["ruby", "x.rb"]), vec!["ruby"]),
        (
            "6c",
            "./FILE (#!)",
            argv(&["./run.py"]),
            vec!["./run.py", "/usr/bin/env", "python3"],
        ),
    ];
    let mut out = Vec::new();
    for (gap, label, words, grant) in cases {
        let line = shlex::try_join(words.iter().map(String::as_str)).expect("join");
        out.push((gap, label, words.clone(), grant.clone()));
        let mut in_sh = grant.clone();
        in_sh.push("sh");
        out.push((gap, label, argv(&["sh", "-c", &line]), in_sh));
    }
    out
}

/// Each case runs a command kGraf cannot read (gap, label, argv, the
/// programs a user would allow).
fn opaque_cases() -> Vec<(&'static str, &'static str, Vec<String>, Vec<&'static str>)> {
    vec![
        (
            "1",
            "docker run IMAGE (no command)",
            argv(&["docker", "run", "alpine"]),
            vec!["docker"],
        ),
        (
            "1",
            "npx URL package",
            argv(&["npx", "github:someone/tool"]),
            vec!["npx"],
        ),
        (
            "1",
            "kubectl exec without --",
            argv(&["kubectl", "exec", "pod", "curl"]),
            vec!["kubectl"],
        ),
        (
            "2",
            "cmd /c %VAR%",
            argv(&["cmd", "/c", "%X% a"]),
            vec!["cmd"],
        ),
        (
            "2",
            "powershell iex",
            argv(&["powershell", "-c", "iex $s"]),
            vec!["powershell"],
        ),
        (
            "2",
            "powershell -File",
            argv(&["powershell", "-File", "x.ps1"]),
            vec!["powershell"],
        ),
        ("2", "cmd (no /c)", argv(&["cmd"]), vec!["cmd"]),
        // Scripts the interpreter may read differently from kGraf: a quote
        // on the command line. Granting the program they seem to run does
        // not cover them.
        (
            "2",
            "cmd /c with a quote",
            argv(&["cmd", "/c", "dir", "\"a\""]),
            vec!["cmd", "dir"],
        ),
        (
            "2",
            "cmd /c one quoted string",
            argv(&["cmd", "/c", "dir /b"]),
            vec!["cmd", "dir"],
        ),
        (
            "2",
            "powershell -c with a quote",
            argv(&["powershell", "-c", "Get-Item 'a'"]),
            vec!["powershell", "Get-Item"],
        ),
        (
            "3",
            "git -c core.hooksPath",
            argv(&["git", "-c", "core.hooksPath=h", "commit"]),
            vec!["git"],
        ),
        (
            "3",
            "git --config-env",
            argv(&["git", "--config-env=core.pager=V", "log"]),
            vec!["git"],
        ),
        ("4", "$CMD args", argv(&["sh", "-c", "$CMD x"]), vec!["sh"]),
        ("4", "${X}", argv(&["sh", "-c", "${X} x"]), vec!["sh"]),
        ("4", "\"$@\"", argv(&["sh", "-c", "\"$@\""]), vec!["sh"]),
        (
            "4",
            "$(...) as the program",
            argv(&["sh", "-c", "$(which curl) x"]),
            vec!["sh", "which"],
        ),
        (
            "4",
            "eval \"$X\"",
            argv(&["sh", "-c", "eval \"$X\""]),
            vec!["sh", "eval"],
        ),
        (
            "4",
            "PATH=DIR CMD",
            argv(&["env", "PATH=./bin", "ls"]),
            vec!["env", "ls"],
        ),
        (
            "4",
            "NODE_OPTIONS=--require",
            argv(&[
                "env",
                "NODE_OPTIONS=--require ./evil.js",
                "node",
                "setup.js",
            ]),
            vec!["env", "node"],
        ),
        (
            "4",
            "LD_PRELOAD=",
            argv(&["sh", "-c", "LD_PRELOAD=./evil.so ls"]),
            vec!["sh", "ls"],
        ),
        (
            "4",
            "export BASH_ENV=",
            argv(&["sh", "-c", "export BASH_ENV=./evil.sh; bash -c ls"]),
            vec!["sh", "export", "bash", "ls"],
        ),
        (
            "6a",
            "make without a Makefile",
            argv(&["make", "-C", "empty"]),
            vec!["make"],
        ),
        (
            "6a",
            "make program named by a != variable",
            argv(&["make", "-C", "bang"]),
            vec!["make"],
        ),
        (
            "6a",
            "npm run in another workspace",
            argv(&["npm", "run", "x", "-w", "pkg"]),
            vec!["npm"],
        ),
        (
            "6b",
            "python3 -c exec()",
            argv(&["python3", "-c", "exec(s)"]),
            vec!["python3"],
        ),
        (
            "6b",
            "node -e eval()",
            argv(&["node", "-e", "eval(s)"]),
            vec!["node"],
        ),
        (
            "6b",
            "awk system($0)",
            argv(&["awk", "{ system($0) }"]),
            vec!["awk"],
        ),
        (
            "6b",
            "php with options and no code or file",
            argv(&["php", "-q"]),
            vec!["php"],
        ),
        (
            "6b",
            "lua -e",
            argv(&["lua", "-e", "os.execute('curl x')"]),
            vec!["lua"],
        ),
        (
            "6c",
            "python3 FILE (dynamic call)",
            argv(&["python3", "dyn.py", "curl x"]),
            vec!["python3"],
        ),
        (
            "6c",
            "python3 FILE (missing)",
            argv(&["python3", "missing.py"]),
            vec!["python3"],
        ),
        ("6c", "python3 (input)", argv(&["python3"]), vec!["python3"]),
    ]
}

fn print_matrix() -> bool {
    std::env::var_os("KGRAF_PRINT_MATRIX").is_some()
}

/// Property 1: a deny rule on the inner program refuses the call in every
/// mode; an ask rule asks in every mode.
#[tokio::test]
async fn a_rule_judges_what_a_runner_a_setting_a_recipe_or_code_runs_in_every_mode() {
    let dir = worktree();
    let mut failures = Vec::new();
    for (gap, label, words, _) in visible_cases() {
        let mut row = Vec::new();
        for mode in PermissionMode::ALL {
            let got = outcome(
                dir.path(),
                mode,
                &[],
                &[],
                rules(&["shell(curl*)"], &[]),
                &words,
            )
            .await;
            row.push(got);
            if got != "deny" {
                failures.push(format!("deny rule, {mode}: {words:?}: {got}"));
            }
            let asked = outcome(
                dir.path(),
                mode,
                &[],
                &[],
                rules(&[], &["shell(curl*)"]),
                &words,
            )
            .await;
            if asked != "ask" {
                failures.push(format!("ask rule, {mode}: {words:?}: {asked}"));
            }
        }
        if print_matrix() {
            println!("RULE | {gap} | {label} | {words:?} | {}", row.join(" | "));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Property 1, no over-reach: what runs nothing the rule names is not
/// refused, and a recipe or script that runs no `curl` passes.
#[tokio::test]
async fn a_rule_does_not_refuse_what_runs_another_program() {
    let dir = worktree();
    for mode in PermissionMode::ALL {
        for words in [
            argv(&["make", "build"]),
            argv(&["npm", "test"]),
            argv(&["just", "build"]),
            argv(&["python3", "clean.py"]),
            argv(&["python3", "-c", "print('curl')"]),
            argv(&["docker", "exec", "web", "ls"]),
            // Separate words: one quoted string is unreadable (cmd may run
            // it as a program name).
            argv(&["cmd", "/c", "echo", "curl"]),
            argv(&["git", "-c", "core.pager=less", "log"]),
            argv(&["git", "st"]),
            argv(&["sh", "-c", "echo $X"]),
        ] {
            let got = outcome(
                dir.path(),
                mode,
                &[],
                &[],
                rules(&["shell(curl*)"], &[]),
                &words,
            )
            .await;
            assert!(got != "deny" && got != "ask", "{mode}: {words:?}: {got}");
        }
    }
}

/// Property 2: what kGraf cannot read asks in every mode under a rule, and
/// with no rule the mode decides and no grant covers it.
#[tokio::test]
async fn an_unreadable_command_asks_under_a_rule_and_is_never_granted() {
    let dir = worktree();
    let mut failures = Vec::new();
    for (gap, label, words, grant) in opaque_cases() {
        let mut row = Vec::new();
        for mode in PermissionMode::ALL {
            for user in [rules(&["shell(curl*)"], &[]), rules(&[], &["shell(curl*)"])] {
                let got = outcome(dir.path(), mode, &[], &[], user, &words).await;
                row.push(got);
                if got != "ask" {
                    failures.push(format!("{mode}: {words:?}: {got}"));
                }
            }
            let granted = outcome(
                dir.path(),
                mode,
                &grant,
                &grant,
                UserRules::default(),
                &words,
            )
            .await;
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
            println!("OPAQUE | {gap} | {label} | {words:?} | {}", row.join(" | "));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Property 3: a grant of the outer program, in the Auto allow set and to
/// the contract, does not grant `curl`.
#[tokio::test]
async fn a_grant_of_the_runner_or_the_interpreter_does_not_grant_what_it_runs() {
    let dir = worktree();
    let mut failures = Vec::new();
    for (gap, label, words, grant) in visible_cases() {
        let mut row = Vec::new();
        for mode in PermissionMode::ALL {
            let got = outcome(
                dir.path(),
                mode,
                &grant,
                &grant,
                UserRules::default(),
                &words,
            )
            .await;
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
            println!("GRANT | {gap} | {label} | {words:?} | {}", row.join(" | "));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Property 3, the other side: with every program granted, including the
/// inner one, the command runs; ordinary commands with no rules are decided
/// as before (Manual asks, Bypass runs, Auto runs what its allow set
/// covers).
#[tokio::test]
async fn ordinary_commands_with_no_rules_are_decided_as_before() {
    let dir = worktree();
    for (words, allow) in [
        (argv(&["cargo", "test"]), vec!["cargo", "curl"]),
        (argv(&["pytest", "-q"]), vec!["pytest"]),
        (argv(&["npm", "test"]), vec!["npm", "jest", "node"]),
        (argv(&["python3", "clean.py"]), vec!["python3"]),
        (argv(&["git", "status"]), vec!["git"]),
        (argv(&["git", "st"]), vec!["git"]),
        (argv(&["npx", "-c", "curl x"]), vec!["npx", "curl"]),
        (
            argv(&["docker", "exec", "web", "curl", "x"]),
            vec!["docker", "curl"],
        ),
        (argv(&["make", "deploy"]), vec!["make", "cc", "curl"]),
    ] {
        for (mode, want) in [
            (PermissionMode::Manual, "user"),
            (PermissionMode::Bypass, "run"),
            (PermissionMode::OrchestratorDecides, "orch"),
            (PermissionMode::Auto, "run"),
        ] {
            let got = outcome(dir.path(), mode, &allow, &[], UserRules::default(), &words).await;
            assert_eq!(got, want, "{mode}: {words:?}");
        }
    }
    // The change for a user who allowed only the build tool: its recipe's
    // programs need a grant too.
    assert_eq!(
        outcome(
            dir.path(),
            PermissionMode::Auto,
            &["npm"],
            &[],
            UserRules::default(),
            &argv(&["npm", "test"])
        )
        .await,
        "user"
    );
}

/// The command policy's `forbid` rules see a command a recipe, a runner or
/// code runs, in Bypass too.
#[tokio::test]
async fn a_forbid_rule_sees_what_a_runner_or_code_runs_in_bypass() {
    let dir = worktree();
    for words in [
        argv(&["npx", "-c", "git push --force"]),
        argv(&["docker", "exec", "web", "git", "push", "--force"]),
        argv(&["cmd", "/c", "git push --force"]),
        argv(&["python3", "-c", "import os; os.system('git push --force')"]),
        argv(&["git", "-c", "core.pager=git push --force", "log"]),
    ] {
        assert_eq!(
            outcome(
                dir.path(),
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
