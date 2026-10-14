//! How long a worker waits for the orchestrator's model to answer its
//! `ask_user_question`, and how long a connection judgment may run
//! (decision 0067, point 4, amendment of 2026-10-06).
//!
//! A fixed wait cannot fit every route: a hosted model answers in seconds, and
//! a local model under parallel load takes minutes. The wait is taken from the
//! answering model's own recent latencies in the ledger:
//!
//! 1. **A user-set wait** (the `answer_wait_secs` run limit) wins.
//! 2. **Measured:** with at least [`MIN_SAMPLES`] counted calls of the answer
//!    kinds ([`ANSWER_CALL_KINDS`]) on the model, the wait is
//!    [`FACTOR`] times the 95th percentile of the newest [`WINDOW`] of them,
//!    held between [`FLOOR`] and [`CEILING`]. With fewer, every generative call
//!    on the model counts (worker rounds measure the same route under the same
//!    load). The factor leaves room for a call slower than any seen yet; the
//!    floor keeps a short history of fast calls from cutting a normal one; the
//!    ceiling bounds the wait when the history holds a stalled call.
//! 3. **Default:** [`DEFAULT`] when the history is too short, or when it cannot
//!    be read (that is a session notice, never silent).
//!
//! Written new: the workspace has percentile helpers only in a benchmark and
//! the shell's metrics module, both private; nearest rank is three lines.

use std::time::Duration;

/// The wait when no history decides it: the former fixed limit of one paid
/// session call. A local 35B model at four parallel runs took at most 237 s
/// over 31 measured answer calls.
pub const DEFAULT: Duration = Duration::from_secs(300);
/// The shortest measured wait.
pub const FLOOR: Duration = Duration::from_secs(120);
/// The longest measured wait. Nearly four times the slowest answer call
/// measured on a local route (237 s); a call still running after it is taken
/// as stalled. The provider's own bound on a request to start answering
/// (`ProviderConfig::timeout`: 600 s hosted, 1,800 s local) is separate.
pub const CEILING: Duration = Duration::from_secs(900);
/// The measured wait is this many times the 95th percentile.
pub const FACTOR: u32 = 2;
/// Fewer timed calls than this, and the history does not decide.
pub const MIN_SAMPLES: usize = 8;
/// How many of the newest calls are read.
pub const WINDOW: usize = 50;
/// The ledger call kinds of the calls this wait bounds.
pub const ANSWER_CALL_KINDS: &[&str] = &["answer_question", "judge_connections"];
/// The longest an answer call runs after its worker stopped waiting: a late
/// answer still reaches the worker until then. Never shorter than the wait.
pub const LATE_ANSWER_LIMIT: Duration = CEILING;

/// The answering model's recent latencies, in milliseconds, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LatencyHistory {
    /// Calls of [`ANSWER_CALL_KINDS`].
    pub answer_calls: Vec<u64>,
    /// Every generative call on the model; read only when `answer_calls` is
    /// too short to decide.
    pub model_calls: Vec<u64>,
    /// Finished calls read by route, newest first, for [`crate::route_speed`].
    /// Empty unless the reader asked for them: the answer wait does not use
    /// them.
    pub route_calls: Vec<RouteCall>,
}

/// Output below this many tokens is a small call (a triage, a re-grounding, a
/// short answer); at or above it, a large one (a plan, a delta, a worker
/// round that writes).
pub const SMALL_OUTPUT_TOKENS: u64 = 400;

/// How much a call produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SizeClass {
    Small,
    Large,
}

/// One finished call on a route: what [`crate::route_speed::RouteSpeed`] is
/// computed from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteCall {
    pub route: String,
    /// Wall time from the call's start to its end, milliseconds. For a
    /// throttled attempt, the time until the 429 came back.
    pub latency_ms: u64,
    pub output_tokens: u64,
    /// Time to first token. `None` when the call did not stream; never zero.
    pub ttft_ms: Option<u64>,
    /// Rate-limit wait inside `latency_ms`. `None` when no limiter admitted
    /// the call.
    pub queued_ms: Option<u64>,
    /// The call ended on a 429, with the wait the server asked for if it
    /// named one.
    pub throttled: Option<Throttle>,
    /// Another call on the route ran during this one (otherwise lone).
    pub overlapped: bool,
}

/// A 429 answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Throttle {
    pub retry_after_ms: Option<u64>,
}

impl RouteCall {
    pub fn size_class(&self) -> SizeClass {
        if self.output_tokens < SMALL_OUTPUT_TOKENS {
            SizeClass::Small
        } else {
            SizeClass::Large
        }
    }

    /// Time spent generating: the call's wall time less its rate-limit wait.
    pub fn generation_ms(&self) -> u64 {
        self.latency_ms.saturating_sub(self.queued_ms.unwrap_or(0))
    }

    /// Output tokens per second of generation. `None` for a call with no
    /// output or no generation time.
    pub fn tokens_per_s(&self) -> Option<f64> {
        let ms = self.generation_ms();
        (self.output_tokens > 0 && ms > 0).then(|| self.output_tokens as f64 * 1_000.0 / ms as f64)
    }

    /// From a ledger row. A row with a negative span is clamped to zero.
    pub fn from_row(row: &kgraf_provider::RouteCallRow) -> Self {
        let ms = |v: i64| v.max(0) as u64;
        Self {
            route: row.route.clone(),
            latency_ms: ms(row.finished_ms - row.started_ms),
            output_tokens: row.output_tokens,
            ttft_ms: row.ttft_ms.map(ms),
            queued_ms: row.queued_ms.map(ms),
            throttled: (row.http_status == Some(429)).then_some(Throttle {
                retry_after_ms: row.retry_after_ms.map(ms),
            }),
            overlapped: row.overlapped,
        }
    }
}

/// Which calls a measured wait was taken from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeasuredFrom {
    AnswerCalls,
    ModelCalls,
}

/// Where a wait came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitSource {
    UserSet,
    Measured {
        from: MeasuredFrom,
        samples: usize,
        p95: Duration,
    },
    /// Too few timed calls (`samples`), or the history could not be read
    /// (`unreadable`).
    Default {
        samples: usize,
        unreadable: Option<String>,
    },
}

/// One wait and its source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerWait {
    pub wait: Duration,
    pub source: WaitSource,
}

impl AnswerWait {
    /// One clause for a notice: the wait and how it was chosen.
    pub fn describe(&self) -> String {
        let s = self.wait.as_secs();
        match &self.source {
            WaitSource::UserSet => format!("{s} s, set by the user"),
            WaitSource::Measured { from, samples, p95 } => format!(
                "{s} s, {FACTOR} times the 95th percentile ({} s) of the answering model's last \
                 {samples} {}, within {} to {} s",
                p95.as_secs(),
                match from {
                    MeasuredFrom::AnswerCalls => "answer calls",
                    MeasuredFrom::ModelCalls => "calls",
                },
                FLOOR.as_secs(),
                CEILING.as_secs()
            ),
            WaitSource::Default {
                unreadable: Some(e),
                ..
            } => format!(
                "{s} s, the default: the answering model's latency history could not be read \
                 ({e})"
            ),
            WaitSource::Default { samples, .. } => format!(
                "{s} s, the default: the answering model has {samples} timed call(s), fewer \
                 than {MIN_SAMPLES}"
            ),
        }
    }
}

/// The 95th percentile by nearest rank. `None` for an empty slice.
fn p95(values: &[u64]) -> Option<u64> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = (sorted.len() * 95).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

/// The wait for one call: the user's, else measured, else the default.
pub fn answer_wait(user: Option<Duration>, history: Result<LatencyHistory, String>) -> AnswerWait {
    if let Some(wait) = user {
        return AnswerWait {
            wait,
            source: WaitSource::UserSet,
        };
    }
    let history = match history {
        Ok(h) => h,
        Err(e) => {
            return AnswerWait {
                wait: DEFAULT,
                source: WaitSource::Default {
                    samples: 0,
                    unreadable: Some(e),
                },
            };
        }
    };
    let (from, calls) = if history.answer_calls.len() >= MIN_SAMPLES {
        (MeasuredFrom::AnswerCalls, &history.answer_calls)
    } else {
        (MeasuredFrom::ModelCalls, &history.model_calls)
    };
    let calls = &calls[..calls.len().min(WINDOW)];
    match p95(calls).filter(|_| calls.len() >= MIN_SAMPLES) {
        Some(p) => {
            let p95 = Duration::from_millis(p);
            AnswerWait {
                wait: (p95 * FACTOR).clamp(FLOOR, CEILING),
                source: WaitSource::Measured {
                    from,
                    samples: calls.len(),
                    p95,
                },
            }
        }
        None => AnswerWait {
            wait: DEFAULT,
            source: WaitSource::Default {
                samples: history.answer_calls.len().max(history.model_calls.len()),
                unreadable: None,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(v: &[u64]) -> Vec<u64> {
        v.iter().map(|s| s * 1_000).collect()
    }

    /// A measured local route (answer calls of median 44 s, p90 126 s, max
    /// 237 s) gets a wait above every call it made.
    #[test]
    fn a_slow_route_gets_a_wait_above_its_measured_latency() {
        let calls = secs(&[
            44, 30, 126, 237, 52, 41, 39, 46, 36, 33, 26, 122, 51, 22, 47, 45,
        ]);
        let w = answer_wait(
            None,
            Ok(LatencyHistory {
                answer_calls: calls.clone(),
                model_calls: Vec::new(),
                ..Default::default()
            }),
        );
        assert!(
            matches!(
                w.source,
                WaitSource::Measured {
                    from: MeasuredFrom::AnswerCalls,
                    samples: 16,
                    ..
                }
            ),
            "{w:?}"
        );
        let slowest = Duration::from_millis(*calls.iter().max().expect("calls"));
        assert!(w.wait > slowest, "{w:?} is not above {slowest:?}");
        assert!(w.wait > DEFAULT, "a slower route waits longer than before");
        assert_eq!(w.wait, Duration::from_secs(474), "{}", w.describe());
    }

    /// A fast route's wait is held at the floor, and a stalled call in the
    /// history cannot push the wait past the ceiling.
    #[test]
    fn the_measured_wait_is_held_between_the_floor_and_the_ceiling() {
        let fast = answer_wait(
            None,
            Ok(LatencyHistory {
                answer_calls: secs(&[2; 10]),
                model_calls: Vec::new(),
                ..Default::default()
            }),
        );
        assert_eq!(fast.wait, FLOOR);
        let stalled = answer_wait(
            None,
            Ok(LatencyHistory {
                answer_calls: secs(&[3_000; 10]),
                model_calls: Vec::new(),
                ..Default::default()
            }),
        );
        assert_eq!(stalled.wait, CEILING);
    }

    /// With too few answer calls, the model's other calls decide.
    #[test]
    fn too_few_answer_calls_fall_back_to_the_models_other_calls() {
        let w = answer_wait(
            None,
            Ok(LatencyHistory {
                answer_calls: secs(&[10; 3]),
                model_calls: secs(&[200; 12]),
                ..Default::default()
            }),
        );
        assert_eq!(w.wait, Duration::from_secs(400));
        assert!(matches!(
            w.source,
            WaitSource::Measured {
                from: MeasuredFrom::ModelCalls,
                ..
            }
        ));
    }

    /// The user's wait wins over any history.
    #[test]
    fn the_user_wait_wins() {
        let w = answer_wait(
            Some(Duration::from_secs(45)),
            Ok(LatencyHistory {
                answer_calls: secs(&[200; 20]),
                model_calls: Vec::new(),
                ..Default::default()
            }),
        );
        assert_eq!(w.wait, Duration::from_secs(45));
        assert_eq!(w.source, WaitSource::UserSet);
        assert!(w.describe().contains("set by the user"), "{}", w.describe());
    }

    /// No history, or one that cannot be read, gives the stated default, and
    /// the description says why.
    #[test]
    fn a_missing_history_falls_back_to_the_stated_default() {
        let empty = answer_wait(None, Ok(LatencyHistory::default()));
        assert_eq!(empty.wait, DEFAULT);
        assert_eq!(DEFAULT, Duration::from_secs(300));
        assert!(
            empty.describe().contains("the default") && empty.describe().contains("0 timed"),
            "{}",
            empty.describe()
        );
        let short = answer_wait(
            None,
            Ok(LatencyHistory {
                answer_calls: secs(&[500; 7]),
                model_calls: secs(&[500; 7]),
                ..Default::default()
            }),
        );
        assert_eq!(short.wait, DEFAULT, "seven calls do not decide");
        let broken = answer_wait(None, Err("disk I/O error".into()));
        assert_eq!(broken.wait, DEFAULT);
        assert!(
            broken
                .describe()
                .contains("could not be read (disk I/O error)"),
            "{}",
            broken.describe()
        );
    }

    #[test]
    fn p95_is_the_nearest_rank() {
        assert_eq!(p95(&[]), None);
        assert_eq!(p95(&[7]), Some(7));
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(p95(&v), Some(95));
        let v: Vec<u64> = (1..=20).collect();
        assert_eq!(p95(&v), Some(19));
    }
}
