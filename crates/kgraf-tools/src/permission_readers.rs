//! What a program runs from its arguments, for the programs whose inner
//! command the generic scan in `embedded_at` does not read (decision 0094,
//! amended 2026-10-06): package runners (`npx`, `pnpm dlx`, `yarn dlx`,
//! `bunx`, `uvx`, `pipx run`), container and remote runners (`docker exec`,
//! `docker run`, `podman`, `kubectl exec`, `lxc exec`, `vagrant ssh -c`,
//! `adb shell`, `wsl`), a program named by a path that is a script file
//! (`./x.py` with a `#!` line), and the dispatch to the readers of Windows
//! shells, git, build-tool recipes and interpreter code.
//!
//! The inner command is read for the user's deny and ask rules and the
//! command policy's `forbid` rules, and a grant of the runner covers none of
//! it: each program it runs needs its own grant ([`super::programs_to_grant`]).
//! A runner that runs a command kGraf cannot name (an image's own command, a
//! package given by a URL, an interactive shell) is unreadable.
//!
//! Which runners are read is a list (decision 0094): the package runners
//! of npm, pnpm, yarn, bun, uv and pipx; docker, podman and nerdctl (`exec`, `run`, `container exec|run`,
//! `compose exec|run`), docker-compose and podman-compose; kubectl and oc
//! (`exec`, `run`, `debug`, after `--`); lxc and incus `exec`; vagrant `ssh
//! -c`; adb `shell`; wsl.

use super::{Embedded, ReadContext, base_name, program_key};

/// The commands the program at `words[at]` starts, when a reader of this
/// family knows the program; `None` hands it to the generic scan.
pub(super) fn embedded_by_reader(
    words: &[String],
    at: usize,
    ctx: &ReadContext,
) -> Option<Vec<Embedded>> {
    let program = &words[at];
    let key = program_key(program);
    let args = &words[at + 1..];
    // A program named by a path to a file of the worktree runs that file,
    // whatever its name says (`./make`, `bin/sh`). Without a known
    // directory kGraf cannot tell whether a relative path names such a
    // file; an absolute path is taken as the program it names, as an allow
    // set lists it (a worker's worktree path changes with every attempt).
    if program.contains('/') {
        if ctx.cwd.is_none() && !program.starts_with('/') {
            return Some(unreadable(
                program,
                "is named by a path, and kGraf cannot tell which directory it is taken from, \
                 so it may be a script of the worktree",
            ));
        }
        if super::worktree_file_exists(ctx, program) {
            return path_program(program, args, ctx);
        }
    }
    let found = match key.as_str() {
        "npx" | "pnpx" | "bunx" => package_runner(&key, args),
        "bun" if super::code::bun_inline(args, ctx).is_some() => {
            super::code::bun_inline(args, ctx)?
        }
        // `busybox APPLET ARGS` runs the applet as that program.
        "busybox" => match args.first() {
            Some(applet) if !applet.starts_with('-') => vec![Embedded::Argv(args.to_vec())],
            _ => Vec::new(),
        },
        "npm" | "pnpm" | "yarn" | "bun" => match node_runner(&key, args) {
            Some(found) => found,
            None => super::recipes::package_scripts(&key, args, ctx)?,
        },
        "uvx" => tool_runner(
            &key,
            args,
            &["--from", "--with", "-p", "--python", "--index"],
        ),
        "uv" if args.first().is_some_and(|a| a == "tool")
            && args.get(1).is_some_and(|a| a == "run" || a == "x") =>
        {
            tool_runner(
                "uv tool run",
                &args[2..],
                &["--from", "--with", "-p", "--python", "--index"],
            )
        }
        "pipx" if args.first().is_some_and(|a| a == "run") => tool_runner(
            "pipx run",
            &args[1..],
            &["--spec", "--python", "--index-url"],
        ),
        "docker" | "podman" | "nerdctl" => container(&key, args),
        "docker-compose" | "podman-compose" => compose(&key, args),
        "kubectl" | "oc" => kubectl(&key, args),
        "lxc" | "incus" => instance_exec(&key, args),
        "vagrant" => vagrant(args),
        "adb" => adb(args),
        "wsl" => wsl(args),
        "cmd" => super::windows::cmd(args),
        "powershell" | "pwsh" => super::windows::powershell(&key, args),
        "git" => super::git::git(words, at, ctx),
        "make" | "gmake" => super::recipes::make(&words[..at], args, ctx),
        "just" => super::recipes::just(args, ctx),
        "cargo" => super::recipes::cargo(words, at, ctx)?,
        "deno" => super::code::deno(args, ctx)?,
        _ if super::code::interpreter(&key).is_some() => {
            super::code::interpreter_call(super::code::interpreter(&key)?, &key, args, ctx)?
        }
        _ => return None,
    };
    Some(found)
}

fn unreadable(program: &str, why: &str) -> Vec<Embedded> {
    vec![Embedded::Unreadable(format!("`{program}` {why}"))]
}

/// The value of option `name` when `arg` is `name=VALUE`, or `name` itself
/// with the value in the next word (`next`). `None` when `arg` is not it.
fn valued<'a>(arg: &'a str, name: &str, next: Option<&'a String>) -> Option<Option<&'a str>> {
    if arg == name {
        return Some(next.map(String::as_str));
    }
    arg.strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('='))
        .map(Some)
}

/// The program a package runner runs for a package spec: the package's
/// name, without a scope (`@scope/`) or a version (`@1.2`, `==1.2`). `None`
/// for a spec that is not a registry name (a path, a URL, a git spec): the
/// program it runs cannot be named.
fn package_program(spec: &str) -> Option<String> {
    let unscoped = match spec.strip_prefix('@') {
        Some(rest) => rest.split_once('/').map(|(_, name)| name)?,
        None => spec,
    };
    let name = unscoped
        .split(['@', '=', '<', '>', '~', '!', '['])
        .next()
        .unwrap_or_default();
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.');
    plain.then(|| name.to_owned())
}

/// `npx [OPTIONS] PKG [ARGS]`, `npx -p PKG CMD`, `npx -c 'CMD'` (and `pnpx`,
/// `bunx`, `npm exec`, `npm x`, `pnpm dlx`, `yarn dlx`, `bun x`): the
/// package's program with its arguments, or the `-c` script.
fn package_runner(runner: &str, args: &[String]) -> Vec<Embedded> {
    // Options of npm and its kin that take a value in the next word.
    const VALUED: &[&str] = &[
        "--registry",
        "--cache",
        "--userconfig",
        "--prefix",
        "-w",
        "--workspace",
        "--node-options",
        "--script-shell",
        "--shell",
        "--loglevel",
    ];
    let mut packages: Vec<&str> = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        if let Some(v) = valued(a, "-c", args.get(i + 1)).or(valued(a, "--call", args.get(i + 1))) {
            return match v {
                Some(script) => vec![Embedded::Script(script.to_owned())],
                None => unreadable(
                    runner,
                    "with `-c` and no script runs nothing kGraf can read",
                ),
            };
        }
        if let Some(v) =
            valued(a, "-p", args.get(i + 1)).or(valued(a, "--package", args.get(i + 1)))
        {
            let Some(v) = v else {
                return unreadable(runner, "names no package");
            };
            packages.push(v);
            i += if a.contains('=') { 1 } else { 2 };
            continue;
        }
        if VALUED.iter().any(|o| a == o) {
            // `--shell X` or `--script-shell X` runs the `-c` script with
            // another shell; any other value is a setting.
            if (a == "--shell" || a == "--script-shell")
                && args
                    .get(i + 1)
                    .is_some_and(|s| !matches!(base_name(s), "sh" | "bash" | "dash" | "zsh"))
            {
                return unreadable(runner, "runs its script with a shell kGraf does not read");
            }
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        break;
    }
    let rest = args.get(i..).unwrap_or_default();
    match rest.split_first() {
        // With `-p PKG`, the first word is a program the package provides.
        Some((cmd, tail)) if !packages.is_empty() => {
            let mut argv = vec![cmd.clone()];
            argv.extend(tail.iter().cloned());
            vec![Embedded::Argv(argv)]
        }
        Some((spec, tail)) => match package_program(spec) {
            Some(program) => {
                let mut argv = vec![program];
                argv.extend(tail.iter().cloned());
                vec![Embedded::Argv(argv)]
            }
            None => unreadable(
                runner,
                &format!(
                    "runs the package `{}`, whose program kGraf cannot name",
                    spec.escape_debug()
                ),
            ),
        },
        None => match packages.last().and_then(|p| package_program(p)) {
            Some(program) => vec![Embedded::Argv(vec![program])],
            None if packages.is_empty() => Vec::new(),
            None => unreadable(runner, "runs a package whose program kGraf cannot name"),
        },
    }
}

/// The subcommands of npm, pnpm, yarn and bun that run a package or a
/// command rather than a script of `package.json`: `npm exec`, `npm x`,
/// `npm init PKG` and `npm create PKG` (which run `create-PKG`), `pnpm
/// dlx`, `pnpm exec`, `pnpm create`, `yarn dlx`, `yarn exec`, `yarn create`,
/// `yarn node`, `bun x`, `bun exec`, `bun create`. `None` for any other
/// subcommand (a script reader may know it).
fn node_runner(tool: &str, args: &[String]) -> Option<Vec<Embedded>> {
    // Global options before the subcommand that take a value, and those
    // that move the directory the command runs in.
    let mut i = 0;
    let mut moved = false;
    while let Some(a) = args.get(i) {
        if !a.starts_with('-') {
            break;
        }
        let takes = matches!(
            a.as_str(),
            "-C" | "--dir" | "--prefix" | "--cwd" | "-w" | "--workspace" | "--filter" | "-F"
        );
        if matches!(a.as_str(), "-C" | "--dir" | "--prefix" | "--cwd")
            || a.starts_with("--dir=")
            || a.starts_with("--prefix=")
            || a.starts_with("--cwd=")
        {
            moved = true;
        }
        i += if takes { 2 } else { 1 };
    }
    let sub = args.get(i)?.as_str();
    let rest = &args[i + 1..];
    let found = match (tool, sub) {
        ("npm", "exec" | "x") | ("pnpm", "dlx") | ("yarn", "dlx") | ("bun", "x") => {
            package_runner(&format!("{tool} {sub}"), rest)
        }
        ("pnpm" | "yarn", "exec") => {
            // `pnpm exec CMD ARGS`; `pnpm -c exec 'CMD'` runs a shell.
            let shell_mode = args[..i].iter().any(|a| a == "-c" || a == "--shell-mode")
                || rest
                    .first()
                    .is_some_and(|a| a == "-c" || a == "--shell-mode");
            let cmd: Vec<String> = rest
                .iter()
                .skip_while(|a| a.starts_with('-'))
                .cloned()
                .collect();
            if cmd.is_empty() {
                Vec::new()
            } else if shell_mode {
                vec![Embedded::Script(cmd.join(" "))]
            } else {
                vec![Embedded::Argv(cmd)]
            }
        }
        ("bun", "exec") => match rest.iter().find(|a| !a.starts_with('-')) {
            Some(script) => vec![Embedded::Script(script.clone())],
            None => Vec::new(),
        },
        ("yarn", "node") => {
            let mut argv = vec!["node".to_owned()];
            argv.extend(rest.iter().cloned());
            vec![Embedded::Argv(argv)]
        }
        ("npm", "init" | "create" | "innit") | ("pnpm" | "yarn" | "bun", "create") => {
            // `npm init PKG` runs `create-PKG` (`npm init @scope` runs
            // `@scope/create`); `npm init` alone writes `package.json`.
            match rest.iter().find(|a| !a.starts_with('-')) {
                Some(spec) => {
                    let name = match spec.strip_prefix('@') {
                        Some(scoped) => match scoped.split_once('/') {
                            Some((_, name)) => format!("create-{name}"),
                            None => "create".to_owned(),
                        },
                        None => format!("create-{spec}"),
                    };
                    match package_program(&name) {
                        Some(program) => vec![Embedded::Argv(vec![program])],
                        None => unreadable(
                            tool,
                            "runs an initializer package whose program kGraf cannot name",
                        ),
                    }
                }
                None => Vec::new(),
            }
        }
        _ => return None,
    };
    Some(if moved {
        found
            .into_iter()
            .map(|f| Embedded::In(ReadContext::default(), Box::new(f)))
            .collect()
    } else {
        found
    })
}

/// `uvx [OPTIONS] PKG [ARGS]`, `uv tool run`, `pipx run`: the package's
/// program with its arguments (`--from SPEC CMD` runs CMD).
fn tool_runner(runner: &str, args: &[String], valued_options: &[&str]) -> Vec<Embedded> {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') {
            break;
        }
        i += if valued_options.contains(&a.as_str()) {
            2
        } else {
            1
        };
    }
    match args.get(i..).unwrap_or_default().split_first() {
        Some((spec, tail)) => match package_program(spec) {
            Some(program) => {
                let mut argv = vec![program];
                argv.extend(tail.iter().cloned());
                vec![Embedded::Argv(argv)]
            }
            None => unreadable(
                runner,
                &format!(
                    "runs `{}`, whose program kGraf cannot name",
                    spec.escape_debug()
                ),
            ),
        },
        None => Vec::new(),
    }
}

/// Where the operands of a container command start, past its options:
/// `valued` take a value in the next word (or after `=`), `flags` do not,
/// and short flags may be bundled (`-it`). `None` for an option kGraf does
/// not know, whose value it cannot place.
fn past_options(
    args: &[String],
    valued: &[&str],
    flags: &[&str],
    short_flags: &str,
) -> Option<usize> {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "--" {
            return Some(i + 1);
        }
        if !a.starts_with('-') || a == "-" {
            return Some(i);
        }
        let name = a.split_once('=').map_or(a.as_str(), |(n, _)| n);
        if valued.contains(&name) {
            i += if a.contains('=') { 1 } else { 2 };
        } else if flags.contains(&name)
            || (a.len() > 1
                && !a.starts_with("--")
                && a[1..].chars().all(|c| short_flags.contains(c)))
        {
            i += 1;
        } else if !a.starts_with("--") && a.len() > 2 && valued.contains(&&a[..2]) {
            // `-eFOO=1`: a short valued option with its value attached.
            i += 1;
        } else {
            return None;
        }
    }
    Some(i)
}

/// The options of `docker exec` and `podman exec`.
const EXEC_VALUED: &[&str] = &[
    "-e",
    "--env",
    "--env-file",
    "-u",
    "--user",
    "-w",
    "--workdir",
    "--detach-keys",
    "--preserve-fds",
];
const EXEC_FLAGS: &[&str] = &[
    "-d",
    "--detach",
    "-i",
    "--interactive",
    "-t",
    "--tty",
    "--privileged",
    "-l",
    "--latest",
];

/// The options of `docker run` and `podman run` that take a value.
const RUN_VALUED: &[&str] = &[
    "-a",
    "--attach",
    "--add-host",
    "--annotation",
    "--blkio-weight",
    "--blkio-weight-device",
    "--cap-add",
    "--cap-drop",
    "--cgroup-parent",
    "--cgroupns",
    "--cidfile",
    "--cpu-period",
    "--cpu-quota",
    "--cpu-rt-period",
    "--cpu-rt-runtime",
    "-c",
    "--cpu-shares",
    "--cpus",
    "--cpuset-cpus",
    "--cpuset-mems",
    "--device",
    "--device-cgroup-rule",
    "--device-read-bps",
    "--device-read-iops",
    "--device-write-bps",
    "--device-write-iops",
    "--dns",
    "--dns-option",
    "--dns-search",
    "--domainname",
    "--entrypoint",
    "-e",
    "--env",
    "--env-file",
    "--expose",
    "--gpus",
    "--group-add",
    "--health-cmd",
    "--health-interval",
    "--health-retries",
    "--health-start-period",
    "--health-timeout",
    "-h",
    "--hostname",
    "--ip",
    "--ip6",
    "--ipc",
    "--isolation",
    "--kernel-memory",
    "-l",
    "--label",
    "--label-file",
    "--link",
    "--link-local-ip",
    "--log-driver",
    "--log-opt",
    "--mac-address",
    "-m",
    "--memory",
    "--memory-reservation",
    "--memory-swap",
    "--memory-swappiness",
    "--mount",
    "--name",
    "--network",
    "--net",
    "--network-alias",
    "--oom-score-adj",
    "--pid",
    "--pids-limit",
    "--platform",
    "-p",
    "--publish",
    "--pull",
    "--restart",
    "--runtime",
    "--security-opt",
    "--shm-size",
    "--stop-signal",
    "--stop-timeout",
    "--storage-opt",
    "--sysctl",
    "--tmpfs",
    "--ulimit",
    "-u",
    "--user",
    "--userns",
    "--uts",
    "-v",
    "--volume",
    "--volume-driver",
    "--volumes-from",
    "-w",
    "--workdir",
];
const RUN_FLAGS: &[&str] = &[
    "-d",
    "--detach",
    "-i",
    "--interactive",
    "-t",
    "--tty",
    "--rm",
    "--privileged",
    "--init",
    "--read-only",
    "-P",
    "--publish-all",
    "--sig-proxy",
    "--no-healthcheck",
    "--oom-kill-disable",
    "-q",
    "--quiet",
    "--disable-content-trust",
    "--replace",
];

/// The global options of docker and podman before the subcommand that take
/// a value.
const CONTAINER_GLOBAL_VALUED: &[&str] = &[
    "-H",
    "--host",
    "--context",
    "-c",
    "--config",
    "-l",
    "--log-level",
    "--tlscacert",
    "--tlscert",
    "--tlskey",
    "--connection",
    "--url",
    "--identity",
    "--root",
    "--runroot",
    "--storage-driver",
    "--namespace",
    "-n",
];

/// `docker exec C CMD`, `docker run IMAGE CMD`, `docker container exec|run`,
/// `docker compose exec|run`, and the same for podman and nerdctl.
fn container(tool: &str, args: &[String]) -> Vec<Embedded> {
    let Some(start) = past_options(
        args,
        CONTAINER_GLOBAL_VALUED,
        &["-D", "--debug", "--tls", "--tlsverify", "--remote", "-r"],
        "D",
    ) else {
        return unreadable(tool, "has an option kGraf cannot place");
    };
    let rest = &args[start..];
    let (sub, tail) = match rest {
        [c, sub, tail @ ..] if c == "container" => (sub.as_str(), tail),
        [sub, tail @ ..] => (sub.as_str(), tail),
        [] => return Vec::new(),
    };
    match sub {
        "exec" => exec_in_container(tool, tail),
        "run" => run_container(tool, tail, RUN_VALUED, RUN_FLAGS, "dit"),
        "compose" => compose(&format!("{tool} compose"), tail),
        _ => Vec::new(),
    }
}

fn exec_in_container(tool: &str, args: &[String]) -> Vec<Embedded> {
    let Some(i) = past_options(args, EXEC_VALUED, EXEC_FLAGS, "dit") else {
        return unreadable(tool, "has an `exec` option kGraf cannot place");
    };
    // The container, then the command.
    match args.get(i + 1..) {
        Some(cmd) if !cmd.is_empty() => vec![Embedded::Argv(cmd.to_vec())],
        _ => Vec::new(),
    }
}

fn run_container(
    tool: &str,
    args: &[String],
    valued: &[&str],
    flags: &[&str],
    short: &str,
) -> Vec<Embedded> {
    let Some(i) = past_options(args, valued, flags, short) else {
        return unreadable(tool, "has a `run` option kGraf cannot place");
    };
    let entrypoint = super::option_values(args.get(..i).unwrap_or_default(), &["entrypoint"])
        .into_iter()
        .last();
    let mut out: Vec<Embedded> =
        super::option_values(args.get(..i).unwrap_or_default(), &["health-cmd"])
            .into_iter()
            .map(Embedded::Script)
            .collect();
    let cmd = args.get(i + 1..).unwrap_or_default();
    match (entrypoint, cmd.is_empty()) {
        (Some(ep), _) => {
            let mut argv: Vec<String> = ep.split_whitespace().map(str::to_owned).collect();
            argv.extend(cmd.iter().cloned());
            out.push(Embedded::Argv(argv));
        }
        (None, false) => out.push(Embedded::Argv(cmd.to_vec())),
        (None, true) => out.extend(unreadable(
            tool,
            "runs the image's own command, which kGraf cannot read",
        )),
    }
    out
}

/// `docker compose exec SERVICE CMD`, `docker compose run SERVICE [CMD]`,
/// `docker-compose ...`.
fn compose(tool: &str, args: &[String]) -> Vec<Embedded> {
    const GLOBAL_VALUED: &[&str] = &[
        "-f",
        "--file",
        "-p",
        "--project-name",
        "--profile",
        "--env-file",
        "--project-directory",
        "--ansi",
        "--parallel",
        "--progress",
    ];
    let Some(start) = past_options(
        args,
        GLOBAL_VALUED,
        &[
            "--compatibility",
            "--dry-run",
            "--all-resources",
            "--verbose",
        ],
        "",
    ) else {
        return unreadable(tool, "has an option kGraf cannot place");
    };
    let Some((sub, tail)) = args[start..].split_first() else {
        return Vec::new();
    };
    match sub.as_str() {
        "exec" => {
            let Some(i) = past_options(
                tail,
                &["-e", "--env", "-u", "--user", "-w", "--workdir", "--index"],
                &[
                    "-d",
                    "--detach",
                    "-T",
                    "--no-TTY",
                    "--privileged",
                    "-i",
                    "-t",
                ],
                "dTit",
            ) else {
                return unreadable(tool, "has an `exec` option kGraf cannot place");
            };
            match tail.get(i + 1..) {
                Some(cmd) if !cmd.is_empty() => vec![Embedded::Argv(cmd.to_vec())],
                _ => Vec::new(),
            }
        }
        "run" => run_container(
            tool,
            tail,
            &[
                "-e",
                "--env",
                "--entrypoint",
                "--name",
                "-p",
                "--publish",
                "-u",
                "--user",
                "-v",
                "--volume",
                "-w",
                "--workdir",
                "-l",
                "--label",
                "--pull",
                "--cap-add",
                "--cap-drop",
            ],
            &[
                "--rm",
                "-d",
                "--detach",
                "-T",
                "--no-TTY",
                "--no-deps",
                "--service-ports",
                "--use-aliases",
                "-i",
                "--interactive",
                "-t",
                "--tty",
                "--build",
                "--quiet-pull",
                "--remove-orphans",
                "-P",
            ],
            "dTitP",
        ),
        _ => Vec::new(),
    }
}

/// `kubectl exec POD -- CMD`, `kubectl run NAME --image=X -- CMD`,
/// `kubectl debug ... -- CMD`: the command after `--`. Without `--` the
/// command is the image's own or cannot be placed: unreadable.
fn kubectl(tool: &str, args: &[String]) -> Vec<Embedded> {
    // Global options that take a value in the next word.
    const VALUED: &[&str] = &[
        "-n",
        "--namespace",
        "--context",
        "--kubeconfig",
        "--cluster",
        "--user",
        "-s",
        "--server",
        "--token",
        "--as",
        "--as-group",
        "--request-timeout",
        "-v",
        "--cache-dir",
    ];
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if !a.starts_with('-') {
            break;
        }
        i += if VALUED.contains(&a.as_str()) { 2 } else { 1 };
    }
    let sub = args.get(i).map(String::as_str).unwrap_or_default();
    if !matches!(sub, "exec" | "run" | "debug") {
        return Vec::new();
    }
    if args
        .iter()
        .any(|a| a == "--overrides" || a.starts_with("--overrides="))
    {
        return unreadable(
            tool,
            "runs what its `--overrides` say, which kGraf does not read",
        );
    }
    match args.iter().position(|a| a == "--") {
        Some(p) if p + 1 < args.len() => vec![Embedded::Argv(args[p + 1..].to_vec())],
        _ if sub == "exec" => unreadable(
            tool,
            "`exec` without `--` runs a command kGraf cannot place",
        ),
        _ => unreadable(
            tool,
            "runs the image's own command, which kGraf cannot read",
        ),
    }
}

/// `lxc exec INSTANCE [--] CMD`, `incus exec ...`.
fn instance_exec(tool: &str, args: &[String]) -> Vec<Embedded> {
    if args.first().map(String::as_str) != Some("exec") {
        return Vec::new();
    }
    let rest = &args[1..];
    if let Some(p) = rest.iter().position(|a| a == "--") {
        return match rest.get(p + 1..) {
            Some(cmd) if !cmd.is_empty() => vec![Embedded::Argv(cmd.to_vec())],
            _ => unreadable(tool, "`exec` with no command starts a shell"),
        };
    }
    let Some(i) = past_options(
        rest,
        &["--env", "--user", "--group", "--cwd", "--mode", "--project"],
        &[
            "-n",
            "-t",
            "-T",
            "--force-interactive",
            "--force-noninteractive",
        ],
        "ntT",
    ) else {
        return unreadable(tool, "has an `exec` option kGraf cannot place");
    };
    match rest.get(i + 1..) {
        Some(cmd) if !cmd.is_empty() => vec![Embedded::Argv(cmd.to_vec())],
        _ => unreadable(tool, "`exec` with no command starts a shell"),
    }
}

/// `vagrant ssh -c CMD` runs CMD on the guest; `vagrant ssh` alone is an
/// interactive shell there.
fn vagrant(args: &[String]) -> Vec<Embedded> {
    if args.first().map(String::as_str) != Some("ssh") {
        return Vec::new();
    }
    match super::dash_c(&args[1..], &["--command"]) {
        Some(cmd) => vec![Embedded::Script(cmd)],
        None => unreadable(
            "vagrant",
            "`ssh` with no `-c` command starts a shell on the guest",
        ),
    }
}

/// `adb [-s SERIAL] shell CMD...` runs CMD on the device's shell.
fn adb(args: &[String]) -> Vec<Embedded> {
    let Some(i) = past_options(
        args,
        &["-s", "-t", "-H", "-P", "-L"],
        &["-d", "-e", "-a"],
        "dea",
    ) else {
        return unreadable("adb", "has an option kGraf cannot place");
    };
    match args.get(i..) {
        Some([sub, cmd @ ..]) if sub == "shell" => {
            let cmd: Vec<&String> = cmd
                .iter()
                .skip_while(|a| a.starts_with('-') && a.len() == 2)
                .collect();
            if cmd.is_empty() {
                unreadable(
                    "adb",
                    "`shell` with no command starts a shell on the device",
                )
            } else {
                vec![Embedded::Script(
                    cmd.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "),
                )]
            }
        }
        _ => Vec::new(),
    }
}

/// `wsl [OPTIONS] [--] CMD` runs CMD through the Linux user's shell; `wsl -e
/// CMD` runs it as an argv; `wsl` alone starts that shell.
fn wsl(args: &[String]) -> Vec<Embedded> {
    const VALUED: &[&str] = &[
        "-d",
        "--distribution",
        "-u",
        "--user",
        "--cd",
        "--shell-type",
        "--distribution-id",
    ];
    // Management commands run nothing of the caller's.
    const MANAGE: &[&str] = &[
        "--install",
        "--list",
        "-l",
        "--shutdown",
        "--status",
        "--update",
        "--set-default",
        "-s",
        "--set-version",
        "--set-default-version",
        "--terminate",
        "-t",
        "--unregister",
        "--export",
        "--import",
        "--import-in-place",
        "--mount",
        "--unmount",
        "--version",
        "-v",
        "--help",
        "--manage",
    ];
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if MANAGE.contains(&a.as_str()) {
            return Vec::new();
        }
        if a == "-e" || a == "--exec" {
            return match args.get(i + 1..) {
                Some(cmd) if !cmd.is_empty() => vec![Embedded::Argv(cmd.to_vec())],
                _ => unreadable("wsl", "`--exec` names no command"),
            };
        }
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') {
            break;
        }
        i += if VALUED.contains(&a.as_str()) { 2 } else { 1 };
    }
    match args.get(i..) {
        Some(cmd) if !cmd.is_empty() => vec![Embedded::Script(cmd.join(" "))],
        _ => unreadable("wsl", "with no command starts a Linux shell"),
    }
}

/// A program named by a path in the worktree (`./x.py`, `scripts/run`):
/// the kernel runs a `#!` file with its interpreter, so it is read as that
/// interpreter given the file (`./x.py` with `#!/usr/bin/env python3` is
/// `python3 ./x.py`, read as Python). A file with no `#!` line that is text
/// runs in a shell, which kGraf does not read; a compiled program is not
/// read, as `cargo run` is not. A path outside the worktree, or a file that
/// is not there, is a program like any other. `None` hands the path to the
/// generic scan.
fn path_program(program: &str, args: &[String], ctx: &ReadContext) -> Option<Vec<Embedded>> {
    if !super::worktree_file_exists(ctx, program) {
        return None;
    }
    let file = match super::read_worktree_file(ctx, program) {
        Ok(f) => f,
        // Not UTF-8 text: a compiled program, which is not read.
        Err(why) if why.contains("not UTF-8") => return None,
        Err(why) => return Some(vec![Embedded::Unreadable(why)]),
    };
    let first = file.text.lines().next().unwrap_or_default();
    let Some(line) = first.strip_prefix("#!") else {
        if file.text.contains('\0') {
            return None;
        }
        return Some(unreadable(
            program,
            "is a text file with no `#!` line, which a shell runs, and kGraf does not read \
             shell script files",
        ));
    };
    // `#!INTERPRETER [ONE-ARGUMENT]`, as the kernel splits it; `env -S`
    // splits the rest further, which the `env` reader does.
    let line = line.trim();
    let (interpreter, rest) = match line.split_once(char::is_whitespace) {
        Some((i, r)) => (i, Some(r.trim())),
        None => (line, None),
    };
    let mut argv = vec![interpreter.to_owned()];
    if let Some(r) = rest.filter(|r| !r.is_empty()) {
        if base_name(interpreter) == "env" {
            argv.extend(r.split_whitespace().map(str::to_owned));
        } else {
            argv.push(r.to_owned());
        }
    }
    argv.push(program.to_owned());
    argv.extend(args.iter().cloned());
    Some(vec![Embedded::Argv(argv)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    fn read(words: &[&str]) -> Vec<Embedded> {
        let w = argv(words);
        embedded_by_reader(&w, 0, &ReadContext::default()).unwrap_or_default()
    }

    fn inner(words: &[&str]) -> Vec<Vec<String>> {
        read(words)
            .into_iter()
            .filter_map(|e| match e {
                Embedded::Argv(a) => Some(a),
                Embedded::Script(s) => Some(vec![format!("script:{s}")]),
                _ => None,
            })
            .collect()
    }

    fn is_unreadable(words: &[&str]) -> bool {
        read(words)
            .iter()
            .any(|e| matches!(e, Embedded::Unreadable(_)))
    }

    #[test]
    fn package_runners_run_the_package_program() {
        assert_eq!(
            inner(&["npx", "cowsay", "hi"]),
            vec![argv(&["cowsay", "hi"])]
        );
        assert_eq!(
            inner(&["npx", "-y", "cowsay@1.2.0", "hi"]),
            vec![argv(&["cowsay", "hi"])]
        );
        assert_eq!(
            inner(&["npx", "@scope/tool@2", "x"]),
            vec![argv(&["tool", "x"])]
        );
        assert_eq!(
            inner(&["npx", "-p", "pkg", "curl", "x"]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&["npx", "-c", "curl x"]),
            vec![argv(&["script:curl x"])]
        );
        assert_eq!(
            inner(&["npm", "exec", "--", "curl", "x"]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&["pnpm", "dlx", "curl", "x"]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&["pnpm", "exec", "curl", "x"]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&["yarn", "dlx", "curl", "x"]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(inner(&["bunx", "curl", "x"]), vec![argv(&["curl", "x"])]);
        assert_eq!(
            inner(&["bun", "x", "curl", "x"]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&["uvx", "ruff@0.4", "check"]),
            vec![argv(&["ruff", "check"])]
        );
        assert_eq!(
            inner(&["pipx", "run", "black", "."]),
            vec![argv(&["black", "."])]
        );
        assert_eq!(
            inner(&["npm", "init", "vite"]),
            vec![argv(&["create-vite"])]
        );
        assert!(is_unreadable(&["npx", "github:evil/pkg"]));
        assert!(is_unreadable(&["npx", "./local-pkg"]));
    }

    #[test]
    fn container_runners_run_the_inner_command() {
        assert_eq!(
            inner(&["docker", "exec", "-it", "web", "curl", "x"]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&[
                "docker", "exec", "-e", "A=1", "-u", "root", "web", "curl", "x"
            ]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&[
                "docker", "run", "--rm", "-v", "/a:/b", "alpine", "curl", "x"
            ]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&["docker", "run", "--entrypoint", "curl", "alpine", "x"]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&["podman", "container", "run", "-it", "alpine", "curl", "x"]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&["docker", "compose", "exec", "web", "curl", "x"]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&[
                "kubectl", "exec", "-it", "pod", "-c", "app", "--", "curl", "x"
            ]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&["lxc", "exec", "c1", "--", "curl", "x"]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&["vagrant", "ssh", "-c", "curl x"]),
            vec![argv(&["script:curl x"])]
        );
        assert_eq!(
            inner(&["adb", "shell", "curl", "x"]),
            vec![argv(&["script:curl x"])]
        );
        assert_eq!(inner(&["wsl", "curl", "x"]), vec![argv(&["script:curl x"])]);
        assert_eq!(
            inner(&["wsl", "-e", "curl", "x"]),
            vec![argv(&["curl", "x"])]
        );
        // What cannot be named is unreadable.
        assert!(is_unreadable(&["docker", "run", "alpine"]));
        assert!(is_unreadable(&[
            "docker",
            "run",
            "--weird-option",
            "v",
            "alpine",
            "curl"
        ]));
        assert!(is_unreadable(&["kubectl", "exec", "pod", "curl"]));
        assert!(is_unreadable(&["kubectl", "run", "x", "--image=alpine"]));
        assert!(is_unreadable(&["vagrant", "ssh"]));
        assert!(is_unreadable(&["wsl"]));
        // Commands that run nothing of the caller's.
        assert!(read(&["docker", "ps"]).is_empty());
        assert!(read(&["kubectl", "get", "pods"]).is_empty());
        assert!(read(&["wsl", "--list"]).is_empty());
    }

    fn in_other_directory(words: &[&str]) -> Vec<Vec<String>> {
        read(words)
            .into_iter()
            .filter_map(|e| match e {
                Embedded::In(_, inner) => match *inner {
                    Embedded::Argv(a) => Some(a),
                    Embedded::Script(s) => Some(vec![format!("script:{s}")]),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// Each option of a package runner is skipped by its kind: a valued one
    /// takes the next word, a flag does not, `-p` names a package, `-c` and
    /// `--call` give a script, `--` ends the options; a shell the reader
    /// does not know makes the script unreadable.
    #[test]
    fn package_runner_options_are_placed_by_their_kind() {
        for opt in [
            "--registry",
            "--cache",
            "--userconfig",
            "--prefix",
            "-w",
            "--workspace",
            "--node-options",
            "--loglevel",
        ] {
            // A value that looks like a shell program must not matter.
            assert_eq!(
                inner(&["npx", opt, "fish", "cowsay", "hi"]),
                vec![argv(&["cowsay", "hi"])],
                "{opt}"
            );
            assert_eq!(
                inner(&["npx", "-y", opt, "v", "cowsay"]),
                vec![argv(&["cowsay"])],
                "{opt}"
            );
        }
        assert_eq!(
            inner(&["npx", "-y", "--yes", "cowsay", "hi"]),
            vec![argv(&["cowsay", "hi"])]
        );
        assert_eq!(
            inner(&["npx", "--", "cowsay", "hi"]),
            vec![argv(&["cowsay", "hi"])]
        );
        assert_eq!(
            inner(&["npx", "--call", "curl x"]),
            vec![argv(&["script:curl x"])]
        );
        assert_eq!(
            inner(&["npx", "--call=curl x"]),
            vec![argv(&["script:curl x"])]
        );
        assert_eq!(
            inner(&["npx", "-y", "--call", "curl x"]),
            vec![argv(&["script:curl x"])]
        );
        assert!(is_unreadable(&["npx", "-c"]));
        assert!(is_unreadable(&["npx", "-p"]));
        for (words, want) in [
            (vec!["npx", "-p", "cowsay"], "cowsay"),
            (vec!["npx", "--package", "cowsay"], "cowsay"),
            (vec!["npx", "--package=cowsay"], "cowsay"),
            (vec!["npx", "-p=cowsay"], "cowsay"),
            (vec!["npx", "-y", "-p", "cowsay@1"], "cowsay"),
        ] {
            assert_eq!(inner(&words), vec![argv(&[want])], "{words:?}");
        }
        // With `-p`, the first word is a program of the package, as written.
        assert_eq!(
            inner(&["npx", "-p", "pkg", "/usr/bin/curl", "x"]),
            vec![argv(&["/usr/bin/curl", "x"])]
        );
        assert_eq!(
            inner(&["npx", "-p", "pkg", "curl@1", "x"]),
            vec![argv(&["curl@1", "x"])]
        );
        // A package whose program cannot be named, with and without a command.
        assert!(is_unreadable(&["npx", "-p", "./local"]));
        assert!(read(&["npx"]).is_empty());
        assert!(read(&["npx", "-y"]).is_empty());
        // A shell the reader knows runs the script; another one is unreadable.
        for shell in ["--shell", "--script-shell"] {
            for known in ["sh", "bash", "dash", "zsh", "/bin/zsh"] {
                assert_eq!(
                    inner(&["npx", shell, known, "-c", "curl x"]),
                    vec![argv(&["script:curl x"])],
                    "{shell} {known}"
                );
            }
            assert!(
                is_unreadable(&["npx", shell, "fish", "-c", "curl x"]),
                "{shell}"
            );
            assert!(is_unreadable(&["npx", shell, "fish", "cowsay"]), "{shell}");
        }
    }

    /// The global options of a node tool that move its directory wrap what
    /// it runs, in every spelling; the ones that only take a value do not.
    #[test]
    fn a_node_tool_that_changes_directory_wraps_what_it_runs() {
        for words in [
            vec!["npm", "-C", "sub", "exec", "curl", "x"],
            vec!["npm", "--dir", "sub", "exec", "curl", "x"],
            vec!["npm", "--prefix", "sub", "exec", "curl", "x"],
            vec!["npm", "--cwd", "sub", "exec", "curl", "x"],
            vec!["npm", "--dir=sub", "exec", "curl", "x"],
            vec!["npm", "--prefix=sub", "exec", "curl", "x"],
            vec!["npm", "--cwd=sub", "exec", "curl", "x"],
        ] {
            assert_eq!(
                in_other_directory(&words),
                vec![argv(&["curl", "x"])],
                "{words:?}"
            );
            assert!(inner(&words).is_empty(), "{words:?}");
        }
        for words in [
            vec!["pnpm", "-F", "pkg", "exec", "curl", "x"],
            vec!["pnpm", "--filter", "pkg", "exec", "curl", "x"],
            vec!["pnpm", "-w", "ws", "exec", "curl", "x"],
            vec!["pnpm", "--workspace", "ws", "exec", "curl", "x"],
            vec!["pnpm", "--silent", "exec", "curl", "x"],
        ] {
            assert_eq!(inner(&words), vec![argv(&["curl", "x"])], "{words:?}");
            assert!(in_other_directory(&words).is_empty(), "{words:?}");
        }
    }

    /// `pnpm exec` and `yarn exec` run a program, or a shell script with
    /// `-c` or `--shell-mode` before or after the subcommand; `bun exec`
    /// runs a script; `yarn node` runs node.
    #[test]
    fn node_tool_subcommands_run_what_they_name() {
        for tool in ["pnpm", "yarn"] {
            assert_eq!(
                inner(&[tool, "exec", "curl", "x"]),
                vec![argv(&["curl", "x"])]
            );
            for flag in ["-c", "--shell-mode"] {
                assert_eq!(
                    inner(&[tool, flag, "exec", "curl x"]),
                    vec![argv(&["script:curl x"])],
                    "{tool} {flag} before"
                );
                assert_eq!(
                    inner(&[tool, "exec", flag, "curl x"]),
                    vec![argv(&["script:curl x"])],
                    "{tool} {flag} after"
                );
            }
            assert!(read(&[tool, "exec"]).is_empty());
        }
        assert_eq!(
            inner(&["bun", "exec", "curl x"]),
            vec![argv(&["script:curl x"])]
        );
        assert_eq!(
            inner(&["bun", "exec", "--silent", "curl x"]),
            vec![argv(&["script:curl x"])]
        );
        assert!(read(&["bun", "exec"]).is_empty());
        assert_eq!(
            inner(&["yarn", "node", "app.js", "--x"]),
            vec![argv(&["node", "app.js", "--x"])]
        );
    }

    /// `bun -e CODE` is read as inline code, not as a package script.
    #[test]
    fn bun_inline_code_is_read_as_code() {
        assert!(is_unreadable(&["bun", "-e"]));
        assert!(is_unreadable(&["bun", "--eval"]));
    }

    /// The compose forms run the inner command, and name nothing when there
    /// is none.
    #[test]
    fn compose_runs_the_inner_command() {
        for tool in [
            vec!["docker-compose"],
            vec!["podman-compose"],
            vec!["docker", "compose"],
        ] {
            fn extend<'a>(tool: &[&'a str], rest: &[&'a str]) -> Vec<&'a str> {
                let mut words = tool.to_vec();
                words.extend_from_slice(rest);
                words
            }
            let with = |rest: &[&'static str]| extend(&tool, rest);
            assert_eq!(
                inner(&with(&["exec", "web", "curl", "x"])),
                vec![argv(&["curl", "x"])],
                "{tool:?}"
            );
            assert_eq!(
                inner(&with(&["run", "--rm", "web", "curl", "x"])),
                vec![argv(&["curl", "x"])],
                "{tool:?}"
            );
            assert_eq!(
                inner(&with(&["run", "-e", "A=1", "web", "curl"])),
                vec![argv(&["curl"])],
                "{tool:?}"
            );
            assert!(read(&with(&["exec", "web"])).is_empty(), "{tool:?}");
            assert!(read(&with(&["exec", "-T", "web"])).is_empty(), "{tool:?}");
            assert!(read(&with(&["ps"])).is_empty(), "{tool:?}");
            assert!(is_unreadable(&with(&["run", "web"])), "{tool:?}");
        }
        assert!(read(&["docker", "exec", "web"]).is_empty());
        assert!(read(&["docker", "exec", "-it", "web"]).is_empty());
        assert!(read(&["podman", "container", "exec", "web"]).is_empty());
    }

    /// Where the operands start: past `--`, at a lone `-`, past valued
    /// options (spaced, `=` or attached), flags and bundled short flags;
    /// `None` for an option it does not know.
    #[test]
    fn options_are_placed_by_their_kind() {
        let words = |w: &[&str]| argv(w);
        let place = |w: &[&str]| past_options(&words(w), &["-e", "--env"], &["--rm"], "dit");
        assert_eq!(place(&[]), Some(0));
        assert_eq!(place(&["web"]), Some(0));
        assert_eq!(place(&["-"]), Some(0));
        assert_eq!(place(&["--", "web"]), Some(1));
        assert_eq!(place(&["--rm", "--", "web"]), Some(2));
        assert_eq!(place(&["-e", "A=1", "web"]), Some(2));
        assert_eq!(place(&["--env", "A=1", "web"]), Some(2));
        assert_eq!(place(&["--env=A=1", "web"]), Some(1));
        assert_eq!(place(&["-eA=1", "web"]), Some(1));
        assert_eq!(place(&["-it", "web"]), Some(1));
        assert_eq!(place(&["-i", "-t", "--rm", "web"]), Some(3));
        assert_eq!(place(&["-itz", "web"]), None);
        assert_eq!(place(&["-z", "web"]), None);
        assert_eq!(place(&["-zA=1", "web"]), None);
        assert_eq!(place(&["--weird", "web"]), None);
        assert_eq!(place(&["--weird=1", "web"]), None);
        // With no short flags known, a bundle is no flag.
        assert_eq!(past_options(&words(&["-d", "web"]), &[], &[], ""), None);
        // The same through the container commands.
        assert_eq!(
            inner(&["docker", "exec", "--", "web", "curl", "x"]),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            inner(&["docker", "exec", "-eA=1", "web", "curl", "x"]),
            vec![argv(&["curl", "x"])]
        );
        assert!(is_unreadable(&["docker", "exec", "-zA", "web", "curl"]));
        assert!(is_unreadable(&["docker", "exec", "-z", "web", "curl"]));
    }
}

#[cfg(test)]
#[path = "permission_readers_more_tests.rs"]
mod more_tests;
