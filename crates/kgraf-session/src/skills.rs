//! Skills at run time (decision 0053).
//!
//! The catalog (names and descriptions) enters the shared prefix; bodies load on
//! demand into a worker's private remainder, tagged as skill-injected. The two
//! frontmatter controls are **enforced here, by the runtime, never by the prompt**:
//!
//! - `disable-model-invocation`: [`SkillCatalog::activate`] refuses a model
//!   invocation, and the skill is left out of the model-facing catalog.
//! - `allowed-tools`: an activated skill narrows a [`ToolPolicy`]. The policy is a
//!   `CallPolicy` on the worker's approval chain, so a refused call is denied,
//!   recorded, and never yields an `ApprovedCall`; [`ToolPolicy::dispatch`] checks
//!   again at execution.
//!
//! **The tool definitions are never filtered.** Every worker on one vendor and
//! model receives the same, full definitions, because they sit in the
//! byte-identical shared prefix (0043). A restricted worker sees a tool it may not
//! call; calling it is refused. Filtering would change that worker's prefix bytes
//! and cost a cache write for a restriction the runtime already enforces.
//!
//! **Pattern grammar** (Claude Code's `allowed-tools` form, since that is where most
//! ingested skills come from):
//!
//! - `Name`: every call to the tool with this wire name.
//! - `Prefix*`: every tool whose wire name starts with `Prefix`.
//! - `mcp__server`: every tool of that MCP server.
//! - `Name(*)`: same as `Name`.
//! - `Name(prefix:*)`: calls whose permission subject is `prefix` or starts with
//!   `prefix` followed by whitespace. The boundary keeps `git:*` from matching `gitk`.
//! - `Name(prefix*)`: calls whose subject starts with `prefix`.
//! - `Name(exact)`: calls whose subject equals `exact`.
//!
//! A scoped pattern against a tool that reports no subject refuses. A shell tool
//! must therefore report a subject only for a command it has parsed as simple; for
//! a compound command (`git status && rm -rf x`) it must report `None`, or a
//! `git:*` pattern would admit the whole line. That parsing belongs to the command
//! policy (codex-rs `execpolicy/`), not here.
//!
//! **Several active skills intersect.** Each one narrows; none widens.

use crate::SessionError;
use kgraf_instructions::{Inventory, Skill};
use kgraf_tools::approval::{ApprovedCall, CallPolicy};
use kgraf_tools::{MCP_PREFIX, QualifiedName, ToolContext, ToolOutput, ToolRegistry};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Who is invoking a skill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invoker {
    /// The model, through the skill tool.
    Model,
    /// The user, directly.
    User,
}

/// A catalog entry: what the shared prefix carries for one skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
}

/// The usable skills, by name.
#[derive(Clone, Debug, Default)]
pub struct SkillCatalog {
    skills: BTreeMap<String, Skill>,
}

impl SkillCatalog {
    /// A catalog of the inventory's usable skills. Unresolved conflicts are already
    /// absent from `Inventory::skills`, so they are absent here (0053).
    pub fn from_inventory(inventory: &Inventory) -> Result<Self, SessionError> {
        Self::from_skills(inventory.skills.iter().cloned())
    }

    /// A catalog of the given skills. Two with one name are refused, not merged.
    pub fn from_skills(skills: impl IntoIterator<Item = Skill>) -> Result<Self, SessionError> {
        let mut map = BTreeMap::new();
        for skill in skills {
            if map.contains_key(&skill.name) {
                return Err(SessionError::DuplicateSkill { name: skill.name });
            }
            map.insert(skill.name.clone(), skill);
        }
        Ok(Self { skills: map })
    }

    /// The entries the model sees, sorted by name. Skills the model may not invoke
    /// are left out, so the model is not offered what it will be refused.
    pub fn model_catalog(&self) -> Vec<CatalogEntry> {
        self.skills
            .values()
            .filter(|s| !s.disable_model_invocation)
            .map(|s| CatalogEntry {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect()
    }

    /// Activate a skill for `invoker`, enforcing the frontmatter controls.
    pub fn activate(&self, name: &str, invoker: Invoker) -> Result<ActiveSkill, SessionError> {
        let skill = self
            .skills
            .get(name)
            .ok_or_else(|| SessionError::UnknownSkill {
                name: name.to_owned(),
            })?;
        match invoker {
            Invoker::Model if skill.disable_model_invocation => {
                return Err(SessionError::ModelInvocationDisabled {
                    name: name.to_owned(),
                });
            }
            Invoker::User if !skill.user_invocable => {
                return Err(SessionError::NotUserInvocable {
                    name: name.to_owned(),
                });
            }
            _ => {}
        }
        let allowed = match &skill.allowed_tools {
            None => None,
            Some(entries) => Some(
                entries
                    .iter()
                    .map(|e| {
                        ToolPattern::parse(e).ok_or_else(|| SessionError::BadAllowedTools {
                            skill: name.to_owned(),
                            entry: e.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        };
        Ok(ActiveSkill {
            name: skill.name.clone(),
            hash: skill.hash.clone(),
            body: skill.body.clone(),
            allowed,
        })
    }
}

/// A skill activated for one worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveSkill {
    name: String,
    hash: String,
    body: String,
    allowed: Option<Vec<ToolPattern>>,
}

impl ActiveSkill {
    /// The skill's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the skill restricts the tool set.
    pub fn restricts_tools(&self) -> bool {
        self.allowed.is_some()
    }

    /// The body as a tagged message for the private remainder, so nothing
    /// downstream mistakes it for something the user said (0053).
    pub fn injection(&self) -> SkillInjection {
        SkillInjection {
            skill: self.name.clone(),
            skill_hash: self.hash.clone(),
            body: self.body.clone(),
        }
    }
}

/// A skill body injected into a worker's context, tagged with its origin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInjection {
    pub skill: String,
    /// The content hash of the skill version used.
    pub skill_hash: String,
    pub body: String,
}

/// One `allowed-tools` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolPattern {
    name: NameMatch,
    scope: Scope,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NameMatch {
    Exact(String),
    Prefix(String),
    Server(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Scope {
    Any,
    WordPrefix(String),
    Prefix(String),
    Exact(String),
}

impl ToolPattern {
    /// Parse one entry, or `None` when it cannot be read.
    fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim();
        let (name, scope) = match entry.find('(') {
            None => (entry, Scope::Any),
            Some(open) => {
                let inner = entry[open + 1..].strip_suffix(')')?;
                if inner.is_empty() {
                    return None;
                }
                let scope = if inner == "*" {
                    Scope::Any
                } else if let Some(p) = inner.strip_suffix(":*") {
                    Scope::WordPrefix(p.to_owned())
                } else if let Some(p) = inner.strip_suffix('*') {
                    Scope::Prefix(p.to_owned())
                } else {
                    Scope::Exact(inner.to_owned())
                };
                (&entry[..open], scope)
            }
        };
        if name.is_empty() || name.contains(char::is_whitespace) {
            return None;
        }
        let name = if let Some(prefix) = name.strip_suffix('*') {
            NameMatch::Prefix(prefix.to_owned())
        } else if let Some(server) = name.strip_prefix(MCP_PREFIX)
            && !server.contains("__")
        {
            NameMatch::Server(server.to_owned())
        } else {
            NameMatch::Exact(name.to_owned())
        };
        Some(Self { name, scope })
    }

    fn names(&self, tool: &QualifiedName) -> bool {
        match &self.name {
            NameMatch::Exact(n) => tool.wire() == *n,
            NameMatch::Prefix(p) => tool.wire().starts_with(p.as_str()),
            NameMatch::Server(s) => tool.server() == Some(s.as_str()),
        }
    }

    fn permits(&self, tool: &QualifiedName, subject: Option<&str>) -> bool {
        if !self.names(tool) {
            return false;
        }
        match (&self.scope, subject) {
            (Scope::Any, _) => true,
            (_, None) => false,
            (Scope::Exact(e), Some(s)) => s == e,
            (Scope::Prefix(p), Some(s)) => s.starts_with(p.as_str()),
            (Scope::WordPrefix(p), Some(s)) => word_boundary_prefix(s, p),
        }
    }
}

/// Whether `subject` is exactly `prefix`, or starts with `prefix` followed by
/// whitespace.
///
/// Shared with [`crate::approval_store`]'s persisted allow rules: a raw `subject.starts_with(prefix)` would let a `"git "` rule admit
/// `"gitk ..."`, because `"gitk"` starts with `"git"` as a plain string. The
/// word boundary this function requires (`prefix` must be the whole subject,
/// or be followed by whitespace) is what a `Name(prefix:*)` pattern like
/// `Bash(git:*)` already relies on here; approval rules use the identical
/// rule so a persisted "always allow" cannot be broader than the equivalent
/// skill pattern would be.
pub(crate) fn word_boundary_prefix(subject: &str, prefix: &str) -> bool {
    subject == prefix
        || subject
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

/// The tool restrictions of every skill active on one worker.
#[derive(Clone, Debug, Default)]
pub struct ToolPolicy {
    /// One layer per restricting skill: (skill name, its patterns).
    layers: Vec<(String, Vec<ToolPattern>)>,
}

impl ToolPolicy {
    /// No restriction.
    pub fn unrestricted() -> Self {
        Self::default()
    }

    /// Narrow the policy by an activated skill. A skill without `allowed-tools`
    /// leaves it unchanged.
    pub fn restrict(&mut self, skill: &ActiveSkill) {
        if let Some(patterns) = &skill.allowed {
            self.layers.push((skill.name.clone(), patterns.clone()));
        }
    }

    /// Whether a call is permitted: every layer must permit it.
    pub fn check(&self, tool: &QualifiedName, subject: Option<&str>) -> Result<(), SessionError> {
        for (skill, patterns) in &self.layers {
            if !patterns.iter().any(|p| p.permits(tool, subject)) {
                return Err(SessionError::ToolRefused {
                    tool: tool.wire(),
                    skill: skill.clone(),
                });
            }
        }
        Ok(())
    }

    /// Check an approved call again and run it. The tool is not invoked when the
    /// policy refuses. The approval chain already applied this policy; the second
    /// check covers an approval made on a chain the policy was not attached to.
    pub async fn dispatch(
        &self,
        registry: &ToolRegistry,
        approved: ApprovedCall,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, SessionError> {
        let registered =
            registry
                .get(approved.tool())
                .ok_or_else(|| kgraf_tools::ToolError::UnknownTool {
                    name: approved.tool().to_owned(),
                })?;
        let subject = registered.tool().permission_subject(approved.input());
        self.check(registered.name(), subject.as_deref())?;
        Ok(registry.execute(approved, ctx).await?)
    }
}

impl CallPolicy for ToolPolicy {
    fn name(&self) -> &str {
        "skills:allowed-tools"
    }

    fn fingerprint(&self) -> String {
        format!("{:?}", self.layers)
    }

    fn check(&self, tool: &QualifiedName, subject: Option<&str>) -> Result<(), String> {
        ToolPolicy::check(self, tool, subject).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_tools::approval::{ApprovalChain, MemoryApprovalLog, PendingCall, Verdict};
    use kgraf_tools::{Access, CallPermit, DiscoveredTools, Exposure, Tool, ToolError, ToolView};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn skill(name: &str, allowed: Option<&[&str]>, disable_model: bool) -> Skill {
        Skill {
            name: name.into(),
            description: format!("{name} description"),
            frontmatter: json!({}),
            body: format!("{name} body"),
            files: vec![],
            allowed_tools: allowed.map(|a| a.iter().map(|s| (*s).to_owned()).collect()),
            disable_model_invocation: disable_model,
            user_invocable: true,
            hash: format!("{name}-hash"),
            sources: vec![],
        }
    }

    /// A tool that counts its invocations, so a test can prove a refused call
    /// never reached it.
    struct Counting {
        name: &'static str,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Tool for Counting {
        fn name(&self) -> &str {
            self.name
        }
        fn definition(&self) -> kgraf_tools::ToolDefinition {
            kgraf_tools::ToolDefinition {
                name: self.name.into(),
                description: "d".into(),
                input_schema: json!({"type": "object"}),
            }
        }
        fn access(&self, _input: &Value) -> Result<Access, ToolError> {
            Ok(Access::none())
        }
        fn permission_subject(&self, input: &Value) -> Option<String> {
            input["cmd"].as_str().map(str::to_owned)
        }
        async fn call(
            &self,
            _input: Value,
            _ctx: &ToolContext,
            _permit: &CallPermit,
        ) -> Result<ToolOutput, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::text("ran"))
        }
    }

    fn ctx() -> ToolContext {
        ToolContext {
            worker: "w1".into(),
            task: "t1".into(),
            workspace_root: std::env::temp_dir(),
            cancel: tokio_util::sync::CancellationToken::new(),
            progress: kgraf_tools::ProgressSink::default(),
        }
    }

    fn registry() -> (ToolRegistry, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let edit_calls = Arc::new(AtomicUsize::new(0));
        let bash_calls = Arc::new(AtomicUsize::new(0));
        let mut reg = ToolRegistry::new();
        reg.register_builtin(
            Arc::new(Counting {
                name: "Edit",
                calls: edit_calls.clone(),
            }),
            Exposure::Prefix,
        )
        .expect("edit");
        reg.register_builtin(
            Arc::new(Counting {
                name: "Bash",
                calls: bash_calls.clone(),
            }),
            Exposure::Prefix,
        )
        .expect("bash");
        (reg, edit_calls, bash_calls)
    }

    fn pending(tool: &str, input: Value) -> PendingCall {
        PendingCall {
            call_id: "c1".into(),
            tool: tool.into(),
            input,
            worker: "w1".into(),
            task: "t1".into(),
        }
    }

    /// Review a call on a chain carrying `policy`, and run it when approved.
    async fn attempt(
        policy: &ToolPolicy,
        reg: &ToolRegistry,
        tool: &str,
        input: Value,
    ) -> Result<ToolOutput, String> {
        let chain = ApprovalChain::new(vec![]).with_policy(Arc::new(policy.clone()));
        let discovered = DiscoveredTools::new();
        let view = ToolView {
            registry: reg,
            discovered: &discovered,
        };
        let mut log = MemoryApprovalLog::default();
        match chain
            .review(view, pending(tool, input), &mut log)
            .await
            .map_err(|e| e.to_string())?
        {
            Verdict::Run(approved) => policy
                .dispatch(reg, approved, &ctx())
                .await
                .map_err(|e| e.to_string()),
            other => Err(format!("{other:?}")),
        }
    }

    #[tokio::test]
    async fn a_tool_outside_allowed_tools_is_refused_and_never_runs() {
        // Attempt the refused call and assert refusal, not a config value.
        let catalog = SkillCatalog::from_skills([skill("edit-only", Some(&["Edit"]), false)])
            .expect("catalog");
        let active = catalog
            .activate("edit-only", Invoker::Model)
            .expect("activate");
        let mut policy = ToolPolicy::unrestricted();
        policy.restrict(&active);
        let (reg, edit_calls, bash_calls) = registry();

        let refused = attempt(&policy, &reg, "Bash", json!({"cmd": "ls"})).await;
        let reason = refused.expect_err("Bash must be refused");
        assert!(reason.contains("edit-only"), "{reason}");
        assert_eq!(bash_calls.load(Ordering::SeqCst), 0, "refused tool ran");

        let allowed = attempt(&policy, &reg, "Edit", json!({})).await;
        assert_eq!(allowed.expect("edit runs").render(), "ran");
        assert_eq!(edit_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dispatch_refuses_an_approval_made_without_the_policy() {
        let catalog = SkillCatalog::from_skills([skill("edit-only", Some(&["Edit"]), false)])
            .expect("catalog");
        let mut policy = ToolPolicy::unrestricted();
        policy.restrict(&catalog.activate("edit-only", Invoker::Model).expect("a"));
        let (reg, _, bash_calls) = registry();
        let discovered = DiscoveredTools::new();
        let view = ToolView {
            registry: &reg,
            discovered: &discovered,
        };
        let mut log = MemoryApprovalLog::default();
        let Verdict::Run(approved) = ApprovalChain::new(vec![])
            .review(view, pending("Bash", json!({"cmd": "ls"})), &mut log)
            .await
            .expect("review")
        else {
            panic!("a chain without the policy approves");
        };
        let result = policy.dispatch(&reg, approved, &ctx()).await;
        assert!(matches!(result, Err(SessionError::ToolRefused { .. })));
        assert_eq!(bash_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_restricted_worker_still_receives_every_prefix_definition() {
        // 0043: the prefix is byte-identical across workers, so allowed-tools may
        // not remove definitions. There is no filtering API; this pins that the
        // registry a restricted worker sees is the shared one.
        let (reg, _, _) = registry();
        let names: Vec<String> = reg
            .prefix_definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["Bash", "Edit"]);
    }

    #[tokio::test]
    async fn a_scoped_pattern_admits_only_matching_subjects() {
        let catalog = SkillCatalog::from_skills([skill("git", Some(&["Bash(git:*)"]), false)])
            .expect("catalog");
        let mut policy = ToolPolicy::unrestricted();
        policy.restrict(&catalog.activate("git", Invoker::Model).expect("activate"));
        let (reg, _, bash_calls) = registry();

        for ok in ["git status", "git"] {
            attempt(&policy, &reg, "Bash", json!({"cmd": ok}))
                .await
                .expect(ok);
        }
        for refused in [
            json!({"cmd": "gitk"}),
            json!({"cmd": "rm -rf x"}),
            json!({}),
        ] {
            assert!(
                attempt(&policy, &reg, "Bash", refused.clone())
                    .await
                    .is_err(),
                "{refused} must be refused"
            );
        }
        assert_eq!(bash_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_model_invocation_of_a_disabled_skill_is_refused_and_it_is_not_in_the_catalog() {
        let catalog =
            SkillCatalog::from_skills([skill("deploy", None, true), skill("review", None, false)])
                .expect("catalog");
        assert!(matches!(
            catalog.activate("deploy", Invoker::Model),
            Err(SessionError::ModelInvocationDisabled { .. })
        ));
        // The user may still invoke it.
        assert!(catalog.activate("deploy", Invoker::User).is_ok());
        let names: Vec<String> = catalog
            .model_catalog()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["review"]);
    }

    #[test]
    fn a_skill_not_user_invocable_refuses_the_user() {
        let mut s = skill("internal", None, false);
        s.user_invocable = false;
        let catalog = SkillCatalog::from_skills([s]).expect("catalog");
        assert!(matches!(
            catalog.activate("internal", Invoker::User),
            Err(SessionError::NotUserInvocable { .. })
        ));
        assert!(catalog.activate("internal", Invoker::Model).is_ok());
    }

    #[test]
    fn several_active_skills_intersect() {
        let catalog = SkillCatalog::from_skills([
            skill("a", Some(&["Edit", "Bash"]), false),
            skill("b", Some(&["Edit"]), false),
        ])
        .expect("catalog");
        let mut policy = ToolPolicy::unrestricted();
        policy.restrict(&catalog.activate("a", Invoker::Model).expect("a"));
        policy.restrict(&catalog.activate("b", Invoker::Model).expect("b"));
        let bash = QualifiedName::builtin("Bash").expect("name");
        let edit = QualifiedName::builtin("Edit").expect("name");
        assert!(matches!(
            policy.check(&bash, None),
            Err(SessionError::ToolRefused { ref skill, .. }) if skill == "b"
        ));
        assert!(policy.check(&edit, None).is_ok());
    }

    #[test]
    fn claude_code_style_server_names_match() {
        let gmail = QualifiedName::mcp("claude_ai_Gmail", "search").expect("name");
        assert!(
            ToolPattern::parse("mcp__claude_ai_Gmail")
                .expect("p")
                .permits(&gmail, None)
        );
        assert!(
            ToolPattern::parse("mcp__claude_ai_Gmail__search")
                .expect("p")
                .permits(&gmail, None)
        );
    }

    #[test]
    fn server_and_prefix_patterns_match_mcp_tools() {
        let issue = QualifiedName::mcp("github", "create_issue").expect("name");
        let other = QualifiedName::mcp("gitlab", "create_issue").expect("name");
        let server = ToolPattern::parse("mcp__github").expect("parse");
        assert!(server.permits(&issue, None));
        assert!(!server.permits(&other, None));
        let exact = ToolPattern::parse("mcp__github__create_issue").expect("parse");
        assert!(exact.permits(&issue, None));
        let prefix = ToolPattern::parse("mcp__git*").expect("parse");
        assert!(prefix.permits(&issue, None) && prefix.permits(&other, None));
    }

    #[test]
    fn an_unreadable_allowed_tools_entry_refuses_activation() {
        let catalog =
            SkillCatalog::from_skills([skill("bad", Some(&["Bash(git:*"]), false)]).expect("c");
        assert!(matches!(
            catalog.activate("bad", Invoker::Model),
            Err(SessionError::BadAllowedTools { .. })
        ));
        assert!(ToolPattern::parse("Bash()").is_none());
        assert!(ToolPattern::parse("").is_none());
    }

    #[test]
    fn an_empty_allowed_tools_list_permits_nothing() {
        let catalog = SkillCatalog::from_skills([skill("none", Some(&[]), false)]).expect("c");
        let mut policy = ToolPolicy::unrestricted();
        policy.restrict(&catalog.activate("none", Invoker::Model).expect("activate"));
        let edit = QualifiedName::builtin("Edit").expect("name");
        assert!(policy.check(&edit, None).is_err());
    }

    #[test]
    fn a_skill_without_allowed_tools_does_not_restrict() {
        let catalog = SkillCatalog::from_skills([skill("free", None, false)]).expect("c");
        let active = catalog.activate("free", Invoker::Model).expect("activate");
        assert!(!active.restricts_tools());
        let mut policy = ToolPolicy::unrestricted();
        policy.restrict(&active);
        let edit = QualifiedName::builtin("Edit").expect("name");
        assert!(policy.check(&edit, None).is_ok());
    }

    #[test]
    fn the_injection_is_tagged_with_the_skill_and_its_hash() {
        let catalog = SkillCatalog::from_skills([skill("s", None, false)]).expect("c");
        let injection = catalog
            .activate("s", Invoker::Model)
            .expect("a")
            .injection();
        assert_eq!(injection.skill, "s");
        assert_eq!(injection.skill_hash, "s-hash");
        assert_eq!(injection.body, "s body");
    }

    #[test]
    fn duplicate_names_and_unknown_skills_are_errors() {
        assert!(matches!(
            SkillCatalog::from_skills([skill("x", None, false), skill("x", None, false)]),
            Err(SessionError::DuplicateSkill { .. })
        ));
        let catalog = SkillCatalog::default();
        assert!(matches!(
            catalog.activate("missing", Invoker::User),
            Err(SessionError::UnknownSkill { .. })
        ));
    }

    #[test]
    fn a_catalog_builds_from_an_inventory() {
        let inventory = Inventory {
            skills: vec![skill("one", None, false)],
            ..Inventory::default()
        };
        let catalog = SkillCatalog::from_inventory(&inventory).expect("catalog");
        assert_eq!(catalog.model_catalog().len(), 1);
    }
}
