//! What is recorded about routes acting on a forced tool choice
//! (`kgraf_provider::ToolChoice::Required`), kept by the rule decision 0098
//! keeps for reasoning levels: a parameter recorded as ignored is not sent,
//! and one with no record is sent and said to be unverified.
//!
//! An answer does not show whether the field was read: a server that drops
//! it answers 200 as if it were absent. So only a record can tell. A row is
//! added only from a recorded live exchange or from the server's own source
//! showing the field is never read (that proves "ignored", never "acted
//! on"), and names where it is documented. A row with no model covers every
//! model on the route: whether the field is read is the server's doing.

/// What a record found the route does with a forced tool choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolChoiceOutcome {
    /// The reply was made a tool call as asked.
    Honored,
    /// The field is accepted and has no effect.
    Ignored,
}

/// One record of one route, for one model or for all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolChoiceMeasurement {
    /// The catalog route id.
    pub route: &'static str,
    /// The model name as the route is given it; `None` for every model.
    pub model: Option<&'static str>,
    pub outcome: ToolChoiceOutcome,
    /// What was found, and where the record is.
    pub record: &'static str,
}

/// Every record.
pub const TOOL_CHOICE_MEASUREMENTS: &[ToolChoiceMeasurement] = &[ToolChoiceMeasurement {
    route: "ollama",
    model: None,
    outcome: ToolChoiceOutcome::Ignored,
    record: "Ollama 0.34.4 source, openai/openai.go: the Chat Completions request type has no \
             tool_choice field and the handler binds the body with gin's ShouldBindJSON, which \
             drops fields it does not know, so the request runs as if the field were absent; \
             read 2026-10-10; docs/MODEL-SUPPORT.md, Known limits",
}];

/// The record for `route` with `model`: a row for that model wins over a
/// row for every model on the route.
pub fn tool_choice_measurement(route: &str, model: &str) -> Option<&'static ToolChoiceMeasurement> {
    let on_route = || TOOL_CHOICE_MEASUREMENTS.iter().filter(|m| m.route == route);
    on_route()
        .find(|m| m.model == Some(model))
        .or_else(|| on_route().find(|m| m.model.is_none()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row names a catalog route, and no route and model is recorded
    /// twice (two rows could disagree).
    #[test]
    fn every_record_names_a_catalog_route_once() {
        for (i, m) in TOOL_CHOICE_MEASUREMENTS.iter().enumerate() {
            assert!(crate::entry(m.route).is_ok(), "{m:?}");
            assert!(
                TOOL_CHOICE_MEASUREMENTS[..i]
                    .iter()
                    .all(|o| (o.route, o.model) != (m.route, m.model)),
                "{m:?}"
            );
        }
    }

    #[test]
    fn ollama_is_recorded_as_ignoring_it_for_every_model_and_others_are_unrecorded() {
        for model in ["qwen3.6:35b", "llama3"] {
            let m = tool_choice_measurement("ollama", model).expect("row");
            assert_eq!(m.outcome, ToolChoiceOutcome::Ignored);
            assert!(m.record.contains("Ollama 0.34.4"), "{}", m.record);
        }
        assert!(tool_choice_measurement("llama-cpp", "any").is_none());
        assert!(tool_choice_measurement("anthropic", "any").is_none());
    }
}
