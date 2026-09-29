//! The bench worker must add `parse_duration`, matching this signature
//! exactly (the fixed consumer test in `tests/consumer.rs` compiles against
//! it; do not change that file):
//!
//! `pub fn parse_duration(input: &str) -> Result<u64, String>`
//!
//! Accepts an integer followed by `s` (seconds) or `m` (minutes) and returns
//! the duration in seconds. Anything else is an `Err` explaining why.
