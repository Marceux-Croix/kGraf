# Decision 0096: The round budget is a progress checkpoint, not a wall

**Status:** Accepted
**Related:** decisions 0043, 0056, 0057, 0067, 0075, 0095

## Context

A worker's round budget was a wall: at 30 rounds an orchestrated worker stopped, whatever
it was doing. Long contracts that were going well failed at the wall and were retried from
the start, while a worker that had spent 30 rounds on the same failing patch was treated
the same way. The stall detector (0056) only catches identical calls with no successful
write between them; it does not see a worker that varies its failing patch or churns a
file. Sloppy work needs an effective cap; work that is producing results should not be
capped.

## Decision

1. **The budget is a block.** The brief's round budget `B` (the orchestrator's default is
   30, `kgraf worker`'s is 12) is the size of one block of rounds. At the end of each block
   the runtime measures a checkpoint between rounds. The first checkpoint falls where the
   step budget would warn that three rounds remain: `B - min(3, B - 1)` rounds in. A block
   that is granted adds `B` rounds, so later checkpoints fall every `B` rounds. Measuring at
   the warning point keeps a run that ends exactly as before (the warning, then the final
   answer round), and a run that goes on is never told a warning that was not true.

2. **Progress is measured, never claimed.** Since the previous checkpoint:
   - **(a) Net file change.** Every file the worker wrote, every file git lists as dirty
     (when the worktree's recorded gitlink allows the read), and every file the previous
     checkpoint recorded is hashed with blake3. A file counts when its content differs from
     the previous checkpoint's record. A file the previous checkpoint did not record is
     compared with its content captured just before the block's first write to it, or,
     with no capture, by whether git lists it as dirty. A file written and put back is no
     change.
   - **(b) Checks improving.** A command the worker ran (the `shell` tool, or a tool whose
     input names `command` or `cmd`) is scored from its output: pass and fail counts for
     `cargo test`, `cargo nextest`, pytest, unittest, `node --test`, jest and vitest; error
     counts for cargo builds and `tsc`; otherwise the exit status. The command's latest
     score in the block is compared with its score at the previous checkpoint, or with its
     first score in the block. Better means fewer failures without fewer passes, more
     passes without more failures, fewer build errors, a build that now reaches its tests,
     or a nonzero exit that became zero.
   - **(c) No sloppy pattern.** In the block: one identical error from a non-command tool
     (digits ignored) 3 times; at least 3 failed write calls and more failed than
     succeeded; files written and put back with no net change anywhere; one identical call
     with an identical, unmasked result 3 times (the stall detector's call digest, 0056).

   A block is progressing when (a) or (b) holds and (c) finds nothing. Denied, refused,
   unrun and steer-interrupted calls are not observed. Model text is never read.

3. **Verdicts.**

   | Measured | Earlier checkpoints | Verdict | Effect |
   | --- | --- | --- | --- |
   | progressing | below the ceiling | `continue` | the same worker gets `B` more rounds |
   | progressing | at the ceiling | `ceiling` | the old wall: the warning, then the final answer round |
   | not progressing | none productive | `end_no_progress` | the old wall |
   | not progressing | the previous one productive | `grace` | `B` more rounds and a notice to the model |
   | not progressing | the previous one not | `end_stuck` | `BudgetExhausted` at once, no answer round |

   A worker that never measured progress meets its round budget exactly as before. A
   worker that was productive and then shows no progress at two checkpoints in a row ends
   at once: an answer round would let model text turn a stuck attempt into a completion.
   The outcome keeps the orchestrator's `BudgetExhausted` handling: a failed live outcome
   for the route (0075), a stuck report naming the checkpoint, a retry, then a hold.

4. **The same worker continues.** A granted block is not a new worker and not a context
   reset; the conversation goes on. Context filling still hands back or compacts first
   (0057, 0095): the context check runs before the checkpoint at every boundary.

5. **The ceiling.** `LoopSettings::max_checkpoints`, default 10: at most 10 blocks. At the
   ceiling the worker gets the old wall. 1 restores the old behavior exactly.

6. **The record.** Each checkpoint is a `WorkerEvent::Checkpoint` in the transcript (built
   by the loop alone, between rounds; never model input) carrying the measurement, the
   verdict, the rounds granted and the baseline the next checkpoint compares with. A
   resume reads them back, restores the granted rounds and replays the open block's
   events. The result carries `round_blocks` (block size, ceiling, checkpoints). The grace
   notice is a `Notice` of kind `no_progress`.

7. **The orchestrator's sizing signal.** A worker that ran past its round budget is
   evidence the contract was bigger than planned, the same signal as a hand-back or a
   stop-point compaction: `WorkerFinished.sizing_miss` is set, the run counts one sizing
   miss per worker (not again when the worker also handed back or compacted),
   `round_extensions` and `checkpoint_stops` are counted, the trace gets
   `worker_checkpoints`, and an informational report says so. The ledger holds the
   worker's rounds and peak context, which the sizer projects later plans from (0057).

## Options considered

- **Raise the budget.** Rejected: it caps sloppy work later, not better.
- **Let the model ask for more rounds.** Rejected: a model's claim of progress is the
  signal least worth trusting.
- **A new worker for each block.** Rejected: it resets the context the work was built in.
- **Grace at the first checkpoint too.** Rejected: a worker that measured no progress in
  its whole first block would get double the old budget; the grace is for a worker that
  was productive and paused to read.
- **An answer round when a stuck worker ends.** Rejected: the attempt ends as stuck; the
  answer round would let model text claim a completion the verifier then has to refuse.
- **Reuse.** Searched mini-swe-agent (a fixed step limit), SWE-agent (a cost limit and an
  autosubmit), OpenHands (an iteration limit and a stuck detector over repeated actions),
  codex-rs and goose (turn limits). None extends a budget from measured worktree and test
  progress. Written new.

## Consequences

- **Tests, on the mechanism** (`kgraf-runtime/tests/progress_gate.rs`): a productive task
  runs 81 rounds on a budget of 30 in one worker and completes, with the checkpoints on
  its result and no warning; a sloppy loop after a productive block ends `BudgetExhausted`
  at the third checkpoint with no answer round and one grace notice; a worker that never
  measured progress meets its budget as before; research after a productive block gets
  the grace and continues; write-then-revert is no progress; new files beside a repeating
  failed patch are not progress; the ceiling stops and reports; model text claiming
  progress changes nothing; a resume keeps the granted rounds. `kgraf-orchestrator/tests/
  round_checkpoints.rs`: one attempt, one sizing miss, one `worker_checkpoints` event.
  Mutations of the sloppy veto, the progress test, the pre-write capture, the grace rule
  and the resume restore were each caught.
- **Cost.** A checkpoint hashes the files the worker touched and runs one `git status`,
  once per block; a declared write reads the file once before its first write in a block.
- **Induced demand.** Longer workers are now cheaper than retries; `round_extensions` and
  the `worker_checkpoints` trace make the count of extended workers per run measurable.
- **The ceiling is a setting.** `max_checkpoints` is a global user setting (Settings >
  General slider, 1 to 50, default 10; 1 means the old fixed round limit), a key in
  `.kgraf/run.json`, and the `--max-checkpoints` flag of `kgraf orchestrate` and `kgraf
  worker`, with the same precedence as the other run limits (decision 0017): flag, then run
  file, then user setting, then default. It is passed into `LoopSettings::max_checkpoints`
  of every assembled worker.
- **Reactivation.** Reopen if recorded runs show extended workers verifying measurably less
  often than workers that finish inside one block, or a sloppy pattern the gate misses.

## Amendment (2026-10-01): an empty turn is not a completed attempt

A turn with no tool call and no visible text (the plan only in the model's reasoning, stop
reason end of turn) was recorded as a completed attempt. Now the first such turn is dropped,
the model is told "You ended your turn without calling a tool or giving an answer; continue
the task with a tool call, or answer.", and the round stays counted, so a checkpoint sees it
as a round without progress. A second empty turn in a row, or an empty turn on the final
round, ends the attempt as failed with a reason that names the stop reason. A tool call or an
answer between two empty turns resets the count. The adapters normalize the provider's finish
reason into the stop reason, which is recorded on the turn; the raw string is not carried.
Tests: `an_empty_turn_gets_one_notice_round_and_is_not_completed`,
`two_empty_turns_in_a_row_fail_the_attempt`,
`a_tool_call_between_empty_turns_resets_the_empty_turn_count`.

## Amendment (2026-10-02): three gaps closed

- **The first block is measured against its midpoint and its checks.** The first checkpoint
  compared files with the empty start, so any early write counted, and a worker that wrote its
  files in round 2, then rewrote them while its test run stayed at the same failure, was
  extended block after block. In the first block, a changed file now counts only if it also
  differs from its content at the block's midpoint (the worker captures the digests once, at
  half the first checkpoint's round). And when the block ran a test or build command, none of
  those improved, and every one ended failing, changed files alone do not count: the
  checkpoint lists them as `files_discounted` and says so. A command that ended passing leaves
  the files counted. Later blocks, the sloppy patterns and the verdict table are unchanged. A
  resumed attempt, or a block too short to have a midpoint, falls back to the compare with the
  start. Series now ending at the old wall instead of `Continue`: early writes, churn, flat
  failing check.
- **An end as done with only reads is not a completion.** The no-change notice required that
  the attempt had made no tool call at all, so an attempt that read two files and ended its
  turn was recorded completed. It now keys on no successful write and no command that changed
  the tree, for a contract that declares writes. The failure text says "no write and no
  mutating command".
- **A pinned context that cannot fit is not retried.** When the prefix, slices and instruction
  already reach the stop point, no round can run and the same brief fails the same way. The
  contract is now held after that attempt, with the sizes and the remedy (split the contract,
  use a route with a larger window, or raise the stop point). If any attempt of the round
  failed another way, the normal retry rules apply, since another route may fit.

Tests: `progress.rs` `a_first_block_with_a_flat_failing_check_does_not_count_file_changes`,
`a_first_block_with_a_failing_check_that_improved_is_progress`,
`a_first_block_whose_check_ends_passing_keeps_its_file_changes`,
`a_first_block_counts_only_files_that_changed_since_its_midpoint`,
`a_later_block_is_measured_against_the_last_checkpoint_as_before`;
`progress_gate.rs` `a_first_block_of_early_writes_and_churn_with_a_flat_failing_check_gets_no_extension`;
`compaction.rs` `a_read_only_attempt_that_used_a_tool_is_not_a_completion_when_the_contract_writes`;
`kgraf-orchestrator` `pinned_context_hold.rs`.

## Amendment (2026-10-06): a failed write made good later is not a sloppy pattern

A worker that cannot hand back compacted once, then failed three patches in a row (two context
mismatches and an update chunk with only added lines), then replaced the module whole with
`*** Add File` (nine of its ten tests passed on that file). At its next stop point the segment
since the compaction measured 1 file changed and "3 failed write(s) against 1 that succeeded",
so the failed-writes pattern vetoed the change and the worker was stopped with "no measured
progress", its changed file named in the same sentence. Its work was kept in the worktree and
never checked.

The sloppy patterns exist to stop a worker that cannot write or that goes round in circles.
A failed write that the worker later made good is neither. Now a failed write call whose
every path a later successful write call of the same block (or of the segment since a
compaction) wrote is recovered:

- it is not counted as a failed write (`FailedWrites` counts the failures not made good);
- its error does not count toward a repeated identical error.

A failure on a file that no later write reached still counts, so successful writes to other
files recover nothing: a worker that writes scratch files beside a patch that keeps failing
is vetoed as before (`progress_gate.rs`
`new_files_beside_a_repeating_failed_patch_are_not_progress`, unchanged). A change still
needs net file change or an improving check; a repeated error of a tool that writes nothing
still counts; failures after the last good write still count; write-then-revert still
counts. The rule lives in the one measure both the checkpoint and the recompaction decision
use (`kgraf-runtime/src/progress.rs`, `measure_log`).

Tests: `progress.rs` `failed_writes_followed_by_a_successful_write_are_not_sloppy`,
`a_recovery_does_not_hide_read_errors_other_files_or_a_later_failed_write`; `compaction.rs`
`failed_writes_before_a_write_that_works_do_not_stop_a_recompaction`.
