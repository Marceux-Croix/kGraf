//! What a run records about landing (decision 0100, section 5): the branch
//! it started from, the commits its root gate passed on, each interface's
//! stub and verified-producer commits, the landing journal, and the user's
//! answers to land suggestions.
//!
//! One field of the run record ([`crate::RunStateRecord::land`]), defaulted,
//! so a record from before it reads as empty. Pure rules over it live here
//! too: which commit lands by default, whether a commit holds a stub whose
//! producer has not verified, and the merge commit's message and trailers.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// The run's landing state, one record field.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LandBook {
    /// The branch checked out when the run first started, and its commit
    /// then. `None` for a run from before the field: the land target is then
    /// the branch checked out at land time, and the land says so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_branch: Option<StartBranch>,
    /// Every commit the root gate passed on, in order. A pass while covers
    /// are still open is recorded too: it makes that commit landable.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub root_gate_passes: Vec<GatePass>,
    /// Interface id -> its stub and producer commits. Written since this
    /// field exists; [`Self::stubs_recorded`] says so.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub interfaces: BTreeMap<String, InterfaceLand>,
    /// Set once this run records stub lands here. A run whose stubs landed
    /// before cannot prove its producers verified, and a land of it is
    /// refused naming the interface.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stubs_recorded: bool,
    /// The land in progress: written before any ref moves, cleared when the
    /// land is complete. A land interrupted between its steps resumes from
    /// it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landing: Option<LandingEntry>,
    /// Completed lands, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub landed: Vec<LandingEntry>,
    /// When the user last answered a land suggestion with "Not now"
    /// (milliseconds since the epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub land_declined_at: Option<i64>,
    /// How many land suggestions the user declined.
    #[serde(skip_serializing_if = "is_zero")]
    pub land_decline_count: u32,
    /// Verified contracts at the last decline: a new suggestion also needs
    /// new verified work since.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_at_decline: Option<usize>,
    /// The hooks and program-valued git settings when the run started. A
    /// land runs the user's own git (hooks, filters, signing programs); a
    /// change since the run started, which a worker could have made, needs
    /// the user's yes naming it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub programs: Option<ProgramSnapshot>,
}

/// The change list's line for a run with no program snapshot (from before
/// the field).
pub const NO_PROGRAM_SNAPSHOT: &str = "this run recorded no snapshot of the repository's hooks \
and program settings when it started, so kGraf cannot tell whether a worker changed them";

/// What the user's own git ran when the run started, and what a land
/// enforces: the hooks are copied into kGraf's own run directory (outside
/// the repository, not writable by workers) and every land runs its user
/// git with `core.hooksPath` at that copy; every program-valued setting is
/// passed with its run-start value, and one added since is passed empty
/// (disabled). So hooks, filters, drivers and a file-system monitor a worker
/// added, by any path, never run in a land.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramSnapshot {
    /// The hooks directory as git resolved it at run start.
    pub hooks_dir: String,
    /// A hash of the copied hooks (names, modes, contents).
    pub hooks_hash: String,
    /// Program-valued settings at run start (`core.fsmonitor`,
    /// `filter.*.smudge`, `gpg.program`, ...), key as `git config --list`
    /// prints it, to the effective value.
    pub config: BTreeMap<String, String>,
    /// Where the hooks were copied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_copy: Option<String>,
}

impl ProgramSnapshot {
    /// A digest of the snapshot, bound into the landing journal.
    pub fn digest(&self) -> String {
        let text = serde_json::to_string(self).unwrap_or_default();
        blake3::hash(text.as_bytes()).to_hex().to_string()
    }

    /// Signing settings that changed from `self` to `now`, one line each.
    /// Signing runs inside kGraf's own git, where the run-start values
    /// cannot be passed, so a change is asked about.
    pub fn signing_changes(&self, now: &ProgramSnapshot) -> Vec<String> {
        let keys: BTreeSet<&String> = self
            .config
            .keys()
            .chain(now.config.keys())
            .filter(|k| k.starts_with("gpg."))
            .collect();
        let mut out = Vec::new();
        for k in keys {
            match (self.config.get(k), now.config.get(k)) {
                (Some(a), Some(b)) if a != b => {
                    out.push(format!("{k} changed from `{a}` to `{b}`"))
                }
                (None, Some(b)) => out.push(format!("{k} was set to `{b}`")),
                (Some(a), None) => out.push(format!("{k} (was `{a}`) was removed")),
                _ => {}
            }
        }
        out
    }
}

impl LandBook {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Record a root-gate pass on `sha` (once per commit).
    pub fn note_root_gate_pass(&mut self, sha: &str, at_ms: i64) {
        if !self.root_gate_passes.iter().any(|p| p.sha == sha) {
            self.root_gate_passes.push(GatePass {
                sha: sha.to_owned(),
                at_ms,
            });
        }
    }

    /// Record that `interface`'s stub landed at `commit`, its producer being
    /// `producer`. A new stub (a revision) makes the earlier producer
    /// verification void: the producer must verify against the new one.
    pub fn note_stub_landed(&mut self, interface: &str, commit: &str, producer: Option<&str>) {
        self.stubs_recorded = true;
        let entry = self.interfaces.entry(interface.to_owned()).or_default();
        if entry.stub_commit.as_deref() == Some(commit) {
            return;
        }
        if let Some(old) = entry.stub_commit.take() {
            entry.earlier_stub_commits.push(old);
        }
        entry.stub_commit = Some(commit.to_owned());
        if let Some(old) = entry.producer_verified_commit.take() {
            entry.earlier_producer_verified.push(old);
        }
        if producer.is_some() {
            entry.producer = producer.map(str::to_owned);
        }
    }

    /// Record that `contract` verified, merged at `commit`: every interface
    /// it produces now has a verified producer at that commit.
    pub fn note_contract_verified(&mut self, contract: &str, commit: &str) {
        for entry in self.interfaces.values_mut() {
            if entry.producer.as_deref() == Some(contract) && entry.stub_commit.is_some() {
                entry.producer_verified_commit = Some(commit.to_owned());
            }
        }
    }
}

/// The branch a run started from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartBranch {
    pub name: String,
    pub sha: String,
}

/// One root-gate pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatePass {
    pub sha: String,
    pub at_ms: i64,
}

/// One interface's stub and verified-producer commits.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InterfaceLand {
    /// The commit that landed the current stub version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stub_commit: Option<String>,
    /// Commits of earlier stub versions, oldest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub earlier_stub_commits: Vec<String>,
    /// The merge commit of the producer's verified work against the current
    /// stub; `None` until it verifies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer_verified_commit: Option<String>,
    /// Producer verifications against earlier stub versions, oldest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub earlier_producer_verified: Vec<String>,
    /// The producing contract.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
}

/// The landing journal entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingEntry {
    /// The run's integration branch (`kgraf/<run>`).
    pub run_branch: String,
    /// The commit of `run_branch` that lands.
    pub candidate: String,
    /// The merge commit: target first parent, candidate second.
    pub merge_commit: String,
    /// The target branch name.
    pub target: String,
    /// The target's commit before the land.
    pub target_old: String,
    pub gate: LandGate,
    pub started_ms: i64,
    /// Set when the target has moved to the merge commit.
    #[serde(default)]
    pub target_moved: bool,
    /// Git was stopped while it updated the target's worktree: files may be
    /// partly written until the user recovers it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub checkout_interrupted: bool,
    /// The config files under the work tree the merge changes, as confirmed;
    /// the move refuses when they differ then.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_changes: Vec<String>,
    /// The digest of the run-start program snapshot the land runs with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub programs_digest: Option<String>,
    /// What happened to `run_branch` after the target moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fold: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<i64>,
}

/// What a land rests on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LandGate {
    /// The root gate passed on `candidate`, on the tree `tree` (the
    /// candidate's own, or the merge result's when the gate ran on it).
    Passed { candidate: String, tree: String },
    /// The user landed it after a warning; no gate passed. `gate` names the
    /// gate, `reason` why it did not pass.
    Untested { gate: String, reason: String },
}

/// The merge commit's message: a subject, the contracts it brings, the
/// untested line when it is one, then the trailers `kGraf-Run` and
/// `kGraf-Gate`. The gate trailer names the candidate the gate ran on and
/// the tree it judged, never the merge commit's own id (a commit cannot name
/// itself).
pub fn land_message(run: &str, target: &str, contracts: &[String], gate: &LandGate) -> String {
    let mut msg = String::new();
    let untested = matches!(gate, LandGate::Untested { .. });
    msg.push_str(&format!(
        "{}Land kGraf run {run} into {target}\n\n",
        if untested { "UNTESTED: " } else { "" }
    ));
    if let LandGate::Untested { gate, reason } = gate {
        msg.push_str(&format!(
            "kGraf: UNTESTED - root gate {gate} did not pass: {reason}\n\n"
        ));
    }
    if contracts.is_empty() {
        msg.push_str("Contracts: none recorded\n\n");
    } else {
        msg.push_str("Contracts:\n");
        for c in contracts {
            msg.push_str(&format!("- {c}\n"));
        }
        msg.push('\n');
    }
    msg.push_str(&format!("kGraf-Run: {run}\n"));
    match gate {
        LandGate::Passed { candidate, tree } => {
            msg.push_str(&format!("kGraf-Gate: {candidate} tree {tree}\n"));
        }
        LandGate::Untested { .. } => msg.push_str("kGraf-Gate: untested\n"),
    }
    msg
}

/// Reads ancestry from the repository: `Ok(true)` when `ancestor` is an
/// ancestor of, or equal to, `descendant`.
pub trait Ancestry {
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, String>;
}

/// Why a commit may not land because of interface stubs, one line per
/// interface (empty: landable). With contract checks on, an interface's stub
/// lands on the run's branch before its producer finishes (decision 0081);
/// a commit holding a stub is landable only when, for every stub version it
/// holds, a producer verification that came after that stub is also in it.
///
/// `known_stubs` is every interface whose stub the run is known to have
/// landed (from the trace or the contract book): one with no entry in the
/// book refuses, since its producer cannot be proven verified.
pub fn stub_blockers(
    book: &LandBook,
    known_stubs: &BTreeSet<String>,
    candidate: &str,
    repo: &dyn Ancestry,
) -> Result<Vec<String>, String> {
    let mut blockers = Vec::new();
    for id in known_stubs {
        if !book.interfaces.contains_key(id) {
            blockers.push(format!(
                "interface `{id}`: its stub landed before this run recorded stub lands, so \
                 its producer cannot be shown verified"
            ));
        }
    }
    for (id, entry) in &book.interfaces {
        let stubs: Vec<&String> = entry
            .earlier_stub_commits
            .iter()
            .chain(entry.stub_commit.iter())
            .collect();
        let mut held = Vec::new();
        for stub in stubs {
            if repo.is_ancestor(stub, candidate)? {
                held.push(stub);
            }
        }
        if held.is_empty() {
            continue;
        }
        let verifications: Vec<&String> = entry
            .earlier_producer_verified
            .iter()
            .chain(entry.producer_verified_commit.iter())
            .collect();
        let mut in_candidate = Vec::new();
        for p in verifications {
            if repo.is_ancestor(p, candidate)? {
                in_candidate.push(p);
            }
        }
        let mut verified_after_all = true;
        for stub in &held {
            let mut covered = false;
            for p in &in_candidate {
                if repo.is_ancestor(stub, p)? {
                    covered = true;
                    break;
                }
            }
            verified_after_all &= covered;
        }
        if !verified_after_all {
            let producer = entry
                .producer
                .as_deref()
                .map(|p| format!(" (producer `{p}`)"))
                .unwrap_or_default();
            blockers.push(format!(
                "interface `{id}`{producer}: the commit holds its stub and not its verified \
                 producer"
            ));
        }
    }
    Ok(blockers)
}

/// The newest root-gate pass that is on the run's branch (`on_branch`), the
/// default candidate. `None`: no gate passed on a commit of the branch.
pub fn newest_passed(
    book: &LandBook,
    on_branch: &dyn Fn(&str) -> Result<bool, String>,
) -> Result<Option<String>, String> {
    for pass in book.root_gate_passes.iter().rev() {
        if on_branch(&pass.sha)? {
            return Ok(Some(pass.sha.clone()));
        }
    }
    Ok(None)
}

/// Interfaces whose stub a run's trace records as landed (decision 0081),
/// for a land with no live loop: the trace is what the contract book is
/// rebuilt from. Lines that do not parse are skipped; the stub rule then
/// refuses on the record alone.
pub fn stub_lands_in_trace(trace: &str) -> BTreeSet<String> {
    trace
        .lines()
        .filter(|l| l.contains("\"stub_landed\""))
        .filter_map(|l| serde_json::from_str::<crate::trace::TraceEvent>(l).ok())
        .filter_map(|e| match e {
            crate::trace::TraceEvent::Contract { record, .. } => match *record {
                crate::challenge::ContractEvent::StubLanded { interface, .. } => Some(interface),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A linear history `c0 <- c1 <- ...`: an ancestor has a smaller index.
    struct Line(Vec<&'static str>);
    impl Ancestry for Line {
        fn is_ancestor(&self, a: &str, d: &str) -> Result<bool, String> {
            let pos = |s: &str| self.0.iter().position(|c| *c == s).ok_or(format!("{s}?"));
            Ok(pos(a)? <= pos(d)?)
        }
    }

    #[test]
    fn a_stub_without_its_verified_producer_blocks() {
        let mut book = LandBook::default();
        book.note_stub_landed("cfg", "s1", Some("producer"));
        let repo = Line(vec!["base", "s1", "work", "p1", "tip"]);
        let none = BTreeSet::new();
        assert!(
            stub_blockers(&book, &none, "base", &repo)
                .unwrap()
                .is_empty()
        );
        let blocked = stub_blockers(&book, &none, "work", &repo).unwrap();
        assert_eq!(blocked.len(), 1);
        assert!(blocked[0].contains("`cfg`") && blocked[0].contains("producer"));
        book.note_contract_verified("producer", "p1");
        assert!(stub_blockers(&book, &none, "work", &repo).unwrap().len() == 1);
        assert!(
            stub_blockers(&book, &none, "tip", &repo)
                .unwrap()
                .is_empty()
        );
    }

    /// A revised stub v2 replaces v1: the producer's verification of v1 does
    /// not cover v2, so a commit holding v2 waits for a new verification.
    #[test]
    fn a_revised_stub_voids_the_earlier_producer_verification() {
        let mut book = LandBook::default();
        book.note_stub_landed("cfg", "s1", Some("producer"));
        book.note_contract_verified("producer", "p1");
        book.note_stub_landed("cfg", "s2", None);
        let repo = Line(vec!["s1", "p1", "s2", "p2"]);
        let none = BTreeSet::new();
        assert!(
            stub_blockers(&book, &none, "p1", &repo).unwrap().is_empty(),
            "v1 and its producer only"
        );
        assert_eq!(stub_blockers(&book, &none, "s2", &repo).unwrap().len(), 1);
        book.note_contract_verified("producer", "p2");
        assert!(stub_blockers(&book, &none, "p2", &repo).unwrap().is_empty());
    }

    /// A producer verified against v2 does not cover a commit holding only v1.
    #[test]
    fn a_producer_verification_must_follow_every_held_stub() {
        let mut book = LandBook::default();
        book.note_stub_landed("cfg", "s1", Some("producer"));
        book.note_stub_landed("cfg", "s2", None);
        book.note_contract_verified("producer", "p2");
        // Non-linear: p2 descends from s2 only in this table.
        struct Graph;
        impl Ancestry for Graph {
            fn is_ancestor(&self, a: &str, d: &str) -> Result<bool, String> {
                Ok(a == d
                    || matches!(
                        (a, d),
                        ("s1", "x") | ("s2", "p2") | ("s1", "p2") | ("p2", "x")
                    ))
            }
        }
        assert!(
            stub_blockers(&book, &BTreeSet::new(), "x", &Graph)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_dropped_producer_never_verifies_so_the_stub_blocks() {
        let mut book = LandBook::default();
        book.note_stub_landed("api", "s1", Some("gone"));
        let repo = Line(vec!["s1", "tip"]);
        let blocked = stub_blockers(&book, &BTreeSet::new(), "tip", &repo).unwrap();
        assert_eq!(blocked.len(), 1);
        assert!(blocked[0].contains("`api`"), "{blocked:?}");
    }

    #[test]
    fn an_old_run_without_stub_records_refuses_naming_the_interface() {
        let book = LandBook::default();
        let known: BTreeSet<String> = ["cfg".to_owned()].into();
        let blocked = stub_blockers(&book, &known, "tip", &Line(vec!["tip"])).unwrap();
        assert_eq!(blocked.len(), 1);
        assert!(blocked[0].contains("`cfg`") && blocked[0].contains("before"));
    }

    #[test]
    fn the_message_carries_both_trailers_and_never_the_merge_commit() {
        let gate = LandGate::Passed {
            candidate: "abc".into(),
            tree: "def".into(),
        };
        let msg = land_message("r1", "main", &["a".into(), "b".into()], &gate);
        assert!(msg.starts_with("Land kGraf run r1 into main\n"));
        assert!(msg.contains("- a\n- b\n"));
        assert!(
            msg.ends_with("kGraf-Run: r1\nkGraf-Gate: abc tree def\n"),
            "{msg}"
        );
        let untested = LandGate::Untested {
            gate: "gate:root".into(),
            reason: "never ran".into(),
        };
        let msg = land_message("r1", "main", &[], &untested);
        assert!(msg.starts_with("UNTESTED: "));
        assert!(msg.contains("kGraf: UNTESTED - root gate gate:root did not pass: never ran"));
        assert!(msg.ends_with("kGraf-Gate: untested\n"));
    }

    #[test]
    fn the_default_candidate_is_the_newest_pass_on_the_branch() {
        let mut book = LandBook::default();
        book.note_root_gate_pass("a", 1);
        book.note_root_gate_pass("b", 2);
        book.note_root_gate_pass("a", 3);
        assert_eq!(book.root_gate_passes.len(), 2, "once per commit");
        let on = |s: &str| Ok(s == "a");
        assert_eq!(newest_passed(&book, &on).unwrap().as_deref(), Some("a"));
        let none = |_: &str| Ok(false);
        assert_eq!(newest_passed(&book, &none).unwrap(), None);
    }

    #[test]
    fn stub_lands_are_read_from_the_trace() {
        let line = serde_json::to_string(&crate::trace::TraceEvent::Contract {
            at_ms: 1,
            record: Box::new(crate::challenge::ContractEvent::StubLanded {
                interface: "cfg".into(),
                version: 1,
                commit: "abc".into(),
            }),
            counters: Default::default(),
        })
        .unwrap();
        let text = format!("{line}\nnot json \"stub_landed\"\n");
        assert_eq!(
            stub_lands_in_trace(&text),
            BTreeSet::from(["cfg".to_owned()])
        );
    }

    #[test]
    fn signing_changes_are_named_and_others_are_not() {
        let then = ProgramSnapshot {
            config: [
                ("gpg.program".to_owned(), "gpg".to_owned()),
                ("core.fsmonitor".to_owned(), "x".to_owned()),
            ]
            .into(),
            ..ProgramSnapshot::default()
        };
        assert!(then.signing_changes(&then).is_empty());
        let now = ProgramSnapshot {
            config: [
                ("gpg.program".to_owned(), "/tmp/x".to_owned()),
                ("gpg.format".to_owned(), "ssh".to_owned()),
                ("core.fsmonitor".to_owned(), "y".to_owned()),
            ]
            .into(),
            ..ProgramSnapshot::default()
        };
        let c = then.signing_changes(&now);
        assert_eq!(c.len(), 2, "{c:?}");
        assert_ne!(then.digest(), now.digest());
    }

    #[test]
    fn an_old_record_reads_as_empty() {
        let book: LandBook = serde_json::from_str("{}").unwrap();
        assert!(book.is_empty());
        assert_eq!(serde_json::to_string(&book).unwrap(), "{}");
    }
}
