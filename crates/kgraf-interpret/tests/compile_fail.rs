//! An approval cannot be forged (attack
//! `approval_without_user_via_deserialize`): an `Interpretation` could be
//! deserialized from edited JSON and approved. Each fixture in `compile_fail/`
//! tries a way to build an approvable reading without the pass; each must fail to
//! compile. Regenerate the expected output with
//! `TRYBUILD=overwrite cargo test -p kgraf-interpret --test compile_fail` after
//! a deliberate change, and read the diff.

// Wraps trybuild and explains a failure caused by a disturbed build (see the module).
#[path = "../../kgraf-provider/tests/support/compile_fail_guard.rs"]
mod compile_fail_guard;

#[test]
fn an_approvable_reading_cannot_be_forged() {
    compile_fail_guard::run("tests/compile_fail/*.rs");
}
