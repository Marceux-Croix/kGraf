//! Driving one worker, on one route, against one brief, through kGraf's
//! normal worker path, the same assembly and `run_segment` `kgraf worker`
//! runs (`kgraf_host::worker`), and recording the result.
//!
//! Each attempt gets its own throwaway git repository (`gitutil`), so
//! concurrent attempts never share a worktree (SR "disjoint writes" holds
//! trivially: every attempt has its own repository, not just its own
//! worktree). Merge-back never moves a branch (`kgraf-cli`'s worker command
//! docs): a satisfied brief is checked against the attempt's own commit,
//! extracted with `gitutil::checkout_commit`.
//!
//! **No route is ever refused for price** (a spend cap is
//! overcontrol here, since a provider's own console already limits spend).
//! A model with no recorded price still runs; its cost is `None`
//! ("unpriced", never a fabricated `$0`) rather than the attempt being
//! skipped. A local route (`RouteHandle::local`: no credential, a localhost
//! base URL, `ollama`, `llama-cpp`) with no price is noted "local,
//! unpriced" rather than plain "unpriced": decision 0041 holds a local model
//! is not special-cased to free (the operator may still price it, e.g. for
//! electricity), so the distinction is informational, not a different cost.
//! An unpriced attempt's cost is never folded into the `ProfileStore`
//! tally's `cost_micros` sum, priced or not: recording it at a fabricated
//! `0` would silently make it look like the cheapest route to the router
//! (decision 0075 picks by cost-per-satisfied), which is exactly the
//! silent degradation that fail-loud forbids. It is still returned in every
//! `run_bench` caller sees (and `kgraf-bench-briefs`'s own printed listing),
//! just not folded into the persisted numeric tally.

use crate::brief::Brief;
use crate::check;
use crate::gitutil;
use crate::suite_store;
use anyhow::{Context, Result};
use kgraf_core::cost::{Money, PriceTable};
use kgraf_core::model_profile::{Source, TaskKind};
use kgraf_graph::{GraphStore, Indexer};
use kgraf_host::worker::ask::Attendance;
use kgraf_host::worker::assemble::{Assembly, GraphParts, allow_store_path};
use kgraf_host::worker::config::{
    DEFAULT_CONTEXT_WINDOW_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_STEP_BUDGET, WorkerConfig,
};
use kgraf_host::worker::run::{Owner, SegmentEnd, open_transcript, plan_segment};
use kgraf_host::worker::segment::{SegmentOptions, run_segment};
use kgraf_host::worker::workspace::{self};
use kgraf_provider::{Ledger, Provider};
use kgraf_runtime::{CallMode, ContextConfig, WorkerBrief, WorkerOutcome};
use kgraf_sandbox::{Identity, MergeBack, WorkspaceManager};
use kgraf_session::approval_store::PersistedAllowStore;
use kgraf_tools::execpolicy::ExecPolicy;
use std::path::Path;
use std::sync::Arc;

/// A configured provider route, ready to run workers.
pub struct RouteHandle {
    /// The catalog route id (`kgraf providers`), or any label of the
    /// caller's choosing for a route built outside the catalog (tests).
    pub route: String,
    /// The catalog vendor (part of the prefix identity, 0043).
    pub vendor: String,
    pub model: String,
    pub provider: Arc<dyn Provider>,
    /// Whether the route's catalog entry is local: no credential and a
    /// localhost base URL (`kgraf-fleet`'s `ollama`/`llama-cpp` entries, or
    /// any route built the same way, see `ProviderConfig::local`). Affects
    /// only an unpriced attempt's note ("local, unpriced" instead of
    /// "unpriced"); a route with a price entry is costed the same whether
    /// local or hosted.
    pub local: bool,
}

/// One attempt the runner means to make (or, in a dry run, would make).
#[derive(Clone, Debug)]
pub struct PlannedAttempt {
    pub route: String,
    pub vendor: String,
    pub model: String,
    pub brief_id: String,
    pub kind: TaskKind,
    pub rep: u32,
    /// A worst-case bound on the attempt's cost (the step budget's rounds,
    /// each at the maximum output size, priced at the model's output rate;
    /// ignores input tokens, so it is a floor on the true worst case, not a
    /// guarantee). `None` when the model has no recorded price, shown as
    /// "unpriced" rather than any number.
    pub estimated_cost: Option<Money>,
}

/// Flags shaping one bench invocation.
pub struct BenchOptions {
    pub reps: u32,
    /// List planned attempts and their estimated cost; make no provider
    /// call and write nothing to `state_dir`.
    pub dry_run: bool,
    /// Each attempt's rounds, output ceiling and context window.
    pub budget: WorkerBudget,
}

/// One worker's rounds, output ceiling per round and context window. The
/// default is `kgraf worker`'s own (the shared constants in
/// `kgraf_host::worker::config`), so a bench attempt measures a model under
/// the budget a real worker gets, not a smaller one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerBudget {
    /// Model rounds, including the final answer round.
    pub step_budget: u32,
    /// Output ceiling per round.
    pub max_output_tokens: u32,
    /// The model's context window in tokens.
    pub context_window_tokens: u64,
}

impl Default for WorkerBudget {
    fn default() -> Self {
        Self {
            step_budget: DEFAULT_STEP_BUDGET,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
        }
    }
}

/// What one attempt did.
#[derive(Clone, Debug, serde::Serialize)]
pub struct AttemptOutcome {
    pub route: String,
    pub vendor: String,
    pub model: String,
    pub brief_id: String,
    pub kind: TaskKind,
    pub rep: u32,
    pub satisfied: bool,
    /// The attempt did not measure the model: the provider failed, or the
    /// worker could not run, or its result could not be committed. Never
    /// tallied. `satisfied` is `false` and `note` says why.
    pub errored: bool,
    /// `None` when the model has no recorded price: never a fabricated
    /// `$0` (see the module docs).
    pub cost: Option<Money>,
    /// Why an attempt is unsatisfied beyond "the check failed" (a worker
    /// error, an assembly failure), or why its cost is unpriced ("unpriced"
    /// / "local, unpriced").
    pub note: Option<String>,
    /// Input tokens (uncached + cache read + cache write) the ledger recorded
    /// under this attempt's task id, summed across every call the attempt
    /// made. `0` for a dry run (no call is made) or an attempt that made no
    /// settled call (e.g. the assembly failed before a call).
    pub tokens_in: u64,
    /// Output tokens (visible text plus reasoning) the ledger recorded under
    /// this attempt's task id. See `tokens_in`.
    pub tokens_out: u64,
    /// Wall-clock time the attempt's `run_segment` call took, in
    /// milliseconds. `0` for a dry run.
    pub wall_ms: u64,
}

/// Whether a `Failed` worker outcome is the model's own behavior (it hit its
/// output ceiling, ended on an unmodeled stop reason, answered inconsistently,
/// was refused or overran its context window (`TurnNotUsable`), paused for a
/// server tool no route uses, or needed an approval nobody could give) rather than a provider or ledger
/// failure. The list is the messages `kgraf_runtime`'s loop raises for the
/// model's behavior; any other `Failed` text is treated as a fault outside
/// the model, so an unknown message is never tallied against it.
fn model_caused(error: &str) -> bool {
    const MODEL_CAUSED: [&str; 8] = [
        "the output ceiling was reached",
        "unmodeled stop reason",
        "the response carries tool calls but stopped with",
        "the model stopped for a tool call and the response carries none",
        "this worker has no one to ask",
        // `ProviderError::TurnNotUsable`: a refusal or a context window
        // exceeded mid-turn. The call was billed; the model gave no answer.
        "ended the turn with stop_reason",
        "the turn produced no usable answer",
        "the provider paused the turn for a server tool",
    ];
    MODEL_CAUSED.iter().any(|m| error.contains(m))
}

/// The note an unpriced attempt's cost carries: decision 0041 holds a local
/// model is not free by default, so its lack of a price is still worth
/// calling out distinctly from a hosted model nobody priced.
fn unpriced_note(local: bool) -> &'static str {
    if local { "local, unpriced" } else { "unpriced" }
}

/// A worst-case bound on one attempt's cost at `model`'s price: the step
/// budget's rounds, each at the output ceiling, priced at the output rate.
/// `None` when the model has no recorded price.
fn worst_case_estimate(
    prices: &PriceTable,
    route: &str,
    model: &str,
    budget: &WorkerBudget,
) -> Option<Money> {
    let price = prices.price_on(Some(route), model)?;
    let tokens = u64::from(budget.max_output_tokens) * u64::from(budget.step_budget);
    let micros = (tokens as i128 * price.output as i128 + 500_000) / 1_000_000;
    Some(Money::from_micros(micros as i64))
}

/// List every attempt the given routes, briefs and repetition count would
/// make, in the order [`run_bench`] makes them. Never touches a provider or
/// the filesystem beyond the price table already in memory.
pub fn plan_attempts(
    routes: &[(String, String, String)],
    briefs: &[Brief],
    reps: u32,
    prices: &PriceTable,
    budget: &WorkerBudget,
) -> Vec<PlannedAttempt> {
    let mut planned = Vec::new();
    for (route, vendor, model) in routes {
        for brief in briefs {
            for rep in 0..reps.max(1) {
                planned.push(PlannedAttempt {
                    route: route.clone(),
                    vendor: vendor.clone(),
                    model: model.clone(),
                    brief_id: brief.id.to_owned(),
                    kind: brief.kind,
                    rep,
                    estimated_cost: worst_case_estimate(prices, route, model, budget),
                });
            }
        }
    }
    planned
}

/// Run every route against every brief, `opts.reps` times each, recording
/// every priced attempt into the `ProfileStore` at `state_dir` with
/// `Source::Bench` (an unpriced attempt's cost is never recorded there; see
/// the module docs). Skipped entirely in a dry run. Returns every attempt
/// made (or, in a dry run, every attempt that would have been made, each
/// unsatisfied with `cost` set to [`PlannedAttempt::estimated_cost`]).
pub async fn run_bench(
    routes: &[RouteHandle],
    briefs: &[Brief],
    prices: &PriceTable,
    state_dir: &Path,
    opts: &BenchOptions,
) -> Result<Vec<AttemptOutcome>> {
    if opts.dry_run {
        let descs: Vec<(String, String, String)> = routes
            .iter()
            .map(|r| (r.route.clone(), r.vendor.clone(), r.model.clone()))
            .collect();
        return Ok(
            plan_attempts(&descs, briefs, opts.reps, prices, &opts.budget)
                .into_iter()
                .map(|p| AttemptOutcome {
                    route: p.route,
                    vendor: p.vendor,
                    model: p.model,
                    brief_id: p.brief_id,
                    kind: p.kind,
                    rep: p.rep,
                    satisfied: false,
                    errored: false,
                    cost: p.estimated_cost,
                    note: Some("dry run: no call made".into()),
                    tokens_in: 0,
                    tokens_out: 0,
                    wall_ms: 0,
                })
                .collect(),
        );
    }

    // Before any model call: the check harness must judge a known-good crate,
    // or every attempt would read as a failed brief.
    check::self_test(check::DEFAULT_TIMEOUT)
        .context("the check harness failed its positive control; no model call was made")?;
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("cannot create {}", state_dir.display()))?;
    // Bench tallies are kept per suite version: another suite's are archived
    // aside, never mixed into this run's rates (`suite_store`).
    let suite = crate::brief::suite_hash();
    let mut store = suite_store::open_for_recording(state_dir, &suite)?;
    let mut outcomes = Vec::new();

    for route in routes {
        for brief in briefs {
            for rep in 0..opts.reps.max(1) {
                let worker_id = format!("bench-{}-{}-{rep}", route.route, brief.id);
                let outcome = run_one(route, brief, &worker_id, rep, prices, &opts.budget)
                    .await
                    .with_context(|| {
                        format!(
                            "attempt {worker_id} (route {}, brief {})",
                            route.route, brief.id
                        )
                    })?;
                // A provider or harness error is not a measurement of the model:
                // it is returned to the caller but never tallied, so an outage
                // cannot read as a run of failed briefs. An unpriced attempt
                // still counts toward the rate; the tally marks its cost unknown
                // rather than zero.
                if !outcome.errored {
                    store.record(
                        &route.model,
                        &route.vendor,
                        Source::Bench,
                        brief.kind,
                        outcome.satisfied,
                        outcome.cost,
                    );
                }
                suite_store::save(state_dir, &store, &suite)?;
                outcomes.push(outcome);
            }
        }
    }
    Ok(outcomes)
}

/// One attempt: a fresh throwaway repository from the brief's fixture, one
/// worker segment through `kgraf_host::worker::run_segment`, and the
/// mechanical check against whatever the worker committed.
async fn run_one(
    route: &RouteHandle,
    brief: &Brief,
    worker_id: &str,
    rep: u32,
    prices: &PriceTable,
    budget: &WorkerBudget,
) -> Result<AttemptOutcome> {
    let attempt_tmp = tempfile::tempdir().context("cannot create the attempt's temp dir")?;
    // Diagnostic switch for the bench only: when KGRAF_BENCH_KEEP_DIR is set,
    // the attempt's temp dir (transcript, ledger, worktree) is kept on disk
    // instead of removed on drop, so a failing attempt's full transcript can
    // be inspected after the run. `.keep()` consumes the `TempDir` into a bare
    // `PathBuf` with no cleanup. The bench's reproduction notes use it.
    let attempt_root: std::path::PathBuf = if std::env::var_os("KGRAF_BENCH_KEEP_DIR").is_some() {
        let kept = attempt_tmp.keep();
        eprintln!("KGRAF_BENCH_KEEP_DIR: keeping {}", kept.display());
        kept
    } else {
        attempt_tmp.path().to_path_buf()
    };
    let root = attempt_root.join("repo");
    let base = gitutil::init_fixture_repo(&brief.fixture_dir(), &root)
        .with_context(|| format!("cannot init the fixture repo for {}", brief.id))?;
    let state = root.join(".kgraf");
    std::fs::create_dir_all(&state)
        .with_context(|| format!("cannot create {}", state.display()))?;

    let manager = Arc::new(
        WorkspaceManager::open(&root, &state.join("workspaces"))
            .context("cannot open the worker workspaces")?,
    );
    let merge_back = MergeBack::open(
        manager.clone(),
        "main",
        Identity {
            name: "kgraf-bench-briefs".into(),
            email: "kgraf-bench-briefs@localhost".into(),
        },
    )
    .context("cannot open merge-back")?;

    let transcript_path = state.join(format!("worker-{worker_id}.jsonl"));
    let mut opened = open_transcript(&transcript_path).await?;
    let owner = Owner::new(worker_id, worker_id, brief.instruction);
    let plan = plan_segment(
        &opened,
        &owner,
        false,
        "HEAD",
        &workspace::GitView {
            repo: &root,
            manager: &manager,
            worker: worker_id,
        },
    )?;
    let segment = workspace::open(&manager, &merge_back, &plan, None)?;
    opened.begin_segment(&owner, &plan).await?;
    let ws = segment.workspace().clone();

    let ledger_path = state.join("ledger.db");
    let ledger = Arc::new(
        Ledger::open(&ledger_path)
            .with_context(|| format!("cannot open {}", ledger_path.display()))?,
    );
    let (allow_store, _rules) = PersistedAllowStore::open(allow_store_path(&state), ws.root())
        .context("cannot open the project's allow rules")?;

    // Index the fixture repo the same way a real run does (`Host::build_base`),
    // so the worker under benchmark gets `GraphSliceTool` and real slices
    // instead of a silently graph-less brief: an unindexed bench attempt was
    // never testing the graph path at all.
    let graph_path = state.join("graph.db");
    let indexed = {
        let store =
            GraphStore::open(&graph_path, &root).context("cannot open the bench graph store")?;
        Indexer::new(&store)
            .index()
            .context("cannot index the fixture repo")?
    };
    let graph = if indexed.nodes > 0 {
        Some(GraphParts {
            slices: GraphStore::open(&graph_path, &root)
                .context("cannot open the bench graph store for slices")?,
            tool: GraphStore::open(&graph_path, &root)
                .context("cannot open the bench graph store for the graph_slice tool")?,
        })
    } else {
        None
    };

    let assembly = Assembly {
        ledger: ledger.clone(),
        provider: route.provider.clone(),
        vendor: route.vendor.clone(),
        model: route.model.clone(),
        workspace: ws.clone(),
        checkout: root.clone(),
        graph,
        config: WorkerConfig::default(),
        worker: worker_id.into(),
        task: worker_id.into(),
        exec_policy: ExecPolicy::default_kgraf_policy(),
        allow_store,
        project: root.to_string_lossy().into_owned(),
        attendance: Attendance::Unattended,
        call_mode: CallMode::Stream,
        max_output_tokens: budget.max_output_tokens,
        max_checkpoints: kgraf_host::limits::DEFAULT_MAX_CHECKPOINTS,
        // A brief run sends no reasoning field: the measured baseline.
        reasoning: Default::default(),
        tool_choice: kgraf_host::worker::assemble::tool_choice_support(&route.route, &route.model),
        path_case: kgraf_tools::detect_path_case(ws.root())
            .context("cannot probe the worktree's filesystem case behavior")?,
        read_denies: vec![state.clone()],
        mcp_home: state.join("mcp-home"),
        extra_tools: Vec::new(),
        session_line: None,
        prefixes: kgraf_runtime::PrefixRegistry::global(),
        machine_gate: None,
        external_executor: None,
        dependency_refetch: None,
        // A benchmark runs the brief alone: no skills, so the prefix and the
        // tool list stay the ones every recorded run was measured with.
        skills: None,
        permissions: std::sync::Arc::new(kgraf_tools::PermissionControl::manual()),
        protected_writes: None,
        refusals: None,
    };
    let worker_brief = WorkerBrief {
        task: worker_id.into(),
        worker: worker_id.into(),
        instruction: brief.instruction.to_owned(),
        workspace_root: ws.root().to_path_buf(),
        model: route.model.clone(),
        slices: Vec::new(),
        skills: Vec::new(),
        step_budget: budget.step_budget,
        context: ContextConfig::for_model(budget.context_window_tokens)
            .context("cannot build the context configuration")?,
        write_scope: None,
    };

    let started = std::time::Instant::now();
    let report = run_segment(
        assembly,
        segment,
        &manager,
        &merge_back,
        &mut opened,
        worker_brief,
        SegmentOptions {
            resume: false,
            cancel: None,
            cancel_on_interrupt: false,
            message: format!("bench {worker_id}"),
        },
        |_assembled| {},
        |_event| {},
    )
    .await;
    let wall_ms = started.elapsed().as_millis() as u64;

    // Tokens in/out for this attempt, read back from the ledger by task id
    // (`assembly.task` was set to `worker_id`, unique per attempt) before the
    // ledger's own temp dir is dropped at the end of this function. Summed
    // across every call `usage_by_task` groups under this (model, task)
    // pair; there is normally exactly one row, but summing rather than
    // indexing tolerates more than one settled call under the same task.
    let (tokens_in, tokens_out) = ledger
        .usage_by_task(None)
        .context("cannot read this attempt's token usage from its ledger")?
        .into_iter()
        .filter(|t| t.task_id == worker_id)
        .fold((0u64, 0u64), |(input, output), t| {
            (
                input + t.usage.uncached_input + t.usage.cache_read + t.usage.cache_write,
                output + t.usage.text_output + t.usage.reasoning_output,
            )
        });

    let mut note = report
        .errors
        .first()
        .map(|e| format!("{e:#}"))
        .filter(|_| report.result.is_none());
    // Every error `run_segment` collected, and a `Failed` run outcome that is
    // not the model's own doing (a provider or ledger failure), means the
    // attempt measured the harness or the provider, not the model.
    let provider_failure = match report.result.as_ref().map(|r| &r.outcome) {
        Some(WorkerOutcome::Failed { error }) if !model_caused(error) => Some(error.clone()),
        _ => None,
    };
    let mut errored = !report.errors.is_empty() || provider_failure.is_some();
    if let Some(error) = &provider_failure {
        note.get_or_insert_with(|| format!("provider or harness error: {error}"));
    }
    let satisfied = match &report.end {
        SegmentEnd::Committed { commit, .. } => {
            let checked_out = attempt_root.join("checked-out");
            gitutil::checkout_commit(&root, commit, &checked_out).with_context(|| {
                format!("cannot check out attempt commit {commit} for {}", brief.id)
            })?;
            let scratch = attempt_root.join("check-scratch");
            std::fs::create_dir_all(&scratch)
                .with_context(|| format!("cannot create {}", scratch.display()))?;
            let verdict = check::verify(
                brief,
                &check::Attempt {
                    dir: &checked_out,
                    repo: &root,
                    base: &base,
                    commit,
                },
                &check::CheckEnv {
                    scratch: &scratch,
                    timeout: check::DEFAULT_TIMEOUT,
                },
            );
            match verdict {
                Ok(check::Verdict::Satisfied) => true,
                Ok(check::Verdict::Unsatisfied(reason)) => {
                    note.get_or_insert_with(|| format!("the check found: {reason}"));
                    false
                }
                Err(e) => {
                    // The check could not run: a harness failure, not a
                    // measurement of the model, so it is never tallied.
                    errored = true;
                    note.get_or_insert_with(|| {
                        format!("the check could not run (harness failure): {e:#}")
                    });
                    false
                }
            }
        }
        SegmentEnd::Empty => {
            note.get_or_insert_with(|| "the worker changed nothing".to_owned());
            false
        }
        SegmentEnd::Refused { reasons, .. } => {
            note.get_or_insert_with(|| format!("merge-back refused the attempt: {reasons}"));
            false
        }
        SegmentEnd::NotCarried { error, .. } => {
            note.get_or_insert_with(|| format!("the attempt was not committed: {error}"));
            false
        }
    };

    // No route is ever refused for price: an unpriced model
    // still runs, and its cost is `None` ("unpriced" / "local, unpriced"),
    // never a fabricated `$0` (see the module docs).
    // The lookup is route-aware, as the ledger's own costing is: a deployment
    // priced under its route-scoped key is priced, and one named like a bare-priced
    // model is not. A price that cannot cost the calls (above a rate tier) leaves
    // the attempt unpriced too: the calls are already paid for, so the run goes on.
    let priced = if prices.is_priced_on(Some(&route.route), &route.model) {
        priced_or_unpriced(ledger.cost_for_agent(worker_id, prices))
            .with_context(|| format!("reading the cost of worker `{worker_id}` from the ledger"))?
    } else {
        None
    };
    let cost = match priced {
        Some(cost) => Some(cost),
        None => {
            let text = unpriced_note(route.local);
            note = Some(match note {
                Some(existing) => format!("{existing}; {text}"),
                None => text.to_owned(),
            });
            None
        }
    };

    Ok(AttemptOutcome {
        route: route.route.clone(),
        vendor: route.vendor.clone(),
        model: route.model.clone(),
        brief_id: brief.id.to_owned(),
        kind: brief.kind,
        rep,
        satisfied: satisfied && !errored,
        errored,
        cost,
        note,
        tokens_in,
        tokens_out,
        wall_ms,
    })
}

/// A ledger cost as an attempt's cost: `None` (unpriced) only when the price
/// table cannot cost the calls (no entry, or a call above a rate tier). Any
/// other failure, a ledger read error or a corrupt row, is an error: reading
/// it as "unpriced" would hide a broken ledger behind a plausible note.
fn priced_or_unpriced(cost: Result<Money, kgraf_provider::LedgerError>) -> Result<Option<Money>> {
    use kgraf_core::cost::CostError;
    use kgraf_provider::LedgerError;
    match cost {
        Ok(cost) => Ok(Some(cost)),
        Err(LedgerError::Cost(
            CostError::AboveTierThreshold { .. } | CostError::UnpricedModel { .. },
        )) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brief::suite;
    use kgraf_core::cost::ModelPrice;
    use kgraf_core::model_profile::ProfileStore;
    use kgraf_core::{Protocol, TokenUsage};
    use kgraf_provider::capability::{Capabilities, StreamEvent, Streaming};
    use kgraf_provider::{CallToken, Content, ProviderError, Request, Response, StopReason};
    use std::sync::Mutex;

    /// A worker that applies one fixed patch on its first round, then stops.
    /// `patch` is a full `apply_patch` envelope body (kgraf-tools' format);
    /// see the two scripted providers below for real ones.
    struct Scripted {
        /// `(tool name, input)` calls to make on round 0; empty means "stop
        /// immediately with no changes" (a `Failing` case).
        calls: Vec<(&'static str, serde_json::Value)>,
        rounds: Mutex<usize>,
        /// Each request's output ceiling, in order.
        max_output_tokens: Mutex<Vec<u32>>,
    }

    fn usage() -> TokenUsage {
        TokenUsage {
            uncached_input: 100,
            text_output: 10,
            ..TokenUsage::default()
        }
    }

    #[async_trait::async_trait]
    impl Provider for Scripted {
        fn adapter(&self) -> &'static str {
            "bench-test-fake"
        }
        fn protocol(&self) -> Protocol {
            Protocol::Additive
        }
        async fn complete(
            &self,
            request: &Request,
            token: &CallToken,
        ) -> Result<Response, ProviderError> {
            token.settle(&usage(), None)?;
            Ok(self.reply(request))
        }
        fn capabilities(&self) -> Capabilities<'_> {
            Capabilities::none().with_streaming(self)
        }
    }

    impl Scripted {
        fn reply(&self, request: &Request) -> Response {
            self.max_output_tokens
                .lock()
                .expect("lock")
                .push(request.max_output_tokens);
            let round = {
                let mut r = self.rounds.lock().expect("lock");
                let n = *r;
                *r += 1;
                n
            };
            let mut content = Vec::new();
            let calls = if round == 0 { &self.calls[..] } else { &[][..] };
            let text = if calls.is_empty() { "done" } else { "working" };
            content.push(Content::text(text));
            for (i, (name, input)) in calls.iter().enumerate() {
                content.push(Content::ToolUse {
                    id: format!("c{round}-{i}"),
                    name: (*name).to_owned(),
                    input: input.clone(),
                    signature: None,
                });
            }
            Response {
                finish_reason: None,
                model: request.model.clone(),
                content,
                stop_reason: if calls.is_empty() {
                    StopReason::EndTurn
                } else {
                    StopReason::ToolUse
                },
                usage: usage(),
                request_id: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl Streaming for Scripted {
        async fn stream(
            &self,
            request: &Request,
            token: &CallToken,
            sink: &mut (dyn FnMut(StreamEvent) + Send),
        ) -> Result<Response, ProviderError> {
            let response = self.reply(request);
            sink(StreamEvent::Started {
                model: request.model.clone(),
                request_id: Some("req".into()),
            });
            sink(StreamEvent::Delta {
                content: Content::text(match response.content.first() {
                    Some(Content::Text { text, .. }) => text.clone(),
                    _ => String::new(),
                }),
            });
            sink(StreamEvent::Usage { usage: usage() });
            sink(StreamEvent::Finished {
                stop_reason: response.stop_reason,
            });
            token.settle(&usage(), None)?;
            Ok(response)
        }
    }

    fn prices() -> PriceTable {
        let mut p = PriceTable::new();
        p.set(
            "fake-model",
            ModelPrice {
                uncached_input: 1_000_000,
                cache_read: 100_000,
                cache_write: 1_000_000,
                output: 5_000_000,
                reasoning: 5_000_000,
            },
        );
        p
    }

    fn route(provider: Arc<dyn Provider>) -> RouteHandle {
        RouteHandle {
            route: "fake".into(),
            vendor: "fake-vendor".into(),
            model: "fake-model".into(),
            provider,
            local: false,
        }
    }

    fn local_route(provider: Arc<dyn Provider>, model: &str) -> RouteHandle {
        RouteHandle {
            route: "ollama".into(),
            vendor: "ollama-vendor".into(),
            model: model.into(),
            provider,
            local: true,
        }
    }

    fn apply_patch_add(path: &str, contents: &str) -> serde_json::Value {
        let patch = format!(
            "*** Begin Patch\n*** Add File: {path}\n{}\n*** End Patch\n",
            contents
                .lines()
                .map(|l| format!("+{l}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        serde_json::json!({ "patch": patch })
    }

    fn tests_sum_brief() -> Brief {
        suite()
            .into_iter()
            .find(|b| b.id == "tests-sum")
            .expect("tests-sum brief")
    }

    fn failing_provider() -> Arc<Scripted> {
        // Never writes the required test: the check must fail.
        Arc::new(Scripted {
            calls: vec![],
            rounds: Mutex::new(0),
            max_output_tokens: Mutex::new(Vec::new()),
        })
    }

    /// Writes the `tests-sum` brief's required test, so `SegmentEnd` is
    /// `Committed` and the check passes -- used where a test cares about the
    /// price note alone and would otherwise also pick up the unrelated "the
    /// worker changed nothing" note a no-op `SegmentEnd::Empty` attempt gets.
    fn satisfying_provider() -> Arc<Scripted> {
        let more_tests = "use fx_tests_sum::sum;\n\n#[test]\nfn handles_empty_and_negative() {\n    assert_eq!(sum(&[]), 0);\n    assert_eq!(sum(&[-3, 5, -2]), 0);\n}\n";
        Arc::new(Scripted {
            calls: vec![(
                "apply_patch",
                apply_patch_add("tests/more_tests.rs", more_tests),
            )],
            rounds: Mutex::new(0),
            max_output_tokens: Mutex::new(Vec::new()),
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_satisfying_fake_records_a_satisfied_bench_tally() {
        let brief = tests_sum_brief();
        let more_tests = "use fx_tests_sum::sum;\n\n#[test]\nfn handles_empty_and_negative() {\n    assert_eq!(sum(&[]), 0);\n    assert_eq!(sum(&[-3, 5, -2]), 0);\n}\n";
        let provider: Arc<Scripted> = Arc::new(Scripted {
            calls: vec![(
                "apply_patch",
                apply_patch_add("tests/more_tests.rs", more_tests),
            )],
            rounds: Mutex::new(0),
            max_output_tokens: Mutex::new(Vec::new()),
        });
        let state = tempfile::tempdir().expect("state");
        let outcomes = run_bench(
            &[route(provider)],
            std::slice::from_ref(&brief),
            &prices(),
            state.path(),
            &BenchOptions {
                reps: 1,
                dry_run: false,
                budget: WorkerBudget::default(),
            },
        )
        .await
        .expect("bench");
        assert_eq!(outcomes.len(), 1);
        let outcome = &outcomes[0];
        assert!(outcome.satisfied, "{outcome:?}");
        assert!(outcome.cost.expect("priced").micros() > 0);

        let store = ProfileStore::load(state.path()).expect("store");
        let tally = store
            .get("fake-model")
            .and_then(|p| p.tally(Source::Bench, TaskKind::Tests))
            .expect("tally");
        assert_eq!(tally.attempts, 1);
        assert_eq!(tally.satisfied, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failing_fake_records_an_unsatisfied_bench_tally() {
        let brief = tests_sum_brief();
        let state = tempfile::tempdir().expect("state");
        let outcomes = run_bench(
            &[route(failing_provider())],
            std::slice::from_ref(&brief),
            &prices(),
            state.path(),
            &BenchOptions {
                reps: 1,
                dry_run: false,
                budget: WorkerBudget::default(),
            },
        )
        .await
        .expect("bench");
        assert_eq!(outcomes.len(), 1);
        assert!(!outcomes[0].satisfied);

        let store = ProfileStore::load(state.path()).expect("store");
        let tally = store
            .get("fake-model")
            .and_then(|p| p.tally(Source::Bench, TaskKind::Tests))
            .expect("tally");
        assert_eq!(tally.attempts, 1);
        assert_eq!(tally.satisfied, 0);
    }

    /// No route is ever refused for price. A hosted route
    /// with no price entry still runs (the fake's `rounds` counter proves
    /// the provider was actually called), its cost comes back `None`, and
    /// the note says plain "unpriced" (not "local, unpriced": this route
    /// is not local) -- never a fabricated `$0`. The attempt's cost is not
    /// folded into the persisted `ProfileStore` tally, but `attempts`/
    /// `satisfied` reflect only what *did* get recorded (nothing here),
    /// while the outcome itself is still returned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_hosted_route_with_no_price_still_runs_and_is_marked_unpriced() {
        let brief = tests_sum_brief();
        let provider = satisfying_provider();
        let unpriced_route = RouteHandle {
            route: "hosted".into(),
            vendor: "hosted-vendor".into(),
            model: "nobody-prices-this-model".into(),
            provider: provider.clone(),
            local: false,
        };
        let state = tempfile::tempdir().expect("state");
        let outcomes = run_bench(
            &[unpriced_route],
            std::slice::from_ref(&brief),
            &prices(),
            state.path(),
            &BenchOptions {
                reps: 1,
                dry_run: false,
                budget: WorkerBudget::default(),
            },
        )
        .await
        .expect("bench");
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].cost, None);
        assert_eq!(outcomes[0].note.as_deref(), Some("unpriced"));
        assert!(
            *provider.rounds.lock().expect("lock") > 0,
            "an unpriced route must still be called"
        );

        let store = ProfileStore::load(state.path()).expect("store");
        let tally = store
            .get("nobody-prices-this-model")
            .and_then(|p| p.tally(Source::Bench, outcomes[0].kind))
            .expect("an unpriced attempt still counts toward the rate");
        assert_eq!((tally.attempts, tally.unpriced), (1, 1));
        assert_eq!(tally.cost_per_satisfied(), None, "unknown, never zero");
    }

    /// The same, for a local route: the note says "local, unpriced"
    /// (decision 0041: a local model is not free by default, so this is
    /// informational, not a claim of `$0`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_local_route_with_no_price_runs_and_is_marked_local_unpriced() {
        let brief = tests_sum_brief();
        let provider = satisfying_provider();
        let state = tempfile::tempdir().expect("state");
        let outcomes = run_bench(
            &[local_route(provider.clone(), "llama3-nobody-prices")],
            std::slice::from_ref(&brief),
            &prices(),
            state.path(),
            &BenchOptions {
                reps: 1,
                dry_run: false,
                budget: WorkerBudget::default(),
            },
        )
        .await
        .expect("bench");
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].cost, None);
        assert_eq!(outcomes[0].note.as_deref(), Some("local, unpriced"));
        assert!(*provider.rounds.lock().expect("lock") > 0);
    }

    /// A local route the price table *does* have an entry for is costed
    /// exactly like a hosted one -- decision 0041's "not special-cased to
    /// free" holds both ways.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_local_route_with_a_price_entry_is_charged_that_price() {
        let brief = tests_sum_brief();
        let provider = satisfying_provider();
        let state = tempfile::tempdir().expect("state");
        let outcomes = run_bench(
            &[local_route(provider, "fake-model")],
            std::slice::from_ref(&brief),
            &prices(),
            state.path(),
            &BenchOptions {
                reps: 1,
                dry_run: false,
                budget: WorkerBudget::default(),
            },
        )
        .await
        .expect("bench");
        assert_eq!(outcomes.len(), 1);
        let cost = outcomes[0].cost.expect("priced local route");
        assert!(cost.micros() > 0, "{cost:?}");
        assert_eq!(outcomes[0].note, None);

        let store = ProfileStore::load(state.path()).expect("store");
        let tally = store
            .get("fake-model")
            .and_then(|p| p.tally(Source::Bench, TaskKind::Tests))
            .expect("a priced local route is tallied like any other");
        assert_eq!(tally.attempts, 1);
    }

    /// A tally from an older suite version is set aside, not added to: the
    /// rate would otherwise mix runs of different briefs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_run_under_a_new_suite_does_not_add_to_an_old_suites_tally() {
        let state = tempfile::tempdir().expect("state");
        // Five satisfied Tests attempts recorded under an older suite.
        let mut old = ProfileStore::default();
        for _ in 0..5 {
            old.record(
                "fake-model",
                "fake-vendor",
                Source::Bench,
                TaskKind::Tests,
                true,
                Some(Money::from_micros(1)),
            );
        }
        suite_store::save(state.path(), &old, "older-suite-hash").expect("seed");
        let brief = tests_sum_brief();
        let opts = |dry_run| BenchOptions {
            reps: 1,
            dry_run,
            budget: WorkerBudget::default(),
        };
        let attempts = || {
            ProfileStore::load(state.path())
                .expect("store")
                .get("fake-model")
                .and_then(|p| p.tally(Source::Bench, TaskKind::Tests))
                .map(|t| (t.attempts, t.satisfied))
        };

        // A dry run leaves the store alone.
        run_bench(
            &[route(failing_provider())],
            std::slice::from_ref(&brief),
            &prices(),
            state.path(),
            &opts(true),
        )
        .await
        .expect("dry run");
        assert_eq!(attempts(), Some((5, 5)));

        // A real run (an empty attempt: unsatisfied, and counted) starts from
        // zero rather than from the old suite's five.
        run_bench(
            &[route(failing_provider())],
            std::slice::from_ref(&brief),
            &prices(),
            state.path(),
            &opts(false),
        )
        .await
        .expect("bench");
        assert_eq!(
            attempts(),
            Some((1, 0)),
            "the old suite's satisfied attempts must not be mixed in"
        );
        assert!(
            state
                .path()
                .join("model_profiles.oldersuitehash.json")
                .is_file(),
            "the old store is kept aside"
        );
    }

    #[tokio::test]
    async fn a_dry_run_makes_no_call_and_writes_nothing() {
        let brief = tests_sum_brief();
        let state = tempfile::tempdir().expect("state");
        let outcomes = run_bench(
            &[route(failing_provider())],
            std::slice::from_ref(&brief),
            &prices(),
            state.path(),
            &BenchOptions {
                reps: 2,
                dry_run: true,
                budget: WorkerBudget::default(),
            },
        )
        .await
        .expect("bench");
        assert_eq!(outcomes.len(), 2);
        // "fake-model" is priced, so the dry-run estimate is `Some`, not a
        // real cost (no call was made).
        assert!(outcomes.iter().all(|o| o.cost.is_some()));
        assert!(
            !state
                .path()
                .join(kgraf_core::model_profile::FILE_NAME)
                .exists(),
            "a dry run must not write the store"
        );
    }

    #[tokio::test]
    async fn a_dry_run_shows_unpriced_rather_than_a_number_for_an_unpriced_model() {
        let brief = tests_sum_brief();
        let unpriced_route = RouteHandle {
            route: "hosted".into(),
            vendor: "hosted-vendor".into(),
            model: "nobody-prices-this-model".into(),
            provider: failing_provider(),
            local: false,
        };
        let state = tempfile::tempdir().expect("state");
        let outcomes = run_bench(
            &[unpriced_route],
            std::slice::from_ref(&brief),
            &prices(),
            state.path(),
            &BenchOptions {
                reps: 1,
                dry_run: true,
                budget: WorkerBudget::default(),
            },
        )
        .await
        .expect("bench");
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].cost, None);
    }

    /// A deployment priced under its route-scoped key has an estimate; the bare
    /// model's price does not stand in for a deployment on a scoped-only route.
    #[test]
    fn the_estimate_is_route_aware() {
        let mut table = prices();
        table.require_route_scoped("scoped");
        let price = *table.get("fake-model").expect("price");
        table.set(PriceTable::route_key("dep", "scoped"), price);
        let descs = [
            ("scoped".to_owned(), "v".to_owned(), "dep".to_owned()),
            ("scoped".to_owned(), "v".to_owned(), "fake-model".to_owned()),
        ];
        let plan = plan_attempts(
            &descs,
            &[tests_sum_brief()],
            1,
            &table,
            &WorkerBudget::default(),
        );
        assert!(plan[0].estimated_cost.is_some(), "scoped price is used");
        assert_eq!(plan[1].estimated_cost, None, "bare price is not");
    }

    /// A deployment named like a bare-priced model runs unpriced: no abort after
    /// paid calls, no bare price applied.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_scoped_route_deployment_named_like_a_priced_model_runs_unpriced() {
        let mut table = prices();
        table.require_route_scoped("fake");
        let provider = satisfying_provider();
        let state = tempfile::tempdir().expect("state");
        let outcomes = run_bench(
            &[route(provider.clone())],
            std::slice::from_ref(&tests_sum_brief()),
            &table,
            state.path(),
            &BenchOptions {
                reps: 1,
                dry_run: false,
                budget: WorkerBudget::default(),
            },
        )
        .await
        .expect("the run is not aborted for price");
        assert_eq!(outcomes[0].cost, None);
        assert_eq!(outcomes[0].note.as_deref(), Some("unpriced"));
    }

    /// The dry-run estimate is the budget's rounds at its output ceiling,
    /// priced at the output rate: with the default budget, `kgraf worker`'s
    /// own, and with an override, the override.
    #[test]
    fn the_estimate_follows_the_budget_the_worker_gets() {
        let descs = [("r".to_owned(), "v".to_owned(), "fake-model".to_owned())];
        let briefs = [tests_sum_brief()];
        let estimate = |budget: &WorkerBudget| {
            plan_attempts(&descs, &briefs, 1, &prices(), budget)[0]
                .estimated_cost
                .expect("priced")
                .micros()
        };
        // `fake-model` output: $5 per million tokens, 5 micros per token.
        assert_eq!(
            estimate(&WorkerBudget::default()),
            i64::from(DEFAULT_STEP_BUDGET) * i64::from(DEFAULT_MAX_OUTPUT_TOKENS) * 5
        );
        let small = WorkerBudget {
            step_budget: 2,
            max_output_tokens: 100,
            context_window_tokens: 32_000,
        };
        assert_eq!(estimate(&small), 2 * 100 * 5);
    }

    /// A real attempt's requests carry the budget's output ceiling: the
    /// default is `kgraf worker`'s own, and an override reaches the provider.
    #[tokio::test]
    async fn an_attempt_requests_the_budgets_output_ceiling() {
        for budget in [
            WorkerBudget::default(),
            WorkerBudget {
                max_output_tokens: 777,
                ..WorkerBudget::default()
            },
        ] {
            let provider = Arc::new(Scripted {
                calls: Vec::new(),
                rounds: Mutex::new(0),
                max_output_tokens: Mutex::new(Vec::new()),
            });
            let state = tempfile::tempdir().expect("state");
            run_bench(
                &[route(provider.clone())],
                &[tests_sum_brief()],
                &prices(),
                state.path(),
                &BenchOptions {
                    reps: 1,
                    dry_run: false,
                    budget,
                },
            )
            .await
            .expect("bench");
            let seen = provider.max_output_tokens.lock().expect("lock").clone();
            assert!(!seen.is_empty());
            assert!(
                seen.iter().all(|m| *m == budget.max_output_tokens),
                "{seen:?} for {budget:?}"
            );
        }
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;
    use crate::brief::suite;
    use kgraf_core::Protocol;
    use kgraf_core::cost::ModelPrice;
    use kgraf_core::model_profile::ProfileStore;
    use kgraf_provider::capability::{Capabilities, StreamEvent, Streaming};
    use kgraf_provider::{CallToken, ProviderError, Request, Response};

    /// A provider that fails every call, as an outage or a bad key does.
    struct Down;

    fn outage() -> ProviderError {
        ProviderError::Http {
            adapter: "bench-test-down",
            status: 503,
            body: "unavailable".into(),
        }
    }

    #[async_trait::async_trait]
    impl Provider for Down {
        fn adapter(&self) -> &'static str {
            "bench-test-down"
        }
        fn protocol(&self) -> Protocol {
            Protocol::Additive
        }
        async fn complete(
            &self,
            _request: &Request,
            _token: &CallToken,
        ) -> Result<Response, ProviderError> {
            Err(outage())
        }
        fn capabilities(&self) -> Capabilities<'_> {
            Capabilities::none().with_streaming(self)
        }
    }

    #[async_trait::async_trait]
    impl Streaming for Down {
        async fn stream(
            &self,
            _request: &Request,
            _token: &CallToken,
            _sink: &mut (dyn FnMut(StreamEvent) + Send),
        ) -> Result<Response, ProviderError> {
            Err(outage())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_provider_error_is_returned_as_an_error_and_never_tallied() {
        let mut prices = PriceTable::new();
        prices.set(
            "down-model",
            ModelPrice {
                uncached_input: 1,
                cache_read: 1,
                cache_write: 1,
                output: 1,
                reasoning: 1,
            },
        );
        let route = RouteHandle {
            route: "down".into(),
            vendor: "down-vendor".into(),
            model: "down-model".into(),
            provider: Arc::new(Down),
            local: false,
        };
        let brief = suite()
            .into_iter()
            .find(|b| b.id == "tests-sum")
            .expect("brief");
        let state = tempfile::tempdir().expect("state");
        let outcomes = run_bench(
            &[route],
            std::slice::from_ref(&brief),
            &prices,
            state.path(),
            &BenchOptions {
                reps: 1,
                dry_run: false,
                budget: WorkerBudget::default(),
            },
        )
        .await
        .expect("a provider outage is an outcome, not a bench abort");
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].errored, "{:?}", outcomes[0]);
        assert!(!outcomes[0].satisfied);
        assert!(outcomes[0].note.is_some());

        let store = ProfileStore::load(state.path()).expect("store");
        assert!(
            store
                .get("down-model")
                .and_then(|p| p.tally(Source::Bench, TaskKind::Tests))
                .is_none(),
            "an outage must not read as an unsatisfied brief"
        );
    }

    #[test]
    fn only_the_models_own_failures_count_against_it() {
        assert!(model_caused(
            "the output ceiling was reached before the answer finished"
        ));
        assert!(model_caused(
            "the provider ended the turn with an unmodeled stop reason"
        ));
        // A refusal or a context overrun ends the turn with no answer: the
        // model's behavior, as the runtime words it.
        for stop_reason in ["refusal", "model_context_window_exceeded"] {
            let e = kgraf_provider::ProviderError::TurnNotUsable {
                adapter: "anthropic",
                stop_reason: stop_reason.into(),
            };
            assert!(model_caused(&e.to_string()), "{e}");
        }
        assert!(!model_caused("provider `x` returned HTTP 503: unavailable"));
        assert!(!model_caused("cannot open a ledger row: disk full"));
        assert!(!model_caused("something no one has seen before"));
    }

    #[test]
    fn only_a_price_that_cannot_cost_the_calls_reads_as_unpriced() {
        use kgraf_core::cost::CostError;
        use kgraf_provider::LedgerError;
        let money = Money::from_micros(5);
        assert_eq!(priced_or_unpriced(Ok(money)).expect("priced"), Some(money));
        for unpriced in [
            CostError::AboveTierThreshold {
                model: "m".into(),
                threshold: 200_000,
                input: 300_000,
            },
            CostError::UnpricedModel { model: "m".into() },
        ] {
            assert_eq!(
                priced_or_unpriced(Err(LedgerError::Cost(unpriced))).expect("unpriced"),
                None
            );
        }
        for broken in [
            LedgerError::Io("read failed".into()),
            LedgerError::WriterStopped,
        ] {
            assert!(
                priced_or_unpriced(Err(broken)).is_err(),
                "a broken ledger is not an unpriced model"
            );
        }
    }
}
