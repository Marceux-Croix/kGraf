//! Search (decision 0032): one query language over everything indexed, whose result
//! is a selection on the graph and snippets in the one text surface (0034), never a
//! results view.
//!
//! **Query.** Plain text, operators typed inline, Gmail style. Terms are ANDed.
//! `"quoted words"` is one phrase. Operators:
//!
//! | Operator | Matches |
//! |---|---|
//! | `agent:` | agents by holder id, and their conversations |
//! | `symbol:` | code by name or qualified name |
//! | `file:` | code by path (files included) |
//! | `task:` / `contract:` | work nodes by id, label or brief |
//! | `touched:` | agents and work nodes that held or touched a path |
//! | `status:` | work nodes by derived state (`done`, `blocked`, `held`, `cap`, ...) |
//! | `vendor:` / `model:` | work nodes by the route that ran them |
//! | `before:` / `after:` | conversation turns by time (`YYYY-MM-DD`) |
//!
//! A word such as `std::fs` or `http://x` is text, not an operator: only the names
//! above are operators. A malformed value (`before:tomorrow`) is an error the field
//! shows; the query is never silently narrowed.
//!
//! **Ranking** (0032: structural boost, recency, focus). Code ranking is ported from
//! codebase-memory-mcp `src/mcp/mcp.c` (the `search_graph` BM25 query) at commit
//! 64c23fab6c3eba3de226355c0ca1ff750116edab (MIT, `third_party/codebase-memory-mcp/
//! UPSTREAM`): an exact-name tier (the name spelled exactly +30, case-insensitively
//! +20), then kind tiers (functions and methods +10, type-like labels +5: here
//! classes, interfaces, structs, enums, traits and type aliases), with files
//! excluded from bare-text results. Upstream also excludes folders, variables and
//! projects, which kGraf's graph does not have, and no longer excludes modules
//! and sections. Deviation: upstream ranks the rest by BM25 over an FTS5 index;
//! here a fixed match tier (prefix, camelCase word, substring of name, qualified
//! name, path) stands in, because the graph store has no FTS table. Names split on
//! camelCase and `_`, as upstream's tokenizer does. Conversation turns rank by
//! recency when they carry a time. Focus (preferring the neighborhood being looked
//! at) is not built.
//!
//! The index is plain vectors rebuilt when the model or the runs change; a query is
//! a linear scan. Measured in `tests` below and in the report: well under a frame
//! at 10,000 code nodes in a release build.

use std::collections::HashMap;

use kgraf_galaxy::ProjectModel;
use kgraf_graph::NodeKind;
use kgraf_orchestrator::{Closure, StoredRun};
use std::sync::Arc;

use crate::transcripts::Transcript;

/// An operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Agent,
    Symbol,
    File,
    Task,
    Touched,
    Status,
    Route,
    Before,
    After,
}

const OPS: &[(&str, Op)] = &[
    ("agent", Op::Agent),
    ("symbol", Op::Symbol),
    ("file", Op::File),
    ("task", Op::Task),
    ("contract", Op::Task),
    ("touched", Op::Touched),
    ("status", Op::Status),
    ("vendor", Op::Route),
    ("model", Op::Route),
    ("before", Op::Before),
    ("after", Op::After),
];

/// One term: text (lowercased), or an operator and its value (lowercased).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Text(String),
    Op(Op, String),
}

/// A parsed query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub terms: Vec<Term>,
    /// The text terms as typed, for the exact-spelling tier.
    pub spelled: Vec<String>,
    /// Epoch milliseconds, exclusive.
    pub before: Option<i64>,
    /// Epoch milliseconds, inclusive.
    pub after: Option<i64>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.before.is_none() && self.after.is_none()
    }

    fn has_op(&self, op: Op) -> bool {
        self.terms
            .iter()
            .any(|t| matches!(t, Term::Op(o, _) if *o == op))
    }
}

/// A query the field cannot run. Each is shown in the field, and the query is not
/// run in any narrower form.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    #[error("`{0}:` needs a value")]
    EmptyValue(String),
    #[error("`{op}:{value}` is not a date; write YYYY-MM-DD")]
    BadDate { op: String, value: String },
    #[error("a quote is not closed")]
    OpenQuote,
    #[error(
        "`{0}:` is not an operator; operators are agent, symbol, file, task, contract, touched, status, vendor, model, before, after"
    )]
    UnknownOperator(String),
    #[error("`status:{0}` is not a state; states are {STATES}")]
    BadStatus(String),
    #[error("`{0}:` is given twice")]
    Duplicate(String),
    #[error("`{0}` is inside quotes, where operators do not work; take it out of the quotes")]
    QuotedOperator(String),
}

/// The words `status:` takes.
pub const STATES: &str = "done, running, blocked, held, cap, lease, waiting, planned, gate";
const STATE_WORDS: &[&str] = &[
    "done",
    "running",
    "in-flight",
    "inflight",
    "blocked",
    "held",
    "cap",
    "capped",
    "held-by-cap",
    "lease",
    "colliding",
    "waiting",
    "planned",
    "gate",
];

/// Splits on whitespace, keeping `"quoted runs"` whole (quotes removed). Each
/// token says whether it was quoted.
fn tokens(input: &str) -> Result<Vec<(String, bool)>, SearchError> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    for ch in input.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                was_quoted = true;
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push((std::mem::take(&mut cur), was_quoted));
                }
                was_quoted = false;
            }
            c => cur.push(c),
        }
    }
    if quoted {
        return Err(SearchError::OpenQuote);
    }
    if !cur.is_empty() {
        out.push((cur, was_quoted));
    }
    Ok(out)
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's `days_from_civil`, public
/// domain algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

fn parse_date(op: &str, value: &str) -> Result<i64, SearchError> {
    let bad = || SearchError::BadDate {
        op: op.to_owned(),
        value: value.to_owned(),
    };
    let mut parts = value.split('-');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad());
    };
    let (y, m, d): (i64, i64, i64) = (
        y.parse().map_err(|_| bad())?,
        m.parse().map_err(|_| bad())?,
        d.parse().map_err(|_| bad())?,
    );
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return Err(bad());
    }
    Ok(days_from_civil(y, m, d) * 86_400_000)
}

/// Whether `tok` has the shape of an operator: a word of letters, `:`, and a
/// value that does not start with `:` or `/` (so `std::fs` and `http://x` stay
/// text).
fn operator_shape(tok: &str) -> Option<(&str, &str)> {
    let (name, value) = tok.split_once(':')?;
    let shaped = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphabetic())
        && !value.starts_with(':')
        && !value.starts_with('/');
    shaped.then_some((name, value))
}

/// Parse a query.
pub fn parse(input: &str) -> Result<Query, SearchError> {
    let mut q = Query::default();
    for (tok, quoted) in tokens(input)? {
        if quoted {
            // A phrase is text; an operator inside one would silently be text.
            if let Some(word) = tok.split_whitespace().find(|w| {
                operator_shape(w)
                    .is_some_and(|(n, _)| OPS.iter().any(|(o, _)| *o == n.to_ascii_lowercase()))
            }) {
                return Err(SearchError::QuotedOperator(word.to_owned()));
            }
            q.terms.push(Term::Text(tok.to_lowercase()));
            q.spelled.push(tok);
            continue;
        }
        let Some((name, value)) = operator_shape(&tok) else {
            q.terms.push(Term::Text(tok.to_lowercase()));
            q.spelled.push(tok);
            continue;
        };
        let lname = name.to_ascii_lowercase();
        let Some(&(op_name, op)) = OPS.iter().find(|(n, _)| *n == lname) else {
            return Err(SearchError::UnknownOperator(name.to_owned()));
        };
        if value.is_empty() {
            return Err(SearchError::EmptyValue(op_name.to_owned()));
        }
        match op {
            Op::Before if q.before.is_some() => {
                return Err(SearchError::Duplicate(op_name.to_owned()));
            }
            Op::After if q.after.is_some() => {
                return Err(SearchError::Duplicate(op_name.to_owned()));
            }
            Op::Before => q.before = Some(parse_date(op_name, value)?),
            Op::After => q.after = Some(parse_date(op_name, value)?),
            Op::Status => {
                let v = value.to_lowercase();
                if !STATE_WORDS.contains(&v.as_str()) {
                    return Err(SearchError::BadStatus(value.to_owned()));
                }
                q.terms.push(Term::Op(op, v));
            }
            _ => q.terms.push(Term::Op(op, value.to_lowercase())),
        }
    }
    Ok(q)
}

/// A name's words: split on camelCase, `_`, `-`, `.`, `:` and `/`, lowercased.
pub fn words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in name.chars() {
        if !ch.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = ch.is_lowercase() || ch.is_ascii_digit();
        cur.extend(ch.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Kind tier, ported (see the module docs). `None`: excluded from bare text.
fn kind_boost(kind: NodeKind) -> Option<i32> {
    match kind {
        NodeKind::Function | NodeKind::Method => Some(10),
        NodeKind::Class
        | NodeKind::Interface
        | NodeKind::Struct
        | NodeKind::Enum
        | NodeKind::Trait
        | NodeKind::TypeAlias => Some(5),
        NodeKind::File => None,
        NodeKind::Module
        | NodeKind::Impl
        | NodeKind::Macro
        | NodeKind::Constant
        | NodeKind::Decorated
        | NodeKind::Section => Some(0),
    }
}

struct CodeEntry {
    slot: u32,
    /// The name as written, for the exact-spelling tier.
    spelled: String,
    name: String,
    qual: String,
    path: String,
    words: Vec<String>,
    kind: NodeKind,
    degree: u32,
}

struct AgentEntry {
    slot: u32,
    holder: String,
    /// Paths it holds or touched (for `touched:`).
    paths: Vec<String>,
}

struct WorkEntry {
    slot: u32,
    key: String,
    text: String,
    state: String,
    routes: String,
    paths: Vec<String>,
}

/// Where a conversation turn lives, for routing a snippet back (0034).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConvRef {
    pub run: String,
    pub worker: String,
    /// Index of the turn in the worker's transcript view.
    pub turn: usize,
}

/// One worker transcript as a search source. The text is not copied: the source
/// holds the corpus's own `Arc` and search reads the verbatim turns in place,
/// case-insensitively ([`find_ci`]).
#[derive(Debug, Clone)]
pub struct ConvSource {
    pub transcript: Arc<Transcript>,
    /// The work node key of its contract (`<run>/<contract>`).
    pub work_key: String,
    /// The holder prefix `agent:` matches (see `interaction::conv_sources`).
    pub holder: String,
}

/// One contract brief as a search source: open, verified or superseded (0032).
#[derive(Debug, Clone)]
pub struct BriefDoc {
    pub key: String,
    /// `open`, `verified` or `superseded`.
    pub state: &'static str,
    pub instruction: String,
    pub routes: String,
    pub writes: Vec<String>,
}

/// A matching turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnHit {
    pub at: ConvRef,
    pub work_key: String,
}

/// Everything searchable, as plain vectors.
#[derive(Default)]
pub struct Index {
    code: Vec<CodeEntry>,
    agents: Vec<AgentEntry>,
    work: Vec<WorkEntry>,
    briefs: Vec<BriefDoc>,
    convs: Arc<Vec<ConvSource>>,
    /// Work key or holder to slot, for placing matches.
    work_slots: HashMap<String, u32>,
    /// Task key to the agents working it, for placing conversation matches.
    task_agents: HashMap<String, Vec<u32>>,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("code", &self.code.len())
            .field("agents", &self.agents.len())
            .field("work", &self.work.len())
            .field("briefs", &self.briefs.len())
            .field("convs", &self.convs.len())
            .finish()
    }
}

/// Every contract brief of the runs: open ones, and the verified and superseded
/// ones the record keeps in `coordination.closed`.
fn briefs_of(runs: &[StoredRun]) -> Vec<BriefDoc> {
    let mut out = Vec::new();
    for r in crate::run_view::records(runs) {
        let run = r.run.as_str();
        for c in &r.open_contracts {
            let routes: Vec<&str> = c
                .worker_routes
                .iter()
                .map(String::as_str)
                .chain(c.route.as_deref())
                .collect();
            out.push(BriefDoc {
                key: crate::run_view::contract_key(run, &c.contract),
                state: "open",
                instruction: c.instruction.clone(),
                routes: routes.join(" ").to_lowercase(),
                writes: c.writes.iter().map(|w| w.path.to_lowercase()).collect(),
            });
        }
        for c in &r.coordination.closed {
            out.push(BriefDoc {
                key: crate::run_view::contract_key(run, &c.contract),
                state: match c.closure {
                    Closure::Verified => "verified",
                    Closure::Superseded => "superseded",
                },
                instruction: c.instruction.clone(),
                routes: c.worker_routes.join(" ").to_lowercase(),
                writes: Vec::new(),
            });
        }
    }
    out
}

impl Index {
    /// Index the model's code, agents and work nodes, the runs' contract briefs
    /// (open and closed), and the given transcripts.
    pub fn build(model: &ProjectModel, runs: &[StoredRun], convs: Arc<Vec<ConvSource>>) -> Self {
        let code = model
            .code_nodes()
            .map(|(slot, qual, path, kind, degree)| {
                let name = qual.rsplit("::").next().unwrap_or(qual);
                let name = name.rsplit('.').next().unwrap_or(name);
                CodeEntry {
                    slot,
                    spelled: name.to_owned(),
                    name: name.to_lowercase(),
                    qual: qual.to_lowercase(),
                    path: path.to_lowercase(),
                    words: words(qual),
                    kind,
                    degree,
                }
            })
            .collect();
        let mut agent_paths: HashMap<&str, Vec<String>> = HashMap::new();
        let mut task_paths: HashMap<&str, Vec<String>> = HashMap::new();
        for a in model.activity() {
            agent_paths
                .entry(a.agent.as_str())
                .or_default()
                .push(a.path.to_lowercase());
            task_paths
                .entry(a.task.as_str())
                .or_default()
                .push(a.path.to_lowercase());
        }
        let agents = model
            .agents()
            .map(|(holder, slot)| AgentEntry {
                slot,
                holder: holder.to_lowercase(),
                paths: agent_paths.remove(holder).unwrap_or_default(),
            })
            .collect();
        let briefs = briefs_of(runs);
        let by_key: HashMap<&str, &BriefDoc> = briefs.iter().map(|b| (b.key.as_str(), b)).collect();
        let mut work_slots = HashMap::new();
        let work = model
            .plan_nodes()
            .map(|(n, slot)| {
                work_slots.insert(n.key.clone(), slot);
                let brief = by_key.get(n.key.as_str());
                let mut paths = brief.map(|b| b.writes.clone()).unwrap_or_default();
                paths.extend(task_paths.remove(n.key.as_str()).unwrap_or_default());
                WorkEntry {
                    slot,
                    key: n.key.to_lowercase(),
                    text: n.label.to_lowercase(),
                    state: n.state.word().to_owned(),
                    routes: brief.map(|b| b.routes.clone()).unwrap_or_default(),
                    paths,
                }
            })
            .collect();
        let mut task_agents: HashMap<String, Vec<u32>> = HashMap::new();
        for (holder, slot) in model.agents() {
            work_slots.insert(holder.to_owned(), slot);
            for task in model.agent_tasks(holder) {
                task_agents.entry(task.to_owned()).or_default().push(slot);
            }
        }
        Self {
            code,
            agents,
            work,
            briefs,
            convs,
            work_slots,
            task_agents,
        }
    }

    pub fn briefs(&self) -> &[BriefDoc] {
        &self.briefs
    }

    /// The nodes a conversation turn (or brief) with work key `work_key` belongs
    /// to on the graph: its work node, and the agents working that task now. Empty
    /// when its run is not drawn.
    pub fn slots_for(&self, work_key: &str) -> Vec<u32> {
        let mut out: Vec<u32> = self.work_slots.get(work_key).copied().into_iter().collect();
        if let Some(agents) = self.task_agents.get(work_key) {
            out.extend(agents);
        }
        out
    }
}

/// Where `needle` (lowercase) first occurs in `hay`, ignoring case, as a byte
/// offset valid in `hay`. ASCII needles compare byte by byte with no copy of
/// `hay`; a needle with non-ASCII letters lowercases `hay` for that one test,
/// and then only says that it matched (`Some(None)`), since lowercasing can move
/// offsets.
pub fn find_ci(hay: &str, needle: &str) -> Option<Option<usize>> {
    if needle.is_empty() {
        return Some(Some(0));
    }
    if needle.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        if n.len() > h.len() {
            return None;
        }
        let first = n[0];
        for i in 0..=h.len() - n.len() {
            if h[i].to_ascii_lowercase() == first && h[i..i + n.len()].eq_ignore_ascii_case(n) {
                return Some(Some(i));
            }
        }
        return None;
    }
    hay.to_lowercase().contains(needle).then_some(None)
}

/// One lit node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub slot: u32,
    pub score: i32,
}

/// What a query found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Results {
    /// Slots to light, best first, each once.
    pub hits: Vec<Hit>,
    /// Matching conversation turns, best first.
    pub turns: Vec<TurnHit>,
    /// Matching briefs (by index in the index's list), open and closed.
    pub briefs: Vec<usize>,
    /// Matches whose node is not on the graph (a run not shown, a superseded
    /// contract), counted and said, never hidden.
    pub unplaced: usize,
    /// Slots found past [`MAX_HITS`], not lit.
    pub hits_dropped: usize,
    /// Turns found past [`MAX_TURNS`], not listed.
    pub turns_dropped: usize,
}

/// Most slots lit by one query (the best-ranked are kept).
pub const MAX_HITS: usize = 5_000;
/// Most conversation turns returned as snippets.
pub const MAX_TURNS: usize = 200;

fn text_score_code(e: &CodeEntry, t: &str, spelled: &[String]) -> Option<i32> {
    if e.name == t {
        // Upstream's exact-name tier: spelled exactly 30, case-insensitively 20.
        Some(if spelled.contains(&e.spelled) { 30 } else { 20 })
    } else if e.name.starts_with(t) {
        Some(18)
    } else if e.words.iter().any(|w| w == t) {
        Some(14)
    } else if e.name.contains(t) {
        Some(10)
    } else if e.qual.contains(t) {
        Some(6)
    } else if e.path.contains(t) {
        Some(3)
    } else {
        None
    }
}

fn state_matches(state: &str, v: &str) -> bool {
    let alias = match v {
        "cap" | "capped" | "held-by-cap" => "concurrency cap",
        "running" | "in-flight" | "inflight" => "in flight",
        "lease" | "colliding" => "colliding",
        "held" => "held",
        other => other,
    };
    state.contains(alias)
}

/// Run `q` over `index`.
pub fn run(index: &Index, q: &Query) -> Results {
    let mut scored: HashMap<u32, i32> = HashMap::new();
    let mut add = |slot: u32, score: i32| {
        let e = scored.entry(slot).or_insert(i32::MIN);
        *e = (*e).max(score);
    };
    let texts: Vec<&str> = q
        .terms
        .iter()
        .filter_map(|t| match t {
            Term::Text(s) => Some(s.as_str()),
            Term::Op(..) => None,
        })
        .collect();
    let ops: Vec<(Op, &str)> = q
        .terms
        .iter()
        .filter_map(|t| match t {
            Term::Op(o, v) => Some((*o, v.as_str())),
            Term::Text(_) => None,
        })
        .collect();
    let timed = q.before.is_some() || q.after.is_some();
    let op_is = |allowed: &[Op]| ops.iter().all(|(o, _)| allowed.contains(o));
    let mut unplaced = 0;

    // Code: text, symbol:, file:.
    if !timed && op_is(&[Op::Symbol, Op::File]) && !q.is_empty() {
        // Files are noise for bare text, not for an explicit operator.
        let structural = q.has_op(Op::File) || q.has_op(Op::Symbol);
        for e in &index.code {
            let boost = match kind_boost(e.kind) {
                Some(b) => b,
                None if structural => 0,
                None => continue,
            };
            let mut score = boost + (e.degree.min(50) / 10) as i32;
            let mut ok = true;
            for (o, v) in &ops {
                let s = match o {
                    Op::Symbol => (e.qual.contains(v)).then(|| if e.name == *v { 30 } else { 12 }),
                    Op::File => e.path.contains(v).then_some(8),
                    _ => None,
                };
                match s {
                    Some(s) => score += s,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            for t in &texts {
                match text_score_code(e, t, &q.spelled) {
                    Some(s) => score += s,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                add(e.slot, score);
            }
        }
    }

    // Agents: text, agent:, touched:.
    if !timed && op_is(&[Op::Agent, Op::Touched]) && !q.is_empty() {
        for e in &index.agents {
            let ok = ops.iter().all(|(o, v)| match o {
                Op::Agent => e.holder.contains(v),
                Op::Touched => e.paths.iter().any(|p| p.contains(v)),
                _ => false,
            }) && texts.iter().all(|t| e.holder.contains(t));
            if ok {
                add(e.slot, 40);
            }
        }
    }

    // Work nodes: text, task:, status:, touched:, vendor:/model:.
    if !timed && op_is(&[Op::Task, Op::Status, Op::Touched, Op::Route]) && !q.is_empty() {
        for e in &index.work {
            let ok = ops.iter().all(|(o, v)| match o {
                Op::Task => e.key.contains(v) || e.text.contains(v),
                Op::Status => state_matches(&e.state, v),
                Op::Touched => e.paths.iter().any(|p| p.contains(v)),
                Op::Route => e.routes.contains(v),
                _ => false,
            }) && texts
                .iter()
                .all(|t| e.text.contains(t) || e.key.contains(t));
            if ok {
                add(e.slot, 45);
            }
        }
    }

    // Briefs, open and closed: text, task: (contract:), vendor:/model:.
    let mut briefs = Vec::new();
    if !timed && op_is(&[Op::Task, Op::Route]) && !q.is_empty() {
        for (i, b) in index.briefs.iter().enumerate() {
            let key = b.key.to_lowercase();
            let ok = ops.iter().all(|(o, v)| match o {
                Op::Task => key.contains(v) || find_ci(&b.instruction, v).is_some(),
                Op::Route => b.routes.contains(v),
                _ => false,
            }) && texts
                .iter()
                .all(|t| key.contains(t) || find_ci(&b.instruction, t).is_some());
            if ok {
                briefs.push(i);
                let placed = index.slots_for(&b.key);
                if placed.is_empty() {
                    unplaced += 1;
                }
                for s in placed {
                    add(s, 42);
                }
            }
        }
    }

    // Conversation turns: text, agent:, task:, before:/after:, read in place.
    let mut turns: Vec<(TurnHit, i64)> = Vec::new();
    let conv_ops_ok = op_is(&[Op::Agent, Op::Task]);
    if conv_ops_ok && (!texts.is_empty() || timed || !ops.is_empty()) {
        for src in index.convs.iter() {
            let t = &src.transcript;
            let time = t.modified_ms;
            if let Some(b) = q.before
                && time.is_none_or(|t| t >= b)
            {
                continue;
            }
            if let Some(a) = q.after
                && time.is_none_or(|t| t < a)
            {
                continue;
            }
            let source_ok = ops.iter().all(|(o, v)| match o {
                Op::Agent => find_ci(&t.worker, v).is_some() || find_ci(&src.holder, v).is_some(),
                Op::Task => find_ci(&src.work_key, v).is_some(),
                _ => false,
            });
            if !source_ok {
                continue;
            }
            for (i, turn) in t.turns.iter().enumerate() {
                let parts = turn.parts();
                let ok = texts
                    .iter()
                    .all(|w| parts.iter().any(|p| find_ci(p, w).is_some()));
                if ok {
                    turns.push((
                        TurnHit {
                            at: ConvRef {
                                run: t.run.clone(),
                                worker: t.worker.clone(),
                                turn: i,
                            },
                            work_key: src.work_key.clone(),
                        },
                        time.unwrap_or(0),
                    ));
                }
            }
        }
    }
    // Recency first (0032), then transcript order.
    turns.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.at.worker.cmp(&b.0.at.worker))
            .then(a.0.at.turn.cmp(&b.0.at.turn))
    });
    let turns_dropped = turns.len().saturating_sub(MAX_TURNS);
    turns.truncate(MAX_TURNS);
    for (hit, _) in &turns {
        let placed = index.slots_for(&hit.work_key);
        if placed.is_empty() {
            unplaced += 1;
        }
        for s in placed {
            add(s, 35);
        }
    }

    let mut hits: Vec<Hit> = scored
        .into_iter()
        .map(|(slot, score)| Hit { slot, score })
        .collect();
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.slot.cmp(&b.slot)));
    let hits_dropped = hits.len().saturating_sub(MAX_HITS);
    hits.truncate(MAX_HITS);
    Results {
        hits,
        turns: turns.into_iter().map(|(h, _)| h).collect(),
        briefs,
        unplaced,
        hits_dropped,
        turns_dropped,
    }
}

/// A query for the search thread.
pub struct SearchJob {
    pub seq: u64,
    pub index: Arc<Index>,
    pub query: Query,
}

/// A query's answer.
pub struct SearchDone {
    pub seq: u64,
    pub results: Results,
    pub took: std::time::Duration,
}

/// Runs queries off the UI thread (a scan of 64 MB of transcript takes tens of
/// milliseconds; the UI thread must not). Only the newest waiting job runs; an
/// older one it superseded is skipped. `wake` runs after each answer. Dropping
/// the handle ends the thread.
pub struct SearchWorker {
    tx: Option<std::sync::mpsc::Sender<SearchJob>>,
    rx: std::sync::mpsc::Receiver<SearchDone>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for SearchWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchWorker").finish_non_exhaustive()
    }
}

impl SearchWorker {
    pub fn spawn(wake: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let (tx, jobs) = std::sync::mpsc::channel::<SearchJob>();
        let (done_tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("search".into())
            .spawn(move || {
                while let Ok(mut job) = jobs.recv() {
                    // Skip to the newest job waiting.
                    while let Ok(newer) = jobs.try_recv() {
                        job = newer;
                    }
                    let t0 = std::time::Instant::now();
                    let results = run(&job.index, &job.query);
                    let done = SearchDone {
                        seq: job.seq,
                        results,
                        took: t0.elapsed(),
                    };
                    if done_tx.send(done).is_err() {
                        return;
                    }
                    wake();
                }
            })?;
        Ok(Self {
            tx: Some(tx),
            rx,
            thread: Some(thread),
        })
    }

    pub fn submit(&self, job: SearchJob) -> Result<(), String> {
        self.tx
            .as_ref()
            .ok_or_else(|| "the search thread has stopped".to_owned())?
            .send(job)
            .map_err(|_| "the search thread has stopped".to_owned())
    }

    /// The newest answer that arrived, without waiting.
    pub fn try_latest(&self) -> Option<SearchDone> {
        self.rx.try_iter().last()
    }

    /// Waits up to `timeout` for an answer (tests).
    pub fn wait(&self, timeout: std::time::Duration) -> Option<SearchDone> {
        self.rx.recv_timeout(timeout).ok()
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) {
        // Closing the job channel ends the thread's loop.
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operators_are_parsed_inline_and_paths_stay_text() {
        let q = parse(r#"symbol:Parse file:src/ "config file" std::fs"#).expect("parse");
        assert_eq!(
            q.terms,
            [
                Term::Op(Op::Symbol, "parse".into()),
                Term::Op(Op::File, "src/".into()),
                Term::Text("config file".into()),
                Term::Text("std::fs".into()),
            ]
        );
        assert_eq!(
            parse("contract:a vendor:x model:y").expect("parse").terms,
            [
                Term::Op(Op::Task, "a".into()),
                Term::Op(Op::Route, "x".into()),
                Term::Op(Op::Route, "y".into())
            ]
        );
        assert_eq!(
            parse("http://example").expect("parse").terms,
            [Term::Text("http://example".into())],
            "an unknown name is text"
        );
    }

    #[test]
    fn every_malformed_operator_is_a_visible_error() {
        assert_eq!(
            parse("note:x"),
            Err(SearchError::UnknownOperator("note".into()))
        );
        assert_eq!(
            parse("status:finished"),
            Err(SearchError::BadStatus("finished".into()))
        );
        assert!(matches!(
            parse("after:2026-02-31"),
            Err(SearchError::BadDate { .. })
        ));
        assert!(parse("after:2024-02-29").is_ok(), "a leap day is a date");
        assert!(matches!(
            parse("after:2025-02-29"),
            Err(SearchError::BadDate { .. })
        ));
        assert_eq!(
            parse("before:2026-01-01 before:2026-02-01"),
            Err(SearchError::Duplicate("before".into()))
        );
        assert_eq!(
            parse("\"status:done now\""),
            Err(SearchError::QuotedOperator("status:done".into()))
        );
        // Still text: paths and URLs, and a quoted phrase without operators.
        assert!(parse("std::fs http://x \"plain words\"").is_ok());
    }

    /// Briefs of verified and superseded contracts are searchable (0032), and
    /// a superseded one, which has no node, is counted as unplaced and listed.
    #[test]
    fn closed_contracts_are_found_by_their_brief() {
        use kgraf_core::RunId;
        use kgraf_orchestrator::{ClosedContract, RunStateRecord};
        let m = model();
        let mut r = RunStateRecord::new(RunId::new("r1"), "go", 1);
        r.coordination.closed = vec![
            ClosedContract {
                contract: "a".into(),
                closure: Closure::Verified,
                instruction: "Port the Lexer to Rust".into(),
                acceptance: "ok".into(),
                workers: vec![],
                worker_routes: vec![],
                rationale: None,
            },
            ClosedContract {
                contract: "old".into(),
                closure: Closure::Superseded,
                instruction: "Port the lexer in one piece".into(),
                acceptance: "ok".into(),
                workers: vec![],
                worker_routes: vec![],
                rationale: None,
            },
        ];
        let runs = vec![kgraf_orchestrator::StoredRun {
            run: "r1".into(),
            active: false,
            record: Ok(r),
        }];
        let i = Index::build(&m, &runs, Default::default());
        let res = find(&i, &m, "contract:lexer");
        let states: Vec<&str> = res.briefs.iter().map(|&b| i.briefs()[b].state).collect();
        assert_eq!(states, ["verified", "superseded"]);
        assert!(
            slots(&res).contains(&m.task_slot("r1/a").expect("a")),
            "the verified one lights its node (and the agent working it)"
        );
        assert_eq!(
            res.unplaced, 1,
            "the superseded one has no node, and says so"
        );
    }

    #[test]
    fn a_malformed_query_is_an_error_not_a_narrower_search() {
        assert_eq!(
            parse("agent:"),
            Err(SearchError::EmptyValue("agent".into()))
        );
        assert!(matches!(
            parse("before:tomorrow"),
            Err(SearchError::BadDate { .. })
        ));
        assert!(matches!(
            parse("after:2026-13-01"),
            Err(SearchError::BadDate { .. })
        ));
        assert_eq!(parse("\"open"), Err(SearchError::OpenQuote));
    }

    #[test]
    fn dates_are_utc_midnight() {
        let q = parse("after:1970-01-02 before:2026-09-26").expect("parse");
        assert_eq!(q.after, Some(86_400_000));
        // 2026-09-26 is day 20,722 since the epoch.
        assert_eq!(q.before, Some(20_722 * 86_400_000));
    }

    use kgraf_galaxy::{PlanNode, WorkState};
    use kgraf_graph::layout::Position;
    use kgraf_graph::snapshot::{Activity, ActivityKind, GraphSnapshot, SceneNode};

    fn code(id: &str, qual: &str, path: &str, kind: NodeKind) -> SceneNode {
        SceneNode {
            id: id.into(),
            path: path.into(),
            kind,
            qualified_name: qual.into(),
            position: Position {
                x: id.len() as f32,
                y: 0.0,
                z: 0.0,
            },
        }
    }

    /// A model with four code nodes, one agent on `src/parse.rs`, and two plan
    /// nodes.
    fn model() -> ProjectModel {
        let snapshot = GraphSnapshot {
            nodes: vec![
                code("f", "src/parse.rs", "src/parse.rs", NodeKind::File),
                code(
                    "p",
                    "parse::parse_config",
                    "src/parse.rs",
                    NodeKind::Function,
                ),
                code("s", "parse::ConfigParser", "src/parse.rs", NodeKind::Struct),
                code(
                    "m",
                    "parse::consts::PARSE_LIMIT",
                    "src/parse.rs",
                    NodeKind::Constant,
                ),
            ],
            activity: vec![Activity {
                agent: "r1/a@n".into(),
                task: "r1/a".into(),
                kind: ActivityKind::Holds,
                path: "src/parse.rs".into(),
                node_id: Some("f".into()),
            }],
            ..GraphSnapshot::default()
        };
        let (mut m, mut scene) = ProjectModel::build(&snapshot).expect("build");
        let plan = |key: &str, state: WorkState| PlanNode {
            key: key.into(),
            state,
            label: format!("{key}: {}", state.word()),
            blocked_by: vec![],
            asks: 0,
            parent: None,
        };
        m.set_plan(
            &mut scene,
            &[
                plan("r1/a", WorkState::InFlight),
                plan("r1/b", WorkState::HeldByCap),
            ],
        )
        .expect("plan");
        m
    }

    fn slots(r: &Results) -> Vec<u32> {
        r.hits.iter().map(|h| h.slot).collect()
    }

    fn find(i: &Index, m: &ProjectModel, q: &str) -> Results {
        let _ = m;
        run(i, &parse(q).expect("parse"))
    }

    #[test]
    fn code_ranks_functions_first_and_files_are_noise_for_bare_text() {
        let m = model();
        let i = Index::build(&m, &[], Default::default());
        let slot = |id: &str| m.slot_of(id).expect("slot");
        let r = find(&i, &m, "parse");
        let got = slots(&r);
        assert_eq!(
            got.first(),
            Some(&slot("p")),
            "the function outranks the rest"
        );
        assert!(!got.contains(&slot("f")), "a file is noise for bare text");
        assert!(got.contains(&slot("m")), "a constant still matches");
        // file: reaches the file itself.
        assert!(slots(&find(&i, &m, "file:parse.rs")).contains(&slot("f")));
        // camelCase words match.
        assert_eq!(slots(&find(&i, &m, "symbol:parser")), [slot("s")]);
        assert!(slots(&find(&i, &m, "zzz")).is_empty());
    }

    #[test]
    fn agents_work_nodes_and_their_states_are_found() {
        let m = model();
        let i = Index::build(&m, &[], Default::default());
        let agent = m.agent_slot("r1/a@n").expect("agent");
        assert_eq!(slots(&find(&i, &m, "agent:r1/a")), [agent]);
        let capped = m.task_slot("r1/b").expect("b");
        assert_eq!(slots(&find(&i, &m, "status:cap")), [capped]);
        let touched = slots(&find(&i, &m, "touched:parse.rs"));
        assert!(touched.contains(&agent));
        assert!(touched.contains(&m.task_slot("r1/a").expect("a")));
    }

    fn turn(worker: &str, work_key: &str, text: &str, time_ms: Option<i64>) -> ConvSource {
        let mut t = crate::transcripts::Transcript::for_test(
            "r1",
            worker,
            "w.jsonl".into(),
            vec![crate::transcripts::Turn::Assistant(text.into())],
        );
        t.modified_ms = time_ms;
        ConvSource {
            transcript: Arc::new(t),
            work_key: work_key.into(),
            holder: work_key.into(),
        }
    }

    #[test]
    fn a_phrase_from_a_conversation_lights_its_node_and_counts_the_unplaced() {
        let m = model();
        let i = Index::build(
            &m,
            &[],
            Arc::new(vec![
                turn("w1", "r1/a", "We chose the Arena allocator", Some(2_000)),
                turn("w2", "r9/x", "the arena allocator again", Some(1_000)),
                turn("w3", "r1/b", "unrelated", None),
            ]),
        );
        let r = find(&i, &m, "\"arena allocator\"");
        let workers: Vec<&str> = r.turns.iter().map(|t| t.at.worker.as_str()).collect();
        assert_eq!(
            workers,
            ["w1", "w2"],
            "newest first, matched without regard to case"
        );
        let mut lit = slots(&r);
        lit.sort_unstable();
        let mut want = vec![
            m.task_slot("r1/a").expect("a"),
            m.agent_slot("r1/a@n").expect("the agent on r1/a"),
        ];
        want.sort_unstable();
        assert_eq!(lit, want, "the work node and the agent working it");
        assert_eq!(
            r.unplaced, 1,
            "run r9 is not on the graph; said, not hidden"
        );
        // Time bounds apply to turns only, and a turn without a time is excluded.
        let r = find(&i, &m, "after:1970-01-01 arena");
        assert_eq!(r.turns.len(), 2);
        let r = find(&i, &m, "before:1970-01-01");
        assert!(r.turns.is_empty() && r.hits.is_empty());
    }

    /// Measured, not assumed: 10,000 code nodes, one query.
    #[test]
    fn a_query_over_ten_thousand_nodes_is_fast() {
        let nodes: Vec<SceneNode> = (0..10_000)
            .map(|n| {
                code(
                    &format!("n{n}"),
                    &format!("mod{}::item{n}::handleRequest{n}", n % 97),
                    &format!("src/dir{}/f{}.rs", n % 53, n % 400),
                    NodeKind::Function,
                )
            })
            .collect();
        let (m, _) = ProjectModel::build(&GraphSnapshot {
            nodes,
            ..GraphSnapshot::default()
        })
        .expect("build");
        let t0 = std::time::Instant::now();
        let i = Index::build(&m, &[], Default::default());
        let build = t0.elapsed();
        let q = parse("handle request42").expect("parse");
        let t1 = std::time::Instant::now();
        let r = run(&i, &q);
        let query = t1.elapsed();
        eprintln!(
            "10k nodes: index {build:?}, query {query:?}, {} hits",
            r.hits.len()
        );
        assert!(!r.hits.is_empty());
        // Generous for an unoptimized test build; the release figure is reported.
        assert!(query < std::time::Duration::from_millis(250), "{query:?}");
    }

    #[test]
    fn names_split_on_case_and_separators() {
        assert_eq!(
            words("kgraf_shell::app::ShellApp::handleQuitShortcut"),
            [
                "kgraf", "shell", "app", "shell", "app", "handle", "quit", "shortcut"
            ]
        );
        assert_eq!(words("HTTPServer2"), ["httpserver2"]);
    }
}
