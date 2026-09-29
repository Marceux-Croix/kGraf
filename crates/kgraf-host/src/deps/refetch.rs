//! The host's answer when a worker's sandboxed build could not get a
//! dependency (decision 0087): fetch for the worker's worktree, with consent.

use super::{
    Consent, ConsentStore, Decision, DependencyTool, FetchSite, Readiness, RunFetch, Stop, Trigger,
    decide_mid_run, fetch_and_confirm, redact,
};
use kgraf_tools::{DependencyRefetch, RefetchOutcome};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Most fetches one run makes for its workers. A worker that keeps adding
/// dependencies, or keeps failing for a reason a fetch does not fix, meets
/// this bound instead of a fetch per build.
pub const MAX_WORKER_FETCHES: usize = 8;

/// Most offline checks one run makes for its workers. Each check is a cargo
/// process on the host (about a quarter of a second on an 800-package
/// workspace) that a worker's failing build can trigger; a worker whose
/// build keeps failing for another reason meets this bound instead of one
/// check per build.
pub const MAX_WORKER_CHECKS: usize = 32;

/// One run's refetcher, shared by every worker environment of the run.
#[derive(Clone)]
pub struct HostRefetch {
    inner: Arc<Inner>,
}

struct Inner {
    tool: Arc<dyn DependencyTool>,
    store: ConsentStore,
    run: RunFetch,
    /// The stored consent when the run started (see [`decide_mid_run`]). A
    /// file that did not read counts as unset, so any refusal stored later
    /// wins.
    stored_at_start: Consent,
    checkout: PathBuf,
    /// Set by [`HostRefetch::stop`] at the end of the run: kills a fetch in
    /// flight and refuses new ones.
    stop: Stop,
    /// Checks and fetches made so far. Held while a check or fetch runs, so
    /// two workers never fetch at once.
    counts: Mutex<Counts>,
}

#[derive(Default)]
struct Counts {
    checks: usize,
    fetches: usize,
}

impl HostRefetch {
    pub fn new(
        tool: Arc<dyn DependencyTool>,
        store: ConsentStore,
        run: RunFetch,
        checkout: PathBuf,
    ) -> Self {
        let stored_at_start = store.get(tool.registry()).unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                tool,
                store,
                run,
                stored_at_start,
                checkout,
                stop: Stop::default(),
                counts: Mutex::new(Counts::default()),
            }),
        }
    }

    /// The blocking part of [`DependencyRefetch::refetch`].
    pub fn refetch_blocking(&self, worktree: &Path) -> RefetchOutcome {
        self.inner.refetch(worktree, &self.inner.stop.child())
    }

    /// Tests: holds the lock a running check or fetch holds, so a test can
    /// queue a call behind it.
    #[cfg(test)]
    pub(super) fn hold_for_test(&self) -> impl Sized + '_ {
        self.inner.counts.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// End of the run: a fetch still running is killed, and every later
    /// request is refused.
    pub fn stop(&self) {
        self.inner.stop.stop();
    }
}

impl Inner {
    fn refetch(&self, worktree: &Path, stop: &Stop) -> RefetchOutcome {
        let outcome = self.decide_and_fetch(worktree, stop);
        // What the worker reads names no host folder: not the mirror (the
        // tool replaced it), the worktree, the checkout or the home folder.
        let clean = |text: &str| {
            redact(
                text,
                &[(worktree, "<worktree>"), (&self.checkout, "<checkout>")],
            )
        };
        match outcome {
            RefetchOutcome::Fetched { summary } => RefetchOutcome::Fetched {
                summary: clean(&summary),
            },
            RefetchOutcome::NotAllowed { instruction } => RefetchOutcome::NotAllowed {
                instruction: clean(&instruction),
            },
            RefetchOutcome::Failed { reason } => RefetchOutcome::Failed {
                reason: clean(&reason),
            },
        }
    }

    fn decide_and_fetch(&self, worktree: &Path, stop: &Stop) -> RefetchOutcome {
        let registry = self.tool.registry();
        if stop.is_stopped() {
            return RefetchOutcome::Failed {
                reason: "the run has ended".into(),
            };
        }
        // Read at the moment of need, never cached: a consent the user
        // withdrew mid-run applies to the next build.
        let stored = match self.store.get(registry) {
            Ok(c) => c,
            Err(e) => {
                return RefetchOutcome::Failed {
                    reason: e.to_string(),
                };
            }
        };
        let refused = |why: String| RefetchOutcome::NotAllowed {
            instruction: format!(
                "{why}. A new dependency cannot be added in this run; say so in your result."
            ),
        };
        match decide_mid_run(stored, self.stored_at_start, self.run) {
            Decision::Fetch => {}
            Decision::AskFirst => {
                return refused(format!(
                    "the user has not allowed kGraf to fetch from {registry} for this project \
                     (the user can run `kgraf deps allow`, or `{}` in the project)",
                    registry.manual_command()
                ));
            }
            Decision::Refuse { for_this_run } => {
                return refused(format!(
                    "fetching from {registry} is refused for this {}",
                    if for_this_run { "run" } else { "project" }
                ));
            }
        }
        let site = FetchSite {
            source: worktree.to_path_buf(),
            checkout: self.checkout.clone(),
        };
        let mut counts = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        // Waiting for the lock can outlast the call: the run may have ended
        // while another worker's fetch held it.
        if stop.is_stopped() {
            return RefetchOutcome::Failed {
                reason: "the run has ended".into(),
            };
        }
        // The bounds come first: once reached, no cargo process runs at all.
        if counts.fetches >= MAX_WORKER_FETCHES {
            return RefetchOutcome::Failed {
                reason: format!(
                    "this run already fetched {MAX_WORKER_FETCHES} times for its workers, the \
                     most it does"
                ),
            };
        }
        if counts.checks >= MAX_WORKER_CHECKS {
            return RefetchOutcome::Failed {
                reason: format!(
                    "this run already checked its workers' dependencies {MAX_WORKER_CHECKS} \
                     times, the most it does"
                ),
            };
        }
        counts.checks += 1;
        // The build's words matched a dependency failure; the offline check
        // says whether a fetch could help before any network is used.
        match self.tool.check_watched(&site, stop) {
            Readiness::Missing(_) => {}
            Readiness::Ready | Readiness::NotApplicable => {
                return RefetchOutcome::Failed {
                    reason: "every dependency of the worktree is ready offline, so a fetch \
                             would not help; the failure has another cause"
                        .into(),
                };
            }
            Readiness::Unchecked(why) => {
                return RefetchOutcome::Failed {
                    reason: format!("the offline check could not decide: {why}"),
                };
            }
        }
        counts.fetches += 1;
        match fetch_and_confirm(self.tool.as_ref(), &site, Trigger::Worker, stop) {
            Ok(report) => RefetchOutcome::Fetched {
                summary: report.summary(),
            },
            Err(e) => RefetchOutcome::Failed {
                reason: e.to_string(),
            },
        }
    }
}

/// Stops a call's fetch when the call's future is dropped: finished, or
/// cancelled (the worker's turn interrupted) while the fetch still runs.
struct StopOnDrop(Stop);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}

#[async_trait::async_trait]
impl DependencyRefetch for HostRefetch {
    async fn refetch(&self, worktree: &Path) -> RefetchOutcome {
        // A fetch takes seconds to minutes: never on an async worker. The
        // task owns its own handle; a cancelled call stops its fetch through
        // the guard, since the blocking task itself cannot be aborted.
        let inner = self.inner.clone();
        let worktree = worktree.to_path_buf();
        let stop = inner.stop.child();
        let _guard = StopOnDrop(stop.clone());
        match tokio::task::spawn_blocking(move || inner.refetch(&worktree, &stop)).await {
            Ok(outcome) => outcome,
            Err(e) => RefetchOutcome::Failed {
                reason: format!("the fetch task ended abnormally: {e}"),
            },
        }
    }
}
