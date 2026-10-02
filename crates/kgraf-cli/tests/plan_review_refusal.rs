//! A run refused before it starts (decision 0094: plan review is on and the
//! command line has nobody to approve the plan) reports the refusal alone and
//! creates nothing: no `.kgraf`, no databases, no integration branch, and no
//! trace path printed for a trace that is never written. State that existed
//! before the run is left exactly as it was.
//!
//! Replicate: `cargo test -p kgraf-cli --test plan_review_refusal`. The tests run
//! the built `kgraf` binary on a fresh git repository in a temporary directory, with
//! the permissions file, `HOME` and the config folders pointed into temporary
//! directories, so nothing of the user's is read or written.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("README.md"), "stub\n").expect("write");
    std::fs::write(root.join("BRIEF.md"), "write hello.txt containing hello\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "init"]);
}

/// `kgraf orchestrate` on `root` with plan review on, run `run`, the route's server at
/// `base_url`, and the given extra flags.
fn orchestrate_at(
    root: &Path,
    scratch: &Path,
    base_url: &str,
    run: &str,
    extra: &[&str],
) -> Output {
    let permissions = scratch.join("permissions.toml");
    std::fs::write(&permissions, "mode = \"manual\"\n").expect("write");
    let prices = scratch.join("prices.json");
    std::fs::write(&prices, "{}").expect("write");
    // Output goes to files and the wait is bounded: a run that is not refused
    // waits on a model that never answers, and must fail the test, not hang it.
    let out_path = scratch.join(format!("{run}.stdout"));
    let err_path = scratch.join(format!("{run}.stderr"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_kgraf"))
        .env("KGRAF_DIAG_DISABLE", "1")
        .env("HOME", scratch)
        .env("XDG_CONFIG_HOME", scratch)
        .env("XDG_DATA_HOME", scratch)
        .env("KGRAF_PERMISSIONS_PATH", &permissions)
        .env("KGRAF_OLLAMA_BASE_URL", base_url)
        .env_remove("KGRAF_CONTAINER_SANDBOX")
        .arg("-C")
        .arg(root)
        .args(["orchestrate", "--instruction"])
        .arg(root.join("BRIEF.md"))
        .args(["--run", run])
        .args(["--route", "ollama:stub-model", "--prices"])
        .arg(&prices)
        .args(["--gate-command", "true", "--plan-review"])
        .args(extra)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&out_path).expect("stdout file"))
        .stderr(std::fs::File::create(&err_path).expect("stderr file"))
        .spawn()
        .expect("spawn kgraf");
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if started.elapsed() > std::time::Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "kgraf was not refused and kept running\nstdout: {}\nstderr: {}",
                std::fs::read_to_string(&out_path).unwrap_or_default(),
                std::fs::read_to_string(&err_path).unwrap_or_default()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    Output {
        status,
        stdout: std::fs::read(&out_path).expect("stdout"),
        stderr: std::fs::read(&err_path).expect("stderr"),
    }
}

/// The refused run: the route's server accepts connections and never answers, so
/// the reachability check passes and a run that got past the refusal would go on
/// to create its branch and state, the failure these tests pin.
fn orchestrate(root: &Path, scratch: &Path, extra: &[&str]) -> Output {
    // Held until the command ends: connections queue in its backlog.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let port = listener.local_addr().expect("address").port();
    let url = format!("http://127.0.0.1:{port}/v1");
    orchestrate_at(root, scratch, &url, "refused", extra)
}

/// Every file under `dir` (relative path to bytes), `.git` excluded.
fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("entry");
            if entry.file_name() == ".git" {
                continue;
            }
            let path = entry.path();
            let key = path
                .strip_prefix(base)
                .expect("under base")
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                out.insert(format!("{key}/"), Vec::new());
                walk(base, &path, out);
            } else {
                out.insert(key, std::fs::read(&path).expect("read"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn kgraf_branches(root: &Path) -> String {
    git(
        root,
        &["for-each-ref", "--format=%(refname)", "refs/heads/kgraf"],
    )
}

#[test]
fn a_run_refused_for_plan_review_says_the_refusal_and_creates_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let scratch = tempfile::tempdir().expect("scratch");
    let root = dir.path();
    repo(root);
    let before = snapshot(root);

    let out = orchestrate(root, scratch.path(), &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("plan review is on, and this run has nobody to approve the plan"),
        "the refusal is the error: {stderr}"
    );
    assert!(
        !stdout.contains("trace "),
        "no trace path for a trace never written: {stdout}"
    );
    assert!(
        !stdout.contains("could not be read") && !stderr.contains("could not be read"),
        "no notice about a missing trace: {stdout}\n{stderr}"
    );
    assert!(
        kgraf_branches(root).trim().is_empty(),
        "a branch was created: {}",
        kgraf_branches(root)
    );
    assert_eq!(
        snapshot(root),
        before,
        "the refused run created files or directories (.kgraf, graph.db, ledger.db)"
    );
}

#[test]
fn a_refused_run_leaves_the_state_of_earlier_runs_exactly_as_it_was() {
    let dir = tempfile::tempdir().expect("dir");
    let scratch = tempfile::tempdir().expect("scratch");
    let root = dir.path();
    repo(root);
    // State an earlier run left. A real command makes the databases: this one
    // opens the project (creating `.kgraf`, `graph.db` and `ledger.db`) and stops
    // at the route, which nothing listens for. A worktree base with content, a run
    // folder and an integration branch are added by hand.
    let earlier = orchestrate_at(
        root,
        scratch.path(),
        "http://127.0.0.1:9/v1",
        "earlier",
        &["--approve-plan"],
    );
    assert!(!earlier.status.success(), "{earlier:?}");
    let state = root.join(".kgraf");
    for db in ["graph.db", "ledger.db"] {
        assert!(state.join(db).is_file(), "the earlier run made {db}");
    }
    std::fs::create_dir_all(state.join("worktrees/earlier-w1")).expect("mkdir");
    std::fs::create_dir_all(state.join("runs/earlier")).expect("mkdir");
    std::fs::write(state.join("worktrees/earlier-w1/note"), b"keep").expect("write");
    std::fs::write(state.join("runs/earlier/trace.jsonl"), b"{}\n").expect("write");
    git(root, &["branch", "kgraf/earlier"]);
    let before = snapshot(root);
    let branches = kgraf_branches(root);
    assert!(branches.contains("kgraf/earlier"), "{branches}");

    let out = orchestrate(root, scratch.path(), &[]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nobody to approve the plan"),
        "{out:?}"
    );

    assert_eq!(snapshot(root), before, "earlier state changed");
    assert_eq!(kgraf_branches(root), branches, "branches changed");
}

/// The early refusal is the orchestrator's own rule, not a wider one: with
/// `--approve-plan` the command goes on past it.
#[test]
fn approving_the_plan_on_the_command_line_is_not_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let scratch = tempfile::tempdir().expect("scratch");
    repo(dir.path());
    // Nothing listens at the route, so the run ends at once for that reason.
    let out = orchestrate_at(
        dir.path(),
        scratch.path(),
        "http://127.0.0.1:9/v1",
        "approved",
        &["--approve-plan"],
    );
    let all = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!all.contains("nobody to approve the plan"), "{all}");
}
