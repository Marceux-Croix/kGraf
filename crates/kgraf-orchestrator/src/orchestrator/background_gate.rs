//! The loop's side of a background gate (decision 0100, section 6): a small
//! request's root gate runs on each verified tip of `kgraf/<run>`, one run at
//! a time, coalescing onto the newest tip, never preempting the run in
//! flight. The policy is in `gates.rs` ([`crate::gates::BackgroundRun`]);
//! `coord.rs` calls the hooks here at the points it starts and ends a gate run.
//!
//! A multi-contract plan's gates are not background gates: they run as
//! decision 0018 states, unchanged.

use super::*;
use crate::gates::{self, BackgroundStart};

impl RunLoop {
    /// Hook at the top of `dispatch_gates`: `ready` holds the gates ready as
    /// any gate is. A running background gate takes the newest tip into its
    /// queued slot; an idle one that is due on a fresh tip while a cover is
    /// still open joins `ready` as an information run. Returns the ready ids
    /// and those of them that start as information.
    pub(super) fn background_ready(
        &mut self,
        ready: Vec<String>,
    ) -> (Vec<String>, BTreeSet<String>) {
        if self.record.coordination.background_gates.is_empty() {
            return (ready, BTreeSet::new());
        }
        let tip = self.record.coordination.integration_tip.clone();
        let mut out = ready.clone();
        let mut info = BTreeSet::new();
        let ids: Vec<String> = self
            .record
            .coordination
            .background_gates
            .keys()
            .cloned()
            .collect();
        for id in ids {
            let Some(g) = self
                .record
                .coordination
                .gates
                .iter()
                .find(|g| g.id == id)
                .cloned()
            else {
                continue;
            };
            let Some(mut run) = self.record.coordination.background_gates.get(&id).cloned() else {
                continue;
            };
            if let Some(t) = tip.as_deref()
                && gates::background_tip(&g, &mut run, t)
            {
                let at = self.now();
                self.queue(TraceEvent::Gate {
                    at_ms: at,
                    duration_ms: None,
                    gate: id.clone(),
                    state: "coalesced".into(),
                    detail: format!(
                        "an older queued tip was dropped; the next run is on {t} \
                         ({} dropped so far)",
                        run.dropped
                    ),
                });
            }
            let record = &self.record;
            let due = gates::background_due(&g, &run, tip.as_deref(), ready.contains(&id), &|c| {
                record.is_verified(c)
            });
            if due == Some(BackgroundStart::Info) && !out.contains(&id) {
                out.push(id.clone());
                info.insert(id.clone());
            }
            self.record.coordination.background_gates.insert(id, run);
        }
        (out, info)
    }

    /// Hook where `dispatch_gates` marks a gate running on `pin`.
    pub(super) fn background_started(&mut self, gate: &str, pin: Option<String>, info: bool) {
        let merged: Vec<String> = self
            .record
            .coordination
            .integration
            .iter()
            .filter(|(_, i)| i.accepted())
            .map(|(c, _)| c.clone())
            .collect();
        if let Some(run) = self.record.coordination.background_gates.get_mut(gate) {
            run.merged_at_start = merged;
            let start = if info {
                BackgroundStart::Info
            } else {
                BackgroundStart::Full
            };
            gates::background_started(run, pin, start);
        }
    }

    /// Hook in `on_gate_finished`, before the verdict is recorded: a
    /// background run that does not judge the gate (it started as
    /// information, a cover is open, or a newer tip is queued) is taken as
    /// information here and `true` is returned; the caller records nothing
    /// more. A failure is shown either way.
    pub(super) async fn background_finished(
        &mut self,
        gate: &str,
        verdict: &Result<gates::GateVerdict, String>,
        pinned: Option<String>,
        duration_ms: Option<u64>,
    ) -> Result<bool, OrchestratorError> {
        let Some(mut run) = self.record.coordination.background_gates.get(gate).cloned() else {
            return Ok(false);
        };
        let open: Vec<String> = self
            .record
            .coordination
            .gates
            .iter()
            .find(|g| g.id == gate)
            .map(|g| {
                g.covers
                    .iter()
                    .filter(|c| !self.record.is_verified(c))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let passed = matches!(verdict, Ok(v) if v.passed);
        if passed {
            run.passed_on.clone_from(&pinned);
            self.record
                .coordination
                .background_gates
                .insert(gate.to_owned(), run.clone());
            self.clear_vacuous_marks(gate, &run.merged_at_start, pinned.as_deref())
                .await?;
        } else {
            self.show_background_failure(gate, verdict, pinned.as_deref(), &open)
                .await?;
        }
        if gates::background_judges(&run, !open.is_empty()) {
            return Ok(false);
        }
        let at = self.now();
        let why = match (&run.queued_tip, open.is_empty()) {
            (Some(t), _) => format!("a newer tip {t} is queued, so the gate runs again there"),
            (None, false) => format!("{} still open", open.join(", ")),
            (None, true) => "it started while a cover was open".to_owned(),
        };
        gates::background_information(
            &mut self.record.coordination.gates,
            &mut run,
            gate,
            passed,
            pinned.clone(),
            at,
        );
        self.record
            .coordination
            .background_gates
            .insert(gate.to_owned(), run);
        self.queue(TraceEvent::Gate {
            at_ms: at,
            duration_ms,
            gate: gate.to_owned(),
            state: "information".into(),
            detail: format!(
                "{} on {}, information only: {why}",
                if passed { "passed" } else { "failed" },
                pinned.as_deref().unwrap_or("the unversioned workspace")
            ),
        });
        Ok(true)
    }

    /// The gate's commands passed on a tip that holds `merged`: a contract
    /// left unverified because its narrowed check selected no test
    /// ([`crate::shortfall::SCOPED_CHECK_VACUOUS`]) is now judged by the
    /// whole suite, so its mark is cleared, with a notice.
    async fn clear_vacuous_marks(
        &mut self,
        gate: &str,
        merged: &[String],
        pinned: Option<&str>,
    ) -> Result<(), OrchestratorError> {
        let cleared: Vec<String> = merged
            .iter()
            .filter(|c| {
                self.record
                    .unverified
                    .get(*c)
                    .is_some_and(|r| r.starts_with(crate::shortfall::SCOPED_CHECK_VACUOUS))
            })
            .cloned()
            .collect();
        for c in &cleared {
            self.record.unverified.remove(c);
            let detail = format!(
                "contract {c}: no longer unverified: gate {gate} passed on {}, which holds its merge",
                pinned.unwrap_or("the unversioned workspace")
            );
            self.notices.insert(detail.clone());
            let at = self.now();
            self.trace(TraceEvent::Notice {
                at_ms: at,
                contract: c.clone(),
                worker: String::new(),
                detail,
            })
            .await?;
        }
        Ok(())
    }

    /// A background gate run failed: shown to the user as a notice and a
    /// trace notice, since the merge it judges has already happened and the
    /// user may have moved on.
    async fn show_background_failure(
        &mut self,
        gate: &str,
        verdict: &Result<gates::GateVerdict, String>,
        pinned: Option<&str>,
        open: &[String],
    ) -> Result<(), OrchestratorError> {
        let what = match verdict {
            Ok(v) => format!("`{}` failed", v.by),
            Err(e) => format!("it could not run: {e}"),
        };
        let then = if open.is_empty() {
            "a remediation follows as decision 0018 states, unless a newer tip is queued".to_owned()
        } else {
            format!(
                "information only while {} is open; no remediation starts",
                open.join(", ")
            )
        };
        let detail = kgraf_secrets::scrub_persist_owned(format!(
            "background gate {gate} failed on {} after the merge: {what}; {then}",
            pinned.unwrap_or("the unversioned workspace")
        ));
        self.notices.insert(detail.clone());
        let at = self.now();
        self.trace(TraceEvent::Notice {
            at_ms: at,
            contract: String::new(),
            worker: String::new(),
            detail,
        })
        .await
    }
}
