//! The measured compaction recommendation (decision 0057, addendum 1).
//!
//! The slider's recommendation is "the point measured from that model's own runs where
//! task outcome holds". The runs are the trace files the orchestrator writes: each
//! finished worker records its peak context, its route's effective window and its
//! outcome. This module reads those records and turns them into a
//! [`Recommendation`]; [`crate::settings::CompactionSlider::update_recommendation`]
//! takes it from there, so a slider the user has not moved follows it.
//!
//! The rule, stated so the number can be checked by hand:
//!
//! - A sample is a finished worker whose outcome is `completed` (held) or `failed`,
//!   `stalled` or `budget_exhausted` (did not hold), with a known window and a peak
//!   above zero. A hand-back and a cancel say nothing about whether the outcome held
//!   and are left out, and so is a worker the runtime compacted (decision 0095): it
//!   finished on a summary, not on the context its peak describes.
//! - The model with the most samples is measured, and needs [`MIN_SAMPLES`] of them.
//! - The recommendation is the highest point `p` (a multiple of 5 in the slider's
//!   range) such that the workers whose peak stayed at or under `p` percent of the
//!   window held at [`HOLD_PERCENT`] or better, over at least [`MIN_SAMPLES`] / 2
//!   workers. If no `p` qualifies the recommendation stays unmeasured.
//!
//! The read runs off the UI thread ([`Measurement`]), at most once per
//! [`MEASURE_INTERVAL`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kgraf_orchestrator::{TraceEvent, TraceLog};

use crate::settings::{
    COMPACTION_MAX_PERCENT, COMPACTION_MIN_PERCENT, CompactionPoint, Recommendation,
};

/// Fewest samples for a model to be measured at all.
pub const MIN_SAMPLES: usize = 20;
/// Share of workers, in percent, that must have held for a point to qualify.
pub const HOLD_PERCENT: usize = 90;
/// How often the trace files are read again.
pub const MEASURE_INTERVAL: Duration = Duration::from_secs(60);

/// One finished worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSample {
    pub model: String,
    pub peak_tokens: u64,
    pub window_tokens: u64,
    pub held: bool,
}

impl WorkerSample {
    /// Peak context as a percent of the window, rounded up so a worker never counts
    /// as staying under a point it went over.
    fn fill_percent(&self) -> u64 {
        (self.peak_tokens.saturating_mul(100)).div_ceil(self.window_tokens.max(1))
    }
}

/// The samples in one trace: one per finished worker that says whether its outcome
/// held.
pub fn samples_from_trace(events: &[TraceEvent]) -> Vec<WorkerSample> {
    // A worker the runtime compacted (decision 0095) finished on a summary of
    // its context, not on the context its peak describes.
    let compacted: std::collections::HashSet<&str> = events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::WorkerCompacted { worker, .. } => Some(worker.as_str()),
            _ => None,
        })
        .collect();
    events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::WorkerFinished {
                worker,
                model,
                outcome,
                peak_context_tokens,
                window_tokens,
                ..
            } if *window_tokens > 0
                && *peak_context_tokens > 0
                && !compacted.contains(worker.as_str()) =>
            {
                let held = match outcome.as_str() {
                    "completed" => true,
                    "failed" | "stalled" | "budget_exhausted" => false,
                    _ => return None,
                };
                Some(WorkerSample {
                    model: model.clone(),
                    peak_tokens: *peak_context_tokens,
                    window_tokens: *window_tokens,
                    held,
                })
            }
            _ => None,
        })
        .collect()
}

/// The recommendation the samples support; unmeasured when they do not support one.
pub fn measure(samples: &[WorkerSample]) -> Recommendation {
    let mut per_model: HashMap<&str, Vec<&WorkerSample>> = HashMap::new();
    for s in samples {
        per_model.entry(s.model.as_str()).or_default().push(s);
    }
    // Ties go to the model name, so the answer does not depend on hash order.
    let Some((_, chosen)) = per_model
        .into_iter()
        .max_by(|(an, a), (bn, b)| a.len().cmp(&b.len()).then_with(|| bn.cmp(an)))
    else {
        return Recommendation::default();
    };
    if chosen.len() < MIN_SAMPLES {
        return Recommendation::default();
    }
    let mut fills: Vec<(u64, bool)> = chosen.iter().map(|s| (s.fill_percent(), s.held)).collect();
    fills.sort_unstable();
    let step = 5u8;
    let mut p = COMPACTION_MAX_PERCENT;
    loop {
        let under: Vec<&(u64, bool)> = fills
            .iter()
            .take_while(|(fill, _)| *fill <= u64::from(p))
            .collect();
        let held = under.iter().filter(|(_, h)| *h).count();
        if under.len() >= MIN_SAMPLES / 2
            && held * 100 >= under.len() * HOLD_PERCENT
            && let Ok(point) = CompactionPoint::new(i64::from(p))
        {
            return Recommendation::Measured(point);
        }
        if p < COMPACTION_MIN_PERCENT + step {
            return Recommendation::default();
        }
        p -= step;
    }
}

/// The trace files of every run stored under a project root.
fn trace_paths(root: &Path) -> Vec<PathBuf> {
    let Some(state) = kgraf_graph::snapshot::graph_db_path(root)
        .parent()
        .map(Path::to_path_buf)
    else {
        return Vec::new();
    };
    let Ok(runs) = std::fs::read_dir(state.join("runs")) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = runs
        .filter_map(Result::ok)
        .map(|e| e.path().join("trace.jsonl"))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    paths
}

/// The recommendation from every stored run of these projects. A trace that cannot
/// be read is logged and left out; the others still count.
pub fn measure_projects(roots: &[PathBuf]) -> Recommendation {
    let rt = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(rt) => rt,
        Err(e) => {
            log::warn!("cannot read run traces for the compaction recommendation: {e}");
            return Recommendation::default();
        }
    };
    let mut samples = Vec::new();
    for path in roots.iter().flat_map(|r| trace_paths(r)) {
        let events = rt.block_on(async {
            TraceLog::open(&path)
                .await
                .map_err(|e| e.to_string())?
                .load()
                .await
                .map_err(|e| e.to_string())
        });
        match events {
            Ok(events) => samples.extend(samples_from_trace(&events)),
            Err(e) => log::warn!("run trace {} not read: {e}", path.display()),
        }
    }
    measure(&samples)
}

/// A background read, started at most once per [`MEASURE_INTERVAL`] and polled from
/// the UI's logic pass.
#[derive(Default)]
pub struct Measurement {
    started: Option<Instant>,
    running: Option<JoinHandle<Recommendation>>,
}

impl Measurement {
    /// A finished read's recommendation, if one just finished; starts the next read
    /// when the interval has passed. `roots` is read only when a read starts.
    pub fn poll(
        &mut self,
        now: Instant,
        roots: impl FnOnce() -> Vec<PathBuf>,
    ) -> Option<Recommendation> {
        let mut done = None;
        if self.running.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(handle) = self.running.take()
        {
            done = handle.join().ok();
        }
        let due = self
            .started
            .is_none_or(|t| now.duration_since(t) >= MEASURE_INTERVAL);
        if self.running.is_none() && due {
            let roots = roots();
            self.started = Some(now);
            match std::thread::Builder::new()
                .name("compaction-measure".into())
                .spawn(move || measure_projects(&roots))
            {
                Ok(handle) => self.running = Some(handle),
                Err(e) => log::warn!("cannot start the compaction measurement: {e}"),
            }
        }
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(model: &str, fill: u64, held: bool) -> WorkerSample {
        WorkerSample {
            model: model.into(),
            peak_tokens: fill * 1_000,
            window_tokens: 100_000,
            held,
        }
    }

    /// Workers held up to 39% of the window and failed above 60%: the point lands
    /// where the held share drops under the bar, not at the default.
    #[test]
    fn the_point_is_the_highest_fill_up_to_which_outcomes_held() {
        let mut samples: Vec<WorkerSample> = (0..30).map(|i| sample("m", 10 + i, true)).collect();
        samples.extend((0..10).map(|i| sample("m", 61 + i * 3, false)));
        assert_eq!(
            measure(&samples),
            Recommendation::Measured(CompactionPoint::new(65).expect("valid")),
            "30 held up to 39%; two failures (61, 64) fit under 65 at 94% held, a third \
             (67) is above it, and 70 would hold only 30 of 34"
        );
    }

    /// The whole path from trace files on disk: a project with stored runs yields a
    /// measured recommendation, and a project with none does not.
    #[test]
    fn stored_run_traces_yield_a_measured_recommendation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runs = kgraf_graph::snapshot::graph_db_path(dir.path())
            .parent()
            .expect("state dir")
            .join("runs")
            .join("r1");
        std::fs::create_dir_all(&runs).expect("mkdir");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let mut log = TraceLog::open(&runs.join("trace.jsonl"))
                .await
                .expect("open");
            for i in 0..30u64 {
                log.record(TraceEvent::WorkerFinished {
                    last_stop: None,
                    tool_calls: 0,
                    at_ms: 0,
                    contract: format!("c{i}"),
                    worker: format!("w{i}"),
                    route: "r".into(),
                    model: "m".into(),
                    outcome: "completed".into(),
                    steps: 1,
                    peak_context_tokens: (10 + i) * 1_000,
                    projected_tokens: 0,
                    stop_tokens: 0,
                    window_tokens: 100_000,
                    sizing_miss: false,
                    calls: Vec::new(),
                    spend_micros: 0,
                    error: None,
                })
                .await
                .expect("record");
            }
        });
        // Every worker held, so the highest point the slider allows qualifies.
        assert_eq!(
            measure_projects(&[dir.path().to_path_buf()]),
            Recommendation::Measured(CompactionPoint::new(95).expect("valid"))
        );
        let empty = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            measure_projects(&[empty.path().to_path_buf()]),
            Recommendation::Unmeasured(_)
        ));
    }

    /// The poll starts a read, hands back its result, and the slider follows it
    /// unless the user pinned a value.
    #[test]
    fn a_polled_measurement_moves_a_following_slider_and_not_a_pinned_one() {
        use crate::settings::CompactionSlider;
        let dir = tempfile::tempdir().expect("tempdir");
        let runs = kgraf_graph::snapshot::graph_db_path(dir.path())
            .parent()
            .expect("state dir")
            .join("runs")
            .join("r1");
        std::fs::create_dir_all(&runs).expect("mkdir");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let mut log = TraceLog::open(&runs.join("trace.jsonl"))
                .await
                .expect("open");
            for i in 0..30u64 {
                let held = i < 25;
                log.record(TraceEvent::WorkerFinished {
                    last_stop: None,
                    tool_calls: 0,
                    at_ms: 0,
                    contract: format!("c{i}"),
                    worker: format!("w{i}"),
                    route: "r".into(),
                    model: "m".into(),
                    outcome: if held { "completed" } else { "failed" }.into(),
                    steps: 1,
                    peak_context_tokens: (10 + 2 * i) * 1_000,
                    projected_tokens: 0,
                    stop_tokens: 0,
                    window_tokens: 100_000,
                    sizing_miss: false,
                    calls: Vec::new(),
                    spend_micros: 0,
                    error: None,
                })
                .await
                .expect("record");
            }
        });
        let roots = vec![dir.path().to_path_buf()];
        let mut following = CompactionSlider::default();
        let mut pinned = CompactionSlider::default();
        pinned.set_by_user(CompactionPoint::new(33).expect("valid"));

        let mut measurement = Measurement::default();
        let start = Instant::now();
        let mut got = measurement.poll(start, || roots.clone());
        for _ in 0..500 {
            if got.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            got = measurement.poll(start, || roots.clone());
        }
        let rec = got.expect("the read finished");
        assert!(matches!(rec, Recommendation::Measured(_)), "{rec:?}");
        following.update_recommendation(rec);
        pinned.update_recommendation(rec);
        assert_eq!(following.value(), rec.point());
        assert_ne!(rec.point().percent(), 80, "not the unmeasured default");
        assert_eq!(pinned.value().percent(), 33, "a pinned value stays put");
    }

    #[test]
    fn a_model_with_too_few_runs_stays_unmeasured() {
        let samples: Vec<WorkerSample> = (0..MIN_SAMPLES as u64 - 1)
            .map(|i| sample("m", 10 + i, true))
            .collect();
        assert!(matches!(measure(&samples), Recommendation::Unmeasured(_)));
    }

    #[test]
    fn a_hand_back_and_a_cancel_are_not_samples() {
        let finished = |outcome: &str| TraceEvent::WorkerFinished {
            last_stop: None,
            tool_calls: 0,
            at_ms: 0,
            contract: "c".into(),
            worker: "w".into(),
            route: "r".into(),
            model: "m".into(),
            outcome: outcome.into(),
            steps: 1,
            peak_context_tokens: 5_000,
            projected_tokens: 0,
            stop_tokens: 0,
            window_tokens: 100_000,
            sizing_miss: false,
            calls: Vec::new(),
            spend_micros: 0,
            error: None,
        };
        let events: Vec<TraceEvent> = ["completed", "handed_back", "cancelled", "failed"]
            .into_iter()
            .map(finished)
            .collect();
        let samples = samples_from_trace(&events);
        assert_eq!(
            samples.iter().map(|s| s.held).collect::<Vec<_>>(),
            [true, false]
        );
    }

    #[test]
    fn a_worker_the_runtime_compacted_is_not_a_sample() {
        let finished = |worker: &str| TraceEvent::WorkerFinished {
            last_stop: None,
            tool_calls: 0,
            at_ms: 0,
            contract: "c".into(),
            worker: worker.into(),
            route: "r".into(),
            model: "m".into(),
            outcome: "completed".into(),
            steps: 1,
            peak_context_tokens: 80_000,
            projected_tokens: 0,
            stop_tokens: 80_000,
            window_tokens: 100_000,
            sizing_miss: worker == "compacted",
            calls: Vec::new(),
            spend_micros: 0,
            error: None,
        };
        let notice = kgraf_runtime::CompactionNotice {
            worker: "compacted".into(),
            task: "t".into(),
            trigger: kgraf_runtime::CompactionTrigger::StopPoint,
            at_ms: 0,
            at_tokens: 80_000,
            stop_tokens: 80_000,
            window_tokens: 100_000,
            summary_tokens: 300,
            summary_call: None,
            summary_source: kgraf_runtime::SummarySource::Model,
            transcript_path: None,
            archived_entries: 10,
            recall: true,
            items_done: 3,
            items_remaining: 1,
            number: 1,
            may_compact_again: false,
        };
        let events = vec![
            TraceEvent::WorkerCompacted {
                at_ms: 0,
                contract: "c".into(),
                worker: "compacted".into(),
                notice,
            },
            finished("compacted"),
            finished("plain"),
        ];
        let samples = samples_from_trace(&events);
        assert_eq!(samples.len(), 1, "only the plain worker: {samples:?}");
    }
}
