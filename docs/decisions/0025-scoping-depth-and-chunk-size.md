# Decision 0025: Scope deeply on interfaces, and chunk size has an optimum rather than a direction

**Status:** Accepted. Refines decision 0014's recursion floor, which said
decomposition stops when a part fits one worker's brief, without saying what
determines that.
**Related:** decisions 0004, 0012, 0014, 0015, 0017, 0019, 0023, 0024

---

## Context

Decision 0023 recorded that expectation accounts for 42 percent and
commitment 32 percent of multi-agent failures. Decision 0024 recorded that
these are what a supervisor writing both briefs structurally addresses.

Two design instincts follow, and they need different treatment.

### Why scoping depth is endorsed, with a limit

Expectation failure is a scoping failure by definition: one worker assumed
another would deliver something it did not. More thorough upfront
specification attacks it directly.

The cost profile is favorable. Scoping is paid once and amortized across
every worker it governs, and it is paid in orchestrator effort rather than
worker effort.

The limit is that decomposition is ongoing under decision 0015: the plan
changes while work proceeds, so specifying everything before starting is both
impossible and a return to a waterfall process. Implementation cannot be
predicted.

Interfaces can be. What a part delivers, in what shape, to whom, is
specifiable before the implementation is known. That is the distinction:
deep on contracts, shallow on mechanism.

### Why smaller chunks are not simply better

What improves as chunks shrink:

- Per-pair collision probability falls. The mean merge scenario has 0.63
  files changed by both branches, and smaller changes push that down.
- Briefs become specifiable. A small task can be fully described; a large one
  cannot, which is where expectation failure originates.
- Delivery becomes verifiable. Checking a small result against a small brief
  is tractable in a way that checking a large one is not.

What worsens as chunks shrink:

- Per-chunk orchestrator overhead is linear. Every chunk costs a brief, a
  dispatch, and a result fold-in. Halving chunk size doubles that load,
  against an entity that is already a context bottleneck.
- Prompt cache amortization degrades sharply at the short end. A worker
  running 4 turns pays roughly 3.3 times more per turn for the same prefix
  than one running 60, because amortization is proportional to turns riding
  the cached prefix. Smaller chunks mean shorter-lived workers, which is the
  worst possible shape for caching.
- Gate count couples to hierarchy depth. A gate, a validation checkpoint,
  attaches to every node with children, so a deeper decomposition carries
  more levels of validation. A five-level decomposition would carry four
  levels of gates. Chunk size and gate cost are the same question.

What does not worsen, contrary to the obvious concern: concurrent pair count
does not grow quadratically with chunk count, because a concurrency cap
bounds simultaneous pairs regardless of how many chunks exist. The cap
already protects against that failure.

## Decision

Scoping is deep on interfaces and shallow on implementation. The orchestrator
specifies what each part delivers to the others in detail, and does not
attempt to specify how each part will be built.

Chunk size is a tuned parameter with an optimum, not a direction. Smaller is
better along some axes and worse along others, and the balance is
measurable.

The recursion floor from decision 0014 is tuned against the forces above,
rather than against an intuition about what fits a brief.

## Consequences

The interface contract is a first-class part of a brief, distinct from the
task description. Two individually clear briefs that assume different
interfaces is the expectation-failure case, and only the contract prevents
it.

Chunk size becomes a measured parameter, evaluated against task success and
total cost, not against collision counts alone. The measurement must include
cache hit rates, because the cache penalty at the short end is invisible to a
success-only comparison and is the cost most likely to be missed.

What counts as a major node for gating, and where decomposition stops, are
both answers to "how small should a chunk be." They should be decided
together rather than separately.

Scoping effort is front-loaded and amortized. Orchestrator effort spent
specifying interfaces is cheap relative to worker effort spent building the
wrong thing, which is the trade this record rests on.

## Open questions

The optimum is unknown and probably project-dependent. A codebase with
tightly coupled modules and one with well-separated ones will not share a
chunk size. Whether kGraf should infer it, expose it as a setting, or adapt
it from observed failure rates is unresolved.

How an interface contract is expressed remains unspecified. Natural language
between two workers is what the failing peer configuration already had, so
something more structured is likely required.

## Alternatives considered

**Specify everything upfront.** Rejected. It contradicts the ongoing-
decomposition requirement, it is impossible for implementation, and it is a
waterfall process.

**Chunk as small as possible.** Rejected. It optimizes three axes and
degrades three others, two of which, cache economics and gate count, are
real costs that do not appear in a collision-focused analysis.

**Chunk as large as a worker can handle.** Rejected for the opposite reason:
large chunks are where expectation failure originates, because they cannot be
fully specified or cheaply verified.

**Deep interface scoping, tuned chunk size.** Chosen.
