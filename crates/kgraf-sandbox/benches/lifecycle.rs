//! Budgets: worktree create and remove time, and sandbox
//! launch overhead per command.
//!
//! Two repositories: a synthetic one (200 small files) and a local clone of
//! this repository, so the numbers cover a realistic checkout size.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use kgraf_sandbox::{Network, SandboxPolicy, WorkspaceManager, WorkspaceSpec};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

fn synthetic_repo(root: &Path) -> PathBuf {
    let repo = root.join("synthetic");
    std::fs::create_dir_all(repo.join("src")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    for i in 0..200 {
        std::fs::write(
            repo.join("src").join(format!("f{i}.rs")),
            format!("// {i}\n"),
        )
        .expect("write");
    }
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=b",
            "-c",
            "user.email=b@b",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
    repo
}

fn cloned_self(root: &Path) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dst = root.join("self");
    git(
        root,
        &[
            "clone",
            "-q",
            "--local",
            "--no-hardlinks",
            src.to_str().expect("utf-8 path"),
            dst.to_str().expect("utf-8 path"),
        ],
    );
    dst
}

fn bench_repo(c: &mut Criterion, label: &str, repo: &Path, base: &Path) {
    let mgr = WorkspaceManager::open(repo, base).expect("open");
    mgr.startup().expect("startup");
    let mut n = 0u64;

    c.bench_function(&format!("worktree_create/{label}"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                n += 1;
                let name = format!("c{n}");
                let spec = WorkspaceSpec {
                    name: &name,
                    base: "HEAD",
                    branch: None,
                };
                let start = Instant::now();
                let ws = mgr.create(&spec).expect("create");
                total += start.elapsed();
                mgr.remove(ws).expect("remove");
            }
            total
        })
    });

    c.bench_function(&format!("worktree_remove/{label}"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                n += 1;
                let name = format!("r{n}");
                let spec = WorkspaceSpec {
                    name: &name,
                    base: "HEAD",
                    branch: None,
                };
                let ws = mgr.create(&spec).expect("create");
                let start = Instant::now();
                mgr.remove(ws).expect("remove");
                total += start.elapsed();
            }
            total
        })
    });
}

fn worktrees(c: &mut Criterion) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let synthetic = synthetic_repo(tmp.path());
    bench_repo(
        c,
        "synthetic_200_files",
        &synthetic,
        &tmp.path().join("ws-synth"),
    );
    let own = cloned_self(tmp.path());
    bench_repo(c, "kgraf_clone", &own, &tmp.path().join("ws-self"));
}

fn launch_overhead(c: &mut Criterion) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    c.bench_function("launch/unconfined_true", |b| {
        b.iter(|| {
            let status = Command::new("/usr/bin/true")
                .current_dir(&root)
                .status()
                .expect("spawn");
            assert!(status.success());
        })
    });
    for (label, network) in [("deny", Network::Deny), ("allow", Network::Allow)] {
        // Linux without seccomp refuses Network::Deny; skip rather than fail.
        let policy = SandboxPolicy::new(vec![root.clone()], network);
        if kgraf_sandbox::sandboxed_command(&policy, "/usr/bin/true", &root).is_err() {
            continue;
        }
        c.bench_function(&format!("launch/sandboxed_true_network_{label}"), |b| {
            b.iter(|| {
                let status = kgraf_sandbox::sandboxed_command(&policy, "/usr/bin/true", &root)
                    .expect("policy")
                    .status()
                    .expect("spawn");
                assert!(status.success());
            })
        });
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20).measurement_time(Duration::from_secs(8));
    targets = worktrees, launch_overhead
}
criterion_main!(benches);
