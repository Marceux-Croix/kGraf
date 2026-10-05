//! Which tools a permission rule or an allow-set entry may name (decision
//! 0094, amended 2026-10-02: a rule that names no tool is refused; amended
//! 2026-10-05: a Claude Code tool kGraf lacks is a notice).
//!
//! A rule that names a tool no worker has matches nothing, so a deny rule
//! written that way protects nothing while the user believes it does. Every
//! name is therefore checked where it is read:
//!
//! - a kGraf built-in tool ([`BUILTIN_TOOLS`]) is taken as written;
//! - a Claude Code tool name ([`CLAUDE_CODE_ALIASES`]) is read as the kGraf
//!   tool that does the same job, so `Bash(rm *)` is `shell(rm *)`;
//! - a Claude Code tool kGraf has no tool for ([`CLAUDE_CODE_WITHOUT_COUNTERPART`],
//!   `WebFetch` and the like) is accepted, matches no call, and is said as a
//!   notice that names what kGraf uses instead and the kGraf rule that gives
//!   the intended effect ([`counterpart_notice`]);
//! - an MCP name (`mcp__<server>__<tool>`, or `mcp__<server>` for every tool
//!   of that server) is accepted here and checked at run start, when the
//!   servers have listed their tools ([`unmatched_entries`]);
//! - `*`, and in a deny or ask rule a tool-name glob that can match a
//!   built-in or an MCP name, are accepted;
//! - anything else is refused, naming the tools a rule may name.
//!
//! Wildcards follow Claude Code's permission-rule documentation: a `*` in a
//! pattern stands for any text, a trailing ` *` (or `:*`) that is the only
//! wildcard also matches the bare command, and `mcp__<server>` names every
//! tool of that server. Each of these only widens what a rule matches, so no
//! rule that matched before matches less.

use super::{EntryList, PermissionSettings};

/// Every built-in tool a kGraf worker may have, under some configuration.
/// A run may lack some (no model routes, no skills, no session line); a rule
/// on one it lacks is said at run start ([`unmatched_entries`]).
/// `kgraf-host` checks this list against the names each crate defines.
pub const BUILTIN_TOOLS: &[&str] = &[
    "apply_patch",
    "ask_user_question",
    "challenge_contract",
    "decide",
    "embed",
    "graph_slice",
    "list_directory",
    "read_file",
    "recall_history",
    "report_shortfall",
    "request_connection",
    "rerank",
    "search_files",
    "send_message",
    "shell",
    "skill",
    "spawn_subagent",
];

/// Built-in tools whose calls a rule's pattern matches by path: the paths
/// `apply_patch` writes, or a read tool's `path`.
pub const PATH_TOOLS: &[&str] = &["apply_patch", "list_directory", "read_file", "search_files"];

/// Claude Code tool names a rule or an allow-set entry may use, with the
/// kGraf tool each is read as. The matching is the same:
/// - `Bash` matches command text as `shell` does (kGraf also matches each
///   command of an `sh -c` script and past wrappers, which Claude Code does
///   not, so a kGraf rule refuses at least what Claude Code's would);
/// - `Edit`, `Write`, `MultiEdit` and `NotebookEdit` match the paths
///   `apply_patch` writes. Claude Code consults path rules on `Edit` only;
///   kGraf applies them for all four, the stricter reading;
/// - `Read` matches `read_file`'s path, `Grep` and `Glob` `search_files`'s,
///   `LS` `list_directory`'s. Claude Code applies a `Read` rule to its
///   search tools too; kGraf does not, so a path the searches must not see
///   needs its own `search_files(...)` rule.
pub const CLAUDE_CODE_ALIASES: &[(&str, &str)] = &[
    ("Bash", "shell"),
    ("Edit", "apply_patch"),
    ("Write", "apply_patch"),
    ("MultiEdit", "apply_patch"),
    ("NotebookEdit", "apply_patch"),
    ("Read", "read_file"),
    ("Grep", "search_files"),
    ("Glob", "search_files"),
    ("LS", "list_directory"),
];

/// Claude Code tools kGraf has no tool for, each with what kGraf uses instead
/// and the kGraf rule that gives the intended effect. A rule or allow-set
/// entry that names one loads, matches no call (so it changes no decision),
/// and is said as a notice ([`counterpart_notice`]): such a name is a Claude
/// Code habit or a file shared with Claude Code, not a typo, so refusing it
/// would stop a run for a tool no worker can call. A name in neither this
/// table nor [`CLAUDE_CODE_ALIASES`] is still refused.
///
/// From Claude Code's tools reference (code.claude.com/docs/en/tools-reference,
/// read for Claude Code 2.1.289), plus `Task` (its sub-agents page: the older
/// name of `Agent`, still read in settings), `SlashCommand` and `TaskOutput`
/// (its changelog: shipped, later merged into skills or removed), since old
/// settings files still hold them. The names the tools reference lists that
/// kGraf reads as its own tools are in [`CLAUDE_CODE_ALIASES`].
pub const CLAUDE_CODE_WITHOUT_COUNTERPART: &[(&str, &str)] = &[
    (
        "Agent",
        "Workers start subagents with the spawn_subagent tool. To block that, add \
         `spawn_subagent`.",
    ),
    (
        "Artifact",
        "kGraf does not publish files outside your computer. You do not need a rule for it.",
    ),
    (
        "AskUserQuestion",
        "Workers ask you questions with the ask_user_question tool. To block that, add \
         `ask_user_question`.",
    ),
    ("CronCreate", NO_SCHEDULE),
    ("CronDelete", NO_SCHEDULE),
    ("CronList", NO_SCHEDULE),
    (
        "EndConversation",
        "Workers cannot end your session. You do not need a rule for it.",
    ),
    ("EnterPlanMode", NO_PLAN_MODE),
    ("EnterWorktree", OWN_WORKTREE),
    ("ExitPlanMode", NO_PLAN_MODE),
    ("ExitWorktree", OWN_WORKTREE),
    (
        "ListAgents",
        "Workers send messages to other sessions with the send_message tool. To block that, \
         add `send_message`.",
    ),
    ("ListMcpResourcesTool", MCP_ONLY_TOOLS),
    (
        "LSP",
        "Workers get code structure from the graph_slice tool and read files with the \
         read_file tool. To block reads of a path, add `read_file(<path>)` and \
         `search_files(<path>)`.",
    ),
    (
        "Monitor",
        "Workers run all commands, background commands too, through the shell tool, and your \
         shell rules apply to them. To block a command, add `shell(<command> *)`.",
    ),
    (
        "PowerShell",
        "Workers run all commands through the shell tool. To block a command, add \
         `shell(<command> *)`.",
    ),
    (
        "PushNotification",
        "Workers do not send notifications. kGraf shows run events in its window. You do not \
         need a rule for it.",
    ),
    ("ReadMcpResourceTool", MCP_ONLY_TOOLS),
    (
        "RemoteTrigger",
        "Workers cannot make or start routines or runs. You do not need a rule for it.",
    ),
    (
        "ReportFindings",
        "Workers give their results to the orchestrator when they hand back. You do not need a \
         rule for it.",
    ),
    ("ScheduleWakeup", NO_SCHEDULE),
    (
        "SendFeedback",
        "kGraf sends no feedback reports and no telemetry. You do not need a rule for it.",
    ),
    (
        "SendMessage",
        "Workers send messages with the send_message tool. To block that, add `send_message`.",
    ),
    (
        "SendUserFile",
        "Workers do not send files to you. Their changes stay in their worktree. You do not \
         need a rule for it.",
    ),
    (
        "ShareOnboardingGuide",
        "kGraf does not upload files. You do not need a rule for it.",
    ),
    (
        "Skill",
        "Workers run skills with the skill tool. To block every skill, add `skill`.",
    ),
    (
        "SlashCommand",
        "Workers run skills with the skill tool. To block every skill, add `skill`.",
    ),
    (
        "SubagentHandback",
        "Workers have no tool that does this. You do not need a rule for it.",
    ),
    (
        "Task",
        "Task is the older Claude Code name for Agent. Workers start subagents with the \
         spawn_subagent tool. To block that, add `spawn_subagent`.",
    ),
    ("TaskCreate", NO_TASK_LIST),
    ("TaskGet", NO_TASK_LIST),
    ("TaskList", NO_TASK_LIST),
    (
        "TaskOutput",
        "Workers get the output of a command in the result of the shell tool. You do not need \
         a rule for it.",
    ),
    (
        "TaskStop",
        "Workers cannot stop other tasks. You stop runs in kGraf. You do not need a rule for it.",
    ),
    ("TaskUpdate", NO_TASK_LIST),
    ("TodoWrite", NO_TASK_LIST),
    ("ToolSearch", MCP_CONNECTED),
    ("WaitForMcpServers", MCP_CONNECTED),
    (
        "WebFetch",
        "Workers reach the web through the shell (curl, wget) or MCP tools. To block that, add \
         `shell(curl*)`, `shell(wget*)` or a rule for the MCP tool.",
    ),
    (
        "WebSearch",
        "Workers search the web only through the shell (curl, wget) or MCP tools. To block \
         that, add `shell(curl*)`, `shell(wget*)` or a rule for the MCP tool.",
    ),
    (
        "Workflow",
        "The orchestrator runs the work, and workers start subagents with the spawn_subagent \
         tool. To block that, add `spawn_subagent`.",
    ),
];

const NO_SCHEDULE: &str = "Workers cannot schedule prompts. You set scheduled runs in kGraf. \
You do not need a rule for it.";
const NO_PLAN_MODE: &str = "Workers have no plan mode. The orchestrator makes the plan. To see \
each plan before a worker starts, set `plan_review = true`.";
const OWN_WORKTREE: &str = "Each worker works in its own worktree and cannot move out of it. \
You do not need a rule for it.";
const MCP_ONLY_TOOLS: &str = "Workers use an MCP server only through the tools that the server \
lists. To block a server, add `mcp__<server>`.";
const NO_TASK_LIST: &str = "Workers have no task list. The orchestrator tracks the work on the \
graph. You do not need a rule for it.";
const MCP_CONNECTED: &str = "kGraf connects the MCP servers before a worker starts and gives \
the worker their tools. You do not need a rule for it.";

/// What kGraf uses instead of the Claude Code tool `name`, if `name` is one
/// kGraf has no tool for ([`CLAUDE_CODE_WITHOUT_COUNTERPART`]). Exact case,
/// as Claude Code matches tool names.
pub fn without_counterpart(name: &str) -> Option<&'static str> {
    CLAUDE_CODE_WITHOUT_COUNTERPART
        .iter()
        .find(|(cc, _)| *cc == name)
        .map(|(_, instead)| *instead)
}

/// The notice for an entry of `list` written as `written` and naming the
/// tool `name`, when `name` is a Claude Code tool kGraf has no tool for;
/// `None` for any other name. The command line, the run trace and Settings
/// show this same text.
pub fn counterpart_notice(list: EntryList, written: &str, name: &str) -> Option<String> {
    let instead = without_counterpart(name)?;
    let (what, effect) = match list {
        EntryList::Deny => ("deny rule", "it blocks nothing"),
        EntryList::Ask => ("ask rule", "it asks about nothing"),
        EntryList::AllowTools | EntryList::AllowPrograms | EntryList::AllowPaths => {
            ("allow-set tool", "Auto grants nothing by it")
        }
    };
    Some(format!(
        "your {what} `{written}` matches no call, so {effect}. kGraf has no {name} tool. \
         {instead}"
    ))
}

/// Claude Code's `Bash` input fields. `Bash(timeout:30)` is a parameter rule
/// there; kGraf's shell tool has no such parameters, so it is refused rather
/// than read as a command named `timeout:30`.
const BASH_PARAMETERS: &[&str] = &[
    "command",
    "dangerouslyDisableSandbox",
    "description",
    "run_in_background",
    "timeout",
];

const MCP_PREFIX: &str = "mcp__";

/// The kGraf tool a Claude Code name is read as, if it is one.
pub fn claude_code_alias(name: &str) -> Option<&'static str> {
    CLAUDE_CODE_ALIASES
        .iter()
        .find(|(cc, _)| *cc == name)
        .map(|(_, kgraf)| *kgraf)
}

/// The kGraf tool that does the job of a name other coding agents use
/// (Claude Code's names, `write_file` and the like), or of a built-in spelled
/// in another case. For a hint in a refusal; never a match.
pub fn tool_hint(name: &str) -> Option<&'static str> {
    if let Some(kgraf) = claude_code_alias(name) {
        return Some(kgraf);
    }
    match name {
        "write_file" | "edit_file" | "create_file" => return Some("apply_patch"),
        _ => {}
    }
    CLAUDE_CODE_ALIASES
        .iter()
        .find(|(cc, _)| cc.eq_ignore_ascii_case(name))
        .map(|(_, kgraf)| *kgraf)
        .or_else(|| {
            BUILTIN_TOOLS
                .iter()
                .find(|b| b.eq_ignore_ascii_case(name))
                .copied()
        })
}

/// `*` matches any text, `/` and spaces included; every other byte matches
/// itself. Anchored at both ends. Byte-wise is exact for UTF-8 text, since
/// `*` is one byte and never part of another character.
pub(super) fn glob(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    // The last `*` seen and the text position it was tried from: on a
    // mismatch the star takes one more byte. Linear in practice for rule
    // patterns, which hold one or two stars.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// A pattern matches a subject: [`glob`], or, when the pattern's only
/// wildcard is a trailing ` *`, the bare command before it (Claude Code:
/// `Bash(ls *)` matches `ls`).
pub(super) fn pattern_matches(pattern: &str, subject: &str) -> bool {
    glob(pattern, subject)
        || pattern
            .strip_suffix(" *")
            .is_some_and(|stem| !stem.contains('*') && stem == subject)
}

/// Whether a rule's (canonical) tool name covers the wire name `tool`.
pub(super) fn tool_name_matches(rule_tool: &str, tool: &str) -> bool {
    // A Claude Code tool kGraf has no tool for covers no call, even one an
    // MCP server or a test registers under that bare name: its notice says it
    // does nothing, so it must change no decision.
    if without_counterpart(rule_tool).is_some() {
        return false;
    }
    if rule_tool == tool || rule_tool == "*" {
        return true;
    }
    if rule_tool.contains('*') {
        return glob(rule_tool, tool);
    }
    // `mcp__<server>` names every tool of that server. A server name may not
    // contain `__` (`QualifiedName::mcp`), so the first `__` ends it.
    match rule_tool.strip_prefix(MCP_PREFIX) {
        Some(server) if !server.is_empty() && !server.contains("__") => tool
            .strip_prefix(rule_tool)
            .is_some_and(|rest| rest.starts_with("__") && rest.len() > 2),
        _ => false,
    }
}

/// Where a tool name was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    /// A deny or ask rule.
    Rule,
    /// An allow-set tool (what Auto grants).
    Allow,
}

/// The tool a written name stands for, or why the name is refused.
/// A Claude Code tool kGraf has no tool for stands for itself, and covers no
/// call ([`tool_name_matches`]).
pub(super) fn canonical_tool(name: &str, written: &str, role: Role) -> Result<String, String> {
    if let Some(kgraf) = claude_code_alias(name) {
        return Ok(kgraf.to_owned());
    }
    if BUILTIN_TOOLS.contains(&name) || without_counterpart(name).is_some() {
        return Ok(name.to_owned());
    }
    if let Some(rest) = name.strip_prefix(MCP_PREFIX) {
        let server = rest.split("__").next().unwrap_or_default();
        if server.is_empty() {
            return Err(format!(
                "`{written}` names no MCP server: write mcp__<server>__<tool>, or mcp__<server> \
                 for every tool of that server"
            ));
        }
        if role == Role::Allow && server.contains('*') {
            return Err(format!(
                "`{written}` is an allow-set tool whose server is a wildcard: an allow entry \
                 must name one server you configured (mcp__<server>__*), so it cannot grant \
                 every server's tools"
            ));
        }
        return Ok(name.to_owned());
    }
    match role {
        Role::Rule => {
            let mcp_like = {
                let literal = name.split('*').next().unwrap_or_default();
                MCP_PREFIX.starts_with(literal) || literal.starts_with(MCP_PREFIX)
            };
            if name == "*"
                || (name.contains('*') && (mcp_like || BUILTIN_TOOLS.iter().any(|b| glob(name, b))))
            {
                return Ok(name.to_owned());
            }
        }
        Role::Allow if name.contains('*') => {
            return Err(format!(
                "`{written}` is an allow-set tool with a wildcard: Auto would grant every tool it \
                 matches, so an allow entry names one tool, or one MCP server's tools \
                 (mcp__<server> or mcp__<server>__*)"
            ));
        }
        Role::Allow => {}
    }
    Err(unknown(name, written, role))
}

fn unknown(name: &str, written: &str, role: Role) -> String {
    let hint = tool_hint(name)
        .map(|t| format!(" (use `{t}`)"))
        .unwrap_or_default();
    let what = match role {
        Role::Rule => "a rule",
        Role::Allow => "an allow-set entry",
    };
    let aliases: Vec<String> = CLAUDE_CODE_ALIASES
        .iter()
        .map(|(cc, kgraf)| format!("{cc} as {kgraf}"))
        .collect();
    format!(
        "`{written}` names `{name}`, which is no kGraf tool{hint}, so it would match nothing. \
         {what} may name a kGraf tool ({}), a Claude Code tool kGraf reads as one ({}), an MCP \
         server's tool (mcp__<server>__<tool>, or mcp__<server> for all of them){}",
        BUILTIN_TOOLS.join(", "),
        aliases.join(", "),
        match role {
            Role::Rule => ", or * for every tool",
            Role::Allow => "",
        }
    )
}

/// A rule's pattern as kGraf matches it and whether it matches at any
/// depth; or why the rule can never match. `name` is the tool as written, `tool` the tool it stands for.
pub(super) fn canonical_pattern(
    name: &str,
    tool: &str,
    pattern: &str,
    written: &str,
) -> Result<(String, bool), String> {
    let alias = claude_code_alias(name).is_some();
    if tool == "shell" {
        if alias
            && let Some((param, value)) = pattern.split_once(':')
            && BASH_PARAMETERS.contains(&param.trim())
            && value.trim() != "*"
        {
            return Err(format!(
                "`{written}` is a Claude Code parameter rule (`{}:`); kGraf's shell tool has no \
                 such parameter, so it would match nothing. Write the command, as in \
                 Bash(rm *)",
                param.trim()
            ));
        }
        // Claude Code: `:*` at the end is a trailing ` *`.
        return Ok(match pattern.strip_suffix(":*") {
            Some(stem) if !stem.is_empty() => (format!("{stem} *"), false),
            _ => (pattern.to_owned(), false),
        });
    }
    if BUILTIN_TOOLS.contains(&tool) && !PATH_TOOLS.contains(&tool) {
        return Err(format!(
            "`{written}` has a pattern, but a `{tool}` call names no command or path, so the \
             rule would match nothing; write `{name}` to cover every call"
        ));
    }
    if !(alias && PATH_TOOLS.contains(&tool)) {
        return Ok((pattern.to_owned(), false));
    }
    // A Claude Code path rule, read as a path in the repository: kGraf's file
    // tools act only inside the worktree, with paths relative to its root.
    if pattern.starts_with("//") || pattern.starts_with('~') {
        return Err(format!(
            "`{written}` names a path outside the repository; kGraf's file tools act only on \
             paths in the repository, so it would match nothing. Write the path relative to the \
             repository root"
        ));
    }
    if pattern.starts_with('!') {
        return Err(format!(
            "`{written}` is a carve-out (`!`); kGraf rules have no carve-outs, so it would \
             match nothing. Remove it, or narrow the rule it carves out of"
        ));
    }
    // `/path` (anchored at the settings source in Claude Code) and `./path`
    // are anchored at the repository root. Any other relative pattern matches
    // at any depth, as Claude Code's deny and ask rules do for a bare name or
    // `dir/**` (kGraf does so for every relative pattern, the stricter reading).
    let (stem, any_depth) = match pattern
        .strip_prefix("./")
        .or_else(|| pattern.strip_prefix('/'))
    {
        Some(rest) => (rest, false),
        None => (pattern, true),
    };
    if stem.is_empty() {
        return Err(format!("`{written}` names no path"));
    }
    Ok((stem.to_owned(), any_depth))
}

/// Run-start sentences for every deny or ask rule and allow-set tool that
/// names no tool in `tools`, the tools this run's workers have (MCP tools
/// included). Such an entry was accepted when it was read, because an MCP
/// server's tools are known only once it connects; here it matches nothing,
/// and the run says so. An entry on a Claude Code tool kGraf has no tool for
/// gets its own notice ([`counterpart_notice`]), which says what to write
/// instead. Empty when every entry names some tool.
pub fn unmatched_entries(settings: &PermissionSettings, tools: &[String]) -> Vec<String> {
    let mut sorted: Vec<&str> = tools.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let list = sorted.join(", ");
    let none = |tool: &str| !tools.iter().any(|t| tool_name_matches(tool, t));
    let mut out = Vec::new();
    for (kind, entry_list, effect, rules) in [
        ("deny", EntryList::Deny, "blocks", &settings.rules.deny),
        ("ask", EntryList::Ask, "asks about", &settings.rules.ask),
    ] {
        for rule in rules.iter().filter(|r| none(&r.tool)) {
            if let Some(notice) = counterpart_notice(entry_list, &rule.text(), &rule.tool) {
                out.push(notice);
                continue;
            }
            out.push(format!(
                "your {kind} rule `{}` names no tool any worker of this run has, so it {effect} \
                 nothing in this run. The workers' tools are: {list}",
                rule.text()
            ));
        }
    }
    for tool in settings.allow.tools.iter().filter(|t| none(t)) {
        if let Some(notice) = counterpart_notice(EntryList::AllowTools, tool, tool) {
            out.push(notice);
            continue;
        }
        out.push(format!(
            "your allow-set tool `{tool}` names no tool any worker of this run has, so Auto \
             grants nothing by it in this run. The workers' tools are: {list}"
        ));
    }
    out
}
