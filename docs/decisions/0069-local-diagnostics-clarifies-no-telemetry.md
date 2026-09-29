# Decision 0069: Local-only diagnostics clarifies "no telemetry"

**Status:** Accepted
**Related:** kGraf's local-only standing rule; `crates/kgraf-diag`

## Context

kGraf keeps a standing rule against telemetry: nothing about a user's project or machine is
ever collected by kGraf as a project. That rule was written to forbid a hosted analytics
service or an automatic crash reporter, the kind of mechanism many applications bundle by
default. It was not written with a purely local diagnostic file in mind, and read literally it
could be misapplied either to block a genuinely local, user-controlled diagnostic file, or,
read the other way, to be treated as a loophole that later justifies something that actually
does phone home.

## Decision

kGraf keeps one capped, local file of basic usage counts and every error it encounters, each
labeled with a stable glossary code. This is diagnostics, not telemetry, because nothing in it
is ever transmitted anywhere: the code that writes this file contains no networking code at
all, not a client, not an opt-in upload, not a background sync. If a problem ever needs
diagnosing, the user runs a command that exports the file, and shares it however they choose,
the same way they might attach a log file to a bug report.

The no-telemetry rule is clarified to read: no telemetry ever leaves the machine; a local,
user-exported diagnostics file is a distinct and permitted thing.

## Consequences

- A future feature that wants to send anything from this file anywhere is a new decision in
  its own right; this one does not pre-authorize that.
- The file's error glossary is generated directly from the code that defines it, so the two
  cannot silently drift apart.

## Alternatives considered

- **Rely only on ordinary application logging.** Rejected: ordinary logs are not capped, carry
  no common error glossary, and require a user to reproduce a problem with logging turned up,
  which is a worse diagnostic experience than exporting one small file.
- **A hosted crash reporter.** Rejected outright: this is exactly what the no-telemetry rule
  forbids, opt-in or not, because it is a service kGraf operates rather than one the user
  configured with their own credentials.
