//! One orchestrator run hosted in the caller's process and driven from a thread
//! that is not an async task: the desktop shell's UI thread.
//!
//! It is glue over existing seams, nothing new in kind:
//! - the run is built by an [`OrchestratorFactory`] (the same seam scheduled runs
//!   use, so every run of the process shares one [`crate::Admission`]);
//! - the box talks to it through a [`channel_port`] (the message port, as the CLI's
//!   stdin channel does);
//! - `/compact`, the user's order and stop go through the [`OrchestratorHandle`].
//!
//! Every call returns at once. Results that need the loop (a compact refusal, an
//! order accepted, the run's end) arrive later as [`RunEvent`]s from
//! [`InteractiveRun::poll`], and each arrival calls `wake`, so a UI that repaints only
//! on change still sees them.
//!
//! State machine:
//! ```text
//!   start/resume --build ok--> Running --report--> Finished(report)
//!   start/resume --build or readiness fails--> error returned (no run)
//!   Running --stop--> Running (workers cancel) --report--> Finished(Stopped)
//! ```

use crate::asks::PendingQuestionView;
use crate::box_port::{PortClient, Wake, channel_port};
use crate::orchestrator::{CompactError, OrchestratorHandle, OrderError, RunReport, SessionError};
use crate::port::{InboundKind, OutboundReply, PortError};
use crate::starter::OrchestratorFactory;
use kgraf_core::AgentId;
use kgraf_core::RunId;
use kgraf_core::session::{InboxKind, InjectMode};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

/// What arrived from the run since the last poll.
#[derive(Clone, Debug, PartialEq)]
pub enum RunEvent {
    /// The orchestrator answered a message the box sent.
    Reply(OutboundReply),
    /// The answer to a `/compact`.
    Compacted {
        target: Option<String>,
        result: Result<(), CompactError>,
    },
    /// The answer to a user order.
    Ordered(Result<(), OrderError>),
    /// The run ended: its report, or the error that ended it.
    Finished(Result<Box<RunReport>, String>),
    /// Decision 0067, point 2: the answer to `send_to_session` (the persisted
    /// message's id, or the refusal).
    SessionSent {
        session: String,
        result: Result<String, SessionError>,
    },
    /// The answer to `stop_session`.
    SessionStopped {
        session: String,
        result: Result<(), SessionError>,
    },
    /// The answer to `resume_session`.
    SessionResumed {
        session: String,
        result: Result<(), SessionError>,
    },
    /// The answer to `answer_question` (decision 0067, point 4).
    QuestionAnswered {
        question: u64,
        result: Result<(), SessionError>,
    },
    /// Something about a session the user must see (decision 0067):
    /// a note that was not delivered before its session ended, a question
    /// that skipped the orchestrator or had no answer in time.
    SessionNotice { text: String },
}

/// Why a run could not be started.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("the routes are not ready: {0}")]
    NotReady(String),
    #[error(transparent)]
    Build(#[from] crate::orchestrator::OrchestratorError),
}

/// A run the caller's process hosts.
pub struct InteractiveRun {
    run: RunId,
    client: PortClient,
    handle: OrchestratorHandle,
    runtime: tokio::runtime::Handle,
    events_tx: Sender<RunEvent>,
    events: Receiver<RunEvent>,
    wake: Wake,
    finished: bool,
}

impl std::fmt::Debug for InteractiveRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractiveRun")
            .field("run", &self.run)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

/// How the run begins.
enum Begin {
    Start(String),
    Resume,
}

impl InteractiveRun {
    /// Plan and run `instruction` as `run`, on `runtime`. The factory's
    /// whole-process readiness (`configured`) is checked first.
    pub async fn start(
        runtime: tokio::runtime::Handle,
        factory: Arc<dyn OrchestratorFactory>,
        run: RunId,
        instruction: String,
        wake: Wake,
    ) -> Result<Self, HostError> {
        Self::begin(runtime, factory, run, Begin::Start(instruction), wake).await
    }

    /// Resume `run` from its record.
    pub async fn resume(
        runtime: tokio::runtime::Handle,
        factory: Arc<dyn OrchestratorFactory>,
        run: RunId,
        wake: Wake,
    ) -> Result<Self, HostError> {
        Self::begin(runtime, factory, run, Begin::Resume, wake).await
    }

    async fn begin(
        runtime: tokio::runtime::Handle,
        factory: Arc<dyn OrchestratorFactory>,
        run: RunId,
        begin: Begin,
        wake: Wake,
    ) -> Result<Self, HostError> {
        // Whole-process readiness (routes configured, credential plumbing). An
        // interactive run belongs to no schedule, so the per-schedule
        // `ready(schedule_id)` has nothing to check here.
        factory
            .configured()
            .await
            .map_err(|e| HostError::NotReady(e.to_string()))?;
        // The per-run facts (credentials) the factory checks for a schedule; a box
        // run has no schedule id, and its run id serves as the key.
        factory
            .ready(run.as_str())
            .await
            .map_err(|e| HostError::NotReady(e.to_string()))?;
        let (events_tx, events) = channel();
        // A box run is attended: a worker's question waits for the user as an
        // approval ask in the box does, and session notices reach the box.
        let notices = events_tx.clone();
        let notices_wake = wake.clone();
        let orchestrator = factory
            .build(&run)?
            .with_question_attendance(crate::asks::QuestionAttendance::Box)
            .with_session_notices(Arc::new(move |text: String| {
                let _ = notices.send(RunEvent::SessionNotice { text });
                notices_wake();
            }));
        let (port, client) = channel_port("box", wake.clone());
        let handle = orchestrator.handle();
        // attach_port spawns its reader on the current runtime; make that `runtime`
        // whichever runtime awaited this. No await follows, so the guard is safe.
        let _guard = runtime.enter();
        orchestrator.attach_port(port);
        let tx = events_tx.clone();
        let w = wake.clone();
        let id = run.clone();
        runtime.spawn(async move {
            let result = match begin {
                Begin::Start(instruction) => orchestrator.start(id, instruction).await,
                Begin::Resume => orchestrator.resume(id).await,
            };
            let _ = tx.send(RunEvent::Finished(
                result.map(Box::new).map_err(|e| e.to_string()),
            ));
            w();
        });
        Ok(Self {
            run,
            client,
            handle,
            runtime,
            events_tx,
            events,
            wake,
            finished: false,
        })
    }

    pub fn run(&self) -> &RunId {
        &self.run
    }

    /// Whether the run has ended (after its [`RunEvent::Finished`] was polled).
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Send a note or a status question through the port. Returns the message id
    /// the reply will name.
    pub fn send(
        &mut self,
        kind: InboundKind,
        text: &str,
        now_ms: i64,
    ) -> Result<String, PortError> {
        self.client.send(kind, text, now_ms)
    }

    /// Run `call` against the handle off the caller's thread, then post
    /// `event(result)` and wake the UI.
    fn spawn_reply<T, Fut>(
        &self,
        call: impl FnOnce(OrchestratorHandle) -> Fut + Send + 'static,
        event: impl FnOnce(T) -> RunEvent + Send + 'static,
    ) where
        T: Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
    {
        let handle = self.handle.clone();
        let tx = self.events_tx.clone();
        let wake = self.wake.clone();
        self.runtime.spawn(async move {
            let result = call(handle).await;
            let _ = tx.send(event(result));
            wake();
        });
    }

    /// `/compact`, answered later as [`RunEvent::Compacted`].
    pub fn compact(&self, target: Option<String>) {
        let t = target.clone();
        self.spawn_reply(
            move |h| async move { h.compact(t).await },
            move |result| RunEvent::Compacted { target, result },
        );
    }

    /// The user's order (0014), answered later as [`RunEvent::Ordered`].
    pub fn set_order(&self, contracts: Vec<String>) {
        self.spawn_reply(
            move |h| async move { h.set_order(contracts).await },
            RunEvent::Ordered,
        );
    }

    /// Send a note, redirect, answer or message to one session directly
    /// (decision 0067): the exception route beside the orchestrator box.
    /// Answered later as [`RunEvent::SessionSent`].
    pub fn send_to_session(
        &self,
        session: AgentId,
        kind: InboxKind,
        mode: InjectMode,
        text: String,
    ) {
        let name = session.to_string();
        self.spawn_reply(
            move |h| async move { h.send_to_session(session, kind, mode, text).await },
            move |result| RunEvent::SessionSent {
                session: name,
                result,
            },
        );
    }

    /// Stop one session's attempt directly. Answered later as
    /// [`RunEvent::SessionStopped`].
    pub fn stop_session(&self, session: AgentId) {
        let name = session.to_string();
        self.spawn_reply(
            move |h| async move { h.stop_session(session).await },
            move |result| RunEvent::SessionStopped {
                session: name,
                result,
            },
        );
    }

    /// Resume a session directly. Answered later as [`RunEvent::SessionResumed`].
    pub fn resume_session(&self, session: AgentId) {
        let name = session.to_string();
        self.spawn_reply(
            move |h| async move { h.resume_session(session).await },
            move |result| RunEvent::SessionResumed {
                session: name,
                result,
            },
        );
    }

    /// Answer a worker's `ask_user_question` (decision 0067).
    /// Answered later as [`RunEvent::QuestionAnswered`].
    pub fn answer_question(&self, question: u64, answer: String) {
        self.spawn_reply(
            move |h| async move { h.answer_question(question, answer).await },
            move |result| RunEvent::QuestionAnswered { question, result },
        );
    }

    /// Questions waiting for the user right now (for the box's pinned
    /// cards). Blocks the calling thread on the loop's own answer, the same
    /// way `stop_and_wait` does; call from a thread that is not a runtime
    /// worker.
    pub fn pending_questions(&self) -> Vec<PendingQuestionView> {
        let handle = self.handle.clone();
        self.runtime
            .block_on(async move { handle.pending_questions().await })
    }

    /// Stop the run: its workers are cancelled and its record stays resumable.
    pub fn stop(&self) {
        let handle = self.handle.clone();
        self.runtime.spawn(async move { handle.stop().await });
    }

    /// Stops the run and waits, at most `timeout`, for it to end: its workers are
    /// cancelled and its record is saved as stopped, so it resumes later. For a
    /// window that is closing. Call from a thread that is not a runtime
    /// worker. Returns whether the run ended in time.
    pub fn stop_and_wait(&mut self, timeout: std::time::Duration) -> bool {
        if self.finished {
            return true;
        }
        let deadline = std::time::Instant::now() + timeout;
        let handle = self.handle.clone();
        // The timer is made inside the runtime: block_on drives the future on
        // this thread, but a timer needs the runtime's context to exist.
        let _ = self
            .runtime
            .block_on(async move { tokio::time::timeout(timeout, handle.stop()).await });
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.events.recv_timeout(left) {
                Ok(RunEvent::Finished(_)) => {
                    self.finished = true;
                    return true;
                }
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    }

    /// Everything that arrived since the last poll, replies first.
    pub fn poll(&mut self) -> Vec<RunEvent> {
        let mut out: Vec<RunEvent> = self
            .client
            .drain_replies()
            .into_iter()
            .map(RunEvent::Reply)
            .collect();
        out.extend(self.events.try_iter());
        if out.iter().any(|e| matches!(e, RunEvent::Finished(_))) {
            self.finished = true;
        }
        out
    }
}
