# kGraf usage guide

The full command reference for the `kgraf` command line and the `kgraf-app` desktop
window. The [README](../README.md) has the short version.

## Build

```
cargo build --workspace
cargo test --workspace
```

The workspace needs Rust 1.95 or later. It builds two programs: `kgraf` (the command
line, `crates/kgraf-cli`) and `kgraf-app` (the desktop shell,
`crates/kgraf-shell`).

## Set up a project

Workers build and test with the network off (decision 0084), so a run needs the project's
dependencies in the local cargo cache before it starts. Answer once per project whether
kGraf may fetch them (decision 0087):

```
kgraf deps allow     # fetch from crates.io whenever a run needs it, without asking again
kgraf deps status    # the stored answer, and whether the dependencies are ready offline
kgraf deps fetch     # fetch now, once
kgraf deps refuse    # never fetch; a run that needs a fetch stops before it starts
kgraf deps reset     # forget the answer; the next run that needs a fetch asks
```

- The answer is kept in `.kgraf/dependency-fetch.json`, outside the repository's tracked
  files. It is per project.
- At every run start (the command, scheduled and tray-fired runs, the desktop app) kGraf
  checks offline whether the dependencies are ready (about a third of a second on a large
  workspace). Only when one is missing does the answer matter: allowed runs `cargo fetch` on
  this machine, outside the workers' sandbox, and says which index it contacted; refused
  stops the run with what to do.
- With no answer, `kgraf orchestrate` at a terminal asks once (once for this run, always for
  this project, not now, never for this project), and the desktop app shows the question
  as a card (Allow once, Always for this project, Not now). With nobody there to answer (a
  scheduled run, a script), the run stops before it starts and says how to answer; a
  schedule is turned off with that reason until you answer and run `kgraf schedule enable`.
  kGraf never fetches unasked.
- `--fetch-deps` and `--no-fetch-deps` on `orchestrate` and `schedule run-now` answer for
  one run and store nothing.
- A worker that adds a crates.io dependency gets it fetched when the project allows
  fetching; otherwise its build fails and says why. At most 8 fetches and 32 checks per run
  for its workers, and 3,000 packages per fetch; a fetch still running stops when the run
  ends. A refusal you store during a run (`kgraf deps refuse`) applies to the next worker
  build, even in a run started with `--fetch-deps` or "Allow once".
- Only crates.io. A git dependency or another registry in any `Cargo.toml` of the project,
  a workspace member or not, stops the fetch: run `cargo fetch` yourself if you trust it.
  `cargo fetch` downloads and unpacks; it runs no build script. kGraf's fetch never uses a
  git transport, so a crates.io index you configured as a git index cannot be fetched by it.
- With no `Cargo.toml` at the project root but one below it, nothing is checked or fetched,
  and each run start says so.
- Workers cannot merge changes to a `.cargo` folder or a `rust-toolchain` file: make those
  by hand. The same holds for the other build tools' and version managers' configuration:
  `.npmrc`, `.yarnrc*`, `.pnpmfile.cjs`, `.mvn/`, `gradle.properties`, `gradle/wrapper/`,
  `init.gradle`, `pip.conf`, `pip.ini`, `.pypirc`, `NuGet.Config`, `Directory.Build.props`,
  `Directory.Build.targets`, `.bundle/config`, `.tool-versions`, `.python-version` and
  `.nvmrc`. Project manifests (`package.json`, `pom.xml`, `go.mod`, `go.work`, a `.csproj`)
  are ordinary files (0084).
- Each fetch is a network event with no tokens; `kgraf diag show` counts them.

For the other languages, the dependencies must be installed in the checkout, as you would to
run the tests by hand (`npm ci`, `pnpm install`, `yarn install`, a `.venv` with
`pip install -r requirements.txt`, `go mod download`, `mvn test` once, `dotnet restore`,
`composer install`, `bundle install`). Install them yourself, or let kGraf run the install
command in its sandbox, with your consent, once per project (decision 0087, amended
2026-10-07):

```
kgraf deps status          # each missing install, its exact command and the stored answer
kgraf deps allow npm       # run npm's missing install in the sandbox whenever a run needs it
kgraf deps install         # run every missing install now, once (or: kgraf deps install npm)
kgraf deps refuse npm      # never run it; runs go on without it and say so
kgraf deps reset npm       # forget the answer; the next run that needs it asks
```

A worker never downloads packages itself. Its shell has the network off, and a command that
downloads (`pip install`, `npm ci`, `go get`, `cargo fetch`, `cargo install`, and the install
forms of uv, pipx, poetry, conda, pnpm, yarn, bun, gem, bundler, composer and `dotnet`) is
refused in every permission mode, Bypass included, whatever is granted (decision 0087, amended
2026-10-09). The refusal names the setting that allows the work: `kgraf deps allow` for
crates.io, or a package declared in the project's dependency files with `kgraf deps allow
<name>` (or `--fetch-deps` for one run), or an install you make before the run. A worker that
reports it short holds its contract with that reason; nothing is granted. Only a run whose
commands execute in a disposable remote environment lets a worker download.

The package managers are `npm` (also pnpm, Yarn and Bun), `python`, `go`, `maven`, `gradle`,
`dotnet`, `composer` and `bundler`.

- **One answer per command.** The answer is kept for the exact command, per package and
  folder (`npm ci` in the project root). If the command changes (a lockfile is added, another
  package manager is used), the next run asks again. A changed lockfile or package script
  does not change the command: what it runs, it runs in the sandbox. The answer is sealed
  with a key kept in kGraf's own data folder (`install-consent.key`), so an answer a
  repository brings in its `.kgraf` folder counts for nothing, and the run asks.
- **"Allow once"** answers the question it is on only. A run that needs cargo's fetch and an
  install asks both in turn; allowing the fetch once does not run the install.
- **At run start**, after cargo's check: with an answer, kGraf runs the command and says so
  (`installed  npm dependencies ...: ran `npm ci` in the project root in kGraf's sandbox ...
  reached registry.npmjs.org:443`). With no answer, `kgraf orchestrate` at a terminal
  asks once (once, always, not now, never) and the desktop app shows a card (Allow once,
  Always for this project, Not now); with nobody there to answer, the run stops before it
  starts and names the command and how to allow it. A refusal (stored, or `--no-fetch-deps`)
  lets the run go on without the install, with a notice. `--fetch-deps` allows it for one
  run. kGraf never installs unasked.
- **The sandbox.** The install runs in the project's folder, in kGraf's sandbox: it can write
  only the project (never its `.git` or `.kgraf` folder, which it cannot read either) and
  kGraf's package cache for the project (`.kgraf/package-stores/`). Install scripts (npm's
  `postinstall`, a `setup.py` build, Maven and Gradle build logic) run inside the same
  sandbox. The install and its scripts can still change the project's own files, as when you
  run the install yourself. No process the install starts outlives it: one that leaves the
  install's process group is stopped, and the install fails and says so. Gradle's daemon and
  MSBuild's reused nodes are turned off for the install.
- **The network.** On macOS and Linux the install reaches only the package registries of its
  package manager, through kGraf's proxy: `registry.npmjs.org` and `registry.yarnpkg.com`;
  `pypi.org` and `files.pythonhosted.org`; `proxy.golang.org` and `sum.golang.org`;
  `repo.maven.apache.org`; for Gradle `plugins.gradle.org`, `plugins-artifacts.gradle.org`,
  `services.gradle.org`, `repo.maven.apache.org`, `github.com` and
  `release-assets.githubusercontent.com` (the wrapper's download); `api.nuget.org`; for
  Composer `repo.packagist.org`, `api.github.com` and `codeload.github.com` (package
  downloads); `rubygems.org` and `index.rubygems.org`. Change a list in the desktop app under
  Settings, Permissions, Install registries (one host per line; `*.example.com` for every name
  under a domain; `host:port` for a port other than 443 and 80), or in the user-level
  `settings.toml`:

  ```toml
  [install_registries]
  npm = ["registry.npmjs.org", "npm.example.com", "127.0.0.1:4873"]
  ```

  A list you name replaces its default; the others keep theirs. On Linux this needs user
  namespaces; without them the install stops and says so. **On Windows** the sandbox cannot
  limit the network or reads (decision 0011): the install runs with the network open, and it
  and its scripts can read any file you can, your credential files included, and send it
  anywhere. Its writes are still confined. The question and the notice say so.
- **Fail loud.** A failed install stops the run with the end of its output. A host the proxy
  blocked is named, with how to allow it (`kGraf blocked npm.example.com:443: ...`).
- **No credentials.** The install runs with a home folder of its own, so it never reads your
  `~/.npmrc`, `~/.pypirc`, `~/.netrc`, Maven `settings.xml` or other login files (on macOS
  and Linux the sandbox also denies them), and pip reads no configuration file. Package
  manager settings in your environment (`PIP_INDEX_URL`, `npm_config_*`, `BUNDLE_*`,
  `GOPROXY` and the like, which can hold a login) are not passed to it. A project that needs a private registry with a login fails
  with the blocked host named: allow the host and run the install yourself. A way to give
  kGraf a registry token of its own is not built yet.
- **Package stores.** Go, Maven, Gradle and NuGet install into a store, not the project.
  kGraf's install fills a store of the project's own in `.kgraf/package-stores/` and never
  writes your `~/go/pkg/mod`, `~/.m2`, `~/.gradle` or `~/.nuget`; workers, checks and gates
  of the project read that store, read-only.
- Each install is a network event with no tokens; `kgraf diag show` counts it with the
  registry fetches.

- Workers, the verifier's check, the gates and the contract checks read what you installed,
  and never write it. Each installed tree beside its project's manifest is linked into the
  worktree at the same place: `node_modules/` (and Yarn's `.pnp.cjs`, `.pnp.loader.mjs`,
  `.pnp.data.json` and `.yarn/`) beside a `package.json`, a `.venv/` or `venv/` with a
  `pyvenv.cfg`, `vendor/` beside a `go.mod`, `composer.json` or `Gemfile`, and
  `vendor/bundle/` beside a `Gemfile`. A write through a link is refused. The links are never
  committed and are not reported as anyone's files (0084).
- In an npm, pnpm or Yarn workspace, whose `node_modules` links its own packages
  (`node_modules/@scope/web -> ../../packages/web`), the worktree gets a `node_modules` folder
  of links instead: each workspace package resolves to the worktree's copy, so tests run the
  worker's edits, and every other package to your install (0084). A pnpm install with
  workspace packages gets a copy-on-write clone of your `node_modules` instead (APFS, or a
  Linux file system with reflinks): pnpm links workspace packages inside
  `node_modules/.pnpm` too, and only in a clone do those links, all relative, resolve to the
  worktree's copies. A clone costs no space until written and about 0.12 ms per file (24,000
  files: 2.8 s, macOS), once per worktree and once per check. Where no clone can be made
  (Linux on ext4, tmpfs or overlayfs, which includes most containers; another volume), the
  install is copied byte for byte if it holds at most 100,000 entries and 1 GiB: about 0.5 s
  for 30,000 entries (188 MiB, Linux), and its full size on disk in each worktree and check.
  The output says it was copied and how large the copy is. A larger install gets the folder
  of links, and a failing check says that such nested links reach your copies.
- The folder made from your `node_modules` is kept until the install changes. A change less
  than one second old is not trusted to show in the folder times (Linux keeps them in steps
  of about 1 ms), so a command started in that second makes the folder again.
- On Windows the links are directory junctions, which need no privilege, and the
  Plug'n'Play files are copied; a write through a junction is refused by the sandbox's
  access control, as through a link elsewhere. A pnpm install gets the folder of
  junctions (no clone). Built and compile-checked; not yet run on Windows.
- The virtual environment of the folder a command works in is activated: the `.venv/` or
  `venv/` of that folder or the nearest one above it, the root one by default, the one of
  `svc/api` for `cd svc/api && python3 -m pytest`. `python3`, `pytest` and the environment's
  other programs are the environment's, run from your checkout's folder.
- The package stores in your home folder are readable, never writable: `~/.m2/repository`,
  `~/.gradle/caches`, `~/.gradle/wrapper/dists`, `~/.nuget/packages`, `~/go/pkg/mod`,
  `~/.npm`, `~/.cache/pip` and `~/.cargo/registry`. The configuration and credential files
  beside them are not: `~/.m2/settings.xml`, `~/.m2/settings-security.xml`,
  `~/.gradle/gradle.properties`, `~/.gradle/init.d`, Gradle's Develocity keys, NuGet's
  `NuGet.Config`, `~/.npmrc`, pnpm's and Bun's global configuration, pip's `pip.conf`,
  Poetry's `auth.toml` and uv's credential store (macOS: also at their `Library` locations),
  and cargo's `credentials.toml`. Gradle and the .NET CLI get a home folder of their own in
  the command's temporary folder, with your Gradle dependency cache and NuGet package folder
  read-only; the .NET CLI's telemetry is off. A Gradle wrapper finds its distribution, when
  you have unpacked it once (`./gradlew --version`), through that home, read-only (on
  Windows through a junction; not yet run there).
- On Linux, a Rust toolchain outside your home folder is readable too, when `RUSTUP_HOME`
  or `CARGO_HOME` names it with an absolute path: the whole `RUSTUP_HOME`, and the `bin`,
  `registry` and `git` folders of `CARGO_HOME`, never `CARGO_HOME` itself, which holds
  `credentials.toml`. A value that is relative, missing, the root, your home folder or a
  folder above it is ignored, and the failing command's output says so. On macOS a
  `CARGO_HOME` outside your home folder keeps its `credentials` and `config` files unreadable.
- The verifier's check, the gates and the contract checks each get a new temporary folder,
  removed when they end: tool settings a worker left in its own scratch directory (an
  `.npmrc`, a Gradle init script, a Go build cache, Python bytecode) never reach a check.
- Nothing a worker's run built is an input of its verifier check. The check's cargo build
  folder is a new folder of its own, outside the worker's reach, never the worker's build
  folder: cargo trusts the build records it finds, and forged ones (a test binary replaced
  by one that exits 0, its records dated in the future) made a check pass broken code. The
  folder's `debug` profile starts as a copy-on-write clone of your checkout's
  `target/debug` when you have one (decision 0060), so only the project's own crates are
  built again: on this repository
  (`cargo test -p kgraf-sandbox --no-run`, macOS, 2 jobs) 0.7 s to clone and 5.6 s to build,
  against 19.9 s cold and 0.2 s when it reused the worker's folder; a two-crate project
  0.4 s, against 0.5 s cold. Without a `target/` the check builds cold. And before the check
  runs, every file git ignores is removed from the worktree (a Gradle `build/` or
  `.gradle/`, .NET `obj/` and `bin/`, a `*.tsbuildinfo`, a worker's own install): each
  build tool takes such files as work already done. The links to your installs are made
  again. A check therefore needs what the merged tree needs, as the gate does.
- When the project declares dependencies that are not installed, the run start asks to run
  the install (above), or says which are missing when you refused; so does a failing check or
  gate, after its own output (`[dependencies] npm dependencies are not installed:
  node_modules has no jest. Run `npm ci` in the project root, or let kGraf run it in its
  sandbox (`kgraf deps allow npm`) ...`).
- Not covered: a link inside `node_modules` to a folder of the checkout that the worktree
  does not have (an untracked package) stays unreadable.

## The code graph

Point it at a repository, including this one:

```
cargo run -p kgraf-cli -- index
cargo run -p kgraf-cli -- symbol slice_for_symbol
cargo run -p kgraf-cli -- slice slice_for_symbol --budget 1500
cargo run -p kgraf-cli -- query 'kind=function -calls-> name=stable_id'
cargo run -p kgraf-cli -- cochange --commits 500
cargo run -p kgraf-cli -- partners ROADMAP.md
```

`-C <dir>` points any command at another repository.

Only `kgraf index` creates or rebuilds the index. `symbol`, `slice`, `query`, `partners` and
`demand` open it read-only, and every other command opens an existing index without
rebuilding it. Where there is no index, or it was written by another schema version, they
stop and say to run `kgraf index`. A slice reads each node's lines from the file, and it
stops with an error when the file changed after it was indexed; run `kgraf index` first. (A
worker in a run is different: a slice it cannot read is left out with a notice, see
`kgraf orchestrate`.) A
name that matches more than one node is refused with the candidates listed (an exact
qualified name still picks its one node).

An index pass skips `target`, `node_modules`, `.git` and similar tool output at any depth,
and `build`, `dist`, `venv` and the like only at the top level, so `src/build/` is indexed.
An index pass also honors the project's `.gitignore` files (the root file and any file in a
subfolder) and the root's `.git/info/exclude`, so a file git ignores is not indexed. It does
not read your global git excludes, a `.ignore` file, or a `.gitignore` in a folder above the
project, so the same repository gives the same index on every machine. Folders that start with
a dot, such as `.github`, are indexed; only `.git` is always skipped. A file that becomes
ignored loses its rows at the next pass.
It prints the excluded directories, files over 2 MiB, files whose bytes are not UTF-8, and
any symbolic link or unreadable file whose indexed rows it kept.

The graph chunks Rust, Python, TypeScript, TSX, JavaScript and Markdown with kGraf's own
rules, and Go, Java, C, C++, Ruby, C# and PHP from each tree-sitter grammar's own tags
query (the tags tier; a slice marks those members as resolved by name only). A call links
only to a definition in its own language family. When the extractor for a language
changes, the next index re-derives that language's files and says so.

## What the roadmap says remains: presumed work nodes

```
cargo run -p kgraf-cli -- index                              # also reads the roadmap
cargo run -p kgraf-cli -- plan                               # list presumed work and its state
cargo run -p kgraf-cli -- plan --from-roadmap --source docs/milestones.md
```

Every `kgraf index` reads the project's roadmap documents (ROADMAP, TODO, PLAN,
MILESTONES, BACKLOG or TASKS, `.md` or `.markdown`, in the root or `docs/`) into
**presumed** work nodes, with no model call: status tables, task lists and status
headings. Each node shows presumed done, partly done or remaining, derived from what the
document claims and whether the code it names exists, and says where the two disagree.
Re-running updates the same nodes; it never duplicates them, never deletes them, and never
overwrites a node you changed. When an approved plan takes an item over, the presumed node
keeps its identity and the galaxy draws the running work on it. `--no-presume` skips the
read; `kgraf plan --from-roadmap` with no roadmap found is an error.

## Providers, spend and the ledger

[MODEL-SUPPORT.md](MODEL-SUPPORT.md) lists which routes and model kinds have run against the live
service and which are built but untested. Read it before you choose a route.

Set a route's key **in your own shell**; it is never read from a file:

```
export KGRAF_KIMI_API_KEY=...        # the route's own variable; never paste a key into a chat
cargo run -p kgraf-cli -- providers
cargo run -p kgraf-cli -- call --provider kimi --model <model-id> --record kimi.json
cargo run -p kgraf-cli -- spend --prices prices.json
cargo run -p kgraf-cli -- unreconciled
cargo run -p kgraf-cli -- unreconciled resolve <call-id> --input 120 --output 30
cargo run -p kgraf-cli -- unreconciled resolve <call-id> --no-charge
cargo run -p kgraf-cli -- unreconciled resolve <call-id> --output 30 --abandoned
cargo run -p kgraf-cli -- ledger verify
cargo run -p kgraf-cli -- demand
```

Every model call goes through the ledger. `spend` totals it (in dollars with a price
table; `total unpriced` when no recorded model has a price), `unreconciled` lists calls
whose spend is not settled and shows the partial usage recorded for each (a lower bound),
`unreconciled resolve` closes one with the usage the provider's own invoice shows (or
`--no-charge` when the provider confirms it billed nothing). A field you leave out keeps the
partial figure already recorded. A call listed as `in_flight` may still be live, so resolve
refuses it unless you pass `--abandoned` to state that its process is gone. There is no bulk
resolve: each call needs its own figure. `ledger
verify` walks the
hash chain, and `ledger redact <call-id> --reason ...` removes a payload captured by
accident while the chain still verifies. `demand` reports induced demand: whether
runs offered workflow templates started more work than runs without. It claims no saving.

### AWS Bedrock

The `bedrock` route speaks Bedrock's Converse and ConverseStream operations, signed with
AWS Signature Version 4 (decision 0091). It is listed as `unverified`: it has been tested
against recorded-shape responses only, never against the service. It takes the standard AWS
configuration, not a `KGRAF_*_API_KEY` variable. Set it in your own shell. A Bedrock API
key is the simplest credential; kGraf sends it as a bearer token, unsigned, and it wins over
access keys when both are set:

```
export AWS_BEARER_TOKEN_BEDROCK=...   # a Bedrock API key; still needs a region, below
```

Or use access keys:

```
export AWS_ACCESS_KEY_ID=...
export AWS_SECRET_ACCESS_KEY=...
export AWS_SESSION_TOKEN=...          # only for temporary keys
export AWS_REGION=us-east-1           # or AWS_DEFAULT_REGION, or `region` in the profile
```

Or name a profile with static keys in `~/.aws/credentials` (`AWS_PROFILE=work`;
`AWS_SHARED_CREDENTIALS_FILE` and `AWS_CONFIG_FILE` move the files). The variables win over a
profile. A profile that uses SSO, role assumption, a credential process or web identity is
refused, because each needs a network exchange kGraf does not make: export temporary keys
first, for example with `aws configure export-credentials --profile work --format env`.

Credentials and region are read again on every call. `kgraf providers` shows the route, and
a missing key or region is reported when the route is checked, before any call. The model id
you configure is the Bedrock model id or inference profile id (for example one ending in
`-v1:0`); the adapter refuses a model outside the set you configured before it sends anything.

What to expect:
- Input excludes cache reads and writes (additive). The reply's `totalTokens` is checked
  against the parts; a mismatch keeps the answer and adds a `kgraf_note` to the call's raw
  usage, because the call was billed.
- A reply served on a service tier other than `default` (`priority`, `flex`, `reserved`), or
  with cache writes on a TTL other than `5m`, keeps its answer and token counts, records the
  tier in the raw usage, and shows as unpriced: the price table has one rate per model.
- Redirects are not followed. A non-finite temperature is refused before anything is sent.
- Blank text is never sent (the service rejects it); an empty tool result goes as
  `(no output)`.
- A `424` reply (an error while the model processed the request) leaves the call unreconciled,
  since the service does not say it is unbilled.
- The region must be lowercase letters, digits and hyphens, because it becomes part of the host
  name.
- A model id the price table does not list (an inference profile id such as
  `us.anthropic...-v1:0` is a different key from the bare model id) shows as unpriced, never
  as zero cost.
- The service does not name the model in its reply, so a substituted model cannot be caught
  by the model check.
- Cache points are off for this route and cannot be turned on in configuration. They work on
  some models only; the service rejects them on the rest.
- A request that sets a reasoning effort is refused: Converse has no model-independent field
  for it.
- A private (VPC) endpoint cannot be set yet; the route uses
  `https://bedrock-runtime.<region>.amazonaws.com`.
## Google routes: Gemini and Vertex AI

Three routes reach Google models. All three are marked `unverified` in `kgraf providers`:
their behavior comes from Google's published references and recorded test bodies, and none
has been run against the live service. Check the recorded raw usage on the first call.

| Route | What it reaches | Credential |
|---|---|---|
| `gemini` | Gemini through its own `generateContent` API, with tool calling, streaming and a thinking control | `KGRAF_GOOGLE_API_KEY`, the key the `google` route also reads |
| `vertex-gemini` | Gemini on Vertex AI | Vertex settings, below |
| `vertex-anthropic` | Anthropic models on Vertex AI | Vertex settings, below |

The `google` route is Gemini's OpenAI-compatibility layer, which ignores parameters it does not
support. Use `gemini` when you need the thinking control or thought summaries.

**Gemini with an API key.** Set the key in your own shell and pick a model id:

```
export KGRAF_GOOGLE_API_KEY=...
cargo run -p kgraf-cli -- call --provider gemini --model gemini-2.5-flash
```

**Vertex AI (both routes).** Vertex takes an OAuth token, not an API key. Set where the
credentials come from, the project that is billed, and the region. There is no default region:
the region sets where data is processed and which models exist.

```
export KGRAF_VERTEX_CREDENTIALS=adc          # or metadata, gcloud, or a service-account key file path
export KGRAF_VERTEX_PROJECT=my-project       # optional with a key file: its project_id is used
export KGRAF_VERTEX_REGION=us-east5          # or `global`
cargo run -p kgraf-cli -- call --provider vertex-gemini --model gemini-2.5-flash
cargo run -p kgraf-cli -- call --provider vertex-anthropic --model <name>@<version>
```

Credential sources. kGraf uses only the one you name and never probes the others:

| Value | Source |
|---|---|
| `adc` | the service-account key file that `GOOGLE_APPLICATION_CREDENTIALS` names, if it is set; otherwise the gcloud user credentials file (`gcloud auth application-default login`). It does not contact the metadata server and does not run `gcloud`. |
| `metadata` | the metadata server of the Google Cloud host kGraf runs on |
| `gcloud` | the output of `gcloud auth print-access-token` |
| a path | a service-account key file |

Other credential file types, such as `external_account` (workload identity federation) files,
are not supported. A key file whose name is `adc`, `metadata` or `gcloud` is written `./adc`.
Where a kGraf variable is not set, kGraf reads `GOOGLE_APPLICATION_CREDENTIALS`,
`GOOGLE_CLOUD_PROJECT` (or `GCLOUD_PROJECT`) and `GOOGLE_CLOUD_LOCATION` (or
`GOOGLE_CLOUD_REGION`). A missing setting fails when the route is checked, naming the variable to
set. The account needs the Vertex AI User role, and each Anthropic model must be enabled for the
project in Vertex AI Model Garden. The region can be a region such as `us-east5`, `global`, or
the multi-region `us` or `eu`.

Use the Anthropic model id as Vertex lists it (`name@version`, or the bare name). kGraf accepts
the id as sent, `name-version`, and a bare name answered with its dated id, and refuses any other
model name in a response. For Gemini, a requested alias such as `gemini-2.0-flash` may be answered
as `gemini-2.0-flash-001`; kGraf accepts a numeric version suffix and refuses any other model name.

Tool schemas: Vertex accepts an OpenAPI subset of JSON Schema. kGraf converts a tool's schema and
refuses the call, before sending, when the schema uses a keyword the subset cannot express
(`$ref`, `oneOf`, `allOf`, or several non-null types), an `enum` whose values are not all strings,
an object with no properties (a free-form object), or a node with no type that has none implied.
The error names the tool and the place in its schema. An MCP tool whose schema is free-form
cannot be offered to a Vertex route. Function names start with a letter or an underscore, hold
only letters, digits, underscores, dots, colons and dashes, and are limited to 64 characters on
Vertex and 128 on the Gemini Developer API. A `-latest` model alias is refused: set the concrete
model id it resolves to.

A Vertex route that uses your `gcloud auth application-default login` credentials sends the quota
project that `gcloud` recorded in that file (`x-goog-user-project`), so Vertex AI is billed to that
project. Set it with `gcloud auth application-default set-quota-project`. kGraf also registers the
secrets in the credentials file it reads (the private key, key id, refresh token and client
secret) so they are redacted if a tool prints the file.

Reasoning effort: `gemini-2.5` models take a token budget, capped at half of the output limit,
and `gemini-3` models take a level. A level the model does not document is refused (for example
`medium` on `gemini-3-pro-preview`, which documents `low` and `high`). A token budget below the
model's minimum (512 on `gemini-2.5-flash-lite`, 128 on the others) is refused, so a Flash-Lite
output limit under 1,024 tokens cannot carry a reasoning effort. A model family kGraf does not
know is refused when an effort is set.

What to expect from usage on the Gemini routes: thinking tokens are counted apart from the reply
and shown as reasoning output; cached prompt tokens are shown as cache reads; a reported total
above the counted parts is billed as reasoning, and a total below them stops the call and leaves
it in `unreconciled` with the counted figures. A turn the service blocks or refuses (a safety
block, a malformed function call) is billed and recorded, then reported as an unusable turn with
Google's reason. Gemini 3 models require the thought signature that came with a tool call to go
back with it. kGraf stores the signature with the tool call in the transcript, so a resumed
session or a new worker process sends it back. A signature is valid only for the model that made
it, so kGraf sends it only back to the same route and model. A tool call that came from another
route or model has no usable signature, and kGraf sends Google's documented skip value for it on
Gemini 3 models. A signature on a reply's text is kept the same way.

## Azure OpenAI

Two routes reach models deployed on an Azure OpenAI resource (decision 0092): `azure-openai`
(the Chat Completions body) and `azure-openai-responses` (the Responses body). Both are marked
`unverified` in `kgraf providers`: they have been tested against bodies written from Microsoft's
documentation and never against the service. Set the four settings in your own shell:

```
export KGRAF_AZURE_OPENAI_API_KEY=...               # the resource's key, sent as `api-key`
export KGRAF_AZURE_OPENAI_RESOURCE=my-resource      # the name alone: my-resource.openai.azure.com
export KGRAF_AZURE_OPENAI_API_VERSION=v1            # or a dated version for Chat, such as 2024-10-21
export KGRAF_AZURE_OPENAI_DEPLOYMENTS=my-chat=gpt-4o-2024-08-06,my-mini=gpt-4o-mini-2024-07-18
cargo run -p kgraf-cli -- call --provider azure-openai --model my-chat
```

The model you pass is the **deployment name**. The service names the underlying model in its
reply, so `KGRAF_AZURE_OPENAI_DEPLOYMENTS` maps each deployment to the exact model string the
reply carries, dated suffix included. A deployment with no mapping fails when the route is
built. A reply naming any other model stops the call and leaves its spend in `unreconciled`;
the error shows both strings, so the first mismatch tells you what to write in the mapping.

`v1` uses `https://<resource>.openai.azure.com/openai/v1/...` and sends the deployment as
`model` in the body. A dated version uses the older address, with `?api-version=` on the
request and the deployment in the path, and is available for Chat Completions only, from
`2024-09-01-preview` on (the first version with stream usage and `max_completion_tokens`);
a date without `-preview` must be `2024-10-21` or later. The check is a floor: a version that
does not exist fails on the wire. The
Responses route takes `v1` alone, because Microsoft documents the Responses body only at
`/openai/v1/responses`. The resource must be the bare name (Foundry project hosts such as
`*.services.ai.azure.com` are not supported), a deployment name cannot be made of dots alone,
and the version must be `v1` or a real date such as `2024-10-21` or `2025-04-01-preview`;
anything else is refused when the route is checked, before a request is built.

What to expect:
- Usage is inclusive, as on OpenAI. Cached prompt tokens show as cache reads and reasoning
  tokens as reasoning output.
- A completion the content filter cut, a prompt the filter flagged, a Responses completion
  marked blocked, or a reply whose filters did not run (an `error` in `content_filter_results`)
  is billed and recorded, then reported as an unusable turn with the reason (`content_filter`,
  `prompt_filter_results` or `content_filter_results_error`). A prompt the service blocks
  outright is an HTTP 400 that billed nothing.
- The output limit is sent as `max_completion_tokens`, so reasoning deployments work on both
  routes.
- Rate limits and 429s are tracked per deployment, from the `x-ratelimit-*` headers and
  `retry-after-ms`. A route with several deployments uses the tightest limit, and a 429 on
  any of them lowers the route's rate.
- Deployments that Azure updates automatically, and model-router deployments, cannot be used:
  their replies name a model that changes, and the mapping holds one string. Deploy a fixed
  model version and map it.
- Cost shows as unpriced unless a price is set for the deployment. An Azure route is never
  priced by a bare model name, since a deployment name is your own choice. Price a deployment
  with a `<deployment>@azure-openai` entry (or `@azure-openai-responses`) in your price file.
  `kgraf spend --prices` and `kgraf schedule list --prices` follow the same rule. The bench
  prices each deployment from the model it is mapped to, using the vendored table's
  `azure/<model>` entries when they exist. Those are standard-deployment rates: a data-zone,
  regional or provisioned deployment bills differently, so set its price in your price file.
- Microsoft Entra ID tokens and the sovereign clouds (`openai.azure.us`, `openai.azure.cn`)
  are not supported. Only the key is.
- Embeddings: both routes also serve the embedding kind, per deployment
  (`--model-route embedding=azure-openai:my-embed`, with `my-embed=text-embedding-3-small` in
  `KGRAF_AZURE_OPENAI_DEPLOYMENTS`). `v1` posts to `/openai/v1/embeddings` with the deployment
  as `model`; a dated version posts to `/openai/deployments/<deployment>/embeddings`, and any
  dated version is accepted for embeddings. The reply must name the mapped model, as for chat.
  A request carries at most 2,048 texts and 300,000 tokens; a longer list is split. Prices
  follow the same `<deployment>@<route>` rule. Unverified, as the chat routes.

## Models of every kind: typed decisions, embeddings, rerank

Every route has a model kind (decision 0089). `kgraf providers` lists the kinds each catalog
route serves in its KINDS column.

| Kind | What it returns | Served by | Tool |
|---|---|---|---|
| `generative` (default) | prose and tool calls | every route except `systemone` and `tei` | none: it runs workers and the planner |
| `typed-decision` | a value in a declared answer space, with a probability (`/v1/systemone`) | `systemone` (local, default `http://localhost:8000`) | `decide` |
| `embedding` | one vector per text | `openai`, `google`, `llama-cpp` (`/embeddings`); `ollama` (its native `/api/embed`); `tei` (its native `/embed`); `azure-openai`, `azure-openai-responses` (per deployment) | `embed` |
| `rerank` | relevance scores of documents against a query | `together`, `llama-cpp` (`/rerank`); `tei` (its native `/rerank`) | `rerank` |

A route serves a kind only where its vendor documents the endpoint at the route's address.
Moonshot (`kimi`, `kimi-code`), DeepSeek, z.ai, Alibaba (`qwen`), xAI (`grok`, `grok-chat`),
and Meta serve generative work only here. Together serves rerank but not embeddings: its
embeddings reply carries no token count, so every call would be billed and then refused.
Cohere's rerank endpoint is not supported: it bills in search units, not tokens. Only
embeddings on `openai` have been checked against the live service; every other claim is
marked unverified, and a reply field the vendor's page does not show is not relied on as
verified.

Together's rerank replies name a serving model of their own rather than the model you asked
for (its reference shows this, with no rule relating the two), so on `together` the rerank
reply's model name is not checked. Every other route still refuses a reply naming another
model.

`tei` is a local Text Embeddings Inference server (default `http://localhost:8080/v1`, the
same port llama.cpp uses; run one of them elsewhere). It serves one model, so its replies
name none and kGraf cannot check that the model answering is the one you named.

`--route` and `--planner` are always generative. Declare a route of any other kind as
`kind=route:model`, repeatable:

```
export KGRAF_OPENAI_API_KEY=...      # the openai route's own variable, as for a generative route
cargo run -p kgraf-cli -- orchestrate --route kimi:<model> --prices prices.json \
  --model-route embedding=openai:text-embedding-3-small \
  --model-route rerank=llama-cpp:<reranker> \
  --model-route typed-decision=systemone:jev-latest \
  --instruction task.txt
```

In `.kgraf/run.json`, the same list is `model_routes`:

```json
{
  "routes": ["kimi:<model>"],
  "prices": "prices.json",
  "model_routes": ["embedding=openai:text-embedding-3-small"]
}
```

The desktop app's Settings > Providers lists every route of `run.json` with its role and kind,
and each model route with the tool that reaches it.

- **Credentials** are the route's own: an `embedding=openai:...` route reads
  `KGRAF_OPENAI_API_KEY`; the local `systemone`, `ollama`, `llama-cpp` and `tei` routes take
  none. A missing key fails the run's readiness check and names the variable.
- **A local route must be running.** At the start of every run, each local route's endpoint
  (planner, worker or model route: `ollama`, `llama-cpp`, `systemone`, `tei`) must accept a
  connection, or the run fails before it builds anything:
  "route `ollama` is unreachable at http://localhost:11434/v1: ...; start its server or
  remove the route". Hosted routes are not dialed at start.
- **The `ollama` server's version is read at run start.** After the connection check passes,
  kGraf asks the route's own server for its version (`GET /api/version`) and for the minimum
  version of each `ollama` model of the planner and workers (`POST /api/show`, field
  `requires`). It uses the base URL without `/v1`, so a moved `KGRAF_OLLAMA_BASE_URL` is
  honored. No other host is contacted. Each request waits at most 3 s and is not retried.
  - A model that needs a newer server fails the run before it builds anything: "route
    `ollama` model `qwen3.6:35b` needs Ollama 0.30.0 or later, and the server at ... runs
    0.29.1; update Ollama or choose another model".
  - A server older than 0.34.4, the version this route was last verified with, gets a
    run-start notice. The run goes on.
  - A server that does not answer as Ollama (another server behind the route, an Ollama
    without the endpoint, a development build, an `https://` address) gets a notice that the
    version is unknown. The run goes on.
  - Every run prints the server version as a notice, so the run's output shows it.
- **A local route's address can be moved.** A route that takes no credential is called at its
  catalog address unless `KGRAF_<ROUTE>_BASE_URL` is set (`KGRAF_OLLAMA_BASE_URL`,
  `KGRAF_LLAMA_CPP_BASE_URL`: the route id upper-cased, `-` as `_`) to an `http://` or
  `https://` URL. A value that is not one stops the run before it builds. A route that takes
  a credential is never moved this way, so a variable cannot send a key to another host.
- **Accounting.** Every call is a ledger row. Embedding and rerank calls record their input
  tokens; a typed decision records what the server reports. Add the model to the price table
  to see its cost; without a row it is shown as unpriced, never as $0. A priced call too
  small to cost one micro-dollar shows as $0.000001, and totals add the exact amounts before
  rounding, so many small calls total what they cost. A reply counting zero tokens for
  non-empty input is treated as a reply without a count: the call is kept as unreconciled,
  never settled as free. The run's reported spend includes every model-tool call, the
  orchestrator's own and each worker's.
- **Use.** The planner is told the run's model routes by kind, and may call their tools
  itself while it plans (at most four rounds and eight tool calls a round, each call on the
  ledger as the orchestrator's; stopping the run cancels a call in flight, and a failed call
  is listed in the run's notices). A run that declares a model route puts its tool (`decide`, `embed`,
  `rerank`) in every worker's tool set, so any worker may call it; the tool set stays the same
  for every contract so workers on one model share one cached prompt prefix. A contract lists
  a tool in its `tools` where its work relies on it. Nothing calls one by default: the
  orchestrator or a worker chooses to.
- **`embed`** takes at most 32 texts. Given a `query`, it returns the texts ranked by cosine
  similarity to it. Without one, it returns the vectors while they fit in 32 KB, else each
  text's cosine similarity to every other. Models that expect a marker on queries and
  documents (for example `query: ` and `passage: `) take it in `query_prefix` and
  `text_prefix`. **`rerank`** takes at most 256 documents, or fewer where the route's server
  takes fewer (32 on `tei`); the tool's description names those routes, and a longer list is
  refused before any call.
- **Input limits.** A longer list of texts (from the orchestrator) is split into requests of
  at most 2,048 texts and about 100,000 tokens (32 texts on `tei`, 100 on `google`, 2,048
  texts and 300,000 tokens on the Azure routes), each its own ledger row.
  An empty text is refused before sending. For a text longer than the model's input limit,
  kGraf asks Ollama and TEI to refuse it rather than cut it, and OpenAI documents a refusal. The
  other routes document no refusal (Google's embeddings guide says it cuts an over-long input
  without an error), and kGraf cannot see a cut the server does not report, so keep texts
  within the model's limit there. A rerank reply with more results than
  `top_n` is cut to the best ones.
- **Refused, never rerouted.** A kind prefix on `--route` or `--planner`, a model route with
  no kind, a route that does not serve its kind (`embedding=anthropic:...`), and a tool call
  naming a route of another kind all fail with the reason. One `route:model` may be declared
  as two kinds (`embedding=llama-cpp:m` and `rerank=llama-cpp:m`).
- **Rate limits.** `--rate-limit ROUTE=...` also paces a model route's calls, on the same
  limiter as that route's worker calls. An embedding or rerank model's rate-limit headers are
  recorded under that model (OpenAI states limits per model), so they do not overwrite the
  chat model's; the machine's capacity reads both and uses the tighter. A route's 429 replies
  are recorded for the machine's learned rate, as for chat. Model-route calls are left out of
  a route's round statistics, which measure worker rounds.
- Verified live: one embeddings call of one text on the `openai` route (2026-09-29), which
  checked the request, the model echo, the vector and the usage record. Request batching, the
  vector-width check, per-model rate-limit records and the zero-count rule came later and are
  tested against recorded replies only, as is every other kind and route here.

### Connecting a System-One model

A System-One model answers typed questions about a state you give it. kGraf speaks the
`/v1/systemone` wire (decision 0042) to any server that serves it, and does not care which
model or hardware sits behind it. To use one:

1. Start a server that serves `POST /v1/systemone` at `http://localhost:8000`. The `systemone`
   route's address is fixed in the catalog. To reach a server on another machine, forward its
   port to `localhost:8000`. The route sends no credential, so put nothing on the server that
   needs one.
2. Declare the route: `--model-route typed-decision=systemone:<model>`, or
   `"model_routes": ["typed-decision=systemone:<model>"]` in `.kgraf/run.json`. `<model>` is
   sent as `model`; the server's default alias is `jev-latest`.
3. Add the model to the price table if it has a price. Without a row its calls show as
   unpriced. A local model can be priced at zero, which says nothing about compute.

The request body is `{"state": "<text>", "model": "<model>", "questions": {"<id>": {"type":
"noul" | "choice" | "score", "instructions": "<prompt>", "criteria": ...}}}`. The reply is
`{"answers": {"<id>": {"type": ..., ...}}, "usage": {"input_tokens": n, "output_tokens": n}}`,
with one answer per question id. A `noul` answer is a probability, a `choice` answer names one
of the given options, and a `score` answer is an expectation over the given levels. An answer
outside its declared space, or a missing or extra question id, is refused. A typed answer is
not a verified answer.

The model is used only where something calls the `decide` tool: the orchestrator while it
plans, or a worker that lists it. No part of a run calls a decider on its own (decision 0045),
and none supplies the collision expectation. The client exchanged a request and a reply with a real server in an earlier probe
(decision 0045); the route itself is checked against recorded replies only.
For code that wants a decider directly, `kgraf_decide::SystemOneClient` implements the
`Decider` trait; `FakeDecider` stands in for tests.

## A worker, and an orchestrated run

```
cargo run -p kgraf-cli -- worker --provider kimi --model <model-id> --brief brief.txt
cargo run -p kgraf-cli -- orchestrate --route kimi:<model-id> --route openai:<model-id> \
    --instruction task.txt --prices prices.json
cargo run -p kgraf-cli -- orchestrate --resume <run-id> --route kimi:<model-id> \
    --prices prices.json
```

- `worker` runs one worker in its own worktree under `.kgraf/workspaces/` and commits
  its work to `refs/kgraf/attempts/<worker>-<n>`; it never merges.
- `orchestrate` decomposes the instruction into contracts, dispatches workers across the
  routes, verifies each attempt with `--verify` (default `cargo test --workspace`), runs
  quality gates (discovered, or `--gate-command`), and merges verified work onto its own
  branch `kgraf/<run>`. Your checked-out branch never moves.
- When `--verify` is a whole-suite test runner kGraf recognizes, each contract is checked on
  the tests of its own files only, and the whole command runs as a root gate on the merged
  result (an `accept` line says so). Recognized: `python3 -m unittest discover [-s DIR]`
  (`test_<name>.py`), `pytest` with options only (`test_<name>.py`, `<name>_test.py`),
  `node --test`, and `npm test` running `node --test`, jest or vitest (`<name>.test.ts`,
  `<name>.spec.ts`, or a file in a `test` folder), `cargo test --workspace` (the package
  of each file), `go test ./...` with `go.mod` at the root (`go test ./<dir>` for the package
  of each `.go` file), `rspec` (`<name>_spec.rb`), `mvn test` or `./mvnw verify`
  (`-Dtest=<Name>Test`, with `-pl <module> -am` when the root `pom.xml` lists the module),
  `gradle test` or `./gradlew test` (`:<project>:test --tests <Class>` for a project named in
  `settings.gradle`), and `npm test --workspaces` or an `npm test` script that runs it
  (`npm test --workspace=<dir>` for a workspace with a `test` script). A script such as
  `sh check.sh`, a command that already selects tests, a contract whose files have no test,
  and a gate remediation run the command whole (0018).
- `--verify` always runs as a root gate on the merged result too, narrowable or not, unless
  it is already a gate command (decision 0018). A whole command can fail on one contract only
  because other contracts of the plan have not merged yet: a check that needs files from two
  contracts of a split cannot pass on the first. When the command fails on an attempt, other
  contracts of the plan are still open, and it also fails at the attempt's dispatch base, the
  check for that contract is deferred to the root gate: the attempt merges after its scope and
  placeholder checks, its verdict reads `<command> (deferred to the root gate)`, and a notice
  names the contract and the contracts not merged. The last contract is checked whole, and the
  run completes only when the root gate passes. A command that passes at the dispatch base and
  fails on the attempt was broken by the attempt: that is a failure, never deferred. A command
  that fails on the attempt and at its dispatch base with nothing to defer to (the last
  contract, or no root gate) is still a failure, and its reason says why: an earlier merge of
  the run broke it (it passes at the run's original base, and the contracts merged since are
  named), or it failed before the run's work. A narrowed check is deferred the same way when it
  needs files that open contracts write and the attempt's tree lacks: a test or package the
  narrowing selects for both, or, under cargo, a member's `src/lib.rs` or `src/main.rs`, without
  which cargo loads no package of the workspace. A contract that writes only a new workspace's
  manifests is then deferred, and the notice names the missing files and their contracts. A
  narrowed check that needs no such file fails as before. A check that fails on an attempt
  after other contracts merged since its dispatch runs once more on the attempt merged onto
  the integration tip, which is the tree that merges: passing there passes the attempt, with a
  notice; failing there, the reason quotes that run, since a retry starts from the tip.
- A worker that asks for a file another contract of the plan writes is told not to edit it,
  and is told the check the verifier will run on its attempt (the narrowed or whole command, read
  in its worktree), and whether a failure that comes from the other contracts' missing files is
  deferred to the root gate. The contract's own Acceptance line is never quoted as the judge.
- A worker's round budget (`worker --budget`, default 12; orchestrated workers 30) is a
  checkpoint, not a wall (decision 0096). Near the end of each block of that many rounds,
  kGraf measures what the worker did since the last checkpoint: files changed on net, its
  own test, build or check runs getting better, and no sloppy pattern (the same error over
  and over, failed patches piling up, writing a file and putting it back, the same call with
  the same result). A failed patch of a file that a later write of the same file made good is
  not held against the worker; a write to another file makes nothing good. A worker that is
  making progress keeps going with another block, in the
  same conversation. One block without progress after a productive one is allowed; a second
  ends the attempt as stuck, and the orchestrator retries it, then holds the contract. A
  worker that never shows progress stops at its budget as before. The first block is
  stricter: files count only if they changed after its midpoint, and when it ran a test or
  build command that stayed failing without improving, changed files alone do not count (the
  checkpoint says how many were not counted). Test runs are scored by their pass and fail
  counts for cargo, nextest, pytest, unittest, `node --test`, Jest, Vitest, `go test`, Maven,
  Gradle, `dotnet test`, RSpec, Minitest and PHPUnit, and builds by their error counts for
  cargo, `tsc`, the go command, gcc, clang, javac and MSBuild; any other command by its exit
  status. No worker runs more
  than `max_checkpoints` blocks (default 10; 1 is the old fixed limit; see "Run limits"). What the model says about its progress is never read. `worker` prints
  each checkpoint; `orchestrate` reports workers that ran past their budget as
  `rounds` and counts each once as a sizing miss.
- An attempt that changed none of its contract's declared files is not verified, even when
  `--verify` passes: the check passed on the tree as dispatched, which proves nothing. It is
  retried with the reason, then held. The exception: an earlier merge of the same run changed
  those files, and the check fails on the run's original base. The attempt is then accepted as
  an empty integration, with a notice `contract <id>: unverified: ...` naming the earlier
  contract (0086).
- kGraf sets no spend limit (decision 0080): set one in your provider's console. Every call
  is still on the ledger, and `kgraf spend` totals it. The concurrency caps are `--max-items`
  (default 4) and `--max-agents` (default 8).
- A model the price table does not list still runs. The ledger keeps every call, and the
  report and `kgraf spend` name its spend as unpriced (a local model as local, unpriced),
  never as zero dollars. Routing never refuses or skips a route for lacking a price. A
  price entry named `<model>@<route>` applies to calls made on that route and wins over the
  model's own entry. A model whose vendor bills a higher rate above an input size is priced
  only up to that size from the bundled catalog; a larger call is reported as unpriced, not
  costed at the base rate.
- `orchestrate` exits non-zero unless the run completed: a run that ends blocked or
  stopped is a failure to a script or a scheduler.
- **Ctrl-C and SIGTERM stop the run, and say what they left.** The first SIGINT (Ctrl-C) or
  SIGTERM during `orchestrate` or `schedule run-now` is the same stop as the app's stop
  button. No new work starts, every worker and every model call in flight is cancelled, and a
  call that was cancelled is settled in the ledger: it is never left `in_flight`, and one that
  may have billed is listed by `kgraf unreconciled`. The usual report prints with status
  `Stopped`, every worktree the run left is named on a `kept       worktree` line with how to
  continue or remove it, and the command exits with 128 plus the signal number (130 for
  SIGINT, 143 for SIGTERM). `--resume RUN` continues the run. Stopping may take up to 30
  seconds; a line says if that bound was reached. A second interrupt during the stop exits at
  once and prints one `cut short` line: calls may still be open and worktrees may be left on
  disk, and the next run's startup removes stale worktrees. An interrupt that arrives while
  the run is being set up starts nothing and exits the same way. On Windows, Ctrl-C does the
  same; there is no SIGTERM.
- A planner answer that is not the JSON asked for, or a plan that breaks a rule, is asked
  for again (at most twice, each ask on the ledger) with the refusal as a note, before the
  run stops. A plan that the interface checks hold before any work started (a handshake
  mismatch, or a check the last answer failed) is sent back to the planner once with the
  failures before the run ends on the hold; the re-planned contracts are named `re1.<id>`.
- **Run limits.** Six limits are global settings, each with a slider in Settings > Runs
  and a key in the user-level `settings.toml`: rounds in one block per worker
  (`rounds_per_worker`, 4 to 200), the most blocks a worker may be granted while it makes
  measured progress (`max_checkpoints`, 1 to 50; 1 means the old fixed round limit), the output cap of one worker reply (`worker_max_tokens`,
  1,024 to 131,072), the output cap of one orchestrator call (`planner_max_tokens`, 4,096 to
  131,072), the context window assumed for models (`context_window`, 8,192 to
  1,000,000) and the answer wait (`answer_wait_secs`, 30 to 1,800; see "Answer wait"
  below). A flag outranks the setting, and the setting outranks the built-in default:

  | Limit | Flag | Built-in default |
  | --- | --- | --- |
  | Rounds per worker | `--steps` (`orchestrate`, `schedule run-now`), `--budget` (`worker`) | 30 for an orchestrated worker, 12 for `kgraf worker` |
  | Most round blocks per worker | `--max-checkpoints` (`orchestrate`, `worker`) | 10 |
  | Worker reply cap | `--max-tokens` | 16,384 |
  | Orchestrator call cap | `--planner-max-tokens` (`orchestrate`) | 32,768 |
  | Context window | `--window` | 128,000 |
  | Answer wait (seconds) | `--answer-wait` (`orchestrate`) | measured per call |

  One setting covers rounds for both commands: a single `kgraf worker` run is one probe, not a
  share of a plan, so it keeps its own smaller default until you set a value. The commands
  print each effective value and where it came from (flag, user setting or built-in
  default) as `limits` lines on stderr. In the app, a limit reaches runs started after the
  change. A key in `.kgraf/run.json` counts as a flag for that project. A settings value
  outside the bounds, or not a whole number, stops the command with the key named; the
  command line reads `KGRAF_SETTINGS_PATH` when set, otherwise the app's settings file.
  A single worker still uses at most 100,000 tokens of the window (decision 0057), so a
  window above that changes the orchestrator's view only.
- **Time limit.** `kgraf orchestrate --time-limit <seconds>` tells a run the wall-clock time it has.
  The planner's optional asks (the interface asks and the re-plans) share 30 percent of it. An ask that
  would start past that budget, or is expected to end past it from the asks so far, is not made: the
  interface edges are left unchecked with a notice, and a held plan is not planned again. The first plan
  is never skipped. Without the flag nothing is held back. The flag does not stop the run at the limit.
- **Answer wait.** A worker's `ask_user_question` goes to the orchestrator's model first, and
  the worker waits for that answer. The wait fits the model that answers: without a set value,
  each call waits twice the 95th percentile of that model's last 50 answer and connection
  judgment calls in the ledger (its other calls when it has fewer than 8 of those), held
  between 120 s and 900 s; with fewer than 8 timed calls it waits 300 s. A set value (the
  slider, `answer_wait_secs`, `--answer-wait`) wins. When the wait ends, the question goes on
  as if the orchestrator had no answer: to you in the orchestrator box, or, in an unattended
  run, back to the worker at once with the instruction to state its assumption. The model
  call is not cancelled. Its answer, when it comes (within 900 s, or the set wait if longer),
  answers the question if it still waits in the box; otherwise it is given to the worker at its
  next round, so it can correct the assumption. A worker that already ended has the answer
  recorded in its inbox. Each step is a session notice in the run output and the trace. A
  connection judgment fails at the same wait. A history that cannot be read uses 300 s and
  says so in a notice.
- **Output caps are clamped to the model.** A cap above what a model supports (from the
  bundled model catalog) is lowered for that run and the run says so on a `clamped` line,
  never silently. Workers rotate across routes, so the worker cap is the lowest limit among
  them. A model the catalog does not list has no known limit, and the cap goes to the
  provider as set. Settings shows the same note under a slider when an open project's route
  would lower it.
- **Warning about rule conflicts.** kGraf runs your own `--verify` command, and any
  `--gate-command` you set, itself, outside your deny and ask rules, which bind workers. When
  the setting `warn_rule_conflicts` is on, a run checks each program of those commands
  against your rules at its start and prints one line for each program a rule blocks, for
  example:

  ```
  notice     Your verify command runs python3 and your rules block python3 for workers. kGraf will continue to block workers from accessing python3 and will run your verify command directly, without AI.
  ```

  The same sentence is recorded in the run's trace as a notice. A chained command
  (`a && b`, `a; b`, `a | b`, or the commands inside `sh -c`) is split the way a worker's
  command is, and each part is checked. A gate command reads "Your gate command ..." in the
  same sentence. The warning changes nothing: your command runs, the rule keeps binding
  workers, and the run never stops for it. It is off by default. Turn it on in Settings >
  Permissions > "Warn about rule conflicts" (key `warn_rule_conflicts = true` in the user-level
  `settings.toml`), or per run with `--warn-rule-conflicts` or `--no-warn-rule-conflicts`
  (`orchestrate`, `schedule run-now`). A flag outranks the setting; a `warn_rule_conflicts`
  key in `.kgraf/run.json` counts as a flag for that project. When off, nothing is printed or
  traced.
- `--rate-limit ROUTE=RPM[/BURST][@MAX_WAIT_MS]` limits one route's call rate; a call that
  would wait too long fails loudly.
- Every run indexes the working tree before it starts (incremental: about 65 ms warm and
  about 7 seconds cold on an 800-file repository), and reindexes a contract's files after
  its merge, so workers read graph slices without a separate `kgraf index`. A graph that is
  still empty is reported as a notice.
- **Worker worktrees and their builds** (decision 0088). Each worker builds in its own
  worktree, and a compiled project builds its own full build folder there (a Rust `target`
  folder can be several gigabytes). By default kGraf deletes a worker's worktree, with the
  build output in it, once nothing needs it: an attempt that is merged, losing, refused,
  empty or failed, a conflict its resolver settled, and a merged writer subagent's.
  Where it is set, one place per surface:
  - `kgraf orchestrate` and `kgraf schedule run-now`: give `--keep-worktrees`. These commands
    do not read `run.json`.
  - The desktop app: Settings > Runs > "Delete worker worktrees after use" (on by
    default) decides for every run the app starts, box and scheduled. It applies to runs
    that start after the change.
  - `run.json` does not set it: a run file with a `worktrees` key is refused, and the error
    names the switch and the flag.

  Keeping keeps every attempt's worktree and builds, for you to inspect or reuse (a merged
  attempt's work is on the branch as well); `kgraf clean <worktree>` removes a kept one. It looks in both places kGraf keeps worktrees, `.kgraf/worktrees` for runs and `.kgraf/workspaces` for `kgraf worker`; when both hold the name, say which (`kgraf clean workspaces/<name>`), and an unknown name is refused with the list of kept worktrees. A
  kept attempt is never continued as live work. Conversation, gate, probe and interface-stub
  worktrees hold no attempt and are removed either way. A worktree that a live worker, a
  stopped run, a lost lease, a pending merge, a later part of the same work (a handoff
  continues in its parent's worktree) or a conflict resolver still needs is never deleted,
  whichever way this is set. A worktree that cannot be deleted stays, and the run's report
  says which one and why (one warning each). This includes a worktree a process started by a
  build or test still runs in (a daemon that detached itself): kGraf does not kill it, and
  removes the worktree at its next start once that process has ended, or `kgraf clean`
  removes it. A worker's build folder lives in the worktree's
  scratch directory and goes with it.
- An attempt that changes no file is accepted only when its check passes, and it is always
  reported. It is refused when a declared Rust write target holds only `todo!` or
  `unimplemented!` bodies (decision 0086; `unreachable!` is not a marker), and an attempt
  that did change files is refused when a declared target it left untouched held only such
  bodies before the run.
- Workers can build and test their own work (decision 0084). The build and test allowlist
  runs without approval, inside the sandbox with the network off: by default `cargo build`,
  `cargo check`, `cargo test`, `cargo clippy` and `cargo fmt --check`, each under argument
  rules (for example, no `--manifest-path` outside the worktree, no `--config`, no `-Z`,
  no `--target-dir`), and each other language's build and test commands under their own
  rules: `npm test` and `npm run` of a build, test, lint, check or typecheck script (the same
  with pnpm and yarn) and `npx` with a test runner; `pytest` and `python -m pytest`;
  `go build`, `go test` and `go vet` (no `-exec`, `-toolexec`, `-modfile`); `mvn` up to
  `verify` (no `install`, `deploy`, plugin goals or `-s`); `gradle` test and compile tasks (no
  `-I`, `-D`, `-P`); `dotnet build` and `dotnet test` (no `-p:`, `--source`, `--output`). No
  install, publish or fetch runs unasked. Plus the run's own `--verify` and `--gate-command`
  commands exactly as configured, and each narrowed form of `--verify` (one test file or
  package). Set `build_commands` in `.kgraf/worker.json` to change the list: each entry is a
  cargo spelling or one of `npm test`, `pytest`, `go test`, `mvn test`, `gradle test`,
  `dotnet test` (`[]` turns it off). A worker with a declared write scope (every orchestrated attempt) runs
  these and no other shell command that can write; the verifier still refuses any commit
  that changes a file outside the declared writes. The list is on unless `build_commands`
  says otherwise, for attended workers too, and a cargo command that breaks an argument rule
  is refused outright, not asked about.
- A worker whose call is refused by policy, or whose instruction lacks something it needs, is
  told to report it and stop (`report_shortfall`, decision 0093). Your permission mode decides
  what happens next (see "Permission modes" below). A permission report is decided on what a
  policy actually refused that worker, never on its wording: the program, file or tool it
  names must be one of its recorded refusals. A call that failed on malformed input (a patch
  that does not start with `*** Begin Patch`, say) is not a refusal; the worker is told to fix
  and retry it. A report that names nothing refused, names several things, asks for a file the
  contract already writes, or asks for a file another contract owns is answered to the worker,
  which keeps working: nothing is granted or held, in any mode. A worker answered twice that
  reports again is stopped and its attempt counts as a failure. A grant your deny or ask rule
  would meet again is never made: a deny rule holds the contract, and an ask rule asks you (or,
  with nobody attending, holds it, naming the rule). An instructions report amends the contract
  once: the next attempt's brief carries the report, and a second one holds the contract for
  you. A contract is re-dispatched at most twice for shortfalls; a grant that did not help
  holds. Every grant, hold and answer is in the run's trace (`shortfall` and `permission`
  events), and each grant is kept on the contract with who asked, why, and who granted it.
- A shortfall that would hold the contract (a plan-shape error for a file no contract of the run listed,
  a denial nobody attends to answer, a deny rule, a skill) is a claim that the contract cannot
  be done in its scope, and the verifier judges it first (decision 0093). When the worker had
  changed files, its attempt is verified as a completed one would be. If the check passes, the
  contract merges, nothing outside its scope is granted or written, and a notice keeps the
  shortfall as a fact ("verified in its own scope after a shortfall that would have held
  it"). If the check fails, or checks nothing (`true`), the contract is held as before and the
  held reason adds what the check said. A question waiting for you is not checked first: you
  answer it.
- Permission modes (decision 0094). One setting decides who answers a permission request past
  the base policy, for a worker's approval ask and for a shortfall report alike:
  - **Manual**: you answer every request.
  - **Accept edits**: file edits are granted (a file tool, a contract's write scope widened to a
    file it needs, and `mkdir`, `touch`, `mv`, `cp`); commands, network and MCP tools ask you.
  - **Auto**: what your allow set names is granted (`[allow] programs`, `tools`, `paths` in
    `permissions.toml`, or `--allow PROGRAM` and `--allow-path PATH`); anything else asks you.
  - **Orchestrator decides**: the orchestrator grants or refuses each request against its
    contract and never asks you. It refuses after two re-dispatches, when a grant did not help,
    for a tool, and for a path outside the repository or one no worker may write.
  - **Bypass**: everything is granted without asking: workers' shell commands run under a write
    scope, except the few the policy forbids and package downloads (see "Set up a project").
    It offers no protection against prompt injection or unintended actions. Workers may also write the repository's git directory (a patch to
    `.git/info/exclude`, `.git/config` or a ref, or, on macOS, any shell command) and kGraf's settings files `.kgraf/run.json` and `.kgraf/worker.json`. Before the first such write,
    kGraf copies the refs, config and index (at most 32 MiB) or the settings file; after each
    write it checks that the repository still reads (every ref resolves, the config parses,
    and `git fsck --connectivity-only` when the attempt ends) and that the settings file still
    loads. A write that fails its check is put back, and the attempt fails with the check's
    output and is not merged; every write, kept or put back, shows in the run's trace and the
    activity card. Workers' shell commands still run in parallel: only a command that changed
    something in `.git` is checked. When two workers' commands run at the same time and the
    check finds damage, both attempts fail, and the trace says the cause is shared ("changed
    while another worker's command also ran; cause not exclusive"). The ledger, the trace, run records, transcripts, the graph and the rest of
    `.kgraf` stay refused in every mode, as do `.git/objects`, the `.git` link file,
    `.gitconfig` and a nested `.git`. In Bypass only, workers may also write `.gitattributes`,
    `.gitmodules` and files under `.githooks`. A shell command never writes the repository's git configuration, even in Bypass: `config`, `config.worktree`, `info/attributes`, each worktree's `commondir` and every file the config includes stay read-only in its sandbox, so `git config`, `git remote add`, a tracking `git checkout -b` and `git worktree add` fail there, and the command's output says to write `.git/config` with a patch instead. That keeps a command from adding a driver or hook while kGraf's own git runs. On Linux and Windows the sandbox cannot keep single files read-only, so in Bypass a shell command runs with the whole git directory read-only and says so; patches to `.git/...` still work. These are ordinary files of your project: a
    contract must declare them like any file, the verifier checks them against that list, and
    they merge as plain data, with no copy taken first (git history keeps every version).
    kGraf's own git never runs what they select (see "kGraf's own git" below), but your git
    does: a merged `.githooks/pre-commit` runs at your next commit if your `core.hooksPath` is
    `.githooks`. Review them as you would any change.

  A shell script is granted command by command, as Claude Code allows a compound command when
  each subcommand is allowed. When Auto, Accept edits or a grant made to the contract after a
  shortfall covers named programs, a worker's `["sh", "-c", SCRIPT]` (or `bash`, `zsh`, `dash`,
  by bare name, with only `-c`) runs without asking when its script is only simple commands
  joined by `;`, `&&`, `||`, `|` or newlines and a grant covers the program of every command:
  `sh -c "mkdir -p out && cp templates/a.txt out/a.txt"` runs under `--allow mkdir --allow cp`
  and is recorded as grants of `mkdir` and `cp`. It runs confined like any granted command. A
  script with one command no grant covers is asked about whole, and the question names that
  command (`The script runs `curl` (...), which no grant covers`); a run nobody attends denies
  it, and a shortfall the worker reports is grounded in that program. A script is not judged
  command by command, and so is asked about, when it expands a variable or substitutes a
  command (`$`, a backquote), opens a subshell or a group, runs something in the background
  (`&`), redirects input, uses a backslash, a comment, `!` or a shell keyword, starts a command
  with a path, a quote or a `VAR=value`, or redirects output anywhere but `/dev/null` or a
  worktree path a worker may write (an absolute path, `..`, `~`, a glob, `.git`, or any file
  after a directory change: `cd`, `pushd` or `popd`, also behind `command`, `builtin` or
  `exec`); the question says which. A command your policy forbids (`git push --force`)
  refuses the whole script. Otherwise Manual asks, and Bypass runs it.

  A script of build and test commands runs without asking in every mode, as each command does
  alone: when every command is on the build and test allowlist under its argument rules,
  `sh -c "cargo check && cargo test --workspace"` runs as a build. A script may also change
  directory once, with `cd DIR` where `DIR` is a relative directory of the worktree (no `..`,
  no absolute path, no `~`, glob or option, not `.git` or `.kgraf`): `sh -c "cd pricing_engine
  && cargo check"`, the usual shape of a planned contract's acceptance, runs as a build there,
  and `sh -c "cd tools && python3 gen.py"` is granted when `python3` is. Before such a script
  runs, kGraf checks that the directory is a directory of the worktree reached without a link
  out of it, that no cargo configuration or toolchain file (`.cargo/`, `rust-toolchain`,
  `rust-toolchain.toml`) sits in a directory between the worktree root and it, and that each
  `--manifest-path` stays in the worktree from there. A bare `cd`, `cd -`, `pushd`, `popd`, a
  second `cd`, and a project's exact acceptance command after a `cd` are asked about as before.
  Your deny and ask rules still match every command of the script.

  You can let a mode grant those constructs too. Each has a switch, off by default, in
  Settings > Permissions under "Shell scripts a mode may grant", in `permissions.toml` as
  `script_constructs = [...]`, and on `kgraf orchestrate` as `--script-constructs LIST`
  (comma-separated, or `none`; it replaces the file's list for the run). `.kgraf/run.json`
  refuses the key. The switches:

  | Key | Settings name | What it lets a mode grant |
  |---|---|---|
  | `variables` | Variables and command substitution | `$VAR`, `${VAR}`, `$(...)`, `$((...))`, backquotes, `<(...)` |
  | `subshells` | Subshells and groups | `( ... )`, `{ ...; }`, brace expansion `{a,b}` |
  | `background` | Background jobs | `&`, `\|&` |
  | `input_redirects` | Input redirects and heredocs | `<`, `<<` heredocs, `<<<`, `<>` |
  | `escapes` | Escapes, comments and other quoting | `\`, `#`, `!`, `$'...'`, a command named by a path or a quoted word, or after `VAR=value` |
  | `outside_redirects` | Output redirects outside the worker's files | `>` or `>>` to any path, after a `cd` too |

  With a switch on, a script using that construct is granted, in a mode that grants programs
  (Auto with an allow set, Accept edits, a program granted to the contract), when a grant covers
  every program kGraf can see in it, the programs inside `$(...)`, backquotes, subshells and
  groups included. What you accept: kGraf cannot see what a variable or a substitution will
  run, so a command named by a variable (`$CMD`) is granted with no check of what it runs, the
  refusal to delete system paths included. While you have a deny or ask rule on `shell` with a
  pattern, such a command asks instead, in every mode (see the commands kGraf cannot read,
  below). What still holds: your deny and ask rules match every command kGraf
  can see, and for a script judged only because of a switch, a deny or ask rule whose program
  appears anywhere in the script text as a word refuses or asks (`deny = ["shell(curl*)"]`
  refuses `sh -c "ls && C=curl; $C u"`). This match is broad on purpose: `shell(git push*)`
  then matches any such script that names `git`. The sandbox does not change: the
  `outside_redirects` switch only lets the mode grant the script; the sandbox still blocks
  writes outside the worktree and its scratch folder, and the attempt's changes are still
  checked against the contract's declared files. Some scripts are never judged whatever the
  switches say: shell keywords (`if`, `for`, `while`), a function definition, a script with no
  program kGraf can see, a heredoc whose text substitutes a command, and a `${...}` holding a
  quote or a substitution. Manual still asks; Bypass already runs these scripts. A run prints
  `permissions mode MODE; script constructs on: LIST` at its start.

  In every mode, Bypass included: your deny rules block (`deny = ["shell(curl*)"]`, or
  `--deny`), your ask rules ask (`ask = [...]`, or `--ask`), deleting the filesystem root, a
  top-level directory or your home directory is refused, deleting the workspace itself or its
  `.git` asks, every hook runs, and the OS sandbox stays on. A run nobody attends (the command
  line, a scheduled run) denies whatever would ask, and the hold says how to allow it. In the
  app, a request waits in the orchestrator box until you answer it; requests raised within 2 s
  of each other arrive as one card, with Grant and Deny on each and Grant all and Deny all.

  A shell rule (`shell(curl*)`, `shell(git push*)`) matches each command a call runs, from
  every place its program may stand. kGraf reads past grouping and prefix words before the
  program: `{`, `(`, `!`, the keywords `if`, `then`, `else`, `elif`, `while`, `until`, `do`
  and `time`, the wrappers `env`, `command`, `builtin`, `exec`, `nohup`, `sudo`, `timeout`,
  `nice`, `xargs` and others like them with their options, `VAR=value` assignments, and
  redirections before the program. So `deny = ["shell(curl*)"]` refuses `sh -c "{ curl x; }"`,
  `sh -c "if curl x; then :; fi"`, `env A=1 curl x` and `command curl x` in every mode, with no
  switch on. A shell behind a wrapper (`env sh -c ...`), a shell or an `eval` started inside a
  script (`sh -c "bash -c 'curl x'"`) is read the same way. Where a word could be an option's
  value or the program (`sudo -u bob curl`), kGraf matches both, so a rule may match a little
  more than the command runs, never less. The refusal to delete system paths reads commands
  the same way, and a directory change behind a prefix word (`command cd /`, `pushd /`) moves
  where it resolves a relative path. Your policy's `forbid` rules (`git push --force`) also
  refuse their command behind a prefix word (`env git push --force`).

  A program given to another program as an argument is read as a command too: `find -exec`,
  `-execdir`, `-ok` and `-okdir` (up to `;` or `+`), `su -c` and `runuser -c`, `sg GROUP CMD`,
  `ssh HOST CMD` (it runs on the other machine; your rules still judge it), `script -c` and BSD
  `script FILE CMD`, `trap 'CMD' SIG`, `watch CMD`, `flock FILE -c CMD`, `env -S`,
  `parallel CMD :::` and `parallel ::: CMD ...`, and `direnv exec`, `mise exec`, `devbox run`,
  `uv run`, `poetry run`, `pipenv run`, `bundle exec` and `conda run`, and an option whose
  value is a program (`ssh -o ProxyCommand=`, `rg --pre`, `tar --to-command` and `-I`,
  `rsync -e`). So
  `deny = ["shell(curl*)"]` refuses `find . -exec curl x \;`, `su -c 'curl x'` and
  `ssh host curl x` in every mode, Bypass included, and the refusal to delete system paths sees
  `find . -exec rm -rf / \;`. A recursive deletion of a placeholder (`find / -exec rm -rf {}
  \;`) or of what `xargs` or `parallel` reads from its input asks. Your policy's `forbid` rules
  refuse their command inside a script and in every command a call starts, in Bypass too.

  Some commands run what kGraf cannot read: `source FILE`, `. FILE`, a shell given a file or its
  input instead of `-c` (`bash run.sh`, `cat x | sh`), `ssh HOST` with no command, `su USER`,
  `sudo -i`, `script FILE` with no command, and `parallel` with commands from a file or its
  input. While you have a deny or ask rule on `shell` with a pattern, such a call asks in every
  mode, Bypass included, because kGraf cannot check it against your rule. With no such rule the
  mode decides, but no grant covers it.

  Runners, Windows shells and git settings are read the same way. A package runner runs the
  package's program: `npx PKG` (and `npx -p PKG CMD`, `npx -c 'CMD'`), `npm exec`, `npm init
  PKG` (which runs `create-PKG`), `pnpm dlx`, `pnpm exec`, `yarn dlx`, `yarn exec`, `bunx`,
  `bun x`, `uvx` and `pipx run`. A container or remote runner runs its command: `docker exec
  CONTAINER CMD`, `docker run IMAGE CMD` (and `--entrypoint`), the same for `podman` and
  `nerdctl`, `docker compose exec` and `run`, `kubectl exec POD -- CMD` (also `run` and `debug`
  after `--`), `lxc exec` and `incus exec`, `vagrant ssh -c`, `adb shell CMD` and `wsl CMD`.
  `cmd /c` and `cmd /k` scripts are split on `&`, `&&`, `||` and `|`; `powershell` and `pwsh`
  with `-Command` (or `-c`), and `powershell` given its script as plain arguments, are split on
  `;`, `|`, `&&`, `||`, newlines and braces; `-EncodedCommand` is decoded (base64 of UTF-16LE
  text) and read. git runs a command from a configuration value: `git -c core.sshCommand=`,
  `core.pager`, `core.editor`, `core.fsmonitor`, `credential.helper`, `alias.NAME=!CMD`,
  `diff.external`, `gpg.program`, filter and merge drivers and the like, and from
  `GIT_SSH_COMMAND`, `GIT_EXTERNAL_DIFF`, `GIT_PAGER`, `GIT_EDITOR`, `PAGER` and `EDITOR`
  set before `git` or exported earlier in the script. `git config KEY VALUE` with such a key
  is read as the command it will run. `git NAME`, where NAME is no git command, runs the alias
  your repository's or your user
  configuration defines (`!CMD` runs CMD), or else the program `git-NAME`. `git rebase --exec`,
  `git bisect run`, `git submodule foreach`, `git filter-branch --*-filter` and `git difftool
  --extcmd` run their command too.

  Build-tool recipes are shell commands, read from the worktree: `make TARGET` reads the
  Makefile's recipe lines for the target and its prerequisites (variables, `$(shell ...)`,
  `-C DIR` and `-f FILE` included); `npm run X`, `npm test`, `npm start`, `pnpm run X`,
  `pnpm X`, `yarn X` and `bun run X` read the `package.json` script with its `pre` and `post`
  scripts; `just RECIPE` reads the justfile recipe and its dependencies; `cargo NAME` reads the
  alias `.cargo/config.toml` defines, and `cargo run`, `test`, `build` and the other commands
  that compile read its `runner`, `rustc-wrapper` and `linker` settings. Each recipe line is
  judged as a command. Interpreter code is read for the calls that start programs: inline code
  (`python3 -c`, `node -e` and `-p`, `deno eval`, `bun -e`, `perl -e`, `ruby -e`, `php -r`, an
  `awk` program, `osascript -e`) and script files (`python3 x.py`, `python3 -m MODULE` for a
  module of the worktree, `node x.js`, `tsx`, `deno run`, `perl x.pl`, `ruby x.rb`, `php x.php`,
  `awk -f`, and a `#!` file run by its path, such as `./run.py`). A call with literal arguments
  is read as a command: `os.system("curl x")` and `subprocess.run(["curl", "x"])` run `curl`.
  The modules a script imports from the worktree are read one level deep. A command is judged
  on its files as they were when it was checked; if another call of the same round rewrites one
  of them first, the command is refused before it runs, naming the file, and runs once it is
  issued again and checked as it is now.

  A rule reads past more spellings of a command. A program is matched by its base name,
  lowercased and without a Windows extension, so `/usr/bin/curl`, `CURL` and `Curl.exe` all
  match `shell(curl*)`. More shells read their `-c` script (`ash`, `mksh`, `yash`, a versioned
  name such as `bash5`, and another case such as `SH`). A name the script defines to run
  another program is read: `alias c=curl; c x` and `hash -p /usr/bin/curl g; g x`. A global
  option before a git subcommand does not hide it: `git -c a=b push`, `git -C . push` and `git
  --no-pager push` all match `shell(git push*)`. An install runs the project's own lifecycle
  scripts, so a `postinstall`, `prepare` or `prepublishOnly` that runs a denied program is
  caught on `npm install`, `npm ci`, `npm publish`, `npm pack` and the pnpm, yarn and bun
  equivalents (dependency scripts are not read). `cross-env`, `xcrun`, `rlwrap` and `eatmydata`
  join the wrappers whose tail is the command they run. In interpreter code, a command string
  spelled with numeric escapes (`\x72`, `r`, `\162`) is decoded, a spawning module reached
  as another module's attribute (`shutil.os.system`) is read, and Ruby's `IO.read("|cmd")` and
  `Kernel.\`` are read.

  A command kGraf cannot tell is unreadable, and asks while you have a shell rule with a
  pattern: a program word the shell expands (a glob `/usr/bin/cur?`, a brace list `{curl,x}`,
  zsh's `=NAME`), a script whose quote or substitution never closes or that does not split
  into words, a command a shell runs from a placeholder its input fills (`xargs -I{} sh -c
  '{}'`, `find -exec {} ;`, `parallel {} :::`), a substitution given as code (`eval "$(...)"`,
  `bash -c "$(...)"`), PHP's `FFI`, and `osascript -l JavaScript` (JXA).

  Some of these cannot be read, and are treated like `source FILE` above: a runner whose
  command kGraf cannot name (`docker run IMAGE` with no command, a package given by a URL,
  `kubectl exec` without `--`), a Windows script with a variable as a program or a construct
  kGraf does not read (`iex`, `[...]::`, `-File`), a cmd or PowerShell script that is not
  plain text (see "Windows: the worker shell" below), a git value it cannot see (`--config-env`,
  `core.hooksPath`, an included configuration file), a command named by a variable or a
  substitution (`$CMD x`, `"$@"`, `${X}`, `$(which curl) x`, `eval "$X"`), a command that sets
  `PATH` (`PATH=./bin ls`) or a variable that makes a program load code first (`LD_PRELOAD`,
  `NODE_OPTIONS`, `PYTHONPATH`, `BASH_ENV` and the like), a missing or unparseable recipe file
  or script, a recipe file kGraf finds only in a parent directory, a spawning call whose
  arguments are not literal
  (`os.system(cmd)`), code that runs text as code (`eval`, `exec`), a local module that imports
  another local module, and an interpreter kGraf has no reader for (`lua`, `Rscript`, `julia`,
  `tclsh` and others) given code or a script. While you have a deny or ask rule on `shell`
  with a pattern, each asks in every mode, Bypass included; with no such rule the mode decides,
  and no grant covers it. A test runner (`pytest`, `cargo test`, `node --test`) runs your test
  code, which kGraf does not read, as before.

  A grant of a program does not grant what it runs. In Auto's allow set, Accept edits' file
  programs and a program granted to the contract, every program a command runs needs its own
  grant: a wrapper and the program after it (`env curl x` runs on grants only when `env` and
  `curl` are both granted), every command of a script it hands to a shell (`su -c`, `bash -c`),
  and a word that may be an option's value or the program (`sudo -u bob curl` names `bob` too,
  so it asks). The same holds for a runner, a Windows shell, git, a build tool and an
  interpreter: `npx -c 'curl x'`, `docker exec web curl x`, `cmd /c curl x`, `make deploy`
  whose recipe runs `curl`, and `python3 x.py` whose code runs `curl` each run on grants only
  when `curl` is granted too. With only `npm` in the allow set, `npm test` asks unless its
  script's programs (`jest`) are there too. As in Claude Code, no program grant ever covers
  `watch`, `setsid`, `ionice` or
  `flock` with a command, or `find` with `-exec`, `-execdir`, `-ok`, `-okdir` or `-delete`;
  kGraf adds `ssh HOST CMD` and every command it cannot read. Manual asks for these, the
  orchestrator decides them in Orchestrator decides, and Bypass runs them unless a rule above
  applies. Claude Code does not match a deny rule inside `sh -c` and strips wrappers such as
  `timeout` and `nice` before matching an allow rule; kGraf matches inside every script and
  needs a grant for the wrapper too, so it is never looser.

  You can edit every list of `permissions.toml` in Settings > Permissions, with no file
  editing. "Rules" holds Deny rules and Ask rules; "Allowed in Auto" holds the allow set:
  Allowed programs (`[allow] programs`), Allowed tools (`[allow] tools`) and Allowed paths
  (`[allow] paths`). Each entry is one line with a Remove button. Type an entry in the field
  under a list and press Add or Enter. The field checks the entry as you type with the check
  the file loader uses: an entry the loader refuses shows its message and cannot be added, and
  an entry that loads but matches no call (a Claude Code tool kGraf does not have, such as
  `WebFetch`) shows the loader's notice under the field and can be added. A listed entry the
  loader refuses is shown in orange, with the reason on hover. Each add or remove is written
  to `permissions.toml` at once (a temporary file, then a rename). The app then reads the file
  again, entry by entry, as it does at start: the valid entries apply to the next call of
  every worker, running ones included, and the refused entries and notices listed at the top
  of the tab are brought up to date. Removing the refused deny or ask entry that kept the app
  in Manual lifts Manual at once, with no restart, and the mode in the file applies. An entry
  is not edited in place: remove it and add it again. Only the list you changed is written;
  the other keys, the comments and the order of the file stay as they were. The window never
  writes a project file.

  A rule names a tool a worker can have, or it is refused where it is read (`--deny`, `--ask`,
  `permissions.toml`, and the app), with the tools a rule may name. On the command line, one
  refused entry in `permissions.toml` stops the command; the error names each refused entry,
  its list and the reason. Correct or remove it, then start again. The app keeps every valid
  entry and lists each refused one, with its reason, in Settings > Permissions. A refused deny
  or ask rule keeps the app in Manual (every request asks) until you correct the file, because a
  block or ask you wrote is missing; a refused allow entry keeps your mode. A file that is not
  valid TOML, or has an unknown key, gives Manual with no rules, and the app does not write over
  it. The names are kGraf's tools (`shell`, `apply_patch`, `replace_in_file`, `read_file`,
  `list_directory`, `search_files`, and the others the refusal lists), `*` for every tool, an
  MCP server's tool as `mcp__<server>__<tool>`, every tool of a server as `mcp__<server>` or
  `mcp__<server>__*`, and Claude Code's names, read as the kGraf tool that does the same job:
  `Bash` as `shell` (`deny = ["Bash(rm *)"]` is `shell(rm *)`), `Edit`, `Write`, `MultiEdit`
  and `NotebookEdit` as `apply_patch`, `Read` as `read_file`, `Grep` and `Glob` as
  `search_files`. A rule or allow entry on `apply_patch` (or on `Edit` and `Write`) also covers
  `replace_in_file`, which writes files the same way, so a deny rule on a path holds for both;
  a rule on `replace_in_file` covers only that tool. Patterns follow Claude Code's: a `*` stands for any text, and a trailing ` *`
  or `:*` that is the only `*` also matches the bare command, so `shell(ls *)` and
  `shell(ls:*)` match `ls` and `ls -la` but not `lsof`, and `shell(ls*)` matches `lsof` too.
  A Claude Code path rule names a path in the repository: `Edit(.env)` or `Edit(secrets/**)`
  matches at any depth, `Edit(/src/**)` and `Edit(./src/*)` only at the root; a path outside
  the repository (`//...`, `~/...`), a carve-out (`!...`) and a parameter rule
  (`Bash(timeout:30)`) are refused, as kGraf can never match them. Differences from Claude
  Code: a `Read` rule covers `read_file` only, so a path the searches must not see needs its
  own `search_files(...)` rule; a `Read` rule does not stop an edit; an `Edit` rule does not
  cover a shell redirection. A rule on a Claude Code tool kGraf does not have (`WebFetch`,
  `WebSearch`, `Agent`, `Task`, `TodoWrite`, `Skill` and the others in
  `kgraf_tools::permissions::CLAUDE_CODE_WITHOUT_COUNTERPART`) loads, matches no call, and is
  said as a notice at run start (a `notice` line and the trace) and in Settings. The notice
  says what kGraf uses instead and which kGraf rule gives the effect you intended, for example:
  "kGraf has no WebFetch tool. Workers reach the web through the shell (curl, wget) or MCP
  tools. To block that, add `shell(curl*)`, `shell(wget*)` or a rule for the MCP tool." A name
  in neither list, such as a typo, is refused. An MCP name is checked when the run starts, once the
  servers have listed their tools: a deny or ask rule, or an allow-set tool, that names no tool
  any worker of the run has is printed as a `notice` line and traced, and the run goes on (the
  same file serves every project, and a tool the workers lack cannot be called). `[allow]
  tools` take the same names, except a wildcard outside one server's `mcp__<server>__`, since
  Auto would grant every tool it matched.

  Only you set the mode: the dropdown at the top of the orchestrator box, the user-level
  `permissions.toml` (in kGraf's configuration directory, beside the app's `settings.toml`; set
  `KGRAF_PERMISSIONS_PATH` or pass `--permissions-file` to use another), or
  `--permission-mode` on `kgraf orchestrate`. A change applies at once, to running workers and
  to requests already waiting. `.kgraf/run.json` and `.kgraf/worker.json` refuse every
  permission key, and a `permissions.toml` inside the project is refused. Bypass is offered in
  the app only after you turn on "Allow Bypass mode" in Settings, and its first use asks you to
  accept a warning; `disable_bypass = true` in `permissions.toml` refuses it everywhere; and it
  is refused when kGraf runs as root unless `KGRAF_CONTAINER_SANDBOX=1` says the process runs
  in a disposable container. On the command line, `--container-sandbox` (with that variable) is
  another name for `--permission-mode bypass`, `--may-grant` is another name for `--allow`, and
  `--allow` given without a mode selects Auto. The command prints a `BYPASS PERMISSION MODE`
  notice to stderr whenever a run starts in Bypass.
- Plan review (decision 0094). Turn on "Review the plan before workers start" beside the mode
  dropdown (or `plan_review = true` in `permissions.toml`, or `--plan-review`). After planning,
  the plan waits in the orchestrator box: each contract with what it waits on, writes and
  reads, and the graph lights what it writes (hover a contract to light its own). Approve plan
  dispatches it; Change plan sends your note to the planner and shows a new plan; Reject plan
  ends the run with nothing dispatched. Turning review off while a plan waits approves it. A run
  nobody can approve (the command line, a scheduled run) is refused before anything is planned,
  unless `--approve-plan` approves the plan for you. On the command line the refusal comes
  before kGraf opens the project: it prints the refusal alone and creates no `.kgraf`, database,
  branch or worktree, and leaves what earlier runs made as it was.
- A contract whose check always passes (`--verify true`, `:`, `exit 0`) is printed as
  `unverified <id>: <reason>` after the `verified` line. The run still completes; the line says
  the check did not look at the work. If the worker also finished after a policy refused a call
  it never ran, the reason says so too.
- A run that did not complete (stopped, for example by Ctrl-C or SIGTERM, or blocked) prints
  `unfinished <id>: <state>` after the `held` lines, for each open contract that is not held
  and each gate that did not pass: "not started: it waits on c1", "not verified: 1 attempt(s)
  dispatched, the last was cut off by the stop; nothing of it was merged", "not run: it covers
  open contract(s) c2". Before, a stopped run printed only its `verified` line, and a contract
  the stop cut off was named nowhere (0056).
- A contract whose real check passed, but whose worker finished after a policy refused a call
  it never ran, is verified, and the refusal is printed as a `notice` line, for example
  "notice contract c1: verified by `cargo test`; the worker finished after a policy refused
  `grep` and never ran it". The check ran on the committed change, so the call the worker did not make does not
  weaken it; the notice keeps the refusal in view. A worker the orchestrator stopped after a
  shortfall report reads "was cancelled", not "finished". A refusal names the program a rule matched: `sh -c "mkdir x && python3
  ..."` under a deny rule on `python3` is a refusal of `python3` (0094). Only a refusal that
  matched one command does that (your deny or ask rule, the deletion limit, or the mode naming
  the command of a script it did not grant). A skill's `allowed-tools`, or a rule on the whole
  tool (`deny = ["shell"]`), refuses the `shell` tool itself, and a report of that tool, or of
  the program the call ran, is grounded in it.
- While a worker's worktree has a `.cargo/config.toml`, `.cargo/config`, `rust-toolchain` or
  `rust-toolchain.toml` that differs from the one it started with, every allowlisted command
  is refused: those files can change what cargo runs whatever its arguments say. The same
  holds for the other build tools' configuration at the worktree root (`.npmrc`, `.yarnrc`,
  `.mvn/jvm.config`, `gradle.properties`, `NuGet.Config`, `Directory.Build.props`, `.nvmrc`,
  and the rest of the list above).
- On macOS a sandboxed build cannot read the rest of the checkout it runs in (only its cargo
  files), so a `.env` at the checkout root stays private. Every value in that `.env` is also
  removed from what a worker's model sees, and build and test output is scrubbed of key-shaped strings before the model sees it. A secrets file a worker reads anywhere in its worktree (`.env`, `.env.local`, `prod.env`, `.envrc`, `secrets.json` and the like, with `read_file`, `search_files`, or a shell command that names it) has its credential values removed from the worker's transcript, the trace and the run record; the worker's model still sees the file as it is.
- A sandboxed build cannot read your per-user cargo config (`~/.cargo/config.toml`,
  `~/.cargo/config`, or the one in `$CARGO_HOME`) on macOS or Linux, since it can hold a
  registry token or a proxy password. kGraf passes its build settings in for you: a source
  replacement (a mirror or vendored directory), registry index URLs, rustflags, rustdocflags,
  a linker, `target-dir` and `net.offline` are copied into a cargo home of the command's own
  (`.kgraf-cargo-home` in the worktree's scratch directory, named by `CARGO_HOME`), with your
  registry cache linked in. Nothing else is copied: no token, credential provider, `[http]`
  proxy, `[env]` value or alias. A source or registry URL with a login in it
  (`https://user:secret@host`) is not copied either, and a failing build's output names each key
  left out. Your project's own `.cargo/config.toml` still wins over these settings, as it does
  outside the sandbox. Package-index logins (pip, Yarn, Poetry) and other command-line tool
  logins in your home are unreadable too. On Linux the Poetry, Pipenv, Playwright, Cypress and
  Deno caches and environments are readable, so tests that use them run.
- The sandbox cannot download or unpack crates. A run checks that the dependencies are in
  the local cargo cache before it starts, and fetches them outside the sandbox only with the
  project's consent (see [Set up a project](#set-up-a-project)).
- `--shell-timeout` (on `worker`, `orchestrate` and `schedule run-now`) is the longest one
  worker shell command may run, in seconds; `shell_timeout_secs` in `.kgraf/worker.json` sets
  it for the project; without either it is 600. A build or test command on the allowlist gets
  all of it. Any other command is stopped after 120 s (or the longest, if shorter) unless the
  model's call sets `timeout_secs`, which may ask for up to the longest; a larger value is cut
  to it, and the result says so. A command killed at its limit returns its output so far,
  marked `[killed after N s]`, with what to do next: run a slow command again with a larger
  `timeout_secs`, or start a command that does not end by itself (a server, a watcher) in
  the background with its output sent to a file, and check on it.
- `--resume <run-id>` continues a killed run from its record without repeating a paid call.
  Each worker's rounds are in its transcript before its next request leaves, so only a
  request that was in flight at the kill is sent again.
  With it, `--retry-gate <gate>` or `--waive-gate <gate>`, plus `--gate-note`, decides a
  held gate.
- `--interactive` reads notes and `?` status questions from stdin while the run goes.
- `--gate-timeout` (1800 seconds) bounds each gate command and each run of the `--verify`
  command. On macOS and Linux a gate command also runs under a CPU-time limit of twice
  that, so a process that escapes its process group cannot spin without limit.
- Interface contracts are experimental and off by default. `--contract-checks` turns them
  on (`contract_checks` in `.kgraf/run.json`). With them on, planning starts with one call
  that returns the contracts only. When the plan has two or more contracts, kGraf then derives
  an interface for each contract that another contract uses, without asking a model: where the
  repository already holds the producer's scaffold (a body of `todo!()` or `raise
  NotImplementedError` in code, not in a string or a comment), that scaffold is the stub; where
  the producer edits existing code, its current files are the stub. The files that use the producer's names
  are the consumer files, and the project's build tool compiles them (`cargo test --no-run
  -p ...`, or a Python import). A derived check that passes is registered as a compile-only
  interface: it is checked before the first worker starts, it costs no model call, and it
  catches a signature a consumer uses and the producer's scaffold does not have. When the
  producer's attempt passes its own check, the consumer files are compiled again against what
  it delivered: a change to a signature or type they use fails the attempt, naming the
  interface and the files, and a change they do not use does not. If the base does not build
  without the delivery either, or the compile cannot run, that is a notice, not a failure. In
  Rust the compile checks every item, signature and type the consumer files use; in Python the
  import checks only the names they import when they load, not call shapes. TypeScript gets
  `node node_modules/typescript/bin/tsc --noEmit -p <dir>` when a `tsconfig.json` and the
  project's own compiler are in the repository; Go gets `go vet <packages>`; Java gets `javac`
  over the consumer files and their sources only when no Maven, Gradle or Ant file is present;
  C# gets `dotnet build --no-restore <project>` only when the project's
  `obj/project.assets.json` is in the repository. Each needs its program on the `PATH`. A check
  runs on a copy of the repository as git tracks it, so an installed `node_modules` or a
  restored `obj/` that git ignores is not there. When a part is missing, the dependency is
  counted under `not run` with the reason. Shell, JavaScript and other languages get no
  derived compile check. A derived check that fails before the first worker starts is not
  registered, so the producer is never held by a check that cannot apply. The planner
  is asked for a behavior check (a reference and a small test of the one producer) only when
  no test in the repository already exercises that producer alone and one could: when the only
  tests need several producers, whose code does not exist yet, no reference of one producer
  can pass them, so none is asked for; nor for a producer whose files are all binary build
  outputs (a wheel, an archive, a compiled library), since a text reference cannot hold
  one. When no producer needs a behavior check, no second
  call is made. Each dependency whose behavior check did not run is counted under `not run`
  in the report's `contracts` line, with a notice and a trace record that name the contracts
  and the reason; its compile-only interface still checks it. Where a
  behavior check is asked for, the request lists what kGraf already supplies and the answer
  holds only the reference and the test, within 4,096 output tokens. That size is held on the
  call only when the planner's reasoning is off for the `interfaces` kind (`--planner-reasoning
  off` or a narrower setting): a model's reasoning counts against the same output limit, so a
  reasoning planner gets `planner_max_tokens` for this call, as for its others. An answer cut
  off at the 4,096 limit says so and is not asked again. Each interface is
  checked as it arrives, in the sandbox: the consumer and the test compile against the stub,
  the test fails against the stub and passes against the reference. A compile command that
  cannot run or does not build the files is replaced by the derived one, without asking
  again. A Python test file that the test command runs as a script and that defines its
  tests without calling them gets the calls added at its end, with a notice; when a test
  takes arguments (pytest fixtures), the reason is sent back instead. An interface that fails any other check, an answer that is not JSON, and a
  dependency no interface binds are sent back, up to three asks in all, with the reason,
  which quotes the answer where it went wrong and says what each entry needs; a re-ask covers
  only the interfaces still refused, with the planner's previous entries for them (the
  refusals quoted are cut at 12,000 characters), and accepted interfaces are kept. The report's `interfaces` line states what the interface call
  cost and what it yielded: `planner 2 calls 310 s 9,400 tokens; accepted 6 compile-only, 1
  behavior`. Output from the host's own developer tools (macOS `xcrun` and `xcodebuild`
  lines) and repeated identical warnings are removed from check output before it is shown or
  sent to a model, with a count of the lines removed; error lines stay. The run is never held
  for an interface the planner could not write. Before the first
  worker starts, the handshakes of a behavior interface's parties are compared too (a
  compile-only interface has none). An interface whose symbols are files (packaging metadata
  such as `pyproject.toml`, a module file) is compared the same way: a party that names such a
  file in backticks makes a claim about it, and the party that writes the file provides it. A
  party that lists several symbols in one sentence ("I will provide `vectorops` and
  `dotproduct`") provides each of them, and a symbol may be named by its qualified name
  (`vectorops.dotproduct`), its call form (`dotproduct(a, b)`) or an import line (`from
  vectorops import dotproduct`). A party's `touch` list holds only the files it will write. A contract
  whose interface fails at that point is held, with the file that failed named. Each
  handshake and each check names every party's route and model, and the run record keeps
  each hold with them, so you can see which models disagreed. A contract whose compiler or
  test runner is not installed is not held: its interface checks are skipped, a notice names the
  program, and the run's verify gate still judges the work. A check command that names a
  script or source file nobody supplies (not in the interface, not in the repository) is asked
  for again with the file named, and a check that finds such a file missing says "file", not
  "program not installed". The report's `not run` counts consumed edges left unchecked and
  interface checks that could not run. A worker can challenge a contract
  with the `challenge_contract` tool; the answer names the artifact that settled it, or
  says it is judgment.
- Every planner prompt carries up to 8 KB about the project so the planner does not invent
  names: the package names from the manifests (`Cargo.toml`, `package.json`,
  `pyproject.toml`), then the signatures, from the code graph, of the files and symbols your
  instruction names. What does not fit is cut, and a notice says how much. A named file the
  graph has no signatures for is included as text up to the cap, with a notice. Name the
  files and symbols you mean in the instruction (in backticks, or by path) to get their
  signatures into the prompt. An interface file or command that still holds a template slot
  such as `<crate_name>` is refused and the planner is asked again. When a re-plan follows a
  hold, it quotes the failed check (stage, command, last lines of its output), and is asked
  again like the first plan if its answer is not JSON or breaks a plan rule. A re-planned
  contract's worker sees, in its brief and marked as data, only the failures that concern
  it: those of the interfaces that bound the contract it replaces, or a file it writes.
- The planner's answer is read leniently where the intent is certain: text up to a leaked
  `</think>` is reasoning and is skipped, the last JSON object that parses is the answer,
  and a line break typed inside a string is read as `\n`. Contracts that all declare the
  same files and that the acceptance check would judge by one test file cannot be verified
  apart; such a plan is sent back once, and merged into one contract if the planner repeats
  it, with a notice.
- `--check-timeout` and `--check-compile-timeout` (600 seconds each) bound one check run.
  `--planner-max-tokens` (default 32,768) is the output limit of each orchestrator call. An
  answer cut off at that limit fails and names the flag; interface contracts carry whole
  files, so they may need more. See "Run limits" below for where the default comes from.
- `--planner-reasoning <off|low|medium|high>` sets the reasoning level of each orchestrator
  call on the planner's route. It is the same as `--reasoning planner=<level>`; see
  "Reasoning level of each model call" below.
- One Rust interface costs about 12.5 seconds of checks and one Python interface about
  5.4 seconds on a small project. The run report says whether checks were on.

## Reasoning level of each model call

One setting, `reasoning`, sets how much the model reasons on every model call of a run
(decision 0098). It has a universal value and overrides:

- `all`: every call.
- A group: `planner` (every call on the planner's route), `checks` (the contract-check calls
  made on a worker route's model) or `worker` (a worker's rounds and its compaction summary).
- One call kind, by the name the ledger records in `call_kind`:

  | Group | Call kinds |
  | --- | --- |
  | `planner` | `decompose`, `interfaces`, `reground`, `answer_question`, `judge_connections`, `interpret`, `question_route`, `question_answer`, `challenge_review` |
  | `checks` | `handshake`, `consumer_test`, `interface_review` |
  | `worker` | `worker_round`, `worker_compaction` |

The most specific scope wins: a call kind's own value, then its group's, then `all`. Each value
is one of:

- `unset`: no reasoning field is sent, and the model's own default applies. This is the
  default for every call. Set it on a kind to turn off a universal level there.
- `off`: no reasoning. `off` never means "less reasoning": kGraf sends it only where a wire form
  is verified to turn reasoning off (Chat Completions sends `reasoning_effort: "none"`), and it
  checks every answer. An answer that still carries reasoning fails its call with an error that
  says the server ignored the setting. A provider with no verified off control refuses the call
  before sending it.
- `low`, `medium`, `high`: sent as the provider spells them. No answer shows whether a server
  honored a graded level, so kGraf uses what was measured (see below).

Where to set it, highest first. A higher place wins scope by scope, so a flag that sets `all`
keeps a `planner` value from the settings file:

1. The command line: `--reasoning <level>` for `all`, `--reasoning <scope>=<level>` for a
   scope. Repeat it for more scopes. `kgraf orchestrate` and `kgraf worker` take it. A scope
   given twice is refused. `--planner-reasoning <level>` is `--reasoning planner=<level>`;
   giving both is refused.
2. The project's `.kgraf/run.json` (the app's runs): a `reasoning` object, for example
   `"reasoning": {"all": "low", "planner": "off"}`. A file that sets both
   `planner_reasoning` and `reasoning.planner` is refused.
3. The user's `settings.toml`, the same file that holds the run limits:

   ```toml
   [reasoning]
   all = "low"
   planner = "off"
   worker_compaction = "unset"
   ```

   `kgraf schedule run-now` and the desktop app's runs read it. An unknown scope or level in
   the table stops the command, naming the table. The app has no control for it yet; it keeps
   the table when it saves its settings.

Examples:

```
kgraf orchestrate --route ollama:<model> --prices prices.json --reasoning off
kgraf orchestrate --route kimi:<model> --prices prices.json --reasoning low --reasoning planner=off
kgraf worker --provider ollama --model <model> --brief brief.md --reasoning worker=off
```

What a run says and refuses. When the run opens, kGraf compares each route's level with the
measurements it has recorded ([MODEL-SUPPORT.md](MODEL-SUPPORT.md), "Reasoning level by
route"):

- A graded level that was measured to have no effect on that route and model stops the run
  before any call. The error names the route, the level, the call kinds and the measurement,
  and tells you to set those kinds to `unset` or `off`. For example, `ollama` with
  `qwen3.6:35b` does not honor `low`, `medium` or `high`.
- A graded level with no measurement for that route and model is sent. The run says so at its
  start, on a `reasoning` line that names the route, the level and the call kinds and says
  `UNVERIFIED`: the level may have no effect.
- `off` is said at the start with its check.
- The effective setting is printed with the other limits, one `reasoning` line per scope and
  where it came from (`flag` or `user setting`), or `reasoning  unset on every call`.

Reasoning tokens are counted in the ledger as before, for every call kind.

## Start from the project's own documents

```
cargo run -p kgraf-cli -- orchestrate --corpus --route kimi:<model-id> --prices prices.json
cargo run -p kgraf-cli -- orchestrate --corpus --correction "the export is CSV, not JSON" ...
cargo run -p kgraf-cli -- orchestrate --corpus --approve <plan-id> --accept-command "cargo test" ...
```

`--corpus` reads the repository's documents and code, prints the plan it read with a plan
id, and stops; nothing is created in the repository. `--correction` reads again with your
correction. `--approve <plan-id>` runs that stored plan with no new model call; a proposed
validation command becomes a gate only when you name it with `--accept-command`. In an
empty directory, give `--instruction` too. The reading's work items become presumed work
nodes at once (the tokens are the reading's, already in the ledger), except those a
roadmap node already stands for; approving the plan adopts them.

## MCP tools, stranded worktrees and scheduled runs

```
cargo run -p kgraf-cli -- allow mcp__<server>__<tool> --note "why"
cargo run -p kgraf-cli -- allow mcp__<server>__<tool> --revoke
cargo run -p kgraf-cli -- clean <worktree> [--force]
cargo run -p kgraf-cli -- schedule add nightly --instruction task.txt --zone America/New_York --cron '0 0 3 * * *'
cargo run -p kgraf-cli -- schedule add docs-check --node <work-node-id> --instruction task.txt --zone America/New_York --cron '0 0 4 * * *'
cargo run -p kgraf-cli -- schedule list --prices prices.json
cargo run -p kgraf-cli -- schedule run-now nightly --route kimi:<model-id> --prices prices.json
```

- MCP servers are configured in `.kgraf/worker.json` (section `mcp_servers`). An
  unattended worker is denied an MCP tool until `allow` records a rule in
  `.kgraf/allow-rules.json`.
- `clean` removes a worktree whose owner was on another machine or in another PID
  namespace, or whose owner's PID now names a process kGraf cannot read (on Windows,
  another user's or a protected process). It refuses while the owner or any process
  started in it runs; `--force` is needed when a run record still names it.
- kGraf names each owner by PID, start time, machine and boot. On Windows these are the
  process creation time, `MachineGuid` and the boot sequence number, so a dead owner's
  worktree and run lease are taken over there as on Linux and macOS.
- `schedule` also has `remove` and `enable`. `--zone` is required and an unknown zone is
  refused; give exactly one of `--cron` (seconds first) or `--at` (RFC 3339).
- A schedule is a property of a work node. `schedule add --node <id>`
  attaches it to that node. Without `--node`, a node `schedules/<id>` is made from the
  instruction. Each run is recorded on the node: start, outcome and spend by run id. The
  galaxy shows the schedule, its last run and any disable reason on the node's label.
  `remove` detaches the schedule; the node keeps its history.
- `schedule run-now` refuses a schedule the claim table has disabled unless you pass
  `--force`, and holds the schedule's run lock while it runs, so the scheduler does not
  start the same schedule beside it and a second `run-now` is refused. `--verify` defaults to
  the first test command the project's build files declare. A run that ends blocked or
  stopped makes `orchestrate` exit non-zero, and `run-now` records it as failed.
- `schedule list` counts a schedule's runs, clock-started and `run-now` together, as
  `runs: N done, M failed`. A `run-now` still going (or one whose process ended before it
  recorded an outcome) is shown on its own line, never counted as done.

## The desktop app

```
cargo run -p kgraf-shell --bin kgraf-app -- --project /path/to/repo
cargo run -p kgraf-shell --bin kgraf-app -- --probe
```

- `--project` opens that project and shows it, its galaxy fed live from its index (run
  `kgraf index` there first). The projects you opened before open too.
- `--probe` prints the graphics adapters and exits. `--force-software` accepts only a
  software (CPU) adapter. The window always states when it renders on the CPU.
- `--screenshot FILE` captures the window to a PNG after the first frame and
  `--screenshot-after` seconds (default 3), then quits.
- Closing the window can leave the app in the tray, where scheduled runs fire. On Linux the
  tray uses the StatusNotifierItem D-Bus service (no GTK needed). Your desktop must run a
  host for it: KDE Plasma has one; GNOME needs the AppIndicator and KStatusNotifierItem
  extension. A click on the icon shows the window.
- **Updates.** Off by default: Settings > General > "Check for updates on start" lets the app read
  the GitHub Releases of `Marceux-Croix/kGraf` at start, and "Check now" always checks.
  This is the only network request the setting causes. An offered release counts on the
  bell; its menu row says "kGraf <version> is available" and carries the button that
  downloads it. Accepting twice queues one download. The download is checked against the
  release's `SHA256SUMS` and refused when the line is missing or does not match. The
  checksum file comes from the same release as the installer, so it detects a corrupt
  download and does not prove who published it; a detached signature is not built yet.
- The app builds runs from `.kgraf/run.json`: the `orchestrate` flags as JSON
  (`routes`, `prices` and the optional ones, snake case; unknown keys are refused, and so is
  `worktrees`, which the app's settings switch decides). An old
  file that still sets `cap_run`, `cap_task` or `cap_hour` loads, prints one notice, and the
  caps do nothing (decision 0080). There is no settings screen for routes yet, so write
  this file by hand.
  Example: `{"routes": ["kimi:<model-id>"], "prices": "prices.json"}`.
- **Settings** has five tabs. Each setting is one row: its name, one line that says what it
  does, then its control. A change applies at once; there is no Save button.

  | Tab | What it holds |
  | --- | --- |
  | General | Background mode; updates: check on start, include prereleases, Check now |
  | Runs | Parallelism, the compaction point, the five run limits, deleting worker worktrees after use |
  | Permissions | What reading `permissions.toml` found, Allow Bypass mode, the rule editor (deny, ask, and the allow set's programs, tools and paths), the shell-script switches, the rule-conflict warning |
  | Appearance | Theme, color vision, reduced motion, the GPU preference and the current adapter |
  | Providers | The active project's routes, read-only |

  The permission mode itself is chosen in the orchestrator box, the one place it is set.
- **The compaction point** (Settings > Runs) is the share of the effective window at which a
  worker's context is compacted. It follows the recommendation measured from your stored runs
  until you move it (80%, labeled unmeasured, until there are enough runs). Every run the app
  starts takes it, from the next run; a `point` key in `.kgraf/run.json` outranks it. The
  command line does not read it: `kgraf orchestrate` and `kgraf worker` take `--point`.
- The settings file holds the compaction slider (5% to 95%, default 80%), the five run
  limits (see "Run limits"), the
  background-mode switch, the worker-worktree switch (`delete_worker_worktrees`, default
  on), the caps, the theme (device, dark or light), the color
  vision mode, reduced motion, whether the guide is open, and the open projects. Two
  windows share it: each saves under a lock and keeps the projects the other opened or
  closed. An unknown key is refused, so a settings file that a newer build changed does
  not open in an older build. A key left at its default is never written.
- **Appearance > Color vision** (Settings): standard, protanopia, deuteranopia,
  tritanopia or achromatopsia. Remaps the five semantic colors only, chosen so they
  stay distinct under a simulation of that color vision deficiency. It applies live, with
  no restart.

**The layout** uses a small set of meaning colors: violet (the accent) for progress and live, cyan for the
selection, a match and the primary action, yellow for needs input, orange on a failed
thing itself, gray for the rest.

- **Masthead:** the menu button, the kGraf mark with the branch, the project, the search
  field, the scheduler toggle with "n of m workers" (it only shows the cap; the cap is
  set in Settings), the render badge (GPU, or CPU rendering in the danger colors), and
  the bell. The bell counts everything that needs you over every open project:
  approvals, questions, held runs and failures; its menu jumps to the pinned card.
  A failure (a box run, a scheduled run, a gate or a check) is a pinned card with
  Dismiss. Dismissing clears it from the dot and the bell, and is kept in
  `.kgraf/acknowledged.json`; a new run of the same work supersedes the failure, and a
  failure of a resumed run is a new card. Scheduled failures come from the graph's
  history, so one from while the app was closed shows at launch.
- **Guide:** collapsed, a rail with Graph, Search, Providers and Settings (the only way
  into settings). The menu button opens the sidebar, which stays open until closed:
  Projects (switch, add a folder, close from the row's menu; a project that works cannot
  be closed; a close pauses its scheduler so no scheduled run starts in between, and
  while the scheduler finishes a check the row says "Closing"),
  Graph and Search, then Configuration. Each project row has a dot: yellow
  needs input (a failure counts as needs input), cyan new results since you looked,
  blinking gray working, none idle; a dot is never orange. The collapsed menu button
  takes the same order over all projects, and its tooltip names them.
  Runs in a project keep going while another is shown.
- **Center:** filter chips (status and crate; each is a search, so its result is a
  selection on the graph), then the graph on a near-black stage with its legend, minimap,
  selection title, the run scrubber (a placeholder at the live edge: going back in time
  is not built) and the view controls (legend, minimap, zoom, fit; keys
  `/`, `m`, `+`, `-`, `0`). Below: the project row, the pinned approval cards (a choice
  asks to confirm), and the composer with the tokens the ledger attributes to the run.
- **Right:** "Up next", the one text surface (Selection and Search tabs; transcripts as a
  chat, search results as rows with a static graph thumbnail), and "Live activity", the
  graph change log newest first, where a row selects its node.

**Inside the window.** The orchestrator box is always present. `/run <instruction>` starts
a paid run for the shown project through the same construction as `kgraf orchestrate`,
and says what is missing when `.kgraf/run.json` is absent or invalid. `/status`
answers from the run record. Tool approvals are answered in place (allow once, allow
always, deny). A run whose dependencies need fetching, in a project with no answer yet,
waits on a card (Allow once, Always for this project, Not now) and starts once allowed. Play, stop and your own order drive execution; the two concurrency caps
are in settings. The search field takes inline operators, and its result is a selection
on the graph, with snippets in the one text panel beside it. Work nodes show done, in
flight, blocked (with an edge to the cause), held by the cap, and colliding. Not built
yet: expanding a work node into its children, and `/compact`.

## Instructions and skills

```
cargo run -p kgraf-cli -- instructions scan
cargo run -p kgraf-cli -- instructions import ~/some/folder/with/skills
cargo run -p kgraf-cli -- instructions show
```

`instructions` gathers the instruction files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, and
others) and `SKILL.md` skills your other coding agents already use into one inventory.
It reports conflicts and malformed files and never silently picks between two variants;
`instructions choose` resolves a conflict.

Workers use the skills in that inventory (`.kgraf/inventory.json`). `worker` and
`orchestrate` read it once, when the run starts; run `instructions scan` again to pick up
changes for the next run. An inventory that exists and cannot be read stops the run. A
Terminal-Bench run (`--tbench-bridge-socket`) reads no skills, and says so on stderr when
an inventory is there.

- **Listing.** Every worker's prompt lists each skill by name and description, and the
  worker gets a `skill` tool to activate one. The skill's instructions come back as that
  tool's result.
- **`disable-model-invocation: true`.** The skill is not listed, and the `skill` tool
  refuses it.
- **`allowed-tools`.** Once a worker activates such a skill, kGraf refuses every call to a
  tool outside the list. The worker sees the refusal and the skill's name as the call's
  result. The restriction lasts for the rest of that worker's run, a resumed run included,
  and applies to that worker and the subagents it spawns while restricted. Two active
  skills allow only the tools both list.
  Entries name kGraf's tools (`read_file`, `list_directory`, `search_files`,
  `apply_patch`, `replace_in_file`, `shell`, `graph_slice`, `skill`, `mcp__<server>__<tool>`), with a
  trailing `*` for a prefix (`mcp__git*`) and `mcp__<server>` for all of a server's
  tools. An entry naming another agent's tool (`Read`, `Bash`) matches nothing here, so
  a skill written only for another agent refuses every call; edit its list to use these
  names.
  A skill that lists a tool the worker does not have (a name from another agent, or
  `write_file`: the write tools are `apply_patch` and `replace_in_file`) is not activated. The worker's call fails
  with each unknown entry, the real tool that does the same job where there is one, and the
  list of tools the worker has, and its tool set is left as it was (decision 0053, amended
  2026-10-02).
  When the permission mode would also ask about or refuse such a call, the worker sees the
  skill's refusal, which names the mode's verdict too, and you are not asked. A worker can
  always call `report_shortfall`, whatever its skills list. A permission report for a
  command or tool its skill refused holds the contract with the skill's name, however the
  report words it, since no
  permission grant lifts a skill's `allowed-tools`: edit the skill's list or the
  instruction, then run the work again. A skill's refusal of a shell call is a refusal of the
  `shell` tool: a report of scope `tool` naming `shell`, or of scope `command` naming the
  program, holds the same way. A call made in the same round as the skill's activation is
  judged before the skill applies; if another policy refused it, a later report of a tool the
  active skill refuses still holds the contract with the skill's name.
- **Resuming after a skill changed.** A resumed worker re-activates its skills at the
  version it activated. If you edited, removed or disabled one of them since, the resume
  stops and names the skill: restore the skill as it was, or start the work again as a new
  run.
- **No skills.** With no inventory, or none a worker may invoke, workers get no `skill`
  tool and their prompt is unchanged.

The orchestrator's own planning calls use no tools, so skills apply to workers only.

## Windows: the sandbox user (`kgraf sandbox setup`)

On Windows, kGraf runs sandboxed commands as a dedicated local user, so Git Bash
and Windows' own TLS (Schannel) work inside the sandbox and the sandbox user's
network can be blocked (decision 0097). This needs administrator rights once per
machine:

```
kgraf sandbox setup     # asks for administrator rights through the UAC prompt
kgraf sandbox status    # what exists; needs no administrator rights
kgraf sandbox remove    # undoes everything setup made; asks for administrator rights
```

What `setup` creates, for the Windows user who runs it:

- two local users, `kgraf-off-<hash>` (no network but kGraf's proxy) and
  `kgraf-on-<hash>` (network open, for dependency installs), in a local group
  `kgraf-sandbox-<hash>`; the hash comes from your Windows account's SID;
- the users may not log on remotely, over the network, as a batch job or as a
  service, and they are hidden from the sign-in screen;
- four Windows Firewall block rules for the offline user: everything off this
  machine, inbound from off this machine, local TCP except kGraf's proxy ports
  47100 to 47131, and local UDP;
- four loopback filters for the same users: Windows Filtering Platform (WFP) filters at
  the connect layers (IPv4 and IPv6) that block a connection on this machine to any port
  outside kGraf's proxy ports, and local UDP. Windows Firewall rules do not act on this
  loopback traffic, so without these filters a command without the network could reach
  any service listening on this machine. If Windows refuses one, setup goes on without it,
  names it with Windows' error, and ends with a notice that loopback connections are not
  confined on this machine;
- DNS filters for the users the firewall rules block (the offline user and each worker
  slot's user without the network, below): Windows Filtering Platform (WFP) filters
  meant to make the DNS Client service refuse their name lookups. A lookup goes through
  that service, which sends it as its own account, so the firewall rules
  do not see it. There are three: one on each name resolution cache layer (IPv4 and
  IPv6), which see a lookup on this machine, and one on the user-mode RPC layer, which
  sees a remote caller only. The filters, their provider and their sublayer are named
  "kGraf sandbox" and stay after a restart. The online users and your own account are
  not filtered. Windows can refuse a filter: some versions refuse the name resolution
  cache filter (Windows Server 2025 answers `FWP_E_OUT_OF_BOUNDS`). Setup then goes on
  without it, names each refused filter with Windows' error, and ends with a notice
  that DNS is not confined on this machine;
- a launcher service, `kgraf-launcher-<hash>`, set to start on demand, so that cargo and
  rustc work (below);
- an undo record under `%ProgramData%\kGraf\sandbox\`.

The users' passwords are random, 40 characters long, and stored only for your
Windows user, sealed with Windows' data protection (DPAPI), in kGraf's configuration
folder. Running `setup` again resets the passwords and refreshes the rules and the WFP
filters. Setup
stops with a message if Windows Firewall is off or a group policy makes local rules
ineffective, and it never takes over an account it did not create.

`setup` also creates two users for each worker slot (`--slots`, four by default, none with
`--slots 0`): one without the network and one with it. A command takes a slot for its own
folder and holds it while it runs; a worker holds the slot of its worktree for its whole
life. Two folders never hold one slot at the same time, so two commands in different
folders, two dependency installs among them, cannot write each other's files. Commands in
one folder share its slot. When every slot is held by another folder, a worker is refused
at once and a command waits up to 60 seconds, then is refused. The error says to run
`kgraf sandbox setup --slots N` with a larger N. With `--slots 0`, one folder at a time
runs commands without the network, and one at a time runs commands with it.

After `setup`, every sandboxed command runs as the sandbox user, through the
`kgraf-sandbox-runner` program installed beside `kgraf`. Checks and ordinary shell
commands run as the offline user. Dependency installs reach the network through kGraf's
proxy. A command can write only where kGraf allows. It cannot read your Windows profile, apart
from the toolchain and program folders kGraf grants it inside that profile. cargo and rustc
need a named kernel object (cargo a jobserver, rustc a named mutex to print any warning or
error). In your own Windows session a sandboxed command can make or open none, because that
session admits its logon SID, which kGraf denies the command so that it cannot open your own
processes. So `setup` installs a launcher service, `kgraf-launcher-<hash>`. It starts a
sandboxed command in session 0, the session Windows gives its services. There a name with no
prefix resolves below the machine-wide object directory, and cargo and rustc can make the
named objects they need. The service starts when a command needs it, runs only kGraf's own
runner program, and accepts requests from your Windows account alone. Nothing runs as SYSTEM.
Setup then proves the path: it runs a probe as a sandbox user that makes a named semaphore
and an unprefixed mutex. `kgraf sandbox status`, `kgraf sandbox selfcheck` and the end of
`setup` print one line that starts with `named objects:`. It says which path is active, why,
and what the probe found.

If the service cannot be set up, or the probe is refused, kGraf asks for administrator
rights once more, for a fallback. It admits the sandbox group to the object directory of your
Windows session with four rights (look a name up in it, list it, make an object in it and
make a subdirectory in it) and the right to read the directory's own access list. The grant
cannot rewrite the access list, take its ownership or delete it. The risk: a sandboxed
command can then put a named object in the directory your own programs read, so a program of
yours can be made to open an object a sandboxed command made instead of its own. Windows
makes the directory again at every logon, so the grant is gone at your next sign-out whether
or not `kgraf sandbox remove` ran. Remove takes it off as well. The same risk line is in
`setup`, `status` and `selfcheck`.

Where neither path works, a sandboxed command starts in your session and cannot make or open
a named object there, so cargo and rustc cannot run as a sandbox user. A failed command whose
output shows this ends with a `[sandbox]` line that says why; the program did not fail on its
own, and `-j1` does not help. Each command still gets a jobserver kGraf makes for it, named in
`CARGO_MAKEFLAGS` (under a `Global\` name when the service starts the command).

A git command in the sandbox
that writes a ref in a repository inside your profile fails with "unable to create
directory for ...": before it writes a ref, git reads the attributes of each folder above
the repository, and your profile's folders are not readable to the sandbox user. This was
seen for `git init`; `git commit` and a new branch write refs the same way. Git in the
command trusts the command's own folders (`safe.directory`, given through
`GIT_CONFIG_COUNT`): they belong to your account, and git refuses a repository another
user owns. rustup may print `warn: could not canonicalize path C:\Users\<you>`: it looks
for a toolchain setting in each folder above the command's folder, and your profile folder
is not readable to the sandbox user. The warning changes nothing.
Without `setup`, the token sandbox described below stays in use. If the users exist but
the runner is missing, every sandboxed command stops with an error. `kgraf sandbox remove`
stops and deletes the launcher service, takes off the fallback grant where setup made one, and
also revokes the folder permissions that kGraf added for the users, and deletes the DNS
and loopback filters, their sublayer and their provider by the keys the undo record lists.
It does not touch other WFP filters.

`kgraf sandbox status` shows each WFP filter on its own line, with its layer and key:

```
  present  DNS filter on the user-mode RPC layer <key> (remote callers of the DNS Client service only; a lookup on this machine does not pass this layer)
  FAILED   DNS filter on the name resolution cache layer, IPv4 <key> (meant to refuse the offline users' lookups; `kgraf sandbox selfcheck` proves the effect): Windows refused it at setup: WFP error 0x80320028 (FWP_E_OUT_OF_BOUNDS); ...
  present  loopback filter on the connect layer, IPv4 <key> (blocks loopback connections of the offline users to any port outside kGraf's egress proxy ports; `kgraf sandbox selfcheck` proves the effect)
```

`FAILED` means Windows refused that filter at setup: DNS (for a DNS filter) or loopback
connections (for a loopback filter) are not confined on this machine, and running setup
again does not change that; status then ends with a non-zero exit.
`MISSING` means setup has not added it; run `kgraf sandbox setup` again. `unknown`
means Windows did not let your account read WFP filters; run `status` as an administrator
to see it. `present` says only that the filter exists. Only `kgraf sandbox selfcheck`
shows that it works (below). Until a selfcheck on your machine prints `dns: confined`,
treat name lookups by the offline user as able to leave the machine.

On macOS and Linux these commands say that no setup is needed.

### Check that the sandbox confines a command (`kgraf sandbox selfcheck`)

`kgraf sandbox selfcheck` is a hidden command. It is not in `--help`. It runs one real
command in kGraf's sandbox and says whether the sandbox held. On macOS, Linux and
Windows, the command:

- writes a file in its writable tree (it must succeed);
- writes a file in a hidden folder inside that tree (it must fail);
- fetches a page from a local origin through kGraf's egress proxy (it must succeed);
- connects to the same origin directly (it must fail).

On Windows the command runs through Git Bash, so Git for Windows must be installed. If it
is not, the check stops and says how to get it (`winget install Git.Git`, or set
`KGRAF_GIT_BASH_PATH`). The check never skips in silence.

The output names the backend that ran. With the Windows sandbox user (after
`kgraf sandbox setup`), the direct connection must fail, because the offline user's
loopback filters block it:

```
sandbox selfcheck: ok
  backend: Windows sandbox user
  network: only the egress proxy was reachable
  dns: confined (the sandbox user's lookup reached no DNS server, and the DNS Client service did not look the name up)
  hidden folder: not writable
  tree: writable
```

With the sandbox user the check also proves the DNS filter by its effect. It makes three
new names of the form `kgraf-probe-<32 hex digits>.invalid`, and then:

1. Your own process looks up the first name. A DNS server's answer (the name does not
   exist) shows that this machine reaches a DNS server.
2. The sandbox user looks up the second name with `Resolve-DnsName` in Windows PowerShell.
3. Your process asks the DNS Client service's cache, without sending anything, about all
   three names. The service keeps "does not exist" for a name it looked up, so the second
   name must look like the third (never looked up), not like the first.

The `dns:` line passes only when step 2 got no DNS server's answer and step 3 shows that
the service did not look the name up. Every other result fails the line and the whole
check, which then starts with `sandbox selfcheck: failed` and exits non-zero:

- `dns: not confined (lookups leave the machine): ...` when the sandbox user's lookup got a
  server's answer, or the service cached its name;
- `dns: not proved: ...` when nothing could be shown either way: this machine reached no DNS
  server, the cache cannot tell a looked-up name from a new one, or the probe printed no
  status. A run without a working network cannot prove DNS confinement.

With the token sandbox (no setup), the Windows sandbox cannot confine the network
(decision 0011). The check runs the command with the network open. It proves nothing about
the network, and it says so on a line that is not a pass:

```
sandbox selfcheck: passed with a gap
  backend: Windows restricted token (token sandbox)
  network: not confined (token sandbox; run `kgraf sandbox setup`)
  dns: not confined (token sandbox; run `kgraf sandbox setup`)
  hidden folder: not writable
  tree: writable
```

On macOS and Linux the check prints no `dns:` line. There a command limited to the proxy
reaches no resolver by construction, and this check does not probe it.

Exit status: 0 when every step passed. A pass with the network gap above also exits 0. The
gap is a known property of the token sandbox, and the output states it. The check exits
non-zero, with the failing step and the backend, when any step fails. That includes a
direct connection that works under the sandbox user, a `dns:` line that is not
`dns: confined` under the sandbox user, or a missing Git Bash. A script that must have a
confined network checks that the first line is `sandbox selfcheck: ok`.

## Windows sandbox: checks and the worker shell run with the network open

The Windows sandbox cannot deny network access without administrator rights (decision
0011). The verifier's check, the quality gates, the contract checks and the workers' shell
commands therefore run there with the network open, and say so:

- Each such check and gate ends its output with `[sandbox] network not blocked: the Windows
  sandbox cannot deny network access without administrator rights (decision 0011); this
  check ran with the network open; run `kgraf sandbox setup` once, with administrator
  rights, to block it (decision 0097)`, a passing one included.
- Each worker's shell tool says it in its description, which the model reads on every
  turn, and in the first result of the tool: `[sandbox] network not blocked: the Windows
  sandbox cannot deny network access without administrator rights (decision 0011); shell
  commands run with the network open; run `kgraf sandbox setup` once, with administrator
  rights, to block it (decision 0097)`.
- The run command prints both sentences as `sandbox` lines when a run opens. The run's
  trace holds each once, and the run report lists the check sentence among its notices.
- The code a check or a command runs can reach any host, and on Windows it can also read
  every file you can read. A check that passes there proves less than one that passes on
  macOS or Linux, where the network is denied.
- Builds still run with cargo offline (`CARGO_NET_OFFLINE=true`): they use only the crates
  you already fetched, as on macOS and Linux, and the sandbox could not write a download
  into your cargo folder anyway.
- macOS and Linux are not changed: the network stays denied for checks, gates and the
  worker shell.

## Windows: the worker shell

On Windows the worker's `shell` tool runs commands under the same restricted-token sandbox
as the checks (decision 0011). A command runs as the argv the model gives, with the same
output caps, timeout, cancel and permission checks as on macOS and Linux. The timeout, a
cancel and a normal exit each end every process the command started, through the job
object.

- **`sh` and `bash` run with Git Bash**, the `bin\bash.exe` of
  [Git for Windows](https://gitforwindows.org/), so `["sh", "-c", "<line>"]` takes the same
  POSIX shell syntax that kGraf's permission checks read. kGraf finds Git Bash beside the
  `git.exe` on your `PATH`, then under `%ProgramFiles%\Git`, `%ProgramFiles(x86)%\Git` and
  `%LOCALAPPDATA%\Programs\Git`. It never uses `C:\Windows\System32\bash.exe`, which is the
  WSL launcher. To name another one, set `KGRAF_GIT_BASH_PATH` to its `bin\bash.exe` (not
  `usr\bin\bash.exe`, which does not set up the tools' `PATH`).
- **No Git Bash, no shell line.** A `sh` or `bash` command then fails with a message that
  says to install Git for Windows or set `KGRAF_GIT_BASH_PATH`. kGraf does not fall back
  to PowerShell or cmd: they would run a different language than the one that was checked.
  A model can still run `powershell -Command ...` or `cmd /c ...` as a program; the
  permission rules read those scripts too.
- **A cmd or PowerShell script is read only when its text is plain** (decision 0094). cmd
  and PowerShell read their command line by their own rules, not the rules kGraf quotes it
  for, so kGraf reads a script as text only when the text it checks is certainly the text
  the interpreter runs. A plain script of words, spaces and the operators kGraf splits on
  reads as before: `["cmd", "/c", "dir"]`, `["cmd", "/c", "cargo", "test", "&&", "echo",
  "done"]`, `["cmd", "/c", "cargo test && echo done"]`, `["powershell", "-Command",
  "Get-Item x; cargo test | Out-Null"]`, and cmd redirections such as `2>&1` and `> out.txt`.
  Any other script is unreadable: like `source FILE`, it asks while you have a shell deny or
  ask rule with a pattern, no program grant covers it, and with no such rule the mode
  decides. kGraf still reads what it can of it, so a deny rule and the command policy still
  refuse a program they name there.
  - cmd: a double quote in any argument, `^`, `%`, `!`, a parenthesis, a line break or other
    control character, a character outside ASCII, a switch before `/c` other than `/X` or
    `/X:VALUE`, an argument with a space next to other arguments, a script given as one
    argument with a space and no operator (cmd runs it as a program name when a file of that
    name exists; give the words as separate arguments or add `/s` before `/c`), a script
    given as one argument that ends in `\`, and a program word that cmd ends at `,`, `;`,
    `=` or `/` or reads as a built-in (`rd\x`, `echo.`).
  - PowerShell (`-Command`, `-c`, a script as plain arguments, and the decoded text of
    `-EncodedCommand`): a quote (straight or typographic), a backtick, `$`, `@`, braces,
    parentheses, `#`, `<`, a control character other than a tab or a line break, a
    character outside ASCII, `&` joined to other text, and an argument with a space that
    ends in `\`.

  To give a script with these, use `["sh", "-c", "<line>"]`, which runs in Git Bash with
  the exact text kGraf checked.
- **Programs that are not `.exe` files** (a `.cmd` or `.bat` file, or a shell script such
  as Node's `npm`) do not run as a plain argv. The error says to run them through Git Bash:
  `["sh", "-c", "npm test"]`.
- **Git** gets the same safety settings as on macOS and Linux (no `core.fsmonitor`, no
  hooks, no system or global configuration), and the sandbox also turns off git's
  credential helper.
- **Not yet run on Windows.** This is built and compile-checked for Windows only. The steps
  to test it on a Windows 11 machine are in [WINDOWS-TESTING.md](WINDOWS-TESTING.md).

## Linux sandbox: cargo files above a worktree

A worktree lies inside your checkout (`<checkout>/.kgraf/worktrees/<name>`). Cargo run
in it reads the checkout's `Cargo.toml` and `.cargo/config.toml` above it. The Linux sandbox
refuses reads outside the worktree by default, so it grants these files one at a time.
On macOS, reads use a deny list, so this section does not apply.

- **Opt-in per command.** Gates, the verifier and the worker's shell tool ask for these
  reads. Contract checks never do: their tree is a copy under `/tmp` or another scratch
  root, and another user could put a `.cargo/config.toml` above it.
- **Bounded at the checkout.** The limit is the checkout root the caller gives, never
  derived from git. Only `Cargo.toml`, `.cargo/config.toml` and `.cargo/config` are
  granted, read-only and never executable. `~/.cargo`, `$CARGO_HOME` and your own
  read-denied paths are never entered. With no home directory known, no `.cargo` file is
  read.
- **Owner and mode trust.** A file is granted only when it and every folder from it up to
  the checkout are owned by you or root and not writable by others. Group-write is
  accepted only for your user private group: your primary group, named for you, with no
  other listed member and no other account using it as its primary group. That covers the
  umask 0002 default of Debian, Ubuntu and Fedora. If the system cannot list its accounts
  (sssd or LDAP with enumeration off), group-write is refused.
- **A refusal names its cause.** Each cargo file that exists and is not granted is
  appended to a failing gate, verification or shell command as a line such as
  `[sandbox] <checkout>/Cargo.toml is not readable in the sandbox: <checkout> is
  group-writable (mode 775) by group 100, which is not this user's private group`.
  `orchestrate` also prints each one once, as a `sandbox` line, when a run opens.

To fix a refusal, remove group or other write from the named folder or file
(`chmod g-w,o-w <path>`), or give it to your own user. Other tools that read
configuration above their directory, such as pytest, get no such rule.

## kGraf's own git

kGraf runs git on your machine, outside the worker sandbox, to create worker worktrees,
commit and merge their work, read status and history, and check a Bypass worker's writes to
`.git`. A repository can make git run a program or reach the network by itself, and a worker
in Bypass may write the git config and the attribute files, so kGraf's own git ignores all of
that:

- hooks, in `.git/hooks`, in a `core.hooksPath` directory such as `.githooks`, and
  config-defined hooks (`hook.<name>.command`);
- every filter (`clean`, `smudge`, `process`), merge driver and diff driver (`textconv`,
  `command`) the config defines, whatever `.gitattributes` selects, and `diff.external`;
- the file system monitor, credential helpers, `core.askPass`, editors and pagers, commit and
  tag signing, and signature checks;
- every network or local transport (a fetch, a submodule clone, a lazy fetch in a partial
  clone), and submodule recursion;
- `GIT_*` variables in kGraf's environment, except your commit identity
  (`GIT_AUTHOR_NAME` and friends) and `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`.

What kGraf keeps: your identity and your global config, and git's built-in attributes
(`text`, `eol`, `ident`), so commits and merges come out as your own git would make them.
What kGraf sets: `core.autocrlf=false`, `core.eol=lf` and `core.safecrlf=false`, for its own
git and for the git a worker runs, so a worker's worktree holds the bytes its commit holds
whatever line-ending setting your machine has (Git for Windows sets `core.autocrlf=true`). A
file with no `text` or `eol` attribute is committed as written; a path your `.gitattributes`
marks, for example `*.bat eol=crlf`, is converted as it says. Your own checkout is not
changed. Two more differences follow. A file your attributes route through a filter, such
as git-lfs or git-crypt, is committed and checked out by kGraf in its stored form, not
filtered. A file with a configured merge driver is reported as a conflict when two attempts
change it, rather than resolved by the driver. kGraf needs git 2.31 or newer for this and
refuses to run an older one.

Your own git is unchanged: when you commit, merge or check out yourself, every hook, filter and
driver runs as before.

## What lives where

State lives in `.kgraf/` inside the repository: the index, the ledger, run records,
worktrees, schedules and allow rules. kGraf adds `/.kgraf/` to the repository's
local exclude file (the `info/exclude` that `git rev-parse --git-path info/exclude` names,
so a linked worktree works too) and never edits your tracked `.gitignore`. If the
repository already tracks files under `.kgraf/`, it warns and names the command that
untracks them; it never changes your index. It holds the ledger, the allow rules and the
run records, so never commit it. Nothing leaves the machine except calls to the providers
and MCP servers you configure, and those calls carry code from the repository.

## Coming from Haustoria

kGraf was earlier named Haustoria. What the former name left
keeps working, and each move prints one notice:

- A project's `.haustoria/` moves to `.kgraf/` on first use. Git worktree registrations,
  attempt refs and the absolute paths in run records and configuration are repaired; the
  ledger, transcripts and traces move unchanged. A project that has both directories is
  refused until you keep one. The move is also refused, before anything changes, when
  git cannot read the repository (or is missing while worktrees exist), when an older
  process still works in a worktree, or when the old directory holds a symbolic link
  where the move would write. Anything that still names `.haustoria` after the move is
  listed in the notice.
- A `HAUSTORIA_*` variable (`HAUSTORIA_KIMI_API_KEY`, `HAUSTORIA_LOG`, ...) is still read
  when its `KGRAF_*` name is not set, with a warning naming the new variable.
- An MCP OAuth token stored under the keychain service `haustoria-mcp-oauth` moves to
  `kgraf-mcp-oauth` the first time it is read.
- The desktop app's settings and log directories move from `Haustoria` to `kGraf` on its
  first launch. On Windows, the kGraf MSI upgrades an installed Haustoria; on Debian,
  the `kgraf` package replaces `haustoria`.
- A worker transcript recorded before the rename does not resume: the worker's system
  prompt names the product, so the prefix differs and the runtime refuses, saying the
  transcript predates the rename. Finish or stop running work before you upgrade.

## Changes users will notice

Behavior that differs from earlier builds. Each item names the decision that holds the reasons.

**Cost and spend**

- A model with no price entry runs, and its spend is shown as unpriced (a local model as
  local, unpriced), never as zero dollars (0075).
- A model priced from the bundled catalog is priced only up to the input size where its vendor
  changes rates. A larger call is shown as unpriced, not costed at the base rate. A price
  entry you set yourself replaces the catalog's tier for that model (0075).
- `kgraf spend` prints `total unpriced` when no recorded model has a price, and
  `total <amount> (partial: some calls could not be costed)` when some calls could not be
  costed (0075).
- Money is summed at full precision and rounded once, so a total can differ by a few
  micro-dollars from a sum of separately rounded calls (0075).
- A benchmark attempt reads as unpriced only when the price table cannot cost its calls. A
  ledger read error stops the attempt with the error.

**The ledger**

- A settle is accepted only for a call that is in flight, and a reconcile only for a call that
  is unreconciled or in flight. Anything else is refused with the call's state, so a resolve
  cannot overwrite a counted row and a late settle cannot overwrite a reconciled figure.
- `kgraf unreconciled` lists each open call with its state (`in_flight` or `unreconciled`) and
  the usage recorded so far, which is a lower bound. A call still in flight needs
  `--abandoned` before `resolve` will close it.
- `kgraf unreconciled resolve` keeps the recorded figure for any field you leave out. Only
  `--no-charge` states zero for every field.
- When an Anthropic-protocol endpoint sends a usage object that does not add up (a cached
  count above the cache read, or a total that differs from the parts), the reply is returned,
  the call stays unreconciled with the counted fields as a lower bound and a note in the raw
  usage, and a warning is logged. Resolve it as any other open call (0006).

**Runs**

- On Windows the worker's shell tool runs commands. Earlier builds refused every call there.
  `sh` and `bash` run with Git Bash, the network is open and the tool says so, and a missing
  Git Bash fails the call with how to fix it (0011).
- On Windows the verifier's check, the gates and the contract checks accept the sandbox's
  partial isolation, which the Windows backend always has; earlier builds refused them
  before they started (0011).
- A contract waiting on a route that is not ready is held after 2,400 checks from the first
  failure (20 minutes at the default tick). The hold says how long it waited and how to try
  again: fix the route, then `kgraf orchestrate --resume <run>`. Earlier builds waited without
  end (0075).
- A contract whose route is not ready moves the rotation on once for each route it finds not
  ready, not once for each check (0075).
- A route whose calls fail at the transport (the server refuses or drops the connection) is out
  of rotation for 30 seconds, doubling with each failure in a row up to 10 minutes. Its
  contracts move to another route, and that attempt does not count against the contract. With
  no other route, the contract waits and tries the route again; after three failures in a row
  it is held with a reason naming the route. The run output has a `notice` line for each route
  that failed, and the trace has `route_fault`, `rerouted` and `route_recovered` records
  (0075).
- A worker's shell command written as one string (`["cargo test --workspace"]`), or with the
  program and its leading words in the first element (`["cargo test", "--workspace", "-q"]`),
  runs as its words when it holds no shell syntax, and the output says so. One with shell syntax (`&&`,
  quotes, `$`) is refused with the way to write it. A string that holds a JSON-encoded list of
  strings (`"[\"python\", \"-c\", \"print(1)\"]"`) runs as that list, each element one argument,
  and the output says so. The build commands run without approval
  with their usual options: `--workspace`, `-p`, `--test`, `--release`, `--no-run` and the
  others the shell tool's description lists (0084).
- Every worker's tool list now includes `replace_in_file(path, old, new)`: it replaces the one
  place where `old` occurs exactly, with no line prefixes and no context lines, for a small
  edit. If `old` occurs more than once or not at all, the call fails and says how many places
  it found (and the nearest line when there is none). An empty `old` creates a file that
  does not exist. It follows the same write scope, protected paths, sandbox confinement, and
  Python and YAML syntax check as `apply_patch`; a deny rule on `apply_patch` covers it. A
  path under `.git` or `.kgraf`, and the files Bypass mode admits to `apply_patch`, are
  refused with a pointer to `apply_patch`. The tool list is part of every worker's shared
  prefix, so a worker transcript recorded by an earlier build cannot be resumed; start that
  work again (0099).
- `apply_patch` also reads, with a note in the result: a git `\ No newline at end of file`
  line (ignored), a blank line with no `+` between `+` lines of an Add File body (an empty
  line of the file), and a line with no ` `, `+` or `-` prefix in an Update File hunk (a
  context line, matched against the file like any other) (0084).
- A worker's `apply_patch` patch that starts with a file header (`*** Add File:`,
  `*** Update File:`, `*** Delete File:`) and lacks `*** Begin Patch` is applied as if it had
  the envelope, and the result says so. So is a patch with no `*** End Patch` line or a
  mangled one, a Delete File plus an Add File of one path (applied as a replace), and an
  `@@ -a,b +c,d @@` header (its numbers are ignored; the text after it is the context line);
  each is a note in the result. Lines match exactly first, then ignoring trailing whitespace,
  indentation and typographic punctuation; a looser match that fits in several places is
  refused, and one that fits in one place is noted. A looser match keeps the file's own copy
  of each unchanged line, and moves the `+` lines by the one indentation shift the matched
  lines show between the patch and the file (the note says by how much); when the matched
  lines are not shifted by one amount, the patch is refused, because the indentation of the
  `+` lines cannot be known. A Python or YAML file that a looser match would leave
  unparseable is refused before it is written. An Update File chunk of only `+` lines is
  refused (it names no place; it used to append at the end) unless the file is empty or the
  chunk ends with `*** End of File`. Any other malformed patch is refused. From the second
  patch a worker has rejected, the rejection ends with the rule it broke and a minimal valid
  patch for the file it targeted (0084).
- A worker turn with no tool call and no visible answer (a plan only in the model's reasoning)
  is not a finished attempt: the worker is told to continue, once. A second empty turn in a
  row fails the attempt, and the failure names the stop reason (0096).
- A server that cannot parse the model's own tool call answers HTTP 500 (Ollama does:
  `XML syntax error on line N` or `error parsing tool call`). The worker sends that round
  again, at most twice in a row, with one added note: "Your previous reply contained a
  malformed tool call ... Send one well-formed tool call." Each try is its own ledger call; the
  failed one stays unreconciled like any 5xx. After the second retry, or on the last round, the
  attempt fails and its error names the parse failure. A bare `{"error":{"message":"EOF"}}`
  500, or a message containing `unexpected EOF`, counts as the same failure (the field is
  read, not the text around it). Any other 5xx is not retried (0039).
- Every orchestrator call that asks a model for JSON (re-grounding, the planner's calls, the
  question answer, the connection judgment, handshakes, consumer tests, challenge and
  interface reviews, question routing, the corpus interpretation) asks again when the answer
  is malformed: not the complete JSON object asked for (for example cut off mid-string with a
  normal stop), or a server that could not parse the model's output. The bound is the
  worker's: at most two more tries, each its own ledger call. A refused answer is asked again
  with the refusal quoted, so the model sees where it broke. A retry that then parses is a
  run notice ("the re-grounding answer was malformed and was asked again 1 time(s) ...").
  After the bound the call fails, and its error ends "the call was made 3 times and every
  answer was malformed"; for re-grounding the step is held with that reason. An answer cut
  at the output limit is not asked again. An `error` object a server sends after it already
  answered 200, in the body or mid-stream, is an error and never part of the answer; one
  that names a model-output parse failure (`EOF`, `error parsing tool call`) is retried as
  the 500 is (0039).
- An orchestrator answer the model ended itself before its JSON object closed no longer
  costs an ask. When only closing brackets and braces are missing after a complete value,
  kGraf appends them, reads the answer as any answer, and says so in a run notice ("the
  interfaces answer of call ... ended before its JSON object closed; kGraf appended `}` to
  close it ...") and in the trace (`answer_repaired`). When it cannot be closed (it stops
  inside a string, after a key, a comma or a number), the same request is sent once more,
  with one sentence saying the previous answer was incomplete. That call is ledgered and
  accounted like any other but does not count as one of the asks; a notice and the trace
  (`answer_reasked`) say so. A second incomplete answer is refused and counts as before. An
  answer cut at the output limit is unchanged (0030, amended 2026-10-09, fourth).
- A worker whose contract writes files and which ends an attempt as done with no successful
  write and no command that changed the tree (it only said what it would do, or only read)
  has not finished. The turn is dropped and the worker is told: "You ended without making any
  change; this contract writes <paths>. Your reply said: "<what it wrote>" It called no tool,
  so nothing ran. Call apply_patch now to create or edit those files (or shell for a command
  that must change them). If the work cannot be done, call report_shortfall." The quote is at
  most 400 characters, and a tool is named only when the worker has it. The next request
  requires a tool call where the provider can send that field and the route is not recorded
  as ignoring it (Ollama is; see [MODEL-SUPPORT.md](MODEL-SUPPORT.md), Known limits); the
  worker's transcript records each such decision and why (a `tool_choice` line, shown as
  "tool call required" or "tool call not required"). An attempt gets two such notices, and
  the second says it is the last. If it ends the same way again, the attempt fails ("... told
  to continue 2 time(s) ...") and the failure names the contract's write paths. A contract
  that writes nothing, and an attempt that wrote or changed something, end as before (0096).
- A contract whose pinned context (the prefix, its slices and its instruction) already reaches
  the worker's stop point cannot run one round, and a second attempt of the same brief fails
  the same way. It is not retried: the contract is held with the reason and the remedy (split
  the contract, use a route with a larger window, or raise the stop point) (0096).
- When a worker needs a file that no contract of the plan writes and nobody attends the run,
  the hold says the plan needs a contract for that file and to re-plan. It does not suggest
  `--allow-path`, which would let one attempt write a file the plan never assigned. A file
  another contract writes, or a command, keeps the permission wording (0093).
- Before a plan is accepted, two checks read your repository. A file the instruction names
  that holds placeholder bodies (`todo!()`, `unimplemented!()`, `raise NotImplementedError`
  outside an abstract method, `throw new NotImplementedException()`) and that no contract
  writes is refused, naming the file. Only code counts: a file that
  mentions `todo!()` inside a string literal, a raw string or a comment holds no placeholder.
  Files are lexed with the grammar of their language; a language kGraf has no grammar for is
  read line by line, skipping comment lines. A contract whose acceptance runs cargo in a
  Cargo workspace is refused when a member's manifest does not exist and neither that contract
  nor one it depends on writes it. Cargo loads every member's manifest before any command, so
  this holds for every form: `--workspace`, `-p <package>`, `--manifest-path
  <member>/Cargo.toml`, `cd <member> && cargo test`, a bare `cargo test` and `cargo fmt`. A
  member manifest needs a source file too (`src/lib.rs` or `src/main.rs`); cargo refuses one
  with no targets. The planner is asked again with the reason and the fixes that build: the
  contract that writes the root `Cargo.toml` also writes a minimal manifest and source for
  each member it lists, or the root lists only the members that exist and each later member's
  contract adds itself. A worker that still asks for such a file, written only by a contract
  that runs later, holds its contract as a plan-shape error that asks for a re-plan (0030).
  A file that only a finished, integrated contract listed, and no open contract writes, is
  handed to the asking contract in an unattended run, with a notice naming the finished
  contract (0030, amended 2026-10-11).
  The member list is read from a `members = [...]` list in the root contract's instruction,
  and also from prose such as "the workspace root Cargo.toml with members wcl-core and wcl":
  after the word `member` or `members` to the end of the sentence, a word counts when the plan
  writes or the repository holds `<word>/Cargo.toml`. Before, a list in prose was not read,
  the plan was accepted, and the root contract failed on the later member's missing manifest.
  The same check covers a Go workspace and a Maven reactor: a `go` command loads every module
  the nearest `go.work` uses, and `mvn` every `<module>` of its `pom.xml`, so a member whose
  `go.mod` or `pom.xml` comes from a later contract is refused the same way. npm and uv
  workspaces skip a missing member, and Gradle and pnpm depend on their version, so those are
  not checked (0030).
- A worker that runs its contract's build script (`python -m build`, `pip wheel`, setuptools
  or Poetry) leaves `dist/*.whl`, `dist/*.tar.gz`, `*.egg-info/` or `build/lib*/` beside the
  project's `pyproject.toml`, `setup.py` or `setup.cfg`. These new files are not the worker's
  writes: they are left out of the attempt and removed before the check, with a notice to
  add them to `.gitignore`, as a worker-made lockfile and `__pycache__` are (0084). Before, the
  attempt failed with "wrote outside the declared scope" for them.
- The same holds for every language's own install, build and test output, which a worker's
  run leaves in the tree when your `.gitignore` does not name it (cargo's goes to the
  worker's scratch directory and never reaches the tree): `node_modules/`, `dist/`,
  `coverage/` and `.next/` beside `package.json`; `build/` beside `package.json`, a Gradle
  build file or `CMakeLists.txt`; Maven's `target/` beside `pom.xml`; `bin/` and `obj/`
  beside a `.csproj`; `.venv/`, `venv/` and `htmlcov/` beside a Python project file;
  `.bundle/` beside a `Gemfile`; `vendor/` beside `composer.json`; files `tsc` emits beside
  their `.ts` source; tool caches anywhere (`.pytest_cache/`, `.mypy_cache/`, `.tox/`,
  `.gradle/`, `.coverage`, `.eslintcache`, ...); `.class`, `.o` and `.tsbuildinfo` files; and
  a program a compiler wrote (`go build`, `make`, `cc`), told by its first bytes. One notice
  lists the build output and one the caches. The same folder names with no project manifest
  beside them, a tracked file a build changed, and an undeclared source file are still writes
  outside the scope (0084). These files do not count toward the commit's size bounds (one file,
  and 512 MiB in total), they are not in the worker's list of what its build changed (one line
  gives their count), and the progress measure does not hash them, so a large install costs
  the run nothing. Lockfiles are recognized for Cargo, npm, Yarn, pnpm, Bun, Poetry, uv, PDM,
  Pipenv, Bundler, Go, Composer, Nix, Mix, Dart (`pubspec.lock`), SwiftPM
  (`Package.resolved`), Gradle, NuGet (`packages.lock.json` beside a project file) and
  CocoaPods (`Podfile.lock`), each beside its manifest (0084).
- Go builds run in the sandbox: the go command's build cache is `GOCACHE=<scratch>/go-build`,
  a cold cache per worker, because the default location is outside what a sandboxed command
  may write. On Linux the module cache under `~/go/pkg/mod` is readable (0084).
- `apply_patch` writes a patch indented with spaces into a tab-indented file (Go, a Makefile)
  with the file's tabs, when every matched line shows the same number of spaces per tab; the
  result says so (0084).
- A Python or Ruby module whose base class raises `NotImplementedError` for subclasses (an
  `@abstractmethod`, a class based on `ABC` or `Protocol`, a class another class in the file
  derives from, or a message that says a subclass supplies it) is not a placeholder. A plan
  may name it without writing it. C#'s `throw new NotImplementedException()` is a placeholder
  (0030).
- An attempt that changed nothing is refused when a file it was to write is still a scaffold:
  every function body one placeholder (`todo!()`, `raise NotImplementedError`,
  `throw new NotImplementedException()`, `throw new Error("not implemented")`,
  `panic("not implemented")`). This reads every language kGraf has a grammar for (Rust,
  Python, TypeScript, JavaScript, Go, Java, C, C++, Ruby, C#, PHP); before, only Rust. A body
  with real code, `pass`, `...` or nothing, an abstract method, and a file with no function
  are never a scaffold (0086).
- A `package.json` whose `test` script is the one `npm init` writes (`echo "Error: no test
  specified" && exit 1`) adds no `npm test` gate; the run says the gate does not apply (0018).
- Gates are discovered for Go (`go test ./...`), Maven (`mvn -o test`), Gradle
  (`./gradlew test --offline` with a Gradle wrapper, `gradle test --offline` without) and .NET
  (`dotnet test`), each only when its tool is there and the project's dependencies are
  installed. A gate that could not pass on correct work is left out with the reason and the
  command that fixes it: an uninstalled dependency of an `npm test` gate, pytest missing from
  both the project's virtual environment and `python3`, a Gradle wrapper whose distribution
  is not unpacked in `~/.gradle/wrapper/dists` (run `./gradlew --version` once). Set `--gate-command` to run a gate yourself (0018).
- A worker that reaches its context stop point before it has written anything is compacted
  once instead of handed back: a child would receive the same whole task and meet the same
  stop point. After that one compaction it goes on while its context fits the model's window;
  past the window it hands back. A worker that has written something still hands back. The
  trace's `compaction` record for a hand-back has a `cause` field: `measured_context` (the
  last reported usage plus the estimate of what was added since reached the stop point) or
  `queued_messages` (the queued steering messages pushed the next round past it). A hand-back
  is never decided from a projection (0095).
- A compacted worker that has written nothing and ends as done is told once to continue, and
  fails on a second such end, as any worker is; one that wrote before its compaction completes
  normally. A file the worker read before its compaction may be read again after it without a
  stall; three identical calls after the compaction still stall. When the model's summary is
  unusable, the summary kGraf writes in its place still names the goal (the instruction's first
  line) and lists each declared file not yet changed as an open item (0095).
- A retry after an attempt that stopped before any verifier saw it carries what stopped it, as
  data, the way a verifier's refusal is carried: a stall names the repeated call, a spent step
  budget says so, a runtime failure gives its reason, and each lists the calls a policy refused
  that no later call got past (the rule or policy and its text). The contract record keeps it
  as `last_stop` (0018).
- A worker the stall check or its step budget ended, after it changed files, still has its tree
  verified as it stands, the same way a finished attempt's is. A pass accepts the work, and the
  run prints a notice that the worker was stopped before it finished. A failed check, a check
  that checks nothing (`true` with no gate command), or a verifier that cannot run fails the
  attempt as the stop alone did. The verifier's refusal then goes to the retry in place of the
  stop note (0009).
- A permission event never says `granted` for a call that a later rule refused. A mode's grant
  is recorded when the approval chain lets the call run; when a deny rule (or another hook)
  refuses the call after the mode allowed it, the trace has one `grant_refused` event naming
  the subject, the mode, and the refusing hook with its reason (0094).
- What a runner, a Windows shell, a git setting, a build-tool recipe or interpreter code runs
  is a command (0094): with `deny = ["shell(curl*)"]`, `npx -c 'curl x'`, `docker exec web curl
  x`, `cmd /c curl x`, `powershell -EncodedCommand ...`, `git -c core.pager='curl x' log`,
  `make deploy` whose recipe runs `curl`, `npm run deploy`, `just deploy`, `python3 -c
  "import os; os.system('curl x')"` and `python3 x.py` whose code does the same are refused in
  every mode, Bypass included. A command named by a variable (`$CMD x`), a runner whose command
  kGraf cannot name, a missing recipe file and a spawning call with arguments that are not
  literal ask in every mode while you have a shell rule with a pattern. A grant of the runner,
  the shell, `git`, the build tool or the interpreter no longer grants what it runs: with
  `npm` in the allow set, `npm test` asks unless its script's programs are there too. A
  shortfall report of a command that runs several programs (`env curl x`) names each program
  that still lacks a grant, the innermost first, and the orchestrator grants them together.
  With no rules, `cargo test` and `pytest` are decided as before in every mode, and so is
  `npm test`, except in Auto when the allow set lacks its script's programs.
- A program given to another program as an argument is a command (0094): your deny and ask
  rules, the refusal to delete system paths and your policy's `forbid` rules see `find -exec`,
  `su -c`, `ssh HOST CMD`, `script -c`, `trap`, `watch`, `parallel` and the like in every mode,
  Bypass included. A command that runs what kGraf cannot read (`source FILE`, `bash run.sh`)
  asks in every mode while you have a shell rule with a pattern. A grant of a program no longer
  grants what it runs: with `env` in the allow set, `env curl x` asks unless `curl` is there
  too, and `watch`, `flock`, `find -exec` and `ssh HOST CMD` are never granted by a program
  grant. Before, these ran past your rules, and a grant of the wrapper granted everything it
  ran.
- In Auto, Accept edits, or with a program granted to the contract, a worker's `sh -c` script
  of simple commands runs without asking when every command's program is granted; before, the
  shell itself had to be granted, so `sh -c "mkdir -p out && cp a out/a"` was asked about (and
  denied with nobody attending) under `--allow mkdir --allow cp`. A script with one command no
  grant covers is asked about, naming that command (0094).
- A worker's `sh -c` script of allowlisted cargo commands, after at most one `cd` into a
  directory of the worktree, runs without asking as a build in every mode. Before,
  `sh -c "cd pricing_engine && cargo check"`, the usual shape of a planned acceptance, was asked
  about and denied with nobody attending, though `cargo check` alone ran (0084, 0094).
- A worker denied a `sh -c` script whole that then runs each program the script held, in any
  form (`cargo test -p a` after `sh -c "cargo test -p a"`), no longer leaves its contract
  reported unverified (0093, 0094).
- A script using variables, subshells, background jobs, input redirects, escapes or outside
  redirects can be granted when you turn on that construct's switch (Settings > Permissions,
  `script_constructs` in `permissions.toml`, or `--script-constructs`); all are off by default.
  Only the programs kGraf can see are checked, and your rules also match program names in the
  script text (0094).
- A skill's refusal of a shell call, or a rule on the whole `shell` tool, is recorded as a
  refusal of the tool: a report naming the `shell` tool is held naming the skill instead of
  being answered as nothing refused (0053, 0094).
- Every worker brief says what its attempt is judged by. When `--verify` cannot be narrowed to
  the contract's tests (`cargo build --workspace`, a script), the brief names the whole command,
  calls the contract's Acceptance line advisory, and says `todo!()` panics and failing tests in other contracts' unfinished
  code are not the worker's to fix, and says a compile error anywhere that the worker's change
  caused (for example by changing a public type's shape) is its own (0018). It also says that a
  command that can only pass once other contracts merge is checked on the merged result, and
  that the worker must not write other contracts' files to make it pass.
- The planner is told that the repository's existing public items (type names and shapes, trait
  signatures, how callers use them) are the interface. A contract instruction that changes the
  shape of a public item that a file the contract writes defines produces a notice at plan intake;
  the plan is not refused, because the instruction may ask for the change. kGraf reads the code
  the instruction shows (backtick spans, fenced blocks, bare `struct`, `enum`, `trait`, `fn` and
  `impl` items) and compares it with the file's public declarations: a struct's kind and public
  fields, an enum variant's fields, and the signature (parameter types, receiver, generics,
  return type) of a trait method, a method or a function. Parameter names do not count. An
  item the repository does not define is an addition, and a private item may change; neither is
  noticed. For an instruction that shows no code, a sentence that names a public item in
  backticks with a verb of change ("change `Act::act`", "add a parameter to `run`") is noticed
  too, unless it says to keep it. Rust files are read by structure; other languages get only the
  unit-struct check (0018). When a worker's
  attempt is refused for a compile error in a file its contract does not write, the retry is
  amended once with the error, the file, and the instruction to keep the public shape that existing
  callers use, instead of being sent again unchanged (0018). A worker that reports its
  instructions insufficient by asking for a change to a file another contract owns is answered, not
  re-dispatched (0093).
- A planner answer is repaired where its intent is certain, with a notice in the run output: a
  task-kind word in a contract's `route` (`implementation`) is read as no route, and an absolute
  write path inside the repository becomes the relative path. A write to `.kgraf` or `.git` is
  refused with a reason that names protected state (0085).
- A run started from the command line or a schedule is unattended: a worker's `ask_user_question`
  that the orchestrator cannot answer from the run record gets an answer at once, telling the
  worker to state its assumption, instead of waiting 60 s (0067). A run started from the
  orchestrator box still waits for you.
- The wait for the orchestrator's own answer to a worker's question is no longer a fixed
  300 s: it is derived from the answering model's recent latencies, or set with
  `answer_wait_secs` (Settings > Runs > Answer wait, `--answer-wait`). An answer that comes
  after the wait reaches the worker at its next round instead of being discarded (0067).
- Workers are told that kGraf commits their worktree after its check, and the planner no
  longer writes acceptance that asks a worker to commit.
- A planner answer that sends an optional list as `null` (`"depends_on": null`) is read as
  empty. A field of the wrong type is still refused, and the refusal names the field
  (`contracts[0].depends_on: ...`).
- A planner answer that gives a contract's acceptance as a list of strings is read as those
  criteria, in order, one per line in the worker's brief. A contract's acceptance is text
  the worker reads; kGraf does not run it. The command that checks each attempt is
  `--verify`, narrowed to the contract's own tests where the runner allows. Any other type
  (null, a number, an object, a list that holds a non-string) is still refused, and the
  refusal names the field.
- Each worktree a run leaves on disk (a held contract's attempt kept for `--resume`, retention,
  a conflict) gets a `kept` line in the run output with how to resume or remove it. kGraf's own
  `.kgraf/worktrees/.holds` and `.kgraf/worktrees/.scratch` are removed at the end of a run that
  kept no worktree; they are internal, not worktrees (0088).
- A file made only of plain structs and trait impls whose bodies are all `todo!()` or
  `unimplemented!()` counts as an abandoned stub. An enum, a constant, a macro call, an
  inherent impl, anything under `cfg`, or a struct with a non-standard derive marks the file as
  finished code (0086).
- A background process that leaves a worker's process group keeps the worktree from being
  deleted. The worktree stays, with a warning; stop the process, then run `kgraf clean
  <worktree>` (0088).
- A worker at the end of a chain of hand-backs (the delegation depth cap) no longer stops its
  contract when its context fills. kGraf compacts that worker once and it goes on: one
  summary call on the worker's own route, recorded in the ledger as `worker_compaction`; the
  worker's instruction is shown to it again unchanged; its earlier history stays in its
  transcript, and the worker can search it with the `recall_history` tool. The run report
  counts the compaction with the hand-backs (`misses`), the trace has a `worker_compacted`
  record naming the transcript file, and the run record lists it under `compactions`. A
  worker that fills its context again is compacted again while it is making progress: since
  its last compaction a file it works on changed or a test or build result improved, with no
  run of failed writes or repeated errors (a failed write of a file that a later write of the
  same file made good does not count). A worker with no such progress, one whose context
  cannot get under the stop point even after a compaction, or one that reached the
  compaction ceiling (the progress checkpoint ceiling, `--max-checkpoints`, default 10; 1
  allows one compaction) hands back, and its contract is held with a reason that says which,
  and names the worktree and the transcript. The run report counts each compaction, and a
  worker as one miss however often it compacted (0095).
- A hand-back's child starts with room to work. Its instruction quotes the original instruction
  once, with a line that says which hand-back it is, instead of quoting each earlier brief
  inside the next. Its pinned slices are cut to what is left after the prefix and the
  instruction under the plan target (half the stop point), each slice counted as rendered; a
  notice says how many were kept, and the worker reads the rest with its tools. When the prefix
  and the instruction alone pass the plan target, no child can fit: the worker is spawned to
  compact at its stop point instead of handing back (again at a later stop point while it makes
  progress), and a re-split that is still asked for is refused with the numbers and the reason
  the worker was not compacted again (0095).
- Every worker's tool list now includes `recall_history`, so a worker transcript recorded by
  an earlier build cannot be resumed; start that work again (0095).
- `/compact <worker>` is refused once that worker has compacted, whether you asked for the
  compaction or kGraf made it at the stop point (0095).
- A contract whose named graph slices all find nothing, or that names none, is given the
  top-level symbols of the files it declares it writes (at most 8, about 4,000 tokens
  together). The trace has a `notice` naming the files, and each name that found nothing is
  still an `empty_graph_lookup`. The run report has a `slices` line: the dispatches given
  slices, how many of those took the declared files' symbols, the named lookups that found
  nothing, and the slices left out of a brief. A run that gave no worker a slice says so on
  that line.
- A worker's slices are read from its own worktree, at the commit it was dispatched from,
  not from your checkout. A slice whose file is not in that tree, holds another version
  there than the graph indexed, or cannot be read (or whose node the graph no longer holds)
  is left out, and the worker starts anyway: its opening ends with a `slices left out` block
  naming each node, its file and the reason, and the trace has one `notice` per slice
  (`slice left out of the brief: node ... in ...: reason`). A read error carries the
  operating system's reason. Only the graph's database failing stops a worker at its start.
  The `graph_slice` tool reads the worker's tree first too, and the indexed copy when the
  worker has changed the file since.
- A conversation file the message bus writes is indexed from a worktree that is removed once
  the file is committed. After the commit kGraf indexes it again from the run's integration
  view, and after each merge it points any indexed file whose recorded tree is gone at the
  view when the view holds the same content, so those nodes stay readable for the rest of
  the run.
- A verified contract's report to a contract whose attempt is running is injected into that
  attempt at its next round boundary, quoted as data, never mid-stream or inside an open
  tool round. Before, it waited for the recipient's next fresh brief, and a recipient that
  finished without one never got it. The trace shows `message` `pending` (`queued for
  <attempt> at its next round boundary`), then `delivered` (`injected into <attempt> at a
  round boundary (transcript seq N)`, at the time of delivery). If the attempt ends before a
  boundary, the next fresh brief carries the report, and a resumed attempt gets it at its
  first boundary. A report still undelivered when its recipient closes is traced as
  `undelivered`, and the sender's contract gets a notice (`your message <id> to <contract>
  was not delivered: ...`).
- A message another agent wrote (a worker's `send_message`, a subagent's report) reaches its
  recipient quoted as data: one line saying it is information about the sender's work, not
  instructions, then an escaped `<agent_message id="..." from="<sender session>"
  kind="...">` block. Before, it arrived as a plain user turn, so one model could instruct
  another. Your own notes from the session panel and kGraf's notices are not quoted (0067).
- `send_message` tells the sending worker `queued: it reaches <session> at its next round
  boundary, ...`, not `delivered`. Delivery is the recipient's transcript (0067).
- A subagent's report is traced `report_queued` when it is handed to its parent, and
  `report_delivered` (`injected into <parent> at a round boundary (transcript seq N)`, at the
  time of delivery) only when it reaches the parent's transcript. Before, `report_delivered`
  was written at the hand-off. A parent that stops before a boundary leaves `report_held`
  (it gets the report when it resumes); one that ends leaves `report_undelivered`, and its
  contract gets a notice (`the report of subagent <child> was not delivered to <parent>:
  ...`). A parent that answered before the report was handed over still shows
  `report_after_parent_finished`, now with the same notice (0067).
- When a run ends (completed, blocked or stopped), every note still not delivered is traced
  `message` `undelivered` with the reason, and its sender gets a notice: a mailbox report to a
  contract that was held or never started (`the recipient <contract> never started before
  the run ended blocked`), and a note or message waiting in the inbox of a session that never
  ended (`<session> was held when the run stopped`). Before, such notes were reported only
  when the recipient closed, so a run that ended first lost them unseen. Each note is
  reported once per recipient; a resumed run can still deliver it (0067).
- A report carried in a contract's fresh brief is traced `delivered` (`in the brief of
  <attempt>`) when that attempt's first model request leaves, not when the brief is built.
  An attempt that fails before any request (for example, its pinned context is too large)
  leaves `message` `pending` (`<attempt> ended before its first model request, so no model
  read its brief: ...`), and the report stays undelivered for the next fresh brief or the
  undelivered report (0067).
- A run that ends on an error also reports its undelivered notes, with the reason `the run
  ended with an error`; the error the run returns is unchanged. A note waiting in a session's
  inbox is reported once over every run of the record: a resumed run that ends again does not
  report it again (0067).
- After a restart, a subagent report the parent's transcript already holds is traced
  `report_delivered` once, from its inbox row's mark. Before, it could stay with no fate in
  the trace, or be called `report_after_parent_finished` although the parent had read it
  (0067).
- A stop leaves every subagent of a stopped parent resumable, and its session shows paused,
  whether the stop cancelled the parent mid-call or found it finished and waiting for its
  children (those keep running until a second stop or the stop's grace). A resume continues
  them and clears the paused mark. Before, a child whose parent was cancelled mid-call was
  resumable but showed neither paused nor ended, and a second stop that came just before a
  parent finished did not stop that parent's children, so the run waited on them (0067).
- A subagent you stop and resume from its session gets the reports and messages sent to it.
  While it is paused they wait in its inbox (`report_held`), and it gets them when it runs
  again. Before, a resumed subagent refused them as if it had already answered, and ran on
  without them (0067).
- kGraf's own git runs no hook, filter, merge driver or diff driver from your repository or
  config, and reaches no remote (see "kGraf's own git"). A file stored through git-lfs or
  git-crypt is committed by kGraf in its stored form, and a file with a configured merge
  driver conflicts when two attempts change it. git 2.31 or newer is required (0094).
- In Bypass, workers may write `.gitattributes`, `.gitmodules` and `.githooks` when the
  contract declares them; they merge as ordinary files (0094).

**Reasoning level**

- One `reasoning` setting sets the reasoning level of every model call, not only the planner's:
  `--reasoning` on `orchestrate` and `worker`, `reasoning` in `run.json`, `[reasoning]` in
  `settings.toml`. Workers, compaction summaries and the contract-check calls send no reasoning
  field unless it is set (0098).
- A graded level measured to have no effect on a route and model stops the run when it opens;
  an unmeasured graded level is sent and said as `UNVERIFIED` at the run's start (0098).

**Scheduled runs**

- A manual run (`kgraf schedule run-now`) keeps its lock when the machine slept past the lease
  and nothing else took the schedule. The run is reported as failed only when another manual
  run or a scheduled claim took the lock.

## Installers

Local packaging scripts build a platform-native artifact from this tree:

| Script | Artifact | Signing |
|---|---|---|
| `scripts/package-macos.sh` | universal (Apple silicon and Intel) `kGraf.app` and `.dmg` | ad-hoc only; notarization runs when a Developer ID exists |
| `scripts/package-linux.sh` | tarball and `.deb` | none |
| `scripts/package-windows.sh` | x64 `.msi` (built with `wixl`) and `.zip`, cross-built with `cargo-xwin` | none |

### Linux: the AppArmor profile

Ubuntu 23.10 and later block user namespaces for a program that has no AppArmor profile
granting them (`kernel.apparmor_restrict_unprivileged_userns` is 1). kGraf needs user namespaces
for installs limited to the package registries and for hidden folders. Without them kGraf
refuses those and says so.

The `.deb` installs `/etc/apparmor.d/kgraf` and loads it. The file holds one profile for
`/usr/bin/kgraf` and one for `/usr/bin/kgraf-app`. Each grants only `userns`; every other
permission stays as if there were no profile. Local additions go in
`/etc/apparmor.d/local/kgraf`. A system without AppArmor skips the load.

To check that it is loaded:

```
sudo aa-status | grep kgraf
cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns
```

For a tarball install, run these from the unpacked folder:

```
sudo install -m 644 share/apparmor/kgraf /etc/apparmor.d/kgraf
sudo apparmor_parser -r -W /etc/apparmor.d/kgraf
```

If you put the programs somewhere other than `/usr/bin`, change the two paths at the start of
the profiles first. The profile attaches by the path of the program.

Each package carries `LICENSE` and `THIRD_PARTY_NOTICES.md`. The version comes from one
place, `[workspace.package].version` in the root `Cargo.toml`, and
`scripts/check-release-version.sh` fails when it does not match the highest `v*` tag's
number. The packages are not signed with a developer certificate yet, so expect Gatekeeper
and SmartScreen warnings. `scripts/release-all.sh` builds every installer from one git
tag. A `cargo-dist` workflow in `.github/workflows/release.yml` builds draft releases from a
version tag when started by hand; pushing a tag does not start it.

### Windows: Git for Windows

kGraf needs git on every platform. On Windows it also runs `sh` and `bash` with Git Bash,
which comes with [Git for Windows](https://gitforwindows.org/). The `.msi` and `.zip` do not
carry Git for Windows: it is a separate package under its own license.

At each start, `kgraf-app.exe` and `kgraf.exe` look for Git Bash. They check, in this
order: `KGRAF_GIT_BASH_PATH`, the `InstallPath` of the `SOFTWARE\GitForWindows` registry key
(`HKLM`, then `HKCU`), each `PATH` folder that holds `git.exe`, and the usual install
folders. When Git Bash is missing:

- If `winget` is available, kGraf asks once whether to install Git for Windows for your
  user account. On yes it runs `winget install --id Git.Git -e --scope user --silent
  --accept-package-agreements --accept-source-agreements --disable-interactivity` and
  looks again. The install needs no administrator rights. A start with no terminal (a
  script, a pipe) cannot ask, so it stops with the same text and command.
- If you answer no, `winget` fails, or `winget` is not there, kGraf stops and shows the
  download link, the `winget` command, and `KGRAF_GIT_BASH_PATH`. It checks again at the next
  start.

`kgraf diag` and `--help` run without the check, and so do `kgraf-app --measure` and
`--screenshot`.

## Diagnostics

kGraf keeps one small, local, per-user file of basic usage counts (launches,
runs started and finished, workers spawned, the providers and models used by
name, and named feature use) and every error, each under a stable `KG-nnnn`
glossary code (`docs/ERRORS.md`). It is capped at about 1 MB, with the oldest
entries trimmed once it grows past that. **Nothing in it is ever sent
anywhere**: there is no network code in the crate that writes it at all. If
you ever need to report a problem, run `kgraf diag show` to read it, or `kgraf
diag export` to write a shareable copy, and send that however you choose.

