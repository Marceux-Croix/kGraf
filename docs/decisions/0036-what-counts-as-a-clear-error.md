# Decision 0036: An error is clear when it checks against a recorded artifact

**Status:** Accepted. Generalizes decision 0035, which named omission as the
example of a clear error and left the class underspecified.
**Related:** decisions 0013, 0026, 0027, 0028, 0030, 0033, 0035

---

## Context

Decision 0035 used omission as its example: the orchestrator demonstrably
did not consult something relevant, checkable against its read set because
read sets are observed rather than declared.

Omission is one instance of a general test, and the list matters less than
the test. Enumerating error types will always be incomplete; the criterion
generalizes.

### The classes this covers, non-exhaustively

| Error | Checked against |
|---|---|
| Omission | the orchestrator's observed read set |
| Contradicts a decision record | the decision record series, immutable and indexed |
| Contradicts a standing rule | the project's standing rules document |
| Contradicts project conventions | the ingested project corpus |
| Contradicts the plan or an acceptance criterion | the derived plan model |
| Contradicts its own earlier statement | the conversation record, permanent |
| Acted on a stale version | change history, since dependency is subscribed and notified |
| Internally inconsistent contract | the signature against its own test |
| Arithmetic or factual error | recomputation |

Each is a comparison against something written down. None requires the
orchestrator to concede it reasoned poorly, which is the concession least
likely to be made reliably. It is shown a discrepancy, not asked for an
opinion.

### Why the criterion matters more than the list

Escalation frequency is a quality metric on the orchestrator. A vague notion
of "clear" makes that metric meaningless, because the boundary moves with
whoever is judging and under whatever pressure.

A stated test fixes the boundary: either a checkable artifact exists or it
does not.

### kGraf is unusually well positioned for this

Most systems have little to check against. This project has an immutable
decision series, explicit standing rules, permanent conversation records,
observed read sets, an ingested document corpus, and a derived plan model.

That is a large recorded surface, and it exists because of documentation
discipline rather than for this purpose. It is not the first time that
discipline has paid off at runtime: the same supersede rule used for
decision records was borrowed for contracts, read sets became evidence, and
now the recorded corpus becomes the substrate for mechanical self-review.

## Decision

An error is clear, and the orchestrator should overturn itself, when the
challenge can be checked against a recorded artifact rather than against
judgment.

Where no such artifact exists, the matter is judgment, and a re-raised
challenge escalates to the user.

## Consequences

Self-review is a lookup before it is a judgment. The orchestrator's first
move on a challenge is to check whether a recorded artifact settles it, not
to reconsider its reasoning.

Escalation becomes meaningful. What reaches the user is what genuinely
cannot be settled from the record, which is the only category worth their
attention.

The recorded surface should be searchable at review time, which the search
design already provides.

A challenge citing no artifact is not thereby wrong. It is judgment, which
is exactly what escalation exists for. The test separates routes, not
merits.

## Open questions

Whether the orchestrator should be required to state which artifact it
checked when upholding a challenge. Doing so makes the rejection auditable
and would strengthen the requirement that reasons be recorded, at the cost
of some verbosity in every rejection.

## Alternatives considered

None recorded separately; this record generalizes decision 0035's example
into a stated test rather than choosing among alternative designs.
