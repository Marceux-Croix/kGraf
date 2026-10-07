//! The scanners of each language, one construct at a time: what a literal
//! spawning call reads as, which spelling of it is read, and which text
//! makes the code unreadable (and says why).

use super::lexical::{AwkToken, awk_tokens, literal_list, quoted, substitution_with_e, word_at};
use super::*;

fn scanned(lang: Lang, code: &str) -> Result<Scan, String> {
    scan(lang, code).as_ref().clone()
}

#[track_caller]
fn found(lang: Lang, code: &str) -> Vec<Embedded> {
    match scanned(lang, code) {
        Ok(s) => s.found,
        Err(why) => panic!("{code:?}: unreadable: {why}"),
    }
}

#[track_caller]
fn script(lang: Lang, code: &str, want: &[&str]) {
    let want: Vec<Embedded> = want
        .iter()
        .map(|w| Embedded::Script((*w).to_owned()))
        .collect();
    assert_eq!(found(lang, code), want, "{code:?}");
}

#[track_caller]
fn argv(lang: Lang, code: &str, want: &[&[&str]]) {
    let want: Vec<Embedded> = want
        .iter()
        .map(|w| Embedded::Argv(w.iter().map(|s| (*s).to_owned()).collect()))
        .collect();
    assert_eq!(found(lang, code), want, "{code:?}");
}

#[track_caller]
fn nothing(lang: Lang, code: &str) {
    assert_eq!(found(lang, code), Vec::<Embedded>::new(), "{code:?}");
}

#[track_caller]
fn refused(lang: Lang, code: &str, part: &str) {
    match scanned(lang, code) {
        Ok(s) => panic!(
            "{code:?}: expected a refusal naming {part:?}, read {:?}",
            s.found
        ),
        Err(why) => assert!(why.contains(part), "{code:?}: {why}"),
    }
}

fn imports(lang: Lang, code: &str) -> Vec<String> {
    scanned(lang, code).expect("readable").imports
}

fn strs(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

// ---------------------------------------------------------------- helpers

#[test]
fn escapes_in_a_literal_name_the_text_they_decode_to() {
    for (raw, want) in [
        ("a\\nb", "a\nb"),
        ("a\\tb", "a\tb"),
        ("a\\rb", "a\rb"),
        ("\\a", "\x07"),
        ("\\b", "\x08"),
        ("\\f", "\x0c"),
        ("\\v", "\x0b"),
        ("\\e", "\x1b"),
        ("a\\\nb", "ab"),
        ("\\162", "r"),
        ("\\0", "\0"),
        ("\\1622", "r2"),
        ("\\7", "\x07"),
        ("\\x72", "r"),
        ("\\x7", "\x07"),
        ("\\x414", "A4"),
        ("\\x4g", "\x04g"),
        ("\\xg", "xg"),
        ("\\u0072", "r"),
        ("\\u{72}", "r"),
        ("\\u{72}x", "rx"),
        ("\\u{zz}x", "ux"),
        ("\\u00zz", "\0zz"),
        ("\\U00000072", "r"),
        ("\\U0000007", "\x07"),
        ("\\Uzzzzzzzz", "Uzzzzzzzz"),
        ("\\q", "q"),
        ("\\\\", "\\"),
        ("\\", "\\"),
        ("plain", "plain"),
        ("", ""),
    ] {
        assert_eq!(unescape(raw), want, "{raw:?}");
    }
    // Digits stop at the radix.
    assert_eq!(unescape("\\18"), "\x018");
    assert_eq!(unescape("\\x7g"), "\x07g");
}

#[test]
fn a_spawning_call_reads_as_a_script_or_an_argv_by_its_arguments_and_the_shell() {
    let s = |x: &str| Lit::Str(x.to_owned());
    let l = |x: &[&str]| Lit::List(strs(x));
    assert_eq!(
        spawned(s("curl x"), &[], false),
        vec![Embedded::Script("curl x".into())]
    );
    assert_eq!(
        spawned(s("curl x"), &[], true),
        vec![Embedded::Script("curl x".into())]
    );
    assert_eq!(
        spawned(s("curl"), &strs(&["x", "y"]), false),
        vec![Embedded::Argv(strs(&["curl", "x", "y"]))]
    );
    assert_eq!(
        spawned(s("curl"), &strs(&["x", "y"]), true),
        vec![Embedded::Script("curl x y".into())]
    );
    assert!(spawned(l(&[]), &[], false).is_empty());
    assert!(spawned(l(&[]), &[], true).is_empty());
    assert_eq!(
        spawned(l(&["curl", "x"]), &[], false),
        vec![Embedded::Argv(strs(&["curl", "x"]))]
    );
    assert_eq!(
        spawned(l(&["curl", "x"]), &[], true),
        vec![Embedded::Script("curl x".into())]
    );
    assert_eq!(
        dynamic("a value"),
        "starts a program with a value, so kGraf cannot tell what it runs"
    );
}

// ----------------------------------------------------------------- python

use Lang::Python as P;

#[test]
fn python_names_each_spawning_function_with_its_form() {
    script(
        P,
        "import subprocess\nsubprocess.getoutput('curl x')",
        &["curl x"],
    );
    script(
        P,
        "import subprocess\nsubprocess.getstatusoutput('curl x')",
        &["curl x"],
    );
    script(
        P,
        "import commands\ncommands.getoutput('curl x')",
        &["curl x"],
    );
    script(
        P,
        "import commands\ncommands.getstatusoutput('curl x')",
        &["curl x"],
    );
    script(P, "import platform\nplatform.popen('curl x')", &["curl x"]);
    script(P, "import popen2\npopen2.Popen3('curl x')", &["curl x"]);
    script(P, "import popen2\npopen2.popen2('curl x')", &["curl x"]);
    script(P, "import os\nos.popen3('curl x')", &["curl x"]);
    script(P, "import os\nos.popen4('curl x')", &["curl x"]);
    script(P, "import posix\nposix.system('curl x')", &["curl x"]);
    script(P, "import nt\nnt.system('curl x')", &["curl x"]);
    argv(
        P,
        "import os\nos.spawnv(os.P_WAIT, '/bin/curl', ['curl', 'x'])",
        &[&["/bin/curl", "x"]],
    );
    for f in ["spawnv", "spawnve", "spawnvp", "spawnvpe"] {
        argv(
            P,
            &format!("import os\nos.{f}(os.P_WAIT, '/bin/curl', ['curl', 'x'])"),
            &[&["/bin/curl", "x"]],
        );
    }
    for f in ["posix_spawn", "posix_spawnp"] {
        argv(
            P,
            &format!("import os\nos.{f}('/bin/curl', ['curl', 'x'], {{}})"),
            &[&["/bin/curl", "x"]],
        );
    }
    for f in ["execv", "execve", "execvp", "execvpe", "fexecve"] {
        argv(
            P,
            &format!("import os\nos.{f}('/bin/curl', ['curl', 'x'])"),
            &[&["/bin/curl", "x"]],
        );
    }
    argv(P, "import os\nos.startfile('curl')", &[&["curl"]]);
    argv(
        P,
        "import os\nos.execle('/bin/curl', 'curl', 'x', {})",
        &[&["/bin/curl", "x"]],
    );
    argv(
        P,
        "import os\nos.execl('/bin/curl', 'curl', 'x')",
        &[&["/bin/curl", "x"]],
    );
    argv(
        P,
        "import os\nos.spawnle(os.P_WAIT, '/bin/curl', 'curl', 'x', {})",
        &[&["/bin/curl", "x"]],
    );
    argv(
        P,
        "import os\nos.spawnl(os.P_WAIT, '/bin/curl', 'curl', 'x')",
        &[&["/bin/curl", "x"]],
    );
    argv(
        P,
        "from distutils import spawn\nspawn.spawn(['curl', 'x'])",
        &[&["curl", "x"]],
    );
    argv(P, "import pty\npty.spawn('curl')", &[&["curl"]]);
    // A spawning module reached as another module's attribute.
    script(P, "import shutil\nshutil.os.system('curl x')", &["curl x"]);
    script(
        P,
        "import tempfile\ntempfile._os.system('curl x')",
        &["curl x"],
    );
    // A name that spawns nothing.
    nothing(P, "import os\nos.getcwd()");
    nothing(P, "import subprocess\nsubprocess.list2cmdline(['a'])");
}

#[test]
fn python_reads_the_arguments_of_a_spawning_call() {
    // `shell=` decides between a script and an argv.
    assert_eq!(
        found(
            P,
            "import subprocess\nsubprocess.run(['curl', 'a b'], shell=True)"
        ),
        vec![Embedded::Script("curl a b".into())]
    );
    assert_eq!(
        found(
            P,
            "import subprocess\nsubprocess.run(['curl', 'a b'], shell=False)"
        ),
        vec![Embedded::Argv(strs(&["curl", "a b"]))]
    );
    assert_eq!(
        found(P, "import subprocess\nsubprocess.run(['curl', 'a b'])"),
        vec![Embedded::Argv(strs(&["curl", "a b"]))]
    );
    // A shell flag that is no literal counts as set.
    assert_eq!(
        found(
            P,
            "import subprocess\nsubprocess.run(['curl', 'a b'], shell=flag)"
        ),
        vec![Embedded::Script("curl a b".into())]
    );
    assert_eq!(
        found(P, "import subprocess\nsubprocess.run(args=['curl', 'x'])"),
        vec![Embedded::Argv(strs(&["curl", "x"]))]
    );
    // `cwd=` moves the command somewhere kGraf cannot follow.
    let moved = found(
        P,
        "import subprocess\nsubprocess.run(['curl', 'x'], cwd='sub')",
    );
    assert!(matches!(moved.as_slice(), [Embedded::In(..)]), "{moved:?}");
    let plain = found(
        P,
        "import subprocess\nsubprocess.run(['curl', 'x'], check=True)",
    );
    assert!(matches!(plain.as_slice(), [Embedded::Argv(_)]), "{plain:?}");
    // `executable=` names the program.
    assert_eq!(
        found(
            P,
            "import subprocess\nsubprocess.run(['ls', 'a'], executable='curl')"
        ),
        vec![Embedded::Argv(strs(&["curl", "a"]))]
    );
    assert_eq!(
        found(
            P,
            "import subprocess\nsubprocess.run('ls a', shell=True, executable='/bin/zsh')"
        ),
        vec![Embedded::Argv(strs(&["/bin/zsh"]))]
    );
    assert_eq!(
        found(P, "import os\nos.system([], executable='curl')"),
        vec![Embedded::Argv(Vec::new())]
    );
    refused(
        P,
        "import subprocess\nsubprocess.run(['a'], executable=x)",
        "`executable` that is not literal",
    );
    // Arguments unpacked from a value.
    refused(
        P,
        "import subprocess\nsubprocess.run(['curl'], **kw)",
        "unpacked",
    );
    refused(P, "import subprocess\nsubprocess.run(*a)", "unpacked");
    // A comment among the arguments is no argument.
    argv(
        P,
        "import asyncio\nasyncio.create_subprocess_exec('curl', # note\n 'x')",
        &[&["curl", "x"]],
    );
    script(
        P,
        "import os\nos.system(\n  # note\n  'curl x')",
        &["curl x"],
    );
    // A string with escapes, a raw string, a parenthesized string.
    script(P, "import os\nos.system('curl\\x20x')", &["curl x"]);
    script(P, "import os\nos.system(r'curl \\x41')", &["curl \\x41"]);
    script(P, "import os\nos.system(R'curl \\x41')", &["curl \\x41"]);
    script(P, "import os\nos.system(b'curl x')", &["curl x"]);
    script(P, "import os\nos.system(('curl x'))", &["curl x"]);
    script(P, "import os\nos.system('curl ' 'x')", &["curl x"]);
    script(P, "import os\nos.system(\"curl\\tx\")", &["curl\tx"]);
    refused(P, "import os\nos.system(f'curl {x}')", "not literal");
    refused(P, "import os\nos.system('a' + 'b')", "not literal");
    // Missing arguments.
    refused(P, "import os\nos.system()", "not literal");
    refused(P, "import os\nos.execv('/bin/curl', args)", "not literal");
    refused(
        P,
        "import os\nos.spawnv(os.P_WAIT, '/bin/curl', args)",
        "not literal",
    );
    refused(P, "import os\nos.execl('/bin/curl', x)", "not literal");
}

#[test]
fn python_refuses_a_spawning_function_that_is_not_called() {
    refused(P, "import os\nx = os.system", "used as a value");
    refused(P, "import os\nprint(os.system)", "used as a value");
    refused(
        P,
        "import os\nos.system.__call__('curl x')",
        "used as a value",
    );
    refused(P, "import os\nm = os", "used as a value");
    refused(P, "import os\nprint(os)", "used as a value");
    refused(P, "import os\nos.__dict__", "reached by name");
    refused(P, "import sys\nsys.modules", "reached by name");
    refused(P, "import os\nx = 'system'", "given as text");
    refused(P, "import subprocess\nx = 'popen'", "given as text");
    // None of these starts a program.
    nothing(P, "import os\nos.path.join('a', 'b')");
    nothing(P, "import os\nx = os.environ.get('A')");
    nothing(P, "import subprocess\nx = subprocess.PIPE");
    nothing(P, "import json\nx = json");
    nothing(P, "x = 'system'");
    nothing(P, "import json\nx = 'system'");
    nothing(P, "import os\nos.path.exists('a')\nprint(os.sep)");
}

#[test]
fn python_refuses_calls_that_run_text_or_reach_a_function_by_name() {
    refused(P, "exec('x')", "runs text as code (`exec`)");
    refused(P, "x = eval('1')", "runs text as code (`eval`)");
    refused(P, "compile('1', 'f', 'exec')", "`compile`");
    refused(P, "__import__('os')", "`__import__`");
    refused(P, "globals()", "`globals`");
    refused(
        P,
        "import os\ngetattr(os, 'system')('curl')",
        "looked up by name",
    );
    refused(
        P,
        "import os\nsetattr(os, 'system', f)",
        "looked up by name",
    );
    refused(P, "getattr(getattr, 'x')", "looked up by name");
    // A lookup on something else is no spawn.
    nothing(P, "class A: pass\ngetattr(A, 'x')");
    nothing(P, "import json\ngetattr(json, 'dumps')");
    refused(
        P,
        "import asyncio\nloop = asyncio.get_event_loop()\nloop.subprocess_exec(f, 'curl')",
        "event loop",
    );
    refused(P, "loop.subprocess_shell(f, 'curl x')", "event loop");
    nothing(P, "loop.run_forever()");
    nothing(P, "subprocess_exec('x')");
}

#[test]
fn python_reads_each_form_of_import() {
    script(P, "import os.path\nos.system('curl x')", &["curl x"]);
    script(P, "import os.path as p\np.system('curl x')", &["curl x"]);
    script(
        P,
        "import subprocess as sp, os\nsp.getoutput('curl x')\nos.system('curl y')",
        &["curl x", "curl y"],
    );
    script(P, "from os import system as s\ns('curl x')", &["curl x"]);
    script(
        P,
        "from os import system, popen\nsystem('curl x')\npopen('curl y')",
        &["curl x", "curl y"],
    );
    script(
        P,
        "from subprocess import (getoutput as g)\ng('curl x')",
        &["curl x"],
    );
    refused(
        P,
        "from os import *\nsystem('curl x')",
        "imports every name",
    );
    refused(P, "from subprocess import *", "imports every name");
    nothing(P, "from json import *");
    refused(P, "import ctypes", "imports `ctypes`");
    refused(P, "import ctypes.util", "imports `ctypes`");
    refused(P, "from sh import ls", "imports `sh`");
    refused(
        P,
        "from importlib import import_module",
        "imports `importlib`",
    );
    refused(P, "import importlib as i", "imports `importlib`");
    // A relative import is local, whatever it is called.
    assert_eq!(imports(P, "from . import sh"), strs(&[".sh", "."]));
    assert_eq!(
        imports(P, "import helper\nimport pkg.mod"),
        strs(&["helper", "pkg.mod"])
    );
    assert_eq!(imports(P, "from .mod import f"), strs(&[".mod.f", ".mod"]));
    assert_eq!(
        imports(P, "from ..up import g as h"),
        strs(&[".up.g", "..up"])
    );
}

// ------------------------------------------------------------- javascript

use Lang::JavaScript as J;

#[test]
fn javascript_reads_strings_templates_and_wrappers() {
    let cp = "const cp = require('child_process');\n";
    script(J, &format!("{cp}cp.execSync('curl\\x20x')"), &["curl x"]);
    script(J, &format!("{cp}cp.execSync(`curl x`)"), &["curl x"]);
    script(J, &format!("{cp}cp.execSync(`curl\\x20x`)"), &["curl x"]);
    script(J, &format!("{cp}cp.execSync(('curl x'))"), &["curl x"]);
    script(J, &format!("{cp}cp.execSync(\"curl\\tx\")"), &["curl\tx"]);
    let ts = Lang::TypeScript;
    let tcp = "import * as cp from 'child_process';\n";
    script(
        ts,
        &format!("{tcp}cp.execSync('curl x' as string)"),
        &["curl x"],
    );
    script(
        ts,
        &format!("{tcp}cp.execSync('curl x' satisfies string)"),
        &["curl x"],
    );
    script(ts, &format!("{tcp}cp.execSync(('curl x'))"), &["curl x"]);
    argv(
        ts,
        "Bun.spawn(['curl', 'x'] as string[])",
        &[&["curl", "x"]],
    );
    argv(
        ts,
        "Bun.spawn(['curl', 'x'] satisfies string[])",
        &[&["curl", "x"]],
    );
    argv(ts, "Bun.spawn((['curl', 'x']))", &[&["curl", "x"]]);
    argv(
        ts,
        "new Deno.Command('curl', { args: ['x'] as string[] })",
        &[&["curl", "x"]],
    );
    // A module wrapped in parentheses or `!` is a value use: refused.
    refused(
        ts,
        &format!("{tcp}cp!.execSync('curl x')"),
        "used as a value",
    );
    refused(
        ts,
        &format!("{tcp}(cp).execSync('curl x')"),
        "used as a value",
    );
    // JSX with type annotations needs the TSX grammar.
    script(
        Lang::Tsx,
        "const f = (x: number) => <b>{x}</b>;\nrequire('child_process').execSync('curl x')",
        &["curl x"],
    );
    // A `#!` line is not code; the rest is.
    script(
        J,
        "#!/usr/bin/env node\nrequire('child_process').execSync('curl x')\n",
        &["curl x"],
    );
    refused(J, &format!("{cp}cp.execSync(`curl ${{x}}`)"), "not literal");
}

#[test]
fn javascript_reads_each_spawning_function_and_option() {
    let cp = "const cp = require('child_process');\n";
    script(J, &format!("{cp}cp.exec('curl x')"), &["curl x"]);
    script(J, &format!("{cp}cp.execSync('curl x')"), &["curl x"]);
    argv(
        J,
        &format!("{cp}cp.execFile('curl', ['x'])"),
        &[&["curl", "x"]],
    );
    argv(
        J,
        &format!("{cp}cp.execFileSync('curl', ['x'])"),
        &[&["curl", "x"]],
    );
    argv(
        J,
        &format!("{cp}cp.spawn('curl', ['x'])"),
        &[&["curl", "x"]],
    );
    argv(
        J,
        &format!("{cp}cp.spawnSync('curl', ['x'])"),
        &[&["curl", "x"]],
    );
    script(J, &format!("{cp}cp.spawn('curl')"), &["curl"]);
    argv(
        J,
        &format!("{cp}cp.fork('./worker.js')"),
        &[&["node", "./worker.js"]],
    );
    refused(J, &format!("{cp}cp.fork(x)"), "not literal");
    assert_eq!(
        found(
            J,
            &format!("{cp}cp.spawn('curl', ['a b'], {{ shell: true }})")
        ),
        vec![Embedded::Script("curl a b".into())]
    );
    assert_eq!(
        found(
            J,
            &format!("{cp}cp.spawn('curl', ['a b'], {{ shell: false }})")
        ),
        vec![Embedded::Argv(strs(&["curl", "a b"]))]
    );
    assert_eq!(
        found(
            J,
            &format!("{cp}cp.spawn('curl', ['a b'], {{ foo: 'x', shell: false }})")
        ),
        vec![Embedded::Argv(strs(&["curl", "a b"]))]
    );
    assert_eq!(
        found(
            J,
            &format!("{cp}cp.spawn('curl', ['a b'], {{ 'shell': true }})")
        ),
        vec![Embedded::Script("curl a b".into())]
    );
    // An options object in the place of the argument list.
    assert_eq!(
        found(J, &format!("{cp}cp.spawn('curl', {{ cwd: 'x' }})")).len(),
        1
    );
    let moved = found(J, &format!("{cp}cp.spawn('curl', ['x'], {{ cwd: 'sub' }})"));
    assert!(matches!(moved.as_slice(), [Embedded::In(..)]), "{moved:?}");
    refused(J, &format!("{cp}cp.spawn('curl', x)"), "not literal");
    refused(J, &format!("{cp}cp.spawn('curl', ...args)"), "spread");
    argv(
        J,
        &format!("{cp}cp.spawn('curl', /* note */ ['x'])"),
        &[&["curl", "x"]],
    );
    // Bun and Deno.
    argv(J, "Bun.spawn(['curl', 'x'])", &[&["curl", "x"]]);
    argv(J, "Bun.spawnSync(['curl', 'x'])", &[&["curl", "x"]]);
    argv(J, "Bun.spawn({ cmd: ['curl', 'x'] })", &[&["curl", "x"]]);
    argv(
        J,
        "import bun from 'bun';\nbun.spawn(['curl', 'x'])",
        &[&["curl", "x"]],
    );
    argv(
        J,
        "import bun from 'bun';\nbun.spawnSync(['curl', 'x'])",
        &[&["curl", "x"]],
    );
    argv(J, "Deno.run({ cmd: ['curl', 'x'] })", &[&["curl", "x"]]);
    argv(
        J,
        "new Deno.Command('curl', { args: ['x'] })",
        &[&["curl", "x"]],
    );
    script(J, "new Deno.Command('curl')", &["curl"]);
    refused(J, "Deno.run(cmd)", "not literal");
    refused(J, "Deno.run({ cwd: 'x' })", "not literal");
    refused(J, "Bun.spawn(cmd)", "not literal");
    refused(J, "Bun.spawn({ cmd })", "not literal");
    refused(J, "new Deno.Command(x)", "not literal");
    script(J, "Bun.$`curl ${url}`", &["curl ARG"]);
    script(J, "Bun.$`curl\\x20x`", &["curl x"]);
    script(J, "import bun from 'bun';\nbun.$`curl x`", &["curl x"]);
    refused(J, "Bun.$`${cmd} x`", "named by a substitution");
    refused(J, "Bun.$('curl')", "not literal");
}

#[test]
fn javascript_refuses_a_function_reached_by_a_computed_or_hidden_name() {
    let cp = "const cp = require('child_process');\n";
    refused(J, &format!("{cp}cp['exec']('curl x')"), "computed name");
    refused(
        J,
        "require('child_process')['exec']('curl x')",
        "process module",
    );
    refused(J, "Bun['spawn'](['curl'])", "computed name");
    refused(J, "Deno['run']({ cmd: ['curl'] })", "computed name");
    refused(J, "globalThis['eval']('x')", "computed name");
    refused(J, "process['binding']('x')", "computed name");
    refused(J, "eval('x')", "runs text as code");
    refused(J, "new Function('x')", "runs text as code");
    refused(J, "globalThis.eval('x')", "runs text as code");
    refused(J, "globalThis.Function('x')", "runs text as code");
    refused(J, "process.binding('x')", "native code");
    refused(J, "process._linkedBinding('x')", "native code");
    refused(J, "process.dlopen(m, 'x')", "native code");
    refused(J, "const b = process.binding", "native code");
    nothing(J, "setTimeout(() => 1, 1)");
    nothing(J, "setTimeout(f, 'x')");
    nothing(J, "process.exit('done')");
    nothing(J, "process.stdout.write('x')");
    // The process module used as a value, a spawning function used as a
    // value, `eval` and `Function` as values.
    refused(J, "foo(require('child_process'))", "process module");
    refused(
        J,
        "const m = require('child_process').exec",
        "spawning function used as a value",
    );
    refused(J, "require('child_process')", "process module");
    refused(J, "require('child_process').foo", "process module");
    refused(
        J,
        "const cp = require('child_process'); foo(cp)",
        "process module",
    );
    refused(
        J,
        "const cp = require('child_process'); foo(cp.execSync)",
        "spawning function used as a value",
    );
    refused(
        J,
        "const cp = require('child_process'); new Foo(cp.execSync)",
        "spawning function used as a value",
    );
    refused(
        J,
        "const cp = require('child_process'); const g = cp[name]",
        "process module",
    );
    refused(J, "const f = eval;", "process module or `eval`");
    refused(J, "const F = Function;", "process module or `eval`");
    nothing(J, "const p = process;");
    nothing(J, "const g = globalThis;");
    nothing(J, "const x = 1; foo(x)");
    // A string that names a spawning function, when the module is loaded.
    refused(
        J,
        "const cp = require('child_process'); const n = 'exec'",
        "named by text",
    );
    refused(
        J,
        "require('child_process').execSync('curl x'); const n = 'exec'",
        "named by text",
    );
    refused(
        J,
        "import { exec } from 'child_process'; const n = 'spawn'",
        "named by text",
    );
    for name in [
        "exec",
        "execSync",
        "execFile",
        "execFileSync",
        "spawn",
        "spawnSync",
        "fork",
    ] {
        refused(
            J,
            &format!("const cp = require('child_process'); const n = '{name}'"),
            "named by text",
        );
    }
    nothing(J, "const n = 'exec'");
    nothing(J, "import fs from 'fs'; const n = 'spawn'");
    // Opaque modules and a module named by a value.
    refused(J, "require('vm')", "loads `vm`");
    refused(J, "import vm from 'node:vm'", "loads `node:vm`");
    refused(J, "const { exec } = require(name)", "named by a value");
    refused(
        J,
        "const { ...rest } = require('child_process')",
        "cannot place",
    );
}

#[test]
fn javascript_reads_each_form_of_import_and_binding() {
    script(
        J,
        "import cp from 'child_process';\ncp.execSync('curl x')",
        &["curl x"],
    );
    script(
        J,
        "import * as cp from 'child_process';\ncp.execSync('curl x')",
        &["curl x"],
    );
    script(
        J,
        "import { execSync } from 'child_process';\nexecSync('curl x')",
        &["curl x"],
    );
    script(
        J,
        "import { execSync as run } from 'child_process';\nrun('curl x')",
        &["curl x"],
    );
    script(
        J,
        "const { execSync } = require('child_process');\nexecSync('curl x')",
        &["curl x"],
    );
    script(
        J,
        "const { execSync: run } = require('child_process');\nrun('curl x')",
        &["curl x"],
    );
    script(
        J,
        "const cp = require('node:child_process');\ncp.execSync('curl x')",
        &["curl x"],
    );
    script(
        J,
        "const cp = module.require('child_process');\ncp.execSync('curl x')",
        &["curl x"],
    );
    assert_eq!(imports(J, "import './a.js'"), strs(&["./a.js"]));
    assert_eq!(
        imports(J, "import x from '/abs/a.js'"),
        strs(&["/abs/a.js"])
    );
    assert_eq!(imports(J, "import x from 'pkg'"), Vec::<String>::new());
    assert_eq!(imports(J, "const a = require('./a')"), strs(&["./a"]));
    assert_eq!(imports(J, "const a = require('/abs/a')"), strs(&["/abs/a"]));
    assert_eq!(imports(J, "const a = require('pkg')"), Vec::<String>::new());
    // A binding is not a use of the module.
    nothing(J, "const cp = require('child_process');");
    nothing(J, "import cp from 'child_process';");
    nothing(J, "import { exec } from 'child_process';");
    nothing(J, "const { exec } = require('child_process');");
    nothing(J, "const { exec: run } = require('child_process');");
}

// ------------------------------------------------------------------- ruby

use Lang::Ruby as R;

#[test]
fn ruby_reads_each_spawning_method_and_spelling() {
    script(R, "system(\"curl\\x20x\")", &["curl x"]);
    script(R, "system(('curl x'))", &["curl x"]);
    script(R, "`curl\\x20x`", &["curl x"]);
    script(R, "%x(curl\\x20x)", &["curl x"]);
    script(R, "Kernel.system('curl x')", &["curl x"]);
    script(R, "Kernel.exec('curl x')", &["curl x"]);
    script(R, "Kernel.spawn('curl x')", &["curl x"]);
    script(R, "spawn('curl x')", &["curl x"]);
    script(R, "exec('curl x')", &["curl x"]);
    script(R, "Process.spawn('curl x')", &["curl x"]);
    script(R, "Process.exec('curl x')", &["curl x"]);
    script(R, "PTY.spawn('curl x')", &["curl x"]);
    script(R, "PTY.getpty('curl x')", &["curl x"]);
    for m in [
        "popen3",
        "popen2",
        "popen2e",
        "capture2",
        "capture2e",
        "capture3",
        "pipeline",
        "pipeline_r",
        "pipeline_w",
        "pipeline_rw",
        "pipeline_start",
    ] {
        script(R, &format!("Open3.{m}('curl x')"), &["curl x"]);
    }
    script(R, "IO.popen('curl x')", &["curl x"]);
    assert_eq!(
        found(R, "IO.popen(['curl', 'x'])"),
        vec![Embedded::Argv(strs(&["curl", "x"]))]
    );
    argv(R, "system('curl', 'x')", &[&["curl", "x"]]);
    argv(R, "system(['curl', 'argv0'], 'x')", &[&["curl", "x"]]);
    refused(R, "system([])", "not literal");
    refused(R, "system(['curl', x])", "not literal");
    refused(R, "system(x)", "not literal");
    refused(R, "system('curl', x)", "not literal");
    refused(R, "system", "not literal");
    refused(R, "IO.popen(x)", "not literal");
    refused(R, "IO.popen", "not literal");
    refused(R, "system(*a)", "unpacked");
    refused(R, "system('curl', **o)", "unpacked");
    script(R, "system('curl x', out: '/dev/null')", &["curl x"]);
    script(R, "system(\n  # note\n  'curl x')", &["curl x"]);
    refused(R, "`curl #{x}`", "substitution");
    // `open` starts a command only for a `|` path.
    script(R, "open('|curl x')", &["curl x"]);
    script(R, "Kernel.open('|curl x')", &["curl x"]);
    script(R, "URI.open('|curl x')", &["curl x"]);
    nothing(R, "open('file.txt')");
    refused(R, "open(path)", "may start with `|`");
    refused(R, "Kernel.open(path)", "may start with `|`");
    nothing(R, "URI.open(path)");
    nothing(R, "IO.open(path)");
    nothing(R, "File.open(path)");
    // `IO.read` and its kin.
    for m in [
        "read",
        "readlines",
        "write",
        "foreach",
        "binread",
        "binwrite",
    ] {
        script(R, &format!("IO.{m}('|curl x')"), &["curl x"]);
        nothing(R, &format!("IO.{m}('file.txt')"));
        refused(R, &format!("IO.{m}(path)"), &format!("`IO.{m}`"));
    }
    // A method named by a symbol.
    for s in ["system", "exec", "spawn", "popen"] {
        refused(R, &format!("m = :{s}"), &format!("`{s}` named by a symbol"));
    }
    nothing(R, "m = :other");
    nothing(R, "puts 'system'");
    for method in [
        "eval",
        "instance_eval",
        "send",
        "public_send",
        "const_get",
        "load",
        "binding",
    ] {
        refused(R, &format!("x.{method}(a)"), &format!("`{method}`"));
    }
    refused(R, "eval", "`eval`");
}

#[test]
fn ruby_reads_the_files_it_requires() {
    assert_eq!(
        imports(
            R,
            "require './a'\nrequire_relative 'b'\nrequire 'json'\nrequire '/abs'"
        ),
        strs(&["./a", "./b", "/abs"])
    );
    assert_eq!(imports(R, "require_relative './b'"), strs(&["././b"]));
    assert_eq!(imports(R, "require 'json'"), Vec::<String>::new());
    refused(R, "require x", "named by a value");
    refused(R, "require_relative x", "named by a value");
    refused(R, "require", "named by a value");
    // A call with a receiver is no require.
    assert_eq!(imports(R, "Bundler.require('./a')"), Vec::<String>::new());
}

// -------------------------------------------------------------------- php

use Lang::PhpOnly as H;

#[test]
fn php_reads_each_spawning_function_and_spelling() {
    script(H, "system('curl x');", &["curl x"]);
    script(H, "system(\"curl\\x20x\");", &["curl x"]);
    script(H, "system(('curl x'));", &["curl x"]);
    script(H, "SYSTEM('curl x');", &["curl x"]);
    script(H, "\\system('curl x');", &["curl x"]);
    for f in [
        "system",
        "exec",
        "shell_exec",
        "passthru",
        "popen",
        "expect_popen",
    ] {
        script(H, &format!("{f}('curl x');"), &["curl x"]);
    }
    // Extra arguments (output, status, mode) do not matter.
    script(H, "exec('curl x', $output, $code);", &["curl x"]);
    script(H, "popen('curl x', 'r');", &["curl x"]);
    script(H, "system('');", &[""]);
    script(H, "system(\"\");", &[""]);
    // A string of nothing but escapes.
    script(H, "system(\"\\x63\\x75\\x72\\x6c\");", &["curl"]);
    script(H, "`curl\\x20x`;", &["curl x"]);
    script(H, "`curl x`;", &["curl x"]);
    refused(H, "`curl $x`;", "backquoted");
    script(H, "proc_open('curl x', $descriptors, $pipes);", &["curl x"]);
    argv(
        H,
        "proc_open(['curl', 'x'], $descriptors, $pipes);",
        &[&["curl", "x"]],
    );
    refused(H, "proc_open($c, $d, $p);", "not literal");
    refused(H, "proc_open();", "not literal");
    argv(H, "pcntl_exec('/bin/curl', ['x']);", &[&["/bin/curl", "x"]]);
    script(H, "pcntl_exec('/bin/curl');", &["/bin/curl"]);
    refused(H, "pcntl_exec('/bin/curl', $args);", "not literal");
    refused(H, "pcntl_exec($p);", "not literal");
    refused(H, "system($cmd);", "not literal");
    refused(H, "system();", "not literal");
    refused(H, "system(...$args);", "unpacked");
    refused(H, "$f('curl');", "named by a value");
    for f in ["call_user_func", "array_map", "usort", "assert", "dl"] {
        refused(H, &format!("{f}('a', 'b');"), &format!("`{f}`"));
    }
    nothing(H, "strlen('abc');");
    // Methods named by a value, FFI, reflection.
    refused(H, "$o->$m();", "method named by a value");
    refused(H, "Foo::$m();", "method named by a value");
    refused(H, "$o?->$m();", "method named by a value");
    nothing(H, "$o->run();");
    nothing(H, "Foo::run();");
    refused(H, "FFI::cdef('int system(const char *);');", "FFI");
    refused(H, "\\FFI::cdef('x');", "FFI");
    nothing(H, "Foo::bar();");
    refused(H, "new ReflectionFunction('system');", "reflection");
    refused(H, "new ReflectionMethod('A', 'b');", "reflection");
    refused(H, "new FFI();", "reflection");
    nothing(H, "new Foo();");
    // A string that names a spawning function.
    refused(H, "$f = 'system';", "named by text");
    refused(H, "$f = \"system\";", "named by text");
    refused(H, "$f = 'SHELL_EXEC';", "named by text");
    nothing(H, "$f = 'sys';");
}

#[test]
fn php_reads_the_files_it_includes() {
    for kind in ["include", "include_once", "require", "require_once"] {
        assert_eq!(
            imports(H, &format!("{kind} 'a.php';")),
            strs(&["a.php"]),
            "{kind}"
        );
    }
    assert_eq!(imports(H, "include('a.php');"), strs(&["a.php"]));
    refused(H, "include $x;", "named by a value");
    refused(H, "require_once __DIR__ . '/a.php';", "named by a value");
    assert_eq!(
        imports(Lang::Php, "<?php include 'a.php';"),
        strs(&["a.php"])
    );
}

// ----------------------------------------------------------------- perl

use Lang::Perl as L;

#[test]
fn perl_quoted_literals_are_read_with_their_rest() {
    let q = |s: &str, perl: bool| quoted(s, perl).map(|(w, r)| (w, r.to_owned()));
    let w = |a: &[&str], r: &str| Some((strs(a), r.to_owned()));
    assert_eq!(q("'abc' rest", false), w(&["abc"], " rest"));
    assert_eq!(q("   'abc'rest", false), w(&["abc"], "rest"));
    assert_eq!(q("\"abc\" rest", false), w(&["abc"], " rest"));
    assert_eq!(q("'a\\'b'", false), w(&["a'b"], ""));
    assert_eq!(q("'a$b'", false), w(&["a$b"], ""));
    assert_eq!(q("'a@b'", false), w(&["a@b"], ""));
    assert_eq!(q("\"a$b\"", false), None);
    assert_eq!(q("\"a@b\"", false), None);
    assert_eq!(q("'\u{e9}' x", false), w(&["\u{e9}"], " x"));
    assert_eq!(q("'abc", false), None);
    assert_eq!(q("", false), None);
    assert_eq!(q("   ", false), None);
    assert_eq!(q("abc", false), None);
    assert_eq!(q("q(abc)", false), None);
    assert_eq!(q("q(abc)rest", true), w(&["abc"], "rest"));
    assert_eq!(q("q(a (b) c) rest", true), w(&["a (b) c"], " rest"));
    assert_eq!(q("q{a {b} c}x", true), w(&["a {b} c"], "x"));
    assert_eq!(q("q[a [b] c]x", true), w(&["a [b] c"], "x"));
    assert_eq!(q("q<a <b> c>x", true), w(&["a <b> c"], "x"));
    assert_eq!(q("q(a(b(c)d)e)x", true), w(&["a(b(c)d)e"], "x"));
    assert_eq!(q("q/abc/x", true), w(&["abc"], "x"));
    assert_eq!(q("q (abc)x", true), w(&["abc"], "x"));
    assert_eq!(q("q   {abc}x", true), w(&["abc"], "x"));
    assert_eq!(q("qq(abc)x", true), w(&["abc"], "x"));
    assert_eq!(q("qq (abc)x", true), w(&["abc"], "x"));
    assert_eq!(q("qq(a$b)", true), None);
    assert_eq!(q("qq(a@b)", true), None);
    assert_eq!(q("q(a$b)x", true), w(&["a$b"], "x"));
    assert_eq!(q("qw(a b  c)x", true), w(&["a", "b", "c"], "x"));
    assert_eq!(q("qw (a b)x", true), w(&["a", "b"], "x"));
    assert_eq!(q("q(a\\)b)x", true), w(&["a)b"], "x"));
    assert_eq!(q("q\u{ab}abc\u{ab}x", true), w(&["abc"], "x"));
    assert_eq!(q("q(abc", true), None);
    // `q` followed by a name character is a word, not a quote.
    for s in ["quit", "q_", "q1", "q,", "q;", "q=", "q"] {
        assert_eq!(q(s, true), None, "{s}");
    }
    assert_eq!(q("q'abc'x", true), w(&["abc"], "x"));
    // The same, with a closing delimiter to find.
    for s in [
        "q_abc_x", "q,abc,x", "q;abc;x", "q=abc=x", "q1abc1x", "qxabcx", "qqxabcx", "qwxa bx",
    ] {
        assert_eq!(q(s, true), None, "{s}");
    }
}

#[test]
fn perl_literal_lists_end_where_a_statement_ends() {
    let l = |s: &str| literal_list(s, true);
    assert_eq!(l("('a', 'b')"), Some(strs(&["a", "b"])));
    assert_eq!(l(" ('a' , 'b' ) "), Some(strs(&["a", "b"])));
    assert_eq!(l("('a' => 'b')"), Some(strs(&["a", "b"])));
    assert_eq!(l("'a', q(b), \"c\""), Some(strs(&["a", "b", "c"])));
    assert_eq!(l("qw(a b), 'c'"), Some(strs(&["a", "b", "c"])));
    assert_eq!(l("'a';"), Some(strs(&["a"])));
    assert_eq!(l("'a'"), Some(strs(&["a"])));
    assert_eq!(l("'a' }"), Some(strs(&["a"])));
    // A newline after trimming never ends a list: the spaces go first.
    assert_eq!(l("'a'\nx"), None);
    assert_eq!(l("'a' or die"), Some(strs(&["a"])));
    assert_eq!(l("'a' and x"), Some(strs(&["a"])));
    assert_eq!(l("'a' || x"), Some(strs(&["a"])));
    assert_eq!(l("'a' && x"), Some(strs(&["a"])));
    assert_eq!(l("'a' if x"), Some(strs(&["a"])));
    assert_eq!(l("'a' unless x"), Some(strs(&["a"])));
    assert_eq!(l("'a' x"), None);
    assert_eq!(l("'a' orx"), None);
    assert_eq!(l("'a' | x"), None);
    assert_eq!(l("'a' & x"), None);
    assert_eq!(l("('a'"), None);
    assert_eq!(l("('a';"), None);
    assert_eq!(l("('a') x"), Some(strs(&["a"])));
    assert_eq!(l("$x"), None);
    assert_eq!(l("'a', $x"), None);
    assert_eq!(l(""), None);
}

#[test]
fn perl_whole_words_are_found_by_their_neighbors() {
    assert_eq!(word_at("system(1) system", "system"), vec![0, 10]);
    assert_eq!(word_at("x; system", "system"), vec![3]);
    assert_eq!(word_at("system", "system"), vec![0]);
    assert_eq!(word_at("a.system", "system"), vec![2]);
    for before in ["x", "_", "9", "$", "@", "%", "-", ">"] {
        assert_eq!(
            word_at(&format!("{before}system"), "system"),
            Vec::<usize>::new(),
            "{before}"
        );
    }
    for after in ["x", "_", "9"] {
        assert_eq!(
            word_at(&format!("system{after}"), "system"),
            Vec::<usize>::new(),
            "{after}"
        );
    }
    for before in [" ", "(", ";", "{", "\n", "&", "!", "="] {
        assert_eq!(
            word_at(&format!("{before}system"), "system"),
            vec![1],
            "{before:?}"
        );
    }
    assert_eq!(word_at("system system_x system", "system"), vec![0, 16]);
    assert_eq!(word_at("$system system", "system"), vec![8]);
    assert_eq!(word_at("", "system"), Vec::<usize>::new());
}

#[test]
fn perl_reads_each_spawning_form() {
    script(L, "system('curl x');", &["curl x"]);
    script(L, "print 1; system('curl x');", &["curl x"]);
    script(L, "exec 'curl x';", &["curl x"]);
    argv(L, "system 'curl', 'x';", &[&["curl", "x"]]);
    argv(L, "print 1; exec('curl', 'x');", &[&["curl", "x"]]);
    argv(L, "system(qw(curl x));", &[&["curl", "x"]]);
    refused(L, "system();", "not literal");
    refused(L, "system { $prog } @args;", "program by a value");
    refused(L, "exec {'/bin/sh'} 'sh', 'x';", "program by a value");
    refused(L, "system $cmd;", "program by a value");
    refused(L, "system('curl', $x);", "not literal");
    refused(L, "system \"curl $x\";", "not literal");
    script(L, "my $o = readpipe('curl x');", &["curl x"]);
    refused(L, "readpipe($c);", "`readpipe`");
    script(L, "my $o = `curl x`;", &["curl x"]);
    script(
        L,
        "my $o = `curl x`; my $p = `curl y`;",
        &["curl x", "curl y"],
    );
    refused(
        L,
        "my $o = `curl $x`;",
        "backquoted command with a variable",
    );
    refused(
        L,
        "my $o = `curl @x`;",
        "backquoted command with a variable",
    );
    refused(L, "my $o = `curl x;", "backquoted");
    script(L, "my $o = qx{curl x};", &["curl x"]);
    script(L, "print 1; my $o = qx(curl x);", &["curl x"]);
    script(L, "my $o = qx'echo $HOME';", &["echo $HOME"]);
    refused(L, "my $o = qx{echo $HOME};", "qx` command with a variable");
    refused(L, "my $o = qx{echo @x};", "qx` command with a variable");
    refused(L, "my $o = qx(echo", "qx` command kGraf cannot read");
    // `qx` that is no quote operator.
    for code in ["qx = 5;", "qx, 1;", "qx;", "qx 1;", "qx_ = 1;"] {
        nothing(L, code);
    }
    // `open` in its forms.
    script(L, "open(F, \"curl x |\");", &["curl x"]);
    script(L, "open(F, \"| curl x\");", &["curl x"]);
    script(L, "open F, \"curl x |\";", &["curl x"]);
    script(L, "open F, \"curl x |\" or die;", &["curl x"]);
    script(L, "open F, \"curl x |\"\n;", &["curl x"]);
    script(L, "open(my $f, '-|', 'curl x');", &["curl x"]);
    argv(L, "open(my $f, '-|', 'curl', 'x');", &[&["curl", "x"]]);
    argv(L, "open(my $f, '|-', 'curl', 'x');", &[&["curl", "x"]]);
    nothing(L, "open(F, '<', 'file');");
    nothing(L, "open(F, '<file');");
    nothing(L, "open(F, 'x|', 'y');");
    refused(L, "open(F, $path);", "two-argument `open` given a value");
    refused(L, "open(F, $mode, 'x');", "mode given by a value");
    refused(L, "open(F, '-|', $cmd);", "not literal");
    refused(L, "open(F, 'x'", "`open` kGraf cannot read");
    nothing(L, "print 'open';");
}

#[test]
fn perl_refuses_text_run_as_code_and_values_called_by_name() {
    refused(L, "eval $code;", "`eval`");
    refused(L, "print 1; eval $code;", "`eval`");
    nothing(L, "print 1; eval { 1 };");
    nothing(L, "eval { 1 };");
    for code in [
        "do 'x.pl';",
        "do \"x.pl\";",
        "do $f;",
        "print 1; do 'x.pl';",
    ] {
        refused(L, code, "`do FILE`");
    }
    nothing(L, "do { 1 } while 0;");
    for code in [
        "require 'x.pl';",
        "require \"x.pl\";",
        "require $f;",
        "print 1; require 'x';",
    ] {
        refused(L, code, "`require FILE`");
    }
    nothing(L, "require strict;");
    refused(L, "use IPC::Open3;", "starts programs");
    for marker in [
        "&$f(1);",
        "&{$f}(1);",
        "$o->$m();",
        "*{$n} = 1;",
        "CORE::GLOBAL::foo();",
        "$o->can('x');",
    ] {
        refused(L, marker, "reached by a value");
    }
    nothing(L, "print 1 for 1..3;");
}

#[test]
fn perl_substitutions_with_the_e_flag_run_their_replacement() {
    for code in [
        "s/a/b/e",
        "$x =~ s/a/b/e;",
        "x = 1; $y =~ s/a/b/e;",
        "$y =~ s/a/b/ge;",
        "$y =~ s/a/b/gie;",
        "$y =~ s/a/b/ee;",
        "$y =~ s|a|b|e;",
        "$y =~ s#a#b#e;",
        "$y =~ s!a!b!e;",
        "$y =~ s{a}{b}e;",
        "$y =~ s{a} {b}e;",
        "$y =~ s{a}\n{b}e;",
        "$y =~ s(a)(b)e;",
        "$y =~ s[a][b]e;",
        "$y =~ s<a><b>e;",
        "$y =~ s{a{1}}{b}e;",
        "$y =~ s{a}{b{c}}e;",
        "$y =~ s/a\\/b/c/e;",
        "$y =~ s/a/b\\/c/e;",
        "print 1; $y =~ s{a}{b}e",
        "$y =~ s{a} (b)e;",
        "$y =~ s{a{1}}(b)e;",
        "$y =~ s(a(1))[b]e;",
        "$y =~ s<a<1>>{b}e;",
        "$y =~ s[a[1]]<b>e;",
        "print 1; $y =~ s{a{1}{2}}(b)e;",
        "$y =~ s/(a)/uc($1)/e;",
    ] {
        assert!(substitution_with_e(code), "{code}");
    }
    for code in [
        "s/a/b/",
        "$y =~ s/a/b/g;",
        "$y =~ s/a/b/gi;",
        "$y =~ s{a}{b}g;",
        "$y =~ s{a} {b};",
        "$y =~ s(a)(b);",
        "$y =~ s/a/b/;e",
        "$y =~ s/a/b/ e",
        "$y =~ s/a\\/b/c/g; e",
        "$y =~ s{a{1}}{b};",
        "s",
        "print s;",
        "s,a,b,e",
        "s;a;b;e",
        "s)a)b)e",
        "s=a=b=e",
        "s>a>b>e",
        "s_a_b_e",
        "s a b e",
        "$x = s1a1b1e",
        "this is es",
        "$y =~ s/a",
        "$y =~ s{a}",
        "$y =~ s{a}{",
        "$y =~ s{a}",
        "",
    ] {
        assert!(!substitution_with_e(code), "{code}");
    }
}

// ---------------------------------------------------------------------- awk

#[test]
fn awk_tokens_skip_comments_and_regular_expressions() {
    use AwkToken::{Other as O, Pipe, Str, Word as W};
    let w = |s: &str| W(s.to_owned());
    let s = |x: &str| Str(x.to_owned());
    assert_eq!(
        awk_tokens("print \"a\" | \"sort\""),
        vec![w("print"), s("a"), Pipe, s("sort")]
    );
    assert_eq!(
        awk_tokens("x = 1 # note\n y"),
        vec![w("x"), O('='), w("1"), w("y")]
    );
    assert_eq!(awk_tokens("# c\n\"x\""), vec![s("x")]);
    assert_eq!(awk_tokens("x # end"), vec![w("x")]);
    assert_eq!(awk_tokens("#"), Vec::<AwkToken>::new());
    assert_eq!(awk_tokens("a || b"), vec![w("a"), O('|'), O('|'), w("b")]);
    assert_eq!(awk_tokens("a |& b"), vec![w("a"), Pipe, w("b")]);
    assert_eq!(awk_tokens("a | b"), vec![w("a"), Pipe, w("b")]);
    assert_eq!(awk_tokens("a |"), vec![w("a"), Pipe]);
    assert_eq!(awk_tokens("a ||"), vec![w("a"), O('|'), O('|')]);
    assert_eq!(
        awk_tokens("/a|b/ { n }"),
        vec![w("REGEX"), O('{'), w("n"), O('}')]
    );
    assert_eq!(awk_tokens("/a\\/b/ x"), vec![w("REGEX"), w("x")]);
    assert_eq!(awk_tokens("/abc"), vec![w("REGEX")]);
    assert_eq!(awk_tokens("/a\\"), vec![w("REGEX")]);
    assert_eq!(awk_tokens("/"), vec![w("REGEX")]);
    assert_eq!(
        awk_tokens("a / b / c"),
        vec![w("a"), O('/'), w("b"), O('/'), w("c")]
    );
    assert_eq!(
        awk_tokens("(x) / 2"),
        vec![O('('), w("x"), O(')'), O('/'), w("2")]
    );
    assert_eq!(
        awk_tokens("a[1] / 2"),
        vec![w("a"), O('['), w("1"), O(']'), O('/'), w("2")]
    );
    assert_eq!(awk_tokens("\"s\" / 2"), vec![s("s"), O('/'), w("2")]);
    assert_eq!(awk_tokens("$ / 2"), vec![O('$'), O('/'), w("2")]);
    assert_eq!(awk_tokens("{ / x/ }"), vec![O('{'), w("REGEX"), O('}')]);
    assert_eq!(awk_tokens("\"a\\\"b\""), vec![s("a\"b")]);
    assert_eq!(awk_tokens("\"abc"), vec![s("abc")]);
    assert_eq!(awk_tokens("\"a\\"), vec![s("a\\")]);
    assert_eq!(awk_tokens("\"\""), vec![s("")]);
    assert_eq!(awk_tokens("abc"), vec![w("abc")]);
    assert_eq!(awk_tokens("a_b1 c"), vec![w("a_b1"), w("c")]);
    assert_eq!(awk_tokens("a;b"), vec![w("a"), O(';'), w("b")]);
    assert_eq!(awk_tokens(""), Vec::<AwkToken>::new());
    assert_eq!(awk_tokens("  \t\n "), Vec::<AwkToken>::new());
}

#[test]
fn awk_reads_system_calls_and_pipes_to_and_from_commands() {
    use Lang::Awk as A;
    script(A, "{ system(\"curl x\") }", &["curl x"]);
    script(
        A,
        "BEGIN { print 1 } END { system(\"curl x\") }",
        &["curl x"],
    );
    refused(A, "{ system($0) }", "`system` given a value");
    refused(A, "{ system(\"a\" \"b\") }", "`system` given a value");
    refused(A, "{ system }", "`system` given a value");
    script(A, "BEGIN { \"curl x\" | getline v }", &["curl x"]);
    script(A, "{ print $1 | \"sort -u\" }", &["sort -u"]);
    script(A, "{ print $1 |& \"sort -u\" }", &["sort -u"]);
    refused(A, "BEGIN { \"curl x\" | foo }", "pipe to or from a command");
    refused(A, "{ print | cmd }", "pipe to or from a command");
    refused(A, "{ cmd | getline }", "pipe to or from a command");
    refused(A, "{ print | \"a\" \"b\" }", "pipe to or from a command");
    for op in ["+", "-", "*", "/", "%", "^"] {
        refused(
            A,
            &format!("{{ print | \"a\" {op} 1 }}"),
            "pipe to or from a command",
        );
    }
    script(A, "{ print | \"a\" ; x = 1 }", &["a"]);
    script(A, "{ print | \"a\" }", &["a"]);
    script(A, "{ print | \"a\" } # note", &["a"]);
    nothing(A, "/a|b/ { print $1 } $2 > 1 || $3 { n++ }");
    nothing(A, "{ print a / b / c }");
    refused(A, "@load \"filefuncs\"", "extension");
    refused(A, "@include \"lib\"", "library");
    refused(A, "{ x = 1 } @load \"f\"", "extension");
}

#[test]
fn applescript_reads_do_shell_script_and_refuses_what_it_cannot_read() {
    use Lang::AppleScript as S;
    script(S, "do shell script \"curl x\"", &["curl x"]);
    script(S, "DO SHELL SCRIPT \"curl x\"", &["curl x"]);
    script(
        S,
        "do shell script \"curl x\"\ndo shell script \"curl y\"",
        &["curl x", "curl y"],
    );
    script(
        S,
        "display dialog \"a\"\ndo shell script \"curl x\"\nbeep\ndo shell script \"curl y\"\nbeep",
        &["curl x", "curl y"],
    );
    script(S, "do script \"curl x\"", &["curl x"]);
    script(S, "doShellScript(\"curl x\")", &["curl x"]);
    refused(S, "do shell script cmd", "not literal");
    refused(S, "do shell script \"curl \" & u", "joined to a value");
    refused(S, "do shell script \"curl \" + u", "joined to a value");
    refused(
        S,
        "do shell script \"curl x\"\ndo shell script cmd",
        "not literal",
    );
    nothing(S, "display notification \"hi\"");
    for marker in [
        "run script x",
        "load script x",
        "store script x",
        "keystroke \"a\"",
        "key code 1",
        "\u{ab}event x",
        "$.NSTask",
        "ObjC.import('x')",
        "eval(x)",
    ] {
        refused(S, marker, "whose effect kGraf cannot read");
    }
}
