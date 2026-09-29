# Decision 0037: An upheld challenge names the artifact it was checked against, or states plainly that it is judgment; a re-raise escalates only with new artifact-backed evidence

**Status:** Accepted, refined after implementation.
**Related:** decisions 0029, 0035, 0036

---

## Context

Decision 0029 required an upheld challenge to be recorded with its reasons.
Decision 0036 established that an error is clear when it checks against a
recorded artifact. It left open whether a rejection should name the artifact
it checked.

It should, with one qualifier that matters more than the requirement.

### Why the requirement must be conditional

An unconditional requirement creates pressure to fabricate. Asked always to
cite an artifact, the orchestrator will produce one where none applies, and
a fabricated citation is worse than an honest absence because it carries the
appearance of evidence without the substance.

So the rule is conditional by design. Where an artifact settles the matter,
name it. Where the matter is judgment, say that it is judgment.

### The format calibrates the challenger

This turns out to be the more useful property.

"Rejected on artifact X" tells the challenger it is wrong, and where to look
to understand why. Re-raising against a citable record is unlikely to be
productive.

"Rejected on judgment" tells the challenger that no record settles this, and
therefore that re-raising escalates to the user. That is precisely the
information needed to decide whether to press.

A rejection that does not distinguish these leaves the challenger guessing,
and the escalation path depends on a challenger deciding whether to maintain
its objection.

### Why a bare re-raise should not itself escalate

Once the design was implemented, a gap appeared: any re-raise of a rejected
challenge escalated to the user, including one that repeated the same
objection with no new evidence. That let a challenger reach the user by
simple repetition, which defeats the purpose of a mechanically checked
rejection and inflates the escalation-frequency metric with disputes the
record had already settled.

The fix narrows the trigger. A re-raise is keyed to the challenger's
contract lineage, so a retried worker is treated as the same challenger
rather than a fresh one. A re-raise must bring a new artifact-backed claim,
evidence classified as "not consulted," "recompute," or "quote", and that
evidence must differ from what the original challenge cited. A re-raise
citing nothing, or citing the same thing again, does not escalate: the
recorded rejection stands.

The new claim is looked up first, through the same process as a first
challenge. If the record settles it, that result is final and nothing
escalates. If the record cannot settle it, it escalates to the user with the
same four-part payload: contract, challenge, rejection, rebuttal.

### Why escalation additionally waits on one verification pass

A further narrowing applies specifically to a re-raise built on a quoted
passage from a project file. Under the first narrowing, any verified quote
escalated immediately, which let a challenger put any dispute before the
user by quoting any line of any file. Escalation frequency is a quality
metric, and it should count disputes that the record and one verification
pass together could not settle.

So a re-raise built on a quote escalates only when: the quote is verified to
exist in the cited file; one bounded check of the contract against the quote
finds a contradiction; and that check cites no passage verbatim in the
contract, meaning the record cannot settle the contradiction on its own.

If the check instead finds a verbatim contradicting passage in the contract,
the orchestrator self-overturns, as for a first challenge. If it finds no
contradiction, the challenge is rejected on judgment. If the check budget for
that contract version is exhausted, it is rejected on judgment. A claim with
no quote that the record cannot settle, an omission claim when no read set
was recorded, is also rejected on judgment, since with no quote there is
nothing to check against the contract.

A bounded per-worker challenge budget, and a rule that a lineage with an open
challenge on an interface cannot open another on the same interface, keep
this process from growing without limit.

## Decision

When the orchestrator upholds a challenge and a recorded artifact settles
it, the rejection names that artifact.

When no artifact settles it, the rejection says so plainly. It does not cite
one.

A re-raise escalates to the user only when it brings a new artifact-backed
claim that the record cannot settle, and, where that claim is a quote, only
after one bounded check confirms a contradiction between the contract and
the quote that the contract's own text does not already resolve.

## Consequences

"Rejected on judgment" is a signal, not a weakness. It should not be written
apologetically or avoided, because it is the honest label for the category
escalation exists to serve.

Rejections become auditable. A citation can be followed and checked, which
matters given that escalation frequency is a quality metric: a pattern of
rejections citing nothing is itself visible.

Verbosity is bounded. A citation is a reference, not a restatement. The
artifact is already recorded and searchable.

A re-raise costs a worker one unit of a bounded challenge budget whatever its
outcome, so a lineage cannot use unlimited re-raises to probe for an
escalation.

## Open questions

Whether a challenger should be able to dispute the citation rather than the
contract, when it believes the named artifact does not say what the
rejection claims. That is a different kind of challenge from the original
one and may need its own route.

## Alternatives considered

None recorded separately for the original rule. The two later narrowings
were chosen over leaving each gap open, for the reasons stated above.
