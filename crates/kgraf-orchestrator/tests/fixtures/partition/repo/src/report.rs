use crate::config::Config;
use crate::ledger::{Ledger, balance_of};
use crate::money::{Amount, format_amount};

pub fn render_report(ledger: &Ledger, cfg: &Config) -> String {
    let mut out = String::new();
    for account in &ledger.accounts {
        let total = balance_of(ledger, &account.name);
        out.push_str(&format_row(&account.name, total, cfg));
        out.push('\n');
    }
    out
}

fn format_row(name: &str, balance: i64, cfg: &Config) -> String {
    format!("{name}: {}", format_amount(&Amount::new(balance), cfg))
}
