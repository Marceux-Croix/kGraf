//! A permission rule or allow-set entry that names no tool is refused where it
//! is read; Claude Code's tool names are read as the kGraf tools that do the
//! same job; an MCP name is accepted and checked at run start; and no rule
//! matches less than it did before.
//!
//! How to run: `cargo test -p kgraf-tools permissions::name_tests`. No
//! network, no files written; paths are fixed strings under a fictional home
//! directory, never the real one. Each test names what it covers in its doc
//! comment; a failure message quotes the rule and the call that broke.

use super::*;
use kgraf_core::{AgentId, TaskId};
use serde_json::json;

fn call(tool: &str, input: serde_json::Value) -> PendingCall {
    PendingCall {
        call_id: "c".into(),
        tool: tool.into(),
        input,
        worker: AgentId::new("w"),
        task: TaskId::new("t"),
    }
}

fn shell(words: &[&str]) -> PendingCall {
    call("shell", json!({ "command": words }))
}

fn patch(path: &str) -> PendingCall {
    call(
        "apply_patch",
        json!({"patch": format!("*** Begin Patch\n*** Add File: {path}\n+x\n*** End Patch")}),
    )
}

fn rules(deny: &[&str], ask: &[&str]) -> UserRules {
    UserRules {
        deny: deny.iter().map(|r| Rule::parse(r).expect(r)).collect(),
        ask: ask.iter().map(|r| Rule::parse(r).expect(r)).collect(),
    }
}

fn hook(mode: PermissionMode, user: UserRules) -> PermissionRulesHook {
    let settings = PermissionSettings {
        mode,
        rules: user,
        ..PermissionSettings::default()
    };
    let control = Arc::new(
        PermissionControl::new(settings, ModeSource::CommandLine, ProcessFacts::default())
            .expect("control"),
    );
    PermissionRulesHook::new(control, Some(PathBuf::from("/home/someone/proj/w1")))
        .with_home(Some(PathBuf::from("/home/someone")))
}

fn hits(rule: &str, c: &PendingCall) -> bool {
    Rule::parse(rule)
        .expect(rule)
        .matches(&c.tool, &subjects(&c.tool, &c.input))
}

/// The defect: `Bash(rm -rf*)` matched nothing, because no worker has a tool
/// named `Bash`. Read as `shell`, it denies `rm -rf x` in every mode, Bypass
/// included, in a script and behind a wrapper too, and the refusal quotes the
/// rule as the user wrote it.
#[tokio::test]
async fn a_claude_code_bash_deny_rule_refuses_the_shell_command_in_every_mode() {
    for mode in PermissionMode::ALL {
        let h = hook(mode, rules(&["Bash(rm -rf*)"], &["Bash(git push:*)"]));
        for words in [
            vec!["rm", "-rf", "x"],
            vec!["sh", "-c", "make && rm -rf x"],
            vec!["env", "A=1", "rm", "-rf", "x"],
        ] {
            assert_eq!(
                h.review(&shell(&words)).await,
                HookDecision::Deny {
                    reason: "your deny rule `Bash(rm -rf*)` blocks this call in every permission \
                             mode"
                        .into()
                },
                "{mode}: {words:?}"
            );
        }
        assert_eq!(
            h.review(&shell(&["rm", "x"])).await,
            HookDecision::Allow,
            "{mode}: a command the rule does not name"
        );
        for words in [vec!["git", "push"], vec!["git", "push", "origin", "main"]] {
            assert!(
                matches!(h.review(&shell(&words)).await, HookDecision::Ask { .. }),
                "{mode}: {words:?} under `Bash(git push:*)`"
            );
        }
    }
}

/// A `Bash(...)` rule matches exactly the calls the same `shell(...)` rule
/// does: the alias changes the name, never the matching.
#[test]
fn a_bash_rule_and_its_shell_rule_match_the_same_calls() {
    let calls = [
        shell(&["rm", "-rf", "x"]),
        shell(&["rm"]),
        shell(&["ls"]),
        shell(&["lsof"]),
        shell(&["git", "push", "origin"]),
        shell(&["git", "log", "-5", "main"]),
        shell(&["node", "--version"]),
        shell(&["sh", "-c", "cd x && rm -rf y"]),
        patch("rm"),
    ];
    for pattern in [
        "",
        "(rm*)",
        "(rm *)",
        "(rm:*)",
        "(ls *)",
        "(ls*)",
        "(git push*)",
        "(git log * main)",
        "(* --version)",
        "(rm -rf y)",
    ] {
        for c in &calls {
            assert_eq!(
                hits(&format!("Bash{pattern}"), c),
                hits(&format!("shell{pattern}"), c),
                "Bash{pattern} vs shell{pattern} on {:?}",
                c.input
            );
        }
    }
}

/// Claude Code's wildcard forms: a `*` anywhere stands for any text, and a
/// trailing ` *` or `:*` that is the only wildcard also matches the bare
/// command; `ls*` (no space) matches `lsof` and `ls *` does not.
#[test]
fn wildcards_match_as_claude_code_documents_them() {
    let cases: &[(&str, &[&str], bool)] = &[
        ("shell(npm run *)", &["npm", "run"], true),
        ("shell(npm run *)", &["npm", "run", "build"], true),
        ("shell(npm run *)", &["npm", "install"], false),
        ("shell(ls *)", &["ls"], true),
        ("shell(ls *)", &["lsof"], false),
        ("shell(ls*)", &["lsof"], true),
        ("shell(ls:*)", &["ls"], true),
        ("shell(ls:*)", &["ls", "-la"], true),
        ("shell(ls:*)", &["lsof"], false),
        ("shell(git log * main)", &["git", "log", "-5", "main"], true),
        ("shell(git log * main)", &["git", "push", "main"], false),
        ("shell(* --version)", &["node", "--version"], true),
        ("shell(* --version)", &["node", "-v"], false),
        ("shell(* --help *)", &["npm", "--help"], false),
        ("shell(* --help *)", &["npm", "--help", "x"], true),
    ];
    for (rule, words, want) in cases {
        assert_eq!(hits(rule, &shell(words)), *want, "{rule} on {words:?}");
    }
}

/// Every rule that matched before still matches: the old matching (exact, or
/// a prefix when the pattern ends in `*`) implies the new one over a corpus
/// of patterns and subjects, `:*` and `./` patterns included.
#[test]
fn no_rule_matches_less_than_before() {
    let old = |p: &str, s: &str| match p.strip_suffix('*') {
        Some(prefix) => s.starts_with(prefix),
        None => s == p,
    };
    let patterns = [
        "rm*",
        "rm -rf*",
        "git:*",
        "git push*",
        "git * main",
        "ls *",
        "*",
        "a*b",
        "./src/*",
        "/src/*",
        "secrets/*",
        ".env",
        "x:*",
    ];
    let subjects = [
        "rm",
        "rm -rf x",
        "git:x",
        "git:",
        "git push",
        "git * main",
        "ls *",
        "ls",
        "a*b",
        "ab",
        "./src/a",
        "/src/a",
        "src/a",
        "secrets/k",
        ".env",
        "x:*",
        "x:y",
        "",
    ];
    for tool in ["shell", "apply_patch", "read_file", "Bash", "Edit", "Read"] {
        for p in patterns {
            let text = format!("{tool}({p})");
            let Ok(rule) = Rule::parse(&text) else {
                // Only the Claude Code names refuse a pattern (a path outside
                // the repository); before, such a rule named no tool at all.
                assert!(
                    claude_code_alias(tool).is_some(),
                    "{text} was refused: {:?}",
                    Rule::parse(&text)
                );
                continue;
            };
            for s in subjects {
                if old(p, s) {
                    let kgraf = rule.tool.clone();
                    assert!(
                        rule.matches(&kgraf, &[s.to_owned()]),
                        "{text} matched `{s}` before and must still"
                    );
                }
            }
        }
    }
    // A rule on every call, on `*` and on an MCP tool still matches.
    assert!(hits("shell", &shell(&["anything"])));
    assert!(hits("*", &call("mcp__web__fetch", json!({}))));
    assert!(hits("mcp__web__fetch", &call("mcp__web__fetch", json!({}))));
}

/// A rule naming no tool is refused where it is read, with the tools a rule
/// may name and a hint where a kGraf tool does the job; so is an allow-set
/// tool, from the user file too.
#[test]
fn a_rule_or_allow_entry_naming_no_tool_is_refused_with_the_known_names() {
    for (rule, hint) in [
        ("bash(rm*)", Some("shell")),
        ("write_file(src/*)", Some("apply_patch")),
        ("Shell", Some("shell")),
        ("WebFetch", None),
        ("Bah(rm*)", None),
        ("Task", None),
    ] {
        let e = Rule::parse(rule).expect_err(rule);
        assert!(e.contains(&format!("`{rule}`")), "{e}");
        assert!(e.contains("which is no kGraf tool"), "{e}");
        for known in [
            "apply_patch",
            "read_file",
            "shell",
            "Bash as shell",
            "mcp__<server>",
        ] {
            assert!(e.contains(known), "{rule}: {e}");
        }
        if let Some(h) = hint {
            assert!(e.contains(&format!("(use `{h}`)")), "{e}");
        }
    }
    let e = PermissionSettings::from_toml_str("deny = [\"Bah(rm*)\"]").expect_err("file deny");
    assert!(e.contains("Bah"), "{e}");
    let e = PermissionSettings::from_toml_str("ask = [\"WebSearch\"]").expect_err("file ask");
    assert!(e.contains("WebSearch"), "{e}");
    let e = PermissionSettings::from_toml_str("[allow]\ntools = [\"fetch\"]").expect_err("allow");
    assert!(e.contains("allow-set tool `fetch`"), "{e}");
    for wide in ["*", "mcp__*", "sh*"] {
        let toml = format!("[allow]\ntools = [\"{wide}\"]");
        let e = PermissionSettings::from_toml_str(&toml).expect_err(wide);
        assert!(e.contains("wildcard"), "{wide}: {e}");
    }
}

/// A rule whose pattern kGraf could never match is refused: a pattern on a
/// tool whose calls name no command or path, a Claude Code parameter rule,
/// a path outside the repository, and a carve-out.
#[test]
fn a_pattern_that_can_never_match_is_refused() {
    for (rule, says) in [
        ("graph_slice(src/*)", "names no command or path"),
        ("report_shortfall(x)", "names no command or path"),
        ("Bash(run_in_background:true)", "parameter rule"),
        ("Bash(command:rm *)", "parameter rule"),
        ("Edit(//etc/passwd)", "outside the repository"),
        ("Read(~/.ssh/*)", "outside the repository"),
        ("Edit(!docs/**)", "carve-out"),
        ("Edit(/)", "names no path"),
    ] {
        let e = Rule::parse(rule).expect_err(rule);
        assert!(e.contains(says), "{rule}: {e}");
    }
    // `timeout:*` is Claude Code's trailing wildcard, not a parameter rule.
    assert!(hits("Bash(timeout:*)", &shell(&["timeout", "5", "x"])));
    // A kGraf file rule keeps its own reading of an absolute path.
    assert!(Rule::parse("apply_patch(/abs/*)").is_ok());
}

/// An MCP name is accepted where it is read; `mcp__<server>` and
/// `mcp__<server>__*` cover that server's tools only, `mcp__*` every MCP
/// tool; an allow-set entry for a server grants its tools in Auto.
#[test]
fn mcp_names_are_accepted_and_cover_their_server() {
    let gh = call("mcp__gh__issue", json!({}));
    let ghx = call("mcp__ghx__issue", json!({}));
    assert!(hits("mcp__gh", &gh));
    assert!(!hits("mcp__gh", &ghx));
    assert!(hits("mcp__gh__*", &gh));
    assert!(!hits("mcp__gh__*", &ghx));
    assert!(hits("mcp__*", &ghx));
    assert!(!hits("mcp__*", &shell(&["ls"])));
    assert!(hits("*__issue", &gh));
    assert!(hits("sh*", &shell(&["ls"])));
    assert!(Rule::parse("mcp__").is_err());
    assert!(
        Rule::parse("Bas*").is_err(),
        "a glob that can match no tool"
    );

    let s = PermissionSettings::from_toml_str(
        "mode = \"auto\"\n[allow]\ntools = [\"mcp__gh\", \"Bash\", \"mcp__web__fetch\"]",
    )
    .expect("allow");
    assert!(s.allow.tools.contains("shell"), "{:?}", s.allow.tools);
    let c = PermissionControl::new(s, ModeSource::CommandLine, ProcessFacts::default())
        .expect("control");
    for tool in ["mcp__gh__issue", "mcp__gh__pr", "mcp__web__fetch", "shell"] {
        assert!(
            matches!(c.route_ask(tool, &json!({})), AskRoute::Grant { .. }),
            "{tool}"
        );
    }
    assert!(!matches!(
        c.route_ask("mcp__web__search", &json!({})),
        AskRoute::Grant { .. }
    ));
}

/// Claude Code path rules on `Edit`, `Write` and `Read` match repository
/// paths: a relative pattern at any depth, `/` and `./` at the root.
#[test]
fn claude_code_path_rules_match_repository_paths() {
    assert!(hits("Edit(.env)", &patch(".env")));
    assert!(hits("Edit(.env)", &patch("config/.env")));
    assert!(hits("Write(secrets/**)", &patch("a/secrets/k.txt")));
    assert!(hits("Edit(/src/**)", &patch("src/a.rs")));
    assert!(!hits("Edit(/src/**)", &patch("vendor/src/a.rs")));
    assert!(hits("Edit(./src/*)", &patch("src/a.rs")));
    assert!(!hits("Edit(./src/*)", &patch("lib/src/a.rs")));
    assert!(hits("Edit(src/**/*.ts)", &patch("src/a/b.ts")));
    assert!(!hits("Edit(src/**/*.ts)", &patch("src/a/b.rs")));
    assert!(hits(
        "Read(.env)",
        &call("read_file", json!({"path": "app/.env"}))
    ));
    assert!(!hits("Read(.env)", &patch(".env")), "Read is not Edit");
    // The user's text is kept for the refusal and the file.
    let r = Rule::parse("Edit(./src/*)").expect("rule");
    assert_eq!(r.text(), "Edit(./src/*)");
    let s = PermissionSettings::from_toml_str("deny = [\"Bash(rm:*)\", \"Edit(./src/*)\"]")
        .expect("file");
    let again =
        PermissionSettings::from_toml_str(&s.to_toml_string().expect("ser")).expect("round trip");
    assert_eq!(again, s);
}

/// At run start, a rule or allow-set tool naming none of the workers' tools
/// is said with the rule as written; one naming a tool they have is not.
#[test]
fn unmatched_entries_name_each_rule_that_names_no_worker_tool() {
    let s = PermissionSettings::from_toml_str(
        "deny = [\"mcp__gh__delete_repo\", \"Bash(rm*)\", \"mcp__echo__echo\"]\n\
         ask = [\"mcp__other\", \"decide\"]\n[allow]\ntools = [\"mcp__gone__x\", \"read_file\"]",
    )
    .expect("file");
    let tools: Vec<String> = ["shell", "read_file", "apply_patch", "mcp__echo__echo"]
        .iter()
        .map(|t| (*t).to_owned())
        .collect();
    let said = unmatched_entries(&s, &tools);
    assert_eq!(said.len(), 4, "{said:#?}");
    assert!(said[0].starts_with("your deny rule `mcp__gh__delete_repo` names no tool"));
    assert!(
        said[0].contains("blocks nothing in this run"),
        "{}",
        said[0]
    );
    assert!(said[0].ends_with("apply_patch, mcp__echo__echo, read_file, shell"));
    assert!(
        said[1].starts_with("your ask rule `mcp__other`"),
        "{}",
        said[1]
    );
    assert!(said[2].starts_with("your ask rule `decide`"), "{}", said[2]);
    assert!(
        said[3].starts_with("your allow-set tool `mcp__gone__x`"),
        "{}",
        said[3]
    );
    assert!(unmatched_entries(&PermissionSettings::default(), &tools).is_empty());
}

#[test]
fn glob_matches_any_text_at_a_star_and_bytes_elsewhere() {
    use permission_names::glob;
    assert!(glob("a*c", "abbbc"));
    assert!(glob("a*c", "ac"));
    assert!(!glob("a*c", "acb"));
    assert!(glob("*", ""));
    assert!(glob("**", "x/y"));
    assert!(glob("a*b*c", "a_b_b_c"));
    assert!(!glob("a*b*c", "a_c"));
    assert!(glob("é*", "éa"));
    assert!(!glob("", "x"));
}
