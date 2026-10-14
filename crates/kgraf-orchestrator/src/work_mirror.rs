//! The plan in the graph, mirror only (decision 0100, section 2).
//!
//! [`mirror`] turns a run's record into the rows the graph holds for it: one
//! plan root, one item per contract, the dependencies between items, and the
//! `work:` facts of each item's history. It is a pure function of the record:
//! the record is the truth, the mirror is derived, and on any disagreement the
//! record wins ([`rebuild`] writes the mirror again from it).
//!
//! - The plan root is `run:<run id>`. [`RunStateRecord::plan_root`] keeps its
//!   meaning (the run whose `decompose` call made the plan), and
//!   [`RunStateRecord::source_plan`] keeps its (an approved corpus plan, which
//!   switches the held-plan re-plan and the held interface checks off); the
//!   mirror sets neither.
//! - An item is `run:<run id>/item/<contract>`, one per contract id with no
//!   revision suffix: a revision keeps the id and a replacement is a new
//!   contract with a new id. The body is the contract's latest spec, whether
//!   the contract is open, verified or removed (a closed contract keeps its
//!   whole spec in the record).
//! - Facts, all subject = the item: `work:revised_by` `change:<n>` for each
//!   applied change that added, amended or replaced it; `work:removed_by`
//!   `change:<n>` for a change that removed it or replaced it by another id;
//!   `work:verified_commit` the commit that verified it. Nothing adopts
//!   presumed nodes yet, so there is no `work:adopts` fact.
//! - No status is stored. Whether an item is done, running or held is derived
//!   from the record where it is drawn (`kgraf-shell`'s `plan_nodes`).
//!
//! The write is one transaction with the record ([`crate::RunStore::save`]),
//! diffed against a hash per item so an unchanged item is not written again.

use crate::run_state::RunStateRecord;
use kgraf_graph::{MirrorFact, PlanMirror, WorkKind, WorkNode};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};

/// The plan root's id and plan id for a run.
pub fn plan_id(run: &str) -> String {
    format!("run:{run}")
}

/// A contract's item id: stable for the contract's whole life.
pub fn item_id(run: &str, contract: &str) -> String {
    format!("run:{run}/item/{contract}")
}

/// The run a plan root id names, when it is a run's.
pub fn run_of_plan(plan: &str) -> Option<&str> {
    plan.strip_prefix("run:")
}

fn title_of(text: &str, fallback: &str) -> String {
    const MAX: usize = 60;
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(fallback)
        .trim();
    line.chars().take(MAX).collect()
}

/// A contract's latest spec as JSON, and the dependencies in it.
fn spec_of(record: &RunStateRecord, id: &str) -> (serde_json::Value, Vec<String>) {
    if let Some(spec) = record.plan_specs.get(id) {
        return (
            serde_json::to_value(spec).unwrap_or(serde_json::Value::Null),
            spec.depends_on.clone(),
        );
    }
    // A record from before specs were kept, or a contract no plan named (a
    // handoff child): what the record holds of it.
    if let Some(c) = record.contract(id) {
        let writes: Vec<&str> = c.writes.iter().map(|w| w.path.as_str()).collect();
        return (
            serde_json::json!({
                "id": id,
                "instruction": c.instruction,
                "acceptance": c.acceptance,
                "writes": writes,
                "depends_on": c.depends_on,
            }),
            c.depends_on.clone(),
        );
    }
    if let Some(c) = record
        .coordination
        .closed
        .iter()
        .rev()
        .find(|c| c.contract == id)
    {
        return (
            serde_json::json!({
                "id": id,
                "instruction": c.instruction,
                "acceptance": c.acceptance,
                "writes": c.writes,
            }),
            Vec::new(),
        );
    }
    (serde_json::json!({ "id": id }), Vec::new())
}

/// The rows the graph holds for `record`. Pure: the same record gives the same
/// mirror, in the same order.
pub fn mirror(record: &RunStateRecord) -> PlanMirror {
    let run = record.run.as_str();
    let root = plan_id(run);
    let mut ids: BTreeSet<&str> = BTreeSet::new();
    ids.extend(record.plan_specs.keys().map(String::as_str));
    ids.extend(record.open_contracts.iter().map(|c| c.contract.as_str()));
    ids.extend(record.verified_contracts.iter().map(String::as_str));
    ids.extend(record.split_contracts.iter().map(String::as_str));
    ids.extend(
        record
            .coordination
            .closed
            .iter()
            .map(|c| c.contract.as_str()),
    );
    let mut nodes = vec![WorkNode {
        id: root.clone(),
        plan_id: root.clone(),
        kind: WorkKind::Plan,
        parent_id: None,
        title: title_of(&record.original_instruction, run),
        body: serde_json::json!({
            "run": run,
            "instruction": record.original_instruction,
            "plan_root": record.plan_root.as_ref().map(|r| r.as_str()),
            "source_plan": record.source_plan,
        })
        .to_string(),
        created_ms: record.updated_ms,
    }];
    let mut deps = Vec::new();
    for id in &ids {
        let (body, wants) = spec_of(record, id);
        let instruction = body
            .get("instruction")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let item = item_id(run, id);
        nodes.push(WorkNode {
            id: item.clone(),
            plan_id: root.clone(),
            kind: WorkKind::Item,
            parent_id: Some(root.clone()),
            title: title_of(instruction, id),
            body: body.to_string(),
            created_ms: record.updated_ms,
        });
        let mut wants: Vec<&String> = wants.iter().filter(|d| ids.contains(d.as_str())).collect();
        wants.sort();
        wants.dedup();
        for d in wants {
            deps.push((item.clone(), item_id(run, d)));
        }
    }
    let mut facts = Vec::new();
    let mut fact = |id: &str, predicate: &str, object: String, valid_at: i64| {
        if ids.contains(id) {
            facts.push(MirrorFact {
                subject: item_id(run, id),
                predicate: predicate.to_owned(),
                object,
                valid_at,
            });
        }
    };
    for change in &record.changes {
        let Some(ops) = &change.applied else { continue };
        let by = format!("change:{}", change.number);
        let at = change.received_ms;
        let revised = ops
            .added
            .iter()
            .chain(&ops.amended)
            .chain(ops.superseded.iter().map(|(_, new)| new))
            .chain(ops.followups.iter().map(|(_, new)| new));
        for id in revised {
            fact(id, "work:revised_by", by.clone(), at);
        }
        let removed = ops
            .removed
            .iter()
            .chain(ops.superseded.iter().map(|(old, _)| old));
        for id in removed {
            fact(id, "work:removed_by", by.clone(), at);
        }
    }
    for (id, commit) in &record.verified_commits {
        fact(
            id,
            "work:verified_commit",
            commit.sha.clone(),
            commit.verified_ms,
        );
    }
    PlanMirror {
        plan_id: root,
        nodes,
        deps,
        facts,
        recorded_ms: record.updated_ms,
        full: true,
    }
}

/// A hash of everything [`mirror`] reads of a record, taken without building a
/// row: a record with the same fingerprint gives the same rows, so a save that
/// did not touch the plan builds and diffs nothing. It is a field-by-field hash,
/// with no allocation. A test changes each field `mirror` reads and finds the
/// fingerprint moves with the rows.
pub fn fingerprint(record: &RunStateRecord) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    record.run.as_str().hash(&mut h);
    record.original_instruction.hash(&mut h);
    record.plan_root.as_ref().map(|r| r.as_str()).hash(&mut h);
    record.source_plan.hash(&mut h);
    for (key, spec) in &record.plan_specs {
        (key, &spec.id, &spec.instruction, &spec.acceptance).hash(&mut h);
        for w in &spec.writes {
            (&w.path, w.node.as_ref().map(|n| n.as_str())).hash(&mut h);
        }
        (
            &spec.depends_on,
            &spec.slices,
            &spec.tools,
            &spec.route,
            spec.attempts,
            spec.task_kind,
            &spec.check,
        )
            .hash(&mut h);
    }
    for c in &record.open_contracts {
        (&c.contract, &c.instruction, &c.acceptance, &c.depends_on).hash(&mut h);
        for w in &c.writes {
            w.path.hash(&mut h);
        }
    }
    (&record.verified_contracts, &record.split_contracts).hash(&mut h);
    for c in &record.coordination.closed {
        (&c.contract, &c.instruction, &c.acceptance, &c.writes).hash(&mut h);
    }
    for change in &record.changes {
        (change.number, change.received_ms).hash(&mut h);
        if let Some(ops) = &change.applied {
            (
                &ops.added,
                &ops.amended,
                &ops.superseded,
                &ops.removed,
                &ops.followups,
            )
                .hash(&mut h);
        }
    }
    for (id, c) in &record.verified_commits {
        (id, &c.sha, c.verified_ms).hash(&mut h);
    }
    h.finish()
}

/// One item's content hash: its node, its dependencies and its facts. Grouped
/// by item first, so the cost grows with the plan, not with its square.
fn item_hashes(mirror: &PlanMirror) -> BTreeMap<String, u64> {
    let mut deps: HashMap<&str, Vec<&str>> = HashMap::new();
    for (from, to) in &mirror.deps {
        deps.entry(from).or_default().push(to);
    }
    let mut facts: HashMap<&str, Vec<&MirrorFact>> = HashMap::new();
    for f in &mirror.facts {
        facts.entry(&f.subject).or_default().push(f);
    }
    mirror
        .nodes
        .iter()
        .map(|n| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (&n.title, &n.body, &n.parent_id).hash(&mut h);
            deps.get(n.id.as_str()).hash(&mut h);
            for f in facts.get(n.id.as_str()).into_iter().flatten() {
                (&f.predicate, &f.object, f.valid_at).hash(&mut h);
            }
            (n.id.clone(), h.finish())
        })
        .collect()
}

/// What a process last wrote of each run's mirror: item id to content hash. An
/// item whose hash is the same is not written again.
#[derive(Debug, Default)]
pub struct MirrorCache {
    written: HashMap<String, BTreeMap<String, u64>>,
    /// The fingerprint of the record at the last write of each run.
    fingerprints: HashMap<String, u64>,
    /// Mirrors built and diffed through this cache.
    built: std::sync::atomic::AtomicUsize,
}

impl MirrorCache {
    /// The part of `mirror` not already written as it is, and every item's hash
    /// to remember once that part is committed.
    pub fn diff(&self, mirror: &PlanMirror) -> (PlanMirror, BTreeMap<String, u64>) {
        self.built
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let hashes = item_hashes(mirror);
        let known = self.written.get(&mirror.plan_id);
        let changed = |id: &str| known.and_then(|k| k.get(id)) != hashes.get(id);
        let nodes: Vec<WorkNode> = mirror
            .nodes
            .iter()
            .filter(|n| changed(&n.id))
            .cloned()
            .collect();
        let stale: BTreeSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        let part = PlanMirror {
            plan_id: mirror.plan_id.clone(),
            deps: mirror
                .deps
                .iter()
                .filter(|(from, _)| stale.contains(from.as_str()))
                .cloned()
                .collect(),
            facts: mirror
                .facts
                .iter()
                .filter(|f| stale.contains(f.subject.as_str()))
                .cloned()
                .collect(),
            nodes,
            recorded_ms: mirror.recorded_ms,
            // Only part of the plan: it cannot say what the graph holds beyond it.
            full: false,
        };
        (part, hashes)
    }

    /// Remember a committed write.
    pub fn remember(&mut self, plan_id: &str, hashes: BTreeMap<String, u64>) {
        self.written.insert(plan_id.to_owned(), hashes);
    }

    /// Forget a run: the next write is the whole mirror.
    pub fn forget(&mut self, plan_id: &str) {
        self.written.remove(plan_id);
        self.fingerprints.remove(plan_id);
    }

    pub fn builds(&self) -> usize {
        self.built.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the record's fingerprint is the one of the last committed write.
    pub fn fingerprint_is(&self, plan_id: &str, fingerprint: u64) -> bool {
        self.fingerprints.get(plan_id) == Some(&fingerprint)
    }

    pub fn remember_fingerprint(&mut self, plan_id: &str, fingerprint: u64) {
        self.fingerprints.insert(plan_id.to_owned(), fingerprint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::{ClosedContract, Closure};
    use crate::delta::{AppliedOps, ChangeSource, RecordedChange};
    use crate::plan::ContractSpec;
    use crate::run_state::VerifiedCommit;
    use kgraf_core::RunId;

    fn spec(id: &str, deps: &[&str]) -> ContractSpec {
        ContractSpec {
            id: id.into(),
            instruction: format!("do {id}\nmore"),
            acceptance: "it works".into(),
            writes: Vec::new(),
            depends_on: deps.iter().map(|d| (*d).to_owned()).collect(),
            slices: Vec::new(),
            tools: Vec::new(),
            route: None,
            attempts: 1,
            task_kind: None,
            check: None,
        }
    }

    fn record() -> RunStateRecord {
        let mut r = RunStateRecord::new(RunId::new("r1"), "build it\nfast", 1_000);
        for (id, deps) in [("a", &[][..]), ("b", &["a"][..]), ("c", &["b"][..])] {
            r.plan_specs.insert(id.into(), spec(id, deps));
        }
        r.plan_root = Some(RunId::new("r1"));
        r
    }

    #[test]
    fn the_mirror_is_one_item_per_contract_with_the_latest_spec_and_its_dependencies() {
        let r = record();
        let m = mirror(&r);
        assert_eq!(m.plan_id, "run:r1");
        let ids: Vec<&str> = m.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(
            ids,
            ["run:r1", "run:r1/item/a", "run:r1/item/b", "run:r1/item/c"]
        );
        assert_eq!(m.nodes[0].kind, WorkKind::Plan);
        assert_eq!(m.nodes[0].title, "build it");
        assert_eq!(m.nodes[2].title, "do b");
        assert!(m.nodes[2].body.contains("\"acceptance\":\"it works\""));
        assert_eq!(
            m.deps,
            [
                ("run:r1/item/b".to_owned(), "run:r1/item/a".to_owned()),
                ("run:r1/item/c".to_owned(), "run:r1/item/b".to_owned())
            ]
        );
        assert!(m.facts.is_empty());
        // A pure function: the same record, the same mirror.
        assert_eq!(mirror(&r), m);
        // It sets neither plan_root nor source_plan, and does not read a status.
        assert_eq!(r.source_plan, None);
    }

    #[test]
    fn a_revision_keeps_the_item_and_a_removal_is_a_fact() {
        let mut r = record();
        let mut c1 = RecordedChange::received(1, ChangeSource::Change, "x", None, 2_000, 0);
        c1.applied = Some(AppliedOps {
            amended: vec!["b".into()],
            ..AppliedOps::default()
        });
        let mut c2 = RecordedChange::received(2, ChangeSource::Change, "y", None, 3_000, 0);
        c2.applied = Some(AppliedOps {
            removed: vec!["c".into()],
            superseded: vec![("a".into(), "d2.a".into())],
            ..AppliedOps::default()
        });
        r.changes = vec![c1, c2];
        r.plan_specs.insert("d2.a".into(), spec("d2.a", &[]));
        r.coordination.closed.push(ClosedContract {
            contract: "c".into(),
            closure: Closure::Removed,
            instruction: "do c".into(),
            acceptance: "it works".into(),
            workers: Vec::new(),
            worker_routes: Vec::new(),
            rationale: None,
            writes: Vec::new(),
        });
        r.verified_commits.insert(
            "b".into(),
            VerifiedCommit {
                sha: "a".repeat(40),
                verified_by: "t".into(),
                verified_ms: 2_500,
            },
        );
        let m = mirror(&r);
        // The removed contract keeps its item, with its whole spec as the body.
        assert!(m.nodes.iter().any(|n| n.id == "run:r1/item/c"));
        let facts: Vec<(&str, &str, &str)> = m
            .facts
            .iter()
            .map(|f| {
                (
                    f.subject.rsplit('/').next().unwrap_or(""),
                    f.predicate.as_str(),
                    f.object.as_str(),
                )
            })
            .collect();
        assert!(
            facts.contains(&("b", "work:revised_by", "change:1")),
            "{facts:?}"
        );
        assert!(
            facts.contains(&("c", "work:removed_by", "change:2")),
            "{facts:?}"
        );
        assert!(
            facts.contains(&("a", "work:removed_by", "change:2")),
            "{facts:?}"
        );
        assert!(
            facts.contains(&("d2.a", "work:revised_by", "change:2")),
            "{facts:?}"
        );
        assert!(
            m.facts
                .iter()
                .any(|f| f.predicate == "work:verified_commit" && f.valid_at == 2_500)
        );
        assert!(m.facts.iter().all(|f| f.predicate.starts_with("work:")));
    }

    /// The fingerprint moves with every field `mirror` reads, and stays when only
    /// time moves: a record with the same fingerprint gives the same rows.
    #[test]
    fn the_fingerprint_moves_with_the_rows() {
        use crate::run_state::VerifiedCommit;
        let base = record();
        let rows = |r: &RunStateRecord| {
            let m = mirror(r);
            (
                m.nodes
                    .iter()
                    .map(|n| {
                        (
                            n.id.clone(),
                            n.title.clone(),
                            n.body.clone(),
                            n.parent_id.clone(),
                        )
                    })
                    .collect::<Vec<_>>(),
                m.deps,
                m.facts,
            )
        };
        let mut time_only = base.clone();
        time_only.updated_ms += 99;
        assert_eq!(fingerprint(&time_only), fingerprint(&base));
        assert_eq!(rows(&time_only), rows(&base));
        let mut a_change = RecordedChange::received(1, ChangeSource::Change, "x", None, 2_000, 0);
        a_change.applied = Some(AppliedOps {
            amended: vec!["b".into()],
            ..AppliedOps::default()
        });
        type Mutation = Box<dyn Fn(&mut RunStateRecord)>;
        let mutations: Vec<(&str, Mutation)> = vec![
            (
                "instruction",
                Box::new(|r| r.original_instruction.push('x')),
            ),
            ("plan_root", Box::new(|r| r.plan_root = None)),
            (
                "source_plan",
                Box::new(|r| r.source_plan = Some("p".into())),
            ),
            (
                "spec instruction",
                Box::new(|r| r.plan_specs.get_mut("a").expect("a").instruction.push('x')),
            ),
            (
                "spec acceptance",
                Box::new(|r| r.plan_specs.get_mut("a").expect("a").acceptance.push('x')),
            ),
            (
                "spec deps",
                Box::new(|r| {
                    r.plan_specs
                        .get_mut("c")
                        .expect("c")
                        .depends_on
                        .push("a".into())
                }),
            ),
            (
                "spec route",
                Box::new(|r| r.plan_specs.get_mut("a").expect("a").route = Some("m".into())),
            ),
            (
                "spec tools",
                Box::new(|r| r.plan_specs.get_mut("a").expect("a").tools.push("t".into())),
            ),
            (
                "spec attempts",
                Box::new(|r| r.plan_specs.get_mut("a").expect("a").attempts = 3),
            ),
            (
                "spec check",
                Box::new(|r| r.plan_specs.get_mut("a").expect("a").check = Some("c".into())),
            ),
            (
                "a new spec",
                Box::new(|r| {
                    r.plan_specs.insert("z".into(), spec("z", &[]));
                }),
            ),
            (
                "verified",
                Box::new(|r| r.verified_contracts.push("v".into())),
            ),
            ("split", Box::new(|r| r.split_contracts.push("s".into()))),
            (
                "a change",
                Box::new(move |r| r.changes.push(a_change.clone())),
            ),
            (
                "a commit",
                Box::new(|r| {
                    r.verified_commits.insert(
                        "a".into(),
                        VerifiedCommit {
                            sha: "a".repeat(40),
                            verified_by: "t".into(),
                            verified_ms: 1,
                        },
                    );
                }),
            ),
        ];
        for (what, mutate) in &mutations {
            let mut r = base.clone();
            mutate(&mut r);
            assert_ne!(
                fingerprint(&r),
                fingerprint(&base),
                "{what} must move the fingerprint"
            );
        }
        // Whatever moves the rows moves the fingerprint (the other way round is a
        // harmless rebuild).
        for (what, mutate) in &mutations {
            let mut r = base.clone();
            mutate(&mut r);
            if rows(&r) != rows(&base) {
                assert_ne!(fingerprint(&r), fingerprint(&base), "{what}");
            }
        }
    }

    #[test]
    fn the_cache_skips_an_unchanged_item_and_writes_a_changed_one() {
        let r = record();
        let m = mirror(&r);
        let mut cache = MirrorCache::default();
        let (all, hashes) = cache.diff(&m);
        assert_eq!(all.nodes.len(), 4, "a cold cache writes everything");
        cache.remember(&m.plan_id, hashes);
        let (none, _) = cache.diff(&m);
        assert!(none.nodes.is_empty() && none.deps.is_empty() && none.facts.is_empty());
        let mut r2 = r.clone();
        r2.plan_specs
            .get_mut("b")
            .expect("spec")
            .acceptance
            .push_str(" twice");
        let (part, _) = cache.diff(&mirror(&r2));
        let ids: Vec<&str> = part.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["run:r1/item/b"]);
        assert_eq!(
            part.deps.len(),
            1,
            "the changed item's own dependencies go with it"
        );
        // A time-only change of the record rewrites nothing.
        let mut r3 = r.clone();
        r3.updated_ms += 5;
        assert!(cache.diff(&mirror(&r3)).0.nodes.is_empty());
    }
}
