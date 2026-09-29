# Decision 0068: Large graphs fold children into their parents under a visible-node budget

**Status:** Accepted
**Related:** decisions 0014, 0015, 0032, 0054, 0058, 0063

## Context

On a large codebase, drawing every node in the graph at once loses its value as a visual
map: with tens of thousands of nodes on screen, structure disappears into clutter. A code
graph reference kGraf drew inspiration from stays legible only because it never scales past a
much smaller codebase.

## Decision

**Nodes fold into their structural parent.** A code chunk sits under the file or symbol that
contains it; a file sits under its directory; a directory sits under its parent directory,
and an empty directory holding only one subdirectory is merged into it. Work items fold the
same way: a task folds into its section or phase, and a phase or contract folds into a larger
unit above it, until the user expands it.

**No node may have more than a fixed number of visible children** (64). A longer list of
children is split into labeled groups of a bounded size, so expanding any one node never draws
more than that many new nodes at once.

**At most a fixed number of nodes are drawn at all** (1,000), chosen to keep the view legible
and fast. When the graph would exceed that budget, the deepest and largest branches fold first,
and the previously drawn set is kept stable as the graph changes, so a small edit does not
suddenly redraw hundreds of unrelated nodes. A view that already exceeds the budget because
the user expanded something by hand is allowed to, and says so plainly.

**A folded node shows a count, and searching still reaches inside it.** Clicking a folded node
expands it; clicking an already-expanded node folds it back. A search result is always drawn,
even if its containing node is folded, and a folded parent shows a single running count of
things needing the user's attention (a failure, or something waiting on approval) rather than
one badge per hidden child.

**Only things that need the user count toward that badge.** An agent quietly working, or a
file simply being read, does not add to the count; only a failure or an item genuinely waiting
on the user does, and each counts once, on the item itself.

## Consequences

- A directory with fifty thousand files, or a plan with thousands of tasks, draws a bounded,
  navigable scene rather than an unusable wall of dots.
- The view's exact folded state depends on the history of edits made to it, not only on the
  graph's current shape; a fresh reload can draw a different (but equally valid) folded state
  than a long-running session of the same graph.
- Folding a node changes only what is drawn; nothing about the underlying graph, or a node's
  identity, changes.

## Alternatives considered

- **Fold whole levels of the hierarchy at once.** Rejected: a single new item at a level
  boundary would flip the visibility of everything else at that level.
- **A folded state with no memory of prior edits.** Rejected: a change right at the visible
  budget's edge could otherwise flip a large, unrelated part of the view on every small edit.
- **Count every kind of activity, not only what needs the user.** Rejected: routine, ongoing
  work would then dominate the badge and drown out the few things that actually need
  attention.
