//! `docs/ERRORS.md` is generated from `kgraf_diag::glossary::GLOSSARY`.
//! This test fails the build the moment the two drift,
//! rather than letting the doc silently go stale.

use std::path::PathBuf;

fn doc_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("kgraf-diag is two directories under the workspace root")
        .join("docs")
        .join("ERRORS.md")
}

#[test]
fn docs_errors_matches_the_generated_glossary() {
    let path = doc_path();
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let generated = kgraf_diag::docs::render_markdown();
    assert_eq!(
        committed,
        generated,
        "{} is out of date; run `cargo run -p kgraf-diag --bin gen-errors-doc` \
         and commit the result",
        path.display()
    );
}
