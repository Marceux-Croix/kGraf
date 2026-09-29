# Decision 0028: Conversation files are never pruned

**Status:** Accepted. Closes the open question in decision 0026 on whether
conversation files are pruned.
**Related:** decisions 0004, 0010, 0026, 0027

---

## Context

Decision 0026 made agent communication durable plain-English markdown, keyed
to stable pairs, and left pruning open because permanence collides with
unbounded growth.

It resolves in favor of permanence for three reasons.

The interface promises an unabridged record: clicking an agent shows every
message it sent or received. A pruning policy makes that promise conditional
on age, which is a different and weaker promise.

Permanence is what makes it memory rather than a log. The most valuable
property of durable conversation, that past exchanges are first-class
retrievable context, depends on the past still being there. A pruned
conversation is exactly the one a returning agent needed.

It is audit-grade. A system that spends the user's money autonomously should
not be able to quietly remove the record of why. Nothing being removable is a
stronger guarantee than a retention policy, and it is simpler to verify.

### The growth is probably not the problem it sounds like

Prose in markdown is small and compresses well, and volume is already bounded
by a batched delivery policy that avoids streaming informational traffic. A
pair exchanging fifty messages of a few hundred words accumulates tens of
kilobytes.

This should still be measured rather than assumed. If growth does become a
problem, the answer is not pruning, since that is what this record
forecloses. It is splitting files, archival storage that remains addressable,
or a different representation. The record stays complete.

## Decision

Agent conversation files are permanent. Nothing is pruned, rotated,
truncated, or archived out of the working set. The record grows
monotonically.

## Consequences

Conversation nodes compete with code nodes for the graph's rendering budget.
A rendering cutover threshold (a point-sprite fallback for very large node
counts) was set without conversations in mind, and agents plus code cross it
sooner than code alone would.

The likely answer is that conversations render as a separate layer or
satellite rather than in the primary field, following precedent from
multi-project graph rendering support elsewhere in the design. This is a
concern for future visual design work that an earlier record did not
anticipate, and it is recorded here rather than discovered at render time.

Repository growth should be measured over time and reported, so that a
surprise is a finding rather than an incident.

A superseded contract's conversation survives it (decision 0027). The
exchange that established an inadequate contract is part of why the work
went the way it did, and is exactly the kind of context a later agent needs.

## Alternatives considered

None recorded separately; permanence is chosen directly against the
alternative of some retention or pruning policy, for the three reasons
stated above.
