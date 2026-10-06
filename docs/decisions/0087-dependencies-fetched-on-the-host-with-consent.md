# Decision 0087: Dependencies are fetched on the host, outside the sandbox, with the user's per-project consent

**Status:** Accepted
**Related:** decisions 0051, 0069, 0083, 0084

## Context

Workers build and test in the sandbox with the network off, and cargo there cannot write its
cache (decision 0084). So a dependency that is not downloaded and unpacked fails every build
inside the worker, where nothing can fix it. Decision 0084 left fetching to the user. That
made every project's first run fail the same way, and any run after cargo's cache cleaning
removed unpacked sources.

## Decision

1. **A per-project consent with three states:** unset (the default), allowed, refused. It is
   kept in the project's state directory (`.kgraf/dependency-fetch.json`), which git
   excludes and the workers' sandbox can neither read nor write. Allowed means automatic from
   then on. Only crates.io is built; the registry type is open for npm and PyPI later. A
   damaged file is an error, never read as unset.
2. **A run's own answer wins and is stored nowhere:** `--fetch-deps` (this run, its start and
   its workers) and `--no-fetch-deps` on `kgraf orchestrate` and `kgraf schedule run-now`;
   the desktop's "Allow once" (that run only).
3. **At every run start, before anything of the run is built** (the command, scheduled and
   tray-fired runs, the desktop), the host checks offline whether the dependencies are
   ready. When one is missing: allowed runs `cargo fetch` on the host, outside the sandbox,
   then checks again (still missing, or a check that cannot decide, fails); unset stops the run with the exact instruction,
   and a terminal or the desktop asks once, then starts again; refused stops the run with the
   instruction. Unattended with no answer always fails loud: never a silent skip, never a
   silent fetch. A project with no root `Cargo.toml` is not checked; when one exists below
   the root, the run goes on with a notice that names it.
4. **Mid-run:** a worker's build that fails for want of a dependency asks the host once for
   that command. With consent, the host checks the worktree offline, fetches for it, checks
   again, and tells the worker to run the command again; without, the failure stands with the
   instruction. The stored consent is read at that moment, and a refusal stored after the run
   started wins even over the run's own "allow". At most 8 fetches and 32 offline checks per
   run for its workers, tested before any cargo process starts; at most 3,000 packages per
   fetch; a fetch still running is killed when the run ends or the build call that asked is
   cancelled. The answer names no host folder (the mirror, worktree, checkout and home folder
   read as placeholders, also in percent-encoded form; a URL's user and password as
   `<redacted>`), and the tool result is
   secret-scrubbed after the answer is added. So a worker can add a crates.io dependency when
   the project allows it. The consent prompt says so: allowing also lets workers fetch crates they add during the
   run, and the names of those crates go to the registry.
5. **The fetch never runs on the project's or the worktree's own files.** It runs in a private
   mirror of every `Cargo.toml` (and the root `Cargo.lock`), each with an empty `src/lib.rs`
   beside it and an empty file for every other target the manifest names, so `cargo metadata`
   does not fail on a missing target file. Cargo runs in its own process group, which is
   killed at a timeout, a cancel and the command's exit, and when kGraf is interrupted. The mirror is read through a directory capability that
   cannot leave the source, skips every symlink and hidden folder, and is bounded; a worktree's cargo configuration and
   toolchain file are never copied. Cargo writes its lockfile into the mirror, not the
   checkout, and no host process writes into a worktree. `cargo fetch` runs no build script
   (measured). A manifest path that leaves the project (a `path`, `workspace` or member entry
   that is absolute or climbs above the root) would make cargo read a manifest elsewhere on
   the host, so with any, cargo is not run: the check says it could not decide, and the fetch
   is refused naming them. `exclude` counts as a path only in the `[workspace]` table;
   `package.exclude` is a list of gitignore-style patterns. The copy of the root manifest is made a workspace root (an empty
   `[workspace]` is appended when it has none and names none), because cargo otherwise reads
   the checkout's own root manifest, which is not mirrored, as its workspace.
6. **The mirror lives under the checkout's state directory** (`.kgraf/deps-mirror`), never in
   the system temporary directory. Cargo and rustup read configuration, a toolchain file and
   a workspace root from every parent folder; measured, files planted above the system
   temporary directory (a `build.rustc-wrapper`, a `path` toolchain, a `[workspace]` with
   patches) applied to the host's cargo there. Under the state directory the parents are that
   directory, which the workers' sandbox can neither read nor write, and then the checkout, so
   cargo and rustup see the checkout's own settings as they would in the project, relative
   paths in them included.
7. **Only crates.io:** a `git`, `registry` or `registry-index` key in a dependency
   specification (a table or an inline table under `dependencies`, `dev-dependencies`,
   `build-dependencies`, `workspace.dependencies`, a target's dependency tables, `patch` or
   `replace`; a feature or a dependency merely named `git` is not one) of any manifest, member
   or not, refuses the fetch and names it: cargo reads
   manifests that are not members (a `[patch]` path into an excluded folder, a path
   dependency there, a workspace root named by `package.workspace`). Cargo's own listing of
   the members' sources is a second reading. As a further defense every cargo command runs
   with `net.git-fetch-with-cli` and `GIT_ALLOW_PROTOCOL=none`, so `git` refuses every
   transport: no git source is ever cloned into the cargo home, which workers can read (a
   `file://` source would copy a local repository there).
   Every cargo command runs with `RUSTUP_AUTO_INSTALL=0`, so the rustup shim never downloads
   a toolchain before consent: a missing toolchain stops the check with a message to install
   it by hand (rustup 1.28 or later).
8. **Never invisible:** each fetch is a record in the local diagnostics file (registry,
   trigger, ok, packages downloaded) that `kgraf diag show` counts as a network event with no
   tokens, and a notice naming every "Updating" line cargo printed, index or not.
9. **Set-up:** `kgraf deps allow` once per project (also `status`, `refuse`, `reset`, and
   `fetch` to fetch now).
10. **No attempt merges cargo or rustup configuration:** the verifier refuses an attempt that
    changes a `.cargo` folder or a `rust-toolchain` file at any depth, even when the contract
    declared it (names compare as the volume resolves them: NFKC, case folding, NTFS short
    names such as `CARGO~1` and `RUST-T~1.TOM`), and tells the model to leave them and say so in its result. Merged, such a
    file would apply to the host's cargo and every later build. Decision 0084's check already
    refuses builds in a worktree whose configuration changed; this keeps the change out of the
    checkout. Plan intake and a stub path refuse the same paths.

Decision 0051 is amended by consent: permitted egress now also covers the package registry of
a project whose user allowed it, for dependency downloads only.

## Consequences

- A project whose dependencies are cached pays one offline check per run start, about a
  third of a second on an 800-package workspace.
- A scheduled run in a project with no answer (or a refusal) disables its schedule, with the
  instruction as the reason, until the user answers (`kgraf deps allow`) and runs `kgraf
  schedule enable`. A failed fetch (the network) is transient.
- More dependency additions per run may follow from making them possible; the diagnostics
  count is where to look.
- The check reads the working tree's manifests, while a run's worktrees start from its
  integration branch. A project with nested crates and no root manifest is not checked (a
  notice says so). A path dependency outside the project is not followed; such a project's
  check says it could not decide and the run goes on with a notice. A crates.io index
  configured as a git index is refused by the git defense; the default sparse index is not
  affected.

## Amendments (2026-09-29)

- **The mid-run trigger is cargo's own lines on stderr.** A refetch is asked for only when a
  line of the failed build's stderr starts as cargo's missing-dependency diagnostics do (its
  `error: failed to download` or `error: failed to unpack package` line, the same words as a
  `Caused by:` entry, the `--offline was specified` cause, or its offline-mode note). The
  words anywhere else (a test's own output, stdout, mid-line) no longer spend one of the
  run's bounded checks.

## Amendment (2026-10-06): other package managers are not fetched for; a missing install is named

Installed dependencies are now readable inside the sandbox (decision 0084, amended
2026-10-06), so a project the user installed is runnable as it is. The consent-based fetch is
not extended to npm, pnpm, yarn, pip, Go, Maven, Gradle or .NET.

- **Why no fetch.** `cargo fetch` downloads and unpacks and runs no package code. The other
  install commands run package code on the host, outside the sandbox: npm, pnpm and yarn run
  install scripts, pip builds a source distribution by running its `setup.py`, Bundler and
  Composer run plugins, and Gradle and Maven run build logic to resolve. The user's own
  install, which the sandbox reads, makes the tree runnable; kGraf names the command.
- **A missing install is named, never a generic test failure.** `kgraf_host::deps::installs`
  checks, by reading files only: each package with declared dependencies has each one in a
  `node_modules` at its folder or above it (or a `.pnp.cjs`); a Python project with tests whose
  build files name pytest has it in its root virtual environment, or in the host interpreter
  when it has none (a project that runs `unittest` needs none); each
  module a root `go.mod` requires is in the module cache, unless it vendors; a root Maven
  project has the test plugin in `~/.m2/repository`; a root Gradle project has
  `~/.gradle/caches/modules-2`; each .NET project has `obj/project.assets.json`. Each missing
  install says what is missing and the command that makes it, chosen from the lockfile where
  there is one (`npm ci`, `pnpm install --frozen-lockfile`, `yarn install`, `bun install`,
  `go mod download`, `dotnet restore`, a `python3 -m venv .venv` and `pip install` line).
- **Where it is said.** At run start, as a notice after the cargo gate (the run goes on); after
  a failing command's output in the verifier's check and the gate, as `[dependencies]` lines;
  and in the gate discovery note of a gate left out for it (decision 0018, amended 2026-10-06).

Tests: `kgraf-host` `deps/installs.rs`
`an_npm_package_names_each_missing_package_and_its_lockfile_s_command`,
`a_workspace_package_resolves_through_the_hoisted_node_modules`,
`python_tests_need_pytest_in_the_venv_or_the_interpreter`,
`a_project_that_never_names_pytest_needs_none`,
`go_requires_are_read_in_both_forms_and_checked_in_the_module_cache`,
`jvm_and_dotnet_projects_need_their_stores_and_restores`; `dependency_links_tests.rs`
`a_missing_install_is_named_in_the_failure`.
