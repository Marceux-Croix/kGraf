# Decision 0039: A provider call requires an unforgeable ledger token, so cost accounting is a compile error rather than a review rule

**Status:** Accepted.
**Amended by:** 0040 (corrects a claim)
**Related:** decisions 0006, 0007, 0008

---

## Context

kGraf's standing rule is that no call to a model provider may be issued
outside the accounting path: a code path that can reach a provider without
being counted is a defect, not an optimization. As written, that is a rule
humans enforce by reading diffs.

Three things make that insufficient here.

kGraf writes its own code. The whole product is worker agents editing this
repository. A rule that depends on a reviewer noticing will be violated by a
worker that had a good local reason to add a fast path, and the violation
will look like a reasonable change.

The failure is invisible. An uncounted call still works. The response is
correct, the feature ships, and the only symptom is that the live cost
readout, a headline of the product, is quietly low.

The precedent is bad. An invariant defended only by a comment, with no test
behind it, is an invariant already lost to one careless edit.

A token construction addresses this one step further than a test would: not
a test that would notice the violation after the fact, but a signature under
which the violation cannot be written at all.

The design also has to handle an interrupted call. At least one provider
states plainly that an interrupted stream may never deliver its usage data.
A request that billed but was not counted violates the accounting rule just
as surely as one that was never counted at all, and the tempting handling is
to treat a usage-less call as a zero-cost call. That would fold a hole into
the total while making the total look complete. So the row survives,
flagged, with the provider's request identifier, which is the handle a
reconciliation pass needs.

Deriving cost rather than storing it follows from a related requirement
that the raw usage response be persisted verbatim: several provider
protocols surveyed changed shape within a year, and the point of keeping the
raw response is to be able to re-derive cost after a pricing or schema
change. A stored cost column would make that impossible for exactly the
rows that needed it.

## Decision

The mandatory provider method takes a `&CallToken` as an argument.

`CallToken` has no public constructor. The only way to obtain one is from
the ledger's `begin` call, which writes the call's row before returning it.

Therefore a code path that reaches a provider without being counted does not
compile. There is no runtime check, no lint, and no convention to remember.

The token is also a guard. Dropping it without settling marks the call
unreconciled rather than removing it, and the ledger reports those.

Cost is not a stored column. It is derived at read time from a price table,
and an unpriced model is reported as excluded from the total rather than
costed at zero.

## Consequences

There is no negative test for this clause, and there cannot be. The tests
state the mechanism in a comment and test the consequence, that every begun
call is on the record, settled or flagged, because the failing program
cannot be expressed in the language at all. That is the strongest available
form of the guarantee and the weakest available form of its test, and the
tradeoff is worth naming.

Every adapter must settle every path it can return from. An HTTP error
settles as failed, because nothing billed. A substituted model or an
undecodable response settles as unreconciled, because the call billed and
its cost is real but unverified. Getting that distinction wrong turns the
unreconciled list, which a human works through, into noise.

The ledger is a serialization point: one mutex-guarded connection, since
there is no second path and every worker in the fleet settles through it.
Deliberate: the write is one small row per provider call, orders of
magnitude cheaper than the call itself. If it ever binds, the fix is
batching within the ledger, never a bypass.

A poisoned ledger mutex is recovered, not propagated. One panic mid-write
must not make every subsequent call unaccountable, which would be a worse
accounting violation than continuing against a connection whose last
statement may have been incomplete.

The unreconciled list is a product surface, not a debug log. It is what the
user is shown when spend exists that kGraf cannot attribute. Hiding it would
be the silent degradation the design otherwise refuses to allow.

## Alternatives considered

**A convention, documented in the provider trait's doc comment.** Rejected:
this is the comment-without-a-test pattern already known to fail silently.

**A wrapper client that every caller is told to use.** Better, and still
rejected: the inner client remains callable, so the bypass exists and is one
import away. A worker optimizing a hot path will find it.

**A runtime assertion: panic if a call is issued without an open ledger
row.** Rejected: it catches the violation in the worst place, in
production, after the spend.

**A test that greps the source for direct client construction.** Rejected
as brittle and easy to defeat accidentally, and it tests the source text
rather than the mechanism.

**Make the accounted call the only signature that exists.** Chosen.

## Reactivation trigger

A provider protocol that cannot report usage at all, which would force a
decision about whether to estimate or refuse. The current position is
refuse: an explicit "usage missing" error rather than an invented number.

**Amended 2026-10-02: a server failure on the model's own malformed tool call is retried within the
attempt.** An OpenAI-compatible server that cannot parse the model's tool-call output answers
HTTP 500 (Ollama: `XML syntax error on line N` or `error parsing tool call`). The adapter
recognizes that body and raises `ProviderError::ModelOutputMalformed`, a typed kind; the worker
loop reads the kind and never the body text. The round is sent again, at most twice in a row. Each
try is its own ledger call. The failed call stays unreconciled like any other 5xx (the model ran,
so it may have billed), and the retry settles on its own. The retry request is the earlier one with
one notice appended at the end ("Your previous reply contained a malformed tool call ..."), so the
provider's cached prefix is unchanged. After the bound, or on the last round, the attempt fails
and its error names the parse failure. The kind is not a route fault: the server is up. Any other
5xx keeps the earlier behavior: no retry, the row open, and only a transport failure moves the
route out of rotation.

**Amended 2026-10-02: a bare `EOF` is the same failure.** A planning call answered HTTP 500 with
`{"error":{"message":"EOF","type":"api_error",...}}`, which the marker list did not match, so the
attempt failed with no retry. The adapter now also parses the body and treats an `error.message`
that is exactly `EOF`, or contains `unexpected EOF`, as `ModelOutputMalformed`. The field is
read, never the text around it: `EOF` elsewhere in a body, a 4xx, or a non-JSON body stays a
plain HTTP error. Tests: `a_bare_eof_error_message_is_malformed_output`,
`eof_elsewhere_than_the_error_message_is_not_malformed_output`.

**Amended 2026-10-05: every JSON-asked orchestrator call handles a malformed answer the same
way, within the same bound.** Re-grounding had no retry, so one answer that a model ended with a normal stop and an unclosed string became a hold on the step. The `[ERROR HERE]` in such a reason is kGraf's own mark of where the parser stopped, not text from the provider. The bound is one constant,
`kgraf_provider::MAX_MALFORMED_OUTPUT_RETRIES` (2), read by the worker loop and by
`ModelCaller::ask_json`, which every single-shot JSON-asked orchestrator call now uses
(re-grounding, question answer, connection judgment, handshake, consumer test, challenge
review, interface review, question routing); the corpus interpretation loops with the same
bound. A malformed answer is one that is not the complete JSON object asked for, or a
`ModelOutputMalformed` from the server. A refused answer is asked again with the refusal
quoted after the request; a server parse failure gets the same request again. Each try is
its own ledger call, and every call is returned to the caller so the hold settles at all of
them. After the bound the error is `PlanError::MalformedAfterRetries`, which says how many
times the call was made. An answer cut at the output limit, a rule broken by a well-formed
answer, and every other provider error are not retried. The planner's contracts and
interfaces calls keep their own re-ask budgets, and a server parse failure is re-askable in
both. The OpenAI-compatible adapters now classify an `error` object sent after HTTP 200 (a
non-streamed body, a stream chunk, a Responses `error` or `response.failed` event) as the
5xx body is classified: one that names a model-output parse failure is `ModelOutputMalformed`
with status 200, any other is a `Decode` error; neither is ever answer text. Tests:
`a_re_grounding_answer_cut_mid_string_is_asked_again_and_the_step_runs`,
`a_re_grounding_malformed_every_time_holds_after_the_bound_and_says_so`,
`a_server_parse_failure_on_a_json_asked_call_is_asked_again_unchanged`,
`an_interpretation_answer_cut_mid_string_is_asked_again`,
`a_mid_stream_eof_error_chunk_is_malformed_output_not_answer_text`.

**Amended 2026-10-08: a tool call the server dropped without an error is sent again within the
attempt.** A local server (Ollama 0.34.4, `qwen3.6:35b`) that could not parse the model's tool
call also answered HTTP 200 with no tool call and `finish_reason: "stop"`, while
`completion_tokens` still counted the call: 1,659 tokens for 718 bytes of reasoning, and on
another worker 1,454 tokens for 507 bytes. The first reply read as an empty turn and then as an
end with no change; the second ended its attempt as completed with its last write missing. A
byte-level tokenizer writes at least one byte per token, so counted text output above the
delivered bytes (text, reasoning, tool names and arguments; reasoning counted apart is left out
on both sides), less 32 for template markers, is a floor of lost tokens
(`Response::undelivered_output_floor`). `Provider::undelivered_output_tokens` reports it where
the protocol's reply carries all of its text share: the Chat Completions adapter only; the
others answer `None`, since their protocols count hidden or summarized reasoning in the output.
A worker reply with no tool call and a floor of 256 or more is dropped (`RoundAbandoned`), and
the model gets one notice (`NoticeKind::LostOutput`) saying how many tokens were counted and how
many are missing, at most `MAX_LOST_OUTPUT_RETRIES` (2) times an attempt; after that the reply
is judged as it arrived. Each try is its own ledger call. Tests: `kgraf-provider`
`counted_output_the_reply_does_not_carry_is_a_floor_of_lost_tokens`,
`separately_counted_reasoning_is_never_counted_as_lost`,
`both_wrappers_pass_the_undelivered_output_question_to_the_adapter`; `kgraf-provider-openai`
`a_reply_whose_tool_call_the_server_dropped_shows_undelivered_output`; `kgraf-runtime`
`a_reply_the_server_cut_down_is_sent_again_not_taken_as_the_answer`,
`lost_output_retries_are_bounded_and_need_a_measuring_provider`; the scripted end-to-end scenario
`lost-output-resent`.
