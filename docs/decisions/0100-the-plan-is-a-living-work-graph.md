# Decision 0100: The plan is a living work graph, and a run is a working session

**Status:** Proposed
**Related:** decisions 0007, 0014, 0015, 0018, 0027, 0030, 0043, 0049, 0052, 0056, 0057, 0064,
0067, 0071, 0073, 0074, 0078, 0081, 0083, 0088, 0093, 0094, 0095, 0096, 0098

## Context

A run was a batch job: an instruction went in, the plan was frozen, a result came out and the
run ended. A changed instruction paid for full planning again. A note typed during a run did not
change the plan, and the run loop could not even read it while a planner call was running. The
user's branch never moved, and a new run started over from the checked-out branch, so verified
work from the last run was not its base.

kGraf's own recorded runs show the cost. A full plan took a median of 13 s on a hosted model and
110 s (90th percentile 244 s) on a local 35B model; in one benchmark task the planner took between
half and nearly all of the time window. From the last planner call to the first worker call took
a median of 14 s locally and 19 s hosted. A small planner call on a local server took a median of
4 s alone and 49 s beside running workers on the same server.

The goal is that working with the orchestrator moves at the speed of thought within the limits of
whatever models and machine the user has. kGraf cannot make a model faster; it can keep its own
overhead small, make the fewest and smallest model calls an interaction needs, and never put a
model call in front of work that does not need it.

## Decision

### 0. Shape

The run becomes the **working session**. It stays open after its plan is done; a change reopens
it; every change builds on the verified work on its branch `kgraf/<run>`. A change re-plans only
the difference, in one small call. No model call runs on the run loop: user input lands outside
the loop and is acknowledged at once, and every planner call runs as a task whose answer the loop
applies. A small request skips planning. An earlier instruction can be opened, edited and run as a
change. The user can talk to the worker holding a work node directly. The user's branch moves only
when the user lands the session, and kGraf suggests landing when it is wise. Every run's contracts
are work nodes in the graph. Every interaction reports where its time went.

No separate session object is added: the run record already holds the plan, the branch, ledger
attribution and resume. "Session" here is the working session; decision 0067's agent sessions
keep their name.

### 1. Speed is budgeted as kGraf's own overhead

No route class is assumed: a user may run only hosted models, only local ones, or both. Each
interaction has two budgets:

- **kGraf overhead**, absolute (rules, record writes, dispatch, worktrees, git, parsing): a target
  at the 50th and 90th percentile, tested against an instant stub model.
- **Model work**: the fewest and smallest calls the interaction needs. Its time is whatever the
  route takes; kGraf predicts it from the route's measured speed (latency, output rate, time to
  first token, rate-limit waits, 429 retries; with first-run defaults per route class) and chooses
  strategies from it.

Every interaction writes one trace event that splits its time into kGraf, model and the project's
own checks, and `/status` shows the newest split per kind.

| # | Interaction | kGraf overhead p50 / p90 | Model work allowed |
|---|---|---|---|
| B1 | A box or terminal line is acknowledged | 50 ms / 150 ms | none |
| B2 | A small request decided by rule reaches its worker's first request | 1.5 s / 3 s | none before the worker |
| B3 | A small request the rule cannot decide reaches its worker | as B2 with an early start | one triage call, reasoning off, 64 output tokens |
| B4 | A change reaches the applied plan | 0.5 s / 1 s | one delta call, reasoning off, output capped by the size of the change |
| B4a | The first added contract of a streamed delta is dispatched | 1.5 s / 3 s after it parses | none extra |
| B5 | A queued contract is rewritten | 100 ms / 300 ms after its change parses | none |
| B6 | A running worker sees a redirect | 0.5 s / 1 s by interrupt, else its next round | none |
| B7 | The galaxy shows a plan change | 0.8 s / 1.5 s | none |
| B8 | Land (one merge commit) | 1 s / 3 s | none |
| B9 | Reopen a run with a change | 1 s / 2 s | as B4 |
| B10 | A small request's contract is verified and merged on `kgraf/<run>` | 3 s / 6 s | the worker's rounds |
| B11 | Open an earlier instruction for editing | 100 ms / 300 ms | then as B4 |
| B12 | The first independent contract of a streaming plan is dispatched | 1.5 s / 3 s after it parses | the plan call already running |
| B13 | A land suggestion is raised after its trigger | 0.8 s / 1.5 s | none |
| B14 | A direct message to a work node is acknowledged | 50 ms / 150 ms | none |
| B15 | That message reaches the worker | 0.5 s / 1 s by interrupt, else its next round | none |

**Strategies chosen from measured speed:**

- **Interactive calls first.** Triage, delta and answer calls take a one-request reserve in the
  shared cross-process rate-limit store on every route; worker calls may not take it. After a 429,
  an interactive call makes one attempt bounded by the retry-after and fails loud instead of
  inheriting worker backoff.
- **Local priority**, only when a local server is configured, shared with the planner, has two or
  more slots, and measures overlapped calls more than twice as slow as lone ones: new worker rounds
  wait during an interactive call, and on five or more slots one slot is kept free within 10
  minutes of user input. Both count against a cap of 20% of worker slot-seconds; at exactly five
  slots the reserved slot alone uses the whole cap.
- **Early start** and **streaming** run only where the predicted model time exceeds kGraf's
  dispatch overhead.

**Where the design still blocks, and why:** the one model call an interaction needs; re-grounding
when it runs on its cadence or after a note; a consumer waiting for its interface check; a plan
change the permission mode asks about; an uncertain box line's confirm; landing's conflicts and the
root gate on a merge result; permission asks; the time limit; local priority (blocks workers, never
the user); an early-started contract's merge waiting for its answer; `Completed` waiting for the
root gate.

### 2. The plan lives in the graph

The run record stays the live plan; every run's contracts are mirrored as work nodes. A new field
`plan_root` names the mirror plan `run:<id>`; `source_plan` keeps its meaning (an approved corpus
plan), since it disables the held-plan re-plan and keeps held interface checks. Revisions,
verified commits, removals and adoptions are temporal facts with a change-log trigger, so the
galaxy hears them. No status is stored; states (verified, landed, in progress, held, remaining,
stale, removed) are derived.

### 3. Re-plan only the change

One `replan_delta` call, reasoning off, reads (in cache order) the system prompt, the project
context, the plan (open and closed contracts with their whole specs), each contract's state, the
original instruction with every applied change verbatim, and the new change. It answers with
contracts to add, change or remove, or `no_change`, or `replan_all` (a full plan, only on the
user's confirmation).

Applied mechanically:

1. Added ids are renamed per change.
2. kGraf, not the model, decides amend or supersede: an amendment only appends text to the
   instruction and acceptance; any change to writes, dependencies or route is a supersession.
3. A change to verified work becomes a follow-up that depends on it, and every live dependent of
   the original also depends on the follow-up. Removing verified work is refused.
4. The merged plan passes the existing plan and repository checks; one re-ask on refusal; a second
   refusal leaves the plan unchanged and records the change as not applied.
5. At application, each operation is re-checked against the run as it is now; a target that
   changed state or was split causes one re-ask quoting the conflict. A delta applies whole or not
   at all.
6. Interface asks run only for added or changed edges whose producer writes code the consumer
   uses; added contracts are not pre-split by a model call; straight chains go through chain
   collapse; affected gates reset; verified dependents of a superseded contract re-run their checks.
7. Streaming: an added contract with no unparsed dependency and no overlapping writes starts as
   soon as it parses; it may verify but merges only after the whole delta applies.

A closed contract from an older record has no recorded dependencies; its staleness shows as
unknown, with a notice.

### 4. Changes apply to a live run

- **Input never waits for the loop.** The app and the terminal write each line to an append-only
  intake table and acknowledge from it; the loop folds rows into the record and records the last
  folded row in the same transaction. Planner tasks append each answer to the same table before
  the loop applies it, so a kill after an answer arrived never repeats the paid call.
- **Changes are not notes**, so they never trigger re-grounding.
- **Routing**: questions go to the answer route; requests that open with "can you", "please" and
  the like are changes; `/note` is information; anything else is a change. The input channel
  decides first (the box is for the plan, a node's composer is for that node). A line is uncertain
  when it carries both question and change cues, when it names the selected node or arrives within
  5 seconds of selecting it, or when it is three words or fewer with no verb while a card is open;
  an uncertain line is acknowledged and then confirmed with one click. Unattended input takes the
  first reading and says so.
- **Asking follows the permission mode.** A delta is additive, reshaping (changes unstarted work)
  or cutting (throws work away):

  | Mode | Additive | Reshaping | Cutting |
  |---|---|---|---|
  | `Manual` | asks | asks | asks |
  | `AcceptEdits` | applies | asks | asks |
  | `Auto` | applies | applies | asks |
  | `OrchestratorDecides` | applies | applies | applies, except it refuses dropping verified work with no replacement |
  | `Bypass` | applies | applies | applies |

  Plan review (0094) still governs the first plan. Unattended, an asking delta stays pending. A
  `--change` or `--edit` file is the user's yes for additive and reshaping deltas; a run that ends
  with any change pending exits nonzero.
- **Application by state**: a queued contract is rewritten; a running one gets a redirect at its
  next boundary for an amendment, or is stopped and dispatched again from the branch tip with its
  attempt digest and new leases for a supersession; a removed one is cancelled. A call cut on
  purpose, planner calls included, settles as cancelled with its partial usage.

### 5. One branch per session, landed on demand

- The session's branch is `kgraf/<run>`; the starting branch is recorded as the default target.
- A change to a completed run reopens it without a resume re-grounding.
- **Landing** (`/land`, the plan root node's action, `kgraf land`): through the loop, or under the
  run's lease when no loop is live. By default the newest commit whose root gate passed lands; no
  commit holding an interface stub whose producer is unverified ever lands. Every land is one merge
  commit as the user, even when a fast-forward is possible, built with `git merge-tree` and
  `git commit-tree` (no `pre-commit` or `commit-msg` hook runs; `post-merge` runs when the checkout
  moves; signing is tested). The checkout only fast-forwards to the merge commit, so the user is
  never left mid-merge; git refuses over a dirty file it would overwrite. The merge result is gated.
  `kgraf/<run>` moves to the merge commit only when its tip is the landed commit; otherwise the
  merge is merged into it, so no verified commit is dropped and the branch never moves backward.
  A commit with no passed gate lands only on the user's yes after a loud warning, with a
  `kGraf: UNTESTED` line and a `kGraf-Gate: untested` trailer; never unattended. Never a rebase,
  force or push.
- **Land suggestions.** kGraf offers to land, and never lands without a yes, when a commit with a
  passed gate is unlanded and one of these holds: every contract is verified (a stopping point);
  the user starts new work on files the unlanded commits changed, which would grow the unlanded
  diff; the target branch moved; or the session is closing. At most one suggestion is open; after
  "Not now", none until new verified work and 15 minutes; none during an interactive call.

### 6. Small requests skip planning

A rule with no model call sorts a request into small, large, or unclear. A small request becomes
one contract: the request verbatim, the named files' package as write scope, and the run's check
narrowed to that package; the whole suite runs as the root gate in the background on verified tips
(information only while its covers are open; a running gate run always finishes, then one run on
the newest tip). A narrowed check that tested nothing leaves the contract unverified (decision
0093). An unclear request makes one triage call; with **early start**, its worker starts beside
the call when the scope is clear, and merges only after the answer. First-plan **streaming**
starts the first independent contract of a fresh plan as soon as it parses, with rules for
overlaps, chain collapse, sizing and interfaces. Both are settings, flags and run-file keys.

### 7. Copy and edit an earlier instruction

`/edit` opens a run's effective instruction; the edit becomes one delta against that run's plan.
`/edit --new` copies the plan into a new session and applies the edit as one delta.

### 8. Talk to a work node directly

The user selects a node and writes to the worker holding it (or `/to <contract> <text>`); no
planner call is on the path. A running worker gets the text at its next boundary or at once; a
queued node's instruction is amended; a verified node gets a follow-up. A change is big if any of
six rules fires (paths outside the scope, another contract's files, an interface, a new dependency,
a route, the acceptance) or the worker reports it with `report_shortfall` (`direct_change`); the
rules on the work watch the whole attempt. A small change is recorded as an amendment of the
contract; a big one is reconciled by one background delta while the worker continues within its
scope. A conflicting change is refused loudly, and kGraf stops the attempt and starts a new one
without the direct text.

### 9. The plan stays in the prompt cache

Planner calls send a byte-identical prefix (system prompt, project context, plan); on the
Anthropic protocol two cache breakpoints mark it; the OpenAI Responses adapter sends
`prompt_cache_key`.

## Options considered

- **A separate session object.** Rejected: it would duplicate the plan, branch, attribution and
  resume the run already holds.
- **Keep batch runs and plan faster.** Rejected: every change would still pay for the whole plan.
- **Plan edits as agent tool calls.** Rejected for now: several rounds per change.
- **The plan as a document the user edits.** Rejected: a second place to look.
- **Fixed-second budgets.** Rejected: model and machine speed vary; kGraf budgets its own overhead.
- **Pause every worker while a change is planned.** Rejected: it blocks the user's other work.
- **Let the model label amend or supersede.** Rejected: the label must not follow what is cheaper.
- **Land automatically.** Rejected: the user's branch moves only on the user's yes.
- **Fast-forward landing.** Rejected: one merge commit per land keeps every landing visible.

## Consequences

- No new unledgered call: triage and delta calls are ledgered; cut calls settle as cancelled.
- Changes per session, tokens per change, early-start waste, cut rounds, priority pauses, confirm
  rates and triage misjudgments are reported, since cheaper changes may invite more of them.
- Decision 0027 gains a mechanical amend-or-supersede test for work contracts; 0057's original
  instruction becomes the original instruction and every change, verbatim; 0094's plan-review
  "Change" becomes a delta; 0064's adoption extends to every run; 0067's direct messages become
  changes on the contract; 0093's `report_shortfall` gains `direct_change`.
- Reopen if deltas are refused on more than a quarter of changes, if triage misjudges more than one
  request in ten, if early starts waste more worker time than they save, or if a kGraf overhead
  target misses its 90th percentile repeatedly.
