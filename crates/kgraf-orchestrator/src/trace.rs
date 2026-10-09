//! Real-use instrumentation for the six open questions of 0056/0057.
//!
//! The owner's rule: measured in real use, not in a soak run, so every record is a
//! trace field written while the harness works, locally. One JSON line per
//! event in the run directory; the ledger already holds per-call tokens and cost,
//! and trace records name ledger calls rather than copying them.
//!
//! | Question | Record |
//! |---|---|
//! | 1. context cap per model | [`TraceEvent::WorkerFinished`]: peak context, outcome, projection |
//! | 2. loss from repeated compaction | [`TraceEvent::Compaction`]: every stop-point handoff |
//! | 3. drift from intent | [`TraceEvent::Regrounding`]: every verdict |
//! | 4. non-identical loops | [`TraceEvent::LoopPattern`], from [`LoopDetector`] |
//! | 5. graph memory sufficiency | [`TraceEvent::EmptyGraphLookup`] |
//! | 6. spend pacing | [`TraceEvent::Spend`]: trailing-hour spend against the caps |
//!
//! Question 2 asks for facts retained per cycle. Workers never compact (0057); the
//! orchestrator holds no growing transcript (its state is the run record), so the
//! compaction events recorded are worker stop points. Fact retention needs a probe
//! of what a continuation still knows, which is not built; the event carries the
//! handoff's done and remaining counts so a retention check can be joined later.

use crate::run_state::GroundingTrigger;
use crate::sizing::ProjectionSource;
use kgraf_session::SessionError;
use kgraf_session::transcript::{JsonlTranscript, TornTail, Transcript};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// A recognized loop shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopKind {
    /// The same tool call repeated inside one worker (the runtime's detector).
    IdenticalCalls,
    /// A, B, A: a lineage's attempts changed the same file set, then another,
    /// then the first again.
    Alternating,
    /// A contract failed again with the same change set: a variant of one
    /// failed fix.
    RepeatedFailedFix,
}

/// Why a planned contract did not dispatch this round.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferReason {
    /// Its write set is leased by another contract (0048): a serialization.
    Lease,
    /// The work-item cap or live-agent ceiling (0017).
    Capacity,
    /// A transient readiness failure.
    Readiness,
}

/// A contract as the plan was accepted. The instruction is written in full:
/// trace lines have no size cap.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlannedContract {
    pub id: String,
    /// The instruction as stored for dispatch (the brief's instruction part;
    /// the worker's brief adds the acceptance, the write list and later
    /// sections around it).
    pub instruction: String,
    pub acceptance: String,
    pub writes: Vec<String>,
    pub depends_on: Vec<String>,
    /// The route the plan pinned, when it pinned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_kind: Option<kgraf_core::model_profile::TaskKind>,
}

/// An interface as the plan was accepted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlannedInterface {
    pub id: String,
    pub parties: Vec<String>,
    pub symbols: Vec<String>,
}

/// One trace record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TraceEvent {
    Dispatch {
        at_ms: i64,
        contract: String,
        route: String,
        model: String,
        workers: Vec<String>,
        projected_tokens: u64,
        target_tokens: u64,
        projection: ProjectionSource,
        estimate_micros: i64,
        /// blake3 hex digest of the instruction the first attempt's worker
        /// opens with (its brief, without the slices that follow it). Empty
        /// in a trace written before the field existed, and when no attempt
        /// got as far as building a brief.
        #[serde(default)]
        brief_digest: String,
        /// The same digest per attempt, keyed by worker id. Attempts that
        /// built no brief (no worktree) are absent.
        #[serde(default)]
        brief_digests: std::collections::BTreeMap<String, String>,
        /// The commit the attempts' worktrees start from: the integration
        /// branch tip when the dispatch began (the commit a remediation names
        /// instead, when the contract carries one). Absent when the workspace
        /// provider has no integration branch, and in a trace written before
        /// the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_commit: Option<String>,
        /// The same commit per attempt, keyed by worker id.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        base_commits: std::collections::BTreeMap<String, String>,
    },
    /// A plan was accepted for dispatch: written once per accepted plan, after
    /// intake normalization and any re-asks, with the contracts as they will
    /// be dispatched. A re-plan that replaces a held plan writes another
    /// event under the next `revision`. A resume that interrupts the plan
    /// between this record and the saved record can write the same revision
    /// twice; the contents are the same.
    PlanAccepted {
        at_ms: i64,
        /// 1 for the first accepted plan; one more for each re-plan.
        revision: u32,
        contracts: Vec<PlannedContract>,
        /// The interfaces the plan registered; empty when it has none.
        #[serde(default)]
        interfaces: Vec<PlannedInterface>,
        /// Notices raised while the plan was taken in (repairs, the unit
        /// struct shape notice, "contract checks did not run", re-ask
        /// reasons), scrubbed.
        #[serde(default)]
        notices: Vec<String>,
    },
    Deferred {
        at_ms: i64,
        contract: String,
        reason: DeferReason,
        detail: String,
    },
    /// Question 1: context size and outcome per worker.
    WorkerFinished {
        at_ms: i64,
        contract: String,
        worker: String,
        route: String,
        model: String,
        outcome: String,
        steps: u32,
        peak_context_tokens: u64,
        projected_tokens: u64,
        stop_tokens: u64,
        /// The route's effective window, so peak context reads as a share of
        /// it. Zero in a trace written before the field existed, and for a
        /// worker whose window is not known.
        #[serde(default)]
        window_tokens: u64,
        sizing_miss: bool,
        calls: Vec<String>,
        spend_micros: i64,
        /// Why a `failed` worker failed, as the runtime said it (a provider
        /// refusal carries its HTTP status and body), scrubbed for storage.
        /// Absent for any other outcome, and in a trace written before the
        /// field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// The stop reason of the attempt's last model round that ended on its
        /// own, normalized (`end_turn`, `tool_use`, `max_tokens`,
        /// `stop_sequence`, `pause_turn`, `other`). Absent when no round
        /// ended, and in a trace written before the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_stop: Option<kgraf_provider::StopReason>,
        /// Tool calls the attempt started. Zero in a trace written before the
        /// field existed.
        #[serde(default)]
        tool_calls: u32,
    },
    /// Question 2.
    Compaction {
        at_ms: i64,
        contract: String,
        worker: String,
        trigger: String,
        context_tokens: u64,
        done_items: usize,
        remaining_items: usize,
        /// What the worker measured against its stop point
        /// (`kgraf_runtime::HandoffCause::as_str`): a hand-back is never a
        /// projection. Empty in a trace from before the field.
        #[serde(default)]
        cause: String,
        /// For a worker that could not hand back and had compacted: why it
        /// was not compacted again (`kgraf_runtime::CompactionStop`). Absent
        /// for every other hand-back and in a trace from before the field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compaction_stop: Option<kgraf_runtime::CompactionStop>,
    },
    /// The runtime compacted a worker (decision 0095): its fixed notice, as
    /// the worker emitted it.
    WorkerCompacted {
        at_ms: i64,
        contract: String,
        worker: String,
        notice: kgraf_runtime::CompactionNotice,
    },
    /// A worker reached a round checkpoint (decision 0096): every checkpoint
    /// it reached, with the measurement and verdict of each. `extended` is
    /// the sizing signal: the worker ran past its planned round budget.
    WorkerCheckpoints {
        at_ms: i64,
        contract: String,
        worker: String,
        extended: bool,
        blocks: kgraf_runtime::RoundBlocks,
    },
    /// Question 3.
    Regrounding {
        at_ms: i64,
        trigger: GroundingTrigger,
        contract: String,
        serves: bool,
        reason: String,
        call: Option<String>,
    },
    /// Question 4.
    LoopPattern {
        at_ms: i64,
        contract: String,
        kind: LoopKind,
        detail: String,
    },
    /// Question 5.
    EmptyGraphLookup {
        at_ms: i64,
        contract: String,
        lookup: String,
    },
    /// The full index pass that opened the run found languages whose
    /// extractor changed since their files were indexed (decision 0065): the
    /// pass re-derived those files although their bytes did not change.
    /// Recorded so the report of that pass is not dropped.
    ExtractorChanged {
        at_ms: i64,
        changes: Vec<kgraf_graph::ExtractorChange>,
    },
    /// Question 6. Accounting only (decision 0080 removed every spend cap).
    Spend {
        at_ms: i64,
        hour_spent_micros: i64,
        held_micros: i64,
        run_spent_micros: i64,
    },
    Verdict {
        at_ms: i64,
        contract: String,
        worker: String,
        passed: bool,
        detail: String,
    },
    /// A worker reported that it cannot do its brief as given, and how the
    /// orchestrator resolved it (decision 0093): `granted ...`, `the contract
    /// was amended ...`, `waiting for the user: ...`, `held: ...`,
    /// `answered: ...` (nothing granted or held; the worker continues) or
    /// `stopped: ...` (the attempt is stuck). `kind` is `permission` or
    /// `instructions`; for a grounded permission report `need` is the refused
    /// subject and `why` ends with the worker's own words.
    Shortfall {
        at_ms: i64,
        contract: String,
        worker: String,
        kind: String,
        need: String,
        why: String,
        resolution: String,
    },
    /// A permission event (decision 0094): a mode change, a plan-review
    /// change, a grant (by a mode, the orchestrator or the user), a request
    /// asked of the user, or a denial. `contract` and `worker` are empty for
    /// a mode change.
    Permission {
        at_ms: i64,
        contract: String,
        worker: String,
        what: String,
        detail: String,
    },
    /// The plan was shown for review, or answered (decision 0094):
    /// `outcome` is `shown`, `approved`, `rejected` or `changed`.
    PlanReview {
        at_ms: i64,
        revision: u32,
        outcome: String,
        detail: String,
    },
    /// A contract accepted on a check that says nothing about the work
    /// (decision 0093): reported unverified, never plainly done.
    Unverified {
        at_ms: i64,
        contract: String,
        worker: String,
        reason: String,
    },
    /// A passing verdict's notice: a fact for the owner, not a failure (a
    /// lockfile the check generated and that was not merged).
    Notice {
        at_ms: i64,
        contract: String,
        worker: String,
        detail: String,
    },
    /// The planner's answer was refused (not the JSON asked for, or a plan that
    /// breaks a rule). One per refusal, the last one included, so a run that
    /// fails planning still shows what it was told and how often. `reason` is
    /// the refusal text; for an answer that is not JSON it carries the start
    /// of the answer.
    PlanRefused {
        at_ms: i64,
        /// Which ask was refused, counting from 1.
        ask: u32,
        /// How many asks the run allows.
        of: u32,
        reason: String,
    },
    /// The interface call's answer was refused, in part or whole: every
    /// interface that was not accepted, and every consumed edge still
    /// unbound, with what each needs. One per refused ask, the last one
    /// included. Apart from `plan_refused`: the contracts were accepted, and
    /// are never asked for again because an interface was wrong.
    InterfacesRefused {
        at_ms: i64,
        /// Which ask of the interface call, counting from 1.
        ask: u32,
        /// How many asks the interface call allows.
        of: u32,
        reason: String,
    },
    Report {
        at_ms: i64,
        kind: String,
        contract: String,
        worker: String,
        note: String,
        recipients: usize,
        interrupt: bool,
    },
    /// A contract's wait state changed kind (`waits.rs`): a lease wait, a
    /// dependency block and a capacity hold are distinct records.
    Wait {
        at_ms: i64,
        contract: String,
        wait: crate::waits::WaitKind,
        detail: String,
    },
    /// A quality gate changed state (0018): planned, running, passed, failed with
    /// its remediation, held; or, with an empty `gate`, the notice that the run
    /// has no gates and why.
    Gate {
        at_ms: i64,
        gate: String,
        state: String,
        detail: String,
    },
    /// Speculative merge across in-flight branches (0074): a `textual` or
    /// `semantic` conflict between `contracts`, found before either landed;
    /// `unvalidated` (no validation commands, so the semantic class is not
    /// checked); or `refused` (a round could not run, with why).
    Speculation {
        at_ms: i64,
        contracts: Vec<String>,
        outcome: String,
        detail: String,
    },
    /// Continuous merge-back of a verified attempt (0018): merged, empty,
    /// unversioned, a conflict (re-dispatched) or a refusal.
    Integration {
        at_ms: i64,
        contract: String,
        outcome: String,
        detail: String,
    },
    /// A worker-to-worker message: posted (with its recipients and files),
    /// delivered in a brief, or refused (an empty or overlong body).
    Message {
        at_ms: i64,
        id: String,
        action: String,
        from: String,
        to: Vec<String>,
        detail: String,
    },
    /// Interface-contract checks, challenges and revisions (0027-0037), with the
    /// cumulative counts after the event. A resume replays these into the book.
    Contract {
        at_ms: i64,
        record: Box<crate::challenge::ContractEvent>,
        counters: crate::challenge::ContractCounters,
    },
    /// Workflow templates (0056): `offered` to the first decomposition
    /// (an empty list is recorded too, so the run's demand-comparison arm is known), `mined`
    /// from a completed run, or `failed_on_reuse`.
    Templates {
        at_ms: i64,
        action: String,
        templates: Vec<String>,
        detail: String,
    },
    /// The run's induced-demand record, written when the run ends.
    Demand {
        at_ms: i64,
        record: Box<crate::demand::DemandRecord>,
    },
    /// A vendor/model routing decision (decision 0075): the policy in force,
    /// the contract's coupling cluster (`None` outside `RoutingPolicy::
    /// Fitness`, or for an uncoupled contract), the route chosen, where its
    /// figures came from (`"live"`, `"bench"` or `"unmeasured"`), the tally
    /// figures used (`None` when unmeasured), whether escalation had to leave
    /// the cluster's vendor, and the reason. Emitted for every Fitness pick;
    /// Rotation never emits this event.
    RouteDecision {
        at_ms: i64,
        contract: String,
        policy: crate::routing::RoutingPolicy,
        cluster: Option<usize>,
        route: String,
        source: String,
        satisfaction_rate: Option<f64>,
        cost_per_satisfied_micros: Option<i64>,
        crossed_cluster: bool,
        reason: String,
    },
    /// A worker's model call failed at the transport (`route_health.rs`):
    /// the route is out of rotation until `out_until_ms`, and the attempt is
    /// not counted against its contract.
    RouteFault {
        at_ms: i64,
        route: String,
        contract: String,
        worker: String,
        /// Faults in a row on this route.
        in_a_row: u32,
        /// Faults on this route over the run.
        total: u32,
        out_until_ms: i64,
        detail: String,
    },
    /// A route that had faulted answered again.
    RouteRecovered {
        at_ms: i64,
        route: String,
        /// The faults in a row it recovered from.
        after: u32,
    },
    /// A contract whose last attempt ended on a route fault was dispatched
    /// on another route.
    Rerouted {
        at_ms: i64,
        contract: String,
        from: String,
        to: String,
        reason: String,
    },
    /// An orchestrator call that failed and was asked again
    /// (`ModelCaller::ask_json`): the failed call's id and what the provider
    /// said. The call's ledger row settles unreconciled with no error text,
    /// so this record is the only evidence of why. A reader tells a local
    /// server's fault (`status` 5xx, or `server_parse` with status 200) from
    /// a call dropped mid-flight by these fields, never by reading `error`.
    CallRetried {
        at_ms: i64,
        /// The ledger id of the call that failed.
        call: String,
        /// The ledger agent that made it.
        agent: String,
        /// The call kind.
        what: String,
        /// The HTTP status of a provider answer; absent for a failure that
        /// was not one (an answer that did not parse as the JSON asked for).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        /// The server could not parse the model's own output.
        #[serde(default)]
        server_parse: bool,
        /// The failure, bounded to 2 KiB.
        error: String,
    },
    /// An orchestrator answer the model ended itself before its JSON object
    /// closed, closed by appending the closing brackets and braces it lacked
    /// (`ModelCaller::ask_read`). The closed answer passed the same parsing
    /// a whole answer passes. No call was added: the answer is the one of
    /// `call`'s ledger row, which this record says was changed.
    AnswerRepaired {
        at_ms: i64,
        /// The ledger id of the call whose answer was closed.
        call: String,
        /// The ledger agent that made it.
        agent: String,
        /// The call kind.
        what: String,
        /// What was appended, in order.
        appended: String,
    },
    /// An orchestrator answer the model ended itself before its JSON object
    /// closed and that could not be closed, so the same request was sent once
    /// more (`ModelCaller::ask_read`). That second call is part of the same
    /// ask and does not count against the ask budget; it is ledgered and
    /// accounted like any call. A second incomplete answer is the ask's
    /// refusal and counts.
    AnswerReasked {
        at_ms: i64,
        /// The ledger id of the call whose answer stopped early.
        call: String,
        /// The ledger agent that made it.
        agent: String,
        /// The call kind.
        what: String,
        /// Why the answer could not be closed, and its refusal, bounded to
        /// 2 KiB.
        reason: String,
    },
}

/// Persist-scrub every string value of a [`TraceEvent`] (decision 0071), including the ones nested
/// in `Contract` and `Demand` records: a check failure's `detail`, a check that could not run's
/// `error`, an inadequacy `note`, a challenge's `claim` and quoted `Evidence`, a lookup, a
/// rejection's reason. Any of these can quote a secret a tool printed or a file held.
///
/// The walk goes over the event's JSON form, so a field added to any nested
/// record is scrubbed without a line here; object keys are left alone. A
/// structured value (an id, an enum name, a count) holds no secret shape, so
/// the walk leaves it byte for byte. If a scrubbed value ever failed to read
/// back as a `TraceEvent`, [`scrub_trace_fields`] scrubs the named free-text
/// fields instead, rather than write the event unscrubbed.
fn scrub_trace_event(event: TraceEvent) -> TraceEvent {
    let walked = serde_json::to_value(&event)
        .ok()
        .map(scrub_json_strings)
        .and_then(|v| serde_json::from_value::<TraceEvent>(v).ok());
    match walked {
        Some(scrubbed) => scrubbed,
        None => scrub_trace_fields(event),
    }
}

/// Every string value in `value`, persist-scrubbed; keys unchanged.
fn scrub_json_strings(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(s) => Value::String(kgraf_secrets::scrub_persist_owned(s)),
        Value::Array(items) => Value::Array(items.into_iter().map(scrub_json_strings).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, scrub_json_strings(v)))
                .collect(),
        ),
        other => other,
    }
}

/// The per-field fallback for [`scrub_trace_event`]: the free-text fields of
/// the flat variants. The boxed `Contract` and `Demand` records are replaced
/// whole by a note if they ever reach here, since their fields cannot be
/// listed without drifting from their types.
fn scrub_trace_fields(event: TraceEvent) -> TraceEvent {
    fn s(text: String) -> String {
        match kgraf_secrets::scrub_persist(&text) {
            std::borrow::Cow::Borrowed(_) => text,
            std::borrow::Cow::Owned(scrubbed) => scrubbed,
        }
    }
    match event {
        TraceEvent::Deferred {
            at_ms,
            contract,
            reason,
            detail,
        } => TraceEvent::Deferred {
            at_ms,
            contract,
            reason,
            detail: s(detail),
        },
        TraceEvent::Regrounding {
            at_ms,
            trigger,
            contract,
            serves,
            reason,
            call,
        } => TraceEvent::Regrounding {
            at_ms,
            trigger,
            contract,
            serves,
            reason: s(reason),
            call,
        },
        TraceEvent::LoopPattern {
            at_ms,
            contract,
            kind,
            detail,
        } => TraceEvent::LoopPattern {
            at_ms,
            contract,
            kind,
            detail: s(detail),
        },
        TraceEvent::Verdict {
            at_ms,
            contract,
            worker,
            passed,
            detail,
        } => TraceEvent::Verdict {
            at_ms,
            contract,
            worker,
            passed,
            detail: s(detail),
        },
        TraceEvent::Notice {
            at_ms,
            contract,
            worker,
            detail,
        } => TraceEvent::Notice {
            at_ms,
            contract,
            worker,
            detail: s(detail),
        },
        TraceEvent::PlanRefused {
            at_ms,
            ask,
            of,
            reason,
        } => TraceEvent::PlanRefused {
            at_ms,
            ask,
            of,
            reason: s(reason),
        },
        TraceEvent::CallRetried {
            at_ms,
            call,
            agent,
            what,
            status,
            server_parse,
            error,
        } => TraceEvent::CallRetried {
            at_ms,
            call,
            agent,
            what,
            status,
            server_parse,
            error: s(error),
        },
        TraceEvent::AnswerReasked {
            at_ms,
            call,
            agent,
            what,
            reason,
        } => TraceEvent::AnswerReasked {
            at_ms,
            call,
            agent,
            what,
            reason: s(reason),
        },
        TraceEvent::InterfacesRefused {
            at_ms,
            ask,
            of,
            reason,
        } => TraceEvent::InterfacesRefused {
            at_ms,
            ask,
            of,
            reason: s(reason),
        },
        TraceEvent::Shortfall {
            at_ms,
            contract,
            worker,
            kind,
            need,
            why,
            resolution,
        } => TraceEvent::Shortfall {
            at_ms,
            contract,
            worker,
            kind,
            need: s(need),
            why: s(why),
            resolution: s(resolution),
        },
        TraceEvent::Permission {
            at_ms,
            contract,
            worker,
            what,
            detail,
        } => TraceEvent::Permission {
            at_ms,
            contract,
            worker,
            what,
            detail: s(detail),
        },
        TraceEvent::PlanReview {
            at_ms,
            revision,
            outcome,
            detail,
        } => TraceEvent::PlanReview {
            at_ms,
            revision,
            outcome,
            detail: s(detail),
        },
        TraceEvent::Unverified {
            at_ms,
            contract,
            worker,
            reason,
        } => TraceEvent::Unverified {
            at_ms,
            contract,
            worker,
            reason: s(reason),
        },
        TraceEvent::Report {
            at_ms,
            kind,
            contract,
            worker,
            note,
            recipients,
            interrupt,
        } => TraceEvent::Report {
            at_ms,
            kind,
            contract,
            worker,
            note: s(note),
            recipients,
            interrupt,
        },
        TraceEvent::Wait {
            at_ms,
            contract,
            wait,
            detail,
        } => TraceEvent::Wait {
            at_ms,
            contract,
            wait,
            detail: s(detail),
        },
        TraceEvent::Gate {
            at_ms,
            gate,
            state,
            detail,
        } => TraceEvent::Gate {
            at_ms,
            gate,
            state,
            detail: s(detail),
        },
        TraceEvent::Speculation {
            at_ms,
            contracts,
            outcome,
            detail,
        } => TraceEvent::Speculation {
            at_ms,
            contracts,
            outcome,
            detail: s(detail),
        },
        TraceEvent::Integration {
            at_ms,
            contract,
            outcome,
            detail,
        } => TraceEvent::Integration {
            at_ms,
            contract,
            outcome,
            detail: s(detail),
        },
        TraceEvent::Message {
            at_ms,
            id,
            action,
            from,
            to,
            detail,
        } => TraceEvent::Message {
            at_ms,
            id,
            action,
            from,
            to,
            detail: s(detail),
        },
        TraceEvent::Templates {
            at_ms,
            action,
            templates,
            detail,
        } => TraceEvent::Templates {
            at_ms,
            action,
            templates,
            detail: s(detail),
        },
        TraceEvent::EmptyGraphLookup {
            at_ms,
            contract,
            lookup,
        } => TraceEvent::EmptyGraphLookup {
            at_ms,
            contract,
            lookup: s(lookup),
        },
        // Not reached in practice (the JSON walk handles them); dropped to a
        // note rather than written with text nobody scrubbed.
        TraceEvent::Contract { at_ms, .. } | TraceEvent::Demand { at_ms, .. } => {
            TraceEvent::Deferred {
                at_ms,
                contract: String::new(),
                reason: DeferReason::Readiness,
                detail: "a trace record could not be scrubbed and was not written".into(),
            }
        }
        TraceEvent::RouteDecision {
            at_ms,
            contract,
            policy,
            cluster,
            route,
            source,
            satisfaction_rate,
            cost_per_satisfied_micros,
            crossed_cluster,
            reason,
        } => TraceEvent::RouteDecision {
            at_ms,
            contract,
            policy,
            cluster,
            route,
            source,
            satisfaction_rate,
            cost_per_satisfied_micros,
            crossed_cluster,
            reason: s(reason),
        },
        // The notice is structured; only a fallback's reason can carry a
        // provider's text.
        TraceEvent::WorkerCompacted {
            at_ms,
            contract,
            worker,
            mut notice,
        } => {
            if let kgraf_runtime::SummarySource::Runtime { reason } = notice.summary_source {
                notice.summary_source = kgraf_runtime::SummarySource::Runtime { reason: s(reason) };
            }
            TraceEvent::WorkerCompacted {
                at_ms,
                contract,
                worker,
                notice,
            }
        }
        // The measurement is structured; only an improvement line quotes a
        // command's first words.
        TraceEvent::WorkerCheckpoints {
            at_ms,
            contract,
            worker,
            extended,
            mut blocks,
        } => {
            for checkpoint in &mut blocks.checkpoints {
                checkpoint.measure.checks_improved =
                    std::mem::take(&mut checkpoint.measure.checks_improved)
                        .into_iter()
                        .map(s)
                        .collect();
            }
            TraceEvent::WorkerCheckpoints {
                at_ms,
                contract,
                worker,
                extended,
                blocks,
            }
        }
        TraceEvent::RouteFault {
            at_ms,
            route,
            contract,
            worker,
            in_a_row,
            total,
            out_until_ms,
            detail,
        } => TraceEvent::RouteFault {
            at_ms,
            route,
            contract,
            worker,
            in_a_row,
            total,
            out_until_ms,
            detail: s(detail),
        },
        TraceEvent::Rerouted {
            at_ms,
            contract,
            from,
            to,
            reason,
        } => TraceEvent::Rerouted {
            at_ms,
            contract,
            from,
            to,
            reason: s(reason),
        },
        TraceEvent::PlanAccepted {
            at_ms,
            revision,
            mut contracts,
            interfaces,
            notices,
        } => {
            for c in &mut contracts {
                c.instruction = s(std::mem::take(&mut c.instruction));
                c.acceptance = s(std::mem::take(&mut c.acceptance));
            }
            TraceEvent::PlanAccepted {
                at_ms,
                revision,
                contracts,
                interfaces,
                notices: notices.into_iter().map(s).collect(),
            }
        }
        other @ (TraceEvent::Dispatch { .. }
        | TraceEvent::RouteRecovered { .. }
        | TraceEvent::AnswerRepaired { .. }
        | TraceEvent::WorkerFinished { .. }
        | TraceEvent::Compaction { .. }
        | TraceEvent::ExtractorChanged { .. }
        | TraceEvent::Spend { .. }) => other,
    }
}

/// The trace file.
pub struct TraceLog {
    file: JsonlTranscript<TraceEvent>,
}

impl TraceLog {
    /// Open or create; an existing trace (a resumed run) is appended to. A
    /// torn last line is refused: see [`TraceLog::open_repairing`].
    pub async fn open(path: &Path) -> Result<Self, SessionError> {
        Ok(Self {
            file: JsonlTranscript::open(path.to_path_buf()).await?,
        })
    }

    /// As [`TraceLog::open`], for the run that owns the trace: a torn last
    /// line (a kill mid-append) is dropped and returned, so the caller
    /// records what was lost and the run resumes; other corruption is still
    /// refused.
    pub async fn open_repairing(path: &Path) -> Result<(Self, Option<TornTail>), SessionError> {
        let (file, torn) = JsonlTranscript::open_repairing_torn_tail(path.to_path_buf()).await?;
        Ok((Self { file }, torn))
    }

    pub async fn record(&mut self, event: TraceEvent) -> Result<(), SessionError> {
        self.file.append(scrub_trace_event(event)).await.map(|_| ())
    }

    pub async fn load(&self) -> Result<Vec<TraceEvent>, SessionError> {
        Ok(self
            .file
            .load()
            .await?
            .into_iter()
            .map(|e| e.event)
            .collect())
    }
}

/// Detects the non-identical loop shapes across attempts.
#[derive(Clone, Debug, Default)]
pub struct LoopDetector {
    /// Per lineage root, the change-set fingerprint of each finished attempt.
    history: BTreeMap<String, Vec<String>>,
    /// Per contract, the fingerprint of its last failed attempt.
    failed: BTreeMap<String, String>,
}

/// A change set's fingerprint: its sorted, joined paths.
pub fn fingerprint(paths: &[std::path::PathBuf]) -> String {
    let mut p: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    p.sort();
    p.join("\n")
}

impl LoopDetector {
    /// Observe a finished attempt. Returns the shapes it completes.
    pub fn observe(
        &mut self,
        root: &str,
        contract: &str,
        changed: &[std::path::PathBuf],
        failed: bool,
    ) -> Vec<(LoopKind, String)> {
        let mut found = Vec::new();
        let fp = fingerprint(changed);
        if failed {
            if self.failed.get(contract) == Some(&fp) && !fp.is_empty() {
                found.push((
                    LoopKind::RepeatedFailedFix,
                    format!("contract {contract} failed twice changing {fp:?}"),
                ));
            }
            self.failed.insert(contract.to_owned(), fp.clone());
        }
        if !fp.is_empty() {
            let h = self.history.entry(root.to_owned()).or_default();
            h.push(fp);
            let n = h.len();
            if n >= 3 && h[n - 1] == h[n - 3] && h[n - 1] != h[n - 2] {
                found.push((
                    LoopKind::Alternating,
                    format!(
                        "lineage {root}: {:?} -> {:?} -> {:?}",
                        h[n - 3],
                        h[n - 2],
                        h[n - 1]
                    ),
                ));
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &[&str]) -> Vec<PathBuf> {
        s.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn an_a_b_a_edit_sequence_in_one_lineage_is_detected() {
        let mut d = LoopDetector::default();
        assert!(d.observe("c1", "c1", &p(&["a.rs"]), false).is_empty());
        assert!(d.observe("c1", "c1.1", &p(&["b.rs"]), false).is_empty());
        let found = d.observe("c1", "c1.2", &p(&["a.rs"]), false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, LoopKind::Alternating);
        // Another lineage's edits do not complete this one's pattern.
        assert!(d.observe("c2", "c2", &p(&["a.rs"]), false).is_empty());
    }

    #[test]
    fn a_second_failure_with_the_same_change_set_is_a_repeated_failed_fix() {
        let mut d = LoopDetector::default();
        assert!(d.observe("c1", "c1", &p(&["a.rs"]), true).is_empty());
        let found = d.observe("c1", "c1", &p(&["a.rs"]), true);
        assert!(found.iter().any(|(k, _)| *k == LoopKind::RepeatedFailedFix));
        // A different change set is a different fix.
        assert!(
            !d.observe("c1", "c1", &p(&["b.rs"]), true)
                .iter()
                .any(|(k, _)| *k == LoopKind::RepeatedFailedFix)
        );
    }

    #[tokio::test]
    async fn trace_records_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("trace.jsonl");
        let mut log = TraceLog::open(&path).await.expect("open");
        let event = TraceEvent::EmptyGraphLookup {
            at_ms: 1,
            contract: "c1".into(),
            lookup: "symbol `Parser`".into(),
        };
        log.record(event.clone()).await.expect("record");
        assert_eq!(log.load().await.expect("load"), vec![event]);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains(r#""event":"empty_graph_lookup""#));
    }

    /// `TraceLog::record` scrubs both a registered exact secret and a
    /// gitleaks-shaped pattern secret out of every free-text trace field,
    /// covering these fields (a verdict's `reason`/`detail`,
    /// a gate's `detail`, a report's `note`, a message's `detail`).
    #[tokio::test]
    async fn record_scrubs_a_registered_key_and_a_pattern_secret_from_every_free_text_field() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("trace.jsonl");
        let mut log = TraceLog::open(&path).await.expect("open");

        let registered = "kgraf-trace-registered-Qm7Xv2Lp9Rz4Tn8Wb";
        kgraf_secrets::register_exact_secret("test-trace-secret", registered);
        let pattern = format!("ghp_{}", "a".repeat(36));

        let events = vec![
            TraceEvent::Verdict {
                at_ms: 1,
                contract: "c1".into(),
                worker: "w1".into(),
                passed: false,
                detail: format!("stderr tail: {registered}\nstdout tail: {pattern}"),
            },
            TraceEvent::Gate {
                at_ms: 2,
                gate: "g1".into(),
                state: "failed".into(),
                detail: format!("gate output carried {registered} and {pattern}"),
            },
            TraceEvent::Report {
                at_ms: 3,
                kind: "worker".into(),
                contract: "c1".into(),
                worker: "w1".into(),
                note: format!("model text: {registered} {pattern}"),
                recipients: 0,
                interrupt: false,
            },
            TraceEvent::Message {
                at_ms: 4,
                id: "m1".into(),
                action: "posted".into(),
                from: "w1".into(),
                to: vec!["w2".into()],
                detail: format!("summary: {registered} {pattern}"),
            },
        ];
        for event in events {
            log.record(event).await.expect("record");
        }

        let loaded = log.load().await.expect("load");
        let all_detail = format!("{loaded:?}");
        assert!(!all_detail.contains(registered), "{all_detail}");
        assert!(!all_detail.contains(&pattern), "{all_detail}");
        assert!(all_detail.contains("[redacted:test-trace-secret;"));
        assert!(all_detail.contains("[redacted:github-pat;"));

        // On disk too, not only in the round-tripped value.
        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(!on_disk.contains(registered));
        assert!(!on_disk.contains(&pattern));
    }

    /// The nested `Contract` records, `EmptyGraphLookup`
    /// and `Demand` are scrubbed too. Every string field of each event holds
    /// the secret; none reaches the file.
    #[tokio::test]
    async fn record_scrubs_nested_contract_lookup_and_demand_text() {
        use crate::challenge::{
            Basis, Challenge, Challenger, ContractCounters, ContractEvent, Evidence, Resolution,
        };
        use crate::checks::{CheckFailure, CheckStage};
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("trace.jsonl");
        let mut log = TraceLog::open(&path).await.expect("open");
        let secret = format!("ghp_{}", "Zq8vN2kLm4Rt6Yw1Xc3Bp5Hd7Jf9Gs0Ae2Ui");
        let t = |label: &str| format!("{label} {secret}");
        let contract = |record: ContractEvent| TraceEvent::Contract {
            at_ms: 1,
            record: Box::new(record),
            counters: ContractCounters::default(),
        };
        let events = vec![
            contract(ContractEvent::CheckFailed {
                interface: "i".into(),
                version: 1,
                failure: CheckFailure {
                    stage: CheckStage::StubCompile,
                    artifact: t("artifact"),
                    detail: t("detail"),
                },
                findings: Vec::new(),
                handshake: Vec::new(),
                held: vec![t("held")],
                parties: Vec::new(),
            }),
            contract(ContractEvent::CheckNotRun {
                interface: "i".into(),
                version: 1,
                error: t("error"),
                held: Vec::new(),
                parties: Vec::new(),
            }),
            contract(ContractEvent::Inadequate {
                interface: "i".into(),
                version: 1,
                worker: kgraf_core::AgentId::new("w1"),
                contract: "c1".into(),
                note: t("note"),
                peers: Vec::new(),
                held: Vec::new(),
            }),
            contract(ContractEvent::Challenged {
                interface: "i".into(),
                version: 1,
                challenge: Challenge {
                    interface: "i".into(),
                    challengers: vec![Challenger {
                        agent: kgraf_core::AgentId::new("w1"),
                        vendor: "v".into(),
                        contract: "c1".into(),
                    }],
                    claim: t("claim"),
                    evidence: Evidence::Quote {
                        artifact: "docs/x.md".into(),
                        text: t("quote"),
                    },
                },
                vendors: 1,
                resolution: Resolution::Rejected {
                    basis: Basis::Judgment,
                    reason: t("reason"),
                },
                held: Vec::new(),
                reviewed: false,
                repeated: false,
                flaky: false,
            }),
            TraceEvent::EmptyGraphLookup {
                at_ms: 2,
                contract: "c1".into(),
                lookup: t("lookup"),
            },
            TraceEvent::Demand {
                at_ms: 3,
                record: Box::new(crate::demand::DemandRecord {
                    run: "r1".into(),
                    status: t("status"),
                    templates_offered: vec![t("template")],
                    template_matched: None,
                    contracts: 0,
                    dispatches: 0,
                    units_started: 0,
                    verified: 0,
                    decomposition_calls: 0,
                    decomposition_spend_micros: 0,
                    spend_micros: 0,
                    unknown_spend_calls: 0,
                }),
            },
        ];
        let n = events.len();
        for event in events {
            log.record(event).await.expect("record");
        }
        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(!on_disk.contains(&secret), "{on_disk}");
        assert_eq!(on_disk.lines().count(), n);
        for label in [
            "detail", "error", "note", "claim", "quote", "reason", "lookup", "status",
        ] {
            assert!(
                on_disk.contains(&format!("{label} [redacted:github-pat;")),
                "{label}: {on_disk}"
            );
        }
        // The walk read back as the same variants, not the fallback note.
        let loaded = log.load().await.expect("load");
        assert_eq!(
            loaded
                .iter()
                .filter(|e| matches!(e, TraceEvent::Contract { .. }))
                .count(),
            4
        );
    }

    /// The per-field fallback scrubs the flat fields and never writes a
    /// nested record it cannot list.
    #[test]
    fn the_per_field_fallback_scrubs_and_never_passes_a_nested_record() {
        let secret = format!("ghp_{}", "Zq8vN2kLm4Rt6Yw1Xc3Bp5Hd7Jf9Gs0Ae2Ui");
        let out = scrub_trace_fields(TraceEvent::EmptyGraphLookup {
            at_ms: 1,
            contract: "c1".into(),
            lookup: secret.clone(),
        });
        assert!(!format!("{out:?}").contains(&secret));
        let out = scrub_trace_fields(TraceEvent::Contract {
            at_ms: 1,
            record: Box::new(crate::challenge::ContractEvent::CheckNotRun {
                interface: "i".into(),
                version: 1,
                error: secret.clone(),
                held: Vec::new(),
                parties: Vec::new(),
            }),
            counters: crate::challenge::ContractCounters::default(),
        });
        assert!(!format!("{out:?}").contains(&secret));
    }
}
