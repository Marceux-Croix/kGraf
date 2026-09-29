# Decision 0021: Cold start is a weak prior, optimistic dispatch, and a concurrency ramp

**Status:** Accepted
**Related:** decisions 0017, 0018, 0019, 0020

## Context

Decision 0020 made co-change the collision relation. A fresh repository has
none, which is the one case with no signal at all.

A repository with existing history is fine even when kGraf was not present
for it, because git records what changed regardless of authorship. The
genuine cold start is commit zero.

### Why optimistic dispatch is affordable here

It would not be, on its own. It is affordable because gates already exist
for independent reasons. Decision 0018 made quality gates first-class work
nodes, and decision 0019 attached one to every major node and put passing
tests inside the definition of done.

That bounds the cost of being wrong. A collision that slips through is
caught at the next gate rather than propagating silently, and the blast
radius is the work since the last passing gate.

Being wrong is also productive. Under decision 0020 an observed collision
is a labeled training example, which is the signal git history structurally
cannot provide. Early collisions in a fresh project are how the model
acquires the data it lacks.

### Why directory proximity rather than the call graph

The call graph has been measured as near-useless for collision prediction,
with a size-matched random file set performing within 0.062 contamination
at every operating point (decision 0020). Directory proximity is crude, but
it is at least a history-adjacent signal: files are grouped by path because
people group work that way.

It is explicitly a weak prior to be replaced, not a mechanism. Its job is
to be better than nothing for the first few tasks.

## Decision

On a project with no usable co-change history, three mechanisms compose
into one policy. They are not alternatives.

**Directory proximity is the day-zero prior.** With no history, files
sharing a path prefix are assumed more likely to collide.

**Dispatch is optimistic, and gates absorb the damage.** kGraf parallelizes
within its cap, observes the collisions it causes, and does not repeat
them.

**The concurrency cap ramps with evidence.** It starts low and rises as the
co-change relation fills in.

## Consequences

**The concurrency cap acquires a second meaning.** Under decision 0017 it
was a user preference about how much to watch and spend. It is now also a
confidence signal. Those can conflict, and the user's setting wins: the
ramp proposes, it does not override. A user who sets the cap to eight on a
fresh project gets eight.

**Collisions must be recorded as first-class events**, not merely handled.
Under decision 0020 they are the training signal, so a collision that is
resolved and forgotten is data thrown away.

**The prior must be visibly temporary.** A user should be able to see that
kGraf is operating on a guess rather than on evidence, because the honest
answer to "why did you run these together" differs at commit ten and
commit ten thousand. The difference should be surfaced rather than
smoothed over.

**Gates now carry a load they were not originally justified by.** They
were adopted for quality under decisions 0018 and 0019. They are now also
the safety net for cold-start parallelism. That is a genuine dependency:
weakening gate cadence would weaken cold-start safety, which is not obvious
from either record alone.

## Open

**The ramp schedule is unspecified.** What evidence justifies raising the
cap, and by how much, is an open question. It should be derived from the
measured relationship between co-change coverage and collision rate rather
than chosen.

**Whether the directory prior helps at all is untested.** It is a reasoned
guess that it beats nothing. It has not been measured, and it should be,
before it is relied on.

## Alternatives considered

**Start fully serialized until the relation fills in.** Safe and honest,
and rejected as the sole policy: it forgoes all parallelism precisely when
a project is smallest and collisions would be cheapest to absorb. Retained
as the low end of the ramp.

**Start optimistic with no cap and no prior.** Rejected. Decision 0017
exists to prevent unbounded spawning, and a fresh project is where the
model knows least.

**Use the call graph as the cold-start prior.** Rejected on the measured
contamination figures above.

**All three, composed.** Chosen. The prior supplies day zero, optimistic
dispatch supplies the operating mode, and the ramp supplies the confidence
schedule.
