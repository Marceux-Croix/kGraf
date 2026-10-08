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

## Amendment (2026-10-06, third): other build tools' configuration, more lockfiles, and generated files cost nothing

Three parts of this decision covered Cargo and npm only.

- **Build-tool configuration.** The tripwire of `.cargo` and `rust-toolchain`
  (`kgraf_core::paths::is_build_config_path`) now also covers the files through which another
  build tool or version manager runs a program, a JVM option, a package source or a toolchain
  in every later build (`BUILD_CONFIG_NAMES`): `.npmrc`, `.yarnrc*`, `.pnpmfile.cjs`, `.mvn/`
  (wrapper, `jvm.config`, `maven.config`, `extensions.xml`), `gradle.properties`,
  `gradle/wrapper/`, `init.gradle(.kts)`, `pip.conf`, `pip.ini`, `.pypirc`, `NuGet.Config`,
  `Directory.Build.props`, `Directory.Build.targets`, `.bundle/config`, `.tool-versions`,
  `.python-version` and `.nvmrc`, at any depth, however spelled (case, Unicode folding, an NTFS
  stream suffix or short name such as `NPMRC~1` or `GRADLE~1.PRO`). The handling is the same: a
  plan or a stub that declares one is refused, an attempt that changes one is refused with the
  advice to leave it and say so, and the shell tool refuses allowlisted builds once one of the
  root-level files changed in the worktree. Not included: a project manifest (`package.json`,
  `pom.xml`, `build.gradle`, `go.mod`, a `.csproj`), `pyproject.toml` (a build-backend change
  there is ordinary work), and `go.work`, which lists the project's own modules (a contract
  that adds a module must add it there) and whose `toolchain` line is the same as `go.mod`'s.
  These files are now ones a worker cannot change; a project that needs such a change gets it
  from the user, as with `rust-toolchain`.
- **Lockfiles.** NuGet's `packages.lock.json` (beside a `.csproj`, `.fsproj` or `.vbproj`) and
  CocoaPods' `Podfile.lock` (beside a `Podfile`) are lockfiles wherever Cargo's and npm's are
  (`LOCKFILES`), next to the existing `Package.resolved`, `pubspec.lock`, `mix.lock` and
  `composer.lock`. The index skips `packages.lock.json`, `Package.resolved`,
  `npm-shrinkwrap.json`, `bun.lockb` and `gradle.lockfile` as lockfiles, as it skipped the
  others.
- **Generated files cost nothing.** The commit's size bounds (`Limits`: one file, the total)
  counted generated files before they were left out, so an install larger than the total bound
  refused the attempt it was left out of. `prescan` now sets aside each new file outside the
  scope that `generated_kind` classifies with its manifest on disk, and counts only the rest; a
  file set aside that the commit keeps after all is bounded again after the exclusion
  (`size_reasons`), so the bound still holds for everything committed. The worker's change list
  (the files a build or test command changed, on resume too) and the progress measure's digests
  (decision 0096) leave the same files out
  (`kgraf_runtime::worktree::TreeStatus::without_generated`): an untracked file outside the
  declared scope that `generated_kind` classifies. One line says how many were left out. An
  install of thousands of files then costs the worker's context and each checkpoint nothing.
  This replaces the "Not covered" sentence of the amendment above on the size bound and the
  change list.

Tests: `kgraf-core` `other_build_tools_configuration_is_build_configuration`; `kgraf-host`
`an_attempt_changing_cargo_or_rustup_configuration_is_refused`; `kgraf-orchestrator` `plan.rs`
(the write-path refusal test); `kgraf-tools`
`a_worker_written_cargo_alias_or_toolchain_file_refuses_builds`; `kgraf-sandbox`
`a_lockfile_is_generated_only_beside_one_of_its_manifests`, `tests/merge.rs`
`the_size_bounds_count_only_what_the_commit_keeps`; `kgraf-ingest`
`a_recognized_lockfile_is_skipped_even_with_a_code_extension`; `kgraf-runtime`
`an_install_is_left_out_of_the_change_list_and_the_work_stays`.

## Amendment (2026-10-06, fourth): installed dependencies are readable, never writable, and every language builds and tests

A worktree holds only what git tracks, and every project ignores its installed dependencies.
The sandbox denied reads of the checkout a worktree was made from (macOS,
`crates/kgraf-sandbox/src/sandbox/macos.rs`, the checkout deny) or never granted them (Linux,
`sandbox/linux.rs`, where reads are an allowlist), and the Linux allowlist named no JVM or .NET
package store. So a correct npm project with jest, a pytest project in a `.venv`, a Maven,
Gradle or .NET project failed every test inside kGraf: in the worker's shell, the verifier's
acceptance check, the gate and the contract checks alike. And the worker's build allowlist
named cargo only (`crates/kgraf-tools/src/execpolicy/build.rs`, `cargo_default`), so a worker
on any other language could not run its own tests unasked.

- **Installed trees are linked in and readable.** Before a command runs in a worktree, a gate
  worktree or a contract-check tree, kGraf finds the checkout's installed trees beside their
  project's manifest (`kgraf_sandbox::find_dependency_trees`: at most six folder levels and
  20,000 folders, never through a link, never in `.git`, `.kgraf` or build output):
  `node_modules/`, Yarn Plug'n'Play's `.pnp.cjs`, `.pnp.loader.mjs`, `.pnp.data.json` and
  `.yarn/` (or its `cache/`, `unplugged/` and `install-state.gz` when the rest of `.yarn/` is
  committed) beside a `package.json`; `.venv/` or `venv/` holding a `pyvenv.cfg`; `vendor/`
  beside a `go.mod` (with `vendor/modules.txt`), a `composer.json` or a `Gemfile`, and
  `vendor/bundle/` beside a `Gemfile`. Each one the tree lacks becomes a symbolic link at the
  same place, pointing at the checkout's copy (`link_dependency_trees`), made relative to a
  folder opened without following links, so a folder the worker swapped for a link can never
  make the host create a link elsewhere. The sandbox then lets the command read and run
  programs from each linked target (`SandboxPolicy::with_dependency_links`): on macOS an allow
  after the checkout deny and before the credential deny list; on Linux a Landlock read and
  execute grant on each tree.
- **Never written.** No write rule names a target, and both backends judge the resolved path,
  so a write through a link is refused ("Operation not permitted"). A link inside a tree that
  points at a credential file is refused at its target: on macOS by the credential deny list,
  which comes after the grant; on Linux because nothing outside the granted paths is readable.
  A tree that is itself a link in the checkout, or that another user can change (the Linux
  ownership test of the ancestor cargo files), is not linked, and the failing command's output
  says why. A tree beneath a read-denied path (kGraf's state) refuses the policy.
- **The root virtual environment is activated.** When the linked trees include the root
  `.venv/` or `venv/`, every command runs with its `bin` folder first on `PATH`, `VIRTUAL_ENV`
  naming it and no `PYTHONHOME`, so `python3 -m pytest`, the gate's own command, runs the
  environment's interpreter and pytest.
- **Package stores in the home folder, read-only, without their configuration.** macOS reads
  were already open apart from the credential deny list; it now also denies
  `~/.gradle/init.d`, `~/.gradle/init.gradle`, `~/.gradle/init.gradle.kts`,
  `~/.nuget/NuGet/NuGet.Config` and `~/.config/NuGet/NuGet.Config` (repository and package
  source passwords), beside the `~/.m2/settings.xml`, `~/.m2/settings-security.xml` and
  `~/.gradle/gradle.properties` it denied before. Linux grants `~/.m2/repository`,
  `~/.gradle/caches`, `~/.gradle/wrapper/dists` and `~/.nuget/packages`, each only where it
  exists, beside the `~/go/pkg/mod`, `~/.npm`, `~/.cache/pip` and `~/.cargo/registry` it
  granted before; never `~/.m2`, `~/.gradle` or `~/.nuget` themselves.
- **The package managers are kept off the user's writable stores.** Where a command has a
  temporary folder: `GRADLE_USER_HOME` is `<tmp>/gradle-home`, with the user's dependency cache
  read-only beside it (`GRADLE_RO_DEP_CACHE`, when `~/.gradle/caches/modules-2` exists);
  `DOTNET_CLI_HOME` is `<tmp>/dotnet-home`, `NUGET_PACKAGES` is `~/.nuget/packages` when it
  exists, and the .NET CLI's telemetry is off; `GOPROXY=off` with the network denied, so a
  module missing from the cache fails at once, naming it.
- **A link is generated, not written.** git lists a link as one file, so a link is a
  generated file beside its project, by its whole name (`kgraf_sandbox::DEPENDENCY_LINKS`):
  `node_modules`, the Plug'n'Play files and `.yarn` paths beside a `package.json`, `.venv` and
  `venv` anywhere, `vendor` beside a `composer.json`, `Gemfile` or `go.mod`, `vendor/bundle`
  beside a `Gemfile`. Merge-back leaves it out of the commit and removes it before the check,
  which links it again; the verifier's notices and the worker's own change list skip it, since
  it is neither the worker's file nor the check's.
- **Cost, measured.** The search reads one directory per folder: about 0.4 s for a checkout of
  20,000 folders, its bound (macOS, APFS, release build; it was 0.8 to 0.95 s while each
  folder's manifests were asked for one by one), a few milliseconds for a checkout of a few
  hundred. One search of a checkout is reused for 30 s (`TREES_FRESH_FOR`), so a worker's
  shell calls do not each pay it; a tree removed meanwhile is never linked.
- **Every language's build and test commands are on the default allowlist.** The default is
  now `BuildAllowlist::standard`: cargo's five entries and one entry per language
  (`ToolCommand`), each held to argument rules. `npm test` covers `npm test`, `npm run SCRIPT`
  and the same under pnpm and yarn for a build, test, lint, check, typecheck, compile or
  coverage script (and `test:`, `build:`, `lint:`, `check:`, `typecheck:` names), with npm's
  own `--silent`, `--if-present` and workspace options before `--` and anything after it, and
  `npx` with jest, vitest, mocha, tsc, ava, eslint, tap, c8 or nyc. `pytest` covers `pytest`,
  `python -m pytest` and `python -m unittest`. `go test` covers `go build`, `go test` and
  `go vet` on packages of the worktree, with their common options; `-exec`, `-toolexec`,
  `-modfile` and `-overlay` are refused. `mvn test` covers the lifecycle phases `clean`,
  `validate`, `compile`, `test-compile`, `test` and `verify`, with `-o`, `-q`, `-B`, `-pl`,
  `-am`, `-T` and only test-selection properties (`-Dtest=`, `-DskipTests`); `install`,
  `deploy`, plugin goals, `-s` and other properties are refused. `gradle test` covers the
  test, check, build, assemble and compile tasks; `-I`, `-D`, `-P` and `--project-dir` are
  refused. `dotnet test` covers `dotnet build` and `dotnet test` with a project in the
  worktree; `-p:`, `--source` and `--output` are refused. Installs, publishes and fetches
  (`npm install`, `npm ci`, `npm exec`, `pnpm add`, `yarn dlx`, `go get`, `go run`,
  `dotnet add`, `dotnet run`) are not listed, so the command policy asks, as before. A script
  that changes into a package and runs these is judged command by command (decision 0094).
  `build_commands` in `worker.json` accepts the six new spellings, and absent means all.
- **Not covered.** A dependency link that resolves into the checkout outside a granted tree
  (an npm workspace package linked as `node_modules/@scope/pkg -> ../../packages/pkg`) reaches
  the checkout's copy, which stays unreadable; such a run fails with the permission error. A
  nested virtual environment is linked and readable but not activated. Windows makes no links
  (a symbolic link needs a privilege a user account lacks by default); a root package there
  still finds the checkout's `node_modules` by walking up from the worktree, as Windows does
  not confine reads. The Gradle wrapper takes a lock in `~/.gradle/wrapper/dists` before it
  runs, which stays read-only, so a wrapper project needs a gate command of its own (decision
  0018). Maven, Gradle and .NET runs inside the sandbox are built and unit-tested but were not
  run against a real installation of those tools.

Tests: `kgraf-sandbox` `dependencies.rs` `installs_are_found_beside_their_manifests_only`,
`a_tree_that_is_a_link_is_named_and_never_followed`,
`links_are_made_where_the_tree_lacks_them_and_kept_on_a_second_call`,
`a_worker_s_own_folder_is_left_and_a_linked_parent_is_never_entered`,
`the_search_is_bounded_in_depth`, `a_tree_removed_since_the_search_is_not_linked`, and the
ignored measurement `walk_cost_at_the_folder_bound`; `sandbox/mod.rs`
`a_dependency_tree_beneath_a_denied_path_is_refused`,
`a_virtual_environment_is_activated_for_the_command`; `generated.rs`
`a_dependency_link_is_generated_beside_its_project`; `tests/stores_macos.rs`
`stores_and_dependency_trees_are_readable_and_credentials_beside_them_are_not`;
`sandbox/linux.rs`
`package_stores_and_dependency_trees_are_readable_but_credentials_beside_them_are_not`
(Linux only); `kgraf-host` `dependency_links_tests.rs` (macOS, real `node` and `python3`)
`node_test_with_a_package_from_the_checkout_node_modules_passes`,
`a_check_cannot_write_the_checkout_install_through_the_link`,
`a_python_test_importing_from_the_checkout_venv_passes`, and the ignored
`pytest_from_the_checkout_venv_passes`; `kgraf-orchestrator` `tests/contract_checks.rs`
`a_check_finds_a_package_the_user_installed_in_the_checkout` (macOS, real `node`);
`kgraf-runtime` `worktree.rs` `a_dependency_link_is_not_a_change_of_the_worker`;
`kgraf-tools` `shell.rs` `a_worker_s_npm_test_finds_the_checkout_s_installed_package` (macOS,
real `npm` and `node`), `build_tools.rs` rule tests,
`build.rs` `the_standard_list_covers_every_language_and_the_cargo_list_does_not`,
`script_build_tests.rs` `a_cd_then_another_language_s_tests_is_a_build`; `kgraf-host`
`worker/config.rs` `build_commands_parse_absent_empty_and_listed`.

## Amendment (2026-10-06, fifth): a check never reads tool settings the worker left, a dependency grant never follows a link, workspace packages resolve to the tree, and the command's own virtual environment is activated

A security review of the fourth amendment above tried to escape through the links, to run code
or reach credentials it should not, and to steer later commands. Each finding below was shown
by a test that failed before its fix.

- **A check gets a temporary folder of its own.** The verifier's acceptance check runs in the
  worker's tree, and it used the workspace's scratch folder as `TMPDIR`, the folder the
  worker's own shell writes. Tool settings the worker left there steered the check: npm reads
  its user config from `<tmp>/.npmrc` (a `script-shell=/usr/bin/true` there made `npm test`
  pass a failing test), Gradle runs every init script in `<tmp>/gradle-home/init.d`, the .NET
  CLI reads `NuGet.Config` under `<tmp>/dotnet-home`, Go reads its build and test cache in
  `<tmp>/go-build`, and Python its bytecode under the cache prefix. None of them is in the
  attempt's diff, so a forged pass would not be seen. Now every confined check
  (`kgraf_host::gates::run_confined`: the verifier's checks, the gate and the probe at the
  dispatch base) gets a new folder `check-<pid>-<nanoseconds>-<n>` in the scratch folder, made
  with `create_dir` (anything already there fails it), used as `TMPDIR` and for every tool home
  and the bytecode prefix, and removed when the check ends. The cargo target folder stays
  shared (see "Not covered").
- **A dependency grant is the path found, or nothing.** The grant is resolved when the sandbox
  is built. A tree that became a link after the search (the search is reused for 30 s, and a
  swap can come between search and grant) was resolved to the link's target and granted, on
  Linux with read and execute. Now each tree is checked again right before its link and grant
  (same canonical path, not a link, same kind, still only its owner can change it); one that
  changed is not linked, and the command's output says why. `SandboxPolicy::resolved` also
  refuses a dependency read whose canonical path is not the path given.
- **The activated environment is the checkout's folder.** `VIRTUAL_ENV` and the first `PATH`
  entry named the link in the worktree, which the worker can replace (from a parallel call,
  between the link and the command's start) with a folder of its own, putting its programs
  first on `PATH`. They now name the environment's canonical folder in the checkout, a granted
  path no sandboxed command can write. The environment's console scripts already name that
  interpreter in their first line.
- **More credential files are denied on macOS, and uv's store on Linux.** The test of every
  credential file a build or test can touch found these readable on macOS: pip's
  `~/Library/Application Support/pip/pip.conf`, Poetry's
  `~/Library/Application Support/pypoetry/auth.toml`, pnpm's `~/Library/Preferences/pnpm/rc`,
  Bun's `~/.bunfig.toml`, uv's credential store `~/.local/share/uv/credentials`, and Gradle's
  Develocity access keys in `~/.gradle/enterprise` and `~/.gradle/develocity`. All are now on
  the deny list. On Linux the grant of `~/.local/share/uv` included uv's credential store; it
  is now `~/.local/share/uv/python` and `~/.local/share/uv/tools` only. A tool that stops at a
  denied file says so ("Operation not permitted"); pnpm with a global `rc` file was not run.
- **Workspace packages resolve to the tree's copy.** In an npm, pnpm or Yarn workspace the
  checkout's `node_modules` links each workspace package into the checkout
  (`node_modules/@scope/web -> ../../packages/web`); through one link to that folder it
  resolved to the checkout's copy, unreadable, and would have run the checkout's code, not the
  worker's. When an install has such links, the tree gets a real `node_modules` folder of links
  (`kgraf_sandbox::dependencies::mirror_plan`): each workspace link points at the same package
  in the tree, every other entry at the checkout's entry, `.bin` is a folder of the same links
  (relative links copied, so a workspace package's program is the tree's), and an `@scope`
  folder with a workspace link is a folder of links. A mark file in the folder
  (`MIRROR_MARK`) holds the modification times it was made from; a later call with the same
  times keeps it, another adds what was installed since. Everything is made relative to
  folders opened without following links; a folder without the mark (the worker's own
  install) is left as it is. Measured: 2,000 entries, about 0.1 s to make, about 1 ms on each
  later call (macOS, APFS, release build). The grant is the same checkout install; the mark and
  the links are not the worker's changes (`kgraf_runtime::worktree::dirty_paths`).
- **The command's own virtual environment is activated.** Every linked environment is kept
  with the tree folder it belongs to. A command runs with the one owning the folder it works
  in, or the nearest one above it: its working directory, or the folder a script starts with
  (`cd svc/api && python3 -m pytest`, `kgraf_sandbox::script_start_dir`, read in the worker's
  shell, the confined checks and the contract checks).
- **The Gradle wrapper gates.** A sandboxed command's Gradle home is `<tmp>/gradle-home`. For
  the distribution the project's `gradle/wrapper/gradle-wrapper.properties` names, each
  finished unpack in `~/.gradle/wrapper/dists/<name>/<hash>/` (its `.zip.ok` mark) is made
  again there (`kgraf_sandbox::gradle_wrapper`): the folder (the wrapper's lock goes in it), an
  empty mark, and a link to the unpacked distribution, read-only. Nothing is copied and
  nothing in the user's home is written; well under a millisecond per command. Gate discovery
  adds `./gradlew test --offline` when that unpack exists (decision 0018, amended 2026-10-06).
  Not run against a real Gradle; Windows gets no wrapper gate.

**Judged and found not a defect**, with the reason:

- *A write through a link, in any form.* Writing, creating, deleting, renaming, a hard link,
  `chmod` and `touch` through the link all fail on macOS (Seatbelt checks the resolved path,
  and a hard link needs write access to its source); on Linux Landlock handles the refer right,
  so a link or rename out of a read-only tree fails with `EXDEV`. A copy-on-write clone
  (`cp -c`) only makes a copy in the tree. Replacing the link in the tree is allowed and changes
  nothing in the checkout; the next call grants nothing for the worker's folder.
- *A link inside an install pointing at a credential, the checkout's `.git` or `.env`, kGraf's
  state or another folder of the checkout.* Refused at its target on macOS (tested); on Linux
  nothing outside the granted paths is readable.
- *Worktree removal following the link.* `std::fs::remove_dir_all` and git's own removal
  unlink a link and never follow it.
- *Running programs the permission rules deny by name.* The build allowlist matches the
  program name exactly as found on `PATH`; the activated environment's `bin` is the user's own
  install, now named by its checkout path. Every allowlisted runner runs code the worker writes
  (npm's `pretest` and `posttest`, a project `.npmrc` or `.mvn/maven.config`, Gradle build
  scripts, a `Directory.Build.props`, a pytest `conftest.py`), as `cargo test` runs `build.rs`:
  the options the rules refuse can also be set by project files. The sandbox stays the
  boundary: writes confined, the network off in every sandbox that links dependencies, kGraf's
  state unreadable. `npx` with a runner that is not installed tries the registry and fails with
  the network off; `go test -exec` and `-toolexec` are refused, and `GOFLAGS` or a `go.work`
  only choose programs inside the same sandbox; `GOPROXY=off` is a fast failure, the network
  deny is the guard.
- *A worker-written `pyvenv.cfg` or `home =` steering a later run.* The environments are found
  in the checkout only, which no sandboxed command writes; `home =` decides which interpreter
  the environment uses, never a grant (on Linux an interpreter outside the granted paths fails
  loud).

**Not covered**, stated: the cargo target folder of a check in the worker's tree is the
workspace's `<scratch>/target`, which the worker's own builds fill; a worker that forges cargo's
build records there could make a check run a stale test binary. A cold build for each check
would cost minutes, so it stays, and this is recorded as an open gap. A Go check now starts
with a cold build cache.

Tests: `kgraf-host` `dependency_links_tests.rs`
`tool_settings_the_worker_leaves_in_its_scratch_folder_never_reach_the_check`,
`an_npm_workspace_package_resolves_to_the_tree_s_copy`,
`the_virtual_environment_of_the_command_s_folder_is_activated`; `kgraf-sandbox`
`dependencies.rs` `a_tree_swapped_for_a_link_after_the_search_is_never_granted`,
`the_activated_environment_is_the_checkout_s_folder`,
`workspace_package_links_resolve_to_the_tree_s_own_packages`,
`a_worker_s_own_node_modules_is_never_turned_into_a_mirror`,
`the_virtual_environment_is_the_nearest_one_above_the_command_folder`,
`a_script_s_start_folder_is_read_from_a_leading_cd_only`, and the ignored measurement
`mirror_cost`; `sandbox/mod.rs` `a_dependency_read_through_a_link_is_refused`;
`gradle_wrapper.rs` `the_distribution_name_is_read_from_the_wrapper_properties`,
`the_wrapper_distribution_is_reachable_and_the_user_home_is_not_written`;
`tests/dependency_escapes_macos.rs`
`a_command_cannot_change_the_checkout_install_through_its_link`,
`links_inside_an_install_reach_nothing_the_sandbox_denies`,
`a_swapped_venv_link_does_not_put_the_worker_s_programs_first_on_path`;
`tests/stores_macos.rs`
`stores_and_dependency_trees_are_readable_and_credentials_beside_them_are_not` (more
credential files, and the wrapper's folder); `sandbox/linux.rs`
`home_grants_exclude_git_credentials_and_local_share_data` (Linux only); `kgraf-runtime`
`worktree.rs` `a_dependency_mirror_is_not_a_change_of_the_worker`.

## Amendment (2026-10-06, sixth): nothing a worker's run built decides its check, pnpm workspaces resolve to the tree, and Windows links by junction

The fifth amendment above left one gap open, and two parts of the fourth amendment did not
reach pnpm and Windows. Each change below was shown by a test that failed before it.

- **The check's cargo target folder is its own.** The verifier's check used the workspace's
  `<scratch>/target` as `CARGO_TARGET_DIR`, the folder the worker's shell builds into. Cargo
  trusts the build records it finds: a worker that replaced its test binary with a script that
  exits 0 and dated the fingerprints, dep-info and binary in the future made `cargo test` run
  the script, and the check passed broken code. Now every confined check
  (`kgraf_host::gates::run_confined`) runs in a new `kgraf_sandbox::CheckFolder`,
  `<base>/.checks/<pid>-<start>-<nanoseconds>-<n>`, made with `create_dir` after the worker's
  processes have stopped, outside every writable root a worker's sandbox has (its worktree
  and its scratch folder): the check's temporary folder and tool homes are in `tmp/`, its
  `CARGO_TARGET_DIR` is `target/`, and its log is there too. The folder is removed when the
  check ends; one left by a process that ended is removed by the next startup cleanup, one of
  a live process is left. So no process of the worker, not even one that escaped the hold
  check, can change what the check reads there.
- **Warm, not cold.** The verifier's check target starts with a copy-on-write clone of the
  checkout's `target/debug` (the warm folder of decision 0060, which the user builds and no
  sandboxed command writes; `kgraf_sandbox::clone_target_dir`, a clone or nothing, never a
  byte copy). Only the `debug` profile: a clone costs one call per file, and a `target/`
  also holds release builds, other targets and documentation; a check that builds another
  profile builds it cold. Cargo then rebuilds the project's own crates, whose sources in the worktree are
  newer, and reuses the rest. Without a `target/`, or where no clone can be made, the check
  builds cold and a failing check's output says so. The gate and the dispatch-base probe build
  cold, as before: their worktrees are new.
- **Ignored files are removed before the check.** What git ignores is never merged, but a
  check in the worker's tree read it, and every build tool keeps records there of work it
  takes as done: Gradle's `.gradle/` and `build/`, .NET's `obj/` and `bin/`, TypeScript's
  `*.tsbuildinfo`, a `make` target, Python bytecode with no source beside it, a `conftest.py`
  a committed `.gitignore` hides, an install of the worker's own. Before the check, every
  ignored file and folder in the worktree is removed (`MergeBack::remove_ignored`,
  `git ls-files --others --ignored --exclude-standard --directory`, removal that never follows
  a link); the dependency links are made again. The check then sees the commit plus the
  links, as the gate sees a new worktree of the merged commit. On Windows, where the
  descriptor-relative removal is refused, these paths are removed by path
  (`kgraf_sandbox::confined::remove_stopped`): every folder on the way must be a real folder,
  and a junction or link is removed itself; safe because no process of the worker runs any
  more.
- **The other languages' caches.** Go's build cache, npm's and jest's caches, Gradle's and
  the .NET CLI's homes and Python's bytecode prefix were already in the check's temporary
  folder (fifth amendment); that folder is now outside the worker's reach as well. Gradle
  build folders, .NET `obj/` and `bin/`, TypeScript build info and bytecode in the tree are
  ignored files, removed as above.
- **Measured** (macOS, APFS, 2 jobs, 2026-10-06). This repository, `cargo test -p
  kgraf-sandbox --no-run`, target 640 MB in 4,905 files: cold 19.9 s; the old reuse of the
  worker's folder 0.2 s; the new check 0.7 s to clone plus 5.6 s to build, 6.3 s. A two-crate
  workspace: cold 0.5 s, old 0.1 s, new 0.4 s. A clone costs about 0.14 ms per file of the
  warm folder.
- **pnpm workspaces resolve to the tree.** pnpm keeps every package as a real folder in
  `node_modules/.pnpm/<name>@<version>/node_modules/` and links workspace packages there and in
  its hidden hoisting folder `.pnpm/node_modules`. Node resolves a package from its real path,
  so a package reached through a link into the checkout's `.pnpm` looked up its own
  dependencies in the checkout's folders: a workspace package it required was the checkout's
  copy, which the sandbox does not grant ("Operation not permitted"), and no folder of links
  can change where those lookups go. A pnpm install whose root or hoisting folder links a
  workspace package now becomes a copy-on-write clone in the tree (`unix::clone_install`):
  pnpm's links are relative, so in the clone every one resolves inside the tree. The clone is
  made in a new folder beside the tree and moved into place with `renameat` relative to
  folders opened without following links; it carries the mirror mark and stamp (which now
  also covers `.pnpm` and `.pnpm/node_modules`), so a later call keeps it. Measured: 24,002
  entries, 2.8 s (macOS, APFS), once per worktree and once per check. Where no clone can be
  made the folder of links is used and the output says that nested workspace links reach the
  checkout's copies.
- **Windows links by junction.** A directory junction needs no privilege. On Windows each
  installed folder (`node_modules/`, a `.venv/`, `vendor/`, `vendor/bundle/`, `.yarn/`) is a
  junction at the same place, a Plug'n'Play file (`.pnp.cjs`, `.pnp.loader.mjs`,
  `.pnp.data.json`, `.yarn/install-state.gz`) is copied with `create_new`, and a workspace
  install gets a folder of junctions, its `.bin` files copied (`dependencies::by_path`, the
  `junction` crate, MIT). Every folder on the way is checked to be a real folder right before
  each junction, and the junction's folder is resolved again after it; one that left the tree
  is removed and reported. A virtual environment is activated with its `Scripts` folder. The
  Gradle wrapper's folders in the command's Gradle home are made the same way, with a junction
  to each unpacked distribution, and gate discovery adds the wrapper gate on Windows too.
  Read-only: a junction target is judged by its own ACL, and the restricted token writes only
  where a writable root's capability SID is granted, at Low integrity; the checkout's installs
  have neither. The writable root's ACL is written before the first junction is made in it
  (`sandbox::prepare_windows_root`), so its inheritable entries are never propagated while a
  junction is there.

**Not covered**, stated: the Windows code is compile-checked with `cargo xwin clippy` and its
by-path logic is unit-tested on macOS with symbolic links in place of junctions; none of it
has run on Windows. Whether Windows propagates a root's ACL through a junction is not
verified; the Low label is written again on each command. Junctions are made by path: a
folder swapped for a junction in the moment between the check and the creation can place one
junction or copy outside the tree until it is found and removed. A pnpm package whose own
folder in `.pnpm` links a workspace package, while neither the root nor the hoisting folder
does, gets the folder of links, not a clone. On Windows a pnpm install gets the folder of
junctions, with the note. The code under test still runs in the check: a test of the worker's
own can still pass itself. A check now needs what the merged tree needs: an install the worker
made in its tree (with the user's approval) is removed before the check, as the gate never had
it.

Tests: `kgraf-host` `check_inputs_tests.rs`
`a_forged_test_binary_in_the_worker_s_target_folder_never_passes_the_check`,
`an_up_to_date_build_output_the_worker_left_never_passes_the_check`,
`the_check_starts_from_the_checkout_s_build_folder_and_leaves_it_as_it_was`;
`dependency_links_tests.rs`
`a_pnpm_package_requiring_a_workspace_package_gets_the_tree_s_copy` (macOS, real `node`),
`tool_settings_the_worker_leaves_in_its_scratch_folder_never_reach_the_check` (the check's
folders are outside the scratch folder); `tests.rs` retention tests (the ignored build output
in the tree is removed by the check); `kgraf-sandbox` `dependencies.rs`
`pnpm_workspace_links_inside_the_store_resolve_to_the_tree_s_packages`,
`a_pnpm_clone_replaces_only_kgraf_s_own_install`; `dependencies/by_path.rs`
`links_and_copies_are_made_only_where_nothing_is`,
`the_mirror_by_path_resolves_workspace_packages_to_the_tree`,
`gradle_folders_and_links_by_path`.

## Amendment (2026-10-07): a pnpm workspace resolves to the tree on Linux too, and a folder made within a time step of an install's change is made again

The sixth amendment's pnpm clone did not reach Linux. On a file system without copy-on-write
(ext4, tmpfs, overlayfs, so every container on Docker's default storage), `cp -R
--reflink=always` fails ("Operation not supported"), and a pnpm workspace install fell back to
the folder of links: a third-party package in `node_modules/.pnpm` that requires a workspace
package still resolved into the checkout, which the sandbox does not grant. The Linux test run
(`rust:trixie`, Linux 6.8, overlayfs over ext4) failed four `kgraf-sandbox` tests that pass on
macOS, and a fifth intermittently. Each change below was shown by a test that failed before it,
on that Linux run.

- **A byte copy where no clone can be made.** `unix::clone_install` now copies the install
  byte for byte when the clone fails and the install is within `INSTALL_COPY_BOUND` (100,000
  entries and 1 GiB; `dependencies::copy_install`). Folders are made new, links are made again
  with the same text (pnpm's links are relative, so in the copy they resolve inside the tree, as
  in a clone), regular files are copied with their permissions, and special files are skipped;
  nothing in the source is followed. `std::fs::copy` on Linux uses `copy_file_range`, which
  shares blocks where the file system can and copies in the kernel otherwise. The copy carries
  the same mark as a clone, and the output says the install was copied, why no clone was made,
  and its entries and size. Past the bound the folder of links is used, and the output names the
  clone's failure and the bound. The bound is counted before anything is copied.
- **Hard links rejected.** A hard link is the checkout's own file, and for pnpm also the file
  in its global store. The sandbox judges a write by path and the tree is writable, so a
  command could change the user's install, and every project's copy of the package, through a
  hard link in the tree. Links to files in real folders were rejected too: Node resolves a
  module from its file's real path, and a link to the checkout's file puts it back in the
  checkout.
- **One file is probed first.** A `cp` that cannot clone goes on past each failed file and
  makes the whole tree of empty files before it exits. A regular file of the install (found
  depth first, never through a link) is cloned first; when that fails the tree clone is not
  tried.
- **A racy stamp is never current.** A folder of links or a clone is kept while the install's
  stamp (the modification times of `node_modules`, `.bin`, each `@scope`, `.pnpm` and
  `.pnpm/node_modules`) is the same. Linux 6.8 on ext4 keeps a folder's time in steps of the
  kernel's coarse clock (1 ms measured; 10 ms at a 100 Hz tick), so an entry added in the same
  step as kGraf made its folder left the stamp as it was, and the folder stayed without the
  entry until the install changed again. APFS keeps nanoseconds, so macOS never showed it. A
  stamp read less than one second after the install's newest time is now racy
  (`dependencies::stamp_is_racy`) and never current, so the next call makes the folder again
  from the install as it is then, as git does for an index entry written in the second its
  file changed. Cost: one more build of the folder per call made within a second of an
  install's last change.
- **Measured** (release build, 2026-10-07, the ignored test `install_copy_cost`, 24,000 files
  of 8 KiB in 2,000 pnpm package folders: 30,001 entries, 188 MiB). Linux 6.8, overlayfs over
  ext4 in a Colima VM: count 59 ms, `clone_install` with the copy 0.48 s (it was 1.15 s before
  the probe; the failed `cp -R --reflink=always` alone took 0.57 s). Linux on btrfs (a loop
  device): the reflink `cp` 0.35 s, `clone_install` 0.56 s, no copy. macOS, APFS: the clone
  4.0 to 4.2 s, a byte copy of the same install 3.2 to 3.5 s; the clone is kept first because it
  uses no space until written. At the bound a copy is about 1 GiB of disk per tree and per
  check, made once per install change.
- **Windows is unchanged.** A pnpm install gets the folder of junctions with the note. pnpm on
  Windows links packages by junctions with absolute targets, so a copy would not move them into
  the tree.

**Not covered**, stated: the bound is a size, not a free-space check; a copy can still fill a
small disk. A network file system whose clock differs from this machine's by more than a
second can make a stamp look older than it is. The racy window does not cover FAT's two-second
steps.

Tests: `kgraf-sandbox` `dependencies.rs`, each stating what this machine does by probing the
same clone call the product makes (a clone on APFS and on btrfs, a copy on ext4, tmpfs and
overlayfs): `pnpm_workspace_links_inside_the_store_resolve_to_the_tree_s_packages`,
`a_pnpm_clone_replaces_only_kgraf_s_own_install`,
`a_pnpm_install_is_cloned_for_a_workspace_link_in_either_place`,
`kgraf_s_own_earlier_link_is_replaced_and_a_foreign_link_is_left`,
`a_pnpm_install_past_the_bound_is_cloned_or_left`,
`an_install_copy_keeps_links_as_links_and_files_with_their_mode`,
`an_install_past_the_copy_bound_is_not_copied`,
`the_clone_probe_file_is_found_without_following_a_link`,
`a_stamp_read_right_after_a_change_is_racy`,
`an_entry_added_in_the_same_time_step_is_linked_on_the_next_call`;
`a_current_mirror_is_not_walked_again` now ages the install first.

## Amendment (2026-10-07): a command reads the package stores kGraf's install filled, read-only

Decision 0087, amended 2026-10-07, has kGraf run a project's missing install in its sandbox
with the user's consent. Four package managers install into a store outside the project, not
into it: Go's module cache, Maven's local repository, Gradle's dependency cache and NuGet's
packages folder. kGraf's install fills a store of the project's own under
`.kgraf/package-stores/` instead of the user's (`kgraf_sandbox::PackageStores`), so the later
commands of the project must find it.

- **Found with the dependency links.** `link_dependency_trees` also returns the stores that
  exist for the checkout (`DependencyLinks::package_stores`); each must be marked filled by a
  successful install (`.kgraf-store-complete`) and be a real folder whose
  path resolves to itself (a link, or a folder below one, is not used). Every place that grants
  the links (the worker's shell, the verifier's checks, the gates, the contract checks) grants
  the stores with them.
- **Read, never written.** Each store is a readable root: on macOS it is allowed again after the
  state directory's deny, like a worktree under it; on Linux it is granted read and execute. No
  write rule names it. A store that now resolves elsewhere refuses the command
  (`InvalidPolicy`).
- **Named to its package manager**, after the user's own stores, so the project's wins:
  `GOMODCACHE`, `-Dmaven.repo.local` appended to `MAVEN_OPTS`, `GRADLE_RO_DEP_CACHE` (the store's
  `caches`), `NUGET_PACKAGES`. The Gradle wrapper's distribution is taken from the store's
  `wrapper` folder when the store exists.
- **Not verified live**: Go, Maven, Gradle and .NET were not installed on the machine where
  this was built. That each tool reads its store read-only and offline (Maven may try to write
  its tracking files) is taken from their documentation and untested; the grants and the
  variables are unit-tested (`package_stores.rs`, `sandbox/mod.rs`).

## Amendment (2026-10-07): ignored files are removed through the handle walk on Windows too

The sixth amendment's Windows removal by path (`kgraf_sandbox::confined::remove_stopped`) is
replaced. Decision 0083's helpers now work on Windows (its 2026-10-07 amendment), so
`MergeBack::remove_ignored` removes through the same removal on every platform: each name is
opened relative to its folder's handle without following a reparse point, and deleted through
that handle. It no longer relies on no writer being left.

## Amendment (2026-10-08): a shell question also names what the permission mode grants

The question a worker reads for a command that needs approval named only this allowlist. In
Accept edits, a worker that ran `mkdir`, `cp` and `python3 -c` in one `sh -c` script was
refused for `python3`; it read the allowlist, took `cp` for a refused program too, and wrote
the copy by hand. The mode's own grant of `cp` was never used. The question now says, before
the allowlist, which programs the live mode runs without approval: Accept edits' file
programs (`cp`, `mkdir`, `mv`, `touch`), or the programs in Auto's allow set. Manual, and a
mode that grants no program by name, add nothing. The program the refusal names is still the
uncovered one.

Tests: `a_shell_question_names_the_programs_the_mode_grants` (fails without the sentence).

## Amendment (2026-10-08): a toolchain outside the home folder is readable on Linux

The Linux sandbox granted toolchain reads only under `$HOME` (`~/.rustup`,
`~/.cargo/{bin,registry,git}`). A toolchain kept elsewhere (`RUSTUP_HOME=/data/rustup`, a CI
image, a shared machine) made `cargo` fail with "Permission denied" for correct work. The
policy now also reads `RUSTUP_HOME` and `CARGO_HOME` from the host environment:

- `$RUSTUP_HOME` is granted whole. Rustup stores toolchains, `settings.toml`, downloads and
  update hashes there, and no login.
- `$CARGO_HOME` is granted by `bin`, `registry` and `git` only, never as a folder:
  `credentials.toml` and `config.toml` are in it, and the config's safe keys still reach the
  command through its own cargo home.
- A value is ignored, with a line in the command's refused reads, when it is relative, does not
  exist, is not a folder, or would grant too much: the root, the home folder or a folder above
  it, or a rustup home that holds the cargo home. Each granted folder is judged at its resolved
  path, so a `registry` link to the home folder grants nothing. The command still runs with the
  defaults under `$HOME`. Ignoring with a notice was chosen over refusing to start, so a
  mistyped variable does not block work that the defaults can run.

macOS reads are open apart from a deny list, so no grant is needed there; but the list named
only `~/.cargo`. A `CARGO_HOME` outside the home folder now has `credentials`,
`credentials.toml`, `config` and `config.toml` denied by name. On Windows reads are not
confined, so the variables need no grant. A command that clears its environment must set the
two variables itself, as the bench check runner does; the orchestrator's check runner does not
clear it, and neither variable is scrubbed.

Tests: `the_toolchain_variables_grant_rustup_whole_and_cargo_by_subfolder`,
`an_unusable_toolchain_variable_grants_nothing_and_says_why`,
`the_toolchain_variables_are_not_scrubbed`, `toolchain_env_linux` (a program under a
`RUSTUP_HOME` and a `CARGO_HOME` outside the home runs in the sandbox; the credentials file does
not read; fails without the grant), and on macOS
`a_cargo_home_outside_the_home_folder_has_its_secret_files_denied` and `cargo_home_reads_macos`.
