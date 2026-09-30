//! A fake model for the interpretation tests: answers through a closure over the
//! brief it was sent, and records every brief. No network (live acceptance
//! is the CLI's `orchestrate --corpus` against a real speaker).

#![allow(dead_code)]

use kgraf_graph::GraphStore;
use kgraf_interpret::{
    AskAnswer, AskFailure, Asker, InterpretError, InterpretRequest, Interpretation, TestResults,
    prepare,
};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Mutex;

type Reply = dyn Fn(&str) -> String + Send + Sync;

/// The fake's identity: part of every plan id.
pub const MODEL: &str = "fake:model";

/// A request for the fake model with the defaults.
/// Whether wall-clock budgets are enforced. Off by default: a budget assertion
/// measures the machine's load as much as the code, so a loaded runner fails a
/// correct build. Set `KGRAF_TIME_BUDGET=1` on a quiet machine to enforce them;
/// the tests print their timings either way, and `benches/` holds the tight ones.
pub fn time_budget_enforced() -> bool {
    std::env::var_os("KGRAF_TIME_BUDGET").is_some_and(|v| !v.is_empty() && v != "0")
}

pub fn request() -> InterpretRequest {
    InterpretRequest::new(MODEL)
}

/// A request with an instruction.
pub fn instruction(text: &str) -> InterpretRequest {
    let mut r = request();
    r.instruction = Some(text.into());
    r
}

pub struct FakeModel {
    reply: Box<Reply>,
    pub briefs: Mutex<Vec<String>>,
}

impl FakeModel {
    pub fn new(reply: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        Self {
            reply: Box::new(reply),
            briefs: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> usize {
        self.briefs.lock().expect("lock").len()
    }

    pub fn last_brief(&self) -> String {
        self.briefs
            .lock()
            .expect("lock")
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Asker for FakeModel {
    fn identity(&self) -> String {
        MODEL.into()
    }

    async fn ask(&self, _system: &str, user: String) -> Result<AskAnswer, AskFailure> {
        let n = {
            let mut b = self.briefs.lock().expect("lock");
            b.push(user.clone());
            b.len()
        };
        Ok(AskAnswer {
            text: (self.reply)(&user),
            call: Some(format!("call-{n}")),
        })
    }
}

/// Prepare, ask and finish.
pub async fn run(
    store: &GraphStore,
    request: InterpretRequest,
    model: &FakeModel,
    tests: &dyn TestResults,
) -> Result<Interpretation, InterpretError> {
    let prepared = prepare(store, request)?;
    prepared.ask(model).await?.finish(store, tests)
}

pub fn write(root: &Path, path: &str, text: &str) {
    let p = root.join(path);
    std::fs::create_dir_all(p.parent().expect("parent")).expect("dir");
    std::fs::write(p, text).expect("write");
}

/// A graph over `root`, stored outside it so the corpus stays as written.
pub fn store(root: &Path, db_dir: &Path) -> GraphStore {
    GraphStore::open(&db_dir.join("graph.db"), root).expect("graph")
}

/// One work item as JSON.
pub fn item(id: &str, deps: &[&str], evidence: Value, claim: &str, acceptance: Value) -> Value {
    json!({
        "id": id,
        "title": format!("item {id}"),
        "instruction": format!("do {id}"),
        "depends_on": deps,
        "acceptance": acceptance,
        "evidence": evidence,
        "writes": [format!("src/{id}.rs")],
        "documents_claim": claim,
        "sources": ["ROADMAP.md"],
    })
}

pub fn answer(items: Vec<Value>, commands: Value, questions: &[&str]) -> String {
    json!({
        "reading": "A small project, read from its documents.",
        "components": [{"id": "core", "name": "Core", "purpose": "the core", "paths": ["src/"]}],
        "constraints": [{"text": "No network access.", "source": "RULES.txt"}],
        "validation_commands": commands,
        "work_items": items,
        "questions": questions,
    })
    .to_string()
}
