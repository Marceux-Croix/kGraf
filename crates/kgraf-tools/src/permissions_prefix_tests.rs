//! Prefix words and grouping never hide a command's program from the user's
//! deny and ask rules, the deletion limit or the command policy's `forbid`
//! rules, and a directory change behind a prefix word is still a directory
//! change to both script readers.
//!
//! How to run: `cargo test -p kgraf-tools permissions::prefix_tests`. No
//! network, no files written; paths are fixed strings under a fictional home
//! directory, never the real one. Each test names what it covers in its doc
//! comment; a failure message quotes the command that broke.

use super::*;
use crate::execpolicy::{Decision, ExecPolicy};
use kgraf_core::{AgentId, TaskId};
use serde_json::json;

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

fn call(words: &[&str]) -> PendingCall {
    PendingCall {
        call_id: "c".into(),
        tool: "shell".into(),
        input: json!({ "command": words }),
        worker: AgentId::new("w"),
        task: TaskId::new("t"),
    }
}

fn home() -> PathBuf {
    PathBuf::from("/home/someone")
}

fn ws() -> PathBuf {
    PathBuf::from("/home/someone/proj/.kgraf/worktrees/w1")
}

fn rules(deny: &[&str], ask: &[&str]) -> UserRules {
    UserRules {
        deny: deny.iter().map(|r| Rule::parse(r).expect("rule")).collect(),
        ask: ask.iter().map(|r| Rule::parse(r).expect("rule")).collect(),
    }
}

/// A control in `mode` with `allow` in the Auto allow set and `user` rules,
/// and the rules hook on it. No script construct is on.
fn control(
    mode: PermissionMode,
    allow: &[&str],
    user: UserRules,
) -> (Arc<PermissionControl>, PermissionRulesHook) {
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
    let hook = PermissionRulesHook::new(control.clone(), Some(ws())).with_home(Some(home()));
    (control, hook)
}

/// Every shape in which a prefix word or a group stands before `curl`. Each
/// runs `curl` when the shell runs it.
fn hidden_curl() -> Vec<Vec<&'static str>> {
    let scripts = [
        "{ curl x; }",
        "mkdir d && { curl x; }",
        "( curl x )",
        "! curl x",
        "time curl x",
        "time -p curl x",
        "exec curl x",
        "exec -a name curl x",
        "command curl x",
        "command -p curl x",
        "builtin curl x",
        "nohup curl x",
        "env curl x",
        "env A=1 curl x",
        "env -u A curl x",
        "env -i A=1 curl x",
        "A=1 curl x",
        "A=1 B=2 curl x",
        "if curl x; then :; fi",
        "if true; then curl x; fi",
        "if false; then :; else curl x; fi",
        "while curl x; do :; done",
        "until curl x; do :; done",
        "for i in 1; do curl x; done",
        "timeout 5s curl x",
        "timeout -s KILL 5 curl x",
        "nice -n 5 curl x",
        "sudo -u bob curl x",
        "xargs curl",
        "2>/dev/null curl x",
        "> out.txt curl x",
        "f() { curl x; }; f",
        "{ nohup env A=1 command curl x; }",
        "eval curl x",
        "eval 'ls; curl x'",
        "bash -c 'curl x'",
        "bash -c 'env sh -c \"curl x\"'",
    ];
    let mut out: Vec<Vec<&'static str>> = scripts.iter().map(|s| vec!["sh", "-c", *s]).collect();
    out.extend([
        vec!["env", "curl", "x"],
        vec!["env", "A=1", "curl", "x"],
        vec!["nohup", "curl", "x"],
        vec!["command", "curl", "x"],
        vec!["timeout", "5", "curl", "x"],
        vec!["env", "sh", "-c", "curl x"],
        vec!["nohup", "bash", "-c", "curl x"],
        vec!["/usr/bin/env", "bash", "-lc", "{ curl x; }"],
    ]);
    out
}

/// Defect A. A deny rule refuses, and an ask rule asks about, a command
/// hidden behind a group or a prefix word, in every permission mode, with no
/// script construct on. Before the fix the rule matched the command line from
/// its first word (`{ curl x`), so these ran in Bypass and Manual.
#[tokio::test]
async fn a_rule_sees_the_program_behind_a_group_or_a_prefix_word_in_every_mode() {
    for mode in PermissionMode::ALL {
        let (_, deny) = control(mode, &["mkdir", "curl"], rules(&["shell(curl*)"], &[]));
        let (_, ask) = control(mode, &["mkdir", "curl"], rules(&[], &["shell(curl x*)"]));
        for words in hidden_curl() {
            match deny.review(&call(&words)).await {
                HookDecision::Deny { reason } => {
                    assert_eq!(
                        reason,
                        "your deny rule `shell(curl*)` blocks this call in every permission mode",
                        "{mode}: {words:?}"
                    );
                    // The refusal names `curl`, not the group or the wrapper.
                    assert_eq!(
                        refused_subject(
                            "shell",
                            &call(&words).input,
                            PERMISSION_RULES_HOOK,
                            &reason
                        ),
                        Some(RefusedSubject::Program("curl".into())),
                        "{mode}: {words:?}"
                    );
                }
                other => panic!("{mode}: {words:?} must be refused: {other:?}"),
            }
            if words.last() == Some(&"xargs curl") {
                // `xargs curl` has no `x` argument for the exact ask rule.
                continue;
            }
            assert!(
                matches!(ask.review(&call(&words)).await, HookDecision::Ask { .. }),
                "{mode}: {words:?} must be asked about"
            );
        }
    }
}

/// Defect A, no over-reach: a command that only names `curl` as an argument
/// or prints it is not refused, and a plain allowed command still runs.
#[tokio::test]
async fn a_rule_does_not_match_a_program_named_only_as_an_argument() {
    for mode in PermissionMode::ALL {
        let (_, deny) = control(mode, &[], rules(&["shell(curl*)"], &[]));
        for words in [
            vec!["sh", "-c", "echo curl x"],
            vec!["sh", "-c", "{ echo curl; }"],
            vec!["sh", "-c", "grep curl notes.txt"],
            vec!["ls"],
        ] {
            assert_eq!(
                deny.review(&call(&words)).await,
                HookDecision::Allow,
                "{mode}: {words:?}"
            );
        }
    }
}

/// The shared prefix scan lists every place the program may stand, and
/// stops one word after the first word that is surely not a prefix.
#[test]
fn command_starts_skips_syntax_wrappers_assignments_options_and_redirections() {
    let cases: &[(&[&str], &[usize])] = &[
        (&["curl", "x"], &[0]),
        (&["{", "curl", "x"], &[0, 1]),
        (&["!", "curl"], &[0, 1]),
        (&["time", "-p", "curl", "x"], &[0, 1, 2, 3]),
        (&["exec", "-a", "n", "curl", "x"], &[0, 1, 2, 3]),
        (&["command", "curl"], &[0, 1]),
        (&["env", "-u", "A", "B=1", "curl", "x"], &[0, 1, 2, 3, 4]),
        (&["A=1", "curl"], &[0, 1]),
        (&["then", "curl"], &[0, 1]),
        (&["timeout", "5s", "curl", "x"], &[0, 1, 2]),
        (&["2>/dev/null", "curl"], &[0, 1]),
        (&[">", "out", "curl"], &[0, 1, 2]),
        (&["2>&1", "curl"], &[0, 1]),
        (&["echo", "rm", "-rf", "/"], &[0]),
    ];
    for (words, want) in cases {
        assert_eq!(command_starts(words), *want, "{words:?}");
    }
    assert!(command_starts::<&str>(&[]).is_empty());
}

/// A shell behind a prefix word, and a shell started inside a script, are
/// split like the call's own shell.
#[test]
fn simple_commands_follow_a_shell_behind_a_prefix_and_a_nested_shell() {
    let words = |c: Vec<Vec<String>>| -> Vec<String> { c.iter().map(|w| w.join(" ")).collect() };
    assert_eq!(
        words(simple_commands(&argv(&[
            "env",
            "A=1",
            "sh",
            "-c",
            "ls; curl x"
        ]))),
        ["env A=1 sh -c ls; curl x", "ls", "curl x"]
    );
    assert_eq!(
        words(simple_commands(&argv(&[
            "sh",
            "-c",
            "ls && bash -c 'curl x; wget y'"
        ]))),
        [
            "sh -c ls && bash -c 'curl x; wget y'",
            "ls",
            "bash -c curl x; wget y",
            "curl x",
            "wget y"
        ]
    );
}

/// Defect B. The strict reader refuses to judge a script that redirects to a
/// file after a directory change hidden behind `command`, `builtin`, `exec`,
/// `env` or `nohup`, or made by `pushd` or `popd`. Before the fix it looked
/// for `cd` only as a command's first word.
#[test]
fn the_strict_reader_sees_a_directory_change_behind_a_prefix_word() {
    for s in [
        "command cd x; echo hi > out.txt",
        "command cd x && echo hi > out.txt",
        "builtin cd x; echo hi > out.txt",
        "exec cd x; echo hi > out.txt",
        "env cd x; echo hi > out.txt",
        "nohup cd x; echo hi > out.txt",
        "command -p cd x; echo hi > out.txt",
        "pushd x; echo hi > out.txt",
        "popd; echo hi > out.txt",
        "command pushd x; echo hi > out.txt",
        "echo hi > out.txt; command cd x",
    ] {
        match script_commands(&argv(&["sh", "-c", s])) {
            Script::Unjudged(why) => assert!(why.contains("changing directory"), "{s:?}: {why}"),
            other => panic!("{s:?} must not be judged: {other:?}"),
        }
        // The second reader, with every switch but outside redirects on,
        // refuses it too.
        let constructs = ScriptConstructs(
            ScriptConstruct::ALL
                .into_iter()
                .filter(|c| *c != ScriptConstruct::OutsideRedirects)
                .collect(),
        );
        assert!(
            matches!(
                script_commands_with(&argv(&["sh", "-c", s]), &constructs),
                Script::Unjudged(_)
            ),
            "{s:?}"
        );
    }
    // A directory change with no file redirection, or with `/dev/null`, is
    // still judged.
    for s in ["command cd x && ls", "command cd x && ls 2>/dev/null"] {
        assert!(
            matches!(
                script_commands(&argv(&["sh", "-c", s])),
                Script::Commands(_)
            ),
            "{s:?}"
        );
    }
}

/// Defect B in every mode: a grant of every program of the script (a
/// contract's grant, and Auto's allow set) does not grant `command cd x;
/// echo hi > out.txt`; the call is asked about with the reason. Bypass grants
/// every script whole without reading it, before and after the fix.
#[test]
fn a_redirect_after_a_hidden_directory_change_is_never_granted_by_a_script_grant() {
    let script = argv(&["sh", "-c", "command cd x; echo hi > out.txt"]);
    let programs = ["command", "echo", "cd"];
    for mode in PermissionMode::ALL {
        let (control, _) = control(mode, &programs, UserRules::default());
        let policy = ExecPolicy::new(Decision::Prompt)
            .with_permissions(control)
            .with_granted_programs(programs.iter().map(|p| (*p).to_owned()));
        let v = policy.evaluate(&script);
        if mode == PermissionMode::Bypass {
            assert!(v.decision == Decision::Allow && v.granted, "{mode}: {v:?}");
            continue;
        }
        assert_eq!(v.decision, Decision::Prompt, "{mode}: {v:?}");
        assert!(!v.granted, "{mode}: {v:?}");
        let note = v.script_note.as_deref().unwrap_or_default();
        assert!(note.contains("changing directory"), "{mode}: {note}");
    }
}

/// The deletion limit sees a deleter behind every prefix shape and a
/// directory change behind a prefix word, `pushd` and `cd -P`.
#[test]
fn the_deletion_limit_sees_a_deleter_and_a_directory_change_behind_prefix_words() {
    let limit = |words: &[&str]| critical_deletion(&argv(words), Some(&home()), Some(&ws()));
    for words in [
        &["sh", "-c", "if rm -rf /; then :; fi"][..],
        &["sh", "-c", "while rm -rf /; do :; done"],
        &["sh", "-c", "timeout 5s rm -rf /"],
        &["sh", "-c", "2>/dev/null rm -rf /"],
        &["sh", "-c", "builtin rm -rf /"],
        &["sh", "-c", "sudo -u cd rm -rf /"],
        &["env", "sh", "-c", "rm -rf /"],
        &["sh", "-c", "bash -c 'rm -rf ~'"],
        &["sh", "-c", "{ cd /; rm -rf *; }"],
        &["sh", "-c", "command cd / && rm -rf *"],
        &["sh", "-c", "builtin cd / && rm -rf *"],
        &["sh", "-c", "pushd / && rm -rf *"],
        &["sh", "-c", "cd -P / && rm -rf *"],
        &["sh", "-c", "! cd / || rm -rf *"],
    ] {
        assert!(
            matches!(limit(words), Some(HardLimit::Deny(_))),
            "{words:?}: {:?}",
            limit(words)
        );
    }
    // An uncertain change keeps the old place too: the parent of the
    // workspace is still a critical path after `command cd a/b/c`.
    assert!(matches!(
        limit(&["sh", "-c", "command cd a/b/c; rm -rf .."]),
        Some(HardLimit::Ask(_))
    ));
    assert_eq!(limit(&["sh", "-c", "command cd sub && rm -rf build"]), None);
}

/// A `forbid` rule of the command policy refuses its shape behind a prefix
/// word too, in every mode, Bypass included.
#[test]
fn a_forbid_rule_refuses_its_shape_behind_a_prefix_word() {
    for mode in PermissionMode::ALL {
        let (control, _) = control(mode, &["env", "command"], UserRules::default());
        let policy = ExecPolicy::default_kgraf_policy().with_permissions(control);
        for words in [
            &["git", "push", "--force"][..],
            &["env", "git", "push", "--force"],
            &["command", "git", "push", "--force", "origin"],
            &["nohup", "env", "A=1", "rm", "-rf", "/"],
        ] {
            let v = policy.evaluate(&argv(words));
            assert_eq!(v.decision, Decision::Forbidden, "{mode}: {words:?}: {v:?}");
        }
        assert_ne!(
            policy.evaluate(&argv(&["env", "git", "push"])).decision,
            Decision::Forbidden,
            "{mode}"
        );
    }
}

// The matcher and deletion limit as they were before prefix words were
// handled, kept only as the reference for the no-loosening check below. Do
// not call these from code.

/// The old split: the whole argv and, only when its first word is a shell,
/// each command of its `-c` script.
fn legacy_simple_commands(words: &[String]) -> Vec<Vec<String>> {
    let mut out = vec![words.to_vec()];
    let Some(program) = words.first() else {
        return out;
    };
    if !SHELLS.contains(&base_name(program)) {
        return out;
    }
    let mut it = words.iter().skip(1);
    while let Some(arg) = it.next() {
        if arg.starts_with('-') && arg.contains('c') && !arg.starts_with("--") {
            if let Some(script) = it.next() {
                for segment in split_script(script) {
                    let w = shlex::split(&segment)
                        .unwrap_or_else(|| segment.split_whitespace().map(str::to_owned).collect());
                    if !w.is_empty() {
                        out.push(w);
                    }
                }
            }
            break;
        }
    }
    out
}

/// The old rule subjects: each old command joined with single spaces.
fn legacy_subjects(words: &[String]) -> Vec<String> {
    legacy_simple_commands(words)
        .iter()
        .map(|c| c.join(" "))
        .collect()
}

/// The old deleter scan.
fn legacy_program_index(words: &[&str]) -> Option<usize> {
    const OLD_WRAPPERS: &[&str] = &[
        "sudo", "doas", "env", "command", "nice", "nohup", "time", "exec", "busybox", "xargs",
        "timeout",
    ];
    for (i, w) in words.iter().enumerate() {
        let b = base_name(w);
        if DELETERS.contains(&b) || b == "find" {
            return Some(i);
        }
        let prefix = OLD_WRAPPERS.contains(&b)
            || matches!(*w, "{" | "!" | "(")
            || w.starts_with('-')
            || w.contains('=')
            || w.parse::<f64>().is_ok()
            || (i > 0 && words[i - 1].starts_with('-') && i > 1);
        if !prefix {
            return None;
        }
    }
    None
}

/// The old deletion limit: one working directory, moved only by a command
/// whose first word is `cd`.
fn legacy_critical_deletion(
    argv: &[String],
    home: Option<&Path>,
    workspace: Option<&Path>,
) -> Option<HardLimit> {
    let mut ask: Option<HardLimit> = None;
    let mut cwd = match workspace {
        Some(ws) => Cwd::Known(ws.to_path_buf()),
        None => Cwd::NoWorkspace,
    };
    for command in legacy_simple_commands(argv) {
        let words: Vec<&str> = command.iter().map(String::as_str).collect();
        if words.first().map(|w| base_name(w)) == Some("cd") {
            cwd = cd(&cwd, words.get(1).copied(), home);
            continue;
        }
        let Some(at) = legacy_program_index(&words) else {
            continue;
        };
        let program = base_name(words[at]);
        let rest = &words[at + 1..];
        if program == "find" {
            if !rest.contains(&"-delete") {
                continue;
            }
            for w in rest
                .iter()
                .take_while(|w| !w.starts_with('-') && **w != "(" && **w != "!")
            {
                if let Target::System(p) = classify(w, home, &cwd, workspace) {
                    return Some(system_refusal(&p));
                }
            }
            continue;
        }
        let recursive = program == "rmdir"
            || rest.iter().any(|w| {
                *w == "--recursive"
                    || (w.starts_with('-')
                        && !w.starts_with("--")
                        && (w.contains('r') || w.contains('R')))
            });
        let mut operands_only = false;
        for w in rest {
            if !operands_only && *w == "--" {
                operands_only = true;
                continue;
            }
            if !operands_only && w.starts_with('-') {
                continue;
            }
            match classify(w, home, &cwd, workspace) {
                Target::System(p) => return Some(system_refusal(&p)),
                Target::Critical(_) => {
                    ask.get_or_insert(HardLimit::Ask(String::new()));
                }
                Target::Unresolved(_) if recursive => {
                    ask.get_or_insert(HardLimit::Ask(String::new()));
                }
                Target::Unresolved(_) | Target::Plain => {}
            }
        }
    }
    ask
}

fn limit_strictness(l: &Option<HardLimit>) -> u8 {
    match l {
        Some(HardLimit::Deny(_)) => 2,
        Some(HardLimit::Ask(_)) => 1,
        None => 0,
    }
}

/// The commands the existing tests refuse or ask about, gathered as one
/// corpus, with directory-change, group and wrapper shapes around them and
/// every hidden shape above.
fn refusal_corpus() -> Vec<Vec<String>> {
    let base: &[&[&str]] = &[
        &["rm", "-rf", "/"],
        &["rm", "-rf", "/usr"],
        &["rm", "-rf", "/etc/"],
        &["rm", "-rf", "/*"],
        &["rm", "-rf", "~"],
        &["rm", "-rf", "~/"],
        &["rm", "-rf", "$HOME"],
        &["rm", "-r", "/home/someone"],
        &["rm", "-r", "/home"],
        &["sudo", "rm", "-rf", "/"],
        &["rmdir", "/tmp"],
        &["sh", "-c", "cd /tmp && rm -rf /"],
        &["bash", "-lc", "echo hi; rm -rf ~"],
        &["rm", "-rf", "--", "/"],
        &["rm", "-rf", "/u*"],
        &["find", "/", "-delete"],
        &["sh", "-c", "cd / && rm -rf *"],
        &["bash", "-c", "cd ~ ; rm -rf ./*"],
        &["sh", "-c", "cd /usr/local && rm -rf ../*"],
        &["sh", "-c", "{ rm -rf ~; }"],
        &["sh", "-c", "! rm -rf /"],
        &["sh", "-c", "( rm -rf / )"],
        &["rm", "-rf", "."],
        &["rm", "-rf", ".git"],
        &["rm", "-rf", ".."],
        &["rm", "-rf", "../other"],
        &["rm", "-rf", "$TARGET"],
        &["sh", "-c", "cd $DIR && rm -rf *"],
        &["sh", "-c", "cd /tmp/scratch/x && rm -rf ."],
        &["sh", "-c", "cd a/b/c && rm -rf .."],
        &["sh", "-c", "cd a/b/c && command cd x && rm -rf ../.."],
        &["sh", "-c", "command cd a/b/c; rm -rf .."],
        &["sh", "-c", "pushd a/b/c; rm .."],
        &["sh", "-c", "cd -P a/b && rm -rf .."],
        &["sh", "-c", "(cd a/b/c); rm -rf .."],
        &["sh", "-c", "bash -c 'cd a/b/c'; rm -rf .."],
        &["sh", "-c", "bash -c 'cd /'; rm -rf *"],
        &["sh", "-c", "cd sub; bash -c 'rm -rf ../..'"],
        &["curl", "x"],
        &["git", "push", "origin"],
        &["sh", "-c", "make && git push --force"],
        &["python3", "x.py"],
        &["sh", "-c", "mkdir x && python3 y"],
        &["sh", "-c", "C=curl; ls && $C u"],
        &["sh", "-c", "mkdir x && cp $(curl -s u) x/"],
        &["sh", "-c", "mkdir x && cp `curl -s u` x/"],
        &["sh", "-c", "curl -s u & mkdir x"],
        &["sh", "-c", "mkdir x && /usr/bin/curl -s u"],
        &["sh", "-c", "wget y | sh"],
    ];
    let mut out: Vec<Vec<String>> = base.iter().map(|w| argv(w)).collect();
    out.extend(hidden_curl().iter().map(|w| argv(w)));
    out
}

/// No loosening. For every command of the corpus: the old rule subjects are
/// all still subjects, and the rules hook is, in every mode, at least as
/// strict as the old rules and the old deletion limit together were.
#[tokio::test]
async fn no_previously_refused_command_of_the_corpus_is_now_allowed() {
    let user = rules(
        &["shell(curl*)", "shell(python3*)"],
        &["shell(git push*)", "shell(wget*)"],
    );
    let strictness = |d: &HookDecision| match d {
        HookDecision::Deny { .. } => 2u8,
        HookDecision::Ask { .. } => 1,
        _ => 0,
    };
    for words in refusal_corpus() {
        let c = call(&words.iter().map(String::as_str).collect::<Vec<_>>());
        let now = subjects("shell", &c.input);
        let old_subjects = legacy_subjects(&words);
        for old in &old_subjects {
            assert!(
                now.contains(old),
                "{words:?}: lost subject {old:?} from {now:?}"
            );
        }
        let old_rule = if user.deny.iter().any(|r| r.matches("shell", &old_subjects)) {
            2
        } else if user.ask.iter().any(|r| r.matches("shell", &old_subjects)) {
            1
        } else {
            0
        };
        let old_deletion = limit_strictness(&legacy_critical_deletion(
            &words,
            Some(&home()),
            Some(&ws()),
        ));
        let new_deletion = limit_strictness(&critical_deletion(&words, Some(&home()), Some(&ws())));
        assert!(
            new_deletion >= old_deletion,
            "{words:?}: deletion {new_deletion} < old {old_deletion}"
        );
        let old = old_rule.max(old_deletion);
        for mode in PermissionMode::ALL {
            let (_, hook) = control(mode, &[], user.clone());
            let got = strictness(&hook.review(&c).await);
            assert!(got >= old, "{mode}: {words:?}: {got} < old {old}");
        }
    }
}
