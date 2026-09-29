//! The loop's side of speculative merge (decision 0074): when a round runs,
//! off the loop, and what its result becomes. The mechanism is in
//! `speculation.rs`.
//!
//! One round at a time, at most once per `OrchestratorConfig::speculate_every`,
//! only while two or more contracts have a worktree in flight, over at most
//! `speculate_max_contracts` of them. Like a gate (0019), a round takes one
//! work-item slot for its validation runs and is skipped, with a trace, when
//! none is free. A round never holds the loop or a merge-back: it reports by
//! message. Its cost is kept in the run record and traced.

use super::*;
use crate::speculation::{self, Round, SpeculativeConflict, ValidationCache};

/// Validated trees kept between rounds before the cache starts over. A tree
/// changes whenever a worker writes, so old entries stop matching; this only
/// bounds memory over a long run.
const CACHE_LIMIT: usize = 256;

/// Loop-only speculation state: nothing here needs to survive a restart.
#[derive(Default)]
pub(super) struct SpecRuntime {
    busy: bool,
    next_ms: i64,
    cache: ValidationCache,
    /// Refusals already traced, so a persistent one is one record.
    refusals: BTreeSet<String>,
    unvalidated_said: bool,
    /// The work-item slot the running round holds.
    slot: Option<Admitted>,
    /// A capacity skip was traced and no round has run since.
    skip_said: bool,
}

/// A round's result, back on the loop.
pub(super) struct SpecDone {
    /// The contracts the round was given.
    inputs: Vec<String>,
    cache: ValidationCache,
    result: Result<Option<Round>, String>,
}

impl RunLoop {
    /// Start a round when one is due. Called once per loop turn.
    pub(super) fn speculate_tick(&mut self) {
        let Some(every) = self.config.speculate_every else {
            return;
        };
        if self.spec.busy || self.stopping {
            return;
        }
        let now = self.now();
        if now < self.spec.next_ms {
            return;
        }
        // One worktree per contract: parallel attempts at one contract are
        // alternatives the verifier selects among (0009), never merged
        // together, so only different contracts are paired. A running or
        // finished-and-unlanded attempt is taken; a failed or stopped one's
        // worktree may already be released.
        let in_flight: Vec<(String, PathBuf)> = self
            .in_flight
            .iter()
            .filter_map(|(id, f)| {
                f.attempts
                    .iter()
                    .find(|a| {
                        matches!(a.status, AttemptStatus::Scattering | AttemptStatus::Success)
                            && !a.workspace.as_os_str().is_empty()
                    })
                    .map(|a| (id.clone(), a.workspace.clone()))
            })
            .collect();
        if in_flight.len() < 2 {
            return;
        }
        let mut in_flight = in_flight;
        in_flight.truncate(self.config.speculate_max_contracts.max(2));
        self.spec.next_ms = now + every.as_millis() as i64;
        let admitted = self.s.admission.gate().try_admit(1);
        let slot = match admitted {
            Ok(a) => a,
            Err(e) => {
                self.record.coordination.speculation.skipped += 1;
                if !self.spec.skip_said {
                    self.spec.skip_said = true;
                    self.queue(TraceEvent::Speculation {
                        at_ms: now,
                        contracts: in_flight.into_iter().map(|(c, _)| c).collect(),
                        outcome: "skipped".into(),
                        detail: format!("no work-item slot for its validation runs: {e}"),
                    });
                }
                return;
            }
        };
        self.spec.skip_said = false;
        self.spec.slot = Some(slot);
        self.spec.busy = true;
        let mut cache = std::mem::take(&mut self.spec.cache);
        if cache.len() > CACHE_LIMIT {
            cache.clear();
        }
        let workspaces = self.s.workspaces.clone();
        let runner = self.coord.gate_runner.clone();
        let tx = self.tx.clone();
        // Inner task per the same panic-supervisor pattern as the attempt and
        // verification spawns: a panic in a round must still reach the loop as
        // a `Speculated` message, or `busy` and the round's slot are held for
        // the rest of the run. The cache is lost with a panicked round (it
        // may be half updated) and starts over.
        tokio::spawn(async move {
            let inputs: Vec<String> = in_flight.iter().map(|(c, _)| c.clone()).collect();
            let round = tokio::spawn(async move {
                let result =
                    speculation::speculate(&*workspaces, runner.as_deref(), &in_flight, &mut cache)
                        .await;
                (result, cache)
            });
            let (result, cache) = match round.await {
                Ok(done) => done,
                Err(e) => {
                    let what = if e.is_panic() {
                        "panicked"
                    } else {
                        "ended abnormally"
                    };
                    (
                        Err(format!("the speculative round {what}: {e}")),
                        ValidationCache::default(),
                    )
                }
            };
            let _ = tx
                .send(LoopMsg::Speculated(Box::new(SpecDone {
                    inputs,
                    cache,
                    result,
                })))
                .await;
        });
    }

    /// Record and trace what a round found. Nothing is delayed or cancelled
    /// (0018, 0074); the pairing preference reads the record.
    pub(super) async fn on_speculated(&mut self, done: SpecDone) -> Result<(), OrchestratorError> {
        let SpecDone {
            inputs,
            cache,
            result,
        } = done;
        self.spec.busy = false;
        self.spec.cache = cache;
        self.release_speculation_slot();
        let at = self.now();
        let round = match result {
            Ok(Some(round)) => round,
            Ok(None) => return Ok(()),
            Err(e) => {
                self.record.coordination.speculation.refused += 1;
                // A contract that left flight while the round ran took its
                // worktree with it: the round's input vanished, which is not
                // a refusal. Anything else is said, once.
                let vanished = inputs.iter().any(|c| !self.in_flight.contains_key(c));
                if !vanished && self.spec.refusals.insert(e.clone()) {
                    self.trace(TraceEvent::Speculation {
                        at_ms: at,
                        contracts: inputs,
                        outcome: "refused".into(),
                        detail: e,
                    })
                    .await?;
                }
                return Ok(());
            }
        };
        if !round.validated && !self.spec.unvalidated_said {
            self.spec.unvalidated_said = true;
            self.trace(TraceEvent::Speculation {
                at_ms: at,
                contracts: inputs.clone(),
                outcome: "unvalidated".into(),
                detail: "the project has no validation commands, so speculative merge checks \
                         textual conflicts only; the semantic class is not checked"
                    .into(),
            })
            .await?;
        }
        let cost = &mut self.record.coordination.speculation;
        cost.rounds += 1;
        cost.validations += u64::from(round.validations);
        cost.validation_ms += round.validation_ms;
        let cost = *cost;
        self.trace(TraceEvent::Speculation {
            at_ms: at,
            contracts: inputs.clone(),
            outcome: "round".into(),
            detail: format!(
                "this round: {} validations, {} ms; this run: {} rounds, {} validations, \
                 {} ms, {} refused, {} skipped for capacity",
                round.validations,
                round.validation_ms,
                cost.rounds,
                cost.validations,
                cost.validation_ms,
                cost.refused,
                cost.skipped
            ),
        })
        .await?;
        for (a, b, kind, detail) in round.conflicts {
            let mut pair = [a, b];
            pair.sort();
            let known = self
                .record
                .coordination
                .speculative_conflicts
                .iter()
                .any(|c| c.contracts == pair && c.kind == kind);
            if known {
                continue;
            }
            // Worker and tool output: stored and shown, so scrubbed (0071).
            let detail = kgraf_secrets::scrub_persist_owned(detail);
            self.record
                .coordination
                .speculative_conflicts
                .push(SpeculativeConflict {
                    contracts: pair.clone(),
                    kind,
                    tip: round.tip.clone(),
                    detail: detail.clone(),
                    at_ms: at,
                });
            self.trace(TraceEvent::Speculation {
                at_ms: at,
                contracts: pair.to_vec(),
                outcome: kind.name().into(),
                detail,
            })
            .await?;
        }
        self.save()?;
        Ok(())
    }

    /// Give back the slot a round holds: when it reports, and at shutdown
    /// (a round still running when the run ends reports to nobody).
    pub(super) fn release_speculation_slot(&mut self) {
        if let Some(slot) = self.spec.slot.take() {
            self.s.admission.gate().release(slot);
        }
    }

    /// Whether speculative merge found `a` and `b` conflicting.
    pub(super) fn speculatively_conflicting(&self, a: &str, b: &str) -> bool {
        self.record
            .coordination
            .speculative_conflicts
            .iter()
            .any(|c| c.involves(a, b))
    }
}
