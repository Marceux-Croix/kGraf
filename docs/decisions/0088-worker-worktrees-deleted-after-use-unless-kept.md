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
- Under `Delete`, a worktree a process still runs in is not removed, including one a daemon
  that left its process group or session keeps busy: removal retries for about one second,
  then reports the worktree once. It stays registered and locked, and the next startup
  cleanup (or `kgraf clean`) removes it once its processes have ended. kGraf does not kill
  such a process: finding it means matching every process's session or working directory,
  and a working-directory match could hit a shell of the user's own open in the worktree.
  Stop the process, then run `kgraf clean`.
- A `run.json` written with the `worktrees` key must drop it.


## Amendment (2026-09-30): the graph reads merged files from the integration view

Under `Delete`, merge-back removed the winner's worktree during the merge, and the reindex
that followed read that missing tree. The indexer took each missing file for a deleted one,
so every merged file and its nodes left the graph, and the first dependent contract failed
at spawn on a slice node that no longer existed.

- **An integration view.** `Workspaces::integration_view` gives a tree holding the
  integration tip's files. `GitWorkspaces` keeps one detached worktree per run (name
  `view-<nanos>`, a kGraf-made worktree that startup cleanup removes), made at the first
  call and moved in place to each new tip (`git checkout --detach`, attributes read from the
  tip). One tree moved in place keeps every file's recorded source root valid across merges
  and rewrites only changed files; a fresh worktree per merge would check out the whole
  repository each time and orphan the roots recorded for earlier merges. It holds no build
  and no worker's work. Dropped with the run's `GitWorkspaces`: removed under `Delete`, left
  under `Keep`.
- **The reindex after a merge reads the view**, under both settings, and slices of merged
  code read it back. A provider without a view (plain directories) has its merged worktree
  read, as before, and must keep it.
- **The indexer refuses a missing root.** `Indexer::index_paths` returns an error when its
  source root (or the store's root) is not an existing directory, and reads and removes
  nothing. A walk of a missing root already errored and kept every file.
- The merged worktree's removal under `Delete` is unchanged: it happens with the merge.
