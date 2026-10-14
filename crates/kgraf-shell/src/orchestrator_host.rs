//! The orchestrator box's real backend: runs hosted in this process through an
//! [`OrchestratorFactory`], or, with no factory, the stored run records alone.
//!
//! Hosting (decision 0052's one-process model): the factory builds every run on the
//! process's one [`Admission`], so the settings surface's concurrency cap
//! ([`HostParts::admission`]) binds every run the window starts. Workers' approval
//! asks reach the box through [`AskDesk`], which the factory must set as each worker
//! environment's ask resolver.
//!
//! The shipped window builds its parts in `lib.rs::run` for a `--project`: one
//! `scheduler_bg::ProjectFactory` over the project's `kgraf_host::Host`
//! (opened from `.kgraf/run.json` on first need, reopened when it changes),
//! shared with the tray's scheduled runs, with the box's desk as every run's ask
//! resolver. With no project there is nothing to host, and the box says so.
//!
//! No tokio runtime exists unless a factory does, so an idle window starts no
//! threads for this.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use kgraf_core::RunId;
use kgraf_orchestrator::{
    Admission, AskAnswer, AskDesk, DeskNotice, HostError, InboundKind, InteractiveRun,
    ModelQuestionClassifier, ModelRouteAnswerer, OfferResolution, OrchestratorFactory, PendingAsk,
    RoadmapCandidate, RouteAnswerer, RouteClassification, RunEvent, RunStatus, UntrustedText, Wake,
    ensure_questions_node, record_answered, record_offer_made, record_offer_resolved,
    route_question as classify_question,
};

use crate::orchestrator_box::{BackendEvent, BackendState, BoxBackend, QuestionOffer};

/// What the box needs from the run factory's host beyond building runs: its
/// shared `Admission` (the caps setting binds it), the housekeeping after a run
/// ends, and the notices of builds. `scheduler_bg::ProjectFactory` implements it
/// over the project's `kgraf_host::Host`, shared with the scheduled runs.
pub trait RunHousekeeping: Send + Sync {
    /// The admission every run of the host shares; `None` until the host is open.
    fn admission(&self) -> Option<Arc<Admission>>;
    /// After a run ends: its housekeeping (attempt-ref sweep, unreconciled
    /// count), as lines for the box.
    fn finish_run(&self, run: &RunId) -> Vec<String>;
    /// Notices of builds made since the last call.
    fn take_notices(&self) -> Vec<String>;
    /// A box run is starting: until its `finish_run`, the host must not be
    /// replaced (a reload would give later runs a second `Admission`).
    fn run_started(&self, _run: &RunId) {}
    /// The user's worktree setting changed (decision 0088): runs built from now
    /// on apply it, and so does every host this project opens later.
    fn set_worktree_retention(&self, _retention: kgraf_host::WorktreeRetention) {}
    /// Tokens the ledger attributes to `run` and its workers, by adapter (the
    /// composer's attribution line). `None` when there is no ledger to
    /// read.
    fn run_tokens(&self, _run: &RunId) -> Option<RunTokens> {
        None
    }
    /// Every configured route's capacity input (decision 0067), if a `Host` is
    /// already open (`Vec::new()` otherwise: a background reader must not
    /// have the side effect of opening one). Default empty so `FixedAdmission`
    /// and any other minimal implementer need not provide one.
    fn route_capacity_inputs(&self) -> Vec<kgraf_host::RouteCapacityInput> {
        Vec::new()
    }

    /// The project's `Host`, for callers that need more than housekeeping:
    /// Question routing reads roadmap candidates
    /// from the graph (`Host::with_graph`) and needs a planner-route caller
    /// for its two ledgered calls (`Host::planner_caller`). `None` for
    /// `FixedAdmission` (tests) or before a `Host` is open, exactly like
    /// `route_capacity_inputs` above.
    fn host(&self) -> Option<Arc<kgraf_host::Host>> {
        None
    }

    /// The user's live permission control and its file (decision 0094),
    /// shared by every run this host builds. `None` for `FixedAdmission`.
    fn permissions(&self) -> Option<crate::permission_bar::PermissionUi> {
        None
    }
}

/// Tokens attributed to one run, from the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunTokens {
    pub total: u64,
    /// (adapter, tokens), largest first.
    pub by_adapter: Vec<(String, u64)>,
}

impl RunTokens {
    /// Sums calls' usage by adapter. Pure.
    pub fn from_calls<'a>(
        calls: impl IntoIterator<Item = (&'a str, &'a kgraf_core::TokenUsage)>,
    ) -> Self {
        let mut by: std::collections::BTreeMap<String, u64> = Default::default();
        for (adapter, u) in calls {
            let n = u.uncached_input
                + u.cache_read
                + u.cache_write
                + u.text_output
                + u.reasoning_output;
            *by.entry(adapter.to_owned()).or_default() += n;
        }
        let mut by_adapter: Vec<(String, u64)> = by.into_iter().collect();
        by_adapter.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Self {
            total: by_adapter.iter().map(|(_, n)| n).sum(),
            by_adapter,
        }
    }

    /// "Run r: 412,880 tokens attributed (kimi 61%, openai 39%)". It reports
    /// spend and never a saving.
    pub fn line(&self, run: &str) -> String {
        let shares: Vec<String> = self
            .by_adapter
            .iter()
            .map(|(a, n)| {
                let pct = (n * 100 + self.total / 2)
                    .checked_div(self.total)
                    .unwrap_or(0);
                format!("{a} {pct}%")
            })
            .collect();
        let mut line = format!("Run {run}: {} tokens attributed", group(self.total));
        if !shares.is_empty() {
            line.push_str(&format!(" ({})", shares.join(", ")));
        }
        line
    }
}

/// `412880` as `412,880`.
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Housekeeping for a factory with a fixed admission and nothing to sweep (tests,
/// or a host whose owner does its own housekeeping).
pub struct FixedAdmission(pub Arc<Admission>);

impl RunHousekeeping for FixedAdmission {
    fn admission(&self) -> Option<Arc<Admission>> {
        Some(self.0.clone())
    }
    fn finish_run(&self, _run: &RunId) -> Vec<String> {
        Vec::new()
    }
    fn take_notices(&self) -> Vec<String> {
        Vec::new()
    }
}

impl RunHousekeeping for crate::scheduler_bg::ProjectFactory {
    fn admission(&self) -> Option<Arc<Admission>> {
        crate::scheduler_bg::ProjectFactory::admission(self)
    }
    fn finish_run(&self, run: &RunId) -> Vec<String> {
        self.finish_box_run(run)
    }
    fn take_notices(&self) -> Vec<String> {
        crate::scheduler_bg::ProjectFactory::take_notices(self)
    }
    fn run_started(&self, run: &RunId) {
        self.box_run_started(run);
    }
    fn set_worktree_retention(&self, retention: kgraf_host::WorktreeRetention) {
        crate::scheduler_bg::ProjectFactory::set_worktree_retention(self, retention);
    }
    fn run_tokens(&self, run: &RunId) -> Option<RunTokens> {
        crate::scheduler_bg::ProjectFactory::run_tokens(self, run)
    }
    fn route_capacity_inputs(&self) -> Vec<kgraf_host::RouteCapacityInput> {
        crate::scheduler_bg::ProjectFactory::host(self)
            .map(|h| h.route_capacity_inputs())
            .unwrap_or_default()
    }
    fn host(&self) -> Option<Arc<kgraf_host::Host>> {
        crate::scheduler_bg::ProjectFactory::host(self)
    }
    fn permissions(&self) -> Option<crate::permission_bar::PermissionUi> {
        crate::scheduler_bg::ProjectFactory::permissions(self)
    }
}

/// What hosting needs. Built by whoever builds the factory, so the factory, the
/// admission and the desk belong to one host.
#[derive(Clone)]
pub struct HostParts {
    pub factory: Arc<dyn OrchestratorFactory>,
    /// The host's admission and housekeeping.
    pub housekeeping: Arc<dyn RunHousekeeping>,
    /// The desk where the box shows and answers asks.
    pub desk: Arc<AskDesk>,
    /// The resolver the host gives every worker, when it routes by run origin:
    /// the box marks the runs it starts, and says when a person is at the window.
    pub router: Option<Arc<kgraf_orchestrator::AskRouter>>,
}

/// Shown when no factory was given.
pub const NO_HOST: &str = "no project is open, so this window hosts no runs: start \
kgraf-app with --project <dir> (runs read .kgraf/run.json there).";

enum Slot {
    Idle,
    /// The start in flight, and the run it starts.
    Starting(Receiver<Result<InteractiveRun, HostError>>, RunId),
    Running(InteractiveRun),
}

/// The box's backend.
pub struct OrchestratorHost {
    parts: Option<HostParts>,
    runtime: Option<tokio::runtime::Runtime>,
    slot: Slot,
    wake: Wake,
    /// The newest stored run's status line, from the run watch.
    record_status: Option<String>,
    events: Vec<BackendEvent>,
    /// The orchestrator refused the last order; the interaction restores the one
    /// before it.
    order_refused: bool,
    /// Why the last run hosted here failed, until the next one starts (the
    /// project's failure card and its yellow dot).
    last_failure: Option<HostFailure>,
    /// The attempt the last `begin` started: its run and start time. A resume
    /// starts the same run id again, so the start time tells attempts apart.
    current: (String, i64),
    /// Question routing, in flight: the classification (and, for
    /// "answer directly", the answer too) runs on the host's own runtime; its
    /// result arrives here for the next `poll` to act on. At most one at a
    /// time (`route_question` refuses a second while this is `Some`).
    routing: Option<Receiver<RoutingResult>>,
    /// Offers waiting on the user (step 3), each a question the routing found
    /// large and off topic.
    offers: Vec<PendingOffer>,
    next_offer_id: u64,
    /// How to start the run in `Slot::Starting` again, should its start stop
    /// on the dependency question (decision 0087).
    starting_restart: Option<crate::dependency_prompt::Restart>,
    /// The dependency questions answered "Allow once" for the start in
    /// `Slot::Starting` (`FetchPrompt::once`); empty for a start the person
    /// began.
    starting_once: Vec<kgraf_host::deps::ConsentSubject>,
    /// The dependency question the box shows, until answered.
    fetch_prompt: Option<crate::dependency_prompt::FetchPrompt>,
    /// The stored runs `/edit` may target, newest first, from the run watch
    /// (decision 0100).
    edit_records: Vec<EditRecord>,
    /// Landing with no live loop and the land-suggestion facts, off the UI
    /// thread (decision 0100).
    land: crate::land_card::LandBackground,
}

/// One stored run as `/edit` sees it: its instruction as it stands, and why
/// it cannot be reopened now, if it cannot (another process holds it, a land
/// is pending).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditRecord {
    pub run: String,
    pub instruction: String,
    pub refusal: Option<String>,
}

/// An offer waiting on the user (step 3): its question (already scrubbed,
/// decision 0071) and, when `record_offer_made` succeeded, the graph
/// item's own id, which its resolution (`accept`/`decline`) updates in place.
struct PendingOffer {
    id: u64,
    question: String,
    graph_id: Option<String>,
}

/// What question routing decided, once acted on (steps 1 and 2) or
/// ready for the user (step 3). Sent back from the spawned routing task.
/// `question` (and, for step 2, `answer`) are already the scrubbed copies
/// (decision 0071), what is stored and shown
/// from here on. `method` is which of the cheap match and the classifier
/// decided (fix: recorded on the graph and shown to the user).
enum RoutingResult {
    /// Attach to a roadmap feature node and dispatch there (step 1).
    Attach {
        node_id: String,
        question: String,
        method: kgraf_orchestrator::DecisionMethod,
    },
    /// The orchestrator answered it itself (step 2).
    Answer {
        question: String,
        answer: String,
        method: kgraf_orchestrator::DecisionMethod,
    },
    /// Very large and off topic: offer a new project (step 3).
    Offer {
        question: String,
        method: kgraf_orchestrator::DecisionMethod,
    },
    Error(String),
}

/// A failed attempt of a run hosted in the box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFailure {
    pub run: String,
    /// When the attempt started, in ms since the epoch; unique per host.
    pub started_ms: i64,
    pub words: String,
}

impl HostFailure {
    /// The failure card's key: one per attempt (`run:<id>@<started_ms>`), so a
    /// dismissed failure never hides the failure of a resumed run (final check
    /// item 1).
    pub fn key(&self) -> String {
        format!("run:{}@{}", self.run, self.started_ms)
    }
}

impl std::fmt::Debug for OrchestratorHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrchestratorHost")
            .field("hosting", &self.parts.is_some())
            .finish_non_exhaustive()
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl OrchestratorHost {
    /// A host with no factory: it starts nothing and says why.
    pub fn idle(wake: Wake) -> Self {
        Self {
            parts: None,
            runtime: None,
            slot: Slot::Idle,
            wake,
            record_status: None,
            events: Vec::new(),
            order_refused: false,
            last_failure: None,
            current: (String::new(), 0),
            routing: None,
            offers: Vec::new(),
            next_offer_id: 0,
            starting_restart: None,
            starting_once: Vec::new(),
            fetch_prompt: None,
            edit_records: Vec::new(),
            land: crate::land_card::LandBackground::default(),
        }
    }

    /// A host. With `parts`, a two-thread tokio runtime is started for runs.
    pub fn new(parts: Option<HostParts>, wake: Wake) -> std::io::Result<Self> {
        let Some(parts) = parts else {
            return Ok(Self::idle(wake));
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("orchestrator-host")
            .enable_all()
            .build()?;
        Ok(Self {
            parts: Some(parts),
            runtime: Some(runtime),
            slot: Slot::Idle,
            wake,
            record_status: None,
            events: Vec::new(),
            order_refused: false,
            last_failure: None,
            current: (String::new(), 0),
            routing: None,
            offers: Vec::new(),
            next_offer_id: 0,
            starting_restart: None,
            starting_once: Vec::new(),
            fetch_prompt: None,
            edit_records: Vec::new(),
            land: crate::land_card::LandBackground::default(),
        })
    }

    /// The admission the cap setting binds, once the host is open.
    pub fn admission(&self) -> Option<Arc<Admission>> {
        self.parts.as_ref().and_then(|p| p.housekeeping.admission())
    }

    /// The housekeeping handle (decision 0067): the shell's capacity re-measure thread reads each
    /// open project's configured routes through this, without needing to know which concrete
    /// factory type the project uses.
    pub fn housekeeping(&self) -> Option<Arc<dyn RunHousekeeping>> {
        self.parts.as_ref().map(|p| p.housekeeping.clone())
    }

    /// This project's `Host`, when one is open: question routing
    /// reads roadmap candidates and records its choices through it, and a
    /// resolved offer's graph record (accepted, naming the new project's
    /// name, or declined) is written here too, from `layout.rs`'s
    /// `take_new_project_offers`, which does not otherwise reach a `Host`.
    pub fn project_host(&self) -> Option<Arc<kgraf_host::Host>> {
        self.parts.as_ref().and_then(|p| p.housekeeping.host())
    }

    /// Tokens attributed to `run`, from the host's ledger.
    pub fn run_tokens(&self, run: &RunId) -> Option<RunTokens> {
        self.parts.as_ref()?.housekeeping.run_tokens(run)
    }

    /// Why the last run hosted here failed, until the next one starts.
    pub fn last_failure(&self) -> Option<&HostFailure> {
        self.last_failure.as_ref()
    }

    fn fail(&mut self, words: String) {
        self.last_failure = Some(HostFailure {
            run: self.current.0.clone(),
            started_ms: self.current.1,
            words,
        });
    }

    /// Whether this window hosts runs (a factory was given).
    pub fn is_hosting(&self) -> bool {
        self.parts.is_some()
    }

    /// The run hosted here, if one runs.
    pub fn running(&self) -> Option<&InteractiveRun> {
        match &self.slot {
            Slot::Running(r) => Some(r),
            _ => None,
        }
    }

    /// Whether the orchestrator refused the last order since the last call.
    pub fn take_order_refused(&mut self) -> bool {
        std::mem::take(&mut self.order_refused)
    }

    /// The run watch's newest status line.
    pub fn set_record_status(&mut self, line: Option<String>) {
        self.record_status = line;
    }

    /// The stored runs `/edit` may target, newest first (decision 0100).
    pub fn set_edit_records(&mut self, records: Vec<EditRecord>) {
        self.edit_records = records;
    }

    /// Reopen `run` with an edited instruction (decision 0100).
    fn reopen_with(
        &mut self,
        run: RunId,
        change: kgraf_orchestrator::ChangeInput,
    ) -> Result<(), String> {
        self.mark_interactive(&run);
        let id = run.clone();
        self.begin(id, move |rt, factory, wake| {
            Box::pin(InteractiveRun::reopen(rt, factory, run, change, wake))
        })?;
        self.fetch_prompt = None;
        self.starting_restart = None;
        self.starting_once = Vec::new();
        Ok(())
    }

    /// Start a new run from a copy of `source`'s plan (decision 0100).
    fn copy_from(&mut self, source: RunId, instruction: String) -> Result<(), String> {
        let run = RunId::new(format!("box-{}", now_ms()));
        self.mark_interactive(&run);
        let id = run.clone();
        self.begin(id, move |rt, factory, wake| {
            Box::pin(InteractiveRun::copy(
                rt,
                factory,
                run,
                source,
                instruction,
                wake,
            ))
        })?;
        self.fetch_prompt = None;
        self.starting_restart = None;
        self.starting_once = Vec::new();
        Ok(())
    }

    /// Resume `run` from its record (the play control, 0014).
    pub fn resume(&mut self, run: RunId) -> Result<(), String> {
        self.mark_interactive(&run);
        let id = run.clone();
        let restart = crate::dependency_prompt::Restart::Resume(run.clone());
        self.begin(id, |rt, factory, wake| {
            Box::pin(InteractiveRun::resume(rt, factory, run, wake))
        })?;
        // A question left open for another start is stale now.
        self.fetch_prompt = None;
        self.starting_restart = Some(restart);
        self.starting_once = Vec::new();
        Ok(())
    }

    /// Start `run` with `instruction` (a fresh box run).
    fn start_as(&mut self, run: RunId, instruction: &str) -> Result<(), String> {
        self.mark_interactive(&run);
        let restart = crate::dependency_prompt::Restart::Start(instruction.to_owned());
        let instruction = instruction.to_owned();
        let id = run.clone();
        self.begin(id, move |rt, factory, wake| {
            Box::pin(InteractiveRun::start(rt, factory, run, instruction, wake))
        })?;
        // A question left open for another start is stale now.
        self.fetch_prompt = None;
        self.starting_restart = Some(restart);
        self.starting_once = Vec::new();
        Ok(())
    }

    /// Decision 0087: the person's answer to the dependency question, then
    /// the held start again (a new run id for a start, the same for a resume).
    ///
    /// The question is taken only once the answer has been spent: every step
    /// that can fail (the host, a window that cannot start a run, storing the
    /// answer) runs first and leaves the question open, and a start that fails
    /// after the answer was applied puts the question back, so the person can
    /// answer again.
    fn answer_fetch_prompt(&mut self, answer: AskAnswer) -> Result<(), String> {
        use crate::dependency_prompt::{FetchChoice, Restart, apply, choice_of, not_now_line};
        let prompt = self
            .fetch_prompt
            .as_ref()
            .ok_or_else(|| "no dependency question is open".to_owned())?;
        let host = self
            .project_host()
            .ok_or_else(|| "the project's host is not open".to_owned())?;
        let choice = choice_of(&answer);
        if choice != FetchChoice::NotNow {
            // Before the answer is applied: a grant must not be spent on a
            // run that cannot start.
            self.can_begin()?;
        }
        let run = match &prompt.restart {
            Restart::Start(_) => RunId::new(format!("box-{}", now_ms())),
            Restart::Resume(run) => run.clone(),
        };
        let go = apply(&host, &run, &prompt.question, choice)?;
        let prompt = self
            .fetch_prompt
            .take()
            .ok_or_else(|| "no dependency question is open".to_owned())?;
        if !go {
            self.events
                .push(BackendEvent::Notice(not_now_line(&prompt.question)));
            return Ok(());
        }
        // The earlier "Allow once" answers of this start, given again to the
        // run id it starts under now, and this one with them.
        let mut once = prompt.once.clone();
        for subject in &once {
            host.grant_dependency_once(&run, subject);
        }
        if choice == FetchChoice::AllowOnce {
            once.push(prompt.question.subject.clone());
        }
        let started = match &prompt.restart {
            Restart::Start(instruction) => self.start_as(run, instruction),
            Restart::Resume(run) => self.resume(run.clone()),
        };
        match started {
            Ok(()) => self.starting_once = once,
            Err(_) => self.fetch_prompt = Some(prompt),
        }
        started
    }

    /// Whether this window can begin a run now.
    fn can_begin(&self) -> Result<(), String> {
        if !matches!(self.slot, Slot::Idle) {
            return Err("a run is already in this window".to_owned());
        }
        if self.parts.is_none() || self.runtime.is_none() {
            return Err(NO_HOST.to_owned());
        }
        Ok(())
    }

    /// A run the box starts or resumes: its asks go to the desk.
    fn mark_interactive(&self, run: &RunId) {
        if let Some(router) = self.parts.as_ref().and_then(|p| p.router.as_ref()) {
            router.mark_interactive(run.as_str());
        }
    }

    /// Until when a person is at the window (visible, focused, recent input):
    /// only then does a scheduled run's ask reach the box.
    pub fn set_attended_until(&self, until: Option<std::time::Instant>) {
        if let Some(router) = self.parts.as_ref().and_then(|p| p.router.as_ref()) {
            router.set_attended_until(until);
        }
    }

    /// The user's order for the running run (0014).
    pub fn set_order(&mut self, contracts: Vec<String>) -> Result<(), String> {
        match &self.slot {
            Slot::Running(r) => {
                r.set_order(contracts);
                Ok(())
            }
            _ => Err("no run in this window to order".to_owned()),
        }
    }

    fn begin(
        &mut self,
        run: RunId,
        make: impl FnOnce(
            tokio::runtime::Handle,
            Arc<dyn OrchestratorFactory>,
            Wake,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<InteractiveRun, HostError>> + Send>,
        >,
    ) -> Result<(), String> {
        self.can_begin()?;
        let (Some(parts), Some(runtime)) = (&self.parts, &self.runtime) else {
            return Err(NO_HOST.to_owned());
        };
        let (tx, rx) = channel();
        let wake = self.wake.clone();
        // Counted before the build, so a reload cannot slip in between.
        parts.housekeeping.run_started(&run);
        let future = make(
            runtime.handle().clone(),
            parts.factory.clone(),
            wake.clone(),
        );
        runtime.spawn(async move {
            let _ = tx.send(future.await);
            wake();
        });
        self.last_failure = None;
        // Strictly increasing, so two starts in one millisecond are two attempts.
        let started = now_ms().max(self.current.1 + 1);
        self.current = (run.to_string(), started);
        self.slot = Slot::Starting(rx, run);
        Ok(())
    }

    fn map(&mut self, event: RunEvent) -> Option<BackendEvent> {
        Some(match event {
            RunEvent::Reply(r) => BackendEvent::Reply {
                in_reply_to: r.in_reply_to,
                text: r.text,
            },
            RunEvent::Compacted { target, result } => {
                let what = target.unwrap_or_else(|| "the orchestrator".to_owned());
                match result {
                    Ok(()) => BackendEvent::Notice(format!("/compact: {what} compacted")),
                    Err(e) => BackendEvent::Error(format!("/compact {what}: {e}")),
                }
            }
            RunEvent::Ordered(Ok(())) => BackendEvent::Notice("your order is set".to_owned()),
            RunEvent::Ordered(Err(e)) => {
                self.order_refused = true;
                BackendEvent::Error(format!("order refused: {e}"))
            }
            RunEvent::Finished(Ok(report)) => {
                let state = match report.status {
                    RunStatus::Running => "running",
                    RunStatus::Completed => "completed",
                    RunStatus::Blocked { .. } => "blocked (the graph shows by what)",
                    RunStatus::Stopped => "stopped (resumable with play)",
                };
                BackendEvent::Notice(format!(
                    "run {} ended {state}: {} verified",
                    report.run,
                    report.verified.len()
                ))
            }
            RunEvent::Finished(Err(e)) => {
                self.fail(format!("the run failed: {e}"));
                BackendEvent::Error(format!("the run failed: {e}"))
            }
            // Decision 0067: real per-session lines in the box.
            // `SessionNotice` below already carries undelivered-message and
            // paced-delivery text from the orchestrator loop verbatim, so it
            // needs no special case here; pinned question cards come from
            // `pending_questions`, not from an event.
            RunEvent::SessionSent { session, result } => match result {
                Ok(_) => BackendEvent::Notice(format!("sent to {session}")),
                Err(e) => BackendEvent::Error(format!("not sent to {session}: {e}")),
            },
            RunEvent::SessionStopped { session, result } => match result {
                Ok(()) => BackendEvent::Notice(format!("{session} stopped")),
                Err(e) => BackendEvent::Error(format!("{session} not stopped: {e}")),
            },
            RunEvent::SessionResumed { session, result } => match result {
                Ok(()) => BackendEvent::Notice(format!("{session} resumed")),
                Err(e) => BackendEvent::Error(format!("{session} not resumed: {e}")),
            },
            RunEvent::QuestionAnswered { question, result } => match result {
                Ok(()) => BackendEvent::Notice(format!("question {question} answered")),
                Err(e) => BackendEvent::Error(format!("question {question} not answered: {e}")),
            },
            RunEvent::SessionNotice { text } => BackendEvent::Notice(text),
            RunEvent::Landed(outcome) => BackendEvent::Land(outcome),
        })
    }

    /// The run a land from this window applies to, if there is one.
    pub fn land_run(&self) -> Option<RunId> {
        self.land_target().ok().map(|(_, run)| run)
    }

    /// The project and run a land with no live loop applies to: this
    /// window's run, else the last run it hosted.
    fn land_target(&self) -> Result<(std::path::PathBuf, RunId), String> {
        let host = self
            .parts
            .as_ref()
            .and_then(|p| p.housekeeping.host())
            .ok_or_else(|| NO_HOST.to_owned())?;
        let run = match &self.slot {
            Slot::Running(r) => r.run().clone(),
            _ if !self.current.0.is_empty() => RunId::new(&self.current.0),
            _ => {
                return Err(
                    "no run in this window to land; `kgraf land --run <run>` lands a stored one"
                        .to_owned(),
                );
            }
        };
        Ok((host.root().to_path_buf(), run))
    }
}

/// Question routing, off the UI thread: reads roadmap candidates
/// from the project's graph, then classifies (the cheap match only
/// pre-selects; the classifier is always called and its own answer decides)
/// and, for "answer directly", makes the second
/// ledgered call for the answer itself. Dispatch (steps 1 and 4) stays on the
/// UI thread (`OrchestratorHost::start`, the same path `/run` uses), so this
/// never spawns a run on its own.
///
/// Scrubbing: `question_text`
/// arrives raw from the box. It is scrubbed once, at the door, into
/// `scrubbed`, which is what every `RoutingResult` variant carries onward -
/// stored and shown from here on. The model calls read `model_copy` of it
/// instead: the recovered real text, kept for this process only, the same
/// pattern `UntrustedText::for_storage`/`for_model` already use. The model's
/// own answer (step 2) is scrubbed the same way before it is carried onward.
async fn route_one(host: Arc<kgraf_host::Host>, question_text: String) -> RoutingResult {
    let scrubbed = kgraf_secrets::intake(&question_text);
    let for_model = UntrustedText::new(kgraf_secrets::model_copy(&scrubbed).into_owned());
    let candidates = match host.with_graph(kgraf_graph::GraphStore::presumed_nodes) {
        Ok(Ok(rows)) => rows
            .into_iter()
            .filter(|p| p.gone_ms.is_none())
            .map(|p| RoadmapCandidate {
                node_id: p.node.id,
                title: p.node.title,
            })
            .collect::<Vec<_>>(),
        Ok(Err(e)) => return RoutingResult::Error(e.to_string()),
        Err(e) => return RoutingResult::Error(e.to_string()),
    };
    let caller = match host.planner_caller("orchestrator:question-route", "question-route") {
        Ok(c) => c,
        Err(e) => return RoutingResult::Error(e.to_string()),
    };
    let classifier = ModelQuestionClassifier(caller);
    let (classification, method) =
        match classify_question(&for_model, &candidates, &classifier).await {
            Ok(v) => v,
            Err(e) => return RoutingResult::Error(e.to_string()),
        };
    match classification {
        RouteClassification::Matched { node_id } => RoutingResult::Attach {
            node_id,
            question: scrubbed,
            method,
        },
        RouteClassification::AnswerDirectly => {
            let caller = match host.planner_caller("orchestrator:question-route", "question-route")
            {
                Ok(c) => c,
                Err(e) => return RoutingResult::Error(e.to_string()),
            };
            match ModelRouteAnswerer(caller).answer(&for_model).await {
                Ok(answer) => RoutingResult::Answer {
                    question: scrubbed,
                    answer: kgraf_secrets::intake(&answer),
                    method,
                },
                Err(e) => RoutingResult::Error(e.to_string()),
            }
        }
        RouteClassification::OffTopicLarge => RoutingResult::Offer {
            question: scrubbed,
            method,
        },
    }
}

/// An instruction that names the roadmap or "Questions" node a routed
/// question was attached to (steps 1 and 4): quoted as data, like every other
/// inbound text this project holds (`UntrustedText::quote_as_data`), so the
/// node id and title cannot be read as instructions by whatever runs next.
fn node_tagged_instruction(node_id: &str, question: &str) -> String {
    format!(
        "{}\n{}",
        UntrustedText::new(node_id.to_owned()).quote_as_data("roadmap_node", &[]),
        question
    )
}

fn notice_text(n: DeskNotice) -> BackendEvent {
    match n {
        DeskNotice::Covered { tool, worker, note } => BackendEvent::Notice(format!(
            "{tool} for {worker} ran under a saved rule ({note})"
        )),
        DeskNotice::RuleRecorded { tool, subject } => {
            BackendEvent::Notice(format!("rule saved: {tool} {subject} runs without asking"))
        }
        DeskNotice::RuleNotRecorded { tool, reason } => {
            BackendEvent::Error(format!("no rule saved for {tool}: {reason}"))
        }
        DeskNotice::DeniedOnClose { tool, worker } => {
            BackendEvent::Error(format!("{tool} for {worker} denied: the box closed"))
        }
        DeskNotice::TimedOut {
            tool,
            worker,
            after,
        } => BackendEvent::Error(format!(
            "{tool} for {worker} denied: no answer within {} s",
            after.as_secs()
        )),
        DeskNotice::RulesUnreadable { reason } => BackendEvent::Error(reason),
    }
}

impl BoxBackend for OrchestratorHost {
    fn state(&self) -> BackendState {
        match &self.slot {
            Slot::Idle => BackendState::Idle {
                can_start: self.parts.is_some(),
                reason: if self.parts.is_some() {
                    String::new()
                } else {
                    NO_HOST.to_owned()
                },
            },
            Slot::Starting(..) => BackendState::Starting,
            Slot::Running(r) => BackendState::Running {
                run: r.run().to_string(),
            },
        }
    }

    fn send(&mut self, kind: InboundKind, text: &str) -> Result<String, String> {
        match &mut self.slot {
            Slot::Running(r) => r.send(kind, text, now_ms()).map_err(|e| e.to_string()),
            _ => Err("no run in this window".to_owned()),
        }
    }

    fn start(&mut self, instruction: &str) -> Result<(), String> {
        let run = RunId::new(format!("box-{}", now_ms()));
        self.start_as(run, instruction)
    }

    fn compact(&mut self, target: Option<String>) -> Result<(), String> {
        match &self.slot {
            Slot::Running(r) => {
                r.compact(target);
                Ok(())
            }
            _ => Err("no run in this window".to_owned()),
        }
    }

    fn pending_asks(&self) -> Vec<PendingAsk> {
        let mut asks: Vec<PendingAsk> = self
            .parts
            .as_ref()
            .map(|p| p.desk.pending())
            .unwrap_or_default();
        if let Some(prompt) = &self.fetch_prompt {
            asks.push(crate::dependency_prompt::pending_ask(&prompt.question));
        }
        asks
    }

    fn answer(&mut self, id: u64, answer: AskAnswer) -> Result<(), String> {
        if id == crate::dependency_prompt::FETCH_PROMPT_ID && self.fetch_prompt.is_some() {
            return self.answer_fetch_prompt(answer);
        }
        let parts = self.parts.as_ref().ok_or_else(|| NO_HOST.to_owned())?;
        parts.desk.answer(id, answer).map_err(|e| e.to_string())
    }

    fn poll(&mut self) -> Vec<BackendEvent> {
        let mut out = std::mem::take(&mut self.events);
        out.extend(self.land.drain());
        if let Slot::Starting(rx, starting) = &self.slot {
            let starting = starting.clone();
            let release = |parts: &Option<HostParts>| {
                if let Some(p) = parts {
                    let _ = p.housekeeping.finish_run(&starting);
                }
            };
            match rx.try_recv() {
                Ok(Ok(run)) => {
                    out.push(BackendEvent::Notice(format!("run {} started", run.run())));
                    self.slot = Slot::Running(run);
                }
                Ok(Err(e)) => {
                    // Decision 0087: a start stopped only for want of an
                    // answer about fetching is a question, not a failure.
                    let question = self
                        .project_host()
                        .and_then(|h| h.take_dependency_question(&starting));
                    match (question, self.starting_restart.take()) {
                        (Some(question), Some(restart)) => {
                            out.push(BackendEvent::Notice(
                                "the run waits for an answer: its dependencies are not in \
                                 the local cache"
                                    .to_owned(),
                            ));
                            self.fetch_prompt = Some(crate::dependency_prompt::FetchPrompt {
                                question,
                                restart,
                                once: std::mem::take(&mut self.starting_once),
                            });
                        }
                        _ => {
                            self.fail(format!("the run did not start: {e}"));
                            out.push(BackendEvent::Error(format!("the run did not start: {e}")));
                        }
                    }
                    release(&self.parts);
                    self.slot = Slot::Idle;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    out.push(BackendEvent::Error(
                        "the run's start task ended without an answer".to_owned(),
                    ));
                    release(&self.parts);
                    self.slot = Slot::Idle;
                }
            }
        }
        // Act on a routing decision once it arrives.
        // Dispatch (steps 1 and 4) reuses `start`, the same path `/run` uses,
        // so it only runs here, on the UI thread, never inside `route_one`.
        if let Some(rx) = &self.routing {
            match rx.try_recv() {
                Ok(RoutingResult::Attach {
                    node_id,
                    question,
                    method,
                }) => {
                    self.routing = None;
                    // Records this run's attachment on the graph (0064): the
                    // node's own change-log row is the record shown, so
                    // nothing here repeats it as a second surface.
                    if let Some(host) = self.project_host() {
                        let now = now_ms();
                        let _ =
                            host.with_graph(|g| g.adopt_presumed(&node_id, "box-question", now));
                    }
                    out.push(BackendEvent::Notice(format!(
                        "routed ({}): attached to roadmap node `{node_id}`; tasking an agent there",
                        method.as_str()
                    )));
                    if let Err(e) = self.start(&node_tagged_instruction(&node_id, &question)) {
                        out.push(BackendEvent::Error(format!("not dispatched: {e}")));
                    }
                }
                Ok(RoutingResult::Answer {
                    question,
                    answer,
                    method,
                }) => {
                    self.routing = None;
                    // route 2 used to leave no trace
                    // on the graph. Filed as a closed item under "Questions",
                    // with no agent dispatched.
                    if let Some(host) = self.project_host() {
                        let now = now_ms();
                        let _ = host
                            .with_graph(|g| record_answered(g, now, &question, &answer, method));
                    }
                    out.push(BackendEvent::Reply {
                        in_reply_to: String::new(),
                        text: answer,
                    });
                }
                Ok(RoutingResult::Offer { question, method }) => {
                    self.routing = None;
                    let id = self.next_offer_id;
                    self.next_offer_id += 1;
                    // the offer used to go unrecorded
                    // on the origin project's graph. `graph_id` is updated in
                    // place once the user resolves it (accept or decline).
                    let graph_id = self.project_host().and_then(|host| {
                        let now = now_ms();
                        host.with_graph(|g| record_offer_made(g, now, &question, method))
                            .ok()
                            .and_then(Result::ok)
                    });
                    self.offers.push(PendingOffer {
                        id,
                        question,
                        graph_id,
                    });
                }
                Ok(RoutingResult::Error(e)) => {
                    self.routing = None;
                    out.push(BackendEvent::Error(format!("routing failed: {e}")));
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.routing = None;
                    out.push(BackendEvent::Error(
                        "routing ended without an answer".to_owned(),
                    ));
                }
            }
        }
        let (events, run_id) = match &mut self.slot {
            Slot::Running(r) => (r.poll(), Some(r.run().clone())),
            _ => (Vec::new(), None),
        };
        for e in events {
            let finished = matches!(e, RunEvent::Finished(_));
            if let Some(b) = self.map(e) {
                out.push(b);
            }
            // The host's housekeeping for the ended run (sweep, unreconciled).
            if finished && let (Some(parts), Some(run)) = (&self.parts, &run_id) {
                out.extend(
                    parts
                        .housekeeping
                        .finish_run(run)
                        .into_iter()
                        .map(BackendEvent::Notice),
                );
            }
        }
        if matches!(&self.slot, Slot::Running(r) if r.is_finished()) {
            self.slot = Slot::Idle;
        }
        if let Some(parts) = &self.parts {
            out.extend(parts.desk.take_notices().into_iter().map(notice_text));
            out.extend(
                parts
                    .housekeeping
                    .take_notices()
                    .into_iter()
                    .map(|n| BackendEvent::Notice(format!("host: {n}"))),
            );
        }
        out
    }

    fn status_from_record(&self) -> Option<String> {
        self.record_status.clone()
    }

    fn pending_change(&self) -> Option<kgraf_orchestrator::ChangeView> {
        match &self.slot {
            Slot::Running(r) => r.pending_change(),
            _ => None,
        }
    }

    fn answer_change(&mut self, number: u32, yes: bool) -> Result<(), String> {
        match &self.slot {
            Slot::Running(r) => r.answer_change(number, yes),
            _ => Err("no run in this window".to_owned()),
        }
    }

    fn edit_target(&self) -> Option<crate::edit_card::EditTarget> {
        let attached = match &self.slot {
            Slot::Running(r) => Some(r.run().to_string()),
            Slot::Starting(_, run) => Some(run.to_string()),
            Slot::Idle => (!self.current.0.is_empty()).then(|| self.current.0.clone()),
        };
        let found = attached
            .as_ref()
            .and_then(|a| self.edit_records.iter().find(|r| &r.run == a));
        let (record, attached) = match found {
            Some(r) => (r, true),
            None => (self.edit_records.first()?, false),
        };
        Some(crate::edit_card::EditTarget {
            run: record.run.clone(),
            instruction: record.instruction.clone(),
            attached,
        })
    }

    fn edit(&mut self, request: crate::edit_card::EditRequest) -> Result<(), String> {
        if !request.new
            && let Some(why) = self
                .edit_records
                .iter()
                .find(|r| r.run == request.run)
                .and_then(|r| r.refusal.clone())
        {
            return Err(why);
        }
        if request.new {
            self.copy_from(RunId::new(request.run), request.edited)
        } else {
            let change = kgraf_orchestrator::ChangeInput::edit(
                request.edited,
                Some(request.opened),
                kgraf_orchestrator::Received::now(),
            );
            self.reopen_with(RunId::new(request.run), change)
        }
    }

    /// Through the live loop when this window runs the run; otherwise under
    /// the run's lease, off the UI thread.
    fn land(&mut self, request: kgraf_orchestrator::land_flow::LandRequest) -> Result<(), String> {
        match &self.slot {
            Slot::Running(r) => {
                r.land(request);
                Ok(())
            }
            Slot::Starting(..) => Err("the run is still starting".to_owned()),
            Slot::Idle => {
                let (root, run) = self.land_target()?;
                let runtime = self.runtime.as_ref().ok_or_else(|| NO_HOST.to_owned())?;
                self.land.land(runtime, root, run, request);
                Ok(())
            }
        }
    }

    fn land_facts(
        &mut self,
        now_ms: i64,
    ) -> Option<(
        kgraf_orchestrator::land_record::LandBook,
        kgraf_orchestrator::land_suggest::SuggestFacts,
    )> {
        let (root, run) = self.land_target().ok()?;
        let runtime = self.runtime.as_ref()?;
        self.land.facts(runtime, root, run, now_ms)
    }

    fn decline_land(&mut self, now_ms: i64) -> Result<(), String> {
        match &self.slot {
            Slot::Running(r) => {
                r.decline_land(now_ms);
                Ok(())
            }
            _ => {
                let (root, run) = self.land_target()?;
                let runtime = self.runtime.as_ref().ok_or_else(|| NO_HOST.to_owned())?;
                self.land.decline(runtime, root, run, now_ms);
                Ok(())
            }
        }
    }

    /// Sends through `InteractiveRun::send_to_session` (fire and forget); the
    /// real answer arrives later as `RunEvent::SessionSent`, mapped in `map`.
    fn send_to_session(
        &mut self,
        session: kgraf_core::AgentId,
        kind: kgraf_core::session::InboxKind,
        mode: kgraf_core::session::InjectMode,
        text: String,
    ) -> Result<(), String> {
        match &self.slot {
            Slot::Running(r) => {
                r.send_to_session(session, kind, mode, text);
                Ok(())
            }
            _ => Err("no run in this window".to_owned()),
        }
    }

    /// See `send_to_session`: the same shape, for `stop_session`.
    fn stop_session(&mut self, session: kgraf_core::AgentId) -> Result<(), String> {
        match &self.slot {
            Slot::Running(r) => {
                r.stop_session(session);
                Ok(())
            }
            _ => Err("no run in this window".to_owned()),
        }
    }

    /// See `send_to_session`: the same shape, for `resume_session`.
    fn resume_session(&mut self, session: kgraf_core::AgentId) -> Result<(), String> {
        match &self.slot {
            Slot::Running(r) => {
                r.resume_session(session);
                Ok(())
            }
            _ => Err("no run in this window".to_owned()),
        }
    }

    /// Real pending questions (design 0067, point 4) while a run is hosted;
    /// empty otherwise, like `pending_asks`. Read from the snapshot the run
    /// loop publishes (decision 0100), so a frame never waits for the loop.
    fn pending_questions(&self) -> Vec<crate::orchestrator_box::PendingQuestion> {
        match &self.slot {
            Slot::Running(r) => r
                .pending_questions()
                .into_iter()
                .map(|q| crate::orchestrator_box::PendingQuestion {
                    id: q.id,
                    worker: q.session,
                    question: q.question,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Answers through `InteractiveRun::answer_question` (fire and forget);
    /// the real answer arrives later as `RunEvent::QuestionAnswered`.
    fn answer_question(&mut self, question: u64, answer: String) -> Result<(), String> {
        match &self.slot {
            Slot::Running(r) => {
                r.answer_question(question, answer);
                Ok(())
            }
            _ => Err("no run in this window".to_owned()),
        }
    }

    /// Routes a question that arrived with no run hosted
    /// here. Spawns the classification (and, for "answer directly", the
    /// answer) on the host's own runtime; `poll` acts on the result.
    fn route_question(&mut self, text: &str) -> Result<(), String> {
        let parts = self.parts.as_ref().ok_or_else(|| NO_HOST.to_owned())?;
        let runtime = self.runtime.as_ref().ok_or_else(|| NO_HOST.to_owned())?;
        if self.routing.is_some() {
            return Err("a question is already being routed here".to_owned());
        }
        let Some(host) = parts.housekeeping.host() else {
            return Err("the project is not ready yet; try again shortly".to_owned());
        };
        let question = text.to_owned();
        let (tx, rx) = channel();
        let wake = self.wake.clone();
        runtime.spawn(async move {
            let result = route_one(host, question).await;
            let _ = tx.send(result);
            wake();
        });
        self.routing = Some(rx);
        Ok(())
    }

    fn pending_offers(&self) -> Vec<QuestionOffer> {
        self.offers
            .iter()
            .map(|o| QuestionOffer {
                id: o.id,
                question: o.question.clone(),
                graph_id: o.graph_id.clone(),
            })
            .collect()
    }

    /// Clears the offer; the box itself queues the new-project request, so a
    /// new project is opened at the workspace level, not here. That level
    /// (`layout.rs::take_new_project_offers`) records the resolution
    /// once the new project's name is known, using the
    /// offer's `graph_id` (`QuestionOffer::graph_id`, read from
    /// `pending_offers` before this call removes it).
    fn accept_question_offer(&mut self, id: u64) -> Result<(), String> {
        let before = self.offers.len();
        self.offers.retain(|o| o.id != id);
        if self.offers.len() == before {
            return Err("no such offer".to_owned());
        }
        Ok(())
    }

    /// Files the question under the standalone "Questions" node (created
    /// once, reused after) and dispatches under it through `start`, the same
    /// path `/run` uses. Also records the offer's resolution (fix,
    /// 2026-09-28) on the same node the offer itself was recorded on.
    fn permissions(&self) -> Option<crate::permission_bar::PermissionUi> {
        self.parts
            .as_ref()
            .and_then(|p| p.housekeeping.permissions())
    }

    fn pending_permissions(&self) -> Vec<kgraf_orchestrator::PermissionRequestView> {
        match &self.slot {
            Slot::Running(r) => r.pending_permissions(),
            _ => Vec::new(),
        }
    }

    fn answer_permission(&mut self, id: u64, grant: bool) -> Result<(), String> {
        match &self.slot {
            Slot::Running(r) => r.answer_permission(id, grant).map_err(|e| e.to_string()),
            _ => Err("no run in this window".to_owned()),
        }
    }

    fn pending_plan(&self) -> Option<kgraf_orchestrator::PlanView> {
        match &self.slot {
            Slot::Running(r) => r.pending_plan(),
            _ => None,
        }
    }

    fn answer_plan(&mut self, answer: kgraf_orchestrator::PlanAnswer) -> Result<(), String> {
        match &self.slot {
            Slot::Running(r) => r.answer_plan(answer).map_err(|e| e.to_string()),
            _ => Err("no run in this window".to_owned()),
        }
    }

    fn decline_question_offer(&mut self, id: u64) -> Result<(), String> {
        let Some(pos) = self.offers.iter().position(|o| o.id == id) else {
            return Err("no such offer".to_owned());
        };
        let offer = self.offers.remove(pos);
        let host = self
            .project_host()
            .ok_or_else(|| "the project is not ready yet; try again shortly".to_owned())?;
        if let Some(graph_id) = &offer.graph_id {
            let _ = host.with_graph(|g| {
                record_offer_resolved(
                    g,
                    now_ms(),
                    graph_id,
                    &offer.question,
                    OfferResolution::Declined,
                )
            });
        }
        let node_id = host
            .with_graph(|g| ensure_questions_node(g, now_ms()))
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        self.start(&node_tagged_instruction(&node_id, &offer.question))
    }
}

/// How long closing the window waits for a hosted run to stop.
pub const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

impl OrchestratorHost {
    /// Begins stopping the hosted run off the calling thread (quitting must
    /// not block the UI). Returns a receiver that yields whether the run ended
    /// within [`STOP_WAIT`], or `None` when no run is hosted. The window can close
    /// at once and exit when the receiver answers.
    pub fn start_shutdown(&mut self) -> Option<Receiver<bool>> {
        let Slot::Running(_) = &self.slot else {
            return None;
        };
        let Slot::Running(mut run) = std::mem::replace(&mut self.slot, Slot::Idle) else {
            return None;
        };
        let (tx, rx) = channel();
        let housekeeping = self.parts.as_ref().map(|p| p.housekeeping.clone());
        let spawned = std::thread::Builder::new()
            .name("run-shutdown".into())
            .spawn(move || {
                let ended = run.stop_and_wait(STOP_WAIT);
                if let Some(h) = housekeeping {
                    for line in h.finish_run(run.run()) {
                        log::info!("{line}");
                    }
                }
                if !ended {
                    log::warn!(
                        "run {} did not stop within {STOP_WAIT:?}; it resumes from its last save",
                        run.run()
                    );
                }
                let _ = tx.send(ended);
            });
        match spawned {
            Ok(_) => Some(rx),
            Err(e) => {
                log::error!("the shutdown thread could not start ({e}); closing without waiting");
                None
            }
        }
    }
}

impl Drop for OrchestratorHost {
    /// A hosted run still here is stopped for real (its workers cancelled, its
    /// record saved as stopped) within [`STOP_WAIT`]; only then are open asks
    /// denied (the desk closes after the workers stop, so no worker is told
    /// "denied" for a close that also cancels it), and the runtime shut down. The
    /// window's own quit uses [`OrchestratorHost::start_shutdown`] instead, so this
    /// wait runs on the UI thread only on paths that bypass it (macOS Cmd+Q).
    fn drop(&mut self) {
        if let Slot::Running(r) = &mut self.slot
            && !r.stop_and_wait(STOP_WAIT)
        {
            log::warn!(
                "run {} did not stop within {STOP_WAIT:?}; its record resumes from its last save",
                r.run()
            );
            eprintln!(
                "kgraf-app: run {} did not stop within {STOP_WAIT:?}; it resumes from its last save",
                r.run()
            );
        }
        if let Some(parts) = &self.parts {
            parts.desk.close();
        }
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_factory_the_host_is_idle_and_says_why_and_starts_no_runtime() {
        let mut host = OrchestratorHost::new(None, Arc::new(|| {})).expect("host");
        assert!(host.runtime.is_none(), "no threads when not hosting");
        assert_eq!(
            host.state(),
            BackendState::Idle {
                can_start: false,
                reason: NO_HOST.to_owned()
            }
        );
        assert_eq!(host.start("do it"), Err(NO_HOST.to_owned()));
        assert!(host.send(InboundKind::Note, "x").is_err());
        assert!(host.compact(None).is_err());
        assert!(host.pending_asks().is_empty());
        assert!(host.poll().is_empty());
        host.set_record_status(Some("run r1 (running)".into()));
        assert_eq!(
            host.status_from_record().as_deref(),
            Some("run r1 (running)")
        );
    }

    /// The dependency question's lifecycle, over a real project host whose
    /// model refuses every call (these tests are about the start, not the run).
    mod fetch_prompt {
        use super::*;
        use crate::dependency_prompt::{FETCH_PROMPT_ID, FetchPrompt, Restart};
        use crate::scheduler_bg::ProjectFactory;
        use kgraf_host::RunConfig;
        use kgraf_host::deps::{ConsentQuestion, Registry};
        use kgraf_orchestrator::AskDesk;
        use std::path::Path;

        struct NoModel;

        #[async_trait::async_trait]
        impl kgraf_provider::Provider for NoModel {
            fn adapter(&self) -> &'static str {
                "fake"
            }
            fn protocol(&self) -> kgraf_core::Protocol {
                kgraf_core::Protocol::Additive
            }
            async fn complete(
                &self,
                request: &kgraf_provider::Request,
                _token: &kgraf_provider::CallToken,
            ) -> Result<kgraf_provider::Response, kgraf_provider::ProviderError> {
                Err(kgraf_provider::ProviderError::ModelUnsupported {
                    adapter: "fake",
                    model: request.model.clone(),
                    available: "none".into(),
                })
            }
        }

        fn git(dir: &Path, args: &[&str]) {
            let out = std::process::Command::new("git")
                .current_dir(dir)
                .args(["-c", "core.hooksPath=/dev/null"])
                .args(args)
                .output()
                .expect("git");
            assert!(out.status.success(), "{args:?}: {out:?}");
        }

        fn window() -> (tempfile::TempDir, Arc<kgraf_host::Host>, OrchestratorHost) {
            let dir = tempfile::tempdir().expect("dir");
            let root = dir.path().join("repo");
            std::fs::create_dir_all(&root).expect("mkdir");
            git(&root, &["init", "-q", "-b", "main"]);
            git(&root, &["config", "user.name", "t"]);
            git(&root, &["config", "user.email", "t@t"]);
            std::fs::write(root.join("README.md"), "# t\n").expect("write");
            std::fs::write(root.join(".gitignore"), ".kgraf/\n").expect("write");
            git(&root, &["add", "."]);
            git(&root, &["commit", "-q", "-m", "init"]);
            let prices = dir.path().join("prices.json");
            std::fs::write(
                &prices,
                r#"{"m": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1}}"#,
            )
            .expect("prices");
            let host = Arc::new(
                kgraf_host::Host::open(RunConfig::new(&root, vec!["fake:m".into()], &prices))
                    .expect("open")
                    .with_route_provider("fake", Arc::new(NoModel))
                    .expect("route"),
            );
            let factory = Arc::new(ProjectFactory::with_host(host.clone()));
            let parts = HostParts {
                factory: factory.clone(),
                housekeeping: factory,
                desk: Arc::new(AskDesk::new(None, Arc::new(|| {}))),
                router: None,
            };
            let window = OrchestratorHost::new(Some(parts), Arc::new(|| {})).expect("window");
            (dir, host, window)
        }

        fn prompt(restart: Restart) -> FetchPrompt {
            FetchPrompt {
                question: ConsentQuestion {
                    subject: kgraf_host::deps::ConsentSubject::Fetch(Registry::CratesIo),
                    project: "/p".into(),
                    detail: "no matching package named `itoa`".into(),
                },
                restart,
                once: Vec::new(),
            }
        }

        fn asked(w: &OrchestratorHost) -> bool {
            w.pending_asks().iter().any(|a| a.id == FETCH_PROMPT_ID)
        }

        /// No host to apply the answer to: the answer fails and the question
        /// stays open, where it used to be lost.
        #[test]
        fn an_answer_that_cannot_be_applied_keeps_the_question() {
            let mut w = OrchestratorHost::idle(Arc::new(|| {}));
            w.fetch_prompt = Some(prompt(Restart::Start("x".into())));
            assert!(w.answer(FETCH_PROMPT_ID, AskAnswer::AllowOnce).is_err());
            assert!(asked(&w), "the question was lost with the failed answer");
        }

        /// A window that already has a run refuses the restart before the
        /// answer is applied: no grant is spent, and the question stays.
        #[test]
        fn a_start_that_cannot_begin_keeps_the_question_and_spends_no_grant() {
            let (_dir, host, mut w) = window();
            w.start("first").expect("start");
            w.fetch_prompt = Some(prompt(Restart::Start("second".into())));
            let err = w
                .answer(FETCH_PROMPT_ID, AskAnswer::AllowAlways)
                .expect_err("a run is already here");
            assert!(err.contains("already"), "{err}");
            assert!(asked(&w));
            assert_eq!(
                host.dependency_consents()
                    .get(Registry::CratesIo)
                    .expect("read"),
                kgraf_host::deps::Consent::Unset,
                "the answer was stored though nothing started"
            );
        }

        /// A start can stop on two questions in turn, and each answer starts
        /// it again under a new run id. An "Allow once" given to the first
        /// question is given again to that id with the second, or the start
        /// would ask the first again, and again.
        #[test]
        fn an_earlier_allow_once_is_given_again_to_the_run_that_starts() {
            let (_dir, host, mut w) = window();
            let mut p = prompt(Restart::Start("x".into()));
            p.question.subject = kgraf_host::deps::ConsentSubject::Install(Vec::new());
            p.once = vec![kgraf_host::deps::ConsentSubject::Fetch(Registry::CratesIo)];
            w.fetch_prompt = Some(p);
            w.answer(FETCH_PROMPT_ID, AskAnswer::AllowOnce)
                .expect("starts again");
            let held = host.retained();
            assert_eq!(
                (held.fetch_grants, held.install_grants),
                (1, 1),
                "each question's own grant, for the run that starts: {held:?}"
            );
            assert_eq!(w.starting_once.len(), 2);
        }

        /// Starting or resuming another run drops a question left open.
        #[test]
        fn starting_another_run_clears_a_stale_question() {
            let (_dir, _host, mut w) = window();
            w.fetch_prompt = Some(prompt(Restart::Start("old".into())));
            w.start("new").expect("start");
            assert!(!asked(&w), "a stale question stayed open");

            let (_dir, _host, mut w) = window();
            w.fetch_prompt = Some(prompt(Restart::Start("old".into())));
            w.resume(RunId::new("box-1")).expect("resume begins");
            assert!(!asked(&w), "a stale question stayed open after a resume");
        }
    }
}
