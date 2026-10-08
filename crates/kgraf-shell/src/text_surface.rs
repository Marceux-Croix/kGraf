//! The one text surface (decision 0034): a panel beside the graph that renders
//! content, fed two ways, and routing back to the graph.
//!
//! - Selecting an agent (or a work node) fills it with that agent's (or that work's)
//!   conversation: the brief and every worker transcript, verbatim and unabridged
//!   (0004).
//! - A search with conversation or brief matches fills it with snippets in context.
//! - Selecting a snippet selects its node on the graph and expands the snippet into
//!   its whole conversation in place, scrolled to the turn.
//!
//! State machine (the feed):
//! ```text
//!   Empty        --select agent/work----------> Conversation(focus none)
//!   Conversation --select another / none------> Conversation / Empty
//!   any          --search with turns or briefs-> Snippets
//!   Snippets     --open a snippet-------------> Conversation(focus turn), node selected
//!   Snippets     --search without them--------> the selection's conversation, or Empty
//! ```
//! The search's own graph selection is not a user selection and does not reroute
//! the panel; the caller says which is which.
//!
//! **Virtualized.** A conversation is flattened into rows of one source line each,
//! all the same height, and only the rows in view are laid out
//! (`ScrollArea::show_rows`), so a transcript with megabytes of tool output costs a
//! frame what a short one costs. Lines are not wrapped (the panel scrolls
//! sideways), so nothing is cut or rewritten. The rows are built once per
//! conversation and corpus change, as byte ranges into the transcript's own text.
//!
//! There is one rendering of conversation content, used by both feeds (0034).

use std::sync::Arc;

use kgraf_orchestrator::StoredRun;

use crate::run_view;
use crate::search::{ConvRef, Index, Results, TurnHit, find_ci};
use crate::session_panel;
use crate::transcripts::{Corpus, Transcript, Turn, contract_of};

/// The two feeds as the "Up next" card's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Selection,
    Search,
}

/// What the panel shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Feed {
    #[default]
    Empty,
    Conversation(Conversation),
    Snippets,
}

/// A conversation: one or more worker transcripts under a title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub title: String,
    /// Briefs, verbatim, by contract.
    pub briefs: Vec<(String, String)>,
    /// (run, worker) transcripts, in attempt order.
    pub sources: Vec<(String, String)>,
    /// The turn to show expanded and scrolled to.
    pub focus: Option<ConvRef>,
    /// The lease holder this conversation is an agent's session for, when it
    /// is one (`conversation_for`'s `agent` branch). `None` for a work-node
    /// conversation (`conversation_for`'s `work` branch) and for a snippet's
    /// single-worker expansion (`TextSurface::open`): both show a work item's
    /// history, not one agent's own session, so `show_session` (design 0067) draws nothing extra
    /// for them.
    pub agent: Option<String>,
}

/// One row of a flattened conversation.
#[derive(Debug, Clone)]
enum Row {
    /// A heading or note, owned (few per conversation).
    Head(String, HeadKind),
    /// One line of a turn: source index, turn, part, byte range.
    Line {
        source: usize,
        turn: usize,
        part: usize,
        start: usize,
        end: usize,
    },
    /// One line of a brief.
    Brief {
        brief: usize,
        start: usize,
        end: usize,
    },
}

/// The rows of the conversation on screen, and what they were built from.
#[derive(Default)]
struct Rows {
    rows: Vec<Row>,
    transcripts: Vec<Option<Arc<Transcript>>>,
    /// The row of each (source, turn) that starts it, for focus.
    focus_row: Option<usize>,
    built_for: Option<(Conversation, usize)>,
}

/// The panel's state.
#[derive(Default)]
pub struct TextSurface {
    pub feed: Feed,
    /// The focus still needs scrolling to.
    scroll_pending: bool,
    /// The conversation the user's selection routed to, for returning to it.
    selection_feed: Option<Conversation>,
    rows: Rows,
    /// A long line being read in pages, if one is.
    pager: Option<Pager>,
    /// The session composer's state (design 0067): the exception route
    /// beside the orchestrator box, kept here because it must persist across
    /// frames tied to the current selection, and `TextSurface` is the shell
    /// state this unit owns (`session_panel.rs`'s module doc explains why
    /// `interaction.rs` is not touched to hold it instead).
    pub composer: crate::session_panel::SessionComposer,
    /// The session view's cached selectable rendering.
    session_cache: Option<SessionLayoutCache>,
    /// Bytes of the most recent conversation kept in the session view's
    /// selectable window; grows by `SESSION_WINDOW_CHUNK` on "Show older".
    session_window: usize,
    /// The project root a graph store was last opened (or failed to open)
    /// for. Reopened only when the root changes or, on a failure, after
    /// `GRAPH_RETRY_BACKOFF` (`graph_store_for`).
    graph_cache: Option<(std::path::PathBuf, GraphCache)>,
    /// The transcript's own selection as of the last frame it was drawn, for
    /// "Attach as context" (see `draw_session`'s comment on why this is one
    /// frame behind rather than read in the same frame the button is drawn).
    session_selection: Option<String>,
    /// The whole session card's rect, as last drawn (`session_card_rect`,
    /// tests only).
    session_card_rect: Option<egui::Rect>,
}

/// A project's graph store, or why opening it last failed (design 0067).
enum GraphCache {
    Open(kgraf_graph::GraphStore),
    Failed(std::time::Instant),
}

impl std::fmt::Debug for TextSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextSurface")
            .field("feed", &self.feed)
            .finish_non_exhaustive()
    }
}

/// A contract's brief, open or closed (verified, superseded).
pub fn brief_of(runs: &[StoredRun], run: &str, contract: &str) -> Option<String> {
    let r = run_view::records(runs).find(|r| r.run.as_str() == run)?;
    r.contract(contract)
        .map(|c| c.instruction.clone())
        .or_else(|| {
            r.coordination
                .closed
                .iter()
                .find(|c| c.contract == contract)
                .map(|c| c.instruction.clone())
        })
}

/// The conversation for a user's selection on the graph, from the run records and
/// the transcripts on disk.
pub fn conversation_for(
    agent: Option<&str>,
    work: Option<&str>,
    runs: &[StoredRun],
    corpus: &Corpus,
) -> Option<Conversation> {
    let mut briefs = Vec::new();
    let mut sources = Vec::new();
    let title;
    if let Some(holder) = agent {
        title = format!("agent {holder}");
        for (r, c) in run_view::contracts_for_holder(runs, holder) {
            briefs.push((c.contract.clone(), c.instruction.clone()));
            for w in &c.workers {
                sources.push((r.run.to_string(), w.to_string()));
            }
        }
    } else {
        let key = work?;
        let (run, contract) = key.split_once('/')?;
        title = format!("work {key}");
        if let Some(brief) = brief_of(runs, run, contract) {
            briefs.push((contract.to_owned(), brief));
        }
        // Workers from the transcripts on disk, so a verified contract (no longer
        // open in the record) still has its conversation.
        for (r, w) in corpus.transcripts.keys() {
            if r == run && contract_of(r, w).as_deref() == Some(contract) {
                sources.push((r.clone(), w.clone()));
            }
        }
    }
    Some(Conversation {
        title,
        briefs,
        sources,
        focus: None,
        agent: agent.map(str::to_owned),
    })
}

/// Bytes of conversation text one "Show older" click adds to the session
/// view's selectable window. A whole conversation can be tens of megabytes
/// (the module docs' own worked example); laying that much text out in one
/// `egui::TextEdit` galley every frame would cost exactly what the
/// virtualized main view exists to avoid costing (an earlier version drew a second,
/// separate box instead of the transcript itself; see
/// `TextSurface::draw_session`), so the window starts at the most recent
/// chunk and grows only when the user asks for more.
const SESSION_WINDOW_CHUNK: usize = 32_768;

/// Before retrying a graph store open that just failed (item 3): a
/// missing (fresh project) or briefly locked (a writer mid-commit)
/// `graph.db` is common, not exceptional, and retrying it every frame wastes
/// a syscall on a condition that has not had time to change.
const GRAPH_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

/// The look for one turn's heading line, mirroring `conversation()`'s row
/// colors (CMP-ROW-2's chat-timestamp and chat-author weights) so the
/// session view's single selectable rendering and the virtualized
/// (non-session) rendering never draw a heading two different ways. Pure
/// (`FontId` needs no `egui::Context`), so it is unit-tested without a
/// harness.
fn head_format(kind: HeadKind, p: &crate::theme::Palette) -> egui::TextFormat {
    let (font, color) = match kind {
        HeadKind::Title => (crate::theme::medium(14.0), p.text),
        HeadKind::Author => (crate::theme::medium(13.0), p.text),
        HeadKind::Note | HeadKind::Turn => (egui::FontId::proportional(11.0), p.text_3),
        // No new color: the five semantic colors (style guide 3.1) do not
        // include a meaning for "a direct message", so it is weight, not
        // color, that sets it apart, as `conversation()` already does.
        HeadKind::Direct => (crate::theme::medium(12.0), p.text),
        HeadKind::Warn => (egui::FontId::proportional(12.0), p.text_2),
        HeadKind::Error => (egui::FontId::proportional(12.0), p.danger),
    };
    egui::TextFormat {
        font_id: font,
        color,
        ..Default::default()
    }
}

/// A turn's body text format: monospace for tool traffic, the body face
/// otherwise, mirroring `conversation()`'s `Row::Line` colors.
fn body_format(tool: bool, p: &crate::theme::Palette) -> egui::TextFormat {
    egui::TextFormat {
        font_id: if tool {
            egui::FontId::monospace(12.0)
        } else {
            egui::FontId::proportional(13.0)
        },
        color: if tool { p.text_2 } else { p.text },
        ..Default::default()
    }
}

/// One `LayoutJob` for the session view's own selectable rendering:
/// every turn across `c`'s sources, oldest first, windowed to
/// the most recent whole turns that fit `window` bytes (a turn is never cut
/// mid-heading), colored exactly as `conversation()` colors the same data.
/// Returns the job and whether older turns exist beyond the window. Pure: no
/// egui context, so it is unit-tested directly.
fn session_layout_job(
    c: &Conversation,
    corpus: &Corpus,
    window: usize,
    p: &crate::theme::Palette,
) -> (egui::text::LayoutJob, bool) {
    let mut turns: Vec<&Turn> = Vec::new();
    for (run, worker) in &c.sources {
        if let Some(t) = corpus.get(run, worker) {
            turns.extend(t.turns.iter());
        }
    }
    let mut start = turns.len();
    let mut total = 0usize;
    while start > 0 {
        let (head, _) = head_of(turns[start - 1]);
        let len = head.len() + turns[start - 1].text().len();
        if total > 0 && total + len > window {
            break;
        }
        total += len;
        start -= 1;
    }
    let truncated = start > 0;
    let mut job = egui::text::LayoutJob::default();
    for t in &turns[start..] {
        let (head, kind) = head_of(t);
        let hf = head_format(kind, p);
        job.append(&head, 0.0, hf.clone());
        job.append("\n", 0.0, hf);
        let tool = matches!(t, Turn::ToolCall { .. } | Turn::ToolResult { .. });
        job.append(&t.text(), 0.0, body_format(tool, p));
        job.append("\n\n", 0.0, body_format(false, p));
    }
    (job, truncated)
}

/// The session view's cached selectable job (design 0067: no
/// full re-layout over the whole text every frame if avoidable). Rebuilt
/// only when the window size or the turn count changes -- a cheap proxy for
/// "the content changed" (turn count is coarser, and so more robust to a
/// same-length edit, than the byte length the wording allowed).
/// The clone this pays each frame it is reused (`egui::TextEdit`'s layouter
/// needs an owned `LayoutJob`) is a data copy, not a re-derivation from
/// turns and colors, and `egui::Fonts::layout_job` itself caches the actual
/// glyph shaping by the job's hash, so an unchanged job costs a rebuild of
/// neither the text nor the glyphs.
struct SessionLayoutCache {
    agent: String,
    window: usize,
    turns: usize,
    job: egui::text::LayoutJob,
    truncated: bool,
}

/// Row height: one line of the body font, plus spacing.
fn row_height(ui: &egui::Ui) -> f32 {
    ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().item_spacing.y
}

/// Pushes one row per line of `text`.
fn lines_of(text: &str, mut row: impl FnMut(usize, usize)) {
    let mut start = 0;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            row(start, i);
            start = i + 1;
        }
    }
    row(start, text.len());
}

/// What a heading row is, so its look comes from the theme when it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadKind {
    /// The conversation's title.
    Title,
    /// A note about the conversation.
    Note,
    /// A worker: drawn as a live-chat author with a neutral avatar.
    Author,
    /// A turn's heading (model, tool call): drawn like a chat timestamp.
    Turn,
    /// A message injected into this session directly (design 0067, point 2):
    /// the exception route beside the orchestrator box, labeled per 0034
    /// ("Direct message: the orchestrator sees it") so it reads differently
    /// from an ordinary model or tool turn.
    Direct,
    /// Something is missing.
    Warn,
    Error,
}

fn head_of(turn: &Turn) -> (String, HeadKind) {
    match turn {
        Turn::Assistant(_) => ("model".to_owned(), HeadKind::Turn),
        Turn::Reasoning(_) => ("reasoning".to_owned(), HeadKind::Turn),
        Turn::ToolCall { tool, call_id, .. } => {
            (format!("tool call {tool} ({call_id})"), HeadKind::Turn)
        }
        Turn::ToolResult {
            call_id, is_error, ..
        } => (
            format!(
                "{} ({call_id})",
                if *is_error {
                    "tool error"
                } else {
                    "tool result"
                }
            ),
            if *is_error {
                HeadKind::Error
            } else {
                HeadKind::Turn
            },
        ),
        Turn::Event(_) => ("event".to_owned(), HeadKind::Turn),
        // Decision 0067, point 2: a message injected outside the run's normal
        // flow, from the user or the orchestrator, or from another agent's
        // send_message. Labeled with 0034's own wording, so
        // the transcript reads the exception route the same way the composer
        // states it when the user sends one.
        Turn::Message { source, .. } => (
            format!("Direct message: the orchestrator sees it (from {source})"),
            HeadKind::Direct,
        ),
        Turn::Unreadable { .. } => ("unreadable".to_owned(), HeadKind::Error),
    }
}

impl Rows {
    fn build(&mut self, c: &Conversation, corpus: &Corpus, generation: usize) {
        if self.built_for.as_ref() == Some(&(c.clone(), generation)) {
            return;
        }
        let mut rows = Vec::new();
        let mut focus_row = None;
        rows.push(Row::Head(c.title.clone(), HeadKind::Title));
        rows.push(Row::Head(
            "Verbatim. Lines are not wrapped; scroll sideways. Times are each transcript file's last write."
                .to_owned(),
            HeadKind::Note,
        ));
        for (i, (contract, brief)) in c.briefs.iter().enumerate() {
            rows.push(Row::Head(format!("brief of {contract}"), HeadKind::Turn));
            lines_of(brief, |start, end| {
                rows.push(Row::Brief {
                    brief: i,
                    start,
                    end,
                })
            });
        }
        let mut transcripts = Vec::new();
        if c.sources.is_empty() {
            rows.push(Row::Head(
                "No worker transcript on disk yet.".to_owned(),
                HeadKind::Note,
            ));
        }
        for (s, (run, worker)) in c.sources.iter().enumerate() {
            rows.push(Row::Head(
                format!("worker {worker} (run {run})"),
                HeadKind::Author,
            ));
            let t = corpus
                .transcripts
                .get(&(run.clone(), worker.clone()))
                .cloned();
            let Some(tr) = &t else {
                rows.push(Row::Head(
                    "transcript not loaded (not on disk yet, or left out by the memory bound)"
                        .to_owned(),
                    HeadKind::Warn,
                ));
                transcripts.push(None);
                continue;
            };
            for (ti, turn) in tr.turns.iter().enumerate() {
                if c.focus
                    .as_ref()
                    .is_some_and(|f| &f.run == run && &f.worker == worker && f.turn == ti)
                {
                    focus_row = Some(rows.len());
                }
                let (head, color) = head_of(turn);
                rows.push(Row::Head(head, color));
                for (p, part) in turn.parts().iter().enumerate() {
                    // A tool call's name is already in its heading.
                    if part.is_empty() || (p == 0 && matches!(turn, Turn::ToolCall { .. })) {
                        continue;
                    }
                    lines_of(part, |start, end| {
                        rows.push(Row::Line {
                            source: s,
                            turn: ti,
                            part: p,
                            start,
                            end,
                        })
                    });
                }
            }
            transcripts.push(t);
        }
        self.rows = rows;
        self.transcripts = transcripts;
        self.focus_row = focus_row;
        self.built_for = Some((c.clone(), generation));
    }

    fn text<'a>(&'a self, row: &'a Row, c: &'a Conversation) -> &'a str {
        match row {
            Row::Head(t, _) => t,
            Row::Brief { brief, start, end } => &c.briefs[*brief].1[*start..*end],
            Row::Line {
                source,
                turn,
                part,
                start,
                end,
            } => self.transcripts[*source]
                .as_ref()
                .and_then(|t| t.turns.get(*turn))
                .map_or("", |t| &t.parts()[*part][*start..*end]),
        }
    }
}

impl TextSurface {
    /// The user selected something on the graph (or cleared the selection).
    pub fn on_selection(&mut self, conversation: Option<Conversation>) {
        self.pager = None;
        self.selection_feed = conversation.clone();
        self.feed = match conversation {
            Some(c) => Feed::Conversation(c),
            None => Feed::Empty,
        };
        self.scroll_pending = false;
    }

    /// A search ran. Snippets show when it matched conversation turns or briefs.
    pub fn on_search(&mut self, results: &Results) {
        if !results.turns.is_empty() || !results.briefs.is_empty() {
            self.feed = Feed::Snippets;
        } else if self.feed == Feed::Snippets {
            self.feed = match &self.selection_feed {
                Some(c) => Feed::Conversation(c.clone()),
                None => Feed::Empty,
            };
        }
    }

    /// A snippet was opened: expand its conversation in place, focused on it.
    pub fn open(&mut self, at: &ConvRef, work_key: &str, runs: &[StoredRun]) {
        self.pager = None;
        let contract = work_key.split_once('/').map_or(work_key, |(_, c)| c);
        let briefs = brief_of(runs, &at.run, contract)
            .map(|b| vec![(contract.to_owned(), b)])
            .unwrap_or_default();
        let c = Conversation {
            title: format!("work {work_key}, worker {}", at.worker),
            briefs,
            sources: vec![(at.run.clone(), at.worker.clone())],
            focus: Some(at.clone()),
            agent: None,
        };
        self.selection_feed = Some(c.clone());
        self.feed = Feed::Conversation(c);
        self.scroll_pending = true;
    }

    /// The "Up next" card's tab: Search while the panel shows snippets, else
    /// Selection. The tabs choose between the two feeds of the one surface; they
    /// are not a second surface (0034).
    pub fn tab(&self) -> Tab {
        match self.feed {
            Feed::Snippets => Tab::Search,
            _ => Tab::Selection,
        }
    }

    /// The user picked a tab: Selection shows the selection's conversation (or
    /// nothing), Search shows the last search's snippets.
    pub fn set_tab(&mut self, tab: Tab) {
        if tab == self.tab() {
            return;
        }
        self.pager = None;
        self.feed = match tab {
            Tab::Search => Feed::Snippets,
            Tab::Selection => match &self.selection_feed {
                Some(c) => Feed::Conversation(c.clone()),
                None => Feed::Empty,
            },
        };
    }

    /// Draws the panel. Returns the snippet opened this frame, if one was.
    /// `generation` changes when the corpus does (the rows are rebuilt then).
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        corpus: &Corpus,
        generation: usize,
        index: Option<&Index>,
        results: &Results,
        terms: &[String],
    ) -> Option<TurnHit> {
        use egui::RichText;
        let palette = crate::theme::Palette::current(ui.ctx());
        if corpus.left_out_count() > 0 {
            ui.colored_label(
                palette.danger,
                format!(
                    "{} older transcript(s) are not loaded (memory bound); search does not reach them",
                    corpus.left_out_count()
                ),
            );
        }
        if let Some(e) = &corpus.error {
            ui.colored_label(palette.danger, e);
        }
        let mut opened = None;
        match &self.feed {
            Feed::Empty => {
                ui.label(
                    RichText::new(
                        "Select an agent or a work node for its conversation, or search for snippets.",
                    )
                    .weak(),
                );
            }
            Feed::Snippets => {
                let p = crate::theme::Palette::current(ui.ctx());
                ui.label(
                    RichText::new(format!(
                        "{} matching turn(s), {} brief(s)",
                        results.turns.len(),
                        results.briefs.len()
                    ))
                    .small()
                    .color(p.text_2),
                );
                egui::ScrollArea::vertical()
                    .id_salt("text_snippets")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Some(index) = index {
                            for &i in &results.briefs {
                                let Some(b) = index.briefs().get(i) else {
                                    continue;
                                };
                                up_next_row(
                                    ui,
                                    &p,
                                    &b.key,
                                    &format!("Brief of {}", b.key),
                                    b.state,
                                    snippet_colored(&b.instruction, terms, p.suggested, p.text),
                                );
                            }
                        }
                        for hit in &results.turns {
                            let text = corpus
                                .get(&hit.at.run, &hit.at.worker)
                                .and_then(|t| t.turns.get(hit.at.turn))
                                .map(Turn::text)
                                .unwrap_or_default();
                            let head = format!("open {} turn {}", hit.at.worker, hit.at.turn);
                            if up_next_row(
                                ui,
                                &p,
                                &hit.work_key,
                                &head,
                                &hit.work_key,
                                snippet_colored(&text, terms, p.suggested, p.text),
                            )
                            .clicked()
                            {
                                opened = Some(hit.clone());
                            }
                        }
                    });
            }
            Feed::Conversation(c) => {
                let c = c.clone();
                self.conversation(ui, &c, corpus, generation);
            }
        }
        opened
    }

    fn conversation(
        &mut self,
        ui: &mut egui::Ui,
        c: &Conversation,
        corpus: &Corpus,
        generation: usize,
    ) {
        self.rows.build(c, corpus, generation);
        if let Some(pager) = &mut self.pager {
            // A line too long to lay out in one row, read in pages: still verbatim.
            let text = self
                .rows
                .rows
                .get(pager.row)
                .map_or("", |row| self.rows.text(row, c));
            let pages = text.len().div_ceil(PAGE_BYTES).max(1);
            pager.page = pager.page.min(pages - 1);
            let mut close = false;
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "line of {} bytes, page {} of {pages}",
                        text.len(),
                        pager.page + 1
                    ))
                    .strong(),
                );
                if ui
                    .add_enabled(pager.page > 0, egui::Button::new("Previous page"))
                    .clicked()
                {
                    pager.page -= 1;
                }
                if ui
                    .add_enabled(pager.page + 1 < pages, egui::Button::new("Next page"))
                    .clicked()
                {
                    pager.page += 1;
                }
                if ui.button("Back to the conversation").clicked() {
                    close = true;
                }
            });
            let (from, to) = page_bounds(text, pager.page);
            egui::ScrollArea::vertical()
                .id_salt("text_line_pager")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(&text[from..to]).monospace()).wrap(),
                    );
                });
            if close {
                self.pager = None;
            }
            return;
        }
        let h = row_height(ui);
        let mut area = egui::ScrollArea::both()
            .id_salt("text_conversation")
            .auto_shrink([false, false]);
        if self.scroll_pending
            && let Some(r) = self.rows.focus_row
        {
            area = area.vertical_scroll_offset((r as f32 * h - 3.0 * h).max(0.0));
            self.scroll_pending = false;
        } else if c.focus.is_none() {
            area = area.stick_to_bottom(true);
        }
        let focus_rows = self.rows.focus_row;
        let rows = &self.rows;
        let mut open_pager = None;
        let p = crate::theme::Palette::current(ui.ctx());
        area.show_rows(ui, h, rows.rows.len(), |ui, range| {
            for i in range {
                let row = &rows.rows[i];
                let full = rows.text(row, c);
                // One row is laid out whole, so a huge line is cut for display, with
                // the way to read all of it in pages beside it.
                let text = &full[..char_floor(full, MAX_LINE_BYTES)];
                let focused = focus_rows == Some(i);
                // Live-chat look (CMP-ROW-2): an author row
                // with a neutral avatar, turn headings like chat timestamps, the
                // model's words in the body face and tool traffic in mono.
                let rich = match row {
                    Row::Head(t, kind) => match kind {
                        HeadKind::Title => egui::RichText::new(t)
                            .font(crate::theme::medium(14.0))
                            .color(p.text),
                        HeadKind::Author => egui::RichText::new(t)
                            .font(crate::theme::medium(13.0))
                            .color(p.text),
                        HeadKind::Note | HeadKind::Turn => {
                            egui::RichText::new(t).size(11.0).color(p.text_3)
                        }
                        // The exception-route label stands out like a chat
                        // author (CMP-ROW-2's weight), not a dim timestamp:
                        // it is the one place in the transcript the user, not
                        // the run, spoke. No new color: the five semantic
                        // colors (style guide 3.1) do not include a meaning
                        // for this, so it is weight, not color, that sets it
                        // apart.
                        HeadKind::Direct => egui::RichText::new(t)
                            .font(crate::theme::medium(12.0))
                            .color(p.text),
                        HeadKind::Warn => egui::RichText::new(t).small().color(p.text_2),
                        HeadKind::Error => egui::RichText::new(t).small().color(p.danger),
                    },
                    Row::Brief { .. } => egui::RichText::new(text).size(13.0).color(p.text),
                    Row::Line { source, turn, .. } => {
                        let tool = rows.transcripts[*source]
                            .as_ref()
                            .and_then(|t| t.turns.get(*turn))
                            .is_some_and(|t| {
                                matches!(t, Turn::ToolCall { .. } | Turn::ToolResult { .. })
                            });
                        if tool {
                            egui::RichText::new(text)
                                .monospace()
                                .size(12.0)
                                .color(p.text_2)
                        } else {
                            egui::RichText::new(text).size(13.0).color(p.text)
                        }
                    }
                };
                let rich = if focused {
                    rich.background_color(p.suggested)
                } else {
                    rich
                };
                if let Row::Head(t, HeadKind::Author) = row {
                    ui.horizontal(|ui| {
                        let side = (h - 4.0).max(12.0);
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
                        let letter = t
                            .strip_prefix("worker ")
                            .and_then(|w| w.chars().next())
                            .unwrap_or('W');
                        crate::chrome::avatar(ui.painter(), rect, letter, &p, None);
                        ui.add(egui::Label::new(rich).extend());
                    });
                    continue;
                }
                if text.len() < full.len() {
                    ui.horizontal(|ui| {
                        if ui
                            .link(format!(
                                "line truncated for display ({} bytes): open in pages",
                                full.len()
                            ))
                            .clicked()
                        {
                            open_pager = Some(i);
                        }
                        ui.add(egui::Label::new(rich).extend());
                    });
                } else {
                    ui.add(egui::Label::new(rich).extend());
                }
            }
        });
        if let Some(row) = open_pager {
            self.pager = Some(Pager { row, page: 0 });
        }
    }

    /// Opens the pager on `row` (tests).
    pub fn open_pager(&mut self, row: usize) {
        self.pager = Some(Pager { row, page: 0 });
    }

    /// Draws the session furniture this unit adds around the transcript in
    /// the one text surface (design 0067): the reasoning line, the hidden-reasoning line, children
    /// as links, "Attach as context" (every session, addressable or not), and, when addressable,
    /// the composer labeled as the exception route, its mode switch, and stop/resume. Draws nothing
    /// when the current feed is not one agent's own session (a work-node conversation or a
    /// snippet's single-worker expansion have `Conversation::agent == None`).
    ///
    /// Returns what the user did this frame rather than acting on a backend
    /// directly: `SessionFrameActions`'s own doc says why (`layout.rs::up_next`
    /// is where it is applied).
    pub fn show_session(
        &mut self,
        ui: &mut egui::Ui,
        runs: &[StoredRun],
        corpus: &Corpus,
        root: Option<&std::path::Path>,
    ) -> SessionFrameActions {
        let Feed::Conversation(c) = &self.feed else {
            return SessionFrameActions::default();
        };
        let Some(agent) = c.agent.clone() else {
            return SessionFrameActions::default();
        };
        let c = c.clone();
        let graph = self.graph_store_for(root);
        let view = session_panel::session_view_for(&agent, runs, graph);
        // A miss is only worth a line when there was somewhere to read from:
        // no project open (`root: None`) is the ordinary idle state, not a
        // failure to report.
        let graph_unavailable =
            root.is_some() && matches!(&self.graph_cache, Some((_, GraphCache::Failed(_))));
        self.draw_session(ui, &agent, &view, &c, corpus, graph_unavailable)
    }

    /// A read-only handle on the project's graph store, for
    /// `session_view_for`'s `addressable` fact and `spawned`-edge children
    /// (design 0067). Reopened only when `root` changes, not every frame; a
    /// failed open is cached too and retried only after
    /// `GRAPH_RETRY_BACKOFF`, not every frame, since a missing (fresh
    /// project) or briefly locked (a writer mid-commit) `graph.db` is a
    /// common, not exceptional, state. `None` while unavailable, in which
    /// case `session_view_for` keeps its own defaults rather than
    /// fabricating either fact, and `show_session` shows a line saying so.
    fn graph_store_for(
        &mut self,
        root: Option<&std::path::Path>,
    ) -> Option<&kgraf_graph::GraphStore> {
        let root = root?;
        let stale = match &self.graph_cache {
            Some((r, GraphCache::Open(_))) => r.as_path() != root,
            Some((r, GraphCache::Failed(at))) => {
                r.as_path() != root || at.elapsed() >= GRAPH_RETRY_BACKOFF
            }
            None => true,
        };
        if stale {
            let db = kgraf_graph::snapshot::graph_db_path(root);
            let state = match kgraf_graph::GraphStore::open_read_only(&db, root) {
                Ok(store) => GraphCache::Open(store),
                Err(_) => GraphCache::Failed(std::time::Instant::now()),
            };
            self.graph_cache = Some((root.to_path_buf(), state));
        }
        match self.graph_cache.as_ref() {
            Some((_, GraphCache::Open(store))) => Some(store),
            _ => None,
        }
    }

    /// The drawing this unit adds, over an already-computed `view` (tests
    /// drive this directly with a crafted `SessionView`, since
    /// `show_session`'s own `session_view_for` always reads `addressable:
    /// true` from today's run records -- there is no resolver yet to make it
    /// `false` -- so this is where the addressable-gate wiring in the drawn
    /// UI is actually checked).
    ///
    /// **One rendering, not two**: the conversation itself is
    /// drawn here, as the selectable read-only `TextEdit`, inside the same
    /// `ScrollArea` the plain (non-session) view uses, filling the space the
    /// transcript uses today. There is no separate plain-text box, and
    /// `layout.rs::up_next` calls either this or the plain virtualized
    /// `text_surface(...)` path for a given frame, never both (see that
    /// call site's own comment).
    fn draw_session(
        &mut self,
        ui: &mut egui::Ui,
        agent: &str,
        view: &session_panel::SessionView,
        c: &Conversation,
        corpus: &Corpus,
        graph_unavailable: bool,
    ) -> SessionFrameActions {
        use egui::RichText;
        let mut actions = SessionFrameActions::default();
        let p = crate::theme::Palette::current(ui.ctx());
        let card = egui::Frame::new()
            .fill(p.additive_soft)
            .corner_radius(crate::theme::radius::CARD)
            .inner_margin(egui::Margin {
                left: 12,
                right: 12,
                top: 8,
                bottom: 8,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // A resolver's own mark (style guide CMP-AV-6): the one role the header names,
                // since no other session kind draws differently here.
                if view.kind == kgraf_core::session::SessionKind::Resolver {
                    ui.label(RichText::new("Resolver").small().strong().color(p.text_2));
                }
                // Reasoning visibility, stated as a model limit.
                if let Some(line) = session_panel::hidden_reasoning_line(
                    view.reasoning_declared,
                    view.reports_reasoning_tokens,
                ) {
                    ui.label(RichText::new(line).small().color(p.text_2));
                }
                if let Some(d) = session_panel::visibility_disagreement(
                    view.reasoning_declared,
                    view.reasoning_observed,
                ) {
                    ui.label(RichText::new(d).small().color(p.text_2));
                }
                if graph_unavailable {
                    ui.label(
                        RichText::new(
                            "The project's graph store could not be read; children and the \
                             addressable state may be stale.",
                        )
                        .small()
                        .color(p.text_2),
                    );
                }
                // Children as links. Highlighting the matching diamond on the
                // stage belongs to the galaxy stage; this
                // reroutes the one text surface to the child's own
                // conversation.
                if !view.children.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Subagents:").small().color(p.text_2));
                        for child in session_panel::child_links(view) {
                            if ui.link(child).clicked() {
                                actions.select_child = Some(child.clone());
                            }
                        }
                    });
                }
                // Footer first: the attach button, stop/resume
                // and the composer are drawn before the transcript, so they
                // claim their own real height first; the transcript then
                // fills whatever `ui.available_height()` reports afterwards,
                // which is now exact rather than a guessed reserve. This
                // also means "Attach as context" reads the *previous*
                // frame's selection (`self.session_selection`, set at the
                // end of this method, below), not the one made in this same
                // frame: a one-frame lag no drag-and-click sequence can
                // notice (dragging itself always spans several frames), and
                // the only way to give the footer priority on space without
                // a two-pass layout.
                //
                // Every session view keeps "Attach as context", addressable
                // or not (a resolver,
                // the one non-addressable case so far, gets it too).
                let attach = ui.button("Attach as context").on_hover_text(
                    "Adds a chip to the orchestrator's message box: the agent id, and the \
                     text last selected in the transcript below, if any.",
                );
                if attach.clicked() {
                    actions.attach = Some(session_panel::attach_as_context(
                        agent,
                        self.session_selection.as_deref(),
                    ));
                }
                let controls = session_panel::controls(view);
                if controls.stop_resume {
                    ui.horizontal(|ui| {
                        if ui.button("Stop").clicked() {
                            actions.stop = Some(kgraf_core::AgentId::new(agent));
                        }
                        if ui.button("Resume").clicked() {
                            actions.resume = Some(kgraf_core::AgentId::new(agent));
                        }
                    });
                }
                if !controls.composer {
                    // Read-only, no composer, no note.
                    ui.label(
                        RichText::new("Read-only: this session takes no direct input.")
                            .small()
                            .color(p.text_2),
                    );
                } else {
                    ui.separator();
                    // 0034's own wording for the exception route beside the
                    // orchestrator box.
                    ui.label(
                        RichText::new("Direct message: the orchestrator sees it")
                            .small()
                            .color(p.text_2),
                    );
                    ui.horizontal(|ui| {
                        for mode in [
                            session_panel::SendMode::AtBoundary,
                            session_panel::SendMode::Interrupt,
                        ] {
                            if crate::chrome::chip(
                                ui,
                                &p,
                                mode.label(),
                                self.composer.mode == mode,
                                None,
                            )
                            .clicked()
                            {
                                self.composer.mode = mode;
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.composer.input);
                        if ui.button("Send").clicked() {
                            match self.composer.submit(view) {
                                session_panel::ComposerAction::Send { text, kind, mode } => {
                                    actions.send =
                                        Some((kgraf_core::AgentId::new(agent), kind, mode, text));
                                }
                                session_panel::ComposerAction::RefusedRetired { text } => {
                                    actions.redirect_to_orchestrator = Some(text);
                                }
                                session_panel::ComposerAction::Refused(reason) => {
                                    actions.local_error = Some(reason.to_owned());
                                }
                            }
                        }
                    });
                    // A note to a finished agent is refused; the view
                    // offers to send it to the orchestrator instead.
                    if let Some(offer) = self.composer.redirect_offer.clone() {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(
                                    "This session has finished; a note to it is refused.",
                                )
                                .small()
                                .color(p.text_2),
                            );
                            if ui.button("Send to the orchestrator instead").clicked() {
                                actions.redirect_to_orchestrator = Some(offer);
                                self.composer.redirect_offer = None;
                            }
                        });
                    }
                }
                // The conversation itself, drawn last: a selectable,
                // read-only, colored transcript (one rendering, not a second box) that takes
                // whatever height the footer above left (a
                // shared budget, footer first). `LabelSelectionState` (what a
                // selectable `egui::Label` uses in this egui version, the
                // virtualized non-session view's own choice) exposes only
                // `has_selection`/`clear_selection`, not the selected range
                // itself. A read-only `egui::TextEdit` does expose it
                // (`TextEditOutput::cursor_range`), and it cannot edit
                // anything here: the buffer is `&str`, whose own
                // `TextBuffer` impl is immutable (`is_mutable() == false`;
                // insert and delete are no-ops), so this is drag-to-select
                // only, never drag-to-edit. Its `.layouter` supplies the same
                // colored `LayoutJob` `session_layout_job` builds (cached;
                // see `SessionLayoutCache`'s own doc), so the transcript
                // keeps its usual look (CMP-ROW-2). Nothing here touches the
                // platform clipboard or `PlatformOutput`.
                if self.session_window == 0 {
                    self.session_window = SESSION_WINDOW_CHUNK;
                }
                let turn_count = c
                    .sources
                    .iter()
                    .filter_map(|(r, w)| corpus.get(r, w))
                    .map(|t| t.turns.len())
                    .sum();
                let stale = self.session_cache.as_ref().is_none_or(|k| {
                    k.agent != agent || k.window != self.session_window || k.turns != turn_count
                });
                if stale {
                    let (job, truncated) = session_layout_job(c, corpus, self.session_window, &p);
                    self.session_cache = Some(SessionLayoutCache {
                        agent: agent.to_owned(),
                        window: self.session_window,
                        turns: turn_count,
                        job,
                        truncated,
                    });
                }
                let cache = self.session_cache.as_ref().expect("just filled above");
                if cache.truncated {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Showing the most recent {} KB of this session.",
                                self.session_window / 1024
                            ))
                            .small()
                            .color(p.text_2),
                        );
                        if ui.button("Show older").clicked() {
                            self.session_window += SESSION_WINDOW_CHUNK;
                        }
                    });
                }
                let job = cache.job.clone();
                let mut buf: &str = &cache.job.text;
                let mut layouter = move |ui: &egui::Ui, _buf: &dyn egui::TextBuffer, _wrap: f32| {
                    ui.fonts_mut(|f| f.layout_job(job.clone()))
                };
                // Whatever the footer above left, floored so a very short
                // card still shows a usable sliver rather than nothing
                // (CMP-FLD-2's minimum-not-maximum rule is for input fields,
                // not this transcript, so there is no ceiling here: a tall
                // card lets it grow to fill the space).
                let scroll_height = ui.available_height().max(80.0);
                let mut selected = None;
                egui::ScrollArea::vertical()
                    .id_salt(("session_transcript", agent))
                    .max_height(scroll_height)
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let out = egui::TextEdit::multiline(&mut buf)
                            .id_salt(("session_transcript_text", agent))
                            .layouter(&mut layouter)
                            .desired_width(ui.available_width())
                            .frame(egui::Frame::NONE)
                            .hint_text("(no transcript yet)")
                            .show(ui);
                        selected = out
                            .cursor_range
                            .and_then(|r| (!r.is_empty()).then(|| r.slice_str(buf).to_owned()));
                    });
                // For next frame's "Attach as context" (see the button's own
                // comment above, on why this is one frame behind): clearing
                // it, not just overwriting it with a new one, so a click that
                // deselects is not silently remembered as still selected.
                self.session_selection = selected;
            });
        // Kept for `session_card_rect` (tests: the required
        // check, that Send's rect lies inside this one).
        self.session_card_rect = Some(card.response.rect);
        actions
    }

    /// The whole session card's rect, as last drawn (tests: asserting a
    /// control inside it is not clipped past the card's own edge).
    #[cfg(test)]
    pub(crate) fn session_card_rect(&self) -> Option<egui::Rect> {
        self.session_card_rect
    }

    /// When the graph store was last found unopenable (tests: asserting a
    /// failed open is cached, not retried every call).
    #[cfg(test)]
    pub(crate) fn graph_cache_failed_at(&self) -> Option<std::time::Instant> {
        match &self.graph_cache {
            Some((_, GraphCache::Failed(at))) => Some(*at),
            _ => None,
        }
    }
}

/// What the user did in the session view this frame (`TextSurface::show_session`,
/// design 0067). Applying these to the real backend needs
/// `Interaction::box_and_host`, and applying `select_child` needs
/// `Interaction::on_selection`; both are reachable only from `layout.rs`
/// without touching `interaction.rs` (a file this unit does not own), so
/// `show_session` returns this instead of calling a backend itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFrameActions {
    pub send: Option<(
        kgraf_core::AgentId,
        kgraf_core::session::InboxKind,
        kgraf_core::session::InjectMode,
        String,
    )>,
    /// The composer refused a note to a retired session; this is the text
    /// to send to the orchestrator instead, if the user chose to.
    pub redirect_to_orchestrator: Option<String>,
    pub stop: Option<kgraf_core::AgentId>,
    pub resume: Option<kgraf_core::AgentId>,
    pub attach: Option<session_panel::AttachmentChip>,
    /// A child link was clicked; its id, for the caller to route the one
    /// text surface to (the galaxy stage handles the graph).
    pub select_child: Option<String>,
    /// A refusal the composer itself caught (an empty line, or a
    /// non-addressable session if a caller ignored `controls` and drew the
    /// composer anyway): shown at once, since it never reaches a backend call
    /// to report through it.
    pub local_error: Option<String>,
}

/// Longest line drawn in one row; longer lines are read in pages.
pub const MAX_LINE_BYTES: usize = 4_096;
/// Bytes per page of a long line.
pub const PAGE_BYTES: usize = 16_384;

/// The largest char boundary of `text` at or below `max`.
fn char_floor(text: &str, max: usize) -> usize {
    if text.len() <= max {
        return text.len();
    }
    let mut i = max;
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Byte bounds of page `page` of `text`, on char boundaries.
fn page_bounds(text: &str, page: usize) -> (usize, usize) {
    let from = char_floor(text, page * PAGE_BYTES);
    let to = char_floor(text, (page + 1) * PAGE_BYTES);
    (from, to)
}

/// A long line read in pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pager {
    row: usize,
    page: usize,
}
/// Chars of context each side of a match.
const CONTEXT: usize = 90;

/// The start of `text[..at]`'s last `n` chars, as a byte offset.
fn back(text: &str, at: usize, n: usize) -> usize {
    text[..at]
        .char_indices()
        .rev()
        .nth(n.saturating_sub(1))
        .map_or(0, |(i, _)| i)
}

/// The end of `text[at..]`'s first `n` chars, as a byte offset.
fn forward(text: &str, at: usize, n: usize) -> usize {
    text[at..]
        .char_indices()
        .nth(n)
        .map_or(text.len(), |(i, _)| at + i)
}

/// Thumbnail size of an up-next row.
pub const THUMB: egui::Vec2 = egui::vec2(96.0, 54.0);

/// A static graph thumbnail for `key`: a black rounded frame with a small
/// constellation seeded from the key's hash and one cyan dot for the match. It is
/// static on purpose: a live render of the galaxy per row would cost a GPU pass
/// each, and the row's job is to be recognizable, not to be a second graph.
pub fn thumbnail(painter: &egui::Painter, rect: egui::Rect, key: &str, p: &crate::theme::Palette) {
    use std::hash::{Hash, Hasher};
    painter.rect_filled(rect, crate::theme::radius::THUMB, crate::theme::STAGE_BG);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    let mut s = h.finish() | 1;
    let mut next = || {
        // xorshift64: deterministic per key, no allocation.
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s % 10_000) as f32 / 10_000.0
    };
    let inner = rect.shrink(8.0);
    let gray = p.indicator(crate::theme::Indicator::Neutral);
    let mut last = inner.center();
    for i in 0..14 {
        let at = egui::pos2(
            inner.left() + next() * inner.width(),
            inner.top() + next() * inner.height(),
        );
        if i % 3 == 0 {
            painter.line_segment(
                [last, at],
                egui::Stroke::new(0.6, gray.gamma_multiply(0.35)),
            );
        }
        painter.circle_filled(at, 1.0 + next() * 1.6, gray.gamma_multiply(0.8));
        last = at;
    }
    painter.circle_filled(last, 3.0, p.indicator(crate::theme::Indicator::Selection));
}

/// One up-next row: a thumbnail, a title, a meta line and the snippet. The whole
/// row is one click target.
fn up_next_row(
    ui: &mut egui::Ui,
    p: &crate::theme::Palette,
    key: &str,
    title: &str,
    meta: &str,
    text: egui::text::LayoutJob,
) -> egui::Response {
    let width = ui.available_width();
    let inner = ui
        .scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
            ui.set_width(width);
            ui.horizontal_top(|ui| {
                let (rect, _) = ui.allocate_exact_size(THUMB, egui::Sense::hover());
                thumbnail(ui.painter(), rect, key, p);
                ui.vertical(|ui| {
                    ui.set_width((width - THUMB.x - 12.0).max(60.0));
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(title)
                                .font(crate::theme::medium(14.0))
                                .color(p.text),
                        )
                        .truncate(),
                    );
                    ui.add(
                        egui::Label::new(egui::RichText::new(meta).small().color(p.text_2))
                            .truncate(),
                    );
                    ui.add(egui::Label::new(text).wrap());
                });
            });
        })
        .response;
    if inner.hovered() {
        ui.painter().rect_filled(
            inner.rect.expand(4.0),
            crate::theme::radius::THUMB,
            p.additive_soft,
        );
    }
    ui.add_space(8.0);
    inner.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The part of `text` around the first match of any of `terms` (lowercase), with
/// the match highlighted, found case-insensitively in place ([`find_ci`]). A match
/// whose offset lowercasing would move (a non-ASCII term) shows the turn's start.
pub fn snippet(text: &str, terms: &[String]) -> egui::text::LayoutJob {
    let p = crate::theme::Palette::dark();
    snippet_colored(text, terms, p.suggested, p.text)
}

/// [`snippet`] with the theme's match highlight and text color.
pub fn snippet_colored(
    text: &str,
    terms: &[String],
    highlight: egui::Color32,
    color: egui::Color32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let plain = egui::TextFormat {
        color,
        font_id: crate::theme::regular(13.0),
        ..Default::default()
    };
    let hit = egui::TextFormat {
        background: highlight,
        ..plain.clone()
    };
    let found = terms
        .iter()
        .filter(|t| !t.is_empty())
        .filter_map(|t| match find_ci(text, t) {
            Some(Some(i)) => Some((i, i + t.len())),
            _ => None,
        })
        .min();
    match found {
        Some((a, b)) if text.is_char_boundary(a) && text.is_char_boundary(b) => {
            let from = back(text, a, CONTEXT);
            let to = forward(text, b, CONTEXT);
            let lead = if from > 0 { "..." } else { "" };
            let tail = if to < text.len() { "..." } else { "" };
            job.append(&format!("{lead}{}", &text[from..a]), 0.0, plain.clone());
            job.append(&text[a..b], 0.0, hit);
            job.append(&format!("{}{tail}", &text[b..to]), 0.0, plain);
        }
        _ => {
            let to = forward(text, 0, CONTEXT * 2);
            let tail = if to < text.len() { "..." } else { "" };
            job.append(&format!("{}{tail}", &text[..to]), 0.0, plain);
        }
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::session::ReasoningVisibility;
    use session_panel::SessionView;

    fn view(addressable: bool) -> SessionView {
        SessionView {
            id: "r1-a-r0-a0".into(),
            kind: kgraf_core::session::SessionKind::Worker,
            addressable,
            retired: false,
            reasoning_declared: ReasoningVisibility::Full,
            reasoning_observed: ReasoningVisibility::Unknown,
            reports_reasoning_tokens: true,
            children: vec!["r1-a-r0-a0.s1".into()],
        }
    }

    /// Pure (no egui): the shared color mapping and windowing
    /// `session_layout_job` builds the selectable session view from,
    /// isolated from the layouter/`TextEdit` plumbing.
    #[test]
    fn session_layout_job_joins_turns_and_windows_to_whole_recent_turns() {
        let p = crate::theme::Palette::dark();
        let (c, corpus) = session_conversation_of(vec![
            Turn::Assistant("first".into()),
            Turn::Assistant("second".into()),
        ]);
        let (job, truncated) = session_layout_job(&c, &corpus, SESSION_WINDOW_CHUNK, &p);
        assert!(job.text.contains("first") && job.text.contains("second"));
        assert!(!truncated);

        // A turn bigger than the window is still whole: windowing drops
        // whole turns, never cuts mid-turn.
        let long = "x".repeat(SESSION_WINDOW_CHUNK + 100);
        let (c2, corpus2) = session_conversation_of(vec![
            Turn::Assistant("dropped, too old".into()),
            Turn::Assistant(long.clone()),
        ]);
        let (job2, truncated2) = session_layout_job(&c2, &corpus2, SESSION_WINDOW_CHUNK, &p);
        assert!(truncated2, "the older turn does not fit and is dropped");
        assert!(job2.text.contains(&long), "the newest turn is kept whole");
        assert!(!job2.text.contains("dropped, too old"));
    }

    /// One transcript, one turn, for the drawn-UI tests below: a real
    /// `Corpus` and `Conversation` so `draw_session` reads the same data path
    /// production does, not a shortcut string.
    fn session_conversation_of(turns: Vec<Turn>) -> (Conversation, Corpus) {
        let mut corpus = Corpus::default();
        corpus.transcripts.insert(
            ("r1".into(), "r1-a-r0-a0".into()),
            Arc::new(Transcript::for_test(
                "r1",
                "r1-a-r0-a0",
                "w.jsonl".into(),
                turns,
            )),
        );
        let c = Conversation {
            title: "agent r1/a".into(),
            briefs: vec![],
            sources: vec![("r1".into(), "r1-a-r0-a0".into())],
            focus: None,
            agent: Some("r1/a".into()),
        };
        (c, corpus)
    }

    fn session_conversation(text: &str) -> (Conversation, Corpus) {
        session_conversation_of(vec![Turn::Assistant(text.to_owned())])
    }

    fn harness(
        surface: TextSurface,
        v: SessionView,
        c: Conversation,
        corpus: Corpus,
    ) -> egui_kittest::Harness<'static, (TextSurface, SessionFrameActions)> {
        let mut h = egui_kittest::Harness::new_ui_state(
            move |ui, (s, actions): &mut (TextSurface, SessionFrameActions)| {
                if crate::theme::ensure(ui.ctx()) {
                    // `Harness::run` steps several frames until stable; a click's
                    // input event is consumed on the first step that sees it, so a
                    // later step in the same `run()` redraws with `.clicked() ==
                    // false` and would return a fresh, empty `SessionFrameActions`.
                    // Accumulate instead of overwriting, so the frame that actually
                    // saw the click is not clobbered by the steady-state frame
                    // after it (the real app only ever reads one frame's result,
                    // so this merging is a test-harness concern, not a production
                    // one).
                    let this_frame = s.draw_session(ui, "r1/a", &v, &c, &corpus, false);
                    actions.send = this_frame.send.or(actions.send.take());
                    actions.redirect_to_orchestrator = this_frame
                        .redirect_to_orchestrator
                        .or(actions.redirect_to_orchestrator.take());
                    actions.stop = this_frame.stop.or(actions.stop.take());
                    actions.resume = this_frame.resume.or(actions.resume.take());
                    actions.attach = this_frame.attach.or(actions.attach.take());
                    actions.select_child = this_frame.select_child.or(actions.select_child.take());
                    actions.local_error = this_frame.local_error.or(actions.local_error.take());
                }
            },
            (surface, SessionFrameActions::default()),
        );
        // The default 800x600 (this must stay the real
        // canvas the bug was found at, not widened to hide it): the footer
        // is drawn before the transcript now, so it claims its own real
        // height first regardless of how much room is left over for the
        // transcript below it.
        h.run();
        h
    }

    /// A non-addressable session, in the drawn UI (not only the model,
    /// `session_panel`'s own test of `controls`): addressable shows the
    /// composer and stop/resume; not addressable shows neither. A mutation
    /// that always shows the composer is caught by the second half of this
    /// test (see the report for the executed mutation and its revert).
    #[test]
    fn addressable_gates_the_composer_and_stop_resume_in_the_drawn_ui() {
        use egui_kittest::kittest::Queryable;
        let (c, corpus) = session_conversation("hi");
        let h = harness(
            TextSurface::default(),
            view(true),
            c.clone(),
            corpus.clone(),
        );
        h.get_by_label("Send");
        h.get_by_label("Stop");
        h.get_by_label("Resume");
        h.get_by_label_contains("Direct message: the orchestrator sees it");

        let h = harness(TextSurface::default(), view(false), c, corpus);
        assert!(h.query_by_label("Send").is_none(), "no composer");
        assert!(h.query_by_label("Stop").is_none(), "no stop control");
        assert!(h.query_by_label("Resume").is_none(), "no resume control");
        h.get_by_label_contains("Read-only");
        // The control every session view keeps, addressable or not (request
        // 18's own text: "resolvers included").
        h.get_by_label("Attach as context");
    }

    /// At the harness's default 800x600 canvas, Send must
    /// be laid out inside the card's own visible rect, not clipped past its
    /// bottom edge. The footer is drawn before the transcript
    /// (`draw_session`'s own comment on why), so this holds regardless of
    /// how much room the transcript is left with.
    #[test]
    fn send_stays_inside_the_cards_visible_rect() {
        use egui_kittest::kittest::Queryable;
        let (c, corpus) = session_conversation("hi");
        let h = harness(TextSurface::default(), view(true), c, corpus);
        let card = h.state().0.session_card_rect().expect("the card was drawn");
        let send = h.get_by_label("Send").rect();
        assert!(
            card.contains_rect(send),
            "Send {send:?} must lie inside the card {card:?}"
        );
    }

    /// Exactly one conversation widget in the session view, never
    /// a second box alongside it (SR "no second place to look").
    #[test]
    fn the_session_view_has_exactly_one_conversation_widget() {
        use egui_kittest::kittest::Queryable;
        let (c, corpus) = session_conversation("hi");
        let h = harness(TextSurface::default(), view(true), c, corpus);
        let count = h
            .get_all_by_role(egui::accesskit::Role::MultilineTextInput)
            .count();
        assert_eq!(count, 1, "one conversation widget, not two");
    }

    /// "Attach as context", in the drawn UI end to end, with no clipboard
    /// involved at any point : clicking the
    /// control with nothing selected attaches the agent id alone.
    #[test]
    fn clicking_attach_as_context_with_no_selection_attaches_the_agent_id_alone() {
        use egui_kittest::kittest::Queryable;
        let (c, corpus) = session_conversation("hi");
        let mut h = harness(TextSurface::default(), view(true), c, corpus);
        h.get_by_label("Attach as context").click();
        h.run();
        assert_eq!(
            h.state().1.attach,
            Some(session_panel::AttachmentChip {
                agent: "r1/a".into(),
                selection: None,
            }),
            "no selection: the agent id alone"
        );
        assert_no_copy_text_was_issued(&h);
    }

    /// A real press, drag and release
    /// over the session's own conversation (the transcript itself, not a
    /// second box) selects text, and clicking Attach carries it. No
    /// `OutputCommand::CopyText` is issued in any frame: nothing here
    /// may touch the system clipboard.
    #[test]
    fn dragging_a_selection_over_the_session_transcript_and_clicking_attach_carries_it() {
        use egui_kittest::kittest::Queryable;
        let full = "use an arena allocator";
        let (c, corpus) = session_conversation(full);
        // The exact text the drag is expected to select: the same job
        // production builds, not the raw turn text (it also carries the
        // turn's heading line).
        let p = crate::theme::Palette::dark();
        let (expected_job, _) = session_layout_job(&c, &corpus, SESSION_WINDOW_CHUNK, &p);
        let mut h = harness(TextSurface::default(), view(true), c, corpus);
        let field = h
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .rect();
        // A drag from before the first character to well past the last row
        // and column selects the whole buffer: robust to font metrics and
        // to the exact number of lines, unlike aiming at a character.
        let start = egui::pos2(field.left() + 1.0, field.top() + 1.0);
        let end = egui::pos2(field.right() + 200.0, field.bottom() + 200.0);
        // `Harness::output()` only exposes the most recent step's output, and
        // `run()` steps several times internally to reach a stable frame, so
        // this checks after every single `step()` (one egui pass each), not
        // only after each `run()`: a `CopyText` issued on an intermediate
        // step and gone by the final steady one must still fail this.
        h.hover_at(start);
        h.step();
        h.drag_at(start);
        h.step();
        assert_no_copy_text_was_issued(&h);
        for i in 1..=10 {
            let t = i as f32 / 10.0;
            h.hover_at(start.lerp(end, t));
            h.step();
            assert_no_copy_text_was_issued(&h);
        }
        h.drop_at(end);
        h.step();
        assert_no_copy_text_was_issued(&h);
        h.get_by_label("Attach as context").click();
        h.run();
        // The drag overshoots past the last character, so the selection may
        // or may not include the job's own trailing blank line; only the
        // meaningful text is asserted.
        let selection = h
            .state()
            .1
            .attach
            .clone()
            .expect("attached")
            .selection
            .expect("a selection, not the agent id alone");
        assert_eq!(selection.trim_end(), expected_job.text.trim_end());
        assert_no_copy_text_was_issued(&h);
    }

    /// Nothing in this path may write to
    /// the system clipboard, drag-select included.
    fn assert_no_copy_text_was_issued(
        h: &egui_kittest::Harness<'_, (TextSurface, SessionFrameActions)>,
    ) {
        let issued = h
            .output()
            .platform_output
            .commands
            .iter()
            .any(|c| matches!(c, egui::OutputCommand::CopyText(_)));
        assert!(!issued, "no path here may write to the system clipboard");
    }

    /// "Its children as links that select their nodes" (design 0067): the
    /// text surface's own half of that (the galaxy stage highlights the
    /// graph diamond). Clicking a child link sets `select_child`, for
    /// `layout.rs::up_next` to route the one text surface to.
    #[test]
    fn clicking_a_child_link_sets_select_child() {
        use egui_kittest::kittest::Queryable;
        let (c, corpus) = session_conversation("hi");
        let mut h = harness(TextSurface::default(), view(true), c, corpus);
        h.get_by_label("r1-a-r0-a0.s1").click();
        h.run();
        assert_eq!(h.state().1.select_child.as_deref(), Some("r1-a-r0-a0.s1"));
    }

    /// The composer's mode switch and Send button reach
    /// `SessionFrameActions::send` with the chosen mode (design 0067, point
    /// 3): the control-to-action wiring, not only `SessionComposer::submit`
    /// (already tested in `session_panel.rs` against the model alone).
    #[test]
    fn sending_a_direct_message_carries_the_chosen_mode() {
        use egui_kittest::kittest::Queryable;
        let (c, corpus) = session_conversation("hi");
        let mut h = harness(TextSurface::default(), view(true), c, corpus);
        h.get_by_label("Interrupt now").click();
        h.run();
        // Two singleline text inputs no longer exist here (the transcript is
        // now multiline), so this still finds only the composer's field.
        h.get_by_role(egui::accesskit::Role::TextInput).focus();
        h.run();
        h.get_by_role(egui::accesskit::Role::TextInput)
            .type_text("steer left");
        h.run();
        h.get_by_label("Send").click();
        h.run();
        let (session, kind, mode, text) = h.state().1.send.clone().expect("sent");
        assert_eq!(session.as_str(), "r1/a");
        assert_eq!(kind, kgraf_core::session::InboxKind::Note);
        assert_eq!(mode, kgraf_core::session::InjectMode::Interrupt);
        assert_eq!(text, "steer left");
    }

    fn text_of(job: &egui::text::LayoutJob) -> (String, String) {
        let hit: String = job
            .sections
            .iter()
            .filter(|s| s.format.background != egui::Color32::TRANSPARENT)
            .map(|s| job.text[s.byte_range.start.0..s.byte_range.end.0].to_owned())
            .collect();
        (job.text.clone(), hit)
    }

    #[test]
    fn a_snippet_shows_the_match_in_context_and_highlights_it() {
        let text = format!(
            "{}We chose the Arena allocator here{}",
            "x".repeat(200),
            "y".repeat(200)
        );
        let job = snippet(&text, &["arena".to_owned()]);
        let (all, hit) = text_of(&job);
        assert_eq!(hit, "Arena", "the original spelling, highlighted");
        assert!(all.starts_with("...") && all.ends_with("..."));
        assert!(all.len() < text.len());
        // No match: the start of the turn.
        let job = snippet("short text", &["zzz".to_owned()]);
        assert_eq!(text_of(&job), ("short text".to_owned(), String::new()));
    }

    #[test]
    fn the_feed_routes_between_selection_search_and_snippet() {
        let conv = |t: &str| Conversation {
            title: t.into(),
            briefs: vec![],
            sources: vec![],
            focus: None,
            agent: None,
        };
        let mut s = TextSurface::default();
        s.on_selection(Some(conv("agent a")));
        assert!(matches!(&s.feed, Feed::Conversation(c) if c.title == "agent a"));
        let at = ConvRef {
            run: "r1".into(),
            worker: "r1-a-r0-a0".into(),
            turn: 4,
        };
        let with_turns = Results {
            turns: vec![TurnHit {
                at: at.clone(),
                work_key: "r1/a".into(),
            }],
            ..Results::default()
        };
        s.on_search(&with_turns);
        assert_eq!(s.feed, Feed::Snippets, "search fills the same panel");
        // A search without conversation matches returns to the selection.
        s.on_search(&Results::default());
        assert!(matches!(&s.feed, Feed::Conversation(c) if c.title == "agent a"));
        s.on_search(&with_turns);
        s.open(&at, "r1/a", &[]);
        let Feed::Conversation(c) = &s.feed else {
            panic!("{:?}", s.feed)
        };
        assert_eq!(
            c.focus.as_ref(),
            Some(&at),
            "expanded in place, focused on the turn"
        );
        assert_eq!(c.sources, [("r1".to_owned(), "r1-a-r0-a0".to_owned())]);
        s.on_selection(None);
        assert_eq!(s.feed, Feed::Empty);
    }

    /// A single 1 MB line is drawn cut to [`MAX_LINE_BYTES`] with the way to open
    /// it, and read whole in pages: nothing verbatim is lost. Measured and printed.
    #[test]
    fn a_one_megabyte_line_is_capped_and_readable_in_pages() {
        use egui_kittest::kittest::Queryable;
        let line: String = "0123456789abcdef".repeat(65_536); // 1 MiB, no newline
        let mut corpus = Corpus::default();
        corpus.transcripts.insert(
            ("r1".into(), "w".into()),
            Arc::new(Transcript::for_test(
                "r1",
                "w",
                "w.jsonl".into(),
                vec![Turn::ToolResult {
                    call_id: "t".into(),
                    output: line.clone(),
                    is_error: false,
                }],
            )),
        );
        let conv = Conversation {
            title: "work r1/a".into(),
            briefs: vec![],
            sources: vec![("r1".into(), "w".into())],
            focus: None,
            agent: None,
        };
        let mut surface = TextSurface::default();
        surface.on_selection(Some(conv));
        let corpus = Arc::new(corpus);
        let c2 = corpus.clone();
        let mut h = egui_kittest::Harness::new_ui_state(
            move |ui, s: &mut TextSurface| {
                if crate::theme::ensure(ui.ctx()) {
                    s.show(ui, &c2, 1, None, &Results::default(), &[]);
                }
            },
            surface,
        );
        h.run();
        let t0 = std::time::Instant::now();
        h.step();
        let steady = t0.elapsed();
        eprintln!("1 MiB line: steady frame {steady:?} (drawn cut to {MAX_LINE_BYTES} bytes)");
        h.get_by_label("line truncated for display (1048576 bytes): open in pages")
            .click();
        h.run();
        h.get_by_label_contains("page 1 of 64");
        h.get_by_label("Next page").click();
        h.run();
        h.get_by_label_contains("page 2 of 64");
        assert!(steady < std::time::Duration::from_millis(500), "{steady:?}");
    }

    /// Only the rows in view are laid out, whatever the transcript's size: a
    /// transcript of 2,000 turns with 20 KB tool outputs each (40 MB) costs a
    /// frame like a short one. Measured and printed.
    #[test]
    fn a_huge_conversation_is_virtualized() {
        let output: String = (0..400)
            .map(|i| format!("line {i} of a large tool output, padded to fifty bytes\n"))
            .collect();
        let turns: Vec<Turn> = (0..2_000)
            .map(|i| Turn::ToolResult {
                call_id: format!("t{i}"),
                output: output.clone(),
                is_error: false,
            })
            .collect();
        let mut corpus = Corpus::default();
        corpus.transcripts.insert(
            ("r1".into(), "w".into()),
            Arc::new(Transcript::for_test("r1", "w", "w.jsonl".into(), turns)),
        );
        let conv = Conversation {
            title: "work r1/a".into(),
            briefs: vec![],
            sources: vec![("r1".into(), "w".into())],
            focus: None,
            agent: None,
        };
        let mut surface = TextSurface::default();
        surface.on_selection(Some(conv));
        let ctx = egui::Context::default();
        crate::theme::install(
            &ctx,
            crate::settings::Appearance::Dark,
            kgraf_core::palette::Vision::default(),
        );
        let frame = |surface: &mut TextSurface| {
            let t0 = std::time::Instant::now();
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(400.0, 800.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    surface.show(ui, &corpus, 1, None, &Results::default(), &[]);
                },
            );
            out.textures_delta.clear();
            let labels = out.shapes.len();
            (t0.elapsed(), labels)
        };
        let (first, _) = frame(&mut surface);
        let (steady, shapes) = frame(&mut surface);
        eprintln!(
            "40 MB conversation, {} rows: first frame {first:?} (builds rows), steady frame {steady:?}, {shapes} shapes",
            surface.rows.rows.len()
        );
        assert!(surface.rows.rows.len() > 800_000);
        assert!(shapes < 400, "only the visible rows are drawn: {shapes}");
    }

    /// Design 0067: a long session (100+ turns) does not
    /// rebuild its `LayoutJob` from the turns every frame -- only when the
    /// window or the turn count changes (`SessionLayoutCache`'s own doc
    /// explains the tradeoff). The steady frame here changes neither, so it
    /// reuses the cached job; only the first frame pays for building it.
    /// Measured and printed.
    #[test]
    fn a_long_session_reuses_its_cached_layout_job_on_steady_frames() {
        let turns: Vec<Turn> = (0..150)
            .map(|i| Turn::Assistant(format!("turn {i} body text, padded a little for realism")))
            .collect();
        let (c, corpus) = session_conversation_of(turns);
        let mut surface = TextSurface::default();
        let v = view(true);
        let ctx = egui::Context::default();
        crate::theme::install(
            &ctx,
            crate::settings::Appearance::Dark,
            kgraf_core::palette::Vision::Standard,
        );
        let frame = |surface: &mut TextSurface| {
            let t0 = std::time::Instant::now();
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 1200.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    surface.draw_session(ui, "r1/a", &v, &c, &corpus, false);
                },
            );
            out.textures_delta.clear();
            t0.elapsed()
        };
        let first = frame(&mut surface);
        assert!(
            surface.session_cache.is_some(),
            "the first frame builds and keeps the cache"
        );
        let cached_before = surface.session_cache.as_ref().map(|k| k.turns);
        let steady = frame(&mut surface);
        assert_eq!(
            surface.session_cache.as_ref().map(|k| k.turns),
            cached_before,
            "the cache key is unchanged: the steady frame did not rebuild it"
        );
        eprintln!(
            "150-turn session: first frame {first:?} (builds the layout job), \
             steady frame {steady:?} (reuses the cached job)"
        );
        assert!(steady < std::time::Duration::from_millis(200), "{steady:?}");
    }

    /// A missing (or locked) `graph.db` is a failure worth
    /// caching, not retrying every call -- the second call within
    /// `GRAPH_RETRY_BACKOFF` must not attempt to reopen it (a fresh
    /// `Instant` would mean it did).
    #[test]
    fn a_failed_graph_open_is_cached_and_not_retried_within_the_backoff() {
        let mut s = TextSurface::default();
        let dir = tempfile::tempdir().expect("dir");
        // No `.kgraf/graph.db` under this root: the open fails.
        assert!(s.graph_store_for(Some(dir.path())).is_none());
        let first = s.graph_cache_failed_at().expect("cached as failed");
        assert!(s.graph_store_for(Some(dir.path())).is_none());
        let second = s.graph_cache_failed_at().expect("still failed");
        assert_eq!(
            first, second,
            "the same failure, not a fresh attempt, within the backoff window"
        );
    }

    /// The session view says so when the graph store cannot be read, rather
    /// than silently showing stale or default children/addressable facts
    ///.
    #[test]
    fn show_session_names_an_unreadable_graph_store_in_the_drawn_ui() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().expect("dir");
        let (c, corpus) = session_conversation("hi");
        let mut surface = TextSurface::default();
        surface.on_selection(Some(c));
        let root = dir.path().to_path_buf();
        let mut h = egui_kittest::Harness::new_ui_state(
            move |ui, s: &mut TextSurface| {
                if crate::theme::ensure(ui.ctx()) {
                    let _ = s.show_session(ui, &[], &corpus, Some(&root));
                }
            },
            surface,
        );
        h.run();
        h.get_by_label_contains("could not be read");
    }
}
