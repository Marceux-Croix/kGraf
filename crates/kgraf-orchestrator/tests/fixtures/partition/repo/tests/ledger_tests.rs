use ledgerlite::config::default_config;
use ledgerlite::entry::parse_entry;
use ledgerlite::ledger::{balance_of, new_ledger, post_entry};

#[test]
fn a_posting_moves_the_balance() {
    let cfg = default_config();
    let mut ledger = new_ledger();
    let entry = parse_entry("2026-01-01 | cash | 5.00", &cfg).expect("entry");
    post_entry(&mut ledger, &entry).expect("post");
    assert_eq!(balance_of(&ledger, "cash"), 500);
}
