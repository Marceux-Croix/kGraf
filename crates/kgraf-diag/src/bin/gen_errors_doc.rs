//! Regenerates `docs/ERRORS.md` from `kgraf_diag::glossary::GLOSSARY`.
//!
//! Run after changing the glossary:
//! `cargo run -p kgraf-diag --bin gen-errors-doc`
//!
//! `crates/kgraf-diag/tests/errors_doc.rs` checks the committed file still
//! matches this output and fails the build otherwise, so this binary is the
//! prescribed fix for that failure, not an optional convenience.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // `crates/kgraf-diag` -> repository root -> `docs/ERRORS.md`.
    let doc_path = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("kgraf-diag is two directories under the workspace root")
        .join("docs")
        .join("ERRORS.md");
    let rendered = kgraf_diag::docs::render_markdown();
    std::fs::write(&doc_path, rendered)
        .unwrap_or_else(|e| panic!("cannot write {}: {e}", doc_path.display()));
    println!("wrote {}", doc_path.display());
}
