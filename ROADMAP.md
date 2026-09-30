# kGraf Roadmap

kGraf is pre-release software. This page lists what works today, what is in progress, and
what is planned next, as things you can do or expect, not as an internal project schedule.

## Works today

- Point kGraf at a repository and index it into a code graph: `kgraf index` covers Rust,
  Python, TypeScript, TSX, JavaScript, Go, Java, C, C++, Ruby, C#, PHP and Markdown.
- Query the graph from the command line: look up a symbol, pull a context slice sized to a
  token budget, or run a structured query over calls and definitions.
- Give the orchestrator an instruction, or let it read your project's own documentation
  first, and it splits the work into contracts and dispatches worker agents across the
  providers you configure. Anthropic, OpenAI and Moonshot's Kimi are verified live, and so
  is a local Ollama server. The route catalog also covers Google (Gemini native and Vertex
  AI), AWS Bedrock, Azure OpenAI, DeepSeek, xAI, z.ai, Qwen, Llama-based hosts and local
  models such as llama.cpp. Those routes are built and tested against recorded replies only;
  `docs/MODEL-SUPPORT.md` says which routes have run against the real service.
- A route can serve a kind of model other than a chat model: embeddings, reranking and typed
  decisions (for example a local System-One server). The orchestrator decides when to use one;
  nothing calls one on its own (decision 0089).
- Workers run in parallel, each in its own git worktree and its own sandbox: full
  confinement on macOS and Linux, a lighter sandbox on Windows.
- Every worker reads the code graph in bounded slices, not whole files, and writes only to
  the files and code elements it declared in advance, so concurrent writers never collide
  on the same file.
- Verified work merges automatically, one worker at a time, onto its own run branch. Your
  checked-out branch never moves.
- Quality gates run your project's own build and test commands against a pinned commit
  before anything merges.
- Workers build and test their own work: a short allowlist of build and test commands
  (`cargo build`, `check`, `test`, `clippy`, `fmt --check`) runs in the sandbox with the
  network off (decision 0084).
- Dependencies a project needs are fetched from crates.io on your machine, outside the
  sandbox, only after you allow it once per project (`kgraf deps allow`); a run that needs
  a fetch you have not allowed stops before it starts (decision 0087).
- Worker worktrees and their builds are deleted after use by default. A setting (and
  `--keep-worktrees`) keeps them for inspection (decision 0088).
- Every run refreshes the code graph before it starts, so workers read graph slices on a
  fresh checkout.
- kGraf sets no spend limit; it records every token so you can set one in your provider's
  console (decision 0080).
- A model with no price entry still runs: its spend is reported as unpriced, never as zero,
  and `kgraf orchestrate` exits non-zero unless the run completed.
- An opt-in update check (off by default) offers a newer release in the notification bell and
  verifies the download against the release's checksum file.
- Every model call is recorded in a tamper-evident ledger. `kgraf spend` totals it and
  `kgraf ledger verify` checks the chain.
- kGraf reads roadmap and to-do files already in your repository and shows them as work
  items on the graph before you approve anything, and reads the instruction and skill
  files your other coding agents already use (for example `CLAUDE.md`, `AGENTS.md`,
  `GEMINI.md` and `SKILL.md` files).
- Connect to MCP servers, local or hosted; their tools become available to workers
  automatically.
- Run kGraf from the command line or from a native desktop app with a live, GPU-rendered
  map of the project, with a software-rendering fallback when there is no GPU. Large
  graphs fold into readable clusters that expand on a click.
- Open several projects in one window, each keeping its own runs.
- Open any individual agent's session directly: send it a note or a redirect mid-task,
  answer a question it raises, or stop and resume it, without going through the
  orchestrator.
- Let an agent spawn subagents of its own, visible on the graph as children of the agent
  that started them.
- Set a cron or one-shot schedule on a work item; the desktop app's tray fires it in the
  background.
- Choose a color-vision mode, including three types of color blindness, for the graph's
  colors.
- A local, capped, never-transmitted diagnostics file records usage counts and errors for
  your own troubleshooting.
- Transcripts are scrubbed for secrets before they are stored or shown, on top of
  exact-match scrubbing of your own configured credentials.

## In progress

- Routing a question that does not fit the current plan: assigning it to an existing
  roadmap item, spinning up a new project from it, or parking it on a dedicated node, at
  the orchestrator's judgment.
- Starting a run directly from the desktop window against a live provider, and firing a
  scheduled run from the tray, end to end.
- Optional cross-checks between two workers before they start: each side gets a written
  interface contract, a stub-compile and vacuity check, and a live handshake between the
  two parties, with either worker able to challenge a contract it disagrees with.
- Automatic conflict handling: when two workers' merges collide, a dedicated resolver
  worker is dispatched to reconcile them before falling back to a plain re-dispatch.
- A live call on each route that is built but not yet run against the real service, starting
  with the cloud routes (Gemini, Vertex AI, Bedrock and Azure OpenAI).
- Full sandbox confinement on Windows. It is compiled today but not yet exercised on
  Windows; macOS and Linux confinement is in daily use.
- Signed, notarized installers for macOS, Linux and Windows. Local packaging scripts
  already build the app and the command line for all three platforms; the packages are
  unsigned today, so expect a security warning on first launch.
- A detached signature for the update check, so a download is checked against a key and not
  only against a checksum file published with it.

## Planned

- Expanding a work item on the graph into the individual contracts it is made of, and
  showing an agent's call trace on the nodes it touched.
- Trying a merge before a worker even finishes, instead of waiting for it to declare done,
  and preferring to schedule similar tasks together as a secondary signal alongside the
  collision-avoidance leases already in place.
- Reading constraints out of your project's documentation, not just tasks and commands,
  and enforcing them on the orchestrator's own behavior.
- A settings screen for provider routes that shows, per configured model, what kind of
  reasoning trace it can actually return.
- A casual messaging channel, for example iMessage or WhatsApp, to check on a run or drop
  in a quick note from your phone. This is a later addition, not part of the initial
  release.

---

kGraf makes no claim to reduce token spend. Running a team of agents costs more than
running one, by design. The case for kGraf is control, visibility, and the freedom to route
each part of a job to the model that suits it, with every token accounted for on the ledger.
