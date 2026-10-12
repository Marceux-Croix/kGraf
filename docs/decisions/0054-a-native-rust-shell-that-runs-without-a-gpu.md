# Decision 0054: A native Rust shell that runs without a GPU

**Status:** Accepted. Supersedes the shell choice and the renderer choice made earlier in the
project.
**Supersedes:** 0002 (the shell choice), 0010 (the renderer choice)
**Amended by:** 0055 (the galaxy renderer, in its addendum)
**Related:** decision 0052

This record exists because the desktop shell's constraints were restated more precisely
partway through the project: the lightest possible program, with a dedicated taskbar icon,
running entirely locally, and with no guarantee that a GPU is present.

## Decision

**The desktop shell is a single native Rust binary.** Its parts:

- **Windowing and icon.** `winit` provides the window and the taskbar icon on every platform.
  The `tray-icon` crate provides the tray presence that the background-run mode from decision
  0052 needs.
- **Panels.** `egui` draws the orchestrator control panel, the transcript surface, search, and
  settings.
- **Galaxy.** A purpose-built instanced renderer draws the code galaxy through `wgpu`. It keeps
  the existing server-side, cached, anchored layout and its density rules; only the rendering
  technology changes.
- **One language.** There is no embedded web view, no TypeScript, and no npm build step.

**It must run without a GPU, and must know when it is doing so.**

- **Adapter selection.** The shell asks `wgpu` for a hardware graphics adapter first. If none
  exists, it takes the software fallback adapter available on the platform.
- **The adapter is named.** Its type, including whether it is a CPU-based software device, is
  read from the adapter and shown in the interface. Rendering on a CPU is a stated mode, never
  a silent slow path.
- **A CPU rendering budget.** In CPU mode the galaxy switches to a budget sized for a software
  rasterizer: point sprites only, and fewer of them; no bloom effect and no per-frame animation
  when nothing changed; edges drawn only on selection; a lower cap on visible nodes before
  aggregation.
- **Redraw on change only.** The scene redraws only when something changed, so an idle window
  costs nothing on either rendering path.

## Why not an embedded-web-view shell

An earlier comparison of shell technologies (a local server with a browser tab, an embedded
web view framework, and a fully web-based interface) never evaluated a native Rust interface
directly. Against the no-GPU and lightweight-process constraints:

1. **No GPU, the decisive factor.**
   - **A web view's GPU failure can be undetectable from inside the app.** Where a web view
     has no hardware graphics path available, its web-based 3D rendering can still succeed by
     falling back to software, and on at least one common Linux web view engine, the renderer
     identification string is masked, so the app running inside it cannot tell it is on the
     software path.
   - **The documented Linux workaround is itself slow.** A commonly documented environment
     variable fix for that engine's GPU failures on Linux disables accelerated compositing
     entirely and copies every rendered frame back through the CPU. One measurement found this
     raised interface-process CPU usage from about 14 percent to about 53 percent.
   - **The heaviest view in the product is exactly the one that would suffer**, and it would
     degrade silently, which the product could not even detect in order to report it.
2. **Weight.** A system web view is not actually light once a heavy interface page runs
   inside it. A tracked issue in a popular embedded-web-view framework recorded that, once
   shared memory is counted properly, such an app running real web content used about as much
   memory as an equivalent fully bundled browser-based app, and sometimes more; the framework's
   durable advantage over a bundled browser is install size, not memory use.
3. **One toolchain.** A single-language shell removes any risk of drift between a Rust backend
   and a separate TypeScript frontend's event contract, along with the separate build
   validation that a second toolchain would need.

## What this gives up

These are real costs.

- **Text-heavy surfaces are weaker in `egui` than on the web.** Rich text selection, long
  markdown transcripts, and code views all need additional supporting crates, and the result
  will be plainer than an equivalent web page.
- **The web 3D-graphics ecosystem is gone.** The galaxy renderer is written by hand. The
  existing layout rules (baked positions, instancing, a point-sprite cutover at a measured node
  count, and density rules) are renderer-agnostic and carry over; a web-specific rendering
  bundle does not.
- **The `wgpu` backend can start slowly.** Its own maintainers report startup times between 1
  and 7 seconds on some systems. An older graphics-API backend is faster to start and also runs
  on the same software fallback path, so it is available as a fallback if startup proves too
  slow.
- **`egui` has no built-in tray support.** The `tray-icon` crate supplies it, requiring the same
  integration work an embedded-web-view framework would otherwise do internally.

## Acceptance

The native shell was accepted and built. Merge commits landed the shell (window, panels, tray,
GPU-optional adapter selection with the adapter type shown in the interface), the `wgpu`
galaxy renderer with its CPU rendering budget, and the macOS, Linux, and Windows installers,
including confirmed detection of a common Linux software rasterizer on real hardware.

A planned side-by-side measurement against an embedded-web-view shell (memory as proportional
set size across all processes, install size, cold start time, and frame time on both a
GPU-equipped and a software-only machine) was not run. What stands in for part of that
criterion is the shipped requirement that the same build runs on a machine with no GPU and
reports that it is rendering in software: this is built and demonstrated on Linux. It remains
unverified on macOS and Windows, and the cold-start, idle-CPU, and software-frame-time budgets
against a target are still owed. The memory and frame-time comparison against an
embedded-web-view shell is not replaced by anything; nothing currently measures it.

## Consequences

The native shell and its tray presence are built. The renderer is `wgpu` with a CPU budget,
and acceptance for that work includes a software-rendering run.

The product ships one binary per platform with no embedded-web-view dependency: an operating
system's own web view component is no longer an install prerequisite.

**Reactivation trigger.** A surface the product needs that `egui` cannot render acceptably.
Rich document editing is the likeliest candidate. Such a surface could be hosted in a web view
embedded for that surface alone, while the galaxy view would stay native.
