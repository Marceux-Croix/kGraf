//! Fixed, correct implementation. Do not change this file.
//!
//! Add a new test file `tests/more_tests.rs` with a test function named
//! exactly `handles_empty_and_negative` that asserts `sum(&[]) == 0` and
//! `sum(&[-3, 5, -2]) == 0`.

pub fn sum(xs: &[i32]) -> i32 {
    xs.iter().sum()
}
