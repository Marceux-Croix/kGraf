# kGraf Roadmap

kGraf is pre-release software. This page lists what works today, what is in progress, and
what is planned.

## Works today

[docs/USAGE.md](docs/USAGE.md) covers each item.

- Indexing, graph queries and context slices, from the command line and in the desktop app.
- Orchestrated runs across providers: contracts, parallel workers in their own worktrees and
  sandboxes, verification, quality gates and merges onto a run branch of their own.
  Anthropic, OpenAI, Moonshot's Kimi and a local Ollama server are verified live. The other
  routes are built and tested against recorded replies only.
  [docs/MODEL-SUPPORT.md](docs/MODEL-SUPPORT.md) lists each route.
- Routes for embedding, rerank and typed-decision models. The orchestrator decides when to
  use one (decision 0089).
- The hash-chained token ledger. kGraf sets no spend limit (decision 0080); set one in your
  provider's console.
- Dependency fetches only after you allow them (`kgraf deps allow`, decision 0087).
- Roadmap and to-do files shown as work items; MCP servers; the instruction files of other
  coding agents; secret scrubbing of transcripts.
- Schedules on work items. The desktop app's background mode runs them while the app is in
  the tray. A test drove this against a live provider. Nobody has yet watched a run fire
  from the real tray icon.
- An opt-in update check, off by default.
- Sandboxed worker commands with the network off on macOS and Linux. Reads outside the
  worktree are denied by default on Linux. On macOS only credential and tool-config paths
  are denied. On Windows, setup, removal and commands run as a dedicated low-privilege user
  (decision 0097), and loopback connections are filtered. See
  [Platform notes](docs/USAGE.md#platform-notes).

## In progress

- Routing a question that does not fit the current plan: assigning it to an existing
  roadmap item, spinning up a new project from it, or parking it on a dedicated node, at
  the orchestrator's judgment.
- Starting a run directly from the desktop window against a live provider, and a hand
  check of a scheduled run fired from the real tray icon.
- Optional cross-checks between two workers before they start: each side gets a written
  interface contract, a stub-compile and vacuity check, and a live handshake between the
  two parties, with either worker able to challenge a contract it disagrees with.
- Automatic conflict handling: when two workers' merges collide, a dedicated resolver
  worker is dispatched to reconcile them before falling back to a plain re-dispatch.
- A live call on each route that is built but not yet run against the real service, starting
  with the cloud routes (Gemini, Vertex AI, Bedrock and Azure OpenAI).
- Full sandbox confinement on Windows. Name lookups (DNS) from the sandbox user still leave
  the machine on Windows Server 2025, because Windows has no working filter for them there.
- Signed installers for macOS, Linux and Windows, notarized on macOS. The packaging
  scripts build all three. The packages are unsigned today (see
  [Platform notes](docs/USAGE.md#platform-notes)).
- A detached signature for the update check, so a download is checked against a key and not
  only against a checksum file published with it.

## Planned

- Expanding a work item on the graph into the individual contracts it is made of, and
  showing an agent's call trace on the nodes it touched.
- Trying a merge before a worker even finishes, instead of waiting for it to declare done,
  and preferring to schedule similar tasks together as a secondary signal alongside
  the advance write declarations that keep workers off the same files.
- Reading constraints out of your project's documentation, not just tasks and commands,
  and enforcing them on the orchestrator's own behavior.
- A settings screen for provider routes that shows, per configured model, what kind of
  reasoning trace it can actually return.
- A casual messaging channel, for example iMessage or WhatsApp, to check on a run or drop
  in a quick note from your phone.
