# kGraf error glossary

GENERATED FILE. Do not hand-edit. Run `cargo run -p kgraf-diag --bin gen-errors-doc` after changing `crates/kgraf-diag/src/glossary.rs`, which is the single source of truth (decision 0069). `crates/kgraf-diag/tests/errors_doc.rs` fails the build if this file drifts from that table.

Every error kGraf's local diagnostics file records carries one of these `KG-nnnn` codes. `KG-xx99` is a crate's catch-all: an error variant not yet given its own code still reaches the file under its crate's catch-all. Most catch-alls are in one table at the end. Use `kgraf diag show` and `kgraf diag export` to read or share this file.

## Unclassified

### KG-0000 Unclassified

**Meaning:** An error occurred and the code that recorded it could not name its own crate.

**Likely cause:** A call site outside the crates this glossary covers, or a new crate not yet given a block.

**What to do:** Read the context string; if this recurs, give the raising crate its own block in kgraf-diag::glossary.

## kgraf-cli

### KG-1099 CLI (Other)

**Meaning:** An unclassified error surfaced at the `kgraf` command line.

**Likely cause:** See the context string; the failing subcommand is named separately in the same event.

**What to do:** Share this diagnostics file; the context names the failing operation.

## kgraf-core

### KG-0101 Price Missing

**Meaning:** A model has no entry in the price table, so its cost is unknown. The call is still made and ledgered; its spend is reported as unpriced.

**Likely cause:** A new or renamed model used before the price table was updated.

**What to do:** Add the model to the price table, or use a priced model.

## kgraf-diag

### KG-9901 Diag Internal

**Meaning:** The diagnostics writer itself failed (a record could not be serialized or written).

**Likely cause:** A disk-full or permissions problem on the diagnostics file's directory.

**What to do:** Free disk space or fix permissions on the kGraf data directory; diagnostics resume once it writes again.

## kgraf-galaxy

### KG-1901 Scene Too Large

**Meaning:** The graph is too large for the GPU to render at once.

**Likely cause:** A very large repository at full detail with no collapsing applied.

**What to do:** Collapse more of the graph, or use CPU mode.

## kgraf-graph

### KG-0201 Graph IO

**Meaning:** The graph store or a source file could not be read or is not valid UTF-8.

**Likely cause:** A file was moved, deleted, or is binary where kGraf expected text.

**What to do:** Re-run `kgraf index`; if it recurs, check the named path's permissions and encoding.

### KG-0202 Unknown Symbol

**Meaning:** A symbol name does not resolve to any node in the graph.

**Likely cause:** The name is misspelled, or the graph is stale relative to the working tree.

**What to do:** Re-run `kgraf index`, then retry the lookup.

### KG-0203 Bad Query

**Meaning:** A structural query pattern did not parse.

**Likely cause:** A typo in the pattern, or a kind/relation name this graph does not use.

**What to do:** Check `kgraf query --help` for pattern syntax.

### KG-0204 Schema Mismatch

**Meaning:** The on-disk graph database's schema does not match this build of kGraf.

**Likely cause:** The graph was built by an older or newer kGraf version.

**What to do:** Delete and rebuild the graph database with `kgraf index`.

### KG-0205 No Index

**Meaning:** A command that only reads the graph found no index file at the project's index path.

**Likely cause:** The project was never indexed, or the index path points somewhere else.

**What to do:** Run `kgraf index` in the project first.

### KG-0206 Ambiguous Symbol

**Meaning:** A symbol name matches more than one node and none is an exact qualified-name match.

**Likely cause:** The short name is defined in several files or scopes.

**What to do:** Retry with a qualified name from the candidate list in the message.

### KG-0207 Stale File

**Meaning:** A node's file changed after it was indexed, so its recorded lines may name other text.

**Likely cause:** The file was edited since the last `kgraf index`, and the change has not been indexed.

**What to do:** Run `kgraf index`, then retry.

### KG-0208 WAL Residue Pending

**Meaning:** A removed project record may still be in the index's write-ahead log: a reader held an older snapshot for the whole retry window, so the log could not be truncated.

**Likely cause:** A long-lived read transaction on the index, such as a viewer or a stuck process.

**What to do:** Close the reader. The log is truncated at the next checkpoint, at the end of a run, or when the last connection closes.

## kgraf-host

### KG-0501 Route Not Ready

**Meaning:** A configured route is missing a credential, an unknown catalog id, or has no price; or a local route's server (Ollama, llama.cpp, System One, TEI) accepts no connection at run start.

**Likely cause:** The environment is not fully set up for the routes a run names, or the local model server is not running.

**What to do:** Run `kgraf providers` (or the shell's readiness check) and fix what it names; for an unreachable local route, start its server (for Ollama, `ollama serve`), point the route at the server's address with `KGRAF_<ROUTE>_BASE_URL` (for example `KGRAF_OLLAMA_BASE_URL`), or remove the route.

### KG-0502 Run Not Found

**Meaning:** No run-state record exists for the given run id.

**Likely cause:** The run id is misspelled, or its state directory was removed.

**What to do:** Check the run id, or start a fresh run instead of resuming.

## kgraf-ingest

### KG-1101 File Too Large

**Meaning:** A document exceeded the ingest size limit and was skipped.

**Likely cause:** A large PDF, DOCX, or text file over the configured byte limit.

**What to do:** Raise the ingest limit if the file must be read, or exclude it.

### KG-1102 Unreadable Document

**Meaning:** A PDF or DOCX could not be parsed (malformed file, or the parser panicked).

**Likely cause:** A corrupted, encrypted, or non-standard document.

**What to do:** Open the file in its native application to confirm it is not corrupted, then retry.

## kgraf-interpret

### KG-1201 Nothing To Interpret

**Meaning:** The corpus root holds no documents and no code, and no instruction was given.

**Likely cause:** An empty or wrong project directory was used with `kgraf orchestrate --corpus`.

**What to do:** Point at a directory with documents or code, or give an instruction directly.

### KG-1202 No Roadmap Found

**Meaning:** `kgraf plan --from-roadmap` found no roadmap document to read.

**Likely cause:** None of ROADMAP, TODO, PLAN, MILESTONES, BACKLOG, or TASKS exists in the expected places.

**What to do:** Name a roadmap document explicitly with --source.

## kgraf-mcp

### KG-0801 MCP OAuth Failed

**Meaning:** A hosted MCP server's OAuth handshake did not complete.

**Likely cause:** An expired or revoked token, or a server-side authorization change.

**What to do:** Reserved: no kGraf code raises this code today.

## kgraf-orchestrator

### KG-0401 No Routes Configured

**Meaning:** A run was asked to start with no provider routes given.

**Likely cause:** `--route route:model` was omitted.

**What to do:** Give at least one --route route:model.

### KG-0402 Run Already Exists

**Meaning:** A run with this id is already recorded; it cannot be started fresh.

**Likely cause:** The run id was reused, often by re-running the same command instead of --resume.

**What to do:** Use --resume to continue it, or choose a new run id.

### KG-0403 Spend Not Recorded

**Meaning:** A model call's spend could not be recorded, so the orchestrator stopped the step. kGraf sets no spend limit.

**Likely cause:** The orchestrator's interpretation call returned no ledger row, so its spend is unknown.

**What to do:** Run `kgraf ledger verify` and check that the provider route records its calls. Share this diagnostics file if it recurs.

## kgraf-provider

### KG-0301 Missing Credential

**Meaning:** No credential is configured for a provider route.

**Likely cause:** The route's environment variable (for example a `_API_KEY`) is not set.

**What to do:** Set the credential kGraf named, or remove the route.

### KG-0302 Provider HTTP Error

**Meaning:** A provider adapter received a non-success HTTP response.

**Likely cause:** An invalid or expired credential, a bad request, or a provider outage.

**What to do:** Check the credential and the request; retry if the provider reports an outage.

### KG-0303 Rate Limited

**Meaning:** A provider or kGraf's own rate limiter refused a call.

**Likely cause:** Too many calls too fast for the configured or provider-side limit.

**What to do:** Wait and retry, or raise the configured rate limit if the provider allows it.

### KG-0304 Model Unsupported

**Meaning:** The requested model is not one an adapter's route can serve.

**Likely cause:** A typo in the model name, or a model retired by the provider.

**What to do:** Use one of the models the adapter lists as available.

### KG-0305 Usage Missing

**Meaning:** A provider response carried no usage, so the call cannot be attributed in the ledger.

**Likely cause:** A provider API change, or a non-standard response from a self-hosted endpoint.

**What to do:** Report this: every token must be attributed to the ledger, so this must be fixed, not ignored.

### KG-0306 Turn Not Usable

**Meaning:** The provider ended the turn with a stop reason that is not an answer, such as a refusal or an exceeded context window. The call was billed and recorded.

**Likely cause:** The model refused the request, or the conversation no longer fits its context window.

**What to do:** Read the stop reason in the message; shorten the context or rephrase the task, then retry.

## kgraf-sandbox

### KG-0601 Workspace Busy

**Meaning:** A worktree is in use and cannot be reopened or removed right now.

**Likely cause:** Another process (or a crashed one that left a lock) still holds it.

**What to do:** Wait for the other process, or check for a stale lock if none is running.

### KG-0602 Isolation Unavailable

**Meaning:** The kernel isolation this platform needs could not be set up, so kGraf refuses to run unconfined.

**Likely cause:** A missing OS feature, an unsupported platform, or insufficient privilege.

**What to do:** See the sandbox's own [sandbox] log line for the missing feature; this is refused by design, not a bug to work around.

### KG-0603 Git Failed

**Meaning:** A `git` command the worktree lifecycle depends on failed or is not on PATH.

**Likely cause:** git is not installed, or the repository state is unexpected (detached HEAD, a missing branch).

**What to do:** Install or update git; check the named git command's stderr in the context.

## kgraf-schedule

### KG-0701 Invalid Time Zone

**Meaning:** A schedule named a time zone that is not a valid IANA zone.

**Likely cause:** A typo, or an offset (`+05:00`) given where a zone name is required.

**What to do:** Use an IANA zone name, for example `America/New_York`.

### KG-0702 Stale Claim

**Meaning:** A scheduled run's claim was already taken or expired before this process acted on it.

**Likely cause:** Two processes raced for the same due run, or a claim's lease expired.

**What to do:** Usually harmless: the run either already started elsewhere or will be claimed on the next tick.

## kgraf-shell

### KG-0901 No Adapter

**Meaning:** The desktop shell could not find a rendering adapter (GPU or CPU fallback).

**Likely cause:** No compatible graphics backend on this machine.

**What to do:** Check the shell's --probe output for what backends were tried.

### KG-0902 Settings File Invalid

**Meaning:** The settings file could not be parsed.

**Likely cause:** A hand edit broke its syntax, or it was written by an incompatible version.

**What to do:** Fix or remove the settings file; the shell recreates a default one.

## kgraf-tools

### KG-1401 Tool Access Refused

**Meaning:** A tool call was refused an access it asked for.

**Likely cause:** The call asked to leave the workspace root, or asked for a hard-linked path.

**What to do:** This is a sandbox boundary working as designed; adjust the tool call's path.

### KG-1402 Tool Timed Out

**Meaning:** A tool call did not complete within its timeout.

**Likely cause:** A slow command, a hung process, or a timeout set too low for the task.

**What to do:** Retry, or raise the tool's timeout if the task is legitimately slow.

## Catch-all codes

Likely cause for every code below: See the context string. What to do: Share this diagnostics file; the context names the failing operation.

| Code | Name | Meaning |
|---|---|---|
| KG-0199 | Core (Other) | An unclassified error from kgraf-core (cost, usage normalization, litellm price sync). |
| KG-0299 | Graph (Other) | An unclassified error from kgraf-graph (layout, ranking, version history, facts). |
| KG-0399 | Provider (Other) | An unclassified error from kgraf-provider (transport, decode, billing, cancellation). |
| KG-0499 | Orchestrator (Other) | An unclassified error from kgraf-orchestrator (decomposition, contracts, steering, compaction). |
| KG-0599 | Host (Other) | An unclassified error from kgraf-host (the shared run driver). |
| KG-0699 | Sandbox (Other) | An unclassified error from kgraf-sandbox (worktrees or kernel isolation). |
| KG-0799 | Schedule (Other) | An unclassified error from kgraf-schedule. |
| KG-0899 | MCP (Other) | An unclassified error from kgraf-mcp (client, config, registration). |
| KG-0999 | Shell (Other) | An unclassified error from kgraf-shell (window, tray, project state). |
| KG-1199 | Ingest (Other) | An unclassified error from kgraf-ingest. |
| KG-1299 | Interpret (Other) | An unclassified error from kgraf-interpret (reading, plans, model calls). |
| KG-1399 | Runtime (Other) | An unclassified error from kgraf-runtime (the worker loop, briefs, events). |
| KG-1499 | Tools (Other) | An unclassified error from kgraf-tools (registry, access, approvals). |
| KG-1599 | Decide (Other) | An unclassified error from kgraf-decide. |
| KG-1699 | Fleet (Other) | An unclassified error from kgraf-fleet (the provider catalog). |
| KG-1799 | Instructions (Other) | An unclassified error from kgraf-instructions (skill and instruction ingest). |
| KG-1899 | Session (Other) | An unclassified error from kgraf-session. |
| KG-1999 | Galaxy (Other) | An unclassified error from kgraf-galaxy (the renderer). |
| KG-9999 | Diag (Other) | An unclassified error from kgraf-diag (the diagnostics writer, reader, or exporter). |
