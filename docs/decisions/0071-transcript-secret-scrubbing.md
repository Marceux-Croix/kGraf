# Decision 0071: Transcripts and traces are scrubbed of secrets before they are written or shown

**Status:** Accepted (2026-09-27). The amendments below are dated.
**Deciders:** the project maintainer.
**Related:** 0051 (MCP header values are read from the environment, never stored), 0056 and 0057 (the transcript is the memory that a resume and a compaction re-ground on), 0059 (the ledger stores token counts only), 0084 (build and test output). Code: `crates/kgraf-secrets`. The pattern rules are ported from gitleaks (MIT); see `third_party/gitleaks/UPSTREAM`.

This record exists so a later change to the worker loop, the ledger or an MCP transport does not open a path for a credential to reach disk, the shell or a model provider unredacted.

## Context

Worker transcripts (`.kgraf/runs/<run>/`, JSONL) and run traces carry whatever a tool printed or a model produced. A `read` of a `.env` file, a `curl` reply that echoes a token in a header, or a model quoting back something it was told all put a credential in the transcript. Before this decision nothing scrubbed it, so a credential could sit on disk (mode 0600, but in the clear) and appear verbatim in the shell. `kgraf-mcp` already removed a configured header value from what an MCP server echoes back over HTTP. That protects one transport only.

## Decision

1. **One scrubber, two sources of secrets, one typed marker.** `kgraf-secrets` replaces a secret with `[redacted:<kind>]`, never the value. The two sources are:
   - pattern rules: 43 rules ported from gitleaks' default set, covering the AI providers in kGraf's catalog, common cloud, version control and package registry credentials, and generic shapes (a JWT, a PEM private key, a `key=` assignment), plus one new rule for a bare `scheme://user:pass@host` URL;
   - an exact-secret registry of the credentials kGraf itself holds (a provider API key, an MCP header value, an MCP stdio server's environment values), matched by exact substring.

   A value shorter than 8 characters is not registered. Placeholders, one-word values, dates, short numbers, absolute paths, URLs without credentials and cloud regions are refused too, with a notice that names the kind and never the value.
2. **Two paths.** The first version scrubbed pattern rules into the text a model is shown. A security review found that this corrupted real code: a generic key rule matched ordinary source, and a patch built from scrubbed text would have written a marker into a file. So the model-facing path uses the exact-secret registry only. The persist and display path (the transcript on disk, the shell, run traces, agent-to-agent messages) uses the registry and the pattern rules. A persist-path marker reads `[redacted:<kind>; re-read the source for exact text]`, because a resumed run rebuilds its context from the persisted text.
3. **One call site, before anything else touches the event.** A worker event is scrubbed at the top of the loop's emit functions: before the sequence checker, before the next request is built, and before the event is persisted or shown. The match over event kinds has no wildcard arm, so a new event kind does not compile until someone decides how each path treats it.
4. **The ledger needs no change.** `calls.raw_usage` is the provider's usage object (token counts only) and `mcp_calls` stores metadata only, so neither can hold a credential.
5. **Cost.** One compiled `RegexSet` scans a text in one linear pass. On a 1 MB tool output it measured 1.5 ms with nothing to redact and 4.1 ms with one match (2026-09-27, one development machine). The set is ASCII-only, as gitleaks' matching is in effect, because Unicode case folding made the compiled program too large.
6. **Each rule is tested both ways:** a true positive shaped like the real secret, and a false positive that shares the keyword or prefix but not the shape. A git commit hash near the word "key" is not redacted.

## Options considered

- **Scrub only at write time and show the model raw text.** Rejected. A secret a tool printed would still enter the model's context and could return on a later turn through another event.
- **Scrub separately in the request builder and the transcript writer.** Rejected. Two call sites must stay byte-identical, or a resume's digest check diverges from what the model saw.
- **The full gitleaks set (about 222 rules).** Rejected for now. Each rule needs its own two tests, and most of the rest are single-vendor tokens outside kGraf's dependencies. The table is additive.
- **The `secretscan` crate.** Rejected. It is a command-line program with no library target, built to walk a checked-out repository, not to scan a string on every tool result.

## Consequences

- A new pattern rule is one table row plus its two tests. A new place where kGraf holds a credential registers it at the point it reads the value.
- A secret is found only when it appears whole in one scrubbed string. A credential split across two tool calls, or encoded (base64, reversed), passes through. A split across two streamed deltas of one round is closed for everything written and for a resumed rebuild. It stays open on the live event stream that a caller prints as it arrives.
- A transcript written before this decision replays unscrubbed on resume. Scrubbing changes what is written from now on. It does not migrate what is already on disk.
- A resumed request is rebuilt from the persisted, pattern-scrubbed text. It can differ from what the live model was sent and can miss the provider's prompt cache from the first marker on. This is accepted: an unscrubbed copy is not stored to avoid it. The resume digest compares the shared prefix, which holds no transcript text.

## Amendment (2026-09-29): a cleared value leaves the write-ahead log

Clearing a stored record asks the database to truncate its write-ahead log without waiting for
readers, since a waiting checkpoint stalled every writer while a viewer read. With a long-lived
reader that left the cleared value in the log until the last connection closed. Now, when the
non-waiting attempt is blocked, one background thread per store retries it every 200 ms for at
most 60 seconds. A failed retry returns at once, so no attempt holds the writer lock. A reader
that outlasts the bound is written to the local diagnostics file (code KG-0208). The store owns
the thread: dropping the store stops it within one interval and waits for it, so a short-lived
process neither loses the retry without a record nor holds a second connection through its own
last close, which would stop that close from checkpointing. A retry cut short by the drop makes
one last attempt and is recorded only when a reader is still in the way; one that cannot open
the database is recorded with its own reason. The retry connection never
creates a database file. The waiting checkpoint at the end of a run stays. Removal from the files remains best effort, delayed only
by a reader.

## Amendment (2026-10-01): secrets files a worker reads are scrubbed from its records

- **Which files.** A file whose name is `.env`, `.env.<anything>`, `<anything>.env`, `.envrc`,
  `.secrets`, `.dev.vars`, `secrets.json`, `secrets.yaml`, `secrets.yml`, `secrets.toml` or
  `credentials.json`, at any depth in the worktree (`kgraf_secrets::is_secrets_file_name`).
  Until now only the checkout-root `.env` was registered (decision 0084), so a worker that read
  `config/.env.local` left its values in its transcript.
- **When.** `read_file` reads one; `search_files` reads one while it searches; a shell command
  names one in its words (`cat config/.env.local`, `sh -c 'source .env.test'`,
  `--env-file=deploy/prod.env`), which kGraf then reads itself, confined to the worktree, before
  the command runs. Each happens before the read becomes a tool result.
- **What is registered.** The pairs `register_dotenv` would register (a credential-shaped name,
  or a value a pattern rule matches), read from dotenv lines and from YAML (`name: value`), TOML
  or ini (`name = value`) and flat JSON (`"name": "value"`) lines
  (`Scrubber::register_records_only`). Ordinary settings (`LOG_LEVEL=info`, `PORT=8080`) are
  not registered.
- **Records only.** The values join a second registry that only the persist path reads:
  transcripts, traces and run records lose them. The model-facing path is unchanged: the read's
  model copy keeps the values, so the worker still reads the file it asked for, as with any
  file. Build and test output, which is persist-scrubbed for the model too (decision 0084),
  loses them there as well. A value already registered for both paths stays there.
- **Limits.** A command that reads such a file without naming it (`grep -r`, a glob, a script
  that opens it) is not seen at the shell; its output keeps the values in records unless an
  earlier read registered them. A file with another name is not covered.

Tests: `kgraf-secrets` `a_secrets_files_values_are_scrubbed_from_records_only` (dotenv, YAML,
TOML and JSON markers removed by the persist scrub and kept by the model-facing one, an ordinary
setting kept, a read's stored copy scrubbed and its model copy whole),
`secrets_file_names_are_recognized`; `kgraf-runtime` `worker_loop.rs`
`a_secrets_file_a_worker_reads_is_scrubbed_from_its_transcript_only` (a worker reads
`config/.env.local` with the real `read_file`: its next request carries the marker, no persisted
event does); `kgraf-tools` `a_search_through_a_secrets_file_scrubs_its_values_from_records`,
`a_shell_read_of_a_secrets_file_is_scrubbed_from_records_only` (through the real sandbox).
