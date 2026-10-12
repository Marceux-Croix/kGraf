# Decision 0002: Rust core, local only, all platforms

**Status:** Accepted. The original Tauri shell choice is superseded by a
later decision; the Rust core, local-only, and all-platform parts stand.
**Superseded by:** 0054 (in part: the shell choice)
**Related:** decision 0001

## Context

The interface requirement and the engine requirement pointed at the same
answer. The graph must render on the order of ten thousand nodes at
interactive framerate with live updates, which is a job for a browser
rendering stack. Everything else is a job for a systems language: parsing a
whole repository into a graph, keeping that index fresh on every edit, and
running many concurrent agents with real cancellation.

A pairing of a Rust process that owns the logic with a webview or native
render surface for the interface, packaged as a native application on all
three platforms, answers both requirements together. The choice of Rust and
the choice of desktop shape are therefore one decision and not two.

Local-only is a product position. The code a person most wants help with is
usually the code they can least justify uploading, and a local process can
read the whole tree, run the build, and idle all day at no marginal cost.

## Decision

kGraf is a desktop application shipping on macOS, Windows, and Linux with a
real application icon. The core is Rust.

It is not a website, it does not run in a user's browser, and there will be
no hosted version.

## Consequences

Packaging, signing, and notarization are real work, and must be solved on
all three platforms.

Sandboxing worker execution must be solved from Rust on three platforms,
which rules out depending on a Node-only sandbox library without shelling
out to it. See decision 0011.

No hosted mode means no server-side anything: no account system, no shared
state, no remote index. Any future proposal requiring a kGraf-operated
service supersedes this record rather than extending it.

## Alternatives considered

**Local server plus a browser tab.** Simplest to build and debug, and a
later hosted mode would be nearly free. Rejected on the product requirement:
the thing must be a program with an icon, where the work lives, not a tab
competing with forty other tabs. The "nearly free hosted mode" is not a
benefit here because a hosted mode is explicitly unwanted.

**Electron.** Mature and well understood. Rejected: it implies a Node core,
which is the wrong engine for repository-scale parsing and for many
concurrent agents, and it ships a browser with every install.

**TypeScript end to end.** One language and shared types across the wire.
Rejected: weak code-parsing ecosystem for the indexer relative to Rust's
tree-sitter bindings, and no real parallelism for the indexing pass.

**Rust core with a webview front end.** Chosen. Slower to write than the
alternatives, and the provider client layer is largely hand-rolled, which
decision 0001 already commits to regardless.
