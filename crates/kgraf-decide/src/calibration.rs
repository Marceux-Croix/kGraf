//! Measuring whether a confidence number deserves to be trusted.
//!
//! Every implementation in this space claims calibrated probabilities, and for
//! kGraf calibration matters more than accuracy. An earlier collision-prediction experiment is the reason:
//! a frontier model predicted same-region collision at 0.613 on a balanced set against
//! a 0.500 baseline, and the conclusion was that the handshake "earns nothing as a
//! gate". A weak signal is not useless, a weak signal with **untrustworthy
//! confidence** is. If a model says 0.9 and is right nine times in ten, a caller can
//! act on the confident tail and ignore the rest, and a 0.613 average becomes a usable
//! mechanism. If it says 0.9 and is right six times in ten, the confidence is
//! decoration.
//!
//! So this module scores the claim. Expected calibration error is the headline, the
//! Brier score keeps it honest about accuracy at the same time, and the reliability
//! bins are reported because a single number hides which end of the range is broken,
//! and for kGraf's purposes only the confident end matters.

/// One bin of a reliability diagram.
#[derive(Clone, Debug, PartialEq)]
pub struct ReliabilityBin {
    /// Lower edge, inclusive.
    pub low: f64,
    /// Upper edge, exclusive except for the last bin.
    pub high: f64,
    /// Predictions that landed in this bin.
    pub count: usize,
    /// Mean predicted probability in this bin.
    pub mean_predicted: f64,
    /// Share of this bin that was actually true.
    pub observed_rate: f64,
}

impl ReliabilityBin {
    /// Signed gap: positive means overconfident, negative means underconfident.
    pub fn gap(&self) -> f64 {
        self.mean_predicted - self.observed_rate
    }
}

/// A calibration report over a set of predictions.
#[derive(Clone, Debug, PartialEq)]
pub struct Calibration {
    /// Predictions scored.
    pub n: usize,
    /// Predictions left out because the probability was not a finite number. They
    /// are counted here, not in `n`, so a NaN never reads as a wrong answer or
    /// turns every aggregate into NaN.
    pub non_finite: usize,
    /// Accuracy at the given decision threshold.
    pub accuracy: f64,
    /// Expected calibration error: the count-weighted mean absolute gap across bins.
    /// Zero is perfect. Below about 0.05 is usually called well calibrated.
    pub ece: f64,
    /// The largest single-bin gap, which a count-weighted mean can hide.
    pub max_bin_gap: f64,
    /// Brier score: mean squared error of the probabilities. Lower is better, and it
    /// penalizes both miscalibration and inaccuracy, so a model cannot game it by
    /// hedging every answer at 0.5.
    pub brier: f64,
    /// Accuracy restricted to predictions the model was confident about, with the count
    /// they represent. `None` when no prediction cleared the threshold.
    ///
    /// **This is the figure that decides whether the mechanism is usable**, because the
    /// intended use is to act on the confident tail and defer the rest.
    pub confident_tail: Option<(usize, f64)>,
    pub bins: Vec<ReliabilityBin>,
}

/// Score predicted probabilities against outcomes.
///
/// `predictions` pairs a probability of the positive class with what actually happened.
/// `decision_threshold` is where a probability becomes a yes; `confident_at` is the
/// probability distance from 0.5 at which a prediction counts as confident, so both
/// tails count.
pub fn score(
    predictions: &[(f64, bool)],
    decision_threshold: f64,
    confident_at: f64,
    bin_count: usize,
) -> Calibration {
    let all = predictions;
    let finite: Vec<(f64, bool)> = all.iter().copied().filter(|(p, _)| p.is_finite()).collect();
    let non_finite = all.len() - finite.len();
    let predictions = finite.as_slice();
    let n = predictions.len();
    if n == 0 {
        return Calibration {
            n: 0,
            non_finite,
            accuracy: 0.0,
            ece: 0.0,
            max_bin_gap: 0.0,
            brier: 0.0,
            confident_tail: None,
            bins: Vec::new(),
        };
    }
    let bin_count = bin_count.max(1);

    let correct = predictions
        .iter()
        .filter(|(p, actual)| (*p >= decision_threshold) == *actual)
        .count();
    let brier = predictions
        .iter()
        .map(|(p, actual)| {
            let target = if *actual { 1.0 } else { 0.0 };
            (p - target).powi(2)
        })
        .sum::<f64>()
        / n as f64;

    let mut bins: Vec<ReliabilityBin> = Vec::with_capacity(bin_count);
    for index in 0..bin_count {
        let low = index as f64 / bin_count as f64;
        let high = (index + 1) as f64 / bin_count as f64;
        let members: Vec<&(f64, bool)> = predictions
            .iter()
            .filter(|(p, _)| {
                // The last bin takes its upper edge, so a probability of exactly 1.0 is
                // counted rather than dropped.
                if index + 1 == bin_count {
                    *p >= low && *p <= high
                } else {
                    *p >= low && *p < high
                }
            })
            .collect();
        let count = members.len();
        let (mean_predicted, observed_rate) = if count == 0 {
            (0.0, 0.0)
        } else {
            (
                members.iter().map(|(p, _)| *p).sum::<f64>() / count as f64,
                members.iter().filter(|(_, actual)| *actual).count() as f64 / count as f64,
            )
        };
        bins.push(ReliabilityBin {
            low,
            high,
            count,
            mean_predicted,
            observed_rate,
        });
    }

    let ece = bins
        .iter()
        .filter(|bin| bin.count > 0)
        .map(|bin| (bin.count as f64 / n as f64) * bin.gap().abs())
        .sum::<f64>();
    let max_bin_gap = bins
        .iter()
        .filter(|bin| bin.count > 0)
        .map(|bin| bin.gap().abs())
        .fold(0.0_f64, f64::max);

    let confident: Vec<&(f64, bool)> = predictions
        .iter()
        .filter(|(p, _)| (p - 0.5).abs() >= confident_at)
        .collect();
    let confident_tail = if confident.is_empty() {
        None
    } else {
        let hits = confident
            .iter()
            .filter(|(p, actual)| (*p >= decision_threshold) == *actual)
            .count();
        Some((confident.len(), hits as f64 / confident.len() as f64))
    };

    Calibration {
        n,
        non_finite,
        accuracy: correct as f64 / n as f64,
        ece,
        max_bin_gap,
        brier,
        confident_tail,
        bins,
    }
}

impl Calibration {
    /// A short report, for a probe's stdout and for a written record.
    pub fn report(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "n {}  accuracy {:.3}  ECE {:.3}  max bin gap {:.3}  Brier {:.3}\n",
            self.n, self.accuracy, self.ece, self.max_bin_gap, self.brier
        ));
        if self.non_finite > 0 {
            out.push_str(&format!(
                "{} prediction(s) with a non-finite probability were left out\n",
                self.non_finite
            ));
        }
        match self.confident_tail {
            Some((count, accuracy)) => out.push_str(&format!(
                "confident tail: {count} of {} predictions at {:.3} accuracy\n",
                self.n, accuracy
            )),
            None => out.push_str("confident tail: empty; nothing cleared the threshold\n"),
        }
        for bin in self.bins.iter().filter(|b| b.count > 0) {
            out.push_str(&format!(
                "  [{:.2},{:.2})  n {:>4}  predicted {:.3}  observed {:.3}  gap {:+.3}\n",
                bin.low,
                bin.high,
                bin.count,
                bin.mean_predicted,
                bin.observed_rate,
                bin.gap()
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_finite_probability_is_counted_apart_and_poisons_nothing() {
        let scored = score(&[(0.9, true), (f64::NAN, true), (0.1, false)], 0.5, 0.3, 10);
        assert_eq!(scored.n, 2);
        assert_eq!(scored.non_finite, 1);
        assert!(scored.brier.is_finite() && scored.ece.is_finite());
        assert!((scored.accuracy - 1.0).abs() < 1e-12);
        assert!(scored.report().contains("non-finite"));
        let none = score(&[(f64::NAN, true)], 0.5, 0.3, 10);
        assert_eq!((none.n, none.non_finite), (0, 1));
    }

    #[test]
    fn a_perfectly_calibrated_set_scores_near_zero_error() {
        // Ten predictions at 0.9, nine of which are true, and so on down. A perfectly
        // calibrated model's ECE is zero by construction.
        let mut predictions = Vec::new();
        for (p, trues, total) in [(0.9, 9, 10), (0.5, 5, 10), (0.1, 1, 10)] {
            for i in 0..total {
                predictions.push((p, i < trues));
            }
        }
        let scored = score(&predictions, 0.5, 0.25, 10);
        assert!(scored.ece < 0.01, "ECE was {}", scored.ece);
    }

    #[test]
    fn an_overconfident_model_is_caught() {
        // Claims 0.99 every time, right only half the time. Accuracy looks like a coin
        // flip and the calibration error is enormous, which is exactly the case that
        // makes a confidence number useless for routing.
        let predictions: Vec<(f64, bool)> = (0..100).map(|i| (0.99, i % 2 == 0)).collect();
        let scored = score(&predictions, 0.5, 0.25, 10);
        assert!(scored.ece > 0.4, "ECE was {}", scored.ece);
        assert!((scored.accuracy - 0.5).abs() < 0.01);
    }

    #[test]
    fn hedging_every_answer_at_one_half_does_not_win_on_brier() {
        // A model that always says 0.5 is perfectly calibrated on a balanced set and
        // completely useless. ECE alone would praise it; Brier is why it is reported.
        let hedged: Vec<(f64, bool)> = (0..100).map(|i| (0.5, i % 2 == 0)).collect();
        let decisive: Vec<(f64, bool)> = (0..100)
            .map(|i| (if i % 2 == 0 { 0.95 } else { 0.05 }, i % 2 == 0))
            .collect();
        let hedged = score(&hedged, 0.5, 0.25, 10);
        let decisive = score(&decisive, 0.5, 0.25, 10);
        assert!(
            hedged.ece < 0.01,
            "the hedger is calibrated: {}",
            hedged.ece
        );
        assert!(
            decisive.brier < hedged.brier,
            "Brier must prefer the decisive model: {} vs {}",
            decisive.brier,
            hedged.brier
        );
    }

    #[test]
    fn the_confident_tail_is_reported_separately_from_the_average() {
        // The whole reason to care about calibration: a mediocre average can contain a
        // trustworthy confident subset, and that subset is what a gate would use.
        let mut predictions = Vec::new();
        // 20 confident and nearly always right.
        for i in 0..20 {
            predictions.push((0.95, i < 19));
        }
        // 80 uncertain and barely better than chance.
        for i in 0..80 {
            predictions.push((0.52, i % 2 == 0));
        }
        let scored = score(&predictions, 0.5, 0.25, 10);
        let (count, accuracy) = scored.confident_tail.expect("a tail exists");
        assert_eq!(count, 20);
        assert!(accuracy > 0.9, "tail accuracy was {accuracy}");
        assert!(
            scored.accuracy < accuracy,
            "the average must be worse than the tail, or the example is wrong"
        );
    }

    #[test]
    fn an_empty_tail_is_reported_as_empty_rather_than_as_zero_accuracy() {
        // Nothing cleared the threshold is a different fact from everything was wrong.
        let predictions: Vec<(f64, bool)> = (0..10).map(|i| (0.51, i % 2 == 0)).collect();
        let scored = score(&predictions, 0.5, 0.3, 10);
        assert_eq!(scored.confident_tail, None);
    }

    #[test]
    fn a_probability_of_exactly_one_is_counted() {
        // Off-by-one at the top bin edge would silently drop the most confident
        // predictions, which are the ones that matter most here.
        let scored = score(&[(1.0, true), (1.0, true)], 0.5, 0.25, 10);
        assert_eq!(scored.n, 2);
        assert_eq!(scored.bins.iter().map(|b| b.count).sum::<usize>(), 2);
    }

    #[test]
    fn an_empty_set_does_not_panic() {
        let scored = score(&[], 0.5, 0.25, 10);
        assert_eq!(scored.n, 0);
        assert!(scored.bins.is_empty());
    }

    #[test]
    fn max_bin_gap_surfaces_a_break_the_mean_would_hide() {
        // One tiny badly-broken bin barely moves a count-weighted mean.
        let mut predictions: Vec<(f64, bool)> = Vec::new();
        for i in 0..990 {
            predictions.push((0.5, i % 2 == 0));
        }
        for _ in 0..10 {
            predictions.push((0.95, false));
        }
        let scored = score(&predictions, 0.5, 0.25, 10);
        assert!(scored.ece < 0.02, "mean hides it: {}", scored.ece);
        assert!(
            scored.max_bin_gap > 0.9,
            "max gap must surface it: {}",
            scored.max_bin_gap
        );
    }
}
