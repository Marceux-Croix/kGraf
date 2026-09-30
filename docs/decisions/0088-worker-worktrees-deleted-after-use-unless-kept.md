# Decision 0088: Worker worktrees and their builds are deleted after use unless the user chooses to keep them

**Status:** Accepted
**Related:** decisions 0017, 0056, 0060, 0067, 0073

## Context

Each worker builds in its own git worktree inside the project, and a compiled project builds its
own full build folder there (a Rust `target/` folder can be several gigabytes). Worktrees that
stay on disk after their attempt fill a user's disk. Someone may still want every build, so the
choice is the user's.

## Decision

1. **One setting, two states.** `WorktreeRetention` is `Delete` (the default) or `Keep`.
   `Delete` removes a worker's worktree, and everything in it, once nothing needs it. `Keep`
   leaves every worker's worktree on disk, with its build output, for the user to inspect or
   reuse; `kgraf clean <worktree>` removes one. There is no option that keeps the tree and
   deletes only build folders: which folders are build output differs by project and
   toolchain, and a list kGraf guessed could miss the large one or delete tracked files.
2. **Where it is set, one source per surface.** `kgraf orchestrate --keep-worktrees` and
   `kgraf schedule run-now --keep-worktrees` (the commands do not read `.kgraf/run.json`). In
   the desktop app, Settings > General > "Delete worker worktrees after use"
   (`delete_worker_worktrees`, on by default) for every run the app starts, box and scheduled.
   `run.json` does not choose it: a run file that sets `worktrees` fails to load, naming the
   switch and the flag, rather than being silently overridden. The host reads the policy when
   it builds a run: a change applies to runs that start after it.
3. **What each setting does with an attempt.** `Delete` removes, once nothing needs it, the
   worktree of every attempt: merged (with the merge), losing, refused by merge-back, a
   conflict its resolver settled, empty (selected or refused), failed without a commit, failed
   its acceptance command, and a merged writer subagent's. `Keep` leaves all of those with their
   build output. A kept attempt's refs are deleted, so it cannot be reopened as live work. A
   kept worktree stays registered with git, and startup cleanup under `Keep` spares it.
4. **Removed under both settings,** because keeping would break correctness or keeps no
   attempt: the earlier worktree of a subagent id a fresh spawn reuses (its stale result must
   never be applied), a spawn that never started, conversation worktrees, and the gate, probe
   and interface-stub worktrees kGraf makes for itself. `kgraf worker` is unchanged: it commits
   and removes its worktree, and keeps one whose commit was refused.
5. **Never deleted under either setting:** a worktree released to be continued (a stopped run, a
   lost lease, a voided pass), one a run record still names for a resume, the winner until its
   merge, and the worktree of a live worker.
6. **A worktree an open contract names is released only when the last one is done.** A handoff
   child continues in its parent's worktree, which holds the parent's uncommitted work; so do
   the child's retry and any later child of the same handoff. A conflict resolver cites the
   attempt kept in its worktree. The orchestrator releases such a worktree for good only once no
   open contract names it. Each child records the inherited worktree's handle, so its next
   attempt, in this process or after a resume, reopens it with the parent's work; one that is
   gone or not intact is refused, never replaced by a fresh one. A merged or empty integration
   ends the sharing: the contracts that continued in it start from the integration tip, which
   holds its work. A resolver records the conflicted worktree's handle too, so a run stopped
   before its resolver settles keeps that worktree, and the run that settles it later removes
   it (`Delete`) or deletes only its refs (`Keep`).
7. **A removal or discard that fails is reported, once.** The run's report carries one warning
   naming the worktree and the reason, for worker, conflict and conversation worktrees alike.
   The worktree stays, and `kgraf clean` removes it. A merge whose worktree cleanup failed is a
   merge with a warning, never a refusal that would redo merged work.

## Consequences

- A failed attempt's worktree is gone by default. A user who inspects failures, or wants every
  build, sets `Keep`.
- Merge-back offers the choice for a merged attempt as `MergeBack::merge_then` with
  `MergedWorktree::Keep` or `Remove`.
- An empty attempt whose worktree cannot be removed yet, because a process still runs in it,
  stays open, so its commit can be retried.
- A `run.json` written with the `worktrees` key must drop it.
