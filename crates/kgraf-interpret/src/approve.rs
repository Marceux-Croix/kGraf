//! Approval and seeding: the orchestrator builds only from an approved plan.
//!
//! [`ApprovedPlan`] is made only by [`Interpretation::approve`] or
//! [`approve_stored`], and both are bound to a plan id: the id printed with the
//! reading the user saw. Code that takes an `ApprovedPlan` cannot be handed an
//! unapproved reading ("the orchestrator refuses to build before there is
//! an agreed plan"). [`approve_stored`] loads the stored answer of that id and the
//! command assessment stored with it, and never makes a model call; an id with no
//! stored reading is refused, and so is one whose corpus changed since it was read.
//!
//! **Commands are accepted one by one.** Approval declares only the proposed
//! validation commands the user names; the rest stay proposed, are not gates, and
//! criteria that name them stay flagged as not checkable. In every mode.
//!
//! **Work already done is not redone.** An item that derives Complete is not
//! seeded as work; one that derives Partial is seeded with what exists named, so
//! the worker finishes it rather than rewriting it; a dependency on a complete item
//! is already satisfied and dropped from the seed (it stays in the graph's work
//! edges).
//!
//! **Seeding** writes the plan to the graph as work nodes (`kgraf_graph::work`):
//! a plan root, one node per component and per item, and the dependency edges.
//! Nodes hold facts only: what was planned and what the documents claim, never a
//! status.

use crate::corpus::GatherOptions;
use crate::error::InterpretError;
use crate::model::{
    Check, CommandProvenance, PlanLimits, Verifiability, normalize_command, path_under,
};
use crate::pass::{Interpretation, assess_all, discovered, load_for_approval};
use crate::reconcile::TestResults;
use crate::reconcile::{Doneness, TestOutcome};
use kgraf_graph::{GraphStore, WorkKind, WorkNode, WorkWrite};
use serde_json::json;

/// A write a constraint's `forbidden_paths` struck from a work item, recorded
/// so approval can show it happened (decision 0013: "demonstrably bind", not
/// silent). The item is seeded without this write; if that was its only
/// write, the item is seeded with none, and the existing write-scope check
/// (`kgraf-host`'s `scope_violations`) then refuses any change at all from
/// its worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedWrite {
    pub item: String,
    pub path: String,
    pub constraint: String,
}

/// One item to hand the orchestrator, in plan order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedItem {
    pub id: String,
    /// The item's instruction; for a partial item, prefixed with what exists.
    pub instruction: String,
    /// The criteria, each marked with its check or with why it cannot be checked.
    pub acceptance: String,
    pub writes: Vec<String>,
    /// Dependencies that are not yet complete.
    pub depends_on: Vec<String>,
    /// Symbols that exist in the graph, for the worker's slices.
    pub slices: Vec<String>,
}

/// What [`ApprovedPlan::seed`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Seeded {
    pub write: WorkWrite,
    /// (presumed node, the plan item node that adopted it), for each presumed
    /// node adopted by this seed.
    pub adopted: Vec<(String, String)>,
}

/// A plan the user approved. Only [`Interpretation::approve`] and
/// [`approve_stored`] make one.
#[derive(Clone, Debug)]
pub struct ApprovedPlan {
    interpretation: Interpretation,
    items: Vec<SeedItem>,
    skipped: Vec<String>,
    /// The declared validation commands: discovered, plus the proposed ones
    /// accepted by name.
    commands: Vec<String>,
    /// Writes a `forbidden_paths` constraint struck before seeding.
    blocked: Vec<BlockedWrite>,
}

/// Approve the stored reading `plan_id` (see the module documentation). No model
/// call is made. Doneness is derived from the code as it is now.
pub fn approve_stored(
    store: &GraphStore,
    plan_id: &str,
    accepted_commands: &[String],
    limits: &PlanLimits,
    gather: &GatherOptions,
    tests: &dyn TestResults,
) -> Result<ApprovedPlan, InterpretError> {
    load_for_approval(store, plan_id, limits, gather, tests)?.approve(plan_id, accepted_commands)
}

impl Interpretation {
    /// Approve this reading. `plan_id` must be this reading's id (the one the user
    /// was shown); `accepted_commands` are the proposed commands the user accepts
    /// as gates, by name. Refused when it has no work items (a scoping round:
    /// answer the questions, then interpret again with them as corrections).
    pub fn approve(
        mut self,
        plan_id: &str,
        accepted_commands: &[String],
    ) -> Result<ApprovedPlan, InterpretError> {
        if plan_id != self.plan_id {
            return Err(InterpretError::PlanIdMismatch {
                expected: self.plan_id.clone(),
                given: plan_id.to_owned(),
            });
        }
        if self.answer.work_items.is_empty() {
            return Err(InterpretError::NothingToApprove {
                questions: self.answer.questions.clone(),
            });
        }
        let proposed: Vec<String> = self
            .proposed_commands()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut declared = discovered(&self.commands);
        for c in accepted_commands {
            let c = normalize_command(c);
            if !proposed.contains(&c) {
                return Err(InterpretError::UnknownCommand { command: c });
            }
            declared.insert(c);
        }
        self.criteria = assess_all(&self.answer, &declared);
        let mut items = Vec::new();
        let mut skipped = Vec::new();
        let mut blocked = Vec::new();
        for w in &self.answer.work_items {
            let state = self.state(&w.id).cloned();
            if state
                .as_ref()
                .is_some_and(|s| s.doneness == Doneness::Complete)
            {
                skipped.push(w.id.clone());
                continue;
            }
            let mut instruction = String::new();
            if let Some(s) = state.as_ref().filter(|s| s.doneness == Doneness::Partial) {
                instruction.push_str(&format!(
                    "Part of this work already exists; build on it and do not rewrite it. Present: {}.",
                    s.present.join(", ")
                ));
                if !s.missing.is_empty() {
                    instruction.push_str(&format!(" Missing: {}.", s.missing.join(", ")));
                }
                let pending: Vec<&str> = s
                    .tests
                    .iter()
                    .filter(|t| !(t.exists && t.outcome == TestOutcome::Passed))
                    .map(|t| t.name.as_str())
                    .collect();
                if !pending.is_empty() {
                    instruction.push_str(&format!(
                        " Tests that must exist and pass: {}.",
                        pending.join(", ")
                    ));
                }
                instruction.push_str("\n\n");
            }
            instruction.push_str(&w.instruction);
            let verdicts = self.criteria.get(&w.id).cloned().unwrap_or_default();
            let acceptance = w
                .acceptance
                .iter()
                .zip(verdicts.iter())
                .map(|(c, v)| match v {
                    Verifiability::Mechanical { check } => {
                        format!("- {} [check: {}]", c.text, describe(check))
                    }
                    Verifiability::Unverifiable { reason, needs } => format!(
                        "- {} [cannot be checked automatically: {reason}; needs {needs}]",
                        c.text
                    ),
                })
                .collect::<Vec<_>>()
                .join("\n");
            let slices = state
                .as_ref()
                .map(|s| {
                    s.present
                        .iter()
                        .filter(|p| w.evidence.symbols.contains(p))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            let writes = w
                .writes
                .iter()
                .filter(|p| {
                    match self.answer.constraints.iter().find(|c| {
                        c.forbidden_paths
                            .iter()
                            .any(|forbidden| path_under(p, forbidden))
                    }) {
                        Some(c) => {
                            blocked.push(BlockedWrite {
                                item: w.id.clone(),
                                path: (*p).clone(),
                                constraint: c.text.clone(),
                            });
                            false
                        }
                        None => true,
                    }
                })
                .cloned()
                .collect();
            items.push(SeedItem {
                id: w.id.clone(),
                instruction,
                acceptance,
                writes,
                depends_on: w
                    .depends_on
                    .iter()
                    .filter(|d| !self.complete_item(d))
                    .cloned()
                    .collect(),
                slices,
            });
        }
        let commands = self
            .commands
            .iter()
            .map(|c| c.command.clone())
            .filter(|c| declared.contains(c))
            .collect();
        Ok(ApprovedPlan {
            interpretation: self,
            items,
            skipped,
            commands,
            blocked,
        })
    }

    fn complete_item(&self, item: &str) -> bool {
        self.state(item)
            .is_some_and(|s| s.doneness == Doneness::Complete)
    }
}

fn describe(check: &Check) -> String {
    match check {
        Check::Command { command } => format!("run `{command}`"),
        Check::Test { name } => format!("test `{name}` passes"),
        Check::Symbol { name } => format!("symbol `{name}` exists"),
        Check::Path { path } => format!("path `{path}` exists"),
        Check::Manual => "manual".into(),
    }
}

impl ApprovedPlan {
    pub fn plan_id(&self) -> &str {
        &self.interpretation.plan_id
    }

    pub fn interpretation(&self) -> &Interpretation {
        &self.interpretation
    }

    /// Items to build, in plan order.
    pub fn items(&self) -> &[SeedItem] {
        &self.items
    }

    /// Items not seeded because they derive Complete.
    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }

    /// Writes a `forbidden_paths` constraint struck before seeding (decision
    /// 0013: constraints demonstrably bind). Empty when no constraint named a
    /// forbidden path, or none of the plan's writes fell under one.
    pub fn blocked_writes(&self) -> &[BlockedWrite] {
        &self.blocked
    }

    /// The instruction the run pins and re-grounds on (0057): the user's own,
    /// verbatim; with none, the approved reading, which is what the user agreed to.
    pub fn original_instruction(&self) -> String {
        match self
            .interpretation
            .instruction
            .as_deref()
            .filter(|i| !i.trim().is_empty())
        {
            Some(i) => i.to_owned(),
            None => format!(
                "Continue the project in this repository as its documents plan it. The reading \
                 you approved (plan {}):\n{}",
                self.interpretation.plan_id, self.interpretation.answer.reading
            ),
        }
    }

    /// The declared validation commands: discovered in the project's documents,
    /// or proposed and accepted by name. Never a proposal nobody accepted.
    pub fn validation_commands(&self) -> Vec<String> {
        self.commands.to_vec()
    }

    /// Write the plan to the graph as work nodes, then mark the presumed nodes
    /// its items stand for as adopted by them (decision 0064,
    /// `presumed::adopt_for_plan`): a presumed node keeps its id and gains the
    /// item that took it over, so the roadmap's node and the plan's item are one
    /// piece of work, not two. Idempotent: the same plan again writes nothing
    /// new and adopts nothing again.
    pub fn seed(&self, store: &GraphStore, now_ms: i64) -> Result<Seeded, InterpretError> {
        let write = self.seed_nodes(store, now_ms)?;
        let adopted = crate::presumed::adopt_for_plan(
            store,
            &self.interpretation.plan_id,
            &self.interpretation.answer.work_items,
            now_ms,
        )?;
        Ok(Seeded { write, adopted })
    }

    fn seed_nodes(&self, store: &GraphStore, now_ms: i64) -> Result<WorkWrite, InterpretError> {
        let i = &self.interpretation;
        let a = &i.answer;
        let plan = i.plan_id.as_str();
        let root = format!("{plan}/plan");
        let component = |id: &str| format!("{plan}/component/{id}");
        let item = |id: &str| format!("{plan}/item/{id}");
        let body = |v: serde_json::Value| v.to_string();
        let mut nodes = vec![WorkNode {
            id: root.clone(),
            plan_id: plan.to_owned(),
            kind: WorkKind::Plan,
            parent_id: None,
            title: format!("{:?} plan", i.mode),
            body: body(json!({
                "reading": a.reading,
                "mode": i.mode,
                "instruction": i.instruction,
                "corrections": i.corrections,
                "constraints": a.constraints,
                "validation_commands": i.commands.iter().map(|c| json!({
                    "command": c.command,
                    "declared_by": match &c.provenance {
                        CommandProvenance::Discovered { path, .. } => path.clone(),
                        CommandProvenance::Proposed if self.commands.contains(&c.command) => {
                            "accepted by name at approval".to_owned()
                        }
                        CommandProvenance::Proposed => "proposed, not accepted: not a gate".to_owned(),
                    },
                })).collect::<Vec<_>>(),
                "questions": a.questions,
                // Counts, not the omitted list: that can run to thousands of labels.
                "brief": {
                    "tokens": i.brief.tokens,
                    "budget_tokens": i.brief.budget_tokens,
                    "sections_included": i.brief.sections_included,
                    "sections_total": i.brief.sections_total,
                    "sections_omitted": i.brief.omitted.len(),
                    "definitions_included": i.brief.definitions_included,
                    "definitions_ranked": i.brief.definitions_ranked,
                },
                "interpretation_call": i.call,
            })),
            created_ms: now_ms,
        }];
        for c in &a.components {
            nodes.push(WorkNode {
                id: component(&c.id),
                plan_id: plan.to_owned(),
                kind: WorkKind::Component,
                parent_id: Some(root.clone()),
                title: c.name.clone(),
                body: body(json!({ "purpose": c.purpose, "paths": c.paths })),
                created_ms: now_ms,
            });
        }
        let mut deps = Vec::new();
        for w in &a.work_items {
            nodes.push(WorkNode {
                id: item(&w.id),
                plan_id: plan.to_owned(),
                kind: WorkKind::Item,
                parent_id: Some(match &w.component {
                    Some(c) => component(c),
                    None => root.clone(),
                }),
                title: w.title.clone(),
                body: body(json!({
                    "instruction": w.instruction,
                    "acceptance": w.acceptance.iter().zip(i.criteria.get(&w.id).into_iter().flatten())
                        .map(|(c, v)| json!({ "text": c.text, "assessment": v }))
                        .collect::<Vec<_>>(),
                    "evidence": w.evidence,
                    "writes": w.writes,
                    "documents_claim": w.documents_claim,
                    "sources": w.sources,
                })),
                created_ms: now_ms,
            });
            for d in &w.depends_on {
                deps.push((item(&w.id), item(d)));
            }
        }
        Ok(store.put_work(&nodes, &deps)?)
    }
}
