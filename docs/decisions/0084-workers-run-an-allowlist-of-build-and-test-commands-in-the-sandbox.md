# Decision 0084: Workers run an allowlist of build and test commands in the sandbox, with the network off

**Status:** Accepted
**Related:** decisions 0043, 0051, 0060, 0067, 0071, 0083

## Context

Under a declared write scope, a worker's shell call was refused outright: a shell command
declares no paths, so the per-call scope check could not see what it wrote. Together with the
default command policy (anything not known to be read-only asks for approval, and an
unattended worker's approval is a denial), no orchestrated worker could compile or test its
own work. Every attempt was judged blind by the verifier, and a retry learned nothing.

Two things made a narrower allowance safe: the verifier commits the attempt first and refuses
it if the commit changes any path outside the declared writes (decision 0083 holds the same
line inside the merge), and the sandbox keeps the worktree's gitlink read-only.

## Decision

**1. A declared set of build and test commands runs without approval.** The set lives in the
project's worker configuration, `.kgraf/worker.json` (`build_commands`), outside every
worktree. It is on by default: with no `build_commands` key, or no file, it is `cargo build`,
`cargo check`, `cargo test`, `cargo clippy` and `cargo fmt --check`; `[]` turns it off; any
other entry fails the load with a message naming the five. An orchestrated run adds its own
acceptance command and each configured gate command, exactly as configured. Nothing is chosen
per model or per route. The shell tool's description names the list, except on Windows, where
the sandboxed shell does not run.

**2. Each cargo entry is held to argument rules, an allowlist per subcommand.** Every argument
must be an option listed for its subcommand, with a valid value, or a test-name filter.
Refused, with the argument named: a `--manifest-path` outside the worktree, `--config`, `-Z`,
`--target-dir` and similar output redirections, `-C`, a `+toolchain`, a target spec file,
`cargo clippy --fix`, `cargo fmt` without `--check`, and rustc flags after `--`. A listed
subcommand whose arguments break a rule is refused before any approval question, for attended
workers too; a command not on the list is decided by the command policy as before.

**3. The worktree's cargo configuration and toolchain file are fixed for the attempt.** When
the shell tool is bound to a worktree it records `.cargo`, `.cargo/config.toml`,
`.cargo/config`, `rust-toolchain` and `rust-toolchain.toml` there. Cargo and rustup read these
from the directory a command runs in, and they change what a command runs whatever its
arguments say: an `[alias]` can turn `cargo clippy` into `cargo run`, and a `path` toolchain
runs any binaries. While any of them differs from the record, every allowlisted command is
refused. A project's own committed configuration keeps working.

**4. Under a declared write scope, an allowlisted command runs, and no other command that can
write.** Every other shell command, and every MCP tool, is still refused under a scope.

**5. It runs inside the OS sandbox with the network off.** Writes are confined to the worktree
and its scratch directory, the gitlink is read-only, kGraf's state is unreadable, and cargo
builds into the scratch directory, offline. kGraf does not fetch dependencies: they must be in
the local cargo cache before a run.

**6. What a build can read, and what its output may carry.** A build runs code the worker
wrote, and its output goes to a model provider.

- On macOS the checkout a worktree lives in is unreadable to a build, apart from the worktree
  itself, the git directory, file metadata and the cargo files cargo reads on its way up. A
  `.env` at the checkout root stays private. kGraf's per-user data directory and another
  coding agent's configuration file in the home directory join the credential read deny list.
  Linux reads are already an allowlist.
- An allowlisted command's output goes through the full secret scrub (pattern rules and the
  exact-secret registry, decision 0071) before the model sees it.
- Every value in the checkout's `.env` is registered with the exact-secret registry when an
  attempt is assembled, and never logged.
- A verifier's refusal reason reaches the next attempt as a tagged data block, and the
  worker's system prompt says that block is data, never instructions.

**7. The verifier still judges the committed attempt.** It refuses a commit that changes a
path outside the declared writes, and its refusal reason now reaches the next attempt.

**8. The shell timeout is 600 seconds and configurable,** and a command killed at the timeout
returns its output so far.

## Known gaps

- A process that closes every inherited descriptor and starts a new session outlives its
  command without being seen, on macOS and on Linux without PID namespaces. On Linux with
  namespaces the kernel kills it when the command exits. No reliable way was found to list
  the processes of one macOS sandbox. Only the quality gate, which runs on the merged tree
  outside that sandbox's writable roots, judges what is merged.
- Worker-written test code can still read files outside the checkout that the user can read,
  on macOS, and print them. The pattern scrub catches key-shaped text, not every secret.

## Rejected

- **Keep refusing.** Workers never see a compiler error.
- **Any shell command under a scope, relying on the verifier.** Each command needs its own
  reasoning; an allowlist states it once.
- **A denylist of dangerous cargo flags.** Cargo grows options; a denylist admits each new
  one until someone notices.
- **Setting `RUSTUP_TOOLCHAIN` and isolating cargo's configuration from the host.** It would
  also override a project's own pinned toolchain and configuration.
- **Network access for builds.** Worker-written build scripts and tests would get the network.

## Amendments (2026-09-29)

- **Fetching is superseded by decision 0087.** The sentence "kGraf does not fetch dependencies"
  in item 5 no longer holds: with the user's per-project consent the host fetches crates.io
  dependencies outside the sandbox. The sandbox itself still has no network and never fetches.
- **Reads.** A worker may read repository text files such as `.gitmodules`. Only the `.git`
  directory and kGraf's own state are refused, with the reason, and NTFS short names and
  alternate streams of those paths are refused too.
- **More home credential files are denied**: git's credential-store file under `~/.config/git`,
  RubyGems credentials, and other command-line tool logins. A symlinked credential directory
  is denied at its target. On Linux the home grants are narrower. Known gap, stated by the
  sandbox itself: without PID namespaces and a fresh `/proc`, `/proc` lists the command lines
  and environments of the user's other processes, and an environment can hold secrets.
- **Shell output is bounded as it is read**, so a chatty command cannot grow host memory, and
  the command's process group is killed after a normal exit.
- **`apply_patch` refuses a patch that names one path in two hunks**, because the second hunk
  would silently replace the first. It keeps CRLF line endings and the final-newline state.
- **Path names fold the way the volume does** when the tool guard and the overlap check
  compare them.
- **The tool guards follow links inside the worktree**, so a link to `.git` or `.gitmodules`
  cannot be written through, and `apply_patch` checks every path it writes before it plans
  any hunk. The NTFS short names `GITCON~N` and `GITHOO~N` are protected too.
- **Narrower reads, more denies.** On Linux `~/.local` and `~/.cache` are no longer granted
  whole; on macOS more credential files are denied (Maven, Gradle, Podman, Hugging Face,
  1Password CLI, Oracle Cloud, Bundler, `~/.claude`). A sandbox root strictly inside a
  denied home path is re-allowed after the deny, so a project kept there still builds.
- **The status read after a sandboxed build uses the git directory recorded at start**,
  with hooks and fsmonitor off, and is refused if `<worktree>/.git` has changed.
- **A worktree is not removed under a running process.**
- **The approval question quotes each argument and escapes control characters.**
- **`apply_patch` keeps each line's own ending** in a file with mixed endings.
- **The host-side status read runs no program a command planted.** `git status` runs a clean
  filter for a changed file and enters populated submodules, so the read ignores submodules,
  allows no fetch, and runs only on a gitlink at the worktree root whose git directories lie
  outside the root. A resume checks the gitlink recorded when the worker first started, and a
  recorded absence stays an absence. A transcript from before the gitlink was recorded
  resumes on the gitlink the host's reopen has just verified, and records it.
- **More denies and grants.** On macOS, pip, Yarn and Poetry index logins, cargo's per-user
  config and more command-line tool logins are denied. On Linux, version-manager toolchain
  trees, Pipenv and Poetry environments, and the Poetry, Playwright, Cypress and Deno caches
  are readable; none holds a login, and the keyrings stay unreadable.
- **The per-user cargo config's settings reach a sandboxed build without its secrets.** The
  host writes only the source replacement, registry index, rustflags, linker, target-dir and
  offline keys of `$CARGO_HOME/config.toml` into a cargo home of the command's own, with the
  registry cache linked back, and points `CARGO_HOME` there. Tokens, credential providers,
  `[http]`, `[env]` and a URL carrying a login stay on the host, and a failing command names
  each withheld key. A project's own config still wins key by key. The file is rewritten before every
  command without following links. A cache cargo makes there while the real cargo home has
  none is kept, then renamed aside (never deleted from the host) once the real one exists.
  Rejected: `CARGO_*` variables, which outrank the project's
  config and cannot express `[source]`.
- **A failed check's output keeps its joiners** (U+200C, U+200D) while control characters,
  separators and bidirectional overrides are still replaced.
- **The approval text escapes every blank or hidden character**, including tag characters,
  fillers, variation selectors and typographic spaces, in the tool name and rule note too.
- **`apply_patch` compares the files hunks reach**, so a link and its target are one file.
- **An abandoned external command is reported to its executor**, and a late response is never
  read as the answer to the next request.
- **A missing-dependency line counts only while cargo itself writes stderr.**
- **Windows ACL probe directories a dead process left in a worktree root are removed.**

## Amendments (2026-09-30)

- **The orchestrator's scope check applies the verifier's generated-file rule.** A worker that
  runs `cargo build` in a project with no committed lockfile creates `Cargo.lock`; the
  orchestrator's merge-back scope check, which runs before the verifier, failed the attempt
  for it, although the verifier's commit already leaves such a file out. The rule now lives in
  one place, `kgraf_sandbox::generated_kind`: Python bytecode, or a lockfile named in
  `LOCKFILES` (Cargo, npm, pnpm, Yarn, Bun, Poetry, uv, PDM, Pipenv, Bundler, Go, Composer,
  Nix, Mix, Pub, SwiftPM, Gradle) with one of its manifests in the same folder. The
  orchestrator's check looks the manifest up in the worker's tree and passes such a file on
  to the verifier; merge-back's commit looks it up in git's index and also requires the file
  to be new against the base, so a tracked lockfile the run changed still fails the
  verifier's scope check.

## Amendments (2026-09-30, corrections to the amendment above)

- **A lockfile the commit left out is removed before the acceptance check**, as bytecode
  already was. Kept in the worktree, it let the acceptance command run against dependency
  versions, `git` sources and registry URLs the worker's run chose, although that file is
  never merged. The check now runs against the committed tree only; a build it runs
  generates its own lockfile, which goes with its leftovers.

## Amendment: a refused worker reports the shortfall, and the operator may widen the shell (decision 0093)

- **A refusal under point 3 tells the worker how to report it** (`report_shortfall`), and the
  result lists the refusal.
- **Two operator-only ways past point 3, both confined as a build is:** a program granted to one
  contract within the operator's `--may-grant` ceiling, and any command under
  `--container-sandbox` (with `KGRAF_CONTAINER_SANDBOX=1`). The verifier's scope check on the
  commit still refuses undeclared writes, a fence still refuses them, and a `forbid` rule still
  wins. Neither can be set in the worker configuration or by a model. The default is unchanged.

## Amendment: the ways past point 3 are the permission modes (decision 0094)

- The `--may-grant` ceiling and `--container-sandbox` are replaced by the user's permission mode:
  Auto's allow set, Accept edits' file programs, a grant on a shortfall, and Bypass each run a
  command as a confined, granted one; the verifier's scope check, a fence and a `forbid` rule
  still apply, and the user's deny and ask rules apply in every mode.

## Amendment: host-side git goes through one hardened runner (decision 0094)

The status read after a sandboxed build, merge-back and the worktree commands no longer set
their own hooks and fsmonitor overrides; each goes through `kgraf_core::host_git`, which also
neutralizes every filter, merge and diff driver and every config-defined hook the config names,
refuses every transport (`GIT_ALLOW_PROTOCOL=none` outranks a repository's
`protocol.file.allow`), and removes the inherited `GIT_*` environment but the identity. An
in-tree `.gitattributes` can name a filter driver, but none runs. Measured on git 2.54: a
config-defined hook (`hook.<name>.command`) runs even with `core.hooksPath=/dev/null`, so
`hook.<name>.enabled=false` is set per name. The full list of overrides and covered call sites
is in decision 0094's amendment.

## Amendment: a command line in one string is read as its words; the description names the options

Models often send the whole command line as one string (`["cargo test --workspace"]`, or a
bare string). Taken literally that names a program that does not exist; the allowlist did not
list it, and an unattended run denied the command the project's own acceptance needs. Models
also read the bare list `cargo build; cargo check; cargo test` as "only without flags". The
argument rules of point 2 already accepted the flag set.

- **One string, split.** A `command` that is one string, or one array element holding
  whitespace, is split on whitespace when the line holds no shell syntax
  (`kgraf_tools::execpolicy::split_command_line`). Every reader of a shell call judges the split
  argv through one parser (`kgraf_tools::command_argv`): the command policy and this allowlist,
  the approval hook, the permission modes, the user's deny and ask rules, the critical-deletion
  limit and shortfall matching. The output says the line was split.
- **One string with shell syntax, refused** as invalid input, with the way to write it
  (separate elements, or `["sh", "-c", "<line>"]`, which needs approval).
- **The description names the options** each cargo entry takes and what is refused, and asks
  for one array element per argument; the worker system prompt says the same.
- **The refused set is unchanged** (`--config`, `-Z`, `+toolchain`, `--target-dir`,
  `--artifact-dir`, `-C`, `--lockfile-path`, a `--manifest-path` outside the worktree, a target
  spec file, `cargo clippy --fix`, `cargo fmt` without `--check`). Allowing `+toolchain` was
  rejected because a linked `path` toolchain runs any directory of binaries (point 2a's reason
  for the toolchain file); a `--config` key allowlist was rejected as the denylist problem in
  another form.
- **No other ecosystem**: the allowlist covers cargo and the run's own configured commands.

Rejected for the one-string form: refusing without splitting (models repeat the form, and an
unattended refusal often holds the contract for a call whose meaning is clear); running every
one-string command through `sh -c` (grants shell syntax to every such call); splitting with
quote rules (a quote is shell syntax here and is refused).

## Amendment: the narrowed forms of the acceptance command are allowed too

The verifier may run the acceptance command narrowed to one test file or package (0018). A
worker may run each such form as well: at build time kGraf reads the tree and adds every
single-part narrowed command (one per test file, or per cargo package) to the allowlist,
exactly, beside the command as configured. A test file a worker adds later is not in that
list; the worker can still run the whole command, and the verifier runs the new file's tests.

## Amendment (2026-10-01): two forms of a model's tool call are repaired, and said

- A shell `command` array whose first element holds the program and its leading words
  (`["cargo test", "--workspace", "-q"]`) is split: the words go before the other elements,
  the policy judges the real argv, and the output says it was split. With shell syntax in
  that element the call is refused with the way to write it. Before, the program was named
  `cargo test` and refused.
- An `apply_patch` patch whose first non-blank line is a file header (`*** Add File:`,
  `*** Update File:`, `*** Delete File:`) and that lacks `*** Begin Patch` is applied as if it
  had the envelope (and the end line, when that is missing too). The result carries "note: the
  patch had no '*** Begin Patch' line; it was applied as if it had". Any other malformed patch
  is still refused, and the tool description states the rule. The description is part of the
  shared prefix, so this resets the prompt cache once.

Tests: `a_first_element_holding_words_is_split_before_the_other_arguments`,
`a_patch_without_its_envelope_is_applied_and_the_result_says_so`.

## Amendment (2026-10-02): a repeated patch rejection carries a worked example

About 10 of 23 worker rounds in one benchmark brief were rejected patches ("invalid hunk",
"must be '*** End Patch'", "Failed to find context"). From the second `apply_patch` call a
worker has rejected, the rejection ends with the one rule it broke and a minimal valid patch
for the file the call targeted (an Add File form with one `+` line, or an Update File form
with one context line, one `-` and one `+`). The first rejection carries none; a policy
refusal (a write outside the scope) is not a rejected patch and carries none; an error that is
not about the patch's format or matching gets no example. The envelope leniency above is
unchanged.

Tests: `kgraf-tools` `a_rejection_help_example_is_a_valid_patch_for_the_targeted_file` (the
example parses), `kgraf-runtime` `a_second_rejected_patch_carries_a_worked_example_for_its_file`.

## Amendment (2026-10-02): `apply_patch` repairs more of what a model's intent makes certain, and refuses a silent append

In one benchmark round 290 of 508 `apply_patch` calls failed (57%): "Failed to find expected
lines", "Failed to find context", "last line must be `*** End Patch`", "Unexpected line in
update hunk", a delete and an add of one file refused as one path in two hunks, a bad hunk
header, a unified-diff `@@ -a,b +c,d @@` header, an empty hunk. Stalled attempts ran for a long
time on retries. The same round showed an Update File chunk of only `+` lines applied as an
append, three times, leaving three `[workspace]` tables in one `Cargo.toml`, each call
reporting success. Every repair below is stated in the call's result as a `note:`; anything
else malformed is still refused.

- **A missing or mangled `*** End Patch`** is supplied, like a missing `*** Begin Patch`. The
  note says to check that no lines were cut off, because a patch cut off by the output limit
  now applies as far as it goes.
- **A Delete File and an Add File of one path in one patch is a replace.** The delete is
  dropped, the add overwrites, and the note says so. A delete with an update of one path is
  still refused.
- **An `@@ -a,b +c,d @@` header** is stripped to `@@` plus the text after it, which is the
  context line. The note says the numbers were ignored.
- **Matching.** Context and `-` lines match exactly first (the first match wins, as before),
  then ignoring trailing whitespace, then indentation, then typographic punctuation. These
  looser rules must fit in exactly one place. Several places is refused, naming the lines, and
  one place is accepted with a note. Before, the first looser match was taken silently.
- **A chunk of only `+` lines names no place** and is refused with how to anchor it (an
  unchanged line before it, `*** End of File`, or `*** Add File`), unless the file is empty or
  the chunk ends with `*** End of File`. This applies with an `@@` context too: the context
  was never the insertion point, the lines went to the end of the file.
- **Not changed:** a line in an Update File hunk that has no `+`, `-` or space prefix is still
  refused. Taking it as context would guess the intent of a patch that already mixes forms.

Tests: `kgraf-tools` `a_missing_or_mangled_end_line_is_applied_and_the_result_says_so`,
`a_delete_and_an_add_of_one_path_is_a_replace_with_a_note`,
`a_unified_diff_hunk_header_is_stripped_and_the_result_says_so`,
`unified_header_detection_is_exact`,
`an_anchorless_addition_through_the_tool_is_refused_and_writes_nothing`,
`a_tolerant_match_is_stated_in_the_result`, `replace.rs`
`an_update_of_only_added_lines_is_refused_not_appended`,
`a_whitespace_tolerant_match_that_fits_twice_is_refused`,
`a_context_line_that_fits_twice_after_ignoring_whitespace_is_refused`.

## Amendment (2026-10-06): a looser `apply_patch` match is written in the file's own whitespace

A worker's patch to a Python stub module had one extra space after the ` ` prefix of every
unchanged line, while its `-` and `+` lines were exact. It matched only after ignoring
indentation and was accepted with a note, but the matched region was replaced by the patch's
own lines, context included. Line 1 of the module gained a leading space
(`IndentationError: unexpected indent`), and the call reported success. The same write-back
put the patch's trailing whitespace and ASCII punctuation into the file after the other two
looser rules. Upstream codex keeps context lines from the file in its line-ending-preserving
path; the port had dropped that path and with it this behavior.

- **Unchanged lines are the file's.** After any looser rule, each context line is written as
  the file has it, not as the patch has it.
- **`+` lines move by the measured shift.** For each non-blank matched line pair, the common
  tail of the two indentations is the line's own nesting, and what is left is the pair's
  shift (patch base, file base). When every pair shows the same shift, each `+` line's patch
  base is replaced by the file base, and the note says how much and how many lines moved.
- **Two shifts refuse the patch.** The `+` lines' indentation cannot be known, so the patch is
  refused with two lines that disagree; when every unchanged line has extra indentation and
  every `-` line is exact, the refusal names that slip. A `+` line with less indentation than
  the patch base is refused too. Spaces in the patch against tabs in the file, with an
  unindented line in the same chunk, is two shifts and is refused; before, it was written in
  the patch's spaces.
- **A parse check backs this up.** After a looser match, a `.py`, `.pyi`, `.pyw`, `.yaml` or
  `.yml` file that parsed before the edit and does not after it is refused before it is
  written, with the line of the first error. Python is checked with an indentation pass that
  follows CPython's tokenizer (needed because the tree-sitter grammar accepts an indented
  first line) and then the tree-sitter grammar; YAML with `serde_yaml_ng`. The check is
  comparative, so a file that already failed is not blocked, and it does not run for an exact
  match, which is the model's own edit. A Makefile has no cheap parser here and relies on the
  shift rule alone.
- **A supplied `*** End Patch` keeps the last line whole.** The repair trimmed the whole patch,
  which cut the trailing whitespace off the last body line when the end line was missing; now
  only blank lines at the ends are dropped.

Tests: `kgraf-tools`
`the_recorded_extra_space_patch_is_refused_and_the_module_is_unchanged` (the recorded patch
and stub module as fixtures),
`a_python_patch_at_a_different_indentation_is_written_at_the_file_s`,
`a_yaml_patch_at_a_different_indentation_is_written_at_the_file_s`,
`an_inconsistent_indentation_offset_is_refused_and_writes_nothing`,
`a_tolerant_python_edit_that_no_longer_parses_is_refused`,
`a_supplied_end_line_keeps_the_last_line_s_trailing_whitespace`,
`a_tolerant_match_is_stated_in_the_result`; `replace.rs`
`indentation_and_trailing_whitespace_differences_match_once_with_a_note`,
`spaces_against_tabs_with_a_pinned_line_are_refused`; the unit tests in `reindent.rs` and
`syntax.rs`.

## Amendment (2026-10-06): a Python package build's output is generated, not written

A contract whose work is a build script runs it, as its instruction and acceptance say, and
the build leaves a wheel and a source distribution in `dist/`. The scope check counted them
as the worker's writes outside its scope and failed the attempt ("wrote outside the declared
scope: [dist/<name>-0.1.0-py2.py3-none-any.whl, dist/<name>-0.1.0.tar.gz]"), and the
contract was held after its retry.

- **The rule.** A new file outside the declared writes is a Python package build's output
  when it is `<dir>/dist/<name>.whl` or `<dir>/dist/<name>.tar.gz`, a file under
  `<dir>/<name>.egg-info/` (`<dir>/src/<name>.egg-info/` for a `src` layout, whose project is
  `<dir>`), or a file under `<dir>/build/lib/`, `<dir>/build/lib.<platform>/` or
  `<dir>/build/bdist.<platform>/`, and `<dir>` holds `pyproject.toml`, `setup.py` or
  `setup.cfg` in the snapshot. The scope check passes it on, the merge-back commit leaves it
  out and records it, and it is removed before the acceptance check, as a worker-made
  lockfile is. One notice names them all and says to add `dist/`, `build/` and `*.egg-info/`
  to `.gitignore`.
- **Not covered.** A `dist/` with no Python project manifest where it was built (a web
  bundle, which projects often commit), a tracked file the build changed, and any declared
  path: those are the worker's writes, judged as before.
- **No missing dependency.** No contract wrote `dist/`; the build that made it was the
  contract's own, and the acceptance check builds it again.

Tests: `kgraf-sandbox` `a_python_package_build_is_generated_only_beside_its_project`,
`tests/merge.rs` `commit_retaining_leaves_a_python_package_build_out`; `kgraf-orchestrator`
`a_python_package_build_the_worker_ran_is_not_an_out_of_scope_write`.

## Amendment (2026-10-06, second): every language's build output is generated, Go builds in the sandbox, and a tab-indented file takes a spaces patch

A worker's cargo builds into its scratch directory (`CARGO_TARGET_DIR`), so a Rust project's
build output never reached the tree, whatever its `.gitignore` said. No other language had
that: an npm install, a Maven or Gradle build, `dotnet build`, a CMake build, a virtual
environment, a coverage run or a test runner's cache landed in the tree, and in a project whose
`.gitignore` did not name it the scope check failed a correct attempt as a write outside its
scope. The fix of the amendment above covered Python's package build only. An audit of every
check that can hold or fail an attempt for assumptions that hold only for Rust found the rest.

- **Build and install output beside its project.** A new file outside the declared writes is
  generated when it is under one of these folders and the folder's project manifest is beside
  it: `node_modules/`, `dist/`, `.next/` and `coverage/` (by `package.json`; `coverage/` also by
  `Gemfile` or `composer.json`), `build/` (by `package.json`, a Gradle build file or
  `CMakeLists.txt`), `target/` (by `pom.xml` or `Cargo.toml`), `CMakeFiles/` (by
  `CMakeLists.txt`), `bin/` and `obj/` (by a `*.csproj`, `*.fsproj` or `*.vbproj` in the
  folder), `.venv/`, `venv/` and `htmlcov/` (by a Python project file), `.bundle/` (by
  `Gemfile`) and `vendor/` (by `composer.json`). A file `tsc` emitted beside its source
  (`x.js`, `x.d.ts` and their maps beside `x.ts`) is generated too.
- **Tool caches and compiled files by name.** At any depth, whatever project holds them:
  `__pycache__/`, `.pytest_cache/`, `.mypy_cache/`, `.ruff_cache/`, `.hypothesis/`, `.tox/`,
  `.nox/`, `.nyc_output/`, `.gradle/`, `.parcel-cache/`, `.turbo/`, `.dart_tool/`,
  `.phpunit.cache/`, `.sass-cache/`, the files `.coverage` (and `.coverage.<id>`),
  `.eslintcache` and `.phpunit.result.cache`, and compiled output by extension: `.class`, `.o`,
  `.tsbuildinfo`.
- **A compiled program by its first bytes.** `go build` writes the module's program into its
  folder and `cc` writes `a.out`; no name tells them. A new file whose first bytes are an ELF,
  Mach-O, universal-binary (and Java class) or PE header is generated. It is read without
  following a link, as every host read of the worker's tree is (decision 0083).
- **The same rule in both places, as before.** `kgraf_sandbox::generated_kind` decides for the
  orchestrator's scope check and for the merge-back commit, which leaves every such new file out,
  records it, and removes it before the acceptance check. A file of a tool's tree passes the
  scope check even under a protected name (an npm package's own `.gitattributes` under
  `node_modules/`): none of it is committed. A nested repository is never generated; it stays a
  refusal. The verifier gives one notice for the build output and one for the caches, each
  saying to add the folders to `.gitignore`.
- **Not covered.** A tracked file a build changed, a declared path, and a folder named like an
  output folder with no project manifest beside it (`docs/node_modules/`, a `dist/` with no
  `package.json`): those are the worker's writes, judged as before. A project that commits its
  `dist/` still fails a build that changes a tracked bundle, as before. The size bound of the
  commit still counts these files before they are left out, so an install larger than 512 MiB
  is refused (`TooMuchData`), and the worker's change list carries every file of an install.
- **Go builds in the sandbox.** The go command writes its build cache on every build and test,
  and stops when it cannot ("failed to initialize build cache"); its default, under
  `~/Library/Caches` or `~/.cache`, is outside the writable roots, so every Go build, test and
  check failed. The sandbox now sets `GOCACHE` to `<tmp>/go-build`, beside the npm cache it
  already set: a cold cache per workspace, as cargo's build directory is. On Linux the module
  cache under the default `GOPATH` (`~/go/pkg/mod`) is readable, as the cargo registry is; Go
  takes private-module logins from `~/.netrc` or a helper, never from there.
- **A tab-indented file takes a patch written with spaces.** gofmt indents Go with tabs, a
  Makefile recipe must start with one, and a model writes four spaces per level. The offset
  then grows with the depth, so the one-offset rule of the amendment above refused every nested
  Go edit. When every matched non-blank line is the file's tabs written as the same number of
  spaces per tab (2 to 8), each added line's leading spaces become one tab per that number, and
  the result says so; an added line between two tab widths is refused, naming the file. The
  `replace.rs` test `spaces_against_tabs_with_a_pinned_line_are_refused` named above became
  `spaces_against_tabs_are_written_as_the_file_s_tabs`, which asserts the new behavior and
  keeps the refusals for uneven and mixed widths.

Tests: `kgraf-sandbox`
`every_language_s_build_and_test_output_is_generated_beside_its_project`,
`an_output_name_without_its_project_is_a_write`,
`a_compiled_program_is_told_by_its_first_bytes`, `tests/merge.rs`
`commit_retaining_leaves_other_languages_build_output_out`, `tests/escape_macos.rs` and
`tests/escape_linux.rs` `the_go_build_cache_is_in_scratch_and_writable`;
`kgraf-orchestrator` `other_languages_build_output_is_not_an_out_of_scope_write`;
`kgraf-tools` `a_go_or_makefile_patch_written_with_spaces_gets_the_file_s_tabs`, `replace.rs`
`spaces_against_tabs_are_written_as_the_file_s_tabs`.
