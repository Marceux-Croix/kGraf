# Decision 0067: Every agent is an addressable session; subagents, connections and a per-machine parallelism limit

**Status:** Accepted
**Supersedes:** decision 0017's starting concurrency numbers and its fixed maximums
**Amends:** decisions 0004, 0005, 0033, 0034
**Amended by:** 0095
**Related:** decisions 0009, 0012, 0014, 0026, 0027, 0028, 0043, 0048, 0056, 0057, 0060, 0063

## Context

kGraf's orchestrator directs a set of worker agents, each attached to one piece of work. Before
this decision, a worker was reachable only by giving it a fresh task; a user could not send it
a note mid-task, and a worker could not create a helper of its own. The orchestrator was the
only addressable party, which meant a small correction had to be routed through a full new
task rather than delivered directly, and there was no way to draw a nested helper on the
graph. Concurrency limits were also fixed numbers chosen without measurement, rather than
values derived from the machine and the model providers actually in use.

## Decision

**Every agent is a session.** The orchestrator, each worker attempt, and each subagent it
spawns is a session with an id, a transcript, and a live view. One worker still serves one
piece of work and then retires; its transcript is kept.

**A user can act on one session directly**, as the exception rather than the normal route: a
note, a redirect, an answer to a question, a stop, or a resume. The orchestrator remains the
normal route for directing work; a direct message is also recorded as data the orchestrator's
next turn reads, so its own account of the run stays complete.

**Steering is built.** A message to a running agent is saved durably before it is
acknowledged, then delivered at the next safe point in that agent's loop. An interrupt mode
exists too: it can abandon a model call already in flight, close out any open tool calls with
a synthetic result, and then deliver the message. Nothing already written to the transcript is
ever rewritten; a message is always appended.

**An agent can ask the user a question directly**, pausing the same way an approval request
already does. If nobody answers within a scheduled run's time limit, the worker is told plainly
that no answer arrived and asked to state its own assumption instead. This outcome is always
recorded, never left silent.

**Agents may spawn subagents.** A subagent is a session with a parent, and it takes one slot
against the same parallelism limit any other agent does; if no slot is free, the request is
refused immediately with its reason, rather than leaving the parent waiting indefinitely. A
parent's stop cascades to its children. Two kinds exist:

- **Read-only subagents** share the parent's working copy for reading, and any write is
  refused by a fence around a deliberately empty writable set, not by removing a tool.
- **Writer subagents** work in their own copy, branched from a checkpoint of the parent's, with
  a write scope that is a subset of the parent's own declared writes. Their finished work is
  merged back into the parent's copy when they complete. No two live agents ever write the
  same path at the same time.

**The orchestrator approves connections between agents, not individual messages.** A
connection is a standing permission for two agents to exchange messages freely. Some
connections are approved automatically by rule (a parent and its child, a declared dependency,
siblings under one parent, a shared file both have read); any other pair needs the
orchestrator's approval. A message to an agent with no open connection is held while a request
is raised, never silently dropped. There is no hard cap on how many agents one agent may
connect to; an unusually high rate of connections or messages raises a visible notice and
slows delivery rather than refusing it outright.

**Parallelism is one setting, with a maximum computed from the machine and the configured
model providers, not a fixed number.** The limit is the smallest of: what the configured model
providers' own rate limits can sustain; available memory; available file descriptors. Cores
do not limit how many agents may be waiting at once, since an agent waiting on a model call
costs almost nothing locally; instead, a separate pool of execution slots, sized to available
cores, gates how many shell commands or builds can run at the same time. The computed maximum
is re-measured periodically and after certain events (a rate-limit response from a provider,
a change to the configured routes, opening or closing a project), and a user-set value below
that maximum is always honored; a value above it is capped, visibly.

**Reasoning visibility is stated, not assumed.** The application shows, per configured model,
whether its reasoning is returned in full, only as a summary, or not at all, both as the
provider's stated capability and as what has actually been observed on live calls. A model's
own reasoning is never paraphrased or sent to a different model; only its own final,
already-composed answer crosses between agents (see decision 0005).

## Consequences

- A spawn requested past the current limit is refused before it starts, and a limit lowered
  mid-run never kills an agent already running.
- Fan-out that used to be blocked by a fixed cap is now paced instead: too many connections or
  messages in a short window slows delivery and raises a notice, but never drops a message.
- Every direct message, subagent spawn, and connection request is recorded in the run's
  history, so an agent's full set of interactions is reconstructable after the fact.
- The user-facing "how much can run at once" control changes shape: it is now a single slider
  bounded by a computed, explained maximum, rather than two independent fixed limits.

## Alternatives considered

- **Give every agent its own operating-system process.** Rejected: it breaks the shared
  in-memory accounting every provider call must go through, and the shared prefix mechanism
  that keeps repeated context cheap to send.
- **Keep workers unaddressable and relay every correction through the orchestrator.** Rejected:
  it pays for a full extra round trip for what is often a one-line correction.
- **Inject a steering message by rewriting the agent's already-sent instructions.** Rejected:
  it breaks the append-only transcript and moves the agent off a cached, previously-sent
  prefix, which is expensive to rebuild.
- **Give subagents a reduced tool list instead of a write fence.** Rejected: a different tool
  list is a different prefix for that agent, which forces its own separate cache entry instead
  of sharing the parent's.
- **A fixed slider maximum, even a generous one.** Rejected: it ignores real provider rate
  limits, and it ignores machines with little memory or disk.

## Amendment (2026-10-01): a compaction replaces a worker's private remainder once

Decision 0095. The cache argument of this decision (the previous request's messages are a
byte prefix of the next request's) has one exception: when the runtime compacts a worker,
its next request carries the opening message plus the compaction notice and summary instead
of the earlier turns. The shared prefix is unchanged, so the prefix digest and the prefix
check still hold; only the worker's private remainder starts over, once. The `Compacted`
event is appended between rounds, like an injection, and refused mid-stream or
mid-tool-round; everything before it stays in the transcript. A user's request to compact
one session travels on the same queue as steering (`Injector::request_compaction`) and is
taken at the next boundary; a session that has compacted refuses a second request.

## Amendment (2026-10-01): thinking text sent as `reasoning`

The table above names `reasoning_content` as the Chat Completions field. Servers disagree on
the name: Ollama, OpenRouter and recent vLLM send `reasoning` (streamed `delta.reasoning`,
buffered `message.reasoning`). The chat adapter reads both; when both are present,
`reasoning_content` is taken and the text is carried once. Before this, a thinking model on
Ollama had its tokens counted and its text dropped. The `ollama` catalog entry now declares
full visibility, from a recorded live stream and a recorded buffered answer.

## Amendment (2026-10-01): a run known to be unattended answers a question at once

An unanswered `ask_user_question` waited the unattended timeout (60 s) even when nobody could
answer. A run is now known to be unattended when the host says so
(`Orchestrator::with_question_attendance(Unattended)`; the host does this for every run, and
a box run then sets `Box`). A question the orchestrator cannot answer from the run record is
then answered at once with a recorded text that says nobody is attached and tells the worker
to state its assumption, and a session notice says so. A run whose attendance was never
declared keeps the timed wait. Test: `a_known_unattended_run_answers_a_question_at_once`.

## Amendment (2026-10-01): a hand-back with nothing written is a compaction

A hand-back from a worker that has written nothing is a compaction (decision 0095, amendment of
the same date): the child of such a hand-back would carry the same whole task, so the one
remainder replacement the runtime makes happens in the worker instead of one level down. The
hand-back's `Handoff` and the trace record name the measured cause.

## Amendment (2026-10-06): a mailbox report to a running recipient is injected at its next boundary

This decision amends 0033 so that a change notice arrives mid-run, and point 3 built the
injector, but the mailbox (`envelope.rs`, `post_message`) still delivered only in the
recipient's next fresh brief, with a `pending` trace that said the runtime cannot inject a
turn. A recipient whose attempt finished without a later brief never got the report, and no
event said so. In measured canary and benchmark traces this was about 1% of mailbox notes.

Now:
- A report to a contract with a running attempt is pushed to that attempt's session inbox as a
  `message` row (id `mail-<message>-<attempt>`, mode `at_boundary`) holding the same quoted
  data block a brief carries, and handed to the attempt's `Injector`. The runtime drains it
  only between rounds, never mid-stream or inside an open tool round. The `Injected` event
  marks the row; the orchestrator then traces `message` `delivered` with the time and
  transcript sequence of the mark, and records the copy on the envelope (`Envelope.queued`).
- An attempt that ends before a boundary leaves the copy `not_reached`. The contract's next
  fresh brief carries the report, as before. A resumed attempt continues its recorded brief,
  so the report is queued for it and injected at its first boundary.
- A report still undelivered when its recipient closes (verified, verified through its
  resolver, or superseded by a hand-back's children) is traced `message` `undelivered` and
  reported to its sender as a session notice. A mailbox row is not reported again when its
  attempt's session retires, because the contract's next brief may still carry it.
- A run that ends with the recipient held or never started does not close it, so no
  `undelivered` event is written for it.

Tests: `mailbox_injection.rs` (injected at the next boundary; falls back to the next fresh
brief; never delivered is traced and reported).

## Amendment (2026-10-06): the wait for the orchestrator's answer fits the answering model

The wait for the orchestrator's own answer to a worker's `ask_user_question` was a fixed
300 s (`SESSION_CALL_TIMEOUT`), and the same limit bounded a connection judgment. A fixed limit
cannot fit every route: a hosted model answers in seconds, while a local model under parallel
load takes minutes per call on the same route. A call that passed the limit was aborted and its
answer discarded, so its tokens were spent for nothing.

The wait is now derived per call (`kgraf-orchestrator::answer_wait`), off the orchestrator's loop:

1. A user-set wait wins: the `answer_wait_secs` run limit (30 to 1,800 s; Settings > Runs >
   Answer wait, the user settings file, a `run.json` key, `--answer-wait`).
2. Otherwise twice the 95th percentile of the answering model's last 50 counted
   `answer_question` and `judge_connections` calls in the ledger, held between 120 s and
   900 s. With fewer than 8 such calls, the model's other generative calls are used, since they
   measure the same route under the same load.
3. Otherwise 300 s. A latency history that cannot be read also gives 300 s, with a session
   notice.

An answer call that passes its wait releases the worker as if the orchestrator had no answer: to
the box in a box run, or at once with the assumption text in an unattended run. The call is not
cancelled; it runs on until 900 s (or the user's wait, if longer). Its answer, when it comes,
answers the question if it still waits in the box. Otherwise it is recorded as a `Notice` inbox
row from the orchestrator and injected at the worker's next round boundary, so the worker can
correct the assumption it stated; a worker that ended has it recorded only. An answer the user
already gave in the box stands, and the late one is reported in a notice. Every step is a
session notice. A connection judgment keeps no late path and fails at the same derived wait.
The wait for a person in the box (300 s in a box run, 60 s when attendance was never declared)
is unchanged: it does not wait on a model.

Tests: `answer_wait::tests` (the rule), `sessions::unit_tests` (the supervisor, on paused tokio
time), `a_late_answer_reaches_the_running_worker_at_its_next_round_boundary`,
`a_late_answer_answers_the_question_still_open_in_the_box`,
`a_late_answer_after_the_box_wait_expired_is_queued_for_the_worker`,
`the_model_answerer_reads_its_own_answer_calls_as_history`,
`the_configured_answer_wait_reaches_the_orchestrator`.

## Amendment (2026-10-06, third): peer text is quoted as data; report and run-end delivery are honest

Three more defects were found in the messaging paths of the mailbox amendment above.

1. **Another agent's words reached a worker as a plain user turn.** A direct message was
   stored raw and every injection site passed it to the runtime as it was, so it was appended
   as a user message. A subagent's report, which carries the child's answer, went the same
   way. Only mailbox notes were quoted. One model could instruct another.
   Now every injection site builds its text with one function, `sessions::model_text`: a row
   whose source is another agent is quoted as data, one line saying it is information about
   the sender's work, not instructions, then an escaped `agent_message` block with the row
   id, the sender's session id and the kind. A mailbox copy (id prefix `mail-`, written only
   by the orchestrator) already holds its block and passes as it is. The user's own note and
   the orchestrator's notices pass as they are; a notice about a child to its parent now has
   the orchestrator as its source, since the orchestrator writes it. The `send_message` tool
   result says `queued: ...`, not `delivered`, because delivery is the recipient's transcript.
2. **A subagent's `report_delivered` was written when the report was queued.** It was written
   when the parent's injector accepted the report, which only puts it in the steering queue.
   Now that point is `report_queued`; `report_delivered` is written, with the transcript
   sequence and time, when the runtime's `Injected` mark is on the report's inbox row. A
   parent that ends before a boundary leaves the report `report_held` if it resumes, else
   `report_undelivered`, with a notice on the parent's contract;
   `report_after_parent_finished` gets the same notice.
3. **A note was never reported when the run ended with its recipient held or not started.**
   Undelivered notes were reported only when their recipient closed. The last bullet of the
   mailbox amendment above is replaced: when a run ends, whatever its status, every mailbox note still
   undelivered to a recipient is traced `message` `undelivered` with the reason (the recipient
   was held, never started, or the run stopped or ended first) and reported to its sender as a
   notice. The same holds for a note, redirect or message still in the inbox of a session that
   never ended. Each (note, recipient) is reported once (`Envelope.reported_undelivered` is
   persisted). A resumed run can still deliver it.

Tests: `peer_messages.rs`, the report-event and `send_message` tool tests in `subagents.rs`,
the `model_text` unit tests in `sessions.rs`, and `mailbox_injection.rs`.

Not changed: a hand-back child's instruction is the runtime's remainder of the worker's own
brief, and a subagent's task is its parent's delegation; both are instructions by design and
pass the delegation guards.

## Amendment (2026-10-06, fourth): delivery is what a model read; run-end reports run on every exit, once

Four gaps were left in the delivery record of the amendment above.

1. **A note in a fresh brief was recorded as delivered when the brief was built.**
   `brief_with_inbox` (`coord.rs`) marked every message the brief carried as delivered and
   traced `delivered` before the attempt's task started. A spawn that failed before its first
   model request (a pinned context too large, a missing route) left the record saying
   delivered with no model having read it. Now the brief records one copy per attempt in the
   state `in_brief`; the attempt's task sets a slot on its first `ModelCall` event (the ledger
   row the runtime opens before the request leaves), and the loop then marks the copy
   `brief_sent` with that time, marks the message delivered and traces `delivered` (`in the
   brief of <attempt>`). An attempt that ends with no request marks the copy `not_reached`
   and traces `pending` with the reason; the message stays undelivered, so the next fresh
   brief carries it, or it is reported undelivered at the recipient's close or the run's end.
   A resumed attempt's opening is rebuilt with the same messages its first brief carried, in
   the same order, so the continued conversation is the one the model had; a first segment
   whose transcript holds a `ModelCall` settles its copies on resume.
2. **The run-end report did not run when the run loop ended with an error.** It ran only on
   the loop's normal return. Now it also runs after the error path has cancelled every worker
   and released what the run held, and after a planning error, with the reason `the run ended
   with an error`. The record is saved and the trace written after it, best effort: a failure
   there is printed, and the loop's own error is the one returned.
3. **A run-end report of a session inbox row was not persisted.** A resumed run that ended
   again reported the same row again. Now each reported row id is kept in the run record
   (`Coordination.reported_undelivered_rows`), and both the run-end report and the report at
   a session's end skip a row already in it, as a mailbox note does with
   `Envelope.reported_undelivered`.
4. **A subagent report in the parent's transcript was not traced `report_delivered` after a
   restart.** The queued-report set is loop-only. A kill after the runtime marked the inbox
   row and before the loop traced it, or after the transcript append and before the mark,
   left the report with no fate in the trace: adoption offered only unmarked rows, a
   duplicate injection was skipped silently, and a resumed parent that finished before the
   wake was called `report_after_parent_finished` with a false notice. Now adoption settles a
   marked report row of the parent whose fate the trace does not hold, a duplicate injection
   is settled from the row's mark, and a parent that is gone or finished is first checked for
   the mark. A resumed run reads the fates the trace already holds (`report_delivered`,
   `report_undelivered`) once, so none is traced twice.

Tests (each fails with its part of the fix reverted): `mailbox_injection.rs`
(`a_message_in_a_brief_no_model_request_carried_is_not_delivered`); `run_exit.rs`
(`a_loop_error_still_reports_the_notes_nothing_carried`); `peer_messages.rs`
(`a_message_to_a_session_stopped_with_the_run_is_reported_at_the_runs_end`, resumed and
stopped again); `subagents.rs` (`a_report_marked_delivered_before_a_kill_is_traced_once_after_resume`,
`a_report_in_the_transcript_but_unmarked_at_a_kill_is_traced_once_after_resume`,
`a_report_whose_fate_was_traced_before_a_kill_is_not_traced_again`).

## Amendment (2026-10-06, fifth): a stop leaves every child of a stopped parent resumable and paused

Point 5 says a parent's stop cascades to its children, and the data model says `paused_ms`
is set when the user stops the session. Both were silent on how a run stop reaches a child,
and two paths disagreed. A run stop that found the parent already finished and held (its
finish parked until its children settle) stopped the children after a second stop or the
grace and marked each one paused. A run stop that landed while the parent's last model call
was still open cancelled the parent mid-call; its children were cancelled resumably, but no
paused mark was written, so the session view showed a resumable child as neither paused nor
ended.

A second defect sat on the held path: the stop of held parents' children ran once. When the
second stop (or the grace) came before the parent's finish reached the loop, it ran with
nothing held and never ran again; the parent was held later, and the run waited on its
children with no bound.

Measured on a loaded machine, 4 parallel copies of the `subagents` test binary per
iteration: the second-stop test failed 9 of 224 runs, 4 with the child not marked paused (the
parent was cancelled after the provider answered and before the worker took the answer) and
5 with a timeout (both stops were handled before the parent was held). The test waited for
"the provider answered", which is neither state.

- **One place writes the mark.** `end_children` with `resumable` marks each child it ends
  paused, at every depth, so every path that stops a child with its parent writes it: a
  parent cancelled mid-call, a held parent stopped by a second stop or the grace, a child
  paused by the user (its own children).
- **Its restart clears it.** A child started again (adopted by its resumed parent, or
  resumed by the user) has its paused mark cleared. `paused_ms` is set while a child is
  stopped resumably, whoever stopped it, and cleared when it runs again.
- **Not one-shot.** Once the second stop or the grace has come, every held parent's children
  are stopped, also those of a parent held later; each held parent is stopped once.
- **The tests wait for the state they assert.** A held parent is waited for by the `stop`
  that ends its last turn in its transcript; the worker writes it only after it has taken
  the final answer, so a stop after it cannot cancel the attempt.

- **A resumed child gets a fresh injector.** A child paused by the user and resumed kept its
  ended worker's injector slot: the worker fills the slot it is given once, so the new
  worker's injector was dropped, and every report and message to the resumed child was
  refused as "answered before the report reached it". The resumed child then ran to its round
  budget, and its test passed only through a second attempt. Now every start of a child gets
  a fresh slot, and a pause clears it, so a report to a paused child is held in its inbox and
  offered again at its restart.

Tests: `subagents.rs` `a_stop_during_a_parents_last_call_stops_its_children_resumably`
(fails without the mark), `a_second_stop_stops_a_finished_parents_children_resumably` (fails
without the clear), `a_parent_that_answers_first_is_held_until_its_child_is_merged`, and
`a_grandchild_cancelled_by_a_pause_its_parent_outran_is_adopted_again` (fails without the
fresh slot). The "not one-shot" fix has no deterministic test: the order of a stop and a
worker's finish in the loop cannot be set from outside it; the second-stop test covers it
under load.

## Amendment (2026-10-08): an orchestrator answer says who answered and grants nothing

A worker refused `python3` asked with `ask_user_question` whether the command could be
approved. The orchestrator's model answered "Yes" from the run record, and the worker read
that as the user's approval and ran the refused command again. The permission check refused
it again, as it must, but the answer had misled the worker. Two changes:

- Every answer the orchestrator's model gives a worker (at once, in place of a box answer, or
  late at a round boundary) starts with a lead: it was answered by the orchestrator from the
  run record, not by the user, and it grants no permission; a call a permission refused stays
  refused.
- The answer prompt says the orchestrator cannot grant a permission: a question that asks to
  approve, allow or run a command, program, path or tool is never answerable from the record,
  so it goes on to the user (or, with nobody attending, the worker is told to state its
  assumption).

Tests: `an_orchestrator_first_answer_is_one_ledgered_call_settled_at_its_row`,
`a_late_answer_answers_the_question_still_open_in_the_box` and
`session_texts_are_stored_scrubbed_and_read_by_the_model_whole` (each fails without the lead),
`the_answer_prompt_says_a_permission_question_is_not_answerable`.

## Amendment (2026-10-10): a merged report injected into a running attempt says its worktree lacks the work

A library contract's `done` report, with the API it now provides, was injected into a running
CLI attempt. The attempt then read the library file three times and got the stub each time: its
worktree was made before the merge. Its reasoning spent rounds on the contradiction.

- **The rule.** When a `done` report is queued for a running attempt and its sender's attempt
  merged, kGraf adds one line after the quoted data, as its own text: "Contract <id>'s work merged
  after your attempt started, so your worktree does not hold it: the files the message names
  still read as they did when you started. Build on what the message says they now provide."
- **Not added** to a fresh brief (it starts from the integration tip), to a report of an empty
  integration, or with plain-directory workspaces.
- The attempt's check copes with the same gap: a failing check runs again on the attempt merged
  onto the tip (decision 0018).

Rejected: merging the tip into the running worktree at the round boundary: the worker may be
mid-edit, and the merge would change the scope of its commit.

Test: `kgraf-orchestrator` `tests/mailbox_injection.rs`
`a_message_to_a_running_recipient_is_injected_at_its_next_round_boundary`. With the line not
added, it fails.
