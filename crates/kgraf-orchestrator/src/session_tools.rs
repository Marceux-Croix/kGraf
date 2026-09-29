//! [`SessionLine`] binds a run's session tools to the orchestrator handle once
//! it is built, the way `ContractLine` binds `challenge_contract`
//! (`challenge_tool.rs`); [`session_tools`] is the four tools decision 0067
//! adds: `ask_user_question`, `spawn_subagent`, `send_message` and
//! `request_connection`.
//!
//! **Registered only behind the host's `sessions` flag**, which defaults to
//! `false` (`kgraf-host/src/config.rs`), so `main` never shows a worker a tool
//! that answers "not built yet" (each tool answers so until a real hook is
//! bound).

use crate::ask_tool::{ASK_USER_QUESTION_TOOL, AskUserQuestionTool};
use crate::message_tool::{
    REQUEST_CONNECTION_TOOL, RequestConnectionTool, SEND_MESSAGE_TOOL, SendMessageTool,
};
use crate::orchestrator::OrchestratorHandle;
use crate::subagent_tool::{SPAWN_SUBAGENT_TOOL, SpawnSubagentTool};
use std::sync::{Arc, OnceLock};

/// Every session tool's final, wire name (decision 0067), in the order
/// [`session_tools`] returns them.
pub const SESSION_TOOL_NAMES: [&str; 4] = [
    ASK_USER_QUESTION_TOOL,
    SPAWN_SUBAGENT_TOOL,
    SEND_MESSAGE_TOOL,
    REQUEST_CONNECTION_TOOL,
];

/// The orchestrator a run's session tools reach: bound once, after the
/// orchestrator is built. Cloning shares the binding.
#[derive(Clone, Default)]
pub struct SessionLine(Arc<OnceLock<OrchestratorHandle>>);

impl SessionLine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind the line. A second binding is refused: one run's tools answer to
    /// one orchestrator.
    pub fn bind(&self, handle: OrchestratorHandle) -> Result<(), String> {
        self.0
            .set(handle)
            .map_err(|_| "the session line is already bound to an orchestrator".to_owned())
    }

    pub fn is_bound(&self) -> bool {
        self.0.get().is_some()
    }

    /// The bound handle, once there is one.
    pub(crate) fn handle(&self) -> Option<&OrchestratorHandle> {
        self.0.get()
    }
}

/// The session tools of decision 0067: `ask_user_question`, `spawn_subagent`,
/// `send_message` and `request_connection`. Every one answers "not built yet"
/// until a hook is bound to `line`.
pub fn session_tools(line: &SessionLine) -> Vec<Arc<dyn kgraf_tools::Tool>> {
    vec![
        Arc::new(AskUserQuestionTool::new(line.clone())),
        Arc::new(SpawnSubagentTool::new(line.clone())),
        Arc::new(SendMessageTool::new(line.clone())),
        Arc::new(RequestConnectionTool::new(line.clone())),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_provider::message::Content;
    use kgraf_runtime::{NeutralPrefixEncoder, SharedPrefix};

    #[test]
    fn a_fresh_line_is_unbound() {
        // `OrchestratorHandle` has no public constructor outside a built
        // `Orchestrator`; binding (and refusing a second binding) against a
        // real one is exercised in `tests/session_line.rs`, which can build
        // one through the crate's own public API.
        let line = SessionLine::new();
        assert!(!line.is_bound());
        assert!(line.handle().is_none());
    }

    #[test]
    fn session_tools_names_all_four_tools_in_the_pinned_order() {
        let line = SessionLine::new();
        let tools = session_tools(&line);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, SESSION_TOOL_NAMES.to_vec());
    }

    // A stub tool's own `call` cannot be exercised here: `CallPermit::new` is
    // `pub(crate)` to `kgraf-tools`, so no other crate can prove a call was
    // approved, even in a test. `kgraf-host`'s
    // `a_session_tool_call_with_the_flag_on_answers_as_an_error` runs a real
    // worker loop instead (the only path that legitimately mints one) and
    // asserts `is_error: true` on the resulting `ToolCallFinished`.

    fn digest_of(tools: Vec<kgraf_provider::message::ToolDefinition>) -> String {
        let prefix = SharedPrefix {
            vendor: "fake".into(),
            model: "m".into(),
            system: vec![Content::text("You are a kGraf worker.")],
            tools,
            skill_catalog: vec![],
            slice_pool: vec![],
            mcp_instructions: vec![],
        };
        prefix
            .digest(&NeutralPrefixEncoder)
            .expect("encode")
            .as_str()
            .to_owned()
    }

    /// The design's own acceptance test: "with the `sessions` flag off, the
    /// assembled prefix digest equals today's". No tool list can be added
    /// without changing every worker's cache key, so this pins the digest of
    /// an otherwise-empty tool list to a literal: a change here is a change in
    /// what every existing run's cache keys on, and must be a deliberate,
    /// reviewed decision, not a side effect.
    #[test]
    fn the_prefix_digest_with_no_session_tools_is_pinned() {
        assert_eq!(
            digest_of(vec![]),
            "cf0ad3a55a2779246a0db42761bde275ce1d7590d5d77d1f3691ea36b31e1e13",
            "a change here means every worker's cache resets even with the \
             sessions flag off, which nothing in this unit may cause"
        );
    }

    /// "With it on, two workers on one route still share one digest" (design),
    /// and all four tools are present only then.
    #[test]
    fn two_lines_on_one_route_produce_one_digest_with_all_four_tools_present() {
        let tools_a: Vec<_> = session_tools(&SessionLine::new())
            .iter()
            .map(|t| t.definition())
            .collect();
        let tools_b: Vec<_> = session_tools(&SessionLine::new())
            .iter()
            .map(|t| t.definition())
            .collect();
        let names: std::collections::BTreeSet<&str> =
            tools_a.iter().map(|d| d.name.as_str()).collect();
        for tool in SESSION_TOOL_NAMES {
            assert!(names.contains(tool), "{tool} is missing with the flag on");
        }
        let digest_a = digest_of(tools_a);
        let digest_b = digest_of(tools_b);
        assert_eq!(
            digest_a, digest_b,
            "two independently built SessionLines must still give one shared \
             digest per vendor and model, or two workers on one route would \
             miss each other's prompt cache"
        );
        assert_ne!(
            digest_a,
            digest_of(vec![]),
            "turning the flag on must change the digest"
        );
    }
}
