# Decision 0018: Continuous merge, with quality gates as first-class work nodes

**Status:** Accepted. Its gate cadence clause is refined by decision 0019.
**Related:** decisions 0009, 0012, 0013, 0014, 0015, 0016, 0017

## Context

Weakening the item boundary (decision 0016) left no natural batching point
for merges. Continuous merge also surfaces conflicts one worker at a time,
small and attributable, rather than as one large merge at a boundary.

The risk continuous merge carries is that merges are unsupervised, and the
cross-agent textual conflict rate measured on real pull requests is 41.7
percent against 19.8 percent for same-agent pairs (decision 0009). A
cross-vendor design is exactly the applicable case.

Gates are the answer to that risk, and making them nodes rather than
background machinery means they inherit everything already decided. A
failing gate is a blocking node under decision 0014, with a visible
blocking edge to what it holds up. No new visual language, no new state
model.

### What gates do for the central design bet

The published evidence that most threatens this architecture concerns
semantic interference: changes that merge cleanly and then fail each
other's tests. That damage is invisible to git, which is why a
history-based measurement alone cannot catch it.

A gate catches exactly that class. It does not make the partition better,
but it makes an imperfect partition survivable rather than silently
corrupting. That materially reduces the cost of being wrong about the
central bet.

## Decision

**Worktrees merge back continuously**, as each worker finishes, not
batched at a boundary.

**Quality gates are work nodes in the graph**, inserted by the orchestrator
at intervals, not invisible infrastructure. A gate node has doneness, can
block, can be expanded, and competes for the concurrency budget like any
other node.

**A gate runs the project's declared validation commands**, discovered
from the document corpus under decision 0013.

**A gate runs against a pinned commit**, not the live trunk, so merging
never freezes.

**A failed gate halts new dispatch but does not cancel in-flight work.**
The remediation becomes its own work node.

## Consequences

**Gate cadence is a policy**, configurable under decision 0017, with
orchestrator discretion to insert an extra gate when risk is elevated, such
as after several cross-vendor merges land in the same region of the graph.

**There is an unverified window between gates**, and it must be named
rather than glossed. Work merged since the last passing gate is
unvalidated. Frequent gates shrink the window and cost more validation
runs; infrequent gates widen the blast radius. This tradeoff is exposed,
not hidden.

**A gate failure is attributable to a commit range**, since gates pin
commits. Whether the orchestrator bisects that range automatically is an
open question and is not decided here.

**New dispatch halts on failure; running work continues.** The halt is a
visible blocking state on the affected region of the graph, distinct from
a dependency block and from a concurrency-cap hold.

**A project with no discoverable validation commands gets no gate, and
kGraf says so.** Gates are discovered, never invented. The orchestrator
states plainly that it cannot gate quality and names what it would need,
rather than constructing a plausible-looking test command or silently
skipping the gate. This is the honest outcome for an untested repository
and the user should be told at ingestion, not at the first failure.

**This clarifies the verifier's primary role.** Decision 0009 introduced a
verifier to select among redundant parallel attempts, and decision 0012
made that mode secondary. The verifier's primary job is the periodic gate.

## Open

Whether the orchestrator bisects a failing commit range automatically, and
whether a gate failure should trigger a re-partition of the affected region
on the theory that the partition was wrong, are both open questions.

## Alternatives considered

**Batch merge at item boundaries.** Rejected: decision 0016 removed the
item boundary as a scheduling concept, and batching produces one large
conflict instead of several small attributable ones.

**Gates as invisible background jobs.** Rejected. An unseen gate is a
second system with its own state, and a failure would need a bespoke way to
surface. As nodes they are already visible, already blockable, already
expandable, and already counted against the concurrency cap.

**Gates run on the live trunk.** Rejected. Workers merge underneath a
running gate, so it tests a moving target and cannot say what passed.
Pinning a commit costs nothing and makes the result attributable.

**Freeze merging while a gate runs.** Rejected: it is serialization, which
decision 0012 already identifies as the trap that makes parallelism
nominal.

**Cancel in-flight workers on gate failure.** Rejected. Those workers may
have had nothing to do with the failure, and discarding their work to
punish someone else's breakage costs more than letting them finish into a
tree already known to be broken.

## Amendment (2026-09-30): a partial gate that cannot judge, and remediations that must fix

Every gate runs the project's validation commands, which cover the whole project
(`cargo test --workspace`). A gate below the root therefore also ran tests of contracts
outside its cover that were not built yet, failed on them, halted all other dispatch, and
spent its remediations on a failure no remediation in its region could fix; the contracts
that would have made it pass never started.

- **A gate below the root that fails is compared with the run's base commit.** The same
  commands run once per run at the integration tip recorded when the gates were planned,
  before any of the plan landed. If they fail there too, the gate is `inconclusive`: no
  remediation, no halt, nothing blocked, and its parent gates may run. The root gate covers
  everything and is never inconclusive, so the run completes only when the root gate passes
  or is waived. If the base passes, the failure is the cover's and the gate fails as before.
  A run with no recorded base (unversioned workspaces) judges its gates as they stand.
- **A remediation must pass the failing gate's commands.** It carries them, and the verifier
  runs them after the run's own check; the run's check alone can pass on the code the gate
  failed on.
- **A remediation that integrates no change counts as failed.** The gate opens the next
  remediation, whose brief says why the last did not count, or holds past the limit; it is
  never run again on the unchanged commit.

## Amendment: a contract is accepted on its own tests; the whole command is the root gate

When the acceptance command (`--verify`) is the whole suite and each contract writes one file,
no contract can pass before its siblings merge: their stubs fail the suite, and workers ask for
writes outside their scope.

- **The rule.** A contract is accepted on the tests that belong to its declared write set. The
  project's whole test command judges the integrated result, as the root gate, once every
  contract has landed.
- **How a test belongs to a file** is read from the runner the command names and the tree, by
  the runner's naming convention:
  - `python -m unittest discover [-s DIR]`: `test_<stem>.py`, run with `-p test_<stem>.py`;
  - `pytest` or `python -m pytest` with options only: `test_<stem>.py` or `<stem>_test.py`,
    appended as a path;
  - `node --test` with options only, and `npm test` whose `test` script is `node --test`,
    `jest` or `vitest`: `<stem>.test.<ext>`, `<stem>.spec.<ext>`, or `<stem>.<ext>` in a
    `test`, `tests` or `__tests__` folder, after `npm test --`;
  - `cargo test` or `cargo nextest run` over a workspace (`--workspace`, `--all`, or a virtual
    manifest): the package of each declared file, with `-p <package>`.
  A declared test file selects itself. One command runs per test file or package, joined with
  `&&`.
- **The whole command runs** when the command is not one of these (a script, `make`, shell
  syntax or quotes), when it already selects tests, when no test belongs to the contract's
  files, and always for a gate remediation. A failed narrowed check names the tests it ran.
- **The root gate.** When the acceptance command is narrowable and is not already a gate
  command, it is added to the gate commands, so the whole suite judges the merged result even
  when discovery found nothing. The run prints an `accept` line saying so.
- **The worker is told** the exact narrowed command and the whole one, and that tests of files
  outside its write set may fail until their contracts merge.
- **Known limit.** Cargo narrows to the package: two contracts in one crate still see each
  other's stubs fail that crate's tests.

Alternatives rejected: comparing failing test sets at the base and on the attempt (an output
parser per runner, and it accepts a partial fix); a planner-written per-contract command (not
a mechanism); a prompt-only change (the verifier would still refuse the correct attempt).

## Amendment (2026-10-01): a worker is always told what it is judged by

When the verify command is not one kGraf can narrow (`cargo build --workspace`, a script,
`make`), the brief used to say nothing about the check. A worker then read the planner's
"Acceptance: cargo test --workspace" as its test and fought sibling contracts' `todo!()`
stubs. Now every contract's brief states: "Your attempt is accepted when `<verify command>`
passes in your worktree. The Acceptance line of your contract is advisory. Failures in other
contracts' unfinished code (stubs such as `todo!()`) are not yours to fix." A narrowed check
keeps its own text. A gate remediation gets no such text: it fixes what the merged result
failed, so sibling failures there are its work. Tests: `scoped_check`
`a_whole_check_brief_names_the_command_and_calls_acceptance_advisory`, `kgraf-host`
`a_remediation_is_checked_with_the_whole_command`.

## Amendment (2026-10-01): a compile error the worker caused is the worker's, and a retry is amended once

A live local-model run on a scaffolded workspace showed two faults. The planner's instruction
turned a unit struct (`pub struct FlatDiscount;`) into a struct with a field, and existing code
constructed it as a value. The compile error appeared in another contract's file. The retry worker
read the brief's sentence about other contracts' unfinished code as covering that error and left it,
and the orchestrator sent the same instruction again with the same result.

- **Brief wording.** The sentence is now: "`todo!()` panics or failing tests in other contracts'
  unfinished code are not yours to fix; a compile error anywhere that your change caused (for
  example by changing a public type's shape) is yours." The advisory-acceptance sentence is
  unchanged.
- **A retry is amended once.** When the verifier's refusal holds a compile error (the
  `error[...]` line followed by `--> file:line` form) and every located error is in a file the
  contract does not write, the contract gets one amendment: it quotes the error and says "The error
  is in `<file>`, which uses `<symbol>` as the existing code defines it; keep the public shape that
  existing callers use." The amendment is stored on the contract (`outside_error_note`), quoted as
  data in every later attempt's brief, traced as a notice, and set at most once per contract. A
  failing test, a panic, a warning, or any error inside the contract's own files does not
  amend. Only the compiler's `-->` location form is read; other toolchains' errors take the
  ordinary retry.
- **Plan intake.** The planner is told that existing public items are the interface and that an
  instruction must not change the shape of a public type or a trait signature unless the user
  asked. An instruction that writes a type with fields where a declared file defines it as a unit
  struct is noticed (a token scan of the declared files, not a parser; a notice, not a refusal).

Tests: `kgraf-host` `scoped_check` `a_whole_check_brief_names_the_command_and_calls_acceptance_advisory`;
`kgraf-orchestrator` `outside_error` unit tests and `tests/shape_guard.rs` (the retry's brief carries
the amendment; once per contract however often it fails; the unit-struct notice).

## Amendment (2026-10-01): a retry carries what stopped the last attempt

A retry after a stall was dispatched with the same first prompt as the stalled attempt: a
user deny rule had refused `python3`, the worker then re-read one file until the stall
detector ended it, and the next attempt knew neither. Only a verifier's refusal was carried.

- `OpenContract::last_stop` holds, for the last failed round's attempts that no verifier saw:
  a stall's repeated call (`tool input`, cut at 300 characters), a spent step budget, or a
  runtime failure's reason, each with the calls a policy refused that no later call of the same
  capability got past (capability, tool, refusing hook, reason). It is scrubbed for storage and
  capped like `last_refusal`.
- The next fresh attempt's brief quotes it as data in the `verifier_refusal` tag, after a
  sentence that says the attempt ended before verification, not to repeat the call, and to call
  `report_shortfall` when a refusal blocks the work.
- A worker that ends an attempt with no tool call although its contract writes files is told
  once and then fails with a named reason (decision 0096, progress checkpoints); that reason
  is carried the same way.

Evidence: `kgraf-orchestrator/src/orchestrator.rs` (unit tests on the stop note and the brief)
and `kgraf-orchestrator/tests/retry_brief_stop.rs` (a real worker loop stalls; the next
attempt's opening message carries the repeated call).

## Amendment (2026-10-02): a whole check a split cannot pass per contract is deferred to the root gate

A run's check (`python3 check.py`) needed two files, and the planner split the work into two
contracts, the second depending on the first. The verifier ran the whole command on the first
contract alone, where it could never pass; the run ended Blocked. A cargo command has a plan
validator for shared test scopes; a plain command had nothing.

- **The rule.** The run's acceptance command (`--verify`) checks the whole instruction. A
  contract whose attempt cannot pass it until other contracts of the plan merge has the check
  deferred, never dropped: it runs on the merged result as the root gate before the run can
  complete.
- **When a check is deferred.** All of these hold: the command runs whole on the attempt (it is
  not narrowed); the contract is not a gate remediation; the attempt committed a change, and its
  scope and placeholder checks passed; other contracts of the plan are still open (the
  orchestrator passes their ids to `Verifier::verify_among`); the command fails on the attempt;
  and it also fails at the attempt's dispatch base, on a throwaway worktree. A command that
  passes at the dispatch base and fails on the attempt was broken by the attempt: a plain
  failure, never deferred, and the reason says "it passes at the dispatch base, so this
  attempt's change makes it fail". A command that passes on the attempt passes as before
  (`cargo build --workspace` over compiling stubs keeps its per-contract check).
- **The last contract is checked whole.** With no other contract open, nothing is missing from
  the tree, so the command must pass on the attempt itself.
- **The command is always a root gate.** It is added to the gate commands whenever it is not
  blank and not one already, narrowable or not (`scoped_check::with_whole_gate`), so a run with
  no discovered gate now has one. The verifier defers only when it knows the root gate runs the
  command (`CommandVerifier::root_gate`). The run prints an `accept` line for a whole command
  too.
- **Said, never silent.** A deferred attempt's verdict is `<command> (deferred to the root
  gate)`, and a notice names the contract, the open contracts, and the first 300 characters of
  the failing output. The brief of a contract checked whole tells the worker that a command
  that can only pass once other contracts merge is checked on the merged result, not to write
  other contracts' files to make it pass, and that breaking a check that passed before is still
  a failure.
- **Known limit.** Blame for a defect in a deferred contract lands on the contract that is
  checked whole after it, or on the root gate's remediation; the deferred contract itself is not
  re-opened.

Rejected: refusing or re-asking the plan at intake when a dependent split makes the command
unpassable per contract. Whether a plain command can pass on part of the work cannot be read
from the command text, and a re-plan costs a planner call per guess. Rejected: accepting a
deferred contract on the planner's per-contract Acceptance line (a model-written command is not
a mechanism; this amendment's base comparison is). Rejected: deferring only by the dependency
graph (contracts with dependents): it misses a parallel split whose siblings the check needs,
and it would drop the per-contract `cargo build` check that passes on stubs today.

Tests: `kgraf-host` `tests.rs`
`a_whole_check_a_dependent_split_cannot_pass_is_deferred_to_the_root_gate`,
`a_whole_check_is_not_deferred_for_the_last_contract_or_without_a_root_gate`,
`a_whole_check_the_attempt_broke_is_never_deferred`; `scoped_check`
`the_acceptance_command_becomes_a_root_gate_once`; `shortfall_tests.rs`
`a_run_check_a_dependent_split_needs_is_deferred_and_judged_at_the_root_gate` and
`a_deferred_run_check_that_never_passes_does_not_complete_the_run`.

## Amendment (2026-10-02): plan intake compares every public shape, not only unit structs

Plan intake noticed one phrasing: a unit struct that an instruction wrote with fields. An
instruction that also changed a trait method's signature was not noticed, and the crate that
called the method stopped building. The check was a text scan for `Name {` and `Name(` against
`struct Name;`; a changed trait method, function, enum variant or struct field has no such text.

- **Structural comparison.** `plan_shapes` reads the public declarations of the Rust files a
  contract writes with tree-sitter (struct kind and public fields, enum variants, trait methods,
  inherent methods, functions) and parses the code the instruction shows the same way (backtick
  spans, fenced blocks, bare items; a bare method signature is read with or without its `;`,
  then inside a trait body). A restated item is compared by shape: parameter names and spacing do
  not count; types, receiver, generics, modifiers and return type do. A change is a notice naming
  the item, the new shape and the existing one. Never a refusal: the user's instruction may ask
  for it.
- **What is not a change.** An item the files do not define (an addition), a private item, a
  private field, a variant the instruction does not restate, the same shape restated.
- **Pattern fallbacks, narrower than before.** A public unit struct given fields as text that is
  no item (`Name { order: Order }`) keeps the older wording. A sentence that names a public item
  in backticks with a verb of change is noticed when no code was shown, unless it says to keep,
  not to change, or to leave it. Files with no grammar keep the unit-struct scan. The text scan
  no longer reads `impl Name {` or `impl Trait for Name {` as a definition of `Name`.

Tests: `plan_shapes` unit tests (the trait restated, bare signatures and fences, free
functions, inherent methods, return types, struct and enum shapes, additions, private items,
prose fallbacks, one report per change); `tests/shape_guard.rs`
`an_instruction_that_changes_a_trait_signature_is_noticed_not_refused` and
`an_added_item_or_a_private_change_is_not_noticed`; `plan` `defines_with_fields`.

## Amendment (2026-10-06): the test script `npm init` writes is not a gate

Gate discovery takes `npm test` whenever `package.json` has a `test` script. The script
`npm init` writes, `echo "Error: no test specified" && exit 1`, only fails. A correct
JavaScript project that had not replaced it got a root gate that failed every time, opened a
remediation contract that no worker could satisfy, and held the run.

- **The rule.** A `test` script that says "no test specified" and exits 1 adds no gate. The
  discovery note says so ("npm test gate not applicable"), as it does for a pytest gate with no
  pytest, and the run's own `--gate-command` or a real test script brings the gate back.

Test: `kgraf-host` `the_npm_init_test_script_is_not_a_gate`.

## Amendment (2026-10-06, second): acceptance narrowing for Go, RSpec, Maven, Gradle and npm workspaces

A contract's acceptance was narrowed to the tests of its own files for unittest, pytest,
`node --test`, jest, vitest and Cargo only; every other runner ran the whole command, which a
split cannot pass per contract before its siblings merge. `scoped_check` now also narrows:

- `go test ./...` with options only and `go.mod` at the root: `go test ./<dir>` for the
  package of each declared `.go` file. A file in a nested module (a `go.mod` between it and the
  root) is not narrowed, since `go test ./<dir>` fails there; `-run`, `-skip`, `-bench`,
  `-list` and `-fuzz` are a selection, and the command runs whole.
- `rspec` / `bundle exec rspec` with options only: `<stem>_spec.rb`; `-e`, `--tag` and
  `--pattern` are a selection.
- `mvn` / `./mvnw` with a test goal (`test`, `verify`, `package`, `install`) and no `-pl`,
  `-f` or `-Dtest`: the test classes `<Stem>Test`, `<Stem>Tests`, `<Stem>IT` and `Test<Stem>`
  under `src/test/`, run as `-Dtest=<Class> -Dsurefire.failIfNoSpecifiedTests=false`, with
  `-pl <module> -am` when the root `pom.xml` lists the module. `-am` builds the modules it
  depends on (a module alone cannot resolve them until they are installed); the test filter
  keeps their tests from running.
- `gradle test` / `./gradlew test` with no selection, no short option and no `projectDir`
  in `settings.gradle`: `:<project>:test --tests <fully.qualified.Class>` per test class, the
  project named by its directory and listed in `settings.gradle`; a single-project build runs
  `test --tests <Class>`. A project's `test` task builds what it depends on and runs no other
  project's tests. A test in a project the settings do not name is not narrowed.
- `npm test --workspaces`, or `npm test` whose script is that: `npm test --workspace=<dir>`
  for the workspace each declared file is in, only when that workspace has a `test` script or
  the command passes `--if-present` (measured with npm 11: `--workspace=<dir>` runs the
  workspace's script; a workspace with no script fails).

`pytest <path>` was already a narrowed form. Each form runs only part of what the whole
command runs; a narrowed check that fails is the contract's own failure, so a form is added
only where it cannot fail for a reason the whole command would not.

Tests: `kgraf-host` `scoped_check.rs`
`go_test_narrows_to_the_packages_of_the_contracts_files`,
`rspec_narrows_to_the_spec_of_each_file`,
`maven_narrows_to_the_test_classes_and_the_listed_module`,
`gradle_narrows_to_the_project_test_task_of_each_class`,
`npm_workspaces_narrow_to_the_workspace_of_each_file`.

## Amendment (2026-10-06, third): Go, Maven, Gradle and .NET gates, and no gate whose dependencies are missing

Gate discovery found cargo, npm, pytest and `make test` only, so a Go, Maven, Gradle or .NET
project had no gate. A gate that cannot pass on a correct project is worse than none: it opens
a remediation contract no worker can satisfy and holds the run.

- **New gates, each only when it can run.** `go test ./...` for a root `go.mod`, when `go` is
  on `PATH` and each required module is in the module cache (or the project vendors);
  `mvn -o test` for a root `pom.xml`, when `mvn` is on `PATH` and `~/.m2/repository` holds the
  test plugin; `gradle test --offline` for a root Gradle build, when `gradle` is on `PATH`, the
  project has no wrapper and `~/.gradle/caches/modules-2` exists; `dotnet test` for a root
  solution or project file, when `dotnet` is on `PATH`, every project is restored and
  `~/.nuget/packages` exists.
- **The existing gates are held to the same rule.** `npm test` is no gate when a declared
  dependency is not installed; the pytest gate needs pytest in the root virtual environment or
  the host interpreter (declaring it is not installing it). With a virtual environment the
  command stays `python3 -m pytest`: the sandbox activates the environment.
- **Every gate left out is said,** with the missing install and its command, the missing
  tool, or, for a Gradle wrapper, that the wrapper writes a lock file in the user's Gradle home,
  which the sandbox keeps read-only, so the project needs a gate command of its own.

Tests: `kgraf-host` `go_maven_gradle_and_dotnet_gates_need_their_tool_and_their_installs`,
`an_npm_test_gate_needs_its_dependencies_installed`,
`a_virtual_environment_with_pytest_is_the_gate_s_interpreter`,
`a_setup_py_or_requirements_project_with_tests_gets_the_pytest_gate`.

## Amendment (2026-10-06, fourth): a Gradle wrapper project gets a gate

The third amendment above left a project with a Gradle wrapper without a gate: the wrapper takes
a lock in its distribution folder under the Gradle home, and the user's home is read-only in the
sandbox. A sandboxed command's Gradle home is now a folder of its own that reaches the user's
unpacked distribution read-only (decision 0084, amended 2026-10-06, fifth), so the wrapper runs
offline and writes its lock there.

- **The rule.** A root Gradle build with `gradlew` gets `./gradlew test --offline` when its
  `gradle/wrapper/gradle-wrapper.properties` names a distribution, that distribution is
  unpacked in `~/.gradle/wrapper/dists` (a `<hash>` folder holding its `.zip.ok` mark), and
  `~/.gradle/caches/modules-2` exists. `gradle` on `PATH` is not needed: the wrapper is the
  program.
- **Otherwise a note.** No distribution named: the properties file is named. Not unpacked:
  "run `./gradlew --version` once in the project root, with the network on". On Windows the
  distribution is not made reachable, so the wrapper gets no gate there and the note says so.

Not run against a real Gradle installation; the layout follows the wrapper's own lookup.

Test: `kgraf-host` `go_maven_gradle_and_dotnet_gates_need_their_tool_and_their_installs`.

## Amendment (2026-10-09): a cut check output keeps its error lines, and a note's span is not an error's

A contract that writes `pricing_engine/src/lib.rs` failed `cargo build --workspace` with six
errors in that file, and the check passed at the dispatch base, so the attempt's change caused
it (the verdict said so). The verdict kept only the last 1,000 bytes of the output. The one
`-->` line left was a `note: method defined here` in `iface/src/lib.rs`, the scaffold's trait
that the last error called; no error header and no file of the contract survived. The retry's
brief quoted that text, and a reader of the verdict could only place the failure in a file no
contract writes. The retry amendment above did not fire (no error header preceded the note's
span), but it could: an own error cut from the head and an outside error left in the tail
would read as "every located error is outside".

- **A cut output keeps its error lines** (`kgraf-host` `gates::read_log`, every check and gate
  output). When the output is longer than its cap, the lines of the cut part that name an error
  are kept first, under `[the error lines of the cut part:]`, then the end under
  `[the end of the output:]`: an error header (a line that begins with `error`, `fail:` or
  `failed `, in any case, or that holds `: error`, as gcc, clang, Go and tsc print it) and the
  `-->` span within two lines after it. Repeats are kept once. They take at most half the cap,
  from the end's share, so the whole stays within the cap. At most the first 8 MiB of the cut
  part is read for them.
- **A note's span is not an error's** (`outside_error`). The span read for an error is the line
  right after its header; any other line between clears it, so a later `note:` span (or another
  crate's, interleaved by cargo after `error: could not compile`) is never taken as the
  location of an error outside the contract's files.

The attribution itself is unchanged: a whole check that fails on an attempt and passes at its
dispatch base was broken by the attempt, whichever file the compiler names, and the retry is
told so; one that fails at the base too, with contracts of the plan not merged, is deferred to
the root gate.

Tests: `kgraf-host` `gates.rs`
`a_cut_compile_failure_keeps_its_error_lines_and_their_files` (the recorded shape: without the
error lines, the kept text names only `iface/src/lib.rs`) and
`located_lines_keep_headers_and_their_spans_within_the_budget`; `kgraf-orchestrator`
`outside_error` `a_notes_span_is_never_read_as_an_error_outside`.
