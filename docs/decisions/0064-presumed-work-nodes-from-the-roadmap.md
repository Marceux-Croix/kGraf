# Decision 0064: Presumed work nodes come from a project's own planning documents, and are adopted, never duplicated

**Status:** Accepted
**Related:** decisions 0007, 0013, 0014, 0015, 0019, 0039, 0055

## Context

A fresh project has no work nodes until a plan is approved and dispatched. Until then, the
graph shows nothing about what the project's own roadmap or task list says remains to be
done. A user opening an unfamiliar project has no way to see, at a glance, what is in flight
or still open, without reading the documents by hand.

## Decision

A presumed work node is an ordinary work node plus a provenance record. It stands for work a
project's own documents describe, before any of it is approved or built. The provenance
record keeps, per node: the source document, the section (a heading, a table row, a task),
how it was parsed, when it was first written, and what real work later adopted it. No status
is stored on the node itself.

The state shown is derived each time the graph is read, from two facts: what the document
claims (a status word, or a checked task box) and whether the files and symbols the entry
names exist in the codebase. A presumed node marked "done" whose named file is missing, or
marked "not started" when the code already exists, shows that disagreement on the node. This
is a claim read against the code, never a verification: it does not run tests.

`kgraf index` reads a project's planning documents (files named ROADMAP, TODO, PLAN,
MILESTONES, BACKLOG or TASKS, in Markdown, in the project root or its `docs/` folder) into
presumed nodes on every run by default. Opening a project in the desktop app does the same
read at its own startup. A deterministic parser reads status tables and task lists first; a
model reads a plan written as prose, since a parser cannot.

Re-reads are idempotent. A node's identity comes from the document's own structure (a table
row's key, a heading path, a task's text), so a document that has not changed writes nothing
new. An entry the document no longer lists is marked gone, not deleted, and returns under the
same identity if it reappears. When a real, approved plan is seeded, each of its items adopts
the presumed node it stands for: the node keeps its identity and its history, and the graph
never draws two nodes for the same underlying work.

## Consequences

- A project with no planning document gets a stated notice, not an error.
- A code path or symbol a document cites counts as missing only when it plausibly belongs to
  this project, so a document that mentions another project's files is not misread as
  unfinished work.
- Only a regular file up to 4 MiB is read, and a cited path is never followed through a link
  leaving the project root.
- The desktop application only reads an already-indexed project; it never creates or rebuilds
  one on its own.

## Alternatives considered

- **A stored status column on the work node.** Rejected: status is a document's claim, not
  the project's actual state, and storing it would let the two drift apart silently.
- **A model reading of every project on every run.** Rejected as the default: it costs a model
  call on every startup and its results are not stable across repeated reads. Kept only for a
  plan written as prose, where a parser has nothing to read.
- **Deleting an entry a document no longer lists.** Rejected: work does not vanish because a
  document was edited; it is marked gone and can return.
