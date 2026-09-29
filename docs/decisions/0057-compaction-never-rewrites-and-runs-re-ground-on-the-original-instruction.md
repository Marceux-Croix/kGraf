# Decision 0057: Compaction never rewrites, and runs re-ground on the original instruction

**Status:** Accepted. Refines decision 0056 and replaces its planned compaction method.
Thresholds superseded by the addendum below.
**Related:** decisions 0043, 0055, 0056

This record exists so the compaction method and the drift-prevention guard are not
re-argued.

## Decision

**Compaction never rewrites.** A worker's private context is reduced by masking old tool
outputs and truncating or dropping content, never by having a model paraphrase it. Every pass
works from the original content; a context that has already been compacted once is never
compacted again from its reduced form. Structured artifacts (diffs, test results, file paths)
are re-read from the graph or from git on demand, not carried forward as prose. The original
task instruction is pinned verbatim for the life of the run. The mechanism ports two existing
open-source agents' history-processing designs: one that filters and masks history, and one
that specifically masks stale tool observations. An existing summarizing compaction mechanism,
which does rewrite, is not used as the source design; it stays only as a reference point.

**Runs re-ground on the original instruction.** On every resume from the run-state record
described in decision 0056, and at a fixed cadence during a run, the orchestrator re-reads the
user's original instruction verbatim, never a summary of it, and records whether the next
planned dispatch still serves it. The run-state record's own claims of progress are checked
against an independent verifier, not accepted as self-reported.

## Thresholds and work-block sizing

The guiding intent: only the orchestrator, which spans the whole run, should ever compact.
Workers are sized so compaction never becomes necessary for them.

**The effective window** of a model is the lesser of its full context window and an absolute
token ceiling. Quality decays with the absolute number of tokens used, not with the fraction
of the window used, so a bare percentage-based threshold would let a very large-window model
run deep into decay before triggering. The ceiling is set per model from measured runs. All
percentages below are of this effective window, not the raw window size.

- Worker task blocks are planned to finish comfortably under the worker's stop threshold, using
  a conservative estimate of turns and tokens per turn until enough history exists to measure
  it directly.
- A worker never compacts. A worker that reaches its stop threshold has been sized wrong: it
  stops at the next clean point, returns a structured handoff describing what is done, what
  remains, and which files are affected, and the orchestrator splits the remainder into new
  blocks. Each such miss is recorded, and the miss rate feeds a measured optimum for how large
  a work block should be.
- The orchestrator compacts once it reaches its own trigger point, reducing back down to a
  lower target, using the non-rewriting method above. Its durable state lives in the run-state
  record, so compaction never drops anything the run actually depends on.

**Why these particular thresholds are engineering judgment, stated so they can be tuned, not
derived from a specific study:** the trigger is set well below the hard limit because a single
turn in flight needs headroom (one large tool result can add a meaningful fraction of the
window, and a trigger too close to the limit risks overflowing mid-turn); the post-compaction
target is set well below the trigger, rather than just under it, so that many turns of growth
happen before the next compaction, avoiding a thrash of very frequent compactions; and the
worker planning target leaves a wide margin under the worker's stop threshold specifically to
absorb ordinary estimation error.

## Context

Two open questions could have changed this design: how much information is lost from repeated
compaction, and how much a long run drifts from the user's original intent. A review of the
published literature answered both in direction, though not in exact magnitude for this
project's own workloads.

Observation masking (dropping or hiding stale tool output rather than rewriting it) matches
LLM-based summarization in task performance at roughly half the raw processing cost on a
standard coding-agent benchmark (arXiv:2508.21433). Compaction that never rephrases and never
compacts an already-compacted context keeps context faithful to the source (arXiv:2609.26779).

Model drift from an original instruction is inherited from prior steps in a run more than it
is caused directly by pressure to change course (arXiv:2603.03258). A resumed run is exactly an
inherited trajectory, so resuming is the right moment to re-ground. Verifier-backed state
tracking substantially lifts long-horizon task success in its published settings
(arXiv:2605.23574). Repeating the original instruction verbatim measurably reduces instruction
drift (arXiv:2402.10962).

## Alternatives considered

- **LLM-based summarization**, as used by an existing coding agent's own compaction step.
  Costs more, loses fidelity on each cycle, and a summary of a summary compounds error.
  Rejected as the method.
- **An independent monitor model watching for intent drift.** Proposed in the literature, not
  measured directly here. Not built; the cheaper re-grounding check above is used instead.
- **A learned, trained compaction policy.** Needs its own training, and no baseline
  confirming an advantage over observation masking was found. Not taken.

## Consequences

The compaction method changes to the masking-based design described above.

The orchestrator gains the re-grounding step at every resume and on a fixed cadence, plus
independent verifier checks on the run-state record's claimed progress.

The exact magnitude of information loss per compaction cycle, and the single correct ceiling
for current frontier models, stay open questions, measured from real use rather than assumed.

Reactivated if real-use records show masking losing information that a summarization approach
would have kept.

## Addendum: the compaction point is a user-facing setting

A settings control sets the compaction trigger point directly. Its default is the recommended
value for the active model: the point measured from that model's own recorded runs where task
outcome holds up, once enough runs exist. Until a model has enough recorded runs, the
recommendation is a stated default value, labeled as not yet measured for that model. The
control marks the current recommendation; while the user has not moved it, it follows the
recommendation as measurements update, and once moved it stays where the user put it, with a
control to return to the recommendation. The post-compaction target and the worker planning
target both follow the same control proportionally. The control is bounded to a fixed range; a
value outside that range, including one set directly in a configuration file, is treated as an
error rather than silently clamped.

Compaction runs two ways: automatically when the orchestrator reaches the set point, and on an
explicit user command issued from the orchestrator's own control surface. A command may also
target a specific selected worker, as an explicit, user-initiated override of the rule that
workers otherwise never compact on their own. Both paths use the same non-rewriting method and
are recorded with what triggered them.

Any test of this behavior moves the control and asserts that automatic compaction fires at the
new point, and issues the manual command and asserts that compaction actually ran, rather than
merely asserting that a setting value was saved.

## Addendum 2: evidence-based thresholds

The first set of thresholds was engineering judgment. They are replaced by the values below,
each tied to a specific piece of external evidence. Where the evidence gives a range, the
choice made within that range is stated.

| Setting | Value | Evidence |
|---|---|---|
| Effective window | the lesser of the model's window and 100,000 tokens by default; set per model from measured runs | Quality decays with absolute token count: one benchmark finds effective usable lengths of 64,000 tokens or less for 2024-era frontier models against 128,000 to 200,000 claimed context windows; another finds 11 of 13 models fall to half their short-context score by 32,000 tokens. A widely used context-editing default trigger elsewhere in the industry is 100,000 input tokens. Newer models decay less on some tasks, so the ceiling is set per model rather than fixed. |
| Orchestrator auto-compact point (default) | 80% of the effective window | Production defaults read from two existing open-source agent harnesses: one uses a fixed 0.8 threshold; another uses 90% of window against a 95% usable window. 80% is the more conservative of the two. |
| Compact down to | half the trigger point (40% at the default) | Matches an existing open-source agent's own condensing target of half its trigger size. |
| Worker context hygiene | mask old tool outputs continuously, keeping only the most recent few unmasked (published designs use a window of 5 to 10) | Observation masking matches LLM-based summarization at half the raw processing cost on a standard coding-agent benchmark (arXiv:2508.21433). This is not compaction; it is what keeps workers from needing it in the first place. |
| Worker plan target | under half the trigger point (40% of the effective window, 40,000 tokens at the default ceiling) | A published bounded-context compaction method keeps full-context task performance on a standard coding-agent benchmark with bounded contexts of 16,000 to 32,000 tokens for two different models (arXiv:2609.26779), so a single-issue coding task comfortably fits inside 40,000 tokens with faithful reduction. |
| Worker stop point | at the trigger; the worker hands its remainder back | Follows directly from the rule that workers never compact on their own. |
| Control range | 5% to 95% | Matches the user-adjustable range of an existing open-source coding agent and the usable-input cap of another. |

**Not yet supported by evidence, and therefore measured rather than assumed:** a single
correct ceiling for current frontier models, and the exact information loss per compaction
cycle. The pinned floor of context that can never be dropped (system prompt, tool
definitions, shared context slices, the original instruction) must sit below the
post-compaction target, or compaction cannot free any room; that constraint is a matter of
arithmetic, not a tuned value.
