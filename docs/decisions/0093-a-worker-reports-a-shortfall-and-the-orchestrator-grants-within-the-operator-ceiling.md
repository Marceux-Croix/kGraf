# Decision 0093: A worker reports a permission or instruction shortfall, and the orchestrator grants within the operator's ceiling, amends, or asks the user

**Status:** Accepted
**Related:** decisions 0004, 0009, 0027, 0029, 0035, 0053, 0083, 0084, 0086

## Context

Under a declared write scope, a worker's shell runs only the build and test allowlist (decision
0084). A task that needed another program, for example python to read a dataset, had its
command refused. The worker could say so only in prose that nothing routed anywhere, so it
wrote a guessed answer, a check that always passes accepted it, and the run completed with a
wrong result.

A worker that cannot do its brief because of its tool set or its instructions should say so to
the orchestrator, which starts a fresh attempt with a wider permission or better instructions.
Widening permissions without limit would undo decision 0084, so the widening is bounded by what
the operator allowed in advance.

## Decision

1. **A policy refusal tells the worker to report it, and the result lists it.** When a policy
   refuses a worker's call (the write scope's shell rule, the command policy, a skill's
   `allowed-tools`, an unattended approval, a fence), the refusal text ends with: do not work
   around it or guess an answer; call `report_shortfall` naming the command or tool and why,
   then stop. The worker's result lists every policy refusal with its capability (a shell
   call's program, or the tool's name) and whether a later call of it ran without error. A
   denial for malformed input or a path outside the worktree is not a policy refusal.

2. **The report travels the challenge path.** `report_shortfall` is registered in every
   orchestrated worker's environment and bound late, as `challenge_contract` is (decision
   0029). Who reports is the calling worker's agent id; the orchestrator finds the contract
   from the attempt that worker runs. Kind `permission`: a call the brief needs was refused.
   Kind `instructions`: the brief is missing something or is wrong (decision 0027's
   inadequacy, for a work contract). The input takes no other field.

3. **The orchestrator decides at once, mechanically, and stops the attempt.**
   - A permission within the ceiling: the program (an exact `argv[0]`) is granted to the
     contract, recorded with who asked, why and when, and a fresh attempt is dispatched whose
     shell allows it and whose brief names it.
   - An instructions report, the first time: the contract is amended additively; the next
     attempt's brief quotes the report as data in a `<worker_shortfall>` block.
   - Anything else holds the contract for the user, with a question naming what would allow
     it: a program beyond the ceiling, a second instructions report, a grant that did not help,
     or the bound reached.

   The reporting attempt is neither verified nor counted a failure. Every resolution is a
   `shortfall` trace event and a needs-input report.

4. **The ceiling is the operator's.** `--may-grant PROGRAM` (repeatable) on `kgraf
   orchestrate` sets it. It is not in the run file or the worker configuration (both refuse the
   key), and a scheduled run passes none. The default is empty: nothing beyond the configured
   policy is granted, so every permission shortfall holds. The grant decision reads only the
   ceiling and the contract's record; a worker's words choose which program is asked for, never
   whether it may be granted.

5. **Bounded.** A contract is re-dispatched at most twice for shortfalls, grants and
   amendments together. A third report holds; a program granted and reported short again holds;
   one attempt may report once.

6. **"The container is the sandbox" is an operator-only run setting.** `--container-sandbox`
   means the run's commands execute in a disposable container: the shell runs any command no
   `forbid` rule refuses, without approval, under a declared write scope; a skill's
   `allowed-tools` still applies. It turns on only with the flag and
   `KGRAF_CONTAINER_SANDBOX=1` in the environment together; the flag alone fails the command,
   and the variable alone turns nothing on. When on, the command prints a notice to stderr and
   the shell tool's description says so. Where commands run on the host, the OS sandbox still
   confines writes and keeps the network off. kGraf cannot detect a container reliably, so the
   variable is a second statement, set by whatever made the container.

7. **A granted command is confined as a build is** (decision 0084): accepted under a write
   scope, git asked afterwards what it changed, and the verifier's scope check on the committed
   attempt still refuses a change outside the declared writes (decision 0083).

8. **An acceptance that says nothing is reported unverified.** A contract accepted on a check
   that always passes (`true`, `:`, `exit 0`), or whose worker finished after a policy refused
   a call it never then ran, is recorded as unverified with the reason, traced, and printed as
   `unverified <id>: <reason>` after the `verified` line. The run still completes, but never as
   a plainly checked result.

9. **The Terminal-Bench bridge turns the container setting on for the orchestrated arm**, since
   each task runs in a disposable container and every command is routed into it. The
   single-worker arm sets no write scope and needs neither.

## Options considered

- **A new report channel.** Rejected: the challenge path already carries a worker's objection
  with an identity the worker cannot forge.
- **Let the model name the grant.** Rejected: only the operator's ceiling makes a program
  grantable.
- **Have the orchestrator's model rewrite the instruction.** Not taken: the resolution is
  mechanical and cannot be talked past. An instructions report goes to the next attempt as
  data, and a second one asks the user. Rewriting through the planner remains open.
- **Keep the ceiling in a project file.** Rejected: the setting must come only from the
  operator's command line.
- **Detect a container.** Rejected: unreliable, and a false positive would lift the policy on a
  real machine.
- **Fail a contract whose worker was refused and then finished.** Rejected: the work may be
  right. It is reported unverified, and the verifier still decides the merge.

## Consequences

- Every orchestrated worker's prefix gains `report_shortfall` and a longer system prompt: one
  cache reset per route.
- A permission shortfall with no ceiling holds the contract, so a run that used to complete on
  a guessed answer ends blocked with the program named.
- A grant reaches a fresh attempt only through an environment builder that applies it; one that
  cannot refuses a grant rather than dropping it.
- Open: whether an instructions report should go to the planner before the user, and whether an
  MCP tool refused under a scope should be grantable. Both hold today.

## Amendment: the ceiling is the permission mode (decision 0094)

- The operator ceiling is replaced by the user's permission mode. `--may-grant P` is now
  `--allow P`, a program in the Auto mode's allow set; given alone it selects Auto, and the
  program runs at once with no shortfall round trip. Orchestrator decides grants any command or
  file-scope request and refuses without asking; Manual and Accept edits ask the user for what
  they do not grant; with nobody attending, what would ask is denied and the hold names how to
  allow it. A report may name a scope (command, file, tool); a file grant adds the path to the
  contract's declared writes.
- A request the mode would ask about waits on the contract and in the orchestrator box, grouped
  with requests raised alongside it, until the user answers or a mode change settles it.
- The container setting is the Bypass mode: `--container-sandbox`, still with
  `KGRAF_CONTAINER_SANDBOX=1`, selects `--permission-mode bypass`, which keeps the limits listed
  in decision 0094. The Terminal-Bench bridge selects Bypass.
