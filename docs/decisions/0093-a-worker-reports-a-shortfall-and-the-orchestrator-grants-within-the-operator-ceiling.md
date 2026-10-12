# Decision 0093: A worker reports a permission or instruction shortfall, and the orchestrator grants within the operator's ceiling, amends, or asks the user

**Status:** Accepted
**Superseded by:** 0094 (in part: the operator ceiling becomes the permission mode)
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

## Amendment (2026-10-01, second): an instructions report about another contract's file is answered

A worker filed an `instructions` shortfall asking for the stub in a file that belongs to another
contract to be changed. The orchestrator amended the worker's own contract with the request and
re-dispatched it, which could not change a file the contract may not write.

An `instructions` report whose text names a file that another contract of the run declares, and
none of the reporting contract's own files, is answered to its worker, as a file-scope report is:
nothing is amended and nothing is re-dispatched; the file belongs to that contract; keep the public
shape that file uses of the worker's items as the existing code defines it; the attempt is judged
by its own acceptance check. The answer counts toward `MAX_ANSWERED_SHORTFALLS` like any other. A
report that names no such file is amended as before, once, then held.

Tests: `kgraf-orchestrator` `shortfall.rs`
`an_instructions_report_about_a_file_another_contract_owns_is_answered`,
`an_instructions_report_is_amended_when_it_names_no_foreign_file_or_its_own`; `kgraf-host`
`shortfall_tests.rs` `an_instructions_report_about_a_sibling_contracts_file_is_answered_not_amended`.

## Amendment (2026-10-02): a script refused whole is retried when the commands it held run

A refusal counted as retried only when a later call of the same capability ran without error,
and a shell script refused whole is recorded against its shell (`sh`). A worker denied
`sh -c "cargo test -p a"` that then ran `cargo test -p a` as argv had run what the script held,
yet its attempt was reported unverified because `sh` never ran again. Such a refusal now records
the programs the script held (`RefusedCall::held`, a directory change left out), and counts as
retried once each of them has run without error since, in any form. A refusal a rule narrowed to
one program is matched on that program only, as before. Details and evidence: decision 0094,
amendment of the same date ("a script of build commands, after one `cd` into the worktree, runs
as a build").

## Amendment (2026-10-02, second): a held shortfall is verified in scope before it blocks

A contract's instruction asked for a dependency to be added to `Cargo.toml`, but its `writes`
listed only `src/lib.rs`, and the dependency was already in `Cargo.toml`. The worker wrote the
correct `lib.rs` body, then tried to edit `Cargo.toml`, was refused, and reported a file
shortfall. No contract wrote that file, so the unattended run held the contract as a plan-shape
error. The attempt was never verified, and its correct in-scope change was thrown away; the run
ended Blocked with the stub still in place.

- **The rule.** A held shortfall is a claim that the contract cannot be done inside its scope. The
  verifier judges that claim before the hold applies. When an attempt stops on a shortfall the
  orchestrator decided to hold, and the attempt changed files, it goes to verification as a
  completed attempt would.
- **A pass merges it.** The attempt is a candidate like any other: the verifier's scope check,
  placeholder check, contract checks and merge-back all apply. The hold is dropped, and the
  shortfall is kept as a fact: a notice and a trace `notice` event name the contract, the check
  that passed, and the held reason in full.
- **A failure keeps the hold.** The attempt is marked stopped again, as before: not a failure,
  its worktree kept for `--resume`, no retry. The held reason gains "Its in-scope result was
  checked first and failed: ...".
- **What does not qualify.** A grant or an amendment (they re-dispatch), a question waiting for
  the user, an attempt that changed nothing, and a pass on a check that checks nothing (`true`
  with no gate command), which is turned into a failure.
- **Fail loud holds.** Nothing unverified merges, and scope is never widened: the refused write
  stays refused, and the verifier refuses any change outside the declared writes.
- **Live tallies.** A held attempt that fails its check is not recorded against its route: it was
  cancelled before it finished.

Rejected: checking the worker's claim against the repository (it needs a semantic reading of the
instruction and the file per file kind; the verifier already says whether the contract is met).
Rejected: granting the path in an unattended run (it widens scope to a file the plan never
assigned).

Tests: `kgraf-host` `shortfall_tests.rs`
`a_held_shortfall_whose_in_scope_result_verifies_is_merged_and_kept_as_a_fact`,
`a_held_shortfall_whose_in_scope_result_fails_its_check_stays_held`,
`a_vacuous_check_does_not_overrule_a_held_shortfall`.

## Amendment (2026-10-05): a refusal beside a real check that passed is a fact, not an unverified mark

A worker added a trait with `apply_patch`, read the file back, then tried
`sh -c 'grep -q "pub trait Greeter" src/lib.rs && ...'` to check its own edit. Manual mode with
nobody attending denied it, because `grep` is not on the build and test allowlist. The worker
then ran `cargo test` (exit 0) and finished. The contract's `cargo test` passed, the change
merged, and the root gate passed, yet kGraf printed "unverified c1: the worker finished after a
policy refused `grep` and never ran it". In recorded local runs, every `unverified` line came
from such a refusal (of `sh`, `bash`, `cd`, `grep`, `diff`, `od` or a `cargo` form, each a
self-check or a build the acceptance command repeated), none from a check that checks nothing,
and where an independent check existed it passed.

- **The rule.** `unverified` means kGraf could not confirm the work. A check that always passes,
  with no gate command beside it, confirms nothing: the contract is unverified, and a refusal its
  worker never retried is added to the reason, as before. A real check that passed did confirm
  the work: the verifier runs it on the committed change whatever the worker did, so a call the
  worker did not make does not weaken it. The contract is verified, and the refusals the worker
  never retried are kept as a fact: a `notice` line and a trace `notice` event, "contract c1:
  verified by `cargo test`; the worker finished after a policy refused `grep` and never ran it"
  (`accepted_standing` in `kgraf-orchestrator/src/shortfall.rs`). A check deferred to the root
  gate counts as real: the run cannot complete until that gate passes.
- **The verb follows the worker's outcome.** A merged attempt that the orchestrator cancelled
  after its shortfall report (the previous amendment) read "the worker finished after ...". It
  now reads "was cancelled", and the other outcomes read "ran out of rounds", "stalled", "handed
  its work back" or "failed". A refusal that named a path is named with it ("`apply_patch` on
  `cfg.toml`"), because the tool itself may have run before.
- **Fail loud holds.** No refusal is hidden: it is in the unverified reason, or in a notice and
  the trace. A check that fails still merges nothing. The verifier, not the worker's trajectory,
  decides whether the contract is met.
- **What is given up.** A worker refused the program its task needed, that worked around it, and
  whose check is weak but not vacuous (`test -f answer.txt`), is now reported verified with a
  notice. That check is weak with or without a refusal, and marking every refused self-check
  unverified made the mark noise in Manual mode. The Terminal-Bench bridge, which uses `true`, is
  still unverified.

Rejected: keeping the mark only when the worker changed files after the refusal. A successful
shell call can write, so this needs a tree comparison after each call, and a worker's next call
is often a shell call (`cargo test`) that writes `target/`. Rejected: the stricter reading of the
option "Fail a contract whose worker was refused and finished" above. It was written for a `true`
check, and its own words give the reason kept here: the work may be right, because the worker
found another way within policy or the refused call was incidental.

Tests: `kgraf-orchestrator` `shortfall.rs`
`a_vacuous_check_is_unverified_and_a_refusal_beside_a_real_check_is_a_fact`,
`a_refusal_names_how_the_worker_ended`; `kgraf-host` `shortfall_tests.rs`
`a_refusal_beside_a_passing_check_is_a_fact_not_an_unverified_mark`,
`a_refusal_beside_a_vacuous_check_is_unverified`,
`a_refusal_beside_a_failing_check_is_never_called_verified`,
`a_vacuous_check_is_unverified_and_a_real_one_is_not`, and
`a_held_shortfall_whose_in_scope_result_verifies_is_merged_and_kept_as_a_fact`.

## Amendment (2026-10-06): a program granted to a contract grants nothing it runs

A program granted to a contract is still an exact program word, but it now covers that program
only, never what it runs (decision 0094, amendment of 2026-10-06). A command runs on grants only
when every program it runs is granted, each by the contract or the user's mode: a grant of `env`
or `xargs` covers `env curl x` or `xargs curl` only with `curl` granted too, and no program grant
covers `ssh HOST CMD`, `watch`, `setsid`, `ionice`, `flock` with a command, `find` with `-exec`,
`-execdir`, `-ok`, `-okdir` or `-delete`, or a command kGraf cannot read (`source FILE`).

Tests: `kgraf-tools` `tests/inner_programs.rs`
`a_grant_of_the_outer_program_does_not_grant_the_inner_one` and
`a_command_runs_when_every_program_it_runs_is_granted`, each with the grant made to the contract
in every mode.

## Amendment (2026-10-06): a report of a wrapped command is granted every program it runs

A refusal of a command that runs several programs (`env curl x`, `xargs curl`, `npx -c 'curl
x'`) named only its first word, `env`, because a refused call's capability was its `argv[0]`
(`capability_of` in `kgraf-runtime` `shortfall.rs`). A worker that reported `curl`, the program
it needed, was answered that no policy refused `curl`; one that reported `env` got a grant of
`env`, and the next attempt was refused again, since a program grant covers that program only
(the amendment above).

Now, when a grant could lift the refusal (the command policy's, the access check's, a write
scope's refusal of a command that declares no paths, or an unattended ask), the refusal on
record lists every program a grant must name for the command to run, the innermost first
(`RefusedCall::needs`), and its capability is the innermost. A report may name any of them. The
orchestrator grounds the report in that refusal and grants, together, the named program and
every other one it lists that the contract and the user's mode do not already grant
(`Grant::Programs`, recorded as one grant record per program); each is checked against the
user's deny and ask rules first, deny over ask, and an unattended hold says how to allow all of
them (`--allow curl --allow env`). A waiting request keeps the list (`PendingPermission::with`)
and grants it whole when the user or a mode change answers. A command of one program, a script
refused whole, and a refusal by a user rule are unchanged. The command policy's question names
the programs that still lack a grant, the innermost first.

Tests: `kgraf-host` `shortfall_tests.rs` `a_wrapped_command_is_granted_with_every_program_it_runs`
(a hosted run: a worker refused `env cp ...` reports `cp`, the orchestrator grants `cp` and
`env`, and the fresh attempt runs the command); with the change reverted, the worker is
answered that nothing refused `cp` and the run is blocked.

## Amendment (2026-10-08): the next step comes before the policy's text

Point 1 put the next step at the end of the refusal. A shell refusal under the Manual mode
quotes the whole build and test allowlist (about 2,400 characters once the npm, Go, Maven,
Gradle and .NET entries were added), so the step came after it. In a recorded run a worker was
refused six times, tried one listed-looking command after another, and never called
`report_shortfall`; in three earlier runs with a refusal half as long, the worker asked after
two to five refusals.

A refusal a worker sees is now: the header as before (`ERROR: denied by <policy>: ` or
`ERROR: refused: `), then the next step, then `The refusal: ` and the policy's own text, whole.
Every reader that matches the header still does, and the record `report_shortfall` reads holds
the policy's text as it was. Mechanism: `kgraf_runtime::shortfall::with_refusal_hint`.

Tests: `kgraf-runtime` `shortfall.rs` `the_next_step_comes_before_a_long_refusal_text` and
`the_header_and_the_policy_name_are_kept`; `kgraf-host` `build_allowlist_tests.rs`
`a_granted_program_runs_under_a_scope_and_another_is_still_refused` (a real unattended shell
refusal: the step comes before the quoted allowlist).

## Amendment (2026-10-10): an answer names the check the verifier runs

The answers to a report about another contract's file (a permission report, and an
instructions report) said "Your attempt is judged by its own acceptance check (<the contract's
Acceptance line>)". The command verifier never runs that line: it runs the run's acceptance
command, narrowed to the tests of the contract's files or whole (decision 0018). A worker told
that `[ -f Cargo.toml ] && ...` judged it was then failed on `cargo test -p ...`.

- **The judge comes from the verifier.** `Verifier::judge(contract, workspace, unmerged)` says
  what judges the attempt, read in its worktree as it is when the report arrives, by the rule
  `verify_among` applies: the command (narrowed with its selection, or whole), and, when the
  check needs work of open contracts the tree lacks (decision 0018, amended 2026-10-10), that a
  failure on the attempt and on the dispatched tree alike is deferred to the root gate, which
  runs the whole command; without a root gate, that the check cannot be deferred. A narrowed
  answer says that the tests are chosen again at verification.
- **No judge is better than a wrong one.** `path_answer` and `instructions_answer` take the
  judge text and quote it (at most 1,500 characters). A verifier that cannot say (`None`, the
  trait default) gets an answer that names no judge; the contract's Acceptance line is never
  quoted in its place.

Tests: `kgraf-orchestrator` `shortfall.rs`
`a_path_answer_names_the_verifier_s_judge_never_the_acceptance_line`; `kgraf-host` `tests.rs`
`the_judge_named_to_a_worker_is_the_check_the_verifier_runs`; `shortfall_tests.rs`
`bypass_answers_a_sibling_contracts_file_and_never_grants_it` and
`an_instructions_report_about_a_sibling_contracts_file_is_answered_not_amended`. With the old
answer text, or with the verifier saying nothing, they fail.

## Amendment (2026-10-11): kGraf files the report for a worker that is refused one program twice

A model often ignores "call `report_shortfall`". In a recorded unattended run in Manual mode on a
local model, the worker was refused `mkdir` once and `python3` four times, never reported, worked
around the refusals, and ran to the time limit. A refusal that only asks the model to report
leaves the outcome to the model. This follows the no-change guard (`MAX_NO_CHANGE_NOTICES`):
a notice that quotes the facts, then kGraf acts, bounded.

- **The rule** (`RepeatedRefusals`, `kgraf-runtime` `shortfall.rs`). When one shell program has
  been refused twice in an attempt, with no run of it without error in between, and the model has
  not called `report_shortfall` in the attempt, kGraf files a permission report (`scope` command,
  `need` the program refused twice). Counted refusals are those of a command policy or the
  unattended ask. Not counted: a refusal of a whole tool (a skill's `allowed-tools`), a refusal
  naming a path (a write scope: a file report is a different report), and a user deny rule (a
  grant cannot lift it). Two different programs refused once each do not trigger it. Any call of
  `report_shortfall` by the model ends the guard for the attempt.
- **It goes the model's way.** kGraf sends the report through the approval chain and the
  registered `report_shortfall` tool, so the grounding in the refusal record, the grant, ask or
  hold decision, and the cancel of the attempt are the ones a report by the model gets. When the
  orchestrator answers without a decision, the answer reaches the model in a notice.
- **At most one per attempt.** There is no loop: the guard is spent when it fires, also when the
  chain does not clear the tool (kept on `trace_failures`).
- **It is shown as kGraf's.** The report's `why` opens with `KGRAF_FILED_LEAD` and names every
  refused program with its command lines, so the run trace carries it. The worker log has a
  `NoticeKind::KgrafShortfall` notice when it is filed and a `KgrafShortfallAnswer` notice for
  the answer. `WorkerResult::kgraf_shortfall` holds the report and the tool's answer. The
  permissions scenarios count it as a fact (`kgraf_filed_shortfalls`) and label the evidence.
- An `ask_user_question` whose text names a refused program does not count toward the rule.

Tests: `kgraf-runtime` `tests/kgraf_files_shortfall.rs`
`two_refusals_of_one_program_without_a_report_are_filed_by_kgraf`,
`one_refusal_then_a_report_by_the_model_files_nothing`,
`a_report_by_the_model_before_the_second_refusal_ends_the_guard`,
`two_different_programs_refused_once_each_file_nothing`,
`a_program_that_later_ran_does_not_count_its_earlier_refusal`. With the call to the guard
removed, the first fails and the others pass; restored, all pass.

Correction (2026-10-12): the rule fired too early. In an internal test run of a Python project the worker
tried two different exploratory `python3 -c` commands, each refused once, and neither was needed
because the gate runs the tests itself. The filing held the contract while the worker's work was
unfinished. The rule now needs the same command line refused twice in the attempt: the same
words, whitespace aside, with the model's `report_shortfall` not called in between. Two different
command lines of one program, refused once each, do not trigger it. The program refused is the
`need` as before. A second trigger (one program refused three or more times with no file change)
was considered and not added: the recorded run it would cover (`mkdir -p out` refused twice with
other calls between) is already covered by the same-line rule. The exclusions, the once-per-attempt
limit, the label and the notice are unchanged. Tests: `two_different_commands_of_one_program_refused_once_each_file_nothing`
fails on the earlier rule and passes on this one; `the_same_command_refused_twice_without_a_report_is_filed_by_kgraf`
(renamed from `two_refusals_of_one_program_without_a_report_are_filed_by_kgraf`) and
`the_same_command_with_other_spacing_counts_as_the_same` cover the trigger.

Addition (2026-10-12): a shell command whose only effect is writing a file does not count. In an internal test run a worker wrote a whole source file with `sh -c "cat > PATH << 'EOF' ... EOF"` twice, Manual mode refused both, and kGraf filed a permission report for `sh`. The worker lacked no permission: `apply_patch` writes that file. A call is a pure file write when it is `sh -c` or `bash -c` with one `cat > PATH` heredoc, or one line of `cat > PATH`, `printf ... > PATH` or `echo ... > PATH`, or `tee PATH`, and PATH is a plain path inside the worktree (no `..`, no expansion; a second command, a pipe, an append or text after the closing line makes it not pure). Such a refusal is not counted toward the filing, and its text tells the worker to write the file with `apply_patch`, with the whole-file patch for that path (a Delete File line only when the file exists). Any other command keeps the earlier behavior. Tests: `a_heredoc_file_write_refused_twice_files_nothing_and_names_apply_patch` fails with the skip removed and passes with it; `a_heredoc_write_outside_the_worktree_still_files`, `a_write_that_also_runs_a_command_still_files` and `the_same_command_refused_twice_without_a_report_is_filed_by_kgraf` (`mkdir -p out`) show that other commands still file.

Addition (2026-10-12): the same path refused for the write scope twice files a `file` report. Several attempts in internal test runs were refused "X is outside the declared write scope" more than once and never filed a report. In one, the worker was refused `pyproject.toml` twice with an identical patch about 15 calls apart, spent the rest of its 30-step budget on `pip install`, `git log` and `recall_history`, ran out of steps, and the next attempt got the grant. The rule: when the same repository path is refused for the write scope twice in one attempt (any tool, any content) and the model has not called `report_shortfall`, kGraf files a report with `scope` file and `need` the path. It uses the same filing path, label, notice and fact as the command report, and the one filing per attempt is shared: a command and a path never both file. The orchestrator decides as usual, including the handover of a finished contract (decision 0030) and the plan-shape hold. This replaces the "not counted: a refusal naming a path" line above for a write-scope refusal only; a protected path, a user rule on a path and a fence refusal still do not count. Two different paths refused once each do not file. `KgrafShortfall::scope` carries the scope. Tests: `the_same_path_refused_for_the_write_scope_twice_files_a_file_report` fails with the path rule disabled and passes with it; `two_different_paths_refused_once_each_file_nothing` and `a_report_by_the_model_ends_the_path_guard` are its counter-cases.

Addition (2026-10-12): a report never asks for a shell that runs a script. In an internal test run the refused calls were `sh -c "mkdir -p out && cp ..."` and `sh -c "sha256sum ..."`. kGraf filed a report for `sh`, the orchestrator granted `sh`, and the next attempt was refused again ("`sh` was granted and an attempt still reported it short"). A grant of `sh` runs none of the commands inside its script; the refusal text says so: the script is "not judged command by command, so no grant covers it". The rule: when the refused call is a script of `SCRIPT_SHELLS` (`sh -c`, `bash -c`, `zsh -c`, `dash -c`), the refusal record lists the programs inside the script that a grant must name (`RefusedCall::needs`, script order, a directory change left out, as for a wrapped command, decision 0094 amended 2026-10-06) and keeps the shell as the call's capability. The report kGraf files has the first of them as `need` and names all of them in `why`; it never names the shell. If kGraf cannot read the script command by command (a substitution, a heredoc, a path-named shell, a recipe it cannot read), no program grant covers it: nothing is filed, and the refusal ends with `SCRIPT_SPLIT_HINT`, which tells the worker to split the script into separate commands. The orchestrator side matches. A report a model files for the shell of a script is not grounded in it: the answer says a script is not granted by naming its shell and names the programs inside it, or says to split an unreadable one. Naming any program inside a readable script grounds the report in the whole set, and the grant names them together. A refusal of a shell that held no script (`sh FILE`) grounds as before. Tests: `a_script_refused_twice_files_for_the_programs_inside_it_not_the_shell` and `an_unparsable_script_refused_twice_files_nothing_and_says_to_split` in `kgraf-runtime`, and `a_script_is_grounded_in_the_programs_inside_it_not_the_shell` and `a_refusal_of_the_shell_alone_still_grounds_and_an_unreadable_script_does_not` in `kgraf-orchestrator`. The two heredoc counter-cases of the pure-write rule now assert that nothing is filed and the split hint is given (a heredoc is unreadable), and `a_write_that_also_runs_a_command_still_files` uses a readable script (`touch a.txt && ls`, filed for `touch`).
