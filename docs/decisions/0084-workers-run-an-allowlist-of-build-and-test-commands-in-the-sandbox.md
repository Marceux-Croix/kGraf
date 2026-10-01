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
