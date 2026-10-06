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
