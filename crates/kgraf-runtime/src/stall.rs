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
}

/// Counts identical calls over a run.
#[derive(Clone, Debug)]
pub struct StallDetector {
    threshold: u32,
    seen: HashMap<String, u32>,
}

impl StallDetector {
    /// A detector that reports a stall when one call has been issued `threshold`
    /// times. A threshold below 2 would call the first repeat of anything a stall
    /// and is raised to 2.
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold: threshold.max(2),
            seen: HashMap::new(),
        }
    }

    /// Forget every call seen: the workspace changed, so an identical call may
    /// now give a different result.
    pub fn reset(&mut self) {
        self.seen.clear();
    }

    /// Record a round's calls, in order. Returns the first stall reached, if any.
    /// Every call is counted even after a stall is found, so the counts stay a
    /// true record of the round.
    pub fn observe<'a>(
        &mut self,
        calls: impl IntoIterator<Item = (&'a str, &'a serde_json::Value)>,
    ) -> Option<Stall> {
        let mut stall = None;
        for (tool, input) in calls {
            let digest = call_digest(tool, input);
            let count = self.seen.entry(digest.clone()).or_insert(0);
            *count += 1;
            if stall.is_none() && *count >= self.threshold {
                stall = Some(Stall {
                    call_digest: digest,
                    repeats: *count,
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
    fn a_threshold_below_two_is_raised() {
        let mut d = StallDetector::new(0);
        assert_eq!(d.observe([("t", &json!({}))]), None);
    }
}
