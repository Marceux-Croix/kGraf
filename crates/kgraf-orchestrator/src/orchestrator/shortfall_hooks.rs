//! The run loop's side of a shortfall report (decisions 0093 and 0094; see
//! `crate::shortfall` for the rules).
//!
//! 1. `on_shortfall`: the report arrives while the worker runs. A permission
//!    report that its worker's refusals do not ground, or that asks for a
//!    path needing no grant or owned by another contract, is answered and the
//!    worker continues (`answer_shortfall`, bounded), unless the path is one
//!    the contract's cargo acceptance cannot pass without and only a later
//!    contract writes it: that holds for a re-plan (`member_hold`). Otherwise the
//!    orchestrator decides at once under the user's permission mode (grant,
//!    amend, ask the user, or hold), records the decision on the contract and
//!    in the trace, answers the worker, then cancels the attempt so the
//!    worker stops rather than guesses.
//! 2. `take_shortfall`: when that worker finishes, its attempt is marked
//!    stopped (not a candidate, not a failure) and its worker id retired, so
//!    the next attempt is a fresh worker built with the grant.
//!    `take_shortfall_stuck`: a worker stopped for reporting past the bound
//!    of answered reports finishes as an error, counted a failure.
//!    `verify_before_hold`: an attempt stopped on a held shortfall that
//!    changed files in its scope is verified first (amended 2026-10-02);
//!    `settle_held_verify` merges it on a pass and keeps the hold otherwise.
//! 3. `finalize_ordinary` applies a held decision as the contract's held
//!    reason, once every attempt has finished. A request waiting for the
//!    user holds the contract until it is answered (`permission_hooks`).

use super::*;
use crate::challenge::ContractReply;
use crate::shortfall::{
    Grounding, MAX_ANSWERED_SHORTFALLS, RequestScope, ShortfallDecision, ShortfallKind,
    decide_in_plan, instructions_answer, package_fetch_hold, path_answer, skill_hold_reason,
    stored,
};

/// The longest part of a failed check quoted in a shortfall's hold, in
/// characters: enough for the first error line, short enough for a status
/// line.
const MAX_HELD_CHECK_CHARS: usize = 400;

impl RunLoop {
    /// The plan-shape hold for a worker of `contract` whose report names a
    /// file its cargo acceptance cannot pass without, when only a contract
    /// that runs later writes it ([`crate::plan_repo::member_hold`]).
    /// `text` is the reported path, or an instructions report's words.
    fn member_hold(&self, contract: &str, text: &str) -> Option<String> {
        let root = self.s.workspaces.repo_root()?;
        let named: Vec<&str> = text
            .split(|c: char| {
                c.is_whitespace() || matches!(c, '`' | '\'' | '"' | '(' | ')' | ',' | ';')
            })
            .map(|t| t.trim_end_matches(['.', ':']))
            .filter(|t| !t.is_empty())
            .collect();
        crate::plan_repo::member_hold(
            &self.record.open_contracts,
            contract,
            &named,
            &crate::plan_repo::DiskRepo(root),
        )
    }

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
        // Decision 0030, amended 2026-10-06: a file the contract's cargo
        // acceptance cannot pass without, written only by a contract that
        // runs later, holds the contract for a re-plan. Answering "do not
        // edit it" would leave the worker at a check that cannot pass.
        let mut plan_shape_hold: Option<String> = None;
        // The other programs a refused command runs (decision 0094, amended
        // 2026-10-06), granted with the one the report names.
        let mut with: Vec<String> = Vec::new();
        let (need, why) = match (kind, grounding) {
            (ShortfallKind::Instructions, _) => {
                plan_shape_hold = self.member_hold(&contract_id, &format!("{asked} {why}"));
                // Decision 0093, amended 2026-10-01: a report that asks for a
                // change to a file another contract owns is answered, not
                // written into this contract's own instruction, where it
                // would change nothing.
                let answer = plan_shape_hold
                    .is_none()
                    .then(|| {
                        instructions_answer(&asked, &why, &contract, &self.record.open_contracts)
                    })
                    .flatten();
                if let Some(answer) = answer {
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
            (
                ShortfallKind::Permission,
                Some(Grounding::Refused {
                    subject,
                    reason,
                    with: others,
                }),
            ) => {
                with = others;
                if scope == RequestScope::File {
                    plan_shape_hold = self.member_hold(&contract_id, &subject);
                }
                // Decision 0087, amended 2026-10-09: a package download is
                // refused whatever is granted, so a grant would only start an
                // attempt the policy refuses again. Held with the refusal,
                // which says what allows the work.
                if kgraf_tools::execpolicy::is_package_fetch_refusal(&reason) {
                    plan_shape_hold = Some(package_fetch_hold(&subject, &reason));
                }
                let answer = (scope == RequestScope::File && plan_shape_hold.is_none())
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
            &with,
            &why,
            &contract,
            &self.authority(),
            Some(&self.record.open_contracts),
        );
        let decision = match plan_shape_hold {
            Some(reason) => ShortfallDecision::Hold { reason },
            None => decision,
        };
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

    /// Whether an attempt that stopped on a shortfall is verified before the
    /// shortfall's hold applies (decision 0093, amended 2026-10-02). Only a
    /// hold qualifies: a grant or an amendment re-dispatches, and a question
    /// waiting for the user is the user's to answer. The attempt must have
    /// changed files: an empty attempt has nothing to judge. Scope is not
    /// widened: the verifier refuses any change outside the declared writes,
    /// and the policy already refused the write the worker asked for.
    pub(super) fn verify_before_hold(&self, contract: &str, result: &WorkerResult) -> bool {
        self.record.contract(contract).is_some_and(|c| {
            c.shortfall_hold.is_some()
                && c.pending_permission.is_none()
                && !result.changed_paths.is_empty()
        })
    }

    /// The verdict on an attempt verified before its held shortfall applies.
    ///
    /// - A pass on a check that checks something: the verifier overrules the
    ///   claim. The hold is dropped, the attempt is a candidate as any
    ///   completed one is, and the report is kept as a fact (a notice and a
    ///   trace event), never silently.
    /// - A pass on a check that checks nothing (`true`, with no gate command)
    ///   overrules nothing: it is turned into a failure.
    /// - A failure: the attempt is stopped again (not counted a failure, its
    ///   worktree kept for a resume), and the hold names what the check said.
    pub(super) async fn settle_held_verify(
        &mut self,
        contract_id: &str,
        worker: &AgentId,
        verdict: Verdict,
        flight: &mut InFlight,
    ) -> Result<Verdict, OrchestratorError> {
        let gate_checked = self
            .record
            .contract(contract_id)
            .is_some_and(|c| !c.gate_commands.is_empty());
        let verdict = match verdict {
            Verdict::Pass { by, .. } if !gate_checked && crate::shortfall::vacuous_check(&by) => {
                Verdict::Fail {
                    reason: format!(
                        "the check `{}` checks nothing, so it cannot overrule the shortfall",
                        by.trim()
                    ),
                }
            }
            other => other,
        };
        match &verdict {
            Verdict::Pass { by, .. } => {
                let held = self
                    .record
                    .contract_mut(contract_id)
                    .and_then(|c| c.shortfall_hold.take())
                    .unwrap_or_default();
                let detail = kgraf_secrets::scrub_persist_owned(format!(
                    "contract {contract_id}: verified in its own scope after a shortfall that \
                     would have held it: its in-scope result passed `{}`, so the shortfall did \
                     not block the contract. Nothing outside its write scope was granted or \
                     written. The shortfall, kept as a fact: {held}",
                    by.trim()
                ));
                self.notices.insert(detail.clone());
                let at = self.now();
                self.trace(TraceEvent::Notice {
                    at_ms: at,
                    contract: contract_id.to_owned(),
                    worker: worker.to_string(),
                    detail,
                })
                .await?;
            }
            Verdict::Fail { reason } => {
                if let Some(a) = flight.attempts.iter_mut().find(|a| a.worker == *worker) {
                    a.status = AttemptStatus::Stopped;
                }
                let checked: String = reason.chars().take(MAX_HELD_CHECK_CHARS).collect();
                if let Some(c) = self.record.contract_mut(contract_id)
                    && let Some(hold) = c.shortfall_hold.as_mut()
                {
                    *hold = kgraf_secrets::scrub_persist_owned(format!(
                        "{hold}. Its in-scope result was checked first and failed: {checked}"
                    ));
                }
            }
        }
        self.save()?;
        Ok(verdict)
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
