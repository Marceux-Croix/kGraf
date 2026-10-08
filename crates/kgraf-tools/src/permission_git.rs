//! What git runs besides itself (decision 0094, amended 2026-10-06): a
//! configuration value that names a command, given with `-c KEY=VALUE`,
//! `--config-env`, an environment assignment before `git`, or written with
//! `git config KEY VALUE`; an alias defined with `!`; and the commands some
//! subcommands take as arguments (`rebase --exec`, `bisect run`, `submodule
//! foreach`, `filter-branch --*-filter`, `difftool --extcmd`, `send-email
//! --sendmail-cmd`).
//!
//! A literal value is read as the command git runs (through a shell, as git
//! runs most of them). A value kGraf cannot see (`--config-env`, an included
//! configuration file, a hooks directory) is unreadable.
//!
//! An alias (`git st`) is looked up in `-c alias.st=...`, then in the
//! repository's configuration (the worktree's `.git` file or directory, its
//! common directory's `config` and the worktree's `config.worktree`) and the
//! user's (`~/.gitconfig`, `~/.config/git/config`). A name that is no git
//! command and no alias there runs the program `git-NAME`. When a read
//! configuration file includes another (`[include]`, `[includeIf]`) and the
//! alias is not found, it is unreadable. The system configuration is not
//! read: it is not the project's to write.
//!
//! Which configuration keys run commands is a list (decision 0094), taken
//! from git-config(1).

use super::{Embedded, ReadContext, program_key};
use std::path::{Path, PathBuf};

fn unreadable(why: &str) -> Vec<Embedded> {
    vec![Embedded::Unreadable(format!("`git` {why}"))]
}

/// git's own commands (`git help -a`, git 2.54), so a name outside this
/// list is an alias or an external `git-NAME` program.
const GIT_COMMANDS: &[&str] = &[
    "add",
    "am",
    "annotate",
    "apply",
    "archimport",
    "archive",
    "backfill",
    "bisect",
    "blame",
    "branch",
    "bugreport",
    "bundle",
    "cat-file",
    "check-attr",
    "check-ignore",
    "check-mailmap",
    "check-ref-format",
    "checkout",
    "checkout-index",
    "cherry",
    "cherry-pick",
    "citool",
    "clean",
    "clone",
    "column",
    "commit",
    "commit-graph",
    "commit-tree",
    "config",
    "count-objects",
    "credential",
    "credential-cache",
    "credential-store",
    "cvsexportcommit",
    "cvsimport",
    "cvsserver",
    "daemon",
    "describe",
    "diagnose",
    "diff",
    "diff-files",
    "diff-index",
    "diff-pairs",
    "diff-tree",
    "difftool",
    "fast-export",
    "fast-import",
    "fetch",
    "fetch-pack",
    "filter-branch",
    "fmt-merge-msg",
    "for-each-ref",
    "for-each-repo",
    "format-patch",
    "fsck",
    "gc",
    "get-tar-commit-id",
    "gitk",
    "gitweb",
    "grep",
    "gui",
    "hash-object",
    "help",
    "history",
    "hook",
    "http-backend",
    "imap-send",
    "index-pack",
    "init",
    "instaweb",
    "interpret-trailers",
    "last-modified",
    "log",
    "ls-files",
    "ls-remote",
    "ls-tree",
    "mailinfo",
    "mailsplit",
    "maintenance",
    "merge",
    "merge-base",
    "merge-file",
    "merge-index",
    "merge-one-file",
    "merge-tree",
    "mergetool",
    "mktag",
    "mktree",
    "multi-pack-index",
    "mv",
    "name-rev",
    "notes",
    "p4",
    "pack-objects",
    "pack-redundant",
    "pack-refs",
    "patch-id",
    "prune",
    "prune-packed",
    "pull",
    "push",
    "quiltimport",
    "range-diff",
    "read-tree",
    "rebase",
    "reflog",
    "refs",
    "remote",
    "repack",
    "replace",
    "replay",
    "repo",
    "request-pull",
    "rerere",
    "reset",
    "restore",
    "rev-list",
    "rev-parse",
    "revert",
    "rm",
    "scalar",
    "send-email",
    "send-pack",
    "sh-i18n",
    "sh-setup",
    "shortlog",
    "show",
    "show-branch",
    "show-index",
    "show-ref",
    "sparse-checkout",
    "stage",
    "stash",
    "status",
    "stripspace",
    "submodule",
    "svn",
    "switch",
    "symbolic-ref",
    "tag",
    "unpack-file",
    "unpack-objects",
    "update-index",
    "update-ref",
    "update-server-info",
    "upload-archive",
    "upload-pack",
    "var",
    "verify-commit",
    "verify-pack",
    "verify-tag",
    "version",
    "whatchanged",
    "worktree",
    "write-tree",
];

/// What a configuration key's value is to git.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyRuns {
    /// A command git runs through a shell.
    Script,
    /// A program path (with arguments) git runs.
    Program,
    /// `credential.helper`: `!CMD`, a path, or `NAME` for
    /// `git credential-NAME`.
    Helper,
    /// A setting kGraf cannot follow (a hooks directory, an included file).
    Opaque,
}

/// Whether `key` (as written, any case) names a command git runs, from
/// git-config(1). `alias.*` is handled by the alias reader.
fn key_runs(key: &str) -> Option<KeyRuns> {
    let lower = key.to_ascii_lowercase();
    let mut parts = lower.split('.');
    let section = parts.next().unwrap_or_default();
    let rest: Vec<&str> = parts.collect();
    let name = rest.last().copied().unwrap_or_default();
    let sub = rest.len() > 1;
    use KeyRuns::*;
    Some(match (section, name, sub) {
        ("core", "sshcommand" | "pager" | "editor" | "askpass" | "gitproxy", false) => Script,
        ("core", "fsmonitor", false) => Script,
        ("core", "alternaterefscommand", false) => Script,
        ("core", "hookspath", false) => Opaque,
        ("sequence", "editor", false) => Script,
        ("credential", "helper", _) => Helper,
        ("diff", "external", false) => Script,
        ("diff", "command" | "textconv", true) => Script,
        ("merge", "driver", true) => Script,
        ("filter", "clean" | "smudge" | "process", true) => Script,
        ("gpg", "program", _) => Program,
        ("gpg", "defaultkeycommand", true) => Script,
        ("pager", _, _) if !rest.is_empty() => Script,
        ("interactive", "difffilter", false) => Script,
        ("uploadpack", "packobjectshook", false) => Script,
        ("remote", "uploadpack" | "receivepack", true) => Program,
        ("remote", "vcs", true) => Program,
        ("web", "browser", false) => Program,
        ("browser" | "difftool" | "mergetool" | "man", "cmd", true) => Script,
        ("browser" | "difftool" | "mergetool" | "man", "path", true) => Program,
        ("sendemail", "sendmailcmd" | "tocmd" | "cccmd" | "smtpserver", _) => Script,
        ("submodule", "update", true) => Script,
        ("hook", "command", true) => Script,
        ("instaweb", "httpd", false) => Program,
        ("include", "path", false) | ("includeif", "path", true) => Opaque,
        _ => return None,
    })
}

/// What a value of a key that runs commands makes git run.
fn value_runs(key: &str, kind: KeyRuns, value: &str) -> Vec<Embedded> {
    let value = value.trim();
    let lower_key = key.to_ascii_lowercase();
    // Settings that turn a feature on or off run nothing of their own
    // (`core.fsmonitor=true` uses git's built-in daemon, `pager.log=false`).
    if matches!(
        value.to_ascii_lowercase().as_str(),
        "" | "true" | "false" | "yes" | "no" | "on" | "off" | "0" | "1"
    ) && (lower_key == "core.fsmonitor" || lower_key.starts_with("pager.") || value.is_empty())
    {
        return Vec::new();
    }
    match kind {
        KeyRuns::Script => {
            // `submodule.NAME.update=!CMD` runs CMD; its other values are
            // modes (`rebase`, `merge`).
            if lower_key.starts_with("submodule.") {
                return match value.strip_prefix('!') {
                    Some(cmd) => vec![Embedded::Script(cmd.to_owned())],
                    None => Vec::new(),
                };
            }
            vec![Embedded::Script(value.to_owned())]
        }
        KeyRuns::Program => vec![Embedded::Argv(
            value.split_whitespace().map(str::to_owned).collect(),
        )],
        KeyRuns::Helper => match value.strip_prefix('!') {
            Some(cmd) => vec![Embedded::Script(cmd.to_owned())],
            // Git runs an absolute path as written: on Windows also
            // `C:/tools/helper.exe`, the form Git for Windows writes.
            None if value.starts_with('/')
                || value.contains('\\')
                || super::paths::shape(super::paths::Flavor::HOST, value)
                    == super::paths::Shape::Absolute =>
            {
                vec![Embedded::Argv(
                    value.split_whitespace().map(str::to_owned).collect(),
                )]
            }
            None => {
                let mut words = value.split_whitespace();
                let first = words.next().unwrap_or_default();
                let mut argv = vec![format!("git-credential-{first}")];
                argv.extend(words.map(str::to_owned));
                vec![Embedded::Argv(argv)]
            }
        },
        KeyRuns::Opaque => unreadable(&format!(
            "is given `{key}`, which makes it run files kGraf does not read"
        )),
    }
}

/// Environment variables git reads for a command it runs, from git(1).
fn env_runs(name: &str) -> Option<KeyRuns> {
    Some(match name {
        "GIT_SSH_COMMAND"
        | "GIT_EXTERNAL_DIFF"
        | "GIT_PAGER"
        | "PAGER"
        | "GIT_EDITOR"
        | "EDITOR"
        | "VISUAL"
        | "GIT_SEQUENCE_EDITOR"
        | "GIT_PROXY_COMMAND" => KeyRuns::Script,
        "GIT_SSH" | "GIT_ASKPASS" | "SSH_ASKPASS" => KeyRuns::Program,
        "GIT_CONFIG_PARAMETERS"
        | "GIT_CONFIG_COUNT"
        | "GIT_CONFIG"
        | "GIT_CONFIG_GLOBAL"
        | "GIT_CONFIG_SYSTEM"
        | "GIT_EXEC_PATH"
        | "GIT_TEMPLATE_DIR" => KeyRuns::Opaque,
        _ if name.starts_with("GIT_CONFIG_KEY_") || name.starts_with("GIT_CONFIG_VALUE_") => {
            KeyRuns::Opaque
        }
        _ => return None,
    })
}

/// The global options of git that take a value in the next word.
const GLOBAL_VALUED: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--config-env",
    "--super-prefix",
    "--list-cmds",
    "--attr-source",
];

/// The commands `git` (at `words[at]`) runs besides itself.
/// `git [GLOBAL OPTIONS] SUBCOMMAND ARGS` as `git SUBCOMMAND ARGS`, so a
/// rule on the subcommand (`shell(git push*)`) matches however the global
/// options are written (`git -c a=b push`, `git -C . push`, `git
/// --no-pager push`). `None` when `words[at]` is not `git` or names no
/// subcommand. The command still runs as written; this is only the subject
/// a rule reads (decision 0094, amended 2026-10-06).
pub(super) fn normalized(words: &[String], at: usize) -> Option<Vec<String>> {
    if program_key(&words[at]) != "git" {
        return None;
    }
    let args = &words[at + 1..];
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if !a.starts_with('-') {
            break;
        }
        let takes = GLOBAL_VALUED.contains(&a.as_str());
        i += if takes { 2 } else { 1 };
    }
    let sub = args.get(i)?;
    let mut out = vec!["git".to_owned(), sub.clone()];
    out.extend(args[i + 1..].iter().cloned());
    Some(out)
}

pub(super) fn git(words: &[String], at: usize, ctx: &ReadContext) -> Vec<Embedded> {
    let args = &words[at + 1..];
    let mut out: Vec<Embedded> = Vec::new();
    // Assignments before `git` (`GIT_SSH_COMMAND=... git fetch`, `env
    // GIT_PAGER=... git log`).
    let mut config_moved = false;
    // The variables the call set: before `git` and earlier in its script.
    let env = ctx.with_assignments(&words[..at]).env;
    for (name, value) in &env {
        let (name, value) = (name.as_str(), value.as_str());
        if matches!(
            name,
            "GIT_DIR" | "GIT_COMMON_DIR" | "HOME" | "XDG_CONFIG_HOME"
        ) {
            config_moved = true;
        }
        if let Some(kind) = env_runs(name) {
            if kind == KeyRuns::Opaque {
                return unreadable(&format!(
                    "is given `{name}`, which makes it run what kGraf cannot read"
                ));
            }
            out.extend(value_runs(name, kind, value));
        }
    }
    // Global options: `-c KEY=VALUE`, `--config-env=KEY=VAR`, `-C DIR`.
    let mut aliases: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    let mut dir_moved = false;
    while let Some(a) = args.get(i) {
        if !a.starts_with('-') {
            break;
        }
        let (name, attached) = match a.split_once('=') {
            Some((n, v)) if a.starts_with("--") => (n, Some(v.to_owned())),
            _ => (a.as_str(), None),
        };
        let takes = GLOBAL_VALUED.contains(&name) && attached.is_none();
        let value = attached.or_else(|| takes.then(|| args.get(i + 1).cloned()).flatten());
        match name {
            "-c" => {
                let Some(pair) = value.as_deref() else {
                    return unreadable("is given `-c` with no setting");
                };
                let (key, v) = pair.split_once('=').unwrap_or((pair, "true"));
                if let Some(alias) = key.to_ascii_lowercase().strip_prefix("alias.") {
                    aliases.push((alias.to_owned(), v.to_owned()));
                } else if let Some(kind) = key_runs(key) {
                    out.extend(value_runs(key, kind, v));
                }
            }
            "--config-env" => {
                let pair = value.unwrap_or_default();
                let key = pair.split_once('=').map_or(pair.as_str(), |(k, _)| k);
                if key_runs(key).is_some() || key.to_ascii_lowercase().starts_with("alias.") {
                    return unreadable(&format!(
                        "takes `{key}` from an environment variable, which kGraf cannot read"
                    ));
                }
            }
            "--exec-path" if value.is_some() => {
                return unreadable("is given another directory of git programs");
            }
            "-C" | "--git-dir" | "--work-tree" => dir_moved = true,
            _ => {}
        }
        i += if takes { 2 } else { 1 };
    }
    let Some(sub) = args.get(i) else {
        return out;
    };
    let rest = &args[i + 1..];
    let lower = sub.as_str();
    if !GIT_COMMANDS.contains(&lower) {
        out.extend(alias(sub, rest, &aliases, ctx, config_moved || dir_moved));
        return out;
    }
    out.extend(subcommand(lower, rest));
    out
}

/// What a git subcommand runs from its arguments.
fn subcommand(sub: &str, rest: &[String]) -> Vec<Embedded> {
    let value_of = |names: &[&str]| -> Vec<String> {
        let mut found = Vec::new();
        let mut it = rest.iter();
        while let Some(a) = it.next() {
            for n in names {
                if a == n {
                    if let Some(v) = it.next() {
                        found.push(v.clone());
                    }
                    break;
                }
                if let Some(v) = a.strip_prefix(&format!("{n}=")) {
                    found.push(v.to_owned());
                    break;
                }
                // `-xCMD`: a short option with its value attached.
                if n.len() == 2
                    && !n.starts_with("--")
                    && a.len() > 2
                    && a.starts_with(n)
                    && !a.starts_with("--")
                {
                    found.push(a[2..].to_owned());
                    break;
                }
            }
        }
        found
    };
    match sub {
        "rebase" => value_of(&["-x", "--exec"])
            .into_iter()
            .map(Embedded::Script)
            .collect(),
        "bisect" => match rest.split_first() {
            Some((run, cmd)) if run == "run" && !cmd.is_empty() => {
                vec![Embedded::Argv(cmd.to_vec())]
            }
            _ => Vec::new(),
        },
        "submodule" => {
            let Some(p) = rest.iter().position(|a| a == "foreach") else {
                return Vec::new();
            };
            let cmd: Vec<&String> = rest[p + 1..]
                .iter()
                .skip_while(|a| a.starts_with("--") || a == &"-q")
                .collect();
            if cmd.is_empty() {
                Vec::new()
            } else {
                vec![Embedded::Script(
                    cmd.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "),
                )]
            }
        }
        "filter-branch" => value_of(&[
            "--env-filter",
            "--tree-filter",
            "--index-filter",
            "--parent-filter",
            "--msg-filter",
            "--commit-filter",
            "--tag-name-filter",
        ])
        .into_iter()
        .map(Embedded::Script)
        .collect(),
        "difftool" => value_of(&["-x", "--extcmd"])
            .into_iter()
            .map(Embedded::Script)
            .collect(),
        "send-email" => value_of(&["--sendmail-cmd", "--smtp-server", "--to-cmd", "--cc-cmd"])
            .into_iter()
            .map(Embedded::Script)
            .collect(),
        // `git config KEY VALUE` (or `git config set KEY VALUE`) writes a
        // value git will run later: read it now, as the command it is.
        "config" => {
            let plain: Vec<&String> = rest.iter().filter(|a| !a.starts_with('-')).collect();
            let plain: Vec<&String> = match plain.first().map(|s| s.as_str()) {
                Some("set") => plain[1..].to_vec(),
                _ => plain,
            };
            // `--file F`, `-f F`, `--blob B` take a value, which is not the
            // key.
            let plain: Vec<&String> = plain
                .into_iter()
                .filter(|p| {
                    !rest.windows(2).any(|w| {
                        matches!(w[0].as_str(), "-f" | "--file" | "--blob" | "--type")
                            && &&w[1] == p
                    })
                })
                .collect();
            match plain.as_slice() {
                [key, value, ..] => {
                    let lower = key.to_ascii_lowercase();
                    if let Some(alias) = lower.strip_prefix("alias.") {
                        match value.strip_prefix('!') {
                            Some(cmd) => vec![Embedded::Script(cmd.to_owned())],
                            None => {
                                let _ = alias;
                                Vec::new()
                            }
                        }
                    } else if let Some(kind) = key_runs(key) {
                        value_runs(key, kind, value)
                    } else {
                        Vec::new()
                    }
                }
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// `git NAME ARGS` where NAME is no git command: an alias, or the program
/// `git-NAME`.
fn alias(
    name: &str,
    rest: &[String],
    given: &[(String, String)],
    ctx: &ReadContext,
    config_moved: bool,
) -> Vec<Embedded> {
    let lower = name.to_ascii_lowercase();
    let defined = given
        .iter()
        .rev()
        .find(|(n, _)| *n == lower)
        .map(|(_, v)| Ok(v.clone()))
        .or_else(|| {
            if config_moved {
                return Some(Err(
                    "reads its configuration from elsewhere, so kGraf cannot tell whether \
                     this name is an alias"
                        .to_owned(),
                ));
            }
            lookup_alias(&lower, ctx)
        });
    match defined {
        Some(Ok(value)) => match value.trim().strip_prefix('!') {
            // git runs `!CMD` with the arguments after it.
            Some(cmd) => {
                let mut script = cmd.to_owned();
                for a in rest {
                    script.push(' ');
                    script.push_str(
                        &shlex::try_quote(a).map_or_else(|_| a.clone(), |q| q.into_owned()),
                    );
                }
                vec![Embedded::Script(script)]
            }
            None => match shlex::split(&value) {
                Some(words) if !words.is_empty() => {
                    let mut argv = vec!["git".to_owned()];
                    argv.extend(words);
                    argv.extend(rest.iter().cloned());
                    vec![Embedded::Argv(argv)]
                }
                _ => unreadable(&format!("has an alias `{name}` kGraf cannot split")),
            },
        },
        Some(Err(why)) => unreadable(&why),
        None => {
            let mut argv = vec![format!("git-{name}")];
            argv.extend(rest.iter().cloned());
            vec![Embedded::Argv(argv)]
        }
    }
}

/// The largest git configuration file read.
const MAX_CONFIG_BYTES: u64 = 256 * 1024;

/// Read a configuration file outside the worktree's confinement (git's
/// files live in the common directory and the home directory): a regular
/// file only, bounded, never a FIFO. `None` when it is absent.
fn read_config(path: &Path) -> Option<Result<String, String>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return Some(Err(format!("`{}` is not a regular file", path.display())));
    }
    if meta.len() > MAX_CONFIG_BYTES {
        return Some(Err(format!("`{}` is too large to read", path.display())));
    }
    Some(
        std::fs::read_to_string(path)
            .map_err(|e| format!("`{}` cannot be read: {e}", path.display())),
    )
}

/// The configuration files that may define an alias for a command run in
/// `ctx`'s directory, in the order git reads them (later wins).
fn config_files(ctx: &ReadContext) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    if let Some(home) = &ctx.home {
        files.push(home.join(".config/git/config"));
        files.push(home.join(".gitconfig"));
    }
    let Some(root) = &ctx.root else {
        return Err("kGraf cannot tell which repository the command runs in".into());
    };
    let dot_git = root.join(".git");
    let git_dir = match std::fs::metadata(&dot_git) {
        Ok(m) if m.is_dir() => dot_git,
        Ok(m) if m.is_file() => {
            let text = match read_config(&dot_git) {
                Some(Ok(t)) => t,
                Some(Err(e)) => return Err(e),
                None => return Err("the worktree's `.git` cannot be read".into()),
            };
            let target = text
                .lines()
                .find_map(|l| l.strip_prefix("gitdir:"))
                .map(str::trim)
                .ok_or("the worktree's `.git` names no repository")?;
            let p = PathBuf::from(target);
            if p.is_absolute() { p } else { root.join(p) }
        }
        _ => return Err("the worktree has no `.git`".into()),
    };
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(c) => {
            let p = PathBuf::from(c.trim());
            if p.is_absolute() { p } else { git_dir.join(p) }
        }
        Err(_) => git_dir.clone(),
    };
    files.push(common.join("config"));
    files.push(git_dir.join("config.worktree"));
    Ok(files)
}

/// The value of `alias.NAME` in the configuration files, the last one
/// wins. `None` when no read file defines it and none includes another;
/// `Err` when it cannot be told.
fn lookup_alias(name: &str, ctx: &ReadContext) -> Option<Result<String, String>> {
    let files = match config_files(ctx) {
        Ok(f) => f,
        Err(why) => {
            return Some(Err(format!(
                "cannot tell whether `{name}` is an alias: {why}"
            )));
        }
    };
    let mut found: Option<String> = None;
    let mut includes = false;
    for f in files {
        let text = match read_config(&f) {
            None => continue,
            Some(Ok(t)) => t,
            Some(Err(why)) => return Some(Err(why)),
        };
        ctx.witness(&f, &text);
        let (value, inc) = alias_in(&text, name);
        includes |= inc;
        if value.is_some() {
            found = value;
        }
    }
    match found {
        Some(v) => Some(Ok(v)),
        None if includes => Some(Err(format!(
            "reads a configuration file that includes another, so kGraf cannot tell whether \
             `{name}` is an alias"
        ))),
        None => None,
    }
}

/// The last `alias.NAME` value in one configuration file's text, and
/// whether the file includes another. A small reader of git's format:
/// `[section]` and `[section "sub"]` headers, `key = value` lines, quotes,
/// `#` and `;` comments, and a backslash at the end of a line.
fn alias_in(text: &str, name: &str) -> (Option<String>, bool) {
    let mut section = String::new();
    let mut found = None;
    let mut includes = false;
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let mut line = line.trim().to_owned();
        while line.ends_with('\\') {
            line.pop();
            match lines.next() {
                Some(next) => line.push_str(next.trim()),
                None => break,
            }
        }
        if line.starts_with('#') || line.starts_with(';') || line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let header = header.split(']').next().unwrap_or_default();
            section = header
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if section == "include" || section == "includeif" {
                includes = true;
            }
            // `[alias] st = status` may follow the header on its line.
            if let Some(rest) = line.split_once(']').map(|(_, r)| r.trim())
                && !rest.is_empty()
            {
                line = rest.to_owned();
            } else {
                continue;
            }
        }
        if section != "alias" {
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (line.trim(), "true"),
        };
        if key.eq_ignore_ascii_case(name) {
            found = Some(unquote_config_value(value));
        }
    }
    (found, includes)
}

/// A configuration value without its quotes and trailing comment, with
/// `\"`, `\\`, `\n` and `\t` resolved.
fn unquote_config_value(value: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(o) => out.push(o),
                None => {}
            },
            '#' | ';' if !quoted => break,
            _ => out.push(c),
        }
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every configuration key that names a command git runs is read as the
    /// kind of command it is, in any case, and a key near it is not.
    #[test]
    fn each_command_running_key_is_known_by_its_kind() {
        use KeyRuns::*;
        for (key, kind) in [
            ("core.sshCommand", Script),
            ("core.fsmonitor", Script),
            ("core.alternateRefsCommand", Script),
            ("core.hooksPath", Opaque),
            ("sequence.editor", Script),
            ("credential.helper", Helper),
            ("credential.https://x.example.helper", Helper),
            ("diff.external", Script),
            ("diff.x.command", Script),
            ("diff.x.textconv", Script),
            ("merge.x.driver", Script),
            ("filter.x.clean", Script),
            ("filter.x.smudge", Script),
            ("filter.x.process", Script),
            ("gpg.program", Program),
            ("gpg.ssh.program", Program),
            ("gpg.x.defaultKeyCommand", Script),
            ("pager.log", Script),
            ("interactive.diffFilter", Script),
            ("uploadpack.packObjectsHook", Script),
            ("remote.x.uploadpack", Program),
            ("remote.x.receivepack", Program),
            ("remote.x.vcs", Program),
            ("web.browser", Program),
            ("browser.x.cmd", Script),
            ("difftool.x.cmd", Script),
            ("mergetool.x.cmd", Script),
            ("man.x.cmd", Script),
            ("browser.x.path", Program),
            ("difftool.x.path", Program),
            ("mergetool.x.path", Program),
            ("man.x.path", Program),
            ("sendemail.sendmailCmd", Script),
            ("sendemail.toCmd", Script),
            ("sendemail.ccCmd", Script),
            ("sendemail.smtpServer", Script),
            ("submodule.x.update", Script),
            ("hook.x.command", Script),
            ("instaweb.httpd", Program),
            ("include.path", Opaque),
            ("includeIf.gitdir:x.path", Opaque),
        ] {
            assert_eq!(key_runs(key), Some(kind), "{key}");
        }
        for key in [
            "user.name",
            "core.bare",
            "diff.x.algorithm",
            "merge.x.name",
            "pager",
            "submodule.update",
        ] {
            assert_eq!(key_runs(key), None, "{key}");
        }
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    fn read(words: &[&str], ctx: &ReadContext) -> Vec<Embedded> {
        let w = argv(words);
        let at = super::super::command_starts(&w)
            .into_iter()
            .find(|&i| w[i] == "git")
            .unwrap_or(0);
        git(&w, at, ctx)
    }

    fn scripts(found: &[Embedded]) -> Vec<String> {
        found
            .iter()
            .filter_map(|e| match e {
                Embedded::Script(s) => Some(s.clone()),
                Embedded::Argv(a) => Some(a.join(" ")),
                _ => None,
            })
            .collect()
    }

    fn opaque(found: &[Embedded]) -> bool {
        found.iter().any(|e| matches!(e, Embedded::Unreadable(_)))
    }

    #[test]
    fn configuration_values_that_run_commands_are_read() {
        let none = ReadContext::default();
        for (words, want) in [
            (
                vec!["git", "-c", "core.sshCommand=curl x", "fetch"],
                "curl x",
            ),
            (vec!["git", "-c", "core.pager=curl x", "log"], "curl x"),
            (vec!["git", "-c", "core.editor=curl x", "commit"], "curl x"),
            (
                vec!["git", "-c", "core.fsmonitor=curl x", "status"],
                "curl x",
            ),
            (
                vec!["git", "-c", "credential.helper=!curl x", "fetch"],
                "curl x",
            ),
            (
                vec!["git", "-c", "credential.helper=store", "fetch"],
                "git-credential-store",
            ),
            (vec!["git", "-c", "diff.external=curl x", "diff"], "curl x"),
            (vec!["git", "-c", "gpg.program=curl", "commit"], "curl"),
            (vec!["git", "-c", "alias.x=!curl x", "x"], "curl x"),
            (
                vec!["git", "-c", "filter.a.smudge=curl x", "checkout"],
                "curl x",
            ),
            (vec!["GIT_SSH_COMMAND=curl x", "git", "fetch"], "curl x"),
            (vec!["GIT_EXTERNAL_DIFF=curl x", "git", "diff"], "curl x"),
            (vec!["GIT_PAGER=curl x", "git", "log"], "curl x"),
            (vec!["GIT_EDITOR=curl x", "git", "commit"], "curl x"),
            (vec!["env", "GIT_PAGER=curl x", "git", "log"], "curl x"),
            (vec!["git", "rebase", "--exec", "curl x", "main"], "curl x"),
            (vec!["git", "rebase", "-xcurl x", "main"], "curl x"),
            (vec!["git", "bisect", "run", "curl", "x"], "curl x"),
            (vec!["git", "submodule", "foreach", "curl x"], "curl x"),
            (
                vec!["git", "filter-branch", "--tree-filter", "curl x"],
                "curl x",
            ),
            (vec!["git", "config", "alias.x", "!curl x"], "curl x"),
            (
                vec!["git", "config", "--local", "core.pager", "curl x"],
                "curl x",
            ),
        ] {
            let found = read(&words, &none);
            assert!(
                scripts(&found).iter().any(|s| s == want),
                "{words:?}: {found:?}"
            );
        }
        for words in [
            vec!["git", "-c", "core.hooksPath=hooks", "commit"],
            vec!["git", "--config-env=core.pager=X", "log"],
            vec!["git", "-c", "include.path=x", "log"],
            vec!["GIT_CONFIG_PARAMETERS='core.pager=curl'", "git", "log"],
            vec!["git", "--exec-path=/tmp/x", "status"],
        ] {
            assert!(opaque(&read(&words, &none)), "{words:?}");
        }
        // Ordinary settings run nothing.
        for words in [
            vec!["git", "-c", "user.name=x", "commit"],
            vec!["git", "-c", "core.fsmonitor=false", "status"],
            vec!["git", "-c", "pager.log=false", "log"],
            vec!["git", "status"],
            vec!["git", "log", "--oneline"],
        ] {
            assert!(read(&words, &none).is_empty(), "{words:?}");
        }
    }

    #[test]
    fn an_alias_is_read_from_the_repository_and_user_configuration() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("wt");
        let home = dir.path().join("home");
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            root.join(".git/config"),
            "[core]\n\tbare = false\n[alias]\n\tbad = \"!curl x\"\n\tst = status -s\n",
        )
        .expect("config");
        std::fs::write(home.join(".gitconfig"), "[alias]\n\tlg = log --oneline\n").expect("home");
        let ctx = ReadContext::in_worktree(&root, Some(&home));
        assert_eq!(scripts(&read(&["git", "bad"], &ctx)), vec!["curl x"]);
        assert_eq!(scripts(&read(&["git", "st"], &ctx)), vec!["git status -s"]);
        assert_eq!(
            scripts(&read(&["git", "lg"], &ctx)),
            vec!["git log --oneline"]
        );
        // No alias: the external program `git-NAME`.
        assert_eq!(
            scripts(&read(&["git", "lfs", "pull"], &ctx)),
            vec!["git-lfs pull"]
        );
        // An include may define it: unreadable.
        std::fs::write(home.join(".gitconfig"), "[include]\n\tpath = other\n").expect("home");
        assert!(opaque(&read(&["git", "zz"], &ctx)));
        // No directory known: unreadable.
        assert!(opaque(&read(&["git", "zz"], &ReadContext::default())));
    }

    #[test]
    fn a_worktree_git_file_leads_to_the_common_configuration() {
        let dir = tempfile::tempdir().expect("dir");
        let main = dir.path().join("main");
        let gitdir = main.join(".git/worktrees/w1");
        std::fs::create_dir_all(&gitdir).expect("gitdir");
        std::fs::write(gitdir.join("commondir"), "../..\n").expect("commondir");
        std::fs::write(main.join(".git/config"), "[alias]\n\tbad = !curl x\n").expect("config");
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).expect("wt");
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).expect(".git");
        let ctx = ReadContext::in_worktree(&wt, None);
        assert_eq!(scripts(&read(&["git", "bad"], &ctx)), vec!["curl x"]);
    }

    /// A worktree whose configuration defines no alias, so a name that is no
    /// git command resolves to the program `git-NAME` unless something moved
    /// the configuration.
    fn plain_worktree() -> (tempfile::TempDir, ReadContext) {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("wt");
        let home = dir.path().join("home");
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(root.join(".git/config"), "[core]\n\tbare = false\n").expect("config");
        let ctx = ReadContext::in_worktree(&root, Some(&home));
        (dir, ctx)
    }

    /// A credential helper that is a path, or has a backslash (a Windows
    /// path), runs as the program it names; a bare name is `git-credential-NAME`.
    #[test]
    fn a_credential_helper_path_runs_as_named() {
        let none = ReadContext::default();
        for (value, want) in [
            ("/usr/bin/curl x", "/usr/bin/curl x"),
            ("C:\\tools\\helper.exe x", "C:\\tools\\helper.exe x"),
            ("tools\\helper x", "tools\\helper x"),
            ("store --file f", "git-credential-store --file f"),
        ] {
            let setting = format!("credential.helper={value}");
            let found = read(&["git", "-c", &setting, "fetch"], &none);
            assert_eq!(scripts(&found), vec![want], "{value}");
        }
        // A drive path with forward slashes, as Git for Windows writes it,
        // is a path only where it is one.
        let setting = "credential.helper=C:/tools/helper.exe x";
        let found = read(&["git", "-c", setting, "fetch"], &none);
        let want = if cfg!(windows) {
            "C:/tools/helper.exe x"
        } else {
            "git-credential-C:/tools/helper.exe x"
        };
        assert_eq!(scripts(&found), vec![want]);
    }

    /// The program variables run what they hold, and the variables that
    /// replace git's configuration keep the call unreadable; a variable git
    /// does not read is ignored.
    #[test]
    fn environment_variables_that_run_or_replace_configuration_are_read() {
        let none = ReadContext::default();
        for name in ["GIT_SSH", "GIT_ASKPASS", "SSH_ASKPASS"] {
            let assignment = format!("{name}=curl x");
            let found = read(&[&assignment, "git", "fetch"], &none);
            assert_eq!(scripts(&found), vec!["curl x"], "{name}");
        }
        for name in [
            "GIT_SSH_COMMAND",
            "GIT_EXTERNAL_DIFF",
            "GIT_PAGER",
            "PAGER",
            "GIT_EDITOR",
            "EDITOR",
            "VISUAL",
            "GIT_SEQUENCE_EDITOR",
            "GIT_PROXY_COMMAND",
        ] {
            assert_eq!(env_runs(name), Some(KeyRuns::Script), "{name}");
        }
        for name in [
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_EXEC_PATH",
            "GIT_TEMPLATE_DIR",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_7",
        ] {
            assert_eq!(env_runs(name), Some(KeyRuns::Opaque), "{name}");
            let assignment = format!("{name}=x");
            assert!(opaque(&read(&[&assignment, "git", "log"], &none)), "{name}");
        }
        for name in ["FOO", "GIT_CONFIG_KEY", "GIT_CONFIG_VAL", "HOME", "GIT_DIR"] {
            assert_eq!(env_runs(name), None, "{name}");
        }
        assert!(read(&["FOO=bar", "git", "status"], &none).is_empty());
    }

    /// A call that moves git's configuration or directory (`HOME`, `-C`,
    /// `--git-dir`, `--work-tree`) cannot be matched to an alias, each alone;
    /// a short option with an `=` is not split, and an option given after an
    /// attached value still counts.
    #[test]
    fn a_moved_configuration_or_directory_makes_a_name_unreadable() {
        let (_keep, ctx) = plain_worktree();
        assert_eq!(scripts(&read(&["git", "lfs"], &ctx)), vec!["git-lfs"]);
        for words in [
            vec!["HOME=/x", "git", "lfs"],
            vec!["XDG_CONFIG_HOME=/x", "git", "lfs"],
            vec!["GIT_DIR=/x", "git", "lfs"],
            vec!["GIT_COMMON_DIR=/x", "git", "lfs"],
            vec!["git", "-C", "/x", "lfs"],
            vec!["git", "--git-dir=/x", "lfs"],
            vec!["git", "--git-dir", "/x", "lfs"],
            vec!["git", "--work-tree", "/w", "lfs"],
        ] {
            assert!(opaque(&read(&words, &ctx)), "{words:?}");
        }
        // `-C=/x` is no option git knows; it moves nothing here.
        assert_eq!(
            scripts(&read(&["git", "-C=/x", "lfs"], &ctx)),
            vec!["git-lfs"]
        );
        // A valued option with its value attached does not take the next word.
        let found = read(
            &["git", "--work-tree=/w", "-c", "core.pager=curl x", "log"],
            &ReadContext::default(),
        );
        assert_eq!(scripts(&found), vec!["curl x"]);
        // `--exec-path` alone prints the path; with a value it is refused.
        assert!(read(&["git", "--exec-path"], &ReadContext::default()).is_empty());
        assert!(opaque(&read(
            &["git", "--exec-path", "/x", "status"],
            &ReadContext::default()
        )));
    }

    /// Subcommands that run a command given as an option value or word.
    #[test]
    fn subcommand_options_that_run_commands_are_read() {
        let none = ReadContext::default();
        for (words, want) in [
            (vec!["git", "difftool", "-x", "curl x"], vec!["curl x"]),
            (vec!["git", "difftool", "--extcmd=curl x"], vec!["curl x"]),
            (vec!["git", "difftool", "-xcurl x"], vec!["curl x"]),
            (
                vec!["git", "send-email", "--sendmail-cmd=curl x", "a.patch"],
                vec!["curl x"],
            ),
            (
                vec!["git", "send-email", "--smtp-server", "/bin/x"],
                vec!["/bin/x"],
            ),
            (vec!["git", "send-email", "--to-cmd=curl x"], vec!["curl x"]),
            (
                vec!["git", "send-email", "--cc-cmd", "curl x"],
                vec!["curl x"],
            ),
            (
                vec!["git", "config", "set", "core.pager", "curl x"],
                vec!["curl x"],
            ),
            (
                vec!["git", "config", "set", "alias.x", "!curl x"],
                vec!["curl x"],
            ),
            (
                vec!["git", "config", "--file", "cfg", "core.pager", "curl x"],
                vec!["curl x"],
            ),
            (
                vec!["git", "config", "-f", "cfg", "core.pager", "curl x"],
                vec!["curl x"],
            ),
            (
                vec!["git", "config", "--type", "bool", "core.pager", "curl x"],
                vec!["curl x"],
            ),
            (
                vec!["git", "submodule", "foreach", "-q", "curl x"],
                vec!["curl x"],
            ),
            (
                vec![
                    "git",
                    "submodule",
                    "foreach",
                    "--quiet",
                    "--recursive",
                    "curl",
                    "x",
                ],
                vec!["curl x"],
            ),
        ] {
            assert_eq!(scripts(&read(&words, &none)), want, "{words:?}");
        }
    }

    /// Words that look like the arms above but run nothing.
    #[test]
    fn subcommand_words_that_run_nothing_are_not_read_as_commands() {
        let none = ReadContext::default();
        for words in [
            vec!["git", "rebase", "-Xours", "main"],
            vec!["git", "rebase", "-i", "main"],
            vec!["git", "bisect", "good"],
            vec!["git", "bisect", "start", "HEAD~2"],
            vec!["git", "bisect", "run"],
            vec!["git", "submodule", "update"],
            vec!["git", "config", "user.name", "x"],
        ] {
            assert!(read(&words, &none).is_empty(), "{words:?}");
        }
    }
}

#[cfg(test)]
#[path = "permission_git_more_tests.rs"]
mod more_tests;
