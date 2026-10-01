//! The protected-write guard against real repositories: each test makes a
//! repository with a linked worker worktree in a temporary directory.

use super::*;
use crate::permissions::{ModeSource, PermissionSettings, ProcessFacts};

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

#[test]
fn a_good_git_write_is_kept_and_logged() {
    let r = repo();
    let c = control(PermissionMode::Bypass);
    let g = guard(&r, c.clone());
    g.before_write(&Area::Git).expect("snapshot");
    let exclude = r.common.join("info/exclude");
    let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
    text.push_str("/scratch/\n");
    std::fs::write(&exclude, &text).expect("write");
    g.after_write(&Area::Git, ".git/info/exclude")
        .expect("passes");
    assert_eq!(std::fs::read_to_string(&exclude).expect("read"), text);
    assert_eq!(
        outcomes(&c),
        vec![(".git/info/exclude".into(), "kept".into())]
    );
    g.finish().expect("fsck passes");
    assert!(g.failure().is_none());
}

/// A problem the repository had before the attempt's first write is not
/// blamed on the write.
#[test]
fn a_problem_that_was_already_there_does_not_fail_a_good_write() {
    let r = repo();
    std::fs::write(r.common.join("refs/heads/broken"), "junk\n").expect("pre-existing");
    let g = guard(&r, control(PermissionMode::Bypass));
    g.before_write(&Area::Git).expect("snapshot");
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
    g.before_write(&Area::Git).expect("snapshot");
    std::fs::write(r.common.join("refs/heads/work"), "not a ref at all\n").expect("corrupt");
    let err = g
        .after_write(&Area::Git, ".git/refs/heads/work")
        .expect_err("detected");
    assert!(err.contains("refs/heads/work"), "{err}");
    assert_eq!(git(&r.checkout, &["rev-parse", "refs/heads/work"]), before);
    git(
        &r.checkout,
        &["fsck", "--connectivity-only", "--no-dangling"],
    );
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
    g.before_write(&Area::Git).expect("snapshot");
    std::fs::write(
        r.common.join("refs/heads/work"),
        format!("{}\n", "1".repeat(40)),
    )
    .expect("corrupt");
    let err = g
        .after_write(&Area::Git, ".git/refs/heads/work")
        .expect_err("detected");
    assert!(
        err.contains("does not exist") || err.contains("for-each-ref"),
        "{err}"
    );
    assert_eq!(
        std::fs::read(r.common.join("refs/heads/work")).expect("ref"),
        before
    );
}

/// The restore puts back the broken file only: a ref another worker's merge
/// moved meanwhile (valid) is kept.
#[test]
fn the_restore_keeps_a_valid_ref_moved_meanwhile() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Bypass));
    g.before_write(&Area::Git).expect("snapshot");
    // Another merge: `other` moves to a new commit.
    std::fs::write(r.checkout.join("b.txt"), "b\n").expect("write");
    git(&r.checkout, &["add", "b.txt"]);
    git(&r.checkout, &["commit", "-q", "-m", "two"]);
    let moved = git(&r.checkout, &["rev-parse", "HEAD"]);
    git(&r.checkout, &["update-ref", "refs/heads/other", &moved]);
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
    g.before_write(&Area::Git).expect("snapshot");
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

/// A shell command's git writes are found by comparing with the snapshot.
#[test]
fn a_shell_command_that_corrupts_a_ref_is_caught_after_it_runs() {
    let r = repo();
    let c = control(PermissionMode::Bypass);
    let g = guard(&r, c.clone());
    let root = g.before_shell().expect("snapshot").expect("bypass");
    assert_eq!(root, r.common);
    g.after_shell().expect("nothing changed");
    assert!(outcomes(&c).is_empty());
    std::fs::write(r.common.join("refs/heads/other"), "zzz\n").expect("corrupt");
    let err = g.after_shell().expect_err("detected");
    assert!(err.contains(".git/refs/heads/other"), "{err}");
    git(&r.checkout, &["rev-parse", "--verify", "refs/heads/other"]);
    assert!(g.finish().is_err());
}

#[test]
fn outside_bypass_the_shell_gets_no_git_root() {
    let r = repo();
    let g = guard(&r, control(PermissionMode::Auto));
    assert_eq!(g.before_shell().expect("ok"), None);
}

#[test]
fn a_settings_write_is_checked_and_restored_when_invalid() {
    let r = repo();
    let c = control(PermissionMode::Bypass);
    let g = guard(&r, c.clone());
    let run = r.state.join("run.json");
    std::fs::write(&run, "{\"routes\":[]}").expect("seed");
    let area = Area::Settings("run.json".into());
    g.before_write(&area).expect("snapshot");
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
    g.before_write(&area).expect("snapshot");
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
    assert!(err.contains("bound"), "{err}");
}
