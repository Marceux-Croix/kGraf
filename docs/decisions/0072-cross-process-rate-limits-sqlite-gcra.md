# Decision 0072: Cross-process rate limits are a shared, disk-backed store, never a silent per-process fallback

**Status:** Accepted
**Related:** decision 0056

## Context

kGraf gives every provider route its own rate limiter, but that limiter lived only in one
process's memory. The desktop application can host several projects at once, and the
command-line tool or a scheduled run can be a separate process again. All of them can call the
same model provider with the same credential at the same time, and none of their in-memory
limiters knows about the others, so together they can exceed the provider's real rate limit
even though each process, on its own, stays within its configured rate.

## Decision

Every kGraf process on one machine shares one rate-limit counter per route and credential,
kept in one file on disk at the user's own per-user data directory. The algorithm is the same
admission curve kGraf's in-process limiter already used, adapted so its state can live on disk
instead of in memory. Two processes checking the same route at the same moment serialize
against each other through the file's own locking, rather than racing.

If the shared store cannot be opened, the run refuses to start rather than silently falling
back to a per-process limiter: a silent fallback would reintroduce exactly the problem this
decision exists to close.

## Consequences

- A route's credential is never stored in the shared file; only a short, one-way hash of it
  is, so the file carries no more exposure than kGraf's existing accounting already does.
- The store favors speed over absolute durability for this specific purpose: losing the very
  last write to a crash only makes the next few checks slightly more permissive, which is the
  safe direction for a rate limit to err in.
- The most recently configured limit for a route wins, not the stricter of any two configured
  limits, so a limit that was lowered can always be raised again later by reconfiguring.
- A network-mounted data directory is unsupported and fails clearly at startup rather than
  behaving unpredictably later.

## Alternatives considered

- **A server-based rate limiter (for example, one backed by a small caching server).**
  Rejected: it would require the user to keep a separate server process running alongside a
  local, single-user desktop application, which conflicts with kGraf's local-only design.
- **A hand-rolled file-locking scheme built from scratch.** Rejected: kGraf already depends on
  an embedded database for its accounting ledger, and already proves out the same
  concurrent-writer pattern this needed; a second, separate coordination mechanism would add
  surface for no new guarantee.
- **Open a fresh connection to the shared file on every single check.** Tried first, and
  measured to cost more than the whole time budget for one check by itself, even with no other
  process contending for the file at all. Replaced with one persistent connection per route.
- **Silently limit only within the current process if the shared store cannot be opened.**
  Rejected outright: it reintroduces the exact bug this decision closes, as an unnoticed error
  path instead of a normal one.
