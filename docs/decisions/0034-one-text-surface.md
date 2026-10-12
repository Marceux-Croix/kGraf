# Decision 0034: One text surface, fed by selection or search, routing back to the graph

**Status:** Accepted. Refines decision 0032's result presentation. Makes
explicit where decision 0004's unabridged transcript promise actually
renders.
**Amended by:** 0063 (one live activity card), 0067
**Related:** decisions 0003, 0004, 0010, 0014, 0026, 0028, 0032

---

## Context

### It resolves something left implicit elsewhere

Decision 0004 promises that clicking an agent shows every message it sent or
received, verbatim and unabridged. The graph cannot render a transcript as
node geometry, so a text surface was always implied by that promise. It was
never located.

A search snippet panel is the search-scoped version of exactly that surface.
Treating them as two panels would create a second place to look at the same
content; treating them as one surface with two feeds does not.

### Why a content panel does not duplicate the graph

kGraf avoids any surface duplicating what the graph and the main input box
already show.

The graph renders structure. The panel renders content. A status readout, an
activity feed, or a run history duplicates work state the graph already
draws, which is what the design avoids. Text at the character level is
something the graph cannot draw at all, so a surface showing it duplicates
nothing.

This is the same boundary drawn elsewhere for the settings surface and the
search field: an additional surface is permitted when it does something the
graph structurally cannot, and forbidden when it restates what the graph
already does.

### Why a snippet must route back rather than terminate

A snippet that can only be read, with no way back to the structure it came
from, turns the panel into a destination. Selecting one therefore selects
its node, which is what keeps the graph the primary surface.

Expanding in place rather than opening elsewhere matters for the same
reason. Conversations are permanent, so a snippet from a months-old exchange
has a great deal of surrounding context, and that context belongs in the
same panel rather than in a third view.

## Decision

There is one text surface, a panel beside the graph.

It is fed two ways. Selecting an agent fills it with that agent's verbatim
transcript. Searching fills it with matching snippets, each shown with
surrounding context.

Selecting a snippet does two things at once: it selects the corresponding
node on the graph, and it expands the snippet into its full surrounding
conversation in place.

Matching nodes light on the graph while the panel shows snippets. The panel
indexes the lit set; it does not replace it.

## Consequences

The panel has one rendering for conversation content, used by both feeds, so
a transcript read after clicking an agent and a transcript read after
following a snippet look and behave identically.

Search reaches retired agents. Agent identity is durable while the process
is ephemeral, and conversations are permanent, so a phrase from an exchange
between two agents that no longer run is findable, and selecting it must
resolve to something on the graph rather than to nothing. How a retired
agent is represented is a question for future visual design work.

Snippet ranking follows the same blend of structural boost, recency, and
current focus used for search generally. With a permanently growing corpus,
ranking quality degrades without recency weighting.

The panel is not a work surface. It shows content. Acting on what is found
happens on the graph or through the orchestrator.

## Open questions

Whether the panel is dismissible or persistent is unsettled. A permanent
panel costs screen area that the graph wants; a transient one makes
returning to a search result a repeat query.

## Alternatives considered

**A separate search results panel alongside a transcript panel.** Rejected.
Two text surfaces showing the same underlying material is duplication the
design otherwise avoids, and they would diverge in formatting and behavior.

**No snippets, only node highlighting.** Rejected. A phrase search that
lights up twelve nodes without showing why each matched is unusable, and the
reason a node matched is text the graph cannot display.

**Snippets inline on the graph, as labels or tooltips.** Rejected. Graph
labels are already capped for legibility at scale, and character-level text
does not survive the density rules at scale.

**Snippet opens a full-screen reader.** Rejected. It makes the panel a
destination rather than a route.
