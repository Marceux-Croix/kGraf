//! The orchestrator box one always-present input to the orchestrator,
//! with its replies, pending tool approvals and slash commands inline.
//!
//! It speaks the message port (a line is a note; `/status` is a status question),
//! the same port the CLI's `--interactive` stdin channel speaks, through the
//! orchestrator crate's [`kgraf_orchestrator::channel_port`]. There is no second
//! protocol. Approvals come from the orchestrator crate's
//! [`kgraf_orchestrator::AskDesk`] and are answered in place: allow once, allow
//! always, deny.
//!
//! Commands:
//! - text: a note for the run; with no run in this window and a host that can start
//!   one, the text is the run's instruction.
//! - `/status`: a status question. With no run in this window it is answered from
//!   the newest run record in the project store, and the answer says so.
//! - `/compact`: compact the orchestrator (0057 addendum). `/compact selected`
//!   targets the worker selected on the graph; `/compact <worker>` names one.
//! - `/help`: the list. Anything else starting with `/` is refused, never sent as a
//!   note. `//text` sends `/text` as a note.
//!
//! The model ([`BoxModel`]) is pure over a [`BoxBackend`], so its transitions are
//! tested without a window; [`crate::orchestrator_host`] is the real backend.

use kgraf_orchestrator::{AskAnswer, InboundKind, PendingAsk, UntrustedText};

use crate::session_panel::AttachmentChip;

/// One parsed line of input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoxCommand {
    Empty,
    Note(String),
    Status,
    Compact(CompactTarget),
    Help,
    /// `/run <instruction>`: start a run, explicitly (it costs money).
    Run(String),
    Unknown(String),
}

/// What `/compact` targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactTarget {
    Orchestrator,
    /// The worker selected on the graph.
    Selected,
    Worker(String),
}

pub const HELP: &str = "Type a note for the running run. /run <instruction> starts a run \
(a paid one: it is never started by a plain line). /status asks where the run stands. \
/compact compacts the orchestrator; /compact selected or /compact <worker> targets a \
worker. //text sends /text as a note.";

/// Parse one line of input.
pub fn parse(input: &str) -> BoxCommand {
    let line = input.trim();
    if line.is_empty() {
        return BoxCommand::Empty;
    }
    if let Some(rest) = line.strip_prefix("//") {
        return BoxCommand::Note(format!("/{rest}"));
    }
    let Some(command) = line.strip_prefix('/') else {
        return BoxCommand::Note(line.to_owned());
    };
    if let Some(instruction) = command
        .strip_prefix("run")
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        let instruction = instruction.trim();
        return if instruction.is_empty() {
            BoxCommand::Unknown(line.to_owned())
        } else {
            BoxCommand::Run(instruction.to_owned())
        };
    }
    let mut words = command.split_whitespace();
    let name = words.next().unwrap_or("");
    let args: Vec<&str> = words.collect();
    match (name, args.as_slice()) {
        ("status", []) => BoxCommand::Status,
        ("help", []) => BoxCommand::Help,
        ("compact", []) => BoxCommand::Compact(CompactTarget::Orchestrator),
        ("compact", ["selected"]) => BoxCommand::Compact(CompactTarget::Selected),
        ("compact", [worker]) => BoxCommand::Compact(CompactTarget::Worker((*worker).to_owned())),
        _ => BoxCommand::Unknown(line.to_owned()),
    }
}

/// Where the backend stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendState {
    /// No run in this window. `can_start` when a line would start one.
    Idle {
        can_start: bool,
        reason: String,
    },
    /// A run is being started (readiness, build).
    Starting,
    Running {
        run: String,
    },
}

/// What arrived from the backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendEvent {
    Reply { in_reply_to: String, text: String },
    Notice(String),
    Error(String),
}

/// What the box talks to.
pub trait BoxBackend {
    fn state(&self) -> BackendState;
    /// A note or status question to the running run. Returns the message id.
    fn send(&mut self, kind: InboundKind, text: &str) -> Result<String, String>;
    /// Start a run with `instruction`. Only valid when idle and able to start.
    fn start(&mut self, instruction: &str) -> Result<(), String>;
    /// `/compact`; the answer arrives as an event.
    fn compact(&mut self, target: Option<String>) -> Result<(), String>;
    fn pending_asks(&self) -> Vec<PendingAsk>;
    fn answer(&mut self, id: u64, answer: AskAnswer) -> Result<(), String>;
    fn poll(&mut self) -> Vec<BackendEvent>;
    /// With no run here: where the newest stored run stands, from its record.
    fn status_from_record(&self) -> Option<String>;

    /// Worker questions pending an answer (design 0067, point 4:
    /// `ask_user_question`). Empty by default; `OrchestratorHost` reads real
    /// ones from `InteractiveRun::pending_questions` while a run is hosted.
    fn pending_questions(&self) -> Vec<PendingQuestion> {
        Vec::new()
    }
    /// Answers a pending question (`InteractiveRun::answer_question`, fire
    /// and forget: the real result arrives later as a `BackendEvent` from
    /// `poll`). Refused by default when no backend is reachable.
    fn answer_question(&mut self, _question: u64, _answer: String) -> Result<(), String> {
        Err("no run in this window".to_owned())
    }

    /// Sends a note, a redirect, or a plain message to one session directly
    /// (design 0067, point 2), through `InteractiveRun::send_to_session`
    /// (fire and forget: the real result arrives later as a `BackendEvent`).
    /// Refused by default when no backend is reachable.
    fn send_to_session(
        &mut self,
        _session: kgraf_core::AgentId,
        _kind: kgraf_core::session::InboxKind,
        _mode: kgraf_core::session::InjectMode,
        _text: String,
    ) -> Result<(), String> {
        Err("no run in this window".to_owned())
    }
    /// Stops one session (`InteractiveRun::stop_session`). See
    /// `send_to_session`.
    fn stop_session(&mut self, _session: kgraf_core::AgentId) -> Result<(), String> {
        Err("no run in this window".to_owned())
    }
    /// Resumes one stopped session (`InteractiveRun::resume_session`). See
    /// `send_to_session`.
    fn resume_session(&mut self, _session: kgraf_core::AgentId) -> Result<(), String> {
        Err("no run in this window".to_owned())
    }

    /// Routes a question that arrived with no run in
    /// this window (a plain line typed here has "no roadmap home" yet). Fire
    /// and forget: the routing runs in order (a roadmap feature node first, an
    /// answer of its own second, an offer to start a new project third) and
    /// the result arrives later as a `BackendEvent` or a `pending_offers`
    /// entry. Refused by default when no project is open to route against.
    fn route_question(&mut self, _text: &str) -> Result<(), String> {
        Err("no project is open to route this question against".to_owned())
    }

    /// Offers waiting on the user: "this looks like
    /// a large, off-topic question; start a new project for it instead?".
    /// Empty by default, like `pending_asks`.
    fn pending_offers(&self) -> Vec<QuestionOffer> {
        Vec::new()
    }

    /// Accepts an offer: a new project, seeded with the question, is started
    /// through the same path "Add project" uses (`Workspace`, one level up
    /// from a single project's backend); this only clears the offer here.
    fn accept_question_offer(&mut self, _id: u64) -> Result<(), String> {
        Err("no such offer".to_owned())
    }

    /// Declines an offer: the question is filed
    /// under the standalone "Questions" node (created once, reused after) and
    /// an agent is tasked there, through the same dispatch path as step 1.
    fn decline_question_offer(&mut self, _id: u64) -> Result<(), String> {
        Err("no such offer".to_owned())
    }
}

/// One question-routing offer pending an answer, as
/// a pinned card in the orchestrator box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionOffer {
    pub id: u64,
    pub question: String,
    /// The offer's own item id on the graph (`record_offer_made`), when the
    /// write succeeded: carried onward so accepting it can record the
    /// resolution on the same item once the new project's name is known
    /// (fix, 2026-09-28).
    pub graph_id: Option<String>,
}

/// One worker question pending an answer (design 0067, point 4), as a pinned
/// card in the orchestrator box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingQuestion {
    pub id: u64,
    pub worker: String,
    pub question: String,
}

/// One line of the box's history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    You(String),
    /// The orchestrator's reply, or an answer read from the run record.
    Orchestrator(String),
    Notice(String),
    Error(String),
}

/// Lines kept. A box is not a log; the run record and the transcripts keep
/// everything (0028). Older lines leave the box, not the record.
pub const MAX_LINES: usize = 200;

/// The box's state.
#[derive(Debug, Default)]
pub struct BoxModel {
    pub input: String,
    lines: Vec<Line>,
    /// Message ids sent and not yet answered.
    awaiting: Vec<String>,
    /// The user tried to send an empty line; the field shows the error cue until
    /// they type.
    pub empty_send: bool,
    /// A pending approval's chosen answer, waiting for Confirm (CMP-CARD-3).
    pub confirming: Option<(u64, AskAnswer)>,
    /// "Attach as context" chips, shown before
    /// sending and removable; consumed exactly once, into the next note or
    /// run instruction actually sent (`compose_with_attachments`), never
    /// resent on the message after.
    attachments: Vec<AttachmentChip>,
    /// A question card's in-progress answer text, by question id (design
    /// 0067, point 4).
    question_answers: std::collections::BTreeMap<u64, String>,
    /// An accepted "start a new project" offer's question and its graph item
    /// id, waiting for `Workspace::tick` to pick it
    /// up once (the same "Add project" path, seeded, and the origin project's
    /// graph record resolved once the new project's name is known). `decline_question_offer` never
    /// sets this: it dispatches in this project instead (step 4).
    pending_new_project: Option<(String, Option<String>)>,
    /// An offer card's chosen answer (`true` accept, `false` decline), waiting
    /// on its own CMP-CARD-3 confirm step. Kept apart from `confirming`
    /// (approval cards): offer ids and ask ids are assigned by unrelated
    /// counters, so the same id number could otherwise mean two different
    /// pending choices.
    confirming_offer: Option<(u64, bool)>,
}

impl BoxModel {
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// Messages sent and not yet answered.
    pub fn awaiting(&self) -> usize {
        self.awaiting.len()
    }

    /// Add a line from outside the input (play, the user's order).
    pub fn say(&mut self, line: Line) {
        self.push(line);
    }

    /// The pending "Attach as context" chips, for the composer to draw.
    pub fn attachments(&self) -> &[AttachmentChip] {
        &self.attachments
    }

    /// A session view's "Attach as context" control adds a chip here. Does not send anything by
    /// itself.
    pub fn attach(&mut self, chip: AttachmentChip) {
        self.attachments.push(chip);
    }

    /// Removes one pending chip before it is sent.
    pub fn remove_attachment(&mut self, index: usize) {
        if index < self.attachments.len() {
            self.attachments.remove(index);
        }
    }

    /// Turns pending attachment chips into quoted data blocks ahead of `text`,
    /// and consumes them. Each agent's output is wrapped in
    /// [`UntrustedText::quote_as_data`] (already used the same way for every
    /// other inbound text this project holds as data, `envelope.rs`,
    /// `checks.rs`): the orchestrator receives it marked as data, never as an
    /// instruction, with no orchestrator-crate change needed). With no chips, `text` is
    /// returned unchanged, so a plain note pays nothing extra.
    fn compose_with_attachments(&mut self, text: &str) -> String {
        if self.attachments.is_empty() {
            return text.to_owned();
        }
        let mut out = String::new();
        for chip in self.attachments.drain(..) {
            let body = chip
                .selection
                .as_deref()
                .unwrap_or("(no text was selected; the whole session's output)");
            out.push_str(
                &UntrustedText::new(body)
                    .quote_as_data("agent_output", &[("agent", chip.agent.as_str())]),
            );
            out.push('\n');
        }
        out.push_str(text);
        out
    }

    fn push(&mut self, line: Line) {
        self.lines.push(line);
        if self.lines.len() > MAX_LINES {
            let over = self.lines.len() - MAX_LINES;
            self.lines.drain(..over);
        }
    }

    /// Act on the current input and clear it. `selected_worker` resolves
    /// `/compact selected`.
    pub fn submit(&mut self, backend: &mut dyn BoxBackend, selected_worker: Option<&str>) {
        let input = std::mem::take(&mut self.input);
        let command = parse(&input);
        if command != BoxCommand::Empty {
            self.push(Line::You(input.trim().to_owned()));
        }
        let state = backend.state();
        match command {
            BoxCommand::Empty => {}
            BoxCommand::Help => self.push(Line::Notice(HELP.to_owned())),
            BoxCommand::Unknown(line) => self.push(Line::Error(format!(
                "`{line}` is not a command, and was not sent. /help lists them."
            ))),
            BoxCommand::Note(text) => match state {
                BackendState::Running { .. } => {
                    let composed = self.compose_with_attachments(&text);
                    self.send(backend, InboundKind::Note, &composed)
                }
                // A plain line still never blindly starts a paid run as its own
                // instruction; with no run in this window it is routed
                // instead: a roadmap feature node
                // first, the orchestrator's own answer second, an offer to
                // start a new project third. `/run` remains the one explicit
                // way to start a run with arbitrary text as its instruction.
                BackendState::Idle {
                    can_start: true, ..
                } => {
                    let composed = self.compose_with_attachments(&text);
                    match backend.route_question(&composed) {
                        Ok(()) => self.push(Line::Notice(
                            "routing this question: a roadmap feature node first, else the \
                             orchestrator answers it, else an offer to start a new project"
                                .to_owned(),
                        )),
                        Err(e) => self.push(Line::Error(format!("not routed: {e}"))),
                    }
                }
                BackendState::Idle { reason, .. } => {
                    self.push(Line::Error(format!("not sent: {reason}")))
                }
                BackendState::Starting => self.push(Line::Error(
                    "not sent: the run is still starting; send it again once it runs".to_owned(),
                )),
            },
            BoxCommand::Run(instruction) => match state {
                BackendState::Idle {
                    can_start: true, ..
                } => {
                    let composed = self.compose_with_attachments(&instruction);
                    match backend.start(&composed) {
                        Ok(()) => self.push(Line::Notice(format!(
                            "Starting a run with this instruction: {instruction}"
                        ))),
                        Err(e) => self.push(Line::Error(format!("the run did not start: {e}"))),
                    }
                }
                BackendState::Idle { reason, .. } => {
                    self.push(Line::Error(format!("not started: {reason}")))
                }
                BackendState::Starting | BackendState::Running { .. } => self.push(Line::Error(
                    "not started: a run is already in this window".to_owned(),
                )),
            },
            BoxCommand::Status => match state {
                BackendState::Running { .. } => {
                    self.send(backend, InboundKind::StatusQuestion, "status")
                }
                _ => match backend.status_from_record() {
                    Some(line) => self.push(Line::Orchestrator(format!(
                        "{line} (read from the run record; no orchestrator runs in this window)"
                    ))),
                    None => self.push(Line::Error(
                        "no run: nothing runs in this window and the project store holds no run record"
                            .to_owned(),
                    )),
                },
            },
            BoxCommand::Compact(target) => {
                let target = match target {
                    CompactTarget::Orchestrator => Ok(None),
                    CompactTarget::Worker(w) => Ok(Some(w)),
                    CompactTarget::Selected => selected_worker
                        .map(|w| Some(w.to_owned()))
                        .ok_or_else(|| "select a worker on the graph first".to_owned()),
                };
                let sent = match (target, state) {
                    (Err(e), _) => Err(e),
                    (Ok(t), BackendState::Running { .. }) => backend.compact(t),
                    (Ok(_), _) => Err("no run in this window to compact".to_owned()),
                };
                if let Err(e) = sent {
                    self.push(Line::Error(format!("/compact not sent: {e}")));
                }
            }
        }
    }

    fn send(&mut self, backend: &mut dyn BoxBackend, kind: InboundKind, text: &str) {
        match backend.send(kind, text) {
            Ok(id) => self.awaiting.push(id),
            Err(e) => self.push(Line::Error(format!("not delivered: {e}"))),
        }
    }

    /// Takes an accepted new-project offer, once:
    /// `Workspace::tick` calls this on every open project each frame and, for
    /// the one that answers `Some` (the question, and the offer's graph item
    /// id if it has one), opens a new project through the same "Add project"
    /// path, seeded with the question, and records the resolution on the
    /// origin project's graph once the new project's name is known.
    pub fn take_new_project_request(&mut self) -> Option<(String, Option<String>)> {
        self.pending_new_project.take()
    }

    /// Resolves a pending offer: accepting queues
    /// the new-project request for `Workspace::tick`; declining dispatches an
    /// agent under the standalone "Questions" node, in this project.
    pub fn resolve_offer(
        &mut self,
        backend: &mut dyn BoxBackend,
        offer: &QuestionOffer,
        accept: bool,
    ) {
        if accept {
            let _ = backend.accept_question_offer(offer.id);
            self.pending_new_project = Some((offer.question.clone(), offer.graph_id.clone()));
            self.push(Line::Notice(
                "starting a new project, seeded with this question".to_owned(),
            ));
        } else {
            match backend.decline_question_offer(offer.id) {
                Ok(()) => self.push(Line::Notice(
                    "filed under Questions; an agent is tasked there".to_owned(),
                )),
                Err(e) => self.push(Line::Error(format!("not filed: {e}"))),
            }
        }
    }

    /// Answer a pending approval.
    pub fn answer(&mut self, backend: &mut dyn BoxBackend, ask: &PendingAsk, answer: AskAnswer) {
        let what = match &answer {
            AskAnswer::AllowOnce => "allowed once",
            AskAnswer::AllowAlways => "allowed always",
            AskAnswer::Deny { .. } => "denied",
        };
        match backend.answer(ask.id, answer) {
            Ok(()) => self.push(Line::Notice(format!(
                "{} for {} ({}): {what}",
                ask.tool, ask.worker, ask.question
            ))),
            Err(e) => self.push(Line::Error(format!("the answer was not taken: {e}"))),
        }
    }

    /// Take what the backend has for the box.
    pub fn absorb(&mut self, backend: &mut dyn BoxBackend) -> bool {
        let events = backend.poll();
        let any = !events.is_empty();
        for e in events {
            match e {
                BackendEvent::Reply { in_reply_to, text } => {
                    self.awaiting.retain(|id| id != &in_reply_to);
                    self.push(Line::Orchestrator(text));
                }
                BackendEvent::Notice(t) => self.push(Line::Notice(t)),
                BackendEvent::Error(t) => self.push(Line::Error(t)),
            }
        }
        any
    }
}

/// The allow-always button's words: for an `mcp__` tool the rule covers every
/// call to the tool (an any-call rule), and the button says so (D4).
pub fn always_label(tool: &str) -> String {
    if tool == crate::dependency_prompt::FETCH_PROMPT_TOOL {
        "Always for this project".to_owned()
    } else if tool.starts_with("mcp__") {
        format!("Allow every call to {tool}")
    } else {
        "Allow always".to_owned()
    }
}

/// The deny button's words: "Not now" for the dependency question, which
/// stores nothing (decision 0087).
pub fn deny_label(tool: &str) -> &'static str {
    if tool == crate::dependency_prompt::FETCH_PROMPT_TOOL {
        "Not now"
    } else {
        "Deny"
    }
}

/// The one-line consequence the confirm step states (CMP-CARD-3).
pub fn consequence(tool: &str, answer: &AskAnswer) -> String {
    if tool == crate::dependency_prompt::FETCH_PROMPT_TOOL {
        return match answer {
            AskAnswer::AllowOnce => {
                "kGraf runs cargo fetch for this run only; the next run asks again.".to_owned()
            }
            AskAnswer::AllowAlways => "Saves the answer: kGraf fetches this project's \
                                       dependencies whenever a run needs them."
                .to_owned(),
            AskAnswer::Deny { .. } => {
                "Nothing is fetched and the run does not start; you will be asked again.".to_owned()
            }
        };
    }
    match answer {
        AskAnswer::AllowOnce => format!("{tool} runs this once; the next call asks again."),
        AskAnswer::AllowAlways if tool.starts_with("mcp__") => {
            format!("Saves a rule: every call to {tool} runs without asking, from now on.")
        }
        AskAnswer::AllowAlways => {
            format!("Saves a rule: this {tool} call runs without asking, from now on.")
        }
        AskAnswer::Deny { .. } => {
            format!("The worker's {tool} call is refused, and it is told so.")
        }
    }
}

/// History height before it scrolls, in points: the galaxy keeps the space.
pub const HISTORY_HEIGHT: f32 = 132.0;

/// The words and the indicator for the backend's state (neutral unless it needs
/// the user: the semantic color rule).
fn state_label(state: &BackendState) -> String {
    match state {
        BackendState::Running { run } => format!("run {run}"),
        BackendState::Starting => "starting".to_owned(),
        BackendState::Idle {
            can_start: true, ..
        } => "no run".to_owned(),
        BackendState::Idle { .. } => "not hosting".to_owned(),
    }
}

/// What the window passes to the box besides its model and backend.
#[derive(Debug, Clone, Default)]
pub struct BoxOptions {
    /// The token-attribution line under the composer.
    pub attribution: String,
    /// The pending ask to flash (the bell's jump), if any.
    pub flash_ask: Option<u64>,
    /// Give the composer keyboard focus this frame.
    pub focus_input: bool,
}

/// The box with default options (tests and headless use).
pub fn show(
    ui: &mut egui::Ui,
    model: &mut BoxModel,
    backend: &mut dyn BoxBackend,
    selected_worker: Option<&str>,
) {
    show_styled(ui, model, backend, selected_worker, &BoxOptions::default());
}

/// A pinned card: a pending action card (CMP-CARD-1), for one pending approval. Answered in
/// place by its buttons. Returns the card's rect.
fn approval_card(
    ui: &mut egui::Ui,
    model: &mut BoxModel,
    backend: &mut dyn BoxBackend,
    ask: &PendingAsk,
    flash: bool,
    p: &crate::theme::Palette,
) -> egui::Rect {
    use egui::RichText;
    let fill = if flash { p.suggested } else { p.additive };
    let stroke = if flash {
        egui::Stroke::new(2.0, p.indicator(crate::theme::Indicator::Selection))
    } else {
        egui::Stroke::NONE
    };
    egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(crate::theme::radius::CARD)
        .inner_margin(egui::Margin {
            left: 14,
            right: 14,
            top: 10,
            bottom: 12,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::hover());
                let letter = ask
                    .worker
                    .chars()
                    .find(|c| c.is_alphanumeric())
                    .unwrap_or('W');
                crate::chrome::avatar(
                    ui.painter(),
                    rect,
                    letter,
                    p,
                    Some(p.indicator(crate::theme::Indicator::NeedsInput)),
                );
                ui.label(
                    RichText::new(format!("Approval: {} for {}", ask.tool, ask.worker))
                        .font(crate::theme::medium(13.0))
                        .color(p.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("Pinned by kGraf").small().color(p.text_2));
                    ui.label(crate::icons::text(crate::icons::PIN, 14.0).color(p.text_2));
                });
            });
            ui.label(RichText::new(&ask.question).color(p.text));
            ui.collapsing(
                RichText::new(format!("arguments of ask {}", ask.id))
                    .small()
                    .color(p.text_2),
                |ui| {
                    ui.monospace(&ask.input);
                },
            );
            // CMP-CARD-3: a choice does not act at once. The row turns into a
            // confirm step that states the consequence; Confirm answers, Cancel
            // brings the choices back.
            let confirming = model
                .confirming
                .as_ref()
                .filter(|(id, _)| *id == ask.id)
                .map(|(_, a)| a.clone());
            match confirming {
                None => {
                    ui.horizontal_wrapped(|ui| {
                        // CMP-BTN-2: the recommended choice is filled, the rest tonal.
                        let choice = if crate::chrome::filled_button(ui, p, "Allow once").clicked()
                        {
                            Some(AskAnswer::AllowOnce)
                        } else if crate::chrome::tonal_button(ui, p, &always_label(&ask.tool))
                            .clicked()
                        {
                            Some(AskAnswer::AllowAlways)
                        } else if crate::chrome::tonal_button(ui, p, deny_label(&ask.tool))
                            .clicked()
                        {
                            Some(AskAnswer::Deny {
                                reason: "denied in the orchestrator box".to_owned(),
                            })
                        } else {
                            None
                        };
                        if let Some(c) = choice {
                            model.confirming = Some((ask.id, c));
                        }
                    });
                }
                Some(answer) => {
                    ui.horizontal(|ui| {
                        ui.label(crate::icons::text(crate::icons::WARNING, 16.0).color(p.danger));
                        ui.label(
                            RichText::new(consequence(&ask.tool, &answer))
                                .small()
                                .color(p.danger),
                        );
                    });
                    ui.horizontal(|ui| {
                        let confirm = crate::chrome::cta_button(ui, p, "Confirm", true);
                        if confirm.clicked() {
                            model.confirming = None;
                            model.answer(backend, ask, answer.clone());
                        }
                        if crate::chrome::text_button(ui, p.text, "Cancel").clicked() {
                            model.confirming = None;
                        }
                    });
                }
            }
        })
        .response
        .rect
}

/// A pinned card (CMP-CARD-1) for one worker question (design 0067, point 4:
/// `ask_user_question`). Shaped like `approval_card`'s header and frame, but
/// the answer is free text, not one of a fixed set of choices, so it has no
/// CMP-CARD-3 confirm step: an answer is not the kind of action that step's
/// consequence line exists to warn about.
fn question_card(
    ui: &mut egui::Ui,
    model: &mut BoxModel,
    backend: &mut dyn BoxBackend,
    q: &PendingQuestion,
    p: &crate::theme::Palette,
) -> egui::Rect {
    use egui::RichText;
    egui::Frame::new()
        .fill(p.additive)
        .corner_radius(crate::theme::radius::CARD)
        .inner_margin(egui::Margin {
            left: 14,
            right: 14,
            top: 10,
            bottom: 12,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::hover());
                let letter = q
                    .worker
                    .chars()
                    .find(|c| c.is_alphanumeric())
                    .unwrap_or('W');
                crate::chrome::avatar(
                    ui.painter(),
                    rect,
                    letter,
                    p,
                    Some(p.indicator(crate::theme::Indicator::NeedsInput)),
                );
                ui.label(
                    RichText::new(format!("Question from {}", q.worker))
                        .font(crate::theme::medium(13.0))
                        .color(p.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("Pinned by kGraf").small().color(p.text_2));
                    ui.label(crate::icons::text(crate::icons::PIN, 14.0).color(p.text_2));
                });
            });
            ui.label(RichText::new(&q.question).color(p.text));
            let answer = model.question_answers.entry(q.id).or_default();
            ui.text_edit_singleline(answer);
            let has_text = !answer.trim().is_empty();
            if crate::chrome::cta_button(ui, p, "Answer", has_text).clicked() {
                let answer = model.question_answers.remove(&q.id).unwrap_or_default();
                match backend.answer_question(q.id, answer) {
                    Ok(()) => model.push(Line::Notice(format!(
                        "answer sent to {} (question {})",
                        q.worker, q.id
                    ))),
                    Err(e) => model.push(Line::Error(format!("the answer was not taken: {e}"))),
                }
            }
        })
        .response
        .rect
}

/// A pinned card (CMP-CARD-1) offering a new project for a question that
/// turned out to be large and off topic. Shaped
/// like `approval_card`: a confirm step (CMP-CARD-3) states the consequence
/// before either choice acts, since starting a project or filing the question
/// away are both non-reversible (INT-8).
fn offer_card(
    ui: &mut egui::Ui,
    model: &mut BoxModel,
    backend: &mut dyn BoxBackend,
    offer: &QuestionOffer,
    p: &crate::theme::Palette,
) -> egui::Rect {
    use egui::RichText;
    egui::Frame::new()
        .fill(p.additive)
        .corner_radius(crate::theme::radius::CARD)
        .inner_margin(egui::Margin {
            left: 14,
            right: 14,
            top: 10,
            bottom: 12,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::hover());
                crate::chrome::avatar(
                    ui.painter(),
                    rect,
                    'Q',
                    p,
                    Some(p.indicator(crate::theme::Indicator::NeedsInput)),
                );
                ui.label(
                    RichText::new("This looks like a large, separate question")
                        .font(crate::theme::medium(13.0))
                        .color(p.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("Pinned by kGraf").small().color(p.text_2));
                    ui.label(crate::icons::text(crate::icons::PIN, 14.0).color(p.text_2));
                });
            });
            ui.label(RichText::new(&offer.question).color(p.text));
            // CMP-CARD-3: neither choice acts at once.
            let confirming = model
                .confirming_offer
                .filter(|(id, _)| *id == offer.id)
                .map(|(_, accept)| accept);
            match confirming {
                None => {
                    ui.horizontal_wrapped(|ui| {
                        // CMP-BTN-2: starting a new project is the recommended,
                        // filled choice; filing it here is the tonal alternative.
                        let choice = if crate::chrome::filled_button(ui, p, "Start a new project")
                            .clicked()
                        {
                            Some(true)
                        } else if crate::chrome::tonal_button(ui, p, "No, file it here").clicked() {
                            Some(false)
                        } else {
                            None
                        };
                        if let Some(accept) = choice {
                            model.confirming_offer = Some((offer.id, accept));
                        }
                    });
                }
                Some(accept) => {
                    let consequence = if accept {
                        "A new kGraf project starts, seeded with this question as its \
                         starting prompt."
                    } else {
                        "The question is filed under this project's standalone Questions \
                         node, and an agent is tasked with it."
                    };
                    ui.horizontal(|ui| {
                        ui.label(crate::icons::text(crate::icons::WARNING, 16.0).color(p.danger));
                        ui.label(RichText::new(consequence).small().color(p.danger));
                    });
                    ui.horizontal(|ui| {
                        let confirm = crate::chrome::cta_button(ui, p, "Confirm", true);
                        if confirm.clicked() {
                            model.confirming_offer = None;
                            model.resolve_offer(backend, offer, accept);
                        }
                        if crate::chrome::text_button(ui, p.text, "Cancel").clicked() {
                            model.confirming_offer = None;
                        }
                    });
                }
            }
        })
        .response
        .rect
}

/// The box in the Stage style (CMP-CARD, CMP-FLD): the pinned cards (pending approvals;
/// questions join them once the runtime can ask one), the history, and the
/// composer with the token-attribution line. Call once per frame.
/// `selected_worker` resolves `/compact selected`.
pub fn show_styled(
    ui: &mut egui::Ui,
    model: &mut BoxModel,
    backend: &mut dyn BoxBackend,
    selected_worker: Option<&str>,
    opts: &BoxOptions,
) {
    use egui::RichText;
    let p = crate::theme::Palette::current(ui.ctx());
    model.absorb(backend);
    let state = backend.state();

    // Pinned cards, two to a row.
    let asks = backend.pending_asks();
    for pair in asks.chunks(2) {
        ui.columns(2, |cols| {
            for (col, ask) in cols.iter_mut().zip(pair) {
                let rect =
                    approval_card(col, model, backend, ask, opts.flash_ask == Some(ask.id), &p);
                if opts.flash_ask == Some(ask.id) {
                    col.scroll_to_rect(rect, Some(egui::Align::Center));
                }
            }
        });
        ui.add_space(8.0);
    }

    // Pinned question cards (design 0067, point 4). Empty on every backend
    // shipped so far (`BoxBackend::pending_questions`'s own default), so
    // nothing draws here yet in the running app; the path is real and tested
    // against a fake backend that does return one (`orchestrator_box::tests`).
    let questions = backend.pending_questions();
    for pair in questions.chunks(2) {
        ui.columns(2, |cols| {
            for (col, q) in cols.iter_mut().zip(pair) {
                question_card(col, model, backend, q, &p);
            }
        });
        ui.add_space(8.0);
    }

    // An offer to start a new project, seeded with a
    // question that turned out to be large and off topic. CMP-CARD-1/3, like
    // `approval_card`: a choice does not act at once, and the confirm step
    // states the consequence (INT-8: starting a project is significant).
    let offers = backend.pending_offers();
    for pair in offers.chunks(2) {
        ui.columns(2, |cols| {
            for (col, offer) in cols.iter_mut().zip(pair) {
                offer_card(col, model, backend, offer, &p);
            }
        });
        ui.add_space(8.0);
    }

    if !model.lines().is_empty() {
        egui::ScrollArea::vertical()
            .id_salt("orchestrator_history")
            .max_height(HISTORY_HEIGHT)
            .stick_to_bottom(true)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for line in model.lines() {
                    let mut job = egui::text::LayoutJob::default();
                    let fmt = |font: egui::FontId, color| egui::TextFormat {
                        font_id: font,
                        color,
                        ..Default::default()
                    };
                    let body = crate::theme::regular(13.0);
                    match line {
                        Line::You(t) => {
                            job.append(
                                "you",
                                0.0,
                                egui::TextFormat {
                                    background: p.inverted, // CMP-AV-3 (1.6): the user's chip is inverted
                                    ..fmt(crate::theme::medium(13.0), p.text_inv)
                                },
                            );
                            job.append(&format!(": {t}"), 0.0, fmt(body, p.text));
                        }
                        Line::Orchestrator(t) => {
                            job.append(
                                "orchestrator",
                                0.0,
                                fmt(crate::theme::medium(13.0), p.text),
                            );
                            job.append(&format!(": {t}"), 0.0, fmt(body, p.text));
                        }
                        Line::Notice(t) => {
                            job.append(t, 0.0, fmt(crate::theme::regular(12.0), p.text_2))
                        }
                        Line::Error(t) => {
                            job.append(t, 0.0, fmt(crate::theme::regular(12.0), p.danger))
                        }
                    }
                    ui.add(egui::Label::new(job).wrap());
                }
            });
        ui.add_space(4.0);
    }

    // "Attach as context" chips: CMP-CHIP-1's shape (32 high,
    // radius 8, additive fill) is the closest fit the style guide states for a
    // small removable token; it has no dedicated removable-chip rule, so this
    // reuses `chrome::chip` as-is and makes the whole chip the remove target
    // (a tooltip says so), rather than inventing new geometry the guide never
    // specified. CMP-CHIP-5's outlined "context" chip is not used here because
    // it is declared non-interactive except for its tooltip (9.3), and this
    // chip must be removable before sending.
    if !model.attachments().is_empty() {
        let mut removed = None;
        ui.horizontal_wrapped(|ui| {
            for (i, chip) in model.attachments().iter().enumerate() {
                let label = format!("{}  {}", chip.label(), crate::icons::CLOSE);
                let resp = crate::chrome::chip(ui, &p, &label, false, None)
                    .on_hover_text("Click to remove this attachment before sending");
                if resp.clicked() {
                    removed = Some(i);
                }
            }
        });
        if let Some(i) = removed {
            model.remove_attachment(i);
        }
        ui.add_space(4.0);
    }

    // The composer: the "you" avatar, an underlined field, Send.
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(40.0, 40.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 20.0, p.inverted); // CMP-AV-3 (1.6)
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            crate::icons::PERSON,
            crate::theme::icon_font(20.0),
            p.text_inv,
        );
        ui.add_space(8.0);
        ui.vertical(|ui| {
            let send_w = 84.0;
            ui.horizontal(|ui| {
                let label = ui.label(RichText::new(state_label(&state)).small().color(p.text_2));
                let hint = match &state {
                    BackendState::Idle { reason, .. } if !reason.is_empty() => reason.clone(),
                    _ => state_label(&state),
                };
                label.on_hover_text(hint);
                let waiting = model.awaiting();
                if waiting > 0 {
                    ui.label(
                        RichText::new(format!("{waiting} awaiting a reply"))
                            .small()
                            .color(p.text_2),
                    );
                }
            });
            let mut send = false;
            ui.horizontal(|ui| {
                let width = (ui.available_width() - send_w).max(80.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut model.input)
                        .hint_text(
                            "Tell the orchestrator what to do next: a note for the run, \
                             /run <instruction>, /status, /compact, /help",
                        )
                        .frame(egui::Frame::NONE)
                        .desired_width(width),
                );
                let line_y = resp.rect.bottom() + 2.0;
                let (color, w) = if model.empty_send {
                    (p.danger, 2.0)
                } else if resp.has_focus() {
                    (p.text, 2.0)
                } else {
                    (p.outline, 1.0)
                };
                ui.painter().line_segment(
                    [
                        egui::pos2(resp.rect.left(), line_y),
                        egui::pos2(resp.rect.right(), line_y),
                    ],
                    egui::Stroke::new(w, color),
                );
                if resp.changed() {
                    model.empty_send = false;
                }
                if opts.focus_input {
                    resp.request_focus();
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    send = true;
                    resp.request_focus();
                }
                let has_text = !model.input.trim().is_empty();
                if crate::chrome::cta_button(ui, &p, "Send", has_text).clicked() {
                    send = true;
                }
            });
            if send {
                if model.input.trim().is_empty() {
                    model.empty_send = true;
                } else {
                    model.submit(backend, selected_worker);
                }
            }
            if model.empty_send {
                ui.label(
                    RichText::new("Type a message first. An empty line is not sent.")
                        .small()
                        .color(p.danger),
                );
            }
            if !opts.attribution.is_empty() {
                ui.add(
                    egui::Label::new(RichText::new(&opts.attribution).small().color(p.text_2))
                        .truncate(),
                );
            }
        });
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn parsing_keeps_text_as_data_and_refuses_unknown_commands() {
        assert_eq!(parse("  "), BoxCommand::Empty);
        assert_eq!(parse("use tabs"), BoxCommand::Note("use tabs".into()));
        assert_eq!(parse("/status"), BoxCommand::Status);
        assert_eq!(parse("/run port it"), BoxCommand::Run("port it".into()));
        assert_eq!(parse("/runner"), BoxCommand::Unknown("/runner".into()));
        assert_eq!(
            parse("/compact"),
            BoxCommand::Compact(CompactTarget::Orchestrator)
        );
        assert_eq!(
            parse("/compact selected"),
            BoxCommand::Compact(CompactTarget::Selected)
        );
        assert_eq!(
            parse("/compact r1-a-r0-a0"),
            BoxCommand::Compact(CompactTarget::Worker("r1-a-r0-a0".into()))
        );
        assert_eq!(parse("/stat"), BoxCommand::Unknown("/stat".into()));
        assert_eq!(
            parse("/status now"),
            BoxCommand::Unknown("/status now".into())
        );
        assert_eq!(
            parse("//etc is a path"),
            BoxCommand::Note("/etc is a path".into())
        );
    }

    /// A backend that records what the box asked of it.
    #[derive(Default)]
    pub(crate) struct Recorder {
        pub state: Option<BackendState>,
        pub sent: Vec<(InboundKind, String)>,
        pub started: Vec<String>,
        pub compacted: Vec<Option<String>>,
        pub answered: Vec<(u64, AskAnswer)>,
        pub asks: Vec<PendingAsk>,
        pub events: Vec<BackendEvent>,
        pub record: Option<String>,
        pub questions: Vec<PendingQuestion>,
        pub question_answers: Vec<(u64, String)>,
        /// Text handed to `route_question`.
        pub routed: Vec<String>,
        pub offers: Vec<QuestionOffer>,
        pub offer_answers: Vec<(u64, bool)>,
    }

    impl BoxBackend for Recorder {
        fn state(&self) -> BackendState {
            self.state.clone().unwrap_or(BackendState::Idle {
                can_start: false,
                reason: "no host".into(),
            })
        }
        fn send(&mut self, kind: InboundKind, text: &str) -> Result<String, String> {
            self.sent.push((kind, text.to_owned()));
            Ok(format!("m{}", self.sent.len()))
        }
        fn start(&mut self, instruction: &str) -> Result<(), String> {
            self.started.push(instruction.to_owned());
            Ok(())
        }
        fn compact(&mut self, target: Option<String>) -> Result<(), String> {
            self.compacted.push(target);
            Ok(())
        }
        fn pending_asks(&self) -> Vec<PendingAsk> {
            self.asks.clone()
        }
        fn answer(&mut self, id: u64, answer: AskAnswer) -> Result<(), String> {
            self.answered.push((id, answer));
            Ok(())
        }
        fn poll(&mut self) -> Vec<BackendEvent> {
            std::mem::take(&mut self.events)
        }
        fn status_from_record(&self) -> Option<String> {
            self.record.clone()
        }
        fn pending_questions(&self) -> Vec<PendingQuestion> {
            self.questions.clone()
        }
        fn answer_question(&mut self, question: u64, answer: String) -> Result<(), String> {
            self.question_answers.push((question, answer));
            Ok(())
        }
        fn route_question(&mut self, text: &str) -> Result<(), String> {
            self.routed.push(text.to_owned());
            Ok(())
        }
        fn pending_offers(&self) -> Vec<QuestionOffer> {
            self.offers.clone()
        }
        fn accept_question_offer(&mut self, id: u64) -> Result<(), String> {
            self.offer_answers.push((id, true));
            Ok(())
        }
        fn decline_question_offer(&mut self, id: u64) -> Result<(), String> {
            self.offer_answers.push((id, false));
            Ok(())
        }
    }

    fn running() -> Recorder {
        Recorder {
            state: Some(BackendState::Running { run: "r1".into() }),
            ..Recorder::default()
        }
    }

    fn submit(m: &mut BoxModel, b: &mut Recorder, text: &str, selected: Option<&str>) {
        m.input = text.to_owned();
        m.submit(b, selected);
    }

    #[test]
    fn a_note_and_a_status_question_go_through_the_port_and_replies_close_them() {
        let (mut m, mut b) = (BoxModel::default(), running());
        submit(&mut m, &mut b, "prefer small functions", None);
        submit(&mut m, &mut b, "/status", None);
        assert_eq!(
            b.sent,
            [
                (InboundKind::Note, "prefer small functions".to_owned()),
                (InboundKind::StatusQuestion, "status".to_owned())
            ]
        );
        assert!(m.input.is_empty(), "the input clears on submit");
        assert_eq!(m.awaiting(), 2);
        b.events.push(BackendEvent::Reply {
            in_reply_to: "m2".into(),
            text: "run r1: 1 verified".into(),
        });
        assert!(m.absorb(&mut b));
        assert_eq!(m.awaiting(), 1);
        assert_eq!(
            m.lines().last(),
            Some(&Line::Orchestrator("run r1: 1 verified".into()))
        );
    }

    #[test]
    fn an_unknown_command_is_refused_and_never_sent() {
        let (mut m, mut b) = (BoxModel::default(), running());
        submit(&mut m, &mut b, "/deploy", None);
        assert!(b.sent.is_empty());
        assert!(matches!(m.lines().last(), Some(Line::Error(_))));
    }

    #[test]
    fn with_no_run_only_an_explicit_run_command_starts_one() {
        let mut m = BoxModel::default();
        let mut b = Recorder {
            state: Some(BackendState::Idle {
                can_start: true,
                reason: String::new(),
            }),
            ..Recorder::default()
        };
        // A plain line still never starts a run as its own instruction;
        // with no run in this window it is routed instead.
        submit(&mut m, &mut b, "port the parser", None);
        assert!(b.started.is_empty() && b.sent.is_empty());
        assert_eq!(b.routed, ["port the parser"]);
        assert!(matches!(m.lines().last(), Some(Line::Notice(n)) if n.contains("routing")));
        // /run does, with the instruction shown.
        submit(&mut m, &mut b, "/run port the parser", None);
        assert_eq!(b.started, ["port the parser"]);
        assert!(matches!(m.lines().last(), Some(Line::Notice(n)) if n.contains("port the parser")));
        submit(&mut m, &mut b, "/run", None);
        assert_eq!(
            b.started.len(),
            1,
            "/run without an instruction starts nothing"
        );

        let mut m = BoxModel::default();
        let mut b = Recorder::default();
        submit(&mut m, &mut b, "port the parser", None);
        assert!(b.started.is_empty() && b.sent.is_empty());
        assert_eq!(
            m.lines().last(),
            Some(&Line::Error("not sent: no host".into())),
            "the reason is shown, the text is not silently dropped"
        );
    }

    /// Accepting an offer queues a new-project
    /// request (`Workspace::tick` picks it up, the same "Add project" path)
    /// and clears the offer here; declining dispatches in this project
    /// instead and never queues one.
    #[test]
    fn accepting_an_offer_queues_a_new_project_request_and_declining_never_does() {
        let mut m = BoxModel::default();
        let mut b = Recorder::default();
        let offer = QuestionOffer {
            id: 7,
            question: "design an entirely separate mobile app".into(),
            graph_id: Some("questions/offer-1-abc".into()),
        };
        m.resolve_offer(&mut b, &offer, true);
        assert_eq!(b.offer_answers, [(7, true)]);
        assert_eq!(
            m.take_new_project_request(),
            Some((
                "design an entirely separate mobile app".to_owned(),
                Some("questions/offer-1-abc".to_owned())
            ))
        );
        assert_eq!(m.take_new_project_request(), None, "taken once");

        let mut m = BoxModel::default();
        let mut b = Recorder::default();
        m.resolve_offer(&mut b, &offer, false);
        assert_eq!(b.offer_answers, [(7, false)]);
        assert_eq!(
            m.take_new_project_request(),
            None,
            "declining never queues one"
        );
        assert!(matches!(m.lines().last(), Some(Line::Notice(n)) if n.contains("Questions")));
    }

    #[test]
    fn status_with_no_run_here_reads_the_record_and_says_so() {
        let mut m = BoxModel::default();
        let mut b = Recorder {
            record: Some("run r9 (running): 2 verified, 1 open; c running".into()),
            ..Recorder::default()
        };
        submit(&mut m, &mut b, "/status", None);
        assert!(b.sent.is_empty());
        let Some(Line::Orchestrator(text)) = m.lines().last() else {
            panic!("{:?}", m.lines());
        };
        assert!(text.starts_with("run r9 (running)"));
        assert!(text.contains("read from the run record"));
    }

    #[test]
    fn compact_targets_the_orchestrator_the_selected_worker_or_a_named_one() {
        let (mut m, mut b) = (BoxModel::default(), running());
        submit(&mut m, &mut b, "/compact", Some("w7"));
        submit(&mut m, &mut b, "/compact selected", Some("w7"));
        submit(&mut m, &mut b, "/compact w2", None);
        assert_eq!(b.compacted, [None, Some("w7".into()), Some("w2".into())]);
        submit(&mut m, &mut b, "/compact selected", None);
        assert_eq!(b.compacted.len(), 3, "nothing selected: not sent");
        assert!(matches!(m.lines().last(), Some(Line::Error(e)) if e.contains("select a worker")));
    }

    #[test]
    fn answering_an_ask_reaches_the_backend_with_the_chosen_answer() {
        let (mut m, mut b) = (BoxModel::default(), running());
        let ask = PendingAsk {
            id: 4,
            worker: "w1".into(),
            task: "t1".into(),
            tool: "shell".into(),
            question: "run `git push`?".into(),
            asked_by: "shell".into(),
            input: "{}".into(),
        };
        m.answer(&mut b, &ask, AskAnswer::AllowAlways);
        assert_eq!(b.answered, [(4, AskAnswer::AllowAlways)]);
        assert!(matches!(m.lines().last(), Some(Line::Notice(n)) if n.contains("allowed always")));
    }

    type State = (BoxModel, Recorder);

    fn harness(backend: Recorder) -> egui_kittest::Harness<'static, State> {
        let mut h = egui_kittest::Harness::new_ui_state(
            |ui, (m, b): &mut State| {
                // The theme's fonts apply from the pass after they are set.
                if crate::theme::ensure(ui.ctx()) {
                    show(ui, m, b, Some("w7"));
                }
            },
            (BoxModel::default(), backend),
        );
        h.run();
        h
    }

    /// The real widget, driven through AccessKit: focus, type, Enter.
    #[test]
    fn typing_in_the_widget_and_pressing_enter_sends_through_the_backend() {
        use egui_kittest::kittest::Queryable;
        let mut h = harness(running());
        h.get_by_role(egui::accesskit::Role::TextInput).focus();
        h.run();
        h.get_by_role(egui::accesskit::Role::TextInput)
            .type_text("prefer small functions");
        h.run();
        h.key_press(egui::Key::Enter);
        h.run();
        let (m, b) = h.state();
        assert_eq!(
            b.sent,
            [(InboundKind::Note, "prefer small functions".to_owned())]
        );
        assert!(m.input.is_empty());
        // The reply shows in place, with no other surface involved.
        h.state_mut().1.events.push(BackendEvent::Reply {
            in_reply_to: "m1".into(),
            text: "noted, as information for the run".into(),
        });
        h.run();
        h.get_by_label("orchestrator: noted, as information for the run");
    }

    #[test]
    fn a_pending_approval_is_answered_by_its_buttons_in_place() {
        use egui_kittest::kittest::Queryable;
        let ask = |id| PendingAsk {
            id,
            worker: "w1".into(),
            task: "t1".into(),
            tool: "shell".into(),
            question: "run `git push`?".into(),
            asked_by: "shell".into(),
            input: "{\"command\":[\"git\",\"push\"]}".into(),
        };
        let mut h = harness(Recorder {
            asks: vec![ask(3)],
            ..running()
        });
        h.run();
        h.get_by_label("Approval: shell for w1");
        // CMP-CARD-3: a choice does not act at once; it asks to confirm and
        // says what will happen. Cancel brings the choices back.
        h.get_by_label("Allow always").click();
        h.run();
        assert!(h.state().1.answered.is_empty(), "nothing answered yet");
        h.get_by_label_contains("Saves a rule");
        h.get_by_label("Cancel").click();
        h.run();
        assert!(h.state().1.answered.is_empty());
        let choose = |h: &mut egui_kittest::Harness<'static, State>, label: &str| {
            h.get_by_label(label).click();
            h.run();
            h.get_by_label("Confirm").click();
            h.run();
        };
        choose(&mut h, "Allow always");
        assert_eq!(h.state().1.answered, [(3, AskAnswer::AllowAlways)]);
        // An MCP tool: the button says the rule covers every call to it.
        h.state_mut().1.asks = vec![PendingAsk {
            tool: "mcp__docs__search".into(),
            ..ask(9)
        }];
        h.run();
        choose(&mut h, "Allow every call to mcp__docs__search");
        assert_eq!(
            h.state().1.answered.last(),
            Some(&(9, AskAnswer::AllowAlways))
        );
        h.state_mut().1.asks = vec![ask(4)];
        h.run();
        choose(&mut h, "Deny");
        assert!(matches!(
            h.state().1.answered.last(),
            Some((4, AskAnswer::Deny { .. }))
        ));
        h.state_mut().1.asks = vec![ask(5)];
        h.run();
        choose(&mut h, "Allow once");
        assert_eq!(
            h.state().1.answered.last(),
            Some(&(5, AskAnswer::AllowOnce))
        );
    }

    /// Design 0067, point 4: a worker question is a pinned card, answered
    /// through `answer_question`. `Recorder` here is a fake backend that
    /// does return one, proving the card and its answer path are real and
    /// drawn; `OrchestratorHost`'s own `pending_questions` is empty until
    /// `AskDesk` holds a question row.
    #[test]
    fn a_pending_question_is_answered_by_its_card_in_place() {
        use egui_kittest::kittest::Queryable;
        let mut h = harness(Recorder {
            questions: vec![PendingQuestion {
                id: 7,
                worker: "w3".into(),
                question: "arena or bump allocator?".into(),
            }],
            ..running()
        });
        h.run();
        h.get_by_label("Question from w3");
        h.get_by_label_contains("arena or bump allocator");
        // Two text inputs exist (the composer's own field, always drawn, and
        // the question card's): the card draws first, so its field is first.
        let field = h
            .get_all_by_role(egui::accesskit::Role::TextInput)
            .next()
            .expect("the question card's text field");
        field.focus();
        h.run();
        // Mutation check (tried by hand, reverted): with no text typed, the
        // Answer button is disabled and clicking a disabled button in
        // egui_kittest does nothing, so this must type first.
        h.get_all_by_role(egui::accesskit::Role::TextInput)
            .next()
            .expect("the question card's text field")
            .type_text("arena");
        h.run();
        h.get_by_label("Answer").click();
        h.run();
        assert_eq!(h.state().1.question_answers, [(7, "arena".to_owned())]);
        h.get_by_label_contains("answer sent to w3");
    }

    #[test]
    fn the_box_keeps_a_bounded_history() {
        let (mut m, mut b) = (BoxModel::default(), running());
        for i in 0..(MAX_LINES + 20) {
            submit(&mut m, &mut b, &format!("note {i}"), None);
        }
        assert_eq!(m.lines().len(), MAX_LINES);
        assert_eq!(
            m.lines().last(),
            Some(&Line::You(format!("note {}", MAX_LINES + 19)))
        );
    }

    // --- "Attach as context" --------------------------------

    #[test]
    fn a_sent_note_carries_attached_agent_output_quoted_as_data() {
        let (mut m, mut b) = (BoxModel::default(), running());
        m.attach(AttachmentChip {
            agent: "r1-a-r0-a0".into(),
            selection: Some("use an arena allocator".into()),
        });
        submit(&mut m, &mut b, "apply this everywhere", None);
        let (_, sent) = &b.sent[0];
        assert!(
            sent.contains("<agent_output agent=\"r1-a-r0-a0\">"),
            "the agent id is on the block: {sent}"
        );
        assert!(sent.contains("use an arena allocator"));
        assert!(sent.ends_with("apply this everywhere"));
        assert!(
            m.attachments().is_empty(),
            "consumed exactly once, into this message"
        );
        // The user's own history line shows what they typed, not the quoted
        // block: the attachment is data for the orchestrator, not a rewrite
        // of what the user said.
        assert_eq!(
            m.lines().first(),
            Some(&Line::You("apply this everywhere".to_owned()))
        );
    }

    #[test]
    fn with_no_selection_the_attached_block_carries_only_the_agent_id() {
        let (mut m, mut b) = (BoxModel::default(), running());
        m.attach(AttachmentChip {
            agent: "r1-a-r0-a1".into(),
            selection: None,
        });
        submit(&mut m, &mut b, "look at this one", None);
        let (_, sent) = &b.sent[0];
        assert!(sent.contains("<agent_output agent=\"r1-a-r0-a1\">"));
        assert!(sent.contains("no text was selected"));
    }

    #[test]
    fn a_plain_note_with_no_attachments_is_sent_unchanged() {
        // Mutation check (tried by hand, reverted): removing the empty-check
        // in `compose_with_attachments` and always building a block would
        // make this assertion fail (the sent text would carry an empty
        // `<agent_output>` wrapper it does not today).
        let (mut m, mut b) = (BoxModel::default(), running());
        submit(&mut m, &mut b, "no attachments here", None);
        assert_eq!(
            b.sent,
            [(InboundKind::Note, "no attachments here".to_owned())]
        );
    }

    #[test]
    fn removing_a_pending_attachment_drops_it_before_it_is_sent() {
        let mut m = BoxModel::default();
        m.attach(AttachmentChip {
            agent: "a1".into(),
            selection: None,
        });
        m.attach(AttachmentChip {
            agent: "a2".into(),
            selection: None,
        });
        m.remove_attachment(0);
        assert_eq!(m.attachments().len(), 1);
        assert_eq!(m.attachments()[0].agent, "a2");
        let mut b = running();
        submit(&mut m, &mut b, "final text", None);
        let (_, sent) = &b.sent[0];
        assert!(sent.contains("agent=\"a2\""));
        assert!(!sent.contains("agent=\"a1\""));
    }

    #[test]
    fn an_unsent_note_leaves_the_attachment_pending_for_the_next_attempt() {
        // With no run in this window, nothing is sent, so the chip stays: the
        // user should not have to re-attach after fixing an unrelated reason
        // the message did not go out.
        let mut m = BoxModel::default();
        m.attach(AttachmentChip {
            agent: "a1".into(),
            selection: None,
        });
        let mut b = Recorder::default();
        submit(&mut m, &mut b, "no host yet", None);
        assert!(b.sent.is_empty());
        assert_eq!(m.attachments().len(), 1, "kept for the next attempt");
    }
}
