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

## Amendment: object restore and a repository lock for Bypass git writes

The amendment above left two limits: destroyed objects were detected but not restored, and a
worker's git writes were not serialized against other workers' or kGraf's own. Both are closed.

- **Git windows.** A *window* is a protected git write through `apply_patch`, or a Bypass shell
  command with the shared git directory writable, together with the check and the restore that
  follow it. In Bypass, a shell command opens a window only when one of its words contains
  `git` (any case: the program, a `.git` path, a script that runs it) or the shared git
  directory's path. Every other command runs with the git directory read-only, as in every other
  mode, and takes no lock; a git write it makes anyway fails with the sandbox's permission error,
  and a failed command's output says why. Holding a repository lock for every command would
  queue all workers' builds and tests behind each other and make every merge wait out a build.
- **The repository lock.** Each window holds the repository's lock exclusively, from its
  snapshot through its check and restore. Each git process kGraf runs on the repository holds it
  shared: worktree add, remove, prune and reopen, the integration branch, merges (`merge-tree`,
  `commit-tree`, the branch compare-and-swap, `gc --auto`, attempt refs) and the integration view.
  Shared holders run together; they wait only while a window is open, and a window waits until
  none runs. The lock is taken per git process, never across a multi-step operation and never
  nested in one thread.
  - *Where.* An advisory `flock`. On Unix it is taken on the shared git directory itself, not on
    a file in it: a window's sandbox may write inside that directory and could delete a lock file
    there, while the directory itself cannot be removed or renamed from inside the sandbox. On
    Windows, which cannot lock a directory, it is `kgraf-repo.lock` in that directory, opened
    without delete sharing. A host-side mutex alone does not reach a second kGraf process on the
    same repository; the flock does.
  - *Stale rule.* None to judge: the kernel releases a flock when its holder's descriptors close,
    which a crash or a kill does. Nothing is written to disk for the lock. A holder that hangs
    while alive is not stale; its waiters time out.
  - *Waits.* At most 15 minutes by default (longer than the shell's 10-minute command timeout). A
    wait past it fails loud and names the holders in this process, or says another process holds
    it: a protected write is not made, a shell command that names git is not run, a kGraf git
    process fails with the error, and an end-of-attempt check that cannot take the lock fails the
    attempt.
  - *Exact attribution.* No other writer runs inside a window, so its snapshot and baseline are
    taken when it opens, and every change at its end is its own; a window's changed files are
    recorded as its write. A shell window dropped without its check is checked at the next window
    or at the end of the attempt, and that failure says the attribution is no longer exact.
- **The object snapshot.** At the attempt's first shell window, under the lock: every regular
  file under `objects/` (loose objects, packs with their `.idx`, `.rev`, `.bitmap` and the rest,
  `info/*`) is hard-linked into `.kgraf/git-snapshots/` in the project's state directory, which
  is never a sandbox writable root. Git never changes an object file once written, so a link is
  a snapshot that copies no data. Each later shell window brings it up to date. Where a hard link
  fails (the state directory on another file system) files are copied instead, hashed with BLAKE3
  while copied, at most 512 MiB in all; a store over the bound gets no shell git window, and the
  command says why. An owner record beside the files lets the next snapshot remove one a crashed
  process left. The snapshot is deleted when the attempt ends.
- **Check and restore.** At the end of a shell window, an object file the snapshot holds that is
  missing, or whose length, time or inode differs, is lost; when any is, the check adds
  `git fsck --connectivity-only`, so a `git gc` that packed loose objects passes and a deletion
  does not. On a failed check the lost files are put back first (a linked file only if the shared
  file still has its recorded length, time and inode; a copied one only if its hash matches), then
  the ref, config and index files as before. Object files that appeared since the snapshot are
  never removed: objects are additive. The attempt is failed even when the restore makes the
  repository whole: the lock rules out any other writer, so the damage was the attempt's own, and
  a repaired repository does not make the attempt's work trustworthy.
- **Cost,** measured on macOS (APFS) with 20,000 loose objects: taking the snapshot 5.7 s, the
  loss scan 0.12 s, bringing it up to date 0.10 s, removing it 1.0 s; paid once per attempt at its
  first shell git window. Git's automatic gc packs loose objects at 6,700, and a pack is one link.
- **Not taken.** Copy-on-write clones are independent of in-place edits but platform-specific
  and absent on ext4. Hashing every linked file reads the whole store. fsck after every window
  runs only when an object file was lost.
- **Limits.** An object file altered in place under the hard-link method (git writes them
  read-only, so this takes a `chmod`) altered the snapshot too and is reported as not restorable.
  The end-of-attempt fsck counts a problem against the attempt when its first window did not see
  it, including one another worker's window left in between. On a network file system without
  flock support the lock may not exclude another machine. A git write by a command that does not
  name git is refused by the sandbox, not checked.

## Amendment: kGraf's own git runs nothing a repository selects, and Bypass writes `.gitattributes`, `.gitmodules` and `.githooks`

This narrows the Bypass refusal list of the first amendment above (Bypass writes the git
directory and kGraf's settings).

**1. One hardened runner for every git kGraf runs on the host** (`kgraf_core::host_git`).
kGraf runs git outside the worker sandbox to create worktrees, commit and merge attempts, read
status and history, and check protected writes. Each call site used to carry its own partial
overrides (hooks and fsmonitor at best), so a filter or merge driver selected by attributes and
defined in the config, a config-defined hook, an inherited `GIT_*` variable or a repository's
`protocol.file.allow` reached kGraf's own git.

- **Fixed overrides on every call:** `core.hooksPath` at the null device (`/dev/null`; `NUL`
  on Windows), `core.fsmonitor=false`, `core.editor` and `sequence.editor` set to `:` (git
  never launches it), `credential.helper` and `core.askPass` emptied, `protocol.allow=never`
  and `GIT_ALLOW_PROTOCOL=none` (the variable outranks a repository's
  `protocol.<name>.allow`, which `protocol.allow` does not), `submodule.recurse=false`,
  `fetch.recurseSubmodules=false`, `push.recurseSubmodules=no`,
  `status.submoduleSummary=false`, `commit.gpgSign`, `tag.gpgSign`,
  `merge.verifySignatures` and `log.showSignature` false (they run `gpg.program`),
  `diff.external` emptied, `gc.auto=0` and `maintenance.auto=false`.
- **Per-repository overrides:** git cannot disable drivers or config hooks by kind, only by
  name, so before each command the effective config is listed (`git config --includes -z
  --list`, same location and environment) and every name found is neutralized:
  `filter.<name>.clean`, `.smudge` and `.process` emptied and `.required=false` (git stores
  and checks out the bytes unchanged); `merge.<name>.driver` emptied (git cannot run it, and
  a path changed on both sides is a conflict, never resolved); `diff.<name>.textconv` and
  `.command` emptied; `hook.<name>.enabled=false` (config-defined hooks, which git 2.54 runs
  even with `core.hooksPath` at the null device: measured); `tar.<name>.command` emptied. An
  empty command is never run; git fails with "cannot run" instead.
- **Per-subcommand flags:** `diff`, `log`, `show` and `whatchanged` get `--no-ext-diff
  --no-textconv`; `blame` and `annotate` get `--no-textconv`.
- **Environment:** every inherited `GIT_*` variable is removed except the commit identity
  (`GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_COMMITTER_NAME`, `GIT_COMMITTER_EMAIL`) and
  `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`; `GIT_TERMINAL_PROMPT=0`, `GIT_NO_LAZY_FETCH=1`,
  `GIT_MERGE_AUTOEDIT=no`, `LC_ALL=C` and `--no-pager` are set.
- **How they travel:** as `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>`, not
  `-c`: `-c` splits at the first `=`, so a driver named `a=b` would be misread and left
  active. The variables also reach the git processes git starts itself (`worktree add` runs
  a checkout). They need git 2.31; an older git is refused rather than run without them.
- **Kept, so results stay byte-correct:** the user's global and system config and attributes,
  so the commit identity, `core.autocrlf`, `text`, `eol`, `ident` and
  `working-tree-encoding` behave as in the user's own git (they are built in, not programs).
  Merge-back keeps its own `GIT_CONFIG_NOSYSTEM=1` and its attribute source pinned to the
  integration tip. Not set by the runner: `GIT_CONFIG_NOSYSTEM` (the Windows system config
  carries `core.autocrlf`; its drivers are neutralized like any other) and
  `GIT_ATTR_NOSYSTEM` (attributes only select drivers, and every configured driver is
  neutralized).
- **Consequence:** a path whose attributes select a configured filter (git-lfs, git-crypt) is
  stored and checked out by kGraf in its committed form, not filtered, and a path whose merge
  driver is configured conflicts when both sides change it. The user's own git is untouched
  and still honors every hook, filter and driver.
- **Covered call sites:** `kgraf_sandbox::git::Git::run` (worktree add, remove, list and
  repair, the integration view, `kgraf worker`'s attempt reads, the check runner, the state
  directory move), merge-back's `SafeGit` (commit, merge, fsck, gc), the subagent and
  speculation snapshot runner in `kgraf-host`'s workspaces, the worker's status read
  (`kgraf_runtime::worktree`), the co-change history read (`kgraf_graph::cochange`), the
  protected-write checks (`kgraf_tools::protected`, including the end-of-attempt `git fsck`),
  and the brief benchmark's fixture helpers. Not covered, by design: the worker's own `git`
  inside the sandbox (its subject rules and `-c` overrides are unchanged), and cargo's own
  git for dependency fetches (decision 0087).
- **With the repository lock** (the amendment above). Every call site above that runs git on a
  repository also holds the repository's lock shared for that one process: the sandbox's `Git`
  runner, merge-back's runner, the host's snapshot and view runner, the worker's status read,
  the co-change history read and the brief benchmark's fixture helpers. The lock module lives in
  `kgraf_core::repo_lock` (re-exported as `kgraf_sandbox::repo_lock`), so `kgraf-runtime` and
  `kgraf-graph` take it without depending on `kgraf-sandbox`. The lock is taken before
  `host_git::command` builds the invocation, so the config listing runs inside the same shared
  hold as the command it prepares; still one hold per git process, never nested. The
  protected-write checks run inside their window's exclusive hold, so their listing does too.
- **Measured cost:** the config listing is one more git process per call, about 6 ms on the
  development machine (200 calls each of `git config --list` and `git rev-parse HEAD`, both
  about 5.8 ms). The version check runs once per git executable.
- **No gap between the listing and the command.** They are two processes, but both run inside
  one hold of the repository lock: a Bypass shell command that can write the config runs only in
  an exclusive window, which cannot open while the shared hold lasts, so the config the listing
  read is the config the command reads.
- **Known limit:** Windows behavior of the null hooks path is not run.

**2. Bypass writes the three tracked git files.**

- **What changes.** In Bypass only, a worker may write `.gitattributes` (at any depth),
  `.gitmodules` and anything under `.githooks` (`kgraf_core::paths::is_bypass_git_file`; an
  NTFS short name or a stream suffix of them stays refused, as do `.gitconfig`, a nested
  `.git` and kGraf's state). They are ordinary tracked files in the worktree, so:
  - `apply_patch` writes them in the worktree (`kgraf_tools::protected::Area::Tracked`), with
    ordinary files in the same patch, deletions and moves included, under the write guard
    that still refuses a link that resolves into `.git`. Each write is recorded as a
    protected write (`kept`).
  - The declared write scope applies to them like any file: the worker's own scope check,
    the orchestrator's check of the attempt's changes, and the verifier's scope check. A plan
    may declare them while the mode is Bypass (`validate_plan_with`); a file-scope shortfall
    for one is grantable in Bypass.
  - Merge-back commits and merges them as plain data
    (`kgraf_sandbox::Attempt::allow_bypass_git_files`, set by the verifier when the mode is
    Bypass), the hook's executable bit kept; the subagent checkpoint, result and speculation
    snapshots let them through on the same condition.
  - In every other mode they are refused, at the tool, the scope checks and merge-back, as
    before.
- **No snapshot or check.** Git history keeps every earlier version of a tracked file, so a
  faulty write is a normal diff the verifier and the user see; the `.git` snapshot exists
  because the git directory has no history.
- **Why this is safe for kGraf itself.** kGraf's own git runs nothing these files select
  (the host runner above), and merge-back still reads attributes only from the integration
  tip's tree, never a worker's. A landed `.gitattributes` becomes that tree for later
  attempts: its builtin attributes (`text`, `eol`, `merge=union`, `binary`) then apply to
  kGraf's merges as they do to the user's own; programs never run.
- **What the user's own git does.** It honors them: a landed `.githooks/pre-commit` runs at
  the user's next commit when `core.hooksPath` is `.githooks`, and a landed `.gitmodules` is
  read by the user's `git submodule` commands. This is the same as accepting any other change
  from a worker, and is why the mode is Bypass.

Evidence: mechanism tests in `kgraf-core` (an armed repository where plain git runs every
planted program, then kGraf's commit, checkout, merge, worktree add, diff, log, blame, status
and fsck leave no marker, bytes unchanged, merges byte-correct, transports refused),
`kgraf-sandbox` (the same through the real worktree manager and merge-back, and the allowance
merging the three files as data), `kgraf-tools` (the tool path in each mode), and `kgraf-host`
(a hosted run with a scripted model: declared files merged in Bypass, undeclared ones held to
the scope, another mode refusing them). Mutation checks remove the hooks override, the driver
overrides, the verifier's allowance and the worker's scope check; each makes a test fail.

## Amendment: every Bypass shell command writes the git directory; only real changes are checked

This replaces the word rule and the command-long exclusive lock of the amendment above. The word
rule missed a script that runs git, and the lock queued parallel workers' git commands behind
each other.

- **Which commands.** Every shell command in Bypass gets the shared git directory as a sandbox
  writable root. Outside Bypass it stays read-only.
- **No lock while a command runs.** A shell window takes no repository lock before or during its
  command, so parallel workers' commands run at the same time. kGraf's own git processes keep the
  shared lock per process, and a protected write through `apply_patch` keeps its exclusive
  window. The exclusive lock is taken only for the check after a command that changed
  something.
- **Pre-state,** before each command, without a lock: the git files a restore needs (the shared
  `config`, `HEAD`, `packed-refs`, `shallow`, every file under `refs/` and `info/`, each worktree
  admin directory's `HEAD`, `gitdir` and `commondir`, this worktree's `index`) with their stamps
  (length, modification and change times, inode) and bytes, cached and read again only when a
  stamp differs or was not settled (both times older than the read by 3 s, the rule git applies
  to its index, since a coarse clock may not show a second change within one tick); and the
  object snapshot, brought up to date per directory, reading only directories whose stamp
  changed, are new or gone, or are not settled. Git's transient names (`tmp_*`, `*.lock`) are not
  snapshotted. The cheap repository check runs once per attempt, at its first command.
- **Change detection,** after the command, without a lock: a git file whose bytes differ, a file
  added or removed, or a snapshotted object file missing or replaced in a changed directory. No
  change: nothing more, no lock, nothing recorded.
- **Check,** on a change, under the exclusive lock: the checks and the restore of the amendments
  above, from the window's own pre-state. A changed file exactly as a check that passed during
  this command left it was already checked, and is neither checked nor recorded again. When the
  attempt ends, every object file is compared with the snapshot once, which also finds a file
  altered in place, and the fsck runs when anything changed.
- **Attribution,** from a per-repository registry of the open windows in the kGraf process:
  - A check that finds a change names every other window still open whose pre-state differs from
    what the check found in the changed files (every open window when object files were lost):
    those commands started before the change was there. A command that ended before the change
    was found is not named: its own check ran after it ended.
  - A passing check that names others records `kept; changed while another worker's command
    also ran; cause not exclusive (...)`.
  - On damage, the check marks each named window before anything is restored, then restores. Its
    own attempt fails, and each marked window's attempt fails when its command ends, whatever its
    own check found, recorded `restored by another worker's check: ...` with the same phrase: a
    restore can erase the trace of the write that caused the damage. So when two commands overlap
    and either damages the repository, both attempts fail and both records say the cause is
    shared. A window whose pre-state already held the damaged bytes is not marked.
  - A changed command whose check cannot take the lock in time fails its attempt, recorded
    `unchecked: <error>`. A window dropped without its check is checked at the attempt's next
    window or at its end.
- **Cost,** measured on macOS (APFS), release build: per command that changes nothing, 0.26 ms on
  a small repository and 15 ms with 2,000 loose refs and 6,000 loose objects. A whole sandboxed
  shell call: 17.0 ms without the guard, 18.5 to 18.9 ms with it. The attempt's first command pays
  the object snapshot: 25 ms on a small repository, 2.2 s with 6,000 loose objects.
- **Not taken.** A per-command manifest of every object file (0.12 s per scan at 20,000 loose
  objects, twice per command). A per-command exclusive lock. A shared lock held by each command:
  a check would wait out every parallel build, and it would not tell who wrote what.
- **Limits.** When commands overlap, a change is attributed to all that may have made it, not to
  the one that did. A change kGraf's own git work makes while a command runs (a merge moving the
  integration branch) is seen by that command's check: checked and, when valid, recorded against
  it. The last restore step (every changed file) can put back a ref another command moved validly
  meanwhile; it runs only when restoring the invalid files did not make the check pass. An object
  file altered in place in a settled directory is found when the attempt ends. Commands of a
  second kGraf process on the same repository are serialized at their checks but not named in
  attribution. The first command of every Bypass attempt pays the object snapshot.

**Correction (the two amendments above interact).** The hardening amendment says there is no gap
between kGraf's configuration listing and the git command it prepares, because a Bypass shell
command that can write the configuration ran only in an exclusive window. Since every Bypass shell
command now runs without the lock, that premise no longer holds: a Bypass command can change the
repository configuration between the listing and the command, so a filter, driver or hook it adds
in that interval could run in kGraf's own git. The gap is open until kGraf's git stops depending on
a listing taken in a separate process.

## Amendment: a Bypass command's sandbox keeps the git configuration read-only

This closes the gap the Correction above names. Mechanisms:
`kgraf_sandbox::SandboxPolicy::with_frozen_git_config` (`FROZEN_GIT_FILES`),
`kgraf_core::host_git::IncludeScan`, `kgraf_tools::protected::ShellWindow::frozen_files` and the
shell tool.

- **What is frozen.** Inside the shared git directory a Bypass command may write: `config`,
  `config.worktree`, `info/attributes`, each worktree admin directory's `commondir` and
  `config.worktree`, and the lock file git writes each through; every include target
  (`include.path` and `includeIf.<condition>.path`, whatever the condition, found recursively,
  existing or not) that lies under a sandbox writable root; and every directory above each of
  them, below its root (`info`, `worktrees`, `worktrees/<name>`, an include target's
  directories). Frozen means no write, creation, deletion, rename or link. Everything else in the
  git directory stays writable: refs, objects, logs, `HEAD`, the index, `packed-refs`,
  `info/exclude`, `hooks`.
- **Why each.** `config` and `config.worktree` define drivers and config hooks. `info/attributes`
  selects drivers. `commondir` names the directory whose `config` git reads, so pointing it at a
  planted repository replaces the whole configuration. An include target adds configuration, and
  git skips a missing one silently, so creating it later counts. A directory above a frozen file
  is frozen because Seatbelt checks a directory rename against the directory's own path, not the
  files inside: renaming `worktrees/<name>` away and moving in a copy built elsewhere would swap
  `commondir` without writing it (measured).
- **Why this closes the gap.** kGraf's git lists the configuration and runs the command it
  prepared inside one shared hold of the repository lock. With these files frozen, the only other
  writers are a protected write through `apply_patch`, which holds the lock exclusively, and the
  user. The listing therefore reads the configuration the command reads.
- **Not frozen: `hooks`.** kGraf's git sets `core.hooksPath` to the null device, so the
  repository's hooks directory never runs for kGraf; a worker's `pre-commit install` keeps
  working.
- **What a Bypass command can no longer do.** Write the repository configuration: `git config`,
  `git remote add`, `git branch --set-upstream-to`, a tracking `checkout -b`, the config part of
  `git branch -d` and `-m`, and `git worktree add`, `move`, `remove` and `prune`. Such a command
  fails with git's own message, and its output gets a note saying the write goes through
  `apply_patch` as `.git/config`, which checks it under the exclusive lock.
- **Platforms.**
  - macOS (Seatbelt): enforced. Each frozen path is an anchored `regex` in a `deny file-write*`
    after the writable-root allow (measured: write, append, create, rename onto, rename away,
    unlink, hard link, chmod and write through a symlink all refused, writes beside them
    allowed). On a case-insensitive volume a name spelled in another case is refused too,
    existing or not: Seatbelt compares names the way the volume does (measured).
  - Linux (Landlock grants whole hierarchies; a read-only bind mount over a file cannot stop the
    directory above it being renamed away) and Windows (a deny ACE on a file is bypassed by
    `FILE_DELETE_CHILD` on its parent, and nothing is verified there):
    `IsolationStrength::git_config_freezable` is false. A Bypass shell command there runs with
    the git directory read-only and says why; git directory writes go through `apply_patch`. A
    policy that asks for the freeze is refused (`SandboxError::PolicyNotEnforceable`) rather than
    run without it. This narrows the previous amendment ("every Bypass shell command writes the
    git directory") on Linux and Windows: degraded and said, never silently open.
- **The include scan.** One hardened git process (`git config --includes --show-origin -z
  --list`, with the fixed overrides of `host_git`) per scan, plus one per include target git did
  not read itself (missing, or behind a condition that did not hold for the shared directory).
  Cached per attempt and read again only when a file it read changed (length, times, inode) or
  was written within 3 s, the racy margin the git-file pre-state uses. A scan that fails leaves
  the git directory read-only for that command, with the reason.
- **Measured** (macOS, debug build, median of 30 whole sandboxed shell calls): 15.0 ms without the
  guard; 17.7 ms with it while the configuration files are settled (cached scan); 22.1 ms while
  they are within the racy margin (a scan on every call).
- **Options considered.**
  - (a) kGraf's git reads a configuration snapshot it controls. No such mechanism exists: git
    reads `$GIT_COMMON_DIR/config` whenever it finds a repository, and there is no
    `GIT_CONFIG_NOLOCAL`. `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT` and `--config-env` add
    command-scope entries above the repository configuration and do not remove it; `includeIf`
    adds files; `extensions.worktreeConfig` adds `config.worktree`. Pointing `GIT_DIR` or
    `GIT_COMMON_DIR` at a kGraf-owned directory moves the refs and `packed-refs` with it.
    Rejected: not possible.
  - (b) A configuration hash and time checked right before the exec and after the exit, failing
    and quarantining the run on a change. The window between the check and git's own read stays,
    git reads the configuration again in processes it starts (`worktree add` runs a checkout),
    and a check after the exit sees a program after it ran. Detection, not prevention. Rejected.
  - (c) Freeze the files in the sandbox. Chosen: it prevents the write instead of detecting it,
    and the only cost is that configuration writes take the `apply_patch` path.
  - Also rejected: an exclusive lock for each Bypass command (the queueing the previous amendment
    removed), and neutralizing drivers by attribute (config hooks need no attribute, and
    `info/attributes` is in the git directory).
- **Limits.** A Bypass command cannot change the repository configuration through the shell.
  Linux and Windows Bypass shells lose git directory writes. The scan does not see a global or
  system configuration file created during a run (such a file lies outside every writable root).
  `objects/info/alternates` is not frozen: it names object directories, not programs.

Evidence: `kgraf-sandbox` (`macos.rs`: the profile denies every frozen path and its directories,
an include target under a writable root, none outside one, after the writable-root allow;
`tests/frozen_git_macos.rs`, through the real Seatbelt: 21 ways to change the config, a
per-worktree config, a `commondir`, the info attributes or an include target are refused with
EPERM and the files are unchanged; a replacement admin directory cannot be moved in; refs,
`info/exclude` and a commit inside the sandbox still work, and `git config` fails; without the
freeze the same `git config` lands). `kgraf-core` (`host_git/includes.rs`: includes found
whatever their condition, relative and nested, a missing target listed, the cache bypassed while
racy and refreshed on a change). `kgraf-tools` (`shell.rs`: kGraf lists the configuration for a
merge, a Bypass command then writes a merge driver into the config through the real sandbox,
then kGraf's merge runs, and the driver never runs, with a control showing plain git runs it; the
same race run live, a command writing and removing the driver 60 times while kGraf runs 30
merges, never runs it; a frozen config write's output names `apply_patch`; on Linux and Windows
a Bypass command keeps the git directory read-only and says why). `kgraf-host`: the hosted
corrupted-ref and deleted-object tests run on macOS only, where a shell command still writes the
git directory. Mutation checks: with the freeze rules not emitted, both race tests and three of
the four Seatbelt tests fail; with the directories above a frozen file not frozen, the
replacement-directory and config tests fail. Not run: the Linux and Windows paths (no cross
toolchain with a C sysroot on the development machine); their code refuses the freeze and
degrades as described.

## Amendment: a call a skill refuses is not put to the user

The approval chain checks the run's policies (a skill's `allowed-tools`, decision 0053) not
only after the last hook but also when a hook asks or denies. A call a policy refuses is
never put to the user: it is denied by that policy, and the reason names the hook that
would have asked or that denied it. A call the policies permit is asked about or denied
exactly as before. The permission mode is unchanged; only which refusal is reported, and
whether a doomed call raises a request, changed.

The Bypass notice is unchanged: `kgraf orchestrate` prints it on stderr at start whenever
the run's mode is Bypass, whether the flag or the user's permissions file set it. A test
runs the binary both ways and checks stderr.

## Amendment (2026-10-01): no grant is made against a deny or an ask rule

Under Bypass with the ask rule `shell(python3*)`, a worker's report was granted `python3`
(`mode:bypass`), and the next worker met the same ask rule, unanswered, again: `decide` read
the mode and the allow set, never the user's rules. The rules hook enforces them on every call
in every mode, so such a grant cannot help, and it skips the question the user asked to be put.

- `Authority` carries the user's rules (`rules`), read from the permission control with the mode.
- Before any mode grants a program or a path, `decide` checks
  `kgraf_tools::permissions::rule_on_grant`: a rule that would reach some call the grant allows
  (a `shell` rule whose pattern's first word is the program, or whose one-word prefix pattern
  matches it; a file rule whose pattern matches the path; a rule with no pattern on the tool).
  A deny rule holds the contract in every mode, naming the rule. An ask rule asks the user when
  a person attends, and with nobody attending holds, naming the rule. Deny wins over ask.
- A request waiting for the user is re-decided the same way after a mode or rule change.

Evidence: `kgraf-tools` (`permissions.rs`: `a_grant_meets_the_rule_its_calls_would_meet`),
`kgraf-orchestrator` (`shortfall.rs`: `an_ask_rule_blocks_a_bypass_grant`, which also checks a
deny rule under Bypass, Orchestrator decides and Auto with the program in the allow set), and
`kgraf-host` (`shortfall_tests.rs`: `bypass_still_honors_a_deny_rule_through_a_hosted_run`).
