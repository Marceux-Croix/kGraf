//! The **old** graph-meta key layout for a schedule definition, and schedule id
//! validation.
//!
//! Schedules used to be one graph-meta row each, `schedule:<id>`. Since
//! decision 0052 section 3 a schedule lives on a work node (`crate::node_store`),
//! and nothing writes these keys any more. They are kept so
//! [`crate::node_store::migrate_legacy`] can find an old row and move it onto a work
//! node. [`validate_schedule_id`] is still the one id check every caller uses.

use crate::error::ScheduleError;

/// **Superseded.** This used to name a single
/// shared row listing every schedule id, read-modify-written by every `schedule
/// add`/`remove` - which meant two concurrent calls could both read the same
/// snapshot, both write back, and the second write's "before" state would not
/// reflect the first, silently losing an entry. No code here writes this key
/// anymore; ids are now derived by scanning `meta` keys under [`SCHEDULE_PREFIX`]
/// (each schedule owns only its own row, so two concurrent calls on two different
/// ids touch two different rows and can never race on a shared one). Kept only so
/// [`schedule_ids_from_meta_keys`] can recognize and skip a leftover blob from a
/// database that predates this change, rather than trying to parse it as a schedule
/// definition and failing the whole read.
pub const INDEX_KEY: &str = "schedule:index";

/// The key prefix every schedule's own row starts with.
pub const SCHEDULE_PREFIX: &str = "schedule:";

/// One schedule's own graph-meta key.
pub fn schedule_key(id: &str) -> String {
    format!("{SCHEDULE_PREFIX}{id}")
}

/// Recovers every schedule id from the raw meta keys a caller already fetched with
/// `GraphStore::meta_keys(SCHEDULE_PREFIX)`. This crate does no I/O of
/// its own (see the module doc), but turning those raw keys into ids - stripping the
/// prefix, and skipping [`INDEX_KEY`] itself, a leftover a database migrated from the
/// earlier single-blob design may still carry - is exactly the shared interpretation
/// this module exists to keep both binaries from drifting on.
pub fn schedule_ids_from_meta_keys(keys: impl IntoIterator<Item = String>) -> Vec<String> {
    keys.into_iter()
        .filter(|k| k != INDEX_KEY)
        .filter_map(|k| k.strip_prefix(SCHEDULE_PREFIX).map(str::to_owned))
        .collect()
}

/// Refuses an id that would collide with [`INDEX_KEY`] itself, or that is not safe as
/// a key/path/ref component (a schedule literally named
/// `index` used to silently overwrite the index blob on the old single-key design;
/// today's per-key layout no longer collides that way, but a permissive id can still
/// collide with a *future* reserved prefix, or break assumptions a shell command, a
/// file name, or a git ref built from it would otherwise get to rely on - refused
/// once, here, rather than trusted everywhere an id is used).
pub fn validate_schedule_id(id: &str) -> Result<(), ScheduleError> {
    if id.is_empty() {
        return Err(ScheduleError::InvalidScheduleId {
            id: id.to_owned(),
            reason: "empty",
        });
    }
    if id == "index" {
        return Err(ScheduleError::InvalidScheduleId {
            id: id.to_owned(),
            reason: "`index` is reserved",
        });
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
    {
        return Err(ScheduleError::InvalidScheduleId {
            id: id.to_owned(),
            reason: "only [A-Za-z0-9_.-] is allowed",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_ids_pass() {
        for id in ["nightly", "a.b-c_9", "X"] {
            validate_schedule_id(id).unwrap();
        }
    }

    #[test]
    fn the_reserved_index_id_is_refused() {
        assert!(validate_schedule_id("index").is_err());
    }

    #[test]
    fn an_empty_id_is_refused() {
        assert!(validate_schedule_id("").is_err());
    }

    #[test]
    fn a_disallowed_character_is_refused() {
        for id in ["a/b", "a b", "a:b", "a\nb"] {
            assert!(
                validate_schedule_id(id).is_err(),
                "{id:?} should be refused"
            );
        }
    }

    /// A test ties them: both binaries build a schedule's key from the same
    /// function, so `foo` and `foo-bar` (a prefix of one another) can never collide
    /// no matter which binary wrote which.
    #[test]
    fn schedule_key_does_not_let_a_prefix_collide_with_a_longer_id() {
        assert_ne!(schedule_key("foo"), schedule_key("foo-bar"));
        assert_ne!(schedule_key("foo"), INDEX_KEY);
    }

    #[test]
    fn schedule_ids_from_meta_keys_skips_the_superseded_index_blob() {
        // A database migrated from the earlier single-blob design
        // may still carry `schedule:index` as a leftover row. It must never surface
        // as a phantom schedule literally named "index" (a name `validate_schedule_id`
        // has always refused for exactly this reason), and it must never be handed to
        // a caller that would then try to parse it as a schedule definition and fail
        // the whole read over one stale row.
        let keys = vec![
            "schedule:index".to_owned(),
            "schedule:nightly".to_owned(),
            "schedule:foo-bar".to_owned(),
            "unrelated:key".to_owned(),
        ];
        let mut ids = schedule_ids_from_meta_keys(keys);
        ids.sort();
        assert_eq!(ids, vec!["foo-bar".to_owned(), "nightly".to_owned()]);
    }
}
