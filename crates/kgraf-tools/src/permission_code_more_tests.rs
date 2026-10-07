//! Interpreter command lines: each option of each interpreter takes the
//! words it takes and finds the code it names, and the reader of a file
//! looks where the script is. Each case is a command line over files that
//! run `curl x` (or `curl pre`), so a mis-parsed line shows as the wrong
//! command, or as no command, or as unreadable.

use super::*;

fn commands(found: &[Embedded]) -> Vec<String> {
    let mut out = Vec::new();
    for e in found {
        match e {
            Embedded::Script(s) => out.push(s.clone()),
            Embedded::Argv(a) => out.push(a.join(" ")),
            Embedded::In(_, inner) => out.extend(commands(std::slice::from_ref(inner))),
            Embedded::Unreadable(_) => {}
        }
    }
    out
}

fn why(found: &[Embedded]) -> String {
    found
        .iter()
        .filter_map(|e| match e {
            Embedded::Unreadable(w) => Some(w.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn opaque(found: &[Embedded]) -> bool {
    !why(found).is_empty()
}

fn files(files: &[(&str, &str)]) -> (tempfile::TempDir, ReadContext) {
    let dir = tempfile::tempdir().expect("dir");
    for (name, text) in files {
        let p = dir.path().join(name);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(p, text).expect("write");
    }
    let ctx = ReadContext::in_worktree(dir.path(), None);
    (dir, ctx)
}

const PY: &str = "import os\nos.system('curl x')\n";
const JS: &str = "require('child_process').execSync('curl x')\n";
const PRE: &str = "require('child_process').execSync('curl pre')\n";
const RB: &str = "system('curl x')\n";
const PL: &str = "system('curl x');\n";
const PHP: &str = "<?php system('curl x');\n";
const AWK: &str = "BEGIN { system(\"curl x\") }\n";

fn world() -> (tempfile::TempDir, ReadContext) {
    files(&[
        ("x.py", PY),
        ("x.js", JS),
        ("main.js", JS),
        ("pre.js", PRE),
        ("-weird.js", JS),
        ("-x.php", PHP),
        ("x.rb", RB),
        ("lib.rb", "system('curl lib')\n"),
        ("x.pl", PL),
        ("x.php", PHP),
        ("x.awk", AWK),
        ("x.ts", JS),
        ("x.scpt", "do shell script \"curl x\"\n"),
        ("a.test.js", JS),
        ("mymod.py", PY),
        ("pkg/sub.py", PY),
        ("pkg2/__main__.py", PY),
        ("deno.json", "{}"),
    ])
}

type Call = Option<Vec<Embedded>>;

fn call(ctx: &ReadContext, key: &str, args: &[&str]) -> Call {
    let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    interpreter_call(interpreter(key).expect("interpreter"), key, &args, ctx)
}

#[track_caller]
fn runs(ctx: &ReadContext, key: &str, args: &[&str], want: &[&str]) {
    let found = call(ctx, key, args).unwrap_or_else(|| panic!("{key} {args:?}: nothing read"));
    assert!(!opaque(&found), "{key} {args:?}: {}", why(&found));
    assert_eq!(commands(&found), want, "{key} {args:?}");
}

#[track_caller]
fn refused(ctx: &ReadContext, key: &str, args: &[&str]) -> String {
    let found = call(ctx, key, args).unwrap_or_else(|| panic!("{key} {args:?}: nothing read"));
    assert!(opaque(&found), "{key} {args:?}: {:?}", commands(&found));
    why(&found)
}

#[track_caller]
fn nothing(ctx: &ReadContext, key: &str, args: &[&str]) {
    assert!(call(ctx, key, args).is_none(), "{key} {args:?}");
}

#[test]
fn a_language_tag_keeps_the_scans_of_different_languages_apart() {
    // One text, three languages: the cache is keyed by the language.
    let text = "system('curl tagtest')\n";
    let ruby = read_code(
        "x",
        Lang::Ruby,
        text,
        &Origin {
            ctx: ReadContext::default(),
        },
    );
    let python = read_code(
        "x",
        Lang::Python,
        text,
        &Origin {
            ctx: ReadContext::default(),
        },
    );
    let php = read_code(
        "x",
        Lang::PhpOnly,
        "system('curl tagtest');\n",
        &Origin {
            ctx: ReadContext::default(),
        },
    );
    assert_eq!(commands(&ruby), vec!["curl tagtest"]);
    assert!(commands(&python).is_empty(), "{python:?}");
    assert_eq!(commands(&php), vec!["curl tagtest"]);
    let tags: std::collections::BTreeSet<&str> = [
        Lang::Python,
        Lang::JavaScript,
        Lang::TypeScript,
        Lang::Tsx,
        Lang::Ruby,
        Lang::Php,
        Lang::PhpOnly,
        Lang::Perl,
        Lang::Awk,
        Lang::AppleScript,
    ]
    .iter()
    .map(|l| l.tag())
    .collect();
    assert_eq!(tags.len(), 10);
    assert!(!tags.contains(""));
    assert!(!tags.contains("xyzzy"));
    assert_eq!(Lang::PhpOnly.tag(), "php-only");
}

#[test]
fn a_javascript_file_is_read_in_the_language_its_extension_names() {
    for (name, lang) in [
        ("a.js", Lang::JavaScript),
        ("a.mjs", Lang::JavaScript),
        ("a.cjs", Lang::JavaScript),
        ("a.jsx", Lang::JavaScript),
        ("a", Lang::JavaScript),
        ("a.ts", Lang::TypeScript),
        ("a.mts", Lang::TypeScript),
        ("a.cts", Lang::TypeScript),
        ("A.TS", Lang::TypeScript),
        ("a.tsx", Lang::Tsx),
        ("A.TSX", Lang::Tsx),
        ("dir.ts/a.js", Lang::JavaScript),
    ] {
        assert_eq!(Lang::of_js_file(name), lang, "{name}");
    }
}

#[test]
fn a_program_name_is_an_interpreter_by_family_and_version() {
    for (key, want) in [
        ("python", Some(Interp::Python)),
        ("python3", Some(Interp::Python)),
        ("python3.12", Some(Interp::Python)),
        ("pypy", Some(Interp::Python)),
        ("pypy3", Some(Interp::Python)),
        ("py", Some(Interp::Python)),
        ("pythonx", None),
        ("pypyx", None),
        ("pyx", None),
        ("node", Some(Interp::Node)),
        ("nodejs", Some(Interp::Node)),
        ("tsx", Some(Interp::TsRunner)),
        ("ts-node", Some(Interp::TsRunner)),
        ("perl", Some(Interp::Perl)),
        ("perl5.38", Some(Interp::Perl)),
        ("ruby", Some(Interp::Ruby)),
        ("ruby3.2", Some(Interp::Ruby)),
        ("php", Some(Interp::Php)),
        ("php8.2", Some(Interp::Php)),
        ("php-cgi", None),
        ("awk", Some(Interp::Awk)),
        ("gawk", Some(Interp::Awk)),
        ("osascript", Some(Interp::Osascript)),
        ("lua", Some(Interp::Unread)),
        ("lua5.4", Some(Interp::Unread)),
        ("julia", Some(Interp::Unread)),
        ("rscript", Some(Interp::Unread)),
        ("jshell", Some(Interp::Unread)),
        ("luax", None),
        ("ls", None),
    ] {
        assert_eq!(interpreter(key), want, "{key}");
    }
}

#[test]
fn an_interpreter_with_no_reader_is_unreadable_unless_asked_its_version() {
    let (_d, ctx) = world();
    for key in ["lua", "julia", "xonsh"] {
        assert!(opaque(&call(&ctx, key, &[]).expect("read")), "{key}");
        assert!(opaque(&call(&ctx, key, &["x.lua"]).expect("read")));
        assert!(opaque(&call(&ctx, key, &["--version", "x"]).expect("read")));
        nothing(&ctx, key, &["--version"]);
        nothing(&ctx, key, &["-h"]);
        nothing(&ctx, key, &["-version"]);
        nothing(&ctx, key, &["-v", "--help"]);
    }
}

#[test]
fn a_typescript_runner_reads_the_first_file_it_is_given() {
    let (_d, ctx) = world();
    runs(&ctx, "tsx", &["x.ts"], &["curl x"]);
    runs(&ctx, "tsx", &["--watch", "x.ts"], &["curl x"]);
    runs(&ctx, "ts-node", &["-T", "x.ts", "arg"], &["curl x"]);
    nothing(&ctx, "tsx", &[]);
    nothing(&ctx, "tsx", &["--version"]);
    assert!(opaque(&call(&ctx, "tsx", &["missing.ts"]).expect("read")));
}

#[test]
fn a_script_file_run_by_bun_is_read_as_code() {
    let (_d, ctx) = world();
    assert_eq!(commands(&script_file_call("bun", "x.js", &ctx)), ["curl x"]);
    assert_eq!(commands(&script_file_call("bun", "x.ts", &ctx)), ["curl x"]);
    assert!(opaque(&script_file_call("bun", "nope.js", &ctx)));
}

// ----------------------------------------------------------------- python

#[test]
fn python_takes_its_options_and_finds_the_script() {
    let (_d, ctx) = world();
    runs(&ctx, "python3", &["x.py"], &["curl x"]);
    runs(&ctx, "python3", &["-B", "x.py"], &["curl x"]);
    runs(&ctx, "python3", &["-u", "-B", "--", "x.py"], &["curl x"]);
    // After `--` a word that starts with `-` is the script.
    assert!(opaque(
        &call(&ctx, "python3", &["-B", "--", "-x.py"]).expect("read")
    ));
    runs(
        &ctx,
        "python3",
        &["-B", "--check-hash-based-pycs", "always", "x.py"],
        &["curl x"],
    );
    runs(&ctx, "python3", &["-B", "--unknown", "x.py"], &["curl x"]);
    // `-W`, `-X` and `-Q` take a word, joined or apart.
    runs(
        &ctx,
        "python3",
        &["-B", "-W", "ignore", "x.py"],
        &["curl x"],
    );
    runs(&ctx, "python3", &["-B", "-Wignore", "x.py"], &["curl x"]);
    runs(&ctx, "python3", &["-X", "dev", "x.py"], &["curl x"]);
    runs(&ctx, "python3", &["-Xdev", "x.py"], &["curl x"]);
    runs(&ctx, "python3", &["-Q", "new", "x.py"], &["curl x"]);
    runs(&ctx, "python3", &["-BW", "ignore", "x.py"], &["curl x"]);
    runs(&ctx, "python3", &["-BWignore", "x.py"], &["curl x"]);
    // `-c` and `-m` take the code or module joined or apart.
    runs(
        &ctx,
        "python3",
        &["-c", "import os; os.system('curl x')"],
        &["curl x"],
    );
    runs(
        &ctx,
        "python3",
        &["-Bcimport os; os.system('curl x')"],
        &["curl x"],
    );
    runs(&ctx, "python3", &["-m", "mymod"], &["curl x"]);
    runs(&ctx, "python3", &["-mmymod"], &["curl x"]);
    runs(&ctx, "python3", &["-m", "pkg.sub"], &["curl x"]);
    runs(&ctx, "python3", &["-m", "pkg2"], &["curl x"]);
    // An installed module is no local file; kGraf reads nothing of it.
    runs(&ctx, "python3", &["-m", "pytest"], &[]);
    // `-` and a missing word.
    assert!(why(&call(&ctx, "python3", &["-B", "-"]).expect("read")).contains("from its input"));
    assert!(why(&call(&ctx, "python3", &["-c"]).expect("read")).contains("names no code"));
    assert!(why(&call(&ctx, "python3", &["-m"]).expect("read")).contains("names no code"));
    assert!(why(&call(&ctx, "python3", &["-B"]).expect("read")).contains("from its input"));
    // `-i` keeps reading code from the terminal.
    let text = refused(&ctx, "python3", &["-i", "x.py"]);
    assert!(text.contains("more code from its input"), "{text}");
    let text = refused(&ctx, "python3", &["-i", "-c", "pass"]);
    assert!(text.contains("more code from its input"), "{text}");
    let text = refused(&ctx, "python3", &["-ic", "pass"]);
    assert!(text.contains("more code from its input"), "{text}");
    // Version and help questions read nothing.
    nothing(&ctx, "python3", &["--version"]);
    nothing(&ctx, "python3", &["-V"]);
    nothing(&ctx, "python3", &["-h"]);
}

#[test]
fn a_python_module_with_no_directory_is_unreadable() {
    let ctx = ReadContext::default();
    let found = python_module("python3", "pytest", &ctx);
    assert!(why(&found).contains("cannot tell whether it is a local one"));
    let (_d, ctx) = world();
    assert_eq!(
        commands(&python_module("python3", "mymod", &ctx)),
        ["curl x"]
    );
    assert_eq!(
        commands(&python_module("python3", "pkg.sub", &ctx)),
        ["curl x"]
    );
    assert_eq!(
        commands(&python_module("python3", "pkg2", &ctx)),
        ["curl x"]
    );
    assert!(python_module("python3", "pytest", &ctx).is_empty());
}

// ------------------------------------------------------------------- node

#[test]
fn node_takes_its_options_and_finds_the_script() {
    let (_d, ctx) = world();
    runs(&ctx, "node", &["x.js"], &["curl x"]);
    runs(&ctx, "node", &["--no-warnings", "x.js"], &["curl x"]);
    runs(
        &ctx,
        "node",
        &["--no-warnings", "-C", "dev", "x.js"],
        &["curl x"],
    );
    runs(&ctx, "node", &["--", "x.js"], &["curl x"]);
    runs(&ctx, "node", &["-q", "--", "x.js"], &["curl x"]);
    // After `--` a word that starts with `-` is the script.
    runs(&ctx, "node", &["--", "-weird.js"], &["curl x"]);
    // Options that take a word apart from the option.
    for option in [
        "--input-type",
        "--env-file",
        "--title",
        "--conditions",
        "-C",
        "--stack-size",
        "--inspect-port",
        "--experimental-default-type",
        "--watch-path",
        "--test-reporter",
        "--test-reporter-destination",
        "--test-name-pattern",
    ] {
        runs(
            &ctx,
            "node",
            &["--no-warnings", option, "value", "x.js"],
            &["curl x"],
        );
        runs(&ctx, "node", &[option, "value", "x.js"], &["curl x"]);
    }
    // A `-` reads code from input, even when a file follows.
    assert!(why(&call(&ctx, "node", &["-", "x.js"]).expect("read")).contains("from its input"));
    assert!(why(&call(&ctx, "node", &[]).expect("read")).contains("from its input"));
    assert!(why(&call(&ctx, "node", &["--no-warnings"]).expect("read")).contains("from its input"));
    // `-c` and `--check` read nothing: the syntax is checked, not run.
    nothing(&ctx, "node", &["-c", "x.js"]);
    nothing(&ctx, "node", &["--check", "x.js"]);
    // Code in the command line, in each spelling.
    let code = "require('child_process').execSync('curl x')";
    for flag in ["-e", "--eval", "-p", "--print"] {
        runs(&ctx, "node", &[flag, code], &["curl x"]);
    }
    for flag in ["--eval", "--print"] {
        runs(&ctx, "node", &[&format!("{flag}={code}")], &["curl x"]);
    }
    for flag in ["-e", "--eval", "-p", "--print"] {
        assert!(why(&call(&ctx, "node", &[flag]).expect("read")).contains("names no code"));
    }
    // Modules loaded first are read when they are local files.
    for flag in ["-r", "--require", "--import"] {
        runs(
            &ctx,
            "node",
            &[flag, "./pre.js", "main.js"],
            &["curl pre", "curl x"],
        );
        runs(
            &ctx,
            "node",
            &["-q", flag, "./pre.js", "main.js"],
            &["curl pre", "curl x"],
        );
        runs(&ctx, "node", &[flag, "pkg", "main.js"], &["curl x"]);
    }
    let absolute = format!("{}/pre.js", ctx.root.as_ref().expect("root").display());
    runs(
        &ctx,
        "node",
        &["-r", &absolute, "main.js"],
        &["curl pre", "curl x"],
    );
    for flag in ["--require", "--import"] {
        runs(
            &ctx,
            "node",
            &[&format!("{flag}=./pre.js"), "main.js"],
            &["curl pre", "curl x"],
        );
        runs(
            &ctx,
            "node",
            &[&format!("{flag}={absolute}"), "main.js"],
            &["curl pre", "curl x"],
        );
        runs(
            &ctx,
            "node",
            &[&format!("{flag}=pkg"), "main.js"],
            &["curl x"],
        );
        runs(
            &ctx,
            "node",
            &["-q", &format!("{flag}=./pre.js"), "main.js"],
            &["curl pre", "curl x"],
        );
    }
    // A loader replaces how files load.
    for args in [
        &["--loader", "x", "main.js"][..],
        &["--experimental-loader", "x", "main.js"],
        &["--loader=x", "main.js"],
        &["--experimental-loader=x", "main.js"],
    ] {
        assert!(why(&call(&ctx, "node", args).expect("read")).contains("module loader"));
    }
}

#[test]
fn node_test_reads_the_files_named_after_it() {
    let (_d, ctx) = world();
    runs(&ctx, "node", &["--test", "a.test.js"], &["curl x"]);
    runs(
        &ctx,
        "node",
        &["--test", "a.test.js", "main.js"],
        &["curl x", "curl x"],
    );
    runs(
        &ctx,
        "node",
        &["--test", "--no-warnings", "a.test.js"],
        &["curl x"],
    );
    runs(
        &ctx,
        "node",
        &["--title", "t", "--test", "a.test.js"],
        &["curl x"],
    );
    runs(&ctx, "node", &["--test"], &[]);
    runs(&ctx, "node", &["--test", "--no-warnings"], &[]);
}

// ------------------------------------------------------------------- deno

#[test]
fn deno_reads_eval_run_serve_and_files() {
    let (_d, ctx) = world();
    let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let d = |a: &[&str]| deno(&args(a), &ctx);
    let code = "new Deno.Command('curl', { args: ['x'] })";
    assert_eq!(commands(&d(&["eval", code]).expect("eval")), ["curl x"]);
    assert_eq!(
        commands(&d(&["eval", "-q", code]).expect("eval")),
        ["curl x"]
    );
    assert!(d(&["eval"]).is_none());
    assert!(d(&["eval", "-q"]).is_none());
    assert_eq!(commands(&d(&["run", "x.ts"]).expect("run")), ["curl x"]);
    assert_eq!(
        commands(&d(&["run", "-A", "x.ts"]).expect("run")),
        ["curl x"]
    );
    assert_eq!(commands(&d(&["serve", "x.ts"]).expect("serve")), ["curl x"]);
    assert_eq!(
        commands(&d(&["run", "--allow-all", "-c", "deno.json", "x.ts"]).expect("run")),
        ["curl x"]
    );
    for valued in [
        "-c",
        "--config",
        "--import-map",
        "--lock",
        "--cert",
        "--location",
        "--seed",
        "--env-file",
        "--node-modules-dir",
        "--watch-exclude",
    ] {
        assert_eq!(
            commands(&d(&["run", "-A", valued, "deno.json", "x.ts"]).expect("run")),
            ["curl x"],
            "{valued}"
        );
    }
    assert!(d(&["run"]).is_none());
    assert!(d(&["run", "-A"]).is_none());
    let found = d(&["run", "https://example.com/x.ts"]).expect("url");
    assert!(why(&found).contains("from a URL"));
    let found = d(&["task", "build"]).expect("task");
    assert!(why(&found).contains("task of `deno.json`"));
    // A file named as the sub-command is run; a flag with a dot, or a
    // sub-command without one, is not.
    assert_eq!(commands(&d(&["x.ts"]).expect("file")), ["curl x"]);
    assert!(d(&["--config=a.json"]).is_none());
    assert!(d(&["test"]).is_none());
    assert!(d(&["fmt", "x.ts"]).is_none());
    assert!(d(&[]).is_none());
}

#[test]
fn bun_inline_code_is_read_and_a_file_is_left_to_the_caller() {
    let (_d, ctx) = world();
    let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let b = |a: &[&str]| bun_inline(&args(a), &ctx);
    let code = "Bun.spawn(['curl', 'x'])";
    for flag in ["-e", "--eval", "-p", "--print"] {
        assert_eq!(commands(&b(&[flag, code]).expect(flag)), ["curl x"]);
        assert_eq!(
            commands(&b(&["--smol", flag, code]).expect(flag)),
            ["curl x"]
        );
        assert!(why(&b(&[flag]).expect(flag)).contains("names no code"));
    }
    assert!(b(&["x.ts"]).is_none());
    assert!(b(&["x.ts", "-e", code]).is_none());
    assert!(b(&["--smol", "x.ts"]).is_none());
    assert!(b(&["--smol"]).is_none());
    assert!(b(&[]).is_none());
}

// ------------------------------------------------------------------- perl

#[test]
fn perl_takes_its_options_and_finds_the_code() {
    let (_d, ctx) = world();
    runs(&ctx, "perl", &["x.pl"], &["curl x"]);
    runs(&ctx, "perl", &["-w", "x.pl"], &["curl x"]);
    runs(&ctx, "perl", &["-w", "--", "x.pl"], &["curl x"]);
    runs(&ctx, "perl", &["-e", "system('curl x')"], &["curl x"]);
    runs(&ctx, "perl", &["-E", "system('curl x')"], &["curl x"]);
    runs(&ctx, "perl", &["-wesystem('curl x')"], &["curl x"]);
    runs(&ctx, "perl", &["-le", "system('curl x')"], &["curl x"]);
    runs(&ctx, "perl", &["-l012e", "system('curl x')"], &["curl x"]);
    runs(&ctx, "perl", &["-0777e", "system('curl x')"], &["curl x"]);
    runs(
        &ctx,
        "perl",
        &["-e", "system('curl a')", "-e", "system('curl b')"],
        &["curl a", "curl b"],
    );
    // The options that take a word, joined or apart.
    runs(&ctx, "perl", &["-I", "lib", "x.pl"], &["curl x"]);
    runs(&ctx, "perl", &["-Ilib", "x.pl"], &["curl x"]);
    runs(&ctx, "perl", &["-w", "-I", "lib", "x.pl"], &["curl x"]);
    runs(&ctx, "perl", &["-F:", "x.pl"], &["curl x"]);
    runs(&ctx, "perl", &["-i", "x.pl"], &["curl x"]);
    runs(&ctx, "perl", &["-ie", "x.pl"], &["curl x"]);
    runs(&ctx, "perl", &["-i.bak", "x.pl"], &["curl x"]);
    runs(&ctx, "perl", &["-C", "x.pl"], &["curl x"]);
    runs(&ctx, "perl", &["-CSD", "x.pl"], &["curl x"]);
    runs(&ctx, "perl", &["-an", "x.pl"], &["curl x"]);
    // Modules that start programs.
    for args in [
        &["-MIPC::Open3", "-e", "1"][..],
        &["-M", "IPC::Open3", "-e", "1"],
        &["-mIPC::Run", "-e", "1"],
        &["-m", "IPC::Run", "-e", "1"],
        &["-M-IPC::Open3", "-e", "1"],
        &["-MShell=x", "-e", "1"],
        &["-w", "-MCapture::Tiny", "-e", "1"],
    ] {
        let text = refused(&ctx, "perl", args);
        assert!(
            text.contains("starts programs kGraf cannot read"),
            "{args:?}: {text}"
        );
    }
    runs(&ctx, "perl", &["-MList::Util", "-e", "1"], &[]);
    runs(&ctx, "perl", &["-M", "strict", "-e", "1"], &[]);
    // Options whose effect is not read.
    for flag in ["-x", "-d", "-S"] {
        let text = refused(&ctx, "perl", &[flag, "x.pl"]);
        assert!(text.contains(&format!("`{flag}`")), "{text}");
    }
    assert!(why(&call(&ctx, "perl", &["-", "x.pl"]).expect("read")).contains("from its input"));
    assert!(why(&call(&ctx, "perl", &["-e"]).expect("read")).contains("names no code"));
    assert!(why(&call(&ctx, "perl", &["-w"]).expect("read")).contains("from its input"));
    nothing(&ctx, "perl", &["-v"]);
}

// ------------------------------------------------------------------- ruby

#[test]
fn ruby_takes_its_options_and_finds_the_code() {
    let (_d, ctx) = world();
    runs(&ctx, "ruby", &["x.rb"], &["curl x"]);
    runs(&ctx, "ruby", &["-w", "x.rb"], &["curl x"]);
    runs(&ctx, "ruby", &["-w", "--", "x.rb"], &["curl x"]);
    runs(&ctx, "ruby", &["--disable-gems", "x.rb"], &["curl x"]);
    runs(&ctx, "ruby", &["-w", "--disable-gems", "x.rb"], &["curl x"]);
    runs(&ctx, "ruby", &["-e", "system('curl x')"], &["curl x"]);
    runs(&ctx, "ruby", &["-esystem('curl x')"], &["curl x"]);
    runs(&ctx, "ruby", &["-w", "-e", "system('curl x')"], &["curl x"]);
    runs(
        &ctx,
        "ruby",
        &["-e", "system('curl a')", "-e", "system('curl b')"],
        &["curl a", "curl b"],
    );
    // Options that take no separate word.
    for flag in ["-W", "-F", "-K", "-T", "-0"] {
        runs(&ctx, "ruby", &[flag, "x.rb"], &["curl x"]);
    }
    runs(&ctx, "ruby", &["-W0", "x.rb"], &["curl x"]);
    runs(&ctx, "ruby", &["-F:", "x.rb"], &["curl x"]);
    // Options that take a word apart from the option.
    runs(&ctx, "ruby", &["-I", "lib", "x.rb"], &["curl x"]);
    runs(&ctx, "ruby", &["-Ilib", "x.rb"], &["curl x"]);
    runs(&ctx, "ruby", &["-E", "utf-8", "x.rb"], &["curl x"]);
    runs(&ctx, "ruby", &["-r", "json", "x.rb"], &["curl x"]);
    runs(&ctx, "ruby", &["-rjson", "x.rb"], &["curl x"]);
    // Libraries required first are read when they are local files.
    runs(
        &ctx,
        "ruby",
        &["-r", "./lib.rb", "x.rb"],
        &["curl lib", "curl x"],
    );
    runs(
        &ctx,
        "ruby",
        &["-r./lib.rb", "x.rb"],
        &["curl lib", "curl x"],
    );
    let absolute = format!("{}/lib.rb", ctx.root.as_ref().expect("root").display());
    runs(
        &ctx,
        "ruby",
        &["-r", &absolute, "x.rb"],
        &["curl lib", "curl x"],
    );
    // Options whose effect is not read.
    for args in [
        &["-C", "dir", "x.rb"][..],
        &["-Cdir", "x.rb"],
        &["-x", "x.rb"],
    ] {
        let text = refused(&ctx, "ruby", args);
        assert!(
            text.contains("whose effect kGraf does not read"),
            "{args:?}: {text}"
        );
    }
    assert!(why(&call(&ctx, "ruby", &["-S", "rake"]).expect("read")).contains("searches PATH"));
    assert!(why(&call(&ctx, "ruby", &["-", "x.rb"]).expect("read")).contains("from its input"));
    assert!(why(&call(&ctx, "ruby", &["-w"]).expect("read")).contains("from its input"));
    nothing(&ctx, "ruby", &["-v"]);
}

// -------------------------------------------------------------------- php

#[test]
fn php_takes_its_options_and_finds_the_code() {
    let (_d, ctx) = world();
    runs(&ctx, "php", &["x.php"], &["curl x"]);
    runs(&ctx, "php", &["-q", "x.php"], &["curl x"]);
    runs(&ctx, "php", &["-q", "--", "x.php"], &["curl x"]);
    // After `--` a word that starts with `-` is the script.
    runs(&ctx, "php", &["--", "-x.php"], &["curl x"]);
    runs(&ctx, "php", &["-q", "--", "-x.php"], &["curl x"]);
    // Code in the command line, in each spelling.
    for flag in [
        "-r",
        "--run",
        "-B",
        "--process-begin",
        "-R",
        "--process-code",
        "-E",
        "--process-end",
    ] {
        runs(&ctx, "php", &[flag, "system('curl x');"], &["curl x"]);
        runs(&ctx, "php", &["-q", flag, "system('curl x');"], &["curl x"]);
        assert!(why(&call(&ctx, "php", &[flag]).expect("read")).contains("names no code"));
    }
    runs(&ctx, "php", &["-r", "echo 1;"], &[]);
    runs(
        &ctx,
        "php",
        &["-r", "system('curl a');", "-r", "system('curl b');"],
        &["curl a", "curl b"],
    );
    // Script files, in each spelling.
    for flag in ["-f", "--file", "-F", "--process-file"] {
        runs(&ctx, "php", &[flag, "x.php"], &["curl x"]);
        runs(&ctx, "php", &["-q", flag, "x.php"], &["curl x"]);
        assert!(why(&call(&ctx, "php", &[flag]).expect("read")).contains("names no file"));
    }
    // Interactive and server modes run code kGraf does not read.
    for flag in ["-a", "--interactive"] {
        assert!(why(&call(&ctx, "php", &[flag]).expect("read")).contains("from its input"));
        // Even with a script named after it.
        let text = refused(&ctx, "php", &[flag, "x.php"]);
        assert!(text.contains("from its input"), "{text}");
    }
    for flag in ["-S", "--server", "-z", "--zend-extension"] {
        let text = refused(&ctx, "php", &[flag, "localhost:80"]);
        assert!(text.contains(&format!("`{flag}`")), "{text}");
    }
    // A setting, or a configuration file or folder, takes a word.
    runs(
        &ctx,
        "php",
        &["-d", "display_errors=1", "x.php"],
        &["curl x"],
    );
    runs(
        &ctx,
        "php",
        &["-q", "-d", "display_errors=1", "x.php"],
        &["curl x"],
    );
    runs(&ctx, "php", &["--define", "a=1", "x.php"], &["curl x"]);
    for flag in ["-c", "--php-ini", "-t", "--docroot"] {
        runs(&ctx, "php", &[flag, "dir", "x.php"], &["curl x"]);
        runs(&ctx, "php", &["-q", flag, "dir", "x.php"], &["curl x"]);
    }
    for setting in ["auto_prepend_file=y.php", "auto_append_file=y.php"] {
        for flag in ["-d", "--define"] {
            let text = refused(&ctx, "php", &[flag, setting, "x.php"]);
            assert!(text.contains("run another file first"), "{text}");
        }
    }
    // Questions that run nothing.
    for args in [
        &["-l", "x.php"][..],
        &["--syntax-check", "x.php"],
        &["--version"],
        &["-v"],
        &["-i"],
        &["--info"],
        &["-m"],
        &["--modules"],
        &["--ini"],
        &["--rf", "strlen"],
        &["--ri", "json"],
    ] {
        nothing(&ctx, "php", args);
    }
    // Options and no code or file: php reads its code from input.
    for args in [
        &[][..],
        &["-q"],
        &["-d", "a=1"],
        &["-c", "php.ini"],
        &["-q", "--"],
    ] {
        let text = refused(&ctx, "php", args);
        assert!(text.contains("from its input"), "{args:?}: {text}");
    }
}

// -------------------------------------------------------------------- awk

#[test]
fn awk_takes_its_options_and_finds_the_program() {
    let (_d, ctx) = world();
    let prog = "BEGIN { system(\"curl x\") }";
    runs(&ctx, "awk", &[prog], &["curl x"]);
    runs(&ctx, "awk", &[prog, "data.txt"], &["curl x"]);
    runs(&ctx, "awk", &["--", prog], &["curl x"]);
    runs(&ctx, "awk", &["-F:", "--", prog], &["curl x"]);
    // After `--` a program that starts with `-` is the program.
    runs(&ctx, "awk", &["--", "-1"], &[]);
    // Program files, in each spelling.
    for flag in ["-f", "--file", "-E", "--exec"] {
        runs(&ctx, "awk", &[flag, "x.awk"], &["curl x"]);
        runs(
            &ctx,
            "awk",
            &["-F:", flag, "x.awk", "data.txt"],
            &["curl x"],
        );
        assert!(why(&call(&ctx, "awk", &[flag]).expect("read")).contains("names no program file"));
    }
    runs(&ctx, "awk", &["-fx.awk"], &["curl x"]);
    runs(&ctx, "awk", &["-F:", "-fx.awk"], &["curl x"]);
    runs(&ctx, "awk", &["--file=x.awk"], &["curl x"]);
    runs(&ctx, "awk", &["-F:", "--file=x.awk"], &["curl x"]);
    // Program text in an option.
    for flag in ["-e", "--source"] {
        runs(&ctx, "awk", &[flag, prog], &["curl x"]);
        runs(&ctx, "awk", &["-F:", flag, prog], &["curl x"]);
        assert!(why(&call(&ctx, "awk", &[flag]).expect("read")).contains("names no program"));
    }
    runs(
        &ctx,
        "awk",
        &[
            "-e",
            "BEGIN { system(\"curl a\") }",
            "-e",
            "BEGIN { system(\"curl b\") }",
        ],
        &["curl a", "curl b"],
    );
    runs(
        &ctx,
        "awk",
        &[
            "--source",
            "BEGIN { system(\"curl a\") }",
            "--source",
            "BEGIN { system(\"curl b\") }",
        ],
        &["curl a", "curl b"],
    );
    runs(&ctx, "awk", &[&format!("--source={prog}")], &["curl x"]);
    runs(
        &ctx,
        "awk",
        &["-F:", &format!("--source={prog}")],
        &["curl x"],
    );
    // Libraries and extensions are not read.
    for flag in ["-i", "--include", "-l", "--load"] {
        let text = refused(&ctx, "awk", &[flag, "lib", prog]);
        assert!(text.contains("library or an extension"), "{text}");
    }
    // Options that take a word apart from the option.
    for flag in ["-v", "-F", "--assign", "--field-separator", "-W"] {
        runs(&ctx, "awk", &[flag, "x=1", prog], &["curl x"]);
        runs(&ctx, "awk", &["-b", flag, "x=1", prog], &["curl x"]);
    }
    runs(&ctx, "awk", &["-F:", prog], &["curl x"]);
    runs(&ctx, "awk", &["-b", "-F:", prog], &["curl x"]);
    runs(&ctx, "awk", &["--posix", prog], &["curl x"]);
    // A lone `-` is the program; no program is unreadable.
    runs(&ctx, "awk", &["-"], &[]);
    assert!(why(&call(&ctx, "awk", &[]).expect("read")).contains("names no program"));
    assert!(why(&call(&ctx, "awk", &["-F:"]).expect("read")).contains("names no program"));
    nothing(&ctx, "awk", &["--version"]);
}

// ---------------------------------------------------------------- osascript

#[test]
fn osascript_takes_its_options_and_finds_the_code() {
    let (_d, ctx) = world();
    let code = "do shell script \"curl x\"";
    runs(&ctx, "osascript", &["-e", code], &["curl x"]);
    runs(&ctx, "osascript", &["x.scpt"], &["curl x"]);
    runs(&ctx, "osascript", &["-ss", "x.scpt"], &["curl x"]);
    runs(&ctx, "osascript", &["-s", "o", "-e", code], &["curl x"]);
    runs(&ctx, "osascript", &["-s", "o", "x.scpt"], &["curl x"]);
    runs(
        &ctx,
        "osascript",
        &["-l", "AppleScript", "-e", code],
        &["curl x"],
    );
    runs(
        &ctx,
        "osascript",
        &["-l", "applescript", "x.scpt"],
        &["curl x"],
    );
    runs(
        &ctx,
        "osascript",
        &["-e", "display notification \"a\"", "-e", code],
        &["curl x"],
    );
    // JavaScript for Automation has no reader.
    let text = refused(&ctx, "osascript", &["-l", "JavaScript", "-e", "1"]);
    assert!(text.contains("JavaScript for Automation"), "{text}");
    refused(&ctx, "osascript", &["-l", "javascript", "x.scpt"]);
    // Interactive mode, and no code at all, read code from input.
    assert!(why(&call(&ctx, "osascript", &["-i"]).expect("read")).contains("from its input"));
    let text = refused(&ctx, "osascript", &["-i", "x.scpt"]);
    assert!(text.contains("from its input"), "{text}");
    assert!(why(&call(&ctx, "osascript", &[]).expect("read")).contains("from its input"));
    assert!(why(&call(&ctx, "osascript", &["-ss"]).expect("read")).contains("from its input"));
    // `-s` takes a word apart from the option.
    assert!(opaque(
        &call(&ctx, "osascript", &["-s", "x.scpt"]).expect("read")
    ));
}

// --------------------------------------------------------- local modules

#[test]
fn a_module_is_read_from_the_folder_of_the_script_that_imports_it() {
    let (_d, ctx) = files(&[
        ("sub/run.py", "import helper2\n"),
        ("sub/helper2.py", PY),
        ("sub/run.rb", "require_relative 'h.rb'\n"),
        ("sub/h.rb", RB),
    ]);
    runs(&ctx, "python3", &["sub/run.py"], &["curl x"]);
    runs(&ctx, "ruby", &["sub/run.rb"], &["curl x"]);
}

#[test]
fn an_import_that_should_be_local_and_is_missing_is_unreadable() {
    let (_d, ctx) = files(&[
        ("a.py", "from .missing import x\n"),
        ("b.js", "require('./missing')\n"),
        ("c.js", "require('/nonexistent/x')\n"),
        ("d.php", "<?php include 'missing.php';\n"),
        ("e.js", "require('lodash')\n"),
        ("f.rb", "require_relative './missing'\n"),
    ]);
    for (key, file) in [
        ("python3", "a.py"),
        ("node", "b.js"),
        ("node", "c.js"),
        ("php", "d.php"),
        ("ruby", "f.rb"),
    ] {
        let text = refused(&ctx, key, &[file]);
        assert!(text.contains("cannot find"), "{file}: {text}");
    }
    runs(&ctx, "node", &["e.js"], &[]);
    // No directory: a bare name may be a local module.
    let none = ReadContext::default();
    match local_module(Lang::JavaScript, "lodash", &none) {
        Err(text) => assert!(text.contains("cannot tell whether it is a local module")),
        Ok(_) => panic!("a bare name with no directory must be unknown"),
    }
    assert!(local_module(Lang::Python, "os", &none).is_err());
    // A language with no local modules.
    assert!(matches!(local_module(Lang::Perl, "x", &none), Ok(None)));
    assert!(matches!(local_module(Lang::Awk, "x", &none), Ok(None)));
}

#[test]
fn a_local_json_file_is_not_code_and_the_languages_find_their_files() {
    let (_d, ctx) = files(&[
        ("data.json", "{}"),
        ("m.rb", RB),
        ("lib/index.js", JS),
        ("inc.php", PHP),
        ("m2.py", PY),
        ("pk/__init__.py", PY),
    ]);
    assert!(matches!(
        local_module(Lang::JavaScript, "./data", &ctx),
        Ok(None)
    ));
    let read = |lang, import: &str| {
        local_module(lang, import, &ctx)
            .expect("found")
            .expect("a file")
            .text
            .to_string()
    };
    assert_eq!(read(Lang::Ruby, "m"), RB);
    assert_eq!(read(Lang::Ruby, "m.rb"), RB);
    assert_eq!(read(Lang::JavaScript, "./lib"), JS);
    assert_eq!(read(Lang::TypeScript, "./lib"), JS);
    assert_eq!(read(Lang::Tsx, "./lib"), JS);
    assert_eq!(read(Lang::Php, "inc.php"), PHP);
    assert_eq!(read(Lang::PhpOnly, "inc.php"), PHP);
    assert_eq!(read(Lang::Python, "m2"), PY);
    assert_eq!(read(Lang::Python, "pk"), PY);
}
