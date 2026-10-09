//! Decision 0062: a project used under kGraf's former name, Haustoria, has its
//! state in `.haustoria`. The first use under kGraf moves it to `.kgraf` and
//! repairs what points into the old path. These tests build that state for
//! real (a git repository, linked worktrees registered under the old path,
//! attempt refs, a run record, a hash-chained ledger, configuration files,
//! a transcript) and check the effect, not a setting.

use kgraf_core::legacy::{LEGACY_STATE_DIR, STATE_DIR};
use kgraf_core::{Protocol, TokenUsage};
use kgraf_graph::GraphStore;
use kgraf_host::migrate::{MIGRATION_LOCK, MIGRATION_PENDING};
use kgraf_orchestrator::{OpenContract, RunStateRecord, RunStore};
use kgraf_provider::Ledger;
use kgraf_sandbox::{WorkspaceManager, WorkspaceSpec};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A repository with one commit. Canonical, so the paths git records and the
/// paths the test compares are spelled the same.
fn repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("dir");
    let root = kgraf_core::plain_path::canonicalize(dir.path()).expect("canonical");
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "a").expect("write");
    git(&root, &["add", "a.txt"]);
    git(&root, &["commit", "-q", "-m", "one"]);
    (dir, root)
}

/// Every admin `gitdir` file in the repository's common directory.
fn registrations(root: &Path) -> Vec<(PathBuf, String)> {
    let common = PathBuf::from(git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ));
    let Ok(entries) = std::fs::read_dir(common.join("worktrees")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path().join("gitdir"))
        .filter(|p| p.is_file())
        .map(|p| {
            let text = std::fs::read_to_string(&p).expect("gitdir");
            (p, text.trim().to_owned())
        })
        .collect()
}

/// Rewrite what the current build wrote into the form the former one did:
/// the worktree base marker's name and every lock reason's tag.
fn make_legacy(root: &Path, base: &Path) {
    std::fs::rename(
        base.join(".kgraf-workspaces"),
        base.join(".haustoria-workspaces"),
    )
    .expect("legacy marker");
    let common = PathBuf::from(git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ));
    for entry in std::fs::read_dir(common.join("worktrees"))
        .expect("admin")
        .flatten()
    {
        let locked = entry.path().join("locked");
        if let Ok(reason) = std::fs::read_to_string(&locked) {
            let old = reason.replacen(kgraf_sandbox::LOCK_TAG, kgraf_sandbox::LEGACY_LOCK_TAG, 1);
            std::fs::write(&locked, old).expect("legacy lock");
        }
    }
}

#[test]
fn a_haustoria_project_moves_to_kgraf_with_worktrees_records_and_ledger_intact() {
    let (_dir, root) = repo();
    let old = root.join(LEGACY_STATE_DIR);
    let new = root.join(STATE_DIR);
    let base = old.join("worktrees");

    // Two worktrees registered under the old path, as a killed run leaves
    // them: one still on disk with uncommitted work, one whose directory is
    // already gone.
    let manager = WorkspaceManager::open(&root, &base).expect("manager");
    let live = manager
        .create(&WorkspaceSpec {
            name: "w1",
            base: "HEAD",
            branch: None,
        })
        .expect("w1");
    let gone = manager
        .create(&WorkspaceSpec {
            name: "w2",
            base: "HEAD",
            branch: None,
        })
        .expect("w2");
    std::fs::write(live.root().join("work.txt"), "in progress").expect("work");
    let gone_root = gone.root().to_path_buf();
    drop((live, gone, manager));
    std::fs::remove_dir_all(&gone_root).expect("remove w2");
    make_legacy(&root, &base);
    assert!(
        registrations(&root)
            .iter()
            .all(|(_, p)| p.contains("/.haustoria/")),
        "{:?}",
        registrations(&root)
    );

    // An attempt ref and an integration branch under the former prefix.
    git(
        &root,
        &["update-ref", "refs/haustoria/attempts/w1+1", "HEAD"],
    );
    git(&root, &["branch", "haustoria/r1", "HEAD"]);

    // A hash-chained ledger with two settled calls.
    {
        let ledger = Ledger::open(&old.join("ledger.db")).expect("ledger");
        for worker in ["w1", "w2"] {
            let token = ledger
                .begin("test", Protocol::Inclusive, "m", worker, "t")
                .expect("begin");
            token
                .settle(
                    &TokenUsage {
                        uncached_input: 10,
                        text_output: 2,
                        ..TokenUsage::default()
                    },
                    None,
                )
                .expect("settle");
        }
        ledger.flush_anchor();
    }
    assert!(old.join("ledger.tip").is_file(), "the anchor is beside it");

    // A run record naming a worktree by its absolute path.
    let old_worktree = base.join("w1");
    {
        let graph = GraphStore::open(&old.join("graph.db"), &root).expect("graph");
        let store = RunStore::new(Arc::new(Mutex::new(graph)));
        let mut record = RunStateRecord::new("r1".into(), "do it", 1);
        let mut contract = OpenContract::planned("c1", "t1".into(), "do it", "done");
        contract.workspace = Some(old_worktree.clone());
        record.open_contracts.push(contract);
        store.save(&record).expect("save");
    }

    // Configuration a person wrote by hand, and the allow-rule store.
    let run_json = format!(
        "{{\n  \"prices\": \"{}\",\n  \"routes\": [\"kimi\"]\n}}\n",
        old.join("prices.json").display()
    );
    std::fs::write(old.join("run.json"), &run_json).expect("run.json");
    let allow = format!(
        "{{\"rules\":[{{\"subject\":\"cat {}/notes.txt\"}}]}}",
        old.display()
    );
    std::fs::write(old.join("allow-rules.json"), &allow).expect("allow");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            old.join("allow-rules.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("mode");
    }

    // A transcript: history, never rewritten.
    let transcript = format!("{{\"text\":\"wrote {}/w1/x\"}}\n", base.display());
    std::fs::create_dir_all(old.join("runs/r1/workers")).expect("runs");
    std::fs::write(old.join("runs/r1/workers/w1.jsonl"), &transcript).expect("transcript");

    // First use under kGraf.
    let report = kgraf_host::migrate_state_dir(&root)
        .expect("migrated")
        .expect("something moved");
    let state = kgraf_host::state_dir(&root).expect("state dir");
    assert_eq!(state, new);
    assert!(!old.exists(), "the old directory is gone");
    assert!(!root.join(MIGRATION_LOCK).exists(), "no pending mark left");
    assert_eq!(report.from, old);
    assert_eq!(report.to, new);
    let notice = report.notice();
    assert!(notice.contains("former name"), "{notice}");

    // Git: every registration names the new path, the moved worktree works
    // there with its uncommitted work, and git's own view agrees.
    let new_live = new.join("worktrees/w1");
    assert_eq!(report.worktrees_repaired, vec![new_live.clone()]);
    assert_eq!(report.registrations_rewritten, 1);
    let regs = registrations(&root);
    assert_eq!(regs.len(), 2, "{regs:?}");
    for (file, pointed) in &regs {
        assert!(
            !pointed.contains(".haustoria"),
            "{}: {pointed}",
            file.display()
        );
        assert!(pointed.contains("/.kgraf/worktrees/"), "{pointed}");
    }
    let listing = git(&root, &["worktree", "list", "--porcelain"]);
    assert!(!listing.contains(".haustoria"), "{listing}");
    // Compared as paths: git writes `/` between components on Windows too.
    let listed: Vec<PathBuf> = listing
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect();
    assert!(
        listed.contains(&new_live),
        "{} not in {listing}",
        new_live.display()
    );
    let status = git(&new_live, &["status", "--porcelain"]);
    assert!(status.contains("work.txt"), "{status}");
    let link = std::fs::read_to_string(new_live.join(".git")).expect("link");
    let admin = PathBuf::from(link.trim().trim_start_matches("gitdir:").trim());
    let back = std::fs::read_to_string(admin.join("gitdir")).expect("back");
    assert_eq!(PathBuf::from(back.trim()), new_live.join(".git"));
    // The sandbox opens the moved base: the marker under the former name is
    // adopted, and the locks under the former tag are read as ours.
    let manager = WorkspaceManager::open(&root, &new.join("worktrees")).expect("reopen base");
    assert!(new.join("worktrees/.kgraf-workspaces").is_file());
    assert!(!new.join("worktrees/.haustoria-workspaces").exists());
    manager.startup().expect("startup cleanup");
    let after = git(&root, &["worktree", "list", "--porcelain"]);
    assert!(
        !after.contains("worktrees/w2"),
        "the gone worktree's registration was cleaned at its new path: {after}"
    );

    // Refs: the attempt ref moved; the user's integration branch did not.
    assert_eq!(
        git(
            &root,
            &["for-each-ref", "--format=%(refname)", "refs/haustoria/"]
        ),
        ""
    );
    assert_eq!(
        git(
            &root,
            &["for-each-ref", "--format=%(refname)", "refs/kgraf/"]
        ),
        "refs/kgraf/attempts/w1+1"
    );
    assert_eq!(report.refs_moved, 1);
    assert_eq!(
        git(&root, &["branch", "--list", "haustoria/r1"]).trim(),
        "haustoria/r1"
    );

    // The ledger moved as a file: its chain and anchor verify unchanged.
    assert!(new.join("ledger.tip").is_file());
    let ledger = Ledger::open(&new.join("ledger.db")).expect("ledger");
    let verified = ledger.verify().expect("verify");
    assert!(verified.ok, "{:?}", verified.first_broken);
    assert!(verified.rows_checked >= 2, "{}", verified.rows_checked);

    // The run record names the worktree at its new path.
    let graph = GraphStore::open(&new.join("graph.db"), &root).expect("graph");
    let store = RunStore::new(Arc::new(Mutex::new(graph)));
    let record = store.load(&"r1".into()).expect("load").expect("record");
    assert_eq!(record.open_contracts[0].workspace, Some(new_live.clone()));
    assert!(report.records_rewritten >= 1);

    // Configuration rewritten in place, layout and permissions kept.
    let run_after = std::fs::read_to_string(new.join("run.json")).expect("run.json");
    assert_eq!(
        run_after,
        run_json.replace(&old.display().to_string(), &new.display().to_string())
    );
    let allow_after = std::fs::read_to_string(new.join("allow-rules.json")).expect("allow");
    assert!(
        allow_after.contains(&format!("cat {}/notes.txt", new.display())),
        "{allow_after}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(new.join("allow-rules.json"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // History is left as it was.
    assert_eq!(
        std::fs::read_to_string(new.join("runs/r1/workers/w1.jsonl")).expect("transcript"),
        transcript
    );

    // The exclude file names the new directory.
    let exclude = git(&root, &["rev-parse", "--git-path", "info/exclude"]);
    let exclude = std::fs::read_to_string(root.join(exclude)).expect("exclude");
    assert!(exclude.lines().any(|l| l == "/.kgraf/"), "{exclude}");

    // Once only: a second use finds nothing to move.
    assert_eq!(kgraf_host::migrate_state_dir(&root).expect("again"), None);
}

#[test]
fn both_directories_present_is_refused_naming_both() {
    let dir = tempfile::tempdir().expect("dir");
    let root = kgraf_core::plain_path::canonicalize(dir.path()).expect("canonical");
    std::fs::create_dir_all(root.join(LEGACY_STATE_DIR)).expect("old");
    std::fs::create_dir_all(root.join(STATE_DIR)).expect("new");
    std::fs::write(root.join(LEGACY_STATE_DIR).join("ledger.db"), "old").expect("write");

    let err = kgraf_host::state_dir(&root).expect_err("refused");
    let text = err.to_string();
    assert!(
        text.contains(&root.join(LEGACY_STATE_DIR).display().to_string()),
        "{text}"
    );
    assert!(
        text.contains(&root.join(STATE_DIR).display().to_string()),
        "{text}"
    );
    // Nothing was touched.
    assert_eq!(
        std::fs::read_to_string(root.join(LEGACY_STATE_DIR).join("ledger.db")).expect("read"),
        "old"
    );
    assert!(!root.join(MIGRATION_LOCK).exists());

    // The same under the lock (a stale, empty lock file from a killed first
    // use): refused, and no lock file is left behind.
    std::fs::write(root.join(MIGRATION_LOCK), "").expect("stale lock");
    let err = kgraf_host::migrate_state_dir(&root).expect_err("refused");
    assert!(err.to_string().contains("both"), "{err}");
    assert!(!root.join(MIGRATION_LOCK).exists());
}

#[test]
fn a_directory_a_process_still_runs_in_is_not_moved() {
    let dir = tempfile::tempdir().expect("dir");
    let root = kgraf_core::plain_path::canonicalize(dir.path()).expect("canonical");
    let holds = root.join(LEGACY_STATE_DIR).join("worktrees/.holds");
    std::fs::create_dir_all(&holds).expect("holds");
    let hold = std::fs::File::create(holds.join("w1")).expect("hold");
    hold.lock_shared().expect("held by a running worker");

    let err = kgraf_host::migrate_state_dir(&root).expect_err("refused");
    assert!(err.to_string().contains("still running"), "{err}");
    assert!(root.join(LEGACY_STATE_DIR).is_dir(), "left in place");
    assert!(!root.join(STATE_DIR).exists());
    assert!(!root.join(MIGRATION_LOCK).exists(), "no pending mark");

    drop(hold);
    let report = kgraf_host::migrate_state_dir(&root)
        .expect("moved once the worker is gone")
        .expect("report");
    assert!(!report.resumed);
    assert!(root.join(STATE_DIR).is_dir());
}

/// A move killed after the rename and before its repairs leaves the pending
/// mark; the next use finishes the repairs.
#[test]
fn an_interrupted_move_is_finished_by_the_next_use() {
    let (_dir, root) = repo();
    let old = root.join(LEGACY_STATE_DIR);
    let new = root.join(STATE_DIR);
    let manager = WorkspaceManager::open(&root, &old.join("worktrees")).expect("manager");
    let ws = manager
        .create(&WorkspaceSpec {
            name: "w1",
            base: "HEAD",
            branch: None,
        })
        .expect("w1");
    drop((ws, manager));
    make_legacy(&root, &old.join("worktrees"));
    git(&root, &["update-ref", "refs/haustoria/attempts/w1", "HEAD"]);

    // The kill: renamed, mark written, nothing repaired.
    std::fs::rename(&old, &new).expect("rename");
    std::fs::write(root.join(MIGRATION_LOCK), MIGRATION_PENDING).expect("mark");

    let report = kgraf_host::migrate_state_dir(&root)
        .expect("finished")
        .expect("report");
    assert!(report.resumed);
    assert_eq!(report.worktrees_repaired, vec![new.join("worktrees/w1")]);
    assert_eq!(report.refs_moved, 1);
    assert!(!root.join(MIGRATION_LOCK).exists());
    for (_, pointed) in registrations(&root) {
        assert!(!pointed.contains(".haustoria"), "{pointed}");
    }
}

/// A repository that ships `.kgraf.migrating` as a link must
/// not have the link's target opened, truncated or written.
#[cfg(unix)]
#[test]
fn a_linked_migration_lock_is_refused_and_its_target_untouched() {
    let dir = tempfile::tempdir().expect("dir");
    let root = kgraf_core::plain_path::canonicalize(dir.path()).expect("canonical");
    let victim = root.join("victim.txt");
    std::fs::write(&victim, "precious").expect("victim");
    std::fs::create_dir_all(root.join(LEGACY_STATE_DIR)).expect("old");
    std::os::unix::fs::symlink(&victim, root.join(MIGRATION_LOCK)).expect("link");

    let err = kgraf_host::migrate_state_dir(&root).expect_err("refused");
    assert!(err.to_string().contains("symbolic link"), "{err}");
    assert_eq!(std::fs::read_to_string(&victim).expect("read"), "precious");
    assert!(root.join(LEGACY_STATE_DIR).is_dir(), "nothing moved");
    assert!(!root.join(STATE_DIR).exists());

    // A dangling link is not followed to create its target either.
    std::fs::remove_file(root.join(MIGRATION_LOCK)).expect("rm");
    let elsewhere = root.join("created-through-link");
    std::os::unix::fs::symlink(&elsewhere, root.join(MIGRATION_LOCK)).expect("link");
    assert!(kgraf_host::migrate_state_dir(&root).is_err());
    assert!(!elsewhere.exists());
}

/// A link among the files the move writes or locks (a JSON
/// file, the graph database, a hold file) refuses the move before anything
/// moves, and the link's target is left as it was.
#[cfg(unix)]
#[test]
fn links_in_the_old_state_directory_are_refused_before_the_move() {
    for planted in ["worker.json", "graph.db", "worktrees/.holds/w1"] {
        let dir = tempfile::tempdir().expect("dir");
        let root = kgraf_core::plain_path::canonicalize(dir.path()).expect("canonical");
        let old = root.join(LEGACY_STATE_DIR);
        let victim = root.join("victim.txt");
        let text = format!("{{\"p\":\"{}/x\"}}", old.display());
        std::fs::write(&victim, &text).expect("victim");
        let link = old.join(planted);
        std::fs::create_dir_all(link.parent().expect("parent")).expect("mkdir");
        std::os::unix::fs::symlink(&victim, &link).expect("link");

        let err = kgraf_host::migrate_state_dir(&root).expect_err(planted);
        assert!(
            err.to_string().contains("symbolic link"),
            "{planted}: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).expect("read"),
            text,
            "{planted}"
        );
        assert!(old.is_dir(), "{planted}: nothing moved");
        assert!(!root.join(STATE_DIR).exists(), "{planted}");
        assert!(!root.join(MIGRATION_LOCK).exists(), "{planted}");
    }
}

/// A worktree under the old directory whose git lock names a live
/// owner of an earlier version refuses the move.
#[test]
fn a_worktree_locked_by_a_live_older_process_is_not_moved() {
    let (_dir, root) = repo();
    let base = root.join(LEGACY_STATE_DIR).join("worktrees");
    let manager = WorkspaceManager::open(&root, &base).expect("manager");
    let ws = manager
        .create(&WorkspaceSpec {
            name: "w1",
            base: "HEAD",
            branch: None,
        })
        .expect("w1");
    let path = ws.root().to_path_buf();
    drop((ws, manager));
    // A live process other than this one owns it, as a Haustoria worker would.
    let mut child = Command::new("sleep").arg("60").spawn().expect("sleep");
    git(&root, &["worktree", "unlock", &path.to_string_lossy()]);
    let reason = format!("haustoria:v1:pid={}:name=w1", child.id());
    git(
        &root,
        &[
            "worktree",
            "lock",
            "--reason",
            &reason,
            &path.to_string_lossy(),
        ],
    );

    let result = kgraf_host::migrate_state_dir(&root);
    let _ = child.kill();
    let _ = child.wait();
    let err = result.expect_err("refused");
    assert!(err.to_string().contains("still works in"), "{err}");
    assert!(path.is_dir(), "left in place");
    assert!(!root.join(STATE_DIR).exists());

    // Once the owner is gone, the move goes ahead.
    let report = kgraf_host::migrate_state_dir(&root)
        .expect("moved")
        .expect("report");
    assert_eq!(report.worktrees_repaired.len(), 1);
}

/// Many first uses at once. Exactly one moves; the others see a
/// finished move ("nothing to do"), never one to resume; no lock is left.
#[test]
fn concurrent_first_uses_move_once() {
    const N: usize = 8;
    for _ in 0..5 {
        let dir = tempfile::tempdir().expect("dir");
        let root = Arc::new(kgraf_core::plain_path::canonicalize(dir.path()).expect("canonical"));
        std::fs::create_dir_all(root.join(LEGACY_STATE_DIR).join("runs")).expect("old");
        std::fs::write(root.join(LEGACY_STATE_DIR).join("run.json"), "{}").expect("run.json");
        let barrier = Arc::new(std::sync::Barrier::new(N));
        let threads: Vec<_> = (0..N)
            .map(|_| {
                let root = Arc::clone(&root);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    kgraf_host::migrate_state_dir(&root)
                })
            })
            .collect();
        let results: Vec<_> = threads
            .into_iter()
            .map(|t| t.join().expect("thread").expect("no error"))
            .collect();
        let moved: Vec<_> = results.iter().flatten().collect();
        assert_eq!(moved.len(), 1, "{results:?}");
        assert!(!moved[0].resumed, "{results:?}");
        assert!(root.join(STATE_DIR).join("run.json").is_file());
        assert!(!root.join(LEGACY_STATE_DIR).exists());
        assert!(!root.join(MIGRATION_LOCK).exists());
    }
}

/// A record naming the old directory in a spelling the rewrite
/// does not match is said in the notice, never silently left.
#[test]
fn a_path_the_rewrite_cannot_match_is_named_in_the_notice() {
    let dir = tempfile::tempdir().expect("dir");
    let root = kgraf_core::plain_path::canonicalize(dir.path()).expect("canonical");
    let old = root.join(LEGACY_STATE_DIR);
    std::fs::create_dir_all(&old).expect("old");
    std::fs::write(
        old.join("run.json"),
        format!(
            "{{\"prices\":\"{}/prices.json\",\"other\":\".haustoria/prices.json\"}}",
            old.display()
        ),
    )
    .expect("run.json");
    let report = kgraf_host::migrate_state_dir(&root)
        .expect("moved")
        .expect("report");
    assert_eq!(report.still_naming_old, vec!["run.json".to_owned()]);
    let notice = report.notice();
    assert!(notice.contains("run.json"), "{notice}");
    assert!(notice.contains("check them by hand"), "{notice}");
}
