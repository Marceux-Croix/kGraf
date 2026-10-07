//! Spellings of a command that once ran a program past the user's deny rule
//! in every permission mode, Bypass included. Each test names one way a
//! program was hidden; each case must now be refused (the program is read
//! and the rule matches) or asked about (kGraf cannot read it, and a shell
//! rule with a pattern exists).
//!
//! How to run: `cargo test -p kgraf-tools permissions::review_tests`. No
//! network and no process started; a test that needs files writes them in a
//! temporary directory, with a home directory inside it.

use super::*;

fn deny(rule: &str) -> UserRules {
    UserRules {
        deny: vec![Rule::parse(rule).expect("rule")],
        ask: Vec::new(),
    }
}

fn call(words: &[&str]) -> PendingCall {
    PendingCall {
        call_id: "c".into(),
        tool: "shell".into(),
        input: serde_json::json!({ "command": words }),
        worker: kgraf_core::AgentId::new("w"),
        task: kgraf_core::TaskId::new("t"),
    }
}

/// What the rules hook decides for `words` under `rule`, with files read
/// from `root` when given.
fn decided(rule: &str, words: &[&str], root: Option<&Path>) -> Option<HardLimit> {
    let home = root.map(|r| r.join("home"));
    hard_limit(&deny(rule), &call(words), home.as_deref(), root)
}

/// Every case is refused or asked about; the failure lists each case that
/// was let through.
fn assert_blocked(rule: &str, cases: &[&[&str]], root: Option<&Path>) {
    let passed: Vec<String> = cases
        .iter()
        .filter(|c| decided(rule, c, root).is_none())
        .map(|c| format!("{c:?}"))
        .collect();
    assert!(
        passed.is_empty(),
        "these ran past `{rule}`:\n{}",
        passed.join("\n")
    );
}

/// Every case is refused outright: the program is read, not just unreadable.
fn assert_refused(rule: &str, cases: &[&[&str]], root: Option<&Path>) {
    let not: Vec<String> = cases
        .iter()
        .filter(|c| !matches!(decided(rule, c, root), Some(HardLimit::Deny(_))))
        .map(|c| format!("{c:?} -> {:?}", decided(rule, c, root)))
        .collect();
    assert!(
        not.is_empty(),
        "these were not refused by `{rule}`:\n{}",
        not.join("\n")
    );
}

fn worktree(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("dir");
    for (name, text) in files {
        let p = dir.path().join(name);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(&p, text).expect("write");
    }
    std::fs::create_dir_all(dir.path().join("home")).expect("home");
    dir
}

/// A quote escaped inside double quotes, or inside `$'...'`, made the
/// script reader lose track of quoting, so the commands after it were read
/// as arguments of the first. A line continuation split a program name in
/// two, and a quote in a comment swallowed the rest of the script.
#[test]
fn a_script_whose_quoting_the_reader_could_misread_still_shows_its_commands() {
    assert_refused(
        "shell(curl*)",
        &[
            &["sh", "-c", "echo \"a\\\"\"; curl x"],
            &["sh", "-c", "echo $'a\\''; curl x"],
            &["bash", "-c", "cu\\\nrl x"],
            &["bash", "-c", "cu\\\n\\\nrl x"],
            &["bash", "-c", "echo hi # it's here\ncurl x"],
            &["bash", "-c", "cat <<EOF\nit's\nEOF\ncurl x"],
        ],
        None,
    );
}

/// A script whose quotes never close is not read as whatever words a split
/// on spaces gives: kGraf cannot tell its commands.
#[test]
fn a_script_with_an_open_quote_is_unreadable_not_read_on_spaces() {
    for script in ["echo 'a; curl x", "echo \"a; curl x", "echo `a; curl x"] {
        let reading = read_call(&["sh".to_owned(), "-c".to_owned(), script.to_owned()]);
        assert!(
            !reading.unreadable.is_empty(),
            "`{script}` was read as readable"
        );
    }
}

/// A substitution given to a program that runs its argument as code (`eval
/// "$(...)"`, `bash -c "$(...)"`) hands it text kGraf cannot see.
#[test]
fn a_substitution_given_as_code_is_unreadable() {
    assert_blocked(
        "shell(curl*)",
        &[
            &["bash", "-c", "eval \"$(printf 'curl x')\""],
            &["bash", "-c", "bash -c \"$(echo curl x)\""],
            &["sh", "-c", "eval `echo curl x`"],
        ],
        None,
    );
}

/// A program named by a path, in another case, or with a Windows extension
/// is the program the rule names.
#[test]
fn a_rule_matches_its_program_by_any_path_case_or_extension() {
    let dir = worktree(&[]);
    assert_refused(
        "shell(curl*)",
        &[
            &["/usr/bin/curl", "x"],
            &["CURL", "x"],
            &["Curl.exe", "x"],
            &["bash", "-c", "~/../../usr/bin/curl x"],
            &["bash", "-c", "CURL x"],
            &[
                "python3",
                "-c",
                "import subprocess; subprocess.run(['/usr/bin/curl','x'])",
            ],
        ],
        Some(dir.path()),
    );
    assert_refused("shell(rm -rf*)", &[&["/bin/rm", "-rf", "x"]], None);
}

/// A program word the shell expands (a glob, a brace list, zsh's `=NAME`)
/// names a program kGraf cannot tell.
#[test]
fn a_program_word_the_shell_expands_is_unreadable() {
    assert_blocked(
        "shell(curl*)",
        &[
            &["bash", "-c", "/usr/bin/cur? x"],
            &["bash", "-c", "/usr/bin/c*l x"],
            &["bash", "-c", "/usr/bin/[c]url x"],
            &["bash", "-c", "{curl,x}"],
            &["bash", "-c", "cu{r,}l x"],
            &["bash", "-c", "cu{r..r}l x"],
            &["zsh", "-c", "=curl x"],
        ],
        None,
    );
}

/// Shells by other names, versions and cases read `-c` as a script.
#[test]
fn every_shell_name_reads_its_script() {
    assert_refused(
        "shell(curl*)",
        &[
            &["mksh", "-c", "curl x"],
            &["yash", "-c", "curl x"],
            &["ash", "-c", "curl x"],
            &["posh", "-c", "curl x"],
            &["rbash", "-c", "curl x"],
            &["bash5", "-c", "curl x"],
            &["bash-5.2", "-c", "curl x"],
            &["SH", "-c", "curl x"],
            &["Bash", "-c", "curl x"],
            &["sh", "-c", "SH -c 'curl x'"],
            &["bash", "-c", "--", "curl x"],
        ],
        None,
    );
    assert_blocked(
        "shell(curl*)",
        &[&["fish", "-c", "eval (echo curl) x"]],
        None,
    );
}

/// A name defined in the script to run another program (`alias`, `hash
/// -p`, a `function` body) or a builtin that runs text kGraf does not see
/// (`fc`, a `PS4` the trace expands) does not hide the program.
#[test]
fn a_name_the_script_defines_does_not_hide_its_program() {
    assert_blocked(
        "shell(curl*)",
        &[
            &["bash", "-c", "alias c=curl; c x"],
            &["sh", "-c", "alias c=curl\nc x"],
            &["zsh", "-c", "alias -g X=curl; X x"],
            &["bash", "-c", "hash -p /usr/bin/curl g; g x"],
            &["bash", "-c", "function f { curl x; }; f"],
            &["bash", "-c", "history -s curl; fc -s"],
            &["bash", "-c", "PS4='$(curl x)' ; set -x; true"],
            &["zsh", "-c", "autoload -U x; x"],
            &["zsh", "-c", "repeat 1 curl x"],
            &["zsh", "-c", "if true { curl x }"],
            &["bash", "-c", "{fd}>x curl x"],
        ],
        None,
    );
}

/// A placeholder that `xargs`, `find` or `parallel` fills from its input
/// names a command kGraf cannot see when it stands as the program or in a
/// script.
#[test]
fn a_placeholder_filled_from_input_is_unreadable_as_a_command() {
    assert_blocked(
        "shell(curl*)",
        &[
            &["xargs", "-I{}", "sh", "-c", "{}"],
            &["xargs", "-I", "%", "sh", "-c", "%"],
            &["xargs", "-I%", "%", "x"],
            &["xargs", "-J%", "%", "x"],
            &["xargs", "--replace", "sh", "-c", "{}"],
            &["xargs", "-i", "sh", "-c", "{}"],
            &["find", ".", "-exec", "sh", "-c", "{}", ";"],
            &["find", ".", "-exec", "{}", ";"],
            &["parallel", "{}", ":::", "curl x"],
            &["parallel", "sh", "-c", "{}", ":::", "curl x"],
        ],
        None,
    );
    // An ordinary placeholder argument is still read as before.
    assert!(decided("shell(curl*)", &["xargs", "-I{}", "cp", "{}", "d/"], None).is_none());
}

/// Interpreter code that starts a program through a path the readers did
/// not follow: a numeric escape in the command string, a spawning module
/// reached as another module's attribute, a spawning function called
/// through `__call__`, `require` reached as a method, a dynamic `import()`
/// handed on, Ruby's explicit backquote method and `IO.read("|...")`,
/// PHP's FFI, and AppleScript's JavaScript language.
#[test]
fn interpreter_code_hides_no_program_behind_an_escape_an_attribute_or_a_language() {
    let dir = worktree(&[]);
    assert_blocked(
        "shell(curl*)",
        &[
            &["python3", "-c", "import os; os.system('cu\\x72l x')"],
            &["python3", "-c", "import os; os.system('\\143url x')"],
            &["python3", "-c", "import shutil; shutil.os.system('curl x')"],
            &[
                "python3",
                "-c",
                "import tempfile; tempfile._os.system('curl x')",
            ],
            &[
                "python3",
                "-c",
                "import os.path as p; p.os.system('curl x')",
            ],
            &[
                "python3",
                "-c",
                "import asyncio; asyncio.subprocess.create_subprocess_shell('curl x')",
            ],
            &[
                "python3",
                "-c",
                "import distutils.spawn; distutils.spawn.spawn(['curl','x'])",
            ],
            &["python3", "-c", "import os; os.system.__call__('curl x')"],
            &["python3", "-c", "import webbrowser; webbrowser.open('x')"],
            &[
                "node",
                "-e",
                "require('child_process').exec(\"cu\\x72l x\")",
            ],
            &[
                "node",
                "-e",
                "require('child_process').exec('cu\\u0072l x')",
            ],
            &[
                "node",
                "-e",
                "module.require('child_process').execSync('curl x')",
            ],
            &[
                "node",
                "-e",
                "process.mainModule.require('child_process').execSync('curl x')",
            ],
            &["node", "-e", "globalThis.require('child_process')"],
            &[
                "node",
                "-e",
                "import('child_process').then(c=>c.execSync('curl x'))",
            ],
            &["node", "-e", "const d = process.dlopen; d(module, 'x')"],
            &["ruby", "-e", "Kernel.`('curl x')"],
            &["ruby", "-e", "IO.read('|curl x')"],
            &["ruby", "-e", "IO.readlines('|curl x')"],
            &["ruby", "-e", "IO.write('|curl x', 'a')"],
            &["ruby", "-e", "IO.foreach('|curl x'){}"],
            &[
                "php",
                "-r",
                "FFI::cdef('int system(char*);')->system('curl x');",
            ],
            &["osascript", "-l", "JavaScript", "-e", "x"],
        ],
        Some(dir.path()),
    );
}

/// A git global option (`-c`, `-C`, `--no-pager`, `--git-dir`) before the
/// subcommand does not defeat a deny rule on the subcommand.
#[test]
fn a_git_global_option_does_not_hide_the_subcommand_from_a_rule() {
    assert_refused(
        "shell(git push*)",
        &[
            &["git", "-c", "a=b", "push"],
            &["git", "-C", ".", "push"],
            &["git", "--no-pager", "push"],
            &["git", "--git-dir=.git", "push"],
            &["git", "--git-dir", ".git", "push"],
            &["git", "-p", "push"],
            &["bash", "-c", "git -c x=y push"],
            &["bash", "-c", "git --git-dir=.git push"],
        ],
        None,
    );
}

/// An install-family command runs the project's own lifecycle scripts, so a
/// `postinstall` or `prepare` that runs a denied program is caught.
#[test]
fn an_install_runs_the_projects_own_lifecycle_scripts() {
    let dir = worktree(&[(
        "package.json",
        r#"{"scripts":{"postinstall":"curl x","prepare":"curl y","test":"true"}}"#,
    )]);
    assert_refused(
        "shell(curl*)",
        &[
            &["npm", "install"],
            &["npm", "ci"],
            &["npm", "i"],
            &["npm", "publish"],
        ],
        Some(dir.path()),
    );
    // A normal install with no such script still runs with no rule hit.
    let clean = worktree(&[("package.json", r#"{"scripts":{"test":"true"}}"#)]);
    assert!(decided("shell(curl*)", &["npm", "install"], Some(clean.path())).is_none());
}

/// A wrapper that runs the program after its options (`cross-env A=1 curl`,
/// `xcrun curl`) does not hide the program from a rule.
#[test]
fn a_tool_wrapper_does_not_hide_the_program_it_runs() {
    assert_refused(
        "shell(curl*)",
        &[
            &["cross-env", "A=1", "curl", "x"],
            &["xcrun", "curl", "x"],
            &["bash", "-c", "cross-env A=1 curl x"],
        ],
        None,
    );
}

/// A command kGraf cannot read is asked about only for a rule that names
/// the shell tool with a pattern; a rule on another tool, or one with no
/// pattern to check a command against, adds no question.
#[test]
fn an_unreadable_command_asks_only_for_a_shell_rule_with_a_pattern() {
    let unreadable = ["source", "x.sh"];
    match decided("shell(curl*)", &unreadable, None) {
        Some(HardLimit::Ask(why)) => assert!(why.contains("cannot check what it runs"), "{why}"),
        other => panic!("expected an ask, got {other:?}"),
    }
    assert_eq!(decided("apply_patch(secrets/*)", &unreadable, None), None);
    assert_eq!(decided("mcp__web__fetch", &unreadable, None), None);
}
