//! The build and test allowlist (decision 0084): the commands a worker with a
//! declared write scope may run through the shell tool without approval.
//!
//! **Why it exists.** Before 0084, every command the policy did not classify
//! read-only needed approval, and an unattended worker's approval is a denial;
//! under a declared write scope the worker loop also refused any shell call,
//! since a shell call declares no paths (the guard of commit 16291df). So an
//! orchestrated worker could never compile or test its own work. Decision
//! 0084 allows a declared set of build and test commands, and nothing else
//! that can write, inside the OS sandbox with the network off.
//!
//! **What an entry is.** Either a cargo subcommand checked by the argument
//! rules below ([`CargoSubcommand`]), or one exact argv ([`BuildCommand::Exact`]):
//! the project's own configured acceptance or gate command, matched only when
//! the worker's argv is identical to it.
//!
//! **Argument rules are an allowlist, not a denylist.** A cargo argument is
//! accepted only when it is on the list for its subcommand; everything else is
//! refused with the argument named. So an option cargo adds later is refused
//! until someone reads it and lists it. The rules keep a command to "build or
//! test this worktree": `--manifest-path` is relative, inside the worktree and
//! never through `..`; `--config`, `-Z`, `--target-dir`, `--artifact-dir`,
//! `-C`, `--lockfile-path`, a `+toolchain` and a target spec file are refused;
//! `cargo fmt` needs `--check` (cargo's own, or rustfmt's after `--`); `cargo clippy --fix` is refused; arguments after
//! `--` are held to the test harness's and clippy's own short lists.
//!
//! **The rules are not the security boundary.** `cargo test` runs code the
//! worker wrote, and that code can do whatever the sandbox allows. What holds
//! the line is the sandbox (writes confined to the worktree and its scratch
//! directory, the gitlink read-only, the network off, kGraf's state
//! unreadable) and the verifier's scope check on the committed attempt, which
//! refuses any change outside the declared writes. The argument rules keep the
//! command honest about what it is; they are defense in depth.
//!
//! **Written new.** Searched: this crate's port of codex-rs `execpolicy`
//! (prefix rules only; codex's own "known safe" list covers read-only commands
//! and has no cargo argument rules), and this repository's evidence and reuse
//! candidates (decision 0055). No network search was made.

use std::fmt;

/// A cargo subcommand the allowlist can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CargoSubcommand {
    Build,
    Check,
    Test,
    Clippy,
    /// `cargo fmt`, and only with `--check`: without it, fmt rewrites files.
    FmtCheck,
}

impl CargoSubcommand {
    /// The subcommand word as argv carries it.
    fn word(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Check => "check",
            Self::Test => "test",
            Self::Clippy => "clippy",
            Self::FmtCheck => "fmt",
        }
    }

    /// The configuration spelling: `cargo build`, ..., `cargo fmt --check`.
    pub fn spelling(self) -> &'static str {
        match self {
            Self::Build => "cargo build",
            Self::Check => "cargo check",
            Self::Test => "cargo test",
            Self::Clippy => "cargo clippy",
            Self::FmtCheck => "cargo fmt --check",
        }
    }

    const ALL: [Self; 5] = [
        Self::Build,
        Self::Check,
        Self::Test,
        Self::Clippy,
        Self::FmtCheck,
    ];
}

/// One allowlist entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildCommand {
    /// A cargo subcommand, held to that subcommand's argument rules.
    Cargo(CargoSubcommand),
    /// Exactly this argv: nothing added, removed or reordered.
    Exact(Vec<String>),
}

impl fmt::Display for BuildCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cargo(sub) => f.write_str(sub.spelling()),
            Self::Exact(argv) => write!(f, "exactly {argv:?}"),
        }
    }
}

/// A configuration entry that names no command the allowlist knows.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "`{entry}` is not a build command kGraf can allow; use one of `cargo build`, \
     `cargo check`, `cargo test`, `cargo clippy`, `cargo fmt --check` (a run's own \
     acceptance and gate commands are allowed without being listed here)"
)]
pub struct UnknownBuildCommand {
    pub entry: String,
}

/// How one argv stands against the allowlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AllowlistMatch {
    /// On the list, and every argument passes its rules.
    Allowed,
    /// A listed cargo subcommand whose arguments break a rule: refused, with
    /// the reason naming the argument.
    Refused { reason: String },
    /// Not on the list at all: the command policy decides as it did before.
    NotListed,
}

/// The set of build and test commands (see the module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildAllowlist {
    commands: Vec<BuildCommand>,
}

impl BuildAllowlist {
    /// Nothing is allowed.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The safe default for a project that does not configure one: the five
    /// cargo commands, each under its argument rules.
    pub fn cargo_default() -> Self {
        Self {
            commands: CargoSubcommand::ALL
                .into_iter()
                .map(BuildCommand::Cargo)
                .collect(),
        }
    }

    /// Parse the configuration spelling of the cargo entries
    /// ([`CargoSubcommand::spelling`]), words separated by any whitespace.
    pub fn parse_cargo_entry(entry: &str) -> Result<BuildCommand, UnknownBuildCommand> {
        let words: Vec<&str> = entry.split_whitespace().collect();
        CargoSubcommand::ALL
            .into_iter()
            .find(|sub| sub.spelling().split(' ').eq(words.iter().copied()))
            .map(BuildCommand::Cargo)
            .ok_or_else(|| UnknownBuildCommand {
                entry: entry.to_owned(),
            })
    }

    /// Add one entry (a duplicate is kept once).
    pub fn with(mut self, command: BuildCommand) -> Self {
        if !self.commands.contains(&command) {
            self.commands.push(command);
        }
        self
    }

    /// Add a command the project configured as a shell string (the
    /// acceptance command `verify`, a gate command), exactly as configured:
    /// as `sh -c <string>`, which is how the verifier and the gate run it, and,
    /// when the string holds no shell syntax, also as its whitespace-split argv.
    pub fn with_configured(self, command: &str) -> Self {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return self;
        }
        let mut out = self.with(BuildCommand::Exact(vec![
            "sh".into(),
            "-c".into(),
            command.to_owned(),
        ]));
        if !trimmed.chars().any(is_shell_syntax) {
            out = out.with(BuildCommand::Exact(
                trimmed.split_whitespace().map(str::to_owned).collect(),
            ));
        }
        out
    }

    pub fn commands(&self) -> &[BuildCommand] {
        &self.commands
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// The entries, for a message or a tool description.
    ///
    /// The cargo entries name their options. A bare list (`cargo build;
    /// cargo test`) was read by models as "only without flags", so a worker
    /// asked for `cargo test --workspace` instead of running it.
    pub fn describe(&self) -> String {
        if self.commands.is_empty() {
            return "none".into();
        }
        let cargo: Vec<String> = self
            .commands
            .iter()
            .filter(|c| matches!(c, BuildCommand::Cargo(_)))
            .map(ToString::to_string)
            .collect();
        let mut parts = Vec::new();
        if !cargo.is_empty() {
            // The `sh -c` form is named: a worker that read only the argv
            // form took `cd crate && cargo check` for a command needing
            // approval, and asked for one instead of running it.
            parts.push(format!(
                "{}, each with its usual options ({CARGO_OPTIONS_SUMMARY}), given as one array \
                 element per argument, or as a `sh -c` script of these commands joined by `&&` \
                 or `;`, after at most one `cd` into a directory of the worktree",
                cargo.join(", ")
            ));
        }
        parts.extend(
            self.commands
                .iter()
                .filter(|c| matches!(c, BuildCommand::Exact(_)))
                .map(ToString::to_string),
        );
        parts.join("; ")
    }

    /// Where `argv` stands. An exact entry wins over the cargo rules, so a
    /// configured acceptance command is never refused by them.
    pub fn check(&self, argv: &[String]) -> AllowlistMatch {
        if self
            .commands
            .iter()
            .any(|c| matches!(c, BuildCommand::Exact(exact) if exact == argv))
        {
            return AllowlistMatch::Allowed;
        }
        let (Some(program), Some(word)) = (argv.first(), argv.get(1)) else {
            return AllowlistMatch::NotListed;
        };
        // Exactly `cargo`, found on PATH: a path (`./cargo`, `/tmp/cargo`) could
        // name a program the worker wrote.
        if program != "cargo" {
            return AllowlistMatch::NotListed;
        }
        let listed = self.commands.iter().find_map(|c| match c {
            BuildCommand::Cargo(sub) if sub.word() == word => Some(*sub),
            _ => None,
        });
        let Some(sub) = listed else {
            return AllowlistMatch::NotListed;
        };
        match check_cargo(sub, &argv[2..]) {
            Ok(()) => AllowlistMatch::Allowed,
            Err(why) => AllowlistMatch::Refused {
                reason: format!(
                    "`{}` is on the build and test allowlist only under its argument rules: {why}",
                    sub.spelling()
                ),
            },
        }
    }
}

impl BuildAllowlist {
    /// Whether `argv` is `cargo SUBCOMMAND ...` for a listed subcommand whose
    /// arguments pass its rules. Exact entries are not consulted: a script
    /// judged command by command (decision 0094) may run a command in a
    /// directory it changed into, and an exact entry is a project command
    /// line that may name a file relative to where it runs.
    pub fn allows_by_cargo_rules(&self, argv: &[String]) -> bool {
        let (Some(program), Some(word)) = (argv.first(), argv.get(1)) else {
            return false;
        };
        if program != "cargo" {
            return false;
        }
        self.commands
            .iter()
            .find_map(|c| match c {
                BuildCommand::Cargo(sub) if sub.word() == word => Some(*sub),
                _ => None,
            })
            .is_some_and(|sub| check_cargo(sub, &argv[2..]).is_ok())
    }
}

/// What [`BuildAllowlist::describe`] says the cargo entries accept. A summary
/// of the lists below, not the lists themselves: the rules are what decide.
const CARGO_OPTIONS_SUMMARY: &str = "--workspace, -p/--package NAME, --exclude NAME, --lib, \
     --bins, --tests, --test NAME, --all-targets, -F/--features, --all-features, \
     --no-default-features, --release, --profile NAME, -q, -v, --locked, --offline, --frozen, \
     -j N, --no-run and --no-fail-fast for tests, one test name filter, and after `--` the \
     test harness's options such as --nocapture and --test-threads N, or clippy's lint \
     levels such as -D warnings; not --config, -Z, +toolchain, --target-dir or a \
     --manifest-path outside the worktree";

/// The words of a whole command line a model gave as one string, when the
/// line holds no shell syntax and more than one word: `"cargo test -p x"` is
/// `["cargo", "test", "-p", "x"]`. `None` otherwise: a line with shell syntax
/// means a shell, and splitting it on whitespace would run something else
/// than what it says (`a && b` is not `a` with arguments `&&` and `b`).
pub fn split_command_line(line: &str) -> Option<Vec<String>> {
    if line.chars().any(is_shell_syntax) {
        return None;
    }
    let words: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
    (words.len() > 1).then_some(words)
}

/// Whether `line` holds a character a shell would interpret.
pub(crate) fn has_shell_syntax(line: &str) -> bool {
    line.chars().any(is_shell_syntax)
}

fn is_shell_syntax(c: char) -> bool {
    matches!(
        c,
        ';' | '&'
            | '|'
            | '`'
            | '$'
            | '('
            | ')'
            | '<'
            | '>'
            | '\''
            | '"'
            | '\\'
            | '*'
            | '?'
            | '['
            | ']'
            | '{'
            | '}'
            | '~'
            | '#'
            | '\n'
    )
}

/// A cargo option's shape.
#[derive(Clone, Copy)]
enum Opt {
    /// Takes no value.
    Flag,
    /// Takes one value, checked by the function.
    Value(fn(&str) -> Result<(), String>),
}

/// Options every compiling subcommand (build, check, test, clippy) takes.
const COMPILE_OPTIONS: &[(&str, Opt)] = &[
    ("--workspace", Opt::Flag),
    ("--all", Opt::Flag),
    ("--lib", Opt::Flag),
    ("--bins", Opt::Flag),
    ("--tests", Opt::Flag),
    ("--examples", Opt::Flag),
    ("--benches", Opt::Flag),
    ("--all-targets", Opt::Flag),
    ("--all-features", Opt::Flag),
    ("--no-default-features", Opt::Flag),
    ("--release", Opt::Flag),
    ("-r", Opt::Flag),
    ("--keep-going", Opt::Flag),
    ("--frozen", Opt::Flag),
    ("--locked", Opt::Flag),
    ("--offline", Opt::Flag),
    ("--ignore-rust-version", Opt::Flag),
    ("--future-incompat-report", Opt::Flag),
    ("-v", Opt::Flag),
    ("-vv", Opt::Flag),
    ("--verbose", Opt::Flag),
    ("-q", Opt::Flag),
    ("--quiet", Opt::Flag),
    ("-p", Opt::Value(name_value)),
    ("--package", Opt::Value(name_value)),
    ("--exclude", Opt::Value(name_value)),
    ("--bin", Opt::Value(name_value)),
    ("--test", Opt::Value(name_value)),
    ("--example", Opt::Value(name_value)),
    ("--bench", Opt::Value(name_value)),
    ("-F", Opt::Value(name_value)),
    ("--features", Opt::Value(name_value)),
    ("--profile", Opt::Value(word_value)),
    ("-j", Opt::Value(jobs_value)),
    ("--jobs", Opt::Value(jobs_value)),
    ("--target", Opt::Value(target_value)),
    ("--message-format", Opt::Value(word_value)),
    ("--color", Opt::Value(color_value)),
    ("--manifest-path", Opt::Value(manifest_value)),
];

/// `cargo test`'s own options, on top of [`COMPILE_OPTIONS`].
const TEST_OPTIONS: &[(&str, Opt)] = &[
    ("--doc", Opt::Flag),
    ("--no-run", Opt::Flag),
    ("--no-fail-fast", Opt::Flag),
];

/// `cargo clippy`'s own options. `--fix` rewrites files and is not listed.
const CLIPPY_OPTIONS: &[(&str, Opt)] = &[("--no-deps", Opt::Flag)];

/// `cargo fmt --check`'s options. `--check` itself is required.
const FMT_OPTIONS: &[(&str, Opt)] = &[
    ("--check", Opt::Flag),
    ("--all", Opt::Flag),
    ("-v", Opt::Flag),
    ("--verbose", Opt::Flag),
    ("-q", Opt::Flag),
    ("--quiet", Opt::Flag),
    ("-p", Opt::Value(name_value)),
    ("--package", Opt::Value(name_value)),
    ("--manifest-path", Opt::Value(manifest_value)),
    ("--message-format", Opt::Value(word_value)),
];

/// Short options whose value may be attached (`-pfoo`, `-j2`, `-Ffoo`).
const ATTACHABLE: &[&str] = &["-p", "-j", "-F"];

/// Options after `--` that the libtest harness takes (filters are bare words).
/// `--logfile` writes a file and `-Z` unlocks unstable options: not listed.
const HARNESS_OPTIONS: &[(&str, Opt)] = &[
    ("--nocapture", Opt::Flag),
    ("--no-capture", Opt::Flag),
    ("--show-output", Opt::Flag),
    ("--ignored", Opt::Flag),
    ("--include-ignored", Opt::Flag),
    ("--exact", Opt::Flag),
    ("--list", Opt::Flag),
    ("-q", Opt::Flag),
    ("--quiet", Opt::Flag),
    ("--test-threads", Opt::Value(jobs_value)),
    ("--skip", Opt::Value(filter_value)),
    ("--color", Opt::Value(color_value)),
    ("--format", Opt::Value(harness_format_value)),
];

/// Options after `--` that clippy takes: lint levels only. Any other rustc
/// flag (`-C linker=...`, `--extern`, `-L`) is not listed.
const LINT_OPTIONS: &[(&str, Opt)] = &[
    ("-D", Opt::Value(lint_value)),
    ("-W", Opt::Value(lint_value)),
    ("-A", Opt::Value(lint_value)),
    ("-F", Opt::Value(lint_value)),
    ("--deny", Opt::Value(lint_value)),
    ("--warn", Opt::Value(lint_value)),
    ("--allow", Opt::Value(lint_value)),
    ("--forbid", Opt::Value(lint_value)),
];

/// Short lint options whose value may be attached (`-Dwarnings`).
const LINT_ATTACHABLE: &[&str] = &["-D", "-W", "-A", "-F"];

/// Whether `argv` is a cargo command a persisted exact "always allow" rule may
/// name (the shell tool's `permission_subject`). The same argument rules as the
/// build allowlist, so a rule can never cover a spelling the allowlist refuses
/// for smuggling (`--config`, `-Z`, `+toolchain`, a `--manifest-path` outside
/// the worktree). One difference: `cargo fmt` may omit `--check`, since a user
/// who answered "always" chose to let it rewrite files; `--check` is required
/// by the allowlist only because that list runs without anyone being asked.
pub(crate) fn cargo_rule_subject_allowed(argv: &[String]) -> bool {
    let (Some(program), Some(word)) = (argv.first(), argv.get(1)) else {
        return false;
    };
    if program != "cargo" {
        return false;
    }
    let Some(sub) = CargoSubcommand::ALL.into_iter().find(|s| s.word() == word) else {
        return false;
    };
    let args = &argv[2..];
    if sub == CargoSubcommand::FmtCheck && !args.iter().any(|a| a == "--check") {
        let mut with_check = args.to_vec();
        with_check.insert(0, "--check".to_owned());
        return check_cargo(sub, &with_check).is_ok();
    }
    check_cargo(sub, args).is_ok()
}

fn check_cargo(sub: CargoSubcommand, args: &[String]) -> Result<(), String> {
    let split = args.iter().position(|a| a == "--");
    let (own, passthrough) = match split {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[][..]),
    };
    let lists: Vec<&[(&str, Opt)]> = match sub {
        CargoSubcommand::Build | CargoSubcommand::Check => vec![COMPILE_OPTIONS],
        CargoSubcommand::Test => vec![COMPILE_OPTIONS, TEST_OPTIONS],
        CargoSubcommand::Clippy => vec![COMPILE_OPTIONS, CLIPPY_OPTIONS],
        CargoSubcommand::FmtCheck => vec![FMT_OPTIONS],
    };
    // `cargo test NAME`: one bare filter word is the test name filter.
    let bare_allowed = if sub == CargoSubcommand::Test { 1 } else { 0 };
    check_options(own, &lists, ATTACHABLE, bare_allowed, filter_value)?;
    // `cargo fmt --all -- --check` hands `--check` to rustfmt, which is the
    // same check-only mode as cargo's own `--check`.
    let rustfmt_check = passthrough == ["--check"];
    if sub == CargoSubcommand::FmtCheck
        && !own.iter().any(|a| a == "--check")
        && !passthrough.iter().any(|a| a == "--check")
    {
        return Err("`cargo fmt` rewrites files unless it has `--check`".into());
    }
    match sub {
        _ if split.is_none() => Ok(()),
        CargoSubcommand::FmtCheck if rustfmt_check => Ok(()),
        CargoSubcommand::Test => check_options(
            passthrough,
            &[HARNESS_OPTIONS],
            &[],
            usize::MAX,
            filter_value,
        ),
        CargoSubcommand::Clippy => check_options(
            passthrough,
            &[LINT_OPTIONS],
            LINT_ATTACHABLE,
            0,
            filter_value,
        ),
        other => Err(format!(
            "`{}` takes no arguments after `--`",
            other.spelling()
        )),
    }
}

/// Every argument must be a listed option (with a valid value), or one of at
/// most `bare_allowed` bare words, each valid under `bare`.
fn check_options(
    args: &[String],
    lists: &[&[(&str, Opt)]],
    attachable: &[&str],
    bare_allowed: usize,
    bare: fn(&str) -> Result<(), String>,
) -> Result<(), String> {
    let lookup = |name: &str| {
        lists
            .iter()
            .flat_map(|l| l.iter())
            .find(|(n, _)| *n == name)
            .map(|(_, o)| *o)
    };
    let mut bare_seen = 0;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        i += 1;
        if arg.chars().any(char::is_control) {
            return Err(format!("{arg:?} holds a control character"));
        }
        if !arg.starts_with('-') || arg == "-" {
            bare_seen += 1;
            if bare_seen > bare_allowed {
                return Err(format!("the argument `{arg}` is not allowed here"));
            }
            bare(arg).map_err(|why| format!("`{arg}`: {why}"))?;
            continue;
        }
        // `--name=value`
        if let Some((name, value)) = arg.split_once('=')
            && name.starts_with("--")
        {
            match lookup(name) {
                Some(Opt::Value(check)) => {
                    check(value).map_err(|why| format!("`{name}` {value:?}: {why}"))?;
                    continue;
                }
                Some(Opt::Flag) => return Err(format!("`{name}` takes no value")),
                None => return Err(not_listed(name)),
            }
        }
        match lookup(arg) {
            Some(Opt::Flag) => continue,
            Some(Opt::Value(check)) => {
                let Some(value) = args.get(i) else {
                    return Err(format!("`{arg}` needs a value"));
                };
                i += 1;
                check(value).map_err(|why| format!("`{arg}` {value:?}: {why}"))?;
                continue;
            }
            None => {}
        }
        // `-pfoo`, `-j2`, `-Dwarnings`
        if let Some(short) = attachable
            .iter()
            .find(|s| arg.len() > 2 && arg.starts_with(**s))
        {
            let value = &arg[short.len()..];
            if let Some(Opt::Value(check)) = lookup(short) {
                check(value).map_err(|why| format!("`{short}` {value:?}: {why}"))?;
                continue;
            }
        }
        return Err(not_listed(arg));
    }
    Ok(())
}

fn not_listed(name: &str) -> String {
    let hint = match name {
        "--manifest-path" => "",
        "--config" => " (it can set any cargo configuration, a runner or a wrapper included)",
        "-Z" => " (unstable options)",
        "--target-dir" | "--artifact-dir" | "--out-dir" => {
            " (build output goes to the worktree's scratch directory, CARGO_TARGET_DIR)"
        }
        "-C" => " (it changes the directory cargo works in)",
        "--lockfile-path" => " (it names a lockfile elsewhere)",
        "--fix" | "--allow-dirty" | "--allow-staged" => " (it rewrites files)",
        "--logfile" => " (it writes a file)",
        _ => "",
    };
    format!("the option `{name}` is not on the allowlist{hint}")
}

fn name_value(v: &str) -> Result<(), String> {
    if v.is_empty() || v.starts_with('-') {
        return Err("expected a name".into());
    }
    Ok(())
}

fn filter_value(v: &str) -> Result<(), String> {
    if v.is_empty() {
        return Err("empty".into());
    }
    Ok(())
}

fn word_value(v: &str) -> Result<(), String> {
    if !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ','))
    {
        Ok(())
    } else {
        Err("expected a plain word".into())
    }
}

fn jobs_value(v: &str) -> Result<(), String> {
    let digits = v.strip_prefix('-').unwrap_or(v);
    if v == "default" || (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())) {
        Ok(())
    } else {
        Err("expected a number".into())
    }
}

fn color_value(v: &str) -> Result<(), String> {
    match v {
        "auto" | "always" | "never" => Ok(()),
        _ => Err("expected auto, always or never".into()),
    }
}

fn harness_format_value(v: &str) -> Result<(), String> {
    match v {
        "pretty" | "terse" => Ok(()),
        _ => Err("expected pretty or terse".into()),
    }
}

/// A target triple, never a path to a target specification file (which cargo
/// would read from anywhere).
fn target_value(v: &str) -> Result<(), String> {
    if !v.is_empty()
        && !v.ends_with(".json")
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        Ok(())
    } else {
        Err("expected a target triple, not a path".into())
    }
}

fn lint_value(v: &str) -> Result<(), String> {
    if !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':'))
    {
        Ok(())
    } else {
        Err("expected a lint name".into())
    }
}

/// A manifest inside the worktree: relative, no `..` at all, ending in
/// `Cargo.toml`. A symlink out is refused on disk by the shell tool before the
/// command runs.
fn manifest_value(v: &str) -> Result<(), String> {
    let path = std::path::Path::new(v);
    if path.is_absolute() || v.starts_with('~') {
        return Err("must be a path relative to the worktree".into());
    }
    if path.components().any(|c| {
        !matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err("must stay inside the worktree (no `..`)".into());
    }
    if path.file_name().and_then(|n| n.to_str()) != Some("Cargo.toml") {
        return Err("must name a Cargo.toml".into());
    }
    Ok(())
}

/// Every `--manifest-path` value of an allowed cargo argv, for the on-disk
/// check. All of them, not the first: cargo 1.95 refuses a repeated
/// `--manifest-path`, but the check must not depend on which one a cargo
/// version would use.
pub(crate) fn manifest_paths_of(argv: &[String]) -> Vec<&str> {
    let own_end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    let own = &argv[..own_end];
    own.iter()
        .enumerate()
        .filter_map(|(i, a)| {
            if a == "--manifest-path" {
                own.get(i + 1).map(String::as_str)
            } else {
                a.strip_prefix("--manifest-path=")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    fn check(words: &[&str]) -> AllowlistMatch {
        BuildAllowlist::cargo_default().check(&argv(words))
    }

    #[track_caller]
    fn allowed(words: &[&str]) {
        assert_eq!(check(words), AllowlistMatch::Allowed, "{words:?}");
    }

    #[track_caller]
    fn refused(words: &[&str], needle: &str) {
        match check(words) {
            AllowlistMatch::Refused { reason } => {
                assert!(reason.contains(needle), "{words:?}: {reason}")
            }
            other => panic!("{words:?} must be refused, got {other:?}"),
        }
    }

    #[test]
    fn the_everyday_commands_are_allowed() {
        allowed(&["cargo", "build"]);
        allowed(&["cargo", "check", "--workspace", "--all-targets"]);
        allowed(&["cargo", "test", "-p", "kgraf-tools", "--lib"]);
        allowed(&["cargo", "test", "--package=kgraf-tools", "some_test"]);
        allowed(&["cargo", "test", "-pkgraf-tools", "-j2", "--", "--nocapture"]);
        allowed(&[
            "cargo",
            "test",
            "--",
            "--test-threads",
            "1",
            "name",
            "--exact",
        ]);
        allowed(&[
            "cargo",
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ]);
        allowed(&[
            "cargo",
            "clippy",
            "--",
            "-Dwarnings",
            "-A",
            "clippy::too_many_lines",
        ]);
        allowed(&["cargo", "fmt", "--all", "--check"]);
        allowed(&["cargo", "fmt", "--all", "--", "--check"]);
        allowed(&["cargo", "fmt", "--check", "--", "--check"]);
        allowed(&["cargo", "build", "--manifest-path", "crates/x/Cargo.toml"]);
        allowed(&["cargo", "build", "--target", "aarch64-apple-darwin"]);
        allowed(&["cargo", "test", "--features", "serde/derive", "--release"]);
    }

    /// The flag set a worker reaches for when it runs a scaffold's own
    /// acceptance commands: package and target selection, features, profile,
    /// verbosity, `--no-run`, the offline and lockfile flags, and test-binary
    /// arguments after `--`.
    #[test]
    fn the_acceptance_command_flag_set_is_allowed() {
        allowed(&["cargo", "test", "--workspace"]);
        allowed(&["cargo", "test", "-p", "tax_us"]);
        allowed(&["cargo", "test", "--test", "pricing", "--", "--nocapture"]);
        allowed(&["cargo", "test", "--workspace", "--", "--test-threads=1"]);
        allowed(&["cargo", "test", "--workspace", "--", "--test-threads", "1"]);
        allowed(&["cargo", "test", "--no-run", "-v", "--locked"]);
        allowed(&[
            "cargo",
            "test",
            "-q",
            "--offline",
            "--frozen",
            "name_filter",
        ]);
        allowed(&[
            "cargo",
            "build",
            "--workspace",
            "--release",
            "--all-targets",
        ]);
        allowed(&[
            "cargo",
            "build",
            "-p",
            "iface",
            "-F",
            "serde",
            "--profile",
            "dev",
        ]);
        allowed(&["cargo", "check", "--workspace", "--all-features", "--quiet"]);
        allowed(&["cargo", "check", "--no-default-features", "--features=a,b"]);
        allowed(&["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]);
        allowed(&[
            "cargo",
            "test",
            "--doc",
            "--no-fail-fast",
            "--",
            "--exact",
            "x",
        ]);
    }

    /// The description names the options, so a model does not read the bare
    /// subcommand names as "without flags".
    #[test]
    fn the_description_names_the_accepted_options() {
        let text = BuildAllowlist::cargo_default().describe();
        for needle in ["--workspace", "-p", "--test", "--release", "--no-run", "--"] {
            assert!(text.contains(needle), "{needle}: {text}");
        }
        assert!(text.contains("one array element per argument"), "{text}");
    }

    /// A whole command line given as one string is split into its words when
    /// it holds no shell syntax; with shell syntax it is not split.
    #[test]
    fn a_command_line_in_one_string_splits_only_without_shell_syntax() {
        assert_eq!(
            split_command_line("cargo test --workspace"),
            Some(argv(&["cargo", "test", "--workspace"]))
        );
        assert_eq!(
            split_command_line("  cargo\ttest  -p x "),
            Some(argv(&["cargo", "test", "-p", "x"]))
        );
        assert_eq!(split_command_line("cargo"), None);
        assert_eq!(split_command_line("cargo test && rm -rf ."), None);
        assert_eq!(split_command_line("cargo test $(id)"), None);
        assert_eq!(split_command_line("echo 'a b'"), None);
    }

    #[test]
    fn argument_smuggling_is_refused_with_the_argument_named() {
        refused(
            &["cargo", "test", "--manifest-path", "/elsewhere/Cargo.toml"],
            "relative",
        );
        refused(
            &["cargo", "test", "--manifest-path=../other/Cargo.toml"],
            "no `..`",
        );
        refused(
            &["cargo", "build", "--manifest-path", "crates/x/other.toml"],
            "Cargo.toml",
        );
        refused(
            &["cargo", "build", "--config", "build.rustc-wrapper='x'"],
            "--config",
        );
        refused(
            &["cargo", "build", "--config=target.x.runner='x'"],
            "--config",
        );
        refused(&["cargo", "build", "-Z", "unstable-options"], "-Z");
        refused(&["cargo", "build", "-Zbuild-std"], "-Zbuild-std");
        refused(
            &["cargo", "build", "--target-dir", "/tmp/t"],
            "--target-dir",
        );
        refused(
            &["cargo", "build", "--artifact-dir", "out"],
            "--artifact-dir",
        );
        refused(&["cargo", "build", "-C", "/elsewhere"], "-C");
        refused(
            &["cargo", "build", "--lockfile-path", "x/Cargo.lock"],
            "--lockfile-path",
        );
        refused(
            &["cargo", "build", "--target", "../evil.json"],
            "target triple",
        );
        refused(
            &["cargo", "build", "--target", "spec.json"],
            "target triple",
        );
        refused(&["cargo", "clippy", "--fix"], "--fix");
        refused(&["cargo", "clippy", "--", "-C", "linker=/tmp/x"], "-C");
        refused(&["cargo", "test", "--", "--logfile", "x"], "--logfile");
        refused(&["cargo", "test", "--", "-Z", "unstable-options"], "-Z");
        refused(&["cargo", "fmt"], "--check");
        refused(&["cargo", "fmt", "--all"], "--check");
        refused(&["cargo", "fmt", "--all", "--"], "--check");
        refused(
            &["cargo", "fmt", "--", "--check", "--emit", "files"],
            "after `--`",
        );
        refused(&["cargo", "fmt", "--", "--check=false"], "--check");
        refused(
            &["cargo", "fmt", "--check", "--", "--emit", "files"],
            "after `--`",
        );
        refused(&["cargo", "build", "positional"], "positional");
        refused(&["cargo", "test", "one", "two"], "two");
        refused(&["cargo", "build", "--jobs", "many"], "number");
        refused(&["cargo", "build", "--color", "rainbow"], "auto");
        refused(&["cargo", "build", "-p"], "needs a value");
        refused(&["cargo", "build", "--release=yes"], "takes no value");
    }

    #[test]
    fn commands_off_the_list_are_not_listed_and_decided_as_before() {
        for words in [
            vec!["cargo", "run"],
            vec!["cargo", "install", "ripgrep"],
            vec!["cargo", "+nightly", "build"],
            vec!["cargo", "--config", "x", "build"],
            vec!["./cargo", "build"],
            vec!["/tmp/cargo", "build"],
            vec!["cargo"],
            vec!["make", "test"],
        ] {
            assert_eq!(check(&words), AllowlistMatch::NotListed, "{words:?}");
        }
    }

    #[test]
    fn an_empty_allowlist_lists_nothing() {
        assert_eq!(
            BuildAllowlist::empty().check(&argv(&["cargo", "build"])),
            AllowlistMatch::NotListed
        );
    }

    #[test]
    fn a_configured_command_matches_exactly_and_nothing_near_it() {
        let list = BuildAllowlist::empty()
            .with_configured("make test")
            .with_configured("cargo test --workspace && ./scripts/lint.sh");
        assert_eq!(
            list.check(&argv(&["make", "test"])),
            AllowlistMatch::Allowed
        );
        assert_eq!(
            list.check(&argv(&["sh", "-c", "make test"])),
            AllowlistMatch::Allowed
        );
        assert_eq!(
            list.check(&argv(&[
                "sh",
                "-c",
                "cargo test --workspace && ./scripts/lint.sh"
            ])),
            AllowlistMatch::Allowed
        );
        for near in [
            vec!["make", "test", "extra"],
            vec!["make"],
            vec!["make", "install"],
            vec!["sh", "-c", "make test; rm -rf ."],
            vec!["cargo", "test", "--workspace", "&&", "./scripts/lint.sh"],
        ] {
            assert_eq!(
                list.check(&argv(&near)),
                AllowlistMatch::NotListed,
                "{near:?}"
            );
        }
    }

    #[test]
    fn configuration_spellings_parse_and_unknown_ones_are_refused() {
        assert_eq!(
            BuildAllowlist::parse_cargo_entry("cargo  fmt --check"),
            Ok(BuildCommand::Cargo(CargoSubcommand::FmtCheck))
        );
        assert_eq!(
            BuildAllowlist::parse_cargo_entry("cargo test"),
            Ok(BuildCommand::Cargo(CargoSubcommand::Test))
        );
        for bad in ["cargo run", "cargo fmt", "cargo test --workspace", "make"] {
            assert!(BuildAllowlist::parse_cargo_entry(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn every_manifest_path_is_found_for_the_on_disk_check() {
        assert_eq!(
            manifest_paths_of(&argv(&[
                "cargo",
                "build",
                "--manifest-path",
                "a/Cargo.toml"
            ])),
            ["a/Cargo.toml"]
        );
        assert_eq!(
            manifest_paths_of(&argv(&["cargo", "build", "--manifest-path=b/Cargo.toml"])),
            ["b/Cargo.toml"]
        );
        assert_eq!(
            manifest_paths_of(&argv(&[
                "cargo",
                "build",
                "--manifest-path=a/Cargo.toml",
                "--manifest-path",
                "b/Cargo.toml"
            ])),
            ["a/Cargo.toml", "b/Cargo.toml"]
        );
        assert!(
            manifest_paths_of(&argv(&["cargo", "test", "--", "--manifest-path", "x"])).is_empty()
        );
    }
}
