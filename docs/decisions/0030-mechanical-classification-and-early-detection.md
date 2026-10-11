# Decision 0030: Amend versus supersede is decided mechanically, and three checks fire before the gate

**Status:** Accepted. Refines decision 0027's classification test and
decision 0029's detection path.
**Related:** decisions 0013, 0018, 0019, 0024, 0026, 0027, 0029

---

## Context

### Why classification should not be a judgment call

Decision 0027 stated the amend-versus-supersede test as additive with prior
work holding, against breaking with prior work wrong, and noted that without
a stated test the distinction collapses toward whichever answer is less
disruptive.

Stating the test does not remove that pressure. Deciding it mechanically
does.

Running the prior contract's tests against the proposed revision is exact
rather than heuristic. It costs one test run, and both halves already exist:
the test is already part of the contract, and the harness already discovers
the test runner.

Type-level compatibility is a solved problem outside this project, with
tooling that already classifies widening against narrowing, added optional
parameters, and removed variants. Where the language supports such a check,
it is a second gate.

A worker may claim an amendment. The orchestrator verifies before accepting.
That is what removes the pressure toward calling a breaking change an
amendment because supersession is costly.

### Why a third outcome is needed

Forcing every revision into amend or supersede hides a real case: the
interface is not repairable because the decomposition was wrong, and these
two parts should not have been separated this way.

Without a name, that case is recorded as a supersession and produces a
second contract with the same defect. Re-decomposing is more disruptive than
supersession and returns the work to the orchestrator's decomposition step
rather than to its contract-authoring step.

### Why three checks, and what each one catches

Decision 0029 gave agents a way to challenge a contract, which requires an
agent to notice. These checks fire earlier and require nobody to notice.

The vacuity check catches a contract whose test asserts nothing. If the test
passes against an empty implementation, it constrains no behavior and both
workers will satisfy it trivially while building incompatible things. This
is ordinary test-driven discipline applied to the contract, costs one run,
and happens before any worker is dispatched.

The stub-compile check catches structural mismatch between the two sides
before either writes real code. A trivial stub satisfying the producer
contract, compiled against the consumer's expectations, is cheap and exact.

The handshake is different in kind and is not a mechanism this project can
yet claim works.

### The handshake is a hypothesis with a published number attached

A published interference study found that a message of roughly 130 tokens
recovered 82 to 98 percent of the ground lost to blind parallel work.

That study's own evidence flags a limitation: the message carried
experimenter-injected ground truth, in a sequential arm. It establishes that
130 tokens of the right information fixes the problem. It does not establish
that agents can produce that information themselves.

A pre-implementation handshake is exactly the attempt to have agents
generate that message. Each worker states, in the shared conversation file,
what it understands it will deliver and consume, before writing
implementation.

This makes the open question testable against a published figure rather
than against intuition. It is adopted as an experiment, not as a mechanism,
and future measurement should report whether agent-generated handshakes
recover anything comparable.

## Decision

Classification is mechanical, not editorial. A proposed contract revision is
classified by running the prior contract's tests against it, plus a
type-compatibility check where the language supports one. Both passing means
amend. Either failing means supersede.

A third outcome exists: re-decompose. Some contracts are not fixable because
the split was wrong, not the interface.

Three checks fire before the gate, in order of earliness:

1. **Vacuity check.** The contract's test must fail against an empty
   implementation before dispatch.
2. **Stub-compile check.** Consumer expectations must compile against a
   trivial stub of the producer side.
3. **Pre-implementation handshake.** Each worker states what it will deliver
   and consume before writing anything.

The first two are mechanisms. The third is a hypothesis and is labeled as
one.

## Consequences

The orchestrator runs three checks before dispatching concurrent work, each
cheap: one test run, one compile, and a handshake exchange. These are
pre-flight costs paid against work that would otherwise be built wrong.

Re-decompose returns to an earlier step. Recording it distinctly means the
orchestrator's decomposition is revisited rather than its interface patched,
which matters because decomposition quality is a core competence.

Classification disputes become verifiable. A disagreement about whether a
revision is additive is settled by running a suite, not by argument.

Handshake efficacy is a future measurement, reported honestly whichever way
it comes out. A negative result is informative: it would mean the 82 to 98
percent recovery depends on information agents cannot generate, and that the
gains require an orchestrator that already knows the answer.

## Open questions

Whether the vacuity check can be fooled. A test that fails against an empty
implementation for the wrong reason, such as a compile error rather than an
assertion, would pass this check while still constraining nothing.

What the handshake should contain. The published 130-token message carried
ground truth. What an agent should state without it, at what length, is
unspecified, and getting this wrong is the most likely reason the experiment
returns null.

## Alternatives considered

**Leave classification to the orchestrator's judgment.** Rejected. It is the
pressure decision 0027 named, and a mechanical test removes it at negligible
cost.

**Two outcomes only.** Rejected. It forces a bad decomposition to be
recorded as a bad interface and reproduces the defect.

**Rely on challenges alone.** Retained, and insufficient by itself.
Challenges require an agent to notice; these checks require nobody to.

**Adopt the handshake as an established mechanism.** Rejected. The published
result is an upper bound obtained with injected ground truth, and treating
it as a proven capability would repeat an error already corrected elsewhere
in this project: recording that same figure as a bound rather than a
demonstration.

## Amendments (2026-09-30)

- **A check command that cannot run whatever the code is refused when the plan is read**, and
  the planner is asked again with the reason, instead of the interface failing its checks. Two
  shapes: a whole command line given as the program (`["cargo build --package x"]`, looked
  up as one program name), and an output outside the repository (an absolute path given to
  an output flag such as `-o /tmp/x`, or any path under a temporary directory), which the
  check sandbox refuses to write. An absolute path read elsewhere is allowed. The interface request now says an argv is the program and each
  argument as its own string, and that a command writes only inside the repository.

## Amendments (2026-09-30, corrections to the amendment above)

- **The command rules judge a new plan only.** An interface persisted before them is not
  invalidated when a run resumes; its check runs and reports what the command did. A missing
  program is still an incomplete artifact.
- **A whole command line is a program string with a shell operator, or one that splits into
  a program and at least one flag** (`cargo build --package x`). A program path with a space
  is a program.
- **`/dev/null` is allowed as an output.** An absolute path into the repository stays
  refused: a check runs in a scratch copy of the repository whose own directory is the only
  one it may write, so only a relative path works.

## Amendment (2026-10-01): consuming contracts need an interface, or the run says checks did not run

A live run's planner answered `"interfaces": null` although one contract consumed four others. The
run went on and no contract check ran; nothing said so.

With contract checks on, a plan in which any contract depends on another contract of the answer and
that holds no interface (null or empty) is refused back to the planner once, with "contracts c5
consume c1, c2, c3, c4; give an interface for each consumed contract". The plan is sent back for
this reason once, however many re-asks other refusals leave. If the answer still holds none, or the
re-ask budget is spent, the plan is taken and a notice, shown in the run output and traced, says
"contract checks did not run: the plan named no interfaces for c5 <- c1, c2, c3, c4". A run without
contract checks is unchanged. A dependency is the only signal of consumption read; a plan that
names another contract's symbols without a dependency is not detected.

Tests: `kgraf-orchestrator` `tests/contract_flow.rs`
`a_plan_whose_consumers_name_no_interface_is_sent_back_and_the_corrected_plan_is_checked`,
`a_plan_still_without_interfaces_after_one_ask_runs_with_a_loud_notice`,
`a_plan_with_dependencies_and_no_interfaces_is_taken_when_checks_are_off`; `plan.rs` unit tests.

## Amendment (2026-10-01): the planner is told the project's names, and four planning faults are closed

Paid benchmark runs showed four faults that blocked or misdirected contract checks.

1. **Consumer canary.** The canary-consumer compile run layered the broken consumer files and then
   the valid tests files. A consumer file and a tests file at one path (allowed when their content
   is the same) was restored by the tests overlay, the compile succeeded, and the check blamed the
   compile command. 21 runs across three benchmarks were blocked this way. The consumer canary now
   breaks only the consumer files that no tests file shares a path with, and applies the valid
   tests last. A shared file is broken by the test canary, which applies its canary last. When every
   consumer path is shared, the test canary covers them and no consumer canary runs.
2. **Names and signatures.** The planner never saw file contents, so it wrote crate names that were
   not the package's, trait methods with another parameter count and struct fields that did not
   exist. Every decomposition prompt now carries a section of at most 8 KB (`planner_context.rs`):
   package names from the manifests (the first 3 KB), then signatures, read through the code graph,
   of the files and symbols the instruction names. A trait, struct, enum or class is given as its
   declaration, at most 1.5 KB each, because its header alone names no method and no field. What
   does not fit is cut with a count, and a notice says so. A named file the graph holds no signature
   for is included as text up to the cap, with a notice (the whole-file read is the fallback, and it
   is noticed).
3. **Re-plan.** The re-plan's note quotes each failure as the check wrote it: stage, command and the
   stderr tail. The re-planned contracts' instructions end with the same text in a
   `held_plan_failures` data block, so a worker sees what was wrong with the plan it replaces.
4. **Placeholders.** A template slot (`<crate_name>`, `<package>`, `<your-name>`) in an interface
   command, or used as a path root or import target in an interface file, is refused at intake with
   the slot named, and the planner is asked again. Generics and markup in source are not slots. Like
   the command rules, this judges a new plan only; a persisted interface is not invalidated.

Tests: `kgraf-orchestrator` `tests/contract_checks.rs`
`a_consumer_file_shared_with_the_tests_still_gets_a_canary_and_a_sound_contract_passes` (real
runner); `tests/orchestrator.rs` `the_planner_prompt_carries_the_crate_name_and_the_trait_signature`;
`tests/contract_flow.rs` `a_plan_the_gate_holds_before_any_work_is_re_planned_once_with_the_failure`;
`plan.rs` `a_placeholder_in_a_command_or_an_interface_file_is_refused_with_a_reason`;
`planner_context.rs` unit tests.

## Amendment (2026-10-02): interfaces are a second planner call, and an unwritten interface is counted, never held

A live local-model run (`qwen3.6:35b`, 13 cases with contract checks on) ended with no
registered, checked interface in any case. The planner's answers, read one by one, showed why:

- One re-ask budget (two re-asks) was shared by parse errors, schema errors, a missing interface
  and a failed check, and each ask regenerated the whole plan (three to eight minutes on the local
  model) without the model seeing its previous answer.
- A refusal quoted the first 200 characters of the answer, not the place it broke (one parse error
  was at column 3,938).
- The answer was taken from the first `{` to the last `}`: a reasoning draft leaked before a
  literal `</think>` refused a sound plan.
- The prompt's example interface had empty file lists, and models copied them as empty shells; they
  put contract ids in `consumer`, gave commands as lists of lists, and named the repository's own
  test file by path, which the prompt did not allow.
- On the last ask an answer with no interface was taken with a notice, while a malformed one held
  the run: seven cases dropped their interfaces on the last ask.
- A re-plan was one ask with no re-ask, so a stray control character or an unknown id ended it.

**Two calls.** With contract checks on, the contracts call returns the contracts only; the prompt
says that interfaces follow and that `depends_on` must list every contract whose code a contract
uses. When the answer has two or more contracts, or is a split, the interface call
(`interface_plan.rs`, `orchestrator/interface_intake.rs`) is given the accepted contracts, the
`consumer <- producers` edges, and the same repository context, and returns
`{"interfaces":[...]}` only. Its system prompt is static and its varying text (the accepted
interfaces, the previous answer, the refusal) comes last, so each re-ask extends the previous
prompt. The contracts are never asked for again because an interface was wrong.

**Its own budget, and accumulation.** The interface call has three asks
(`MAX_INTERFACE_ASKS`). Each interface of an answer is taken on its own: sanitized, its parties
checked, its shape checked (`shape_problems`: symbols, stub, a reference covering every stub path,
at least one test, both commands, no path in both stub and consumer, every stub path a declared
write of a party), its consumer and tests files named by path alone read from the dispatch base
(`SandboxRunner::read_base`), a stub file the repository already has must be listed in `replaces`
(the stub land refuses it before dispatch otherwise, decision 0081), then its mechanical checks
run. An accepted interface is kept; a
re-ask quotes the previous answer and every refusal, names the accepted interfaces and the edges
still unbound, and asks only for those. A repeat of an accepted interface is ignored; a new one
under an accepted id is renamed, with a notice.

**One policy.** An omitted interface and a malformed one are retried the same way, within the
same budget. When the budget is spent, or the answer was cut off at the output limit, kGraf
derives a compile check from the repository for each producer of an unbound edge
(`derive_interface.rs`): the producer's declared files that hold a scaffold body (`todo!()`,
`unimplemented!()`, `NotImplementedError`) are the stub; the repository files that use its names
(graph edges, or the consumer contracts' own existing files; names read by keyword when the graph
has none) are the consumer and test files; `cargo test --no-run -p <package>` or a Python import
compiles them. Stub-compile and its canaries run (`check_derived_compile`); the vacuity check
cannot, because no reference exists. The derived check is not registered. Every consumed edge
that no accepted interface binds (a dependency, or a pair a refused interface declared) is then
recorded as `ContractEvent::Unchecked` (one per consumer, with the reason: the last refusal and the
derived check's result), counted one per edge in `checks_not_run` (the report's `not run`) and in
`unchecked_edges`, and said in a notice ("contract checks did not run for ..."). The book counts
an edge once however often the event is applied, so a replay and a resume agree. The run is never
held for an interface the planner could not write, and never skips one without a counted reason.
This replaces the 2026-10-01 rule (a plan with no interface was sent back once and then taken with
a notice) and the 2026-09-30 amendment of decision 0031 (the last ask's plan taken without its
mechanical checks, so the gate held on the same failure).

**Actionable refusals.** The answer is extracted leniently where the intent is certain
(`answer_json.rs`): text through the last `</think>` is skipped; the last top-level object that
parses is the answer, and a broken object before it is skipped, never mined for inner objects; a
raw control character inside a string is escaped and the object parsed again. A parse refusal
quotes the answer around the point the parser stopped, marked `[ERROR HERE]`, with the usual cause
(a string left unclosed, a key without quotes). A type error names the field, the value found and
what the field takes. Every interface file entry that is not a file is named at once
(`interfaces[0].consumer[0] is a string "c4", not a file; "consumer" takes files ..., never
contract ids: list the consuming contracts in "parties"`). A bare path string in `consumer` or
`tests` is read as the path-alone entry. In the re-ask, a `<` that could open or close a tag is
escaped; `c5 <- c1` stays as written.

**Schema.** The interface request shows one populated example entry (stub, reference, a consumer
named by path alone, a test, both commands), says that `stub` and `reference` always give content,
that a consumer or test file the repository already has may be named by path alone, and that a
path is never in both stub and consumer.

**One file, one test.** Contracts that declare one identical write set which the verifier's
narrowed check (`Verifier::plan_check_scope`, the `scoped_check` rule) judges by one test file
cannot be verified apart: the second contract of the long-run case was refused because the first
one's work already passed the test. The plan is refused once naming them and the two fixes (merge,
or split the file); if the planner repeats it, the contracts are merged into the first (`merge_groups`:
instructions as parts, dependencies renamed), with a notice.

Known limits. A dependency is still the only signal of consumption besides the pairs a refused
interface declares, so a dependency kept only for ordering is counted as an unchecked edge. The
derived compile check proves the consumers build against the scaffold, not that any test tells a
wrong producer from a right one. A crash between the two calls asks for the contracts again.

The planner answers the tests replay, verbatim, are in
`crates/kgraf-orchestrator/tests/fixtures/planner-answers/`.

Tests: `kgraf-orchestrator` `tests/plan_intake.rs`
`a_draft_leaked_before_think_end_does_not_cost_an_ask`,
`contracts_sharing_one_file_and_one_test_are_refused_then_merged`,
`contracts_sharing_a_file_under_a_whole_check_are_left_alone`; `tests/contract_flow.rs`
`a_recorded_answer_with_contract_ids_in_consumer_is_refused_naming_the_fix`,
`a_recorded_answer_of_empty_shells_is_refused_with_what_each_needs`,
`a_test_file_named_by_path_alone_is_read_from_the_repository`,
`with_no_interface_a_compile_check_is_derived_from_the_scaffold`,
`unchecked_edges_count_the_same_after_a_resume`,
`after_three_asks_a_failing_interface_is_counted_unchecked_and_the_run_goes_on`,
`a_plan_still_without_interfaces_after_the_budget_runs_counted_and_noticed`,
`an_interface_naming_a_contract_the_plan_lacks_is_asked_again_then_counted`,
`a_stub_over_an_existing_file_is_asked_to_declare_it_in_replaces`; unit tests in
`answer_json.rs`, `interface_plan.rs`, `derive_interface.rs`, `plan.rs`
(`contracts_sharing_one_write_set_and_one_test_are_grouped`) and `challenge.rs`
(`an_unchecked_edge_counts_once_per_edge_however_often_it_is_applied`); `kgraf-host`
`the_plan_scope_of_a_write_set_is_its_own_test_file`.

Renamed tests, now checking the behavior above:
`a_plan_still_without_interfaces_after_one_ask_runs_with_a_loud_notice` is
`a_plan_still_without_interfaces_after_the_budget_runs_counted_and_noticed`;
`an_interface_naming_a_contract_the_plan_lacks_is_refused` is
`an_interface_naming_a_contract_the_plan_lacks_is_asked_again_then_counted`;
`a_dependency_on_an_interface_id_is_refused_by_name` is
`a_dependency_outside_the_plan_is_refused_by_name` (the contracts answer holds no interface ids
now).

## Amendment (2026-10-02): the plan is checked against the repository, and a plan-shape hold says so

- **A named scaffold file that no contract writes is refused.** A file the instruction names
  (by path, or through an item in it that the planner context shows) that holds placeholder
  bodies (`todo!()`, `unimplemented!()`, `raise NotImplementedError`, on a line that is code)
  and is in no contract's writes is refused with the file named and the fix. The work it
  stands for would be done by nobody, or a worker would meet it outside its scope and the
  contract would be held.
- **A workspace acceptance before its members exist is refused.** A contract whose acceptance
  runs `cargo build|test|check|clippy` with `--workspace` or `--all` is refused when the
  workspace's own `Cargo.toml`, or a member's manifest, is neither in the repository nor
  written by that contract or one it depends on. The members come from the manifest in the
  repository, or, for a root manifest the plan creates, from the member list in the
  instruction. A member written only by another contract that this one does not depend on is
  the deadlock the check exists for: the build fails on the missing manifest and the contract
  that creates it never runs first. This is broader than "written by no contract". Both
  refusals are re-askable and are not applied to a split's children, whose earlier work is not
  in the checkout the check reads. The reasons name the file and suggest the fix.
- **A hold for a file no contract writes asks for a re-plan.** When a worker needs a file that
  no open contract writes, and nobody attends the run, the hold says that is a plan-shape
  error and that the plan needs a contract that lists the file. It no longer suggests
  `--allow-path`, which would let one attempt write a file the plan never assigned. Attended
  runs still ask, and a file another contract writes, or a command, keeps the permission wording.

Tests: `kgraf-orchestrator` `plan_repo.rs` unit tests, `tests/plan_repo_intake.rs`,
`planner_context.rs` `the_named_files_are_the_paths_the_section_shows`, `shortfall.rs`
`a_hold_for_a_file_no_contract_writes_asks_for_a_replan`.

## Amendment (2026-10-02, second): derive first, ask the planner only for judgment

A live local-model run (13 cases with contract checks on) spent three asks of the interface call in every case, 287 to 795 s of local planner time per attempt, and every
ask was refused. D1, for one: asks of 144, 123 and 166 s (433 s in all, against 225 s for the
contracts call), refused for a stub over a file that exists and was not in `replaces`, for JSON
that did not parse (line 6, column 75), and for a compile command that does not build the test
files. Most refusals in the other cases were the vacuity check, "the test fails against the
reference, so nothing may satisfy it": the benchmark's only test (`checkout/tests/integration.rs`)
calls the outermost contract and so needs all six producers, so no reference of one producer can
pass it. No wording of the request fixes that; the check was asking for something that cannot exist.
A refusal text reached 28 KB, mostly one `xcrun` warning block per crate and the same output once per
interface. The mechanical fallback then derived a compile check for each producer, ran it, and all
passed, but the result was recorded in a reason string and registered nowhere: the dispatch gate and
the counters never saw it.

**Derive first, at no model cost.** Before the interface call, for every consumed edge kGraf derives
the stub (the producer's declared files that hold a scaffold body), the consumer files (repository
files that use the producer's names: graph edges and the consumers' own files) and the compile
command (`derive_interface::compile_command`, from the manifests), and runs stub-compile and its
canaries. A derived check that passes is **registered as an interface of kind compile-only**
(`InterfaceKind::CompileOnly`, id `derived-<producer>`): it has no reference, no test and no test
command, its one check is the compile half, and it is event-sourced like any interface
(`ContractEvent::Registered`), so the dispatch gate checks it and `compile_only_registered` counts it
apart. Its gate costs no model call: no handshake (signatures are checked by compiling), no consumer
test (no reference to gate one against), no stub land (the stub is the scaffold already in the
repository). A derived check that fails is not registered; its failure is the reason in the edge's
unchecked record. A revision of a compile-only interface is classified on compatibility alone
(`classify`: the prior consumers compile against the revised stub).

**Ask only for judgment.** A behavior check (a reference and a small test of one producer alone) is
asked of the planner for a producer only when (a) no repository test already exercises that producer
alone and (b) a single-producer test can exist (`derive_interface::behavior_plan`). A test reaches a
contract when it uses the contract's crate or module name or a distinctive item name, and reaches
everything that contract depends on. Over the tests that reach the producer: none means ask; one
that reaches no other unimplemented contract means the repository has the test and the verifier runs
it, so no ask; all of them reaching other unimplemented contracts (a scaffold body, or a file not
written yet) means a reference of the producer alone cannot pass any of them, so no ask. In both
no-ask cases the edge's behavior check is recorded as not run with a one-line reason
(`ContractEvent::Unchecked`, counted in `not run` and `unchecked_edges`), and the compile-only
interface stays. When no producer needs a behavior check the interface call makes no planner call at
all. A plan with no consumed edge is asked once, as before.

**What the ask holds.** The request names, per producer, the stub, the consumer files and the names
kGraf supplies, and says the answer holds only `id`, `parties`, `reference`, `tests` and `test`
(with `stub` and `compile` too where no scaffold exists). kGraf fills the stub (its paths declared in
`replaces`), the consumer files, the names and the compile command into the interface before it
checks it. The answer is limited to 4,096 output tokens, stated in the request and enforced on the
call; an answer cut off falls back like a spent budget.

**Narrow re-asks, mechanical repairs.** A re-ask covers only the interfaces still refused or
unbound: the previous answer quoted is those interfaces' entries alone (`answer_subset`), with their
refusals; an answer that did not parse is quoted whole, and the refusals quoted are cut at 12,000
characters, saying how much was left out. A compile command the sandbox cannot run (a
whole command line in one string, an output path outside the tree) or that fails the stub-compile
stage is replaced by the derived command and the checks run again, with a notice, and no re-ask.

**Host noise out of check output.** `filter_host_noise` drops macOS `xcrun`, `xcodebuild`,
`DVTDeveloperPaths`, `DVTFilePathFSEvents` and `MacOSX.sdk` lines (a warning block about them whole)
and repeated identical warning blocks, before the output is cut to its byte budget, so the noise
takes no room from the errors and never reaches a model, the trace or a notice. It says how many
lines it removed. An error line is never removed.

**Measure.** Each interface ask is a `ContractEvent::InterfaceAsked` (calls, wall-clock
milliseconds and output tokens, read from the ledger), counted in `ContractCounters`
(`interface_asks`, `interface_ask_ms`, `interface_ask_output_tokens`), with `compile_only_registered`.
The report prints `interfaces  planner N calls S s T tokens; accepted C compile-only, B behavior`.
Seconds and tokens per accepted interface follow. The `contracts` line keeps its form, and its
`interfaces` count includes the compile-only ones.

Result on the benchmark's shape (the test `the_order_pricing_workspace_gets_compile_only_interfaces_and_no_interface_call`
over the recorded contracts answer): five compile-only interfaces registered, zero interface calls,
about 12 s of checks, against three refused asks and 433 s on D1.

Known limits. Reach is read from names: a test that reaches a contract through a name kGraf does not
see (reflection, a macro) is read as not reaching it, and the planner is asked, as before. The
compile-only interface proves the consumers build against the scaffold, not that any test tells a
wrong producer from a right one, and a compile-only interface has no handshake: the parties' prose
assumptions about those edges are not compared. The derive step runs one producer at a time.

Tests: `kgraf-orchestrator` `tests/contract_flow.rs`
`the_order_pricing_workspace_gets_compile_only_interfaces_and_no_interface_call`,
`a_producer_no_test_reaches_is_asked_once_for_a_reference_and_a_test_only`,
`a_producer_a_repository_test_exercises_alone_is_not_asked_about`,
`a_compile_command_that_does_not_build_the_files_is_repaired_without_a_re_ask`,
`a_command_that_cannot_run_in_the_sandbox_is_replaced_not_refused`,
`a_re_ask_covers_only_the_refused_interface`,
`a_compile_only_interface_costs_no_handshake_and_lands_no_stub`,
`the_interface_economy_counters_are_the_same_after_a_resume`,
`a_run_stopped_inside_the_interface_call_counts_each_ask_once_after_a_resume`,
`an_ask_that_fails_is_counted_and_not_asked_again`,
`an_answer_naming_the_consumer_first_still_gets_the_producers_stub`,
`with_no_interface_a_compile_check_is_derived_from_the_scaffold`; `tests/contract_checks.rs`
`a_compile_only_interface_is_checked_by_compiling_alone`,
`a_compile_only_revision_is_classified_by_compatibility_alone`; unit tests in `derive_interface.rs`
(`behavior_plan`, `use_tokens`), `interface_plan.rs` (`answer_subset`, the supplied request, the refusal cut), `plan.rs`
(`an_interface_ask_is_limited_to_its_stated_output_cap`),
`challenge.rs` (`interface_asks_and_compile_only_registrations_are_counted_from_events`),
`check_runner.rs` (`host_toolchain_noise_is_removed_from_a_recorded_refusal_and_the_failure_stays`,
`error_lines_stay_and_identical_warning_blocks_are_said_once`,
`the_tail_a_model_reads_carries_no_host_noise`) and `kgraf-cli`
(`the_printed_report_states_the_interface_calls_cost_and_yield`). The recorded contracts answer is
`tests/fixtures/planner-answers/d1-contracts-order-pricing.txt`; the recorded 5.7 KB refusal is
`tests/fixtures/check-output/vacuity-refusal-with-xcrun-noise.txt`.

## Amendment (2026-10-02, third): a check command names only files the check can see, and `not run` is two counts

Two faults seen in live local-model runs.

**A script nobody supplied.** A planner-written interface gave the compile command
`["bash", "-n", "serve_local.sh"]`. No file `serve_local.sh` was in the interface (stub,
consumer, tests) or in the repository; a contract in the plan might write it later, but the check
runs before any contract is dispatched. `bash` exited 127 with `bash: serve_local.sh: No such file
or directory`; the runner read exit 127 as a missing program, so the interface was taken and both
parties were held with "the check program `serve_local.sh` is not installed ...: contracts in its
language cannot be checked on this machine". Both halves were wrong: the program was installed,
the language was fine, and the interface was refusable at intake.

The rule: a compile or test command may read only files that exist where the check runs, which is
the repository at the dispatch base with the interface's own files laid over it. Two mechanisms.
- At intake, `InterfaceContract::unsupplied_command_files` lists the arguments that name a script or
  source file (a relative path of plain path characters ending in a known script or source extension;
  not a flag, an output after `-o`, an absolute or parent path, or quoted program text) and that the
  interface does not supply. `missing_command_files` looks each up at the dispatch base and refuses the
  interface when none exists, naming the file and, when a contract's writes include it, that this
  contract writes it only after the check. The planner is asked again (one of the three asks).
- At run time, when a program that is installed fails with exit 127 and the missing name is one of its
  own arguments with "No such file", the error is `CheckError::InputMissing` (the file named, the
  program said to be installed), not `ProgramMissing`. It is still a check that could not run, held
  and counted in `checks_not_run`, so an interface persisted before this rule reads true on resume.

**Not run is two counts.** `checks_not_run`, the report's `not run`, counts each `Unchecked` edge and
each `CheckNotRun` event, both event-sourced (`challenge.rs`, `apply`). A report can say `not run 1`
with no unchecked edge: one interface whose check could not run. The scenario canary compared the
counter with the unchecked edges alone and read that as a disagreement; it now compares it with
unchecked edges plus `check_not_run` events.

**An edge with nothing to derive.** A producer that edits an existing file holding no scaffold has no
stub to stand in, so no compile check can be derived for the edges that consume it. kGraf records each
as `unchecked` with the cause ("no file P writes holds a scaffold (a placeholder body) to stand in as
its stub") and the skipped behavior check, and counts it in `not run`. That is correct behavior. The
scenario canary treats it as honest only when the reason says the behavior check was skipped on
purpose and states a cause; a derived check that failed or could not run, a bare "could not be
derived", and an edge with no record are still defects.

Tests: `kgraf-orchestrator` `checks.rs` (`a_script_argument_is_a_file_the_command_reads`,
`a_command_file_the_interface_does_not_supply_is_listed`), `tests/contract_checks.rs`
`a_script_argument_the_tree_lacks_is_a_missing_file_not_a_missing_program`, `tests/contract_flow.rs`
`a_check_command_naming_a_file_nobody_supplies_is_asked_again`.

## Amendment (2026-10-02, fourth): a placeholder is found only in code

A run on a repository whose source names `todo!()` in prose inside string literals was
refused: the scaffold refusal read files line by line and skipped only lines that start as a
comment, so it took the prose for a placeholder body, and every re-ask was refused the same
way. The scaffold test of the derived interface matched plain substrings and had the same
defect.

- **One rule, in code only.** Every check that asks whether a file holds a placeholder body
  reads `placeholder.rs`: the scaffold refusal above (`plan_repo.rs`), the scaffold that
  stands in as a derived stub and the "code does not exist yet" fact that `behavior_plan`
  reads (`derive_interface::is_scaffold`), and the untouched-placeholder refusal of decision
  0086 (`stub_land::is_placeholder_source`, which now shares the lexer). The file is lexed
  with the tree-sitter grammar of its language (Rust, Python, TypeScript, JavaScript, Go,
  Java, C, C++, Ruby, C#, PHP; the grammars the code graph already compiles), every comment
  and every string, raw string, byte string, character, template, heredoc and regex literal
  is removed, and only the code left is read. A placeholder is a `todo!` or `unimplemented!`
  call (any delimiter, a path such as `std::todo!` allowed, `my_todo!` not one), `raise` or
  `fail` of `NotImplementedError`, or a literal that says "not implemented" as the message of
  a `raise`, `throw`, `panic` or `fail` on the same statement line.
- **What changes.** A file that names `todo!()` only in a string literal, raw string, doc
  comment or comment is no longer refused and is no longer a scaffold. `except
  NotImplementedError` is not a raise. A "not implemented" comment or a string that is not
  raised is not a placeholder. The template-slot check (`<crate_name>`) judges an interface
  file's code the same way, so a slot in a comment or a string is not refused. Every form a
  real placeholder body took before is still found.
- **Limits.** A language with no grammar is read line by line, skipping lines that start as a
  comment; that reader cannot tell a string from code. A prose file (Markdown, text) holds no
  placeholder. A lex costs about 6 ms for a 34 KB Rust file and about 50 ms for a 397 KB file
  in a release build; it runs only on named files, producer writes and untouched writes.
- **The refusal stays re-askable.** It is a `PlanError::Invalid`; the planner is asked again
  with the reason, up to the plan ask limit.

Tests: `kgraf-orchestrator` `placeholder.rs` unit tests (every grammar, Rust and Python forms,
raised messages, the line reader, prose files),
`plan_repo::tests::a_placeholder_named_only_in_strings_and_comments_is_not_refused`,
`derive_interface::tests::a_file_that_names_a_placeholder_only_in_text_is_not_a_scaffold`,
`plan::tests::a_placeholder_in_a_command_or_an_interface_file_is_refused_with_a_reason`, the
`stub_land` masking tests, and `tests/plan_repo_intake.rs`
`a_named_file_that_mentions_a_placeholder_only_in_text_is_not_refused`.

## Amendment (2026-10-06): a surface of files is anchored in the handshake

A live run that built a Python package was held before any work started. The plan registered an
interface whose symbols are files: `pyproject.toml` and `vectorops/__init__.py`, with those files
as its stub and reference, both written by the producer. Each party answered the handshake and
named the files in backticks: the producer "I will provide `pyproject.toml` defining package
metadata ...", the consumer "I assume you provide `pyproject.toml` ...". The comparator set every
path anchor aside before it looked at the surface (decision 0078, rule 3), so no sentence had a
subject: each party was `no_claims` and each file `symbol_unprovided`, four mismatches by
construction. The other two were `touch_outside_scope`: the consumer listed files it only reads
under `touch`, and the prompt gave the key with no meaning.

- **A path that is a surface symbol names it** (`resolve_path`). A path anchor is compared with the
  surface first; a path no surface symbol names is still set aside. A leading `./` or `/` is not
  part of the path.
- **A file is provided by the party that writes that stub file** (`stub_writers`). Before, the
  symbol's last `.` segment (`toml`, `py`) was looked up as a word in the stub files, so a stub
  file holding `import toml` made its writer the provider of `pyproject.toml`.
- **A touch is one file however it is spelled**: the scope test and the overlap compare the same
  normalized path, so `/a` from the party that writes `a` is in scope, and `./a` against `/a` from
  two parties that may run at once is an overlap.
- **The prompt says what `touch` is**: "`touch` lists only the paths you will write, from your
  declared writes; a file you only read is not one you touch."

Replayed on the recorded statements, the handshake finds no `no_claims` and no
`symbol_unprovided`; it holds on the two `touch_outside_scope` findings, which are true of the
statements as given. A party that names no surface symbol is still `no_claims` and holds.

The same run's first plan had two edges with nothing to derive after its interface call was refused
at its last ask; kGraf recorded both as `unchecked` with the asked reason and the cause of the
missing compile check, counted in `not run`. That is correct behavior. The scenario canary now reads
that case as honest too, only after the last ask and only with the cause stated.

Tests: `kgraf-orchestrator` `handshake_diagnosis_tests.rs`
`a_surface_of_files_binds_its_claims_and_only_real_findings_hold` (the recorded handshake, verbatim,
as `tests/fixtures/handshake/files-as-contract-symbols.json`),
`a_file_on_the_surface_is_provided_by_its_writer`, `a_surface_of_files_still_holds_real_disagreements`,
`a_path_resolves_only_to_a_path_on_the_surface`, `a_touch_is_one_file_however_it_is_spelled`,
`the_handshake_prompt_defines_touch_as_writes`.

## Amendment (2026-10-06): every cargo command in a workspace loads every member

The workspace check of the 2026-10-02 amendment looked only at `--workspace` and `--all`, and
its remedy told the planner to use `cargo test -p <package>`. Cargo loads the manifest of every
workspace member before it runs any command, so that remedy fails the same way. A plan whose
root-manifest contract accepted on `cargo test -p <member>`, while another member's manifest
came from a contract that depends on it, was accepted and could never pass. Its worker asked
for the missing manifest and was told to continue, since its own check judges it.

- **Measured with cargo 1.95.** With a root `Cargo.toml` that lists a member whose directory
  has no `Cargo.toml`, all of these fail with "failed to load manifest for workspace member":
  `cargo test -p <other>`, `cargo test --manifest-path <other>/Cargo.toml`, `cd <other> &&
  cargo test`, a bare `cargo test` at the root, `cargo fmt --all -- --check` (through `cargo
  metadata`), and `cargo metadata`. An empty member directory, a glob member (`crates/*`) with
  a directory that has no manifest, and `default-members` that leave the member out fail the
  same way. A member manifest with no target (no `src/lib.rs`, no `src/main.rs`, no target
  section) fails with "no targets specified in the manifest". These pass: the root contract
  also writes `<member>/Cargo.toml` (a `[package]` with name, version and edition) and
  `<member>/src/main.rs` or `src/lib.rs`; the root lists only the members that exist, and a
  later contract adds its member to the list; a package built from its own manifest with no
  workspace manifest above it.
- **The check reads every cargo command an acceptance runs.** Each subcommand that loads the
  workspace (`build`, `test`, `check`, `clippy`, `run`, `bench`, `doc`, `fmt`, `metadata`,
  `tree`, `fix` and the like) is found in the acceptance, also inside prose, with the
  directory it runs in (after `cd`, `-C`, or the directory of `--manifest-path`; a directory
  outside the repository or unknown is left out). The workspace is the nearest manifest
  above that directory that has members: one the plan writes, or the repository's root
  `Cargo.toml`. The member list is the one the contract sees: from the instruction of the
  last contract among it and its dependencies that writes the manifest, else from the
  repository's file, with `exclude` honored. A `<prefix>/*` member counts every directory
  under the prefix that the contract or its dependencies write into.
- **Refused:** a member manifest that is neither in the repository nor written by the
  contract or one it depends on; a member manifest the plan writes when no Rust source under
  that member is in the repository or written in time; a command at a workspace's root when
  the workspace manifest comes from a contract that runs later. The reason names the
  command, the file and the measured remedies. "Make it depend on the contract that writes
  it" is offered only when that contract does not depend on this one, so the edge makes no
  cycle.
- **A worker that meets this shape asks for a re-plan.** When a worker reports a file
  shortfall (or an instructions report naming the file) for a file its contract's cargo
  acceptance cannot pass without, and only an open contract that runs later writes it, the
  contract is held as a plan-shape error that asks for a re-plan. The answer "do not edit it;
  your attempt is judged by its own acceptance check" is no longer given for such a file, as
  that check cannot pass. A file no open contract writes keeps the existing plan-shape hold.
  As with every hold, the attempt's in-scope result is verified first.
- **Other workspace tools.** uv 0.11 workspaces skip a listed member that has no
  `pyproject.toml`, and npm 11 runs with a listed workspace directory missing, so neither
  has this failure and neither is checked. Go workspaces (`go.work`) are not checked.
- **Limits.** The command reader is not a shell: quotes only end words, and variables are not
  expanded. A member package with its own `[workspace]` table, or `package.workspace`, is
  read as a member of the workspace above it.

Tests: `kgraf-orchestrator` `plan_repo.rs` unit tests (every cargo form, the remedies, a
manifest with no source, glob and exclude members, a complete workspace not refused, the
re-plan hold, and `the_check_agrees_with_cargo`, which runs cargo over each plan shape and
requires the check's verdict on every contract to match cargo's),
`tests/plan_repo_intake.rs`
`the_package_form_before_its_members_exist_is_refused_then_the_named_remedy_is_accepted`, and
`kgraf-host` `a_member_manifest_a_later_contract_writes_holds_for_a_replan`.

## Amendment (2026-10-06, second): a member list in prose is read

The workspace check above read a member list only in the form `members = [...]`. A planner
that stated the list in prose ("the workspace root Cargo.toml with members wcl-core and wcl")
was read as listing no member, so the plan was accepted and its root contract failed on a
later contract's missing member manifest ("failed to load manifest for workspace member
`wcl`"). Its retry dropped the member from the list, which the later contract could not add
back, as the root manifest was not in its writes.

- **A member list in prose is read.** When the root contract's instruction has no
  `members = [...]` list, the words after `member` or `members` are read to the end of their
  sentence (a period before a space, a semicolon, a parenthesis or a line break), or to a word
  that ends the list (`not`, `except`, `until`, `later`, `but` and the like). A word counts
  only when the plan writes, or the repository holds, `<word>/Cargo.toml`, so prose such as
  "the members listed below" names none. A `members = [...]` list wins over prose.
- **Why that guard.** A word that is no member directory cannot make cargo fail, so reading
  it would only refuse a plan for nothing. A member the instruction names in prose but that no
  contract writes and the repository lacks is not read; that plan fails at its acceptance
  check, as before.
- **The re-plan hold reads it too**, since it uses the same findings.

Tests: `kgraf-orchestrator` `plan_repo.rs`
`a_member_list_in_prose_is_read_and_refused_like_a_toml_list`,
`prose_members_count_only_member_directories_and_stop_at_the_sentence`,
`a_toml_member_list_wins_over_prose`.

## Amendment (2026-10-06, third): a producer that edits existing code is checked against its current code

The derive step took the stub from the producer's scaffold only. A producer contract that
edits existing code (its files hold no placeholder body) got no derived check: its consumed
edge was recorded unchecked with "no compile check could be derived: no file `<producer>`
writes holds a scaffold". The shapes a consumer uses still exist in that case; they are the
code as it is. A multi-crate plan whose producers edit finished crates had no compile check on
any edge.

- **The current code is the stub.** `derive_interface::derived_stub`: the scaffold files when
  any file the producer writes holds one, else every file it writes that exists at the dispatch
  base. The consumer files, the compile command, stub-compile and its canaries are as before. The
  interface is registered compile-only (`derived-<producer>`). Before dispatch the check proves
  the compile command builds the consumer files (the canaries) and that they build now. Only a
  scaffold is given to a behavior ask as its stub; current code is not a stub a test fails
  against.
- **The check that matters runs after the producer.** When the producer's attempt passes its own
  verification, `checks::check_delivery` lays the producer's delivered files (every declared write
  its workspace holds, so a new module beside an edited file is included) over the check base and
  runs the interface's compile command. The consumer files are not laid over it: the base, which
  follows the integration tip, holds their current text. A failure fails the attempt with the
  interface, the consumer files and the compiler output named, and tells the worker to call
  `challenge_contract` if its contract must change the shape. A change the consumer files do not
  use compiles, so it passes. This check runs for every compile-only interface, scaffold ones
  included: the worker brief already told the producer to keep the signatures the consumer files
  use, and now that is checked.
- **Never a false block.** When the delivery fails, a control build without it runs. A control
  that fails too means the base is broken for another reason: no verdict. A check that cannot run
  or times out twice gives no verdict either. Each case is a notice with the pass
  (`DeliveryCheck::NotJudged`), never a failure of the producer.
- **Current code left in place is not a stub left in place.** `stub_land::stub_left_in_place`
  counts only scaffold files of a compile-only interface: a producer need not change every file
  it may write.
- **Per language.** Rust: `cargo test --no-run -p <consumer packages>` type-checks every item,
  signature and type the consumer files use. Python: `python3 -c "import <modules>"` resolves the
  names the consumer modules import when they load; a changed call shape or an attribute use is
  not checked, so it never blocks. Every other language (shell, TypeScript, JavaScript, files of
  more than one language, a file with no extension) gets no derived check, and the edge is
  recorded unchecked with the language named (`derive_interface::no_command_reason`), decided
  from the producer's own files before any search for their users. The derived-check sentence of
  an edge's reason names the stub's source (`stub: the scaffold of ...` or `stub: the current
  code of ...`) and what the command checks (`derive_interface::coverage`).

Known limits. An overlay cannot delete: a file the producer removed stays in the check tree. A
planned shape change that the consumer contract is to adopt fails the producer until the worker
challenges the interface. The post-producer compile builds the consumer packages in a fresh target
directory, as every check does, so it adds one cold build of them to each producer's verification.

Tests: `kgraf-orchestrator` `derive_interface.rs`
`a_producer_that_edits_existing_code_gets_its_current_code_as_the_stub`,
`no_file_yet_or_no_user_or_no_command_derives_nothing_and_says_why`,
`a_shell_or_typescript_user_gets_no_compile_check_and_the_reason_names_the_language`;
`tests/contract_checks.rs`
`rust_a_delivery_that_breaks_a_used_signature_fails_and_an_unused_change_passes`,
`python_a_changed_call_shape_does_not_block_and_a_removed_imported_name_does`,
`a_base_that_does_not_build_or_a_check_that_cannot_run_gives_no_verdict`,
`the_delivery_is_every_declared_file_the_workspace_holds`; `tests/contract_flow.rs`
`a_producer_that_edits_existing_code_gets_a_compile_check_of_its_current_code`,
`a_shell_producer_is_recorded_unchecked_with_its_reason_and_never_blocked`,
`a_producer_that_breaks_a_shape_of_its_current_code_fails_verification`,
`a_producer_that_keeps_the_used_shapes_of_its_current_code_passes`.

## Amendment (2026-10-06, fourth): an abstract method is not a scaffold, and a check that cannot apply to a language never blocks

An audit of every check that can hold, refuse or fail a plan, a contract or an attempt, for
assumptions that hold only for Rust, found the placeholder rule of the 2026-10-02 fourth amendment above
too wide for Python and Ruby. In both, raising `NotImplementedError` is also how a correct base
class makes a subclass supply a method. A plan whose instruction named such a module (so the
worker could read the base) and wrote only the new subclass was refused as leaving an
unwritten scaffold, and the run failed when its re-asks were spent. The same rule decides what
a derived interface takes as the producer's stub.

- **The rule.** A raise of `NotImplementedError` does not count as a placeholder when its
  method has an `abstractmethod` decorator (any spelling: `@abstractmethod`,
  `@abc.abstractmethod`, `@abstractproperty`), when its class is based on `ABC`, `ABCMeta` or
  `Protocol`, when another class in the same file derives from its class (`class
  Circle(Shape)`, `class Circle < Shape`), or when its message says a subclass is to supply it
  (`subclass`, `abstract`, `override`, `overridden`). Read from the indentation of the lines
  around the raise, after comments and strings are blanked. Only Python and Ruby files are
  read this way.
- **Still a placeholder.** The same raise in a module-level function, in a method of a class
  nothing marks abstract, or with a message such as "must be implemented" alone, which a
  scaffold says too; every `todo!()` and `unimplemented!()`.
- **A missed form found.** C#'s `throw new NotImplementedException()` (and
  `System.NotImplementedException`), which Visual Studio writes into every generated member,
  is now a placeholder; a `catch` of it or a comment is not.
- **The standing rule for language reach.** A derived check that kGraf cannot make for a
  language (no compile command for the files, no test runner it can narrow, no member check for
  a non-cargo workspace) records why and leaves the edge unchecked or the command whole. It
  never holds the run. The audit found every derive, narrowing and member check already did;
  the false blocks it found are fixed here and in decisions 0018,
  0078, 0084 and 0096 (amendments of 2026-10-06).

Tests: `kgraf-orchestrator` `placeholder.rs` `an_abstract_method_is_not_a_placeholder`,
`a_csharp_not_implemented_exception_is_a_placeholder`; `plan_repo.rs`
`a_named_abstract_base_class_is_not_an_unwritten_scaffold`.

## Amendment (2026-10-06, fifth): derived compile checks and the member check reach other languages

The derived compile command of the second 2026-10-02 amendment existed for Rust and Python only,
and the member check of the 2026-10-06 amendment for Cargo only. A project in another language
got no derived check and no member check: less protection, never a false hold. Each new check
below obeys one rule: it never holds, refuses or fails a correct project. When kGraf cannot be
sure, the edge or the plan is recorded unchecked with the reason.

- **Derived compile commands for TypeScript, Go, Java and C#**
  (`derive_interface::command_for`). Each runs only where a cheap, offline check exists and the
  project and the machine allow it; every other case is an unchecked edge whose reason names the
  missing part:

  | Language | Command | Only when |
  | --- | --- | --- |
  | TypeScript | `node <tsc> --noEmit -p <dir>` | a `tsconfig.json` above every consumer file (one), the project's own compiler at `node_modules/typescript` in the repository, `node` on the `PATH` |
  | Go | `go vet <packages>` (`go -C <module> vet ...` below the root) | one `go.mod` above every file, `go` on the `PATH` |
  | Java | `javac -proc:none -d .kgraf-javac-out -sourcepath <roots> <files>` | no `pom.xml`, Gradle or Ant file above any file, a `package` line that matches each file's directory, `javac` on the `PATH` |
  | C# | `dotnet build --no-restore <project>` | one `.csproj` above every file, its `obj/project.assets.json` in the repository (dependencies restored), `dotnet` on the `PATH` |

  The check tree is made from the base commit, so only what the repository tracks is there: an
  installed `node_modules` or a restored `obj/` that git ignores is not, and the edge is
  unchecked with that reason. The command runs before dispatch as every derived command does:
  one that fails at the base (a `go vet` finding the base already has, a broken toolchain such
  as a `javac` stub with no JDK) is not registered and blocks nothing. `go vet` is used, not
  `go build`, because it type-checks the test files as well. The run's report names what each
  command checks (`derive_interface::coverage`).
- **C# and Java test files are tests.** `derive_interface::is_test_path` also reads
  `*Test.<ext>` and `*Tests.<ext>` (a capital `T` after a lower-case letter or digit, so
  `Contest.java` is not one), `*_spec.<ext>`, a `Tests` directory and a .NET test project
  directory (`App.Tests/`).
- **Go and record names are read.** The keyword fallback `derive_interface::declared_names`
  reads Go's `func` (a method's receiver is skipped) and `record`, so a Go or C# producer the
  graph has not indexed still has names a consumer can use.
- **The member check covers Go workspaces and Maven reactors** (`plan_repo::module_findings`).
  A `go` command that loads packages reads the nearest `go.work` above its directory and every
  module it `use`s; `mvn` builds every `<module>` of the `pom.xml` it runs on. A member whose
  `go.mod` or `pom.xml` is neither in the repository nor written by the contract or one it
  depends on fails the command, and the plan is refused naming the file, as for Cargo. Only a
  manifest the repository holds and the contract's side does not rewrite is read; a
  `GOWORK=off` command, a command in a module's own directory, and a module in a Maven profile
  are not judged. Not checked: npm (measured with npm 11: a listed workspace that is missing is
  skipped, and only `--workspace=<missing>` fails), uv (measured: skipped), Gradle and pnpm
  (whether a listed project with no directory fails depends on their version).

Known limits. The live type checks were exercised by their unit tests and by the never-block
flow test; no TypeScript, Go, Java or C# compiler was present on the machine that built this, so
the registered path of those four languages has not run against a real compiler there.

Tests: `kgraf-orchestrator` `derive_interface.rs`
`typescript_gets_a_type_check_only_with_its_own_compiler`,
`go_gets_go_vet_of_the_packages_when_go_is_installed`,
`java_gets_javac_only_without_a_build_tool`, `csharp_gets_dotnet_build_only_when_restored`,
`config_probes_read_nothing_for_rust_and_python`,
`test_paths_python_modules_and_cargo_packages_are_read_by_convention`,
`declared_names_are_read_by_keyword`; `tests/contract_flow.rs`
`a_correct_project_in_any_language_is_never_blocked_by_a_derived_check`; `plan_repo.rs`
`a_go_work_member_written_later_is_refused_and_one_in_time_is_not`,
`a_maven_module_written_later_is_refused_and_a_profile_module_is_not`,
`workspaces_that_do_not_load_a_missing_member_are_never_refused`.

## Amendment (2026-10-08): a package the workspace does not list is refused

The member check read one direction only: a member the workspace lists whose manifest comes too
late. The other direction passed intake. A contract wrote a new package manifest
(`wcl/Cargo.toml`) under the root workspace, the root manifest as its dependency wrote it listed
`wcl-core` alone, and the contract could not write `Cargo.toml` to add `wcl`. Its check
`cargo check -p wcl` could not pass; its worker asked for `Cargo.toml` and the contract was held,
and the run ended blocked with a merged result whose tests passed only because `wcl` was never
built.

- **A package the workspace does not list is refused** (`plan_repo::unlisted_package`). A
  contract whose acceptance runs cargo in a workspace, and that writes a package manifest under
  the workspace's directory that the workspace, as that contract gets it, does not list (not an
  explicit member, not under a `<prefix>/*` member, not excluded), is refused unless it writes
  the workspace manifest itself. Measured with cargo 1.99: `cargo check -p <package>` fails
  ("did not match any packages"), `cargo run -p <package>` fails ("not found in workspace"), a
  command in the package's directory or with `--manifest-path` fails ("believes it's in a
  workspace when it's not"), and `cargo test --workspace` passes without building the package.
  The `--workspace` form is refused too: it would check nothing of the package. The remedy names
  two shapes: the contract also writes the workspace manifest to add its member (and depends on
  any other writer of it that could run at the same time), or the contract that writes the
  workspace manifest lists the member and writes the package's stub. A package manifest the
  repository already holds, one with a nearer workspace manifest in the repository, and one
  with no workspace above it are not judged.
- **The plan-shape hold says "open".** A worker that needs a file no open contract writes is
  held for a re-plan. The hold now says no *open* contract writes it: a contract that finished
  may have written it, and its writes closed with it.

Tests: `kgraf-orchestrator` `plan_repo.rs`
`a_package_the_workspace_does_not_list_is_refused_with_its_remedies`,
`a_package_the_workspace_covers_or_the_repository_holds_is_not_refused`,
`the_check_agrees_with_cargo` (the refused plan fails in a real cargo run, its remedy passes),
`a_toml_member_list_wins_over_prose`, `this_repository_with_ordinary_plans_is_not_refused`;
`shortfall.rs` `a_hold_for_a_file_no_contract_writes_asks_for_a_replan`.

## Amendment (2026-10-08, second): an ask the planner cannot win is not made, and a script test gets its call

A three-contract Python packaging plan (write the package, build its wheel and source archive,
serve and install them) spent all three asks of the interface call, about 100 s each on a local
model, and accepted no interface. Three faults, each enough to spend an ask:

- **The test ran no assertion.** Every test file the planner wrote defined `def test_...`
  functions and never called them, and the test command was `python test_x.py`. The file exits
  0 against the stub and against the reference alike. The vacuity check refused it for passing
  against the stub, which names the symptom; the planner made the assertion stricter and kept
  the shape on every ask.
- **The build contract was asked about.** Its declared writes were a `.whl` and a `.tar.gz`. A
  stub and a reference are text the planner writes, and a text file is not a wheel, so no
  reference of it can exist. Its answers were refused for an empty stub and reference, then
  for a reference too small to be a wheel.
- **A refusal the planner was not asked to fix bought a re-ask.** Any refused interface sent
  the call back to the planner, including one for a producer kGraf would not have asked about.

The rules:

- **A script test gets its call** (`script_tests`). When an interface's test command runs a
  test file of the answer as a script (`python`, `python3` or `python3.N`, no `-m`, no `-c`),
  and the file's top level only defines tests (`def test_...` functions, `unittest.TestCase`
  classes) without calling them, kGraf appends the calls under `if __name__ == "__main__":`
  (the functions in order, then `unittest.main()`), says so in a notice, and checks the
  interface with the calls. This is the same kind of repair as a compile command that does not
  build the files being replaced. When a test cannot be called with certainty (it takes
  arguments such as pytest fixtures, is async or decorated, or is a pytest class), the
  interface is refused with that cause and the remedy instead. A top level that could already
  run its tests (an `if`, a loop, `with`, `try`, a top-level `assert`, a line naming a test,
  `globals()`, `unittest.main`, `pytest.main`) is left alone, and so is a test file named by
  path alone from the repository.
- **A producer of binary build outputs only is not asked about**
  (`derive_interface::binary_outputs_only`). When every file a producer declares is a package
  archive, compiled object or library (`.whl`, `.tar.gz`, `.zip`, `.jar`, `.so`, `.dll`,
  `.pyc`, `.class`, `.wasm` and the others in `BINARY_OUTPUTS`), its behavior check is
  recorded as not run, `behavior check not run: <producer> writes only binary build outputs
  (...)`, like the two existing skips. A producer that also writes one source file is asked
  about as before.
- **A refused interface whose producer is skipped does not buy a re-ask.** Its refusal goes
  into a notice, and its edges keep the skip reason as their unchecked reason. An interface for
  a skipped producer that passes its checks is still accepted.

Accepting an interface has a cost of its own: its gate states a handshake per open party, one
model call each, made one after the other before the parties dispatch. An ask this amendment
saves can be spent there instead, so it claims no time saved; it claims asks that can succeed.

Tests: `kgraf-orchestrator` `script_tests.rs` (the script argument, the repair, the refusal
causes, files left alone, docstrings), `derive_interface.rs`
`a_producer_of_binary_build_outputs_only_is_not_asked_about`; `contract_flow.rs`
`a_script_test_that_never_calls_its_test_gets_the_call_added`,
`a_script_test_kgraf_cannot_call_is_refused_with_the_cause`,
`a_producer_of_binary_build_outputs_is_not_asked_about`, and
`the_recorded_pypi_server_answers_are_accepted_on_the_second_ask` (the four planner answers of
the run, verbatim, in `tests/fixtures/planner-answers/tb-pypi-*.txt`: without the three rules
they spend three asks and accept nothing, as the run did; with them, two asks and
`i_pkg_create` accepted; with any one rule removed, three asks).

## Amendment (2026-10-09): a contract that declares no writes is refused

A greenfield plan was refused for a workspace member no earlier contract wrote. The planner's
second answer kept the four contracts and their instructions (create `Cargo.toml`, create the
`wcl-core` crate, create the `wcl` binary, add tests) and declared no writes in any of them.
Every check this decision keys on declared writes (a listed member whose manifest comes late, a
package the workspace does not list, a stub no contract writes, contracts sharing one file and
one test) found nothing to judge, and the answer was accepted. A worker may change only the
files its contract lists, so c1's worker was refused on `Cargo.toml`, asked for it, and the
contract was held as a plan-shape error; the run ended blocked with no `Cargo.toml`.

- **A contract that declares no writes is refused** (`plan::writeless_refusal`, applied to
  every contracts answer of the planner, a split's included). The planner is asked again with
  the contracts named and the shape of a write (`{"path":"src/x.rs"}`). With an empty write
  scope a contract can change nothing, so no task kind needs one. A corpus plan is not judged
  here: the user approved it as it stands, and its intake is its own.

Tests: `kgraf-orchestrator` `plan.rs` `a_contract_that_declares_no_writes_is_refused_by_name`;
`plan_intake.rs` `contracts_that_declare_no_writes_are_refused_and_asked_again` (the contracts
of the run as kGraf's trace recorded them, `tests/fixtures/planner-answers/greenfield-writeless.txt`:
without the rule the answer is accepted on the first ask; with it, refused once with the reason,
and the corrected answer is accepted).

## Amendment (2026-10-09, second): only a plan in which no contract declares a write is refused

The amendment above refused every answer in which any contract declared no writes. That also
refused a correct plan: a Terminal-Bench `pypi-server` answer of three contracts, the first
writing the package, the second writing the server script, and the third (`c3-install-verify`)
installing the package from the server and checking an import. A contract that only installs
and verifies changes no file. The answer was refused, the next answer came back malformed, and
the run failed planning.

- **The rule now refuses only a plan in which no contract declares a write**
  (`plan::writeless_refusal`): such a plan can change no file, and every check keyed on declared
  writes passes it unjudged. The refusal names the contracts, says that a check-only contract
  may declare none, and gives the shape of a write. One contract with no writes next to
  contracts that write is accepted. Still applied to every contracts answer, a split's included.
- **A writeless contract whose instruction needs a file is met at run time, not at intake.**
  Whether an instruction asks for a file is not decided from its text: instructions are prose,
  and "write" and "create" also name what a check-only contract does (create a virtualenv,
  write to stdout), so a text rule would refuse correct plans. A write such a contract attempts
  is refused, as any write outside a declared scope is, with a reason that names the plan
  shape: the contract declares no writes, so it may run checks and change no file; a file the
  work needs belongs to the contract that lists it, and if none does, the plan is missing that
  write. A shell call that may write anywhere says the same.

Tests: `kgraf-orchestrator` `plan.rs`
`a_plan_in_which_no_contract_declares_a_write_is_refused_by_name`; `plan_intake.rs`
`a_verify_only_contract_next_to_contracts_that_write_is_accepted` (the answer verbatim,
`tests/fixtures/planner-answers/tb-pypi-verify-only.txt`: refused on every ask with the previous
rule, taken on the first ask now) and
`contracts_that_declare_no_writes_are_refused_and_asked_again` (still refused once without its
writes; with the rule disabled, accepted on the first ask); `kgraf-runtime` `worker_loop.rs`
`a_write_by_a_contract_that_declares_no_writes_names_the_plan_shape`.

## Amendment (2026-10-09, third): the interface answer cap holds only where the model does not reason

The 4,096-token cap on an interface answer for which kGraf supplies the stub ("What the ask
holds", above) was sent as the call's output limit. Every protocol kGraf speaks counts a model's
reasoning against that limit: `max_tokens` on Chat Completions, which Ollama passes on as
`num_predict`; `max_output_tokens` on Responses; `max_tokens` with thinking on Messages. A
reasoning planner held to 4,096 tokens spends them reasoning and is cut off before it answers, on
every ask, and the cut-off answer is not asked again. The refusal also told the user to raise
`--planner-max-tokens`, which cannot raise the cap.

- **The cap is enforced only when the `interfaces` kind's reasoning level resolves to off**
  (decision 0098; `ModelCaller::output_limit`). Off is the one level whose absence of reasoning
  is checked on the answer. With the level unset, low, medium or high, the call gets the
  planner's own limit, and its reservation matches (`Decomposer::output_limit`). The request
  still states the size: "at most 4096 output tokens, not counting any reasoning before it".
- **A cut at the cap says what it is.** `PlanError::Truncated` carries `answer_cap`: an answer
  cut at a cap below the planner's limit names the cap and that the answer itself was longer,
  and names no setting; a cut at the planner's limit still names `--planner-max-tokens`.
- **Nothing else changes.** A cut at either limit is still never guessed at, parsed or asked
  again. The decomposition and re-grounding calls had no cap and keep the planner's limit.

Tests: `kgraf-orchestrator` `plan.rs`
`an_interface_ask_is_held_to_its_output_cap_only_with_reasoning_off`; `interface_plan.rs`
`the_request_says_what_is_supplied_and_states_the_output_cap`.

Measured on `qwen3.6:35b` served by Ollama: at a limit of 8,192, decomposition calls ended with
`finish_reason: "length"` at 8,192 completion tokens, one with no answer at all, only reasoning.
Interface calls with reasoning on took 1,433 to 27,614 completion tokens (median 9,118). Answers
refused because they end before the JSON object closes came back with `finish_reason: "stop"`
far below the limit: the model ended them itself, and this amendment does not change how they
are handled.

## Amendment (2026-10-09, fourth): an answer the model stopped before its JSON closed is closed or asked once more, not refused

An answer the model ends itself (`finish_reason: "stop"`, `StopReason::EndTurn`) before its JSON
object closes used to be refused and cost one of the three asks, though its content was not at
fault. Measured on recorded planner calls: about 3 in 62 interface calls and 1 in 758
decomposition calls. Two shapes occur: the final `}` missing after a complete value, and a stop
inside a string, mid-word.

- **Every orchestrator call that parses JSON reads its answer one way**
  (`ModelCaller::ask_read`, `read_answer`): `decompose` (with or without model tools),
  `interfaces`, `reground`, `answer_question`, `judge_connections`, `interpret`,
  `question_route`, `challenge_review`, `handshake`, `consumer_test`, `interface_review`.
  `question_answer` asks for prose and is not read as JSON.
- **Closing rule** (`answer_json::closing`). Only for an answer the model ended itself. The
  answer's open object must fail to parse with an end-of-input error, so the parser read to the
  end of the text with no other error; and the text, without trailing white space or a closing
  code fence, must stop right after a complete value (a closed string value, `}`, `]`, `true`,
  `false`, `null`). Then the closing brackets and braces of every open container are appended
  in order, and the closed text must pass the same parser a whole answer passes. Never inside a
  string, never after a key, a colon, a comma, an opening bracket or a number. An error before
  the end of the text, such as a string left open in the middle of a finished answer, is the
  answer's own mistake and is refused as before.
- **One free re-ask.** An answer the model stopped early that cannot be closed, or whose closed
  form the parser refuses, sends the same request once more with one sentence added at its end
  (`INCOMPLETE_ANSWER_NOTE`). That call belongs to the same ask: it is not counted against the
  ask budget, writes no `plan_refused` or `interfaces_refused`, and is ledgered and attributed
  like any call. At most one per ask: a second incomplete answer is the ask's normal refusal and
  counts. In the model-tool loop the same round is sent again and is not a round.
- **A cut at the output limit is unchanged.** `StopReason::MaxTokens` is still
  `PlanError::Truncated`: never closed, never parsed, never asked again.
- **Never silent.** Each closing and each re-ask is written to the run's trace as
  `answer_repaired` (call, agent, kind, `appended`; no call added) or `answer_reasked` (call,
  agent, kind, `reason`), with a notice in the run's output naming the call.
- **Measured.** A cheaper path can be taken more often, so the canary records `answer_repairs`
  and `answer_reasks`, with counts per call kind, for every model scenario. Neither is a wasted
  ask.
- **Accounting.** A closed answer costs no call. The re-ask's tokens go through the same
  ledgered path.

The JSON repair crates guess quotes, commas and brackets; this rule appends only closers, and
only where nothing else can follow.

Tests: `kgraf-orchestrator` `incomplete_answer_tests.rs`, built from recorded answers
(`tests/fixtures/incomplete-answers/`); `malformed_answer_retry.rs`; `intake.rs`.

## Amendment (2026-10-11): a file only a finished contract wrote is handed to the contract that needs it

A greenfield canary run (round 17d, `qwen3.6:35b`) planned `c1`, which wrote the root `Cargo.toml`
(a workspace with member `wcl-core`) and `wcl-core`, and `c2`, which wrote `wcl/Cargo.toml` and
`wcl/src/main.rs`. `c1` finished and was integrated. The worker of `c2` correctly reported that
`wcl` must be a root member for `cargo test -p wcl --offline` to pass. No open contract wrote the
root `Cargo.toml`, because the writes of `c1` closed when it finished, so the unattended run held
`c2` as a plan-shape error and ended Blocked. A planner that leaves a shared manifest with an
earlier contract is a common model deficiency; the run now recovers from it.

An unattended shortfall for a path is now granted to the asking contract when all of these hold:
no open contract writes the path; a contract of the run that closed as verified and integrated
listed the path in its writes; and no user deny or ask rule applies (those checks run first and
still win). Disjoint writes forbids concurrent writers. The file's only planned writer is done and
no open contract writes it, so it has no concurrent writer, and the asking contract's result is
verified as usual. The grant is loud: its `by` text reads "`<path>` was written by finished
contract `<id>` and no open contract writes it, so it was handed to this contract", and it reaches
the trace and the contract's grant record like every other grant.

A path that no contract of the run ever listed keeps the plan-shape hold and its re-plan advice.
An attended run is still asked. The re-plan stays what it was: it runs once, before work starts.

The closed-contract record now keeps the paths the contract listed in its writes (`writes`, empty
for a record from before this change). `decide_in_plan` takes the run's closed contracts.

Tests: `kgraf-orchestrator` `shortfall.rs`:
`a_file_only_a_finished_contract_wrote_is_handed_to_the_asking_contract` (grant with the finished
writer named; unlisted path still held; a superseded contract does not count; attended still
asks) and `a_user_rule_wins_over_the_handover_of_a_finished_contracts_file`. The first was seen to
fail with the finished-writer lookup disabled.

## Amendment (2026-10-11, second): a cargo workspace refusal with a mechanical remedy is repaired, not asked again

A greenfield canary run (round 17e, `qwen3.6:35b`) asked the planner three times. The checks
against the repository refused all three plans, and the run ended PlanFailed. The three refusals:
`c1` ran `cargo test --workspace` while `wcl/Cargo.toml` was written only by `c2`, which `c1` did
not depend on; `c1` ran `cargo test -p wcl-core` at the root while only `c3`, which ran after
`c1`, wrote the root `Cargo.toml`; `c1` wrote `wcl/Cargo.toml` but no Rust source under `wcl/`.
Each refusal named a fix that needs no judgment. Ordering a Rust workspace across contracts is a
common model deficiency, and this decision already says to derive first and to ask the planner
only for judgment.

A cargo workspace finding now carries a structured remedy (`plan_repo::Remedy`) next to its
refusal text. Before a refused plan spends a re-ask, intake makes the remedy of the first finding,
checks again, and repeats, at most 4 changes per plan. The remedies, each only where it is safe:

- **An edge.** A contract needs a workspace or member manifest that a contract it does not
  depend on writes: it depends on that writer, when the writer does not depend on it (no cycle).
  The edge is always tried first.
- **A manifest moved to the contract that needs it.** When every writer of that manifest runs
  after the contract (so the edge would be a cycle), the contract also writes the manifest. The
  later writer rewrites it, which the plan rules allow for two contracts a dependency orders. A
  manifest no contract writes is not repaired: the plan left it out, which is judgment.
- **A minimal member source.** A member manifest with no Rust source in time: the contract that
  writes the manifest also writes `<member>/src/main.rs` when the plan shows a binary (a contract
  writes `src/main.rs` or a file under `src/bin/`, or an instruction calls the member a binary
  within two words of its name), else `<member>/src/lib.rs`.

A write is never added when a contract that can run at the same time also declares the path. Go
and Maven findings, and a package the workspace does not list (the fix changes the member list
inside a manifest), keep the re-ask. If a finding has no remedy, a remedy is not safe, the
repaired plan is still refused, or it fails the plan rules, the plan is asked for again with its
unchanged refusal, exactly as before; on the last ask the run fails as before.

A repair is loud: one `plan_repaired` trace event per change (the ask, the contract, the change in
words) and one run notice per change, which the accepted plan's notices carry. The canary counts
the events as `plan_repairs` beside `answer_repairs`; a repair writes no `plan_refused`, so it is
never a wasted ask.

Tests: `kgraf-orchestrator` `plan_repo.rs`:
`a_member_manifest_with_no_source_gets_a_minimal_source_from_its_writer` (the third refusal's
shape), `a_workspace_manifest_a_later_contract_writes_moves_to_the_contract_that_needs_it` (the
second), `a_member_manifest_another_contract_writes_is_reached_by_an_edge_first` (the first),
`an_edge_that_makes_a_cycle_is_never_made`, `a_write_a_concurrent_contract_declares_is_never_made`,
`a_refusal_with_no_mechanical_remedy_is_not_repaired`, `a_member_is_a_binary_only_on_evidence`
and `repaired_plans_pass_with_cargo` (each repaired plan's contracts pass a real cargo run).
`tests/plan_repo_intake.rs`: two deadlocked plans are accepted at the first ask with their
`plan_repaired` events and notices, and a member no contract writes is still refused and asked
again. With the repair call disabled the repair tests failed; with the cycle and concurrent-writer
guards disabled their two tests failed.
