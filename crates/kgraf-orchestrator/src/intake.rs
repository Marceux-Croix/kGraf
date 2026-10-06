//! Starting a run from a corpus: the orchestrator's side of the
//! interpretation pass in `kgraf-interpret`.
//!
//! - [`ModelAsker`] asks the interpretation question through the orchestrator's
//!   ledgered [`ModelCaller`], trace kind `interpret`, so the call is attributed like
//!   decomposition. Its identity (`route:model`) is part of the plan id.
//! - [`interpret_corpus`] runs the pass with the call **reserved (accounting only;
//!   decision 0080 removed every spend cap) before it is made** and settled at its
//!   ledger cost, as every orchestrator call is. It needs no worktrees, branch or
//!   workers, so reading a corpus creates nothing in the repository. A brief this
//!   model answered before costs no call and no reservation.
//!   - A **failed** call is settled at its ledger cost (a failed row is free, an
//!     unreconciled one costs its hold) and returned as charged.
//!   - A call that **overran** its hold is settled and returned as charged, kept
//!     as normal: the overrun is an accounting fact (the estimate undercounted
//!     the call), never a reason to drop the answer.
//!   - A **cancelled** interpretation (the future dropped mid-call) releases its
//!     hold ([`HeldCall`]'s `Drop`); the ledger row it opened is charged at its worst
//!     case when the run opens, as every unreconciled call of a run is.
//! - [`Orchestrator::interpret`] runs it on the orchestrator's services and records
//!   the charged calls, so opening the run does not charge them a second time.
//! - [`Orchestrator::start_from_plan`] (in `orchestrator.rs`) takes only an
//!   `ApprovedPlan`, which only a plan-id-bound approval makes. [`seeded_record`]
//!   seeds the plan's work nodes and turns its items into the run's decomposition.

use crate::admission::{Admission, Overrun, Reservation};
use crate::clock::Clock;
use crate::orchestrator::{Orchestrator, OrchestratorError};
use crate::plan::{ContractSpec, ModelCaller, sanitize_plan, validate_plan};
use crate::run_state::{PlanProgress, RunStateRecord, WriteTarget};
use crate::sizing::{bound_tokens, dear_usage};
use crate::store::{SharedGraph, StoreError};
use kgraf_core::{CallId, Money, PriceTable, RunId};
use kgraf_interpret::{
    ApprovedPlan, AskAnswer, AskFailure, Asker, InterpretError, InterpretRequest, Interpretation,
    TestResults,
};
use kgraf_provider::{Ledger, Settlement};
use kgraf_runtime::blocking;
use std::collections::BTreeSet;

/// The interpretation question through the ledger.
pub struct ModelAsker {
    pub caller: ModelCaller,
    /// The route the caller's provider is, for the identity.
    pub route: String,
}

#[async_trait::async_trait]
impl Asker for ModelAsker {
    fn identity(&self) -> String {
        format!("{}:{}", self.route, self.caller.model)
    }

    async fn ask(&self, system: &str, user: String) -> Result<AskAnswer, AskFailure> {
        let (result, call) = self
            .caller
            .ask_keeping_call("interpret", system, user)
            .await;
        let call = call.map(|c| c.to_string());
        match result {
            Ok(text) => Ok(AskAnswer { text, call }),
            Err(e) => Err(AskFailure {
                message: e.to_string(),
                call,
                malformed: e.is_malformed_answer(),
            }),
        }
    }
}

/// What reading a corpus needs: no workers, worktrees or branch.
pub struct IntakeParts<'a> {
    pub graph: &'a SharedGraph,
    pub ledger: &'a Ledger,
    pub prices: &'a PriceTable,
    pub admission: &'a Admission,
    pub clock: &'a dyn Clock,
}

/// An interpretation's outcome and the ledger calls already settled against the
/// run's budget, which opening the run must not charge again. The calls are
/// returned on failure too.
pub struct Intake {
    pub result: Result<Interpretation, OrchestratorError>,
    pub charged: Vec<CallId>,
}

/// A hold for one call. Settled explicitly; released when dropped unsettled (a
/// cancelled interpretation), so a hold never outlives its call.
struct HeldCall<'a> {
    admission: &'a Admission,
    reservation: Option<Reservation>,
}

impl HeldCall<'_> {
    fn amount(&self) -> Money {
        self.reservation
            .as_ref()
            .map_or(Money::ZERO, Reservation::amount)
    }

    fn settle(mut self, run: &str, actual: Money, now: i64) -> Option<Overrun> {
        let reservation = self.reservation.take()?;
        self.admission
            .budget()
            .settle(reservation, run, run, actual, now)
    }
}

impl Drop for HeldCall<'_> {
    fn drop(&mut self) {
        if let Some(r) = self.reservation.take() {
            self.admission.budget().release(r);
        }
    }
}

/// A call's ledger cost; an unsettled or unpriced one is charged the hold, a failed
/// one nothing.
fn cost_of(
    ledger: &Ledger,
    prices: &PriceTable,
    call: &CallId,
    held: Money,
) -> Result<Money, OrchestratorError> {
    let row = blocking(|| ledger.call(call))?;
    Ok(match row {
        Some(r) if matches!(r.settlement, Settlement::Counted | Settlement::Cancelled) => prices
            .cost_on(r.route.as_deref(), &r.model, &r.usage)
            .unwrap_or(held),
        Some(r) if r.settlement == Settlement::Failed => Money::ZERO,
        _ => held,
    })
}

/// Interpret the graph store's root for `run` (see the module documentation).
pub async fn interpret_corpus(
    parts: IntakeParts<'_>,
    run: &RunId,
    request: InterpretRequest,
    asker: &ModelAsker,
    tests: &dyn TestResults,
) -> Intake {
    let mut charged = Vec::new();
    let result = interpret_inner(&parts, run, request, asker, tests, &mut charged).await;
    Intake { result, charged }
}

async fn interpret_inner(
    parts: &IntakeParts<'_>,
    run: &RunId,
    request: InterpretRequest,
    asker: &ModelAsker,
    tests: &dyn TestResults,
    charged: &mut Vec<CallId>,
) -> Result<Interpretation, OrchestratorError> {
    // A malformed answer (cut off mid-string, no object, or a server that
    // could not parse the model's output) is asked again within the bound
    // every JSON-asked call shares; each try is its own call, held and
    // settled on its own. The brief is prepared again per try because an ask
    // consumes it; the corpus read is the same.
    let mut retries = 0u32;
    loop {
        let result = interpret_once(parts, run, request.clone(), asker, tests, charged).await;
        match result {
            Err(OrchestratorError::Interpret(e))
                if e.is_malformed_answer()
                    && retries < kgraf_provider::MAX_MALFORMED_OUTPUT_RETRIES =>
            {
                retries += 1;
            }
            Err(OrchestratorError::Interpret(e)) if e.is_malformed_answer() => {
                return Err(OrchestratorError::Interpret(exhausted(e, retries + 1)));
            }
            other => return other,
        }
    }
}

/// `e`, the last of `asks` malformed answers, saying that it was the last.
fn exhausted(e: InterpretError, asks: u32) -> InterpretError {
    let said = format!("; the call was made {asks} times and every answer was malformed");
    match e {
        InterpretError::BadAnswer { reason, excerpt } => InterpretError::BadAnswer {
            reason: format!("{reason}{said}"),
            excerpt,
        },
        InterpretError::Model {
            message,
            call,
            malformed,
        } => InterpretError::Model {
            message: format!("{message}{said}"),
            call,
            malformed,
        },
        other => other,
    }
}

/// One interpretation ask: prepare, hold, ask, settle, finish.
async fn interpret_once(
    parts: &IntakeParts<'_>,
    run: &RunId,
    request: InterpretRequest,
    asker: &ModelAsker,
    tests: &dyn TestResults,
    charged: &mut Vec<CallId>,
) -> Result<Interpretation, OrchestratorError> {
    let graph = parts.graph.clone();
    let prepared = blocking(|| {
        let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
        Ok::<_, OrchestratorError>(kgraf_interpret::prepare(&g, request)?)
    })?;
    let finish = |answered: kgraf_interpret::Answered| {
        blocking(|| {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            Ok::<_, OrchestratorError>(answered.finish(&g, tests)?)
        })
    };
    if prepared.is_cached() {
        return finish(prepared.ask(asker).await?);
    }

    let basis = asker.caller.cost_basis();
    let input = bound_tokens(prepared.brief().system) + bound_tokens(&prepared.brief().user);
    // An unpriced model's hold is zero: the call is made and ledgered, and its
    // spend is unknown, never a known `$0` (decision 0075).
    let estimate = parts
        .prices
        .cost(
            &basis.model,
            &dear_usage(
                parts.prices,
                &basis.model,
                input,
                u64::from(basis.max_output_tokens),
            ),
        )
        .unwrap_or(Money::ZERO);
    let key = run.to_string();
    let held = HeldCall {
        admission: parts.admission,
        reservation: Some(parts.admission.budget().reserve(
            &key,
            &key,
            estimate,
            parts.clock.now_ms(),
            true,
        )),
    };
    let hold = held.amount();
    let asked = prepared.ask(asker).await;
    let call = match &asked {
        Ok(a) => a.call().map(|c| CallId::from(c.to_owned())),
        Err(InterpretError::Model { call, .. }) => call.clone().map(CallId::from),
        Err(_) => None,
    };
    let Some(call) = call else {
        // No ledger row was opened (a mismatched asker, or a failure before the
        // call): nothing was spent; the hold is released as `held` drops.
        return Err(match asked {
            Ok(_) => OrchestratorError::Budget(
                "the interpretation call returned no ledger row; its spend is unknown".into(),
            ),
            Err(e) => e.into(),
        });
    };
    let actual = cost_of(parts.ledger, parts.prices, &call, hold)?;
    let overrun = held.settle(&key, actual, parts.clock.now_ms());
    charged.push(call);
    let answered = asked?;
    // There is no spend cap (decision 0080): an overrun (the estimate
    // undercounted the call) is accounting only and never drops the answer.
    let _ = overrun;
    finish(answered)
}

impl Orchestrator {
    /// Interpret the graph store's root for `run` on this orchestrator's services
    /// (see [`interpret_corpus`]).
    pub async fn interpret(
        &self,
        run: &RunId,
        request: InterpretRequest,
        asker: &ModelAsker,
        tests: &dyn TestResults,
    ) -> Result<Interpretation, OrchestratorError> {
        let s = &self.services;
        let intake = interpret_corpus(
            IntakeParts {
                graph: &s.graph,
                ledger: &s.ledger,
                prices: &s.prices,
                admission: &s.admission,
                clock: s.clock.as_ref(),
            },
            run,
            request,
            asker,
            tests,
        )
        .await;
        self.precharged
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .extend(intake.charged);
        intake.result
    }
}

/// Seed the approved plan into the graph and build the run's record, its items
/// already the decomposition (`plan_progress`), validated as a decomposition
/// answer is.
pub(crate) fn seeded_record(
    o: &Orchestrator,
    run: RunId,
    plan: &ApprovedPlan,
) -> Result<RunStateRecord, OrchestratorError> {
    let s = &o.services;
    let now = s.clock.now_ms();
    let specs: Vec<ContractSpec> = plan
        .items()
        .iter()
        .map(|i| ContractSpec {
            id: i.id.clone(),
            instruction: i.instruction.clone(),
            acceptance: i.acceptance.clone(),
            writes: i
                .writes
                .iter()
                .map(|p| WriteTarget {
                    path: p.clone(),
                    node: None,
                })
                .collect(),
            depends_on: i.depends_on.clone(),
            slices: i.slices.clone(),
            tools: Vec::new(),
            route: None,
            attempts: 1,
            // The corpus-derived plan (`kgraf-interpret`) does not
            // yet label a kind; Fitness routing defaults an absent kind to
            // Implementation and traces the default (decision 0075).
            task_kind: None,
        })
        .collect();
    let specs = sanitize_plan(specs)?;
    let roster: Vec<String> = s.routes.iter().map(|r| r.name.clone()).collect();
    validate_plan(
        &specs,
        &BTreeSet::new(),
        &roster,
        &[],
        o.config.max_contracts_per_plan,
    )?;
    let graph = s.graph.clone();
    // Seeding also marks the presumed nodes the plan's items stand for as
    // adopted by them (decision 0064); the run record names the plan, so a
    // contract can be drawn on its presumed node.
    blocking(|| {
        let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
        plan.seed(&g, now)?;
        Ok::<_, OrchestratorError>(())
    })?;
    let mut record = RunStateRecord::new(run, plan.original_instruction(), now);
    record.source_plan = Some(plan.plan_id().to_owned());
    record.plan_progress = Some(PlanProgress::seeded(specs));
    Ok(record)
}
