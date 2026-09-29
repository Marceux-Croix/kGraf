# Decision 0077: A worker's result carries its own written reasons, for a dependent worker to read

**Status:** Accepted
**Related:** decisions 0005, 0026, 0043, 0057, 0071

## Context

A contract's worker sometimes makes a judgment call a later, dependent contract's own worker
needs to know about to build against it correctly: which of two equally valid designs it
picked, what it deliberately chose not to do, or what it assumed the dependent would do with
what it delivered. None of that reached the dependent before this decision; a dependent
worker saw only the finished code and the accepted result, never the reasoning behind it. When
the dependent is on a different model or vendor from the producer, it has no way to ask the
producer what it was thinking.

## Decision

A worker's result can carry an optional, short written explanation: what it decided and why,
what it considered and rejected, and what it assumed about the contracts it depends on or
that depend on it. This is bounded to a small amount of text, and is written by the worker
itself into its own final answer, in a form its instructions ask for. When a dependent
contract is later dispatched, the producer's explanation is included in its brief, clearly
labeled as another model's own words, describing the reasoning it produced, and quoted as
reference material rather than as an instruction.

This is different from a model's private reasoning trace, which never crosses between
different models under kGraf's design, because it is meaningful only in the context of the
specific model that produced it. A written explanation is not that: it is the model's own
ordinary, composed prose, the same kind of text that already crosses between agents as a
report or a message.

A worker that gives no explanation is recorded as having given none; kGraf never invents one
on its behalf.

## Consequences

- A dependency's explanation is quoted as data in the dependent's private brief, not woven into
  the shared portion of its instructions, so it never disturbs the model providers' own cached,
  reused context.
- If an explanation runs over its length limit, it is cut, and the cut is marked plainly rather
  than silently dropped.
- A contract that closes without an explanation (because the worker gave none, or the contract
  was replaced rather than verified) simply contributes nothing to its dependent's brief;
  nothing is fabricated to fill the gap.

## Alternatives considered

- **A dedicated structured field for the explanation, filled through its own separate model
  call.** Rejected for now: it adds a cost to every worker's request, whether or not that
  worker ever produces an explanation. Reading it out of the worker's ordinary final answer
  costs nothing extra on rounds that carry no explanation at all.
- **Fabricate a plausible-sounding explanation from the code change if the worker gave none.**
  Rejected outright: a reader would have no way to tell kGraf's own invented explanation from
  the producer's real one, which is worse than no explanation at all.
- **Leave the length unbounded.** Rejected: every dependency's explanation is added to every
  one of its dependents' own requests, so an unbounded explanation is an unbounded, compounding
  cost.
