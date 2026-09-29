use crate::config::Config;
use crate::ledger::Ledger;
use crate::money::{Amount, format_amount};

pub fn export_csv(ledger: &Ledger, cfg: &Config) -> String {
    let mut out = String::from("account,balance\n");
    for account in &ledger.accounts {
        out.push_str(&format_row(&account.name, account.balance, cfg));
        out.push('\n');
    }
    out
}

fn format_row(name: &str, balance: i64, cfg: &Config) -> String {
    format!(
        "{},{}",
        escape_csv(name),
        format_amount(&Amount::new(balance), cfg)
    )
}

fn escape_csv(field: &str) -> String {
    field.replace('"', "\"\"")
}
