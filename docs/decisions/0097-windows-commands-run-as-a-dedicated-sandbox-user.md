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
