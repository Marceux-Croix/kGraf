//! kGraf's own git, through the real worktree manager and merge-back, in a
//! repository armed to run programs: hooks in `.githooks` (with
//! `core.hooksPath` set) and in `.git/hooks`, a config-defined hook, the file
//! system monitor, and a committed `.gitattributes` selecting a required
//! filter, a merge driver and a diff driver defined in `.git/config` (what a
//! Bypass worker could leave behind), plus a `.gitmodules` pointing at a
//! local bare repository with the gitlink committed. Every assertion reads
//! the marker directory, git's objects or the file system.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::git;
use kgraf_sandbox::{FinishOutcome, Identity, MergeBack, WorkspaceManager, WorkspaceSpec};

const BRANCH: &str = "integration";

struct Hostile {
    tmp: tempfile::TempDir,
    repo: PathBuf,
    marks: PathBuf,
    mgr: Arc<WorkspaceManager>,
}

impl Hostile {
    fn marks(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.marks)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}

fn set_exec(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn hostile() -> Hostile {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let marks = root.join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    let mk = root.join("mk");
    std::fs::write(
        &mk,
        format!(
            "#!/bin/sh\ntouch '{}'/\"$1\"\nshift\nif [ $# -gt 0 ] && [ -f \"$1\" ]; then cat \"$1\"; else cat; fi\n",
            marks.display()
        ),
    )
    .unwrap();
    set_exec(&mk);
    let m = |name: &str| format!("'{}' {name}", mk.display());

    let bare = root.join("sub.git");
    git(root, &["init", "-q", "--bare", "-b", "main", "sub.git"]);
    let src = root.join("subsrc");
    common::init_repo(&src);
    git(&src, &["push", "-q", bare.to_str().unwrap(), "main"]);
    let sub_head = git(&src, &["rev-parse", "HEAD"]);

    let repo = common::init_repo(&root.join("repo"));
    std::fs::write(
        repo.join(".gitattributes"),
        "*.t filter=ev merge=mev diff=dev\n",
    )
    .unwrap();
    std::fs::write(repo.join("a.t"), "x\r\nline2\r\n").unwrap();
    std::fs::write(repo.join("plain.txt"), "1\n2\n3\n4\n5\n").unwrap();
    std::fs::write(
        repo.join(".gitmodules"),
        format!(
            "[submodule \"sub\"]\n\tpath = sub\n\turl = {}\n",
            bare.display()
        ),
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_head},sub"),
        ],
    );
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "armed",
        ],
    );
    git(&repo, &["branch", BRANCH, "main"]);
    // Armed after the fixture's own commits.
    for (k, v) in [
        ("filter.ev.clean", m("clean")),
        ("filter.ev.smudge", m("smudge")),
        ("filter.ev.required", "true".to_owned()),
        (
            "merge.mev.driver",
            format!("{} </dev/null; exit 0", m("mergedriver")),
        ),
        ("diff.dev.textconv", m("textconv")),
        ("diff.external", m("external")),
        ("core.fsmonitor", m("fsmonitor")),
        ("hook.cfg.command", m("confighook")),
        ("hook.cfg.event", "pre-commit".to_owned()),
        ("submodule.recurse", "true".to_owned()),
        ("protocol.file.allow", "always".to_owned()),
        ("core.hooksPath", ".githooks".to_owned()),
    ] {
        git(&repo, &["config", k, &v]);
    }
    for event in ["post-commit", "post-checkout", "reference-transaction"] {
        git(&repo, &["config", "--add", "hook.cfg.event", event]);
    }
    for hook in [
        "pre-commit",
        "post-commit",
        "post-checkout",
        "post-merge",
        "reference-transaction",
        "post-index-change",
    ] {
        let path = repo.join(".git/hooks").join(hook);
        std::fs::write(&path, format!("#!/bin/sh\n{} </dev/null\n", m(hook))).unwrap();
        set_exec(&path);
    }
    let mgr = Arc::new(WorkspaceManager::open(&repo, &root.join("ws")).unwrap());
    mgr.startup().unwrap();
    Hostile {
        tmp,
        repo,
        marks,
        mgr,
    }
}

fn identity() -> Identity {
    Identity {
        name: "kGraf Merge".into(),
        email: "merge@kgraf.invalid".into(),
    }
}

fn plant_hooks(h: &Hostile, root: &Path) {
    let mk = h.tmp.path().join("mk");
    std::fs::create_dir_all(root.join(".githooks")).unwrap();
    for hook in [
        "pre-commit",
        "post-commit",
        "post-checkout",
        "post-merge",
        "reference-transaction",
        "post-index-change",
    ] {
        let path = root.join(".githooks").join(hook);
        std::fs::write(
            &path,
            format!("#!/bin/sh\n'{}' {hook} </dev/null\n", mk.display()),
        )
        .unwrap();
        set_exec(&path);
    }
}

/// A blob's exact bytes, read with plain git (`cat-file blob` applies no
/// filter).
fn blob(repo: &Path, spec: &str) -> Vec<u8> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "blob", spec])
        .env_remove("GIT_DIR")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn worktree_creation_commit_and_merge_run_no_program() {
    let h = hostile();
    // The planted worktree hooks are not staged: staging `.githooks` is a
    // separate question from whether kGraf's git runs them.
    std::fs::write(h.repo.join(".git/info/exclude"), ".githooks/\n").unwrap();
    // Hooks where a relative `core.hooksPath` resolves for each kind of call:
    // the checkout root (worktree add), the shared git directory (ref
    // updates run there) and, below, each worktree root.
    plant_hooks(&h, &h.repo.clone());
    plant_hooks(&h, &h.repo.join(".git"));
    let mb = MergeBack::open(h.mgr.clone(), BRANCH, identity()).unwrap();
    let ws = |name, base| {
        h.mgr
            .create(&WorkspaceSpec {
                name,
                base,
                branch: None,
            })
            .unwrap()
    };
    // Worktree add checks out `a.t` without its smudge filter, and leaves
    // the submodule alone.
    let mut w1 = mb.attempt(ws("w1", BRANCH)).unwrap();
    let mut w2 = mb.attempt(ws("w2", BRANCH)).unwrap();
    let mut w3 = mb.attempt(ws("w3", BRANCH)).unwrap();
    let root1 = w1.workspace().root().to_path_buf();
    assert_eq!(std::fs::read(root1.join("a.t")).unwrap(), b"x\r\nline2\r\n");
    assert!(
        std::fs::read_dir(root1.join("sub"))
            .map(|mut d| d.next().is_none())
            .unwrap_or(true),
        "the submodule was checked out"
    );
    for w in [&w1, &w2, &w3] {
        plant_hooks(&h, w.workspace().root());
    }
    std::fs::write(root1.join("a.t"), "w1\r\n").unwrap();
    std::fs::write(root1.join("plain.txt"), "w1\n2\n3\n4\n5\n").unwrap();
    let out = mb.finish(&mut w1, "w1", &["a.t", "plain.txt"]).unwrap();
    assert!(matches!(out, FinishOutcome::Merged { .. }), "{out:?}");
    // The commit stored the worktree's bytes, unfiltered.
    assert_eq!(blob(&h.repo, &format!("{BRANCH}:a.t")), b"w1\r\n");

    // w2, from the same base, changes another line: a three-way text merge,
    // byte for byte.
    let root2 = w2.workspace().root().to_path_buf();
    std::fs::write(root2.join("plain.txt"), "1\n2\n3\n4\nw2\n").unwrap();
    let out = mb.finish(&mut w2, "w2", &["plain.txt"]).unwrap();
    assert!(matches!(out, FinishOutcome::Merged { .. }), "{out:?}");
    assert_eq!(
        blob(&h.repo, &format!("{BRANCH}:plain.txt")),
        b"w1\n2\n3\n4\nw2\n"
    );

    // w3 changes `a.t` too: changed on both sides with a merge driver
    // configured, it is a conflict, never resolved by the driver.
    std::fs::write(w3.workspace().root().join("a.t"), "w3\r\n").unwrap();
    let out = mb.finish(&mut w3, "w3", &["a.t"]).unwrap();
    assert!(matches!(out, FinishOutcome::Conflicted(_)), "{out:?}");

    assert_eq!(h.marks(), Vec::<String>::new(), "a planted program ran");
    assert!(
        !h.repo.join(".git/modules").exists(),
        "the submodule was fetched"
    );
}

/// The control: the same repository runs the planted programs under plain
/// git, so an empty marker list above means the overrides held.
#[test]
fn the_armed_repository_runs_programs_under_plain_git() {
    let h = hostile();
    plant_hooks(&h, &h.repo.clone());
    std::fs::write(h.repo.join("a.t"), "changed\n").unwrap();
    git(&h.repo, &["add", "a.t"]);
    git(
        &h.repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "c",
        ],
    );
    let marks = h.marks();
    for expected in ["clean", "confighook", "fsmonitor", "post-commit"] {
        assert!(marks.iter().any(|m| m == expected), "{expected}: {marks:?}");
    }
}

/// Bypass's tracked git files (decision 0094): with the attempt's allowance
/// set, `.gitattributes`, `.gitmodules` and `.githooks/...` are committed and
/// merged as plain data, the hook's executable bit kept; `.gitconfig` is
/// still refused. The landed `.gitattributes` selects the armed filter and
/// merge driver for every path, and the next merge-back, which reads its
/// attributes from that tip, still runs neither.
#[test]
fn bypass_git_files_merge_as_plain_data_and_select_nothing_that_runs() {
    use std::os::unix::fs::PermissionsExt;
    let h = hostile();
    let mb = MergeBack::open(h.mgr.clone(), BRANCH, identity()).unwrap();
    let ws = |name| {
        h.mgr
            .create(&WorkspaceSpec {
                name,
                base: BRANCH,
                branch: None,
            })
            .unwrap()
    };
    let files: [(&str, &str); 3] = [
        (".gitattributes", "* filter=ev merge=mev diff=dev\n"),
        (
            ".gitmodules",
            "[submodule \"x\"]\n\tpath = x\n\turl = ../x\n",
        ),
        (".githooks/pre-commit", "#!/bin/sh\nexit 1\n"),
    ];
    let declared: Vec<&str> = files.iter().map(|(p, _)| *p).collect();

    // Without the allowance: refused, as before.
    let mut refused = mb.attempt(ws("r1")).unwrap();
    std::fs::write(
        refused.workspace().root().join(".gitattributes"),
        files[0].1,
    )
    .unwrap();
    let out = mb.finish(&mut refused, "r1", &[".gitattributes"]).unwrap();
    assert!(matches!(out, FinishOutcome::Refused(_)), "{out:?}");

    let mut a = mb.attempt(ws("b1")).unwrap();
    a.allow_bypass_git_files(true);
    let root = a.workspace().root().to_path_buf();
    for (path, text) in files {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
    }
    std::fs::set_permissions(
        root.join(".githooks/pre-commit"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let out = mb.finish(&mut a, "b1", &declared).unwrap();
    assert!(matches!(out, FinishOutcome::Merged { .. }), "{out:?}");
    for (path, text) in files {
        assert_eq!(
            blob(&h.repo, &format!("{BRANCH}:{path}")),
            text.as_bytes(),
            "{path}"
        );
    }
    let mode = git(&h.repo, &["ls-tree", BRANCH, ".githooks/pre-commit"]);
    assert!(mode.starts_with("100755"), "{mode}");

    // `.gitconfig` stays refused with the allowance.
    let mut c = mb.attempt(ws("b2")).unwrap();
    c.allow_bypass_git_files(true);
    std::fs::write(c.workspace().root().join(".gitconfig"), "[core]\n").unwrap();
    let out = mb.finish(&mut c, "b2", &[".gitconfig"]).unwrap();
    assert!(matches!(out, FinishOutcome::Refused(_)), "{out:?}");

    // A later merge-back reads attributes from the new tip, which selects
    // the filter, the merge driver and the diff driver for every path.
    let mb2 = MergeBack::open(h.mgr.clone(), BRANCH, identity()).unwrap();
    let mut d = mb2.attempt(ws("b3")).unwrap();
    std::fs::write(d.workspace().root().join("plain.txt"), "after\n").unwrap();
    let out = mb2.finish(&mut d, "b3", &["plain.txt"]).unwrap();
    assert!(matches!(out, FinishOutcome::Merged { .. }), "{out:?}");
    assert_eq!(blob(&h.repo, &format!("{BRANCH}:plain.txt")), b"after\n");
    assert_eq!(h.marks(), Vec::<String>::new(), "a planted program ran");
}
