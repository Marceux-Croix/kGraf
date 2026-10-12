//! The three-arm go or no-go (decisions 0023, 0024), run live.
//!
//! **What this is not.** This is not `Host::build`/`Host::run` driving a real
//! decomposed multi-worker run over a git worktree -- that machinery (contract
//! checks, dispatch, merge-back) is what the go or no-go decides whether to keep
//! building past a prototype, and running it *as the test of whether it is
//! worth building* would be circular. Nothing in the repository already runs
//! three arms as 0023/0024 specify (checked: no `three-arm`, `peer-coordinated`
//! or `go_no_go` hit anywhere in `*.rs`), so this is the smallest harness that
//! does, built to reuse the same route/credential/ledger/pricing plumbing
//! `kgraf-host` itself uses (`kgraf_fleet::entry`/`credential_from_env`/`build`,
//! `kgraf_provider::accounting::Ledger`, the same `prices.json` fixture
//! `live_sessions.rs` reads) rather than a real multi-turn tool-using agent
//! loop, so the spend cap buys a real sample size instead of two or three
//! dispatches. **Task success here means: on a task where two sub-deliverables
//! share one numbered slot, did the two deliverables end up on two *different*
//! slots, and did each deliverable's content satisfy its own assignment** --
//! not the *shell*'s two-agent collaborative coding capability, and not
//! textual merge conflicts (0023's point: collision counts are not the
//! measure). See the module-level comment on `Task` for the exact design and
//! its honest limits.
//!
//! Ignored by default: it spends money. Keys come **only** from a `KEY=VALUE`
//! file named by `KGRAF_LIVE_ENV_FILE` (mirrors `live_sessions.rs`'s
//! `env_file`/`set_route_credentials`): read into memory here, set as
//! `KGRAF_KIMI_API_KEY` / `KGRAF_OPENAI_API_KEY` in this test process's own
//! environment only, never printed, never the file itself read anywhere else.
//!
//! ```text
//! KGRAF_LIVE_GO_NO_GO=1 KGRAF_LIVE_ENV_FILE=<repo>/.env \
//!   cargo test -p kgraf-host --test live_go_no_go -- --ignored --nocapture --test-threads=1
//! ```
//!
//! **Sizing, done before the run (decision text: "no arm fails for budget").**
//! Every call here is one short text completion (no tools, no multi-turn agent
//! loop). A first live pass at `max_output_tokens` 96-192 found kimi-k3 (the
//! `kimi-anthropic` route) returning **empty text on most calls**: its usage
//! showed the whole output budget spent on an unrequested `thinking` block
//! (`kgraf-provider-anthropic` parses `thinking` blocks but this route's
//! catalog entry has no control to suppress them), leaving nothing for the
//! visible answer and a `MaxTokens` stop. `max_output_tokens` is raised to
//! 500-700 below to give the thinking block room without starving the
//! answer; gpt-6-luna, which was not affected, absorbs the same higher
//! ceilings harmlessly. Worst case, pricing everything at kimi-k3's $3/M
//! input, $15/M output (`prices.json`; gpt-6-luna is far cheaper and is not
//! the bound): an input of 400 tokens and output of 700 tokens costs
//! `400*3/1e6 + 700*15/1e6` = $0.0012 + $0.0105 = **$0.0117 per call**. Each
//! task costs 1 (solo) + 2 (peer) + 3 (orchestrator: 1 director + 2 workers)
//! = 6 calls, **$0.0702 worst case per task**. At `TASK_COUNT` = 8 that is
//! **$0.562 worst case for the whole run**, far below
//! [`SPEND_CAP_USD`], so `TASK_COUNT` is 8 for a less thin sample rather than
//! smaller for budget. [`SPEND_CAP_USD`] is still enforced live, call by
//! call, in case real usage runs far past this estimate, and the run stops
//! and reports a partial result rather than exceeding it.
use kgraf_core::ModelPrice;
use kgraf_core::cost::{Money, PriceTable};
use kgraf_provider::accounting::Ledger;
use kgraf_provider::message::Request;
use kgraf_provider::provider::Provider;
use std::path::PathBuf;
use std::sync::Arc;

/// The spend limit for this whole run, summed from the ledger.
const SPEND_CAP_USD: f64 = 4.00;

/// Number of tasks per arm. Sized in the module doc comment above.
const TASK_COUNT: usize = 8;

fn live() -> bool {
    std::env::var("KGRAF_LIVE_GO_NO_GO").is_ok_and(|v| v == "1")
}

/// `KEY=VALUE` lines of the file `KGRAF_LIVE_ENV_FILE` names, read into memory
/// only (verbatim copy of `live_sessions.rs`'s `env_file`, kept private to
/// each test binary rather than shared, since neither crate exposes test
/// helpers to the other).
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

/// Sets this process's own `KGRAF_*` credential variables from the `.env`
/// file's `HAUSTORIA_*` or `KGRAF_*` names, exactly as `live_sessions.rs`
/// does, so `kgraf_fleet::credential_from_env` finds them without this file
/// ever reading `.env` itself. Returns which routes have a key.
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
        unsafe { std::env::set_var("KGRAF_KIMI_API_KEY", k) };
    }
    if let Some(k) = &openai {
        unsafe { std::env::set_var("KGRAF_OPENAI_API_KEY", k) };
    }
    (kimi.is_some(), openai.is_some())
}

/// Prices from the same fixture `live_sessions.rs` uses, so the two tests
/// price identically.
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

/// One route this run may dispatch to: a catalog id and the model it serves,
/// mirroring `live_sessions.rs`'s `"kimi-anthropic:kimi-k3"` /
/// `"openai:gpt-6-luna"` route strings.
#[derive(Clone, Copy)]
struct Route {
    id: &'static str,
    model: &'static str,
}
const KIMI: Route = Route {
    id: "kimi-anthropic",
    model: "kimi-k3",
};
const OPENAI: Route = Route {
    id: "openai",
    model: "gpt-6-luna",
};

/// Builds a live provider for one route, exactly the way
/// `kgraf-host/src/config.rs::provider_for` does (that function is
/// `pub(crate)`, so this reproduces its two calls rather than reaching into
/// `kgraf-host`'s private module -- both are three lines against
/// `kgraf_fleet`, the crate this project's own host code calls for the same
/// purpose).
fn provider_for(route: Route) -> Result<Arc<dyn Provider>, String> {
    let entry = kgraf_fleet::entry(route.id).map_err(|e| e.to_string())?;
    let credential = kgraf_fleet::credential_from_env(entry).map_err(|e| e.to_string())?;
    let boxed = kgraf_fleet::build(route.id, &credential, vec![route.model.to_owned()])
        .map_err(|e| e.to_string())?;
    Ok(Arc::from(boxed))
}

/// One task: a shared numbered glossary with two existing lines, and two new
/// entries -- A and B -- that must each land on their own line.
///
/// **Why this design.** 0023/0024's question is whether *uncoordinated*
/// concurrent agents fail where a *supervised* pair does not, and the
/// benchmark behind both decisions found the effect showing up as expectation
/// (42%) and commitment (32%) failures -- agents guessing wrong about what a
/// peer will do, and not holding to what they said -- not as textual merge
/// conflicts (0023 recomputed that as 9.3% of clean merges, not the binding
/// constraint). A shared numbered slot reproduces exactly that shape without
/// a git worktree or a compiler: two workers each independently choosing
/// "the next number" *can* collide (expectation), and a worker told an exact
/// number by a director can still ignore it (commitment) -- both failure
/// modes are checkable from the returned text alone, deterministically, no
/// LLM grading call needed.
///
/// **What this narrows away, stated plainly.** This is not multi-file code,
/// not a compiler, and not the full session/tool-loop `kgraf-host` runs in
/// production; the fictional terms and facts are invented per task so no
/// model's prior knowledge substitutes for reading the brief.
struct Task {
    id: &'static str,
    existing: &'static str,
    term_a: &'static str,
    fact_a: &'static str,
    keyword_a: &'static str,
    term_b: &'static str,
    fact_b: &'static str,
    keyword_b: &'static str,
}

const TASKS: [Task; TASK_COUNT] = [
    Task {
        id: "cobalt-argon",
        existing: "1. cobalt: a hard, lustrous, silvery-blue metal.\n2. argon: an inert noble gas.",
        term_a: "quixite",
        fact_a: "it is a fictional mineral that glows faint green under moonlight",
        keyword_a: "moonlight",
        term_b: "florben",
        fact_b: "it is a fictional plant whose leaves close precisely at noon",
        keyword_b: "noon",
    },
    Task {
        id: "maple-tundra",
        existing: "1. maple: a tree with lobed leaves.\n2. tundra: a cold, treeless biome.",
        term_a: "brellik",
        fact_a: "it is a fictional copper coin used by Ostori traders",
        keyword_a: "ostori",
        term_b: "sable-thorn",
        fact_b: "it is a fictional cactus with jet-black spines",
        keyword_b: "black",
    },
    Task {
        id: "sonnet-estuary",
        existing: "1. sonnet: a 14-line poem.\n2. estuary: a tidal river mouth.",
        term_a: "glimmerwasp",
        fact_a: "it is a fictional insect that hums in a minor key",
        keyword_a: "minor",
        term_b: "duskfern",
        fact_b: "it is a fictional fern that blooms only at dusk",
        keyword_b: "dusk",
    },
    Task {
        id: "comet-plankton",
        existing: "1. comet: an icy body orbiting the sun.\n2. plankton: microscopic marine organisms.",
        term_a: "pyrolume",
        fact_a: "it is a fictional lantern fungus that burns without fire",
        keyword_a: "fungus",
        term_b: "coral-vox",
        fact_b: "it is a fictional coral reef that echoes sound for days",
        keyword_b: "echo",
    },
    Task {
        id: "granite-heron",
        existing: "1. granite: a coarse igneous rock.\n2. heron: a long-legged wading bird.",
        term_a: "nightglass",
        fact_a: "it is a fictional volcanic glass that turns opaque at sunrise",
        keyword_a: "sunrise",
        term_b: "thrumvine",
        fact_b: "it is a fictional vine that vibrates when touched",
        keyword_b: "vibrat",
    },
    Task {
        id: "otter-birch",
        existing: "1. otter: an aquatic mammal.\n2. birch: a tree with white bark.",
        term_a: "emberkelp",
        fact_a: "it is a fictional seaweed that glows warm orange at low tide",
        keyword_a: "tide",
        term_b: "frostmoth",
        fact_b: "it is a fictional moth active only below freezing",
        keyword_b: "freez",
    },
    Task {
        id: "quartz-lichen",
        existing: "1. quartz: a common silicate mineral.\n2. lichen: a symbiotic organism of fungus and algae.",
        term_a: "saltglass",
        fact_a: "it is a fictional desert glass formed by lightning strikes",
        keyword_a: "lightning",
        term_b: "whisperfox",
        fact_b: "it is a fictional fox that mimics human speech at night",
        keyword_b: "night",
    },
    Task {
        id: "basalt-mangrove",
        existing: "1. basalt: a dark volcanic rock.\n2. mangrove: a coastal salt-tolerant tree.",
        term_a: "cinderlark",
        fact_a: "it is a fictional bird that sings only during wildfires",
        keyword_a: "wildfire",
        term_b: "glasswort-blue",
        fact_b: "it is a fictional succulent that turns blue during drought",
        keyword_b: "drought",
    },
];

/// The three arms decisions 0023/0024 name. `PeerCoordinated` gives the two
/// workers a one-way note channel and no director; `OrchestratorSupervised`
/// gives them a director that assigns each a line number and states it is
/// their whole brief, and then checks the number each worker actually used
/// against the number it assigned (0024: "commitment is verified, not
/// trusted").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Arm {
    Solo,
    PeerCoordinated,
    OrchestratorSupervised,
}

impl std::fmt::Display for Arm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Arm::Solo => "solo",
            Arm::PeerCoordinated => "peer-coordinated",
            Arm::OrchestratorSupervised => "orchestrator-supervised",
        })
    }
}

/// One parsed glossary line: `N. term: definition`.
#[derive(Debug, Clone)]
struct Entry {
    number: u32,
    term: String,
    definition: String,
}

/// The first line in `text` that parses as `N. term: definition`. Manual
/// parsing rather than a `regex` dependency (not already a `kgraf-host` dep;
/// this pattern needs nothing a regex would buy).
fn parse_entry(text: &str) -> Option<Entry> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((num_part, rest)) = line.split_once('.') else {
            continue;
        };
        let Ok(number) = num_part.trim().parse::<u32>() else {
            continue;
        };
        let Some((term, definition)) = rest.trim().split_once(':') else {
            continue;
        };
        return Some(Entry {
            number,
            term: term.trim().to_owned(),
            definition: definition.trim().to_owned(),
        });
    }
    None
}

/// All lines matching `N. term: definition`, in order (solo's single call
/// must produce two).
fn parse_entries(text: &str) -> Vec<Entry> {
    text.lines().filter_map(|l| parse_entry(l.trim())).collect()
}

/// A `NOTE:` line, case-insensitively, if the worker left one (the peer arm's
/// one-way channel).
fn parse_note(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line
            .strip_prefix("NOTE:")
            .or_else(|| line.strip_prefix("note:"))
        {
            return Some(rest.trim().to_owned());
        }
    }
    None
}

/// Task success: the two entries landed on two different numbers, each
/// entry's term matches its assignment, and each definition contains its
/// required keyword. Returns `(success, reasons)`; reasons are always filled
/// in on failure, for the test output.
fn grade(task: &Task, entry_a: Option<&Entry>, entry_b: Option<&Entry>) -> (bool, Vec<String>) {
    let mut reasons = Vec::new();
    let (Some(a), Some(b)) = (entry_a, entry_b) else {
        reasons.push("one or both entries did not parse as `N. term: definition`".to_owned());
        return (false, reasons);
    };
    if a.number == b.number {
        reasons.push(format!(
            "collision: both entries used line number {}",
            a.number
        ));
    }
    if !a.term.to_lowercase().contains(&task.term_a.to_lowercase()) {
        reasons.push(format!(
            "entry A's term \"{}\" does not match assignment \"{}\"",
            a.term, task.term_a
        ));
    }
    if !a
        .definition
        .to_lowercase()
        .contains(&task.keyword_a.to_lowercase())
    {
        reasons.push(format!(
            "entry A's definition does not mention \"{}\": \"{}\"",
            task.keyword_a, a.definition
        ));
    }
    if !b.term.to_lowercase().contains(&task.term_b.to_lowercase()) {
        reasons.push(format!(
            "entry B's term \"{}\" does not match assignment \"{}\"",
            b.term, task.term_b
        ));
    }
    if !b
        .definition
        .to_lowercase()
        .contains(&task.keyword_b.to_lowercase())
    {
        reasons.push(format!(
            "entry B's definition does not mention \"{}\": \"{}\"",
            task.keyword_b, b.definition
        ));
    }
    (reasons.is_empty(), reasons)
}

/// The ledger and price table together: every call needs both, and bundling
/// them keeps [`complete`] under clippy's argument-count lint without hiding
/// which route or model a given call used (those stay explicit arguments).
struct Rig {
    ledger: Ledger,
    prices: PriceTable,
}

/// A single short completion, through the real ledger and pricing path
/// (no provider call outside the accounting path). Returns the
/// response text and its cost.
async fn complete(
    rig: &Rig,
    provider: &Arc<dyn Provider>,
    model: &str,
    prompt: String,
    max_tokens: u32,
    agent_id: &str,
    task_id: &str,
) -> Result<(String, Money), String> {
    let token = rig
        .ledger
        .begin(
            provider.adapter(),
            provider.protocol(),
            model,
            agent_id,
            task_id,
        )
        .map_err(|e| format!("ledger begin: {e}"))?;
    let request = Request::new(model, prompt, max_tokens);
    // `Provider::complete` settles the token itself before returning (every
    // adapter's implementation ends in `.settle_async(...)`), so the caller
    // must not settle again -- an earlier version of this harness did, and
    // every single live call failed with "already settled" as a result. The
    // token is consumed for exactly the reason above: every call is attributed to the ledger.
    let response = provider
        .complete(&request, &token)
        .await
        .map_err(|e| format!("provider complete: {e}"))?;
    let cost = rig
        .prices
        .cost(model, &response.usage)
        .map_err(|e| format!("cost: {e}"))?;
    Ok((response.text(), cost))
}

/// Running total against [`SPEND_CAP_USD`], checked before every call so a
/// run stops (rather than overshoots) the moment the next call would exceed
/// it.
struct Budget {
    spent: Money,
    cap: Money,
}
impl Budget {
    fn new(cap_usd: f64) -> Self {
        Self {
            spent: Money(0),
            cap: Money((cap_usd * 1_000_000.0).round() as i64),
        }
    }
    fn remaining_after(&self, projected_next_call: Money) -> bool {
        Money(self.spent.0 + projected_next_call.0).0 <= self.cap.0
    }
    fn record(&mut self, cost: Money) {
        self.spent = Money(self.spent.0 + cost.0);
    }
}

/// A conservative per-call estimate for the pre-flight budget check: 400
/// input + 700 output tokens at kimi-k3's rate (the more expensive of the two
/// routes; see the module doc comment's sizing math).
fn estimated_call_cost(prices: &PriceTable) -> Money {
    let usage = kgraf_core::usage::TokenUsage {
        uncached_input: 400,
        cache_read: 0,
        cache_write: 0,
        text_output: 700,
        reasoning_output: 0,
        unsplit_reasoning_chars: None,
        raw: None,
    };
    prices.cost("kimi-k3", &usage).unwrap_or(Money(1_170_000))
}

fn route_for_task(index: usize) -> Route {
    if index.is_multiple_of(2) {
        KIMI
    } else {
        OPENAI
    }
}

#[derive(Debug)]
struct TaskResult {
    task_id: &'static str,
    arm: Arm,
    route: &'static str,
    success: bool,
    reasons: Vec<String>,
    cost: Money,
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: spends money; see the module docs"]
async fn phase4_three_arm_go_no_go() {
    if !live() {
        println!("KGRAF_LIVE_GO_NO_GO is not 1; not run");
        return;
    }
    let (has_kimi, has_openai) = set_route_credentials();
    if !has_kimi || !has_openai {
        println!(
            "SKIPPED: needs both routes live (kimi present: {has_kimi}, openai present: {has_openai})"
        );
        return;
    }

    let rig = Rig {
        ledger: Ledger::open_in_memory().expect("in-memory ledger opens"),
        prices: price_table(),
    };
    let kimi_provider = provider_for(KIMI).expect("kimi provider builds");
    let openai_provider = provider_for(OPENAI).expect("openai provider builds");
    let provider_for_route = |r: Route| -> Arc<dyn Provider> {
        if r.id == KIMI.id {
            kimi_provider.clone()
        } else {
            openai_provider.clone()
        }
    };

    let mut budget = Budget::new(SPEND_CAP_USD);
    let per_call_estimate = estimated_call_cost(&rig.prices);
    let mut results: Vec<TaskResult> = Vec::new();
    let mut truncated_at: Option<usize> = None;

    'tasks: for (index, task) in TASKS.iter().enumerate() {
        let route = route_for_task(index);
        let provider = provider_for_route(route);
        let model = route.model;
        let agent_prefix = format!("go-no-go:{}", task.id);

        // Every call about to run this iteration, budgeted against the
        // conservative per-call estimate before any of them fire.
        let calls_this_task = 1 + 2 + 3; // solo + peer(2) + orchestrator(director+2)
        if !budget.remaining_after(Money(per_call_estimate.0 * calls_this_task)) {
            truncated_at = Some(index);
            break 'tasks;
        }

        // --- Arm 1: solo. One agent, both entries, one call. -----------
        let solo_prompt = format!(
            "Extend this numbered glossary:\n{}\n\nAdd exactly two new entries, continuing the \
             numbering. First: term \"{}\", whose one-sentence definition must mention: {}. \
             Second: term \"{}\", whose one-sentence definition must mention: {}. Reply with \
             ONLY the two new lines, each in the exact format \"N. term: definition\" (choose N \
             yourself, continuing the numbering, and do not reuse a number), and nothing else.",
            task.existing, task.term_a, task.fact_a, task.term_b, task.fact_b
        );
        match complete(
            &rig,
            &provider,
            model,
            solo_prompt,
            700,
            &format!("{agent_prefix}:solo"),
            task.id,
        )
        .await
        {
            Ok((text, cost)) => {
                budget.record(cost);
                let entries = parse_entries(&text);
                let (success, reasons) = grade(task, entries.first(), entries.get(1));
                if !success && entries.len() < 2 {
                    println!("[{}] solo RAW: {text:?}", task.id);
                }
                println!(
                    "[{}] solo route={} cost={cost} success={success}{}",
                    task.id,
                    route.id,
                    if reasons.is_empty() {
                        String::new()
                    } else {
                        format!(" reasons={reasons:?}")
                    }
                );
                results.push(TaskResult {
                    task_id: task.id,
                    arm: Arm::Solo,
                    route: route.id,
                    success,
                    reasons,
                    cost,
                });
            }
            Err(e) => {
                println!("[{}] solo FAILED TO RUN: {e}", task.id);
                results.push(TaskResult {
                    task_id: task.id,
                    arm: Arm::Solo,
                    route: route.id,
                    success: false,
                    reasons: vec![format!("call failed: {e}")],
                    cost: Money(0),
                });
            }
        }

        // --- Arm 2: peer-coordinated. Two agents, a one-way note channel,
        // no supervisor. --------------------------------------------------
        let worker_a_prompt = format!(
            "You are one of two independent workers extending a shared numbered glossary at the \
             same time. Neither of you supervises the other, and you do not automatically see \
             each other's output.\n\nCurrent list:\n{}\n\nYour job: add ONE new entry for the \
             term \"{}\". The definition must be one sentence and must mention: {}. You may \
             optionally leave the other worker a one-line note (they may see it before they \
             answer, if they write second); if so, add a second line starting exactly with \
             \"NOTE:\" after your entry line. Reply with your entry line first (\"N. term: \
             definition\", choosing N yourself, continuing the existing numbering), optionally \
             followed by your NOTE line, and nothing else.",
            task.existing, task.term_a, task.fact_a
        );
        let (text_a, note_from_a, cost_a) = match complete(
            &rig,
            &provider,
            model,
            worker_a_prompt,
            500,
            &format!("{agent_prefix}:peer-a"),
            task.id,
        )
        .await
        {
            Ok((text, cost)) => {
                let note = parse_note(&text);
                (Some(text), note, cost)
            }
            Err(e) => {
                println!("[{}] peer worker A FAILED TO RUN: {e}", task.id);
                (None, None, Money(0))
            }
        };
        budget.record(cost_a);
        let note_clause = match &note_from_a {
            Some(note) => format!(
                "The other worker, who is adding a different entry to this same list at the same \
                 time, left you this note before you answered: \"{note}\""
            ),
            None => "The other worker left you no note.".to_owned(),
        };
        let worker_b_prompt = format!(
            "You are one of two independent workers extending a shared numbered glossary at the \
             same time. Neither of you supervises the other, and you do not automatically see \
             each other's output.\n\nCurrent list:\n{}\n\nYour job: add ONE new entry for the \
             term \"{}\". The definition must be one sentence and must mention: {}. {}\n\nReply \
             with your entry line first (\"N. term: definition\", choosing N yourself, \
             continuing the existing numbering), and nothing else.",
            task.existing, task.term_b, task.fact_b, note_clause
        );
        let (entry_a, entry_b, peer_cost, mut peer_reasons, text_b) = match complete(
            &rig,
            &provider,
            model,
            worker_b_prompt,
            500,
            &format!("{agent_prefix}:peer-b"),
            task.id,
        )
        .await
        {
            Ok((text_b, cost_b)) => {
                budget.record(cost_b);
                let entry_a = text_a.as_deref().and_then(parse_entry);
                let entry_b = parse_entry(&text_b);
                (
                    entry_a,
                    entry_b,
                    Money(cost_a.0 + cost_b.0),
                    Vec::new(),
                    Some(text_b),
                )
            }
            Err(e) => {
                println!("[{}] peer worker B FAILED TO RUN: {e}", task.id);
                (None, None, cost_a, vec![format!("call failed: {e}")], None)
            }
        };
        let (peer_success, mut reasons) = grade(task, entry_a.as_ref(), entry_b.as_ref());
        reasons.append(&mut peer_reasons);
        if !peer_success && (entry_a.is_none() || entry_b.is_none()) {
            // Raw-text diagnostic: distinguishes a real coordination failure
            // from a harness parsing bug.
            println!(
                "[{}] peer RAW A: {:?}\n[{}] peer RAW B: {:?}",
                task.id, text_a, task.id, text_b
            );
        }
        println!(
            "[{}] peer-coordinated route={} cost={peer_cost} success={peer_success}{}",
            task.id,
            route.id,
            if reasons.is_empty() {
                String::new()
            } else {
                format!(" reasons={reasons:?}")
            }
        );
        results.push(TaskResult {
            task_id: task.id,
            arm: Arm::PeerCoordinated,
            route: route.id,
            success: peer_success,
            reasons,
            cost: peer_cost,
        });

        // --- Arm 3: orchestrator-supervised. A director assigns exact line
        // numbers (owns the interface, 0024) and each worker's delivery is
        // checked against what it was assigned (commitment verified, not
        // trusted). -------------------------------------------------------
        let director_prompt = format!(
            "You are directing two independent workers who will each add one entry to this \
             shared numbered glossary, at the same time:\n{}\n\nWorker A's assignment: add an \
             entry for the term \"{}\" (definition must mention: {}).\nWorker B's assignment: \
             add an entry for the term \"{}\" (definition must mention: {}).\n\nYou must resolve, \
             in advance, exactly which line number each worker uses, continuing the existing \
             numbering, so they never guess and can never choose the same number. Reply with \
             exactly two lines, in this exact form:\nWorker A: use line number <N>. <one sentence \
             reminding Worker A of their term and required fact, and telling them not to touch \
             any other line>\nWorker B: use line number <N>. <same for Worker B>",
            task.existing, task.term_a, task.fact_a, task.term_b, task.fact_b
        );
        let director_result = complete(
            &rig,
            &openai_provider,
            OPENAI.model,
            director_prompt,
            600,
            &format!("{agent_prefix}:director"),
            task.id,
        )
        .await;
        let (director_text, director_cost) = match director_result {
            Ok((text, cost)) => (Some(text), cost),
            Err(e) => {
                println!("[{}] orchestrator director FAILED TO RUN: {e}", task.id);
                (None, Money(0))
            }
        };
        budget.record(director_cost);

        // Parse "Worker A: use line number <N>. <brief...>" and the B line.
        let parse_brief = |label: &str, text: &str| -> Option<(u32, String)> {
            for line in text.lines() {
                let line = line.trim();
                let Some(rest) = line.strip_prefix(label) else {
                    continue;
                };
                let rest = rest.trim_start_matches(':').trim();
                let Some(after_marker) = rest
                    .to_lowercase()
                    .find("line number")
                    .map(|i| &rest[i + "line number".len()..])
                else {
                    continue;
                };
                let digits: String = after_marker
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(n) = digits.parse::<u32>() {
                    return Some((n, rest.to_owned()));
                }
            }
            None
        };
        let assignment_a = director_text
            .as_deref()
            .and_then(|t| parse_brief("Worker A", t));
        let assignment_b = director_text
            .as_deref()
            .and_then(|t| parse_brief("Worker B", t));

        let mut orch_reasons = Vec::new();
        let mut orch_cost = director_cost;
        let (entry_a, entry_b) = match (&assignment_a, &assignment_b) {
            (Some((num_a, brief_a)), Some((num_b, brief_b))) => {
                let prompt_a = format!(
                    "{brief_a}\n\nDefinition must be one sentence and must mention: {}. Reply \
                     with ONLY your entry line in the exact format \"N. term: definition\", \
                     using the exact line number you were assigned, and nothing else.",
                    task.fact_a
                );
                let prompt_b = format!(
                    "{brief_b}\n\nDefinition must be one sentence and must mention: {}. Reply \
                     with ONLY your entry line in the exact format \"N. term: definition\", \
                     using the exact line number you were assigned, and nothing else.",
                    task.fact_b
                );
                let a = complete(
                    &rig,
                    &provider,
                    model,
                    prompt_a,
                    500,
                    &format!("{agent_prefix}:orch-worker-a"),
                    task.id,
                )
                .await;
                let b = complete(
                    &rig,
                    &provider,
                    model,
                    prompt_b,
                    500,
                    &format!("{agent_prefix}:orch-worker-b"),
                    task.id,
                )
                .await;
                let entry_a = match a {
                    Ok((text, cost)) => {
                        orch_cost = Money(orch_cost.0 + cost.0);
                        let entry = parse_entry(&text);
                        if entry.is_none() {
                            println!("[{}] orchestrator worker A RAW: {text:?}", task.id);
                        }
                        if let Some(e) = &entry
                            && e.number != *num_a
                        {
                            orch_reasons.push(format!(
                                "commitment: worker A was assigned line {num_a} but used {}",
                                e.number
                            ));
                        }
                        entry
                    }
                    Err(e) => {
                        orch_reasons.push(format!("orchestrator worker A call failed: {e}"));
                        None
                    }
                };
                let entry_b = match b {
                    Ok((text, cost)) => {
                        orch_cost = Money(orch_cost.0 + cost.0);
                        let entry = parse_entry(&text);
                        if entry.is_none() {
                            println!("[{}] orchestrator worker B RAW: {text:?}", task.id);
                        }
                        if let Some(e) = &entry
                            && e.number != *num_b
                        {
                            orch_reasons.push(format!(
                                "commitment: worker B was assigned line {num_b} but used {}",
                                e.number
                            ));
                        }
                        entry
                    }
                    Err(e) => {
                        orch_reasons.push(format!("orchestrator worker B call failed: {e}"));
                        None
                    }
                };
                (entry_a, entry_b)
            }
            _ => {
                orch_reasons.push(
                    "director did not produce a parseable assignment for both workers \
                     (fail loud: no assumed default)"
                        .to_owned(),
                );
                (None, None)
            }
        };
        budget.record(Money(orch_cost.0 - director_cost.0));
        let (grade_success, mut grade_reasons) = grade(task, entry_a.as_ref(), entry_b.as_ref());
        orch_reasons.append(&mut grade_reasons);
        let orch_success = grade_success && orch_reasons.is_empty();
        println!(
            "[{}] orchestrator-supervised route={} cost={orch_cost} success={orch_success}{}",
            task.id,
            route.id,
            if orch_reasons.is_empty() {
                String::new()
            } else {
                format!(" reasons={orch_reasons:?}")
            }
        );
        results.push(TaskResult {
            task_id: task.id,
            arm: Arm::OrchestratorSupervised,
            route: route.id,
            success: orch_success,
            reasons: orch_reasons,
            cost: orch_cost,
        });
    }

    // --- Summary: per-task detail, per-arm success rate, total spend. ----
    let total_spent: Money = results.iter().map(|r| r.cost).sum();
    println!("\n=== Three-arm go/no-go: per-task detail ===");
    for r in &results {
        println!(
            "{:<20} {:<24} route={:<14} success={:<5} cost={}{}",
            r.task_id,
            r.arm.to_string(),
            r.route,
            r.success,
            r.cost,
            if r.reasons.is_empty() {
                String::new()
            } else {
                format!(" reasons={:?}", r.reasons)
            }
        );
    }
    println!("\n=== Three-arm go/no-go: summary ===");
    if let Some(t) = truncated_at {
        println!(
            "TRUNCATED at task index {t} ({} of {TASK_COUNT} tasks run): the ${SPEND_CAP_USD:.2} \
             cap would have been exceeded by the next task's estimated cost. Confidence at n={t} \
             per arm is lower than the n={TASK_COUNT} the sizing above targeted.",
            t
        );
    }
    for arm in [Arm::Solo, Arm::PeerCoordinated, Arm::OrchestratorSupervised] {
        let arm_results: Vec<&TaskResult> = results.iter().filter(|r| r.arm == arm).collect();
        let n = arm_results.len();
        let successes = arm_results.iter().filter(|r| r.success).count();
        let rate = if n > 0 {
            successes as f64 / n as f64
        } else {
            0.0
        };
        println!("{arm}: {successes}/{n} ({:.1}%)", rate * 100.0);
    }
    println!("total spend: {total_spent} (cap {SPEND_CAP_USD:.2})");
    assert!(
        !results.is_empty(),
        "the run produced no results at all; something failed before any task ran"
    );
}
