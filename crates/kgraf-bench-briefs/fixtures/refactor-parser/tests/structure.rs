use fx_refactor_parser::parse_int_field;

#[test]
fn helper_is_extracted() {
    assert_eq!(parse_int_field(" 7 "), 7);
    assert_eq!(parse_int_field("nope"), 0);
}
