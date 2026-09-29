use ledgerlite::config::default_config;
use ledgerlite::money::{format_amount, parse_amount};

#[test]
fn amounts_round_trip() {
    let cfg = default_config();
    let a = parse_amount("USD 12.30", &cfg).expect("amount");
    assert_eq!(format_amount(&a, &cfg), "USD 12.30");
}
