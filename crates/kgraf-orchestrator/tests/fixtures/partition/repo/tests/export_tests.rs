use ledgerlite::config::default_config;
use ledgerlite::export::export_csv;
use ledgerlite::ledger::new_ledger;

#[test]
fn an_empty_export_has_a_header() {
    assert_eq!(export_csv(&new_ledger(), &default_config()), "account,balance\n");
}
