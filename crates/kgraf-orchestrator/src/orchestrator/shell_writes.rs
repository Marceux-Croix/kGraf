//! Files an attempt's shell commands wrote outside its contract's declared
//! writes (decisions 0083, 0093 and 0094).
//!
//! The write tools refuse a path outside the scope while the worker runs, and
//! the worker's shortfall report (or the one kGraf files for it) asks the
//! user's permission mode for it: a mode that grants a file-scope widening
//! adds the path to the contract's writes. A shell command is not refused per
//! path: the sandbox confines it to the worktree, and git says afterwards
//! what it changed. Without this module such a file failed the attempt at the
//! verifier's scope check in every mode, Bypass included, while the same file
//! written with the write tool would have been granted.
//!
//! The rule: when an attempt that finished changed paths outside its declared
//! writes, each path is put to the same decision a file-scope shortfall gets
//! ([`decide_in_plan`]), with the user's deny and ask rules, the paths no
//! worker may write, and the plan's owners all applying. A path another open
//! contract declares is never granted, in any mode (disjoint writes). Only
//! when every such path is granted are they added to the contract's writes,
//! each recorded as a grant by the mode, a `permission` trace event and a
//! notice; the attempt then goes on to verification and merge-back, which
//! hold it to the widened writes. Otherwise the attempt fails as before, and
//! its reason names, per path, why it was not granted.
//!
//! A repeat (`repeated_scope_hold`): an attempt that fails the scope check on
//! the same paths as the contract's last such failure holds the contract once
//! its round ends, since a fresh attempt with the same scope fails the same
//! way.

use super::*;
use crate::shortfall::{RequestScope, ShortfallDecision, ShortfallKind, decide_in_plan};

/// Starts the reason of an attempt that failed the verifier's scope check.
pub(super) const OUTSIDE_SCOPE_PREFIX: &str = "wrote outside the declared scope: ";

impl RunLoop {
    /// Put the paths `outside` that attempt `worker` of `contract_id` changed
    /// outside its declared writes to the user's permission mode. `Ok(())`:
    /// every one was granted and added to the contract's writes, and each
    /// grant is recorded. `Err(reason)`: none was granted; `reason` is the
    /// attempt's failure, naming each path and why it was not granted.
    pub(super) async fn grant_shell_writes(
        &mut self,
        contract_id: &str,
        worker: &AgentId,
        outside: &[String],
    ) -> Result<Result<(), String>, OrchestratorError> {
        let base = format!("{OUTSIDE_SCOPE_PREFIX}{outside:?}");
        let Some(contract) = self.record.contract(contract_id).cloned() else {
            return Ok(Err(base));
        };
        let authority = self.authority();
        // No fresh attempt follows a grant here: the attempt that wrote the
        // files is the one verified. So the shortfall re-dispatch bound,
        // which counts fresh attempts, does not apply.
        let mut asking = contract.clone();
        asking.shortfall_redispatches = 0;
        let why = "the attempt's shell commands wrote it, outside the contract's declared writes";
        let mut granted: Vec<(crate::shortfall::Grant, String)> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        let same = |declared: &str, path: &str| {
            kgraf_runtime::same_path(Path::new(declared), Path::new(path))
        };
        for path in outside {
            // An open contract that declares the path owns it; a closed one
            // (verified, superseded) already delivered it, and this attempt,
            // started from an earlier tip, would overwrite that delivery.
            let declared_by = self
                .record
                .open_contracts
                .iter()
                .find(|o| o.contract != contract_id && o.writes.iter().any(|w| same(&w.path, path)))
                .map(|o| o.contract.clone())
                .or_else(|| {
                    self.record
                        .coordination
                        .closed
                        .iter()
                        .find(|c| {
                            c.contract != contract_id && c.writes.iter().any(|w| same(w, path))
                        })
                        .map(|c| c.contract.clone())
                });
            if let Some(holder) = declared_by {
                refused.push(format!(
                    "`{path}` belongs to contract `{holder}` (disjoint writes: never granted in \
                     any permission mode)"
                ));
                continue;
            }
            match decide_in_plan(
                ShortfallKind::Permission,
                RequestScope::File,
                path,
                &[],
                why,
                &asking,
                &authority,
                Some(&self.record.open_contracts),
                &self.record.coordination.closed,
            ) {
                ShortfallDecision::Grant { grant, by } => granted.push((grant, by)),
                ShortfallDecision::AskUser { question, .. } => refused.push(format!(
                    "`{path}`: the {} permission mode would ask before a write tool wrote it, \
                     and a finished attempt cannot wait for the answer ({question}); grant the \
                     path with `--allow-path {path}`, choose a mode that grants it, or re-plan \
                     with it in the contract's writes",
                    authority.mode.label()
                )),
                ShortfallDecision::Hold { reason } => refused.push(format!("`{path}`: {reason}")),
                ShortfallDecision::Amend => refused.push(format!(
                    "`{path}`: a file write is not an instructions report (internal error)"
                )),
            }
        }
        if !refused.is_empty() {
            return Ok(Err(format!(
                "{base}. The {} permission mode does not grant {}: {}",
                authority.mode.label(),
                if refused.len() == 1 {
                    "this write"
                } else {
                    "these writes"
                },
                refused.join("; ")
            )));
        }
        let now = self.now();
        if let Some(c) = self.record.contract_mut(contract_id) {
            for (grant, by) in &granted {
                Self::record_grant(c, grant, by, worker.as_str(), why, now);
            }
        }
        for (grant, by) in &granted {
            self.note_permission(
                "granted",
                contract_id,
                worker.as_str(),
                format!(
                    "granted {} to contract {contract_id} ({by}): the attempt's shell commands \
                     wrote it outside the declared writes",
                    grant.describe()
                ),
            )
            .await?;
        }
        let detail = kgraf_secrets::scrub_persist_owned(format!(
            "contract {contract_id}: attempt {worker}'s shell commands wrote outside the \
             contract's declared writes ({}); the {} permission mode grants such writes to the \
             write tool and no other contract declares them, so they were added to the \
             contract's writes and the attempt goes on to verification",
            outside.join(", "),
            authority.mode.label()
        ));
        self.notices.insert(detail.clone());
        self.trace(TraceEvent::Notice {
            at_ms: now,
            contract: contract_id.to_owned(),
            worker: worker.to_string(),
            detail,
        })
        .await?;
        self.save()?;
        Ok(Ok(()))
    }

    /// Record that attempt `worker` of `contract_id` failed the scope check
    /// on the paths `outside`. When the contract's last such failure named
    /// the same paths, returns the held reason for the contract: a fresh
    /// attempt with the same scope fails the same way. The repeat is said in
    /// a notice and the trace.
    pub(super) async fn repeated_scope_hold(
        &mut self,
        contract_id: &str,
        worker: &AgentId,
        outside: &[String],
        reason: &str,
    ) -> Result<Option<String>, OrchestratorError> {
        let cause = scope_cause(outside);
        let Some(c) = self.record.contract_mut(contract_id) else {
            return Ok(None);
        };
        let repeated = c.scope_violation.as_deref() == Some(cause.as_str());
        c.scope_violation = Some(cause);
        if !repeated {
            return Ok(None);
        }
        let held = kgraf_secrets::scrub_persist_owned(format!(
            "attempt {worker} failed for the same cause as an earlier attempt: it wrote outside \
             the declared scope on the same paths ({}). A fresh attempt with the same scope \
             fails the same way, so it was not retried. Add the paths to the contract's writes \
             (re-plan), or choose a permission mode that grants them, then resume. The \
             failure: {reason}",
            outside.join(", ")
        ));
        let detail = format!("contract {contract_id}: held at once: {held}");
        self.notices.insert(detail.clone());
        let at = self.now();
        self.trace(TraceEvent::Notice {
            at_ms: at,
            contract: contract_id.to_owned(),
            worker: worker.to_string(),
            detail,
        })
        .await?;
        Ok(Some(held))
    }
}

/// The cause of a scope failure, compared between attempts: the paths
/// written outside the scope, sorted, one per line.
fn scope_cause(outside: &[String]) -> String {
    let mut paths: Vec<&str> = outside.iter().map(String::as_str).collect();
    paths.sort_unstable();
    paths.dedup();
    paths.join("\n")
}

#[cfg(test)]
mod tests {
    use super::scope_cause;

    #[test]
    fn a_scope_cause_ignores_order_and_repeats() {
        let a = scope_cause(&["b".into(), "a".into(), "a".into()]);
        assert_eq!(a, scope_cause(&["a".into(), "b".into()]));
        assert_ne!(a, scope_cause(&["a".into()]));
    }
}
