//! Speculative merge (decision 0074) against real git worktrees: two workers'
//! uncommitted trees, trial-merged onto the run's integration branch while
//! both are still in flight. A child of `tests.rs`, for its fixtures.

use super::*;
use kgraf_orchestrator::{ConflictKind, Round};

fn all_refs(root: &Path) -> String {
    git_out(root, &["for-each-ref", "--format=%(refname) %(objectname)"])
}

/// A prepared, unverified worktree for `id` with `files` written in it: a
/// worker still at work.
async fn in_flight(r: &Repo, id: &str, files: &[(&str, &str)]) -> PathBuf {
    let ws = r
        .workspaces
        .prepare(&named(files[0].0, id), &id.into())
        .await
        .expect("worktree");
    for (file, text) in files {
        std::fs::write(ws.join(file), text).expect("write");
    }
    ws
}

/// Verify then integrate the attempt in `ws` (contract `id` writing `file`).
async fn land(r: &Repo, id: &str, file: &str, ws: &Path) -> Integration {
    let c = named(file, id);
    let v = verifier(r, "true").verify(&c, ws, &result()).await;
    assert!(v.expect("ran").passed());
    r.workspaces.integrate(&c, ws).await.expect("integrate")
}

async fn speculate_pair(r: &Repo, runner: Option<&CommandGateRunner>, a: &Path, b: &Path) -> Round {
    let mut cache = BTreeMap::new();
    kgraf_orchestrator::speculate(
        &*r.workspaces,
        runner.map(|g| g as &dyn GateRunner),
        &[("a".into(), a.to_path_buf()), ("b".into(), b.to_path_buf())],
        &mut cache,
    )
    .await
    .expect("the round ran")
    .expect("git worktrees are versioned")
}

#[tokio::test]
async fn speculation_finds_a_textual_conflict_before_either_branch_lands_and_writes_nothing() {
    let r = repo();
    let wa = in_flight(&r, "w-a", &[("same.txt", "from a\n")]).await;
    let wb = in_flight(&r, "w-b", &[("same.txt", "from b\n")]).await;
    let tip = r.merge_back.integration_tip().expect("tip");
    let refs = all_refs(&r.root);
    let index = |ws: &Path| std::fs::read(git_out(ws, &["rev-parse", "--git-path", "index"])).ok();
    let (ia, ib) = (index(&wa), index(&wb));

    let round = speculate_pair(&r, None, &wa, &wb).await;
    let [(a, b, kind, detail)] = round.conflicts.as_slice() else {
        panic!("{:?}", round.conflicts);
    };
    assert_eq!((a.as_str(), b.as_str()), ("a", "b"));
    assert_eq!(*kind, ConflictKind::Textual);
    assert!(detail.contains("same.txt"), "{detail}");
    assert!(
        !round.validated,
        "no runner: the semantic class is not checked"
    );

    // Nothing was written: no ref at all (the integration branch included),
    // and each worker's index, files and HEAD are as they were.
    assert_eq!(r.merge_back.integration_tip().expect("tip"), tip);
    assert_eq!(all_refs(&r.root), refs);
    assert_eq!((index(&wa), index(&wb)), (ia, ib));
    assert_eq!(
        std::fs::read_to_string(wa.join("same.txt")).expect("read"),
        "from a\n"
    );
    assert_eq!(git_out(&wa, &["status", "--porcelain"]), "?? same.txt");

    // It was ahead of the fact: the first lands, and the second then meets
    // exactly that conflict at merge-back.
    assert!(matches!(
        land(&r, "w-a", "same.txt", &wa).await,
        Integration::Merged { .. }
    ));
    let second = land(&r, "w-b", "same.txt", &wb).await;
    assert!(
        matches!(&second, Integration::Conflict { paths, .. } if paths == &["same.txt"]),
        "{second:?}"
    );
}

/// A worker-written `.gitattributes` selecting a
/// filter the user configured never runs it on the host while speculation
/// snapshots the worktree; the round is refused, loudly, naming the path.
#[tokio::test]
async fn speculation_never_runs_a_filter_a_worker_attribute_selects() {
    let r = repo();
    let marker = r.outside.join("FILTER_RAN");
    host_git(
        &r.root,
        &[
            "config",
            "filter.evil.clean",
            &format!("echo ran >> '{}'; cat", marker.display()),
        ],
    );
    // Control: plain git in a worktree with the planted attributes runs it.
    let ctl = in_flight(&r, "w-ctl", &[(".gitattributes", "* filter=evil\n")]).await;
    std::fs::write(ctl.join("x.txt"), "x\n").expect("write");
    host_git(&ctl, &["add", "x.txt"]);
    assert!(marker.exists(), "control: the filter is live");
    std::fs::remove_file(&marker).expect("rm");

    let wa = in_flight(
        &r,
        "w-a",
        &[(".gitattributes", "* filter=evil\n"), ("x.txt", "x\n")],
    )
    .await;
    let wb = in_flight(&r, "w-b", &[("b.txt", "b\n")]).await;
    let refs = all_refs(&r.root);
    let mut cache = BTreeMap::new();
    let refused = kgraf_orchestrator::speculate(
        &*r.workspaces,
        None,
        &[("a".into(), wa), ("b".into(), wb)],
        &mut cache,
    )
    .await;
    let Err(why) = refused else {
        panic!("{refused:?}");
    };
    assert!(why.contains(".gitattributes"), "{why}");
    assert!(!marker.exists(), "the filter ran on the host");
    assert_eq!(all_refs(&r.root), refs);
}

#[tokio::test]
async fn speculation_finds_a_semantic_conflict_that_textual_merge_misses() {
    let r = repo();
    let wa = in_flight(&r, "w-a", &[("a.flag", "a\n")]).await;
    let wb = in_flight(&r, "w-b", &[("b.flag", "b\n")]).await;
    // Each alone passes; together they fail.
    let runner = gate_runner(
        &r,
        &["test ! -f a.flag || test ! -f b.flag"],
        Duration::from_secs(60),
    );
    let refs = all_refs(&r.root);
    let round = speculate_pair(&r, Some(&runner), &wa, &wb).await;
    assert!(round.validated);
    let [(a, b, kind, detail)] = round.conflicts.as_slice() else {
        panic!("{:?}", round.conflicts);
    };
    assert_eq!((a.as_str(), b.as_str()), ("a", "b"));
    assert_eq!(*kind, ConflictKind::Semantic);
    assert!(detail.contains("test ! -f a.flag"), "{detail}");
    assert_eq!(all_refs(&r.root), refs, "the validation runs wrote no ref");
    // Textual merge would not have caught it: both land cleanly.
    for (id, file, ws) in [("w-a", "a.flag", &wa), ("w-b", "b.flag", &wb)] {
        assert!(matches!(
            land(&r, id, file, ws).await,
            Integration::Merged { .. }
        ));
    }
}

#[tokio::test]
async fn speculation_never_blames_a_pair_for_a_failure_one_member_has_alone() {
    let r = repo();
    let wa = in_flight(&r, "w-a", &[("a.flag", "a\n")]).await;
    let wb = in_flight(&r, "w-b", &[("b.flag", "b\n")]).await;
    let runner = gate_runner(&r, &["test ! -f a.flag"], Duration::from_secs(60));
    let round = speculate_pair(&r, Some(&runner), &wa, &wb).await;
    assert!(round.validated);
    assert!(round.conflicts.is_empty(), "{:?}", round.conflicts);
}

#[tokio::test]
async fn speculation_finds_nothing_in_a_clean_pair_and_both_land() {
    let r = repo();
    let wa = in_flight(&r, "w-a", &[("a.txt", "a\n")]).await;
    let wb = in_flight(&r, "w-b", &[("b.txt", "b\n")]).await;
    let runner = gate_runner(&r, &["test -f README"], Duration::from_secs(60));
    let refs = all_refs(&r.root);
    let round = speculate_pair(&r, Some(&runner), &wa, &wb).await;
    assert!(round.validated);
    assert!(round.conflicts.is_empty(), "{:?}", round.conflicts);
    assert_eq!(all_refs(&r.root), refs);
    for (id, file, ws) in [("w-a", "a.txt", &wa), ("w-b", "b.txt", &wb)] {
        assert!(matches!(
            land(&r, id, file, ws).await,
            Integration::Merged { .. }
        ));
    }
}
