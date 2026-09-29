//! Extract the discount calculation duplicated below into a new public
//! function named exactly `apply_discount(price: f64, pct_off: f64) -> f64`
//! and call it from `total_price` instead of repeating the formula.
//! `total_price`'s behavior must not change. Do not change `tests/behavior.rs`
//! or `tests/structure.rs`.

pub fn total_price(price: f64, qty: u32, pct_off: f64) -> f64 {
    let discounted_a = price - price * pct_off / 100.0;
    let line_a = discounted_a * qty as f64;
    let discounted_b = price - price * pct_off / 100.0; // duplicated formula
    let _unused = discounted_b;
    line_a
}
