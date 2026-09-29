//! The session view (design 0067): reasoning visibility as a stated model
//! limit, the `addressable` gate, and "Attach as context".
//!
//! **This is drawn, in the running app, in the one text surface's "Up next"
//! card** (`layout.rs::up_next`, `text_surface::TextSurface::show_session`):
//! selecting an agent node fills that one surface with its transcript
//! (`text_surface.rs`'s existing job, 0034) *and*, from this module, its
//! reasoning line, the hidden-reasoning line, its children as links, the
//! composer (labeled as the exception route), the mode switch, stop/resume
//! and "Attach as context". This file itself holds the pure view model and
//! functions; `text_surface.rs` draws them; `layout.rs::up_next` applies the
//! resulting actions (send, stop, resume, attach, select a child) to the real
//! backend, because doing so needs `Interaction::box_and_host` and
//! `Interaction::on_selection`, which only `layout.rs` may call without
//! touching `interaction.rs`.
//!
//! **The graph highlight is separate.** Feeding `read_session_graph` into the
//! galaxy stage (session diamonds, `spawned`/`connection` overlay edges,
//! `kgraf-galaxy/src`, `run_view.rs`) is not part of this view. `child_links`
//! reroutes the text surface to a child's own conversation; the matching graph
//! highlight belongs to the galaxy stage.
//!
//! **Driven through the orchestrator's session seams.** `send_to_session`,
//! `stop_session`, `resume_session` and `answer_question` are methods on
//! [`kgraf_orchestrator::OrchestratorHandle`]. Where no hook is bound, each
//! replies [`kgraf_orchestrator::SessionError::NotBuilt`]
//! (`kgraf-orchestrator/src/orchestrator/sessions.rs`). This module calls
//! those seams and does not special-case that reply.
//!
//! **`addressable` mirrors a stored fact.** A merge-conflict resolver's session
//! is read-only: no composer, no note (decision 0073).
//! `SessionFacts::addressable` is a durable, written fact (`kgraf-orchestrator`'s
//! `record_worker_row`), and `session_view_for`'s `graph_facts` reads it (and `SessionFacts::kind`,
//! for CMP-AV-6's "Resolver" label) straight through, never fabricating either.

use kgraf_core::session::{InboxKind, InjectMode, ReasoningVisibility, SessionKind};
use kgraf_orchestrator::StoredRun;

/// One session's facts as this view needs them: a slice of
/// [`kgraf_core::session::SessionFacts`] plus display-only additions this unit
/// owns (see the module docs on `addressable`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub id: String,
    pub kind: SessionKind,
    /// True by default (only a merge-conflict resolver is ever `false`). `false` removes the
    /// composer and the stop/resume controls, and a note addressed to it is refused (`controls` and
    /// `SessionComposer::submit` both enforce this, so a caller cannot show one without the other
    /// by mistake).
    pub addressable: bool,
    /// Ended, from the session's own facts (`SessionRecords::lifecycle`).
    /// A note to a finished agent is
    /// refused locally and the composer offers to send it to the orchestrator
    /// instead, without a round trip through the (still-stubbed) handle.
    pub retired: bool,
    pub reasoning_declared: ReasoningVisibility,
    /// `Unknown` until a round has run; see [`observed_visibility`].
    pub reasoning_observed: ReasoningVisibility,
    /// Whether the route reports a reasoning token count at all, even with no
    /// text (`kgraf_fleet::Entry::reports_reasoning_tokens`): changes which of
    /// [`hidden_reasoning_line`]'s two sentences applies.
    pub reports_reasoning_tokens: bool,
    /// The session's children (subagents, handoff children), for the view to
    /// link to.
    pub children: Vec<String>,
}

/// What controls a session's view shows, derived from [`SessionView`] alone so
/// a caller cannot show a composer for a non-addressable session by drawing
/// the two pieces separately (the single point every gating rule goes through).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Controls {
    pub composer: bool,
    pub stop_resume: bool,
}

/// A non-addressable session (a merge-conflict
/// resolver) shows thought and output read-only, with no composer and no
/// stop/resume controls.
pub fn controls(view: &SessionView) -> Controls {
    Controls {
        composer: view.addressable,
        stop_resume: view.addressable,
    }
}

/// The observed classification of what a session's model actually returned,
/// from live data (design 0067, "What visible thought means per provider"):
/// - no rounds yet: `Unknown` (nothing to observe);
/// - reasoning tokens billed but no reasoning text: `None` (the vendor hides
///   the text even though it charges for it);
/// - reasoning text present: the catalog's declared kind (`Full` or
///   `Summary`; the declared value already says which shape the text is in,
///   so the observation confirms it rather than re-deriving it).
pub fn observed_visibility(
    reasoning_tokens: u64,
    reasoning_text_chars: u64,
    rounds: u32,
    declared: ReasoningVisibility,
) -> ReasoningVisibility {
    if rounds == 0 {
        return ReasoningVisibility::Unknown;
    }
    if reasoning_text_chars > 0 {
        return declared;
    }
    if reasoning_tokens > 0 {
        return ReasoningVisibility::None;
    }
    ReasoningVisibility::None
}

/// A declared value that disagrees with what was actually observed, shown
/// rather than resolved silently. `Unknown` on either side is
/// not a disagreement: it means "not enough rounds yet", not "wrong".
pub fn visibility_disagreement(
    declared: ReasoningVisibility,
    observed: ReasoningVisibility,
) -> Option<String> {
    if observed == ReasoningVisibility::Unknown || declared == observed {
        return None;
    }
    Some(format!(
        "the catalog declares {} but this session observed {} (model limit; not resolved \
         automatically)",
        label(declared),
        label(observed)
    ))
}

/// The reasoning visibility's reader-facing name (Settings > Providers,
/// and `visibility_disagreement` above).
pub fn label(v: ReasoningVisibility) -> &'static str {
    match v {
        ReasoningVisibility::Full => "full reasoning",
        ReasoningVisibility::Summary => "a summary only",
        ReasoningVisibility::None => "none",
        ReasoningVisibility::Unknown => "unknown",
    }
}

/// Where reasoning is hidden, the session view states this (design 0067):
/// a model limit, not a bug. `None` when reasoning is visible or not yet
/// known, so a caller shows nothing extra in those cases.
pub fn hidden_reasoning_line(v: ReasoningVisibility, reports_tokens: bool) -> Option<&'static str> {
    match v {
        ReasoningVisibility::None if reports_tokens => {
            Some("This model's reasoning is not returned by its provider (model limit).")
        }
        ReasoningVisibility::None => Some(
            "This model's reasoning is not returned by its provider (model limit); its \
             token count is not reported either.",
        ),
        ReasoningVisibility::Full | ReasoningVisibility::Summary | ReasoningVisibility::Unknown => {
            None
        }
    }
}

/// The composer's mode switch (design 0067, point 3): at the next tool-batch
/// boundary, or interrupting the round in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SendMode {
    #[default]
    AtBoundary,
    Interrupt,
}

impl SendMode {
    pub fn label(self) -> &'static str {
        match self {
            SendMode::AtBoundary => "At the next boundary",
            SendMode::Interrupt => "Interrupt now",
        }
    }
}

impl From<SendMode> for InjectMode {
    fn from(m: SendMode) -> Self {
        match m {
            SendMode::AtBoundary => InjectMode::AtBoundary,
            SendMode::Interrupt => InjectMode::Interrupt,
        }
    }
}

/// What submitting the composer's text should do next; the caller (the real
/// window, once wired) turns [`ComposerAction::Send`] into a call to
/// [`kgraf_orchestrator::OrchestratorHandle::send_to_session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerAction {
    Send {
        text: String,
        kind: InboxKind,
        mode: InjectMode,
    },
    /// Refused, with the text offered back so the
    /// caller can send it to the orchestrator instead without the user
    /// retyping it.
    RefusedRetired { text: String },
    /// Never sent: an empty line, or a non-addressable session
    /// (`controls` already hides the composer for this case, so this
    /// arm is the model's own belt-and-braces check, not the only guard).
    Refused(&'static str),
}

/// The session composer's state: the input, the send mode, and the offer left
/// behind by a refused note.
#[derive(Debug, Default)]
pub struct SessionComposer {
    pub input: String,
    pub mode: SendMode,
    /// Set by a [`ComposerAction::RefusedRetired`], so "send to the
    /// orchestrator instead" has the exact text to reuse; cleared once acted
    /// on or replaced by a new attempt.
    pub redirect_offer: Option<String>,
}

impl SessionComposer {
    /// Acts on the current input against `view`, clearing it. Every rule this
    /// enforces is enforced here, in one place, so a UI cannot bypass it by
    /// calling the handle directly.
    pub fn submit(&mut self, view: &SessionView) -> ComposerAction {
        let text = std::mem::take(&mut self.input);
        if text.trim().is_empty() {
            return ComposerAction::Refused("an empty message is not sent");
        }
        if !view.addressable {
            return ComposerAction::Refused("this session takes no direct input (read-only)");
        }
        if view.retired {
            self.redirect_offer = Some(text.clone());
            return ComposerAction::RefusedRetired { text };
        }
        self.redirect_offer = None;
        ComposerAction::Send {
            text,
            kind: InboxKind::Note,
            mode: self.mode.into(),
        }
    }
}

/// One "Attach as context" chip: added to the
/// orchestrator's message box (`orchestrator_box::BoxModel::attach`), shown
/// before sending, and removable. It carries the agent id alone with no
/// selection, or the agent id and the selected text with one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentChip {
    pub agent: String,
    pub selection: Option<String>,
}

impl AttachmentChip {
    /// The chip's label in the composer (style guide CMP-CHIP-1 shape; see
    /// `orchestrator_box`'s render for the full rule citation).
    pub fn label(&self) -> String {
        match &self.selection {
            Some(text) => {
                let snippet: String = text.chars().take(40).collect();
                let ellipsis = if text.chars().count() > 40 { "…" } else { "" };
                format!("{}: \"{snippet}{ellipsis}\"", self.agent)
            }
            None => self.agent.clone(),
        }
    }
}

/// Builds the chip "Attach as context" adds: the agent id alone with no
/// selection, or the agent id and the selected text with one. If nothing is highlighted, the agent
/// id is sent. If something is highlighted, the agent id and the highlighted content are sent.
/// Whitespace-only "selected" text counts as no selection.
pub fn attach_as_context(agent: &str, selected_text: Option<&str>) -> AttachmentChip {
    AttachmentChip {
        agent: agent.to_owned(),
        selection: selected_text
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    }
}

/// The session's children, as the view's own links. Highlighting the matching
/// diamond on the stage belongs to the galaxy stage.
/// `text_surface::TextSurface::show_session` routes the one text surface to a
/// child's own conversation when one of these is clicked.
pub fn child_links(view: &SessionView) -> &[String] {
    &view.children
}

/// `SessionView` from the run records, plus `graph`'s own facts where one is
/// open (honest about what it cannot know, rather than fabricating it, when
/// `graph` is `None`: no project open, or the store could not be read).
///
/// - `addressable` and `kind`: `graph_facts`' real `SessionFacts`, keyed by
///   the worker (attempt) id, not `agent` (the lease holder). `true` and
///   `SessionKind::Worker` with no graph row yet (a worker dispatched before
///   its graph row is written, or no store open): the ordinary case, never
///   a resolver's.
/// - `retired`: the holder has no open contract left in the run record. An
///   agent that never had one reads as retired too (there is nothing else to
///   show for it); a caller only calls this for an agent that was, in fact,
///   selected from something that exists.
/// - `reasoning_declared`/`reports_reasoning_tokens`: the catalog's value for
///   the worker's own recorded route (`OpenContract::worker_routes`, the same
///   index as `OpenContract::workers`), split on `id:model` as
///   `kgraf-host/src/config.rs::split_route` writes it. `Unknown`/`false` for
///   a route not in the catalog or not yet recorded.
/// - `children`: `graph_facts`' `spawned` edges; empty with no graph row.
pub fn session_view_for(
    agent: &str,
    runs: &[StoredRun],
    graph: Option<&kgraf_graph::GraphStore>,
) -> SessionView {
    let contracts = crate::run_view::contracts_for_holder(runs, agent);
    let mut declared = ReasoningVisibility::Unknown;
    let mut reports_tokens = false;
    // `agent` is the graph's lease holder (`<run>/<lineage>`); its current
    // attempt is the newest worker of its dispatched contract
    // (`run_view::worker_for_holder`'s own rule), and that worker's recorded
    // route is at the same index in `worker_routes`.
    let worker = crate::run_view::worker_for_holder(runs, agent);
    if let Some(worker) = &worker {
        'outer: for (_, c) in &contracts {
            for (w, route) in c.workers.iter().zip(c.worker_routes.iter()) {
                if w.as_str() == worker {
                    let id = route.split_once(':').map_or(route.as_str(), |(id, _)| id);
                    if let Ok(e) = kgraf_fleet::entry(id) {
                        declared = e.reasoning_visibility;
                        reports_tokens = e.reports_reasoning_tokens;
                    }
                    break 'outer;
                }
            }
        }
    }
    // Design 0067: `addressable` is a session fact, and a subagent's parent
    // is a graph fact (`spawned` edges), both only in the project's graph
    // store; `worker` (the attempt id, `SessionFacts::id`), not `agent` (the
    // lease holder), is the row's own key. No row yet (a worker dispatched
    // before its graph row is written, or no store open) keeps the
    // defaults below rather than fabricating either.
    let (addressable, kind, children) = worker
        .as_deref()
        .zip(graph)
        .and_then(|(w, g)| graph_facts(g, w))
        .unwrap_or((true, SessionKind::Worker, Vec::new()));
    SessionView {
        id: agent.to_owned(),
        kind,
        addressable,
        retired: contracts.is_empty(),
        reasoning_declared: declared,
        reasoning_observed: ReasoningVisibility::Unknown,
        reports_reasoning_tokens: reports_tokens,
        children,
    }
}

/// `worker`'s `addressable` and `kind` facts (a resolver's
/// `kind` is what tells the panel to draw CMP-AV-6's label) and its
/// children's ids (design 0067's `spawned` edges), from the project's graph
/// store. `None` when the store has no row for `worker` yet.
fn graph_facts(
    store: &kgraf_graph::GraphStore,
    worker: &str,
) -> Option<(bool, SessionKind, Vec<String>)> {
    let id = kgraf_core::AgentId::new(worker);
    let facts = store.session_by_id(&id).ok()??;
    let graph = kgraf_graph::read_session_graph(store, None).ok()?;
    let children = graph
        .spawned
        .into_iter()
        .filter(|(parent, _)| parent.as_str() == worker)
        .map(|(_, child)| child.to_string())
        .collect();
    Some((facts.addressable, facts.kind, children))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_view::tests::{contract, stored};
    use kgraf_core::RunId;
    use kgraf_orchestrator::{ContractStatus, RunStateRecord};

    #[test]
    fn session_view_for_reads_the_route_from_the_worker_s_own_recorded_route() {
        let mut c = contract("a", ContractStatus::Dispatched, &["r1-a-r0-a0"]);
        c.worker_routes = vec!["kimi:kimi-k3".into()];
        let mut r = RunStateRecord::new(RunId::new("r1"), "go", 1);
        r.open_contracts = vec![c];
        let runs = [stored(r)];
        // "r1/a": the graph's lease-holder id (run "r1", lineage "a"), not the
        // worker attempt id ("r1-a-r0-a0"); `contracts_for_holder` parses it
        // that way (`run_view::parse_holder`).
        let v = session_view_for("r1/a", &runs, None);
        assert_eq!(
            v.reasoning_declared,
            ReasoningVisibility::Full,
            "kimi (chat) declares full"
        );
        assert!(v.reports_reasoning_tokens);
        assert!(v.addressable);
        assert!(!v.retired, "an open, dispatched contract: not retired");
    }

    /// Design 0067: `addressable` and `children` come from the project's
    /// real graph store (`kgraf_graph::sessions::read_session_graph`'s
    /// `spawned` edges), not a hardcoded default, once one is open.
    #[test]
    fn session_view_for_reads_children_and_addressable_from_the_real_graph_store() {
        use kgraf_core::session::{SessionFacts, SessionKind as SK};
        let dir = tempfile::tempdir().expect("dir");
        let db = dir.path().join("graph.db");
        let store = kgraf_graph::GraphStore::open(&db, dir.path()).expect("graph");
        let run = RunId::new("r1");
        let parent = SessionFacts {
            id: kgraf_core::AgentId::new("r1-a-r0-a0"),
            run: run.clone(),
            kind: SK::Worker,
            parent: None,
            contract: None,
            route: None,
            model: None,
            depth: 0,
            transcript: None,
            worktree: None,
            write_scope: Vec::new(),
            reasoning: ReasoningVisibility::Unknown,
            started_ms: 1,
            addressable: false,
        };
        store.session_upsert(&parent).expect("parent");
        let child = SessionFacts {
            id: kgraf_core::AgentId::new("r1-a-r0-a0.s1"),
            parent: Some(parent.id.clone()),
            kind: SK::Subagent,
            depth: 1,
            ..parent.clone()
        };
        store.session_upsert(&child).expect("child");

        let mut c = contract("a", ContractStatus::Dispatched, &["r1-a-r0-a0"]);
        c.worker_routes = vec!["kimi:kimi-k3".into()];
        let mut r = RunStateRecord::new(run, "go", 1);
        r.open_contracts = vec![c];
        let runs = [stored(r)];

        let v = session_view_for("r1/a", &runs, Some(&store));
        assert!(
            !v.addressable,
            "the real session fact, not the model's own true default"
        );
        assert_eq!(
            v.children,
            ["r1-a-r0-a0.s1".to_owned()],
            "a real spawned edge, not an empty default"
        );
    }

    #[test]
    fn session_view_for_a_holder_with_no_open_contract_reads_as_retired() {
        let r = RunStateRecord::new(RunId::new("r1"), "go", 1);
        let runs = [stored(r)];
        let v = session_view_for("r1/a", &runs, None);
        assert!(
            v.retired,
            "mutation check (tried, reverted): `retired: false` here breaks Q6"
        );
        assert_eq!(v.reasoning_declared, ReasoningVisibility::Unknown);
    }

    #[test]
    fn session_view_for_an_unknown_route_declares_unknown() {
        let mut c = contract("a", ContractStatus::Dispatched, &["r1-a-r0-a0"]);
        c.worker_routes = vec!["not-a-catalog-route:m".into()];
        let mut r = RunStateRecord::new(RunId::new("r1"), "go", 1);
        r.open_contracts = vec![c];
        let runs = [stored(r)];
        let v = session_view_for("r1/a", &runs, None);
        assert_eq!(v.reasoning_declared, ReasoningVisibility::Unknown);
        assert!(!v.reports_reasoning_tokens);
    }

    fn view(addressable: bool, retired: bool) -> SessionView {
        SessionView {
            id: "r1-a-r0-a0".into(),
            kind: SessionKind::Worker,
            addressable,
            retired,
            reasoning_declared: ReasoningVisibility::Full,
            reasoning_observed: ReasoningVisibility::Unknown,
            reports_reasoning_tokens: true,
            children: Vec::new(),
        }
    }

    // --- observed_visibility -------------------------------------------------

    #[test]
    fn observed_visibility_covers_the_four_cases() {
        let declared = ReasoningVisibility::Full;
        assert_eq!(
            observed_visibility(0, 0, 0, declared),
            ReasoningVisibility::Unknown,
            "no rounds: unknown"
        );
        assert_eq!(
            observed_visibility(120, 0, 3, declared),
            ReasoningVisibility::None,
            "tokens billed, no text: none"
        );
        assert_eq!(
            observed_visibility(120, 40, 3, declared),
            declared,
            "text present: the declared kind"
        );
        assert_eq!(
            observed_visibility(0, 0, 3, declared),
            ReasoningVisibility::None,
            "no reasoning at all reported: none"
        );
    }

    #[test]
    fn a_disagreement_between_declared_and_observed_is_reported_not_resolved() {
        let d = visibility_disagreement(ReasoningVisibility::Full, ReasoningVisibility::None)
            .expect("declared full, observed none: a disagreement");
        assert!(d.contains("full reasoning") && d.contains("none"));
        assert_eq!(
            visibility_disagreement(ReasoningVisibility::Full, ReasoningVisibility::Full),
            None,
            "agreement is not reported"
        );
        assert_eq!(
            visibility_disagreement(ReasoningVisibility::Full, ReasoningVisibility::Unknown),
            None,
            "unknown means not enough rounds yet, not a disagreement"
        );
    }

    #[test]
    fn hidden_reasoning_line_names_the_model_limit_and_whether_tokens_are_reported() {
        assert!(
            hidden_reasoning_line(ReasoningVisibility::None, true)
                .unwrap()
                .contains("model limit")
        );
        assert!(
            !hidden_reasoning_line(ReasoningVisibility::None, true)
                .unwrap()
                .contains("token count")
        );
        assert!(
            hidden_reasoning_line(ReasoningVisibility::None, false)
                .unwrap()
                .contains("token count")
        );
        assert_eq!(hidden_reasoning_line(ReasoningVisibility::Full, true), None);
        assert_eq!(
            hidden_reasoning_line(ReasoningVisibility::Unknown, true),
            None
        );
    }

    // --- addressable gate -----------------------------------------------------

    #[test]
    fn a_non_addressable_session_shows_no_composer_and_no_stop_resume() {
        let c = controls(&view(false, false));
        assert!(!c.composer);
        assert!(!c.stop_resume);
        // Mutation check (tried by hand, reverted): if `controls` returned
        // `composer: true` unconditionally, this assertion is what catches it.
        let c = controls(&view(true, false));
        assert!(c.composer);
        assert!(c.stop_resume);
    }

    #[test]
    fn a_note_to_a_non_addressable_session_is_refused_even_if_the_ui_shows_it_anyway() {
        // The composer itself refuses, not only `controls`: a caller that
        // ignores `controls` (a bug) still cannot reach a non-addressable
        // session through the model.
        let mut composer = SessionComposer {
            input: "hello".into(),
            ..SessionComposer::default()
        };
        let action = composer.submit(&view(false, false));
        assert!(matches!(action, ComposerAction::Refused(_)));
    }

    // --- A note to a finished agent ----------------------------------------

    #[test]
    fn a_note_to_a_retired_session_is_refused_and_offered_to_the_orchestrator() {
        let mut composer = SessionComposer {
            input: "please fix the typo".into(),
            ..SessionComposer::default()
        };
        let action = composer.submit(&view(true, true));
        assert_eq!(
            action,
            ComposerAction::RefusedRetired {
                text: "please fix the typo".into()
            }
        );
        assert_eq!(
            composer.redirect_offer.as_deref(),
            Some("please fix the typo"),
            "the exact text is kept so the redirect need not be retyped"
        );
        // Mutation check (tried by hand, reverted): if `submit` checked
        // `addressable` only and skipped the `retired` check, this test fails
        // because the action would be `Send`, not `RefusedRetired`.
    }

    #[test]
    fn a_note_to_a_running_addressable_session_sends() {
        let mut composer = SessionComposer {
            input: "steer left".into(),
            mode: SendMode::Interrupt,
            ..SessionComposer::default()
        };
        let action = composer.submit(&view(true, false));
        assert_eq!(
            action,
            ComposerAction::Send {
                text: "steer left".into(),
                kind: InboxKind::Note,
                mode: InjectMode::Interrupt,
            }
        );
    }

    #[test]
    fn an_empty_line_is_never_sent() {
        let mut composer = SessionComposer {
            input: "   ".into(),
            ..SessionComposer::default()
        };
        assert_eq!(
            composer.submit(&view(true, false)),
            ComposerAction::Refused("an empty message is not sent")
        );
    }

    // --- Attach as context ---------------------------------

    #[test]
    fn no_selection_attaches_the_agent_id_alone() {
        let chip = attach_as_context("r1-a-r0-a0", None);
        assert_eq!(chip.agent, "r1-a-r0-a0");
        assert_eq!(chip.selection, None);
        assert_eq!(chip.label(), "r1-a-r0-a0");
    }

    #[test]
    fn a_selection_attaches_the_agent_id_and_the_text() {
        let chip = attach_as_context("r1-a-r0-a0", Some("use an arena allocator here"));
        assert_eq!(chip.agent, "r1-a-r0-a0");
        assert_eq!(
            chip.selection.as_deref(),
            Some("use an arena allocator here")
        );
        assert!(chip.label().contains("r1-a-r0-a0"));
        assert!(chip.label().contains("use an arena allocator here"));
    }

    #[test]
    fn whitespace_only_selection_counts_as_no_selection() {
        let chip = attach_as_context("r1-a-r0-a0", Some("   \n  "));
        assert_eq!(
            chip.selection, None,
            "mutation check (tried, reverted): dropping the `.filter` here would attach an \
             empty-looking chip instead of falling back to the agent id"
        );
    }

    #[test]
    fn a_subagents_link_is_carried_on_its_parents_session_view() {
        let mut v = view(true, false);
        v.children = vec!["r1-a-r0-a0.s1".into(), "r1-a-r0-a0.s2".into()];
        // This view only carries the ids; the galaxy stage resolves them.
        assert_eq!(
            child_links(&v),
            ["r1-a-r0-a0.s1".to_owned(), "r1-a-r0-a0.s2".to_owned()]
        );
    }

    #[test]
    fn a_long_selection_is_shown_as_a_snippet_in_the_label_but_kept_whole_in_the_chip() {
        let text = "x".repeat(200);
        let chip = attach_as_context("r1-a-r0-a0", Some(&text));
        assert_eq!(chip.selection.as_deref(), Some(text.as_str()), "kept whole");
        assert!(chip.label().len() < text.len(), "the label is a snippet");
        assert!(chip.label().contains('…'));
    }
}
