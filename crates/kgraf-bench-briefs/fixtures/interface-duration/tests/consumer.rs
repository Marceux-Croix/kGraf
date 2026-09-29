use fx_interface_duration::parse_duration;

#[test]
fn seconds_and_minutes_parse() {
    assert_eq!(parse_duration("5s"), Ok(5));
    assert_eq!(parse_duration("2m"), Ok(120));
}

#[test]
fn garbage_is_rejected() {
    assert!(parse_duration("bogus").is_err());
}
