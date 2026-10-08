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

## Amendment (2026-10-07): kGraf runs the other package managers' installs itself, with consent, in its sandbox

This replaces the 2026-10-06 amendment's "no fetch" for npm, pnpm, Yarn, Bun,
pip, Go, Maven, Gradle, .NET, Composer and Bundler. Its detection stays and is what the gate
runs on; cargo's fetch does not change.

1. **One answer per install command.** The consent file holds, beside the registry answers,
   one answer per install, keyed by package manager and folder (`npm:` for the root,
   `npm:packages/web`), with the exact command it was given for (`npm ci`). The answer holds for
   that command only: a changed command (a lockfile added, another package manager) reads as
   unset and the run asks again. A file with install answers is written as version 2; one
   without stays version 1, so an older kGraf still reads it. `kgraf_host::deps::ConsentStore::
   install_consent`, `set_install`, `reset_installs`.
2. **The gate, after cargo's, before anything of the run is built** (`Host::ensure_dependencies`,
   so the command, scheduled and tray-fired runs and the desktop all pass it): the missing
   installs are listed (`missing_installs`), and each is decided by this run's answer over the
   stored one for its command (`deps::install::ensure_installs`).
   - Any unanswered: the run stops before anything runs, with `DepsError::InstallConsentNeeded`
     naming each command, its folder and its package manager, and how to allow it
     (`kgraf deps allow npm`, `--fetch-deps`). A terminal asks (once, always, not now, never),
     then builds again; the desktop shows the pinned card ("dependency install": Allow once /
     Always for this project / Not now); unattended it fails loud and the scheduler disables
     the schedule with the instruction (not transient, 0052). Never a silent install.
   - Allowed (stored, `--fetch-deps`, or "Allow once"): kGraf runs it in its sandbox, then
     checks again. A failure fails loud (`InstallFailed`, transient): its exit, each host the
     proxy blocked with how to allow it, and the last 4,000 characters of its output. An
     install that succeeds and leaves the install missing fails loud (`InstallIncomplete`).
     Each run is a `Notice::DependenciesInstalled` naming the command and the hosts it reached,
     and a `RegistryFetch` diagnostics record with trigger `install`.
   - Refused (stored "never", or `--no-fetch-deps`): the run goes on without it, with the
     not-installed notice, as before this amendment. a refusal does not stop
     the run (cargo's does), because a project whose tests do not need the install still runs,
     and the user who refused chose that.
   - Bounds: 20 minutes per install, then its whole process group is killed; the group is
     registered with the interrupt handler cargo's fetch uses, so an interrupted kGraf leaves no
     install behind.
3. **The sandbox an install runs in.** in the user's checkout, in the
   package's folder, as the user would run it (a worktree would leave the checkout, which every
   worktree links its dependencies from, uninstalled). Writable: the checkout and the project's
   package stores (5). Read-only: the checkout's `.git` (the existing repository-link rule of each
   backend). Hidden, neither readable nor writable: the state directory `.kgraf` (consents,
   permission rules, transcripts, worker configuration), apart from the stores inside it. A new
   policy option does this, `SandboxPolicy::hide` (decision 0011, amended 2026-10-07). Install
   scripts (npm lifecycle scripts, a `setup.py` build, Composer and Bundler plugins, Maven and
   Gradle build logic, MSBuild targets) run inside the same sandbox, so they get the same
   limits. What stays open, by design: the install and its scripts can change files of the
   project, as when the user runs the install; a nested `.git` (a submodule) is writable.
4. **The network: kGraf's egress proxy.** Neither Seatbelt, Landlock nor a network namespace
   knows a host name, so an install gets `Network::Proxy { port }`: TCP to one loopback port
   only, where kGraf's proxy listens, and no other destination, no UDP, no DNS resolver.
   `kgraf_sandbox::egress::EgressProxy` takes `CONNECT host:port` and absolute-form
   `GET http://...`, serves at most 256 connections at once (each takes a thread in kGraf's
   own process), allows only the hosts of the install's ecosystem, and records each host it
   refused (named in the failure) and reached (named in the notice).
   - **macOS**: `(allow network-outbound (remote ip "localhost:PORT"))` and no other IP rule;
     the network policy's mach services stay (trustd checks the tunneled TLS certificates); the
     DNS resolver socket is not allowed.
   - **Linux**: the command gets its own network namespace, whose only interface is its
     loopback. Between fork and exec, before Landlock and seccomp, its first process brings the
     loopback up, opens a listening socket on `127.0.0.1:3128` inside the namespace, and sends
     it to kGraf over a socket pair (`SCM_RIGHTS`); kGraf serves the proxy on it from outside.
     A socket keeps the namespace it was made in, so connections to it come only from the
     command, and the proxy's own connections leave from the host's namespace. Needs user
     namespaces; a kernel without them refuses the policy (fail loud).
   - **Host rules.** An entry is `host`, `*.domain` (every name below it, never the domain
     itself, never an IP address) or either with `:port`. an entry without a
     port allows 443 and 80 only. An allowed name that resolves to a loopback, unspecified,
     link-local, multicast or broadcast address or a cloud metadata endpoint is refused unless
     an address entry names that address; the address checked is the address dialed. Plain
     HTTP requests are forwarded in origin form with `Connection: close` and the judged host as
     their `Host` (never the client's own); credentials in a URL
     and `Proxy-Authorization` are never forwarded.
   - **Default lists** (`InstallHosts::default_hosts`), user-editable: npm
     `registry.npmjs.org`, `registry.yarnpkg.com`; Python `pypi.org`, `files.pythonhosted.org`;
     Go `proxy.golang.org`, `sum.golang.org`; Maven `repo.maven.apache.org`; Gradle
     `plugins.gradle.org`, `plugins-artifacts.gradle.org`, `services.gradle.org`,
     `repo.maven.apache.org`, `github.com`, `release-assets.githubusercontent.com`; .NET
     `api.nuget.org`; Composer `repo.packagist.org`, `api.github.com`, `codeload.github.com`;
     Bundler `rubygems.org`, `index.rubygems.org`. the GitHub hosts of Gradle
     and Composer are there because, measured with `curl -I` on 2026-10-07, the
     plugin portal sends jars to `plugins-artifacts.gradle.org` (303), the distribution service
     sends the wrapper's download to a GitHub release (307 to `github.com`, then 302 to
     `release-assets.githubusercontent.com`), and Packagist's packages download from
     `api.github.com` (302 to `codeload.github.com`). Without them the default lists fail for
     every Gradle wrapper and almost every Composer project.
   - **Settings.** The `install_registries` table of the user-level `settings.toml`
     (`npm = ["registry.npmjs.org", "127.0.0.1:4873"]`): a named list replaces its default, an
     unnamed one keeps it. The desktop edits it under Settings, Permissions, Install registries
     (one host per line, a bad entry shown and not applied, "Default" restores the list). The
     host reads the file when an install is about to run, so the latest list applies; never a
     project file a worker could write. A bad entry stops the install, naming the key.
   - **What the proxy does not stop**: a registry takes uploads as well as downloads, so a
     script holding its own registry token can publish to an allowed registry; a server that
     hosts several names at one address can be asked for another name inside an allowed
     tunnel; later plain HTTP requests on one connection reach the same allowed host. The
     user's own outgoing proxy (a company proxy) is not chained: an install behind one cannot
     reach the registry (open item).
   - **Mechanisms rejected.** Landlock's TCP port rules (ABI 4) limit ports, not hosts: a
     script could reach any host on the proxy's port. Anthropic's sandbox-runtime bridges the
     namespace with `socat` relays over a unix socket bound into it; here the seccomp filter
     refuses unix sockets, and a passed listener needs no helper program. A MITM proxy
     (`hudsucker`) would need a certificate the clients trust and pulls hyper and rustls.
5. **Package stores.** an install never writes the user's own stores, which
   every other project reads outside kGraf: Maven plugins, Gradle build scripts and MSBuild
   targets run during the install and could plant an artifact there. Each project gets its own
   stores under `.kgraf/package-stores/` (`kgraf_sandbox::PackageStores`): `go` (GOMODCACHE),
   `maven` (`-Dmaven.repo.local`), `gradle` (GRADLE_USER_HOME), `nuget` (NUGET_PACKAGES), the
   download caches of npm, Yarn, Bun, pip and Composer, and the install's own home folder. A
   later command of the project (a worker's shell, a check, a gate) reads the four stores
   read-only and is pointed at them by their variables, through `DependencyLinks::
   package_stores` (decision 0084, amended 2026-10-07); the Gradle wrapper's distribution is
   taken from the store when it is there. The detection counts an install found in a store.
   A store is used only once an install into it succeeded (kGraf writes
   `.kgraf-store-complete` in it then): a failed or half-done install leaves the user's own
   store in use. An install can write the stores, a link included, so kGraf's own writes there
   (the install's home folder, its npm configuration, the marker) refuse a link (`write_confined`,
   decision 0083). `JAVA_TOOL_OPTIONS` and `MAVEN_OPTS` are split on white space, so a store or
   home path with a space goes to the JVM as a link without one in the system temporary
   folder (`kgraf-jvm-<hash>`), owned by the user and pointing at the path; a link there that
   another user owns, or that points elsewhere, is never used (`kgraf_sandbox::jvm_path`).
6. **No credentials.** The install's `HOME` (and `USERPROFILE`, `XDG_*`) is an empty folder in
   the stores, so no package manager finds the user's `~/.npmrc`, `~/.yarnrc.yml`, `~/.pypirc`,
   `~/.netrc`, `~/.m2/settings.xml`, `~/.gradle/gradle.properties`, NuGet or Composer
   configuration; npm reads no user or global configuration file; pip reads no configuration
   file at all (`PIP_CONFIG_FILE=/dev/null`); the JVM gets `-Duser.home` (it takes its home from
   the password database, not `HOME`); the sandbox removes credential-shaped variables. The
   project's own configuration (a `.npmrc` in the project) is read, as it is the project's.
   Toolchain managers found from `HOME` (`CARGO_HOME`, `RUSTUP_HOME`, `PYENV_ROOT`,
   `RBENV_ROOT`, `ASDF_DATA_DIR`, `MISE_DATA_DIR`, `VOLTA_HOME`, `NVM_DIR`, `SDKMAN_DIR`,
   `COREPACK_HOME`) are pointed at the user's real folders when they exist and the variable is
   unset; none holds a registry login. A project that needs a private registry fails loud:
   the proxy names the blocked host, and the message says to allow it, that kGraf does not read
   login files, and to run the install by hand. Open item: a token the user sets for kGraf
   itself (kept with kGraf's secrets, injected by the proxy for one named host) is a design of
   its own and is not built.
7. **Windows** (decision 0011: the backend cannot deny the network). The install asks the same
   way and shows the command; it runs with the network open (`Network::Allow`, partial isolation
   accepted, `cmd /D /S /C`), its writes still confined to the checkout and the stores, `.kgraf`
   write-denied by an access entry for the checkout's capability (reads are not confined
   there), and the question, the notice and a failure say "network not limited". The backend
   refuses `Network::Proxy`, since nothing would keep a command on the proxy.
   `install::install_network(backend)` is the one decision, tested on every platform.
8. **More detection.** A Composer package whose `composer.json` requires a package
   (`vendor/name`) and has no `vendor/autoload.php` needs `composer install`. A Bundler project
   whose `.bundle/config` sets a relative `BUNDLE_PATH` that does not exist needs `bundle
   install`. Bundler without a project path installs into the Ruby installation, outside the
   project, and is not detected (open item).
9. **Commands.** `kgraf deps allow|refuse|reset npm` (or `python`, `go`, `maven`, `gradle`,
   `dotnet`, `composer`, `bundler`; the package managers' own names work too) stores the answer
   for each missing install of that package manager, with its exact command; `kgraf deps
   install [package-manager]` runs the missing installs now, once, storing nothing; `kgraf deps
   status` lists each missing install with its command and stored answer. Without a package
   manager, `allow`, `refuse` and `reset` are cargo's, as before. `--fetch-deps` and
   `--no-fetch-deps` cover installs too.

**Reuse.** Searched: this repository (no proxy or forwarder in Rust; the canary's forwarder is
Python test tooling), crates.io (CONNECT proxies `hudsucker` and `http-mitm-proxy` intercept TLS;
no allowlisting proxy without hyper), and Anthropic's sandbox-runtime
(github.com/anthropics/sandbox-runtime, Apache-2.0, read 2026-10-07: `src/sandbox/http-proxy.ts`,
`domain-pattern.ts`, `resolved-address-guard.ts`, `macos-sandbox-utils.ts`,
`linux-sandbox-utils.ts`, `sandbox-utils.ts`). Taken from it: the design (a host-side HTTP
proxy with the sandbox limited to its loopback port), the Seatbelt rule form, the host pattern
rules, the denied address classes, and the proxy variables (`NO_PROXY` emptied). Its code is
TypeScript on Node's HTTP stack, so the behavior is ported, not the code.

**Induced demand.** Unmeasured: an install that runs by itself is cheaper than one the user
runs, so more projects may have their installs run. Count installs per run (`kgraf diag show`,
registry fetches with trigger `install`) on the next live runs.

**Tests.** `kgraf-sandbox`: `egress.rs` (entries, denied addresses, forwarding, refusal named
and never contacted, tunnel, a late request, loopback resolution refused, the variables);
`tests/egress.rs` on macOS and Linux (`only_allowed_hosts_are_reached_and_only_through_the_proxy`,
`a_hidden_folder_is_untouchable_while_a_root_inside_it_stays_writable`,
`a_hidden_folder_outside_every_root_is_refused`); `package_stores.rs` (the completion marker,
the JVM link); `sandbox/mod.rs` `the_projects_own_stores_are_named_and_readable_never_writable`;
`windows.rs`
`a_hidden_folder_denies_the_roots_above_it_only`. `kgraf-host`: `deps/install/tests.rs`
(`a_real_install_succeeds_and_its_script_is_confined` against a local registry stub, whose
package's `postinstall` tries to write outside the tree, into `.kgraf` and into `.git/hooks`, and
to reach another host through the proxy and around it; `a_failing_install_shows_its_output_and_the_blocked_host`;
the consent rules: `without_a_consent_nothing_runs_and_the_question_names_the_command`,
`a_stored_consent_runs_the_install_once_without_asking`, `a_changed_command_asks_again`,
`a_refusal_goes_on_without_running`, `allow_for_this_run_runs_and_stores_nothing`,
`an_install_that_leaves_it_missing_fails_loud`; `a_link_an_install_left_in_the_stores_is_never_followed_by_kgraf`;
the settings table; the consent file version;
the install environment; the Windows rule); `deps_gate_tests.rs` (an unattended start without
an answer stops with the command; a stored answer installs before the run and is said; a run's
refusal goes on). `kgraf-shell`: the install card through the window, its words, the settings
round trip. `kgraf-cli`: `allow npm` stores the current command. The real-install tests need
`npm` and `tar` on PATH and say "skipped" without them.

## Amendment (2026-10-07, second): the install's consent, processes, variables and proxy, hardened

Each item closes a hole a test showed failing. This changes items 1, 2, 4, 6 and 7 of the
amendment above where it says so; the rest stands.

1. **An install answer counts only with kGraf's seal** (changes item 1). The consent file lives
   in the project's `.kgraf`, and a project's own files are not the user's word: a repository
   could commit `.kgraf/dependency-fetch.json`, or an archive unpack one, with an install
   "allowed" for the exact command kGraf would run, and kGraf ran it without asking (shown).
   Each stored install answer now carries `seal`: a keyed BLAKE3 hash of its key, command and
   answer under a random 32-byte key kept per user, outside every project, in kGraf's data
   folder (`install-consent.key`, made on first use, owner-only; `KGRAF_CONSENT_KEY_PATH` names
   another file). An answer without a valid seal, with another user's seal, or with a seal made
   for another answer reads as unset, and the run asks. The key is read-denied to every
   sandboxed command on macOS (kGraf's data folder is on the deny list) and never granted on
   Linux; Windows does not confine reads (decision 0011), but a command there cannot write the
   consent file either. Cargo's registry answer is unchanged: a committed one allows a fetch
   that runs no package code. `ConsentStore::with_key` and `Host::with_consent_key` give
   another crate's tests their own key, so no test makes or reads the user's file.
2. **"Allow once" answers its own question only** (changes item 2). One per-run grant covered
   both questions, so "Allow once" on cargo's fetch also ran the project's install scripts,
   whose question was never shown (shown), and "Allow once" on the install allowed workers'
   fetches on the host. The grants are now apart (`Host::grant_dependency_fetch`,
   `grant_dependency_install`, `grant_dependency_once`). The desktop starts a run again under a
   new run id after each answer, so it gives each earlier "Allow once" of the same start again
   to the new id (`FetchPrompt::once`), and two questions are asked once each. `--fetch-deps`
   and `--no-fetch-deps` still cover both, as item 9 says.
3. **No process outlives the install** (changes item 2's bounds). An install script that left
   its process group (its own `setsid`) kept running after kGraf took the install as done,
   with the checkout writable (shown on macOS). Every process of the install now holds a shared
   lock on a hold file in `.kgraf` (`SandboxPolicy::holding`), which the install can neither
   read nor replace. When the install's first process exits, its group is killed again and
   again for at most 250 ms, until the hold is free; a holder left after that left the group.
   On macOS each holder is named with `lsof` (seen twice, 100 ms apart, never kGraf itself) and
   killed, and the install fails loud either way ("left its process group"). On Linux the
   install is PID 1 of its own PID namespace, and the kernel ends every process left in it.
   What still escapes on macOS: a process that closes the descriptors it inherited and leaves
   the group, decision 0084's known gap. The install's environment keeps the build daemons
   that do exactly that from starting: `GRADLE_OPTS=-Dorg.gradle.daemon=false` and
   `MSBUILDDISABLENODEREUSE=1`. `lsof` is the one public way on macOS to name
   the holders of a file; it runs only when a holder is left.
4. **Package-manager configuration in the environment is removed** (changes item 6). The
   sandbox removes credential-shaped names, but several package managers take configuration
   that can hold a login from variables whose names do not look like one: `PIP_INDEX_URL`
   and `UV_INDEX_URL` (an index URL with a password), Bundler's per-host credentials
   (`BUNDLE_GEMS__EXAMPLE__COM=user:pass`), `VSS_NUGET_EXTERNAL_FEED_ENDPOINTS` (feed tokens),
   `GOPROXY`, Gradle project properties, git's command-line configuration (an
   `http.extraHeader` with a token). An install inherits none of the variables starting with
   `PIP_`, `UV_`, `POETRY_`, `PDM_`, `PIPENV_`, `TWINE_`, `NPM_CONFIG_`, `YARN_`, `PNPM_` (not
   `PNPM_HOME`), `BUN_` (not `BUN_INSTALL`), `BUNDLE_`, `COMPOSER_`, `NUGET_`, `VSS_NUGET_`,
   `ORG_GRADLE_PROJECT_`, `GIT_CONFIG`, `CARGO_REGISTRIES_`, `CARGO_REGISTRY_` (any case), nor
   `GOPROXY`, `GONOPROXY`, `GOPRIVATE`, `GONOSUMDB`, `GONOSUMCHECK`, `GOINSECURE`, `GOFLAGS`,
   `GOAUTH`, `GRADLE_OPTS`, `_JAVA_OPTIONS`, `JDK_JAVA_OPTIONS`, `MAVEN_ARGS`, `GIT_ASKPASS`,
   `SSH_ASKPASS`, `NETRC`; kGraf then sets its own values (`install::inherited_configuration`).
   Toolchain variables (`PATH`, `JAVA_HOME`, `GOROOT`, `CC`, the toolchain homes) stay.
5. **The proxy's connections fit the descriptor limit** (changes item 4's cap). Each
   connection can hold six descriptors in kGraf's own process. A process started from the
   macOS Finder or a default terminal has a soft limit of 256, and idle connections under the
   fixed cap of 256 left the proxy's process unable to open a file (measured: "Too many open
   files" after 156 client connections in one test process). The cap is now a quarter of the
   soft limit divided by six (10 at a limit of 256), at least 4 and at most 256. A client past
   the cap is not refused with 503 (a package manager backs off for minutes, or gives up); it
   waits in the listen backlog until a connection ends.
6. **The Windows question says reads are open too** (changes item 7). The question ended
   "kGraf never reads your credential files" on every platform. On Windows the sandbox
   confines neither reads nor the network, so an install script there can read the user's
   credential files and send them anywhere. The Windows question now says so; the notice and a
   failure say "its reads were not confined either"; Settings says it beside the host lists.
   Elsewhere the question says the credential files are not readable to the install.
7. **A folder name cannot rewrite the question.** The folder of an install comes from the
   repository and is shown on the terminal before the person answers. A control character in
   it (an escape sequence that clears the line) is shown escaped (`MissingInstall::place`).
8. **What a changed lockfile does.** Stated in the question and the code, unchanged in
   behavior: an answer is for the exact command; a changed lockfile or package script does not
   change the command, and what it runs, it runs in the sandbox. Binding the
   answer to the lockfile would ask on every dependency update, and with item 2 of the
   amendment above it would disable every schedule of the project each time; the sandbox, not
   the answer, is the defense against a script that changed.

Judged not to need a change, each with its test or measurement: host matching (case, trailing
dot, a non-ASCII name, an address literal in `CONNECT`, `host:port@other`, userinfo in an
absolute URL: the host judged is the host dialed); an allowed name that resolves to loopback,
link-local or a metadata address (refused after resolution, the address checked is the address
dialed); the other ways out of the macOS sandbox, each probed live (UDP to an outside
address, a name lookup, the resolver's socket, another loopback port, binding a port, raw and
ICMP sockets, a routing socket: each refused; a hard link out of `.kgraf` or onto `.git/config`:
refused); the Linux handoff (the listener is bound in the command's fresh namespace before the
command runs, so nothing can take the port first, and it is closed in the command after it is
sent; without user namespaces the policy is refused). Not changed, and said: a tunnel to an
allowed host can carry another name in its TLS handshake, and later plain HTTP requests on one
connection reach the same allowed host (item 4 of the amendment above).

**Tests.** `kgraf-host` `deps/install/tests.rs`: `a_consent_kgraf_did_not_write_is_no_answer`,
`a_seal_from_another_key_or_another_answer_is_no_answer`,
`an_install_process_that_leaves_its_group_is_stopped_and_fails_the_install` (macOS and Linux;
needs `perl`), `package_manager_configuration_from_the_environment_is_removed`,
`the_windows_question_says_an_install_script_can_read_and_send_any_file`,
`a_control_character_in_a_folder_name_is_shown_escaped`; `deps_gate_tests.rs`:
`allowing_the_fetch_once_does_not_allow_the_install`,
`allowing_the_install_once_does_not_allow_the_fetch`. `kgraf-sandbox` `egress.rs`:
`the_connection_cap_keeps_to_a_quarter_of_the_descriptor_limit`,
`a_client_past_the_cap_waits_and_is_then_served`,
`look_alike_names_never_match_an_allowed_host`. `kgraf-shell`:
`allow_once_on_the_fetch_card_does_not_run_the_install`,
`an_earlier_allow_once_is_given_again_to_the_run_that_starts`.

## Amendment (2026-10-07, third): a process left behind is named in kGraf's own process, and a stop is said only when one happened

Changes item 3 of the amendment above, on macOS.

1. **The holders of the hold file are read from the process table in kGraf's own process**
   (`kgraf_sandbox::lifecycle::holders`: libproc, each open file matched by device and inode),
   not with `lsof`. One scan of about 500 processes took 0.1 s, and 0.23 s with four CPU-bound
   processes per core; one `lsof` run took 0.4 s idle and 0.5 to 1.1 s under that load, and it
   ran twice, 100 ms apart. A process that left the group goes on with its work while kGraf
   looks for it: under that load a script that waited 2 s and then wrote into the checkout
   wrote first in 4 of 8 runs, before kGraf killed it.
2. **Each holder is killed the moment it is named**; the listing is tried again until the hold
   is free or the grace (5 s) runs out. The second look 100 ms later, which kept a descriptor
   copied by another thread's fork from being taken for a holder, is replaced by a rule: a
   child of kGraf is never named. Such a copy is only ever in a child of kGraf, and gone at its
   exec; an install process is never a child of kGraf once the install's first process is
   reaped.
3. **A stop is said only when one happened.** A hold that freed with nobody killed was
   reported as "kGraf stopped it (process )": the process had ended by itself, after its work.
   That case now says the process ended by itself before kGraf found it and may have changed
   files. A listing that fails is named in the failure.

**Tests.** `kgraf-host` `deps/install/tests.rs`:
`a_holder_that_ends_unnamed_is_not_reported_as_stopped`,
`a_listing_that_fails_leaves_the_holder_reported_as_running`,
`a_holder_that_left_the_group_is_killed_before_its_work`, and
`an_install_process_that_leaves_its_group_is_stopped_and_fails_the_install`, whose script now
leaves the group before its parent exits (else the group kill can end it first and nothing
escapes) and which checks that the stop names a process. `kgraf-sandbox` `lifecycle.rs`:
`holders_names_a_detached_holder_and_not_a_child_or_this_process`.

## Amendment (2026-10-07, fourth): a process that leaves the install's group is found while the install runs

Changes items 1 and 2 of the amendment above, on macOS.

1. **A scan after the install's first process exits came too late under heavy load.** With
   16 CPU-bound processes on 16 cores and the host test suite running in the same process
   (load average about 120), the scan and kill after the exit took 1.26 s, and a process that
   left the group and wrote 1 s later wrote first. Any scan that starts at the exit races the
   escaped process's own work.
2. **A watch runs while the install runs.** A helper thread scans every 50 ms for processes
   that hold the install's hold file and are not in the install's process group, and kills
   each at once; the install is then stopped and fails. The install's first process leads its
   own session and group (the sandbox calls `setsid`), and every process it starts stays in
   that group unless it calls `setsid` or `setpgid` itself. So a holder in the group is one of
   the install's own processes and is never killed; a holder in another group is one a group
   kill cannot reach. kGraf and its children are never named. A process that leaves the group
   holds the hold file from its fork (it inherits the descriptor), so the watch sees it as
   soon as it has left.
3. **The watch reads only processes started since the install began**
   (`kgraf_sandbox::lifecycle::holders_started_since`): one cheap read of each process's table
   entry, and the descriptor list only of a new process. With about 520 processes and 16
   CPU-bound processes on 16 cores, a scan took 0.5 to 42 ms where the full scan took 0.21 to
   0.63 s. A process that the clock leaves out (a clock stepped back by more than the 1 s
   margin) is still found by the full scan after the exit, which stays as the final sweep and
   waits for the hold to be free.
4. **The failure names every process killed**, during the run and after it. A scan that fails
   during the run fails the install, with the scan's error: kGraf could not watch, so a
   process may have changed files unseen.

**Tests.** `kgraf-host` `deps/install/tests.rs`:
`a_holder_that_left_the_group_is_killed_before_its_work` now runs a real install whose first
process sleeps 30 s while a process that left the group writes 300 ms after it left; it fails
with the watch disabled (the install passed and the file was written).
`the_watch_kills_only_a_holder_outside_the_group`,
`a_process_killed_while_the_install_ran_is_named`,
`a_holder_still_running_after_a_kill_is_reported_with_the_kill`,
`a_failed_watch_fails_the_install`. `kgraf-sandbox` `lifecycle.rs`:
`holders_started_since_names_new_holders_with_their_group`.
