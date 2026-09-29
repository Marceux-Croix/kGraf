# Decision 0063: One live activity card is permitted, and every row routes back to the graph

**Status:** Accepted. Amends decision 0034 and the no-second-place-to-look rule.
**Related:** decisions 0004, 0007, 0015, 0017, 0028, 0032, 0034

This record exists so the live activity card is not cut on an overly strict reading of the
no-second-place-to-look rule, and so it does not grow into the kind of dashboard that rule
exists to prevent.

## Decision

1. **One live activity card is permitted**, shown beside the graph, below the one text
   surface. It lists the run's events in time order, newest first while the run is live.

2. **Its rows are change-log events and nothing else.** Each row is one event from the
   underlying event stream: a time, the agent or provider involved, and one line of text. The
   card stores no state of its own and derives no summary. It has no counts, no status table,
   no progress summary, and no grouping by status.

3. **Every row routes back to the graph.** Selecting a row selects the node the event touched
   and uses the graph's existing highlight and camera behavior. If a time scrubber exists,
   selecting a row also moves it to that event's time. The card is an index into the graph's
   history, never a second rendering of the graph itself.

4. **The card is not a work surface.** It has no actions: no approve, retry, reorder, or
   dismiss. Tool approvals and questions to the user stay in the orchestrator's own control
   surface. It has no filters by status. Rows are shown in time order only.

5. **One card only.** A second activity card, a separate run-history page, or an activity panel
   inside settings would still be the violation this rule exists to prevent.

6. **The no-second-place-to-look rule is amended** to permit this specific card. The rest of
   the one-text-surface rule stands: there is still exactly one text surface.

## Context

A mockup styled after a video-sharing site's interface showed live events as captions
overlaid on the graph that disappeared a few seconds later. That was rejected in favor of a
persistent sidebar card placed under the text surface.

### What the graph cannot draw

The no-second-place-to-look rule asks, of any proposed panel, why the information cannot live
on the graph itself. The graph draws state at one instant: what exists, what is done, what is
blocked, who holds which node. It shows a change as a brief animation in place, and that
animation is gone once it finishes playing. Two things follow, and the graph structurally
cannot show either one:

- **The order in which things happened.** Sequence is a property of time, and the graph has no
  time axis. A user who wants to know that one event came before another cannot read that from
  node positions and colors alone.
- **An event the user did not see.** A brief animation that played while the user was looking
  elsewhere leaves nothing behind. A glance at the card shows what the graph changed recently,
  even if it was missed live.

This is an existing design principle applied to time: the graph renders structure, the text
surface renders content, and this card renders order. None of the three duplicates what
another already draws.

### Why the stated conditions

The conditions keep the card on the permitted side of the line drawn by the settings-surface
rule and the one-text-surface rule. The card duplicates nothing the graph shows as long as its
rows stay events. It would become the second place to look the moment it starts summarizing
work state: counts, status filters, and grouping by status are all summaries. Actions would
make it a second work surface, which the no-kanban, no-board rule already forbids elsewhere.
Routing every row back to the graph is the same condition already placed on search and on
code-snippet display elsewhere in the design.

## Alternatives considered

- **Keep the original prohibition, and show events only as transient animation on the graph.**
  Rejected: a missed event cannot be recovered, and the order of events is unreadable from the
  graph alone.
- **A third feed inside the one text surface.** Rejected: showing activity there would
  displace the transcript the user is actively reading, and switching between two feeds in one
  surface is an extra mode the user has to manage. A separate card was requested instead.
- **Ask the orchestrator directly what just happened.** Always available, but each answer
  costs a full provider call, and "what just happened" is a question a user may ask often
  enough that a cheap, repeatable action taken frequently would not actually bound the total
  cost.
- **A run-history view with filters and search.** Rejected: that would be a destination, a
  second rendering of the work graph. Search already reaches old events, and its result is a
  selection on the graph itself.

## Consequences

The card reads the same change-log tail that already drives the live graph. It adds no new
event source and no stored state of its own. Its rows cover only events that already reach the
change log; an event that does not reach the change log cannot appear on the card either.

Every change-log row the card shows must name the node it touched, or the route back to the
graph fails; a row with no linkable node is shown without a link, and that gap is treated as a
defect in the event's own data, not as a defect in the card.

The list is virtualized and bounded, matching the text surface's own approach. In a
CPU-only rendering mode, rows appear without animation.

**Reactivation.** The card has become the dashboard this design otherwise forbids, and must be
cut back to this record, if it gains any of the following: a count or summary of work state; a
status filter; an action; or a second instance anywhere in the product.

## Open question

**A time scrubber.** A related request asked whether the run's progress indicator could become
a scrubber showing the graph as of any point in a run's history. That is not decided here. The
change log is timestamped and never pruned, work nodes carry a creation time, and lease
changes are logged, but derived status is not fully reconstructable from the log today: some
of the facts status derives from have no change-log trigger of their own, some event payloads
are partial, and reconstructing code content at a past time needs git directly. If a scrubber
is adopted later, the activity card would follow the scrubber's chosen time, and rows after
that time would be shown as not yet happened.
