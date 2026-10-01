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
