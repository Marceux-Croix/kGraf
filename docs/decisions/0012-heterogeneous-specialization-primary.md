# Decision 0012: Heterogeneous specialization is the primary mode; verifier-selected redundancy is secondary

**Status:** Accepted. Supersedes the verifier-selection clause of decision
0009. The disjoint-write requirement of 0009 stands unchanged and becomes
more load-bearing, not less.
**Related:** decisions 0004, 0008, 0009

## Context

The intended goal is multiple vendors working different parts of the same
problem at the same time, cooperating, rather than N independent attempts
at one problem with a verifier picking the winner. That distinction changes
which evidence applies, and it changes it for the worse.

### What the correction costs

It sets aside the strongest evidence in this design's favor for this
purpose. The repeated-sampling result, going from 15.9 percent to 56
percent success on a coding benchmark through many independent attempts,
only holds where a verifier selects among redundant attempts on the same
unit. It says nothing about splitting a problem across specialists. That
argument is not available as a primary justification for division of labor.

It places the project in the mode with the worst measured numbers.
CooperBench measured precisely this arrangement: 30 frozen feature pairs,
two agents, two different features, one repository. Uncoordinated, pair
pass was 23.3 percent.

The cross-vendor multiplier is now the design, not an edge case. A
33,596-pull-request study found cross-agent pairs conflict at 41.7 percent
against 19.8 percent for same-agent pairs, with non-overlapping confidence
intervals. Many vendors on one repository is exactly the 41.7 percent
bucket by construction.

Anthropic's warning that coding is a poor multi-agent fit binds harder
here, because its stated reason is that most coding tasks involve fewer
truly parallelizable subtasks than research. The primary mode depends
entirely on clean decomposition, so that warning attacks the mechanism
directly rather than incidentally.

### What the correction is worth

The fix is known, measured, and cheap. Static pre-write admission control
took CooperBench's pair pass from 23.3 to 50.0 percent and integration
success from 65.6 to 96.7 percent, with conflict-labeled pairs going from
6.7 to 60.0 percent.

The coordination channel is the highest-leverage part of the system. In the
interference study cited in decision 0009, a coordination message of
roughly 130 tokens recovered 82 to 98 percent of the ground lost to blind
parallel work. The failure is coordination, not capability, and it is the
central design target here.

The shape has a strong precedent: an orchestrator-worker pattern, a lead
agent delegating to specialized subagents operating in parallel, measurably
outperformed a single agent on an internal research evaluation, though that
evaluation was on research work, not coding, and at a much higher token
multiple than a coding task would tolerate.

## Decision

**The primary mode is division of labor.** The orchestrator decomposes a
problem into parts and assigns each part to a worker chosen for vendor and
model fitness: one vendor for scripting, another for architecture, and so
on. Workers collaborate concurrently on different parts of one problem.

**Redundant attempts on the same unit, with a verifier selecting a
winner, is retained as a secondary mode**, available for a high-value or
high-uncertainty unit. It is not the default and it is not the
justification for the project.

**Pre-write scope declaration is mandatory for concurrent writers**, not an
optional safety feature. A worker declares the files and graph nodes it
intends to write before it writes them; the orchestrator admits or defers.

## Consequences

**Scope declaration is a requirement of the agent runtime and the
orchestrator, not a sandboxing nicety layered on later.**

**kGraf must measure and report its own serialization rate.** Admission
control in the CooperBench study serialized 96.7 percent of executions, and
the paper states the conservative policy largely serialized the workload
with no useful wall-clock gain. If kGraf's rate approaches that, its
parallelism is nominal and the honest report says so. The dynamic,
still-parallel arm of that study regressed to 22.2 percent, with 46 of 90
executions failing on undeclared scope, so a permissive policy is not the
escape.

**Decomposition quality becomes the product's core competence.** The
orchestrator's ability to cut a problem into genuinely independent parts is
now the thing kGraf lives or dies on.

**The verifier's job changes in the primary mode.** It validates
integration rather than selecting a winner. Integration success, not pair
pass, is the metric it moves.

**The worker pattern is the subagent pattern, deliberately.** Narrow brief,
compact structured return, orchestrator never paying for intermediates.

**Disjoint writes matter more, not less.** Decision 0009's core requirement
is unchanged and is now the only thing standing between the design and the
41.7 percent conflict rate.

**A useful baseline for measuring the design's own overhead is a user's own
instrumented single-agent usage**, subagent delegations included, rather
than a literature figure for a solo session.

**Reactivation trigger:** a measured serialization rate high enough that
concurrent specialization provides no wall-clock or quality benefit over
sequential work. That would argue for sequential specialization, which
keeps the vendor-fitness routing and drops the concurrency, and it would
supersede this record.

## Alternatives considered

**Keep verifier-selected redundancy as the primary mode.** Rejected: it is
not the product the user wants, and evidence convenience is not a reason to
build the wrong thing.

**Division of labor without admission control.** Rejected on the 23.3
percent pair-pass rate. This is the arrangement the evidence condemns most
directly.

**Division of labor with mandatory pre-write scope declaration, redundancy
retained as an opt-in secondary mode.** Chosen.
