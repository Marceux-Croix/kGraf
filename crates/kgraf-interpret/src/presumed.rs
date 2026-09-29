//! Presumed work nodes from a project's own documents (decision 0064).
//!
//! A fresh or half-built project has plans before it has work: a roadmap, a
//! `TODO.md`, a plan a model read from the corpus. This module writes them into
//! the graph as **presumed** work nodes, so the galaxy shows what is done, in
//! flight and still to do from the first index, and re-reads them as the
//! documents and the code change.
//!
//! ```text
//!   roadmap docs --parse (roadmap.rs, no model)--> entries --sync--> presumed nodes
//!   model reading (pass.rs, one ledgered call) ----------------sync--> presumed nodes
//!   approved plan (approve.rs) --adopt--> the presumed nodes its items cite
//!   views(): claim (what the document says) + named code (what exists) -> state
//! ```
//!
//! - **Two sources.** [`presume_roadmaps`] parses documents deterministically and
//!   costs no tokens. [`presume_reading`] writes the work items of a model's
//!   reading of the corpus (`kgraf orchestrate --corpus`), which is the only path
//!   for a project whose plan is prose, or an empty directory with an
//!   instruction; the call is the interpretation pass's own, made through the
//!   caller's ledgered `Asker` (every token is attributed to the ledger), and its model and ledger call are
//!   recorded on every node it presumes. A reading item that cites a section a
//!   roadmap node already stands for is not presumed twice.
//! - **The store's rules** (`kgraf_graph::presumed`): stable ids, re-reads that
//!   update and never duplicate or delete, edits by anyone else never
//!   overwritten, adopted nodes left alone.
//! - **No status is stored.** A document's status word is its *claim*. The
//!   [`PresumedState`] shown is derived at read time from the claim and from
//!   whether the paths and symbols the entry names exist in the graph. A claim of
//!   done is shown as presumed done, never as verified: done includes tests
//!   (decision 0019), and no test outcome is read here.

use crate::error::InterpretError;
use crate::model::{Claim, WorkItem};
use crate::pass::Interpretation;
use crate::reconcile::{path_exists, symbol_exists, test_exists};
use crate::roadmap::{self, EntryKind, Named, Roadmap, anchor_of_citation, slug};
use kgraf_graph::{GraphStore, PresumedNode, PresumedOrigin, PresumedSync, WorkKind, WorkNode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path};

/// `source_kind` of nodes parsed from a document.
pub const ROADMAP: &str = "roadmap";
/// `source_kind` (and `source`) of nodes from a model reading.
pub const READING: &str = "reading";

/// File stems looked for, case-insensitively, with a `.md` or `.markdown`
/// extension.
pub const ROADMAP_STEMS: &[&str] = &["roadmap", "todo", "plan", "milestones", "backlog", "tasks"];
/// Directories looked in, relative to the root (`""` is the root).
pub const ROADMAP_DIRS: &[&str] = &["", "docs"];

/// The largest roadmap document read, in bytes. A larger one is
/// refused with its size, never parsed in part: 4 MiB is far above any roadmap
/// written by hand (this repository's is about 0.4 MiB).
pub const MAX_ROADMAP_BYTES: u64 = 4 * 1024 * 1024;

/// What [`discover`] looks for, for a message.
pub fn looked_for() -> String {
    format!(
        "{} (.md or .markdown, any case) in the project root and docs/",
        ROADMAP_STEMS.join(", ")
    )
}

/// The roadmap documents of a project, repository-relative, sorted.
pub fn discover(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for dir in ROADMAP_DIRS {
        // A linked `docs` is not walked into, and a linked file is not a file
        // here (`file_type` does not follow links): discovery proposes nothing
        // that could resolve outside the project.
        if !dir.is_empty() && !std::fs::symlink_metadata(root.join(dir)).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let lower = name.to_lowercase();
            let Some((stem, ext)) = lower.rsplit_once('.') else {
                continue;
            };
            if matches!(ext, "md" | "markdown") && ROADMAP_STEMS.contains(&stem) {
                found.push(if dir.is_empty() {
                    name
                } else {
                    format!("{dir}/{name}")
                });
            }
        }
    }
    found.sort();
    found
}

/// What one roadmap document yielded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceReport {
    pub source: String,
    /// The structures found ([`Roadmap::shape`]).
    pub shape: String,
    pub items: usize,
    pub components: usize,
    pub sync: PresumedSync,
}

/// What a node's body holds, as read back. Every field defaults, so a body
/// someone else rewrote reads as "no claim, names nothing" rather than failing.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Body {
    #[serde(default)]
    claim: Claim,
    #[serde(default)]
    claim_text: String,
    #[serde(default)]
    named: Named,
    #[serde(default)]
    tests: Vec<String>,
    #[serde(default)]
    anchors: Vec<String>,
    #[serde(default)]
    reading_plan: Option<String>,
    #[serde(default)]
    item_id: Option<String>,
}

fn body_of(node: &WorkNode) -> Body {
    serde_json::from_str(&node.body).unwrap_or_default()
}

/// A path the reader may open: relative, inside the root, no `..`.
fn inside(path: &str) -> bool {
    let p = Path::new(path);
    !path.trim().is_empty()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// A claim's stored name (`complete`, `partial`, `not_started`, `unstated`).
fn claim_name(c: Claim) -> String {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// A stored claim name back to the claim; an empty or unknown one is `None`.
fn claim_of_name(name: &str) -> Option<Claim> {
    serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()
}

fn plan_of(source: &str) -> String {
    format!("presumed/{ROADMAP}/{source}")
}

/// The presumed nodes one parsed document stands for.
fn roadmap_nodes(source: &str, r: &Roadmap, now_ms: i64) -> Vec<PresumedNode> {
    let plan = plan_of(source);
    let origin = |section: &str, claim: Claim, claim_text: &str| PresumedOrigin {
        source_kind: ROADMAP.into(),
        source: source.into(),
        section: section.into(),
        parser: roadmap::PARSER.into(),
        claim: claim_name(claim),
        claim_text: claim_text.into(),
    };
    let id_of = |i: usize| {
        let e = &r.entries[i];
        let kind = match e.kind {
            EntryKind::Component => "component",
            EntryKind::Item => "item",
        };
        format!("{plan}/{kind}/{}", e.key.join("/"))
    };
    let mut nodes = vec![PresumedNode {
        node: WorkNode {
            id: plan.clone(),
            plan_id: plan.clone(),
            kind: WorkKind::Plan,
            parent_id: None,
            title: r.title.clone().unwrap_or_else(|| source.to_owned()),
            // What a re-read would change only when the document's identity
            // changes: the counts of what it holds are reported, not stored, so a
            // new row does not rewrite the root.
            body: json!({
                "presumed": true,
                "source_kind": ROADMAP,
                "source": source,
            })
            .to_string(),
            created_ms: now_ms,
        },
        origin: origin("", Claim::Unstated, ""),
    }];
    for (i, e) in r.entries.iter().enumerate() {
        // A component is its heading: its body carries no excerpt, which would
        // hold its table's text and change with every row.
        let excerpt = match e.kind {
            EntryKind::Component => "",
            EntryKind::Item => e.excerpt.as_str(),
        };
        nodes.push(PresumedNode {
            node: WorkNode {
                id: id_of(i),
                plan_id: plan.clone(),
                kind: match e.kind {
                    EntryKind::Component => WorkKind::Component,
                    EntryKind::Item => WorkKind::Item,
                },
                parent_id: Some(e.parent.map_or_else(|| plan.clone(), id_of)),
                title: e.title.clone(),
                body: json!({
                    "presumed": true,
                    "source_kind": ROADMAP,
                    "source": source,
                    "section": e.section,
                    "excerpt": excerpt,
                    "named": e.named,
                    "anchors": e.anchors,
                })
                .to_string(),
                created_ms: now_ms,
            },
            origin: origin(&e.section, e.claim, &e.claim_text),
        });
    }
    nodes
}

/// Parse each roadmap document in `sources` (repository-relative) and bring its
/// presumed nodes into step with it. A document that cannot be read is an
/// error, never skipped (fail loud); one with no structure the parser reads yields
/// no items, which the report says ([`SourceReport::items`] is 0).
pub fn presume_roadmaps(
    store: &GraphStore,
    sources: &[String],
    now_ms: i64,
) -> Result<Vec<SourceReport>, InterpretError> {
    let mut reports = Vec::new();
    for source in sources {
        let source = source.trim().trim_start_matches("./").to_owned();
        if !inside(&source) {
            return Err(InterpretError::RoadmapPath { path: source });
        }
        let path = store.root().join(&source);
        let unreadable = |e: std::io::Error| InterpretError::RoadmapUnreadable {
            path: source.clone(),
            reason: e.to_string(),
        };
        // A symbolic link (the file, or a directory on its way) may point out of
        // the project; what is read must resolve inside it.
        let resolved = path.canonicalize().map_err(unreadable)?;
        let root = store.root().canonicalize().map_err(unreadable)?;
        if !resolved.starts_with(&root) {
            return Err(InterpretError::RoadmapPath { path: source });
        }
        // Only a regular file is read: a FIFO would block the read forever, and
        // a device or directory is not a document. Its size is bounded before
        // it is read: the parse runs on every index, and under the
        // host's graph lock in `orchestrate --corpus`.
        let meta = std::fs::metadata(&resolved).map_err(unreadable)?;
        if !meta.is_file() {
            return Err(InterpretError::RoadmapUnreadable {
                path: source,
                reason: "not a regular file".into(),
            });
        }
        if meta.len() > MAX_ROADMAP_BYTES {
            return Err(InterpretError::RoadmapTooLarge {
                path: source,
                bytes: meta.len(),
                limit: MAX_ROADMAP_BYTES,
            });
        }
        let text = std::fs::read_to_string(&resolved).map_err(unreadable)?;
        let parsed = roadmap::parse(&source, &text);
        let nodes = roadmap_nodes(&source, &parsed, now_ms);
        let sync = store.sync_presumed(ROADMAP, &source, &nodes, &[], now_ms)?;
        reports.push(SourceReport {
            shape: parsed.shape(),
            items: parsed.items().count(),
            components: parsed
                .entries
                .iter()
                .filter(|e| e.kind == EntryKind::Component)
                .count(),
            source,
            sync,
        });
    }
    Ok(reports)
}

/// What one spin-up read of the roadmap documents did (decision 0064): retirement, then every discovered document read on its own, so
/// one that fails does not stop the others. `kgraf index` (`presume_after_index`
/// in `kgraf-cli`) and the desktop shell, opening a project (`kgraf-shell`'s
/// `presume_bg`), both call [`presume_spinup`] and nothing else: the read, the
/// retirement and the per-source error handling are decided once, here, so a
/// caller only ever formats or shows what it returns.
#[derive(Debug, Default)]
pub struct SpinupReport {
    pub retired: Vec<(String, PresumedSync)>,
    pub sources: Vec<SourceReport>,
    /// A document discovery found but could not read: (source, why).
    pub failed: Vec<(String, InterpretError)>,
    /// Set when discovery found no roadmap document at all: what was looked
    /// for. A project with none is not a failure (the fail-loud rule still applies to a
    /// document discovery *does* find and cannot read, which goes to `failed`).
    pub no_roadmap: Option<String>,
}

/// Retire missing sources, discover the roadmap documents, and read each on its
/// own (decision 0064's spin-up read). Makes no model call and touches no
/// ledger: its only argument is the graph, so no token can be spent through it.
pub fn presume_spinup(store: &GraphStore, now_ms: i64) -> Result<SpinupReport, InterpretError> {
    let retired = retire_missing(store, now_ms)?;
    let found = discover(store.root());
    if found.is_empty() {
        return Ok(SpinupReport {
            retired,
            no_roadmap: Some(looked_for()),
            ..SpinupReport::default()
        });
    }
    let (sources, failed) = read_sources(store, &found, now_ms);
    Ok(SpinupReport {
        retired,
        sources,
        failed,
        no_roadmap: None,
    })
}

/// Read each of `sources` on its own, so one that fails does not stop the
/// others. The loop [`presume_spinup`] and `kgraf plan --from-roadmap`
/// (`kgraf-cli`'s `plan_cmd::from_roadmap`) both need, kept in one place.
/// Returns the reports and the failures as (source, error).
pub fn read_sources(
    store: &GraphStore,
    sources: &[String],
    now_ms: i64,
) -> (Vec<SourceReport>, Vec<(String, InterpretError)>) {
    let mut reports = Vec::new();
    let mut failed = Vec::new();
    for source in sources {
        match presume_roadmaps(store, std::slice::from_ref(source), now_ms) {
            Ok(mut r) => reports.append(&mut r),
            Err(e) => failed.push((source.clone(), e)),
        }
    }
    (reports, failed)
}

/// Mark gone every node of a stored roadmap source whose file is no longer
/// there (moved, renamed or deleted): no read would ever list its
/// nodes again, so without this they stay live forever. Adopted nodes are kept,
/// as every sync keeps them. A source whose file still exists is left alone,
/// even when this read did not name it: `kgraf plan --from-roadmap --source`
/// reads documents discovery does not find, and a later `kgraf index` must not
/// retire them. Returns (source, what the sync did) for each source retired
/// now; one already fully gone is not reported again.
pub fn retire_missing(
    store: &GraphStore,
    now_ms: i64,
) -> Result<Vec<(String, PresumedSync)>, InterpretError> {
    let mut sources: BTreeSet<String> = BTreeSet::new();
    for p in store.presumed_nodes()? {
        if p.origin.source_kind == ROADMAP && p.gone_ms.is_none() && p.adopted.is_none() {
            sources.insert(p.origin.source.clone());
        }
    }
    let mut out = Vec::new();
    for source in sources {
        let present = inside(&source) && listed_exactly(store.root(), &source);
        if present {
            continue;
        }
        let sync = store.sync_presumed(ROADMAP, &source, &[], &[], now_ms)?;
        out.push((source, sync));
    }
    Ok(out)
}

/// Whether `rel` exists under `root` with exactly this spelling: each of its
/// components appears, byte for byte, in its parent directory's listing.
/// A lookup by path alone is answered case-insensitively on
/// the default macOS and Windows file systems, so after `TODO.md` was renamed to
/// `todo.md` the old path still "existed", the old source was never retired, and
/// both sources' nodes stayed live. A symbolic link counts as listed (the read
/// itself refuses one that leaves the project).
fn listed_exactly(root: &Path, rel: &str) -> bool {
    let mut dir = root.to_path_buf();
    for part in Path::new(rel.trim_start_matches("./")).components() {
        let Component::Normal(name) = part else {
            return false;
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return false;
        };
        if !entries.flatten().any(|e| e.file_name() == name) {
            return false;
        }
        dir.push(name);
    }
    true
}

/// What presuming a model reading did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadingReport {
    pub sync: PresumedSync,
    /// Reading items not presumed because a roadmap node already stands for the
    /// section they cite: (item id, roadmap node id).
    pub covered: Vec<(String, String)>,
    /// Set when the reading had no work items (a scoping round): nothing was
    /// written, and earlier reading nodes were left as they are.
    pub questions_only: bool,
}

/// Anchor -> roadmap node id, for every roadmap node still listed.
fn roadmap_anchors(store: &GraphStore) -> Result<BTreeMap<String, String>, InterpretError> {
    let mut map = BTreeMap::new();
    for p in store.presumed_nodes()? {
        if p.origin.source_kind == ROADMAP && p.gone_ms.is_none() && p.node.kind == WorkKind::Item {
            for a in body_of(&p.node).anchors {
                map.entry(a).or_insert_with(|| p.node.id.clone());
            }
        }
    }
    Ok(map)
}

fn citations(item: &WorkItem) -> Vec<String> {
    item.sources
        .iter()
        .filter_map(|s| anchor_of_citation(s))
        .collect()
}

/// Write a model reading's work items as presumed nodes (see the module
/// documentation). No model call is made here: the reading was already paid for
/// and ledgered by the pass; its model and ledger call are recorded on each node.
pub fn presume_reading(
    store: &GraphStore,
    reading: &Interpretation,
    now_ms: i64,
) -> Result<ReadingReport, InterpretError> {
    let answer = reading.answer();
    if answer.work_items.is_empty() {
        return Ok(ReadingReport {
            questions_only: true,
            ..ReadingReport::default()
        });
    }
    let plan = format!("presumed/{READING}");
    let parser = format!(
        "model {} (reading {}, ledger call {})",
        reading.model(),
        reading.plan_id(),
        reading.call().unwrap_or("none recorded")
    );
    let origin = |section: &str, claim: Claim, claim_text: &str| PresumedOrigin {
        source_kind: READING.into(),
        source: READING.into(),
        section: section.into(),
        parser: parser.clone(),
        claim: claim_name(claim),
        claim_text: claim_text.into(),
    };
    let anchors = roadmap_anchors(store)?;
    let mut report = ReadingReport::default();
    let mut nodes = vec![PresumedNode {
        node: WorkNode {
            id: plan.clone(),
            plan_id: plan.clone(),
            kind: WorkKind::Plan,
            parent_id: None,
            title: "Plan read from the corpus by a model".into(),
            body: json!({
                "presumed": true,
                "source_kind": READING,
                "reading_plan": reading.plan_id(),
                "reading": answer.reading,
            })
            .to_string(),
            created_ms: now_ms,
        },
        origin: origin("", Claim::Unstated, ""),
    }];
    // Components and items are told apart by their id prefix, so each kind has
    // its own set: a component's name never shifts an item's id.
    let unique = |taken: &mut BTreeSet<String>, base: String| {
        let mut key = base.clone();
        let mut n = 1;
        while !taken.insert(key.clone()) {
            n += 1;
            key = format!("{base}~{n}");
        }
        key
    };
    let (mut components_taken, mut items_taken) = (BTreeSet::new(), BTreeSet::new());
    let mut component_ids = BTreeMap::new();
    for c in &answer.components {
        let id = format!(
            "{plan}/component/{}",
            unique(&mut components_taken, slug(&c.name))
        );
        component_ids.insert(c.id.clone(), id.clone());
        nodes.push(PresumedNode {
            node: WorkNode {
                id,
                plan_id: plan.clone(),
                kind: WorkKind::Component,
                parent_id: Some(plan.clone()),
                title: c.name.clone(),
                body: json!({
                    "presumed": true,
                    "source_kind": READING,
                    "purpose": c.purpose,
                    "paths": c.paths,
                })
                .to_string(),
                created_ms: now_ms,
            },
            origin: origin(&c.name, Claim::Unstated, ""),
        });
    }
    let mut item_ids = BTreeMap::new();
    for w in &answer.work_items {
        let cited = citations(w);
        if let Some(node) = cited.iter().find_map(|a| anchors.get(a)) {
            report.covered.push((w.id.clone(), node.clone()));
            continue;
        }
        let id = format!("{plan}/item/{}", unique(&mut items_taken, slug(&w.title)));
        item_ids.insert(w.id.clone(), id.clone());
        nodes.push(PresumedNode {
            node: WorkNode {
                id,
                plan_id: plan.clone(),
                kind: WorkKind::Item,
                parent_id: Some(
                    w.component
                        .as_ref()
                        .and_then(|c| component_ids.get(c).cloned())
                        .unwrap_or_else(|| plan.clone()),
                ),
                title: w.title.clone(),
                body: json!({
                    "presumed": true,
                    "source_kind": READING,
                    "section": w.title,
                    "named": Named {
                        paths: w.evidence.paths.clone(),
                        symbols: w.evidence.symbols.clone(),
                        capped: 0,
                    },
                    "tests": w.evidence.tests,
                    "instruction": w.instruction,
                    "sources": w.sources,
                    "anchors": cited,
                    "reading_plan": reading.plan_id(),
                    "item_id": w.id,
                })
                .to_string(),
                created_ms: now_ms,
            },
            origin: origin(&w.title, w.documents_claim, ""),
        });
    }
    let deps: Vec<(String, String)> = answer
        .work_items
        .iter()
        .flat_map(|w| {
            let from = item_ids.get(&w.id).cloned();
            w.depends_on
                .iter()
                .filter_map(|d| item_ids.get(d).cloned())
                .filter_map(move |to| from.clone().map(|f| (f, to)))
                .collect::<Vec<_>>()
        })
        .collect();
    report.sync = store.sync_presumed(READING, READING, &nodes, &deps, now_ms)?;
    Ok(report)
}

/// Mark the presumed nodes an approved plan's items stand for as adopted by
/// them: a reading node by the reading's plan id and item id, a roadmap node by
/// a section the item cites. Returns (presumed node, adopting item node) for
/// each node adopted now. Called by `ApprovedPlan::seed`.
pub(crate) fn adopt_for_plan(
    store: &GraphStore,
    plan_id: &str,
    items: &[WorkItem],
    now_ms: i64,
) -> Result<Vec<(String, String)>, InterpretError> {
    let presumed = store.presumed_nodes()?;
    let mut adopted = Vec::new();
    for w in items {
        let by = format!("{plan_id}/item/{}", w.id);
        let cited: BTreeSet<String> = citations(w).into_iter().collect();
        for p in &presumed {
            if p.adopted.is_some() || p.node.kind != WorkKind::Item {
                continue;
            }
            let b = body_of(&p.node);
            let from_reading = p.origin.source_kind == READING
                && b.reading_plan.as_deref() == Some(plan_id)
                && b.item_id.as_deref() == Some(w.id.as_str());
            let from_roadmap = p.origin.source_kind == ROADMAP
                && p.gone_ms.is_none()
                && b.anchors.iter().any(|a| cited.contains(a));
            if (from_reading || from_roadmap) && store.adopt_presumed(&p.node.id, &by, now_ms)? {
                adopted.push((p.node.id.clone(), by.clone()));
            }
        }
    }
    Ok(adopted)
}

/// A presumed node's derived state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PresumedState {
    /// The document says done, and nothing it names is missing. Not verified:
    /// no test outcome is read.
    Done,
    /// In flight or partly there: the document says so, or some of what it
    /// names exists, or it says done while something it names is missing.
    Partial,
    /// Not started: the document says so (or says nothing) and nothing it names
    /// exists.
    Remaining,
}

impl PresumedState {
    pub fn word(self) -> &'static str {
        match self {
            PresumedState::Done => "presumed done",
            PresumedState::Partial => "presumed partly done",
            PresumedState::Remaining => "presumed remaining",
        }
    }
}

/// One presumed node as the graph and the code show it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresumedView {
    pub id: String,
    pub plan_id: String,
    pub kind: WorkKind,
    pub parent_id: Option<String>,
    pub title: String,
    pub source_kind: String,
    pub source: String,
    pub section: String,
    pub parser: String,
    pub claim: Claim,
    pub claim_text: String,
    pub state: PresumedState,
    /// Named paths, symbols and tests found in the graph or on disk.
    pub present: Vec<String>,
    /// Named ones not found where they would be. A path counts only when it
    /// would be in this tree (a directory part whose first segment exists); a
    /// bare file name or another project's path is neither present nor missing.
    pub missing: Vec<String>,
    /// Where the claim and the code disagree, said plainly.
    pub detail: Option<String>,
    /// The real work that adopted it.
    pub adopted_by: Option<String>,
    /// Changed by someone else since it was read: re-reads leave it alone.
    pub edited: bool,
    /// Its source no longer lists it (kept, not deleted).
    pub gone: bool,
}

fn document_of(source_kind: &str) -> &'static str {
    if source_kind == READING {
        "reading"
    } else {
        "roadmap"
    }
}

/// A sort key that compares runs of digits as numbers.
fn natural_key(s: &str) -> Vec<(u8, u64, String)> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut n = String::new();
            while let Some(&d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                n.push(d);
                chars.next();
            }
            out.push((0, n.parse().unwrap_or(u64::MAX), String::new()));
        } else {
            let mut t = String::new();
            while let Some(&d) = chars.peek().filter(|d| !d.is_ascii_digit()) {
                t.push(d);
                chars.next();
            }
            out.push((1, 0, t));
        }
    }
    out
}

/// Whether a named path that does not exist would be in this repository: it has
/// a directory part, and its first segment exists at the root.
/// The first directory must resolve inside the root and the path may hold no
/// `..`: a model reading's `../x` or a link out of the project is
/// not this repository's to miss.
fn placed_here(store: &GraphStore, path: &str) -> bool {
    let path = path.trim_start_matches("./");
    let plain = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    match path.split_once('/') {
        Some((first, _)) if plain && !first.is_empty() => {
            crate::reconcile::resolves_inside(store, first)
        }
        _ => false,
    }
}

/// A derived state: the state, what was found, what was not, and the
/// disagreement between the document and the code, if any.
type Derived = (PresumedState, Vec<String>, Vec<String>, Option<String>);

fn derive(store: &GraphStore, body: &Body, source_kind: &str) -> Result<Derived, InterpretError> {
    let mut present = Vec::new();
    let mut missing = Vec::new();
    for p in &body.named.paths {
        if path_exists(store, p)? {
            present.push(p.clone());
        } else if placed_here(store, p) {
            missing.push(p.clone());
        }
        // Otherwise the path is not this repository's to miss: a bare file name
        // (`SKILL.md`) that could live anywhere, or a path under a top-level
        // directory this tree does not have (`sweagent/agent/...`, an upstream
        // project a roadmap cites). Neither present nor missing.
    }
    for s in &body.named.symbols {
        if symbol_exists(store, s)? {
            present.push(s.clone());
        } else {
            missing.push(s.clone());
        }
    }
    for t in &body.tests {
        if test_exists(store, t)? {
            present.push(t.clone());
        } else {
            missing.push(t.clone());
        }
    }
    let doc = document_of(source_kind);
    let (state, detail) = match body.claim {
        Claim::Complete if !missing.is_empty() => (
            PresumedState::Partial,
            Some(format!(
                "the {doc} says done, but this is not found: {}",
                missing.join(", ")
            )),
        ),
        Claim::Complete => (PresumedState::Done, None),
        Claim::Partial => (PresumedState::Partial, None),
        Claim::NotStarted if !present.is_empty() => (
            PresumedState::Partial,
            Some(format!(
                "the {doc} says not started, but this exists: {}",
                present.join(", ")
            )),
        ),
        Claim::Unstated if !present.is_empty() => (PresumedState::Partial, None),
        Claim::NotStarted | Claim::Unstated => (PresumedState::Remaining, None),
    };
    Ok((state, present, missing, detail))
}

/// Every presumed node with its state derived now. A component's state follows
/// its items: done when all are, remaining when all are, partly done otherwise
/// (gone items do not count). The plan roots are included, with the same rule.
pub fn views(store: &GraphStore) -> Result<Vec<PresumedView>, InterpretError> {
    let stored = store.presumed_nodes()?;
    let mut out: Vec<PresumedView> = Vec::with_capacity(stored.len());
    for p in &stored {
        let mut body = body_of(&p.node);
        // The document's claim lives in the presumed row; a row
        // from before that column keeps the one in its body.
        if let Some(c) = claim_of_name(&p.origin.claim) {
            body.claim = c;
            body.claim_text = p.origin.claim_text.clone();
        }
        let (state, present, missing, detail) = if p.node.kind == WorkKind::Item {
            derive(store, &body, &p.origin.source_kind)?
        } else {
            (PresumedState::Remaining, Vec::new(), Vec::new(), None)
        };
        out.push(PresumedView {
            id: p.node.id.clone(),
            plan_id: p.node.plan_id.clone(),
            kind: p.node.kind,
            parent_id: p.node.parent_id.clone(),
            title: p.node.title.clone(),
            source_kind: p.origin.source_kind.clone(),
            source: p.origin.source.clone(),
            section: p.origin.section.clone(),
            parser: p.origin.parser.clone(),
            claim: body.claim,
            claim_text: body.claim_text,
            state,
            present,
            missing,
            detail,
            adopted_by: p.adopted.as_ref().map(|(by, _)| by.clone()),
            edited: !p.as_written,
            gone: p.gone_ms.is_some(),
        });
    }
    // Document order for people: numbers compare as numbers, so "Milestone 10"
    // follows "Milestone 9", not "Milestone 1.6".
    out.sort_by_cached_key(|v| natural_key(&v.id));
    // Groups from their items, deepest first so a component's state is known
    // before its parent's is taken from it.
    // Maps, not scans: parent by id, and children by parent.
    let index: HashMap<String, usize> = out
        .iter()
        .enumerate()
        .map(|(i, v)| (v.id.clone(), i))
        .collect();
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, v) in out.iter().enumerate() {
        if let Some(&p) = v.parent_id.as_ref().and_then(|p| index.get(p)) {
            children.entry(p).or_default().push(i);
        }
    }
    let depth = |i: usize| -> usize {
        let mut d = 0;
        let mut cur = out[i].parent_id.as_ref().and_then(|p| index.get(p));
        // Bounded by the node count: a parent cycle cannot loop forever.
        while let Some(&p) = cur {
            d += 1;
            if d > out.len() {
                break;
            }
            cur = out[p].parent_id.as_ref().and_then(|q| index.get(q));
        }
        d
    };
    let mut groups: Vec<(usize, usize)> = out
        .iter()
        .enumerate()
        .filter(|(_, v)| v.kind != WorkKind::Item)
        .map(|(i, _)| (depth(i), i))
        .collect();
    groups.sort_by(|a, b| b.cmp(a));
    for (_, g) in groups {
        let states: Vec<PresumedState> = children
            .get(&g)
            .map(|c| {
                c.iter()
                    .filter(|&&i| !out[i].gone)
                    .map(|&i| out[i].state)
                    .collect()
            })
            .unwrap_or_default();
        out[g].state = if !states.is_empty() && states.iter().all(|s| *s == PresumedState::Done) {
            PresumedState::Done
        } else if states.iter().all(|s| *s == PresumedState::Remaining) {
            PresumedState::Remaining
        } else {
            PresumedState::Partial
        };
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_finds_roadmap_names_in_the_root_and_docs_only() {
        let dir = tempfile::tempdir().expect("dir");
        let w = |p: &str| {
            let p = dir.path().join(p);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(p, "x").expect("write");
        };
        w("ROADMAP.md");
        w("docs/todo.markdown");
        w("docs/deep/PLAN.md");
        w("README.md");
        w("plan.txt");
        assert_eq!(discover(dir.path()), ["ROADMAP.md", "docs/todo.markdown"]);
    }

    #[test]
    fn a_source_outside_the_root_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        for bad in ["../x.md", "/etc/passwd", ""] {
            assert!(matches!(
                presume_roadmaps(&store, &[bad.to_owned()], 1),
                Err(InterpretError::RoadmapPath { .. })
            ));
        }
        assert!(matches!(
            presume_roadmaps(&store, &["MISSING.md".to_owned()], 1),
            Err(InterpretError::RoadmapUnreadable { .. })
        ));
    }

    /// A cited path reached through a link to a directory outside
    /// the project, or holding `..`, is neither present nor missing; a path
    /// under a real directory of the project that is absent is missing.
    #[cfg(unix)]
    #[test]
    fn cited_paths_do_not_follow_links_out_of_the_project() {
        let dir = tempfile::tempdir().expect("dir");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("a.rs"), "fn a() {}\n").expect("write");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("ext")).expect("link");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        let body = Body {
            claim: Claim::Complete,
            named: Named {
                paths: vec![
                    "ext/a.rs".into(),
                    "ext/none.rs".into(),
                    "../x/y.rs".into(),
                    "src/missing.rs".into(),
                ],
                ..Named::default()
            },
            ..Body::default()
        };
        let (state, present, missing, _) = derive(&store, &body, ROADMAP).expect("derive");
        assert!(
            present.is_empty(),
            "a link out is not presence: {present:?}"
        );
        assert_eq!(missing, ["src/missing.rs"]);
        assert_eq!(state, PresumedState::Partial);
    }

    /// A case-only rename (`TODO.md` to `todo.md`) retires
    /// the old source. On the default macOS file system the old path still
    /// resolves, so a lookup by path alone kept both sources live. Runs on any
    /// file system; it catches the bug only where names are case-insensitive.
    #[test]
    fn a_case_only_rename_retires_the_old_source() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("TODO.md"), "## T\n\n- [ ] one\n- [x] two\n")
            .expect("write");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        presume_roadmaps(&store, &["TODO.md".to_owned()], 1).expect("presume");
        std::fs::rename(dir.path().join("TODO.md"), dir.path().join("todo.md")).expect("mv");
        assert_eq!(discover(dir.path()), ["todo.md"]);

        let retired = retire_missing(&store, 2).expect("retire");
        assert_eq!(retired.len(), 1, "{retired:?}");
        assert_eq!(retired[0].0, "TODO.md");
        presume_roadmaps(&store, &["todo.md".to_owned()], 3).expect("presume");
        let live: Vec<String> = views(&store)
            .expect("views")
            .into_iter()
            .filter(|v| v.kind == WorkKind::Item && !v.gone)
            .map(|v| v.source)
            .collect();
        assert_eq!(live, ["todo.md", "todo.md"], "one live copy of each item");
        // A source whose exact name is still listed is not retired.
        assert!(retire_missing(&store, 4).expect("retire").is_empty());
    }

    /// A roadmap over the size cap is refused with its size,
    /// before it is read, and nothing is written.
    #[test]
    fn a_roadmap_over_the_size_cap_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let big = vec![b'x'; MAX_ROADMAP_BYTES as usize + 1];
        std::fs::write(dir.path().join("ROADMAP.md"), big).expect("write");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        match presume_roadmaps(&store, &["ROADMAP.md".to_owned()], 1) {
            Err(InterpretError::RoadmapTooLarge { bytes, limit, .. }) => {
                assert_eq!((bytes, limit), (MAX_ROADMAP_BYTES + 1, MAX_ROADMAP_BYTES));
            }
            other => panic!("expected RoadmapTooLarge, got {other:?}"),
        }
        assert!(store.presumed_nodes().expect("nodes").is_empty());
        // Exactly at the cap is read.
        std::fs::write(
            dir.path().join("ROADMAP.md"),
            vec![b'x'; MAX_ROADMAP_BYTES as usize],
        )
        .expect("write");
        assert!(presume_roadmaps(&store, &["ROADMAP.md".to_owned()], 2).is_ok());
    }

    /// [`presume_spinup`] is what the shell's spin-up and `kgraf index` both
    /// call: a fresh project with no roadmap says so (no error), a roadmap
    /// present is read and re-reads change nothing, an edit updates in place,
    /// and a deletion retires it.
    #[test]
    fn spinup_reads_updates_and_retires_a_roadmap() {
        let dir = tempfile::tempdir().expect("dir");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");

        // No roadmap: said, not an error.
        let empty = presume_spinup(&store, 1).expect("spinup");
        assert!(empty.no_roadmap.is_some());
        assert!(empty.sources.is_empty() && empty.retired.is_empty());
        assert!(store.presumed_nodes().expect("nodes").is_empty());

        // A roadmap appears.
        std::fs::write(dir.path().join("ROADMAP.md"), "## A\n\n- [ ] one\n").expect("write");
        let first = presume_spinup(&store, 2).expect("spinup");
        assert!(first.no_roadmap.is_none());
        assert_eq!(first.sources.len(), 1);
        assert_eq!(
            first.sources[0].sync.inserted.len(),
            3,
            "the plan, the component and one item"
        );

        // A re-read with nothing changed writes nothing.
        let again = presume_spinup(&store, 3).expect("spinup");
        assert_eq!(again.sources[0].sync.inserted.len(), 0);
        assert_eq!(again.sources[0].sync.updated.len(), 0);

        // An edit updates in place, not a duplicate.
        std::fs::write(dir.path().join("ROADMAP.md"), "## A\n\n- [x] one\n").expect("write");
        let edited = presume_spinup(&store, 4).expect("spinup");
        assert_eq!(edited.sources[0].sync.inserted.len(), 0);
        assert!(
            !edited.sources[0].sync.claims_updated.is_empty(),
            "{:?}",
            edited.sources[0].sync
        );
        let live: Vec<_> = views(&store)
            .expect("views")
            .into_iter()
            .filter(|v| v.kind == WorkKind::Item && !v.gone)
            .collect();
        assert_eq!(live.len(), 1, "one item, not two");

        // Deleting the file marks its nodes gone.
        std::fs::remove_file(dir.path().join("ROADMAP.md")).expect("rm");
        let deleted = presume_spinup(&store, 5).expect("spinup");
        assert!(deleted.no_roadmap.is_some());
        assert_eq!(deleted.retired.len(), 1);
        let gone: Vec<_> = views(&store)
            .expect("views")
            .into_iter()
            .filter(|v| v.kind == WorkKind::Item)
            .collect();
        assert!(gone.iter().all(|v| v.gone), "{gone:?}");
    }

    /// `presume_spinup` takes only the graph: no `Asker`, no ledger, no model or
    /// provider handle reaches it, so no call and no token spend is even
    /// constructible through it. This is a compile-time property (the
    /// signature), checked here by calling it with nothing else in scope that
    /// could make a call.
    #[test]
    fn spinup_spends_no_token() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("TODO.md"), "- [ ] a\n").expect("write");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        presume_spinup(&store, 1).expect("spinup");
        // No ledger file was ever touched: nothing here has a ledger handle.
        assert!(!dir.path().join(".kgraf").join("ledger.db").exists());
    }

    /// A roadmap that is a symbolic link out of the project is not read.
    #[cfg(unix)]
    #[test]
    fn a_roadmap_linked_from_outside_the_project_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.md"), "- [ ] secret\n").expect("write");
        std::os::unix::fs::symlink(outside.path().join("secret.md"), dir.path().join("TODO.md"))
            .expect("link");
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        assert!(matches!(
            presume_roadmaps(&store, &["TODO.md".to_owned()], 1),
            Err(InterpretError::RoadmapPath { .. })
        ));
        assert!(store.presumed_nodes().expect("nodes").is_empty());
    }
}
