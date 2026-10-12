# Decision 0019: Every major node carries a gate, and done includes tests

**Status:** Accepted. Refines decision 0018's gate cadence, which was
configurable and time-based. The rest of decision 0018 stands.
**Amends:** 0018 (gate cadence)
**Related:** decisions 0007, 0013, 0014, 0018

## Context

Leaving gate cadence as a configurable interval with orchestrator
discretion is workable, but attaching gates to the hierarchy instead is
both simpler and more predictable: gate cost scales with the shape of the
plan rather than with elapsed time, and a reader of the graph can see where
validation will happen without knowing a timer's setting.

Writing tests for what you build is ordinary practice and needs no
justification here. Two of its consequences in this system are not
ordinary, and those are what this record exists to fix.

### It resolves an apparent conflict with decision 0013

Decision 0013 requires that gates be discovered and never invented: a
project with no discoverable validation commands gets no gate and is told
so.

That appears to conflict with workers producing tests, which looks like
kGraf creating gate material. The distinction reconciles them: the runner
is discovered, the content is produced. kGraf never fabricates a plausible
test command; it learns the command from the project and fills it with
work its agents did.

For a greenfield project with no test infrastructure at all, establishing
that infrastructure is itself a work node, visible on the graph like any
other, not something conjured silently at gate time.

### It changes how doneness derives

Under decisions 0007 and 0014, doneness is derived and never stored. If
done means code plus passing tests, derivation must check test existence
and status, not merely whether code exists.

This lands on a simple rule: code with passing tests derives as complete;
code without tests derives as partial. The status word is not a label
someone applies but a consequence of what is on disk.

## Decision

**Every major node carries a gate.** A major node is one with children:
gates sit at the joins of the work hierarchy, firing when a node's children
complete. Leaf tasks do not each carry a full gate; their parent's gate
covers them.

Cadence is therefore structural, not temporal. A timed gate remains
available as a supplement for long-running work, but the hierarchy is the
mechanism.

**A work node is not done when its code exists. It is done when its code
exists and has passing tests.** Producing tests for what it produced is
part of a worker's brief.

**The test runner is discovered; the test content is produced.** Discovery
comes from the corpus under decision 0013. What the runner executes comes
from the workers.

## Consequences

**Gate count is a function of hierarchy shape**, known once the plan is
decomposed. A wide shallow plan gates more often than a narrow deep one,
and that is visible in advance rather than emerging from a timer.

**Gates still compete for the concurrency budget** under decision 0017, so
a plan with many major nodes spends more of its budget on validation. This
is a real tradeoff and it is exposed, not hidden.

**Resuming work on an existing repository requires distinguishing finished
work from work that is merely written.** A documented status of complete
over untested code is a discrepancy kGraf reports.

**The test is that the gate runs and blocks**, not that a gate node exists
in the graph. A gate that is present and never enforces is the same
advisory-limit failure described in decision 0017.

**A worker that cannot test what it produced must say so**, rather than
returning done. Some work is genuinely hard to unit test, and the honest
outcome is a node reporting partial with the reason, not a node reporting
complete because the tests were skipped.

## Open

What counts as a major node in a deep hierarchy is not fully settled. Every
node with children is the current reading, but a five-level decomposition
would then carry four levels of gates, which may be more validation than
the work warrants. Whether gates should attach only above a certain depth,
or only where a node's children touch more than one region of the code
graph, is an open question.

## Alternatives considered

**Timed gate cadence**, as decision 0018 had it. Rejected as the primary
mechanism: its cost is unpredictable, it is invisible in the plan's
structure, and it has no principled value.

**A gate on every node including leaves.** Rejected on cost. A full
validation run per leaf task would dominate the work, and a leaf's
correctness is covered by its parent's gate.

**Tests as a separate phase or a follow-up node.** Rejected. Deferred tests
are tests that do not get written, and a node that reports done without
them would make derived doneness a lie. Keeping tests inside the brief
keeps the derivation honest.
