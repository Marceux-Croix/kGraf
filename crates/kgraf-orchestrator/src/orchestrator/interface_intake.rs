//! Plan intake: the contracts call, then the interface call (decision 0030,
//! amendment of 2026-10-02).
//!
//! ```text
//!   contracts ask --refused (parse, schema, rule)--> re-ask, at most MAX_PLAN_REASKS
//!   contracts ask --accepted--> checks off, or one contract --> done
//!                            --> interface ask
//!   interface ask --every interface accepted, every consumed edge bound--> done
//!                 --a refusal, or an edge left unbound--> re-ask with the previous
//!                   answer and the refusal; accepted interfaces are kept
//!                 --asks spent, or the answer cut off--> derive a compile check
//!                   from the repository for each unbound producer; every edge
//!                   still without an interface is recorded as unchecked
//! ```
//!
//! An omitted interface and a malformed one end the same way: both are asked
//! for again within the interface budget, then fall back. Neither holds the
//! run, and neither is skipped without a recorded, counted reason.

use super::*;
use crate::challenge::ContractEvent;
use crate::checks::{InterfaceContract, SandboxRunner};
use crate::derive_interface::{self, ProducerFacts};
use crate::interface_plan::{
    self, InterfaceDraft, InterfaceReply, InterfaceRequest, PreviousAsk, uncovered,
};
use crate::run_state::UncheckedEdge;

/// Asks of the interface call: the first, and two more with the previous
/// answer and its refusal.
pub(crate) const MAX_INTERFACE_ASKS: u32 = 3;

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
                Ok(Some(specs)) => {
                    // The plan against the repository as it is now (an
                    // unwritten scaffold file, a workspace its acceptance
                    // builds before its members exist). A split's children
                    // are not checked: earlier contracts' work is not in the
                    // checkout this reads.
                    if let Some(refusal) = self.plan_repo_refusal(&specs, &named_files, outer) {
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
        self.note_reshaped_unit_structs(&specs).await?;
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
        let out = tokio::select! {
            out = self.s.decomposer.decompose(request) => Some(out),
            () = stop.cancelled() => None,
        };
        let Some(out) = out else {
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
                Ok(Some(specs))
            }
            Err(e) => {
                self.settle_call(reservation, e.calls())?;
                Err(e.into())
            }
        }
    }

    /// The interface call, on its own budget of [`MAX_INTERFACE_ASKS`]. Each
    /// answer's interfaces are taken one by one ([`Self::take_interfaces`]):
    /// an accepted one is kept across re-asks, and a re-ask asks only for what
    /// is still refused or unbound, quoting the previous answer and every
    /// refusal. Consumed edges still unbound after the budget get a derived
    /// compile check where the repository allows one, and are returned as
    /// unchecked. `None`: a stop arrived during a call.
    async fn interface_asks(
        &mut self,
        context: PlanRequest,
        specs: &[ContractSpec],
        outer: &BTreeSet<String>,
    ) -> Result<Option<(Vec<InterfaceContract>, Vec<UncheckedEdge>)>, OrchestratorError> {
        let consumers = interface_plan::consumers(specs);
        // Edges a refused interface declared (its producer and each other
        // party): the planner said they consume, so they are counted with
        // the dependencies when nothing accepted binds them.
        let mut declared: Vec<(String, Vec<String>)> = Vec::new();
        let mut accepted: Vec<InterfaceContract> = Vec::new();
        let mut previous: Option<PreviousAsk> = None;
        let mut last_refusal: Option<String> = None;
        for ask in 1..=MAX_INTERFACE_ASKS {
            let request = InterfaceRequest {
                context: context.clone(),
                contracts: specs.to_vec(),
                outer: outer.iter().cloned().collect(),
                consumers: consumers.clone(),
                accepted: bound(&accepted),
                previous: previous.take(),
            };
            let (refusal, answer) = match self.interface_ask_once(&request).await {
                Ok(None) => return Ok(None),
                Ok(Some(reply)) => {
                    let (mut reasons, implied) = self
                        .take_interfaces(&reply.text, specs, outer, &mut accepted)
                        .await?;
                    for (consumer, producer) in implied {
                        add_edge(&mut declared, &consumer, &producer);
                    }
                    let open = uncovered(&consumers, &bound(&accepted));
                    if !open.is_empty() {
                        reasons.push(interface_plan::unbound_reason(&open));
                    }
                    if reasons.is_empty() {
                        // An earlier refusal stays the reason for any edge a
                        // refused interface declared and nothing bound.
                        break;
                    }
                    (reasons.join("\n"), Some(reply.text))
                }
                // A cut-off answer is not asked again (it would be cut off
                // again); it falls back like a spent budget.
                Err(OrchestratorError::Plan(e)) if !e.reaskable() && is_cut_off(&e) => {
                    (e.to_string(), None)
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
        let open = uncovered(&wanted, &bound(&accepted));
        let mut unchecked = Vec::new();
        if !open.is_empty() {
            let why = last_refusal.unwrap_or_else(|| "the planner wrote none".to_owned());
            let derived = self.derive_compile_checks(specs, &open).await?;
            for (consumer, producers) in &open {
                for producer in producers {
                    let found = derived
                        .iter()
                        .find(|(p, _)| p == producer)
                        .map_or("no compile check was derived", |(_, r)| r.as_str());
                    unchecked.push(UncheckedEdge {
                        consumer: consumer.clone(),
                        producer: producer.clone(),
                        reason: kgraf_secrets::scrub_persist_owned(format!(
                            "no interface the planner wrote was accepted ({}); {found}",
                            one_line(&why, 1_500)
                        )),
                    });
                }
            }
        }
        Ok(Some((accepted, unchecked)))
    }

    /// One ask of the interface call: reserved, ledgered and settled as the
    /// contracts call is. `None`: a stop arrived during the call.
    async fn interface_ask_once(
        &mut self,
        request: &InterfaceRequest,
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
        let basis = self.s.decomposer.cost_basis();
        let reservation = self.reserve_rounds(&basis, input, 0);
        let stop = self.stop_token.clone();
        let out = tokio::select! {
            out = self.s.decomposer.interfaces(request) => Some(out),
            () = stop.cancelled() => None,
        };
        let Some(out) = out else {
            self.stopping = true;
            self.settle_call(reservation, None)?;
            self.notices.insert(
                "the run was stopped during a planner call; the call may have billed and is \
                 listed by `kgraf unreconciled`"
                    .to_owned(),
            );
            return Ok(None);
        };
        match out {
            Ok(reply) => {
                self.settle_call(reservation, Some(&reply.calls))?;
                Ok(Some(reply))
            }
            Err(e) => {
                self.settle_call(reservation, e.calls())?;
                Err(e.into())
            }
        }
    }

    /// Take an interface answer: every interface that is well formed, binds
    /// contracts of the plan, names every file it needs and passes its
    /// mechanical checks joins `accepted`; each other one is refused with
    /// what it needs. Returns the refusals, each naming its interface, and the
    /// `(consumer, producer)` edges the refused interfaces declared.
    async fn take_interfaces(
        &mut self,
        text: &str,
        specs: &[ContractSpec],
        outer: &BTreeSet<String>,
        accepted: &mut Vec<InterfaceContract>,
    ) -> Result<(Vec<String>, Vec<(String, String)>), OrchestratorError> {
        let drafts = match crate::interface_plan::parse_interfaces(text) {
            Ok(d) => d,
            Err(e) => return Ok((vec![e.to_string()], Vec::new())),
        };
        let mut implied: Vec<(String, String)> = Vec::new();
        let ids: BTreeSet<&str> = specs.iter().map(|s| s.id.as_str()).collect();
        let writes_of = |p: &str| -> Option<Vec<String>> {
            specs
                .iter()
                .find(|s| s.id == p)
                .map(|s| s.writes.iter().map(|w| w.path.clone()).collect())
        };
        let mut refusals = Vec::new();
        for draft in drafts {
            let InterfaceDraft {
                interface,
                from_repository,
            } = draft;
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
                    implied.extend(declared_edges(&raw, &writes_of, &ids));
                    refusals.push(e.to_string());
                    continue;
                }
            };
            // A re-ask answer that repeats an accepted interface adds nothing.
            if accepted.iter().any(|a| same_artifact(a, &ic)) {
                continue;
            }
            if accepted.iter().any(|a| a.id == ic.id) {
                let base = ic.id.clone();
                let mut n = 2;
                while accepted.iter().any(|a| a.id == format!("{base}-{n}")) {
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
            problems.extend(interface_plan::shape_problems(&ic, writes_of));
            if problems.is_empty() {
                problems.extend(self.fill_from_repository(&mut ic, &from_repository).await);
            }
            if problems.is_empty() {
                problems.extend(self.undeclared_replacements(&ic).await);
            }
            if problems.is_empty()
                && let Some(runner) = self.checks_runner()
                && let Ok(Err(failure)) = crate::checks::check_mechanical(&runner, &ic).await
            {
                // A check that could not run says nothing about the interface:
                // it is taken, and the gate records the check as not run.
                problems.push(format!(
                    "interface `{}` failed its pre-dispatch check: {failure}{}",
                    ic.id,
                    check_guidance(failure.stage)
                ));
            }
            if problems.is_empty() {
                accepted.push(ic);
            } else {
                implied.extend(declared_edges(&ic, &writes_of, &ids));
                refusals.extend(problems);
            }
        }
        Ok((refusals, implied))
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
                     has no such file; a file named by path alone must already exist, so give a \
                     new file's content",
                    ic.id, file.path
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

    /// For each producer in `open`, a compile check derived from the
    /// repository ([`crate::derive_interface`]), run now: what it found, as a
    /// sentence for the unchecked edges' reason. Never holds the run.
    async fn derive_compile_checks(
        &mut self,
        specs: &[ContractSpec],
        open: &[(String, Vec<String>)],
    ) -> Result<Vec<(String, String)>, OrchestratorError> {
        let Some(runner) = self.checks_runner() else {
            return Ok(Vec::new());
        };
        let mut by_producer: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (consumer, producers) in open {
            for p in producers {
                by_producer
                    .entry(p.clone())
                    .or_default()
                    .push(consumer.clone());
            }
        }
        let mut out = Vec::new();
        for (producer, consumers) in by_producer {
            let Some(spec) = specs.iter().find(|s| s.id == producer) else {
                continue;
            };
            let facts = self
                .producer_facts(&runner, spec, &consumers, specs)
                .await?;
            let mut manifests: BTreeMap<String, Option<String>> = BTreeMap::new();
            for (path, _) in &facts.users {
                for dir in derive_interface::ancestors(path) {
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
            let package = |file: &str| {
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
            };
            let summary = match derive_interface::derive(&facts, |files| {
                derive_interface::compile_command(files, package)
            }) {
                Err(why) => format!("no compile check could be derived: {why}"),
                Ok(ic) => {
                    let files: Vec<&str> = ic
                        .consumer
                        .iter()
                        .chain(&ic.tests)
                        .map(|f| f.path.as_str())
                        .collect();
                    let what = format!(
                        "kGraf derived a compile check from the repository (stub: the scaffold \
                         of {}; files: {}; `{}`)",
                        ic.stub
                            .iter()
                            .map(|f| f.path.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        files.join(", "),
                        ic.compile.join(" ")
                    );
                    let verdict = match crate::checks::check_derived_compile(&runner, &ic).await {
                        Ok(Ok(runs)) => format!(
                            "it passed ({} run(s): stub-compile and canaries)",
                            runs.len()
                        ),
                        Ok(Err(failure)) => format!("it failed: {failure}"),
                        Err(e) => format!("it could not run: {e}"),
                    };
                    format!(
                        "{what}, and {verdict}; the vacuity check did not run: no reference \
                         implementation exists to pass a test"
                    )
                }
            };
            out.push((producer, summary));
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
        let open = uncovered(&consumers, &bound(interfaces));
        if open.is_empty() {
            return Ok(());
        }
        let detail = format!(
            "contract checks did not run for {}: no interface kGraf could accept binds them, \
             so they are counted as not run",
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

/// The `(consumer, producer)` edges an interface declares among the
/// contracts `ids`: the producer is the party whose writes hold a stub file,
/// failing that the first party named; every other party consumes it.
fn declared_edges(
    ic: &InterfaceContract,
    writes_of: &impl Fn(&str) -> Option<Vec<String>>,
    ids: &BTreeSet<&str>,
) -> Vec<(String, String)> {
    let producer = ic
        .parties
        .iter()
        .find(|p| writes_of(p).is_some_and(|w| ic.stub.iter().any(|f| w.contains(&f.path))))
        .or_else(|| ic.parties.first());
    let Some(producer) = producer.filter(|p| ids.contains(p.as_str())) else {
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
