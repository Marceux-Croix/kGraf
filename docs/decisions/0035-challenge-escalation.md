# Decision 0035: The orchestrator may overturn itself; a re-raised challenge escalates to the user

**Status:** Accepted. Closes the open question in decision 0029 on who
reviews the reviewer.
**Related:** decisions 0004, 0027, 0029, 0033

---

## Context

### One class of challenge is mechanically resolvable

Decision 0033 made read sets first-class and observed rather than declared,
for agents. The same observation covers the orchestrator.

So a challenge of the form "you did not read X" is answerable from the
record, not from the orchestrator's account of itself. Whether it consulted
a document, a decision record, or a standing rule is a fact, and an error of
omission demonstrated against that record is clear evidence rather than a
matter of judgment.

The same holds for a contract that contradicts a recorded decision or a
standing rule: the contradiction is checkable.

This matters because it removes the least trustworthy part of self-review.
The orchestrator is not being asked to concede it reasoned badly. It is
being shown that it lacked an input.

### Why a re-raise is the right trigger

Decision 0029 required an upheld challenge to be recorded with its reasons,
so it is not raised again indefinitely.

Re-raising after seeing those reasons is therefore already meaningful. The
challenger has read the rejection and maintains its position, which is the
definition of a genuine dispute rather than a misunderstanding.

Using it as the escalation trigger solves two problems with one rule: the
loop the earlier record was guarding against does not occur, because the
second raise escalates rather than repeats, and arbitration is reserved for
disputes that survived an explanation.

### Why user arbitration does not weaken orchestrator authority

The orchestrator holds full directional control over the agents. The user
has always been above the orchestrator, interacting with it directly at all
times.

Escalation is not a new authority. It is the existing hierarchy made
explicit at the one point where the orchestrator cannot be its own check.

## Decision

A challenge resolves three ways.

**Self-overturn.** Where the error is clear on review, the orchestrator
reverses its own contract and amends or supersedes it. A clear error
includes an omission: context it demonstrably did not consult.

**Uphold.** Where the contract is sound, the orchestrator upholds it and
records the reasons.

**Escalate.** Where it is genuinely contested, it goes to the user, who is
the final interpreter and arbiter.

The escalation trigger is a re-raised challenge. A challenger that maintains
its objection after reading the recorded reasons for rejection escalates it.

## Consequences

Escalation frequency is a quality metric, not just a workflow. A high rate
means the orchestrator is writing bad contracts, which is a finding about
the system rather than about the challengers. It should be reported.

An escalation carries a compact payload: the contract, the challenge, the
reasons for upholding, and the rebuttal. The user arbitrates by reading four
things, not by reconstructing a history.

Escalation is a visible blocking state, joining other states rendered on the
graph, distinct from dependency-blocked, collision-deferred,
concurrency-capped, and contract-under-review.

The orchestrator's own read set becomes evidence. Making it observable was
justified for agents; it now also serves as the record against which the
orchestrator's omissions are checked.

Self-overturn must be recorded as such, not silently folded into a
supersession. A contract reversed because the orchestrator lacked context is
a different event from one revised because requirements changed, and the
distinction is what makes the escalation-rate metric meaningful.

## Open questions

Whether the user should see challenges that resolved without escalation.
Seeing them costs attention; not seeing them means a pattern of
self-overturns is invisible until the metric is inspected.

## Alternatives considered

**Orchestrator decides, always, with no escalation.** Rejected. A
systematically wrong orchestrator is unreachable, and the gate is both late
and blind to anything that compiles and passes tests.

**Escalate every challenge.** Rejected. It costs the user's attention, which
is the product. Most challenges should resolve without them.

**A quorum of agents can overrule the orchestrator.** Rejected. It inverts
the orchestrator's directional control, and agents drawn from similar models
are correlated, so a quorum is weaker evidence than it appears.

**Route review to a fresh context or a different model.** Not rejected, and
not required here. It follows the same independence logic recorded for
cross-vendor challenges, and it costs a model call. Recommended, left open
for future design.
