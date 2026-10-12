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
//!
//! **In the worker loop.** [`SkillSession`] is one worker environment's skills:
//! its catalog, the `skill` tool the model activates a skill with
//! ([`SkillSession::tool`]), and one [`WorkerSkills`] policy per worker
//! ([`SkillSession::attach`]), which the runtime puts on that worker's approval
//! chain. The tool activates for the worker named in its `ToolContext`, and
//! refuses a worker with no attached policy, so a skill is never active where
//! its `allowed-tools` is not enforced. The tool declares a barrier access,
//! so the calls after it in a round wait for it, and the runtime checks the
//! policy again when each of them starts
//! (`kgraf_tools::approval::ApprovalChain::recheck`).
//!
//! **Subagents and resumes.** A subagent starts from its parent's
//! restrictions as data ([`SkillRestriction`], [`SkillSession::attach_inheriting`]),
//! since it may run in another environment with its own session. A resumed
//! worker re-activates its skills with [`SkillSession::reactivate`], only at the
//! content hash its activation result recorded ([`recorded_hash`]).

use crate::SessionError;
use kgraf_instructions::{INVENTORY_FILE, Inventory, Skill};
use kgraf_tools::approval::{ApprovedCall, CallPolicy};
use kgraf_tools::{
    Access, CallPermit, MCP_PREFIX, QualifiedName, Tool, ToolContext, ToolDefinition, ToolError,
    ToolOutput, ToolRegistry,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

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

    /// The catalog of the project's inventory in `state_dir` (the file
    /// `kgraf instructions` writes), or `None` when there is no inventory. An
    /// inventory that exists and cannot be read is an error.
    pub fn load(state_dir: &Path) -> Result<Option<Self>, SessionError> {
        match Inventory::load(&Self::inventory_path(state_dir))? {
            Some(inventory) => Self::from_inventory(&inventory).map(Some),
            None => Ok(None),
        }
    }

    /// Where [`Self::load`] reads the inventory in `state_dir`.
    pub fn inventory_path(state_dir: &Path) -> std::path::PathBuf {
        state_dir.join(INVENTORY_FILE)
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
            allowed_entries: skill.allowed_tools.clone(),
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
    /// The `allowed-tools` entries as written, for the activation result.
    allowed_entries: Option<Vec<String>>,
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

    /// The `allowed-tools` entries whose tool name matches none of `tools`, as
    /// written. The scope in parentheses is not part of the name. The tools in
    /// [`NEVER_RESTRICTED`] always exist for this purpose: no skill restricts
    /// them, so an entry naming one is not a mistake.
    fn entries_naming_no_tool(&self, tools: &[QualifiedName]) -> Vec<String> {
        let (Some(entries), Some(patterns)) = (&self.allowed_entries, &self.allowed) else {
            return Vec::new();
        };
        entries
            .iter()
            .zip(patterns)
            .filter(|(_, p)| {
                !tools.iter().any(|t| p.names(t))
                    && !NEVER_RESTRICTED
                        .iter()
                        .any(|n| QualifiedName::builtin(n).is_ok_and(|q| p.names(&q)))
            })
            .map(|(e, _)| e.clone())
            .collect()
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

/// One skill's `allowed-tools` restriction as data: what a subagent inherits
/// from its parent and what the spawn record keeps, so a restarted child is
/// restricted the same way without reading the inventory again (decision 0053).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRestriction {
    /// The skill that restricts.
    pub skill: String,
    /// The content hash of the skill version that was activated.
    pub hash: String,
    /// Its `allowed-tools` entries, as written.
    pub allowed_tools: Vec<String>,
}

/// One restricting skill on a policy.
#[derive(Clone, Debug)]
struct Layer {
    skill: String,
    hash: String,
    entries: Vec<String>,
    patterns: Vec<ToolPattern>,
}

/// The tool restrictions of every skill active on one worker.
#[derive(Clone, Debug, Default)]
pub struct ToolPolicy {
    /// One layer per restricting skill, in activation order.
    layers: Vec<Layer>,
}

impl ToolPolicy {
    /// No restriction.
    pub fn unrestricted() -> Self {
        Self::default()
    }

    /// Narrow the policy by an activated skill. A skill without `allowed-tools`
    /// leaves it unchanged.
    pub fn restrict(&mut self, skill: &ActiveSkill) {
        if let (Some(patterns), Some(entries)) = (&skill.allowed, &skill.allowed_entries) {
            self.layers.push(Layer {
                skill: skill.name.clone(),
                hash: skill.hash.clone(),
                entries: entries.clone(),
                patterns: patterns.clone(),
            });
        }
    }

    /// Narrow the policy by restrictions taken from another policy (a
    /// parent's). An entry that does not parse refuses the whole set, and the
    /// policy is left unchanged: a restriction is never dropped.
    pub fn inherit(&mut self, restrictions: &[SkillRestriction]) -> Result<(), SessionError> {
        let mut layers = Vec::with_capacity(restrictions.len());
        for r in restrictions {
            let patterns = r
                .allowed_tools
                .iter()
                .map(|e| {
                    ToolPattern::parse(e).ok_or_else(|| SessionError::BadAllowedTools {
                        skill: r.skill.clone(),
                        entry: e.clone(),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            layers.push(Layer {
                skill: r.skill.clone(),
                hash: r.hash.clone(),
                entries: r.allowed_tools.clone(),
                patterns,
            });
        }
        self.layers.extend(layers);
        Ok(())
    }

    /// Every restriction on the policy, in activation order.
    pub fn restrictions(&self) -> Vec<SkillRestriction> {
        self.layers
            .iter()
            .map(|l| SkillRestriction {
                skill: l.skill.clone(),
                hash: l.hash.clone(),
                allowed_tools: l.entries.clone(),
            })
            .collect()
    }

    /// Whether a call is permitted: every layer must permit it. The built-in
    /// tools in [`NEVER_RESTRICTED`] are always permitted.
    pub fn check(&self, tool: &QualifiedName, subject: Option<&str>) -> Result<(), SessionError> {
        if tool.server().is_none() && NEVER_RESTRICTED.contains(&tool.wire().as_str()) {
            return Ok(());
        }
        for layer in &self.layers {
            if !layer.patterns.iter().any(|p| p.permits(tool, subject)) {
                return Err(SessionError::ToolRefused {
                    tool: tool.wire(),
                    skill: layer.skill.clone(),
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
        kgraf_tools::SKILLS_POLICY
    }

    fn fingerprint(&self) -> String {
        format!("{:?}", self.restrictions())
    }

    fn check(&self, tool: &QualifiedName, subject: Option<&str>) -> Result<(), String> {
        ToolPolicy::check(self, tool, subject).map_err(|e| e.to_string())
    }
}

/// The wire name of the tool the model activates a skill with.
pub const SKILL_TOOL: &str = "skill";

/// Built-in tools no skill's `allowed-tools` restricts. `report_shortfall`
/// runs nothing and changes nothing: it tells the orchestrator what the worker
/// lacks, and every policy refusal tells the worker to call it. Restricting it
/// would leave a worker that a skill refused with no way to say so except to
/// work around the refusal. The `skill` tool is not here: decision 0053 keeps
/// it subject to an active restriction.
pub const NEVER_RESTRICTED: &[&str] = &["report_shortfall"];

/// One worker environment's skills, and the skills each of its workers
/// activated.
///
/// Per-worker state is keyed by worker id because the `skill` tool is one
/// registry entry shared by every worker of the environment; each worker's
/// entry is a weak reference to the policy its approval chain holds, so it
/// goes when that chain does.
pub struct SkillSession {
    catalog: Arc<SkillCatalog>,
    /// The tools this environment's workers have, set once the registry is
    /// complete ([`Self::bind_tools`]). The session is built before the MCP tools
    /// are registered, so it cannot take them at construction.
    tools: OnceLock<Vec<QualifiedName>>,
    workers: Mutex<HashMap<String, Weak<Mutex<ToolPolicy>>>>,
}

impl std::fmt::Debug for SkillSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillSession")
            .field("skills", &self.catalog.skills.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl SkillSession {
    /// A session over `catalog`, with no worker attached.
    pub fn new(catalog: Arc<SkillCatalog>) -> Arc<Self> {
        Arc::new(Self {
            catalog,
            tools: OnceLock::new(),
            workers: Mutex::new(HashMap::new()),
        })
    }

    /// Tell the session which tools this environment's workers have. From then on
    /// a skill whose `allowed-tools` names a tool outside this set is refused at
    /// activation ([`SessionError::UnknownAllowedTools`]). The host calls this
    /// once, after the registry is complete. A second call is ignored: the set is
    /// the one the shared prefix was built from.
    ///
    /// A session never bound checks nothing; only unit tests build one.
    pub fn bind_tools(&self, tools: impl IntoIterator<Item = QualifiedName>) {
        let _ = self.tools.set(tools.into_iter().collect());
    }

    /// The entries the shared prefix lists (skills the model may invoke).
    pub fn model_catalog(&self) -> Vec<CatalogEntry> {
        self.catalog.model_catalog()
    }

    /// The `skill` tool for this session's environment.
    pub fn tool(self: &Arc<Self>) -> Arc<dyn Tool> {
        Arc::new(SkillTool {
            session: self.clone(),
        })
    }

    /// Start `worker` with no active skill and return the policy its approval
    /// chain must carry. Replaces whatever an earlier run of that worker id
    /// left: a worker's restrictions are its transcript's, re-activated by the
    /// runtime on a resume.
    pub fn attach(&self, worker: &str) -> WorkerSkills {
        let skills = WorkerSkills {
            worker: worker.to_owned(),
            policy: Arc::new(Mutex::new(ToolPolicy::unrestricted())),
        };
        self.register(&skills);
        skills
    }

    /// [`Self::attach`], starting from restrictions the worker inherits (a
    /// subagent's, from its parent). Refused when an inherited entry does not
    /// parse.
    pub fn attach_inheriting(
        &self,
        worker: &str,
        inherited: &[SkillRestriction],
    ) -> Result<WorkerSkills, SessionError> {
        let skills = WorkerSkills::inheriting(worker, inherited)?;
        self.register(&skills);
        Ok(skills)
    }

    fn register(&self, skills: &WorkerSkills) {
        // A poisoned map lock means a panic mid-insert elsewhere; the map is
        // still a valid map, and the entry below overwrites this worker's.
        let mut workers = self.workers.lock().unwrap_or_else(PoisonError::into_inner);
        workers.retain(|_, w| w.strong_count() > 0);
        workers.insert(skills.worker.clone(), Arc::downgrade(&skills.policy));
    }

    /// Re-activate `name` for `worker` on a resume, as its transcript
    /// recorded it. `recorded` is the content hash the transcript's activation
    /// result names ([`recorded_hash`]). Refused when the transcript names no
    /// hash, or when the skill changed since: the restriction would not be
    /// the one the worker ran under.
    pub fn reactivate(
        &self,
        worker: &str,
        name: &str,
        recorded: Option<&str>,
    ) -> Result<ActiveSkill, SessionError> {
        let Some(recorded) = recorded else {
            return Err(SessionError::SkillVersionUnrecorded {
                name: name.to_owned(),
            });
        };
        let current = self
            .catalog
            .skills
            .get(name)
            .ok_or_else(|| SessionError::UnknownSkill {
                name: name.to_owned(),
            })?;
        if current.hash != recorded {
            return Err(SessionError::SkillVersionChanged {
                name: name.to_owned(),
                recorded: recorded.to_owned(),
                current: current.hash.clone(),
            });
        }
        self.activate(worker, name)
    }

    /// Why `worker`'s active skills refuse `tool` with `subject`, if they do:
    /// the refusal text names the skill. `None` for a worker this session does
    /// not hold, or whose skills permit the call.
    pub fn refusal_for(
        &self,
        worker: &str,
        tool: &QualifiedName,
        subject: Option<&str>,
    ) -> Option<String> {
        let policy = self
            .workers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(worker)
            .and_then(Weak::upgrade)?;
        let policy = policy.lock().unwrap_or_else(PoisonError::into_inner);
        policy.check(tool, subject).err().map(|e| e.to_string())
    }

    /// Activate `name` for `worker` as the model, narrowing the worker's policy
    /// by the skill's `allowed-tools`. Refused for a worker with no attached
    /// policy, and for a skill the model may not invoke.
    pub fn activate(&self, worker: &str, name: &str) -> Result<ActiveSkill, SessionError> {
        let policy = self
            .workers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(worker)
            .and_then(Weak::upgrade)
            .ok_or_else(|| SessionError::NoSkillPolicy {
                name: name.to_owned(),
                worker: worker.to_owned(),
            })?;
        let active = self.catalog.activate(name, Invoker::Model)?;
        // Refuse before narrowing the policy: a refused activation restricts
        // nothing. Carrying on with a notice would leave a worker under a
        // restriction that quietly lacks tools the skill's body tells it to use.
        if let Some(tools) = self.tools.get() {
            let unknown = active.entries_naming_no_tool(tools);
            if !unknown.is_empty() {
                let mut available: Vec<String> = tools.iter().map(QualifiedName::wire).collect();
                available.sort();
                return Err(SessionError::UnknownAllowedTools {
                    skill: name.to_owned(),
                    unknown: unknown
                        .iter()
                        .map(|entry| describe_unknown(entry, &available))
                        .collect(),
                    available,
                });
            }
        }
        policy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .restrict(&active);
        Ok(active)
    }
}

/// An unknown `allowed-tools` entry as the refusal prints it: the entry, and the
/// tool this worker has that does the same job when the name is one a coding
/// agent commonly uses (Claude Code's `Bash`, `Read`, `Edit`, `Write`, `Grep`,
/// `Glob`, `LS`, or `write_file`; the table the permission rules read them by,
/// `kgraf_tools::permissions::tool_hint`). Only a tool the worker really has is
/// suggested.
fn describe_unknown(entry: &str, available: &[String]) -> String {
    let name = entry.split('(').next().unwrap_or(entry).trim();
    let Some(suggestion) = kgraf_tools::permissions::tool_hint(name) else {
        return format!("`{entry}`");
    };
    if available.iter().any(|a| a == suggestion) {
        format!("`{entry}` (use `{suggestion}`)")
    } else {
        format!("`{entry}`")
    }
}

/// The skill sessions of one run's worker environments, so a run-wide tool
/// (`report_shortfall`) can ask what a given worker's skills refuse. Passed
/// explicitly to the tool and to the code that builds each environment; holds
/// weak references, so an environment's session goes when the environment
/// does.
#[derive(Debug, Default)]
pub struct SkillDirectory {
    sessions: Mutex<Vec<Weak<SkillSession>>>,
}

impl SkillDirectory {
    /// An empty directory.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Add an environment's session.
    pub fn register(&self, session: &Arc<SkillSession>) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.retain(|s| s.strong_count() > 0);
        if !sessions
            .iter()
            .any(|s| std::ptr::eq(s.as_ptr(), Arc::as_ptr(session)))
        {
            sessions.push(Arc::downgrade(session));
        }
    }

    /// [`SkillSession::refusal_for`] across every live session: worker ids
    /// are unique within a run, so at most one session holds the worker.
    pub fn refusal_for(
        &self,
        worker: &str,
        tool: &QualifiedName,
        subject: Option<&str>,
    ) -> Option<String> {
        let sessions: Vec<Arc<SkillSession>> = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        sessions
            .iter()
            .find_map(|s| s.refusal_for(worker, tool, subject))
    }
}

/// One worker's skill restrictions, as a policy on its approval chain.
///
/// The fingerprint is the worker id alone, not the current restrictions: the
/// chain checks every policy live, after the last hook of a review and again
/// after an answered ask, and the runtime checks again when the call starts, so
/// a restriction added while a call waits still applies to it.
#[derive(Clone, Debug)]
pub struct WorkerSkills {
    worker: String,
    policy: Arc<Mutex<ToolPolicy>>,
}

impl WorkerSkills {
    /// A policy for `worker` that starts from inherited restrictions and is
    /// registered with no [`SkillSession`]: for a subagent whose environment
    /// has no skills of its own, so only the inherited restrictions apply.
    pub fn inheriting(worker: &str, inherited: &[SkillRestriction]) -> Result<Self, SessionError> {
        let mut policy = ToolPolicy::unrestricted();
        policy.inherit(inherited)?;
        Ok(Self {
            worker: worker.to_owned(),
            policy: Arc::new(Mutex::new(policy)),
        })
    }

    /// The names of the skills that restrict this worker, in activation order.
    pub fn restricting(&self) -> Vec<String> {
        self.restrictions().into_iter().map(|r| r.skill).collect()
    }

    /// This worker's restrictions now, in activation order: what a subagent
    /// it spawns inherits.
    pub fn restrictions(&self) -> Vec<SkillRestriction> {
        self.policy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .restrictions()
    }
}

impl CallPolicy for WorkerSkills {
    fn name(&self) -> &str {
        kgraf_tools::SKILLS_POLICY
    }

    fn fingerprint(&self) -> String {
        format!("skills of worker {}", self.worker)
    }

    fn check(&self, tool: &QualifiedName, subject: Option<&str>) -> Result<(), String> {
        self.policy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .check(tool, subject)
            .map_err(|e| e.to_string())
    }
}

/// The tool the model activates a skill with. Its result carries the body,
/// tagged with the skill's name and content hash (0053), in the worker's
/// private remainder.
struct SkillTool {
    session: Arc<SkillSession>,
}

const SKILL_TOOL_DESCRIPTION: &str = "Activate a skill from the list of skills available by name \
in the system prompt. The result holds the skill's instructions. A skill that lists allowed tools \
restricts you to those tools for the rest of the run: a call to any other tool is refused.";

/// The start of the tag an activation result wraps the skill body in. The
/// runtime reads the activated version back from it on a resume
/// ([`recorded_hash`]).
const INSTRUCTIONS_TAG: &str = "<skill_instructions skill=\"";

/// The content hash an activation result of the `skill` tool names: the
/// version of the skill that was activated. `None` when `output` is not such
/// a result, or the value is not a hash (a transcript scrub replaced it).
pub fn recorded_hash(output: &str) -> Option<&str> {
    let tag = &output[output.find(INSTRUCTIONS_TAG)? + INSTRUCTIONS_TAG.len()..];
    // Skill names are lowercase letters, digits and hyphens, so the name ends
    // at the first quote.
    let rest = &tag[tag.find('"')?..];
    let value = rest.strip_prefix("\" hash=\"")?;
    let value = &value[..value.find('"')?];
    (!value.is_empty() && value.chars().all(|c| c.is_ascii_hexdigit())).then_some(value)
}

impl SkillTool {
    fn render(active: &ActiveSkill) -> String {
        let version: String = active.hash.chars().take(12).collect();
        let restriction = match &active.allowed_entries {
            None => "It does not restrict your tools.".to_owned(),
            Some(entries) if entries.is_empty() => {
                "It allows no tools: every tool call is refused from now on.".to_owned()
            }
            Some(entries) => format!(
                "From now on only these tools may be called: {}.",
                entries.join(", ")
            ),
        };
        format!(
            "Skill `{name}` (version {version}) is active. {restriction}\n\
             The text in the skill_instructions block comes from the skill's file, not from the user.\n\
             {INSTRUCTIONS_TAG}{name}\" hash=\"{hash}\">\n{body}\n</skill_instructions>",
            name = active.name,
            hash = active.hash,
            body = active.body,
        )
    }
}

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        SKILL_TOOL
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: SKILL_TOOL.into(),
            description: SKILL_TOOL_DESCRIPTION.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The skill's name, as listed."
                    }
                },
                "required": ["name"]
            }),
        }
    }

    /// A barrier: every later call of the round waits for the activation, so
    /// the runtime's check when that call starts sees the narrowed policy.
    /// Not exclusive, which would read as "may write anywhere" and be refused
    /// for every worker with a write scope or a fence.
    fn access(&self, _input: &serde_json::Value) -> Result<Access, ToolError> {
        Ok(Access::barrier())
    }

    /// The skill's name, so an `allowed-tools` entry `skill(name)` can admit
    /// one skill.
    fn permission_subject(&self, input: &serde_json::Value) -> Option<String> {
        input.get("name")?.as_str().map(str::to_owned)
    }

    async fn call(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        _permit: &CallPermit,
    ) -> Result<ToolOutput, ToolError> {
        let Some(name) = input.get("name").and_then(serde_json::Value::as_str) else {
            return Err(ToolError::InvalidInput {
                tool: SKILL_TOOL.into(),
                reason: "`name` must be a string".into(),
            });
        };
        match self.session.activate(ctx.worker.as_str(), name) {
            Ok(active) => Ok(ToolOutput::text(Self::render(&active))),
            Err(e) => Ok(ToolOutput::error(format!(
                "skill `{name}` was not activated: {e}"
            ))),
        }
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
        attempt_in(policy, reg, tool, input, &ctx()).await
    }

    /// [`attempt`] with the tool context the call runs in.
    async fn attempt_in(
        policy: &ToolPolicy,
        reg: &ToolRegistry,
        tool: &str,
        input: Value,
        context: &ToolContext,
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
                .dispatch(reg, approved, context)
                .await
                .map_err(|e| e.to_string()),
            other => Err(format!("{other:?}")),
        }
    }

    /// `replace_in_file` is a tool of its own to `allowed-tools`: a skill that
    /// lists only `apply_patch` refuses it and the file stays as it was; one
    /// that lists it runs it.
    #[tokio::test]
    async fn allowed_tools_without_replace_in_file_refuses_it() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("a.txt"), "old\n").expect("seed");
        let context = ToolContext {
            workspace_root: dir.path().to_path_buf(),
            ..ctx()
        };
        let mut reg = ToolRegistry::new();
        reg.register_builtin(Arc::new(kgraf_tools::ApplyPatchTool), Exposure::Prefix)
            .expect("apply_patch");
        reg.register_builtin(
            Arc::new(kgraf_tools::ReplaceInFileTool::default()),
            Exposure::Prefix,
        )
        .expect("replace_in_file");
        let call = json!({"path": "a.txt", "old": "old", "new": "new"});
        for (allowed, runs) in [
            (&["apply_patch"][..], false),
            (&["replace_in_file"][..], true),
        ] {
            std::fs::write(dir.path().join("a.txt"), "old\n").expect("seed");
            let catalog =
                SkillCatalog::from_skills([skill("s", Some(allowed), false)]).expect("catalog");
            let mut policy = ToolPolicy::unrestricted();
            policy.restrict(&catalog.activate("s", Invoker::Model).expect("activate"));
            let result = attempt_in(&policy, &reg, "replace_in_file", call.clone(), &context).await;
            let text = std::fs::read_to_string(dir.path().join("a.txt")).expect("read");
            if runs {
                result.unwrap_or_else(|e| panic!("{allowed:?}: {e}"));
                assert_eq!(text, "new\n", "{allowed:?}");
            } else {
                let reason = result.expect_err("refused");
                assert!(reason.contains("replace_in_file"), "{allowed:?}: {reason}");
                assert_eq!(text, "old\n", "{allowed:?}: a refused call never writes");
            }
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

    /// Round 7: a skill listed `write_file`, no worker has it, and the skill was
    /// accepted. Under it nothing could write. Activation is refused, names the
    /// unknown entry and the real tools, and restricts nothing.
    #[test]
    fn a_skill_naming_a_tool_no_worker_has_is_refused_and_restricts_nothing() {
        let catalog = SkillCatalog::from_skills([
            skill(
                "writer",
                Some(&["read_file", "write_file", "Bash(git:*)"]),
                false,
            ),
            skill(
                "fine",
                Some(&["read_file", "report_shortfall", "mcp__gh"]),
                false,
            ),
        ])
        .expect("catalog");
        let session = SkillSession::new(Arc::new(catalog));
        let tool = |n: &str| QualifiedName::builtin(n).expect("name");
        session.bind_tools([
            tool("read_file"),
            tool("apply_patch"),
            tool("shell"),
            QualifiedName::mcp("gh", "issue").expect("name"),
        ]);
        let worker = session.attach("w1");

        let err = session.activate("w1", "writer").expect_err("refused");
        let text = err.to_string();
        assert!(
            matches!(err, SessionError::UnknownAllowedTools { .. }),
            "{text}"
        );
        assert!(text.contains("`write_file` (use `apply_patch`)"), "{text}");
        assert!(text.contains("`Bash(git:*)` (use `shell`)"), "{text}");
        assert!(
            !text.contains("`read_file` ("),
            "a real tool is not listed as unknown: {text}"
        );
        assert!(
            text.contains("apply_patch, mcp__gh__issue, read_file, shell"),
            "real tools listed: {text}"
        );
        assert!(
            worker.restricting().is_empty(),
            "a refused activation restricts nothing"
        );

        // Real tools, the never-restricted tool, and a connected server are known.
        session.activate("w1", "fine").expect("activates");
    }

    /// `report_shortfall` runs nothing: it tells the orchestrator what the
    /// worker lacks. A skill that does not list it must not leave a refused
    /// worker with no way to say so, since every refusal tells the worker to
    /// call it. Only the built-in is exempt; an MCP tool of that name is not.
    #[test]
    fn reporting_a_shortfall_is_never_restricted_by_a_skill() {
        let catalog = SkillCatalog::from_skills([skill("none", Some(&[]), false)]).expect("c");
        let mut policy = ToolPolicy::unrestricted();
        policy.restrict(&catalog.activate("none", Invoker::Model).expect("activate"));
        let report = QualifiedName::builtin("report_shortfall").expect("name");
        assert!(policy.check(&report, None).is_ok());
        let mcp = QualifiedName::mcp("srv", "report_shortfall").expect("name");
        assert!(policy.check(&mcp, None).is_err());
        let edit = QualifiedName::builtin("Edit").expect("name");
        assert!(
            policy.check(&edit, None).is_err(),
            "everything else stays refused"
        );
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

    /// Review `tool` for `worker` on a chain carrying `policy`, and run it
    /// through the registry when approved, as the runtime does.
    async fn run_as(
        worker: &str,
        policy: &WorkerSkills,
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
        let mut call = pending(tool, input);
        call.worker = worker.into();
        match chain
            .review(view, call, &mut log)
            .await
            .map_err(|e| e.to_string())?
        {
            Verdict::Run(approved) => {
                let mut ctx = ctx();
                ctx.worker = worker.into();
                reg.execute(approved, &ctx).await.map_err(|e| e.to_string())
            }
            other => Err(format!("{other:?}")),
        }
    }

    fn session_registry(session: &Arc<SkillSession>) -> (ToolRegistry, Arc<AtomicUsize>) {
        let (mut reg, _, bash_calls) = registry();
        reg.register_builtin(session.tool(), Exposure::Prefix)
            .expect("skill tool");
        (reg, bash_calls)
    }

    #[tokio::test]
    async fn a_skill_activated_through_the_tool_restricts_only_its_own_worker() {
        let session = SkillSession::new(Arc::new(
            SkillCatalog::from_skills([skill("edit-only", Some(&["Edit"]), false)])
                .expect("catalog"),
        ));
        let (reg, bash_calls) = session_registry(&session);
        let w1 = session.attach("w1");
        let w2 = session.attach("w2");

        let out = run_as("w1", &w1, &reg, SKILL_TOOL, json!({"name": "edit-only"}))
            .await
            .expect("activation runs");
        assert!(!out.is_error);
        let text = out.render();
        assert!(text.contains("edit-only body"), "{text}");
        assert!(
            text.contains("only these tools may be called: Edit"),
            "{text}"
        );
        assert!(
            text.contains("<skill_instructions skill=\"edit-only\""),
            "{text}"
        );
        assert_eq!(w1.restricting(), vec!["edit-only"]);

        let refused = run_as("w1", &w1, &reg, "Bash", json!({"cmd": "ls"})).await;
        let reason = refused.expect_err("w1 is restricted");
        assert!(reason.contains("edit-only"), "{reason}");
        assert_eq!(bash_calls.load(Ordering::SeqCst), 0, "refused tool ran");

        run_as("w2", &w2, &reg, "Bash", json!({"cmd": "ls"}))
            .await
            .expect("w2 is not restricted");
        assert_eq!(bash_calls.load(Ordering::SeqCst), 1);
        assert!(w2.restricting().is_empty());
    }

    /// A run-wide directory answers what one worker's skills refuse, naming
    /// the skill, and nothing for a worker no skill restricts.
    #[test]
    fn the_directory_names_the_skill_that_refuses_a_workers_call() {
        let session = SkillSession::new(Arc::new(
            SkillCatalog::from_skills([skill("edit-only", Some(&["Edit"]), false)])
                .expect("catalog"),
        ));
        let directory = SkillDirectory::new();
        directory.register(&session);
        directory.register(&session);
        let _w1 = session.attach("w1");
        let _w2 = session.attach("w2");
        session.activate("w1", "edit-only").expect("activate");
        let bash = QualifiedName::builtin("Bash").expect("name");
        let edit = QualifiedName::builtin("Edit").expect("name");
        let refusal = directory
            .refusal_for("w1", &bash, Some("ls"))
            .expect("w1's skill refuses Bash");
        assert!(refusal.contains("edit-only"), "{refusal}");
        assert_eq!(directory.refusal_for("w1", &edit, None), None);
        assert_eq!(directory.refusal_for("w2", &bash, Some("ls")), None);
        assert_eq!(directory.refusal_for("nobody", &bash, None), None);
        drop(session);
        assert_eq!(
            directory.refusal_for("w1", &bash, None),
            None,
            "a dropped environment's session is gone"
        );
    }

    #[tokio::test]
    async fn the_skill_tool_refuses_a_skill_the_model_may_not_invoke_and_restricts_nothing() {
        let session = SkillSession::new(Arc::new(
            SkillCatalog::from_skills([skill("deploy", Some(&["Edit"]), true)]).expect("catalog"),
        ));
        assert!(session.model_catalog().is_empty());
        let (reg, _) = session_registry(&session);
        let w1 = session.attach("w1");
        let out = run_as("w1", &w1, &reg, SKILL_TOOL, json!({"name": "deploy"}))
            .await
            .expect("the tool answers");
        assert!(out.is_error);
        let text = out.render();
        assert!(text.contains("disable-model-invocation"), "{text}");
        assert!(!text.contains("deploy body"), "{text}");
        assert!(w1.restricting().is_empty());
    }

    #[test]
    fn a_skill_is_never_activated_for_a_worker_without_an_attached_policy() {
        let session = SkillSession::new(Arc::new(
            SkillCatalog::from_skills([skill("edit-only", Some(&["Edit"]), false)])
                .expect("catalog"),
        ));
        assert!(matches!(
            session.activate("w1", "edit-only"),
            Err(SessionError::NoSkillPolicy { .. })
        ));
        // A policy whose chain is gone does not count as attached.
        drop(session.attach("w1"));
        assert!(matches!(
            session.activate("w1", "edit-only"),
            Err(SessionError::NoSkillPolicy { .. })
        ));
        let held = session.attach("w1");
        session.activate("w1", "edit-only").expect("attached");
        // A second attach starts the worker again, unrestricted.
        let again = session.attach("w1");
        assert_eq!(held.restricting(), vec!["edit-only"]);
        assert!(again.restricting().is_empty());
    }

    #[test]
    fn a_catalog_loads_from_the_state_directory_and_is_none_without_an_inventory() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(SkillCatalog::load(dir.path()).expect("absent").is_none());
        let inventory = Inventory {
            skills: vec![skill("one", None, false)],
            ..Inventory::default()
        };
        std::fs::write(
            dir.path().join(INVENTORY_FILE),
            serde_json::to_string(&inventory).expect("json"),
        )
        .expect("write");
        let catalog = SkillCatalog::load(dir.path())
            .expect("load")
            .expect("present");
        assert_eq!(catalog.model_catalog()[0].name, "one");
        std::fs::write(dir.path().join(INVENTORY_FILE), "[").expect("write");
        assert!(matches!(
            SkillCatalog::load(dir.path()),
            Err(SessionError::Inventory(_))
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

    /// A skill whose content hash is hex, as a real inventory's is.
    fn versioned(name: &str, allowed: &[&str], hash: &str) -> Skill {
        let mut s = skill(name, Some(allowed), false);
        s.hash = hash.into();
        s
    }

    #[tokio::test]
    async fn a_worker_attached_with_inherited_restrictions_is_restricted_by_them() {
        let parent_session = SkillSession::new(Arc::new(
            SkillCatalog::from_skills([versioned("edit-only", &["Edit"], "abc123")])
                .expect("catalog"),
        ));
        let parent = parent_session.attach("p");
        parent_session.activate("p", "edit-only").expect("activate");
        let inherited = parent.restrictions();
        assert_eq!(
            inherited,
            vec![SkillRestriction {
                skill: "edit-only".into(),
                hash: "abc123".into(),
                allowed_tools: vec!["Edit".into()],
            }]
        );

        // The child's environment has its own session, with no skill active.
        let child_session = SkillSession::new(Arc::new(SkillCatalog::default()));
        let (reg, bash_calls) = session_registry(&child_session);
        let child = child_session
            .attach_inheriting("p.s1", &inherited)
            .expect("attach");
        let refused = run_as("p.s1", &child, &reg, "Bash", json!({"cmd": "ls"})).await;
        assert!(
            refused.expect_err("inherited").contains("edit-only"),
            "the child is refused by the parent's skill"
        );
        assert_eq!(bash_calls.load(Ordering::SeqCst), 0, "refused tool ran");
        // A grandchild inherits the child's restrictions, inherited ones included.
        assert_eq!(child.restrictions(), inherited);

        // With no session at all, the inherited restriction still applies.
        let detached = WorkerSkills::inheriting("p.s2", &inherited).expect("detached");
        let (reg, bash_calls) = session_registry(&child_session);
        assert!(
            run_as("p.s2", &detached, &reg, "Bash", json!({"cmd": "ls"}))
                .await
                .is_err()
        );
        assert_eq!(bash_calls.load(Ordering::SeqCst), 0, "refused tool ran");
    }

    #[test]
    fn an_unreadable_inherited_entry_is_refused_not_dropped() {
        let bad = [SkillRestriction {
            skill: "odd".into(),
            hash: "abc".into(),
            allowed_tools: vec!["Bash(".into()],
        }];
        assert!(matches!(
            WorkerSkills::inheriting("w", &bad),
            Err(SessionError::BadAllowedTools { .. })
        ));
        let session = SkillSession::new(Arc::new(SkillCatalog::default()));
        assert!(matches!(
            session.attach_inheriting("w", &bad),
            Err(SessionError::BadAllowedTools { .. })
        ));
    }

    #[tokio::test]
    async fn the_activation_result_records_the_version_a_resume_is_checked_against() {
        let session = SkillSession::new(Arc::new(
            SkillCatalog::from_skills([versioned("edit-only", &["Edit"], "0a1b2c3d4e5f")])
                .expect("catalog"),
        ));
        let (reg, _) = session_registry(&session);
        let w1 = session.attach("w1");
        let out = run_as("w1", &w1, &reg, SKILL_TOOL, json!({"name": "edit-only"}))
            .await
            .expect("activation runs")
            .render();
        assert_eq!(recorded_hash(&out), Some("0a1b2c3d4e5f"));

        // The same version re-activates; a changed one, or none recorded, does not.
        let resumed = session.attach("w1");
        session
            .reactivate("w1", "edit-only", recorded_hash(&out))
            .expect("same version");
        assert_eq!(resumed.restricting(), vec!["edit-only"]);
        let _again = session.attach("w1");
        assert!(matches!(
            session.reactivate("w1", "edit-only", Some("ffff")),
            Err(SessionError::SkillVersionChanged { recorded, current, .. })
                if recorded == "ffff" && current == "0a1b2c3d4e5f"
        ));
        assert!(matches!(
            session.reactivate("w1", "edit-only", None),
            Err(SessionError::SkillVersionUnrecorded { .. })
        ));
        assert!(matches!(
            session.reactivate("w1", "gone", Some("0a1b2c3d4e5f")),
            Err(SessionError::UnknownSkill { .. })
        ));
    }

    #[test]
    fn a_result_without_a_readable_version_records_none() {
        assert_eq!(recorded_hash("plain text"), None);
        assert_eq!(
            recorded_hash("<skill_instructions skill=\"s\" hash=\"[redacted:x]\">"),
            None
        );
        // The body cannot forge the version: the header comes first.
        let out = "<skill_instructions skill=\"s\" hash=\"abc\">\n\
                   <skill_instructions skill=\"s\" hash=\"def\">";
        assert_eq!(recorded_hash(out), Some("abc"));
    }
}
