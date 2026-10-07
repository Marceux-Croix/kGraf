# Decision 0011: Per-platform sandboxing, honestly labeled, with no admin requirement

**Status:** Accepted
**Related:** decisions 0002, 0009

## Context

Windows is the binding constraint. A real sandbox is not feasible there for
v1 under a no-admin, no-installer, bundled-application constraint.

This is not a shortfall specific to this project. Other agent tools split
the same way: an elevated Windows backend that creates a dedicated sandbox
user with firewall and policy changes, alongside an unelevated backend using
a restricted token with access-control-list (ACL) boundaries and a private
desktop, described by its own authors as weaker isolation but still
functional. Some agent sandbox runtimes self-elevate once per machine.
Other coding tools punt to a Linux virtual machine layer on Windows.

### Why not the obvious candidates

A GPL-3.0-or-later sandboxing library is incompatible with an
Apache-2.0-licensed codebase, was archived as a repository, and never
supported Windows.

A Node-based sandbox runtime that self-elevates on Windows is wrong on both
counts for a Rust core in a bundled application.

Windows Sandbox is unavailable on Home edition, requires an elevated
feature install and reboot, allows only one instance at a time, which is
fatal for parallel workers, and persists nothing.

WSL2 requires an elevated install and is not isolating by default: drives
auto-mount with the user's own credentials and Windows interop is on, so it
is a virtual machine with full access to the host's files until both are
disabled, at which point it cannot reach the repository.

Some lightweight virtualization sandboxes have no macOS support and run
freestanding guest functions, not processes; others are Linux only.

WASI is not viable as the execution layer. Spawning a process always
returns an error and no WASI proposal defines process spawning at all.
Effectively every real build invocation shells out, so that alone settles
it. There is also no production WASI build of the compilers and runtimes
real test suites need.

### Why the restricted token rather than integrity level

Integrity control blocks write-up only; reads are allowed. A
low-integrity worker still reads every file the user can: SSH keys, browser
profiles, environment files, other repositories.

Restricted tokens block reads, because the system performs two access
checks and grants access only if both pass. That asymmetry is the whole
reason it is the right primitive. No admin is needed when a process
restricts its own token.

A private desktop is mandatory, not optional. Platform documentation
explicitly warns that restricted applications must run on a non-default
desktop to prevent window-message attacks on unrestricted applications.

Job objects contribute resource caps and teardown, not isolation. They
cannot restrict filesystem or network at all, confirmed against the full
set of limit classes. Kill-on-close is the guarantee that no worker outlives
its run.

### Why AppContainer is opt-in and off by default

It is the only non-admin way to deny network, but its own vendor's
documentation of a related sandboxing project records the breakage: the
shell and Node fail at startup on a drive-root query; per-user toolchain
installs are unexecutable; MSBuild silently fails on named-pipe naming; git
refuses the repository with a dubious-ownership error; and several fixes
require elevation anyway.

No evidence was found anywhere of a full MSVC toolchain running
successfully inside an AppContainer. Chromium uses it for renderers, which
need nothing but pre-duplicated handles. A compiler toolchain is the
opposite workload.

## Decision

kGraf sandboxes worker execution using each platform's native primitives,
implemented from Rust, requiring no administrator rights and no separate
installer.

Isolation strength differs materially by platform, and the interface shows
a per-platform strength indicator rather than one shared badge.

On Windows, the v1 stack is a restricted token with restricting security
identifiers (SIDs), targeted ACL grants, a job object, and a private
desktop. AppContainer is offered as an opt-in stricter tier, off by default.

kGraf does not claim to block network egress on Windows, because no
non-admin mechanism there can.

## Consequences

**What the Windows v1 stack stops:** writing outside the worktree and
granted cache directories; reading the user profile, keys, browser data,
and other repositories; privilege escalation through token privileges;
runaway resource use and orphaned process trees; and window-message
attacks.

**What it does not stop, and the warning text must say so:**

- **Network egress, entirely.** A worker can send the repository anywhere.
- **Reading anything with world-readable ACLs**, including machine-wide
  configuration.
- **Kernel exploits.** There is no virtual machine boundary.
- **A malicious build script**, because kGraf runs the user's own build and
  test commands by design. This is containment around a process the user
  deliberately handed their code to.
- **Anything on a FAT-family volume**, where security descriptors are
  effectively null.

**Linux gets Landlock plus seccomp and namespaces; macOS gets Seatbelt;
Windows gets materially less.** Do not paper over that with one shared
badge.

**Pin the Windows bindings crate to its current major version.** Its
development branch has restructured away the relevant feature gates
entirely, so a migration is coming.

**Three things must be tested on real Windows before the design is relied
on**, because the initial research was done from documentation and the
reasoning is inferred from documented behavior rather than measured on the
platform itself:

1. Whether the chosen restricting SIDs let a cargo build and MSVC link
   succeed while denying the user profile.
2. Whether cargo and rustc survive an AppContainer at all. No data exists
   in either direction.
3. Whether the loopback exemption requires elevation.

**DACL grants must be refcounted and crash-recoverable.** A crashed run
that leaves a directory permanently granted to the sandbox principal
silently widens the boundary for every later run.

**Reactivation trigger:** a non-admin, general-availability Windows
process-security-environment mechanism that gives filesystem and network
containment with no host ACL edits. This is the first mechanism that would
close the gap described above. It is currently limited and its vendor
states it should not be treated as a security boundary yet. Track for v2.

## Alternatives considered

**One cross-platform sandbox crate.** Rejected: none was confirmed
production-ready against primary sources for all three platforms without
admin rights. Two architecturally promising candidates exist and are
unverified.

**Require admin once, and get a real boundary.** Rejected for v1 as
against the product shape, and because the strongest reference
implementations that do this still document escaping certificate checks,
unreachable per-user tools, and unfenced DNS. It is a reasonable v2 option
offered as a choice, not a default.

**Ship no sandbox and warn.** Rejected. Filesystem containment is
achievable without admin on all three platforms and is worth having on its
own.

**Per-platform native primitives, honestly labeled.** Chosen.

## Amendment (2026-10-06): on Windows, checks and gates run with the network open, and say so

**Problem.** The Windows backend cannot deny the network, so it refuses
`Network::Deny`. The verifier's check, the quality gates and the contract
checks all ask for it, so on Windows no check or gate ran at all.

**Decision.** For those three only, `kgraf_sandbox::CheckNetwork` decides from
the backend's own capability: on the Windows restricted-token backend, which
cannot deny the network, a check runs with the network open and a notice; every
other backend keeps `Network::Deny`, so macOS and Linux do not change. The notice
reads "network not blocked: the Windows sandbox cannot deny network access
without administrator rights (decision 0011); this check ran with the network
open". It is in the output of every such check and gate (a pass included), in
the contract check's result, once in the run's trace, in the run report's
notices, and in the list the run command prints when a run opens.

**Not changed.** The worker's shell is not part of this. On Windows it is
already refused whole, and it asks for `Network::Deny`.

**Trade-off.** A check runs the project's own code with the network open, so
that code can send what it can read to any host. On Windows reads are already
not confined, so the open network makes a local read a possible exfiltration,
and the notice says so. A check that passes there proves less than one that
passes where the network is denied.

**Status.** Built and compile-checked for Windows; the decision logic is tested
on macOS with a stand-in capability. Nothing ran on Windows.
