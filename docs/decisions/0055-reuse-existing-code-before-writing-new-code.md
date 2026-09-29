# Decision 0055: Reuse existing code before writing new code

**Status:** Accepted. Amended by the addendum below and by decision 0061.
**Related:** decisions 0051, 0054

This record exists so the build method is not re-argued. If a future contributor asks why a
given piece of code was taken from another project instead of written from scratch, the
reasoning is here.

## Decision

kGraf is assembled from existing, permissively licensed code wherever that code covers the
work needed. Novel code is written only when a search finds nothing usable, and the reason is
recorded. A separate operating standard covers the detail: search order, qualifying licenses,
provenance comments, and a `THIRD_PARTY_NOTICES.md` file.

Each area of planned work carries a list of reuse candidates: blocks of existing code to
evaluate first. A candidate is a lead, not a commitment: its license is verified at the source
on the day it is taken, and the resulting code must pass the same acceptance criteria as code
written from scratch.

## Context

Whenever possible, code that already accomplishes a task is pulled in from existing
permissively licensed projects on GitHub, rather than written from scratch, because searching
is fast and writing from nothing is not.

Early foundational work was written largely from scratch. The remaining major areas (the
agent loop, orchestration, scheduling, a native shell, a graph renderer, document parsing,
sandboxing, and packaging) are areas where mature open-source code already exists.

## License positions

- **LibreChat:** MIT license.
- **big-AGI:** MIT license.
- **Open WebUI:** BSD-3-Clause with additional proprietary terms layered on top. Non-permissive
  as a result. Not a reuse candidate; no code is copied or ported from it.

## Alternatives considered

- **Write each area from the design, with no reuse.** Gives full control and a small
  dependency tree, but spends the most time and repeats already-solved problems.
- **Adopt one framework whole**, for example a Rust agent framework covering orchestration and
  the agent loop broadly. Fast at first, but a framework that owns the provider call directly
  conflicts with the requirement that every token be attributed through the project's own
  accounting path, and with adapters that must be verified against a real running server
  before they are trusted. Taken per block instead, wherever it does not create that conflict.
- **Reuse per block, with licenses verified at the source.** Chosen. Take the smallest unit
  that covers the work: a crate, a module, or a function ported to Rust.

## Consequences

Every area of work starts with a reuse search. A `THIRD_PARTY_NOTICES.md` file exists at the
repository root from the first reused block onward.

Reused code does not relax any of the project's own standards. A reused block that bypasses
the accounting path, degrades silently instead of failing loudly, or has not been verified
against a real running counterpart is rejected the same way written code would be.

**Performance on the harness path.** kGraf must be highly performant. Reused harness-path code
is reviewed and benchmarked before it merges, and each area of harness work sets measured
latency budgets and fails acceptance when one is missed. Rust-native code already in
production use elsewhere is preferred over a fresh port from another language.

**The UI is fully custom.** The interface will be reshaped heavily over time, so UI code taken
from other projects is copied into kGraf's own tree and owned there, never consumed as an
external dependency the project cannot freely edit. Underlying platform crates (`winit`,
`wgpu`, `egui`) stay as ordinary dependencies.

**Amends the galaxy renderer.** Decision 0054 called for a purpose-built instanced renderer on
`wgpu`. The galaxy renderer instead starts from a copy of a dual-licensed (MIT or Apache-2.0)
open-source renderer built for a similar visualization use, and is reshaped from there.
Decision 0054's GPU-optional requirement and its CPU-rendering report are unchanged.

**Vendoring convention.** Code from repositories not published as installable packages is
copied under a `third_party/<repo>/` directory with an `UPSTREAM` file naming the repository,
commit, paths, and license, trimmed to what kGraf actually calls. Whole-workspace external
dependencies pointed at a git repository are not used, since they pull an entire dependency
tree into the build. A license-scanning tool generates `THIRD_PARTY_NOTICES.md` for ordinary
package dependencies; the `UPSTREAM` files supply the entries for vendored code.

Reactivated if a reused dependency becomes unmaintained or changes its license: the affected
block is then pinned, vendored with its notice, or replaced.

## Addendum: the release pipeline for the Windows installer changed

- **What this decision originally planned.** For final packaging, a cross-platform release
  tool was named as the release pipeline, with a per-OS installer packager as a fallback for
  producing installers on macOS, Windows, and Linux.
- **What changed.** The Windows `.msi` installer is instead built with a different,
  Linux/macOS-native tool invoked as a subprocess, recorded in decision 0061. The originally
  planned tools are not used for the MSI specifically.
- **Why.** Neither originally planned tool could produce a Windows installer from a
  non-Windows build machine without a Windows-based build environment, which was not
  available. The chosen tool runs natively on macOS and Linux and produces a verified, genuine
  MSI file.
- **Scope of the amendment.** It covers the Windows MSI step only; the originally planned tool
  stays the plan for the rest of the release pipeline. Decision 0061 states when to revisit the
  MSI step.
- **The chosen MSI build tool is a build tool, not reused code.** Its license is not on the
  list of licenses accepted for code that ships inside the product, but it does not need to
  be: it runs as an external subprocess at packaging time, the same way other OS-native
  packaging tools do, and nothing from it is linked into or shipped inside the kGraf binary. It
  is therefore not a reuse candidate and is not listed in `THIRD_PARTY_NOTICES.md`.
