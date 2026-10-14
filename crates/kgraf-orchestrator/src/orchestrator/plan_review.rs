//! Plan review (decision 0094): with the user's option on, the plan is shown
//! after planning and no worker starts until the user says yes.
//!
//! ```text
//!   planned --review off, or approved before--> run
//!   planned --approve_plan (command line)--> approved by --approve-plan --> run
//!   planned --nobody attends--> refused (fail loud, before any planning call)
//!   planned --shown--> waiting
//!   waiting --Approve--> approved by the user --> run
//!   waiting --Reject--> blocked, nothing dispatched
//!   waiting --Change(note)--> the note is recorded for the planner --> planned again --> shown
//!   waiting --review turned off--> approved by turning review off --> run
//!   waiting --stop--> stopped (resumable; the plan is shown again on resume)
//! ```

use super::*;
use crate::review_desk::{PlanAnswer, PlanItemView, PlanView};
use crate::run_state::PlanApproval;

/// Plan review is on and nobody can approve: the one rule, so a caller that
/// knows these three facts before it creates anything (the command line, which
/// is never attended) refuses with the same error the orchestrator would.
pub fn plan_review_refusal(
    plan_review: bool,
    attended: bool,
    approve_plan: bool,
) -> Result<(), OrchestratorError> {
    if plan_review && !attended && !approve_plan {
        return Err(OrchestratorError::PlanReviewUnattended);
    }
    Ok(())
}

impl Orchestrator {
    /// Plan review is on and nobody can approve: refused before anything is
    /// planned or paid for.
    pub(super) fn check_plan_review(&self) -> Result<(), OrchestratorError> {
        plan_review_refusal(
            self.perms.control.plan_review(),
            self.perms.attended,
            self.config.approve_plan,
        )
    }
}

impl RunLoop {
    /// Review the plan if it is due, then run; the desk is cleared when the
    /// run ends, however it ends.
    pub(super) async fn run_reviewed(
        mut self,
        planned: Result<bool, OrchestratorError>,
    ) -> Result<RunReport, OrchestratorError> {
        let desk = self.perms.desk.clone();
        let planned = match planned {
            Ok(true) => on_heap(|| self.review_plan()).await,
            other => other,
        };
        let planned = match planned {
            Ok(p) => on_heap(|| self.open_permissions()).await.map(|()| p),
            Err(e) => Err(e),
        };
        // On the heap (`kgraf_core::heap_future`); `run_after` takes the
        // loop by value, so the desk is cloned first.
        let report = on_heap(|| self.run_after(planned)).await;
        desk.close();
        report
    }

    /// Whether the plan must be shown: review is on, no yes was recorded,
    /// and nothing of the plan has started.
    fn plan_review_due(&self) -> bool {
        self.perms.control.plan_review()
            && !self.small_review_skipped()
            && self.record.plan_approved.is_none()
            && self.record.planned
            && !self.record.open_contracts.is_empty()
            && self.record.verified_contracts.is_empty()
            && self
                .record
                .open_contracts
                .iter()
                .all(|c| c.status == ContractStatus::Planned && c.workers.is_empty())
    }

    /// The plan as the box shows it: each contract, what it depends on,
    /// what it writes and what it reads.
    fn plan_view(&self, revision: u32) -> PlanView {
        PlanView {
            run: self.record.run.to_string(),
            revision,
            items: self
                .record
                .open_contracts
                .iter()
                .map(|c| PlanItemView {
                    contract: c.contract.clone(),
                    instruction: c.instruction.clone(),
                    acceptance: c.acceptance.clone(),
                    depends_on: c.depends_on.clone(),
                    writes: c.writes.iter().map(|w| w.path.clone()).collect(),
                    slices: c.slices.iter().map(|n| n.to_string()).collect(),
                    route: c.route.clone(),
                })
                .collect(),
        }
    }

    async fn plan_approved(&mut self, revision: u32, by: &str) -> Result<(), OrchestratorError> {
        let at_ms = self.now();
        self.record.plan_approved = Some(PlanApproval {
            at_ms,
            by: by.to_owned(),
            revision,
        });
        self.perms.desk.clear_plan();
        self.save()?;
        self.trace(TraceEvent::PlanReview {
            at_ms,
            revision,
            outcome: "approved".into(),
            detail: format!("approved by {by}"),
        })
        .await?;
        self.note_permission(
            "plan_approved",
            "",
            "",
            format!("plan {revision} approved by {by}"),
        )
        .await
    }

    /// Show the plan until the user approves it, rejects it or the run stops.
    /// `Ok(true)`: dispatch; `Ok(false)`: dispatch nothing.
    async fn review_plan(&mut self) -> Result<bool, OrchestratorError> {
        let mut revision = 0;
        loop {
            if !self.plan_review_due() {
                return Ok(true);
            }
            revision += 1;
            if self.config.approve_plan {
                self.plan_approved(revision, "--approve-plan").await?;
                return Ok(true);
            }
            if !self.perms.attended {
                return Err(OrchestratorError::PlanReviewUnattended);
            }
            let view = self.plan_view(revision);
            let at_ms = self.now();
            self.trace(TraceEvent::PlanReview {
                at_ms,
                revision,
                outcome: "shown".into(),
                detail: format!("{} contract(s)", view.items.len()),
            })
            .await?;
            self.perms.desk.show_plan(view);
            let mut changes = self.perms.control.subscribe();
            let answer = loop {
                if let Some(a) = self.perms.desk.take_plan_answer() {
                    break Some(a);
                }
                if !self.perms.control.plan_review() {
                    break Some(PlanAnswer::Approve);
                }
                if self.stop_requested() {
                    break None;
                }
                let answered = self.perms.desk.answered();
                tokio::select! {
                    () = answered => {}
                    _ = changes.changed() => {}
                    m = self.rx.recv() => match m {
                        Some(LoopMsg::Control(Control::Stop)) => self.stopping = true,
                        Some(other) => self.backlog.push_back(other),
                        None => {}
                    },
                }
            };
            match answer {
                None => {
                    self.perms.desk.clear_plan();
                    return Ok(false);
                }
                Some(PlanAnswer::Approve) => {
                    let by = if self.perms.control.plan_review() {
                        "the user"
                    } else {
                        "turning plan review off"
                    };
                    self.plan_approved(revision, by).await?;
                    return Ok(true);
                }
                Some(PlanAnswer::Reject { reason }) => {
                    let at_ms = self.now();
                    let reason = kgraf_secrets::scrub_persist_owned(reason);
                    let detail = if reason.trim().is_empty() {
                        "the user rejected the plan".to_owned()
                    } else {
                        format!("the user rejected the plan: {reason}")
                    };
                    self.trace(TraceEvent::PlanReview {
                        at_ms,
                        revision,
                        outcome: "rejected".into(),
                        detail: detail.clone(),
                    })
                    .await?;
                    self.note_permission("plan_rejected", "", "", detail.clone())
                        .await?;
                    self.record.status = RunStatus::Blocked { reason: detail };
                    self.stopping = true;
                    self.save()?;
                    return Ok(false);
                }
                Some(PlanAnswer::Change { note }) => {
                    let at_ms = self.now();
                    self.trace(TraceEvent::PlanReview {
                        at_ms,
                        revision,
                        outcome: "changed".into(),
                        detail: kgraf_secrets::scrub_persist_owned(note.clone()),
                    })
                    .await?;
                    self.note_permission(
                        "plan_changed",
                        "",
                        "",
                        format!("plan {revision} sent back to the planner with a change"),
                    )
                    .await?;
                    let n = self.record.notes.len();
                    self.record.notes.push(RecordedNote {
                        id: format!("plan-change-{revision}-{n}"),
                        channel: "plan_review".into(),
                        received_ms: at_ms,
                        kind: InboundKind::Note,
                        // Scrubbed at intake; the planner reads it as data.
                        text: UntrustedText::new(format!(
                            "The user reviewed the plan and asked for this change before \
                             any work starts: {note}"
                        ))
                        .for_storage(),
                        delivered: false,
                    });
                    self.discard_plan()?;
                    if !self.plan().await? {
                        return Ok(false);
                    }
                }
            }
        }
    }

    /// Forget an unstarted plan so it is planned again.
    fn discard_plan(&mut self) -> Result<(), OrchestratorError> {
        self.record.open_contracts.clear();
        self.record.coordination.gates.clear();
        self.record.coordination.base_commit = None;
        self.record.planned = false;
        self.record.plan_progress = None;
        self.save()
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::plan_review_refusal;

    /// Refused exactly when review is on and neither a person nor
    /// `--approve-plan` can say yes.
    #[test]
    fn only_review_on_with_nobody_and_no_approval_is_refused() {
        for plan_review in [false, true] {
            for attended in [false, true] {
                for approve_plan in [false, true] {
                    let refused = plan_review_refusal(plan_review, attended, approve_plan).is_err();
                    assert_eq!(
                        refused,
                        plan_review && !attended && !approve_plan,
                        "review {plan_review}, attended {attended}, approve {approve_plan}"
                    );
                }
            }
        }
    }
}
