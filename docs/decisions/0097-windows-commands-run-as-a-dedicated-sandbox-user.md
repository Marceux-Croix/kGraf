# Decision 0097: On Windows, sandboxed commands run as a dedicated local sandbox user, set up once with administrator rights

**Status:** Accepted (2026-10-08). Setup, the runner, the launch path and the ACL confinement are built and compile-checked for Windows; none has run on Windows yet.
**Deciders:** the project maintainer.
**Related:** 0011 (sandboxing per platform; its Windows token design is superseded here once the later units land), 0084 (the build and test allowlist and read grants), 0087 (dependency installs and the egress proxy), 0083 (host writes never follow a link).

This record exists so the Windows sandbox model is not re-argued. If a future
contributor asks "why does kGraf create Windows user accounts," the reasoning is here.

---

## Decision

1. **A dedicated sandbox user runs every sandboxed command on Windows**, with that
   user's own token. The token carries **no restricting SIDs**. Writes are confined
   by access-control entries that grant the sandbox user write access only where a
   command may write; reads are confined by the same means, so the sandbox user
   cannot read the user's profile by default.
2. **Two users per user, offline and online** (as in openai/codex's elevated
   Windows sandbox). The offline user runs ordinary commands; the online user runs
   commands that need the network, such as dependency installs (0087).
3. **One-time setup with administrator rights:** `kgraf sandbox setup` asks for
   administrator rights once through the UAC prompt and creates the users, a local
   group holding them, denied logon rights (remote interactive, network, batch,
   service), sign-in screen hiding, and the firewall rules. `kgraf sandbox remove`
   undoes exactly what the undo record lists; `kgraf sandbox status` reports what
   exists without administrator rights. Setup is idempotent: running it again
   resets the passwords and refreshes the rules.
4. **Names come from the installing user's SID** (`kgraf-off-<hash>`, `kgraf-on-<hash>`,
   group `kgraf-sandbox-<hash>`), each user carries a comment naming the user's
   SID, and setup refuses an account of that name it did not create.
5. **Passwords:** 40 random characters from the system random source, wiped from
   memory after use. The elevated step may run as another administrator account,
   so it does not seal the passwords itself: it sends them over a pipe the user's
   own process created, and the user's process seals them with DPAPI for the
   current user (`CryptProtectData` without the machine flag) in kGraf's per-user
   configuration folder.
6. **The setup pipe:** the user's unelevated process creates it (first instance
   only, no remote clients, a random name, access for SYSTEM, Administrators and
   the user) before it starts the elevated step, so no other program can create a
   pipe of that name first. The user's process checks that the client is the
   elevated child it started; the elevated step checks that the server is the
   process that started it.
7. **Firewall:** four block rules scoped to the offline user's SID through the
   rule's local user list: outbound and inbound to everything but loopback, loopback
   TCP except kGraf's egress proxy ports (`47100` to `47131` on Windows), and
   loopback UDP. Block rules win over allow rules in Windows Firewall, so the offline
   user reaches nothing but kGraf's proxy. Setup refuses when the firewall is off or
   a group policy makes local rules ineffective.
8. **A runner program (later unit)** starts first as the sandbox user and makes the
   caller's logon SID deny-only before it starts the real command:
   `CreateProcessWithLogonW`, the only call a standard user can use to start a
   process as another user, gives the child the caller's logon SID, which would admit
   the sandbox to objects in the user's session. The runner also removes the call's
   1,024-character command line limit.
9. **The undo record** lives under ProgramData (administrator writes), one file per
   user, rewritten after every step, so a setup cut short can still be undone.
   Removal touches only names derived for this user, so an edited record cannot
   remove anything else.


## Context

Under the token sandbox of 0011 (a WRITE_RESTRICTED token whose restricting SIDs are
the write boundary), two things fail because the restricting-SID second access
check fails:

- **Schannel TLS:** `AcquireCredentialsHandle` returns `SEC_E_NO_CREDENTIALS` in any
  token with restricting SIDs (openai/codex#17459, with reproductions on several
  Windows machines; kGraf's own runs on a hosted Windows Server 2025 machine).
  System curl, PowerShell, .NET and WinHTTP installers cannot use HTTPS.
- **Git Bash:** the MSYS2 runtime creates its signal pipe with an access list for
  the user, Administrators and SYSTEM, then opens it for writing; none is a
  restricting SID, so the runtime stops ("couldn't create signal pipe, Win32 error
  5"; openai/codex#7031, #15016).

Codex's elevated sandbox already runs as dedicated users but keeps restricting SIDs
on their tokens, and still fails Schannel (codex#17459, comments of 2026-08-20 and
2026-08-21). So the fix is the user's own token without restricting SIDs, with ACLs
as the write boundary.

A firewall rule scoped by user SID tells a dedicated user apart from the user who set it up
(measured on a hosted Windows runner: a rule scoped to the user's SID blocked the
user and the token sandbox alike). That makes network blocking possible for the
first time on Windows, closing the "network open with a notice" gap of 0011's
option 3.

## Options considered

1. **The user's SID as a restricting SID:** Low integrity becomes the only write
   boundary; other workspaces and sandboxes become writable to each other. Rejected.
2. **A Low-only token for shell commands:** the same cost; may still fail on MSYS2
   objects. Rejected.
3. **A dedicated sandbox user, set up once with administrator rights:** keeps
   write confinement, fixes Schannel and Git Bash, adds read confinement and network
   blocking. Costs one UAC prompt per machine and two local accounts. **Chosen.**
4. **busybox-w32 instead of Git Bash:** keeps the token; git's own `sh`, hooks and
   `!` aliases still fail; Schannel still fails. Rejected.
5. **Leave it:** Windows workers cannot run `sh` or `bash`, and HTTPS fails for
   Schannel tools. Rejected.

## Consequences

- Windows needs one administrator step per machine; macOS and Linux need none
  (`kgraf sandbox setup` says so there).
- **Open for the ACL unit:** workers that run as one user can write each other's
  worktrees, which the per-root SIDs of the token sandbox prevented. A small pool of
  users, one per worker slot, closes this; setup is idempotent, so a later run adds
  them.
- Until the runner and launch units land, the token sandbox of 0011 stays in use. (Note, 2026-10-08: the runner and
  launch units have landed; see the amendment below.)
- Reuse: the design follows openai/codex `codex-rs/windows-sandbox-rs` (Apache-2.0),
  recorded in `third_party/codex/UPSTREAM`. Differences: passwords from the system
  random source (Codex uses a non-cryptographic generator), sealed by the user's
  process, not the elevated one; the pipe created by the user's process.

---

## Amendment (2026-10-08): the runner, the launch path and the ACLs

Units 2 and 3 are built and compile-checked for Windows. They have not run on Windows.
Until they do, the flags `writes_confined` and `gitlink_protected` stay false.

**The runner.** `kgraf-sandbox-runner` is a binary target of `kgraf-sandbox`, installed
beside the kgraf executable. The user's process starts it with `CreateProcessWithLogonW`
as the sandbox user, suspended, puts it in the kill-on-close job, and resumes it. The call
names the user's own desktop, so no window station access list changes. A named pipe
carries one binary request: program, command line (no 1,024-character limit), working
folder and environment block. The pipe has one duplex instance, refuses remote clients, and
admits SYSTEM, the user and the sandbox user. Each end checks the other's process ID.

The runner then:

1. makes the logon SID deny-only (`CreateRestrictedToken` with `DISABLE_MAX_PRIVILEGE`, no
   restricting SIDs) and checks the result;
2. sets a default access list that admits the sandbox user and SYSTEM;
3. creates its own window station and desktop that admit only the sandbox user and SYSTEM
   (the alternate window station follows Chromium's sandbox, as a design only);
4. starts the command there with an explicit handle list;
5. exits with the command's exit code.

**What the review of these units changed.** A review before the first Windows run found
six defects, each now fixed:

- A job object's user interface limits include one that stops a process in the job from
  creating a desktop, and the runner creates one. The owner's job therefore carries every
  limit but that one, and the runner puts the command in a job of its own, a child of it,
  that carries them all. Without this, no command started at all.
- The command runs as the same user as the runner, and the access list Windows gives a new
  process admits that user fully, while the owner of an object keeps an implicit right to
  rewrite its list. The runner now writes protected lists on its own process, thread and
  token before it opens the pipe: SYSTEM in full, the sandbox user only to wait and read
  basic information, and OWNER RIGHTS to reading the list alone. It reads the process list
  back and refuses to start the command unless every entry for the sandbox user stays
  inside that mask.
- The handshake has one deadline of 60 seconds, and the request and the reply are read on
  their own thread, so ending the job releases it. A runner that stops responding can no
  longer block the caller.
- The Windows proxy listener sits on a loopback port that every program of the sandbox user
  may reach, a command of another run whose network is denied among them. The proxy for a
  sandboxed command there holds a secret for that run, carries it to the command in its
  proxy variables, and answers 407 and forwards nothing without it.
- A console handle carries more than writing text, and a handle given to a child is
  admitted with no access check. A command that inherits this process's console now writes
  to a pipe, and a thread here copies what arrives to this process's own stream.
- The password copy the launch call needs is built in one buffer of the size it needs,
  cleared when it is dropped.

**Selection**, once per process:

- No stored passwords: the token sandbox of 0011 runs, and its network notices say that
  `kgraf sandbox setup` blocks the network.
- The store opens, the user names and SIDs match, and the runner is found: the backend is
  "Windows sandbox user".
- Anything in between: every sandboxed command fails with an error.
- `KGRAF_SANDBOX_TAG` selects the tagged users of a test run.

**Network.** The offline user runs commands with `Network::Deny` and `Network::Proxy`. The
online user runs commands with `Network::Allow`. On Windows the egress proxy for sandboxed
commands binds the first free port from 47100 to 47131. Checks deny the network. Dependency
installs go through the proxy.

**Access lists** for the chosen user:

- write on the writable roots;
- write deny on a top-level `.git`;
- read and write deny on hidden folders;
- read and run on the policy's read roots, the dependency trees, the toolchain (cargo home
  `bin`, `registry` and `git` only, never `credentials.toml`; the rustup home), the `PATH`
  folders inside the user's profile, and the runner executable (for both users).

`CARGO_HOME` and `RUSTUP_HOME` point at the user's toolchain unless the command sets
them. A reparse point is refused, never followed (0083).

**Changes from the plan.**

- (a) **Grants are not removed when a command ends.** Writing a folder's access list
  rewrites the inherited entries of every file below it. A write and a removal for each
  command on a worktree with a large `target` folder would cost seconds to minutes each
  time. Grants on a worktree go with the worktree. Grants on the user's own folders are
  written once and listed in `sandbox-credentials[-tag].grants` beside the password store.
  `kgraf sandbox remove` revokes each listed grant before it removes the users.
- (b) **The command runs on the runner's own window station**, not the user's.
- (c) **The worker pool is deferred.** Commands that run at the same time run as the same
  user and can write each other's writable folders while both run. A pool of offline users,
  one per worker slot, is the planned close; setup is idempotent, so a later run adds them.
- (d) **The firewall writes IPv6 loopback as `::/127`.** Windows Firewall refuses `::1`
  alone with `E_INVALIDARG` (found by a hosted Windows run).

**Known gaps** (the `gaps` text of the backend's strength):

- The sandbox user reads whatever every local user can read, but not the user's profile.
- DNS goes through the system DNS client service, which the firewall rules do not scope to
  the sandbox user. A name lookup can leave the machine.
- Concurrent commands share a user (item c).
- Write confinement, the `.git` deny and the network block are unverified on Windows.
