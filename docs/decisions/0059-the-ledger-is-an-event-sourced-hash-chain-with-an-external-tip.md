# Decision 0059: The ledger is an event-sourced hash chain with an external tip

**Status:** Accepted, addressing a request for tamper evidence without a blockchain
**Related:** decisions 0039, 0040, 0051

This record exists so the accounting ledger's integrity design is not re-argued, and so what
it can and cannot prove is stated once.

## Decision

- **Events, not rows, are chained.** Every ledger mutation (begin, settle, reconcile, redact)
  is recorded as an immutable event appended to a chain table, with a digest over the exact
  stored bytes of that event and a chain hash over the previous chain hash plus that digest.
  Verification checks that each accounting row equals the fold of its own events.
- **Redaction** removes only the raw payload of a row. A stored hash of the raw payload keeps
  the row's digest checkable even after redaction, and every redacted row must have a matching
  redact event on the chain.
- **An external tip anchor.** The latest chain position (sequence number and chain hash) is
  written to a separate anchor file by a background writer, off the main settlement path,
  using a write-then-rename pattern with a forced disk flush. Verification accepts an anchor
  that lags slightly behind the chain, and reports either a chain shorter than the anchor
  (tail truncation) or a chain that differs at the anchored sequence number (a rewrite).
- **A truncated or failed stream stays visible.** A call that fails partway through is
  recorded with a lower-bound usage estimate and its request id; the row is excluded from
  totals until reconciled, rather than silently dropped.
- **Durability.** Write-ahead logging with full synchronous commits, an immediate write lock
  around every mutation, a busy-retry timeout, and a one-time backfill of historical data
  gated on a schema version marker.

## What it proves, and what it does not

It detects an edited, deleted, or forged row; a gap in the sequence; a redacted row without
its corresponding event; a hand-inserted row that bypasses the chain; and tail truncation or a
rebuilt chain, up to the point of the last completed anchor write. It does not detect a
coordinated rewrite of both the database and the anchor file together, and events recorded
after the last completed anchor write are not covered by that anchor.

## Alternatives considered

- **A blockchain.** A single-user local app has no second party to reach consensus with;
  consensus machinery would add latency and network traffic for no benefit.
- **A synchronous anchor write on every settlement.** Measured at roughly 5 milliseconds per
  settled call on disk, which breaks the agent loop's own latency budget. Replaced by the
  background writer, measured at roughly 600 microseconds.
- **Chaining ledger rows only at settlement time.** Broken by an ordinary reconciliation step
  and could hide an edit behind the redacted flag. Replaced by chaining every event instead of
  only settled rows.

## Consequences

Roughly 600 microseconds per settled call on disk, roughly 80 microseconds if held only in
memory.

Reactivated if the anchor's lag window proves too wide in practice.

## Addendum: single writer, and what "durable" means on macOS

The write path uses a single writer thread that batches queued operations into one committed
transaction and acknowledges each caller only after that commit completes, measured at
roughly 9,300 settled calls per second at 16 concurrent tasks on disk, up from roughly 1,600
without batching. An acknowledged row survives a process crash and an operating-system crash.
On macOS it is not guaranteed to survive a full power loss: the ledger keeps the database
engine's default disk-flush behavior, which on macOS does not force the drive's own write
cache to flush. The anchor file uses the same flush mechanism, so the two stay consistent with
each other, but after a power loss the anchor can end up ahead of the chain, and verification
can then report a truncation that did not actually happen. This is a deliberate performance
choice, since a full flush on every commit measurably stalled the agent loop, and it is
recorded here so it is never mistaken for a stronger durability guarantee than it actually
provides.
