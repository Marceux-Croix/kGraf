//! Startup and frame-time measurement for the 0054 confirmation budgets.
//!
//! The numbers set the shell's budgets, so they are recorded in-process where the frame
//! loop is visible, and printed by the binary's `--measure` mode.

use std::time::{Duration, Instant};

/// Summary statistics over frame samples, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub count: usize,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

/// Nearest-rank percentile over an ascending slice. Pure.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Summarizes samples in milliseconds. `None` when there are no samples.
pub fn summarize(samples_ms: &[f64]) -> Option<Summary> {
    if samples_ms.is_empty() {
        return None;
    }
    let mut sorted = samples_ms.to_vec();
    sorted.sort_by(f64::total_cmp);
    let sum: f64 = sorted.iter().sum();
    Some(Summary {
        count: sorted.len(),
        mean_ms: sum / sorted.len() as f64,
        p50_ms: percentile(&sorted, 50.0),
        p95_ms: percentile(&sorted, 95.0),
        max_ms: *sorted.last().unwrap_or(&0.0),
    })
}

/// What happened to the previous frame, known only at the start of the next one:
/// egui-wgpu acquires the surface after `ui` returns, and reports a failed acquire
/// through the drop counters (`crate::surface`). Pure.
pub fn previous_frame_dropped(drops_seen: u64, drops_now: u64) -> bool {
    drops_now > drops_seen
}

/// Frame timeline collected by the app. Only frames that reached the screen count
/// toward intervals, CPU time and "first frame presented".
#[derive(Debug)]
pub struct FrameClock {
    process_start: Instant,
    /// Start of the first UI pass, presented or not.
    first_ui_start: Option<Duration>,
    /// When the first presented frame was confirmed (the start of the frame after it).
    /// This is the upper bound for "cold start to first frame".
    first_frame_presented: Option<Duration>,
    /// Start of the previous frame, whose outcome is not yet known.
    pending: Option<Instant>,
    /// Start of the last presented frame, if the frame right after it is `pending`.
    /// Cleared by a drop so an interval never spans a skipped frame.
    last_presented_start: Option<Instant>,
    /// Drop-counter total already attributed to earlier frames.
    drops_seen: u64,
    /// Start-to-start time between consecutive presented frames.
    intervals_ms: Vec<f64>,
    /// CPU time egui reports per presented frame (`Frame::info().cpu_usage`).
    cpu_ms: Vec<f64>,
    presented: usize,
    dropped: usize,
    /// Outcome of the most recently settled frame.
    previous_dropped: bool,
    /// Frames dropped before the first presented one (startup, window still hidden).
    dropped_before_first: usize,
    /// Additional egui passes inside a frame (discarded sizing passes).
    extra_passes: usize,
}

impl FrameClock {
    pub fn new(process_start: Instant) -> Self {
        Self {
            process_start,
            first_ui_start: None,
            first_frame_presented: None,
            pending: None,
            last_presented_start: None,
            drops_seen: 0,
            intervals_ms: Vec::new(),
            cpu_ms: Vec::new(),
            presented: 0,
            dropped: 0,
            previous_dropped: false,
            dropped_before_first: 0,
            extra_passes: 0,
        }
    }

    /// Whether the frame before the current one was dropped (valid after
    /// `frame_start`).
    pub fn previous_frame_was_dropped(&self) -> bool {
        self.previous_dropped
    }

    /// Call at the top of the first UI pass of every frame.
    ///
    /// `prev_cpu_s` is egui's CPU time for the previous frame; `drops_now` is the drop
    /// counters' current total. Together they settle the previous frame.
    pub fn frame_start(&mut self, now: Instant, prev_cpu_s: Option<f32>, drops_now: u64) {
        if self.first_ui_start.is_none() {
            self.first_ui_start = Some(now.duration_since(self.process_start));
        }
        let dropped = previous_frame_dropped(self.drops_seen, drops_now);
        self.drops_seen = drops_now;
        self.previous_dropped = dropped;
        if let Some(prev_start) = self.pending.take() {
            if dropped {
                self.dropped += 1;
                self.last_presented_start = None;
            } else {
                self.presented += 1;
                if self.first_frame_presented.is_none() {
                    self.first_frame_presented = Some(now.duration_since(self.process_start));
                    self.dropped_before_first = self.dropped;
                }
                if let Some(cpu) = prev_cpu_s {
                    self.cpu_ms.push(f64::from(cpu) * 1000.0);
                }
                if let Some(last) = self.last_presented_start {
                    self.intervals_ms
                        .push(prev_start.duration_since(last).as_secs_f64() * 1000.0);
                }
                self.last_presented_start = Some(prev_start);
            }
        }
        self.pending = Some(now);
    }

    /// Call for every pass after the first within one frame.
    pub fn extra_pass(&mut self) {
        self.extra_passes += 1;
    }

    pub fn extra_passes(&self) -> usize {
        self.extra_passes
    }

    pub fn process_start(&self) -> Instant {
        self.process_start
    }

    pub fn first_ui_start(&self) -> Option<Duration> {
        self.first_ui_start
    }

    pub fn first_frame_presented(&self) -> Option<Duration> {
        self.first_frame_presented
    }

    pub fn interval_summary(&self) -> Option<Summary> {
        summarize(&self.intervals_ms)
    }

    pub fn cpu_summary(&self) -> Option<Summary> {
        summarize(&self.cpu_ms)
    }

    /// Frames confirmed on screen.
    pub fn presented(&self) -> usize {
        self.presented
    }

    /// Frames whose UI ran but whose surface acquire failed.
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// Of `dropped`, how many came before the first presented frame.
    pub fn dropped_before_first(&self) -> usize {
        self.dropped_before_first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_of_known_samples() {
        let s = summarize(&[4.0, 1.0, 3.0, 2.0, 10.0]).expect("non-empty");
        assert_eq!(s.count, 5);
        assert!((s.mean_ms - 4.0).abs() < 1e-9);
        assert_eq!(s.p50_ms, 3.0);
        assert_eq!(s.p95_ms, 10.0);
        assert_eq!(s.max_ms, 10.0);
    }

    #[test]
    fn empty_is_none() {
        assert!(summarize(&[]).is_none());
    }

    #[test]
    fn drop_flag_is_a_counter_increase() {
        assert!(!previous_frame_dropped(0, 0));
        assert!(previous_frame_dropped(3, 4));
        assert!(!previous_frame_dropped(4, 4));
    }

    fn ms(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_millis(n)
    }

    #[test]
    fn presented_frames_only_count() {
        let t0 = Instant::now();
        let mut c = FrameClock::new(t0);
        // Frame A at 100: its acquire is Occluded (counter goes 0 -> 1).
        c.frame_start(ms(t0, 100), None, 0);
        // Frame B at 101 settles A as dropped. B presents.
        c.frame_start(ms(t0, 101), Some(0.009), 1);
        assert_eq!(
            c.first_frame_presented(),
            None,
            "a dropped frame is not first"
        );
        assert!(c.previous_frame_was_dropped());
        // Frame C at 117 settles B as presented.
        c.frame_start(ms(t0, 117), Some(0.002), 1);
        assert_eq!(c.first_frame_presented(), Some(Duration::from_millis(117)));
        assert!(!c.previous_frame_was_dropped());
        // Frame D at 133 settles C as presented: interval B->C = 16 ms.
        c.frame_start(ms(t0, 133), Some(0.003), 1);

        assert_eq!(c.first_ui_start(), Some(Duration::from_millis(100)));
        assert_eq!(c.dropped(), 1);
        assert_eq!(c.dropped_before_first(), 1);
        assert_eq!(c.presented(), 2);
        let i = c.interval_summary().expect("one interval");
        assert_eq!(i.count, 1);
        assert!((i.max_ms - 16.0).abs() < 1e-6);
        // The dropped frame's CPU sample (9 ms) is excluded.
        let cpu = c.cpu_summary().expect("two samples");
        assert_eq!(cpu.count, 2);
        assert!(cpu.max_ms < 5.0);
    }

    #[test]
    fn interval_never_spans_a_dropped_frame() {
        let t0 = Instant::now();
        let mut c = FrameClock::new(t0);
        c.frame_start(ms(t0, 0), None, 0); // A pending
        c.frame_start(ms(t0, 16), None, 0); // A presented; B pending, then drops
        c.frame_start(ms(t0, 17), None, 1); // B dropped; C pending
        c.frame_start(ms(t0, 33), None, 1); // C presented
        assert_eq!(c.presented(), 2);
        assert_eq!(c.dropped(), 1);
        assert!(c.interval_summary().is_none(), "A->C spans the drop");
    }
}
