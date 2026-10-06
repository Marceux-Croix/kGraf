//! The interpretation pass: prepare (gather, rank, brief), ask (one model call, or
//! none when the same brief was answered by the same model before), finish (parse,
//! validate, assess, reconcile, record).
//!
//! The stages are separate so a caller holding the graph behind a lock never holds
//! it across the model call: `prepare` and `finish` take `&GraphStore`, `ask` takes
//! neither. There is no one-call convenience here on purpose: the model call must
//! be reserved against a budget first, which is the orchestrator's job
//! (`kgraf_orchestrator::Orchestrator::interpret`).
//!
//! **A reading is recorded, and approval names it.** A validated answer is stored
//! in the graph's `meta` under `interpret/reading/<plan id>`, with the validation
//! commands as assessed when it was read and the corpus fingerprint. The plan id is
//! a hash over everything the user is shown and approves: the brief hash, the model
//! identity (`route:model`), the instruction, the corrections, the full answer text,
//! the assessed commands and the corpus fingerprint. Loading recomputes it, so a
//! stored reading edited in any of those is refused. A separate pointer,
//! `interpret/cache/<hash of brief and model>`, finds the reading of the same brief
//! by the same model so it is not asked again. Approval (`approve_stored`) takes the
//! plan id and loads that stored answer and its stored command assessment; it never
//! makes a model call, it refuses an id with no stored reading, and it refuses when
//! the corpus changed since the reading (read again). Reading again after a change
//! the brief does not show (a function body, an omitted section) finds the stored
//! answer and records it anew for the current corpus, with no model call: a new
//! plan id, which approval then accepts. At most [`KEEP_READINGS`]
//! readings are kept; older ones are evicted.
//!
//! Doneness is never stored: it is derived from the code on every finish and every
//! approval.

use crate::brief::{self, Brief, BriefInput, BriefStats};
use crate::corpus::{self, Corpus, CorpusStats, GatherOptions};
use crate::error::InterpretError;
use crate::model::{
    Answer, AssessedCommand, CommandProvenance, DeclaredAs, PlanLimits, Verifiability,
    assess_commands, assess_criterion, parse_answer, validate_answer,
};
use crate::reconcile::{Discrepancy, Doneness, ItemState, TestResults, reconcile};
use kgraf_graph::{GraphStore, WorkKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

/// `meta` key prefix of stored readings.
pub const READING_PREFIX: &str = "interpret/reading/";
/// `meta` key prefix of the (brief, model) to plan id pointers.
pub const CACHE_PREFIX: &str = "interpret/cache/";
/// Readings kept in one graph; the oldest beyond this are evicted.
pub const KEEP_READINGS: usize = 16;
/// The stored-reading format this build writes and reads.
const READING_VERSION: u32 = 2;

/// One model answer and the ledger call that produced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskAnswer {
    pub text: String,
    /// The ledger call id, when a call was made.
    pub call: Option<String>,
}

/// A failed ask, with the ledger call when one was opened, so it can be settled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskFailure {
    pub message: String,
    pub call: Option<String>,
    /// The model's output was malformed (the server could not parse it), so
    /// another sample of the same ask is worth making. The caller decides
    /// whether to ask again; this crate never does.
    pub malformed: bool,
}

/// The model, through whatever accounting path the caller owns. The orchestrator's
/// implementation goes through its ledgered caller (every token is attributed to the ledger); this crate never
/// reaches a provider itself.
#[async_trait::async_trait]
pub trait Asker: Send + Sync {
    /// `route:model`: which model answers. Part of the plan id.
    fn identity(&self) -> String;
    async fn ask(&self, system: &str, user: String) -> Result<AskAnswer, AskFailure>;
}

/// What to interpret.
#[derive(Clone, Debug)]
pub struct InterpretRequest {
    /// The user's instruction, verbatim. Required for an empty directory.
    pub instruction: Option<String>,
    /// Corrections to a previous reading, verbatim, in order.
    pub corrections: Vec<String>,
    /// The input bound in estimated tokens (0057: 40% of the planner's effective
    /// window; `ContextConfig::worker_plan_target_tokens`).
    pub budget_tokens: u64,
    /// `route:model` of the asker; part of the plan id.
    pub model: String,
    /// The orchestrator's plan limits, checked now rather than at dispatch.
    pub limits: PlanLimits,
    pub gather: GatherOptions,
}

impl InterpretRequest {
    /// A request for `model` with the defaults.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            instruction: None,
            corrections: Vec::new(),
            budget_tokens: brief::DEFAULT_INPUT_BUDGET,
            model: model.into(),
            limits: PlanLimits::default(),
            gather: GatherOptions::default(),
        }
    }
}

/// The three acceptance modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// An empty directory and an instruction.
    Greenfield,
    /// A project with documentation and no planned work done yet.
    Existing,
    /// A project where some planned work already exists in the code.
    Resume,
}

/// Wall-clock milliseconds of each stage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Timings {
    /// Indexing and ingest.
    pub gather_ms: u64,
    /// The index pass alone, within `gather_ms`.
    pub index_ms: u64,
    /// Ranking and brief assembly.
    pub brief_ms: u64,
    /// The model call (zero when the reading was stored).
    pub model_ms: u64,
    /// Parsing, validation, assessment and reconciliation.
    pub reconcile_ms: u64,
    /// From the start of `prepare` to a plan with its first dispatchable item named:
    /// time to first useful action.
    pub first_action_ms: u64,
}

/// What a stored reading holds. Written only by `finish` after validation, and
/// validated again on load: its plan id must be the one its own contents hash to,
/// and its answer must parse and validate.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReading {
    version: u32,
    plan_id: String,
    model: String,
    brief_hash: String,
    corpus_fingerprint: String,
    instruction: Option<String>,
    corrections: Vec<String>,
    answer: AskAnswer,
    /// The validation commands as assessed against the corpus when it was read:
    /// what the user saw, and what approval declares from.
    commands: Vec<AssessedCommand>,
    greenfield: bool,
    created_ms: i64,
    brief_tokens: u64,
    budget_tokens: u64,
    sections_included: usize,
    sections_total: usize,
    sections_omitted: usize,
    definitions_included: usize,
    definitions_ranked: usize,
}

/// The (brief, model) key a reading is found by before it is asked for.
fn cache_key_of(brief_hash: &str, model: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(brief_hash.as_bytes());
    h.update(&[0]);
    h.update(model.as_bytes());
    h.finalize().to_hex()[..32].to_owned()
}

/// `plan-` and the first 16 hex digits of blake3 over everything the user is shown
/// and approves (see the module documentation).
fn plan_id_of(r: &StoredReading) -> Result<String, InterpretError> {
    let covered = serde_json::to_string(&(
        &r.brief_hash,
        &r.model,
        &r.instruction,
        &r.corrections,
        &r.answer.text,
        &r.commands,
        &r.corpus_fingerprint,
    ))
    .map_err(|e| InterpretError::InvalidPlan(format!("cannot hash the reading: {e}")))?;
    Ok(format!(
        "plan-{}",
        &blake3::hash(covered.as_bytes()).to_hex()[..16]
    ))
}

fn reading_key(plan_id: &str) -> String {
    format!("{READING_PREFIX}{plan_id}")
}

fn cache_pointer_key(cache_key: &str) -> String {
    format!("{CACHE_PREFIX}{cache_key}")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Load and validate a stored reading; `None` when there is none.
fn load_reading(
    store: &GraphStore,
    plan_id: &str,
    limits: &PlanLimits,
) -> Result<Option<(StoredReading, Answer)>, InterpretError> {
    let Some(json) = store.meta(&reading_key(plan_id))? else {
        return Ok(None);
    };
    let bad = |reason: String| InterpretError::BadStoredReading {
        plan_id: plan_id.to_owned(),
        reason,
    };
    let r: StoredReading = serde_json::from_str(&json).map_err(|e| bad(e.to_string()))?;
    if r.version != READING_VERSION {
        return Err(bad(format!(
            "format {} is not {READING_VERSION}",
            r.version
        )));
    }
    if r.plan_id != plan_id || plan_id_of(&r)? != plan_id {
        return Err(bad("its contents do not hash to its plan id".into()));
    }
    let answer = parse_answer(&r.answer.text)?;
    validate_answer(&answer, limits)?;
    Ok(Some((r, answer)))
}

/// Keep the newest [`KEEP_READINGS`] readings.
fn evict_readings(store: &GraphStore) -> Result<(), InterpretError> {
    let mut dated: Vec<(i64, String)> = Vec::new();
    for key in store.meta_keys(READING_PREFIX)? {
        let created = store
            .meta(&key)?
            .and_then(|j| serde_json::from_str::<StoredReading>(&j).ok())
            .map_or(i64::MIN, |r| r.created_ms);
        dated.push((created, key));
    }
    dated.sort_by(|a, b| b.cmp(a));
    for (_, key) in dated.into_iter().skip(KEEP_READINGS) {
        store.clear_meta(&key)?;
    }
    // Pointers to evicted readings go too.
    for key in store.meta_keys(CACHE_PREFIX)? {
        let target = store.meta(&key)?;
        let alive = match target {
            Some(id) => store.meta(&reading_key(&id))?.is_some(),
            None => false,
        };
        if !alive {
            store.clear_meta(&key)?;
        }
    }
    Ok(())
}

/// A prepared brief.
pub struct Prepared {
    started: Instant,
    request: InterpretRequest,
    brief: Brief,
    cache_key: String,
    corpus: CorpusStats,
    fingerprint: String,
    greenfield: bool,
    /// Documents as (path, text), for checking where commands come from.
    texts: Vec<(String, String)>,
    stored: Option<StoredReading>,
    prior_plans: Vec<String>,
    timings: Timings,
}

fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

fn texts_of(corpus: &Corpus) -> Vec<(String, String)> {
    corpus
        .documents
        .iter()
        .map(|d| (d.path.display().to_string(), d.text.clone()))
        .collect()
}

/// Gather the corpus, rank it and build the bounded brief; find a stored reading
/// of the same brief by the same model.
pub fn prepare(store: &GraphStore, request: InterpretRequest) -> Result<Prepared, InterpretError> {
    if request.model.trim().is_empty() {
        return Err(InterpretError::NoModel);
    }
    let started = Instant::now();
    let corpus = corpus::gather(store, &request.gather)?;
    let gather_ms = ms(started);
    if corpus.is_empty()
        && request
            .instruction
            .as_deref()
            .is_none_or(|i| i.trim().is_empty())
    {
        return Err(InterpretError::NothingToInterpret {
            root: corpus.root.clone(),
        });
    }
    let t = Instant::now();
    let (brief, _rank) = brief::build(
        &corpus,
        store,
        &BriefInput {
            instruction: request.instruction.as_deref(),
            corrections: &request.corrections,
            budget_tokens: request.budget_tokens,
        },
    )?;
    let brief_ms = ms(t);
    let cache_key = cache_key_of(&brief.hash, &request.model);
    // A pointer whose reading was evicted is a miss; a reading that fails
    // validation is an error.
    let stored = match store.meta(&cache_pointer_key(&cache_key))? {
        Some(plan_id) => load_reading(store, &plan_id, &request.limits)?.map(|(r, _)| r),
        None => None,
    };
    let prior_plans = store
        .work_plans()?
        .into_iter()
        .filter(|n| n.kind == WorkKind::Plan)
        // The plan `kgraf schedule add` creates to hold scheduled work is not
        // an interpretation (decision 0052 section 3), so it is not a prior plan.
        .filter(|n| n.plan_id != kgraf_graph::SCHEDULE_PLAN_ID)
        .map(|n| n.plan_id)
        .collect();
    Ok(Prepared {
        started,
        greenfield: corpus.is_empty(),
        texts: texts_of(&corpus),
        fingerprint: corpus.fingerprint.clone(),
        corpus: corpus.stats(),
        timings: Timings {
            gather_ms,
            index_ms: corpus.index_ms,
            brief_ms,
            ..Timings::default()
        },
        request,
        brief,
        cache_key,
        stored,
        prior_plans,
    })
}

impl Prepared {
    pub fn brief(&self) -> &Brief {
        &self.brief
    }

    /// The (brief, model) key a stored reading is found by. Not the plan id: that
    /// covers the answer too, so it exists only after the answer.
    pub fn cache_key(&self) -> &str {
        &self.cache_key
    }

    /// Whether this brief was answered by this model before (no call will be made).
    pub fn is_cached(&self) -> bool {
        self.stored.is_some()
    }

    /// Estimated input tokens of the call.
    pub fn input_tokens(&self) -> u64 {
        self.brief.stats.tokens
    }

    /// Ask the model, unless this brief was answered by this model before. The
    /// asker must be the model the request names.
    pub async fn ask(mut self, asker: &dyn Asker) -> Result<Answered, InterpretError> {
        let identity = asker.identity();
        if identity != self.request.model {
            return Err(InterpretError::ModelMismatch {
                expected: self.request.model.clone(),
                got: identity,
            });
        }
        if let Some(r) = &self.stored {
            let answer = r.answer.clone();
            return Ok(Answered {
                prepared: self,
                answer,
                cached: true,
            });
        }
        let t = Instant::now();
        let answer = asker
            .ask(self.brief.system, self.brief.user.clone())
            .await
            .map_err(|f| InterpretError::Model {
                message: f.message,
                call: f.call,
                malformed: f.malformed,
            })?;
        self.timings.model_ms = ms(t);
        Ok(Answered {
            prepared: self,
            answer,
            cached: false,
        })
    }
}

/// A model answer not yet checked.
pub struct Answered {
    prepared: Prepared,
    answer: AskAnswer,
    cached: bool,
}

impl Answered {
    /// The ledger call, when one was made now.
    pub fn call(&self) -> Option<&str> {
        if self.cached {
            None
        } else {
            self.answer.call.as_deref()
        }
    }

    pub fn is_cached(&self) -> bool {
        self.cached
    }

    /// Parse, validate, assess and reconcile; record a new valid reading.
    pub fn finish(
        self,
        store: &GraphStore,
        tests: &dyn TestResults,
    ) -> Result<Interpretation, InterpretError> {
        let t = Instant::now();
        let Answered {
            prepared: p,
            answer: asked,
            cached,
        } = self;
        let answer = parse_answer(&asked.text)?;
        validate_answer(&answer, &p.request.limits)?;
        // A stored reading of this brief by this model is reused as is only when
        // the corpus is the one it was read from. The brief can be unchanged while
        // the corpus changed (a function body, a section the bound left out); then
        // the same answer is recorded again, with no model call, as a new reading:
        // commands re-assessed against the corpus as it is, the new fingerprint, a
        // new plan id to show and approve.
        let current = p
            .stored
            .as_ref()
            .filter(|r| r.corpus_fingerprint == p.fingerprint);
        let (plan_id, commands) = if let Some(r) = current {
            (r.plan_id.clone(), r.commands.clone())
        } else {
            let commands = assess_commands(
                &answer.validation_commands,
                p.texts.iter().map(|(a, b)| (a.as_str(), b.as_str())),
            );
            let s = &p.brief.stats;
            let mut record = StoredReading {
                version: READING_VERSION,
                plan_id: String::new(),
                model: p.request.model.clone(),
                brief_hash: p.brief.hash.clone(),
                corpus_fingerprint: p.fingerprint.clone(),
                instruction: p.request.instruction.clone(),
                corrections: p.request.corrections.clone(),
                answer: asked.clone(),
                commands: commands.clone(),
                greenfield: p.greenfield,
                created_ms: now_ms(),
                brief_tokens: s.tokens,
                budget_tokens: s.budget_tokens,
                sections_included: s.sections_included,
                sections_total: s.sections_total,
                sections_omitted: s.omitted.len(),
                definitions_included: s.definitions_included,
                definitions_ranked: s.definitions_ranked,
            };
            record.plan_id = plan_id_of(&record)?;
            let json = serde_json::to_string(&record).map_err(|e| {
                InterpretError::InvalidPlan(format!("cannot serialize the reading: {e}"))
            })?;
            store.set_meta(&reading_key(&record.plan_id), &json)?;
            store.set_meta(&cache_pointer_key(&p.cache_key), &record.plan_id)?;
            evict_readings(store)?;
            (record.plan_id, commands)
        };
        let mut i = Interpretation::assemble(
            store,
            Parts {
                plan_id,
                greenfield: p.greenfield,
                instruction: p.request.instruction,
                corrections: p.request.corrections,
                answer,
                commands,
                brief: p.brief.stats,
                corpus: p.corpus,
                prior_plans: p.prior_plans,
                cached,
                call: asked.call,
                model: p.request.model.clone(),
            },
            tests,
        )?;
        i.timings = p.timings;
        i.timings.reconcile_ms = ms(t);
        i.timings.first_action_ms = ms(p.started);
        Ok(i)
    }
}

/// Assess every criterion against the declared commands.
pub(crate) fn assess_all(
    answer: &Answer,
    declared: &BTreeSet<String>,
) -> BTreeMap<String, Vec<Verifiability>> {
    answer
        .work_items
        .iter()
        .map(|w| {
            (
                w.id.clone(),
                w.acceptance
                    .iter()
                    .map(|c| assess_criterion(c, declared))
                    .collect(),
            )
        })
        .collect()
}

/// The commands a project document declared.
pub(crate) fn discovered(commands: &[AssessedCommand]) -> BTreeSet<String> {
    commands
        .iter()
        .filter(|c| matches!(c.provenance, CommandProvenance::Discovered { .. }))
        .map(|c| c.command.clone())
        .collect()
}

/// A criterion that cannot be checked automatically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Unverifiable {
    pub item: String,
    pub criterion: String,
    pub reason: String,
    pub needs: String,
}

/// What an [`Interpretation`] is assembled from.
pub(crate) struct Parts {
    pub plan_id: String,
    pub greenfield: bool,
    pub instruction: Option<String>,
    pub corrections: Vec<String>,
    pub answer: Answer,
    /// As assessed when the reading was made.
    pub commands: Vec<AssessedCommand>,
    pub brief: BriefStats,
    pub corpus: CorpusStats,
    pub prior_plans: Vec<String>,
    pub cached: bool,
    pub call: Option<String>,
    /// `route:model` of the reading, as recorded with it.
    pub model: String,
}

/// A corpus read into a plan model, not yet approved.
///
/// Its fields are private and it cannot be deserialized: one is made only by the
/// pass (`finish`) or by loading a validated stored reading (`approve_stored`), so
/// an approval can never be of a hand-built or edited reading.
#[derive(Clone, Debug)]
pub struct Interpretation {
    pub(crate) plan_id: String,
    pub(crate) mode: Mode,
    pub(crate) instruction: Option<String>,
    pub(crate) corrections: Vec<String>,
    pub(crate) answer: Answer,
    pub(crate) commands: Vec<AssessedCommand>,
    pub(crate) criteria: BTreeMap<String, Vec<Verifiability>>,
    pub(crate) states: Vec<ItemState>,
    pub(crate) discrepancies: Vec<Discrepancy>,
    pub(crate) brief: BriefStats,
    pub(crate) corpus: CorpusStats,
    pub(crate) prior_plans: Vec<String>,
    pub(crate) cached: bool,
    pub(crate) call: Option<String>,
    pub(crate) model: String,
    pub(crate) first_action: Option<String>,
    pub(crate) timings: Timings,
}

impl Interpretation {
    pub(crate) fn assemble(
        store: &GraphStore,
        p: Parts,
        tests: &dyn TestResults,
    ) -> Result<Self, InterpretError> {
        let commands = p.commands;
        let criteria = assess_all(&p.answer, &discovered(&commands));
        let (states, discrepancies) = reconcile(store, &p.answer, tests)?;
        let mode = if p.greenfield {
            Mode::Greenfield
        } else if states.iter().any(|s| s.doneness != Doneness::NotStarted) {
            Mode::Resume
        } else {
            Mode::Existing
        };
        let mut i = Interpretation {
            plan_id: p.plan_id,
            mode,
            instruction: p.instruction,
            corrections: p.corrections,
            answer: p.answer,
            commands,
            criteria,
            states,
            discrepancies,
            brief: p.brief,
            corpus: p.corpus,
            prior_plans: p.prior_plans,
            cached: p.cached,
            call: p.call,
            model: p.model,
            first_action: None,
            timings: Timings::default(),
        };
        i.first_action = i.next_dispatchable();
        Ok(i)
    }

    /// The id approval must name.
    pub fn plan_id(&self) -> &str {
        &self.plan_id
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn instruction(&self) -> Option<&str> {
        self.instruction.as_deref()
    }

    pub fn answer(&self) -> &Answer {
        &self.answer
    }

    pub fn commands(&self) -> &[AssessedCommand] {
        &self.commands
    }

    /// The assessment of each of an item's criteria, in order.
    pub fn criteria(&self, item: &str) -> &[Verifiability] {
        self.criteria.get(item).map_or(&[], Vec::as_slice)
    }

    pub fn states(&self) -> &[ItemState] {
        &self.states
    }

    pub fn discrepancies(&self) -> &[Discrepancy] {
        &self.discrepancies
    }

    pub fn brief(&self) -> &BriefStats {
        &self.brief
    }

    pub fn corpus(&self) -> &CorpusStats {
        &self.corpus
    }

    /// Plans already seeded in this graph, newest first.
    pub fn prior_plans(&self) -> &[String] {
        &self.prior_plans
    }

    /// Whether the answer was a stored reading (no model call this time).
    pub fn is_cached(&self) -> bool {
        self.cached
    }

    /// The ledger call that produced the answer (the original one when stored).
    pub fn call(&self) -> Option<&str> {
        self.call.as_deref()
    }

    /// `route:model` of the model that answered.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The first item that can be dispatched now.
    pub fn first_action(&self) -> Option<&str> {
        self.first_action.as_deref()
    }

    pub fn timings(&self) -> &Timings {
        &self.timings
    }

    pub fn state(&self, item: &str) -> Option<&ItemState> {
        self.states.iter().find(|s| s.item == item)
    }

    pub(crate) fn complete(&self, item: &str) -> bool {
        self.state(item)
            .is_some_and(|s| s.doneness == Doneness::Complete)
    }

    /// The first item in plan order that is not complete and whose dependencies
    /// are.
    pub fn next_dispatchable(&self) -> Option<String> {
        self.answer
            .work_items
            .iter()
            .find(|w| !self.complete(&w.id) && w.depends_on.iter().all(|d| self.complete(d)))
            .map(|w| w.id.clone())
    }

    /// Every criterion that cannot be checked automatically.
    pub fn unverifiable(&self) -> Vec<Unverifiable> {
        let mut out = Vec::new();
        for w in &self.answer.work_items {
            for (c, v) in w.acceptance.iter().zip(self.criteria(&w.id)) {
                if let Verifiability::Unverifiable { reason, needs } = v {
                    out.push(Unverifiable {
                        item: w.id.clone(),
                        criterion: c.text.clone(),
                        reason: reason.clone(),
                        needs: needs.clone(),
                    });
                }
            }
        }
        out
    }

    /// Proposed commands: gates only if the user accepts each by name.
    pub fn proposed_commands(&self) -> Vec<&str> {
        self.commands
            .iter()
            .filter(|c| c.provenance == CommandProvenance::Proposed)
            .map(|c| c.command.as_str())
            .collect()
    }

    /// The reading, stated back to the user for approval or correction.
    pub fn reading(&self) -> String {
        let a = &self.answer;
        let mut out = String::new();
        out.push_str(&format!(
            "Plan {} ({:?})\n\n{}\n",
            self.plan_id, self.mode, a.reading
        ));
        if self.cached {
            out.push_str(
                "\n(stored reading of this same brief by this same model; no model call)\n",
            );
        }
        let b = &self.brief;
        out.push_str(&format!(
            "\nRead {} of {} document sections and {} of {} ranked definitions in {} of {} \
             input tokens.\n",
            b.sections_included,
            b.sections_total,
            b.definitions_included,
            b.definitions_ranked,
            b.tokens,
            b.budget_tokens
        ));
        if !b.omitted.is_empty() {
            out.push_str(&format!(
                "{} sections were not read (input bound); the first: {}\n",
                b.omitted.len(),
                b.omitted
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for u in &self.corpus.unreadable {
            out.push_str(&format!("Could not read {}: {}\n", u.path, u.error));
        }
        for n in &self.corpus.nested_repositories_skipped {
            out.push_str(&format!("Not read (a nested checkout): {n}\n"));
        }
        let excluded = &self.corpus.excluded_directories;
        if !excluded.is_empty() {
            out.push_str(&format!(
                "Not read ({} excluded directories: build output, dependencies): {}{}\n",
                excluded.len(),
                excluded
                    .iter()
                    .take(20)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
                if excluded.len() > 20 { ", ..." } else { "" }
            ));
        }
        if !a.components.is_empty() {
            out.push_str("\nComponents:\n");
            for c in &a.components {
                out.push_str(&format!("- {} {}: {}\n", c.id, c.name, c.purpose));
            }
        }
        out.push_str("\nWork items:\n");
        for w in &a.work_items {
            let state = self.state(&w.id);
            out.push_str(&format!(
                "- {} [{}] {}{}\n",
                w.id,
                state.map_or("?", |s| match s.doneness {
                    Doneness::Complete => "complete",
                    Doneness::Partial => "partial",
                    Doneness::NotStarted => "not started",
                }),
                w.title,
                if w.depends_on.is_empty() {
                    String::new()
                } else {
                    format!(" (after {})", w.depends_on.join(", "))
                }
            ));
            if let Some(s) = state
                && s.underivable
            {
                out.push_str("    names no evidence: doneness cannot be derived\n");
            }
        }
        if let Some(f) = &self.first_action {
            out.push_str(&format!("\nFirst dispatchable item: {f}\n"));
        }
        if !self.discrepancies.is_empty() {
            out.push_str("\nWhere the documents and the code disagree:\n");
            for d in &self.discrepancies {
                out.push_str(&format!("- {}: {}\n", d.item, d.detail));
            }
        }
        let flags = self.unverifiable();
        if !flags.is_empty() {
            out.push_str("\nAcceptance criteria that cannot be checked automatically:\n");
            for f in &flags {
                out.push_str(&format!(
                    "- {}: \"{}\": {}; needs {}\n",
                    f.item, f.criterion, f.reason, f.needs
                ));
            }
        }
        if !self.commands.is_empty() {
            out.push_str("\nValidation commands:\n");
            for c in &self.commands {
                match &c.provenance {
                    CommandProvenance::Discovered { path, declared_as } => {
                        out.push_str(&format!(
                            "- {} (declared in {path} as {}{})\n",
                            c.command,
                            declared_as.describe(),
                            if *declared_as == DeclaredAs::ProseLine {
                                "; a line of prose that is exactly this text, check it is meant as a command"
                            } else {
                                ""
                            }
                        ));
                    }
                    CommandProvenance::Proposed => out.push_str(&format!(
                        "- {} (proposed: no document declares it; a gate only if you accept it by name)\n",
                        c.command
                    )),
                }
            }
        }
        if !a.constraints.is_empty() {
            out.push_str("\nConstraints:\n");
            for c in &a.constraints {
                out.push_str(&format!(
                    "- {}{}\n",
                    c.text,
                    c.source
                        .as_deref()
                        .map(|s| format!(" ({s})"))
                        .unwrap_or_default()
                ));
            }
        }
        if !a.questions.is_empty() {
            out.push_str("\nQuestions:\n");
            for q in &a.questions {
                out.push_str(&format!("- {q}\n"));
            }
        }
        out
    }
}

/// Load the stored reading `plan_id` for approval: no model call. The answer and
/// the command assessment are the ones stored, what the user was shown. The corpus
/// is gathered again only to check it is unchanged (refused when it changed: read
/// again) and to derive doneness now.
pub(crate) fn load_for_approval(
    store: &GraphStore,
    plan_id: &str,
    limits: &PlanLimits,
    gather: &GatherOptions,
    tests: &dyn TestResults,
) -> Result<Interpretation, InterpretError> {
    let (r, answer) =
        load_reading(store, plan_id, limits)?.ok_or_else(|| InterpretError::NoSuchReading {
            plan_id: plan_id.to_owned(),
        })?;
    let corpus = corpus::gather(store, gather)?;
    if corpus.fingerprint != r.corpus_fingerprint {
        return Err(InterpretError::CorpusChanged {
            plan_id: plan_id.to_owned(),
        });
    }
    Interpretation::assemble(
        store,
        Parts {
            plan_id: r.plan_id.clone(),
            greenfield: r.greenfield,
            instruction: r.instruction.clone(),
            corrections: r.corrections.clone(),
            answer,
            commands: r.commands.clone(),
            brief: BriefStats {
                budget_tokens: r.budget_tokens,
                tokens: r.brief_tokens,
                sections_included: r.sections_included,
                sections_total: r.sections_total,
                definitions_included: r.definitions_included,
                definitions_ranked: r.definitions_ranked,
                ..BriefStats::default()
            },
            corpus: corpus.stats(),
            prior_plans: Vec::new(),
            cached: true,
            call: r.answer.call.clone(),
            model: r.model.clone(),
        },
        tests,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeclaredAs;
    use crate::reconcile::NoTestRuns;

    struct Fixed(String);

    #[async_trait::async_trait]
    impl Asker for Fixed {
        fn identity(&self) -> String {
            "fake:model".into()
        }

        async fn ask(&self, _system: &str, _user: String) -> Result<AskAnswer, AskFailure> {
            Ok(AskAnswer {
                text: self.0.clone(),
                call: None,
            })
        }
    }

    /// Approval declares from the command assessment stored with the reading, not
    /// from a fresh one. Shown directly, with the corpus unchanged (so the corpus
    /// check does not mask it): a stored reading whose assessment says `make evil`
    /// was declared (validly hashed) is approved with it as a gate, although the
    /// corpus declares nothing; the same reading as recorded keeps it proposed.
    #[tokio::test]
    async fn approval_declares_from_the_stored_assessment() {
        let dir = tempfile::tempdir().expect("dir");
        let db = tempfile::tempdir().expect("db");
        std::fs::write(dir.path().join("README.md"), "# T\n\nthing\n").expect("write");
        let store = GraphStore::open(&db.path().join("g.db"), dir.path()).expect("store");
        let reply = serde_json::json!({"reading": "r",
            "validation_commands": [{"command": "make evil"}],
            "work_items": [{"id": "a", "title": "a", "instruction": "do",
                "acceptance": [{"text": "x"}]}]})
        .to_string();
        let i = prepare(&store, InterpretRequest::new("fake:model"))
            .expect("prepare")
            .ask(&Fixed(reply))
            .await
            .expect("ask")
            .finish(&store, &NoTestRuns)
            .expect("finish");
        let id = i.plan_id().to_owned();
        let limits = PlanLimits::default();
        let gather = GatherOptions::default();
        let as_read =
            crate::approve::approve_stored(&store, &id, &[], &limits, &gather, &NoTestRuns)
                .expect("approve");
        assert!(as_read.validation_commands().is_empty(), "proposed as read");

        // Rewrite the stored assessment (and its plan id, so it validates).
        let json = store
            .meta(&reading_key(&id))
            .expect("meta")
            .expect("stored");
        let mut r: StoredReading = serde_json::from_str(&json).expect("parse");
        r.commands[0].provenance = CommandProvenance::Discovered {
            path: "README.md".into(),
            declared_as: DeclaredAs::CodeSpan,
        };
        r.plan_id = plan_id_of(&r).expect("hash");
        store
            .set_meta(
                &reading_key(&r.plan_id),
                &serde_json::to_string(&r).expect("json"),
            )
            .expect("store");
        let stored =
            crate::approve::approve_stored(&store, &r.plan_id, &[], &limits, &gather, &NoTestRuns)
                .expect("approve");
        assert_eq!(stored.validation_commands(), ["make evil".to_owned()]);
    }
}
