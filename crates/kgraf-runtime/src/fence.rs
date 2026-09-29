//! A per-path write fence for subagents (decision 0067, point 5).
//!
//! A read-only subagent keeps its parent's full tool definitions, so the
//! shared prefix stays byte-identical across parent and child (0043): a
//! reduced tool list would be a different prefix. Instead, every write the
//! child attempts is refused by [`ScopeFence::read_only`], which fences every
//! path. A writer subagent's parent fences exactly the paths the child owns
//! while it is live ([`ScopeFence::fence`]); the returned [`FenceGuard`] lifts
//! them on drop (the child finishing, failing or being cancelled), so the
//! parent's own writes there resume without another call anywhere.
//!
//! `Worker::scope_violation` (`worker.rs`) checks a fence when the brief was
//! given one. `SpawnOptions::fence` gives a worker a non-`None` value.
//!
//! **Paths compare as the file system resolves them** ([`crate::paths::fold_path`]),
//! the same rule the declared write scope and the forbidden-path check already
//! use: `SRC/a.rs` must fence `src/a.rs` on a case-insensitive volume, or the
//! fence is a check that is weaker than the scope it exists to backstop.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

#[derive(Default)]
struct Inner {
    /// The currently fenced paths, each as [`crate::paths::fold_path`] folded
    /// them, so lookup and storage use the same key, with the number of live
    /// guards holding it. A guard dropping lifts only its own hold: a path
    /// stays fenced until every guard that fenced it is gone.
    paths: BTreeMap<String, usize>,
    /// Set by [`ScopeFence::read_only`]: every path is fenced, regardless of
    /// `paths`.
    all: bool,
}

/// Paths a worker's own writes are refused to, because a subagent holds them
/// (or, for [`ScopeFence::read_only`], because the worker itself is read-only).
/// Cloning shares the same fenced set.
#[derive(Clone, Default)]
pub struct ScopeFence(Arc<RwLock<Inner>>);

impl ScopeFence {
    /// Fence every path: every write this worker attempts is refused, whatever
    /// path it names. For a read-only subagent.
    pub fn read_only() -> Self {
        let fence = Self::default();
        fence.write_lock().all = true;
        fence
    }

    /// Fence `paths` until the returned guard drops. For a writer subagent's
    /// parent, over the paths the child owns.
    pub fn fence(&self, paths: &[String]) -> FenceGuard {
        let folded: Vec<String> = paths.iter().map(|p| fold(p)).collect();
        {
            let mut inner = self.write_lock();
            for path in &folded {
                *inner.paths.entry(path.clone()).or_insert(0) += 1;
            }
        }
        FenceGuard {
            fence: self.clone(),
            folded,
        }
    }

    /// Whether `path` is currently refused.
    pub fn is_fenced(&self, path: &str) -> bool {
        let inner = self.0.read().unwrap_or_else(|e| e.into_inner());
        inner.all || inner.paths.contains_key(&fold(path))
    }

    /// Whether this fence refuses anything at all: every path
    /// ([`ScopeFence::read_only`]), or at least one fenced path. An exclusive
    /// call (one that declares no paths, so [`ScopeFence::is_fenced`] is never
    /// asked about a specific one) must still be refused when this is true, or
    /// the fence fails open on exactly the calls it most needs to catch.
    pub fn is_any_fenced(&self) -> bool {
        let inner = self.0.read().unwrap_or_else(|e| e.into_inner());
        inner.all || !inner.paths.is_empty()
    }

    fn write_lock(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.0.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// Fold a path string the way the file system resolves it
/// ([`crate::paths::fold_path`]), so `SRC/a.rs` and `src/a.rs` fence the same
/// key.
fn fold(path: &str) -> String {
    crate::paths::fold_path(Path::new(path))
}

/// Lifts the paths it was made with when dropped.
pub struct FenceGuard {
    fence: ScopeFence,
    /// The folded keys [`ScopeFence::fence`] inserted, so the guard removes
    /// exactly what it added even if the caller's own strings used different
    /// case or separators.
    folded: Vec<String>,
}

impl Drop for FenceGuard {
    fn drop(&mut self) {
        let mut inner = self.fence.write_lock();
        for path in &self.folded {
            if let Some(holds) = inner.paths.get_mut(path) {
                *holds -= 1;
                if *holds == 0 {
                    inner.paths.remove(path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_fence_refuses_nothing() {
        let fence = ScopeFence::default();
        assert!(!fence.is_fenced("src/lib.rs"));
    }

    #[test]
    fn read_only_fences_every_path() {
        let fence = ScopeFence::read_only();
        assert!(fence.is_fenced("src/lib.rs"));
        assert!(fence.is_fenced("anything at all"));
    }

    #[test]
    fn fencing_is_scoped_to_the_guard() {
        let fence = ScopeFence::default();
        {
            let _guard = fence.fence(&["src/a.rs".to_owned()]);
            assert!(fence.is_fenced("src/a.rs"));
            assert!(!fence.is_fenced("src/b.rs"));
        }
        assert!(
            !fence.is_fenced("src/a.rs"),
            "the guard lifts the fence on drop"
        );
    }

    #[test]
    fn a_path_held_by_two_guards_stays_fenced_until_both_drop() {
        let fence = ScopeFence::default();
        let first = fence.fence(&["src/a.rs".to_owned(), "src/only_first.rs".to_owned()]);
        let second = fence.fence(&["SRC/a.rs".to_owned()]);
        drop(first);
        assert!(
            fence.is_fenced("src/a.rs"),
            "the second guard still holds the path"
        );
        assert!(
            !fence.is_fenced("src/only_first.rs"),
            "a path only the first guard held is lifted"
        );
        drop(second);
        assert!(!fence.is_fenced("src/a.rs"));
        assert!(!fence.is_any_fenced());
    }

    #[test]
    fn a_path_repeated_within_one_guard_is_lifted_by_that_guard() {
        let fence = ScopeFence::default();
        let guard = fence.fence(&["a.rs".to_owned(), "a.rs".to_owned()]);
        drop(guard);
        assert!(!fence.is_any_fenced());
    }

    #[test]
    fn a_fenced_path_is_fenced_under_any_case_or_normalization() {
        let fence = ScopeFence::default();
        // Fence the already-folded spelling, then look up a differently-cased
        // one: if `is_fenced` compared the raw string instead of folding its
        // own argument, this could only pass by coincidence (looking up the
        // same casing that was fenced proves nothing about folding on the
        // read side).
        let _guard = fence.fence(&["src/a.rs".to_owned()]);
        assert!(
            fence.is_fenced("SRC/A.rs"),
            "the same file on a case-insensitive volume must be fenced too"
        );
        assert!(!fence.is_fenced("src/b.rs"));
    }

    #[test]
    fn is_any_fenced_reports_a_fresh_fence_as_empty_and_read_only_as_everything() {
        let fresh = ScopeFence::default();
        assert!(!fresh.is_any_fenced());
        let _guard = fresh.fence(&["a.rs".to_owned()]);
        assert!(fresh.is_any_fenced());

        assert!(ScopeFence::read_only().is_any_fenced());
    }
}
