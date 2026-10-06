//! The permission check's cost per shell call, on the harness path (decision
//! 0094, amended 2026-10-06): the user's rules and the deletion limit
//! (`hard_limit`, the rules hook) and the command policy
//! (`ExecPolicy::evaluate`, the shell hook), on typical commands, with the
//! worktree's files (`package.json`, a Makefile, a Python script) read where
//! a command runs them. Each case is measured once per call, the way a worker
//! call meets both hooks and the tool's check that the files did not change
//! since the review; repeated calls on the same files hit the caches, as a
//! worker's repeated commands do.
//!
//! How to run: `cargo bench -p kgraf-tools --bench permission_reading`. No
//! network, no process started; the worktree is a temporary directory.

use std::path::Path;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use kgraf_core::{AgentId, TaskId};
use kgraf_tools::approval::PendingCall;
use kgraf_tools::execpolicy::ExecPolicy;
use kgraf_tools::permissions::{
    AllowSet, ModeSource, PermissionControl, PermissionMode, PermissionSettings, ProcessFacts,
    Rule, UserRules, hard_limit,
};
use serde_json::json;

fn worktree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("dir");
    let script: String = (0..100)
        .map(|i| format!("def f{i}(x):\n    return x + {i}\n"))
        .collect::<String>()
        + "import json\nprint(json.dumps([f0(1)]))\n";
    let files: Vec<(&str, String)> = vec![
        (
            "package.json",
            r#"{"name":"app","scripts":{"test":"jest --coverage","build":"tsc && vite build","pretest":"eslint ."}}"#
                .to_owned(),
        ),
        (
            "Makefile",
            "CC := cc\nCFLAGS = -O2\nall: build\nbuild: main.o\n\t$(CC) $(CFLAGS) -o app main.o\nmain.o: main.c\n\t$(CC) -c main.c\n"
                .to_owned(),
        ),
        ("script.py", script),
        ("crates/x/src/lib.rs", "pub fn f() {}\n".to_owned()),
    ];
    // Written a minute in the past, as a worktree's files are by the time a
    // worker runs commands: a file changed within the last two seconds is
    // always read again, never taken from the cache.
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    for (name, text) in files {
        let p = dir.path().join(name);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(&p, text).expect("write");
        std::fs::File::options()
            .write(true)
            .open(&p)
            .and_then(|f| f.set_modified(past))
            .expect("set modified");
    }
    dir
}

fn control() -> Arc<PermissionControl> {
    let settings = PermissionSettings {
        mode: PermissionMode::Auto,
        allow: AllowSet::default()
            .with_programs([
                "cargo", "git", "npm", "jest", "eslint", "make", "cc", "python3",
            ])
            .expect("allow"),
        rules: UserRules {
            deny: vec![Rule::parse("shell(curl*)").expect("rule")],
            ask: vec![Rule::parse("shell(git push*)").expect("rule")],
        },
        ..PermissionSettings::default()
    };
    Arc::new(
        PermissionControl::new(settings, ModeSource::CommandLine, ProcessFacts::default())
            .expect("control"),
    )
}

fn policy(control: Arc<PermissionControl>, root: &Path) -> ExecPolicy {
    ExecPolicy::default_kgraf_policy()
        .with_permissions(control)
        .with_working_dir(root)
}

fn bench(c: &mut Criterion) {
    let dir = worktree();
    let root = dir.path();
    let control = control();
    let rules = control.rules();
    let policy = policy(control, root);
    let home = root.join("home");
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("cargo test", vec!["cargo", "test", "-p", "kgraf-tools"]),
        ("git status", vec!["git", "status", "--short"]),
        (
            "sh -c cd && cargo check",
            vec!["sh", "-c", "cd crates/x && cargo check -q"],
        ),
        ("npm test", vec!["npm", "test"]),
        ("make build", vec!["make", "build"]),
        ("python3 script.py", vec!["python3", "script.py"]),
    ];
    let mut group = c.benchmark_group("permission_check");
    for (name, words) in cases {
        let argv: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
        let call = PendingCall {
            call_id: "c".into(),
            tool: "shell".into(),
            input: json!({ "command": argv }),
            worker: AgentId::new("w"),
            task: TaskId::new("t"),
        };
        // The rules hook and the shell hook at review, and the tool's check
        // that the files it read did not change before the run.
        group.bench_function(name, |b| {
            b.iter(|| {
                let limit = hard_limit(&rules, &call, Some(&home), Some(root));
                let verdict = policy.evaluate(&argv);
                policy.note_reviewed(&argv);
                let changed = policy.files_changed_since_review(&argv);
                std::hint::black_box((limit, verdict, changed))
            })
        });
    }
    // The first check of a script kGraf has not seen: 400 distinct scripts
    // in turn, more than the caches hold, so each check reads and parses.
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let cold: Vec<Vec<String>> = (0..400)
        .map(|n| {
            let name = format!("cold{n}.py");
            let text: String = (0..100)
                .map(|i| format!("def f{i}_{n}(x):\n    return x + {i}\n"))
                .collect::<String>()
                + "import subprocess\nsubprocess.run(['cargo', 'test'])\n";
            let p = root.join(&name);
            std::fs::write(&p, text).expect("write");
            std::fs::File::options()
                .write(true)
                .open(&p)
                .and_then(|f| f.set_modified(past))
                .expect("set modified");
            vec!["python3".to_owned(), name]
        })
        .collect();
    let mut next = 0;
    group.bench_function("python3 script.py (first check)", |b| {
        b.iter(|| {
            let argv = &cold[next % cold.len()];
            next += 1;
            let call = PendingCall {
                call_id: "c".into(),
                tool: "shell".into(),
                input: json!({ "command": argv }),
                worker: AgentId::new("w"),
                task: TaskId::new("t"),
            };
            let limit = hard_limit(&rules, &call, Some(&home), Some(root));
            let verdict = policy.evaluate(argv);
            policy.note_reviewed(argv);
            let changed = policy.files_changed_since_review(argv);
            std::hint::black_box((limit, verdict, changed))
        })
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
