//! The run loop's side of a shortfall report (decisions 0093 and 0094; see
//! `crate::shortfall` for the rules).
//!
//! 1. `on_shortfall`: the report arrives while the worker runs. A permission
//!    report that its worker's refusals do not ground, or that asks for a
//!    path needing no grant or owned by another contract, is answered and the
//!    worker continues (`answer_shortfall`, bounded). Otherwise the
//!    orchestrator decides at once under the user's permission mode (grant,
//!    amend, ask the user, or hold), records the decision on the contract and
//!    in the trace, answers the worker, then cancels the attempt so the
//!    worker stops rather than guesses.
//! 2. `take_shortfall`: when that worker finishes, its attempt is marked
//!    stopped (not a candidate, not a failure) and its worker id retired, so
//!    the next attempt is a fresh worker built with the grant.
//!    `take_shortfall_stuck`: a worker stopped for reporting past the bound
//!    of answered reports finishes as an error, counted a failure.
//! 3. `finalize_ordinary` applies a held decision as the contract's held
//!    reason, once every attempt has finished. A request waiting for the
//!    user holds the contract until it is answered (`permission_hooks`).

use super::*;
use crate::challenge::ContractReply;
use crate::shortfall::{
    Grounding, MAX_ANSWERED_SHORTFALLS, RequestScope, ShortfallDecision, ShortfallKind,
    decide_in_plan, instructions_answer, path_answer, skill_hold_reason, stored,
};

impl RunLoop {
    /// Seven arguments: the six fields of the report as the control carries
    /// them, plus `self`. Bundling them would rebuild the control variant.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn on_shortfall(
        &mut self,
        worker: AgentId,
        kind: ShortfallKind,
        scope: RequestScope,
        need: String,
        why: String,
        grounding: Option<Grounding>,
        skill_refusal: Option<String>,
    ) -> Result<ContractReply, OrchestratorError> {
        let Some(contract_id) = self
            .in_flight
            .iter()
            .find(|(_, f)| f.attempts.iter().any(|a| a.worker == worker))
            .map(|(c, _)| c.clone())
        else {
            return Ok(ContractReply::Refused(format!(
                "worker `{worker}` is not running an attempt of this run"
            )));
        };
        let Some(contract) = self.record.contract(&contract_id).cloned() else {
            return Ok(ContractReply::Refused(format!(
                "contract `{contract_id}` is not open"
            )));
        };
        // One report per attempt: the attempt is stopping.
        if contract.shortfall_workers.contains(&worker)
            || contract.shortfall_stuck.contains(&worker)
        {
            return Ok(ContractReply::Refused(
                "this attempt already reported a shortfall; stop now".into(),
            ));
        }
        // A request already waiting for the user on this contract is the
        // one the user sees; a second attempt's report waits behind it.
        if contract.pending_permission.is_some() {
            return Ok(ContractReply::Refused(
                "a permission request for this contract is already waiting for the user; stop \
                 now"
                .into(),
            ));
        }
        // Decision 0093, amended 2026-10-01: a permission report is decided
        // on the subject its worker's refusals ground it in; a report that
        // grounds nothing, or asks for a path that needs no grant or belongs
        // to another contract, is answered and the worker continues.
        let asked = stored(&need);
        let why = stored(&why);
        let (need, why) = match (kind, grounding) {
            (ShortfallKind::Instructions, _) => {
                // Decision 0093, amended 2026-10-01: a report that asks for a
                // change to a file another contract owns is answered, not
                // written into this contract's own instruction, where it
                // would change nothing.
                if let Some(answer) =
                    instructions_answer(&asked, &why, &contract, &self.record.open_contracts)
                {
                    return self
                        .answer_shortfall(&contract_id, &worker, kind, &asked, &why, answer, false)
                        .await;
                }
                (asked, why)
            }
            (ShortfallKind::Permission, None) => {
                return Ok(ContractReply::Refused(
                    "a permission report must carry the worker's refusal record".into(),
                ));
            }
            (ShortfallKind::Permission, Some(Grounding::NothingRefused { answer })) => {
                return self
                    .answer_shortfall(&contract_id, &worker, kind, &asked, &why, answer, false)
                    .await;
            }
            (ShortfallKind::Permission, Some(Grounding::Ambiguous { answer })) => {
                return self
                    .answer_shortfall(&contract_id, &worker, kind, &asked, &why, answer, true)
                    .await;
            }
            (ShortfallKind::Permission, Some(Grounding::Refused { subject, reason })) => {
                let answer = (scope == RequestScope::File)
                    .then(|| path_answer(&subject, &contract, &self.record.open_contracts, &reason))
                    .flatten();
                if let Some(answer) = answer {
                    return self
                        .answer_shortfall(
                            &contract_id,
                            &worker,
                            kind,
                            &subject,
                            &why,
                            answer,
                            false,
                        )
                        .await;
                }
                let why = if asked == subject {
                    why
                } else {
                    stored(&format!("{why} (the worker asked for: {asked})"))
                };
                (stored(&subject), why)
            }
        };
        let decision = decide_in_plan(
            kind,
            scope,
            &need,
            &why,
            &contract,
            &self.authority(),
            Some(&self.record.open_contracts),
        );
        let decision = match decision {
            ShortfallDecision::Grant { grant, .. } | ShortfallDecision::AskUser { grant, .. }
                if self.grant_unappliable(&grant).is_some() =>
            {
                ShortfallDecision::Hold {
                    reason: self.grant_unappliable(&grant).unwrap_or_default(),
                }
            }
            other => other,
        };
        // A skill's `allowed-tools` is not the permission mode's to lift: a
        // grant would start a fresh attempt the same skill refuses again. The
        // contract is held, naming the skill.
        let decision = match (kind, skill_refusal) {
            (ShortfallKind::Permission, Some(refusal)) => ShortfallDecision::Hold {
                reason: skill_hold_reason(&refusal),
            },
            _ => decision,
        };
        let now = self.now();
        let request = match &decision {
            ShortfallDecision::AskUser { grant, question } => Some(self.raise_request(
                &contract_id,
                grant,
                worker.as_str(),
                &need,
                &why,
                question,
            )),
            _ => None,
        };
        if let Some(c) = self.record.contract_mut(&contract_id) {
            c.shortfall_workers.push(worker.clone());
            match &decision {
                ShortfallDecision::Grant { grant, by } => {
                    Self::apply_grant(c, grant, by, worker.as_str(), &why, now);
                }
                ShortfallDecision::Amend => {
                    c.shortfall_notes.push(format!("{need}: {why}"));
                    c.shortfall_redispatches += 1;
                }
                ShortfallDecision::AskUser { question, .. } => {
                    c.pending_permission = request;
                    c.shortfall_hold = Some(kgraf_secrets::scrub_persist_owned(format!(
                        "waiting for your answer: {question}"
                    )));
                }
                ShortfallDecision::Hold { reason } => {
                    c.shortfall_hold = Some(kgraf_secrets::scrub_persist_owned(reason.clone()));
                }
            }
        }
        let resolution = decision.describe();
        self.trace(TraceEvent::Shortfall {
            at_ms: now,
            contract: contract_id.clone(),
            worker: worker.to_string(),
            kind: kind.as_str().to_owned(),
            need: need.clone(),
            why: why.clone(),
            resolution: resolution.clone(),
        })
        .await?;
        match &decision {
            ShortfallDecision::Grant { grant, by } => {
                self.note_permission(
                    "granted",
                    &contract_id,
                    worker.as_str(),
                    format!(
                        "granted {} to contract {contract_id} ({by})",
                        grant.describe()
                    ),
                )
                .await?
            }
            ShortfallDecision::AskUser { question, .. } => {
                self.note_permission(
                    "asked",
                    &contract_id,
                    worker.as_str(),
                    format!("contract {contract_id}: {question}"),
                )
                .await?
            }
            _ => {}
        }
        self.submit_report(
            &contract,
            Some(&worker),
            ReportKind::NeedsInput,
            &format!("{} shortfall: {need}: {resolution}", kind.as_str()),
        );
        self.save()?;
        // The answer goes first; the cancel reaches the worker after its
        // tool call returns, so the transcript shows the resolution.
        if let Some(flight) = self.in_flight.get(&contract_id)
            && let Some(n) = flight.cancels.get(&worker)
        {
            n.notify_one();
        }
        Ok(ContractReply::Shortfall(resolution))
    }

    /// Answer a report without a decision: nothing is granted, amended or
    /// held, and the worker keeps working (`invalid`: told, as for malformed
    /// input, to name exactly one subject). Bounded: once the worker has been
    /// answered `MAX_ANSWERED_SHORTFALLS` times, its next such report stops
    /// it, and its attempt is stuck (`take_shortfall_stuck`).
    #[allow(clippy::too_many_arguments)]
    async fn answer_shortfall(
        &mut self,
        contract_id: &str,
        worker: &AgentId,
        kind: ShortfallKind,
        need: &str,
        why: &str,
        answer: String,
        invalid: bool,
    ) -> Result<ContractReply, OrchestratorError> {
        let Some(contract) = self.record.contract(contract_id).cloned() else {
            return Ok(ContractReply::Refused(format!(
                "contract `{contract_id}` is not open"
            )));
        };
        let answered = contract
            .shortfall_answers
            .iter()
            .filter(|w| *w == worker)
            .count();
        let stuck = answered >= MAX_ANSWERED_SHORTFALLS;
        let resolution = if stuck {
            format!(
                "stopped: the worker was answered {answered} times that nothing refused what it \
                 reported, and reported again; the attempt ends as stuck. Last answer: {answer}"
            )
        } else {
            format!("answered: {answer}")
        };
        let resolution = kgraf_secrets::scrub_persist_owned(resolution);
        if let Some(c) = self.record.contract_mut(contract_id) {
            if stuck {
                c.shortfall_stuck.push(worker.clone());
            } else {
                c.shortfall_answers.push(worker.clone());
            }
        }
        self.trace(TraceEvent::Shortfall {
            at_ms: self.now(),
            contract: contract_id.to_owned(),
            worker: worker.to_string(),
            kind: kind.as_str().to_owned(),
            need: need.to_owned(),
            why: why.to_owned(),
            resolution: resolution.clone(),
        })
        .await?;
        if stuck {
            self.submit_report(
                &contract,
                Some(worker),
                ReportKind::Stuck,
                &format!("{} shortfall: {need}: {resolution}", kind.as_str()),
            );
        }
        self.save()?;
        if !stuck {
            return Ok(if invalid {
                ContractReply::ShortfallInvalid(answer)
            } else {
                ContractReply::ShortfallAnswered(answer)
            });
        }
        if let Some(flight) = self.in_flight.get(contract_id)
            && let Some(n) = flight.cancels.get(worker)
        {
            n.notify_one();
        }
        Ok(ContractReply::Shortfall(resolution))
    }

    /// Whether `worker`'s attempt was stopped for reporting shortfalls past
    /// the bound. If it was, the attempt is marked an error (counted a
    /// failure, as a stalled attempt is) and the flight renews its workers.
    pub(super) fn take_shortfall_stuck(
        &mut self,
        contract: &str,
        worker: &AgentId,
        flight: &mut InFlight,
    ) -> bool {
        let Some(c) = self.record.contract_mut(contract) else {
            return false;
        };
        c.shortfall_answers.retain(|w| w != worker);
        let Some(at) = c.shortfall_stuck.iter().position(|w| w == worker) else {
            return false;
        };
        c.shortfall_stuck.remove(at);
        if let Some(a) = flight.attempts.iter_mut().find(|a| a.worker == *worker) {
            a.status = AttemptStatus::Error;
        }
        flight.renew_workers = true;
        true
    }

    /// Whether `worker`'s attempt ended in a shortfall report. If it did, the
    /// attempt is marked stopped and the flight renews its workers: the next
    /// attempt is a fresh worker, whose environment carries any grant.
    pub(super) fn take_shortfall(
        &mut self,
        contract: &str,
        worker: &AgentId,
        flight: &mut InFlight,
    ) -> bool {
        let Some(c) = self.record.contract_mut(contract) else {
            return false;
        };
        let Some(at) = c.shortfall_workers.iter().position(|w| w == worker) else {
            // Its answers are dropped by `take_shortfall_stuck`, which runs
            // next for an attempt that did not stop on a report.
            return false;
        };
        c.shortfall_workers.remove(at);
        c.shortfall_answers.retain(|w| w != worker);
        if let Some(a) = flight.attempts.iter_mut().find(|a| a.worker == *worker) {
            a.status = AttemptStatus::Stopped;
        }
        flight.renew_workers = true;
        true
    }
}
