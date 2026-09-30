//! Tool, registry, access and approval errors.

/// The harness could not run a tool call.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The input does not match the tool's schema or rules.
    #[error("invalid input for tool `{tool}`: {reason}")]
    InvalidInput { tool: String, reason: String },

    /// The declared footprint is not usable, or a path was refused. The source
    /// is part of the message: the model reads this text, and a refusal that
    /// does not name its path cannot be corrected (fail loud).
    #[error("tool `{tool}` refused the access: {source}")]
    Access {
        tool: String,
        #[source]
        source: AccessError,
    },

    /// The worker was cancelled while the tool ran.
    #[error("tool `{tool}` was cancelled")]
    Cancelled { tool: String },

    /// The tool timed out.
    #[error("tool `{tool}` timed out after {after_ms} ms")]
    Timeout { tool: String, after_ms: u64 },

    /// The model named a tool that is not in the registry.
    #[error("no tool named `{name}`")]
    UnknownTool { name: String },

    /// The model named a deferred tool it has not discovered through tool search.
    #[error("tool `{name}` is deferred and has not been discovered")]
    NotDiscovered { name: String },

    /// Anything else, with the tool named so the failure is attributable.
    #[error("tool `{tool}` failed: {reason}")]
    Internal { tool: String, reason: String },
}

impl kgraf_diag::DiagCode for ToolError {
    fn diag_code(&self) -> &'static str {
        match self {
            ToolError::Access { .. } => "KG-1401",
            ToolError::Timeout { .. } => "KG-1402",
            _ => kgraf_diag::catch_all_for("kgraf-tools").code,
        }
    }

    fn diag_component(&self) -> &'static str {
        "kgraf-tools"
    }
}

#[cfg(test)]
mod diag_code_tests {
    use super::*;
    use kgraf_diag::DiagCode;

    /// Every `diag_code` branch resolves to a real glossary entry. One instance per named branch, plus one
    /// unnamed variant for the catch-all.
    #[test]
    fn every_diag_code_branch_names_a_real_glossary_entry() {
        let instances: Vec<ToolError> = vec![
            ToolError::Access {
                tool: "x".into(),
                source: AccessError::Escapes { path: "x".into() },
            },
            ToolError::Timeout {
                tool: "x".into(),
                after_ms: 0,
            },
            // Not named in `diag_code`'s match: exercises the catch-all arm.
            ToolError::UnknownTool { name: "x".into() },
        ];
        for e in &instances {
            let code = e.diag_code();
            assert!(
                kgraf_diag::GLOSSARY.iter().any(|spec| spec.code == code),
                "{e:?} names code {code}, which is not in kgraf_diag::GLOSSARY"
            );
        }
    }
}

/// A tool could not be registered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// A tool with this wire name is already registered. Refused rather than
    /// replaced, so a second server cannot shadow a first (fail loud).
    #[error("a tool named `{name}` is already registered")]
    Duplicate { name: String },

    /// A name breaks the naming rules.
    #[error("invalid tool name `{name}`: {reason}")]
    InvalidName { name: String, reason: String },

    /// The tool's definition names something other than the tool itself.
    #[error("tool `{tool}` returned a definition named `{definition}`")]
    DefinitionMismatch { tool: String, definition: String },

    /// A built-in tool's input schema has a node with no `type`, which strict
    /// vendor validators refuse for every request offering the tool
    /// (`untyped_schema_nodes`).
    #[error("built-in tool `{tool}` has schema nodes with no `type`: {paths:?}")]
    UntypedSchema { tool: String, paths: Vec<String> },
}

/// A declared path cannot be compared safely.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AccessError {
    /// Paths are relative to the workspace root; an absolute path is refused.
    #[error("path `{path}` is absolute; access paths are relative to the workspace root")]
    Absolute { path: String },
    /// The path climbs above the workspace root.
    #[error("path `{path}` escapes the workspace root")]
    Escapes { path: String },
    /// A regular file with more than one hard link. Another of its names can be
    /// outside the workspace, where a write through this one would land, and
    /// no path check inside the workspace can see that other name.
    #[error(
        "path `{path}` is a regular file with {links} hard links; another link can \
         name a file outside the workspace, so it is refused"
    )]
    HardLinked { path: String, links: u64 },
}

/// Approval could not complete.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    /// A decision could not be recorded, so no approval was issued.
    #[error("cannot record approval decision: {0}")]
    Log(String),

    /// A paused call no longer matches its seal: its input, its position in the
    /// chain or another field was changed after the ask.
    #[error("paused call `{call_id}` was altered after the ask; refused")]
    PausedCallAltered { call_id: String },

    /// The chain answering a paused call is not the chain that paused it.
    #[error("paused call `{call_id}` was paused by a different approval chain; refused")]
    ChainChanged { call_id: String },
}
