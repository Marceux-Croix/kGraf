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

## Amendment (2026-10-01): a report is grounded in the worker's recorded refusals

Live local-model runs showed `decide` acting on the worker's words. A malformed `apply_patch`
came back as `denied by access: invalid input ...`; the worker read it as a permission
refusal, reported a need of "write access to discount_flat/src/lib.rs via apply_patch tool",
and the first word became a file grant of `write`. A list of four sibling contracts' files was
held as "writes to `checkout/src/lib.rs,`". Under Orchestrator decides, the prose "running
arbitrary Python code (e.g., python3 -c ...)" granted the program `running` and paid for a
re-dispatch that changed nothing. A skill hold never named the skill when the need was a
sentence, because the tool name was parsed from the whole need.

- **Malformed input is not a refusal.** The approval chain labels a call whose input the tool
  cannot read `invalid-input` (`kgraf_tools::INVALID_INPUT`), not `access`. The worker sees
  `ERROR: invalid call: <reason>; fix the call and retry. This is not a permission refusal.`,
  with no reporting hint, and the call is not recorded as a policy refusal. The worker loop
  keeps its newest input error.
- **The record is readable while the worker runs.** Each policy refusal carries the policy
  that refused it (`by`, `skill:<name>` for a skill) and, for a write-scope refusal, the path.
  The worker loop mirrors its record into the run's `RefusalDirectory`, which
  `report_shortfall` reads by the calling worker's id.
- **`ground` resolves the subject from the record.** The candidates are the unretried
  refusals of the report's scope: shell programs (command), refused paths (file), other tools
  (tool). A `need` that is one clean token must name a candidate; prose or a list picks the one
  candidate it mentions, or the only candidate when it mentions none. The decision, the trace
  and a request to the user name the grounded subject; the worker's words are kept after its
  reason.
- **A report that grounds nothing is answered, not decided.** With no matching refusal the
  worker is told that nothing was granted or held, shown the refusals on record and its last
  input error, and keeps working. A need naming several refused subjects is answered as invalid
  input: name exactly one. A file the contract already declares is answered "you may write it";
  a file another contract of the run declares is answered "it belongs to contract X; do not
  edit it", with the contract's acceptance check. Neither file case is granted in any mode,
  Bypass and Orchestrator decides included (disjoint writes).
- **Answers are bounded.** A worker is answered at most `MAX_ANSWERED_SHORTFALLS` (2) times; its
  next report that grounds nothing stops it, the trace records `stopped: ...`, a `stuck` report
  is raised, and the attempt ends as an error that counts toward the contract's failures, as a
  stalled attempt does. `OpenContract` records the answers (`shortfall_answers`) and the
  stopped workers (`shortfall_stuck`); a worker's entries go when its attempt finishes.
- **A skill hold names the skill from the record.** `skill_refusal` asks the live skills about
  the grounded subject, and falls back to the worker's newest unretried skill refusal of that
  same subject.
- **The trace's `resolution`** reads `granted ...`, `the contract was amended ...`, `waiting for
  the user: ...`, `held: ...`, `answered: ...` or `stopped: ...`.

Evidence, through the real tool and the real run loop: `kgraf-tools` (`approval.rs`:
`malformed_input_is_denied_as_invalid_input_not_by_access`); `kgraf-runtime` (`worker_loop.rs`:
`a_malformed_patch_is_an_invalid_call_not_a_policy_refusal`; `shortfall.rs`: the directory
mirrors a running worker and forgets a finished one); `kgraf-orchestrator` (`shortfall.rs`: one
test per case above, each driving `ShortfallTool` with a recorded refusal set, plus grounding,
path and skill tests); `kgraf-host` (`shortfall_tests.rs`: a report after the granted command
ran is answered and the worker finishes; baseless reports are answered twice, then the attempt
is stuck; Bypass answers a sibling contract's file and never grants it).
