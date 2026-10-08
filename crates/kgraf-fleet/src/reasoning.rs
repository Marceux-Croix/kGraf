//! Measured reasoning-level profiles (decision 0098): what a recorded
//! measurement found a route and model do with each reasoning level.
//!
//! An answer shows whether reasoning was off (it carries no reasoning), so
//! `off` is checked on every call by the adapter. An answer does not show
//! whether a graded level (`low`, `medium`, `high`) was honored: a server
//! that ignores it answers with HTTP 200 and the model's default amount of
//! reasoning. Only a measurement can tell, so the table records them.
//!
//! The catalog lists no model names (see the crate docs); this table does,
//! because a measurement is of one model on one route, and another model on
//! the same server can behave differently. A row is added only from a
//! recorded measurement, and names where it is documented.

use kgraf_provider::ReasoningEffort;

/// What a measurement found a level does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelOutcome {
    /// The level changed the reasoning as asked.
    Honored,
    /// The level was accepted and had no measurable effect.
    NotHonored,
}

/// One recorded measurement of one level on one route and model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReasoningMeasurement {
    /// The catalog route id.
    pub route: &'static str,
    /// The model name as the route is given it.
    pub model: &'static str,
    pub level: ReasoningEffort,
    pub outcome: LevelOutcome,
    /// What was measured, and where the record is.
    pub record: &'static str,
}

const OLLAMA_QWEN: &str = "Ollama 0.34.4, qwen3.6:35b, 2026-10-08, three requests per form on \
                           /v1/chat/completions; docs/MODEL-SUPPORT.md, Known limits";

/// Every recorded measurement.
pub const REASONING_MEASUREMENTS: &[ReasoningMeasurement] = &[
    ReasoningMeasurement {
        route: "ollama",
        model: "qwen3.6:35b",
        level: ReasoningEffort::Off,
        outcome: LevelOutcome::Honored,
        record: OLLAMA_QWEN,
    },
    ReasoningMeasurement {
        route: "ollama",
        model: "qwen3.6:35b",
        level: ReasoningEffort::Low,
        outcome: LevelOutcome::NotHonored,
        record: OLLAMA_QWEN,
    },
    ReasoningMeasurement {
        route: "ollama",
        model: "qwen3.6:35b",
        level: ReasoningEffort::Medium,
        outcome: LevelOutcome::NotHonored,
        record: OLLAMA_QWEN,
    },
    ReasoningMeasurement {
        route: "ollama",
        model: "qwen3.6:35b",
        level: ReasoningEffort::High,
        outcome: LevelOutcome::NotHonored,
        record: OLLAMA_QWEN,
    },
];

/// The recorded measurement of `level` on `route` with `model`, if any.
pub fn reasoning_measurement(
    route: &str,
    model: &str,
    level: ReasoningEffort,
) -> Option<&'static ReasoningMeasurement> {
    REASONING_MEASUREMENTS
        .iter()
        .find(|m| m.route == route && m.model == model && m.level == level)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row names a catalog route, and no route, model and level is
    /// measured twice (two rows could disagree).
    #[test]
    fn every_measurement_names_a_catalog_route_once() {
        for (i, m) in REASONING_MEASUREMENTS.iter().enumerate() {
            assert!(crate::entry(m.route).is_ok(), "{m:?}");
            assert!(
                REASONING_MEASUREMENTS[..i]
                    .iter()
                    .all(|o| (o.route, o.model, o.level) != (m.route, m.model, m.level)),
                "{m:?}"
            );
        }
    }

    #[test]
    fn the_graded_levels_on_ollama_qwen_are_recorded_as_not_honored() {
        for level in [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
        ] {
            let m = reasoning_measurement("ollama", "qwen3.6:35b", level).expect("row");
            assert_eq!(m.outcome, LevelOutcome::NotHonored);
        }
        assert_eq!(
            reasoning_measurement("ollama", "qwen3.6:35b", ReasoningEffort::Off)
                .expect("row")
                .outcome,
            LevelOutcome::Honored
        );
        assert!(reasoning_measurement("ollama", "llama3", ReasoningEffort::Low).is_none());
    }
}
