pub struct Config {
    pub currency: String,
    pub rounding: u32,
}

pub fn default_config() -> Config {
    Config {
        currency: "USD".to_owned(),
        rounding: 2,
    }
}

pub fn load_config(path: &str) -> Config {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut cfg = default_config();
    for line in text.lines() {
        parse_setting(&mut cfg, line);
    }
    cfg
}

fn parse_setting(cfg: &mut Config, line: &str) {
    let mut parts = line.splitn(2, '=');
    let key = parts.next().unwrap_or("").trim();
    let value = parts.next().unwrap_or("").trim();
    match key {
        "currency" => cfg.currency = value.to_owned(),
        "rounding" => cfg.rounding = value.parse().unwrap_or(2),
        _ => {}
    }
}
