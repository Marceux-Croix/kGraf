use ledgerlite::config::default_config;
use ledgerlite::import::import_file;

#[test]
fn a_missing_file_imports_nothing() {
    let ledger = import_file("does-not-exist", &default_config());
    assert!(ledger.accounts.is_empty());
}
