

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

## Amendment: secrets files a worker reads are scrubbed from its records

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
