//! `send_message` and `request_connection` (decision 0067, point 6): messages
//! flow freely over an open connection; a message to an agent with no
//! connection is held and a request is opened for it.
//!
//! Late-bound through [`SessionLine`], the way `challenge_contract`
//! (`challenge_tool.rs`) is; registered behind the host's `sessions` flag.
//!
//! Until the host binds a hook, both tools answer "not built yet". The bound
//! hooks live in `orchestrator/connections.rs`.

use crate::session_tools::SessionLine;
use kgraf_core::AgentId;
use kgraf_tools::{Access, CallPermit, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::Deserialize;

/// The tool's name, as workers call it.
pub const SEND_MESSAGE_TOOL: &str = "send_message";
/// The tool's name, as workers call it.
pub const REQUEST_CONNECTION_TOOL: &str = "request_connection";

/// `send_message`: send text to another session over an open connection.
pub struct SendMessageTool {
    line: SessionLine,
}

impl SendMessageTool {
    pub fn new(line: SessionLine) -> Self {
        Self { line }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendInput {
    /// The recipient session id.
    to: String,
    text: String,
}

#[async_trait::async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        SEND_MESSAGE_TOOL
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: SEND_MESSAGE_TOOL.into(),
            description: "Send text to another session (another worker, a subagent, or the \
                orchestrator) by its session id. Delivered at once over an open connection; \
                otherwise a connection request is opened first and you are told when it is \
                refused."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "The recipient's session id."},
                    "text": {"type": "string", "description": "The message text."}
                },
                "required": ["to", "text"],
                "additionalProperties": false
            }),
        }
    }

    fn access(&self, _input: &serde_json::Value) -> Result<Access, ToolError> {
        Ok(Access::none())
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let input: SendInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput {
                tool: SEND_MESSAGE_TOOL.into(),
                reason: e.to_string(),
            })?;
        let Some(handle) = self.line.handle() else {
            return Ok(ToolOutput::error(format!(
                "{SEND_MESSAGE_TOOL} is not built yet (decision 0067); not sent to `{}`: {}",
                input.to, input.text
            )));
        };
        match handle
            .send_message(ctx.worker.clone(), AgentId::new(input.to), input.text)
            .await
        {
            Ok(()) => Ok(ToolOutput::text("delivered")),
            Err(e) => Ok(ToolOutput::error(e.to_string())),
        }
    }
}

/// `request_connection`: ask the orchestrator to open a connection to another
/// session, when no rule already covers the pair.
pub struct RequestConnectionTool {
    line: SessionLine,
}

impl RequestConnectionTool {
    pub fn new(line: SessionLine) -> Self {
        Self { line }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestInput {
    to: String,
    reason: String,
}

#[async_trait::async_trait]
impl Tool for RequestConnectionTool {
    fn name(&self) -> &str {
        REQUEST_CONNECTION_TOOL
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: REQUEST_CONNECTION_TOOL.into(),
            description: "Ask the orchestrator to open a connection to another session, when \
                none of the automatic rules (parent and child, a declared dependency, a shared \
                observed read, siblings) already cover the pair. The answer names whether it \
                opened and, if not, why."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "The other session's id."},
                    "reason": {"type": "string", "description": "Why the connection is needed."}
                },
                "required": ["to", "reason"],
                "additionalProperties": false
            }),
        }
    }

    fn access(&self, _input: &serde_json::Value) -> Result<Access, ToolError> {
        Ok(Access::none())
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let input: RequestInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput {
                tool: REQUEST_CONNECTION_TOOL.into(),
                reason: e.to_string(),
            })?;
        let Some(handle) = self.line.handle() else {
            return Ok(ToolOutput::error(format!(
                "{REQUEST_CONNECTION_TOOL} is not built yet (decision 0067); not requested with \
                 `{}`: {}",
                input.to, input.reason
            )));
        };
        match handle
            .request_connection(ctx.worker.clone(), AgentId::new(input.to), input.reason)
            .await
        {
            Ok(()) => Ok(ToolOutput::text("connection open")),
            Err(e) => Ok(ToolOutput::error(e.to_string())),
        }
    }
}
