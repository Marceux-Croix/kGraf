//! Continuous merge-back against real git repositories. Every
//! assertion reads git's refs, objects or the filesystem, never this crate's
//! bookkeeping alone.

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{fixture, git};
use kgraf_sandbox::git::Git;
use kgraf_sandbox::{
    Attempt, AttemptGroup, AttemptState, CommitOutcome, FinishOutcome, GeneratedKind, Identity,
    Limits, MergeBack, MergeError, MergeOutcome, MergedWorktree, RefusalReason, WorkspaceManager,
    WorkspaceSpec,
};
// Only referenced by the `#[cfg(unix)]` gitlink-tampering test below (planting a
// symlinked `.git` file is a unix-specific attack shape here); a plain, unconditional
// import left this "unused" on a non-unix `--all-targets` build (found by Windows
// cross-compilation under `-D warnings`).
#[cfg(unix)]
use kgraf_sandbox::WorkspaceError;

const BRANCH: &str = "integration";

/// No declared writes: the attempt may change nothing (decision 0083).
const NONE: &[&str] = &[];

struct Env {
    f: common::Fixture,
    mgr: Arc<WorkspaceManager>,
}

fn env() -> Env {
    let f = fixture();
    git(&f.repo, &["branch", BRANCH, "main"]);
    let mgr = Arc::new(WorkspaceManager::open(&f.repo, &f.base).unwrap());
    mgr.startup().unwrap();
    Env { f, mgr }
}

fn identity() -> Identity {
    Identity {
        name: "kGraf Merge".into(),
        email: "merge@kgraf.invalid".into(),
    }
}

fn merger(e: &Env) -> MergeBack {
    MergeBack::open(e.mgr.clone(), BRANCH, identity()).unwrap()
}

fn attempt(e: &Env, name: &str) -> Attempt {
    let ws = e
        .mgr
        .create(&WorkspaceSpec {
            name,
            base: BRANCH,
            branch: None,
        })
        .unwrap();
    merger(e).attempt(ws).unwrap()
}

fn write(a: &Attempt, rel: &str, text: &str) {
    let path = a.workspace().root().join(rel);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

fn show(repo: &Path, spec: &str) -> String {
    git(repo, &["show", spec])
}

fn tip(repo: &Path) -> String {
    git(repo, &["rev-parse", BRANCH])
}

fn ref_exists(repo: &Path, name: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", name])
        .env_remove("GIT_DIR")
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn disjoint_workers_merge_cleanly_in_finish_order() {
    let e = env();
    let mb = merger(&e);
    let start = tip(&e.f.repo);
    let mut w1 = attempt(&e, "w1");
    let mut w2 = attempt(&e, "w2");
    write(&w1, "a.txt", "from w1\n");
    write(&w2, "src/b.txt", "from w2\n");

    // w2 finishes first and lands before w1 is done: continuous, not batched.
    let out2 = mb.finish(&mut w2, "w2: add b", &["src/b.txt"]).unwrap();
    let FinishOutcome::Merged {
        merge_commit: m2,
        integration_before,
    } = out2
    else {
        panic!("{out2:?}")
    };
    assert_eq!(integration_before, start);
    assert_eq!(tip(&e.f.repo), m2);
    assert_eq!(show(&e.f.repo, &format!("{BRANCH}:src/b.txt")), "from w2");
    assert!(matches!(w1.state(), AttemptState::Working));

    let out1 = mb.finish(&mut w1, "w1: add a", &["a.txt"]).unwrap();
    let FinishOutcome::Merged {
        merge_commit: m1,
        integration_before,
    } = out1
    else {
        panic!("{out1:?}")
    };
    assert_eq!(integration_before, m2, "w1 merged on top of w2's merge");

    // First-parent history is the finish order; each merge is a real merge
    // commit (upstream --no-ff) whose second parent is the worker's commit.
    let log = git(&e.f.repo, &["log", "--first-parent", "--format=%s", BRANCH]);
    assert_eq!(
        log.lines().take(2).collect::<Vec<_>>(),
        [
            "Merge attempt w1 into integration",
            "Merge attempt w2 into integration"
        ]
    );
    assert_eq!(git(&e.f.repo, &["rev-parse", &format!("{m1}^1")]), m2);
    assert_eq!(
        git(&e.f.repo, &["log", "-1", "--format=%s", &format!("{m1}^2")]),
        "w1: add a"
    );
    assert_eq!(
        git(&e.f.repo, &["log", "-1", "--format=%an <%ae>|%cn", &m1]),
        "kGraf Merge <merge@kgraf.invalid>|kGraf Merge"
    );
    assert_eq!(show(&e.f.repo, &format!("{BRANCH}:a.txt")), "from w1");
    assert_eq!(show(&e.f.repo, &format!("{BRANCH}:src/b.txt")), "from w2");

    // Merged attempts leave nothing behind.
    for a in [&w1, &w2] {
        assert!(matches!(a.state(), AttemptState::Merged { .. }));
        assert!(!a.workspace().root().exists());
        assert!(!ref_exists(&e.f.repo, &a.attempt_ref()));
    }
    // The user's checkout was never touched.
    assert_eq!(git(&e.f.repo, &["status", "--porcelain"]), "");
    assert_eq!(git(&e.f.repo, &["rev-parse", "main"]), start);
}

#[test]
fn overlapping_edits_produce_a_typed_conflict_with_both_sides_preserved() {
    let e = env();
    let mb = merger(&e);
    let mut w1 = attempt(&e, "w1");
    let mut w2 = attempt(&e, "w2");
    write(&w1, "README", "w1 says hi\n");
    write(&w2, "README", "w2 says hi\n");
    write(&w2, "extra.txt", "clean part\n");

    assert!(matches!(
        mb.finish(&mut w1, "w1", &["README"]).unwrap(),
        FinishOutcome::Merged { .. }
    ));
    let before = tip(&e.f.repo);
    let out = mb.finish(&mut w2, "w2", &["README", "extra.txt"]).unwrap();
    let FinishOutcome::Conflicted(conflict) = out else {
        panic!("{out:?}")
    };

    // Nothing moved and nothing was overwritten.
    assert_eq!(tip(&e.f.repo), before);
    assert_eq!(show(&e.f.repo, &format!("{BRANCH}:README")), "w1 says hi");
    assert!(!ref_exists(&e.f.repo, &format!("{BRANCH}:extra.txt")));

    assert_eq!(conflict.attempt, "w2");
    assert_eq!(conflict.integration_tip, before);
    assert_eq!(conflict.paths.len(), 1);
    let readme = &conflict.paths[0];
    assert_eq!(readme.path, "README");
    let blob = |side: &Option<kgraf_sandbox::ConflictSide>| {
        git(&e.f.repo, &["cat-file", "-p", &side.as_ref().unwrap().blob])
    };
    assert_eq!(blob(&readme.base), "hello");
    assert_eq!(blob(&readme.integration), "w1 says hi");
    assert_eq!(blob(&readme.attempt), "w2 says hi");
    assert!(
        conflict
            .messages
            .iter()
            .any(|m| m.kind.starts_with("CONFLICT") && m.paths == ["README"]),
        "{:?}",
        conflict.messages
    );
    // Git's marked-up tree is available for display.
    let marked = show(&e.f.repo, &format!("{}:README", conflict.conflicted_tree));
    assert!(marked.contains("<<<<<<<") && marked.contains("w1 says hi"));

    // The attempt side survives under its ref and in its worktree.
    assert!(matches!(w2.state(), AttemptState::Conflicted(_)));
    assert_eq!(
        git(&e.f.repo, &["rev-parse", &w2.attempt_ref()]),
        conflict.attempt_commit
    );
    assert_eq!(
        show(&e.f.repo, &format!("{}:README", w2.attempt_ref())),
        "w2 says hi"
    );
    assert!(w2.workspace().root().join("README").is_file());

    // A conflicted attempt cannot be merged again; only discarded.
    let err = mb.merge(&mut w2).unwrap_err();
    assert!(matches!(err, MergeError::InvalidTransition { .. }), "{err}");
    mb.discard(&mut w2).unwrap();
    assert_eq!(w2.state(), &AttemptState::Discarded);
    assert!(!w2.workspace().root().exists());
    assert!(!ref_exists(&e.f.repo, &w2.attempt_ref()));
    assert_eq!(tip(&e.f.repo), before);
}

#[test]
fn verifier_selection_merges_one_attempt_and_discards_the_rest() {
    let e = env();
    let mb = merger(&e);
    let mut a1 = attempt(&e, "task7-a1");
    let mut a2 = attempt(&e, "task7-a2");
    let a3 = attempt(&e, "task7-a3");
    write(&a1, "README", "attempt one\n");
    write(&a2, "README", "attempt two\n");
    write(&a3, "README", "attempt three\n");
    // Two were committed before the verifier ruled, one was still working.
    mb.commit_declared(&mut a1, "a1", &["README"]).unwrap();
    mb.commit_declared(&mut a2, "a2", &["README"]).unwrap();
    let a1_commit = git(&e.f.repo, &["rev-parse", &a1.attempt_ref()]);
    let dirs: Vec<_> = [&a1, &a2, &a3]
        .iter()
        .map(|a| a.workspace().root().to_path_buf())
        .collect();
    let refs: Vec<_> = [&a1, &a2, &a3].iter().map(|a| a.attempt_ref()).collect();

    let group = AttemptGroup::new("task7", vec![a1, a2, a3]).unwrap();
    let (winner, outcome) = mb.select(group, "task7-a2", "unused", &["README"]).unwrap();
    assert_eq!(winner.name(), "task7-a2");
    assert!(
        matches!(outcome, FinishOutcome::Merged { .. }),
        "{outcome:?}"
    );
    assert_eq!(show(&e.f.repo, &format!("{BRANCH}:README")), "attempt two");

    // Losers are gone with their worktrees and refs, and never reached the branch.
    for (dir, r) in dirs.iter().zip(&refs) {
        assert!(!dir.exists(), "{dir:?}");
        assert!(!ref_exists(&e.f.repo, r), "{r}");
    }
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&e.f.repo)
        .args(["merge-base", "--is-ancestor", &a1_commit, BRANCH])
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(1),
        "loser a1 is not in integration history"
    );
    let log = git(&e.f.repo, &["log", "--format=%s", BRANCH]);
    assert!(!log.contains("a1") && !log.contains("a3"), "{log}");
}

#[test]
fn unknown_winner_is_refused() {
    let e = env();
    let mb = merger(&e);
    let group = AttemptGroup::new("t", vec![attempt(&e, "x1")]).unwrap();
    let err = mb.select(group, "nope", "m", NONE).unwrap_err();
    assert!(matches!(err.error, MergeError::Selection { .. }), "{err}");
    assert_eq!(err.attempts.len(), 1, "the attempt comes back");
}

#[test]
fn an_empty_attempt_merges_nothing_and_is_removed() {
    let e = env();
    let mb = merger(&e);
    let before = tip(&e.f.repo);
    let mut w = attempt(&e, "idle");
    assert_eq!(
        mb.finish(&mut w, "noop", NONE).unwrap(),
        FinishOutcome::Empty
    );
    assert_eq!(tip(&e.f.repo), before);
    assert!(!w.workspace().root().exists());
}

/// An empty attempt whose worktree cannot be removed yet (a process started
/// in it still runs) stays `Working`, so the commit can be tried again once
/// the process has ended; the worktree is removed then.
#[cfg(unix)]
#[test]
fn an_empty_attempt_whose_removal_is_refused_can_be_committed_again() {
    let e = env();
    let mb = merger(&e);
    let mut w = attempt(&e, "idle-busy");
    let mut child =
        kgraf_sandbox::workspace_command(w.workspace(), kgraf_sandbox::Network::Deny, "/bin/sh")
            .unwrap()
            // `exec`: the kill below must end the only process holding the
            // worktree. A shell that forks `sleep` (dash does) would leave it
            // running, holding the hold, where no PID namespace kills it with the
            // shell (macOS, Linux without user namespaces).
            .args(["-c", "exec sleep 30"])
            .spawn()
            .unwrap();
    let busy = mb.commit(&mut w, "noop");
    // Stop it before any assertion can fail, so no process outlives the test.
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        matches!(
            &busy,
            Err(MergeError::Workspace(WorkspaceError::Busy { .. }))
        ),
        "{busy:?}"
    );
    assert_eq!(w.state(), &AttemptState::Working, "not final yet");
    assert!(w.workspace().root().is_dir());
    assert_eq!(mb.commit(&mut w, "noop").unwrap(), CommitOutcome::Empty);
    assert!(!w.workspace().root().exists());
}

/// Decision 0088: with worktrees kept, a merged attempt's worktree and the
/// build output in it and in its scratch directory stay on disk; the work is
/// on the branch and the attempt ref is gone. The default removes them.
#[test]
fn a_merged_attempt_keeps_its_worktree_and_builds_only_when_asked() {
    for after in [MergedWorktree::Keep, MergedWorktree::Remove] {
        let e = env();
        let mb = merger(&e);
        let mut w = attempt(&e, "built");
        write(&w, "README", "merged work\n");
        let root = w.workspace().root().to_path_buf();
        let in_scratch = w.workspace().scratch().join("target/debug/blob");
        std::fs::create_dir_all(in_scratch.parent().unwrap()).unwrap();
        std::fs::write(&in_scratch, [7u8; 64]).unwrap();
        mb.commit_declared(&mut w, "work", &["README"]).unwrap();
        let outcome = mb.merge_then(&mut w, after).unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Merged { .. }),
            "{outcome:?}"
        );
        assert_eq!(show(&e.f.repo, &format!("{BRANCH}:README")), "merged work");
        assert!(!ref_exists(&e.f.repo, &w.attempt_ref()), "{after:?}");
        let kept = after == MergedWorktree::Keep;
        assert_eq!(root.join("README").is_file(), kept, "{after:?}");
        assert_eq!(in_scratch.is_file(), kept, "{after:?}");
        if kept {
            // Still a worktree git knows, so it can be removed later (by
            // `kgraf clean` once this process has ended).
            assert!(
                git(&e.f.repo, &["worktree", "list", "--porcelain"]).contains("built"),
                "{after:?}"
            );
            e.mgr.remove(w.workspace().clone()).unwrap();
            assert!(!root.exists() && !in_scratch.exists());
        }
    }
}

/// The snapshot a check runs after: `commit_retaining` keeps an empty
/// attempt's worktree; what is written after the commit is listed against the
/// snapshot, never merged, and removed by `discard_leftovers`.
#[test]
fn what_is_written_after_the_snapshot_is_listed_removed_and_never_merged() {
    let e = env();
    let mb = merger(&e);
    let none = |_: &Path| false;
    let mut idle = attempt(&e, "idle-kept");
    assert!(matches!(
        mb.discard_leftovers(&idle).unwrap_err(),
        MergeError::InvalidTransition { .. }
    ));
    assert_eq!(
        mb.commit_retaining(&mut idle, "noop", &none).unwrap(),
        CommitOutcome::Empty
    );
    assert!(idle.workspace().root().is_dir(), "kept for the check");
    assert!(mb.discard_leftovers(&idle).unwrap().is_empty());

    let mut w = attempt(&e, "snap");
    write(&w, "new.txt", "worker\n");
    // The worker declared `new.txt`: merge refuses anything else (0083).
    let new_only = |p: &Path| p == Path::new("new.txt");
    let CommitOutcome::Committed { commit } =
        mb.commit_retaining(&mut w, "snap", &new_only).unwrap()
    else {
        panic!("committed");
    };
    let before = tip(&e.f.repo);
    write(&w, "Cargo.lock", "generated\n");
    write(&w, "new.txt", "rewritten by the check\n");
    let left = mb.discard_leftovers(&w).unwrap();
    assert_eq!(left.created, vec![std::path::PathBuf::from("Cargo.lock")]);
    assert_eq!(left.changed, vec![std::path::PathBuf::from("new.txt")]);
    assert!(!w.workspace().root().join("Cargo.lock").exists());
    assert_eq!(
        std::fs::read_to_string(w.workspace().root().join("new.txt")).unwrap(),
        "worker\n"
    );
    // A rewrite with the same content (new stat, same bytes) is no leftover.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(&w, "new.txt", "worker\n");
    assert!(mb.discard_leftovers(&w).unwrap().is_empty());
    assert_eq!(tip(&e.f.repo), before, "listing and cleanup never merge");
    assert!(matches!(
        mb.merge(&mut w).unwrap(),
        MergeOutcome::Merged { .. }
    ));
    assert_eq!(show(&e.f.repo, &format!("{BRANCH}:new.txt")), "worker");
    let tree = git(&e.f.repo, &["ls-tree", "--name-only", &commit]);
    assert!(!tree.lines().any(|p| p == "Cargo.lock"), "{tree}");
}

/// A plain `commit` (the worker's own segments, `select`) never leaves
/// anything out; `commit_retaining` leaves out a new, out-of-scope lockfile
/// with its manifest beside it and bytecode, and records them.
#[test]
fn only_commit_retaining_leaves_generated_files_out() {
    let e = env();
    let mb = merger(&e);
    let none = |_: &Path| false;
    let mut w = attempt(&e, "gen");
    write(&w, "Cargo.toml", "[package]\n");
    write(&w, "Cargo.lock", "lock\n");
    write(&w, "p/__pycache__/m.cpython-39.pyc", "b");
    let CommitOutcome::Committed { commit } = mb.commit_retaining(&mut w, "gen", &none).unwrap()
    else {
        panic!("committed");
    };
    let tree = git(&e.f.repo, &["ls-tree", "-r", "--name-only", &commit]);
    assert!(tree.lines().any(|p| p == "Cargo.toml"), "{tree}");
    assert!(
        !tree.contains("Cargo.lock") && !tree.contains("__pycache__"),
        "{tree}"
    );
    let kinds: Vec<_> = w
        .excluded()
        .iter()
        .map(|x| (x.path.clone(), x.kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (
                std::path::PathBuf::from("Cargo.lock"),
                GeneratedKind::Lockfile
            ),
            (
                std::path::PathBuf::from("p/__pycache__/m.cpython-39.pyc"),
                GeneratedKind::PythonBytecode
            ),
        ]
    );
    mb.remove_excluded(&w).unwrap();
    assert!(!w.workspace().root().join("p").exists());
    assert!(
        !w.workspace().root().join("Cargo.lock").exists(),
        "a check never runs against the worker's lockfile"
    );
    assert!(w.workspace().root().join("Cargo.toml").is_file());

    let mut plain = attempt(&e, "plain");
    write(&plain, "Cargo.toml", "[package]\n");
    write(&plain, "Cargo.lock", "lock\n");
    let CommitOutcome::Committed { commit } = mb.commit(&mut plain, "plain").unwrap() else {
        panic!("committed");
    };
    let tree = git(&e.f.repo, &["ls-tree", "-r", "--name-only", &commit]);
    assert!(tree.lines().any(|p| p == "Cargo.lock"), "{tree}");
}

#[test]
fn merging_before_commit_is_an_illegal_transition() {
    let e = env();
    let mb = merger(&e);
    let mut w = attempt(&e, "w");
    write(&w, "x", "x\n");
    let err = mb.merge(&mut w).unwrap_err();
    assert!(matches!(err, MergeError::InvalidTransition { .. }), "{err}");
    assert!(matches!(w.state(), AttemptState::Working));
}

#[test]
fn a_checked_out_integration_branch_is_refused() {
    let e = env();
    // `main` is checked out in the fixture repository.
    let mb = MergeBack::open(e.mgr.clone(), "main", identity()).unwrap();
    let mut w = attempt(&e, "w");
    write(&w, "x", "x\n");
    mb.commit_declared(&mut w, "x", &["x"]).unwrap();
    let before = git(&e.f.repo, &["rev-parse", "main"]);
    let err = mb.merge(&mut w).unwrap_err();
    assert!(
        matches!(err, MergeError::IntegrationCheckedOut { .. }),
        "{err}"
    );
    assert_eq!(git(&e.f.repo, &["rev-parse", "main"]), before);
    assert!(matches!(w.state(), AttemptState::Committed { .. }));
}

#[test]
fn a_missing_integration_branch_fails_at_open() {
    let e = env();
    let err = MergeBack::open(e.mgr.clone(), "nope", identity())
        .err()
        .unwrap();
    assert!(
        matches!(err, MergeError::NoIntegrationBranch { .. }),
        "{err}"
    );
}

#[test]
fn concurrent_finishes_all_land_one_merge_each() {
    let e = env();
    let mb = Arc::new(merger(&e));
    let attempts: Vec<_> = (0..4)
        .map(|i| {
            let a = attempt(&e, &format!("c{i}"));
            write(&a, &format!("f{i}.txt"), &format!("{i}\n"));
            (a, format!("f{i}.txt"))
        })
        .collect();
    let handles: Vec<_> = attempts
        .into_iter()
        .map(|(mut a, file)| {
            let mb = mb.clone();
            std::thread::spawn(move || {
                let out = mb.finish(&mut a, "c", &[file]).unwrap();
                assert!(matches!(out, FinishOutcome::Merged { .. }), "{out:?}");
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    for i in 0..4 {
        assert_eq!(
            show(&e.f.repo, &format!("{BRANCH}:f{i}.txt")),
            i.to_string()
        );
    }
    let merges = git(
        &e.f.repo,
        &["rev-list", "--first-parent", "--merges", BRANCH],
    );
    assert_eq!(merges.lines().count(), 4);
}

/// Wrap the real git in a script that logs each invocation, so "no git
/// command ran" is observed, not inferred.
#[cfg(unix)]
fn logging_git(dir: &Path) -> (Git, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let real = Git::discover().unwrap().exe().to_path_buf();
    let log = dir.join("git-invocations.log");
    let script = dir.join("git-logged");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (Git::at(script), log)
}

#[cfg(unix)]
#[test]
fn a_tampered_gitlink_is_refused_before_any_git_command() {
    let e = env();
    let (logged, log) = logging_git(e.f.tmp.path());
    let mb =
        MergeBack::open_with(e.mgr.clone(), BRANCH, identity(), Limits::default(), logged).unwrap();
    assert!(
        std::fs::read_to_string(&log).unwrap().contains("version"),
        "the wrapper is live"
    );

    // A decoy repository the worker would like host-side git to act on.
    let decoy = common::init_repo(&e.f.tmp.path().join("decoy"));
    let decoy_head = git(&decoy, &["rev-parse", "HEAD"]);
    let gitlink = format!("gitdir: {}\n", decoy.join(".git").display());

    // Commit path.
    let mut w = attempt(&e, "w");
    write(&w, "x", "x\n");
    std::fs::write(w.workspace().root().join(".git"), &gitlink).unwrap();
    std::fs::write(&log, "").unwrap();
    let err = mb.commit(&mut w, "x").unwrap_err();
    assert!(
        matches!(
            err,
            MergeError::Workspace(WorkspaceError::GitlinkTampered { .. })
        ),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "", "git ran");
    assert!(matches!(w.state(), AttemptState::Working));

    // Merge path: committed honestly, then tampered before the merge.
    let mut v = attempt(&e, "v");
    write(&v, "y", "y\n");
    mb.commit_declared(&mut v, "y", &["y"]).unwrap();
    let before = tip(&e.f.repo);
    std::fs::write(v.workspace().root().join(".git"), &gitlink).unwrap();
    std::fs::write(&log, "").unwrap();
    let err = mb.merge(&mut v).unwrap_err();
    assert!(
        matches!(
            err,
            MergeError::Workspace(WorkspaceError::GitlinkTampered { .. })
        ),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "", "git ran");
    assert_eq!(tip(&e.f.repo), before);
    assert_eq!(git(&decoy, &["rev-parse", "HEAD"]), decoy_head);
    assert_eq!(git(&decoy, &["status", "--porcelain"]), "");
}

#[cfg(unix)]
fn hook(path: &Path, sentinel: &Path, name: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\necho {name} >> '{}'\nexit 0\n",
            sentinel.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn hooks_and_fsmonitor_do_not_run_during_merge_back() {
    const HOOKS: &[&str] = &[
        "pre-commit",
        "prepare-commit-msg",
        "commit-msg",
        "post-commit",
        "pre-merge-commit",
        "post-merge",
        "post-checkout",
        "post-index-change",
        "reference-transaction",
    ];
    let e = env();
    let sentinel = e.f.tmp.path().join("hook-ran.log");
    // The repository resolves hooks relative to each worktree (this project's
    // own layout: core.hooksPath=.githooks), so a worker that can write its
    // root can plant hooks. Also plant the repository-wide hooks directory and
    // an fsmonitor hook, which git would run on any index refresh.
    git(&e.f.repo, &["config", "core.hooksPath", ".githooks"]);
    // Ignored, so the planted hooks are not staged (staging a `.githooks`
    // path would refuse the attempt before the merge step is exercised).
    std::fs::write(e.f.repo.join(".git/info/exclude"), ".githooks/\n").unwrap();
    let fsmonitor = e.f.tmp.path().join("fsmonitor.sh");
    hook(&fsmonitor, &sentinel, "fsmonitor");
    git(
        &e.f.repo,
        &["config", "core.fsmonitor", fsmonitor.to_str().unwrap()],
    );
    let common_hooks = e.f.repo.join(".git").join("hooks");
    for name in HOOKS {
        hook(&common_hooks.join(name), &sentinel, name);
    }

    let mut w = attempt(&e, "w");
    for name in HOOKS {
        hook(
            &w.workspace().root().join(".githooks").join(name),
            &sentinel,
            name,
        );
    }
    write(&w, "work.txt", "done\n");

    // Control: the planted hooks are live for an ordinary git command in a
    // worktree set up the same way.
    let ctl = attempt(&e, "ctl");
    for name in HOOKS {
        hook(
            &ctl.workspace().root().join(".githooks").join(name),
            &sentinel,
            name,
        );
    }
    git(
        ctl.workspace().root(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "control",
        ],
    );
    let control = std::fs::read_to_string(&sentinel).unwrap();
    assert!(
        control.contains("pre-commit") && control.contains("fsmonitor"),
        "control run fired: {control}"
    );
    std::fs::remove_file(&sentinel).unwrap();

    let mb = merger(&e);
    let out = mb.finish(&mut w, "w", &["work.txt"]).unwrap();
    assert!(matches!(out, FinishOutcome::Merged { .. }), "{out:?}");
    assert_eq!(show(&e.f.repo, &format!("{BRANCH}:work.txt")), "done");
    assert!(
        !sentinel.exists(),
        "a hook ran during merge-back: {}",
        std::fs::read_to_string(&sentinel).unwrap_or_default()
    );
}

// ---- Regression tests for merge-back. ----

fn refusal_of(out: FinishOutcome) -> Vec<RefusalReason> {
    match out {
        FinishOutcome::Refused(r) => r.reasons,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A filter named by worker-written `.gitattributes` must
/// never run on the host, not while staging, not while hashing the tree.
#[test]
fn worker_attributes_never_run_a_filter_on_the_host() {
    let e = env();
    let marker = e.f.tmp.path().join("FILTER_RAN");
    // A filter the user defines in config (git-lfs, git-crypt, ...).
    git(
        &e.f.repo,
        &[
            "config",
            "filter.evil.clean",
            &format!("echo ran >> '{}'; cat", marker.display()),
        ],
    );
    // Control: plain git in a worktree with the planted attributes runs it.
    let ctl = attempt(&e, "ctl");
    write(&ctl, ".gitattributes", "* filter=evil\n");
    write(&ctl, "x.txt", "x\n");
    git(ctl.workspace().root(), &["add", "x.txt"]);
    assert!(marker.exists(), "control: the filter is live");
    std::fs::remove_file(&marker).unwrap();

    let mb = merger(&e);
    let before = tip(&e.f.repo);
    let mut a = attempt(&e, "a");
    write(&a, ".gitattributes", "* filter=evil\n");
    write(&a, "x.txt", "x\n");
    let reasons = refusal_of(
        mb.finish(&mut a, "a", &[".gitattributes", "x.txt"])
            .unwrap(),
    );
    assert_eq!(
        reasons,
        [RefusalReason::ProtectedPath {
            path: ".gitattributes".into()
        }]
    );
    assert!(!marker.exists(), "filter ran on the host");
    assert_eq!(tip(&e.f.repo), before);
}

/// Paths that steer git are refused, never merged; the worktree
/// is kept for inspection and nothing reaches the branch.
#[test]
fn attempts_touching_git_control_files_are_refused() {
    let e = env();
    let mb = merger(&e);
    let before = tip(&e.f.repo);
    let mut a = attempt(&e, "a");
    write(&a, "ok.txt", "fine\n");
    write(&a, ".gitmodules", "[submodule \"x\"]\n");
    write(&a, "sub/.gitattributes", "* merge=union\n");
    write(&a, ".githooks/pre-commit", "#!/bin/sh\n");
    write(&a, ".gitignore", "target/\nextra/\n");
    write(&a, ".github/workflows/ci.yml", "on: push\n");
    let declared = [
        "ok.txt",
        ".gitmodules",
        "sub/.gitattributes",
        ".githooks/pre-commit",
        ".gitignore",
        ".github/workflows/ci.yml",
    ];
    let reasons = refusal_of(mb.finish(&mut a, "a", &declared).unwrap());
    let mut paths: Vec<_> = reasons
        .iter()
        .map(|r| match r {
            RefusalReason::ProtectedPath { path } => path.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        [".githooks/pre-commit", ".gitmodules", "sub/.gitattributes"]
    );
    assert_eq!(tip(&e.f.repo), before);
    assert!(matches!(a.state(), AttemptState::Refused(_)));
    assert!(!ref_exists(&e.f.repo, &a.attempt_ref()));
    assert!(a.workspace().root().join("ok.txt").is_file());
    mb.discard(&mut a).unwrap();
    assert!(!a.workspace().root().exists());
}

/// Decision 0062: a worker that writes a state directory (under either name,
/// in any spelling) is refused at merge-back. Merged, `.haustoria/` in the
/// checkout would make the next kGraf start refuse the project ("both state
/// directories exist").
#[test]
fn attempts_writing_a_state_directory_are_refused() {
    let e = env();
    let mb = merger(&e);
    let before = tip(&e.f.repo);
    let mut a = attempt(&e, "a");
    write(&a, "ok.txt", "fine\n");
    write(&a, ".haustoria/x", "planted\n");
    write(&a, "sub/.Haustoria/y", "planted\n");
    write(&a, ".kgraf/allow-rules.json", "{}\n");
    // The migration lock, as a regular file and as a link.
    write(&a, ".kgraf.migrating", "planted mark\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        "../ok.txt",
        a.workspace().root().join("sub/.KGRAF.MIGRATING"),
    )
    .unwrap();
    let declared = [
        "ok.txt",
        ".haustoria/x",
        "sub/.Haustoria/y",
        ".kgraf/allow-rules.json",
        ".kgraf.migrating",
        "sub/.KGRAF.MIGRATING",
    ];
    let reasons = refusal_of(mb.finish(&mut a, "a", &declared).unwrap());
    let mut paths: Vec<_> = reasons
        .iter()
        .filter_map(|r| match r {
            RefusalReason::ProtectedPath { path } => Some(path.as_str()),
            // The link is also refused as a link (decision 0083).
            RefusalReason::Symlink { path, .. } if path == "sub/.KGRAF.MIGRATING" => None,
            other => panic!("{other:?}"),
        })
        .collect();
    paths.sort_unstable();
    #[allow(unused_mut)]
    let mut expected = vec![
        ".haustoria/x",
        ".kgraf.migrating",
        ".kgraf/allow-rules.json",
        "sub/.Haustoria/y",
    ];
    #[cfg(unix)]
    expected.push("sub/.KGRAF.MIGRATING");
    assert_eq!(paths, expected);
    assert_eq!(tip(&e.f.repo), before);
    assert!(!ref_exists(&e.f.repo, &a.attempt_ref()));
    mb.discard(&mut a).unwrap();
}

/// A nested repository would become a gitlink to an unreachable
/// commit and its files would vanish under outcome Merged.
#[test]
fn a_nested_repository_is_refused_naming_its_path() {
    let e = env();
    let mb = merger(&e);
    let before = tip(&e.f.repo);
    let mut a = attempt(&e, "a");
    write(&a, "top.txt", "t\n");
    let sub = a.workspace().root().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-q"]);
    std::fs::write(sub.join("work.rs"), "important\n").unwrap();
    git(&sub, &["add", "."]);
    git(
        &sub,
        &[
            "-c",
            "user.name=x",
            "-c",
            "user.email=x@x",
            "commit",
            "-qm",
            "x",
        ],
    );
    let pwn = e.f.tmp.path().join("PWNED");
    git(
        &sub,
        &[
            "config",
            "core.fsmonitor",
            &format!("touch {}", pwn.display()),
        ],
    );
    let reasons = refusal_of(mb.finish(&mut a, "a", &["top.txt", "sub"]).unwrap());
    assert_eq!(
        reasons,
        [RefusalReason::NestedRepository { path: "sub".into() }]
    );
    assert!(!pwn.exists(), "nested repository's fsmonitor ran");
    assert_eq!(tip(&e.f.repo), before);
}

/// A `.git` file in a subdirectory.
#[test]
fn a_gitlink_file_in_a_subdirectory_does_not_merge_silently() {
    let e = env();
    let mb = merger(&e);
    let before = tip(&e.f.repo);
    let mut a = attempt(&e, "a");
    write(&a, "sub/code.rs", "important\n");
    let other = common::init_repo(&e.f.tmp.path().join("other"));
    write(
        &a,
        "sub/.git",
        &format!("gitdir: {}\n", other.join(".git").display()),
    );
    // Either a refusal or an error; never Merged with the files gone.
    match mb.finish(&mut a, "a", &["sub/code.rs", "sub/.git", "sub"]) {
        Ok(FinishOutcome::Refused(r)) => assert!(
            r.reasons
                .iter()
                .any(|x| matches!(x, RefusalReason::NestedRepository { path } if path == "sub")),
            "{r:?}"
        ),
        Ok(other) => panic!("{other:?}"),
        Err(err) => assert!(matches!(err, MergeError::Workspace(_)), "{err}"),
    }
    assert_eq!(tip(&e.f.repo), before);
}

/// Paths differing only by case merge cleanly in git but collide
/// on macOS and Windows file systems.
#[test]
fn case_only_collisions_are_a_conflict() {
    let e = env();
    let mb = merger(&e);
    let mut a = attempt(&e, "a");
    let mut b = attempt(&e, "b");
    write(&a, "Foo.txt", "A\n");
    write(&b, "foo.txt", "B\n");
    assert!(matches!(
        mb.finish(&mut a, "a", &["Foo.txt"]).unwrap(),
        FinishOutcome::Merged { .. }
    ));
    let before = tip(&e.f.repo);
    let FinishOutcome::Conflicted(c) = mb.finish(&mut b, "b", &["foo.txt"]).unwrap() else {
        panic!("case collision merged")
    };
    assert_eq!(c.collisions, [["Foo.txt", "foo.txt"]]);
    assert!(c.paths.is_empty());
    assert!(
        c.messages
            .iter()
            .any(|m| m.kind == "CONFLICT (path collision)")
    );
    assert_eq!(tip(&e.f.repo), before);
    assert_eq!(
        git(
            &e.f.repo,
            &["ls-tree", "--name-only", BRANCH, "--", "foo.txt"]
        ),
        ""
    );
}

/// NFC and NFD spellings of one name.
#[test]
fn normalization_only_collisions_are_a_conflict() {
    let e = env();
    let mb = merger(&e);
    let mut a = attempt(&e, "a");
    let mut b = attempt(&e, "b");
    write(&a, "caf\u{e9}.txt", "A\n");
    write(&b, "cafe\u{301}.txt", "B\n");
    mb.finish(&mut a, "a", &["caf\u{e9}.txt"]).unwrap();
    let before = tip(&e.f.repo);
    // git on macOS may precompose the name (a content conflict); elsewhere it
    // keeps both spellings (a collision). Both must come back as conflicts.
    let out = mb.finish(&mut b, "b", &["cafe\u{301}.txt"]).unwrap();
    assert!(matches!(out, FinishOutcome::Conflicted(_)), "{out:?}");
    assert_eq!(tip(&e.f.repo), before);
}

/// Delete and file/directory conflicts are typed.
#[test]
fn delete_and_file_directory_conflicts_are_typed() {
    let e = env();
    let mb = merger(&e);
    let mut s = attempt(&e, "seed");
    write(&s, "r.txt", "line1\nline2\nline3\nline4\nline5\n");
    mb.finish(&mut s, "seed", &["r.txt"]).unwrap();
    let mut i = attempt(&e, "integ");
    let mut w = attempt(&e, "w");
    std::fs::remove_file(i.workspace().root().join("r.txt")).unwrap();
    write(&i, "d", "file\n");
    std::fs::remove_file(w.workspace().root().join("r.txt")).unwrap();
    write(&w, "q.txt", "line1\nline2\nline3\nline4\nline5\nline6\n");
    write(&w, "d/x", "dir\n");
    mb.finish(&mut i, "i", &["r.txt", "d"]).unwrap();
    let before = tip(&e.f.repo);
    let out = mb.finish(&mut w, "w", &["r.txt", "q.txt", "d/x"]).unwrap();
    let FinishOutcome::Conflicted(c) = out else {
        panic!("{out:?}")
    };
    assert!(c.paths.iter().any(|p| p.path.starts_with('d')), "{c:?}");
    assert_eq!(tip(&e.f.repo), before);
    assert_eq!(show(&e.f.repo, &format!("{BRANCH}:d")), "file");
}

/// A FIFO is reported, not skipped silently.
#[cfg(unix)]
#[test]
fn a_fifo_is_reported_not_skipped() {
    let e = env();
    let mb = merger(&e);
    let mut a = attempt(&e, "a");
    write(&a, "ok.txt", "ok\n");
    let st = std::process::Command::new("mkfifo")
        .arg(a.workspace().root().join("pipe"))
        .status()
        .unwrap();
    assert!(st.success());
    let reasons = refusal_of(mb.finish(&mut a, "a", NONE).unwrap());
    assert_eq!(
        reasons,
        [RefusalReason::NotARegularFile {
            path: "pipe".into(),
            kind: "fifo"
        }]
    );
}

/// Oversized files refuse the attempt before staging; ignored
/// files are not counted.
#[test]
fn oversized_files_are_refused_and_ignored_ones_are_not_counted() {
    let e = env();
    let limits = Limits {
        max_file_bytes: 10,
        ..Limits::default()
    };
    let mb = MergeBack::open_with(
        e.mgr.clone(),
        BRANCH,
        identity(),
        limits,
        Git::discover().unwrap(),
    )
    .unwrap();
    let mut a = attempt(&e, "a");
    write(&a, "small.txt", "tiny\n");
    write(&a, "big.bin", "0123456789ABCDEF");
    write(&a, "target/huge.o", &"x".repeat(1000));
    let reasons = refusal_of(mb.finish(&mut a, "a", NONE).unwrap());
    assert_eq!(
        reasons,
        [RefusalReason::FileTooLarge {
            path: "big.bin".into(),
            bytes: 16,
            limit: 10
        }]
    );
    // Nothing was staged: the index still matches the base.
    assert_eq!(
        git(a.workspace().root(), &["diff", "--cached", "--name-only"]),
        ""
    );
}

/// Many files each under the per-file limit are refused together when their
/// total is over the total limit (a build directory the repository does not
/// ignore, for one), naming the largest; nothing is staged.
#[test]
fn an_attempt_over_the_total_byte_limit_is_refused_naming_its_largest_files() {
    let e = env();
    let limits = Limits {
        max_file_bytes: 100,
        max_total_bytes: 250,
        ..Limits::default()
    };
    let mb = MergeBack::open_with(
        e.mgr.clone(),
        BRANCH,
        identity(),
        limits,
        Git::discover().unwrap(),
    )
    .unwrap();
    let mut a = attempt(&e, "a");
    for i in 0..4 {
        write(&a, &format!("out/part{i}.rlib"), &"x".repeat(90 + i));
    }
    let reasons = refusal_of(mb.finish(&mut a, "a", NONE).unwrap());
    assert_eq!(
        reasons,
        [RefusalReason::TooMuchData {
            bytes: 90 + 91 + 92 + 93,
            limit: 250,
            files: 4,
            largest: vec![
                "out/part3.rlib".into(),
                "out/part2.rlib".into(),
                "out/part1.rlib".into(),
                "out/part0.rlib".into(),
            ],
        }]
    );
    assert_eq!(
        git(a.workspace().root(), &["diff", "--cached", "--name-only"]),
        ""
    );
}

/// Every git child is bounded in time.
#[cfg(unix)]
#[test]
fn a_hung_git_is_killed_at_the_timeout() {
    use std::os::unix::fs::PermissionsExt;
    let e = env();
    let real = Git::discover().unwrap().exe().to_path_buf();
    let script = e.f.tmp.path().join("git-slow");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = write-tree ] && exec sleep 30; done\nexec '{}' \"$@\"\n",
            real.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let limits = Limits {
        git_timeout: std::time::Duration::from_secs(2),
        ..Limits::default()
    };
    let mb =
        MergeBack::open_with(e.mgr.clone(), BRANCH, identity(), limits, Git::at(script)).unwrap();
    let mut a = attempt(&e, "a");
    write(&a, "x", "x\n");
    let started = std::time::Instant::now();
    let err = mb.finish(&mut a, "a", &["x"]).unwrap_err();
    assert!(matches!(err, MergeError::GitTimeout { .. }), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert!(matches!(a.state(), AttemptState::Working));
}

/// An attempt name must form a valid ref.
#[test]
fn attempt_names_that_are_not_valid_refs_are_refused() {
    let e = env();
    let mb = merger(&e);
    for name in ["x.lock", "a..b", "trail."] {
        let ws = e
            .mgr
            .create(&WorkspaceSpec {
                name,
                base: BRANCH,
                branch: None,
            })
            .unwrap();
        let err = mb.attempt(ws.clone()).unwrap_err();
        assert!(
            matches!(err, MergeError::InvalidAttemptName { .. }),
            "{name}: {err}"
        );
        e.mgr.remove(ws).unwrap();
    }
}

/// The winner is committed before any loser is discarded, and a
/// failure returns every attempt not yet discarded.
#[test]
fn select_commits_the_winner_before_discarding_losers() {
    let e = env();
    let mb = merger(&e);
    let w = attempt(&e, "win");
    let l = attempt(&e, "lose");
    write(&w, "a", "1\n");
    write(&l, "b", "2\n");
    // Make the winner's commit fail: occupy its attempt ref.
    let t = tip(&e.f.repo);
    git(&e.f.repo, &["update-ref", "refs/kgraf/attempts/win", &t]);
    let lose_root = l.workspace().root().to_path_buf();
    let win_root = w.workspace().root().to_path_buf();
    let group = AttemptGroup::new("t", vec![w, l]).unwrap();
    let failure = mb.select(group, "win", "m", &["a"]).unwrap_err();
    assert!(
        matches!(failure.error, MergeError::AttemptRefExists(_)),
        "{failure}"
    );
    assert!(
        lose_root.exists(),
        "loser discarded before the winner committed"
    );
    assert!(win_root.exists());
    let mut names: Vec<_> = failure
        .attempts
        .iter()
        .map(|a| a.name().to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["lose", "win"]);
    assert_eq!(tip(&e.f.repo), t);
}

/// The commit author is always the user; a Claude identity is refused.
#[test]
fn a_claude_identity_is_refused_at_open() {
    let e = env();
    for (name, email) in [
        ("Claude", "noreply@anthropic.com"),
        ("Someone", "claude@example.com"),
        ("", "x@y"),
    ] {
        let err = MergeBack::open(
            e.mgr.clone(),
            BRANCH,
            Identity {
                name: name.into(),
                email: email.into(),
            },
        )
        .err()
        .unwrap();
        assert!(matches!(err, MergeError::IdentityRefused(_)), "{err}");
    }
}

#[test]
fn changed_paths_names_everything_a_committed_attempt_changed_against_its_base() {
    let e = env();
    let mb = merger(&e);
    let mut w = attempt(&e, "scope");
    // Before a commit there is nothing to read.
    assert!(matches!(
        mb.changed_paths(&w),
        Err(MergeError::InvalidTransition { .. })
    ));
    write(&w, "src/a.rs", "a\n");
    write(&w, "stray.txt", "b\n");
    std::fs::remove_file(w.workspace().root().join("README")).unwrap();
    let out = mb.commit(&mut w, "scope").unwrap();
    assert!(matches!(
        out,
        kgraf_sandbox::CommitOutcome::Committed { .. }
    ));
    let mut paths = mb.changed_paths(&w).unwrap();
    paths.sort();
    assert_eq!(paths, ["README", "src/a.rs", "stray.txt"]);
}

// ---- Decision 0083: links and declared writes. ----

#[cfg(unix)]
fn links_refused(reasons: &[RefusalReason]) -> Vec<(String, String)> {
    let mut out: Vec<_> = reasons
        .iter()
        .filter_map(|r| match r {
            RefusalReason::Symlink { path, target } => Some((path.clone(), target.clone())),
            _ => None,
        })
        .collect();
    out.sort();
    out
}

/// A worker commits a directory link and a leaf link, both aimed out of the
/// tree at a temp directory. Merge-back refuses the attempt, naming each link
/// and its target, even though both were declared writes; nothing reaches
/// the branch, so no later checkout of it can steer a host write outside.
#[cfg(unix)]
#[test]
fn a_worker_link_is_refused_naming_it_and_its_target() {
    let e = env();
    let mb = merger(&e);
    let before = tip(&e.f.repo);
    let outside = e.f.tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let mut a = attempt(&e, "a");
    let root = a.workspace().root().to_path_buf();
    write(&a, "ok.txt", "fine\n");
    std::os::unix::fs::symlink("../../outside", root.join("gen")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let leaf = outside.join("rc");
    std::os::unix::fs::symlink(&leaf, root.join("src/config.rs")).unwrap();
    // A link whose target reads as inside is refused too: a chain of such
    // links can still leave the tree.
    std::os::unix::fs::symlink("ok.txt", root.join("alias.txt")).unwrap();
    let declared = ["ok.txt", "gen", "src/config.rs", "alias.txt"];
    let reasons = refusal_of(mb.finish(&mut a, "a", &declared).unwrap());
    assert_eq!(
        links_refused(&reasons),
        [
            ("alias.txt".to_owned(), "ok.txt".to_owned()),
            ("gen".to_owned(), "../../outside".to_owned()),
            (
                "src/config.rs".to_owned(),
                leaf.to_string_lossy().into_owned()
            ),
        ]
    );
    assert_eq!(reasons.len(), 3, "{reasons:?}");
    assert_eq!(tip(&e.f.repo), before);
    assert!(!ref_exists(&e.f.repo, &a.attempt_ref()));
}

/// A link the base already holds may stay as it is or be deleted; it may not
/// be retargeted, and a file may not become one.
#[cfg(unix)]
#[test]
fn a_link_on_the_base_may_stay_or_go_but_never_be_retargeted() {
    let e = env();
    std::os::unix::fs::symlink("README", e.f.repo.join("link")).unwrap();
    git(&e.f.repo, &["add", "link"]);
    git(
        &e.f.repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "a link",
        ],
    );
    git(&e.f.repo, &["branch", "-f", BRANCH, "main"]);
    let mb = merger(&e);

    let mut keep = attempt(&e, "keep");
    write(&keep, "a.txt", "a\n");
    assert!(matches!(
        mb.finish(&mut keep, "keep", &["a.txt"]).unwrap(),
        FinishOutcome::Merged { .. }
    ));

    let mut retarget = attempt(&e, "retarget");
    let root = retarget.workspace().root().to_path_buf();
    std::fs::remove_file(root.join("link")).unwrap();
    std::os::unix::fs::symlink("../../outside", root.join("link")).unwrap();
    let reasons = refusal_of(mb.finish(&mut retarget, "r", &["link"]).unwrap());
    assert_eq!(
        links_refused(&reasons),
        [("link".to_owned(), "../../outside".to_owned())]
    );

    let mut to_link = attempt(&e, "to-link");
    let root = to_link.workspace().root().to_path_buf();
    std::fs::remove_file(root.join("README")).unwrap();
    std::os::unix::fs::symlink("/", root.join("README")).unwrap();
    let reasons = refusal_of(mb.finish(&mut to_link, "b", &["README"]).unwrap());
    assert_eq!(
        links_refused(&reasons),
        [("README".to_owned(), "/".to_owned())]
    );

    let mut delete = attempt(&e, "delete");
    std::fs::remove_file(delete.workspace().root().join("link")).unwrap();
    assert!(matches!(
        mb.finish(&mut delete, "d", &["link"]).unwrap(),
        FinishOutcome::Merged { .. }
    ));
    assert!(!git(&e.f.repo, &["ls-tree", "--name-only", BRANCH]).contains("link"));
}

/// With no scope, a worker's commit could replace any
/// unprotected file on the branch (`Cargo.toml`, `build.rs`,
/// `.cargo/config.toml`, `.envrc`, `.github/workflows/*`). Now each path
/// outside the declared writes is refused by name, and an attempt that
/// declared none may change nothing.
#[test]
fn finish_refuses_every_path_outside_the_declared_writes() {
    let e = env();
    let mb = merger(&e);
    let before = tip(&e.f.repo);
    let mut a = attempt(&e, "a");
    write(&a, "docs/shared-notes/a--b.md", "# hi\n");
    for p in [
        "Cargo.toml",
        "build.rs",
        ".cargo/config.toml",
        ".envrc",
        ".github/workflows/ci.yml",
    ] {
        write(&a, p, "planted\n");
    }
    let reasons = refusal_of(
        mb.finish(&mut a, "a", &["docs/shared-notes/a--b.md"])
            .unwrap(),
    );
    let mut outside: Vec<_> = reasons
        .iter()
        .map(|r| match r {
            RefusalReason::OutsideDeclaredWrites { path } => path.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    outside.sort_unstable();
    assert_eq!(
        outside,
        [
            ".cargo/config.toml",
            ".envrc",
            ".github/workflows/ci.yml",
            "Cargo.toml",
            "build.rs"
        ]
    );
    assert_eq!(tip(&e.f.repo), before);

    let mut b = attempt(&e, "b");
    write(&b, "README", "changed\n");
    let reasons = refusal_of(mb.finish(&mut b, "b", NONE).unwrap());
    assert_eq!(
        reasons,
        [RefusalReason::OutsideDeclaredWrites {
            path: "README".into()
        }]
    );
    assert_eq!(tip(&e.f.repo), before);

    // Declared paths compare as the file system resolves names.
    let mut c = attempt(&e, "c");
    write(&c, "README", "changed\n");
    assert!(matches!(
        mb.finish(&mut c, "c", &["readme"]).unwrap(),
        FinishOutcome::Merged { .. }
    ));
}

/// Two distinct paths a case-insensitive file system resolves as one are
/// refused at commit, named together: a declared `Foo.rs` would match both,
/// and in an APFS or NTFS checkout one overwrites the other.
#[test]
fn two_paths_the_file_system_folds_as_one_are_refused() {
    let e = env();
    let mb = merger(&e);
    let before = tip(&e.f.repo);
    let mut a = attempt(&e, "a");
    let root = a.workspace().root().to_path_buf();
    write(&a, "Foo.rs", "upper\n");
    write(&a, "foo.rs", "upper\n");
    // Both names in the index whatever the file system folds.
    let blob = git(&root, &["hash-object", "-w", "Foo.rs"]);
    for name in ["Foo.rs", "foo.rs"] {
        git(
            &root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{name}"),
            ],
        );
    }
    let reasons = refusal_of(mb.finish(&mut a, "a", &["Foo.rs"]).unwrap());
    assert!(
        reasons.contains(&RefusalReason::PathCollision {
            paths: vec!["Foo.rs".into(), "foo.rs".into()]
        }),
        "{reasons:?}"
    );
    assert_eq!(tip(&e.f.repo), before);
}

/// A new path that folds onto a path already in the base tree is refused at
/// commit, although only the new path is in the change list: in an APFS or
/// NTFS checkout it would overwrite the existing file. The long s, a new
/// directory beside an existing one, and a plain case twin are all caught.
#[test]
fn a_new_path_that_folds_onto_a_base_path_is_refused() {
    let e = env();
    let mb = merger(&e);
    let mut seed = attempt(&e, "seed");
    write(&seed, "Twin.rs", "one\n");
    write(&seed, "sample.rs", "two\n");
    write(&seed, "Dir/a.rs", "three\n");
    assert!(matches!(
        mb.finish(&mut seed, "seed", &["Twin.rs", "sample.rs", "Dir/a.rs"])
            .unwrap(),
        FinishOutcome::Merged { .. }
    ));
    let before = tip(&e.f.repo);
    for (existing, twin) in [
        ("Twin.rs", "twin.rs"),
        ("sample.rs", "\u{17F}ample.rs"),
        ("Dir", "dir"),
    ] {
        let new_path = if twin == "dir" { "dir/b.rs" } else { twin };
        let name = format!("twin-{}", existing.len());
        let mut a = attempt(&e, &name);
        let root = a.workspace().root().to_path_buf();
        write(&a, "unrelated.txt", "x\n");
        // On disk too, or staging everything would drop the index entry as
        // deleted (a folding volume resolves the name to the existing file).
        write(&a, new_path, "x\n");
        let blob = git(&root, &["hash-object", "-w", "unrelated.txt"]);
        // In the index whatever the file system folds (`ignorecase` off, or
        // git itself would fold the new name onto the existing entry).
        git(
            &root,
            &[
                "-c",
                "core.ignorecase=false",
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{new_path}"),
            ],
        );
        let reasons = refusal_of(
            mb.finish(&mut a, &name, &["unrelated.txt", new_path])
                .unwrap(),
        );
        assert!(
            reasons.iter().any(|r| matches!(
                r,
                RefusalReason::PathCollision { paths }
                    if paths.iter().any(|p| p == existing) && paths.iter().any(|p| p == twin)
            )),
            "{existing} / {twin}: {reasons:?}"
        );
        assert_eq!(tip(&e.f.repo), before);
    }
}

/// Merge itself holds the scope, whatever the caller checked: an attempt
/// committed with no scope, or committed with changes outside its scope, is
/// never merged and the branch does not move.
#[test]
fn merge_refuses_an_unscoped_or_out_of_scope_attempt() {
    let e = env();
    let mb = merger(&e);
    let before = tip(&e.f.repo);
    let mut plain = attempt(&e, "plain");
    write(&plain, "x.txt", "x\n");
    mb.commit(&mut plain, "plain").unwrap();
    let err = mb.merge(&mut plain).unwrap_err();
    assert!(matches!(err, MergeError::Unscoped { .. }), "{err}");

    let mut wide = attempt(&e, "wide");
    write(&wide, "x.txt", "x\n");
    write(&wide, "Cargo.toml", "[package]\n");
    let x_only = |p: &Path| p == Path::new("x.txt");
    assert!(matches!(
        mb.commit_retaining(&mut wide, "wide", &x_only).unwrap(),
        CommitOutcome::Committed { .. }
    ));
    let err = mb.merge(&mut wide).unwrap_err();
    let MergeError::OutsideDeclaredWrites { paths, .. } = &err else {
        panic!("{err}");
    };
    assert_eq!(paths, &["Cargo.toml".to_owned()]);
    assert_eq!(tip(&e.f.repo), before);
    mb.discard(&mut wide).unwrap();
}
