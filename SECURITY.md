# Security policy

## Supported versions

kGraf is pre-release software. Security fixes go to the latest version on the `main`
branch only.

## Report a vulnerability

Do not open a public issue for a security problem.

Report it privately through
[GitHub private vulnerability reporting](https://github.com/Marceux-Croix/kGraf/security/advisories/new).
Include:

- the affected version or commit,
- the steps to reproduce the problem,
- what an attacker can do with it.

You will get a reply within 7 days. When the fix is released, you will be credited in the
advisory unless you ask not to be.

## Scope

These areas are in scope and of most interest:

- escape from a worker's sandbox or git worktree,
- provider keys or other secrets that leak into transcripts, the ledger, logs or
  diagnostics,
- network traffic to any destination other than the model providers and MCP servers the
  user configured,
- tampering with the hash-chained ledger that `kgraf ledger verify` does not detect.

Problems in a model provider's own service are out of scope. Report those to the provider.
