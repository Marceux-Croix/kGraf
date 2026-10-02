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
