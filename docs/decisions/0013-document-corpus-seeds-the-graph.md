# Decision 0013: The whole document corpus seeds the graph, and kGraf imposes no format

**Status:** Accepted
**Related:** decisions 0004, 0007, 0012

## Context

Requiring a specific plan or roadmap format would restrict kGraf to
projects it scaffolded itself. That is a far smaller product than one that
can be pointed at an existing repository.

### What the corpus supplies

A project's existing documentation spine maps onto what the orchestrator
needs:

| Document role | What it supplies |
|---|---|
| Purpose and refusals | the north star, and the boundaries to check scope against |
| Conventions and gates | coding conventions, and the build and validation commands, which are the verifier |
| Plan | the work queue, dependency structure, acceptance criteria |
| Architecture | planned components, which become seed nodes with no code yet |
| Decision records | binding constraints, and reactivation triggers to watch for |
| Standing rules | behavioral policy binding the orchestrator and workers |
| History | what has been done, and resume points |

The gates row matters beyond seeding: the integration verifier does not
need inventing; it is whatever the corpus declares as validation. This is
how the project avoids a model's opinion standing in for a test.

### Machinery that already exists

A markdown indexer that treats each heading as a section node, with the
span as the heading line and the body captured as a property, and
maintains the index incrementally, makes documents first-class graph
citizens alongside code, with change detection already applying to them.

## Decision

kGraf ingests a project's entire existing documentation corpus, not a
single designated roadmap file.

It imposes no format, no filename, and no arrangement. Interpreting
whatever the project has into a usable form is kGraf's job, not the user's.

Interpretation produces a normalized internal plan model, stored as durable
facts and seeded into the graph alongside code nodes. The orchestrator
shows the user its reading and takes correction before acting on it.

Where no usable corpus exists, the orchestrator falls back to producing one
with the user, as decision 0004 already requires.

## Consequences

**A new component exists: document ingestion and intent interpretation.**
It discovers documents without requiring names, reads them, and emits the
plan model. Its output feeds the plan and the verifier.

**Interpretation is a language-model pass over prose and will sometimes be
wrong.** The mitigation is structural, not hopeful: the orchestrator states
its reading and takes correction before acting. For an existing project
this becomes the natural first conversation, which is better than the
greenfield alternative because the user is correcting something concrete
rather than specifying from nothing.

**Re-interpretation must be incremental.** Re-reading the whole corpus on
every document change is expensive. Change detection scopes it to what
moved.

**The plan model is durable facts** under decision 0007. Status stays
derived.

**Constraints extracted from documents must bind, not decorate.** A
standing rule saying no kanban must make the orchestrator unable to produce
one. A refusal in the purpose document must actually constrain scope. If
the corpus is only a seed and the rules are advisory text, the ingestion is
theater.

**Gates are discovered, never invented.** If the corpus declares no
validation commands, the orchestrator says so and asks, rather than
constructing a plausible-looking test command.

**Prose acceptance criteria may not be machine-checkable.** The
orchestrator must be able to say that a criterion cannot be verified
automatically and name what it would need, rather than quietly treating an
unverifiable criterion as met.

**Two cold-start paths**, selected by whether a usable corpus was found:
interpret and confirm, or produce with the user.

**Reactivation trigger:** evidence that interpretation accuracy is too low
to be corrected conversationally, meaning users spend more effort fixing
the reading than writing a structured file would have cost. That would
argue for an optional structured format as a fast path, never as a
requirement.

## Alternatives considered

**Require a structured roadmap schema.** Rejected. It works only on
greenfield projects, it starts a format argument with every user, and it
pushes work onto the person the product exists to relieve.

**Parse conventional formats heuristically.** Rejected as brittle.
Conventional is a moving target, and a parser that silently mis-reads a
heading is exactly the kind of silent degradation this project's design
principles forbid.

**Language-model interpretation into a normalized plan model, confirmed by
the user.** Chosen.
