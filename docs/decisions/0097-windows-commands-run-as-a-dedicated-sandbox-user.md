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
   a group policy makes local rules ineffective. (Note, 2026-10-09: the loopback rules do
   not act on loopback traffic; WFP filters at the connect layers block it instead, see
   the amendment on the first re-run as the sandbox user.)
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
names the user's own desktop, so no window station access list changes (superseded: see the amendment on job nesting below). A named pipe
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
  that carries them all. Without this, no command started at all. (Superseded:
  the second job itself stopped every command; see the amendment on job nesting
  below.)
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
- (c) **The worker pool was deferred, then built.** See the amendment below.
- (d) **The firewall writes IPv6 loopback as `::/127`.** Windows Firewall refuses `::1`
  alone with `E_INVALIDARG` (found by a hosted Windows run).

**Known gaps** (the `gaps` text of the backend's strength):

- The sandbox user reads whatever every local user can read, but not the user's profile.
- DNS goes through the system DNS client service, which the firewall rules do not scope to
  the sandbox user. A name lookup can leave the machine.
- Commands with the network open share the online user.
- Write confinement, the `.git` deny and the network block are unverified on Windows.

**Amendment (2026-10-08): one user per worker slot.** The open item of the
Consequences above is closed. `kgraf sandbox setup` creates a user per worker
slot as well as the two fixed users: `--slots`, four by default, none with
`--slots 0`. A slot user is named `kgraf-sNN-<hash>` (18 characters, within the
20 a Windows local user name allows), is confined exactly like the offline
user, and is blocked by the same firewall rules.

A command without the network runs as the slot user of its own first writable
root, chosen by a stable fold of that root's text. Two commands in different
worktrees therefore usually run as different users, and neither can write the
other's files. Two commands in one worktree share a user, which they must,
because they share the folder. Where two worktrees fold to the same slot they
share a user, as every command did before. The command with the network open
still runs as the single online user, so two installs that run at the same
time can still write each other's writable folders.

The slot comes from the worktree, not from a worker slot
number threaded through the sandbox interface. The orchestrator knows its
slots, but passing one would change `SandboxPolicy` and every caller of it,
and the worktree is what the access lists are written on, so it is the thing
that must differ. The cost is the fold: two worktrees can share a slot.

The four firewall rules now name every blocked user in one local user list, so
the rule names do not change with the slot count. Each password is judged
against its own account's name only. The stored passwords file carries a role
byte per slot and its version is 2: a file from before this refuses to open
and asks for a fresh setup, rather than reading as a setup with no slots, and
a file that names a slot but not every slot below it is refused for the same
reason. `kgraf sandbox status` reports the slots the machine has, and
`kgraf sandbox remove` clears every slot a larger setup made.

**Amendment (2026-10-08): job nesting, the runner's own objects, its window station,
and diagnostics.** A second review before the first Windows run found the defects below.
Each is fixed and compile-checked for Windows; none has run on Windows.

- **No command could start: nested jobs with user interface limits.** The user's job
  carried every user interface limit but one, and the runner put the command, already in
  that job, in a second job with all of them. Windows nests a job inside another only if
  neither sets user interface limits (Microsoft, "Nested Jobs"), so that assignment fails
  for every command. The second job is gone. On the sandbox user path the user's job sets
  no user interface limits, which also lets the runner join it if the secondary logon
  service put it in a job of its own; the runner and the command stay in it (kill on
  close, no breakaway). The command's user interface isolation is its own window station
  and desktop, as in Chromium's sandbox. A failure to put the runner in the job reports
  the Win32 error and `IsProcessInJob` for the runner. The token sandbox of 0011 keeps its
  limits: it puts the command in one job only. The same rule applies there if kGraf itself
  runs inside a job; the token sandbox's measured runs passed, so it did not in those runs.
- **The command could take over the runner through a thread or its console host.** Only
  the runner's process, main thread and token had protected lists; its other threads and
  its console host had the default list, which admits the sandbox user fully, and the
  runner holds the enabled logon SID the command must not get. The runner is now a
  windows-subsystem program, so Windows makes no console host and no console window for it;
  its standard handles still arrive through `STARTF_USESTDHANDLES`. Before it opens the
  pipe it sets its token's default list to one that admits the sandbox user only to wait,
  so every thread made from then on is protected, then writes and reads back protected
  lists on its process, on every thread it has (`CreateToolhelp32Snapshot`), and on its
  token. The read-back now refuses any entry but SYSTEM's that allows a forbidden right,
  not only the sandbox user's own entry.
- **Two more defects in the same function would have stopped every command.** The runner
  read its token's user through a handle opened without `TOKEN_QUERY`, and wrote its
  token's list to wait-and-read before it opened the token again to make the command's
  token. The runner now opens its token once with every right it uses, and makes the
  command's token before it changes its own default list, so the command's token object
  has the list a token of this user normally has and the command can still open its own
  token, as programs such as the MSYS2 runtime do.
- **The runner's window station.** The documentation of `CreateProcessWithLogonW` says that
  with a desktop named, the application must give the user access to it, even for
  `WinSta0\Default`; whether the runner reaches the user's desktop through the logon SID
  under a service session was unknown. kGraf now creates a window station and a desktop
  of its own for each sandbox user, once per process, whose lists admit SYSTEM, the
  user and that user (every right but rewriting the list, taking ownership, deleting and,
  on the desktop, switching to it), and names that desktop when it starts the runner. No
  list of the user's session changes. Creating the desktop switches the process's window
  station for a moment, once per user and process; Chromium's broker does the same.
- **An early runner end said nothing.** When the runner ends before it connects, the error
  carries its exit code and names the known ones: `0xC0000142` (a DLL failed to start,
  most likely the window station or desktop), `0xC0000022`, `0xC0000135`, and the
  runner's own codes, `0x2B470005` among them (it could not protect its own objects). A
  self-protection failure is also sent over the pipe with its step and Win32 error.
- **A command of the same user could hold the runner pipe.** The pipe admits the sandbox
  user, so a command of that user already running can connect first. The user's process
  disconnects any client that is not the runner it started and waits again (16 times at
  most), and the runner waits for a busy pipe instead of giving up, so this is no longer a
  denial of the start; the error names any process that connected first. The list is not
  narrowed: the runner and the command are one user, and the only SID that tells them
  apart is the logon SID, whose value under the secondary logon service is not measured.
- **Password copies.** Every buffer that holds a password is cleared when dropped: the
  setup pipe's password line is built at its final size, the user's side reads the pipe
  into cleared buffers instead of `BufRead::lines`, and the wide strings passed to
  `NetUserAdd`, `NetUserSetInfo` and `LogonUserW` are allocated at their final size and
  cleared.
- **Account lockout.** A logon refused with `ERROR_ACCOUNT_LOCKED_OUT` says so and how to
  fix it: `kgraf sandbox setup` resets the passwords and clears the lockout (it rewrites
  the user's flags without `UF_LOCKOUT`); otherwise the lockout ends with the lockout
  policy's duration. A wrong password, a disabled account, a denied logon type and a
  disabled Secondary Logon service are named the same way.
- **A tag without a store.** With `KGRAF_SANDBOX_TAG` set, a missing store is an error, not
  a fall back to the token sandbox, so a test that meant to run as the sandbox user cannot
  pass while it tests the other backend.

Windows tests in `tests/runner_windows.rs` check, from inside the command, that no runner
thread opens for `THREAD_SET_CONTEXT` or any other right that changes it, that the runner's
process and token refuse everything but waiting and reading, that the runner has no
console host, that the command opens its own token, and that standard input and output
pass through the runner.

**Amendment (2026-10-08): worker slots are held, not folded, and the online user is a pool.**
This replaces the fold of the amendment above. Two defects of it are closed:
two worktrees whose paths folded to one slot ran as one user with no message,
and two dependency installs that ran at the same time ran as the one online
user. A third defect is closed with them: the elevated step was never told
`--slots`, so a setup made no slot users at all.

- **Two users per slot.** `kgraf sandbox setup --slots N` makes, per slot, one
  user without the network (`kgraf-sNN-<hash>`, blocked by the firewall rules)
  and one with it (`kgraf-oNN-<hash>`, not blocked), beside the two fixed users.
  The stored passwords file keeps version 2 and gains a role byte for the
  online slot users (128 + slot). A file with slot users of one kind only is
  refused and asks for a fresh setup. `status` reports both users of every
  slot the machine has. `remove` deletes every user the undo record lists.
- **The elevated step is told the slot count.** Its arguments carry
  `--slots N` for a setup, and it refuses a setup without them. The user's
  process stores nothing unless the passwords it receives are exactly the
  users it asked for.
- **A slot is held.** A command takes a slot of its pool (without or with the
  network) for its first writable root, its folder. A slot has a lock file;
  the process that holds the slot holds the lock, so holders of different
  folders never share a slot, in one process or in two. Holders of one folder
  in one process share its slot, because they share the folder. The command
  holds its slot until its job is closed.
- **The worker's slot comes from the worker.** The shell tool of a worker takes
  the slot of its worktree at its first command and holds it for its life
  (`kgraf_sandbox::worker_slot`, passed with `SandboxPolicy::with_worker_slot`).
  A command in the same process with no slot of its own, such as a gate in that
  worktree, finds the worker's slot by its folder. A command with no worker
  (`kgraf worker` outside an orchestrated run, `kgraf sandbox selfcheck`, a
  check in its own folder, a dependency install) takes a slot for itself alone.
- **More folders than slots.** A worker is refused at once, with an error that
  names `kgraf sandbox setup --slots`. A command without a worker waits up to 60
  seconds for a free slot, then is refused the same way. It never runs as the
  user of another folder.
- **Grants move with the slot.** A write grant stays on a folder when its
  command ends, as before. A table beside the stored passwords
  (`sandbox-credentials[-tag].slots`) lists the folders each slot user holds a
  grant on. A free slot already bound to the folder is taken first. A slot that
  moves to another folder first loses its grant on every folder it held, so a
  user never keeps write access to a folder another user may now work in.
  `kgraf sandbox remove` revokes every folder the table lists, then deletes it.
- **Linked dependencies.** Before kGraf links the checkout's installed
  dependencies into a worktree, it grants write on the worktree to the user of
  the worktree's own slot only. It granted every sandbox user, so a command in
  one worktree could write another that had dependencies linked.

Choices made with this amendment:

- (e) **Refuse a worker, let a command wait.** Waiting would hide a setup with
  fewer slots than the machine runs workers; the orchestrator already bounds how
  many run at once. A command without a worker waits, because its holders are
  commands that end.
- (f) **The slot comes from a held lease, not from a number the orchestrator
  assigns.** The orchestrator and host number no worker slots: their limits are
  counts, and gates, checks and installs build their own sandbox policies. A
  number assigned in one process would also not stop a second kGraf process from
  taking the same user. The lease is keyed by the folder the access lists are
  written on, which is the thing that must differ.
- (g) **A command without a worker takes a slot for itself, without a notice.**
  It cannot share a user with another folder, so nothing is degraded; when no
  slot is free it fails with an error.
- (h) **Only the worktree's own slot user is granted before dependencies are
  linked**, not that slot's online user. The online user of the same slot can be
  held by another folder at that moment, so granting it would let that folder's
  install write this worktree. A command with the network open in the worktree
  is granted when it takes its slot.

Open, to watch in the next Windows run: whether writing a grant on a worktree
that already holds a dependency junction follows the junction. That happens
only when a worktree's command takes a slot other than the one granted before
the link, which the table makes rare. Two users can still both hold a grant on
a folder that two of their folders share, such as a project's package stores;
those writes are shared by design.

**Amendment (2026-10-08): the DNS filter.** This closes the DNS item of the known gaps
above in code. A Windows run has not proved it, so the gap stays in the backend's gaps
text until one does.

A program looks a name up through the DNS Client service (Dnscache). The service runs as
NETWORK SERVICE and sends the query as that account, so the firewall rules, which match
the sandbox user's SID, do not see it. `kgraf sandbox setup` now adds a step,
`dns-filter`, after the firewall rules. The step adds three persistent Windows Filtering
Platform (WFP) objects in one transaction:

- a provider and a sublayer, both named "kGraf sandbox";
- one filter on the user-mode RPC layer (`FWPM_LAYER_RPC_UM`) with the action block and
  two conditions: the RPC interface of the DNS Client service (`RPC_IF_UUID`,
  `45776b01-5956-4485-9f80-f428f7d60129`) and the caller's token
  (`REMOTE_USER_TOKEN`).

The token condition is a security descriptor with one allow entry for
`FWP_ACTRL_MATCH_FILTER` per user the firewall rules block: the fixed offline user
(`kgraf-off-<hash>`) and each slot's user without the network (`kgraf-sNN-<hash>`).
The list comes from the same function as the firewall rules' user list, so the two
cannot drift, and a unit test holds them equal for one slot, four and the maximum.
The sandbox group is not in it, because the group also holds the online users
(`kgraf-on-<hash>` and each `kgraf-oNN-<hash>`), which need name lookups for
dependency installs. The installing user is not in it. A lookup by a
blocked user then fails inside its own process. The firewall rules already block the
direct path, a query the process sends itself.

Keys and undo. The provider, sublayer and filter keys are derived from the installing
user's SID and the tag, as the user names are. Two users of one machine, or a tagged test
setup and a real one, never share an object. The undo record lists the three keys
(`wfp-provider`, `wfp-sublayer`, `wfp-filter`). `kgraf sandbox remove` deletes the filter,
then the sublayer, then the provider, by those keys only, before it removes the users. It
does not use `netsh rpc filter delete`, which deletes every RPC filter on the machine. A
key that is not this user's derived key is refused, like a user name that is not ours.
Setup again deletes the filter and adds it from the current list of blocked users, so a
setup with more slots refreshes it in place.

Choices made here:

- (e) **Keys per installing user**, not fixed keys as in codex. A fixed provider would be
  shared by two users of one machine, and removing one setup could delete the other's
  objects.
- (f) **The block clears the action right** (`FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT`), so a
  permit filter in another sublayer cannot override it.
- (g) **Status shows presence, never effect.** `kgraf sandbox status` has a `DNS filter`
  line: present, MISSING, or unknown where the account may not read WFP filters.
- (h) **The proof is `kgraf sandbox selfcheck`**, which gets a `dns:` line under the
  sandbox user. It uses three new names under `.invalid`. The installing user's process
  looks up the first, as a control: a DNS server's answer proves a working network. The
  sandbox user looks up the second with `Resolve-DnsName` in Windows PowerShell. The
  installing user's process then asks the DNS Client cache, without sending anything,
  about all three; the service caches a name error, so the second name must look like
  the third (never looked up), not like the first. The line passes only when the
  sandbox user's lookup got no server answer and the cache shows the service did not look
  its name up. A server answer or a cached name is `dns: not confined (lookups leave the
  machine)`. A run that cannot decide (no network, a cache that does not tell the names
  apart, a probe that printed no status) is `dns: not proved`. Both fail the whole check.
  A blocked RPC call and a server's name error can look alike to a program, so the cache
  is a second, independent witness.
- (i) **The Windows self-test lets a DNS-only failure through its gate.** If selfcheck
  proved everything but DNS on the sandbox user, the re-run of the phase still runs, and
  the summary says `DNS FAILED`. One unproved filter must not throw away the other
  evidence the phase collects.
- (j) **The fallback is the failed line.** If Windows does not honor the filter, the gap
  stays, and selfcheck reports it as a failed line, never as a pass.

Reuse. The WFP engine, transaction, provider and sublayer code is ported from openai/codex
`codex-rs/windows-sandbox-rs/src/wfp.rs` (Apache-2.0, commit
665e5f45ab91d43ec3a49487fb9be90109dd2875). The `wfp` crate (mullvad/wfp-rs 0.2.0, MIT OR
Apache-2.0) was checked first. It builds no RPC-layer filter and no security-descriptor
condition. Codex's own filters match connects by the sandbox user's process at the
connect layers, so they do not stop the DNS Client service either.

Not yet confirmed on Windows:

- the interface UUID, taken from the ReactOS reimplementation (`dnsrslvr.idl`), not from
  Windows' own `dnsrslvr.dll`. A wrong value blocks nothing, and only the selfcheck shows
  it;
- whether WFP evaluates RPC filters for local RPC (`ncalrpc`) calls to this service, and
  whether the caller-token condition matches the client's token there;
- the status a program gets from a blocked lookup, and whether the resolver then sends
  the query from the process itself (the firewall rules block that);
- whether `localhost` lookups by an offline or slot user go through the service and fail;
- whether a standard user may read WFP filters (if not, status shows `unknown`);
- whether the DNS Client cache keeps name errors on every machine; where it does not,
  selfcheck says `dns: not proved`;
- the `netsh wfp show filters` text of the filter (name, layer, action), which a Windows
  test checks.

**Amendment (2026-10-08): the DNS filter is at the name resolution cache layers, not only
at the RPC layer.** A confinement review of the filter above found that the `RPC_UM` filter
can never see a local name lookup, so the setup reported success, `netsh wfp show filters`
listed the filter, `kgraf sandbox status` said present, the Windows test passed, and the
lookup still left the machine. None of the code below has run on Windows either.

Evidence for the finding. `FWPM_LAYER_RPC_UM` is classified in user mode by the RPC
runtime and is reached only for a DCE/RPC call with a remote caller, never for a local
ALPC call. The DNS Client service serves its interface on `ncalrpc` only, which is LRPC
over ALPC, so a lookup on this machine is never classified at that layer. Microsoft's own
`netsh rpc` field name for the condition is `remote_user_token`, "Token for remote user
authentication", and the condition identifier table gives
`FWPM_CONDITION_REMOTE_USER_TOKEN` as "The identification of the remote user".

Choices made here:

- (k) **Two more filters, at `FWPM_LAYER_NAME_RESOLUTION_CACHE_V4` and
  `FWPM_LAYER_NAME_RESOLUTION_CACHE_V6`**, each a block with the single condition
  `FWPM_CONDITION_ALE_USER_ID`. Microsoft's "Filtering conditions available at each
  filtering layer" (Fwpmu.h, ms.date 05/02/2024) lists `FWPM_CONDITION_ALE_USER_ID` at
  both of those layers for "Windows 7 and later", and "Filtering condition identifiers"
  gives it as "The identification of the local user. Data type:
  FWP_SECURITY_DESCRIPTOR_TYPE". That is the same data type as the RPC layer's condition,
  so the existing user match descriptor is reused unchanged, with the same one allow entry
  for `FWP_ACTRL_MATCH_FILTER` per blocked user. This is the only layer pair at which WFP
  is handed the local caller of a name lookup.
- (l) **The `RPC_UM` filter stays**, for a caller that reaches the interface over a
  network transport. Its code comment and its description say that it cannot cover the
  local path.
- (m) **Five objects, five keys.** The key set grows from three to five: provider,
  sublayer, and one filter per layer. Each key is still derived from the installing user's
  SID and the tag, removal is still by those keys only, a key that is not this user's
  derived key is still refused, and it is still one transaction. The RPC filter keeps the
  undo record word `wfp-filter` it had when it was the only filter, so a record an earlier
  setup wrote still parses; the new ones are `wfp-filter-name-cache-v4` and
  `wfp-filter-name-cache-v6`. `kgraf sandbox status` prints one line per filter, each
  naming its layer and its own key, because a single line could say present while another
  layer was open.

**Not known, and a Windows run is the only way to settle it.** Whether a block at the
name resolution cache layers makes the lookup fail, or only keeps the DNS Client service
from reading and filling its cache while the query still leaves the machine. Microsoft
documents the layer in one sentence, "This filtering layer allows for querying the names
recently resolved by the system", and says nothing about the effect of a block there. The
gap therefore stays in the Windows sandbox user backend's gaps text, and
`kgraf sandbox selfcheck` stays the only proof: it judges the filters by their effect,
never by their presence.

**Amendment (2026-10-08): the DNS probe must prove a local failure.** The same review
found that `dns: confined` could be printed with no probe evidence. A line equal to
`KGRAF-DNS-CODE=` with nothing after it set the "a status was printed" flag by its prefix
alone and left the status unset; the old polarity, "anything that is not a server answer is
a local failure", then read that as confinement, and `sandbox selfcheck: ok` followed. The
output really can be cut there: after a normal exit the readers get only a short grace and
a truncated tail is returned as if it were whole.

Changed here:

- (n) **Three-way classification.** The probe's status is sorted three ways: a DNS
  server's answer is a leak, a named local refusal is the confined side
  (`ERROR_ACCESS_DENIED`, `E_ACCESSDENIED`, `RPC_S_SERVER_UNAVAILABLE`,
  `EPT_S_NOT_REGISTERED`, `DNS_ERROR_NO_DNS_SERVERS`, `WSAEACCES`), and anything else is
  `dns: not proved` with the status named. There is no "not a server answer, so a local
  failure" case.
- (o) **A timeout is not proof.** `ERROR_TIMEOUT` and `WSAETIMEDOUT` are `not proved`
  with the reason named: the probe uses `-QuickTimeout`, so a query that left the machine
  and got no answer in time also fails inside the process.
- (p) **An end marker.** The probe prints `KGRAF-DNS-END` last, on both the `try` and the
  `catch` path. Output without it is refused, so a cut-off tail is reported instead of
  read as a pass. Output that carries the end but neither a status nor a named error is
  refused too: a missing witness must never read as a pass.
- (q) **The cache is read twice where the first reading passed.** The first reading is
  taken as soon as the probe's process has exited, which can be before a query that left
  the machine has its answer. A second reading five seconds later decides. A leak or a
  `not proved` is decided at once: a second reading cannot turn either into a pass.
- (r) **A disabled filter is present and not effective.** The read returns each filter's
  flags, not only its key. `kgraf sandbox status` reports a filter that carries
  `FWPM_FILTER_FLAG_DISABLED` as WRONG, not present, so the setup is not ready; the setup
  step is a Create, not an Update of something that works; and the Windows test fails on
  the flag. The documentation does not agree with itself on whether a provider that names
  no service has its filters disabled at every start, which is the reason to read the flag
  rather than trust either sentence. The Windows test plan now has a reboot step, so one
  run settles it.
- (s) **The WFP objects are removed last, after the accounts.** Removal stops at the first
  step that fails. A block filter that will not go is fail-safe: it blocks name lookups for
  users that no longer exist. An account that will not go is not. With the filters first, a
  stopped Base Filtering Engine, a firewall service that is off, or a sublayer another
  object still names left the sandbox users, the group, the denied logon rights, the
  firewall rules and the folder deny entries on the machine. Nothing in the removal of a
  filter needs the users: its user condition is a security descriptor of SIDs, which
  deletes with or without them. The record still lists any key that did not go, so a second
  `kgraf sandbox remove` retries it.
- (t) **The probe has its own bound.** `kgraf sandbox selfcheck` waits at most 120 seconds
  for the lookup in the sandbox, not the two hours a sandboxed command gets when it names
  no bound of its own. The bound is set by the probe's longest legitimate holder, Windows
  PowerShell 5.1's own start-up on a cold machine; the lookup itself is bounded by
  `-QuickTimeout`. A probe that reaches the bound, or that will not start, is
  `dns: not proved` with the reason, never an error that throws away the lines the check
  has already proved.
- (u) **The sublayer weight is the maximum**, not the reuse source's mid-range 0x8000, and
  the wording of (f) above is corrected: clearing the action right stops a *lower*-weight
  sublayer from permitting over the block, not every sublayer. Microsoft's "Filter
  Arbitration" page: traffic traverses sublayers from the highest priority to the lowest, a
  permit whose action right is cleared is a hard permit that only a callout veto can block,
  and "In order to silently override 'Hard Permit' filters a firewall has to add its
  filters within a higher priority sub-layer". At 0x8000 a third-party sublayer of higher
  weight with a hard permit was decided first. The maximum is right here because this
  sublayer holds block filters only: a sublayer that can never permit cannot take traffic
  away from another provider by being decided first. Setup now deletes and adds the
  sublayer as well, inside the same transaction and after the filters that name it, so a
  machine set up by an earlier version picks up the weight.
- (v) **A failed DNS step says what it leaves.** A first setup that fails stores no
  passwords, so no command runs as a sandbox user. A re-run is different: the credential
  store of the earlier setup stays, and `state.rs load()` does not look at the WFP keys, so
  commands keep running as the sandbox user with whatever filter the machine has. A command's
  launch cannot check the filters itself, because reading a WFP object can need
  administrator rights and a launch is a standard user's. The failing setup therefore names
  it, in the error of the step: the users and the stored passwords of the earlier setup stay
  in place, a name lookup can leave the machine, and `kgraf sandbox status`,
  `kgraf sandbox selfcheck` and `kgraf sandbox remove` are the commands to run. The backend's
  gaps text carries the same sentence for the case where nobody reads the error.

**Amendment (2026-10-08): bounded waits, pipe ends inheritable for one call, no console
input, the runner protected before it runs.** The Windows self-test hung in the
kgraf-host tests. A fix outside the backend bounded every child wait and lock
(`kgraf_core::child_run`, `kgraf_core::waits`); this one closes what remained in it.
None of it has run on Windows yet.

- **Every wait for a sandboxed command ends.** `SandboxedChild::wait_timeout(bound)`
  waits with `WaitForSingleObject` and the bound. `wait`, `wait_with_output`, and so
  the Windows `status` and `output`, are bounded by `WAIT_BOUND` (2 hours); callers
  with their own bound use `wait_within` and `wait_with_output_within`. At the bound
  the command's tree is ended and the error is `TimedOut`, naming the process and the
  bound. After a normal exit the tree is ended within `child_run::KILL_BOUND`, and the
  readers get `child_run::DRAIN_GRACE`; a pipe still open is recorded
  (`waits::note_pipes_held`) and the output returned. The steps sit behind a small
  trait, unit-tested on every host.
- **A command's pipe ends are inheritable only for its own creation call.** Every
  handle the backend makes for a command's standard streams is created
  non-inheritable. They are marked inheritable only around `CreateProcessAsUserW`
  and `CreateProcessWithLogonW`, under one process-wide spawn lock
  (`kgraf_core::child_run::spawn_lock`, bounded at 60 s) that `child_run::run` also
  takes around its spawn, and cleared before the lock is released. Before, they were
  inheritable from their creation, and on the sandbox-user path through the whole
  runner handshake: any spawn of std or tokio in kGraf could copy them, and the
  command's reader then waited for that unrelated process. The flag is still needed
  for the call: the handle list accepts only inheritable handles, and the logon call's
  handling of non-inheritable standard handles is not documented. **Not closed:** a
  spawn through std, tokio or process-wrap that does not take the lock can still copy
  the handles during the one creation call.
- **Standard input is never a console.** An inherited standard input that is a
  console, or any other character device, is refused on both paths (the sandbox-user
  path checks first and names its backend). The default stays the NUL device.
- **The runner is protected before it runs.** The runner starts suspended
  (`CREATE_SUSPENDED` is a documented flag of `CreateProcessWithLogonW`). After it joins
  the job and before it is resumed, kGraf writes protected lists on its process (the
  sandbox user may only wait, so no program of that user can open the runner's token,
  whose list is still the default) and on its main thread, and reads each back with the
  runner's own check. The runner's `protect_self` is unchanged and compatible. The
  rights of the handles the logon call returns are not measured: a failed write does
  not stop the launch, its error (step and Win32 code) is kept on the child
  (`SandboxedChild::runner_lists_before_resume`) and added to any later launch error,
  and the runner's own protection is then its first step. **Not closed, and stated in
  the backend's gaps text:** the moment between the runner's creation and kGraf's
  write, threads the runner starts before it protects itself (the loader's), and the
  whole runner where the write fails.
- **The remaining unbounded locks are bounded** with `kgraf_core::waits::lock_file`,
  each failing with a named timeout. In the forked child on Unix, where only
  async-signal-safe calls are allowed, the worktree hold is taken with
  `LOCK_SH | LOCK_NB` in a loop using `clock_gettime` and `select` as the pause.

Choices made with this amendment (each bound is chosen for its longest legitimate
holder; the reasoning is in a comment beside each constant):

- (i) `WAIT_BOUND`, 2 hours: a person's build and test through `kgraf-wintest
  sandbox-run`, which runs for most of an hour.
- (j) `SPAWN_LOCK_WAIT`, 60 s: one creation call per holder, a logon at worst a few
  seconds.
- (k) `HOLD_WAIT`, 1 hour, the worktree hold in the forked child: an exclusive holder
  may wait 30 minutes for the worktree-admin lock, then run git commands of up to 10
  minutes or delete a large build folder.
- (l) The migration lock, 30 minutes: a rename and a few bounded git repairs.
- (m) The diagnostics lock, 60 s: one append and at most one trim of a capped file.
- (n) The MCP keychain lock, 10 minutes: a person answering the operating system's
  keychain prompt.
- (o) The desktop settings lock, 5 s: another window's write of one small file, waited
  for on the window's own thread.
- (p) A failed write of the runner's lists before resume falls back to the runner's
  own protection rather than refusing the command, because the rights of the logon
  call's handles are not measured; the next Windows run measures them.

**Not closed, found while bounding the hold:** a Unix child that waits for its
worktree hold before exec holds copies of every descriptor open in kGraf at its fork,
the close-on-exec status pipes of other spawns in flight among them, so a spawn that
was in flight at that fork waits for that child as well (now at most `HOLD_WAIT`).
Taking the shared hold in the parent before the fork would remove the wait from the
child, but the async spawn path builds its own process in another crate.

**Closed (2026-10-08):** the shared hold is now taken in the parent, before the fork
(`HoldGate`, in the Unix child module). The parent opens the hold file close-on-exec and
waits for `LOCK_SH` with a try-lock and a sleep, at most `HOLD_WAIT`, then fails with
`SandboxError::HoldTimeout`, which names the hold file. The child only marks the
inherited descriptor to survive exec; `flock` belongs to the open file, so the parent's
copy and the command's are one lock, and the parent closes its copy after the spawn and
never unlocks it. A child forked without a taken hold fails with `ENOLCK` and says so.
`SandboxedCommand::spawn`, `output` and `status` take the hold themselves; `into_parts`
returns the gate as well, and the two async callers (the shell tool and the check
runner) take it on a blocking thread, stopped by a cancel or a dropped check. Test:
`a_spawn_waiting_for_a_quiesced_worktree_does_not_delay_other_spawns`
(`kgraf-sandbox/tests/reopen.rs`).

**Amendment (2026-10-08): the record folder, the shared folders, links before a first
grant, and Java behind the proxy.** Four fixes from a review of the setup and the
backend. None of it has run on Windows yet.

- **The undo record's folder is trusted only when administrators alone can change
  it.** Every local user may create a folder under ProgramData and holds full
  control of what it creates. A sandboxed command could create kGraf's record folder
  first and put a link in it, and the administrator step would then write a file that
  the command chose. Setup now creates the folder with a protected access list (owner
  Administrators; full control for SYSTEM and Administrators; read and list for Users,
  because `kgraf sandbox status` reads the record without administrator rights;
  `TRUSTED_FOLDER_SDDL`). It refuses a folder that exists already when its owner is
  not SYSTEM, Administrators, TrustedInstaller or the elevated user, when another
  principal may change it, or when it is a link. It takes ProgramData from the
  known-folder call, not from the environment. It opens, writes and deletes the record
  without following a reparse point. Remove refuses an untrusted folder before it reads
  or writes in it.
- **The folders every local user may write are closed to the sandbox group.** The
  sandbox users could write wherever every local user may. Setup writes a deny entry
  for the sandbox group on each folder of that kind: create a file or a folder in it,
  and change it (`CLOSED_FOLDER_DENY`); reading stays. The entry is on that folder
  alone, never inherited: an inherited entry on a drive root would rewrite the access
  list of every file on the drive, and an administrator cannot write the lists of the
  system's own files. Setup lists each folder in the undo record with the group's SID;
  remove takes exactly that entry off before the group goes; status reports each
  folder. **Not closed, and stated in the backend's gaps text:** folders that exist
  below these and already grant every user write stay writable (many do below
  ProgramData).
- **No first grant on a tree while a link in it leads outside it.** Writing an
  inheritable entry on a folder makes Windows write it on everything that already
  exists below, and whether that walk enters a junction is not documented. An earlier
  command could plant a junction that points at the owner's files, and the next grant
  for a new SID (another sandbox user, a new capability) would carry write access onto
  them. Before the first write grant or read grant for a SID on a tree, kGraf walks the
  tree without entering links (`first_outside_link`) and refuses while a link resolves
  outside the tree or does not resolve. A link that stays inside is allowed (pnpm lays
  out `node_modules` with such junctions). A Windows test plants a junction and checks
  the target's list; another measures whether the propagation follows a junction.
  **Not closed:** a command of another sandbox user in the same folder could plant a
  link between the walk and the write. The first grant for a SID on a folder comes
  before any command of that SID runs there.
- **The proxy secret is kept out of `JAVA_TOOL_OPTIONS`.** Every JVM prints that
  variable on start ("Picked up JAVA_TOOL_OPTIONS: ..."), so the secret went into the
  command's output and transcript. The JDK also refuses Basic proxy authentication for
  HTTPS tunnels by default (`jdk.http.auth.tunneling.disabledSchemes`), so the secret
  did not get a Maven or Gradle download through. Java now gets the proxy host and port
  without the secret; only the URL variables (`HTTP_PROXY`, `HTTPS_PROXY`) carry it. A
  request without the secret gets the proxy's 407 with a text that says Java dependency
  installs through the proxy are not supported on Windows, and
  `EgressProxy::refused_without_secret` lists each such request for the caller's
  report, so a 407 is never the only trace. Java installs on Windows fail loudly, not
  by a firewall timeout.

Choices made with this amendment:

- (q) The closed folders: the root of each fixed local drive whose file system keeps
  access lists, ProgramData, Public and the folders directly in it, and the
  `Temp`, `Tasks` and `tracing` folders of the Windows folder (`closed_folders`,
  `WINDOWS_CHILDREN`). These are where every user may create entries by default and
  no program of a sandbox user needs to: each command gets its own `TMP` and `TEMP`.
  Other folders of that kind under the Windows folder belong to TrustedInstaller,
  whose lists an administrator cannot change without taking ownership, so they stay
  open, and the gaps text says so.

**Amendment (2026-10-08): the command's window station from the owner's process, every
spawn under the spawn lock, the logon call measured, the runner's list check.** None of
it has run on Windows yet.

- **The runner makes no window station.** The runner, running as the sandbox user, made
  a window station and desktop for the command with `CreateWindowStationW`. Nothing
  documents that a local user started through the secondary logon service may create a
  window station in the session, and a refusal there would stop every command; the
  upstream Codex sandbox makes only a desktop, from the parent's side. kGraf's own
  process now makes the command's window station and its `Default` desktop as well as
  the runner's, once per sandbox user and process, with lists that admit SYSTEM, the
  owner and that sandbox user (the sandbox user gets every station right, and every
  desktop right but switching to it; never rewriting the list, taking ownership or
  deleting). The owner's entry lets kGraf's process make the desktop on the station it
  entered; the command holds neither the owner's SID nor, enabled, the logon SID, so the
  entry admits no sandboxed program. The runner request (version 2) names
  `<station>\Default` in a new field; the runner checks that it can open the station
  (`OpenWindowStationW`) and starts the command there. Each creation step names its
  Win32 call in its error, and each station made is recorded with its name and the
  sandbox user's SID. The commands of one sandbox user share that station, as they share
  that user's other objects; it stays apart from the owner's session and from the
  runner's station.
- **Every spawn takes the spawn lock.** On Windows, std marks its child's pipe ends
  inheritable during its own spawn, and `CreateProcessWithLogonW` copies the caller's
  inheritable handles into the runner, so a spawn that did not take the lock could leave
  a pipe end in a runner, and its reader then waited as long as that runner lived (up to
  `WAIT_BOUND`). Every process creation in kGraf now goes through the lock:
  `child_run::run`, the new `child_run::spawn`, `output` and `status` for std commands
  (the wait outside the lock), and `child_run::spawn_locked` for tokio, process-wrap and
  rmcp spawns. A test reads every crate's source and fails on a direct `spawn`, `output`
  or `status` call outside test code that is not one of these, unless it is listed with
  the reason it cannot overlap a sandboxed command's start (the sandbox's own Unix
  spawns, `SandboxedCommand::spawn`, which takes the lock on Windows, a macOS and Linux
  `cp`, test-support code). The sites routed: the pytest probe of the gates, the cargo
  dependency fetch, the MCP stdio servers, the check runner's `tar`, the `winget` probe
  and install, the update installer launch, and the benchmark and soak programs' `git`,
  `tar` and stub spawns. The Windows self-test runner is a separate program and is not
  read. **Not closed:** a process a library starts on its own; none is known.
- **The logon call holds the lock for its whole length, and is measured.** The window
  cannot be narrowed: the standard handles must stay inheritable until seclogon has
  copied them, at a point inside the call that is not documented, and the call copies
  every inheritable handle, so it must exclude other spawns whatever the standard handles
  are. Each call's duration and the wait for the lock before it are recorded, and the
  spawn lock counts its takes, timeouts, longest wait and longest hold. `kgraf sandbox
  selfcheck` prints these lines on standard error, so the Windows self-test collects them.
- **The runner's list check reads every entry type.** It skipped every entry that was not
  a plain allow entry, so an allowed-object, callback or compound entry granting the
  sandbox user write access to the runner passed unchecked. The check now reads the list's
  bytes in a platform-neutral function: plain allow entries are checked, deny entries
  skipped, any other type fails.

Choices made with this amendment:

- (q) The command's station and desktop also admit the owner, as the runner's do, so that
  kGraf's process can make the desktop on the station it entered whatever `CreateDesktopW`
  checks; the command holds no SID that entry names. A list without the owner is one
  change of two SDDL strings once a Windows run shows `CreateDesktopW` succeeds without it.
- (r) One command station per sandbox user and process, not one per command: making a
  desktop switches the whole process's window station for a moment, and commands of one
  sandbox user already share that user's files and objects.
- (s) The runner's list check fails on any entry type it does not read, unknown types
  included, not only the other allow types: the lists kGraf writes hold plain allow
  entries only.

**Amendment (2026-10-08): the link check in front of every inheritable access
entry.** The guard that refuses to write an access entry on a tree while a link
below it leads out of that tree stays, because whether Windows carries an
inheritable entry through a junction is still unmeasured
(`measure_whether_propagation_follows_a_junction`). It refused three legitimate
cases for good, and three more writes went around it. None of this has run on
Windows yet.

- **A link into a folder the same grant admits is allowed.** The check now takes a
  set of allowed targets: the folders the same call admits the same user to (the
  writable roots and the read roots), and, for a grant on one of the checkout's own
  dependency trees, the checkout that tree belongs to. This closes three refusals: a
  worktree that comes back on another slot user, where kGraf's own dependency
  junctions are already in place; a read grant on the checkout's tree in an npm,
  yarn or pnpm workspace, where `node_modules/@scope/pkg` is a junction to a package
  of the checkout; and the first grant for the online slot user on such a worktree
  (item (h) above). The refusal message no longer says "remove the link": kGraf makes
  its own links again. A link anywhere else is still refused and nothing is written.
  The policy carries the checkout the trees came from
  (`DependencyLinks::checkout`), so no name such as `node_modules` is hard-coded.
- **Why those targets carry no new reach, and what is still unknown.** The argument
  for an in-tree link is that the propagation reaches only what the grant covers
  anyway; it holds for a folder the same call grants the same user. Two things are
  still wider than the grant where propagation does follow a junction: a write grant
  on a worktree whose dependency link leads to the checkout's tree would carry write
  onto a folder the user is granted to read, and an entry carried onto a folder
  admits the user to what that folder holds now. The checkout root is therefore
  never in the set for a write grant: a command may write in its own worktree and
  could plant a junction to the checkout root there. A grant on a dependency tree is
  read and run only, and no command can write inside the checkout's trees, so no
  command can plant a link there.
- **The tree is walked again after the write.** A command of another sandbox user
  that may write in the same folder can plant a junction between the walk and the
  write (`mklink /J` needs no privilege), and Windows writes the tree in one call of
  its own, so the window cannot be closed here. A link the second walk finds now
  fails loud, naming the folder and saying the entry may already have been carried
  onto the link's target.
- **The deny entries and the Low label go through the same check.** The deny of a
  top-level `.git`, the deny on a hidden folder and the Low label are inheritable
  writes as well and were not checked at all. The Low label is now written only
  while it is absent, as the grants are: before, every command rewrote it, which
  walked the whole tree's labels each time and could not be guarded at that cost.
- **A folder the walk cannot read is named**, not reported as a failure of the root.
- **The walk is bounded**: 2,000,000 entries and 60 seconds, each a refusal with its
  own message. A link whose target is an unreachable network or removable path holds
  `canonicalize` until the operating system gives up; the bound is checked per entry,
  so the walk stops at the next entry after such a call returns.
- **A read root the walk refuses is not granted, and the command says so**
  (`SandboxedCommand::refused_reads`), instead of failing. The walk also runs on the
  toolchain read roots and the `PATH` folders inside the owner's profile, so a
  toolchain linked into the rustup home (`rustup toolchain link`) or a link in a
  cargo registry crate, both the owner's own, would otherwise stop every command on
  that machine. Nothing is written either way.

**Not closed, and in the backend's gaps text.**

- **Hard links are invisible to the check.** `is_symlink` is true only for a reparse
  point with the name-surrogate bit, and a file's other hard links share one security
  descriptor, so an inheritable entry written on a tree reaches every name of a
  hard-linked file in it, wherever that name is. pnpm and uv hard-link into a shared
  store. No check was added: it needs the link count of every file, one open each,
  which on a worktree with a built `target` folder costs more than the whole grant,
  and in a pnpm or uv tree nearly every file has more than one link, so a refusal
  would refuse every command of a legitimate project. The reach is a read or write
  entry for the sandbox user on store files the user is usually granted to read
  anyway; a write entry on the store is the case to watch.
- **A tree the walk cannot finish is refused**, so the first grant on a tree larger
  than the bound fails rather than running unchecked.
- **A grant removal is not checked.** Writing a folder's list again to remove a
  user's entries propagates the folder's remaining inheritable entries to what is
  below it, so where Windows follows a junction a removal reaches the junction's
  target as a grant would. Its callers, the slot table and `kgraf sandbox remove`,
  know the folder and nothing else, so no set of allowed targets can be built there,
  and an empty set would refuse every removal on a worktree that holds kGraf's own
  dependency links. A refused removal would leave one user's write grant on a folder
  another user now works in, which is worse than the propagation the check guards
  against, so the removal stays unchecked and the gap is recorded.
- Whether propagation follows a junction at all is still unmeasured, and every
  allowance above rests on that question.

Choices made with this amendment:

- (q) For a write grant the allowed set is the exact dependency trees, not the
  checkout root: a command that may write its own worktree could plant a junction to
  the checkout root there, and the checkout is the owner's repository.
- (r) A refused read root becomes a note rather than failing the command, which
  follows the Linux backend's ancestor reads. A refused write grant still fails.
- (s) The walk's bounds (2,000,000 entries, 60 seconds) end in a refusal, because
  there is no safe "allow" for a tree that could not be checked.

---

## Amendment (2026-10-08): the access-list lock, owners of files in a granted tree, and a removal that finishes

The defects below are fixed and compile-checked for Windows; none has run on Windows.

- **One lock around every access-list read-modify-write.** Every grant, deny and
  revoke read a folder's list, merged its entries into what it read, and wrote the
  result back, with no lock, and a tree walk sat inside that window. Four slot users
  granted on one toolchain folder at the same time each wrote a list holding its own
  entry alone, so the last writer took the other three grants away: random
  access-denied failures on the first parallel commands. A grant that held a list read
  before a revoke wrote the revoked entries back, and nothing would ever remove them,
  because the table no longer names that folder for that user. `slots::lock_acls` is
  one exclusive lock file in the slot folder, bounded (`ACL_LOCK_WAIT`, ten minutes,
  set by the longest legitimate holder: one list write on a large build tree), and
  reentrant within a thread, because a revoke inside a lease writes lists. Lock order:
  the access-list lock, then the table lock, never the other way round. The link walk
  stays outside the lock and the list is read again under it, immediately before the
  write.
- **An owner of a file in a granted tree.** A standard user owns every file it
  creates, and an owner may read and rewrite the access list of what it owns whatever
  that list says. A slot user that moved to another folder still owned the files it
  created in the old one, so from its new folder it could rewrite their lists and
  write and delete them, although the revoke took its entries away. A write grant on a
  worktree now carries an inheritable OWNER RIGHTS entry of READ_CONTROL alone, and
  kGraf's own user is named with READ_CONTROL and WRITE_DAC, the two rights that entry
  would otherwise take from kGraf, which owns the tree. Nothing gains a right it did
  not have. The alternative, resetting the owner of every object on each revoke, was
  rejected: an owner is not inherited, so it would mean a walk of the whole tree,
  object by object, on the slot-move path. The token sandbox keeps the entries it had:
  every command there runs as kGraf's own user.
- **A root recorded before it was judged wedged its slot.** The lease recorded every
  root in the table before anything judged it, so a root that then failed (a junction,
  a volume without access lists, a folder whose list this user cannot write) stayed
  there, and the revoke of that path, which can never succeed, aborted every later
  lease of that slot. The sandbox-user path now judges every root before it takes a
  slot, and a lease whose candidate cannot lose its old grants lets that candidate go,
  with its table entry as it was, and tries the next; the failure is returned only
  when no candidate works. A candidate is still taken only when every one of its
  revokes succeeded.
- **`kgraf sandbox remove` finishes.** It stopped at the first failed revoke, keeping
  the table and the list, so an unready drive, a junction, a folder whose list this
  user cannot write or one damaged table line left the users, the firewall rules and
  the DNS filter on the machine for good. Every folder is now tried, what kept its
  entries is reported line by line, and the users are deleted whatever is left: an
  entry for a deleted user's SID grants nothing to anyone. A damaged table is read as
  far as it goes.
- **A worker slot follows its tool's folder.** The shell tool kept the slot of the
  first folder it ran in until the tool was dropped. Where its environment outlives
  one attempt (a route's environment), that slot never went back to the pool, and a
  command in another folder was handed a slot held for a folder it does not write. The
  held slot is keyed by the folder now, and a tool used for another folder drops the
  old slot first.
- **The table is written durably.** The temporary file is flushed with `sync_all`
  before the rename (a table of zero length reads as a setup that holds no folder, so
  the grants it listed would never be revoked), the rename is tried a few times, and
  the error of a table that cannot be read names the two commands that recover it.
- **Smaller ones.** A folder whose path the table cannot give back unchanged, or whose
  name holds a line break, is refused instead of written in another spelling that no
  revoke would find. A lock file whose folder went away is opened again, with the
  folder made again between tries. The refusal when every slot is held names the folder
  each slot was last taken for.

Two items are left as they are:

- A worker with no wait can still be refused while its own slot is being released, in
  the window between a lease's last handle dropping and its lock file closing. The fix
  is one retry of the candidate loop inside the lease.
- Two processes can each hold a slot of the same folder, which uses the pool up
  faster, and two users then write one folder. Sharing a slot between processes needs
  a shared lock mode and a count of holders in the lock file.

---

## Amendment (2026-10-08): where the access-list lock is held, and a slot whose revokes fail

Two of the fixes above meet in `Leases::lease`. One holds the slot table's lock and the
in-process lock only to choose a slot, bind it and write the table, and runs the revokes
with no lock held, because a revoke rewrites the access lists of a whole tree and costs
seconds to minutes. The other wraps the revoke loop in the access-list lock and lets a
candidate whose revoke fails keep what it held, so the next candidate can be tried. The
merged function keeps both, as follows.

- The lease takes the access-list lock at no point. Each write of a list takes it for
  itself, a revoke included (`revoke_sandbox_grant`), so every read-modify-write of a
  list is covered, and a tree walk still runs outside the lock. One take around the
  whole revoke loop would hold a lock bounded at ten minutes for minutes of tree
  writing, so a slow revoke of one folder would block every other folder's lease; and
  it would protect nothing more, because the hazard is one list, read and written by
  two writers.
- The lock order stands: the access-list lock, then the table lock, never the other way
  round. `lease` holds neither across a revoke and never holds both: it releases the
  table lock before the revokes run, and the lock a revoke takes is released before the
  table lock is taken again to drop the revoked folders.
- One candidate is tried per pass. The slot is chosen, bound and the table written under
  the table lock, with the folders the slot is losing listed ahead of the new ones; both
  locks go; the revokes run. A revoke that fails ends the pass: the slot's lock file is
  released, its table entry is put back as it was, in the place it had in the order, and
  the next candidate is tried. The first failure is returned only when no candidate
  works.
- The folders a slot is losing stay listed until their revokes are done, so a holder
  killed between the write and the revokes leaves them to the next holder of that
  folder. A folder already revoked when a later one fails stays listed as well, which
  costs one revoke that finds nothing, rather than leaving a grant no revoke would find.
- `apply_inheritable` reads the list under the access-list lock and writes it there, so
  no caller hands it a list read earlier. The link walk before the write and the walk
  after it stay outside the lock, because each reads a whole tree while the lock's bound
  is set by one list write.
- The test `a_lease_holds_the_access_list_lock_while_it_revokes` asserted the structure
  that went. It is replaced by
  `each_revoke_of_a_lease_holds_the_access_list_lock_and_the_lease_holds_it_at_no_point`,
  which pins what survives: the lock is free to another holder while a revoke runs, the
  revoke's own write of a list is covered by it, and it is reentrant within a thread.

---

## Amendment (2026-10-08): the undo record's folder and the deny entries

The changes below harden the folder the undo record lives in and the deny entries on the
closed folders. Each bullet is one behavior change. None of it has run on Windows.

- **The deny entries are written with `SetSecurityInfo`, on a handle opened with
  `MAXIMUM_ALLOWED`.** (Superseded: system folders refuse that handle; see the amendment
  of 2026-10-09.) They were written with `SetKernelObjectSecurity`, which Microsoft
  documents must not be used on a file system object: "This function should not be used
  when setting a security descriptor on file system objects. Instead, use the
  SetSecurityInfo or SetNamedSecurityInfo functions." The reason the call was picked is
  real, because `SetSecurityInfo` normally walks the children, which on a drive root is a
  walk of the whole drive, but the documented switch for that is the handle's access mask:
  "If the supplied handle was opened with an ACCESS_MASK value of MAXIMUM_ALLOWED, then
  the SetSecurityInfo function will not propagate ACEs to children." The old code also
  built its own descriptor and copied the folder's `SE_DACL_PROTECTED` and
  `SE_DACL_AUTO_INHERITED` bits into it, and relied on an undocumented path honoring
  them; had Windows dropped the protected bit, `C:\ProgramData` would have become open to
  inheritance from `C:\`, and the drive root's inheritable `Authenticated Users` entry
  would have reached ProgramData and everything below it at the next write on `C:\`. The
  write now passes `DACL_SECURITY_INFORMATION` alone, with neither
  `PROTECTED_DACL_SECURITY_INFORMATION` nor `UNPROTECTED_DACL_SECURITY_INFORMATION`, so
  the folder keeps the protection Windows had on it, and the hand-built descriptor is
  gone.
- **Taking a deny entry off is read back, as writing one was.** `close_folder` read
  the folder's list again and refused when the entry it had just written was absent, while
  `reopen_folder` returned on a bare success from the write, and `remove` then deleted the
  record line. A write that silently kept the old list would have left the deny on the
  folder with nothing left to say so. `reopen_folder` now reads the list again and fails
  while the entry is still there, so the record line stays and `remove` can run again.
- **The record folder's check opens each part once.** `existing_trusted_folder`
  opened a part to find out whether it exists and then dropped the handle, and the check
  that followed opened it again, so the path was resolved twice. The check now runs on the
  handle the first open returned.
- **A link above a folder being closed is refused.** The no-follow open covers the
  last component alone, so a link in a component above it was followed and the access list
  of its target would have been written. The write path now checks every component above
  the folder and refuses a link there, the way the record folder's check walks part by
  part. The check and the open are two steps, so this is not atomic; the atomic form needs
  an `NtCreateFile` open with `OBJ_DONT_REPARSE`, which this code does not have, and the
  components above a closed folder are writable by administrators alone.
- **The undo record's folder is checked for its volume and its shape.** The closed
  folders are picked only from fixed local drives whose file system keeps access lists, and
  the record's folder was checked by neither rule, although its access list is the whole
  reason it is trusted. The elevated step now refuses a record folder whose drive is
  removable, remote or without persistent access lists, and one whose text is not a plain
  path on a local drive: a component carrying a colon names an alternate data stream, and a
  relative component could leave the drive.
- **A `closed` record line carries the entry's own mask and flags, and removal
  matches them exactly.** The line named the folder and the group's SID alone, and removal
  looked for an entry whose mask covered today's `CLOSED_FOLDER_DENY`, a subset test. A
  later version that denied more rights would then have orphaned every entry an earlier
  version wrote, while deleting its record line, and a version that denied fewer would have
  taken off a wider entry than it wrote. The line is now
  `closed <sid> <mask> <flags> <path>`, removal matches the mask and the flags exactly, and
  a line whose mask carries a right outside the write rights, or whose flags carry any
  inheritance bit, is refused when the record is read.
- **A `closed` record line's folder and SID are both bounded.** The path went
  straight from the record to the call that rewrites a folder's access list, and the SID
  test accepted any SID starting with `S-1-5-21-` once the group was gone, which every
  domain SID does too, so `Domain Users` passed. Writing the record needs administrator
  rights, so this was the last fence and not the first, but an edited line could have made
  `remove` strip an explicit deny entry for another principal from another folder. Removal
  now takes an entry off only on a folder `closed_folders` names on this machine, and only
  when the SID is the sandbox group's own, or an account of the user's own authority, not
  the user itself, that no longer resolves, which is the shape of a sandbox group an
  earlier setup made and deleted. A SID that still resolves, or one whose lookup failed, is
  refused.
- **A deny entry kGraf cannot judge no longer blocks every removal, and a stale
  generation is cleaned up.** `remove` refused the whole plan on the first `closed` record
  line whose SID was not the current group's, and built no step at all: not the users, the
  group, the logon rights, the firewall rules, the WFP filter, nor twenty deny entries. The
  record reaches that state without anyone editing it. A group deleted outside kGraf (a
  cleanup script, a removal cut short, an image rebuild) leaves ten lines whose SID no
  longer resolves, and no relative identifier is reused, so the next setup makes a group
  with a new SID and adds ten more lines. From then on every `remove` refused everything,
  and the user's only way out was `icacls` by hand. Two changes: setup takes the entries
  of a group of the user's own authority that no longer resolves off the folders and drops
  their lines before it writes its own, so a generation does not pile up; and a `closed`
  entry this removal may not undo is now one kept step with its reason, sent to the user's
  terminal, while every other step runs and the entry stays in the record. The setup pipe
  carries a new non-fatal `note` message for that reason.
- **Every folder is tried, and the failures are reported together.** The step stopped
  at the first folder it could not close, and the setup failed there, after the accounts,
  the logon rights and the firewall rules already existed. One machine can hold several
  such folders at once (`Tasks` or `tracing` under the Windows folder where Administrators
  hold less than full control, a drive reported fixed that cannot be opened), so the user
  learned of the next one only on the next run. Each folder is now tried, the others are
  closed and recorded, and the failure names every folder with the principal that owns it
  and the reason, so one run tells the user everything to fix. A junction planted at one
  of these names is also no longer listed as a folder to close: the existence test does not
  follow a link any more.
- **The record's folders are held open from the check to the last write, and the
  folder they are made under is judged.** The check dropped its handle and every write
  after it resolved the path by name, with `FILE_SHARE_DELETE` granted, which lets another
  process delete or rename a folder while it is open. The elevated step now keeps a handle
  on `%ProgramData%` and on each folder below it until the step ends, opened without
  `FILE_SHARE_DELETE` and with `FILE_LIST_DIRECTORY | FILE_TRAVERSE`, which take part in
  sharing checks, so none of them can be renamed away between the check and the writes.
  The base is judged as well, by its own rules: it must be a folder, not a link, and an
  administrator principal must own it. The writer test is deliberately not applied to the
  base, because every local user may create a folder directly under ProgramData by default,
  which is the premise of the whole scheme; what carries the safety is the folder below it,
  which is made with a protected list of its own, owner Administrators, and is checked by
  that test. Handle-relative opens for the record file itself were weighed and not taken:
  they need the `NtCreateFile` path this code does not have, and the file is written to a
  new name and renamed over the old one inside a folder no other principal may write.
- **`kgraf sandbox status` reads the record through the checked folder.** It built the
  path by name and read whatever was there, so a junction in place of the record's folder
  could feed the user a record of someone else's choosing, including one that makes a
  removal look complete. Status now uses the same folder judgment the elevated step uses,
  and reports a folder that does not pass as a record that cannot be trusted, with the
  reason. Status still needs no administrator rights: the folder grants Users read.
- **The sandbox-user path verifies effective write, and refuses a writable root that
  setup closed.** It wrote the grants and stopped there, while the token backend checks
  effective write with `AccessCheck` after every grant. A grant that reads back proves only
  that the allow entry is present, and a deny entry is evaluated first, so a writable root
  equal to ProgramData, Public, a child of Public, a drive root or one of the three Windows
  folders passed the read-back and then failed every write at runtime with no message
  naming the cause. Two changes: such a root is refused before any grant, with an error
  naming the folder and the deny; and after every grant and deny of the command is written,
  the sandbox user's own write access on each root is checked with `AccessCheck` under a
  token from its own logon. The existing probe could not serve, because it builds a
  restricted token from the user's own, in which a deny entry for the sandbox group is not
  evaluated at all; the cost is one more logon per command, beside the one the runner
  already performs.

---

**Amendment (2026-10-08): kGraf propagates its own access entries, and a hard link
never carries one out of a tree.** This closes the hard-link gap the amendment
above recorded as open ("Hard links are invisible to the check"). The sentence in that
amendment that says "No check was added" no longer holds; the earlier text stays as it
was written, and this amendment is the correction. None of it has run on Windows.

- **The kernel's propagation is suppressed, and kGraf writes each existing child
  itself.** Writing a folder's access list makes Windows walk the tree and write every
  object already below it, with rules kGraf cannot steer: it may enter a junction, and
  it writes the one security descriptor a hard-linked file shares with every other name
  of that file, which can lie outside the tree (decision 0083). kGraf now writes the
  folder's own list through a handle opened with `MAXIMUM_ALLOWED`, which Microsoft's
  `SetSecurityInfo` page documents as not propagating entries to children ("If the
  supplied handle was opened with an ACCESS_MASK value of MAXIMUM_ALLOWED, then the
  SetSecurityInfo function will not propagate ACEs to children"; upstream openai/codex
  `windows-sandbox-rs` opens its handles the same way), with
  `FILE_FLAG_BACKUP_SEMANTICS` and `FILE_FLAG_OPEN_REPARSE_POINT` and neither
  protection flag, so the object keeps the inheritance protection Windows set. kGraf
  then walks the tree once: one open per child, which reports the reparse state and
  `nNumberOfLinks` in the same call, then the write through that same handle. The
  kernel's own propagation opens each child too, so this replaces a cost instead of
  adding one; a `MEASURE` line in the self-test reports the wall time of a first grant
  on a tree of 100 folders and 2,000 files, and the Windows testing guide says what
  to read from it.
- **What holds the tree boundary.** A reparse point is not entered and not written, and
  it is judged against the allowed-target set by the same walk as before. A file with
  more than one hard link is never written: it keeps the list it has, which is the right
  outcome for a dependency store file, because the command needs to read it and the read
  grant on the store is what makes the dependency usable. Each such file is recorded and
  named in a note that goes out with the command's output. **A command therefore cannot
  write a hard-linked file in its own tree**: a package manager that writes such a file
  in place fails with an access error, while replacing it works, because the new file is
  a new object that inherits the folder's entry.
- **The entry a child gets is a plain explicit entry, not the inherited copy Windows
  would derive**. Reproducing the derivation (`INHERITED_ACE`, `INHERIT_ONLY_ACE`,
  `NO_PROPAGATE_INHERIT_ACE`, the container and object mapping, the canonical entry
  order) cannot be judged correct without a Windows machine, and a child that ends
  narrower than Windows would have made it is a failed build. A child therefore gets the
  tree mask: a folder with the same inheritable flags the root carries, so everything
  made in it later still gets its entry from Windows, and a file the effective entry.
  The entry grants nothing beyond the tree, because the whole tree is granted to that
  user already, and the two rules above are what keep it inside the tree. Two
  consequences: `icacls` shows these entries as explicit on every file and folder, not
  as inherited copies, so the output differs from the restricted-token backend's; and a
  later write of the folder's list does not remove them, so a removal takes them off
  itself.
- **The grant pass does not enter what kGraf denies.** The top-level `.git` and the
  policy's hidden folders are skipped and not entered, so no generous entry lands where
  a deny must hold. The deny entries are written after the grants, and
  `SetEntriesInAclW` keeps a deny entry before an allow one, so a deny wins on a child
  that holds both. A deny or the Low label, which are inheritable writes as well, go
  through the same pass and hold nothing back from it.
- **A child that cannot be opened, written or listed is recorded, and the pass goes
  on.** Microsoft's same page states that "if share access to the children of the object
  is not available, this function will not propagate unprotected ACEs to the children",
  so Windows skips such a child as well; failing here would refuse a grant that worked
  before. The note names the first few with their reasons and counts the rest.
- **The link walk in front of every write stays, as the second line of defense.** With
  the propagation suppressed, a junction out of the tree is no longer reached by the
  write at all. The walk holds where the suppression does not: it rests on one
  documented sentence that no Windows run has measured. A `MEASURE` line now reports
  whether the suppression worked, beside the older one that reports whether Windows'
  own propagation follows a junction.
- Tests: the per-child decision and the walk are pure functions over data and run on
  every host (a hard-linked file, a folder whatever its link count, a junction, a denied
  folder and a file inside it, a child that cannot be opened, a root that cannot be
  listed). The Windows calls have tests that compile only here: a hard-linked file that
  keeps the owner's store file's list while a plain file beside it gets the entry, the
  denied `.git`, and the two `MEASURE` lines.

**Amendment (2026-10-08): a grant removal propagates nothing and still refuses
nothing.** This closes the removal gap the amendment above recorded as open ("A grant
removal is not checked"). The sentence there that says "the removal stays unchecked and the gap is recorded" no longer
holds; the earlier text stays as it was written, and this amendment is the correction.
It has not run on Windows.

- **The exposure.** Writing a folder's list again to take a user's entries off made
  Windows propagate the folder's remaining inheritable entries to everything below it,
  so a removal carried the same risk as a grant: an entry through a junction, or onto
  the one descriptor a hard-linked file shares with a name outside the tree. The
  amendment above declined to guard it, because its callers (the slot table and
  `kgraf sandbox remove`) know only the folder, so they can build no set of allowed
  link targets, and refusing a removal is worse than the exposure: it would leave one
  user's write grant on a folder another user now works in.
- **The fix needs no guard and no refusal.** The folder's own list is written with the
  propagation suppressed, as a grant is now, and the entries are taken off each child
  by the same pass the grant uses. The pass only ever takes access away, so nothing of
  kGraf's can land outside the tree, whatever the tree holds: it enters no reparse
  point, writes no hard-linked file, and records each child it skipped or could not
  write. A removal is therefore never refused, and no link walk runs in front of it.
- **The pass has to run now.** A child carries kGraf's entry as an explicit entry of
  its own, not as an inherited copy, so rewriting the folder's list no longer takes it
  off by itself. A Windows test checks exactly that: it grants a tree, reads the entry
  back on a child, removes the grant and checks that no entry is left on the folder, the
  subfolder or the file.
- **What a removal leaves, and it is in the backend's gaps text.** A hard-linked file is
  skipped, as during a grant. After the grant pass of this version such a file never
  holds an entry of kGraf's, so there is nothing to take off; a tree granted by an
  earlier kGraf, whose kernel propagation did reach it, can still hold one, and it is
  taken off with `icacls` on the file itself. A child that cannot be opened keeps its
  entry too, and is named in the note.
- **The note has an output path.** A slot lease that moves a slot to a new folder
  returns what its removals left, and the command that took the slot carries it with its
  output. `kgraf sandbox remove` drops the note, because the users themselves go right
  after it, so an entry a file kept names a user that no longer exists.

---

## Amendment (2026-10-09): one entry set at the root and on every existing child

Two of the fixes above meet on the tree grant. One gave a write grant an inheritable
`OWNER RIGHTS: READ_CONTROL` entry, with kGraf's own user named for the two rights that
entry would otherwise take from it, so a sandbox user that owns a file it created cannot
rewrite that file's access list after a revoke took its entries away. The other stops
relying on the kernel's propagation: the root's list is written through a handle opened
with `MAXIMUM_ALLOWED`, which propagates nothing to the children that already exist, and
kGraf walks the tree itself so that no entry reaches a reparse point's target or a file
that has a hard link outside the tree.

Written separately, the second undoes the first for every file that already exists: the
root would carry the owner-rights restriction while the files in the worktree kept the
sandbox user as their owner, with an owner's implicit right to rewrite their own lists.
The resolution:

- The entry set is derived once, in `acl::tree_grant`, and used in both places. The
  per-child set is `acl::child_grant` of that same list: every entry that applies below
  the root is written on the child with its own trustee, effective on a file, effective
  and inheritable on a folder. An entry that applies to the root alone is not a child's.
  So the owner-rights restriction and kGraf's compensating entry reach a file that
  existed before the grant.
- The two rules that hold the tree boundary are unchanged and are what keeps those
  entries inside it: a reparse point is neither entered nor written, and a file with more
  than one hard link is never written and is recorded.
- Proved on any host, over the pure per-child functions, by
  `a_file_that_existed_before_the_grant_carries_the_owner_rights_restriction`: a
  pre-existing file is a child the pass writes; its set names the sandbox user with the
  tree mask, `OWNER RIGHTS` with `READ_CONTROL` alone and kGraf with
  `READ_CONTROL | WRITE_DAC`; nothing on a file is inheritable; a folder of the tree
  carries the same three trustees, effective and inherited; and a hard-linked file and a
  link are skipped, so neither entry is ever written on them. With the per-child set
  reduced to the sandbox user's entry alone, the test fails on the owner-rights entry.
- The two denies and the Low label go through the same pass. Each writes one entry for
  one trustee, so there is nothing to derive for them; what they needed was the pass
  itself, which they now have, because each was an inheritable write whose propagation is
  suppressed as the grant's is.
- The access-list lock is held for each list written: the root's own read-modify-write,
  and each child's inside the pass (`write_child_entries`). It is not held across the
  pass or across a walk, which would hold a lock bounded at ten minutes for as long as a
  walk of a large tree takes; the hazard it exists for is one list, read and written by
  two writers, and the lock is reentrant within a thread, so a nested take costs a
  counter.
- One sentence of the backend's gaps text no longer held and was rewritten: a link into a
  folder the same grant admits no longer carries a write entry onto that folder, because
  the pass does not enter a reparse point and the kernel propagates nothing. What the
  command reaches through such a link is what the grant on that folder gives it.

## Amendment (2026-10-09): what the first Windows run of setup and remove found

A hosted Windows Server 2025 run of `kgraf sandbox setup --slots 4` and
`kgraf sandbox remove` ran both for the first time. Setup
stopped at the deny entries, and the removal stopped part-way. Each bullet is one behavior
change. Host tests prove the protocol and plan changes; the Windows tests named below run
on the next Windows run.

- **The deny entries are written through a handle no other open can refuse.** Setup
  failed on `C:\`, `C:\ProgramData` and `C:\Windows\Tasks` with `ERROR_SHARING_VIOLATION`
  (os error 32). The share mode was already full. The cause was the access: a handle opened
  with `MAXIMUM_ALLOWED` on a folder that administrators control fully is given every
  right, `DELETE` and the data rights among them, and those rights take part in sharing
  checks. Windows and its services hold those three folders open without
  `FILE_SHARE_DELETE` (a change notification or a service's own handle; a current folder
  was named here too, and a later run measured that a current folder alone does not do it,
  see the amendment on that run), and the elevated step held
  ProgramData itself the same way while it ran. The handle now asks for
  `READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES`, none of which takes part in a sharing
  check, with full sharing; the link check runs on the same handle as before. That handle is
  not `MAXIMUM_ALLOWED`, so `SetSecurityInfo` on it would propagate, which on `C:\` is a
  walk of the whole drive. The write is therefore `SetKernelObjectSecurity` again, with a
  descriptor that carries the folder's own `SE_DACL_PROTECTED` and `SE_DACL_AUTO_INHERITED`
  bits and the whole list as it was read, inherited entries included, without
  `SE_DACL_AUTO_INHERIT_REQ`. The risk recorded above for this call (a dropped
  protected bit opening ProgramData to inheritance from `C:\`) is now checked, not assumed:
  the list and the two bits are read back after every write, and on any difference the old
  list and bits are written back, read back again, and the step fails with what it found.
  The write needs none of the inheritance work the documentation warns about, because the
  one entry it adds or takes off carries no inheritance flag. Windows tests:
  `a_folder_held_open_without_delete_sharing_is_closed_and_opened_again` (a folder held by
  a handle without delete sharing, as the elevated step holds the record's folders),
  `a_protected_folder_keeps_its_protection_and_its_list`, and the existing
  `closing_a_folder_changes_no_child_and_keeps_the_protection`.
- **ProgramData is checked, not held.** The record's base folder is opened for its check
  with full sharing and only access that takes no part in sharing checks. Only an
  administrator may rename ProgramData, so holding it bought nothing, and it took part in
  the sharing check of every later open of ProgramData. The `kGraf` and `sandbox` folders
  below it, which setup made, are still held from the check to the last write.
- **A removal key carries any path.** The removal stopped with "a setup pipe message is
  not understood" at its first deny entry: the key `remove-closed:C:\Users\Public` holds a
  backslash, which the owner's parser refused, and a path with a space would have split the
  line. Keys are now written with every byte outside letters, digits and `-_:.` as `%XX`
  and read back to the same text, so a key from an older elevated step, which never held
  such a byte, reads the same. A key longer than 1,024 bytes is cut at a character. The
  host test `every_message_the_elevated_step_sends_reaches_the_owner_intact` sends every
  setup step key, every removal key of a record with an entry of every kind (deny entries
  on paths with backslashes, spaces, non-ASCII text, `%` and control characters), every
  outcome, notes, a failure per key and a password per role through the owner's receiver.
- **The owner reads past a line it cannot parse.** After the greeting, a line the owner's
  side cannot parse is named (its kind, and the key for a step, note or failure; a password
  line by its kind alone) and read past, so the elevated step goes on removing; the command
  fails at the end and lists every such line. Before the greeting nothing is read past.
- **A failed removal step no longer ends the removal.** Its entry stays in the record, the
  owner is told why, every later step runs, and the removal fails at the end naming the
  steps. No later step needs an earlier one: a deny entry left behind a deleted group names
  a SID that resolves to nothing, which the next removal may take off.
- **A removal reads every slot user.** The owner's process asked for the removal with
  every slot, but `--slots` was passed to the elevated step for a setup alone, so the
  elevated removal read the two fixed users only. Each slot user then looked absent, and
  the plan marked its record lines as already gone and dropped them: the run reported every
  slot user's sign-in screen entry as already gone while it was still there, and the users
  themselves would have followed. The elevated removal now reads all 99 slots whatever it is
  told, and the plan refuses a record that names a user it was not asked to read
  (`PlanError::NotRead`) instead of dropping the line. Host test:
  `a_removal_that_did_not_read_a_slot_user_refuses_instead_of_dropping_it`.
- **A planted junction at the record's temporary name is deleted, not a stop.** Writing the
  record deleted the `.tmp` name only when the create reported that it exists. A create over
  a folder, a junction among them, is refused with `ERROR_ACCESS_DENIED` instead, so every
  record write failed. The name is now deleted without being followed before every create,
  and the create stays `CREATE_NEW`. Windows test:
  `a_planted_temporary_name_is_replaced_and_its_target_kept`.
- **Measured: a file written by the grant pass also takes in its parent's entry.** After
  the pass wrote the grant on a folder and then on a file in it, the file held two entries
  for the sandbox user, not one: the pass's explicit entry and one inherited from the
  folder. `SetSecurityInfo` on a list that is not protected takes in the parent's
  inheritable entries. Both allow, and a deny written later still stands before both; the
  test `a_deny_on_a_child_stands_before_the_grant_pass_entry` asserted exactly one entry and
  now asserts one explicit entry and the deny before every allow. Open, to measure: a file
  that holds only an inherited entry is not rewritten by a revoke (`names_sid` counts
  explicit entries), so whether its inherited entry goes when the folder's does is not
  known.
- **The setup test checks that nothing is left.** `setup_status_again_remove_leave_nothing_behind`
  accepted any failing status after the removal, so it passed with every slot user still on
  the machine. It now fails on any `present` line other than the machine's firewall facts.


## Amendment (2026-10-09): a current folder in a granted tree no longer stops a grant or a revoke

Correction (2026-10-09): this amendment's premise, that Windows holds every process's current
folder open without `FILE_SHARE_DELETE`, is wrong. A later Windows run measured that a folder
`ping.exe` has as its current folder still opens with `MAXIMUM_ALLOWED`. Error 32 needs a
handle held without `FILE_SHARE_DELETE`, such as the ones Windows and its services held on
`C:\` and `C:\Windows\Tasks`. The behavior changes below stand; their tests now create the
condition with such a handle (the amendment on that run).

The worktree grant, deny and removal passes opened every folder and file with
`MAXIMUM_ALLOWED`, because a handle opened that way is the one documented switch that keeps
`SetSecurityInfo` from propagating (the amendment on kGraf's own propagation pass above). The
same open is given `DELETE` on an object kGraf controls, and Windows holds every process's
current folder open without `FILE_SHARE_DELETE`, so the open failed with
`ERROR_SHARING_VIOLATION` (os error 32) whenever a worker, a build or a test had its current
folder in the tree. A revoke during `kgraf sandbox remove`, or when a slot moved to another
folder, then failed for the whole folder. The first Windows run of setup found the same
cause in setup's shared folders (the amendment above); this is the worktree side of it. Each bullet is one behavior
change.

- **The `MAXIMUM_ALLOWED` open stays first; a sharing violation opens the object again.**
  The second handle holds `READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES` with full
  sharing, none of which takes part in a sharing check, and is opened by the shared folders'
  own opener (`secure::open_for_lone_write`, no link followed). Only error 32 takes the
  second open; every other error stands as before. The common case keeps the open whose
  behavior Windows runs have measured. The alternative, the minimal handle for every write,
  was rejected: `SetSecurityInfo` on a child also takes the parent's inheritable entries into
  the child's own list (measured on Windows), and a removal written through
  `SetSecurityInfo` relies on that to drop a child's inherited copies of a revoked entry.
  Giving that up for every child would change every grant and removal to fix the rare held
  object.
- **Through the second handle the list is written alone, with the shared folders' helper.**
  `SetSecurityInfo` through a handle that is not `MAXIMUM_ALLOWED` propagates (Microsoft,
  `SetSecurityInfo`: "the system automatically propagates any inheritable access control
  entries (ACEs) to existing child objects"). The write is therefore `SetKernelObjectSecurity`,
  with the object's own protected and auto-inherited bits, then a read-back and a roll-back on
  any difference, by the same function setup uses (`secure::write_dacl_alone`, which wraps the
  existing `set_folder_dacl`; no second copy). Documentation evidence on propagation:
  Microsoft's "Automatic Propagation of Inheritable ACEs" names `SetNamedSecurityInfo` and
  `SetSecurityInfo` as the functions that propagate, and the `SetKernelObjectSecurity` page says
  it "should not be used when setting a security descriptor on file system objects" and points
  to those two. Neither page says in so many words that `SetKernelObjectSecurity` propagates
  nothing; that is measured by a Windows test, not assumed. kGraf's own pass stays the only
  writer of the children.
- **A removal written alone drops the inherited copies itself.** The lone write takes
  nothing in from the parent, so a held child would keep an inherited copy of the entry its
  parent just lost, and the slot user its access there. Through the second handle a removal
  also writes a child that holds only an inherited allow entry for a removed SID, and drops
  every such inherited allow entry. The pass writes a folder before it enters it, so the
  copy's source is gone by then; a copy whose source lies above the removed tree is dropped
  too, which only narrows the removed user's access, as the removal already does for that
  user's explicit entries. Through `MAXIMUM_ALLOWED` nothing changes: a child that holds only
  inherited entries is still skipped (the open measurement of the amendment above, now in
  `removal_writes`, which replaces `names_sid`). Host tests: `list_write_tests` (the open
  decision, which children a removal writes, which entries it keeps).
- **The Low label is not written alone.** The second handle holds no `WRITE_OWNER`, which a
  label write needs, and the helper writes access lists only. A held object fails the label
  write with os error 32, as it did before, and the pass records it.
- **`kgraf sandbox remove` fails at the end when a folder kept its grant.** Every folder was
  already tried and each failure named; the command then printed the list and succeeded. It
  now fails at the end with the list (`plan::removal_outcome`), and a failed elevated step
  names those folders too. Host test:
  `a_removal_that_left_a_folder_fails_at_the_end_naming_it`.

What the next Windows run must show, in `1-sandbox-lib.txt`:

- `a_lone_write_on_a_held_folder_changes_no_child` passes: the second open is taken, and an
  inheritable entry written on the held folder reaches neither the file nor the folder below
  it. A failure here means `SetKernelObjectSecurity` propagated, and the second handle must
  not be used.
- `a_grant_and_a_revoke_succeed_with_current_folders_in_the_tree` (since renamed
  `a_grant_and_a_revoke_succeed_with_folders_held_without_delete_sharing`) passes: a grant
  and a revoke with the root and a folder below it held, no child
  blocked, the held folder holding the pass's one entry and no inherited copy, and no object
  naming the user after the revoke.
- `a_revoke_through_a_lone_write_drops_the_inherited_copy` passes.
- Each of the three first checks that the `MAXIMUM_ALLOWED` open is refused with os error 32
  at the held folder; a panic that says "the condition under test is absent" voids its
  result.
- `MEASURE maximum-allowed-suppresses-propagation: suppressed true` still holds, and the
  existing propagation tests still pass.

## Amendment (2026-10-09): what the second Windows run of setup found

A hosted Windows Server 2025 run (Datacenter 24H2, build 26100.33438) ran setup again. Nothing
below has run on Windows since; each bullet says what is proved on the host and what stays
unproved. Each bullet is one behavior change.

- **A DNS filter Windows refuses no longer stops setup.** `kgraf sandbox setup --slots 4`
  failed at step `dns-filter`: `FwpmFilterAdd0` refused the filter on
  `FWPM_LAYER_NAME_RESOLUTION_CACHE_V4` with 0x80320028 (`FWP_E_OUT_OF_BOUNDS`).
  openai/codex leaves these filters out for the same refusal. Because setup stopped there, no password was stored, so selfcheck ran on the
  token sandbox and the sandbox user phase stopped at its gate: the DNS step cost the whole
  phase. The gap is now kept and reported as a failed line, never a pass. Setup now adds the provider and the sublayer in one transaction (which also deletes
  every earlier copy of the filters) and each filter in a transaction of its own. A filter
  Windows refuses is sent to the owner's process as a `gap` line with its name, key and
  Windows' status, the step's outcome is `partial`, the undo record keeps a `dns-refused`
  line per refused filter with the reason, and setup stores the passwords and ends with a
  notice that DNS is not confined on this machine (`plan::dns_step_report`,
  `plan::dns_notice`). A failure of the provider or sublayer transaction marks every filter
  not added, the same way. `kgraf sandbox status` shows a filter the record lists as refused
  as `FAILED` with the reason, not `MISSING`, and when the refused filters are the only gap
  it ends with "DNS is not confined on this machine" instead of asking for setup again, and
  exits non-zero. Selfcheck is unchanged: its `dns:` line still judges the effect and can
  only say `not confined` or `not proved` on such a machine. The gaps text keeps DNS and now
  names the refusal. Removal needed no change of plan: every WFP key stays in the undo list,
  and a key Windows never added is removed as already gone; a host test pins a partial set,
  and a removal clears the `dns-refused` lines. The self-test parser reads `FAILED` as a
  problem. Host tests: `a_refused_dns_filter_makes_the_step_partial_not_failed`,
  `status_shows_a_refused_dns_filter_as_failed`, `remove_takes_off_a_partial_dns_filter_set`,
  `refused_dns_filters_round_trip_and_stay_on_one_line`, `a_failed_status_line_is_a_problem`,
  `a_refusal_is_read_from_the_two_probes`.
- **What the RPC filter did.** It was added first, and `FwpmFilterAdd0` returned success for
  it inside the transaction; the IPv4 refusal then aborted the transaction, which rolled the
  RPC filter back, so status listed it `MISSING`. With a transaction per filter it is kept
  when it adds without an error. It covers remote callers of the DNS Client interface only:
  `FWPM_LAYER_RPC_UM` is not reached by a local ALPC call, so it confines nothing on this
  machine, and status says so on its line.
- **What the refusal says about its cause, and what stays open.** The RPC filter took the
  same security descriptor through a condition of the same data type
  (`FWP_SECURITY_DESCRIPTOR_TYPE`, `FWPM_CONDITION_REMOTE_USER_TOKEN`) in the same sublayer,
  so neither the descriptor's bytes nor the sublayer weight explains it; every filter leaves
  its own weight to the engine (`FWP_EMPTY`). The engine has distinct statuses for a
  condition the layer does not have (`FWP_E_CONDITION_NOT_FOUND`), a value of the wrong type
  (`FWP_E_TYPE_MISMATCH`), an action (`FWP_E_INVALID_ACTION_TYPE`) and a weight
  (`FWP_E_INVALID_WEIGHT`), and returned none of them; codex reports the refusal for
  "ordinary static filter shapes" at these layers, which points at the layer rather than at
  kGraf's condition. That is an inference, not a measurement. So the next run measures it:
  when a name resolution cache layer refuses kGraf's filter, setup adds two probe filters at
  that layer, each a non-persistent permit in a transaction it always aborts (one with no
  condition, one with kGraf's `FWPM_CONDITION_ALE_USER_ID` condition), and the refusal's
  reason carries what they show (`wfp::probe_reading`): the layer refuses any static filter
  of this shape, or the user condition is the cause, or the block action and its flags are.
  Options for a later Windows run, none shipped as working: if the user condition is the
  cause, `FWPM_CONDITION_ALE_APP_ID`, `FWPM_CONDITION_IP_REMOTE_ADDRESS` and
  `FWPM_CONDITION_PEER_NAME` are the other conditions Microsoft lists at these layers, and
  none names a user, so none alone fits a per-user filter; if the layer refuses every static filter, the remaining routes are a
  callout driver (out of scope for kGraf), the RPC filter on `ncalrpc` (unproved, because
  the RPC layer is documented for remote calls), or the gap kept as now.
- **The hosted run's stack.** `kgraf-host`
  `tests::shortfall::auto_runs_a_script_of_allowed_programs_and_names_the_one_it_does_not_allow`
  overflowed its stack on Windows. Measured on the host (debug, macOS, a probe allocator
  that records the deepest stack seen on the test thread): the hosted run path took
  1,829,360 bytes before this change and 1,804,736 bytes at the previous Windows run's
  commit, against 2 MiB for a test thread. Nothing large was added between the two: about 22 KB of frame growth spread
  over the chain, the most in `RunLoop::on_finished` (+6 KB) and `RunLoop::run_loop`
  (+5 KB), on a path that already used 86% of 2 MiB on macOS. That it crossed the limit on
  Windows, and not on macOS, fits larger frames there; Windows frames were not measured. The
  cause is structural: in a build without optimizations each `async
  fn` builds the future of every call it awaits in a stack slot of its own poll frame, so
  frames grow with the futures they await and add up along the chain (frames measured from
  the binary's prologues: `PreparedRun::run_inner` 359 KB, `RunLoop::on_finished` 242 KB,
  `RunLoop::run_loop` 203 KB, `Orchestrator::start` 166 KB, and six more). The fix moves the
  large futures off the poll frames: `kgraf_core::heap_future::on_heap` builds a future in
  its own short-lived frame and returns it boxed, and the hosted run path uses it at every
  large await (`PreparedRun::run` and `run_inner`, `Orchestrator::start`, `start_from_plan`
  and `resume`, the plan review, `run_after`, `run`, every handler of `run_loop`, the large
  calls of `on_finished`, and the verifier task it spawns). The same path now takes 449,128
  bytes; the test passes with a 512 KiB stack, where it overflowed at 1.75 MiB before. The
  stack size was not raised. The same path runs outside tests: the desktop shell polls the
  run on tokio worker threads of 2 MiB, so a debug build of the shell could overflow the
  same way; the CLI's `orchestrate` runs it on the main thread, which has 8 MB on Windows
  (`/STACK`). Release builds have far smaller frames. Tests:
  `a_hosted_run_fits_in_half_a_test_threads_stack` (the scenario on a 1 MiB thread; reverted,
  it overflows) and `a_prepared_runs_future_is_small` (reverted, the future is 64,160
  bytes), and `heap_future`'s own test.
- **Correction: a process's current folder does not cause error 32.** The amendment before
  this one, and the one on the first Windows run's setup, named a process's current folder
  as a holder that makes a `MAXIMUM_ALLOWED` open fail with `ERROR_SHARING_VIOLATION`. This
  run measured otherwise: the three propagation tests started `ping.exe` with its current folder
  in the tree, and the folder still opened with `MAXIMUM_ALLOWED` ("the condition under test
  is absent"); a test in `confined.rs` had already found the same for `ping` and `cmd`. The
  error needs a handle held without `FILE_SHARE_DELETE`, as Windows and its services held
  `C:\` and `C:\Windows\Tasks` in the first Windows run of setup. The tests now open that handle themselves
  (`CreateFileW`, `FILE_FLAG_BACKUP_SEMANTICS`, list and traverse, read and write shared,
  delete not; sharing is checked per handle, not per process) and still refuse to pass when a
  `MAXIMUM_ALLOWED` open of the held folder does not fail with 32. The grant test is renamed
  `a_grant_and_a_revoke_succeed_with_folders_held_without_delete_sharing`. The code comments
  that named a current folder are corrected. The second open, the lone write and the
  removal behavior are unchanged.
- **A lone write asks Windows to keep the auto-inherited bit.**
  `closing_a_folder_changes_no_child_and_keeps_the_protection` and
  `a_protected_folder_keeps_its_protection_and_its_list` failed with "did not keep the list
  as written (protection bits 0x0400 before, 0x0000 after; entries as written)" and "0x1400
  before, 0x1000 after". `SetKernelObjectSecurity` kept the entries and the protected bit and
  dropped `SE_DACL_AUTO_INHERITED`; the read-back refused it, as designed, and the roll-back,
  written the same way, could not put the bit back. Both tests had run `icacls` first, which
  sets that bit; the tests on a bare temporary folder and setup's own folders passed. A
  folder whose list had the bit is now written with `SE_DACL_AUTO_INHERIT_REQ` as well
  (`acl::lone_write_control`, host test `a_lone_write_asks_to_keep_the_auto_inherited_bit`);
  a folder without it is written as before. That the request bit keeps the bit on this path
  is not documented and not measured; the read-back and the roll-back still decide, and a
  folder that loses the bit still fails the write loud.

What the next Windows run must show:

- Setup: exit 0 on a machine that refuses the name resolution cache filter, a `refused` line
  per refused filter whose reason carries `FWP_E_OUT_OF_BOUNDS` and a `probe:` reading, the
  step line `partial`, a received password for every user, and the closing notice that DNS
  is not confined. The probe reading decides which option above is worth a try.
- Status (c): `present` for the RPC filter if Windows took it, `FAILED` with the reason for
  each refused filter, every other line `present`, and the closing sentence about DNS.
- Selfcheck (d): backend `Windows sandbox user`, every line but `dns:` a pass, and `dns:
  not confined` or `dns: not proved`; the phase then runs the re-run (e) as a DNS-only
  failure.
- Remove (f): every WFP key removed or already gone, and status after it all `MISSING`.
- `a_lone_write_on_a_held_folder_changes_no_child`,
  `a_grant_and_a_revoke_succeed_with_folders_held_without_delete_sharing` and
  `a_revoke_through_a_lone_write_drops_the_inherited_copy` pass, with no "condition under
  test is absent" panic.
- `closing_a_folder_changes_no_child_and_keeps_the_protection` and
  `a_protected_folder_keeps_its_protection_and_its_list` pass. A failure that still reports
  the auto-inherited bit dropped means the request bit does not keep it on this path.
- `tests::shortfall::auto_runs_a_script_of_allowed_programs_and_names_the_one_it_does_not_allow`
  and `a_hosted_run_fits_in_half_a_test_threads_stack` pass, and no `kgraf_host` binary ends
  with "has overflowed its stack".

## Amendment (2026-10-09): what the first re-run as the sandbox user found

A hosted Windows Server 2025 run (Datacenter 24H2, build 26100.33438). It ran the self-test's re-run (e) as the sandbox user for the first time. As that user
Git Bash, Git's curl over TLS (Schannel), programs that load `user32.dll` and the stdio pipe
test passed, the reasons this decision exists. Nothing below has run on Windows since; each
bullet says what is proved on the host and what a run must show. Each bullet is one behavior
change.

- **Loopback is blocked by WFP filters, not by the firewall rules.** The rule
  `block-loopback-tcp` was present for the offline slot user, and that user still opened a
  TCP connection to a listener on 127.0.0.1 outside the proxy ports: selfcheck failed with
  "a direct connection to the origin worked; only the proxy should be reachable", and
  `a_command_without_the_network_reaches_the_proxy_ports_and_nothing_else` with "reached a
  local listener ... connected". The rule existed and did not act, so Windows Firewall does
  not filter this loopback traffic. Setup now adds four filters in kGraf's WFP sublayer at
  `FWPM_LAYER_ALE_AUTH_CONNECT_V4` and `..._V6`, with the same user match descriptor as the
  DNS filters and `FWPM_CONDITION_FLAGS` with `FWP_CONDITION_FLAG_IS_LOOPBACK`: per IP
  version, one blocks any remote port outside the proxy ports (two `FWP_MATCH_RANGE`
  conditions on `FWPM_CONDITION_IP_REMOTE_PORT`, 0 to 47099 and 47132 to 65535, which WFP
  joins with OR), and one blocks UDP (`FWPM_CONDITION_IP_PROTOCOL` 17). openai/codex filters
  at the same layers with the user ID and the remote port. They are WFP objects like the DNS
  filters (`wfp-filter-loopback-v4`, `-v6`, `wfp-filter-loopback-udp-v4`, `-v6` in the undo
  record), so setup, status, remove and the refusal path cover them: a filter Windows
  refuses is a `refused` line with a `probe:` reading, a `FAILED` status line that says
  loopback connections are not confined, and a setup notice of its own; status then ends
  with "loopback connections are not confined on this machine". The firewall rules stay:
  they block traffic off the machine. Host tests:
  `the_loopback_filter_blocks_every_port_but_the_proxy_ports`,
  `filters_are_dns_or_loopback_and_say_which_layer`,
  `a_refused_loopback_filter_is_a_gap_the_notice_names`,
  `status_shows_a_refused_loopback_filter_as_failed`; with the four filters left out of the
  set, six tests fail. Whether Windows takes these filter shapes, and whether the block takes
  effect beside the firewall's own loopback handling, is not measured; selfcheck's `network:`
  line decides. Consequence: a command without the network can no longer reach a listener
  on loopback outside the proxy ports, its own included, which is what the loopback rules
  meant from the start. A project test that starts a local server and connects to it fails
  as the offline user, where Linux, which gives each command a loopback of its own, passes
  it. No condition found at the connect layers names the listener's user, so the block
  cannot tell the command's own listener from another program's.
- **A jobserver per command.** Every cargo build as the sandbox user stopped at rustc's
  start: "failed to create jobserver: Os { code: 5, kind: PermissionDenied }"
  (rustc_data_structures `jobserver.rs`), exit 0xC0000409, and cargo's "failed to run
  `rustc` to learn about target-specific information". With no jobserver to join, the
  `jobserver` crate creates a named semaphore with no prefix, in the session's
  `BaseNamedObjects`, and the command cannot create objects there: the session's directory
  admits the session's logon SID, which the runner makes deny-only. The same tests failed in
  the baseline under the token sandbox, whose restricting SIDs refuse it the same way.
  kGraf's process now makes a semaphore for each command run as the sandbox user,
  `kgraf-jobserver-<16 hex>`, protected, admitting SYSTEM and the owner fully and the
  command's sandbox user to `SYNCHRONIZE | SEMAPHORE_MODIFY_STATE` only, with as many tokens
  as the machine's parallelism, never a name that already exists, and names it in
  `CARGO_MAKEFLAGS` the way cargo does (`-j --jobserver-fds=<name> --jobserver-auth=<name>`),
  over an inherited value. The started child holds it. The launch record has a line per
  process. That the directory refuses the creation is inferred from the error and the
  directory's usual access list, not measured; the new escape test
  `the_sandbox_user_opens_its_jobserver_and_named_objects_are_measured` measures a creation
  by the sandbox user and asserts the open of the jobserver. Other programs that create a
  named object in the session are still refused (a gap the measurement shows). rustup also
  printed "warn: could not canonicalize path C:\Users\<user>": it canonicalizes each folder
  above the command's folder to look for a toolchain override, and the owner's profile
  folder is not readable. It is a warning; the profile stays unreadable.
- **safe.directory for the sandbox user's own roots.** `git status` in the worker's
  worktree failed with "detected dubious ownership in repository" (the end-to-end check
  `shell.git` and the worker end-to-end test): the worktree belongs to the owner and the
  command runs as another user. A command run as the sandbox user now gets `safe.directory`
  entries through `GIT_CONFIG_COUNT`, which git reads as the command scope, a protected
  scope: each writable root, each repository below it (`<root>/*`), and each read root
  alone, in the form git compares. A repository in a writable root is one the command can
  rewrite already; trusting it gives the command nothing. Host test
  `the_sandbox_user_gets_safe_directory_entries_for_its_roots`; fails with the entries left
  out.
- **A refused `MAXIMUM_ALLOWED` open gets the minimal open and the lone write.** The read
  grant on a repository's `.git` reached most of it and left git's loose objects with
  "Access is denied", in the owner's own repository, which then failed the gate and
  speculation tests. Git writes loose objects read-only; a `MAXIMUM_ALLOWED` open of a
  read-only file is refused although the list grants everything (inferred from which files
  failed, not documented). Error 5 now takes the same second open (`READ_CONTROL`,
  `WRITE_DAC`, `FILE_READ_ATTRIBUTES`) and lone write as error 32; a caller with no right is
  refused the second time too. The pass's reasons now name the failing step (`open:`,
  `read:`, `write (SetSecurityInfo):`, `write (lone):`). Host test
  `a_sharing_violation_or_a_refusal_opens_the_object_again_for_a_lone_write`.
- **quiesce sees the commands kGraf started in a worktree on Windows.** On Unix every
  process of a command holds a shared lock on the worktree's hold file; Windows had no hold,
  and `a_worktree_the_worker_still_writes_is_not_snapshotted` ended "expected a busy
  failure, got Pass": the verifier committed a tree a background process still wrote. Each
  command started with a hold is now listed under it by a weak reference to its job, and
  `WorkspaceManager::quiesce` is busy while one of those jobs holds a process (or cannot be
  read). The list never keeps a job open, so dropping the child still ends its tree. A
  command started by another process is not seen; kGraf runs its workers' commands in its
  own process.
- **A gate's status after its tree ended is waited for.** `a_gate_command_past_its_timeout_is_killed_and_fails`
  failed with "`sleep 30` still runs after its tree ended": the job held no process, and
  one `try_wait` on the runner's handle found it not yet signaled. The order of the two is
  not documented; the gate now polls the status for up to 5 seconds. Host tests
  `a_status_that_arrives_after_the_tree_ended_is_collected`,
  `a_status_that_never_arrives_is_none_at_the_bound`.
- **Tests that assumed the token sandbox or an earlier design.** The window station tests
  expected `kgraf-sandbox-<hex>` and a station per command; kGraf makes `kgraf-cmd-<hex>`
  per sandbox user and process, so two commands of one user share it. The junction test
  ran its second kind beside the junction the first planted, and the next command was
  refused by the link walk before any entry was written, as designed; the test now pins
  that refusal and removes the junction. The cleared environment allows the sandbox user's
  toolchain variables (`WINDOWS_SANDBOX_USER_VARS`). The shell tool test, the self-test's
  `notice.once` check and the prefix digest pin expect the network notice only where the
  backend leaves the network open. Windows paths in Git Bash's `pwd` and in traces are
  compared in one form. The check-cleanup test links to a file in the repository's shared
  git folder, which every backend lets the check read. The DNS tests judge the lookup, not
  the filters' presence, and print each filter's state; netsh does not list the RPC layer's
  filter (kGraf's own reading found it, netsh did not), so that filter is not looked for
  there. The self-test calls a selfcheck that stopped at its probe a failure on the sandbox
  user that printed no result lines, and says whether the probe ran and failed a step or
  did not finish.

What the next Windows run must show:

- Setup (b): the WFP step's line `created` or `partial` with no `refused` line for a
  loopback filter. Status (c): four `present  loopback filter on the connect layer` lines.
- Selfcheck (d): `network: only the egress proxy was reachable`, a launch line `made
  jobserver semaphore`, and every line but `dns:` a pass.
- Re-run (e): the proxy-port escape test, the cargo escape test, the bench-briefs checks,
  the contract checks, the worker end-to-end tests, the end-to-end `shell.git` and
  `notice.once` checks, the escape window station tests, the junction test, the startup
  environment test, and the twelve kgraf_host tests pass; the named-semaphore measurement
  lines are present; no "could not be opened or written" note for a `.git` read grant.
- A failure of any of these carries its evidence: a `refused` loopback line with its probe
  reading, cargo's own lines, the parent's view of the fence refusal, a note naming the
  failing step.
