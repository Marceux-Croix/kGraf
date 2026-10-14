//! Straight chains of a plan, collapsed into one contract (decision 0014,
//! amendment of 2026-10-14).
//!
//! A chain gains no parallelism: each step waits for the one before. Splitting
//! it still costs an interface to negotiate, a handshake, a merge, and a fresh
//! worker that loses the previous step's working state. So a straight chain is
//! one contract, unless one worker cannot carry it or its steps would run on
//! different routes.
//!
//! The rule, over the dependency graph of one plan:
//!
//! - link `A -> B`: `B` depends on `A` alone, and `A` has `B` as its only
//!   dependent;
//! - a chain is a maximal run of links `A1 -> A2 -> ... -> An`, `n >= 2`. Its
//!   interior contracts have one dependency and one dependent. The head
//!   `A1` may depend on anything outside the chain; the tail `An` may have any
//!   dependents.
//!
//! The merged contract keeps the head's id and depends on the head's
//! dependencies; whatever depended on the tail depends on it. No cycle can be
//! made: a path through the merged contract enters through the head and leaves
//! through the tail, as it did before.
//!
//! A link may be kept split (amendment note of 2026-10-14 to 0014): when its
//! producer writes code an interface can check and both steps are large, the
//! interface stub lets both start at once (decision 0081), which a merge would
//! give up. The chain is then cut at that link, and each run of two or more
//! links on either side is a chain of its own, judged as any other.
//!
//! This module is the shape of the rule. Whether a link is kept split, and
//! whether a chain may merge (one route, one worker's size), are the caller's
//! rulings ([`collapse`]'s `keep_link` and [`Verdict`]).

use crate::plan::ContractSpec;
use kgraf_core::model_profile::TaskKind;
use std::collections::{BTreeMap, BTreeSet};

/// The caller's ruling on one chain, given the links and the merged contract.
pub enum Verdict {
    /// Merge. The route the merged contract runs on and its projected size,
    /// for the record.
    Collapse {
        route: String,
        projected_tokens: u64,
        target_tokens: u64,
    },
    /// Leave the chain split, for this reason.
    Keep(String),
}

/// What happened to one chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Collapsed {
        /// The links' ids in order; the first is the merged contract's id.
        ids: Vec<String>,
        route: String,
        projected_tokens: u64,
        target_tokens: u64,
    },
    Kept {
        ids: Vec<String>,
        reason: String,
    },
    /// The link `from -> to` of the straight chain `chain` was kept split
    /// (`keep_link`'s reason), and the chain cut there.
    LinkKept {
        chain: Vec<String>,
        from: String,
        to: String,
        reason: String,
    },
}

fn deps_of(spec: &ContractSpec) -> BTreeSet<&str> {
    spec.depends_on
        .iter()
        .map(String::as_str)
        .filter(|d| *d != spec.id)
        .collect()
}

/// The maximal straight chains of `specs`, each as indices in order.
pub fn straight_chains(specs: &[ContractSpec]) -> Vec<Vec<usize>> {
    let index: BTreeMap<&str, usize> = specs
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect();
    let deps: Vec<BTreeSet<&str>> = specs.iter().map(deps_of).collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); specs.len()];
    for (j, d) in deps.iter().enumerate() {
        for id in d {
            if let Some(&i) = index.get(id) {
                dependents[i].push(j);
            }
        }
    }
    // next[i] = j when i -> j is a link.
    let mut next: Vec<Option<usize>> = vec![None; specs.len()];
    let mut has_prev = vec![false; specs.len()];
    for (i, out) in dependents.iter().enumerate() {
        if let [j] = out[..]
            && i != j
            && deps[j].len() == 1
            && deps[j].contains(specs[i].id.as_str())
        {
            next[i] = Some(j);
            has_prev[j] = true;
        }
    }
    let mut chains = Vec::new();
    for head in 0..specs.len() {
        if has_prev[head] || next[head].is_none() {
            continue;
        }
        let mut chain = vec![head];
        let mut at = head;
        while let Some(n) = next[at] {
            chain.push(n);
            at = n;
        }
        chains.push(chain);
    }
    chains
}

fn push_unique<T: PartialEq + Clone>(into: &mut Vec<T>, from: &[T]) {
    for x in from {
        if !into.contains(x) {
            into.push(x.clone());
        }
    }
}

/// One contract for the chain `links` (in order). Id: the head's, so what
/// already names the head (a trace reader, a resumed record, a dependent
/// plan's notes) keeps working; the other ids are in the `chain_collapsed`
/// record and the instruction's step headings.
///
/// - instruction: a line saying it is one contract of steps, then each link's
///   instruction under `Step k of n (`id`):`, in order;
/// - acceptance: each link's acceptance under the same heading, in order;
/// - writes, slices and tools: the union, first occurrence first;
/// - `depends_on`: the head's;
/// - attempts: the most any link asked for (as `merge_groups` does);
/// - route: the first pin among the links (the caller refuses a chain whose
///   pins differ);
/// - task kind: the links' shared kind, `Implementation` when they differ (the
///   merged contract mostly implements; the route is fixed, so no routing
///   depends on it).
pub fn merge_chain(links: &[&ContractSpec]) -> ContractSpec {
    let n = links.len();
    let head = links[0];
    let mut merged = head.clone();
    let steps = |pick: &dyn Fn(&ContractSpec) -> &str| {
        links
            .iter()
            .enumerate()
            .map(|(k, l)| format!("Step {} of {n} (`{}`):\n{}", k + 1, l.id, pick(l)))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    merged.instruction = format!(
        "This is one contract of {n} steps. Do them in order, in this working tree: each step \
         builds on the one before.\n\n{}",
        steps(&|l| l.instruction.as_str())
    );
    merged.acceptance = steps(&|l| l.acceptance.as_str());
    merged.writes = Vec::new();
    merged.slices = Vec::new();
    merged.tools = Vec::new();
    for l in links {
        push_unique(&mut merged.writes, &l.writes);
        push_unique(&mut merged.slices, &l.slices);
        push_unique(&mut merged.tools, &l.tools);
    }
    merged.depends_on = head.depends_on.clone();
    merged.attempts = links.iter().map(|l| l.attempts).max().unwrap_or(1);
    merged.route = links.iter().find_map(|l| l.route.clone());
    let kinds: BTreeSet<Option<TaskKind>> = links.iter().map(|l| l.task_kind).collect();
    merged.task_kind = match kinds.len() {
        1 => head.task_kind,
        _ => Some(TaskKind::Implementation),
    };
    merged
}

/// `chain` cut after every position `k` where `keep(k)` holds (the link from
/// `chain[k]` to `chain[k + 1]` is kept split): the runs between the cuts, in
/// order, a run of one included.
pub fn cut_chain(chain: &[usize], mut keep: impl FnMut(usize) -> bool) -> Vec<Vec<usize>> {
    let mut runs = Vec::new();
    let mut run = Vec::new();
    for (k, &i) in chain.iter().enumerate() {
        run.push(i);
        if k + 1 < chain.len() && keep(k) {
            runs.push(std::mem::take(&mut run));
        }
    }
    runs.push(run);
    runs
}

/// `specs` with every chain the verdict accepts replaced by its merged
/// contract, at the head's place, and with every dependency on a merged link
/// naming the head.
///
/// `keep_link(a, b)` rules on each link `a -> b` of a chain first: a reason
/// keeps that link split ([`Outcome::LinkKept`]), and the chain
/// is cut there ([`cut_chain`]). Each run of two or more contracts left is
/// then given to `decide`. One outcome per kept link, then one per run, each
/// in plan order.
pub fn collapse(
    specs: Vec<ContractSpec>,
    mut keep_link: impl FnMut(&ContractSpec, &ContractSpec) -> Option<String>,
    mut decide: impl FnMut(&[&ContractSpec], &ContractSpec) -> Verdict,
) -> (Vec<ContractSpec>, Vec<Outcome>) {
    let mut outcomes = Vec::new();
    let mut replace: BTreeMap<usize, ContractSpec> = BTreeMap::new();
    let mut dropped: BTreeSet<usize> = BTreeSet::new();
    let mut rename: BTreeMap<String, String> = BTreeMap::new();
    let mut runs: Vec<Vec<usize>> = Vec::new();
    for whole in straight_chains(&specs) {
        let runs_of = cut_chain(&whole, |k| {
            let (a, b) = (&specs[whole[k]], &specs[whole[k + 1]]);
            match keep_link(a, b) {
                Some(reason) => {
                    outcomes.push(Outcome::LinkKept {
                        chain: whole.iter().map(|&i| specs[i].id.clone()).collect(),
                        from: a.id.clone(),
                        to: b.id.clone(),
                        reason,
                    });
                    true
                }
                None => false,
            }
        });
        runs.extend(runs_of.into_iter().filter(|r| r.len() >= 2));
    }
    for chain in runs {
        let links: Vec<&ContractSpec> = chain.iter().map(|&i| &specs[i]).collect();
        let ids: Vec<String> = links.iter().map(|l| l.id.clone()).collect();
        let merged = merge_chain(&links);
        match decide(&links, &merged) {
            Verdict::Collapse {
                route,
                projected_tokens,
                target_tokens,
            } => {
                for id in &ids[1..] {
                    rename.insert(id.clone(), ids[0].clone());
                }
                dropped.extend(chain[1..].iter().copied());
                replace.insert(chain[0], merged);
                outcomes.push(Outcome::Collapsed {
                    ids,
                    route,
                    projected_tokens,
                    target_tokens,
                });
            }
            Verdict::Keep(reason) => outcomes.push(Outcome::Kept { ids, reason }),
        }
    }
    let mut out = Vec::with_capacity(specs.len());
    for (i, spec) in specs.into_iter().enumerate() {
        if dropped.contains(&i) {
            continue;
        }
        let mut spec = replace.remove(&i).unwrap_or(spec);
        let own = spec.id.clone();
        let mut deps: Vec<String> = Vec::new();
        for d in &spec.depends_on {
            let d = rename.get(d).cloned().unwrap_or_else(|| d.clone());
            if d != own && !deps.contains(&d) {
                deps.push(d);
            }
        }
        spec.depends_on = deps;
        out.push(spec);
    }
    (out, outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_state::WriteTarget;

    fn spec(id: &str, deps: &[&str]) -> ContractSpec {
        ContractSpec {
            id: id.into(),
            instruction: format!("do {id}"),
            acceptance: format!("{id} holds"),
            writes: Vec::new(),
            depends_on: deps.iter().map(|d| (*d).to_owned()).collect(),
            slices: Vec::new(),
            tools: Vec::new(),
            route: None,
            attempts: 1,
            task_kind: None,
        }
    }

    fn ids(chains: &[Vec<usize>], specs: &[ContractSpec]) -> Vec<Vec<String>> {
        chains
            .iter()
            .map(|c| c.iter().map(|&i| specs[i].id.clone()).collect())
            .collect()
    }

    fn always(links: &[&ContractSpec], _: &ContractSpec) -> Verdict {
        Verdict::Collapse {
            route: "r".into(),
            projected_tokens: links.len() as u64,
            target_tokens: 100,
        }
    }

    #[test]
    fn a_straight_chain_is_one_chain() {
        let s = vec![spec("a", &[]), spec("b", &["a"]), spec("c", &["b"])];
        assert_eq!(ids(&straight_chains(&s), &s), vec![vec!["a", "b", "c"]]);
    }

    #[test]
    fn chains_are_found_whatever_the_order_of_the_list() {
        let s = vec![spec("c", &["b"]), spec("a", &[]), spec("b", &["a"])];
        assert_eq!(ids(&straight_chains(&s), &s), vec![vec!["a", "b", "c"]]);
    }

    #[test]
    fn a_diamond_has_no_chain() {
        let s = vec![
            spec("a", &[]),
            spec("b", &["a"]),
            spec("c", &["a"]),
            spec("d", &["b", "c"]),
        ];
        assert!(straight_chains(&s).is_empty());
    }

    #[test]
    fn a_fan_out_breaks_the_chain_at_the_fork() {
        // a -> b, then b feeds c and d: only a -> b is a chain.
        let s = vec![
            spec("a", &[]),
            spec("b", &["a"]),
            spec("c", &["b"]),
            spec("d", &["b"]),
        ];
        assert_eq!(ids(&straight_chains(&s), &s), vec![vec!["a", "b"]]);
    }

    #[test]
    fn a_second_dependency_keeps_a_link_out() {
        // c depends on b and on x: b -> c is not a link.
        let s = vec![
            spec("a", &[]),
            spec("x", &[]),
            spec("b", &["a"]),
            spec("c", &["b", "x"]),
        ];
        assert_eq!(ids(&straight_chains(&s), &s), vec![vec!["a", "b"]]);
    }

    #[test]
    fn independent_contracts_are_not_a_chain() {
        let s = vec![spec("a", &[]), spec("b", &[])];
        assert!(straight_chains(&s).is_empty());
    }

    #[test]
    fn the_head_may_depend_outside_and_the_tail_may_have_dependents() {
        // root feeds side and a, so root -> a is not a link; a -> b is, and
        // two contracts wait on b.
        let s = vec![
            spec("root", &[]),
            spec("side", &["root"]),
            spec("a", &["root"]),
            spec("b", &["a"]),
            spec("after", &["b"]),
            spec("also", &["b"]),
        ];
        let (out, outcomes) = collapse(s, |_, _| None, always);
        let ids: Vec<&str> = out.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["root", "side", "a", "after", "also"]);
        assert_eq!(out[2].depends_on, ["root"]);
        // What depended on the tail depends on the merged contract.
        assert_eq!(out[3].depends_on, ["a"]);
        assert_eq!(out[4].depends_on, ["a"]);
        assert_eq!(outcomes.len(), 1);
    }

    #[test]
    fn the_merge_carries_every_step_in_order() {
        let mut a = spec("c1", &["root"]);
        a.writes = vec![WriteTarget {
            path: "x.rs".into(),
            node: None,
        }];
        a.tools = vec!["write_file".into()];
        a.task_kind = Some(TaskKind::Interface);
        let mut b = spec("c2", &["c1"]);
        b.writes = vec![
            WriteTarget {
                path: "x.rs".into(),
                node: None,
            },
            WriteTarget {
                path: "y.rs".into(),
                node: None,
            },
        ];
        b.attempts = 3;
        b.route = Some("r".into());
        b.task_kind = Some(TaskKind::Tests);
        let m = merge_chain(&[&a, &b]);
        assert_eq!(m.id, "c1");
        assert_eq!(m.depends_on, ["root"]);
        assert_eq!(m.writes.len(), 2);
        assert_eq!(m.tools, ["write_file"]);
        assert_eq!(m.attempts, 3);
        assert_eq!(m.route.as_deref(), Some("r"));
        assert_eq!(m.task_kind, Some(TaskKind::Implementation));
        let (s1, s2) = (
            m.instruction
                .find("Step 1 of 2 (`c1`):\ndo c1")
                .expect("step 1"),
            m.instruction
                .find("Step 2 of 2 (`c2`):\ndo c2")
                .expect("step 2"),
        );
        assert!(s1 < s2);
        assert_eq!(
            m.acceptance,
            "Step 1 of 2 (`c1`):\nc1 holds\n\nStep 2 of 2 (`c2`):\nc2 holds"
        );
    }

    #[test]
    fn a_shared_task_kind_is_kept() {
        let mut a = spec("a", &[]);
        let mut b = spec("b", &["a"]);
        a.task_kind = Some(TaskKind::Docs);
        b.task_kind = Some(TaskKind::Docs);
        assert_eq!(merge_chain(&[&a, &b]).task_kind, Some(TaskKind::Docs));
    }

    #[test]
    fn a_chain_is_cut_after_each_kept_link() {
        let chain = [10, 11, 12, 13];
        assert_eq!(cut_chain(&chain, |_| false), vec![vec![10, 11, 12, 13]]);
        assert_eq!(
            cut_chain(&chain, |k| k == 0),
            vec![vec![10], vec![11, 12, 13]]
        );
        assert_eq!(
            cut_chain(&chain, |k| k == 1),
            vec![vec![10, 11], vec![12, 13]]
        );
        assert_eq!(
            cut_chain(&chain, |_| true),
            vec![vec![10], vec![11], vec![12], vec![13]]
        );
    }

    #[test]
    fn a_kept_link_splits_the_chain_and_the_rest_merges_on_each_side() {
        // a -> b -> c -> d -> e with b -> c kept: a+b merge, c+d+e merge, and
        // the merged c waits on the merged a.
        let s = vec![
            spec("a", &[]),
            spec("b", &["a"]),
            spec("c", &["b"]),
            spec("d", &["c"]),
            spec("e", &["d"]),
        ];
        let (out, outcomes) = collapse(
            s,
            |a, b| (a.id == "b" && b.id == "c").then(|| "big".to_owned()),
            always,
        );
        let ids: Vec<&str> = out.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a", "c"]);
        assert!(out[0].depends_on.is_empty());
        assert_eq!(out[1].depends_on, ["a"]);
        assert_eq!(
            outcomes[0],
            Outcome::LinkKept {
                chain: ["a", "b", "c", "d", "e"].map(String::from).to_vec(),
                from: "b".into(),
                to: "c".into(),
                reason: "big".into()
            }
        );
        let merged: Vec<Vec<String>> = outcomes[1..]
            .iter()
            .map(|o| match o {
                Outcome::Collapsed { ids, .. } => ids.clone(),
                _ => panic!("{o:?}"),
            })
            .collect();
        assert_eq!(merged, [vec!["a", "b"], vec!["c", "d", "e"]]);
    }

    #[test]
    fn a_kept_link_at_each_end_leaves_one_contract_on_that_side() {
        // a -> b -> c with a -> b kept: a stays, b+c merge.
        let s = vec![spec("a", &[]), spec("b", &["a"]), spec("c", &["b"])];
        let (out, _) = collapse(s, |a, _| (a.id == "a").then(String::new), always);
        let ids: Vec<&str> = out.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(out[1].depends_on, ["a"]);
    }

    #[test]
    fn a_kept_chain_leaves_the_plan_as_it_was() {
        let s = vec![spec("a", &[]), spec("b", &["a"])];
        let (out, outcomes) = collapse(s.clone(), |_, _| None, |_, _| Verdict::Keep("why".into()));
        assert_eq!(out, s);
        assert_eq!(
            outcomes,
            vec![Outcome::Kept {
                ids: vec!["a".into(), "b".into()],
                reason: "why".into()
            }]
        );
    }
}
