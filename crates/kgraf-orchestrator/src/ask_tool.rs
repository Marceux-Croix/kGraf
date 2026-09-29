//! `ask_user_question` (decision 0067, point 4): a worker pauses for the
//! user's answer the way an approval ask pauses today (`AskDesk`).
//!
//! **Late-bound**, like `challenge_contract` (`challenge_tool.rs`): the tool is
//! registered before the orchestrator exists, so it holds a [`SessionLine`]
//! the host binds once the orchestrator is built. Behind the host's `sessions`
//! flag (default off; `kgraf-host/src/config.rs`), so `main` never shows
//! workers a tool that always fails.
//!
//! Until the host binds a hook, `call` answers "not built yet". The bound hook
//! runs through `AskDesk` and `answer_question`.

use crate::session_tools::SessionLine;
use kgraf_tools::{Access, CallPermit, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::Deserialize;

/// The tool's name, as workers call it.
pub const ASK_USER_QUESTION_TOOL: &str = "ask_user_question";

/// `ask_user_question`: pause and ask the user, as `AskDesk` already pauses an
/// approval (decision 0067, point 4).
pub struct AskUserQuestionTool {
    line: SessionLine,
}

impl AskUserQuestionTool {
    pub fn new(line: SessionLine) -> Self {
        Self { line }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    question: String,
}

#[async_trait::async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        ASK_USER_QUESTION_TOOL
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: ASK_USER_QUESTION_TOOL.into(),
            description: "Ask the user a direct question and wait for their answer. Your call \
                pauses the way an approval ask pauses. On a scheduled run with nobody present, \
                you are told no answer came and to state your assumption instead."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "The question, in prose."}
                },
                "required": ["question"],
                "additionalProperties": false
            }),
        }
    }

    /// Touches no file: the question and its answer live in the run record and
    /// the desk, not the worktree.
    fn access(&self, _input: &serde_json::Value) -> Result<Access, ToolError> {
        Ok(Access::none())
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = serde_json::from_value(input).map_err(|e| ToolError::InvalidInput {
            tool: ASK_USER_QUESTION_TOOL.into(),
            reason: e.to_string(),
        })?;
        let Some(handle) = self.line.handle() else {
            return Ok(ToolOutput::error(format!(
                "{ASK_USER_QUESTION_TOOL} is not built yet (decision 0067); your question was: {}",
                input.question
            )));
        };
        match handle
            .ask_question(ctx.worker.clone(), input.question)
            .await
        {
            Ok(answer) => Ok(ToolOutput::text(answer)),
            Err(e) => Ok(ToolOutput::error(e.to_string())),
        }
    }
}
