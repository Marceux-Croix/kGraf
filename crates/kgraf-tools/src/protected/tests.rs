//! The protected-write guard against real repositories: each test makes a
//! repository with a linked worker worktree in a temporary directory.

use super::*;
use crate::permissions::{ModeSource, PermissionSettings, ProcessFacts};
use std::time::Duration;

pub(crate) struct Repo {
    _dir: tempfile::TempDir,
    pub(crate) checkout: PathBuf,
    pub(crate) worktree: PathBuf,
    pub(crate) common: PathBuf,
    pub(crate) admin: PathBuf,
    pub(crate) state: PathBuf,
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for var in kgraf_sandbox::git::GIT_REDIRECT_ENV {
        cmd.env_remove(var);
    }
    let out = cmd
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A repository with one commit on `main`, a second branch `other`, a linked
/// worktree on branch `work`, and a state directory beside the worktrees.
pub(crate) fn repo() -> Repo {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().canonicalize().expect("canonical");
    let checkout = base.join("project");
    std::fs::create_dir_all(&checkout).expect("mkdir");
    git(&checkout, &["init", "-q", "-b", "main"]);
    std::fs::write(checkout.join("a.txt"), "a\n").expect("write");
    git(&checkout, &["add", "a.txt"]);
    git(&checkout, &["commit", "-q", "-m", "one"]);
    git(&checkout, &["branch", "other"]);
    let state = checkout.join(".kgraf");
    std::fs::create_dir_all(state.join("worktrees")).expect("state");
    let worktree = state.join("worktrees").join("w1");
    git(
        &checkout,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "work",
            worktree.to_str().expect("utf8"),
        ],
    );
    let common = checkout.join(".git");
    let admin = common.join("worktrees").join("w1");
    assert!(admin.is_dir(), "{}", admin.display());
    Repo {
        _dir: dir,
        checkout,
        worktree,
        common,
        admin,
        state,
    }
}

pub(crate) fn control(mode: PermissionMode) -> Arc<PermissionControl> {
    let settings = PermissionSettings {
        mode,
        ..PermissionSettings::default()
    };
    Arc::new(
        PermissionControl::new(settings, ModeSource::CommandLine, ProcessFacts::default())
            .expect("control"),
    )
}

/// Settings are valid JSON objects; the host's own parser stands behind this
/// in a real run.
pub(crate) fn json_check() -> SettingsCheck {
    Arc::new(|name: &str, path: &Path| {
        let text = std::fs::read(path).map_err(|e| e.to_string())?;
        match serde_json::from_slice::<serde_json::Value>(&text) {
            Ok(v) if v.is_object() => Ok(()),
            Ok(_) => Err(format!("{name} is not a JSON object")),
            Err(e) => Err(format!("{name} does not parse: {e}")),
        }
    })
}

pub(crate) fn guard(r: &Repo, control: Arc<PermissionControl>) -> ProtectedWrites {
    ProtectedWrites::from_parts(
        control,
        "w1",
        r.worktree.clone(),
        r.admin.clone(),
        r.common.clone(),
        r.state.clone(),
        json_check(),
    )
}

fn outcomes(control: &PermissionControl) -> Vec<(String, String)> {
    control
        .events_since(0)
        .into_iter()
        .filter_map(|(_, e)| match e {
            crate::PermissionEvent::ProtectedWrite {
                path,
                outcome,
                mode,
                ..
            } => {
                assert_eq!(mode, PermissionMode::Bypass);
                Some((path, outcome))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn outside_bypass_every_protected_path_is_refused_with_the_old_reason() {
    let r = repo();
    for mode in [
        PermissionMode::Manual,
        PermissionMode::AcceptEdits,
        PermissionMode::Auto,
        PermissionMode::OrchestratorDecides,
    ] {
        let g = guard(&r, control(mode));
        for p in [
            ".git/info/exclude",
            ".git/refs/heads/work",
            ".kgraf/run.json",
        ] {
            let why = g
                .admit(Path::new(p))
                .expect("protected")
                .expect_err("refused");
            assert_eq!(
                Some(why),
                kgraf_core::paths::forbidden_reason(Path::new(p)),
                "{mode:?} {p}"
            );
        }
        assert!(g.admit(Path::new("src/lib.rs")).is_none());
    }
}

#[test]
fn bypass_resolves_git_paths_the_way_git_does() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    let t = g
        .admit(Path::new(".git/info/exclude"))
        .expect("protected")
        .expect("admitted");
    assert_eq!(
        (t.area, t.root, t.rel),
        (Area::Git, r.common.clone(), PathBuf::from("info/exclude"))
    );
    let t = g
        .admit(Path::new(".git/refs/heads/work"))
        .expect("p")
        .expect("a");
    assert_eq!(
        (t.root, t.rel),
        (r.common.clone(), PathBuf::from("refs/heads/work"))
    );
    let t = g.admit(Path::new(".git/HEAD")).expect("p").expect("a");
    assert_eq!((t.root, t.rel), (r.admin.clone(), PathBuf::from("HEAD")));
    let t = g
        .admit(Path::new(".kgraf/run.json"))
        .expect("p")
        .expect("a");
    assert_eq!(
        (t.area, t.root, t.rel),
        (
            Area::Settings("run.json".into()),
            r.state.clone(),
            PathBuf::from("run.json")
        )
    );
    assert!(g.admit(Path::new(".kgraf/worker.json")).expect("p").is_ok());
}

/// kGraf's own books stay refused in Bypass; so do the link, objects, the
/// other names that steer git, and paths that leave the worktree.
#[test]
fn bypass_still_refuses_the_books_and_the_rest() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    for p in [
        ".kgraf/ledger.db",
        ".kgraf/ledger.db-wal",
        ".kgraf/graph.db",
        ".kgraf/schedule.db",
        ".kgraf/runs/r1/trace.jsonl",
        ".kgraf/runs/r1/record.json",
        ".kgraf/worker-w1.jsonl",
        ".kgraf/allow-rules.json",
        ".kgraf/worktrees/w2/src/lib.rs",
        ".kgraf",
    ] {
        let why = g.admit(Path::new(p)).expect("protected").expect_err(p);
        assert!(why.contains("books"), "{p}: {why}");
    }
    for p in [
        ".git",
        ".git/objects/ab/cdef",
        ".gitattributes",
        ".gitmodules",
        ".githooks/pre-commit",
        "src/.git/config",
        ".GIT/config",
        ".haustoria/ledger.db",
        "../x",
    ] {
        assert!(g.admit(Path::new(p)).expect("protected").is_err(), "{p}");
    }
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// A second linked worktree `name` on a new branch, with its admin dir.
fn add_worktree(r: &Repo, name: &str) -> (PathBuf, PathBuf) {
    let wt = r.state.join("worktrees").join(name);
    git(
        &r.checkout,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &format!("b-{name}"),
            wt.to_str().expect("utf8"),
        ],
    );
    (wt, r.common.join("worktrees").join(name))
}

fn guard_for(
    r: &Repo,
    c: Arc<PermissionControl>,
    name: &str,
    wt: PathBuf,
    admin: PathBuf,
) -> ProtectedWrites {
    ProtectedWrites::from_parts(
        c,
        name,
        wt,
        admin,
        r.common.clone(),
        r.state.clone(),
        json_check(),
    )
}

fn fsck_ok(r: &Repo) {
    git(
        &r.checkout,
        &["fsck", "--connectivity-only", "--no-dangling"],
    );
}

fn loose(r: &Repo, oid: &str) -> PathBuf {
    r.common.join("objects").join(&oid[..2]).join(&oid[2..])
}

/// Pack every object so far, then commit `file` on `main`, so the store has
/// one pack and the new commit's loose objects. Returns (pack files, commit).
fn packed_then_loose(r: &Repo, file: &str) -> (Vec<PathBuf>, String) {
    git(&r.checkout, &["repack", "-a", "-d", "-q"]);
    let mut packs: Vec<PathBuf> = std::fs::read_dir(r.common.join("objects/pack"))
        .expect("pack dir")
        .flatten()
        .map(|e| e.path())
        .collect();
    packs.sort();
    assert!(
        packs
            .iter()
            .any(|p| p.extension() == Some(OsStr::new("pack"))),
        "{packs:?}"
    );
    std::fs::write(r.checkout.join(file), format!("{file}\n")).expect("write");
    git(&r.checkout, &["add", file]);
    git(&r.checkout, &["commit", "-q", "-m", file]);
    (packs, git(&r.checkout, &["rev-parse", "HEAD"]))
}

fn bytes(paths: &[PathBuf]) -> Vec<Vec<u8>> {
    paths
        .iter()
        .map(|p| std::fs::read(p).expect("read"))
        .collect()
}

#[test]
fn a_good_git_write_is_kept_and_logged() {
    let r = repo();
    let c = control(PermissionMode::Bypass);
    let g = guard(&r, c.clone());
    let w = g.before_write(&Area::Git).expect("snapshot");
    let exclude = r.common.join("info/exclude");
    let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
    text.push_str("/scratch/\n");
    std::fs::write(&exclude, &text).expect("write");
    g.after_write(&Area::Git, ".git/info/exclude")
        .expect("passes");
    drop(w);
    assert_eq!(std::fs::read_to_string(&exclude).expect("read"), text);
    assert_eq!(
        outcomes(&c),
        vec![(".git/info/exclude".into(), "kept".into())]
    );
    g.finish().expect("fsck passes");
    assert!(g.failure().is_none());
}

/// A problem the repository had before the window opened is not blamed on
/// the write.
#[test]
fn a_problem_that_was_already_there_does_not_fail_a_good_write() {
    let r = repo();
    std::fs::write(r.common.join("refs/heads/broken"), "junk\n").expect("pre-existing");
    let g = guard(&r, control(PermissionMode::Bypass));
    let _w = g.before_write(&Area::Git).expect("snapshot");
    std::fs::write(r.common.join("info/exclude"), "/x/\n").expect("write");
    g.after_write(&Area::Git, ".git/info/exclude")
        .expect("kept");
    assert_eq!(
        std::fs::read_to_string(r.common.join("refs/heads/broken")).expect("left as found"),
        "junk\n"
    );
}

#[test]
fn a_garbage_ref_is_detected_restored_and_fails_the_attempt() {
    let r = repo();
    let c = control(PermissionMode::Bypass);
    let g = guard(&r, c.clone());
    let before = git(&r.checkout, &["rev-parse", "refs/heads/work"]);
    let w = g.before_write(&Area::Git).expect("snapshot");
    std::fs::write(r.common.join("refs/heads/work"), "not a ref at all\n").expect("corrupt");
    let err = g
        .after_write(&Area::Git, ".git/refs/heads/work")
        .expect_err("detected");
    drop(w);
    assert!(err.contains("refs/heads/work"), "{err}");
    assert_eq!(git(&r.checkout, &["rev-parse", "refs/heads/work"]), before);
    fsck_ok(&r);
    let failure = g.finish().expect_err("the attempt fails");
    assert!(
        failure.contains("restored") && failure.contains("refs/heads/work"),
        "{failure}"
    );
    let logged = outcomes(&c);
    assert_eq!(logged.len(), 1);
    assert!(logged[0].1.starts_with("restored: "), "{logged:?}");
}

/// A ref that names a missing object is as broken as garbage.
#[test]
fn a_ref_to_a_missing_object_is_detected() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    let before = std::fs::read(r.common.join("refs/heads/work")).expect("ref");
    let _w = g.before_write(&Area::Git).expect("snapshot");
    std::fs::write(
        r.common.join("refs/heads/work"),
        format!("{}\n", "1".repeat(40)),
    )
    .expect("corrupt");
    let err = g
        .after_write(&Area::Git, ".git/refs/heads/work")
        .expect_err("detected");
    assert!(err.contains("does not exist"), "{err}");
    assert_eq!(
        std::fs::read(r.common.join("refs/heads/work")).expect("ref"),
        before
    );
}

/// A ref another merge moved between this attempt's windows is the
/// repository's state when the next window opens, never reverted by that
/// window's restore.
#[test]
fn the_restore_keeps_a_ref_moved_between_windows() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    let w = g.before_write(&Area::Git).expect("snapshot");
    std::fs::write(r.common.join("info/exclude"), "/x/\n").expect("write");
    g.after_write(&Area::Git, ".git/info/exclude")
        .expect("kept");
    drop(w);
    // Another merge, between the windows: `other` moves to a new commit.
    std::fs::write(r.checkout.join("b.txt"), "b\n").expect("write");
    git(&r.checkout, &["add", "b.txt"]);
    git(&r.checkout, &["commit", "-q", "-m", "two"]);
    let moved = git(&r.checkout, &["rev-parse", "HEAD"]);
    git(&r.checkout, &["update-ref", "refs/heads/other", &moved]);
    let _w = g.before_write(&Area::Git).expect("snapshot");
    std::fs::write(r.common.join("refs/heads/work"), "garbage\n").expect("corrupt");
    g.after_write(&Area::Git, ".git/refs/heads/work")
        .expect_err("detected");
    assert_eq!(git(&r.checkout, &["rev-parse", "refs/heads/other"]), moved);
    git(&r.checkout, &["rev-parse", "--verify", "refs/heads/work"]);
}

#[test]
fn a_broken_config_is_restored() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    let before = std::fs::read(r.common.join("config")).expect("config");
    let _w = g.before_write(&Area::Git).expect("snapshot");
    std::fs::write(r.common.join("config"), "[core\nbroken = = =\n").expect("corrupt");
    let err = g
        .after_write(&Area::Git, ".git/config")
        .expect_err("detected");
    assert!(err.contains("config"), "{err}");
    assert_eq!(
        std::fs::read(r.common.join("config")).expect("config"),
        before
    );
}

/// A shell command's git writes are found by comparing with the snapshot,
/// and named exactly (no other writer ran during the window).
#[test]
fn a_shell_command_that_corrupts_a_ref_is_caught_after_it_runs() {
    let r = repo();
    let c = control(PermissionMode::Bypass);
    let g = guard(&r, c.clone());
    let w = g
        .before_shell(&argv(&["git", "status"]))
        .expect("snapshot")
        .expect("bypass");
    assert_eq!(w.root(), r.common);
    g.after_shell(w).expect("nothing changed");
    assert!(outcomes(&c).is_empty());
    let w = g
        .before_shell(&argv(&["git", "status"]))
        .expect("snapshot")
        .expect("bypass");
    std::fs::write(r.common.join("refs/heads/other"), "zzz\n").expect("corrupt");
    let err = g.after_shell(w).expect_err("detected");
    assert!(
        err.contains("a protected write to .git/refs/heads/other failed"),
        "{err}"
    );
    assert!(!err.contains("while a shell command ran"), "{err}");
    git(&r.checkout, &["rev-parse", "--verify", "refs/heads/other"]);
    assert!(g.finish().is_err());
}

#[test]
fn outside_bypass_the_shell_gets_no_git_window() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Auto));
    assert!(
        g.before_shell(&argv(&["git", "commit"]))
            .expect("ok")
            .is_none()
    );
}

/// In Bypass only a command that names git or the git directory opens a
/// window; a build does not, so it never waits on the repository lock.
#[test]
fn only_a_command_that_names_git_opens_a_window() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    for words in [
        vec!["git", "commit", "-m", "x"],
        vec!["/usr/bin/git", "status"],
        vec!["sh", "-c", "echo x > .git/info/exclude"],
        vec!["sh", "-c", "make GIT=1"],
    ] {
        assert!(g.names_git(&argv(&words)), "{words:?}");
    }
    for words in [
        vec!["cargo", "build"],
        vec!["sh", "-c", "make test"],
        vec!["ls", "-la"],
    ] {
        assert!(!g.names_git(&argv(&words)), "{words:?}");
        assert!(g.before_shell(&argv(&words)).expect("ok").is_none());
    }
}

#[test]
fn a_settings_write_is_checked_and_restored_when_invalid() {
    let r = repo();
    let c = control(PermissionMode::Bypass);
    let g = guard(&r, c.clone());
    let run = r.state.join("run.json");
    std::fs::write(&run, "{\"routes\":[]}").expect("seed");
    let area = Area::Settings("run.json".into());
    let _w = g.before_write(&area).expect("snapshot");
    std::fs::write(&run, "{\"routes\":[\"a:b\"]}").expect("write");
    g.after_write(&area, ".kgraf/run.json").expect("valid");
    // The snapshot is refreshed to the kept write, then a bad write is undone.
    std::fs::write(&run, "{ not json").expect("write");
    let err = g
        .after_write(&area, ".kgraf/run.json")
        .expect_err("invalid");
    assert!(err.contains("does not parse"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&run).expect("read"),
        "{\"routes\":[\"a:b\"]}"
    );
    let logged = outcomes(&c);
    assert_eq!(logged[0], (".kgraf/run.json".into(), "kept".into()));
    assert!(logged[1].1.starts_with("restored: "));
    assert!(g.finish().is_err());
}

#[test]
fn a_new_invalid_settings_file_is_removed() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    let area = Area::Settings("worker.json".into());
    let _w = g.before_write(&area).expect("snapshot");
    std::fs::write(r.state.join("worker.json"), "[]").expect("write");
    g.after_write(&area, ".kgraf/worker.json")
        .expect_err("not an object");
    assert!(!r.state.join("worker.json").exists());
}

#[test]
fn a_settings_file_over_the_bound_is_not_snapshotted() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    let big = vec![b' '; (SETTINGS_FILE_LIMIT + 1) as usize];
    std::fs::write(r.state.join("run.json"), big).expect("write");
    let err = g
        .before_write(&Area::Settings("run.json".into()))
        .expect_err("over the bound");
    assert!(
        matches!(&err, WindowError::Unprotected(e) if e.contains("bound")),
        "{err}"
    );
}

// ---- the object store ----

/// A Bypass shell command that deletes a loose object and a pack is caught
/// when it ends: both are put back from the hard-link snapshot, byte for
/// byte, the repository passes fsck, and the attempt is failed (the damage
/// was its own; a repaired repository does not make its work trustworthy).
/// The snapshot is deleted when the attempt ends.
#[test]
fn deleted_objects_are_restored_and_the_attempt_fails() {
    let r = repo();
    let c = control(PermissionMode::Bypass);
    let g = guard(&r, c.clone());
    let (packs, head) = packed_then_loose(&r, "b.txt");
    let loose_file = loose(&r, &head);
    let mut victims = packs.clone();
    victims.push(loose_file.clone());
    let before = bytes(&victims);
    let w = g
        .before_shell(&argv(&["sh", "-c", "git gc --prune=now"]))
        .expect("snapshot")
        .expect("window");
    let (method, dir) = g.object_snapshot().expect("taken");
    assert_eq!(method, ObjectSnapshotMethod::HardLink);
    assert!(
        dir.starts_with(r.state.join(SNAPSHOT_DIR)),
        "{}",
        dir.display()
    );
    for v in &victims {
        std::fs::remove_file(v).expect("delete");
    }
    let err = g.after_shell(w).expect_err("detected");
    assert!(err.contains(".git/objects"), "{err}");
    assert!(err.contains("object files restored"), "{err}");
    assert_eq!(bytes(&victims), before);
    fsck_ok(&r);
    let logged = outcomes(&c);
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(logged[0].0.contains(".git/objects"), "{logged:?}");
    assert!(logged[0].1.starts_with("restored: "), "{logged:?}");
    let failure = g.finish().expect_err("the attempt fails");
    assert!(failure.contains("restored"), "{failure}");
    assert!(!dir.exists(), "the snapshot goes with the attempt");
    assert!(g.object_snapshot().is_none());
}

/// A command that repacks (loose objects gone, a new pack) passes: nothing
/// is missing for fsck, so nothing is restored and the attempt stands.
#[test]
fn a_repack_that_loses_no_object_passes() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    let (_packs, head) = packed_then_loose(&r, "b.txt");
    let w = g
        .before_shell(&argv(&["git", "repack", "-a", "-d"]))
        .expect("snapshot")
        .expect("window");
    git(&r.checkout, &["repack", "-a", "-d", "-q"]);
    git(&r.checkout, &["prune-packed"]);
    assert!(!loose(&r, &head).exists());
    g.after_shell(w).expect("nothing lost");
    assert!(!loose(&r, &head).exists(), "not put back");
    g.finish().expect("the attempt stands");
}

/// Objects written outside this attempt's window (another worker's commit
/// between its windows) and new objects the command itself wrote survive a
/// restore: objects are additive and a restore never removes a file.
#[test]
fn objects_written_by_others_and_new_objects_survive_a_restore() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    let (packs, _head) = packed_then_loose(&r, "b.txt");
    let w = g
        .before_shell(&argv(&["git", "status"]))
        .expect("snapshot")
        .expect("window");
    g.after_shell(w).expect("nothing changed");
    // Another worker's merge, between this attempt's windows.
    let (wt2, _admin2) = add_worktree(&r, "w2");
    std::fs::write(wt2.join("c.txt"), "c\n").expect("write");
    git(&wt2, &["add", "c.txt"]);
    git(&wt2, &["commit", "-q", "-m", "c"]);
    let theirs = git(&wt2, &["rev-parse", "HEAD"]);
    let w = g
        .before_shell(&argv(&["git", "status"]))
        .expect("snapshot")
        .expect("window");
    // The command writes a new object, then deletes the pack and the other
    // worker's commit.
    let mine = git(&r.checkout, &["hash-object", "-w", "--stdin"]);
    std::fs::remove_file(loose(&r, &theirs)).expect("delete theirs");
    for p in &packs {
        std::fs::remove_file(p).expect("delete pack");
    }
    g.after_shell(w).expect_err("detected");
    assert!(
        loose(&r, &theirs).exists(),
        "the other worker's commit is back"
    );
    assert!(
        loose(&r, &mine).exists(),
        "the command's new object is kept"
    );
    assert!(packs.iter().all(|p| p.exists()));
    fsck_ok(&r);
}

/// Where hard links are impossible (the state directory on another file
/// system), the snapshot copies, hashes each copy, and restores from it.
#[test]
fn the_copy_fallback_restores_and_verifies_by_hash() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass)).with_object_copies();
    let (packs, head) = packed_then_loose(&r, "b.txt");
    let mut victims = packs.clone();
    victims.push(loose(&r, &head));
    let before = bytes(&victims);
    let w = g
        .before_shell(&argv(&["git", "gc"]))
        .expect("snapshot")
        .expect("window");
    let (method, dir) = g.object_snapshot().expect("taken");
    assert_eq!(method, ObjectSnapshotMethod::Copy);
    assert!(dir.join("owner").is_file(), "{}", dir.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let rel = victims[0]
            .strip_prefix(r.common.join("objects"))
            .expect("rel");
        let a = std::fs::metadata(&victims[0]).expect("meta").ino();
        let b = std::fs::metadata(dir.join("objects").join(rel))
            .expect("meta")
            .ino();
        assert_ne!(a, b, "a copy, not a link");
    }
    for v in &victims {
        std::fs::remove_file(v).expect("delete");
    }
    g.after_shell(w).expect_err("detected");
    assert_eq!(bytes(&victims), before);
    fsck_ok(&r);
}

/// A copy that no longer matches its hash is not put back.
#[test]
fn a_copy_that_fails_its_hash_is_not_restored() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass)).with_object_copies();
    let (_packs, head) = packed_then_loose(&r, "b.txt");
    let w = g
        .before_shell(&argv(&["git", "gc"]))
        .expect("snapshot")
        .expect("window");
    let (_, dir) = g.object_snapshot().expect("taken");
    let file = loose(&r, &head);
    let rel = file.strip_prefix(r.common.join("objects")).expect("rel");
    let copy = dir.join("objects").join(rel);
    let mut perms = std::fs::metadata(&copy).expect("meta").permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&copy, perms).expect("chmod");
    std::fs::write(&copy, b"tampered").expect("tamper");
    std::fs::remove_file(&file).expect("delete");
    let err = g.after_shell(w).expect_err("detected");
    assert!(err.contains("no longer matches its hash"), "{err}");
    assert!(err.contains("still fails"), "{err}");
    assert!(!file.exists());
}

/// Under the hard-link method an object file altered in place shares its
/// change with the snapshot: reported as not restorable, never put back
/// as if it were good.
#[cfg(unix)]
#[test]
fn an_object_altered_in_place_is_reported_not_restorable() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    let (_packs, head) = packed_then_loose(&r, "b.txt");
    let w = g
        .before_shell(&argv(&["git", "status"]))
        .expect("snapshot")
        .expect("window");
    let file = loose(&r, &head);
    let mut perms = std::fs::metadata(&file).expect("meta").permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&file, perms).expect("chmod");
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&file)
        .and_then(|mut f| std::io::Write::write_all(&mut f, b"x"))
        .expect("in place");
    let err = g.after_shell(w).expect_err("detected");
    assert!(err.contains("altered in place"), "{err}");
}

/// A real other file system, where this machine has one at hand (Linux
/// `/dev/shm`): the hard link fails and the snapshot copies on its own.
#[test]
fn a_state_directory_on_another_file_system_falls_back_to_copies() {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let shm = Path::new("/dev/shm");
        let r = repo();
        let other = match tempfile::tempdir_in(shm) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("no /dev/shm here ({e}); the forced-copy test covers the path");
                return;
            }
        };
        let dev = |p: &Path| std::fs::metadata(p).expect("meta").dev();
        if dev(other.path()) == dev(&r.common) {
            eprintln!("/dev/shm is the repository's file system; nothing to cross");
            return;
        }
        let g = ProtectedWrites::from_parts(
            control(PermissionMode::Bypass),
            "w1",
            r.worktree.clone(),
            r.admin.clone(),
            r.common.clone(),
            other.path().to_path_buf(),
            json_check(),
        );
        let (_packs, head) = packed_then_loose(&r, "b.txt");
        let w = g
            .before_shell(&argv(&["git", "gc"]))
            .expect("snapshot")
            .expect("window");
        assert_eq!(
            g.object_snapshot().expect("taken").0,
            ObjectSnapshotMethod::Copy
        );
        std::fs::remove_file(loose(&r, &head)).expect("delete");
        g.after_shell(w).expect_err("detected");
        assert!(loose(&r, &head).exists());
    }
}

/// Over the copy bound no window opens: the command would run with the git
/// directory read-only. The lock is not kept and no snapshot is left.
#[test]
fn an_object_store_over_the_copy_bound_gets_no_window() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass))
        .with_object_copies()
        .with_object_copy_limit(16);
    let err = g
        .before_shell(&argv(&["git", "commit"]))
        .expect_err("over the bound");
    assert!(
        matches!(&err, WindowError::Unprotected(e) if e.contains("16-byte bound")),
        "{err}"
    );
    assert!(g.object_snapshot().is_none());
    let left: Vec<_> = std::fs::read_dir(r.state.join(SNAPSHOT_DIR))
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
    repo_lock::acquire(
        &r.common,
        repo_lock::Mode::Exclusive,
        "test",
        Duration::from_millis(200),
    )
    .expect("the lock was released");
}

// ---- serialization ----

/// Two workers' Bypass shell windows that each write a ref at the same time
/// run one after the other: their windows never overlap, each check names
/// only its own ref, and both writes land.
#[test]
fn two_workers_writing_refs_at_once_are_serialized() {
    let r = repo();
    let (wt2, admin2) = add_worktree(&r, "w2");
    let tree = git(&r.checkout, &["rev-parse", "HEAD^{tree}"]);
    let c1 = control(PermissionMode::Bypass);
    let c2 = control(PermissionMode::Bypass);
    let g1 = Arc::new(guard(&r, c1.clone()));
    let g2 = Arc::new(guard_for(&r, c2.clone(), "w2", wt2, admin2));
    let commits: Vec<String> = ["a", "b"]
        .iter()
        .map(|m| git(&r.checkout, &["commit-tree", &tree, "-m", m]))
        .collect();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let spans = Arc::new(Mutex::new(Vec::new()));
    let checkout = r.checkout.clone();
    let threads: Vec<_> = [(g1, "refs/heads/from-w1"), (g2, "refs/heads/from-w2")]
        .into_iter()
        .zip(commits.clone())
        .map(|((g, refname), oid)| {
            let barrier = barrier.clone();
            let spans = spans.clone();
            let checkout = checkout.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let w = g
                    .before_shell(&argv(&["git", "update-ref", refname, &oid]))
                    .expect("window")
                    .expect("bypass");
                let start = std::time::Instant::now();
                git(&checkout, &["update-ref", refname, &oid]);
                std::thread::sleep(Duration::from_millis(200));
                let end = std::time::Instant::now();
                g.after_shell(w).expect("a valid ref passes");
                spans.lock().expect("spans").push((start, end));
            })
        })
        .collect();
    for t in threads {
        t.join().expect("join");
    }
    let spans = spans.lock().expect("spans").clone();
    let (a, b) = (spans[0], spans[1]);
    assert!(a.1 <= b.0 || b.1 <= a.0, "windows overlapped: {spans:?}");
    assert_eq!(
        outcomes(&c1),
        vec![(".git/refs/heads/from-w1".into(), "kept".into())]
    );
    assert_eq!(
        outcomes(&c2),
        vec![(".git/refs/heads/from-w2".into(), "kept".into())]
    );
    assert_eq!(
        git(&r.checkout, &["rev-parse", "refs/heads/from-w1"]),
        commits[0]
    );
    assert_eq!(
        git(&r.checkout, &["rev-parse", "refs/heads/from-w2"]),
        commits[1]
    );
}

/// kGraf's own git (the runner merges and worktree changes use) waits while a
/// window is open, and a window waits while kGraf's git runs.
#[test]
fn kgraf_git_and_a_window_wait_for_each_other() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    let tree = git(&r.checkout, &["rev-parse", "HEAD^{tree}"]);
    let oid = git(&r.checkout, &["commit-tree", &tree, "-m", "k"]);
    let w = g
        .before_shell(&argv(&["git", "status"]))
        .expect("window")
        .expect("bypass");
    let checkout = r.checkout.clone();
    let merge = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        kgraf_sandbox::git::Git::discover()
            .expect("git")
            .run(&checkout, ["update-ref", "refs/heads/merged", oid.as_str()])
            .expect("update-ref");
        start.elapsed()
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&r.checkout)
            .args(["rev-parse", "--verify", "--quiet", "refs/heads/merged"])
            .env_remove("GIT_DIR")
            .output()
            .expect("git")
            .stdout
            .is_empty(),
        "the merge ran inside the window"
    );
    g.after_shell(w).expect("nothing changed");
    let waited = merge.join().expect("join");
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    git(&r.checkout, &["rev-parse", "--verify", "refs/heads/merged"]);

    // A merge in flight (a shared hold) makes the window wait.
    let held = repo_lock::acquire(
        &r.common,
        repo_lock::Mode::Shared,
        "merge-back",
        Duration::from_secs(1),
    )
    .expect("shared");
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(held);
    });
    let start = std::time::Instant::now();
    let w = g
        .before_shell(&argv(&["git", "status"]))
        .expect("window")
        .expect("bypass");
    assert!(start.elapsed() >= Duration::from_millis(250));
    g.after_shell(w).expect("nothing changed");
    releaser.join().expect("join");
}

/// A wait past the lock timeout fails loud, naming the holder: no write is
/// made and no window opens.
#[test]
fn a_lock_wait_past_the_timeout_fails_loud() {
    let r = repo();
    let g =
        guard(&r, control(PermissionMode::Bypass)).with_lock_timeout(Duration::from_millis(200));
    let _held = repo_lock::acquire(
        &r.common,
        repo_lock::Mode::Shared,
        "merge-back git update-ref",
        Duration::from_secs(1),
    )
    .expect("shared");
    let err = g.before_write(&Area::Git).expect_err("timed out");
    assert!(
        matches!(&err, WindowError::Busy(e) if e.contains("merge-back git update-ref")),
        "{err}"
    );
    let err = g
        .before_shell(&argv(&["git", "commit"]))
        .expect_err("timed out");
    assert!(matches!(err, WindowError::Busy(_)), "{err}");
    // A settings write takes no repository lock.
    let _w = g
        .before_write(&Area::Settings("run.json".into()))
        .expect("settings");
}

/// A snapshot a crashed process left (its owner gone) is removed when the
/// next one is taken; one whose owner runs is left alone.
#[cfg(unix)]
#[test]
fn a_snapshot_a_dead_process_left_is_swept() {
    let r = repo();
    let base = r.state.join(SNAPSHOT_DIR);
    let mut child = std::process::Command::new("true").spawn().expect("spawn");
    let dead = child.id();
    child.wait().expect("reaped");
    let host = kgraf_sandbox::HostId::current().fields();
    let abandoned = base.join("w9-dead-0");
    let live = base.join("w8-live-0");
    for (dir, pid) in [(&abandoned, dead), (&live, std::process::id())] {
        std::fs::create_dir_all(dir.join("objects/ab")).expect("mkdir");
        std::fs::write(dir.join("objects/ab/cd"), "x").expect("file");
        std::fs::write(dir.join("owner"), format!("pid={pid}:start=-:{host}\n")).expect("owner");
    }
    let g = guard(&r, control(PermissionMode::Bypass));
    let w = g
        .before_shell(&argv(&["git", "status"]))
        .expect("snapshot")
        .expect("window");
    g.after_shell(w).expect("nothing changed");
    assert!(!abandoned.exists(), "the dead owner's snapshot is gone");
    assert!(live.exists(), "a live owner's snapshot is kept");
}
