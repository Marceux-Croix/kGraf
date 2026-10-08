# Decision 0097: On Windows, sandboxed commands run as a dedicated local sandbox user, set up once with administrator rights

**Status:** Accepted (2026-10-08). Setup built; the runner and launch path, and the ACL confinement, are later units.
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
- Until the runner and launch units land, the token sandbox of 0011 stays in use.
- Reuse: the design follows openai/codex `codex-rs/windows-sandbox-rs` (Apache-2.0),
  recorded in `third_party/codex/UPSTREAM`. Differences: passwords from the system
  random source (Codex uses a non-cryptographic generator), sealed by the user's
  process, not the elevated one; the pipe created by the user's process.
