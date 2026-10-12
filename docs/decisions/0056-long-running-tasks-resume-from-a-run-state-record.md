# Decision 0056: Long-running tasks resume from a run-state record, and the graph is the memory

**Status:** Accepted
**Amended by:** 0057 (replaces its planned compaction method)
**Related:** decisions 0015, 0026, 0028, 0030, 0036, 0043, 0052, 0055

This record exists so the long-running task design is not re-argued. If a contributor asks
why kGraf has no separate memory system, or how a multi-day run survives a restart, the
reasoning is here.

## Decision

Users can run tasks that last hours to days across many separate sessions. Four mechanisms
support this, each taken from existing published work where possible.

1. **A run-state record.** The orchestrator keeps a machine-readable record of each run: open
   contracts, admitted write leases, the last verified commit, and the next planned dispatch.
   On restart it reads this record before it re-plans. The record lives in the graph store
   itself, not in a second surface. This follows a published pattern for long-running agent
   harnesses that combines a progress record with a git checkpoint. Durable step replay is
   evaluated against an existing SQLite-backed workflow library before any custom engine is
   written.
2. **A capped private context, reset through compaction.** A worker's private context is
   capped and periodically reduced, because measured quality degrades well below the nominal
   context limit of most models.
3. **Stall detection.** The agent loop detects repeated identical tool calls inside its step
   budget. No existing permissively licensed library provides this, so it is built new, kept
   small.
4. **The graph is the memory.** kGraf does not add a separate vector-store or summary-based
   memory system. Agent conversations and decisions are durable graph nodes that are never
   pruned, and retrieval is structural rather than semantic search over a separate store.
   Where the graph itself lacks a notion of temporal validity (when a fact stopped being
   true), an existing open-source graph-memory design is ported for that purpose.

Two further items are planned as measured, rather than assumed, improvements: workflow
templates mined from successful task breakdowns, and measuring whether making long tasks
easier to run increases how many get run, since the population of long tasks is expected to
grow as models improve. Acceptance for the orchestrator work also adds a published checklist
of multi-agent failure modes.

## Context

Long-running tasks were identified as a first-class use of the product, on the reasoning that
pre-planning and coordination make long tasks feasible, and that they demand maximum
efficiency. Much of the existing design already covered this; a review of the published
literature confirmed those choices and exposed four gaps: restart state for the orchestrator
itself, degradation inside a worker's context over time, stall detection, and an unstated
position on memory.

## Alternatives considered

- **A separate memory system**, of the kind used by several existing agent memory products.
  Their reported gains are largely single-paper and self-reported results, and such a system
  would duplicate what the graph already holds. Their consolidation ideas are ported only
  where the graph genuinely lacks them.
- **A workflow engine with a server component.** Needs a standalone server process, which
  conflicts with a local, single-user app. One considered option is also under a
  source-available license that does not qualify for reuse.
- **Resume from a single worker's own transcript alone.** Covers one worker's own continuity,
  not the orchestrator's overall plan. Kept as the mechanism for an individual worker; the
  run-state record covers the orchestrator.

## Consequences

Early work gains the context cap and stall detection. Later orchestrator work gains the
run-state record, restart acceptance criteria, and the failure-mode checklist. Later
measurement work gains workflow templates and the induced-demand measurement described above.

No savings claim follows from any of this.

Reactivated if measurement shows that structural retrieval from the graph misses information
that a semantic memory system would find in long runs.


## Amendment: SIGINT and SIGTERM stop a run honestly

`kgraf orchestrate` used to end the process in the middle of a call on Ctrl-C or SIGTERM:
the provider call stayed `in_flight` in the ledger and worker worktrees were left with no
word to the user.

The first SIGINT or SIGTERM now sends the run the same stop the app's stop button sends: no
new dispatch, every worker cancelled, and the planner's own call in flight ended. A call
dropped that way settles as unreconciled (it may have billed); a worker's cancelled call
settles as cancelled. Neither stays `in_flight`. The command prints the usual report with
status `Stopped`, one `kept       worktree` line per worktree left, and exits with 128 plus the
signal number. `schedule run-now` shares the path and records the run as failed.

- **Bounded.** The stop is given 30 seconds. At the bound, or at a second interrupt, the
  process prints one `cut short` line and exits at once with the same code; startup cleanup
  handles what is left.
- **Listeners are installed first**, so an interrupt during setup is kept, and the
  dependency fetch's handler chains to ours instead of ending the process.
- **Windows** uses Ctrl-C; there is no SIGTERM.
- **Rejected.** Letting the default handler end the process (the defect). Exiting at once
  after the first signal and leaving settlement to startup cleanup: the user would not be
  told what was left.

## Amendment (2026-10-06): a run that did not complete names what it left

A stopped run printed `status Stopped` and its `verified` line. A contract whose worker the
stop cancelled, and the root gate that never ran, were named nowhere, so the report could
not be told from a stop with nothing left.

- **What is printed.** After the `held` lines, one `unfinished <id>: <state>` line per open
  contract that is not held and per gate that neither passed, was waived nor is held. A
  contract reads "not started" or "not started: it waits on <ids>", or, once dispatched,
  "not verified: <n> attempt(s) dispatched, the last was cut off by the stop; nothing of it
  was merged" (for a blocked run, without the stop). A gate reads "not run: it covers open
  contract(s) <ids>", "not passed: its run did not finish", "failed; its remediation
  `<id>` did not finish" or "inconclusive: <reason>". A completed run prints none.
- **Where it comes from.** `RunStateRecord::unfinished` reads the run record, so the
  report and a resumed run agree; `RunReport::unfinished` carries it to the command.

Tests: `kgraf-orchestrator` `run_state.rs`
`a_stopped_run_names_every_contract_and_gate_it_left_unfinished`; `kgraf-cli`
`a_stopped_run_prints_what_it_left_unfinished`.

## Amendment (2026-10-06): a paid round is stored before the next request leaves

A worker's events reach its transcript through a reader: the worker loop sends each event on
a channel, and the reader writes it. The loop did not wait for the reader, so under load the
transcript could trail the loop by whole rounds. A process killed while a later request was
in flight lost rounds that were already paid and answered (their tool calls had run), and
the resume asked them again. Measured on a loaded machine: the kill-and-resume subagent test
asked a parent's second round again in 13 of 40 runs. The transcript read at the kill ended
at the second round's `assistant_message`, while the provider had already received the
fourth request.

- **The barrier.** Before each model request, and before a compaction summary request, the
  loop waits on `WorkerEndpoint::stored`: a marker on the event channel that the reader
  answers when it asks for the next event. Both transcript writers (the orchestrator's
  attempt and `kgraf-host`'s single-worker transcript) ask for the next event only after
  they have written the previous one, so the answer means every earlier event, and the
  request's own `model_call`, is in the transcript.
- **What a kill repeats.** At most the one request in flight at the kill. A cancel while the
  loop waits settles the call as a known zero, as a cancel before the request leaves already
  did. A reader that went away stores nothing more, so the loop does not wait for it.
- **Cost.** One channel round trip per request, plus the time the reader takes to write what
  it holds. A reader that stops reading holds the loop at the next request, as a full bulk
  channel already did.
- **Rejected.** Writing the transcript on the worker's own task (it would put file I/O on
  the loop's path for every delta). Waiting in the test for the transcript to catch up (it
  would hide a real repeat of a paid round).

Tests: `kgraf-runtime` `worker_loop.rs`
`no_request_leaves_before_the_reader_has_stored_every_earlier_event` (fails with the barrier
removed); `handle.rs` `stored_resolves_only_when_the_reader_asks_past_every_earlier_event`
and `stored_gives_way_to_a_cancel`; `kgraf-orchestrator` `subagents.rs`
`a_kill_while_a_child_runs_then_resume_wakes_the_parent_once_and_repeats_no_round`.

## Amendment (2026-10-06): a stopped run's subagents are resumed with it

This decision was silent on subagents. A stop leaves every live subagent of a stopped parent
resumable: its spawn record and session stay open, and its session is marked paused, both
when the stop cancels the parent mid-call and when it finds the parent finished and held for
its children. `--resume` continues each one from its transcript when its parent is
dispatched again, and clears the mark. The details, and the defects this closed, are in the
fourth 2026-10-06 amendment of 0067.
