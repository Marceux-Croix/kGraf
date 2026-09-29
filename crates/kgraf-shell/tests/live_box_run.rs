//! Live check: a `/run <instruction>` typed into the window's
//! orchestrator box, against a real provider. No fake provider and no
//! in-process model: `BoxModel::submit` (the exact code the composer calls on
//! Enter) drives `OrchestratorHost` (`impl BoxBackend`), built the same way
//! `kgraf_shell::open_project_parts` builds it for the shipped window --
//! `scheduler_bg::ProjectFactory` over `.kgraf/run.json`, through `kgraf-host`.
//!
//! Ignored by default: it spends money. Keys come **only** from a `KEY=VALUE`
//! file named by `KGRAF_LIVE_ENV_FILE` (mirrors
//! `kgraf-host/tests/live_sessions.rs`), read into memory here and set as this
//! test process's own `KGRAF_OPENAI_API_KEY` only -- never printed.
//!
//! ```text
//! KGRAF_LIVE_BOX_RUN=1 KGRAF_LIVE_ENV_FILE=<repo>/.env \
//!   cargo test -p kgraf-shell --test live_box_run -- --ignored --nocapture --test-threads=1
//! ```
//!
//! **Why in-process, not a real egui window.** `OrchestratorHost` is the
//! window's own backend struct: `crates/kgraf-shell/src/lib.rs::run_with_host`
//! builds one `HostParts` per project and hands it to the same
//! `OrchestratorHost::new` this test calls, and the box's `show`/`show_styled`
//! functions do nothing more than call `BoxModel::submit` on Enter and
//! `BoxModel::absorb` on each repaint -- both called directly here. What a real
//! window would add on top (eframe's event loop, egui's paint pass) draws the
//! same state this test reads through `BoxModel::lines()`; it adds no
//! additional path to a real provider. No `osascript` or window automation is
//! used or needed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use kgraf_orchestrator::{AskAnswer, AskDesk};
use kgraf_shell::orchestrator_box::{BackendState, BoxBackend, BoxModel, Line};
use kgraf_shell::orchestrator_host::{HostParts, OrchestratorHost};
use kgraf_shell::scheduler_bg::ProjectFactory;

/// `KEY=VALUE` lines of the file `KGRAF_LIVE_ENV_FILE` names (mirrors
/// `live_scheduled_run.rs::env_file`, kept file-local rather than shared so
/// each live test stays self-contained and auditable on its own).
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

fn set_openai_credential() -> bool {
    let file = env_file();
    let Some(key) = file
        .get("HAUSTORIA_OPENAI_API_KEY")
        .or_else(|| file.get("KGRAF_OPENAI_API_KEY"))
        .cloned()
    else {
        return false;
    };
    unsafe { std::env::set_var("KGRAF_OPENAI_API_KEY", key) };
    true
}

fn live() -> bool {
    std::env::var("KGRAF_LIVE_BOX_RUN").is_ok_and(|v| v == "1")
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

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

fn prices_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/prices.json")
}

const INSTRUCTION: &str = "This is ONE small, atomic task for ONE worker; there is exactly one \
    contract and one worker. This repository has one file, NOTES.txt. Call the write_file tool \
    exactly once (not the shell tool, and no other tool) to append one new line to NOTES.txt for \
    the term \"quasar\" in \"term: definition\" format with a short, correct definition. Do not \
    run any shell command. Then reply with one short sentence confirming the line was added, \
    ending that sentence with the exact word GALAXY-BOX-0028.";

#[test]
#[ignore = "live: spends money; see the module docs"]
fn a_slash_run_from_the_orchestrator_box_reaches_a_real_provider() {
    if !live() {
        println!("KGRAF_LIVE_BOX_RUN is not 1; not run");
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

    // `kgraf_shell::open_project_parts`'s own construction, without the desk's
    // persisted allow-rule store (no `.kgraf/allow-rules.json` machinery is
    // under test here): a factory over the project's host, and a desk with no
    // rules, so any ask this run does raise still reaches the box as a real
    // `PendingAsk` the loop below answers, exactly as a person at the window
    // would.
    let wake: kgraf_orchestrator::Wake = Arc::new(|| {});
    let factory = Arc::new(ProjectFactory::for_project(project.clone()));
    let desk = Arc::new(AskDesk::new(None, wake.clone()));
    let parts = HostParts {
        factory: factory.clone(),
        housekeeping: factory.clone(),
        desk,
        router: None,
    };
    let mut backend = OrchestratorHost::new(Some(parts), wake).expect("host builds");
    let mut model = BoxModel::default();

    model.input = format!("/run {INSTRUCTION}");
    model.submit(&mut backend, None);
    // `submit` pushed either a "Starting a run..." notice or an error line;
    // print every line so a failed assertion below shows exactly what the box
    // said, the same text a person at the window would read.
    for line in model.lines() {
        println!("{line:?}");
    }
    assert!(
        matches!(
            backend.state(),
            BackendState::Starting | BackendState::Running { .. }
        ),
        "the box should have started a run; state is {:?}",
        backend.state()
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    let mut finished = false;
    let mut run_id: Option<String> = None;
    while std::time::Instant::now() < deadline {
        if run_id.is_none()
            && let BackendState::Running { run } = backend.state()
        {
            run_id = Some(run);
        }
        std::thread::sleep(Duration::from_millis(500));
        model.absorb(&mut backend);
        // Answer any approval ask the run raises, exactly as a person at the
        // window would with "allow once" -- defensive: the instruction asks for
        // no tool but write_file, which this project's approval policy does not
        // gate, so none is expected, but a real run is not scripted and must
        // not silently stall this test if one appears.
        for ask in backend.pending_asks() {
            println!("answering ask: {ask:?}");
            model.answer(&mut backend, &ask, AskAnswer::AllowOnce);
        }
        if matches!(backend.state(), BackendState::Idle { .. }) {
            finished = true;
            break;
        }
    }
    // Drain any events left after the state flipped to `Idle`.
    model.absorb(&mut backend);
    for line in model.lines() {
        println!("{line:?}");
    }

    assert!(
        finished,
        "the run did not return to idle within the deadline"
    );
    // A `/run`'s own outcome arrives as `RunEvent::Finished`, mapped to a
    // `Line::Notice` ("run <id> ended completed: N verified"), not a
    // `Line::Orchestrator` reply (`orchestrator_host.rs::map`) -- replies are
    // for notes sent to a run already in progress. Assert on that notice text
    // instead of an orchestrator line, which this event never produces.
    let ended_completed = model
        .lines()
        .iter()
        .any(|l| matches!(l, Line::Notice(t) if t.contains("ended completed")));
    assert!(
        ended_completed,
        "the box should have shown the run ending completed: {:?}",
        model.lines()
    );
    let had_error = model.lines().iter().any(|l| matches!(l, Line::Error(_)));
    assert!(
        !had_error,
        "the box showed an error line: {:?}",
        model.lines()
    );

    if let Some(run) = &run_id
        && let Some(spend) = factory.run_spend(run)
    {
        println!("run {run}: {}", spend.text);
        assert!(spend.micros > 0, "a real provider call has nonzero spend");
    }

    // The worker writes inside its own git worktree and the run merges that
    // work onto an integration branch (`kgraf/<run id>`); the project's
    // checked-out working tree (`master` here) is never touched by a run, so
    // the merged content is read with `git show`, not from the file on disk.
    let run = run_id.as_deref().expect("a run id was observed");
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
        notes.to_lowercase().contains("quasar"),
        "the worker's write_file call should have appended the quasar line on \
         {integration_branch}: {notes}"
    );
}
