# Decision 0043: Workers share one byte-identical prefix and read private slices after it

**Status:** Accepted
**Related:** decisions 0006, 0008, 0012, 0025, 0042

## Decision

Every worker's request is assembled in two parts, in this order:

1. A **shared prefix**, byte-identical across every worker on the same vendor, model, and
   protocol: the system prompt, the tool definitions, and a **context pool** of slices that
   are broadly useful across workers. It ends at a cache breakpoint where the protocol has
   one.
2. A **private remainder**: this worker's brief, its own graph slices, its transcript.
   Nothing here is shared and nothing here is expected to cache.

**Nothing worker-specific may appear before the breakpoint.** No worker id, no timestamp, no
brief, no per-worker ordering. A single byte of variation moves the prefix into a private
cache entry and removes the entire benefit.

Pool membership (which slices are broadly useful enough to include) is chosen per dispatch
and recorded, rather than fixed.

## Context

Provider prompt caches are keyed on an exact token prefix, not on a conversation. Two
unrelated requests that open with identical bytes hit the same cache entry.

Under a per-worker prefix, N workers pay N cache writes, each amortized over that worker's
few turns. Under one shared prefix, N workers pay one cache write between them and every
subsequent worker reads. Fan-out into many short-lived workers therefore is not the worst
shape for prompt caching once the prefix is understood to be shared by bytes rather than by
conversation.

A shared prefix must be identical for everyone; a graph slice is per-symbol and therefore
per-worker. The two cannot occupy the same region of the request. Broadly useful context
that changes rarely goes in the pool, before the breakpoint, and is paid for once.
Task-specific context goes after the breakpoint, is paid for per worker, and should be kept
as small as possible. A slice in the pool that only one worker needs is waste charged to
every worker; a widely needed slice left out of the pool is paid for repeatedly. A worker
that needs something outside its slice reads it from the graph directly, and that read lands
in the private remainder.

**Limits.** This does not reduce the number of tokens processed; it reduces the price of
those tokens, at whatever discount the provider offers on a cache read, and even that price
reduction is not guaranteed until measured. It does not survive fan-out across vendors: a
cache entry belongs to one vendor's endpoint and one model, so a fleet spread across several
vendors shares each prefix only within its group, and the saving scales with the largest
same-vendor group rather than the whole fleet. This must never override vendor-fitness
routing: a cheaper wrong model is not a bargain. It is bounded by the cache's expiry, on the
order of minutes unless a longer duration is available and honored, so workers share a
prefix only if dispatched inside that window. It also depends on a capability not every
provider has: some protocols expose explicit cache breakpoints, some cache automatically with
no breakpoint field, and at least one has no caching concept at all. Where breakpoints are
unavailable, the discipline still likely helps, because automatic caching also keys on prefix
identity, but this is unverified.

## Alternatives considered

**Per-worker prefixes.** Discards a real price reduction once the prefix is understood to be
shared by bytes.

**One long-lived worker instead of fan-out.** The honest alternative; whether sequential
specialization wins over fan-out is a separate, open question.

**A shared conversation that workers append to.** Concurrent workers writing one transcript
is shared mutable state with a high measured conflict rate, and it destroys the append-only
property a transcript needs, since editing an earlier message invalidates later reasoning
tied to it.

**Put every slice in the pool.** The pool is charged to every worker, so a slice one worker
needs becomes many copies of waste, and it inflates every worker's context.

**One shared prefix plus private slices.** Chosen.

## Consequences

The prefix's byte-identical property needs an automated check, not just a comment: two
workers dispatched to the same vendor and model must produce identical bytes up to the
breakpoint, and the check must fail if a worker id is interpolated into the system prompt.

Cache reads and writes are counted separately, so the effect is visible in usage records with
no new instrumentation: a fleet sharing a prefix shows one worker with a large cache write and
the rest with large cache reads. If it shows many large writes instead, the invariant is
broken.

Pool membership is recorded per dispatch, so a later measurement can attribute a change in
cost to a change in the pool.

A cheaper prefix invites a larger pool, and a larger pool is charged to every worker, so the
right measurement is cost per successful task, not cost per cached token.

**Reactivation trigger.** A finding that sequential specialization outperforms fan-out, which
would remove the fan-out this optimizes. Or a measured finding that grouping workers by
vendor for cache sharing degrades task success, in which case vendor fitness wins and the
saving is abandoned.
