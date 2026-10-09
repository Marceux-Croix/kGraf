# Decision 0082: At most two contracts of one coupling group run at the same time

**Status:** Accepted
**Related:** decisions 0012, 0017, 0023, 0067, 0074, 0075

## Context

Measured success falls sharply as the number of concurrent agents rises, and cross-vendor
pairs of agents conflict measurably more often than same-vendor pairs. That risk is not spread
evenly: two contracts editing unrelated files are not competing over the same lines or the
same interface the way two contracts whose work is genuinely coupled (they overlap in the
files they write, depend on each other, or tend to change together) are. Lowering the overall
concurrency limit to guard against this would slow down every contract in a run, including the
large majority that are not coupled to anything else.

## Decision

**A new limit holds a coupled contract back from starting while two other members of its own
coupling group are already running** (a default of two at once). The grouping used is the same
one kGraf's own coupling-aware routing already computes: contracts that overlap in what they
write, that depend on each other, or that tend to change together are one group.

**A contract held back by this limit is deferred, not refused.** It stays ready to run and is
retried automatically as soon as a member of its own group finishes, whether that finish is a
success, a failure past its retries, or the contract being held for a user's decision.

**This is a concurrency limit, never a spend limit.** It sits inside kGraf's existing overall
work-item and agent-count ceilings, and only ever tightens concurrency further for contracts
likely to collide; it never loosens either of the broader limits.

**A configured value of zero is treated as one, not refused outright**, since holding every
coupled contract back forever would be a silent stall, which kGraf's fail-loud design forbids.
A value of one is a legitimate, deliberate choice to serialize a coupling group entirely.

## Consequences

- The common case, a plan made mostly of independent contracts, runs at full concurrency,
  unaffected by this limit.
- Every time a contract is held back by this limit, the reason and the group it belongs to are
  recorded in the run's own history.

## Alternatives considered

- **Lower the overall work-item concurrency limit instead.** Rejected: it would slow every
  contract in a run, coupled or not, for a risk that is concentrated specifically in coupled
  work.
- **Refuse a configured value of zero at startup.** Rejected for the same value specifically:
  a silent, permanent stall for every coupled contract is worse than flooring the value to the
  smallest meaningful one.

## Amendment (2026-10-08): one trace record per cause

A cluster-cap deferral and a work-item or agent ceiling refusal share one trace reason and
are told apart by the detail's prefix. The orchestrator's once-per-wait records were keyed
by the reason alone, so a contract refused at the ceiling first and held at its cluster cap
later kept the ceiling record for the whole wait: one run held a contract about 800 s at its
cluster cap while its trace said only that three agents were live. A deferral is now
recorded once per contract, reason and cause, and a wait record is due when its kind or, for
a capacity wait, its cause changes. The cap, its default and its trace reason are unchanged.
