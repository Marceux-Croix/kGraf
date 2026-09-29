//! Speculative merge, build and test across in-flight worker branches
//! (decision 0022, mechanism 3; decision 0074).
//!
//! **What it does.** Each in-flight contract's worktree is snapshotted as it
//! stands ([`Workspaces::snapshot`], a commit built from a private index, so the
//! worker's tree, index and HEAD are untouched). Every snapshot is trial-merged
//! onto the integration tip alone, then every pair together
//! ([`Workspaces::trial_merge`], computed in the object database). A pair whose
//! members each merge alone but not together is a **textual** conflict. A pair
//! that merges cleanly is validated with the project's discovered validation
//! commands (the gate runner, 0018/0019); when the pair fails while each member
//! alone passes, it is a **semantic** conflict: one textual merge would not
//! have caught.
//!
//! This is Crystal's pairwise relationship classification (Brun, Holmes,
//! Ernst, Notkin, ESEC/FSE 2011: "textual conflict", "build conflict", "test
//! conflict" from actually merging and running). Only
//! the method is taken; no code (Crystal targets Mercurial clones on a
//! developer desktop). The merge itself is `kgraf-sandbox`'s own merge-back
//! computation, so a trial reports exactly what a real merge-back would.
//!
//! **What it never does** (0074): write a ref (the integration branch
//! included), touch a worker's worktree, or delay a merge-back. The result is
//! reported (trace, run record) and used by the scheduler's pairing preference
//! (`pairing.rs`); gates stay the checkpoint where it becomes blocking (0022,
//! mechanism 4). A snapshot, merge or validation that cannot run is an error,
//! never a clean result.

use crate::gates::{GateRequest, GateRunner};
use crate::orchestrator::Workspaces;
use crate::run_state::VerifiedCommit;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The gate id a speculative validation run carries, so a runner or a trace
/// can tell it from a quality gate.
pub const SPECULATION_GATE: &str = "speculation";

/// Detail kept from a validation's output: the end, where failures are.
const DETAIL_CHARS: usize = 2_000;

/// A trial merge's result, as a workspace provider reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trial {
    /// Every input merged; `commit` is the unreferenced merge onto the tip and
    /// `tree` its tree (the key a repeated validation is skipped on).
    Clean { commit: String, tree: String },
    /// An input did not merge onto the tip plus the inputs before it.
    Conflict { paths: Vec<String>, detail: String },
}

/// What a speculative conflict is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Each merges alone; together git cannot merge them.
    Textual,
    /// Together they merge cleanly and fail validation; each alone passes.
    Semantic,
}

impl ConflictKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Textual => "textual",
            Self::Semantic => "semantic",
        }
    }
}

/// One conflict between two in-flight contracts, found before either landed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeculativeConflict {
    /// The two contracts, in sorted order.
    pub contracts: [String; 2],
    pub kind: ConflictKind,
    /// The integration tip the trial was computed on.
    pub tip: String,
    /// Conflicted paths (textual), or the validation output's tail
    /// (semantic): written by workers and the project's tools, so data.
    pub detail: String,
    pub at_ms: i64,
}

impl SpeculativeConflict {
    pub fn involves(&self, a: &str, b: &str) -> bool {
        (self.contracts[0] == a && self.contracts[1] == b)
            || (self.contracts[0] == b && self.contracts[1] == a)
    }
}

/// Cumulative cost of speculative merge in one run, kept in the run
/// record and traced after every round so the cadence can be tuned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpeculationCost {
    /// Rounds that ran to a result.
    pub rounds: u64,
    /// Validation runs (build and test) those rounds started.
    pub validations: u64,
    /// Their wall-clock time.
    pub validation_ms: u64,
    /// Rounds that could not run (traced as `refused`).
    pub refused: u64,
    /// Rounds skipped because no admission slot was free.
    pub skipped: u64,
}

/// Validation results by merged tree: a pair whose trees did not change since
/// the last round is not validated again. Owned by the loop, lent to a job.
pub type ValidationCache = BTreeMap<String, (bool, String)>;

/// One round's findings.
#[derive(Debug, Default)]
pub struct Round {
    pub tip: String,
    pub conflicts: Vec<(String, String, ConflictKind, String)>,
    /// Whether clean pairs were validated (false: no validation commands, so
    /// the semantic class was not checked, and the caller says so).
    pub validated: bool,
    /// Validation runs this round started (cache hits not counted), and
    /// their wall-clock time (the cost is recorded).
    pub validations: u32,
    pub validation_ms: u64,
}

/// Run one speculative round over `in_flight` (contract, worktree), at least
/// two. `Ok(None)`: the provider keeps no versioned worktrees, so there is
/// nothing to merge. Every failure to snapshot, merge or validate is `Err`.
pub async fn speculate(
    workspaces: &dyn Workspaces,
    runner: Option<&dyn GateRunner>,
    in_flight: &[(String, PathBuf)],
    cache: &mut ValidationCache,
) -> Result<Option<Round>, String> {
    let Some(tip) = workspaces.integration_tip().await? else {
        return Ok(None);
    };
    // (contract, snapshot, the snapshot merged onto the tip alone, its tree)
    let mut branches: Vec<(String, String, String, String)> = Vec::new();
    for (contract, workspace) in in_flight {
        let Some(snapshot) = workspaces.snapshot(workspace).await? else {
            return Ok(None);
        };
        // A branch that does not merge onto the tip alone is not a pairwise
        // finding: its own merge-back will conflict and 0073's resolver takes
        // it. It is left out of the pairs rather than blamed on a partner.
        let Some(trial) = workspaces
            .trial_merge(std::slice::from_ref(&snapshot))
            .await?
        else {
            return Ok(None);
        };
        if let Trial::Clean { commit, tree } = trial {
            branches.push((contract.clone(), snapshot, commit, tree));
        }
    }
    let commands = runner.map(|r| r.commands()).unwrap_or_default();
    let validate = runner.filter(|_| !commands.is_empty());
    let mut round = Round {
        tip,
        conflicts: Vec::new(),
        validated: validate.is_some(),
        validations: 0,
        validation_ms: 0,
    };
    for i in 0..branches.len() {
        for j in (i + 1)..branches.len() {
            let (a, a_commit, _, _) = &branches[i];
            let (b, b_commit, _, _) = &branches[j];
            let pair = [a_commit.clone(), b_commit.clone()];
            let Some(trial) = workspaces.trial_merge(&pair).await? else {
                return Ok(None);
            };
            match trial {
                Trial::Conflict { paths, detail } => {
                    let text = format!("{detail}: {}", paths.join(", "));
                    round
                        .conflicts
                        .push((a.clone(), b.clone(), ConflictKind::Textual, text));
                }
                Trial::Clean { commit, tree } => {
                    let Some(runner) = validate else { continue };
                    let covers = vec![a.clone(), b.clone()];
                    let (passed, output) = validated(
                        runner, &commands, &covers, &commit, &tree, cache, &mut round,
                    )
                    .await?;
                    if passed {
                        continue;
                    }
                    // Attribute before blaming the pair: a member that fails
                    // alone is its own failure, which its verification or the
                    // gate reports, not a conflict between the two.
                    let mut alone_pass = true;
                    for k in [i, j] {
                        let (c, _, single, single_tree) = &branches[k];
                        let (ok, _) = validated(
                            runner,
                            &commands,
                            std::slice::from_ref(c),
                            single,
                            single_tree,
                            cache,
                            &mut round,
                        )
                        .await?;
                        alone_pass &= ok;
                    }
                    if alone_pass {
                        round.conflicts.push((
                            a.clone(),
                            b.clone(),
                            ConflictKind::Semantic,
                            output,
                        ));
                    }
                }
            }
        }
    }
    // The round wrote loose objects (snapshots, merges); let git decide
    // whether they need packing, as it does after its own commands.
    workspaces.collect_garbage().await?;
    Ok(Some(round))
}

/// Validate `commit` (tree `tree`) once per tree, counting each run.
async fn validated(
    runner: &dyn GateRunner,
    commands: &[String],
    covers: &[String],
    commit: &str,
    tree: &str,
    cache: &mut ValidationCache,
    round: &mut Round,
) -> Result<(bool, String), String> {
    if let Some(hit) = cache.get(tree) {
        return Ok(hit.clone());
    }
    let started = std::time::Instant::now();
    round.validations += 1;
    let verdict = runner
        .run(GateRequest {
            gate: SPECULATION_GATE.into(),
            covers: covers.to_vec(),
            commands: commands.to_vec(),
            pinned: Some(VerifiedCommit {
                sha: commit.to_owned(),
                verified_by: "speculative merge".into(),
                verified_ms: 0,
            }),
        })
        .await
        .map_err(|e| format!("speculative validation could not run: {e}"))?;
    round.validation_ms += started.elapsed().as_millis() as u64;
    let result = (verdict.passed, verdict.output.to_description(DETAIL_CHARS));
    cache.insert(tree.to_owned(), result.clone());
    Ok(result)
}
