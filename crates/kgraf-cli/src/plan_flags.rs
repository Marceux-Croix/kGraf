//! `kgraf orchestrate`'s small-request flags (decision 0100): `--plan` and
//! the early-start pair. Each overrides the user's setting; with neither,
//! the setting decides, and with no setting a run is planned in full with no
//! early start, as before these flags existed.

use kgraf_orchestrator::triage::PlanMode;

/// The flags, flattened into `kgraf orchestrate`.
#[derive(clap::Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanFlags {
    /// How the instruction is planned: `auto` sizes it first, and a small
    /// request (one sentence, one or two named files) runs as one contract
    /// with no planning call; a request the rule cannot size gets one short
    /// triage call. `full` always plans. Overrides the `plan_mode` user
    /// setting; `full` by default.
    #[arg(long = "plan", value_name = "MODE", value_parser = parse_plan_mode)]
    pub plan: Option<PlanMode>,
    /// With `--plan auto`, start the one worker beside the triage call when
    /// it can start at once; its work merges only after the triage answer.
    /// Overrides the `early_start` user setting; off by default.
    #[arg(long, conflicts_with = "no_early_start")]
    pub early_start: bool,
    /// Never start a worker before the triage answer, whatever the user
    /// setting says. Benchmark drivers pass it.
    #[arg(long)]
    pub no_early_start: bool,
}

fn parse_plan_mode(s: &str) -> Result<PlanMode, String> {
    s.parse()
}

impl PlanFlags {
    /// This run's early-start answer from the flag pair (clap refuses both):
    /// `None` leaves it to the user setting.
    pub fn early_start(&self) -> Option<bool> {
        match (self.early_start, self.no_early_start) {
            (true, _) => Some(true),
            (false, true) => Some(false),
            (false, false) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        flags: PlanFlags,
    }

    fn parse(args: &[&str]) -> Result<PlanFlags, clap::Error> {
        let mut argv = vec!["kgraf"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).map(|c| c.flags)
    }

    #[test]
    fn the_flags_parse_and_leave_the_setting_alone_when_absent() {
        let none = parse(&[]).expect("no flags");
        assert_eq!((none.plan, none.early_start()), (None, None));
        let auto = parse(&["--plan", "auto", "--early-start"]).expect("auto");
        assert_eq!(
            (auto.plan, auto.early_start()),
            (Some(PlanMode::Auto), Some(true))
        );
        let full = parse(&["--plan", "full", "--no-early-start"]).expect("full");
        assert_eq!(
            (full.plan, full.early_start()),
            (Some(PlanMode::Full), Some(false))
        );
        assert!(parse(&["--plan", "small"]).is_err());
        assert!(parse(&["--early-start", "--no-early-start"]).is_err());
    }
}
