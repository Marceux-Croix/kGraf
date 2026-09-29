# Decision 0045: The local decider loses on this task; the seam stays and no weights ship

**Status:** Accepted. Amended by decisions 0046 and 0047.
**Related:** decisions 0022, 0023, 0030, 0031, 0033, 0042, 0044

## Decision

A local, small typed-decision model ("decider") was measured across three checkpoints from
two model families, across five configuration arms, on 150 balanced test cases asking
whether two code changes would land in the same region of a shared file. Every arm lands at
or below the 0.500 majority-class baseline, against a frontier model's 0.613 on identical
inputs, and none produces a confident answer worth acting on. Four things follow.

**The interface for a decider stays.** Its trait, its integration with the accounting ledger,
its answer-space verification, and its calibration scoring are all cheap, reversible, and now
exercised against a real server.

**No weights ship** with the product, and the installer keeps shipping no default model
manifest.

**No decider may supply this class of decision**, not as a hard gate (already forbidden on
other grounds) and not as a weak prior either. A prior whose confident answers are actively
wrong is worse than no prior, because a caller acting on it does worse than a caller ignoring
it.

**The scope of the negative result is narrower than it first appears.** A frontier model
reached 0.613 on these identical cases, so something is extractable from the inputs. What
closes is this model class on this task, not the question generally.

## Context

### What the numbers say

The purpose-built member of this model category is trained specifically so that honest
probabilities are the only way to maximize its reward, which is exactly the property that
made this seam worth building. It does no better than the alternatives measured.

Two confounds were controlled with dedicated arms. Quantization: an int8 path against an
fp32 path for one family agreed to within 0.007 accuracy. Export fidelity: one model run
through two different runtimes agreed on 149 of 150 cases exactly, with the residual
difference explained by the wire format's own rounding. Neither runtime choice explains the
result.

| Arm | Accuracy | Calibration error | Confident answers | Says "conflict" |
|---|---:|---:|---|---:|
| Model A, English, run 1 | 0.500 | 0.371 | 130 at 0.477 | 144 of 150 |
| Model A, English, run 2 | 0.500 | 0.371 | 130 at 0.477 | 144 of 150 |
| Model A, alternate mode | 0.500 | 0.140 | none | 128 of 150 |
| Model B, run 1 | 0.487 | 0.198 | 42 at 0.357 | 30 of 150 |
| Model B, run 2 | 0.480 | 0.202 | 37 at 0.297 | 35 of 150 |
| Random control | 0.480 | 0.268 | 67 at 0.507 | - |
| Always "conflict" | 0.500 | 0.500 | 150 at 0.500 | 150 of 150 |
| Frontier model | 0.613 | - | - | 75 of 150 |

The two model families fail in opposite directions and land in the same place. One almost
never answers "conflict" (30 of 150 on a balanced set), with a confident tail below chance.
The other almost always answers "conflict" (144 of 150), which is how it matches the
always-"conflict" baseline exactly, and it is the most overconfident arm measured.

The best-calibrated arm is the one that refuses to answer with confidence: every one of its
150 answers sits in a narrow middle band, so its confident tail is empty. That is the correct
behavior for a model that knows nothing useful about the task, and a mechanism with no
confident tail cannot gate or inform anything regardless of its average accuracy.

Ranking ability (how well an arm separates the two classes, independent of any threshold) was
below chance on all five arms measured, consistent with no separation.

### Why this needed a real measurement rather than an assumption

The alternative reading was to treat a poor result as a tuning problem: the servers expose a
temperature, an answer-rendering mode, a state format, and other settings, and some
combination would eventually clear 0.500 on 150 cases. That is the exact failure this project
tries to avoid: sweeping a setting until it passes a test measures the sweep, not the model.
Two confounds were controlled because each had a specific, testable explanation, and a third
checkpoint was added because it is a genuinely different model. No setting was swept.

### What stays open

Fine-tuning a small model on the project's own commit history is not ruled out here, though
it is a large undertaking, and the frontier model's own 0.613 bounds what could plausibly be
expected from any model on this exact task.

This measurement covered one decision, the hardest one considered. Other candidate decisions
(routing among providers, judging whether a task fits one worker) are different questions on
different available inputs and are not settled by this result. Each needs its own measurement
before a model is trusted with it, which is the reason a cheap, reversible seam exists in the
first place.

## Alternatives considered

**Delete the interface entirely.** Discards a working wire client, a calibration module, and
an accounting integration on the strength of one question's result, when other candidate
decisions still need somewhere to be placed.

**Ship the weights anyway, for other future questions.** Commits a large download for a
component whose only measured task it fails.

**Sweep the server's settings until a configuration clears 0.500.** Rejected on method, as
above.

**Try a larger implementation of the same model family.** Rejected for now; the larger
versions need dedicated accelerator hardware the product does not require, and a result
gated on hardware the product cannot require does not tell the product what to ship by
default. It remains reachable through the same interface for a user who has that hardware.

**Measure only the first model checkpoint found.** Rejected during the run itself, which is
why three checkpoints across two families were measured rather than one; a negative result
about a whole model category is only as strong as its best member.

**Record the negative result, keep the interface, ship no weights, narrow the claim.**
Chosen.

## Consequences

This is the third negative result this project has recorded for a prediction mechanism on
this kind of decision. The pattern is established enough to state plainly: every prediction
mechanism measured so far for collision detection has failed outright or survived only
weakly, and the ones that survive are used as soft priors, never as gates.

Exact runtime file-lease control (rather than prediction) is reinforced as the correct
mechanism for preventing write collisions between workers, for the fourth time.

Sequential-versus-parallel worker scheduling is unaffected by this result; it never depended
on a decider.

Whether a small model should judge whether a task fits inside one worker's context needs its
own measurement; until then that judgment is made by a larger, more capable model, and any
cost concern about that stands unresolved.

The installation-size estimate for shipping these weights is corrected: earlier figures were
estimates. Measured: one family's checkpoint is 2.30 GB, its compact encoder 1.41 GB; the
other family's exported graph carries 1.69 GB of external data.

The optional-confidence design in decision 0044 earned its value immediately: one server
publishes no confidence figure at all, the other publishes two, and the same client reads
both correctly because a synthesized default was never introduced.
