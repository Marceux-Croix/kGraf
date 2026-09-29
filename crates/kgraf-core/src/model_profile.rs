//! Measured per-model capability profiles (decision 0075).
//!
//! A profile records, per task kind, how often a model's result satisfied the
//! brief it was given and what those attempts cost. Two sources write it: the
//! brief-following bench (`kgraf-bench-briefs`) and live verified outcomes. The
//! router reads it. Nothing here is a vendor's claim about its own model: every
//! count is an observed attempt.
//!
//! The store is one JSON file, [`FILE_NAME`], in the project's state directory.
//! A missing file is an empty store, which the router must report as
//! "unmeasured" rather than treat as evidence. A file that does not
//! parse is an error, never an empty store.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cost::Money;

/// The store's file name inside the state directory.
pub const FILE_NAME: &str = "model-profiles.json";

/// The store format this build reads and writes.
pub const STORE_VERSION: u32 = 1;

/// What a unit of work mainly asks of a model. A contract has one kind; the
/// orchestrator assigns it at decomposition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// Declaring or changing an interface other work builds against.
    Interface,
    /// Implementing behavior behind a fixed interface.
    Implementation,
    /// Writing tests against a given behavior.
    Tests,
    /// Changing structure without changing behavior.
    Refactor,
    /// Shell, build and glue scripts.
    Scripting,
    /// Prose: docs, comments, messages.
    Docs,
}

impl TaskKind {
    pub const ALL: [TaskKind; 6] = [
        TaskKind::Interface,
        TaskKind::Implementation,
        TaskKind::Tests,
        TaskKind::Refactor,
        TaskKind::Scripting,
        TaskKind::Docs,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Interface => "interface",
            TaskKind::Implementation => "implementation",
            TaskKind::Tests => "tests",
            TaskKind::Refactor => "refactor",
            TaskKind::Scripting => "scripting",
            TaskKind::Docs => "docs",
        }
    }
}

/// Where a tally's attempts came from. Kept apart so a bench score is never
/// mistaken for live performance, or the reverse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Bench,
    Live,
}

/// Attempts of one kind from one source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindTally {
    pub attempts: u64,
    /// Attempts whose result satisfied the brief (its tests passed and the
    /// delivery check held).
    pub satisfied: u64,
    /// Total cost of every priced attempt, satisfied or not, in micro-dollars.
    pub cost_micros: i64,
    /// Attempts whose model had no price. They count toward the rate but make
    /// the cost unknown, never zero.
    #[serde(default)]
    pub unpriced: u64,
}

impl KindTally {
    /// Record one attempt. `cost` is `None` when the model has no price.
    pub fn record(&mut self, satisfied: bool, cost: Option<Money>) {
        self.attempts += 1;
        if satisfied {
            self.satisfied += 1;
        }
        match cost {
            Some(c) => self.cost_micros += c.micros(),
            None => self.unpriced += 1,
        }
    }

    /// Satisfied over attempts, or `None` with no attempts.
    pub fn satisfaction_rate(&self) -> Option<f64> {
        (self.attempts > 0).then(|| self.satisfied as f64 / self.attempts as f64)
    }

    /// Total cost over satisfied attempts: cost per successful task (0008).
    /// `None` when nothing was satisfied, since the figure is then unbounded,
    /// and when any attempt was unpriced, since the figure is then unknown.
    pub fn cost_per_satisfied(&self) -> Option<Money> {
        (self.satisfied > 0 && self.unpriced == 0)
            .then(|| Money::from_micros(self.cost_micros / self.satisfied as i64))
    }
}

/// One model's measured tallies.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProfile {
    /// The vendor as the provider catalog names it.
    pub vendor: String,
    pub tallies: BTreeMap<Source, BTreeMap<TaskKind, KindTally>>,
}

impl ModelProfile {
    pub fn tally(&self, source: Source, kind: TaskKind) -> Option<&KindTally> {
        self.tallies.get(&source)?.get(&kind)
    }

    pub fn record(&mut self, source: Source, kind: TaskKind, satisfied: bool, cost: Option<Money>) {
        self.tallies
            .entry(source)
            .or_default()
            .entry(kind)
            .or_default()
            .record(satisfied, cost);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("model profiles at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("model profiles at {path} do not parse: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("model profiles at {path} are version {found}; this build reads {STORE_VERSION}")]
    Version { path: String, found: u32 },
}

/// Every measured model, keyed by model id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileStore {
    pub version: u32,
    pub profiles: BTreeMap<String, ModelProfile>,
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            profiles: BTreeMap::new(),
        }
    }
}

impl ProfileStore {
    /// Read the store in `state_dir`. A missing file is an empty store.
    pub fn load(state_dir: &Path) -> Result<Self, ProfileError> {
        let path = state_dir.join(FILE_NAME);
        let shown = path.display().to_string();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ProfileError::Io {
                    path: shown,
                    source,
                });
            }
        };
        let store: Self = serde_json::from_str(&text).map_err(|source| ProfileError::Parse {
            path: shown.clone(),
            source,
        })?;
        if store.version != STORE_VERSION {
            return Err(ProfileError::Version {
                path: shown,
                found: store.version,
            });
        }
        Ok(store)
    }

    /// Write the store to `state_dir`, replacing the file atomically.
    pub fn save(&self, state_dir: &Path) -> Result<(), ProfileError> {
        let path = state_dir.join(FILE_NAME);
        let tmp = state_dir.join(format!("{FILE_NAME}.tmp"));
        let io = |source| ProfileError::Io {
            path: path.display().to_string(),
            source,
        };
        let body = serde_json::to_vec_pretty(self).map_err(|source| ProfileError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        std::fs::create_dir_all(state_dir).map_err(io)?;
        let mut f = std::fs::File::create(&tmp).map_err(io)?;
        f.write_all(&body).map_err(io)?;
        f.sync_all().map_err(io)?;
        std::fs::rename(&tmp, &path).map_err(io)
    }

    pub fn get(&self, model: &str) -> Option<&ModelProfile> {
        self.profiles.get(model)
    }

    pub fn record(
        &mut self,
        model: &str,
        vendor: &str,
        source: Source,
        kind: TaskKind,
        satisfied: bool,
        cost: Option<Money>,
    ) {
        let p = self.profiles.entry(model.to_owned()).or_default();
        if p.vendor.is_empty() {
            p.vendor = vendor.to_owned();
        }
        p.record(source, kind, satisfied, cost);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_an_empty_store_and_a_bad_one_is_an_error() {
        let dir = std::env::temp_dir().join(format!("kgraf-profiles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join(FILE_NAME));
        assert_eq!(ProfileStore::load(&dir).unwrap(), ProfileStore::default());

        std::fs::write(dir.join(FILE_NAME), "{not json").unwrap();
        assert!(matches!(
            ProfileStore::load(&dir),
            Err(ProfileError::Parse { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_saved_store_round_trips_and_keeps_sources_apart() {
        let dir = std::env::temp_dir().join(format!("kgraf-profiles-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = ProfileStore::default();
        s.record(
            "m",
            "v",
            Source::Bench,
            TaskKind::Tests,
            true,
            Some(Money::from_micros(300)),
        );
        s.record(
            "m",
            "v",
            Source::Bench,
            TaskKind::Tests,
            false,
            Some(Money::from_micros(100)),
        );
        s.record(
            "m",
            "v",
            Source::Live,
            TaskKind::Tests,
            true,
            Some(Money::from_micros(50)),
        );
        s.save(&dir).unwrap();

        let back = ProfileStore::load(&dir).unwrap();
        assert_eq!(back, s);
        let bench = back
            .get("m")
            .unwrap()
            .tally(Source::Bench, TaskKind::Tests)
            .unwrap();
        assert_eq!(bench.satisfaction_rate(), Some(0.5));
        assert_eq!(bench.cost_per_satisfied(), Some(Money::from_micros(400)));
        let live = back
            .get("m")
            .unwrap()
            .tally(Source::Live, TaskKind::Tests)
            .unwrap();
        assert_eq!(live.attempts, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unpriced_attempt_counts_toward_the_rate_but_makes_the_cost_unknown() {
        let mut t = KindTally::default();
        t.record(true, None);
        t.record(false, None);
        assert_eq!(t.satisfaction_rate(), Some(0.5));
        assert_eq!(t.unpriced, 2);
        assert_eq!(t.cost_per_satisfied(), None);
        t.record(true, Some(Money::from_micros(90)));
        assert_eq!(t.cost_per_satisfied(), None);
    }

    #[test]
    fn an_old_tally_without_the_unpriced_field_loads() {
        let t: KindTally =
            serde_json::from_str(r#"{"attempts":1,"satisfied":1,"cost_micros":7}"#).unwrap();
        assert_eq!(t.unpriced, 0);
        assert_eq!(t.cost_per_satisfied(), Some(Money::from_micros(7)));
    }

    #[test]
    fn a_tally_with_no_success_has_no_cost_per_success() {
        let mut t = KindTally::default();
        assert_eq!(t.satisfaction_rate(), None);
        t.record(false, Some(Money::from_micros(10)));
        assert_eq!(t.cost_per_satisfied(), None);
    }
}
