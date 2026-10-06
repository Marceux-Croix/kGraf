# Decision 0095: A worker that cannot hand back is compacted once by the runtime, with a notice it cannot suppress and a recall tool

**Status:** Accepted
**Amends:** 0057 (a worker may now compact once, by the runtime, with a model-written
summary; the run record is still never rewritten), 0067 (a compaction is the one point
where a worker's next request is not a byte extension of its last; the request to compact
travels on the steering seam)
**Related:** decisions 0004, 0005, 0025, 0043, 0055, 0056, 0071

## Context

Decision 0057 ruled that workers never compact: they are sized to finish under half the stop
point and hand their remainder back at it. The remainder becomes a child contract through
the delegation guards, and the guards end a chain at the depth cap. A worker with one
indivisible piece of work that keeps overflowing therefore reached the cap and its contract
was held. A long run should degrade instead of stopping, should still tell the orchestrator
that the piece was too big, and should lose nothing.

## Decision

1. **Trigger: the stop point, where a hand-back cannot be split.** The stop point stays at
   the compaction point (80% of the effective window by default). A worker spawned with
   `StopPointPolicy::HandBack`, the default, hands its remainder back there as before. The
   orchestrator spawns a worker with `StopPointPolicy::Compact` when the delegation guards
   would refuse the hand-back's child (the remainder is worded as the runtime words it,
   `kgraf_runtime::handback_remainder`, and put to `guards::check`): the depth cap, the task
   length bound, or a child id already taken. That worker compacts at the stop point instead.
   A worker compacts at most once; a second overflow hands back with everything the loop
   observed. The model never triggers a compaction and its cooperation is never needed.

2. **The summary: one ledgered call in a fixed schema; the instruction is never
   summarized.** The runtime sends the worker's own context (as its next round would have
   carried it, masked as every request is) to the worker's own route, with a fixed request
   for one JSON object: `goal`, `decisions`, `files_touched`, `open_items`, `constraints`. The
   call is a ledger row of the worker (kind `worker_compaction`) and is listed on its result,
   so every token is attributed. A constraint is kept only if the instruction holds it word
   for word (whitespace aside); a paraphrase is dropped and counted. A reply out of schema, or
   a failed call, gives a summary built from what the loop observed (the writes that
   succeeded and the changed files), labeled `summary_source: runtime` with the reason; the
   worker goes on either way. The files the worktree shows changed are added to
   `files_touched` whatever the model said. The worker continues from the opening message
   (instruction and slices, verbatim) followed by the runtime's notice and the summary. The
   shared prefix does not change (decision 0043).

3. **The notice: a fixed structured event the model cannot skip, change or forge.**
   `WorkerEvent::Compacted { notice, summary }` is built by the loop alone, between rounds; no
   model output becomes one, and text that claims a compaction is only text. The notice
   carries the worker, task, trigger (`stop_point` or `user_request`), time, tokens in use,
   stop point, window, summary size, the summary call's ledger id, the summary source, the
   transcript path, the number of archived entries, whether the worker has the recall tool,
   items done and items remaining. It is on the worker's result. The orchestrator writes it
   to the trace (`worker_compacted`) and the run record (`compactions`), counts a stop-point
   compaction as one sizing miss per worker, and reports that the contract was larger than
   planned. The ledger already holds the worker's peak context and the summary call, which is
   what the sizer projects later plans from.

4. **The full history stays on disk.** The transcript file is append-only, so every event
   before the `compacted` line is the pre-compaction history, unchanged, and the notice names
   the file. A resume rebuilds the compacted remainder from the stored events and does not
   compact again.

5. **Recall: `recall_history`.** Every assembled worker lists the tool, so the tool list stays
   one per vendor and model. The worker loop answers every call itself, after approval like
   any call, from the calling worker's own pre-compaction events as the transcript stores
   them (with secrets scrubbed, decision 0071), never another worker's. `query` lists up to 8
   entries, those holding the whole phrase first, then by the number of query words held, with
   a short excerpt; `from` (with `count`, at most 20, and `offset`) reads entries whole. A
   result is at most 8,000 characters and says where to continue. Before a compaction it
   answers that there is nothing to recall.

6. **Graceful degradation at the depth cap.** Before this decision, a chain of hand-backs
   that reached the cap held its contract ("re-split refused: delegation depth N exceeds the
   maximum M"); the worktree was kept and nothing failed, but the work stopped. Now the worker
   at the cap compacts and goes on. A worker that overflows again after its compaction hands
   back; the re-split is refused, and the held reason adds that the worker had already
   compacted, that its work is in the worktree, and where its full transcript is.

7. **A user's `/compact <worker>` uses the same mechanism.** The request travels on the
   worker's steering queue (`Injector::request_compaction`) and the worker compacts at its next
   boundary, once. A worker that finishes before its next boundary never compacts. A worker
   that has compacted refuses a second request. A user's compaction is recorded and traced
   but is not a sizing miss. The orchestrator itself holds no transcript, so `/compact`
   without a target is refused as before.

## Alternatives considered

- **Compact every worker at the stop point instead of handing back.** Rejected: a hand-back
  that can be split gives the remainder a fresh worker with a full window and no summary
  loss; compaction is the fallback where that is not possible.
- **Let the model decide when to compact.** Rejected: a model-triggered compaction is exactly
  the notice a model could skip.
- **Masking and truncation only, no summary.** Rejected for this fallback: at the stop point
  masking has already run on every request, so only replacing the remainder frees room. The
  recall tool keeps every masked or dropped detail reachable.
- **A second summary at a second overflow.** Rejected: a summary of a summary compounds error.
  The second overflow hands back with full information instead.
- **Keep the recall tool out of the prefix until a compaction.** Rejected: a different tool
  list is a different prefix and cache entry, and the prefix check would refuse the request.
- **Reuse.** codex-rs `compact.rs`, goose's summarize-on-threshold and OpenHands'
  `LLMSummarizingCondenser` were read; none quotes the instruction verbatim, keeps the history
  searchable by the agent, or fits the accounting path. Written new.

## Consequences

- Tested on the mechanism, in `kgraf-runtime/tests/compaction.rs` and
  `kgraf-orchestrator/tests/compaction_depth.rs`: one compaction with the summary call in the
  ledger; the next request is the verbatim opening plus the notice on the same prefix; model
  text claiming a compaction does nothing and a refusing reply cannot stop one; a failed
  summary call still compacts; the earlier events are intact and a resume keeps the archive;
  recall finds a detail the summary dropped; a second overflow hands back; a cancel inside the
  summary call writes no notice and no round, and the resume compacts once; a chain at the
  depth cap compacts and finishes instead of holding; a worker that overflows again is held
  with its transcript named.
- Every worker's prefix gains `recall_history`; a transcript recorded before it cannot be
  resumed on the new prefix (refused, as any prefix change is).
- Subagents still hand back to their parent, which decides what to do with the rest. The
  single-attempt `kgraf worker` command keeps the default hand-back policy.
- The measured compaction recommendation leaves compacted workers out: they finished on a
  summary, not on the context their peak describes.
- Compaction makes a long worker cheaper to continue than to stop; compactions per run and per
  model are recorded from the first build, so whether more work leans on it can be measured.
- Reopen if recorded runs show summaries losing facts that recall does not recover, or if
  compacted workers complete measurably less often than handed-back chains.

## Amendment (2026-10-01): no hand-back with nothing written, and a hand-back says why

A local model with a 32,768-token window and a stop point at 35% (11,468 tokens) handed back
after one step, twice in a row: one read of the file plus a long first reply (6,319 prompt and
3,661 generated tokens, the read result on top) measured 11,561 tokens. Each child received the
whole task again and did the same, until the depth cap. The measurement was right; the hand-back
moved no work.

- **No hand-back with nothing written.** At the stop point a worker that has written nothing
  (no `done` item, no changed path) compacts once, under either policy. After that compaction,
  with nothing written and a context under the model's effective window, it goes on; at or
  past the window it hands back, because the next request would not fit. A worker that wrote
  something hands back as before.
- **The cause is recorded.** `Handoff::cause` is `measured_context` (the last reported usage
  plus the estimate of what was added since reached the stop point) or `queued_messages` (the
  transcript was below the point and the queued steering messages crossed it). The trace's
  `compaction` record carries it as `cause`. A hand-back is never a projection of growth, and
  no tool lets the model request one; a projection only sizes a plan at dispatch.

Evidence: `kgraf-runtime/tests/compaction.rs` (`a_stop_point_reached_with_nothing_written_...`,
`a_second_stop_point_with_nothing_written_goes_on_while_the_window_holds`,
`a_hand_back_with_nothing_written_past_the_window_names_the_measured_context`,
`a_hand_back_after_a_write_stands_and_names_the_measured_context`) and
`kgraf-orchestrator/tests/orchestrator.rs` (the trace's `cause`).

## Amendment (2026-10-01): the context measure counts what the next request carries

The measure was the last usage report's input plus its output, plus an estimate of what was
added since. The output included reasoning, and no adapter sends reasoning back: Chat
Completions, Responses, Messages, Gemini and Converse each drop the reasoning part when they
serialize a request. A thinking model's reasoning is billed on the round that produced it and
is not in any later prompt, so the measure read high by the reasoning size and a worker reached
the stop point too early (a 6,319-token prompt with 3,661 generated tokens measured 9,980
before the read result, while the next prompt would hold about 6,400).

- **Reasoning counts only where the route resends it.** `Provider::resends_reasoning`
  (default `false`; the rate-limit and route wrappers forward it) tells the transcript builder.
  An adapter that serializes reasoning must override it to `true`. Before the loop sets it, the
  builder counts reasoning, the conservative side.
- **Where the route reports reasoning apart** (`reasoning_output`), exactly that share leaves
  the measure. **Where it does not** (Ollama's chat API, Converse) the reasoning is inside
  `text_output`, and a lower bound leaves: the reasoning text kGraf holds at five bytes per
  token, never more than the reported text output. Real text runs three to four bytes per
  token, so some reasoning stays counted. A round no usage report covered adds its reasoning
  estimate only where it is resent.
- **The trace reports the measure that decided.** `WorkerResult::peak_context_tokens` is the
  largest value any stop check read (queued messages included), and the trace's
  `peak_context_tokens` is the larger of that and the provider's reported input. It was the
  reported input alone, which hid the output share.
- Each adapter has a test that a request built from a message with reasoning carries none of
  its text, so the flag follows the code.

## Amendment (2026-10-02): a hand-back's child is sized to fit under the stop point

A local run with the stop point at 30% of a 32,768-token window (9,830 tokens) showed a child
contract made by a hand-back starting at 6.7k to 7.5k tokens. One round of real work crossed the
stop point, the child handed back, and so on to the depth cap of 3, where kGraf failed loud. Two
causes. The child inherited the parent's whole pinned slice set (`child.slices =
parent.slices.clone()`), and its instruction was the runtime's remainder of the parent's whole
assembled brief, which for a child already quoted its parent's: the first request grew by 150 to
490 tokens per level (4,627, 5,114, 5,272, 5,430 uncached input tokens). Nothing measured the
child against the stop point.

- **One wrapper.** `handback_fit::flatten_remainder` rebuilds the remainder as the header, the
  chain's original instruction (the last lineage task that is not a remainder), and the
  paragraphs the parent's brief appended at dispatch, each once. The acceptance and write lines
  are left out, since the child's own brief states them. A last line says which hand-back the
  child is: the delegation guards' cycle rule compares tasks, and a child that restated its
  parent's task word for word would be refused as a cycle (the nesting had hidden that).
- **A cap on pinned slices.** The child is sized like a planned contract (0057): the route's
  prefix estimate at one and a half times and the brief at one token per two bytes must be at
  most the plan target (half the stop point). What is left is the most tokens of slices it gets;
  the parent's slices are kept in order while they fit, each counted as the route's slice source
  renders it (a slice grows with neighbors up to the per-slice budget, so the node's source is
  no measure). A notice says how many were kept. The cap is stored on the child
  (`slice_cap_tokens`) and applied at every dispatch, so re-resolving the slice names cannot
  restore the set.
- **A child that cannot fit is not made.** When the prefix and the instruction alone pass the
  target, `stop_point_policy` spawns the parent with `StopPointPolicy::Compact`, as it does when
  the guards would refuse the child. `resplit` refuses the split at the first hand-back with the
  numbers, and holds the parent with its worktree, as it does for a guard refusal.

Tests: `handback_fit` unit tests (the cap, the unfit case with its numbers, slice choice, a chain
of four remainders does not grow, an interface paragraph is carried once, a worker's own handoff
item is untouched); `kgraf-runtime` `original_instruction_reads_through_every_hand_back_wrapper`;
`tests/handback_fit.rs` measures the first request of every child (at most the plan target, some
slices kept, equal size at every depth, and a contract whose child cannot fit compacts without
handing back).

## Amendment (2026-10-06): what a compaction clears, what it carries, and a summary that keeps the task

A local run with a 32,768-token window compacted both attempts of one contract. Three defects:

- The second attempt had written nothing when it compacted. After the compaction it read two
  files and ended its turn with an announcement of the work. It was recorded completed, and
  verification failed on the untouched stubs: the no-change guard was off for every compacted
  worker. The guard's only plausible reason to skip a compacted worker, not to catch one whose
  work was done before its compaction, is already met: the writes and changed paths carry across
  the compaction and are rebuilt from every replayed round on resume.
- The first attempt read one file twice before its compaction and once after. The third read
  was refused as a stall: the stall detector kept its counts across the compaction, although the
  earlier results were no longer in the model's context.
- The second attempt's summary reply was not valid JSON. The fallback summary held only the
  observed writes, which were none: an empty goal and no open item.

Rulings:

- **The no-change guard holds after a compaction.** A compacted worker with nothing written that
  ends as done gets the one notice, and a second such end fails; a worker that wrote before its
  compaction completes as before. The notice is one per attempt and is not reset by the
  compaction.
- **A compaction clears what describes the replaced context** (`Worker::reset_context_state`):
  the stall counts, and the one-continuation flags of an empty turn and an output-ceiling stop,
  whose notice or cut-off turn is not in the remainder. A call repeated to the threshold after
  the compaction still stalls. A resume resets the replayed stall counts at the first round after
  the compaction (`TranscriptBuilder::replay_compaction_round`).
- **A compaction carries what describes the work:** writes and changed paths, the no-change
  notice, the patch-rejection count, the step budget, the progress checkpoints, the refusal and
  denial counts and the malformed-output bound.
- **The summary is completed from kGraf's state** (`compaction::complete_from_state`): an empty
  goal becomes the instruction's first non-blank line (through every hand-back wrapper, at most
  300 characters), the observed writes fill an empty decision list, the changed files are added,
  and each declared write path with no recorded change and no open item naming it becomes the
  open item "write <path>: no change to it is recorded yet". A fallback summary is exactly this; a model summary keeps its
  own entries and gains only what it left out.

Tests (`kgraf-runtime/tests/compaction.rs`); each except the second, which guards the case the
guard must not catch, fails with its fix reverted:
`the_no_change_guard_holds_after_a_compaction_with_nothing_written`,
`a_write_before_the_compaction_still_counts_after_it`,
`a_re_read_after_a_compaction_is_not_a_stall`, `a_real_repeat_after_a_compaction_still_stalls`,
`a_resume_after_a_compaction_counts_repeats_from_the_compaction`,
`an_empty_turn_after_a_compaction_gets_its_own_continuation`,
`an_output_ceiling_stop_after_a_compaction_gets_its_own_continuation`,
`a_runtime_summary_carries_the_goal_and_the_unwritten_paths`; unit test
`state_fills_an_empty_summary_and_only_the_gaps_of_a_model_summary`.
