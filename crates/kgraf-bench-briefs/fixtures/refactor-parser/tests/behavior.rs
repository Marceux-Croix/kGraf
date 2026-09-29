use fx_refactor_parser::tally_csv_ints;

#[test]
fn sums_fields() {
    assert_eq!(tally_csv_ints(" 1, 2,x, 4 "), 7);
}
