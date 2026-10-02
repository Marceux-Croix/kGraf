# Decision 0031: The vacuity check runs against the stub and is two-sided; the handshake states assumptions about the counterparty

**Status:** Accepted. Closes both open questions in decision 0030.
**Related:** decisions 0022, 0023, 0026, 0029, 0030

---

## Context

### The vacuity hole, and why it closes for free

Decision 0030 required the contract's test to fail against an empty
implementation, and flagged that a test failing because nothing compiles
proves nothing about whether it asserts anything.

Decision 0030's second check already produces the fix. The stub-compile
check builds a trivial stub satisfying the producer contract. Running the
test against that stub rather than against emptiness guarantees compilation
by construction, so any failure is necessarily an assertion failure. The two
checks compose, and the hole was an artifact of running them independently.

### Why it must also be two-sided

There is a second vacuity mode decision 0030 did not name: a contract test
that nothing can satisfy. It fails against the stub, passes the check, and
then both workers fail against it. The failure presents as two bad workers
rather than one bad contract, which is the worst possible misattribution.

So the test must also pass against a correct reference implementation.
Failing against a wrong implementation and passing against a right one is
what establishes that the test constrains something achievable.

An optional, stronger version uses a second stub returning plausible-but-
wrong values rather than defaults or panics. A test that only catches an
unimplemented placeholder catches very little. This is mutation-style
discipline and is harder to generate, so it is a refinement rather than a
requirement.

### What the handshake should contain

Decision 0030 adopted the handshake as an experiment and flagged that
getting its content wrong is the likeliest reason it returns null.

The valuable content is not what an agent will do. That is already in its
brief, which the orchestrator wrote. Restating it adds tokens and no
information.

The valuable content is what an agent assumes about the other. Expectation
failure, at 42 percent of measured multi-agent failures, is precisely one
party assuming the other would deliver something it did not. That assumption
is never written down, which is why it diverges silently.

Four elements:

1. **What I will provide**, in the contract's terms.
2. **What I assume you will provide.**
3. **What I assume will not change** while I work.
4. **What I will touch**, which is the write-scope declaration used for
   collision control.

Element 3 is the one usually left implicit and the one that breaks. An agent
depending on something staying stable, without saying so, is the same class
of failure as the undeclared scope that defeated a published admission-
control mechanism measured elsewhere.

### Prose, and still mechanically comparable

Agent communication is required to be plain English. A structured diff
requires structure.

These reconcile through anchoring: the handshake is prose that names
contract symbols. A statement that `parse_config` returns a `Config` is
readable and still comparable at the symbol level. The orchestrator extracts
the symbol-level claims and compares them; a person reads the prose.

### Why the orchestrator compares, and why restating beats confirming

Agents must not be relied on to detect their own mismatch. They already
failed to, and that failure is the same 42 percent this mechanism addresses.
The orchestrator holds both statements and compares them.

And a counterparty must restate in its own words rather than confirm. An
agent asked to confirm an assumption will tend to agree, which produces an
echo rather than evidence. Two independent statements can be compared; a
statement and its acknowledgment cannot.

## Decision

The vacuity check runs against the stub, not against emptiness, and is
two-sided: the contract's test must fail against a wrong implementation and
pass against a correct one.

The handshake states assumptions about the counterparty, not a restatement
of one's own brief. Four elements, anchored to contract symbols.

The counterparty restates rather than confirms, and the orchestrator
compares the two statements. Agents do not check their own agreement.

## Consequences

The three pre-dispatch checks are now ordered and dependent. The stub is
built first, the vacuity check consumes it, and the handshake follows. They
are not independent and should not be run as such.

A reference implementation is needed for the two-sided check. For a small
contract this is cheap; for a large one it is not, which is another force
pushing toward smaller chunks.

Handshake mismatch is an orchestrator-detected event, joining existing
blocking states rendered on the graph: a node held pending handshake
reconciliation is distinct from dependency-blocked, collision-deferred,
concurrency-capped, and contract-under-review.

The experiment sharpens. Decision 0030 could not say what a null result
would mean, because the content was unspecified. With content specified, a
null result means agents cannot articulate their assumptions about each
other usefully, which is a finding about model capability rather than about
the design.

## Open questions

Whether element 3 can be answered at all. Stating what one assumes will not
change requires knowing what one depends on, which is close to the
blast-radius prediction problem already found not to work for a call graph.
An agent may be no better at this.

## Alternatives considered

**Keep the vacuity check against emptiness and accept the hole.** Rejected
once it became clear the stub check already closes it at no cost.

**One-sided vacuity check.** Rejected on the unsatisfiable-contract case,
which misattributes a contract defect to two workers.

**Handshake as a restatement of one's own brief.** Rejected. It is the
obvious design, it costs tokens, and it carries no information the
orchestrator does not already have.

**Structured handshake format instead of prose.** Rejected. Anchoring to
symbols gets mechanical comparison without giving up readability.

**Agents confirm each other's handshakes.** Rejected on sycophantic echo,
and because it delegates detection to the parties that already failed to
detect.

## Amendment (2026-09-30): a failing check goes back to the planner first

A plan whose interface failed a mechanical check ended the run with no dispatch, on a
mistake a second ask could fix: an interface with no test files, stubs of the wrong trait
arity, a test importing the wrong crate. These now go back to the planner through the
existing bounded re-ask before any hold.

- **The mechanical checks (artifact, stub-compile, vacuity) run on each planner answer.** A
  failing interface refuses the answer with the check's failure text, and the planner is
  asked again, at most twice. The last answer is taken, and the pre-dispatch gate holds the
  parties on the same failure as before. A check that could not run is left to the gate.
  The gate runs the checks again at dispatch, against the tip of that moment.
- **A check's kept output leads with the compiler's errors.** Error blocks (the error line
  and its location and context lines) come first within the 4096-byte cap, then the tail,
  so trailing toolchain warnings cannot push out the error that explains the failure.
- **Two artifact refusals, fed to the same re-ask:** a contract test at the path of a file
  the base already tracks, with different content (it would make the check validate a
  rewrite of the project's own test); and stub, consumer or test files naming one path with
  different content (overlaid in one tree, the later would silently win).

## Amendment (2026-09-30): a plan held before any work goes back to the planner once

A handshake runs only at the pre-dispatch gate, after planning, and the plan taken on the
last ask is not pre-checked, so these failures never reached the planner. The gate held the
parties, and with nothing else to do the run ended blocked.

- **When the run would end on a hold, and nothing has acted on the plan**, the whole plan
  goes back to the planner once with the failures of every held interface, a handshake's
  findings included. "Nothing has acted" means: no contract dispatched or verified, no
  merge onto the integration branch, no interface stub landed, and no interface the run was
  given before planning binds the plan. The orchestrator ends any run whose remaining
  contracts are all held, interactive or not, so the rule applies to every run mode; a run
  from an approved plan is never re-planned (its plan is the user's).
- **The re-plan is one ask, taken as it is**: no re-ask, and the gate checks it as any
  plan. A hold on it ends the run. The bound: the two re-asks of the first plan stay; the
  re-plan adds one ask, so the top-level plan is asked for at most four times. A re-plan
  answer refused outright (for example, invalid JSON) leaves the held plan, and the run
  ends on its hold with the refusal noticed.
- **The re-plan's contracts and interfaces get ids of their own** (`re1.c1`, `re1.i1`). The
  held plan's interfaces stay registered under their ids, because the book keeps an
  interface once registered; a new contract reusing an old id would be bound, and held, by
  the old plan's failed interface.
- **Recorded:** `replans` and, until the re-plan is taken, `replan_note` in the run record,
  so a resume in between still sends the failures; a notice and a trace `notice` event name
  the failures.

## Amendment (2026-10-02): the re-plan is re-asked, and its failures reach only the contracts they concern

These bullets supersede the matching parts of the two 2026-09-30 amendments above.

- **No plan is taken without its checks.** The interface checks run on every ask of the
  interface call (decision 0030, amendment of 2026-10-02); an interface that still fails after
  that call's budget is not registered, and its edges are counted as not run. The last ask's
  plan is no longer taken unchecked for the gate to hold on the same failure. The gate still
  runs the checks again at dispatch, and the handshake runs only there.
- **The re-plan's contracts answer is re-asked** like the first plan's, at most twice, on an
  answer that is not JSON or breaks a plan rule; its interfaces get the interface call's own
  budget. A live re-plan ended on one answer whose file `content` string was never closed, so
  a line break fell inside it and the parser stopped on a control character. A hold on the
  re-plan still ends the run (`MAX_REPLANS = 1`).
- **A re-planned contract's brief carries only the failures that concern it**, as a quoted
  data block: those of the held plan's interfaces that bound the contract it replaces (the
  same id before the `re1.` prefix) or that name a file it writes. A contract no failure
  concerns gets none; before, every re-planned worker's brief carried every failure (requests
  of 20 to 48 KB). The run record keeps the failures one by one (`replan_failure_parts`: the
  text, the held parties and the interface's files) next to the joined text.

Tests: `contract_flow.rs` `a_re_plan_answer_that_is_not_json_is_asked_again` (the recorded
re-plan answer), `a_held_plans_failure_reaches_only_the_contracts_it_concerns`,
`a_handshake_failure_before_any_work_is_sent_back_to_the_planner_once`,
`a_handshake_that_keeps_failing_is_held_after_one_re_plan`.

Renamed tests: `after_two_re_asks_a_failing_interface_is_held` is
`after_three_asks_a_failing_interface_is_counted_unchecked_and_the_run_goes_on`;
`a_plan_the_gate_holds_before_any_work_is_re_planned_once_with_the_failure` is
`a_held_plans_failure_reaches_only_the_contracts_it_concerns` (a stub-compile failure no longer
reaches the gate, so a handshake mismatch holds the plan there).
