# Decision 0042: A decider seam, CPU-first, aimed at a wire format, and accepted on calibration rather than accuracy

**Status:** Accepted.
**Superseded by:** 0047 (in part: the acceptance premise)
**Related:** decisions 0002, 0006, 0008, 0020, 0022, 0025, 0031, 0039, 0041

---

## Context

### Why a second seam rather than a wide one

A model provider returns prose and is billed per token. A decider returns a
typed value and, run locally, costs no money. The same argument decision
0006 made against a wide interface with capability flags applies with more
force here: folding these two together would require one of them to
misrepresent what it returns.

The project turns out to want a great many typed decisions. Whether a brief
fits one worker is a yes/no. Provider routing is a pick-one over a bounded
list, which the vendor catalog from decision 0041 now enumerates. Ranking
among passing speculative merges is a score. Task-similarity dispatch
preference is a yes/no. Every one of those is currently specified as a call
to a frontier model, and per-chunk orchestrator overhead is already a cost
that rises linearly as chunks get smaller.

### Why CPU-first is a hard constraint and not a preference

Implementations of this kind of small decision-making model span roughly
four orders of magnitude of hardware requirement. The most accurate open
implementation surveyed reaches close to hosted commercial accuracy, and
needs a recent-generation NVIDIA card, a container runtime, and its GPU
toolkit. Another needs an 80-gigabyte accelerator card and 50 gigabytes of
weights.

kGraf ships to consumer machines on three platforms with no server and no
daemon to install, and local-only operation is a product position. A
default decider requiring an NVIDIA GPU would exclude every Apple machine
kGraf targets and most laptops, and requiring a container runtime would
contradict the installation story directly.

So the floor is CPU. Small models in this class answer in tens of
milliseconds on CPU, far below anything kGraf needs for a scheduling
decision. Hardware acceleration through a platform's own graphics or machine
learning APIs is then a bonus a machine may or may not have, mattering
specifically because some acceleration paths cover non-NVIDIA hardware
too.

The accelerator in use is reported rather than assumed, because a silently
absent one is a tenfold slowdown that presents only as "this feels
sluggish." The degradation is fine; the silence is not.

### Why a wire format rather than a library

Several independent projects already serve a common decision-making wire
format, and they sit at opposite ends of the size-versus-accuracy tradeoff.
Targeting the format rather than one project's library means the default
can be a small in-process model while a user who owns a capable machine can
point at a heavier server instead, as a configuration change rather than a
port. It also means no single project's maintenance becomes kGraf's problem,
which matters when the whole category is new.

### Why calibration is the acceptance property

This is the part worth getting right, because it inverts the obvious
framing.

An earlier measurement found a frontier model reaching 0.613 accuracy
predicting whether two changes land in the same region of a shared file,
against a 0.500 balanced baseline and a 0.501 shuffled-label control. The
recorded conclusion was that this signal survives weakly and earns nothing
as a gate: at the natural base rate for this question, the same answers
project to 0.613 where simply predicting "no conflict" outright scores
0.825.

The reason it earns nothing is not that 0.613 is low. It is that a bare
label at 0.613 gives a caller nothing to act on differentially. If the same
model said 0.95 on a fifth of cases and was right nineteen times in twenty
on those, the mechanism becomes usable immediately: act on the confident
tail, defer the rest. A weak average concealing a trustworthy tail is a
useful mechanism; a weak average with decorative confidence is not.

Claims of calibrated probability should be distrusted until measured, since
this project has already found at least one reputation for an efficiency
claim to be marketing copy read back as a product claim, rather than a
tested effect. So calibration is measured directly: expected calibration
error, the largest single-bin gap, a Brier score so a model cannot win by
hedging every answer at 0.5, and the confident tail's accuracy reported
separately from the average.

### Typed does not mean correct

The headline claim for this model class is that it cannot hallucinate.
Precisely, it cannot return a value outside the declared answer space. A
value inside that space can still be confidently wrong, and the distinction
is the difference between well-formedness and correctness. A typed answer is
never treated as a verified one, the answer-space check runs on every reply
rather than being assumed, and no decider may serve as a safety gate:
collision control is exact runtime leasing on purpose, and a decider is a
preference signal on the other side of that line.

## Decision

kGraf gains a second model seam, separate from the provider trait: a decider
answers a typed question about supplied state and returns a value inside a
declared answer space plus a confidence. Three primitives only: a yes/no
probability, a bounded pick-one, and an ordinal score.

CPU is the floor and acceleration is opportunistic. Any backend kGraf ships
by default must answer on CPU alone on macOS, Windows, and Linux. A GPU is a
speedup that may be absent, the execution provider in use is reported rather
than assumed, and no default path may require a particular vendor's hardware
or a container runtime.

The integration target is a common decision-making wire format, not any one
project's library, so a user may swap a small local model for a heavyweight
server without kGraf changing.

A decision is an accounted call: the decider's call takes the same
ledger-minted token a provider call takes.

The acceptance property is calibration, not accuracy. A mechanism built on
this is accepted on the measured accuracy of its confident tail and its
expected calibration error, never on a headline average.

## Consequences

A dedicated crate holds the seam, the wire client, a deterministic control,
and the calibration scoring. No model weights ship, and its tests run with
no network and no checkpoint, so the gates stay green on a machine that has
never downloaded a model.

A follow-on measurement is planned: the same committed cases used for the
0.613 result, on the same balanced set, measured again against that figure.
It needs no credential, which is why it is worth running early.

A deterministic control ships with the mechanism, not after it. An earlier
finding killed a code-graph predictor by comparing it against a size-matched
random baseline, and a control written later by someone who already knows
the answer is not a control. The control used here is a hashed, reproducible
function rather than a seeded random generator, because a seeded generator
would still vary with iteration order across processes and platforms.

The decider's context window is a real constraint. Small models in this
class sit near 8,000 tokens, well under a frontier model's, so a graph slice
sized for a worker will not always fit a decider. Oversized state raises
rather than being truncated, because a truncated state answers a different
question than the one asked.

No efficiency claim attaches to this. Moving decisions off a frontier model
removes coordination-token spend, not production-token spend, and does not
touch duplicated work between agents. It may even raise total spend, because
decisions that cost nothing get made more often and more orchestration spawns
more workers. Whether it changes total cost at all is a future measurement,
and a negative result there is an accepted possible outcome, not a failure
of the experiment.

## Alternatives considered

**Use a frontier model for these decisions, as currently specified.** The
status quo. Rejected on cost and on latency: it prices the smallest
decisions out of existence and makes a scheduling preference a network round
trip.

**Bundle the most accurate open implementation.** Rejected on hardware. It
reaches near hosted commercial accuracy and requires an NVIDIA card plus a
container runtime, which contradicts the local-only, no-install product
position. Retained as a supported optional backend, reachable through the
same wire format, for users who have the machine.

**Bundle a small local model now and measure later.** Rejected on
sequencing. A model checkpoint is a large addition to an otherwise small
application, and its accuracy on this project's tasks is unmeasured. The
interface is nearly free and reversible; the model is a commitment. So the
seam lands first and a follow-on measurement decides whether the weights
follow.

**Fine-tune a small encoder on the project's own committed fixtures.**
Genuinely promising and deferred rather than rejected: hundreds of commits
with ground truth already exist in the repository. It is a larger piece of
work than measuring an existing model, and the measurement should come
first because it may show the task is simply hard.

**A second seam, CPU-first, wire-format targeted, calibration-gated.**
Chosen.

## Reactivation trigger

A follow-on measurement returning at or below the 0.500 balanced baseline,
which would say the task is not learnable from the available signal and
close the question rather than inviting a bigger model. Or a calibrated
confident tail above roughly 0.9, which would reopen whether this kind of
signal can gate outright, a question currently answered no.
