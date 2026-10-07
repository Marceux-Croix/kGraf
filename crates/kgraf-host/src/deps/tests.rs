//! The consent gate with a fake package manager: no test contacts a registry.

use super::*;
use std::sync::Mutex;

/// A package manager whose cache is a flag: `check` says missing until a
/// `fetch` runs (unless `fetch_fixes` is false). Records every call.
#[derive(Default)]
pub(crate) struct FakeTool {
    pub(crate) applies: bool,
    pub(crate) ready: Mutex<bool>,
    pub(crate) fetch_fixes: bool,
    pub(crate) fetch_fails: bool,
    /// The failed fetch's words, when set (default "network unreachable").
    pub(crate) fail_detail: Option<String>,
    /// `fetch` waits (up to 10 s) for its stop, then fails.
    pub(crate) wait_for_stop: bool,
    pub(crate) saw_stop: std::sync::atomic::AtomicBool,
    /// Once `fetch` ran, `check` cannot decide.
    pub(crate) unchecked_after_fetch: bool,
    /// `check_watched` waits (up to 10 s) for its stop, then cannot decide.
    pub(crate) check_waits_for_stop: bool,
    pub(crate) check_saw_stop: std::sync::atomic::AtomicBool,
    pub(crate) calls: Mutex<Vec<&'static str>>,
}

impl FakeTool {
    pub(crate) fn missing() -> Self {
        Self {
            applies: true,
            fetch_fixes: true,
            ..Self::default()
        }
    }

    pub(crate) fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
}

impl DependencyTool for FakeTool {
    fn registry(&self) -> Registry {
        Registry::CratesIo
    }
    fn applies(&self, _site: &FetchSite) -> bool {
        self.calls.lock().unwrap().push("applies");
        self.applies
    }
    fn check(&self, site: &FetchSite) -> Readiness {
        self.check_watched(site, &Stop::default())
    }
    fn check_watched(&self, _site: &FetchSite, stop: &Stop) -> Readiness {
        self.calls.lock().unwrap().push("check");
        if self.check_waits_for_stop {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !stop.is_stopped() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.check_saw_stop
                .store(stop.is_stopped(), std::sync::atomic::Ordering::SeqCst);
            return Readiness::Unchecked("stopped".into());
        }
        if self.unchecked_after_fetch && self.calls.lock().unwrap().contains(&"fetch") {
            return Readiness::Unchecked("cargo metadata: could not start".into());
        }
        if *self.ready.lock().unwrap() {
            Readiness::Ready
        } else {
            Readiness::Missing("no matching package named `itoa`".into())
        }
    }
    fn fetch(&self, _site: &FetchSite, stop: &Stop) -> Result<FetchReport, DepsError> {
        self.calls.lock().unwrap().push("fetch");
        if self.wait_for_stop {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !stop.is_stopped() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.saw_stop
                .store(stop.is_stopped(), std::sync::atomic::Ordering::SeqCst);
            return Err(DepsError::Fetch {
                registry: Registry::CratesIo,
                detail: "stopped".into(),
            });
        }
        if self.fetch_fails {
            return Err(DepsError::Fetch {
                registry: Registry::CratesIo,
                detail: self
                    .fail_detail
                    .clone()
                    .unwrap_or_else(|| "network unreachable".into()),
            });
        }
        if self.fetch_fixes {
            *self.ready.lock().unwrap() = true;
        }
        Ok(FetchReport {
            registry: Registry::CratesIo,
            contacted: vec!["Updating crates.io index".into()],
            downloaded: 1,
            elapsed: Duration::from_millis(5),
        })
    }
}

fn setup() -> (tempfile::TempDir, ConsentStore, FetchSite) {
    let dir = tempfile::tempdir().unwrap();
    let store = ConsentStore::in_state_dir(dir.path());
    let site = FetchSite::project(dir.path());
    (dir, store, site)
}

#[test]
fn unset_and_no_answer_fails_loud_with_the_instruction_and_never_fetches() {
    let (_d, store, site) = setup();
    let tool = FakeTool::missing();
    let err = ensure(&tool, &site, &store, RunFetch::Stored, Trigger::RunStart).unwrap_err();
    assert!(matches!(err, DepsError::ConsentNeeded { .. }), "{err}");
    let text = err.to_string();
    for needle in [
        "kgraf deps allow",
        "--fetch-deps",
        "--no-fetch-deps",
        "cargo fetch",
        "crates.io",
        "itoa",
    ] {
        assert!(text.contains(needle), "`{needle}` missing from: {text}");
    }
    assert!(!tool.calls().contains(&"fetch"), "{:?}", tool.calls());
    assert!(!err.is_transient());
}

#[test]
fn stored_allowed_fetches_before_going_on_then_checks_again() {
    let (_d, store, site) = setup();
    store.set(Registry::CratesIo, Consent::Allowed).unwrap();
    let tool = FakeTool::missing();
    let out = ensure(&tool, &site, &store, RunFetch::Stored, Trigger::RunStart).unwrap();
    assert!(
        matches!(&out, GateOutcome::Fetched(r) if r.downloaded == 1),
        "{out:?}"
    );
    assert_eq!(tool.calls(), ["applies", "check", "fetch", "check"]);
}

#[test]
fn stored_refused_fails_loud_and_never_fetches() {
    let (_d, store, site) = setup();
    store.set(Registry::CratesIo, Consent::Refused).unwrap();
    let tool = FakeTool::missing();
    let err = ensure(&tool, &site, &store, RunFetch::Stored, Trigger::RunStart).unwrap_err();
    assert!(
        matches!(
            err,
            DepsError::Refused {
                for_this_run: false,
                ..
            }
        ),
        "{err}"
    );
    assert!(err.to_string().contains("kgraf deps allow"), "{err}");
    assert!(!tool.calls().contains(&"fetch"));
}

#[test]
fn a_runs_answer_wins_over_the_stored_one_both_ways() {
    let (_d, store, site) = setup();
    store.set(Registry::CratesIo, Consent::Refused).unwrap();
    let tool = FakeTool::missing();
    assert!(matches!(
        ensure(&tool, &site, &store, RunFetch::Allow, Trigger::RunStart).unwrap(),
        GateOutcome::Fetched(_)
    ));
    store.set(Registry::CratesIo, Consent::Allowed).unwrap();
    let tool = FakeTool::missing();
    let err = ensure(&tool, &site, &store, RunFetch::Refuse, Trigger::RunStart).unwrap_err();
    assert!(
        matches!(
            err,
            DepsError::Refused {
                for_this_run: true,
                ..
            }
        ),
        "{err}"
    );
    assert!(err.to_string().contains("--no-fetch-deps"), "{err}");
    assert!(!tool.calls().contains(&"fetch"));
    // A run's answer is stored nowhere.
    assert_eq!(store.get(Registry::CratesIo).unwrap(), Consent::Allowed);
}

#[test]
fn ready_or_not_applicable_needs_no_consent_file_and_no_fetch() {
    let (_d, store, site) = setup();
    // A damaged file would fail any read: the ready path must not read it.
    std::fs::write(store.path(), "not json").unwrap();
    let tool = FakeTool::missing();
    *tool.ready.lock().unwrap() = true;
    assert_eq!(
        ensure(&tool, &site, &store, RunFetch::Stored, Trigger::RunStart).unwrap(),
        GateOutcome::Ready
    );
    let other = FakeTool::default();
    assert_eq!(
        ensure(&other, &site, &store, RunFetch::Stored, Trigger::RunStart).unwrap(),
        GateOutcome::NotApplicable
    );
    assert_eq!(
        other.calls(),
        ["applies"],
        "no check for a project without cargo"
    );
}

#[test]
fn a_fetch_that_leaves_a_dependency_missing_fails_loud() {
    let (_d, store, site) = setup();
    store.set(Registry::CratesIo, Consent::Allowed).unwrap();
    let tool = FakeTool {
        fetch_fixes: false,
        ..FakeTool::missing()
    };
    let err = ensure(&tool, &site, &store, RunFetch::Stored, Trigger::RunStart).unwrap_err();
    assert!(matches!(err, DepsError::StillMissing { .. }), "{err}");
    let failing = FakeTool {
        fetch_fails: true,
        ..FakeTool::missing()
    };
    let err = ensure(&failing, &site, &store, RunFetch::Stored, Trigger::RunStart).unwrap_err();
    assert!(
        matches!(err, DepsError::Fetch { .. }) && err.is_transient(),
        "{err}"
    );
}

/// After a fetch, a check that cannot decide confirms nothing: the fetch
/// fails loud with the reason, at run start and for a worker alike.
#[test]
fn a_check_that_cannot_decide_after_a_fetch_is_a_failure_to_confirm() {
    let (_d, store, site) = setup();
    store.set(Registry::CratesIo, Consent::Allowed).unwrap();
    let tool = FakeTool {
        unchecked_after_fetch: true,
        ..FakeTool::missing()
    };
    let err = ensure(&tool, &site, &store, RunFetch::Stored, Trigger::RunStart).unwrap_err();
    assert!(
        matches!(&err, DepsError::StillMissing { detail, .. }
            if detail.contains("could not confirm") && detail.contains("could not start")),
        "{err}"
    );
    assert_eq!(tool.calls(), ["applies", "check", "fetch", "check"]);
}

/// The consent question says what the consent covers beyond the start of the
/// run: workers may fetch crates they add, up to the per-run bound.
#[test]
fn the_consent_text_says_workers_may_fetch_what_they_add() {
    let q = ConsentQuestion {
        subject: crate::deps::ConsentSubject::Fetch(Registry::CratesIo),
        project: PathBuf::from("/p"),
        detail: "no matching package named `itoa`".into(),
    };
    let text = q.text();
    assert!(
        text.contains("workers fetch crates they add during the run"),
        "{text}"
    );
    assert!(
        text.contains(&format!("up to {MAX_WORKER_FETCHES} fetches per run")),
        "{text}"
    );
    assert!(
        text.contains("names of those crates go to the registry"),
        "{text}"
    );
    assert!(text.contains("outside the workers' sandbox"), "{text}");
}

#[test]
fn the_stored_setting_persists_across_stores_and_resets() {
    let (dir, store, _site) = setup();
    assert_eq!(store.get(Registry::CratesIo).unwrap(), Consent::Unset);
    store.set(Registry::CratesIo, Consent::Allowed).unwrap();
    let reopened = ConsentStore::in_state_dir(dir.path());
    assert_eq!(reopened.get(Registry::CratesIo).unwrap(), Consent::Allowed);
    let text = std::fs::read_to_string(reopened.path()).unwrap();
    assert!(text.contains("\"crates.io\": \"allowed\""), "{text}");
    reopened.set(Registry::CratesIo, Consent::Refused).unwrap();
    assert_eq!(store.get(Registry::CratesIo).unwrap(), Consent::Refused);
    reopened.set(Registry::CratesIo, Consent::Unset).unwrap();
    assert_eq!(store.get(Registry::CratesIo).unwrap(), Consent::Unset);
}

#[test]
fn a_damaged_or_foreign_consent_file_is_an_error_never_unset() {
    let (_d, store, _site) = setup();
    std::fs::write(
        store.path(),
        "{\"version\":1,\"registries\":{\"crates.io\":\"maybe\"}}",
    )
    .unwrap();
    assert!(matches!(
        store.get(Registry::CratesIo),
        Err(DepsError::Store { .. })
    ));
    std::fs::write(store.path(), "{\"version\":3,\"registries\":{}}").unwrap();
    assert!(matches!(
        store.get(Registry::CratesIo),
        Err(DepsError::Store { .. })
    ));
}

/// Every cell of the consent rule, so a mutation of any arm fails here.
#[test]
fn the_consent_rule_table() {
    use Consent::*;
    let cases = [
        (Unset, RunFetch::Stored, Decision::AskFirst),
        (Allowed, RunFetch::Stored, Decision::Fetch),
        (
            Refused,
            RunFetch::Stored,
            Decision::Refuse {
                for_this_run: false,
            },
        ),
        (Unset, RunFetch::Allow, Decision::Fetch),
        (Allowed, RunFetch::Allow, Decision::Fetch),
        (Refused, RunFetch::Allow, Decision::Fetch),
        (
            Unset,
            RunFetch::Refuse,
            Decision::Refuse { for_this_run: true },
        ),
        (
            Allowed,
            RunFetch::Refuse,
            Decision::Refuse { for_this_run: true },
        ),
        (
            Refused,
            RunFetch::Refuse,
            Decision::Refuse { for_this_run: true },
        ),
    ];
    for (stored, run, want) in cases {
        assert_eq!(decide(stored, run), want, "{stored:?} {run:?}");
    }
}

mod mid_run {
    use super::*;
    use kgraf_tools::RefetchOutcome;

    fn refetcher(tool: Arc<FakeTool>, store: &ConsentStore, run: RunFetch) -> HostRefetch {
        HostRefetch::new(tool, store.clone(), run, PathBuf::from("/checkout"))
    }

    use std::sync::Arc;

    #[test]
    fn with_consent_the_host_fetches_for_the_worktree() {
        let (_d, store, _site) = setup();
        store.set(Registry::CratesIo, Consent::Allowed).unwrap();
        let tool = Arc::new(FakeTool::missing());
        let out =
            refetcher(tool.clone(), &store, RunFetch::Stored).refetch_blocking(Path::new("/wt"));
        assert!(
            matches!(&out, RefetchOutcome::Fetched { summary } if summary.contains("crates.io")),
            "{out:?}"
        );
        assert_eq!(tool.calls(), ["check", "fetch", "check"]);
    }

    #[test]
    fn a_runs_allow_covers_its_workers_too() {
        let (_d, store, _site) = setup();
        let tool = Arc::new(FakeTool::missing());
        let out =
            refetcher(tool.clone(), &store, RunFetch::Allow).refetch_blocking(Path::new("/wt"));
        assert!(matches!(out, RefetchOutcome::Fetched { .. }), "{out:?}");
    }

    #[test]
    fn without_consent_the_failure_stands_with_the_instruction() {
        let (_d, store, _site) = setup();
        let tool = Arc::new(FakeTool::missing());
        let out =
            refetcher(tool.clone(), &store, RunFetch::Stored).refetch_blocking(Path::new("/wt"));
        assert!(
            matches!(&out, RefetchOutcome::NotAllowed { instruction } if instruction.contains("kgraf deps allow")),
            "{out:?}"
        );
        store.set(Registry::CratesIo, Consent::Refused).unwrap();
        let out =
            refetcher(tool.clone(), &store, RunFetch::Stored).refetch_blocking(Path::new("/wt"));
        assert!(matches!(out, RefetchOutcome::NotAllowed { .. }), "{out:?}");
        let out =
            refetcher(tool.clone(), &store, RunFetch::Refuse).refetch_blocking(Path::new("/wt"));
        assert!(matches!(out, RefetchOutcome::NotAllowed { .. }), "{out:?}");
        assert!(
            tool.calls().is_empty(),
            "nothing checked or fetched: {:?}",
            tool.calls()
        );
    }

    #[test]
    fn a_consent_withdrawn_mid_run_applies_to_the_next_build() {
        let (_d, store, _site) = setup();
        store.set(Registry::CratesIo, Consent::Allowed).unwrap();
        let tool = Arc::new(FakeTool {
            fetch_fixes: false,
            ..FakeTool::missing()
        });
        let r = refetcher(tool.clone(), &store, RunFetch::Stored);
        assert!(matches!(
            r.refetch_blocking(Path::new("/wt")),
            RefetchOutcome::Failed { .. }
        ));
        store.set(Registry::CratesIo, Consent::Refused).unwrap();
        assert!(matches!(
            r.refetch_blocking(Path::new("/wt")),
            RefetchOutcome::NotAllowed { .. }
        ));
        assert_eq!(tool.calls().iter().filter(|c| **c == "fetch").count(), 1);
    }

    #[test]
    fn a_ready_worktree_is_not_fetched_for_and_fetches_are_bounded() {
        let (_d, store, _site) = setup();
        store.set(Registry::CratesIo, Consent::Allowed).unwrap();
        let ready = Arc::new(FakeTool::missing());
        *ready.ready.lock().unwrap() = true;
        let out =
            refetcher(ready.clone(), &store, RunFetch::Stored).refetch_blocking(Path::new("/wt"));
        assert!(matches!(out, RefetchOutcome::Failed { .. }), "{out:?}");
        assert_eq!(ready.calls(), ["check"]);

        let stuck = Arc::new(FakeTool {
            fetch_fixes: false,
            ..FakeTool::missing()
        });
        let r = refetcher(stuck.clone(), &store, RunFetch::Stored);
        for _ in 0..MAX_WORKER_FETCHES + 3 {
            r.refetch_blocking(Path::new("/wt"));
        }
        let fetched = stuck.calls().iter().filter(|c| **c == "fetch").count();
        assert_eq!(fetched, MAX_WORKER_FETCHES);
        // The bound is tested before the check: past it, no cargo runs at all
        // (each fetch was confirmed by one check before and one after).
        let checks = stuck.calls().iter().filter(|c| **c == "check").count();
        assert_eq!(checks, 2 * MAX_WORKER_FETCHES, "{:?}", stuck.calls());
    }

    /// Checks are bounded too: a worker whose build keeps failing for a
    /// reason a fetch does not fix meets the bound, not a check per build.
    #[test]
    fn checks_are_bounded_per_run() {
        let (_d, store, _site) = setup();
        store.set(Registry::CratesIo, Consent::Allowed).unwrap();
        let ready = Arc::new(FakeTool::missing());
        *ready.ready.lock().unwrap() = true;
        let r = refetcher(ready.clone(), &store, RunFetch::Stored);
        let mut last = None;
        for _ in 0..MAX_WORKER_CHECKS + 5 {
            last = Some(r.refetch_blocking(Path::new("/wt")));
        }
        assert_eq!(ready.calls().len(), MAX_WORKER_CHECKS);
        assert!(
            matches!(&last, Some(RefetchOutcome::Failed { reason }) if reason.contains("32")),
            "{last:?}"
        );
    }

    /// A refusal stored after the run started wins over the run's own
    /// "allow"; one stored before it was overridden by that answer.
    #[test]
    fn a_refusal_stored_mid_run_wins_over_the_runs_allow() {
        let (_d, store, _site) = setup();
        let tool = Arc::new(FakeTool {
            fetch_fixes: false,
            ..FakeTool::missing()
        });
        let r = refetcher(tool.clone(), &store, RunFetch::Allow);
        assert!(matches!(
            r.refetch_blocking(Path::new("/wt")),
            RefetchOutcome::Failed { .. }
        ));
        store.set(Registry::CratesIo, Consent::Refused).unwrap();
        let out = r.refetch_blocking(Path::new("/wt"));
        assert!(
            matches!(&out, RefetchOutcome::NotAllowed { instruction } if instruction.contains("project")),
            "{out:?}"
        );
        assert_eq!(tool.calls().iter().filter(|c| **c == "fetch").count(), 1);
        // Refused before the start: the run's "allow" stands.
        let before = Arc::new(FakeTool::missing());
        let r = refetcher(before.clone(), &store, RunFetch::Allow);
        assert!(matches!(
            r.refetch_blocking(Path::new("/wt")),
            RefetchOutcome::Fetched { .. }
        ));
    }

    /// A second start gate of the run replaces the consent the refetcher took
    /// at the first: a refusal the user had stored before the second gate
    /// was overridden by that run's answer, so the run's "allow" stands
    /// again; the fetch count of the run is kept.
    #[test]
    fn a_second_start_gate_replaces_the_refetchers_consent_at_start() {
        let (_d, store, _site) = setup();
        // The fetch runs but leaves the package missing: each attempt is
        // `Failed` (it fetched) or `NotAllowed` (it did not).
        let tool = Arc::new(FakeTool {
            fetch_fixes: false,
            ..FakeTool::missing()
        });
        let fetches = || tool.calls().iter().filter(|c| **c == "fetch").count();
        // First gate: consent unset, so a refusal stored later wins.
        let r = refetcher(tool.clone(), &store, RunFetch::Allow);
        assert!(matches!(
            r.refetch_blocking(Path::new("/wt")),
            RefetchOutcome::Failed { .. }
        ));
        store.set(Registry::CratesIo, Consent::Refused).unwrap();
        assert!(matches!(
            r.refetch_blocking(Path::new("/wt")),
            RefetchOutcome::NotAllowed { .. }
        ));
        assert_eq!(fetches(), 1);
        // Second gate: it read the refusal and the run's answer overrode it.
        r.set_consent_at_start(Consent::Refused);
        assert!(matches!(
            r.refetch_blocking(Path::new("/wt")),
            RefetchOutcome::Failed { .. }
        ));
        assert_eq!(fetches(), 2);
    }

    /// The end of the run kills a worker's fetch in flight and refuses the
    /// next one.
    #[test]
    fn stopping_the_refetcher_stops_its_fetch() {
        let (_d, store, _site) = setup();
        store.set(Registry::CratesIo, Consent::Allowed).unwrap();
        let tool = Arc::new(FakeTool {
            wait_for_stop: true,
            ..FakeTool::missing()
        });
        let r = refetcher(tool.clone(), &store, RunFetch::Stored);
        let worker = {
            let r = r.clone();
            std::thread::spawn(move || r.refetch_blocking(Path::new("/wt")))
        };
        while !tool.calls().contains(&"fetch") {
            std::thread::sleep(Duration::from_millis(5));
        }
        r.stop();
        let out = worker.join().unwrap();
        assert!(matches!(out, RefetchOutcome::Failed { .. }), "{out:?}");
        assert!(tool.saw_stop.load(std::sync::atomic::Ordering::SeqCst));
        let after = r.refetch_blocking(Path::new("/wt"));
        assert!(
            matches!(&after, RefetchOutcome::Failed { reason } if reason.contains("ended")),
            "{after:?}"
        );
    }

    /// A fetch whose confirming check cannot decide is a failure for the
    /// worker, with the reason.
    #[test]
    fn a_worker_is_told_when_the_check_after_its_fetch_cannot_decide() {
        let (_d, store, _site) = setup();
        store.set(Registry::CratesIo, Consent::Allowed).unwrap();
        let tool = Arc::new(FakeTool {
            unchecked_after_fetch: true,
            ..FakeTool::missing()
        });
        let out = refetcher(tool, &store, RunFetch::Stored).refetch_blocking(Path::new("/wt"));
        assert!(
            matches!(&out, RefetchOutcome::Failed { reason } if reason.contains("could not confirm")),
            "{out:?}"
        );
    }

    /// The offline check is stopped with the run too: it does not run on to
    /// its own timeout after the run ended.
    #[test]
    fn stopping_the_refetcher_stops_its_check() {
        let (_d, store, _site) = setup();
        store.set(Registry::CratesIo, Consent::Allowed).unwrap();
        let tool = Arc::new(FakeTool {
            check_waits_for_stop: true,
            ..FakeTool::missing()
        });
        let r = refetcher(tool.clone(), &store, RunFetch::Stored);
        let worker = {
            let r = r.clone();
            std::thread::spawn(move || r.refetch_blocking(Path::new("/wt")))
        };
        while !tool.calls().contains(&"check") {
            std::thread::sleep(Duration::from_millis(5));
        }
        r.stop();
        let out = worker.join().unwrap();
        assert!(matches!(out, RefetchOutcome::Failed { .. }), "{out:?}");
        assert!(
            tool.check_saw_stop
                .load(std::sync::atomic::Ordering::SeqCst),
            "the check was given the run's stop"
        );
        assert!(!tool.calls().contains(&"fetch"), "{:?}", tool.calls());
    }

    /// A call queued behind another worker's fetch is stopped when the run
    /// ends while it waits: no check, no fetch after the lock is free.
    #[test]
    fn a_call_waiting_for_the_lock_when_the_run_ends_never_runs() {
        let (_d, store, _site) = setup();
        store.set(Registry::CratesIo, Consent::Allowed).unwrap();
        let tool = Arc::new(FakeTool::missing());
        let r = refetcher(tool.clone(), &store, RunFetch::Stored);
        let held = r.hold_for_test();
        let worker = {
            let r = r.clone();
            std::thread::spawn(move || r.refetch_blocking(Path::new("/wt")))
        };
        std::thread::sleep(Duration::from_millis(300));
        r.stop();
        drop(held);
        let out = worker.join().unwrap();
        assert!(
            matches!(&out, RefetchOutcome::Failed { reason } if reason.contains("ended")),
            "{out:?}"
        );
        assert!(tool.calls().is_empty(), "{:?}", tool.calls());
    }

    /// A cancelled call (its future dropped) stops its own fetch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_call_stops_its_fetch() {
        let (_d, store, _site) = setup();
        store.set(Registry::CratesIo, Consent::Allowed).unwrap();
        let tool = Arc::new(FakeTool {
            wait_for_stop: true,
            ..FakeTool::missing()
        });
        let r = refetcher(tool.clone(), &store, RunFetch::Stored);
        let call = kgraf_tools::DependencyRefetch::refetch(&r, Path::new("/wt"));
        let timed_out = tokio::time::timeout(Duration::from_millis(200), call).await;
        assert!(timed_out.is_err(), "the fake fetch waits for its stop");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !tool.saw_stop.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(tool.saw_stop.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// What the worker reads names no host folder and no proxy credential.
    #[test]
    fn host_paths_and_credentials_are_replaced_in_the_answer() {
        let (d, store, _site) = setup();
        store.set(Registry::CratesIo, Consent::Allowed).unwrap();
        let checkout = d.path().join("checkout");
        let worktree = checkout.join(".kgraf/worktrees/w1");
        std::fs::create_dir_all(&worktree).unwrap();
        let home = std::env::var("HOME").unwrap_or_default();
        // Set by rustup for cargo and its tests; checked only when set.
        let cargo_home = std::env::var("CARGO_HOME").unwrap_or_default();
        let tool = Arc::new(FakeTool {
            fetch_fails: true,
            fail_detail: Some(format!(
                "failed in {}/src and {}/Cargo.toml; notes {home}/kgraf-test-notes; cache \
                 {cargo_home}/registry; proxy \
                 http://alice:s3cret@proxy.example:3128/",
                worktree.display(),
                checkout.display()
            )),
            ..FakeTool::missing()
        });
        let r = HostRefetch::new(tool, store.clone(), RunFetch::Stored, checkout.clone());
        let out = r.refetch_blocking(&worktree);
        let RefetchOutcome::Failed { reason } = out else {
            panic!("{out:?}")
        };
        assert!(reason.contains("<worktree>/src"), "{reason}");
        assert!(reason.contains("<checkout>/Cargo.toml"), "{reason}");
        assert!(
            reason.contains("http://<redacted>@proxy.example:3128"),
            "{reason}"
        );
        assert!(
            !reason.contains("s3cret") && !reason.contains("alice"),
            "{reason}"
        );
        assert!(
            !reason.contains(&d.path().display().to_string()),
            "{reason}"
        );
        if home.len() > 1 {
            assert!(reason.contains("~/kgraf-test-notes"), "{reason}");
            assert!(!reason.contains(&home), "{reason}");
        }
        if cargo_home.len() > 1 {
            assert!(reason.contains("<cargo-home>/registry"), "{reason}");
        }
    }
}

#[test]
fn the_mid_run_rule_table() {
    use Consent::*;
    let refuse_project = Decision::Refuse {
        for_this_run: false,
    };
    // A refusal stored after the start wins over every run answer.
    for run in [RunFetch::Stored, RunFetch::Allow] {
        for start in [Unset, Allowed] {
            assert_eq!(decide_mid_run(Refused, start, run), refuse_project);
        }
    }
    // Otherwise the plain rule.
    for now in [Unset, Allowed, Refused] {
        for run in [RunFetch::Stored, RunFetch::Allow, RunFetch::Refuse] {
            let start = if now == Refused { Refused } else { Unset };
            assert_eq!(decide_mid_run(now, start, run), decide(now, run));
        }
    }
    assert_eq!(
        decide_mid_run(Refused, Unset, RunFetch::Refuse),
        refuse_project
    );
}

/// A `file://` URL spells a space `%20`; a message naming one must not
/// carry the path in that form either.
#[test]
fn a_percent_encoded_path_is_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let odd = dir.path().join("Want To Develop, 100% (a)");
    std::fs::create_dir_all(&odd).unwrap();
    let plain = odd.display().to_string();
    let upper = plain
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace(',', "%2C")
        .replace('(', "%28")
        .replace(')', "%29");
    let lower = upper.replace("%2C", "%2c");
    // What the `url` crate writes: only the space and the percent sign.
    let path_set = plain.replace('%', "%25").replace(' ', "%20");
    let text = format!(
        "error: file://{upper}/x and file://{lower}/y and {plain}/z and file://{path_set}/w"
    );
    let out = redact(&text, &[(odd.as_path(), "<mirror>")]);
    assert!(!out.contains("Want%20To"), "{out}");
    assert!(!out.contains("Want To"), "{out}");
    assert_eq!(out.matches("<mirror>").count(), 4, "{out}");
}

/// The credentials end at the last `@` before the host, whatever the
/// password holds.
#[test]
fn url_credentials_with_odd_characters_are_redacted() {
    for (text, gone) in [
        ("via http://al:p/ss@proxy.example:3128/x", "p/ss"),
        ("via http://al:p)ss@proxy.example/x", "p)ss"),
        ("via (http://al:p?ss#x@proxy.example)", "p?ss"),
        ("via ssh://git:tok@en@host.example/r.git", "tok"),
    ] {
        let out = redact_url_credentials(text);
        assert!(!out.contains(gone), "{text} -> {out}");
        assert!(out.contains("<redacted>@"), "{text} -> {out}");
    }
    // A URL with no credentials is unchanged.
    let plain = "see https://index.crates.io/config.json (ok)";
    assert_eq!(redact_url_credentials(plain), plain);
}
