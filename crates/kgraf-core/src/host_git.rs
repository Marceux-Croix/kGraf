//! The one way kGraf runs git on the host for its own operations: commits,
//! merges, worktree creation and removal, status, fsck, history reads.
//!
//! kGraf's git runs outside the worker sandbox, in the user's repository. A
//! repository can make git run a program or reach the network on its own: a
//! hook, a filter, merge or diff driver that `.gitattributes` selects and the
//! config defines, a config-defined hook (`hook.<name>.command`), the file
//! system monitor, a credential helper, submodule recursion. A worker in the
//! Bypass mode may write the git config and the attribute files, so none of
//! that may reach kGraf's own git. [`command`] builds every such invocation
//! with these overrides, which no caller can leave out:
//!
//! - **Fixed overrides** ([`FIXED_OVERRIDES`]): hooks off (`core.hooksPath` at
//!   the null device), the file system monitor off, editors set to `:` (git
//!   never launches it), the credential helper list and `core.askPass`
//!   emptied, every transport refused (`protocol.allow=never`, and
//!   `GIT_ALLOW_PROTOCOL=none` below, which also outranks a repository's
//!   `protocol.<name>.allow`; kGraf's git is local only, so a lazy fetch in a
//!   partial clone fails instead of fetching), no submodule recursion, commit and tag signing and signature
//!   checks off (they run `gpg.program`), `diff.external` emptied, automatic
//!   gc and maintenance off.
//! - **Per-repository overrides** ([`driver_overrides`]): git cannot disable
//!   drivers or config hooks by kind, only by name, so before each command
//!   the effective config is listed (`git config --list --includes -z`, with
//!   the same location and environment) and every name found is neutralized:
//!   `filter.<name>.clean`, `.smudge` and `.process` emptied and `.required`
//!   false (git then stores and checks out the bytes unchanged);
//!   `merge.<name>.driver` emptied (git cannot run it and reports the path as
//!   a conflict, never as resolved); `diff.<name>.textconv` and `.command`
//!   emptied; `hook.<name>.enabled` false; `tar.<name>.command` emptied.
//!   An empty command is never run: git fails with "cannot run" instead.
//! - **Per-subcommand flags**: `diff`, `log`, `show` and `whatchanged` get
//!   `--no-ext-diff --no-textconv`, `blame` and `annotate` `--no-textconv`,
//!   so they work on a path with a diff driver instead of failing on the
//!   emptied command.
//! - **Environment**: every inherited `GIT_*` variable is removed except the
//!   commit identity (`GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`,
//!   `GIT_COMMITTER_NAME`, `GIT_COMMITTER_EMAIL`) and the config file
//!   locations (`GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`), so an inherited
//!   `GIT_DIR`, `GIT_EXTERNAL_DIFF`, `GIT_SSH_COMMAND` or `GIT_CONFIG_*`
//!   cannot redirect or reconfigure the command. `GIT_TERMINAL_PROMPT=0`,
//!   `GIT_NO_LAZY_FETCH=1`, `GIT_ALLOW_PROTOCOL=none`, `GIT_MERGE_AUTOEDIT=no`
//!   and `LC_ALL=C` are set,
//!   and `--no-pager` is passed.
//!
//! The overrides travel as `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_<n>`/
//! `GIT_CONFIG_VALUE_<n>` rather than `-c key=value`: `-c` splits at the
//! first `=`, so a driver named `a=b` (a valid subsection) would be misread
//! and left active. The variables also reach the git processes git itself
//! starts. They need git 2.31; an older git is refused ([`HostGitError::TooOld`])
//! rather than run without them.
//!
//! What is kept: the user's global and system config and attributes, so the
//! commit identity, `core.autocrlf`, `text`, `eol`, `ident` and
//! `working-tree-encoding` work as in the user's own git (they are built in,
//! not programs). Not set: `GIT_CONFIG_NOSYSTEM` (on Windows the system config
//! carries `core.autocrlf`; its drivers are neutralized like any other) and
//! `GIT_ATTR_NOSYSTEM` (attributes only select drivers, and every configured
//! driver is neutralized). A caller may still set either.
//!
//! Consequence: a path whose attributes select a configured filter (git-lfs,
//! git-crypt) is stored and checked out by kGraf in its committed form, not
//! filtered. The user's own git is untouched and still honors every hook,
//! filter and driver.
//!
//! The listing and the command are two processes, so nothing may change the
//! configuration between them (decision 0094):
//!
//! - A protected write through `apply_patch` holds the repository's lock
//!   exclusively, and every caller that runs git on a repository takes it
//!   shared ([`crate::repo_lock`]) before calling [`command`] and holds it
//!   until the command exits, so the listing and the command run in one
//!   hold that such a write cannot enter.
//! - A Bypass shell command holds no lock while it runs. Its sandbox keeps
//!   every file git reads configuration from read-only (the repository's
//!   `config`, `config.worktree`, `info/attributes`, each worktree's
//!   `commondir`, and every include target [`IncludeScan`] finds), so it
//!   cannot write between them either. Where the sandbox cannot do that, the
//!   command runs with the git directory read-only.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

mod includes;
pub use includes::{IncludeScan, RACY as INCLUDE_SCAN_RACY};

/// The oldest git whose `GIT_CONFIG_COUNT` carries the overrides.
pub const MIN_GIT: (u32, u32) = (2, 31);

/// The null device as git resolves a path setting. `core.hooksPath` at it
/// gives `<null>/<hook>`, which never exists. Git for Windows also accepts
/// `/dev/null` in some places, but `NUL` is the native device name.
pub const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// Settings applied to every invocation, whatever the repository holds.
pub const FIXED_OVERRIDES: &[(&str, &str)] = &[
    ("core.hooksPath", NULL_DEVICE),
    ("core.fsmonitor", "false"),
    ("core.editor", ":"),
    ("sequence.editor", ":"),
    ("core.askPass", ""),
    ("credential.helper", ""),
    ("protocol.allow", "never"),
    ("submodule.recurse", "false"),
    ("fetch.recurseSubmodules", "false"),
    ("push.recurseSubmodules", "no"),
    ("status.submoduleSummary", "false"),
    ("commit.gpgSign", "false"),
    ("tag.gpgSign", "false"),
    ("merge.verifySignatures", "false"),
    ("log.showSignature", "false"),
    ("diff.external", ""),
    ("gc.auto", "0"),
    ("maintenance.auto", "false"),
];

/// Inherited `GIT_*` variables kept: the commit identity and where the
/// config files are. Everything else starting with `GIT_` is removed.
pub const KEPT_ENV: &[&str] = &[
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
];

/// Why a hardened git command could not be built.
#[derive(Debug, thiserror::Error)]
pub enum HostGitError {
    #[error("cannot run git ({exe}): {source}")]
    Spawn {
        exe: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{found} is too old for kGraf's own git operations: git {}.{} or newer is needed \
         (GIT_CONFIG_COUNT carries the settings that keep hooks and drivers from running)",
        MIN_GIT.0,
        MIN_GIT.1
    )]
    TooOld { found: String },
    /// Listing the config failed, so its drivers cannot be neutralized; the
    /// command was not run. The command itself would read the same config,
    /// so callers treat this as git failing with this status and message.
    #[error("git cannot read the repository's configuration: {stderr}")]
    Config { code: Option<i32>, stderr: String },
    #[error("git listed configuration kGraf cannot read: {0}")]
    Parse(String),
    #[error("`{0}` is not accepted as a global git option here")]
    Misuse(String),
}

/// Where and how a hardened command runs.
#[derive(Debug, Clone, Copy)]
pub struct Invocation<'a> {
    /// The git executable (`git` resolves through `PATH`).
    pub exe: &'a Path,
    /// The process's working directory; `None` keeps the caller's.
    pub cwd: Option<&'a Path>,
    /// Global options that locate the repository, before the subcommand
    /// (`-C <dir>`, `--git-dir=`, `--work-tree=`, `--attr-source=`). The
    /// config listing runs with the same ones.
    pub pre: &'a [OsString],
}

impl<'a> Invocation<'a> {
    /// `git -C <dir>` with `git` from `PATH`.
    pub fn in_dir(pre: &'a [OsString]) -> Self {
        Self {
            exe: Path::new("git"),
            cwd: None,
            pre,
        }
    }
}

/// `["-C", dir]`, the usual [`Invocation::pre`].
pub fn dash_c(dir: &Path) -> Vec<OsString> {
    vec![OsString::from("-C"), dir.as_os_str().to_owned()]
}

/// A hardened `git <pre> <args>`. Stdin, stdout and stderr are left for the
/// caller to set. The caller may add environment variables (an index file,
/// an identity, `GIT_CONFIG_NOSYSTEM`) but must not pass `-c` options that
/// undo an override: `-c` outranks `GIT_CONFIG_COUNT`.
pub fn command<S: AsRef<OsStr>>(inv: &Invocation<'_>, args: &[S]) -> Result<Command, HostGitError> {
    check_version(inv.exe)?;
    let args: Vec<&OsStr> = args.iter().map(AsRef::as_ref).collect();
    // Global options a caller put ahead of the subcommand join `pre`, so the
    // listing sees the same repository (`--git-dir X`) the command will.
    let (globals, args) = split_globals(&args)?;
    let mut pre: Vec<OsString> = inv.pre.to_vec();
    pre.extend(globals.iter().map(|g| (*g).to_owned()));
    let inv = Invocation { pre: &pre, ..*inv };
    let listing = list_config(&inv)?;
    let mut overrides: Vec<(String, String)> = FIXED_OVERRIDES
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    overrides.extend(driver_overrides(&listing)?);
    let mut cmd = base(&inv);
    set_overrides(&mut cmd, &overrides);
    if let Some((sub, rest)) = args.split_first() {
        cmd.arg(sub);
        cmd.args(subcommand_flags(&sub.to_string_lossy()));
        cmd.args(rest);
    }
    Ok(cmd)
}

/// Global options that take their value as the next argument.
const GLOBALS_WITH_VALUE: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--config-env",
    "--attr-source",
];

/// The leading global options of `args`, and the subcommand with its
/// arguments. `--exec-path` is refused: it names the directory git runs its
/// own programs from.
fn split_globals<'s>(args: &[&'s OsStr]) -> Result<(Vec<&'s OsStr>, Vec<&'s OsStr>), HostGitError> {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let text = arg.to_string_lossy();
        if !text.starts_with('-') {
            break;
        }
        if text.starts_with("--exec-path") || text == "--" {
            return Err(HostGitError::Misuse(text.into_owned()));
        }
        i += if GLOBALS_WITH_VALUE.contains(&text.as_ref()) {
            2
        } else {
            1
        };
    }
    if i > args.len() {
        return Err(HostGitError::Misuse(
            args.last()
                .map(|a| a.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ));
    }
    Ok((args[..i].to_vec(), args[i..].to_vec()))
}

/// The flags [`command`] adds right after `sub`.
pub fn subcommand_flags(sub: &str) -> &'static [&'static str] {
    match sub {
        "diff" | "log" | "show" | "whatchanged" => &["--no-ext-diff", "--no-textconv"],
        "blame" | "annotate" => &["--no-textconv"],
        _ => &[],
    }
}

/// The scrubbed environment, `--no-pager` and the location options, without
/// any override: the start of both the listing and the command.
fn base(inv: &Invocation<'_>) -> Command {
    let mut cmd = Command::new(inv.exe);
    if let Some(cwd) = inv.cwd {
        cmd.current_dir(cwd);
    }
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("GIT_") && !KEPT_ENV.contains(&name.as_ref()) {
            cmd.env_remove(&key);
        }
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        // Outranks every `protocol.<name>.allow` in any config: a repository's
        // `protocol.file.allow=always` beats `protocol.allow=never`, this
        // does not. No protocol is named `none`, so none is allowed.
        .env("GIT_ALLOW_PROTOCOL", "none")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .env("LC_ALL", "C")
        .arg("--no-pager")
        .args(inv.pre);
    cmd
}

fn set_overrides(cmd: &mut Command, overrides: &[(String, String)]) {
    for (n, (key, value)) in overrides.iter().enumerate() {
        cmd.env(format!("GIT_CONFIG_KEY_{n}"), key)
            .env(format!("GIT_CONFIG_VALUE_{n}"), value);
    }
    cmd.env("GIT_CONFIG_COUNT", overrides.len().to_string());
}

/// The effective config, NUL-separated `key\nvalue` records. Run with the
/// fixed overrides so the listing itself is as constrained as the command.
fn list_config(inv: &Invocation<'_>) -> Result<Vec<u8>, HostGitError> {
    let mut cmd = base(inv);
    let fixed: Vec<(String, String)> = FIXED_OVERRIDES
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    set_overrides(&mut cmd, &fixed);
    cmd.args(["config", "--includes", "-z", "--list"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().map_err(|source| HostGitError::Spawn {
        exe: inv.exe.display().to_string(),
        source,
    })?;
    if !out.status.success() {
        return Err(HostGitError::Config {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(out.stdout)
}

/// The settings that neutralize every driver and config hook named in a
/// `git config -z --list` listing.
pub fn driver_overrides(listing: &[u8]) -> Result<Vec<(String, String)>, HostGitError> {
    let mut names: BTreeSet<(String, String)> = BTreeSet::new();
    for record in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let key = record.split(|b| *b == b'\n').next().unwrap_or_default();
        let key = std::str::from_utf8(key)
            .map_err(|_| HostGitError::Parse(String::from_utf8_lossy(key).into_owned()))?;
        // `section.subsection.variable`: section and variable never hold a
        // dot, the subsection may. Two-part keys name no driver.
        let Some((section, rest)) = key.split_once('.') else {
            return Err(HostGitError::Parse(key.to_owned()));
        };
        let Some((sub, _variable)) = rest.rsplit_once('.') else {
            continue;
        };
        let section = section.to_ascii_lowercase();
        if matches!(
            section.as_str(),
            "filter" | "merge" | "diff" | "hook" | "tar"
        ) {
            names.insert((section, sub.to_owned()));
        }
    }
    let mut out = Vec::new();
    for (section, name) in names {
        let set: &[(&str, &str)] = match section.as_str() {
            "filter" => &[
                ("clean", ""),
                ("smudge", ""),
                ("process", ""),
                ("required", "false"),
            ],
            "merge" => &[("driver", "")],
            "diff" => &[("textconv", ""), ("command", "")],
            "hook" => &[("enabled", "false")],
            "tar" => &[("command", "")],
            _ => &[],
        };
        for (variable, value) in set {
            out.push((format!("{section}.{name}.{variable}"), (*value).to_owned()));
        }
    }
    Ok(out)
}

/// Executables already checked, with the result. A git binary's version does
/// not change while kGraf runs, and checking costs a process, so it is
/// checked once per path.
static CHECKED: Mutex<Vec<(PathBuf, Result<(), String>)>> = Mutex::new(Vec::new());

fn check_version(exe: &Path) -> Result<(), HostGitError> {
    {
        let checked = CHECKED.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, result)) = checked.iter().find(|(p, _)| p == exe) {
            return result
                .clone()
                .map_err(|found| HostGitError::TooOld { found });
        }
    }
    let out = Command::new(exe)
        .arg("version")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|source| HostGitError::Spawn {
            exe: exe.display().to_string(),
            source,
        })?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let result = match parse_version(&text) {
        Some(v) if out.status.success() && v >= MIN_GIT => Ok(()),
        _ => Err(if text.is_empty() {
            "git (no version printed)".to_owned()
        } else {
            text
        }),
    };
    CHECKED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((exe.to_path_buf(), result.clone()));
    result.map_err(|found| HostGitError::TooOld { found })
}

/// `(major, minor)` from `git version 2.54.0` or `git version 2.39.3 (Apple Git-146)`.
pub fn parse_version(text: &str) -> Option<(u32, u32)> {
    let rest = text.trim().strip_prefix("git version ")?;
    let mut parts = rest.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

#[cfg(test)]
mod tests;
