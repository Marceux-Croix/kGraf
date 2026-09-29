use crate::config::Config;

pub struct Amount {
    pub minor: i64,
}

impl Amount {
    pub fn new(minor: i64) -> Amount {
        Amount { minor }
    }
}

pub fn parse_amount(text: &str, cfg: &Config) -> Option<Amount> {
    let digits = strip_currency(text, cfg);
    let value: f64 = digits.parse().ok()?;
    Some(Amount::new(round_minor(value, cfg.rounding)))
}

pub fn format_amount(amount: &Amount, cfg: &Config) -> String {
    let scale = 10_i64.pow(cfg.rounding);
    format!(
        "{} {}.{:0width$}",
        cfg.currency,
        amount.minor / scale,
        amount.minor % scale,
        width = cfg.rounding as usize
    )
}

fn strip_currency<'a>(text: &'a str, cfg: &Config) -> &'a str {
    text.trim().trim_start_matches(cfg.currency.as_str()).trim()
}

fn round_minor(value: f64, places: u32) -> i64 {
    (value * 10_f64.powi(places as i32)).round() as i64
}
