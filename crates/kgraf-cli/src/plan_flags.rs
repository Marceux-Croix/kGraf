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
    /// A plan's contract may carry a check of its own, run only when the
    /// run's check tests nothing; its pass verifies the contract. Overrides
    /// the `own_checks` user setting; off by default.
    #[arg(long, conflicts_with = "no_own_checks")]
    pub own_checks: bool,
    /// Ignore contracts' own checks, whatever the user setting says.
    /// Benchmark drivers pass it.
    #[arg(long)]
    pub no_own_checks: bool,
    /// A planned contract whose narrowed check selects no test stays
    /// unverified until the root gate passes over it, as a small request's
    /// does. Overrides the `zero_test_plans` user setting; off by default.
    #[arg(long, conflicts_with = "no_zero_test_plans")]
    pub zero_test_plans: bool,
    /// Apply the zero-test rule to small requests only, whatever the user
    /// setting says. Benchmark drivers pass it.
    #[arg(long)]
    pub no_zero_test_plans: bool,
    /// Run the planner calls a contract's dispatch waits on (an interface's
    /// handshake, its stub land, its consumers' tests, a re-grounding) beside
    /// the run, so other contracts verify, merge and dispatch meanwhile; the
    /// contract that needs the answer waits for it. Overrides the
    /// `concurrent_planner` user setting; off by default.
    #[arg(long, conflicts_with = "no_concurrent_planner")]
    pub concurrent_planner: bool,
    /// Wait for each such planner call before anything else runs, whatever
    /// the user setting says. Benchmark drivers pass it.
    #[arg(long)]
    pub no_concurrent_planner: bool,
    /// A change to a finished run sends its plan block in id order, marked
    /// for the provider's prompt cache where the provider takes marks and a
    /// read can still come, with the run's cache key. Overrides the
    /// `cache_layout` user setting; off by default.
    #[arg(long, conflicts_with = "no_cache_layout")]
    pub cache_layout: bool,
    /// Send change requests as they were before the cache layout, whatever
    /// the user setting says. Benchmark drivers pass it.
    #[arg(long)]
    pub no_cache_layout: bool,
}

fn parse_plan_mode(s: &str) -> Result<PlanMode, String> {
    s.parse()
}

impl PlanFlags {
    /// This run's early-start answer from the flag pair (clap refuses both):
    /// `None` leaves it to the user setting.
    pub fn early_start(&self) -> Option<bool> {
        pair(self.early_start, self.no_early_start)
    }

    /// The own-checks answer from its flag pair; `None` leaves it to the
    /// user setting.
    pub fn own_checks(&self) -> Option<bool> {
        pair(self.own_checks, self.no_own_checks)
    }

    /// The zero-test answer from its flag pair; `None` leaves it to the
    /// user setting.
    pub fn zero_test_plans(&self) -> Option<bool> {
        pair(self.zero_test_plans, self.no_zero_test_plans)
    }

    /// The concurrent-planner answer from its flag pair; `None` leaves it to
    /// the user setting.
    pub fn concurrent_planner(&self) -> Option<bool> {
        pair(self.concurrent_planner, self.no_concurrent_planner)
    }

    /// The cache-layout answer from its flag pair; `None` leaves it to the
    /// user setting.
    pub fn cache_layout(&self) -> Option<bool> {
        pair(self.cache_layout, self.no_cache_layout)
    }
}

/// A `--x` / `--no-x` pair (clap refuses both): `None` when neither is given.
fn pair(on: bool, off: bool) -> Option<bool> {
    match (on, off) {
        (true, _) => Some(true),
        (false, true) => Some(false),
        (false, false) => None,
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

    /// The two opt-in settings' pairs: unset by default, each side read,
    /// both together refused.
    #[test]
    fn the_own_check_and_zero_test_pairs_parse() {
        let none = parse(&[]).expect("no flags");
        assert_eq!((none.own_checks(), none.zero_test_plans()), (None, None));
        let on = parse(&["--own-checks", "--zero-test-plans"]).expect("on");
        assert_eq!(
            (on.own_checks(), on.zero_test_plans()),
            (Some(true), Some(true))
        );
        let off = parse(&["--no-own-checks", "--no-zero-test-plans"]).expect("off");
        assert_eq!(
            (off.own_checks(), off.zero_test_plans()),
            (Some(false), Some(false))
        );
        assert!(parse(&["--own-checks", "--no-own-checks"]).is_err());
        assert!(parse(&["--zero-test-plans", "--no-zero-test-plans"]).is_err());
    }

    #[test]
    fn the_concurrent_planner_pair_parses() {
        assert_eq!(parse(&[]).expect("none").concurrent_planner(), None);
        let on = parse(&["--concurrent-planner"]).expect("on");
        assert_eq!(on.concurrent_planner(), Some(true));
        let off = parse(&["--no-concurrent-planner"]).expect("off");
        assert_eq!(off.concurrent_planner(), Some(false));
        assert!(parse(&["--concurrent-planner", "--no-concurrent-planner"]).is_err());
    }

    /// The cache-layout pair: unset by default, each side read, both refused.
    #[test]
    fn the_cache_layout_pair_parses() {
        assert_eq!(parse(&[]).expect("none").cache_layout(), None);
        assert_eq!(
            parse(&["--cache-layout"]).expect("on").cache_layout(),
            Some(true)
        );
        assert_eq!(
            parse(&["--no-cache-layout"]).expect("off").cache_layout(),
            Some(false)
        );
        assert!(parse(&["--cache-layout", "--no-cache-layout"]).is_err());
    }
}
