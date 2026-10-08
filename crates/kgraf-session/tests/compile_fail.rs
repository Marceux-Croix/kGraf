//! Compile-fail tests for decision 0057: "a compacted context is never
//! compacted again."
//!
//! Each fixture in `compile_fail/` tries to feed a [`kgraf_session::reduction::ReducedView`]'s
//! output back into [`kgraf_session::reduction::reduce`]. The test passes when the compiler
//! rejects every fixture and fails the moment one compiles.
//!
//! As with `kgraf-tools`' fixtures, a rustc release that rewords an error
//! fails these spuriously; refresh with
//! `TRYBUILD=overwrite cargo test -p kgraf-session --test compile_fail` after
//! reading the new output.

// Wraps trybuild and explains a failure caused by a disturbed build (see the module).
#[path = "../../kgraf-provider/tests/support/compile_fail_guard.rs"]
mod compile_fail_guard;

#[test]
fn a_reduced_view_cannot_be_reduced_again() {
    compile_fail_guard::run("tests/compile_fail/*.rs");
}
