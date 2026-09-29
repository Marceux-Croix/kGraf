# Decision 0074: Speculative merge reports a conflict early and never delays a merge; the scheduler prefers less-related contracts alongside each other

**Status:** Accepted
**Related:** decisions 0009, 0014, 0018, 0022, 0073

## Context

A merge conflict that git itself catches is one class of problem two concurrent contracts can
cause; a second, harder class is a pair of changes that merge together cleanly but break the
project's own tests or build once combined, even though each change passes on its own. Nothing
in kGraf caught that second class before either side's work actually landed.

## Decision

**While two or more contracts are in flight, the orchestrator periodically trial-merges their
in-progress work**, on a private, throwaway copy of the repository, never touching the real
integration branch. It checks each pair together: if two attempts merge cleanly alone but not
together, that is a textual conflict; if they merge cleanly together but fail the project's
own validation while each passes alone, that is a semantic conflict.

**The result is only ever reported; it never delays or cancels anything.** A trial merge is
computed from a snapshot of work still being written, so it can be stale by the time either
side actually finishes; acting on a stale guess (holding a merge, or pre-emptively starting a
resolver) would mean acting on a guess about the eventual result. The finding is recorded and
kept in the run's history; the actual merge, when either side lands for real, proceeds exactly
as it otherwise would.

**The scheduler prefers to run less-related contracts alongside each other where it can.**
Among contracts free to be dispatched, one whose task overlaps least with what is already
running is dispatched first, as a preference, never as something that withholds a contract
from running.

## Consequences

- Each trial-merge round costs one build-and-test run per changed, cleanly-mergeable pair, and
  is skipped, with a note, when no capacity is free for it.
- A trial merge is refused, rather than run, if it would touch a path that could steer git's
  own behavior in ways outside the project's own repository configuration, since a
  worker-written value there could otherwise select unexpected git behavior on the host
  machine.

## Alternatives considered

- **Hold the second merge of a found pair until it is resolved.** Rejected: it freezes merging
  on a snapshot that might already be stale, and a genuine textual conflict is already handled
  by 0073's resolver at the point it actually lands.
- **Re-validate every merge against the current tip before it lands, as a merge queue.**
  Rejected for now: this would serialize every merge behind one validation run, which conflicts
  with kGraf's preference for heterogeneous, concurrent work over serialized checking.
