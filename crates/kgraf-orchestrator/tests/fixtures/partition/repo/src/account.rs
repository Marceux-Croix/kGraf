use crate::entry::Entry;

pub struct Account {
    pub name: String,
    pub balance: i64,
}

impl Account {
    pub fn new(name: &str) -> Account {
        Account {
            name: name.to_owned(),
            balance: 0,
        }
    }
}

pub fn open_account(name: &str) -> Account {
    Account::new(name)
}

pub fn apply_entry(account: &mut Account, entry: &Entry) {
    account.balance += entry.amount.minor;
}
