# Decision 0094: Permission modes set by the user, the limits Bypass keeps, grouped permission requests, and plan review before dispatch

**Status:** Accepted
**Related:** decisions 0004, 0017, 0027, 0029, 0035, 0053, 0063, 0084, 0093

## Context

Decision 0093 let a worker report a permission shortfall, and let the orchestrator grant within
a ceiling given on the command line, with a separate switch for runs inside a disposable
container. Permissions are better handled as Claude Code handles them: one mode, chosen in the
desktop and changed at any time, including a mode that leaves the orchestrator in charge and a
Bypass mode that grants everything, with the limits Claude Code keeps in bypass kept the same
way. Many requests arrive at once and should be answered together, and a user may want to see
the plan and say yes before any worker starts.

## Decision

1. **One setting, five modes, set only by the user.**

   | Mode | A worker's approval ask | A worker's permission shortfall (0093) |
   |---|---|---|
   | Manual | the user answers | the user answers |
   | Accept edits | granted for a file tool and the file programs `mkdir`, `touch`, `mv`, `cp`; the rest asks | a file-scope widening and those programs are granted; the rest asks |
   | Auto | granted for what the user's allow set names (programs, tools, paths); the rest asks | the same |
   | Orchestrator decides | granted, in the orchestrator's name | the orchestrator grants or refuses each one, and never asks |
   | Bypass | granted | granted; the shell runs every command no `forbid` rule refuses |

   The mode lives in one live control shared by the orchestrator, every worker's shell policy
   and ask resolver, and the desktop. It changes only from the desktop's dropdown, the
   user-level `permissions.toml` (in kGraf's per-user configuration directory, refused when it
   lies inside the project), or the command line. `run.json` and `worker.json` refuse every
   permission key, and the `report_shortfall` tool refuses any field beyond the report.

2. **A mode change applies at once, including to requests already waiting.** A worker's shell
   reads the mode on every command (its tool description does not change, so the shared prefix
   stays identical). An approval ask waiting in the box is re-routed on each change and released
   when the new mode grants it. A shortfall request waiting for the user is decided again on
   each change: granted, refused, or left waiting.

3. **The ceiling is the mode.** It replaces decision 0093's operator ceiling. The decision reads
   the mode and the allow set, never the worker's words. A grant is a program the contract's
   shell allows, or a path added to the contract's declared writes (leased at the next dispatch
   and accepted by the verifier's scope check). Orchestrator decides refuses, without asking,
   when the re-dispatch bound is reached, when a grant did not help, for a tool, and for a path
   that is absolute, climbs out, or is one no worker may write. Where the user would be asked:
   with a person in the orchestrator box the request waits on the contract, which is held until
   the user answers or a mode change settles it; with nobody attending, it is denied and the
   hold says how to allow it.

4. **The limits Bypass does not lift.** A hook installed last in every worker's approval chain,
   so it reviews the input every earlier hook left, and the ask resolvers enforce:
   - the user's deny rules (`tool` or `tool(pattern)`) block in every mode, matched against a
     shell command line, every command of a `sh -c` script, and a file tool's paths;
   - the user's ask rules ask in every mode; such an ask is never covered by a persisted allow
     rule or granted by a mode;
   - deleting the filesystem root, a top-level directory, the home directory or a glob over one
     is refused in every mode (inside `sh -c` scripts too, following their `cd`s); deleting the
     workspace itself, its `.git`, a path that climbs out
     of it, or recursively deleting an operand that cannot be resolved asks;
   - hooks cannot override deny or ask rules, and every hook runs in every mode;
   - the OS sandbox stays in force;
   - a run nobody attends denies what would still ask;
   - `disable_bypass = true` in the user file refuses Bypass from every source;
   - Bypass is refused when kGraf runs as root, unless `KGRAF_CONTAINER_SANDBOX=1` states that
     it runs in a disposable container;
   - the desktop offers Bypass only after the user turns on "Allow Bypass mode" in the settings
     surface, and its first use shows a warning the user must accept.

   kGraf's read tools never read outside the worktree, `ask_user_question` and the dependency
   consent are not permission requests and no mode answers them, and a skill's `allowed-tools`
   still restricts in Bypass. A worker's write to `.git`, `.kgraf` and the other protected
   paths is still refused in Bypass; Claude Code's bypass allows writes to its protected paths,
   so this differs. kGraf has no managed settings scope and no restricted sessions.

5. **Every grant and every mode change is recorded:** in the run's trace and in the graph's
   permission events, which the activity card shows from the change log. A grant on a shortfall
   is also kept on the contract with who granted it.

6. **Grouping.** Requests raised within 2,000 ms of a group's first request are one group.
   Approval asks and shortfall requests group together. A group is one card in the box, with
   Grant and Deny on each item and Deny all and Grant all, which confirm first.

7. **Plan review.** Off by default; set by the switch beside the mode dropdown, the user file
   or `--plan-review`. When on, the plan waits in the orchestrator box after planning, each
   contract with what it waits on, writes and reads, and the graph lights what it writes.
   Approve dispatches it; Change sends the user's note to the planner and shows the new plan;
   Reject ends the run blocked with nothing dispatched. A run with nobody to approve is refused
   before any planning call, unless `--approve-plan` approves it.

8. **The command line.** `kgraf orchestrate` takes `--permission-mode`, `--allow PROGRAM`
   (`--may-grant` is its older name; alone it selects Auto), `--allow-path`, `--deny`, `--ask`,
   `--permissions-file`, `--plan-review` and `--approve-plan`. `--container-sandbox`, with
   `KGRAF_CONTAINER_SANDBOX=1`, is another name for `--permission-mode bypass`.

## Options considered

- **Keep a separate ceiling beside the mode.** Rejected: the limit depends on the mode.
- **Store the mode in a project file.** Rejected: a file a worker can write must never choose it.
- **Put the user's rules first in the chain.** Rejected: a later hook's rewrite would go
  unchecked.
- **Detect a container for the root check.** Rejected: a false positive lifts the refusal on a
  real machine.
- **A new panel for requests and the plan.** Rejected: both live in the orchestrator box, and the
  plan's reach is shown on the graph.

## Consequences

- `--may-grant P` now lets P run at once, with no shortfall round trip.
- A run started from the box may wait indefinitely on a permission request; it does not end on
  one.
- The allow set and the rules are edited in `permissions.toml`; there is no editor for them yet.
  Tools are not grantable to one attempt, and no attempt can turn the network on.
- A mode change made while no run is in flight is kept in the user file but not in any
  project's change log.

## Amendment: Bypass writes the git directory and kGraf's settings, behind a snapshot and a check

Point 4 left a worker's write to `.git` and `.kgraf` refused in Bypass. Bypass now allows the
git directory and kGraf's settings, as Claude Code's bypass allows its protected paths, with a
snapshot and an integrity check around every such write. kGraf's own books stay refused.

- **The split.** In Bypass only, a worker may write:
  - `.git/<path>`, resolved as git resolves it (`git rev-parse --git-path`, against the admin
    directory recorded when the worktree was made, never the gitlink file on disk): `HEAD` and
    `index` land in the worktree's admin directory, `config`, `info/exclude` and `refs/...` in
    the shared repository. Through `apply_patch` (a patch holding only protected hunks) and
    through the shell, whose sandbox gets the shared git directory as an extra writable root.
  - `.kgraf/run.json` and `.kgraf/worker.json`, kGraf's settings, through `apply_patch` only
    (the state directory holds every worker's worktree, so it is never a sandbox writable root).

  Refused in every mode, Bypass included: the spend ledger, the trace, run records, worker
  transcripts, the graph database, the schedule, the persisted allow rules, the worktrees and
  the rest of `.kgraf`; `.git` itself and `.git/objects` (objects are not snapshotted, so a
  faulty write there could not be restored); `.gitattributes`, `.gitmodules`, `.githooks`,
  `.gitconfig` and a nested `.git`; any path that leaves the worktree. Every other mode refuses
  every protected path as before. The deletion limits of point 4 are unchanged.
- **Snapshot,** before the first write to an area in an attempt and again after each passing
  check: for git, copies of the shared `config`, `HEAD`, `packed-refs`, `shallow`, every file
  under `refs/` and `info/`, each worktree admin directory's `HEAD`, `gitdir` and `commondir`,
  and this worktree's `index`, at most 32 MiB in total (over it, no protected git write is made,
  and the tool says why); for a settings file, its bytes or its absence, at most 1 MiB. A
  `git bundle` copies objects, which are append-only and far larger, and holds neither config
  nor the index, so it was not used.
- **Check,** right after each protected write and after each Bypass shell command that changed
  a snapshotted file, before the call returns: the config parses, `git for-each-ref` reports no
  broken ref, each ref and `HEAD` names an existing object or is symbolic, the worktree's `HEAD`
  resolves to a commit and its index reads. When the attempt ends, `git fsck
  --connectivity-only` over the shared repository is added; the run's branches and integration
  view are refs and worktrees of that repository, so the check covers them. A settings file is
  checked by the parser the host loads it with, which also refuses a permission key. A
  problem the per-write check already found before the attempt's first git write is the
  repository's own and is not counted against a write. A patch that deletes a protected path
  is refused, as a deletion inside `.git` asks in every mode when the shell does it.
- **On a failed check,** the snapshot is restored (first the changed files that are invalid on
  their own, so a ref another worker's merge moved meanwhile is kept, then every changed file if
  the check still fails), the call returns the check's output, and the worker's result is
  failed with it, so the attempt is never merged. A passing write is kept. Every write is
  recorded with its worker, path, mode and outcome (`kept`, or `restored:` with the check's
  output) in the trace and in the graph's permission events.
- **Limits.** Objects are not restored; a check that finds a corrupt object fails the attempt
  and says so. Another worker's git operation that runs during this worker's shell command is
  not serialized against it. `kgraf worker run`, outside an orchestrated attempt, refuses every
  protected path.
