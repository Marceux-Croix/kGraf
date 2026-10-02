//! Stall detection (decision 0056): repeated identical tool calls inside the step
//! budget end the run with a typed outcome.
//!
//! Written new: 0056 records that no permissive library provides this.
//!
//! **Identity.** A call is its tool's wire name plus its input in a canonical JSON
//! form (object keys sorted, recursively). The canonical form is computed here and
//! not taken from `serde_json::to_vec`: this workspace enables serde_json's
//! `preserve_order` feature through feature unification, so two inputs that differ
//! only in key order would otherwise hash differently and a stall would go unseen.
//!
//! **Only approved calls count.** A denied call did nothing, so repeating it is not
//! a worker going round in circles on the workspace; the loop counts denials apart
//! (`WorkerResult::denied`).
//!
//! **Counting.** Every call counts, not only consecutive ones: a worker cycling
//! A, B, A, B is as stuck as one repeating A. The threshold is the number of times
//! one identical call may be issued; reaching it is the stall. Because the step
//! budget bounds the run, the counter is bounded too.
//!
//! **A change to the workspace starts a new count.** Running the same test after
//! each edit, or reading a file again after writing it, is progress and not a
//! loop. The worker calls [`StallDetector::reset`] after a call that wrote a
//! path succeeds, so identical calls only count while the workspace stays as it
//! was. A write that fails does not reset, so a worker repeating a failing edit
//! still stalls. Repeating one successful write is bounded by the step budget.
//!
//! **A masked result starts a new count.** The worker's observation masker
//! replaces a tool result with a placeholder once it leaves the last few
//! rounds. A worker that then repeats the call is re-reading what it can no
//! longer see, not going in circles. A call whose previous issue is older than
//! the masker's window ([`StallDetector::with_window`]) counts from one again, when
//! the masker did hide that result ([`StallDetector::note_result`]; a short result
//! is never hidden). A read-only worker, which never writes and so never resets, would otherwise
//! reach the threshold by re-reading a masked file.

use std::collections::HashMap;

/// The default number of identical calls that marks a stall.
pub const DEFAULT_STALL_REPEATS: u32 = 3;

/// A digest of one call: blake3 over the wire name and canonical input, hex.
pub fn call_digest(tool: &str, input: &serde_json::Value) -> String {
    let mut h = blake3::Hasher::new();
    h.update(&(tool.len() as u64).to_le_bytes());
    h.update(tool.as_bytes());
    canonical(input, &mut h);
    h.finalize().to_hex().to_string()
}

/// Feed a canonical encoding of `value` to the hasher: objects with sorted keys,
/// every item length-prefixed or tagged so no two values share an encoding.
fn canonical(value: &serde_json::Value, h: &mut blake3::Hasher) {
    use serde_json::Value;
    match value {
        Value::Null => {
            h.update(b"n");
        }
        Value::Bool(b) => {
            h.update(if *b { b"t" } else { b"f" });
        }
        Value::Number(n) => {
            let text = n.to_string();
            h.update(b"#");
            h.update(&(text.len() as u64).to_le_bytes());
            h.update(text.as_bytes());
        }
        Value::String(s) => {
            // Whitespace-insensitive: runs of whitespace compare as one space and
            // the ends are trimmed, so a model re-issuing the same call with
            // reflowed text is still seen repeating. The trade-off: two writes that
            // differ only in indentation count as one call.
            let normalized = s.split_whitespace().collect::<Vec<_>>().join(" ");
            h.update(b"s");
            h.update(&(normalized.len() as u64).to_le_bytes());
            h.update(normalized.as_bytes());
        }
        Value::Array(items) => {
            h.update(b"[");
            h.update(&(items.len() as u64).to_le_bytes());
            for item in items {
                canonical(item, h);
            }
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            h.update(b"{");
            h.update(&(keys.len() as u64).to_le_bytes());
            for key in keys {
                h.update(&(key.len() as u64).to_le_bytes());
                h.update(key.as_bytes());
                if let Some(v) = map.get(key) {
                    canonical(v, h);
                }
            }
        }
    }
}

/// A detected stall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stall {
    pub call_digest: String,
    pub repeats: u32,
    /// The repeated call, as `tool input`, cut at [`STALL_CALL_CHARS`].
    pub call: String,
}

/// The longest repeated-call text a stall keeps, in characters.
pub const STALL_CALL_CHARS: usize = 300;

/// `tool input` for a call, cut at [`STALL_CALL_CHARS`].
fn describe_call(tool: &str, input: &serde_json::Value) -> String {
    let text = format!("{tool} {input}");
    if text.chars().count() > STALL_CALL_CHARS {
        let head: String = text.chars().take(STALL_CALL_CHARS).collect();
        format!("{head}...")
    } else {
        text
    }
}

/// Counts identical calls over a run.
#[derive(Clone, Debug)]
pub struct StallDetector {
    threshold: u32,
    /// Per call digest: how many times it was issued, in which observed round
    /// it was last issued, and whether the result of that last issue is one the
    /// masker hides once it leaves the window (`note_result`).
    seen: HashMap<String, (u32, u64, bool)>,
    /// Observed rounds so far (one per [`StallDetector::observe`] call).
    round: u64,
    /// How many recent rounds keep their results unmasked, when results are
    /// masked at all.
    window: Option<u64>,
}

impl StallDetector {
    /// A detector that reports a stall when one call has been issued `threshold`
    /// times. A threshold below 2 would call the first repeat of anything a stall
    /// and is raised to 2.
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold: threshold.max(2),
            seen: HashMap::new(),
            round: 0,
            window: None,
        }
    }

    /// A result stays visible for `window` rounds after the round that issued
    /// it; a call repeated later than that counts from one, but only when its
    /// result was one the masker hides (`note_result`): a short result is never
    /// masked, so the model still sees it and a repeat is still a loop. `None`:
    /// results are never masked.
    pub fn with_window(mut self, window: Option<usize>) -> Self {
        self.window = window.map(|w| w as u64);
        self
    }

    /// Forget every call seen: the workspace changed, so an identical call may
    /// now give a different result.
    pub fn reset(&mut self) {
        self.seen.clear();
    }

    /// Say whether the result of the call with `digest` (see [`call_digest`])
    /// is one the masker hides after the window. A call whose result was not
    /// reported here counts as not masked, so its repeats are never forgiven.
    pub fn note_result(&mut self, digest: &str, masked_when_old: bool) {
        if let Some(entry) = self.seen.get_mut(digest) {
            entry.2 = masked_when_old;
        }
    }

    /// Record one round's calls, in order (an empty round still advances the
    /// round count that expires masked results). Returns the first stall reached, if any.
    /// Every call is counted even after a stall is found, so the counts stay a
    /// true record of the round.
    pub fn observe<'a>(
        &mut self,
        calls: impl IntoIterator<Item = (&'a str, &'a serde_json::Value)>,
    ) -> Option<Stall> {
        let mut stall = None;
        self.round += 1;
        for (tool, input) in calls {
            let digest = call_digest(tool, input);
            let entry = self
                .seen
                .entry(digest.clone())
                .or_insert((0, self.round, false));
            if let Some(window) = self.window
                && self.round - entry.1 > window
                && entry.2
            {
                // The last result of this call has been masked.
                entry.0 = 0;
            }
            entry.2 = false;
            entry.0 += 1;
            entry.1 = self.round;
            if stall.is_none() && entry.0 >= self.threshold {
                stall = Some(Stall {
                    call_digest: digest,
                    repeats: entry.0,
                    call: describe_call(tool, input),
                });
            }
        }
        stall
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_order_does_not_change_the_digest_but_values_do() {
        // serde_json's preserve_order is on in this workspace; build both orders.
        let a: serde_json::Value = serde_json::from_str(r#"{"path":"a","n":1}"#).expect("a");
        let b: serde_json::Value = serde_json::from_str(r#"{"n":1,"path":"a"}"#).expect("b");
        assert_eq!(call_digest("read", &a), call_digest("read", &b));
        assert_ne!(
            call_digest("read", &a),
            call_digest("read", &json!({"path": "b", "n": 1}))
        );
        assert_ne!(call_digest("read", &a), call_digest("write", &a));
        // A string "1" is not the number 1.
        assert_ne!(
            call_digest("t", &json!({"n": "1"})),
            call_digest("t", &json!({"n": 1}))
        );
    }

    #[test]
    fn whitespace_differences_do_not_change_the_digest() {
        assert_eq!(
            call_digest("shell", &json!({"cmd": "cargo  test\n --all "})),
            call_digest("shell", &json!({"cmd": "cargo test --all"}))
        );
        assert_ne!(
            call_digest("shell", &json!({"cmd": "cargo test"})),
            call_digest("shell", &json!({"cmd": "cargotest"}))
        );
    }

    #[test]
    fn the_third_identical_call_is_a_stall_even_when_interleaved() {
        let mut d = StallDetector::new(3);
        let a = json!({"path": "src/lib.rs"});
        let b = json!({"path": "src/main.rs"});
        assert_eq!(d.observe([("read", &a)]), None);
        assert_eq!(d.observe([("read", &b)]), None);
        assert_eq!(d.observe([("read", &a)]), None);
        assert_eq!(d.observe([("read", &b)]), None);
        let stall = d.observe([("read", &a)]).expect("third identical call");
        assert_eq!(stall.repeats, 3);
        assert_eq!(stall.call_digest, call_digest("read", &a));
    }

    #[test]
    fn repeats_inside_one_round_count() {
        let mut d = StallDetector::new(2);
        let a = json!({});
        assert!(d.observe([("t", &a), ("t", &a)]).is_some());
    }

    #[test]
    fn a_reset_starts_a_new_count() {
        let mut d = StallDetector::new(3);
        let a = json!({"cmd": "cargo test"});
        assert_eq!(d.observe([("shell", &a)]), None);
        assert_eq!(d.observe([("shell", &a)]), None);
        d.reset();
        assert_eq!(d.observe([("shell", &a)]), None);
        assert_eq!(d.observe([("shell", &a)]), None);
        assert!(d.observe([("shell", &a)]).is_some());
    }

    #[test]
    fn a_repeat_after_the_result_was_masked_starts_a_new_count() {
        // Results stay visible for 2 rounds. A read-only worker re-reads a file
        // it can no longer see: never a stall, however often.
        let mut d = StallDetector::new(3).with_window(Some(2));
        let a = json!({"path": "src/lib.rs"});
        let other = json!({"path": "elsewhere"});
        let digest = call_digest("read", &a);
        for round in 0..30 {
            assert_eq!(d.observe([("read", &a)]), None, "round {round}");
            d.note_result(&digest, true);
            // Two other rounds: the result of `a` leaves the window.
            assert_eq!(d.observe([("read", &other)]), None);
            d.note_result(&call_digest("read", &other), true);
            assert_eq!(d.observe([("shell", &json!({"n": round}))]), None);
        }
    }

    #[test]
    fn a_repeat_of_a_result_the_masker_never_hides_still_counts() {
        // A short result is never masked: the model sees it every round, so a
        // cycle of calls longer than the window is still a loop.
        let mut d = StallDetector::new(3).with_window(Some(2));
        let calls: Vec<serde_json::Value> = (0..6).map(|n| json!({ "n": n })).collect();
        let mut stalled = false;
        for _cycle in 0..3 {
            for c in &calls {
                stalled |= d.observe([("t", c)]).is_some();
                d.note_result(&call_digest("t", c), false);
            }
        }
        assert!(
            stalled,
            "a 6-call cycle of unmasked results reaches the threshold"
        );
    }

    #[test]
    fn a_repeat_while_the_result_is_visible_still_counts_with_a_window() {
        let mut d = StallDetector::new(3).with_window(Some(2));
        let a = json!({"path": "src/lib.rs"});
        let other = json!({"path": "elsewhere"});
        assert_eq!(d.observe([("read", &a)]), None);
        assert_eq!(d.observe([("read", &other)]), None);
        assert_eq!(d.observe([("read", &a)]), None);
        assert_eq!(d.observe([("read", &other)]), None);
        assert!(d.observe([("read", &a)]).is_some());
    }

    #[test]
    fn a_threshold_below_two_is_raised() {
        let mut d = StallDetector::new(0);
        assert_eq!(d.observe([("t", &json!({}))]), None);
    }
}
