//! The approval desk: who answers a worker's approval ask when the orchestrator runs
//! inside the desktop shell (pending tool approvals show
//! on the node or in the orchestrator box).
//!
//! [`AskDesk`] implements the runtime's [`AskResolver`]. A factory that builds
//! orchestrators for the shell sets it as every worker environment's `asks`. The
//! answer comes from, in order:
//!
//! 1. **A persisted allow rule** ([`DeskRules`], the project's
//!    `PersistedAllowStore`), exactly as the CLI's `RuleOrPromptResolver` consults
//!    it. A call a rule covers runs, and the desk says so ([`DeskNotice::Covered`]).
//! 2. **The user, in the orchestrator box**: the ask waits as a [`PendingAsk`] until
//!    [`AskDesk::answer`] is called with allow once, allow always or deny.
//!    Allow always records an exact rule for the call's permission subject, the same
//!    rule the CLI records for `a` at its terminal. A tool that reports no exact
//!    subject cannot be named by a rule; the call is then allowed once, and the desk
//!    says that no rule was recorded.
//! 3. **A denial**, when the desk is closed ([`AskDesk::close`]) with asks open.
//!
//! The answer semantics are the CLI resolver's, ported (same file:
//! `crates/kgraf-cli/src/worker/ask.rs`); only the place the person answers
//! differs. The waiting is a oneshot per ask, so the desk holds no lock across an
//! await.

use crate::orchestrator::SessionError;
use kgraf_core::AgentId;
use kgraf_runtime::AskResolver;
use kgraf_session::approval_store::PersistedAllowStore;
use kgraf_tools::ToolRegistry;
use kgraf_tools::approval::{PausedCall, UserAnswer};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

pub use crate::box_port::Wake;

/// One ask waiting for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingAsk {
    /// The desk's id for it; [`AskDesk::answer`] takes it.
    pub id: u64,
    /// The worker whose call is paused.
    pub worker: String,
    /// The worker's task.
    pub task: String,
    /// The tool's wire name.
    pub tool: String,
    /// The hook's question, verbatim.
    pub question: String,
    /// The hook that asked.
    pub asked_by: String,
    /// The call's arguments, as pretty JSON, verbatim.
    pub input: String,
}

/// The user's answer in the box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskAnswer {
    AllowOnce,
    /// Allow, and record a rule so this exact call is not asked about again.
    AllowAlways,
    Deny {
        reason: String,
    },
}

/// Something the desk did without the user, reported so it is never silent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeskNotice {
    /// A persisted rule covered the call; it ran unasked.
    Covered {
        tool: String,
        worker: String,
        note: String,
    },
    /// Allow always recorded a rule.
    RuleRecorded { tool: String, subject: String },
    /// Allow always could not record a rule; the call was allowed once.
    RuleNotRecorded { tool: String, reason: String },
    /// The desk closed with the ask open; the call was denied.
    DeniedOnClose { tool: String, worker: String },
    /// Nobody answered within the desk's timeout; the call was denied.
    TimedOut {
        tool: String,
        worker: String,
        after: std::time::Duration,
    },
    /// The allow-rule file changed and could not be read again; the rules read
    /// before stay in force.
    RulesUnreadable { reason: String },
}

/// Why an answer was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AskError {
    #[error("no pending ask {0}: it was answered already, or its worker stopped")]
    Unknown(u64),
}

/// The project's persisted allow rules, as the CLI keeps them. The file is read
/// again whenever its length or modification time changes (a rule added at the
/// CLI's terminal, or by hand, is seen at the next ask).
pub struct DeskRules {
    store: Mutex<Loaded>,
    path: std::path::PathBuf,
    workspace_root: std::path::PathBuf,
    project: String,
    registry: Arc<ToolRegistry>,
}

/// A file's length and modification time.
type Stamp = Option<(u64, Option<std::time::SystemTime>)>;
/// The store as read, and the stamp of the file it was read from.
type Loaded = (PersistedAllowStore, Stamp);

fn file_stamp(path: &std::path::Path) -> Stamp {
    std::fs::metadata(path)
        .ok()
        .map(|m| (m.len(), m.modified().ok()))
}

impl DeskRules {
    /// Opens the store at `path` (refused inside `workspace_root`, as the CLI's).
    pub fn open(
        path: impl Into<std::path::PathBuf>,
        workspace_root: impl Into<std::path::PathBuf>,
        project: impl Into<String>,
        registry: Arc<ToolRegistry>,
    ) -> Result<Self, kgraf_session::SessionError> {
        let path = path.into();
        let workspace_root = workspace_root.into();
        let (store, _) = PersistedAllowStore::open(&path, &workspace_root)?;
        let stamp = file_stamp(&path);
        Ok(Self {
            store: Mutex::new((store, stamp)),
            path,
            workspace_root,
            project: project.into(),
            registry,
        })
    }

    fn subject(&self, paused: &PausedCall) -> Option<String> {
        let call = paused.call();
        self.registry
            .get(&call.tool)?
            .tool()
            .permission_subject(&call.input)
    }

    /// The store, read again first when the file changed on disk. A reload that
    /// fails keeps the rules already read and says why.
    fn fresh(&self) -> (std::sync::MutexGuard<'_, Loaded>, Option<String>) {
        let mut g = self.store.lock().unwrap_or_else(|p| p.into_inner());
        let now = file_stamp(&self.path);
        let mut problem = None;
        if now != g.1 {
            match PersistedAllowStore::open(&self.path, &self.workspace_root) {
                Ok((store, _)) => *g = (store, now),
                Err(e) => problem = Some(format!("the allow rules could not be read again: {e}")),
            }
        }
        (g, problem)
    }

    fn covering(&self, paused: &PausedCall) -> (Option<String>, Option<String>) {
        let subject = self.subject(paused);
        let (g, problem) = self.fresh();
        let note =
            g.0.matching_rule(&self.project, &paused.call().tool, subject.as_deref())
                .map(|r| r.note.clone());
        (note, problem)
    }

    /// Records the allow-always rule for this call: an exact rule for its
    /// permission subject, or, for an `mcp__` tool (which reports no subject), an
    /// any-call rule for the tool, as the CLI's terminal `a` and `kgraf allow`
    /// record it. Returns what the rule covers.
    fn record(&self, paused: &PausedCall) -> Result<String, String> {
        let tool = paused.call().tool.clone();
        let subject = self.subject(paused);
        let (mut g, _) = self.fresh();
        let covers = match subject {
            Some(subject) => {
                g.0.allow_exact(
                    self.project.clone(),
                    tool,
                    subject.clone(),
                    "allowed always in the orchestrator box",
                )
                .map_err(|e| e.to_string())?;
                subject
            }
            None if tool.starts_with("mcp__") => {
                g.0.allow_any_call(
                    self.project.clone(),
                    tool.clone(),
                    "allowed always in the orchestrator box",
                    kgraf_session::approval_store::ConfirmAllowAnyCall,
                )
                .map_err(|e| e.to_string())?;
                format!("any call to {tool}")
            }
            None => {
                return Err(format!(
                    "{tool} reports no exact subject for this call, so no rule can name it"
                ));
            }
        };
        // Our own write is not a change to read again.
        g.1 = file_stamp(&self.path);
        Ok(covers)
    }
}

struct Waiting {
    ask: PendingAsk,
    paused: PausedCall,
    reply: oneshot::Sender<UserAnswer>,
}

#[derive(Default)]
struct DeskState {
    next: u64,
    waiting: BTreeMap<u64, Waiting>,
    notices: Vec<DeskNotice>,
    closed: bool,
}

/// How long an ask waits for the user before it is denied: the CLI's terminal
/// prompt timeout (`kgraf-cli/src/worker/ask.rs`, `DEFAULT_PROMPT_TIMEOUT`).
/// An unanswered ask must not hold a worker forever.
pub const DEFAULT_ASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Answers approval asks from rules or from the user in the box.
pub struct AskDesk {
    state: Mutex<DeskState>,
    rules: Option<DeskRules>,
    wake: Wake,
    timeout: std::time::Duration,
}

impl std::fmt::Debug for AskDesk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskDesk")
            .field("pending", &self.pending().len())
            .field("rules", &self.rules.is_some())
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Removes an ask from the desk when the future waiting on it ends for any
/// reason: answered, timed out, or dropped because its worker was aborted. A
/// dropped worker must not leave a ghost ask in the box.
struct WaitGuard<'a> {
    desk: &'a AskDesk,
    id: u64,
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        let removed = self.desk.state().waiting.remove(&self.id).is_some();
        if removed {
            (self.desk.wake)();
        }
    }
}

impl AskDesk {
    /// A desk. Without `rules`, nothing is covered and allow always allows once,
    /// saying no rule store is configured. `wake` runs whenever the desk changes.
    /// Asks wait [`DEFAULT_ASK_TIMEOUT`].
    pub fn new(rules: Option<DeskRules>, wake: Wake) -> Self {
        Self {
            state: Mutex::new(DeskState::default()),
            rules,
            wake,
            timeout: DEFAULT_ASK_TIMEOUT,
        }
    }

    /// The same desk with asks denied after `timeout` unanswered.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn state(&self) -> std::sync::MutexGuard<'_, DeskState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Asks waiting for the user, oldest first.
    pub fn pending(&self) -> Vec<PendingAsk> {
        self.state()
            .waiting
            .values()
            .map(|w| w.ask.clone())
            .collect()
    }

    /// What the desk did on its own since the last call.
    pub fn take_notices(&self) -> Vec<DeskNotice> {
        std::mem::take(&mut self.state().notices)
    }

    /// Answer one pending ask. An ask whose worker has gone is refused before
    /// anything is recorded; allow always records its rule only once the answer
    /// has reached the worker.
    pub fn answer(&self, id: u64, answer: AskAnswer) -> Result<(), AskError> {
        let waiting = self
            .state()
            .waiting
            .remove(&id)
            .ok_or(AskError::Unknown(id))?;
        if waiting.reply.is_closed() {
            return Err(AskError::Unknown(id));
        }
        let tool = waiting.ask.tool.clone();
        let always = answer == AskAnswer::AllowAlways;
        let reply = match answer {
            AskAnswer::AllowOnce | AskAnswer::AllowAlways => UserAnswer::Allow,
            AskAnswer::Deny { reason } => UserAnswer::Deny { reason },
        };
        waiting
            .reply
            .send(reply)
            .map_err(|_| AskError::Unknown(id))?;
        if always {
            let notice = match &self.rules {
                None => DeskNotice::RuleNotRecorded {
                    tool,
                    reason: "no allow-rule store is configured for this project; allowed once"
                        .into(),
                },
                Some(rules) => match rules.record(&waiting.paused) {
                    Ok(subject) => DeskNotice::RuleRecorded { tool, subject },
                    Err(reason) => DeskNotice::RuleNotRecorded {
                        tool,
                        reason: format!("{reason}; allowed once"),
                    },
                },
            };
            self.state().notices.push(notice);
        }
        (self.wake)();
        Ok(())
    }

    /// Deny every open ask and every later one: the box is going away.
    pub fn close(&self) {
        let waiting = {
            let mut s = self.state();
            s.closed = true;
            std::mem::take(&mut s.waiting)
        };
        let mut notices = Vec::new();
        for (_, w) in waiting {
            notices.push(DeskNotice::DeniedOnClose {
                tool: w.ask.tool.clone(),
                worker: w.ask.worker.clone(),
            });
            let _ = w.reply.send(UserAnswer::Deny {
                reason: "the orchestrator box closed before the user answered".into(),
            });
        }
        self.state().notices.extend(notices);
        (self.wake)();
    }
}

/// Persist-scrubbed (decision 0071): `question` and `input` are what
/// the shell shows live in the orchestrator box while a call waits for an
/// answer, before the call is ever turned into a `WorkerEvent::Approval`
/// (which is scrubbed at that later point too, in `RecordingLog::record`) --
/// this is the earlier, live-display path a scrub there does not reach. A
/// tool call's own argument, `input`, is exactly where a
/// `curl -H "token ghp_..."`-shaped command shows its credential.
fn pending_of(id: u64, paused: &PausedCall) -> PendingAsk {
    let call = paused.call();
    let input = serde_json::to_string_pretty(&call.input)
        .unwrap_or_else(|e| format!("(the arguments could not be shown: {e})"));
    PendingAsk {
        id,
        worker: call.worker.to_string(),
        task: call.task.to_string(),
        tool: call.tool.clone(),
        question: scrub(paused.question()),
        asked_by: paused.asked_by().to_owned(),
        input: scrub(&input),
    }
}

fn scrub(text: &str) -> String {
    match kgraf_secrets::scrub_persist(text) {
        std::borrow::Cow::Borrowed(_) => text.to_owned(),
        std::borrow::Cow::Owned(scrubbed) => scrubbed,
    }
}

#[async_trait::async_trait]
impl AskResolver for AskDesk {
    async fn answer(&self, paused: &PausedCall) -> UserAnswer {
        self.ask_within(paused, self.timeout).await
    }
}

impl AskDesk {
    /// Ask the user in the box, waiting at most `timeout` (then denied).
    pub async fn ask_within(
        &self,
        paused: &PausedCall,
        timeout: std::time::Duration,
    ) -> UserAnswer {
        if let Some(rules) = &self.rules {
            let (note, problem) = rules.covering(paused);
            if let Some(reason) = problem {
                self.state()
                    .notices
                    .push(DeskNotice::RulesUnreadable { reason });
            }
            if let Some(note) = note {
                self.state().notices.push(DeskNotice::Covered {
                    tool: paused.call().tool.clone(),
                    worker: paused.call().worker.to_string(),
                    note,
                });
                (self.wake)();
                return UserAnswer::Allow;
            }
        }
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut s = self.state();
            if s.closed {
                return UserAnswer::Deny {
                    reason: "the orchestrator box is closed; nobody can answer".into(),
                };
            }
            s.next += 1;
            let id = s.next;
            s.waiting.insert(
                id,
                Waiting {
                    ask: pending_of(id, paused),
                    paused: paused.clone(),
                    reply: tx,
                },
            );
            id
        };
        let _guard = WaitGuard { desk: self, id };
        (self.wake)();
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => UserAnswer::Deny {
                reason: "the ask was dropped before the user answered".into(),
            },
            Err(_) => {
                self.state().notices.push(DeskNotice::TimedOut {
                    tool: paused.call().tool.clone(),
                    worker: paused.call().worker.to_string(),
                    after: timeout,
                });
                UserAnswer::Deny {
                    reason: format!("no answer in the orchestrator box within {:?}", timeout),
                }
            }
        }
    }
}

impl AskDesk {
    /// The answer for a run nobody attends (0052): allowed when a persisted rule
    /// covers the call, denied at once otherwise, with the CLI's unattended
    /// wording. Never waits and never shows an ask in the box.
    pub fn answer_unattended(&self, paused: &PausedCall) -> UserAnswer {
        let tool = paused.call().tool.clone();
        if let Some(rules) = &self.rules {
            let (note, problem) = rules.covering(paused);
            if let Some(reason) = problem {
                self.state()
                    .notices
                    .push(DeskNotice::RulesUnreadable { reason });
            }
            if let Some(note) = note {
                self.state().notices.push(DeskNotice::Covered {
                    tool,
                    worker: paused.call().worker.to_string(),
                    note,
                });
                (self.wake)();
                return UserAnswer::Allow;
            }
        }
        let how = if tool.starts_with("mcp__") {
            format!(
                "record one with `kgraf allow {tool}`, or answer allow always in the \
                 orchestrator box"
            )
        } else {
            "answer allow always in the orchestrator box to record one for this exact call"
                .to_owned()
        };
        UserAnswer::Deny {
            reason: format!(
                "{} Nobody is attached to answer, and no persisted allow rule for this project \
                 covers it; {how}.",
                paused.question()
            ),
        }
    }
}

/// How long a scheduled run's ask waits in the box when a person is there.
pub const SCHEDULED_ASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How often a waiting scheduled-run ask checks that the person is still there.
const ATTENDANCE_CHECK: std::time::Duration = std::time::Duration::from_millis(250);

/// Routes each approval ask by where its run came from: a run started from the
/// orchestrator box asks the user there ([`AskDesk`], 300 s timeout); any other
/// run (a scheduled run, 0052) is unattended: allowed only by a persisted rule,
/// denied at once otherwise ([`AskDesk::answer_unattended`]). A scheduled run's
/// ask is shown in the box only while a person is there (the window visible and
/// focused, with input recently: [`AskRouter::set_attended_until`]), waits at most
/// [`SCHEDULED_ASK_TIMEOUT`], and is denied the moment attendance lapses. The host
/// gets this router as its one ask resolver, so box and scheduled runs share a
/// host without a scheduled run waiting on nobody.
pub struct AskRouter {
    desk: Arc<AskDesk>,
    /// Runs started from the box.
    interactive: Mutex<std::collections::BTreeSet<String>>,
    /// Until when a person is at the box: the window visible and focused, and
    /// the last input recent enough. `None` when nobody is.
    attended_until: Mutex<Option<std::time::Instant>>,
    scheduled_timeout: std::time::Duration,
}

impl std::fmt::Debug for AskRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskRouter").finish_non_exhaustive()
    }
}

impl AskRouter {
    pub fn new(desk: Arc<AskDesk>) -> Self {
        Self {
            desk,
            interactive: Mutex::new(std::collections::BTreeSet::new()),
            attended_until: Mutex::new(None),
            scheduled_timeout: SCHEDULED_ASK_TIMEOUT,
        }
    }

    /// The same router with scheduled-run asks waiting at most `timeout`.
    pub fn with_scheduled_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.scheduled_timeout = timeout;
        self
    }

    pub fn desk(&self) -> &Arc<AskDesk> {
        &self.desk
    }

    /// `run` was started from the box: its asks go to the desk.
    pub fn mark_interactive(&self, run: &str) {
        self.interactive
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(run.to_owned());
    }

    /// Until when a person is at the box (`None`: nobody). The shell passes the
    /// last input's time plus its attention window while the window is visible
    /// and focused, and `None` otherwise.
    pub fn set_attended_until(&self, until: Option<std::time::Instant>) {
        *self
            .attended_until
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = until;
    }

    /// Whether a person is at the box now.
    pub fn attended_now(&self) -> bool {
        self.attended_until
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some_and(|until| std::time::Instant::now() < until)
    }

    /// The run an ask belongs to: the task id's run part (`<run>/<contract>`).
    fn run_of(paused: &PausedCall) -> String {
        let task = paused.call().task.to_string();
        task.split_once('/')
            .map_or(task.clone(), |(run, _)| run.to_owned())
    }
}

#[async_trait::async_trait]
impl AskResolver for AskRouter {
    async fn answer(&self, paused: &PausedCall) -> UserAnswer {
        let interactive = self
            .interactive
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&Self::run_of(paused));
        if interactive {
            return AskResolver::answer(&*self.desk, paused).await;
        }
        if !self.attended_now() {
            return self.desk.answer_unattended(paused);
        }
        // A scheduled run's ask, shown while a person is there: it waits at most
        // the scheduled timeout, and is denied the moment attendance lapses.
        let lapse = async {
            while self.attended_now() {
                tokio::time::sleep(ATTENDANCE_CHECK).await;
            }
        };
        tokio::select! {
            answer = self.desk.ask_within(paused, self.scheduled_timeout) => answer,
            () = lapse => UserAnswer::Deny {
                reason: format!(
                    "{} Nobody is at the orchestrator box any more (the window lost focus or \
                     saw no input), and this scheduled run is unattended.",
                    paused.question()
                ),
            },
        }
    }
}

/// One `ask_user_question` waiting for the user, once the orchestrator could
/// not answer it from the run record (decision 0067, point 4). Shaped like [`AskDesk`]'s own waiting list (a numbered pending
/// question, listed until answered), but for free-text answers rather than
/// allow/deny, so it does not reuse [`PausedCall`]'s sealed approval-chain
/// type, which names a permission decision, not a question and its text.
///
/// Nothing waits forever: every question carries a deadline from
/// [`QuestionAttendance`], and the orchestrator's loop takes the expired ones
/// ([`QuestionDesk::take_ended`]) on its tick and answers them with the
/// recorded no-answer text. A question whose worker stopped waiting (its
/// reply channel closed) is taken too, so no ghost card stays in the box.
#[derive(Default)]
pub struct QuestionDesk {
    state: Mutex<QuestionDeskState>,
}

#[derive(Default)]
struct QuestionDeskState {
    next: u64,
    waiting: BTreeMap<u64, PendingQuestion>,
}

struct PendingQuestion {
    session: AgentId,
    question: String,
    deadline: std::time::Instant,
    reply: oneshot::Sender<Result<String, SessionError>>,
}

/// One question waiting for the user, as the box lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingQuestionView {
    pub id: u64,
    pub session: String,
    pub question: String,
}

/// Who can answer a worker's question in time, which sets how long it waits
///. It follows [`AskRouter`]'s rule for approval asks: a run the
/// box started is attended and waits [`DEFAULT_ASK_TIMEOUT`], as an approval
/// ask in the box does; any other run (scheduled, or started from the CLI,
/// where no box exists) is unattended and waits [`SCHEDULED_ASK_TIMEOUT`].
/// Neither waits forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionAttendance {
    /// Started from the orchestrator box ([`crate::InteractiveRun`]).
    Box,
    /// Nobody is known to be there.
    Unattended,
}

impl QuestionAttendance {
    /// How long a question waits for the user.
    pub fn timeout(self) -> std::time::Duration {
        match self {
            QuestionAttendance::Box => DEFAULT_ASK_TIMEOUT,
            QuestionAttendance::Unattended => SCHEDULED_ASK_TIMEOUT,
        }
    }
}

/// A question [`QuestionDesk::take_ended`] took off the desk.
pub(crate) struct EndedQuestion {
    pub session: AgentId,
    /// `true`: its deadline passed with the worker still waiting; `false`:
    /// the worker stopped waiting (its reply channel closed).
    pub expired: bool,
    pub reply: oneshot::Sender<Result<String, SessionError>>,
}

impl std::fmt::Debug for QuestionDesk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuestionDesk")
            .field("pending", &self.pending_count())
            .finish()
    }
}

impl QuestionDesk {
    fn state(&self) -> std::sync::MutexGuard<'_, QuestionDeskState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn pending_count(&self) -> usize {
        self.state().waiting.len()
    }

    /// Queue `session`'s `question` for the user until `deadline`; `reply` is
    /// answered later, by [`Self::answer`] or, once the deadline passes, by
    /// the loop's tick. Never blocks: the caller stores the reply and returns
    /// at once, so the orchestrator's loop stays free while a question waits.
    pub(crate) fn wait_for(
        &self,
        session: AgentId,
        question: String,
        deadline: std::time::Instant,
        reply: oneshot::Sender<Result<String, SessionError>>,
    ) -> u64 {
        let mut s = self.state();
        s.next += 1;
        let id = s.next;
        s.waiting.insert(
            id,
            PendingQuestion {
                session,
                question,
                deadline,
                reply,
            },
        );
        id
    }

    /// Questions waiting for the user, oldest first (shown as pinned cards). A
    /// question whose worker stopped waiting is not listed.
    pub fn pending(&self) -> Vec<PendingQuestionView> {
        self.state()
            .waiting
            .iter()
            .filter(|(_, p)| !p.reply.is_closed())
            .map(|(id, p)| PendingQuestionView {
                id: *id,
                session: p.session.to_string(),
                question: p.question.clone(),
            })
            .collect()
    }

    /// Whether `session` has a question still waiting.
    pub(crate) fn is_waiting(&self, session: &AgentId) -> bool {
        self.state()
            .waiting
            .values()
            .any(|p| &p.session == session && !p.reply.is_closed())
    }

    /// Take every question whose deadline passed at `now`, and every question
    /// whose worker stopped waiting. The caller answers the expired ones.
    pub(crate) fn take_ended(&self, now: std::time::Instant) -> Vec<EndedQuestion> {
        let mut s = self.state();
        let ended: Vec<u64> = s
            .waiting
            .iter()
            .filter(|(_, p)| p.reply.is_closed() || p.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        ended
            .into_iter()
            .filter_map(|id| s.waiting.remove(&id))
            .map(|p| EndedQuestion {
                session: p.session,
                expired: !p.reply.is_closed(),
                reply: p.reply,
            })
            .collect()
    }

    /// Drop every waiting question (the run ended): each worker still waiting
    /// sees its call end as stopped.
    pub(crate) fn clear(&self) {
        self.state().waiting.clear();
    }

    /// The user's answer to pending question `id`. `None`: no such pending
    /// question (already answered, timed out, or its worker stopped waiting).
    /// Otherwise: the session it was asked for, so the caller can record the
    /// answer to that session's inbox (visible in its transcript view).
    pub(crate) fn answer(&self, id: u64, answer: String) -> Option<AgentId> {
        let pending = self.state().waiting.remove(&id)?;
        let session = pending.session;
        pending.reply.send(Ok(answer)).ok().map(|()| session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A paused call as a worker's chain would hand it over. The desk reads it and
    /// never checks its seal (the chain does, on resume), so a deserialized value
    /// is the real type in the real shape.
    fn paused(tool: &str, worker: &str) -> PausedCall {
        serde_json::from_value(serde_json::json!({
            "call": {
                "call_id": "c1",
                "tool": tool,
                "input": {"command": ["git", "push"]},
                "worker": worker,
                "task": "t1"
            },
            "asked_by": "shell",
            "question": "run `git push`?",
            "next_hook": 1,
            "rewritten": false,
            "deadline_ms": null,
            "chain_digest": "d",
            "seal": "s"
        }))
        .expect("paused call")
    }

    fn desk() -> (Arc<AskDesk>, Arc<AtomicUsize>) {
        let n = Arc::new(AtomicUsize::new(0));
        let m = n.clone();
        let wake: Wake = Arc::new(move || {
            m.fetch_add(1, Ordering::SeqCst);
        });
        (Arc::new(AskDesk::new(None, wake)), n)
    }

    async fn until_pending(desk: &AskDesk) -> PendingAsk {
        for _ in 0..200 {
            if let Some(a) = desk.pending().into_iter().next() {
                return a;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("no ask reached the desk");
    }

    /// A pending ask's `question` and `input` (the shell's live
    /// orchestrator-box display, before any `WorkerEvent::Approval` exists)
    /// are scrubbed of both a registered exact secret and a gitleaks-shaped
    /// pattern secret.
    #[tokio::test]
    async fn a_pending_ask_scrubs_a_registered_key_and_a_pattern_secret() {
        let registered = "kgraf-ask-desk-registered-Hs3Kd8Pq1Vy6Zm0Rc";
        kgraf_secrets::register_exact_secret("test-ask-secret", registered);
        let pattern = format!("ghp_{}", "a".repeat(36));
        let paused: PausedCall = serde_json::from_value(serde_json::json!({
            "call": {
                "call_id": "c1",
                "tool": "shell",
                "input": {"command": ["curl", "-H", format!("token {pattern}"), registered]},
                "worker": "w1",
                "task": "t1"
            },
            "asked_by": "shell",
            "question": format!("run `curl -H \"token {pattern}\" {registered}`?"),
            "next_hook": 1,
            "rewritten": false,
            "deadline_ms": null,
            "chain_digest": "d",
            "seal": "s"
        }))
        .expect("paused call");

        let (desk, _woken) = desk();
        let d = desk.clone();
        let worker = tokio::spawn(async move { AskResolver::answer(&*d, &paused).await });
        let ask = until_pending(&desk).await;

        assert!(!ask.question.contains(registered), "{}", ask.question);
        assert!(!ask.question.contains(&pattern), "{}", ask.question);
        assert!(ask.question.contains("[redacted:test-ask-secret;"));
        assert!(ask.question.contains("[redacted:github-pat;"));
        assert!(!ask.input.contains(registered), "{}", ask.input);
        assert!(!ask.input.contains(&pattern), "{}", ask.input);
        assert!(ask.input.contains("[redacted:test-ask-secret;"));
        assert!(ask.input.contains("[redacted:github-pat;"));

        desk.answer(ask.id, AskAnswer::AllowOnce).expect("answer");
        worker.await.expect("join");
    }

    #[tokio::test]
    async fn an_ask_waits_for_the_user_and_allow_once_resumes_the_worker() {
        let (desk, woken) = desk();
        let d = desk.clone();
        let worker =
            tokio::spawn(async move { AskResolver::answer(&*d, &paused("shell", "w1")).await });
        let ask = until_pending(&desk).await;
        assert_eq!(ask.tool, "shell");
        assert_eq!(ask.worker, "w1");
        assert_eq!(ask.task, "t1");
        assert_eq!(ask.question, "run `git push`?");
        assert!(ask.input.contains("\"push\""), "{}", ask.input);
        assert!(
            woken.load(Ordering::SeqCst) >= 1,
            "the box is woken for the ask"
        );
        assert!(!worker.is_finished(), "the worker waits for the answer");
        desk.answer(ask.id, AskAnswer::AllowOnce).expect("answer");
        assert_eq!(worker.await.expect("join"), UserAnswer::Allow);
        assert!(desk.pending().is_empty());
        assert_eq!(
            desk.answer(ask.id, AskAnswer::AllowOnce),
            Err(AskError::Unknown(ask.id)),
            "an ask is answered once"
        );
    }

    #[tokio::test]
    async fn deny_reaches_the_worker_with_the_users_reason() {
        let (desk, _) = desk();
        let d = desk.clone();
        let worker =
            tokio::spawn(async move { AskResolver::answer(&*d, &paused("shell", "w1")).await });
        let ask = until_pending(&desk).await;
        desk.answer(
            ask.id,
            AskAnswer::Deny {
                reason: "not now".into(),
            },
        )
        .expect("answer");
        assert_eq!(
            worker.await.expect("join"),
            UserAnswer::Deny {
                reason: "not now".into()
            }
        );
    }

    #[tokio::test]
    async fn allow_always_without_a_rule_store_allows_once_and_says_so() {
        let (desk, _) = desk();
        let d = desk.clone();
        let worker =
            tokio::spawn(async move { AskResolver::answer(&*d, &paused("shell", "w1")).await });
        let ask = until_pending(&desk).await;
        desk.answer(ask.id, AskAnswer::AllowAlways).expect("answer");
        assert_eq!(worker.await.expect("join"), UserAnswer::Allow);
        let notices = desk.take_notices();
        assert!(
            matches!(&notices[..], [DeskNotice::RuleNotRecorded { tool, .. }] if tool == "shell"),
            "{notices:?}"
        );
    }

    #[tokio::test]
    async fn an_aborted_worker_leaves_no_ghost_ask_and_its_answer_is_refused() {
        let (desk, _) = desk();
        let d = desk.clone();
        let worker =
            tokio::spawn(async move { AskResolver::answer(&*d, &paused("shell", "w1")).await });
        let ask = until_pending(&desk).await;
        worker.abort();
        let _ = worker.await;
        assert!(desk.pending().is_empty(), "the ask left with its worker");
        assert_eq!(
            desk.answer(ask.id, AskAnswer::AllowAlways),
            Err(AskError::Unknown(ask.id))
        );
        assert!(
            desk.take_notices().is_empty(),
            "no rule notice: nothing was recorded for a gone worker"
        );
    }

    #[tokio::test]
    async fn an_unanswered_ask_is_denied_at_the_timeout() {
        let (desk, _) = desk();
        let desk = Arc::new(
            Arc::try_unwrap(desk)
                .expect("sole owner")
                .with_timeout(std::time::Duration::from_millis(50)),
        );
        let answer = AskResolver::answer(&*desk, &paused("shell", "w1")).await;
        assert!(
            matches!(&answer, UserAnswer::Deny { reason } if reason.contains("no answer")),
            "{answer:?}"
        );
        assert!(desk.pending().is_empty());
        assert!(
            desk.take_notices()
                .iter()
                .any(|n| matches!(n, DeskNotice::TimedOut { .. }))
        );
        assert_eq!(DEFAULT_ASK_TIMEOUT, std::time::Duration::from_secs(300));
    }

    #[tokio::test]
    async fn closing_the_desk_denies_open_and_later_asks() {
        let (desk, _) = desk();
        let d = desk.clone();
        let worker =
            tokio::spawn(async move { AskResolver::answer(&*d, &paused("shell", "w1")).await });
        until_pending(&desk).await;
        desk.close();
        assert!(matches!(
            worker.await.expect("join"),
            UserAnswer::Deny { .. }
        ));
        let later = AskResolver::answer(&*desk, &paused("shell", "w2")).await;
        assert!(matches!(later, UserAnswer::Deny { .. }));
        assert!(
            desk.take_notices()
                .iter()
                .any(|n| matches!(n, DeskNotice::DeniedOnClose { worker, .. } if worker == "w1"))
        );
    }
}
