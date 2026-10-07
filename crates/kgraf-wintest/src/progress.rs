//! Step accounting, the time estimate and the one-line progress bar.
//!
//! Pure: every method takes the current time as seconds since the run
//! started, so the estimate and the rendered line are unit-tested without a
//! clock. `console.rs` owns the terminal and calls [`Progress::render`].
//!
//! `indicatif` was considered. It draws well on Windows, but the estimate
//! here is weighted by expected step durations rather than by a position
//! counter, and the rendered line must be a pure string for the tests; a
//! single line redrawn with a carriage return is all the console needs.

/// Seconds of prior work assumed at the expected rate. The observed rate is
/// pulled toward 1.0 by this much, so one fast or slow early step does not
/// swing the estimate.
const PRIOR_SECS: f64 = 60.0;
/// No estimate is shown before this many seconds of the run.
const WARMUP_SECS: f64 = 10.0;
/// A step that runs this many times its expected time is marked as long.
const LONG_FACTOR: f64 = 2.0;

#[derive(Clone, Debug, PartialEq)]
pub enum StepState {
    Pending,
    Running { started: f64 },
    Done { secs: f64 },
}

#[derive(Clone, Debug)]
pub struct Step {
    pub name: String,
    /// Expected seconds on a reference machine; the observed rate scales it.
    pub expected_secs: f64,
    pub state: StepState,
}

#[derive(Clone, Debug, Default)]
pub struct Progress {
    steps: Vec<Step>,
    current: Option<usize>,
    /// What the running step is doing now ("test 3 of 12").
    pub detail: String,
    pub passed: u64,
    pub failed: u64,
}

/// The time estimate to show next to the bar.
#[derive(Clone, Debug, PartialEq)]
pub enum Eta {
    Estimating,
    /// Seconds to total completion; never negative.
    About(f64),
    Finished,
}

impl Progress {
    pub fn new(steps: Vec<(String, f64)>) -> Self {
        Self {
            steps: steps
                .into_iter()
                .map(|(name, expected_secs)| Step {
                    name,
                    expected_secs: expected_secs.max(0.1),
                    state: StepState::Pending,
                })
                .collect(),
            ..Self::default()
        }
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Add a step at the end (the extended set is added once it is chosen).
    pub fn push(&mut self, name: String, expected_secs: f64) {
        self.steps.push(Step {
            name,
            expected_secs: expected_secs.max(0.1),
            state: StepState::Pending,
        });
    }

    /// Replace a step's expected time (once its test count is known).
    pub fn set_expected(&mut self, index: usize, expected_secs: f64) {
        if let Some(s) = self.steps.get_mut(index) {
            s.expected_secs = expected_secs.max(0.1);
        }
    }

    /// Start step `index`; a running step is finished first.
    pub fn start(&mut self, index: usize, now: f64) {
        if let Some(cur) = self.current
            && cur != index
        {
            self.finish(cur, now);
        }
        if let Some(s) = self.steps.get_mut(index) {
            s.state = StepState::Running { started: now };
            self.current = Some(index);
            self.detail.clear();
        }
    }

    pub fn finish(&mut self, index: usize, now: f64) {
        if let Some(s) = self.steps.get_mut(index)
            && let StepState::Running { started } = s.state
        {
            s.state = StepState::Done {
                secs: (now - started).max(0.0),
            };
        }
        if self.current == Some(index) {
            self.current = None;
        }
    }

    pub fn done_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| matches!(s.state, StepState::Done { .. }))
            .count()
    }

    /// Fraction of the expected work done, 0.0 to 1.0. The running step
    /// counts up to its expected time, so the bar never runs backward.
    pub fn fraction(&self, now: f64) -> f64 {
        let total: f64 = self.steps.iter().map(|s| s.expected_secs).sum();
        if total <= 0.0 {
            return 1.0;
        }
        let done: f64 = self
            .steps
            .iter()
            .map(|s| match s.state {
                StepState::Done { .. } => s.expected_secs,
                // Up to 95 percent of a running step: the last part is shown
                // only when the step really ends.
                StepState::Running { started } => {
                    (now - started).clamp(0.0, s.expected_secs * 0.95)
                }
                StepState::Pending => 0.0,
            })
            .sum();
        (done / total).clamp(0.0, 1.0)
    }

    /// Actual seconds per expected second so far, pulled toward 1.0 by
    /// [`PRIOR_SECS`] of assumed work at the expected rate. A running step
    /// that has overrun its expected time counts its overrun: it is evidence
    /// that this machine is slower.
    pub fn rate(&self, now: f64) -> f64 {
        let mut expected = 0.0;
        let mut actual = 0.0;
        for s in &self.steps {
            match s.state {
                StepState::Done { secs } => {
                    expected += s.expected_secs;
                    actual += secs;
                }
                StepState::Running { started } => {
                    let e = (now - started).max(0.0);
                    if e > s.expected_secs {
                        expected += s.expected_secs;
                        actual += e;
                    }
                }
                StepState::Pending => {}
            }
        }
        (actual + PRIOR_SECS) / (expected + PRIOR_SECS)
    }

    /// The estimate to total completion: the remaining expected work divided
    /// by the observed rate. The running step never counts less than a tenth
    /// of its scaled expected time, so an overrun does not show zero.
    pub fn eta(&self, now: f64) -> Eta {
        if self
            .steps
            .iter()
            .all(|s| matches!(s.state, StepState::Done { .. }))
        {
            return Eta::Finished;
        }
        if now < WARMUP_SECS {
            return Eta::Estimating;
        }
        let r = self.rate(now);
        let mut remaining = 0.0;
        for s in &self.steps {
            match s.state {
                StepState::Pending => remaining += s.expected_secs * r,
                StepState::Running { started } => {
                    let scaled = s.expected_secs * r;
                    let e = (now - started).max(0.0);
                    remaining += (scaled - e).max(scaled * 0.1);
                }
                StepState::Done { .. } => {}
            }
        }
        Eta::About(remaining.max(0.0))
    }

    /// True when the running step has passed twice its expected time.
    pub fn current_is_long(&self, now: f64) -> bool {
        self.current
            .and_then(|i| self.steps.get(i))
            .is_some_and(|s| match s.state {
                StepState::Running { started } => now - started > s.expected_secs * LONG_FACTOR,
                _ => false,
            })
    }

    pub fn current_name(&self) -> Option<&str> {
        self.current
            .and_then(|i| self.steps.get(i))
            .map(|s| s.name.as_str())
    }

    fn current_elapsed(&self, now: f64) -> f64 {
        self.current
            .and_then(|i| self.steps.get(i))
            .map_or(0.0, |s| match s.state {
                StepState::Running { started } => (now - started).max(0.0),
                _ => 0.0,
            })
    }

    /// One console line, at most `width - 1` characters so it never wraps.
    /// `tick` turns the spinner.
    pub fn render(&self, now: f64, tick: usize, width: usize) -> String {
        const BAR: usize = 20;
        const SPIN: [char; 4] = ['|', '/', '-', '\\'];
        let frac = self.fraction(now);
        let filled = ((frac * BAR as f64).round() as usize).min(BAR);
        let bar: String = "#".repeat(filled) + &"-".repeat(BAR - filled);
        let eta = match self.eta(now) {
            Eta::Estimating => "ETA estimating...".to_owned(),
            Eta::Finished => "done".to_owned(),
            Eta::About(secs) => format!("ETA {}", eta_words(secs)),
        };
        let mut line = format!(
            "[{bar}] {:>3}% {}/{} {} {} | {}",
            (frac * 100.0).floor() as u32,
            self.done_count(),
            self.steps.len(),
            clock(now),
            eta,
            self.current_name().unwrap_or("-"),
        );
        if !self.detail.is_empty() {
            line.push(' ');
            line.push_str(&self.detail);
        }
        line.push_str(&format!(
            " | {} ok {} failed | {} {}s",
            self.passed,
            self.failed,
            SPIN[tick % SPIN.len()],
            self.current_elapsed(now) as u64
        ));
        if self.current_is_long(now) {
            line.push_str(" (longer than expected)");
        }
        truncate_chars(&line, width.saturating_sub(1).max(20))
    }
}

/// "about 7 min", rounded so the number does not flicker: under a minute,
/// whole minutes under 10, then steps of 5.
pub fn eta_words(secs: f64) -> String {
    let mins = secs / 60.0;
    if mins < 1.0 {
        "under 1 min".to_owned()
    } else if mins < 10.0 {
        format!("about {} min", mins.round() as u64)
    } else {
        format!("about {} min", ((mins / 5.0).round() * 5.0) as u64)
    }
}

/// `m:ss`, or `h:mm:ss` past an hour.
pub fn clock(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three() -> Progress {
        Progress::new(vec![
            ("a".into(), 100.0),
            ("b".into(), 100.0),
            ("c".into(), 200.0),
        ])
    }

    #[test]
    fn the_fraction_is_weighted_by_expected_time_not_step_count() {
        let mut p = three();
        p.start(0, 0.0);
        p.finish(0, 100.0);
        // One of three steps, but a quarter of the expected work.
        assert!((p.fraction(100.0) - 0.25).abs() < 1e-9);
        assert_eq!(p.done_count(), 1);
    }

    #[test]
    fn a_slow_machine_raises_the_estimate() {
        let mut fast = three();
        fast.start(0, 0.0);
        fast.finish(0, 100.0);
        fast.start(1, 100.0);
        let mut slow = three();
        slow.start(0, 0.0);
        slow.finish(0, 300.0);
        slow.start(1, 300.0);
        let (Eta::About(f), Eta::About(s)) = (fast.eta(100.0), slow.eta(300.0)) else {
            panic!("both runs are past the warm-up");
        };
        // At the expected rate: 100 + 200 seconds remain.
        assert!((f - 300.0).abs() < 1e-6, "{f}");
        // Three times slower, pulled toward 1.0 by the prior: (300+60)/(100+60).
        let r = 360.0 / 160.0;
        assert!((s - 300.0 * r).abs() < 1e-6, "{s}");
        assert!(s > f);
    }

    #[test]
    fn an_overrunning_step_never_shows_zero_or_negative() {
        let mut p = three();
        p.start(0, 0.0);
        p.finish(0, 100.0);
        p.start(1, 100.0);
        // Step b expected 100 s, now 400 s in.
        let Eta::About(secs) = p.eta(500.0) else {
            panic!("estimate expected");
        };
        assert!(secs > 0.0);
        // At least the pending step c and a tenth of b, each scaled.
        let r = p.rate(500.0);
        assert!(secs >= 200.0 * r + 10.0 * r - 1e-6, "{secs}");
        assert!(p.current_is_long(500.0));
        assert!(!p.current_is_long(150.0));
        assert!(p.render(500.0, 0, 200).contains("longer than expected"));
    }

    #[test]
    fn the_first_seconds_say_estimating_and_the_end_says_done() {
        let mut p = three();
        p.start(0, 0.0);
        assert_eq!(p.eta(5.0), Eta::Estimating);
        assert!(p.render(5.0, 0, 200).contains("estimating"));
        p.finish(0, 20.0);
        p.start(1, 20.0);
        p.finish(1, 30.0);
        p.start(2, 30.0);
        p.finish(2, 40.0);
        assert_eq!(p.eta(40.0), Eta::Finished);
    }

    #[test]
    fn eta_words_round_so_the_number_does_not_flicker() {
        assert_eq!(eta_words(30.0), "under 1 min");
        assert_eq!(eta_words(7.4 * 60.0), "about 7 min");
        assert_eq!(eta_words(23.0 * 60.0), "about 25 min");
        assert_eq!(clock(3725.0), "1:02:05");
        assert_eq!(clock(65.0), "1:05");
    }

    #[test]
    fn the_line_fits_the_console_and_shows_counts_and_the_step() {
        let mut p = three();
        p.start(0, 0.0);
        p.detail = "test 3 of 12".into();
        p.passed = 7;
        p.failed = 1;
        let line = p.render(12.0, 1, 200);
        assert!(line.contains("a test 3 of 12"), "{line}");
        assert!(line.contains("7 ok 1 failed"), "{line}");
        assert!(line.contains("/ 12s"), "{line}");
        assert!(line.starts_with("[#-"), "{line}");
        assert!(p.render(12.0, 1, 40).chars().count() <= 39);
    }
}
