//! Renders [`glossary::GLOSSARY`] as the Markdown in `docs/ERRORS.md`.
//! `src/bin/gen_errors_doc.rs` writes this to disk; `tests/errors_doc.rs`
//! checks the committed file still matches it.

use crate::glossary::{ErrorSpec, GLOSSARY};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The shared cause and action of a plain catch-all entry. An entry with both
/// is listed in one table at the end of the doc, not as a section of its own.
const CATCH_ALL_CAUSE: &str = "See the context string.";
const CATCH_ALL_ACTION: &str =
    "Share this diagnostics file; the context names the failing operation.";

fn is_plain_catch_all(e: &ErrorSpec) -> bool {
    e.code.ends_with("99") && e.likely_cause == CATCH_ALL_CAUSE && e.action == CATCH_ALL_ACTION
}

/// The generated Markdown, byte-for-byte what `docs/ERRORS.md` should hold.
/// Grouped by crate, each crate's entries in code order, so a diff against a
/// future glossary change stays small and readable.
pub fn render_markdown() -> String {
    let mut by_crate: BTreeMap<&str, Vec<&ErrorSpec>> = BTreeMap::new();
    let mut catch_alls: Vec<&ErrorSpec> = Vec::new();
    for e in GLOSSARY {
        if is_plain_catch_all(e) {
            catch_alls.push(e);
            continue;
        }
        by_crate.entry(e.crate_name).or_default().push(e);
    }
    for entries in by_crate.values_mut() {
        entries.sort_by_key(|e| e.code);
    }

    let mut out = String::new();
    out.push_str("# kGraf error glossary\n\n");
    out.push_str(
        "GENERATED FILE. Do not hand-edit. Run `cargo run -p kgraf-diag --bin \
         gen-errors-doc` after changing `crates/kgraf-diag/src/glossary.rs`, \
         which is the single source of truth (decision 0069). \
         `crates/kgraf-diag/tests/errors_doc.rs` fails the build if this file \
         drifts from that table.\n\n",
    );
    out.push_str(
        "Every error kGraf's local diagnostics file records carries one of these \
         `KG-nnnn` codes. `KG-xx99` is a crate's catch-all: an error variant \
         not yet given its own code still reaches the file under its crate's \
         catch-all. Most catch-alls are in one table at the end. Use `kgraf diag show` and `kgraf diag export` to \
         read or share this file.\n\n",
    );

    // "unknown" (KG-0000) first, since it is outside every crate's block, then
    // every crate block in name order (BTreeMap order).
    if let Some(entries) = by_crate.remove("unknown") {
        write_section(&mut out, "Unclassified", &entries);
    }
    for (crate_name, entries) in by_crate {
        write_section(&mut out, crate_name, &entries);
    }
    catch_alls.sort_by_key(|e| e.code);
    let _ = writeln!(out, "## Catch-all codes\n");
    let _ = writeln!(
        out,
        "Likely cause for every code below: {CATCH_ALL_CAUSE} What to do: {CATCH_ALL_ACTION}\n"
    );
    out.push_str("| Code | Name | Meaning |\n|---|---|---|\n");
    for e in catch_alls {
        let _ = writeln!(
            out,
            "| {} | {} | {} |",
            e.code,
            e.name.replace('|', "\\|"),
            e.meaning.replace('|', "\\|")
        );
    }
    out
}

fn write_section(out: &mut String, heading: &str, entries: &[&ErrorSpec]) {
    let _ = writeln!(out, "## {heading}\n");
    for e in entries {
        let _ = writeln!(out, "### {} {}\n", e.code, e.name);
        let _ = writeln!(out, "**Meaning:** {}\n", e.meaning);
        let _ = writeln!(out, "**Likely cause:** {}\n", e.likely_cause);
        let _ = writeln!(out, "**What to do:** {}\n", e.action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_appears_in_the_rendered_doc() {
        let doc = render_markdown();
        for e in GLOSSARY {
            assert!(doc.contains(e.code), "{} missing from rendered doc", e.code);
        }
    }

    #[test]
    fn rendering_is_deterministic() {
        assert_eq!(render_markdown(), render_markdown());
    }
}
