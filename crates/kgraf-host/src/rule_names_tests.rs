//! A permission rule naming an MCP tool is accepted when it is read and
//! checked at run start, once the servers have listed their tools: a rule on a
//! tool the server lists says nothing, and a rule no worker tool matches is
//! said as a notice and traced. Through a real hosted run with the real MCP
//! echo server over stdio; the model is a script (the shortfall tests'
//! worker, which writes `a.txt`). A child of `tests.rs`, for its fixtures.
//!
//! How to run: `cargo test -p kgraf-host rule_names`. Builds
//! `mcp-echo-server` once; no network.

use super::shortfall::{Script, Worker};
use super::*;

/// A control in Manual mode with the user-file text `toml`.
fn control_from(toml: &str) -> Arc<kgraf_tools::PermissionControl> {
    Arc::new(
        kgraf_tools::PermissionControl::new(
            kgraf_tools::PermissionSettings::from_toml_str(toml).expect("settings"),
            kgraf_tools::ModeSource::CommandLine,
            kgraf_tools::permissions::ProcessFacts::default(),
        )
        .expect("control"),
    )
}

fn unmatched(said: &[crate::Notice]) -> Vec<String> {
    said.iter()
        .filter_map(|n| match n {
            crate::Notice::RuleMatchesNoTool(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// The echo server lists `echo`, so a deny rule on `mcp__echoserver__echo` is
/// accepted and says nothing; a rule on a tool it does not list, and one on a
/// server this run does not configure, are each said once at run start and
/// traced, and the run goes on. Every built-in tool the run's workers have is
/// one a rule may name ([`kgraf_tools::permissions::BUILTIN_TOOLS`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_mcp_rule_is_checked_against_the_tools_the_servers_list_at_run_start() {
    let (_dir, mut config) = project();
    config.routes = vec!["mine:llama3".into()];
    config.verify = "test -f a.txt".into();
    let root = config.root.clone();
    let state = root.join(".kgraf");
    std::fs::create_dir_all(&state).expect("state");
    let worker = serde_json::json!({"mcp_servers": [{
        "name": "echoserver",
        "transport": {"type": "stdio", "command": echo_binary().to_string_lossy()}
    }]});
    std::fs::write(state.join("worker.json"), worker.to_string()).expect("worker.json");
    let rules = control_from(
        "deny = [\"mcp__echoserver__echo\", \"mcp__echoserver__nosuch\", \"Bash(rm -rf*)\", \
         \"WebFetch\"]\n\
         ask = [\"mcp__other\"]\n",
    );
    let host = Host::open(config)
        .expect("open")
        .with_permissions(rules)
        .with_route_provider("mine", Script::new(Worker::Writes))
        .expect("a used route");
    let mut said = Vec::new();
    let id = "rn-mcp";
    let outcome = host
        .prepare(RunId::new(id), None, &mut |n| said.push(n))
        .await
        .expect("prepare")
        .run(RunStart::Instruction("copy README.md to a.txt".into()))
        .await
        .expect("run");
    let mut lines = unmatched(&said);
    assert_eq!(lines.len(), 3, "{lines:#?}");
    // A Claude Code tool kGraf lacks is said with what to write instead.
    let web = lines.remove(1);
    assert!(
        web.starts_with(
            "your deny rule `WebFetch` matches no call, so it blocks nothing. kGraf \
                         has no WebFetch tool."
        ),
        "{web}"
    );
    assert!(web.contains("`shell(curl*)`"), "{web}");
    assert!(
        lines[0].starts_with("your deny rule `mcp__echoserver__nosuch` names no tool"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].starts_with("your ask rule `mcp__other` names no tool"),
        "{}",
        lines[1]
    );
    let (_, tools) = lines[0]
        .split_once("The workers' tools are: ")
        .expect("the tool list");
    let tools: Vec<&str> = tools.split(", ").collect();
    assert!(
        tools.contains(&"mcp__echoserver__echo"),
        "the server connected and listed `echo`: {tools:?}"
    );
    for t in tools.iter().filter(|t| !t.starts_with("mcp__")) {
        assert!(
            kgraf_tools::permissions::BUILTIN_TOOLS.contains(t),
            "`{t}` is a worker tool a rule cannot name"
        );
    }
    let trace = std::fs::read_to_string(root.join(".kgraf/runs").join(id).join("trace.jsonl"))
        .expect("trace");
    for line in lines.iter().chain(std::iter::once(&web)) {
        let traced = trace
            .lines()
            .filter(|l| l.contains(r#""event":"notice""#) && l.contains(&line[..60]))
            .count();
        assert_eq!(traced, 1, "{line}\n{trace}");
    }
    assert_eq!(
        outcome.report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "a notice never stops the run: {:?}\n{}",
        outcome.report,
        super::failed_verdicts(&trace)
    );
}

/// Every built-in tool name the crates define is one a rule may name, so no
/// rule on a real tool is refused as unknown.
#[test]
fn every_builtin_tool_name_is_one_a_rule_may_name() {
    let known = kgraf_tools::permissions::BUILTIN_TOOLS;
    let mut names: Vec<&str> = vec![
        kgraf_runtime::SHORTFALL_TOOL,
        kgraf_runtime::RECALL_TOOL,
        kgraf_session::skills::SKILL_TOOL,
        kgraf_orchestrator::CHALLENGE_TOOL,
    ];
    names.extend(kgraf_orchestrator::SESSION_TOOL_NAMES.iter().copied());
    names.extend([
        kgraf_tools::DECIDE_TOOL,
        kgraf_tools::EMBED_TOOL,
        kgraf_tools::RERANK_TOOL,
    ]);
    for n in names {
        assert!(known.contains(&n), "`{n}` is missing from BUILTIN_TOOLS");
        kgraf_tools::permissions::Rule::parse(n).expect(n);
    }
}
