use crate::entry::Entry;

pub fn record_posting(entry: &Entry) {
    audit_log("posting", &entry.account);
}

pub fn audit_log(event: &str, subject: &str) {
    eprintln!("audit: {event} {subject}");
}
