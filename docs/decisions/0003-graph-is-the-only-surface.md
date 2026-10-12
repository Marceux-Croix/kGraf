# Decision 0003: The graph is the only work surface; no kanban

**Status:** Accepted
**Amended by:** 0017 (the scope of the no-second-surface rule: settings holds configuration only)
**Related:** none

## Context

Kanban solves a real problem: coordinating task flow across a team of humans
who each need to see what is theirs and what is blocked. kGraf does not have
that problem. The user is not a member of the team doing the tasks, and the
agents do not need a visual queue to know what to work on next. The
orchestrator sequences work autonomously from the plan.

The user's job in kGraf is directing architecture and intent. The
kanban-shaped work happens underneath that, autonomously. Putting a board on
screen would invite the user down from directing architecture into shuffling
cards, which is the one thing the product exists to make unnecessary.

## Decision

The interface is the graph plus a permanently present orchestrator input
box. There is no kanban board: no columns, no cards, no backlog view, no
drag-to-prioritize.

Any proposed panel must first answer why its information cannot live on the
graph or be obtained by asking the orchestrator.

## Consequences

Every state a board would have shown must find an expression on the graph,
or be answerable by asking the orchestrator. This is a real design
constraint on the interface, not a free simplification, and some states will
be genuinely harder to show this way.

Sequencing and prioritization are the orchestrator's job. The user
influences them by talking to the orchestrator and by changing the plan, not
by dragging anything.

Requests for a board, a task list, a sprint view, or an activity feed are
refused by default. Overturning this requires a superseding record that
argues the case, not a feature ticket.

**Reactivation trigger:** evidence from real use that a specific work state
is both important and genuinely unrepresentable on the graph. That argues
for representing that one state, not for a board.

## Alternatives considered

**A board alongside the graph.** The conventional answer, and users know how
to read one. Rejected: it is a second surface that must be watched, will
drift out of sync with the graph, and lowers the user's altitude. It also
duplicates information the graph already carries, since work in flight is
visible as agents attached to nodes.

**A board instead of the graph.** Rejected outright. The graph is the
product.

**Graph only, with task state expressed as node and edge state.** Chosen.
Work in flight appears as agents attached to the nodes they are acting on.
Planned work appears as nodes seeded from the plan that have not filled in
yet. Blocked work is a property of a node, shown on the node.
