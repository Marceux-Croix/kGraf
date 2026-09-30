# Decision 0075: Vendor and model routing by coupling and measured capability, opt-in beside plain rotation

**Status:** Accepted
**Related:** decisions 0008, 0012, 0017, 0020, 0023, 0024, 0041, 0043

## Context

kGraf's default routing simply gives each new contract the next model provider in rotation.
Cross-vendor pairs of contracts conflict measurably more often than same-vendor pairs, which
rotation does nothing to avoid, and nothing in kGraf measured which configured model actually
performs well at which kind of task, so routing decisions had no basis in kGraf's own recorded
outcomes.

## Decision

**A second routing policy, "fitness," is added beside the existing rotation policy.**
Rotation stays the default and is unchanged.

**A model is routed by the kind of task it is given** (an interface, an implementation, a
test, a refactor, a script, or documentation), never by a vendor's general reputation alone.

**Capability is measured from kGraf's own recorded outcomes, never assumed from a vendor's
claim.** A prior estimate from an offline benchmark is used only until a route accumulates
enough of its own live, verified results for a given kind of task, at which point the live
record replaces the prior for that kind. The two are never blended into one figure.

**The objective is lowest expected cost per successfully completed task for that kind of
work, not lowest price.** A cheap model with a poor success rate is not necessarily a
bargain once failed attempts are counted.

**A route with no known price, whether local or hosted, is never treated as an error or
excluded.** A free local model is ranked at zero cost; an unpriced hosted route is ranked on
its measured success rate instead, since kGraf does not police what a user spends with their
own provider.

**Contracts whose work is coupled (they touch overlapping files, depend on each other, or
tend to change together) are kept on one vendor, even when the specific model chosen for each
differs within that vendor.** This follows directly from the measured, higher conflict rate
between different vendors' agents working on related code.

**Interface work has an absolute quality floor.** A route measured below a minimum
reliability for interface work is never given interface work, regardless of price, since a
bad interface is a correctness problem, not a cost trade-off. A route with no measurement yet
is not treated as below the floor; it remains eligible until it is actually measured.

**A retry escalates to a better-measured route, starting inside the same vendor first**,
only crossing vendors as a last resort, and any such crossing is recorded plainly as breaking
the original coupling decision.

**A small, deterministic share of dispatches always goes to an unmeasured route**, so a new or
untried model can accumulate enough of its own results to be judged fairly, rather than being
permanently ranked behind an established one it never gets a chance to compete against.

## Consequences

- Fitness routing is opt-in; every existing configuration and behavior under plain rotation is
  unaffected.
- Every routing decision is recorded with its reasoning: the policy in force, which coupling
  group a contract belongs to, the route chosen, and why.
- Even under plain rotation, every verified attempt's outcome is still recorded into the same
  measured history fitness routing later reads, so switching to fitness routing later does not
  start from nothing.
- This decision does not claim that fitness routing reduces overall spend; whether cheaper
  routing changes how much work is attempted overall is a separate, open measurement.

## Alternatives considered

- **Pin a whole coupled group of contracts to one specific model, not just one vendor.**
  Rejected: a coupled group can contain different kinds of work a single model is unevenly
  good at; picking the best model per kind within one vendor keeps the coupling benefit
  without discarding per-kind fitness.
- **Rank candidates by price alone.** Rejected: cost per successfully completed task, not raw
  price, is the actual objective; a cheap, unreliable model is often the more expensive choice
  in practice.
- **Make fitness routing the default immediately.** Rejected for now: it has not yet been
  measured in live use, and an existing rotation-based experiment depends on rotation's
  behavior staying unchanged.

**Addendum, 2026-09-29: the run itself never refuses an unpriced model.** Ranking already
treated an unpriced route this way, but a run still refused a model with no price entry when it
took in a plan, checked readiness and dispatched. Those refusals are gone: the ledger keeps
every call, and reports name the spend as unpriced, or as local, unpriced. The spend total
says `total unpriced` when no recorded model has a price, never zero dollars. The rotation
cursor advances only when a route is used, a route that lacks a contract's required tool is
skipped, and an infrastructure failure (a provider outage, a rate limit) is not counted against
a route's live satisfaction tally. A live attempt the ledger cannot price is recorded as
unpriced, never as a known zero, and a profile store written with such a zero is repaired on
load. A pick found transiently not ready also moves the cursor (and, under `Fitness`, is passed
over for that contract), so one unready route does not hold a contract while another is
healthy.
