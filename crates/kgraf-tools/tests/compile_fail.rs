//! Compile-fail tests binding approval to execution.
//!
//! `Tool::call` requires a `CallPermit`; only `ToolRegistry::execute` makes one, and
//! it takes an `ApprovedCall`, which only the approval chain makes after recording
//! every decision. Each fixture in `compile_fail/` tries a way around that. The test
//! passes when the compiler rejects every fixture and fails the moment one compiles.
//!
//! As with `kgraf-provider`'s fixtures, a rustc release that rewords an error
//! fails these spuriously; refresh with
//! `TRYBUILD=overwrite cargo test -p kgraf-tools --test compile_fail` after
//! reading the new output.

#[test]
fn a_tool_cannot_be_run_without_passing_approval() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
