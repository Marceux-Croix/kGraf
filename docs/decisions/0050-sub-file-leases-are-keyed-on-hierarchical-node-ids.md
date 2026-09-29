# Decision 0050: A sub-file lease is keyed on a hierarchical node id; a vanished id invalidates its lease loudly

**Status:** Accepted. Follows decision 0049.
**Related:** decisions 0010, 0015, 0048, 0049

This record exists so the key used for sub-file leasing is not chosen again from intuition,
and so the registry's line-range columns are not later promoted into the key that decides
exclusion.

## Decision

1. **The sub-file lease key, when sub-file leasing is built, is the hierarchical,
   signature-derived node id described in decision 0049.** The key is not a line range and not
   a positional ordinal.
2. **The registry's stored range-start and range-end columns stay measurement-only.** They
   exist to compute the needless-wait fraction from decision 0049. They must never become the
   key that decides whether two writes collide.
3. **A lease whose node id no longer exists after a write is invalid, and the registry says
   so.** The holder re-acquires against the current state. The registry never carries a lease
   forward onto a "nearest" surviving element; doing so would be a silent substitution.
4. **An element with unstable identity is leased at whole-file granularity**, as decision 0049
   already requires.

## Context

An identity-survival measurement ran the product's own code-parsing logic over a large sample
of real commit history from an external project, covering nearly 200,000 unchanged code
elements across the sampled commits.

| Scheme | Unchanged elements correctly kept | Real rebinds | Lost |
|---|---:|---:|---:|
| Hierarchical | 99.96% | 0 | 74 |
| Positional | 99.97% | 12 | 49 |
| Line range | 45.8% | 1,977 | 104,437 |

A rebound identifier is the only failure type that matters here, because nothing can detect
it after the fact: a lease on a rebound identifier protects code nobody asked for, while the
code that was actually asked for goes unprotected, with every check reporting success. A lost
identifier, by contrast, is detectable by definition, because it is simply absent.

Line-range identifiers rebind roughly one percent of unchanged elements, which disqualifies
them as a key outright. Positional ordinals retain marginally more elements but still rebind.
Hierarchical identifiers never rebound in this sample; they lost 74 identifiers, most of them
when an enclosing structure's own signature changed.

## Alternatives considered

**Line range.** Rejected on measurement: about 1 percent rebinding, and essentially no
retention for any element that moved position at all.

**Positional ordinal.** Rejected on measurement: 12 rebinds, every one a same-named sibling
renumbered by an insertion elsewhere in the file.

**Hierarchical id with a parent-independent fallback match**, which would look for a single
element with the same name and signature under any parent when an id vanishes, and carry the
lease over to it. Rejected for now. It would recover some of the losses caused by an
enclosing structure's signature changing, but it is exactly the kind of "nearest match"
substitution this decision forbids, and in the one measured case where an element was copied
and then edited, it would have picked the wrong element.

**Tracked identity across renames.** Deferred, as in decision 0049.

**Hierarchical id, with a vanished id treated as an invalid lease.** Chosen.

## Consequences

The expected cost of this safety choice is small: roughly 0.04 percent of unchanged elements
lost their identifier in a single commit in the sample. That share of leases would be refused
and re-acquired rather than silently misapplied. A refusal is visible and costs one extra
acquisition; a silent rebind would cost an undetected collision.

The whole-file fallback is appropriately sized: in the external sample, about 1.71 percent of
elements have unstable identity and would fall back to a whole-file lease; in this project's
own code, the share is zero.

**Reactivation.** Any measured real rebind under the hierarchical scheme reopens this
decision. So does a loss rate high enough that the resulting re-acquisitions show up
meaningfully in the needless-wait accounting from decision 0049.
