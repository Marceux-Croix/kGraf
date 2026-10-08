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

## Amendment (2026-10-07): the worker shell runs on Windows, through Git Bash, with the network open

**Problem.** Workers could not run a shell command on Windows: the shell tool's
non-Unix path refused every call. Every policy on Windows was also refused
before it started, because the backend is always partial and no production
caller opted in to that, so the checks and gates of the 2026-10-06 amendment
could not run there either. And a POSIX shell command (`sh -c`) finds no `sh`
on a default Windows `PATH`.

**Decision.**

1. **Network.** The worker shell takes its network from the same
   `CheckNetwork` decision as checks: denied on macOS and Linux, open on the
   Windows restricted-token backend. Where it is open, the notice "network not
   blocked: the Windows sandbox cannot deny network access without
   administrator rights (decision 0011); shell commands run with the network
   open" is in the tool's description, in the first result of each tool, once
   in the run's trace, and in the list the run command prints.
2. **Partial isolation follows the same decision.**
   `CheckNetwork::accepts_partial_isolation` is true exactly where the network
   is open with the notice; the worker shell, the contract check runner, the
   gates and the benchmark-brief checks pass it to the policy. Elsewhere it is
   false, so a partial Linux backend still refuses.
3. **Git Bash for `sh` and `bash`, with no fallback.** A command whose program
   is `sh` or `bash` runs with Git Bash (`bin\bash.exe` of Git for Windows);
   every other argv runs as given. Claude Code makes the same choice for its
   Bash tool; Codex defaults to PowerShell. Git Bash is chosen because kGraf's
   permission checks read an `sh -c` script as POSIX shell text, and running it
   in PowerShell or cmd would run a different language than the one judged.
   With no Git Bash the command fails and says to install Git for Windows or
   set `KGRAF_GIT_BASH_PATH`. `bash` is never looked up on `PATH`, where
   `System32\bash.exe` is the WSL launcher.
4. **The argv goes through the environment.** The MSYS2 runtime splits a
   command line from a Windows parent with its own quoting and globbing rules.
   The command line therefore carries one fixed script with no quote and no
   backslash, `IFS=;set -f;eval $KGRAF_SHELL_COMMAND`, and the argv travels in
   `KGRAF_SHELL_COMMAND`, single-quoted, as an `exec` of `/usr/bin/sh` or
   `/usr/bin/bash`. This sits in the backend's spawn, so the gates' and the
   contract checks' `sh -c` use it too.
5. **`CARGO_NET_OFFLINE=true` stays on Windows.** It keeps a build to the
   dependencies the user fetched, as on every other platform, and the sandbox
   cannot write the user's cargo home there anyway.

**Behavior.** The same contract as on Unix: environment, working directory,
output caps, the timeout marker, cancel, git's safety settings and the
permission checks. The job object ends every process of the command at a
timeout, a cancel and a normal exit.

**Known gaps.** A `.cmd` or `.bat` program or an extensionless script runs only
through `["sh", "-c", ...]`, and the error says so. The `cmd /c` script reader
judges the argv's text, while cmd.exe parses the raw command line; the two can
differ for a script with embedded quotes.

**Status.** Built and compile-checked for Windows; the decision logic and the
argument transport are tested on macOS. Nothing ran on Windows. Test steps:
`docs/WINDOWS-TESTING.md`.

## Amendment (2026-10-07): a network limited to kGraf's egress proxy, and a hidden folder inside a writable root

Decision 0087, amended 2026-10-07, runs a project's dependency install in the sandbox with the
network limited to the package registries. Two policy options are new, each enforced per
backend or refused.

1. **`Network::Proxy { port }`: TCP to one loopback port only**, where kGraf's egress proxy
   (`kgraf_sandbox::egress`) listens and decides by host name.
   - **macOS**: Seatbelt allows `connect()` to `localhost:PORT` and no other IP destination
     (the rule form Anthropic's sandbox-runtime uses for its proxy). The DNS resolver socket is
     not allowed; the network policy's mach services stay, for TLS certificate checks.
   - **Linux**: the command gets a network namespace of its own. Its first process, between
     fork and exec and before Landlock and seccomp, brings the namespace's loopback up, listens
     on `127.0.0.1:3128` there, and passes the listening socket to kGraf over a socket pair
     (`SCM_RIGHTS`); kGraf accepts on it from outside
     (`SandboxedCommand::take_loopback_listener`, `EgressProxy::spawn`). seccomp allows IP
     sockets as with `Network::Allow`; the namespace has nothing else to reach. Needs user
     namespaces, else `PolicyNotEnforceable`. Landlock's TCP port rules were rejected: they
     limit ports, not hosts.
   - **Windows**: refused (`PolicyNotEnforceable`): nothing would keep a command on the proxy.
     kGraf's Windows install runs with the network open and says so (0087).
2. **`SandboxPolicy::hide(dir)`: a folder beneath a writable root that a command can neither
   read nor write**, apart from writable roots beneath it. Used for the checkout's `.kgraf`
   while an install runs with the checkout writable.
   - **macOS**: after the root allows, `deny file-read*` and `deny file-write*` of the folder,
     then the roots beneath it allowed again, and the metadata (never the listing) of the
     folders between it and them, which realpath(3) needs. Renaming or replacing the folder is
     a write in it and is denied.
   - **Linux**: in the command's mount namespace, each root beneath the folder is cloned
     (`open_tree`), an empty tmpfs is mounted on the folder, each clone is moved back onto its
     path, and the tmpfs is made read-only. A mount point cannot be renamed. Needs user
     namespaces, else `PolicyNotEnforceable`.
   - **Windows**: a deny-write access entry for the capability of each writable root that holds
     the folder, inherited by what is in it; a root beneath it keeps its own capability and its
     explicit grant, which Windows orders before the inherited deny. Reads are not confined on
     this backend. Unverified on Windows, like the rest of the backend.

Tests: `kgraf-sandbox` `tests/egress.rs` (macOS and Linux, real backends), `windows.rs`
`a_hidden_folder_denies_the_roots_above_it_only` (the Windows plan, on every platform).

## Amendment (2026-10-07): Schannel TLS fails inside the Windows sandbox; the fix is open

**Found.** On a Windows Server 2025 host, `curl.exe` from System32 reaches an
HTTPS site as the user and fails in the sandbox, with the network allowed:
`curl: (35) schannel: AcquireCredentialsHandle failed: SEC_E_NO_CREDENTIALS
(0x8009030E)`. Every program that uses the Windows TLS stack fails the same way
before it sends a byte: System32 `curl.exe`, PowerShell `Invoke-WebRequest`,
.NET `HttpClient` and `SslStream`, WinHTTP and WinINet clients, installers built
on them, git with `http.sslBackend=schannel`. Programs with their own TLS
library (OpenSSL in Git for Windows, Node, Python, rustls) are not affected.

**Cause: the restricting SIDs, not the environment.** The sandbox does not
substitute `USERPROFILE`, `APPDATA` or `LOCALAPPDATA`, and the token keeps the
user's SID, logon session and registry hive. Schannel's
`AcquireCredentialsHandle` is served inside LSA, which gives each security
package a `SECPKG_CLIENT_INFO` whose `Restricted` member says whether the caller
"is restricted in its ability to access securable objects" (Microsoft Learn,
`ntsecpkg.h`), the wording Microsoft uses for a restricted token. Microsoft does
not document that Schannel refuses such a caller, but it is measured
(openai/codex issue 17459): under impersonation of tokens made with
`CreateRestrictedToken`, the user's token, `DISABLE_MAX_PRIVILEGE` alone and
`LUA_TOKEN` alone get `SEC_E_OK`; one restricting SID (Everyone), with or
without `WRITE_RESTRICTED`, gets `0x8009030E`, and NTLM and Negotiate refuse
default credentials the same way. This backend's token always carries
restricting SIDs: they are its write boundary. No change to the environment,
the ACLs or the profile access fixes it.

**Tests (written for Windows).** `kgraf-sandbox` `src/sandbox/windows.rs`,
module `schannel`, asks Schannel for a client credential under each token: the
user's own token and a Low copy of it get `SEC_E_OK`; the sandbox token and a
token with one restricting SID get `SEC_E_NO_CREDENTIALS`. `tests/tls_windows.rs`
runs System32 `curl.exe -k` against a TCP listener on 127.0.0.1 and asserts a TLS
handshake record arrives: outside the sandbox (the control) and inside it (fails
until a fix lands).

**Options, not yet chosen (each is a security trade-off).**

1. Keep the restricted token; steer git and multi-backend curl to OpenSSL; detect
   `SEC_E_NO_CREDENTIALS` in output and name the cause. Strength unchanged;
   System32 curl, PowerShell, .NET and WinHTTP still fail; git trusts Git for
   Windows' CA bundle instead of the Windows store and loses Schannel's
   revocation check.
2. Drop the restricting SIDs and make the Low integrity level the write
   boundary, with Medium labels for the gitlink and hidden folders. Schannel
   works if the measured premise holds; writes are then confined to Low-labeled
   locations, not per root, so other workspaces' roots and `LocalLow` become
   writable and cross-sandbox process access needs other protection.
3. A one-time elevated setup with a dedicated sandbox user, a loaded profile and
   no restricting SIDs. Needs administrator rights once, against this decision's
   constraint; also makes network denial possible.
4. An AppContainer tier for network-open commands. AppContainer processes use
   Schannel, but the toolchains this decision found broken there are the ones
   that need the network.

A TLS proxy outside the sandbox does not help: a Schannel client needs its own
credential before it sends a ClientHello.

**Status.** The cause is documented in the backend and its strength gaps. No
option is built. Nothing here ran on Windows.

**Note (2026-10-07): option 1 is built.** With the network open, every sandboxed command on
Windows gets `CURL_SSL_BACKEND=openssl`, and `http.sslBackend=openssl` is appended to the
`GIT_CONFIG_COUNT` entries after the host's own and the `credential.helper` reset
(`git_config_entries` in `windows.rs`). `GIT_CONFIG_NOSYSTEM` and `GIT_CONFIG_GLOBAL` are not
touched, so the frozen git configuration keeps its effect. The values are set before a caller's
own environment, so a caller can still override them; the host environment is not read. A
shell result on Windows whose output holds `SEC_E_NO_CREDENTIALS` or `0x8009030E` gets one
line that names this decision. `tests/tls_windows.rs` now asserts the documented state: the
System32 `curl.exe` control passes outside the sandbox; the curl of Git for Windows
(`mingw64\bin\curl.exe`) reaches the listener inside it; System32 `curl.exe` still fails
with `SEC_E_NO_CREDENTIALS` inside it. Still fails inside the sandbox: any tool whose only TLS
stack is Schannel (System32 `curl.exe`, PowerShell, .NET, WinHTTP installers). Options 2 to 4
are not built. Compile-checked for Windows; nothing here ran on Windows.

## Amendment (2026-10-07): Desktop and environment; MSYS2 programs cannot start under the write-restricted token

**Found.** On Windows 11 and Windows Server 2025, every `sh` and `bash` command in the
sandbox exited with 0xC0000142 (STATUS_DLL_INIT_FAILED) and printed nothing, while
`cmd /d /c echo` ran. On Windows the benchmark-brief check's environment was also not
cleared.

**Cause, two layers.**

1. **The private desktop admitted the user SID only.** The `bin\bash.exe` launcher of Git for
   Windows links `shell32`, which loads `user32.dll`; `user32.dll` connects to the desktop
   while it loads, with rights from the desktop's generic write mapping. A `WRITE_RESTRICTED`
   token must also pass the check with its restricting SIDs for a write right, and the user
   SID is not one, so every program that loads `user32.dll` at start failed with 0xC0000142.
   `cmd.exe` does not load it. openai/codex issue 46412 measures the same split; codex grants
   its desktop to the logon SID, one of its restricting SIDs, which this backend leaves out.
2. **The MSYS2 runtime's signal pipe.** `msys-2.0.dll` creates its signal pipe while it loads,
   with an ACL that grants only the user SID, Administrators and SYSTEM, and opens it for
   `GENERIC_WRITE` (msys2-runtime `sigproc.cc`, `sec/helper.cc`, `fhandler/pipe.cc`). No
   restricting SID is in that ACL, so the open is refused and the runtime stops with
   "couldn't create signal pipe, Win32 error 5" (also reported for codex's unelevated
   sandbox, openai/codex issues 7031 and 15016).

**Built.**

1. Each private desktop has its own fresh SID, granted on that desktop only, and every command
   on it carries the SID as a restricting SID. No write is added anywhere else. Whether
   `user32.dll` also needs a write right on the window station is not yet measured.
2. `SandboxedCommand::env_clear` works on every platform. On Windows a cleared command still
   inherits `WINDOWS_BASE_VARS` (`SystemRoot`, `PATH`, `PATHEXT`, `ComSpec`, the profile and
   program folders, the processor variables, `USERNAME`), and the sandbox sets its `TMP`,
   `TEMP`, git `credential.helper` reset and TLS settings again, numbered from
   `GIT_CONFIG_KEY_0`. The profile variables keep their real values. No base variable is
   credential-shaped.

**MSYS2 layer: options, not yet chosen.** Every fix inside the token makes the user SID (or a
group granted wherever the user is) a restricting SID, which removes the per-root write
confinement and leaves the Low integrity level as the only write boundary.

1. The user SID as a restricting SID: other workspaces' Low-labeled roots, `LocalLow` and
   other sandboxes' processes become writable.
2. A Low-only token for shell commands: the same costs; also fixes Schannel; MSYS2 may still
   fail on shared objects a Medium process created.
3. A dedicated sandbox user set up once with administrator rights: keeps confinement and fixes
   Schannel; needs administrator rights once.
4. A POSIX shell without the Cygwin runtime (busybox-w32): keeps confinement; the MSYS2 tools
   of Git for Windows, and what git runs through its own `sh`, still fail.
5. Keep it: `sh` and `bash` do not run on Windows; the failure shows the runtime's message.

**Tests.** Unit tests for the environment filter and the kept settings run on every platform.
Windows-only tests pin the pipe refusal and the desktop admission by impersonation, and run
`whoami.exe`, `where.exe`, PowerShell, `cmd`, `sh` and `bash` in the sandbox, with and
without a cleared environment. Compile-checked for Windows; nothing here ran on Windows.
