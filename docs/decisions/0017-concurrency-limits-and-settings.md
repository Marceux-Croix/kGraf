# Decision 0017: Bounded autonomy, a user-facing concurrency cap, and a settings surface

**Status:** Accepted. Amends the scope of the no-second-surface rule from
decision 0003 to state the boundary explicitly: settings holds
configuration only.
**Related:** decisions 0003, 0008, 0014, 0016

## Context

Removing the item boundary (decision 0016) let the eligible frontier become
arbitrarily wide, so concurrency stopped being bounded by structure. Making
decomposition recursive (decision 0014) means agents can beget agents.
Together those are an explosion shape, and nothing in the design otherwise
stops it.

The reasons for a cap are both cognitive and economic: too many concurrent
items is overwhelming to watch and burns tokens; too few accomplishes
nothing useful. Both halves matter, and the tradeoff between them is not
one a default can settle permanently, which is why it is a user control
rather than a constant.

### Why pre-flight enforcement, specifically

A limit enforced only after the cost is incurred is not a limit. Some
surveyed reference systems set a very high default token cap per agent,
made enforcement advisory, and used a reaper that killed the worker only
after the spend had already happened, with a test that asserted only that
the configured value persisted. That failure mode is exactly what a
pre-flight check must avoid: the check itself, not the setting that
describes it, must be the thing under test.

### Why a settings surface does not violate the no-second-surface rule

Configuration is not work state, so a settings surface duplicates nothing
the graph already shows. The risk is drift: a settings panel that grows a
status readout, a run history, or an activity list has become the second
place to look that the no-second-surface rule exists to prevent.

## Decision

**Play means autonomous within predefined limits, not unbounded.** The
user does not approve each partition; the orchestrator schedules and the
user watches. But kGraf cannot spawn agents without a ceiling.

**A user-facing concurrency control sets how many work items may be in
flight at once.** It lives on a slider with a stated default, because the
right value is a judgment the user makes and changes.

**A hard ceiling on total live agents is enforced separately**, because
recursive decomposition means one item's worker can itself cause more
agents.

**Both limits are enforced pre-flight.** A spawn that would exceed a limit
is refused, never started and then killed.

**A settings surface exists**, scoped to configuration only. It never
displays work state.

## Consequences

**Two limits, not one.** Concurrent work items is the user-facing control.
Total live agents is a separate ceiling that recursive spawning cannot
breach. Naming them distinctly matters, because a user setting items to
four and seeing eleven agents would reasonably consider the control broken.

**The default starts in a small band, roughly three to five.** Multiple
surveyed agent systems default their concurrency to a small number in this
range, and documentation for at least one notes that a handful of focused
workers often outperforms more scattered ones, with diminishing returns
beyond a point. This is a starting default to be tuned by measurement, not
a settled number.

**Enforcement is tested, not the setting.** The acceptance test attempts to
exceed each limit and asserts refusal. A test that sets the slider and
reads it back does not count.

**A refused spawn is a visible state, not a silent drop.** Under decision
0014 the interface distinguishes blocked from colliding; a node waiting on
the concurrency cap is a third reason, and it must be surfaced rather than
the node simply sitting still.

**Settings is configuration only.** Model and provider preferences,
concurrency limits, credentials, and project paths belong there. Anything
describing what is happening right now belongs on the graph or in the
orchestrator's answer.

## Open

A spend cap is a separate control from a concurrency cap and is not
decided here. Concurrency bounds how much runs at once; it does not bound
what a single long-running job costs. Whether kGraf carries a budget limit,
and whether it warns or halts on approach, is unresolved. Every call is
already attributed under the accounting design, so the data to enforce one
exists; only the policy is missing.

## Alternatives considered

**Approve each partition before agents spawn.** Rejected. Decision 0015
made partitioning continuous, so approval would be constant, which is the
altitude loss the product exists to prevent.

**Unbounded autonomy on play.** Rejected on both the cognitive and economic
grounds above. It is also the failure mode in the 16-agent build case cited
in decision 0009, where many agents produced nothing useful until the work
was forcibly made disjoint.

**A fixed internal constant instead of a control.** Rejected. The right
value depends on the project, the models in use, and the user's tolerance,
and none of those are knowable in advance.

**A cap on concurrent items only.** Rejected as insufficient, because
recursive decomposition means item count and agent count are not the same
number.
