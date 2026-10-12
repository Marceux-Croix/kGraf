# Decision 0046: The local-decider negative was a category error, not a data gap

**Status:** Accepted. Amends the diagnosis in decision 0045. Superseded in part by decision
0047.
**Amended by:** 0047
**Related:** decisions 0042, 0044, 0045

This record exists because decision 0045 attributed its negative result to a mismatch
between the model's training data and the test cases, and that attribution is wrong. It
changes what the result licenses, and it demotes an option decision 0045 left open.

## Decision

The measurement in decision 0045 asked whether two changes, described only by their commit
summaries, would land in the same region of a shared file. Answering that requires inferring
what each change entails, inferring where in a file such a concern would live, and comparing
the two. **That is deliberation.**

The model category measured is defined as the non-deliberative half of a two-tier design: it
answers a typed question about supplied state in a single forward pass, without working
through a chain of intermediate reasoning steps. The category's own documentation says so.

So the measurement tested a model class against the one thing that class is defined not to
do. The correct diagnosis of the negative result is a category error in the experiment's
design, not a data-distribution problem in the model. Three consequences follow, and the
third is the reusable one.

**The result is narrower than a reader would take it.** It says nothing about whether this
model class is useful in general. It confirms a property the category already documents about
itself.

**Fine-tuning is demoted** from a deferred, promising option to probably futile for this use.
Decision 0045 reasoned that being out of the training distribution "is the gap a fine-tune
addresses." If the limitation is architectural rather than informational, fine-tuning a small
non-autoregressive model does not reach it.

**A selection rule, which is the durable output of this record:** a question may go to this
kind of decider only if it is recognition over features already present in the supplied
state. A question that requires a world model the state does not contain belongs to a
reasoning model, or to a mechanical check, and putting it to a non-deliberative decider
measures nothing.

## Context

Decision 0045 was written from the numbers, and the numbers are consistent with either
explanation: a data-distribution mismatch and a category error both produce chance-level
accuracy and poor calibration, and both survive the confounds that measurement controlled for.
Nothing in the results alone distinguishes them.

What distinguishes them is the category's own definition. The interface's own documentation
opens by contrasting a model that answers in prose, one step of reasoning at a time, with one
that answers a typed question in a single forward pass, and the first question put through
this interface was an inference problem dressed as a classification.

The frontier model's 0.613 accuracy on identical inputs is the confirming detail. It is not
evidence that a small model missed a shallow signal; it is evidence that most of the eleven
points over chance came from reasoning, since reasoning is the one capability the two model
types do not share.

The test cases were chosen because they were already available, already balanced, and already
scored against a frontier baseline. Every one of those is a convenience property; none is a
fitness property. The convenience of an available test set, not its suitability, drove the
experiment's design.

This does not soften decision 0045's operative conclusions. No decider supplies this kind of
decision, not even as a weak prior; a confident answer at well below even odds is actively
misleading. No weights ship. The interface stays available for future, better-fitted
questions, and this record does not predict that any of them will succeed either.

## Alternatives considered

**Leave decision 0045's diagnosis as written.** A future reader considering the fine-tune
option would act on a reason that does not hold.

**Reopen the earlier measurement's status.** The measurement itself is sound and its data is
correctly recorded; what was wrong is the interpretation, which this record corrects without
reopening the earlier record.

**Record the category error, demote the fine-tune option, and state the selection rule.**
Chosen.

## Consequences

The earlier result should be read as a negative about a question this model class does not
answer by construction, not as evidence about the class generally.

Any future decider question must pass the selection rule above before it is measured, not
after: a decision that is a lookup over already-declared features (for example, choosing among
several available providers by their documented properties) passes; a decision that requires
inferring intent from free text does not, unless the supplied state also carries the concrete
signals (such as file counts or a dependency list) that make it recognition rather than
inference.

The fine-tune option is not closed permanently, but reviving it requires arguing the target
question is recognition, not that the earlier model was merely out of distribution.

**Reactivation trigger.** A decider question that passes the selection rule and is then
measured, which would be the first fair test this model category has had in this project.
