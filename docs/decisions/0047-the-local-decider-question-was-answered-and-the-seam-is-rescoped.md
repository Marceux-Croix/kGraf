# Decision 0047: The local-decider question was already answered in the literature; the measurement is void as model evidence and the interface is re-scoped

**Status:** Accepted. Supersedes the acceptance premise of decision 0042 and the robustness
argument in decision 0045. Amends decision 0046.
**Related:** decisions 0020, 0021, 0022

This is the fourth record about one measurement. The first three each diagnosed it, and each
was wrong in a different way. This one does not add another theory: it establishes what the
measurement cannot support, and what was already known before it ran.

## Decision

**The earlier local-decider measurement is void as evidence about the models it tested**, for
four documented usage errors, described below. It stays on record and its data stays
committed, because the run is honestly recorded and its independent finding (that an adapter
must be verified against a real server) is sound.

**The measurement was also unnecessary.** Its underlying question was already answered by
published research this project had already reviewed and cited elsewhere.

**No weights ship, on the present evidence, for this kind of decision.** Not deferred;
removed from the plan until a task exists that has labeled outcomes and passes the
recognition-over-supplied-features test of decision 0046. None currently does.

**Cold-start collision avoidance is a structural rule, not a model.** A pre-dispatch check
runs on declared facts, not on a model's judgment.

**The acceptance premise of decision 0042 is superseded.** That record accepted building the
decider interface on the reasoning that a weak average accuracy concealing a trustworthy
confident subset would still be usable. The premise is not wrong in principle; it is
unreachable in practice here, because the achievable ceiling on this task is low and a much
simpler structural check already reaches it.

## Context

### The question was already answered, and the citation was already in this project's records

A published, large-scale study of merge-conflict prediction found that a single feature,
"number of files changed in both branches," accounts for 0.93 to 0.98 of feature importance
across every language studied; every other feature scores 0.00 to 0.02. A realistic
machine-learning approach beats a one-feature decision tree by only 0.00 to 0.11 F1, and ties
it in one language studied. That is described in the underlying literature as the most
decision-relevant result in over two decades of research on this problem.

As a pre-dispatch gate, the simple structural check ("do both branches edit the same
function") performs within a percentage point of the best machine-learned model on the
metrics that matter for this use: how often it needlessly blocks work, and how often it
misses a real conflict.

So the answer to "can a small model do this better than a structural check" was already on
file: no model of any size in this study family buys more than a rounding error over one
structural feature. The local-decider measurement spent a model download and five
configuration arms rediscovering that.

### Confirmed independently on this project's own data

A feature sweep over the project's own 150 test cases, with thresholds fitted on one half of
the cases and scored on the other, disjoint half:

| Feature | Held-out accuracy |
|---|---:|
| Two or more files shared | 0.611 |
| A shared file is a documentation file | 0.611 |
| Frontier model | 0.613 |
| Best local decider | 0.500 |
| Majority class | 0.500 |

The winning feature is the same one the published literature reports at 0.93 to 0.98
importance. Both features are available at decision time without any model, since workers
already declare their write scope before writing, and a file name is free to inspect.

Stated with its proper uncertainty: with only 90 held-out cases, the margin of error is
roughly plus or minus 0.05, so 0.611 against 0.500 is a modest signal, and eight candidate
features were tried with the best two reported, which inflates the apparent result. The claim
is not that 0.611 is precise. It is that a simple structural feature and a frontier model land
in the same narrow band just above chance, matching what the published literature predicts.

### The four documented usage errors

Each is drawn from the model publishers' own documentation, reviewed after the measurement
ran.

1. **A known failure mode on the exact checkpoint used.** The model's own documentation warns
   that its bare-probability answer format can get stuck following its option labels rather
   than the actual input, most strongly on the checkpoint used, and recommends checking for
   this and switching to a two-option choice format if it appears stuck. One arm answered
   "conflict" on 144 of 150 cases with high, narrow confidence, which matches the documented
   failure. The recommended workaround was never applied.
2. **Run without task-specific adaptation against documentation that disclaims exactly that
   use.** The model's own card states it is "a fast base to specialize, not a zero-shot
   decision engine," and its own zero-shot benchmark score is below its own majority-class
   baseline.
3. **Calibration measured on a model that documents itself as shipping uncalibrated.** The
   model's card states it ships over-confident, and that refitting a temperature per question
   type substantially improves calibration, with an explicit instruction to do this before
   trusting its probabilities. The measurement used the unrefitted probabilities, and
   calibration was the one property the earlier acceptance decision rested on.
4. **The second model family measured is not a decision model.** It is a general-purpose
   text-understanding encoder wrapped by a server to answer typed questions, and that
   wrapper's own documentation states it is less accurate than purpose-built models on
   reasoning-heavy tasks.

The earlier robustness argument, that a negative result about a whole model category is only
as good as its strongest member, does not survive this: neither family was used as its
documentation instructs, and one is not a decision model to begin with.

### What this does to the category-error diagnosis

An earlier correction reasoned that the underlying task is deliberative and the model
category is defined as non-deliberative. The task probably is deliberative in the sense
described, but the measurement cannot support even that specific claim, because the
instruments were misused. The selection rule from that correction (a decider question must be
recognition over supplied features) stands on its own merits; its causal claim about this
particular measurement does not.

## Alternatives considered

**Correct the usage errors and re-run the measurement.** This is the substantive choice.
Correcting all four errors is meaningful additional work for a question where the published
literature already puts the achievable ceiling over a one-feature check at 0.00 to 0.11 F1,
and the project's own data already puts a single feature comparison level with a frontier
model. No result available would be worth the cost.

**Fine-tune a small model on the project's own commit history.** Rejected for now, on two
grounds: usable labels exist only for the collision-detection task, which this line of
decisions has already ruled out as a source of a hard gate or even a weak prior; and the
literature's ceiling applies to a fine-tuned model as much as to a zero-shot one.

**Remove the decider interface entirely.** The wire client is now verified against real
servers, the calibration module is the only tool available for judging any probability output,
and other future typed decisions still need somewhere to be placed. What is removed is the
plan to ship weights by default, not the interface itself.

**Void the measurement, record the literature, re-scope the interface.** Chosen.

## Consequences

The pre-dispatch collision check is specified as a structural feature: shared declared write
scope, with file-name class as a secondary signal, computed from already-declared write
intentions. No model, no download, no calibration step, and per the literature no worse than
the best learned alternative on this task.

The decider interface ships with no default model. The installer's decision to ship no
manifest is now the permanent position rather than a pending step.

Any future proposal to use a decider model must clear three conditions, in order: a task with
labeled outcomes exists; it passes the recognition-over-supplied-features test; and a
published or measured result shows a model beating the best cheap structural feature on that
specific task. The earlier measurement cleared none of these and was run anyway.

The recurring finding across this whole line of decisions is a pattern of convenient
substitutes standing in for the right instrument: a mock stood in for a real protocol
partner, a convenient test set stood in for a suitable task, license and size stood in for
documented fitness, and an ad hoc search stood in for the project's own prior literature
review, which already contained the answer. That pattern is worth watching for directly.

**Reactivation trigger.** A decider task meeting all three conditions above. Or a published
result showing a learned model beating a file-overlap feature on merge-conflict prediction by
more than the 0.00 to 0.11 F1 margin the literature records, which would reopen the ceiling
question rather than this interface.
