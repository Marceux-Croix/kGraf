//! The orchestrator: decomposition, contracts, delegation, the run-state
//! record (0056), re-grounding (0057) and the channel-independent message port.
//!
//! - [`Orchestrator`]: plans an instruction into contracts sized under the plan
//!   target (0057), dispatches them to workers across routes with readiness,
//!   admission, budget reservation and leases checked first, verifies and selects
//!   among attempts, re-splits handoffs, and resumes from its record.
//! - [`RunStateRecord`], [`RunStore`]: what a restart reads, in the graph store.
//! - [`MessagePort`]: notes and status questions from any channel; inbound text is
//!   data ([`UntrustedText`]). [`channel_port`] is the in-process transport the
//!   desktop shell's orchestrator box uses; [`AskDesk`] answers workers' approval
//!   asks from persisted rules or from the user in that box.
//! - [`ScheduledRuns`]: `kgraf_schedule::RunStarter` for scheduled runs.
//! - [`Orchestrator::interpret`], [`Orchestrator::start_from_plan`], [`ModelAsker`]:
//!   a run started from a corpus through `kgraf-interpret`, only
//!   from a plan the user approved.
//! - [`InterfaceContract`], [`check_mechanical`], [`compare_handshake`]: the three
//!   pre-dispatch checks (0030, 0031), run in the sandbox.
//! - [`ContractBook`], [`Challenge`], [`classify`]: challenges, escalation and
//!   mechanical revision classification (0027, 0029, 0035-0037).
//! - Live path: with contract checks on, a second planner call writes the
//!   interface contracts binding the accepted contracts ([`parse_interfaces`]),
//!   which are checked and registered before any party is dispatched; a worker challenges one through
//!   [`ChallengeTool`], bound to the run by a [`ContractLine`].
//!
//! **Reused code.** Each block carries its provenance comment; the
//! `third_party/` directories hold the `UPSTREAM` records and license texts.
//!
//! - `guards.rs`: CowAgent's six delegation guards (MIT), ported.
//! - `reports.rs`: Agent Orchestrator's report delivery policy (Apache-2.0), ported.
//! - `admission.rs`: LibreChat's budget reservation with half-TTL renewal (MIT), ported.
//! - `readiness.rs`: LibreChat's readiness before capacity (MIT), ported.
//! - `verify.rs`: big-AGI Beam's per-attempt lifecycle and throttle (MIT), ported.
//! - `orchestrator.rs`: the delegate lifecycle of goose `subagent_handler.rs` and
//!   codex-rs `codex_delegate.rs` (Apache-2.0), followed, not copied.
//!
//! Coordination (written new; each module records what was searched):
//!
//! - `gates.rs`: quality gates as work nodes on every major node (0018, 0019).
//! - `envelope.rs`: the worker-to-worker envelope, routed by observed reads
//!   (0033), bounded, appended to `docs/agent-comms/` in the run's integration
//!   worktree and indexed from there (0026, 0028).
//! - `waits.rs`: the one-line status; a lease wait, a dependency block, a gate
//!   block and a capacity hold are distinct states in the record and the trace.
//! - `partition.rs`: partition precision, recall, pair safety, low-confidence
//!   share and serialization against known write sets, measured separately.
//!
//! Written new, with the search recorded in each module: work-block sizing from the
//! ledger, the plan and grounding prompts, the loop-shape detector, the run store.
//! Durable step replay was evaluated against `duroxide` 0.1.30 (MIT,
//! microsoft/duroxide, read 2026-09-26) as the roadmap asks, and not taken:
//! - its SQLite provider is `sqlx` 0.8 with `libsqlite3-sys` bundled and
//!   `native-tls`; the workspace links SQLite through `rusqlite` and uses rustls,
//!   and two crates linking `sqlite3` at different versions do not build together;
//! - its unit of durability is an activity: a worker killed mid-run would re-run as
//!   a whole activity, repeating its paid rounds, unless the activity checkpoints
//!   itself, which the worker transcript already does;
//! - it is marked preview.
//!
//! The resume path instead reads the run record (0056) and continues each worker's
//! own transcript, so a completed model round is never issued again.

mod admission;
mod answer_json;
pub mod answer_wait;
mod ask_tool;
mod asks;
mod box_port;
mod challenge;
mod challenge_tool;
mod check_runner;
mod checks;
mod clock;
pub mod cluster_cap;
mod consumer_tests;
mod coordination;
pub mod demand;
mod derive_interface;
mod envelope;
mod gates;
mod guards;
mod handback_fit;
mod intake;
mod interactive;
mod interface_plan;
pub mod interface_review;
pub mod land_flow;
pub mod land_record;
pub mod land_suggest;
pub mod latency;
mod message_tool;
pub mod model_kinds;
pub mod occupancy;
mod orchestrator;
mod outside_error;
pub mod pairing;
mod partition;
mod placeholder;
mod plan;
mod plan_chain;
mod plan_repo;
mod plan_shapes;
mod planner_budget;
pub mod planner_context;
mod port;
mod question_route;
mod readiness;
mod reports;
mod review_desk;
mod route_health;
pub mod route_speed;
pub mod routing;
mod run_deadline;
mod run_state;
pub mod scheduler_loop;
mod script_tests;
mod session_tools;
mod shortfall;
mod sizing;
mod speculation;
mod starter;
mod store;
mod stub_land;
mod subagent_tool;
pub mod templates;
mod trace;
pub mod triage;
mod verify;
mod waits;

pub use admission::{
    Admission, AdmissionError, AdmissionGate, Admitted, BudgetBook, BudgetStats,
    DEFAULT_MAX_LIVE_AGENTS, DEFAULT_MAX_WORK_ITEMS, DEFAULT_RESERVATION_TTL_MS, ExecSlot, Limits,
    LimitsError, MachineGate, Refusal, Reservation, process_machine_gate,
};
pub use asks::{
    AskAnswer, AskDesk, AskError, AskRouter, DEFAULT_ASK_TIMEOUT, DeskNotice, DeskRules,
    PendingAsk, PendingQuestionView, QuestionAttendance, QuestionDesk, SCHEDULED_ASK_TIMEOUT,
};
pub use box_port::{ChannelPort, PORT_QUEUE, PortClient, Wake, channel_port};
pub use challenge::{
    ArtifactReader, Basis, Challenge, Challenger, Classification, CommitArtifacts, ContractBook,
    ContractChecks, ContractControl, ContractCounters, ContractEvent, ContractReply,
    DEFAULT_MAX_CHALLENGES_PER_WORKER, DEFAULT_MAX_REVIEWS, DEFAULT_MAX_SUPERSESSIONS,
    DEFAULT_RECOMPUTE_RUNS, Escalation, Evidence, InterfaceState, Lookup, MAX_ARTIFACT_BYTES,
    MIN_QUOTE_CHARS, MIN_QUOTE_WORDS, ModelReviewer, Recomputed, Resolution, ReviewRequest,
    ReviewVerdict, Reviewer, Revision, RevisionClass, classify, data_block, excerpt_in_contract,
    read_artifact, resolve_by_lookup, resolve_by_recompute, straddles,
};
pub use challenge_tool::{CHALLENGE_TOOL, ChallengeTool, ContractLine};
pub use check_runner::{
    Artifacts, CheckBase, DEFAULT_TAIL_BYTES, ResourceLimits, max_concurrent_checks,
};
pub use checks::{
    CANARY, CheckError, CheckFailure, CheckRun, CheckStage, DeliveryCheck, FileOverlay,
    HandshakeFinding, HandshakeRequest, HandshakeStatement, Handshaker, InterfaceContract,
    InterfaceKind, ModelHandshaker, PartyModel, RoutedHandshaker, RunResult, SandboxRunner,
    anchors, check_delivery, check_derived_compile, check_mechanical, compare_handshake,
    compare_handshake_ordered, delivered_files, ordered_pairs, parse_handshake,
};
pub use clock::{Clock, ManualClock, SystemClock};
pub use cluster_cap::{ClusterCapHold, DEFAULT_MAX_IN_FLIGHT_PER_CLUSTER};
pub use consumer_tests::{
    ConsumerTestBook, ConsumerTestFailure, ConsumerTestKey, ConsumerTestOutcome,
    ConsumerTestRejection, ConsumerTestRequest, ConsumerTestWriter, ModelConsumerTestWriter,
    RoutedConsumerTestWriter, gate_consumer_test, parse_consumer_test, producer_of,
    verify_producer_against_consumer_tests,
};
pub use coordination::{BaseCheck, ClosedContract, Closure, Coordination, Integration};
pub use envelope::{
    Addressee, COMMS_DIR, COMMS_FILE_MAX_BYTES, COMMS_LOCK, CopyState, Draft, Envelope,
    EnvelopeError, FeedEntry, MAX_CONCERNS, MAX_LABEL_CHARS, Mailbox, QUEUED_ROW_PREFIX,
    QueuedCopy, append_to_comms, heading_name, label, observed_reads, pair_file, pair_file_for,
    queued_row_id,
};
pub use gates::{
    BASE_GATE_PROBE, DEFAULT_MAX_REMEDIATIONS, GateDecision, GateDecisionError, GateDecisionRecord,
    GateNode, GateRequest, GateRunner, GateStatus, GateVerdict, ROOT_GATE,
};
pub use guards::{
    DEFAULT_MAX_DEPTH, DEFAULT_MAX_TASK_CHARS, Delegation, DelegationPolicy, DelegationRequest,
    GuardRefusal, MAX_POLICY_DEPTH, PolicyError,
};
pub use intake::{Intake, IntakeParts, ModelAsker, interpret_corpus};
pub use interactive::{HostError, InteractiveRun, RunEvent};
pub use interface_plan::{
    InterfaceDraft, InterfaceReply, InterfaceRequest, PreviousAsk, parse_interfaces,
};
pub use interface_review::{
    ConsumerRoute, Finding, InterfaceReviewer, ReviewContext, ReviewOutcome, ReviewRecord,
    RoutedInterfaceReviewer,
};
pub use model_kinds::ModelKindCalls;
pub use orchestrator::sessions::{
    AnswerOutput, AnswerRequest, ConnectionRequest, ConnectionVerdict, JudgeOutput, JudgeRequest,
    QuestionAnswerer, SessionNoticeSink,
};
pub use orchestrator::{
    AttemptEnv, AttemptEnvs, AttemptProcesses, CloseEnv, CompactError,
    DEFAULT_MAX_CONTRACTS_PER_PLAN, DEFAULT_MAX_SPLITS, DirWorkspaces, Orchestrator,
    OrchestratorConfig, OrchestratorError, OrchestratorHandle, OrderError, PrepareFailure, Route,
    RunReport, Services, SessionError, StatusReport, SteerError, VERIFIER_REFUSAL_TAG, Workspaces,
    plan_review_refusal, run_lease_holder_is_live,
};
pub use orchestrator::{
    LAND_LEASE_TTL_MS, LandLease, take_run_lease_for_land, take_run_lease_for_land_with_ttl,
};
pub use partition::{
    CONFIDENT, CauseScore, GraphPredictor, MissCause, PairScore, PartitionReport,
    SerializationScore, TaskTruth, WriteSetScore, measure, random_predictions, score_causes,
    score_pairs, score_serialization, score_write_sets,
};
pub use plan::{
    ContractSpec, CostBasis, Decomposer, GroundOutput, GroundRequest, Grounder,
    INCOMPLETE_ANSWER_NOTE, ModelAnswerer, ModelCaller, ModelDecomposer, ModelGrounder, PlanError,
    PlanOutput, PlanRequest, answer_prompt, judge_prompt, normalize_plan, parse_answer,
    parse_grounding, parse_plan, parse_verdicts, sanitize_id, sanitize_interfaces, sanitize_plan,
    valid_write_path, validate_plan, validate_plan_with,
};
pub use port::{InboundKind, InboundMessage, MessagePort, OutboundReply, PortError, UntrustedText};
pub use question_route::{
    DecisionMethod, ModelQuestionClassifier, ModelRouteAnswerer, OfferResolution,
    QUESTIONS_NODE_ID, QUESTIONS_NODE_TITLE, QUESTIONS_PLAN_ID, QuestionClassifier,
    QuestionDispatcher, RoadmapCandidate, RouteAnswerer, RouteClassification, RouteError,
    RouteOutcome, act_on_classification, cheap_match, ensure_questions_node, file_under_questions,
    record_answered, record_offer_made, record_offer_resolved, route_question,
};
pub use readiness::{AlwaysReady, FnProbe, ReadinessFailure, ReadinessProbe, check_tools};
pub use reports::{Delivery, Recipient, ReportError, ReportKind, ReportQueue, WorkerReport};
pub use review_desk::{
    PermissionRequestView, PlanAnswer, PlanItemView, PlanView, ReviewDesk, ReviewError,
};
pub use routing::{
    CLUSTER_COCHANGE_MIN_COUNT, EXPLORATION_EVERY, FigureSource, INTERFACE_FLOOR,
    LIVE_MIN_ATTEMPTS, RouteInfo, RoutePick, RoutingPolicy,
};
pub use run_state::{
    AdmittedLease, CheckHold, ContractCheckSettings, ContractStatus, GroundingRecord,
    GroundingTrigger, OpenContract, PendingPermission, PlanApproval, PlanItem, PlanProgress,
    PlannedDispatch, RUN_STATE_SCHEMA_VERSION, RecordedNote, RunCounters, RunStateError,
    RunStateRecord, RunStatus, VerifiedCommit, WriteTarget,
};
pub use scheduler_loop::{
    DEFAULT_LEASE, DEFAULT_ON_TIME_WINDOW, SchedulerLoop, TickEvent, TrackedSchedule,
};
pub use session_tools::{SESSION_TOOL_NAMES, SessionLine, session_tools};
pub use shortfall::{
    AcceptedStanding, Authority, Grant, GrantRecord, Grounding, MAX_ANSWERED_SHORTFALLS,
    MAX_SHORTFALL_REDISPATCHES, RequestScope, SCOPED_CHECK_VACUOUS, SHORTFALL_TAG, SHORTFALL_TOOL,
    ShortfallDecision, ShortfallKind, ShortfallTool, accepted_standing, vacuous_check,
};
pub use sizing::{
    ORCHESTRATOR_AGENT_PREFIX, Projection, ProjectionSource, Sizer, dear_usage, dear_usage_on,
};
pub use speculation::{
    ConflictKind, Round, SPECULATION_GATE, SpeculationCost, SpeculativeConflict, Trial,
    ValidationCache, speculate,
};
pub use starter::{Finished, OrchestratorFactory, ScheduledRuns};
pub use store::{RunFiles, RunStore, SharedGraph, StoreError, StoredRun, read_runs};
pub use stub_land::{
    GitStubLander, StubLander, StubScope, is_placeholder_source, placeholder_left_in_place_reason,
    placeholder_writes_left_in_place, stub_left_in_place, stub_left_in_place_failure,
};
pub use subagent_tool::{
    ChildMerge, MAX_REPORT_ANSWER_CHARS, SPAWN_SUBAGENT_TOOL, SUBAGENT_REPORT_MARKER,
    SUBAGENT_REPORT_PREFIX, SpawnRefusal, SpawnRequest, Spawned, SubagentReport, spawned_text,
};
pub use trace::{
    DeferReason, LoopDetector, LoopKind, PlannedContract, PlannedInterface, RateLimitWaitsSource,
    RouteWaits, TraceEvent, TraceLog,
};
pub use verify::{Attempt, AttemptStatus, Verdict, Verifier, select, status_of};
pub use waits::{WaitKind, WaitState, one_line};
