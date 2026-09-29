//! [`DiagCode`]: the trait an error type implements to name its own glossary
//! entry, so a caller recording it never has to guess or hand-map a variant
//! to a code: a variant left to hand-mapping never reaches its specific
//! glossary entry.
//!
//! This trait lives here, not on `ErrorSpec` or in a crate any error type
//! already depends on, specifically so implementing it never creates a
//! dependency cycle: `kgraf-diag` depends on nothing in this workspace except
//! `kgraf-core`, so any crate (`kgraf-graph`, `kgraf-provider`,
//! `kgraf-sandbox`, `kgraf-host`, `kgraf-orchestrator`, and so on) can depend
//! on `kgraf-diag` and implement `DiagCode` on its own error enum without
//! `kgraf-diag` ever needing to know those crates exist.

/// An error type that can name its own diagnostics code. Implemented on a
/// crate's `thiserror` enum, next to the enum itself (see `crates/kgraf-graph
/// /src/error.rs` for the pattern): one match arm per variant that has its
/// own entry in `glossary::GLOSSARY`, falling back to that crate's `KG-xx99`
/// catch-all (`glossary::catch_all_for`) for anything not yet promoted to a
/// specific code.
pub trait DiagCode {
    /// The stable `KG-nnnn` code for this error value.
    fn diag_code(&self) -> &'static str;

    /// The crate or component name this error belongs to, matching
    /// `ErrorSpec::crate_name` in the glossary (for example `"kgraf-graph"`).
    fn diag_component(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::DiagCode;

    #[derive(Debug)]
    enum Fake {
        Known,
        Other,
    }

    impl DiagCode for Fake {
        fn diag_code(&self) -> &'static str {
            match self {
                Fake::Known => "KG-0201",
                Fake::Other => crate::glossary::catch_all_for("kgraf-graph").code,
            }
        }

        fn diag_component(&self) -> &'static str {
            "kgraf-graph"
        }
    }

    #[test]
    fn a_named_variant_and_the_catch_all_both_resolve() {
        assert_eq!(Fake::Known.diag_code(), "KG-0201");
        assert_eq!(Fake::Other.diag_code(), "KG-0299");
    }
}
