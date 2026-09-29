use ledgerlite::config::default_config;
use ledgerlite::ledger::new_ledger;
use ledgerlite::report::render_report;

#[test]
fn an_empty_ledger_renders_nothing() {
    assert_eq!(render_report(&new_ledger(), &default_config()), "");
}
