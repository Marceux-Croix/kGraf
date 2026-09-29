use crate::config::Config;
use crate::entry::parse_entry;
use crate::ledger::{Ledger, new_ledger, post_entry};

pub fn import_file(path: &str, cfg: &Config) -> Ledger {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut ledger = new_ledger();
    for line in text.lines() {
        if let Some(entry) = parse_entry(line, cfg) {
            let _ = post_entry(&mut ledger, &entry);
        }
    }
    ledger
}
