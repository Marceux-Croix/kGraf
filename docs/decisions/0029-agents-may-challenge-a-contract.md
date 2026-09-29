# Decision 0029: Agents may challenge a contract; the orchestrator reviews and decides

**Status:** Accepted. Closes the expectation-failure gap flagged in decisions
0024 and 0026.
**Related:** decisions 0004, 0009, 0012, 0024, 0026, 0027

---

## Context

Decision 0027 added a fifth report kind for contract inadequacy, triggered
when a worker cannot satisfy the contract it was given. That catches
individual failure.

It does not catch the failure decision 0024 flagged as likeliest. If the
orchestrator writes a wrong interface, both workers build correctly against
it. Each satisfies its contract. The compiler passes and the tests pass.
Nobody fails individually, so nothing in decision 0027 fires, and the problem
surfaces only at integration or not at all.

Reaching that case needs a signal that does not depend on a worker failing.
An agent noticing that what it has correctly built does not make sense is
one. Two agents comparing notes in their shared conversation file and jointly
concluding the contract is wrong is a stronger one.

### Why a challenge must be advisory

The orchestrator holds full directional control over dispatched work. A
binding challenge would let any agent stall work, and would erode that
control by increments rather than by a decision.

Advisory preserves it exactly: agents supply evidence, the director decides.
This is the same shape as other places the user or an agent influences a
choice the orchestrator otherwise owns.

Recording the rejection matters as much as permitting it. Without a record,
the same challenge recurs, and an agent that has been overruled has no way
to know it was considered.

### Group challenges are stronger evidence, and less independent than they look

Several agents converging on the same objection is better evidence than one.
But agents drawn from the same model are correlated, and their agreement
partly reflects a shared prior rather than independent judgment.

A challenge raised by workers on different vendors' models is materially
more independent than the same challenge from two instances of one model.
Their failure modes are not shared, so convergence carries real information.

This is a separate argument for using models from multiple vendors, distinct
from routing tasks to whichever vendor fits them best. Uncorrelated judgment
is a second, separate benefit, and it applies precisely where the design is
weakest: detecting that the orchestrator itself was wrong.

## Decision

Any agent may challenge a contract, sending it back to the orchestrator for
review. A challenge may be raised by one agent or jointly by several.

A challenge is advisory. The orchestrator reviews and decides. It may uphold
the contract, amend it, or supersede it.

An upheld challenge is recorded with the reasons it was rejected, so it is
not raised again indefinitely.

## Consequences

The orchestrator must be able to evaluate a challenge to its own work. This
is the hardest part, and it is not obviously solved by asking the same
orchestrator that wrote the contract. Whether review should route to a fresh
context, or to a different model, is a question not decided here.

Challenges and their outcomes are recorded in the conversation files, so the
reasoning survives and a later agent can see that a contract was questioned
and why it stood.

A challenge is visible on the graph. A contract under review is a blocking
state on the nodes depending on it, using existing graph machinery, distinct
from dependency-blocked, collision-deferred, and concurrency-capped.

Cross-vendor challenge independence should be measured, not assumed. Whether
agents on different models actually disagree usefully, or converge anyway
because they share training data and context, is testable and currently
unknown.

## Open questions

Who reviews the reviewer. If the orchestrator wrote the contract and also
judges the challenge, a systematically wrong orchestrator is unreachable. A
gate remains the only independent check, and it is late.

## Alternatives considered

**Rely on the gate.** Retained as a backstop, rejected as the primary path.
The gate is the latest possible detection point, after both workers have
built against a bad contract.

**Make challenges binding.** Rejected. It lets one agent halt work and
inverts the orchestrator's directional control.

**Permit only individual challenges.** Rejected. The failure this record
exists to catch is one where no individual agent fails.

**Require a quorum before a challenge is heard.** Rejected as premature. A
single well-reasoned objection may be correct, and the orchestrator is
capable of weighing one. Worth revisiting if frivolous challenges prove to be
a real cost.
