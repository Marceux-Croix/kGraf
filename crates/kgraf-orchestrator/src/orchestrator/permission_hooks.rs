//! The run loop's side of the user's permission mode (decision 0094).
//!
//! State machine of one permission request raised by a worker's shortfall:
//!
//! ```text
//!   report --mode grants--> granted (recorded) --> re-dispatched
//!   report --mode refuses (bound, useless grant, tool)--> held
//!   report --mode would ask, attended--> waiting (contract held, request on the desk)
//!   report --mode would ask, nobody attends--> denied (held, with how to allow it)
//!   waiting --user grants--> granted --> re-dispatched
//!   waiting --user denies--> held ("denied by the user")
//!   waiting --mode change grants--> granted --> re-dispatched
//!   waiting --mode change refuses--> held (the orchestrator's refusal)
//!   waiting --mode change still asks--> waiting
//! ```
//!
//! Every loop turn also drains the control's events (mode changes, grants a
//! mode made on a worker's ask) into the trace and the graph's permission
//! events, which the activity card shows.

use super::*;
use crate::review_desk::{PermissionRequestView, ReviewDesk};
use crate::run_state::PendingPermission;
use crate::shortfall::{
    Authority, Grant, GrantRecord, RequestScope, ShortfallDecision, ShortfallKind, decide,
};
use kgraf_tools::permissions::PermissionControl;

/// What an [`Orchestrator`] is given: the live control, the desk the box
/// reads, and whether a person attends the run.
pub(crate) struct PermissionParts {
    pub(crate) control: Arc<PermissionControl>,
    pub(crate) desk: Arc<ReviewDesk>,
    pub(crate) attended: bool,
}

impl Default for PermissionParts {
    /// Manual, no rules: a run given no control asks for everything, and
    /// with nobody attending denies it.
    fn default() -> Self {
        Self {
            control: Arc::new(PermissionControl::manual()),
            desk: Arc::new(ReviewDesk::default()),
            attended: false,
        }
    }
}

/// The loop's permission state.
pub(super) struct PermissionRuntime {
    pub(super) control: Arc<PermissionControl>,
    pub(super) desk: Arc<ReviewDesk>,
    pub(super) attended: bool,
    /// The control's events already recorded by this run.
    cursor: u64,
    /// The control's change generation this run has acted on.
    seen: tokio::sync::watch::Receiver<u64>,
    /// The last request id given out.
    next_id: u64,
}

impl PermissionRuntime {
    pub(super) fn new(parts: PermissionParts) -> Self {
        let mut seen = parts.control.subscribe();
        seen.borrow_and_update();
        Self {
            cursor: parts.control.last_seq(),
            seen,
            next_id: 0,
            control: parts.control,
            desk: parts.desk,
            attended: parts.attended,
        }
    }

    /// Resolves when the mode (or the rules, or plan review) changes, or when
    /// the user answers on the desk. Owns what it waits on, so the loop can
    /// wait on it beside its message queue.
    pub(super) fn waiter(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut changes = self.seen.clone();
        let answered = self.desk.answered();
        async move {
            tokio::select! {
                changed = changes.changed() => {
                    if changed.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
                () = answered => {}
            }
        }
    }
}

impl RunLoop {
    /// The user's current mode, allow set and rules, and whether a person
    /// attends.
    pub(super) fn authority(&self) -> Authority {
        let s = self.perms.control.settings();
        Authority {
            mode: s.mode,
            allow: s.allow,
            rules: s.rules,
            attended: self.perms.attended,
        }
    }

    /// Record the mode this run starts under, and put back on the desk any
    /// request a resumed record still has waiting (or deny it, with nobody
    /// attending, or grant it, if the mode now grants it).
    pub(super) async fn open_permissions(&mut self) -> Result<(), OrchestratorError> {
        let mode = self.perms.control.mode();
        self.note_permission(
            "mode_at_start",
            "",
            "",
            format!("the run starts in the {} permission mode", mode.label()),
        )
        .await?;
        self.recheck_pending().await
    }

    /// One loop turn's permission work: record the control's new events,
    /// re-route waiting requests after a change, and apply the user's answers.
    pub(super) async fn pump_permissions(&mut self) -> Result<(), OrchestratorError> {
        for (seq, event) in self.perms.control.events_since(self.perms.cursor) {
            self.perms.cursor = seq;
            let (name, worker) = match &event {
                kgraf_tools::PermissionEvent::ModeChanged { .. } => ("mode_changed", String::new()),
                kgraf_tools::PermissionEvent::PlanReviewChanged { .. } => {
                    ("plan_review_changed", String::new())
                }
                kgraf_tools::PermissionEvent::Granted { worker, .. } => ("granted", worker.clone()),
                kgraf_tools::PermissionEvent::ProtectedWrite { worker, .. } => {
                    ("protected_write", worker.clone())
                }
            };
            // A worker's ask granted by the mode belongs to this run only if
            // the worker is one of its own.
            let ours = crate::plan::sanitize_id(&format!("{}-", self.record.run.as_str()));
            if !worker.is_empty() && !worker.starts_with(&ours) {
                continue;
            }
            self.note_permission(name, "", &worker, event.line())
                .await?;
        }
        if self.perms.seen.has_changed().unwrap_or(false) {
            self.perms.seen.borrow_and_update();
            self.recheck_pending().await?;
        }
        for (id, grant) in self.perms.desk.take_answers() {
            self.on_permission_answer(id, grant).await?;
        }
        Ok(())
    }

    /// Whether a request waits for a person who is there: the run does not
    /// end on it.
    pub(super) fn permission_wait_keeps_run(&self) -> bool {
        self.perms.attended
            && self
                .record
                .open_contracts
                .iter()
                .any(|c| c.pending_permission.is_some())
    }

    /// A new id for a request: above every id the record holds, and never
    /// reused in this process, so a late answer to a settled request cannot
    /// land on a new one.
    fn next_request_id(&mut self) -> u64 {
        let persisted = self
            .record
            .open_contracts
            .iter()
            .filter_map(|c| c.pending_permission.as_ref().map(|p| p.id))
            .max()
            .unwrap_or(0);
        self.perms.next_id = self.perms.next_id.max(persisted) + 1;
        self.perms.next_id
    }

    /// Trace a permission event and record it in the graph, where the
    /// activity card shows it. A graph write that fails is reported with the
    /// run, never hidden, and does not stop the run.
    pub(super) async fn note_permission(
        &mut self,
        event: &str,
        contract: &str,
        worker: &str,
        line: String,
    ) -> Result<(), OrchestratorError> {
        let at_ms = self.now();
        let line = kgraf_secrets::scrub_persist_owned(line);
        self.trace(TraceEvent::Permission {
            at_ms,
            contract: contract.to_owned(),
            worker: worker.to_owned(),
            what: event.to_owned(),
            detail: line.clone(),
        })
        .await?;
        let row = kgraf_graph::PermissionEventRow {
            at_ms,
            run_id: Some(self.record.run.to_string()),
            agent: (!worker.is_empty()).then(|| worker.to_owned()),
            event: event.to_owned(),
            line,
        };
        let graph = self.s.graph.clone();
        let written = blocking(move || {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            Ok::<_, OrchestratorError>(g.record_permission_event(&row)?)
        });
        if let Err(e) = written {
            self.notices.insert(format!(
                "a permission event could not be recorded in the graph: {e}"
            ));
        }
        Ok(())
    }

    /// Apply a grant to a contract: a program its shell allows from now on,
    /// or a path added to its write scope (leased at the next dispatch, and
    /// accepted by the verifier's scope check).
    pub(super) fn apply_grant(
        c: &mut OpenContract,
        grant: &Grant,
        by: &str,
        worker: &str,
        why: &str,
        at_ms: i64,
    ) {
        match grant {
            Grant::Program(p) => c.granted_programs.push(p.clone()),
            Grant::Path(p) => {
                if !c.writes.iter().any(|w| w.path == *p) {
                    c.writes.push(WriteTarget {
                        path: p.clone(),
                        node: None,
                    });
                }
            }
        }
        c.grants.push(GrantRecord {
            program: grant.subject().to_owned(),
            scope: grant.scope(),
            worker: worker.to_owned(),
            why: why.to_owned(),
            at_ms,
            by: by.to_owned(),
        });
        c.shortfall_redispatches += 1;
    }

    /// A program grant takes effect only through a per-attempt environment
    /// builder (decision 0093); without one it would be silently dropped,
    /// so it is refused instead.
    pub(super) fn grant_unappliable(&self, grant: &Grant) -> Option<String> {
        match grant {
            Grant::Program(p) if self.attempt_envs.is_none() => Some(format!(
                "`{p}` would be granted, but this run builds no per-attempt environment that \
                 could apply a grant"
            )),
            _ => None,
        }
    }

    /// Put a waiting request on the contract and the desk.
    pub(super) fn raise_request(
        &mut self,
        contract: &str,
        grant: &Grant,
        worker: &str,
        need: &str,
        why: &str,
        question: &str,
    ) -> PendingPermission {
        let id = self.next_request_id();
        let request = PendingPermission {
            id,
            scope: grant.scope(),
            subject: grant.subject().to_owned(),
            worker: worker.to_owned(),
            need: need.to_owned(),
            why: why.to_owned(),
            question: question.to_owned(),
            raised_at_ms: self.now(),
        };
        self.perms
            .desk
            .show_request(self.view_of(contract, &request));
        request
    }

    fn view_of(&self, contract: &str, p: &PendingPermission) -> PermissionRequestView {
        PermissionRequestView {
            id: p.id,
            run: self.record.run.to_string(),
            contract: contract.to_owned(),
            worker: p.worker.clone(),
            scope: p.scope,
            subject: p.subject.clone(),
            why: p.why.clone(),
            question: p.question.clone(),
            raised_at_ms: p.raised_at_ms,
        }
    }

    /// Settle a waiting request: granted (the contract is released for a
    /// fresh attempt) or denied (it stays held, saying why).
    async fn settle_request(
        &mut self,
        contract: &str,
        outcome: Result<(Grant, String), String>,
    ) -> Result<(), OrchestratorError> {
        let now = self.now();
        let Some(c) = self.record.contract_mut(contract) else {
            return Ok(());
        };
        let Some(p) = c.pending_permission.take() else {
            return Ok(());
        };
        let still_running = c.shortfall_hold.is_some();
        let (event, line) = match &outcome {
            Ok((grant, by)) => {
                Self::apply_grant(c, grant, by, &p.worker, &p.why, now);
                if still_running {
                    // The reporting attempt is still stopping: it finishes as
                    // `Planned`, and the next dispatch carries the grant.
                    c.shortfall_hold = None;
                } else if matches!(c.status, ContractStatus::Held { .. }) {
                    c.status = ContractStatus::Planned;
                }
                (
                    "granted",
                    format!(
                        "granted {} to contract {contract} ({by}); asked by {}",
                        grant.describe(),
                        p.worker
                    ),
                )
            }
            Err(reason) => {
                let reason = kgraf_secrets::scrub_persist_owned(reason.clone());
                if still_running {
                    c.shortfall_hold = Some(reason.clone());
                } else {
                    c.status = ContractStatus::Held {
                        reason: reason.clone(),
                    };
                }
                ("denied", format!("contract {contract}: {reason}"))
            }
        };
        self.perms.desk.drop_request(p.id);
        self.note_permission(event, contract, &p.worker, line)
            .await?;
        self.save()
    }

    /// The user answered request `id` in the box.
    async fn on_permission_answer(
        &mut self,
        id: u64,
        grant: bool,
    ) -> Result<(), OrchestratorError> {
        let Some((contract, p)) = self.record.open_contracts.iter().find_map(|c| {
            c.pending_permission
                .as_ref()
                .filter(|p| p.id == id)
                .map(|p| (c.contract.clone(), p.clone()))
        }) else {
            return Ok(());
        };
        let wanted = match p.scope {
            RequestScope::File => Grant::Path(p.subject.clone()),
            _ => Grant::Program(p.subject.clone()),
        };
        let outcome = if !grant {
            Err(format!("denied by the user: {}", p.question))
        } else if let Some(why) = self.grant_unappliable(&wanted) {
            Err(why)
        } else {
            Ok((wanted, "user".to_owned()))
        };
        self.settle_request(&contract, outcome).await
    }

    /// Re-route every waiting request under the current mode: a mode that
    /// grants it releases it, one that refuses it holds it, and one that
    /// still asks leaves it on the desk. Also re-shows what a resumed record
    /// has waiting.
    pub(super) async fn recheck_pending(&mut self) -> Result<(), OrchestratorError> {
        let authority = self.authority();
        let waiting: Vec<(String, PendingPermission, OpenContract)> = self
            .record
            .open_contracts
            .iter()
            .filter_map(|c| {
                c.pending_permission
                    .clone()
                    .map(|p| (c.contract.clone(), p, c.clone()))
            })
            .collect();
        for (contract, p, snapshot) in waiting {
            let decision = decide(
                ShortfallKind::Permission,
                p.scope,
                &p.need,
                &p.why,
                &snapshot,
                &authority,
            );
            match decision {
                ShortfallDecision::Grant { grant, by } => {
                    let outcome = match self.grant_unappliable(&grant) {
                        Some(why) => Err(why),
                        None => Ok((grant, by)),
                    };
                    self.settle_request(&contract, outcome).await?;
                }
                ShortfallDecision::Hold { reason } => {
                    self.settle_request(&contract, Err(reason)).await?;
                }
                ShortfallDecision::AskUser { .. } | ShortfallDecision::Amend => {
                    let view = self.view_of(&contract, &p);
                    self.perms.desk.show_request(view);
                }
            }
        }
        Ok(())
    }
}
