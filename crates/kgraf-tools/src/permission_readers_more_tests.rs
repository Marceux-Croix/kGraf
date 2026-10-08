//! The dispatch of programs to their readers, and the argument walkers of
//! the tool runners and container commands.

use super::*;

fn strs(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

fn tree() -> (tempfile::TempDir, ReadContext) {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(dir.path().join("Makefile"), "all:\n\techo hi\n").expect("write");
    std::fs::write(dir.path().join("justfile"), "r:\n    echo hi\n").expect("write");
    let ctx = ReadContext::in_worktree(dir.path(), None);
    (dir, ctx)
}

fn read(ctx: &ReadContext, words: &[&str]) -> Option<Vec<Embedded>> {
    embedded_by_reader(&strs(words), 0, ctx)
}

fn opaque(found: &[Embedded]) -> bool {
    found.iter().any(|e| matches!(e, Embedded::Unreadable(_)))
}

#[test]
fn each_program_goes_to_the_reader_of_its_family() {
    let (_d, ctx) = tree();
    // Windows shells.
    let cmd = read(&ctx, &["cmd", "/c", "curl", "x"]).expect("cmd");
    assert_eq!(cmd, vec![Embedded::Argv(strs(&["curl", "x"]))]);
    for shell in ["powershell", "pwsh"] {
        let found = read(&ctx, &[shell, "-Command", "curl x"]).expect(shell);
        assert_eq!(found, vec![Embedded::Argv(strs(&["curl", "x"]))], "{shell}");
    }
    // git: a plain command is read and runs nothing else.
    assert_eq!(read(&ctx, &["git", "status"]), Some(Vec::new()));
    // Build tools.
    for make in ["make", "gmake"] {
        assert_eq!(
            read(&ctx, &[make]),
            Some(vec![Embedded::Script("echo hi".into())]),
            "{make}"
        );
    }
    assert_eq!(
        read(&ctx, &["just"]),
        Some(vec![Embedded::Script("echo hi".into())])
    );
    let found = read(&ReadContext::default(), &["cargo", "xt"]).expect("cargo");
    assert!(opaque(&found));
    assert_eq!(read(&ctx, &["cargo", "fmt"]), None);
    // deno: a task is unreadable; a version question is not read.
    assert!(opaque(&read(&ctx, &["deno", "task", "x"]).expect("deno")));
    assert_eq!(read(&ctx, &["deno", "fmt"]), None);
    // An interpreter, and a program no reader knows.
    assert!(opaque(&read(&ctx, &["lua", "x.lua"]).expect("lua")));
    assert_eq!(read(&ctx, &["python3", "--version"]), None);
    assert_eq!(read(&ctx, &["ls", "-l"]), None);
    assert_eq!(read(&ctx, &["lsx"]), None);
}

#[test]
fn a_tool_runner_skips_its_options_and_runs_the_package() {
    let t = |args: &[&str]| tool_runner("uvx", &strs(args), &["--from", "--python"]);
    let argv = |w: &[&str]| vec![Embedded::Argv(strs(w))];
    assert_eq!(t(&["ruff", "check"]), argv(&["ruff", "check"]));
    assert_eq!(t(&["-q", "ruff", "check"]), argv(&["ruff", "check"]));
    assert_eq!(t(&["-q", "-v", "ruff"]), argv(&["ruff"]));
    assert_eq!(
        t(&["-q", "--python", "3.11", "ruff", "a"]),
        argv(&["ruff", "a"])
    );
    assert_eq!(t(&["-q", "--from", "pkg", "ruff"]), argv(&["ruff"]));
    // After `--`, the next word is the package even when it starts with `-`.
    assert_eq!(t(&["-q", "--", "ruff", "a"]), argv(&["ruff", "a"]));
    assert_eq!(t(&["--", "ruff"]), argv(&["ruff"]));
    assert_eq!(t(&["-q", "--", "-weird"]), argv(&["-weird"]));
    assert_eq!(t(&["--", "-weird"]), argv(&["-weird"]));
    // A spec that is no registry name.
    assert!(opaque(&t(&["./local"])));
    assert!(opaque(&t(&["https://example.com/x.whl"])));
    // No package: nothing runs.
    assert!(t(&[]).is_empty());
    assert!(t(&["-q"]).is_empty());
    assert!(t(&["--"]).is_empty());
}

#[test]
fn container_options_are_walked_by_their_kind() {
    let valued = ["-e", "--env"];
    let flags = ["-d", "--detach"];
    let p = |args: &[&str]| past_options(&strs(args), &valued, &flags, "it");
    assert_eq!(p(&["web", "ls"]), Some(0));
    assert_eq!(p(&["-d", "web", "ls"]), Some(1));
    assert_eq!(p(&["-d", "-d", "web"]), Some(2));
    assert_eq!(p(&["-e", "A=1", "web"]), Some(2));
    assert_eq!(p(&["--env", "A=1", "web"]), Some(2));
    assert_eq!(p(&["--env=A=1", "web"]), Some(1));
    assert_eq!(p(&["-d", "-e", "A=1", "web"]), Some(3));
    assert_eq!(p(&["-it", "web"]), Some(1));
    assert_eq!(p(&["-i", "-t", "web"]), Some(2));
    assert_eq!(p(&["-eA=1", "web"]), Some(1));
    assert_eq!(p(&["-d", "-eA=1", "web"]), Some(2));
    assert_eq!(p(&["--", "web"]), Some(1));
    assert_eq!(p(&["-d", "--", "-x"]), Some(2));
    assert_eq!(p(&["-"]), Some(0));
    assert_eq!(p(&["-d", "-"]), Some(1));
    assert_eq!(p(&[]), Some(0));
    assert_eq!(p(&["-d"]), Some(1));
    // An option kGraf does not know, a long one among them.
    assert_eq!(p(&["-x", "web"]), None);
    assert_eq!(p(&["--unknown", "web"]), None);
    assert_eq!(p(&["-d", "--unknown"]), None);
    assert_eq!(p(&["-itx", "web"]), None);
    assert_eq!(p(&["-z"]), None);
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

fn argv_of(words: &[&str]) -> Vec<Embedded> {
    vec![Embedded::Argv(strs(words))]
}

fn script_of(text: &str) -> Vec<Embedded> {
    vec![Embedded::Script(text.to_owned())]
}

#[test]
fn kubectl_runs_the_command_after_the_double_dash() {
    let k = |args: &[&str]| kubectl("kubectl", &strs(args));
    assert_eq!(k(&["exec", "pod", "--", "ls", "a"]), argv_of(&["ls", "a"]));
    assert_eq!(k(&["run", "x", "--image=y", "--", "ls"]), argv_of(&["ls"]));
    assert_eq!(k(&["debug", "pod", "--", "ls"]), argv_of(&["ls"]));
    // Global options before the sub-command, one that takes a value among them.
    assert_eq!(
        k(&["-n", "ns", "exec", "pod", "--", "ls"]),
        argv_of(&["ls"])
    );
    assert_eq!(
        k(&["--insecure", "-n", "ns", "exec", "pod", "--", "ls"]),
        argv_of(&["ls"])
    );
    assert_eq!(
        k(&["--insecure", "--context", "c", "exec", "pod", "--", "ls"]),
        argv_of(&["ls"])
    );
    assert_eq!(
        k(&["--insecure", "--v=1", "exec", "pod", "--", "ls"]),
        argv_of(&["ls"])
    );
    // Other sub-commands run nothing.
    assert!(k(&["get", "pods"]).is_empty());
    assert!(k(&["-n", "ns", "get", "pods"]).is_empty());
    assert!(k(&["--insecure", "-n", "ns", "get", "pods"]).is_empty());
    assert!(k(&[]).is_empty());
    // `--overrides` changes what runs.
    for o in ["--overrides", "--overrides={}"] {
        let found = k(&["run", "x", o, "--", "ls"]);
        assert!(why(&found).contains("`--overrides`"), "{o}");
    }
    // No command after `--`, or no `--`.
    let found = k(&["exec", "pod", "--"]);
    assert!(
        why(&found).contains("`exec` without `--`"),
        "{}",
        why(&found)
    );
    let found = k(&["exec", "pod"]);
    assert!(
        why(&found).contains("`exec` without `--`"),
        "{}",
        why(&found)
    );
    let found = k(&["run", "x", "--image=y"]);
    assert!(
        why(&found).contains("image's own command"),
        "{}",
        why(&found)
    );
    let found = k(&["run", "x", "--image=y", "--"]);
    assert!(
        why(&found).contains("image's own command"),
        "{}",
        why(&found)
    );
    let found = k(&["debug", "pod"]);
    assert!(
        why(&found).contains("image's own command"),
        "{}",
        why(&found)
    );
}

#[test]
fn lxc_exec_runs_the_command_after_the_instance() {
    let l = |args: &[&str]| instance_exec("lxc", &strs(args));
    assert_eq!(
        l(&["exec", "web", "--", "ls", "-l"]),
        argv_of(&["ls", "-l"])
    );
    assert_eq!(l(&["exec", "web", "ls"]), argv_of(&["ls"]));
    assert_eq!(l(&["exec", "web", "ls", "-l"]), argv_of(&["ls", "-l"]));
    assert_eq!(l(&["exec", "-T", "web", "ls"]), argv_of(&["ls"]));
    assert_eq!(
        l(&["exec", "--env", "A=1", "web", "ls", "a"]),
        argv_of(&["ls", "a"])
    );
    assert_eq!(l(&["exec", "-n", "-t", "web", "ls"]), argv_of(&["ls"]));
    assert_eq!(
        l(&["exec", "--project", "p", "web", "--", "ls"]),
        argv_of(&["ls"])
    );
    // Other sub-commands run nothing.
    assert!(l(&["list"]).is_empty());
    assert!(l(&[]).is_empty());
    // No command starts a shell.
    for args in [
        &["exec", "web", "--"][..],
        &["exec", "web"],
        &["exec"],
        &["exec", "--"],
    ] {
        let found = l(args);
        assert!(
            why(&found).contains("starts a shell"),
            "{args:?}: {}",
            why(&found)
        );
    }
    let found = l(&["exec", "--bogus", "web", "ls"]);
    assert!(why(&found).contains("option kGraf cannot place"));
}

#[test]
fn adb_runs_the_shell_command_after_its_options() {
    let a = |args: &[&str]| adb(&strs(args));
    assert_eq!(a(&["shell", "ls", "-l"]), script_of("ls -l"));
    assert_eq!(
        a(&["-s", "serial", "shell", "ls", "-l"]),
        script_of("ls -l")
    );
    assert_eq!(a(&["-d", "shell", "ls"]), script_of("ls"));
    assert_eq!(a(&["shell", "-t", "ls"]), script_of("ls"));
    assert_eq!(a(&["shell", "-t", "-x", "ls"]), script_of("ls"));
    assert_eq!(a(&["shell", "--foo", "ls"]), script_of("--foo ls"));
    assert_eq!(a(&["shell", "ls", "-l", "-a"]), script_of("ls -l -a"));
    assert_eq!(a(&["shell", "-t", "ls", "-l"]), script_of("ls -l"));
    assert!(a(&["devices"]).is_empty());
    assert!(a(&["devices", "-l"]).is_empty());
    assert!(a(&[]).is_empty());
    for args in [&["shell"][..], &["shell", "-t"], &["shell", "-t", "-x"]] {
        let found = a(args);
        assert!(
            why(&found).contains("starts a shell on the device"),
            "{args:?}"
        );
    }
    assert!(why(&a(&["-Z", "shell", "ls"])).contains("option kGraf cannot place"));
}

#[test]
fn wsl_runs_the_command_through_the_linux_shell() {
    let w = |args: &[&str]| wsl(&strs(args));
    assert_eq!(w(&["ls", "-l"]), script_of("ls -l"));
    assert_eq!(w(&["-d", "Ubuntu", "ls"]), script_of("ls"));
    assert_eq!(
        w(&["--distribution", "Ubuntu", "--user", "u", "ls", "a"]),
        script_of("ls a")
    );
    assert_eq!(w(&["-x", "-d", "Ubuntu", "ls"]), script_of("ls"));
    assert_eq!(w(&["-x", "ls"]), script_of("ls"));
    assert_eq!(w(&["--", "ls", "-l"]), script_of("ls -l"));
    assert_eq!(w(&["-x", "--", "ls"]), script_of("ls"));
    assert_eq!(w(&["-e", "ls", "-l"]), argv_of(&["ls", "-l"]));
    assert_eq!(w(&["--exec", "ls"]), argv_of(&["ls"]));
    assert_eq!(w(&["-d", "u", "-e", "ls"]), argv_of(&["ls"]));
    // Management commands run nothing of the caller's.
    for flag in [
        "--list",
        "-l",
        "--shutdown",
        "--status",
        "--update",
        "-t",
        "--version",
        "--help",
        "-s",
    ] {
        assert!(w(&[flag]).is_empty(), "{flag}");
        assert!(w(&["-d", "u", flag]).is_empty(), "{flag}");
    }
    // No command starts a shell.
    for args in [&[][..], &["-d", "u"], &["--"]] {
        assert!(why(&w(args)).contains("starts a Linux shell"), "{args:?}");
    }
    assert!(why(&w(&["-e"])).contains("names no command"));
    assert!(why(&w(&["-d", "u", "--exec"])).contains("names no command"));
}

#[test]
fn a_script_path_is_read_as_its_interpreter_given_the_file() {
    let dir = tempfile::tempdir().expect("dir");
    let w = |name: &str, bytes: &[u8]| std::fs::write(dir.path().join(name), bytes).expect("write");
    w("env.py", b"#!/usr/bin/env python3\nprint(1)\n");
    w("env_s.py", b"#!/usr/bin/env -S python3 -u\nprint(1)\n");
    w("sh.sh", b"#!/bin/sh -e\nls\n");
    w("bare.sh", b"#!/bin/bash\nls\n");
    w("plain.txt", b"just text\n");
    // Not `nul.bin`: on Windows `NUL` with any extension is a device.
    w("zero.bin", b"\0\x01\x02 binary");
    w("utf.bin", &[0xff, 0xfe, 0x00, 0x01]);
    w("space.sh", b"#!  /bin/sh   -x  \nls\n");
    let ctx = ReadContext::in_worktree(dir.path(), None);
    let p = |program: &str, args: &[&str]| path_program(program, &strs(args), &ctx);
    assert_eq!(
        p("env.py", &["a"]),
        Some(argv_of(&["/usr/bin/env", "python3", "env.py", "a"]))
    );
    assert_eq!(
        p("env_s.py", &[]),
        Some(argv_of(&[
            "/usr/bin/env",
            "-S",
            "python3",
            "-u",
            "env_s.py"
        ]))
    );
    assert_eq!(
        p("sh.sh", &["x", "y"]),
        Some(argv_of(&["/bin/sh", "-e", "sh.sh", "x", "y"]))
    );
    assert_eq!(p("bare.sh", &[]), Some(argv_of(&["/bin/bash", "bare.sh"])));
    assert_eq!(
        p("space.sh", &[]),
        Some(argv_of(&["/bin/sh", "-x", "space.sh"]))
    );
    // A text file with no `#!` line runs in a shell, which is not read.
    let found = p("plain.txt", &[]).expect("read");
    assert!(why(&found).contains("no `#!` line"), "{}", why(&found));
    // A compiled program, a file that is not there, or a binary with a NUL.
    assert_eq!(p("zero.bin", &[]), None);
    assert_eq!(p("utf.bin", &[]), None);
    assert_eq!(p("missing", &[]), None);
}

/// On Windows a device name, a stream or a device path is never placed in
/// the worktree, so a program named by one is unreadable (asked about)
/// rather than taken as a program by its name.
#[cfg(windows)]
#[test]
fn a_program_named_by_a_windows_form_kgraf_cannot_place_is_unreadable() {
    let (_dir, ctx) = tree();
    for program in [
        r".\nul.bin",
        "./CON",
        r".\x.sh:s",
        "/c/x/make",
        r"\\.\C:\x\make",
    ] {
        let found = read(&ctx, &[program]).expect("read");
        assert!(
            why(&found).contains("which may be a script of the worktree"),
            "{program}: {}",
            why(&found)
        );
    }
    // A path in Git Bash's own tree is the program it names, as before.
    assert!(!opaque(
        &read(&ctx, &["/usr/bin/env", "ls"]).unwrap_or_default()
    ));
}

#[cfg(unix)]
#[test]
fn a_script_path_that_leaves_the_worktree_is_unreadable() {
    let dir = tempfile::tempdir().expect("dir");
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("x.sh"), "#!/bin/sh\nls\n").expect("write");
    std::os::unix::fs::symlink(outside.path().join("x.sh"), dir.path().join("link.sh"))
        .expect("ln");
    let ctx = ReadContext::in_worktree(dir.path(), None);
    let found = path_program("link.sh", &[], &ctx).expect("read");
    assert!(!why(&found).is_empty(), "{found:?}");
    assert!(!why(&found).contains("not UTF-8"));
}
