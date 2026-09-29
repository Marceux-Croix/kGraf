use crate::config::load_config;
use crate::export::export_csv;
use crate::import::import_file;
use crate::report::render_report;

pub fn run(args: &[String]) -> String {
    let cfg = load_config(&args[0]);
    let ledger = import_file(&args[1], &cfg);
    if args.get(2).map(String::as_str) == Some("--csv") {
        export_csv(&ledger, &cfg)
    } else {
        render_report(&ledger, &cfg)
    }
}
