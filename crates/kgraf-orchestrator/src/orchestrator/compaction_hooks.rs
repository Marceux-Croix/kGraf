//! The run loop's side of runtime compaction (decision 0095; the mechanism is
//! `kgraf_runtime::compaction`).
//!
//! 1. `stop_point_policy`, at dispatch: the remainder the worker would hand
//!    back is put to the delegation guards as a would-be child. When they
//!    would refuse it (the depth cap, the task length bound, a taken child
//!    id), or the child could not start with room to work (`handback_fit`), a
//!    hand-back would only hold the contract, so the worker is spawned with
//!    `StopPointPolicy::Compact`: it compacts and goes on, and compacts again
//!    at a later stop point while it makes measured progress
//!    (`kgraf_runtime::recompaction`). Where the guards would pass the child
//!    and it fits, the worker hands back as before.
//! 2. `record_compactions`, when the worker finishes: the runtime's notices
//!    are the orchestrator's evidence that the contract was larger than
//!    planned. Each is written to the trace (`worker_compacted`) and the run
//!    record (`compactions`); the worker counts as one sizing miss and gets
//!    one report. The ledger already holds the worker's peak context and the
//!    summary calls, which is what the sizer projects later plans from.
//! 3. `compaction_note`: a compacted worker that was not compacted again hands
//!    back with the runtime's reason (`kgraf_runtime::CompactionStop`: no room
//!    under the stop point, no measured progress, or the ceiling); when the
//!    re-split is refused, the held reason names that reason and says where
//!    the work and the full transcript are, so nothing is lost.
//! 4. `request_compaction`: `/compact <worker>` reaches the running worker's
//!    queue; the worker compacts at its next boundary.

use super::*;
use kgraf_runtime::{CompactRequestError, CompactionNotice, CompactionTrigger, StopPointPolicy};

impl RunLoop {
    /// Whether a hand-back of `contract`, briefed with `instruction`, could be
    /// split. The would-be child is worded as the runtime words a hand-back.
    pub(super) fn stop_point_policy(
        &self,
        contract: &OpenContract,
        instruction: &str,
        stop_tokens: u64,
    ) -> StopPointPolicy {
        let roster = self.roster();
        let Some(first) = roster.first() else {
            return StopPointPolicy::Compact;
        };
        let source_route = contract.route.clone().unwrap_or_else(|| first.clone());
        let task = self.flattened_remainder(
            contract,
            &kgraf_runtime::handback_remainder(instruction, stop_tokens, stop_tokens),
        );
        let child = format!("{}.h1", contract.contract);
        let taken = self.record.contract(&child).is_some()
            || self.record.verified_contracts.contains(&child)
            || self.record.split_contracts.contains(&child);
        let passes = guards::check(
            &self.delegation_policy(),
            &roster,
            &DelegationRequest {
                source_contract: &contract.contract,
                source_route: &source_route,
                target_route: &source_route,
                task: &task,
                trace: &contract.lineage,
                trace_tasks: &contract.lineage_tasks,
            },
        )
        .is_ok();
        // A child that could not start with room to work is no better than a
        // refused one: the worker compacts instead (`handback_fit`).
        let fits = {
            let mut probe = contract.clone();
            probe.instruction = kgraf_secrets::intake(&task);
            self.route(&source_route).is_none_or(|r| {
                crate::handback_fit::child_fit(
                    r.env.prefix.estimated_tokens(),
                    &brief_text(&probe),
                    r.context.worker_plan_target_tokens(),
                )
                .is_ok()
            })
        };
        if passes && !taken && fits {
            StopPointPolicy::HandBack
        } else {
            StopPointPolicy::Compact
        }
    }

    /// A hand-back remainder the runtime made from `parent`'s assembled brief,
    /// brought back to one wrapper around the chain's original instruction, so
    /// a child's instruction does not grow with the depth of the chain.
    pub(super) fn flattened_remainder(&self, parent: &OpenContract, item: &str) -> String {
        let parent_instruction = kgraf_secrets::model_copy(&parent.instruction).into_owned();
        let root = parent
            .lineage_tasks
            .iter()
            .rev()
            .map(|t| kgraf_secrets::model_copy(t).into_owned())
            .find(|t| !t.starts_with(kgraf_runtime::REMAINDER_HEAD))
            .unwrap_or_else(|| parent_instruction.clone());
        // The child's place in the chain: the remainders already in its lineage,
        // and itself.
        let level = 1 + parent
            .lineage_tasks
            .iter()
            .filter(|t| kgraf_secrets::model_copy(t).starts_with(kgraf_runtime::REMAINDER_HEAD))
            .count();
        crate::handback_fit::flatten_remainder(
            item,
            &brief_text(parent),
            &parent_instruction,
            &root,
            level,
        )
    }

    /// Record a worker's compactions, in order: each to the trace and the
    /// run record; one sizing miss for the worker when any came from the stop
    /// point (a worker is one miss however often it compacted), and one
    /// report.
    pub(super) async fn record_compactions(
        &mut self,
        contract: &OpenContract,
        worker: &AgentId,
        notices: &[&CompactionNotice],
    ) -> Result<(), OrchestratorError> {
        let Some(last) = notices.last() else {
            return Ok(());
        };
        for notice in notices {
            self.trace(TraceEvent::WorkerCompacted {
                at_ms: notice.at_ms,
                contract: contract.contract.clone(),
                worker: worker.to_string(),
                notice: (*notice).clone(),
            })
            .await?;
            // A user's request says nothing about the plan; the stop point does.
            let sizing_miss = notice.trigger == CompactionTrigger::StopPoint;
            self.record
                .compactions
                .push(crate::run_state::CompactionRecord {
                    contract: contract.contract.clone(),
                    worker: worker.to_string(),
                    sizing_miss,
                    notice: (*notice).clone(),
                });
        }
        let sizing_miss = notices
            .iter()
            .any(|n| n.trigger == CompactionTrigger::StopPoint);
        if sizing_miss {
            self.record.counters.sizing_misses += 1;
        }
        let at: Vec<String> = notices.iter().map(|n| n.at_tokens.to_string()).collect();
        let text = if sizing_miss {
            format!(
                "compacted {} at {} of {} tokens: the contract was larger than planned; {}",
                times(notices.len()),
                at.join(", "),
                last.window_tokens,
                transcript_words(last)
            )
        } else {
            format!(
                "compacted {} on request at {} tokens; {}",
                times(notices.len()),
                at.join(", "),
                transcript_words(last)
            )
        };
        self.submit_report(contract, Some(worker), ReportKind::Informational, &text);
        Ok(())
    }

    /// What a held reason adds when the hand-back came from a worker that had
    /// already compacted: how often, why it was not compacted again (`stop`,
    /// from the runtime), and where its work and its full history are.
    pub(super) fn compaction_note(
        &self,
        worker: &AgentId,
        stop: Option<&kgraf_runtime::CompactionStop>,
    ) -> String {
        let mine: Vec<&crate::run_state::CompactionRecord> = self
            .record
            .compactions
            .iter()
            .filter(|c| c.worker == worker.as_str())
            .collect();
        let Some(last) = mine.last() else {
            return String::new();
        };
        let why = stop.map_or_else(String::new, |s| format!(": {}", s.describe()));
        format!(
            "; the worker had already compacted {} (last at {} tokens) and reached its stop \
             point again{why}; its work so far is kept in the worktree, and {}",
            times(mine.len()),
            last.notice.at_tokens,
            transcript_words(&last.notice)
        )
    }

    /// `/compact <worker>`: ask the running worker to compact at its next
    /// boundary.
    pub(super) fn request_compaction(&self, worker: String) -> Result<(), CompactError> {
        let slot = self.in_flight.values().find_map(|f| {
            f.injectors
                .iter()
                .find(|(w, _)| w.as_str() == worker)
                .map(|(_, slot)| slot.clone())
        });
        let Some(slot) = slot else {
            return Err(CompactError::UnknownWorker(worker));
        };
        let Some(injector) = slot.get() else {
            return Err(CompactError::NotStarted(worker));
        };
        injector.request_compaction().map_err(|e| match e {
            CompactRequestError::Finished { .. } => CompactError::UnknownWorker(worker),
            CompactRequestError::AlreadyCompacted { .. } => CompactError::AlreadyCompacted(worker),
        })
    }
}

/// A count of compactions in words.
fn times(n: usize) -> String {
    match n {
        1 => "once".to_owned(),
        2 => "twice".to_owned(),
        n => format!("{n} times"),
    }
}

/// Where the pre-compaction history is, in words.
fn transcript_words(notice: &CompactionNotice) -> String {
    match &notice.transcript_path {
        Some(path) => format!(
            "the full transcript before compaction is kept in {}",
            path.display()
        ),
        None => "the full transcript before compaction is kept in the worker's transcript".into(),
    }
}
