//! Live context-window occupancy per worker (0055).
//!
//! Ported in idea from Munder Difflin (`L422Y/munder-difflin`, MIT): tell
//! the orchestrator how full each live agent's context is, because an agent at 99
//! percent is effectively busy even when its cumulative token count looks modest.
//! Munder Difflin reads the tail of each vendor CLI's session log; kGraf owns
//! the loop, so the figure is read from the worker's own `Usage` events as they
//! arrive, with no log parsing. No code is copied.
//!
//! **Occupancy is the latest call's input tokens**, not a cumulative sum and not
//! the peak: a worker never compacts (0057), so its context only grows, and the
//! latest call's input (uncached plus cache read plus cache write) is the context
//! it is carrying now. The denominator is the worker's stop point (0057 addendum
//! 2), where it hands its remainder back, because that is where the worker ends,
//! not the model's window.
//!
//! **Consumers.** The status answer (its structured field and its reply text)
//! lists every live worker's occupancy, and [`OccupancyBoard::near_stop`] flags in
//! that text the workers about to hand back. A dispatch decision does not consume
//! it: a kGraf worker is never given a second contract (each attempt is a new
//! worker), so Munder Difflin's "treat a full agent as busy" has no direct
//! counterpart. Steering and peer-message delivery to a worker near its stop point
//! are the candidate consumers, not built.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// One live worker's occupancy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occupancy {
    pub worker: String,
    /// Input tokens of the worker's latest call.
    pub context_tokens: u64,
    /// The worker's stop point.
    pub stop_tokens: u64,
    /// The route's effective window.
    pub window_tokens: u64,
    /// Accounted calls observed so far.
    pub calls: u64,
}

impl Occupancy {
    /// Occupancy as a whole percent of the stop point (can exceed 100). `None`
    /// when the stop point is unknown (zero): unknown, not 0 percent.
    pub fn percent_of_stop(&self) -> Option<u64> {
        (self.context_tokens.saturating_mul(100)).checked_div(self.stop_tokens)
    }
}

/// Every live worker's occupancy, shared between the loop and the attempts.
#[derive(Debug, Default)]
pub struct OccupancyBoard {
    live: Mutex<BTreeMap<String, Occupancy>>,
}

impl OccupancyBoard {
    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Occupancy>> {
        // A panic while holding this lock left at worst one stale figure; the
        // map itself is still consistent, so recover it.
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A worker started (or resumed with `context_tokens` already carried).
    pub fn start(&self, worker: &str, context_tokens: u64, stop_tokens: u64, window_tokens: u64) {
        self.map().insert(
            worker.to_owned(),
            Occupancy {
                worker: worker.to_owned(),
                context_tokens,
                stop_tokens,
                window_tokens,
                calls: 0,
            },
        );
    }

    /// A call's usage arrived.
    pub fn observe(&self, worker: &str, input_tokens: u64) {
        if let Some(o) = self.map().get_mut(worker) {
            o.context_tokens = input_tokens;
            o.calls += 1;
        }
    }

    /// The worker ended.
    pub fn finish(&self, worker: &str) {
        self.map().remove(worker);
    }

    /// Every live worker, by name.
    pub fn snapshot(&self) -> Vec<Occupancy> {
        self.map().values().cloned().collect()
    }

    /// Workers at or past `percent` of their stop point.
    pub fn near_stop(&self, percent: u64) -> Vec<Occupancy> {
        self.map()
            .values()
            .filter(|o| o.percent_of_stop().is_some_and(|p| p >= percent))
            .cloned()
            .collect()
    }
}

/// Removes a worker from the board when its attempt ends, however it ends.
pub struct OnBoard<'a> {
    board: &'a OccupancyBoard,
    worker: String,
}

impl<'a> OnBoard<'a> {
    pub fn new(
        board: &'a OccupancyBoard,
        worker: &str,
        context: u64,
        stop: u64,
        window: u64,
    ) -> Self {
        board.start(worker, context, stop, window);
        Self {
            board,
            worker: worker.to_owned(),
        }
    }

    pub fn observe(&self, input_tokens: u64) {
        self.board.observe(&self.worker, input_tokens);
    }
}

impl Drop for OnBoard<'_> {
    fn drop(&mut self) {
        self.board.finish(&self.worker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occupancy_follows_the_latest_call_not_the_sum() {
        let board = OccupancyBoard::default();
        {
            let on = OnBoard::new(&board, "w1", 0, 80_000, 100_000);
            on.observe(20_000);
            on.observe(60_000);
            let s = board.snapshot();
            assert_eq!(s[0].context_tokens, 60_000, "latest, not 80,000 summed");
            assert_eq!(s[0].calls, 2);
            assert_eq!(s[0].percent_of_stop(), Some(75));
            assert!(board.near_stop(90).is_empty());
            on.observe(76_000);
            assert_eq!(board.near_stop(90).len(), 1, "95% of its stop point");
        }
        assert!(board.snapshot().is_empty(), "removed when the attempt ends");
    }

    #[test]
    fn a_zero_stop_point_reads_as_unknown_not_zero_percent() {
        let o = Occupancy {
            worker: "w".into(),
            context_tokens: 5,
            stop_tokens: 0,
            window_tokens: 0,
            calls: 1,
        };
        assert_eq!(o.percent_of_stop(), None);
    }
}
