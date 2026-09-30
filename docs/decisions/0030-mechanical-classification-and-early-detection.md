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
