//! Induced-demand measurement for workflow templates (decision 0056).
//!
//! A template is expected to make decomposition cheaper per call. A cheaper
//! action gets taken more often, so the question is not whether a decomposition
//! costs less but whether runs that were offered templates **start more work**:
//! more contracts, more workers, more spend in total. Each run writes one
//! [`DemandRecord`] when it ends, from the first run on, whether or not a template
//! was offered, so both arms of the comparison exist.
//!
//! The record is a graph fact (subject `run:<id>`, predicate `demand`), so a
//! resumed run's later record closes its earlier one rather than overwriting it
//! (0028), and the records survive a graph schema rebuild.
//!
//! The report prints each arm's figures side by side with a fixed caveat, and
//! states no direction. The arms are not randomized (a run is offered templates
//! only when its instruction resembles an earlier completed run), so a difference
//! between them is not evidence that templates change spend, and the report never
//! ranks them. Decision 0008's measure, spend per verified task, is one
//! of the printed figures.

use kgraf_graph::{GraphStore, NewFact};
use serde::{Deserialize, Serialize};

/// Predicate of demand records.
pub const DEMAND: &str = "demand";
/// Subject prefix of demand records.
pub const RUN_SUBJECT: &str = "run:";
/// Runs under which an arm's figures are labeled single observations. A starting
/// point, not a researched number.
pub const MIN_RUNS_PER_ARM: usize = 3;

/// One run's measurement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DemandRecord {
    pub run: String,
    /// How the run ended: `completed`, `blocked`, `stopped`, `running`, or
    /// `plan_failed` (decomposition never produced a plan).
    pub status: String,
    /// Template ids offered to the run's decomposition. Empty: the run is in the
    /// without-templates arm.
    pub templates_offered: Vec<String>,
    /// The offered template whose structure the final plan has, if any (a
    /// structural match, not the model's claim).
    pub template_matched: Option<String>,
    /// Contracts in the final plan.
    pub contracts: u64,
    /// Contracts dispatched.
    pub dispatches: u64,
    /// Worker attempts started: the unit of work this measurement counts.
    pub units_started: u64,
    /// Contracts the verifier accepted.
    pub verified: u64,
    /// Decomposition calls (the first plan and every split), failed ones included.
    pub decomposition_calls: u64,
    /// Settled spend of those calls, in micro-dollars.
    pub decomposition_spend_micros: i64,
    /// The run's settled spend, in micro-dollars.
    pub spend_micros: i64,
    /// Calls whose spend is not known yet (unreconciled or unpriced); the spend
    /// figures above exclude them and are lower bounds when this is not zero.
    pub unknown_spend_calls: u64,
}

impl DemandRecord {
    pub fn with_templates(&self) -> bool {
        !self.templates_offered.is_empty()
    }

    /// Store the record, closing the run's earlier one if it differs.
    pub fn store(&self, g: &GraphStore, now_ms: i64) -> Result<(), DemandError> {
        let subject = format!("{RUN_SUBJECT}{}", self.run);
        let object = serde_json::to_string(self).map_err(|e| DemandError::Record {
            subject: subject.clone(),
            reason: e.to_string(),
        })?;
        g.assert_fact(
            &NewFact {
                subject: &subject,
                predicate: DEMAND,
                object: &object,
                valid_at: now_ms,
                invalid_at: None,
                episode: &self.run,
            },
            now_ms,
        )?;
        Ok(())
    }

    /// Every run's current record. A record this build cannot read is an error,
    /// not a skipped row: a report over a silently shorter set would mislead.
    pub fn load_all(g: &GraphStore) -> Result<Vec<Self>, DemandError> {
        g.open_facts(DEMAND, RUN_SUBJECT)?
            .into_iter()
            .map(|f| {
                serde_json::from_str(&f.object).map_err(|e| DemandError::Record {
                    subject: f.subject,
                    reason: e.to_string(),
                })
            })
            .collect()
    }
}

/// A demand report that cannot be produced.
#[derive(Debug, thiserror::Error)]
pub enum DemandError {
    #[error("graph: {0}")]
    Graph(#[from] kgraf_graph::GraphError),
    #[error("demand record `{subject}` cannot be read: {reason}")]
    Record { subject: String, reason: String },
}

/// One arm's figures.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Arm {
    pub runs: usize,
    pub completed: usize,
    pub units_per_run: f64,
    pub contracts_per_run: f64,
    pub decomposition_calls_per_run: f64,
    /// Micro-dollars.
    pub decomposition_spend_per_run: f64,
    pub spend_per_run: f64,
    pub spend_per_task: Option<f64>,
    /// Decision 0008's metric.
    pub spend_per_verified_task: Option<f64>,
    pub unknown_spend_calls: u64,
}

fn arm(records: &[&DemandRecord]) -> Arm {
    let n = records.len();
    if n == 0 {
        return Arm::default();
    }
    let sum = |f: &dyn Fn(&DemandRecord) -> f64| records.iter().map(|r| f(r)).sum::<f64>();
    let spend = sum(&|r| r.spend_micros as f64);
    let contracts = sum(&|r| r.contracts as f64);
    let verified = sum(&|r| r.verified as f64);
    let nf = n as f64;
    Arm {
        runs: n,
        completed: records.iter().filter(|r| r.status == "completed").count(),
        units_per_run: sum(&|r| r.units_started as f64) / nf,
        contracts_per_run: contracts / nf,
        decomposition_calls_per_run: sum(&|r| r.decomposition_calls as f64) / nf,
        decomposition_spend_per_run: sum(&|r| r.decomposition_spend_micros as f64) / nf,
        spend_per_run: spend / nf,
        spend_per_task: (contracts > 0.0).then(|| spend / contracts),
        spend_per_verified_task: (verified > 0.0).then(|| spend / verified),
        unknown_spend_calls: records.iter().map(|r| r.unknown_spend_calls).sum(),
    }
}

/// Both arms' figures.
#[derive(Clone, Debug, PartialEq)]
pub struct DemandSummary {
    pub with: Arm,
    pub without: Arm,
}

pub fn summarize(records: &[DemandRecord]) -> DemandSummary {
    let with: Vec<&DemandRecord> = records.iter().filter(|r| r.with_templates()).collect();
    let without: Vec<&DemandRecord> = records.iter().filter(|r| !r.with_templates()).collect();
    DemandSummary {
        with: arm(&with),
        without: arm(&without),
    }
}

/// Printed under every report, whatever the figures. The arms
/// are not randomized: a run is offered templates only when its instruction
/// resembles an earlier completed run, so the two groups of instructions differ.
pub const CAVEAT: &str = "The two arms are not comparable groups. A run is offered templates \
only when its instruction resembles an earlier completed run, so the arms hold different kinds \
of instructions, and a difference between the columns may come from the instructions, not from \
the templates. These figures do not show that templates change spend; decision 0008's measure \
is spend per verified task, and it is reported here as a number only.";

fn dollars(micros: Option<f64>) -> String {
    match micros {
        Some(m) => format!("${:.4}", m / 1_000_000.0),
        None => "n/a".to_owned(),
    }
}

/// The report as text: the figures, the run counts, and [`CAVEAT`]. It states
/// no direction: which column is larger is for the reader to see, with the
/// caveat beside it.
pub fn render(s: &DemandSummary) -> String {
    let mut out = String::from(
        "Induced demand: runs offered workflow templates, and runs not offered any.\n\n",
    );
    out.push_str(&format!(
        "{:<34}{:>16}{:>16}\n",
        "", "with templates", "without"
    ));
    let rows: [(&str, String, String); 9] = [
        (
            "runs (completed)",
            format!("{} ({})", s.with.runs, s.with.completed),
            format!("{} ({})", s.without.runs, s.without.completed),
        ),
        (
            "work units started per run",
            format!("{:.2}", s.with.units_per_run),
            format!("{:.2}", s.without.units_per_run),
        ),
        (
            "contracts per run",
            format!("{:.2}", s.with.contracts_per_run),
            format!("{:.2}", s.without.contracts_per_run),
        ),
        (
            "decomposition calls per run",
            format!("{:.2}", s.with.decomposition_calls_per_run),
            format!("{:.2}", s.without.decomposition_calls_per_run),
        ),
        (
            "decomposition spend per run",
            dollars(Some(s.with.decomposition_spend_per_run)),
            dollars(Some(s.without.decomposition_spend_per_run)),
        ),
        (
            "spend per run",
            dollars(Some(s.with.spend_per_run)),
            dollars(Some(s.without.spend_per_run)),
        ),
        (
            "spend per task",
            dollars(s.with.spend_per_task),
            dollars(s.without.spend_per_task),
        ),
        (
            "spend per verified task (0008)",
            dollars(s.with.spend_per_verified_task),
            dollars(s.without.spend_per_verified_task),
        ),
        (
            "calls with unknown spend",
            s.with.unknown_spend_calls.to_string(),
            s.without.unknown_spend_calls.to_string(),
        ),
    ];
    for (label, a, b) in rows {
        out.push_str(&format!("{label:<34}{a:>16}{b:>16}\n"));
    }
    out.push('\n');
    if s.with.runs < MIN_RUNS_PER_ARM || s.without.runs < MIN_RUNS_PER_ARM {
        out.push_str(&format!(
            "An arm has under {MIN_RUNS_PER_ARM} runs ({} with templates, {} without); \
             its figures are single observations.\n",
            s.with.runs, s.without.runs
        ));
    }
    if s.with.unknown_spend_calls + s.without.unknown_spend_calls > 0 {
        out.push_str(
            "Some calls have unknown spend (unreconciled or unpriced); spend figures are lower bounds.\n",
        );
    }
    out.push_str(CAVEAT);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(run: &str, offered: bool, units: u64, decomp: i64, spend: i64) -> DemandRecord {
        DemandRecord {
            run: run.into(),
            status: "completed".into(),
            templates_offered: if offered { vec!["t1".into()] } else { vec![] },
            template_matched: None,
            contracts: units,
            dispatches: units,
            units_started: units,
            verified: units,
            decomposition_calls: 1,
            decomposition_spend_micros: decomp,
            spend_micros: spend,
            unknown_spend_calls: 0,
        }
    }

    fn arms(with: (u64, i64, i64), without: (u64, i64, i64)) -> Vec<DemandRecord> {
        let mut records = Vec::new();
        for i in 0..3 {
            records.push(record(&format!("w{i}"), true, with.0, with.1, with.2));
            records.push(record(
                &format!("o{i}"),
                false,
                without.0,
                without.1,
                without.2,
            ));
        }
        records
    }

    /// The text with every figure removed and the two columns' figures in a
    /// canonical order: what the report *says*, apart from the numbers.
    fn wording(text: &str) -> String {
        text.lines()
            .map(|l| {
                l.split_whitespace()
                    .filter(|w| !w.chars().any(|c| c.is_ascii_digit()) && *w != "n/a")
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_small_sample_is_labeled_and_figures_are_shown() {
        let s = summarize(&[record("a", true, 3, 10, 100), record("b", false, 2, 20, 90)]);
        let text = render(&s);
        assert!(text.contains("under 3 runs"), "{text}");
        assert!(text.contains(CAVEAT), "{text}");
    }

    #[test]
    fn figures_are_per_arm_and_correct() {
        let s = summarize(&arms((6, 10_000, 600_000), (2, 40_000, 300_000)));
        assert_eq!(s.with.units_per_run, 6.0);
        assert_eq!(s.without.spend_per_verified_task, Some(150_000.0));
        assert_eq!(s.with.spend_per_verified_task, Some(100_000.0));
        let text = render(&s);
        assert!(
            text.contains("$0.1000") && text.contains("$0.1500"),
            "{text}"
        );
    }

    /// Makes no savings claim: whichever arm spends less, the report says the same
    /// thing. If it ranked the arms ("lower", "cheaper"), swapping which arm has
    /// the smaller figures would change its wording.
    #[test]
    fn the_report_says_the_same_thing_whichever_arm_spends_less() {
        let a = render(&summarize(&arms(
            (6, 10_000, 600_000),
            (2, 40_000, 300_000),
        )));
        let b = render(&summarize(&arms(
            (2, 40_000, 300_000),
            (6, 10_000, 600_000),
        )));
        assert_ne!(a, b, "the figures differ");
        assert_eq!(wording(&a), wording(&b), "the wording does not");
        let lower = a.to_lowercase();
        for banned in [
            "saving", "saved", "reduce", "cheaper", "lower", "higher", "less", "more", "fewer",
            "better", "worse",
        ] {
            assert!(
                !lower
                    .split(|c: char| !c.is_alphabetic())
                    .any(|w| w == banned),
                "`{banned}` in {a}"
            );
        }
        assert!(a.contains(CAVEAT));
    }

    #[test]
    fn a_resumed_runs_later_record_replaces_the_earlier_one_in_the_report() {
        let g = GraphStore::open_in_memory(std::path::Path::new(".")).expect("g");
        let mut r = record("run-1", false, 1, 5, 50);
        r.status = "stopped".into();
        r.store(&g, 10).expect("first");
        r.status = "completed".into();
        r.units_started = 2;
        r.store(&g, 20).expect("second");
        let all = DemandRecord::load_all(&g).expect("load");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].units_started, 2);
        let history = g.fact_history("run:run-1", DEMAND).expect("history");
        assert_eq!(
            history.len(),
            2,
            "the earlier record is closed, not deleted"
        );
    }

    /// Two records written in the same millisecond (a stop and a resume that
    /// ends at once) must not both count.
    #[test]
    fn two_records_at_one_timestamp_count_once() {
        let g = GraphStore::open_in_memory(std::path::Path::new(".")).expect("g");
        let mut r = record("run-1", false, 1, 5, 50);
        r.store(&g, 10).expect("first");
        r.units_started = 4;
        r.store(&g, 10).expect("second");
        let all = DemandRecord::load_all(&g).expect("load");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].units_started, 4);
        assert_eq!(summarize(&all).without.runs, 1);
    }
}
