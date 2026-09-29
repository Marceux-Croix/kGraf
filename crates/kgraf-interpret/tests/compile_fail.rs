//! An approval cannot be forged (attack
//! `approval_without_user_via_deserialize`): an `Interpretation` could be
//! deserialized from edited JSON and approved. Each fixture in `compile_fail/`
//! tries a way to build an approvable reading without the pass; each must fail to
//! compile. Regenerate the expected output with
//! `TRYBUILD=overwrite cargo test -p kgraf-interpret --test compile_fail` after
//! a deliberate change, and read the diff.

#[test]
fn an_approvable_reading_cannot_be_forged() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
