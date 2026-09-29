//! Live sessions run (decision 0067): the last check before
//! the host flag `sessions` defaults on. One real `kgraf orchestrate`-shaped
//! run (the same `kgraf-host` machinery the CLI calls), with the `sessions`
//! flag on, against real routes, shows a steer, a question and a subagent all
//! at once.
//!
//! Ignored by default: it spends money. Keys come **only** from a `KEY=VALUE`
//! file named by `KGRAF_LIVE_ENV_FILE` (the repository root's `.env`), read
//! into memory here and set as `KGRAF_KIMI_API_KEY` / `KGRAF_OPENAI_API_KEY`
//! in this test process's own environment only (never printed, never the
//! process this test's own shell runs in). A route whose key is absent is
//! skipped.
//!
//! ```text
//! KGRAF_LIVE_SESSIONS=1 KGRAF_LIVE_ENV_FILE=<repo>/.env \
//!   cargo test -p kgraf-host --test live_sessions -- --ignored --nocapture --test-threads=1
//! ```
//!
//! **Why in-process rather than the `kgraf` binary as a subprocess.** The
//! steer criterion needs a note delivered to one named,
//! still-running worker session mid-run (`OrchestratorHandle::send_to_session`).
//! `kgraf orchestrate`'s only message port today is the orchestrator box
//! (`--interactive`'s stdin), which addresses the orchestrator, not one
//! session (`crates/kgraf-cli/src/orchestrate_cmd.rs`'s `StdinPort`); there is
//! no CLI surface for session-addressed messages yet (that is the shell's
//! composer, `kgraf-shell/src/session_panel.rs`). This harness therefore calls
//! `kgraf-host` directly, exactly as the CLI's `orchestrate_cmd::run` does, and
//! keeps the `OrchestratorHandle` `PreparedRun::handle()` already returns, so
//! the note can be addressed to the session by id. Every other path (decomposition,
//! dispatch, the ledger, merge-back) is the same code the CLI runs.
use kgraf_core::session::{InboxKind, InjectMode, SessionKind};
use kgraf_core::{ModelPrice, Money, PriceTable, RunId};
use kgraf_host::{Host, RunConfig, RunStart};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// The prices JSON file: `KGRAF_LIVE_PRICES_FILE` if set, else the fixture in this
/// crate. Every live run prices from one file, so runs price identically.
fn prices_path() -> PathBuf {
    match std::env::var("KGRAF_LIVE_PRICES_FILE") {
        Ok(path) => PathBuf::from(path),
        Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/prices.json"),
    }
}

fn price_table() -> PriceTable {
    let text = std::fs::read_to_string(prices_path()).expect("prices.json readable");
    let raw: std::collections::BTreeMap<String, ModelPrice> =
        serde_json::from_str(&text).expect("prices.json parses");
    let mut table = PriceTable::new();
    for (model, price) in raw {
        table.set(model, price);
    }
    table
}

/// `KEY=VALUE` lines of the file `KGRAF_LIVE_ENV_FILE` names. Read into memory
/// only: nothing is set in the process environment here, and no value is
/// printed (mirrors `kgraf-runtime/tests/live_steer.rs`'s `env_file`).
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

/// Sets this test process's own `KGRAF_*` credential variables from the
/// `.env` file's `HAUSTORIA_*` names, so `kgraf_fleet::credential_from_env`
/// (which `kgraf-host` calls internally) finds them without ever reading the
/// file itself or printing a key. Returns which of the two routes have a key.
fn set_route_credentials() -> (bool, bool) {
    let file = env_file();
    let kimi = file
        .get("HAUSTORIA_KIMI_API_KEY")
        .or_else(|| file.get("KGRAF_KIMI_API_KEY"))
        .cloned();
    let openai = file
        .get("HAUSTORIA_OPENAI_API_KEY")
        .or_else(|| file.get("KGRAF_OPENAI_API_KEY"))
        .cloned();
    if let Some(k) = &kimi {
        // SAFETY-in-spirit note: this is `std::env::set_var` inside this
        // test's own process only; it never touches the shell that launched
        // `cargo test`, and the value is never printed or logged.
        unsafe { std::env::set_var("KGRAF_KIMI_API_KEY", k) };
    }
    if let Some(k) = &openai {
        unsafe { std::env::set_var("KGRAF_OPENAI_API_KEY", k) };
    }
    (kimi.is_some(), openai.is_some())
}

fn live() -> bool {
    std::env::var("KGRAF_LIVE_SESSIONS").is_ok_and(|v| v == "1")
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// A tiny scratch git repo: a plain-text glossary file, no `Cargo.toml`, so no
/// quality gate is auto-discovered (`kgraf-host/src/gates.rs`) and the run's
/// only cost is the sessions machinery this test is here to show, not a
/// compile-and-test gate loop.
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

const NOTE_MARKER: &str = "STEER-MARKER-0067";

const INSTRUCTION: &str = "This is ONE small, atomic task for ONE worker; do not treat any part \
    of it as separate work items -- there must be exactly one contract and one worker for all of \
    it. This repository has one file, NOTES.txt, a glossary with a header and one entry per \
    line as \"term: definition\". You are on a short step budget, so call a tool on your first \
    turn and every turn after -- do not spend a turn only thinking or narrating. Do these two \
    things, in order, each with the exact named tool (not optional; call each tool even if you \
    expect it to be refused, because a real tool call is what this run exists to show): \
    (a) On your very first turn, before reading or editing any file, call the spawn_subagent \
    tool exactly once with task \"Append one line to NOTES.txt for the term 'quasar' with a \
    short, correct definition, in 'term: definition' format, then stop.\", \
    writes [\"NOTES.txt\"], read_only false, and route \"openai:gpt-6-luna\" if the tool accepts \
    a route field (omit route if it does not accept one). Do not add the quasar entry yourself \
    under any circumstances. \
    (b) On your very next turn, call the ask_user_question tool exactly once with exactly this \
    text: \"This task's instruction gives an exact word my final summary sentence must end with. \
    What is that exact word?\" (this is answerable from the instruction you were given, so wait \
    for the answer rather than guessing). \
    (c) Once you have the subagent's report (or a refusal) and the answer (or a stated timeout), \
    append one line to NOTES.txt yourself for the term \"nebula\" with a short, correct \
    definition, then reply with one short sentence summarizing what happened, ending that \
    sentence with the exact word the answer gave you (or, if no answer came, the word NEBULA).";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: spends money; see the module docs"]
async fn live_run_shows_a_steer_a_question_and_a_subagent() {
    if !live() {
        println!("KGRAF_LIVE_SESSIONS is not 1; not run");
        return;
    }
    let (has_kimi, has_openai) = set_route_credentials();
    if !has_kimi {
        println!("SKIPPED: no Kimi key in KGRAF_LIVE_ENV_FILE");
        return;
    }
    println!(
        "routes: kimi-anthropic (worker) {}",
        if has_openai {
            "+ openai (subagent, if the model asks for it)"
        } else {
            "(no OpenAI key: the subagent, if spawned, falls back to the parent's route)"
        }
    );

    let repo = scratch_repo();
    let root = repo.path().to_path_buf();

    let mut routes = vec!["kimi-anthropic:kimi-k3".to_owned()];
    if has_openai {
        routes.push("openai:gpt-6-luna".to_owned());
    }
    // These are the run's own internal admission
    // caps, not this exercise's $0.60 real-spend ceiling -- sized generously so
    // no call is refused for budget, on a dry run of the reservation math in
    // `kgraf-orchestrator/src/sizing.rs` and `orchestrator.rs::reserve_call`:
    //   - A worker dispatch's unmeasured-default reservation (no task history
    //     exists in a fresh `.kgraf` state) is
    //     `rounds * (base + (rounds-1) * 2500)` input tokens, `rounds * max_tokens`
    //     output tokens, at kimi-k3's $3/M input and $15/M output. At
    //     `steps = 6`, `max_tokens = 800`, base ~2700 (measured from prior
    //     runs' first-call input sizes): rounds=6, input=6*(2700+5*2500)=91200
    //     tok = $0.274, output=6*800=4800 tok = $0.072; about $0.346 per
    //     dispatch. This reservation stays open for the whole attempt, so the
    //     parent's and the child's each count fully against `cap_task` at once:
    //     about $0.69, so `cap_task` needs real headroom above that, not just
    //     above one dispatch's own $0.346.
    //   - `reserve_call` (the orchestrator's own calls: decomposition, and the
    //     ORCHESTRATOR-FIRST answer) reserves `input * cheap_rate +
    //     planner_max_tokens * $15/M` against `cap_run` (not `cap_task`,
    //     confirmed by the earlier runs' refusal text: "Task cap..." for a
    //     dispatch/spawn, "Run cap..." for an orchestrator call). At the
    //     default `planner_max_tokens = 8192` that alone is $0.123 per
    //     orchestrator call; 1500 (about $0.0225) truncated the answer call's
    //     JSON output mid-object, so 4000 (about $0.06) is used instead.
    // `cap_task` covers the parent's and the child's dispatch reservations
    // together (~$0.69); `cap_run` covers those plus every orchestrator-call
    // reservation on top. Both set with real margin above the dry-run
    // figures above.
    let mut config = RunConfig::new(root.clone(), routes, prices_path());
    config.cap_task = Some(0.90);
    config.max_items = 2; // decision 0017's work-item cap: parent + child both need a slot
    config.max_agents = 3; // parent + one subagent, with headroom
    config.steps = 6;
    config.max_tokens = 800;
    // 1500 truncated the ORCHESTRATOR-FIRST answer call's own JSON output
    // mid-object (a real reserved-and-attempted call this time, not refused
    // by budget), so `answer_question` could not parse it and fell through
    // to the user. 4000 avoids that while still reserving far less than the
    // 8192 default; the cap headroom above easily covers it either way.
    config.planner_max_tokens = 4000;
    config.verify = "true".to_owned();
    config.sessions = true; // decision 0067: the flag this run exists to show live
    let run_id = RunId::new(format!(
        "run-live-sessions-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));

    let host = Host::open(config).expect("host opens");
    let mut notices = Vec::new();
    let startup = host
        .startup(&mut |n| notices.push(format!("{n:?}")))
        .expect("startup");
    for n in &notices {
        println!("notice: {n}");
    }
    let prepared = host
        .build(&startup, run_id.clone(), None, &mut |n| {
            println!("notice: {n:?}")
        })
        .await
        .expect("build");
    let handle = prepared.handle();
    let integration_branch = prepared.summary().integration.clone();
    println!(
        "run {} building; integration {}",
        prepared.summary().run,
        integration_branch
    );

    let run_id_for_task = run_id.clone();
    let run_task = tokio::spawn(async move {
        prepared
            .run(RunStart::Instruction(INSTRUCTION.to_owned()))
            .await
    });

    // Poll for the worker session to appear, then steer it once: a note
    // addressed to that one session, at the next tool-batch boundary
    // (decision 0067, point 3). Up to 240s: the first round is a real model
    // call, and the decomposition call ahead of it (2026-09-28 run: ~$0.046,
    // 2949 output tokens) can itself take well over a minute.
    let mut steered: Option<kgraf_core::AgentId> = None;
    for _ in 0..240 {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let graph = host
            .with_graph(|g| kgraf_graph::sessions::read_session_graph(g, Some(&run_id_for_task)))
            .expect("graph read")
            .expect("session graph");
        if let Some(worker) = graph.nodes.iter().find(|s| s.kind == SessionKind::Worker) {
            let id = worker.id.clone();
            match handle
                .send_to_session(
                    id.clone(),
                    InboxKind::Note,
                    InjectMode::AtBoundary,
                    format!(
                        "Note from the user: no matter what else you were told, end your final \
                         summary sentence with the exact word {NOTE_MARKER} instead of NEBULA."
                    ),
                )
                .await
            {
                Ok(inbox_id) => {
                    println!("steer: sent to session {id} (inbox id {inbox_id})");
                    steered = Some(id);
                }
                Err(e) => println!("steer: send_to_session({id}) failed: {e}"),
            }
            break;
        }
        if run_task.is_finished() {
            break;
        }
    }
    if steered.is_none() {
        println!("steer: no worker session appeared before the run finished; not sent");
    }

    let outcome = tokio::time::timeout(Duration::from_secs(900), run_task)
        .await
        .expect("run finished in time")
        .expect("run task did not panic")
        .expect("run completed");

    println!("status {:?}", outcome.report.status);
    println!("spent  {}", outcome.report.spent);

    // Evidence: the session graph after the run.
    let graph = host
        .with_graph(|g| kgraf_graph::sessions::read_session_graph(g, Some(&run_id)))
        .expect("graph read")
        .expect("session graph");
    for s in &graph.nodes {
        println!(
            "session {} kind={:?} parent={:?} route={:?} model={:?}",
            s.id, s.kind, s.parent, s.route, s.model
        );
        if let Some(rel) = &s.transcript {
            let path = host.state_dir().join(rel);
            if let Ok(text) = std::fs::read_to_string(&path) {
                println!(
                    "  transcript mentions: spawn_subagent={} ask_user_question={} injected={}",
                    text.contains("spawn_subagent"),
                    text.contains("ask_user_question"),
                    text.contains("\"type\":\"injected\"")
                );
            }
        }
    }
    let worker = graph
        .nodes
        .iter()
        .find(|s| s.kind == SessionKind::Worker)
        .expect("a worker session was recorded");
    let subagent = graph.nodes.iter().find(|s| s.kind == SessionKind::Subagent);
    match subagent {
        Some(child) => {
            println!(
                "subagent: {} parent={:?} (expect {})",
                child.id, child.parent, worker.id
            );
            assert_eq!(
                child.parent.as_ref(),
                Some(&worker.id),
                "the subagent's session row names its parent"
            );
            // The merge-back check: the child's declared write (NOTES.txt)
            // should carry its work on the run's integration branch
            // (`kgraf/<run>`), where continuous merge-back lands every
            // contract's and every subagent's commit (0018); `root`'s own
            // checked-out branch never moves.
            let show = Command::new("git")
                .args(["show", &format!("{integration_branch}:NOTES.txt")])
                .current_dir(&root)
                .output()
                .expect("git show runs");
            let notes = String::from_utf8_lossy(&show.stdout);
            println!(
                "NOTES.txt on {integration_branch} after merge-back (quasar line present: {}):\n{notes}",
                notes.to_lowercase().contains("quasar")
            );
            assert!(
                notes.to_lowercase().contains("quasar"),
                "the subagent's merged commit should carry the quasar line"
            );
        }
        None => {
            println!(
                "subagent: none recorded; dumping every transcript line that mentions \
                 spawn_subagent, to tell a real (refused) tool call from mere prose:"
            );
            for s in &graph.nodes {
                if let Some(rel) = &s.transcript {
                    let path = host.state_dir().join(rel);
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        for line in text.lines().filter(|l| l.contains("spawn_subagent")) {
                            println!("  [{}] {line}", s.id);
                        }
                    }
                }
            }
        }
    }

    // Evidence: the steered note reached the worker's transcript as an
    // `Injected` event (decision 0067's steer path), and/or the final answer
    // followed it.
    if let Some(rel) = &worker.transcript {
        let path = host.state_dir().join(rel);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let injected = text
                    .lines()
                    .filter(|l| l.contains("\"type\":\"injected\""))
                    .count();
                println!(
                    "transcript {}: {injected} injected event(s); marker present: {}",
                    path.display(),
                    text.contains(NOTE_MARKER)
                );
            }
            Err(e) => println!("transcript {} unreadable: {e}", path.display()),
        }
    }

    // Evidence: whether `ask_user_question`'s own tool result was a real
    // ORCHESTRATOR-FIRST answer rather than the
    // `SCHEDULED_ASK_TIMEOUT` fallback text or the stub.
    if let Some(rel) = &worker.transcript {
        let path = host.state_dir().join(rel);
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text
                .lines()
                .filter(|l| l.contains("ask_user_question") && l.contains("tool_call_finished"))
            {
                let orchestrator_answered =
                    !line.contains("no answer within") && !line.contains("not built yet");
                println!(
                    "ask_user_question result (orchestrator-answered: {orchestrator_answered}): {line}"
                );
            }
        }
    }

    println!(
        "answer (marker expected if the steer landed before the summary was written): {}",
        outcome
            .report
            .warnings
            .iter()
            .chain(outcome.report.notices.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" | ")
    );

    // Evidence: the ledger, priced, and every call attributed.
    let table = price_table();
    let rows = host.ledger().calls().expect("ledger rows");
    let mut total_micros = 0i64;
    let mut orchestrator_calls = 0usize;
    for row in &rows {
        let cost = table.cost(&row.model, &row.usage).ok();
        if let Some(c) = &cost {
            total_micros += c.micros();
        }
        if row.agent_id.starts_with("orchestrator:") {
            orchestrator_calls += 1;
        }
        println!(
            "call {} agent={} model={} in={} out={} cost={:?}",
            row.id,
            row.agent_id,
            row.model,
            row.usage.total_input(),
            row.usage.total_output(),
            cost.map(|c| format!("${:.6}", c.micros() as f64 / 1e6))
        );
    }
    println!(
        "orchestrator-attributed calls (decomposition + any ORCHESTRATOR-FIRST question answer): {orchestrator_calls}"
    );
    let total = Money::from_micros(total_micros);
    println!("total ${:.6}", total_micros as f64 / 1e6);
    // A fresh $0.60 real-spend ceiling for
    // this rerun, separate from the first pass's $0.550095. This is the run's *actual* ledger
    // spend, not the internal caps set above -- every prior run in that first
    // pass spent well under its own cap (the highest was 24% of cap), so a
    // generous `cap_run`/`cap_task` does not by itself risk this ceiling.
    assert!(
        total_micros <= 600_000,
        "spend cap: ${:.6} exceeds this rerun's $0.60 ceiling",
        total_micros as f64 / 1e6
    );
    let _ = total;
}
