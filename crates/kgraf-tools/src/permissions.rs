//! Permission modes, the user's rules, and the limits no mode lifts
//! (decision 0094).
//!
//! **One setting, the user's.** [`PermissionMode`] decides who answers a
//! permission request past the base policy: the user (`Manual`), nobody for
//! file edits (`AcceptEdits`), nobody inside the user's allow set (`Auto`),
//! the orchestrator (`OrchestratorDecides`), or nobody at all (`Bypass`).
//! [`PermissionControl`] holds the live setting. It changes only through
//! [`PermissionControl::set_mode`], which takes a [`ModeSource`]: the
//! desktop control, the user-level file, or the command line. There is no
//! source for a project file, a model, a skill or a tool's output, and the
//! project files (`run.json`, `worker.json`) refuse the keys.
//!
//! **Changes apply at once.** Every change bumps a `tokio::sync::watch`
//! generation. A waiting request (an approval ask in the box, a held
//! shortfall) re-checks itself against the new mode when the generation
//! moves, and a running worker's shell reads the mode on every call
//! ([`crate::execpolicy::ExecPolicy::with_permissions`]).
//!
//! **Limits no mode lifts.** Mirrored from Claude Code's permission modes
//! (its documentation, quoted in decision 0094):
//! - the user's deny rules block in every mode, bypass included;
//! - the user's ask rules ask in every mode;
//! - deleting a system path (the filesystem root, a top-level directory, the
//!   home directory) is refused in every mode; deleting another critical path
//!   (the workspace itself, its `.git`, or a path that climbs out of it) asks;
//! - hooks cannot override deny or ask: [`PermissionRulesHook`] runs last in
//!   the chain, on the input every earlier hook left, and its asks are never
//!   covered by a persisted allow rule or granted by a mode;
//! - a run nobody attends denies what would still ask;
//! - `disable_bypass` in the user-level file refuses the mode from every
//!   source; running as root refuses it unless the process was started inside
//!   a container that says so (`KGRAF_CONTAINER_SANDBOX=1`);
//! - the desktop offers the mode only after the user turned on
//!   `allow_bypass`, and its first use needs the warning accepted.
//!
//! Written new: the mode names and the limits follow Claude Code's
//! documentation (no code of it is available to reuse); the rule syntax
//! `tool(pattern)` follows its `Bash(npm run test:*)` shape. Command lines
//! are split with `shlex` (MIT OR Apache-2.0), already in the lockfile.

use crate::approval::{ApprovalHook, HookDecision, PendingCall};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

#[path = "permission_names.rs"]
pub mod permission_names;

// What programs run from their arguments and from the worktree's files
// (decision 0094, amended 2026-10-06), one reader per family.
#[path = "permission_code.rs"]
mod code;
#[path = "permission_git.rs"]
mod git;
#[path = "permission_readers.rs"]
mod readers;
#[path = "permission_recipes.rs"]
mod recipes;
#[path = "permission_windows.rs"]
mod windows;
pub use permission_names::{
    BUILTIN_TOOLS, CLAUDE_CODE_ALIASES, CLAUDE_CODE_WITHOUT_COUNTERPART, claude_code_alias,
    counterpart_notice, tool_hint, unmatched_entries, without_counterpart,
};

/// The name of the hook that enforces the user's rules and the critical-path
/// limit. An ask from it is a hard ask: no persisted allow rule covers it and
/// no mode grants it; a run nobody attends denies it.
pub const PERMISSION_RULES_HOOK: &str = "user-permission-rules";

/// The environment variable a container harness sets to say this process
/// runs inside a disposable container (decision 0093). Here it is also the
/// only statement that lets Bypass start as root.
pub const CONTAINER_STATEMENT_ENV: &str = "KGRAF_CONTAINER_SANDBOX";

/// The user-level permissions file's name, in kGraf's per-user configuration
/// directory beside the desktop's `settings.toml`.
pub const PERMISSIONS_FILE_NAME: &str = "permissions.toml";

/// Points the command line and the desktop at another user-level
/// permissions file (a test, or a user keeping several).
pub const PERMISSIONS_PATH_ENV: &str = "KGRAF_PERMISSIONS_PATH";

/// Shell programs Accept edits treats as file edits, as Claude Code's
/// `acceptEdits` does ("common filesystem commands (mkdir, touch, mv, cp,
/// ...)"). Deletion is not among them.
pub const FILE_EDIT_PROGRAMS: &[&str] = &["mkdir", "touch", "mv", "cp"];

/// Tools that edit files.
pub const FILE_EDIT_TOOLS: &[&str] = &["apply_patch"];

/// How long a permission request waits for others before its group is shown:
/// requests raised within this many milliseconds of a group's first request
/// join that group. A fixed window from the first request, so a steady stream
/// cannot keep one group open forever.
pub const GROUP_WINDOW_MS: i64 = 2_000;

/// Who answers a permission request past the base policy.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Every request goes to the user.
    #[default]
    Manual,
    /// File edits (a file tool's ask, a file-scope widening, the common
    /// filesystem programs) are granted; commands, network and MCP ask.
    AcceptEdits,
    /// What the user's allow set names is granted; the rest asks.
    Auto,
    /// Requests go to the orchestrator, which grants or refuses each and
    /// never asks the user.
    OrchestratorDecides,
    /// Nothing is refused and nothing asks, except the limits no mode lifts.
    Bypass,
}

impl PermissionMode {
    pub const ALL: [PermissionMode; 5] = [
        PermissionMode::Manual,
        PermissionMode::AcceptEdits,
        PermissionMode::Auto,
        PermissionMode::OrchestratorDecides,
        PermissionMode::Bypass,
    ];

    /// The key form: `manual`, `accept_edits`, `auto`,
    /// `orchestrator_decides`, `bypass`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::AcceptEdits => "accept_edits",
            Self::Auto => "auto",
            Self::OrchestratorDecides => "orchestrator_decides",
            Self::Bypass => "bypass",
        }
    }

    /// The words the dropdown shows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "Manual",
            Self::AcceptEdits => "Accept edits",
            Self::Auto => "Auto",
            Self::OrchestratorDecides => "Orchestrator decides",
            Self::Bypass => "Bypass",
        }
    }

    /// One line on what the mode does.
    pub fn summary(self) -> &'static str {
        match self {
            Self::Manual => "Every permission request asks you.",
            Self::AcceptEdits => "File edits are granted; commands, network and MCP tools ask you.",
            Self::Auto => "What your allow set names is granted; anything else asks you.",
            Self::OrchestratorDecides => {
                "The orchestrator grants or refuses each request against its contract; \
                 you are not asked."
            }
            Self::Bypass => {
                "Everything is granted without asking, except your deny and ask rules and \
                 deleting system paths. No protection against prompt injection."
            }
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PermissionMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase().replace('-', "_");
        Ok(match key.as_str() {
            "manual" => Self::Manual,
            "accept_edits" => Self::AcceptEdits,
            "auto" => Self::Auto,
            "orchestrator_decides" | "orchestrator" => Self::OrchestratorDecides,
            "bypass" => Self::Bypass,
            _ => {
                return Err(format!(
                    "`{s}` is not a permission mode: use manual, accept-edits, auto, \
                     orchestrator-decides or bypass"
                ));
            }
        })
    }
}

/// Where a mode change came from. Only the user's surfaces exist here: a
/// project file, a model, a skill or a tool's output has no variant, so no
/// code path can say one set the mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeSource {
    /// The dropdown near the orchestrator box.
    UserInterface,
    /// The user-level permissions file, outside every project.
    UserConfig,
    /// A command-line flag.
    CommandLine,
}

impl ModeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserInterface => "the permission control",
            Self::UserConfig => "the user-level permissions file",
            Self::CommandLine => "the command line",
        }
    }
}

/// One user rule: `tool` (every call of the tool) or `tool(pattern)`. A `*`
/// in the pattern stands for any text; a pattern with no `*` matches a
/// subject exactly; a trailing ` *` or `:*` that is the only wildcard also
/// matches the bare command (Claude Code's forms). A shell call's subjects
/// are its command line and every simple command of a `sh -c` script, each
/// also from every place its program may stand past prefix words
/// ([`subjects`]); a file tool's are the paths it writes or reads.
///
/// The tool must be one a worker can have ([`permission_names`]): a kGraf
/// tool, a Claude Code name read as one (`Bash(rm *)` is `shell(rm *)`), an
/// MCP name, or `*`. A Claude Code tool kGraf has no tool for (`WebFetch`)
/// loads but matches no call, and is said as a notice
/// ([`counterpart_notice`]). Anything else is refused when the rule is read,
/// since it would match nothing. [`Rule::text`] is the rule as the user wrote
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// The kGraf tool (or tool-name glob) the rule is on.
    tool: String,
    /// The pattern as kGraf matches it.
    pattern: Option<String>,
    /// The rule as written, for messages and the user file.
    written: String,
    /// The pattern as written, when reading it changed it (`git:*` read as
    /// `git *`, `./src/*` as `src/*`). It still matches, so no rule matches
    /// less than its text did before it was read this way.
    legacy: Option<String>,
    /// A Claude Code relative path rule: it matches the path at any depth
    /// (`Edit(.env)` matches `config/.env`).
    any_depth: bool,
}

impl Rule {
    pub fn parse(text: &str) -> Result<Self, String> {
        let t = text.trim();
        let (tool, pattern) = match t.split_once('(') {
            None => (t, None),
            Some((tool, rest)) => {
                let Some(p) = rest.strip_suffix(')') else {
                    return Err(format!("rule `{t}` opens a pattern and does not close it"));
                };
                (tool, Some(p.trim().to_owned()))
            }
        };
        let name = tool.trim();
        if name.is_empty() || name.chars().any(|c| c.is_whitespace()) {
            return Err(format!("rule `{t}` names no tool"));
        }
        if pattern.as_deref() == Some("") {
            return Err(format!("rule `{t}` has an empty pattern"));
        }
        let tool = permission_names::canonical_tool(name, t, permission_names::Role::Rule)
            .map_err(|e| format!("rule {e}"))?;
        let (canonical, any_depth) = match &pattern {
            None => (None, false),
            Some(p) => {
                let (c, deep) = permission_names::canonical_pattern(name, &tool, p, t)
                    .map_err(|e| format!("rule {e}"))?;
                (Some(c), deep)
            }
        };
        let legacy = pattern.filter(|p| canonical.as_ref() != Some(p));
        Ok(Self {
            tool,
            pattern: canonical,
            written: t.to_owned(),
            legacy,
            any_depth,
        })
    }

    pub fn text(&self) -> String {
        self.written.clone()
    }

    /// Whether the rule is on the tool with wire name `tool`, whatever its
    /// pattern.
    pub fn names_tool(&self, tool: &str) -> bool {
        permission_names::tool_name_matches(&self.tool, tool)
    }

    fn matches(&self, tool: &str, subjects: &[String]) -> bool {
        if !self.names_tool(tool) {
            return false;
        }
        match &self.pattern {
            None => true,
            Some(p) => subjects.iter().any(|s| self.matches_subject(p, s)),
        }
    }

    fn matches_subject(&self, pattern: &str, subject: &str) -> bool {
        let at = |s: &str| {
            permission_names::pattern_matches(pattern, s)
                || self
                    .legacy
                    .as_deref()
                    .is_some_and(|l| permission_names::glob(l, s))
        };
        at(subject)
            || (self.any_depth
                && subject
                    .match_indices('/')
                    .any(|(i, _)| at(&subject[i + 1..])))
    }
}

/// The user's deny and ask rules. Deny wins over ask.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserRules {
    pub deny: Vec<Rule>,
    pub ask: Vec<Rule>,
}

/// What Auto grants without asking.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowSet {
    /// Shell programs, each an exact program word. A command is granted only
    /// when every program it runs is listed ([`programs_to_grant`]): `env`
    /// listed grants `env curl` only with `curl` listed too.
    pub programs: BTreeSet<String>,
    /// Tools by wire name, or an MCP server's tools (`mcp__<server>`,
    /// `mcp__<server>__*`). Read through [`AllowSet::with_tools`].
    pub tools: BTreeSet<String>,
    /// Repository paths a contract's write scope may be widened to: exact,
    /// or a prefix when the entry ends in `*`.
    pub paths: Vec<String>,
}

impl AllowSet {
    /// Add programs, each a plain program name (no whitespace), checked by
    /// [`validate_entry`].
    pub fn with_programs<I, S>(mut self, programs: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for p in programs {
            self.add(validate_entry(EntryList::AllowPrograms, &p.into())?.entry);
        }
        Ok(self)
    }

    /// Add one entry [`validate_entry`] accepted. A rule is not an allow-set
    /// entry and is ignored here.
    fn add(&mut self, entry: Entry) {
        match entry {
            Entry::Program(p) => {
                self.programs.insert(p);
            }
            Entry::Tool(t) => {
                self.tools.insert(t);
            }
            Entry::Path(p) => self.paths.push(p),
            Entry::Deny(_) | Entry::Ask(_) => {}
        }
    }

    pub fn allows_program(&self, program: &str) -> bool {
        self.programs.contains(program)
    }

    /// Whether an entry covers the wire name `tool`: the tool itself, or an
    /// MCP server's tools (`mcp__<server>`, `mcp__<server>__*`).
    pub fn allows_tool(&self, tool: &str) -> bool {
        self.tools
            .iter()
            .any(|t| permission_names::tool_name_matches(t, tool))
    }

    /// Add tools, each a kGraf tool, a Claude Code name read as one (`Bash`
    /// is `shell`), a Claude Code tool kGraf has no tool for (kept, granting
    /// nothing), or an MCP name, checked by [`validate_entry`]. Any other name
    /// is refused: Auto would grant nothing by it.
    pub fn with_tools<I, S>(mut self, tools: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for t in tools {
            self.add(validate_entry(EntryList::AllowTools, t.as_ref())?.entry);
        }
        Ok(self)
    }

    pub fn allows_path(&self, path: &str) -> bool {
        self.paths.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => path.starts_with(prefix),
            None => path == p,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.programs.is_empty() && self.tools.is_empty() && self.paths.is_empty()
    }

    /// For a held reason and the trace.
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.extend(self.programs.iter().map(|p| format!("`{p}`")));
        parts.extend(self.tools.iter().map(|t| format!("tool `{t}`")));
        parts.extend(self.paths.iter().map(|p| format!("path `{p}`")));
        if parts.is_empty() {
            "nothing".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

/// A list of the user-level permissions file, and of the flags that add to
/// it: where one entry was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryList {
    /// `deny = [...]`, `--deny`.
    Deny,
    /// `ask = [...]`, `--ask`.
    Ask,
    /// `[allow] tools = [...]`.
    AllowTools,
    /// `[allow] programs = [...]`, `--allow`.
    AllowPrograms,
    /// `[allow] paths = [...]`, `--allow-path`.
    AllowPaths,
}

impl EntryList {
    /// Every list, in file order.
    pub const ALL: [EntryList; 5] = [
        Self::Deny,
        Self::Ask,
        Self::AllowTools,
        Self::AllowPrograms,
        Self::AllowPaths,
    ];

    /// The list's key in `permissions.toml`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
            Self::AllowTools => "allow.tools",
            Self::AllowPrograms => "allow.programs",
            Self::AllowPaths => "allow.paths",
        }
    }

    /// A deny or ask list: an entry refused here is a block or an ask the
    /// user intended and does not have.
    pub fn is_rule(self) -> bool {
        matches!(self, Self::Deny | Self::Ask)
    }
}

/// One entry [`validate_entry`] accepted, as kGraf reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Deny(Rule),
    Ask(Rule),
    /// An allow-set tool, by the name kGraf matches (`Bash` is `shell`).
    Tool(String),
    /// An allow-set program, an exact `argv[0]`.
    Program(String),
    /// An allow-set path, exact or a prefix ending in `*`.
    Path(String),
}

/// What [`validate_entry`] returns for an entry it accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidEntry {
    pub entry: Entry,
    /// Set when the entry loads but matches no call: it names a Claude Code
    /// tool kGraf has no tool for. The text says what kGraf uses instead and
    /// which kGraf rule gives the intended effect ([`counterpart_notice`]).
    pub notice: Option<String>,
}

/// Check one entry of the user's permission settings, as written in `list`.
///
/// This is the one check every reader uses: `permissions.toml` (strict on
/// the command line, entry by entry in the desktop), the `--deny`, `--ask`,
/// `--allow` and `--allow-path` flags, and an editor that checks an entry as
/// the user types it. `Ok` carries the entry as kGraf reads it and, for an
/// entry that loads but matches no call, the notice to show. `Err` is the
/// reason the entry is refused, in a sentence that names the entry and what a
/// valid one looks like.
///
/// - `Deny`, `Ask`: [`Rule::parse`]. A kGraf tool, a Claude Code name read as
///   one, an MCP name, a tool-name glob that can match some tool, or a Claude
///   Code tool kGraf has no tool for (loaded with a notice). Any other name,
///   and a pattern kGraf could never match, is refused.
/// - `AllowTools`: the same names, without wildcards except one MCP
///   server's tools (`mcp__<server>__*`).
/// - `AllowPrograms`: one program name with no whitespace or control
///   characters.
/// - `AllowPaths`: any text.
pub fn validate_entry(list: EntryList, text: &str) -> Result<ValidEntry, String> {
    let (entry, notice) = match list {
        EntryList::Deny | EntryList::Ask => {
            let rule = Rule::parse(text)?;
            let notice = counterpart_notice(list, &rule.text(), &rule.tool);
            let entry = if list == EntryList::Deny {
                Entry::Deny(rule)
            } else {
                Entry::Ask(rule)
            };
            (entry, notice)
        }
        EntryList::AllowTools => {
            let t = text.trim();
            let tool = permission_names::canonical_tool(t, t, permission_names::Role::Allow)
                .map_err(|e| format!("allow-set tool {e}"))?;
            let notice = counterpart_notice(list, t, &tool);
            (Entry::Tool(tool), notice)
        }
        EntryList::AllowPrograms => {
            if text.is_empty() || text.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!(
                    "`{}` is not a program name: an allow set lists programs, each an exact \
                     argv[0], with no spaces",
                    text.escape_debug()
                ));
            }
            (Entry::Program(text.to_owned()), None)
        }
        EntryList::AllowPaths => (Entry::Path(text.to_owned()), None),
    };
    Ok(ValidEntry { entry, notice })
}

/// An entry of `permissions.toml` that [`validate_entry`] refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusedLine {
    pub list: EntryList,
    /// The entry as written in the file.
    pub text: String,
    /// Why it was refused.
    pub reason: String,
}

impl RefusedLine {
    /// One sentence naming the entry, its list and the reason.
    pub fn sentence(&self) -> String {
        format!(
            "The {} entry \"{}\" is refused: {}.",
            self.list.key(),
            self.text.escape_debug(),
            self.reason.trim_end_matches('.')
        )
    }
}

/// What reading `permissions.toml` entry by entry found
/// ([`PermissionSettings::read_entries`]): the settings with every valid
/// entry, each refused entry with its reason, a notice for each entry that
/// loads but matches no call, and, when a deny or ask entry was refused, why
/// the mode must be Manual. The desktop runs on this and Settings shows it;
/// the command line refuses the file if any entry was refused.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsLoad {
    /// The file's settings without the refused entries. `mode` is the
    /// file's own; [`SettingsLoad::effective`] applies `forced_manual`.
    pub settings: PermissionSettings,
    pub refused: Vec<RefusedLine>,
    pub notices: Vec<String>,
    /// Set when a deny or ask entry was refused (a block or ask the user
    /// intended is missing), or the file could not be read at all: the mode
    /// is Manual, so every request asks, until the file is fixed.
    pub forced_manual: Option<String>,
    /// The file could not be read as settings at all
    /// ([`SettingsLoad::unreadable`]); nothing may be written over it.
    pub unreadable: bool,
}

impl SettingsLoad {
    /// The settings to run under: [`SettingsLoad::settings`], in Manual when
    /// [`SettingsLoad::forced_manual`] is set.
    pub fn effective(&self) -> PermissionSettings {
        let mut s = self.settings.clone();
        if self.forced_manual.is_some() {
            s.mode = PermissionMode::Manual;
        }
        s
    }

    /// The result for a file that could not be read as settings at all
    /// (`why`): Manual, no rules, and the reason.
    pub fn unreadable(why: &str) -> Self {
        Self {
            forced_manual: Some(format!(
                "Manual applies, with no rules, because the permission settings could not be \
                 read: {why}"
            )),
            unreadable: true,
            ..Self::default()
        }
    }

    /// The text to write to the user-level file for the live settings
    /// `live`: the refused entries are kept as written, so a change made in
    /// the desktop does not delete an entry the user must still fix; while
    /// Manual is forced, the file keeps the mode the user chose. Refused for
    /// a file that could not be read at all, so it is not written over.
    pub fn file_text(&self, live: &PermissionSettings) -> Result<String, String> {
        if self.unreadable {
            return Err(
                "permissions.toml could not be read, so kGraf does not write over it; correct \
                 the file first"
                    .to_owned(),
            );
        }
        let mut f = PermissionsFile::from_settings(live);
        if self.forced_manual.is_some() {
            f.mode = Some(self.settings.mode);
        }
        for line in &self.refused {
            let list = match line.list {
                EntryList::Deny => &mut f.deny,
                EntryList::Ask => &mut f.ask,
                EntryList::AllowTools => &mut f.allow.tools,
                EntryList::AllowPrograms => &mut f.allow.programs,
                EntryList::AllowPaths => &mut f.allow.paths,
            };
            if !list.contains(&line.text) {
                list.push(line.text.clone());
            }
        }
        toml::to_string(&f).map_err(|e| e.to_string())
    }

    /// One line for a log: the forced-Manual reason, each refused entry and
    /// each notice; `None` when there is nothing to say.
    pub fn summary(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        parts.extend(self.forced_manual.iter().cloned());
        parts.extend(self.refused.iter().map(RefusedLine::sentence));
        parts.extend(self.notices.iter().cloned());
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

/// A shell construct the strict script reader ([`script_commands`]) refuses
/// to judge, which the user may let a mode grant (decision 0094, amended
/// 2026-10-02). Each is off by default. With one on, a `SHELL -c` script that
/// uses it is granted, in a mode that grants programs, when a grant covers
/// every program kGraf can see in the script ([`script_commands_with`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptConstruct {
    /// `$VAR`, `${VAR}`, `$(...)`, `$((...))`, backquotes, `<(...)`.
    Variables,
    /// `( ... )`, `{ ...; }` and brace expansion (`{a,b}`).
    Subshells,
    /// `&` and `|&`.
    Background,
    /// `<`, `<<` heredocs, `<<<` here-strings and `<>`.
    InputRedirects,
    /// Backslash escapes, comments, `!`, `$'...'`, and commands named by a
    /// path, a quoted word, or after `VAR=value`.
    Escapes,
    /// Output redirects to any path, after a `cd` too.
    OutsideRedirects,
}

impl ScriptConstruct {
    pub const ALL: [ScriptConstruct; 6] = [
        ScriptConstruct::Variables,
        ScriptConstruct::Subshells,
        ScriptConstruct::Background,
        ScriptConstruct::InputRedirects,
        ScriptConstruct::Escapes,
        ScriptConstruct::OutsideRedirects,
    ];

    /// The key form in `permissions.toml` (on the command line, `-` may
    /// stand for `_`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Variables => "variables",
            Self::Subshells => "subshells",
            Self::Background => "background",
            Self::InputRedirects => "input_redirects",
            Self::Escapes => "escapes",
            Self::OutsideRedirects => "outside_redirects",
        }
    }

    /// The switch's name in the settings surface.
    pub fn label(self) -> &'static str {
        match self {
            Self::Variables => "Variables and command substitution: $VAR, $(...), backquotes",
            Self::Subshells => "Subshells and groups: ( ... ), { ...; }, {a,b}",
            Self::Background => "Background jobs: &",
            Self::InputRedirects => "Input redirects and heredocs: <, <<, <<<",
            Self::Escapes => {
                "Escapes, comments and other quoting: \\, #, !, and commands named by a path \
                 or after VAR=value"
            }
            Self::OutsideRedirects => "Output redirects outside the worker's files",
        }
    }

    /// One plain line under the switch: what kGraf checks when it is on.
    pub fn explanation(self) -> &'static str {
        match self {
            Self::Variables => {
                "kGraf cannot see what a variable holds or what a substitution prints. It checks \
                 the programs it can read, those inside $(...) and backquotes too; a command \
                 named by a variable, such as $CMD, is granted with no check of what it runs, \
                 and asks instead while you have a deny or ask rule on the shell."
            }
            Self::Subshells => {
                "The commands inside the parentheses or braces are checked like every other \
                 command."
            }
            Self::Background => {
                "A command started with & keeps running while the next one starts; each is \
                 checked like every other command."
            }
            Self::InputRedirects => {
                "A command may read any file or text the script gives it. A heredoc whose text \
                 runs a substitution is still asked about."
            }
            Self::Escapes => {
                "kGraf reads the escape or the quote and checks the program it names; a program \
                 named by a path is checked by that exact path, as your allow set lists it."
            }
            Self::OutsideRedirects => {
                "This only lets the mode grant the script. The sandbox still blocks writes \
                 outside the worktree and its scratch folder, and the work's changes are still \
                 checked against its declared files."
            }
        }
    }
}

impl std::fmt::Display for ScriptConstruct {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.key())
    }
}

impl std::str::FromStr for ScriptConstruct {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().replace('-', "_");
        Self::ALL
            .into_iter()
            .find(|c| c.key() == key)
            .ok_or_else(|| {
                format!(
                    "`{}` is not a script construct: use {}, or none",
                    s.escape_debug(),
                    Self::ALL.map(|c| c.key().replace('_', "-")).join(", ")
                )
            })
    }
}

/// The script constructs the user turned on (none by default).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScriptConstructs(pub BTreeSet<ScriptConstruct>);

impl ScriptConstructs {
    pub fn allows(&self, c: ScriptConstruct) -> bool {
        self.0.contains(&c)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Turn one construct on or off.
    pub fn set(&mut self, c: ScriptConstruct, on: bool) {
        if on {
            self.0.insert(c);
        } else {
            self.0.remove(&c);
        }
    }

    /// The keys, comma-separated, or `none`.
    pub fn describe(&self) -> String {
        if self.0.is_empty() {
            return "none".to_owned();
        }
        self.0
            .iter()
            .map(|c| c.key())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Parse a command-line list: names separated by commas, or `none`.
    pub fn parse_list(text: &str) -> Result<Self, String> {
        if text.trim() == "none" {
            return Ok(Self::default());
        }
        let set = text
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(str::parse)
            .collect::<Result<BTreeSet<_>, _>>()?;
        if set.is_empty() {
            return Err("name at least one script construct, or none".to_owned());
        }
        Ok(Self(set))
    }
}

/// What the user accepts by turning a script construct on, shown above the
/// switches and in the documentation.
pub const SCRIPT_CONSTRUCTS_RISK: &str = "These switches apply only where a mode grants \
programs: Auto with your allow set, Accept edits for file programs, and programs granted to one \
piece of work. With a switch on, a script that uses the construct is granted when every program \
kGraf can see in it is granted. kGraf cannot see what a variable or a substitution will run, so \
you accept that a program it cannot see runs unchecked, the refusal to delete system paths \
included. Your deny and ask rules still apply to every command kGraf can see, a deny or ask \
rule whose program is named anywhere in the script text still refuses or asks, and while you \
have a deny or ask rule on the shell, a command named by a variable asks. The sandbox does not \
change. Manual still asks, and Bypass already allows these scripts.";

/// Everything the user set: the mode, Auto's allow set, the rules, plan
/// review, and the bypass switches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionSettings {
    pub mode: PermissionMode,
    pub allow: AllowSet,
    pub rules: UserRules,
    /// Show the plan and wait for the user's yes before any worker starts.
    pub plan_review: bool,
    /// Refuse Bypass from every source.
    pub disable_bypass: bool,
    /// The desktop offers Bypass only with this on.
    pub allow_bypass: bool,
    /// The user accepted the one-time Bypass warning.
    pub bypass_warning_accepted: bool,
    /// Shell constructs a mode may grant inside a script (all off by
    /// default).
    pub script_constructs: ScriptConstructs,
}

/// The user-level file's form. Unknown keys are refused, so a misspelled
/// rule fails loud instead of protecting nothing.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PermissionsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<PermissionMode>,
    #[serde(default)]
    plan_review: bool,
    #[serde(default)]
    disable_bypass: bool,
    #[serde(default)]
    allow_bypass: bool,
    #[serde(default)]
    bypass_warning_accepted: bool,
    #[serde(default)]
    allow: AllowFile,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    deny: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ask: Vec<String>,
    #[serde(default, skip_serializing_if = "ScriptConstructs::is_empty")]
    script_constructs: ScriptConstructs,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AllowFile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    programs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<String>,
}

/// Why the user-level file was refused.
#[derive(Debug, thiserror::Error)]
pub enum SettingsFileError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
    #[error(
        "{path} is inside the project at {root}: permission settings come only from the \
         user-level file outside every project, which a worker cannot write"
    )]
    InsideProject { path: PathBuf, root: PathBuf },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl PermissionsFile {
    fn from_settings(s: &PermissionSettings) -> Self {
        Self {
            mode: Some(s.mode),
            plan_review: s.plan_review,
            disable_bypass: s.disable_bypass,
            allow_bypass: s.allow_bypass,
            bypass_warning_accepted: s.bypass_warning_accepted,
            allow: AllowFile {
                programs: s.allow.programs.iter().cloned().collect(),
                tools: s.allow.tools.iter().cloned().collect(),
                paths: s.allow.paths.clone(),
            },
            deny: s.rules.deny.iter().map(Rule::text).collect(),
            ask: s.rules.ask.iter().map(Rule::text).collect(),
            script_constructs: s.script_constructs.clone(),
        }
    }
}

impl PermissionSettings {
    /// Parse the user-level file's text, refusing it if any entry is refused
    /// (the command line's reading). The error names every refused entry, its
    /// reason, and how to fix it.
    pub fn from_toml_str(text: &str) -> Result<Self, String> {
        let load = Self::read_entries(text)?;
        if load.refused.is_empty() {
            return Ok(load.settings);
        }
        let lines: Vec<String> = load.refused.iter().map(RefusedLine::sentence).collect();
        Err(format!(
            "{} Correct or remove each refused entry in the file, then start again.",
            lines.join(" ")
        ))
    }

    /// Parse the user-level file's text entry by entry (the desktop's
    /// reading): every entry goes through [`validate_entry`]; a valid entry
    /// loads, a refused one is listed with its reason, and a refused deny or
    /// ask entry forces Manual. `Err` only when the text is not a permissions
    /// file at all (bad TOML, an unknown key, a value of the wrong type, an
    /// unknown mode or script construct).
    pub fn read_entries(text: &str) -> Result<SettingsLoad, String> {
        let f: PermissionsFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut settings = Self {
            mode: f.mode.unwrap_or_default(),
            plan_review: f.plan_review,
            disable_bypass: f.disable_bypass,
            allow_bypass: f.allow_bypass,
            bypass_warning_accepted: f.bypass_warning_accepted,
            script_constructs: f.script_constructs,
            ..Self::default()
        };
        let mut load = SettingsLoad::default();
        for (list, entries) in [
            (EntryList::Deny, &f.deny),
            (EntryList::Ask, &f.ask),
            (EntryList::AllowTools, &f.allow.tools),
            (EntryList::AllowPrograms, &f.allow.programs),
            (EntryList::AllowPaths, &f.allow.paths),
        ] {
            for text in entries {
                match validate_entry(list, text) {
                    Ok(valid) => {
                        load.notices.extend(valid.notice);
                        match valid.entry {
                            Entry::Deny(r) => settings.rules.deny.push(r),
                            Entry::Ask(r) => settings.rules.ask.push(r),
                            other => settings.allow.add(other),
                        }
                    }
                    Err(reason) => load.refused.push(RefusedLine {
                        list,
                        text: text.clone(),
                        reason,
                    }),
                }
            }
        }
        let missing: Vec<String> = load
            .refused
            .iter()
            .filter(|l| l.list.is_rule())
            .map(|l| format!("{} rule \"{}\"", l.list.key(), l.text.escape_debug()))
            .collect();
        if !missing.is_empty() {
            load.forced_manual = Some(format!(
                "Manual applies until you correct permissions.toml, because kGraf refused your {}. \
                 A block or an ask that you wrote is missing, so every request asks.",
                missing.join(", ")
            ));
        }
        load.settings = settings;
        Ok(load)
    }

    pub fn to_toml_string(&self) -> Result<String, String> {
        toml::to_string(&PermissionsFile::from_settings(self)).map_err(|e| e.to_string())
    }

    /// Load the user-level file at `path`. A missing file is the defaults
    /// (Manual, no rules). A file inside `project_root` is refused: a worker
    /// can write inside the project, and the mode is the user's alone. Any
    /// refused entry refuses the file ([`PermissionSettings::from_toml_str`]).
    pub fn load(path: &Path, project_root: Option<&Path>) -> Result<Self, SettingsFileError> {
        Self::read_file(path, project_root, Self::from_toml_str).map(Option::unwrap_or_default)
    }

    /// Load the user-level file at `path` entry by entry
    /// ([`PermissionSettings::read_entries`]). A missing file is the
    /// defaults. `Err` when the file cannot be read, lies inside
    /// `project_root`, or is not a permissions file at all.
    pub fn load_entries(
        path: &Path,
        project_root: Option<&Path>,
    ) -> Result<SettingsLoad, SettingsFileError> {
        Self::read_file(path, project_root, Self::read_entries).map(Option::unwrap_or_default)
    }

    /// `parse` over the file's text; `None` for a missing file.
    fn read_file<T>(
        path: &Path,
        project_root: Option<&Path>,
        parse: impl Fn(&str) -> Result<T, String>,
    ) -> Result<Option<T>, SettingsFileError> {
        refuse_inside_project(path, project_root)?;
        match std::fs::read_to_string(path) {
            Ok(text) => parse(&text)
                .map(Some)
                .map_err(|reason| SettingsFileError::Invalid {
                    path: path.to_owned(),
                    reason,
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(SettingsFileError::Read {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Write the user-level file atomically (a temporary file, then a rename).
    pub fn save(&self, path: &Path) -> Result<(), SettingsFileError> {
        let text = self
            .to_toml_string()
            .map_err(|reason| SettingsFileError::Invalid {
                path: path.to_owned(),
                reason,
            })?;
        write_settings_file(path, &text)
    }
}

/// Write `text` to the user-level file at `path` atomically (a temporary
/// file, then a rename).
pub fn write_settings_file(path: &Path, text: &str) -> Result<(), SettingsFileError> {
    let io = |source| SettingsFileError::Write {
        path: path.to_owned(),
        source,
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

fn refuse_inside_project(path: &Path, root: Option<&Path>) -> Result<(), SettingsFileError> {
    let Some(root) = root else { return Ok(()) };
    let root_c = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    // The file may not exist yet: resolve its nearest existing ancestor.
    let mut probe = abs.as_path();
    let mut tail = Vec::new();
    let resolved = loop {
        if let Ok(c) = probe.canonicalize() {
            let mut r = c;
            for t in tail.iter().rev() {
                r.push(t);
            }
            break r;
        }
        match (probe.parent(), probe.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_owned());
                probe = parent;
            }
            _ => break abs.clone(),
        }
    };
    if resolved.starts_with(&root_c) {
        return Err(SettingsFileError::InsideProject {
            path: path.to_owned(),
            root: root.to_owned(),
        });
    }
    Ok(())
}

/// Facts about this process that decide whether Bypass may start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessFacts {
    /// The effective user is root (sudo included).
    pub is_root: bool,
    /// `KGRAF_CONTAINER_SANDBOX=1`: the harness that made this process's
    /// container says so. A statement, not a detection (decision 0093).
    pub container_statement: bool,
}

impl ProcessFacts {
    pub fn detect() -> Self {
        Self {
            is_root: effective_root(),
            container_statement: std::env::var(CONTAINER_STATEMENT_ENV).as_deref() == Ok("1"),
        }
    }
}

#[cfg(unix)]
fn effective_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn effective_root() -> bool {
    false
}

/// Why a mode change was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModeError {
    #[error("Bypass is disabled: `disable_bypass = true` in the user-level permissions file")]
    BypassDisabled,
    #[error("Bypass is not offered here until you turn on Allow Bypass in the settings surface")]
    BypassNotAllowed,
    #[error("Bypass needs its warning accepted first")]
    BypassWarningNotAccepted,
    #[error(
        "Bypass is refused when kGraf runs as root, unless it runs inside a container that \
         says so ({CONTAINER_STATEMENT_ENV}=1)"
    )]
    RootOutsideContainer,
}

/// The warning shown the first time Bypass is chosen. Claude Code's own
/// words about its bypass mode are quoted: it "offers no protection against
/// prompt injection or unintended actions."
pub const BYPASS_WARNING: &str = "Bypass grants every permission request without asking: \
workers run any command and every orchestrator request is granted. Only your deny and ask \
rules and the refusal to delete system paths still apply. It offers no protection against \
prompt injection or unintended actions. Use it only where a mistake costs nothing, such as a \
disposable container.";

/// Something the control recorded: every mode change and every grant a mode
/// made, for the trace, the run record and the activity card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PermissionEvent {
    ModeChanged {
        from: PermissionMode,
        to: PermissionMode,
        source: ModeSource,
    },
    PlanReviewChanged {
        on: bool,
        source: ModeSource,
    },
    /// A request granted without the user: which worker, what (a program, a
    /// tool, a path), under which mode, and by whom (`mode` or
    /// `orchestrator`).
    Granted {
        worker: String,
        subject: String,
        mode: PermissionMode,
        by: String,
    },
    /// A worker's write to a protected path, allowed in Bypass only
    /// ([`crate::protected`]): which worker, the path as the worker named it,
    /// the mode, and what became of it: `kept` after its integrity check
    /// passed, or `restored: <check output>` when the check failed and the
    /// snapshot was put back. A Bypass shell command's outcome adds
    /// [`crate::protected::NOT_EXCLUSIVE`] and the other workers when
    /// commands running at the same time may have made the change; it is
    /// `restored by another worker's check: ...` when another command's
    /// check found damage this one may have caused, and `unchecked: ...`
    /// when the check could not run.
    ProtectedWrite {
        worker: String,
        path: String,
        mode: PermissionMode,
        outcome: String,
    },
    /// A request the mode (or the orchestrator under it) granted, and a later
    /// rule of the approval chain then refused: a user deny rule, a critical
    /// path limit, or a user's "no". The call did not run. This is the one
    /// event for the call, so the trace never says `granted` for a call that
    /// was refused.
    GrantRefused {
        worker: String,
        subject: String,
        mode: PermissionMode,
        /// Who granted: `mode` or `orchestrator`.
        by: String,
        /// The hook that refused, as the approval log names it.
        refused_by: String,
        reason: String,
    },
}

impl PermissionEvent {
    /// One line, for the trace and the activity card.
    pub fn line(&self) -> String {
        match self {
            Self::ModeChanged { from, to, source } => format!(
                "permission mode {} -> {} (set by {})",
                from.label(),
                to.label(),
                source.as_str()
            ),
            Self::PlanReviewChanged { on, source } => format!(
                "plan review {} (set by {})",
                if *on { "on" } else { "off" },
                source.as_str()
            ),
            Self::Granted {
                worker,
                subject,
                mode,
                by,
            } => format!("granted {subject} to {worker} ({}, by {by})", mode.label()),
            Self::GrantRefused {
                worker,
                subject,
                mode,
                by,
                refused_by,
                reason,
            } => format!(
                "refused {subject} to {worker} ({}: allowed by {by}, then refused by \
                 {refused_by}: {reason})",
                mode.label()
            ),
            Self::ProtectedWrite {
                worker,
                path,
                mode,
                outcome,
            } => format!(
                "protected write to {path} by {worker} ({}): {outcome}",
                mode.label()
            ),
        }
    }
}

/// How an approval ask is answered under the current mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskRoute {
    /// Granted without the user; `by` is `mode` or `orchestrator`.
    Grant { by: &'static str },
    /// The user answers (or, with nobody there, the ask is denied).
    User,
}

/// Events kept for late readers. A reader that falls further behind loses
/// the oldest, which it is told by a gap in the sequence numbers.
const MAX_EVENTS: usize = 1_000;

struct ControlState {
    settings: PermissionSettings,
    events: VecDeque<(u64, PermissionEvent)>,
    next_seq: u64,
    /// (worker, subject, mode) already recorded as granted: a grant is the
    /// permission, not each use of it.
    granted: BTreeSet<(String, String, PermissionMode)>,
    /// Grants a mode made for a call still in the approval chain, by
    /// (worker, call id): (subject, by). A later hook may still refuse the
    /// call, so a grant is recorded only when the chain settles it
    /// ([`PermissionControl::settle_call`]).
    staged: BTreeMap<(String, String), Vec<(String, String)>>,
    /// (worker, subject, refusing hook, reason) already recorded as refused.
    refused: BTreeSet<(String, String, String, String)>,
}

/// The live permission setting of this process. Shared by the orchestrator,
/// every worker's shell and ask resolver, and the desktop control.
pub struct PermissionControl {
    state: Mutex<ControlState>,
    generation: tokio::sync::watch::Sender<u64>,
    facts: ProcessFacts,
}

impl std::fmt::Debug for PermissionControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionControl")
            .field("mode", &self.mode())
            .finish_non_exhaustive()
    }
}

impl PermissionControl {
    /// A control holding `settings`, whose mode came from `source`. A Bypass
    /// that `source` may not select is refused here, so a run never starts in
    /// a mode it could not have been switched to.
    pub fn new(
        settings: PermissionSettings,
        source: ModeSource,
        facts: ProcessFacts,
    ) -> Result<Self, ModeError> {
        if settings.mode == PermissionMode::Bypass {
            bypass_allowed(&settings, source, facts)?;
        }
        let (generation, _) = tokio::sync::watch::channel(0);
        Ok(Self {
            state: Mutex::new(ControlState {
                settings,
                events: VecDeque::new(),
                next_seq: 1,
                granted: BTreeSet::new(),
                staged: BTreeMap::new(),
                refused: BTreeSet::new(),
            }),
            generation,
            facts,
        })
    }

    /// Manual, no rules, no allow set: the default for a run given no control.
    pub fn manual() -> Self {
        Self::new(
            PermissionSettings::default(),
            ModeSource::UserConfig,
            ProcessFacts::default(),
        )
        .unwrap_or_else(|_| unreachable!("Manual is always allowed"))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ControlState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn mode(&self) -> PermissionMode {
        self.state().settings.mode
    }

    pub fn settings(&self) -> PermissionSettings {
        self.state().settings.clone()
    }

    pub fn facts(&self) -> ProcessFacts {
        self.facts
    }

    pub fn plan_review(&self) -> bool {
        self.state().settings.plan_review
    }

    /// Wakes on every change of mode, rules, allow set or plan review.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.generation.subscribe()
    }

    fn bump(&self) {
        self.generation.send_modify(|g| *g += 1);
    }

    fn push(state: &mut ControlState, event: PermissionEvent) {
        let seq = state.next_seq;
        state.next_seq += 1;
        state.events.push_back((seq, event));
        while state.events.len() > MAX_EVENTS {
            state.events.pop_front();
        }
    }

    /// Change the mode. Refused for Bypass when the user disabled it, when
    /// the desktop's Allow Bypass is off or its warning not accepted (for the
    /// desktop control), or as root outside a container that says so.
    pub fn set_mode(
        &self,
        mode: PermissionMode,
        source: ModeSource,
    ) -> Result<PermissionEvent, ModeError> {
        let event = {
            let mut s = self.state();
            if mode == PermissionMode::Bypass {
                bypass_allowed(&s.settings, source, self.facts)?;
            }
            let from = s.settings.mode;
            s.settings.mode = mode;
            let event = PermissionEvent::ModeChanged {
                from,
                to: mode,
                source,
            };
            Self::push(&mut s, event.clone());
            event
        };
        self.bump();
        Ok(event)
    }

    pub fn set_plan_review(&self, on: bool, source: ModeSource) -> PermissionEvent {
        let event = {
            let mut s = self.state();
            s.settings.plan_review = on;
            let event = PermissionEvent::PlanReviewChanged { on, source };
            Self::push(&mut s, event.clone());
            event
        };
        self.bump();
        event
    }

    /// The desktop's Allow Bypass switch. Turning it off while in Bypass
    /// returns the mode to Manual at once.
    pub fn set_allow_bypass(&self, on: bool) {
        let fell_back = {
            let mut s = self.state();
            s.settings.allow_bypass = on;
            if !on && s.settings.mode == PermissionMode::Bypass {
                s.settings.mode = PermissionMode::Manual;
                Self::push(
                    &mut s,
                    PermissionEvent::ModeChanged {
                        from: PermissionMode::Bypass,
                        to: PermissionMode::Manual,
                        source: ModeSource::UserInterface,
                    },
                );
                true
            } else {
                false
            }
        };
        let _ = fell_back;
        self.bump();
    }

    /// The user accepted the one-time Bypass warning.
    pub fn accept_bypass_warning(&self) {
        self.state().settings.bypass_warning_accepted = true;
        self.bump();
    }

    /// The shell constructs a mode may grant inside a script.
    pub fn script_constructs(&self) -> ScriptConstructs {
        self.state().settings.script_constructs.clone()
    }

    /// Turn one script construct on or off (the user's switch). Applies to
    /// the next call judged, a running worker's included.
    pub fn set_script_construct(&self, construct: ScriptConstruct, on: bool) {
        self.state().settings.script_constructs.set(construct, on);
        self.bump();
    }

    /// Replace Auto's allow set (the user edited it).
    pub fn set_allow(&self, allow: AllowSet) {
        self.state().settings.allow = allow;
        self.bump();
    }

    pub fn rules(&self) -> UserRules {
        self.state().settings.rules.clone()
    }

    /// Replace the deny and ask rules (the user edited them). Applies to the
    /// next call judged, a running worker's included.
    pub fn set_rules(&self, rules: UserRules) {
        self.state().settings.rules = rules;
        self.bump();
    }

    /// Hold a grant the mode made for call `call_id` of `worker` until the
    /// approval chain settles the call ([`Self::settle_call`]): a hook later
    /// in the chain can still refuse it.
    pub fn stage_grant(&self, worker: &str, call_id: &str, subject: &str, by: &str) {
        self.state()
            .staged
            .entry((worker.to_owned(), call_id.to_owned()))
            .or_default()
            .push((subject.to_owned(), by.to_owned()));
    }

    /// The approval chain reached its end for call `call_id`: `refusal` is
    /// `None` when the call runs, or the refusing hook and its reason. The
    /// call's staged grants are recorded as granted when it runs, and as one
    /// refused event each (once per worker, subject, hook and reason) when it
    /// does not. Returns the events recorded.
    pub fn settle_call(
        &self,
        worker: &str,
        call_id: &str,
        refusal: Option<(&str, &str)>,
    ) -> Vec<PermissionEvent> {
        self.settle_call_naming(worker, call_id, refusal, None)
    }

    /// [`Self::settle_call`], where a refused event names `refused_program`
    /// (the program of the command a rule matched, [`refused_subject`]) as its
    /// subject instead of the grant's subject: the call's own program may be a
    /// shell that refused nothing. `None` keeps the staged subject, as for a
    /// refusal of the whole tool.
    pub fn settle_call_naming(
        &self,
        worker: &str,
        call_id: &str,
        refusal: Option<(&str, &str)>,
        refused_program: Option<&str>,
    ) -> Vec<PermissionEvent> {
        let staged = self
            .state()
            .staged
            .remove(&(worker.to_owned(), call_id.to_owned()))
            .unwrap_or_default();
        let mut events = Vec::new();
        for (subject, by) in staged {
            match refusal {
                None => events.extend(self.record_grant(worker, &subject, &by)),
                Some((refused_by, reason)) => {
                    let subject = refused_program.map_or(subject, |p| format!("`{p}`"));
                    let mut s = self.state();
                    let mode = s.settings.mode;
                    if !s.refused.insert((
                        worker.to_owned(),
                        subject.clone(),
                        refused_by.to_owned(),
                        reason.to_owned(),
                    )) {
                        continue;
                    }
                    let event = PermissionEvent::GrantRefused {
                        worker: worker.to_owned(),
                        subject,
                        mode,
                        by,
                        refused_by: refused_by.to_owned(),
                        reason: reason.to_owned(),
                    };
                    Self::push(&mut s, event.clone());
                    events.push(event);
                }
            }
        }
        if !events.is_empty() {
            self.bump();
        }
        events
    }

    /// Record a grant a mode (or the orchestrator under its mode) made. The
    /// first grant of one subject to one worker under one mode is recorded;
    /// later uses of it are not new grants.
    pub fn record_grant(&self, worker: &str, subject: &str, by: &str) -> Option<PermissionEvent> {
        let mut s = self.state();
        let mode = s.settings.mode;
        if !s
            .granted
            .insert((worker.to_owned(), subject.to_owned(), mode))
        {
            return None;
        }
        let event = PermissionEvent::Granted {
            worker: worker.to_owned(),
            subject: subject.to_owned(),
            mode,
            by: by.to_owned(),
        };
        Self::push(&mut s, event.clone());
        Some(event)
    }

    /// Record a worker's write to a protected path and what became of it
    /// (decision 0094). Every write is recorded, not once per worker: each is
    /// a change to the repository's or kGraf's own files.
    pub fn record_protected_write(
        &self,
        worker: &str,
        path: &str,
        outcome: &str,
    ) -> PermissionEvent {
        let mut s = self.state();
        let event = PermissionEvent::ProtectedWrite {
            worker: worker.to_owned(),
            path: path.to_owned(),
            mode: s.settings.mode,
            outcome: outcome.to_owned(),
        };
        Self::push(&mut s, event.clone());
        event
    }

    /// Events after sequence number `after`, oldest first, with their
    /// numbers. Each reader keeps its own cursor.
    pub fn events_since(&self, after: u64) -> Vec<(u64, PermissionEvent)> {
        self.state()
            .events
            .iter()
            .filter(|(seq, _)| *seq > after)
            .cloned()
            .collect()
    }

    /// The newest event's sequence number (0 before any).
    pub fn last_seq(&self) -> u64 {
        self.state().next_seq - 1
    }

    /// Whether the shell runs every command no `forbid` rule refuses without
    /// approval, confined like a build: in Bypass only.
    pub fn shell_unrestricted(&self) -> bool {
        self.mode() == PermissionMode::Bypass
    }

    /// Whether the current mode grants the one program `program`: Auto's
    /// allow set, or Accept edits' file programs. A command runs on grants
    /// only when every program it runs is granted ([`programs_to_grant`]);
    /// a grant of `env` or `sh` grants nothing they run.
    pub fn policy_grants_program(&self, program: &str) -> Option<&'static str> {
        let s = self.state();
        match s.settings.mode {
            PermissionMode::Auto if s.settings.allow.allows_program(program) => {
                Some("in the Auto permission mode's allow set")
            }
            PermissionMode::AcceptEdits if FILE_EDIT_PROGRAMS.contains(&base_name(program)) => {
                Some("a file edit under the Accept edits permission mode")
            }
            _ => None,
        }
    }

    /// Whether the current mode grants some named programs on its own: Auto
    /// with programs in its allow set, or Accept edits' file programs. A
    /// shell script is judged command by command
    /// ([`crate::execpolicy::ExecPolicy::evaluate`]) when this holds, when a
    /// program is granted to the contract, or when the policy has a build and
    /// test allowlist.
    pub fn grants_programs(&self) -> bool {
        let s = self.state();
        match s.settings.mode {
            PermissionMode::Auto => !s.settings.allow.programs.is_empty(),
            PermissionMode::AcceptEdits => true,
            _ => false,
        }
    }

    /// How an approval ask on `tool` with `input` is answered under the
    /// current mode. Hard asks (from [`PERMISSION_RULES_HOOK`]) never come
    /// here.
    pub fn route_ask(&self, tool: &str, input: &serde_json::Value) -> AskRoute {
        let s = self.state();
        match s.settings.mode {
            PermissionMode::Bypass => AskRoute::Grant { by: "mode" },
            PermissionMode::OrchestratorDecides => AskRoute::Grant { by: "orchestrator" },
            PermissionMode::Auto => {
                let allow = &s.settings.allow;
                // Every program the command runs needs its own entry: an
                // entry for `env` or `sh` grants nothing they run.
                let granted = allow.allows_tool(tool)
                    || (tool == "shell"
                        && programs_to_grant(&shell_argv(input)).is_ok_and(|ps| {
                            !ps.is_empty() && ps.iter().all(|p| allow.allows_program(p))
                        }))
                    || (FILE_EDIT_TOOLS.contains(&tool) && {
                        let paths = file_paths(tool, input);
                        !paths.is_empty() && paths.iter().all(|p| allow.allows_path(p))
                    });
                if granted {
                    AskRoute::Grant { by: "mode" }
                } else {
                    AskRoute::User
                }
            }
            PermissionMode::AcceptEdits => {
                let edit = FILE_EDIT_TOOLS.contains(&tool)
                    || (tool == "shell"
                        && programs_to_grant(&shell_argv(input)).is_ok_and(|ps| {
                            !ps.is_empty()
                                && ps
                                    .iter()
                                    .all(|p| FILE_EDIT_PROGRAMS.contains(&base_name(p)))
                        }));
                if edit {
                    AskRoute::Grant { by: "mode" }
                } else {
                    AskRoute::User
                }
            }
            PermissionMode::Manual => AskRoute::User,
        }
    }
}

/// Whether `source` may select Bypass under `settings` in this process.
pub fn bypass_allowed(
    settings: &PermissionSettings,
    source: ModeSource,
    facts: ProcessFacts,
) -> Result<(), ModeError> {
    if settings.disable_bypass {
        return Err(ModeError::BypassDisabled);
    }
    if facts.is_root && !facts.container_statement {
        return Err(ModeError::RootOutsideContainer);
    }
    if source == ModeSource::UserInterface {
        if !settings.allow_bypass {
            return Err(ModeError::BypassNotAllowed);
        }
        if !settings.bypass_warning_accepted {
            return Err(ModeError::BypassWarningNotAccepted);
        }
    }
    Ok(())
}

fn base_name(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// A shell call's argv as the shell tool runs it (`crate::command_argv`: a
/// one-string command line is split into its words), so a mode, a rule or the
/// deletion check judges the argv that runs. Input the tool refuses gives its
/// raw strings, for a rule's message; it never runs. Empty when there are none.
fn shell_argv(input: &serde_json::Value) -> Vec<String> {
    crate::command_argv(input).unwrap_or_else(|| {
        input
            .get("command")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// The paths a file tool's call writes: `apply_patch`'s headers, or a
/// `path` / `file_path` field.
fn file_paths(tool: &str, input: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    if tool == "apply_patch" {
        let patch = input
            .get("patch")
            .or_else(|| input.get("input"))
            .and_then(|p| p.as_str())
            .unwrap_or_default();
        for line in patch.lines() {
            for head in [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ] {
                if let Some(p) = line.strip_prefix(head) {
                    out.push(p.trim().to_owned());
                }
            }
        }
    }
    for key in ["path", "file_path"] {
        if let Some(p) = input.get(key).and_then(|p| p.as_str()) {
            out.push(p.to_owned());
        }
    }
    out
}

/// Shell interpreters whose `-c` script is read as commands.
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "fish", "ash", "mksh", "lksh", "oksh", "pdksh", "posh",
    "yash", "rbash", "bosh",
];

/// How deep [`simple_commands`] follows a command started by another
/// (`sh -c "bash -c 'curl x'"`, `find . -exec sh -c 'curl x' \;`). Each
/// level is a strict part of the level above, so real commands stop long
/// before this; the bound only keeps recursion finite.
const MAX_SHELL_DEPTH: usize = 32;

/// Every simple command a shell call runs, as argv lists: the call's argv
/// itself and every command it starts ([`Embedded`]): for `sh -c SCRIPT` (or
/// `-lc`, `-ec`) and `eval`, each command of the script split on `;`, `&&`,
/// `||`, `|`, `&` and newlines; the argv a program runs from its arguments
/// (`find -exec PROG ... ;`, `script FILE PROG`, `uv run PROG`, `npx PKG`,
/// `docker exec CONTAINER PROG`); the script text a program hands to a shell
/// (`su -c`, `ssh HOST CMD`, `script -c`, `trap CMD SIG`, `watch CMD`,
/// `flock F -c`, `env -S`, `parallel CMD :::`, `cmd /c`, `powershell
/// -Command`, a git configuration value that runs a command); and, with a
/// [`ReadContext`], what a file in the worktree makes a program run (a
/// Makefile recipe, a `package.json` script, a justfile recipe, a cargo
/// alias, the calls of interpreter code that start programs). The starting
/// program may stand after prefix words ([`command_starts`]: `env sh -c
/// ...`, `nohup bash -c ...`), and each command found is read the same way,
/// its commands listed after it. A script that cannot be split (unbalanced
/// quotes) yields its words split on whitespace, so a rule still sees them.
/// What a call starts that kGraf cannot read (`source FILE`, `$CMD`) is
/// listed by [`unreadable_commands`].
///
/// With no [`ReadContext`], no file is read: every command that runs what a
/// file says is unreadable.
pub fn simple_commands(argv: &[String]) -> Vec<Vec<String>> {
    simple_commands_in(argv, &ReadContext::default())
}

/// [`simple_commands`], reading the files the call's programs run from
/// `ctx`'s directory.
pub fn simple_commands_in(argv: &[String], ctx: &ReadContext) -> Vec<Vec<String>> {
    read_call_in(argv, ctx)
        .commands
        .into_iter()
        .map(|c| c.words)
        .collect()
}

/// What a call starts that kGraf cannot read, each as the reason
/// ([`Embedded::Unreadable`]): `source FILE`, `. FILE`, a shell given a file
/// or its input instead of `-c`, `ssh HOST` with no command, `su` with no
/// command, a command named by a variable or a substitution, a recipe or
/// script file that is missing or cannot be parsed, interpreter code that
/// starts a program with arguments kGraf cannot see. Empty when every
/// command the call runs is in [`simple_commands`].
pub fn unreadable_commands(argv: &[String]) -> Vec<String> {
    unreadable_commands_in(argv, &ReadContext::default())
}

/// [`unreadable_commands`], reading files from `ctx`'s directory.
pub fn unreadable_commands_in(argv: &[String], ctx: &ReadContext) -> Vec<String> {
    read_call_in(argv, ctx).unreadable
}

/// Where the files a call's programs read are found: a Makefile, a
/// `package.json`, a justfile, `.cargo/config.toml`, a script file given to
/// an interpreter, git's configuration (decision 0094, amended 2026-10-06).
/// The default knows no directory, so every command that runs what a file
/// says is unreadable.
#[derive(Clone, Debug, Default)]
pub struct ReadContext {
    /// The worktree. No file outside it is read, git's configuration aside.
    pub root: Option<PathBuf>,
    /// The directory the command starts in, inside `root`. `None` when
    /// kGraf cannot tell, as after a `cd` it cannot follow.
    pub cwd: Option<PathBuf>,
    /// The user's home directory, for git's user configuration.
    pub home: Option<PathBuf>,
    /// Variables the command sees that the call itself set: assignments
    /// before the program (`GIT_PAGER=x git log`, `env A=1 make`) and
    /// earlier in its script (`export GIT_SSH_COMMAND=x; git fetch`). A
    /// reader treats them as the program would (git's command variables,
    /// make's variables, cargo's wrappers).
    pub(crate) env: Vec<(String, String)>,
    /// Where each file read and a hash of its text are noted, when the
    /// caller wants to know what a decision rested on
    /// ([`ReadContext::witnessed`]).
    pub(crate) witness: Option<Arc<Mutex<FilesRead>>>,
}

/// The files a reading read, each with a hash of its text, in order.
pub(crate) type FilesRead = Vec<(PathBuf, blake3::Hash)>;

/// Two contexts are equal when they read the same files; where a reading
/// is noted does not change what it reads.
impl PartialEq for ReadContext {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
            && self.cwd == other.cwd
            && self.home == other.home
            && self.env == other.env
    }
}

impl Eq for ReadContext {}

impl ReadContext {
    /// Commands that start in the worktree root `root`.
    pub fn in_worktree(root: &Path, home: Option<&Path>) -> Self {
        Self {
            root: Some(root.to_path_buf()),
            cwd: Some(root.to_path_buf()),
            home: home.map(Path::to_path_buf),
            env: Vec::new(),
            witness: None,
        }
    }

    /// The value the command sees for variable `name`, if the call set it.
    pub(crate) fn var(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The same context with `assignments` (`NAME=value` words) added.
    pub(crate) fn with_assignments<'a>(
        &self,
        assignments: impl IntoIterator<Item = &'a String>,
    ) -> Self {
        let mut next = self.clone();
        for w in assignments {
            if is_assignment(w)
                && let Some((name, value)) = w.split_once('=')
            {
                next.env
                    .push((name.trim_end_matches('+').to_owned(), value.to_owned()));
            }
        }
        next
    }

    /// Every file reading `argv` in this context reads, each with a hash of
    /// its text, in the order read: what a decision on the call rested on.
    /// A command the files of the worktree make run something else after
    /// the decision (another call of the same round rewrote them) is found
    /// by comparing this before and after.
    pub fn witnessed(&self, argv: &[String]) -> Vec<(PathBuf, blake3::Hash)> {
        let witness = Arc::new(Mutex::new(Vec::new()));
        let ctx = Self {
            witness: Some(witness.clone()),
            ..self.clone()
        };
        let _ = read_call_in(argv, &ctx);
        witness.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Note that `path` was read with `text`.
    fn witness(&self, path: &Path, text: &str) {
        if let Some(w) = &self.witness {
            w.lock()
                .unwrap_or_else(|p| p.into_inner())
                .push((path.to_path_buf(), blake3::hash(text.as_bytes())));
        }
    }

    /// The same context, starting in `dir` instead: a relative `dir` is
    /// taken from the current directory. The directory is unknown when the
    /// current one is, when `dir` holds an expansion, or when it leads out
    /// of the worktree.
    fn moved_to(&self, dir: &str) -> Self {
        let cwd = match (&self.cwd, &self.root) {
            (Some(cwd), Some(root))
                if !dir.is_empty()
                    && !dir.contains(['$', '`', '~', '*', '?', '['])
                    && dir != "-" =>
            {
                lexical_normalize(&cwd.join(dir)).filter(|p| p.starts_with(root))
            }
            _ => None,
        };
        Self {
            cwd,
            ..self.clone()
        }
    }

    /// The same context with an unknown current directory.
    fn lost(&self) -> Self {
        Self {
            cwd: None,
            ..self.clone()
        }
    }
}

/// How many commands one call may start before kGraf stops reading it, as
/// unreadable: a recipe or script that runs another, which runs another,
/// can fan out without bound; real calls stay far below this.
const MAX_READ_COMMANDS: usize = 2_000;

/// One command of a call, from [`read_call`].
struct Nested {
    /// 0 for the call's argv, 1 for a command it starts, and so on.
    depth: usize,
    words: Vec<String>,
    /// Whether the command runs in the shell of the command that started
    /// it: the call itself, or a command of a `sh -c` or `eval` script. A
    /// `cd` anywhere else (`find -exec cd`, `ssh host cd`) moves no later
    /// command.
    same_shell: bool,
    /// The directory the command starts in, when a program or a `cd` moved
    /// it there (`make -C DIR`, `cd DIR && ...`); `None` for the worktree
    /// root, or when the directory is not known.
    dir: Option<PathBuf>,
}

/// A call read as every command it runs ([`simple_commands`]) and what it
/// starts that cannot be read ([`unreadable_commands`]).
struct CallReading {
    commands: Vec<Nested>,
    unreadable: Vec<String>,
}

impl CallReading {
    fn unreadable(&mut self, why: String) {
        if !self.unreadable.contains(&why) {
            self.unreadable.push(why);
        }
    }
}

fn read_call(argv: &[String]) -> CallReading {
    read_call_in(argv, &ReadContext::default())
}

fn read_call_in(argv: &[String], ctx: &ReadContext) -> CallReading {
    let mut reading = CallReading {
        commands: vec![Nested {
            depth: 0,
            words: argv.to_vec(),
            same_shell: true,
            dir: None,
        }],
        unreadable: Vec::new(),
    };
    if let Some(why) = changes_path(argv) {
        reading.unreadable(why);
    }
    push_embedded(argv, 1, ctx, &mut reading);
    reading
}

/// Why a command a shell expands is named by an expansion: its program word
/// holds a variable, a substitution or a placeholder kGraf could not
/// resolve (`$CMD x`, `"$@"`, `${X}`). Such a command runs whatever the
/// expansion holds, which no rule or grant can check (decision 0094, amended
/// 2026-10-06).
fn named_by_expansion(words: &[String]) -> Option<String> {
    command_roles(words)
        .into_iter()
        .filter(|(_, role)| matches!(role, WordRole::Program | WordRole::Wrapper))
        .map(|(i, _)| &words[i])
        .find(|w| w.contains(['$', '`']) || w.contains("{}"))
        .map(|w| {
            format!(
                "the command `{}` is named by a variable, a substitution or a placeholder, so \
                 kGraf cannot tell what it runs",
                w.escape_debug()
            )
        })
}

/// Why a command changes where program names are found: it sets `PATH`
/// for a program (`PATH=./bin ls`, `env PATH=./bin ls`) or for the commands
/// after it (`export PATH=./bin`, `PATH=./bin` alone). A name such a command
/// runs may be any file, which no rule on the name checks (decision 0094,
/// amended 2026-10-06).
fn changes_path(words: &[String]) -> Option<String> {
    let set = |w: &String| -> Option<&'static str> {
        let name = w.split_once('=')?.0.trim_end_matches('+');
        CODE_VARIABLES.iter().find(|v| **v == name).copied()
    };
    let roles = command_roles(words);
    let prefix_sets = roles
        .iter()
        .filter(|&&(_, role)| role == WordRole::Prefix)
        .find_map(|&(i, _)| set(&words[i]));
    let exported = matches!(
        words.first().map(String::as_str),
        Some("export" | "declare" | "typeset" | "readonly" | "local")
    )
    .then(|| words[1..].iter().find_map(set))
    .flatten();
    prefix_sets.or(exported).map(|name| {
        if name == "PATH" {
            "the command sets PATH, so kGraf cannot tell which file a program name runs".to_owned()
        } else {
            format!(
                "the command sets `{name}`, which makes a program load code kGraf does not read"
            )
        }
    })
}

/// Environment variables that choose which file a program name runs
/// (`PATH`) or make a program load code before its own (decision 0094,
/// amended 2026-10-06): the dynamic loader's preload and search paths, the
/// shell's startup file for scripts, and the interpreters' startup and
/// module paths.
const CODE_VARIABLES: &[&str] = &[
    "PATH",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "BASH_ENV",
    "ENV",
    "ZDOTDIR",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PYTHONHOME",
    "PYTHONUSERBASE",
    "NODE_OPTIONS",
    "NODE_PATH",
    "PERL5OPT",
    "PERL5LIB",
    "PERLLIB",
    "RUBYOPT",
    "RUBYLIB",
    "PHPRC",
    "PHP_INI_SCAN_DIR",
];

/// The context a directory change moves the next commands of a script to
/// (`cd DIR`, `pushd DIR`, `popd`); `None` when `words` changes no
/// directory. The directory stays known only for a plain `cd` to a literal
/// path in the worktree.
fn after_directory_change(words: &[String], ctx: &ReadContext) -> Option<ReadContext> {
    let at = command_starts(words)
        .into_iter()
        .find(|&i| DIRECTORY_CHANGERS.contains(&base_name(&words[i])))?;
    let plain = at == 0 && base_name(&words[0]) == "cd";
    Some(match &words[1..] {
        [dir] if plain && !dir.starts_with('-') => ctx.moved_to(dir),
        _ => ctx.lost(),
    })
}

fn push_embedded(words: &[String], depth: usize, ctx: &ReadContext, reading: &mut CallReading) {
    if depth > MAX_SHELL_DEPTH {
        return;
    }
    for (at, found) in embedded(words, ctx) {
        push_found(words, at, found, depth, ctx, reading);
    }
}

fn push_found(
    words: &[String],
    at: usize,
    found: Embedded,
    depth: usize,
    ctx: &ReadContext,
    reading: &mut CallReading,
) {
    if reading.commands.len() >= MAX_READ_COMMANDS {
        reading.unreadable("it starts more commands than kGraf reads".into());
        return;
    }
    let dir_of = |c: &ReadContext| c.cwd.clone().filter(|d| Some(d) != c.root.as_ref());
    match found {
        Embedded::In(moved, inner) => push_found(words, at, *inner, depth, &moved, reading),
        Embedded::Argv(argv) => {
            if argv.is_empty() {
                return;
            }
            for why in [named_by_expansion(&argv), changes_path(&argv)]
                .into_iter()
                .flatten()
            {
                reading.unreadable(why);
            }
            reading.commands.push(Nested {
                depth,
                words: argv.clone(),
                same_shell: false,
                dir: dir_of(ctx),
            });
            push_embedded(&argv, depth + 1, ctx, reading);
        }
        Embedded::Script(script) => {
            let b = program_key(&words[at]);
            let same_shell = b == "eval" || shell_name(&b).is_some();
            let split = split_script_parts(&script);
            if split.by_substitution {
                reading.unreadable(
                    "a command is named by a substitution, so kGraf cannot tell what it runs"
                        .into(),
                );
            }
            // A script whose quotes never close, or a command that does not
            // split into words, is read otherwise by the shell: kGraf
            // cannot tell its commands, so it never reads it as readable.
            if split.unbalanced {
                reading.unreadable(
                    "a quote or a substitution of the script never closes, so kGraf cannot tell \
                     its commands"
                        .into(),
                );
            }
            // fish runs `(...)` as a command substitution anywhere in a word.
            if shell_name(&b) == Some("fish") && script.contains('(') {
                reading.unreadable(
                    "the fish script holds a command substitution, so kGraf cannot tell what it \
                     runs"
                        .into(),
                );
            }
            // A `cd` in the script moves the commands after it, for the
            // files they read (`cd web && npm test` reads web's
            // `package.json`).
            let mut here = ctx.clone();
            for (segment, body) in split.parts {
                let words = match shlex::split(&segment) {
                    Some(w) => w,
                    None => {
                        if !body {
                            reading.unreadable(format!(
                                "the command `{}` does not split into words, so kGraf cannot \
                                 tell what it runs",
                                segment.escape_debug()
                            ));
                        }
                        segment.split_whitespace().map(str::to_owned).collect()
                    }
                };
                for words in split_at_braces(words) {
                    if words.is_empty() {
                        continue;
                    }
                    for why in [
                        named_by_expansion(&words),
                        changes_path(&words),
                        expanded_program(&words),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        reading.unreadable(why);
                    }
                    for value in prompt_assignments(&words) {
                        push_found(
                            &words,
                            0,
                            Embedded::Script(value),
                            depth + 1,
                            &here,
                            reading,
                        );
                    }
                    reading.commands.push(Nested {
                        depth,
                        words: words.clone(),
                        same_shell,
                        dir: dir_of(&here),
                    });
                    push_embedded(&words, depth + 1, &here, reading);
                    if let Some(moved) = after_directory_change(&words, &here) {
                        here = moved;
                    }
                    if let Some(assigned) = after_assignment(&words, &here) {
                        here = assigned;
                    }
                }
            }
        }
        Embedded::Unreadable(why) => reading.unreadable(why),
    }
}

/// What a command runs besides its own program, read from its arguments
/// (decision 0094, amended 2026-10-06).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Embedded {
    /// An argv run as a command: `find -exec PROG ARGS ;`, `script FILE
    /// PROG` (BSD), `direnv exec DIR PROG`, `uv run PROG`.
    Argv(Vec<String>),
    /// Text a shell reads as a script: `sh -c`, `eval`, `su -c`, `ssh HOST
    /// CMD`, `script -c`, `trap CMD SIG`, `watch CMD`, `flock F -c`, `env
    /// -S`, `parallel CMD :::`.
    Script(String),
    /// Commands kGraf cannot read, and why: a file (`source FILE`, `bash
    /// FILE`), a shell's input (`bash` with no `-c`), a shell on another
    /// machine (`ssh HOST`), an interactive shell (`su USER`, `script FILE`).
    Unreadable(String),
    /// What the inner value runs, read with another context: a command
    /// that starts in another directory (`make -C DIR`, a recipe of a
    /// Makefile there), or one whose directory kGraf cannot tell.
    In(ReadContext, Box<Embedded>),
}

/// What a word of a command is, as [`command_roles`] reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WordRole {
    /// Shell syntax, an assignment, a redirection or its target, an option,
    /// a number or a wrapper's operand: never a program.
    Prefix,
    /// A wrapper of [`WRAPPERS`]: a program that runs the rest.
    Wrapper,
    /// The value an option may take, or the program (`sudo -u bob curl`):
    /// read as both.
    ValueOrProgram,
    /// The first word that is surely not a prefix: the program.
    Program,
}

/// Every command a command's arguments start, each with the index of the
/// word that starts it. Read at each place [`command_roles`] lists that may
/// hold a program, so a wrapper hides none of them.
fn embedded(words: &[String], ctx: &ReadContext) -> Vec<(usize, Embedded)> {
    let mut out = Vec::new();
    for (at, role) in command_roles(words) {
        if role == WordRole::Prefix {
            continue;
        }
        for found in embedded_at(words, at, ctx) {
            out.push((at, found));
        }
    }
    out
}

/// Programs that run their command on another machine: the command is read
/// for the user's rules, and no local grant covers it.
const REMOTE_RUNNERS: &[&str] = &["ssh"];

/// Wrappers that run another command and that no program grant covers, as
/// in Claude Code, where only an exact rule for the whole command line
/// approves them; kGraf's grants name programs, never command lines.
const EXEC_WRAPPERS: &[&str] = &["watch", "setsid", "ionice", "flock"];

/// `find` actions that run a program or delete what is found. A grant of
/// `find` covers none of them, as in Claude Code.
const FIND_ACTIONS: &[&str] = &["-exec", "-execdir", "-ok", "-okdir", "-delete"];

/// Programs that run a command given after a subcommand and that many
/// operands: `direnv exec DIR PROG`, `uv run PROG`.
const SUBCOMMAND_RUNNERS: &[(&str, &str, usize)] = &[
    ("direnv", "exec", 1),
    ("mise", "exec", 0),
    ("mise", "x", 0),
    ("devbox", "run", 0),
    ("uv", "run", 0),
    ("poetry", "run", 0),
    ("pipenv", "run", 0),
    ("bundle", "exec", 0),
    ("conda", "run", 0),
];

/// The value of a `-c` option among `args`: the word after the first short
/// option holding `c` (`-c CMD`, `-lc CMD`), or after a long name in `long`
/// (`--command CMD`, `--command=CMD`). `None` when there is none.
fn dash_c(args: &[String], long: &[&str]) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        for name in long {
            if arg == name {
                return it.next().cloned();
            }
            if let Some(v) = arg
                .strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('='))
            {
                return Some(v.to_owned());
            }
        }
        if arg.starts_with('-') && !arg.starts_with("--") && arg.contains('c') {
            // `bash -c -- SCRIPT`: `--` ends the options, and the script
            // follows it.
            let next = it.next();
            if next.is_some_and(|n| n == "--") {
                return it.next().cloned();
            }
            return next.cloned();
        }
    }
    None
}

/// The shell of [`SHELLS`] a program key ([`program_key`]) names, without
/// a version suffix (`bash5`, `bash-5.2`, `ksh93`): its `-c` argument is a
/// script. The key is lowercase, so `SH` and `Bash`, which a
/// case-insensitive file system runs as `sh` and `bash`, are shells too.
fn shell_name(key: &str) -> Option<&'static str> {
    let stem = key.trim_end_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | '-' | '_'));
    SHELLS.iter().copied().find(|s| *s == stem)
}

/// A command's words split where a brace group opens or closes (`{`, `}`)
/// after its first word: zsh runs `if true { curl x }`, and a word after
/// `{` is a program in every shell. Each brace word starts the command
/// after it, where [`command_starts`] skips it.
fn split_at_braces(words: Vec<String>) -> Vec<Vec<String>> {
    if !words.iter().skip(1).any(|w| w == "{" || w == "}") {
        return vec![words];
    }
    let mut out: Vec<Vec<String>> = vec![Vec::new()];
    for (i, w) in words.into_iter().enumerate() {
        if i > 0 && (w == "{" || w == "}") {
            out.push(Vec::new());
        }
        if let Some(last) = out.last_mut() {
            last.push(w);
        }
    }
    out
}

/// Why a command of a script names a program the shell expands into
/// another name: a program word holding a glob (`/usr/bin/cur?`), a brace
/// list or range (`cu{r,}l`, `{curl,x}`), or zsh's `=NAME` (the path of
/// NAME). kGraf cannot tell which program such a word names (decision 0094,
/// amended 2026-10-06).
fn expanded_program(words: &[String]) -> Option<String> {
    let braces = |w: &str| -> bool {
        w.find('{').is_some_and(|open| {
            w[open..].find('}').is_some_and(|close| {
                let inside = &w[open + 1..open + close];
                inside.contains(',') || inside.contains("..")
            })
        })
    };
    let roles = command_roles(words);
    let equals = roles
        .iter()
        .map(|&(i, _)| words[i].as_str())
        .find(|w| w.len() > 1 && w.starts_with('='));
    let expanded = roles
        .iter()
        .filter(|(_, role)| matches!(role, WordRole::Program | WordRole::Wrapper))
        .map(|&(i, _)| words[i].as_str())
        .find(|w| {
            *w != "["
                && *w != "[["
                && (w.contains(['*', '?']) || (w.contains('[') && w.contains(']')) || braces(w))
        });
    expanded.or(equals).map(|w| {
        format!(
            "the program word `{}` is expanded by the shell, so kGraf cannot tell which program \
             it names",
            w.escape_debug()
        )
    })
}

/// Variables a shell expands as commands or command substitutions without
/// a command naming them: the prompts (`PS4` on every traced command with
/// `set -x`; `PS0`, `PS1`, `PS2` and `PROMPT_COMMAND` in an interactive
/// shell).
const PROMPT_VARIABLES: &[&str] = &["PS0", "PS1", "PS2", "PS4", "PROMPT_COMMAND"];

/// The values a command assigns to a [`PROMPT_VARIABLES`] variable, each
/// read as a script (`PS4='$(curl x)'`).
fn prompt_assignments(words: &[String]) -> Vec<String> {
    words
        .iter()
        .filter_map(|w| w.split_once('='))
        .filter(|(name, _)| PROMPT_VARIABLES.contains(name))
        .map(|(_, value)| value.to_owned())
        .collect()
}

/// The placeholder `xargs` fills from its input when it has one: `-I R`,
/// `-IR`, `-J R` (BSD), `-i[R]` and `--replace[=R]` (`{}` by default).
fn xargs_placeholder(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" || !a.starts_with('-') {
            break;
        }
        for name in ["-I", "-J"] {
            if a == name {
                return it.next().cloned();
            }
            if let Some(v) = a.strip_prefix(name).filter(|v| !v.is_empty()) {
                return Some(v.to_owned());
            }
        }
        if a == "-i" || a == "--replace" {
            return Some("{}".to_owned());
        }
        if let Some(v) = a
            .strip_prefix("--replace=")
            .or_else(|| a.strip_prefix("-i"))
        {
            return Some(v.to_owned());
        }
    }
    None
}

/// Why the command an input filler runs (`xargs`, `find -exec`, `parallel`)
/// is named by its input: the placeholder stands in its program word, or in
/// any argument of a program that runs its argument as code (a shell,
/// `eval`, an interpreter). Elsewhere a placeholder is an argument like any
/// other (`xargs -I{} cp {} d/`).
fn placeholder_runs(argv: &[String], placeholder: &str) -> Option<String> {
    let starts = command_starts(argv);
    let program = starts.last().copied()?;
    let key = program_key(&argv[program]);
    let runs_code = shell_name(&key).is_some()
        || key == "eval"
        || code::interpreter(&key).is_some()
        || matches!(key.as_str(), "cmd" | "powershell" | "pwsh");
    let named = starts.iter().any(|&i| argv[i].contains(placeholder))
        || (runs_code && argv[program + 1..].iter().any(|a| a.contains(placeholder)));
    named.then(|| {
        format!(
            "the command it runs is filled in from its input at `{placeholder}`, so kGraf \
             cannot tell what it runs"
        )
    })
}

/// The string of `env -S STRING` (`--split-string`), which `env` splits into
/// the command it runs.
fn env_split_string(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "-S" || arg == "--split-string" {
            return it.next().cloned();
        }
        if let Some(v) = arg
            .strip_prefix("--split-string=")
            .or_else(|| arg.strip_prefix("-S").filter(|v| !v.is_empty()))
        {
            return Some(v.to_owned());
        }
    }
    None
}

/// The values of the options `names` among `args`: a one-letter name as
/// `-X VALUE` or `-XVALUE`, a longer one as `--NAME VALUE` or
/// `--NAME=VALUE`.
fn option_values(args: &[String], names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        for name in names {
            let (exact, attached) = if name.len() == 1 {
                (format!("-{name}"), format!("-{name}"))
            } else {
                (format!("--{name}"), format!("--{name}="))
            };
            if *arg == exact {
                if let Some(v) = it.next() {
                    out.push(v.clone());
                }
                break;
            }
            if let Some(v) = arg.strip_prefix(&attached).filter(|v| !v.is_empty()) {
                out.push(v.to_owned());
                break;
            }
        }
    }
    out
}

/// The largest file the permission check reads (a recipe file, a script):
/// a larger one is unreadable. Real recipe and script files are far
/// smaller; the cap bounds the time and memory a check can take.
const MAX_READ_FILE_BYTES: u64 = 1024 * 1024;

/// A text file of the worktree, read by the permission check.
struct ReadFile {
    /// Where it was found, lexically normalized.
    path: PathBuf,
    text: Arc<str>,
}

/// What identifies one version of a file without reading it: its inode,
/// length, and modification and change times. A rewrite changes the change
/// time, which no program can set back.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileVersion {
    ino: u64,
    len: u64,
    modified: Option<std::time::SystemTime>,
    changed_ns: i128,
}

impl FileVersion {
    fn of(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let (ino, changed_ns) = {
            use std::os::unix::fs::MetadataExt;
            (
                meta.ino(),
                i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
            )
        };
        #[cfg(not(unix))]
        let (ino, changed_ns) = (
            0,
            meta.created()
                .ok()
                .and_then(|c| c.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as i128),
        );
        Self {
            ino,
            len: meta.len(),
            modified: meta.modified().ok(),
            changed_ns,
        }
    }

    /// Whether the file changed within the last two seconds: a file system
    /// whose clock ticks coarsely can give two quick writes one time, so a
    /// recent version is always read again (the rule git uses for its
    /// index).
    fn recent(&self) -> bool {
        self.modified.is_none_or(|m| {
            std::time::SystemTime::now()
                .duration_since(m)
                .map_or(true, |age| age < std::time::Duration::from_secs(2))
        })
    }
}

/// The text of each file the permission check read, by path and version, so
/// a repeated check stats a file instead of reading it again (decision 0094,
/// amended 2026-10-06). Process-wide on purpose, as [`ContentCache`]: an
/// entry is used only for the very version it was read from. Bounded:
/// cleared when full.
static FILE_TEXTS: std::sync::LazyLock<Mutex<FileTexts>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Each read file's text by path, with the version it was read from.
type FileTexts = std::collections::HashMap<PathBuf, (FileVersion, Arc<str>)>;

/// Read `name` (relative to `ctx`'s directory, or absolute) as UTF-8 text,
/// for the permission check: only a regular file lexically inside the
/// worktree, at most [`MAX_READ_FILE_BYTES`], opened confined to the
/// worktree so a link cannot lead out of it, never a FIFO or a device
/// ([`crate::fs_safety::read_capped`]). `Err` says why it cannot be read.
fn read_worktree_file(ctx: &ReadContext, name: &str) -> Result<ReadFile, String> {
    let (Some(root), Some(cwd)) = (&ctx.root, &ctx.cwd) else {
        return Err(format!(
            "kGraf cannot tell which directory `{}` is read from",
            name.escape_debug()
        ));
    };
    if name.is_empty() || name.contains(['$', '`', '*', '?']) || name.starts_with('~') {
        return Err(format!(
            "`{}` is not a literal file name",
            name.escape_debug()
        ));
    }
    let path = lexical_normalize(&cwd.join(name))
        .filter(|p| p.starts_with(root))
        .ok_or_else(|| format!("`{}` is outside the worktree", name.escape_debug()))?;
    let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
    let version = std::fs::metadata(&path).ok().map(|m| FileVersion::of(&m));
    let cached = version.filter(|v| !v.recent()).and_then(|v| {
        FILE_TEXTS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&path)
            .filter(|(seen, _)| *seen == v)
            .map(|(_, text)| text.clone())
    });
    let text = match cached {
        Some(text) => text,
        None => {
            let dir = crate::fs_safety::open_workspace("shell", root).map_err(|e| e.to_string())?;
            let bytes =
                crate::fs_safety::read_capped("shell", &dir, &rel, MAX_READ_FILE_BYTES, true)
                    .map_err(|e| format!("`{}` cannot be read: {e}", rel.display()))?;
            let text: Arc<str> = String::from_utf8(bytes)
                .map_err(|_| format!("`{}` is not UTF-8 text", rel.display()))?
                .into();
            if let Some(v) = version {
                let mut texts = FILE_TEXTS.lock().unwrap_or_else(|p| p.into_inner());
                if texts.len() >= CONTENT_CACHE_ENTRIES {
                    texts.clear();
                }
                texts.insert(path.clone(), (v, text.clone()));
            }
            text
        }
    };
    ctx.witness(&path, &text);
    Ok(ReadFile { path, text })
}

/// Whether `name` exists in `ctx`'s directory as a regular file (following
/// links), lexically inside the worktree.
fn worktree_file_exists(ctx: &ReadContext, name: &str) -> bool {
    let (Some(root), Some(cwd)) = (&ctx.root, &ctx.cwd) else {
        return false;
    };
    lexical_normalize(&cwd.join(name))
        .filter(|p| p.starts_with(root))
        .and_then(|p| std::fs::metadata(p).ok())
        .is_some_and(|m| m.is_file())
}

/// A memo of what the permission check parsed out of a file or a piece of
/// code, keyed by a hash of the text it was parsed from (decision 0094,
/// amended 2026-10-06). Process-wide on purpose: the key is the content,
/// so an entry can never go stale and holds no policy state, and the check
/// runs on every shell call, often on the same `package.json` or script.
/// Bounded: cleared when full.
struct ContentCache<T> {
    entries: std::sync::LazyLock<Mutex<std::collections::HashMap<blake3::Hash, Arc<T>>>>,
}

/// How many parsed texts a [`ContentCache`] keeps before it starts over.
const CONTENT_CACHE_ENTRIES: usize = 256;

impl<T> ContentCache<T> {
    const fn new() -> Self {
        Self {
            entries: std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new())),
        }
    }
    /// The parse of `text` under `kind`, computed by `parse` once per
    /// distinct text.
    fn get(&self, kind: &str, text: &str, parse: impl FnOnce() -> T) -> Arc<T> {
        let mut h = blake3::Hasher::new();
        h.update(kind.as_bytes());
        h.update(&[0]);
        h.update(text.as_bytes());
        let key = h.finalize();
        if let Some(hit) = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
        {
            return hit.clone();
        }
        let value = Arc::new(parse());
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if entries.len() >= CONTENT_CACHE_ENTRIES {
            entries.clear();
        }
        entries.insert(key, value.clone());
        value
    }
}

/// A program name as Windows and the shells compare it: the last path
/// component after `/` or `\`, lowercase, without `.exe`, `.cmd` or `.bat`.
fn program_key(word: &str) -> String {
    let last = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let lower = last.to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".com"] {
        if let Some(stem) = lower.strip_suffix(ext) {
            return stem.to_owned();
        }
    }
    lower
}

/// The context the program at `words[at]` reads its files in, after the
/// prefix words before it: a wrapper that changes the directory (`env -C
/// DIR`, `sudo -D DIR`, `nsenter -w`) or the root (`chroot`) leaves it
/// unknown, so no file a different directory holds is mistaken for the one
/// that runs.
fn prefix_context(words: &[String], at: usize, ctx: &ReadContext) -> ReadContext {
    let mut i = 0;
    while i < at {
        let b = base_name(&words[i]);
        let rest = &words[i + 1..at];
        let moves = match b {
            "chroot" => true,
            "env" => rest.iter().any(|w| {
                w == "-C" || w == "--chdir" || w.starts_with("--chdir=") || w.starts_with("-C")
            }),
            "sudo" => rest.iter().any(|w| {
                w == "-D" || w == "--chdir" || w.starts_with("--chdir=") || w.starts_with("-D")
            }),
            "nsenter" => rest
                .iter()
                .any(|w| w.starts_with("-w") || w.starts_with("--wd") || w.starts_with("-r")),
            "doas" | "su" | "runuser" | "setpriv" | "unshare" | "firejail" | "systemd-run" => {
                rest.iter().any(|w| {
                    w.contains("chdir") || w.contains("cwd") || w.contains("working-directory")
                })
            }
            _ => false,
        };
        if moves {
            return ctx.lost().with_assignments(&words[..at]);
        }
        i += 1;
    }
    ctx.with_assignments(&words[..at])
}

/// The context an assignment command of a script leaves the commands after
/// it in (`X=1`, `export X=1`, `declare -x X=1`); `None` when `words`
/// assigns nothing.
pub(crate) fn after_assignment(words: &[String], ctx: &ReadContext) -> Option<ReadContext> {
    let all_assignments = !words.is_empty() && words.iter().all(|w| is_assignment(w));
    let declares = matches!(
        words.first().map(String::as_str),
        Some("export" | "declare" | "typeset" | "readonly" | "local")
    );
    if all_assignments {
        return Some(ctx.with_assignments(words));
    }
    if declares {
        return Some(ctx.with_assignments(&words[1..]));
    }
    None
}

/// The commands the program at `words[at]` starts from its arguments, read
/// in `ctx` (a file it runs is read from there).
fn embedded_at(words: &[String], at: usize, ctx: &ReadContext) -> Vec<Embedded> {
    let ctx = &prefix_context(words, at, ctx);
    if let Some(found) = readers::embedded_by_reader(words, at, ctx) {
        return found;
    }
    let b = base_name(&words[at]);
    let args = &words[at + 1..];
    let unreadable = |why: &str| vec![Embedded::Unreadable(format!("`{b}` {why}"))];
    // Each place a wrapper-like program's command may start, among
    // `words[at..end]` (`watch -n 1 CMD`, `parallel -j 4 CMD`).
    let tails = |end: usize| -> Vec<usize> {
        command_roles(&words[at..end])
            .into_iter()
            .filter(|&(i, role)| {
                i > 0
                    && matches!(role, WordRole::ValueOrProgram | WordRole::Program)
                    && words[at + i].parse::<f64>().is_err()
            })
            .map(|(i, _)| at + i)
            .collect()
    };
    if b == "eval" {
        return vec![Embedded::Script(args.join(" "))];
    }
    if shell_name(&program_key(&words[at])).is_some() {
        if let Some(script) = dash_c(args, &[]) {
            return vec![Embedded::Script(script)];
        }
        // Asked for its version or help, or to check a script's syntax, a
        // shell runs nothing.
        let inert = args
            .iter()
            .any(|a| matches!(a.as_str(), "--version" | "--help" | "-n" | "--noexec"));
        return if inert {
            Vec::new()
        } else {
            unreadable("reads its commands from a file or its input")
        };
    }
    match b {
        "find" => {
            let mut out = Vec::new();
            let mut i = 0;
            while i < args.len() {
                if matches!(args[i].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
                    let end = args[i + 1..]
                        .iter()
                        .position(|w| matches!(w.as_str(), ";" | "\\;" | "+"))
                        .map_or(args.len(), |p| i + 1 + p);
                    if end > i + 1 {
                        let argv = args[i + 1..end].to_vec();
                        let filled = placeholder_runs(&argv, "{}");
                        out.push(Embedded::Argv(argv));
                        if let Some(why) = filled {
                            out.push(Embedded::Unreadable(format!("`find` {why}")));
                        }
                    }
                    i = end + 1;
                } else {
                    i += 1;
                }
            }
            out
        }
        "su" | "runuser" => match dash_c(args, &["--command", "--session-command"]) {
            Some(script) => vec![Embedded::Script(script)],
            // `runuser -u USER PROG` runs PROG as an argv, which the wrapper
            // scan reads; `su USER` and `runuser USER` start a shell.
            None if b == "runuser" && args.iter().any(|a| a == "-u" || a == "--user") => Vec::new(),
            None => unreadable("with no `-c` command starts a shell that reads its input"),
        },
        "sg" => {
            // `sg GROUP [-c] CMD`: the command is a script.
            let rest: Vec<&str> = args
                .iter()
                .skip(1)
                .map(String::as_str)
                .filter(|a| *a != "-c")
                .collect();
            if rest.is_empty() {
                unreadable("with no command starts a shell that reads its input")
            } else {
                vec![Embedded::Script(rest.join(" "))]
            }
        }
        "ssh" => {
            // Options that take a value, as `ssh(1)` lists them.
            const VALUED: &str = "BbcDEeFIiJLlmOoPpQRSWw";
            let mut i = 0;
            while let Some(a) = args.get(i) {
                if a == "--" {
                    i += 1;
                    break;
                }
                let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) else {
                    break;
                };
                i += 1;
                // A valued option's value is the rest of its word, or the
                // next word when the option ends its word.
                if let Some(k) = flags.find(|c| VALUED.contains(c))
                    && k + 1 == flags.len()
                {
                    i += 1;
                }
            }
            // `-o ProxyCommand=CMD` and `-o LocalCommand=CMD` run CMD on
            // this machine, through a shell.
            let mut out: Vec<Embedded> = option_values(args, &["o"])
                .into_iter()
                .filter_map(|v| {
                    let (key, value) = v.split_once(['=', ' '])?;
                    matches!(
                        key.to_ascii_lowercase().as_str(),
                        "proxycommand" | "localcommand"
                    )
                    .then(|| Embedded::Script(value.trim_start_matches('=').trim().to_owned()))
                })
                .collect();
            match args.get(i + 1..) {
                Some(rest) if !rest.is_empty() => out.push(Embedded::Script(rest.join(" "))),
                _ => out.extend(unreadable(
                    "with no command starts a shell on another machine",
                )),
            }
            out
        }
        // Options whose value is a program the command runs: ripgrep's
        // preprocessor, tar's filters and scripts, rsync's remote shell.
        "rg" | "ripgrep" => option_values(args, &["pre", "hostname-bin"])
            .into_iter()
            .map(|v| Embedded::Argv(vec![v]))
            .collect(),
        "tar" | "gtar" | "bsdtar" => {
            let mut out: Vec<Embedded> = option_values(
                args,
                &[
                    "to-command",
                    "use-compress-program",
                    "I",
                    "rsh-command",
                    "info-script",
                    "new-volume-script",
                    "F",
                ],
            )
            .into_iter()
            .map(Embedded::Script)
            .collect();
            out.extend(
                option_values(args, &["checkpoint-action"])
                    .into_iter()
                    .filter_map(|v| {
                        v.strip_prefix("exec=")
                            .map(|c| Embedded::Script(c.to_owned()))
                    }),
            );
            out
        }
        "rsync" => option_values(args, &["e", "rsh"])
            .into_iter()
            .map(Embedded::Script)
            .collect(),
        "script" => {
            if let Some(cmd) = dash_c(args, &["--command"]) {
                return vec![Embedded::Script(cmd)];
            }
            // BSD and macOS: `script [-q] [FILE [PROG ARGS]]`. Options that
            // take a value, util-linux's and BSD's, skip it.
            const VALUED: &[&str] = &["-E", "-I", "-O", "-B", "-T", "-m", "-o", "-F", "-t"];
            let mut i = 0;
            while let Some(a) = args.get(i) {
                if a == "--" {
                    i += 1;
                    break;
                }
                if !a.starts_with('-') {
                    break;
                }
                i += if VALUED.contains(&a.as_str()) { 2 } else { 1 };
            }
            match args.get(i + 1..) {
                Some(rest) if !rest.is_empty() => vec![Embedded::Argv(rest.to_vec())],
                _ => unreadable("with no command starts an interactive shell"),
            }
        }
        "trap" => {
            let args: Vec<&String> = args.iter().filter(|a| *a != "--").collect();
            match args.as_slice() {
                [handler, _signal, ..]
                    if !handler.is_empty()
                        && !handler.starts_with('-')
                        && handler.parse::<u32>().is_err() =>
                {
                    vec![Embedded::Script((*handler).clone())]
                }
                _ => Vec::new(),
            }
        }
        "source" | "." => unreadable("runs the commands of a file"),
        // `xargs -I R CMD`: the wrapper scan reads CMD; a placeholder in
        // its program word or in code it runs is filled from the input.
        "xargs" => {
            let Some(r) = xargs_placeholder(args) else {
                return Vec::new();
            };
            match tails(words.len()).first() {
                Some(&p) => placeholder_runs(&words[p..], &r)
                    .map(|why| unreadable(&why))
                    .unwrap_or_default(),
                None => Vec::new(),
            }
        }
        // A name the script defines to run something else: `alias c=curl`
        // runs `curl` as `c`, `hash -p /usr/bin/curl g` runs it as `g`. The
        // value is read as the command it runs.
        "alias" => args
            .iter()
            .filter_map(|a| a.split_once('='))
            .map(|(_, v)| Embedded::Script(v.to_owned()))
            .collect(),
        "hash" => {
            let mut out = Vec::new();
            let mut it = args.iter();
            while let Some(a) = it.next() {
                if a == "-p"
                    && let Some(path) = it.next()
                {
                    out.push(Embedded::Argv(vec![path.clone()]));
                }
            }
            out
        }
        // Builtins that run text kGraf does not see: history (`fc`), a
        // loaded builtin (`enable -f`), a function file (`autoload`).
        "fc" => unreadable("runs commands from the shell's history"),
        "enable" if args.iter().any(|a| a.starts_with('-') && a.contains('f')) => {
            unreadable("loads a builtin from a file")
        }
        "autoload" => unreadable("loads functions from files kGraf does not read"),
        "env" => env_split_string(args)
            .map(|s| vec![Embedded::Script(s)])
            .unwrap_or_default(),
        // `flock FILE -c CMD`; `flock FILE PROG ARGS` is read by the wrapper
        // scan.
        "flock" => dash_c(args.get(1..).unwrap_or_default(), &["--command"])
            .map(|s| vec![Embedded::Script(s)])
            .unwrap_or_default(),
        "watch" => {
            // `watch` hands its command words, joined, to `sh -c`; with
            // `-x` it runs them as an argv, which the wrapper scan reads.
            if args.iter().any(|a| a == "-x" || a == "--exec") {
                return Vec::new();
            }
            tails(words.len())
                .into_iter()
                .map(|p| Embedded::Script(words[p..].join(" ")))
                .collect()
        }
        "parallel" => {
            let sep = |w: &String| matches!(w.as_str(), ":::" | "::::" | ":::+" | "::::+");
            let end = words[at..]
                .iter()
                .position(sep)
                .map_or(words.len(), |p| at + p);
            // GNU parallel hands its command, joined, to a shell. Its
            // placeholders (`{}`, `{.}`, `{1}`) are filled from the input.
            let commands: Vec<Embedded> = tails(end)
                .into_iter()
                .flat_map(|p| {
                    let script = words[p..end].join(" ");
                    let as_words = shlex::split(&script)
                        .unwrap_or_else(|| script.split_whitespace().map(str::to_owned).collect());
                    let filled = placeholder_runs(&as_words, "{")
                        .map(|why| Embedded::Unreadable(format!("`parallel` {why}")));
                    std::iter::once(Embedded::Script(script)).chain(filled)
                })
                .collect();
            if !commands.is_empty() {
                return commands;
            }
            // No command: each input is a command. Inputs from a file or
            // from standard input cannot be read.
            if end == words.len() || words[end..].iter().any(|w| w.starts_with("::::")) {
                return unreadable(
                    "with no command runs the commands it reads from its input or a file",
                );
            }
            words[end..]
                .iter()
                .filter(|w| !sep(w))
                .map(|w| Embedded::Script(w.clone()))
                .collect()
        }
        "sudo" | "doas" => {
            // `sudo -s` and `sudo -i` with no command start a shell.
            let shell = args
                .iter()
                .any(|a| matches!(a.as_str(), "-s" | "-i" | "--shell" | "--login"));
            if shell && tails(words.len()).is_empty() {
                unreadable("starts a shell that reads its input")
            } else {
                Vec::new()
            }
        }
        _ => SUBCOMMAND_RUNNERS
            .iter()
            .find(|(p, sub, _)| *p == b && args.first().is_some_and(|a| a == sub))
            .map(|&(_, _, operands)| {
                let tail = args.get(1 + operands..).unwrap_or_default();
                // `uv run --directory DIR`, `direnv exec DIR`, `mise exec
                // -C DIR`: the command starts elsewhere, so a file it reads
                // is not read from here.
                let moved = operands > 0
                    || tail.iter().any(|w| {
                        [
                            "--directory",
                            "--project",
                            "--cwd",
                            "--dir",
                            "-C",
                            "--chdir",
                        ]
                        .iter()
                        .any(|o| w == o || w.starts_with(&format!("{o}=")))
                    });
                command_roles(tail)
                    .into_iter()
                    .filter(|&(_, role)| {
                        matches!(role, WordRole::ValueOrProgram | WordRole::Program)
                    })
                    .map(|(i, _)| {
                        // `uv run x.py` runs the file with Python, not a
                        // program of that name.
                        let mut argv = tail[i..].to_vec();
                        if b == "uv" && argv[0].ends_with(".py") {
                            argv.insert(0, "python3".to_owned());
                        }
                        let argv = Embedded::Argv(argv);
                        if moved {
                            Embedded::In(ctx.lost(), Box::new(argv))
                        } else {
                            argv
                        }
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Every program a command runs that a program grant must name, in order,
/// or why no program grant covers the command (decision 0094, amended
/// 2026-10-06). A grant of a program that runs another (`env`, `xargs`,
/// `sudo`, `sh -c`, `su -c`) does not grant what it runs: each wrapper, the
/// program it runs, every word that may be either (`sudo -u bob curl` names
/// `bob` too, which errs toward asking) and every command of a script it
/// hands to a shell (read by the strict reader, [`script_commands`]) is
/// listed. Refused: a command started on another machine
/// ([`REMOTE_RUNNERS`]), the exec wrappers ([`EXEC_WRAPPERS`]), `find` with
/// an action of [`FIND_ACTIONS`], a command kGraf cannot read
/// ([`unreadable_commands`]) and a script the strict reader does not judge.
pub fn programs_to_grant(words: &[String]) -> Result<Vec<String>, String> {
    programs_to_grant_in(words, &ReadContext::default())
}

/// [`programs_to_grant`], reading the files the command's programs run
/// (a recipe, a script, a configuration value) from `ctx`'s directory: a
/// grant of `make`, `npm`, `just` or `python3` covers none of what the file
/// makes them run, which needs its own grants; a file kGraf cannot read
/// leaves the command to no grant (decision 0094, amended 2026-10-06).
pub fn programs_to_grant_in(words: &[String], ctx: &ReadContext) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut visits = 0;
    collect_programs_to_grant(words, 0, ctx, &mut visits, &mut out)?;
    Ok(out)
}

fn collect_programs_to_grant(
    words: &[String],
    depth: usize,
    ctx: &ReadContext,
    visits: &mut usize,
    out: &mut Vec<String>,
) -> Result<(), String> {
    if depth > MAX_SHELL_DEPTH {
        return Err("its commands nest too deeply to read".into());
    }
    *visits += 1;
    if *visits > MAX_READ_COMMANDS {
        return Err("it starts more commands than kGraf reads".into());
    }
    // A command a shell expands whose program is a variable or a
    // substitution runs what no grant names. The call's own argv is not
    // expanded by any shell.
    if depth > 0
        && let Some(why) = named_by_expansion(words)
    {
        return Err(why);
    }
    if let Some(why) = changes_path(words) {
        return Err(why);
    }
    for (at, role) in command_roles(words) {
        let w = &words[at];
        let named = match role {
            WordRole::Prefix => false,
            WordRole::Wrapper | WordRole::Program => true,
            WordRole::ValueOrProgram => w.parse::<f64>().is_err(),
        };
        if !named {
            continue;
        }
        let b = base_name(w);
        if REMOTE_RUNNERS.contains(&b) {
            return Err(format!(
                "`{b}` runs its command on another machine, and no grant covers that"
            ));
        }
        if EXEC_WRAPPERS.contains(&b) {
            return Err(format!(
                "`{b}` runs another command, and a program grant never covers it"
            ));
        }
        if b == "find"
            && words[at + 1..]
                .iter()
                .any(|a| FIND_ACTIONS.contains(&a.as_str()))
        {
            return Err(
                "`find` with `-exec`, `-execdir`, `-ok`, `-okdir` or `-delete` does more than a \
                 grant of `find` covers"
                    .into(),
            );
        }
        if !out.contains(w) {
            out.push(w.clone());
        }
        for found in embedded_at(words, at, ctx) {
            collect_found(b, found, depth, ctx, visits, out)?;
        }
    }
    Ok(())
}

fn collect_found(
    starter: &str,
    found: Embedded,
    depth: usize,
    ctx: &ReadContext,
    visits: &mut usize,
    out: &mut Vec<String>,
) -> Result<(), String> {
    match found {
        Embedded::In(moved, inner) => collect_found(starter, *inner, depth, &moved, visits, out),
        Embedded::Argv(argv) => collect_programs_to_grant(&argv, depth + 1, ctx, visits, out),
        Embedded::Script(script) => {
            if split_script_flagged(&script).1 {
                return Err(
                    "a command is named by a substitution, so kGraf cannot tell what it runs"
                        .into(),
                );
            }
            let as_script = ["sh".to_owned(), "-c".to_owned(), script];
            match strict_script_commands(&as_script) {
                Script::Commands(commands) => {
                    // A `cd` moves the commands after it, for the files
                    // they read.
                    let mut here = ctx.clone();
                    for c in commands {
                        collect_programs_to_grant(&c, depth + 1, &here, visits, out)?;
                        if let Some(moved) = after_directory_change(&c, &here) {
                            here = moved;
                        }
                        if let Some(assigned) = after_assignment(&c, &here) {
                            here = assigned;
                        }
                    }
                    Ok(())
                }
                Script::Unjudged(why) => Err(format!(
                    "the script `{starter}` runs is not judged command by command: {why}"
                )),
                Script::NotAScript => Err(format!("`{starter}` runs a script kGraf cannot read")),
            }
        }
        Embedded::Unreadable(why) => Err(why),
    }
}

/// Split a script on command separators outside quotes.
#[cfg(test)]
fn split_script(script: &str) -> Vec<String> {
    split_script_flagged(script).0
}

/// A script split by [`split_script_parts`].
struct ScriptSplit {
    /// Each command's text, in order, with a flag for a line of a here-doc
    /// body: its quotes are literal, so a body line that does not split
    /// into words is no sign of a misread script.
    parts: Vec<(String, bool)>,
    /// A command of the script is named by a command substitution.
    by_substitution: bool,
    /// A quote, a `$'...'`, a backquote or a `$(` never closes: the shell
    /// refuses the script or reads it otherwise, so kGraf cannot tell its
    /// commands.
    unbalanced: bool,
}

/// The word a command substitution leaves in the command around it: a
/// variable, so a program or a script made of it is named by an expansion
/// ([`named_by_expansion`]).
const SUBSTITUTION_WORD: &str = "$__kgraf_substitution";

/// [`split_script`], and whether kGraf cannot tell the script's commands:
/// a command is named by a command substitution (`$(which curl) x`, ``
/// `echo curl` x ``, `env $(cmd)`), whose output is the program, or a quote
/// never closes (decision 0094, amended 2026-10-06). A substitution
/// elsewhere in a command (`echo $(date)`, `x=$(date)`) is split out and
/// read as a command of its own.
fn split_script_flagged(script: &str) -> (Vec<String>, bool) {
    let split = split_script_parts(script);
    (
        split.parts.into_iter().map(|(p, _)| p).collect(),
        split.by_substitution || split.unbalanced,
    )
}

/// One open parenthesis or backquote of [`split_script_parts`].
enum Opened {
    /// `$(` or a backquote: the text of the command around it, resumed
    /// with [`SUBSTITUTION_WORD`] when it closes, and whether double quotes
    /// were open outside it.
    Substitution {
        outer: String,
        double: bool,
        backquote: bool,
    },
    /// A plain `(`: a subshell, a function's `()`, a process substitution.
    Group { double: bool },
}

/// The delimiter of a here-doc whose `<<` is at `chars[at]`, unquoted, and
/// whether `<<-` strips leading tabs from its body, with the index after
/// the delimiter.
fn heredoc_delimiter(chars: &[char], at: usize) -> (String, bool, usize) {
    let mut j = at + 2;
    let strip = chars.get(j) == Some(&'-');
    if strip {
        j += 1;
    }
    while chars.get(j).is_some_and(|c| *c == ' ' || *c == '\t') {
        j += 1;
    }
    let mut delimiter = String::new();
    while let Some(&d) = chars.get(j) {
        if d.is_whitespace() || matches!(d, ';' | '|' | '&' | '<' | '>' | '(' | ')') {
            break;
        }
        if !matches!(d, '\'' | '"' | '\\') {
            delimiter.push(d);
        }
        j += 1;
    }
    (delimiter, strip, j)
}

/// Split a script into its commands as a POSIX shell reads it: on `;`,
/// `&`, `|`, `&&`, `||` and newlines outside quotes, with backslash escapes,
/// `$'...'`, line continuations, comments, here-doc bodies, and each
/// command substitution read as a command of its own (decision 0094,
/// amended 2026-10-06). Every part is read again as words by `shlex`.
fn split_script_parts(script: &str) -> ScriptSplit {
    let mut parts: Vec<(String, bool)> = Vec::new();
    let mut cur = String::new();
    let (mut single, mut double, mut ansi) = (false, false, false);
    let mut opened: Vec<Opened> = Vec::new();
    let mut by_substitution = false;
    // Here-docs started on the current line: each delimiter, and whether
    // `<<-` strips leading tabs from the body.
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    // Whether a substitution opening after `before` stands where the
    // command's program is: the words before it name no program yet.
    let at_program = |before: &str| -> bool {
        let mut words: Vec<String> = before.split_whitespace().map(str::to_owned).collect();
        match words.last_mut() {
            // The substitution continues the last word (`x=$(cmd)`).
            Some(last) if !before.ends_with(char::is_whitespace) => {
                last.push_str("SUBSTITUTION");
            }
            _ => words.push("SUBSTITUTION".into()),
        }
        let n = words.len() - 1;
        command_roles(&words)
            .into_iter()
            .any(|(i, role)| i == n && matches!(role, WordRole::Program | WordRole::ValueOrProgram))
    };
    let chars: Vec<char> = script.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if single {
            single = c != '\'';
            cur.push(c);
            i += 1;
            continue;
        }
        if ansi {
            cur.push(c);
            if c == '\\' {
                if let Some(&n) = chars.get(i + 1) {
                    cur.push(n);
                }
                i += 2;
                continue;
            }
            ansi = c != '\'';
            i += 1;
            continue;
        }
        match c {
            '\\' => {
                match chars.get(i + 1) {
                    // A line continuation joins the lines.
                    Some('\n') => {}
                    Some(&n) => {
                        cur.push(c);
                        cur.push(n);
                    }
                    None => cur.push(c),
                }
                i += 2;
                continue;
            }
            '$' if !double && chars.get(i + 1) == Some(&'\'') => {
                ansi = true;
                cur.push_str("$'");
                i += 2;
                continue;
            }
            '\'' if !double => single = true,
            '"' => double = !double,
            // A comment runs to the end of the line, where a word starts.
            '#' if !double && (cur.is_empty() || cur.ends_with(char::is_whitespace)) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '<' if !double
                && chars.get(i + 1) == Some(&'<')
                && chars.get(i + 2) != Some(&'<')
                && !cur.ends_with('<') =>
            {
                let (delimiter, strip, end) = heredoc_delimiter(&chars, i);
                if !delimiter.is_empty() {
                    heredocs.push((delimiter, strip));
                }
                cur.extend(&chars[i..end]);
                i = end;
                continue;
            }
            // `>&` and `<&` duplicate a descriptor (`2>&1`); they separate
            // nothing.
            '&' if !double && i > 0 && matches!(chars[i - 1], '>' | '<') => {}
            ';' | '\n' | '|' | '&' if !double => {
                parts.push((std::mem::take(&mut cur), false));
                // `&&` and `||` are one separator.
                if (c == '&' || c == '|') && chars.get(i + 1) == Some(&c) {
                    i += 1;
                }
                i += 1;
                // The bodies of the here-docs this line started. Each line
                // is read as commands too: an unquoted body runs its
                // substitutions, and a body a shell reads runs its lines.
                // Its quotes are literal, so they open nothing.
                if c == '\n' {
                    for (delimiter, strip) in std::mem::take(&mut heredocs) {
                        while i < chars.len() {
                            let end = chars[i..]
                                .iter()
                                .position(|&c| c == '\n')
                                .map_or(chars.len(), |p| i + p);
                            let line: String = chars[i..end].iter().collect();
                            i = (end + 1).min(chars.len());
                            let bare = if strip {
                                line.trim_start_matches('\t')
                            } else {
                                line.as_str()
                            };
                            if bare == delimiter {
                                break;
                            }
                            let inner = split_script_parts(&line);
                            by_substitution |= inner.by_substitution;
                            parts.extend(inner.parts.into_iter().map(|(p, _)| (p, true)));
                        }
                    }
                }
                continue;
            }
            '`' => {
                let closes = matches!(
                    opened.last(),
                    Some(Opened::Substitution {
                        backquote: true,
                        ..
                    })
                );
                if closes {
                    if let Some(Opened::Substitution {
                        outer, double: d, ..
                    }) = opened.pop()
                    {
                        parts.push((std::mem::take(&mut cur), false));
                        cur = outer;
                        cur.push_str(SUBSTITUTION_WORD);
                        double = d;
                    }
                } else {
                    if at_program(cur.trim_end_matches('"')) {
                        by_substitution = true;
                    }
                    opened.push(Opened::Substitution {
                        outer: std::mem::take(&mut cur),
                        double,
                        backquote: true,
                    });
                    double = false;
                }
                i += 1;
                continue;
            }
            '(' if !double || cur.ends_with('$') => {
                if cur.ends_with('$') {
                    // `$(cmd)`, also inside double quotes.
                    cur.pop();
                    if at_program(cur.trim_end_matches('"')) {
                        by_substitution = true;
                    }
                    opened.push(Opened::Substitution {
                        outer: std::mem::take(&mut cur),
                        double,
                        backquote: false,
                    });
                } else {
                    opened.push(Opened::Group { double });
                    parts.push((std::mem::take(&mut cur), false));
                }
                double = false;
                i += 1;
                continue;
            }
            ')' if !double => {
                match opened.pop() {
                    Some(Opened::Substitution {
                        outer,
                        double: d,
                        backquote: false,
                    }) => {
                        parts.push((std::mem::take(&mut cur), false));
                        cur = outer;
                        cur.push_str(SUBSTITUTION_WORD);
                        double = d;
                    }
                    // A `)` inside a backquote substitution closes no
                    // backquote: the substitution stays open.
                    Some(
                        frame @ Opened::Substitution {
                            backquote: true, ..
                        },
                    ) => {
                        opened.push(frame);
                        parts.push((std::mem::take(&mut cur), false));
                    }
                    Some(Opened::Group { double: d }) => {
                        parts.push((std::mem::take(&mut cur), false));
                        double = d;
                    }
                    // A `case` pattern's `)`.
                    None => parts.push((std::mem::take(&mut cur), false)),
                }
                i += 1;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        i += 1;
    }
    parts.push((cur, false));
    let unbalanced = single
        || double
        || ansi
        || opened
            .iter()
            .any(|o| matches!(o, Opened::Substitution { .. }));
    let parts = parts
        .into_iter()
        .map(|(p, body)| (p.trim().to_owned(), body))
        .filter(|(p, _)| !p.is_empty())
        .collect();
    ScriptSplit {
        parts,
        by_substitution,
        unbalanced,
    }
}

/// The program a simple command runs: its first word that is not a
/// `VAR=value` assignment or shell syntax ([`COMMAND_SYNTAX`]).
fn program_of_command(words: &[String]) -> Option<&str> {
    words
        .iter()
        .map(String::as_str)
        .find(|w| !is_assignment(w) && !COMMAND_SYNTAX.contains(w))
}

/// Words of shell syntax that may stand before a command's program: a group
/// or a subshell opened or closed, `!`, and the keywords that begin a clause
/// (`if curl x; then curl y; fi` runs `curl` twice). Never a program.
const COMMAND_SYNTAX: &[&str] = &[
    "{", "}", "(", ")", "!", "if", "then", "else", "elif", "while", "until", "do", "time",
    "coproc", "repeat",
];

/// Leading words that run the rest of the command as a command: as another
/// process (`env`, `nohup`, `sudo`, `timeout`), in place of the shell
/// (`exec`), or skipping functions and aliases (`command`, `builtin`).
const WRAPPERS: &[&str] = &[
    "sudo",
    "doas",
    "env",
    "command",
    "builtin",
    "exec",
    "nice",
    "nohup",
    "time",
    "busybox",
    "xargs",
    "timeout",
    "stdbuf",
    "setsid",
    "ionice",
    "chrt",
    "taskset",
    "unbuffer",
    "caffeinate",
    "chroot",
    "flock",
    "noglob",
    "nocorrect",
    "watch",
    "strace",
    "ltrace",
    "valgrind",
    "parallel",
    "runuser",
    "pkexec",
    "unshare",
    "nsenter",
    "setpriv",
    "firejail",
    "torsocks",
    "proxychains",
    "proxychains4",
    "sandbox-exec",
    "arch",
    "systemd-run",
    "dbus-run-session",
    "faketime",
    "cross-env",
    "xcrun",
    "rlwrap",
    "eatmydata",
];

/// Wrappers whose first operand comes before the command they run
/// (`timeout 5s curl`, `chroot /srv curl`, `flock /tmp/l curl`, `faketime
/// 2020-01-01 curl`).
const OPERAND_WRAPPERS: &[&str] = &["timeout", "chroot", "flock", "taskset", "chrt", "faketime"];

/// The text after a redirection operator at the start of `word` (`2>`,
/// `>>`, `&>`, `<`, `>&`): empty when the target is the next word, `None`
/// when `word` is not a redirection.
fn redirection(word: &str) -> Option<&str> {
    // A descriptor number (`2>`) or a variable bash stores it in (`{fd}>`).
    let rest = match word.strip_prefix('{').and_then(|r| r.split_once('}')) {
        Some((name, rest)) if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => rest,
        _ => word.trim_start_matches(|c: char| c.is_ascii_digit()),
    };
    if !(rest.starts_with('>') || rest.starts_with('<') || rest.starts_with("&>")) {
        return None;
    }
    Some(rest.trim_start_matches(['<', '>', '&', '|']))
}

/// Every place in a simple command where its program may stand: index 0,
/// then each word after a leading prefix word. Prefix words are shell syntax
/// ([`COMMAND_SYNTAX`]: `{`, `(`, `!`, `then`), wrappers ([`WRAPPERS`]:
/// `env`, `command`, `builtin`, `exec`, `nohup`, `time`), a wrapper's
/// operand (`timeout 5s`), options and the value an option may take, numbers,
/// `VAR=value` assignments and redirections (`2>/dev/null curl`).
///
/// Where a word could be an option's value or the program (`sudo -u bob
/// curl`, `sudo -E curl`), both places are listed, and the list goes one word
/// past the first word that is surely not a prefix: a rule or a limit that
/// matches any listed place applies, which errs toward refusing. One list
/// serves the user's rules ([`subjects`]), the deletion limit
/// ([`program_index`]), the shell a script starts ([`simple_commands`]) and
/// the command policy's `forbid` rules, so a prefix word hides a program from
/// none of them.
pub(crate) fn command_starts<S: AsRef<str>>(words: &[S]) -> Vec<usize> {
    command_roles(words).into_iter().map(|(i, _)| i).collect()
}

/// [`command_starts`] with what each listed word is ([`WordRole`]). The last
/// entry is the program ([`WordRole::Program`]) unless every word is a prefix
/// word.
fn command_roles<S: AsRef<str>>(words: &[S]) -> Vec<(usize, WordRole)> {
    let mut roles = Vec::new();
    // Local mutable state, scoped to one scan: what the previous word asked
    // of this one.
    let (mut operand_due, mut value_due, mut target_due) = (false, false, false);
    for (i, w) in words.iter().enumerate() {
        let w = w.as_ref();
        let b = base_name(w);
        let role = if target_due {
            target_due = false;
            WordRole::Prefix
        } else if COMMAND_SYNTAX.contains(&w) {
            WordRole::Prefix
        } else if w == "function" {
            // `function NAME { ... }`: the name is no program.
            target_due = true;
            WordRole::Prefix
        } else if let Some(rest) = redirection(w) {
            target_due = rest.is_empty();
            WordRole::Prefix
        } else if w.contains('=') {
            // `VAR=value`, and anything else holding `=` (`--opt=v`).
            WordRole::Prefix
        } else if WRAPPERS.contains(&b) {
            operand_due = OPERAND_WRAPPERS.contains(&b);
            value_due = false;
            WordRole::Wrapper
        } else if w.starts_with('-') {
            value_due = true;
            WordRole::Prefix
        } else if value_due {
            value_due = false;
            WordRole::ValueOrProgram
        } else if w.parse::<f64>().is_ok() {
            // A number is a wrapper's operand when one is due
            // (`timeout 5 curl`).
            operand_due = false;
            WordRole::Prefix
        } else if std::mem::take(&mut operand_due) {
            // A wrapper's operand (`timeout 5s`), once.
            WordRole::Prefix
        } else {
            WordRole::Program
        };
        roles.push((i, role));
        if role == WordRole::Program {
            break;
        }
    }
    roles
}

/// Whether a command names a directory changer ([`DIRECTORY_CHANGERS`]) in
/// any word: `command cd x`, `builtin cd x` and `exec pushd x` move the
/// shell as surely as `cd x`. Deliberately broad (`echo cd` counts): it only
/// makes a reader refuse a redirection it would otherwise judge.
fn names_directory_change(words: &[String]) -> bool {
    words
        .iter()
        .any(|w| DIRECTORY_CHANGERS.contains(&base_name(w)))
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Every program a shell call runs: the call's own program and, for
/// `sh -c SCRIPT`, the program of each command of the script
/// ([`simple_commands`]). Distinct, in order.
pub fn programs_run(input: &serde_json::Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for command in simple_commands(&shell_argv(input)) {
        if let Some(p) = program_of_command(&command)
            && !out.iter().any(|o| o == p)
        {
            out.push(p.to_owned());
        }
    }
    out
}

/// What a policy refused when it refused a call ([`refused_subject`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefusedSubject {
    /// The tool itself: a skill's `allowed-tools` (decision 0053), or a user
    /// rule on every call of the tool (`shell`, `*`).
    Tool,
    /// The program of one command a rule, the deletion limit or the mode's
    /// command policy matched (`sh -c "mkdir x && python3 y"` refused by
    /// `shell(python3*)` is a refusal of `python3`, not of `sh`).
    Program(String),
    /// A path a user rule on a file tool matched (`apply_patch(secrets/*)`).
    Path(String),
}

/// How the mode's command policy names the command of a script that decided
/// its verdict: ``the script runs `PROGRAM` (`COMMAND`)``
/// ([`crate::execpolicy::ExecPolicy::evaluate`]).
pub const SCRIPT_RUNS_LEAD: &str = "script runs `";

/// How the mode's command policy names the program of a command that still
/// lacks a grant while a program it runs has one (`env curl x` with `env`
/// granted): ``the command runs `PROGRAM` (`COMMAND`)``, the innermost
/// program first (decision 0094, amended 2026-10-06).
pub const COMMAND_RUNS_LEAD: &str = "command runs `";

/// What the policy `by` refused when it refused a call of `tool` with
/// `input`, giving `reason`; `None` when the call's own capability stands
/// (a shell call's program, another tool's name).
///
/// Only a refusal a command-level match produced names another program:
/// a user deny or ask rule ([`PERMISSION_RULES_HOOK`], or the user's denial of
/// that rule's ask), the deletion limit, or the mode's command policy naming
/// the command of a script it judged ([`SCRIPT_RUNS_LEAD`]). A skill's
/// refusal is of the tool whatever its reason quotes after it (decision 0053:
/// `allowed-tools` restricts tools), and a rule with no pattern is a rule on
/// the tool. A rule's or the deletion limit's text counts only where it
/// begins the reason, which is where those policies put it; the command
/// policy's lead is read from its last occurrence, after the model-written
/// command line the question quotes first.
pub fn refused_subject(
    tool: &str,
    input: &serde_json::Value,
    by: &str,
    reason: &str,
) -> Option<RefusedSubject> {
    refused_subject_of(tool, &subjects(tool, input), by, reason)
}

/// [`refused_subject`] for a call whose rule subjects ([`subjects`]: a
/// shell call's command lines, the whole call first; a file tool's paths)
/// were kept instead of its input.
pub fn refused_subject_of(
    tool: &str,
    subjects: &[String],
    by: &str,
    reason: &str,
) -> Option<RefusedSubject> {
    if by == crate::approval::SKILLS_POLICY {
        return Some(RefusedSubject::Tool);
    }
    let program_of_line = |line: &str| {
        line.split_whitespace()
            .find(|w| !is_assignment(w) && !COMMAND_SYNTAX.contains(w))
            .map(str::to_owned)
    };
    let from_rules = by == PERMISSION_RULES_HOOK || by == crate::approval::USER;
    if from_rules && let Some(text) = rule_beginning(reason) {
        let rule = Rule::parse(text).ok()?;
        if rule.pattern.is_none() {
            return Some(RefusedSubject::Tool);
        }
        let matched = subjects
            .iter()
            .find(|s| rule.matches(tool, std::slice::from_ref(*s)))?;
        return if tool == "shell" {
            program_of_line(matched).map(RefusedSubject::Program)
        } else {
            Some(RefusedSubject::Path(matched.clone()))
        };
    }
    if tool != "shell" {
        return None;
    }
    if from_rules && (reason.starts_with(DELETING_LEAD) || reason.starts_with(DELETES_LEAD)) {
        return subjects.iter().find_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            program_index(&words).map(|at| RefusedSubject::Program(words[at].to_owned()))
        });
    }
    if by == crate::shell::SHELL_POLICY_HOOK || by == crate::approval::USER {
        let lead = [SCRIPT_RUNS_LEAD, COMMAND_RUNS_LEAD]
            .into_iter()
            .filter_map(|l| reason.rfind(l).map(|at| (at, l)))
            .max_by_key(|(at, _)| *at)?;
        let rest = &reason[lead.0 + lead.1.len()..];
        let (program, _) = rest.split_once('`')?;
        return is_plain_program(program).then(|| RefusedSubject::Program(program.to_owned()));
    }
    None
}

/// The rule text a reason begins with (`your deny rule `R` ...`, `your ask
/// rule `R` ...`), as [`hard_limit`] words it.
fn rule_beginning(reason: &str) -> Option<&str> {
    ["your deny rule `", "your ask rule `"]
        .iter()
        .find_map(|lead| reason.strip_prefix(lead))
        .and_then(|rest| rest.split_once('`'))
        .map(|(rule, _)| rule)
}

/// Shells whose `-c` script a permission mode judges command by command
/// ([`script_commands`]).
pub const SCRIPT_SHELLS: &[&str] = &["sh", "bash", "zsh", "dash"];

/// A shell call read as a script of simple commands ([`script_commands`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Script {
    /// Not `SHELL -c SCRIPT` for a shell of [`SCRIPT_SHELLS`].
    NotAScript,
    /// A script holding a construct the per-command check does not judge,
    /// and which.
    Unjudged(String),
    /// Simple commands only, each as its words, in order.
    Commands(Vec<Vec<String>>),
}

/// Read a shell call's argv as a script a mode may grant command by command
/// (decision 0094, amended 2026-10-02): `["sh", "-c", SCRIPT]` (or `bash`,
/// `zsh`, `dash`, by bare name, with exactly the `-c` option and no
/// arguments after the script) whose script is only simple commands joined by
/// `;`, `&&`, `||`, `|` or newlines.
///
/// Each command starts with a plain program name (letters, digits, `.`,
/// `_`, `+`, `-`; no path, quote, assignment or glob). Quoted arguments and
/// argument globs are allowed. Refused as [`Script::Unjudged`]: command or
/// variable expansion (`$`, backquote), subshells and groups (`(`, `{`),
/// backgrounding (`&`, `|&`), input redirection, backslash escapes, comments,
/// `!`, shell keywords, an operator with no command on one side, and an
/// output redirection whose target is not `/dev/null` or a worktree path a
/// worker may write ([`kgraf_core::paths::forbidden_reason`], lexically inside
/// the worktree, no glob or `~`), or that follows a `cd`. The commands are
/// checked against [`simple_commands`], which the user's rules match, so a
/// grant and a rule never judge different commands.
pub fn script_commands(argv: &[String]) -> Script {
    script_commands_with(argv, &ScriptConstructs::default())
}

/// [`script_commands`], where a script the strict reader refuses is read
/// again by a reader that also accepts the constructs the user turned on
/// (decision 0094, amended 2026-10-02). With none on, it is
/// [`script_commands`] exactly.
///
/// The second reader returns the commands whose program it can see, the
/// commands inside substitutions, subshells and groups included; a command
/// named by a variable or a substitution has no program to check and is left
/// out, which is what the user accepted by turning variables on. A script
/// with no program it can see at all is not judged. The strict reader's
/// cross-check against [`simple_commands`] does not apply to it: that split
/// does not follow these constructs, so the user's rules are also matched by
/// program name against the script text ([`construct_rule_limit`]).
pub fn script_commands_with(argv: &[String], constructs: &ScriptConstructs) -> Script {
    match strict_script_commands(argv) {
        Script::Unjudged(why) if !constructs.is_empty() => match script_text(argv) {
            Some(script) => match RelaxedReader::new(script, constructs).read() {
                Ok(commands) => Script::Commands(commands),
                Err(relaxed_why) => Script::Unjudged(relaxed_why),
            },
            None => Script::Unjudged(why),
        },
        other => other,
    }
}

/// Whether the call is a script a mode judges only because of a construct
/// the user turned on: the strict reader refuses it and the second reader
/// accepts it ([`script_commands_with`]).
pub fn relies_on_constructs(argv: &[String], constructs: &ScriptConstructs) -> bool {
    !constructs.is_empty()
        && matches!(strict_script_commands(argv), Script::Unjudged(_))
        && matches!(script_commands_with(argv, constructs), Script::Commands(_))
}

/// The script of `[SHELL, "-c", SCRIPT]` for a shell of [`SCRIPT_SHELLS`]
/// named bare; `None` for any other shape.
fn script_text(argv: &[String]) -> Option<&str> {
    match argv {
        [shell, flag, script] if flag == "-c" && SCRIPT_SHELLS.contains(&shell.as_str()) => {
            Some(script)
        }
        _ => None,
    }
}

/// The strict reading behind [`script_commands`].
fn strict_script_commands(argv: &[String]) -> Script {
    let Some(shell) = argv.first() else {
        return Script::NotAScript;
    };
    if !SCRIPT_SHELLS.contains(&base_name(shell)) {
        return Script::NotAScript;
    }
    if !SCRIPT_SHELLS.contains(&shell.as_str()) {
        return Script::Unjudged(format!(
            "the shell is named by the path `{}`",
            shell.escape_debug()
        ));
    }
    let script = match argv {
        [_, flag, script] if flag == "-c" => script,
        [_, flag, _, _, ..] if flag == "-c" => {
            return Script::Unjudged("arguments follow the script".into());
        }
        _ => return Script::Unjudged("the shell is given options other than `-c`".into()),
    };
    let commands = match ScriptReader::default().read(script) {
        Ok(c) => c,
        Err(why) => return Script::Unjudged(why),
    };
    // The user's rules match `simple_commands`; a script it splits otherwise
    // is not judged here. A command another program starts from its
    // arguments (`find -exec`, `su -c`) is not a command of this shell: the
    // grant of the command that starts it covers it ([`programs_to_grant`]).
    // A nested shell's commands are, and leave the script unjudged.
    let theirs: Vec<Option<String>> = read_call(argv)
        .commands
        .iter()
        .skip(1)
        .filter(|c| c.same_shell)
        .map(|c| program_of_command(&c.words).map(str::to_owned))
        .collect();
    let mine: Vec<Option<String>> = commands.iter().map(|c| c.first().cloned()).collect();
    if theirs != mine {
        return Script::Unjudged("its commands could not be told apart reliably".into());
    }
    Script::Commands(commands)
}

/// A program name a script command may start with: no path, quote, glob,
/// assignment or control character.
fn is_plain_program(word: &str) -> bool {
    !word.is_empty()
        && !word.starts_with('-')
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// Words a script's command may not start with: shell syntax, not programs.
const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "select", "time", "coproc", "in",
];

/// Builtins that change the directory later redirections resolve against.
const DIRECTORY_CHANGERS: &[&str] = &["cd", "pushd", "popd"];

/// One word being read: its text, whether any of it was quoted, and whether
/// an unquoted part holds a glob, `~` or `=`.
#[derive(Default)]
struct ScriptWord {
    text: String,
    quoted: bool,
    special: bool,
}

/// The strict reader behind [`script_commands`]. Local mutable state, scoped
/// to one read: a character loop is clearer here than a combinator parser.
#[derive(Default)]
struct ScriptReader {
    commands: Vec<Vec<String>>,
    words: Vec<String>,
    word: Option<ScriptWord>,
    /// The next word is an output redirection's target.
    target_next: bool,
    /// `&&`, `||` or `|` was read: a command must follow.
    operator_pending: bool,
    redirects_to_file: bool,
    changes_directory: bool,
}

impl ScriptReader {
    fn read(mut self, script: &str) -> Result<Vec<Vec<String>>, String> {
        let chars: Vec<char> = script.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            match c {
                ' ' | '\t' => self.end_word()?,
                '\n' => self.end_command(false)?,
                ';' => {
                    if next == Some(';') {
                        return Err("it holds `;;`".into());
                    }
                    self.end_command(true)?;
                }
                '&' if next == Some('&') => {
                    self.end_operator()?;
                    i += 1;
                }
                '&' => return Err("`&` runs a command in the background".into()),
                '|' if next == Some('|') => {
                    self.end_operator()?;
                    i += 1;
                }
                '|' if next == Some('&') => return Err("it holds `|&`".into()),
                '|' => self.end_operator()?,
                '>' => i = self.redirect(&chars, i)?,
                '<' => return Err("it redirects input".into()),
                '$' | '`' => {
                    return Err("it expands a variable or substitutes a command".into());
                }
                '(' | ')' => return Err("it opens a subshell or a group".into()),
                '{' | '}' => return Err("it holds a brace group or brace expansion".into()),
                '\\' => return Err("it holds a backslash escape".into()),
                '#' => return Err("it holds a comment".into()),
                '!' => return Err("it holds `!`".into()),
                '\'' => {
                    let end = chars[i + 1..]
                        .iter()
                        .position(|&q| q == '\'')
                        .ok_or("a quote is not closed")?;
                    let w = self.word.get_or_insert_with(ScriptWord::default);
                    w.text.extend(&chars[i + 1..i + 1 + end]);
                    w.quoted = true;
                    i += end + 1;
                }
                '"' => {
                    let end = chars[i + 1..]
                        .iter()
                        .position(|&q| q == '"')
                        .ok_or("a quote is not closed")?;
                    let inner = &chars[i + 1..i + 1 + end];
                    if inner.iter().any(|q| matches!(q, '$' | '`' | '\\')) {
                        return Err("a double-quoted word expands, substitutes or escapes".into());
                    }
                    let w = self.word.get_or_insert_with(ScriptWord::default);
                    w.text.extend(inner);
                    w.quoted = true;
                    i += end + 1;
                }
                c if c.is_control() => return Err("it holds a control character".into()),
                c => {
                    let w = self.word.get_or_insert_with(ScriptWord::default);
                    w.text.push(c);
                    if matches!(c, '*' | '?' | '[' | ']' | '~' | '=') {
                        w.special = true;
                    }
                }
            }
            i += 1;
        }
        self.end_command(false)?;
        if self.operator_pending {
            return Err("an operator has no command after it".into());
        }
        if self.commands.is_empty() {
            return Err("it runs no command".into());
        }
        if self.redirects_to_file && self.changes_directory {
            return Err("it redirects to a file after changing directory".into());
        }
        Ok(self.commands)
    }

    /// An output redirection at `chars[at]` (`>`, `>>`, `N>`, `>&N`).
    /// Returns the index of its last character.
    fn redirect(&mut self, chars: &[char], at: usize) -> Result<usize, String> {
        if self.target_next {
            return Err("a redirection names no file".into());
        }
        // `2>`: the digits before `>` are the descriptor, not a word.
        match &self.word {
            Some(w) if !w.quoted && w.text.chars().all(|c| c.is_ascii_digit()) => {
                self.word = None;
            }
            _ => self.end_word()?,
        }
        if self.words.is_empty() {
            return Err("a redirection comes before its command's program".into());
        }
        let mut i = at;
        if chars.get(i + 1) == Some(&'>') {
            i += 1;
        }
        match chars.get(i + 1) {
            Some('|') => Err("it holds `>|`".into()),
            Some('&') => {
                // `>&N` or `>&-` duplicates or closes a descriptor.
                let digits = chars[i + 2..]
                    .iter()
                    .take_while(|c| c.is_ascii_digit() || **c == '-')
                    .count();
                if digits == 0 {
                    return Err("it redirects with `>&` to a file".into());
                }
                Ok(i + 1 + digits)
            }
            _ => {
                self.target_next = true;
                Ok(i)
            }
        }
    }

    fn end_word(&mut self) -> Result<(), String> {
        let Some(w) = self.word.take() else {
            return Ok(());
        };
        if self.target_next {
            self.target_next = false;
            return self.check_target(&w);
        }
        if self.words.is_empty() {
            if w.quoted || w.special || !is_plain_program(&w.text) {
                return Err(format!(
                    "the command `{}` does not start with a plain program name",
                    w.text.escape_debug()
                ));
            }
            if SHELL_KEYWORDS.contains(&w.text.as_str()) {
                return Err(format!("it uses the shell keyword `{}`", w.text));
            }
        }
        self.words.push(w.text);
        Ok(())
    }

    fn check_target(&mut self, w: &ScriptWord) -> Result<(), String> {
        if check_redirect_target(&w.text, w.special)? {
            self.redirects_to_file = true;
        }
        Ok(())
    }

    /// `;` (`explicit`) or a newline ends a command. An empty command is
    /// allowed only at a newline or the end, never before `;` or after an
    /// operator.
    fn end_command(&mut self, explicit: bool) -> Result<(), String> {
        self.end_word()?;
        if self.target_next {
            return Err("a redirection names no file".into());
        }
        if self.words.is_empty() {
            if explicit {
                return Err("an operator has no command on one side".into());
            }
            return Ok(());
        }
        self.push_command();
        self.operator_pending = false;
        Ok(())
    }

    fn end_operator(&mut self) -> Result<(), String> {
        self.end_word()?;
        if self.target_next || self.words.is_empty() {
            return Err("an operator has no command on one side".into());
        }
        self.push_command();
        self.operator_pending = true;
        Ok(())
    }

    /// Keep the command just read. Any word naming a directory changer counts
    /// as a `cd` ([`names_directory_change`]), not only the first: `command
    /// cd x` moves the shell as surely as `cd x`.
    fn push_command(&mut self) {
        if names_directory_change(&self.words) {
            self.changes_directory = true;
        }
        self.commands.push(std::mem::take(&mut self.words));
    }
}

/// Check an output redirection's target `t` (`special`: an unquoted part
/// holds a glob, `~` or `=`): `/dev/null`, or a worktree path a worker may
/// write ([`kgraf_core::paths::forbidden_reason`]). `Ok(true)` when it
/// writes a file.
fn check_redirect_target(t: &str, special: bool) -> Result<bool, String> {
    if special {
        return Err(format!(
            "a redirection target `{}` holds a glob, `~` or `=`",
            t.escape_debug()
        ));
    }
    if t == "/dev/null" {
        return Ok(false);
    }
    let path = crate::normalize_relative(Path::new(t)).map_err(|_| {
        format!(
            "a redirection writes `{}`, outside the worktree",
            t.escape_debug()
        )
    })?;
    if let Some(why) = kgraf_core::paths::forbidden_reason(&path) {
        return Err(format!(
            "a redirection writes `{}`, which a worker may not write: {why}",
            t.escape_debug()
        ));
    }
    Ok(true)
}

/// What closes the command list being read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Close {
    End,
    /// `)` of a subshell, `$(...)` or `<(...)`.
    Paren,
    /// `}` of a brace group.
    Brace,
}

/// One word read by [`RelaxedReader`]: its text with quotes and escapes
/// removed (an expansion adds nothing), whether any of it was quoted or
/// escaped, whether an unquoted part holds a glob, brace, `~` or `=`, and
/// whether it holds an expansion kGraf cannot resolve.
#[derive(Default)]
struct RelaxedWord {
    text: String,
    quoted: bool,
    special: bool,
    opaque: bool,
}

/// What the next word of a command is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NextWord {
    Output,
    Input,
    Heredoc { strip_tabs: bool },
}

/// A heredoc whose body follows the line it was opened on.
struct PendingHeredoc {
    delimiter: String,
    strip_tabs: bool,
    expands: bool,
}

/// One command being read by [`RelaxedReader`].
#[derive(Default)]
struct RelaxedCommand {
    /// The program and its arguments, when the program is visible.
    words: Vec<String>,
    started: bool,
    program_seen: bool,
    visible: bool,
    group: bool,
    next: Option<NextWord>,
}

impl RelaxedCommand {
    fn at_start(&self) -> bool {
        !self.started
    }
}

/// The second script reader of [`script_commands_with`]: the strict reader's
/// grammar plus the constructs the user turned on, each refused with the
/// strict reader's reason while it is off. Recursive over substitutions,
/// subshells and groups, so the commands inside them are returned too.
/// Local mutable state, scoped to one read, as in [`ScriptReader`].
///
/// Searched before writing: a full POSIX or bash parser crate
/// (`brush-parser`, `conch-parser`, `yash-syntax`) would read more than this
/// needs and would still have to be mapped onto "which programs can kGraf
/// see"; this reader only has to find program names and refuse what it
/// cannot place, so it stays a small, auditable character loop.
struct RelaxedReader<'a> {
    chars: Vec<char>,
    pos: usize,
    allow: &'a ScriptConstructs,
    commands: Vec<Vec<String>>,
    heredocs: Vec<PendingHeredoc>,
    redirects_to_file: bool,
    changes_directory: bool,
    unseen: bool,
}

/// Characters that end an unquoted word.
fn ends_word(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
    )
}

impl<'a> RelaxedReader<'a> {
    fn new(script: &str, allow: &'a ScriptConstructs) -> Self {
        Self {
            chars: script.chars().collect(),
            pos: 0,
            allow,
            commands: Vec::new(),
            heredocs: Vec::new(),
            redirects_to_file: false,
            changes_directory: false,
            unseen: false,
        }
    }

    fn read(mut self) -> Result<Vec<Vec<String>>, String> {
        self.list(Close::End)?;
        if !self.heredocs.is_empty() {
            return Err("a heredoc has no body".into());
        }
        if self.commands.is_empty() {
            return Err(if self.unseen {
                "kGraf can see no program it runs".into()
            } else {
                "it runs no command".into()
            });
        }
        if !self.allow.allows(ScriptConstruct::OutsideRedirects)
            && self.redirects_to_file
            && self.changes_directory
        {
            return Err("it redirects to a file after changing directory".into());
        }
        Ok(self.commands)
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.chars.get(self.pos + n).copied()
    }

    fn need(&self, c: ScriptConstruct, why: &str) -> Result<(), String> {
        if self.allow.allows(c) {
            Ok(())
        } else {
            Err(why.to_owned())
        }
    }

    /// Read commands until `close` (consumed).
    fn list(&mut self, close: Close) -> Result<(), String> {
        let mut cmd = RelaxedCommand::default();
        let mut operator_pending = false;
        loop {
            let Some(c) = self.peek() else {
                if close != Close::End {
                    return Err("a subshell, group or substitution is not closed".into());
                }
                self.end_command(&mut cmd, false, &mut operator_pending)?;
                if operator_pending {
                    return Err("an operator has no command after it".into());
                }
                return Ok(());
            };
            let next = self.peek_at(1);
            match c {
                ' ' | '\t' => self.pos += 1,
                '\\' if next == Some('\n') => {
                    self.need(ScriptConstruct::Escapes, "it holds a backslash escape")?;
                    self.pos += 2;
                }
                '\n' => {
                    self.pos += 1;
                    self.end_command(&mut cmd, false, &mut operator_pending)?;
                    self.heredoc_bodies()?;
                }
                ';' => {
                    if next == Some(';') {
                        return Err("it holds `;;`".into());
                    }
                    self.pos += 1;
                    self.end_command(&mut cmd, true, &mut operator_pending)?;
                }
                '&' if next == Some('&') => {
                    self.pos += 2;
                    self.end_operator(&mut cmd, &mut operator_pending)?;
                }
                // `&>` and `&>>` write stdout and stderr to a file.
                '&' if next == Some('>') => {
                    self.pos += 1;
                    self.redirect_out(&mut cmd)?;
                }
                '&' => {
                    self.need(
                        ScriptConstruct::Background,
                        "`&` runs a command in the background",
                    )?;
                    self.pos += 1;
                    self.end_command(&mut cmd, true, &mut operator_pending)?;
                }
                '|' if next == Some('|') => {
                    self.pos += 2;
                    self.end_operator(&mut cmd, &mut operator_pending)?;
                }
                '|' if next == Some('&') => {
                    self.need(ScriptConstruct::Background, "it holds `|&`")?;
                    self.pos += 2;
                    self.end_operator(&mut cmd, &mut operator_pending)?;
                }
                '|' => {
                    self.pos += 1;
                    self.end_operator(&mut cmd, &mut operator_pending)?;
                }
                // Process substitution runs a command, like `$(...)`.
                '<' | '>' if next == Some('(') => {
                    self.need(
                        ScriptConstruct::Variables,
                        "it expands a variable or substitutes a command",
                    )?;
                    self.pos += 2;
                    self.list(Close::Paren)?;
                    let w = RelaxedWord {
                        opaque: true,
                        ..RelaxedWord::default()
                    };
                    self.push_word(&mut cmd, w)?;
                }
                '>' => self.redirect_out(&mut cmd)?,
                '<' => self.redirect_in(&mut cmd)?,
                '(' => {
                    self.need(ScriptConstruct::Subshells, "it opens a subshell or a group")?;
                    if !cmd.at_start() {
                        return Err(
                            "it defines a function or opens a subshell inside a command".into()
                        );
                    }
                    self.pos += 1;
                    self.list(Close::Paren)?;
                    cmd.started = true;
                    cmd.group = true;
                }
                ')' => {
                    if close != Close::Paren {
                        return Err("it opens a subshell or a group".into());
                    }
                    self.pos += 1;
                    self.end_command(&mut cmd, false, &mut operator_pending)?;
                    if operator_pending {
                        return Err("an operator has no command after it".into());
                    }
                    return Ok(());
                }
                '{' if cmd.at_start()
                    && next.is_none_or(|n| n == ' ' || n == '\t' || n == '\n') =>
                {
                    self.need(
                        ScriptConstruct::Subshells,
                        "it holds a brace group or brace expansion",
                    )?;
                    self.pos += 1;
                    self.list(Close::Brace)?;
                    cmd.started = true;
                    cmd.group = true;
                }
                '}' if close == Close::Brace && cmd.at_start() && next.is_none_or(ends_word) => {
                    self.pos += 1;
                    self.end_command(&mut cmd, false, &mut operator_pending)?;
                    if operator_pending {
                        return Err("an operator has no command after it".into());
                    }
                    return Ok(());
                }
                '#' => {
                    self.need(ScriptConstruct::Escapes, "it holds a comment")?;
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.pos += 1;
                    }
                }
                c if c.is_ascii_digit() && self.descriptor_then_redirect() => {
                    // `2>`, `0<`: the digits are a descriptor, not a word.
                    while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                        self.pos += 1;
                    }
                }
                _ => {
                    let w = self.word()?;
                    self.push_word(&mut cmd, w)?;
                }
            }
        }
    }

    /// Digits at the read position directly followed by `>` or `<`.
    fn descriptor_then_redirect(&self) -> bool {
        let digits = self.chars[self.pos..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        matches!(self.peek_at(digits), Some('>' | '<'))
    }

    /// An output redirection at `>` (`>`, `>>`, `>|`, `>&N`, `>&-`, `>&FILE`).
    fn redirect_out(&mut self, cmd: &mut RelaxedCommand) -> Result<(), String> {
        if cmd.next.is_some() {
            return Err("a redirection names no file".into());
        }
        cmd.started = true;
        let mut i = self.pos + 1;
        if self.chars.get(i) == Some(&'>') {
            i += 1;
        }
        match self.chars.get(i) {
            Some('|') => {
                self.need(ScriptConstruct::OutsideRedirects, "it holds `>|`")?;
                i += 1;
            }
            Some('&') => {
                let n = self.chars[i + 1..]
                    .iter()
                    .take_while(|c| c.is_ascii_digit() || **c == '-')
                    .count();
                if n > 0 {
                    self.pos = i + 1 + n;
                    return Ok(());
                }
                self.need(
                    ScriptConstruct::OutsideRedirects,
                    "it redirects with `>&` to a file",
                )?;
                i += 1;
            }
            _ => {}
        }
        self.pos = i;
        cmd.next = Some(NextWord::Output);
        Ok(())
    }

    /// An input redirection at `<` (`<`, `<&N`, `<<`, `<<-`, `<<<`, `<>`).
    fn redirect_in(&mut self, cmd: &mut RelaxedCommand) -> Result<(), String> {
        self.need(ScriptConstruct::InputRedirects, "it redirects input")?;
        if cmd.next.is_some() {
            return Err("a redirection names no file".into());
        }
        cmd.started = true;
        let mut i = self.pos + 1;
        let next = match self.chars.get(i) {
            Some('<') if self.chars.get(i + 1) == Some(&'<') => {
                i += 2;
                NextWord::Input
            }
            Some('<') => {
                i += 1;
                let strip_tabs = self.chars.get(i) == Some(&'-');
                if strip_tabs {
                    i += 1;
                }
                NextWord::Heredoc { strip_tabs }
            }
            Some('&') => {
                let n = self.chars[i + 1..]
                    .iter()
                    .take_while(|c| c.is_ascii_digit() || **c == '-')
                    .count();
                if n == 0 {
                    return Err("it redirects input with `<&` to a word".into());
                }
                self.pos = i + 1 + n;
                return Ok(());
            }
            // `<>` opens the file for writing too.
            Some('>') => {
                i += 1;
                NextWord::Output
            }
            _ => NextWord::Input,
        };
        self.pos = i;
        cmd.next = Some(next);
        Ok(())
    }

    fn push_word(&mut self, cmd: &mut RelaxedCommand, w: RelaxedWord) -> Result<(), String> {
        cmd.started = true;
        if let Some(next) = cmd.next.take() {
            return match next {
                NextWord::Output => self.check_target(&w),
                NextWord::Input => Ok(()),
                NextWord::Heredoc { strip_tabs } => {
                    if w.opaque {
                        return Err("a heredoc's delimiter expands".into());
                    }
                    self.heredocs.push(PendingHeredoc {
                        delimiter: w.text,
                        strip_tabs,
                        expands: !w.quoted,
                    });
                    Ok(())
                }
            };
        }
        if cmd.group {
            return Err("a word follows a subshell or group".into());
        }
        if cmd.program_seen {
            if cmd.visible {
                cmd.words.push(w.text);
            }
            return Ok(());
        }
        let not_plain = || {
            format!(
                "the command `{}` does not start with a plain program name",
                w.text.escape_debug()
            )
        };
        // `! cmd` negates the command's status.
        if !w.quoted && !w.opaque && w.text == "!" {
            return self.need(ScriptConstruct::Escapes, "it holds `!`");
        }
        if !w.quoted && is_assignment(&w.text) {
            return self.need(ScriptConstruct::Escapes, &not_plain());
        }
        cmd.program_seen = true;
        if w.opaque {
            // Named by a variable or a substitution: nothing to check.
            self.unseen = true;
            return Ok(());
        }
        if w.special
            || w.text.is_empty()
            || w.text.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(not_plain());
        }
        if w.quoted || !is_plain_program(&w.text) {
            self.need(ScriptConstruct::Escapes, &not_plain())?;
        }
        if !w.quoted && SHELL_KEYWORDS.contains(&w.text.as_str()) {
            return Err(format!("it uses the shell keyword `{}`", w.text));
        }
        cmd.visible = true;
        cmd.words.push(w.text);
        Ok(())
    }

    fn check_target(&mut self, w: &RelaxedWord) -> Result<(), String> {
        if self.allow.allows(ScriptConstruct::OutsideRedirects) {
            return Ok(());
        }
        if w.opaque {
            return Err(format!(
                "a redirection writes `{}`, a path kGraf cannot resolve",
                w.text.escape_debug()
            ));
        }
        if check_redirect_target(&w.text, w.special)? {
            self.redirects_to_file = true;
        }
        Ok(())
    }

    fn finish(&mut self, cmd: &mut RelaxedCommand) {
        let done = std::mem::take(cmd);
        if done.visible {
            if names_directory_change(&done.words) {
                self.changes_directory = true;
            }
            self.commands.push(done.words);
        }
    }

    fn end_command(
        &mut self,
        cmd: &mut RelaxedCommand,
        explicit: bool,
        operator_pending: &mut bool,
    ) -> Result<(), String> {
        if cmd.next.is_some() {
            return Err("a redirection names no file".into());
        }
        if !cmd.started {
            if explicit {
                return Err("an operator has no command on one side".into());
            }
            return Ok(());
        }
        self.finish(cmd);
        *operator_pending = false;
        Ok(())
    }

    fn end_operator(
        &mut self,
        cmd: &mut RelaxedCommand,
        operator_pending: &mut bool,
    ) -> Result<(), String> {
        if cmd.next.is_some() || !cmd.started {
            return Err("an operator has no command on one side".into());
        }
        self.finish(cmd);
        *operator_pending = true;
        Ok(())
    }

    /// The bodies of the heredocs opened on the line just ended.
    fn heredoc_bodies(&mut self) -> Result<(), String> {
        for doc in std::mem::take(&mut self.heredocs) {
            loop {
                if self.pos >= self.chars.len() {
                    return Err("a heredoc is not closed".into());
                }
                let end = self.chars[self.pos..]
                    .iter()
                    .position(|&c| c == '\n')
                    .map_or(self.chars.len(), |n| self.pos + n);
                let line: String = self.chars[self.pos..end].iter().collect();
                self.pos = (end + 1).min(self.chars.len());
                let compared = if doc.strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if compared == doc.delimiter {
                    break;
                }
                if doc.expands {
                    if line.contains('`') || line.contains("$(") {
                        return Err("a heredoc's text substitutes a command".into());
                    }
                    if line.contains('$') {
                        self.need(
                            ScriptConstruct::Variables,
                            "it expands a variable or substitutes a command",
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn word(&mut self) -> Result<RelaxedWord, String> {
        let mut w = RelaxedWord::default();
        while let Some(c) = self.peek() {
            match c {
                c if ends_word(c) => break,
                '\'' => {
                    let end = self.chars[self.pos + 1..]
                        .iter()
                        .position(|&q| q == '\'')
                        .ok_or("a quote is not closed")?;
                    w.text.extend(&self.chars[self.pos + 1..self.pos + 1 + end]);
                    w.quoted = true;
                    self.pos += end + 2;
                }
                '"' => self.double_quoted(&mut w)?,
                '\\' => {
                    self.need(ScriptConstruct::Escapes, "it holds a backslash escape")?;
                    match self.peek_at(1) {
                        None => return Err("the script ends in a backslash".into()),
                        Some('\n') => {}
                        Some(e) => {
                            w.text.push(e);
                            w.quoted = true;
                        }
                    }
                    self.pos += 2;
                }
                '$' => self.dollar(&mut w, false)?,
                '`' => self.backquote(&mut w)?,
                '!' => {
                    self.need(ScriptConstruct::Escapes, "it holds `!`")?;
                    w.text.push('!');
                    self.pos += 1;
                }
                '#' => {
                    self.need(ScriptConstruct::Escapes, "it holds a comment")?;
                    w.text.push('#');
                    self.pos += 1;
                }
                '{' | '}' => {
                    self.need(
                        ScriptConstruct::Subshells,
                        "it holds a brace group or brace expansion",
                    )?;
                    w.text.push(c);
                    w.special = true;
                    self.pos += 1;
                }
                c if c.is_control() => return Err("it holds a control character".into()),
                c => {
                    w.text.push(c);
                    if matches!(c, '*' | '?' | '[' | ']' | '~' | '=') {
                        w.special = true;
                    }
                    self.pos += 1;
                }
            }
        }
        Ok(w)
    }

    fn double_quoted(&mut self, w: &mut RelaxedWord) -> Result<(), String> {
        const WHY: &str = "a double-quoted word expands, substitutes or escapes";
        w.quoted = true;
        self.pos += 1;
        loop {
            match self.peek() {
                None => return Err("a quote is not closed".into()),
                Some('"') => {
                    self.pos += 1;
                    return Ok(());
                }
                Some('\\') => {
                    self.need(ScriptConstruct::Escapes, WHY)?;
                    match self.peek_at(1) {
                        Some(e @ ('$' | '`' | '"' | '\\')) => {
                            w.text.push(e);
                            self.pos += 2;
                        }
                        Some('\n') => self.pos += 2,
                        _ => {
                            w.text.push('\\');
                            self.pos += 1;
                        }
                    }
                }
                Some('$') => {
                    self.need(ScriptConstruct::Variables, WHY)?;
                    self.dollar(w, true)?;
                }
                Some('`') => {
                    self.need(ScriptConstruct::Variables, WHY)?;
                    self.backquote(w)?;
                }
                Some(c) => {
                    w.text.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// A `$` at the read position.
    fn dollar(&mut self, w: &mut RelaxedWord, in_quotes: bool) -> Result<(), String> {
        let next = self.peek_at(1);
        if next == Some('\'') && !in_quotes {
            // `$'...'`: a quote with backslash escapes.
            self.need(ScriptConstruct::Escapes, "it holds a `$'...'` quote")?;
            self.pos += 2;
            loop {
                match self.peek() {
                    None => return Err("a quote is not closed".into()),
                    Some('\'') => {
                        self.pos += 1;
                        break;
                    }
                    Some('\\') => {
                        if let Some(e) = self.peek_at(1) {
                            w.text.push(e);
                        }
                        self.pos += 2;
                    }
                    Some(c) => {
                        w.text.push(c);
                        self.pos += 1;
                    }
                }
            }
            w.quoted = true;
            return Ok(());
        }
        self.need(
            ScriptConstruct::Variables,
            "it expands a variable or substitutes a command",
        )?;
        match next {
            Some('(') if self.peek_at(2) == Some('(') => {
                // `$((...))`: arithmetic, no command in it.
                self.pos += 3;
                let mut depth = 2;
                while depth > 0 {
                    match self.peek() {
                        None => return Err("an arithmetic expansion is not closed".into()),
                        Some('(') => depth += 1,
                        Some(')') => depth -= 1,
                        Some('`') => {
                            return Err("an arithmetic expansion substitutes a command".into());
                        }
                        Some('$') if self.peek_at(1) == Some('(') => {
                            return Err("an arithmetic expansion substitutes a command".into());
                        }
                        _ => {}
                    }
                    self.pos += 1;
                }
                w.opaque = true;
            }
            Some('(') => {
                self.pos += 2;
                self.list(Close::Paren)?;
                w.opaque = true;
            }
            Some('{') => {
                // `${...}`: refused when it could hide a command or a quote.
                self.pos += 2;
                let mut depth = 1;
                while depth > 0 {
                    match self.peek() {
                        None => return Err("a parameter expansion is not closed".into()),
                        Some('{') => depth += 1,
                        Some('}') => depth -= 1,
                        Some('`') | Some('\'') | Some('"') | Some('\\') => {
                            return Err("a parameter expansion holds a quote, an escape or a \
                                 substitution"
                                .into());
                        }
                        Some('$') if self.peek_at(1) == Some('(') => {
                            return Err("a parameter expansion holds a quote, an escape or a \
                                 substitution"
                                .into());
                        }
                        _ => {}
                    }
                    self.pos += 1;
                }
                w.opaque = true;
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                self.pos += 1;
                while self
                    .peek()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    self.pos += 1;
                }
                w.opaque = true;
            }
            Some(c) if c.is_ascii_digit() || "?@*#$!-".contains(c) => {
                self.pos += 2;
                w.opaque = true;
            }
            // `$"..."`: a quote; the next round reads it.
            Some('"') if !in_quotes => self.pos += 1,
            _ => {
                w.text.push('$');
                self.pos += 1;
            }
        }
        Ok(())
    }

    /// A backquote substitution at the read position, read as its own
    /// script.
    fn backquote(&mut self, w: &mut RelaxedWord) -> Result<(), String> {
        self.need(
            ScriptConstruct::Variables,
            "it expands a variable or substitutes a command",
        )?;
        let end = self.chars[self.pos + 1..]
            .iter()
            .position(|&q| q == '`')
            .ok_or("a backquote is not closed")?;
        let inner: String = self.chars[self.pos + 1..self.pos + 1 + end]
            .iter()
            .collect();
        if inner.contains('\\') {
            return Err("a backquote substitution holds a backslash".into());
        }
        let mut sub = RelaxedReader::new(&inner, self.allow);
        sub.list(Close::End)?;
        if !sub.heredocs.is_empty() {
            return Err("a heredoc inside a backquote substitution has no body".into());
        }
        self.commands.append(&mut sub.commands);
        self.redirects_to_file |= sub.redirects_to_file;
        self.changes_directory |= sub.changes_directory;
        self.unseen |= sub.unseen;
        self.pos += end + 2;
        w.opaque = true;
        Ok(())
    }
}

/// The program a shell rule names, for [`construct_rule_limit`]: the base
/// name of its pattern's first word, as a prefix when that word holds a `*`
/// (the text before the first `*`; a `*` stands for any text, so
/// `* --version` names every program). `None` for a rule on another tool or
/// on every shell call (those already match every call).
fn rule_program(rule: &Rule) -> Option<(String, bool)> {
    if !rule.names_tool("shell") {
        return None;
    }
    let pattern = rule.pattern.as_deref()?;
    let first = pattern.split_whitespace().next()?;
    let (word, prefix) = match first.split_once('*') {
        Some((stem, _)) => (stem, true),
        None => (first, false),
    };
    Some((base_name(word).to_owned(), prefix))
}

/// The user's shell rules matched by program name against the text of a
/// script a mode judges only because of a construct the user turned on
/// ([`relies_on_constructs`]). Such a script may hide a command from
/// [`simple_commands`] (`C=curl; $C u` names no program), so a rule whose
/// program appears anywhere in the script as a word (split at every
/// character a program name cannot hold, `/` included) refuses or asks.
/// Deliberately broad: `shell(git push*)` here matches any script naming
/// `git`. Deny wins over ask. `None` for any other call.
pub fn construct_rule_limit(
    rules: &UserRules,
    constructs: &ScriptConstructs,
    call: &PendingCall,
) -> Option<HardLimit> {
    if call.tool != "shell" {
        return None;
    }
    let argv = shell_argv(&call.input);
    if !relies_on_constructs(&argv, constructs) {
        return None;
    }
    let words: Vec<&str> = argv
        .iter()
        .flat_map(|a| {
            a.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-')))
        })
        .filter(|w| !w.is_empty())
        .collect();
    let named = |rule: &Rule| -> Option<String> {
        let (program, prefix) = rule_program(rule)?;
        words
            .iter()
            .find(|w| {
                if prefix {
                    w.starts_with(program.as_str())
                } else {
                    **w == program
                }
            })
            .map(|w| (*w).to_owned())
    };
    let why = format!(
        "the script names `{{}}`, and with {} on, kGraf cannot see every command it runs",
        constructs.describe()
    );
    if let Some((rule, word)) = rules.deny.iter().find_map(|r| named(r).map(|w| (r, w))) {
        return Some(HardLimit::Deny(format!(
            "your deny rule `{}` blocks this call in every permission mode: {}",
            rule.text(),
            why.replace("{}", &word)
        )));
    }
    rules
        .ask
        .iter()
        .find_map(|r| named(r).map(|w| (r, w)))
        .map(|(rule, word)| {
            HardLimit::Ask(format!(
                "your ask rule `{}` asks about this call in every permission mode: {}; allow it?",
                rule.text(),
                why.replace("{}", &word)
            ))
        })
}

/// A call's subjects, for rule matching: a shell call's command lines (the
/// whole argv and each simple command, joined with single spaces), each also
/// from every place its program may stand ([`command_starts`]), so `{ curl
/// x`, `env A=1 curl x` and `command curl x` all give `curl x` too; or a file
/// tool's paths. The whole call comes first.
pub fn subjects(tool: &str, input: &serde_json::Value) -> Vec<String> {
    subjects_in(tool, input, &ReadContext::default())
}

/// [`subjects`], with the commands the files of `ctx`'s directory make the
/// call's programs run (a recipe, a script's literal calls).
pub fn subjects_in(tool: &str, input: &serde_json::Value, ctx: &ReadContext) -> Vec<String> {
    if tool == "shell" {
        return subjects_of(&read_call_in(&shell_argv(input), ctx));
    }
    file_paths(tool, input)
}

/// A shell call's rule subjects ([`subjects`]) from its reading.
fn subjects_of(reading: &CallReading) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for command in &reading.commands {
        let command = &command.words;
        if command.is_empty() {
            out.push(String::new());
            continue;
        }
        for at in command_starts(command) {
            let line = command[at..].join(" ");
            if !out.contains(&line) {
                out.push(line);
            }
            // The program by its name alone, as a rule names it: a path
            // (`/usr/bin/curl`), another case (`CURL`, which a
            // case-insensitive file system runs as `curl`) or a Windows
            // extension (`curl.exe`) runs the same program.
            let key = program_key(&command[at]);
            if !key.is_empty() && key != command[at] {
                let mut named = vec![key];
                named.extend(command[at + 1..].iter().cloned());
                let line = named.join(" ");
                if !out.contains(&line) {
                    out.push(line);
                }
            }
            // `git [GLOBAL OPTIONS] SUBCOMMAND` as `git SUBCOMMAND`, so a
            // rule on the subcommand matches past the options.
            if let Some(line) = git::normalized(command, at).map(|w| w.join(" "))
                && !out.contains(&line)
            {
                out.push(line);
            }
        }
    }
    out
}

/// What a limit no mode lifts decided about one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HardLimit {
    Deny(String),
    Ask(String),
}

/// How a deletion limit's reason begins, so [`refused_subject`] knows the
/// reason came from the deletion check and not from a rule.
const DELETING_LEAD: &str = "deleting ";
const DELETES_LEAD: &str = "this command deletes ";

/// Programs that delete what they are given.
const DELETERS: &[&str] = &["rm", "rmdir", "unlink", "shred"];

/// The limit on deleting critical paths, for one shell call. `home` is the
/// user's home directory; `workspace` the worker's worktree (the command's
/// working directory). A system path (the filesystem root, a top-level
/// directory, the home directory, or a glob over one of them) is refused; the
/// workspace itself, its `.git`, a relative path that climbs out of the
/// workspace, or a recursive deletion of an operand kGraf cannot resolve
/// (a variable or a substitution) asks.
pub fn critical_deletion(
    argv: &[String],
    home: Option<&Path>,
    workspace: Option<&Path>,
) -> Option<HardLimit> {
    // The commands a file in the workspace makes a program run (a recipe, a
    // script's literal calls) are read too (decision 0094, amended
    // 2026-10-06).
    let ctx = workspace
        .map(|ws| ReadContext::in_worktree(ws, home))
        .unwrap_or_default();
    critical_deletion_of(&read_call_in(argv, &ctx), home, workspace)
}

/// [`critical_deletion`] over a call already read.
fn critical_deletion_of(
    reading: &CallReading,
    home: Option<&Path>,
    workspace: Option<&Path>,
) -> Option<HardLimit> {
    let mut ask: Option<HardLimit> = None;
    // Where a relative operand may point. The first entry is the workspace,
    // moved by each plain `cd` (the first word of a script's own command), as
    // it always was (`( cd x )` is split at the parenthesis and counts as
    // plain, as before). A directory change kGraf is less sure of
    // (`command cd`, `{ cd`, `pushd`, `cd -P`, a `cd` in a nested shell) adds
    // where it may lead instead of replacing anything, and an operand is
    // judged by the strictest result over every entry. So a directory change
    // can only make the limit stricter.
    let mut cwds = vec![match workspace {
        Some(ws) => Cwd::Known(ws.to_path_buf()),
        None => Cwd::NoWorkspace,
    }];
    for Nested {
        depth,
        words: command,
        same_shell,
        dir,
    } in &reading.commands
    {
        let (depth, same_shell) = (*depth, *same_shell);
        let words: Vec<&str> = command.iter().map(String::as_str).collect();
        let starts = command_starts(&words);
        if let Some(&at) = starts
            .iter()
            .find(|&&i| DIRECTORY_CHANGERS.contains(&base_name(words[i])))
        {
            let moved: Vec<Cwd> = cwds
                .iter()
                .map(|c| change_directory(c, &words[at..], home))
                .collect();
            let plain = depth <= 1 && same_shell && at == 0 && base_name(words[0]) == "cd";
            if plain {
                for c in cwds.iter_mut() {
                    *c = cd(c, words.get(1).copied(), home);
                }
            }
            for m in moved {
                if cwds.len() < MAX_CWDS && !cwds.contains(&m) {
                    cwds.push(m);
                }
            }
            // A plain `cd` deletes nothing. Any other command is still read
            // for a deleter: `cd` may only be an option's value
            // (`sudo -u cd rm -rf /`).
            if plain {
                continue;
            }
        }
        let Some(at) = program_index(&words) else {
            continue;
        };
        let program = base_name(words[at]);
        let rest = &words[at + 1..];
        // A command a program started in another directory (`make -C DIR`,
        // a recipe there, a command after `cd DIR`) is also judged from
        // there.
        let mut judged = cwds.clone();
        if let Some(d) = &dir {
            let d = Cwd::Known(d.clone());
            if !judged.contains(&d) {
                judged.push(d);
            }
        }
        if program == "find" {
            // Only a `-delete` search deletes; its roots are the words before
            // the first expression. Deleting under a root is not deleting the
            // root itself, so only a system root is refused.
            if !rest.contains(&"-delete") {
                continue;
            }
            for w in rest
                .iter()
                .take_while(|w| !w.starts_with('-') && **w != "(" && **w != "!")
            {
                for cwd in &judged {
                    if let Target::System(p) = classify(w, home, cwd, workspace) {
                        return Some(system_refusal(&p));
                    }
                }
            }
            continue;
        }
        let recursive = program == "rmdir"
            || rest.iter().any(|w| {
                *w == "--recursive"
                    || (w.starts_with('-')
                        && !w.starts_with("--")
                        && (w.contains('r') || w.contains('R')))
            });
        // `xargs rm -rf` and `parallel rm -rf` take more operands from
        // their input, which kGraf cannot see.
        if recursive
            && words[..at]
                .iter()
                .any(|w| matches!(base_name(w), "xargs" | "parallel"))
        {
            ask.get_or_insert(HardLimit::Ask(format!(
                "{DELETES_LEAD}what `{}` reads from its input recursively, and kGraf cannot tell \
                 what it names; allow it?",
                words[..at]
                    .iter()
                    .rev()
                    .find(|w| matches!(base_name(w), "xargs" | "parallel"))
                    .map_or("xargs", |w| base_name(w))
            )));
        }
        let mut operands_only = false;
        for w in rest {
            if !operands_only && *w == "--" {
                operands_only = true;
                continue;
            }
            if !operands_only && w.starts_with('-') {
                continue;
            }
            for cwd in &judged {
                match classify(w, home, cwd, workspace) {
                    Target::System(p) => return Some(system_refusal(&p)),
                    Target::Critical(p) => {
                        ask.get_or_insert(HardLimit::Ask(format!(
                            "{DELETES_LEAD}{p}, a critical path (the workspace itself, its \
                             .git, or a path outside the workspace); allow it?"
                        )));
                    }
                    Target::Unresolved(p) if recursive => {
                        ask.get_or_insert(HardLimit::Ask(format!(
                            "{DELETES_LEAD}`{p}` recursively, and kGraf cannot tell what it \
                             names; allow it?"
                        )));
                    }
                    Target::Unresolved(_) | Target::Plain => {}
                }
            }
        }
    }
    ask
}

/// How many places [`critical_deletion`] keeps for where relative operands
/// may point. The first, the plain-`cd` path, is always kept.
const MAX_CWDS: usize = 16;

/// Where the directory change `words` (a changer of [`DIRECTORY_CHANGERS`]
/// and its arguments) moves from `cwd`: `cd` and `pushd` to their first
/// operand after options (`cd -P /` goes to `/`), `pushd` with no directory
/// or a stack index and `popd` to a place kGraf cannot tell.
fn change_directory(cwd: &Cwd, words: &[&str], home: Option<&Path>) -> Cwd {
    let changer = base_name(words[0]);
    if changer == "popd" {
        return Cwd::Unknown;
    }
    let mut operands = words[1..].iter().copied();
    let target = loop {
        match operands.next() {
            Some("--") => break operands.next(),
            Some(w) if w.starts_with('-') && w != "-" => continue,
            other => break other,
        }
    };
    match target {
        None if changer == "pushd" => Cwd::Unknown,
        Some(t) if changer == "pushd" && t.starts_with('+') => Cwd::Unknown,
        t => cd(cwd, t, home),
    }
}

fn system_refusal(path: &str) -> HardLimit {
    HardLimit::Deny(format!(
        "{DELETING_LEAD}{path} is refused in every permission mode: it is a system path (the \
         filesystem root, a top-level directory or the home directory)"
    ))
}

/// Where the deleting program stands in a simple command: the first place
/// [`command_starts`] lists that names a deleter or `find`. `None` when the
/// command runs another program first (`echo rm -rf /` deletes nothing).
fn program_index(words: &[&str]) -> Option<usize> {
    command_starts(words).into_iter().find(|&i| {
        let b = base_name(words[i]);
        DELETERS.contains(&b) || b == "find"
    })
}

enum Target {
    System(String),
    Critical(String),
    Unresolved(String),
    Plain,
}

/// The directory a script's relative operands resolve against.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cwd {
    Known(PathBuf),
    /// After `cd $X`, `cd -` or another target that cannot be resolved.
    Unknown,
    /// No workspace was given: relative operands are judged by their shape.
    NoWorkspace,
}

/// `~`, `~/x`, `$HOME` and `${HOME}/x` with the home directory put in.
fn expand_home(word: &str, home: Option<&Path>) -> Option<String> {
    if let Some(h) = home {
        if matches!(word, "~" | "$HOME" | "${HOME}") {
            return Some(h.display().to_string());
        }
        for prefix in ["~/", "$HOME/", "${HOME}/"] {
            if let Some(rest) = word.strip_prefix(prefix) {
                return Some(format!("{}/{}", h.display(), rest));
            }
        }
    }
    None
}

/// Where `cd target` moves from `cwd`.
fn cd(cwd: &Cwd, target: Option<&str>, home: Option<&Path>) -> Cwd {
    let target = match target {
        None | Some("~") => return home.map_or(Cwd::Unknown, |h| Cwd::Known(h.to_path_buf())),
        Some(t) => t,
    };
    let t = expand_home(target, home).unwrap_or_else(|| target.to_owned());
    if t == "-" || t.contains('$') || t.contains('`') {
        return Cwd::Unknown;
    }
    let path = PathBuf::from(&t);
    let joined = if path.is_absolute() {
        path
    } else {
        match cwd {
            Cwd::Known(c) => c.join(path),
            Cwd::Unknown | Cwd::NoWorkspace => return Cwd::Unknown,
        }
    };
    lexical_normalize(&joined).map_or(Cwd::Known(PathBuf::from("/")), Cwd::Known)
}

fn classify(word: &str, home: Option<&Path>, cwd: &Cwd, workspace: Option<&Path>) -> Target {
    if matches!(word, "~" | "$HOME" | "${HOME}") {
        return Target::System(word.to_owned());
    }
    // `~` and `$HOME` name the home directory.
    let w = expand_home(word, home).unwrap_or_else(|| word.to_owned());
    // A variable, a substitution, or a placeholder another program fills in
    // (`find -exec rm -rf {} ;`, `parallel rm -rf {} :::`, a brace
    // expansion) names what kGraf cannot see.
    if w.contains('$') || w.contains('`') || (w.contains('{') && w.contains('}')) {
        return Target::Unresolved(word.to_owned());
    }
    let path = PathBuf::from(&w);
    let absolute = path.is_absolute();
    let joined = if absolute {
        path.clone()
    } else {
        match cwd {
            Cwd::Known(c) => c.join(&path),
            Cwd::Unknown => return Target::Unresolved(word.to_owned()),
            Cwd::NoWorkspace => return relative_target(&path, word),
        }
    };
    let Some(norm) = lexical_normalize(&joined) else {
        // Climbed above the filesystem root.
        return Target::System(word.to_owned());
    };
    // A glob over a component makes it as wide as its parent.
    let comps: Vec<String> = norm
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let glob_at = comps
        .iter()
        .position(|c| c.contains('*') || c.contains('?') || c.contains('['));
    let depth = glob_at.unwrap_or(comps.len());
    let effective: PathBuf = std::iter::once("/".to_owned())
        .chain(comps.iter().take(depth).cloned())
        .collect();
    if depth <= 1 {
        return Target::System(word.to_owned());
    }
    if let Some(h) = home
        && let Some(hn) = lexical_normalize(h)
        && (effective == hn || hn.starts_with(&effective))
    {
        return Target::System(word.to_owned());
    }
    if let Some(ws) = workspace
        && let Some(wn) = lexical_normalize(ws)
    {
        if effective == wn || wn.starts_with(&effective) || effective == wn.join(".git") {
            return Target::Critical(word.to_owned());
        }
        if effective.starts_with(wn.join(".git")) {
            return Target::Critical(word.to_owned());
        }
        if !absolute && !effective.starts_with(&wn) {
            return Target::Critical(word.to_owned());
        }
    }
    Target::Plain
}

fn relative_target(path: &Path, word: &str) -> Target {
    let mut depth: i64 = 0;
    for c in path.components() {
        match c {
            Component::ParentDir => depth -= 1,
            Component::Normal(_) => depth += 1,
            _ => {}
        }
        if depth < 0 {
            return Target::Critical(word.to_owned());
        }
    }
    if depth == 0 || path.starts_with(".git") {
        return Target::Critical(word.to_owned());
    }
    Target::Plain
}

/// `.` and `..` removed without touching the filesystem; `None` when `..`
/// climbs above the root.
fn lexical_normalize(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::RootDir | Component::Prefix(_) => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || out.as_os_str().is_empty() {
                    return None;
                }
            }
            Component::Normal(s) => out.push(s),
        }
    }
    Some(out)
}

/// The limits no mode lifts, for one call: the user's deny rules, then the
/// critical-path deletion limit, then the user's ask rules.
pub fn hard_limit(
    rules: &UserRules,
    call: &PendingCall,
    home: Option<&Path>,
    workspace: Option<&Path>,
) -> Option<HardLimit> {
    // The files of the workspace a call's programs run (a recipe, a
    // script) are read from it (decision 0094, amended 2026-10-06).
    let ctx = workspace
        .map(|ws| ReadContext::in_worktree(ws, home))
        .unwrap_or_default();
    // One reading serves the rules, the deletion limit and the unreadable
    // check: each would otherwise read the same files again.
    let reading = (call.tool == "shell").then(|| read_call_in(&shell_argv(&call.input), &ctx));
    let subjects = match &reading {
        Some(r) => subjects_of(r),
        None => file_paths(&call.tool, &call.input),
    };
    if let Some(rule) = rules.deny.iter().find(|r| r.matches(&call.tool, &subjects)) {
        return Some(HardLimit::Deny(format!(
            "your deny rule `{}` blocks this call in every permission mode",
            rule.text()
        )));
    }
    let deletion = reading
        .as_ref()
        .and_then(|r| critical_deletion_of(r, home, workspace));
    if let Some(HardLimit::Deny(reason)) = &deletion {
        return Some(HardLimit::Deny(reason.clone()));
    }
    if let Some(rule) = rules.ask.iter().find(|r| r.matches(&call.tool, &subjects)) {
        return Some(HardLimit::Ask(format!(
            "your ask rule `{}` asks about this call in every permission mode; allow it?",
            rule.text()
        )));
    }
    // A command kGraf cannot read (`source FILE`, `bash FILE`, `ssh HOST`)
    // may run what a shell rule names, so while the user has one the call is
    // asked about in every mode, Bypass too (decision 0094, amended
    // 2026-10-06). A rule with no pattern already matched above.
    if let Some(why) = reading.as_ref().and_then(|r| r.unreadable.first())
        && let Some((kind, rule)) = rules
            .deny
            .iter()
            .map(|r| ("deny", r))
            .chain(rules.ask.iter().map(|r| ("ask", r)))
            .find(|(_, r)| r.names_tool("shell") && r.pattern.is_some())
    {
        return Some(HardLimit::Ask(format!(
            "{why}, so kGraf cannot check what it runs against your {kind} rule `{}`; allow it?",
            rule.text()
        )));
    }
    deletion
}

/// What a grant to a contract would allow, for [`rule_on_grant`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantSubject<'a> {
    /// A shell program (`argv[0]`).
    Program(&'a str),
    /// A repository path a file tool would write.
    Path(&'a str),
    /// A tool by wire name.
    Tool(&'a str),
}

impl GrantSubject<'_> {
    /// Whether `rule` would apply to some call the grant allows.
    fn reached_by(&self, rule: &Rule) -> bool {
        match *self {
            Self::Program(program) => {
                if !rule.names_tool("shell") {
                    return false;
                }
                let Some(p) = &rule.pattern else {
                    return true;
                };
                // A one-word prefix pattern (`python*`) matches the program as
                // a prefix; a first word holding a `*` elsewhere (`py*n x`,
                // `* --version`) matches a program starting with the text
                // before it; otherwise the pattern's first word is a program,
                // and some command line of that program may match the rest.
                let first = p.split_whitespace().next().unwrap_or_default();
                match p.strip_suffix('*') {
                    Some(prefix) if !prefix.contains(char::is_whitespace) => {
                        program.starts_with(prefix) || prefix.starts_with(program)
                    }
                    _ => match first.split_once('*') {
                        Some((stem, _)) => program.starts_with(stem),
                        None => first == program,
                    },
                }
            }
            Self::Path(path) => {
                if rule.tool == "shell" {
                    return false;
                }
                match &rule.pattern {
                    None => rule.names_tool("apply_patch"),
                    Some(_) => rule.matches(&rule.tool, &[path.to_owned()]),
                }
            }
            Self::Tool(tool) => rule.names_tool(tool),
        }
    }
}

/// The user's rule a grant to a contract would meet again (decision 0094).
/// A grant cannot lift a deny or an ask rule: the rules hook runs on every
/// call in every mode, so a program, path or tool granted against a rule is
/// refused or asked about again on the next attempt. Deny wins over ask.
pub fn rule_on_grant(rules: &UserRules, subject: GrantSubject<'_>) -> Option<HardLimit> {
    if let Some(rule) = rules.deny.iter().find(|r| subject.reached_by(r)) {
        return Some(HardLimit::Deny(format!(
            "your deny rule `{}` blocks it in every permission mode, and a grant cannot lift a \
             rule",
            rule.text()
        )));
    }
    rules
        .ask
        .iter()
        .find(|r| subject.reached_by(r))
        .map(|rule| {
            HardLimit::Ask(format!(
                "your ask rule `{}` asks about it in every permission mode, and a grant cannot \
                 lift a rule",
                rule.text()
            ))
        })
}

/// The approval hook for the limits no mode lifts. Install it **last** in the
/// chain: it then reviews the input every earlier hook left, so no earlier
/// hook's rewrite or allow gets past a deny or an ask rule.
pub struct PermissionRulesHook {
    control: Arc<PermissionControl>,
    home: Option<PathBuf>,
    workspace: Option<PathBuf>,
}

impl PermissionRulesHook {
    pub fn new(control: Arc<PermissionControl>, workspace: Option<PathBuf>) -> Self {
        Self {
            control,
            home: std::env::var_os("HOME").map(PathBuf::from),
            workspace,
        }
    }

    /// The same hook with another home directory (tests).
    pub fn with_home(mut self, home: Option<PathBuf>) -> Self {
        self.home = home;
        self
    }
}

#[async_trait::async_trait]
impl ApprovalHook for PermissionRulesHook {
    fn name(&self) -> &str {
        PERMISSION_RULES_HOOK
    }

    async fn review(&self, call: &PendingCall) -> HookDecision {
        let settings = self.control.settings();
        let limit = hard_limit(
            &settings.rules,
            call,
            self.home.as_deref(),
            self.workspace.as_deref(),
        );
        // Decision 0094, amended 2026-10-02: a script judged only because of
        // a construct the user turned on also meets the rules by program
        // name in its text. Bypass grants scripts whole, whatever the
        // switches say, so its limits stay as they were.
        let by_name = if settings.mode == PermissionMode::Bypass {
            None
        } else {
            construct_rule_limit(&settings.rules, &settings.script_constructs, call)
        };
        let decided = match (limit, by_name) {
            (Some(HardLimit::Deny(r)), _) | (_, Some(HardLimit::Deny(r))) => {
                Some(HardLimit::Deny(r))
            }
            (Some(l), _) | (None, Some(l)) => Some(l),
            (None, None) => None,
        };
        match decided {
            Some(HardLimit::Deny(reason)) => HookDecision::Deny { reason },
            Some(HardLimit::Ask(question)) => HookDecision::Ask { question },
            None => HookDecision::Allow,
        }
    }
}

/// One permission request for grouping: its id and when it was raised.
pub trait Raised {
    fn raised_at_ms(&self) -> i64;
}

/// Group requests: sorted by time, each group holds the requests raised
/// within [`GROUP_WINDOW_MS`] of its first one. Pure.
pub fn group_requests<T: Raised + Clone>(items: &[T]) -> Vec<Vec<T>> {
    let mut sorted: Vec<T> = items.to_vec();
    sorted.sort_by_key(|i| i.raised_at_ms());
    let mut groups: Vec<Vec<T>> = Vec::new();
    for item in sorted {
        match groups.last_mut() {
            Some(g) if item.raised_at_ms() - g[0].raised_at_ms() <= GROUP_WINDOW_MS => g.push(item),
            _ => groups.push(vec![item]),
        }
    }
    groups
}

#[cfg(test)]
#[path = "permissions_prefix_tests.rs"]
mod prefix_tests;

#[cfg(test)]
#[path = "permissions_review_tests.rs"]
mod review_tests;

#[cfg(test)]
#[path = "permission_names_tests.rs"]
mod name_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::{AgentId, TaskId};
    use serde_json::json;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    fn shell(words: &[&str]) -> PendingCall {
        PendingCall {
            call_id: "c".into(),
            tool: "shell".into(),
            input: json!({ "command": words }),
            worker: AgentId::new("w"),
            task: TaskId::new("t"),
        }
    }

    fn home() -> PathBuf {
        PathBuf::from("/home/someone")
    }

    fn ws() -> PathBuf {
        PathBuf::from("/home/someone/proj/.kgraf/worktrees/w1")
    }

    fn limit(words: &[&str]) -> Option<HardLimit> {
        critical_deletion(&argv(words), Some(&home()), Some(&ws()))
    }

    #[test]
    fn deleting_a_system_path_is_refused() {
        for words in [
            &["rm", "-rf", "/"][..],
            &["rm", "-rf", "/usr"],
            &["rm", "-rf", "/etc/"],
            &["rm", "-rf", "/*"],
            &["rm", "-rf", "~"],
            &["rm", "-rf", "~/"],
            &["rm", "-rf", "$HOME"],
            &["rm", "-r", "/home/someone"],
            &["rm", "-r", "/home"],
            &["sudo", "rm", "-rf", "/"],
            &["rmdir", "/tmp"],
            &["sh", "-c", "cd /tmp && rm -rf /"],
            &["bash", "-lc", "echo hi; rm -rf ~"],
            &["rm", "-rf", "--", "/"],
            &["rm", "-rf", "/u*"],
            &["find", "/", "-delete"],
            &["sh", "-c", "cd / && rm -rf *"],
            &["bash", "-c", "cd ~ ; rm -rf ./*"],
            &["sh", "-c", "cd /usr/local && rm -rf ../*"],
        ] {
            assert!(
                matches!(limit(words), Some(HardLimit::Deny(_))),
                "{words:?}: {:?}",
                limit(words)
            );
        }
    }

    #[test]
    fn deleting_a_critical_path_asks_and_an_ordinary_one_passes() {
        for words in [
            &["rm", "-rf", "."][..],
            &["rm", "-rf", ".git"],
            &["rm", "-rf", ".."],
            &["rm", "-rf", "../other"],
            &["rm", "-rf", "$TARGET"],
            &["sh", "-c", "cd $DIR && rm -rf *"],
            &["sh", "-c", "cd /tmp/scratch/x && rm -rf ."],
        ] {
            assert!(
                matches!(limit(words), Some(HardLimit::Ask(_))),
                "{words:?}: {:?}",
                limit(words)
            );
        }
        for words in [
            &["rm", "-rf", "target"][..],
            &["rm", "src/old.rs"],
            &["rm", "-f", "/tmp/scratch/file"],
            &["ls", "/"],
            &["echo", "rm", "-rf", "/"],
            &["rm", "$FILE"],
        ] {
            assert_eq!(limit(words), None, "{words:?}");
        }
    }

    #[test]
    fn rules_parse_and_match_prefixes_and_script_commands() {
        let r = Rule::parse("shell(git push*)").expect("rule");
        assert!(r.matches(
            "shell",
            &subjects("shell", &json!({"command": ["git", "push", "origin"]}))
        ));
        assert!(r.matches(
            "shell",
            &subjects(
                "shell",
                &json!({"command": ["sh", "-c", "make && git push --force"]})
            )
        ));
        assert!(!r.matches(
            "shell",
            &subjects("shell", &json!({"command": ["git", "status"]}))
        ));
        let any = Rule::parse("mcp__web__fetch").expect("rule");
        assert!(any.matches("mcp__web__fetch", &[]));
        let path = Rule::parse("apply_patch(secrets/*)").expect("rule");
        let patch = json!({"patch": "*** Begin Patch\n*** Update File: secrets/key.txt\n@@\n-a\n+b\n*** End Patch"});
        assert!(path.matches("apply_patch", &subjects("apply_patch", &patch)));
        assert!(Rule::parse("shell(").is_err());
        assert!(Rule::parse("(x)").is_err());
        assert!(Rule::parse("shell()").is_err());
    }

    /// A grant meets the rule that would refuse or ask about the calls it
    /// allows; an unrelated rule is not met.
    #[test]
    fn a_grant_meets_the_rule_its_calls_would_meet() {
        let rules = UserRules {
            deny: vec![Rule::parse("shell(curl*)").expect("rule")],
            ask: vec![
                Rule::parse("shell(python3*)").expect("rule"),
                Rule::parse("apply_patch(secrets/*)").expect("rule"),
                Rule::parse("mcp__web__fetch").expect("rule"),
                Rule::parse("shell(git push*)").expect("rule"),
            ],
        };
        let deny = rule_on_grant(&rules, GrantSubject::Program("curl"));
        assert!(
            matches!(&deny, Some(HardLimit::Deny(r)) if r.contains("shell(curl*)")),
            "{deny:?}"
        );
        let ask = rule_on_grant(&rules, GrantSubject::Program("python3"));
        assert!(
            matches!(&ask, Some(HardLimit::Ask(r)) if r.contains("shell(python3*)")),
            "{ask:?}"
        );
        assert!(matches!(
            rule_on_grant(&rules, GrantSubject::Program("git")),
            Some(HardLimit::Ask(_))
        ));
        assert_eq!(rule_on_grant(&rules, GrantSubject::Program("cp")), None);
        assert!(matches!(
            rule_on_grant(&rules, GrantSubject::Path("secrets/key.txt")),
            Some(HardLimit::Ask(_))
        ));
        assert_eq!(
            rule_on_grant(&rules, GrantSubject::Path("src/lib.rs")),
            None
        );
        assert!(matches!(
            rule_on_grant(&rules, GrantSubject::Tool("mcp__web__fetch")),
            Some(HardLimit::Ask(_))
        ));
        assert_eq!(rule_on_grant(&rules, GrantSubject::Tool("mcp__x__y")), None);
        let every_shell = UserRules {
            deny: vec![Rule::parse("shell").expect("rule")],
            ask: vec![],
        };
        assert!(matches!(
            rule_on_grant(&every_shell, GrantSubject::Program("ls")),
            Some(HardLimit::Deny(_))
        ));
    }

    /// A deny rule wins over everything; an ask rule asks; the critical-path
    /// refusal holds without any rule. No mode is consulted here at all:
    /// these hold in every mode, Bypass included.
    #[test]
    fn hard_limits_come_from_rules_and_paths_not_from_the_mode() {
        let rules = UserRules {
            deny: vec![Rule::parse("shell(curl*)").expect("rule")],
            ask: vec![Rule::parse("shell(git push*)").expect("rule")],
        };
        let h = home();
        let w = ws();
        let lim = |c: &PendingCall| hard_limit(&rules, c, Some(&h), Some(&w));
        assert!(matches!(
            lim(&shell(&["curl", "x"])),
            Some(HardLimit::Deny(_))
        ));
        assert!(matches!(
            lim(&shell(&["git", "push"])),
            Some(HardLimit::Ask(_))
        ));
        assert!(matches!(
            lim(&shell(&["rm", "-rf", "/"])),
            Some(HardLimit::Deny(_))
        ));
        assert_eq!(lim(&shell(&["cargo", "test"])), None);
        // A command line in one string is judged as the words the shell tool
        // runs, so neither a rule nor the deletion limit is dodged by it.
        assert!(matches!(lim(&shell(&["curl x"])), Some(HardLimit::Deny(_))));
        assert!(matches!(
            lim(&shell(&["rm -rf /"])),
            Some(HardLimit::Deny(_))
        ));
        let in_home = format!("rm -rf {}", h.display());
        assert!(matches!(
            lim(&shell(&[in_home.as_str()])),
            Some(HardLimit::Deny(_))
        ));
    }

    fn program(p: &str) -> Option<RefusedSubject> {
        Some(RefusedSubject::Program(p.to_owned()))
    }

    #[test]
    fn a_refusal_names_the_program_of_the_command_a_rule_matched() {
        let rules = UserRules {
            deny: vec![Rule::parse("shell(python3*)").expect("rule")],
            ask: vec![Rule::parse("shell(git push*)").expect("rule")],
        };
        let reason_of = |c: &PendingCall| match hard_limit(&rules, c, Some(&home()), Some(&ws())) {
            Some(HardLimit::Deny(r) | HardLimit::Ask(r)) => r,
            None => panic!("expected a limit"),
        };
        let rules_hook = PERMISSION_RULES_HOOK;
        // A deny rule on one command of a script names that command.
        let wrapped = shell(&["sh", "-c", "mkdir x && python3 -c pass"]);
        assert_eq!(
            refused_subject("shell", &wrapped.input, rules_hook, &reason_of(&wrapped)),
            program("python3")
        );
        // An ask rule too, also through `-lc` and a pipe, and when the user
        // (or an unattended run) denied the ask, whose reason begins with it.
        let ask = shell(&["bash", "-lc", "echo hi | git push origin main"]);
        let asked = format!("{} Nobody is attached to answer.", reason_of(&ask));
        assert_eq!(
            refused_subject("shell", &ask.input, crate::approval::USER, &asked),
            program("git")
        );
        // A plain call is its own program.
        let plain = shell(&["python3", "x.py"]);
        assert_eq!(
            refused_subject("shell", &plain.input, rules_hook, &reason_of(&plain)),
            program("python3")
        );
        // A rule on the shell itself names the shell.
        let sh_rules = UserRules {
            deny: vec![Rule::parse("shell(sh*)").expect("rule")],
            ask: vec![],
        };
        let sh = shell(&["sh", "-c", "mkdir x"]);
        let Some(HardLimit::Deny(reason)) = hard_limit(&sh_rules, &sh, Some(&home()), Some(&ws()))
        else {
            panic!("expected a deny");
        };
        assert_eq!(
            refused_subject("shell", &sh.input, rules_hook, &reason),
            program("sh")
        );
        // A deletion limit names the deleting command, not its shell.
        let rm = shell(&["sh", "-c", "mkdir x && rm -rf /usr"]);
        let Some(HardLimit::Deny(reason)) =
            hard_limit(&UserRules::default(), &rm, Some(&home()), Some(&ws()))
        else {
            panic!("expected a deny");
        };
        assert_eq!(
            refused_subject("shell", &rm.input, rules_hook, &reason),
            program("rm")
        );
        // Another tool, or a reason that quotes no rule, leaves the call's own program.
        assert_eq!(
            refused_subject("apply_patch", &wrapped.input, rules_hook, "x"),
            None
        );
        assert_eq!(
            refused_subject("shell", &wrapped.input, rules_hook, "no rule"),
            None
        );
    }

    /// A refusal of the whole tool keeps the tool and its scope: a skill's
    /// `allowed-tools` (decision 0053), even when its reason goes on to quote
    /// a rule that matched one command, and a user rule with no pattern. Only
    /// a command-level match names a program; a reason that merely contains
    /// a rule's text further on, or a hook that is not a rule's, names none.
    #[test]
    fn a_tool_level_refusal_keeps_the_tool_and_its_scope() {
        let wrapped = shell(&["sh", "-c", "mkdir x && python3 -c pass"]);
        let composed = "tool `shell` is not permitted by skill `read-only-audit`'s allowed-tools; \
                        `user-permission-rules` also refused it: your deny rule \
                        `shell(python3*)` blocks this call in every permission mode";
        assert_eq!(
            refused_subject(
                "shell",
                &wrapped.input,
                crate::approval::SKILLS_POLICY,
                composed
            ),
            Some(RefusedSubject::Tool)
        );
        // The same text under another hook is no rule's refusal.
        assert_eq!(
            refused_subject("shell", &wrapped.input, "mcp-call-review", composed),
            None
        );
        assert_eq!(
            refused_subject("shell", &wrapped.input, PERMISSION_RULES_HOOK, composed),
            None
        );
        // A rule on every call of the tool refuses the tool.
        for text in ["shell", "*"] {
            let rules = UserRules {
                deny: vec![Rule::parse(text).expect("rule")],
                ask: vec![],
            };
            let Some(HardLimit::Deny(reason)) =
                hard_limit(&rules, &wrapped, Some(&home()), Some(&ws()))
            else {
                panic!("expected a deny for {text}");
            };
            assert_eq!(
                refused_subject("shell", &wrapped.input, PERMISSION_RULES_HOOK, &reason),
                Some(RefusedSubject::Tool),
                "{text}"
            );
        }
        // A rule on a file tool's path refuses that path.
        let rules = UserRules {
            deny: vec![Rule::parse("apply_patch(secrets/*)").expect("rule")],
            ask: vec![],
        };
        let patch = PendingCall {
            tool: "apply_patch".into(),
            input: json!({"patch": "*** Begin Patch\n*** Add File: notes.md\n+a\n*** Add File: secrets/key.txt\n+b\n*** End Patch"}),
            ..shell(&["x"])
        };
        let Some(HardLimit::Deny(reason)) = hard_limit(&rules, &patch, Some(&home()), Some(&ws()))
        else {
            panic!("expected a deny");
        };
        assert_eq!(
            refused_subject("apply_patch", &patch.input, PERMISSION_RULES_HOOK, &reason),
            Some(RefusedSubject::Path("secrets/key.txt".into()))
        );
        // The command policy's script lead names a program only from the
        // command policy or the user's denial of its ask, read from its last
        // occurrence: a model-written command line quoting the lead earlier
        // does not choose the program.
        let tricky = shell(&["sh", "-c", "echo 'script runs `mkdir`' && curl x"]);
        let question = "Run `sh -c 'echo ... script runs `mkdir` ...'`? The script runs `curl` \
                        (`curl x`), which no grant covers.";
        assert_eq!(
            refused_subject(
                "shell",
                &tricky.input,
                crate::shell::SHELL_POLICY_HOOK,
                question
            ),
            program("curl")
        );
        assert_eq!(
            refused_subject("shell", &tricky.input, crate::approval::USER, question),
            program("curl")
        );
        assert_eq!(
            refused_subject("shell", &tricky.input, "some-hook", question),
            None
        );
    }

    fn script(s: &str) -> Script {
        script_commands(&argv(&["sh", "-c", s]))
    }

    fn programs_of(s: &str) -> Vec<String> {
        match script(s) {
            Script::Commands(c) => c.into_iter().map(|w| w[0].clone()).collect(),
            other => panic!("{s:?} should be judged: {other:?}"),
        }
    }

    /// The commands a mode may grant one by one: simple commands joined by
    /// `;`, `&&`, `||`, `|` or newlines, with quoted arguments, argument
    /// globs, and output redirections into the worktree or `/dev/null`.
    #[test]
    fn a_plain_script_is_read_as_its_commands() {
        assert_eq!(
            programs_of("mkdir -p out && cp templates/greeting.txt out/greeting.txt"),
            ["mkdir", "cp"]
        );
        assert_eq!(
            programs_of("mkdir a; touch a/b || echo no | wc -l\nls"),
            ["mkdir", "touch", "echo", "wc", "ls"]
        );
        assert_eq!(
            programs_of("python3 -c 'print(1); x = \"$HOME\"' && cp src/*.txt out/"),
            ["python3", "cp"]
        );
        assert_eq!(
            programs_of("python3 gen.py > out/data.json 2>/dev/null && cat out/data.json 2>&1"),
            ["python3", "cat"]
        );
        assert_eq!(programs_of("mkdir x &&\n cp a x/"), ["mkdir", "cp"]);
        assert_eq!(programs_of("echo hi>>out/log.txt;"), ["echo"]);
        // The other shells, by bare name, with exactly `-c`.
        for sh in ["bash", "zsh", "dash"] {
            assert!(matches!(
                script_commands(&argv(&[sh, "-c", "mkdir x"])),
                Script::Commands(_)
            ));
        }
        assert_eq!(script_commands(&argv(&["mkdir", "x"])), Script::NotAScript);
        assert_eq!(
            script_commands(&argv(&["fish", "-c", "mkdir x"])),
            Script::NotAScript
        );
    }

    /// Everything the per-command check does not judge is refused by name,
    /// so the call is asked about whole.
    #[test]
    fn a_script_with_any_other_construct_is_not_judged() {
        for (s, why) in [
            ("mkdir x && $(curl x)", "expands"),
            ("mkdir x && echo `id`", "expands"),
            ("echo \"$HOME\"", "double-quoted"),
            ("mkdir x & cp a b", "background"),
            ("mkdir x |& cat", "|&"),
            ("(mkdir x) && cp a b", "subshell"),
            ("{ mkdir x; }", "brace"),
            ("cp a b < in.txt", "input"),
            ("m\\kdir x", "backslash"),
            ("mkdir x # note", "comment"),
            ("! mkdir x", "`!`"),
            ("if true; then mkdir x; fi", "keyword"),
            ("mkdir x &&", "no command after"),
            ("&& mkdir x", "no command"),
            ("mkdir x;; cp a b", ";;"),
            ("mkdir x; ; cp a b", "no command"),
            ("PATH=/tmp mkdir x", "plain program"),
            ("./mkdir x", "plain program"),
            ("/bin/mkdir x", "plain program"),
            ("'mkdir' x", "plain program"),
            ("m* x", "plain program"),
            ("echo x > /etc/passwd", "outside the worktree"),
            ("echo x > ../up.txt", "outside the worktree"),
            ("echo x > ~/.profile", "glob, `~`"),
            ("echo x > out/*.txt", "glob, `~`"),
            ("echo x > .git/config", "may not write"),
            ("echo x >| out.txt", ">|"),
            ("echo x >&out.txt", ">&"),
            ("echo x >", "names no file"),
            ("cd sub && echo x > out.txt", "changing directory"),
            ("echo 'open", "not closed"),
            ("", "no command"),
            ("mkdir x\u{1b}[2K", "control"),
        ] {
            match script(s) {
                Script::Unjudged(reason) => assert!(reason.contains(why), "{s:?}: {reason}"),
                other => panic!("{s:?} must not be judged: {other:?}"),
            }
        }
        for (a, why) in [
            (argv(&["/bin/sh", "-c", "mkdir x"]), "path"),
            (argv(&["bash", "-lc", "mkdir x"]), "options"),
            (argv(&["sh", "-e", "-c", "mkdir x"]), "options"),
            (argv(&["sh", "-c", "mkdir \"$0\"", "x"]), "arguments follow"),
        ] {
            match script_commands(&a) {
                Script::Unjudged(reason) => assert!(reason.contains(why), "{a:?}: {reason}"),
                other => panic!("{a:?} must not be judged: {other:?}"),
            }
        }
        // `cd` without a file redirection, and `/dev/null` after a `cd`, are
        // still judged: nothing they write depends on the directory.
        assert_eq!(programs_of("cd sub && ls 2>/dev/null"), ["cd", "ls"]);
    }

    fn only(c: ScriptConstruct) -> ScriptConstructs {
        ScriptConstructs([c].into_iter().collect())
    }

    fn all_but(c: ScriptConstruct) -> ScriptConstructs {
        ScriptConstructs(
            ScriptConstruct::ALL
                .into_iter()
                .filter(|x| *x != c)
                .collect(),
        )
    }

    fn all() -> ScriptConstructs {
        ScriptConstructs(ScriptConstruct::ALL.into_iter().collect())
    }

    fn read_with(s: &str, constructs: &ScriptConstructs) -> Script {
        script_commands_with(&argv(&["sh", "-c", s]), constructs)
    }

    /// Scripts each construct switch lets a mode judge, with the programs
    /// kGraf can see in them, in the order their commands end.
    fn construct_cases() -> Vec<(ScriptConstruct, &'static str, Vec<&'static str>)> {
        use ScriptConstruct::*;
        vec![
            (
                Variables,
                "mkdir x && cp $(ls src) x/",
                vec!["mkdir", "ls", "cp"],
            ),
            (Variables, "echo \"$HOME\" `date`", vec!["date", "echo"]),
            (Variables, "$CMD x && mkdir y", vec!["mkdir"]),
            (Variables, "diff <(ls a) b", vec!["ls", "diff"]),
            (Variables, "echo $((1 + 2)) ${HOME}", vec!["echo"]),
            (
                Subshells,
                "(mkdir x && touch x/a) && { cp a b; }",
                vec!["mkdir", "touch", "cp"],
            ),
            (Subshells, "cp a{1,2} out/", vec!["cp"]),
            (Background, "mkdir x & cp a b", vec!["mkdir", "cp"]),
            (Background, "mkdir x |& cat", vec!["mkdir", "cat"]),
            (InputRedirects, "wc -l < in.txt", vec!["wc"]),
            (
                InputRedirects,
                "cat <<EOF > out/x\nhello\nEOF\nls",
                vec!["cat", "ls"],
            ),
            (InputRedirects, "cat <<'EOF'\n$(id) `id`\nEOF", vec!["cat"]),
            (InputRedirects, "grep a <<< 'a b'", vec!["grep"]),
            (Escapes, "mkdir x # note", vec!["mkdir"]),
            (Escapes, "! mkdir x", vec!["mkdir"]),
            (Escapes, "PATH=/tmp mkdir x", vec!["mkdir"]),
            (Escapes, "/bin/mkdir x", vec!["/bin/mkdir"]),
            (Escapes, "'mkdir' x", vec!["mkdir"]),
            (Escapes, "m\\kdir x", vec!["mkdir"]),
            (OutsideRedirects, "echo x > /etc/passwd", vec!["echo"]),
            (
                OutsideRedirects,
                "cd sub && echo x > out.txt",
                vec!["cd", "echo"],
            ),
            (OutsideRedirects, "echo x > .git/config", vec!["echo"]),
            (OutsideRedirects, "echo x >| out.txt", vec!["echo"]),
        ]
    }

    /// Each switch is the gate for its construct: off (and with every other
    /// switch on) the script is not judged, as before; on, it is read as the
    /// commands whose programs kGraf can see.
    #[test]
    fn each_script_construct_is_judged_only_with_its_switch_on() {
        for (c, s, programs) in construct_cases() {
            assert!(
                matches!(
                    read_with(s, &ScriptConstructs::default()),
                    Script::Unjudged(_)
                ),
                "{c}: {s:?} judged with every switch off"
            );
            assert!(
                matches!(script(s), Script::Unjudged(_)),
                "{c}: {s:?} judged by the strict reader"
            );
            match read_with(s, &all_but(c)) {
                Script::Unjudged(_) => {}
                other => panic!("{c}: {s:?} judged without its switch: {other:?}"),
            }
            match read_with(s, &only(c)) {
                Script::Commands(cmds) => assert_eq!(
                    cmds.iter().map(|w| w[0].as_str()).collect::<Vec<_>>(),
                    programs,
                    "{c}: {s:?}"
                ),
                other => panic!("{c}: {s:?} not judged with its switch on: {other:?}"),
            }
            assert!(relies_on_constructs(&argv(&["sh", "-c", s]), &only(c)));
        }
        // A plain script reads the same with any switch on, and does not
        // rely on one.
        let plain = "mkdir -p out && cp a out/a";
        assert_eq!(read_with(plain, &all()), script(plain));
        assert!(!relies_on_constructs(&argv(&["sh", "-c", plain]), &all()));
    }

    /// What no switch opens: shell keywords, function definitions, a script
    /// with no program kGraf can see, a heredoc or a parameter expansion
    /// that could hide a command, a glob as a program, and the call shapes
    /// the strict reader refuses.
    #[test]
    fn some_scripts_are_never_judged_whatever_the_switches() {
        for (s, why) in [
            ("if true; then mkdir x; fi", "keyword"),
            ("f() { ls; }", "function"),
            ("$CMD x", "no program"),
            ("cat <<EOF\n$(id)\nEOF", "substitutes a command"),
            ("cat <<EOF\nhello", "not closed"),
            ("cat <<EOF", "no body"),
            ("echo ${X:-$(id)}", "parameter expansion"),
            ("m* x", "plain program"),
            ("(mkdir x", "not closed"),
            ("mkdir x)", "subshell"),
            ("{ mkdir x", "not closed"),
            ("( ls ) x", "follows a subshell"),
            ("echo `ls \\`", "backslash"),
            ("mkdir x;; cp a b", ";;"),
            ("mkdir x &&", "no command after"),
        ] {
            match read_with(s, &all()) {
                Script::Unjudged(reason) => assert!(reason.contains(why), "{s:?}: {reason}"),
                other => panic!("{s:?} must not be judged: {other:?}"),
            }
        }
        // A redirection to a path kGraf cannot resolve needs the outside
        // switch even with variables on.
        let s = "echo x > $OUT";
        assert!(
            matches!(read_with(s, &only(ScriptConstruct::Variables)), Script::Unjudged(r) if r.contains("cannot resolve"))
        );
        assert!(matches!(read_with(s, &all()), Script::Commands(_)));
        for a in [
            argv(&["/bin/sh", "-c", "mkdir $x"]),
            argv(&["sh", "-c", "mkdir \"$0\"", "x"]),
        ] {
            assert!(matches!(
                script_commands_with(&a, &all()),
                Script::Unjudged(_)
            ));
        }
    }

    fn rules(deny: &[&str], ask: &[&str]) -> UserRules {
        UserRules {
            deny: deny.iter().map(|r| Rule::parse(r).expect("rule")).collect(),
            ask: ask.iter().map(|r| Rule::parse(r).expect("rule")).collect(),
        }
    }

    fn hook_with(
        mode: PermissionMode,
        allow: &[&str],
        user: UserRules,
        constructs: ScriptConstructs,
    ) -> (Arc<PermissionControl>, PermissionRulesHook) {
        let settings = PermissionSettings {
            mode,
            allow: AllowSet::default()
                .with_programs(allow.iter().copied())
                .expect("allow"),
            rules: user,
            script_constructs: constructs,
            ..PermissionSettings::default()
        };
        let control = Arc::new(
            PermissionControl::new(settings, ModeSource::CommandLine, ProcessFacts::default())
                .expect("control"),
        );
        let hook = PermissionRulesHook::new(control.clone(), Some(ws())).with_home(Some(home()));
        (control, hook)
    }

    /// With a switch on, the mode grants a script whose visible programs
    /// are allowed, but a deny rule on a program inside `$(...)`, in a
    /// group or in a background job still refuses the call, and an ask rule
    /// still asks: the rules hook runs after the mode, in every mode but
    /// Bypass, and matches the program by name in the script text.
    #[tokio::test]
    async fn a_rule_on_a_program_inside_a_construct_still_refuses_or_asks() {
        use crate::execpolicy::{Decision, ExecPolicy};
        let allowed = ["mkdir", "cp", "curl", "ls", "/usr/bin/curl"];
        for (c, s) in [
            (ScriptConstruct::Variables, "mkdir x && cp $(curl -s u) x/"),
            (ScriptConstruct::Variables, "mkdir x && cp `curl -s u` x/"),
            (ScriptConstruct::Subshells, "mkdir x && { curl -s u; }"),
            (ScriptConstruct::Background, "curl -s u & mkdir x"),
            (ScriptConstruct::Escapes, "mkdir x && /usr/bin/curl -s u"),
        ] {
            let (control, hook) = hook_with(
                PermissionMode::Auto,
                &allowed,
                rules(&["shell(curl*)"], &[]),
                only(c),
            );
            let policy = ExecPolicy::default_kgraf_policy().with_permissions(control);
            let a = argv(&["sh", "-c", s]);
            // The mode alone would grant it: every visible program is allowed.
            let v = policy.evaluate(&a);
            assert!(
                v.decision == Decision::Allow && v.granted,
                "{c}: {s:?}: {v:?}"
            );
            match hook.review(&shell(&["sh", "-c", s])).await {
                HookDecision::Deny { reason } => assert!(
                    reason.starts_with("your deny rule `shell(curl*)`"),
                    "{c}: {s:?}: {reason}"
                ),
                other => panic!("{c}: {s:?} must be refused: {other:?}"),
            }
            let (_, asking) = hook_with(
                PermissionMode::Auto,
                &allowed,
                rules(&[], &["shell(curl*)"]),
                only(c),
            );
            assert!(
                matches!(
                    asking.review(&shell(&["sh", "-c", s])).await,
                    HookDecision::Ask { .. }
                ),
                "{c}: {s:?}"
            );
        }
        // The text match reaches what a variable will run: a denied program
        // named only in an assignment refuses too.
        let (_, hook) = hook_with(
            PermissionMode::Auto,
            &allowed,
            rules(&["shell(curl*)"], &[]),
            ScriptConstructs(
                [ScriptConstruct::Variables, ScriptConstruct::Escapes]
                    .into_iter()
                    .collect(),
            ),
        );
        assert!(matches!(
            hook.review(&shell(&["sh", "-c", "C=curl; ls && $C u"]))
                .await,
            HookDecision::Deny { .. }
        ));
        // The name match applies only to a script judged because of a
        // switch: with the switches off, or for a plain script, or in Bypass,
        // it does not apply. The rules themselves still see a command inside
        // a group ([`command_starts`]), in Bypass too.
        let s = "mkdir x && { curl -s u; }";
        let (_, off) = hook_with(
            PermissionMode::Auto,
            &allowed,
            rules(&["shell(curl*)"], &[]),
            ScriptConstructs::default(),
        );
        assert_eq!(
            construct_rule_limit(
                &off_rules(),
                &ScriptConstructs::default(),
                &shell(&["sh", "-c", s])
            ),
            None
        );
        assert_eq!(
            off.review(&shell(&["sh", "-c", "mkdir x && ls"])).await,
            HookDecision::Allow
        );
        let (_, bypass) = hook_with(
            PermissionMode::Bypass,
            &[],
            rules(&["shell(curl*)"], &[]),
            only(ScriptConstruct::Subshells),
        );
        match bypass.review(&shell(&["sh", "-c", s])).await {
            HookDecision::Deny { reason } => assert_eq!(
                reason,
                "your deny rule `shell(curl*)` blocks this call in every permission mode"
            ),
            other => panic!("Bypass must refuse `curl` inside a group: {other:?}"),
        }
        assert_eq!(
            bypass
                .review(&shell(&["sh", "-c", "mkdir x && { ls; }"]))
                .await,
            HookDecision::Allow
        );
    }

    fn off_rules() -> UserRules {
        rules(&["shell(curl*)"], &[])
    }

    /// A deletion of a system path inside a group or after `!` is refused
    /// as it is in a plain command.
    #[test]
    fn deleting_a_system_path_inside_a_group_is_refused() {
        for s in ["{ rm -rf ~; }", "! rm -rf /", "( rm -rf / )"] {
            assert!(
                matches!(limit(&["sh", "-c", s]), Some(HardLimit::Deny(_))),
                "{s:?}"
            );
        }
    }

    /// The switches live in the user-level file, off unless listed, and an
    /// unknown name is refused rather than ignored; the command-line list
    /// takes either spelling or `none`.
    #[test]
    fn script_constructs_round_trip_through_the_user_file() {
        let none = PermissionSettings::from_toml_str("").expect("empty");
        assert!(none.script_constructs.is_empty());
        assert!(
            !none
                .to_toml_string()
                .expect("toml")
                .contains("script_constructs")
        );
        let s = PermissionSettings::from_toml_str(
            "script_constructs = [\"variables\", \"input_redirects\"]",
        )
        .expect("file");
        assert!(s.script_constructs.allows(ScriptConstruct::Variables));
        assert!(s.script_constructs.allows(ScriptConstruct::InputRedirects));
        assert!(!s.script_constructs.allows(ScriptConstruct::Background));
        let again =
            PermissionSettings::from_toml_str(&s.to_toml_string().expect("toml")).expect("again");
        assert_eq!(again, s);
        assert!(PermissionSettings::from_toml_str("script_constructs = [\"evals\"]").is_err());
        assert_eq!(
            ScriptConstructs::parse_list("input-redirects,escapes").expect("list"),
            ScriptConstructs(
                [ScriptConstruct::InputRedirects, ScriptConstruct::Escapes]
                    .into_iter()
                    .collect()
            )
        );
        assert!(
            ScriptConstructs::parse_list("none")
                .expect("none")
                .is_empty()
        );
        assert!(ScriptConstructs::parse_list("evals").is_err());
        assert!(ScriptConstructs::parse_list(",").is_err());
    }

    #[test]
    fn programs_run_lists_every_command_of_a_script() {
        let c = shell(&[
            "sh",
            "-c",
            "A=1 mkdir x && python3 -c pass; /usr/bin/env ls",
        ]);
        assert_eq!(
            programs_run(&c.input),
            ["sh", "mkdir", "python3", "/usr/bin/env"]
        );
    }

    #[tokio::test]
    async fn the_hook_reads_the_live_rules_in_bypass() {
        let settings = PermissionSettings {
            mode: PermissionMode::Bypass,
            rules: UserRules {
                deny: vec![Rule::parse("shell(python3*)").expect("rule")],
                ask: vec![],
            },
            ..PermissionSettings::default()
        };
        let control = Arc::new(
            PermissionControl::new(settings, ModeSource::CommandLine, ProcessFacts::default())
                .expect("bypass from the command line"),
        );
        let hook = PermissionRulesHook::new(control, Some(ws())).with_home(Some(home()));
        assert!(matches!(
            hook.review(&shell(&["python3", "x.py"])).await,
            HookDecision::Deny { .. }
        ));
        assert!(matches!(
            hook.review(&shell(&["rm", "-rf", "~"])).await,
            HookDecision::Deny { .. }
        ));
        assert_eq!(hook.review(&shell(&["ls"])).await, HookDecision::Allow);
    }

    #[test]
    fn bypass_is_refused_when_disabled_as_root_or_without_the_desktop_steps() {
        let facts = ProcessFacts::default();
        let mut s = PermissionSettings::default();
        assert!(bypass_allowed(&s, ModeSource::CommandLine, facts).is_ok());
        assert_eq!(
            bypass_allowed(&s, ModeSource::UserInterface, facts),
            Err(ModeError::BypassNotAllowed)
        );
        s.allow_bypass = true;
        assert_eq!(
            bypass_allowed(&s, ModeSource::UserInterface, facts),
            Err(ModeError::BypassWarningNotAccepted)
        );
        s.bypass_warning_accepted = true;
        assert!(bypass_allowed(&s, ModeSource::UserInterface, facts).is_ok());
        let root = ProcessFacts {
            is_root: true,
            container_statement: false,
        };
        for source in [
            ModeSource::UserInterface,
            ModeSource::UserConfig,
            ModeSource::CommandLine,
        ] {
            assert_eq!(
                bypass_allowed(&s, source, root),
                Err(ModeError::RootOutsideContainer)
            );
        }
        let root_in_container = ProcessFacts {
            is_root: true,
            container_statement: true,
        };
        assert!(bypass_allowed(&s, ModeSource::CommandLine, root_in_container).is_ok());
        s.disable_bypass = true;
        for source in [
            ModeSource::UserInterface,
            ModeSource::UserConfig,
            ModeSource::CommandLine,
        ] {
            assert_eq!(
                bypass_allowed(&s, source, root_in_container),
                Err(ModeError::BypassDisabled)
            );
        }
        // A control refuses to start in a Bypass it could not switch to.
        let settings = PermissionSettings {
            mode: PermissionMode::Bypass,
            ..PermissionSettings::default()
        };
        assert!(PermissionControl::new(settings, ModeSource::CommandLine, root).is_err());
    }

    #[test]
    fn a_mode_change_is_recorded_and_wakes_subscribers() {
        let c = PermissionControl::manual();
        let mut rx = c.subscribe();
        let before = *rx.borrow_and_update();
        c.set_mode(PermissionMode::Auto, ModeSource::UserInterface)
            .expect("auto");
        assert!(rx.has_changed().expect("sender alive"));
        assert_ne!(*rx.borrow_and_update(), before);
        let events = c.events_since(0);
        assert_eq!(
            events.last().map(|(_, e)| e.clone()),
            Some(PermissionEvent::ModeChanged {
                from: PermissionMode::Manual,
                to: PermissionMode::Auto,
                source: ModeSource::UserInterface
            })
        );
        // A grant is recorded once per worker, subject and mode.
        assert!(c.record_grant("w", "`python3`", "mode").is_some());
        assert!(c.record_grant("w", "`python3`", "mode").is_none());
        assert_eq!(c.events_since(c.last_seq() - 1).len(), 1);
    }

    #[test]
    fn each_mode_routes_an_ask() {
        let c = PermissionControl::manual();
        // Inline code the mode can read whole: a script file is read from
        // the worker's directory, which routing does not know (below).
        let py = json!({"command": ["python3", "-c", "print(1)"]});
        let py_file = json!({"command": ["python3", "x.py"]});
        let mk = json!({"command": ["mkdir", "out"]});
        let patch = json!({"patch": "*** Begin Patch\n*** Add File: src/a.rs\n+x\n*** End Patch"});
        assert_eq!(c.route_ask("shell", &py), AskRoute::User);
        assert_eq!(c.route_ask("apply_patch", &patch), AskRoute::User);
        c.set_mode(PermissionMode::AcceptEdits, ModeSource::CommandLine)
            .expect("mode");
        assert_eq!(c.route_ask("shell", &py), AskRoute::User);
        assert_eq!(c.route_ask("shell", &mk), AskRoute::Grant { by: "mode" });
        assert_eq!(
            c.route_ask("apply_patch", &patch),
            AskRoute::Grant { by: "mode" }
        );
        assert_eq!(c.route_ask("mcp__web__fetch", &json!({})), AskRoute::User);
        c.set_mode(PermissionMode::Auto, ModeSource::CommandLine)
            .expect("mode");
        assert_eq!(c.route_ask("shell", &py), AskRoute::User);
        c.set_allow(
            AllowSet::default()
                .with_programs(["python3"])
                .expect("allow"),
        );
        assert_eq!(c.route_ask("shell", &py), AskRoute::Grant { by: "mode" });
        // A script file kGraf cannot read here is granted by no allow set
        // (decision 0094, amended 2026-10-06); the worker's command policy,
        // which reads it from the worktree, grants it there.
        assert_eq!(c.route_ask("shell", &py_file), AskRoute::User);
        assert_eq!(c.route_ask("shell", &mk), AskRoute::User);
        c.set_mode(PermissionMode::OrchestratorDecides, ModeSource::CommandLine)
            .expect("mode");
        assert_eq!(
            c.route_ask("mcp__web__fetch", &json!({})),
            AskRoute::Grant { by: "orchestrator" }
        );
        c.set_mode(PermissionMode::Bypass, ModeSource::CommandLine)
            .expect("mode");
        assert_eq!(
            c.route_ask("anything", &json!({})),
            AskRoute::Grant { by: "mode" }
        );
        assert!(c.shell_unrestricted());
    }

    #[test]
    fn the_user_file_allow_list_keeps_its_tools_and_paths() {
        let s = PermissionSettings::from_toml_str(
            "[allow]\ntools = [\"mcp__web__fetch\"]\npaths = [\"docs/*\", \"Cargo.toml\"]\n",
        )
        .expect("parse");
        assert!(s.allow.tools.contains("mcp__web__fetch"));
        assert_eq!(s.allow.paths, vec!["docs/*", "Cargo.toml"]);
    }

    #[test]
    fn the_user_file_round_trips_refuses_unknown_keys_and_refuses_a_project_path() {
        let text = r#"
mode = "auto"
plan_review = true
deny = ["shell(curl*)"]
ask = ["shell(git push*)"]
[allow]
programs = ["python3"]
"#;
        let s = PermissionSettings::from_toml_str(text).expect("parse");
        assert_eq!(s.mode, PermissionMode::Auto);
        assert!(s.plan_review && s.allow.allows_program("python3"));
        assert_eq!(s.rules.deny.len(), 1);
        let again =
            PermissionSettings::from_toml_str(&s.to_toml_string().expect("ser")).expect("reparse");
        assert_eq!(again, s);
        assert!(PermissionSettings::from_toml_str("mdoe = \"bypass\"").is_err());
        assert!(PermissionSettings::from_toml_str("[allow]\nprograms = [\"python3 -c\"]").is_err());

        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path().join("proj");
        std::fs::create_dir_all(project.join(".kgraf")).expect("mkdir");
        let inside = project.join(".kgraf/permissions.toml");
        std::fs::write(&inside, "mode = \"bypass\"").expect("write");
        let err = PermissionSettings::load(&inside, Some(&project)).expect_err("refused");
        assert!(
            matches!(err, SettingsFileError::InsideProject { .. }),
            "{err}"
        );
        let outside = dir.path().join("user/permissions.toml");
        assert_eq!(
            PermissionSettings::load(&outside, Some(&project)).expect("missing is default"),
            PermissionSettings::default()
        );
        s.save(&outside).expect("save");
        assert_eq!(
            PermissionSettings::load(&outside, Some(&project)).expect("load"),
            s
        );
    }

    #[derive(Clone, Debug, PartialEq)]
    struct At(i64);
    impl Raised for At {
        fn raised_at_ms(&self) -> i64 {
            self.0
        }
    }

    #[test]
    fn requests_within_the_window_of_a_groups_first_are_one_group() {
        let groups =
            group_requests(&[At(5_000), At(0), At(1_500), At(2_000), At(2_001), At(3_900)]);
        assert_eq!(
            groups,
            vec![
                vec![At(0), At(1_500), At(2_000)],
                vec![At(2_001), At(3_900)],
                vec![At(5_000)]
            ]
        );
        assert!(group_requests::<At>(&[]).is_empty());
    }

    #[test]
    fn a_mode_name_parses_in_either_spelling() {
        for m in PermissionMode::ALL {
            assert_eq!(m.as_str().parse::<PermissionMode>(), Ok(m));
            assert_eq!(
                m.as_str().replace('_', "-").parse::<PermissionMode>(),
                Ok(m)
            );
        }
        assert!("dont_ask".parse::<PermissionMode>().is_err());
        assert!("plan".parse::<PermissionMode>().is_err());
    }
}
