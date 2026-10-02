//! The warning about a verify command that runs a program the user's rules
//! block for workers, through a real hosted run. The model is a script (the
//! shortfall tests' worker, which writes `a.txt` with `apply_patch`); the host,
//! the orchestrator, the approval chain and the gate are real. A child of
//! `tests.rs`, for its fixtures.

use super::shortfall::{Script, Worker};
use super::*;

/// A control in Manual mode with `deny` and `ask` rules.
fn ruled(deny: &[&str], ask: &[&str]) -> Arc<kgraf_tools::PermissionControl> {
    use kgraf_tools::permissions::{Rule, UserRules};
    let parse = |v: &[&str]| v.iter().map(|r| Rule::parse(r).expect("rule")).collect();
    Arc::new(
        kgraf_tools::PermissionControl::new(
            kgraf_tools::PermissionSettings {
                rules: UserRules {
                    deny: parse(deny),
                    ask: parse(ask),
                },
                ..kgraf_tools::PermissionSettings::default()
            },
            kgraf_tools::ModeSource::CommandLine,
            kgraf_tools::permissions::ProcessFacts::default(),
        )
        .expect("control"),
    )
}

/// One hosted run whose `verify` is the user's command, with the warning `warn`
/// and the user's `rules`. Returns the notices the build said, the trace text
/// and the report.
async fn run_warned(
    warn: bool,
    rules: Arc<kgraf_tools::PermissionControl>,
    verify: &str,
    id: &str,
) -> (Vec<crate::Notice>, String, kgraf_orchestrator::RunReport) {
    let (_dir, mut config) = project();
    config.routes = vec!["mine:llama3".into()];
    config.verify = verify.into();
    config.warn_rule_conflicts = warn;
    let root = config.root.clone();
    let host = Host::open(config)
        .expect("open")
        .with_permissions(rules)
        .with_route_provider("mine", Script::new(Worker::Writes))
        .expect("a used route");
    let mut said = Vec::new();
    let outcome = host
        .prepare(RunId::new(id), None, &mut |n| said.push(n))
        .await
        .expect("prepare")
        .run(RunStart::Instruction("copy README.md to a.txt".into()))
        .await
        .expect("run");
    let trace = std::fs::read_to_string(root.join(".kgraf/runs").join(id).join("trace.jsonl"))
        .expect("trace");
    (said, trace, outcome.report)
}

fn said_sentences(said: &[crate::Notice]) -> Vec<String> {
    said.iter()
        .filter_map(|n| match n {
            crate::Notice::RuleConflict(c) => Some(c.sentence()),
            _ => None,
        })
        .collect()
}

fn warning_traces(trace: &str) -> usize {
    trace
        .lines()
        .filter(|l| l.contains(r#""event":"notice""#) && l.contains("your rules block"))
        .count()
}

/// Setting on, verify runs a denied program: one line is said and traced, with
/// the owner's wording, and the run goes on and completes (the command runs).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_denied_verify_program_is_said_and_traced_and_the_run_still_completes() {
    let (said, trace, report) = run_warned(
        true,
        ruled(&["shell(cat*)"], &[]),
        "test -f a.txt && cat a.txt",
        "rc-on",
    )
    .await;
    let expected = "Your verify command runs cat and your rules block cat for workers. kGraf will \
                    continue to block workers from accessing cat and will run your verify \
                    command directly, without AI.";
    assert_eq!(said_sentences(&said), [expected]);
    assert_eq!(warning_traces(&trace), 1, "{trace}");
    assert!(trace.contains(expected), "{trace}");
    assert_eq!(
        report.status,
        kgraf_orchestrator::RunStatus::Completed,
        "the warning never stops the run: {report:?}"
    );
    assert_eq!(report.verified, vec!["a".to_owned()], "the command ran");
}

/// Off (the default): nothing is said and nothing is traced, whatever the
/// rules.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_setting_off_nothing_is_said_or_traced() {
    assert!(!RunConfig::new("/x", vec![], "p").warn_rule_conflicts);
    let (said, trace, _) = run_warned(
        false,
        ruled(&["shell(cat*)"], &[]),
        "test -f a.txt && cat a.txt",
        "rc-off",
    )
    .await;
    assert!(said_sentences(&said).is_empty(), "{said:?}");
    assert_eq!(warning_traces(&trace), 0, "{trace}");
}

/// A program no rule covers says nothing, with the setting on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uncovered_verify_program_says_nothing() {
    let (said, trace, _) = run_warned(
        true,
        ruled(&["shell(curl*)"], &["shell(wget*)"]),
        "test -f a.txt && cat a.txt",
        "rc-none",
    )
    .await;
    assert!(said_sentences(&said).is_empty(), "{said:?}");
    assert_eq!(warning_traces(&trace), 0, "{trace}");
}

/// A chained verify command is split: the second part matches an ask rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chained_verify_command_matches_on_its_second_part() {
    let (said, trace, _) = run_warned(
        true,
        ruled(&[], &["shell(cat*)"]),
        "test -f a.txt && cat a.txt",
        "rc-chain",
    )
    .await;
    let lines = said_sentences(&said);
    assert_eq!(lines.len(), 1, "{said:?}");
    assert!(
        lines[0].starts_with("Your verify command runs cat and"),
        "{}",
        lines[0]
    );
    assert_eq!(warning_traces(&trace), 1, "{trace}");
}
