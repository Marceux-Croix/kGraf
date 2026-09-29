use crate::account::{Account, apply_entry, open_account};
use crate::audit::record_posting;
use crate::entry::Entry;

pub struct Ledger {
    pub accounts: Vec<Account>,
}

impl Ledger {
    pub fn new() -> Ledger {
        Ledger {
            accounts: Vec::new(),
        }
    }
}

pub fn new_ledger() -> Ledger {
    Ledger::new()
}

pub fn post_entry(ledger: &mut Ledger, entry: &Entry) -> Result<(), String> {
    validate(entry)?;
    let account = find_or_open(ledger, &entry.account);
    apply_entry(account, entry);
    record_posting(entry);
    Ok(())
}

pub fn balance_of(ledger: &Ledger, name: &str) -> i64 {
    ledger
        .accounts
        .iter()
        .find(|a| a.name == name)
        .map(|a| a.balance)
        .unwrap_or(0)
}

fn find_or_open<'a>(ledger: &'a mut Ledger, name: &str) -> &'a mut Account {
    if !ledger.accounts.iter().any(|a| a.name == name) {
        ledger.accounts.push(open_account(name));
    }
    ledger
        .accounts
        .iter_mut()
        .find(|a| a.name == name)
        .expect("just opened")
}

fn validate(entry: &Entry) -> Result<(), String> {
    if entry.account.is_empty() {
        return Err("an entry needs an account".to_owned());
    }
    Ok(())
}
