//! The run loop's side of a shortfall report (decisions 0093 and 0094; see
//! `crate::shortfall` for the rules).
//!
//! 1. `on_shortfall`: the report arrives while the worker runs. The
//!    orchestrator decides at once under the user's permission mode (grant,
//!    amend, ask the user, or hold), records the decision on the contract and
//!    in the trace, answers the worker, then cancels the attempt so the
//!    worker stops rather than guesses.
//! 2. `take_shortfall`: when that worker finishes, its attempt is marked
//!    stopped (not a candidate, not a failure) and its worker id retired, so
//!    the next attempt is a fresh worker built with the grant.
//! 3. `finalize_ordinary` applies a held decision as the contract's held
//!    reason, once every attempt has finished. A request waiting for the
//!    user holds the contract until it is answered (`permission_hooks`).

use super::*;
use crate::challenge::ContractReply;
use crate::shortfall::{RequestScope, ShortfallDecision, ShortfallKind, decide, stored};

impl RunLoop {
    pub(super) async fn on_shortfall(
        &mut self,
        worker: AgentId,
        kind: ShortfallKind,
        scope: RequestScope,
        need: String,
        why: String,
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
        if contract.shortfall_workers.contains(&worker) {
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
        let (need, why) = (stored(&need), stored(&why));
        let decision = decide(kind, scope, &need, &why, &contract, &self.authority());
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
            return false;
        };
        c.shortfall_workers.remove(at);
        if let Some(a) = flight.attempts.iter_mut().find(|a| a.worker == *worker) {
            a.status = AttemptStatus::Stopped;
        }
        flight.renew_workers = true;
        true
    }
}
