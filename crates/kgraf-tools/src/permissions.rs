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

/// One user rule: `tool` (every call of the tool) or `tool(pattern)`, where
/// the pattern matches a call's subject exactly, or as a prefix when it ends
/// in `*`. A shell call's subjects are its command line and every simple
/// command of a `sh -c` script; a file tool's are the paths it writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    tool: String,
    pattern: Option<String>,
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
        let tool = tool.trim();
        if tool.is_empty() || tool.chars().any(|c| c.is_whitespace()) {
            return Err(format!("rule `{t}` names no tool"));
        }
        if pattern.as_deref() == Some("") {
            return Err(format!("rule `{t}` has an empty pattern"));
        }
        Ok(Self {
            tool: tool.to_owned(),
            pattern,
        })
    }

    pub fn text(&self) -> String {
        match &self.pattern {
            None => self.tool.clone(),
            Some(p) => format!("{}({p})", self.tool),
        }
    }

    fn matches(&self, tool: &str, subjects: &[String]) -> bool {
        if self.tool != tool && self.tool != "*" {
            return false;
        }
        match &self.pattern {
            None => true,
            Some(p) => subjects.iter().any(|s| match p.strip_suffix('*') {
                Some(prefix) => s.starts_with(prefix),
                None => s == p,
            }),
        }
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
    /// Shell programs, each an exact `argv[0]`.
    pub programs: BTreeSet<String>,
    /// Tools by wire name.
    pub tools: BTreeSet<String>,
    /// Repository paths a contract's write scope may be widened to: exact,
    /// or a prefix when the entry ends in `*`.
    pub paths: Vec<String>,
}

impl AllowSet {
    /// Add programs, each a plain program name (no whitespace).
    pub fn with_programs<I, S>(mut self, programs: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for p in programs {
            let p = p.into();
            if p.is_empty() || p.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!(
                    "`{}` is not a program name: an allow set lists programs, each an exact \
                     argv[0], with no spaces",
                    p.escape_debug()
                ));
            }
            self.programs.insert(p);
        }
        Ok(self)
    }

    pub fn allows_program(&self, program: &str) -> bool {
        self.programs.contains(program)
    }

    pub fn allows_tool(&self, tool: &str) -> bool {
        self.tools.contains(tool)
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

impl PermissionSettings {
    /// Parse the user-level file's text.
    pub fn from_toml_str(text: &str) -> Result<Self, String> {
        let f: PermissionsFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let rules = |list: &[String]| -> Result<Vec<Rule>, String> {
            list.iter().map(|r| Rule::parse(r)).collect()
        };
        Ok(Self {
            mode: f.mode.unwrap_or_default(),
            allow: AllowSet {
                tools: f.allow.tools.into_iter().collect(),
                paths: f.allow.paths,
                ..AllowSet::default()
            }
            .with_programs(f.allow.programs)?,
            rules: UserRules {
                deny: rules(&f.deny)?,
                ask: rules(&f.ask)?,
            },
            plan_review: f.plan_review,
            disable_bypass: f.disable_bypass,
            allow_bypass: f.allow_bypass,
            bypass_warning_accepted: f.bypass_warning_accepted,
        })
    }

    pub fn to_toml_string(&self) -> Result<String, String> {
        let f = PermissionsFile {
            mode: Some(self.mode),
            plan_review: self.plan_review,
            disable_bypass: self.disable_bypass,
            allow_bypass: self.allow_bypass,
            bypass_warning_accepted: self.bypass_warning_accepted,
            allow: AllowFile {
                programs: self.allow.programs.iter().cloned().collect(),
                tools: self.allow.tools.iter().cloned().collect(),
                paths: self.allow.paths.clone(),
            },
            deny: self.rules.deny.iter().map(Rule::text).collect(),
            ask: self.rules.ask.iter().map(Rule::text).collect(),
        };
        toml::to_string(&f).map_err(|e| e.to_string())
    }

    /// Load the user-level file at `path`. A missing file is the defaults
    /// (Manual, no rules). A file inside `project_root` is refused: a worker
    /// can write inside the project, and the mode is the user's alone.
    pub fn load(path: &Path, project_root: Option<&Path>) -> Result<Self, SettingsFileError> {
        refuse_inside_project(path, project_root)?;
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml_str(&text).map_err(|reason| SettingsFileError::Invalid {
                path: path.to_owned(),
                reason,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
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

    /// Replace Auto's allow set (the user edited it).
    pub fn set_allow(&self, allow: AllowSet) {
        self.state().settings.allow = allow;
        self.bump();
    }

    pub fn rules(&self) -> UserRules {
        self.state().settings.rules.clone()
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
    /// (the program of the command a rule matched, [`refused_program`]) as its
    /// subject instead of the grant's subject: the call's own program may be a
    /// shell that refused nothing.
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

    /// Whether the current mode lets the shell run `program` as a granted,
    /// confined command: Auto's allow set, or Accept edits' file programs.
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
                let granted = allow.allows_tool(tool)
                    || (tool == "shell"
                        && shell_argv(input)
                            .first()
                            .is_some_and(|p| allow.allows_program(p)))
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
                        && shell_argv(input)
                            .first()
                            .is_some_and(|p| FILE_EDIT_PROGRAMS.contains(&base_name(p))));
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
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

/// Every simple command a shell call runs, as argv lists: the call's argv
/// itself and, for `sh -c SCRIPT` (or `-lc`, `-ec`), each command of the
/// script split on `;`, `&&`, `||`, `|`, `&` and newlines. A script that
/// cannot be split (unbalanced quotes) yields its words split on whitespace,
/// so a rule still sees them.
pub fn simple_commands(argv: &[String]) -> Vec<Vec<String>> {
    let mut out = vec![argv.to_vec()];
    let Some(program) = argv.first() else {
        return out;
    };
    if !SHELLS.contains(&base_name(program)) {
        return out;
    }
    let mut it = argv.iter().skip(1);
    while let Some(arg) = it.next() {
        if arg.starts_with('-') && arg.contains('c') && !arg.starts_with("--") {
            if let Some(script) = it.next() {
                for segment in split_script(script) {
                    let words = shlex::split(&segment)
                        .unwrap_or_else(|| segment.split_whitespace().map(str::to_owned).collect());
                    if !words.is_empty() {
                        out.push(words);
                    }
                }
            }
            break;
        }
    }
    out
}

/// Split a script on command separators outside quotes.
fn split_script(script: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let (mut single, mut double) = (false, false);
    let chars: Vec<char> = script.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            ';' | '\n' | '|' | '&' if !single && !double => {
                parts.push(std::mem::take(&mut cur));
                // `&&` and `||` are one separator.
                if (c == '&' || c == '|') && chars.get(i + 1) == Some(&c) {
                    i += 1;
                }
                i += 1;
                continue;
            }
            // `$(...)` and backticks run commands too: split there so the
            // inner command is seen.
            '`' if !single => {
                parts.push(std::mem::take(&mut cur));
                i += 1;
                continue;
            }
            '(' | ')' if !single && !double => {
                parts.push(std::mem::take(&mut cur));
                i += 1;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        i += 1;
    }
    parts.push(cur);
    parts
        .into_iter()
        .map(|p| p.trim().trim_start_matches('$').trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect()
}

/// The program a simple command runs: its first word that is not a
/// `VAR=value` assignment.
fn program_of_command(words: &[String]) -> Option<&str> {
    words.iter().map(String::as_str).find(|w| !is_assignment(w))
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

/// The text of the rule a refusal reason quotes (`your deny rule `R` ...`,
/// `your ask rule `R` ...`).
fn rule_quoted_in(reason: &str) -> Option<&str> {
    ["deny rule `", "ask rule `"].iter().find_map(|lead| {
        let rest = &reason[reason.find(lead)? + lead.len()..];
        rest.split_once('`').map(|(rule, _)| rule)
    })
}

/// The program a refusal of a shell call is about, when it is not the
/// call's own program: `sh -c "mkdir x && python3 -c pass"` refused by the
/// rule `shell(python3*)` is a refusal of `python3`, not of `sh`, which ran
/// no refused command. The rule the `reason` quotes is matched against the
/// call's command lines the way [`hard_limit`] matched it (the whole call
/// first, then each command of a script); a deletion limit's reason names the
/// first deleting command. `None` for any other tool or reason: the call's
/// program stands.
pub fn refused_program(tool: &str, input: &serde_json::Value, reason: &str) -> Option<String> {
    if tool != "shell" {
        return None;
    }
    let commands = simple_commands(&shell_argv(input));
    if let Some(text) = rule_quoted_in(reason) {
        let rule = Rule::parse(text).ok()?;
        return commands
            .iter()
            .find(|c| rule.matches(tool, &[c.join(" ")]))
            .and_then(|c| program_of_command(c))
            .map(str::to_owned);
    }
    if reason.contains(DELETING_LEAD) || reason.contains(DELETES_LEAD) {
        return commands.iter().find_map(|c| {
            let words: Vec<&str> = c.iter().map(String::as_str).collect();
            program_index(&words).map(|at| words[at].to_owned())
        });
    }
    None
}

/// A call's subjects, for rule matching: a shell call's command lines (the
/// whole argv and each simple command, joined with single spaces), or a file
/// tool's paths.
pub fn subjects(tool: &str, input: &serde_json::Value) -> Vec<String> {
    if tool == "shell" {
        return simple_commands(&shell_argv(input))
            .iter()
            .map(|c| c.join(" "))
            .collect();
    }
    file_paths(tool, input)
}

/// What a limit no mode lifts decided about one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HardLimit {
    Deny(String),
    Ask(String),
}

/// How a deletion limit's reason begins, so [`refused_program`] knows the
/// reason came from the deletion check and not from a rule.
const DELETING_LEAD: &str = "deleting ";
const DELETES_LEAD: &str = "this command deletes ";

/// Programs that delete what they are given.
const DELETERS: &[&str] = &["rm", "rmdir", "unlink", "shred"];

/// Leading words that run the rest as a command.
const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "command", "nice", "nohup", "time", "exec", "busybox", "xargs",
    "timeout",
];

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
    let mut ask: Option<HardLimit> = None;
    // Where a relative operand points: the workspace, then wherever a `cd` in
    // a script moved (unknown after `cd $X` or `cd -`).
    let mut cwd = match workspace {
        Some(ws) => Cwd::Known(ws.to_path_buf()),
        None => Cwd::NoWorkspace,
    };
    for command in simple_commands(argv) {
        let words: Vec<&str> = command.iter().map(String::as_str).collect();
        if words.first().map(|w| base_name(w)) == Some("cd") {
            cwd = cd(&cwd, words.get(1).copied(), home);
            continue;
        }
        let Some(at) = program_index(&words) else {
            continue;
        };
        let program = base_name(words[at]);
        let rest = &words[at + 1..];
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
                if let Target::System(p) = classify(w, home, &cwd, workspace) {
                    return Some(system_refusal(&p));
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
        let mut operands_only = false;
        for w in rest {
            if !operands_only && *w == "--" {
                operands_only = true;
                continue;
            }
            if !operands_only && w.starts_with('-') {
                continue;
            }
            match classify(w, home, &cwd, workspace) {
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
    ask
}

fn system_refusal(path: &str) -> HardLimit {
    HardLimit::Deny(format!(
        "{DELETING_LEAD}{path} is refused in every permission mode: it is a system path (the \
         filesystem root, a top-level directory or the home directory)"
    ))
}

/// Where the deleting program stands in a simple command: after wrappers
/// (`sudo`, `env`, `timeout`), their options and option values, and
/// `VAR=value` words. `None` when the command runs another program first
/// (`echo rm -rf /` deletes nothing).
fn program_index(words: &[&str]) -> Option<usize> {
    for (i, w) in words.iter().enumerate() {
        let b = base_name(w);
        if DELETERS.contains(&b) || b == "find" {
            return Some(i);
        }
        let prefix = WRAPPERS.contains(&b)
            || w.starts_with('-')
            || w.contains('=')
            || w.parse::<f64>().is_ok()
            || (i > 0 && words[i - 1].starts_with('-') && i > 1);
        if !prefix {
            return None;
        }
    }
    None
}

enum Target {
    System(String),
    Critical(String),
    Unresolved(String),
    Plain,
}

/// The directory a script's relative operands resolve against.
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
    if w.contains('$') || w.contains('`') {
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
    let subjects = subjects(&call.tool, &call.input);
    if let Some(rule) = rules.deny.iter().find(|r| r.matches(&call.tool, &subjects)) {
        return Some(HardLimit::Deny(format!(
            "your deny rule `{}` blocks this call in every permission mode",
            rule.text()
        )));
    }
    let deletion = if call.tool == "shell" {
        critical_deletion(&shell_argv(&call.input), home, workspace)
    } else {
        None
    };
    if let Some(HardLimit::Deny(reason)) = &deletion {
        return Some(HardLimit::Deny(reason.clone()));
    }
    if let Some(rule) = rules.ask.iter().find(|r| r.matches(&call.tool, &subjects)) {
        return Some(HardLimit::Ask(format!(
            "your ask rule `{}` asks about this call in every permission mode; allow it?",
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
                if rule.tool != "shell" && rule.tool != "*" {
                    return false;
                }
                let Some(p) = &rule.pattern else {
                    return true;
                };
                // A one-word prefix pattern (`python*`) matches the program as
                // a prefix; otherwise the pattern's first word is a program,
                // and some command line of that program may match the rest.
                match p.strip_suffix('*') {
                    Some(prefix) if !prefix.contains(char::is_whitespace) => {
                        program.starts_with(prefix) || prefix.starts_with(program)
                    }
                    _ => p.split_whitespace().next() == Some(program),
                }
            }
            Self::Path(path) => {
                if rule.tool == "shell" {
                    return false;
                }
                match &rule.pattern {
                    None => rule.tool == "*" || rule.tool == "apply_patch",
                    Some(_) => rule.matches(&rule.tool, &[path.to_owned()]),
                }
            }
            Self::Tool(tool) => rule.tool == tool || rule.tool == "*",
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
        let rules = self.control.rules();
        match hard_limit(
            &rules,
            call,
            self.home.as_deref(),
            self.workspace.as_deref(),
        ) {
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
        // A deny rule on one command of a script names that command.
        let wrapped = shell(&["sh", "-c", "mkdir x && python3 -c pass"]);
        assert_eq!(
            refused_program("shell", &wrapped.input, &reason_of(&wrapped)).as_deref(),
            Some("python3")
        );
        // An ask rule too, also through `-lc` and a pipe.
        let ask = shell(&["bash", "-lc", "echo hi | git push origin main"]);
        assert_eq!(
            refused_program("shell", &ask.input, &reason_of(&ask)).as_deref(),
            Some("git")
        );
        // A plain call is its own program.
        let plain = shell(&["python3", "x.py"]);
        assert_eq!(
            refused_program("shell", &plain.input, &reason_of(&plain)).as_deref(),
            Some("python3")
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
            refused_program("shell", &sh.input, &reason).as_deref(),
            Some("sh")
        );
        // A deletion limit names the deleting command, not its shell.
        let rm = shell(&["sh", "-c", "mkdir x && rm -rf /usr"]);
        let Some(HardLimit::Deny(reason)) =
            hard_limit(&UserRules::default(), &rm, Some(&home()), Some(&ws()))
        else {
            panic!("expected a deny");
        };
        assert_eq!(
            refused_program("shell", &rm.input, &reason).as_deref(),
            Some("rm")
        );
        // Another tool, or a reason that quotes no rule, leaves the call's own program.
        assert_eq!(refused_program("apply_patch", &wrapped.input, "x"), None);
        assert_eq!(refused_program("shell", &wrapped.input, "no rule"), None);
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
        let py = json!({"command": ["python3", "x.py"]});
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
            "[allow]\ntools = [\"fetch\"]\npaths = [\"docs/*\", \"Cargo.toml\"]\n",
        )
        .expect("parse");
        assert!(s.allow.tools.contains("fetch"));
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
