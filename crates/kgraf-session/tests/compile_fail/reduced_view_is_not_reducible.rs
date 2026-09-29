//! A caller tries to reduce a `ReducedView`'s own output, skipping the rule
//! that reduction always starts from the original transcript (decision 0057).

use kgraf_session::reduction::{MaskConfig, Reducible, TokenBudget, reduce};
use std::collections::BTreeSet;

#[derive(Clone)]
struct Event(String);

impl Reducible for Event {
    fn approx_tokens(&self) -> usize {
        self.0.len()
    }
    fn as_observation(&self) -> Option<&str> {
        Some(&self.0)
    }
    fn masked(&self, placeholder: &str) -> Self {
        Event(placeholder.to_owned())
    }
}

fn main() {
    let original = vec![Event("a".into()), Event("b".into())];
    let view = reduce(
        &original,
        &BTreeSet::new(),
        TokenBudget(1000),
        &MaskConfig::default(),
    );
    // `view.entries` is `Vec<Reduced<Event>>`; `Reduced<Event>` has no
    // `Reducible` impl, so this does not type-check.
    let _again = reduce(
        &view.entries,
        &BTreeSet::new(),
        TokenBudget(1000),
        &MaskConfig::default(),
    );
}
