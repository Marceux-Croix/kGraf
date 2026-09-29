use fx_refactor_shipping::apply_discount;

#[test]
fn discount_is_extracted() {
    assert!((apply_discount(100.0, 10.0) - 90.0).abs() < 1e-9);
}
