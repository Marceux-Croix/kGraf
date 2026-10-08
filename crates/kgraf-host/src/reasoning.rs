//! What a run's reasoning setting asks of each route, checked when the run
//! opens (decision 0098).
//!
//! Each call kind runs on a known route: the planner's kinds on the planner's
//! route, a worker's kinds and the contract-check kinds on the worker routes.
//! For each route and each level the setting asks of it:
//!
//! - `off` is sent where the adapter has a verified wire form and checked on
//!   every answer (an answer that still reasons fails its call); an adapter
//!   with no verified form refuses the call before sending it. Said at run
//!   start.
//! - A graded level that a recorded measurement found the route and model do
//!   not honor is refused here, before any call: sending it would be an
//!   ignored parameter, and a lower or default amount of reasoning in its
//!   place would be silent.
//! - A graded level with no measurement for the route and model is sent,
//!   and the run says at its start that it is unverified there: no answer
//!   shows whether a graded level was honored.
//!
//! An unset level sends nothing and is not said per route; the run's report
//! lines say what is set (`ResolvedLimits::reasoning_report`).

use crate::config::RouteSpec;
use crate::error::{Error, Result};
use kgraf_fleet::LevelOutcome;
use kgraf_provider::{ReasoningEffort, ReasoningGroup, ReasoningLevels, ReasoningProcess};

/// The call kinds that run on each route of the run, routes in first-use
/// order (the planner's first). A route used for two roles is listed once.
pub fn route_uses(
    planner: &RouteSpec,
    workers: &[RouteSpec],
    contract_checks: bool,
    interface_review: bool,
) -> Vec<(RouteSpec, Vec<ReasoningProcess>)> {
    let mut out: Vec<(RouteSpec, Vec<ReasoningProcess>)> = Vec::new();
    let mut add = |route: &RouteSpec, process: ReasoningProcess| {
        let at = match out.iter().position(|(r, _)| r == route) {
            Some(at) => at,
            None => {
                out.push((route.clone(), Vec::new()));
                out.len() - 1
            }
        };
        if !out[at].1.contains(&process) {
            out[at].1.push(process);
        }
    };
    for process in ReasoningProcess::ALL {
        let runs = match process {
            // Asked only when interface contracts are written and checked.
            ReasoningProcess::Interfaces | ReasoningProcess::ChallengeReview => contract_checks,
            _ => true,
        };
        if runs && process.group() == ReasoningGroup::Planner {
            add(planner, process);
        }
    }
    for route in workers {
        for process in ReasoningProcess::ALL {
            let runs = match process.group() {
                ReasoningGroup::Planner => false,
                ReasoningGroup::Worker => true,
                ReasoningGroup::Checks => match process {
                    ReasoningProcess::InterfaceReview => interface_review,
                    _ => contract_checks,
                },
            };
            if runs {
                add(route, process);
            }
        }
    }
    out
}

/// The call kinds of a single worker on `route` (`kgraf worker`): its rounds
/// and its compaction summaries.
pub fn worker_uses(route: &RouteSpec) -> Vec<(RouteSpec, Vec<ReasoningProcess>)> {
    vec![(
        route.clone(),
        vec![
            ReasoningProcess::WorkerRound,
            ReasoningProcess::WorkerCompaction,
        ],
    )]
}

/// Check what `levels` asks of each route in `uses`. A graded level measured
/// as not honored on its route and model is an error naming every such
/// route, level and call kind; everything else is a sentence to say at the
/// run's start, one per route and level.
pub fn check(
    levels: &ReasoningLevels,
    uses: &[(RouteSpec, Vec<ReasoningProcess>)],
) -> Result<Vec<String>> {
    let mut said = Vec::new();
    let mut refused = Vec::new();
    for (route, processes) in uses {
        let name = format!("{}:{}", route.route, route.model);
        for level in ReasoningEffort::ALL {
            let kinds: Vec<&str> = processes
                .iter()
                .filter(|p| levels.effort(**p) == Some(level))
                .map(|p| p.call_kind())
                .collect();
            if kinds.is_empty() {
                continue;
            }
            let kinds = kinds.join(", ");
            let measured = kgraf_fleet::reasoning_measurement(&route.route, &route.model, level);
            match (level, measured) {
                (_, Some(m)) if m.outcome == LevelOutcome::NotHonored => refused.push(format!(
                    "`{name}` was measured not to honor reasoning level `{level}` ({}), and the \
                     run asks for it on {kinds}",
                    m.record
                )),
                (ReasoningEffort::Off, measured) => said.push(format!(
                    "`{name}` asks for no reasoning on {kinds}{}; an answer that still carries \
                     reasoning fails its call, and a provider with no verified off control \
                     refuses the call before sending it",
                    measured.map_or(String::new(), |m| format!(
                        " (measured to turn reasoning off: {})",
                        m.record
                    ))
                )),
                (_, Some(m)) => said.push(format!(
                    "`{name}` asks for reasoning level `{level}` on {kinds}; measured to take \
                     effect ({})",
                    m.record
                )),
                (_, None) => said.push(format!(
                    "`{name}` asks for reasoning level `{level}` on {kinds}; UNVERIFIED: no \
                     measurement of this level on this route and model is recorded, and no \
                     answer shows whether a server honors a graded level, so it may have no \
                     effect"
                )),
            }
        }
    }
    if refused.is_empty() {
        Ok(said)
    } else {
        Err(Error::Invalid(format!(
            "{}. kGraf does not send a reasoning level that has no effect. Set these call \
             kinds to `unset` (the model's default) or `off`, or use another route or model",
            refused.join("; ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_provider::{ReasoningLevel, ReasoningScope};

    fn spec(s: &str) -> RouteSpec {
        RouteSpec::parse(s).expect("spec")
    }

    const LOW: ReasoningLevel = ReasoningLevel::Effort(ReasoningEffort::Low);

    #[test]
    fn each_call_kind_is_placed_on_the_route_it_runs_on() {
        let planner = spec("ollama:p");
        let workers = [spec("ollama:w"), spec("ollama:p")];
        let uses = route_uses(&planner, &workers, true, false);
        assert_eq!(uses.len(), 2, "the planner's route is listed once");
        let on = |route: &str| {
            uses.iter()
                .find(|(r, _)| r.model == route)
                .map(|(_, p)| p.clone())
                .expect("route")
        };
        assert!(on("p").contains(&ReasoningProcess::Decompose));
        assert!(on("p").contains(&ReasoningProcess::WorkerRound));
        assert!(on("w").contains(&ReasoningProcess::Handshake));
        assert!(!on("w").contains(&ReasoningProcess::InterfaceReview));
        assert!(!on("w").contains(&ReasoningProcess::Decompose));
        let plain = route_uses(&planner, &workers[..1], false, false);
        assert!(!plain[0].1.contains(&ReasoningProcess::Interfaces));
        assert!(!plain[1].1.contains(&ReasoningProcess::Handshake));
    }

    /// A graded level measured as not honored is refused before any call,
    /// naming the route, the level, the call kinds and the record; the same
    /// level on an unmeasured model runs, and is said to be unverified.
    #[test]
    fn a_level_measured_as_not_honored_is_refused_and_an_unmeasured_one_is_said() {
        let levels = ReasoningLevels::new().with(ReasoningScope::All, LOW);
        let measured = route_uses(&spec("ollama:qwen3.6:35b"), &[], false, false);
        let e = check(&levels, &measured).expect_err("refused").to_string();
        assert!(
            e.contains("`ollama:qwen3.6:35b` was measured not to honor reasoning level `low`"),
            "{e}"
        );
        assert!(
            e.contains("decompose") && e.contains("Ollama 0.34.4"),
            "{e}"
        );
        let unmeasured = route_uses(&spec("ollama:llama3"), &[], false, false);
        let said = check(&levels, &unmeasured).expect("runs");
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].contains("level `low`") && said[0].contains("UNVERIFIED"),
            "{}",
            said[0]
        );
        // `off` on the measured route runs, and says the record.
        let off = ReasoningLevels::new().with(
            ReasoningScope::All,
            ReasoningLevel::Effort(ReasoningEffort::Off),
        );
        let said = check(&off, &measured).expect("runs");
        assert!(
            said[0].contains("measured to turn reasoning off"),
            "{said:?}"
        );
        // Unset sends nothing and says nothing per route.
        assert!(
            check(&ReasoningLevels::new(), &measured)
                .expect("runs")
                .is_empty()
        );
    }
}
