//! Model-facing tool names.
//!
//! **Scheme.** A built-in tool is sent under its own name. An MCP tool is sent as
//! `mcp__<server>__<tool>`. That is the form Claude Code uses, so a `SKILL.md`
//! ingested from it (0053) whose `allowed-tools` names `mcp__github__create_issue`
//! matches the same tool here without translation.
//!
//! **Charset.** The two largest tool-calling protocols accept tool names matching
//! `^[a-zA-Z0-9_-]{1,64}$`. A name outside that is refused, not repaired: a
//! silently rewritten name would make an `allowed-tools` entry or a transcript
//! refer to a tool that does not exist (fail loud).

use crate::error::RegistryError;
use std::fmt;

/// The prefix that marks an MCP tool's wire name.
pub const MCP_PREFIX: &str = "mcp__";
/// The longest wire name the tool-calling protocols accept.
pub const MAX_WIRE_NAME_CHARS: usize = 64;
const SEPARATOR: &str = "__";

/// A tool's identity: built-in, or an MCP server's tool.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QualifiedName {
    server: Option<String>,
    tool: String,
}

impl QualifiedName {
    /// A built-in tool's name.
    ///
    /// It may not start with [`MCP_PREFIX`], so a built-in can never pose as a
    /// server's tool.
    pub fn builtin(tool: &str) -> Result<Self, RegistryError> {
        check_charset(tool, tool)?;
        if tool.starts_with(MCP_PREFIX) {
            return Err(invalid(
                tool,
                "a built-in tool may not use the `mcp__` prefix",
            ));
        }
        let name = Self {
            server: None,
            tool: tool.to_owned(),
        };
        name.check_length()?;
        Ok(name)
    }

    /// An MCP server's tool.
    ///
    /// Server names follow Claude Code's: ASCII letters, digits, `_` and `-`, as
    /// ingested `allowed-tools` entries spell them (`mcp__claude_ai_Gmail__search`).
    /// A server name may not contain `__` or start or end with `_`, so the first
    /// `__` after the prefix always ends the server part and
    /// [`QualifiedName::parse`] is unambiguous. Two names that still render to the
    /// same wire string are refused by the registry as duplicates.
    pub fn mcp(server: &str, tool: &str) -> Result<Self, RegistryError> {
        Self::check_server(server)?;
        check_charset(tool, tool)?;
        let name = Self {
            server: Some(server.to_owned()),
            tool: tool.to_owned(),
        };
        name.check_length()?;
        Ok(name)
    }

    /// Check a server name against the scheme's rules.
    pub fn check_server(server: &str) -> Result<(), RegistryError> {
        check_charset(server, server)?;
        if server.contains(SEPARATOR) || server.starts_with('_') || server.ends_with('_') {
            return Err(invalid(
                server,
                "a server name may not contain `__` or start or end with `_`",
            ));
        }
        Ok(())
    }

    /// Read a wire name back into its parts.
    pub fn parse(wire: &str) -> Result<Self, RegistryError> {
        match wire.strip_prefix(MCP_PREFIX) {
            None => Self::builtin(wire),
            Some(rest) => {
                let (server, tool) = rest
                    .split_once(SEPARATOR)
                    .ok_or_else(|| invalid(wire, "an MCP name needs `mcp__<server>__<tool>`"))?;
                Self::mcp(server, tool)
            }
        }
    }

    /// The server, for an MCP tool.
    pub fn server(&self) -> Option<&str> {
        self.server.as_deref()
    }

    /// The tool's own name.
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// The name sent to the model.
    pub fn wire(&self) -> String {
        match &self.server {
            None => self.tool.clone(),
            Some(server) => format!("{MCP_PREFIX}{server}{SEPARATOR}{}", self.tool),
        }
    }

    fn check_length(&self) -> Result<(), RegistryError> {
        let wire = self.wire();
        if wire.chars().count() > MAX_WIRE_NAME_CHARS {
            return Err(invalid(
                &wire,
                "longer than the 64 characters tool-calling protocols accept",
            ));
        }
        Ok(())
    }
}

impl fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.wire())
    }
}

fn check_charset(name: &str, shown: &str) -> Result<(), RegistryError> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(invalid(
            shown,
            "a tool name must be nonempty ASCII letters, digits, `_` and `-`",
        ))
    }
}

fn invalid(name: &str, reason: &str) -> RegistryError {
    RegistryError::InvalidName {
        name: name.to_owned(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_mcp_name_round_trips_through_its_wire_form() {
        let name = QualifiedName::mcp("github", "create_issue").expect("valid");
        assert_eq!(name.wire(), "mcp__github__create_issue");
        assert_eq!(QualifiedName::parse(&name.wire()).expect("parse"), name);
    }

    #[test]
    fn a_tool_name_containing_the_separator_still_parses_unambiguously() {
        // The server may not contain `_`, so the first `__` ends it.
        let name = QualifiedName::mcp("fs", "read__raw").expect("valid");
        let back = QualifiedName::parse(&name.wire()).expect("parse");
        assert_eq!(back.server(), Some("fs"));
        assert_eq!(back.tool(), "read__raw");
    }

    #[test]
    fn a_builtin_cannot_pose_as_a_server_tool() {
        assert!(QualifiedName::builtin("mcp__github__create_issue").is_err());
        assert_eq!(
            QualifiedName::builtin("edit").expect("valid").server(),
            None
        );
    }

    #[test]
    fn names_the_protocols_reject_are_refused_not_repaired() {
        assert!(QualifiedName::mcp("github", "create.issue").is_err());
        assert!(QualifiedName::mcp("git__hub", "x").is_err());
        assert!(QualifiedName::mcp("_github", "x").is_err());
        assert!(QualifiedName::mcp("github_", "x").is_err());
        assert!(QualifiedName::mcp("git hub", "x").is_err());
        assert!(QualifiedName::mcp("", "x").is_err());
        assert!(QualifiedName::builtin("").is_err());
        assert!(QualifiedName::builtin("has space").is_err());
        let long = "t".repeat(60);
        assert!(QualifiedName::mcp("server", &long).is_err());
        assert!(QualifiedName::builtin(&"t".repeat(64)).is_ok());
        assert!(QualifiedName::builtin(&"t".repeat(65)).is_err());
    }

    #[test]
    fn claude_code_server_names_with_uppercase_and_underscores_are_accepted() {
        // Ingested allowed-tools entries spell servers this way (0053).
        let name = QualifiedName::mcp("claude_ai_Gmail", "search_threads").expect("valid");
        assert_eq!(name.wire(), "mcp__claude_ai_Gmail__search_threads");
        let back = QualifiedName::parse(&name.wire()).expect("parse");
        assert_eq!(back.server(), Some("claude_ai_Gmail"));
        assert_eq!(back.tool(), "search_threads");
    }

    #[test]
    fn a_malformed_mcp_wire_name_is_refused() {
        assert!(QualifiedName::parse("mcp__nosep").is_err());
    }
}
