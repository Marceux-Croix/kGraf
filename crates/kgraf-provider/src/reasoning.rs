//! The reasoning level of every model call kGraf makes, as one setting
//! (decision 0098).
//!
//! A run makes generative calls of a fixed set of kinds, each named by the
//! `call_kind` its ledger row records ([`ReasoningProcess`]). One universal
//! level applies to all of them; an override for a group of kinds
//! ([`ReasoningGroup`]) or for one kind replaces it there. Each level is
//! either [`ReasoningLevel::Unset`] (no reasoning field is sent, so the
//! model's own default applies) or one of the four [`ReasoningEffort`]
//! values.
//!
//! The type is `Copy` (one fixed slot per scope, no map): it rides in the
//! user's limits, which the desktop shares by value.
//!
//! Resolution, most specific first: the kind's own override, its group's
//! override, the universal level, then unset. A level is never lowered or
//! replaced on the way: what resolves is what the request carries.

use crate::message::ReasoningEffort;

/// One generative model-call kind of a run, by its ledger `call_kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReasoningProcess {
    /// The first plan of a run, on the planner's route.
    Decompose,
    /// The interface contracts of a plan, on the planner's route.
    Interfaces,
    /// Whether the next contract still serves the instruction.
    Reground,
    /// The orchestrator answers a worker's question.
    AnswerQuestion,
    /// The orchestrator judges a proposed connection between contracts.
    JudgeConnections,
    /// Reading the project's corpus into a plan (`--corpus`).
    Interpret,
    /// Classifying a user's question: who answers it.
    QuestionRoute,
    /// The orchestrator answering a user's question itself.
    QuestionAnswer,
    /// The orchestrator reviewing a challenge to an interface.
    ChallengeReview,
    /// A party's statement of an interface, on that party's own route.
    Handshake,
    /// A consumer's own test of a producer's interface, on its route.
    ConsumerTest,
    /// The cross-vendor review of a delivery, on a consumer's route.
    InterfaceReview,
    /// One round of a worker's loop.
    WorkerRound,
    /// A worker's summary call when it compacts its context.
    WorkerCompaction,
}

/// A group of call kinds an override can name at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReasoningGroup {
    /// Every call on the planner's route.
    Planner,
    /// The contract-check calls made on a worker route's model.
    Checks,
    /// A worker's own calls.
    Worker,
}

impl ReasoningGroup {
    pub const ALL: [ReasoningGroup; 3] = [
        ReasoningGroup::Planner,
        ReasoningGroup::Checks,
        ReasoningGroup::Worker,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningGroup::Planner => "planner",
            ReasoningGroup::Checks => "checks",
            ReasoningGroup::Worker => "worker",
        }
    }
}

impl ReasoningProcess {
    /// Every kind, grouped, in the order a user is shown them.
    pub const ALL: [ReasoningProcess; 14] = [
        ReasoningProcess::Decompose,
        ReasoningProcess::Interfaces,
        ReasoningProcess::Reground,
        ReasoningProcess::AnswerQuestion,
        ReasoningProcess::JudgeConnections,
        ReasoningProcess::Interpret,
        ReasoningProcess::QuestionRoute,
        ReasoningProcess::QuestionAnswer,
        ReasoningProcess::ChallengeReview,
        ReasoningProcess::Handshake,
        ReasoningProcess::ConsumerTest,
        ReasoningProcess::InterfaceReview,
        ReasoningProcess::WorkerRound,
        ReasoningProcess::WorkerCompaction,
    ];

    /// The `call_kind` the ledger records for this kind of call. Also its
    /// name in flags and files.
    pub fn call_kind(self) -> &'static str {
        match self {
            ReasoningProcess::Decompose => "decompose",
            ReasoningProcess::Interfaces => "interfaces",
            ReasoningProcess::Reground => "reground",
            ReasoningProcess::AnswerQuestion => "answer_question",
            ReasoningProcess::JudgeConnections => "judge_connections",
            ReasoningProcess::Interpret => "interpret",
            ReasoningProcess::QuestionRoute => "question_route",
            ReasoningProcess::QuestionAnswer => "question_answer",
            ReasoningProcess::ChallengeReview => "challenge_review",
            ReasoningProcess::Handshake => "handshake",
            ReasoningProcess::ConsumerTest => "consumer_test",
            ReasoningProcess::InterfaceReview => "interface_review",
            ReasoningProcess::WorkerRound => "worker_round",
            ReasoningProcess::WorkerCompaction => "worker_compaction",
        }
    }

    /// The kind whose ledger `call_kind` is `kind`.
    pub fn from_call_kind(kind: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.call_kind() == kind)
    }

    pub fn group(self) -> ReasoningGroup {
        match self {
            ReasoningProcess::Decompose
            | ReasoningProcess::Interfaces
            | ReasoningProcess::Reground
            | ReasoningProcess::AnswerQuestion
            | ReasoningProcess::JudgeConnections
            | ReasoningProcess::Interpret
            | ReasoningProcess::QuestionRoute
            | ReasoningProcess::QuestionAnswer
            | ReasoningProcess::ChallengeReview => ReasoningGroup::Planner,
            ReasoningProcess::Handshake
            | ReasoningProcess::ConsumerTest
            | ReasoningProcess::InterfaceReview => ReasoningGroup::Checks,
            ReasoningProcess::WorkerRound | ReasoningProcess::WorkerCompaction => {
                ReasoningGroup::Worker
            }
        }
    }
}

/// One level as a setting holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningLevel {
    /// Send no reasoning field: the model's own default.
    Unset,
    /// Send this level.
    Effort(ReasoningEffort),
}

impl ReasoningLevel {
    /// Every value, in the order a user is shown them.
    pub const ALL: [ReasoningLevel; 5] = [
        ReasoningLevel::Unset,
        ReasoningLevel::Effort(ReasoningEffort::Off),
        ReasoningLevel::Effort(ReasoningEffort::Low),
        ReasoningLevel::Effort(ReasoningEffort::Medium),
        ReasoningLevel::Effort(ReasoningEffort::High),
    ];

    /// The level a request carries: `None` for unset.
    pub fn effort(self) -> Option<ReasoningEffort> {
        match self {
            ReasoningLevel::Unset => None,
            ReasoningLevel::Effort(e) => Some(e),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningLevel::Unset => "unset",
            ReasoningLevel::Effort(e) => e.as_str(),
        }
    }
}

impl From<Option<ReasoningEffort>> for ReasoningLevel {
    fn from(effort: Option<ReasoningEffort>) -> Self {
        effort.map_or(ReasoningLevel::Unset, ReasoningLevel::Effort)
    }
}

impl From<ReasoningEffort> for ReasoningLevel {
    fn from(effort: ReasoningEffort) -> Self {
        ReasoningLevel::Effort(effort)
    }
}

impl std::fmt::Display for ReasoningLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReasoningLevel {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|level| level.as_str() == text)
            .ok_or_else(|| {
                format!(
                    "`{text}` is not a reasoning level; use one of: {}",
                    Self::ALL.map(Self::as_str).join(", ")
                )
            })
    }
}

/// What a level applies to: every call, a group, or one call kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReasoningScope {
    All,
    Group(ReasoningGroup),
    Process(ReasoningProcess),
}

/// The number of scopes: one universal, the groups, the kinds.
const SCOPES: usize = 1 + ReasoningGroup::ALL.len() + ReasoningProcess::ALL.len();

impl ReasoningScope {
    /// Every scope, broadest first.
    pub fn all() -> impl Iterator<Item = ReasoningScope> {
        std::iter::once(ReasoningScope::All)
            .chain(ReasoningGroup::ALL.into_iter().map(ReasoningScope::Group))
            .chain(
                ReasoningProcess::ALL
                    .into_iter()
                    .map(ReasoningScope::Process),
            )
    }

    /// The scope's name in flags and files: `all`, a group, or a call kind.
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningScope::All => "all",
            ReasoningScope::Group(g) => g.as_str(),
            ReasoningScope::Process(p) => p.call_kind(),
        }
    }

    fn slot(self) -> usize {
        match self {
            ReasoningScope::All => 0,
            ReasoningScope::Group(g) => {
                1 + ReasoningGroup::ALL
                    .iter()
                    .position(|x| *x == g)
                    .unwrap_or_default()
            }
            ReasoningScope::Process(p) => {
                1 + ReasoningGroup::ALL.len()
                    + ReasoningProcess::ALL
                        .iter()
                        .position(|x| *x == p)
                        .unwrap_or_default()
            }
        }
    }
}

impl std::fmt::Display for ReasoningScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReasoningScope {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::all().find(|s| s.as_str() == text).ok_or_else(|| {
            format!(
                "`{text}` is not a reasoning scope; use `all`, a group ({}) or a call kind ({})",
                ReasoningGroup::ALL.map(ReasoningGroup::as_str).join(", "),
                ReasoningProcess::ALL
                    .map(ReasoningProcess::call_kind)
                    .join(", ")
            )
        })
    }
}

/// One `scope=level` assignment, as a flag gives it. A bare level is the
/// universal one (`all=<level>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReasoningAssignment {
    pub scope: ReasoningScope,
    pub level: ReasoningLevel,
}

impl std::str::FromStr for ReasoningAssignment {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.split_once('=') {
            Some((scope, level)) => Ok(Self {
                scope: scope.trim().parse()?,
                level: level.trim().parse()?,
            }),
            None => Ok(Self {
                scope: ReasoningScope::All,
                level: text.trim().parse()?,
            }),
        }
    }
}

/// The reasoning setting: a level for each scope that sets one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReasoningLevels {
    slots: [Option<ReasoningLevel>; SCOPES],
}

impl ReasoningLevels {
    /// Nothing set: every call sends no reasoning field.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `scope` to `level`.
    pub fn with(mut self, scope: ReasoningScope, level: ReasoningLevel) -> Self {
        self.set(scope, level);
        self
    }

    pub fn set(&mut self, scope: ReasoningScope, level: ReasoningLevel) {
        self.slots[scope.slot()] = Some(level);
    }

    /// The level `scope` itself sets, if it sets one.
    pub fn get(&self, scope: ReasoningScope) -> Option<ReasoningLevel> {
        self.slots[scope.slot()]
    }

    /// Whether no scope sets a level.
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    /// The scopes that set a level, broadest first.
    pub fn entries(&self) -> impl Iterator<Item = (ReasoningScope, ReasoningLevel)> + '_ {
        ReasoningScope::all().filter_map(|s| self.get(s).map(|l| (s, l)))
    }

    /// The level `process` resolves to, and the scope it came from (`None`:
    /// nothing set it, so it is unset).
    pub fn resolve(&self, process: ReasoningProcess) -> (ReasoningLevel, Option<ReasoningScope>) {
        [
            ReasoningScope::Process(process),
            ReasoningScope::Group(process.group()),
            ReasoningScope::All,
        ]
        .into_iter()
        .find_map(|s| self.get(s).map(|l| (l, Some(s))))
        .unwrap_or((ReasoningLevel::Unset, None))
    }

    /// The level a request of `process` carries.
    pub fn effort(&self, process: ReasoningProcess) -> Option<ReasoningEffort> {
        self.resolve(process).0.effort()
    }

    /// The level a request whose ledger `call_kind` is `kind` carries. A
    /// kind that is not a [`ReasoningProcess`] takes the universal level:
    /// the universal value applies to every call, named or not.
    pub fn effort_for_call_kind(&self, kind: &str) -> Option<ReasoningEffort> {
        match ReasoningProcess::from_call_kind(kind) {
            Some(process) => self.effort(process),
            None => self
                .get(ReasoningScope::All)
                .and_then(ReasoningLevel::effort),
        }
    }

    /// These levels over `lower`: each scope this sets wins, each scope it
    /// leaves out takes `lower`'s. A flag over a file, a file over a user
    /// setting.
    pub fn over(self, lower: ReasoningLevels) -> ReasoningLevels {
        let mut out = lower;
        for (scope, level) in self.entries() {
            out.set(scope, level);
        }
        out
    }

    /// Levels from assignments. A scope assigned twice is refused, not
    /// resolved by order: two values for one scope is a mistake to show.
    pub fn from_assignments(
        assignments: impl IntoIterator<Item = ReasoningAssignment>,
    ) -> Result<Self, String> {
        let mut out = Self::new();
        for a in assignments {
            if let Some(before) = out.get(a.scope) {
                return Err(format!(
                    "reasoning scope `{}` is set twice (`{before}` and `{}`); set it once",
                    a.scope, a.level
                ));
            }
            out.set(a.scope, a.level);
        }
        Ok(out)
    }
}

impl serde::Serialize for ReasoningLevels {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(None)?;
        for (scope, level) in self.entries() {
            map.serialize_entry(scope.as_str(), level.as_str())?;
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for ReasoningLevels {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let map = std::collections::BTreeMap::<String, String>::deserialize(d)?;
        let mut out = Self::new();
        for (scope, level) in map {
            let scope: ReasoningScope = scope.parse().map_err(D::Error::custom)?;
            let level: ReasoningLevel = level.parse().map_err(D::Error::custom)?;
            out.set(scope, level);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOW: ReasoningLevel = ReasoningLevel::Effort(ReasoningEffort::Low);
    const HIGH: ReasoningLevel = ReasoningLevel::Effort(ReasoningEffort::High);
    const OFF: ReasoningLevel = ReasoningLevel::Effort(ReasoningEffort::Off);

    /// Every call kind resolves to the universal level when nothing else is
    /// set, and to nothing when nothing is set at all.
    #[test]
    fn the_universal_level_reaches_every_call_kind() {
        let none = ReasoningLevels::new();
        let all = ReasoningLevels::new().with(ReasoningScope::All, LOW);
        for p in ReasoningProcess::ALL {
            assert_eq!(none.effort(p), None, "{p:?}");
            assert_eq!(all.effort(p), Some(ReasoningEffort::Low), "{p:?}");
            assert_eq!(all.resolve(p).1, Some(ReasoningScope::All));
        }
    }

    /// A kind's own override beats its group's, which beats the universal
    /// level; an explicit `unset` override sends nothing even under a
    /// universal level.
    #[test]
    fn an_override_beats_the_universal_level() {
        let levels = ReasoningLevels::new()
            .with(ReasoningScope::All, LOW)
            .with(ReasoningScope::Group(ReasoningGroup::Planner), OFF)
            .with(ReasoningScope::Process(ReasoningProcess::Reground), HIGH)
            .with(
                ReasoningScope::Process(ReasoningProcess::WorkerCompaction),
                ReasoningLevel::Unset,
            );
        assert_eq!(
            levels.effort(ReasoningProcess::Reground),
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            levels.effort(ReasoningProcess::Decompose),
            Some(ReasoningEffort::Off)
        );
        assert_eq!(
            levels.effort(ReasoningProcess::WorkerRound),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(levels.effort(ReasoningProcess::WorkerCompaction), None);
        assert_eq!(
            levels.resolve(ReasoningProcess::WorkerCompaction).1,
            Some(ReasoningScope::Process(ReasoningProcess::WorkerCompaction))
        );
    }

    #[test]
    fn a_call_kind_maps_to_its_process_and_an_unknown_kind_takes_the_universal_level() {
        for p in ReasoningProcess::ALL {
            assert_eq!(ReasoningProcess::from_call_kind(p.call_kind()), Some(p));
        }
        let levels = ReasoningLevels::new()
            .with(ReasoningScope::All, LOW)
            .with(ReasoningScope::Group(ReasoningGroup::Planner), HIGH);
        assert_eq!(
            levels.effort_for_call_kind("decompose"),
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            levels.effort_for_call_kind("not_a_kind"),
            Some(ReasoningEffort::Low)
        );
    }

    #[test]
    fn a_higher_layer_wins_scope_by_scope() {
        let user = ReasoningLevels::new()
            .with(ReasoningScope::All, LOW)
            .with(ReasoningScope::Group(ReasoningGroup::Planner), OFF);
        let flag = ReasoningLevels::new().with(ReasoningScope::All, HIGH);
        let merged = flag.over(user);
        assert_eq!(merged.get(ReasoningScope::All), Some(HIGH));
        assert_eq!(
            merged.get(ReasoningScope::Group(ReasoningGroup::Planner)),
            Some(OFF)
        );
    }

    #[test]
    fn assignments_parse_and_a_scope_set_twice_is_refused() {
        let a: ReasoningAssignment = "low".parse().expect("bare");
        assert_eq!(a.scope, ReasoningScope::All);
        let b: ReasoningAssignment = "planner=off".parse().expect("group");
        assert_eq!(b.scope, ReasoningScope::Group(ReasoningGroup::Planner));
        let c: ReasoningAssignment = "worker_compaction=unset".parse().expect("kind");
        assert_eq!(c.level, ReasoningLevel::Unset);
        assert!("none".parse::<ReasoningAssignment>().is_err());
        assert!("planer=off".parse::<ReasoningAssignment>().is_err());
        let twice = ReasoningLevels::from_assignments([b, "planner=low".parse().expect("p")]);
        assert!(twice.expect_err("twice").contains("set twice"));
    }

    #[test]
    fn the_setting_round_trips_as_a_map_and_refuses_an_unknown_scope() {
        let levels = ReasoningLevels::new().with(ReasoningScope::All, LOW).with(
            ReasoningScope::Process(ReasoningProcess::Handshake),
            ReasoningLevel::Unset,
        );
        let text = serde_json::to_string(&levels).expect("json");
        assert_eq!(text, r#"{"all":"low","handshake":"unset"}"#);
        let back: ReasoningLevels = serde_json::from_str(&text).expect("back");
        assert_eq!(back, levels);
        assert!(serde_json::from_str::<ReasoningLevels>(r#"{"workers":"low"}"#).is_err());
        assert!(serde_json::from_str::<ReasoningLevels>(r#"{"all":"none"}"#).is_err());
    }
}
