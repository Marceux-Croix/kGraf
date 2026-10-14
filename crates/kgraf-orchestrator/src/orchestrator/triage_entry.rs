//! Small requests skip planning (decision 0100, section 6), seen from the run
//! loop. The rule and the other policies are in `triage.rs`; this file applies
//! them to the run, and the loop carries one-line hooks: the entry in
//! `start()`, the resume check, the merge hold before `finalize_ordinary`,
//! the card hold in `raise_request`, and the terminal check.
//!
//! ```text
//!   start --plan full--> plan() (unchanged)
//!   start --rule: large--> plan()
//!   start --rule: small--> one contract, no plan call --> loop
//!   start --rule: ask, a verified one-step template matches--> one contract --> loop
//!   start --rule: ask--> triage task spawned
//!       --early start, all five conditions--> early contract --> loop
//!            loop --TriageAnswered "one"--> merge released, cards shown
//!            loop --TriageAnswered "split" or failed--> early worker cancelled,
//!                 leases and worktree released, usage recorded --> plan()
//!       --a condition unmet (each logged)--> wait for TriageAnswered
//!            --"one"--> one contract --> loop
//!            --"split" or failed--> plan() with a notice
//!   resume --triage still out--> settled `lost`, early contract cancelled,
//!          plan() (no second triage call, no second early start)
//! ```

use super::*;
use crate::review_desk::PermissionRequestView;
use crate::triage::{
    self, AnswerSize, EarlyCancel, EarlyFacts, FileState, PlanMode, RequestFacts, Settled, Size,
    TriageRecord,
};

/// The id of a small request's one contract.
pub(super) const SMALL_CONTRACT: &str = "c1";

/// Graph meta key of the past triage answers, `[one, split]`, for the split
/// rate early start reads.
const ANSWERS_KEY: &str = "triage/answers";

/// The acceptance of a small request's contract: its check is the run's,
/// narrowed by the scoped check to the packages it writes.
use crate::triage::SMALL_ACCEPTANCE;

/// Loop-only triage state: nothing here needs to survive a restart (a
/// restart settles a pending triage as lost).
#[derive(Default)]
pub(super) struct TriageRuntime {
    /// The early contract's finished flight, held before its merge until the
    /// triage answer arrives.
    held: Option<(OpenContract, InFlight)>,
    /// Permission cards an early worker raised while the answer was pending.
    cards: Vec<PermissionRequestView>,
    /// The triage call's budget hold, settled when the answer arrives.
    reservation: Option<Reservation>,
}

/// The triage task's result, back on the loop.
pub(super) struct TriageDone {
    answer: Result<crate::plan::TriageOutput, crate::plan::PlanError>,
}

/// What a settled answer leads to.
enum Answered {
    One(Vec<String>),
    Plan(String),
}

impl RunLoop {
    fn triage_record(&self) -> Option<&TriageRecord> {
        self.record.triage.as_ref()
    }

    /// Whether a triage call is out.
    pub(super) fn triage_pending(&self) -> bool {
        self.triage_record().is_some_and(TriageRecord::pending)
    }

    fn early_contract(&self) -> Option<&str> {
        self.triage_record().and_then(|t| t.early.as_deref())
    }

    /// Whether the run was started by the small path: one contract, no plan
    /// call. Such a run is not mined into a workflow template.
    pub(super) fn small_path_run(&self) -> bool {
        self.triage_record().is_some_and(|t| t.small_path)
    }

    /// Whether plan review skips a small request's card: the permission mode
    /// never asks (decision 0100's asking table).
    pub(super) fn small_review_skipped(&self) -> bool {
        self.small_path_run() && !triage::small_card_shown(self.perms.control.mode())
    }

    async fn trace_triage(
        &mut self,
        step: &str,
        size: Size,
        detail: String,
    ) -> Result<(), OrchestratorError> {
        let at = self.now();
        let received = self.triage_record().map_or(at, |t| t.received_ms);
        self.trace(TraceEvent::Triage {
            at_ms: at,
            step: step.to_owned(),
            size,
            decided_us: (at - received).max(0).saturating_mul(1_000),
            detail: kgraf_secrets::scrub_persist_owned(detail),
        })
        .await
    }

    /// The entry in `start()`: plan in full, or size the request first
    /// (`--plan auto`). `received_ms` is when the request arrived.
    pub(super) async fn plan_entry(&mut self, received_ms: i64) -> Result<bool, OrchestratorError> {
        if self.config.small.plan != PlanMode::Auto
            || self.record.source_plan.is_some()
            || self.record.triage.is_some()
        {
            return self.plan().await;
        }
        let facts = self.request_facts()?;
        let decision = triage::decide(&facts);
        let now = self.now();
        self.record.triage = Some(TriageRecord {
            size: decision.size,
            reasons: decision.reasons.clone(),
            received_ms,
            decided_ms: now,
            settled: (decision.size != Size::Ask).then_some(Settled::Rule),
            answer: None,
            small_path: false,
            early: None,
            early_unmet: Vec::new(),
            early_cancelled: None,
        });
        self.save()?;
        self.trace_triage("decision", decision.size, decision.reasons.join("; "))
            .await?;
        match decision.size {
            Size::Large => self.plan().await,
            Size::Small => {
                let writes = self.widened(&facts.named_paths())?.0;
                self.install_small(writes, false).await?;
                Ok(true)
            }
            Size::Ask => self.ask(&facts).await,
        }
    }

    /// The facts the rule reads: the scrubbed request, the repository, and
    /// the live leases.
    fn request_facts(&self) -> Result<RequestFacts, OrchestratorError> {
        let text = self.record.original_instruction.clone();
        let known = self.known_files()?;
        let graph = self.s.graph.clone();
        let now = self.now();
        blocking(move || {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            let root = g.root().to_path_buf();
            let state = |p: &str| file_state(&root, p);
            let registry = LeaseRegistry::new(&g);
            let leased = |p: &str| registry.holder_of(p, now).ok().flatten().is_some();
            Ok(triage::facts(&text, &known, &state, &leased))
        })
    }

    /// The named files widened to their package ([`package_files`]), and
    /// whether a live lease is held in that package. Leased, or a package too
    /// big to list: the named files alone.
    fn widened(&self, named: &[String]) -> Result<(Vec<String>, bool), OrchestratorError> {
        let graph = self.s.graph.clone();
        let now = self.now();
        let named = named.to_vec();
        blocking(move || {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            let root = g.root().to_path_buf();
            let Some(package) = package_files(&root, &named) else {
                return Ok((named, false));
            };
            let registry = LeaseRegistry::new(&g);
            let leased = package
                .iter()
                .any(|p| registry.holder_of(p, now).ok().flatten().is_some());
            Ok(if leased {
                (named, true)
            } else {
                (package, false)
            })
        })
    }

    /// One contract for the request, verbatim, writing `writes`; no planner
    /// call. `early`: started beside the triage call, merged only after its
    /// answer.
    async fn install_small(
        &mut self,
        writes: Vec<String>,
        early: bool,
    ) -> Result<(), OrchestratorError> {
        let run = self.run_id();
        let id = SMALL_CONTRACT.to_owned();
        let mut contract = OpenContract::planned(
            id.clone(),
            format!("{run}/{id}").as_str().into(),
            self.record.original_instruction.clone(),
            SMALL_ACCEPTANCE,
        );
        contract.writes = writes
            .iter()
            .map(|p| WriteTarget {
                path: p.clone(),
                node: None,
            })
            .collect();
        contract.slice_from = writes.first().cloned();
        self.record.open_contracts = vec![contract];
        // No interface is registered for it: one contract has no edge. The
        // early contract is never an interface party.
        if let Some(desk) = &self.contracts
            && desk.parties_include(&id)
        {
            return Err(OrchestratorError::Contracts(format!(
                "the small request's contract {id} is an interface party; a small request has \
                 no interfaces"
            )));
        }
        self.note_plan()?;
        self.trace_plan_accepted(Vec::new()).await?;
        self.plan_gates_hook(&BTreeMap::new());
        if self
            .record
            .coordination
            .gates
            .iter()
            .any(|g| g.id == crate::gates::ROOT_GATE)
        {
            self.record
                .coordination
                .background_gates
                .insert(crate::gates::ROOT_GATE.to_owned(), Default::default());
        }
        if let Some(t) = self.record.triage.as_mut() {
            if early {
                t.early = Some(id.clone());
            } else {
                t.small_path = true;
            }
        }
        self.record.planned = true;
        self.record.plan_progress = None;
        self.save()?;
        let size = self.triage_record().map_or(Size::Small, |t| t.size);
        let step = if early { "early_start" } else { "one_contract" };
        self.trace_triage(
            step,
            size,
            format!("contract {id} writes {}", writes.join(", ")),
        )
        .await?;
        if self.small_review_skipped() && self.perms.control.plan_review() {
            let at = self.now();
            self.trace(TraceEvent::PlanReview {
                at_ms: at,
                revision: 1,
                outcome: "skipped".into(),
                detail: format!(
                    "a small request's one contract runs without a card in the {} mode",
                    self.perms.control.mode().label()
                ),
            })
            .await?;
        }
        Ok(())
    }

    /// The rule could not size the request: a template, or the triage call,
    /// with an early start when its conditions hold.
    async fn ask(&mut self, facts: &RequestFacts) -> Result<bool, OrchestratorError> {
        let named = facts.named_paths();
        if let Some(id) = self.template_decides(&named)? {
            if let Some(t) = self.record.triage.as_mut() {
                t.settled = Some(Settled::Template { id: id.clone() });
                t.answer = Some(AnswerSize::One);
            }
            self.trace_triage(
                "template",
                Size::Ask,
                format!("verified one-step template {id} writes the named files"),
            )
            .await?;
            let writes = self.widened(&named)?.0;
            self.install_small(writes, false).await?;
            return Ok(true);
        }
        self.spawn_triage(&named)?;
        self.trace_triage("asked", Size::Ask, "one triage call".to_owned())
            .await?;
        let early = self.early_facts(&named)?;
        match triage::early_start(&early) {
            Ok(scope) => {
                let detail = format!("{} scope", scope.label());
                let files = scope.files().to_vec();
                self.install_small(files, true).await?;
                self.trace_triage("early_scope", Size::Ask, detail).await?;
                return Ok(true);
            }
            Err(unmet) => {
                if let Some(t) = self.record.triage.as_mut() {
                    t.early_unmet = unmet.clone();
                }
                self.save()?;
                self.trace_triage("early_unmet", Size::Ask, unmet.join("; "))
                    .await?;
            }
        }
        // No early start: the answer is waited for here; every other message
        // waits in the backlog, as during plan review.
        // The backlog is read first: a stop check elsewhere may have moved the
        // answer there.
        let done = loop {
            let queued = self
                .backlog
                .iter()
                .position(|m| matches!(m, LoopMsg::TriageAnswered(_)));
            if let Some(LoopMsg::TriageAnswered(done)) = queued.and_then(|i| self.backlog.remove(i))
            {
                break *done;
            }
            if self.stopping || self.stop_token.is_cancelled() {
                self.stopping = true;
                self.settle_triage_hold(None)?;
                return Ok(false);
            }
            let tick = self.config.tick;
            let stop = self.stop_token.clone();
            tokio::select! {
                m = self.rx.recv() => match m {
                    Some(LoopMsg::TriageAnswered(done)) => break *done,
                    Some(LoopMsg::Control(Control::Stop)) => self.stopping = true,
                    Some(other) => self.backlog.push_back(other),
                    None => {}
                },
                () = stop.cancelled() => self.stopping = true,
                () = tokio::time::sleep(tick) => {}
            }
        };
        match self.take_answer(done).await? {
            Answered::One(writes) => {
                let writes = self.widened(&writes)?.0;
                self.install_small(writes, false).await?;
                Ok(true)
            }
            Answered::Plan(_) => self.plan().await,
        }
    }

    /// A verified one-step template whose write patterns are exactly the
    /// named files' decides "one" with no call.
    fn template_decides(&mut self, named: &[String]) -> Result<Option<String>, OrchestratorError> {
        if named.is_empty() {
            return Ok(None);
        }
        let wanted: BTreeSet<String> = named
            .iter()
            .map(|p| crate::templates::write_pattern(p))
            .collect();
        let instruction = self.record.original_instruction.clone();
        let graph = self.s.graph.clone();
        let now = self.now();
        let found = blocking(move || {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            Ok::<_, OrchestratorError>(crate::templates::offers(&g, &instruction, now)?)
        })?;
        Ok(found
            .offers
            .into_iter()
            .find(|o| {
                o.template.steps.len() == 1
                    && o.template.steps[0]
                        .writes
                        .iter()
                        .cloned()
                        .collect::<BTreeSet<_>>()
                        == wanted
            })
            .map(|o| o.template.id))
    }

    /// The planner route's triage speed, p50 and p90 in milliseconds, from
    /// its measured triage calls (`RouteSpeed`'s count), else decision 0100's
    /// first-run defaults for the route's class, with where it came from.
    fn triage_speed(&self) -> (u64, u64, String) {
        let model = self.s.decomposer.cost_basis().model;
        let local = self.s.routes.iter().any(|r| r.local && r.model() == model);
        let measured = self
            .s
            .ledger
            .recent_latencies_ms(&model, Some(&["triage"]), crate::route_speed::WINDOW)
            .unwrap_or_default();
        if measured.len() >= crate::route_speed::MIN_CALLS {
            let n = measured.len();
            return (
                crate::sizing::percentile(measured.clone(), 50).unwrap_or(0),
                crate::sizing::percentile(measured, 90).unwrap_or(0),
                format!("measured from {n} calls"),
            );
        }
        if local {
            (8_000, 30_000, "default for a local route".to_owned())
        } else {
            (3_000, 8_000, "default for a hosted route".to_owned())
        }
    }

    /// Reserve the triage call and start it off the loop. Its answer comes
    /// back as [`LoopMsg::TriageAnswered`], even when the task panics.
    fn spawn_triage(&mut self, named: &[String]) -> Result<(), OrchestratorError> {
        let known = self.known_files()?;
        let (_, p90, _) = self.triage_speed();
        let run_left = self.run_deadline.as_ref().map(|d| d.left_ms(self.now()));
        let request = crate::plan::TriageRequest {
            request: kgraf_secrets::model_copy(&self.record.original_instruction).into_owned(),
            named: named.to_vec(),
            known_files: known,
            deadline: Some(std::time::Duration::from_millis(
                triage::triage_deadline_ms(p90, run_left),
            )),
        };
        let model = self.s.decomposer.cost_basis().model;
        let basis = CostBasis {
            model,
            max_output_tokens: self
                .s
                .decomposer
                .output_limit("triage", Some(triage::TRIAGE_MAX_OUTPUT_TOKENS)),
        };
        let input = estimate_tokens(&request.request)
            + request
                .known_files
                .iter()
                .take(200)
                .map(|f| estimate_tokens(f) + 1)
                .sum::<u64>()
            + 200;
        self.triage_rt.reservation = Some(self.reserve_rounds(&basis, input, 0));
        let decomposer = self.s.decomposer.clone();
        let tx = self.tx.clone();
        let task = tokio::spawn(async move { decomposer.triage(&request).await });
        tokio::spawn(async move {
            let answer = match task.await {
                Ok(a) => a,
                Err(e) => Err(crate::plan::PlanError::Invalid(format!(
                    "the triage task ended abnormally: {e}"
                ))),
            };
            let _ = tx
                .send(LoopMsg::TriageAnswered(Box::new(TriageDone { answer })))
                .await;
        });
        Ok(())
    }

    fn settle_triage_hold(&mut self, calls: Option<&[CallId]>) -> Result<(), OrchestratorError> {
        match self.triage_rt.reservation.take() {
            Some(r) => self.settle_call(r, calls),
            None => Ok(()),
        }
    }

    /// Settle the triage call: its hold, the record, the answer counts, a
    /// notice on a fallback. Returns what the answer leads to.
    async fn take_answer(&mut self, done: TriageDone) -> Result<Answered, OrchestratorError> {
        let calls: Vec<CallId> = match &done.answer {
            Ok(out) => out.calls.clone(),
            Err(e) => e.calls().map(<[CallId]>::to_vec).unwrap_or_default(),
        };
        self.settle_triage_hold(Some(&calls))?;
        let (answered, settled, answer) = match done.answer {
            Ok(out) if out.answer.size == AnswerSize::One => {
                let writes = self.answer_writes(&out.answer.writes)?;
                if writes.is_empty() {
                    let why = "the triage answer named no file to write".to_owned();
                    (
                        Answered::Plan(why.clone()),
                        Settled::Failed { why },
                        Some(AnswerSize::One),
                    )
                } else {
                    (Answered::One(writes), Settled::Call, Some(AnswerSize::One))
                }
            }
            Ok(_) => (
                Answered::Plan("the triage call answered \"split\"".to_owned()),
                Settled::Call,
                Some(AnswerSize::Split),
            ),
            Err(e) => {
                let why = format!("the triage call failed: {e}");
                (Answered::Plan(why.clone()), Settled::Failed { why }, None)
            }
        };
        if let Some(a) = answer {
            self.count_answer(a)?;
        }
        if let Some(t) = self.record.triage.as_mut() {
            t.settled = Some(settled);
            t.answer = answer;
        }
        self.save()?;
        match &answered {
            Answered::One(writes) => {
                self.trace_triage("answer", Size::Ask, format!("one: {}", writes.join(", ")))
                    .await?;
            }
            Answered::Plan(why) => {
                let notice = format!("the request is planned in full: {why}");
                self.notices
                    .insert(kgraf_secrets::scrub_persist_owned(notice.clone()));
                self.trace_triage("fallback", Size::Ask, notice).await?;
            }
        }
        Ok(answered)
    }

    /// The answer's files that exist or sit in an existing directory; the
    /// named files when it names none.
    fn answer_writes(&self, writes: &[String]) -> Result<Vec<String>, OrchestratorError> {
        let graph = self.s.graph.clone();
        let mut writes = writes.to_vec();
        let named = self.request_facts()?.named_paths();
        if writes.is_empty() {
            writes = named;
        }
        let known = self.known_files()?;
        blocking(move || {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            let root = g.root().to_path_buf();
            Ok(writes
                .into_iter()
                .filter_map(|p| {
                    // A name with no directory stands for one indexed file.
                    let (p, s) = if p.contains('/') {
                        let s = file_state(&root, &p);
                        (p, s)
                    } else {
                        triage::resolve_bare(&p, &known)
                    };
                    (s != FileState::Missing).then_some(p)
                })
                .collect())
        })
    }

    fn count_answer(&mut self, answer: AnswerSize) -> Result<(), OrchestratorError> {
        let mut counts: [u32; 2] = self.meta_json(ANSWERS_KEY)?;
        match answer {
            AnswerSize::One => counts[0] += 1,
            AnswerSize::Split => counts[1] += 1,
        }
        self.set_meta_json(ANSWERS_KEY, &counts)
    }

    /// What the five early-start conditions read.
    fn early_facts(&mut self, named: &[String]) -> Result<EarlyFacts, OrchestratorError> {
        let (package, package_leased) = if named.is_empty() {
            (Vec::new(), false)
        } else {
            self.widened(named)?
        };
        let repository = if named.is_empty() {
            self.known_files()?
        } else {
            Vec::new()
        };
        let (items, agents) = self.s.admission.gate().in_flight();
        let limits = self.s.admission.gate().limits();
        let slot_free = items < limits.max_work_items() && agents < limits.max_live_agents();
        let (predicted, _, source) = self.triage_speed();
        let counts: [u32; 2] = self.meta_json(ANSWERS_KEY)?;
        Ok(EarlyFacts {
            enabled: self.config.small.early_start,
            plan_review: self.perms.control.plan_review()
                && triage::small_card_shown(self.perms.control.mode()),
            named: named.to_vec(),
            package: if package_leased { Vec::new() } else { package },
            package_leased,
            other_live: !self.record.open_contracts.is_empty() || self.other_lease_live()?,
            repository,
            slot_free,
            predicted_triage_ms: predicted,
            prediction_source: source,
            answers: (counts[0], counts[1]),
            // Interactive priority on a shared local server is not built:
            // nothing holds the worker's first request behind the triage
            // call.
            local_hold: false,
        })
    }

    /// Whether a live lease is held by anything but this run: another run's
    /// contract, or another run itself. A repository-scope early start would
    /// write beside it.
    fn other_lease_live(&self) -> Result<bool, OrchestratorError> {
        let graph = self.s.graph.clone();
        let now = self.now();
        let own = format!("{}/", self.run_id());
        let run_holder = self.run_holder.clone();
        blocking(move || {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            let leases = LeaseRegistry::new(&g).all()?;
            Ok::<_, OrchestratorError>(leases.iter().any(|l| {
                !l.is_expired(now) && l.agent_id != run_holder && !l.agent_id.starts_with(&own)
            }))
        })
    }

    /// The triage answer arrived on the loop (an early start is running).
    pub(super) async fn on_triage_answered(
        &mut self,
        done: TriageDone,
    ) -> Result<(), OrchestratorError> {
        if !self.triage_pending() {
            // Settled already (a stop, a resume): the call is accounted, and
            // nothing waits on it.
            let calls: Vec<CallId> = match &done.answer {
                Ok(out) => out.calls.clone(),
                Err(e) => e.calls().map(<[CallId]>::to_vec).unwrap_or_default(),
            };
            return self.settle_triage_hold(Some(&calls));
        }
        match self.take_answer(done).await? {
            Answered::One(_) => {
                if let Some(t) = self.record.triage.as_mut() {
                    t.small_path = true;
                }
                self.save()?;
                for view in std::mem::take(&mut self.triage_rt.cards) {
                    self.perms.desk.show_request(view);
                }
                if let Some((contract, flight)) = self.triage_rt.held.take() {
                    self.trace_triage(
                        "merge_released",
                        Size::Ask,
                        format!("contract {} merges", contract.contract),
                    )
                    .await?;
                    return self.finalize(contract, flight).await;
                }
                Ok(())
            }
            Answered::Plan(_) => {
                for view in std::mem::take(&mut self.triage_rt.cards) {
                    self.perms.desk.drop_request(view.id);
                }
                if let Some(id) = self.early_contract().map(str::to_owned) {
                    self.cancel_early(&id).await?;
                }
                self.plan_after_triage().await
            }
        }
    }

    /// Plan the request in full after the small path gave way.
    async fn plan_after_triage(&mut self) -> Result<(), OrchestratorError> {
        self.record.open_contracts.clear();
        self.record.coordination.gates.clear();
        self.record.coordination.background_gates.clear();
        self.record.coordination.waits.clear();
        self.record.planned = false;
        self.record.plan_progress = None;
        self.save()?;
        if !self.plan().await? {
            self.stopping = true;
        }
        Ok(())
    }

    /// Stop the early worker, release its worktree and leases, record what it
    /// used, then close its contract: in that order, so nothing it holds is
    /// left when the plan is made.
    async fn cancel_early(&mut self, id: &str) -> Result<(), OrchestratorError> {
        let Some(contract) = self.record.contract(id).cloned() else {
            return Ok(());
        };
        let holder = self.holder(&contract);
        let flight = match self.triage_rt.held.take() {
            Some((_, f)) => Some(f),
            None => self.in_flight.remove(id),
        };
        let mut workers: Vec<AgentId> = contract.workers.clone();
        if let Some(mut flight) = flight {
            // 1. Cancel, and wait (bounded) until every worker task ended.
            for n in flight.cancels.values() {
                n.notify_one();
            }
            for (worker, rx) in std::mem::take(&mut flight.ended) {
                wait_for_end(rx, &format!("worker `{worker}`")).await;
            }
            // 2. Release the worktrees and the slot.
            for a in &flight.attempts {
                self.s.workspaces.release(&a.workspace, false).await;
                if !workers.contains(&a.worker) {
                    workers.push(a.worker.clone());
                }
            }
            if let Some(a) = flight.admitted.take() {
                self.s.admission.gate().release(a);
            }
            // 3. Settle each worker's hold at its exact spend so far.
            let run = self.run_id();
            let task = contract.task.as_str().to_owned();
            for (worker, r) in std::mem::take(&mut flight.reservations) {
                let held = r.amount();
                let spend = exact_agent_spend(
                    &self.s.ledger,
                    &self.s.prices,
                    &task,
                    &worker,
                    &flight.charged,
                    held,
                );
                let now = self.now();
                let _ = self.s.admission.budget().settle(r, &run, &task, spend, now);
            }
        }
        // The leases, once nothing open shares the holder.
        self.record.open_contracts.retain(|c| c.contract != id);
        self.release_leases_if_unused(&holder)?;
        for w in &workers {
            if let Err(e) = self.retire_session(w, SessionOutcome::Cancelled) {
                eprintln!(
                    "kgraf-orchestrator: the cancelled early worker's session could not be \
                     retired: worker {w}: {e}"
                );
            }
        }
        // What it used, from the ledger: a cut call settled cancelled with
        // its partial usage.
        let rows = self
            .s
            .ledger
            .calls_under_task(contract.task.as_str(), None)
            .unwrap_or_default();
        let cancel = EarlyCancel {
            contract: id.to_owned(),
            calls: rows.len() as u32,
            input_tokens: rows.iter().map(|r| r.usage.total_input()).sum(),
            output_tokens: rows.iter().map(|r| r.usage.total_output()).sum(),
            at_ms: self.now(),
        };
        let detail = format!(
            "early contract {id} cancelled: {} calls, {} input and {} output tokens; its \
             worktree and leases are released",
            cancel.calls, cancel.input_tokens, cancel.output_tokens
        );
        let closed = crate::coordination::ClosedContract::of(
            &contract,
            crate::coordination::Closure::Superseded,
        );
        self.record.coordination.closed.push(closed);
        self.record.coordination.waits.remove(id);
        if let Some(t) = self.record.triage.as_mut() {
            t.early = None;
            t.early_cancelled = Some(cancel);
        }
        self.save()?;
        self.trace_triage("early_cancelled", Size::Ask, detail)
            .await
    }

    /// Hook in `resume()`: a triage the restart cut is settled as lost and
    /// the request planned in full; no second call is made and no second
    /// early worker starts.
    pub(super) async fn resume_triage(&mut self) -> Result<(), OrchestratorError> {
        if !self.triage_pending() {
            return Ok(());
        }
        if let Some(t) = self.record.triage.as_mut() {
            t.settled = Some(Settled::Lost);
        }
        let notice = "the triage answer was lost to the restart; the request is planned in full \
                      with no second triage call";
        self.notices.insert(notice.to_owned());
        self.trace_triage("lost", Size::Ask, notice.to_owned())
            .await?;
        if let Some(id) = self.early_contract().map(str::to_owned) {
            self.cancel_early(&id).await?;
            self.record.open_contracts.clear();
            self.record.coordination.gates.clear();
            self.record.coordination.background_gates.clear();
            self.record.planned = false;
            self.record.plan_progress = None;
        }
        self.save()
    }

    /// Hook where the loop ends on a stop: an early worker whose triage
    /// answer never came is cancelled by `cancel_early`'s steps (its
    /// worktree, slot and leases released, its usage recorded), never merged.
    /// A resume then plans the request in full ([`Self::resume_triage`]).
    pub(super) async fn stop_triage(&mut self) -> Result<(), OrchestratorError> {
        if !self.triage_pending() {
            return Ok(());
        }
        // The call still out may have billed: its hold is charged whole.
        self.settle_triage_hold(None)?;
        if let Some(id) = self.early_contract().map(str::to_owned) {
            self.cancel_early(&id).await?;
            self.record.coordination.gates.clear();
            self.record.coordination.background_gates.clear();
            self.record.planned = false;
            self.save()?;
        }
        Ok(())
    }

    /// Hook before `finalize_ordinary`: an early contract's verified flight
    /// waits for the triage answer. `None`: held.
    pub(super) async fn hold_merge_for_triage(
        &mut self,
        contract: OpenContract,
        flight: InFlight,
    ) -> Result<Option<(OpenContract, InFlight)>, OrchestratorError> {
        // Held while stopping too: an early worker never merges without the
        // answer. A stop cancels it instead (`stop_triage`).
        let held =
            self.triage_pending() && self.early_contract() == Some(contract.contract.as_str());
        if !held {
            return Ok(Some((contract, flight)));
        }
        let detail = format!(
            "contract {} finished; its merge waits for the triage answer",
            contract.contract
        );
        self.triage_rt.held = Some((contract, flight));
        self.trace_triage("merge_held", Size::Ask, detail).await?;
        Ok(None)
    }

    /// Hook in `raise_request`: whether `view` waits for the triage answer
    /// ([`triage::card_held`]). Held cards are shown on "one" and dropped on
    /// "split".
    pub(super) fn hold_card_for_triage(
        &mut self,
        contract: &str,
        view: &PermissionRequestView,
    ) -> bool {
        let early = self.early_contract() == Some(contract);
        if !triage::card_held(self.perms.control.mode(), self.triage_pending(), early) {
            return false;
        }
        self.triage_rt.cards.push(view.clone());
        true
    }

    /// Hook at the top of `emit_dispatch_gap`: a small request's contract is
    /// measured from the request's receipt to its worker's first ledger row,
    /// as B2 (decided with no call) or B3 (decided by the triage call). The
    /// triage call inside the interval is model time. `true`: written.
    pub(super) async fn small_dispatch_latency(
        &mut self,
        contract: &str,
        worker: &AgentId,
    ) -> Result<bool, OrchestratorError> {
        let Some(t) = self.triage_record().cloned() else {
            return Ok(false);
        };
        if contract != SMALL_CONTRACT || !(t.small_path || t.early.is_some()) {
            return Ok(false);
        }
        let planner = format!("orchestrator:{}", self.run_id());
        let facts = match self
            .s
            .ledger
            .dispatch_gap_facts(worker.as_str(), &planner, i64::MAX)
        {
            Ok(Some(f)) => f,
            Ok(None) => return Ok(true),
            Err(e) => {
                self.notices.insert(format!(
                    "contract {contract}: the dispatch interaction could not be read from the \
                     ledger: {e}"
                ));
                return Ok(true);
            }
        };
        self.latency.gap_written.insert(contract.to_owned());
        let anchor = t.received_ms;
        let gap_us = (facts.worker_started_ms - anchor).max(0) * 1_000;
        let run = self.run_id();
        let triage_rows = self
            .s
            .ledger
            .calls_under_task(&run, None)
            .unwrap_or_default();
        let triage_calls: Vec<&kgraf_provider::CallRecord> = triage_rows
            .iter()
            .filter(|r| r.task_id == run && r.trace.kind.as_deref() == Some("triage"))
            .collect();
        let model = crate::latency::call_intervals(&triage_calls, anchor, self.now());
        // The request is eligible on receipt. A slot wait (first deferral to
        // the dispatching attempt) and any sandbox check on the path are
        // named apart from kGraf's own time, as for a planned contract.
        let at = |ms: i64| (ms - anchor) * 1_000;
        let checks: Vec<crate::latency::Interval> = self
            .check_spans_of(contract, facts.worker_started_ms)
            .iter()
            .map(|(s, f)| crate::latency::Interval::new(at(*s), at(*f)))
            .collect();
        let dispatched_ms = self
            .latency
            .contracts
            .get(contract)
            .map_or(facts.worker_started_ms, |c| c.dispatched_wall_ms);
        let capacity: Vec<crate::latency::Interval> = self
            .latency
            .first_deferred_ms
            .get(contract)
            .map(|first| {
                crate::latency::Interval::new(
                    at(*first),
                    at(dispatched_ms.min(facts.worker_started_ms)),
                )
            })
            .into_iter()
            .collect();
        let split = crate::latency::attribute_dispatch(gap_us, &model, &checks, &capacity);
        let at_ms = self.now();
        self.write_latency_event(TraceEvent::DispatchGap {
            at_ms,
            contract: contract.to_owned(),
            planner_ended_ms: anchor,
            worker_started_ms: facts.worker_started_ms,
            gap_us,
            eligible_ms: Some(anchor),
            capacity_wait_us: split.capacity_wait_us,
            checks_us: split.checks_us,
            worker_queued_ms: facts.worker_queued_ms,
        })
        .await;
        let kind = if t.size == Size::Ask && t.template().is_none() {
            crate::latency::LatencyKind::TriageDispatch
        } else {
            crate::latency::LatencyKind::SmallRequestDispatch
        };
        self.emit_latency(kind, split, Some(contract.to_owned()))
            .await?;
        Ok(true)
    }

    /// Hook in `close_frugality`: a run the small path started is not mined
    /// into a template; one a template decided "one" for, that was re-split
    /// or missed its size, counts against that template.
    pub(super) async fn small_path_frugality(&mut self) -> Result<(), OrchestratorError> {
        let Some(template) = self
            .triage_record()
            .and_then(|t| t.template().map(str::to_owned))
        else {
            return Ok(());
        };
        let resplit = !self.record.split_contracts.is_empty();
        let sizing_miss = self.record.compactions.iter().any(|c| c.sizing_miss);
        if !(resplit || sizing_miss) {
            return Ok(());
        }
        let run = self.run_id();
        let now = self.now();
        let graph = self.s.graph.clone();
        let id = template.clone();
        blocking(move || {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            crate::templates::record_failure(&g, &id, &run, now)?;
            Ok::<_, OrchestratorError>(())
        })?;
        let at = self.now();
        self.trace(TraceEvent::Templates {
            at_ms: at,
            action: "failed_on_reuse".into(),
            templates: vec![template],
            detail: if resplit {
                "it decided one contract, and the contract was re-split".into()
            } else {
                "it decided one contract, and the contract missed its size".into()
            },
        })
        .await
    }
}

/// Where `path` stands under `root`: a file, a new file in an existing
/// directory, or neither. A path that leaves the root is missing.
pub(super) fn file_state(root: &Path, path: &str) -> FileState {
    if path.starts_with('/') || path.split('/').any(|c| c == "..") {
        return FileState::Missing;
    }
    let full = root.join(path);
    if full.is_file() {
        return FileState::Exists;
    }
    match full.parent() {
        Some(dir) if dir.is_dir() => FileState::InExistingDir,
        _ => FileState::Missing,
    }
}

/// Manifests that mark a package root.
const PACKAGE_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "pom.xml",
    "build.gradle",
];

/// Directories never listed into a write scope.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".kgraf",
    "node_modules",
    "target",
    "build",
    "dist",
    ".venv",
    "venv",
    "__pycache__",
];

/// The package directory of `path`: the nearest ancestor (inside `root`)
/// holding a manifest, else the file's own directory.
fn package_dir(root: &Path, path: &str) -> String {
    let mut dir = Path::new(path).parent().map(Path::to_path_buf);
    while let Some(d) = dir {
        if PACKAGE_MANIFESTS
            .iter()
            .any(|m| root.join(&d).join(m).is_file())
        {
            return d.to_string_lossy().into_owned();
        }
        if d.as_os_str().is_empty() {
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The named files and every file of their packages, sorted; `None` when the
/// packages hold more than [`triage::PACKAGE_WIDEN_CAP`] files (the named
/// files are then the scope alone). A nested package's files are its own and
/// are not listed.
pub(super) fn package_files(root: &Path, named: &[String]) -> Option<Vec<String>> {
    let mut out: BTreeSet<String> = named.iter().cloned().collect();
    for dir in named
        .iter()
        .map(|p| package_dir(root, p))
        .collect::<BTreeSet<_>>()
    {
        let mut stack = vec![root.join(&dir)];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    let nested = path != root.join(&dir)
                        && PACKAGE_MANIFESTS.iter().any(|m| path.join(m).is_file());
                    if !SKIP_DIRS.contains(&name.as_str()) && !nested {
                        stack.push(path);
                    }
                } else if kind.is_file()
                    && let Ok(rel) = path.strip_prefix(root)
                {
                    out.insert(rel.to_string_lossy().replace('\\', "/"));
                    if out.len() > triage::PACKAGE_WIDEN_CAP {
                        return None;
                    }
                }
            }
        }
    }
    Some(out.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_files_widen_to_the_manifest_directory_and_stop_at_the_cap() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("crates/a/src")).expect("mkdir");
        std::fs::create_dir_all(root.join("crates/a/target")).expect("mkdir");
        std::fs::create_dir_all(root.join("crates/a/inner/src")).expect("mkdir");
        std::fs::write(root.join("crates/a/Cargo.toml"), "").expect("w");
        std::fs::write(root.join("crates/a/src/lib.rs"), "").expect("w");
        std::fs::write(root.join("crates/a/src/b.rs"), "").expect("w");
        std::fs::write(root.join("crates/a/target/x.rs"), "").expect("w");
        std::fs::write(root.join("crates/a/inner/Cargo.toml"), "").expect("w");
        std::fs::write(root.join("crates/a/inner/src/lib.rs"), "").expect("w");
        let files = package_files(root, &["crates/a/src/new.rs".into()]).expect("files");
        assert_eq!(
            files,
            vec![
                "crates/a/Cargo.toml".to_owned(),
                "crates/a/src/b.rs".to_owned(),
                "crates/a/src/lib.rs".to_owned(),
                "crates/a/src/new.rs".to_owned(),
            ],
            "the build output and the nested package are not listed"
        );
        for i in 0..=triage::PACKAGE_WIDEN_CAP {
            std::fs::write(root.join(format!("crates/a/src/f{i}.rs")), "").expect("w");
        }
        assert_eq!(package_files(root, &["crates/a/src/lib.rs".into()]), None);
    }

    #[test]
    fn file_state_reads_the_tree_and_refuses_an_escape() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src/a.rs"), "").expect("w");
        assert_eq!(file_state(dir.path(), "src/a.rs"), FileState::Exists);
        assert_eq!(
            file_state(dir.path(), "src/new.rs"),
            FileState::InExistingDir
        );
        assert_eq!(file_state(dir.path(), "nowhere/x.rs"), FileState::Missing);
        assert_eq!(file_state(dir.path(), "../x.rs"), FileState::Missing);
    }

    /// Package widening for a file outside any manifest is its directory.
    #[test]
    fn a_file_with_no_manifest_widens_to_its_directory() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src/a.py"), "").expect("w");
        std::fs::write(dir.path().join("src/b.py"), "").expect("w");
        std::fs::write(dir.path().join("top.py"), "").expect("w");
        let files = package_files(dir.path(), &["src/a.py".into()]).expect("files");
        assert_eq!(files, vec!["src/a.py".to_owned(), "src/b.py".to_owned()]);
    }
}
