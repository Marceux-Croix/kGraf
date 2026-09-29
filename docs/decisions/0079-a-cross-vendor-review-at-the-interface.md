# Decision 0079: A producer's delivery gets one review from a model on a cross-vendor consumer's route, off by default

**Status:** Accepted
**Related:** decisions 0012, 0024, 0026, 0029, 0035, 0036, 0037

## Context

kGraf already lets any worker challenge a contract's own reasoning. That mechanism is keyed to
an optional, explicitly declared interface artifact between a producer and a consumer, which
most producer-consumer relationships in a run do not have; the general dependency relation
between contracts is much broader than the narrower set of pairs that get a formal interface
artifact written for them.

## Decision

**When a producer's work has passed its own checks, and before it is counted as verified, and
the setting is turned on, and at least one contract depending on it is routed to a different
model provider than the producer's own attempt ran on:** one review call is sent to a model on
that dependent contract's own route. It is shown the contract's brief, its declared symbols,
and only the producer's delivered files that fall within its own declared write scope, nothing
else.

**The review is advisory.** A finding must name a specific contract symbol and the mismatch it
found; it never overrides the actual verification result on its own, and it is recorded the
same way a worker's own self-reported inadequacy claim already is, so it reaches the same
attention path. An answer that cannot be parsed is recorded and surfaced, never silently
treated as approval.

**Off by default.** Its benefit is not yet measured: kGraf does not yet know whether a
different vendor's model reliably catches interface problems a same-vendor verification would
miss.

**No cost cap and no refusal.** Consistent with kGraf's stance that it does not police a
user's own spend, this adds at most one extra call per producer verification, and its cost is
recorded in the ledger like any other call.

## Consequences

- In a run where every consumer of a producer happens to share its vendor, this mechanism does
  nothing, silently and by design; that is the expected, common case, not a failure.
- The reviewed content is exactly the producer's own declared write scope, narrowed to the
  files its attempt actually changed, which is deterministic and reproducible from the run's
  own record, and keeps the cost of the call bounded and predictable.
- Turning this on by default, or widening it to review every dependent rather than one picked
  deterministically, would follow from a future measurement showing its findings actually
  correlate with real defects a same-vendor verification missed.

## Alternatives considered

- **Route every finding through the existing challenge mechanism.** Rejected as the default
  path: that mechanism requires the formal interface artifact to exist for the pair, which is
  the minority case for a general dependency relationship.
- **Use one fixed, dedicated reviewer model rather than the dependent's own route.** Rejected:
  it drops the independence argument that a same-lineage reviewer is correlated with the
  producer's own failure modes, and it adds a model and a route the run's own configuration
  does not otherwise use.
- **Block or automatically fail a contract on a review finding.** Rejected outright: a review
  finding is advisory by design, the same as any other challenge.
