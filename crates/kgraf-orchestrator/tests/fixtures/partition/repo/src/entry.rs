use crate::config::Config;
use crate::money::{Amount, parse_amount};

pub struct Entry {
    pub date: String,
    pub account: String,
    pub amount: Amount,
}

pub fn parse_entry(line: &str, cfg: &Config) -> Option<Entry> {
    let fields = split_fields(line);
    if fields.len() != 3 {
        return None;
    }
    let amount = parse_amount(fields[2], cfg)?;
    Some(Entry {
        date: fields[0].to_owned(),
        account: fields[1].to_owned(),
        amount,
    })
}

fn split_fields(line: &str) -> Vec<&str> {
    line.split('|').map(str::trim).collect()
}
