# Decision 0084: Workers run an allowlist of build and test commands in the sandbox, with the network off

**Status:** Accepted
**Related:** decisions 0043, 0051, 0060, 0067, 0071, 0083

## Context

Under a declared write scope, a worker's shell call was refused outright: a shell command
declares no paths, so the per-call scope check could not see what it wrote. Together with the
default command policy (anything not known to be read-only asks for approval, and an
unattended worker's approval is a denial), no orchestrated worker could compile or test its
own work. Every attempt was judged blind by the verifier, and a retry learned nothing.

Two things made a narrower allowance safe: the verifier commits the attempt first and refuses
it if the commit changes any path outside the declared writes (decision 0083 holds the same
line inside the merge), and the sandbox keeps the worktree's gitlink read-only.

## Decision

**1. A declared set of build and test commands runs without approval.** The set lives in the
project's worker configuration, `.kgraf/worker.json` (`build_commands`), outside every
worktree. It is on by default: with no `build_commands` key, or no file, it is `cargo build`,
`cargo check`, `cargo test`, `cargo clippy` and `cargo fmt --check`; `[]` turns it off; any
other entry fails the load with a message naming the five. An orchestrated run adds its own
acceptance command and each configured gate command, exactly as configured. Nothing is chosen
per model or per route. The shell tool's description names the list, except on Windows, where
the sandboxed shell does not run.

**2. Each cargo entry is held to argument rules, an allowlist per subcommand.** Every argument
must be an option listed for its subcommand, with a valid value, or a test-name filter.
Refused, with the argument named: a `--manifest-path` outside the worktree, `--config`, `-Z`,
`--target-dir` and similar output redirections, `-C`, a `+toolchain`, a target spec file,
`cargo clippy --fix`, `cargo fmt` without `--check`, and rustc flags after `--`. A listed
subcommand whose arguments break a rule is refused before any approval question, for attended
workers too; a command not on the list is decided by the command policy as before.

**3. The worktree's cargo configuration and toolchain file are fixed for the attempt.** When
the shell tool is bound to a worktree it records `.cargo`, `.cargo/config.toml`,
`.cargo/config`, `rust-toolchain` and `rust-toolchain.toml` there. Cargo and rustup read these
from the directory a command runs in, and they change what a command runs whatever its
arguments say: an `[alias]` can turn `cargo clippy` into `cargo run`, and a `path` toolchain
runs any binaries. While any of them differs from the record, every allowlisted command is
refused. A project's own committed configuration keeps working.

**4. Under a declared write scope, an allowlisted command runs, and no other command that can
write.** Every other shell command, and every MCP tool, is still refused under a scope.

**5. It runs inside the OS sandbox with the network off.** Writes are confined to the worktree
and its scratch directory, the gitlink is read-only, kGraf's state is unreadable, and cargo
builds into the scratch directory, offline. kGraf does not fetch dependencies: they must be in
the local cargo cache before a run.

**6. What a build can read, and what its output may carry.** A build runs code the worker
wrote, and its output goes to a model provider.

- On macOS the checkout a worktree lives in is unreadable to a build, apart from the worktree
  itself, the git directory, file metadata and the cargo files cargo reads on its way up. A
  `.env` at the checkout root stays private. kGraf's per-user data directory and another
  coding agent's configuration file in the home directory join the credential read deny list.
  Linux reads are already an allowlist.
- An allowlisted command's output goes through the full secret scrub (pattern rules and the
  exact-secret registry, decision 0071) before the model sees it.
- Every value in the checkout's `.env` is registered with the exact-secret registry when an
  attempt is assembled, and never logged.
- A verifier's refusal reason reaches the next attempt as a tagged data block, and the
  worker's system prompt says that block is data, never instructions.

**7. The verifier still judges the committed attempt.** It refuses a commit that changes a
path outside the declared writes, and its refusal reason now reaches the next attempt.

**8. The shell timeout is 600 seconds and configurable,** and a command killed at the timeout
returns its output so far.

## Known gaps

- A process that closes every inherited descriptor and starts a new session outlives its
  command without being seen, on macOS and on Linux without PID namespaces. On Linux with
  namespaces the kernel kills it when the command exits. No reliable way was found to list
  the processes of one macOS sandbox. Only the quality gate, which runs on the merged tree
  outside that sandbox's writable roots, judges what is merged.
- Worker-written test code can still read files outside the checkout that the user can read,
  on macOS, and print them. The pattern scrub catches key-shaped text, not every secret.

## Rejected

- **Keep refusing.** Workers never see a compiler error.
- **Any shell command under a scope, relying on the verifier.** Each command needs its own
  reasoning; an allowlist states it once.
- **A denylist of dangerous cargo flags.** Cargo grows options; a denylist admits each new
  one until someone notices.
- **Setting `RUSTUP_TOOLCHAIN` and isolating cargo's configuration from the host.** It would
  also override a project's own pinned toolchain and configuration.
- **Network access for builds.** Worker-written build scripts and tests would get the network.
