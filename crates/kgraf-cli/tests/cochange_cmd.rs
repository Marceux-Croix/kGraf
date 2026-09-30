//! `kgraf cochange` reports what the orchestrator observed: the write sets it
//! folded into the relation and the collisions it recorded (decision 0020).

use kgraf_graph::{GraphStore, cochange};
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn observed_write_sets_and_collisions_are_reported() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(root.join("src/a.rs"), "pub fn a() {}\n").expect("write");
    git(root, &["init", "-q"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "first"]);

    // What a run records: a write set of two files, and a collision on one.
    std::fs::create_dir_all(root.join(".kgraf")).expect("state");
    {
        let store = GraphStore::open(&root.join(".kgraf").join("graph.db"), root).expect("open");
        let paths = vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()];
        cochange::record_task_writes(&store, "r1/a", &paths, 10).expect("writes");
        cochange::record_collision(&store, "r1/a", "r1/b", "src/a.rs", "textual", 11)
            .expect("collision");
    }

    let out = Command::new(env!("CARGO_BIN_EXE_kgraf"))
        .env("KGRAF_DIAG_DISABLE", "1")
        .arg("-C")
        .arg(root)
        .args(["cochange", "--commits", "5"])
        .output()
        .expect("run kgraf");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("observed pairs merged  1"), "{stdout}");
    assert!(stdout.contains("collisions observed    1"), "{stdout}");
}
