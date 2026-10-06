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

## Amendment (2026-10-01): a permission event states the final decision

The trace said `granted python3 to <worker> (Bypass, by mode)` for a worker whose every
`python3` call the user's deny rule `shell(python3*)` refused. The mode's grant was recorded at
the shell policy hook, before the rules hook (last in the chain) refused the call.

- The shell policy hook and the mode resolver now stage a mode's grant for the call
  (`PermissionControl::stage_grant`, keyed by worker and call id) instead of recording it.
- The approval chain settles the call at its final verdict (`ApprovalChain::with_permissions`,
  `PermissionControl::settle_call`): a call that runs records the grant as before (once per
  worker, subject and mode); a call refused by a later hook records one
  `PermissionEvent::GrantRefused` (worker, subject, mode, who granted, refusing hook, reason),
  once per worker, subject, hook and reason. The run trace names it `grant_refused`.
- A call that is still waiting on an ask settles when the answer arrives.

Evidence: `kgraf-tools/src/approval.rs` tests
(`a_grant_a_later_deny_rule_refuses_is_traced_as_refused_and_never_as_granted`,
`a_grant_the_chain_lets_run_is_recorded_as_granted`,
`the_shell_hook_stages_a_mode_grant_and_does_not_record_it`).

## Amendment (2026-10-02): a refusal names the program a rule matched

A call `["sh", "-c", "mkdir x && python3 -c pass"]` refused by the deny rule `shell(python3*)`
was recorded as a refusal of `sh`: the trace said `refused sh`, and the unverified line said the
worker "finished after a policy refused `sh` and never ran it", though `sh` ran fine. The rule
matcher already judged each command of a script; the record named only the call's own program.

- `refused_program` (`kgraf-tools`, `permissions.rs`) matches the rule a refusal quotes (or, for
  the deletion limit, the first deleting command) against the call's command lines in the order
  the rule matcher uses them: the whole call first, then each command of the script. The
  program of the command that matched is what was refused. A rule on the shell itself, or a plain
  call, still names its own program.
- The worker records that program as the refusal's capability (`RefusedCall`, the refusal
  directory, the stop note, the unverified line), and the permission trace's `grant_refused`
  event names it as its subject (`PermissionControl::settle_call_naming`). This covers the deny
  rule, the ask rule (also when nobody attends and the ask is denied), and the deletion limit.
- A call that ran without error clears the refusals of every program its script ran
  (`programs_run`), not only the shell's: a later `sh -c "... && python3 ..."` that succeeded
  retried `python3`, and one that never ran `python3` did not.

Evidence: `kgraf-tools` `a_refusal_names_the_program_of_the_command_a_rule_matched`,
`programs_run_lists_every_command_of_a_script`,
`a_refusal_of_a_wrapped_command_names_that_commands_program` (`approval.rs`);
`kgraf-runtime` `a_wrapped_refusal_is_recorded_against_the_refused_program`.

## Amendment (2026-10-02): a warning when your own command runs a program your rules block

kGraf runs the user's own verify command (`--verify`) and configured gate commands itself,
through the gate runner, outside the approval chain. The user's deny and ask rules bind
workers; they do not bind these commands. A user who denied `shell(python3*)` and passed
`--verify "python3 check.py"` may expect the rule to cover the command.

- **A warning, off by default.** With the user setting `warn_rule_conflicts` on (Settings >
  General, the user-level `settings.toml`, or `--warn-rule-conflicts` and
  `--no-warn-rule-conflicts` on `orchestrate` and `schedule run-now`, or a run.json key), a
  run's start splits each user command as a worker's command is split (`sh -c`, `&&`, `;`,
  `|`) and matches each part with the approval chain's own rule matcher (`hard_limit`). For
  each program a deny or ask rule matches it prints one line, `notice     ` then the
  sentence, and records the same sentence as a trace notice. When off, nothing is printed or
  traced.
- **Fixed wording.** "Your verify command runs python3 and your rules block python3 for
  workers. kGraf will continue to block workers from accessing python3 and will run your
  verify command directly, without AI." The program is substituted; a gate command reads
  "gate" for "verify".
- **No change of behavior.** The command runs, the rule keeps binding workers, the run never
  stops for the warning. The deletion limit is not a rule and is not reported.
- **Where.** `kgraf-host/src/rule_conflicts.rs`; `Notice::RuleConflict`;
  `Orchestrator::with_start_notices` traces the sentences when a fresh run opens (not on a
  resume). The setting rides in `UserLimits` with the run limits (decision 0017's 2026-10-01
  amendment): same file, same flag-then-setting-then-default order.

Evidence: `kgraf-host` `rule_conflict_tests.rs` (a denied program is said and traced and the
run completes; off says nothing; an uncovered program says nothing; a chained command matches
on its second part) and `rule_conflicts::tests`.

## Amendment (2026-10-02): a tool refusal keeps its tool; a script is granted command by command

**A refusal of the whole tool keeps the tool and its scope.** The amendment above named a
program for any shell refusal whose reason quoted a rule anywhere, from any policy. A skill's
`allowed-tools` refusal of a shell call composed with a rule's ("`user-permission-rules` also
refused it: your deny rule `shell(python3*)` ...") was then recorded as a refusal of `python3`,
and a rule with no pattern (`shell`, `*`) named the call's first program. Neither refused a
program: both refused the `shell` tool.

- `refused_program` is replaced by `refused_subject` (`kgraf-tools`, `permissions.rs`), which
  returns the tool, a program, or a path. A skill's refusal (`SKILLS_POLICY`) is of the tool. A
  user rule (`user-permission-rules`, or the user's denial of its ask) counts only where its text
  begins the reason, as `hard_limit` writes it: a rule with no pattern refuses the tool; a rule on
  `shell` names the program of the command it matched; a rule on a file tool names the path it
  matched. The deletion limit names the deleting command, also only where its text begins the
  reason. The mode's command policy names the command of a script it judged (below). Any other
  refusal keeps the call's own capability.
- `RefusedCall` gains `whole_tool`. A refusal of the whole shell tool is recorded with it set and
  with the program the call ran as its capability, so a report of scope `tool` naming `shell`
  and a report of scope `command` naming the program both ground in it (decision 0093).
- `RefusedCall::skill` reads the skill's name from the refusal the policy writes under
  `skills:allowed-tools`. It read a `skill:<name>` prefix no policy produced, so a hold from the
  recorded refusal (the live skills gone) never named the skill.
- A report of scope `tool` naming a tool the worker's live skills refuse, one of whose calls
  another policy refused this attempt, grounds in that refusal and carries the skill's refusal
  (`ground_report`, `kgraf-orchestrator`). The round's calls are reviewed before any of them
  runs, so a call made in the same round as the skill's activation is judged by the other
  policies only; the skill refuses every later call of the tool, so the contract is held naming
  the skill. Without an active skill, a report of the shell tool when only commands were refused
  is answered with the refusals on record and how a refused command is reported.

**A script is granted command by command.** Claude Code's permission rules document compound
commands this way: "A rule must match each subcommand independently", with `&&`, `||`, `;`,
`|`, `|&`, `&` and newlines as separators, and "Deny and ask rules apply when any subcommand
matches them". kGraf's shell tool runs an argv, so a script arrives as `sh -c SCRIPT`. In Auto
with `mkdir` and `cp` allowed, `["sh", "-c", "mkdir -p out && cp templates/greeting.txt
out/greeting.txt"]` was asked about ("not on the build and test allowlist") because the mode
judged only `argv[0]`, `sh`, and a run nobody attended denied it.

- `script_commands` (`kgraf-tools`, `permissions.rs`) reads `["sh" | "bash" | "zsh" | "dash",
  "-c", SCRIPT]` (bare name, exactly `-c`, nothing after the script) as simple commands joined by
  `;`, `&&`, `||`, `|` or newlines. It refuses to judge, naming why: `$` and backquotes,
  parentheses and braces, `&` and `|&`, input redirection, backslashes, comments, `!`, shell
  keywords, an operator with no command on one side, a command that does not start with a plain
  program name (a path, a quote, a glob, `VAR=value`), and an output redirection whose target is
  not `/dev/null` or a worktree path a worker may write (lexically inside the worktree, no `~` or
  glob, `kgraf_core::paths::forbidden_reason` clear), or any file redirection in a script with a
  `cd`. Its commands must agree with `simple_commands`, which the user's rules match, so a grant
  and a rule never judge different commands. `simple_commands` no longer splits at the `&` of
  `>&` or `<&` (`2>&1`), which separates nothing.
- `ExecPolicy::evaluate`: when no rule names the whole call and the call is not already granted,
  and something grants named programs (Auto with an allow set, Accept edits, or a program granted
  to the contract after a shortfall), a judged script is granted, confined like any granted
  command, when a grant covers the program of every command. A command a `forbid` rule or the
  build allowlist refuses refuses the whole call, naming it ("the script runs `git` (...), which
  is refused: ..."). A command no grant covers leaves the call to be asked, and the question names
  the first such command ("The script runs `curl` (...), which no grant covers ..."); a script
  that is not judged is asked about with the reason. `refused_subject` reads the program from the
  last occurrence of that lead, after the model-written command line the question quotes, so a
  refusal and a later report name it.
- The mode's grants of a script are staged per program (`mkdir`, `cp`), never as `sh`.
- Unchanged: the user's deny and ask rules, whose hook runs last on every command of the script,
  in every mode; Bypass, which grants the call whole; Manual, which asks; persisted allow rules,
  which still never name a shell wrapper. A program granted by its path (`./mkdir`) is not a plain
  program name inside a script, though Accept edits grants a direct call of it by base name.

Evidence: `kgraf-tools` (`permissions.rs`: `a_tool_level_refusal_keeps_the_tool_and_its_scope`,
`a_refusal_names_the_program_of_the_command_a_rule_matched`,
`a_plain_script_is_read_as_its_commands`, `a_script_with_any_other_construct_is_not_judged`;
`execpolicy/policy.rs`: `auto_grants_a_script_when_it_grants_every_command`,
`a_script_with_one_command_no_grant_covers_is_asked_naming_it`,
`a_script_with_substitution_or_an_outside_redirect_is_not_granted`,
`accept_edits_contract_grants_and_forbid_rules_apply_per_command`,
`manual_and_bypass_are_unchanged_for_scripts`; `approval.rs`:
`a_skill_refusal_of_the_shell_tool_is_not_renamed_to_a_program`,
`auto_runs_a_script_whose_every_command_it_allows`,
`a_deny_rule_on_one_command_refuses_a_script_the_mode_grants`; `shell.rs`:
`auto_runs_a_script_of_allowed_programs_confined`, through the macOS sandbox); `kgraf-runtime`
(`a_tool_level_refusal_of_a_shell_call_is_recorded_as_of_the_tool`,
`a_skill_refusal_names_its_skill`); `kgraf-orchestrator`
(`a_skill_refusal_of_the_shell_tool_is_held_naming_the_skill`,
`a_shell_report_after_a_same_round_skill_activation_is_held_naming_the_skill`); `kgraf-host`
(`auto_runs_a_script_of_allowed_programs_and_names_the_one_it_does_not_allow`, a hosted run).

## Amendment (2026-10-02): the user may let a mode grant the script constructs the strict reader refuses

**Why.** The amendment above grants a script command by command only when the strict reader can
judge it. Variables, substitution, subshells, groups, background jobs, input redirects, escapes,
comments, path or `VAR=value` command names, and output redirects outside the worker's files are
ordinary shell; a script using one was always asked about, and denied with nobody attending.
Each construct group is now a user setting, off by default, so the default behavior does
not change.

**The setting.** `ScriptConstruct` (`kgraf-tools`, `permissions.rs`) has six values; the
user-level `permissions.toml` lists those on as `script_constructs = [...]`:

| Key | Settings name | Lets a mode grant |
|---|---|---|
| `variables` | Variables and command substitution: $VAR, $(...), backquotes | `$VAR`, `${VAR}`, `$(...)`, `$((...))`, backquotes, `<(...)`, `>(...)` |
| `subshells` | Subshells and groups: ( ... ), { ...; }, {a,b} | `( ... )`, `{ ...; }`, brace expansion |
| `background` | Background jobs: & | `&`, `\|&` |
| `input_redirects` | Input redirects and heredocs: <, <<, <<< | `<`, `<&N`, `<<` and `<<-` heredocs, `<<<`, `<>` |
| `escapes` | Escapes, comments and other quoting | `\`, `#` comments, `!`, `$'...'`, a command named by a path or a quoted word, `VAR=value` before a command |
| `outside_redirects` | Output redirects outside the worker's files | `>`, `>>`, `>|`, `>&FILE` to any path, after a `cd` too |

Sources and precedence: `kgraf orchestrate --script-constructs LIST` (comma-separated, or `none`)
replaces the file's list for the run; then the file; then all off. The desktop's Settings >
Permissions shows one switch per construct with a one-line explanation under the risk statement
(`SCRIPT_CONSTRUCTS_RISK`), and a change reaches the live `PermissionControl` at once and is
written to the file. `.kgraf/run.json` refuses `script_constructs` by name, as it refuses every
permission key: a worker can write a project file, so a project file never loosens what a
worker may run (this departs from the run-limit pattern, flag > run.json > user setting >
default, for that reason). `kgraf worker` and `kgraf schedule run-now` read the file. Every
command that builds a control prints `permissions mode MODE; script constructs on: LIST`.

**The reading.** `script_commands_with(argv, constructs)` runs the strict reader first; with no
construct on it is `script_commands` exactly. When the strict reader refuses and some construct
is on, a second reader (`RelaxedReader`) reads the script with the strict grammar plus the
constructs on, refusing each construct that is off with the strict reader's reason. It is
recursive over `$(...)`, backquotes, `<(...)`, subshells and groups, and returns every command
whose program it can see, those inside them included. A command named by a variable or a
substitution has no program to check and is left out. Never judged, whatever the switches: shell
keywords, a function definition, a script with no program kGraf can see, a heredoc whose
unquoted text substitutes a command (`$(` or a backquote), a `${...}` holding a quote, an escape
or a substitution, a backquote substitution holding a backslash, an arithmetic expansion holding
a substitution, a glob or brace as a program name, `;;`, a control character, and the call shapes
the strict reader refuses (a shell named by a path, options other than `-c`, arguments after the
script). A redirection target kGraf cannot resolve (`> $OUT`) needs `outside_redirects`. The
strict reader's cross-check against `simple_commands` does not apply to the second reader: that
split does not follow these constructs.

**What a mode does with it.** `ExecPolicy::script_verdict` and `record_mode_grant` read the
constructs from the live control on every evaluation. Where something grants named programs
(Auto with an allow set, Accept edits, a program granted to the contract), a script the second
reader accepts is granted, confined like any granted command, when a grant covers every program
it returned; the grant's reason names the constructs on and says only the visible programs were
checked, and grants are staged per program. A visible program no grant covers is asked about
naming it, as before. Manual without a contract grant still asks; Bypass already grants the call
whole.

**What the user accepts.** kGraf cannot see what a variable or a substitution will run: a command
named by a variable (`$CMD`) runs with no check, the refusal to delete system paths included.
The risk statement says so in the app and in USAGE.

**What still holds.** The user's deny and ask rules match every command `simple_commands` sees,
in every mode. Because a script judged only because of a switch may hide a command from that
split (`{ curl x; }` splits as `{ curl x`), `construct_rule_limit` also matches the user's shell
rules by program name against the script text, for such a script (`relies_on_constructs`): the
base name of a rule's first pattern word (a prefix when the pattern is one word ending in `*`)
matched against every word of the argv split at each character a program name cannot hold, `/`
included. Deliberately broad: `shell(git push*)` matches any such script naming `git`, and a
program named only in an assignment (`C=curl; $C u`) is matched. The rules hook applies it in
every mode but Bypass, deny before ask, after its own deny. The deletion limit's
`program_index` now also skips `{`, `!` and `(`, so `{ rm -rf ~; }` and `! rm -rf /` are refused
like the plain command. The sandbox does not change: `outside_redirects` only lets the mode
grant the script; the workspace sandbox still confines a granted command's writes to the
worktree and its scratch directory, and the verifier still checks the attempt's changes against
the contract's declared writes.

**Alternatives rejected.** A full shell parser crate (`brush-parser`, `conch-parser`,
`yash-syntax`) would read more than "which programs can kGraf see" and would still need that
mapping; the second reader is a small character loop that refuses what it cannot place. Applying
the text match to every script, or with the switches off, would change the default behavior.
Letting `run.json` set the switches would let a worker loosen its own permissions.

Evidence: `kgraf-tools` (`permissions.rs`: `each_script_construct_is_judged_only_with_its_switch_on`,
`some_scripts_are_never_judged_whatever_the_switches`,
`a_rule_on_a_program_inside_a_construct_still_refuses_or_asks`,
`deleting_a_system_path_inside_a_group_is_refused`,
`script_constructs_round_trip_through_the_user_file`; `execpolicy/policy.rs`:
`each_script_construct_switch_lets_auto_grant_a_script_using_it`,
`a_script_granted_by_a_switch_records_each_visible_program`); `kgraf-cli`
(`the_script_constructs_flag_wins_over_the_file_and_the_default_is_none`,
`orchestrate_takes_a_script_constructs_list`); `kgraf-host`
(`the_run_file_cannot_set_the_permission_mode_or_plan_review`); `kgraf-shell`
(`the_script_construct_switches_start_off_and_reach_the_shared_control`, kittest). Each new test
fails with its mechanism reverted.

## Amendment (2026-10-02): grouping and prefix words never hide a program from the rules

**Why.** Two gaps remained after the amendment above. First, a user's deny or ask rule matched a
command line from its first word, so `{ curl x; }` was matched as `{ curl x` and
`deny = ["shell(curl*)"]` did not refuse it in Manual, Bypass or any mode with the switches off.
The same held for `!`, `if`/`then`/`while`/`do`, `time`, `exec`, `command`, `builtin`,
`nohup`, `env`, `VAR=value` before a command, a leading redirection, a shell behind a wrapper
(`env sh -c ...`) and a shell started inside a script. The text match of the amendment above
covered only scripts judged because of a switch. Second, the strict reader looked for a
directory change only as a command's first word, so it judged `command cd x; echo hi > f` as a
plain script and a grant of `command` and `echo` ran it, though the redirection then wrote
relative to `x`.

**The change.** One function, `command_starts` (`kgraf-tools`, `permissions.rs`), lists every
place in a simple command where its program may stand: index 0, then each word after shell
syntax (`{`, `}`, `(`, `)`, `!`, `if`, `then`, `else`, `elif`, `while`, `until`, `do`, `time`,
`coproc`), a wrapper (`sudo`, `doas`, `env`, `command`, `builtin`, `exec`, `nice`, `nohup`,
`time`, `busybox`, `xargs`, `timeout`, `stdbuf`, `setsid`, `ionice`, `chrt`, `taskset`,
`unbuffer`, `caffeinate`, `chroot`, `flock`, `noglob`, `nocorrect`, `watch`, `strace`,
`ltrace`), a wrapper's first operand where it takes one (`timeout 5s`, `chroot DIR`), an option
and the value it may take, a number, a word holding `=`, and a redirection with its target.
Where a word could be an option's value or the program (`sudo -u bob curl`), both places are
listed, and the list goes one word past the first word that is surely not a prefix. It serves:

- the user's rules: a shell call's subjects are now each command line from every listed place
  (`subjects`), the whole call first, so a rule matches the program behind any prefix in every
  mode, with no switch on, Bypass included;
- the script split: `simple_commands` finds a shell (or `eval`) at any listed place and splits
  its script, and splits a shell or `eval` started inside a script the same way, to a depth of
  32;
- the deletion limit: `program_index` is the first listed place naming a deleter, replacing its
  own wrapper list (`if rm -rf /; then ...`, `timeout 5s rm -rf /` and `2>/dev/null rm -rf /` are
  now refused);
- the command policy: a `forbid` rule also refuses its shape from a listed place
  (`env git push --force`). Allow rules still name the exact argv.

Both script readers count a directory changer (`cd`, `pushd`, `popd`) in any word of a command,
through one helper (`names_directory_change`); a redirection to a file in a script with one
leaves it unjudged. The deletion limit follows a directory change behind a prefix word, `pushd`,
`popd`, `cd -P DIR` and a `cd` in a nested shell by adding the place it may lead to the places a
relative operand may resolve against (at most 16), never replacing one; only a plain `cd` as the
first word of the call's own script moves the first place, as before. An operand is judged by
the strictest result over every place.

**No loosening.** Every change adds subjects, places or refusals; none removes one. A test keeps
the pre-change matcher, split, deleter scan and deletion limit as a reference and checks, over a
corpus of the commands earlier tests refuse or ask about plus every hidden shape, that each old
subject is still a subject and that the rules hook is, in every mode, at least as strict as the
old rules and deletion limit together. The rules hook still skips the text match in Bypass; the
rules themselves now see the hidden command there.

**Cost.** Broad by design: a rule may match a word that is an option's value (`shell(bob*)`
matches `sudo -u bob curl`), and `echo cd > f` leaves a script unjudged. Both err toward asking.

**Not read as a command.** A program named as an argument rather than a prefix: `find -exec`,
`su -c`, `ssh HOST CMD`, `script -c`, `trap 'CMD'`, `parallel`, `source FILE`; and a command named
by a variable. A grant still names `argv[0]`: a user who puts a wrapper (`env`, `xargs`) in the
Auto allow set grants what it runs, though the user's rules still match the program behind it.

Evidence: `kgraf-tools` (`permissions_prefix_tests.rs`:
`a_rule_sees_the_program_behind_a_group_or_a_prefix_word_in_every_mode`,
`a_rule_does_not_match_a_program_named_only_as_an_argument`,
`command_starts_skips_syntax_wrappers_assignments_options_and_redirections`,
`simple_commands_follow_a_shell_behind_a_prefix_and_a_nested_shell`,
`the_strict_reader_sees_a_directory_change_behind_a_prefix_word`,
`a_redirect_after_a_hidden_directory_change_is_never_granted_by_a_script_grant`,
`the_deletion_limit_sees_a_deleter_and_a_directory_change_behind_prefix_words`,
`a_forbid_rule_refuses_its_shape_behind_a_prefix_word`,
`no_previously_refused_command_of_the_corpus_is_now_allowed`; `permissions.rs`:
`a_rule_on_a_program_inside_a_construct_still_refuses_or_asks`, which now expects Bypass to
refuse `curl` inside a group); `kgraf-host` (`rule_conflicts.rs`:
`a_program_behind_a_group_or_a_prefix_word_is_found_and_named`). Each regression test fails with
its part of the change reverted.

## Amendment (2026-10-02): a script of build commands, after one `cd` into the worktree, runs as a build

**The gap.** A planner writes a contract's acceptance as `cd pricing_engine && cargo check`, and
a worker runs it as `["sh", "-c", "cd pricing_engine && cargo check"]`. The command policy
judged a script command by command only where a mode or a contract's grant covered named
programs, and only a grant counted: the build and test allowlist (decision 0084) and `cd` did
not. So in Manual, or in Auto with an empty allow set, the script was asked about whole, and a
run nobody attended denied the worker the check its own contract named, though `cargo check`
alone runs unasked. The acceptance and the worker's allowed commands disagreed by construction.
A worker denied `sh -c "cargo test -p discount_flat"` that then ran
`cargo test -p discount_flat` as argv was still reported unverified, because the refusal was
recorded against `sh` and `sh` never ran again.

**The rule.** A script's command is covered by a grant of its program (as before), by the
build and test allowlist's cargo rules, or, once per script, by a `cd` into the worktree:

- `cd DIR` with exactly one operand, a relative path with no `..`, no absolute path, no `~`,
  glob or option, and not the `.git` directory or kGraf's state. A bare `cd`, `cd -`,
  `pushd`, `popd`, a second `cd`, and a `cd` that can leave the worktree are not covered, and
  the question says why.
- A cargo command counts by the allowlist's cargo rules only. An exact allowlist entry (the
  project's configured acceptance or gate command) is not counted inside a script: it is a
  project command line, which may name a file relative to the directory it runs in.
- A script whose commands are all allowlisted cargo commands, with or without the `cd`, is a
  build (`PolicyVerdict::build`). One that needs a grant for any command is granted, and only
  the mode-granted programs are recorded as grants.
- Only the strict script reader's commands get the allowlist and `cd` coverage; a script read
  only because a construct switch is on keeps the grant-only rule.

**Checks where the script runs.** Before an allowed script runs
(`PolicyVerdict::script_checked`), the shell tool checks the `cd` target on disk: a directory
of the worktree, reached without a link that leaves it. For each allowlisted cargo command it
runs decision 0084's checks in the directory the command starts in: the worktree's cargo
configuration and toolchain file unchanged since the worker started, none of them in a
directory between the worktree root and the `cd` target (cargo and rustup read both from the
start directory upward, and those were never recorded), and each `--manifest-path` inside the
worktree, resolved from that directory.

**What does not change.** The user's deny and ask rules match every command of the script in
every mode, through the rules hook, which runs after the command policy. A `forbid` rule or a
cargo argument the allowlist refuses on one command refuses the whole script. Bypass grants a
script whole, as before, and its hard limits stay. Nothing outside the allowlist or a grant runs
unasked after a `cd`.

**The report.** A shell script refused whole (recorded against its shell, with no
command-level match naming another program) records the programs it held
(`RefusedCall::held`, a directory change left out). It counts as retried once each of them
later ran without error, in any form, so the contract is no longer reported unverified when the
worker ran what the refused script held. A refusal a rule narrowed to one program is matched
on that program only, as before.

**Plan intake.** No intake change is needed for this shape: the acceptance form a planner
writes now runs with the same per-command checks at the moment the worker calls it, under the
live mode. A command outside the allowlist named in an acceptance still goes through the
shortfall path and the mode.

The tool description now says the allowlisted cargo commands may be given as a `sh -c` script
of those commands after at most one `cd` into the worktree.

Evidence: `kgraf-tools` (`execpolicy/script_build_tests.rs`:
`a_cd_into_the_worktree_then_allowlisted_cargo_commands_is_a_build`,
`a_cd_never_widens_what_a_script_may_run`,
`a_cd_then_a_granted_program_is_granted_as_its_commands`,
`deny_and_ask_rules_still_apply_inside_a_cd_script`; `shell.rs`:
`a_cd_into_the_worktree_then_an_allowlisted_build_runs`,
`a_cd_script_is_checked_on_disk_where_it_runs`,
`an_allowlisted_command_is_a_sandboxed_build_and_nothing_else_is`, which now expects
`sh -c "cargo build"` to be a build); `kgraf-runtime` (`shortfall.rs`:
`a_script_refused_whole_is_retried_when_its_commands_run`); `kgraf-host`
(`build_allowlist_tests.rs`: `a_cd_into_the_worktree_then_an_allowlisted_build_runs_unattended`).
Each regression test fails with its part of the change reverted.

## Amendment (2026-10-02): a run refused before it starts creates nothing

- **The refusal comes first.** A new run on the command line with plan review on and no
  `--approve-plan` is refused before the project is opened: no `.kgraf`, no `graph.db` or
  `ledger.db`, no `kgraf/<run>` branch, no worktree base, and no `trace` line for a trace
  that is never written. The output is the refusal alone. The command line is never
  attended, so the three facts the rule needs (review on, nobody attending, no approval) are
  known at once. The rule is one function, `plan_review_refusal`, shared with the
  orchestrator's own check, so the early refusal is never wider than the orchestrator's.
- **Not rolled back.** The alternative, creating the branch and state and deleting them on
  refusal, would risk deleting a project's earlier runs' databases and worktrees, which this
  command did not create and cannot tell apart from its own. Refusing first deletes nothing.
- **Unchanged cases.** A reading (`--corpus` without `--approve`) starts no run. A resume is
  judged against its own record, which exists, by the orchestrator. A run built by a host
  that may be attended (the desktop box) is judged by the orchestrator after the build, as before.
- **A missing trace is not a notice.** When a run fails before its first event, the command
  no longer prints "the trace could not be read" for a file that was never written. A trace
  that exists and cannot be read still gets the notice.

## Amendment (2026-10-02): a rule names a tool a worker can have

A rule that names a tool no worker has matches nothing, and nothing said so. `Rule::parse` took
any name: `deny = ["Bash(rm*)"]`, written the way Claude Code spells its shell tool, was
accepted and compared against calls of a tool named `Bash`, which no worker has, so `rm` ran
in every mode while the user believed it was denied. `[allow] tools` had the same gap. The
cause, in `Rule::matches`: `if self.tool != tool && self.tool != "*" { return false; }`, with
`self.tool` whatever the user wrote.

- **Claude Code's names are read as kGraf's tools.** The matching is the same, so an alias
  makes a Claude Code habit protect what it says: `Bash` is `shell` (command text; kGraf also
  matches each command of an `sh -c` script and past wrappers, which Claude Code does not),
  `Edit`, `Write`, `MultiEdit` and `NotebookEdit` are `apply_patch` (Claude Code consults path
  rules on `Edit` only; kGraf applies them for all four, the stricter reading), `Read` is
  `read_file`, `Grep` and `Glob` are `search_files`, `LS` is `list_directory`. Names whose
  semantics differ: a Claude Code `Read` rule also covers its search tools and blocks an edit
  of the same path, and an `Edit` rule covers shell redirections; kGraf's `Read` covers
  `read_file` only, and neither covers a redirection. Claude Code tools kGraf has no
  counterpart for (`WebFetch`, `WebSearch`, `Agent`, `Task` and the rest) are refused. The
  rule keeps its text as written for refusals and the user file.
- **Wildcards follow Claude Code's documentation.** A `*` anywhere stands for any text; a
  trailing ` *` or `:*` that is the only wildcard also matches the bare command (`ls *` and
  `ls:*` match `ls`, not `lsof`; `ls*` matches both). A Claude Code path rule names a
  repository path: relative patterns match at any depth (Claude Code does so for a bare name
  and `dir/**` in deny and ask rules; kGraf for every relative pattern, the stricter reading),
  `/path` and `./path` at the repository root. `mcp__<server>` covers every tool of that
  server, and a deny or ask rule's tool name may be a glob (`mcp__*`). Each of these only
  widens what a rule matched before: the pattern as written still matches as it did (so
  `git:*` still matches `git:x`), and a test checks old matching implies new over a corpus.
- **Any other name is refused where the rule is read.** `--deny`, `--ask`, `permissions.toml`
  and the desktop all read rules through `Rule::parse`, so each refuses `bash(rm*)`,
  `WebFetch` or `write_file(src/*)`, naming the rule, a hint where a kGraf tool does the job
  (`use shell`), and every name a rule may use. So are rules kGraf could never match: a pattern
  on a tool whose calls name no command or path (`graph_slice(x)`), a Claude Code parameter
  rule (`Bash(timeout:30)`), a path outside the repository (`Edit(//etc/x)`, `Read(~/.ssh/*)`),
  and a carve-out (`Edit(!docs/**)`). Refusal, not a notice: the set of built-in names is known
  when the rule is read, and a deny rule that cannot match must not let a run start. The
  command line stops; the desktop runs in Manual, where every request asks, and says why.
- **An MCP name is checked at run start.** A server's tools are known only once it connects,
  so `mcp__<server>__<tool>`, `mcp__<server>` and MCP globs are accepted when read. After the
  route probes connect every configured server, each deny or ask rule and allow-set tool that
  matches none of the workers' tools is said once (`Notice::RuleMatchesNoTool`), printed as a
  `notice` line and traced as a run-start notice; `kgraf worker` prints the same line once its
  servers have listed their tools. A notice, not a refusal: the user file serves every
  project, and a tool this run's workers lack cannot be called in it.
- **Allow-set tools.** `[allow] tools` take the same names; a wildcard is refused unless it
  follows one server's literal `mcp__<server>__`, since Auto would grant every tool it matched.
- **One table.** The skill refusal's hint (`allowed-tools` naming `Bash` suggests `shell`)
  reads the same table (`kgraf_tools::permissions::tool_hint`).
- **The desktop has no rule editor.** Rules are set in `permissions.toml`; the Settings
  Permissions section now says that Claude Code names are read and an unknown name is refused.

Evidence: `kgraf-tools` (`permission_names_tests.rs`:
`a_claude_code_bash_deny_rule_refuses_the_shell_command_in_every_mode`,
`a_bash_rule_and_its_shell_rule_match_the_same_calls`,
`wildcards_match_as_claude_code_documents_them`, `no_rule_matches_less_than_before`,
`a_rule_or_allow_entry_naming_no_tool_is_refused_with_the_known_names`,
`a_pattern_that_can_never_match_is_refused`, `mcp_names_are_accepted_and_cover_their_server`,
`claude_code_path_rules_match_repository_paths`,
`unmatched_entries_name_each_rule_that_names_no_worker_tool`); `kgraf-cli`
(`permissions_cli.rs`: `a_flag_rule_naming_no_tool_is_refused_and_a_claude_code_name_is_read`);
`kgraf-host` (`rule_names_tests.rs`:
`an_mcp_rule_is_checked_against_the_tools_the_servers_list_at_run_start`, with the real MCP
echo server over stdio, and `every_builtin_tool_name_is_one_a_rule_may_name`); `kgraf-shell`
(`permission_bar.rs`:
`a_rule_naming_no_tool_is_refused_in_the_window_and_a_claude_code_name_loads`). Each
regression test fails with its part of the change reverted.

## Amendment (2026-10-05): a Claude Code tool kGraf lacks is a notice; the desktop keeps the valid entries

The amendment of 2026-10-02 (rule names) refused every name outside kGraf's tools, its Claude
Code aliases and MCP names. Two effects followed. A permissions file shared with Claude Code,
naming a tool kGraf does not have (`deny = ["WebFetch"]`), stopped every command-line run. And
the desktop, on any refused entry, dropped every rule: `PermissionUi::load` read the file with
`PermissionSettings::load`, whose `from_toml_str` returned the first refused entry as an error
(`list.iter().map(|r| Rule::parse(r)).collect()`), and the window then ran
`PermissionControl::manual()`, Manual with no rules. Safe, since every request asked, but the
user's valid deny rules, allow set and plan review were gone, and more files now hit it.

- **A Claude Code tool kGraf has no tool for loads with a notice.** A table
  (`kgraf_tools::permissions::CLAUDE_CODE_WITHOUT_COUNTERPART`) lists the tools of Claude
  Code's tools reference (read for Claude Code 2.1.289) that kGraf neither has nor reads as one
  of its own, plus `Task` (the older name of `Agent`, which Claude Code still reads in
  settings), `SlashCommand` and `TaskOutput` (in its changelog; old settings files still hold
  them). Each entry carries a plain sentence: what kGraf uses instead, and the kGraf rule that
  gives the intended effect (`WebFetch`: add `shell(curl*)`, `shell(wget*)` or a rule for the
  MCP tool; `Agent` and `Task`: add `spawn_subagent`; `Skill`: add `skill`). A deny, ask or
  allow-set entry naming one loads, from the file or a flag. It matches no call:
  `tool_name_matches` refuses a table name before any other test, so the entry changes no
  decision in any mode, even for a call of a tool registered under that bare name. Its notice
  (`counterpart_notice`) is said at run start in place of the generic "names no tool" sentence,
  printed as a `notice` line and traced, and shown in Settings. Such a name is a Claude Code
  habit or a shared file, not a typo, so a refusal stopped a run for a tool no worker can call.
  A name in neither table is still refused exactly as before, and the aliases do not change.
- **One per-entry check.** `kgraf_tools::permissions::validate_entry(list, text)` checks one
  entry of one list (`EntryList`: deny, ask, allow tools, allow programs, allow paths) and
  returns the entry as kGraf reads it with its notice, or the reason it is refused. The file
  readers, `AllowSet::with_tools` and `with_programs`, and the `--deny`, `--ask`, `--allow` and
  `--allow-path` flags all use it; `kgraf worker` and `kgraf orchestrate` build their control
  through the same command-line reader, and the desktop's scheduled runs share the desktop's
  control. A settings editor checks an entry with the same function.
- **The command line still refuses.** `PermissionSettings::from_toml_str` refuses a file with
  any refused entry. The error names each one, its list and its reason, and says to correct or
  remove it.
- **The desktop keeps the valid entries.** `PermissionSettings::read_entries` (and
  `load_entries`) reads the file entry by entry into a `SettingsLoad`: the settings with every
  valid entry, each refused entry with its reason (`RefusedLine`), the notices, and
  `forced_manual`. A refused deny or ask entry sets `forced_manual`, with a reason that names
  the entry: an intended block or ask is missing, so the window runs in Manual and the dropdown
  refuses another mode until the file is fixed. Refused allow entries alone keep the user's
  mode; what they would grant asks instead. Settings > Permissions shows the reason, each
  refused entry and each notice. A change made in the window keeps each refused entry as
  written and, while Manual is forced, the user's mode (`SettingsLoad::file_text`), so the
  window never deletes an entry the user must still fix. A file that is not a permissions file
  at all (bad TOML, an unknown key, an unknown mode) keeps the earlier behavior: Manual, no
  rules, and the reason. The window now also refuses to write over such a file.

Evidence: `kgraf-tools` (`permission_names_tests.rs`:
`a_claude_code_tool_kgraf_lacks_loads_with_a_notice_and_changes_no_decision`,
`the_table_of_claude_code_tools_kgraf_lacks_is_consistent`,
`a_refused_entry_is_listed_and_only_a_refused_deny_or_ask_forces_manual`,
`a_rule_or_allow_entry_naming_no_tool_is_refused_with_the_known_names`, typos included);
`kgraf-cli` (`permissions_cli.rs`:
`the_command_line_refuses_a_bad_entry_by_name_and_loads_a_tool_kgraf_lacks`); `kgraf-host`
(`rule_names_tests.rs`: `an_mcp_rule_is_checked_against_the_tools_the_servers_list_at_run_start`,
which now also traces the `WebFetch` notice in a real run); `kgraf-shell` (`permission_bar.rs`:
`a_claude_code_name_loads_and_a_tool_kgraf_lacks_is_a_notice_in_the_window`,
`a_bad_deny_entry_keeps_the_other_rules_and_forces_manual_with_the_reason`,
`a_bad_allow_entry_keeps_the_mode`,
`a_file_whose_mode_cannot_start_falls_back_to_manual_and_says_so`). Each regression test fails
with its part of the change reverted and passes with it restored.

## Amendment (2026-10-06): a program given as an argument is a command, and a grant of a wrapper grants nothing it runs

**Why.** Two gaps remained after the 2026-10-02 prefix-word amendment, both recorded there as
"not read as a command". First, a program another program runs from its arguments was not
read: `find -exec PROG`, `su -c CMD`, `ssh HOST CMD`, `script -c CMD`, `trap 'CMD' SIG`,
`parallel CMD :::`, `watch 'CMD'`, `flock FILE -c CMD`, `env -S CMD`. With
`deny = ["shell(curl*)"]`, `find . -exec curl x \;` and `su -c 'curl x'` ran in Bypass and were
granted by Auto, and `source FILE` ran whatever the file held with no rule consulted. The root
cause was in `permissions.rs`: `push_script_commands` followed only a shell's `-c` script and
`eval` (`shell_script`), so `simple_commands`, and with it `subjects`, the deletion limit and
the rule-conflict warning, never saw these commands. Second, every grant named `argv[0]` only:
`ExecPolicy::evaluate` (`listed` and `by_mode`), `ExecPolicy::script_verdict`
(`grants_program(&words[0])`) and `PermissionControl::route_ask` (Auto's allow set, Accept
edits' file programs) each looked at the first word. A user who put `env`, `xargs`, `sh`,
`find` or `ssh` in the Auto allow set, or a contract granted one, granted whatever it ran. The
command policy's `forbid` rules also matched only the call's own words, so Bypass ran
`sh -c 'git push --force'`.

**The change: reading.** `simple_commands` now reads, at each place `command_starts` lists, what
the program there starts (`Embedded`, `embedded_at` in `permissions.rs`), and reads each found
command the same way, to a depth of 32:

- an argv: `find -exec`, `-execdir`, `-ok`, `-okdir` up to `;` or `+`; BSD `script FILE PROG`;
  `direnv exec DIR PROG`, `mise exec`, `mise x`, `devbox run`, `uv run`, `poetry run`,
  `pipenv run`, `bundle exec`, `conda run`;
- a script, split like a `sh -c` script: `sh -c` and `eval` as before; `su -c` and
  `runuser -c` (also `-lc`, `--command`), `sg GROUP CMD`, `ssh HOST CMD` (after `ssh`'s
  options and their values), `script -c`, `trap CMD SIG`, `watch CMD` (its words joined, as
  `watch` hands them to `sh -c`), `flock FILE -c CMD`, `env -S`, `parallel CMD :::` (and each
  input of `parallel ::: CMD...`); an option whose value is a program the command runs on this
  machine: `ssh -o ProxyCommand=` and `-o LocalCommand=`, `rg --pre` and `--hostname-bin`,
  `tar --to-command`, `-I` (`--use-compress-program`), `--rsh-command`, `-F`
  (`--info-script`), `--new-volume-script` and `--checkpoint-action=exec=`, and `rsync -e`
  (`--rsh`);
- unreadable, with the reason (`unreadable_commands`): `source FILE`, `. FILE`, a shell given a
  file or its input instead of `-c` (`bash run.sh`, `cat x | sh`; `--version`, `--help` and
  `-n` run nothing), `ssh HOST` with no command, `su USER` and `runuser USER` with no command,
  `sudo -i` and `sudo -s` with no command, `script FILE` with no command, and `parallel` with
  commands from a file or its input.

The wrapper list adds `valgrind`, `parallel`, `runuser`, `pkexec`, `unshare`, `nsenter`,
`setpriv`, `firejail`, `torsocks`, `proxychains`, `proxychains4`, `sandbox-exec`, `arch`,
`systemd-run`, `dbus-run-session` and `faketime` (with an operand), and a number after a wrapper
that takes an operand is that operand (`timeout 5 curl`: the program is `curl`, where the scan
before read `curl` as the operand and listed `x` as well).

So the user's deny and ask rules, the deletion limit and the rule-conflict warning see these
commands in every mode, Bypass included. The deletion limit also asks about a recursive deletion
of a placeholder (`find / -exec rm -rf {} \;`, `parallel rm -rf {} :::`; a word holding `{` and
`}` is unresolved, a brace expansion too) and of what `xargs` or `parallel` reads from its input
(`echo / | xargs rm -rf`). A `cd` that a program runs (`find -exec cd`, `ssh host cd`) only adds
a place a relative operand may resolve against; only a `cd` in the call's own shell moves the
first place.

**The change: unreadable commands.** While the user has a deny or ask rule on the shell with a
pattern, a call that starts a command kGraf cannot read is asked about in every mode, Bypass
included (`hard_limit`): the question names the command and the rule it cannot be checked
against. A rule with no pattern already matches every call. With no such rule, the mode decides
as for any command; no grant covers it (below), so Manual, Accept edits and Auto ask, the
orchestrator decides in Orchestrator decides, and Bypass runs it.

**The change: grants.** `programs_to_grant` lists every program a command runs that a grant must
name: each wrapper, the program after it, each word that may be an option's value or the program
(`sudo -u bob curl` names `bob`, which errs toward asking; a number does not count), and every
command of a script a program hands to a shell, read by the strict reader. It refuses outright,
so no program grant covers: a command run on another machine (`ssh HOST CMD`), the exec wrappers
`watch`, `setsid`, `ionice` and `flock`, `find` with `-exec`, `-execdir`, `-ok`, `-okdir` or
`-delete`, an unreadable command, and a script the strict reader does not judge. Claude Code
does the same for the exec wrappers and the `find` actions: a prefix rule never approves them,
only an exact rule for the whole command line, and kGraf's grants name programs, never command
lines. The command policy (`ExecPolicy::evaluate`, `script_verdict`, `record_mode_grant`) and
`route_ask` grant a command only when a grant covers every listed program, each from the
contract or the mode. A grant of `env` grants `env curl x` only with `curl` granted too; a
grant of `sh` and `curl` grants `sh -c 'curl x'`; a script is still granted command by command
with the shell itself ungranted, as before. A recorded mode grant names every program the mode
granted. The strict reader's cross-check against `simple_commands` counts the commands of the
script's own shell and of a nested shell, as before, but not a command another program starts
from its arguments: the grant check of the command that starts it covers that one, so
`sh -c 'uv run pytest'` is granted when `uv` and `pytest` are.

**The change: `forbid` rules.** A `forbid` rule of the command policy also refuses its shape in
every command the call starts (`sh -c 'git push --force'`, `find . -exec git push --force \;`,
`su -c`, `ssh HOST CMD`), in Bypass too, and names that command the way a script's refused
command is named (`the script runs `git` (...)`), so the refusal names its program.

**Compared with Claude Code.** Claude Code strips `timeout`, `time`, `nice`, `nohup`, `stdbuf`,
`command`, `builtin`, `noglob` and bare `xargs` before matching, does not match a deny rule
inside `sh -c` or `bash -c`, and does not approve `watch`, `setsid`, `ionice`, `flock` or
`find -exec` / `-delete` by a prefix rule (Claude Code documentation, "Configure permissions",
Bash section, read 2026-10-06). kGraf is at least as strict on each point: a rule matches inside
every script and every command a program runs, a wrapper needs its own grant where Claude Code
strips it, and the same exec wrappers and `find` actions are never granted. kGraf is never
looser.

**Cost.** Broad by design, and each cost errs toward asking. A command that sources a file
(`. ./venv/bin/activate && pytest`) asks in every mode while any shell rule with a pattern
exists. A user who allowed `uv`, `env` or `timeout` now also needs the program it runs in the
allow set. `ssh HOST CMD` is never granted by a program grant. A command named by a variable,
and a program an interpreter runs from its own code (`python3 x.py`, `npm run test`, `make`),
are still not read as commands: kGraf does not read program code.

Evidence: `kgraf-tools` (`tests/inner_programs.rs`:
`a_deny_rule_refuses_the_inner_program_in_every_mode`,
`a_deny_rule_does_not_refuse_another_inner_program_or_data`,
`an_unreadable_inner_command_is_asked_about_and_never_granted`,
`a_grant_of_the_outer_program_does_not_grant_the_inner_one`,
`a_command_runs_when_every_program_it_runs_is_granted`,
`a_forbid_rule_sees_the_inner_command_in_bypass`, each over every mode through the rules hook,
the command policy and the ask routing; `permissions_prefix_tests.rs`:
`simple_commands_read_a_program_given_as_an_argument`,
`unreadable_commands_name_a_file_an_input_or_a_remote_shell`,
`programs_to_grant_lists_every_program_a_command_runs`,
`a_numeric_operand_is_the_wrappers_operand`,
`the_deletion_limit_sees_a_deleter_given_as_an_argument`, and the unchanged
`no_previously_refused_command_of_the_corpus_is_now_allowed`; `execpolicy/policy.rs`:
`a_different_spelling_of_a_forbidden_shape_falls_to_prompt_not_allow`, which now expects
`sh -c 'rm -rf /'` to be refused); `kgraf-host` (`rule_conflicts.rs`:
`a_program_given_as_an_argument_is_found_and_named`). Each regression test fails with the change
reverted and passes with it restored.

## Amendment (2026-10-06, second): runners, Windows shells, git settings, commands named by a variable, recipes and interpreter code are read

**Why.** The amendment above left five ways to run a program past a rule, and one way for a
grant to fall short. With `deny = ["shell(curl*)"]`, each of these ran `curl` in Bypass, and
Auto granted it to a user who allowed only the outer program:

1. Package, container and remote runners: `npx -c 'curl x'`, `pnpm dlx curl`, `yarn dlx`,
   `bunx`, `docker exec web curl x`, `docker run alpine curl x`, `podman`, `kubectl exec POD --
   curl x`. `embedded_at` (`permissions.rs`) had no case for them, and its fallback, the
   subcommand runners (`uv run`, `direnv exec`), named none of them.
2. Windows shells: `cmd /c`, `cmd /k`, `powershell -Command`, `-EncodedCommand`, `pwsh`. Only
   the POSIX shells of `SHELLS` were read, by a base name that split on `/` alone.
3. git configuration values that run a command: `git -c core.sshCommand=... fetch`,
   `core.pager`, `core.editor`, `core.fsmonitor`, `credential.helper`, `alias.X=!CMD`,
   `diff.external`, `gpg.program`, `GIT_SSH_COMMAND=` and the like before `git`, and `git
   ALIAS` for an alias defined with `!`. git had no reader.
4. A command named by a variable: `sh -c '$CMD x'` read as the program `CMD`, because
   `split_script` trimmed a leading `$` from every part
   (`.trim_start_matches('$')`), meant for the `$` before `$(`; `"$@"`, `${X}`, `$(which curl)
   x` and `eval "$X"` were read the same way, with no reader marking the command unreadable.
5. Interpreter code and build-tool recipes: `python3 -c "import os; os.system('curl x')"`,
   `python3 x.py`, `node -e`, `perl -e`, `ruby -e`, `php -r`, `awk 'BEGIN{system("curl x")}'`,
   `make deploy`, `npm run deploy`, `just deploy`, `cargo run` with a `runner` in
   `.cargo/config.toml`. No reader looked at code or at a file: `embedded_at` had no
   directory to read one from.
6. A shortfall naming the wrapper: when `env curl x` was refused, the refusal on record named
   `env` (`capability_of` in `kgraf-runtime` `shortfall.rs` takes `argv[0]`), so a worker that
   reported `curl` was answered that nothing refused it, and a grant of `env` left the command
   refused, since 0094's amendment above needs `curl` granted too.

**The change: reading context.** The readers now take a `ReadContext`: the worktree, the
directory the command starts in, and the user's home directory (for git's user
configuration), with the variables the call itself sets for a program: assignments before it
(`GIT_PAGER=x git log`, `env A=1 make`) and assignments earlier in its script (`export
GIT_SSH_COMMAND=x; git fetch`). The rules hook reads it from the worker's workspace, and the command policy
from `ExecPolicy::with_working_dir`, which the host sets to the worktree the shell runs in.
With no context, no file is read: every command that runs what a file says is unreadable, and
so is a program named by a relative path (it may be a script of the worktree).
A `cd DIR` in a script moves where the commands after it read their files (`cd web && npm
test` reads `web/package.json`); a `cd` kGraf cannot follow (`cd $X`, `cd -`), a wrapper that
changes the directory (`env -C`, `sudo -D`, `chroot`) and a runner's directory option
(`uv run --directory`, `npm --prefix`) leave it unknown. Files are read only inside the
worktree, regular files of at most 1 MiB, opened confined to it (`fs_safety::read_capped`),
never a FIFO or a device. What a file was parsed into is cached by a hash of its text, and a
file's text by its version (below).

**The change: runners** (`permission_readers.rs`). The package runners run the package's
program with its arguments (the name without a scope or a version): `npx PKG`, `npx -p PKG
CMD`, `npx -c 'CMD'` (a script), `pnpx`, `bunx`, `npm exec` and `npm x`, `npm init PKG` and
`npm create PKG` (which run `create-PKG`), `pnpm dlx`, `pnpm exec`, `pnpm create`, `yarn dlx`,
`yarn exec`, `yarn create`, `yarn node`, `bun x`, `bun exec`, `bun create`, `uvx`, `uv tool
run`, `pipx run`. A package given by a path, a URL or a git spec is unreadable. The container
and remote runners run their command after their options: `docker exec CONTAINER CMD`,
`docker run IMAGE CMD` (with `--entrypoint` naming the program, and `--health-cmd` read as a
script), `docker container exec|run`, `docker compose exec|run`, the same for `podman`,
`nerdctl`, `docker-compose` and `podman-compose`; `kubectl exec`, `run` and `debug` (and `oc`)
after `--`; `lxc exec` and `incus exec`; `vagrant ssh -c`; `adb shell CMD`; `wsl CMD` (a
script) and `wsl -e CMD` (an argv); `busybox APPLET`. Unreadable: `docker run IMAGE` with no
command (the image's own command), an option kGraf cannot place, `kubectl exec` without `--`,
`kubectl run --overrides`, `vagrant ssh`, `adb shell` and `wsl` with no command. A program
named by a path to a file of the worktree runs that file whatever its name (`./make`,
`bin/sh`): a `#!` file is read as its interpreter given the file (`./run.py` with
`#!/usr/bin/env python3` is `/usr/bin/env python3 ./run.py`), a text file with no `#!` line is
unreadable (a shell runs it), and a compiled program is not read. A command that sets `PATH`
(`PATH=./bin ls`, `env PATH=... ls`, `export PATH=...`) is unreadable: a name it runs may be
any file. So is one that sets a variable that makes a program load code before its own:
`LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `DYLD_INSERT_LIBRARIES`, `DYLD_LIBRARY_PATH`,
`DYLD_FRAMEWORK_PATH`, `BASH_ENV`, `ENV`, `ZDOTDIR`, `PYTHONPATH`, `PYTHONSTARTUP`,
`PYTHONHOME`, `PYTHONUSERBASE`, `NODE_OPTIONS`, `NODE_PATH`, `PERL5OPT`, `PERL5LIB`,
`PERLLIB`, `RUBYOPT`, `RUBYLIB`, `PHPRC` and `PHP_INI_SCAN_DIR`. `uv run FILE.py` runs the
file with Python and is read as `python3 FILE.py`.

**The change: Windows shells** (`permission_windows.rs`). `cmd /c` and `/k` (after `/q`,
`/d`, `/s`, `/v:on` and the like) is split on `&`, `&&`, `||` and `|` outside double quotes,
`^` escaping, redirections dropped; `call PROG` and `start ["title"] [/opts] PROG` name their
program. Unreadable: a parenthesized group, `if` and `for`, a program word holding `%` (or `!`
with `/v:on`), `call :label`, and `cmd` with no `/c` or `/k`. `powershell` and `pwsh` read
`-Command` (`-c`, `-CommandWithArgs`), `-EncodedCommand` (`-e`, `-ec`, `-enc`; base64 of
UTF-16LE, decoded), and, for `powershell`, a script given as plain arguments; options are
matched by PowerShell's prefix rule. A script is split into statements on `;`, `|`, `&&`,
`||`, newlines, braces and parentheses outside quotes; a statement that starts with a
variable, a literal or a keyword is no command (the right side of `=` is read again), `&
'PROG'` names PROG, and `Start-Process PROG -ArgumentList ...` names PROG. Unreadable:
`Invoke-Expression` (`iex`), `Invoke-Command`, `Start-Job`, `Add-Type`, `New-Object`,
`Set-Alias`, `Import-Module`, `Invoke-WebRequest`, `.Invoke`, a .NET call (`[`, `::`), a
subexpression (`$(`, `@(`), a backtick escape, `&` or `.` on a variable, dot-sourcing, a program
word holding `$`, `-File` (and `pwsh FILE`), `-Command -`, `-NoExit`, an option kGraf cannot
place, and text that does not decode.

**The change: git** (`permission_git.rs`). A configuration value that names a command is read
as the command git runs (through a shell, as git runs most of them), from `git -c KEY=VALUE`,
from `git config KEY VALUE` (and `git config set`), and from an environment assignment before
`git` or exported earlier in the call's script. The keys: `core.sshCommand`, `core.pager`, `core.editor`, `core.askPass`,
`core.gitProxy`, `core.fsmonitor` (unless `true` or `false`), `core.alternateRefsCommand`,
`sequence.editor`, `credential.helper` and `credential.URL.helper` (`!CMD`, a path, or `NAME`
for `git-credential-NAME`), `diff.external`, `diff.DRIVER.command` and `textconv`,
`merge.DRIVER.driver`, `filter.DRIVER.clean`, `smudge` and `process`, `gpg.program` and
`gpg.FORMAT.program`, `gpg.ssh.defaultKeyCommand`, `pager.CMD`, `interactive.diffFilter`,
`uploadpack.packObjectsHook`, `remote.NAME.uploadpack`, `receivepack` and `vcs`,
`web.browser`, `browser|difftool|mergetool|man.TOOL.cmd` and `path`, `sendemail.sendmailCmd`,
`toCmd`, `ccCmd` and `smtpServer`, `submodule.NAME.update=!CMD`, `hook.NAME.command` and
`instaweb.httpd`. The variables: `GIT_SSH_COMMAND`, `GIT_SSH`, `GIT_EXTERNAL_DIFF`,
`GIT_PAGER`, `PAGER`, `GIT_EDITOR`, `EDITOR`, `VISUAL`, `GIT_SEQUENCE_EDITOR`,
`GIT_PROXY_COMMAND`, `GIT_ASKPASS` and `SSH_ASKPASS`. Unreadable: `core.hooksPath`,
`include.path` and `includeIf.*.path`, `--config-env` for such a key, `--exec-path=DIR`,
`GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT` and its keys, `GIT_CONFIG`, `GIT_CONFIG_GLOBAL`,
`GIT_CONFIG_SYSTEM`, `GIT_EXEC_PATH` and `GIT_TEMPLATE_DIR`. `git NAME`, where NAME is not
one of git's commands (`git help -a`, git 2.54), is an alias: from `-c alias.NAME=`, else from
the repository's configuration (the worktree's `.git` file or directory, its common
directory's `config`, and `config.worktree`) and the user's (`~/.gitconfig`,
`~/.config/git/config`), the last definition winning. `!CMD` runs CMD with the arguments after
it; any other alias runs `git` with its words. A name no read file defines runs the program
`git-NAME`; when a read file includes another, or `-C`, `--git-dir` or `GIT_DIR` points
elsewhere, it is unreadable. The system configuration is not read. `git rebase --exec` (`-x`),
`git bisect run`, `git submodule foreach`, `git filter-branch --*-filter`, `git difftool
--extcmd` (`-x`) and `git send-email --sendmail-cmd` run their command too.

**The change: commands named by a variable** (`permissions.rs`). `split_script` keeps a
variable's `$` and drops only the `$` of `$(`; it also tracks quotes across substitutions, so
`"$(curl x)"` is read. A command a shell expands whose program word (or a wrapper's) holds `$`
or a backquote (`$CMD x`, `"$@"`, `${X}`, `eval "$X"`), and a command whose program is a
command substitution (`$(which curl) x`, `` `echo curl` x ``, `env $(cmd)`), is unreadable. A
call's own argv is not expanded by any shell, so a literal `$` there is a name. With the
`variables` script construct on, a mode may still grant a script holding such a command, as
the switch says; the rules hook asks about it in every mode while a shell rule with a pattern
exists, because no rule can check it, and the switch never lifts a rule.

**The change: build-tool recipes** (`permission_recipes.rs`). Each recipe line is returned as
a script, so the rules, the `forbid` rules, the deletion limit and the grants judge it:

- `make TARGET` (and `gmake`): the recipe lines of each target named (or the default goal) and
  of its prerequisites, from `GNUmakefile`, `makefile` or `Makefile` (or `-f FILE`), in the
  directory `-C DIR` names; `$(shell ...)` and `!=` assignments are read as the commands they
  run; variables defined once (`=`, `:=`, `::=`, `?=`, `+=`), make's built-in programs (`MAKE`,
  `CC`, `RM` and the like) and the automatic variables are expanded; a variable given before
  `make`, exported earlier in its script, or given as `VAR=value` on its command line is an
  override; an undefined variable is empty,
  as in make; `define` blocks expand to their lines; pattern rules' recipes are read for a
  target without a rule of its own. A variable defined more than once is left unresolved (a
  program word holding it is unreadable), and is unreadable outright when one of its values, or
  a function call's arguments, may hold shell syntax; `$(call)`, `$(value)`, `$(eval)`,
  `$(foreach)` and `$(file)` are unreadable. Unreadable: `include`, `.RECIPEPREFIX`, a `SHELL`
  that is not a POSIX shell, `--eval`, and no makefile. Both branches of a conditional are
  read. make's built-in implicit rules are not read.
- `npm run X` (`run-script`), `npm test` (`t`), `npm start`, `stop`, `restart`, `pnpm run X`,
  `pnpm X`, `yarn X`, `bun run X` and `bun X`: the script of `package.json` with its `pre` and
  `post` scripts and the arguments after `--`; npm's default `node server.js` for a missing
  start script; for pnpm, yarn and bun, the package's program of that name when no script has
  it; `bun run FILE` reads the file as code. Unreadable: a workspace or directory option
  (`-w`, `--workspace`, `-ws`, `--prefix`, `-C`, `--dir`, `--filter`, `-r`), a `script-shell`
  in the directory's `.npmrc` or `npm_config_script_shell` in the environment, and no
  `package.json`. `npm install` and the lifecycle scripts
  of dependencies are not read.
- `just RECIPE ARGS`: the recipe's lines and its dependencies' (`dep`, `(dep args)`, `&& dep`),
  with `{{ }}` interpolation of a variable assigned a literal (anything else unresolved), the
  backtick commands of assignments and lines, aliases, parameters counted to tell recipes from
  their arguments, and `-f`, `-d`. Unreadable: `import`, `mod`, a shebang recipe, a `[script]`
  recipe, `[working-directory]` and `[no-cd]`, `set shell` that is not a POSIX shell, `set
  windows-powershell`, `set working-directory`, `--shell`, `--command`, `--choose`, and an
  unknown recipe.
- `cargo NAME`: an alias from `--config alias.NAME=...`, `CARGO_ALIAS_NAME=`, or `[alias]` of
  `.cargo/config.toml` (or `.cargo/config`) in each directory from the worktree root down to the
  command's, deeper winning, expanded as `cargo WORDS ARGS`; for the commands that compile or
  run (`build`, `check`, `test`, `run`, `bench`, `clippy`, `doc`, `rustc`, `rustdoc`, `fix`,
  `install`, `miri` and the built-in aliases), `build.rustc-wrapper`,
  `build.rustc-workspace-wrapper`, `build.rustc`, `build.rustdoc` and `[target.*] linker`, and,
  for `run`, `test` and `bench`, `[target.*] runner`, from those files and from `RUSTC_WRAPPER`,
  `RUSTC`, `CARGO_TARGET_*_RUNNER` and the like. A name that is not one of cargo's commands and
  no alias is an external `cargo-NAME` program, as before. Unreadable: a configuration file
  that does not parse or holds `include`, `--config FILE`, `CARGO_HOME=`, `-C DIR`, and a
  directory kGraf cannot tell. What cargo builds and runs (`build.rs`, tests) is not read.

Files are read from the command's directory only, never from a parent directory (npm, just
and cargo search parents); a recipe kGraf does not find there leaves the command unreadable.

**The change: interpreter code** (`permission_code.rs`). The calls that start a program are
found and their literal arguments read as commands: a string is a script, a list of strings an
argv. Python, JavaScript and TypeScript, Ruby and PHP are parsed with the tree-sitter grammars
kGraf already builds; a parse error makes the code unreadable.

- Python (`python`, `python3`, `python3.N`, `pypy`, `py`; `-c`, a script, `-m MODULE` for a
  module of the worktree, an installed module being a program as before): `os.system`,
  `os.popen`, `subprocess.run`, `call`, `check_call`, `check_output`, `Popen`, `getoutput` and
  `getstatusoutput`, `os.exec*`, `os.spawn*`, `os.posix_spawn` and `posix_spawnp`,
  `os.startfile`, `pty.spawn`, `asyncio.create_subprocess_exec` and `_shell`,
  `platform.popen`, `commands.*`, through any import alias, with `shell=True`, `executable=`
  and `cwd=` read. Unreadable: such a call with arguments that are not literal (`*args`,
  `**kwargs` included), such a function or module used as a value, `exec`, `eval`, `compile`,
  `__import__`, `globals`, `locals`, `vars`, `getattr` on a spawning module, a spawning
  module's own `__` attributes (`os.__dict__`), `sys.modules`, a string naming a spawning
  function in code that imports a spawning module (`methodcaller("system")`), an event loop's
  `subprocess_exec`, `from os import *`, and the import of `builtins`, `ctypes`, `cffi`, `sh`, `plumbum`,
  `pexpect`, `invoke`, `fabric`, `delegator`, `envoy`, `sarge`, `importlib`, `runpy`, `code`,
  `codeop`, `pickle`, `dill`, `cloudpickle`, `marshal`, `shelve`, `multiprocessing`,
  `concurrent` or `pyautogui`.
- JavaScript and TypeScript (`node -e`, `-p`, a script, `-r ./x` and `--import ./x`; `tsx`,
  `ts-node`; `deno eval`, `deno run FILE`; `bun -e`): `child_process` `exec`, `execSync`
  (scripts), `execFile`, `execFileSync`, `spawn`, `spawnSync` (argvs; `{ shell: true }` makes a
  script), `fork` (`node FILE`), through `require` or `import` under any name;
  `Bun.spawn`, `Bun.spawnSync` and ``Bun.$`...` `` (a substitution there is one argument);
  `new Deno.Command` and `Deno.run`. Unreadable: such a call with arguments that are not
  literal or spread, such a function used as a value, a computed lookup on it (on a
  `require(...)` too), a string naming one of its functions in code that loads the module
  (`Reflect.get(cp, "exec")`), `eval`,
  `Function`, a string given to `setTimeout`, `process.binding` and `dlopen`, `require` or
  `import()` of a name that is not literal, and the modules `vm`, `worker_threads`, `module`,
  `inspector`, `repl`, `shelljs`, `execa`, `zx`, `cross-spawn`, `child-process-promise`,
  `node-pty`, `ffi-napi` and `koffi`. `deno run` of a URL and `deno task` are unreadable.
- Ruby (`ruby -e`, a script, `-r ./x`): `system`, `exec`, `spawn`, backquotes and `%x()`,
  `Process.spawn`, `IO.popen`, `Open3.*`, `PTY.spawn`, `open("|CMD")`. Unreadable: such a call
  with arguments that are not literal, a substitution in backquotes, `open` given a value,
  `eval` and its kin, `send`, `public_send`, `method`, `define_method`, `const_get`, `load`,
  `syscall`, a symbol naming a spawning method, and `require` of a name that is not literal.
- PHP (`php -r`, `-B`, `-R`, `-E`, `-f`, a script): `system`, `exec`, `shell_exec`,
  `passthru`, `popen`, `proc_open`, `pcntl_exec`, backquotes. Unreadable: such a call with
  arguments that are not literal, a variable function, a method named by a value,
  `eval`, `assert`, `create_function`, `call_user_func` and the functions that call a callback
  by name, reflection, a string naming a spawning function, `include` of a name that is not
  literal, `-a`, `-S`, and `auto_prepend_file`.
- Perl, awk and AppleScript have no grammar here; their text is scanned. Perl (`-e`, `-E`, a
  script): `system` and `exec` given literal lists, `readpipe`, `qx`, backquotes, and `open`
  with `-|`, `|-` or a two-argument pipe; unreadable: those with a value, `eval` of a string,
  `do FILE`, `require FILE`, `&$`, `&{`, `->$`, `*{`, `->can(`, `s///e`, `-x`, `-d`, `-S`, and
  `IPC::Open2`, `IPC::Open3`, `IPC::Run`, `IPC::Run3`, `IPC::Cmd`, `IPC::System::Simple`,
  `Capture::Tiny`, `Expect`, `Shell`, `Proc::Background`, `Proc::Simple`, `Inline` or
  `FFI::Platypus`. awk (`awk`, `gawk`, `mawk`, `nawk`; the program, `-e`, `-f`):
  `system("CMD")`, `"CMD" | getline`, `print | "CMD"` (and `|&`), regular expressions and
  strings skipped; unreadable: those with a value, `@load`, `@include`, `-i`, `-l`.
  AppleScript (`osascript -e`, a text script file): `do shell script "CMD"`, `do script "CMD"`,
  `doShellScript("CMD")`; unreadable: those given a value or text joined to one, `run
  script`, `load script`, `store script`, `keystroke`, `key code`, raw events, and the
  JavaScript bridge (`ObjC`, `$.NSTask`). A spawning word in a comment or a string is read too:
  the scan errs toward unreadable.
- Local modules: a script's imports of modules of the worktree (`import helper`, `from . import
  x`, `require('./x')`, `require_relative 'x'`, `include 'x.php'`) are scanned too, one level
  deep; a local module that imports another local module makes the command unreadable, and so
  does a relative import kGraf cannot find. Installed packages are not read, apart from the
  modules listed above. Without a directory, an import that may be local is unreadable.
- An interpreter with no reader (`lua`, `luajit`, `Rscript`, `R`, `julia`, `tclsh`, `wish`,
  `expect`, `guile`, `racket`, `elixir`, `erl`, `escript`, `scala`, `groovy`, `swift`,
  `jshell`, `csh`, `tcsh`, `nu`, `xonsh`, `elvish`) given code or a script is unreadable; asked
  only for its version or help, it runs nothing.
- Asked for its version, its help, or a syntax check (`node -c`, `php -l`), an interpreter
  runs nothing; given no code and no file, it reads its input, which is unreadable.

Test runners (`pytest`, `cargo test`, `node --test`, `bun test`, `deno test`) run the
project's test code, which kGraf does not read, as before.

**The change: grants.** `programs_to_grant_in` reads with the same context and refuses what is
unreadable, so a grant of `npx`, `docker`, `cmd`, `powershell`, `git`, `make`, `npm`, `just`,
`cargo` or an interpreter covers none of what it runs: each program needs its own grant, as
for `env` above. A container command is granted when the runner and every program it runs are
granted; `docker run IMAGE` with no command never is. In Auto, a user who allowed only `npm`
now needs the programs of the script too (`jest`); `cargo test` needs a grant for a `runner`
or wrapper the configuration names. The command policy's grant check of a script reads each
command's files from where the script's `cd` took it. `PermissionControl::route_ask`, which
has no directory, grants only what needs no file, so it is never looser than the command
policy.

**The change: the shortfall names what lacks a grant.** A refused command that runs several
programs (`env curl x`, `npx -c 'curl x'`) is recorded with every program a grant must name,
the innermost first (`RefusedCall::needs`), and its capability is the innermost (`curl`), when
a grant could lift the refusal (the command policy's, the access check's, a write scope's
refusal of a command that declares no paths, an unattended ask). The command policy's
question names those programs (`The command runs `curl` (`env curl x`); ... `curl`, `env` have
no grant.`), and a refusal it quotes is grounded in the first. A grant made for the report
covers every one of them that the contract and the mode do not already grant
(`Grant::Programs`), so the next attempt runs the command; each is checked against the user's
rules, deny over ask. A grant the mode records names each program (`granted `cp` and `env``).
See decision 0093's amendment of the same day.

**Compared with Claude Code.** Claude Code matches a Bash rule against the command line and
the commands of a compound command; it does not read what `npx`, `docker exec`, `cmd /c`, a
git setting, a recipe or interpreter code runs, and a prefix rule for the outer program
(`Bash(npm run:*)`, `Bash(make:*)`, `Bash(python3:*)`) approves whatever it runs (Claude Code
documentation, "Configure permissions", Bash section, read 2026-10-06). kGraf matches a deny or
ask rule against each of these inner commands, asks where it cannot read, and needs a grant
for the inner program. It is never looser.

**Cost.** Broad by design, each cost erring toward asking. While any shell rule with a pattern
exists, a command kGraf cannot read asks in every mode: `docker run IMAGE`, `make` with an
`include`, a Python script importing a package of the worktree that imports another, `node
x.js` requiring a module kGraf cannot find, `$PYTHON x.py`. In Auto, an allow set naming only
the build tool or the interpreter asks for what its recipe or script runs. A spawning word in a
Perl, awk or AppleScript comment can make the command unreadable. The scan is not a sandbox:
code can reach a program in ways no static reading sees (a module installed outside the
worktree, data that becomes code), so the mode and the sandbox stay the boundary; the scan
makes the user's rules see every program the code names plainly.

**The change: a judgment rests on the files it read.** Every call of a round is reviewed
before any of them runs, so another call of the round (an `apply_patch`) could rewrite a
recipe or a script between its review and its run. The shell hook notes the files its review
read, each with a hash of its text (`ExecPolicy::note_reviewed`, through
`ReadContext::witnessed`), and the shell tool refuses to run the command when one of them has
changed, appeared or gone away since (`ExecPolicy::files_changed_since_review`), naming the
files and asking for the command to be run again, so it is judged as it is now.

**Performance.** The check is on the harness path, so its cost was measured with `cargo bench
-p kgraf-tools --bench permission_reading` (criterion, release build; Apple M4 Max, 16 cores,
macOS 27.0; `rustc 1.95.0`): per call, the rules hook (`hard_limit`), the shell hook
(`ExecPolicy::evaluate` and `note_reviewed`) and the tool's file check together, in Auto with a
deny and an ask rule, the worktree's files a minute old. Before the change the first two cost
1.2 to 1.6 microseconds a call (10.9 for `sh -c 'cd crates/x && cargo check -q'`). After: `git
status` 3.1, `cargo test -p kgraf-tools` 14.0 (it now looks for `.cargo/config.toml`), the `sh
-c` script 64, `npm test` 88, `make build` 56, `python3 script.py` (a 200-line script) 43, and
562 for the first check of a script kGraf has not seen (it reads and parses it once). A first
version took 170 to 210 for the file-reading cases; one reading per hook instead of three, and
a cache of each file's text by its inode, length and modification and change times (a file
changed within the last two seconds is always read again, as git does for its index) brought
them down. For scale, the lightest sandboxed shell call (`shell/sandboxed_echo` in `benches/
tools.rs`) takes 15.5 milliseconds on the same machine, so the largest of these is under 1% of
a call.

Evidence: `kgraf-tools` `tests/runners_and_code.rs` (each case over every mode through the
rules hook, the command policy and the ask routing, in a real worktree:
`a_rule_judges_what_a_runner_a_setting_a_recipe_or_code_runs_in_every_mode`,
`a_rule_does_not_refuse_what_runs_another_program`,
`an_unreadable_command_asks_under_a_rule_and_is_never_granted`,
`a_grant_of_the_runner_or_the_interpreter_does_not_grant_what_it_runs`,
`ordinary_commands_with_no_rules_are_decided_as_before`,
`a_forbid_rule_sees_what_a_runner_or_code_runs_in_bypass`); `shell.rs`
`a_command_whose_files_changed_after_its_review_is_refused` (through the real hook and tool);
unit tests in
`permission_readers.rs`, `permission_windows.rs`, `permission_git.rs`,
`permission_recipes.rs` and `permission_code_tests.rs`; `execpolicy/policy.rs` and
`execpolicy/script_build_tests.rs`, whose `python3` grants now read the scripts from a
worktree (`a_cd_then_a_granted_program_is_granted_as_its_commands` also checks that a script
starting `curl`, or one kGraf cannot find, is asked about); `permissions.rs`
`each_mode_routes_an_ask`; `kgraf-host` `shortfall_tests.rs`
`a_wrapped_command_is_granted_with_every_program_it_runs` (a hosted run through the real ask
path). Each regression test fails with its part of the change reverted and passes with it
restored.

## Amendment (2026-10-06, third): the readers close more ways a program was run past a rule

Spellings of a command that still ran a program past `deny = ["shell(curl*)"]` in every mode,
Bypass included, are now refused (the program is read and the rule matches) or asked about in
every mode (kGraf cannot read it, and a shell rule with a pattern exists); fail closed, never
readable-and-empty.

- **Script quoting.** The script splitter tracks single quotes, double quotes, `$'...'`, line
  continuations, comments and here-doc bodies, and reads each command substitution as a
  command of its own. A quote or substitution that never closes, and a command that does not
  split into words, make the script unreadable instead of being read as a split on spaces. A
  here-doc body's lines are read as commands. `eval "$(...)"` and `bash -c "$(...)"` are
  unreadable.
- **A program the shell expands.** A program word holding a glob (`/usr/bin/cur?`), a brace
  list or range (`cu{r,}l`) or zsh's `=NAME` is unreadable. A fish script holding `(...)` is
  unreadable.
- **A rule matches its program by path, case and extension.** A subject is also offered under
  the program's base name, lowercased and without a Windows extension, so `/usr/bin/curl`,
  `CURL` and `Curl.exe` all match `shell(curl*)`.
- **More shells, more names.** `ash`, `mksh`, `lksh`, `oksh`, `pdksh`, `posh`, `yash`,
  `rbash`, `bosh`, a versioned name (`bash5`, `bash-5.2`) and another case (`SH`, `Bash`) read
  their `-c` script; `bash -c -- SCRIPT` reads past the `--`.
- **Names the script defines.** `alias c=curl; c x` and `hash -p /usr/bin/curl g; g x` read
  the value as the command it runs; a `function NAME { ... }` name is no program; `fc`,
  `enable -f`, `autoload` and a `PS4`/`PROMPT_COMMAND` assignment are read or made unreadable.
- **Placeholders filled from input.** A `{}` (xargs `-I`/`-i`/`--replace`/`-J`), a `find
  -exec {}` and a `parallel {}` that stands in the program word, or in code an inner shell or
  interpreter runs, is unreadable; an ordinary placeholder argument (`xargs -I{} cp {} d/`) is
  read as before.
- **Tool wrappers.** `cross-env`, `xcrun`, `rlwrap` and `eatmydata` run the command after
  their options.
- **git global options.** A rule on a subcommand matches past the global options: `git -c
  a=b push`, `git -C . push`, `git --no-pager push` and `git --git-dir=.git push` are offered
  the subject `git push`.
- **An install runs the project's own lifecycle scripts.** `npm install` (and `i`, `ci`,
  `add`, `rebuild`), `npm publish` and `npm pack`, and the pnpm, yarn and bun equivalents, run
  the project's own `preinstall`, `install`, `postinstall`, `prepare` family, `prepublishOnly`
  and `prepack`/`postpack` scripts. Dependency lifecycle scripts are still not read.
- **Interpreter code.** Numeric escapes in a command string are decoded (`\x72`, `r`,
  `\u{72}`, `\U00000072`, `\162`, and the C escapes). A spawning module reached as another
  module's attribute (`shutil.os.system`, `asyncio.subprocess`, `distutils.spawn`) is read; a
  spawning function called through `__call__` is a value; Python's `webbrowser` is opaque. In
  JavaScript, `require` reached as a method, a dynamic `import()` handed on, and
  `process.dlopen` are handled. Ruby's `Kernel.\`` called by name and `IO.read`, `readlines`,
  `write` and `foreach` of a `"|cmd"` path are read. PHP's `FFI::cdef(...)` is unreadable.
  `osascript -l JavaScript` (JXA) is unreadable.
- **The refusal names the inner program.** A rule that matches a command inside a recipe, a
  script or a configuration value now names the inner program (`curl`) in the refusal record,
  not the recipe runner (`make`): the refusal subjects are read with the worker's worktree.

Known limits: the window between a file's review and the command's exec is narrowed, not
closed; third-party task chainers (`npm-run-all`, `run-s`) and debugger, profiler and
file-watcher wrappers (`gdb --args`, `perf stat`, `entr`) are opaque and ask in every attended
mode; environment variables that name a command (`EDITOR`, `PAGER`, `MANPAGER`, `LESSOPEN`)
are read only for git; bash's `/dev/tcp` redirection is network egress no `shell(curl*)` rule
names.
