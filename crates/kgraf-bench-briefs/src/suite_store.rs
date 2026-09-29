//! Keeping bench tallies apart by suite version.
//!
//! A tally from one suite says nothing about another: a brief's instruction
//! or check changed, so the rate measures different work. The `ProfileStore`
//! has no suite field, and the router reads its `Source::Bench` tallies as they
//! are, so the store file at `--state-dir` holds only the current suite's
//! bench tallies. The suite's hash sits beside it in [`MARKER`]. When the
//! marker names another suite (or a store has bench tallies and no marker), the
//! first write archives the whole old store as `model_profiles.<hash>.json`
//! and starts the current suite from empty bench tallies. Live tallies are kept.

use anyhow::{Context, Result, bail};
use kgraf_core::model_profile::{FILE_NAME, ProfileStore, Source};
use std::path::{Path, PathBuf};

/// The file naming the suite the bench tallies in the store came from.
pub const MARKER: &str = "bench_suite.json";

/// The store as the current suite sees it.
pub struct SuiteView {
    /// The profile store with no bench tally from another suite.
    pub store: ProfileStore,
    /// The hash of another suite whose bench tallies were set aside, or
    /// `"unknown"` for a store with bench tallies and no marker.
    pub stale: Option<String>,
}

fn has_bench(store: &ProfileStore) -> bool {
    store
        .profiles
        .values()
        .any(|p| p.tallies.contains_key(&Source::Bench))
}

fn read_marker(state_dir: &Path) -> Result<Option<String>> {
    let path = state_dir.join(MARKER);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let value: serde_json::Value = serde_json::from_str(&text)
                .with_context(|| format!("{} does not parse", path.display()))?;
            Ok(value
                .get("suite_hash")
                .and_then(|v| v.as_str())
                .map(str::to_owned))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Read the store in `state_dir` for `suite_hash` without writing anything.
/// Bench tallies recorded under another suite are dropped from the returned
/// store and named in `stale`.
pub fn inspect(state_dir: &Path, suite_hash: &str) -> Result<SuiteView> {
    let mut store = ProfileStore::load(state_dir)
        .with_context(|| format!("cannot load the profile store at {}", state_dir.display()))?;
    let marker = read_marker(state_dir)?;
    let stale = match marker {
        Some(m) if m == suite_hash => None,
        Some(m) => has_bench(&store).then_some(m),
        None => has_bench(&store).then(|| "unknown".to_owned()),
    };
    if stale.is_some() {
        for profile in store.profiles.values_mut() {
            profile.tallies.remove(&Source::Bench);
        }
        store.profiles.retain(|_, p| !p.tallies.is_empty());
    }
    Ok(SuiteView { store, stale })
}

/// The path an old suite's store is archived to.
fn archive_path(state_dir: &Path, old: &str) -> PathBuf {
    let short: String = old
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(16)
        .collect();
    state_dir.join(format!("model_profiles.{short}.json"))
}

/// Open the store for recording under `suite_hash`: archive another suite's
/// bench tallies (see the module docs) and return the store to record into.
/// Nothing is written when the store already belongs to this suite; the marker
/// is written by [`save`].
pub fn open_for_recording(state_dir: &Path, suite_hash: &str) -> Result<ProfileStore> {
    let view = inspect(state_dir, suite_hash)?;
    if let Some(old) = &view.stale {
        let target = archive_path(state_dir, old);
        if target.exists() {
            bail!(
                "{} exists already; move it aside before recording a new suite",
                target.display()
            );
        }
        std::fs::copy(state_dir.join(FILE_NAME), &target).with_context(|| {
            format!(
                "cannot archive the old suite's store to {}",
                target.display()
            )
        })?;
    }
    Ok(view.store)
}

/// Save `store` and the suite marker.
pub fn save(state_dir: &Path, store: &ProfileStore, suite_hash: &str) -> Result<()> {
    store
        .save(state_dir)
        .with_context(|| format!("cannot save the profile store at {}", state_dir.display()))?;
    let body = serde_json::json!({ "suite_hash": suite_hash }).to_string();
    let path = state_dir.join(MARKER);
    let tmp = state_dir.join(format!("{MARKER}.tmp"));
    std::fs::write(&tmp, body).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::cost::Money;
    use kgraf_core::model_profile::TaskKind;

    fn record(store: &mut ProfileStore, source: Source, ok: bool) {
        store.record(
            "m",
            "v",
            source,
            TaskKind::Docs,
            ok,
            Some(Money::from_micros(10)),
        );
    }

    #[test]
    fn a_new_suite_starts_from_empty_bench_tallies_and_keeps_the_old_ones_aside() {
        let dir = tempfile::tempdir().expect("dir");
        let mut store = open_for_recording(dir.path(), "suite-a").expect("open");
        record(&mut store, Source::Bench, true);
        record(&mut store, Source::Live, true);
        save(dir.path(), &store, "suite-a").expect("save");

        // Same suite: tallies carry on.
        let same = open_for_recording(dir.path(), "suite-a").expect("open");
        assert_eq!(
            same.get("m")
                .and_then(|p| p.tally(Source::Bench, TaskKind::Docs))
                .map(|t| t.attempts),
            Some(1)
        );

        // Another suite: the bench tally is gone from the working store, the
        // live one stays, and the old store is archived whole.
        let mut fresh = open_for_recording(dir.path(), "suite-b").expect("open");
        let p = fresh.get("m").expect("live profile kept");
        assert!(p.tally(Source::Bench, TaskKind::Docs).is_none());
        assert!(p.tally(Source::Live, TaskKind::Docs).is_some());
        record(&mut fresh, Source::Bench, false);
        save(dir.path(), &fresh, "suite-b").expect("save");
        let archived: ProfileStore = serde_json::from_str(
            &std::fs::read_to_string(archive_path(dir.path(), "suite-a")).expect("archive"),
        )
        .expect("parse");
        assert_eq!(
            archived
                .get("m")
                .and_then(|p| p.tally(Source::Bench, TaskKind::Docs))
                .map(|t| (t.attempts, t.satisfied)),
            Some((1, 1))
        );
        let now = ProfileStore::load(dir.path()).expect("load");
        let t = now
            .get("m")
            .and_then(|p| p.tally(Source::Bench, TaskKind::Docs))
            .expect("tally");
        assert_eq!((t.attempts, t.satisfied), (1, 0), "no old attempt mixed in");
    }

    #[test]
    fn inspect_hides_another_suites_tallies_without_writing() {
        let dir = tempfile::tempdir().expect("dir");
        let mut store = ProfileStore::default();
        record(&mut store, Source::Bench, true);
        // An old store: tallies and no marker.
        store.save(dir.path()).expect("save");
        let view = inspect(dir.path(), "suite-a").expect("inspect");
        assert_eq!(view.stale.as_deref(), Some("unknown"));
        assert!(view.store.profiles.is_empty(), "no bench tally shown");
        assert!(!dir.path().join(MARKER).exists(), "inspect never writes");

        save(dir.path(), &store, "suite-a").expect("save");
        let view = inspect(dir.path(), "suite-a").expect("inspect");
        assert_eq!(view.stale, None);
        assert_eq!(view.store.profiles.len(), 1);
        let other = inspect(dir.path(), "suite-b").expect("inspect");
        assert_eq!(other.stale.as_deref(), Some("suite-a"));
    }

    #[test]
    fn an_empty_state_dir_is_this_suites() {
        let dir = tempfile::tempdir().expect("dir");
        let view = inspect(dir.path(), "suite-a").expect("inspect");
        assert_eq!(view.stale, None);
        assert!(view.store.profiles.is_empty());
    }
}
