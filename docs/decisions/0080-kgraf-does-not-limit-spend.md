# Decision 0080: kGraf does not limit spend

**Status:** Accepted; supersedes the spend-cap parts of decision 0017 (its concurrency
limits, the work-item cap and the live-agent ceiling, stand unchanged)
**Related:** kGraf's every-token-attributed standing rule

## Context

An earlier build gave kGraf per-task, per-run, and per-hour spending caps, and a separate
per-worker spend ceiling, so a run could be automatically paused or ended once it crossed a
configured dollar limit.

## Decision

**kGraf never refuses a dispatch, a reservation, or a round of work for want of budget.**
Every spend cap is removed. A user who wants a hard stop on spend sets it directly in their
own model provider's own console; kGraf is not that console.

**Accounting stays complete and unchanged.** Every token and every dollar spent is still
attributed and recorded exactly as before; every cost figure the application shows still comes
from the same real, settled numbers it always did. What changes is only that nothing compares
those numbers to a limit and refuses on the result.

**An old configuration file that still sets a spend cap loads without error**, with a one-time
notice that the setting is now ignored, rather than failing to load at all.

**Every other kind of limit stays untouched.** How many pieces of work or agents may run at
once, how many rounds a worker may take, and the size of a single request are all concurrency
or scope limits, not spend limits, and none of them changed.

## Consequences

- A run that would previously have hit a spend cap now simply runs to completion, with every
  call still fully recorded in the ledger.
- Removing a cap is not a claim that overall spend goes down; kGraf makes no savings claim
  anywhere in its own design, and this decision does not create one.
- Old scripts or saved run configurations that used the previous spend-limiting flags will need
  those flags removed; the underlying settings are no longer read.

## Alternatives considered

- **Keep the caps, but raise their default values very high.** Rejected: a limit a user cannot
  see, verify, or reach without knowing an internal number is still a limit, just a hidden one,
  and it still needs a value that could surprise someone reusing an old, smaller one.
- **Remove the enforcement, but keep the underlying reservation-and-refusal machinery in
  place, always returning success.** Rejected as more code than it is worth: a refusal type
  that can never actually be produced is dead weight a future reader would have to prove is
  dead.
