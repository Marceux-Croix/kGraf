use fx_refactor_shipping::total_price;

#[test]
fn known_total() {
    assert!((total_price(100.0, 3, 10.0) - 270.0).abs() < 1e-9);
}
