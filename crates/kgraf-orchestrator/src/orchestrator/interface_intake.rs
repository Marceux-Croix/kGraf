//! Plan intake: the contracts call, then the interface call (decision 0030,
//! amendment of 2026-10-02).
//!
//! ```text
//!   contracts ask --refused (parse, schema, rule)--> re-ask, at most MAX_PLAN_REASKS
//!   contracts ask --accepted--> checks off, or one contract --> done
//!                            --> derive, no model call: per consumed edge the
//!                                producer's scaffold, or its current code when
//!                                it holds none (stub), the files that use
//!                                it, the compile command; each derived compile
//!                                check that passes is registered (compile-only)
//!                            --> per producer: behavior ask, or not asked
//!   behavior ask  --no producer needs one--> done, no call
//!   behavior ask  --every interface accepted, every asked edge bound--> done
//!                 --a refusal, or an edge left unbound--> re-ask the refused
//!                   interfaces only, with their previous answer and refusal;
//!                   accepted interfaces are kept; a compile command that does
//!                   not build the files is replaced, and a script test that
//!                   never calls its tests gets the calls, not asked about; a
//!                   refused interface of a producer not asked about is a
//!                   notice, not a re-ask
//!                 --asks spent, or the answer cut off--> every edge no behavior
//!                   interface binds is recorded as unchecked, with the reason
//!                   (a producer that declares no file writes, or no source
//!                   file, is never asked about: no stub can stand in for it)
//!   next ask --the run's planner time budget is spent, or the ask is expected
//!             to end past it--> not made; the same unchecked record, with the
//!             budget as the reason, a notice, and a counted event
//!             ([`crate::planner_budget`]); no time limit, no such stop
//! ```
//!
//! A behavior check is asked of the planner for a producer only when no
//! repository test already exercises it alone and one can exist
//! ([`derive_interface::behavior_plan`]), and not when it writes only binary
//! build outputs ([`derive_interface::binary_outputs_only`]); the answer holds a reference and a
//! small test, and kGraf fills the stub, consumer files and compile command.
//! An omitted interface and a malformed one end the same way: both are asked
//! for again within the interface budget, then fall back. Neither holds the
//! run, and neither is skipped without a recorded, counted reason.

use super::*;
use crate::challenge::ContractEvent;
use crate::checks::{InterfaceContract, SandboxRunner};
use crate::derive_interface::{self, BehaviorPlan, ProducerFacts};
use crate::interface_plan::{
    self, InterfaceDraft, InterfaceReply, InterfaceRequest, PreviousAsk, Supplied, uncovered,
};
use crate::run_state::UncheckedEdge;

/// Asks of the interface call: the first, and one more with the previous
/// answer and its refusal. Measured over 22 recorded runs of one benchmark
/// task, a third ask almost never bought an accepted interface and each ask
/// cost minutes on a local model; after the last ask the edges are recorded
/// unchecked and the run goes on at once.
pub(crate) const MAX_INTERFACE_ASKS: u32 = 2;

/// The guidance added to a mechanical check failure, so the refusal says what
/// a passing interface needs and not only what failed.
fn check_guidance(stage: crate::checks::CheckStage) -> &'static str {
    use crate::checks::CheckStage;
    match stage {
        CheckStage::StubCompile => {
            " (the consumer and tests files must compile against the stub with the compile \
             command, and the compile command must build every one of them)"
        }
        CheckStage::Vacuity => {
            " (the tests must fail against the stub and pass against the reference)"
        }
        CheckStage::Artifact => {
            " (a test file that already exists may be named by path alone, unchanged; a new \
             test goes in a new file)"
        }
        _ => "",
    }
}

impl RunLoop {
    /// One decomposition: the contracts, and, when the run checks contracts and
    /// the answer has two or more, the interfaces binding them and the edges
    /// left unchecked. An interface party must be a contract of this answer or
    /// one of `outer`: the plan's other contracts, which a split's interface
    /// may bind.
    ///
    /// A contracts answer the planner got wrong (not the JSON asked for, or a
    /// plan that breaks a rule) is asked for again, at most
    /// [`MAX_PLAN_REASKS`] times, with the refusal as a note. Every ask is a
    /// ledgered, reserved call; a plan still wrong after the last ask fails the
    /// run with the last refusal. The interfaces have their own budget
    /// ([`Self::interface_asks`]).
    pub(super) async fn decompose(
        &mut self,
        mut request: PlanRequest,
        note_ids: &[String],
        outer: &BTreeSet<String>,
    ) -> Result<Option<Decomposition>, OrchestratorError> {
        request.interfaces = self.contracts.is_some();
        request.own_checks = self.config.own_checks;
        // The planner is told the package names and the signatures of what the
        // instruction names, from the manifests and the graph, so it does not
        // invent them (`planner_context`). Built once: a re-ask sees the same.
        let graph = self.s.graph.clone();
        let (instruction, files) = (request.instruction.clone(), request.known_files.clone());
        let context = blocking(move || {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            Ok::<_, OrchestratorError>(crate::planner_context::project_context(
                &g,
                &instruction,
                &files,
            ))
        })?;
        request.project_context = context.text;
        let named_files = context.named_files;
        for notice in context.notices {
            self.intake_notice(notice);
        }
        // The interface call sees the notes the plan was given (a held plan's
        // failures among them), not the contracts call's own refusals.
        let given_notes = request.notes.clone();
        let mut reasks = 0;
        // A plan whose contracts share one write set checked by one test file
        // is sent back once; a repeat is merged ([`crate::plan::merge_groups`]).
        let mut shared_refused = false;
        let specs = loop {
            let last_ask = reasks >= MAX_PLAN_REASKS;
            let refusal = match self.decompose_once(&request, note_ids).await {
                Ok(None) => return Ok(None),
                Ok(Some(mut specs)) => {
                    // The plan against the repository as it is now (an
                    // unwritten scaffold file, a workspace its acceptance
                    // builds before its members exist). A split's children
                    // are not checked: earlier contracts' work is not in the
                    // checkout this reads. A cargo workspace refusal with a
                    // mechanical remedy is repaired before an ask is spent;
                    // otherwise the unchanged refusal is asked with.
                    let repo_refusal = match self.plan_repo_refusal(&specs, &named_files, outer) {
                        Some(refusal) => match self.plan_repo_repair(&specs, &named_files, outer) {
                            Some((fixed, remedies)) => {
                                self.note_plan_repairs(reasks + 1, &remedies).await?;
                                specs = fixed;
                                None
                            }
                            None => Some(refusal),
                        },
                        None => None,
                    };
                    if let Some(refusal) = repo_refusal {
                        refusal
                    } else {
                        let groups = self.single_test_groups(&specs);
                        if groups.is_empty() {
                            break specs;
                        }
                        let reason = crate::plan::single_test_reason(&groups);
                        if shared_refused || last_ask {
                            let (merged, notices) = crate::plan::merge_groups(specs, &groups);
                            // A merge can join a dependency cycle through a contract
                            // between the merged ones: checked like any plan.
                            match validate_plan_with(
                                &merged,
                                &BTreeSet::new(),
                                &self.roster(),
                                &self.tools(),
                                self.config.max_contracts_per_plan,
                                self.bypass_git_files(),
                            ) {
                                Ok(()) => {
                                    for n in notices {
                                        self.intake_notice(n);
                                    }
                                    break merged;
                                }
                                Err(e) => PlanError::Invalid(format!(
                                    "{reason}; merging them is not possible as the plan stands ({e})"
                                )),
                            }
                        } else {
                            shared_refused = true;
                            PlanError::Invalid(reason)
                        }
                    }
                }
                Err(OrchestratorError::Plan(e)) if e.reaskable() => e,
                Err(e) => return Err(e),
            };
            // One record per refusal, the last included: a run that fails
            // planning still shows every answer it was refused on.
            let at = self.now();
            self.trace(TraceEvent::PlanRefused {
                at_ms: at,
                ask: reasks + 1,
                of: MAX_PLAN_REASKS + 1,
                reason: refusal.to_string(),
            })
            .await?;
            if last_ask {
                return Err(OrchestratorError::Plan(refusal));
            }
            reasks += 1;
            // The refusal can quote the model's own output: scrubbed before it
            // is shown or persisted.
            self.intake_notice(kgraf_secrets::scrub_persist_owned(format!(
                "the planner's answer was refused ({refusal}); asking again ({reasks} of \
                 {MAX_PLAN_REASKS})"
            )));
            request.notes.push(format!(
                "<plan_refused>\nYour previous answer was refused: {}\nAnswer again with one \
                 corrected JSON object.\n</plan_refused>",
                interface_plan::quote_safe(&kgraf_secrets::model_copy(&refusal.to_string()))
            ));
        };
        self.note_shape_changes(&specs).await?;
        // A straight chain is one contract before any interface is derived
        // (`chain_collapse.rs`).
        let specs = self.collapse_chains(specs, outer).await?;
        // One contract binds nothing, unless it is a split's, whose interface
        // may bind the plan's other contracts.
        if !request.interfaces || (specs.len() < 2 && outer.is_empty()) {
            return Ok(Some((specs, Vec::new(), Vec::new())));
        }
        let mut context = request;
        context.notes = given_notes;
        match self.interface_asks(context, &specs, outer).await? {
            Some((interfaces, unchecked)) => Ok(Some((specs, interfaces, unchecked))),
            None => Ok(None),
        }
    }

    /// The refusal of `specs` against the repository checkout, when it has
    /// one ([`crate::plan_repo`]). Not applied to a split's children (`outer`
    /// is not empty).
    fn plan_repo_refusal(
        &self,
        specs: &[ContractSpec],
        named_files: &[String],
        outer: &BTreeSet<String>,
    ) -> Option<PlanError> {
        if !outer.is_empty() {
            return None;
        }
        let root = self.s.workspaces.repo_root()?;
        crate::plan_repo::refusal(specs, named_files, &crate::plan_repo::DiskRepo(root))
    }

    /// `specs` repaired against the repository checkout so the cargo
    /// workspace checks pass, with the changes made
    /// ([`crate::plan_repo::repaired`]), when the repaired plan also passes
    /// the plan rules. `None` leaves the refusal to the planner.
    fn plan_repo_repair(
        &self,
        specs: &[ContractSpec],
        named_files: &[String],
        outer: &BTreeSet<String>,
    ) -> Option<(Vec<ContractSpec>, Vec<crate::plan_repo::Remedy>)> {
        if !outer.is_empty() {
            return None;
        }
        let root = self.s.workspaces.repo_root()?;
        let (fixed, remedies) =
            crate::plan_repo::repaired(specs, named_files, &crate::plan_repo::DiskRepo(root))?;
        validate_plan_with(
            &fixed,
            &BTreeSet::new(),
            &self.roster(),
            &self.tools(),
            self.config.max_contracts_per_plan,
            self.bypass_git_files(),
        )
        .ok()?;
        Some((fixed, remedies))
    }

    /// One trace record and one run notice per change of a repaired plan, so
    /// a repair is never silent and a round can count them.
    async fn note_plan_repairs(
        &mut self,
        ask: u32,
        remedies: &[crate::plan_repo::Remedy],
    ) -> Result<(), OrchestratorError> {
        for r in remedies {
            let at = self.now();
            self.trace(TraceEvent::PlanRepaired {
                at_ms: at,
                ask,
                contract: r.contract().to_owned(),
                change: r.to_string(),
            })
            .await?;
            // Contract ids and paths come from the model's answer: scrubbed
            // before they are shown or persisted.
            self.intake_notice(kgraf_secrets::scrub_persist_owned(format!(
                "the planner's answer failed a cargo workspace check; kGraf repaired it \
                 instead of asking again: {r}"
            )));
        }
        Ok(())
    }

    /// The groups of contracts that declare one write set checked by one test
    /// file, as the verifier narrows the check ([`Verifier::plan_check_scope`]).
    fn single_test_groups(&self, specs: &[ContractSpec]) -> Vec<crate::plan::SharedWrites> {
        let root = self.s.workspaces.repo_root();
        let verifier = self.s.verifier.clone();
        crate::plan::single_test_groups(specs, |writes| {
            let writes: Vec<&str> = writes.iter().map(String::as_str).collect();
            verifier.plan_check_scope(&writes, root.as_deref())
        })
    }

    /// One ask of the contracts call, checked against the plan rules.
    async fn decompose_once(
        &mut self,
        request: &PlanRequest,
        note_ids: &[String],
    ) -> Result<Option<Vec<ContractSpec>>, OrchestratorError> {
        let input = estimate_tokens(&request.instruction)
            + request
                .notes
                .iter()
                .map(|n| estimate_tokens(n))
                .sum::<u64>()
            + request
                .known_files
                .iter()
                .take(400)
                .map(|f| estimate_tokens(f) + 1)
                .sum::<u64>()
            + request
                .templates
                .iter()
                .map(|t| estimate_tokens(t))
                .sum::<u64>()
            + 1_000;
        let basis = self.s.decomposer.cost_basis();
        // With model tools offered, one decomposition is up to
        // `MAX_MODEL_TOOL_ROUNDS + 1` planner calls, each resending the
        // conversation so far; the hold covers them all, so a planner that uses
        // its rounds does not settle over its estimate.
        let tool_rounds = if request.model_calls.is_some() && !request.model_routes.is_empty() {
            crate::plan::MAX_MODEL_TOOL_ROUNDS
        } else {
            0
        };
        let reservation = self.reserve_rounds(&basis, input, tool_rounds);
        // A stop ends the ask: dropping the call leaves its ledger row
        // unreconciled (it may have billed), the hold is settled whole, and
        // the loop ends as stopped. Nothing of the answer is used.
        let stop = self.stop_token.clone();
        let asked_at = self.now();
        let asked = std::time::Instant::now();
        self.publish_planning(Some("decompose"));
        let out =
            super::planner_tasks::cut_by_stop(&stop, self.s.decomposer.decompose(request)).await;
        self.publish_planning(None);
        self.planner_budget
            .record(u64::try_from(self.now() - asked_at).unwrap_or(0));
        let Some(out) = out else {
            self.trace_planning_cut("decompose", asked).await;
            self.stopping = true;
            self.settle_model_tool_calls()?;
            self.settle_call(reservation, None)?;
            self.notices.insert(
                "the run was stopped during a planner call; the call may have billed and is \
                 listed by `kgraf unreconciled`"
                    .to_owned(),
            );
            return Ok(None);
        };
        // The planner's own model-tool calls, settled now whatever the answer.
        self.settle_model_tool_calls()?;
        // The first plan's calls, kept for the change report's measure
        // (decision 0100): not a held plan's re-plan, nor a handoff's split,
        // nor plan review's full re-plan (it leaves a `plan-change-*` note).
        let first_plan = !self.record.planned
            && self.record.replans == 0
            && !self
                .record
                .notes
                .iter()
                .any(|n| n.id.starts_with("plan-change-"));
        let calls_of: Vec<String> = match &out {
            Ok(o) => o.calls.iter().map(ToString::to_string).collect(),
            Err(e) => e
                .calls()
                .unwrap_or_default()
                .iter()
                .map(ToString::to_string)
                .collect(),
        };
        if first_plan {
            self.record.plan_calls.extend(calls_of);
        }
        match out {
            Ok(out) => {
                self.settle_call(reservation, Some(&out.calls))?;
                self.mark_delivered(note_ids);
                let specs = sanitize_plan(out.contracts)?;
                // Repairs the planner's intent makes certain (a task-kind word
                // in `route`, an absolute path inside the repository), each
                // stated as a notice.
                let (specs, repairs) = crate::plan::normalize_plan(
                    specs,
                    self.s.workspaces.repo_root().as_deref(),
                    &self.roster(),
                );
                for r in repairs {
                    self.intake_notice(kgraf_secrets::scrub_persist_owned(r));
                }
                validate_plan_with(
                    &specs,
                    &BTreeSet::new(),
                    &self.roster(),
                    &self.tools(),
                    self.config.max_contracts_per_plan,
                    self.bypass_git_files(),
                )?;
                if let Some(refusal) = crate::plan::writeless_refusal(&specs) {
                    return Err(refusal.into());
                }
                // A contract's own check is judged at intake (decision
                // 0100): one that checks nothing, or that the user's
                // permissions would not run, is refused and asked again.
                if self.config.own_checks {
                    let settings = self.perms.control.settings();
                    if let Some(why) = specs
                        .iter()
                        .filter_map(|s| s.check.as_deref().map(|c| (s, c)))
                        .find_map(|(s, c)| {
                            let writes: Vec<String> =
                                s.writes.iter().map(|w| w.path.clone()).collect();
                            crate::shortfall::check_refusal(c, &writes, &settings)
                                .map(|why| format!("contract `{}`: {why}", s.id))
                        })
                    {
                        return Err(PlanError::Invalid(why).into());
                    }
                }
                Ok(Some(specs))
            }
            Err(e) => {
                self.settle_call(reservation, e.calls())?;
                Err(e.into())
            }
        }
    }

    /// The interface call. kGraf derives first, at no model cost: for every
    /// consumed edge, the producer's scaffold as the stub, the repository files
    /// that use it as the consumer files, and the project's compile command.
    /// Each derived compile check that passes is registered as an interface of
    /// kind compile-only. The planner is asked only for what needs judgment, a
    /// behavior check (a reference and a small test of one producer alone) of
    /// a producer no repository test already exercises alone and for which
    /// such a test can exist ([`derive_interface::behavior_plan`]); when no
    /// producer needs one, no call is made.
    ///
    /// The ask has its own budget of [`MAX_INTERFACE_ASKS`]. Each answer's
    /// interfaces are taken one by one ([`Self::take_interfaces`]): an
    /// accepted one is kept across re-asks, and a re-ask covers only what is
    /// still refused or unbound, quoting the previous answer of the refused
    /// interfaces alone and their refusals. Every consumed edge no behavior
    /// interface binds is returned as unchecked, with the reason. `None`: a
    /// stop arrived during a call.
    pub(super) async fn interface_asks(
        &mut self,
        context: PlanRequest,
        specs: &[ContractSpec],
        outer: &BTreeSet<String>,
    ) -> Result<Option<(Vec<InterfaceContract>, Vec<UncheckedEdge>)>, OrchestratorError> {
        let consumers = interface_plan::consumers(specs);
        // Events of this call are applied once: a resumed run's trace is
        // replayed into the book before the first one.
        self.replay_contracts().await?;
        let derived = self
            .derive_edges(specs, &consumers, &context.known_files)
            .await?;
        let mut accepted: Vec<InterfaceContract> =
            derived.iter().filter_map(|d| d.interface.clone()).collect();
        let mut skipped: BTreeMap<String, String> = derived
            .iter()
            .filter_map(|d| match &d.behavior {
                BehaviorPlan::Skip(why) => Some((d.producer.clone(), why.clone())),
                BehaviorPlan::Ask => None,
            })
            .collect();
        // A producer whose writes cannot carry a stub (none, or no source
        // file) is never asked about, whether or not edges were derived (with
        // no sandbox runner none are).
        for spec in specs {
            let writes: Vec<String> = spec.writes.iter().map(|w| w.path.clone()).collect();
            if let Some(why) = derive_interface::unaskable_writes(&spec.id, &writes) {
                skipped.entry(spec.id.clone()).or_insert(why);
            }
        }
        // The edges the planner is asked a behavior check of.
        let asked: Vec<(String, Vec<String>)> = consumers
            .iter()
            .filter_map(|(c, producers)| {
                let open: Vec<String> = producers
                    .iter()
                    .filter(|p| !skipped.contains_key(*p))
                    .cloned()
                    .collect();
                (!open.is_empty()).then(|| (c.clone(), open))
            })
            .collect();
        self.note_derived(&derived, &asked);
        let supplied = supplied_for(&derived, &asked);
        let output_cap = supplied
            .iter()
            .any(|s| !s.stub.is_empty())
            .then_some(interface_plan::SUPPLIED_OUTPUT_TOKENS);
        // The contracts the ask is about: its producers and their consumers.
        let involved: BTreeSet<&str> = asked
            .iter()
            .flat_map(|(c, ps)| std::iter::once(c.as_str()).chain(ps.iter().map(String::as_str)))
            .collect();
        let ask_specs: Vec<ContractSpec> = if output_cap.is_some() {
            specs
                .iter()
                .filter(|s| involved.contains(s.id.as_str()))
                .cloned()
                .collect()
        } else {
            specs.to_vec()
        };
        // Edges a refused interface declared (its producer and each other
        // party): the planner said they consume, so they are counted with
        // the dependencies when nothing accepted binds them.
        let mut declared: Vec<(String, Vec<String>)> = Vec::new();
        let mut behavior: Vec<InterfaceContract> = Vec::new();
        let mut previous: Option<PreviousAsk> = None;
        // What the last answer was refused for, per interface, and the edges
        // whose interface the planner repeated: those are asked no more.
        let mut refused_before: Vec<RefusedRecord> = Vec::new();
        let mut repeated: BTreeMap<(String, String), String> = BTreeMap::new();
        let mut asked = asked;
        let mut last_refusal: Option<String> = None;
        // Set when the planner time budget ended the call before its asks ran out.
        let mut budget_stop: Option<String> = None;
        for ask in 1..=MAX_INTERFACE_ASKS {
            // Nothing to ask: every asked edge is bound, or every consumed edge
            // has its behavior check decided without the planner. A plan with
            // no consumed edge is asked once, as before: the planner may bind
            // contracts that share a surface and no dependency.
            if uncovered(&asked, &bound(&behavior)).is_empty()
                && previous.is_none()
                && !(consumers.is_empty() && ask == 1)
            {
                break;
            }
            if let Some(why) = self.planner_budget_gate("interfaces").await? {
                budget_stop = Some(why);
                break;
            }
            let request = InterfaceRequest {
                context: context.clone(),
                contracts: ask_specs.clone(),
                outer: outer.iter().cloned().collect(),
                consumers: uncovered(&asked, &bound(&behavior)),
                accepted: bound(&behavior),
                supplied: supplied.clone(),
                output_cap,
                previous: previous.take(),
            };
            let (refusal, answer) = match self.interface_ask_once(&request, ask).await {
                Ok(None) => return Ok(None),
                Ok(Some(reply)) => {
                    let taken = self
                        .take_interfaces(
                            &reply.text,
                            specs,
                            outer,
                            &mut behavior,
                            &derived,
                            &refused_before,
                        )
                        .await?;
                    // Kept across asks: a reason refused in any earlier ask
                    // counts as told, whichever name the interface had then.
                    refused_before.extend(taken.records);
                    accepted.extend(taken.compile_only);
                    for r in taken.repeated {
                        for (consumer, producer) in r.edges {
                            for (c, ps) in &mut asked {
                                if *c == consumer {
                                    ps.retain(|p| *p != producer);
                                }
                            }
                            repeated.insert((consumer, producer), r.reason.clone());
                        }
                    }
                    asked.retain(|(_, ps)| !ps.is_empty());
                    let mut reasons = taken.refusals;
                    for (consumer, producer) in taken.implied {
                        add_edge(&mut declared, &consumer, &producer);
                    }
                    let open = uncovered(&asked, &bound(&behavior));
                    if !open.is_empty() {
                        reasons.push(interface_plan::unbound_reason(&open));
                    }
                    if reasons.is_empty() {
                        break;
                    }
                    // Only the interfaces refused are quoted back; an answer
                    // that was not read at all is quoted whole.
                    let quoted = if taken.unparsed {
                        reply.text
                    } else {
                        interface_plan::answer_subset(&reply.text, &taken.failed)
                            .unwrap_or(reply.text)
                    };
                    (reasons.join("\n"), Some(quoted))
                }
                // A cut-off answer is not asked again (it would be cut off
                // again); it falls back like a spent budget.
                Err(OrchestratorError::Plan(e)) if !e.reaskable() && is_cut_off(&e) => {
                    (e.to_string(), None)
                }
                // The server could not parse the model's own output: asked
                // again on the same budget as a refused answer, with no
                // answer to quote (the shared malformed-output handling).
                Err(OrchestratorError::Plan(e)) if is_server_malformed(&e) => {
                    (e.to_string(), Some(String::new()))
                }
                Err(OrchestratorError::Plan(e)) if e.reaskable() => (e.to_string(), None),
                Err(e) => return Err(e),
            };
            let at = self.now();
            self.trace(TraceEvent::InterfacesRefused {
                at_ms: at,
                ask,
                of: MAX_INTERFACE_ASKS,
                reason: refusal.clone(),
            })
            .await?;
            last_refusal = Some(refusal.clone());
            let Some(answer) = answer else {
                break;
            };
            if ask == MAX_INTERFACE_ASKS {
                break;
            }
            self.intake_notice(kgraf_secrets::scrub_persist_owned(format!(
                "the planner's interfaces were refused ({refusal}); asking again ({ask} of {})",
                MAX_INTERFACE_ASKS - 1
            )));
            previous = Some(PreviousAsk { answer, refusal });
        }
        let mut wanted = consumers.clone();
        for (consumer, producers) in &declared {
            for p in producers {
                add_edge(&mut wanted, consumer, p);
            }
        }
        let open = uncovered(&wanted, &bound(&behavior));
        let mut unchecked = Vec::new();
        for (consumer, producers) in &open {
            for producer in producers {
                let found = derived
                    .iter()
                    .find(|d| &d.producer == producer)
                    .map_or("no compile check was derived", |d| d.compile_note.as_str());
                let why = match skipped.get(producer) {
                    Some(why) => format!("behavior check not run: {why}"),
                    None if budget_stop.is_some() => format!(
                        "behavior check not run: the interface ask was not made, the planner time budget \
                         was spent ({}){}",
                        budget_stop.as_deref().unwrap_or_default(),
                        last_refusal
                            .as_deref()
                            .map_or_else(String::new, |r| format!(
                                "; the last refusal was: {}",
                                one_line(r, 600)
                            ))
                    ),
                    None if repeated.contains_key(&(consumer.clone(), producer.clone())) => {
                        format!(
                            "behavior check not run: the planner repeated an answer refused for \
                             the same reason ({})",
                            repeated[&(consumer.clone(), producer.clone())]
                        )
                    }
                    None => format!(
                        "no behavior interface the planner wrote was accepted ({})",
                        one_line(
                            last_refusal.as_deref().unwrap_or("the planner wrote none"),
                            1_500
                        )
                    ),
                };
                unchecked.push(UncheckedEdge {
                    consumer: consumer.clone(),
                    producer: producer.clone(),
                    reason: kgraf_secrets::scrub_persist_owned(format!("{why}; {found}")),
                });
            }
        }
        accepted.extend(behavior);
        Ok(Some((accepted, unchecked)))
    }

    /// The planner time budget's answer before an optional ask (`phase`:
    /// `interfaces` or `replan`). `Some(why)`: the ask is not to be made; a
    /// notice and a counted event have recorded the stop. `None`: ask.
    pub(super) async fn planner_budget_gate(
        &mut self,
        phase: &str,
    ) -> Result<Option<String>, OrchestratorError> {
        let Err(over) = self.planner_budget.check() else {
            return Ok(None);
        };
        let why = over.describe();
        self.intake_notice(kgraf_secrets::scrub_persist_owned(format!(
            "the planner time budget stopped the {phase} ask: {why}"
        )));
        self.contract_event(ContractEvent::PlannerBudgetSkipped {
            phase: phase.to_owned(),
            spent_ms: over.spent_ms,
            budget_ms: over.budget_ms,
            expected_ms: over.expected_ms,
        })
        .await?;
        Ok(Some(why))
    }

    /// A notice of what kGraf derived and what it asks the planner: the
    /// compile-only interfaces registered, and per producer whether a behavior
    /// check is asked for.
    fn note_derived(&mut self, derived: &[Derived], asked: &[(String, Vec<String>)]) {
        if derived.is_empty() {
            return;
        }
        let compile: Vec<&str> = derived
            .iter()
            .filter(|d| d.interface.is_some())
            .map(|d| d.producer.as_str())
            .collect();
        let asking: BTreeSet<&str> = asked
            .iter()
            .flat_map(|(_, ps)| ps.iter().map(String::as_str))
            .collect();
        let mut text = format!(
            "kGraf derived {} compile-only interface(s) from the repository{}; a behavior \
             check is asked of the planner for {}",
            compile.len(),
            if compile.is_empty() {
                String::new()
            } else {
                format!(" ({})", compile.join(", "))
            },
            if asking.is_empty() {
                "no producer".to_owned()
            } else {
                asking.into_iter().collect::<Vec<_>>().join(", ")
            }
        );
        for d in derived {
            if let BehaviorPlan::Skip(why) = &d.behavior {
                text.push_str(&format!("; not asked for {}: {why}", d.producer));
            }
        }
        self.intake_notice(kgraf_secrets::scrub_persist_owned(text));
    }

    /// One ask of the interface call: reserved, ledgered and settled as the
    /// contracts call is, and counted ([`ContractEvent::InterfaceAsked`]) with
    /// the time and output tokens it took. `None`: a stop arrived during the
    /// call.
    async fn interface_ask_once(
        &mut self,
        request: &InterfaceRequest,
        ask: u32,
    ) -> Result<Option<InterfaceReply>, OrchestratorError> {
        let input = estimate_tokens(&request.context.instruction)
            + request
                .context
                .known_files
                .iter()
                .take(400)
                .map(|f| estimate_tokens(f) + 1)
                .sum::<u64>()
            + estimate_tokens(&request.context.project_context)
            + request
                .contracts
                .iter()
                .map(|c| estimate_tokens(&c.instruction) + 40)
                .sum::<u64>()
            + request.previous.as_ref().map_or(0, |p| {
                estimate_tokens(&p.answer) + estimate_tokens(&p.refusal)
            })
            + 2_000;
        let mut basis = self.s.decomposer.cost_basis();
        basis.max_output_tokens = self
            .s
            .decomposer
            .output_limit("interfaces", request.output_cap);
        let reservation = self.reserve_rounds(&basis, input, 0);
        let stop = self.stop_token.clone();
        let asked_at = self.now();
        let asked = std::time::Instant::now();
        self.publish_planning(Some("interfaces"));
        let out =
            super::planner_tasks::cut_by_stop(&stop, self.s.decomposer.interfaces(request)).await;
        self.publish_planning(None);
        self.planner_budget
            .record(u64::try_from(self.now() - asked_at).unwrap_or(0));
        let Some(out) = out else {
            self.trace_planning_cut("interfaces", asked).await;
            self.stopping = true;
            self.settle_call(reservation, None)?;
            self.notices.insert(
                "the run was stopped during a planner call; the call may have billed and is \
                 listed by `kgraf unreconciled`"
                    .to_owned(),
            );
            return Ok(None);
        };
        let producers: Vec<String> = {
            let mut p: Vec<String> = request
                .consumers
                .iter()
                .flat_map(|(_, ps)| ps.iter().cloned())
                .collect();
            p.sort();
            p.dedup();
            p
        };
        match out {
            Ok(reply) => {
                self.settle_call(reservation, Some(&reply.calls))?;
                self.count_interface_ask(ask, producers, &reply.calls)
                    .await?;
                Ok(Some(reply))
            }
            Err(e) => {
                let calls = e.calls().map(<[CallId]>::to_vec).unwrap_or_default();
                self.settle_call(reservation, e.calls())?;
                self.count_interface_ask(ask, producers, &calls).await?;
                Err(e.into())
            }
        }
    }

    /// Record what `calls` of one interface ask cost: their number, their
    /// wall-clock time and their output tokens, read from the ledger.
    async fn count_interface_ask(
        &mut self,
        ask: u32,
        producers: Vec<String>,
        calls: &[CallId],
    ) -> Result<(), OrchestratorError> {
        let ledger = self.s.ledger.clone();
        let ids = calls.to_vec();
        let (ms, output_tokens) = blocking(move || {
            let (mut ms, mut tokens) = (0u64, 0u64);
            for id in &ids {
                if let Some(r) = ledger.call(id)? {
                    ms += r
                        .finished_ms
                        .map_or(0, |f| u64::try_from(f - r.started_ms).unwrap_or(0));
                    tokens += r.usage.text_output + r.usage.reasoning_output;
                }
            }
            Ok::<_, kgraf_provider::LedgerError>((ms, tokens))
        })?;
        self.contract_event(ContractEvent::InterfaceAsked {
            ask,
            producers,
            calls: calls.len() as u64,
            ms,
            output_tokens,
        })
        .await
    }

    /// Take an interface answer: every interface that is well formed, binds
    /// contracts of the plan, names every file it needs and passes its
    /// mechanical checks joins `accepted`; each other one is refused with
    /// what it needs. What kGraf derived for a producer fills what the answer
    /// leaves out (the stub, the consumer files, the names, the compile
    /// command), and a compile command that does not build the files is
    /// replaced by the derived one and the checks run again, with no re-ask.
    async fn take_interfaces(
        &mut self,
        text: &str,
        specs: &[ContractSpec],
        outer: &BTreeSet<String>,
        accepted: &mut Vec<InterfaceContract>,
        derived: &[Derived],
        previous: &[RefusedRecord],
    ) -> Result<Taken, OrchestratorError> {
        let mut taken = Taken::default();
        let drafts = match crate::interface_plan::parse_interfaces(text) {
            Ok(d) => d,
            Err(e) => {
                taken.unparsed = true;
                taken.refusals.push(e.to_string());
                return Ok(taken);
            }
        };
        let ids: BTreeSet<&str> = specs.iter().map(|s| s.id.as_str()).collect();
        let writes_of = |p: &str| -> Option<Vec<String>> {
            specs
                .iter()
                .find(|s| s.id == p)
                .map(|s| s.writes.iter().map(|w| w.path.clone()).collect())
        };
        for (index, draft) in drafts.into_iter().enumerate() {
            let InterfaceDraft {
                mut interface,
                from_repository,
            } = draft;
            self.fill_derived(&mut interface, &from_repository, derived)
                .await;
            // Sanitized one at a time: one interface's command problem refuses
            // that interface, not the others.
            let mut ic = match sanitize_interfaces(vec![interface.clone()]) {
                Ok(mut v) => v.remove(0),
                Err(e) => {
                    let mut raw = interface;
                    raw.parties = raw
                        .parties
                        .iter()
                        .map(|p| crate::plan::sanitize_id(p))
                        .collect();
                    let edges = declared_edges(&raw, &writes_of, &ids);
                    taken.implied.extend(edges.iter().cloned());
                    let problems = [e.to_string()];
                    if let Some(note) = unasked_refusal(&raw, &problems, &writes_of, &ids, derived)
                    {
                        self.intake_notice(note);
                        continue;
                    }
                    if let Some(note) = taken.refuse(&raw.id, edges, &problems, index, previous) {
                        self.intake_notice(note);
                    }
                    continue;
                }
            };
            // A re-ask answer that repeats an accepted interface adds nothing.
            if accepted.iter().any(|a| same_artifact(a, &ic)) {
                continue;
            }
            let id_taken = |id: &str, accepted: &[InterfaceContract]| {
                accepted.iter().any(|a| a.id == id)
                    || derived
                        .iter()
                        .any(|d| d.interface.as_ref().is_some_and(|i| i.id == id))
            };
            if id_taken(&ic.id, accepted) {
                let base = ic.id.clone();
                let mut n = 2;
                while id_taken(&format!("{base}-{n}"), accepted) {
                    n += 1;
                }
                ic.id = format!("{base}-{n}");
                self.intake_notice(format!(
                    "interface id `{base}` was taken by an accepted interface; the new one is \
                     `{}`",
                    ic.id
                ));
            }
            let mut problems = Vec::new();
            if ic.parties.is_empty() {
                problems.push(format!(
                    "interface `{}` names no parties: list the producer and its consumers",
                    ic.id
                ));
            }
            for p in &ic.parties {
                if !ids.contains(p.as_str()) && !outer.contains(p) {
                    problems.push(format!(
                        "interface `{}` names party `{p}`, which is not a contract of this \
                         plan; parties are contract ids ({})",
                        ic.id,
                        ids.iter().copied().collect::<Vec<_>>().join(", ")
                    ));
                }
            }
            // A symbol written as a bare file name names the one stub or
            // reference file of that name; the parties are asked about the
            // path. Two such files leave the planner to say which it meant.
            let files = crate::checks::resolve_file_symbols(&ic);
            for (bare, path) in &files.resolved {
                self.intake_notice(format!(
                    "interface `{}` symbol `{bare}` is read as `{path}`, its only stub or \
                     reference file of that name",
                    ic.id
                ));
            }
            for (bare, paths) in &files.ambiguous {
                problems.push(format!(
                    "interface `{}` names symbol `{bare}`, a file name that {} stub or \
                     reference files have ({}): name the file by its path",
                    ic.id,
                    paths.len(),
                    paths.join(", ")
                ));
            }
            ic.symbols = files.symbols;
            problems.extend(interface_plan::shape_problems(&ic, writes_of));
            if problems.is_empty() {
                problems.extend(self.fill_from_repository(&mut ic, &from_repository).await);
            }
            if problems.is_empty() {
                problems.extend(self.call_script_tests(&mut ic, &from_repository));
            }
            if problems.is_empty() {
                problems.extend(self.undeclared_replacements(&ic).await);
            }
            if problems.is_empty() {
                problems.extend(self.missing_command_files(&ic, specs).await);
            }
            if problems.is_empty()
                && let Some(runner) = self.checks_runner()
            {
                // A check that could not run (`Err`) says nothing about the
                // interface: it is taken, and the gate records the check as not
                // run.
                if let Ok(Err(failure)) = self.check_with_repair(&runner, &mut ic).await {
                    problems.push(format!(
                        "interface `{}` failed its pre-dispatch check: {failure}{}",
                        ic.id,
                        check_guidance(failure.stage)
                    ));
                }
            }
            if problems.is_empty() {
                accepted.push(ic);
            } else {
                let edges = declared_edges(&ic, &writes_of, &ids);
                taken.implied.extend(edges.iter().cloned());
                if let Some(note) = unasked_refusal(&ic, &problems, &writes_of, &ids, derived) {
                    self.intake_notice(note);
                    continue;
                }
                // The planner left out the tests file again and nothing else
                // is wrong: the interface still checks that the consumers
                // compile against the stub, so it is kept as compile-only.
                if problems == [interface_plan::no_tests_problem(&ic.id)]
                    && Taken::repeats(&ic.id, &edges, &problems, previous)
                {
                    match self.compile_only_of(&ic, &from_repository).await {
                        Ok(co) => {
                            self.intake_notice(kgraf_secrets::scrub_persist_owned(format!(
                                "interface `{}` was refused for a missing tests file twice; \
                                 kGraf registers it as a compile-only interface (the consumers \
                                 compile against the stub, no behavior test), and its behavior \
                                 check does not run",
                                ic.id
                            )));
                            taken.repeated.push(Repeated {
                                edges,
                                reason: kgraf_secrets::scrub_persist_owned(problems.join("; ")),
                            });
                            taken.compile_only.push(co);
                            continue;
                        }
                        Err(why) => {
                            self.intake_notice(kgraf_secrets::scrub_persist_owned(format!(
                                "interface `{}` could not be kept as compile-only ({why})",
                                ic.id
                            )))
                        }
                    }
                }
                if let Some(note) = taken.refuse(&ic.id, edges, &problems, index, previous) {
                    self.intake_notice(note);
                }
            }
        }
        Ok(taken)
    }

    /// `ic` as a compile-only interface: no reference, no test, and its
    /// compile check run against the stub. `Err`: why it cannot be kept (a
    /// file named by path alone is missing, a stub path replaces a file
    /// undeclared, no check base, the compile check fails).
    async fn compile_only_of(
        &mut self,
        ic: &InterfaceContract,
        from_repository: &[(&'static str, usize)],
    ) -> Result<InterfaceContract, String> {
        let mut co = ic.clone();
        co.kind = crate::checks::InterfaceKind::CompileOnly;
        co.reference.clear();
        co.wrong.clear();
        co.tests.clear();
        co.test.clear();
        let mut problems = self.fill_from_repository(&mut co, from_repository).await;
        problems.extend(self.undeclared_replacements(&co).await);
        let Some(runner) = self.checks_runner() else {
            return Err("this run has no check base".to_owned());
        };
        if problems.is_empty() {
            match crate::checks::check_derived_compile(&runner, &co).await {
                Ok(Ok(_)) => {}
                Ok(Err(failure)) => problems.push(failure.to_string()),
                Err(e) => problems.push(e.to_string()),
            }
        }
        if problems.is_empty() {
            Ok(co)
        } else {
            Err(one_line(&problems.join("; "), 600))
        }
    }

    /// Fill into `ic` what kGraf derived for its producer and the answer left
    /// out: the stub (the scaffold the repository holds, declared in
    /// `replaces`: it stands in for that file), the consumer files, the names
    /// on the surface. A compile command the answer lacks, or that cannot run
    /// in the check sandbox, is replaced by the one derived over the files the
    /// interface builds ([`Self::derived_compile`]).
    async fn fill_derived(
        &mut self,
        ic: &mut InterfaceContract,
        from_repository: &[(&'static str, usize)],
        derived: &[Derived],
    ) {
        // The producer is the party kGraf derived a base for whose consumers
        // the other parties are; failing that, the first party with a base.
        let parties: Vec<String> = ic
            .parties
            .iter()
            .map(|p| crate::plan::sanitize_id(p))
            .collect();
        let with_base: Vec<&Derived> = parties
            .iter()
            .filter_map(|p| {
                derived
                    .iter()
                    .find(|d| &d.producer == p && d.base.is_some())
            })
            .collect();
        let producer = with_base
            .iter()
            .find(|d| {
                parties
                    .iter()
                    .any(|p| *p != d.producer && d.consumers.contains(p))
            })
            .or(with_base.first())
            .and_then(|d| d.base.as_ref());
        if let Some(base) = producer {
            if ic.stub.is_empty() {
                ic.stub = base.stub.clone();
                for f in &base.stub {
                    if !ic.replaces.contains(&f.path) {
                        ic.replaces.push(f.path.clone());
                    }
                }
            }
            if ic.consumer.is_empty() && !from_repository.iter().any(|(l, _)| *l == "consumer") {
                ic.consumer = base.consumer.clone();
            }
            if ic.symbols.is_empty() {
                ic.symbols = base
                    .symbols
                    .iter()
                    .take(MAX_FILLED_SYMBOLS)
                    .cloned()
                    .collect();
            }
        }
        let broken = ic
            .command_problems()
            .iter()
            .any(|p| p.starts_with("the compile"));
        if (ic.compile.is_empty() || broken)
            && let Some(cmd) = self.derived_compile(ic).await
        {
            if broken {
                self.intake_notice(format!(
                    "interface `{}`: the compile command could not run in the check sandbox; \
                     kGraf uses `{}`",
                    ic.id,
                    cmd.join(" ")
                ));
            }
            ic.compile = cmd;
        }
    }

    /// The compile command the project's build tool takes over the consumer
    /// and test files of `ic`, or `None` when the language or a package is
    /// not known ([`derive_interface::compile_command`]).
    async fn derived_compile(&self, ic: &InterfaceContract) -> Option<Vec<String>> {
        let runner = self.checks_runner()?;
        let files: Vec<String> = ic
            .consumer
            .iter()
            .chain(&ic.tests)
            .map(|f| f.path.clone())
            .collect();
        let manifests = manifests_for(&runner, &files).await;
        let texts: Vec<(String, String)> = ic
            .consumer
            .iter()
            .chain(&ic.tests)
            .map(|f| (f.path.clone(), f.content.clone()))
            .collect();
        let build = build_facts_for(&runner, &files, &texts).await;
        derive_interface::command_for(&files, |f| package_in(&manifests, f), &build).ok()
    }

    /// The mechanical checks of `ic`; when the stub-compile stage fails, the
    /// derived compile command replaces the answer's and they run again, once,
    /// so a command that does not build every file is repaired and not asked
    /// about. `ic` takes the repaired command when the checks then pass.
    async fn check_with_repair(
        &mut self,
        runner: &SandboxRunner,
        ic: &mut InterfaceContract,
    ) -> Result<Result<(), crate::checks::CheckFailure>, crate::checks::CheckError> {
        let first = crate::checks::check_mechanical(runner, ic).await?;
        let Err(failure) = first else {
            return Ok(Ok(()));
        };
        if failure.stage != crate::checks::CheckStage::StubCompile {
            return Ok(Err(failure));
        }
        let Some(cmd) = self.derived_compile(ic).await.filter(|c| *c != ic.compile) else {
            return Ok(Err(failure));
        };
        let mut repaired = ic.clone();
        repaired.compile = cmd;
        match crate::checks::check_mechanical(runner, &repaired).await? {
            Ok(_) => {
                self.intake_notice(format!(
                    "interface `{}`: the answer's compile command did not build every file; \
                     kGraf replaced it with `{}`, and the checks passed",
                    ic.id,
                    repaired.compile.join(" ")
                ));
                *ic = repaired;
                Ok(Ok(()))
            }
            Err(second) => Ok(Err(second)),
        }
    }

    /// Fill the consumer and tests entries an answer named by path alone from
    /// the dispatch base. Returns a refusal for each path that is not inside
    /// the repository, that the base has no file at, or that cannot be read.
    async fn fill_from_repository(
        &mut self,
        ic: &mut InterfaceContract,
        entries: &[(&'static str, usize)],
    ) -> Vec<String> {
        let mut problems = Vec::new();
        if entries.is_empty() {
            return problems;
        }
        let Some(runner) = self.checks_runner() else {
            return vec![format!(
                "interface `{}` names files by path alone, and this run has no check base to \
                 read them from",
                ic.id
            )];
        };
        for &(list, n) in entries {
            let file = match list {
                "consumer" => ic.consumer.get_mut(n),
                "tests" => ic.tests.get_mut(n),
                _ => None,
            };
            let Some(file) = file else {
                continue;
            };
            if !crate::plan::valid_write_path(&file.path) {
                problems.push(format!(
                    "interface `{}`: the {list} entry `{}` is not a path inside the repository",
                    ic.id, file.path
                ));
                continue;
            }
            // A read that fails is said as this interface's problem: the run
            // is not ended over one file the planner named.
            match runner.read_base(&file.path).await {
                Ok(Some(text)) => file.content = text,
                Ok(None) => problems.push(format!(
                    "interface `{}`: the {list} entry `{}` gives no content, and the repository \
                     has no file at that path (a file named by path alone is read from the \
                     repository). Do one of two things: give the file's content, as an object \
                     {{\"path\": \"{}\", \"content\": \"...\"}} (a bare string is a path alone), \
                     or name a file that exists in the repository",
                    ic.id, file.path, file.path
                )),
                Err(e) => problems.push(format!(
                    "interface `{}`: the {list} entry `{}` could not be read from the repository \
                     ({e}); give its content",
                    ic.id, file.path
                )),
            }
        }
        problems
    }

    /// A Python test file of the answer's own that the test command runs as a
    /// script and that never calls its tests ([`crate::script_tests`]): the
    /// calls are appended when every test can be called without arguments,
    /// with a notice, as a compile command that does not build the files is
    /// replaced; otherwise the refusal names the cause. Without this the
    /// vacuity check refuses the file for passing against the stub, which
    /// does not say why, and the planner repeats it on every ask.
    fn call_script_tests(
        &mut self,
        ic: &mut InterfaceContract,
        from_repository: &[(&'static str, usize)],
    ) -> Option<String> {
        let written = |i: usize| !from_repository.contains(&("tests", i));
        match crate::script_tests::check(ic, written)? {
            crate::script_tests::ScriptTests::Refused(why) => Some(why),
            crate::script_tests::ScriptTests::Repaired {
                index,
                content,
                called,
            } => {
                let path = ic.tests[index].path.clone();
                ic.tests[index].content = content;
                self.intake_notice(format!(
                    "interface `{}`: `{}` runs `{path}` as a script, which defined {} without \
                     calling them; kGraf added the calls at the end of the file",
                    ic.id,
                    ic.test.join(" "),
                    called.join(", ")
                ));
                None
            }
        }
    }

    /// A stub file that already exists in the repository and is not listed in
    /// `replaces`: the stub land refuses it before any party dispatches
    /// (decision 0081), so it is asked for again here, where the planner can
    /// still list it, instead of holding the plan at the gate. A scaffold the
    /// producer fills in is the usual case.
    async fn undeclared_replacements(&self, ic: &InterfaceContract) -> Vec<String> {
        let Some(runner) = self.checks_runner() else {
            return Vec::new();
        };
        let mut problems = Vec::new();
        for f in &ic.stub {
            if ic.replaces.iter().any(|r| r == &f.path) {
                continue;
            }
            if matches!(runner.read_base(&f.path).await, Ok(Some(_))) {
                problems.push(format!(
                    "interface `{}`: the stub file `{}` already exists in the repository; list it \
                     in \"replaces\" (the stub stands in for it until the producer's work lands)",
                    ic.id, f.path
                ));
            }
        }
        problems
    }

    /// A file a compile or test command names that exists nowhere the check
    /// can see: not among the interface's own files, and not in the repository
    /// at the dispatch base. The check runs before any party is dispatched, so
    /// a file a contract writes later is not there, and the command would fail
    /// on the missing file, not on the code. Asked for again here, naming the
    /// file, so the interface supplies it or uses a command over files that
    /// exist. A read that fails says nothing about the file and is not a
    /// problem.
    async fn missing_command_files(
        &self,
        ic: &InterfaceContract,
        specs: &[ContractSpec],
    ) -> Vec<String> {
        let candidates = ic.unsupplied_command_files();
        if candidates.is_empty() {
            return Vec::new();
        }
        let Some(runner) = self.checks_runner() else {
            return Vec::new();
        };
        let mut problems = Vec::new();
        for (what, path) in candidates {
            if !matches!(runner.read_base(&path).await, Ok(None)) {
                continue;
            }
            let writers: Vec<&str> = specs
                .iter()
                .filter(|s| s.writes.iter().any(|w| w.path == path))
                .map(|s| s.id.as_str())
                .collect();
            let later = if writers.is_empty() {
                String::new()
            } else {
                format!(
                    " ({} writes it only when it runs, after this check)",
                    writers.join(", ")
                )
            };
            problems.push(format!(
                "interface `{}`: the {what} command names `{path}`, which the interface does \
                 not supply and the repository does not hold{later}; the check runs before any \
                 contract is dispatched, so give the file's content in the interface (stub, \
                 consumer or tests), or use a command over files that exist",
                ic.id
            ));
        }
        problems
    }

    /// For every producer of a consumed edge, what kGraf can derive before any
    /// planner call, and whether a behavior check needs the planner. The
    /// compile check of each derived interface runs now; a derived interface
    /// whose check fails is not registered (its failure is the reason an edge
    /// reads as unchecked). Never holds the run.
    async fn derive_edges(
        &mut self,
        specs: &[ContractSpec],
        consumers: &[(String, Vec<String>)],
        known_files: &[String],
    ) -> Result<Vec<Derived>, OrchestratorError> {
        let Some(runner) = self.checks_runner() else {
            return Ok(Vec::new());
        };
        let mut by_producer: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (consumer, producers) in consumers {
            for p in producers {
                by_producer
                    .entry(p.clone())
                    .or_default()
                    .push(consumer.clone());
            }
        }
        let tests = repo_tests(&runner, known_files).await;
        let contract_facts = contract_facts(&runner, specs).await;
        // The facts are read one producer at a time (they need the graph
        // through the loop's own state); the compile checks that follow are
        // independent of each other and run together.
        let mut found = Vec::new();
        for (producer, consumers) in by_producer {
            let Some(spec) = specs.iter().find(|s| s.id == producer) else {
                continue;
            };
            let facts = self
                .producer_facts(&runner, spec, &consumers, specs)
                .await?;
            let writes: Vec<String> = spec.writes.iter().map(|w| w.path.clone()).collect();
            found.push((producer, consumers, writes, facts));
        }
        let started = self.now();
        let checked = in_order_together(found.iter().map(|f| &f.3), |facts| {
            derive_one(&runner, facts)
        })
        .await;
        // The sandbox time of plan intake, for the latency split.
        if !found.is_empty() {
            let ended = self.now();
            self.latency.plan_checks.push((started, ended));
        }
        let mut out = Vec::new();
        for ((producer, consumers, writes, _), (interface, base, compile_note)) in
            found.into_iter().zip(checked)
        {
            let behavior = match derive_interface::unaskable_writes(&producer, &writes) {
                Some(why) => BehaviorPlan::Skip(why),
                None => derive_interface::behavior_plan(&producer, &tests, &contract_facts),
            };
            out.push(Derived {
                producer,
                consumers,
                interface,
                compile_note,
                base,
                behavior,
            });
        }
        Ok(out)
    }

    /// What the graph and the dispatch base say about `spec` as a producer:
    /// its files, the names they define, and the files that use them.
    async fn producer_facts(
        &mut self,
        runner: &SandboxRunner,
        spec: &ContractSpec,
        consumers: &[String],
        specs: &[ContractSpec],
    ) -> Result<ProducerFacts, OrchestratorError> {
        let paths: Vec<String> = spec.writes.iter().map(|w| w.path.clone()).collect();
        let graph = self.s.graph.clone();
        let own = paths.clone();
        let (symbols, mut users) = blocking(move || {
            let g = graph.lock().map_err(|_| StoreError::Poisoned)?;
            let mut symbols: Vec<String> = Vec::new();
            let mut users: BTreeSet<String> = BTreeSet::new();
            for path in &own {
                for n in g.nodes_in_file(path)? {
                    if matches!(
                        n.kind,
                        kgraf_graph::NodeKind::File
                            | kgraf_graph::NodeKind::Impl
                            | kgraf_graph::NodeKind::Section
                    ) {
                        continue;
                    }
                    if !symbols.contains(&n.name) {
                        symbols.push(n.name.clone());
                    }
                    for (other, kind, _, outgoing) in g.neighbors(&n.id)? {
                        if !outgoing
                            && kind != kgraf_graph::EdgeKind::Contains
                            && !own.contains(&other.path)
                        {
                            users.insert(other.path.clone());
                        }
                    }
                }
            }
            Ok::<_, OrchestratorError>((symbols, users))
        })?;
        // The consumers' own files the repository already has: what they
        // build on, whether or not the graph saw an edge into the producer.
        for c in consumers {
            if let Some(s) = specs.iter().find(|s| &s.id == c) {
                users.extend(s.writes.iter().map(|w| w.path.clone()));
            }
        }
        let mut writes = Vec::new();
        for p in &paths {
            writes.push((p.clone(), runner.read_base(p).await.ok().flatten()));
        }
        // A file the graph has not indexed still declares its names.
        let mut symbols = symbols;
        if symbols.is_empty() {
            for (_, content) in &writes {
                for name in content
                    .as_deref()
                    .map(derive_interface::declared_names)
                    .unwrap_or_default()
                {
                    if !symbols.contains(&name) {
                        symbols.push(name);
                    }
                }
            }
        }
        let mut found = Vec::new();
        for u in users {
            if let Some(text) = runner.read_base(&u).await.ok().flatten() {
                found.push((u, text));
            }
        }
        Ok(ProducerFacts {
            producer: spec.id.clone(),
            consumers: consumers.to_vec(),
            writes,
            symbols,
            users: found,
        })
    }

    /// Record every consumed edge of the final plan that no accepted interface
    /// binds, as a [`ContractEvent::Unchecked`] per consumer (counted one per
    /// edge in `not run`), with a notice that says so. `reasons` holds what the
    /// interface call found per edge; an edge it did not see (a split's,
    /// renamed) reads as having no interface.
    pub(super) async fn record_unchecked(
        &mut self,
        contracts: &[OpenContract],
        interfaces: &[InterfaceContract],
        reasons: &[UncheckedEdge],
    ) -> Result<(), OrchestratorError> {
        if self.contracts.is_none() {
            return Ok(());
        }
        let ids: BTreeSet<&str> = contracts.iter().map(|c| c.contract.as_str()).collect();
        let mut consumers: Vec<(String, Vec<String>)> = contracts
            .iter()
            .filter_map(|c| {
                let deps: Vec<String> = c
                    .depends_on
                    .iter()
                    .filter(|d| ids.contains(d.as_str()) && **d != c.contract)
                    .cloned()
                    .collect();
                (!deps.is_empty()).then(|| (c.contract.clone(), deps))
            })
            .collect();
        for e in reasons {
            if ids.contains(e.consumer.as_str()) && ids.contains(e.producer.as_str()) {
                add_edge(&mut consumers, &e.consumer, &e.producer);
            }
        }
        // A compile-only interface binds an edge for compilation only: the
        // edge's behavior check is the one that did not run.
        let behavior: Vec<InterfaceContract> = interfaces
            .iter()
            .filter(|i| !i.is_compile_only())
            .cloned()
            .collect();
        let open = uncovered(&consumers, &bound(&behavior));
        if open.is_empty() {
            return Ok(());
        }
        let detail = format!(
            "contract checks did not run for {}: no behavior interface kGraf could accept binds \
             them (a compile-only interface may), so they are counted as not run",
            interface_plan::describe_consumers(&open)
        );
        self.intake_notice(detail.clone());
        let at = self.now();
        self.trace(TraceEvent::Notice {
            at_ms: at,
            contract: open.first().map(|(c, _)| c.clone()).unwrap_or_default(),
            worker: String::new(),
            detail,
        })
        .await?;
        for (consumer, producers) in open {
            let mut why: Vec<String> = Vec::new();
            for p in &producers {
                let r = reasons
                    .iter()
                    .find(|e| e.consumer == consumer && &e.producer == p)
                    .map_or_else(
                        || "no interface binds them".to_owned(),
                        |e| e.reason.clone(),
                    );
                why.push(format!("{consumer} <- {p}: {r}"));
            }
            self.contract_event(ContractEvent::Unchecked {
                consumer,
                producers,
                reason: why.join("\n"),
            })
            .await?;
        }
        Ok(())
    }
}

/// The producer an interface declares among the contracts `ids`: the party
/// whose writes hold a stub file, failing that the first party named.
fn declared_producer<'a>(
    ic: &'a InterfaceContract,
    writes_of: &impl Fn(&str) -> Option<Vec<String>>,
    ids: &BTreeSet<&str>,
) -> Option<&'a String> {
    ic.parties
        .iter()
        .find(|p| writes_of(p).is_some_and(|w| ic.stub.iter().any(|f| w.contains(&f.path))))
        .or_else(|| ic.parties.first())
        .filter(|p| ids.contains(p.as_str()))
}

/// The notice for a refused interface whose producer kGraf decided needs no
/// behavior check ([`BehaviorPlan::Skip`]), `None` for any other. The planner
/// was not asked for it, so its refusal does not buy another ask of the
/// interface call (each one minutes on a local model); its edges keep the
/// skip reason as their unchecked reason.
fn unasked_refusal(
    ic: &InterfaceContract,
    problems: &[String],
    writes_of: &impl Fn(&str) -> Option<Vec<String>>,
    ids: &BTreeSet<&str>,
    derived: &[Derived],
) -> Option<String> {
    let producer = declared_producer(ic, writes_of, ids)?;
    let BehaviorPlan::Skip(why) = &derived.iter().find(|d| &d.producer == producer)?.behavior
    else {
        return None;
    };
    Some(kgraf_secrets::scrub_persist_owned(format!(
        "interface `{}` binds {producer}, whose behavior check kGraf does not ask for ({why}); \
         it was refused and is not asked for again: {}",
        ic.id,
        one_line(&problems.join("; "), 600)
    )))
}

/// The `(consumer, producer)` edges an interface declares among the
/// contracts `ids`: the producer is [`declared_producer`]; every other party
/// consumes it.
fn declared_edges(
    ic: &InterfaceContract,
    writes_of: &impl Fn(&str) -> Option<Vec<String>>,
    ids: &BTreeSet<&str>,
) -> Vec<(String, String)> {
    let Some(producer) = declared_producer(ic, writes_of, ids) else {
        return Vec::new();
    };
    ic.parties
        .iter()
        .filter(|p| *p != producer && ids.contains(p.as_str()))
        .map(|p| (p.clone(), producer.clone()))
        .collect()
}

/// Add the edge `consumer <- producer` to `edges`, once.
fn add_edge(edges: &mut Vec<(String, Vec<String>)>, consumer: &str, producer: &str) {
    match edges.iter_mut().find(|(c, _)| c == consumer) {
        Some((_, producers)) => {
            if !producers.iter().any(|p| p == producer) {
                producers.push(producer.to_owned());
            }
        }
        None => edges.push((consumer.to_owned(), vec![producer.to_owned()])),
    }
}

/// Whether two interfaces are one artifact under any id: the same parties,
/// files and commands. Files named by path alone are compared as filled in.
fn same_artifact(a: &InterfaceContract, b: &InterfaceContract) -> bool {
    a.parties == b.parties
        && a.symbols == b.symbols
        && a.stub == b.stub
        && a.reference == b.reference
        && a.wrong == b.wrong
        && a.compile == b.compile
        && a.test == b.test
        && a.consumer
            .iter()
            .map(|f| &f.path)
            .eq(b.consumer.iter().map(|f| &f.path))
        && a.tests
            .iter()
            .map(|f| &f.path)
            .eq(b.tests.iter().map(|f| &f.path))
}

/// `(id, parties)` of each interface.
fn bound(interfaces: &[InterfaceContract]) -> Vec<(String, Vec<String>)> {
    interfaces
        .iter()
        .map(|ic| (ic.id.clone(), ic.parties.clone()))
        .collect()
}

/// Whether `e` is an answer cut off at the output limit.
/// Whether the server could not parse the model's own output
/// ([`kgraf_provider::ProviderError::ModelOutputMalformed`]).
fn is_server_malformed(e: &PlanError) -> bool {
    match e {
        PlanError::Attributed { error, .. } => is_server_malformed(error),
        PlanError::Provider(p) => p.is_malformed_model_output(),
        _ => false,
    }
}

fn is_cut_off(e: &PlanError) -> bool {
    match e {
        PlanError::Attributed { error, .. } => is_cut_off(error),
        PlanError::Truncated { .. } => true,
        _ => false,
    }
}

/// `text` on one line, at most `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}...", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}

/// What kGraf found for one producer before any planner call.
struct Derived {
    producer: String,
    /// The plan's consumers of the producer.
    consumers: Vec<String>,
    /// The compile-only interface, when one was derived and its check passed:
    /// registered with the plan's interfaces.
    interface: Option<InterfaceContract>,
    /// What the compile check found, as a clause of an unchecked edge's reason.
    compile_note: String,
    /// What the producer's repository files give an answer: its stub, the
    /// consumer files, the names.
    base: Option<DerivedBase>,
    /// Whether the planner is asked for a behavior check.
    behavior: BehaviorPlan,
}

/// The stub, consumer files and names a behavior answer need not give.
#[derive(Clone)]
struct DerivedBase {
    stub: Vec<crate::checks::FileOverlay>,
    consumer: Vec<crate::checks::FileOverlay>,
    symbols: Vec<String>,
}

/// What [`RunLoop::take_interfaces`] found in one answer.
#[derive(Default)]
struct Taken {
    /// Refusals, each naming its interface.
    refusals: Vec<String>,
    /// The `(consumer, producer)` edges the refused interfaces declared.
    implied: Vec<(String, String)>,
    /// Positions in the answer's list of the interfaces refused.
    failed: Vec<usize>,
    /// The answer could not be read as a list of interfaces.
    unparsed: bool,
    /// Every interface refused and kept for the next ask, with its reasons.
    records: Vec<RefusedRecord>,
    /// The interfaces refused for the same reasons as their previous answer:
    /// not asked for again.
    repeated: Vec<Repeated>,
    /// Interfaces kept as compile-only after a repeated missing tests file.
    compile_only: Vec<InterfaceContract>,
}

/// One refused interface, as the next ask compares its answer with it.
#[derive(Clone)]
struct RefusedRecord {
    id: String,
    /// The `(consumer, producer)` edges it declared.
    edges: Vec<(String, String)>,
    /// Its refusals with the interface id taken out ([`normalized`]).
    reasons: BTreeSet<String>,
}

/// An interface the planner answered again with nothing new to refuse.
struct Repeated {
    edges: Vec<(String, String)>,
    /// The refusals, as the planner saw them.
    reason: String,
}

/// A refusal with the interface's id taken out, so the same cause is the
/// same text when the planner renames the interface between asks.
fn normalized(problem: &str, id: &str) -> String {
    let flat = problem.split_whitespace().collect::<Vec<_>>().join(" ");
    if id.is_empty() {
        flat
    } else {
        flat.replace(id, "<interface>")
    }
}

impl Taken {
    /// Take one refused interface. When its previous answer was refused for
    /// at least these reasons (the same interface: its id, or the same
    /// edges), asking again gives the planner nothing it has not been told:
    /// the interface is recorded as repeated and a notice is returned.
    /// Otherwise its reasons go to the next ask.
    /// Whether every reason of `problems` was already given for the same
    /// interface in an earlier ask: the same id, or an edge in common (the
    /// planner renames and regroups interfaces between asks). The reasons of
    /// every earlier ask count, not only the last.
    fn repeats(
        id: &str,
        edges: &[(String, String)],
        problems: &[String],
        previous: &[RefusedRecord],
    ) -> bool {
        let told: Vec<&RefusedRecord> = previous
            .iter()
            .filter(|r| r.id == id || edges.iter().any(|e| r.edges.contains(e)))
            .collect();
        !told.is_empty()
            && problems.iter().all(|p| {
                let key = normalized(p, id);
                told.iter().any(|r| r.reasons.contains(&key))
            })
    }

    fn refuse(
        &mut self,
        id: &str,
        edges: Vec<(String, String)>,
        problems: &[String],
        index: usize,
        previous: &[RefusedRecord],
    ) -> Option<String> {
        let reasons: BTreeSet<String> = problems.iter().map(|p| normalized(p, id)).collect();
        if Self::repeats(id, &edges, problems, previous) {
            let reason = kgraf_secrets::scrub_persist_owned(one_line(&problems.join("; "), 600));
            let note = kgraf_secrets::scrub_persist_owned(format!(
                "interface `{id}` was refused again for the same reason as its previous answer \
                 ({reason}); it is not asked for again, and its behavior check does not run"
            ));
            self.repeated.push(Repeated { edges, reason });
            return Some(note);
        }
        self.records.push(RefusedRecord {
            id: id.to_owned(),
            edges,
            reasons,
        });
        self.refusals.extend(problems.iter().cloned());
        self.failed.push(index);
        None
    }
}

/// The most names kGraf fills into an interface from a producer's files.
const MAX_FILLED_SYMBOLS: usize = 40;

/// The most repository test files read to decide where a test of one producer
/// alone can exist.
const MAX_TEST_FILES: usize = 60;

/// What the planner is shown of each producer it is asked a behavior check of.
fn supplied_for(derived: &[Derived], asked: &[(String, Vec<String>)]) -> Vec<Supplied> {
    let producers: BTreeSet<&str> = asked
        .iter()
        .flat_map(|(_, ps)| ps.iter().map(String::as_str))
        .collect();
    // Only when kGraf holds something for at least one: with nothing derived
    // the request is the plain one.
    if !derived
        .iter()
        .any(|d| producers.contains(d.producer.as_str()) && d.base.is_some())
    {
        return Vec::new();
    }
    derived
        .iter()
        .filter(|d| producers.contains(d.producer.as_str()))
        .map(|d| Supplied {
            producer: d.producer.clone(),
            consumers: d.consumers.clone(),
            stub: d.base.as_ref().map_or_else(Vec::new, |b| {
                b.stub.iter().map(|f| f.path.clone()).collect()
            }),
            consumer: d.base.as_ref().map_or_else(Vec::new, |b| {
                b.consumer.iter().map(|f| f.path.clone()).collect()
            }),
            symbols: d.base.as_ref().map_or_else(Vec::new, |b| b.symbols.clone()),
        })
        .collect()
}

/// The `Cargo.toml` of every directory above each of `files`, read from the
/// dispatch base (`None`: there is none).
async fn manifests_for(
    runner: &SandboxRunner,
    files: &[String],
) -> BTreeMap<String, Option<String>> {
    let mut manifests: BTreeMap<String, Option<String>> = BTreeMap::new();
    for file in files {
        for dir in derive_interface::ancestors(file) {
            let manifest = if dir.is_empty() {
                "Cargo.toml".to_owned()
            } else {
                format!("{dir}/Cargo.toml")
            };
            if manifests.contains_key(&manifest) {
                continue;
            }
            let text = runner.read_base(&manifest).await.ok().flatten();
            manifests.insert(manifest, text);
        }
    }
    manifests
}

/// What [`derive_interface::command_for`] needs to know of the base and the
/// machine for `files` ([`derive_interface::config_probes`]); `texts` gives
/// the content of files the caller already holds. Nothing is read for Rust
/// and Python files.
async fn build_facts_for(
    runner: &SandboxRunner,
    files: &[String],
    texts: &[(String, String)],
) -> derive_interface::BuildFacts {
    let (paths, dirs, programs) = derive_interface::config_probes(files);
    let mut build = derive_interface::BuildFacts::default();
    for p in paths {
        let has = runner.base_has(&p).await;
        build.present.insert(p, has);
    }
    for d in dirs {
        let names = runner.list_base(&d).await;
        build.listings.insert(d, names);
    }
    for p in programs {
        if runner.has_program(p) {
            build.programs.insert(p.to_owned());
        }
    }
    for f in files.iter().filter(|f| f.ends_with(".java")) {
        let text = match texts.iter().find(|(p, _)| p == f) {
            Some((_, t)) => Some(t.clone()),
            None => runner.read_base(f).await.ok().flatten(),
        };
        if let Some(t) = text {
            build.texts.insert(f.clone(), t);
        }
    }
    build
}

/// The Cargo package that holds `file`: the nearest manifest above it that
/// declares one.
fn package_in(manifests: &BTreeMap<String, Option<String>>, file: &str) -> Option<String> {
    derive_interface::ancestors(file)
        .into_iter()
        .find_map(|dir| {
            let manifest = if dir.is_empty() {
                "Cargo.toml".to_owned()
            } else {
                format!("{dir}/Cargo.toml")
            };
            manifests
                .get(&manifest)
                .and_then(|m| m.as_deref())
                .and_then(derive_interface::cargo_package)
        })
}

/// The compile-only interface kGraf derives for `facts`, with its compile
/// check run, what a behavior answer can be given, and what was found as a
/// clause for the record. An interface whose check fails is not returned.
async fn derive_one(
    runner: &SandboxRunner,
    facts: &ProducerFacts,
) -> (Option<InterfaceContract>, Option<DerivedBase>, String) {
    // Only a scaffold is given to a behavior answer as its stub: a test must
    // fail against the stub, and current code is not a stub any test fails
    // against.
    let (derived, source) = derive_interface::derived_stub(facts);
    let scaffold = |stub: Vec<crate::checks::FileOverlay>| match source {
        derive_interface::StubSource::Scaffold => stub,
        derive_interface::StubSource::CurrentCode => Vec::new(),
    };
    let stub = scaffold(derived);
    let files: Vec<String> = facts.users.iter().map(|(p, _)| p.clone()).collect();
    let manifests = manifests_for(runner, &files).await;
    let build = build_facts_for(runner, &files, &facts.users).await;
    let package = |f: &str| package_in(&manifests, f);
    let stub_only = |stub: Vec<crate::checks::FileOverlay>| {
        (!stub.is_empty()).then(|| DerivedBase {
            stub,
            consumer: Vec::new(),
            symbols: facts.symbols.clone(),
        })
    };
    let ic = match derive_interface::derive(
        facts,
        |files| derive_interface::command_for(files, package, &build).ok(),
        |files| match derive_interface::command_for(files, package, &build) {
            Err(why) => why,
            Ok(_) => derive_interface::no_command_reason(files, package),
        },
    ) {
        Err(why) => {
            return (
                None,
                stub_only(stub),
                format!("no compile check could be derived: {why}"),
            );
        }
        Ok(ic) => ic,
    };
    let files: Vec<&str> = ic
        .consumer
        .iter()
        .chain(&ic.tests)
        .map(|f| f.path.as_str())
        .collect();
    let what = format!(
        "kGraf derived a compile check from the repository (stub: the {} of {}; files: {}; \
         `{}`; {})",
        match source {
            derive_interface::StubSource::Scaffold => "scaffold",
            derive_interface::StubSource::CurrentCode => "current code",
        },
        ic.stub
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        files.join(", "),
        ic.compile.join(" "),
        derive_interface::coverage(&ic.compile)
    );
    match crate::checks::check_derived_compile(runner, &ic).await {
        Ok(Ok(runs)) => {
            let base = DerivedBase {
                stub: scaffold(ic.stub.clone()),
                consumer: ic.consumer.clone(),
                symbols: ic.symbols.clone(),
            };
            (
                Some(ic),
                Some(base),
                format!(
                    "{what}, and it passed ({} run(s): stub-compile and canaries); it is \
                     registered as a compile-only interface, and runs again against the \
                     producer's delivery before the delivery is accepted",
                    runs.len()
                ),
            )
        }
        Ok(Err(failure)) => (
            None,
            stub_only(stub),
            format!("{what}, and it failed: {failure}; it is not registered"),
        ),
        Err(e) => (
            None,
            stub_only(stub),
            format!("{what}, and it could not run: {e}; it is not registered"),
        ),
    }
}

/// The repository's test files, read from the dispatch base (at most
/// [`MAX_TEST_FILES`], each under 200 KB).
async fn repo_tests(
    runner: &SandboxRunner,
    known_files: &[String],
) -> Vec<derive_interface::TestFile> {
    let mut out = Vec::new();
    for path in known_files
        .iter()
        .filter(|f| derive_interface::is_test_path(f))
        .take(MAX_TEST_FILES)
    {
        if let Some(text) = runner.read_base(path).await.ok().flatten()
            && text.len() <= 200_000
        {
            out.push(derive_interface::TestFile {
                path: path.clone(),
                text,
            });
        }
    }
    out
}

/// What a test's reach needs to know of each contract of the plan: whether
/// its code exists yet, and the words that show a file uses it.
async fn contract_facts(
    runner: &SandboxRunner,
    specs: &[ContractSpec],
) -> Vec<derive_interface::ContractFacts> {
    let mut out = Vec::new();
    for spec in specs {
        let paths: Vec<String> = spec.writes.iter().map(|w| w.path.clone()).collect();
        let manifests = manifests_for(runner, &paths).await;
        let mut writes = Vec::new();
        for p in &paths {
            writes.push((p.clone(), runner.read_base(p).await.ok().flatten()));
        }
        let unimplemented = writes.iter().any(|(p, c)| {
            c.as_deref()
                .is_none_or(|c| derive_interface::is_scaffold(p, c))
        });
        out.push(derive_interface::ContractFacts {
            id: spec.id.clone(),
            depends_on: spec.depends_on.clone(),
            unimplemented,
            tokens: derive_interface::use_tokens(&writes, |f| package_in(&manifests, f)),
        });
    }
    out
}

/// `f` of every item, all in flight together, the results in the order of
/// the items. The derived compile checks of a plan are independent: each
/// compiles its own copy of the check base in the sandbox, which bounds how
/// many run at once (`check_runner::max_concurrent_checks`), so a plan of N
/// producers waits for the slowest rather than the sum.
async fn in_order_together<'a, T: 'a, R, Fut>(
    items: impl IntoIterator<Item = &'a T>,
    f: impl Fn(&'a T) -> Fut,
) -> Vec<R>
where
    Fut: std::future::Future<Output = R>,
{
    futures_util::future::join_all(items.into_iter().map(f)).await
}

#[cfg(test)]
mod together_tests {
    use super::in_order_together;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// Every call is in flight before any finishes, and the results come back in
    /// the order of the items whichever finishes first. Each call waits (up to a
    /// guard that is not part of the assertion) until all of them are in flight,
    /// so overlap is shown by a count, not by a clock. A loop that awaits one
    /// call at a time never gets past one in flight and fails the count.
    #[tokio::test]
    async fn every_check_is_in_flight_together_and_results_keep_their_order() {
        let items = [3u64, 1, 2];
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let out = in_order_together(items.iter(), |n| {
            let (in_flight, peak) = (&in_flight, &peak);
            async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let waited = Instant::now();
                while peak.load(Ordering::SeqCst) < items.len()
                    && waited.elapsed() < Duration::from_secs(3)
                {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                // The call that was given the largest number finishes last.
                tokio::time::sleep(Duration::from_millis(*n)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                *n * 10
            }
        })
        .await;
        assert_eq!(peak.load(Ordering::SeqCst), 3, "all three overlapped");
        assert_eq!(out, [30, 10, 20], "results follow the items");
    }

    #[tokio::test]
    async fn no_item_is_no_call() {
        let none: [u8; 0] = [];
        let out = in_order_together(none.iter(), |_| async { 1 }).await;
        assert!(out.is_empty());
    }
}

#[cfg(test)]
mod refusal_memory_tests {
    use super::*;

    fn edges() -> Vec<(String, String)> {
        vec![("c2".into(), "c1".into())]
    }

    fn no_content(id: &str, path: &str) -> String {
        format!("interface `{id}`: the consumer entry `{path}` gives no content")
    }

    fn first_ask() -> Vec<RefusedRecord> {
        let mut taken = Taken::default();
        let note = taken.refuse("i_a", edges(), &[no_content("i_a", "x.sh")], 0, &[]);
        assert!(note.is_none(), "nothing precedes the first ask");
        assert_eq!(taken.failed, vec![0]);
        taken.records
    }

    #[test]
    fn the_same_reason_under_a_renamed_interface_is_repeated() {
        let before = first_ask();
        let mut taken = Taken::default();
        let note = taken.refuse("i_b", edges(), &[no_content("i_b", "x.sh")], 0, &before);
        assert!(note.is_some_and(|n| n.contains("refused again")));
        assert_eq!(taken.repeated.len(), 1);
        assert!(taken.refusals.is_empty(), "a repeat buys no ask");
        assert!(taken.failed.is_empty(), "a repeat is not quoted back");
    }

    #[test]
    fn a_new_reason_or_a_new_path_is_asked_again() {
        let before = first_ask();
        for problem in [
            no_content("i_a", "y.sh"),
            "interface `i_a` failed its pre-dispatch check: compile".to_owned(),
            // One old reason and one new: the new one is information.
        ] {
            let mut taken = Taken::default();
            let note = taken.refuse("i_a", edges(), &[problem], 0, &before);
            assert!(note.is_none());
            assert_eq!(taken.refusals.len(), 1);
            assert!(taken.repeated.is_empty());
        }
        let mut taken = Taken::default();
        let both = [no_content("i_a", "x.sh"), no_content("i_a", "y.sh")];
        assert!(taken.refuse("i_a", edges(), &both, 0, &before).is_none());
        assert_eq!(taken.refusals.len(), 2);
    }

    #[test]
    fn a_reason_told_in_an_earlier_ask_is_repeated_under_a_new_name_and_edges() {
        let tests = |id: &str| interface_plan::no_tests_problem(id);
        let mut taken = Taken::default();
        let setup = vec![("c3".to_owned(), "c1".to_owned())];
        assert!(
            taken
                .refuse("i_a_c3", setup, &[tests("i_a_c3")], 0, &[])
                .is_none()
        );
        // Ask 2 refuses the same edge for another reason; only that one is the last ask's.
        let mut second = Taken::default();
        let setup = vec![("c3".to_owned(), "c1".to_owned())];
        let other = "interface `i_a_c3` has no symbols: name the items".to_owned();
        assert!(
            second
                .refuse("i_a_c3", setup, &[other], 0, &taken.records)
                .is_none()
        );
        let history: Vec<RefusedRecord> = taken.records.into_iter().chain(second.records).collect();
        // Ask 3 merges the edges under a new id and says the first reason again.
        let both = vec![
            ("c2".to_owned(), "c1".to_owned()),
            ("c3".to_owned(), "c1".to_owned()),
        ];
        assert!(Taken::repeats("i_a", &both, &[tests("i_a")], &history));
        assert!(!Taken::repeats(
            "i_a",
            &both,
            &[tests("i_a")],
            &history[1..]
        ));
    }

    #[test]
    fn another_interface_is_not_measured_against_it() {
        let before = first_ask();
        let mut taken = Taken::default();
        let other = vec![("c3".to_owned(), "c2".to_owned())];
        let note = taken.refuse("i_z", other, &[no_content("i_z", "x.sh")], 0, &before);
        assert!(note.is_none());
        assert_eq!(taken.refusals.len(), 1);
    }
}
