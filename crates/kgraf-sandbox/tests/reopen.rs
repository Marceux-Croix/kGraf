//! Reopening a killed run's worktree by its recorded gitlink, and numbered
//! verifications, against the real git CLI. Every assertion reads
//! git's refs, the worktree listing or the filesystem.

mod common;

use std::sync::Arc;

use common::{dead_pid, fixture, git};
use kgraf_sandbox::{
    CommitOutcome, HostId, Identity, LOCK_TAG, MergeBack, MergeError, MergeOutcome,
    ProcessIdentity, ReopenSpec, Workspace, WorkspaceError, WorkspaceManager, WorkspaceSpec,
};

const BRANCH: &str = "integration";

fn identity() -> Identity {
    Identity {
        name: "kGraf Merge".into(),
        email: "merge@kgraf.invalid".into(),
    }
}

struct Env {
    f: common::Fixture,
    mgr: Arc<WorkspaceManager>,
}

fn env() -> Env {
    let f = fixture();
    git(&f.repo, &["branch", BRANCH, "main"]);
    let mgr = Arc::new(WorkspaceManager::open(&f.repo, &f.base).unwrap());
    Env { f, mgr }
}

fn create(e: &Env, name: &str) -> Workspace {
    e.mgr
        .create(&WorkspaceSpec {
            name,
            base: BRANCH,
            branch: None,
        })
        .unwrap()
}

/// What a killed process leaves: the lock names a PID that no longer runs,
/// in this boot and PID namespace.
fn kill_owner(e: &Env, ws: &Workspace) {
    let root = ws.root().to_str().unwrap();
    git(&e.f.repo, &["worktree", "unlock", root]);
    let reason = format!(
        "{LOCK_TAG}:pid={}:{}:name={}",
        dead_pid(),
        HostId::current().fields(),
        ws.name()
    );
    git(&e.f.repo, &["worktree", "lock", "--reason", &reason, root]);
}

fn spec(ws: &Workspace) -> ReopenSpec<'_> {
    ReopenSpec {
        name: ws.name(),
        gitlink: ws.gitlink(),
        base: ws.head(),
        integration: Some(BRANCH),
    }
}

fn lock_reason(e: &Env, ws: &Workspace) -> String {
    e.mgr
        .list()
        .unwrap()
        .into_iter()
        .find(|x| x.path == ws.root())
        .and_then(|x| x.locked)
        .expect("locked")
}

/// Whether a sandboxed command runs as PID 1 of its own PID namespace. Then
/// (Linux with namespaces) an orphan cannot outlive it: when the command
/// exits, the kernel kills everything left in the namespace. Elsewhere (macOS,
/// or Linux without namespaces) the child only `setsid`s, and an orphan runs
/// on after the command exits.
#[cfg(target_os = "linux")]
fn command_has_its_own_pid_namespace() -> bool {
    kgraf_sandbox::linux_confinement().namespaces
}

// Unix only: every caller is a Unix test (Windows has no orphans to outlive a
// command in this sense), so on Windows it would be dead code.
#[cfg(all(unix, not(target_os = "linux")))]
fn command_has_its_own_pid_namespace() -> bool {
    false
}

/// The shell for [`CLOSE_INHERITED`]. Not `/bin/sh`: dash (Debian's
/// `/bin/sh`) accepts only single-digit descriptors in a redirection, so there
/// `exec 10>&-` runs a command named `10`, fails, and ends the shell. Found on
/// Linux, where the dead shell's unreaped waiter still looked like a live
/// session, so the test passed for the wrong reason.
#[cfg(unix)]
const BASH: &str = "/bin/bash";

/// A [`BASH`] fragment that closes every descriptor above stderr, up to the
/// shell's limit: the hold's number depends on how many the test process had
/// open when it spawned (more under parallel tests), so none is assumed.
#[cfg(unix)]
const CLOSE_INHERITED: &str = "n=$(ulimit -n); case $n in unlimited|''|*[!0-9]*) n=10240;; esac; \
     [ $n -gt 10240 ] && n=10240; \
     i=3; while [ $i -lt $n ]; do eval \"exec $i>&-\" 2>/dev/null; i=$((i+1)); done";

/// The session ids the sandboxed children of `ws` logged.
#[cfg(target_os = "linux")]
fn logged_sessions(ws: &Workspace) -> Vec<u32> {
    std::fs::read_to_string(session_log_of(ws))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_whitespace().next()?.parse().ok())
        .collect()
}

/// Every running process on the system in session `sid`, read from `/proc`
/// directly (not through the code under test). A zombie is left out: it has
/// exited and holds nothing, only its parent has not reaped it (a container
/// whose PID 1 never reaps keeps a killed sandbox's PID 1 so).
#[cfg(target_os = "linux")]
fn session_members(sid: u32) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| {
            let pid: u32 = e.ok()?.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // After `(comm)`: state, ppid, pgrp, session.
            let mut rest = stat.get(stat.rfind(')')? + 1..)?.split_whitespace();
            let state = rest.next()?;
            let session: u32 = rest.nth(2)?.parse().ok()?;
            (session == sid && state != "Z" && state != "X").then_some(pid)
        })
        .collect()
}

/// Linux with PID namespaces, once the command has exited: nothing it started
/// is left. Checked independently of the code under test: the hold is free
/// (the orphan held it), and no process runs in a session the command logged.
#[cfg(target_os = "linux")]
fn assert_the_command_left_nothing(ws: &Workspace) {
    let f = std::fs::File::open(ws.hold_file()).unwrap();
    f.try_lock()
        .expect("the hold is free: the orphan that held it died with the command");
    f.unlock().unwrap();
    let sessions = logged_sessions(ws);
    assert!(!sessions.is_empty(), "the command logged its session");
    for sid in sessions {
        assert_eq!(session_members(sid), Vec::<u32>::new(), "session {sid}");
    }
}

#[test]
fn a_killed_worktree_is_reopened_with_its_changes_and_relocked() {
    let e = env();
    let ws = create(&e, "w1");
    std::fs::write(ws.root().join("work.txt"), "unfinished\n").unwrap();
    kill_owner(&e, &ws);

    // A new manager, as a restarted process opens one.
    let mgr = WorkspaceManager::open(&e.f.repo, &e.f.base).unwrap();
    let back = mgr.reopen(&spec(&ws)).unwrap();
    assert_eq!(back, ws, "same root, gitlink and base");
    assert_eq!(
        std::fs::read_to_string(back.root().join("work.txt")).unwrap(),
        "unfinished\n",
        "the worker's change survived"
    );
    let reason = lock_reason(&e, &ws);
    assert!(
        reason.starts_with(&format!("{LOCK_TAG}:pid={}:", std::process::id())),
        "{reason}"
    );
    // And it merges back like a fresh attempt.
    let mb = MergeBack::open(e.mgr.clone(), BRANCH, identity()).unwrap();
    let mut a = mb.attempt_numbered(back, 1).unwrap();
    assert!(matches!(
        mb.commit_declared(&mut a, "w1", &["work.txt"]).unwrap(),
        CommitOutcome::Committed { .. }
    ));
    assert!(matches!(
        mb.merge(&mut a).unwrap(),
        MergeOutcome::Merged { .. }
    ));
    assert_eq!(
        git(&e.f.repo, &["show", &format!("{BRANCH}:work.txt")]),
        "unfinished"
    );
}

/// A lock recorded on another machine (or namespace) is
/// not judged dead from here, so the worktree is not reopened.
#[test]
fn an_owner_on_another_machine_keeps_its_worktree() {
    let e = env();
    let ws = create(&e, "w1");
    let root = ws.root().to_str().unwrap();
    git(&e.f.repo, &["worktree", "unlock", root]);
    // Another machine runs another kernel: its boot id differs too. (An equal
    // boot id alone proves the same PID table, machine id or not.)
    let elsewhere = HostId {
        machine: Some("another-machine".into()),
        boot: Some("another-machines-boot".into()),
        ..HostId::current()
    };
    let reason = format!(
        "{LOCK_TAG}:pid={}:{}:name=w1",
        dead_pid(),
        elsewhere.fields()
    );
    git(&e.f.repo, &["worktree", "lock", "--reason", &reason, root]);
    let err = e.mgr.reopen(&spec(&ws)).unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::NotReopenable { reason, .. } if reason.contains("another machine")),
        "{err}"
    );
}

#[test]
fn a_tampered_gitlink_is_not_reopened() {
    let e = env();
    let ws = create(&e, "w1");
    kill_owner(&e, &ws);
    std::fs::write(ws.root().join(".git"), "gitdir: /tmp/elsewhere\n").unwrap();
    let err = e.mgr.reopen(&spec(&ws)).unwrap_err();
    assert!(
        matches!(err, WorkspaceError::GitlinkTampered { .. }),
        "{err}"
    );
}

#[test]
fn a_live_owner_keeps_its_worktree() {
    let e = env();
    let ws = create(&e, "w1");
    let root = ws.root().to_str().unwrap();
    let mut other = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    git(&e.f.repo, &["worktree", "unlock", root]);
    let reason = format!("{LOCK_TAG}:pid={}:name=w1", other.id());
    git(&e.f.repo, &["worktree", "lock", "--reason", &reason, root]);
    let err = e.mgr.reopen(&spec(&ws)).unwrap_err();
    let _ = other.kill();
    let _ = other.wait();
    assert!(
        matches!(&err, WorkspaceError::Busy { reason, .. } if reason.contains("live")),
        "{err}"
    );
    assert_eq!(lock_reason(&e, &ws), reason, "left as it was");
}

#[test]
fn a_worker_commit_is_not_intact() {
    let e = env();
    let ws = create(&e, "w1");
    std::fs::write(ws.root().join("x"), "x").unwrap();
    git(ws.root(), &["add", "x"]);
    git(
        ws.root(),
        &[
            "-c",
            "user.name=w",
            "-c",
            "user.email=w@w",
            "commit",
            "-q",
            "-m",
            "mine",
        ],
    );
    kill_owner(&e, &ws);
    let err = e.mgr.reopen(&spec(&ws)).unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::NotReopenable { reason, .. } if reason.contains("HEAD")),
        "{err}"
    );
}

/// The collision this numbering exists for: a verification was committed
/// (HEAD moved, attempt ref written) and the process died before the merge.
/// The resumed run reopens the tree and verifies it again. Unnumbered, or with
/// the same number, the new commit collides with the old ref; numbered per
/// verification, it does not.
#[test]
fn a_reverified_worktree_never_collides_with_its_earlier_attempt_ref() {
    let e = env();
    let mb = MergeBack::open(e.mgr.clone(), BRANCH, identity()).unwrap();
    let ws = create(&e, "w1");
    std::fs::write(ws.root().join("a.txt"), "first\n").unwrap();
    let mut first = mb.attempt_numbered(ws.clone(), 1).unwrap();
    let CommitOutcome::Committed { commit: c1 } = mb.commit(&mut first, "v1").unwrap() else {
        panic!("not committed");
    };
    assert_eq!(first.attempt_ref(), "refs/kgraf/attempts/w1+1");
    kill_owner(&e, &ws);

    let back = e.mgr.reopen(&spec(&ws)).unwrap();
    assert_eq!(back.head(), ws.head(), "measured against the recorded base");
    assert_eq!(
        git(back.root(), &["rev-parse", "HEAD"]),
        ws.head(),
        "HEAD moved back from the unmerged verification"
    );
    // The rolled-back commit's attempt ref is removed.
    assert!(
        !ref_exists(&e.f.repo, "refs/kgraf/attempts/w1+1"),
        "the unmerged commit's ref goes with it"
    );
    std::fs::write(back.root().join("a.txt"), "second\n").unwrap();

    // A kill between the rollback and that removal leaves the ref behind;
    // the same number then collides with it.
    git(&e.f.repo, &["update-ref", "refs/kgraf/attempts/w1+1", &c1]);
    let mut same = mb.attempt_numbered(back.clone(), 1).unwrap();
    let err = mb.commit(&mut same, "v1 again").unwrap_err();
    assert!(matches!(err, MergeError::AttemptRefExists(_)), "{err}");
    assert_eq!(
        git(back.root(), &["rev-parse", "HEAD"]),
        ws.head(),
        "atomic"
    );

    // The next number does not.
    let mut next = mb.attempt_numbered(back, 2).unwrap();
    let CommitOutcome::Committed { commit: c2 } =
        mb.commit_declared(&mut next, "v2", &["a.txt"]).unwrap()
    else {
        panic!("not committed");
    };
    assert_ne!(c1, c2);
    assert_eq!(
        mb.changed_paths(&next).unwrap(),
        vec!["a.txt".to_owned()],
        "scope is measured from the recorded base"
    );
    assert!(matches!(
        mb.merge(&mut next).unwrap(),
        MergeOutcome::Merged { .. }
    ));
    assert_eq!(
        git(&e.f.repo, &["show", &format!("{BRANCH}:a.txt")]),
        "second"
    );
}

fn ref_exists(repo: &std::path::Path, name: &str) -> bool {
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

/// Killed after the merge and before the worktree was
/// retired. Rolling HEAD back would merge the same change again; reopen
/// retires the worktree and its ref instead, and says so.
#[test]
fn a_worktree_whose_commit_is_already_merged_is_retired_not_reopened() {
    let e = env();
    let mb = MergeBack::open(e.mgr.clone(), BRANCH, identity()).unwrap();
    let ws = create(&e, "w1");
    std::fs::write(ws.root().join("a.txt"), "done\n").unwrap();
    let mut a = mb.attempt_numbered(ws.clone(), 1).unwrap();
    let CommitOutcome::Committed { commit } = mb.commit(&mut a, "v1").unwrap() else {
        panic!("not committed");
    };
    // The merge landed (a fast-forward here); the retire did not.
    git(
        &e.f.repo,
        &["update-ref", &format!("refs/heads/{BRANCH}"), &commit],
    );
    kill_owner(&e, &ws);

    let err = e.mgr.reopen(&spec(&ws)).unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::AlreadyMerged { commit: c, .. } if *c == commit),
        "{err}"
    );
    assert!(!ws.root().exists(), "retired");
    assert!(!ref_exists(&e.f.repo, "refs/kgraf/attempts/w1+1"));
    assert_eq!(
        git(&e.f.repo, &["rev-parse", BRANCH]),
        commit,
        "merged once"
    );
}

#[test]
fn process_identity_is_pid_reuse_safe() {
    let me = ProcessIdentity::current();
    assert!(me.is_alive());
    let here = HostId::current();
    // The boot id is readable on Linux, macOS and Windows; the machine id is
    // not on a slim Linux container (no /etc/machine-id), and need not be.
    // Windows records its boot (a counter) only with its machine id.
    #[cfg(any(unix, windows))]
    assert!(here.boot.is_some(), "the boot is readable here: {here:?}");
    #[cfg(windows)]
    assert!(here.machine.is_some(), "MachineGuid is readable: {here:?}");
    let host = Some(here.clone());
    assert!(
        !ProcessIdentity {
            pid: dead_pid(),
            start: None,
            host: host.clone(),
        }
        .is_alive()
    );
    if let Some(start) = me.start {
        assert!(
            !ProcessIdentity {
                pid: me.pid,
                start: Some(start + 1),
                host: host.clone(),
            }
            .is_alive(),
            "same PID, another start time: a reused PID"
        );
    }
    // Another machine (another kernel, so another boot too) or namespace, or
    // an unreadable host: never judged dead.
    for other in [
        HostId {
            machine: Some("another-machine".into()),
            boot: Some("another-machines-boot".into()),
            ..here.clone()
        },
        HostId {
            ns: Some("1".into()),
            ..here.clone()
        },
        HostId::default(),
    ] {
        assert!(
            ProcessIdentity {
                pid: dead_pid(),
                start: None,
                host: Some(other),
            }
            .is_alive()
        );
    }
    // The machine id unknown, the boot and namespace equal: the same PID
    // table, so judged by PID and start time. This is every lock on a Linux
    // host without a machine id; before, all of them were unknowable, even
    // this process's own.
    let unknown_machine = HostId {
        machine: None,
        ..here.clone()
    };
    assert!(
        ProcessIdentity {
            pid: me.pid,
            start: me.start,
            host: Some(unknown_machine.clone()),
        }
        .is_alive(),
        "this very process"
    );
    assert!(
        !ProcessIdentity {
            pid: dead_pid(),
            start: None,
            host: Some(unknown_machine.clone()),
        }
        .is_alive(),
        "a dead PID of this boot and namespace"
    );
    if let Some(start) = me.start {
        assert!(
            !ProcessIdentity {
                pid: me.pid,
                start: Some(start + 1),
                host: Some(unknown_machine.clone()),
            }
            .is_alive(),
            "a reused PID of this boot and namespace"
        );
    }
    // The boot unknown: unknowable, so never judged dead, machine id or not.
    for machine in [None, here.machine.clone()] {
        assert!(
            ProcessIdentity {
                pid: dead_pid(),
                start: None,
                host: Some(HostId {
                    machine,
                    boot: None,
                    ..here.clone()
                }),
            }
            .is_alive()
        );
    }
    // Another boot. With the same known machine and namespace: a reboot, so
    // dead, even for our PID. With the machine unknown: a reboot here and
    // another machine look the same, so unknowable (never dead).
    let previous_boot = ProcessIdentity {
        pid: me.pid,
        start: me.start,
        host: Some(HostId {
            boot: Some("a-previous-boot".into()),
            ..here.clone()
        }),
    };
    assert_eq!(!previous_boot.is_alive(), here.machine.is_some());
    assert!(
        ProcessIdentity {
            pid: dead_pid(),
            start: None,
            host: Some(HostId {
                machine: None,
                boot: Some("a-previous-boot".into()),
                ..here.clone()
            }),
        }
        .is_alive()
    );
    // No host recorded (a lock from before host ids): PID and start time.
    assert!(
        !ProcessIdentity {
            pid: dead_pid(),
            start: None,
            host: None,
        }
        .is_alive()
    );
    assert!(me.is_alive());
}

/// A sandboxed child only `setsid`s, so on macOS it
/// outlives a killed orchestrator. Every child now holds a shared lock on the
/// worktree's hold file (inherited by its descendants), and reopen refuses
/// while any holder runs. The orphan here is real: a sandboxed shell that
/// starts a background `sleep` and exits, so the sleep is reparented and
/// nobody waits for it.
///
/// Linux with namespaces: the shell is PID 1 of its PID namespace, so the
/// kernel kills the sleep when the shell exits. No orphan is left, and reopen
/// proceeds at once. (The PID the shell writes is its namespace's, not ours,
/// so it is never used there.) The live case on Linux is
/// `a_live_command_keeps_its_orphan_and_the_hold_on_linux`.
#[cfg(unix)]
#[test]
fn a_worktree_with_a_live_orphan_is_not_reopened_until_it_exits() {
    let e = env();
    let ws = create(&e, "w1");
    let pidfile = ws.scratch().join("orphan.pid");
    let mut cmd =
        kgraf_sandbox::workspace_command(&ws, kgraf_sandbox::Network::Deny, "/bin/sh").unwrap();
    cmd.args([
        "-c",
        &format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > '{}'",
            pidfile.display()
        ),
    ]);
    assert!(cmd.status().unwrap().success(), "the launcher exited");
    kill_owner(&e, &ws);

    if command_has_its_own_pid_namespace() {
        #[cfg(target_os = "linux")]
        assert_the_command_left_nothing(&ws);
        assert_eq!(
            e.mgr.reopen(&spec(&ws)).unwrap(),
            ws,
            "the orphan died with the command"
        );
        return;
    }

    let err = e.mgr.reopen(&spec(&ws)).unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::Busy { reason, .. } if reason.contains("still running")),
        "{err}"
    );

    // End the orphan.
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    let killed = std::process::Command::new("kill")
        .args(["-9", pid.trim()])
        .status()
        .unwrap();
    assert!(killed.success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match e.mgr.reopen(&spec(&ws)) {
            Ok(back) => {
                assert_eq!(back, ws);
                break;
            }
            Err(WorkspaceError::Busy { reason, .. }) if reason.contains("still running") => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the hold was never released"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(other) => panic!("{other}"),
        }
    }
}

/// A lock recorded on this machine and namespace before a reboot
/// counted as alive forever, so the worktree was never cleaned or reopened.
/// The same machine and namespace with another boot is now dead: startup
/// cleans an unreferenced worktree, and reopen continues a referenced one.
#[test]
fn after_a_reboot_a_worktree_is_cleaned_or_reopened() {
    let e = env();
    let previous = HostId {
        boot: Some("previous-boot-uuid".into()),
        ..HostId::current()
    };
    let relock = |ws: &Workspace| {
        let root = ws.root().to_str().unwrap();
        git(&e.f.repo, &["worktree", "unlock", root]);
        // Even this process's PID: in the previous boot it was another process.
        let reason = format!(
            "{LOCK_TAG}:pid={}:{}:name={}",
            std::process::id(),
            previous.fields(),
            ws.name()
        );
        git(&e.f.repo, &["worktree", "lock", "--reason", &reason, root]);
    };
    let cleaned = create(&e, "w1");
    let kept = create(&e, "w2");
    relock(&cleaned);
    relock(&kept);
    let removed = e.mgr.startup_keeping(&|n| n == "w2").unwrap();
    if HostId::current().machine.is_none() {
        // No machine id here (a slim Linux container): a reboot of this
        // machine cannot be told from another machine, so both are stranded,
        // listed, and left for `clean`.
        assert!(
            removed.iter().all(|r| r.path != cleaned.root()),
            "{removed:?}"
        );
        assert!(cleaned.root().exists());
        assert_eq!(
            e.mgr.stranded().unwrap(),
            vec![cleaned.root().to_path_buf(), kept.root().to_path_buf()]
        );
        let err = e.mgr.reopen(&spec(&kept)).unwrap_err();
        assert!(
            matches!(&err, WorkspaceError::NotReopenable { reason, .. } if reason.contains("another boot")),
            "{err}"
        );
        return;
    }
    assert!(
        removed.iter().any(|r| r.path == cleaned.root()),
        "{removed:?}"
    );
    assert!(!cleaned.root().exists());
    assert_eq!(e.mgr.reopen(&spec(&kept)).unwrap(), kept);

    // Another machine's lock is still never touched.
    let foreign = create(&e, "w3");
    let root = foreign.root().to_str().unwrap();
    git(&e.f.repo, &["worktree", "unlock", root]);
    let elsewhere = HostId {
        machine: Some("another-machine".into()),
        ..previous.clone()
    };
    let reason = format!(
        "{LOCK_TAG}:pid={}:{}:name=w3",
        dead_pid(),
        elsewhere.fields()
    );
    git(&e.f.repo, &["worktree", "lock", "--reason", &reason, root]);
    e.mgr.startup().unwrap();
    assert!(foreign.root().exists());
}

/// Python's `subprocess` closes inherited descriptors by default, so
/// its child dropped the flock hold and reopen returned Ok with it alive. The
/// sandboxed child now logs its session; reopen refuses while any process of
/// a logged session runs, then succeeds once it has exited.
///
/// Linux with namespaces: the grandchild dies with the shell (PID 1 of its
/// PID namespace), so nothing is left and reopen proceeds at once.
#[cfg(unix)]
#[test]
fn a_grandchild_that_closed_its_descriptors_still_blocks_reopen() {
    let python = [
        "/usr/bin/python3",
        "/usr/local/bin/python3",
        "/opt/homebrew/bin/python3",
    ]
    .into_iter()
    .find(|p| std::path::Path::new(p).is_file());
    let Some(python) = python else {
        eprintln!("no python3: the descriptor-closing grandchild cannot be made here");
        return;
    };
    let e = env();
    let ws = create(&e, "w1");
    let pidfile = ws.scratch().join("orphan.pid");
    let mut cmd =
        kgraf_sandbox::workspace_command(&ws, kgraf_sandbox::Network::Deny, "/bin/sh").unwrap();
    cmd.args([
        "-c",
        &format!(
            "{python} -c \"import subprocess; p = subprocess.Popen(['/bin/sleep','30'], \
             stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); \
             open('{}','w').write(str(p.pid))\"",
            pidfile.display()
        ),
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    if command_has_its_own_pid_namespace() {
        assert!(pidfile.is_file(), "the grandchild was started");
        kill_owner(&e, &ws);
        #[cfg(target_os = "linux")]
        assert_the_command_left_nothing(&ws);
        assert_eq!(
            e.mgr.reopen(&spec(&ws)).unwrap(),
            ws,
            "the grandchild died with the command"
        );
        return;
    }
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    let alive = |pid: &str| {
        std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .unwrap()
            .success()
    };
    assert!(alive(&pid), "the grandchild runs");
    kill_owner(&e, &ws);

    let res = e.mgr.reopen(&spec(&ws));
    assert!(
        matches!(&res, Err(WorkspaceError::Busy { reason, .. }) if reason.contains("session")),
        "{res:?}"
    );

    let _ = std::process::Command::new("kill")
        .args(["-9", pid.trim()])
        .status();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match e.mgr.reopen(&spec(&ws)) {
            Ok(back) => {
                assert_eq!(back, ws);
                break;
            }
            Err(WorkspaceError::Busy { reason, .. }) if reason.contains("running") => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "never released: {reason}"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(other) => panic!("{other}"),
        }
    }
}

// Unix only, like every test that reads the session log.
#[cfg(unix)]
fn session_log_of(ws: &Workspace) -> std::path::PathBuf {
    let mut log = ws.hold_file().as_os_str().to_owned();
    log.push("+sessions");
    std::fs::create_dir_all(ws.hold_file().parent().unwrap()).unwrap();
    std::path::PathBuf::from(log)
}

/// The log held bare session ids, so an unrelated live session leader
/// that reused one blocked reopen. Each id is now logged with its leader's
/// start time and the boot: a live leader with another start time is not
/// ours, a log from another boot is ignored, and dead ids are pruned.
#[cfg(unix)]
#[test]
fn a_recycled_session_id_does_not_block_reopen() {
    let python = [
        "/usr/bin/python3",
        "/usr/local/bin/python3",
        "/opt/homebrew/bin/python3",
    ]
    .into_iter()
    .find(|p| std::path::Path::new(p).is_file());
    let Some(python) = python else {
        eprintln!("no python3: an unrelated session leader cannot be made here");
        return;
    };
    let e = env();
    let ws = create(&e, "w1");
    // An unrelated long-lived process that is its own session leader.
    let mut other = std::process::Command::new(python)
        .args(["-c", "import os,time; os.setsid(); time.sleep(30)"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let boot = HostId::current().boot.unwrap();
    let log = session_log_of(&ws);
    // Its PID, logged as if it were a session of ours that started at another
    // time; the same id from a previous boot; and the bare id the earlier log
    // format held, which nothing can check (and which blocked reopen).
    std::fs::write(
        &log,
        format!(
            "{} 1 {boot}\n{} 1 previous-boot\n{}\n",
            other.id(),
            other.id(),
            other.id()
        ),
    )
    .unwrap();
    kill_owner(&e, &ws);
    let r = e.mgr.reopen(&spec(&ws));
    let _ = other.kill();
    let _ = other.wait();
    assert_eq!(r.unwrap(), ws, "a recycled id is not ours");
    assert!(!log.exists(), "the stale ids were pruned");
}

/// A genuine session is still recognized by its logged
/// start time; the log is pruned to it.
#[cfg(unix)]
#[test]
fn a_live_logged_session_blocks_reopen_and_the_log_is_pruned_to_it() {
    let e = env();
    let ws = create(&e, "w1");
    // A process that drops its inherited descriptors (so the flock hold is
    // gone) and stays in the session: only the session log can see it.
    let mut cmd =
        kgraf_sandbox::workspace_command(&ws, kgraf_sandbox::Network::Deny, BASH).unwrap();
    cmd.args(["-c", &format!("{CLOSE_INHERITED}; exec /bin/sleep 30")]);
    // Push the hold's descriptor number up, as parallel tests do: the shell
    // must find it wherever it lands.
    let _padding: Vec<std::fs::File> = (0..64)
        .map(|_| std::fs::File::open("/dev/null").unwrap())
        .collect();
    let mut child = cmd.spawn().unwrap();
    // Wait (bounded) until the shell has dropped its descriptors, so the hold
    // is free and only the session log can see the process.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let f = std::fs::File::open(ws.hold_file()).unwrap();
        match f.try_lock() {
            Ok(()) => {
                f.unlock().unwrap();
                break;
            }
            Err(_) => {
                assert!(std::time::Instant::now() < deadline, "the hold never freed");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    let log = session_log_of(&ws);
    let logged = std::fs::read_to_string(&log).unwrap();
    let boot = HostId::current().boot.unwrap();
    let mine = logged.lines().next().unwrap().to_owned();
    // `<sid> <start> <boot> <pid ns> <time ns>`, as this process sees them.
    let here = HostId::current();
    let dash = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    let (ns, time_ns) = (dash(here.ns), dash(here.time_ns));
    let fields: Vec<&str> = mine.split_whitespace().collect();
    assert_eq!(
        fields.get(2..),
        Some(&[boot.as_str(), ns.as_str(), time_ns.as_str()][..]),
        "{mine}"
    );
    // Noise: ids of this boot that no process uses.
    std::fs::write(&log, format!("{logged}999999 5 {boot}\n")).unwrap();
    kill_owner(&e, &ws);
    // The command really runs (an exited, unreaped child would still sit in
    // its session and pass the check below for the wrong reason).
    assert!(
        child.try_wait().unwrap().is_none(),
        "the command still runs"
    );
    let r = e.mgr.reopen(&spec(&ws));
    assert!(
        matches!(&r, Err(WorkspaceError::Busy { reason, .. }) if reason.contains("session")),
        "{r:?}"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "the command still runs"
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap(), format!("{mine}\n"));
    let _ = child.kill();
    let _ = child.wait();
}

/// A large log costs one process-table scan, and is pruned by it.
#[cfg(unix)]
#[test]
fn a_large_session_log_is_scanned_once_and_pruned() {
    let e = env();
    let ws = create(&e, "w1");
    let boot = HostId::current().boot.unwrap();
    let log = session_log_of(&ws);
    let text: String = (0..100_000)
        .map(|i| format!("{} 1 {boot}\n", 3_000_000 + i))
        .collect();
    std::fs::write(&log, text).unwrap();
    kill_owner(&e, &ws);
    let t = std::time::Instant::now();
    assert_eq!(e.mgr.reopen(&spec(&ws)).unwrap(), ws);
    let took = t.elapsed();
    assert!(took < std::time::Duration::from_secs(5), "{took:?}");
    assert!(!log.exists());
}

/// The machine id is the hardware UUID (IOPlatformUUID),
/// which survives OS updates, not `kern.uuid` (the kernel build's UUID).
#[cfg(target_os = "macos")]
#[test]
fn the_machine_id_is_the_hardware_uuid() {
    let out = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let platform = text
        .lines()
        .find(|l| l.contains("IOPlatformUUID"))
        .and_then(|l| l.split('"').nth(3))
        .unwrap()
        .to_owned();
    assert_eq!(
        HostId::current().machine.as_deref(),
        Some(platform.as_str())
    );
    let kernel = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.uuid"])
        .output()
        .unwrap();
    assert_ne!(
        HostId::current().machine.as_deref(),
        Some(String::from_utf8_lossy(&kernel.stdout).trim()),
        "not the kernel build UUID"
    );
}

/// A worktree whose owner cannot be judged from here is
/// listed as stranded (never silent) and removed only by `clean`, which
/// refuses a live owner.
#[test]
fn a_stranded_worktree_is_listed_and_cleaned_on_request() {
    let e = env();
    let stranded = create(&e, "w1");
    let live = create(&e, "w2");
    let root = stranded.root().to_str().unwrap();
    git(&e.f.repo, &["worktree", "unlock", root]);
    let elsewhere = HostId {
        ns: Some("4026531999".into()),
        ..HostId::current()
    };
    let reason = format!(
        "{LOCK_TAG}:pid={}:{}:name=w1",
        dead_pid(),
        elsewhere.fields()
    );
    git(&e.f.repo, &["worktree", "lock", "--reason", &reason, root]);
    e.mgr.startup().unwrap();
    assert!(stranded.root().exists(), "never removed on its own");
    assert_eq!(
        e.mgr.stranded().unwrap(),
        vec![stranded.root().to_path_buf()]
    );
    assert!(
        e.mgr.clean("w2").is_err(),
        "a live owner keeps its worktree"
    );
    assert!(live.root().exists());
    assert_eq!(e.mgr.clean("w1").unwrap(), stranded.root());
    assert!(!stranded.root().exists());
    assert!(e.mgr.stranded().unwrap().is_empty());
}

/// Startup cleanup
/// checked only the owner, so it removed a worktree an orphan still wrote in.
/// It now takes the hold and checks the sessions first, and leaves the
/// worktree until the orphan has ended.
///
/// Linux with namespaces: the orphan dies with the shell (PID 1 of its PID
/// namespace), so startup removes the dead owner's worktree at once.
#[cfg(unix)]
#[test]
fn startup_leaves_a_dead_owners_worktree_while_an_orphan_runs_in_it() {
    let e = env();
    let ws = create(&e, "w1");
    let mut cmd =
        kgraf_sandbox::workspace_command(&ws, kgraf_sandbox::Network::Deny, "/bin/sh").unwrap();
    let pidfile = ws.scratch().join("o.pid");
    cmd.args([
        "-c",
        &format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > '{}'",
            pidfile.display()
        ),
    ]);
    assert!(cmd.status().unwrap().success());
    if command_has_its_own_pid_namespace() {
        kill_owner(&e, &ws);
        #[cfg(target_os = "linux")]
        assert_the_command_left_nothing(&ws);
        let removed = e.mgr.startup().unwrap();
        assert!(
            removed.iter().any(|r| r.path == ws.root()),
            "the orphan died with the command: {removed:?}"
        );
        assert!(!ws.root().exists());
        return;
    }
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    kill_owner(&e, &ws);
    let removed = e.mgr.startup().unwrap();
    assert!(removed.iter().all(|r| r.path != ws.root()), "{removed:?}");
    assert!(ws.root().exists(), "kept while the orphan runs");
    let _ = std::process::Command::new("kill")
        .args(["-9", pid.trim()])
        .status();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let removed = e.mgr.startup().unwrap();
        if removed.iter().any(|r| r.path == ws.root()) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "never cleaned");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!ws.root().exists());
}

/// Linux with namespaces: an orphan lives only as long as the command, so
/// here the hold and session checks are exercised with a command that stays
/// running. PID 1 (the shell) starts a background `sleep`, closes its own
/// inherited descriptors (the hold among them) and waits. The hold is then
/// kept only by the orphan, which inherited it across exec: that proves the
/// child hook clears close-on-exec on the hold after the Linux step marks
/// every inherited descriptor close-on-exec. Reopen and startup refuse on the
/// hold. Once the orphan is killed (from inside: its PID is the namespace's),
/// reopen refuses on the session, where PID 1 still runs. Once the command is
/// killed, reopen proceeds.
#[cfg(target_os = "linux")]
#[test]
fn a_live_command_keeps_its_orphan_and_the_hold_on_linux() {
    if !command_has_its_own_pid_namespace() {
        eprintln!("no PID namespaces: the orphan tests above cover this host");
        return;
    }
    let e = env();
    let ws = create(&e, "w1");
    let ready = ws.scratch().join("ready");
    let go = ws.scratch().join("go");
    let killed = ws.scratch().join("killed");
    let mut cmd =
        kgraf_sandbox::workspace_command(&ws, kgraf_sandbox::Network::Deny, BASH).unwrap();
    cmd.args([
        "-c",
        &format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & o=$!; {CLOSE_INHERITED}; \
             : > '{}'; while [ ! -e '{}' ]; do sleep 0.05; done; \
             kill -9 $o; wait $o; : > '{}'; exec sleep 30",
            ready.display(),
            go.display(),
            killed.display()
        ),
    ]);
    // Push the hold's descriptor number up, as parallel tests do.
    let _padding: Vec<std::fs::File> = (0..64)
        .map(|_| std::fs::File::open("/dev/null").unwrap())
        .collect();
    let mut child = cmd.spawn().unwrap();
    let wait_for = |path: &std::path::Path| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    wait_for(&ready);

    // The orphan alone keeps the hold; the shell and the orphan run in the
    // logged session (with the waiter that leads it).
    let f = std::fs::File::open(ws.hold_file()).unwrap();
    assert!(
        matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
        "the orphan keeps the hold across exec"
    );
    drop(f);
    let sessions = logged_sessions(&ws);
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let members = session_members(sessions[0]);
    assert!(members.len() >= 3, "waiter, shell and orphan: {members:?}");

    kill_owner(&e, &ws);
    let r = e.mgr.reopen(&spec(&ws));
    assert!(
        matches!(&r, Err(WorkspaceError::Busy { reason, .. }) if reason.contains("hold is taken")),
        "{r:?}"
    );
    let removed = e.mgr.startup().unwrap();
    assert!(removed.iter().all(|r| r.path != ws.root()), "{removed:?}");
    assert!(ws.root().exists(), "kept while the orphan runs");

    // End the orphan. The hold is free, but PID 1 still runs in the session.
    std::fs::write(&go, "").unwrap();
    wait_for(&killed);
    let r = e.mgr.reopen(&spec(&ws));
    assert!(
        matches!(&r, Err(WorkspaceError::Busy { reason, .. }) if reason.contains("session")),
        "{r:?}"
    );

    // End the command: the namespace goes with it, and reopen proceeds.
    child.kill().unwrap();
    child.wait().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match e.mgr.reopen(&spec(&ws)) {
            Ok(back) => {
                assert_eq!(back, ws);
                break;
            }
            Err(WorkspaceError::Busy { reason, .. }) if reason.contains("running") => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "never released: {reason}"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(other) => panic!("{other}"),
        }
    }
    assert_eq!(session_members(sessions[0]), Vec::<u32>::new());
}

/// A C program whose main thread leaves its own session-led process through
/// `pthread_exit` while a worker thread keeps running (and writes `argv[1]`
/// once it runs alone).
#[cfg(target_os = "linux")]
const LEADER_EXITS_C: &str = r#"
#include <pthread.h>
#include <stdio.h>
#include <unistd.h>

static const char *ready;

static void *worker(void *arg) {
    (void)arg;
    FILE *f = fopen(ready, "w");
    if (f) fclose(f);
    for (;;) pause();
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    ready = argv[1];
    if (setsid() < 0) return 3;
    pthread_t t;
    if (pthread_create(&t, 0, worker, 0) != 0) return 4;
    pthread_exit(0);
}
"#;

/// The state letter of `/proc/<pid>/stat` or `/proc/<pid>/task/<tid>/stat`.
#[cfg(target_os = "linux")]
fn proc_state(stat: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(stat).ok()?;
    let rest = text.get(text.rfind(')')? + 1..)?;
    Some(rest.split_whitespace().next()?.to_owned())
}

/// A process whose main thread called `pthread_exit` while
/// a worker thread runs shows `Z` in `/proc/<pid>/stat` (the leader's state),
/// but it lives: it holds its descriptors and can write. Judged by that line
/// alone it counted as a zombie, so its session no longer blocked reopen and,
/// as a lock owner, it was judged dead. Both must still block.
#[cfg(target_os = "linux")]
#[test]
fn a_process_whose_leader_thread_exited_still_blocks_reopen() {
    let e = env();
    let ws = create(&e, "w1");
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("leader_exits.c");
    let bin = dir.path().join("leader_exits");
    let ready = dir.path().join("ready");
    std::fs::write(&src, LEADER_EXITS_C).unwrap();
    let built = std::process::Command::new("cc")
        .arg("-pthread")
        .arg("-o")
        .arg(&bin)
        .arg(&src)
        .status()
        .expect("a C compiler (`cc`) builds the leader-exit program for this test");
    assert!(built.success(), "cc failed");
    // Null stdio and a kill on drop: a failed assertion must not leave the
    // program running with the test harness's output pipe open.
    struct KillOnDrop(std::process::Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child = KillOnDrop(
        std::process::Command::new(&bin)
            .arg(&ready)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = child.0.id();

    // Wait until the worker runs alone: the leader shows Z, a thread does not.
    let proc_dir = std::path::PathBuf::from(format!("/proc/{pid}"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let live_threads = || {
        std::fs::read_dir(proc_dir.join("task"))
            .map(|d| {
                d.filter_map(|t| proc_state(&t.ok()?.path().join("stat")))
                    .filter(|s| s != "Z" && s != "X")
                    .count()
            })
            .unwrap_or(0)
    };
    while !(ready.exists() && proc_state(&proc_dir.join("stat")).as_deref() == Some("Z")) {
        assert!(
            std::time::Instant::now() < deadline,
            "the leader never exited alone"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(live_threads(), 1, "the worker thread runs");

    // Its session, logged as a sandboxed child logs its own.
    let here = HostId::current();
    let dash = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    let log = session_log_of(&ws);
    std::fs::write(
        &log,
        format!(
            "{pid} - {} {} {}\n",
            dash(here.boot.clone()),
            dash(here.ns.clone()),
            dash(here.time_ns.clone())
        ),
    )
    .unwrap();
    kill_owner(&e, &ws);
    let r = e.mgr.reopen(&spec(&ws));
    assert!(
        matches!(&r, Err(WorkspaceError::Busy { reason, .. }) if reason.contains("session")),
        "the session blocks: {r:?}"
    );

    // As a lock owner it is alive too.
    std::fs::remove_file(&log).unwrap();
    let root = ws.root().to_str().unwrap();
    git(&e.f.repo, &["worktree", "unlock", root]);
    let reason = format!("{LOCK_TAG}:pid={pid}:{}:name=w1", here.fields());
    git(&e.f.repo, &["worktree", "lock", "--reason", &reason, root]);
    let r = e.mgr.reopen(&spec(&ws));
    assert!(
        matches!(&r, Err(WorkspaceError::Busy { reason, .. }) if reason.contains("live process")),
        "the owner is alive: {r:?}"
    );

    drop(child);
    assert_eq!(e.mgr.reopen(&spec(&ws)).unwrap(), ws, "once it has exited");
}

/// Decision 0084, known gap. A sandboxed command (a build script, a test)
/// starts a process that closes every inherited descriptor, so it drops the
/// hold, and starts a new session, so the session log does not name it; then
/// the command exits. With a PID namespace (Linux) the kernel kills that
/// process with the command, and `quiesce` passes because nothing is left.
/// Elsewhere (macOS, Linux without namespaces) it runs on, and `quiesce`,
/// which the verifier calls before it commits and after the check, cannot see
/// it: the test pins that, so a change that closes the gap shows here. Its
/// writes after the verifier's commit are not in the committed attempt; only
/// the quality gate, which runs on the merged tree, judges what is merged.
#[cfg(unix)]
#[test]
fn a_process_that_leaves_its_session_is_seen_only_through_a_pid_namespace() {
    let python = [
        "/usr/bin/python3",
        "/usr/local/bin/python3",
        "/opt/homebrew/bin/python3",
    ]
    .into_iter()
    .find(|p| std::path::Path::new(p).is_file());
    let Some(python) = python else {
        eprintln!("no python3: a detaching process cannot be made here");
        return;
    };
    let e = env();
    let ws = create(&e, "w1");
    let pidfile = ws.scratch().join("detached.pid");
    let detach = format!(
        "import os,time\n\
         if os.fork() == 0:\n\
         \x20   os.closerange(0, 10240)\n\
         \x20   os.setsid()\n\
         \x20   open({pidfile:?}, 'w').write(str(os.getpid()))\n\
         \x20   time.sleep(30)\n\
         \x20   os._exit(0)\n",
        pidfile = pidfile.to_str().unwrap()
    );
    let mut cmd =
        kgraf_sandbox::workspace_command(&ws, kgraf_sandbox::Network::Deny, python).unwrap();
    cmd.args(["-c", &detach]);
    assert!(cmd.status().unwrap().success(), "the command exited");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let pid = loop {
        if let Some(pid) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        {
            break Some(pid);
        }
        if command_has_its_own_pid_namespace() || std::time::Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    // SAFETY: kill(pid, 0) sends nothing; it only asks whether `pid` exists.
    let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;

    if command_has_its_own_pid_namespace() {
        // The PID it wrote, if any, is its namespace's; the check is that
        // nothing of the command's is left, and quiesce passes.
        #[cfg(target_os = "linux")]
        assert_the_command_left_nothing(&ws);
        e.mgr.quiesce(&ws).expect("nothing is left to see");
        return;
    }

    let pid = pid.expect("the detached process started");
    assert!(alive(pid), "the detached process runs");
    let quiet = e.mgr.quiesce(&ws);
    // Stop it before any assertion can fail, so no process outlives the test.
    // SAFETY: `pid` is the process this test started, read back above.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let gone = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while alive(pid) && std::time::Instant::now() < gone {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!alive(pid), "the detached process was stopped");
    assert!(
        quiet.is_ok(),
        "the known gap closed: quiesce now sees a process that left its session \
         ({quiet:?}); update decision 0084 and this test"
    );
}
