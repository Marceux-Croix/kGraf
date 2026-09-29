//! Fixed, correct implementation. Do not change this file.
//!
//! Add a new test file `tests/more_tests.rs` with a test function named
//! exactly `reverses_multiple_words` that asserts
//! `reverse_words("a b c") == "c b a"` and `reverse_words("") == ""`.

pub fn reverse_words(s: &str) -> String {
    s.split_whitespace().rev().collect::<Vec<_>>().join(" ")
}
