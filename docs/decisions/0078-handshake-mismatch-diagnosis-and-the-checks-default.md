# Decision 0078: The handshake comparator reads qualified names and signatures; interface checks stay off by default

**Status:** Accepted
**Related:** decisions 0026, 0030, 0031, 0033, 0035

## Context

kGraf's optional interface checks include a "handshake": each side of an interface states, in
plain prose, what it expects from the other, and the orchestrator compares the two statements
mechanically. In its first live test against real model providers, the comparator flagged
seven mismatches. On inspection, six were noise: real models write a symbol's fully qualified
name rather than its bare name, write out a full type signature rather than restating it word
for word, and sometimes describe their own provisions more fully than the comparator expected.
Only one of the seven flagged a genuine disagreement between the two parties.

## Decision

**The comparator is widened to read a symbol however a real model actually writes it**: a
fully qualified name, a bare name, or a full signature are all recognized as referring to the
same underlying symbol, rather than requiring an exact, word-for-word match. A statement about
a party's own symbol is no longer treated as if it were a claim about the other party.

Replayed against the earlier live test, the comparator now reports only the one genuine
disagreement, and continues to catch every deliberately constructed real mismatch used to test
it.

**Interface checks stay off by default.** The widened comparator is a necessary condition for
turning them on by default, but not yet sufficient: an unrelated default limit elsewhere in
the pipeline still cuts off a large enough decomposition before it reaches these checks at all,
and no live run with the checks turned on has yet reached the point of actually dispatching
work.

## Consequences

- The comparator's known blind spots are documented and grow slightly narrower rather than
  wider: a bare type is still matched loosely to a full signature containing it, and a symbol's
  containing module or file path is not itself compared, since an earlier build-time check
  already proves the two files reference the same real symbol.
- The condition for eventually turning interface checks on by default is now explicit: a live
  run, with an adequately sized decomposition limit, that reaches dispatch with the handshake
  passing.

## Alternatives considered

- **Loosen the comparator with a fuzzy text-similarity score.** Rejected: the handshake is
  meant to be a precise, symbol-level comparison; a similarity score has no clean line between
  harmless rewording and an actual, meaningful change.
- **Ask models to write bare symbol names instead of qualified ones.** Rejected as the fix:
  the handshake is explicitly testing what models actually say unprompted; a comparator that
  only understands one specific style of answer will keep failing on the next model provider
  tried.
- **Flip the default on immediately, since the comparator now behaves correctly.** Rejected:
  a correct comparator is necessary but not sufficient; a separate limit elsewhere in the
  pipeline, and the lack of any full passing live run, are both still open.

## Amendments (2026-09-30)

Orchestrated runs held on handshake findings the parties' words did not support. Each case is
now a test, each with a companion that proves a real disagreement still fails.

- **The interface's own label is set aside**, like party ids. The bare id is set aside too,
  unless it is also a surface symbol's name.
- **Claims are the anchors written after the subject.** An anchor before the symbol a
  sentence is about describes the party's own code, not the symbol. Known blind spot: an
  off-surface type written before the symbol is not compared.
- **A one-word claim written in the provision's prose matches**: a provider that writes "for
  input CA" states the `CA` an expecter backticks. Case-sensitive, whole words, prose only; a
  type or a signature must still be among the provider's anchors.
- **A provision about a symbol is not a provision of it.** When the prose before the symbol
  names a test or a use of it ("integration tests for `x`", "the call sites that invoke `x`"),
  the party is not counted as a second provider. "An implementation of `x`" still provides
  `x`. Known blind spot: "a wrapper that calls `x`" is read as not providing `x`.
- **A negated sentence with no anchor states an absence** ("I assume the other party provides
  nothing I need") and is not reported as unanchored. A positive unanchored sentence still
  is.

## Amendments (2026-09-30, corrections to the amendment above)

Each relaxation above was probed with a true disagreement. Three hid one, and one dropped a
real provision. Each probe is now a test; every case above still passes. These bullets
supersede the matching bullets above.

- **Claims are the anchors on either side of the subject, except those before it in an
  earlier clause.** "I assume `i32` is what `double` accepts and returns" claims `i32` of
  `double`. A clause ends at `;`, at a sentence end, or at a comma followed by a joiner (and,
  but, or, while, which, and similar); a plain comma separates a list. The blind spot above
  narrows to an anchor in an earlier clause.
- **A one-word claim matches the provider's prose only when it is not a type.** A primitive
  (`i64`, `u8`, `f32`, `int32`, `str`, `bool`) or a CamelCase name must be among the
  provider's anchors. State codes (`CA`, `OR`) still match.
- **A use word counts only when it governs the symbol**: it is the last word of the clause
  before the symbol, skipping articles and prepositions ("tests for `x`"). "Using `clap`, I
  implement `run`" provides `run`.
- **Only a stated absence of symbols is set aside**, not every negated sentence with no
  anchor: negated, with a verb of providing or needing and the noun symbol or item ("I
  provide no contract symbols of my own"). "I assume the other party does not mutate the
  input or panic" is `unanchored`.

## Amendments (2026-09-30, plans held on wording)

Coherent plans were held on the handshake: a test writer counted as a second provider, two
test writers of one file in dependency order counted as an overlap, and sentences naming no
surface symbol held the plan. Three recorded handshakes, two coherent and one with three
real implementers of one symbol, are replayed verbatim as test fixtures.

- **A provision counts only from a party that writes the symbol's stub file** (a stub file
  naming the symbol, or any stub file when none names it). A test writer ("I will provide
  `x` integration-test coverage", "an integration test verifying `x`") or a party that
  writes nothing provides nothing, whatever the word before the symbol. With no party
  writing such a file the sentences alone decide, as before.
- **A shared touch is an overlap only between parties no dependency orders.** Two test
  writers of one file, one after the other, never write it at once.
- **`unanchored` and `outside_surface` hold nothing.** They are recorded as warnings with
  the pass. Every other finding holds.
- **An expectation claim the interface's own files state is not a disagreement with a
  silent provider**: a type the stub's signature gives, a crate name its test imports. The
  files are held to the parties by stub-compile and vacuity already. A provider that states
  a signature for the symbol the files do not give still disagrees. Known blind spot: a
  claim the files state anywhere (`i64` in another function of the stub) backs a silent
  provider.
- **A negation contradicts only between the same anchors.** An expectation with anchors of
  its own beyond the symbol negates something about those, which a provision naming more
  anchors does not state the opposite of. Different numbers still contradict.

On the fixtures, the two coherent plans hold nothing (warnings only), and the three
implementers still hold on the double provision, with no overlap between them.

## Amendment (2026-09-30): a negated assumption about a third party

A coherent plan was held on a false contradiction: a test writer expected "I assume
`c1.c2.c2` does not change the `parse_duration` interface.", and the implementer provided
"I will provide the public `parse_duration` function using its existing signature." The
two agree. Party ids are set aside when a sentence is read, so the expectation had no
anchors of its own, and "not" against no negation read as the opposite behavior. The
recorded handshake is replayed verbatim as a test fixture.

- **A negated expectation is compared with a provision only when it is about the provider:**
  it names the providing party, backticked or not, or it names no party. An assumption
  naming only other parties says what they leave alone, which the provider's sentence does
  not state the opposite of. Different numbers follow the same rule. `disagree` (a claim the
  provider does not state) is unchanged: a claim about the symbol is about the symbol,
  whoever the sentence names.

## Amendment: a hold names the models of its parties

Handshake statements and contract-check events named parties, never their routes or models, so
an interface hold could not be tied to the models that caused it.

- **Each handshake statement** (`HandshakeStatement`) carries `route`, the route its party runs
  on as the request named it, and `model`, the model that stated it: the handshaker's caller
  (`ModelHandshaker`, `RoutedHandshaker`), or the route's model when a handshaker does not say.
  Both are set by kGraf after the answer is parsed (`parse_handshake` clears them), never read
  from the model's text, like `party`.
- **Each check event** that names parties (`ChecksPassed`, `CheckFailed`, `CheckNotRun`) carries
  `parties`: every party of the interface, held or not, with the route it runs on (its recorded
  worker route, else its assigned or pinned route, else the route its handshake was asked on) and
  that route's model. A party with no route yet (verified before the run, or a route that is not
  configured) has both empty. Reading them picks no route.
- **The run record** keeps every hold a check placed (`RunStateRecord::check_holds`): the
  interface and version, the failed stage (`artifact`, `stub_compile`, `vacuity`, `handshake`,
  `stub_land`, or `not_run`), the contracts held and the same `parties`. A check that held
  nothing adds no entry.
- **Older records** read with the fields empty (`serde(default)`); nothing is rewritten.

Tests: `contract_flow.rs` `a_handshake_hold_names_each_partys_route_and_model` (a mismatch on
two routes: each statement names its route and the model that stated it, the `CheckFailed`
event names both parties with their routes and models on different routes, and the run record's
last `check_holds` entry holds the same parties and held contracts); `checks.rs`
`a_party_states_its_handshake_on_its_own_route` (a routed statement names `b:mb` and `mb`, and a
model's answer naming a route or a model is cleared).

## Amendment (2026-10-06): a path that is a surface symbol is not set aside

Paths were set aside as claims. On an interface whose symbols are files (`pyproject.toml`,
`vectorops/__init__.py`), that left no claim any party could anchor, and a coherent handshake held
on `no_claims` and `symbol_unprovided`. A path that is a surface symbol now names it; a path no
surface symbol names is still set aside; a file symbol is provided by the party that writes that
stub file; a path anchor or touch is one file whatever its leading `./` or `/`; and the prompt says
`touch` lists only the paths the party will write. The details and tests are in decision 0030's
amendment of 2026-10-06.

## Amendment (2026-10-06, second): a provision provides every symbol it lists, and an anchor is a code span

A coherent handshake held on `symbol_unprovided` for a symbol its provider did state:

> I will provide `` `vectorops` `` (a Python package) and `` `dotproduct` ``. The symbol
> `` `dotproduct` `` refers to a function with signature
> `` dotproduct(numbers_a: list[float], numbers_b: list[float]) -> float ``.

The reader took the first surface anchor as the only subject and read every later anchor as a
claim about it, so `dotproduct` was never a provision. It also split at every backtick, so a
CommonMark double-backtick span was read as an empty anchor with its content as prose, and the
signature's types were lost.

- **An anchor is a CommonMark code span**: a run of N backticks opens a span that the next run of
  exactly N closes, a run with no closer is prose, and backticks nested inside a span are trimmed
  ("`` `x` ``" anchors `x`).
- **A provision provides every symbol it lists**: a later surface symbol listed with a provided
  one (a comma or a joiner, a parenthetical dropped, kind words allowed), or one that leads a
  later clause, is provided too, with the claims written after it. The writer rule and
  `double_provided` apply to each. A symbol named as a claim ("which returns a `Config`") or under
  a use word ("and tests for `b`") is not provided. Expectations read one subject, as before.
- **Other languages' names**: an import line names its symbols (`from vectorops import
  dotproduct`, `use a::{b, c};`, `import { b } from "a"`); `#` separates segments; a call form or
  an untyped `def` names the symbol and claims no arity; a default value, Python's `*` and `/`
  markers and `cls` are not types; a JavaScript or TypeScript `function` line with a `: T` return
  reads as a signature. Names are compared with case kept.

A provider that never names a symbol is still `symbol_unprovided` and holds; no re-ask is added.
Still not read: a symbol named only as contained ("`vectorops` containing `dotproduct`") and a
signature with types before or after names without a colon (Go, C). Tests:
`handshake_forms_tests.rs`, with the recorded statements as
`tests/fixtures/handshake/two-symbols-in-one-provision.json`.

## Amendment (2026-10-06, third): a bare file name in any language is a path

A path in a sentence is set aside, not read as a claim about a symbol. An anchor with no `/`
was a path only when its extension was on a short list (`rs`, `py`, `toml`, `json`, `md`,
`txt`, `js`, `ts`, `go`, `c`, `h`, `cpp`, `yaml`, `yml`, `lock`). Every other file name
(`serve_local.sh`, `config.rb`, `Config.java`, `Config.cs`, `config.php`, `App.tsx`,
`pom.xml`, `build.gradle`, `setup.cfg`) was read as a claim about the subject. An expecter
that wrote "I assume you provide `parse_config` in `config.rb`" then needed the provider to
repeat `config.rb`, and two parties that agreed on everything were held for a disagreement.
The same sentence with `config.py` passed.

- **The rule.** An anchor is a path when it holds a `/`, or its extension is that of any
  language kGraf reads (`kgraf_graph::Language::from_path`: Rust, Python, TypeScript and TSX,
  JavaScript, Go, Java, C, C++, Ruby, C#, PHP, Markdown) or of a script, build or
  configuration file (`sh`, `bash`, `zsh`, `ps1`, `bat`, `kt`, `kts`, `swift`, `scala`,
  `xml`, `gradle`, `csproj`, `sln`, `cfg`, `ini`, `env`, `mod`, `sum`, `cmake`, `mk`, `sql`,
  `html`, `css`, and the like).
- **A dotted name that names a surface symbol is still that symbol.** `np.sum` read as a file
  by its extension resolves to the surface symbol `sum` when the surface holds one, as an
  identifier path does; otherwise it is set aside.

Test: `kgraf-orchestrator` `checks.rs`
`a_bare_file_name_in_any_language_is_a_path_not_a_claim` (nineteen file names; seventeen
held before).

## Amendment (2026-10-06, fourth): a Go or C-family signature is compared by its types

A signature anchor was read only in the Rust, Python and TypeScript forms (`fn f(x: T) -> R`,
`def f(x: T) -> R:`, `f(x: T): R`), where the types follow the names after a colon. A Go
signature (`func ParseConfig(text string) Config`) and a Java, C# or C one (`Config
parseConfig(String text)`) were not read as signatures and named no symbol. Two parties that
stated their sides as such signatures were each held for stating no claim, and the symbol as
unprovided, however well they agreed; a party that named the symbol bare and gave the
signature beside it had the signature compared as raw text, so other parameter names or Go's
grouped names (`a, b string` for `a string, b string`) disagreed.

- **Go.** `func Name(params) results`, with or without a method receiver: the parameters are
  all named (a name with no type takes the type of the next named one) or all unnamed, and the
  results are the text after the parameters.
- **C family.** `[modifiers] type name(params)`: access and storage modifiers (`public`,
  `static`, `final`, `virtual`, ...) are dropped, the words before the name are the return
  type, and each parameter's type is its words but the last (`const char *s` is `const char*`,
  `int a[]` is `int[]`). A word that starts a statement (`return`, `await`, `new`, ...) makes
  the anchor a call, not a signature; a C++ `const` or a Java `throws` clause after the
  parameters is allowed.
- Both reduce to the canonical `fn(types) -> result` the other forms use, so names do not
  matter and a changed type still disagrees. Not read: a C++ return type whose template
  arguments hold spaces.

Test: `kgraf-orchestrator` `checks.rs` `a_signature_in_every_language_is_compared_by_its_types`.
