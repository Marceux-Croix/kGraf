# Decision 0039: A provider call requires an unforgeable ledger token, so cost accounting is a compile error rather than a review rule

**Status:** Accepted.
**Related:** decisions 0006, 0007, 0008

---

## Context

kGraf's standing rule is that no call to a model provider may be issued
outside the accounting path: a code path that can reach a provider without
being counted is a defect, not an optimization. As written, that is a rule
humans enforce by reading diffs.

Three things make that insufficient here.

kGraf writes its own code. The whole product is worker agents editing this
repository. A rule that depends on a reviewer noticing will be violated by a
worker that had a good local reason to add a fast path, and the violation
will look like a reasonable change.

The failure is invisible. An uncounted call still works. The response is
correct, the feature ships, and the only symptom is that the live cost
readout, a headline of the product, is quietly low.

The precedent is bad. An invariant defended only by a comment, with no test
behind it, is an invariant already lost to one careless edit.

A token construction addresses this one step further than a test would: not
a test that would notice the violation after the fact, but a signature under
which the violation cannot be written at all.

The design also has to handle an interrupted call. At least one provider
states plainly that an interrupted stream may never deliver its usage data.
A request that billed but was not counted violates the accounting rule just
as surely as one that was never counted at all, and the tempting handling is
to treat a usage-less call as a zero-cost call. That would fold a hole into
the total while making the total look complete. So the row survives,
flagged, with the provider's request identifier, which is the handle a
reconciliation pass needs.

Deriving cost rather than storing it follows from a related requirement
that the raw usage response be persisted verbatim: several provider
protocols surveyed changed shape within a year, and the point of keeping the
raw response is to be able to re-derive cost after a pricing or schema
change. A stored cost column would make that impossible for exactly the
rows that needed it.

## Decision

The mandatory provider method takes a `&CallToken` as an argument.

`CallToken` has no public constructor. The only way to obtain one is from
the ledger's `begin` call, which writes the call's row before returning it.

Therefore a code path that reaches a provider without being counted does not
compile. There is no runtime check, no lint, and no convention to remember.

The token is also a guard. Dropping it without settling marks the call
unreconciled rather than removing it, and the ledger reports those.

Cost is not a stored column. It is derived at read time from a price table,
and an unpriced model is reported as excluded from the total rather than
costed at zero.

## Consequences

There is no negative test for this clause, and there cannot be. The tests
state the mechanism in a comment and test the consequence, that every begun
call is on the record, settled or flagged, because the failing program
cannot be expressed in the language at all. That is the strongest available
form of the guarantee and the weakest available form of its test, and the
tradeoff is worth naming.

Every adapter must settle every path it can return from. An HTTP error
settles as failed, because nothing billed. A substituted model or an
undecodable response settles as unreconciled, because the call billed and
its cost is real but unverified. Getting that distinction wrong turns the
unreconciled list, which a human works through, into noise.

The ledger is a serialization point: one mutex-guarded connection, since
there is no second path and every worker in the fleet settles through it.
Deliberate: the write is one small row per provider call, orders of
magnitude cheaper than the call itself. If it ever binds, the fix is
batching within the ledger, never a bypass.

A poisoned ledger mutex is recovered, not propagated. One panic mid-write
must not make every subsequent call unaccountable, which would be a worse
accounting violation than continuing against a connection whose last
statement may have been incomplete.

The unreconciled list is a product surface, not a debug log. It is what the
user is shown when spend exists that kGraf cannot attribute. Hiding it would
be the silent degradation the design otherwise refuses to allow.

## Alternatives considered

**A convention, documented in the provider trait's doc comment.** Rejected:
this is the comment-without-a-test pattern already known to fail silently.

**A wrapper client that every caller is told to use.** Better, and still
rejected: the inner client remains callable, so the bypass exists and is one
import away. A worker optimizing a hot path will find it.

**A runtime assertion: panic if a call is issued without an open ledger
row.** Rejected: it catches the violation in the worst place, in
production, after the spend.

**A test that greps the source for direct client construction.** Rejected
as brittle and easy to defeat accidentally, and it tests the source text
rather than the mechanism.

**Make the accounted call the only signature that exists.** Chosen.

## Reactivation trigger

A provider protocol that cannot report usage at all, which would force a
decision about whether to estimate or refuse. The current position is
refuse: an explicit "usage missing" error rather than an invented number.
