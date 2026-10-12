# Decision 0065: A call resolves only within its own language family; each language extractor is stamped; a lighter tier adds more languages

**Status:** Accepted
**Amends:** 0038 (a call resolves only within its language family)
**Related:** refines decision 0038; decisions 0049, 0050, 0055

## Context

The indexer had recorded a Python function as the caller of a same-named Rust function,
purely because the two names matched. On a mixed-language codebase this produces false call
edges. Separately, the index tracked only a single, hand-maintained schema version, so a
change to how one language's code is chunked left old rows unrefreshed under unchanged file
bytes. Several code-parsing libraries kGraf already depends on ship a general-purpose tagging
query that a lighter indexing tier can reuse, rather than kGraf writing a bespoke table for
every language it wants to support.

## Decision

**A call resolves only to a definition in the caller's own language family.** Families:
Rust; Python; TypeScript, TSX and JavaScript; C and C++; Go; Java; Ruby; C#; PHP; Markdown.
This check runs before the existing proximity-scoping rule, so a definition in another family
is never a candidate at any scope. A call whose name is defined only in another family is
counted and reported as unresolved across languages.

**Each language's extractor carries a stamp**, built from its own parsing rules, its call
kinds, and the grammar it depends on. When the stamp changes, only that language's files are
re-derived; other languages and the rest of the index are untouched, and the tool reports
which language changed.

**A lighter "tags" tier indexes several more languages (Go, Java, C, C++, Ruby, C# and PHP)
from each language grammar's own general-purpose tagging query**, rather than a table kGraf
writes by hand for each one. Definitions and call sites come from that query directly. A node
built this way is marked as tags-tier, since its calls resolve by name only and some queries
miss certain call shapes (a bare call in PHP, a non-member call in C#, any call at all in C).

## Consequences

- A mixed-language repository loses false call edges between languages, at the cost of losing
  real, deliberate cross-language calls too (for example a scripting language calling into a
  native extension). Nothing in the index sees these calls today; a caller needs a rule of its
  own for a binding layer before they return.
- The application binaries grow by roughly a third, because a parsing grammar's generated
  tables are large; the two largest additions (C# and C++) are the first candidates to trim
  or make optional if size matters more than the languages they add.
- The first index after this change re-derives every file once, since no stamp existed
  before.

## Alternatives considered

- **Resolve across languages, but weight the edge lower.** Rejected: a cross-language edge by
  name alone is wrong, not merely weak evidence, and should not be kept at any weight.
- **Bump a single schema version for every extractor change.** Rejected: it forces a rebuild of
  every language's rows and depends on someone remembering to do it. A per-language stamp
  captures exactly what changed.
- **Write a dedicated table for every added language up front.** Rejected for now: it is
  ground the upstream tagging queries already cover; a language can graduate to its own table
  later if the lighter tier proves too coarse.
