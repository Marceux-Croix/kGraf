//! The interpreter code reader: literal spawning calls are read as
//! commands, dynamic ones make the code unreadable, per language.

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

fn opaque(found: &[Embedded]) -> bool {
    found.iter().any(|e| match e {
        Embedded::Unreadable(_) => true,
        Embedded::In(_, inner) => opaque(std::slice::from_ref(inner)),
        _ => false,
    })
}

fn code(lang: Lang, src: &str) -> Vec<Embedded> {
    let dir = tempfile::tempdir().expect("dir");
    let ctx = ReadContext::in_worktree(dir.path(), None);
    read_code("x", lang, src, &Origin { ctx })
}

#[track_caller]
fn reads(lang: Lang, src: &str, want: &[&str]) {
    let found = code(lang, src);
    assert!(!opaque(&found), "{src}: {found:?}");
    assert_eq!(commands(&found), want, "{src}");
}

#[track_caller]
fn refuses(lang: Lang, src: &str) {
    let found = code(lang, src);
    assert!(opaque(&found), "{src}: {found:?}");
}

#[test]
fn python_literal_calls_are_read() {
    use Lang::Python as P;
    reads(P, "import os\nos.system('curl x')", &["curl x"]);
    reads(
        P,
        "import subprocess\nsubprocess.run(['curl', 'x'], check=True)",
        &["curl x"],
    );
    reads(
        P,
        "import subprocess as sp\nsp.check_output(('curl', 'x'))",
        &["curl x"],
    );
    reads(
        P,
        "from subprocess import Popen\nPopen('curl x', shell=True)",
        &["curl x"],
    );
    reads(P, "from os import system as s\ns('curl ' 'x')", &["curl x"]);
    reads(
        P,
        "import os\nos.execvp('curl', ['curl', 'x'])",
        &["curl x"],
    );
    reads(P, "import os\nos.execlp('curl', 'curl', 'x')", &["curl x"]);
    reads(
        P,
        "import os\nos.spawnlp(os.P_WAIT, 'curl', 'curl', 'x')",
        &["curl x"],
    );
    reads(
        P,
        "import asyncio\nasyncio.create_subprocess_exec('curl', 'x')",
        &["curl x"],
    );
    reads(P, "import pty\npty.spawn(['curl', 'x'])", &["curl x"]);
    reads(
        P,
        "import subprocess\nsubprocess.run(['ls'], executable='curl')",
        &["curl"],
    );
    // Code that starts nothing.
    reads(
        P,
        "import os, json\nprint(os.path.join('a', 'b'))\njson.dumps({})",
        &[],
    );
    reads(P, "print('system')", &[]);
}

#[test]
fn python_dynamic_calls_are_unreadable() {
    use Lang::Python as P;
    refuses(P, "import os\nos.system(cmd)");
    refuses(P, "import os\nos.system(f'curl {x}')");
    refuses(P, "import subprocess\nsubprocess.run(args)");
    refuses(P, "import subprocess\nsubprocess.run(['curl', x])");
    refuses(P, "import subprocess\nsubprocess.run(*a)");
    refuses(P, "import subprocess\nrun = subprocess.run\nrun(['curl'])");
    refuses(P, "import os\nf = os\nf.system('curl')");
    refuses(P, "import os\ngetattr(os, 'sys' + 'tem')('curl')");
    refuses(P, "exec('import os')");
    refuses(P, "eval(s)");
    refuses(P, "__import__('os').system('curl')");
    refuses(P, "import ctypes");
    refuses(P, "from os import *\nsystem('curl')");
    refuses(P, "import importlib\nimportlib.import_module('os')");
    refuses(P, "def f(:\n");
    refuses(P, "import os\nos.__dict__['system']('curl x')");
    refuses(P, "import sys, os\nsys.modules['os'].system('curl x')");
    refuses(
        P,
        "import os, operator\noperator.methodcaller('system', 'curl x')(os)",
    );
    refuses(P, "import builtins\nbuiltins.exec('1')");
}

#[test]
fn javascript_literal_calls_are_read() {
    use Lang::JavaScript as J;
    reads(
        J,
        "const cp = require('child_process'); cp.execSync('curl x')",
        &["curl x"],
    );
    reads(
        J,
        "const { spawn } = require('node:child_process'); spawn('curl', ['x'])",
        &["curl x"],
    );
    reads(
        J,
        "const { exec: run } = require('child_process'); run('curl x')",
        &["curl x"],
    );
    reads(
        J,
        "import { execFileSync } from 'child_process'; execFileSync('curl', ['x'])",
        &["curl x"],
    );
    reads(
        J,
        "import * as cp from 'child_process'; cp.spawn('curl', ['x'], { shell: true })",
        &["curl x"],
    );
    reads(
        J,
        "require('child_process').execSync('curl x')",
        &["curl x"],
    );
    reads(J, "Bun.spawn(['curl', 'x'])", &["curl x"]);
    reads(J, "Bun.spawnSync({ cmd: ['curl', 'x'] })", &["curl x"]);
    reads(J, "await Bun.$`curl ${url}`", &["curl ARG"]);
    reads(J, "new Deno.Command('curl', { args: ['x'] })", &["curl x"]);
    reads(
        J,
        "console.log('exec'); const fs = require('fs'); fs.readFileSync('a')",
        &[],
    );
    reads(
        Lang::TypeScript,
        "import { spawn } from 'child_process'; spawn('curl' as string, ['x'])",
        &["curl x"],
    );
}

#[test]
fn javascript_dynamic_calls_are_unreadable() {
    use Lang::JavaScript as J;
    refuses(J, "const cp = require('child_process'); cp.execSync(cmd)");
    refuses(
        J,
        "const cp = require('child_process'); cp.execSync(`curl ${x}`)",
    );
    refuses(
        J,
        "const cp = require('child_process'); cp.spawn('curl', args)",
    );
    refuses(
        J,
        "const cp = require('child_process'); const f = cp.exec; f('curl')",
    );
    refuses(
        J,
        "const cp = require('child_process'); cp['ex' + 'ec']('curl')",
    );
    refuses(J, "eval('x')");
    refuses(J, "new Function('return 1')()");
    refuses(J, "require(name)");
    refuses(J, "require('vm')");
    refuses(J, "process.binding('spawn_sync')");
    refuses(J, "Bun.$`${cmd} x`");
    refuses(J, "function (");
    refuses(J, "require('child_process')['exec']('curl x')");
    refuses(
        J,
        "const cp = require('child_process'); Reflect.get(cp, 'execSync')('curl x')",
    );
}

#[test]
fn ruby_literal_calls_are_read_and_dynamic_ones_refused() {
    use Lang::Ruby as R;
    reads(R, "system 'curl x'", &["curl x"]);
    reads(R, "system('curl', 'x')", &["curl x"]);
    reads(R, "`curl x`", &["curl x"]);
    reads(R, "%x(curl x)", &["curl x"]);
    reads(R, "IO.popen(['curl', 'x'])", &["curl x"]);
    reads(R, "Open3.capture2('curl x')", &["curl x"]);
    reads(R, "open('|curl x')", &["curl x"]);
    reads(R, "puts 'system'\nFile.read('a')", &[]);
    refuses(R, "system(cmd)");
    refuses(R, "`curl #{x}`");
    refuses(R, "eval('1')");
    refuses(R, "Kernel.send(:system, 'curl')");
    refuses(R, "open(path)");
}

#[test]
fn php_literal_calls_are_read_and_dynamic_ones_refused() {
    reads(Lang::PhpOnly, "system('curl x');", &["curl x"]);
    reads(Lang::PhpOnly, "echo shell_exec(\"curl x\");", &["curl x"]);
    reads(Lang::PhpOnly, "`curl x`;", &["curl x"]);
    reads(Lang::Php, "<?php passthru('curl x');", &["curl x"]);
    reads(Lang::PhpOnly, "echo strlen('a');", &[]);
    refuses(Lang::PhpOnly, "system($cmd);");
    refuses(Lang::PhpOnly, "exec(\"curl $x\");");
    refuses(Lang::PhpOnly, "$f = 'system'; $f('curl');");
    refuses(Lang::PhpOnly, "eval('x');");
    refuses(Lang::PhpOnly, "call_user_func('system', 'curl');");
}

#[test]
fn perl_awk_and_applescript_are_scanned_for_spawning_words() {
    reads(Lang::Perl, "system('curl x');", &["curl x"]);
    reads(Lang::Perl, "system 'curl', 'x';", &["curl x"]);
    reads(Lang::Perl, "my $o = `curl x`;", &["curl x"]);
    reads(Lang::Perl, "my $o = qx{curl x};", &["curl x"]);
    reads(Lang::Perl, "open(my $f, '-|', 'curl', 'x');", &["curl x"]);
    reads(Lang::Perl, "open(my $f, '<', 'a.txt'); print 1;", &[]);
    reads(Lang::Perl, "print 1 for 1..3; eval { 1 };", &[]);
    refuses(Lang::Perl, "system($cmd);");
    refuses(Lang::Perl, "system \"curl $x\";");
    refuses(Lang::Perl, "eval $code;");
    refuses(Lang::Perl, "do 'x.pl';");
    refuses(Lang::Perl, "$s =~ s/a/system('x')/e;");
    refuses(Lang::Perl, "use IPC::Open3;");
    refuses(Lang::Perl, "&$f('x');");
    reads(Lang::Awk, "{ system(\"curl x\") }", &["curl x"]);
    reads(Lang::Awk, "BEGIN { \"curl x\" | getline v }", &["curl x"]);
    reads(Lang::Awk, "{ print $1 | \"sort -u\" }", &["sort -u"]);
    reads(Lang::Awk, "/a|b/ { print $1 } $2 > 1 || $3 { n++ }", &[]);
    refuses(Lang::Awk, "{ system($0) }");
    refuses(Lang::Awk, "{ print | cmd }");
    refuses(Lang::Awk, "{ cmd | getline }");
    reads(Lang::AppleScript, "do shell script \"curl x\"", &["curl x"]);
    reads(Lang::AppleScript, "display notification \"hi\"", &[]);
    refuses(Lang::AppleScript, "do shell script \"curl \" & u");
    refuses(Lang::AppleScript, "do shell script cmd");
    refuses(
        Lang::AppleScript,
        "tell application \"System Events\" to keystroke \"x\"",
    );
}

#[test]
fn script_files_and_their_local_modules_are_read_one_level_deep() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path();
    let w = |name: &str, text: &str| {
        let p = root.join(name);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(p, text).expect("write");
    };
    w("main.py", "import helper\nhelper.go()\n");
    w(
        "helper.py",
        "import os\ndef go():\n    os.system('curl x')\n",
    );
    w("deep.py", "import mid\n");
    w("mid.py", "import leaf\n");
    w("leaf.py", "import os\n");
    w("plain.py", "import json\nprint(json.dumps(1))\n");
    w("dyn.py", "import os, sys\nos.system(sys.argv[1])\n");
    w("bad.py", "def (:\n");
    w("app.js", "const h = require('./h');\n");
    w("h.js", "require('child_process').execSync('curl x');\n");
    let ctx = ReadContext::in_worktree(root, None);
    let run = |file: &str| read_script_file("python3", Lang::Python, file, &ctx);
    assert_eq!(commands(&run("main.py")), vec!["curl x"]);
    assert!(opaque(&run("deep.py")));
    assert!(!opaque(&run("plain.py")) && commands(&run("plain.py")).is_empty());
    assert!(opaque(&run("dyn.py")));
    assert!(opaque(&run("bad.py")));
    assert!(opaque(&run("missing.py")));
    assert!(opaque(&run("../outside.py")));
    assert_eq!(
        commands(&read_script_file("node", Lang::JavaScript, "app.js", &ctx)),
        vec!["curl x"]
    );
}

#[test]
fn interpreter_calls_find_their_code() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(dir.path().join("x.py"), "import os\nos.system('curl x')\n").expect("write");
    std::fs::write(
        dir.path().join("x.js"),
        "require('child_process').execSync('curl x')\n",
    )
    .expect("write");
    let ctx = ReadContext::in_worktree(dir.path(), None);
    let a = |w: &[&str]| w.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let call = |key: &str, args: &[&str]| {
        interpreter_call(interpreter(key).expect("interpreter"), key, &a(args), &ctx)
    };
    assert_eq!(
        commands(&call("python3", &["-c", "import os; os.system('curl x')"]).unwrap_or_default()),
        vec!["curl x"]
    );
    assert_eq!(
        commands(&call("python3", &["-u", "x.py"]).unwrap_or_default()),
        vec!["curl x"]
    );
    assert_eq!(
        commands(
            &call("python3.12", &["-Bc", "import os; os.system('curl x')"]).unwrap_or_default()
        ),
        vec!["curl x"]
    );
    assert_eq!(
        commands(
            &call(
                "node",
                &["-e", "require('child_process').execSync('curl x')"]
            )
            .unwrap_or_default()
        ),
        vec!["curl x"]
    );
    assert_eq!(
        commands(&call("node", &["x.js"]).unwrap_or_default()),
        vec!["curl x"]
    );
    assert_eq!(
        commands(&call("perl", &["-le", "system('curl x')"]).unwrap_or_default()),
        vec!["curl x"]
    );
    assert_eq!(
        commands(&call("ruby", &["-e", "system('curl x')"]).unwrap_or_default()),
        vec!["curl x"]
    );
    assert_eq!(
        commands(&call("php", &["-r", "system('curl x');"]).unwrap_or_default()),
        vec!["curl x"]
    );
    assert_eq!(
        commands(&call("awk", &["BEGIN { system(\"curl x\") }"]).unwrap_or_default()),
        vec!["curl x"]
    );
    assert_eq!(
        commands(&call("osascript", &["-e", "do shell script \"curl x\""]).unwrap_or_default()),
        vec!["curl x"]
    );
    // An installed module and a version question run nothing kGraf reads.
    assert!(
        call("python3", &["-m", "pytest"])
            .unwrap_or_default()
            .is_empty()
    );
    assert!(call("python3", &["--version"]).is_none());
    assert!(call("node", &["--version"]).is_none());
    // Input, a missing file, an interpreter with no reader.
    assert!(opaque(&call("python3", &[]).unwrap_or_default()));
    assert!(opaque(&call("python3", &["-"]).unwrap_or_default()));
    assert!(opaque(&call("python3", &["nope.py"]).unwrap_or_default()));
    assert!(opaque(
        &call("lua", &["-e", "os.execute('curl x')"]).unwrap_or_default()
    ));
    assert!(opaque(
        &call("perl", &["-MIPC::Open3", "-e", "1"]).unwrap_or_default()
    ));
    assert!(opaque(
        &call("awk", &["{ system($0) }"]).unwrap_or_default()
    ));
}
