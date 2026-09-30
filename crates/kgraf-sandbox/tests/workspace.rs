//! Worktree lifecycle against the real git CLI (the mechanism, not
//! the setting). Every assertion reads git's own porcelain listing or the
//! filesystem, never this crate's bookkeeping.

mod common;

use common::{dead_pid, fixture, git, init_repo};
use kgraf_sandbox::git::WorktreeEntry;
use kgraf_sandbox::{LOCK_TAG, OrphanCause, WorkspaceError, WorkspaceManager, WorkspaceSpec};

fn spec(name: &str) -> WorkspaceSpec<'_> {
    WorkspaceSpec {
        name,
        base: "HEAD",
        branch: None,
    }
}

fn entry_for<'a>(list: &'a [WorktreeEntry], path: &std::path::Path) -> Option<&'a WorktreeEntry> {
    list.iter().find(|e| e.path == path)
}

#[test]
fn create_registers_a_locked_tagged_worktree_at_head() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    mgr.startup().unwrap();
    let ws = mgr.create(&spec("w1")).unwrap();

    assert!(ws.root().join("README").is_file());
    assert!(ws.scratch().is_dir());
    assert_eq!(ws.head(), git(&f.repo, &["rev-parse", "HEAD"]));
    let list = mgr.list().unwrap();
    let entry = entry_for(&list, ws.root()).expect("registered");
    let reason = entry.locked.as_deref().expect("locked");
    assert!(reason.starts_with(&format!("{LOCK_TAG}:pid={}", std::process::id())));
    assert!(entry.detached);
}

#[test]
fn create_with_branch_checks_out_the_new_branch() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let ws = mgr
        .create(&WorkspaceSpec {
            name: "w1",
            base: "main",
            branch: Some("worker/w1"),
        })
        .unwrap();
    assert_eq!(
        git(ws.root(), &["symbolic-ref", "HEAD"]),
        "refs/heads/worker/w1"
    );
}

#[test]
fn invalid_branch_fails_before_any_state_is_created() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let err = mgr
        .create(&WorkspaceSpec {
            name: "w1",
            base: "HEAD",
            branch: Some("bad..name"),
        })
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::GitFailed { .. }), "{err}");
    assert!(!mgr.base().join("w1").exists());
    assert_eq!(mgr.list().unwrap().len(), 1);
}

#[test]
fn second_create_of_a_live_workspace_is_refused() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let _ws = mgr.create(&spec("w1")).unwrap();
    let err = mgr.create(&spec("w1")).unwrap_err();
    assert!(matches!(err, WorkspaceError::AlreadyExists(_)), "{err}");
}

#[test]
fn remove_deletes_directory_registration_and_scratch() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let ws = mgr.create(&spec("w1")).unwrap();
    std::fs::write(ws.root().join("untracked.txt"), "x").unwrap();
    std::fs::create_dir_all(ws.root().join("target")).unwrap();
    std::fs::write(ws.root().join("target/ignored.o"), "x").unwrap();
    let (root, scratch) = (ws.root().to_path_buf(), ws.scratch().to_path_buf());

    mgr.remove(ws).unwrap();
    assert!(!root.exists());
    assert!(!scratch.exists());
    assert!(entry_for(&mgr.list().unwrap(), &root).is_none());
    assert!(
        !git(&f.repo, &["worktree", "list", "--porcelain"]).contains("w1"),
        "git still lists w1"
    );
}

/// A worktree is not deleted under a process started in it: remove reads
/// busy and leaves the tree and its registration in place, then succeeds
/// once the process has ended.
#[cfg(unix)]
#[test]
fn remove_refuses_while_a_process_started_in_the_worktree_runs() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let ws = mgr.create(&spec("w1")).unwrap();
    let mut child = kgraf_sandbox::workspace_command(&ws, kgraf_sandbox::Network::Deny, "/bin/sh")
        .unwrap()
        .args(["-c", "sleep 30"])
        .spawn()
        .unwrap();
    let root = ws.root().to_path_buf();
    let busy = mgr.remove(ws.clone());
    // Stop it before any assertion can fail, so no process outlives the test.
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        matches!(&busy, Err(WorkspaceError::Busy { .. })),
        "{busy:?}"
    );
    assert!(root.join("README").is_file(), "the tree must be left alone");
    assert!(entry_for(&mgr.list().unwrap(), &root).is_some());

    mgr.remove(ws).unwrap();
    assert!(!root.exists());
    assert!(entry_for(&mgr.list().unwrap(), &root).is_none());
}

#[test]
fn remove_succeeds_after_the_worker_rewrote_its_gitlink() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let ws = mgr.create(&spec("w1")).unwrap();
    std::fs::write(ws.root().join(".git"), "gitdir: /nonexistent\n").unwrap();
    let root = ws.root().to_path_buf();
    mgr.remove(ws).unwrap();
    assert!(!root.exists());
    assert!(entry_for(&mgr.list().unwrap(), &root).is_none());
}

#[test]
fn gitlink_tamper_is_detected() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let ws = mgr.create(&spec("w1")).unwrap();
    ws.verify_gitlink().unwrap();
    std::fs::write(ws.root().join(".git"), "gitdir: /elsewhere/.git\n").unwrap();
    let err = ws.verify_gitlink().unwrap_err();
    assert!(
        matches!(err, WorkspaceError::GitlinkTampered { .. }),
        "{err}"
    );
}

#[test]
fn startup_cleans_a_crashed_workspace_whose_directory_vanished() {
    let f = fixture();
    let root;
    {
        let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
        let ws = mgr.create(&spec("w1")).unwrap();
        root = ws.root().to_path_buf();
        // Crash: the directory disappears and remove() never runs.
        std::fs::remove_dir_all(&root).unwrap();
    }
    // Plain prune must NOT clear it: the lock is what keeps the crash visible.
    git(&f.repo, &["worktree", "prune"]);
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    assert!(entry_for(&mgr.list().unwrap(), &root).is_some());

    let removed = mgr.startup().unwrap();
    assert!(
        removed
            .iter()
            .any(|r| r.path == root && r.cause == OrphanCause::DirectoryMissing),
        "{removed:?}"
    );
    assert!(entry_for(&mgr.list().unwrap(), &root).is_none());
}

#[test]
fn startup_cleans_a_workspace_whose_owner_process_died() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let pid = dead_pid();
    let path = mgr.base().join("ghost");
    let reason = format!("{LOCK_TAG}:pid={pid}:name=ghost");
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "--lock",
            "--reason",
            &reason,
            "--detach",
            path.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert!(path.is_dir());

    let removed = mgr.startup().unwrap();
    assert!(
        removed
            .iter()
            .any(|r| r.path == path && r.cause == OrphanCause::OwnerDead { pid }),
        "{removed:?}"
    );
    assert!(!path.exists());
    assert!(entry_for(&mgr.list().unwrap(), &path).is_none());
}

#[test]
fn startup_leaves_live_and_foreign_locks_alone() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let live = mgr.create(&spec("live")).unwrap();
    let foreign = mgr.base().join("foreign");
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "--lock",
            "--reason",
            "user lock",
            "--detach",
            foreign.to_str().unwrap(),
            "HEAD",
        ],
    );
    let removed = mgr.startup().unwrap();
    assert!(removed.is_empty(), "{removed:?}");
    assert!(live.root().is_dir());
    assert!(foreign.is_dir());
}

#[test]
fn startup_removes_unregistered_directories_and_stray_scratch() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let stray = mgr.base().join("half-created");
    std::fs::create_dir_all(stray.join("src")).unwrap();
    let stray_scratch = mgr.base().join(".scratch").join("gone");
    std::fs::create_dir_all(&stray_scratch).unwrap();

    let removed = mgr.startup().unwrap();
    assert!(!stray.exists());
    assert!(!stray_scratch.exists());
    assert_eq!(removed.len(), 2, "{removed:?}");
    assert!(removed.iter().all(|r| r.cause == OrphanCause::Unregistered));
}

#[test]
fn create_replaces_a_dead_workspace_with_the_same_name() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let path = mgr.base().join("w1");
    let reason = format!("{LOCK_TAG}:pid={}:name=w1", dead_pid());
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "--lock",
            "--reason",
            &reason,
            "--detach",
            path.to_str().unwrap(),
            "HEAD",
        ],
    );
    std::fs::write(path.join("leftover"), "x").unwrap();
    let ws = mgr.create(&spec("w1")).unwrap();
    assert!(!ws.root().join("leftover").exists());
    let list = mgr.list().unwrap();
    let reason = entry_for(&list, ws.root()).unwrap().locked.clone().unwrap();
    assert!(reason.contains(&format!("pid={}", std::process::id())));
}

#[test]
fn create_never_overrides_a_user_lock() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let path = mgr.base().join("w1");
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "--lock",
            "--reason",
            "user lock",
            "--detach",
            path.to_str().unwrap(),
            "HEAD",
        ],
    );
    std::fs::write(path.join("user-work"), "x").unwrap();
    let err = mgr.create(&spec("w1")).unwrap_err();
    assert!(matches!(err, WorkspaceError::AlreadyExists(_)), "{err}");
    assert!(path.join("user-work").is_file());
}

/// A worktree registered with `reason` as its lock, holding one file of work.
fn locked_worktree(
    f: &common::Fixture,
    mgr: &WorkspaceManager,
    reason: &str,
) -> std::path::PathBuf {
    let path = mgr.base().join("w1");
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "--lock",
            "--reason",
            reason,
            "--detach",
            path.to_str().unwrap(),
            "HEAD",
        ],
    );
    std::fs::write(path.join("work"), "x").unwrap();
    path
}

/// kGraf's own lock, cut short before its owner: `create` neither keeps it
/// in silence nor replaces it, but fails naming `kgraf clean` and the path.
/// The worktree is listed as stranded, and `clean` removes it on a person's
/// word, after which `create` works again.
#[test]
fn create_fails_loud_on_an_unreadable_kgraf_lock_and_clean_removes_it() {
    for truncated in [
        format!("{LOCK_TAG}:pid="),
        format!("{LOCK_TAG}:pi"),
        "haustoria:v1:pid=x".to_owned(),
    ] {
        let f = fixture();
        let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
        let path = locked_worktree(&f, &mgr, &truncated);

        let err = mgr.create(&spec("w1")).unwrap_err();
        assert!(
            matches!(&err, WorkspaceError::UnreadableLock { name, .. } if name == "w1"),
            "{truncated:?}: {err}"
        );
        let message = err.to_string();
        assert!(message.contains("kgraf clean w1"), "{message}");
        assert!(message.contains(path.to_str().unwrap()), "{message}");
        assert!(path.join("work").is_file(), "nothing was changed");

        assert_eq!(mgr.stranded().unwrap(), vec![path.clone()]);
        assert_eq!(mgr.clean("w1").unwrap(), path);
        assert!(!path.exists());
        mgr.create(&spec("w1")).unwrap();
    }
}

/// A lock without kGraf's tag is someone else's: kept, not listed as
/// stranded, and not removed by `clean`.
#[test]
fn a_foreign_lock_is_never_listed_or_cleaned() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let path = locked_worktree(&f, &mgr, "kgraf-ish but not ours");
    let err = mgr.create(&spec("w1")).unwrap_err();
    assert!(matches!(err, WorkspaceError::AlreadyExists(_)), "{err}");
    assert!(mgr.stranded().unwrap().is_empty());
    assert!(mgr.clean("w1").is_err());
    assert!(path.join("work").is_file());
}

#[test]
fn non_empty_unmarked_base_is_refused() {
    let f = fixture();
    std::fs::create_dir_all(&f.base).unwrap();
    std::fs::write(f.base.join("precious.txt"), "user data").unwrap();
    let err = WorkspaceManager::open(&f.repo, &f.base).err().unwrap();
    assert!(matches!(err, WorkspaceError::UnsafeBaseDir { .. }), "{err}");
    assert!(f.base.join("precious.txt").is_file());
}

#[test]
fn base_containing_the_repository_is_refused() {
    let f = fixture();
    let err = WorkspaceManager::open(&f.repo, f.tmp.path()).err().unwrap();
    assert!(matches!(err, WorkspaceError::UnsafeBaseDir { .. }), "{err}");
}

#[test]
fn non_repository_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let err = WorkspaceManager::open(&plain, &tmp.path().join("ws"))
        .err()
        .unwrap();
    assert!(matches!(err, WorkspaceError::NotARepository(_)), "{err}");
}

#[test]
fn parallel_creates_yield_disjoint_worktrees() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let names: Vec<String> = (0..8).map(|i| format!("p{i}")).collect();
    let roots: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = names
            .iter()
            .map(|n| s.spawn(|| mgr.create(&spec(n)).unwrap().root().to_path_buf()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let list = mgr.list().unwrap();
    for root in &roots {
        assert!(entry_for(&list, root).is_some(), "{root:?} not registered");
    }
    assert_eq!(list.len(), 9);
}

#[test]
fn marker_names_the_owning_repository() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let marker = std::fs::read_to_string(mgr.base().join(".kgraf-workspaces")).unwrap();
    assert!(
        marker.contains(&format!("repo={}", mgr.repo().display())),
        "{marker}"
    );
}

#[test]
fn a_second_repository_cannot_open_or_clean_the_same_base() {
    let f = fixture();
    let r2 = init_repo(&f.tmp.path().join("repo2"));
    let r1_mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let live = r1_mgr.create(&spec("w1")).unwrap();

    // The scenario: r2's startup deleted r1's live worktree.
    let err = WorkspaceManager::open(&r2, &f.base).err().unwrap();
    assert!(matches!(err, WorkspaceError::UnsafeBaseDir { .. }), "{err}");
    assert!(live.root().join("README").is_file());
    assert!(entry_for(&r1_mgr.list().unwrap(), live.root()).is_some());
}

#[test]
fn startup_refuses_to_delete_a_foreign_worktree_planted_in_the_base() {
    let f = fixture();
    let r2 = init_repo(&f.tmp.path().join("repo2"));
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let planted = mgr.base().join("planted");
    git(
        &r2,
        &[
            "worktree",
            "add",
            "--detach",
            planted.to_str().unwrap(),
            "HEAD",
        ],
    );
    let err = mgr.startup().unwrap_err();
    assert!(matches!(err, WorkspaceError::UnsafeBaseDir { .. }), "{err}");
    assert!(planted.join("README").is_file(), "foreign worktree deleted");
}

#[test]
fn a_reused_pid_does_not_pin_an_orphan() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let path = mgr.base().join("reused");
    // Our own (alive) PID, but a start time that is not ours: what a lock
    // looks like after the owner died and the OS handed its PID to us.
    let reason = format!("{LOCK_TAG}:pid={}:start=1:name=reused", std::process::id());
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "--lock",
            "--reason",
            &reason,
            "--detach",
            path.to_str().unwrap(),
            "HEAD",
        ],
    );
    let removed = mgr.startup().unwrap();
    let me = std::process::id();
    assert!(
        removed
            .iter()
            .any(|r| r.path == path && r.cause == OrphanCause::OwnerDead { pid: me }),
        "{removed:?}"
    );
    assert!(!path.exists());
}

/// Run `body` on its own thread and fail if it does not finish within
/// `limit`: a lock regression then fails this test instead of hanging the
/// suite. The stuck thread is leaked; the process ends with the test run.
fn within<T: Send + 'static>(
    limit: std::time::Duration,
    body: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // A send error means the test already timed out; nothing to report.
        let _ = tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)));
    });
    match rx.recv_timeout(limit) {
        Ok(Ok(value)) => value,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!("did not finish within {limit:?}: a lock is starved or deadlocked"),
    }
}

#[test]
fn creates_survive_concurrent_startup_in_this_and_another_manager() {
    // Under full-suite load this once ran past a minute: std's RwLock let the
    // looping cleanup retake the exclusive side ahead of the waiting create.
    // The manager's gate is now first come first served; the deadline turns
    // any return of that starvation into a failure.
    within(std::time::Duration::from_secs(180), || {
        use std::sync::atomic::{AtomicBool, Ordering};
        let f = fixture();
        let creator = WorkspaceManager::open(&f.repo, &f.base).unwrap();
        // A second manager on the same base stands in for a second process: it
        // shares no in-process lock with `creator`, only the marker flock.
        let other = WorkspaceManager::open(&f.repo, &f.base).unwrap();
        let names: Vec<String> = (0..16).map(|i| format!("c{i}")).collect();
        let stop = AtomicBool::new(false);
        // Both cleaners are running before the first create starts, so the
        // creates overlap cleanup rather than racing thread start-up.
        let started = std::sync::Barrier::new(3);
        let roots: Vec<_> = std::thread::scope(|s| {
            let cleaners: Vec<_> = [&creator, &other]
                .into_iter()
                .map(|mgr| {
                    let (stop, started) = (&stop, &started);
                    s.spawn(move || {
                        started.wait();
                        let mut runs = 0u32;
                        while !stop.load(Ordering::Relaxed) {
                            mgr.startup().unwrap();
                            runs += 1;
                        }
                        runs
                    })
                })
                .collect();
            started.wait();
            let roots: Vec<_> = names
                .iter()
                .map(|n| creator.create(&spec(n)).unwrap().root().to_path_buf())
                .collect();
            stop.store(true, Ordering::Relaxed);
            for c in cleaners {
                assert!(c.join().unwrap() > 0, "cleanup never ran");
            }
            roots
        });
        let list = creator.list().unwrap();
        for root in &roots {
            assert!(root.join("README").is_file(), "{root:?} deleted");
            assert!(entry_for(&list, root).is_some(), "{root:?} unregistered");
        }
    });
}

/// Deterministic form of the race: another process is mid-create (it holds
/// the shared marker lock and its directory is not yet registered). Startup
/// must wait for it rather than delete the directory.
#[test]
fn startup_waits_for_another_process_mid_create() {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let marker = std::fs::File::open(mgr.base().join(".kgraf-workspaces")).unwrap();
    marker.lock_shared().unwrap();
    let inflight = mgr.base().join("inflight");
    std::fs::create_dir_all(&inflight).unwrap();

    std::thread::scope(|s| {
        let cleaner = s.spawn(|| mgr.startup().unwrap());
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!cleaner.is_finished(), "startup did not wait for the lock");
        assert!(inflight.is_dir(), "startup deleted an in-flight workspace");
        marker.unlock().unwrap();
        cleaner.join().unwrap();
    });
}
