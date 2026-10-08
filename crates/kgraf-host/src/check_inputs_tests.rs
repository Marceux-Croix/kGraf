//! What a worker's run leaves behind is never an input of its acceptance
//! check (decision 0084, amended 2026-10-06). The verifier's check runs in
//! the worker's tree; before this, its cargo target folder was the worker's
//! own `<scratch>/target` and every ignored file the worker's tools wrote
//! stayed in the tree. A worker that forged a build tool's records there
//! (a test binary that exits 0 with cargo's build records dated in the
//! future; an up-to-date build output) made the check run the forgery and
//! pass work that fails.
//!
//! Each test first shows the forgery works where it was planted (the same
//! command, run against the worker's own folders, passes the broken tree),
//! then that the verifier fails the broken tree.
//!
//! How to run: `cargo test -p kgraf-host check_inputs` on macOS or Linux,
//! with `cargo` on `PATH`; no network (the fixture crate has no
//! dependencies).

use super::*;

/// The worker's scratch folder for the worktree at `ws`.
fn scratch_of(ws: &Path) -> PathBuf {
    ws.parent()
        .expect("base")
        .join(".scratch")
        .join(ws.file_name().expect("name"))
}

/// Run `program args` in `dir` on the host, with `env`; whether it passed,
/// and its output.
fn host_run(dir: &Path, program: &str, args: &[&str], env: &[(&str, &Path)]) -> (bool, String) {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).current_dir(dir);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// Set every file and folder under `dir` to a modification time far in the
/// future, as a forger does so a tool takes its records as newer than any
/// source.
fn date_in_the_future(dir: &Path) {
    let (ok, out) = host_run(
        dir,
        "find",
        &[".", "-exec", "touch", "-t", "209901010000", "{}", "+"],
        &[],
    );
    assert!(ok, "{out}");
}

/// Every entry under `dir` with its size and modification time, sorted.
fn listing(dir: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).expect("read").flatten() {
            let meta = entry.path().symlink_metadata().expect("meta");
            if meta.is_dir() {
                stack.push(entry.path());
            }
            out.push((entry.path(), meta.len(), meta.modified().expect("time")));
        }
    }
    out.sort();
    out
}

const BROKEN_LIB: &str =
    "pub fn two() -> u8 { 3 }\n#[test]\nfn is_two() { assert_eq!(two(), 2); }\n";

/// The worker builds and tests a passing tree into its own target folder,
/// breaks the code, replaces the test binary with a script that exits 0 and
/// dates cargo's build records (fingerprints, dep-info, the binary) in the
/// future, so cargo takes everything as fresh. `cargo test` against that
/// folder passes the broken code. The verifier's check has a target folder
/// of its own and fails it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_test_binary_in_the_worker_s_target_folder_never_passes_the_check() {
    let (r, denies) = lockless_crate();
    let c = named("src/lib.rs", "w-forge");
    let ws = prepared(&r, &c, "w-forge").await;
    let target = scratch_of(&ws).join("target");
    let env = [("CARGO_TARGET_DIR", target.as_path())];
    let (ok, out) = host_run(&ws, "cargo", &["test", "--offline"], &env);
    assert!(ok, "the worker's own passing build: {out}");

    std::fs::write(ws.join("src/lib.rs"), BROKEN_LIB).expect("write");
    let deps = target.join("debug/deps");
    let mut forged = 0;
    for entry in std::fs::read_dir(&deps).expect("deps").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // The test binary: `tiny-<hash>`, no extension.
        if name.starts_with("tiny-") && !name.contains('.') {
            let path = entry.path();
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("forge");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            forged += 1;
        }
    }
    assert!(forged > 0, "no test binary found in {}", deps.display());
    date_in_the_future(&target);
    let (ok, out) = host_run(&ws, "cargo", &["test", "--offline"], &env);
    assert!(
        ok,
        "the forgery must pass the broken code against the worker's own target folder, or \
         this test proves nothing: {out}"
    );

    let v = CommandVerifier {
        read_denies: denies.clone(),
        ..verifier(&r, "cargo test --offline")
    };
    let verdict = v.verify(&c, &ws, &result()).await.expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("the forged test binary passed the check: {verdict:?}");
    };
    assert!(reason.contains("is_two"), "{reason}");
}

/// A tracked test script builds `build/app` from `src/app.sh` only when the
/// output is older than the source, as `make`, Gradle, MSBuild or `tsc -b`
/// skip work they take as done. The worker breaks the source and leaves an
/// ignored `build/app` that exits 0, dated in the future: the script, run in
/// the worker's tree, passes. The verifier removes ignored files before its
/// check, so the script builds from the committed source and fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_up_to_date_build_output_the_worker_left_never_passes_the_check() {
    let dir = tempfile::Builder::new()
        .prefix("check inputs ")
        .tempdir()
        .expect("dir");
    let root = dir.path().join("project");
    let state = root.join(".kgraf");
    let files = [
        (".gitignore", "build/\n.kgraf/\n"),
        (
            "test.sh",
            "#!/bin/sh\n[ build/app -nt src/app.sh ] || { mkdir -p build && \
             cp src/app.sh build/app && chmod +x build/app; }\n./build/app\n",
        ),
        ("src/app.sh", "#!/bin/sh\nexit 0\n"),
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
    let c = named("src/app.sh", "w-stale");
    let ws = prepared(&r, &c, "w-stale").await;
    std::fs::write(ws.join("src/app.sh"), "#!/bin/sh\necho broken\nexit 1\n").expect("write");
    std::fs::create_dir_all(ws.join("build")).expect("mkdir");
    std::fs::write(ws.join("build/app"), "#!/bin/sh\nexit 0\n").expect("forge");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(ws.join("build/app"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    date_in_the_future(&ws.join("build"));
    let (ok, out) = host_run(&ws, "sh", &["test.sh"], &[]);
    assert!(
        ok,
        "the forgery must pass in the worker's tree, or this test proves nothing: {out}"
    );

    let v = CommandVerifier {
        read_denies: denies,
        ..verifier(&r, "sh test.sh")
    };
    let verdict = v.verify(&c, &ws, &result()).await.expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("the stale build output passed the check: {verdict:?}");
    };
    assert!(reason.contains("broken"), "{reason}");
}

/// With the checkout's own `target/` built, the check's target folder starts
/// as a clone of it, and the check of a correct change passes; the clone is
/// gone afterwards, and nothing was written into the checkout's folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_check_starts_from_the_checkout_s_build_folder_and_leaves_it_as_it_was() {
    let (r, denies) = lockless_crate();
    let warm = r.root.join("target");
    let (ok, out) = host_run(
        &r.root,
        "cargo",
        &["test", "--offline", "--no-run"],
        &[("CARGO_TARGET_DIR", warm.as_path())],
    );
    assert!(ok, "{out}");
    let warm_before = listing(&warm);
    let c = named("src/lib.rs", "w-warm");
    let ws = prepared(&r, &c, "w-warm").await;
    std::fs::write(ws.join("src/lib.rs"), LIB_V2).expect("write");
    let v = CommandVerifier {
        read_denies: denies,
        ..verifier(&r, "cargo test --offline")
    };
    let verdict = v.verify(&c, &ws, &result()).await.expect("ran");
    assert!(verdict.passed(), "{verdict:?}");
    assert_eq!(
        listing(&warm),
        warm_before,
        "the checkout's build folder changed"
    );
    let live = r.workspaces.get(&ws).expect("live");
    assert!(!live.checks_dir().exists(), "a check folder is left");
}
