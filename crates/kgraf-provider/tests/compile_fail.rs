//! Compile-fail tests for the accounting guarantee.
//!
//! Decision 0039 makes the accounting rule a property of the type system: a provider call
//! requires a `CallToken`, a `CallToken` can only come from `Ledger::begin`, and
//! `Ledger::begin` writes the call's row before returning one. So a provider call
//! that is not counted cannot be written.
//!
//! That record names a cost, and this file is the answer to it. The cost was that
//! the guarantee had **no test**, because the failing program cannot be expressed,
//! and an invariant with no test is the exact failure the test-the-mechanism rule exists to prevent. If
//! a later edit relaxed the signature, made a field public, or added a convenient
//! constructor, the guarantee would quietly evaporate with every existing test
//! still green.
//!
//! The way out is to invert the assertion. Each fixture in `compile_fail/` is a
//! program that tries to obtain a token without the ledger. The test passes when
//! the compiler **rejects** it, and fails the moment one of them starts compiling.
//! The bad thing still cannot be written; now something notices if that changes.
//!
//! **Known brittleness, stated rather than discovered later.** These fixtures
//! compare the compiler's exact output, so a rustc release that rewords an error
//! will fail them spuriously. That is a loud, one-command fix
//! (`TRYBUILD=overwrite cargo test -p kgraf-provider --test compile_fail`) and
//! it is the right direction to fail in: a false alarm costs a minute, and a
//! silently lost accounting guarantee costs a wrong invoice.

// Wraps trybuild and explains a failure caused by a disturbed build (see the module).
#[path = "support/compile_fail_guard.rs"]
mod compile_fail_guard;

#[test]
fn a_provider_call_cannot_be_made_without_a_ledger_minted_token() {
    compile_fail_guard::run("tests/compile_fail/*.rs");
}
