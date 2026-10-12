# Decision 0032: Search is a primary navigation mode, modeled on Gmail, and its results are a graph selection

**Status:** Accepted.
**Amended by:** 0034 (result presentation)
**Related:** decisions 0003, 0007, 0010, 0013, 0014, 0026, 0027, 0028

---

## Context

### Why search is navigation, not a feature

kGraf targets on the order of 10,000 nodes at interactive framerate, and
conversation records grow permanently on top of the code.

At that scale nothing is findable by looking. Panning a galaxy view to
locate a function is not navigation. Search is how a user reaches anything
not already on screen, which makes it a primary affordance rather than a
convenience, in the way that Gmail's power users search rather than browse
folders.

### Why Gmail specifically

Four properties are worth taking, and one is worth rejecting.

**One box, operators inline.** A plain text field where `agent:`, `before:`,
and `status:` are typed rather than configured. No form, no advanced-search
panel, no mode switch.

**Results are the real objects.** Gmail returns emails rendered as emails,
not search-result rows. kGraf returns nodes rendered as nodes, lit on the
graph.

**Search across everything by default, narrowed by operators.** Not a scope
picker the user must set first.

**Incremental, with suggestions.** Results and completions appear while
typing.

**Rejected: labels as a parallel organizing system.** Gmail's labels are a
second structure layered over the mail. kGraf already has one structure, the
graph, and does not add a second one.

### Why results must be a graph selection

kGraf avoids any surface that duplicates what the graph and the main input
box already show. A search results page listing nodes is exactly that: a
second rendering of objects the graph already renders, which will diverge
from it.

So a search is a selection. Matching nodes light up and everything else
recedes, using the graph's existing highlight behavior, with the camera
moving to the matched set. The accompanying list is an index into that
selection, not a replacement rendering of it.

This is the same boundary drawn elsewhere in the design: an additional
control is permitted, an additional place to look at work state is not.

### Why two boxes rather than one

Merging search into the orchestrator input would be elegant and is rejected.
"Where is the config parser" is ambiguous between a question to an agent and
a query over an index, and the ambiguity runs in both directions: a user
intending a cheap lookup would spend a model call, and a user intending a
question would get a result list.

Two fields, visually distinct, with distinct behavior.

## Decision

A search field is always present, alongside the orchestrator input box and
distinct from it.

Operators are typed inline in plain text, not selected from a form.

Results are a selection on the graph, rendered with the existing highlight
behavior, accompanied by a ranked list. Search does not open a separate
view.

Everything indexed is searchable, including completed work, collapsed
children, superseded contracts, and old conversations.

## Operators

The corpus spans code, conversations, work nodes, contracts, and an ingested
document corpus. Operators name those dimensions:

| Operator | Matches |
|---|---|
| `agent:` | messages sent or received by an agent, and its actions |
| `symbol:` | a code element by name or qualified name |
| `file:` | path pattern |
| `task:` | work nodes by title or brief content |
| `contract:` | contract artifacts, including superseded ones |
| `touched:` | agents or tasks that wrote a given file or node |
| `status:` | derived state, such as blocked, done, or under-review |
| `vendor:` / `model:` | work done by a given provider or model |
| `before:` / `after:` | time bounds |

Bare terms search everything.

### The backends already exist

A reference code-indexing implementation provides full-text search with
camelCase splitting and structural boosting that favors functions and routes
over files and variables, a regex name-pattern mode, a semantic keyword
mode, a grep-first code search that folds matches into their containing
function and ranks by structural importance, and a query language subset for
structural queries.

The user should never choose among them. The field routes by query shape and
merges results, the way a single Gmail query spans headers, body, and
attachments.

## Consequences

Ranking blends three signals: the structural boost the index already
computes, recency, and the user's current focus, so a search while looking
at a subsystem prefers that neighborhood. None of these is sufficient alone.

Permanence makes search more valuable and ranking harder. A monotonically
growing conversation corpus is precisely what makes past decisions findable
and precisely what will make a naive relevance ranking noisier over time.
Recency weighting and time operators are the mitigation, and ranking quality
should be measured as the corpus grows rather than assumed stable.

Search reaches what the graph has collapsed. Completed children collapse
into their parent node, and superseded contracts remain in the index. Both
are still indexed, so search surfaces work the visual layer has folded away,
which is a second reason it is navigation rather than decoration.

Selecting a result uses existing machinery. The highlight, dim, and camera
behavior already defined for the graph, and node expansion behavior, are
what a result selection triggers. No new interaction model.

## Open questions

Saved or pinned queries are not decided. They are useful and they edge
toward the parallel organizing system this record rejects in Gmail's labels.
If adopted, they should be shortcuts that produce a graph selection, never a
stored view with its own rendering.

Whether search should span projects is unresolved. A reference
implementation supports multiple indexed projects and renders linked ones as
satellites, so the machinery exists; whether a query should cross that
boundary by default is a question for when multi-project work is real.

## Alternatives considered

None recorded separately beyond the rejected design choices named above
(merging search into the orchestrator input, and labels as a parallel
structure).
