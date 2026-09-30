//! A schedule's persisted definition: what to run, and when (decision 0052).
//!
//! This type carries no opinion on storage. Decision 0052 puts a schedule "on the
//! graph, as a property of a work node"; `crate::node_store` serializes it with
//! `serde_json` into the graph's `work_schedules` table, against the node it belongs
//! to. Keeping it here, not duplicated in each binary, is the one place its shape is
//! defined.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::ScheduleError;
use crate::schedule::{MissedRunPolicy, Schedule, to_whole_millis};

/// A one-shot instant or a cron pattern, as stored (not yet validated against a
/// zone; see [`ScheduleDefinition::to_schedule`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DefinitionKind {
    OneShot { at: DateTime<Utc> },
    Recurring { cron: String },
}

/// A schedule as it is written to and read from storage.
///
/// `zone` is stored as the raw IANA string, not `chrono_tz::Tz`, so a definition
/// with a zone the running binary's bundled tzdb no longer recognizes (a tzdb
/// downgrade, in practice never expected) still round-trips through storage; it only
/// fails, loudly, at [`ScheduleDefinition::to_schedule`] time, which is exactly
/// where decision 0052's "an unknown timezone is refused" already applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleDefinition {
    pub id: String,
    /// The instruction, verbatim (decision 0056/0057: an instruction is never
    /// paraphrased or summarized on the way into a run).
    pub instruction: String,
    pub zone: String,
    pub kind: DefinitionKind,
    #[serde(default)]
    pub missed_run_policy: MissedRunPolicy,
    /// Turning this off suspends the schedule without deleting its definition or
    /// its run history in the claim table (decision 0052: background mode's global
    /// switch does the same at the process level; this is the per-schedule one).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// When this schedule was added. A scheduler loop uses
    /// this as the baseline for `last_seen` the very first time it ever evaluates
    /// this schedule, instead of the claim table's `None`: without it, a one-shot
    /// due ten seconds after creation, or a daily schedule added while the app was
    /// closed, is skipped outright the first time anything looks at it, because
    /// `Schedule::resolve_wake`'s own `None` branch means "nothing missed by
    /// definition" - correct for a schedule that has genuinely never had an
    /// occurrence yet, wrong for one whose first occurrence already passed between
    /// creation and the first tick.
    #[serde(default = "default_created_at")]
    pub created_at: DateTime<Utc>,
}

fn default_true() -> bool {
    true
}

/// The serde default for a definition stored before `created_at` existed: the
/// earliest representable instant, so such a schedule is always treated as "due
/// since before now" (an immediate wake, `missed_run_policy`'s call from there) -
/// visibly conservative, never silently skipped.
fn default_created_at() -> DateTime<Utc> {
    DateTime::<Utc>::MIN_UTC
}

impl ScheduleDefinition {
    pub fn one_shot(
        id: impl Into<String>,
        instruction: impl Into<String>,
        at: DateTime<Utc>,
        zone: impl Into<String>,
    ) -> Result<Self, ScheduleError> {
        let zone = zone.into();
        let id = id.into();
        // Validated at creation (decision 0052), not deferred to the first tick: a
        // bad zone or pattern is refused before it is ever persisted. The id
        // itself is checked here too, so every caller that ever builds a
        // `ScheduleDefinition` gets the same refusal, not just whichever binary
        // happened to add its own check.
        crate::storage::validate_schedule_id(&id)?;
        // Whole milliseconds, the claim table's resolution (see `to_whole_millis`).
        let at = to_whole_millis(at);
        Schedule::one_shot(id.clone(), at, &zone)?;
        Ok(Self {
            id,
            instruction: instruction.into(),
            zone,
            kind: DefinitionKind::OneShot { at },
            missed_run_policy: MissedRunPolicy::default(),
            enabled: true,
            created_at: to_whole_millis(Utc::now()),
        })
    }

    pub fn recurring(
        id: impl Into<String>,
        instruction: impl Into<String>,
        cron: impl Into<String>,
        zone: impl Into<String>,
    ) -> Result<Self, ScheduleError> {
        let zone = zone.into();
        let cron = cron.into();
        let id = id.into();
        crate::storage::validate_schedule_id(&id)?;
        Schedule::recurring(id.clone(), &cron, &zone)?;
        Ok(Self {
            id,
            instruction: instruction.into(),
            zone,
            kind: DefinitionKind::Recurring { cron },
            missed_run_policy: MissedRunPolicy::default(),
            enabled: true,
            created_at: to_whole_millis(Utc::now()),
        })
    }

    pub fn with_missed_run_policy(mut self, policy: MissedRunPolicy) -> Self {
        self.missed_run_policy = policy;
        self
    }

    /// When this schedule fires, in words: `cron `...` in <zone>` or `once at
    /// <instant>`. Used on the work node's label and by `schedule list`.
    pub fn when(&self) -> String {
        match &self.kind {
            DefinitionKind::OneShot { at } => format!("once at {at}"),
            DefinitionKind::Recurring { cron } => format!("cron `{cron}` in {}", self.zone),
        }
    }

    /// The creation instant in whole milliseconds, as a scheduler loop should use
    /// it: a definition stored before instants were cut (or edited by hand) may
    /// carry sub-millisecond digits.
    pub fn created_at_millis(&self) -> DateTime<Utc> {
        to_whole_millis(self.created_at)
    }

    /// Builds the runtime [`Schedule`] this definition describes. A one-shot
    /// instant is cut to whole milliseconds here too, so a definition stored with
    /// sub-millisecond digits is safe on load. Re-validates the
    /// zone and, for a recurring schedule, the cron pattern: storage round-trips a
    /// definition as plain data, so re-checking here is what keeps the fail-loud refusal
    /// binding on every read, not just at the moment a schedule was created.
    pub fn to_schedule(&self) -> Result<Schedule, ScheduleError> {
        let schedule = match &self.kind {
            DefinitionKind::OneShot { at } => Schedule::one_shot(self.id.clone(), *at, &self.zone)?,
            DefinitionKind::Recurring { cron } => {
                Schedule::recurring(self.id.clone(), cron, &self.zone)?
            }
        };
        Ok(schedule.with_missed_run_policy(self.missed_run_policy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn an_unknown_timezone_is_refused_at_creation() {
        let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let err = ScheduleDefinition::one_shot("s1", "do it", at, "Mars/OlympusMons")
            .expect_err("an unknown zone must be refused");
        assert!(matches!(err, ScheduleError::InvalidTimeZone { .. }));
    }

    #[test]
    fn a_valid_definition_round_trips_through_json_and_builds_its_schedule() {
        let def =
            ScheduleDefinition::recurring("nightly", "run the nightly build", "0 0 3 * * *", "UTC")
                .expect("valid");
        let json = serde_json::to_string(&def).expect("encode");
        let back: ScheduleDefinition = serde_json::from_str(&json).expect("decode");
        assert_eq!(def, back);
        back.to_schedule().expect("builds a runtime schedule");
    }

    #[test]
    fn a_definition_stored_before_created_at_existed_still_decodes() {
        // No `created_at` key at all: exactly what a definition written by an older
        // version of this crate looks like on disk.
        let json = r#"{"id":"old","instruction":"do it","zone":"UTC","kind":{"OneShot":{"at":"2026-01-01T00:00:00Z"}},"missed_run_policy":"RunOnceOnWake","enabled":true}"#;
        let def: ScheduleDefinition = serde_json::from_str(json).expect("decode");
        assert_eq!(def.created_at, DateTime::<Utc>::MIN_UTC);
    }

    /// Every schedule instant is whole milliseconds, at creation
    /// and on load, matching the claim table's resolution.
    #[test]
    fn instants_are_cut_to_whole_milliseconds_at_creation_and_on_load() {
        let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
            + chrono::TimeDelta::microseconds(1_234_567);
        let whole = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap()
            + chrono::TimeDelta::milliseconds(234);
        let def = ScheduleDefinition::one_shot("once", "do it", at, "UTC").expect("valid");
        assert_eq!(def.kind, DefinitionKind::OneShot { at: whole });
        assert_eq!(def.created_at.timestamp_subsec_micros() % 1000, 0);
        // A definition stored with sub-millisecond digits (an older build, a hand
        // edit) is cut on load.
        let mut stored = def.clone();
        stored.kind = DefinitionKind::OneShot { at };
        stored.created_at = at;
        let schedule = stored.to_schedule().expect("builds");
        assert_eq!(
            schedule
                .next_after(whole - chrono::TimeDelta::seconds(1))
                .expect("next"),
            Some(whole)
        );
        assert_eq!(stored.created_at_millis(), whole);
    }

    #[test]
    fn disabling_a_stored_definition_does_not_change_its_shape() {
        let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let mut def = ScheduleDefinition::one_shot("once", "do it", at, "UTC").expect("valid");
        def.enabled = false;
        let json = serde_json::to_string(&def).expect("encode");
        let back: ScheduleDefinition = serde_json::from_str(&json).expect("decode");
        assert!(!back.enabled);
    }
}
