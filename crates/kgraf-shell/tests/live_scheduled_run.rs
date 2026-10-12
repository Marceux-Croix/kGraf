//! Live check: a scheduled run fired from the desktop shell's own
//! background thread ([`kgraf_shell::scheduler_bg::SchedulerHandle`]), exactly the
//! mechanism that runs while the app sits in the tray (decision 0052). No fake
//! provider: the run goes through `kgraf-host` to a real route, the same
//! construction `kgraf orchestrate` and the shipped window's tray both use.
//!
//! Ignored by default: it spends money. Keys come **only** from a `KEY=VALUE`
//! file named by `KGRAF_LIVE_ENV_FILE` (mirrors
//! `kgraf-host/tests/live_sessions.rs`), read into memory here and set as this
//! test process's own `KGRAF_OPENAI_API_KEY` only -- never printed, never the
//! file itself read a second time, never the shell this test's own `cargo test`
//! runs in.
//!
//! ```text
//! KGRAF_LIVE_SCHEDULE=1 KGRAF_LIVE_ENV_FILE=<repo>/.env \
//!   cargo test -p kgraf-shell --test live_scheduled_run -- --ignored --nocapture --test-threads=1
//! ```
//!
//! **Why the scheduler thread, not the CLI's `schedule run-now`.** `run-now`
//! bypasses the claim table entirely (its own doc comment says so); it proves
//! the run construction, not the tray-fired path. This test instead attaches a
//! one-shot schedule directly to a work node with `kgraf_schedule::node_store`
//! (what `kgraf schedule add` calls) and spawns the real
//! `SchedulerHandle::spawn_with` background thread with `background_mode: true`
//! (window closed, app in the tray), the same call `kgraf_shell::open_project_parts`
//! makes for the shipped window. The thread ticks on its own 20-second interval
//! and claims the occurrence itself; nothing here reaches into the claim table.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use kgraf_graph::GraphStore;
use kgraf_schedule::ScheduleDefinition;
use kgraf_schedule::node_store::{self, NodeChoice};
use kgraf_shell::scheduler_bg::{ProjectFactory, SchedulerHandle};

/// `KEY=VALUE` lines of the file `KGRAF_LIVE_ENV_FILE` names. Read into memory
/// only; nothing is set in the process environment here, and no value is ever
/// printed (mirrors `kgraf-host/tests/live_sessions.rs::env_file`).
fn env_file() -> std::collections::BTreeMap<String, String> {
    let Ok(path) = std::env::var("KGRAF_LIVE_ENV_FILE") else {
        return Default::default();
    };
    let text = std::fs::read_to_string(&path).expect("KGRAF_LIVE_ENV_FILE is readable");
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("export ").unwrap_or(l).split_once('='))
        .map(|(k, v)| {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            (k.trim().to_owned(), v.to_owned())
        })
        .collect()
}

/// Sets this test process's own `KGRAF_OPENAI_API_KEY` from the `.env` file's
/// `HAUSTORIA_OPENAI_API_KEY` (or its current name), so
/// `kgraf_fleet::credential_from_env` finds it without this file ever reading
/// or printing the `.env` file's contents again. `true` when a key was found.
fn set_openai_credential() -> bool {
    let file = env_file();
    let Some(key) = file
        .get("HAUSTORIA_OPENAI_API_KEY")
        .or_else(|| file.get("KGRAF_OPENAI_API_KEY"))
        .cloned()
    else {
        return false;
    };
    // SAFETY-in-spirit note: `set_var` inside this test's own process only; the
    // value is never printed, logged, or passed to a shell.
    unsafe { std::env::set_var("KGRAF_OPENAI_API_KEY", key) };
    true
}

fn live() -> bool {
    std::env::var("KGRAF_LIVE_SCHEDULE").is_ok_and(|v| v == "1")
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// A tiny scratch git repo: no `Cargo.toml`, so no quality gate is
/// auto-discovered and the run's only cost is the one worker turn this test
/// exists to show.
fn scratch_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("NOTES.txt"),
        "GLOSSARY\n=========\n(one entry per line, \"term: definition\")\n",
    )
    .expect("write NOTES.txt");
    git(dir.path(), &["init", "-q"]);
    git(
        dir.path(),
        &["config", "user.email", "scratch@example.invalid"],
    );
    git(dir.path(), &["config", "user.name", "scratch"]);
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "seed"]);
    dir
}

/// Prices from the same fixture the other live tests use.
fn prices_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/prices.json")
}

const INSTRUCTION: &str = "This is ONE small, atomic task for ONE worker; there is exactly one \
    contract and one worker. This repository has one file, NOTES.txt. Call the write_file tool \
    exactly once (not the shell tool, and no other tool) to append one new line to NOTES.txt for \
    the term \"nebula\" in \"term: definition\" format with a short, correct definition. Do not \
    run any shell command. Then reply with one short sentence confirming the line was added, \
    ending that sentence with the exact word GALAXY-SCHED-0028.";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: spends money; see the module docs"]
async fn a_scheduled_run_fires_from_the_shells_background_thread_against_a_real_provider() {
    if !live() {
        println!("KGRAF_LIVE_SCHEDULE is not 1; not run");
        return;
    }
    if !set_openai_credential() {
        println!("SKIPPED: no OpenAI key in KGRAF_LIVE_ENV_FILE");
        return;
    }

    let repo = scratch_repo();
    let project = repo.path().to_path_buf();
    let state = project.join(".kgraf");
    std::fs::create_dir_all(&state).expect("state dir");

    // The run's own settings, written by hand exactly as the module doc of
    // `scheduler_bg` says a real project's `.kgraf/run.json` is today (no
    // settings surface for routes yet). gpt-6-luna is OpenAI's cheapest listed
    // model ($0.10/M input, $0.50/M output; see the fixture's prices.json),
    // chosen so one worker turn costs a small fraction of a cent.
    let run_config = serde_json::json!({
        "routes": ["openai:gpt-6-luna"],
        "prices": prices_path(),
        "cap_run": 0.20,
        "cap_task": 0.15,
        "max_items": 1,
        "max_agents": 1,
        "steps": 6,
        "max_tokens": 400,
        "verify": "true",
        "sessions": false,
        "planner_max_tokens": 3000
    });
    std::fs::write(
        state.join("run.json"),
        serde_json::to_string_pretty(&run_config).expect("json"),
    )
    .expect("write run.json");

    // Attach a one-shot schedule to a work node, due in 5 seconds -- exactly what
    // `kgraf schedule add <id> --instruction <file> --zone UTC --at <RFC3339>`
    // does (`kgraf-cli/src/schedule_cmd.rs::add`), called here directly against
    // the same `kgraf_schedule::node_store` API so the graph is left open for
    // this test to read from afterward.
    let due = Utc::now() + chrono::TimeDelta::seconds(5);
    let def = ScheduleDefinition::one_shot("live-sched-0028", INSTRUCTION, due, "UTC")
        .expect("a valid one-shot definition");
    let node_id = {
        let graph = GraphStore::open(&state.join("graph.db"), &project).expect("graph opens");
        let node_id = node_store::add(&graph, &def, NodeChoice::FromInstruction, Utc::now())
            .expect("schedule attaches to a new node");
        println!("schedule live-sched-0028 attached to work node {node_id}, due {due}");
        node_id
    };

    // Spawn the real background thread: `background_mode: true` is decision
    // 0052's tray state (the window closed, the app in the tray) -- the exact
    // call `kgraf_shell::open_project_parts` makes for the shipped window.
    let factory = Arc::new(ProjectFactory::for_project(project.clone()));
    let handle = SchedulerHandle::spawn_with(project.clone(), true, factory, None);

    // Poll the node's history for a settled outcome. The thread ticks every
    // `scheduler_bg::TICK_INTERVAL` (20s); allow several ticks plus the real
    // model call and merge-back.
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    let mut settled = None;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let graph = GraphStore::open(&state.join("graph.db"), &project).expect("graph opens");
        let views = node_store::views(&graph).expect("views read");
        let view = views
            .iter()
            .find(|v| v.schedule_id == "live-sched-0028")
            .expect("the schedule's view exists");
        if let Some(last) = &view.last_run
            && matches!(last.event.as_str(), "completed" | "failed")
        {
            println!(
                "schedule settled: event={} detail={:?} spend={:?}",
                last.event, last.detail, last.spend
            );
            settled = Some(last.clone());
            break;
        }
    }

    // Stop the thread before asserting, so a failed assertion never leaves it
    // running past the test.
    drop(handle);

    let settled = settled.expect("the scheduled run settled within the deadline");
    assert_eq!(
        settled.event, "completed",
        "the scheduled run should complete: {:?}",
        settled.detail
    );
    let spend = settled.spend.expect("a completed run records its spend");
    println!("node {node_id}: {}", spend.text);
    assert!(spend.micros > 0, "a real provider call has nonzero spend");

    // The worker writes inside its own git worktree and the run merges that
    // work onto an integration branch (`kgraf/<run id>`); the project's
    // checked-out working tree is never touched by a run, so the merged
    // content is read with `git show`, not from the file on disk (mirrors
    // `live_box_run.rs`, which hit the same thing first).
    let run = settled.run_id.expect("a completed run recorded its run id");
    let integration_branch = format!("kgraf/{run}");
    let notes = Command::new("git")
        .args(["show", &format!("{integration_branch}:NOTES.txt")])
        .current_dir(&project)
        .output()
        .expect("git show runs");
    assert!(
        notes.status.success(),
        "cannot read NOTES.txt from {integration_branch}: {}",
        String::from_utf8_lossy(&notes.stderr)
    );
    let notes = String::from_utf8_lossy(&notes.stdout).into_owned();
    assert!(
        notes.to_lowercase().contains("nebula"),
        "the worker's write_file call should have appended the nebula line on \
         {integration_branch}: {notes}"
    );
}
