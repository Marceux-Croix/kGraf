//! Lands over real temporary repositories: every target state, the
//! concurrent move, signing, the user's hooks and filters, local changes,
//! the journal across an interruption, and the run branch kept whole.

use super::*;
use kgraf_orchestrator::land_flow::{UntestedBinding, land};
use kgraf_orchestrator::land_record::LandGate;
use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

/// Plain git for fixtures, never redirected by a hook's environment.
fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for v in kgraf_sandbox::git::GIT_REDIRECT_ENV {
        cmd.env_remove(v);
    }
    let out = cmd.output().expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_with_index(dir: &Path, index: &Path, args: &[&str], stdin: Option<&[u8]>) -> String {
    use std::io::Write;
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for v in kgraf_sandbox::git::GIT_REDIRECT_ENV {
        cmd.env_remove(v);
    }
    cmd.env("GIT_INDEX_FILE", index);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input)
            .expect("write");
    } else {
        drop(child.stdin.take());
    }
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct Fx {
    tmp: tempfile::TempDir,
    repo: PathBuf,
    worktrees: PathBuf,
}

const RUN: &str = "kgraf/r1";

fn fixture() -> Fx {
    let tmp = tempfile::tempdir().expect("tmp");
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Test Owner"]);
    git(&repo, &["config", "user.email", "owner@example.com"]);
    git(&repo, &["config", "commit.gpgSign", "false"]);
    std::fs::write(repo.join("base.txt"), "base\n").expect("write");
    git(&repo, &["add", "base.txt"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    git(&repo, &["branch", RUN]);
    let worktrees = tmp.path().join("worktrees");
    Fx {
        tmp,
        repo,
        worktrees,
    }
}

impl Fx {
    /// A commit on `branch` writing `files`, made without a checkout.
    fn commit(&self, branch: &str, files: &[(&str, &str)], msg: &str) -> String {
        let index = self.tmp.path().join("fixture.index");
        let _ = std::fs::remove_file(&index);
        let parent = git(&self.repo, &["rev-parse", &format!("refs/heads/{branch}")]);
        git_with_index(&self.repo, &index, &["read-tree", &parent], None);
        for (path, content) in files {
            let blob = git_with_index(
                &self.repo,
                &index,
                &["hash-object", "-w", "--stdin"],
                Some(content.as_bytes()),
            );
            git_with_index(
                &self.repo,
                &index,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{blob},{path}"),
                ],
                None,
            );
        }
        let tree = git_with_index(&self.repo, &index, &["write-tree"], None);
        let commit = git(
            &self.repo,
            &["commit-tree", tree.as_str(), "-p", &parent, "-m", msg],
        );
        git(
            &self.repo,
            &[
                "update-ref",
                &format!("refs/heads/{branch}"),
                &commit,
                &parent,
            ],
        );
        commit
    }

    fn tip(&self, branch: &str) -> String {
        git(&self.repo, &["rev-parse", &format!("refs/heads/{branch}")])
    }

    fn lander(&self) -> GitLander {
        GitLander::open(&self.repo, RUN, &self.worktrees).expect("open the land")
    }

    /// The working tree's files (outside `.git`) and the index's tree.
    fn snapshot(&self, dir: &Path) -> (BTreeMap<String, Vec<u8>>, String) {
        let mut files = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).expect("read dir") {
                let e = e.expect("entry");
                let p = e.path();
                if e.file_name() == ".git" {
                    continue;
                }
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p.strip_prefix(dir).expect("rel").display().to_string();
                    files.insert(rel, std::fs::read(&p).expect("read"));
                }
            }
        }
        (files, git(dir, &["write-tree"]))
    }
}

/// An in-memory run for the land function.
#[derive(Default)]
struct MemLand {
    book: LandBook,
    contracts: Vec<String>,
    stubs: BTreeSet<String>,
    gate: Option<Result<bool, String>>,
    live: bool,
    gate_ran_on: Vec<String>,
    remediations: Vec<String>,
    saves: Vec<LandBook>,
    /// Runs while the gate runs (something the user does meanwhile).
    during_gate: Option<Box<dyn Fn() + Send>>,
}

#[async_trait::async_trait]
impl LandContext for MemLand {
    fn book(&mut self) -> &mut LandBook {
        &mut self.book
    }
    fn run_id(&self) -> String {
        "r1".into()
    }
    fn contracts(&self) -> Vec<String> {
        self.contracts.clone()
    }
    fn known_stubs(&self) -> BTreeSet<String> {
        self.stubs.clone()
    }
    fn now_ms(&self) -> i64 {
        1
    }
    fn persist(&mut self) -> Result<(), String> {
        self.saves.push(self.book.clone());
        Ok(())
    }
    fn loop_live(&self) -> bool {
        self.live
    }
    async fn run_merge_gate(&mut self, commit: &str) -> Option<Result<GateVerdict, String>> {
        self.gate_ran_on.push(commit.to_owned());
        if let Some(during) = &self.during_gate {
            during();
        }
        self.gate.clone().map(|r| {
            r.map(|passed| GateVerdict {
                passed,
                by: "make test".into(),
                output: kgraf_tools::UntrustedText::new("out"),
            })
        })
    }
    fn open_remediation(&mut self, commit: &str, _verdict: GateVerdict) -> Option<String> {
        let id = format!("fix-root-{}", self.remediations.len() + 1);
        self.remediations.push(commit.to_owned());
        Some(id)
    }
}

fn attended() -> LandRequest {
    LandRequest {
        attended: true,
        ..LandRequest::default()
    }
}

impl Fx {
    /// A run that started from `main` here, with the repository's programs
    /// as they are now.
    fn ctx(&self) -> MemLand {
        let mut ctx = MemLand {
            contracts: vec!["a".into(), "b".into()],
            ..MemLand::default()
        };
        ctx.book.start_branch = Some(kgraf_orchestrator::land_record::StartBranch {
            name: "main".into(),
            sha: self.tip("main"),
        });
        let user = UserGit::from_git(&Git::discover().expect("git"));
        let copy = self.tmp.path().join(format!(
            "land-hooks-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        ctx.book.programs =
            Some(program_snapshot(&user, &self.repo, Some(&copy)).expect("snapshot"));
        ctx
    }

    /// [`Self::ctx`] with a root-gate pass on `sha`.
    fn passed_on(&self, sha: &str) -> MemLand {
        let mut ctx = self.ctx();
        ctx.book.note_root_gate_pass(sha, 1);
        ctx
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(f)
}

fn landed(outcome: LandOutcome) -> (LandingEntry, Vec<String>) {
    match outcome {
        LandOutcome::Landed { entry, notices } => (entry, notices),
        other => panic!("not landed: {other:?}"),
    }
}

/// A target that is an ancestor of the candidate (fast-forwardable) still
/// gets exactly one merge commit, target first, with both trailers; the
/// checked-out branch moves; no `MERGE_HEAD` is ever left.
#[test]
fn a_fast_forwardable_land_is_still_one_merge_commit_with_trailers() {
    let fx = fixture();
    let main_before = fx.tip("main");
    let candidate = fx.commit(RUN, &[("src/a.rs", "fn a() {}\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let (entry, _) = landed(block_on(land(&lander, &mut ctx, &attended())).expect("land"));
    let main = fx.tip("main");
    assert_eq!(main, entry.merge_commit);
    let parents = git(&fx.repo, &["rev-list", "--parents", "-n", "1", &main]);
    assert_eq!(
        parents,
        format!("{main} {main_before} {candidate}"),
        "target first parent, candidate second"
    );
    let tree = git(&fx.repo, &["rev-parse", &format!("{candidate}^{{tree}}")]);
    let msg = git(&fx.repo, &["log", "-1", "--format=%B", &main]);
    assert!(msg.contains("kGraf-Run: r1"), "{msg}");
    assert!(
        msg.contains(&format!("kGraf-Gate: {candidate} tree {tree}")),
        "{msg}"
    );
    assert!(
        !msg.contains(&main),
        "the message never names its own commit"
    );
    assert_eq!(
        entry.gate,
        LandGate::Passed {
            candidate: candidate.clone(),
            tree
        }
    );
    assert!(!fx.repo.join(".git/MERGE_HEAD").exists());
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("src/a.rs")).expect("checked out"),
        "fn a() {}\n"
    );
    assert_eq!(
        git(&fx.repo, &["log", "-1", "--format=%an <%ae>", &main]),
        "Test Owner <owner@example.com>"
    );
    // Journal: written before the move, closed after.
    let first = ctx.saves.first().expect("saved");
    assert!(
        first.landing.as_ref().is_some_and(|j| !j.target_moved),
        "the journal is on disk before the target moves"
    );
    assert!(ctx.book.landing.is_none());
    assert_eq!(ctx.book.landed.len(), 1);
    assert_eq!(fx.tip(RUN), main, "the run branch fast-forwarded to it");
}

/// A target no worktree has checked out moves by compare-and-swap; a
/// concurrent move of it refuses and changes nothing.
#[test]
fn a_target_not_checked_out_moves_by_compare_and_swap_and_a_concurrent_move_refuses() {
    let fx = fixture();
    git(&fx.repo, &["branch", "release"]);
    let candidate = fx.commit(RUN, &[("x.txt", "x\n")], "work");
    let lander = fx.lander();
    let target = lander.resolve_target("release").expect("target");
    assert_eq!(target.location, TargetLocation::NotCheckedOut);
    let mut ctx = fx.passed_on(&candidate);
    let req = LandRequest {
        into: Some("release".into()),
        ..attended()
    };
    let (entry, _) = landed(block_on(land(&lander, &mut ctx, &req)).expect("land"));
    assert_eq!(fx.tip("release"), entry.merge_commit);

    // A stale expected value: someone moved `release` after the merge was
    // built.
    let candidate2 = fx.commit(RUN, &[("y.txt", "y\n")], "more");
    let release = fx.tip("release");
    let built = match lander
        .build(&release, &candidate2, "m", false)
        .expect("build")
    {
        Built::Merge { commit, .. } => commit,
        Built::Conflict { .. } => panic!("conflict"),
    };
    let moved_by_someone = fx.commit("release", &[("z.txt", "z\n")], "user");
    let stale = LandingEntry {
        target_old: release,
        merge_commit: built,
        ..entry
    };
    let err = lander
        .move_target(&stale, &TargetLocation::NotCheckedOut, None)
        .expect_err("refused");
    assert!(matches!(err, LandError::TargetMoved { .. }), "{err}");
    assert_eq!(fx.tip("release"), moved_by_someone, "nothing changed");
}

/// A target checked out in a linked worktree is found there and moved there.
#[test]
fn a_target_checked_out_in_a_linked_worktree_moves_there() {
    let fx = fixture();
    let linked = fx.tmp.path().join("linked");
    git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            linked.to_str().expect("utf8"),
        ],
    );
    let candidate = fx.commit(RUN, &[("f.txt", "f\n")], "work");
    let lander = fx.lander();
    let target = lander.resolve_target("feature").expect("target");
    assert!(matches!(target.location, TargetLocation::LinkedWorktree(_)));
    let main = lander.resolve_target("main").expect("main");
    assert!(matches!(main.location, TargetLocation::MainCheckout(_)));
    let mut ctx = fx.passed_on(&candidate);
    let req = LandRequest {
        into: Some("feature".into()),
        ..attended()
    };
    let (entry, _) = landed(block_on(land(&lander, &mut ctx, &req)).expect("land"));
    assert_eq!(fx.tip("feature"), entry.merge_commit);
    assert_eq!(
        std::fs::read_to_string(linked.join("f.txt")).expect("f"),
        "f\n"
    );
}

#[test]
fn a_detached_head_and_an_unborn_target_refuse_by_name() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let head = fx.tip("main");
    git(&fx.repo, &["checkout", "-q", "--detach", &head]);
    let err = block_on(land(&lander, &mut ctx, &attended())).expect_err("detached");
    assert!(
        matches!(&err, LandError::TargetMismatch { checked_out, .. } if checked_out.contains("detached")),
        "{err}"
    );
    git(&fx.repo, &["checkout", "-q", "--orphan", "fresh"]);
    let err = lander.resolve_target("fresh").expect_err("unborn");
    assert!(
        matches!(&err, LandError::UnbornTarget { branch } if branch == "fresh"),
        "{err}"
    );
}

/// A git older than merge-back's floor (2.42) refuses before anything is
/// written.
#[test]
fn an_older_git_refuses_before_any_ref_moves() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let real = Git::discover().expect("git");
    let fake = fx.tmp.path().join("old-git");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = version ]; then echo 'git version 2.41.0'; exit 0; fi\ndone\nexec '{}' \"$@\"\n",
            real.exe().display()
        ),
    )
    .expect("write");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let candidate = fx.commit(RUN, &[("a", "a\n")], "work");
    let (main, run) = (fx.tip("main"), fx.tip(RUN));
    let err = match GitLander::open_with_git(&fx.repo, RUN, &fx.worktrees, Git::at(fake)) {
        Ok(_) => panic!("an old git was accepted"),
        Err(e) => e,
    };
    assert!(
        matches!(&err, LandError::GitTooOld(m) if m.contains("2.41")),
        "{err}"
    );
    assert_eq!(run, candidate);
    assert_eq!((fx.tip("main"), fx.tip(RUN)), (main, run));
}

fn fake_gpg(fx: &Fx, succeed: bool) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = fx
        .tmp
        .path()
        .join(if succeed { "gpg-ok" } else { "gpg-fail" });
    let body = if succeed {
        "#!/bin/sh\ncat >/dev/null\nprintf '\\n[GNUPG:] SIG_CREATED D 1 8 00 1 TESTKEY\\n' >&2\n\
         printf -- '-----BEGIN PGP SIGNATURE-----\\n\\nZmFrZQ==\\n-----END PGP SIGNATURE-----\\n'\n"
    } else {
        "#!/bin/sh\ncat >/dev/null\necho 'gpg: signing failed: No secret key' >&2\nexit 2\n"
    };
    std::fs::write(&path, body).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// `commit.gpgSign` on: the land's merge commit is signed with the
/// configured key.
#[test]
fn signing_on_signs_the_merge_commit_with_the_configured_key() {
    let fx = fixture();
    let gpg = fake_gpg(&fx, true);
    git(&fx.repo, &["config", "commit.gpgSign", "true"]);
    git(&fx.repo, &["config", "user.signingKey", "TESTKEY"]);
    git(
        &fx.repo,
        &["config", "gpg.program", gpg.to_str().expect("utf8")],
    );
    let candidate = fx.commit(RUN, &[("s.txt", "s\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let (entry, _) = landed(block_on(land(&lander, &mut ctx, &attended())).expect("land"));
    let raw = git(&fx.repo, &["cat-file", "commit", &entry.merge_commit]);
    assert!(
        raw.contains("gpgsig -----BEGIN PGP SIGNATURE-----"),
        "{raw}"
    );
    // kGraf's internal fold commits stay unsigned; this one fast-forwarded.
    assert_eq!(fx.tip(RUN), entry.merge_commit);
}

/// Signing on and the program fails: refused with git's message; neither
/// the target nor `kgraf/<run>` moves.
#[test]
fn a_failing_signer_refuses_and_moves_nothing() {
    let fx = fixture();
    let gpg = fake_gpg(&fx, false);
    git(&fx.repo, &["config", "commit.gpgSign", "true"]);
    git(
        &fx.repo,
        &["config", "gpg.program", gpg.to_str().expect("utf8")],
    );
    let candidate = fx.commit(RUN, &[("s.txt", "s\n")], "work");
    let (main, run) = (fx.tip("main"), fx.tip(RUN));
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let err = block_on(land(&lander, &mut ctx, &attended())).expect_err("refused");
    assert!(
        matches!(&err, LandError::Signing(m) if m.contains("gpg")),
        "{err}"
    );
    assert_eq!((fx.tip("main"), fx.tip(RUN)), (main, run));
    assert!(ctx.book.landing.is_none(), "no journal: nothing was built");
    assert!(ctx.saves.is_empty());
}

/// The user's `post-merge` hook runs when the checked-out target moves.
#[test]
fn the_users_post_merge_hook_runs() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let marker = fx.tmp.path().join("hook-ran");
    let hook = fx.repo.join(".git/hooks/post-merge");
    std::fs::create_dir_all(hook.parent().expect("dir")).expect("mkdir");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\necho ran >> '{}'\n", marker.display()),
    )
    .expect("write");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let candidate = fx.commit(RUN, &[("h.txt", "h\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    landed(block_on(land(&lander, &mut ctx, &attended())).expect("land"));
    assert_eq!(
        std::fs::read_to_string(&marker).expect("the hook ran"),
        "ran\n"
    );
}

/// A path with a smudge filter comes out of the land filtered, as after the
/// user's own merge.
#[test]
fn a_filtered_path_is_checked_out_smudged() {
    let fx = fixture();
    git(&fx.repo, &["config", "filter.up.smudge", "tr a-z A-Z"]);
    git(&fx.repo, &["config", "filter.up.clean", "tr A-Z a-z"]);
    std::fs::write(fx.repo.join(".gitattributes"), "*.up filter=up\n").expect("write");
    git(&fx.repo, &["add", ".gitattributes"]);
    git(&fx.repo, &["commit", "-q", "-m", "attributes"]);
    git(&fx.repo, &["branch", "-f", RUN, "main"]);
    let candidate = fx.commit(RUN, &[("note.up", "hello\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    landed(block_on(land(&lander, &mut ctx, &attended())).expect("land"));
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("note.up")).expect("note"),
        "HELLO\n"
    );
    assert_eq!(git(&fx.repo, &["status", "--porcelain"]), "", "clean");
}

/// A local edit to a file the land does not touch survives it.
#[test]
fn an_unrelated_local_edit_survives_the_land() {
    let fx = fixture();
    std::fs::write(fx.repo.join("base.txt"), "my edit\n").expect("edit");
    let candidate = fx.commit(RUN, &[("new.txt", "n\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let (entry, _) = landed(block_on(land(&lander, &mut ctx, &attended())).expect("land"));
    assert_eq!(fx.tip("main"), entry.merge_commit);
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("base.txt")).expect("base"),
        "my edit\n"
    );
}

/// Git refuses to overwrite a local change, and then nothing changes: the
/// working tree, the index and both branches are as before.
fn refused_with(prepare: impl Fn(&Fx)) {
    let fx = fixture();
    let candidate = fx.commit(
        RUN,
        &[("base.txt", "theirs\n"), ("new.txt", "new\n")],
        "work",
    );
    prepare(&fx);
    let before = fx.snapshot(&fx.repo);
    let (main, run) = (fx.tip("main"), fx.tip(RUN));
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let err = block_on(land(&lander, &mut ctx, &attended())).expect_err("refused");
    assert!(matches!(err, LandError::TargetRefused { .. }), "{err}");
    assert_eq!(fx.snapshot(&fx.repo), before, "tree and index unchanged");
    assert_eq!((fx.tip("main"), fx.tip(RUN)), (main, run));
    assert!(
        ctx.book.landing.is_none(),
        "the journal closed: nothing moved"
    );
    assert!(!fx.repo.join(".git/MERGE_HEAD").exists());
}

#[test]
fn a_modified_overlapping_file_refuses_and_changes_nothing() {
    refused_with(|fx| std::fs::write(fx.repo.join("base.txt"), "mine\n").expect("edit"));
}

#[test]
fn a_staged_overlapping_file_refuses_and_changes_nothing() {
    refused_with(|fx| {
        std::fs::write(fx.repo.join("base.txt"), "staged\n").expect("edit");
        git(&fx.repo, &["add", "base.txt"]);
    });
}

#[test]
fn an_untracked_file_in_the_way_refuses_and_changes_nothing() {
    refused_with(|fx| std::fs::write(fx.repo.join("new.txt"), "untracked\n").expect("write"));
}

/// A contract verified after the gated candidate stays on `kgraf/<run>`:
/// the merge commit is merged into it, never replacing it.
#[test]
fn work_verified_after_the_candidate_stays_on_the_run_branch() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "gated");
    let later = fx.commit(RUN, &[("b.txt", "b\n")], "verified after the gate");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let (entry, _) = landed(block_on(land(&lander, &mut ctx, &attended())).expect("land"));
    let run = fx.tip(RUN);
    assert_ne!(run, later, "folded");
    let parents = git(&fx.repo, &["rev-list", "--parents", "-n", "1", &run]);
    assert_eq!(parents, format!("{run} {later} {}", entry.merge_commit));
    assert!(lander.is_ancestor(&later, &run).expect("ancestry"));
    assert!(
        !lander
            .is_ancestor(&later, &fx.tip("main"))
            .expect("ancestry"),
        "the later work did not land"
    );
    assert_eq!(
        lander.fold(&entry.merge_commit).expect("again"),
        FoldResult::AlreadyContains,
        "folding is idempotent"
    );
}

/// The target gained a commit: the gate runs on the merge result, and its
/// tree is the one the trailer names.
#[test]
fn a_moved_target_gets_its_merge_result_gated() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    fx.commit("main", &[("m.txt", "m\n")], "user work");
    git(&fx.repo, &["reset", "-q", "--hard"]);
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    ctx.gate = Some(Ok(true));
    let (entry, _) = landed(block_on(land(&lander, &mut ctx, &attended())).expect("land"));
    assert_eq!(ctx.gate_ran_on.len(), 1);
    let merged_tree = git(
        &fx.repo,
        &["rev-parse", &format!("{}^{{tree}}", entry.merge_commit)],
    );
    assert_eq!(
        entry.gate,
        LandGate::Passed {
            candidate,
            tree: merged_tree
        }
    );
}

/// The merge result's gate fails: with no loop, the loud untested choice;
/// with a live loop, a remediation contract and nothing moves.
#[test]
fn a_failed_merge_result_gate_asks_or_remediates() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    fx.commit("main", &[("m.txt", "m\n")], "user work");
    git(&fx.repo, &["reset", "-q", "--hard"]);
    let main = fx.tip("main");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    ctx.gate = Some(Ok(false));
    match block_on(land(&lander, &mut ctx, &attended())).expect("asked") {
        LandOutcome::NeedsUntestedYes { reason, .. } => {
            assert!(reason.contains("merge result"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.tip("main"), main);
    ctx.live = true;
    match block_on(land(&lander, &mut ctx, &attended())).expect("remediating") {
        LandOutcome::Remediating { contract, .. } => assert_eq!(contract, "fix-root-1"),
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.tip("main"), main);
    // The fix starts from the run branch with the target merged in, so it
    // sees the code the gate failed on.
    let run_tip = fx.tip(RUN);
    assert!(lander.is_ancestor(&main, &run_tip).expect("ancestry"));
    assert_eq!(ctx.remediations, [run_tip]);
    ctx.live = false;
    let asked = block_on(land(&lander, &mut ctx, &attended())).expect("asked");
    let LandOutcome::NeedsUntestedYes {
        candidate,
        target,
        target_tip,
        ..
    } = asked
    else {
        panic!("{asked:?}");
    };
    let yes = LandRequest {
        candidate: Some(candidate.clone()),
        into: Some(target.clone()),
        untested_confirmed: true,
        untested_for: Some(UntestedBinding {
            candidate,
            target,
            target_tip,
        }),
        ..attended()
    };
    let (entry, _) = landed(block_on(land(&lander, &mut ctx, &yes)).expect("land"));
    let msg = git(&fx.repo, &["log", "-1", "--format=%B", &entry.merge_commit]);
    assert!(msg.starts_with("UNTESTED: "), "{msg}");
    assert!(msg.contains("kGraf: UNTESTED - root gate gate:root did not pass"));
    assert!(msg.trim_end().ends_with("kGraf-Gate: untested"), "{msg}");
}

/// No gate passed: a warning first, an untested land only on the yes, and
/// never unattended.
#[test]
fn no_passed_gate_warns_and_never_lands_unattended() {
    let fx = fixture();
    fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let main = fx.tip("main");
    let lander = fx.lander();
    let mut ctx = fx.ctx();
    assert!(matches!(
        block_on(land(&lander, &mut ctx, &attended())).expect("warned"),
        LandOutcome::NeedsUntestedYes { .. }
    ));
    let unattended = LandRequest {
        untested_confirmed: true,
        attended: false,
        ..LandRequest::default()
    };
    let err = block_on(land(&lander, &mut ctx, &unattended)).expect_err("never");
    assert!(matches!(err, LandError::UnattendedUntested { .. }), "{err}");
    assert_eq!(fx.tip("main"), main);
}

/// A commit holding an interface stub whose producer has not verified does
/// not land, even untested.
#[test]
fn a_held_stub_refuses_the_land_naming_the_interface() {
    let fx = fixture();
    let stub = fx.commit(RUN, &[("api.rs", "pub fn f() { todo!() }\n")], "stub");
    let mut ctx = fx.ctx();
    ctx.book.note_stub_landed("api", &stub, Some("producer"));
    ctx.book.note_root_gate_pass(&stub, 1);
    let lander = fx.lander();
    let yes = LandRequest {
        untested_confirmed: true,
        ..attended()
    };
    let err = block_on(land(&lander, &mut ctx, &yes)).expect_err("held");
    assert!(
        matches!(&err, LandError::StubsHeld(b) if b[0].contains("`api`")),
        "{err}"
    );
    let real = fx.commit(RUN, &[("api.rs", "pub fn f() {}\n")], "producer");
    ctx.book.note_contract_verified("producer", &real);
    ctx.book.note_root_gate_pass(&real, 2);
    landed(block_on(land(&lander, &mut ctx, &attended())).expect("land"));
}

/// Fails the fold once: an interruption after the target moved.
struct CrashAfterMove<'a> {
    inner: &'a GitLander,
    crashed: AtomicBool,
    /// Report another tree for the signed merge commit than the gated one.
    swap_signed_tree: bool,
    /// Runs right before the move (a worker writing meanwhile).
    before_move: Option<Box<dyn Fn() + Send + Sync>>,
}

impl LandGit for CrashAfterMove<'_> {
    fn run_branch(&self) -> &str {
        self.inner.run_branch()
    }
    fn run_tip(&self) -> Result<String, LandError> {
        self.inner.run_tip()
    }
    fn checked_out_branch(&self) -> Result<Option<String>, LandError> {
        self.inner.checked_out_branch()
    }
    fn resolve_target(&self, name: &str) -> Result<TargetInfo, LandError> {
        self.inner.resolve_target(name)
    }
    fn target_busy(&self, t: &str) -> Result<Option<String>, LandError> {
        self.inner.target_busy(t)
    }
    fn is_ancestor(&self, a: &str, d: &str) -> Result<bool, LandError> {
        self.inner.is_ancestor(a, d)
    }
    fn tree_of(&self, c: &str) -> Result<String, LandError> {
        self.inner.tree_of(c)
    }
    fn count_commits(&self, b: &str, t: &str) -> Result<usize, LandError> {
        self.inner.count_commits(b, t)
    }
    fn programs(&self, w: Option<&Path>) -> Result<ProgramSnapshot, LandError> {
        self.inner.programs(w)
    }
    fn post_land_config_changes(&self, t: &TargetInfo, m: &str) -> Result<Vec<String>, LandError> {
        self.inner.post_land_config_changes(t, m)
    }
    fn worktree_clean(&self, w: &Path) -> Result<bool, LandError> {
        self.inner.worktree_clean(w)
    }
    fn checkout_recovery(&self, w: &Path, e: &LandingEntry) -> String {
        self.inner.checkout_recovery(w, e)
    }
    fn signing_on(&self) -> Result<bool, LandError> {
        self.inner.signing_on()
    }
    fn build(&self, t: &str, c: &str, m: &str, s: bool) -> Result<Built, LandError> {
        match self.inner.build(t, c, m, s)? {
            Built::Merge { commit, .. } if s && self.swap_signed_tree => Ok(Built::Merge {
                commit,
                tree: "0".repeat(40),
            }),
            other => Ok(other),
        }
    }
    fn move_target(
        &self,
        e: &LandingEntry,
        l: &TargetLocation,
        p: Option<&ProgramSnapshot>,
    ) -> Result<(), LandError> {
        if let Some(before) = &self.before_move {
            before();
        }
        self.inner.move_target(e, l, p)
    }
    fn fold(&self, m: &str) -> Result<FoldResult, LandError> {
        if !self.crashed.swap(true, Ordering::SeqCst) {
            return Err(LandError::Git("injected crash".into()));
        }
        self.inner.fold(m)
    }
}

/// An interruption between moving the target and folding the run branch:
/// the journal says where the land stood, and the next land completes it.
#[test]
fn a_land_interrupted_between_its_steps_resumes_from_the_journal() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    let crash = CrashAfterMove {
        inner: &lander,
        crashed: AtomicBool::new(false),
        swap_signed_tree: false,
        before_move: None,
    };
    let mut ctx = fx.passed_on(&candidate);
    let err = block_on(land(&crash, &mut ctx, &attended())).expect_err("crashed");
    assert!(err.to_string().contains("injected"));
    let journal = ctx.book.landing.clone().expect("the journal is open");
    assert!(journal.target_moved);
    assert_eq!(fx.tip("main"), journal.merge_commit, "the target moved");
    assert_eq!(fx.tip(RUN), candidate, "the run branch did not");
    // A fresh process: the record is what is left.
    let mut resumed = MemLand {
        book: ctx.book.clone(),
        ..fx.ctx()
    };
    let (entry, notices) =
        landed(block_on(land(&lander, &mut resumed, &attended())).expect("resumed"));
    assert_eq!(entry.merge_commit, journal.merge_commit);
    assert!(notices[0].contains("interrupted"), "{notices:?}");
    assert_eq!(fx.tip(RUN), journal.merge_commit);
    assert!(resumed.book.landing.is_none());
    assert_eq!(resumed.book.landed.len(), 1);
}

/// Nothing new on the run branch: nothing to land.
#[test]
fn a_candidate_already_on_the_target_lands_nothing() {
    let fx = fixture();
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&fx.tip(RUN));
    assert!(matches!(
        block_on(land(&lander, &mut ctx, &attended())).expect("ok"),
        LandOutcome::NothingToLand { .. }
    ));
}

/// The branch name matches the one the run created.
#[test]
fn the_run_branch_name_matches_the_integration_branch() {
    let fx = fixture();
    let made = crate::workspaces::ensure_integration_branch(&fx.repo, "r 1/x").expect("branch");
    let git = Git::discover().expect("git");
    assert_eq!(run_branch_name(&git, &fx.repo, "r 1/x"), made);
}

/// Windows-relevant paths, run where they can: a long path, and a user with
/// `core.autocrlf` on. The merge commit holds the candidate's bytes; only
/// the user's own checkout converts line endings, as their git would.
#[test]
fn long_paths_and_autocrlf_land_byte_for_byte() {
    let fx = fixture();
    git(&fx.repo, &["config", "core.autocrlf", "true"]);
    let deep = format!("{}/file.txt", ["segment-of-a-long-path"; 12].join("/"));
    assert!(deep.len() > 260);
    let candidate = fx.commit(RUN, &[(deep.as_str(), "one\r\ntwo\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let (entry, _) = landed(block_on(land(&lander, &mut ctx, &attended())).expect("land"));
    let theirs = git(&fx.repo, &["rev-parse", &format!("{candidate}:{deep}")]);
    let landed_blob = git(
        &fx.repo,
        &["rev-parse", &format!("{}:{deep}", entry.merge_commit)],
    );
    assert_eq!(theirs, landed_blob);
    assert!(fx.repo.join(&deep).exists());
}

/// Two paths differing only by case refuse as a conflict (merge-back's
/// collision check), so a case-insensitive checkout never loses one.
#[test]
fn a_case_collision_is_a_conflict_and_nothing_moves() {
    let fx = fixture();
    fx.commit("main", &[("Readme.md", "a\n")], "user");
    git(&fx.repo, &["reset", "-q", "--hard"]);
    let candidate = fx.commit(RUN, &[("README.md", "b\n")], "work");
    let main = fx.tip("main");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    match block_on(land(&lander, &mut ctx, &attended())).expect("ok") {
        LandOutcome::Conflict { paths, .. } => {
            assert!(
                paths.iter().any(|p| p.eq_ignore_ascii_case("readme.md")),
                "{paths:?}"
            )
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.tip("main"), main);
}

/// The suggestion rules' facts come from the record and the repository: a
/// stopping point, the target moving, and nothing once it landed; a
/// benchmark run raises none.
#[test]
fn suggestion_facts_read_the_record_and_the_repository() {
    use kgraf_orchestrator::land_record::StartBranch;
    use kgraf_orchestrator::land_suggest::Suggester;
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    let mut record = RunStateRecord::new(RunId::new("r1"), "go", 1);
    record.land.start_branch = Some(StartBranch {
        name: "main".into(),
        sha: fx.tip("main"),
    });
    let none = BTreeSet::new();
    let f = suggest_facts(&lander, &record, &none, false, 10).expect("facts");
    assert!(!f.landable, "no gate passed yet");
    record.land.note_root_gate_pass(&candidate, 1);
    let f = suggest_facts(&lander, &record, &none, false, 10).expect("facts");
    assert!(f.landable && f.stopping_point && !f.target_moved, "{f:?}");
    let bench = suggest_facts(&lander, &record, &none, true, 10).expect("facts");
    assert_eq!(Suggester::default().evaluate(&record.land, &bench), None);
    fx.commit("main", &[("m.txt", "m\n")], "user work");
    git(&fx.repo, &["reset", "-q", "--hard"]);
    let f = suggest_facts(&lander, &record, &none, false, 10).expect("facts");
    assert!(f.target_moved, "{f:?}");
    let mut ctx = fx.passed_on(&candidate);
    ctx.gate = Some(Ok(true));
    landed(block_on(land(&lander, &mut ctx, &attended())).expect("land"));
    record.land = ctx.book.clone();
    let f = suggest_facts(&lander, &record, &none, false, 10).expect("facts");
    assert!(!f.landable, "landed: nothing left to suggest");
}

/// A branch named with other case than the checked-out one is not that
/// branch: on a file system that ignores case `refs/heads/Main` resolves to
/// the file of `main`, and moving it by `update-ref` would move the checked
/// out branch under its worktree.
#[test]
fn a_target_named_with_other_case_is_refused_and_nothing_moves() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let (main, run) = (fx.tip("main"), fx.tip(RUN));
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let req = LandRequest {
        into: Some("Main".into()),
        ..attended()
    };
    let err = block_on(land(&lander, &mut ctx, &req)).expect_err("refused");
    assert!(
        matches!(&err, LandError::NoSuchBranch(m) if m.contains("only by case")),
        "{err}"
    );
    assert_eq!((fx.tip("main"), fx.tip(RUN)), (main, run));
    assert_eq!(git(&fx.repo, &["status", "--porcelain"]), "");
}

/// The default target is the branch the run started from; another branch
/// checked out now refuses naming both, and a run with no recorded start
/// branch needs `--into`.
#[test]
fn the_default_target_is_the_start_branch() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    git(&fx.repo, &["checkout", "-q", "-b", "other"]);
    let err = block_on(land(&lander, &mut ctx, &attended())).expect_err("mismatch");
    let text = err.to_string();
    assert!(text.contains("main") && text.contains("other"), "{text}");
    ctx.book.start_branch = None;
    let err = block_on(land(&lander, &mut ctx, &attended())).expect_err("no target");
    assert!(matches!(err, LandError::NoTarget), "{err}");
    let main = fx.tip("main");
    let into_main = LandRequest {
        into: Some("main".into()),
        ..attended()
    };
    let (entry, _) = landed(block_on(land(&lander, &mut ctx, &into_main)).expect("land"));
    assert_eq!(entry.target, "main");
    assert_eq!(entry.target_old, main);
}

/// An untested yes is bound to the candidate and the target tip it was given
/// for: when the target moved, the warning comes again and nothing lands.
#[test]
fn an_untested_yes_is_bound_to_what_it_was_given_for() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.ctx();
    let binding = match block_on(land(&lander, &mut ctx, &attended())).expect("warned") {
        LandOutcome::NeedsUntestedYes {
            candidate: c,
            target,
            target_tip,
            commits,
            ..
        } => {
            assert_eq!(
                (c.as_str(), target.as_str(), commits),
                (candidate.as_str(), "main", 1)
            );
            UntestedBinding {
                candidate: c,
                target,
                target_tip,
            }
        }
        other => panic!("{other:?}"),
    };
    fx.commit("main", &[("m.txt", "m\n")], "user work");
    git(&fx.repo, &["reset", "-q", "--hard"]);
    let moved = fx.tip("main");
    let yes = LandRequest {
        candidate: Some(binding.candidate.clone()),
        into: Some(binding.target.clone()),
        untested_confirmed: true,
        untested_for: Some(binding.clone()),
        ..attended()
    };
    match block_on(land(&lander, &mut ctx, &yes)).expect("warned again") {
        LandOutcome::NeedsUntestedYes { target_tip, .. } => assert_eq!(target_tip, moved),
        other => panic!("a stale yes landed: {other:?}"),
    }
    assert_eq!(fx.tip("main"), moved, "nothing landed");
    let rebound = LandRequest {
        untested_for: Some(UntestedBinding {
            target_tip: moved,
            ..binding
        }),
        ..yes
    };
    let (entry, _) = landed(block_on(land(&lander, &mut ctx, &rebound)).expect("land"));
    assert!(matches!(entry.gate, LandGate::Untested { .. }));
}

/// git kills a hook that runs past its bound after the branch already
/// moved: the land sees the merge commit on the target and finishes, and
/// keeps the journal until it has.
#[test]
fn an_error_after_the_target_moved_still_finishes_the_land() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let hook = fx.repo.join(".git/hooks/post-merge");
    std::fs::create_dir_all(hook.parent().expect("dir")).expect("mkdir");
    std::fs::write(&hook, "#!/bin/sh\nsleep 30\n").expect("write");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let candidate = fx.commit(RUN, &[("h.txt", "h\n")], "work");
    let lander = fx
        .lander()
        .with_user_git_timeout(std::time::Duration::from_secs(2));
    let mut ctx = fx.passed_on(&candidate);
    let (entry, notices) = landed(block_on(land(&lander, &mut ctx, &attended())).expect("land"));
    assert_eq!(fx.tip("main"), entry.merge_commit);
    assert!(
        notices.iter().any(|n| n.contains("git then reported")),
        "{notices:?}"
    );
    assert_eq!(fx.tip(RUN), entry.merge_commit, "folded");
    assert!(ctx.book.landing.is_none());
}

/// The target moved and the record did not say so (interrupted before the
/// save), and the user committed on top since: resuming counts it landed.
#[test]
fn a_resume_after_the_user_committed_on_top_counts_the_land_done() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    let crash = CrashAfterMove {
        inner: &lander,
        crashed: AtomicBool::new(false),
        swap_signed_tree: false,
        before_move: None,
    };
    let mut ctx = fx.passed_on(&candidate);
    block_on(land(&crash, &mut ctx, &attended())).expect_err("crashed");
    let mut book = ctx.book.clone();
    if let Some(j) = book.landing.as_mut() {
        j.target_moved = false;
    }
    let merge = book.landing.as_ref().expect("journal").merge_commit.clone();
    let on_top = fx.commit("main", &[("mine.txt", "mine\n")], "user on top");
    git(&fx.repo, &["reset", "-q", "--hard"]);
    let mut resumed = MemLand { book, ..fx.ctx() };
    let (entry, _) = landed(block_on(land(&lander, &mut resumed, &attended())).expect("resumed"));
    assert_eq!(entry.merge_commit, merge);
    assert_eq!(fx.tip("main"), on_top, "the user's commit stays");
    assert!(lander.is_ancestor(&merge, &fx.tip(RUN)).expect("ancestry"));
}

/// The target worktree no longer has the target checked out at the move:
/// refused, and no branch moves.
#[test]
fn a_switched_checkout_at_the_move_refuses() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    let target = lander.resolve_target("main").expect("target");
    git(&fx.repo, &["checkout", "-q", "-b", "other"]);
    let main = fx.tip("main");
    let merge = match lander.build(&main, &candidate, "m", false).expect("build") {
        Built::Merge { commit, .. } => commit,
        Built::Conflict { .. } => panic!("conflict"),
    };
    let entry = LandingEntry {
        run_branch: RUN.into(),
        candidate,
        merge_commit: merge,
        target: "main".into(),
        target_old: main.clone(),
        gate: LandGate::Untested {
            gate: "g".into(),
            reason: "r".into(),
        },
        started_ms: 0,
        target_moved: false,
        checkout_interrupted: false,
        include_changes: Vec::new(),
        programs_digest: None,
        fold: None,
        finished_ms: None,
    };
    let err = lander
        .move_target(&entry, &target.location, None)
        .expect_err("refused");
    assert!(matches!(err, LandError::TargetRefused { .. }), "{err}");
    assert_eq!((fx.tip("main"), fx.tip("other")), (main.clone(), main));
}

/// A rebase or bisect in progress on the target refuses the land.
#[test]
fn a_rebase_or_bisect_in_progress_on_the_target_refuses() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    git(&fx.repo, &["checkout", "-q", "-b", "side", "main"]);
    std::fs::write(fx.repo.join("base.txt"), "side\n").expect("write");
    git(&fx.repo, &["commit", "-q", "-am", "side"]);
    git(&fx.repo, &["checkout", "-q", "main"]);
    std::fs::write(fx.repo.join("base.txt"), "main\n").expect("write");
    git(&fx.repo, &["commit", "-q", "-am", "main"]);
    let mut cmd = Command::new("git");
    cmd.current_dir(&fx.repo).args(["rebase", "side"]);
    for v in kgraf_sandbox::git::GIT_REDIRECT_ENV {
        cmd.env_remove(v);
    }
    assert!(
        !cmd.output().expect("rebase").status.success(),
        "the rebase stops"
    );
    let mut ctx = fx.passed_on(&candidate);
    let err = block_on(land(&lander, &mut ctx, &attended())).expect_err("busy");
    assert!(
        matches!(&err, LandError::TargetBusy { what, .. } if what.contains("rebase")),
        "{err}"
    );
    git(&fx.repo, &["rebase", "--abort"]);
    git(&fx.repo, &["bisect", "start"]);
    let err = block_on(land(&lander, &mut ctx, &attended())).expect_err("busy");
    assert!(
        matches!(&err, LandError::TargetBusy { what, .. } if what.contains("bisect")),
        "{err}"
    );
}

/// The gate fails on the merge result, and merging the target into the run
/// branch conflicts with work verified after the candidate: guidance, no
/// remediation that could never reproduce the failure.
#[test]
fn a_target_that_does_not_merge_into_the_run_branch_opens_no_remediation() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "gated");
    fx.commit(RUN, &[("base.txt", "run\n")], "verified after the gate");
    fx.commit("main", &[("base.txt", "user\n")], "user work");
    git(&fx.repo, &["reset", "-q", "--hard"]);
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    ctx.gate = Some(Ok(false));
    ctx.live = true;
    match block_on(land(&lander, &mut ctx, &attended())).expect("guidance") {
        LandOutcome::Conflict { paths, .. } => assert_eq!(paths, ["base.txt"]),
        other => panic!("{other:?}"),
    }
    assert!(ctx.remediations.is_empty());
}

/// A record copy whose lease lapsed and was taken is not written; renewing
/// during a long gate keeps the lease.
#[test]
fn a_land_writes_the_record_only_while_its_lease_holds() {
    use kgraf_orchestrator::land_flow::LandContext;
    let dir = tempfile::tempdir().expect("tmp");
    let root = dir.path().join("p");
    std::fs::create_dir_all(&root).expect("mkdir");
    let store = kgraf_graph::GraphStore::open(&dir.path().join("g.db"), &root).expect("graph");
    let graph: SharedGraph = Arc::new(std::sync::Mutex::new(store));
    let run = RunId::new("r1");
    let runs = RunStore::new(graph.clone());
    runs.save(&RunStateRecord::new(run.clone(), "go", 1))
        .expect("save");

    // Renewed while the gate runs past the lease's lifetime.
    struct Slow;
    #[async_trait::async_trait]
    impl GateRunner for Slow {
        fn commands(&self) -> Vec<String> {
            vec!["slow".into()]
        }
        async fn run(&self, _r: GateRequest) -> Result<GateVerdict, String> {
            tokio::time::sleep(std::time::Duration::from_millis(900)).await;
            Ok(GateVerdict {
                passed: true,
                by: "slow".into(),
                output: kgraf_tools::UntrustedText::new(""),
            })
        }
    }
    let lease = kgraf_orchestrator::take_run_lease_for_land_with_ttl(&graph, &run, now_ms(), 300)
        .expect("lease");
    let record = runs.load(&run).expect("load").expect("record");
    let mut ctx = RecordLand::new(
        RunStore::new(graph.clone()),
        record,
        Some(Arc::new(Slow)),
        BTreeSet::new(),
        Some(lease),
    )
    .renewing_every(std::time::Duration::from_millis(50));
    let verdict = block_on(async { ctx.run_merge_gate("abc").await });
    assert!(matches!(verdict, Some(Ok(_))));
    ctx.persist().expect("still held after the gate");

    // Lapsed and taken by another process: not written.
    std::thread::sleep(std::time::Duration::from_millis(400));
    let _other = kgraf_orchestrator::take_run_lease_for_land(&graph, &run, now_ms())
        .expect("the lapsed lease is free");
    ctx.book().land_decline_count = 7;
    let err = ctx.persist().expect_err("stale");
    assert!(err.contains("stale"), "{err}");
    let stored = runs.load(&run).expect("load").expect("record");
    assert_eq!(stored.land.land_decline_count, 0, "nothing written");
}

/// The greenfield first commit follows the user's signing: with
/// `commit.gpgSign` on and a signer that fails, no commit is made.
#[test]
fn the_greenfield_first_commit_respects_signing() {
    let fx = fixture();
    let gpg = fake_gpg(&fx, false);
    let root = fx.tmp.path().join("fresh");
    std::fs::create_dir(&root).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Test Owner"]);
    git(&root, &["config", "user.email", "owner@example.com"]);
    git(&root, &["config", "commit.gpgSign", "true"]);
    git(
        &root,
        &["config", "gpg.program", gpg.to_str().expect("utf8")],
    );
    let err = crate::workspaces::ensure_integration_branch(&root, "r").expect_err("refused");
    assert!(format!("{err:#}").contains("first commit"), "{err:#}");
    let mut cmd = Command::new("git");
    cmd.current_dir(&root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"]);
    for v in kgraf_sandbox::git::GIT_REDIRECT_ENV {
        cmd.env_remove(v);
    }
    assert!(
        !cmd.output().expect("git").status.success(),
        "no commit was made"
    );
}

/// A rebase started on the target while the gate ran stops the land before
/// anything is written.
#[test]
fn a_rebase_started_during_the_gate_refuses_the_land() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    git(&fx.repo, &["checkout", "-q", "-b", "side", "main"]);
    std::fs::write(fx.repo.join("base.txt"), "side\n").expect("write");
    git(&fx.repo, &["commit", "-q", "-am", "side"]);
    git(&fx.repo, &["checkout", "-q", "main"]);
    std::fs::write(fx.repo.join("base.txt"), "main\n").expect("write");
    git(&fx.repo, &["commit", "-q", "-am", "main moved"]);
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    ctx.gate = Some(Ok(true));
    let repo = fx.repo.clone();
    ctx.during_gate = Some(Box::new(move || {
        let mut cmd = Command::new("git");
        cmd.current_dir(&repo).args(["rebase", "side"]);
        for v in kgraf_sandbox::git::GIT_REDIRECT_ENV {
            cmd.env_remove(v);
        }
        let _ = cmd.output();
    }));
    let err = block_on(land(&lander, &mut ctx, &attended())).expect_err("busy");
    assert!(
        matches!(&err, LandError::TargetBusy { what, .. } if what.contains("rebase")),
        "{err}"
    );
    assert_eq!(ctx.gate_ran_on.len(), 1, "it was refused after the gate");
    assert!(
        ctx.saves.is_empty(),
        "refused before the journal was written"
    );
}

/// A branch moved because it was checked out instead of the target is put
/// back, and the message says how to put the files back too; with no
/// reflog, it says the put-back could not be done.
#[test]
fn a_wrongly_moved_branch_is_put_back_and_the_user_told_how() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    let main = fx.tip("main");
    let merge = match lander.build(&main, &candidate, "m", false).expect("build") {
        Built::Merge { commit, .. } => commit,
        Built::Conflict { .. } => panic!("conflict"),
    };
    let entry = LandingEntry {
        run_branch: RUN.into(),
        candidate,
        merge_commit: merge.clone(),
        target: "main".into(),
        target_old: main.clone(),
        gate: LandGate::Untested {
            gate: "g".into(),
            reason: "r".into(),
        },
        started_ms: 0,
        target_moved: false,
        checkout_interrupted: false,
        include_changes: Vec::new(),
        programs_digest: None,
        fold: None,
        finished_ms: None,
    };
    git(&fx.repo, &["checkout", "-q", "-b", "other"]);
    git(&fx.repo, &["merge", "-q", "--ff-only", &merge]);
    let said = lander
        .restore_wrong_branch(&fx.repo, &entry)
        .expect("noticed");
    assert!(
        said.contains("reset --keep") && said.contains(&main),
        "{said}"
    );
    assert_eq!(fx.tip("other"), main, "put back");

    git(&fx.repo, &["merge", "-q", "--ff-only", &merge]);
    git(&fx.repo, &["reflog", "expire", "--expire=now", "--all"]);
    let said = lander
        .restore_wrong_branch(&fx.repo, &entry)
        .expect("noticed");
    assert!(said.contains("could not be put back"), "{said}");
}

/// The signed commit must carry the tree that was gated and checked.
#[test]
fn a_signed_commit_with_another_tree_moves_nothing() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    let swap = CrashAfterMove {
        inner: &lander,
        crashed: AtomicBool::new(true),
        swap_signed_tree: true,
        before_move: None,
    };
    let mut ctx = fx.passed_on(&candidate);
    let main = fx.tip("main");
    let err = block_on(land(&swap, &mut ctx, &attended())).expect_err("refused");
    assert!(err.to_string().contains("not the gated tree"), "{err}");
    assert_eq!(fx.tip("main"), main);
    assert!(ctx.book.landing.is_none());
}

/// A signer waiting on a passphrase prompt is cut at the bound with the
/// reason, and no first commit is made.
#[test]
fn the_greenfield_signing_wait_is_bounded() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let gpg = fx.tmp.path().join("gpg-waits");
    std::fs::write(&gpg, "#!/bin/sh\nsleep 30\n").expect("write");
    std::fs::set_permissions(&gpg, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let root = fx.tmp.path().join("fresh-wait");
    std::fs::create_dir(&root).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Test Owner"]);
    git(&root, &["config", "user.email", "owner@example.com"]);
    git(&root, &["config", "commit.gpgSign", "true"]);
    git(
        &root,
        &["config", "gpg.program", gpg.to_str().expect("utf8")],
    );
    let started = std::time::Instant::now();
    let err = crate::workspaces::ensure_integration_branch_within(
        &root,
        "r",
        std::time::Duration::from_secs(2),
    )
    .expect_err("cut");
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    assert!(format!("{err:#}").contains("passphrase"), "{err:#}");
}

/// A yes given with no warning shown first (`--untested` up front) is no
/// yes: the warning comes, and the yes is then bound to what it names.
#[test]
fn an_untested_yes_without_the_warning_is_warned_first() {
    let fx = fixture();
    fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.ctx();
    let main = fx.tip("main");
    let up_front = LandRequest {
        untested_confirmed: true,
        ..attended()
    };
    assert!(matches!(
        block_on(land(&lander, &mut ctx, &up_front)).expect("warned"),
        LandOutcome::NeedsUntestedYes { .. }
    ));
    assert_eq!(fx.tip("main"), main);
}

impl Fx {
    /// A commit on `branch` writing `(path, content, mode)` entries.
    fn commit_entries(&self, branch: &str, entries: &[(&str, &str, &str)], msg: &str) -> String {
        let index = self.tmp.path().join("fixture.index");
        let _ = std::fs::remove_file(&index);
        let parent = git(&self.repo, &["rev-parse", &format!("refs/heads/{branch}")]);
        git_with_index(&self.repo, &index, &["read-tree", &parent], None);
        for (path, content, mode) in entries {
            let blob = git_with_index(
                &self.repo,
                &index,
                &["hash-object", "-w", "--stdin"],
                Some(content.as_bytes()),
            );
            git_with_index(
                &self.repo,
                &index,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("{mode},{blob},{path}"),
                ],
                None,
            );
        }
        let tree = git_with_index(&self.repo, &index, &["write-tree"], None);
        let commit = git(
            &self.repo,
            &["commit-tree", tree.as_str(), "-p", &parent, "-m", msg],
        );
        git(
            &self.repo,
            &[
                "update-ref",
                &format!("refs/heads/{branch}"),
                &commit,
                &parent,
            ],
        );
        commit
    }

    /// A shell script that touches `name` under the test directory; its path.
    fn toucher(&self, name: &str) -> (String, PathBuf) {
        let marker = self.tmp.path().join(name);
        (format!("#!/bin/sh\ntouch '{}'\n", marker.display()), marker)
    }

    /// The yes a land of `candidate` into `main` asks for, for the
    /// attribute file the merge changes.
    fn confirm(&self, git: &dyn LandGit, ctx: &mut MemLand, candidate: &str) -> LandRequest {
        match block_on(land(git, ctx, &attended())).expect("asked") {
            LandOutcome::NeedsProgramsYes { changes, .. } => {
                assert!(
                    changes.iter().any(|c| c.contains(".gitattributes")),
                    "{changes:?}"
                );
                LandRequest {
                    programs_confirmed: changes,
                    candidate: Some(candidate.to_owned()),
                    into: Some("main".into()),
                    ..attended()
                }
            }
            other => panic!("{other:?}"),
        }
    }

    /// Land `candidate` into the checked-out `main`, expecting it to land
    /// without a question.
    fn land_quietly(&self, lander: &GitLander, ctx: &mut MemLand) -> LandingEntry {
        landed(block_on(land(lander, ctx, &attended())).expect("land")).0
    }
}

/// A hook a worker commits under a hooks directory inside the work tree
/// never runs: the land runs with the run-start copy of the hooks.
#[test]
fn a_hook_the_merge_checks_out_never_runs() {
    let fx = fixture();
    git(&fx.repo, &["config", "core.hooksPath", ".githooks"]);
    let (script, marker) = fx.toucher("worker-hook");
    let candidate = fx.commit_entries(RUN, &[(".githooks/post-merge", &script, "100755")], "hook");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    fx.land_quietly(&lander, &mut ctx);
    assert!(fx.repo.join(".githooks/post-merge").exists(), "checked out");
    assert!(!marker.exists(), "never run");
}

/// The committed hooks directory is a link the merge retargets: the start
/// hooks (through the old link) run, the new target's never do.
#[test]
fn a_retargeted_hooks_link_never_runs_the_new_hooks() {
    let fx = fixture();
    let (good, good_marker) = fx.toucher("start-hook");
    let (evil, evil_marker) = fx.toucher("retargeted-hook");
    fx.commit_entries(
        "main",
        &[
            ("good/post-merge", &good, "100755"),
            (".githooks", "good", "120000"),
        ],
        "hooks by link",
    );
    git(&fx.repo, &["reset", "-q", "--hard"]);
    git(&fx.repo, &["branch", "-f", RUN, "main"]);
    git(&fx.repo, &["config", "core.hooksPath", ".githooks"]);
    let candidate = fx.commit_entries(
        RUN,
        &[
            ("evil/post-merge", &evil, "100755"),
            (".githooks", "evil", "120000"),
        ],
        "retarget",
    );
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    fx.land_quietly(&lander, &mut ctx);
    assert!(good_marker.exists(), "the run-start hook ran");
    assert!(!evil_marker.exists(), "the retargeted hook never ran");
}

/// Hooks outside the tree, edited after the run started (by a worker),
/// run as they were at the start.
#[test]
fn hooks_outside_the_tree_edited_after_the_start_run_as_they_were() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let dir = fx.tmp.path().join("outside-hooks");
    std::fs::create_dir(&dir).expect("mkdir");
    let (good, good_marker) = fx.toucher("start-hook");
    let (evil, evil_marker) = fx.toucher("edited-hook");
    std::fs::write(dir.join("post-merge"), good).expect("write");
    std::fs::set_permissions(
        dir.join("post-merge"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    git(
        &fx.repo,
        &["config", "core.hooksPath", dir.to_str().expect("utf8")],
    );
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    std::fs::write(dir.join("post-merge"), evil).expect("edit after start");
    fx.land_quietly(&lander, &mut ctx);
    assert!(good_marker.exists() && !evil_marker.exists());
}

/// `core.hooksPath=.GitHooks` on a case-insensitive file system, with the
/// worker's hook committed under `.githooks/`: never runs.
#[test]
fn a_hook_under_another_case_never_runs() {
    let fx = fixture();
    git(&fx.repo, &["config", "core.hooksPath", ".GitHooks"]);
    let (script, marker) = fx.toucher("case-hook");
    let candidate = fx.commit_entries(RUN, &[(".githooks/post-merge", &script, "100755")], "hook");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    fx.land_quietly(&lander, &mut ctx);
    assert!(!marker.exists());
}

/// A hook already in the tree but not executable at the start, made
/// executable by the merge (a mode-only change): never runs.
#[test]
fn a_hook_made_executable_by_the_merge_never_runs() {
    let fx = fixture();
    let (script, marker) = fx.toucher("mode-hook");
    fx.commit_entries(
        "main",
        &[(".githooks/post-merge", &script, "100644")],
        "inert hook",
    );
    git(&fx.repo, &["reset", "-q", "--hard"]);
    git(&fx.repo, &["branch", "-f", RUN, "main"]);
    git(&fx.repo, &["config", "core.hooksPath", ".githooks"]);
    let candidate = fx.commit_entries(RUN, &[(".githooks/post-merge", &script, "100755")], "mode");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    fx.land_quietly(&lander, &mut ctx);
    assert!(!marker.exists());
}

/// A hooks path spelled `./.githooks/../.githooks`: never runs the
/// worker's hook.
#[test]
fn a_dotted_hooks_path_never_runs_the_workers_hook() {
    let fx = fixture();
    git(
        &fx.repo,
        &["config", "core.hooksPath", "./.githooks/../.githooks"],
    );
    let (script, marker) = fx.toucher("dotted-hook");
    let candidate = fx.commit_entries(RUN, &[(".githooks/post-merge", &script, "100755")], "hook");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    fx.land_quietly(&lander, &mut ctx);
    assert!(!marker.exists());
}

/// A config file the user's git includes from the tree, changed by the
/// merge, acts in their git after the land: asked about, nothing moved.
#[test]
fn an_included_config_file_the_merge_changes_is_asked_about() {
    let fx = fixture();
    git(
        &fx.repo,
        &["config", "include.path", "../.shared.gitconfig"],
    );
    let candidate = fx.commit(
        RUN,
        &[(
            ".shared.gitconfig",
            "[core]\n\tfsmonitor = /tmp/not-a-monitor\n",
        )],
        "shared config",
    );
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let main = fx.tip("main");
    match block_on(land(&lander, &mut ctx, &attended())).expect("asked") {
        LandOutcome::NeedsProgramsYes { changes, .. } => assert!(
            changes.iter().any(|c| c.contains(".shared.gitconfig")),
            "{changes:?}"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.tip("main"), main);
}

/// A filter a worker configured after the start, selected by an attribute
/// file under a non-ASCII directory: disabled in the land.
#[test]
fn a_filter_added_after_the_start_never_runs_non_ascii_path() {
    let fx = fixture();
    let candidate = fx.commit(
        RUN,
        &[
            ("dé/.gitattributes", "* filter=evil\n"),
            ("dé/x.txt", "x\n"),
        ],
        "attrs",
    );
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let marker = fx.tmp.path().join("evil-filter");
    git(
        &fx.repo,
        &[
            "config",
            "filter.evil.smudge",
            &format!("touch '{}'; cat", marker.display()),
        ],
    );
    let yes = fx.confirm(&lander, &mut ctx, &candidate);
    landed(block_on(land(&lander, &mut ctx, &yes)).expect("land"));
    assert!(!marker.exists());
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("dé/x.txt")).expect("x"),
        "x\n"
    );
}

/// The same through an attribute macro.
#[test]
fn a_filter_added_after_the_start_never_runs_through_a_macro() {
    let fx = fixture();
    let candidate = fx.commit(
        RUN,
        &[
            (".gitattributes", "[attr]sneaky filter=evil\n* sneaky\n"),
            ("x.txt", "x\n"),
        ],
        "macro",
    );
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let marker = fx.tmp.path().join("macro-filter");
    git(
        &fx.repo,
        &[
            "config",
            "filter.evil.smudge",
            &format!("touch '{}'; cat", marker.display()),
        ],
    );
    let yes = fx.confirm(&lander, &mut ctx, &candidate);
    landed(block_on(land(&lander, &mut ctx, &yes)).expect("land"));
    assert!(!marker.exists());
}

fn monitor(fx: &Fx, name: &str) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let marker = fx.tmp.path().join(name);
    let script = fx.tmp.path().join(format!("{name}.sh"));
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .expect("write");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    (script, marker)
}

/// A file-system monitor set after the start (repository config, or a
/// linked worktree's own config) is disabled in the land.
#[test]
fn a_monitor_set_after_the_start_never_runs() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let (script, marker) = monitor(&fx, "monitor");
    git(
        &fx.repo,
        &["config", "core.fsmonitor", script.to_str().expect("utf8")],
    );
    fx.land_quietly(&lander, &mut ctx);
    assert!(!marker.exists());

    let linked = fx.tmp.path().join("linked");
    git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            linked.to_str().expect("utf8"),
        ],
    );
    git(&fx.repo, &["config", "--unset", "core.fsmonitor"]);
    git(&fx.repo, &["config", "extensions.worktreeConfig", "true"]);
    let candidate = fx.commit(RUN, &[("b.txt", "b\n")], "more");
    let mut ctx = fx.passed_on(&candidate);
    let (script, marker) = monitor(&fx, "worktree-monitor");
    git(
        &linked,
        &[
            "config",
            "--worktree",
            "core.fsmonitor",
            script.to_str().expect("utf8"),
        ],
    );
    let req = LandRequest {
        into: Some("feature".into()),
        ..attended()
    };
    landed(block_on(land(&lander, &mut ctx, &req)).expect("land"));
    assert!(!marker.exists());
}

/// Fails closed: when git cannot list the configuration, the land refuses.
#[test]
fn a_config_read_error_refuses_the_land() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let real = Git::discover().expect("git");
    let fake = fx.tmp.path().join("config-fails");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\ncase \"$*\" in *--show-origin*) echo 'config unreadable' >&2; exit 128;; esac\nexec '{}' \"$@\"\n",
            real.exe().display()
        ),
    )
    .expect("write");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let mut ctx = fx.passed_on(&candidate);
    let lander =
        GitLander::open_with_git(&fx.repo, RUN, &fx.worktrees, Git::at(fake)).expect("open");
    let main = fx.tip("main");
    let err = block_on(land(&lander, &mut ctx, &attended())).expect_err("refused");
    assert!(err.to_string().contains("config"), "{err}");
    assert_eq!(fx.tip("main"), main);
}

/// A worker writes a filter between the check and the move: the move runs
/// with the run-start settings, so it never runs.
#[test]
fn a_filter_written_right_before_the_move_never_runs() {
    let fx = fixture();
    let candidate = fx.commit(
        RUN,
        &[(".gitattributes", "*.txt filter=evil\n"), ("x.txt", "x\n")],
        "attrs",
    );
    let lander = fx.lander();
    let marker = fx.tmp.path().join("late-filter");
    let repo = fx.repo.clone();
    let late = marker.clone();
    let racer = CrashAfterMove {
        inner: &lander,
        crashed: AtomicBool::new(true),
        swap_signed_tree: false,
        before_move: Some(Box::new(move || {
            git(
                &repo,
                &[
                    "config",
                    "filter.evil.smudge",
                    &format!("touch '{}'; cat", late.display()),
                ],
            );
        })),
    };
    let mut ctx = fx.passed_on(&candidate);
    let yes = fx.confirm(&lander, &mut ctx, &candidate);
    landed(block_on(land(&racer, &mut ctx, &yes)).expect("land"));
    assert!(!marker.exists());
}

/// A worker adds an include right before the move, pointing at a file the
/// merge writes: the confirmed list no longer holds, and nothing moves.
#[test]
fn an_include_added_right_before_the_move_refuses_it() {
    let fx = fixture();
    git(
        &fx.repo,
        &["config", "include.path", "../.shared.gitconfig"],
    );
    let candidate = fx.commit(
        RUN,
        &[
            (".shared.gitconfig", "[x]\n\ty = 1\n"),
            (".other.gitconfig", "[x]\n\tz = 1\n"),
        ],
        "configs",
    );
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let changes = match block_on(land(&lander, &mut ctx, &attended())).expect("asked") {
        LandOutcome::NeedsProgramsYes { changes, .. } => changes,
        other => panic!("{other:?}"),
    };
    let repo = fx.repo.clone();
    let racer = CrashAfterMove {
        inner: &lander,
        crashed: AtomicBool::new(true),
        swap_signed_tree: false,
        before_move: Some(Box::new(move || {
            git(
                &repo,
                &["config", "--add", "include.path", "../.other.gitconfig"],
            );
        })),
    };
    let main = fx.tip("main");
    let yes = LandRequest {
        programs_confirmed: changes,
        candidate: Some(candidate),
        into: Some("main".into()),
        ..attended()
    };
    let err = block_on(land(&racer, &mut ctx, &yes)).expect_err("refused");
    assert!(err.to_string().contains("not the ones confirmed"), "{err}");
    assert_eq!(fx.tip("main"), main);
}

/// A land resumed from its journal goes through the same enforced move: a
/// filter a worker added meanwhile never runs.
#[test]
fn a_resumed_land_moves_with_the_same_enforcement() {
    let fx = fixture();
    let candidate = fx.commit(
        RUN,
        &[(".gitattributes", "*.txt filter=evil\n"), ("x.txt", "x\n")],
        "attrs",
    );
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let yes = fx.confirm(&lander, &mut ctx, &candidate);
    match kgraf_orchestrator::land_flow::prepare(&lander, &mut ctx, &yes).expect("prepared") {
        kgraf_orchestrator::land_flow::Step::Move { .. } => {}
        other => panic!("{other:?}"),
    }
    assert!(
        ctx.book.landing.is_some(),
        "journal written, target not moved"
    );
    let marker = fx.tmp.path().join("resume-filter");
    git(
        &fx.repo,
        &[
            "config",
            "filter.evil.smudge",
            &format!("touch '{}'; cat", marker.display()),
        ],
    );
    let mut resumed = MemLand {
        book: ctx.book.clone(),
        ..fx.ctx()
    };
    landed(block_on(land(&lander, &mut resumed, &attended())).expect("resumed"));
    assert!(!marker.exists());
}

/// `--recover`: a file still the new version is put back, a file the land
/// added is removed, a partly written file and an unrelated edit are left
/// alone, and a stale lock is removed.
#[test]
fn recovery_restores_only_what_it_can_tell() {
    let fx = fixture();
    fx.commit("main", &[("notes.txt", "notes\n")], "notes");
    git(&fx.repo, &["reset", "-q", "--hard"]);
    git(&fx.repo, &["branch", "-f", RUN, "main"]);
    let candidate = fx.commit(
        RUN,
        &[
            ("base.txt", "new base\n"),
            ("added.txt", "added\n"),
            ("part.txt", "whole\n"),
        ],
        "work",
    );
    let lander = fx.lander();
    let main = fx.tip("main");
    let merge = match lander.build(&main, &candidate, "m", false).expect("build") {
        Built::Merge { commit, .. } => commit,
        Built::Conflict { .. } => panic!("conflict"),
    };
    std::fs::write(fx.repo.join("base.txt"), "new base\n").expect("write");
    std::fs::write(fx.repo.join("added.txt"), "added\n").expect("write");
    std::fs::write(fx.repo.join("part.txt"), "wh").expect("write");
    std::fs::write(fx.repo.join("notes.txt"), "my edit\n").expect("write");
    std::fs::write(fx.repo.join(".git/index.lock"), "").expect("lock");
    let entry = LandingEntry {
        run_branch: RUN.into(),
        candidate,
        merge_commit: merge,
        target: "main".into(),
        target_old: main,
        gate: LandGate::Untested {
            gate: "g".into(),
            reason: "r".into(),
        },
        started_ms: 0,
        target_moved: false,
        checkout_interrupted: true,
        include_changes: Vec::new(),
        programs_digest: None,
        fold: None,
        finished_ms: None,
    };
    let r = lander
        .recover_checkout(&fx.repo, &entry)
        .expect("recovered");
    assert_eq!(r.restored, ["base.txt"]);
    assert_eq!(r.removed, ["added.txt"]);
    assert_eq!(r.left.len(), 1);
    assert_eq!(r.left[0].0, "part.txt");
    assert!(
        r.lock.as_deref().is_some_and(|l| l.contains("removed")),
        "{r:?}"
    );
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("base.txt")).expect("b"),
        "base\n"
    );
    assert!(!fx.repo.join("added.txt").exists());
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("part.txt")).expect("p"),
        "wh"
    );
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("notes.txt")).expect("n"),
        "my edit\n"
    );
    assert!(!fx.repo.join(".git/index.lock").exists());
}

/// git stopped in the middle of the checkout (a filter that hangs once):
/// the message names `--recover`, never "nothing was changed"; the journal
/// stays marked; after the recovery the land finishes.
#[test]
fn a_checkout_stopped_part_way_is_recovered_with_the_command_it_names() {
    let fx = fixture();
    let flag = fx.tmp.path().join("hung-once");
    git(
        &fx.repo,
        &[
            "config",
            "filter.slow.smudge",
            &format!(
                "if [ -e '{f}' ]; then cat; else touch '{f}'; sleep 30; fi",
                f = flag.display()
            ),
        ],
    );
    git(&fx.repo, &["config", "filter.slow.clean", "cat"]);
    std::fs::write(fx.repo.join(".gitattributes"), "*.slow filter=slow\n").expect("write");
    git(&fx.repo, &["add", ".gitattributes"]);
    git(&fx.repo, &["commit", "-q", "-m", "attributes"]);
    git(&fx.repo, &["branch", "-f", RUN, "main"]);
    let candidate = fx.commit(RUN, &[("a.slow", "x\n"), ("b.txt", "b\n")], "work");
    let lander = fx
        .lander()
        .with_user_git_timeout(std::time::Duration::from_secs(2));
    let mut ctx = fx.passed_on(&candidate);
    let main = fx.tip("main");
    let err = block_on(land(&lander, &mut ctx, &attended())).expect_err("stopped");
    let text = err.to_string();
    assert!(
        matches!(err, LandError::CheckoutInterrupted { .. }),
        "{text}"
    );
    assert!(
        text.contains("partly written") && text.contains("--recover"),
        "{text}"
    );
    assert!(!text.contains("nothing was changed"), "{text}");
    let journal = ctx.book.landing.clone().expect("the journal stays");
    assert!(journal.checkout_interrupted);
    assert_eq!(fx.tip("main"), main);
    lander
        .recover_checkout(&fx.repo, &journal)
        .expect("recovered");
    let (entry, _) = landed(block_on(land(&lander, &mut ctx, &attended())).expect("resumed"));
    assert_eq!(fx.tip("main"), entry.merge_commit);
}

/// The user's own filter runs as it was at the start, even when a worker
/// rewrote its command since.
#[test]
fn a_start_filter_runs_with_its_start_command() {
    let fx = fixture();
    git(&fx.repo, &["config", "filter.up.smudge", "tr a-z A-Z"]);
    git(&fx.repo, &["config", "filter.up.clean", "tr A-Z a-z"]);
    std::fs::write(fx.repo.join(".gitattributes"), "*.up filter=up\n").expect("write");
    git(&fx.repo, &["add", ".gitattributes"]);
    git(&fx.repo, &["commit", "-q", "-m", "attributes"]);
    git(&fx.repo, &["branch", "-f", RUN, "main"]);
    let candidate = fx.commit(RUN, &[("note.up", "hello\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let marker = fx.tmp.path().join("rewritten-filter");
    git(
        &fx.repo,
        &[
            "config",
            "filter.up.smudge",
            &format!("touch '{}'; cat", marker.display()),
        ],
    );
    fx.land_quietly(&lander, &mut ctx);
    assert!(!marker.exists(), "the rewritten command never ran");
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("note.up")).expect("note"),
        "HELLO\n",
        "the start command ran"
    );
}

/// The move runs only with the program snapshot the land was confirmed
/// with.
#[test]
fn a_move_with_another_snapshot_than_confirmed_moves_nothing() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("a.txt", "a\n")], "work");
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    kgraf_orchestrator::land_flow::prepare(&lander, &mut ctx, &attended()).expect("prepared");
    if let Some(p) = ctx.book.programs.as_mut() {
        p.config.insert("core.fsmonitor".into(), "/tmp/x".into());
    }
    let main = fx.tip("main");
    let err = block_on(land(&lander, &mut ctx, &attended())).expect_err("refused");
    assert!(
        err.to_string()
            .contains("not the one this land was confirmed with"),
        "{err}"
    );
    assert_eq!(fx.tip("main"), main);
}

/// Recovery refuses, writing nothing, when it cannot tell whether a
/// process holds the index lock.
#[test]
fn recovery_refuses_when_the_lock_holder_is_unknown() {
    let fx = fixture();
    let candidate = fx.commit(RUN, &[("base.txt", "new base\n")], "work");
    let lander = fx.lander().with_lsof(fx.tmp.path().join("no-lsof-here"));
    let main = fx.tip("main");
    let merge = match lander.build(&main, &candidate, "m", false).expect("build") {
        Built::Merge { commit, .. } => commit,
        Built::Conflict { .. } => panic!("conflict"),
    };
    std::fs::write(fx.repo.join("base.txt"), "new base\n").expect("write");
    let lock = fx.repo.join(".git/index.lock");
    std::fs::write(&lock, "").expect("lock");
    let entry = recovery_entry(candidate, merge, main);
    let err = lander
        .recover_checkout(&fx.repo, &entry)
        .expect_err("refused");
    assert!(err.to_string().contains("index.lock"), "{err}");
    assert!(lock.exists(), "the lock stays");
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("base.txt")).expect("b"),
        "new base\n",
        "nothing was written"
    );
}

fn recovery_entry(candidate: String, merge: String, old: String) -> LandingEntry {
    LandingEntry {
        run_branch: RUN.into(),
        candidate,
        merge_commit: merge,
        target: "main".into(),
        target_old: old,
        gate: LandGate::Untested {
            gate: "g".into(),
            reason: "r".into(),
        },
        started_ms: 0,
        target_moved: false,
        checkout_interrupted: true,
        include_changes: Vec::new(),
        programs_digest: None,
        fold: None,
        finished_ms: None,
    }
}

/// Recovery never writes through a directory that became a link: the path
/// is left alone, and nothing appears outside the work tree.
#[test]
fn recovery_does_not_write_through_a_linked_directory() {
    let fx = fixture();
    fx.commit("main", &[("sub/f.txt", "old\n")], "sub");
    git(&fx.repo, &["reset", "-q", "--hard"]);
    git(&fx.repo, &["branch", "-f", RUN, "main"]);
    let candidate = fx.commit(RUN, &[("sub/f.txt", "new\n")], "work");
    let lander = fx.lander();
    let main = fx.tip("main");
    let merge = match lander.build(&main, &candidate, "m", false).expect("build") {
        Built::Merge { commit, .. } => commit,
        Built::Conflict { .. } => panic!("conflict"),
    };
    let outside = fx.tmp.path().join("outside");
    std::fs::create_dir(&outside).expect("mkdir");
    std::fs::remove_dir_all(fx.repo.join("sub")).expect("rm");
    std::os::unix::fs::symlink(&outside, fx.repo.join("sub")).expect("link");
    let entry = recovery_entry(candidate, merge, main);
    let r = lander
        .recover_checkout(&fx.repo, &entry)
        .expect("recovered");
    assert_eq!(r.left.len(), 1, "{r:?}");
    assert!(r.left[0].1.contains("link"), "{r:?}");
    assert!(!outside.join("f.txt").exists(), "nothing written outside");
}

/// A merge that changes an attribute file (here assigning the user's own
/// lfs filter to new paths) is asked about, named with its diff.
#[test]
fn an_attribute_change_is_asked_about_with_its_diff() {
    let fx = fixture();
    let candidate = fx.commit(
        RUN,
        &[
            ("assets/.gitattributes", "*.bin filter=lfs\n"),
            ("assets/a.bin", "x\n"),
        ],
        "lfs",
    );
    let lander = fx.lander();
    let mut ctx = fx.passed_on(&candidate);
    let main = fx.tip("main");
    match block_on(land(&lander, &mut ctx, &attended())).expect("asked") {
        LandOutcome::NeedsProgramsYes { changes, .. } => assert!(
            changes
                .iter()
                .any(|c| c.contains("assets/.gitattributes") && c.contains("filter=lfs")),
            "{changes:?}"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.tip("main"), main);
}

/// The run-start snapshot is read where the start branch is checked out:
/// a linked worktree's own config (`extensions.worktreeConfig`) is kept.
#[test]
fn the_start_snapshot_is_read_in_the_start_branchs_worktree() {
    let fx = fixture();
    let linked = fx.tmp.path().join("linked");
    git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            linked.to_str().expect("utf8"),
        ],
    );
    git(&fx.repo, &["config", "extensions.worktreeConfig", "true"]);
    git(
        &linked,
        &["config", "--worktree", "filter.mine.smudge", "cat"],
    );
    let snap = start_snapshot(
        &Git::discover().expect("git"),
        &fx.repo,
        Some("feature"),
        &fx.tmp.path().join("start-hooks"),
    )
    .expect("snapshot");
    assert_eq!(
        snap.config.get("filter.mine.smudge").map(String::as_str),
        Some("cat")
    );
}
