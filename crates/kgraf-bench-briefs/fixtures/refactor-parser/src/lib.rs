//! Extract the per-field parsing below into a new public function named
//! exactly `parse_int_field(field: &str) -> i64` (trim the field, parse it,
//! default to 0 on error) and call it from `tally_csv_ints` for each field.
//! `tally_csv_ints`'s behavior must not change. Do not change
//! `tests/behavior.rs` or `tests/structure.rs`.

pub fn tally_csv_ints(line: &str) -> i64 {
    let mut total = 0i64;
    for field in line.split(',') {
        let trimmed = field.trim();
        let value: i64 = trimmed.parse().unwrap_or(0);
        total += value;
    }
    total
}
