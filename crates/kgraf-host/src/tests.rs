//! Unit tests of the run construction: gates, verifier, workspaces and the assembler.

use crate::assembler::*;
use crate::config::*;
use crate::gates::*;
use crate::host::check_routes;
use crate::verifier::*;
use crate::worker::config::WorkerConfig;
use crate::workspaces::*;
use crate::{Host, HostFactory, NotReady, RouteSpec, RunConfig, RunStart};
use kgraf_core::RunId;
use kgraf_graph::GraphStore;
use kgraf_orchestrator::{
    AttemptEnvs, GateRequest, GateRunner, Integration, MachineGate, OpenContract,
    OrchestratorError, OrchestratorFactory, Route, RunStore, Verdict, VerifiedCommit, Verifier,
    Workspaces,
};
use kgraf_provider::{Ledger, Provider};
use kgraf_runtime::{ContextConfig, WorkerResult};
use kgraf_sandbox::{MergeBack, WorkspaceManager};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Counts the calls that reached it, and settles each.
struct Counts(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl Provider for Counts {
    fn adapter(&self) -> &'static str {
        "counts"
    }
    fn protocol(&self) -> kgraf_core::Protocol {
        kgraf_core::Protocol::Additive
    }
    async fn complete(
        &self,
        request: &kgraf_provider::Request,
        token: &kgraf_provider::CallToken,
    ) -> Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let usage = kgraf_core::TokenUsage {
            uncached_input: 1,
            text_output: 1,
            ..kgraf_core::TokenUsage::default()
        };
        token.settle(&usage, None)?;
        Ok(kgraf_provider::Response {
            finish_reason: None,
            model: request.model.clone(),
            content: vec![kgraf_provider::Content::text("ok")],
            stop_reason: kgraf_provider::StopReason::EndTurn,
            usage,
            request_id: None,
        })
    }
}

/// A configured `--rate-limit` throttles the calls of every provider built
/// for the route (planner and worker share it), and leaves other routes alone.
#[tokio::test]
async fn a_configured_limit_throttles_the_routes_provider_calls() {
    let store_dir = tempfile::tempdir().expect("store dir");
    let store_path = store_dir.path().join("rate-limits.sqlite3");
    let used = BTreeSet::from(["kimi".to_owned(), "openai".to_owned()]);
    let limits = limiters(&["kimi=1/1@10".into()], &used, Some(&store_path)).expect("limits");
    let reached = Arc::new(Counts(std::sync::atomic::AtomicUsize::new(0)));
    let planner = limited(reached.clone(), "kimi", "key", &limits);
    let worker = limited(reached.clone(), "kimi", "key", &limits);
    let other = limited(reached.clone(), "openai", "key", &limits);
    let ledger = Ledger::open_in_memory().expect("ledger");
    let request = kgraf_provider::Request::new("m", "hi", 8);
    let call = |p: Arc<dyn Provider>| {
        let ledger = &ledger;
        let request = &request;
        async move {
            let token = ledger
                .begin("counts", kgraf_core::Protocol::Additive, "m", "a", "t")
                .expect("begin");
            p.complete(request, &token).await
        }
    };
    call(planner)
        .await
        .expect("the first kimi call has the slot");
    let refused = call(worker).await.expect_err("the second waits past 10 ms");
    assert!(
        matches!(refused, kgraf_provider::ProviderError::RateLimited { .. }),
        "{refused}"
    );
    call(other.clone()).await.expect("openai is not limited");
    call(other).await.expect("openai is not limited");
    assert_eq!(
        reached.0.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "the refused call never reached the provider"
    );
}

/// A route with no `--rate-limit` still records its route on every ledger row.
/// Without it the row is costed by the bare model entry: an Azure deployment
/// named like a priced model took that model's price although the route is
/// priced only under its own key. The same call on a limited route agrees.
#[tokio::test]
async fn an_unlimited_generative_route_records_its_route_and_stays_unpriced() {
    let dir = tempfile::tempdir().expect("dir");
    let prices_path = dir.path().join("prices.json");
    std::fs::write(
        &prices_path,
        r#"{"gpt-4o": {"uncached_input": 1000000, "cache_read": 0, "cache_write": 0, "output": 0, "reasoning": 0}}"#,
    )
    .expect("write");
    let prices = read_prices(&prices_path).expect("prices");

    for limited_route in [false, true] {
        let store_path = dir.path().join("rate-limits.sqlite3");
        let used = BTreeSet::from(["azure-openai".to_owned()]);
        let specs: Vec<String> = if limited_route {
            vec!["azure-openai=600/60@10".into()]
        } else {
            Vec::new()
        };
        let limits = limiters(&specs, &used, Some(&store_path)).expect("limits");
        let provider = limited(
            Arc::new(Counts(std::sync::atomic::AtomicUsize::new(0))),
            "azure-openai",
            "key",
            &limits,
        );
        let ledger = Ledger::open_in_memory().expect("ledger");
        let request = kgraf_provider::Request::new("gpt-4o", "hi", 8);
        let token = ledger
            .begin(
                "counts",
                kgraf_core::Protocol::Additive,
                "gpt-4o",
                "a",
                "run-1",
            )
            .expect("begin");
        let id = token.id().clone();
        provider.complete(&request, &token).await.expect("call");
        drop(token);

        let row = ledger.call(&id).expect("read").expect("row");
        assert_eq!(
            row.route.as_deref(),
            Some("azure-openai"),
            "{limited_route}"
        );
        let groups = ledger
            .spend_by(kgraf_provider::GroupBy::Run, &prices)
            .expect("spend");
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].unpriced,
            vec!["gpt-4o".to_owned()],
            "limited: {limited_route}"
        );
        assert_eq!(groups[0].cost, kgraf_core::Money::ZERO);
    }
}

#[test]
fn a_rate_limit_is_per_used_route_and_never_ignored() {
    let store_dir = tempfile::tempdir().expect("store dir");
    let store_path = store_dir.path().join("rate-limits.sqlite3");
    let used = BTreeSet::from(["kimi".to_owned(), "openai".to_owned()]);
    let got = limiters(&["kimi=60/5@30000".into()], &used, Some(&store_path)).expect("parsed");
    let limit = got["kimi"].limit();
    assert_eq!((limit.requests_per_minute, limit.burst), (60, 5));
    assert_eq!(limit.max_wait_ms, 30_000);
    assert!(
        !got.contains_key("openai"),
        "an unlimited route stays unlimited"
    );
    // A limit on a route the run never calls would be silently ignored.
    assert!(limiters(&["other=60".into()], &used, Some(&store_path)).is_err());
    assert!(
        limiters(
            &["kimi=60".into(), "kimi=30".into()],
            &used,
            Some(&store_path)
        )
        .is_err()
    );
    assert!(limiters(&["kimi=0".into()], &used, Some(&store_path)).is_err());
    assert!(limiters(&["kimi".into()], &used, Some(&store_path)).is_err());
}

/// No per-user data directory is a fail-loud refusal, never a silent
/// process-local limiter (requirement 3).
#[test]
fn a_missing_data_directory_refuses_rather_than_falling_back_to_a_process_local_limiter() {
    let used = BTreeSet::from(["kimi".to_owned()]);
    let err = match limiters(&["kimi=60".into()], &used, None) {
        Err(e) => e,
        Ok(_) => panic!("no store path must refuse, not silently limit only this process"),
    };
    assert!(err.to_string().contains("data directory"), "{err}");
}

#[test]
fn routes_are_route_colon_model() {
    assert_eq!(
        split_route("kimi:kimi-k3").expect("ok"),
        ("kimi".to_owned(), "kimi-k3".to_owned())
    );
    assert!(split_route("kimi").is_err());
    assert!(split_route(":m").is_err());
    assert!(split_route("r:").is_err());
}

/// A repository with one commit on `main`, a worktree manager whose base is
/// outside it, and merge-back onto `main`.
struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
    workspaces: Arc<GitWorkspaces>,
    merge_back: Arc<MergeBack>,
}

fn host_git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A fresh, generous `MachineGate` for a test's own `AttemptAssembler`
/// (decision 0067): never the process-wide singleton
/// (`kgraf_orchestrator::process_machine_gate`), so one test's admission or
/// exec-slot limits never couple to another test's in the same binary. Wide
/// enough (100 agents, 100 exec slots) that no existing test here is
/// bottlenecked by it; a test that specifically exercises the gate builds its
/// own narrower one instead.
fn test_machine_gate() -> Arc<MachineGate> {
    MachineGate::new(kgraf_core::ParallelismLimits::derive(100, 100, 100))
}

fn repo() -> Repo {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    repo_in(dir, root, &base, &[("README", "hello\n")])
}

/// A repository at `root` with `files` committed on `main`, its worktrees
/// under `base`.
fn repo_in(dir: tempfile::TempDir, root: PathBuf, base: &Path, files: &[(&str, &str)]) -> Repo {
    std::fs::create_dir_all(&root).expect("mkdir");
    host_git(&root, &["init", "-q", "-b", "main"]);
    for (path, text) in files {
        let path = root.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, text).expect("write");
    }
    host_git(&root, &["add", "."]);
    host_git(
        &root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).expect("mkdir");
    let manager = Arc::new(WorkspaceManager::open(&root, base).expect("manager"));
    // The run's integration branch, never checked out (as in `run`).
    let branch = ensure_integration_branch(&root, "r 1").expect("branch");
    assert_eq!(branch, "kgraf/r-1");
    let merge_back = Arc::new(
        MergeBack::open(
            manager.clone(),
            &branch,
            kgraf_sandbox::Identity {
                name: "Test Owner".into(),
                email: "owner@example.invalid".into(),
            },
        )
        .expect("merge-back"),
    );
    Repo {
        _dir: dir,
        root,
        outside,
        workspaces: Arc::new(GitWorkspaces::new(manager, merge_back.clone())),
        merge_back,
    }
}

/// Decision 0062: a run started before the rename has its integration branch
/// under `haustoria/`. Resuming it continues that branch (a fresh `kgraf/`
/// branch at HEAD would drop the work merged there); a new run gets `kgraf/`.
#[test]
fn a_run_started_before_the_rename_keeps_its_integration_branch() {
    let r = repo();
    host_git(&r.root, &["branch", "haustoria/r-old", "HEAD"]);
    host_git(&r.root, &["branch", "kgraf/r-both", "HEAD"]);
    host_git(&r.root, &["branch", "haustoria/r-both", "HEAD"]);
    assert_eq!(
        ensure_integration_branch(&r.root, "r old").expect("branch"),
        "haustoria/r-old"
    );
    assert_eq!(
        git_out(&r.root, &["branch", "--list", "kgraf/r-old"]),
        "",
        "no second branch was made"
    );
    assert_eq!(
        ensure_integration_branch(&r.root, "r both").expect("branch"),
        "kgraf/r-both"
    );
    assert_eq!(
        ensure_integration_branch(&r.root, "r new").expect("branch"),
        "kgraf/r-new"
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A genuinely empty directory (`git init`, no commit) has an unborn
/// `HEAD`, so a plan approved from it could be read but never built (`git
/// branch ... HEAD` has nothing to point at). `ensure_integration_branch` now
/// makes one empty first commit, under the repository's own configured
/// identity, so the greenfield case reaches dispatch like any other.
#[test]
fn a_greenfield_directory_with_no_commit_still_gets_an_integration_branch() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).expect("mkdir");
    host_git(&root, &["init", "-q", "-b", "main"]);
    host_git(&root, &["config", "user.name", "Greenfield Owner"]);
    host_git(&root, &["config", "user.email", "owner@example.invalid"]);
    assert!(
        std::process::Command::new("git")
            .current_dir(&root)
            .args(["rev-parse", "--verify", "--quiet", "HEAD"])
            .status()
            .expect("git")
            .code()
            != Some(0),
        "the fixture must start with no commit"
    );

    let branch = ensure_integration_branch(&root, "r").expect("branch");
    assert_eq!(branch, "kgraf/r");
    assert_eq!(
        git_out(&root, &["rev-list", "--count", "HEAD"]),
        "1",
        "one commit exists now"
    );
    assert_eq!(
        git_out(&root, &["show", "-s", "--format=%an <%ae>", "HEAD"]),
        "Greenfield Owner <owner@example.invalid>",
        "the first commit carries the user's own identity"
    );
    assert_eq!(
        git_out(&root, &["rev-parse", "kgraf/r"]),
        git_out(&root, &["rev-parse", "HEAD"]),
        "the integration branch points at the new first commit"
    );

    // Idempotent: calling it again (a resumed or re-approved run) makes no
    // second commit and reuses the same branch.
    let branch_again = ensure_integration_branch(&root, "r").expect("branch");
    assert_eq!(branch_again, "kgraf/r");
    assert_eq!(git_out(&root, &["rev-list", "--count", "HEAD"]), "1");
}

fn named(path: &str, id: &str) -> OpenContract {
    let mut c = contract(path);
    c.contract = id.into();
    c
}

/// Verify (commit) then integrate one attempt writing `file`.
async fn finish(r: &Repo, worker: &str, file: &str, text: &str) -> Integration {
    let c = named(file, worker);
    let ws = r
        .workspaces
        .prepare(&c, &worker.into())
        .await
        .expect("worktree");
    std::fs::write(ws.join(file), text).expect("write");
    let verdict = verifier(r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    let i = r.workspaces.integrate(&c, &ws).await.expect("integrate");
    r.workspaces.release(&ws, true).await;
    i
}

#[tokio::test]
async fn a_verified_attempt_is_merged_and_the_next_attempt_starts_from_the_new_tip() {
    let r = repo();
    let first = finish(&r, "w-one", "one.txt", "1").await;
    let Integration::Merged { commit } = first else {
        panic!("{first:?}");
    };
    assert_eq!(r.merge_back.integration_tip().expect("tip"), commit);
    // A fresh attempt is created at the tip, so it sees the merged work.
    let ws = r
        .workspaces
        .prepare(&named("two.txt", "w-two"), &"w-two".into())
        .await
        .expect("worktree");
    assert!(
        ws.join("one.txt").is_file(),
        "starts from the integration tip"
    );
    // The user's checkout did not move.
    assert!(!r.root.join("one.txt").exists());
}

/// Under the default worktree policy the merge removes the winner's worktree
/// (decision 0088). The integration view holds what it merged: the graph
/// indexes the merged file from the view, keeps it, and reads its slice from
/// there; the view follows the tip across merges at the same path; and it is
/// removed with the run's workspaces.
#[tokio::test]
async fn merged_files_stay_in_the_graph_after_the_merge_removes_the_worktree() {
    let r = repo();
    assert!(r.workspaces.retention().deletes(), "the default deletes");
    let c = named("a.rs", "w-one");
    let ws = r.workspaces.prepare(&c, &"w-one".into()).await.expect("ws");
    std::fs::write(ws.join("a.rs"), "pub fn a(x: u8) -> u8 {\n    x + 42\n}\n").expect("write");
    let verdict = verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    let merged = r.workspaces.integrate(&c, &ws).await.expect("integrate");
    assert!(matches!(merged, Integration::Merged { .. }), "{merged:?}");
    assert!(!ws.exists(), "the merge removed the winner's worktree");

    let view = r
        .workspaces
        .integration_view()
        .await
        .expect("view")
        .expect("a git provider keeps an integration view");
    let graph = GraphStore::open_in_memory(&r.root).expect("graph");
    kgraf_graph::Indexer::new(&graph)
        .with_source_root(&view)
        .index_paths(&["a.rs".to_owned()])
        .expect("indexed from the view");
    assert!(graph.file("a.rs").expect("file").is_some());
    let slice = kgraf_graph::slice::slice_for_symbol(&graph, "a", 2_000).expect("slice");
    assert!(slice.render().contains("x + 42"), "{}", slice.render());

    // A second merge: the same view, now at the new tip, with both files.
    let second = finish(&r, "w-two", "b.rs", "pub fn b() {}\n").await;
    assert!(matches!(second, Integration::Merged { .. }), "{second:?}");
    let again = r
        .workspaces
        .integration_view()
        .await
        .expect("view")
        .expect("view");
    assert_eq!(again, view, "one view per run, moved to the tip");
    assert!(view.join("b.rs").is_file() && view.join("a.rs").is_file());
    let slice = kgraf_graph::slice::slice_for_symbol(&graph, "a", 2_000).expect("slice");
    assert!(slice.render().contains("x + 42"), "{}", slice.render());
    let name = view.file_name().and_then(|n| n.to_str()).expect("name");
    assert!(
        is_harness_worktree(name),
        "startup cleanup removes a stale view: {name}"
    );

    drop(r.workspaces);
    assert!(!view.exists(), "the view goes with the run's workspaces");
}

/// Under `Keep` the view is left on disk with the run's other worktrees.
#[tokio::test]
async fn under_keep_the_integration_view_outlives_the_run() {
    let r = repo();
    let keep = GitWorkspaces::new(r.workspaces.manager.clone(), r.merge_back.clone())
        .with_retention(WorktreeRetention::Keep);
    let view = keep.integration_view().await.expect("view").expect("view");
    assert!(view.join("README").is_file(), "the view holds the tip");
    drop(keep);
    assert!(view.exists(), "kept under Keep");
}

/// A live-run defect: a run reported `Completed`, and a
/// contract reported verified, when the merge-back its own record claimed
/// landed was not actually on `kgraf/<run>` (a redirected worker's change
/// went missing from the merged result). `Orchestrator::verify_completion_landed`
/// checks a recorded `Integration::Merged { commit }` against
/// [`kgraf_orchestrator::Workspaces::commit_landed`] before a run may report
/// `Completed`; this is that check's ground truth, with real git, standing in
/// for the fault it must catch: a branch that moved (here, forced back by
/// hand) after a merge already reported accepted. Before `commit_is_on_branch`
/// existed, nothing could tell these two states apart -- an
/// `Integration::Merged` outcome was trusted forever once recorded, with no
/// way to ask git whether it still held.
#[tokio::test]
async fn a_merge_moved_off_the_branch_is_no_longer_reported_landed() {
    let r = repo();
    let outcome = finish(&r, "w-one", "one.txt", "1").await;
    let Integration::Merged { commit } = outcome else {
        panic!("{outcome:?}");
    };
    assert!(
        r.merge_back.commit_is_on_branch(&commit).expect("checked"),
        "the merge just landed; it must be an ancestor of the tip"
    );
    // Simulate the fault this check exists for: something (a force-reset, a
    // second process, a bug upstream of this check) moves the branch away
    // from the commit an `Integration::Merged` outcome already named. No
    // orchestrator code does this on its own reachable path -- reproducing it
    // needs reaching in with git directly, exactly because the whole point of
    // `commit_is_on_branch` is that a recorded outcome alone cannot tell.
    let parent = git_out(&r.root, &["rev-parse", &format!("{commit}^1")]);
    host_git(
        &r.root,
        &[
            "update-ref",
            &format!("refs/heads/{}", r.merge_back.integration_branch()),
            &parent,
            &commit,
        ],
    );
    assert!(
        !r.merge_back.commit_is_on_branch(&commit).expect("checked"),
        "the branch no longer holds this commit; the check must say so, not assume the record"
    );
}

#[tokio::test]
async fn a_conflicting_attempt_comes_back_typed_and_the_tip_does_not_move() {
    let r = repo();
    // Both start from the same tip and write the same file differently.
    let a = named("same.txt", "w-a");
    let b = named("same.txt", "w-b");
    let wa = r.workspaces.prepare(&a, &"w-a".into()).await.expect("a");
    let wb = r.workspaces.prepare(&b, &"w-b".into()).await.expect("b");
    std::fs::write(wa.join("same.txt"), "from a\n").expect("write");
    std::fs::write(wb.join("same.txt"), "from b\n").expect("write");
    for (c, ws) in [(&a, &wa), (&b, &wb)] {
        let v = verifier(&r, "true").verify(c, ws, &result()).await;
        let v = v.expect("ran");
        assert!(v.passed(), "{v:?}");
    }
    assert!(matches!(
        r.workspaces.integrate(&a, &wa).await.expect("a"),
        Integration::Merged { .. }
    ));
    let tip = r.merge_back.integration_tip().expect("tip");
    let second = r.workspaces.integrate(&b, &wb).await.expect("b");
    let Integration::Conflict { tip: at, paths, .. } = second else {
        panic!("{second:?}");
    };
    assert_eq!(at, tip);
    assert_eq!(paths, vec!["same.txt".to_owned()]);
    assert_eq!(
        r.merge_back.integration_tip().expect("tip"),
        tip,
        "not moved"
    );
}

/// Decision 0073, against real git worktrees (no fakes): a
/// conflict's attempt is kept, not discarded, until a resolver worker settles
/// it. This drives `GitWorkspaces` directly, the same seam
/// `kgraf-orchestrator`'s `resolver.rs` exercises through the full loop with
/// fakes for the dispatch and session bookkeeping.
#[tokio::test]
async fn a_real_git_conflicts_attempt_is_kept_until_a_resolver_settles_it() {
    let r = repo();
    let a = named("same.txt", "w-a");
    let b = named("same.txt", "w-b");
    let wa = r.workspaces.prepare(&a, &"w-a".into()).await.expect("a");
    let wb = r.workspaces.prepare(&b, &"w-b".into()).await.expect("b");
    std::fs::write(wa.join("same.txt"), "from a\n").expect("write");
    std::fs::write(wb.join("same.txt"), "from b\n").expect("write");
    for (c, ws) in [(&a, &wa), (&b, &wb)] {
        let v = verifier(&r, "true").verify(c, ws, &result()).await;
        let v = v.expect("ran");
        assert!(v.passed(), "{v:?}");
    }
    assert!(matches!(
        r.workspaces.integrate(&a, &wa).await.expect("a"),
        Integration::Merged { .. }
    ));
    let tip = r.merge_back.integration_tip().expect("tip");
    let conflict = r.workspaces.integrate(&b, &wb).await.expect("b");
    let Integration::Conflict {
        commit: Some(lost_commit),
        ..
    } = &conflict
    else {
        panic!("expected a conflict with a kept commit, got {conflict:?}");
    };
    // Kept, not discarded (0073 supersedes 0018's immediate discard): the
    // attempt ref is still there, and it is the losing commit, reachable in
    // the repository even though `refs/heads/kgraf/r-1` never moved for it.
    assert_eq!(
        git_out(
            &r.root,
            &["rev-parse", "--verify", "refs/kgraf/attempts/w-b+1"]
        ),
        *lost_commit,
        "the conflicted attempt's commit is kept reachable"
    );

    // A resolver worker: a fresh worktree at the new tip (as any contract's
    // first attempt would get), which can read the kept commit straight out
    // of the repository's own history and bring in the change by hand.
    let mut resolver = named("same.txt", "w-b-resolver1");
    resolver.base = Some(tip.clone());
    let rws = r
        .workspaces
        .prepare(&resolver, &"w-b-resolver1".into())
        .await
        .expect("resolver worktree");
    assert_eq!(
        git_out(&rws, &["rev-parse", "HEAD"]),
        tip,
        "starts at the new tip, not the old one"
    );
    assert_eq!(
        git_out(&rws, &["show", &format!("{lost_commit}:same.txt")]),
        "from b",
        "the resolver can read the kept commit's content straight from history"
    );
    std::fs::write(rws.join("same.txt"), "from a\nfrom b\n").expect("write");
    let verdict = verifier(&r, "true")
        .verify(&resolver, &rws, &result())
        .await
        .expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    let resolved = r
        .workspaces
        .integrate(&resolver, &rws)
        .await
        .expect("resolver integrate");
    assert!(
        matches!(resolved, Integration::Merged { .. }),
        "{resolved:?}"
    );
    assert_eq!(
        git_out(
            &r.root,
            &[
                "show",
                &format!("{}:same.txt", r.merge_back.integration_tip().expect("tip"))
            ]
        ),
        "from a\nfrom b"
    );

    // Settled: the superseded attempt's ref and worktree are discarded once,
    // whichever way it went.
    r.workspaces.discard_conflict(&resolver, &wb).await;
    assert!(
        !wb.exists(),
        "deleting: the settled attempt's worktree goes"
    );
    assert!(r.workspaces.take_problems().is_empty());
    assert_eq!(
        git_out(
            &r.root,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/kgraf/attempts/w-b+1"
            ]
        ),
        "",
        "the kept attempt is gone once the resolver settled it"
    );
    // A second call is a no-op, never an error, for the fallback path that
    // also discards on failure.
    r.workspaces.discard_conflict(&resolver, &wb).await;
    assert!(r.workspaces.take_problems().is_empty());
}

#[tokio::test]
async fn a_remediation_starts_from_the_commit_its_gate_failed_on() {
    let r = repo();
    let Integration::Merged { commit: failed_at } = finish(&r, "w-1", "x.txt", "1").await else {
        panic!("merge");
    };
    finish(&r, "w-2", "y.txt", "2").await;
    let mut fix = named("x.txt", "fix-root-1");
    fix.base = Some(failed_at.clone());
    let ws = r
        .workspaces
        .prepare(&fix, &"w-fix".into())
        .await
        .expect("worktree");
    assert_eq!(git_out(&ws, &["rev-parse", "HEAD"]), failed_at);
    assert!(!ws.join("y.txt").exists());
}

/// The bus wrote conversation files into the user's
/// checkout and copied them to the branch, so merging `kgraf/<run>`
/// failed on the untracked copies. They are written only in a worktree of
/// the integration tip now; the user's checkout stays clean and the merge
/// succeeds. A second message continues the file from the branch.
#[tokio::test]
async fn conversation_files_reach_the_branch_without_touching_the_checkout() {
    let r = repo();
    let rel = "docs/notes/a--b.md";
    for (i, line) in ["# Conversation\n", "## m2\n"].iter().enumerate() {
        let root = r
            .workspaces
            .open_comms()
            .await
            .expect("open")
            .expect("a worktree");
        assert!(!root.starts_with(&r.root), "never the user's checkout");
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        let before = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(before.is_empty(), i == 0, "continues the branch's file");
        std::fs::write(&path, format!("{before}{line}")).expect("write");
        let msg = format!("kgraf: agent message m{i}");
        let merged = r
            .workspaces
            .commit_comms(&root, &[rel.to_owned()], &msg)
            .await
            .expect("commit");
        assert!(matches!(merged, Integration::Merged { .. }), "{merged:?}");
        assert!(!root.exists(), "the worktree is retired");
    }
    let shown = git_out(&r.root, &["show", &format!("kgraf/r-1:{rel}")]);
    assert_eq!(shown, "# Conversation\n## m2");
    assert!(!r.root.join("docs").exists(), "no copy in the checkout");
    assert_eq!(git_out(&r.root, &["status", "--porcelain"]), "");
    host_git(
        &r.root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "merge",
            "-q",
            "--no-edit",
            "kgraf/r-1",
        ],
    );
    assert!(r.root.join(rel).is_file(), "the owner's merge brings it in");
}

/// A conversation file is indexed from the worktree the bus wrote it in,
/// and that worktree is removed as soon as the file is committed: the file's
/// recorded source root then names a tree that is gone, and the checkout never
/// received the file, so its nodes read as "cannot read" from every reader
/// without a worktree of its own. Re-pointed at the integration view, which
/// holds the committed file, they read again.
#[tokio::test]
async fn a_conversation_node_reads_again_once_its_dead_root_is_repointed_at_the_view() {
    let r = repo();
    let rel = format!("{}/a--b.md", kgraf_graph::BUS_OWNED_DIRS[0]);
    let root = r
        .workspaces
        .open_comms()
        .await
        .expect("open")
        .expect("a worktree");
    let path = root.join(&rel);
    std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
    std::fs::write(&path, "# Conversation\n\n## m1 done from a to b\n\nhello\n").expect("write");
    let graph = GraphStore::open_in_memory(&r.root).expect("graph");
    kgraf_graph::Indexer::new(&graph)
        .with_source_root(&root)
        .index_paths(std::slice::from_ref(&rel))
        .expect("indexed from the conversation worktree");
    let merged = r
        .workspaces
        .commit_comms(&root, std::slice::from_ref(&rel), "kgraf: agent message m1")
        .await
        .expect("commit");
    assert!(matches!(merged, Integration::Merged { .. }), "{merged:?}");
    assert!(!root.exists(), "the conversation worktree is retired");
    let node = graph.nodes_in_file(&rel).expect("nodes")[0].clone();
    let err = graph
        .source_of(&node)
        .expect_err("the recorded root is gone");
    assert!(err.to_string().contains("cannot read"), "{err}");

    let view = r
        .workspaces
        .integration_view()
        .await
        .expect("view")
        .expect("a git provider keeps an integration view");
    let repair = graph.repair_source_roots(&view).expect("repair");
    assert_eq!(repair.repointed, vec![rel.clone()]);
    assert!(
        graph
            .source_of(&node)
            .expect("readable from the view")
            .contains("m1 done from a to b")
    );
}

/// An empty commit, or one that fails (here a tampered
/// gitlink, refused before any git runs in the tree), leaves no worktree.
#[tokio::test]
async fn a_conversation_worktree_is_removed_on_every_commit_outcome() {
    let r = repo();
    let empty = r
        .workspaces
        .open_comms()
        .await
        .expect("open")
        .expect("root");
    let i = r
        .workspaces
        .commit_comms(&empty, &[], "m")
        .await
        .expect("ran");
    assert!(matches!(i, Integration::Empty), "{i:?}");
    let failing = r
        .workspaces
        .open_comms()
        .await
        .expect("open")
        .expect("root");
    std::fs::write(failing.join(".git"), "gitdir: /tmp/elsewhere\n").expect("tamper");
    assert!(r.workspaces.commit_comms(&failing, &[], "m").await.is_err());
    for root in [&empty, &failing] {
        assert!(!root.exists(), "{} removed", root.display());
        assert!(
            !git_out(&r.root, &["worktree", "list"]).contains(&root.display().to_string()),
            "unregistered"
        );
    }
}

#[tokio::test]
async fn a_conversation_worktree_that_is_never_committed_is_removed() {
    let r = repo();
    let root = r
        .workspaces
        .open_comms()
        .await
        .expect("open")
        .expect("root");
    r.workspaces.discard_comms(&root).await;
    assert!(!root.exists());
    assert!(
        r.workspaces.commit_comms(&root, &[], "m").await.is_err(),
        "a discarded worktree is not committed"
    );
}

/// `kgraf clean` refuses a worktree a run record
/// still names unless forced, and a live owner with a clear reason.
#[tokio::test]
async fn clean_refuses_a_worktree_a_record_names_unless_forced() {
    let r = repo();
    let c = named("a.txt", "w-named");
    let ws = r
        .workspaces
        .prepare(&c, &"w-named".into())
        .await
        .expect("ws");
    // Owned by this (live) process: refused, saying so.
    let base = r._dir.path().join("ws");
    let live = clean_in(&r.root, &base, "w-named", true).expect_err("live owner");
    assert!(
        format!("{live:#}").contains("still running here"),
        "{live:#}"
    );
    kill_owner(&r, &ws);
    let state = crate::state_dir(&r.root).expect("state");
    let graph = Arc::new(Mutex::new(
        GraphStore::open(&state.join("graph.db"), &r.root).expect("graph"),
    ));
    let mut record = kgraf_orchestrator::RunStateRecord::new(kgraf_core::RunId::new("r7"), "x", 0);
    let mut named_c = c.clone();
    named_c.workers = vec!["w-named".into()];
    record.open_contracts.push(named_c);
    RunStore::new(graph).save(&record).expect("save");
    let err = clean_in(&r.root, &base, "w-named", false).expect_err("named");
    assert!(format!("{err:#}").contains("run(s) r7"), "{err:#}");
    assert!(ws.exists());
    assert_eq!(
        clean_in(&r.root, &base, "w-named", true).expect("forced"),
        ws
    );
    assert!(!ws.exists());
}

/// `kgraf clean` finds a kept worktree wherever kGraf keeps one: an
/// orchestrated run's under `.kgraf/worktrees` and `kgraf worker`'s under
/// `.kgraf/workspaces` (decision 0088). A name in neither is refused,
/// listing every kept worktree in both; a name in both must say which.
#[tokio::test]
async fn clean_finds_kept_worktrees_of_runs_and_of_kgraf_worker() {
    let r = repo();
    let state = crate::state_dir(&r.root).expect("state");
    let make = |base: &str, name: &str| {
        let mgr = kgraf_sandbox::WorkspaceManager::open(&r.root, &state.join(base)).expect("mgr");
        let ws = mgr
            .create(&kgraf_sandbox::WorkspaceSpec {
                name,
                base: "HEAD",
                branch: None,
            })
            .expect("worktree");
        let root = ws.root().to_path_buf();
        // Released as a finished `kgraf worker` leaves a kept worktree.
        drop(ws);
        kill_owner(&r, &root);
        root
    };
    let run_kept = make("worktrees", "w-run");
    let worker_kept = make("workspaces", "w-worker");
    let twin_run = make("worktrees", "twin");
    let twin_worker = make("workspaces", "twin");

    let err = crate::clean(&r.root, "nope", false).expect_err("unknown");
    let text = format!("{err:#}");
    for want in [
        ".kgraf/worktrees",
        ".kgraf/workspaces",
        "worktrees/w-run",
        "workspaces/w-worker",
    ] {
        assert!(text.contains(want), "{want} missing: {text}");
    }
    let both = crate::clean(&r.root, "twin", false).expect_err("ambiguous");
    assert!(format!("{both:#}").contains("workspaces/twin"), "{both:#}");
    assert!(twin_run.exists() && twin_worker.exists());

    assert_eq!(
        crate::clean(&r.root, "w-worker", false).expect("worker's"),
        kgraf_core::plain_path::canonicalize(&worker_kept).unwrap_or(worker_kept.clone())
    );
    assert!(!worker_kept.exists());
    assert!(crate::clean(&r.root, "w-run", false).is_ok());
    assert!(!run_kept.exists());
    assert!(crate::clean(&r.root, "workspaces/twin", false).is_ok());
    assert!(!twin_worker.exists() && twin_run.exists());
}

/// Relock a worktree as a killed process leaves it.
fn kill_owner(r: &Repo, root: &Path) {
    let path = root.to_str().expect("utf-8");
    host_git(&r.root, &["worktree", "unlock", path]);
    let dead = {
        let mut c = std::process::Command::new("true").spawn().expect("spawn");
        let pid = c.id();
        c.wait().expect("wait");
        pid
    };
    let reason = format!(
        "{}:pid={dead}:{}:name=x",
        kgraf_sandbox::LOCK_TAG,
        kgraf_sandbox::HostId::current().fields()
    );
    host_git(&r.root, &["worktree", "lock", "--reason", &reason, path]);
}

/// Startup cleanup runs at startup, removes what no run record names, and
/// keeps what one does (including the worktrees a resume reopens). Without
/// it, crashed conversation and probe worktrees would stay.
#[tokio::test]
async fn startup_cleanup_keeps_referenced_worktrees_and_removes_the_rest() {
    let r = repo();
    let mut c = named("a.txt", "w-kept");
    let kept = r
        .workspaces
        .prepare(&c, &"w-kept".into())
        .await
        .expect("ws");
    std::fs::write(kept.join("a.txt"), "work\n").expect("write");
    c.worktrees.insert(
        "w-kept".into(),
        r.workspaces.handle(&kept).await.expect("handle"),
    );
    let dropped = r
        .workspaces
        .prepare(&named("b.txt", "w-dropped"), &"w-dropped".into())
        .await
        .expect("ws");
    let comms = r
        .workspaces
        .open_comms()
        .await
        .expect("open")
        .expect("root");
    for root in [&kept, &dropped, &comms] {
        kill_owner(&r, root);
    }
    let graph = Arc::new(Mutex::new(
        GraphStore::open(&r._dir.path().join("graph.db"), &r.root).expect("graph"),
    ));
    let store = RunStore::new(graph);
    let mut record = kgraf_orchestrator::RunStateRecord::new(kgraf_core::RunId::new("r1"), "x", 0);
    c.workers = vec!["w-kept".into()];
    record.open_contracts.push(c.clone());
    store.save(&record).expect("save");

    let manager = WorkspaceManager::open(&r.root, &r._dir.path().join("ws")).expect("m");
    let removed =
        clean_stale_worktrees(&manager, &store, WorktreeRetention::Delete).expect("clean");
    // Worktrees (their scratch directories go with them).
    let mut gone: Vec<PathBuf> = removed
        .iter()
        .map(|x| x.path.clone())
        .filter(|p| !p.components().any(|c| c.as_os_str() == ".scratch"))
        .collect();
    gone.sort();
    let mut expected = vec![dropped.clone(), comms.clone()];
    expected.sort();
    assert_eq!(gone, expected);
    assert!(!dropped.exists() && !comms.exists());
    assert!(
        kept.join("a.txt").is_file(),
        "the referenced worktree stays"
    );
    // And a resume still reopens it.
    let again = restarted(&r);
    assert_eq!(
        again.prepare(&c, &"w-kept".into()).await.expect("reopen"),
        kept
    );
}

/// At the end of a run, attempt refs of the worktrees
/// it worked in are swept, except those an open contract still uses (a
/// reopen needs a committed, unmerged worktree's ref).
#[tokio::test]
async fn attempt_refs_are_swept_at_run_end_except_for_open_work() {
    let r = repo();
    let mut c = named("a.txt", "w-s");
    c.verifications = 1;
    let ws = r.workspaces.prepare(&c, &"w-s".into()).await.expect("ws");
    std::fs::write(ws.join("a.txt"), "x\n").expect("write");
    let verdict = verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    let reference = "refs/kgraf/attempts/w-s+1";
    let exists = || !git_out(&r.root, &["rev-parse", "--verify", "--quiet", reference]).is_empty();
    assert!(exists());
    let mut record = kgraf_orchestrator::RunStateRecord::new(kgraf_core::RunId::new("r1"), "x", 0);
    c.workers = vec!["w-s".into()];
    record.open_contracts.push(c);
    assert!(
        r.workspaces
            .sweep_refs(Some(&record), || Ok(Default::default()))
            .expect("sweep")
            .is_empty()
    );
    assert!(exists(), "still open: kept");
    record.open_contracts.clear();
    assert_eq!(
        r.workspaces
            .sweep_refs(Some(&record), || Ok(Default::default()))
            .expect("sweep"),
        vec![reference.to_owned()]
    );
    assert!(!exists(), "finished: swept");
}

/// A second process's view of the same repository: nothing live, only
/// what the run record kept.
fn restarted(r: &Repo) -> Arc<GitWorkspaces> {
    let manager =
        Arc::new(WorkspaceManager::open(&r.root, &r._dir.path().join("ws")).expect("manager"));
    let merge_back = Arc::new(
        MergeBack::open(
            manager.clone(),
            "kgraf/r-1",
            kgraf_sandbox::Identity {
                name: "Test Owner".into(),
                email: "owner@example.invalid".into(),
            },
        )
        .expect("merge-back"),
    );
    Arc::new(GitWorkspaces::new(manager, merge_back))
}

/// Item 3: a killed run's worktree is reopened by its recorded gitlink,
/// with the worker's changes, rather than refused (which restarted the
/// contract with a fresh worker). Without the record it is still refused.
#[tokio::test]
async fn a_killed_runs_worktree_is_reopened_by_its_recorded_gitlink() {
    let r = repo();
    let mut c = named("a.txt", "w-a");
    let ws = r.workspaces.prepare(&c, &"w-a".into()).await.expect("ws");
    std::fs::write(ws.join("a.txt"), "half done\n").expect("write");
    let handle = r.workspaces.handle(&ws).await.expect("a handle");

    let again = restarted(&r);
    let refused = again.prepare(&c, &"w-a".into()).await;
    assert!(refused.is_err(), "no record: refused, never replaced");
    assert!(ws.join("a.txt").is_file(), "and the work is still there");

    c.worktrees.insert("w-a".into(), handle);
    let reopened = again.prepare(&c, &"w-a".into()).await.expect("reopened");
    assert_eq!(reopened, ws);
    assert_eq!(
        std::fs::read_to_string(reopened.join("a.txt")).expect("read"),
        "half done\n"
    );
    // It verifies and merges like any attempt.
    let v = CommandVerifier {
        workspaces: again.clone(),
        merge_back: again.merge_back.clone(),
        ..verifier(&r, "true")
    };
    let verdict = v.verify(&c, &reopened, &result()).await.expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    assert!(matches!(
        again.integrate(&c, &reopened).await.expect("integrate"),
        Integration::Merged { .. }
    ));
    assert_eq!(git_out(&r.root, &["show", "kgraf/r-1:a.txt"]), "half done");
}

/// Item 1: the process died after its verification committed the attempt
/// and before the merge. The resumed run reopens the worktree and verifies
/// it again; numbered by the record's counter, the new commit does not
/// collide with the attempt ref the dead process left.
#[tokio::test]
async fn a_re_verified_contract_never_collides_with_its_attempt_ref() {
    let r = repo();
    let mut c = named("a.txt", "w-a");
    let ws = r.workspaces.prepare(&c, &"w-a".into()).await.expect("ws");
    std::fs::write(ws.join("a.txt"), "done\n").expect("write");
    c.worktrees.insert(
        "w-a".into(),
        r.workspaces.handle(&ws).await.expect("handle"),
    );
    c.verifications = 1;
    let verdict = verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    let first = git_out(&r.root, &["rev-parse", "refs/kgraf/attempts/w-a+1"]);
    assert_eq!(first.len(), 40, "the first verification's ref");

    let again = restarted(&r);
    let reopened = again.prepare(&c, &"w-a".into()).await.expect("reopened");
    let v = CommandVerifier {
        workspaces: again.clone(),
        merge_back: again.merge_back.clone(),
        ..verifier(&r, "true")
    };
    // The rollback removed the unmerged commit's ref.
    assert_eq!(
        git_out(
            &r.root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/kgraf/attempts/w-a+1"
            ]
        ),
        ""
    );
    // A kill between the rollback and that removal leaves it; reusing the
    // number then collides with it.
    host_git(
        &r.root,
        &["update-ref", "refs/kgraf/attempts/w-a+1", &first],
    );
    let same = v.verify(&c, &reopened, &result()).await.expect("ran");
    assert!(
        matches!(&same, Verdict::Fail { reason } if reason.contains("already exists")),
        "{same:?}"
    );
    // The next number, as the orchestrator assigns it, does not.
    c.verifications = 2;
    let next = v.verify(&c, &reopened, &result()).await.expect("ran");
    assert!(next.passed(), "{next:?}");
    assert!(matches!(
        again.integrate(&c, &reopened).await.expect("integrate"),
        Integration::Merged { .. }
    ));
    assert_eq!(git_out(&r.root, &["show", "kgraf/r-1:a.txt"]), "done");
}

/// An attempt's slices are read from its own worktree. The shape seen live:
/// the graph indexed a scaffold file, the checkout could not give it back,
/// and every attempt of the contract died at spawn with "cannot render slice
/// ... cannot read". Here the file is gone from the checkout but committed, so
/// the attempt's worktree holds it: the slice is rendered from there, and a
/// node whose file the worktree lacks is left out by name, not fatal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attempts_slices_are_read_from_its_own_worktree() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let r = repo_in(
        dir,
        root,
        &base,
        &[(
            "iface/src/lib.rs",
            "pub fn order_total() -> i64 {\n    42\n}\n",
        )],
    );
    let state = r._dir.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let graph_path = state.join("graph.db");
    let (node, lost) = {
        let g = GraphStore::open(&graph_path, &r.root).expect("graph");
        std::fs::write(r.root.join("untracked.rs"), "pub fn lost() {}\n").expect("write");
        kgraf_graph::Indexer::new(&g).index().expect("index");
        let id = |name: &str| {
            kgraf_core::NodeId::new(g.resolve_symbol(name).expect("resolve")[0].id.clone())
        };
        (id("order_total"), id("lost"))
    };
    // The checkout no longer holds the committed file; the worktree, made
    // from the commit, does. `untracked.rs` was never committed, so the
    // worktree lacks it although the checkout has it.
    std::fs::remove_file(r.root.join("iface/src/lib.rs")).expect("rm");
    let ledger = Arc::new(Ledger::open(&state.join("ledger.db")).expect("ledger"));
    let mut routes = BTreeMap::new();
    routes.insert(
        "fake:m".to_owned(),
        RouteParts {
            provider: Arc::new(ShellCaller),
            vendor: "fake (fake)".into(),
            vendor_name: "fake".into(),
            model: "m".into(),
        },
    );
    let assembler = AttemptAssembler {
        root: r.root.clone(),
        state: state.clone(),
        graph_path,
        indexed: true,
        ledger: ledger.clone(),
        config: WorkerConfig::with_servers(vec![]).expect("config"),
        max_output_tokens: 1_000,
        max_checkpoints: 3,
        reasoning: kgraf_provider::ReasoningLevels::new().with(
            kgraf_provider::ReasoningScope::Process(kgraf_provider::ReasoningProcess::WorkerRound),
            kgraf_provider::ReasoningEffort::High.into(),
        ),
        read_denies: vec![state.clone()],
        workspaces: r.workspaces.clone(),
        routes,
        extra_tools: Vec::new(),
        session_line: None,
        prefixes: Arc::new(kgraf_runtime::PrefixRegistry::new()),
        machine_gate: test_machine_gate(),
        tbench_bridge_socket: None,
        dependency_refetch: crate::deps::inert_refetch(),
        skills: None,
        skill_directory: Default::default(),
        refusal_directory: Default::default(),
        permissions: Arc::new(kgraf_tools::PermissionControl::manual()),
    };
    let route = Route {
        name: "fake:m".into(),
        vendor: "fake".into(),
        env: Arc::new(kgraf_runtime::WorkerEnv::new(
            ledger.clone(),
            Arc::new(ShellCaller),
            Arc::new(kgraf_tools::ToolRegistry::new()),
            assemble_prefix(),
        )),
        context: ContextConfig::for_model(200_000).expect("context"),
        readiness: Arc::new(kgraf_orchestrator::AlwaysReady),
        local: false,
    };
    let ws = worktree(&r, "w-slices").await;
    assert!(
        ws.join("iface/src/lib.rs").is_file(),
        "the worktree holds it"
    );
    let built = assembler
        .build(&route, &ws, &"w-slices".into(), &"r9/c-slices".into())
        .await
        .expect("built");
    // Decision 0098: the run's reasoning setting reaches the attempt's loop.
    assert_eq!(
        built
            .env
            .settings
            .reasoning
            .effort(kgraf_provider::ReasoningProcess::WorkerRound),
        Some(kgraf_provider::ReasoningEffort::High)
    );
    let rendering = built
        .env
        .slices
        .render(&[node.clone(), lost.clone()])
        .expect("a stale node does not fail the rendering");
    assert_eq!(rendering.slices.len(), 1, "{rendering:?}");
    assert_eq!(rendering.slices[0].node, node);
    assert!(rendering.slices[0].text.contains("42"), "{rendering:?}");
    assert_eq!(rendering.left_out.len(), 1, "{rendering:?}");
    assert_eq!(rendering.left_out[0].node, lost);
    assert_eq!(rendering.left_out[0].path, "untracked.rs");
    assert!(
        rendering.left_out[0]
            .reason
            .contains("not in the worker's tree"),
        "{rendering:?}"
    );
    if let Some(closer) = built.closer {
        closer.close().await;
    }
}

/// The build found nothing to index (`indexed: false`), but the run's own
/// writes were indexed since, so a later brief names nodes. The attempt opens
/// the graph that now holds them and renders the slice; before, it got no
/// graph and the spawn failed with "no graph is configured for this worker".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attempt_opens_a_graph_indexed_after_the_build_found_none() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let r = repo_in(
        dir,
        root,
        &base,
        &[(
            "iface/src/lib.rs",
            "pub fn order_total() -> i64 {\n    42\n}\n",
        )],
    );
    let state = r._dir.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let graph_path = state.join("graph.db");
    let node = {
        let g = GraphStore::open(&graph_path, &r.root).expect("graph");
        kgraf_graph::Indexer::new(&g).index().expect("index");
        kgraf_core::NodeId::new(
            g.resolve_symbol("order_total").expect("resolve")[0]
                .id
                .clone(),
        )
    };
    let ledger = Arc::new(Ledger::open(&state.join("ledger.db")).expect("ledger"));
    let mut routes = BTreeMap::new();
    routes.insert(
        "fake:m".to_owned(),
        RouteParts {
            provider: Arc::new(ShellCaller),
            vendor: "fake (fake)".into(),
            vendor_name: "fake".into(),
            model: "m".into(),
        },
    );
    let assembler = AttemptAssembler {
        root: r.root.clone(),
        state: state.clone(),
        graph_path,
        indexed: false,
        ledger: ledger.clone(),
        config: WorkerConfig::with_servers(vec![]).expect("config"),
        max_output_tokens: 1_000,
        max_checkpoints: 3,
        reasoning: Default::default(),
        read_denies: vec![state.clone()],
        workspaces: r.workspaces.clone(),
        routes,
        extra_tools: Vec::new(),
        session_line: None,
        prefixes: Arc::new(kgraf_runtime::PrefixRegistry::new()),
        machine_gate: test_machine_gate(),
        tbench_bridge_socket: None,
        dependency_refetch: crate::deps::inert_refetch(),
        skills: None,
        skill_directory: Default::default(),
        refusal_directory: Default::default(),
        permissions: Arc::new(kgraf_tools::PermissionControl::manual()),
    };
    let route = Route {
        name: "fake:m".into(),
        vendor: "fake".into(),
        env: Arc::new(kgraf_runtime::WorkerEnv::new(
            ledger.clone(),
            Arc::new(ShellCaller),
            Arc::new(kgraf_tools::ToolRegistry::new()),
            assemble_prefix(),
        )),
        context: ContextConfig::for_model(200_000).expect("context"),
        readiness: Arc::new(kgraf_orchestrator::AlwaysReady),
        local: false,
    };
    let ws = worktree(&r, "w-late").await;
    let built = assembler
        .build(&route, &ws, &"w-late".into(), &"r9/c-late".into())
        .await
        .expect("built");
    let rendering = built
        .env
        .slices
        .render(std::slice::from_ref(&node))
        .expect("renders");
    assert_eq!(rendering.slices.len(), 1, "{rendering:?}");
    assert!(rendering.left_out.is_empty(), "{rendering:?}");
    if let Some(closer) = built.closer {
        closer.close().await;
    }
}

/// A graph that appears mid-run adds the graph tool to the next attempt's
/// tool list, so its shared prefix differs from the earlier attempt's (seen
/// live: a re-plan's second contract failed at spawn with "already has
/// prefix"). The assembler replaces the run's prefix for the vendor and
/// model, says so with both digests, and the spawn guard admits the new
/// prefix. The guard still refuses the old one afterward: two different
/// prefixes never share a slot unannounced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_graph_that_appears_mid_run_replaces_the_shared_prefix_and_says_so() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let r = repo_in(
        dir,
        root,
        &base,
        &[(
            "iface/src/lib.rs",
            "pub fn order_total() -> i64 {\n    42\n}\n",
        )],
    );
    let state = r._dir.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let graph_path = state.join("graph.db");
    let ledger = Arc::new(Ledger::open(&state.join("ledger.db")).expect("ledger"));
    let mut routes = BTreeMap::new();
    routes.insert(
        "fake:m".to_owned(),
        RouteParts {
            provider: Arc::new(ShellCaller),
            vendor: "fake (fake)".into(),
            vendor_name: "fake".into(),
            model: "m".into(),
        },
    );
    let prefixes = Arc::new(kgraf_runtime::PrefixRegistry::new());
    let assembler = AttemptAssembler {
        root: r.root.clone(),
        state: state.clone(),
        graph_path: graph_path.clone(),
        indexed: false,
        ledger: ledger.clone(),
        config: WorkerConfig::with_servers(vec![]).expect("config"),
        max_output_tokens: 1_000,
        max_checkpoints: 3,
        reasoning: Default::default(),
        read_denies: vec![state.clone()],
        workspaces: r.workspaces.clone(),
        routes,
        extra_tools: Vec::new(),
        session_line: None,
        prefixes: prefixes.clone(),
        machine_gate: test_machine_gate(),
        tbench_bridge_socket: None,
        dependency_refetch: crate::deps::inert_refetch(),
        skills: None,
        skill_directory: Default::default(),
        refusal_directory: Default::default(),
        permissions: Arc::new(kgraf_tools::PermissionControl::manual()),
    };
    let route = Route {
        name: "fake:m".into(),
        vendor: "fake".into(),
        env: Arc::new(kgraf_runtime::WorkerEnv::new(
            ledger.clone(),
            Arc::new(ShellCaller),
            Arc::new(kgraf_tools::ToolRegistry::new()),
            assemble_prefix(),
        )),
        context: ContextConfig::for_model(200_000).expect("context"),
        readiness: Arc::new(kgraf_orchestrator::AlwaysReady),
        local: false,
    };
    // First attempt: no graph yet. Its spawn admits the first prefix.
    let ws1 = worktree(&r, "w-early").await;
    let early = assembler
        .build(&route, &ws1, &"w-early".into(), &"r9/c-early".into())
        .await
        .expect("early built");
    assert!(early.notice.is_none(), "the first prefix replaces nothing");
    prefixes.admit(&early.env.prefix).expect("first admitted");
    // The run indexes its own writes; the next attempt opens that graph.
    {
        let g = GraphStore::open(&graph_path, &r.root).expect("graph");
        kgraf_graph::Indexer::new(&g).index().expect("index");
    }
    let ws2 = worktree(&r, "w-late").await;
    let late = assembler
        .build(&route, &ws2, &"w-late".into(), &"r9/c-late".into())
        .await
        .expect("late built");
    assert_ne!(
        early.env.prefix.digest(),
        late.env.prefix.digest(),
        "the graph tool changes the prefix"
    );
    let notice = late.notice.as_deref().expect("the replacement is said");
    assert!(
        notice.contains(early.env.prefix.digest().as_str())
            && notice.contains(late.env.prefix.digest().as_str())
            && notice.contains("graph"),
        "{notice}"
    );
    let changes = prefixes.invalidations();
    assert_eq!(changes.len(), 1, "{changes:?}");
    // The spawn guard admits the replacement and still refuses a stray prefix.
    prefixes
        .admit(&late.env.prefix)
        .expect("replacement admitted");
    assert!(prefixes.admit(&early.env.prefix).is_err());
    for built in [early, late] {
        if let Some(closer) = built.closer {
            closer.close().await;
        }
    }
}

/// Item 2: each attempt's environment is assembled in its own worktree.
/// The shell runs in that worktree (two attempts see their own files),
/// its approval hook denies an unapproved write (a delegate never asks),
/// and the configured MCP servers are registered. The model is fake; the
/// worker loop, shell sandbox and approval chain are real.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_attempt_gets_a_shell_bound_to_its_own_worktree() {
    let r = repo();
    let state = r._dir.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let ledger = Arc::new(Ledger::open(&state.join("ledger.db")).expect("ledger"));
    let mut routes = BTreeMap::new();
    routes.insert(
        "fake:m".to_owned(),
        RouteParts {
            provider: Arc::new(ShellCaller),
            vendor: "fake (fake)".into(),
            vendor_name: "fake".into(),
            model: "m".into(),
        },
    );
    // A catalog route with a tool-choice record: its attempt's loop gets it.
    routes.insert(
        "ollama:qwen3.6:35b".to_owned(),
        RouteParts {
            provider: Arc::new(ShellCaller),
            vendor: "fake (ollama)".into(),
            vendor_name: "fake".into(),
            model: "m".into(),
        },
    );
    let assembler = AttemptAssembler {
        root: r.root.clone(),
        state: state.clone(),
        graph_path: state.join("graph.db"),
        indexed: false,
        ledger: ledger.clone(),
        config: WorkerConfig::with_servers(vec![echo_server()]).expect("config"),
        max_output_tokens: 1_000,
        max_checkpoints: 3,
        reasoning: Default::default(),
        read_denies: vec![state.clone()],
        workspaces: r.workspaces.clone(),
        routes,
        extra_tools: Vec::new(),
        session_line: None,
        prefixes: Arc::new(kgraf_runtime::PrefixRegistry::new()),
        machine_gate: test_machine_gate(),
        tbench_bridge_socket: None,
        dependency_refetch: crate::deps::inert_refetch(),
        skills: None,
        skill_directory: Default::default(),
        refusal_directory: Default::default(),
        permissions: Arc::new(kgraf_tools::PermissionControl::manual()),
    };
    let context = ContextConfig::for_model(200_000).expect("context");
    let mut seen = Vec::new();
    for (worker, route_name) in [("w-one", "fake:m"), ("w-two", "ollama:qwen3.6:35b")] {
        let ws = worktree(&r, worker).await;
        std::fs::write(ws.join(format!("mark-{worker}")), "x").expect("mark");
        let route = Route {
            name: route_name.into(),
            vendor: "fake".into(),
            // The route's own environment is not what the worker runs in.
            env: Arc::new(kgraf_runtime::WorkerEnv::new(
                ledger.clone(),
                Arc::new(ShellCaller),
                Arc::new(kgraf_tools::ToolRegistry::new()),
                assemble_prefix(),
            )),
            context: context.clone(),
            readiness: Arc::new(kgraf_orchestrator::AlwaysReady),
            local: false,
        };
        let built = assembler
            .build(&route, &ws, &worker.into(), &"r9/c-shell".into())
            .await
            .expect("built");
        assert_eq!(
            built.env.settings.max_checkpoints, 3,
            "the run's checkpoint ceiling reaches the assembled worker's loop settings"
        );
        // The route's tool-choice record reaches the loop: none for a route
        // outside the catalog, "ignored" for the recorded one.
        match (route_name, &built.env.settings.tool_choice) {
            ("fake:m", kgraf_runtime::ToolChoiceSupport::Unmeasured) => {}
            ("ollama:qwen3.6:35b", kgraf_runtime::ToolChoiceSupport::Ignored { record }) => {
                assert!(record.contains("Ollama 0.34.4"), "{record}")
            }
            (name, other) => panic!("{name}: {other:?}"),
        }
        assert!(built.env.tools.names().any(|n| n == "shell"));
        assert!(
            built
                .env
                .tools
                .names()
                .any(|n| n.starts_with("mcp__echoserver__")),
            "the configured MCP server is connected for this attempt"
        );
        let brief = kgraf_runtime::WorkerBrief {
            task: "r9/c-shell".into(),
            worker: worker.into(),
            instruction: "list your files".into(),
            workspace_root: ws.clone(),
            model: "m".into(),
            slices: vec![],
            skills: vec![],
            step_budget: 4,
            context: context.clone(),
            write_scope: None,
        };
        let mut handle = kgraf_runtime::spawn_worker(
            built.env.clone(),
            brief,
            kgraf_runtime::SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        let mut outputs = Vec::new();
        while let Some(ev) = handle.next_event().await {
            if let kgraf_runtime::WorkerEvent::ToolCallFinished {
                output, is_error, ..
            } = ev
            {
                outputs.push((output, is_error));
            }
        }
        if let Some(closer) = built.closer {
            closer.close().await;
        }
        // The two calls of one round finish in either order.
        assert_eq!(outputs.len(), 2, "{outputs:?}");
        let listed = outputs.iter().find(|o| !o.1).expect("ls ran");
        let denied = outputs.iter().find(|o| o.1).expect("touch refused");
        assert!(denied.0.contains("denied"), "{denied:?}");
        assert!(!ws.join("unasked.txt").exists());
        seen.push((worker, listed.0.clone()));
    }
    assert!(seen[0].1.contains("mark-w-one") && !seen[0].1.contains("mark-w-two"));
    assert!(seen[1].1.contains("mark-w-two") && !seen[1].1.contains("mark-w-one"));
}

/// A fake model that calls one named tool on its first round, then answers.
struct CallsOneTool(&'static str, serde_json::Value);

#[async_trait::async_trait]
impl Provider for CallsOneTool {
    fn adapter(&self) -> &'static str {
        "fake"
    }
    fn protocol(&self) -> kgraf_core::Protocol {
        kgraf_core::Protocol::Additive
    }
    async fn complete(
        &self,
        request: &kgraf_provider::Request,
        token: &kgraf_provider::CallToken,
    ) -> std::result::Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
        use kgraf_provider::{Content, Role, StopReason};
        let usage = kgraf_core::TokenUsage {
            uncached_input: 5,
            text_output: 1,
            ..Default::default()
        };
        token.settle(&usage, None)?;
        let first = !request.messages.iter().any(|m| m.role == Role::Assistant);
        let content = if first {
            vec![Content::ToolUse {
                id: "c0".into(),
                name: self.0.into(),
                input: self.1.clone(),
                signature: None,
            }]
        } else {
            vec![Content::text("done")]
        };
        Ok(kgraf_provider::Response {
            finish_reason: None,
            model: request.model.clone(),
            content,
            stop_reason: if first {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            usage,
            request_id: None,
        })
    }
    fn capabilities(&self) -> kgraf_provider::capability::Capabilities<'_> {
        kgraf_provider::capability::Capabilities::none()
    }
}

/// Decision 0067: with the `sessions` flag on, a worker's call to a session
/// tool reaches it (`assemble.rs`'s `builtins.extend(session_tools(line))`),
/// and every one of them, unbuilt, answers with `is_error: true` -- a model
/// reading the ordinary `ToolCallFinished.is_error` field must see this as a
/// failed call, not as text it could act on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_tool_call_with_the_flag_on_answers_as_an_error() {
    let r = repo();
    let state = r._dir.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let ledger = Arc::new(Ledger::open(&state.join("ledger.db")).expect("ledger"));
    let mut routes = BTreeMap::new();
    routes.insert(
        "fake:m".to_owned(),
        RouteParts {
            provider: Arc::new(CallsOneTool(
                "ask_user_question",
                serde_json::json!({"question": "are you sure?"}),
            )),
            vendor: "fake (fake)".into(),
            vendor_name: "fake".into(),
            model: "m".into(),
        },
    );
    let assembler = AttemptAssembler {
        root: r.root.clone(),
        state: state.clone(),
        graph_path: state.join("graph.db"),
        indexed: false,
        ledger: ledger.clone(),
        config: WorkerConfig::default(),
        max_output_tokens: 1_000,
        max_checkpoints: crate::limits::DEFAULT_MAX_CHECKPOINTS,
        reasoning: Default::default(),
        read_denies: vec![state.clone()],
        workspaces: r.workspaces.clone(),
        routes,
        extra_tools: Vec::new(),
        session_line: Some(kgraf_orchestrator::SessionLine::new()),
        prefixes: Arc::new(kgraf_runtime::PrefixRegistry::new()),
        machine_gate: test_machine_gate(),
        tbench_bridge_socket: None,
        dependency_refetch: crate::deps::inert_refetch(),
        skills: None,
        skill_directory: Default::default(),
        refusal_directory: Default::default(),
        permissions: Arc::new(kgraf_tools::PermissionControl::manual()),
    };
    let context = ContextConfig::for_model(200_000).expect("context");
    let ws = worktree(&r, "w-ask").await;
    let route = Route {
        name: "fake:m".into(),
        vendor: "fake".into(),
        env: Arc::new(kgraf_runtime::WorkerEnv::new(
            ledger.clone(),
            Arc::new(CallsOneTool("ask_user_question", serde_json::json!({}))),
            Arc::new(kgraf_tools::ToolRegistry::new()),
            assemble_prefix(),
        )),
        context: context.clone(),
        readiness: Arc::new(kgraf_orchestrator::AlwaysReady),
        local: false,
    };
    let built = assembler
        .build(&route, &ws, &"w-ask".into(), &"r9/c-ask".into())
        .await
        .expect("built");
    assert!(
        built.env.tools.names().any(|n| n == "ask_user_question"),
        "the session tools must be registered when a run carries a SessionLine"
    );
    let brief = kgraf_runtime::WorkerBrief {
        task: "r9/c-ask".into(),
        worker: "w-ask".into(),
        instruction: "ask the user".into(),
        workspace_root: ws.clone(),
        model: "m".into(),
        slices: vec![],
        skills: vec![],
        step_budget: 4,
        context,
        write_scope: None,
    };
    let mut handle = kgraf_runtime::spawn_worker(
        built.env.clone(),
        brief,
        kgraf_runtime::SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    let mut result = None;
    while let Some(ev) = handle.next_event().await {
        if let kgraf_runtime::WorkerEvent::ToolCallFinished {
            output, is_error, ..
        } = ev
        {
            result = Some((output, is_error));
        }
    }
    if let Some(closer) = built.closer {
        closer.close().await;
    }
    let (output, is_error) = result.expect("the tool call finished");
    assert!(
        is_error,
        "the stub tool must report is_error: true: {output}"
    );
    assert!(output.contains("not built yet"));
}

/// The companion to the test above, through the same `assemble()` path, with
/// the flag off: no session tool is registered, and the assembled prefix
/// digest (not a hand-built `SharedPrefix` -- the actual bytes `assemble()`
/// produces from the real built-in tools and system prompt) is pinned to a
/// literal. `session_tools.rs`'s own digest tests hash a `SharedPrefix` it
/// builds itself and never call `assemble()`; this is what proves the flag
/// change here is what a real worker environment actually sees.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_assembled_prefix_digest_with_the_flag_off_is_pinned() {
    let r = repo();
    let state = r._dir.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let ledger = Arc::new(Ledger::open(&state.join("ledger.db")).expect("ledger"));
    let mut routes = BTreeMap::new();
    routes.insert(
        "fake:m".to_owned(),
        RouteParts {
            provider: Arc::new(CallsOneTool("noop", serde_json::json!({}))),
            vendor: "fake (fake)".into(),
            vendor_name: "fake".into(),
            model: "m".into(),
        },
    );
    let assembler = AttemptAssembler {
        root: r.root.clone(),
        state: state.clone(),
        graph_path: state.join("graph.db"),
        indexed: false,
        ledger: ledger.clone(),
        config: WorkerConfig::default(),
        max_output_tokens: 1_000,
        max_checkpoints: crate::limits::DEFAULT_MAX_CHECKPOINTS,
        reasoning: Default::default(),
        read_denies: vec![state.clone()],
        workspaces: r.workspaces.clone(),
        routes,
        extra_tools: Vec::new(),
        session_line: None,
        prefixes: Arc::new(kgraf_runtime::PrefixRegistry::new()),
        machine_gate: test_machine_gate(),
        tbench_bridge_socket: None,
        dependency_refetch: crate::deps::inert_refetch(),
        skills: None,
        skill_directory: Default::default(),
        refusal_directory: Default::default(),
        permissions: Arc::new(kgraf_tools::PermissionControl::manual()),
    };
    let context = ContextConfig::for_model(200_000).expect("context");
    let ws = worktree(&r, "w-off").await;
    let route = Route {
        name: "fake:m".into(),
        vendor: "fake".into(),
        env: Arc::new(kgraf_runtime::WorkerEnv::new(
            ledger.clone(),
            Arc::new(CallsOneTool("noop", serde_json::json!({}))),
            Arc::new(kgraf_tools::ToolRegistry::new()),
            assemble_prefix(),
        )),
        context: context.clone(),
        readiness: Arc::new(kgraf_orchestrator::AlwaysReady),
        local: false,
    };
    let built = assembler
        .build(&route, &ws, &"w-off".into(), &"r9/c-off".into())
        .await
        .expect("built");
    for tool in kgraf_orchestrator::SESSION_TOOL_NAMES {
        assert!(
            !built.env.tools.names().any(|n| n == tool),
            "{tool} must not be registered with the sessions flag off"
        );
    }
    let digest = built.env.prefix.digest().as_str().to_owned();
    if let Some(closer) = built.closer {
        closer.close().await;
    }
    // Pinned by decision 0084: the shell tool's description
    // now names the build and test allowlist, the system prompt says those
    // commands run without approval and that a `<verifier_refusal>` block is
    // data, and the `apply_patch` description was rewritten in the same
    // merge. One deliberate cache reset. Decision 0093 re-pinned it once more:
    // the system prompt now tells a refused worker to report the shortfall and
    // names the `<worker_shortfall>` block as data. Decision 0095 re-pinned it
    // again: every worker lists the `recall_history` tool. Re-pinned once
    // more when the shell tool description began naming the build commands'
    // options, so a model no longer reads the bare names as "without flags",
    // and the system prompt said kGraf commits the worktree itself. Re-pinned
    // again when the `apply_patch` description said a patch without its
    // `*** Begin Patch` line is applied as if it had one. Re-pinned again
    // (decision 0094, amended again 2026-10-02) when the shell tool
    // description said the allowlisted cargo commands may also be given as a
    // `sh -c` script after one `cd` into the worktree: a worker that read only
    // the argv form took `cd crate && cargo check` for a command needing
    // approval. Re-pinned again (decision 0084, amended 2026-10-06) when the
    // default allowlist took every language's build and test commands, which
    // the shell tool description names (measured: with the cargo-only list
    // the digest is the previous one).
    //
    // One pin per platform and Windows backend: on Windows the shell tool's
    // description says on purpose that `sh` and `bash` run with Git Bash,
    // and under the token sandbox also that the network is open (it cannot
    // deny it), so its prefix differs; under the sandbox user the network is
    // denied and that sentence is absent. Within one machine the definition
    // is still byte-identical across a run's workers (0043). The Windows
    // values are the ones the hosted Windows self-test reported, the token
    // sandbox's in its baseline and the sandbox user's in the self-test's re-run as
    // that user; nothing that builds the prefix has changed since but the
    // platform-independent parts this test also pins on Unix.
    //
    // Re-pinned (decision 0084, amended 2026-10-09) when the shell tool's
    // description took its sentence on time limits and its input schema took
    // `timeout_secs`. Both Windows values were measured on macOS with the
    // Windows sentences forced on (the Git Bash sentence; and, for the token
    // sandbox, the network left open); the same forcing on the previous code
    // gave both values the hosted Windows self-test reported, exactly.
    //
    // Re-pinned (decision 0099) when every worker began listing the
    // `replace_in_file` tool and the system prompt named it. The Windows
    // values were measured the same way, with the Windows sentences forced on;
    // the same forcing with the tool and the prompt sentence removed gave the
    // previous Windows values exactly.
    //
    // Re-pinned when the `apply_patch` description said a body line with no
    // prefix is read as a context line, as the parser now reads it. The
    // Windows values were measured the same way; the same forcing with the
    // previous description gave the previous Windows values exactly.
    let pinned = if cfg!(windows) {
        if kgraf_sandbox::CheckNetwork::current()
            .shell_notice()
            .is_some()
        {
            "bbbc8cad796883ca4bac71f71d903d8f0fb14a10fd568b6b8f10289b0c4a3b5a"
        } else {
            "6bd67f92bc79fa479c44ab3db5480f4116d2f8b96cfd4a923a2ee8a83e72a5dc"
        }
    } else {
        "22d24b93d4af72c5cf73e1d20c2a13fdce96b29e7982e24555eb1ea2c32e9f05"
    };
    assert_eq!(
        digest, pinned,
        "a change here means every worker's cache resets even with the \
         sessions flag off, which nothing in decision 0067 may cause \
         (re-pinned for the intentional `apply_patch` description \
         rewrite; the tool's description is part of the assembled prefix the digest is taken over, so this is the \
         one change expected to move it)"
    );
}

/// The real MCP server over stdio, as `kgraf worker`'s tests use it.
fn echo_server() -> kgraf_mcp::McpServerConfig {
    kgraf_mcp::McpServerConfig::new(
        "echoserver",
        kgraf_mcp::McpTransport::Stdio {
            command: echo_binary().to_string_lossy().into_owned(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        },
    )
    .expect("valid")
}

/// The real MCP server's binary, built once per test process.
fn echo_binary() -> &'static PathBuf {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        escargot::CargoBuild::new()
            .bin("mcp-echo-server")
            .manifest_path(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../kgraf-mcp-test-server/Cargo.toml"
            ))
            .current_release()
            .current_target()
            .run()
            .expect("build mcp-echo-server")
            .path()
            .to_path_buf()
    })
}

/// An MCP tool allowed with `kgraf allow` runs in an
/// unattended orchestrate delegate; before that, the same call is denied.
/// The delegate's environment is the one `orchestrate` assembles
/// (`AttemptAssembler`), with the real echo server and worker loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_mcp_tool_allowed_with_kgraf_allow_runs_in_an_unattended_delegate() {
    let r = repo();
    let state = r._dir.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let ledger = Arc::new(Ledger::open(&state.join("ledger.db")).expect("ledger"));
    let assembler = |ledger: Arc<Ledger>| {
        let mut routes = BTreeMap::new();
        routes.insert(
            "fake:m".to_owned(),
            RouteParts {
                provider: Arc::new(McpCaller),
                vendor: "fake-mcp (fake)".into(),
                vendor_name: "fake-mcp".into(),
                model: "m".into(),
            },
        );
        AttemptAssembler {
            root: r.root.clone(),
            state: state.clone(),
            graph_path: state.join("graph.db"),
            indexed: false,
            ledger,
            config: WorkerConfig::with_servers(vec![echo_server()]).expect("config"),
            max_output_tokens: 1_000,
            max_checkpoints: crate::limits::DEFAULT_MAX_CHECKPOINTS,
            reasoning: Default::default(),
            read_denies: vec![state.clone()],
            workspaces: r.workspaces.clone(),
            routes,
            extra_tools: Vec::new(),
            session_line: None,
            prefixes: Arc::new(kgraf_runtime::PrefixRegistry::new()),
            machine_gate: test_machine_gate(),
            tbench_bridge_socket: None,
            dependency_refetch: crate::deps::inert_refetch(),
            skills: None,
            skill_directory: Default::default(),
            refusal_directory: Default::default(),
            permissions: Arc::new(kgraf_tools::PermissionControl::manual()),
        }
    };
    let context = ContextConfig::for_model(200_000).expect("context");
    let mut outcomes = Vec::new();
    for (allowed, worker) in [(false, "w-deny"), (true, "w-allow")] {
        if allowed {
            let project = kgraf_core::plain_path::canonicalize(&r.root).expect("root");
            crate::worker::ask::allow_mcp_tool(
                &state,
                &project.to_string_lossy(),
                "mcp__echoserver__echo",
                "test",
                &["echoserver".to_owned()],
            )
            .expect("allow");
        }
        let ws = worktree(&r, worker).await;
        let a = assembler(ledger.clone());
        let route = Route {
            name: "fake:m".into(),
            vendor: "fake".into(),
            env: Arc::new(kgraf_runtime::WorkerEnv::new(
                ledger.clone(),
                Arc::new(McpCaller),
                Arc::new(kgraf_tools::ToolRegistry::new()),
                assemble_prefix(),
            )),
            context: context.clone(),
            readiness: Arc::new(kgraf_orchestrator::AlwaysReady),
            local: false,
        };
        let built = a
            .build(&route, &ws, &worker.into(), &"r9/c-mcp".into())
            .await
            .expect("built");
        let brief = kgraf_runtime::WorkerBrief {
            task: "r9/c-mcp".into(),
            worker: worker.into(),
            instruction: "echo".into(),
            workspace_root: ws.clone(),
            model: "m".into(),
            slices: vec![],
            skills: vec![],
            step_budget: 4,
            context: context.clone(),
            write_scope: None,
        };
        let mut handle = kgraf_runtime::spawn_worker(
            built.env.clone(),
            brief,
            kgraf_runtime::SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        let mut finished = Vec::new();
        while let Some(ev) = handle.next_event().await {
            if let kgraf_runtime::WorkerEvent::ToolCallFinished {
                output, is_error, ..
            } = ev
            {
                finished.push((output, is_error));
            }
        }
        if let Some(closer) = built.closer {
            closer.close().await;
        }
        outcomes.push(finished);
    }
    let denied = &outcomes[0];
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert!(
        denied[0].1 && denied[0].0.contains("kgraf allow"),
        "{denied:?}"
    );
    let allowed = &outcomes[1];
    assert_eq!(allowed.len(), 1, "{allowed:?}");
    assert_eq!(allowed[0], ("ping".to_owned(), false));
}

/// A fake model: round 0 calls the echo MCP tool; round 1 answers.
struct McpCaller;

#[async_trait::async_trait]
impl Provider for McpCaller {
    fn adapter(&self) -> &'static str {
        "fake"
    }
    fn protocol(&self) -> kgraf_core::Protocol {
        kgraf_core::Protocol::Additive
    }
    async fn complete(
        &self,
        request: &kgraf_provider::Request,
        token: &kgraf_provider::CallToken,
    ) -> std::result::Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
        use kgraf_provider::{Content, Role, StopReason};
        let usage = kgraf_core::TokenUsage {
            uncached_input: 10,
            text_output: 1,
            ..Default::default()
        };
        token.settle(&usage, None)?;
        let first = !request.messages.iter().any(|m| m.role == Role::Assistant);
        let content = if first {
            vec![Content::ToolUse {
                id: "c0".into(),
                name: "mcp__echoserver__echo".into(),
                input: serde_json::json!({"message": "ping"}),
                signature: None,
            }]
        } else {
            vec![Content::text("done")]
        };
        Ok(kgraf_provider::Response {
            finish_reason: None,
            model: request.model.clone(),
            content,
            stop_reason: if first {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            usage,
            request_id: None,
        })
    }
    fn capabilities(&self) -> kgraf_provider::capability::Capabilities<'_> {
        kgraf_provider::capability::Capabilities::none()
    }
}

fn assemble_prefix() -> kgraf_runtime::PrefixSnapshot {
    kgraf_runtime::PrefixSnapshot::new(
        kgraf_runtime::SharedPrefix {
            vendor: "route-only".into(),
            model: "m".into(),
            system: vec![],
            tools: vec![],
            skill_catalog: vec![],
            slice_pool: vec![],
            mcp_instructions: vec![],
        },
        &kgraf_runtime::NeutralPrefixEncoder,
    )
    .expect("prefix")
}

/// A fake model: round 0 lists the worktree and tries an unapproved write;
/// round 1 answers.
struct ShellCaller;

#[async_trait::async_trait]
impl Provider for ShellCaller {
    fn adapter(&self) -> &'static str {
        "fake"
    }
    fn protocol(&self) -> kgraf_core::Protocol {
        kgraf_core::Protocol::Additive
    }
    async fn complete(
        &self,
        request: &kgraf_provider::Request,
        token: &kgraf_provider::CallToken,
    ) -> std::result::Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
        use kgraf_provider::{Content, Role, StopReason};
        let usage = kgraf_core::TokenUsage {
            uncached_input: 10,
            text_output: 1,
            ..Default::default()
        };
        token.settle(&usage, None)?;
        let first = !request.messages.iter().any(|m| m.role == Role::Assistant);
        // Programs the platform has: `ls` and `touch` are not Windows
        // programs, and the ones in Git's `usr\bin` are MSYS2 programs that
        // do not start in the sandbox.
        let (list, write): (&[&str], &[&str]) = if cfg!(windows) {
            (
                &["cmd", "/d", "/c", "dir"],
                &["cmd", "/d", "/c", "type nul > unasked.txt"],
            )
        } else {
            (&["ls"], &["touch", "unasked.txt"])
        };
        let content = if first {
            vec![
                Content::ToolUse {
                    id: "c0".into(),
                    name: "shell".into(),
                    input: serde_json::json!({ "command": list }),
                    signature: None,
                },
                Content::ToolUse {
                    id: "c1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({ "command": write }),
                    signature: None,
                },
            ]
        } else {
            vec![Content::text("done")]
        };
        Ok(kgraf_provider::Response {
            finish_reason: None,
            model: request.model.clone(),
            content,
            stop_reason: if first {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            usage,
            request_id: None,
        })
    }
    fn capabilities(&self) -> kgraf_provider::capability::Capabilities<'_> {
        kgraf_provider::capability::Capabilities::none()
    }
}

/// What each failed check of a run said, from its trace: the detail of every
/// `verdict` event that did not pass, its first lines each. A test that
/// expects a run to complete prints it beside the report, whose held reason
/// says only how many attempts failed, so a failure the platform caused (a
/// check program that could not start) names its cause.
pub(crate) fn failed_verdicts(trace: &str) -> String {
    let mut out = Vec::new();
    for line in trace.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event = v.get("event").unwrap_or(&v);
        if event.get("event").and_then(|e| e.as_str()) != Some("verdict")
            || event.get("passed").and_then(|p| p.as_bool()) != Some(false)
        {
            continue;
        }
        let detail = event.get("detail").and_then(|d| d.as_str()).unwrap_or("");
        let head: Vec<&str> = detail.lines().filter(|l| !l.is_empty()).take(4).collect();
        let head = head.join(" | ");
        if !out.contains(&head) {
            out.push(head);
        }
    }
    if out.is_empty() {
        "failed checks: none in the trace".to_owned()
    } else {
        format!("failed checks:\n{}", out.join("\n"))
    }
}

#[test]
fn failed_verdicts_name_what_each_failed_check_said_once() {
    let trace = concat!(
        r#"{"seq":1,"event":{"event":"verdict","contract":"a","passed":false,"detail":"`true` failed: $ true\n\n[exit code: 0xc0000142]\n[sandbox] note"}}"#,
        "\n",
        r#"{"seq":2,"event":{"event":"verdict","contract":"a","passed":false,"detail":"`true` failed: $ true\n\n[exit code: 0xc0000142]\n[sandbox] note"}}"#,
        "\n",
        r#"{"seq":3,"event":{"event":"verdict","contract":"b","passed":true,"detail":"ok"}}"#,
        "\n",
        r#"{"seq":4,"event":{"event":"notice","detail":"failed"}}"#,
    );
    assert_eq!(
        failed_verdicts(trace),
        "failed checks:\n`true` failed: $ true | [exit code: 0xc0000142] | [sandbox] note"
    );
    assert_eq!(failed_verdicts(""), "failed checks: none in the trace");
}

fn gate_runner(r: &Repo, commands: &[&str], timeout: Duration) -> CommandGateRunner {
    CommandGateRunner {
        commands: commands.iter().map(|c| (*c).to_owned()).collect(),
        manager: r.workspaces.manager.clone(),
        merge_back: r.merge_back.clone(),
        read_denies: Vec::new(),
        timeout,
        output_cap: 2_000,
    }
}

fn request(r: &CommandGateRunner, pinned: &str) -> GateRequest {
    GateRequest {
        gate: "gate:root".into(),
        covers: vec!["w-g".into()],
        commands: r.commands.clone(),
        pinned: Some(VerifiedCommit {
            sha: pinned.to_owned(),
            verified_by: "integration branch".into(),
            verified_ms: 0,
        }),
    }
}

#[tokio::test]
async fn the_gate_runs_the_commands_in_a_worktree_of_the_pinned_integration_commit() {
    let r = repo();
    let Integration::Merged { commit } = finish(&r, "w-g", "made.txt", "ok").await else {
        panic!("merge");
    };
    let pass = gate_runner(
        &r,
        &["test -f made.txt", "echo checked"],
        Duration::from_secs(30),
    );
    // The runner names the commands it runs (the orchestrator reads them).
    assert_eq!(
        GateRunner::commands(&pass),
        vec!["test -f made.txt".to_owned(), "echo checked".to_owned()]
    );
    let v = pass.run(request(&pass, &commit)).await.expect("ran");
    assert!(v.passed, "{v:?}");
    assert!(v.by.contains(&commit));
    let fail = gate_runner(&r, &["echo boom >&2; exit 3"], Duration::from_secs(30));
    let v = fail.run(request(&fail, &commit)).await.expect("ran");
    assert!(!v.passed);
    let quoted = v.output.quote_as_data("gate_output", &[]);
    assert!(quoted.contains("boom"), "{quoted}");
    // The gate worktrees are removed after each run.
    let left: Vec<_> = std::fs::read_dir(r.workspaces.manager.base())
        .expect("base")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("gate-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn a_gate_command_past_its_timeout_is_killed_and_fails() {
    let r = repo();
    let Integration::Merged { commit } = finish(&r, "w-t", "t.txt", "ok").await else {
        panic!("merge");
    };
    let slow = gate_runner(&r, &["sleep 30"], Duration::from_millis(500));
    let started = std::time::Instant::now();
    let v = slow.run(request(&slow, &commit)).await.expect("ran");
    assert!(!v.passed);
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(
        v.output.quote_as_data("o", &[]).contains("killed after"),
        "{v:?}"
    );
}

/// A gate command's descendant that escapes the process group (its own
/// `setsid`, as in `a_check_process_that_escapes_its_group_fails_the_verification`)
/// is not reached by the group kill, so nothing stops a CPU-bound spin inside
/// it except `RLIMIT_CPU`. The escapee spins for 1,200 s of real time, past
/// the test's safety cap, and the group kill cannot reach it, so its death is
/// attributed to the limit, and CPU time far past the limit fails the test.
///
/// Nothing here is bounded by wall time except generous safety caps; each
/// step waits for an event, and the verdict is read from CPU time, which is
/// what the limit counts:
///
/// 1. The escape. The parent waits until the child has written its pid, which
///    it does after `setsid`, so the shell exits only once the child escaped.
///    Under load a sandboxed `python3` could still take longer to start than
///    the shell's 5 s wall budget (seen with two thin-LTO release builds
///    running), so the budget killed the child inside the group and no pid
///    was recorded. That run proves nothing about the limit, so the test runs
///    the command again until a child has escaped (`ESCAPE_ATTEMPTS`), and it
///    starts the real interpreter directly, not the macOS `xcrun` shim, which
///    was most of the start time. The 5 s budget stays: the CPU limit is
///    derived from it (`cpu_rlimit_seconds`), and a larger budget would make
///    the spinner run for minutes.
/// 2. The death. The test polls the escapee's own CPU time until it is gone.
///    It fails if the escapee is seen alive with clearly more CPU time than
///    the limit allows: that is a limit that did nothing, whatever the load.
///    `SAFETY_CAP` only stops a hung test; on a loaded machine the spinner
///    gets a small share of a core and takes long in wall time to spend its
///    CPU seconds.
///
/// Skipped where the sandbox gives the command its own PID namespace (Linux
/// with the namespaces the kernel allows; see [`sandbox_has_pid_namespace`]).
/// The premise fails there twice: the kernel kills everything left in the
/// namespace when the shell exits, so the descendant does not escape and it
/// is not `RLIMIT_CPU` that stops it; and the pid it records is its namespace
/// pid, so a host-side `kill(pid, 0)` would probe an unrelated process. The
/// limit is still set there; the namespace teardown is the stronger bound.
#[cfg(unix)]
#[tokio::test]
async fn an_escaped_cpu_spinning_gate_command_is_stopped_by_the_cpu_limit() {
    /// Runs of the command until one child has escaped.
    const ESCAPE_ATTEMPTS: u32 = 10;
    /// A hung test's stop, not the verdict: at a twentieth of one core the
    /// spinner spends its 10 CPU seconds in 200 s.
    const SAFETY_CAP: Duration = Duration::from_secs(900);
    /// CPU seconds past the limit that mean the limit did nothing: the kernel
    /// checks it about once a second, and `ps` rounds.
    const CPU_MARGIN: f64 = 5.0;
    // The escapee's own end (`end = time.time() + 1200` in the script) lies
    // past `SAFETY_CAP`. The test sends it no signal: once it is gone its pid
    // could name another process.
    const _: () = assert!(1200 > SAFETY_CAP.as_secs());

    if sandbox_has_pid_namespace() {
        eprintln!("skipped: the sandbox runs the command in its own PID namespace here");
        return;
    }
    let r = repo();
    let c = named("b.txt", "w-cpu");
    let ws_path = prepared(&r, &c, "w-cpu").await;
    std::fs::write(ws_path.join("b.txt"), "b\n").expect("write");
    let live = r.workspaces.get(&ws_path).expect("live");
    let marker = live.root().join("escaped.pid");
    // The real interpreter, resolved outside the sandbox: the macOS
    // `/usr/bin/python3` shim runs `xcrun` on every start.
    let python = std::process::Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|p| !p.is_empty() && !p.contains('\''))
        .unwrap_or_else(|| "python3".to_owned());
    // The child renames its pid file into place, so the parent (and the test)
    // never read a partial one.
    let script = format!(
        "'{python}' -c 'import os, time\n\
if os.fork() == 0:\n    \
os.setsid()\n    \
open(\"escaped.tmp\", \"w\").write(str(os.getpid()))\n    \
os.rename(\"escaped.tmp\", \"escaped.pid\")\n    \
end = time.time() + 1200\n    \
while time.time() < end:\n        \
pass\n\
while not os.path.exists(\"escaped.pid\"):\n    \
time.sleep(0.01)\n' >/dev/null 2>&1"
    );
    // The shell's wall budget; the CPU limit that bounds the escapee is
    // derived from it (`cpu_rlimit_seconds`: twice it, 10 s).
    let timeout = Duration::from_secs(5);
    let cpu_seconds = kgraf_sandbox::lifecycle::cpu_rlimit_seconds(timeout);
    let mut attempts = 0;
    let text = loop {
        attempts += 1;
        let _ = run_confined(&live, &r.root, &[], &script, timeout, 2_000, None).expect("ran");
        // A child that escaped just as the budget ran out writes its pid a
        // moment later: look for a little while before running again.
        let seen = std::time::Instant::now();
        while !marker.exists() && seen.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if let Ok(text) = std::fs::read_to_string(&marker) {
            break text;
        }
        assert!(
            attempts < ESCAPE_ATTEMPTS,
            "no child escaped in {ESCAPE_ATTEMPTS} runs: the machine is too loaded for \
             `python3` to start within the shell's {timeout:?}"
        );
    };
    let pid: i32 = text.trim().parse().expect("a pid");
    let started = std::time::Instant::now();
    // SAFETY: signal 0 only probes whether `pid` is alive; it has no other effect.
    while unsafe { libc::kill(pid, 0) } == 0 {
        if let Some(used) = cpu_time_of(pid) {
            assert!(
                used <= cpu_seconds as f64 + CPU_MARGIN,
                "the escaped, CPU-spinning descendant (pid {pid}) ran {used:.1} s of CPU, \
                 past RLIMIT_CPU ({cpu_seconds} s): the limit did not stop it"
            );
        }
        assert!(
            started.elapsed() < SAFETY_CAP,
            "the escaped descendant (pid {pid}) was still alive after {SAFETY_CAP:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A process's CPU time in seconds, as `ps` reports it (`[[dd-]hh:]mm:ss[.ff]`),
/// or `None` once it is gone or unreadable.
#[cfg(unix)]
fn cpu_time_of(pid: i32) -> Option<f64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if text.is_empty() {
        return None;
    }
    let (days, rest) = match text.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().ok()?, r.to_owned()),
        None => (0.0, text),
    };
    let mut seconds = 0.0;
    for part in rest.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86_400.0 + seconds)
}

/// The production layout, end to end: repository in a path with spaces,
/// state in `<root>/.kgraf` and read-denied as `orchestrate` denies it,
/// worktrees and scratch directories beneath it. A worker's change to a small
/// cargo crate passes the sandboxed verifier's `cargo test`, merges, and the
/// sandboxed quality gate's `cargo test` passes on the integration commit.
/// Without the Seatbelt fix both fail on macOS with "could not parse/generate
/// dep info ... Operation not permitted". The gate and verifier tests do not
/// cover it because their worktrees are not under a denied directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cargo_gates_pass_in_worktrees_under_the_read_denied_state_directory() {
    let dir = tempfile::Builder::new()
        .prefix("Want To Develop ")
        .tempdir()
        .expect("dir");
    let root = dir.path().join("An App");
    let state = root.join(".kgraf");
    let files = [
        (".gitignore", "target/\n.kgraf/\n"),
        (
            "Cargo.toml",
            "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        // Committed, as a binary project should. A project without one is
        // `a_lockfile_the_check_generates_is_reported_and_never_merged`.
        (
            "Cargo.lock",
            "# This file is automatically @generated by Cargo.\n\
             # It is not intended for manual editing.\n\
             version = 4\n\n\
             [[package]]\nname = \"tiny\"\nversion = \"0.1.0\"\n",
        ),
        (
            "src/lib.rs",
            "pub fn two() -> u8 { 2 }\n#[test]\nfn is_two() { assert_eq!(two(), 2); }\n",
        ),
    ];
    let r = repo_in(dir, root.clone(), &state.join("worktrees"), &files);
    owner_only_modes(&root, &files);
    std::fs::write(state.join("worker.json"), "Bearer FAKE").expect("config");
    let denies = crate::worker::assemble::state_read_denies(
        &state,
        None,
        &state.join("ledger.db"),
        &state.join("runs"),
    )
    .expect("denies");
    assert!(denies.iter().any(|d| d.ends_with(".kgraf")), "{denies:?}");
    let cargo_test = "cargo test --offline";

    let c = named("src/lib.rs", "w-cargo");
    let ws = r
        .workspaces
        .prepare(&c, &"w-cargo".into())
        .await
        .expect("worktree");
    assert!(
        kgraf_core::plain_path::canonicalize(&ws)
            .expect("ws")
            .starts_with(&denies[0])
    );
    std::fs::write(
        ws.join("src/lib.rs"),
        "pub fn two() -> u8 { 1 + 1 }\n#[test]\nfn is_two() { assert_eq!(two(), 2); }\n",
    )
    .expect("write");
    let v = CommandVerifier {
        read_denies: denies.clone(),
        ..verifier(&r, cargo_test)
    };
    let verdict = v.verify(&c, &ws, &result()).await.expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    let Integration::Merged { commit } = r.workspaces.integrate(&c, &ws).await.expect("merge")
    else {
        panic!("merge");
    };
    r.workspaces.release(&ws, true).await;

    let gate = CommandGateRunner {
        read_denies: denies,
        ..gate_runner(&r, &[cargo_test], Duration::from_secs(300))
    };
    let v = gate.run(request(&gate, &commit)).await.expect("ran");
    assert!(v.passed, "{}", v.output.quote_as_data("gate_output", &[]));
}

/// The checkout root 0755 and its files 0644, whatever the umask: the Linux
/// sandbox grants cargo's reads above a worktree only from a checkout only
/// its owner can change (`kgraf_sandbox::trust`), and a fixture should
/// not pass or fail on the machine's umask.
fn owner_only_modes(root: &Path, files: &[(&str, &str)]) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let set = |p: &Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).expect("chmod")
        };
        set(root, 0o755);
        for (path, _) in files {
            set(&root.join(path), 0o644);
        }
    }
    #[cfg(not(unix))]
    let _ = (root, files);
}

/// A small cargo crate with no committed `Cargo.lock`, its worktrees under a
/// state directory the checks may not read, as `orchestrate` lays it out.
fn lockless_crate() -> (Repo, Vec<PathBuf>) {
    let dir = tempfile::Builder::new()
        .prefix("lockless ")
        .tempdir()
        .expect("dir");
    let root = dir.path().join("crate");
    let state = root.join(".kgraf");
    let files = [
        (".gitignore", "target/\n.kgraf/\n"),
        (
            "Cargo.toml",
            "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        (
            "src/lib.rs",
            "pub fn two() -> u8 { 2 }\n#[test]\nfn is_two() { assert_eq!(two(), 2); }\n",
        ),
    ];
    let r = repo_in(dir, root.clone(), &state.join("worktrees"), &files);
    owner_only_modes(&root, &files);
    let denies = crate::worker::assemble::state_read_denies(
        &state,
        None,
        &state.join("ledger.db"),
        &state.join("runs"),
    )
    .expect("denies");
    (r, denies)
}

const LIB_V2: &str =
    "pub fn two() -> u8 { 1 + 1 }\n#[test]\nfn is_two() { assert_eq!(two(), 2); }\n";

fn branch_tree(r: &Repo) -> String {
    git_out(&r.root, &["ls-tree", "-r", "--name-only", "kgraf/r-1"])
}

async fn prepared(r: &Repo, c: &OpenContract, worker: &str) -> PathBuf {
    r.workspaces
        .prepare(c, &worker.into())
        .await
        .expect("worktree")
}

/// In a project with no committed `Cargo.lock`, the acceptance check's
/// `cargo test` generates one. The scope check must not refuse the worker for
/// it ("changes outside the declared write scope: [\"Cargo.lock\"]"). The
/// scope is judged on the commit taken before the check: the attempt passes
/// and merges, the lockfile is reported as a notice, it is not on the
/// integration branch, and it is gone from the worktree. The quality gate on
/// the merged tree passes too, and leaves the branch as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lockfile_the_check_generates_is_reported_and_never_merged() {
    let (r, denies) = lockless_crate();
    let cargo_test = "cargo test --offline";
    let c = named("src/lib.rs", "w-lockless");
    let ws = prepared(&r, &c, "w-lockless").await;
    std::fs::write(ws.join("src/lib.rs"), LIB_V2).expect("write");
    let v = CommandVerifier {
        read_denies: denies.clone(),
        ..verifier(&r, cargo_test)
    };
    let verdict = v.verify(&c, &ws, &result()).await.expect("ran");
    let Verdict::Pass {
        commit: Some(_),
        notices,
        ..
    } = &verdict
    else {
        panic!("expected a pass with a commit, got {verdict:?}");
    };
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("the check `cargo test --offline` generated Cargo.lock")
            && notices[0].contains("Commit it"),
        "{notices:?}"
    );
    assert!(
        !ws.join("Cargo.lock").exists(),
        "the check's lockfile is removed from the worktree"
    );
    let Integration::Merged { commit } = r.workspaces.integrate(&c, &ws).await.expect("merge")
    else {
        panic!("merge");
    };
    r.workspaces.release(&ws, true).await;
    let tree = branch_tree(&r);
    assert!(!tree.lines().any(|p| p == "Cargo.lock"), "{tree}");
    assert_eq!(
        git_out(&r.root, &["show", "kgraf/r-1:src/lib.rs"]),
        LIB_V2.trim_end()
    );

    let gate = CommandGateRunner {
        read_denies: denies,
        ..gate_runner(&r, &[cargo_test], Duration::from_secs(300))
    };
    let g = gate.run(request(&gate, &commit)).await.expect("ran");
    assert!(g.passed, "{}", g.output.quote_as_data("gate_output", &[]));
    assert_eq!(
        git_out(&r.root, &["rev-parse", "kgraf/r-1"]),
        commit,
        "a gate never moves the branch"
    );
}

/// The same harness, the worker at fault: a file outside the declared scope
/// is still refused, and the check's own lockfile is not named in the
/// refusal. A lockfile name with no manifest beside it is the worker's write,
/// refused like any other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_write_outside_its_scope_is_still_refused_under_a_generating_check() {
    let (r, denies) = lockless_crate();
    let v = CommandVerifier {
        read_denies: denies,
        ..verifier(&r, "cargo test --offline")
    };

    let c = named("src/lib.rs", "w-stray");
    let ws = prepared(&r, &c, "w-stray").await;
    std::fs::write(ws.join("src/lib.rs"), LIB_V2).expect("write");
    std::fs::write(ws.join("src/stray.rs"), "// not declared\n").expect("write");
    match v.verify(&c, &ws, &result()).await.expect("ran") {
        Verdict::Fail { reason } => {
            assert!(
                reason.contains("outside the declared write scope"),
                "{reason}"
            );
            assert!(reason.contains("src/stray.rs"), "{reason}");
            assert!(!reason.contains("Cargo.lock"), "{reason}");
        }
        other => panic!("expected a scope failure, got {other:?}"),
    }

    let c = named("src/lib.rs", "w-nomanifest");
    let ws = prepared(&r, &c, "w-nomanifest").await;
    std::fs::create_dir_all(ws.join("sub")).expect("mkdir");
    std::fs::write(ws.join("sub/Cargo.lock"), "version = 4\n").expect("write");
    match v.verify(&c, &ws, &result()).await.expect("ran") {
        Verdict::Fail { reason } => assert!(reason.contains("sub/Cargo.lock"), "{reason}"),
        other => panic!("a lockfile with no manifest beside it is a write: {other:?}"),
    }
}

/// The worker's own shell generated the lockfile (it ran `cargo build`), out
/// of scope, beside `Cargo.toml`, with none in the base: left out of the
/// commit, removed before the check (the check never runs against dependency
/// versions or sources the worker chose and nothing merges), reported "made by
/// the worker", and not merged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lockfile_the_workers_run_generated_is_left_out_with_a_notice() {
    let (r, denies) = lockless_crate();
    let c = named("src/lib.rs", "w-ownlock");
    let ws = prepared(&r, &c, "w-ownlock").await;
    std::fs::write(ws.join("src/lib.rs"), LIB_V2).expect("write");
    std::fs::write(ws.join("Cargo.lock"), "# made by the worker's build\n").expect("write");
    let v = CommandVerifier {
        read_denies: denies,
        ..verifier(&r, "! grep -qs worker Cargo.lock")
    };
    let verdict = v.verify(&c, &ws, &result()).await.expect("ran");
    let Verdict::Pass { notices, .. } = &verdict else {
        panic!("{verdict:?}");
    };
    assert_eq!(notices.len(), 1, "reported once: {notices:?}");
    assert!(
        notices[0].contains("Cargo.lock (made by the worker)"),
        "{notices:?}"
    );
    assert!(!ws.join("Cargo.lock").exists());
    assert!(matches!(
        r.workspaces.integrate(&c, &ws).await.expect("merge"),
        Integration::Merged { .. }
    ));
    let tree = branch_tree(&r);
    assert!(!tree.lines().any(|p| p == "Cargo.lock"), "{tree}");
    assert!(tree.lines().any(|p| p == "src/lib.rs"), "{tree}");
}

/// Never left out: a lockfile the contract declares (committed and merged),
/// and a change to a lockfile the base tracks (the worker's write, refused
/// when out of scope).
#[tokio::test]
async fn a_declared_or_tracked_lockfile_is_never_left_out() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let r = repo_in(
        dir,
        root,
        &base,
        &[
            ("Cargo.toml", "[package]\nname = \"t\"\n"),
            ("sub/Cargo.toml", "[package]\nname = \"s\"\n"),
            ("sub/Cargo.lock", "version = 4\n"),
        ],
    );
    let mut c = named("Cargo.lock", "w-declared");
    c.writes.push(kgraf_orchestrator::WriteTarget {
        path: "a.txt".into(),
        node: None,
    });
    let ws = prepared(&r, &c, "w-declared").await;
    std::fs::write(ws.join("Cargo.lock"), "version = 4\n").expect("write");
    std::fs::write(ws.join("a.txt"), "a\n").expect("write");
    let verdict = verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    let Verdict::Pass { notices, .. } = &verdict else {
        panic!("{verdict:?}");
    };
    assert!(notices.is_empty(), "{notices:?}");
    r.workspaces.integrate(&c, &ws).await.expect("merge");
    assert!(branch_tree(&r).lines().any(|p| p == "Cargo.lock"));

    let c = named("a.txt", "w-tracked");
    let ws = prepared(&r, &c, "w-tracked").await;
    std::fs::write(ws.join("sub/Cargo.lock"), "version = 4\n# edited\n").expect("write");
    match verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran")
    {
        Verdict::Fail { reason } => assert!(reason.contains("sub/Cargo.lock"), "{reason}"),
        other => panic!("a tracked lockfile's change is the worker's: {other:?}"),
    }
}

/// Bytecode the worker's run compiled into an unignored `__pycache__`: left
/// out of the commit, removed before the check (the check sees the committed
/// sources only), reported "made by the worker". Bytecode the contract
/// declares is the worker's write and is committed.
#[tokio::test]
async fn bytecode_the_workers_run_left_is_left_out_and_removed_before_the_check() {
    let r = repo();
    let c = named("pkg/mod.py", "w-pyc");
    let ws = prepared(&r, &c, "w-pyc").await;
    std::fs::create_dir_all(ws.join("pkg/__pycache__")).expect("mkdir");
    std::fs::write(ws.join("pkg/mod.py"), "X = 1\n").expect("write");
    std::fs::write(ws.join("pkg/__pycache__/mod.cpython-39.pyc"), "b").expect("write");
    std::fs::write(ws.join("pkg/__pycache__/mod.opt-1.pyo"), "b").expect("write");
    let verdict = verifier(&r, "test ! -e pkg/__pycache__")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    let Verdict::Pass { notices, .. } = &verdict else {
        panic!("{verdict:?}");
    };
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("2 Python bytecode file(s) (made by the worker)"),
        "{notices:?}"
    );
    r.workspaces.integrate(&c, &ws).await.expect("merge");
    let tree = branch_tree(&r);
    assert!(!tree.contains("__pycache__"), "{tree}");
    assert!(tree.lines().any(|p| p == "pkg/mod.py"), "{tree}");

    let c = named("pkg/__pycache__/m.cpython-39.pyc", "w-pyc-declared");
    let ws = prepared(&r, &c, "w-pyc-declared").await;
    std::fs::create_dir_all(ws.join("pkg/__pycache__")).expect("mkdir");
    std::fs::write(ws.join("pkg/__pycache__/m.cpython-39.pyc"), "b").expect("write");
    let verdict = verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    r.workspaces.integrate(&c, &ws).await.expect("merge");
    assert!(
        branch_tree(&r)
            .lines()
            .any(|p| p == "pkg/__pycache__/m.cpython-39.pyc")
    );
}

/// A check that rewrites a committed file checked a tree that would not be
/// merged. The verification fails naming the files, and the worktree is back
/// to the commit.
#[tokio::test]
async fn a_check_that_changes_a_committed_file_fails_naming_it() {
    let r = repo();
    let c = named("b.txt", "w-rewrite");
    let ws = prepared(&r, &c, "w-rewrite").await;
    std::fs::write(ws.join("b.txt"), "worker\n").expect("write");
    let check = "echo check >> b.txt && rm README";
    match verifier(&r, check)
        .verify(&c, &ws, &result())
        .await
        .expect("ran")
    {
        Verdict::Fail { reason } => {
            assert!(
                reason.contains("changed or deleted 2 committed file(s)")
                    && reason.contains("README")
                    && reason.contains("b.txt")
                    && reason.contains("would differ"),
                "{reason}"
            );
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(ws.join("b.txt")).expect("read"),
        "worker\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("README")).expect("read"),
        "hello\n"
    );
}

/// A check whose output shows rustc's abort for its diagnostic lock, the
/// failure of every cargo build a Windows sandboxed command runs, fails with
/// the sandbox's line that says why, so the reason does not read as rustc's
/// own crash. The check only prints the text rustc prints on Windows.
#[tokio::test]
async fn a_check_that_fails_on_a_named_object_says_why() {
    let r = repo();
    let c = named("b.txt", "w-named");
    let ws = prepared(&r, &c, "w-named").await;
    std::fs::write(ws.join("b.txt"), "worker\n").expect("write");
    let check = "echo 'failed to create global mutex named `rustc_errors`' >&2; exit 101";
    match verifier(&r, check)
        .verify(&c, &ws, &result())
        .await
        .expect("ran")
    {
        Verdict::Fail { reason } => {
            // The line is added only where this machine's launch path cannot
            // make the named objects a build needs. Where it can, the same
            // output is a failure of the check itself and must not be
            // explained away (`kgraf_sandbox::sandbox_user::cargo_runs`). The
            // gate's own test pins both answers.
            if kgraf_sandbox::sandbox_user::cargo_runs() {
                assert!(!reason.contains("[sandbox]"), "{reason}");
            } else {
                assert!(
                    reason.contains("[sandbox] rustc could not make its named mutex")
                        && reason.contains("did not fail on its own"),
                    "{reason}"
                );
            }
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

/// What a check creates goes, whatever its shape, and nothing outside the
/// worktree is touched: files in new directories, a link to a file outside
/// (the link goes, not its target), a nested repository (git lists it as
/// `nested/` and does not enter it), and a new `.gitignore` that ignores
/// everything, itself included, with a file it would hide.
///
/// The link's target is outside the worktree in the repository's shared git
/// folder, which every backend lets the check read (git in the worktree
/// needs it), and not in a folder outside the repository: the Windows
/// sandbox user cannot read such a folder, and Git Bash's `ln -s` then
/// fails before the link exists ("Not a directory", Windows self-test run
/// 11), so the test measured the read confinement instead of the cleanup.
///
/// The nested repository is written as files (`HEAD`, `objects/`, `refs/`:
/// what git checks to call a folder a repository), not made with `git init`.
/// Git writes a ref only after it reads the attributes of every folder above
/// it, and the Windows sandbox user cannot read those of a folder in the
/// owner's profile, where a test's temporary folder is: `git init` there
/// fails with "cannot lock ref 'HEAD'", and the test measured that instead
/// of the cleanup. The escape test
/// `the_attributes_of_each_folder_above_the_tree_are_measured` measures it.
#[tokio::test]
async fn everything_a_check_creates_is_removed_and_nothing_outside() {
    let r = repo();
    let c = named("b.txt", "w-shapes");
    let ws = prepared(&r, &c, "w-shapes").await;
    std::fs::write(ws.join("b.txt"), "worker\n").expect("write");
    let target = r.root.join(".git").join("kgraf-test-keep.txt");
    std::fs::write(&target, "keep\n").expect("write");
    let check = format!(
        "mkdir -p cache/deep && echo c > cache/deep/c.bin && ln -s '{}' link && \
         mkdir -p nested/.git/objects nested/.git/refs && \
         printf 'ref: refs/heads/main\\n' > nested/.git/HEAD && echo n > nested/n.txt && \
         mkdir hide && printf '*\\n' > hide/.gitignore && echo h > hide/hidden.txt",
        target.display()
    );
    let verdict = verifier(&r, &check)
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    let Verdict::Pass { notices, .. } = &verdict else {
        panic!("{verdict:?}");
    };
    assert_eq!(notices.len(), 1, "{notices:?}");
    for p in [
        "cache/deep/c.bin",
        "link",
        "nested/",
        "hide/.gitignore",
        "hide/hidden.txt",
    ] {
        assert!(
            notices[0].contains(p) || notices[0].contains("more"),
            "{p}: {notices:?}"
        );
    }
    // Git took `nested/` for a repository and did not list what is in it
    // (the notice also quotes the check's command, which names the file).
    let listed = notices[0]
        .split_once("git would stage: ")
        .map_or("", |(_, l)| l);
    assert!(
        listed.contains("nested/") && !listed.contains("nested/n.txt"),
        "{notices:?}"
    );
    for p in ["cache", "link", "nested", "hide"] {
        assert!(ws.join(p).symlink_metadata().is_err(), "{p} remains");
    }
    assert_eq!(std::fs::read_to_string(&target).expect("read"), "keep\n");
    assert!(matches!(
        r.workspaces.integrate(&c, &ws).await.expect("merge"),
        Integration::Merged { .. }
    ));
    assert_eq!(git_out(&r.root, &["show", "kgraf/r-1:b.txt"]), "worker");
}

/// A `.GitIgnore` that ignores everything. On a
/// case-insensitive volume (macOS by default, `core.ignorecase`) git reads it
/// as `.gitignore` and it hides itself and the file beside it; the listing
/// matches the name without case, so both are found and removed. On a
/// case-sensitive volume it is an ordinary file, listed and removed the same.
#[tokio::test]
async fn a_gitignore_spelled_in_another_case_cannot_hide_what_the_check_wrote() {
    let r = repo();
    let c = named("b.txt", "w-icase");
    let ws = prepared(&r, &c, "w-icase").await;
    std::fs::write(ws.join("b.txt"), "worker\n").expect("write");
    let check = "mkdir hide && printf '*\\n' > hide/.GitIgnore && echo h > hide/hidden.txt";
    let verdict = verifier(&r, check)
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    let Verdict::Pass { notices, .. } = &verdict else {
        panic!("{verdict:?}");
    };
    assert!(
        notices.iter().any(|n| n.contains("hide/.GitIgnore")),
        "{notices:?}"
    );
    assert!(
        ws.join("hide").symlink_metadata().is_err(),
        "hide/ remains: {:?}",
        std::fs::read_dir(ws.join("hide")).map(|d| d.count())
    );
}

/// A real background process in the check's own
/// sandbox: it keeps swapping a directory the check created for a symlink to a
/// directory outside the worktree (standing in for `$HOME`, whose
/// `.ssh/authorized_keys` the sandbox refuses the check itself). On 7fdd97d
/// the host listed `d/.ssh/authorized_keys`, then followed the swapped link
/// and deleted the file outside. Now the check's process group is killed when
/// its shell exits, and the removal never follows a link: the file outside
/// survives every round.
#[cfg(unix)]
#[tokio::test]
async fn a_background_process_cannot_steer_the_cleanup_outside_the_worktree() {
    let r = repo();
    let home = r.outside.join("home");
    std::fs::create_dir_all(home.join(".ssh")).expect("mkdir");
    let victim = home.join(".ssh/authorized_keys");
    for round in 0..24 {
        std::fs::write(&victim, "key\n").expect("write");
        let worker = format!("w-probe-{round}");
        let c = named("b.txt", &worker);
        let ws = prepared(&r, &c, &worker).await;
        std::fs::write(ws.join("b.txt"), "b\n").expect("write");
        let check = format!(
            "mkdir -p d/.ssh && echo k > d/.ssh/authorized_keys; \
             (exec >/dev/null 2>&1 </dev/null; while :; do mv d d.real; ln -s '{}' d; rm -f d; mv d.real d; done) \
             >/dev/null 2>&1 & sleep 0.3",
            home.display()
        );
        let verdict = verifier(&r, &check)
            .verify(&c, &ws, &result())
            .await
            .expect("ran");
        assert!(
            victim.is_file(),
            "round {round}: the cleanup deleted a file outside the worktree"
        );
        for p in ["d", "d.real"] {
            assert!(
                ws.join(p).symlink_metadata().is_err(),
                "round {round}: {p} remains: {verdict:?}"
            );
        }
    }
}

/// The deterministic half of the probe: a background job of the check that
/// writes after the check's shell has exited. On 7fdd97d it outlived the
/// cleanup and left its file in the worktree (where a continued worker's
/// next commit would take it as its own). Now the group is killed first.
#[cfg(unix)]
#[tokio::test]
async fn a_check_background_job_is_killed_before_the_cleanup() {
    let r = repo();
    let c = named("b.txt", "w-late");
    let ws = prepared(&r, &c, "w-late").await;
    std::fs::write(ws.join("b.txt"), "b\n").expect("write");
    let check = "(exec >/dev/null 2>&1 </dev/null; sleep 1; echo late > late.txt) & true";
    let verdict = verifier(&r, check)
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    std::thread::sleep(Duration::from_millis(2_000));
    assert!(
        !ws.join("late.txt").exists(),
        "the background job outlived the check"
    );
}

/// Whether a sandboxed command here is PID 1 of its own PID namespace: Linux
/// with the namespaces the kernel allows. There the kernel kills everything
/// left in the namespace when the command exits, so nothing escapes, and a pid
/// the command records is its namespace's: the host must never signal it (as
/// root in a container, pid 2 or 3 names an unrelated process).
#[cfg(unix)]
fn sandbox_has_pid_namespace() -> bool {
    #[cfg(target_os = "linux")]
    {
        kgraf_sandbox::linux_confinement().namespaces
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// A process that leaves the check's process group (its own session) is not
/// reached by the group kill; the hold check finds it, the verification
/// fails, and nothing is cleaned while it runs.
///
/// Linux with a PID namespace: the kernel kills the would-be escapee when the
/// check's shell (PID 1 there) exits, so the tree is quiet, the check passes,
/// and the escapee's late write never lands. Either outcome is checked by
/// what lands in the worktree, never by a host-side signal: the escapee
/// records no pid, and ends when the test writes `release` after the verdict
/// (or after `RELEASE_CAP`), writing `late.txt` as it goes.
///
/// The escapee must outlast everything that waits for a quiet tree: the group
/// kill waits up to `GROUP_EXIT_GRACE` (5 s) for the hold to free, then the
/// verifier's quiet check retries for about 1 s. A fixed lifetime (2 s, then
/// 10 s) could end inside that wait under load, the tree read quiet, and the
/// check passed; so its end is tied to the verdict instead of the clock.
///
/// The parent waits until the child has called `setsid` (the `escaped`
/// marker) before the shell exits, so the child has left the group when the
/// group kill comes; without the wait a slow child was killed inside the
/// group and the test read that as "no escape".
#[cfg(unix)]
#[tokio::test]
async fn a_check_process_that_escapes_its_group_fails_the_verification() {
    // The escapee lives until the test releases it, after the verdict is in,
    // capped so a failed test does not leave it running for long. A fixed
    // lifetime raced the verifier's quiet-tree wait under load: the escapee
    // could end first, and the tree then read quiet.
    const RELEASE_CAP: Duration = Duration::from_secs(120);
    let r = repo();
    let c = named("b.txt", "w-escape");
    let ws = prepared(&r, &c, "w-escape").await;
    std::fs::write(ws.join("b.txt"), "b\n").expect("write");
    let check = format!(
        "python3 -c 'import os, time\n\
if os.fork() == 0:\n    \
os.setsid()\n    \
open(\"escaped\", \"w\").close()\n    \
t = time.time()\n    \
while not os.path.exists(\"release\") and time.time() - t < {cap}:\n        \
time.sleep(0.05)\n    \
open(\"late.txt\", \"w\").write(\"late\")\n    \
os._exit(0)\n\
while not os.path.exists(\"escaped\"):\n    \
time.sleep(0.01)\n' >/dev/null 2>&1",
        cap = RELEASE_CAP.as_secs()
    );
    let verdict = verifier(&r, &check)
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    // Released only now, so the escapee is certainly alive while the verifier
    // looks. Then wait for its late write: it lands promptly if the escapee is
    // alive; in a PID namespace it was killed and never lands.
    std::fs::write(ws.join("release"), "").expect("release");
    let late = ws.join("late.txt");
    let wait = if sandbox_has_pid_namespace() {
        Duration::from_secs(3)
    } else {
        RELEASE_CAP
    };
    let released = std::time::Instant::now();
    while !late.exists() && released.elapsed() < wait {
        std::thread::sleep(Duration::from_millis(50));
    }
    let late = late.exists();
    if sandbox_has_pid_namespace() {
        assert!(
            verdict.passed(),
            "nothing escapes a PID namespace: {verdict:?}"
        );
        assert!(!late, "the would-be escapee outlived the namespace");
    } else {
        match verdict {
            Verdict::Fail { reason } => {
                assert!(reason.contains("left a process running"), "{reason}")
            }
            other => panic!("expected a busy failure, got {other:?}"),
        }
        assert!(
            late,
            "the escapee ran on after the check and wrote once released, as the verdict says"
        );
    }
}

/// A worker that left a process running in its worktree (a background job of
/// its shell) is not verified: its tree is not final.
#[tokio::test]
async fn a_worktree_the_worker_still_writes_is_not_snapshotted() {
    let r = repo();
    let c = named("b.txt", "w-busy");
    let ws = prepared(&r, &c, "w-busy").await;
    std::fs::write(ws.join("b.txt"), "b\n").expect("write");
    let live = r.workspaces.get(&ws).expect("live");
    let policy = kgraf_sandbox::SandboxPolicy::for_workspace(&live, kgraf_sandbox::Network::Deny);
    let mut cmd = kgraf_sandbox::sandboxed_command(&policy, "sleep", live.root()).expect("cmd");
    cmd.args(["60"]);
    let mut child = cmd.spawn().expect("spawn");
    let verdict = verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    let _ = child.kill();
    let _ = child.wait();
    match verdict {
        Verdict::Fail { reason } => assert!(reason.contains("still runs"), "{reason}"),
        other => panic!("expected a busy failure, got {other:?}"),
    }
}

/// An attempt that changed nothing still runs its check; what the check
/// generates is reported, and the attempt is empty, not a change.
#[tokio::test]
async fn an_empty_attempt_is_checked_and_what_its_check_writes_is_not_a_change() {
    let r = repo();
    let c = named("b.txt", "w-empty");
    let ws = prepared(&r, &c, "w-empty").await;
    let failed = verifier(&r, "touch gen.txt; exit 1")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    assert!(matches!(failed, Verdict::Fail { .. }), "{failed:?}");
    assert!(ws.is_dir(), "a failed check leaves the worktree");
    assert!(!ws.join("gen.txt").exists(), "without the check's output");

    // No declared write: an empty attempt on declared writes is refused.
    let mut c2 = named("b.txt", "w-empty2");
    c2.writes.clear();
    let ws = prepared(&r, &c2, "w-empty2").await;
    let verdict = verifier(&r, "touch gen.txt")
        .verify(&c2, &ws, &result())
        .await
        .expect("ran");
    let Verdict::Pass {
        commit: None,
        notices,
        ..
    } = &verdict
    else {
        panic!("expected an empty pass, got {verdict:?}");
    };
    assert!(notices[0].contains("gen.txt"), "{notices:?}");
    assert!(ws.is_dir(), "left for the integration to settle");
    assert!(matches!(
        r.workspaces.integrate(&c2, &ws).await.expect("integrate"),
        Integration::Empty
    ));
    assert!(
        !ws.exists(),
        "deleting: an empty attempt's worktree is removed"
    );
}

/// Decision 0086: an attempt that never touched a declared write target still
/// holding a `todo!()` placeholder from dispatch is refused, not passed --
/// the exact benchmark failure mode this decision closes (a check that only
/// proves the tree still builds, never that the worker did the work).
#[tokio::test]
async fn an_untouched_todo_stub_is_refused_not_passed() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let r = repo_in(
        dir,
        root,
        &base,
        &[("src/lib.rs", "pub fn parse() -> u16 {\n    todo!()\n}\n")],
    );
    let c = named("src/lib.rs", "w-stub");
    let ws = prepared(&r, &c, "w-stub").await;
    let verdict = verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    match verdict {
        Verdict::Fail { reason } => {
            assert!(reason.contains("src/lib.rs"), "{reason}");
            assert!(
                reason.contains("0081") && reason.contains("0086"),
                "{reason}"
            );
        }
        other => panic!("expected a placeholder refusal, got {other:?}"),
    }
}

/// The same shape, retried: a fresh worktree for the same contract is still
/// offered (a refusal is not a hold), so the next attempt can actually do the
/// work.
#[tokio::test]
async fn a_refused_stub_attempt_can_be_retried_with_a_fresh_worktree() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let r = repo_in(
        dir,
        root,
        &base,
        &[("src/lib.rs", "pub fn parse() -> u16 {\n    todo!()\n}\n")],
    );
    let c = named("src/lib.rs", "w-stub");
    let ws = prepared(&r, &c, "w-stub").await;
    let first = verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    assert!(!first.passed(), "{first:?}");

    let ws2 = prepared(&r, &c, "w-stub-2").await;
    std::fs::write(ws2.join("src/lib.rs"), "pub fn parse() -> u16 { 42 }\n").expect("write");
    let second = verifier(&r, "true")
        .verify(&c, &ws2, &result())
        .await
        .expect("ran");
    assert!(second.passed(), "{second:?}");
}

/// Decision 0086: a committed attempt that changed one declared target but
/// left another, a `todo!()` placeholder at dispatch, byte-identical is
/// refused. The dispatch content comes from the base commit.
#[tokio::test]
async fn a_committed_attempt_that_left_a_declared_placeholder_untouched_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let r = repo_in(
        dir,
        root,
        &base,
        &[
            ("src/a.rs", "pub fn a() -> u16 {\n    todo!()\n}\n"),
            ("src/b.rs", "pub fn b() -> u16 {\n    todo!()\n}\n"),
        ],
    );
    let mut c = named("src/a.rs", "w-partial");
    c.writes.push(kgraf_orchestrator::WriteTarget {
        path: "src/b.rs".into(),
        node: None,
    });
    let ws = prepared(&r, &c, "w-partial").await;
    std::fs::write(ws.join("src/a.rs"), "pub fn a() -> u16 { 1 }\n").expect("write");
    let verdict = verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    match verdict {
        Verdict::Fail { reason } => {
            assert!(reason.contains("src/b.rs"), "{reason}");
            assert!(!reason.contains("src/a.rs"), "{reason}");
        }
        other => panic!("expected a placeholder refusal, got {other:?}"),
    }

    // Both targets done: the same contract passes with a commit.
    let ws2 = prepared(&r, &c, "w-partial-2").await;
    std::fs::write(ws2.join("src/a.rs"), "pub fn a() -> u16 { 1 }\n").expect("write");
    std::fs::write(ws2.join("src/b.rs"), "pub fn b() -> u16 { 2 }\n").expect("write");
    let ok = verifier(&r, "true")
        .verify(&c, &ws2, &result())
        .await
        .expect("ran");
    assert!(ok.passed(), "{ok:?}");
}

/// An attempt that changed none of the files its contract declared is not
/// verified, even when its check passes on the tree as dispatched: a check
/// that already passes before the work (`cargo build` over a stub that
/// compiles) proves nothing about the work. The refusal says why, and the
/// contract is retried or held like any failed attempt.
#[tokio::test]
async fn an_attempt_that_changed_none_of_its_declared_files_is_not_verified() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let r = repo_in(
        dir,
        root,
        &base,
        &[("src/lib.rs", "pub fn parse() -> u16 { 42 }\n")],
    );
    let c = named("src/lib.rs", "w-done");
    let ws = prepared(&r, &c, "w-done").await;
    let verdict = verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("an empty attempt on declared writes must fail, got {verdict:?}");
    };
    assert!(reason.contains("changed none of"), "{reason}");
    assert!(reason.contains("src/lib.rs"), "{reason}");
}

/// A run whose first contract merged the work of a file, and whose second
/// contract declares the same file. Returns the repository (its `src/lib.rs`
/// a stub at the run's start) after contract `c1` merged the real body.
async fn repo_where_c1_delivered_the_file() -> Repo {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let r = repo_in(
        dir,
        root,
        &base,
        &[("src/lib.rs", "pub fn parse() -> u16 { todo!() }\n")],
    );
    let c1 = named("src/lib.rs", "c1");
    let ws = prepared(&r, &c1, "w-c1").await;
    std::fs::write(
        ws.join("src/lib.rs"),
        "pub fn parse() -> u16 { 42 } // real\n",
    )
    .expect("write");
    let verdict = verifier(&r, "grep -q real src/lib.rs")
        .verify(&c1, &ws, &result())
        .await
        .expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    let merged = r.workspaces.integrate(&c1, &ws).await.expect("integrate");
    assert!(matches!(merged, Integration::Merged { .. }), "{merged:?}");
    r.workspaces.release(&ws, true).await;
    r
}

/// Two contracts write one file, and the first one's merge implemented all of
/// it. The second is dispatched on a tree where its check already passes, and
/// it changes nothing. The check failed where the run started, so the earlier
/// merge is the evidence: the attempt is accepted as an empty integration,
/// with a notice that names the earlier contract and says it is unverified.
#[tokio::test]
async fn a_contract_whose_work_an_earlier_merge_delivered_is_accepted_empty_with_a_notice() {
    let r = repo_where_c1_delivered_the_file().await;
    let c2 = named("src/lib.rs", "c2");
    let ws = prepared(&r, &c2, "w-c2").await;
    let verdict = verifier(&r, "grep -q real src/lib.rs")
        .verify(&c2, &ws, &result())
        .await
        .expect("ran");
    let Verdict::Pass {
        commit: None,
        notices,
        ..
    } = &verdict
    else {
        panic!("expected an empty pass, got {verdict:?}");
    };
    let notice = notices
        .iter()
        .find(|n| n.contains("unverified"))
        .unwrap_or_else(|| panic!("no unverified notice in {notices:?}"));
    assert!(notice.contains("contract c2"), "{notice}");
    assert!(notice.contains("earlier merge"), "{notice}");
    assert!(notice.contains("(contract c1)"), "{notice}");
    // The worktree is released as an empty integration is.
    let Integration::Empty = r.workspaces.integrate(&c2, &ws).await.expect("integrate") else {
        panic!("an accepted empty attempt integrates as empty");
    };
}

/// The same two contracts with a check that also passes where the run started
/// prove nothing about the work: still refused.
#[tokio::test]
async fn an_empty_attempt_is_still_refused_when_its_check_passes_at_the_run_start() {
    let r = repo_where_c1_delivered_the_file().await;
    let c2 = named("src/lib.rs", "c2");
    let ws = prepared(&r, &c2, "w-c2").await;
    let verdict = verifier(&r, "true")
        .verify(&c2, &ws, &result())
        .await
        .expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("a check that passes at the run start proves nothing, got {verdict:?}");
    };
    assert!(reason.contains("changed none of"), "{reason}");
}

/// An earlier merge that changed other files delivered nothing of this
/// contract's: refused, even though its check passes now and failed at the run
/// start.
#[tokio::test]
async fn an_earlier_merge_of_other_files_does_not_excuse_an_empty_attempt() {
    let r = repo_where_c1_delivered_the_file().await;
    let c3 = named("src/other.rs", "c3");
    let ws = prepared(&r, &c3, "w-c3").await;
    let verdict = verifier(&r, "grep -q real src/lib.rs")
        .verify(&c3, &ws, &result())
        .await
        .expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("another file's delivery is not this contract's, got {verdict:?}");
    };
    assert!(reason.contains("changed none of"), "{reason}");
}

/// A contract with no declared write (a check-only contract) that changes
/// nothing still passes, with the notice that it integrated no change.
#[tokio::test]
async fn a_check_only_contract_that_changes_nothing_passes_with_a_notice() {
    let r = repo();
    let mut c = named("x", "w-check");
    c.writes.clear();
    let ws = prepared(&r, &c, "w-check").await;
    let verdict = verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    let Verdict::Pass {
        commit: None,
        notices,
        ..
    } = &verdict
    else {
        panic!("expected an empty pass, got {verdict:?}");
    };
    assert!(
        notices.iter().any(|n| n.contains("integrated no change")),
        "{notices:?}"
    );
}

/// A two-module Python package whose suite is the run's acceptance command.
fn python_package(dir: tempfile::TempDir) -> Repo {
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let stub = |name: &str| format!("def {name}(s):\n    raise NotImplementedError\n");
    let test = |module: &str, name: &str| {
        format!(
            "import unittest\nfrom textkit.{module} import {name}\n\n\nclass T(unittest.TestCase):\n    \
             def test_it(self):\n        self.assertEqual({name}('Ab'), 'ab')\n"
        )
    };
    let files = [
        ("textkit/__init__.py".to_owned(), String::new()),
        ("textkit/slug.py".to_owned(), stub("slugify")),
        ("textkit/counts.py".to_owned(), stub("lower")),
        ("tests/test_slug.py".to_owned(), test("slug", "slugify")),
        ("tests/test_counts.py".to_owned(), test("counts", "lower")),
    ];
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    repo_in(dir, root, &base, &files)
}

fn python3_present() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

/// The rule of `scoped_check`: a contract is accepted on the tests of its own
/// files. With the whole suite as the acceptance command, a contract that
/// implemented its module passed nothing while a sibling's stub still failed
/// the suite, and its worker asked for writes outside its scope.
#[tokio::test]
async fn a_contract_is_accepted_on_the_tests_of_its_own_files() {
    if !python3_present() {
        eprintln!("python3 is absent; skipped");
        return;
    }
    let r = python_package(tempfile::tempdir().expect("dir"));
    let suite = "python3 -m unittest discover -s tests";
    let c = named("textkit/slug.py", "w-slug");
    let ws = prepared(&r, &c, "w-slug").await;
    std::fs::write(
        ws.join("textkit/slug.py"),
        "def slugify(s):\n    return s.lower()\n",
    )
    .expect("write");
    let v = verifier(&r, suite);
    let brief = v.acceptance_brief(&c, &ws).expect("a narrowed brief");
    assert!(
        brief.contains("`python3 -m unittest discover -s tests -p test_slug.py`"),
        "{brief}"
    );
    let verdict = v.verify(&c, &ws, &result()).await.expect("ran");
    let Verdict::Pass { by, .. } = &verdict else {
        panic!("its own tests pass while the sibling stub fails the suite: {verdict:?}");
    };
    assert_eq!(by, "python3 -m unittest discover -s tests -p test_slug.py");

    // Its own tests still judge it: a wrong implementation fails.
    let c2 = named("textkit/slug.py", "w-slug-wrong");
    let ws2 = prepared(&r, &c2, "w-slug-wrong").await;
    std::fs::write(
        ws2.join("textkit/slug.py"),
        "def slugify(s):\n    return s\n",
    )
    .expect("write");
    let verdict = v.verify(&c2, &ws2, &result()).await.expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("a wrong slugify fails its own test: {verdict:?}");
    };
    assert!(reason.contains("-p test_slug.py"), "{reason}");
}

/// Plan intake reads the same narrowing: the write set's own test file, or
/// nothing when the whole command runs. Contracts sharing one write set
/// checked by one test file are refused at intake on this answer.
#[tokio::test]
async fn the_plan_scope_of_a_write_set_is_its_own_test_file() {
    let r = python_package(tempfile::tempdir().expect("dir"));
    let v = verifier(&r, "python3 -m unittest discover -s tests");
    assert_eq!(
        v.plan_check_scope(&["textkit/slug.py"], Some(&r.root)),
        Some(vec!["tests/test_slug.py".to_owned()])
    );
    assert_eq!(v.plan_check_scope(&["textkit/slug.py"], None), None);
    let whole = verifier(&r, "make check");
    assert_eq!(
        whole.plan_check_scope(&["textkit/slug.py"], Some(&r.root)),
        None
    );
}

/// The whole check of a dependent split (decision 0018, amended 2026-10-02):
/// `out/greeting.txt` is contract `c1`'s file, `out/sha.txt` its dependent
/// `c2`'s, and the run's check needs both. On `c1` alone it fails here and at
/// the dispatch base, so with `c2` open it is deferred to the root gate: the
/// attempt passes, and the verdict and a notice say so.
const BOTH_FILES: &str = "test -f out/greeting.txt && test -f out/sha.txt";

async fn greeting_attempt(r: &Repo, worker: &str) -> (OpenContract, PathBuf) {
    let c = named("out/greeting.txt", worker);
    let ws = prepared(r, &c, worker).await;
    std::fs::create_dir_all(ws.join("out")).expect("mkdir");
    std::fs::write(ws.join("out/greeting.txt"), "hello\n").expect("write");
    (c, ws)
}

#[tokio::test]
async fn a_whole_check_a_dependent_split_cannot_pass_is_deferred_to_the_root_gate() {
    let r = repo();
    let (c, ws) = greeting_attempt(&r, "w-defer").await;
    let v = CommandVerifier {
        root_gate: true,
        ..verifier(&r, BOTH_FILES)
    };
    let verdict = v
        .verify_among(&c, &ws, &result(), &[named("out/sha.txt", "c2")])
        .await
        .expect("ran");
    let Verdict::Pass {
        by,
        notices,
        commit,
    } = &verdict
    else {
        panic!("deferred while c2 is open: {verdict:?}");
    };
    assert!(by.contains("deferred to the root gate"), "{by}");
    assert!(commit.is_some(), "the attempt's work is committed");
    let notice = notices
        .iter()
        .find(|n| n.contains("is deferred to the root gate"))
        .unwrap_or_else(|| panic!("said in a notice: {notices:?}"));
    assert!(notice.contains("contract w-defer"), "{notice}");
    assert!(notice.contains("contracts c2"), "{notice}");
    // The worker was told before it started.
    let brief = v.acceptance_brief(&c, &ws).expect("a brief");
    assert!(brief.contains(crate::verifier::DEFERRED_BRIEF), "{brief}");
    let ungated = verifier(&r, BOTH_FILES)
        .acceptance_brief(&c, &ws)
        .expect("a brief");
    assert!(
        !ungated.contains(crate::verifier::DEFERRED_BRIEF),
        "{ungated}"
    );
}

/// No deferral without the reasons for it: with no other contract open (the
/// last contract of the plan) or without the root gate, the same attempt
/// fails its check, as before.
#[tokio::test]
async fn a_whole_check_is_not_deferred_for_the_last_contract_or_without_a_root_gate() {
    let r = repo();
    let gated = CommandVerifier {
        root_gate: true,
        ..verifier(&r, BOTH_FILES)
    };
    let (c, ws) = greeting_attempt(&r, "w-last").await;
    let verdict = gated
        .verify_among(&c, &ws, &result(), &[])
        .await
        .expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("nothing left to merge: {verdict:?}");
    };
    // The base run is still made, and no merge of the run precedes it.
    assert!(
        reason.contains("where no contract of this run had merged yet"),
        "{reason}"
    );

    let ungated = verifier(&r, BOTH_FILES);
    let (c, ws) = greeting_attempt(&r, "w-ungated").await;
    let verdict = ungated
        .verify_among(&c, &ws, &result(), &[named("out/sha.txt", "c2")])
        .await
        .expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("no root gate would run it: {verdict:?}");
    };
    assert!(
        reason.contains("also fails at the dispatch base"),
        "{reason}"
    );
}

/// A new workspace, as planned: from a tree with only a README, `c0`
/// writes the workspace manifest and both member manifests, `c1` writes
/// `wcl-core/src/lib.rs` and `c2` writes `wcl/src/main.rs`; the run's check is
/// `cargo test --workspace --offline`. On `c0`'s tree the check narrows to
/// the two new packages, which cargo cannot load without their sources.
const GREENFIELD_CHECK: &str = "cargo test --workspace --offline";

fn greenfield_contracts() -> (OpenContract, Vec<OpenContract>) {
    let mut c0 = named("Cargo.toml", "c0");
    c0.acceptance =
        "[ -f Cargo.toml ] && [ -f wcl-core/Cargo.toml ] && [ -f wcl/Cargo.toml ]".into();
    c0.writes = ["Cargo.toml", "wcl-core/Cargo.toml", "wcl/Cargo.toml"]
        .iter()
        .map(|p| kgraf_orchestrator::WriteTarget {
            path: (*p).into(),
            node: None,
        })
        .collect();
    let open = vec![
        named("wcl-core/src/lib.rs", "c1"),
        named("wcl/src/main.rs", "c2"),
    ];
    (c0, open)
}

/// `c0`'s attempt: the three manifests, as its worker wrote them.
async fn greenfield_attempt(r: &Repo, c0: &OpenContract, worker: &str) -> PathBuf {
    let ws = prepared(r, c0, worker).await;
    std::fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"wcl-core\", \"wcl\"]\n",
    )
    .expect("root manifest");
    for (dir, name) in [("wcl-core", "wcl-core"), ("wcl", "wcl")] {
        std::fs::create_dir_all(ws.join(dir)).expect("mkdir");
        std::fs::write(
            ws.join(dir).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .expect("member manifest");
    }
    ws
}

fn readme_repo() -> Repo {
    let r = repo();
    owner_only_modes(&r.root, &[("README", "")]);
    r
}

/// The check needs the sources `c1` and `c2` write, which no attempt of
/// `c0` may write: it fails on the attempt and at the dispatch base, so with
/// a root gate it is deferred there, loudly, instead of failing `c0` for
/// their absence on every attempt.
#[tokio::test]
async fn a_narrowed_check_that_needs_open_contracts_sources_is_deferred_to_the_root_gate() {
    let r = readme_repo();
    let (c0, open) = greenfield_contracts();
    let ws = greenfield_attempt(&r, &c0, "w-green").await;
    let v = CommandVerifier {
        root_gate: true,
        ..verifier(&r, GREENFIELD_CHECK)
    };
    let verdict = v
        .verify_among(&c0, &ws, &result(), &open)
        .await
        .expect("ran");
    let Verdict::Pass {
        by,
        notices,
        commit,
    } = &verdict
    else {
        panic!("deferred while c1 and c2 are open: {verdict:?}");
    };
    assert!(commit.is_some(), "the manifests are committed");
    assert_eq!(
        by,
        "cargo test -p wcl --offline && cargo test -p wcl-core --offline (deferred to the root \
         gate)"
    );
    let notice = notices
        .iter()
        .find(|n| n.contains("is deferred to the root gate"))
        .unwrap_or_else(|| panic!("said in a notice: {notices:?}"));
    assert!(notice.contains("contract c0"), "{notice}");
    assert!(
        notice.contains("`wcl-core/src/lib.rs` of c1")
            && notice.contains("`wcl/src/main.rs` of c2"),
        "{notice}"
    );
}

/// A contract writing `paths`, named `id`.
fn writing(id: &str, paths: &[&str]) -> OpenContract {
    let mut c = named(paths[0], id);
    c.writes = paths
        .iter()
        .map(|p| kgraf_orchestrator::WriteTarget {
            path: (*p).into(),
            node: None,
        })
        .collect();
    c
}

/// A greenfield plan whose first contract wrote only the workspace manifest,
/// listing `crates/wcl-core` and `crates/wcl`. `c2` writes the library crate;
/// `c4`, still open, writes `crates/wcl/Cargo.toml`. The run's check narrows
/// to `cargo test -p wcl-core --offline` on `c2`'s tree, and cargo cannot
/// load the workspace without the other member's manifest ("failed to load
/// manifest for workspace member `<worktree>/crates/wcl`"). The check needs
/// that absent manifest, so the failure is deferred to the root gate.
#[tokio::test]
async fn a_narrowed_check_that_needs_an_open_members_manifest_is_deferred_to_the_root_gate() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let files = [
        (".gitignore", "target/\n"),
        ("README", "wcl\n"),
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\n    \"crates/wcl-core\",\n    \"crates/wcl\",\n]\n\
             resolver = \"2\"\n",
        ),
    ];
    let r = repo_in(dir, root.clone(), &base, &files);
    owner_only_modes(&root, &files);
    let c2 = writing(
        "c2",
        &["crates/wcl-core/Cargo.toml", "crates/wcl-core/src/lib.rs"],
    );
    let open = [
        writing("c3", &["crates/wcl-core/src/lib.rs"]),
        writing("c4", &["crates/wcl/Cargo.toml", "crates/wcl/src/main.rs"]),
    ];
    let ws = prepared(&r, &c2, "w-member").await;
    std::fs::create_dir_all(ws.join("crates/wcl-core/src")).expect("mkdir");
    std::fs::write(
        ws.join("crates/wcl-core/Cargo.toml"),
        "[package]\nname = \"wcl-core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    std::fs::write(
        ws.join("crates/wcl-core/src/lib.rs"),
        "pub struct Counts {\n    pub lines: usize,\n    pub words: usize,\n    pub bytes: \
         usize,\n}\n\npub fn count(text: &str) -> Counts {\n    Counts { lines: \
         text.matches('\\n').count(), words: text.split_whitespace().count(), bytes: \
         text.len() }\n}\n",
    )
    .expect("lib");
    let v = CommandVerifier {
        root_gate: true,
        ..verifier(&r, GREENFIELD_CHECK)
    };
    let verdict = v
        .verify_among(&c2, &ws, &result(), &open)
        .await
        .expect("ran");
    let Verdict::Pass { by, notices, .. } = &verdict else {
        panic!("deferred while c4 is open: {verdict:?}");
    };
    assert_eq!(
        by,
        "cargo test -p wcl-core --offline (deferred to the root gate)"
    );
    let notice = notices
        .iter()
        .find(|n| n.contains("is deferred to the root gate"))
        .unwrap_or_else(|| panic!("said in a notice: {notices:?}"));
    assert!(notice.contains("`crates/wcl/Cargo.toml` of c4"), "{notice}");
    assert!(
        notice.contains("failed to load manifest for workspace member"),
        "{notice}"
    );
    // Without a root gate it fails, and says it waits on that manifest.
    let ws = prepared(&r, &c2, "w-member-ungated").await;
    std::fs::create_dir_all(ws.join("crates/wcl-core/src")).expect("mkdir");
    std::fs::write(
        ws.join("crates/wcl-core/Cargo.toml"),
        "[package]\nname = \"wcl-core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    std::fs::write(ws.join("crates/wcl-core/src/lib.rs"), "pub fn f() {}\n").expect("lib");
    let verdict = verifier(&r, GREENFIELD_CHECK)
        .verify_among(&c2, &ws, &result(), &open)
        .await
        .expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("no root gate would run it: {verdict:?}");
    };
    assert!(reason.contains("`crates/wcl/Cargo.toml` of c4"), "{reason}");
}

/// The worker hears the same rule before it finishes: the answer to its
/// report names the narrowed command the verifier runs and the deferral,
/// never the contract's Acceptance line, which the verifier does not run.
#[tokio::test]
async fn the_judge_named_to_a_worker_is_the_check_the_verifier_runs() {
    let r = readme_repo();
    let (c0, open) = greenfield_contracts();
    let ws = greenfield_attempt(&r, &c0, "w-judge").await;
    let gated = CommandVerifier {
        root_gate: true,
        ..verifier(&r, GREENFIELD_CHECK)
    };
    let judge = gated.judge(&c0, &ws, &open).expect("a judge");
    assert!(
        judge.contains(
            "judged by `cargo test -p wcl --offline && cargo test -p wcl-core --offline`"
        ),
        "{judge}"
    );
    assert!(judge.contains("deferred to the root gate"), "{judge}");
    assert!(judge.contains("`wcl/src/main.rs` of c2"), "{judge}");
    assert!(judge.contains(GREENFIELD_CHECK), "{judge}");
    assert!(!judge.contains("[ -f"), "{judge}");
    // On the tree as dispatched (no manifest yet) the whole command judges,
    // and every open contract's work is waited on.
    let fresh = prepared(&r, &c0, "w-judge-fresh").await;
    let judge = gated.judge(&c0, &fresh, &open).expect("a judge");
    assert!(
        judge.contains(&format!("judged by `{GREENFIELD_CHECK}`")),
        "{judge}"
    );
    assert!(judge.contains("contracts c1, c2"), "{judge}");
    // Without a root gate the answer says the check cannot be deferred.
    let ungated = verifier(&r, GREENFIELD_CHECK)
        .judge(&c0, &ws, &open)
        .expect("a judge");
    assert!(ungated.contains("no root gate"), "{ungated}");
    assert!(!ungated.contains("deferred to the root gate"), "{ungated}");
}

/// Without a root gate nothing could judge a deferred check, so the attempt
/// fails, and the reason says plainly that the check cannot judge this
/// contract alone and which work it waits on.
#[tokio::test]
async fn without_a_root_gate_the_reason_says_the_check_cannot_judge_the_contract_alone() {
    let r = readme_repo();
    let (c0, open) = greenfield_contracts();
    let ws = greenfield_attempt(&r, &c0, "w-green-ungated").await;
    let verdict = verifier(&r, GREENFIELD_CHECK)
        .verify_among(&c0, &ws, &result(), &open)
        .await
        .expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("no root gate would run it: {verdict:?}");
    };
    assert!(
        reason.contains("cannot judge this contract alone"),
        "{reason}"
    );
    assert!(reason.contains("`wcl/src/main.rs` of c2"), "{reason}");
}

/// A narrowed check that fails on the contract's own package is not
/// deferred because some other contract is open: `c1` writes a new file of
/// another package, which these tests do not need. No base run is made.
#[tokio::test]
async fn a_narrowed_check_is_not_deferred_for_an_open_contract_it_does_not_need() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let files = [
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"a\", \"b\"]\n",
        ),
        (
            "Cargo.lock",
            "# This file is automatically @generated by Cargo.\nversion = 4\n\n\
             [[package]]\nname = \"a\"\nversion = \"0.1.0\"\n\n\
             [[package]]\nname = \"b\"\nversion = \"0.1.0\"\n",
        ),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("a/src/lib.rs", "#[test]\nfn stub() { panic!(\"todo\") }\n"),
        (
            "b/Cargo.toml",
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("b/src/lib.rs", "pub fn b() {}\n"),
    ];
    let r = repo_in(dir, root.clone(), &base, &files);
    owner_only_modes(&root, &files);
    let c0 = named("a/src/lib.rs", "c0");
    let ws = prepared(&r, &c0, "w-own").await;
    std::fs::write(
        ws.join("a/src/lib.rs"),
        "// still a stub\n#[test]\nfn stub() { panic!(\"todo\") }\n",
    )
    .expect("write");
    let v = CommandVerifier {
        root_gate: true,
        ..verifier(&r, GREENFIELD_CHECK)
    };
    let verdict = v
        .verify_among(&c0, &ws, &result(), &[named("b/src/extra.rs", "c1")])
        .await
        .expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("its own stub fails its own tests: {verdict:?}");
    };
    assert!(reason.contains("cargo test -p a --offline"), "{reason}");
    assert!(!reason.contains("deferred"), "{reason}");
    assert!(!reason.contains("dispatch base"), "{reason}");
}

/// A greenfield workspace after its first contract: both member manifests,
/// a stub `wcl-core/src/lib.rs` and a stub `wcl/src/main.rs`.
const WCL_BASE: [(&str, &str); 6] = [
    (".gitignore", "target/\n"),
    (
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\"wcl-core\", \"wcl\"]\n",
    ),
    (
        "wcl-core/Cargo.toml",
        "[package]\nname = \"wcl-core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    ),
    ("wcl-core/src/lib.rs", "// empty stub\n"),
    (
        "wcl/Cargo.toml",
        "[package]\nname = \"wcl\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    ),
    ("wcl/src/main.rs", "fn main() {}\n"),
];

/// The CLI contract's manifest, with the dependency on the library.
const WCL_MANIFEST: &str = "[package]\nname = \"wcl\"\nversion = \"0.1.0\"\nedition = \
     \"2021\"\n\n[dependencies]\nwcl-core = { path = \"../wcl-core\" }\n";

/// A CLI that calls the library's `count`, as the library's contract
/// announced it.
const WCL_MAIN: &str = "use std::io::Read;\n\nfn main() {\n    let mut s = String::new();\n    \
     std::io::stdin().read_to_string(&mut s).expect(\"stdin\");\n    let c = \
     wcl_core::count(&s);\n    println!(\"{} {} {}\", c.lines, c.words, c.bytes);\n}\n";

/// The CLI contract `c4` is dispatched on the tree above. While it runs, the
/// library contract `c2` (which it does not wait on) merges `count`. Then
/// `c4`'s attempt is verified, and the tree it was given lacks `count`.
async fn wcl_with_count_merged_after_dispatch(worker: &str) -> (Repo, OpenContract, PathBuf) {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let r = repo_in(dir, root.clone(), &base, &WCL_BASE);
    owner_only_modes(&root, &WCL_BASE);
    let mut c4 = named("wcl/src/main.rs", "c4");
    c4.writes = ["wcl/Cargo.toml", "wcl/src/main.rs"]
        .iter()
        .map(|p| kgraf_orchestrator::WriteTarget {
            path: (*p).into(),
            node: None,
        })
        .collect();
    let ws4 = prepared(&r, &c4, worker).await;
    let c2 = named("wcl-core/src/lib.rs", "c2");
    let ws2 = prepared(&r, &c2, "w-c2").await;
    std::fs::write(
        ws2.join("wcl-core/src/lib.rs"),
        "pub struct Counts {\n    pub lines: usize,\n    pub words: usize,\n    pub bytes: \
         usize,\n}\n\npub fn count(text: &str) -> Counts {\n    Counts { lines: \
         text.lines().count(), words: text.split_whitespace().count(), bytes: text.len() }\n}\n",
    )
    .expect("write");
    let verdict = verifier(&r, "true")
        .verify(&c2, &ws2, &result())
        .await
        .expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    let merged = r.workspaces.integrate(&c2, &ws2).await.expect("integrate");
    assert!(matches!(merged, Integration::Merged { .. }), "{merged:?}");
    r.workspaces.release(&ws2, true).await;
    std::fs::write(ws4.join("wcl/Cargo.toml"), WCL_MANIFEST).expect("manifest");
    (r, c4, ws4)
}

/// The attempt calls the API the library contract merged after the attempt
/// was dispatched. Its check fails on its own tree (`count` is not in the
/// stub it was given) and passes on the attempt merged onto the integration
/// tip, which is what merge-back lands: the attempt passes, and a notice
/// says where its check passed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attempt_that_needs_work_merged_after_its_dispatch_passes_on_the_merged_tip() {
    let (r, c4, ws) = wcl_with_count_merged_after_dispatch("w-c4").await;
    std::fs::write(ws.join("wcl/src/main.rs"), WCL_MAIN).expect("main");
    let v = CommandVerifier {
        root_gate: true,
        ..verifier(&r, GREENFIELD_CHECK)
    };
    let verdict = v.verify_among(&c4, &ws, &result(), &[]).await.expect("ran");
    let Verdict::Pass {
        by,
        notices,
        commit,
    } = &verdict
    else {
        panic!("it passes on the tree that merges: {verdict:?}");
    };
    assert!(commit.is_some(), "the attempt's work is committed");
    assert_eq!(by, "cargo test -p wcl --offline");
    let notice = notices
        .iter()
        .find(|n| n.contains("passed on the attempt merged onto the integration tip"))
        .unwrap_or_else(|| panic!("said in a notice: {notices:?}"));
    assert!(notice.contains("contract c4"), "{notice}");
    // It merges cleanly onto the tip that holds `count`.
    let merged = r.workspaces.integrate(&c4, &ws).await.expect("integrate");
    assert!(matches!(merged, Integration::Merged { .. }), "{merged:?}");
}

/// The same attempt with an error of its own (a duplicate import): it fails
/// on the merged tip too, and the reason quotes that run, where `count`
/// exists, so the retry is told only what is still wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attempt_that_also_fails_on_the_merged_tip_is_told_what_is_wrong_there() {
    let (r, c4, ws) = wcl_with_count_merged_after_dispatch("w-c4-dup").await;
    std::fs::write(
        ws.join("wcl/src/main.rs"),
        format!("use std::io::Read;\n{WCL_MAIN}"),
    )
    .expect("main");
    let v = CommandVerifier {
        root_gate: true,
        ..verifier(&r, GREENFIELD_CHECK)
    };
    let verdict = v.verify_among(&c4, &ws, &result(), &[]).await.expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("a duplicate import fails on the merged tip too: {verdict:?}");
    };
    assert!(reason.contains("E0252"), "{reason}");
    assert!(
        reason.contains("this output is from the attempt merged onto the integration tip"),
        "{reason}"
    );
    assert!(!reason.contains("cannot find function `count`"), "{reason}");
}

/// A run whose first contract `c1` merged a `src/lib.rs` that says `broken`;
/// at the run's start it said `ok`.
async fn repo_where_c1_merged_a_break() -> Repo {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    let r = repo_in(dir, root, &base, &[("src/lib.rs", "ok\n")]);
    let c1 = named("src/lib.rs", "c1");
    let ws = prepared(&r, &c1, "w-c1").await;
    std::fs::write(ws.join("src/lib.rs"), "broken\n").expect("write");
    let verdict = verifier(&r, "true")
        .verify(&c1, &ws, &result())
        .await
        .expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    let merged = r.workspaces.integrate(&c1, &ws).await.expect("integrate");
    assert!(matches!(merged, Integration::Merged { .. }), "{merged:?}");
    r.workspaces.release(&ws, true).await;
    r
}

/// The plan's last contract gets the dispatch-base run too. Its whole check
/// fails on the attempt and at its dispatch base, which holds `c1`'s merge,
/// and passes where the run started: the failure is an earlier delivery's
/// break, and the reason names it. Still a failure (the last contract is
/// checked whole), never a deferral.
#[tokio::test]
async fn the_last_contract_s_failure_is_attributed_to_an_earlier_merge_that_broke_its_check() {
    let r = repo_where_c1_merged_a_break().await;
    let v = CommandVerifier {
        root_gate: true,
        ..verifier(&r, "! grep -q broken src/lib.rs")
    };
    let c2 = named("src/other.rs", "c2");
    let ws = prepared(&r, &c2, "w-c2").await;
    std::fs::write(ws.join("src/other.rs"), "fine\n").expect("write");
    let verdict = v.verify_among(&c2, &ws, &result(), &[]).await.expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("the last contract is checked whole: {verdict:?}");
    };
    assert!(
        reason.contains("also fails at the dispatch base"),
        "{reason}"
    );
    assert!(
        reason.contains("passes at the run's original base"),
        "{reason}"
    );
    assert!(
        reason.contains("an earlier merge of this run broke it"),
        "{reason}"
    );
    assert!(reason.contains("merges of contracts c1"), "{reason}");
    assert!(!reason.contains("deferred"), "{reason}");
}

/// The same last contract with a check that also failed where the run
/// started: the failure predates the run's work, and the reason says so.
/// One its own change broke (it passes at the dispatch base) is still its
/// own failure.
#[tokio::test]
async fn the_last_contract_s_failure_is_told_apart_from_a_check_that_failed_before_the_run() {
    let r = repo_where_c1_merged_a_break().await;
    let gated = |command: &str| CommandVerifier {
        root_gate: true,
        ..verifier(&r, command)
    };
    let c2 = named("src/other.rs", "c2");
    let ws = prepared(&r, &c2, "w-c2-before").await;
    std::fs::write(ws.join("src/other.rs"), "fine\n").expect("write");
    let verdict = gated("test -f src/never.rs")
        .verify_among(&c2, &ws, &result(), &[])
        .await
        .expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("{verdict:?}");
    };
    assert!(
        reason.contains("at the run's original base") && reason.contains("before this run's work"),
        "{reason}"
    );

    let ws = prepared(&r, &c2, "w-c2-own").await;
    std::fs::write(ws.join("src/other.rs"), "fine\n").expect("write");
    let verdict = gated("test ! -f src/other.rs")
        .verify_among(&c2, &ws, &result(), &[])
        .await
        .expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("{verdict:?}");
    };
    assert!(reason.contains("passes at the dispatch base"), "{reason}");
}

/// A whole check that passes at the dispatch base and fails on the attempt
/// was broken by the attempt: never deferred, and the reason says so.
#[tokio::test]
async fn a_whole_check_the_attempt_broke_is_never_deferred() {
    let r = repo();
    let v = CommandVerifier {
        root_gate: true,
        ..verifier(&r, "test ! -f out/greeting.txt")
    };
    let (c, ws) = greeting_attempt(&r, "w-broke").await;
    let verdict = v
        .verify_among(&c, &ws, &result(), &[named("out/sha.txt", "c2")])
        .await
        .expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("the attempt broke a check that passed at its base: {verdict:?}");
    };
    assert!(reason.contains("passes at the dispatch base"), "{reason}");
}

/// A remediation is judged on the whole command and the failing gate's
/// commands: it fixes what the merged result failed, not one file's tests.
#[tokio::test]
async fn a_remediation_is_checked_with_the_whole_command() {
    if !python3_present() {
        eprintln!("python3 is absent; skipped");
        return;
    }
    let r = python_package(tempfile::tempdir().expect("dir"));
    let suite = "python3 -m unittest discover -s tests";
    let mut c = named("textkit/slug.py", "w-fix");
    c.gate_commands = vec!["true".into()];
    let ws = prepared(&r, &c, "w-fix").await;
    std::fs::write(
        ws.join("textkit/slug.py"),
        "def slugify(s):\n    return s.lower()\n",
    )
    .expect("write");
    let v = verifier(&r, suite);
    assert!(v.acceptance_brief(&c, &ws).is_none());
    let verdict = v.verify(&c, &ws, &result()).await.expect("ran");
    assert!(
        matches!(&verdict, Verdict::Fail { reason } if reason.contains(suite)),
        "the sibling stub still fails the whole suite: {verdict:?}"
    );
}

/// Python: an import writes `__pycache__` next to the module when no
/// `.gitignore` covers it. Reported, not merged, not a scope failure.
#[tokio::test]
async fn a_pycache_the_check_writes_is_not_the_workers_change() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("python3 is absent; skipped");
        return;
    }
    let r = repo();
    let c = named("pkg/mod.py", "w-py");
    let ws = prepared(&r, &c, "w-py").await;
    std::fs::create_dir_all(ws.join("pkg")).expect("mkdir");
    std::fs::write(ws.join("pkg/mod.py"), "X = 1\n").expect("write");
    // Apple's python3 sets a `pycache_prefix` under ~/Library; clearing it
    // gives the default layout, `__pycache__` beside the module.
    let check = "cd pkg && env -u PYTHONDONTWRITEBYTECODE -u PYTHONPYCACHEPREFIX python3 -c \
                 'import sys; sys.pycache_prefix = None; import mod; assert mod.X == 1' \
                 && ls __pycache__";
    let verdict = verifier(&r, check)
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    let Verdict::Pass { notices, .. } = &verdict else {
        panic!("{verdict:?}");
    };
    assert!(
        notices.iter().any(|n| n.contains("pkg/__pycache__/mod.")),
        "{notices:?}"
    );
    assert!(!ws.join("pkg/__pycache__").exists());
    assert!(matches!(
        r.workspaces.integrate(&c, &ws).await.expect("merge"),
        Integration::Merged { .. }
    ));
    let tree = branch_tree(&r);
    assert!(!tree.contains("__pycache__"), "{tree}");
    assert!(tree.lines().any(|p| p == "pkg/mod.py"), "{tree}");
}

/// The check runs with `PYTHONPYCACHEPREFIX` in its scratch directory: a plain
/// import writes no `__pycache__` into the worktree, so there is nothing to
/// report or remove.
#[tokio::test]
async fn a_check_import_writes_its_bytecode_to_scratch() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("python3 is absent; skipped");
        return;
    }
    let r = repo();
    let c = named("pkg/mod.py", "w-pyscratch");
    let ws = prepared(&r, &c, "w-pyscratch").await;
    std::fs::create_dir_all(ws.join("pkg")).expect("mkdir");
    std::fs::write(ws.join("pkg/mod.py"), "X = 1\n").expect("write");
    let check = "cd pkg && env -u PYTHONDONTWRITEBYTECODE python3 -c \
                 'import sys, mod; assert sys.pycache_prefix and \"pycache\" in sys.pycache_prefix'";
    let verdict = verifier(&r, check)
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    let Verdict::Pass { notices, .. } = &verdict else {
        panic!("{verdict:?}");
    };
    assert!(notices.is_empty(), "{notices:?}");
}

/// A name that is not UTF-8 is removed as git printed it, byte for byte.
/// Linux only: APFS refuses such names.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_non_utf8_name_the_check_creates_is_removed_byte_for_byte() {
    let r = repo();
    let c = named("b.txt", "w-bytes");
    let ws = prepared(&r, &c, "w-bytes").await;
    std::fs::write(ws.join("b.txt"), "b\n").expect("write");
    let verdict = verifier(&r, "printf x > \"$(printf 'bad\\377name')\"")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    let left: Vec<_> = std::fs::read_dir(&ws)
        .expect("dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .filter(|n| n.to_string_lossy().starts_with("bad"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn notices_name_a_lockfile_apart_from_other_leftovers() {
    let left = kgraf_sandbox::Leftovers {
        created: (0..8)
            .map(|i| PathBuf::from(format!("gen/{i}.o")))
            .chain([
                PathBuf::from("sub/package-lock.json"),
                PathBuf::from("Cargo.lock"),
            ])
            .collect(),
        changed: Vec::new(),
    };
    let excluded = [kgraf_sandbox::Excluded {
        path: PathBuf::from("Cargo.lock"),
        kind: kgraf_sandbox::GeneratedKind::Lockfile,
    }];
    let n = crate::verifier::check_notices("npm test", &left, &excluded);
    assert_eq!(n.len(), 2, "{n:?}");
    assert!(n[0].contains("generated sub/package-lock.json"), "{n:?}");
    assert!(
        n[1].contains("created 8 file(s)") && n[1].contains("and 3 more"),
        "{n:?}"
    );
    let w = crate::verifier::worker_notices(&excluded);
    assert_eq!(w.len(), 1);
    assert!(w[0].contains("Cargo.lock (made by the worker)"), "{w:?}");
    assert!(
        crate::verifier::check_notices("x", &kgraf_sandbox::Leftovers::default(), &[]).is_empty()
    );
}

#[test]
fn validation_commands_are_discovered_or_configured_never_invented() {
    let dir = tempfile::tempdir().expect("dir");
    assert!(
        discover_validation_commands(dir.path(), &[])
            .expect("none")
            .is_empty()
    );
    std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").expect("w");
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"scripts":{"test":"vitest"}}"#,
    )
    .expect("w");
    std::fs::write(
        dir.path().join("Makefile"),
        "build:\n\ttrue\ntest:\n\ttrue\n",
    )
    .expect("w");
    assert_eq!(
        discover_validation_commands(dir.path(), &[]).expect("found"),
        vec!["cargo test --workspace", "npm test", "make test"]
    );
    assert_eq!(
        discover_validation_commands(dir.path(), &["just check".into()]).expect("configured"),
        vec!["just check"]
    );
}

/// The pytest command the platform's interpreter runs.
fn pytest_command() -> &'static str {
    if cfg!(windows) {
        "python -m pytest"
    } else {
        "python3 -m pytest"
    }
}

/// Discovery with the pytest import probe fixed to `importable`.
fn discover_py(root: &std::path::Path, importable: bool) -> (Vec<String>, Vec<String>) {
    crate::gates::discover_with(root, &[], &|| importable).expect("discover")
}

/// Discovery with every probe fixed: pytest importable or not, the programs
/// on `PATH`, and a package store home.
fn discover_probed(
    root: &std::path::Path,
    on_path: &[&str],
    home: &std::path::Path,
) -> (Vec<String>, Vec<String>) {
    let on_path: Vec<String> = on_path.iter().map(|p| (*p).to_owned()).collect();
    crate::gates::discover_probed(
        root,
        &[],
        &crate::gates::GateProbes {
            pytest_importable: &|| true,
            on_path: &|p| on_path.iter().any(|o| o == p),
            env: crate::deps::InstallEnv {
                home: Some(home.to_path_buf()),
                go_mod_cache: Some(home.join("go/pkg/mod")),
                stores: Default::default(),
            },
        },
    )
    .expect("discover")
}

/// Decision 0018, amended 2026-10-06: Go, Maven, Gradle and .NET projects
/// get a gate, but only when the tool is on `PATH` and the project's
/// dependencies are installed; otherwise a note names what is missing and
/// the command that installs it, and no gate is added that would fail every
/// correct project.
#[test]
fn go_maven_gradle_and_dotnet_gates_need_their_tool_and_their_installs() {
    let home = tempfile::tempdir().expect("home");
    let h = home.path();
    let all = ["go", "mvn", "gradle", "dotnet"];

    let go = tempfile::tempdir().expect("go");
    std::fs::write(
        go.path().join("go.mod"),
        "module x\nrequire rsc.io/quote v1.5.2\n",
    )
    .expect("w");
    let (found, notes) = discover_probed(go.path(), &all, h);
    assert!(found.is_empty(), "{found:?}");
    assert!(notes[0].contains("go mod download"), "{notes:?}");
    let cached = h.join("go/pkg/mod/cache/download/rsc.io/quote/@v");
    std::fs::create_dir_all(&cached).expect("mkdir");
    std::fs::write(cached.join("v1.5.2.mod"), "").expect("w");
    assert_eq!(discover_probed(go.path(), &all, h).0, vec!["go test ./..."]);
    let (found, notes) = discover_probed(go.path(), &[], h);
    assert!(
        found.is_empty() && notes[0].contains("`go` is not on PATH"),
        "{notes:?}"
    );

    let mvn = tempfile::tempdir().expect("mvn");
    std::fs::write(mvn.path().join("pom.xml"), "<project/>").expect("w");
    let (found, notes) = discover_probed(mvn.path(), &all, h);
    assert!(
        found.is_empty() && notes[0].contains("mvn test"),
        "{notes:?}"
    );
    std::fs::create_dir_all(
        h.join(".m2/repository/org/apache/maven/plugins/maven-surefire-plugin"),
    )
    .expect("mkdir");
    assert_eq!(discover_probed(mvn.path(), &all, h).0, vec!["mvn -o test"]);

    let gradle = tempfile::tempdir().expect("gradle");
    std::fs::write(gradle.path().join("build.gradle"), "").expect("w");
    let (found, notes) = discover_probed(gradle.path(), &all, h);
    assert!(
        found.is_empty() && notes[0].contains("gradle build"),
        "{notes:?}"
    );
    std::fs::create_dir_all(h.join(".gradle/caches/modules-2")).expect("mkdir");
    assert_eq!(
        discover_probed(gradle.path(), &all, h).0,
        vec!["gradle test --offline"]
    );
    // The wrapper runs from the command's own Gradle home, which reaches the
    // user's unpacked distribution read-only: a gate once the distribution
    // the wrapper names is unpacked, a note before.
    std::fs::write(gradle.path().join("gradlew"), "").expect("w");
    let (found, notes) = discover_probed(gradle.path(), &all, h);
    assert!(
        found.is_empty() && notes[0].contains("distributionUrl"),
        "{notes:?}"
    );
    std::fs::create_dir_all(gradle.path().join("gradle/wrapper")).expect("mkdir");
    std::fs::write(
        gradle
            .path()
            .join("gradle/wrapper/gradle-wrapper.properties"),
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.5-bin.zip\n",
    )
    .expect("w");
    let (found, notes) = discover_probed(gradle.path(), &all, h);
    assert!(
        found.is_empty() && notes[0].contains("./gradlew --version"),
        "{notes:?}"
    );
    let dist = h.join(".gradle/wrapper/dists/gradle-8.5-bin/abc123");
    std::fs::create_dir_all(dist.join("gradle-8.5/lib")).expect("mkdir");
    std::fs::write(dist.join("gradle-8.5-bin.zip.ok"), "").expect("w");
    let (found, notes) = discover_probed(gradle.path(), &[], h);
    // No `gradle` on PATH is needed: the wrapper is the program. Windows runs
    // it through Git Bash and reaches the distribution by a junction, so the
    // gate is the same there (decision 0084).
    assert_eq!(found, vec!["./gradlew test --offline"], "{notes:?}");

    let dotnet = tempfile::tempdir().expect("dotnet");
    std::fs::write(dotnet.path().join("App.csproj"), "<Project/>").expect("w");
    let (found, notes) = discover_probed(dotnet.path(), &all, h);
    assert!(
        found.is_empty() && notes[0].contains("dotnet restore"),
        "{notes:?}"
    );
    std::fs::create_dir_all(dotnet.path().join("obj")).expect("mkdir");
    std::fs::write(dotnet.path().join("obj/project.assets.json"), "{}").expect("w");
    let (found, notes) = discover_probed(dotnet.path(), &all, h);
    assert!(
        found.is_empty() && notes[0].contains("~/.nuget/packages"),
        "{notes:?}"
    );
    std::fs::create_dir_all(h.join(".nuget/packages")).expect("mkdir");
    assert_eq!(
        discover_probed(dotnet.path(), &all, h).0,
        vec!["dotnet test"]
    );
}

/// A test script whose project's dependencies are not installed is no gate:
/// it would fail every correct project with "Cannot find module". The note
/// names the missing install and the lockfile's command.
#[test]
fn an_npm_test_gate_needs_its_dependencies_installed() {
    let dir = tempfile::tempdir().expect("dir");
    let home = tempfile::tempdir().expect("home");
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"scripts":{"test":"jest"},"devDependencies":{"jest":"29"}}"#,
    )
    .expect("w");
    std::fs::write(dir.path().join("package-lock.json"), "{}").expect("w");
    let (found, notes) = discover_probed(dir.path(), &[], home.path());
    assert!(found.is_empty(), "{found:?}");
    assert!(
        notes[0].contains("npm test gate not applicable"),
        "{notes:?}"
    );
    assert!(notes[0].contains("Run `npm ci`"), "{notes:?}");
    std::fs::create_dir_all(dir.path().join("node_modules/jest")).expect("mkdir");
    assert_eq!(
        discover_probed(dir.path(), &[], home.path()).0,
        vec!["npm test"]
    );
}

/// `npm init` writes a test script that only fails. A JavaScript project that
/// kept it (a correct project with no tests yet) gets no `npm test` gate, and
/// the note says why; a real test script is a gate.
#[test]
fn the_npm_init_test_script_is_not_a_gate() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name": "web", "scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#,
    )
    .expect("w");
    let (found, notes) = discover_py(dir.path(), false);
    assert!(found.is_empty(), "{found:?}");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("npm test gate not applicable"),
        "{notes:?}"
    );
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name": "web", "scripts": {"test": "node --test"}}"#,
    )
    .expect("w");
    let (found, notes) = discover_py(dir.path(), false);
    assert_eq!(found, vec!["npm test"]);
    assert!(notes.is_empty(), "{notes:?}");
}

/// A `setup.py`, `setup.cfg` or `requirements.txt` project with Python tests
/// gets the pytest gate as a `pyproject.toml` one does, when pytest is
/// installed. Declaring pytest is not installing it (decision 0087, amended
/// 2026-10-06): with pytest neither importable nor in a virtual environment,
/// the gate failed every correct project, so it is left out with the
/// command that installs it.
#[test]
fn a_setup_py_or_requirements_project_with_tests_gets_the_pytest_gate() {
    for marker in ["setup.py", "setup.cfg", "requirements.txt"] {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir_all(dir.path().join("tests")).expect("mkdir");
        std::fs::write(dir.path().join("tests/test_a.py"), "def test_a(): pass\n").expect("w");
        assert!(
            discover_py(dir.path(), true).0.is_empty(),
            "{marker}: tests alone are not a Python project"
        );
        std::fs::write(dir.path().join(marker), "pytest\n").expect("w");
        assert_eq!(
            discover_py(dir.path(), true).0,
            vec![pytest_command()],
            "{marker}"
        );
        let (found, notes) = discover_py(dir.path(), false);
        assert!(found.is_empty(), "{marker}: {found:?}");
        assert!(
            notes.iter().any(|n| n.contains("-m venv .venv")),
            "{marker}: {notes:?}"
        );
    }
}

/// A project whose virtual environment has pytest gets the pytest gate
/// though the host interpreter has none: the sandbox runs `python3` from
/// that environment. A virtual environment without pytest is named, with
/// the command that installs it.
#[test]
fn a_virtual_environment_with_pytest_is_the_gate_s_interpreter() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("tests")).expect("mkdir");
    std::fs::write(dir.path().join("tests/test_a.py"), "def test_a(): pass\n").expect("w");
    std::fs::write(
        dir.path().join("pyproject.toml"),
        "[project]\nname = \"a\"\n[project.optional-dependencies]\ntest = [\"pytest\"]\n",
    )
    .expect("w");
    std::fs::create_dir_all(dir.path().join(".venv/lib/python3.12/site-packages")).expect("mkdir");
    std::fs::write(dir.path().join(".venv/pyvenv.cfg"), "home = /usr/bin\n").expect("w");
    let (found, notes) = discover_py(dir.path(), true);
    assert!(found.is_empty(), "{found:?}");
    assert!(
        notes
            .iter()
            .any(|n| n.contains("the virtual environment .venv has no pytest")),
        "{notes:?}"
    );
    std::fs::create_dir_all(dir.path().join(".venv/lib/python3.12/site-packages/pytest"))
        .expect("mkdir");
    assert_eq!(discover_py(dir.path(), false).0, vec![pytest_command()]);
}

/// Python tests with no pytest declared and none importable add no pytest
/// gate, and say why; an importable pytest brings the gate back.
#[test]
fn python_tests_without_pytest_say_the_gate_does_not_apply() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("tests")).expect("mkdir");
    std::fs::write(dir.path().join("tests/test_a.py"), "def test_a(): pass\n").expect("w");
    std::fs::write(dir.path().join("requirements.txt"), "requests\n").expect("w");
    let (found, notes) = discover_py(dir.path(), false);
    assert!(found.is_empty(), "{found:?}");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("not applicable"), "{notes:?}");
    let (found, notes) = discover_py(dir.path(), true);
    assert_eq!(found, vec![pytest_command()]);
    assert!(notes.is_empty(), "{notes:?}");
}

/// A `pyproject.toml` alone (the tooling file of a Rust project) adds no
/// pytest gate; Python tests or a pytest configuration do, with the
/// platform's interpreter.
#[test]
fn a_tooling_pyproject_in_a_rust_project_adds_no_pytest_gate() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").expect("w");
    std::fs::write(
        dir.path().join("pyproject.toml"),
        "[tool.ruff]\nline-length = 100\n",
    )
    .expect("w");
    assert_eq!(
        discover_py(dir.path(), true).0,
        vec!["cargo test --workspace"]
    );
    // An empty tests folder, or one with no test files, is not Python tests.
    std::fs::create_dir_all(dir.path().join("tests")).expect("mkdir");
    std::fs::write(dir.path().join("tests/data.json"), "{}").expect("w");
    assert_eq!(
        discover_py(dir.path(), true).0,
        vec!["cargo test --workspace"]
    );
    // A test file under tests/ makes the gate plausible, with pytest importable.
    std::fs::create_dir_all(dir.path().join("tests/unit")).expect("mkdir");
    std::fs::write(
        dir.path().join("tests/unit/test_a.py"),
        "def test_a(): pass\n",
    )
    .expect("w");
    assert_eq!(
        discover_py(dir.path(), true).0,
        vec!["cargo test --workspace", pytest_command()]
    );
    // So does a pytest configuration, with no test file yet, when pytest is
    // installed.
    let cfg = tempfile::tempdir().expect("dir");
    std::fs::write(
        cfg.path().join("pyproject.toml"),
        "[tool.pytest.ini_options]\naddopts = \"-q\"\n",
    )
    .expect("w");
    assert_eq!(discover_py(cfg.path(), true).0, vec![pytest_command()]);
    // A test file with no pyproject.toml is not a Python project's gate.
    let bare = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(bare.path().join("tests")).expect("mkdir");
    std::fs::write(bare.path().join("tests/test_a.py"), "").expect("w");
    assert!(discover_py(bare.path(), true).0.is_empty());
}

/// A stand-in for cargo on a manifest: no manifest fails one way, a manifest
/// with a package table and no target fails another, and a workspace passes.
///
/// It is short on purpose. A deferred check's notice quotes only the first
/// `DEFERRED_OUTPUT_CHARS` characters of the check's output, and the output
/// starts with the command itself. Where the check cannot start at all (the
/// Windows token sandbox ends every child with status 0xc0000142), the status
/// follows the command, and a long command pushes it out of the quote. The
/// test's failure text then would not say why the shell never ran.
const MANIFEST_CHECK: &str = "[ -f Cargo.toml ] || { echo \"error: could not find \
     \\`Cargo.toml\\` in \\`$PWD\\`\"; exit 1; }; \
     if grep -q '^\\[package\\]' Cargo.toml; then echo \"error: failed to parse manifest at \
     \\`$PWD/Cargo.toml\\`\"; echo \"Caused by: no targets specified\"; exit 1; fi";

// The echoed command (`$ ` and the command), its line break and the text
// `[exit code: 0xc0000142]` must fit in the 300 characters of the quote.
const _: () = assert!(MANIFEST_CHECK.len() <= 265);

/// A new-workspace failure: the contract writes a root manifest cargo refuses.
/// The check fails at the dispatch base too (no manifest, a different
/// error) and the other contracts write the members, but the error names the
/// contract's own file, so the attempt is judged failed and the worker gets
/// the error instead of the check being deferred.
#[tokio::test]
async fn a_failure_naming_a_file_the_contract_wrote_is_not_deferred() {
    let r = repo();
    let c = named("Cargo.toml", "c1");
    let ws = prepared(&r, &c, "w-manifest").await;
    std::fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname = \"wcl\"\n\n[workspace]\nmembers = [\"wcl-core\", \"wcl\"]\n",
    )
    .expect("write");
    let v = CommandVerifier {
        root_gate: true,
        ..verifier(&r, MANIFEST_CHECK)
    };
    let verdict = v
        .verify_among(
            &c,
            &ws,
            &result(),
            &[
                named("wcl-core/Cargo.toml", "c2"),
                named("wcl/Cargo.toml", "c3"),
            ],
        )
        .await
        .expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("the contract's own broken manifest is a failure: {verdict:?}");
    };
    assert!(reason.contains("failed to parse manifest at"), "{reason}");
    assert!(reason.contains("no targets specified"), "{reason}");
    assert!(!reason.contains("deferred to the root gate"), "{reason}");
}

/// A failure that names only a file another contract writes is still the
/// deferral's case: it needs their unmerged work.
#[tokio::test]
async fn a_failure_naming_only_another_contracts_file_is_still_deferred() {
    let r = repo();
    let (c, ws) = greeting_attempt(&r, "w-other-file").await;
    let v = CommandVerifier {
        root_gate: true,
        ..verifier(
            &r,
            "test -f out/sha.txt || { echo 'error: missing out/sha.txt'; exit 1; }",
        )
    };
    let verdict = v
        .verify_among(&c, &ws, &result(), &[named("out/sha.txt", "c2")])
        .await
        .expect("ran");
    let Verdict::Pass { by, .. } = &verdict else {
        panic!("another contract's file is deferred: {verdict:?}");
    };
    assert!(by.contains("deferred to the root gate"), "{by}");
}

/// The same path matching for a Windows program's output: cargo names files
/// with backslashes under the worktree's drive path, and Git Bash's programs
/// print the worktree in the MSYS form. A member manifest is not read as the
/// root manifest, and a root manifest in either spelling is.
#[test]
fn own_file_failure_reads_windows_paths_in_each_spelling() {
    use crate::verifier::own_file_failure_as;
    let root = Path::new(r"C:\Work\TEMP~1\.tmpA\ws\w-green");
    let base = Path::new(r"C:\Work\TEMP~1\.tmpA\ws\probe-1");
    let at_base = "error: could not find `Cargo.toml` in \
                   `C:\\Work\\TEMP~1\\.tmpA\\ws\\probe-1` or any parent \
                   directory";
    // A new workspace, as cargo prints it on Windows: the error names
    // the member manifest, which an open contract's source completes, so the
    // failure may be only that contract's missing work.
    let cargo = "error: failed to load manifest for workspace member \
                 `C:\\Work\\TEMP~1\\.tmpA\\ws\\w-green\\wcl-core`\n\
                 referenced by workspace at \
                 `C:\\Work\\TEMP~1\\.tmpA\\ws\\w-green\\Cargo.toml`\n\n\
                 Caused by:\n  failed to parse manifest at \
                 `C:\\Work\\TEMP~1\\.tmpA\\ws\\w-green\\wcl-core\\Cargo.toml`\n\n\
                 Caused by:\n  no targets specified in the manifest";
    let writes = ["Cargo.toml", "wcl-core/Cargo.toml", "wcl/Cargo.toml"];
    let others = ["wcl-core/src/lib.rs", "wcl/src/main.rs"];
    let own = |out: &str, windows: bool| {
        own_file_failure_as(out, &[root], at_base, base, &writes, &others, windows)
    };
    assert_eq!(own(cargo, true), None, "it waits on the open contracts");
    // Read with the old rule, the root manifest's name at the end of the
    // backslash path was taken as the contract's own file.
    assert!(own(cargo, false).is_some());
    // A root manifest cargo refuses, named in the drive form or the MSYS
    // form: the contract's own defect.
    let only_root = ["Cargo.toml"];
    let mine = |out: &str| {
        own_file_failure_as(
            out,
            &[root],
            at_base,
            base,
            &only_root,
            &["wcl-core/Cargo.toml"],
            true,
        )
    };
    for out in [
        "error: failed to parse manifest at \
         `C:\\Work\\TEMP~1\\.tmpA\\ws\\w-green\\Cargo.toml`",
        "error: failed to parse manifest at \
         `/c/Work/TEMP~1/.tmpA/ws/w-green/Cargo.toml`",
        "error: failed to parse manifest at \
         `c:/Work/TEMP~1/.tmpA/ws/w-green/Cargo.toml`",
    ] {
        assert!(mine(out).is_some(), "{out}");
    }
    // A manifest under another folder is not the root one.
    assert_eq!(
        mine("error: failed to parse manifest at `C:\\elsewhere\\Cargo.toml`"),
        None
    );
}

/// Path matching: whole paths only, the worktree prefix in each spelling,
/// and `./`.
#[test]
fn own_file_failure_matches_whole_paths_in_each_spelling() {
    use crate::verifier::own_file_failure;
    let root = Path::new("/private/var/w/attempt");
    let alt = Path::new("/var/w/attempt");
    let base = Path::new("/private/var/w/base");
    let own = ["Cargo.toml"];
    let hit = |out: &str, base_out: &str| {
        own_file_failure(
            out,
            &[root, alt],
            base_out,
            base,
            &own,
            &["wcl-core/src/lib.rs"],
        )
        .is_some()
    };
    assert!(hit("bad manifest at `/var/w/attempt/Cargo.toml`", ""));
    assert!(hit(
        "bad manifest at `/private/var/w/attempt/Cargo.toml`",
        ""
    ));
    assert!(hit("bad manifest at ./Cargo.toml", ""));
    assert!(hit("bad manifest at `Cargo.toml`", ""));
    // Another file with the same name, or a longer name, is not it.
    assert!(!hit("bad manifest at `/var/w/attempt/wcl/Cargo.toml`", ""));
    assert!(!hit("bad manifest at wcl-core/Cargo.toml", ""));
    assert!(!hit("bad manifest at Cargo.toml.bak", ""));
    // A line that also names an open contract's file may be only its missing
    // work, and so may an own manifest whose package that contract completes.
    assert!(!hit(
        "bad manifest at `Cargo.toml`: no wcl-core/src/lib.rs",
        ""
    ));
    let nested = ["wcl-core/Cargo.toml"];
    let others = ["wcl-core/src/lib.rs"];
    assert!(
        own_file_failure(
            "bad wcl-core/Cargo.toml",
            &[root],
            "",
            base,
            &nested,
            &others
        )
        .is_none()
    );
    assert!(own_file_failure("bad wcl-core/Cargo.toml", &[root], "", base, &nested, &[]).is_some());
    // The same line at the dispatch base is not new.
    assert!(!hit(
        "bad manifest at `/var/w/attempt/Cargo.toml`",
        "bad manifest at `/private/var/w/base/Cargo.toml`"
    ));
}

fn verifier(r: &Repo, command: &str) -> CommandVerifier {
    CommandVerifier {
        command: command.into(),
        repo: r.root.clone(),
        workspaces: r.workspaces.clone(),
        merge_back: r.merge_back.clone(),
        read_denies: Vec::new(),
        timeout: Duration::from_secs(300),
        permissions: Arc::new(kgraf_tools::PermissionControl::manual()),
        root_gate: false,
    }
}

fn contract(path: &str) -> OpenContract {
    let mut c = OpenContract::planned("c1", "t".into(), "add it", "it exists");
    c.writes = vec![kgraf_orchestrator::WriteTarget {
        path: path.into(),
        node: None,
    }];
    c
}

fn result() -> WorkerResult {
    WorkerResult {
        steps: 1,
        ..WorkerResult::bare("t", "w", kgraf_runtime::WorkerOutcome::Completed)
    }
}

async fn worktree(r: &Repo, worker: &str) -> PathBuf {
    r.workspaces
        .prepare(&contract("x"), &worker.into())
        .await
        .expect("worktree")
}

#[tokio::test]
async fn a_passing_in_scope_change_is_committed_through_merge_back() {
    let r = repo();
    let ws = worktree(&r, "w-pass").await;
    std::fs::write(ws.join("b.txt"), "b").expect("write");
    let c = contract("b.txt");
    let v = verifier(&r, "test -f b.txt");
    let verdict = v.verify(&c, &ws, &result()).await.expect("ran");
    let Verdict::Pass {
        commit: Some(commit),
        ..
    } = verdict
    else {
        panic!("expected a pass with a commit: {verdict:?}");
    };
    assert!(v.recheck(&commit).await.expect("ran"));
    let bogus = VerifiedCommit {
        sha: "0".repeat(40),
        ..commit
    };
    assert!(!v.recheck(&bogus).await.expect("ran"));
}

/// A gate remediation passes the run's check and must also pass the failing
/// gate's own commands in its worktree: a fix they still fail on is refused,
/// naming the gate's command; one they pass on is accepted.
#[tokio::test]
async fn a_remediation_must_also_pass_the_failing_gates_commands() {
    let r = repo();
    let ws = worktree(&r, "w-fix").await;
    std::fs::write(ws.join("b.txt"), "b").expect("write");
    let mut c = contract("b.txt");
    c.gate_commands = vec!["test -f b.txt".into(), "grep -q fixed b.txt".into()];
    match verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran")
    {
        Verdict::Fail { reason } => {
            assert!(reason.contains("`grep -q fixed b.txt` failed"), "{reason}")
        }
        other => panic!("the run's check alone does not accept it: {other:?}"),
    }
    let ws = worktree(&r, "w-fix-2").await;
    std::fs::write(ws.join("b.txt"), "fixed").expect("write");
    let v = verifier(&r, "true").verify(&c, &ws, &result()).await;
    assert!(
        matches!(
            v,
            Ok(Verdict::Pass {
                commit: Some(_),
                ..
            })
        ),
        "{v:?}"
    );
}

#[tokio::test]
async fn an_undeclared_change_fails_the_attempt() {
    let r = repo();
    let ws = worktree(&r, "w-stray").await;
    std::fs::write(ws.join("b.txt"), "b").expect("write");
    std::fs::write(ws.join("stray.txt"), "x").expect("write");
    match verifier(&r, "true")
        .verify(&contract("b.txt"), &ws, &result())
        .await
        .expect("ran")
    {
        Verdict::Fail { reason } => assert!(reason.contains("stray.txt"), "{reason}"),
        other => panic!("expected a scope failure, got {other:?}"),
    }
}

/// Worker code commits an out-of-scope edit itself,
/// so a diff of the index against the moved HEAD would show nothing.
#[tokio::test]
async fn a_change_the_worker_committed_itself_does_not_escape_the_scope_check() {
    let r = repo();
    let ws = worktree(&r, "w-commits").await;
    std::fs::write(ws.join("b.txt"), "b").expect("write");
    std::fs::write(ws.join("stray.txt"), "x").expect("write");
    host_git(&ws, &["add", "stray.txt"]);
    host_git(
        &ws,
        &[
            "-c",
            "user.name=w",
            "-c",
            "user.email=w@w",
            "commit",
            "-q",
            "-m",
            "sneak",
        ],
    );
    match verifier(&r, "true")
        .verify(&contract("b.txt"), &ws, &result())
        .await
        .expect("ran")
    {
        Verdict::Fail { reason } => assert!(reason.contains("merge-back"), "{reason}"),
        other => panic!("expected a failure, got {other:?}"),
    }
}

/// A write to `.GIT` (the gitlink, on a
/// case-insensitive volume) points git at a config with a planted
/// `core.fsmonitor`. Host git would run it; merge-back does not, and the
/// attempt fails.
#[tokio::test]
async fn a_tampered_gitlink_never_runs_a_planted_fsmonitor() {
    let r = repo();
    let ws = worktree(&r, "w-evil").await;
    let evil = r.outside.join("evil");
    std::fs::create_dir_all(&evil).expect("mkdir");
    host_git(&evil, &["init", "-q"]);
    let marker = r.outside.join("PWNED");
    host_git(
        &evil,
        &[
            "config",
            "core.fsmonitor",
            &format!("touch {}", marker.display()),
        ],
    );
    let dot = format!(".{}", "GIT");
    std::fs::write(
        ws.join(&dot),
        format!("gitdir: {}\n", evil.join(".git").display()),
    )
    .expect("write");
    std::fs::write(ws.join("b.txt"), "b").expect("write");
    let verdict = verifier(&r, "true")
        .verify(&contract(&dot), &ws, &result())
        .await
        .expect("ran");
    assert!(matches!(verdict, Verdict::Fail { .. }), "{verdict:?}");
    assert!(!marker.exists(), "the planted fsmonitor ran");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn the_acceptance_command_runs_confined_to_the_worktree() {
    let r = repo();
    let ws = worktree(&r, "w-confined").await;
    std::fs::write(ws.join("b.txt"), "b").expect("write");
    let escaped = r.outside.join("escaped");
    let command = format!("touch {} ; true", escaped.display());
    let _ = verifier(&r, &command)
        .verify(&contract("b.txt"), &ws, &result())
        .await
        .expect("ran");
    assert!(
        !escaped.exists(),
        "the acceptance command wrote outside its worktree"
    );
}

/// An attempt that changes cargo's or rustup's configuration is refused even
/// when the contract declared the file: merged, it would apply to the host's
/// own cargo and to every later build. The check never runs (its marker is
/// never written), and the verdict tells the model what to do instead.
#[tokio::test]
async fn an_attempt_changing_cargo_or_rustup_configuration_is_refused() {
    let r = repo();
    for (i, path) in [
        ".cargo/config.toml",
        "crates/a/.cargo/config",
        "rust-toolchain.toml",
        "web/.npmrc",
        ".mvn/jvm.config",
        "gradle.properties",
        "Directory.Build.props",
    ]
    .into_iter()
    .enumerate()
    {
        let ws = worktree(&r, &format!("w-config-{i}")).await;
        let file = ws.join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, "[build]\nrustc-wrapper = \"/bin/true\"\n").expect("write");
        let verdict = verifier(&r, "touch CHECK_RAN")
            .verify(&contract(path), &ws, &result())
            .await
            .expect("ran");
        match verdict {
            Verdict::Fail { reason } => {
                assert!(reason.contains(path), "{reason}");
                assert!(reason.contains("never merges"), "{reason}");
                assert!(reason.contains("say so in your result"), "{reason}");
            }
            other => panic!("{path} was not refused: {other:?}"),
        }
        assert!(!ws.join("CHECK_RAN").exists(), "the check ran for {path}");
    }
    assert!(build_config_changes(&["src/lib.rs".into(), "Cargo.toml".into()]).is_empty());
    // A project manifest of another build tool is ordinary work.
    assert!(
        build_config_changes(&[
            "package.json".into(),
            "pom.xml".into(),
            "go.work".into(),
            "App/App.csproj".into(),
        ])
        .is_empty()
    );
}

#[test]
fn scope_compares_as_the_volume_resolves_names() {
    let c = contract("src/Lib.rs");
    assert!(scope_violations(&["src/lib.rs".into()], &c, false).is_empty());
    assert_eq!(
        scope_violations(&["src/other.rs".into(), ".GIT".into()], &c, false),
        vec!["src/other.rs".to_owned(), ".GIT".to_owned()]
    );
}

/// Decision 0094: in Bypass a declared `.gitattributes` is an ordinary
/// in-scope change and an undeclared one is outside the scope; `.git` stays
/// refused when declared; outside Bypass the declared one is refused.
#[test]
fn bypass_judges_tracked_git_files_by_the_declared_scope() {
    let mut c = contract(".gitattributes");
    c.writes.push(kgraf_orchestrator::WriteTarget {
        path: ".git/config".into(),
        node: None,
    });
    let changed = |p: &[&str]| p.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert!(scope_violations(&changed(&[".gitattributes"]), &c, true).is_empty());
    assert_eq!(
        scope_violations(&changed(&[".gitmodules", ".git/config"]), &c, true),
        changed(&[".gitmodules", ".git/config"])
    );
    assert_eq!(
        scope_violations(&changed(&[".gitattributes"]), &c, false),
        changed(&[".gitattributes"])
    );
}

// ---- The host API (new with the extraction). ----

/// A repository with one commit, its price table (llama3 priced), and a
/// configuration on one local route (no credential, nothing called).
fn project() -> (tempfile::TempDir, RunConfig) {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).expect("mkdir");
    host_git(&root, &["init", "-q", "-b", "main"]);
    // The owner's identity, set in the repository itself: the host reads it
    // for merge-back commits and refuses to build without one, so a fixture
    // that relied on the user's global git config failed wherever none is
    // set (root in a bare Linux container).
    host_git(&root, &["config", "user.name", "Test Owner"]);
    host_git(&root, &["config", "user.email", "owner@example.invalid"]);
    std::fs::write(root.join("README.md"), "# t\n").expect("write");
    host_git(&root, &["add", "."]);
    host_git(&root, &["commit", "-q", "-m", "init"]);
    let prices = dir.path().join("prices.json");
    std::fs::write(
        &prices,
        r#"{"llama3": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1}}"#,
    )
    .expect("prices");
    let config = RunConfig::new(&root, vec!["ollama:llama3".into()], prices);
    (dir, config)
}

/// A host whose generative route is in-process, for a test that builds or
/// prepares a run but does not depend on which model answers. A build dials
/// each local route's endpoint first, so a route named `ollama` would make the
/// result depend on whether a server listens on this machine.
fn open_offline(mut config: RunConfig) -> Host {
    config.routes = vec!["mine:llama3".into()];
    Host::open(config)
        .expect("open")
        .with_route_provider("mine", Arc::new(ShellCaller))
        .expect("in-process route")
}

/// `Host::with_local_endpoint` moves the readiness dial of a local route to
/// a listener the test owns (so no server on this machine is needed), and
/// refuses a route that is not a credential-free catalog route and a URL that
/// names no http endpoint.
#[test]
fn a_local_endpoint_moves_the_readiness_dial_and_refuses_what_is_not_local() {
    let listening = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/v1", listening.local_addr().expect("addr"));
    let host = Host::open(project().1)
        .expect("open")
        .with_local_endpoint("ollama", &url)
        .expect("a local route");
    assert_eq!(host.check_reachable(), Ok(()));
    let closed = Host::open(project().1)
        .expect("open")
        .with_local_endpoint("ollama", "http://127.0.0.1:0/v1")
        .expect("a local route");
    assert!(matches!(
        closed.check_reachable(),
        Err(NotReady::Unreachable { .. })
    ));
    let refused = |route: &str, url: &str| {
        Host::open(project().1)
            .expect("open")
            .with_local_endpoint(route, url)
            .err()
    };
    assert!(refused("openai", &url).is_some(), "a hosted route");
    assert!(refused("nowhere", &url).is_some(), "not in the catalog");
    assert!(refused("ollama", "unix:///tmp/s").is_some(), "not http");
}

fn spec(s: &str) -> RouteSpec {
    RouteSpec::parse(s).expect("spec")
}

/// Decision 0067 (a live rerun):
/// `RunConfig::new` leaves `sessions` at its default, now on -- a rerun, with
/// the run's own admission caps sized from a dry run of the reservation math,
/// showed the steer reaching the worker, a question the orchestrator itself
/// answered (a ledgered call whose answer is the tool result), and a subagent
/// admitted, run and merged back, all in one run
/// so a `Host` built
/// from it computes a `SessionLine` (`host.rs`'s `Host::session_line`, called
/// from `build_base`). `sessions: false` still builds none, checked below.
#[test]
fn a_default_run_config_builds_a_session_line() {
    let (_dir, config) = project();
    assert!(
        config.sessions,
        "RunConfig::new must leave the sessions flag at its documented default: on"
    );
    let host = Host::open(config).expect("open");
    assert!(host.session_line().is_some());
}

/// The other half of the pair above: turning the flag off still builds no
/// `SessionLine` at all, not one that is merely never bound (a
/// worker is never shown a tool this run cannot answer).
#[test]
fn sessions_off_builds_no_session_line() {
    let (_dir, mut config) = project();
    config.sessions = false;
    let host = Host::open(config).expect("open");
    assert!(host.session_line().is_none());
}

/// Decision 0067 two `Host`s, as two
/// open projects in one shell process, sharing one `MachineGate` cannot
/// together run more live agents than the gate's own limit, and the next
/// admission is refused before spawn. A fresh, explicit
/// `MachineGate::new(...)` is used, never `process_machine_gate`'s
/// process-wide singleton: `cargo test` runs a crate's tests as threads in
/// one process, so a test that cares about admission limits must never
/// couple to another test's use of the shared one.
#[test]
fn two_hosts_sharing_one_machine_gate_cannot_together_exceed_it() {
    let gate = MachineGate::new(kgraf_core::ParallelismLimits::derive(3, 3, 3));
    let (_dir_a, mut config_a) = project();
    config_a.max_agents = 10;
    config_a.max_items = 10;
    let (_dir_b, mut config_b) = project();
    config_b.max_agents = 10;
    config_b.max_items = 10;
    let host_a =
        Host::open_with_machine_gate(config_a, None, gate.clone()).expect("open project a");
    let host_b =
        Host::open_with_machine_gate(config_b, None, gate.clone()).expect("open project b");

    let held = host_a
        .admission()
        .gate()
        .try_admit(2)
        .expect("project a takes two of the gate's three slots");
    assert_eq!(gate.agents_live(), 2);
    let refused = host_b
        .admission()
        .gate()
        .try_admit(2)
        .expect_err("two more would make four, over the shared gate's three");
    assert!(matches!(
        refused,
        kgraf_orchestrator::Refusal::LiveAgentCeiling { .. }
    ));
    assert_eq!(
        gate.agents_live(),
        2,
        "the refused admission touched neither host's count"
    );

    // Moving the slider down: nothing already admitted is dropped, and the
    // next admission is refused under the lower ceiling.
    gate.set_limits(kgraf_core::ParallelismLimits::derive(1, 1, 1));
    assert_eq!(gate.agents_live(), 2, "nothing already admitted is killed");
    assert!(
        host_b.admission().gate().try_admit(1).is_err(),
        "refused under the lowered ceiling"
    );

    // Moving the slider up: a waiting admission goes through at once.
    gate.set_limits(kgraf_core::ParallelismLimits::derive(3, 3, 3));
    let admitted_b = host_b
        .admission()
        .gate()
        .try_admit(1)
        .expect("admitted once the ceiling rises");

    host_a.admission().gate().release(held);
    host_b.admission().gate().release(admitted_b);
    assert_eq!(gate.agents_live(), 0);
}

/// Decision 0067 (the slider maximum comes from the loaded models and their
/// rate limits as well as this machine): a route with a known, configured rate
/// limit binds `max_agents` below what the machine's own memory and
/// descriptor bounds would allow alone, and the capacity breakdown names
/// that route as the binding bound (what `parallelism_controls` shows
/// verbatim in Settings).
#[test]
fn a_configured_route_rate_limit_binds_max_agents_below_the_machine_bound() {
    let (dir, mut config) = project();
    config.routes = vec!["kimi:kimi-k3".to_owned()];
    config.rate_limits = vec!["kimi=6".to_owned()]; // 6 requests/minute: tiny
    let store_path = dir.path().join("shared-rate-limits.sqlite3");
    let gate = MachineGate::new(kgraf_core::ParallelismLimits::derive(100, 100, 100));
    let host = Host::open_with_machine_gate(config, Some(&store_path), gate).expect("open");

    let inputs: Vec<kgraf_capacity::RouteInputs> = host
        .route_capacity_inputs()
        .into_iter()
        .map(|r| r.inputs)
        .collect();
    assert!(
        inputs.iter().any(|r| r.route == "kimi"),
        "the configured route is present: {inputs:?}"
    );

    // A generous machine: memory, descriptors and disk are effectively
    // unlimited here, so only the route's own rate limit can bind.
    let machine = kgraf_capacity::MachineInputs {
        cores: 16,
        mem_available: 64 * 1024 * 1024 * 1024,
        mem_total: 64 * 1024 * 1024 * 1024,
        fd_soft: 1_000_000,
        fd_in_use: 0,
        disk_free: 500 * 1024 * 1024 * 1024,
        disk_total: 1_000 * 1024 * 1024 * 1024,
        bytes_per_worktree: 1024 * 1024 * 1024,
        mem_per_agent: kgraf_capacity::DEFAULT_MEM_PER_AGENT,
        mem_per_job: kgraf_capacity::DEFAULT_MEM_PER_JOB,
        fd_per_agent: kgraf_capacity::DEFAULT_FD_PER_AGENT,
    };
    let capacity = kgraf_capacity::compute(&inputs, &machine);
    assert!(
        capacity.max_agents < 100,
        "the configured 6 rpm route must bind below the generous machine bounds: {capacity:?}"
    );
    let route_bound = capacity
        .bounds
        .iter()
        .find(|b| b.name == "route")
        .expect("a route bound exists");
    assert_eq!(route_bound.source, kgraf_capacity::LimitSource::Configured);
    assert_eq!(
        capacity.max_agents, route_bound.value,
        "the route bound is the one that binds"
    );
    assert!(
        route_bound.detail.contains("kimi"),
        "Settings shows this detail verbatim; it must name the binding route: {}",
        route_bound.detail
    );
}

/// A vendor's own reported header limit must never *raise* the effective bound above what the user
/// configured. Before this fix, `route_capacity_inputs` preferred the header
/// unconditionally, so a user capped at 6 rpm but whose vendor reports a
/// header limit of 500 would see 500 used instead: about an 80x increase in
/// the number of agents the machine would actually admit against that
/// route, entirely defeating the user's own `--rate-limit`.
///
/// Exercises `tighter_rpm` directly (not through a real `Host` and
/// `kgraf_provider::limits`'s process-wide registry): that registry is
/// shared by every test in this binary, and `cargo test` runs them
/// concurrently, so two tests recording under the same catalog route's real
/// base URL could otherwise race each other's reads. The pure function has
/// no such shared state to race over.
#[test]
fn the_tighter_of_a_header_and_a_configured_limit_wins_never_the_looser_one() {
    assert_eq!(
        crate::host::tighter_rpm(Some(500), Some(6)),
        Some((6, kgraf_capacity::LimitSource::Configured)),
        "the tighter, user-configured 6 rpm must win over the looser 500 rpm header, \
         and its source must be reported as Configured, not Header"
    );
}

/// The reverse: when the header is the *tighter* one, it wins, and is
/// reported as `Header`.
#[test]
fn a_tighter_header_limit_wins_over_a_looser_configured_one() {
    assert_eq!(
        crate::host::tighter_rpm(Some(3), Some(60)),
        Some((3, kgraf_capacity::LimitSource::Header)),
        "the tighter header 3 rpm must win over the looser configured 60 rpm"
    );
}

/// Either side alone still resolves, and neither present is `None`
/// (`kgraf-capacity`'s own "unmeasured" fallback then applies).
#[test]
fn tighter_rpm_falls_through_when_only_one_side_or_neither_is_known() {
    assert_eq!(
        crate::host::tighter_rpm(Some(500), None),
        Some((500, kgraf_capacity::LimitSource::Header))
    );
    assert_eq!(
        crate::host::tighter_rpm(None, Some(6)),
        Some((6, kgraf_capacity::LimitSource::Configured))
    );
    assert_eq!(crate::host::tighter_rpm(None, None), None);
}

/// End to end, through a real `Host`: a header observation that is looser
/// than the configured limit does not leak into `route_capacity_inputs`'
/// output (proves the wiring calls `tighter_rpm`, not only the function in
/// isolation). Safe under `cargo test`'s concurrent execution: no other test
/// in this file records under `kimi`'s real base URL (the two tests above
/// that used to were rewritten to call `tighter_rpm` directly instead, for
/// exactly this reason), so this is the sole reader and writer of that key.
#[test]
fn route_capacity_inputs_wires_the_header_through_tighter_rpm() {
    kgraf_provider::limits::clear_for_test();
    let base_url = "https://api.moonshot.ai/v1"; // kimi's catalog base_url
    kgraf_provider::limits::record(
        base_url,
        kgraf_provider::ObservedLimits {
            requests_limit: Some(500),
            ..Default::default()
        },
    );
    let (dir, mut config) = project();
    config.routes = vec!["kimi:kimi-k3".to_owned()];
    config.rate_limits = vec!["kimi=6".to_owned()];
    let store_path = dir.path().join("shared-rate-limits.sqlite3");
    let gate = MachineGate::new(kgraf_core::ParallelismLimits::derive(100, 100, 100));
    let host = Host::open_with_machine_gate(config, Some(&store_path), gate).expect("open");

    let kimi = host
        .route_capacity_inputs()
        .into_iter()
        .find(|r| r.inputs.route == "kimi")
        .expect("kimi is configured");
    assert_eq!(
        kimi.inputs.rpm,
        Some((6, kgraf_capacity::LimitSource::Configured)),
        "the tighter, user-configured 6 rpm must win over the looser 500 rpm header: {:?}",
        kimi.inputs.rpm
    );
}

/// Every route whose catalog base URL holds a placeholder: a limit or a 429 an
/// adapter recorded under its resolved URL is found by the host. The catalog
/// text (`{resource}`, `{region}`, `{project}`) is what the host used to look
/// up, and never matched.
#[test]
fn a_recorded_429_and_limit_are_found_for_every_placeholder_route() {
    fn env(name: &str) -> Option<String> {
        match name {
            "KGRAF_VERTEX_CREDENTIALS" => Some("adc".to_owned()),
            "KGRAF_VERTEX_PROJECT" => Some("a-project".to_owned()),
            "KGRAF_VERTEX_REGION" => Some("us-central1".to_owned()),
            "KGRAF_AZURE_OPENAI_API_KEY" => Some("an-azure-key-value".to_owned()),
            "KGRAF_AZURE_OPENAI_RESOURCE" => Some("a-resource".to_owned()),
            "KGRAF_AZURE_OPENAI_API_VERSION" => Some("v1".to_owned()),
            "KGRAF_AZURE_OPENAI_DEPLOYMENTS" => Some("dep=some-model".to_owned()),
            "AWS_ACCESS_KEY_ID" => Some("AKIDEXAMPLE".to_owned()),
            "AWS_SECRET_ACCESS_KEY" => Some("a-secret-access-key".to_owned()),
            "AWS_REGION" => Some("us-west-2".to_owned()),
            _ => None,
        }
    }
    let routes = [
        "azure-openai:dep",
        "azure-openai-responses:dep",
        "bedrock:m",
        "vertex-gemini:m",
        "vertex-anthropic:m",
    ];
    let (dir, mut config) = project();
    config.routes = routes.iter().map(|r| (*r).to_owned()).collect();
    let store_path = dir.path().join("shared-rate-limits.sqlite3");
    let gate = MachineGate::new(kgraf_core::ParallelismLimits::derive(100, 100, 100));
    let host = Host::open_with_machine_gate(config, Some(&store_path), gate).expect("open");
    let lookup: &dyn Fn(&str) -> Option<String> = &env;

    // The key an adapter records under is what its own configuration holds.
    for spec in routes {
        let spec = RouteSpec::parse(spec).expect("spec");
        let entry = kgraf_fleet::entry(&spec.route).expect("entry");
        let credential = kgraf_fleet::credential_from(entry, env).expect("credential");
        let config = kgraf_fleet::configure(entry, &credential, vec![spec.model.clone()])
            .expect("configure");
        let recorded_under = if spec.route.starts_with("azure") {
            kgraf_provider::limits::scoped_key(&config.base_url, &spec.model)
        } else {
            config.base_url
        };
        kgraf_provider::limits::record(
            &recorded_under,
            kgraf_provider::ObservedLimits {
                requests_limit: Some(77),
                ..Default::default()
            },
        );
        kgraf_provider::limits::record_429(&recorded_under, 4_242);

        let input = host
            .route_capacity_inputs_with(lookup)
            .into_iter()
            .find(|r| r.inputs.route == spec.route)
            .expect("route present");
        assert_eq!(
            input.inputs.rpm,
            Some((77, kgraf_capacity::LimitSource::Header)),
            "{}: the recorded limit was not found",
            spec.route
        );
        let key = input.base_url.expect("a key");
        assert_eq!(key, recorded_under, "{}", spec.route);
        assert_eq!(
            kgraf_provider::limits::last_429(&key),
            Some(4_242),
            "{}: the recorded 429 was not found",
            spec.route
        );
    }
}

/// A placeholder route whose settings do not resolve reports no limit and no
/// key, rather than the catalog text.
#[test]
fn a_placeholder_route_with_no_settings_reports_no_limit_key() {
    let (dir, mut config) = project();
    config.routes = vec!["azure-openai:dep".to_owned()];
    let store_path = dir.path().join("shared-rate-limits.sqlite3");
    let gate = MachineGate::new(kgraf_core::ParallelismLimits::derive(100, 100, 100));
    let host = Host::open_with_machine_gate(config, Some(&store_path), gate).expect("open");
    let none = |_: &str| None;
    let input = host
        .route_capacity_inputs_with(&none)
        .into_iter()
        .next()
        .expect("route");
    assert_eq!(input.base_url, None);
    assert_eq!(input.inputs.rpm, None);
}

/// A route with several Azure deployments reads every deployment's key: the
/// tightest limit, and the newest 429 wherever it happened.
#[test]
fn an_azure_route_with_two_deployments_reads_both_keys() {
    fn env(name: &str) -> Option<String> {
        match name {
            "KGRAF_AZURE_OPENAI_API_KEY" => Some("an-azure-key-value".to_owned()),
            "KGRAF_AZURE_OPENAI_RESOURCE" => Some("two-deps".to_owned()),
            "KGRAF_AZURE_OPENAI_API_VERSION" => Some("v1".to_owned()),
            "KGRAF_AZURE_OPENAI_DEPLOYMENTS" => Some("da=model-a,db=model-b".to_owned()),
            _ => None,
        }
    }
    let (dir, mut config) = project();
    config.routes = vec!["azure-openai:da".to_owned(), "azure-openai:db".to_owned()];
    let store_path = dir.path().join("shared-rate-limits.sqlite3");
    let gate = MachineGate::new(kgraf_core::ParallelismLimits::derive(100, 100, 100));
    let host = Host::open_with_machine_gate(config, Some(&store_path), gate).expect("open");
    let lookup: &dyn Fn(&str) -> Option<String> = &env;
    let entry = kgraf_fleet::entry("azure-openai").expect("entry");
    let key = |deployment: &str| kgraf_fleet::limit_key(entry, env, deployment).expect("key");
    let (key_a, key_b) = (key("da"), key("db"));
    assert_ne!(key_a, key_b);
    for (k, rpm) in [(&key_a, 300), (&key_b, 60)] {
        kgraf_provider::limits::record(
            k,
            kgraf_provider::ObservedLimits {
                requests_limit: Some(rpm),
                ..Default::default()
            },
        );
    }
    kgraf_provider::limits::record_429(&key_a, 1_000);
    kgraf_provider::limits::record_429(&key_b, 2_000);
    let input = host
        .route_capacity_inputs_with(lookup)
        .into_iter()
        .find(|r| r.inputs.route == "azure-openai")
        .expect("route");
    assert_eq!(
        input.inputs.rpm,
        Some((60, kgraf_capacity::LimitSource::Header)),
        "the tightest deployment limit"
    );
    assert_eq!(
        input.base_url.as_deref(),
        Some(key_b.as_str()),
        "newest 429"
    );
}

/// Decision 0089: a model route's limits are read too, under the key its
/// adapter records them under (the model's own, since OpenAI states limits per
/// model), and a route declared only as a model route still reports capacity.
#[test]
fn model_routes_are_read_for_capacity_under_their_per_model_keys() {
    let (dir, mut config) = project();
    config.routes = vec!["openai:chat-model".to_owned()];
    config.model_routes = vec![
        "embedding=openai:embed-model".to_owned(),
        "rerank=together:rank-model".to_owned(),
    ];
    let store_path = dir.path().join("shared-rate-limits.sqlite3");
    let gate = MachineGate::new(kgraf_core::ParallelismLimits::derive(100, 100, 100));
    let host = Host::open_with_machine_gate(config, Some(&store_path), gate).expect("open");
    let env = |_: &str| Some("a-credential-value".to_owned());
    let lookup: &dyn Fn(&str) -> Option<String> = &env;
    let key = |route: &str, kind, model: &str| {
        kgraf_fleet::kind_limit_key(kgraf_fleet::entry(route).expect("entry"), env, kind, model)
            .expect("key")
    };
    let chat = key("openai", kgraf_core::ModelKind::GENERATIVE, "chat-model");
    let embed = key("openai", kgraf_core::ModelKind::EMBEDDING, "embed-model");
    let rank = key("together", kgraf_core::ModelKind::RERANK, "rank-model");
    assert_ne!(chat, embed, "an embedding model has its own key");
    for (k, rpm) in [(&chat, 500), (&embed, 40), (&rank, 90)] {
        kgraf_provider::limits::record(
            k,
            kgraf_provider::ObservedLimits {
                requests_limit: Some(rpm),
                ..Default::default()
            },
        );
    }
    kgraf_provider::limits::record_429(&embed, 9_000);
    let inputs = host.route_capacity_inputs_with(lookup);
    let openai = inputs
        .iter()
        .find(|r| r.inputs.route == "openai")
        .expect("openai");
    assert_eq!(
        openai.inputs.rpm,
        Some((40, kgraf_capacity::LimitSource::Header)),
        "the tightest of the chat and the embedding model"
    );
    assert_eq!(
        openai.base_url.as_deref(),
        Some(embed.as_str()),
        "newest 429"
    );
    let together = inputs
        .iter()
        .find(|r| r.inputs.route == "together")
        .expect("a route used only as a model route still reports");
    assert_eq!(
        together.inputs.rpm,
        Some((90, kgraf_capacity::LimitSource::Header))
    );
}

/// Seeding rounds once over the hour's calls: ten calls of a fifth of a
/// micro-dollar each seed two micro-dollars, not ten.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seeding_many_small_calls_rounds_their_sum_once() {
    let (_dir, config) = project();
    std::fs::write(
        &config.prices,
        r#"{"e": {"uncached_input": 20000, "cache_read": 0, "cache_write": 0, "output": 0, "reasoning": 0}}"#,
    )
    .expect("prices");
    let state = config.root.join(".kgraf");
    std::fs::create_dir_all(&state).expect("state");
    let ten = kgraf_core::TokenUsage {
        uncached_input: 10,
        ..Default::default()
    };
    {
        let ledger = Ledger::open(&state.join("ledger.db")).expect("ledger");
        for _ in 0..10 {
            ledger
                .begin("fake", kgraf_core::Protocol::Inclusive, "e", "a", "t")
                .expect("begin")
                .settle(&ten, None)
                .expect("settle");
        }
    }
    let host = Host::open(config).expect("open");
    let total = host.seed_trailing_hour().expect("seed");
    assert_eq!(total, kgraf_core::Money::from_micros(2));
}

/// A price file cannot price an Azure deployment by a bare model entry.
#[test]
fn an_azure_route_is_priced_only_under_its_route_scoped_key() {
    let (dir, _config) = project();
    let path = dir.path().join("azure-prices.json");
    std::fs::write(
        &path,
        r#"{"gpt-4o": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1}}"#,
    )
    .expect("prices");
    let table = crate::config::read_prices(&path).expect("read");
    assert!(table.is_priced_on(Some("openai"), "gpt-4o"));
    assert!(!table.is_priced_on(Some("azure-openai"), "gpt-4o"));
    assert!(!table.is_priced_on(Some("azure-openai-responses"), "gpt-4o"));
}

/// Readiness is typed, and builds and calls nothing: an unknown route, a
/// credential absent from the environment. A model with no price is ready
/// (decision 0075).
#[test]
fn readiness_names_the_route_or_the_credential_and_never_the_price() {
    let none = |_: &str| None;
    assert_eq!(
        check_routes(&[spec("ollama:llama3")], &BTreeMap::new(), Some(&none)),
        Ok(())
    );
    assert_eq!(
        check_routes(&[], &BTreeMap::new(), Some(&none)),
        Err(NotReady::NoRoutes)
    );
    assert!(matches!(
        check_routes(&[spec("nowhere:m")], &BTreeMap::new(), Some(&none)),
        Err(NotReady::UnknownRoute { route, .. }) if route == "nowhere"
    ));
    let missing = check_routes(&[spec("kimi:kimi-k2")], &BTreeMap::new(), Some(&none));
    assert!(
        matches!(&missing, Err(NotReady::MissingCredential { route, detail })
            if route == "kimi" && detail.contains("KGRAF_KIMI_API_KEY")),
        "{missing:?}"
    );
    let with_key = |v: &str| (v == "KGRAF_KIMI_API_KEY").then(|| "k".to_owned());
    assert_eq!(
        check_routes(
            &[
                spec("kimi:kimi-k2"),
                spec("ollama:qwen3"),
                spec("ollama:llama3")
            ],
            &BTreeMap::new(),
            Some(&with_key)
        ),
        Ok(()),
        "no route here has a price entry, and none is refused for it"
    );
}

/// The factory: ready for a configured host; `build` refuses a runtime it
/// cannot block on (never panics); on a multi-thread runtime it builds the
/// orchestrator, keeps the build's notices, and `finish_run` does the run's
/// housekeeping once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_factory_builds_on_the_hosts_admission_and_finishes_once() {
    let (_dir, config) = project();
    let host = Arc::new(open_offline(config));
    let factory = HostFactory::new(host.clone());
    factory.configured().await.expect("configured");
    factory.ready("nightly").await.expect("ready");
    let run = RunId::new("f1");
    let orchestrator = factory.build(&run).expect("build");
    drop(orchestrator);
    assert!(
        host.take_notices()
            .iter()
            .any(|n| matches!(n, crate::Notice::NoWorkerConfig(_))),
        "a factory build's notices are kept"
    );
    let done = host.finish_run(&run).expect("the run is known");
    assert_eq!(done.swept.expect("swept"), Vec::<String>::new());
    assert_eq!(done.unreconciled.expect("count"), 0);
    assert!(host.finish_run(&run).is_none(), "finished once");
}

#[test]
fn a_blocking_build_off_a_multi_thread_runtime_is_refused() {
    let (_dir, config) = project();
    let host = Arc::new(Host::open(config).expect("open"));
    let factory = HostFactory::new(host);
    let outside = factory.build(&RunId::new("f2"));
    assert!(
        matches!(&outside, Err(OrchestratorError::Build { detail: m, transient: false }) if m.contains("tokio runtime")),
        "{:?}",
        outside.err()
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let current = rt.block_on(async { factory.build(&RunId::new("f3")) });
    assert!(
        matches!(&current, Err(OrchestratorError::Build { detail: m, transient: false }) if m.contains("multi-thread")),
        "{:?}",
        current.err()
    );
}

/// `start_run` builds, spawns and hands back the run; its end arrives through
/// the handle. A resume of a run with no record fails there, typed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_started_run_ends_through_its_handle() {
    let (_dir, config) = project();
    let host = open_offline(config);
    let mut seen = Vec::new();
    let handle = host
        .start_run(RunId::new("nope"), RunStart::Resume, None, &mut |n| {
            seen.push(n)
        })
        .await
        .expect("started");
    assert_eq!(handle.summary().integration, "kgraf/nope");
    let ended = handle.wait().await;
    assert!(
        matches!(
            ended,
            Err(crate::Error::Orchestrator(OrchestratorError::NoRecord(_)))
        ),
        "{ended:?}"
    );
    assert!(!seen.is_empty(), "the build's notices reach the caller");
}

/// A prepared run refuses an empty instruction, loudly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prepared_run_refuses_an_empty_instruction() {
    let (_dir, config) = project();
    let host = open_offline(config);
    let prepared = host
        .prepare(RunId::new("e1"), None, &mut |_| {})
        .await
        .expect("prepare");
    let refused = prepared
        .run(RunStart::Instruction("  \n".into()))
        .await
        .expect_err("empty");
    assert!(refused.to_string().contains("empty"), "{refused}");
}

/// Answers every ask with Allow, counting them.
struct AllowAll(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl kgraf_runtime::AskResolver for AllowAll {
    async fn answer(
        &self,
        _paused: &kgraf_tools::approval::PausedCall,
    ) -> kgraf_tools::approval::UserAnswer {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        kgraf_tools::approval::UserAnswer::Allow
    }
}

/// The host's own resolver answers the asks of an attempt's approval chain:
/// the write a delegate is denied without it runs once the host allows it.
/// The chain is unchanged; only who answers the ask is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_resolver_answers_the_attempts_asks() {
    let r = repo();
    let state = r._dir.path().join("state");
    std::fs::create_dir_all(&state).expect("state");
    let ledger = Arc::new(Ledger::open(&state.join("ledger.db")).expect("ledger"));
    let mut routes = BTreeMap::new();
    routes.insert(
        "fake:m".to_owned(),
        RouteParts {
            provider: Arc::new(ShellCaller),
            vendor: "fake-asked (fake)".into(),
            vendor_name: "fake-asked".into(),
            model: "m".into(),
        },
    );
    let allow = Arc::new(AllowAll(std::sync::atomic::AtomicUsize::new(0)));
    let envs = ResolverOverride {
        inner: Arc::new(AttemptAssembler {
            root: r.root.clone(),
            state: state.clone(),
            graph_path: state.join("graph.db"),
            indexed: false,
            ledger: ledger.clone(),
            config: WorkerConfig::default(),
            max_output_tokens: 1_000,
            max_checkpoints: crate::limits::DEFAULT_MAX_CHECKPOINTS,
            reasoning: Default::default(),
            read_denies: vec![state.clone()],
            workspaces: r.workspaces.clone(),
            routes,
            extra_tools: Vec::new(),
            session_line: None,
            prefixes: Arc::new(kgraf_runtime::PrefixRegistry::new()),
            machine_gate: test_machine_gate(),
            tbench_bridge_socket: None,
            dependency_refetch: crate::deps::inert_refetch(),
            skills: None,
            skill_directory: Default::default(),
            refusal_directory: Default::default(),
            permissions: Arc::new(kgraf_tools::PermissionControl::manual()),
        }),
        asks: allow.clone(),
    };
    let context = ContextConfig::for_model(200_000).expect("context");
    let ws = worktree(&r, "w-asked").await;
    let route = Route {
        name: "fake:m".into(),
        vendor: "fake".into(),
        env: Arc::new(kgraf_runtime::WorkerEnv::new(
            ledger.clone(),
            Arc::new(ShellCaller),
            Arc::new(kgraf_tools::ToolRegistry::new()),
            assemble_prefix(),
        )),
        context: context.clone(),
        readiness: Arc::new(kgraf_orchestrator::AlwaysReady),
        local: false,
    };
    let built = envs
        .build(&route, &ws, &"w-asked".into(), &"r9/c-ask".into())
        .await
        .expect("built");
    let brief = kgraf_runtime::WorkerBrief {
        task: "r9/c-ask".into(),
        worker: "w-asked".into(),
        instruction: "touch it".into(),
        workspace_root: ws.clone(),
        model: "m".into(),
        slices: vec![],
        skills: vec![],
        step_budget: 4,
        context,
        write_scope: None,
    };
    let mut handle = kgraf_runtime::spawn_worker(
        built.env.clone(),
        brief,
        kgraf_runtime::SpawnOptions::default(),
    )
    .await
    .expect("spawn");
    // Bounded: a call that waits for an answer nobody gives would hold the
    // whole test binary; this way it fails with the events seen so far.
    let mut events = Vec::new();
    let finished = tokio::time::timeout(Duration::from_secs(120), async {
        while let Some(ev) = handle.next_event().await {
            events.push(ev);
        }
    })
    .await
    .is_ok();
    if let Some(closer) = built.closer {
        closer.close().await;
    }
    assert!(finished, "the worker did not end in 120 s: {events:#?}");
    let errors = events
        .iter()
        .filter(|ev| {
            matches!(
                ev,
                kgraf_runtime::WorkerEvent::ToolCallFinished { is_error: true, .. }
            )
        })
        .count();
    assert_eq!(errors, 0, "both calls ran: {events:#?}");
    assert!(ws.join("unasked.txt").exists(), "the allowed write ran");
    assert!(
        allow.0.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "the host's resolver was asked"
    );
}

/// A host's futures can run in a spawned task (the shell's runtime): they are
/// `Send`. Compiled, never called.
#[allow(dead_code)]
fn host_futures_are_send(host: &Host) {
    fn send<T: Send>(_: T) {}
    send(host.prepare(RunId::new("s"), None, &mut |_| {}));
    send(host.start_run(RunId::new("s"), RunStart::Resume, None, &mut |_| {}));
    send(host.read_corpus(&RunId::new("s"), crate::CorpusRequest::default()));
}

/// An in-process provider for a route the configuration does not use would be
/// silently unused: refused. For a used route it needs no catalog entry and no
/// credential, and its model needs no price (decision 0075).
#[test]
fn an_in_process_provider_is_for_a_used_route_and_an_unpriced_model_is_ready() {
    let (_dir, config) = project();
    let refused = Host::open(config.clone())
        .expect("open")
        .with_route_provider("elsewhere", Arc::new(ShellCaller));
    assert!(
        matches!(&refused, Err(crate::Error::Invalid(m)) if m.contains("elsewhere")),
        "{:?}",
        refused.err()
    );
    let mut own = config;
    own.routes = vec!["mine:llama3".into(), "mine:unpriced".into()];
    let host = Host::open(own)
        .expect("open")
        .with_route_provider("mine", Arc::new(ShellCaller))
        .expect("used route");
    assert_eq!(
        host.check_ready(&host.route_set()),
        Ok(()),
        "a model with no price is ready, not refused"
    );
    assert_eq!(host.check_ready(&[spec("mine:llama3")]), Ok(()));
    let catalog = Host::open(project().1)
        .expect("open")
        .with_route_provider("ollama", Arc::new(ShellCaller));
    assert!(
        matches!(&catalog, Err(crate::Error::Invalid(m)) if m.contains("catalog")),
        "a real route is never replaced: {:?}",
        catalog.err()
    );
}

/// Write the price table again with a modification time other than the one
/// read, so the change is seen whatever the file system's time resolution.
fn rewrite_prices(path: &Path, text: &str, seconds_later: u64) {
    let before = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .expect("mtime");
    std::fs::write(path, text).expect("write");
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(before + Duration::from_secs(seconds_later)))
        .expect("set mtime");
}

/// Catalog and prices are whole-process facts, `configured()`'s; the
/// schedule's own `ready()` checks only credentials. A fixed price table is
/// read again when it changes, without reopening the host; a table that no
/// longer reads is refused, never replaced by the old one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_owns_catalog_and_prices_and_reads_a_changed_table() {
    let (_dir, mut config) = project();
    config.routes = vec!["ollama:llama3".into(), "ollama:qwen3".into()];
    let prices = config.prices.clone();
    let host = Arc::new(Host::open(config).expect("open"));
    let factory = HostFactory::new(host.clone());
    factory
        .configured()
        .await
        .expect("qwen3 has no price entry, and that is not a readiness failure");
    factory
        .ready("nightly")
        .await
        .expect("ready checks credentials only; ollama needs none");
    rewrite_prices(
        &prices,
        r#"{"llama3": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1},
            "qwen3": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1}}"#,
        5,
    );
    factory.configured().await.expect("the fixed table applies");
    assert!(host.prices().expect("prices").get("qwen3").is_some());
    rewrite_prices(&prices, "{not json", 10);
    assert!(
        host.prices().is_err(),
        "no fallback: a table that no longer reads is an error, not the old table"
    );
    assert!(
        matches!(
            host.check_configured(),
            Err(NotReady::PricesUnreadable { .. })
        ),
        "a broken table is refused, not replaced by the old one"
    );
    // An unknown route is the process's configuration, not one schedule's.
    let (_d2, mut other) = project();
    other.routes = vec!["nowhere:m".into()];
    let host = Arc::new(Host::open(other).expect("open"));
    assert!(matches!(
        host.check_configured(),
        Err(NotReady::UnknownRoute { .. })
    ));
}

/// The build error of `config` through the factory (on a multi-thread runtime).
fn build_error(config: RunConfig) -> OrchestratorError {
    let host = Arc::new(open_offline(config));
    match HostFactory::new(host).build(&RunId::new("classify")) {
        Ok(_) => panic!("the build was expected to fail"),
        Err(e) => e,
    }
}

/// A settings failure fails the same way every time, so it is not
/// transient: it disables the schedule with its reason. One test per class.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_worker_json_is_a_settings_failure() {
    let (_dir, config) = project();
    let state = config.root.join(".kgraf");
    std::fs::create_dir_all(&state).expect("state");
    std::fs::write(
        state.join("worker.json"),
        r#"{"mcp_servers": [], "mcp_server": []}"#,
    )
    .expect("worker.json");
    let e = build_error(config);
    assert!(
        matches!(&e, OrchestratorError::Build { transient: false, detail } if detail.contains("worker")),
        "{e:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_commit_identity_is_a_settings_failure() {
    let (_dir, config) = project();
    // Set, but empty, in the repository: the owner has no usable identity.
    host_git(&config.root, &["config", "user.name", ""]);
    let e = build_error(config);
    assert!(
        matches!(&e, OrchestratorError::Build { transient: false, detail } if detail.contains("name and email")),
        "{e:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_package_json_is_a_settings_failure() {
    let (_dir, config) = project();
    std::fs::write(config.root.join("package.json"), "{not json").expect("write");
    let e = build_error(config);
    assert!(
        matches!(&e, OrchestratorError::Build { transient: false, detail } if detail.contains("package.json")),
        "{e:?}"
    );
}

#[test]
fn a_provider_the_catalog_cannot_configure_is_a_settings_failure() {
    let e = provider_with("kimi", "", "kimi-k2", &Limiters::new())
        .err()
        .expect("an empty credential cannot configure kimi");
    assert!(matches!(e, crate::Error::Settings(_)), "{e:?}");
    assert!(!e.is_transient());
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A build past its bound leaves nothing behind: its route probe worktree
/// is removed and unregistered, and the stdio MCP server it was starting
/// (which never answers) is killed with its process group. The probe's
/// removal runs on the runtime's blocking pool, and git deletes a worktree's
/// directory before its registration, so the runtime is dropped (which waits
/// for its running blocking tasks) before the end state is checked; no
/// removal step then runs after the test's repository is gone.
#[test]
fn a_timed_out_build_removes_its_probe_and_kills_its_mcp_server() {
    let (dir, config) = project();
    let root = config.root.clone();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let (pid, probes) = rt.block_on(timed_out_build_waits(&dir, config));
    drop(rt);
    assert!(
        !alive(&pid),
        "the MCP server {pid} outlived its aborted build"
    );
    assert_eq!(probes(), 0, "the probe worktree outlived its aborted build");
    let listed = git_out(&root, &["worktree", "list"]);
    assert!(!listed.contains("probe-"), "and its registration: {listed}");
}

/// The build of [`a_timed_out_build_removes_its_probe_and_kills_its_mcp_server`]
/// and the wait for its leftovers to go: the MCP server's pid, and a count of
/// probe worktree directories.
async fn timed_out_build_waits(
    dir: &tempfile::TempDir,
    config: RunConfig,
) -> (String, impl Fn() -> usize + use<>) {
    let state = config.root.join(".kgraf");
    std::fs::create_dir_all(&state).expect("state");
    let pidfile = dir.path().join("server.pid");
    let worker = serde_json::json!({"mcp_servers": [{
        "name": "silent",
        "timeout_ms": 600000,
        "transport": {
            "type": "stdio",
            "command": "sh",
            "args": ["-c", "echo $$ > \"$PIDFILE\"; exec sleep 600"],
            "env": {"PIDFILE": pidfile.to_string_lossy()}
        }
    }]});
    std::fs::write(state.join("worker.json"), worker.to_string()).expect("worker.json");
    let host = Arc::new(open_offline(config).with_build_timeout(Duration::from_secs(3)));
    let e = match HostFactory::new(host).build(&RunId::new("slow")) {
        Ok(_) => panic!("a server that never answers cannot build"),
        Err(e) => e,
    };
    assert!(
        matches!(&e, OrchestratorError::Build { transient: true, detail } if detail.contains("did not finish")),
        "{e:?}"
    );
    let pid = std::fs::read_to_string(&pidfile)
        .expect("the server started")
        .trim()
        .to_owned();
    let worktrees = state.join("worktrees");
    let probes = move || {
        std::fs::read_dir(&worktrees)
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().starts_with("probe-"))
                    .count()
            })
            .unwrap_or(0)
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while (alive(&pid) || probes() > 0) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (pid, probes)
}

/// A build failure the same configuration can get past (an MCP server
/// not answering, the build bound) is transient; a settings failure is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_build_step_is_transient_and_a_settings_failure_is_not() {
    let (_dir, config) = project();
    let state = config.root.join(".kgraf");
    std::fs::create_dir_all(&state).expect("state");
    std::fs::write(
        state.join("worker.json"),
        r#"{"mcp_servers": [{"name": "gone", "transport": {"type": "streamable_http", "url": "http://127.0.0.1:1/mcp"}}]}"#,
    )
    .expect("worker.json");
    let host = Arc::new(open_offline(config.clone()));
    let built = HostFactory::new(host).build(&RunId::new("t1"));
    assert!(
        matches!(&built, Err(OrchestratorError::Build { transient: true, detail }) if detail.contains("gone")),
        "{:?}",
        built.err()
    );
    std::fs::remove_file(state.join("worker.json")).expect("rm");
    let slow = Arc::new(open_offline(config).with_build_timeout(Duration::from_millis(1)));
    let bounded = HostFactory::new(slow).build(&RunId::new("t2"));
    assert!(
        matches!(&bounded, Err(OrchestratorError::Build { transient: true, detail }) if detail.contains("did not finish")),
        "{:?}",
        bounded.err()
    );
}

/// `Host::build` indexes the working tree itself, before every build, so
/// a run never needs a separate `kgraf index` step first: the very first
/// `prepare` already sees the fixture's `README.md` and gets `graph_slice`.
/// A second `prepare` right after sees the same, unchanged index: the
/// incremental indexer's content-hash check leaves an already-current index
/// alone (`kgraf_graph::index`'s own doc comment).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_index_made_before_open_reaches_the_first_build() {
    let (_dir, config) = project();
    let root = config.root.clone();
    std::fs::create_dir_all(root.join(".kgraf")).expect("state dir");
    assert_eq!(
        GraphStore::open(&root.join(".kgraf").join("graph.db"), &root)
            .expect("graph")
            .node_count()
            .expect("count"),
        0,
        "the fixture starts unindexed: `kgraf index` was never run"
    );
    let host = open_offline(config);
    let before = host
        .prepare(RunId::new("i1"), None, &mut |_| {})
        .await
        .expect("prepare");
    assert!(
        before.shape().routes[0]
            .tools
            .iter()
            .any(|t| t == "graph_slice"),
        "the build's own index pass ran before dispatch: {:?}",
        before.shape().routes[0].tools
    );
    let after = host
        .prepare(RunId::new("i2"), None, &mut |_| {})
        .await
        .expect("prepare");
    assert!(
        after.shape().routes[0]
            .tools
            .iter()
            .any(|t| t == "graph_slice"),
        "{:?}",
        after.shape().routes[0].tools
    );
}

/// A probe worktree whose creation finishes after its build was aborted
/// (the task's output is discarded) is removed: the guard is made with the
/// worktree, on the blocking thread, and dropped with the output. That
/// removal runs on a blocking thread.
#[test]
fn a_probe_created_after_an_abort_is_removed() {
    let r = repo();
    assert_an_orphaned_probe_is_removed(r.workspaces.manager.clone(), &r.root);
}

/// The same with a git whose `worktree remove` deletes the directory, waits
/// one second, then runs: the directory is gone for that second while the
/// worktree is still registered. Git's own `worktree remove` has the same
/// order (directory first, registration second), with a window too short to
/// see on an idle machine; under load, a check that stopped waiting when the
/// directory went away could read the registration from inside it.
#[cfg(unix)]
#[test]
fn a_probe_is_unregistered_after_its_directory_is_gone() {
    use std::os::unix::fs::PermissionsExt;
    let r = repo();
    let real = kgraf_sandbox::git::Git::discover().expect("git");
    let wrapper = r.outside.join("slow-remove-git");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             case \" $* \" in *' worktree remove '*)\n  \
             for last; do :; done\n  \
             rm -rf \"$last\"\n  \
             sleep 1\n;;\nesac\n\
             exec '{}' \"$@\"\n",
            real.exe().display()
        ),
    )
    .expect("wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let manager = WorkspaceManager::open_with_git(
        &r.root,
        &r.outside.join("slow-ws"),
        kgraf_sandbox::git::Git::at(wrapper),
    )
    .expect("manager");
    assert_an_orphaned_probe_is_removed(Arc::new(manager), &r.root);
}

/// Create a probe worktree whose guard nobody awaits, as after an abort, and
/// check that it is removed and unregistered. The removal runs on the
/// runtime's blocking pool, so the runtime is dropped before the check:
/// dropping a tokio runtime waits for its running blocking tasks, so the
/// removal has ended and none of its git steps runs after the test's
/// repository is gone. The directory going away shows only that the removal
/// started: git unregisters a worktree after it deletes the directory.
fn assert_an_orphaned_probe_is_removed(manager: Arc<WorkspaceManager>, root: &Path) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let path = manager.base().join("probe-orphan");
    rt.block_on(async {
        drop(ProbeGuard::create(manager.clone(), "probe-orphan".into()));
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut seen = false;
        while std::time::Instant::now() < deadline {
            seen |= path.exists();
            if seen && !path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    drop(rt);
    assert!(!path.exists(), "the orphaned probe worktree was left");
    assert!(
        !git_out(root, &["worktree", "list"]).contains("probe-orphan"),
        "and unregistered"
    );
}

/// Seeding reads the last hour's calls and never fails on a call whose
/// spend is unknown: a settled call counts at its price; a call on a model
/// the table does not price, and an unreconciled call, count at the worst
/// case, each with a visible notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seeding_counts_unknown_spend_at_the_worst_case_with_a_notice() {
    let (_dir, config) = project();
    // Real-sized rates ($3 and $15 per million), so costs are not rounded away.
    std::fs::write(
        &config.prices,
        r#"{"llama3": {"uncached_input": 3000000, "cache_read": 300000, "cache_write": 3750000, "output": 15000000, "reasoning": 15000000}}"#,
    )
    .expect("prices");
    let state = config.root.join(".kgraf");
    std::fs::create_dir_all(&state).expect("state");
    let usage = kgraf_core::TokenUsage {
        uncached_input: 1_000,
        text_output: 100,
        ..Default::default()
    };
    {
        let ledger = Ledger::open(&state.join("ledger.db")).expect("ledger");
        let begin = |model: &str| {
            ledger
                .begin("fake", kgraf_core::Protocol::Additive, model, "a", "t")
                .expect("begin")
        };
        begin("llama3").settle(&usage, None).expect("settle");
        begin("not-in-the-table")
            .settle(&usage, None)
            .expect("settle");
        drop(begin("llama3")); // unreconciled: its spend is not known
    }
    let host = Host::open(config).expect("open");
    let prices = host.prices().expect("prices");
    let priced = prices.cost("llama3", &usage).expect("priced");
    let worst = host
        .worst_case_call(&prices, None, "llama3")
        .expect("worst");
    assert!(priced > kgraf_core::Money::ZERO);
    assert!(worst > priced, "the worst case bounds a real call");
    let total = host
        .seed_trailing_hour()
        .expect("seeding never fails on unknown spend");
    // llama3 is the only (so the dearest) priced model: both estimates are `worst`.
    assert_eq!(total, priced + worst + worst);
    let estimates: Vec<_> = host
        .take_notices()
        .into_iter()
        .filter_map(|n| match n {
            crate::Notice::EstimatedSpend { model, amount, .. } => Some((model, amount)),
            _ => None,
        })
        .collect();
    assert_eq!(estimates.len(), 2, "{estimates:?}");
    assert!(
        estimates
            .iter()
            .any(|(m, a)| m == "not-in-the-table" && *a == worst)
    );
    assert!(estimates.iter().any(|(m, a)| m == "llama3" && *a == worst));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_millis() as i64;
    assert_eq!(host.admission().budget().spent_last_hour(now), total);
    // Only the last hour is read.
    assert_eq!(
        host.ledger()
            .calls_finished_since(now + 60_000)
            .expect("since")
            .len(),
        0
    );
}

/// A table model whose rate tier lies below the window cannot bound a call at
/// its base rate. It does not fail the bound for an unpriced model, and a
/// tiered model's own worst case is an error the seeding skips, not one it
/// propagates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tiered_model_below_the_window_does_not_fail_the_worst_case() {
    let (_dir, config) = project();
    let host = Host::open(config).expect("open");
    let rate = kgraf_core::ModelPrice {
        uncached_input: 3_000_000,
        cache_read: 300_000,
        cache_write: 3_000_000,
        output: 15_000_000,
        reasoning: 15_000_000,
    };
    let mut prices = kgraf_core::PriceTable::new();
    prices.set("flat", rate);
    prices.set("tiered", rate);
    prices.set_tier_threshold("tiered", 1);
    let flat = host
        .worst_case_call(&prices, None, "flat")
        .expect("a flat model prices");
    // The unpriced model is bounded by the flat one; the tiered one is skipped.
    assert_eq!(
        host.worst_case_call(&prices, None, "unlisted")
            .expect("bounded"),
        flat
    );
    assert!(host.worst_case_call(&prices, None, "tiered").is_err());
}

/// A price table that prices nothing does not close the host: a call on an
/// unpriced model is not seeded (its cost is unknown, never `$0`), and opening
/// is not refused for want of a price to bound it with.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seeding_with_an_empty_price_table_skips_unpriced_calls_and_succeeds() {
    let (_dir, config) = project();
    std::fs::write(&config.prices, "{}").expect("prices");
    let state = config.root.join(".kgraf");
    std::fs::create_dir_all(&state).expect("state");
    let usage = kgraf_core::TokenUsage {
        uncached_input: 1_000,
        text_output: 100,
        ..Default::default()
    };
    {
        let ledger = Ledger::open(&state.join("ledger.db")).expect("ledger");
        ledger
            .begin(
                "fake",
                kgraf_core::Protocol::Additive,
                "local-model",
                "a",
                "t",
            )
            .expect("begin")
            .settle(&usage, None)
            .expect("settle");
    }
    let host = Host::open(config).expect("open");
    let total = host
        .seed_trailing_hour()
        .expect("an unpriced call is not a reason to fail");
    assert_eq!(total, kgraf_core::Money::ZERO);
    assert!(host.take_notices().is_empty());
}

/// A credential missing when the run is built (removed after readiness)
/// is its own class, which the factory seam carries as `MissingCredential`.
#[test]
fn a_credential_missing_at_build_time_is_a_missing_credential() {
    let e = provider_from("kimi", "kimi-k2", &Limiters::new(), |_| None)
        .err()
        .expect("no credential");
    assert!(matches!(e, crate::Error::Credential(_)), "{e:?}");
    assert!(!e.is_transient());
    let seam = crate::host::factory_error(e);
    assert!(
        matches!(&seam, OrchestratorError::MissingCredential(d) if d.contains("KGRAF_KIMI_API_KEY")),
        "{seam:?}"
    );
}

// `.kgraf` must never land in a plain `git add -A` (`ensure_local_exclude`).

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("mkdir");
    host_git(dir, &["init", "-q", "-b", "main"]);
    host_git(dir, &["config", "user.name", "t"]);
    host_git(dir, &["config", "user.email", "t@t"]);
    std::fs::write(dir.join("README.md"), "# t\n").expect("write");
    host_git(dir, &["add", "."]);
    host_git(dir, &["commit", "-q", "-m", "init"]);
}

fn exclude_path(root: &Path) -> PathBuf {
    root.join(".git").join("info").join("exclude")
}

/// The first command against a fresh repository adds the line once; a second
/// call (as a later command in the same repo would make) leaves exactly one.
#[test]
fn state_dir_adds_the_exclude_line_once_and_is_idempotent() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    init_repo(&root);

    state_dir(&root).expect("first call");
    let text = std::fs::read_to_string(exclude_path(&root)).expect("exclude");
    assert_eq!(
        text.lines().filter(|l| *l == "/.kgraf/").count(),
        1,
        "{text:?}"
    );

    state_dir(&root).expect("second call");
    let text = std::fs::read_to_string(exclude_path(&root)).expect("exclude");
    assert_eq!(
        text.lines().filter(|l| *l == "/.kgraf/").count(),
        1,
        "run twice, still one line: {text:?}"
    );

    // The user's own tracked .gitignore is never touched.
    assert!(!root.join(".gitignore").exists());
}

/// An exclude file that exists but cannot be read as text fails loud and is
/// left byte for byte as it was: it may hold the user's own local ignores.
#[test]
fn an_unreadable_exclude_file_fails_and_is_never_overwritten() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    init_repo(&root);
    let path = exclude_path(&root);
    let original: &[u8] = b"my-local-ignore\n\xff\xfe not utf-8\n";
    std::fs::write(&path, original).expect("write exclude");

    assert!(
        state_dir(&root).is_err(),
        "an unreadable exclude file must fail"
    );
    assert_eq!(std::fs::read(&path).expect("read back"), original);
}

/// A linked worktree resolves `info/exclude` back to the common directory the
/// worktree it was made from uses, so the line still lands in one place.
#[test]
fn a_linked_worktree_resolves_the_common_info_exclude() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    init_repo(&root);
    let linked = dir.path().join("linked");
    host_git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            linked.to_str().expect("utf8"),
        ],
    );

    state_dir(&linked).expect("state dir from the linked worktree");

    // The line lands in the *common* exclude file (the main checkout's,
    // already seeded with git's own commented template), not a copy under the
    // linked worktree's own admin directory.
    let text = std::fs::read_to_string(exclude_path(&root)).expect("common exclude");
    assert_eq!(
        text.lines().filter(|l| *l == "/.kgraf/").count(),
        1,
        "{text:?}"
    );
    assert!(
        !linked.join(".git").is_dir(),
        "a linked worktree's .git is a gitlink file, not its own admin directory"
    );
}

/// A repository that already tracks files under `.kgraf` gets a warning
/// naming the risk and the untracking command; the index is never touched.
#[test]
fn already_tracked_state_files_produce_a_named_warning() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).expect("mkdir");
    host_git(&root, &["init", "-q", "-b", "main"]);
    host_git(&root, &["config", "user.name", "t"]);
    host_git(&root, &["config", "user.email", "t@t"]);
    std::fs::create_dir_all(root.join(".kgraf")).expect("mkdir state");
    std::fs::write(root.join(".kgraf").join("ledger.db"), "secret").expect("write");
    host_git(&root, &["add", "-A"]);
    host_git(&root, &["commit", "-q", "-m", "oops, committed state"]);

    let git = kgraf_sandbox::git::Git::discover().expect("git");
    let warning =
        crate::workspaces::tracked_state_warning(&git, &root).expect("warning for tracked state");
    assert!(warning.contains(".kgraf"), "{warning}");
    assert!(warning.contains("git rm -r --cached .kgraf"), "{warning}");

    // The warning is informational only: the index still names the file.
    let tracked = git_out(&root, &["ls-files", "--", ".kgraf"]);
    assert!(tracked.contains("ledger.db"), "{tracked}");
}

/// A repository with nothing yet under `.kgraf` gets no warning.
#[test]
fn a_clean_repository_gets_no_tracked_state_warning() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    init_repo(&root);
    state_dir(&root).expect("state");
    let git = kgraf_sandbox::git::Git::discover().expect("git");
    assert!(crate::workspaces::tracked_state_warning(&git, &root).is_none());
}

/// After a command that creates state, `git status --porcelain` shows nothing
/// under `.kgraf`: the exclude line does its job end to end.
#[test]
fn git_status_shows_nothing_under_state_dir_after_state_dir_runs() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    init_repo(&root);
    let state = state_dir(&root).expect("state");
    std::fs::write(state.join("ledger.db"), "data").expect("write");
    std::fs::write(state.join("allow-rules.json"), "{}").expect("write");

    let status = git_out(&root, &["status", "--porcelain"]);
    assert!(
        !status.contains(".kgraf"),
        "state dir leaked into git status: {status:?}"
    );
    // Decision 0062: the former name and the migration lock stay out of git
    // too (a lock left by a killed move, or a recreated `.haustoria/`).
    std::fs::create_dir_all(root.join(".haustoria")).expect("mkdir");
    std::fs::write(root.join(".haustoria/ledger.db"), "data").expect("write");
    std::fs::write(root.join(".kgraf.migrating"), "").expect("write");
    let status = git_out(&root, &["status", "--porcelain", "--untracked-files=all"]);
    assert!(
        !status.contains(".haustoria") && !status.contains(".kgraf.migrating"),
        "{status:?}"
    );
}

/// A root outside any git repository is left alone: `state_dir` still creates
/// the directory, and does not fail for lack of a repository to ignore it in.
#[test]
fn a_non_git_root_is_not_an_error() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("scratch");
    let state = state_dir(&root).expect("state dir without a repository");
    assert!(state.exists());
}

// ---- Linux ancestor reads: a refused cargo file is named. ----

#[cfg(target_os = "linux")]
fn other_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o646)).expect("chmod");
}

/// A checkout `Cargo.toml` anyone may write is not granted to the gate's
/// cargo, and the gate's output says so beside cargo's own "Permission
/// denied": the file and why, not only that cargo failed.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_cargo_file_is_named_in_the_gate_output() {
    let (r, _denies) = lockless_crate();
    other_writable(&r.root.join("Cargo.toml"));
    let gate = gate_runner(&r, &["cargo test --offline"], Duration::from_secs(300));
    let tip = git_out(&r.root, &["rev-parse", "kgraf/r-1"]);
    let v = gate.run(request(&gate, tip.trim())).await.expect("ran");
    assert!(!v.passed);
    let out = v.output.quote_as_data("gate_output", &[]);
    assert!(out.contains("Permission denied"), "{out}");
    assert!(
        out.contains("[sandbox]") && out.contains("is writable by others (mode 646)"),
        "{out}"
    );
}

/// The same refusal is said once when a run opens, before any command
/// fails on it.
#[cfg(target_os = "linux")]
#[test]
fn a_refused_cargo_file_is_a_notice_when_a_run_opens() {
    let (_dir, config) = project();
    let manifest = config.root.join("Cargo.toml");
    std::fs::write(&manifest, "[package]\n").expect("write");
    other_writable(&manifest);
    let host = Host::open(config).expect("open");
    let mut said = Vec::new();
    host.startup(&mut |n| said.push(n)).expect("startup");
    assert!(
        said.iter().any(|n| matches!(
            n,
            crate::Notice::AncestorReadRefused(d)
                if d.contains("Cargo.toml") && d.contains("writable by others (mode 646)")
        )),
        "{said:?}"
    );
}

/// A run the host builds tries a
/// worker's question against the run record first, in one call on the
/// planner's route through the host's ledger, attributed to
/// `orchestrator:<run>`, before any user is asked. Before the fix no host
/// wired an answerer, so every question skipped the orchestrator.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hosted_run_answers_a_workers_question_through_the_planner_first() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    #[derive(Default)]
    struct Script {
        worker_parked: AtomicBool,
        answers: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for Script {
        fn adapter(&self) -> &'static str {
            "script"
        }
        fn protocol(&self) -> kgraf_core::Protocol {
            kgraf_core::Protocol::Additive
        }
        async fn complete(
            &self,
            request: &kgraf_provider::Request,
            token: &kgraf_provider::CallToken,
        ) -> Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
            let system: String = request
                .system
                .iter()
                .filter_map(kgraf_provider::Content::as_text)
                .collect();
            let text = if system.contains("Split the user's instruction") {
                r#"{"contracts":[{"id":"a","instruction":"write src/a.txt","acceptance":"src/a.txt exists","writes":[{"path":"src/a.txt"}],"tools":[]}]}"#.to_owned()
            } else if system.contains("A worker in your run asked") {
                self.answers.fetch_add(1, Ordering::SeqCst);
                r#"{"answerable":true,"answer":"src/a.txt"}"#.to_owned()
            } else if system.contains("still serves") {
                r#"{"serves":true,"reason":"on track"}"#.to_owned()
            } else {
                // A worker: parked until the run is stopped.
                self.worker_parked.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                String::new()
            };
            let usage = kgraf_core::TokenUsage {
                uncached_input: 10,
                text_output: 10,
                ..kgraf_core::TokenUsage::default()
            };
            token.settle(&usage, None)?;
            Ok(kgraf_provider::Response {
                finish_reason: None,
                model: request.model.clone(),
                content: vec![kgraf_provider::Content::text(text)],
                stop_reason: kgraf_provider::StopReason::EndTurn,
                usage,
                request_id: None,
            })
        }
    }
    let (_dir, mut config) = project();
    config.sessions = true;
    config.routes = vec!["mine:llama3".into()];
    let script = Arc::new(Script::default());
    let host = Host::open(config)
        .expect("open")
        .with_route_provider("mine", script.clone())
        .expect("a used route");
    let prepared = host
        .prepare(RunId::new("q1"), None, &mut |_| {})
        .await
        .expect("prepare");
    let handle = prepared.handle();
    let run = tokio::spawn(prepared.run(RunStart::Instruction("make a".into())));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !script.worker_parked.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker never started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let answer = handle
        .ask_question(
            kgraf_core::AgentId::new("q1-a-r0-a0"),
            "which file do I write?".into(),
        )
        .await
        .expect("answered");
    // A lead says the orchestrator answered, not the user, and grants nothing.
    let (lead, body) = answer.split_once("\n\n").expect("a lead, then the answer");
    assert_eq!(body, "src/a.txt");
    assert!(lead.contains("not by the user"), "{lead}");
    assert_eq!(script.answers.load(Ordering::SeqCst), 1, "one answer call");
    let rows: Vec<_> = host
        .ledger()
        .calls()
        .expect("calls")
        .into_iter()
        .filter(|c| c.trace.kind.as_deref() == Some("answer_question"))
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].agent_id, "orchestrator:q1");
    assert_eq!(rows[0].settlement, kgraf_provider::Settlement::Counted);
    handle.stop().await;
    let _ = run.await.expect("join");
}

/// Decision 0013: a plan approved from a genuinely empty directory
/// (no commit at all -- the greenfield case) reaches dispatch, not merely a
/// reading. Before the `ensure_integration_branch` fix, `HEAD` was unborn and
/// `build` could never make the run's integration branch, so this never got
/// past `--corpus` (evidence: "read but not dispatched").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_greenfield_plan_is_approved_and_reaches_dispatch() {
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct Parked(AtomicBool);

    #[async_trait::async_trait]
    impl Provider for Parked {
        fn adapter(&self) -> &'static str {
            "script"
        }
        fn protocol(&self) -> kgraf_core::Protocol {
            kgraf_core::Protocol::Additive
        }
        async fn complete(
            &self,
            request: &kgraf_provider::Request,
            token: &kgraf_provider::CallToken,
        ) -> Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
            let system: String = request
                .system
                .iter()
                .filter_map(kgraf_provider::Content::as_text)
                .collect();
            // A plan-started run has no decomposition call (`seeded_record`
            // builds contracts straight from the approved items); the only
            // calls are the readiness check and the worker itself.
            let text = if system.contains("still serves") {
                r#"{"serves":true,"reason":"on track"}"#.to_owned()
            } else {
                self.0.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                String::new()
            };
            let usage = kgraf_core::TokenUsage {
                uncached_input: 10,
                text_output: 10,
                ..kgraf_core::TokenUsage::default()
            };
            token.settle(&usage, None)?;
            Ok(kgraf_provider::Response {
                finish_reason: None,
                model: request.model.clone(),
                content: vec![kgraf_provider::Content::text(text)],
                stop_reason: kgraf_provider::StopReason::EndTurn,
                usage,
                request_id: None,
            })
        }
    }

    struct SeedAsker;
    #[async_trait::async_trait]
    impl kgraf_interpret::Asker for SeedAsker {
        fn identity(&self) -> String {
            "mine:llama3".into()
        }
        async fn ask(
            &self,
            _system: &str,
            _user: String,
        ) -> std::result::Result<kgraf_interpret::AskAnswer, kgraf_interpret::AskFailure> {
            Ok(kgraf_interpret::AskAnswer {
                text: serde_json::json!({
                    "reading": "a tiny greenfield project",
                    "work_items": [{
                        "id": "a",
                        "title": "a",
                        "instruction": "write src/a.txt",
                        "writes": ["src/a.txt"],
                        "acceptance": [{
                            "text": "src/a.txt exists",
                            "check": {"kind": "path", "path": "src/a.txt"},
                        }],
                    }],
                })
                .to_string(),
                call: None,
            })
        }
    }

    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).expect("mkdir");
    host_git(&root, &["init", "-q", "-b", "main"]);
    host_git(&root, &["config", "user.name", "Test Owner"]);
    host_git(&root, &["config", "user.email", "owner@example.invalid"]);
    assert_eq!(
        git_out(&root, &["rev-list", "--count", "--all"]),
        "0",
        "the fixture starts with no commit: the greenfield case"
    );

    // Write the reading directly, exactly as `kgraf-cli`'s corpus test does:
    // this test's subject is dispatch from an approved plan, not the model
    // call that produces one. The store is closed before `Host::open` reopens
    // the same file, as that test's does too.
    let state = root.join(".kgraf");
    std::fs::create_dir_all(&state).expect("state");
    let plan_id = {
        let store = GraphStore::open(&state.join("graph.db"), &root).expect("graph");
        let mut request = kgraf_interpret::InterpretRequest::new("mine:llama3");
        request.instruction = Some("write a to a file".into());
        let prepared = kgraf_interpret::prepare(&store, request).expect("prepare");
        let asked = prepared.ask(&SeedAsker).await.expect("ask");
        let interpretation = asked
            .finish(&store, &kgraf_interpret::NoTestRuns)
            .expect("finish");
        interpretation.plan_id().to_owned()
    };

    let prices = dir.path().join("prices.json");
    std::fs::write(
        &prices,
        r#"{"llama3": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1}}"#,
    )
    .expect("prices");
    let config = RunConfig::new(&root, vec!["mine:llama3".into()], prices);
    let parked = Arc::new(Parked::default());
    let host = Host::open(config)
        .expect("open")
        .with_route_provider("mine", parked.clone())
        .expect("a used route");

    let approved = host.approve_plan(&plan_id, &[]).expect("approve");
    assert_eq!(approved.items().len(), 1);

    let startup = host.startup(&mut |_| {}).expect("startup");
    let run_id = RunId::new("greenfield-1");
    let prepared = host
        .build(&startup, run_id.clone(), Some(&approved), &mut |_| {})
        .await
        .expect("build");
    assert!(
        !git_out(
            &root,
            &["rev-parse", "--verify", "--quiet", "kgraf/greenfield-1"]
        )
        .is_empty(),
        "the integration branch exists: the greenfield directory got its first commit"
    );

    let handle = prepared.handle();
    let run_task = tokio::spawn(prepared.run(RunStart::Plan(Box::new(approved))));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !parked.0.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker was never dispatched"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    handle.stop().await;
    let outcome = run_task.await.expect("join").expect("run");
    assert!(
        outcome.report.counters.dispatches >= 1,
        "{:?}",
        outcome.report.counters
    );
}

/// A worker in an orchestrated run on a repository that was never `kgraf
/// index`-ed still gets `graph_slice` and a real slice of the code already in
/// the repository: `Host::build` indexes the working tree before planning
/// (`Host::build_base`), so a run never needs a separate `kgraf index` step
/// first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_on_a_fresh_repo_gets_graph_slices() {
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct Captured {
        parked: AtomicBool,
        tool_names: Mutex<Vec<String>>,
    }

    struct Parked(Arc<Captured>);

    #[async_trait::async_trait]
    impl Provider for Parked {
        fn adapter(&self) -> &'static str {
            "script"
        }
        fn protocol(&self) -> kgraf_core::Protocol {
            kgraf_core::Protocol::Additive
        }
        async fn complete(
            &self,
            request: &kgraf_provider::Request,
            token: &kgraf_provider::CallToken,
        ) -> Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
            let system: String = request
                .system
                .iter()
                .filter_map(kgraf_provider::Content::as_text)
                .collect();
            let text = if system.contains("still serves") {
                r#"{"serves":true,"reason":"on track"}"#.to_owned()
            } else {
                *self.0.tool_names.lock().unwrap_or_else(|p| p.into_inner()) =
                    request.tools.iter().map(|t| t.name.clone()).collect();
                self.0.parked.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                String::new()
            };
            let usage = kgraf_core::TokenUsage {
                uncached_input: 10,
                text_output: 10,
                ..kgraf_core::TokenUsage::default()
            };
            token.settle(&usage, None)?;
            Ok(kgraf_provider::Response {
                finish_reason: None,
                model: request.model.clone(),
                content: vec![kgraf_provider::Content::text(text)],
                stop_reason: kgraf_provider::StopReason::EndTurn,
                usage,
                request_id: None,
            })
        }
    }

    struct SeedAsker;
    #[async_trait::async_trait]
    impl kgraf_interpret::Asker for SeedAsker {
        fn identity(&self) -> String {
            "mine:llama3".into()
        }
        async fn ask(
            &self,
            _system: &str,
            _user: String,
        ) -> std::result::Result<kgraf_interpret::AskAnswer, kgraf_interpret::AskFailure> {
            Ok(kgraf_interpret::AskAnswer {
                text: serde_json::json!({
                    "reading": "a tiny project with existing code",
                    "work_items": [{
                        "id": "a",
                        "title": "a",
                        "instruction": "write src/b.rs",
                        "writes": ["src/b.rs"],
                        "acceptance": [{
                            "text": "src/b.rs exists",
                            "check": {"kind": "path", "path": "src/b.rs"},
                        }],
                    }],
                })
                .to_string(),
                call: None,
            })
        }
    }

    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).expect("mkdir");
    // A real symbol, committed but never `kgraf index`-ed: the case this test
    // is for. Its slice (`indexed_fn`) is what the worker's `graph_slice` tool
    // must be able to reach once `Host::build` has indexed it.
    std::fs::create_dir_all(root.join("src")).expect("mkdir src");
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn indexed_fn() -> u32 {\n    42\n}\n",
    )
    .expect("write src/a.rs");
    host_git(&root, &["init", "-q", "-b", "main"]);
    host_git(&root, &["config", "user.name", "Test Owner"]);
    host_git(&root, &["config", "user.email", "owner@example.invalid"]);
    host_git(&root, &["add", "-A"]);
    host_git(&root, &["commit", "-q", "-m", "seed"]);

    let state = root.join(".kgraf");
    std::fs::create_dir_all(&state).expect("state");
    assert_eq!(
        GraphStore::open(&state.join("graph.db"), &root)
            .expect("graph")
            .node_count()
            .expect("node count"),
        0,
        "the fixture starts unindexed"
    );
    let plan_id = {
        let store = GraphStore::open(&state.join("graph.db"), &root).expect("graph");
        let mut request = kgraf_interpret::InterpretRequest::new("mine:llama3");
        request.instruction = Some("write a file".into());
        let prepared = kgraf_interpret::prepare(&store, request).expect("prepare");
        let asked = prepared.ask(&SeedAsker).await.expect("ask");
        let interpretation = asked
            .finish(&store, &kgraf_interpret::NoTestRuns)
            .expect("finish");
        interpretation.plan_id().to_owned()
    };

    let prices = dir.path().join("prices.json");
    std::fs::write(
        &prices,
        r#"{"llama3": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1}}"#,
    )
    .expect("prices");
    let config = RunConfig::new(&root, vec!["mine:llama3".into()], prices);
    let captured = Arc::new(Captured::default());
    let host = Host::open(config)
        .expect("open")
        .with_route_provider("mine", Arc::new(Parked(captured.clone())))
        .expect("a used route");

    let approved = host.approve_plan(&plan_id, &[]).expect("approve");
    let startup = host.startup(&mut |_| {}).expect("startup");
    let run_id = RunId::new("fresh-graph-1");
    let prepared = host
        .build(&startup, run_id.clone(), Some(&approved), &mut |_| {})
        .await
        .expect("build");

    // The build itself indexed the repository, before any worker ran.
    assert!(
        GraphStore::open(&state.join("graph.db"), &root)
            .expect("graph")
            .node_count()
            .expect("node count")
            > 0,
        "`Host::build` did not index the repository"
    );

    let handle = prepared.handle();
    let run_task = tokio::spawn(prepared.run(RunStart::Plan(Box::new(approved))));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !captured.parked.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker was never dispatched"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    handle.stop().await;
    let _ = run_task.await.expect("join");

    assert!(
        captured
            .tool_names
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .any(|n| n == "graph_slice"),
        "the worker's tools did not include graph_slice: {:?}",
        captured
            .tool_names
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    );
}

#[path = "speculation_tests.rs"]
mod speculation;

#[path = "build_allowlist_tests.rs"]
mod build_allowlist;

#[path = "shortfall_tests.rs"]
mod shortfall;

#[path = "rule_conflict_tests.rs"]
mod rule_conflict;

#[path = "rule_names_tests.rs"]
mod rule_names;

#[path = "protected_write_tests.rs"]
mod protected_write;

#[cfg(target_os = "macos")]
#[path = "dependency_links_tests.rs"]
mod dependency_links;

#[cfg(unix)]
#[path = "check_inputs_tests.rs"]
mod check_inputs;

#[cfg(unix)]
#[path = "check_network_tests.rs"]
mod check_network;

#[cfg(unix)]
#[path = "dependents_tests.rs"]
mod dependents;

/// The run-start notice: named, printed, and handed to the orchestrator for
/// the trace and the report, only where the network is open for checks.
#[test]
fn the_open_network_notice_is_announced_only_where_the_network_is_open() {
    let mut said = Vec::new();
    let sentence = crate::host::announce_open_network(
        kgraf_sandbox::CheckNetwork::open_with_notice(),
        &mut |n| said.push(n),
    );
    assert_eq!(
        sentence.as_deref(),
        Some(kgraf_sandbox::NETWORK_OPEN_NOTICE)
    );
    assert_eq!(
        said,
        vec![crate::Notice::ChecksNetworkOpen(
            kgraf_sandbox::NETWORK_OPEN_NOTICE.to_owned()
        )]
    );
    let mut said = Vec::new();
    let sentence =
        crate::host::announce_open_network(kgraf_sandbox::CheckNetwork::deny(), &mut |n| {
            said.push(n)
        });
    assert_eq!(sentence, None);
    assert!(said.is_empty(), "{said:?}");
}

/// The shell's run-start notice: named, printed, and returned for the trace,
/// only where the workers' shell runs with the network open.
#[test]
fn the_open_shell_network_notice_is_announced_only_where_the_network_is_open() {
    let mut said = Vec::new();
    let sentence = crate::host::announce_open_shell_network(
        kgraf_sandbox::CheckNetwork::open_with_notice(),
        &mut |n| said.push(n),
    );
    assert_eq!(
        sentence.as_deref(),
        Some(kgraf_sandbox::SHELL_NETWORK_OPEN_NOTICE)
    );
    assert_eq!(
        said,
        vec![crate::Notice::ShellNetworkOpen(
            kgraf_sandbox::SHELL_NETWORK_OPEN_NOTICE.to_owned()
        )]
    );
    let mut said = Vec::new();
    let sentence =
        crate::host::announce_open_shell_network(kgraf_sandbox::CheckNetwork::deny(), &mut |n| {
            said.push(n)
        });
    assert_eq!(sentence, None);
    assert!(said.is_empty(), "{said:?}");
}

/// A run's index pass does not hold the host's shared graph lock: another
/// run's loop takes that lock for its session and lease calls, and holding it
/// for a large project's whole pass stalls that loop. With the lock held
/// elsewhere the whole time, the pass still finishes, and what it indexed is
/// in the graph afterward.
#[test]
fn the_index_pass_runs_without_the_shared_graph_lock() {
    let (_dir, config) = project();
    std::fs::write(config.root.join("lib.rs"), "pub fn indexed_here() {}\n").expect("write");
    let host = Arc::new(open_offline(config));
    let startup = host.startup(&mut |_| {}).expect("startup");
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let host = host.clone();
        std::thread::spawn(move || {
            host.with_graph(|_| {
                let _ = locked_tx.send(());
                let _ = release_rx.recv();
            })
        })
    };
    locked_rx.recv().expect("the shared lock is held");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    {
        let host = host.clone();
        std::thread::spawn(move || {
            let built = host
                .build_base(&startup, RunId::new("ix"), &mut |_| {})
                .map(|_| ());
            let _ = done_tx.send(built);
        });
    }
    let done = done_rx.recv_timeout(Duration::from_secs(60));
    let _ = release_tx.send(());
    holder.join().expect("holder").expect("lock");
    done.expect("the pass finished while another holder had the shared lock")
        .expect("build base");
    let nodes = host
        .with_graph(|g| g.node_count())
        .expect("lock")
        .expect("count");
    assert!(nodes > 0, "the pass indexed the project");
}

// Decision 0088: a worker's worktree, with its build output, is deleted after
// use unless the user chose to keep worktrees. Real git in temp repositories.

/// `r` with its worktree policy set. The worktree base and merge-back are the
/// fixture's own.
fn with_retention(mut r: Repo, retention: WorktreeRetention) -> Repo {
    r.workspaces = Arc::new(
        GitWorkspaces::new(r.workspaces.manager.clone(), r.merge_back.clone())
            .with_retention(retention),
    );
    r
}

/// A repository whose `.gitignore` names `target/`, as a Rust project's does.
fn repo_ignoring_target() -> Repo {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    let base = dir.path().join("ws");
    repo_in(
        dir,
        root,
        &base,
        &[("README", "hello\n"), (".gitignore", "target/\n")],
    )
}

/// Build output where a worker leaves it: a `target/` folder in the worktree
/// and the shell tool's own build directory in the worktree's scratch
/// directory. Returns both files.
fn write_build_output(r: &Repo, root: &Path) -> (PathBuf, PathBuf) {
    let in_tree = root.join("target").join("debug").join("blob");
    std::fs::create_dir_all(in_tree.parent().expect("parent")).expect("mkdir");
    std::fs::write(&in_tree, vec![7u8; 4096]).expect("write");
    let scratch = r
        .workspaces
        .get(root)
        .expect("live")
        .scratch()
        .to_path_buf();
    let in_scratch = scratch.join("target").join("blob");
    std::fs::create_dir_all(in_scratch.parent().expect("parent")).expect("mkdir");
    std::fs::write(&in_scratch, vec![7u8; 4096]).expect("write");
    (in_tree, in_scratch)
}

#[tokio::test]
async fn a_failed_workers_worktree_and_build_are_deleted_when_deleting() {
    let r = with_retention(repo_ignoring_target(), WorktreeRetention::Delete);
    let ws = worktree(&r, "w-failed").await;
    let (in_tree, in_scratch) = write_build_output(&r, &ws);
    r.workspaces.release(&ws, false).await;
    assert!(!ws.exists(), "the worktree is gone");
    assert!(!in_tree.exists() && !in_scratch.exists(), "the builds too");
    assert!(
        !git_out(&r.root, &["worktree", "list"]).contains("w-failed"),
        "and unregistered"
    );
    assert!(r.workspaces.take_problems().is_empty());
}

#[tokio::test]
async fn a_failed_workers_worktree_and_build_stay_when_keeping() {
    let r = with_retention(repo_ignoring_target(), WorktreeRetention::Keep);
    let ws = worktree(&r, "w-failed").await;
    let (in_tree, in_scratch) = write_build_output(&r, &ws);
    r.workspaces.release(&ws, false).await;
    assert!(in_tree.is_file() && in_scratch.is_file());
    assert!(r.workspaces.take_problems().is_empty());
}

/// The acceptance command failed: the attempt is committed but never
/// registered for merge or discard, the case that used to stay on disk.
#[tokio::test]
async fn a_worktree_whose_acceptance_check_failed_follows_the_policy() {
    for (retention, stays) in [
        (WorktreeRetention::Delete, false),
        (WorktreeRetention::Keep, true),
    ] {
        let r = with_retention(repo_ignoring_target(), retention);
        let ws = worktree(&r, "w-check").await;
        std::fs::write(ws.join("a.txt"), "work\n").expect("write");
        let (in_tree, in_scratch) = write_build_output(&r, &ws);
        let verdict = verifier(&r, "false")
            .verify(&contract("a.txt"), &ws, &result())
            .await
            .expect("ran");
        assert!(matches!(verdict, Verdict::Fail { .. }), "{verdict:?}");
        r.workspaces.release(&ws, false).await;
        assert_eq!(ws.exists(), stays, "{retention:?}");
        // The check removed the ignored build output the worker left in the
        // tree (decision 0084, amended 2026-10-06); its scratch folder
        // follows the policy.
        assert!(!in_tree.exists(), "{retention:?}");
        assert_eq!(in_scratch.exists(), stays, "{retention:?}");
        assert!(r.workspaces.take_problems().is_empty());
    }
}

/// A worktree released to be continued (a stopped run, a lost lease, a
/// voided pass) is never removed, under either policy, and a resume reopens
/// it with its work.
#[tokio::test]
async fn a_worktree_kept_for_its_worker_survives_both_policies_and_reopens() {
    for retention in [WorktreeRetention::Delete, WorktreeRetention::Keep] {
        let r = with_retention(repo_ignoring_target(), retention);
        let mut c = named("a.txt", "w-resume");
        let ws = r
            .workspaces
            .prepare(&c, &"w-resume".into())
            .await
            .expect("ws");
        std::fs::write(ws.join("a.txt"), "work\n").expect("write");
        let (in_tree, in_scratch) = write_build_output(&r, &ws);
        c.worktrees.insert(
            "w-resume".into(),
            r.workspaces.handle(&ws).await.expect("handle"),
        );
        r.workspaces.release(&ws, true).await;
        assert!(in_tree.is_file() && in_scratch.is_file(), "{retention:?}");
        assert!(r.workspaces.take_problems().is_empty());
        let again = restarted(&r);
        let back = again
            .prepare(&c, &"w-resume".into())
            .await
            .expect("reopened");
        let text = std::fs::read_to_string(back.join("a.txt")).expect("read");
        assert_eq!(text, "work\n", "{retention:?}");
    }
}

/// A merged attempt's work is on the integration branch under either policy.
/// Its worktree and build output go with the merge under `Delete` and stay
/// on disk under `Keep`.
#[tokio::test]
async fn a_merged_attempts_worktree_and_build_follow_the_policy() {
    for (retention, stays) in [
        (WorktreeRetention::Delete, false),
        (WorktreeRetention::Keep, true),
    ] {
        let r = with_retention(repo_ignoring_target(), retention);
        let c = named("a.txt", "w-merged");
        let ws = r
            .workspaces
            .prepare(&c, &"w-merged".into())
            .await
            .expect("ws");
        std::fs::write(ws.join("a.txt"), "work\n").expect("write");
        let (in_tree, in_scratch) = write_build_output(&r, &ws);
        let verdict = verifier(&r, "true")
            .verify(&c, &ws, &result())
            .await
            .expect("ran");
        assert!(verdict.passed(), "{verdict:?}");
        let merged = r.workspaces.integrate(&c, &ws).await.expect("integrate");
        assert!(
            matches!(merged, Integration::Merged { .. }),
            "{retention:?}"
        );
        r.workspaces.release(&ws, true).await;
        assert_eq!(ws.exists(), stays, "{retention:?}");
        // Removed by the check (decision 0084, amended 2026-10-06).
        assert!(!in_tree.exists(), "{retention:?}");
        assert_eq!(in_scratch.exists(), stays, "{retention:?}");
        assert_eq!(
            git_out(
                &r.root,
                &[
                    "show",
                    &format!("{}:a.txt", r.merge_back.integration_branch())
                ]
            ),
            "work",
            "{retention:?}: the work is on the branch"
        );
        assert!(r.workspaces.take_problems().is_empty());
    }
}

/// A losing attempt that was committed loses its attempt refs under either
/// policy. Its worktree and build output go under `Delete` and stay under
/// `Keep`; a kept loser cannot be reopened as live work.
#[tokio::test]
async fn a_discarded_losers_worktree_and_build_follow_the_policy() {
    for (retention, stays) in [
        (WorktreeRetention::Delete, false),
        (WorktreeRetention::Keep, true),
    ] {
        let r = with_retention(repo_ignoring_target(), retention);
        let mut c = named("a.txt", "w-loser");
        let ws = r
            .workspaces
            .prepare(&c, &"w-loser".into())
            .await
            .expect("ws");
        std::fs::write(ws.join("a.txt"), "work\n").expect("write");
        let (in_tree, in_scratch) = write_build_output(&r, &ws);
        c.worktrees.insert(
            "w-loser".into(),
            r.workspaces.handle(&ws).await.expect("handle"),
        );
        let verdict = verifier(&r, "true")
            .verify(&c, &ws, &result())
            .await
            .expect("ran");
        assert!(verdict.passed(), "{verdict:?}");
        r.workspaces.release(&ws, false).await;
        assert_eq!(ws.exists(), stays, "{retention:?}");
        // Removed by the check (decision 0084, amended 2026-10-06).
        assert!(!in_tree.exists(), "{retention:?}");
        assert_eq!(in_scratch.exists(), stays, "{retention:?}");
        assert_eq!(attempt_refs(&r, "w-loser"), "", "{retention:?}");
        assert!(r.workspaces.take_problems().is_empty(), "{retention:?}");
        if stays {
            assert!(
                r.workspaces.prepare(&c, &"w-loser".into()).await.is_err(),
                "a kept loser is never reopened as live work"
            );
        }
    }
}

/// The attempt refs of the worktree `name`, one per line.
fn attempt_refs(r: &Repo, name: &str) -> String {
    let refs = git_out(
        &r.root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/kgraf/attempts/",
        ],
    );
    refs.lines()
        .filter(|l| {
            l.strip_prefix("refs/kgraf/attempts/")
                .is_some_and(|t| t == name || t.starts_with(&format!("{name}+")))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A removal that fails is reported, not swallowed: the worktree stays, and
/// the run is told which one and why.
#[cfg(unix)]
#[tokio::test]
async fn a_worktree_that_cannot_be_removed_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let r = with_retention(repo_ignoring_target(), WorktreeRetention::Delete);
    let ws = worktree(&r, "w-stuck").await;
    let stuck = ws.join("stuck");
    std::fs::create_dir_all(&stuck).expect("mkdir");
    std::fs::write(stuck.join("file"), "x").expect("write");
    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o500)).expect("chmod");
    // A user that can remove it anyway (root) cannot show the failure.
    let removable = std::fs::remove_file(stuck.join("file")).is_ok();
    if removable {
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        return;
    }
    r.workspaces.release(&ws, false).await;
    let problems = r.workspaces.take_problems();
    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("w-stuck") && problems[0].contains("could not be removed"),
        "{problems:?}"
    );
    assert!(r.workspaces.take_problems().is_empty(), "taken once");
    assert!(stuck.join("file").is_file(), "nothing was half-deleted");
}

/// Startup cleanup with the keep policy leaves a dead process's worker
/// attempt worktrees alone (a worker's, a writer subagent's), and still
/// removes every worktree kGraf made for itself: conversation, gate, probe and
/// interface-stub worktrees, each made under the name its maker gives it.
#[tokio::test]
async fn startup_cleanup_with_keep_spares_worker_attempt_worktrees_only() {
    let r = repo();
    let tip = r.merge_back.integration_tip().expect("tip");
    let make = |name: &str| {
        r.workspaces
            .manager
            .create(&kgraf_sandbox::WorkspaceSpec {
                name,
                base: &tip,
                branch: None,
            })
            .expect("create")
            .root()
            .to_path_buf()
    };
    let worker = make("r1-c1-r0-a0");
    let child = make("r1-c1-r0-a0.s1");
    let comms = r
        .workspaces
        .open_comms()
        .await
        .expect("open")
        .expect("root");
    let gate = make(&harness_worktree_name(GATE_PREFIX));
    let probe = make(&harness_worktree_name(PROBE_PREFIX));
    let stub = make("stub-api_parse-v2");
    let harness = [&comms, &gate, &probe, &stub];
    for root in [&worker, &child].into_iter().chain(harness) {
        kill_owner(&r, root);
    }
    let graph = Arc::new(Mutex::new(
        GraphStore::open(&r._dir.path().join("graph.db"), &r.root).expect("graph"),
    ));
    let store = RunStore::new(graph);
    let manager = WorkspaceManager::open(&r.root, &r._dir.path().join("ws")).expect("m");
    clean_stale_worktrees(&manager, &store, WorktreeRetention::Keep).expect("clean");
    assert!(worker.exists(), "the worker's worktree stays");
    assert!(child.exists(), "the writer subagent's worktree stays");
    for root in harness {
        assert!(!root.exists(), "{} goes", root.display());
    }
    clean_stale_worktrees(&manager, &store, WorktreeRetention::Delete).expect("clean");
    assert!(
        !worker.exists() && !child.exists(),
        "deleting removes them at the next start"
    );
}

/// Only the names kGraf gives its own worktrees read as its own: a worker's
/// id (whatever its run is called) and a writer subagent's never do.
#[test]
fn harness_worktree_names_never_match_a_workers() {
    for name in [
        harness_worktree_name(COMMS_PREFIX),
        harness_worktree_name(GATE_PREFIX),
        harness_worktree_name(PROBE_PREFIX),
        harness_worktree_name(VIEW_PREFIX),
        "stub-api_parse-v2".to_owned(),
        "stub-a.b-v10".to_owned(),
    ] {
        assert!(is_harness_worktree(&name), "{name}");
    }
    for name in [
        "r1-c1-r0-a0",
        "gate-7-c1-r0-a0",
        "view-c1-r0-a0",
        "comms-run-big.h1-r1n2-a1",
        "stub-api-v2-c1-r0-a0",
        "r1-c1-r0-a0.s3",
        "gate-",
        "stub--v1",
        "stub-api-v",
    ] {
        assert!(!is_harness_worktree(name), "{name}");
    }
}

/// A handoff child continues in its parent's worktree (decision 0067): the
/// worktree is adopted, never replaced, with the parent's uncommitted work.
/// After a failed check released it for the retry that still needs it, and
/// after a restart, it reopens with that work, and the retry verifies and
/// merges it. One that is gone is refused, never replaced by a fresh one.
#[tokio::test]
async fn an_inherited_worktree_is_adopted_with_the_parents_work_and_never_replaced() {
    for retention in [WorktreeRetention::Delete, WorktreeRetention::Keep] {
        let r = with_retention(repo_ignoring_target(), retention);
        let parent = named("a.txt", "w-parent");
        let ws = r
            .workspaces
            .prepare(&parent, &"w-parent".into())
            .await
            .expect("ws");
        std::fs::write(ws.join("a.txt"), "the parent's work\n").expect("write");
        let mut child = named("a.txt", "w-parent.h1");
        child.workspace = Some(ws.clone());
        child.worktrees.insert(
            "w-parent".into(),
            r.workspaces.handle(&ws).await.expect("handle"),
        );
        assert_eq!(
            r.workspaces.adopt(&child, &ws).await.expect("adopt"),
            ws,
            "held: the same worktree"
        );
        let failed = verifier(&r, "false")
            .verify(&child, &ws, &result())
            .await
            .expect("ran");
        assert!(matches!(failed, Verdict::Fail { .. }), "{failed:?}");
        // The orchestrator keeps it: the child's retry still names it.
        r.workspaces.release(&ws, true).await;
        let again = restarted(&r);
        assert_eq!(
            again.adopt(&child, &ws).await.expect("reopened"),
            ws,
            "{retention:?}: a later process reopens it"
        );
        drop(again);
        assert_eq!(r.workspaces.adopt(&child, &ws).await.expect("reopened"), ws);
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).expect("read"),
            "the parent's work\n"
        );
        child.verifications = 2;
        let passed = verifier(&r, "true")
            .verify(&child, &ws, &result())
            .await
            .expect("ran");
        assert!(passed.passed(), "{retention:?}: {passed:?}");
        let merged = r.workspaces.integrate(&child, &ws).await.expect("merge");
        assert!(matches!(merged, Integration::Merged { .. }), "{merged:?}");
        r.workspaces.release(&ws, true).await;
        assert!(r.workspaces.take_problems().is_empty(), "{retention:?}");
        if !retention.deletes() {
            continue;
        }
        let refused = r
            .workspaces
            .adopt(&child, &ws)
            .await
            .expect_err("gone")
            .to_string();
        assert!(refused.contains("is gone"), "{refused}");
        assert!(!ws.exists(), "no fresh worktree took its place");
    }
    let r = repo();
    let outside = r.outside.join("w-elsewhere");
    let refused = r
        .workspaces
        .adopt(&named("a.txt", "c"), &outside)
        .await
        .expect_err("outside");
    assert!(
        refused.to_string().contains("not a worktree of this run"),
        "{refused}"
    );
}

/// Under `Keep`, every attempt's worktree and build output stay, whatever
/// became of it: an empty attempt that was selected, one refused for a
/// placeholder left in place, and a conflict its resolver settled. Under
/// `Delete` each goes once nothing needs it.
#[tokio::test]
async fn every_kind_of_attempt_follows_the_policy() {
    for (retention, stays) in [
        (WorktreeRetention::Delete, false),
        (WorktreeRetention::Keep, true),
    ] {
        let r = with_retention(repo_ignoring_target(), retention);
        // Empty, selected: nothing to merge (a check-only contract: one with
        // declared writes that changed nothing is refused).
        let mut empty = named("b.txt", "w-empty");
        empty.writes.clear();
        let we = prepared(&r, &empty, "w-empty").await;
        let (tree_e, scratch_e) = write_build_output(&r, &we);
        let v = verifier(&r, "true")
            .verify(&empty, &we, &result())
            .await
            .expect("ran");
        assert!(v.passed(), "{v:?}");
        assert!(matches!(
            r.workspaces.integrate(&empty, &we).await.expect("empty"),
            Integration::Empty
        ));
        r.workspaces.release(&we, true).await;
        assert_eq!(we.exists(), stays, "{retention:?} empty");
        // The check removed the ignored build output in the tree (decision
        // 0084, amended 2026-10-06); the scratch folder follows the policy.
        assert!(!tree_e.exists());
        assert_eq!(scratch_e.exists(), stays);

        // Empty, refused: a placeholder left in a declared target.
        let dir = tempfile::tempdir().expect("dir");
        let (root, base) = (dir.path().join("repo"), dir.path().join("ws"));
        let s = with_retention(
            repo_in(
                dir,
                root,
                &base,
                &[("src/lib.rs", "pub fn parse() -> u16 {\n    todo!()\n}\n")],
            ),
            retention,
        );
        let stub = named("src/lib.rs", "w-stub");
        let ws = prepared(&s, &stub, "w-stub").await;
        let refused = verifier(&s, "true")
            .verify(&stub, &ws, &result())
            .await
            .expect("ran");
        assert!(matches!(refused, Verdict::Fail { .. }), "{refused:?}");
        assert!(ws.is_dir(), "left for the release to settle");
        s.workspaces.release(&ws, false).await;
        assert_eq!(ws.exists(), stays, "{retention:?} refused empty");
        assert!(s.workspaces.take_problems().is_empty());

        // Conflict, settled by its resolver.
        let a = named("same.txt", "w-a");
        let b = named("same.txt", "w-b");
        let wa = prepared(&r, &a, "w-a").await;
        let wb = prepared(&r, &b, "w-b").await;
        std::fs::write(wa.join("same.txt"), "from a\n").expect("write");
        std::fs::write(wb.join("same.txt"), "from b\n").expect("write");
        for (c, ws) in [(&a, &wa), (&b, &wb)] {
            let v = verifier(&r, "true").verify(c, ws, &result()).await;
            let v = v.expect("ran");
            assert!(v.passed(), "{v:?}");
        }
        // Written after the check, which removes ignored output in the tree.
        let (tree_b, _) = write_build_output(&r, &wb);
        r.workspaces.integrate(&a, &wa).await.expect("a");
        r.workspaces.release(&wa, true).await;
        assert!(matches!(
            r.workspaces.integrate(&b, &wb).await.expect("b"),
            Integration::Conflict { .. }
        ));
        r.workspaces.release(&wb, true).await;
        r.workspaces
            .discard_conflict(&named("same.txt", "w-b.resolver1"), &wb)
            .await;
        assert_eq!(wb.exists(), stays, "{retention:?} conflict");
        assert_eq!(tree_b.exists(), stays);
        assert_eq!(attempt_refs(&r, "w-b"), "", "{retention:?}");
        assert!(r.workspaces.take_problems().is_empty(), "{retention:?}");
    }
}

/// Decision 0088: the policy a host holds is the one each run it builds
/// applies. A run file's `keep` reaches the run, and a change made while the
/// host is open reaches the runs built after it, not the one already built.
#[test]
fn a_host_builds_each_run_with_the_worktree_policy_in_force() {
    let (_dir, mut config) = project();
    config.worktrees = WorktreeRetention::Keep;
    let host = open_offline(config);
    assert_eq!(host.worktree_retention(), WorktreeRetention::Keep);
    let startup = host.startup(&mut |_| {}).expect("startup");
    let first = host
        .build_base(&startup, RunId::new("k1"), &mut |_| {})
        .expect("build");
    assert_eq!(first.workspaces().retention(), WorktreeRetention::Keep);

    host.set_worktree_retention(WorktreeRetention::Delete);
    let second = host
        .build_base(&startup, RunId::new("k2"), &mut |_| {})
        .expect("build");
    assert_eq!(second.workspaces().retention(), WorktreeRetention::Delete);
    assert_eq!(
        first.workspaces().retention(),
        WorktreeRetention::Keep,
        "a run already built keeps its policy"
    );
}

/// A port on loopback that refuses a connection and that no test of this
/// binary can take, for a readiness dial that must fail.
///
/// A port the operating system hands out (bind port 0) is the wrong choice
/// here, even when a connect to it fails first: the port is only observed
/// free, and the operating system may hand that very port to another test of
/// this binary a moment later, whose listener then answers the dial. A
/// readiness test failed that way, and sampling more ports narrows the
/// window without closing it.
///
/// These ports are outside what any test of this binary can be given. The
/// operating system never assigns a port below 1024 to a bind of port 0:
/// the ephemeral range is 49152 to 65535 on macOS and starts at 32768 on
/// Linux by default. No test here binds a fixed port in the low range
/// either. So the listener that answered the dial cannot appear on one of
/// these.
///
/// Privilege is a second barrier on some systems and is not relied on: a
/// bind below 1024 is refused for this user on macOS 27, Linux lets
/// `net.ipv4.ip_unprivileged_port_start` move the boundary down, and
/// Windows has no privileged range at all. Each candidate is therefore
/// still dialed first, because a machine may run a service of its own
/// there, and the caller keeps its own check for the same reason.
///
/// `None` when every candidate answered, which fails nothing: the caller
/// skips.
fn closed_loopback_port(samples: usize) -> Option<u16> {
    // Ports IANA keeps unassigned or for protocols a developer machine does
    // not run: 1 is reserved, 4 and 6 are unassigned.
    const CANDIDATES: [u16; 4] = [1, 4, 6, 1023];
    CANDIDATES
        .iter()
        .take(samples)
        .copied()
        .find(|port| std::net::TcpStream::connect(("127.0.0.1", *port)).is_err())
}

/// A local route whose server is not running fails the build at its start,
/// naming the route and its endpoint, before a branch or a worktree exists.
#[test]
fn a_build_with_an_unreachable_local_route_fails_at_start() {
    let Some(port) = closed_loopback_port(4) else {
        eprintln!("skipped: every candidate loopback port answered");
        return;
    };
    let endpoint = format!("http://127.0.0.1:{port}");
    let (_dir, mut config) = project();
    // The generative route is in-process: whether a server listens at the
    // catalog's Ollama address on this machine must not decide the result.
    config.routes = vec!["mine:llama3".into()];
    config.model_routes = vec!["typed-decision=systemone:jev-latest".into()];
    let host = Host::open(config)
        .expect("open")
        .with_route_provider("mine", Arc::new(ShellCaller))
        .expect("route")
        // The readiness dial goes to a port no test here can bind, not to
        // the catalog's `localhost:8000`, which any program on this machine
        // may listen on. The catalog's port made this test fail where it
        // meant to skip, and a port the operating system hands out can be
        // handed to another test of this binary in the same moment.
        .with_local_endpoint("systemone", &endpoint)
        .expect("endpoint");
    let startup = host.startup(&mut |_| {}).expect("startup");
    let Err(err) = host.build_base(&startup, RunId::new("unreach"), &mut |_| {}) else {
        // Nothing listened a moment ago. Something took the port between
        // that sample and this dial, which says nothing about the check.
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            eprintln!("skipped: something began to listen on 127.0.0.1:{port}");
            return;
        }
        panic!("an unreachable local route fails the build");
    };
    let text = err.to_string();
    assert!(
        text.contains(&format!("route `systemone` is unreachable at {endpoint}")),
        "{text}"
    );
    assert!(
        !host.root().join(".git/refs/heads/kgraf/unreach").exists(),
        "no integration branch was made"
    );
}

/// A server that accepts, so the dial passes, and answers like Ollama 0.29.1
/// whose model needs 0.30.0. The build stops at the version check, before a
/// branch or a worktree exists, and the version was noted first.
#[test]
fn a_build_whose_model_needs_a_newer_ollama_fails_at_start() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/v1", listener.local_addr().expect("addr"));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            // The request is one write; read it whole before answering.
            let mut raw = Vec::new();
            while !String::from_utf8_lossy(&raw).contains("\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
            }
            let head = String::from_utf8_lossy(&raw).into_owned();
            let body = if head.contains("/api/version") {
                r#"{"version":"0.29.1"}"#
            } else {
                r#"{"requires":"0.30.0"}"#
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let (_dir, mut config) = project();
    config.routes = vec!["ollama:qwen3.6:35b".into()];
    let host = Host::open(config)
        .expect("open")
        .with_local_endpoint("ollama", &url)
        .expect("endpoint");
    let startup = host.startup(&mut |_| {}).expect("startup");
    let mut notices = Vec::new();
    let Err(err) = host.build_base(&startup, RunId::new("too-old"), &mut |n| notices.push(n))
    else {
        panic!("a server older than the model's minimum fails the build");
    };
    let text = err.to_string();
    assert!(
        text.contains("qwen3.6:35b") && text.contains("0.29.1") && text.contains("0.30.0"),
        "{text}"
    );
    assert!(text.contains("update Ollama"), "{text}");
    assert!(
        notices
            .iter()
            .any(|n| matches!(n, crate::Notice::LocalServer(s) if s.contains("Ollama 0.29.1"))),
        "{notices:?}"
    );
    assert!(
        !host.root().join(".git/refs/heads/kgraf/too-old").exists(),
        "no integration branch was made"
    );
}

/// Decision 0089: a run's non-generative routes are built with the run, their
/// tools join every environment of it, and a run that declares none adds none.
#[test]
fn a_runs_model_routes_are_built_and_their_tools_join_every_environment() {
    let (_dir, mut config) = project();
    // The generative route is in-process, so only the model route has an
    // endpoint to dial.
    config.routes = vec!["mine:llama3".into()];
    config.model_routes = vec!["typed-decision=systemone:jev-latest".into()];
    let host = Host::open(config)
        .expect("open")
        .with_route_provider("mine", Arc::new(ShellCaller))
        .expect("route");
    host.check_ready(&host.route_set())
        .expect("a local typed-decision route needs no key");
    // A build dials every local route's endpoint first (`check_reachable`),
    // so something must accept there. This listener does, on a port of this
    // test's own: the catalog's `localhost:8000` is a port any program on
    // this machine may hold, and two tests that shared it had to run one at
    // a time.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let host = host
        .with_local_endpoint("systemone", &format!("http://127.0.0.1:{port}"))
        .expect("endpoint");
    let startup = host.startup(&mut |_| {}).expect("startup");
    let base = host
        .build_base(&startup, RunId::new("mk"), &mut |_| {})
        .expect("build");
    let calls = base.model_calls().expect("model routes built");
    assert_eq!(
        calls.routes().kinds(),
        vec![kgraf_core::ModelKind::TYPED_DECISION]
    );
    assert!(base.extra_tool_names().contains(&"decide".to_owned()));
    assert!(!base.extra_tool_names().contains(&"embed".to_owned()));

    let (_dir, mut config) = project();
    config.routes = vec!["mine:llama3".into()];
    let host = Host::open(config)
        .expect("open")
        .with_route_provider("mine", Arc::new(ShellCaller))
        .expect("route");
    let startup = host.startup(&mut |_| {}).expect("startup");
    let base = host
        .build_base(&startup, RunId::new("mk2"), &mut |_| {})
        .expect("build");
    assert!(base.model_calls().is_none());
    assert!(
        !base
            .extra_tool_names()
            .iter()
            .any(|n| ["decide", "embed", "rerank"].contains(&n.as_str()))
    );
}

/// Decision 0089: a model route whose catalog route does not serve its kind
/// fails readiness, before anything is built.
#[test]
fn a_model_route_on_a_route_that_does_not_serve_its_kind_is_not_ready() {
    let (_dir, mut config) = project();
    config.model_routes = vec!["typed-decision=ollama:llama3".into()];
    let host = Host::open(config).expect("open");
    let err = host.check_ready(&host.route_set()).expect_err("not served");
    assert!(
        err.to_string().contains("does not serve typed-decision"),
        "{err}"
    );
}

/// The full index pass a run opens with owns the extractor stamps, so its
/// report of changed extractors is carried to the run: a stale stamp on an
/// indexed language shows in what `build_base` hands on.
#[test]
fn the_full_index_pass_reports_a_changed_extractor_to_the_build() {
    let (_dir, config) = project();
    std::fs::write(config.root.join("lib.rs"), "pub fn indexed_here() {}\n").expect("write");
    let host = open_offline(config);
    let startup = host.startup(&mut |_| {}).expect("startup");
    let first = host
        .build_base(&startup, RunId::new("ix"), &mut |_| {})
        .expect("first build");
    assert!(first.extractor_changed.is_empty(), "nothing changed yet");
    host.with_graph(|g| g.set_meta("extractor_stamp/rust", "an older extractor"))
        .expect("lock")
        .expect("stamp");
    let second = host
        .build_base(&startup, RunId::new("ix"), &mut |_| {})
        .expect("second build");
    assert!(
        second
            .extractor_changed
            .iter()
            .any(|c| c.language == "rust" && c.files >= 1 && c.had_stamp),
        "{:?}",
        second.extractor_changed
    );
}

// Decision 0088: every failed removal or discard is reported once, and a
// worktree an open contract still names outlives its attempt.

/// Makes a worktree impossible for this user to remove: a read-only folder
/// with a file in it, made writable again on drop. `None` when this user can
/// remove it anyway (root), where no failure can be shown.
#[cfg(unix)]
struct Stuck(PathBuf);

#[cfg(unix)]
impl Stuck {
    fn in_worktree(ws: &Path) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;
        let dir = ws.join("stuck");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("file"), "x").expect("write");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).expect("chmod");
        let stuck = Self(dir);
        if std::fs::remove_file(stuck.0.join("file")).is_ok() {
            return None;
        }
        Some(stuck)
    }
}

#[cfg(unix)]
impl Drop for Stuck {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

/// A committed attempt whose discard fails is reported once: the discard
/// already tried the removal, so the worktree it left is not removed a second
/// time with a second report. The same for a settled conflict.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_discard_is_reported_once() {
    let r = with_retention(repo_ignoring_target(), WorktreeRetention::Delete);
    let c = named("a.txt", "w-lost");
    let ws = prepared(&r, &c, "w-lost").await;
    std::fs::write(ws.join("a.txt"), "work\n").expect("write");
    let v = verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    assert!(v.passed(), "{v:?}");
    let Some(stuck) = Stuck::in_worktree(&ws) else {
        return;
    };
    r.workspaces.release(&ws, false).await;
    let problems = r.workspaces.take_problems();
    drop(stuck);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("w-lost") && problems[0].contains("could not be discarded"),
        "{problems:?}"
    );

    let a = named("same.txt", "w-a");
    let b = named("same.txt", "w-b");
    let wa = prepared(&r, &a, "w-a").await;
    let wb = prepared(&r, &b, "w-b").await;
    std::fs::write(wa.join("same.txt"), "from a\n").expect("write");
    std::fs::write(wb.join("same.txt"), "from b\n").expect("write");
    for (c, ws) in [(&a, &wa), (&b, &wb)] {
        let v = verifier(&r, "true").verify(c, ws, &result()).await;
        let v = v.expect("ran");
        assert!(v.passed(), "{v:?}");
    }
    r.workspaces.integrate(&a, &wa).await.expect("a");
    assert!(matches!(
        r.workspaces.integrate(&b, &wb).await.expect("b"),
        Integration::Conflict { .. }
    ));
    r.workspaces.release(&wb, true).await;
    let Some(stuck) = Stuck::in_worktree(&wb) else {
        return;
    };
    r.workspaces
        .discard_conflict(&named("same.txt", "w-b.resolver1"), &wb)
        .await;
    let problems = r.workspaces.take_problems();
    drop(stuck);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("w-b"), "{problems:?}");
}

/// A merge merge-back refuses (here: the integration branch is checked out)
/// neither discards the attempt on the spot nor drops a failure: the attempt
/// is handed to `release`, which settles it under the policy once the
/// orchestrator says nothing names the worktree.
#[tokio::test]
async fn a_merge_merge_back_refuses_is_settled_by_its_release() {
    for (retention, stays) in [
        (WorktreeRetention::Delete, false),
        (WorktreeRetention::Keep, true),
    ] {
        let r = with_retention(repo_ignoring_target(), retention);
        let c = named("a.txt", "w-refused");
        let ws = prepared(&r, &c, "w-refused").await;
        std::fs::write(ws.join("a.txt"), "work\n").expect("write");
        let v = verifier(&r, "true")
            .verify(&c, &ws, &result())
            .await
            .expect("ran");
        assert!(v.passed(), "{v:?}");
        host_git(&r.root, &["checkout", "-q", "kgraf/r-1"]);
        let refused = r.workspaces.integrate(&c, &ws).await;
        host_git(&r.root, &["checkout", "-q", "main"]);
        assert!(refused.is_err(), "{refused:?}");
        assert!(ws.is_dir(), "{retention:?}: not discarded on the spot");
        r.workspaces.release(&ws, false).await;
        assert_eq!(ws.exists(), stays, "{retention:?}");
        assert_eq!(attempt_refs(&r, "w-refused"), "", "{retention:?}");
        assert!(r.workspaces.take_problems().is_empty(), "{retention:?}");
    }
}

/// A conversation worktree that cannot be removed is reported once, not only
/// printed.
#[cfg(unix)]
#[tokio::test]
async fn a_conversation_worktree_that_cannot_be_removed_is_reported() {
    let r = repo();
    let root = r
        .workspaces
        .open_comms()
        .await
        .expect("open")
        .expect("root");
    let Some(stuck) = Stuck::in_worktree(&root) else {
        return;
    };
    r.workspaces.discard_comms(&root).await;
    let problems = r.workspaces.take_problems();
    drop(stuck);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("conversation worktree") && problems[0].contains("comms-"),
        "{problems:?}"
    );
}

/// `r`'s repository as a later run sees it, with `retention`: nothing held.
fn later_run(r: &Repo, retention: WorktreeRetention) -> GitWorkspaces {
    let manager =
        Arc::new(WorkspaceManager::open(&r.root, &r._dir.path().join("ws")).expect("manager"));
    let merge_back = Arc::new(
        MergeBack::open(
            manager.clone(),
            "kgraf/r-1",
            kgraf_sandbox::Identity {
                name: "Test Owner".into(),
                email: "owner@example.invalid".into(),
            },
        )
        .expect("merge-back"),
    );
    GitWorkspaces::new(manager, merge_back).with_retention(retention)
}

/// A run stopped before its resolver settled: the conflict's attempt was
/// kept by that run's provider, which is gone. The resumed run settles it
/// from what the resolver recorded: under `Delete` the worktree and its refs
/// go (in this process, which still owns its lock, and after a restart, when
/// the owner is dead); under `Keep` the worktree stays and only the refs go.
#[tokio::test]
async fn a_conflict_kept_by_a_stopped_run_is_settled_when_its_resolver_settles() {
    for (retention, stays, restart) in [
        (WorktreeRetention::Delete, false, false),
        (WorktreeRetention::Delete, false, true),
        (WorktreeRetention::Keep, true, false),
    ] {
        let r = with_retention(repo_ignoring_target(), retention);
        let a = named("same.txt", "w-a");
        let b = named("same.txt", "w-b");
        let wa = prepared(&r, &a, "w-a").await;
        let wb = prepared(&r, &b, "w-b").await;
        std::fs::write(wa.join("same.txt"), "from a\n").expect("write");
        std::fs::write(wb.join("same.txt"), "from b\n").expect("write");
        for (c, ws) in [(&a, &wa), (&b, &wb)] {
            let v = verifier(&r, "true").verify(c, ws, &result()).await;
            let v = v.expect("ran");
            assert!(v.passed(), "{v:?}");
        }
        r.workspaces.integrate(&a, &wa).await.expect("a");
        assert!(matches!(
            r.workspaces.integrate(&b, &wb).await.expect("b"),
            Integration::Conflict { .. }
        ));
        let mut resolver = named("same.txt", "w-b.resolver1");
        resolver.resolver_source = Some(wb.clone());
        resolver.worktrees.insert(
            "w-b".into(),
            r.workspaces.handle(&wb).await.expect("handle"),
        );
        r.workspaces.release(&wb, true).await;
        let (in_tree, _) = {
            let dir = wb.join("target").join("debug");
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("blob"), "build").expect("write");
            (dir.join("blob"), ())
        };
        if restart {
            kill_owner(&r, &wb);
        }
        let resumed = later_run(&r, retention);
        resumed.discard_conflict(&resolver, &wb).await;
        assert_eq!(wb.exists(), stays, "{retention:?} restart={restart}");
        assert_eq!(in_tree.exists(), stays);
        assert_eq!(attempt_refs(&r, "w-b"), "", "{retention:?}");
        assert!(
            resumed.take_problems().is_empty(),
            "{retention:?} restart={restart}"
        );
    }
}

/// At the end of a run, the attempt refs of a worktree an open contract still
/// names are kept even though no open contract lists its worker: a handoff
/// child's inherited worktree (its retry reopens it at the ref's commit) and
/// a conflicted attempt a resolver cites.
#[tokio::test]
async fn attempt_refs_of_a_worktree_an_open_contract_names_are_kept() {
    let r = repo();
    let mut c = named("a.txt", "w-kept");
    c.verifications = 1;
    let ws = prepared(&r, &c, "w-kept").await;
    std::fs::write(ws.join("a.txt"), "x\n").expect("write");
    let v = verifier(&r, "true")
        .verify(&c, &ws, &result())
        .await
        .expect("ran");
    assert!(v.passed(), "{v:?}");
    let mut child = named("a.txt", "w-kept.h1");
    child.workspace = Some(ws.clone());
    let mut resolver = named("a.txt", "w-kept.resolver1");
    resolver.resolver_source = Some(ws.clone());
    for open in [child, resolver] {
        let mut record =
            kgraf_orchestrator::RunStateRecord::new(kgraf_core::RunId::new("r1"), "x", 0);
        record.open_contracts.push(open.clone());
        r.workspaces
            .sweep_refs(Some(&record), || Ok(Default::default()))
            .expect("sweep");
        assert_ne!(attempt_refs(&r, "w-kept"), "", "{}", open.contract);
    }
    let record = kgraf_orchestrator::RunStateRecord::new(kgraf_core::RunId::new("r1"), "x", 0);
    r.workspaces
        .sweep_refs(Some(&record), || Ok(Default::default()))
        .expect("sweep");
    assert_eq!(attempt_refs(&r, "w-kept"), "", "nothing names it: swept");
}

/// Decision 0053: a skill's `allowed-tools` and `disable-model-invocation` are
/// enforced by the runtime. These run a real worker loop through the host's
/// attempt assembler, with the skills read from the project's inventory file.
mod skill_enforcement {
    use super::*;
    use kgraf_provider::{Content, Role, StopReason};

    /// One scripted round: the tool calls the fake model issues.
    type Round = Vec<(&'static str, serde_json::Value)>;

    /// A fake model that issues one scripted round of tool calls per round,
    /// then answers, and keeps every request it was sent.
    struct Scripted {
        rounds: Vec<Round>,
        requests: Mutex<Vec<kgraf_provider::Request>>,
    }

    #[async_trait::async_trait]
    impl Provider for Scripted {
        fn adapter(&self) -> &'static str {
            "fake"
        }
        fn protocol(&self) -> kgraf_core::Protocol {
            kgraf_core::Protocol::Additive
        }
        async fn complete(
            &self,
            request: &kgraf_provider::Request,
            token: &kgraf_provider::CallToken,
        ) -> std::result::Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
            let usage = kgraf_core::TokenUsage {
                uncached_input: 5,
                text_output: 1,
                ..Default::default()
            };
            token.settle(&usage, None)?;
            self.requests.lock().expect("lock").push(request.clone());
            let round = request
                .messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .count();
            let (content, stop_reason) = match self.rounds.get(round) {
                Some(calls) => (
                    calls
                        .iter()
                        .enumerate()
                        .map(|(i, (name, input))| Content::ToolUse {
                            id: format!("c{round}-{i}"),
                            name: (*name).into(),
                            input: input.clone(),
                            signature: None,
                        })
                        .collect(),
                    StopReason::ToolUse,
                ),
                None => (vec![Content::text("done")], StopReason::EndTurn),
            };
            Ok(kgraf_provider::Response {
                finish_reason: None,
                model: request.model.clone(),
                content,
                stop_reason,
                usage,
                request_id: None,
            })
        }
        fn capabilities(&self) -> kgraf_provider::capability::Capabilities<'_> {
            kgraf_provider::capability::Capabilities::none()
        }
    }

    /// One skill as the inventory file stores it.
    fn skill_json(name: &str, allowed: Option<&[&str]>, disable_model: bool) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "description": format!("{name} description"),
            "frontmatter": {},
            "body": format!("{name} instructions"),
            "files": [],
            "allowed_tools": allowed,
            "disable_model_invocation": disable_model,
            "user_invocable": true,
            "hash": format!("{name}-hash-0123456789"),
            "sources": [],
        })
    }

    /// Writes the project's inventory: `reader` restricts to `read_file`;
    /// `hidden` may not be invoked by the model.
    fn write_inventory(state: &Path) {
        let inventory = serde_json::json!({
            "layers": [],
            "skills": [
                skill_json("hidden", None, true),
                skill_json("reader", Some(&["read_file"]), false),
                skill_json("badwrite", Some(&["read_file", "write_file"]), false),
            ],
            "conflicts": [],
            "problems": [],
        });
        std::fs::write(
            state.join("inventory.json"),
            serde_json::to_string_pretty(&inventory).expect("json"),
        )
        .expect("write inventory");
    }

    /// What one run left: each finished call's (id, output, is_error), the
    /// requests the model saw, and the environment's tool names.
    struct Ran {
        finished: Vec<(String, String, bool)>,
        requests: Vec<kgraf_provider::Request>,
        tools: Vec<String>,
        /// After each finished call, what the run's skill directory said the
        /// worker's skills refuse of a `wc` shell command.
        directory_refusals: Vec<Option<String>>,
    }

    impl Ran {
        fn result(&self, id: &str) -> (&str, bool) {
            let (_, output, is_error) = self
                .finished
                .iter()
                .find(|(c, _, _)| c == id)
                .unwrap_or_else(|| panic!("call {id} finished: {:?}", self.finished));
            (output.as_str(), *is_error)
        }

        fn system_text(&self, index: usize) -> String {
            self.requests[index]
                .system
                .iter()
                .filter_map(|c| match c {
                    Content::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    /// Runs one worker with `rounds` through the attempt assembler.
    async fn run(with_inventory: bool, rounds: Vec<Round>) -> Ran {
        let r = repo();
        let state = r._dir.path().join("state");
        std::fs::create_dir_all(&state).expect("state");
        if with_inventory {
            write_inventory(&state);
        }
        let ledger = Arc::new(Ledger::open(&state.join("ledger.db")).expect("ledger"));
        let provider = Arc::new(Scripted {
            rounds,
            requests: Mutex::new(Vec::new()),
        });
        let mut routes = BTreeMap::new();
        routes.insert(
            "fake:m".to_owned(),
            RouteParts {
                provider: provider.clone(),
                vendor: "fake (fake)".into(),
                vendor_name: "fake".into(),
                model: "m".into(),
            },
        );
        let assembler = AttemptAssembler {
            root: r.root.clone(),
            state: state.clone(),
            graph_path: state.join("graph.db"),
            indexed: false,
            ledger: ledger.clone(),
            config: WorkerConfig::default(),
            max_output_tokens: 1_000,
            max_checkpoints: crate::limits::DEFAULT_MAX_CHECKPOINTS,
            reasoning: Default::default(),
            read_denies: vec![state.clone()],
            workspaces: r.workspaces.clone(),
            routes,
            extra_tools: Vec::new(),
            session_line: None,
            prefixes: Arc::new(kgraf_runtime::PrefixRegistry::new()),
            machine_gate: test_machine_gate(),
            tbench_bridge_socket: None,
            dependency_refetch: crate::deps::inert_refetch(),
            // Read from the state directory as `Host::build_base` does.
            skills: crate::worker::assemble::run_skills(&state, None).expect("inventory"),
            skill_directory: Default::default(),
            refusal_directory: Default::default(),
            permissions: Arc::new(kgraf_tools::PermissionControl::manual()),
        };
        let context = ContextConfig::for_model(200_000).expect("context");
        let ws = worktree(&r, "w-skill").await;
        let route = Route {
            name: "fake:m".into(),
            vendor: "fake".into(),
            env: Arc::new(kgraf_runtime::WorkerEnv::new(
                ledger.clone(),
                provider.clone(),
                Arc::new(kgraf_tools::ToolRegistry::new()),
                assemble_prefix(),
            )),
            context: context.clone(),
            readiness: Arc::new(kgraf_orchestrator::AlwaysReady),
            local: false,
        };
        let built = assembler
            .build(&route, &ws, &"w-skill".into(), &"r9/c-skill".into())
            .await
            .expect("built");
        let tools: Vec<String> = built.env.tools.names().map(str::to_owned).collect();
        let brief = kgraf_runtime::WorkerBrief {
            task: "r9/c-skill".into(),
            worker: "w-skill".into(),
            instruction: "read the readme".into(),
            workspace_root: ws.clone(),
            model: "m".into(),
            slices: vec![],
            skills: vec![],
            step_budget: 8,
            context,
            write_scope: None,
        };
        let mut handle = kgraf_runtime::spawn_worker(
            built.env.clone(),
            brief,
            kgraf_runtime::SpawnOptions::default(),
        )
        .await
        .expect("spawn");
        let mut finished = Vec::new();
        let shell = kgraf_tools::QualifiedName::builtin("shell").expect("name");
        let mut directory_refusals = Vec::new();
        while let Some(ev) = handle.next_event().await {
            if let kgraf_runtime::WorkerEvent::ToolCallFinished {
                call_id,
                output,
                is_error,
                ..
            } = ev
            {
                // Asked while the worker runs, as `report_shortfall` would be.
                directory_refusals.push(assembler.skill_directory.refusal_for(
                    "w-skill",
                    &shell,
                    Some("wc"),
                ));
                finished.push((call_id, output, is_error));
            }
        }
        if let Some(closer) = built.closer {
            closer.close().await;
        }
        let requests = provider.requests.lock().expect("lock").clone();
        Ran {
            finished,
            requests,
            tools,
            directory_refusals,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_active_skill_refuses_a_tool_outside_its_allowed_tools() {
        let ran = run(
            true,
            vec![
                // Activation and an out-of-set call in one round: the call
                // starts after the activation and is refused.
                vec![
                    ("skill", serde_json::json!({"name": "reader"})),
                    ("list_directory", serde_json::json!({"path": "."})),
                ],
                // A later round: still refused; the allowed tool runs.
                vec![
                    ("list_directory", serde_json::json!({"path": "."})),
                    ("read_file", serde_json::json!({"path": "README"})),
                ],
            ],
        )
        .await;
        let (activated, is_error) = ran.result("c0-0");
        assert!(!is_error, "{activated}");
        assert!(activated.contains("reader instructions"), "{activated}");
        for id in ["c0-1", "c1-0"] {
            let (output, is_error) = ran.result(id);
            assert!(is_error, "{id} must be refused: {output}");
            assert!(
                output.contains("not permitted by skill `reader`"),
                "{id}: {output}"
            );
        }
        let (read, is_error) = ran.result("c1-1");
        assert!(!is_error, "read_file is in the allowed set: {read}");
        assert!(read.contains("hello"), "{read}");
        // The catalog is in the shared prefix, the same bytes in every request.
        let first = ran.system_text(0);
        assert!(first.contains("reader: reader description"), "{first}");
        for i in 1..ran.requests.len() {
            assert_eq!(ran.system_text(i), first, "request {i}");
        }
    }

    /// A shell call both the active skill and the permission mode refuse:
    /// the skill's refusal is the one the worker reads, naming the mode's as
    /// well, and nobody is asked. The run's skill directory knows the
    /// worker's restriction, which a `report_shortfall` carries.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_call_both_a_skill_and_the_mode_refuse_names_the_skill() {
        let ran = run(
            true,
            vec![
                vec![("skill", serde_json::json!({"name": "reader"}))],
                vec![(
                    "shell",
                    serde_json::json!({"command": ["wc", "-l", "README"]}),
                )],
            ],
        )
        .await;
        let (output, is_error) = ran.result("c1-0");
        assert!(is_error, "{output}");
        assert!(
            output.contains("denied by skills:allowed-tools")
                && output.contains("not permitted by skill `reader`"),
            "the skill's refusal comes first: {output}"
        );
        assert!(
            output.contains("would also have asked") || output.contains("also refused it"),
            "the mode's verdict is named too: {output}"
        );
        assert!(!output.contains("denied by user"), "{output}");
        let after_activation = ran.directory_refusals.last().expect("calls finished");
        assert!(
            after_activation
                .as_deref()
                .is_some_and(|r| r.contains("reader")),
            "the run's skill directory holds the worker's restriction: {:?}",
            ran.directory_refusals
        );
    }

    /// Round 7: a skill listing `write_file`, which no worker has, was accepted and
    /// left the worker nothing to write with. Through the real assembler, the
    /// activation is refused, names the entry and the real tools, and restricts
    /// nothing: the next call runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_skill_listing_a_tool_the_worker_lacks_is_refused_with_the_real_tools() {
        let ran = run(
            true,
            vec![
                vec![("skill", serde_json::json!({"name": "badwrite"}))],
                vec![("list_directory", serde_json::json!({"path": "."}))],
            ],
        )
        .await;
        let (output, is_error) = ran.result("c0-0");
        assert!(is_error, "{output}");
        assert!(
            output.contains("`write_file` (use `apply_patch`)"),
            "{output}"
        );
        assert!(
            output.contains("apply_patch") && output.contains("read_file"),
            "{output}"
        );
        assert!(!output.contains("badwrite instructions"), "{output}");
        let (listed, is_error) = ran.result("c1-0");
        assert!(
            !is_error,
            "a refused activation restricts nothing: {listed}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_skill_the_model_may_not_invoke_is_not_listed_and_cannot_be_activated() {
        let ran = run(
            true,
            vec![
                vec![("skill", serde_json::json!({"name": "hidden"}))],
                vec![("list_directory", serde_json::json!({"path": "."}))],
            ],
        )
        .await;
        let (output, is_error) = ran.result("c0-0");
        assert!(is_error, "{output}");
        assert!(output.contains("hidden"), "{output}");
        assert!(!output.contains("hidden instructions"), "{output}");
        // The refused activation restricts nothing.
        let (listed, is_error) = ran.result("c1-0");
        assert!(!is_error, "{listed}");
        let first = ran.system_text(0);
        assert!(first.contains("reader: reader description"), "{first}");
        assert!(!first.contains("hidden"), "{first}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_no_inventory_there_is_no_skill_tool_and_no_catalog() {
        let ran = run(
            false,
            vec![vec![("list_directory", serde_json::json!({"path": "."}))]],
        )
        .await;
        assert!(!ran.tools.iter().any(|t| t == "skill"), "{:?}", ran.tools);
        let (listed, is_error) = ran.result("c0-0");
        assert!(!is_error, "{listed}");
        assert!(!ran.system_text(0).contains("Skills available"));
    }

    /// A Terminal-Bench bridge run reads no skills, so a benchmark's prefix and
    /// tool list do not change with an inventory in its state directory.
    #[test]
    fn a_tbench_bridge_run_reads_no_skills() {
        let dir = tempfile::tempdir().expect("dir");
        write_inventory(dir.path());
        let socket = dir.path().join("bridge.sock");
        assert!(
            crate::worker::assemble::run_skills(dir.path(), Some(&socket))
                .expect("read")
                .is_none()
        );
        let catalog = crate::worker::assemble::run_skills(dir.path(), None)
            .expect("read")
            .expect("an inventory");
        assert!(!catalog.model_catalog().is_empty());
    }
}

#[path = "bypass_git_files_tests.rs"]
mod bypass_git_files;

/// An output cap above a model's supported limit is lowered, and every run
/// says so at its start; the caps the host uses are the lowered ones.
#[test]
fn an_output_cap_above_the_models_limit_is_lowered_with_a_notice() {
    let (_dir, mut config) = project();
    config.routes = vec!["openai:gpt-4o".into()];
    config.max_tokens = 131_072;
    config.planner_max_tokens = 8_192;
    let host = Host::open(config).expect("open");
    assert_eq!(host.output_caps(), (16_384, 8_192), "gpt-4o allows 16,384");
    let mut said = Vec::new();
    host.startup(&mut |n| said.push(n)).expect("startup");
    let clamps: Vec<_> = said
        .iter()
        .filter_map(|n| match n {
            crate::Notice::OutputCapClamped { which, clamp } => Some((*which, clamp.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(clamps.len(), 1, "{said:?}");
    assert_eq!(clamps[0].0, "worker");
    assert_eq!(
        (clamps[0].1.requested, clamps[0].1.limit),
        (131_072, 16_384)
    );
    // The settings surface asks the same question before a run starts.
    let notes = host.output_clamp_notes(131_072, 131_072);
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(host.output_clamp_notes(4_096, 4_096).is_empty());
}

/// `--planner-reasoning` is the `planner` override of the run's reasoning
/// setting: it reaches every planner call kind and no other, and each route
/// and level is said at every run's start; without one, nothing is set and
/// nothing is said.
#[test]
fn a_planner_reasoning_level_reaches_the_planner_caller_and_is_said() {
    use kgraf_provider::{ReasoningEffort, ReasoningProcess};
    for level in [None, Some(ReasoningEffort::Off), Some(ReasoningEffort::Low)] {
        let (_dir, mut config) = project();
        config.routes = vec!["ollama:zz-unlisted-model".into()];
        config.planner = Some("ollama:zz-unlisted-model".into());
        config.planner_reasoning = level;
        let host = Host::open(config).expect("open");
        let caller = host.planner_caller("a", "t").expect("caller");
        assert_eq!(caller.reasoning.effort(ReasoningProcess::Decompose), level);
        assert_eq!(
            caller.reasoning.effort(ReasoningProcess::QuestionRoute),
            level
        );
        assert_eq!(caller.reasoning.effort(ReasoningProcess::WorkerRound), None);
        let mut said = Vec::new();
        host.startup(&mut |n| said.push(n)).expect("startup");
        let sentences: Vec<_> = said
            .iter()
            .filter_map(|n| match n {
                crate::Notice::Reasoning(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        match level {
            None => assert!(sentences.is_empty(), "{said:?}"),
            Some(ReasoningEffort::Off) => {
                assert_eq!(sentences.len(), 1, "{said:?}");
                assert!(
                    sentences[0].contains("`ollama:zz-unlisted-model` asks for no reasoning")
                        && sentences[0].contains("decompose")
                        && sentences[0].contains("fails its call"),
                    "{}",
                    sentences[0]
                );
            }
            Some(_) => {
                assert_eq!(sentences.len(), 1, "{said:?}");
                assert!(
                    sentences[0].contains("level `low`") && sentences[0].contains("UNVERIFIED"),
                    "{}",
                    sentences[0]
                );
            }
        }
    }
}

/// Decision 0098: every caller the host builds carries the run's setting,
/// and each call kind resolves its own level: the planner's caller, the
/// per-route contract-check callers (`handshake`, `consumer_test`) and the
/// interface reviewer. A kind's override beats its group's and the
/// universal level.
#[test]
fn every_caller_the_host_builds_carries_the_runs_reasoning_setting() {
    use kgraf_provider::{
        ReasoningEffort, ReasoningGroup, ReasoningLevel, ReasoningProcess, ReasoningScope,
    };
    let (_dir, mut config) = project();
    config.routes = vec!["ollama:zz-unlisted-model".into()];
    config.reasoning = kgraf_provider::ReasoningLevels::new()
        .with(ReasoningScope::All, ReasoningEffort::Medium.into())
        .with(
            ReasoningScope::Group(ReasoningGroup::Checks),
            ReasoningEffort::Low.into(),
        )
        .with(
            ReasoningScope::Process(ReasoningProcess::ConsumerTest),
            ReasoningLevel::Unset,
        );
    let host = Host::open(config).expect("open");
    let planner = host.planner_caller("a", "t").expect("caller");
    assert_eq!(
        planner.reasoning.effort(ReasoningProcess::Reground),
        Some(ReasoningEffort::Medium)
    );
    let provider: Arc<dyn Provider> = Arc::new(ShellCaller);
    let routes = BTreeMap::from([("r".to_owned(), (provider, "m".to_owned()))]);
    let run = RunId::new("r1");
    let routed = host.routed_callers(&run, &routes);
    let caller = routed.get("r").expect("route");
    assert_eq!(
        caller.reasoning.effort(ReasoningProcess::Handshake),
        Some(ReasoningEffort::Low)
    );
    assert_eq!(
        caller.reasoning.effort(ReasoningProcess::ConsumerTest),
        None
    );
    let reviewer = host.interface_reviewer(&run, &routes);
    assert_eq!(
        reviewer.reasoning.effort(ReasoningProcess::InterfaceReview),
        Some(ReasoningEffort::Low)
    );
}

/// Decision 0098: a graded level that a recorded measurement found a route
/// and model do not honor is refused when the host opens, before any call,
/// naming the route, the level and the call kinds. The same run with that
/// route's kinds set to `off` or `unset` opens.
#[test]
fn a_reasoning_level_measured_as_not_honored_is_refused_at_open() {
    use kgraf_provider::{ReasoningEffort, ReasoningGroup, ReasoningLevel, ReasoningScope};
    let (_dir, mut config) = project();
    config.routes = vec!["ollama:qwen3.6:35b".into()];
    config.reasoning = kgraf_provider::ReasoningLevels::new()
        .with(ReasoningScope::All, ReasoningEffort::Low.into());
    let e = match Host::open(config.clone()) {
        Ok(_) => panic!("opened"),
        Err(e) => e.to_string(),
    };
    assert!(
        e.contains("`ollama:qwen3.6:35b` was measured not to honor reasoning level `low`")
            && e.contains("worker_round")
            && e.contains("decompose"),
        "{e}"
    );
    config.reasoning = config
        .reasoning
        .with(
            ReasoningScope::Group(ReasoningGroup::Planner),
            ReasoningEffort::Off.into(),
        )
        .with(
            ReasoningScope::Group(ReasoningGroup::Worker),
            ReasoningLevel::Unset,
        );
    Host::open(config).expect("off and unset open");
}

/// A run file's `planner_reasoning` and its `reasoning.planner` together are
/// refused at load (one would be ignored); either alone loads, and the
/// file's level wins over the user's setting for that scope while a scope
/// the file leaves out takes the setting.
#[test]
fn a_run_file_reasoning_merges_over_the_user_setting_and_a_double_planner_is_refused() {
    use kgraf_provider::{ReasoningEffort, ReasoningGroup, ReasoningScope};
    let (dir, config) = project();
    let file = dir.path().join("run.json");
    let user = crate::limits::UserLimits {
        reasoning: kgraf_provider::ReasoningLevels::new()
            .with(ReasoningScope::All, ReasoningEffort::Low.into())
            .with(
                ReasoningScope::Group(ReasoningGroup::Planner),
                ReasoningEffort::High.into(),
            ),
        ..Default::default()
    };
    std::fs::write(
        &file,
        r#"{"routes": ["ollama:llama3"], "prices": "prices.json", "planner_reasoning": "off"}"#,
    )
    .expect("write");
    let loaded = RunConfig::load_with(&config.root, &file, &user).expect("load");
    let planner = ReasoningScope::Group(ReasoningGroup::Planner);
    assert_eq!(
        loaded.reasoning.get(planner),
        Some(ReasoningEffort::Off.into())
    );
    assert_eq!(
        loaded.reasoning.get(ReasoningScope::All),
        Some(ReasoningEffort::Low.into())
    );
    std::fs::write(
        &file,
        r#"{"routes": ["ollama:llama3"], "prices": "prices.json", "planner_reasoning": "off",
            "reasoning": {"planner": "low"}}"#,
    )
    .expect("write");
    let e = RunConfig::load_with(&config.root, &file, &user)
        .expect_err("both")
        .to_string();
    assert!(e.contains("set twice"), "{e}");
}

/// A run file without the setting reads as no setting, and a run without
/// the setting writes no field, so older files and runs are unchanged.
#[test]
fn a_run_file_without_planner_reasoning_is_unchanged() {
    let (_dir, config) = project();
    let text = serde_json::to_string(&config).expect("json");
    assert!(!text.contains("planner_reasoning"), "{text}");
    assert!(!text.contains("\"reasoning\""), "{text}");
    let back: crate::RunConfig = serde_json::from_str(&text).expect("read back");
    assert_eq!(back.planner_reasoning, None);
    let mut with = config.clone();
    with.planner_reasoning = Some(kgraf_provider::ReasoningEffort::Off);
    let text = serde_json::to_string(&with).expect("json");
    assert!(text.contains("\"planner_reasoning\":\"off\""), "{text}");
    let back: crate::RunConfig = serde_json::from_str(&text).expect("read back");
    assert_eq!(back.planner_reasoning, with.planner_reasoning);
}

/// A model the catalog does not list has no known limit: the cap stands and
/// nothing is said.
#[test]
fn a_model_with_no_known_limit_keeps_the_requested_cap() {
    let (_dir, mut config) = project();
    config.routes = vec!["ollama:zz-unlisted-model".into()];
    config.max_tokens = 131_072;
    config.planner_max_tokens = 131_072;
    let host = Host::open(config).expect("open");
    assert_eq!(host.output_caps(), (131_072, 131_072));
    let mut said = Vec::new();
    host.startup(&mut |n| said.push(n)).expect("startup");
    assert!(
        !said
            .iter()
            .any(|n| matches!(n, crate::Notice::OutputCapClamped { .. })),
        "{said:?}"
    );
}

/// The answer wait a run takes is the configured one: the orchestrator the
/// host builds carries `answer_wait_secs`, and without it carries none, so
/// each answer call derives its wait from the planner model's latencies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_configured_answer_wait_reaches_the_orchestrator() {
    for (n, set) in [(0, None), (1, Some(77u32))] {
        let (_dir, mut config) = project();
        config.answer_wait_secs = set;
        let host = open_offline(config);
        let run = RunId::new(format!("answer-wait-{n}"));
        let orchestrator = host
            .prepare(run.clone(), None, &mut |_| {})
            .await
            .expect("prepare")
            .into_orchestrator(&host);
        assert_eq!(
            orchestrator.answer_wait(),
            set.map(|s| std::time::Duration::from_secs(u64::from(s)))
        );
        drop(orchestrator);
        host.finish_run(&run);
    }
}

/// A host that lives for days keeps no per-run state of a run that ended: each
/// run's tables fill while it is built and empty at `finish_run`, so many runs
/// on one host leave the same counts the first one did (none).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finish_run_empties_every_per_run_table_of_the_host() {
    let (_dir, config) = project();
    let host = open_offline(config);
    assert_eq!(host.retained(), crate::Retained::default());
    for n in 0..4 {
        let run = RunId::new(format!("retained-{n}"));
        host.grant_dependency_fetch(&run);
        let prepared = host
            .prepare(run.clone(), None, &mut |_| {})
            .await
            .expect("prepare");
        let orchestrator = prepared.into_orchestrator(&host);
        let during = host.retained();
        assert_eq!(during.ended, 1, "the handed-out run waits for its finish");
        assert_eq!(during.fetch_grants, 1);
        assert!(
            during.start_consents == 1 && during.refetchers == 1,
            "the build registered the run in each table: {during:?}"
        );
        drop(orchestrator);
        assert!(host.finish_run(&run).is_some(), "run {n} was handed out");
        assert_eq!(host.retained(), crate::Retained::default(), "after run {n}");
        assert!(
            host.finish_run(&run).is_none(),
            "a second finish is a no-op"
        );
    }
}

/// The run's rate-limit waits are the limiters' growth since the reader was
/// made: a wait before it is not counted, a wait after it is, and a route
/// that never waited reads zero (the orchestrator writes no record for it).
#[tokio::test]
async fn the_rate_limit_wait_reader_counts_only_waits_after_its_baseline() {
    let paced = kgraf_provider::RouteLimiter::new(
        "paced",
        kgraf_provider::RateLimit::parse("600/1@5000").expect("limit"),
    )
    .expect("limiter");
    let idle = kgraf_provider::RouteLimiter::new(
        "idle",
        kgraf_provider::RateLimit::parse("600/1@5000").expect("limit"),
    )
    .expect("limiter");
    let mut limiters = Limiters::new();
    limiters.insert("paced".into(), paced.clone());
    limiters.insert("idle".into(), idle);
    // The first call takes the burst slot; the second waits (before the
    // baseline, so it is not the run's).
    paced.acquire("test").await.expect("first");
    paced.acquire("test").await.expect("second, waits");
    assert_eq!(paced.stats().waited, 1);
    let source = crate::host::rate_limit_waits_since_now(&limiters);
    paced.acquire("test").await.expect("third, waits");
    let read: BTreeMap<String, (u64, u64)> = source()
        .into_iter()
        .map(|w| (w.route, (w.waited_ms, w.waits)))
        .collect();
    assert_eq!(read["paced"].1, 1, "{read:?}");
    assert!(read["paced"].0 >= 50, "{read:?}");
    assert_eq!(read["idle"], (0, 0));
}
