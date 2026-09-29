//! Context thresholds (decision 0057, addendum 2).
//!
//! | Value | Rule |
//! |---|---|
//! | Effective window | min(model window, ceiling); ceiling defaults to 100,000 tokens |
//! | Compaction point | a percent of the effective window; default 80, bounded 5 to 95 |
//! | Compact down to | half the compaction point |
//! | Worker plan target | half the compaction point |
//! | Worker stop point | the compaction point; the worker hands its remainder back |
//!
//! Out-of-range values are errors, never clamped (fail loud), and that holds for a value
//! read from a configuration file too: [`ContextConfig`] deserializes through
//! validation, so an invalid file does not produce a value.
//!
//! The default point is labeled [`PointSource::UnmeasuredDefault`] until a model has
//! recorded runs (0057 addendum): the label travels with the value so the settings
//! surface can say so. That label is only ever attached to the default value; a
//! different value labeled unmeasured-default would misreport a user's or a
//! measurement's choice as the production default, so it is an error.
//!
//! **The pinned floor must fit under the post-compaction target.** The system
//! prompt, tool definitions, shared slices and original instruction are never
//! compacted. If they alone reach the compact-down-to size, compaction cannot free
//! any room (0057 addendum 2: "that is arithmetic, not a tuned value"). A supplied
//! floor is checked on every change, so moving the point down cannot silently
//! invalidate it.

use serde::{Deserialize, Serialize};

/// The default absolute ceiling on the effective window, in tokens.
pub const DEFAULT_CEILING_TOKENS: u64 = 100_000;
/// The default compaction point, in percent of the effective window.
pub const DEFAULT_COMPACTION_POINT_PERCENT: u8 = 80;
/// The lowest compaction point accepted.
pub const MIN_COMPACTION_POINT_PERCENT: u8 = 5;
/// The highest compaction point accepted.
pub const MAX_COMPACTION_POINT_PERCENT: u8 = 95;

/// Where the compaction point came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointSource {
    /// The production default (80%), not yet measured for this model.
    UnmeasuredDefault,
    /// Measured from this model's recorded runs.
    MeasuredRecommendation,
    /// Set by the user.
    User,
}

/// A threshold configuration that cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ThresholdError {
    #[error("the compaction point {percent}% is outside 5% to 95%")]
    PointOutOfRange { percent: u8 },
    #[error("the model window is zero")]
    ZeroWindow,
    #[error("the context ceiling is zero")]
    ZeroCeiling,
    #[error("{percent}% is not the default, so it cannot be labeled the unmeasured default")]
    UnmeasuredLabelOnNonDefault { percent: u8 },
    #[error(
        "the pinned floor of {floor} tokens does not fit under the post-compaction target of {target}"
    )]
    PinnedFloorTooLarge { floor: u64, target: u64 },
}

/// The configured values, as written in a settings file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawContextConfig {
    model_window_tokens: u64,
    #[serde(default = "default_ceiling")]
    ceiling_tokens: u64,
    #[serde(default = "default_point")]
    compaction_point_percent: u8,
    #[serde(default = "default_source")]
    point_source: PointSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pinned_floor_tokens: Option<u64>,
}

fn default_ceiling() -> u64 {
    DEFAULT_CEILING_TOKENS
}
fn default_point() -> u8 {
    DEFAULT_COMPACTION_POINT_PERCENT
}
fn default_source() -> PointSource {
    PointSource::UnmeasuredDefault
}

/// Validated context thresholds for one model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawContextConfig", into = "RawContextConfig")]
pub struct ContextConfig {
    model_window_tokens: u64,
    ceiling_tokens: u64,
    compaction_point_percent: u8,
    point_source: PointSource,
    pinned_floor_tokens: Option<u64>,
}

impl TryFrom<RawContextConfig> for ContextConfig {
    type Error = ThresholdError;

    fn try_from(raw: RawContextConfig) -> Result<Self, Self::Error> {
        let config = Self {
            model_window_tokens: raw.model_window_tokens,
            ceiling_tokens: raw.ceiling_tokens,
            compaction_point_percent: raw.compaction_point_percent,
            point_source: raw.point_source,
            pinned_floor_tokens: raw.pinned_floor_tokens,
        };
        config.validate()?;
        Ok(config)
    }
}

impl From<ContextConfig> for RawContextConfig {
    fn from(c: ContextConfig) -> Self {
        Self {
            model_window_tokens: c.model_window_tokens,
            ceiling_tokens: c.ceiling_tokens,
            compaction_point_percent: c.compaction_point_percent,
            point_source: c.point_source,
            pinned_floor_tokens: c.pinned_floor_tokens,
        }
    }
}

impl ContextConfig {
    /// The defaults for a model with this window: 100,000-token ceiling, 80% point,
    /// labeled unmeasured.
    pub fn for_model(model_window_tokens: u64) -> Result<Self, ThresholdError> {
        RawContextConfig {
            model_window_tokens,
            ceiling_tokens: DEFAULT_CEILING_TOKENS,
            compaction_point_percent: DEFAULT_COMPACTION_POINT_PERCENT,
            point_source: PointSource::UnmeasuredDefault,
            pinned_floor_tokens: None,
        }
        .try_into()
    }

    fn validate(&self) -> Result<(), ThresholdError> {
        if self.model_window_tokens == 0 {
            return Err(ThresholdError::ZeroWindow);
        }
        if self.ceiling_tokens == 0 {
            return Err(ThresholdError::ZeroCeiling);
        }
        let percent = self.compaction_point_percent;
        if !(MIN_COMPACTION_POINT_PERCENT..=MAX_COMPACTION_POINT_PERCENT).contains(&percent) {
            return Err(ThresholdError::PointOutOfRange { percent });
        }
        if self.point_source == PointSource::UnmeasuredDefault
            && percent != DEFAULT_COMPACTION_POINT_PERCENT
        {
            return Err(ThresholdError::UnmeasuredLabelOnNonDefault { percent });
        }
        if let Some(floor) = self.pinned_floor_tokens {
            let target = self.compact_down_to_tokens();
            if floor >= target {
                return Err(ThresholdError::PinnedFloorTooLarge { floor, target });
            }
        }
        Ok(())
    }

    fn checked(self) -> Result<Self, ThresholdError> {
        self.validate()?;
        Ok(self)
    }

    /// Set the per-model ceiling (from measurement).
    pub fn with_ceiling(mut self, ceiling_tokens: u64) -> Result<Self, ThresholdError> {
        self.ceiling_tokens = ceiling_tokens;
        self.checked()
    }

    /// Set the compaction point and where it came from. Out of range is an error,
    /// and only the default value may carry [`PointSource::UnmeasuredDefault`].
    pub fn with_point(mut self, percent: u8, source: PointSource) -> Result<Self, ThresholdError> {
        self.compaction_point_percent = percent;
        self.point_source = source;
        self.checked()
    }

    /// Declare the size of the pinned floor (system prompt, tool definitions,
    /// shared slices, original instruction). It must fit under the post-compaction
    /// target.
    pub fn with_pinned_floor(mut self, tokens: u64) -> Result<Self, ThresholdError> {
        self.pinned_floor_tokens = Some(tokens);
        self.checked()
    }

    /// The compaction point, in percent.
    pub fn compaction_point_percent(&self) -> u8 {
        self.compaction_point_percent
    }

    /// Where the compaction point came from.
    pub fn point_source(&self) -> PointSource {
        self.point_source
    }

    /// The declared pinned floor, if any.
    pub fn pinned_floor_tokens(&self) -> Option<u64> {
        self.pinned_floor_tokens
    }

    /// The lesser of the model's window and the ceiling.
    pub fn effective_window(&self) -> u64 {
        self.model_window_tokens.min(self.ceiling_tokens)
    }

    /// Tokens at which the orchestrator compacts and a worker stops.
    pub fn compaction_point_tokens(&self) -> u64 {
        percent_of(
            self.effective_window(),
            u64::from(self.compaction_point_percent),
        )
    }

    /// What orchestrator compaction reduces to: half the point.
    pub fn compact_down_to_tokens(&self) -> u64 {
        self.compaction_point_tokens() / 2
    }

    /// What a worker's work block is planned to fit under: half the point.
    pub fn worker_plan_target_tokens(&self) -> u64 {
        self.compaction_point_tokens() / 2
    }

    /// Where a worker stops and hands its remainder back: the point itself.
    pub fn worker_stop_tokens(&self) -> u64 {
        self.compaction_point_tokens()
    }

    /// Whether a context of `tokens` has reached the compaction point.
    pub fn reached_point(&self, tokens: u64) -> bool {
        tokens >= self.compaction_point_tokens()
    }
}

/// `value * percent / 100` without overflow for any u64 window.
fn percent_of(value: u64, percent: u64) -> u64 {
    ((u128::from(value) * u128::from(percent)) / 100) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_addendum_2() {
        let c = ContextConfig::for_model(200_000).expect("valid");
        assert_eq!(c.effective_window(), 100_000);
        assert_eq!(c.compaction_point_tokens(), 80_000);
        assert_eq!(c.compact_down_to_tokens(), 40_000);
        assert_eq!(c.worker_plan_target_tokens(), 40_000);
        assert_eq!(c.worker_stop_tokens(), 80_000);
        assert_eq!(c.point_source(), PointSource::UnmeasuredDefault);
    }

    #[test]
    fn a_small_window_is_the_effective_window() {
        let c = ContextConfig::for_model(32_000).expect("valid");
        assert_eq!(c.effective_window(), 32_000);
        assert_eq!(c.compaction_point_tokens(), 25_600);
    }

    #[test]
    fn a_measured_ceiling_replaces_the_default() {
        let c = ContextConfig::for_model(1_000_000)
            .and_then(|c| c.with_ceiling(400_000))
            .expect("valid");
        assert_eq!(c.effective_window(), 400_000);
    }

    #[test]
    fn moving_the_point_moves_every_derived_threshold() {
        // The derived values follow the setting, not only the stored field.
        let c = ContextConfig::for_model(100_000)
            .and_then(|c| c.with_point(50, PointSource::User))
            .expect("valid");
        assert_eq!(c.compaction_point_tokens(), 50_000);
        assert_eq!(c.worker_plan_target_tokens(), 25_000);
        assert_eq!(c.compact_down_to_tokens(), 25_000);
        assert!(!c.reached_point(49_999));
        assert!(c.reached_point(50_000));
        assert_eq!(c.point_source(), PointSource::User);
    }

    #[test]
    fn the_bounds_are_inclusive_and_outside_them_is_an_error() {
        let base = ContextConfig::for_model(100_000).expect("valid");
        assert!(base.clone().with_point(5, PointSource::User).is_ok());
        assert!(
            base.clone()
                .with_point(95, PointSource::MeasuredRecommendation)
                .is_ok()
        );
        assert!(base.clone().with_point(95, PointSource::User).is_ok());
        for bad in [0, 4, 96, 100, 255] {
            assert_eq!(
                base.clone().with_point(bad, PointSource::User),
                Err(ThresholdError::PointOutOfRange { percent: bad })
            );
        }
    }

    #[test]
    fn a_non_default_value_is_never_labeled_the_unmeasured_default() {
        let base = ContextConfig::for_model(100_000).expect("valid");
        assert_eq!(
            base.clone().with_point(50, PointSource::UnmeasuredDefault),
            Err(ThresholdError::UnmeasuredLabelOnNonDefault { percent: 50 })
        );
        assert!(base.with_point(80, PointSource::UnmeasuredDefault).is_ok());
        assert!(
            serde_json::from_str::<ContextConfig>(
                r#"{"model_window_tokens": 100000, "compaction_point_percent": 60}"#
            )
            .is_err(),
            "a file value without a source defaults the label, so it must be the default"
        );
    }

    #[test]
    fn the_pinned_floor_must_fit_under_the_post_compaction_target() {
        // Default: 100,000 effective, point 80,000, compact down to 40,000.
        let c = ContextConfig::for_model(200_000).expect("valid");
        assert!(c.clone().with_pinned_floor(39_999).is_ok());
        assert_eq!(
            c.clone().with_pinned_floor(40_000),
            Err(ThresholdError::PinnedFloorTooLarge {
                floor: 40_000,
                target: 40_000
            })
        );
        // A floor that fits at 80% stops fitting when the point moves down.
        let with_floor = c.with_pinned_floor(30_000).expect("fits at 80%");
        assert!(matches!(
            with_floor.clone().with_point(50, PointSource::User),
            Err(ThresholdError::PinnedFloorTooLarge { .. })
        ));
        assert!(matches!(
            with_floor.with_ceiling(60_000),
            Err(ThresholdError::PinnedFloorTooLarge { .. })
        ));
    }

    #[test]
    fn zero_window_and_zero_ceiling_are_errors() {
        assert_eq!(ContextConfig::for_model(0), Err(ThresholdError::ZeroWindow));
        let c = ContextConfig::for_model(10).expect("valid");
        assert_eq!(c.with_ceiling(0), Err(ThresholdError::ZeroCeiling));
    }

    #[test]
    fn an_out_of_range_value_in_a_file_is_an_error_not_a_clamp() {
        let err = serde_json::from_str::<ContextConfig>(
            r#"{"model_window_tokens": 200000, "compaction_point_percent": 99}"#,
        )
        .expect_err("must refuse");
        assert!(err.to_string().contains("99%"), "{err}");
        assert!(
            serde_json::from_str::<ContextConfig>(
                r#"{"model_window_tokens": 200000, "compaction_pct": 50}"#
            )
            .is_err(),
            "an unknown key is refused"
        );
    }

    #[test]
    fn a_file_value_round_trips_with_defaults_filled() {
        let c: ContextConfig =
            serde_json::from_str(r#"{"model_window_tokens": 128000}"#).expect("valid");
        assert_eq!(c, ContextConfig::for_model(128_000).expect("valid"));
        let back: ContextConfig =
            serde_json::from_str(&serde_json::to_string(&c).expect("ser")).expect("de");
        assert_eq!(back, c);
    }

    #[test]
    fn a_huge_window_does_not_overflow() {
        let c = ContextConfig::for_model(u64::MAX)
            .and_then(|c| c.with_ceiling(u64::MAX))
            .and_then(|c| c.with_point(95, PointSource::User))
            .expect("valid");
        assert_eq!(c.compaction_point_tokens(), percent_of(u64::MAX, 95));
        assert!(c.compaction_point_tokens() < u64::MAX);
    }
}
