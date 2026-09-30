//! `spawn_subagent` (decision 0067, point 5): a worker spawns a child session
//! that takes one slot against the shared parallelism gate through the same
//! pre-flight admission as any agent.
//!
//! Late-bound through [`SessionLine`], the way `challenge_contract`
//! (`challenge_tool.rs`) is; registered behind the host's `sessions` flag.
//!
//! **The call returns at once.** A refusal (no free slot, the budget, a
//! guard, a path outside the caller's scope) is the tool result, with its
//! reason, so a parent never blocks waiting for a slot. A spawn returns the
//! child's id; the child then runs on its own, and its [`SubagentReport`]
//! reaches the parent later as a message (an `Injected` turn from
//! `agent:<child>`), at the parent's next tool-batch boundary. That report is
//! typed: a merge conflict is `ChildMerge::Conflict` with its paths, never a
//! silent resolution (a resolver worker handles it, decision 0073).
//!
//! **Why the access declares the child's paths.** A writer spawn declares a
//! write on every path it hands the child, so the runtime (a) refuses a path
//! outside the caller's own declared scope, (b) refuses a path another live
//! child already holds (the fence), and (c) orders the spawn against the
//! caller's other writes to those paths in the same round: earlier writes
//! land before the checkpoint the child branches from; later ones start after
//! the spawn fenced the path and are refused (`kgraf-runtime`'s start-time
//! fence check). A read-only spawn declares nothing.

use crate::admission::Refusal;
use crate::guards::GuardRefusal;
use crate::session_tools::SessionLine;
use kgraf_core::AgentId;
use kgraf_tools::{Access, CallPermit, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};

/// The tool's name, as workers call it.
pub const SPAWN_SUBAGENT_TOOL: &str = "spawn_subagent";

/// The inbox id prefix of a subagent's report to its parent: the id is this
/// plus the child's session id, so a report is delivered once whatever
/// crashes (the runtime refuses an id its transcript already holds).
pub const SUBAGENT_REPORT_PREFIX: &str = "subagent-report:";

/// The marker before the report's JSON in the text the parent receives.
pub const SUBAGENT_REPORT_MARKER: &str = "kgraf-subagent-report: ";

/// The longest child answer carried in a report, in characters. The full
/// answer stays in the child's own transcript.
pub const MAX_REPORT_ANSWER_CHARS: usize = 8_000;

/// `spawn_subagent`: a read-only or writer child session (decision 0067,
/// point 5).
pub struct SpawnSubagentTool {
    line: SessionLine,
}

impl SpawnSubagentTool {
    pub fn new(line: SessionLine) -> Self {
        Self { line }
    }
}

/// One spawn, as a worker asked for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnRequest {
    /// The child's instruction.
    pub task: String,
    /// A pinned route; the parent's route when absent.
    #[serde(default)]
    pub route: Option<String>,
    /// A subset of this worker's own write scope, for a writer child.
    #[serde(default)]
    pub writes: Vec<String>,
    /// A read-only child shares the parent's worktree for reading and may
    /// never write; a writer child gets its own worktree branched from a
    /// checkpoint of the parent's.
    pub read_only: bool,
}

/// A child that was admitted, recorded and started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawned {
    pub child: AgentId,
    pub read_only: bool,
    pub writes: Vec<String>,
    /// A soft-limit notice (decision 0067: more children under one parent
    /// than `ParallelismLimits::soft_children`). Never a refusal.
    pub notice: Option<String>,
}

/// Why a spawn was refused, before any child existed. Each names what it
/// checked, so the model that asked can correct it in one step.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SpawnRefusal {
    #[error(
        "no slot is free: {0}; the spawn was refused before it started, so continue without it or try again later"
    )]
    NoSlot(Refusal),
    #[error("{0}")]
    Guard(GuardRefusal),
    #[error("a read-only subagent writes nothing, so `writes` must be empty")]
    ReadOnlyWrites,
    #[error("a writer subagent needs at least one path in `writes`")]
    NoWrites,
    #[error("{paths:?} are outside your own write scope {scope:?}")]
    NotSubset {
        paths: Vec<String>,
        scope: Vec<String>,
    },
    #[error("{paths:?} are held by another live subagent until it finishes")]
    Fenced { paths: Vec<String> },
    #[error("`{0}` is not a running agent of this run")]
    UnknownParent(String),
    #[error("the worktree could not be prepared: {0}")]
    Workspace(String),
    #[error("the child's session could not be recorded: {0}")]
    Store(String),
    #[error("the orchestrator has stopped")]
    Stopped,
    #[error("the tool is not bound to an orchestrator")]
    Unbound,
    /// The caller's skill restrictions are not known yet (its worker is still
    /// starting), so a child could not inherit them (decision 0053).
    #[error(
        "your skill restrictions are not known yet, so a subagent could not inherit them; try again"
    )]
    SkillsUnknown,
}

/// How a finished child's work came back to its parent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChildMerge {
    /// A read-only child wrote nothing.
    ReadOnly,
    /// The child's declared changes are in the parent's worktree; `commit`
    /// is the child's result commit. `not_applied`: paths the child changed
    /// without declaring them, never applied (reported, not dropped).
    /// `warning`: something after the apply went wrong (its worktree could
    /// not be removed); the merge itself stands.
    Merged {
        commit: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        not_applied: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
    },
    /// The child changed none of its declared paths.
    Empty {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        not_applied: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
    },
    /// A plain directory with no history: nothing to merge by.
    Unversioned,
    /// The parent's worktree changed some of the child's paths after the
    /// checkpoint. Nothing was merged; both worktrees are as they were.
    Conflict {
        paths: Vec<String>,
        tip: String,
        detail: String,
    },
    /// Declared paths the child wrote are not in its result (a
    /// `.gitignore` match, a filter). Nothing was merged; both worktrees
    /// are as they were.
    Dropped { paths: Vec<String>, detail: String },
    /// The merge was refused (a protected path, a nested repository).
    Refused { detail: String },
    /// The merge could not run, or failed part-way: `applied` are the paths
    /// already in the parent's worktree.
    Failed {
        detail: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        applied: Vec<String>,
    },
    /// The child did not finish its work, so nothing of it was merged.
    NotMerged { reason: String },
}

/// What a parent is told when its child finishes: typed, so a conflict is a
/// value the parent (and a later resolver) acts on, not prose to parse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentReport {
    pub subagent: String,
    pub parent: String,
    /// The child's outcome (`completed`, `failed`, `cancelled`, ...).
    pub outcome: String,
    pub read_only: bool,
    pub merge: ChildMerge,
    /// The child's final answer, cut to [`MAX_REPORT_ANSWER_CHARS`]. Model
    /// text: untrusted, like any agent message.
    pub answer: String,
}

impl SubagentReport {
    /// The text the parent receives: one plain line, then the report as JSON
    /// after [`SUBAGENT_REPORT_MARKER`].
    pub fn render(&self) -> String {
        let headline = match &self.merge {
            ChildMerge::Merged { not_applied, .. } if not_applied.is_empty() => {
                "its changes are merged into your worktree".to_owned()
            }
            ChildMerge::Merged { not_applied, .. } => format!(
                "its declared changes are merged into your worktree; it also changed \
                 {not_applied:?}, which it did not declare, and those were NOT applied"
            ),
            ChildMerge::Conflict { paths, .. } => format!(
                "MERGE CONFLICT on {paths:?}: nothing of it was merged; your worktree and its \
                 worktree are unchanged"
            ),
            ChildMerge::ReadOnly => "it was read-only".to_owned(),
            ChildMerge::Empty { not_applied, .. } if not_applied.is_empty() => {
                "it changed nothing".to_owned()
            }
            ChildMerge::Empty { not_applied, .. } => format!(
                "it changed none of its paths; it changed {not_applied:?}, which it did not \
                 declare, and those were NOT applied"
            ),
            ChildMerge::Dropped { paths, .. } => format!(
                "DROPPED: its work on {paths:?} is not in its result (ignored or filtered); \
                 nothing was merged"
            ),
            ChildMerge::Unversioned => "its worktree has no history to merge".to_owned(),
            ChildMerge::Refused { detail } => format!("its merge was refused: {detail}"),
            ChildMerge::Failed { detail, applied } => {
                format!("its merge failed: {detail}; already applied: {applied:?}")
            }
            ChildMerge::NotMerged { reason } => format!("nothing was merged: {reason}"),
        };
        let headline = match &self.merge {
            ChildMerge::Merged {
                warning: Some(w), ..
            }
            | ChildMerge::Empty {
                warning: Some(w), ..
            } => format!("{headline} (warning: {w})"),
            _ => headline,
        };
        let json = serde_json::to_string(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
        format!(
            "Subagent `{}` finished ({}); {headline}.\n{SUBAGENT_REPORT_MARKER}{json}",
            self.subagent, self.outcome
        )
    }

    /// The report inside a text [`Self::render`] produced, if there is one.
    pub fn parse(text: &str) -> Option<Self> {
        let at = text.find(SUBAGENT_REPORT_MARKER)?;
        let rest = &text[at + SUBAGENT_REPORT_MARKER.len()..];
        let line = rest.lines().next()?;
        serde_json::from_str(line).ok()
    }
}

/// Parse the tool input; `None` when it is not a well-formed request.
fn request_of(input: &serde_json::Value) -> Result<SpawnRequest, String> {
    serde_json::from_value(input.clone()).map_err(|e| e.to_string())
}

#[async_trait::async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        SPAWN_SUBAGENT_TOOL
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: SPAWN_SUBAGENT_TOOL.into(),
            description: "Spawn a subagent: a child session with its own transcript. A \
                read-only child shares your worktree for reading; every one of its writes is \
                refused. A writer child gets its own worktree, branched from a checkpoint of \
                yours, and may write only inside `writes`, which must be inside your own \
                declared write scope; while it runs, your own writes to those paths are refused. \
                Refused at once, with a reason, if no slot is free: you are never blocked \
                waiting for one."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "The child's instruction."},
                    "route": {"type": "string", "description": "A pinned route:model, or your own route when absent."},
                    "writes": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Paths the child may write, a subset of your own write scope; empty for a read-only child."
                    },
                    "read_only": {"type": "boolean", "description": "True: the child shares your worktree for reading and writes nothing."}
                },
                "required": ["task", "read_only"],
                "additionalProperties": false
            }),
        }
    }

    /// A writer spawn declares a write on each path it hands the child (the
    /// module docs say why); a read-only spawn, and a malformed one (refused
    /// by `call`), declare nothing.
    fn access(&self, input: &serde_json::Value) -> Result<Access, ToolError> {
        let Ok(request) = request_of(input) else {
            return Ok(Access::none());
        };
        if request.read_only {
            return Ok(Access::none());
        }
        request
            .writes
            .iter()
            .try_fold(Access::none(), |access, path| access.write_path(path))
            .map_err(|source| ToolError::Access {
                tool: SPAWN_SUBAGENT_TOOL.into(),
                source,
            })
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let request = request_of(&input).map_err(|reason| ToolError::InvalidInput {
            tool: SPAWN_SUBAGENT_TOOL.into(),
            reason,
        })?;
        let Some(handle) = self.line.handle() else {
            return Ok(ToolOutput::error(format!(
                "not spawned: {}",
                SpawnRefusal::Unbound
            )));
        };
        match handle.spawn_subagent(ctx.worker.clone(), request).await {
            Ok(spawned) => Ok(ToolOutput::text(spawned_text(&spawned))),
            Err(refusal) => Ok(ToolOutput::error(format!("not spawned: {refusal}"))),
        }
    }
}

/// The tool result of a spawn.
pub fn spawned_text(s: &Spawned) -> String {
    let kind = if s.read_only {
        "read-only, sharing your worktree".to_owned()
    } else {
        format!(
            "writer on {:?}; your own writes to those paths are refused until it finishes",
            s.writes
        )
    };
    let mut text = format!(
        "spawned subagent `{}` ({kind}). It runs on its own: continue your work. Its report \
         arrives as a message from agent:{} at a later turn. If you finish first, its changes \
         are still merged into your worktree before your work is verified.",
        s.child, s.child
    );
    if let Some(notice) = &s.notice {
        text.push_str("\nNotice: ");
        text.push_str(notice);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_writer_spawn_declares_its_paths_and_a_read_only_spawn_declares_none() {
        let tool = SpawnSubagentTool::new(SessionLine::new());
        let writer = tool
            .access(&serde_json::json!({"task": "t", "read_only": false, "writes": ["src/a.rs"]}))
            .expect("access");
        assert_eq!(writer.writes_paths().len(), 1);
        let reader = tool
            .access(&serde_json::json!({"task": "t", "read_only": true}))
            .expect("access");
        assert!(reader.writes_paths().is_empty());
        assert!(!reader.is_exclusive());
    }

    #[test]
    fn a_report_round_trips_through_the_text_the_parent_receives() {
        let report = SubagentReport {
            subagent: "r1-a-r0-a0.s1".into(),
            parent: "r1-a-r0-a0".into(),
            outcome: "completed".into(),
            read_only: false,
            merge: ChildMerge::Conflict {
                paths: vec!["src/c.rs".into()],
                tip: "abc".into(),
                detail: "changed".into(),
            },
            answer: "done\nwith two lines".into(),
        };
        let text = report.render();
        assert!(text.contains("MERGE CONFLICT"), "{text}");
        assert_eq!(SubagentReport::parse(&text), Some(report));
    }
}
