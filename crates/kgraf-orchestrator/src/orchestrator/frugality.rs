//! The loop's side of long-running support (decision 0056): workflow
//! templates offered and mined, induced-demand records, and live occupancy.
//!
//! A child module of `orchestrator.rs`, like `coord.rs`, so the loop carries only
//! one-line hooks (other streams edit that file in parallel). The policies are in
//! `templates.rs`, `demand.rs` and `occupancy.rs`.
//!
//! **State between restarts** lives in the graph's `meta` table under
//! `frugality/<what>/<run>`: the templates offered (which arm of the demand
//! comparison the run is in, and the structures reuse is matched against) and the
//! plan snapshot. Both are deleted once a run completes and its demand record
//! holds what they said; a stopped, blocked or unplanned run keeps them, since
//! it can be resumed. The run-state record's schema is left unchanged.
//!
//! **Counts come from the ledger** (the ledger is the source for calls):
//! decomposition calls are the run's own calls of kind `decompose`, failed ones
//! included, and work units are the distinct workers that made at least one call
//! under the run. One ledger query at close, no trace re-read.

use super::*;
use crate::demand::DemandRecord;
use crate::gates::GateStatus;
use crate::occupancy::OccupancyBoard;
use crate::templates::{self, Offered, TemplateError};

/// Loop-only state for templates and occupancy.
pub(super) struct FrugalRuntime {
    pub(super) board: Arc<OccupancyBoard>,
    /// Lines for the run report (`RunReport::warnings`), for example a
    /// quarantined template.
    pub(super) warnings: Vec<String>,
}

impl FrugalRuntime {
    pub(super) fn new() -> Self {
        Self {
            board: Arc::new(OccupancyBoard::default()),
            warnings: Vec::new(),
        }
    }
}

/// Occupancy at or above this percent of the stop point is flagged in the status
/// text as near the stop point: the worker is about to hand back.
pub(super) const NEAR_STOP_PERCENT: u64 = 90;

fn offered_key(run: &str) -> String {
    format!("frugality/templates/{run}")
}

fn plan_key(run: &str) -> String {
    format!("frugality/plan/{run}")
}

impl From<TemplateError> for OrchestratorError {
    fn from(e: TemplateError) -> Self {
        OrchestratorError::Frugality(e.to_string())
    }
}

impl From<crate::demand::DemandError> for OrchestratorError {
    fn from(e: crate::demand::DemandError) -> Self {
        OrchestratorError::Frugality(e.to_string())
    }
}

impl RunLoop {
    fn meta_json<T: serde::de::DeserializeOwned + Default>(
        &self,
        key: &str,
    ) -> Result<T, OrchestratorError> {
        let graph = self.s.graph.clone();
        let text = blocking(|| {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            Ok::<_, OrchestratorError>(g.meta(key)?)
        })?;
        match text {
            None => Ok(T::default()),
            Some(t) => serde_json::from_str(&t).map_err(|e| {
                OrchestratorError::Frugality(format!("meta `{key}` is unreadable: {e}"))
            }),
        }
    }

    fn set_meta_json<T: serde::Serialize>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<(), OrchestratorError> {
        let text = serde_json::to_string(value)
            .map_err(|e| OrchestratorError::Frugality(format!("meta `{key}`: {e}")))?;
        let graph = self.s.graph.clone();
        blocking(|| {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            g.set_meta(key, &text)?;
            Ok(())
        })
    }

    fn clear_meta_keys(&self, keys: &[String]) -> Result<(), OrchestratorError> {
        let graph = self.s.graph.clone();
        blocking(|| {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            for k in keys {
                g.clear_meta(k)?;
            }
            Ok(())
        })
    }

    /// Hook in `plan()`: the templates offered to the run's first decomposition,
    /// as data blocks. The offer is recorded (meta and trace) so the run's arm of
    /// the demand comparison is known even when it offers nothing.
    pub(super) async fn template_offers(&mut self) -> Result<Vec<String>, OrchestratorError> {
        let instruction = self.record.original_instruction.clone();
        let graph = self.s.graph.clone();
        let now = self.now();
        let found = blocking(|| {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            Ok::<_, OrchestratorError>(templates::offers(&g, &instruction, now)?)
        })?;
        for (id, reason) in &found.quarantined {
            let line = format!(
                "workflow template {id} quarantined: its stored shape is unreadable ({reason})"
            );
            self.trace(TraceEvent::Templates {
                at_ms: now,
                action: "quarantined".into(),
                templates: vec![id.clone()],
                detail: reason.clone(),
            })
            .await?;
            self.frugal.warnings.push(line);
        }
        let offers = found.offers;
        let offered: Vec<Offered> = offers
            .iter()
            .map(|o| Offered {
                id: o.template.id.clone(),
                structure: o.template.structure.clone(),
                step_terms: templates::step_terms(&o.template.steps)
                    .into_iter()
                    .collect(),
            })
            .collect();
        self.set_meta_json(&offered_key(&self.run_id()), &offered)?;
        let blocks = offers
            .iter()
            .map(templates::data_block)
            .collect::<Result<Vec<_>, _>>()?;
        let at = self.now();
        self.trace(TraceEvent::Templates {
            at_ms: at,
            action: "offered".into(),
            templates: offered.iter().map(|o| o.id.clone()).collect(),
            detail: offers
                .iter()
                .map(|o| format!("{} similarity {:.2}", o.template.id, o.score))
                .collect::<Vec<_>>()
                .join(", "),
        })
        .await?;
        Ok(blocks)
    }

    /// Hook at the end of `plan()`: snapshot the plan's abstracted steps. The loop
    /// removes contracts from the record as they verify, so a completed run's plan
    /// is read from here.
    pub(super) fn note_plan(&mut self) -> Result<(), OrchestratorError> {
        if self.run_lease_lost {
            return Ok(());
        }
        let steps = templates::steps_of(&self.record.open_contracts);
        self.set_meta_json(&plan_key(&self.run_id()), &steps)
    }

    /// Decomposition calls (failed ones included) and work units, from the ledger.
    fn ledger_counts(&self) -> Result<(Vec<CallId>, u64), OrchestratorError> {
        let run = self.run_id();
        let ledger = self.s.ledger.clone();
        let calls = blocking(|| ledger.calls_under_task(&run, None))?;
        let prefix = format!("{run}/");
        let decompositions = calls
            .iter()
            .filter(|c| c.task_id == run && c.trace.kind.as_deref() == Some("decompose"))
            .map(|c| c.id.clone())
            .collect();
        let workers: BTreeSet<&str> = calls
            .iter()
            .filter(|c| c.task_id.starts_with(&prefix))
            .map(|c| c.agent_id.as_str())
            .collect();
        Ok((decompositions, workers.len() as u64))
    }

    /// Settled spend of ledger calls, and how many have no known spend yet.
    /// Uses the ledger's own aggregate (`Ledger::spend_of`), one query.
    fn settled_spend(&self, calls: &[CallId]) -> Result<(Money, u64), OrchestratorError> {
        if calls.is_empty() {
            return Ok((Money::ZERO, 0));
        }
        let ledger = self.s.ledger.clone();
        let prices = &self.s.prices;
        let spend = blocking(|| ledger.spend_of(calls, prices))?;
        Ok((spend.total, spend.unknown.len() as u64))
    }

    /// Hook at the end of `run()` and after a planning failure: mine or credit a
    /// template on a completed run, mark a reused template failed when its run is
    /// blocked, and write the run's demand record.
    pub(super) async fn close_frugality(
        &mut self,
        plan_failed: bool,
    ) -> Result<(), OrchestratorError> {
        if self.run_lease_lost {
            return Ok(());
        }
        let run = self.run_id();
        let now = self.now();
        let offered: Vec<Offered> = self.meta_json(&offered_key(&run))?;
        let steps: Vec<templates::TemplateStep> = if plan_failed {
            Vec::new()
        } else {
            self.meta_json(&plan_key(&run))?
        };
        let planned = steps.len() as u64;
        let completed = !plan_failed && matches!(self.record.status, RunStatus::Completed);
        // A completed run always had a plan. Without its snapshot (a run from
        // before snapshots, or one already folded into its record) there is
        // nothing true to write, so nothing is written.
        if completed && steps.is_empty() {
            return self
                .trace(TraceEvent::Templates {
                    at_ms: now,
                    action: "not_recorded".into(),
                    templates: Vec::new(),
                    detail: "the run completed without a plan snapshot".into(),
                })
                .await;
        }
        let reused = templates::reused(&offered, &steps).map(|o| o.id.clone());
        let status = if plan_failed {
            "plan_failed"
        } else {
            match &self.record.status {
                RunStatus::Running => "running",
                RunStatus::Completed => "completed",
                RunStatus::Blocked { .. } => "blocked",
                RunStatus::Stopped => "stopped",
            }
        };
        // A waived gate is the user's decision to proceed, not a verification:
        // such a run is neither mined nor credited to a template it reused.
        let waived = self
            .record
            .coordination
            .gates
            .iter()
            .any(|g| matches!(g.status, GateStatus::Waived { .. }));

        let graph = self.s.graph.clone();
        if completed && !waived && !steps.is_empty() {
            let (action, id) = match &reused {
                Some(id) => {
                    blocking(|| {
                        let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
                        templates::record_reuse_success(&g, id, &run, now)?;
                        Ok::<_, OrchestratorError>(())
                    })?;
                    ("reused", id.clone())
                }
                None => {
                    let t = templates::from_steps(&self.record.original_instruction, steps)?;
                    blocking(|| {
                        let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
                        templates::record_success(&g, &t, &run, now)?;
                        Ok::<_, OrchestratorError>(())
                    })?;
                    ("mined", t.id)
                }
            };
            self.trace(TraceEvent::Templates {
                at_ms: now,
                action: action.into(),
                templates: vec![id],
                detail: format!("{planned} steps"),
            })
            .await?;
        } else if completed && waived {
            self.trace(TraceEvent::Templates {
                at_ms: now,
                action: "not_mined".into(),
                templates: Vec::new(),
                detail: "a gate was waived, not passed".into(),
            })
            .await?;
        }
        if matches!(self.record.status, RunStatus::Blocked { .. })
            && let Some(id) = &reused
        {
            blocking(|| {
                let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
                templates::record_failure(&g, id, &run, now)?;
                Ok::<_, OrchestratorError>(())
            })?;
            self.trace(TraceEvent::Templates {
                at_ms: now,
                action: "failed_on_reuse".into(),
                templates: vec![id.clone()],
                detail: "the run that reused it is blocked".into(),
            })
            .await?;
        }

        let (decompositions, units_started) = self.ledger_counts()?;
        let (decomposition_spend, unknown) = self.settled_spend(&decompositions)?;
        let record = DemandRecord {
            run: run.clone(),
            status: status.into(),
            templates_offered: offered.iter().map(|o| o.id.clone()).collect(),
            template_matched: reused,
            contracts: planned,
            dispatches: self.record.counters.dispatches,
            units_started,
            verified: self.record.verified_contracts.len() as u64,
            decomposition_calls: decompositions.len() as u64,
            decomposition_spend_micros: decomposition_spend.micros(),
            spend_micros: self.s.admission.budget().spent_run(&run).micros(),
            unknown_spend_calls: unknown,
        };
        blocking(|| {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            record.store(&g, now)?;
            Ok::<_, OrchestratorError>(())
        })?;
        self.trace(TraceEvent::Demand {
            at_ms: now,
            record: Box::new(record),
        })
        .await?;
        // Folded into the record: a finished run's scratch state is removed.
        if completed {
            self.clear_meta_keys(&[offered_key(&run), plan_key(&run)])?;
        }
        Ok(())
    }

    /// Run the end-of-run analytics and report a failure, never raise it: the
    /// failure goes into the trace (`analytics_failed`) and back to the caller for
    /// the run report. If the trace cannot take it either, the returned
    /// text says so.
    pub(super) async fn analytics_outcome(&mut self) -> Option<String> {
        let error = self.close_frugality(false).await.err()?.to_string();
        let at = self.now();
        let traced = self
            .trace(TraceEvent::Templates {
                at_ms: at,
                action: "analytics_failed".into(),
                templates: Vec::new(),
                detail: error.clone(),
            })
            .await;
        Some(match traced {
            Ok(()) => error,
            Err(e) => format!("{error} (and the trace could not record it: {e})"),
        })
    }

    /// The status text's occupancy clause for `live`: every live worker's
    /// context as a percent of its stop point, `unknown` without a stop point,
    /// and the ones near it flagged. Built from the one snapshot the status
    /// reply also carries, so the text and the figures never disagree (the
    /// board changes under the loop as workers start and usage arrives).
    pub(super) fn occupancy_line(live: &[crate::occupancy::Occupancy]) -> String {
        let near: BTreeSet<&str> = live
            .iter()
            .filter(|o| o.percent_of_stop().is_some_and(|p| p >= NEAR_STOP_PERCENT))
            .map(|o| o.worker.as_str())
            .collect();
        if live.is_empty() {
            return String::new();
        }
        let parts: Vec<String> = live
            .iter()
            .map(|o| match o.percent_of_stop() {
                Some(p) if near.contains(o.worker.as_str()) => {
                    format!("{} {p}% of stop point (near stop)", o.worker)
                }
                Some(p) => format!("{} {p}% of stop point", o.worker),
                None => format!("{} unknown", o.worker),
            })
            .collect();
        format!("; context: {}", parts.join(", "))
    }
}
