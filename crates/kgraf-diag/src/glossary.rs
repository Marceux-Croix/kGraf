//! The error glossary: the single source of truth for every
//! `KG-nnnn` code. `docs/ERRORS.md` is *generated* from [`GLOSSARY`] by
//! `src/bin/gen_errors_doc.rs`; `tests/errors_doc.rs` fails the build if the
//! committed doc and the table have drifted, so the two can never quietly
//! diverge.
//!
//! **Numbering.** Each crate with a `thiserror` type owns a hundred-block
//! (`KG-01xx` is `kgraf-core`, `KG-02xx` is `kgraf-graph`, and so on, see the
//! `crate_name` field for the mapping used here). `KG-xx99` in every block is
//! that crate's catch-all: everything the block does not name a specific code
//! for still gets a code, just not its own glossary entry.
//! `KG-0000` is the one code outside any block, used only when a caller cannot
//! even identify the owning crate.
//!
//! **Coverage.** This starts with the error variants a user is most likely to
//! hit directly (a missing credential, a bad path, a busy workspace, a refused
//! cap) across every crate that has an `error.rs` today, plus a catch-all for
//! the rest of that crate's variants and for crates instrumented later. A
//! variant not yet given its own code still reaches the file, under its
//! crate's catch-all; promoting it to a named code is additive and never
//! breaks a caller (see `crate_name` doc comment on why the block, not the
//! individual code, is the thing crates commit to).

/// One glossary entry. Every field is `&'static str` (no allocation, no
/// `Display` formatting): the point of a glossary is a fixed vocabulary, not
/// live text, so a caller can always find a code in this table with `==`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorSpec {
    pub code: &'static str,
    pub name: &'static str,
    /// The crate this code belongs to, for the doc's grouping and for the
    /// per-crate catch-all lookup (`catch_all_for`).
    pub crate_name: &'static str,
    pub meaning: &'static str,
    pub likely_cause: &'static str,
    pub action: &'static str,
}

/// The catch-all code for `crate_name`, or [`UNKNOWN`] when the crate has none
/// registered yet (a crate this instrumentation has not reached). Every
/// production call site should be able to do at least this well: name its own
/// crate, even where mapping the specific error variant is not yet worth the
/// entry.
pub fn catch_all_for(crate_name: &str) -> &'static ErrorSpec {
    GLOSSARY
        .iter()
        .find(|e| e.crate_name == crate_name && e.code.ends_with("99"))
        .unwrap_or(&UNKNOWN)
}

pub const UNKNOWN: ErrorSpec = ErrorSpec {
    code: "KG-0000",
    name: "Unclassified",
    crate_name: "unknown",
    meaning: "An error occurred and the code that recorded it could not name its own crate.",
    likely_cause: "A call site outside the crates this glossary covers, or a new crate not yet given a block.",
    action: "Read the context string; if this recurs, give the raising crate its own block in kgraf-diag::glossary.",
};

macro_rules! spec {
    ($code:literal, $name:literal, $crate_name:literal, $meaning:literal, $cause:literal, $action:literal) => {
        ErrorSpec {
            code: $code,
            name: $name,
            crate_name: $crate_name,
            meaning: $meaning,
            likely_cause: $cause,
            action: $action,
        }
    };
}

pub const GLOSSARY: &[ErrorSpec] = &[
    UNKNOWN,
    // KG-01xx: kgraf-core (price tables, usage normalization, cost).
    spec!(
        "KG-0101",
        "Price Missing",
        "kgraf-core",
        "A model has no entry in the price table, so its cost is unknown. The call is still made and ledgered; its spend is reported as unpriced.",
        "A new or renamed model used before the price table was updated.",
        "Add the model to the price table, or use a priced model."
    ),
    spec!(
        "KG-0199",
        "Core (Other)",
        "kgraf-core",
        "An unclassified error from kgraf-core (cost, usage normalization, litellm price sync).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-02xx: kgraf-graph (index, query, layout, ranking).
    spec!(
        "KG-0201",
        "Graph IO",
        "kgraf-graph",
        "The graph store or a source file could not be read or is not valid UTF-8.",
        "A file was moved, deleted, or is binary where kGraf expected text.",
        "Re-run `kgraf index`; if it recurs, check the named path's permissions and encoding."
    ),
    spec!(
        "KG-0202",
        "Unknown Symbol",
        "kgraf-graph",
        "A symbol name does not resolve to any node in the graph.",
        "The name is misspelled, or the graph is stale relative to the working tree.",
        "Re-run `kgraf index`, then retry the lookup."
    ),
    spec!(
        "KG-0203",
        "Bad Query",
        "kgraf-graph",
        "A structural query pattern did not parse.",
        "A typo in the pattern, or a kind/relation name this graph does not use.",
        "Check `kgraf query --help` for pattern syntax."
    ),
    spec!(
        "KG-0204",
        "Schema Mismatch",
        "kgraf-graph",
        "The on-disk graph database's schema does not match this build of kGraf.",
        "The graph was built by an older or newer kGraf version.",
        "Delete and rebuild the graph database with `kgraf index`."
    ),
    spec!(
        "KG-0205",
        "No Index",
        "kgraf-graph",
        "A command that only reads the graph found no index file at the project's index path.",
        "The project was never indexed, or the index path points somewhere else.",
        "Run `kgraf index` in the project first."
    ),
    spec!(
        "KG-0206",
        "Ambiguous Symbol",
        "kgraf-graph",
        "A symbol name matches more than one node and none is an exact qualified-name match.",
        "The short name is defined in several files or scopes.",
        "Retry with a qualified name from the candidate list in the message."
    ),
    spec!(
        "KG-0207",
        "Stale File",
        "kgraf-graph",
        "A node's file changed after it was indexed, so its recorded lines may name other text.",
        "The file was edited since the last `kgraf index`, and the change has not been indexed.",
        "Run `kgraf index`, then retry."
    ),
    spec!(
        "KG-0208",
        "WAL Residue Pending",
        "kgraf-graph",
        "A removed project record may still be in the index's write-ahead log: a reader held an older snapshot for the whole retry window, so the log could not be truncated.",
        "A long-lived read transaction on the index, such as a viewer or a stuck process.",
        "Close the reader. The log is truncated at the next checkpoint, at the end of a run, or when the last connection closes."
    ),
    spec!(
        "KG-0299",
        "Graph (Other)",
        "kgraf-graph",
        "An unclassified error from kgraf-graph (layout, ranking, version history, facts).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-03xx: kgraf-provider (adapters, accounting, rate limits).
    spec!(
        "KG-0301",
        "Missing Credential",
        "kgraf-provider",
        "No credential is configured for a provider route.",
        "The route's environment variable (for example a `_API_KEY`) is not set.",
        "Set the credential kGraf named, or remove the route."
    ),
    spec!(
        "KG-0302",
        "Provider HTTP Error",
        "kgraf-provider",
        "A provider adapter received a non-success HTTP response.",
        "An invalid or expired credential, a bad request, or a provider outage.",
        "Check the credential and the request; retry if the provider reports an outage."
    ),
    spec!(
        "KG-0303",
        "Rate Limited",
        "kgraf-provider",
        "A provider or kGraf's own rate limiter refused a call.",
        "Too many calls too fast for the configured or provider-side limit.",
        "Wait and retry, or raise the configured rate limit if the provider allows it."
    ),
    spec!(
        "KG-0304",
        "Model Unsupported",
        "kgraf-provider",
        "The requested model is not one an adapter's route can serve.",
        "A typo in the model name, or a model retired by the provider.",
        "Use one of the models the adapter lists as available."
    ),
    spec!(
        "KG-0305",
        "Usage Missing",
        "kgraf-provider",
        "A provider response carried no usage, so the call cannot be attributed in the ledger.",
        "A provider API change, or a non-standard response from a self-hosted endpoint.",
        "Report this: every token must be attributed to the ledger, so this must be fixed, not ignored."
    ),
    spec!(
        "KG-0306",
        "Turn Not Usable",
        "kgraf-provider",
        "The provider ended the turn with a stop reason that is not an answer, such as a refusal or an exceeded context window. The call was billed and recorded.",
        "The model refused the request, or the conversation no longer fits its context window.",
        "Read the stop reason in the message; shorten the context or rephrase the task, then retry."
    ),
    spec!(
        "KG-0399",
        "Provider (Other)",
        "kgraf-provider",
        "An unclassified error from kgraf-provider (transport, decode, billing, cancellation).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-04xx: kgraf-orchestrator (dispatch, contracts, steering, compaction).
    spec!(
        "KG-0401",
        "No Routes Configured",
        "kgraf-orchestrator",
        "A run was asked to start with no provider routes given.",
        "`--route route:model` was omitted.",
        "Give at least one --route route:model."
    ),
    spec!(
        "KG-0402",
        "Run Already Exists",
        "kgraf-orchestrator",
        "A run with this id is already recorded; it cannot be started fresh.",
        "The run id was reused, often by re-running the same command instead of --resume.",
        "Use --resume to continue it, or choose a new run id."
    ),
    spec!(
        "KG-0403",
        "Budget Refused",
        "kgraf-orchestrator",
        "A call was refused because it would exceed a configured spend cap.",
        "The per-task, per-run, or per-hour cap was reached.",
        "Raise the cap if the spend is expected, or let the run stop as designed."
    ),
    spec!(
        "KG-0499",
        "Orchestrator (Other)",
        "kgraf-orchestrator",
        "An unclassified error from kgraf-orchestrator (decomposition, contracts, steering, compaction).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-05xx: kgraf-host (the shared run driver behind the CLI and the shell).
    spec!(
        "KG-0501",
        "Route Not Ready",
        "kgraf-host",
        "A configured route is missing a credential, an unknown catalog id, or has no price; or a local route's server (Ollama, llama.cpp, System One, TEI) accepts no connection at run start.",
        "The environment is not fully set up for the routes a run names, or the local model server is not running.",
        "Run `kgraf providers` (or the shell's readiness check) and fix what it names; for an unreachable local route, start its server (for Ollama, `ollama serve`) or remove the route."
    ),
    spec!(
        "KG-0502",
        "Run Not Found",
        "kgraf-host",
        "No run-state record exists for the given run id.",
        "The run id is misspelled, or its state directory was removed.",
        "Check the run id, or start a fresh run instead of resuming."
    ),
    spec!(
        "KG-0599",
        "Host (Other)",
        "kgraf-host",
        "An unclassified error from kgraf-host (the shared run driver).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-06xx: kgraf-sandbox (worktrees, kernel isolation).
    spec!(
        "KG-0601",
        "Workspace Busy",
        "kgraf-sandbox",
        "A worktree is in use and cannot be reopened or removed right now.",
        "Another process (or a crashed one that left a lock) still holds it.",
        "Wait for the other process, or check for a stale lock if none is running."
    ),
    spec!(
        "KG-0602",
        "Isolation Unavailable",
        "kgraf-sandbox",
        "The kernel isolation this platform needs could not be set up, so kGraf refuses to run unconfined.",
        "A missing OS feature, an unsupported platform, or insufficient privilege.",
        "See the sandbox's own [sandbox] log line for the missing feature; this is refused by design, not a bug to work around."
    ),
    spec!(
        "KG-0603",
        "Git Failed",
        "kgraf-sandbox",
        "A `git` command the worktree lifecycle depends on failed or is not on PATH.",
        "git is not installed, or the repository state is unexpected (detached HEAD, a missing branch).",
        "Install or update git; check the named git command's stderr in the context."
    ),
    spec!(
        "KG-0699",
        "Sandbox (Other)",
        "kgraf-sandbox",
        "An unclassified error from kgraf-sandbox (worktrees or kernel isolation).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-07xx: kgraf-schedule.
    spec!(
        "KG-0701",
        "Invalid Time Zone",
        "kgraf-schedule",
        "A schedule named a time zone that is not a valid IANA zone.",
        "A typo, or an offset (`+05:00`) given where a zone name is required.",
        "Use an IANA zone name, for example `America/New_York`."
    ),
    spec!(
        "KG-0702",
        "Stale Claim",
        "kgraf-schedule",
        "A scheduled run's claim was already taken or expired before this process acted on it.",
        "Two processes raced for the same due run, or a claim's lease expired.",
        "Usually harmless: the run either already started elsewhere or will be claimed on the next tick."
    ),
    spec!(
        "KG-0799",
        "Schedule (Other)",
        "kgraf-schedule",
        "An unclassified error from kgraf-schedule.",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-08xx: kgraf-mcp.
    spec!(
        "KG-0801",
        "MCP OAuth Failed",
        "kgraf-mcp",
        "A hosted MCP server's OAuth handshake did not complete.",
        "An expired or revoked token, or a server-side authorization change.",
        "Re-run the server's pairing flow from Settings."
    ),
    spec!(
        "KG-0899",
        "MCP (Other)",
        "kgraf-mcp",
        "An unclassified error from kgraf-mcp (client, config, registration).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-09xx: kgraf-shell.
    spec!(
        "KG-0901",
        "No Adapter",
        "kgraf-shell",
        "The desktop shell could not find a rendering adapter (GPU or CPU fallback).",
        "No compatible graphics backend on this machine.",
        "Check the shell's --probe output for what backends were tried."
    ),
    spec!(
        "KG-0902",
        "Settings File Invalid",
        "kgraf-shell",
        "The settings file could not be parsed.",
        "A hand edit broke its syntax, or it was written by an incompatible version.",
        "Fix or remove the settings file; the shell recreates a default one."
    ),
    spec!(
        "KG-0999",
        "Shell (Other)",
        "kgraf-shell",
        "An unclassified error from kgraf-shell (window, tray, project state).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-10xx: kgraf-cli.
    spec!(
        "KG-1099",
        "CLI (Other)",
        "kgraf-cli",
        "An unclassified error surfaced at the `kgraf` command line.",
        "See the context string; the failing subcommand is named separately in the same event.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-11xx: kgraf-ingest.
    spec!(
        "KG-1101",
        "File Too Large",
        "kgraf-ingest",
        "A document exceeded the ingest size limit and was skipped.",
        "A large PDF, DOCX, or text file over the configured byte limit.",
        "Raise the ingest limit if the file must be read, or exclude it."
    ),
    spec!(
        "KG-1102",
        "Unreadable Document",
        "kgraf-ingest",
        "A PDF or DOCX could not be parsed (malformed file, or the parser panicked).",
        "A corrupted, encrypted, or non-standard document.",
        "Open the file in its native application to confirm it is not corrupted, then retry."
    ),
    spec!(
        "KG-1199",
        "Ingest (Other)",
        "kgraf-ingest",
        "An unclassified error from kgraf-ingest.",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-12xx: kgraf-interpret.
    spec!(
        "KG-1201",
        "Nothing To Interpret",
        "kgraf-interpret",
        "The corpus root holds no documents and no code, and no instruction was given.",
        "An empty or wrong directory was passed to `kgraf orchestrate --corpus`.",
        "Point at a directory with documents or code, or give an instruction directly."
    ),
    spec!(
        "KG-1202",
        "No Roadmap Found",
        "kgraf-interpret",
        "`kgraf plan --from-roadmap` found no roadmap document to read.",
        "None of ROADMAP, TODO, PLAN, MILESTONES, BACKLOG, or TASKS exists in the expected places.",
        "Name a roadmap document explicitly with --source."
    ),
    spec!(
        "KG-1299",
        "Interpret (Other)",
        "kgraf-interpret",
        "An unclassified error from kgraf-interpret (reading, plans, model calls).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-13xx: kgraf-runtime.
    spec!(
        "KG-1399",
        "Runtime (Other)",
        "kgraf-runtime",
        "An unclassified error from kgraf-runtime (the worker loop, briefs, events).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-14xx: kgraf-tools.
    spec!(
        "KG-1401",
        "Tool Access Refused",
        "kgraf-tools",
        "A tool call was refused an access it asked for.",
        "The call asked to leave the workspace root, or asked for a hard-linked path.",
        "This is a sandbox boundary working as designed; adjust the tool call's path."
    ),
    spec!(
        "KG-1402",
        "Tool Timed Out",
        "kgraf-tools",
        "A tool call did not complete within its timeout.",
        "A slow command, a hung process, or a timeout set too low for the task.",
        "Retry, or raise the tool's timeout if the task is legitimately slow."
    ),
    spec!(
        "KG-1499",
        "Tools (Other)",
        "kgraf-tools",
        "An unclassified error from kgraf-tools (registry, access, approvals).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-15xx: kgraf-decide.
    spec!(
        "KG-1599",
        "Decide (Other)",
        "kgraf-decide",
        "An unclassified error from kgraf-decide.",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-16xx: kgraf-fleet.
    spec!(
        "KG-1699",
        "Fleet (Other)",
        "kgraf-fleet",
        "An unclassified error from kgraf-fleet (the provider catalog).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-17xx: kgraf-instructions.
    spec!(
        "KG-1799",
        "Instructions (Other)",
        "kgraf-instructions",
        "An unclassified error from kgraf-instructions (skill and instruction ingest).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-18xx: kgraf-session.
    spec!(
        "KG-1899",
        "Session (Other)",
        "kgraf-session",
        "An unclassified error from kgraf-session.",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-19xx: kgraf-galaxy.
    spec!(
        "KG-1901",
        "Scene Too Large",
        "kgraf-galaxy",
        "The graph is too large for the GPU to render at once.",
        "A very large repository at full detail with no collapsing applied.",
        "Collapse more of the graph, or use CPU mode, until GPU chunking lands."
    ),
    spec!(
        "KG-1999",
        "Galaxy (Other)",
        "kgraf-galaxy",
        "An unclassified error from kgraf-galaxy (the renderer).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
    // KG-99xx: kgraf-diag itself.
    spec!(
        "KG-9901",
        "Diag Internal",
        "kgraf-diag",
        "The diagnostics writer itself failed (a record could not be serialized or written).",
        "A disk-full or permissions problem on the diagnostics file's directory.",
        "Free disk space or fix permissions on the kGraf data directory; diagnostics resume once it writes again."
    ),
    spec!(
        "KG-9999",
        "Diag (Other)",
        "kgraf-diag",
        "An unclassified error from kgraf-diag (the diagnostics writer, reader, or exporter).",
        "See the context string.",
        "Share this diagnostics file; the context names the failing operation."
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn codes_are_unique() {
        let mut seen = HashSet::new();
        for e in GLOSSARY {
            assert!(seen.insert(e.code), "duplicate code: {}", e.code);
        }
    }

    #[test]
    fn codes_follow_kg_nnnn() {
        for e in GLOSSARY {
            assert!(e.code.starts_with("KG-"), "{} missing KG- prefix", e.code);
            let digits = &e.code[3..];
            assert_eq!(digits.len(), 4, "{} is not KG-nnnn", e.code);
            assert!(
                digits.chars().all(|c| c.is_ascii_digit()),
                "{} has non-digit suffix",
                e.code
            );
        }
    }

    #[test]
    fn every_crate_block_has_a_catch_all() {
        let crates: HashSet<&str> = GLOSSARY.iter().map(|e| e.crate_name).collect();
        for crate_name in crates {
            if crate_name == "unknown" {
                continue;
            }
            assert!(
                GLOSSARY
                    .iter()
                    .any(|e| e.crate_name == crate_name && e.code.ends_with("99")),
                "{crate_name} has no xx99 catch-all"
            );
        }
    }

    #[test]
    fn catch_all_for_falls_back_to_unknown() {
        assert_eq!(catch_all_for("no-such-crate"), &UNKNOWN);
        assert_eq!(catch_all_for("kgraf-graph").code, "KG-0299");
    }
}
